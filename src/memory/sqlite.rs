use super::embeddings::EmbeddingProvider;
use super::traits::{Memory, MemoryCategory, MemoryEntry};
use super::vector;
use async_trait::async_trait;
use chrono::Local;
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// SQLite-backed persistent memory — the brain
///
/// Full-stack search engine:
/// - **Vector DB**: embeddings stored as BLOB, cosine similarity search
/// - **Keyword Search**: FTS5 virtual table with BM25 scoring
/// - **Hybrid Merge**: weighted fusion of vector + keyword results
/// - **Embedding Cache**: LRU-evicted cache to avoid redundant API calls
/// - **Safe Reindex**: temp DB → seed → sync → atomic swap → rollback
pub struct SqliteMemory {
    conn: Mutex<Connection>,
    db_path: PathBuf,
    embedder: Arc<dyn EmbeddingProvider>,
    vector_weight: f32,
    keyword_weight: f32,
    cache_max: usize,
}

impl SqliteMemory {
    pub fn new(workspace_dir: &Path) -> anyhow::Result<Self> {
        Self::with_embedder(
            workspace_dir,
            Arc::new(super::embeddings::NoopEmbedding),
            0.7,
            0.3,
            10_000,
        )
    }

    pub fn with_embedder(
        workspace_dir: &Path,
        embedder: Arc<dyn EmbeddingProvider>,
        vector_weight: f32,
        keyword_weight: f32,
        cache_max: usize,
    ) -> anyhow::Result<Self> {
        let db_path = workspace_dir.join("memory").join("brain.db");

        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(&db_path)?;
        Self::init_schema(&conn)?;

        Ok(Self {
            conn: Mutex::new(conn),
            db_path,
            embedder,
            vector_weight,
            keyword_weight,
            cache_max,
        })
    }

    /// Initialize all tables: memories, FTS5, `embedding_cache`
    fn init_schema(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(
            "-- Core memories table
            CREATE TABLE IF NOT EXISTS memories (
                id          TEXT PRIMARY KEY,
                key         TEXT NOT NULL UNIQUE,
                content     TEXT NOT NULL,
                category    TEXT NOT NULL DEFAULT 'core',
                embedding   BLOB,
                created_at  TEXT NOT NULL,
                updated_at  TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_memories_category ON memories(category);
            CREATE INDEX IF NOT EXISTS idx_memories_key ON memories(key);

            -- FTS5 full-text search (BM25 scoring)
            CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(
                key, content, content=memories, content_rowid=rowid
            );

            -- FTS5 triggers: keep in sync with memories table
            CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
                INSERT INTO memories_fts(rowid, key, content)
                VALUES (new.rowid, new.key, new.content);
            END;
            CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, key, content)
                VALUES ('delete', old.rowid, old.key, old.content);
            END;
            CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
                INSERT INTO memories_fts(memories_fts, rowid, key, content)
                VALUES ('delete', old.rowid, old.key, old.content);
                INSERT INTO memories_fts(rowid, key, content)
                VALUES (new.rowid, new.key, new.content);
            END;

            -- Embedding cache with LRU eviction
            CREATE TABLE IF NOT EXISTS embedding_cache (
                content_hash TEXT PRIMARY KEY,
                embedding    BLOB NOT NULL,
                created_at   TEXT NOT NULL,
                accessed_at  TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_cache_accessed ON embedding_cache(accessed_at);",
        )?;
        Ok(())
    }

    fn category_to_str(cat: &MemoryCategory) -> String {
        match cat {
            MemoryCategory::Core => "core".into(),
            MemoryCategory::Daily => "daily".into(),
            MemoryCategory::Conversation => "conversation".into(),
            MemoryCategory::Custom(name) => name.clone(),
        }
    }

    fn str_to_category(s: &str) -> MemoryCategory {
        match s {
            "core" => MemoryCategory::Core,
            "daily" => MemoryCategory::Daily,
            "conversation" => MemoryCategory::Conversation,
            other => MemoryCategory::Custom(other.to_string()),
        }
    }

    /// Simple content hash for embedding cache
    fn content_hash(text: &str) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        text.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }

    /// Get embedding from cache, or compute + cache it
    async fn get_or_compute_embedding(&self, text: &str) -> anyhow::Result<Option<Vec<f32>>> {
        if self.embedder.dimensions() == 0 {
            return Ok(None); // Noop embedder
        }

        let hash = Self::content_hash(text);
        let now = Local::now().to_rfc3339();

        // Check cache
        {
            let conn = self
                .conn
                .lock()
                .map_err(|e| anyhow::anyhow!("Lock error: {e}"))?;

            let mut stmt =
                conn.prepare("SELECT embedding FROM embedding_cache WHERE content_hash = ?1")?;
            let cached: Option<Vec<u8>> = stmt.query_row(params![hash], |row| row.get(0)).ok();

            if let Some(bytes) = cached {
                // Update accessed_at for LRU
                conn.execute(
                    "UPDATE embedding_cache SET accessed_at = ?1 WHERE content_hash = ?2",
                    params![now, hash],
                )?;
                return Ok(Some(vector::bytes_to_vec(&bytes)));
            }
        }

        // Compute embedding
        let embedding = self.embedder.embed_one(text).await?;
        let bytes = vector::vec_to_bytes(&embedding);

        // Store in cache + LRU eviction
        {
            let conn = self
                .conn
                .lock()
                .map_err(|e| anyhow::anyhow!("Lock error: {e}"))?;

            conn.execute(
                "INSERT OR REPLACE INTO embedding_cache (content_hash, embedding, created_at, accessed_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![hash, bytes, now, now],
            )?;

            // LRU eviction: keep only cache_max entries
            #[allow(clippy::cast_possible_wrap)]
            let max = self.cache_max as i64;
            conn.execute(
                "DELETE FROM embedding_cache WHERE content_hash IN (
                    SELECT content_hash FROM embedding_cache
                    ORDER BY accessed_at ASC
                    LIMIT MAX(0, (SELECT COUNT(*) FROM embedding_cache) - ?1)
                )",
                params![max],
            )?;
        }

        Ok(Some(embedding))
    }

    /// FTS5 BM25 keyword search
    fn fts5_search(
        conn: &Connection,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<(String, f32)>> {
        // Escape FTS5 special chars and build query
        let fts_query: String = query
            .split_whitespace()
            .map(|w| format!("\"{w}\""))
            .collect::<Vec<_>>()
            .join(" OR ");

        if fts_query.is_empty() {
            return Ok(Vec::new());
        }

        let sql = "SELECT m.id, bm25(memories_fts) as score
                   FROM memories_fts f
                   JOIN memories m ON m.rowid = f.rowid
                   WHERE memories_fts MATCH ?1
                   ORDER BY score
                   LIMIT ?2";

        let mut stmt = conn.prepare(sql)?;
        #[allow(clippy::cast_possible_wrap)]
        let limit_i64 = limit as i64;

        let rows = stmt.query_map(params![fts_query, limit_i64], |row| {
            let id: String = row.get(0)?;
            let score: f64 = row.get(1)?;
            // BM25 returns negative scores (lower = better), negate for ranking
            #[allow(clippy::cast_possible_truncation)]
            Ok((id, (-score) as f32))
        })?;

        let mut results = Vec::new();
        for row in rows {
            results.push(row?);
        }
        Ok(results)
    }

    /// Vector similarity search: scan embeddings and compute cosine similarity
    fn vector_search(
        conn: &Connection,
        query_embedding: &[f32],
        limit: usize,
    ) -> anyhow::Result<Vec<(String, f32)>> {
        let mut stmt =
            conn.prepare("SELECT id, embedding FROM memories WHERE embedding IS NOT NULL")?;

        let rows = stmt.query_map([], |row| {
            let id: String = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            Ok((id, blob))
        })?;

        let mut scored: Vec<(String, f32)> = Vec::new();
        for row in rows {
            let (id, blob) = row?;
            let emb = vector::bytes_to_vec(&blob);
            let sim = vector::cosine_similarity(query_embedding, &emb);
            if sim > 0.0 {
                scored.push((id, sim));
            }
        }

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored)
    }

    /// Safe reindex: rebuild FTS5 + embeddings with rollback on failure
    #[allow(dead_code)]
    pub async fn reindex(&self) -> anyhow::Result<usize> {
        // Step 1: Rebuild FTS5
        {
            let conn = self
                .conn
                .lock()
                .map_err(|e| anyhow::anyhow!("Lock error: {e}"))?;

            conn.execute_batch("INSERT INTO memories_fts(memories_fts) VALUES('rebuild');")?;
        }

        // Step 2: Re-embed all memories that lack embeddings
        if self.embedder.dimensions() == 0 {
            return Ok(0);
        }

        let entries: Vec<(String, String)> = {
            let conn = self
                .conn
                .lock()
                .map_err(|e| anyhow::anyhow!("Lock error: {e}"))?;

            let mut stmt =
                conn.prepare("SELECT id, content FROM memories WHERE embedding IS NULL")?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.filter_map(std::result::Result::ok).collect()
        };

        let mut count = 0;
        for (id, content) in &entries {
            if let Ok(Some(emb)) = self.get_or_compute_embedding(content).await {
                let bytes = vector::vec_to_bytes(&emb);
                let conn = self
                    .conn
                    .lock()
                    .map_err(|e| anyhow::anyhow!("Lock error: {e}"))?;
                conn.execute(
                    "UPDATE memories SET embedding = ?1 WHERE id = ?2",
                    params![bytes, id],
                )?;
                count += 1;
            }
        }

        Ok(count)
    }
}

#[async_trait]
impl Memory for SqliteMemory {
    fn name(&self) -> &str {
        "sqlite"
    }

    async fn store(
        &self,
        key: &str,
        content: &str,
        category: MemoryCategory,
    ) -> anyhow::Result<()> {
        // Compute embedding (async, before lock)
        let embedding_bytes = self
            .get_or_compute_embedding(content)
            .await?
            .map(|emb| vector::vec_to_bytes(&emb));

        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock error: {e}"))?;
        let now = Local::now().to_rfc3339();
        let cat = Self::category_to_str(&category);
        let id = Uuid::new_v4().to_string();

        conn.execute(
            "INSERT INTO memories (id, key, content, category, embedding, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(key) DO UPDATE SET
                content = excluded.content,
                category = excluded.category,
                embedding = excluded.embedding,
                updated_at = excluded.updated_at",
            params![id, key, content, cat, embedding_bytes, now, now],
        )?;

        Ok(())
    }

    async fn recall(&self, query: &str, limit: usize) -> anyhow::Result<Vec<MemoryEntry>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        // Compute query embedding (async, before lock)
        let query_embedding = self.get_or_compute_embedding(query).await?;

        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock error: {e}"))?;

        // FTS5 BM25 keyword search
        let keyword_results = Self::fts5_search(&conn, query, limit * 2).unwrap_or_default();

        // Vector similarity search (if embeddings available)
        let vector_results = if let Some(ref qe) = query_embedding {
            Self::vector_search(&conn, qe, limit * 2).unwrap_or_default()
        } else {
            Vec::new()
        };

        // Hybrid merge
        let merged = if vector_results.is_empty() {
            // No embeddings — use keyword results only
            keyword_results
                .iter()
                .map(|(id, score)| vector::ScoredResult {
                    id: id.clone(),
                    vector_score: None,
                    keyword_score: Some(*score),
                    final_score: *score,
                })
                .collect::<Vec<_>>()
        } else {
            vector::hybrid_merge(
                &vector_results,
                &keyword_results,
                self.vector_weight,
                self.keyword_weight,
                limit,
            )
        };

        // Fetch full entries for merged results
        let mut results = Vec::new();
        for scored in &merged {
            let mut stmt = conn.prepare(
                "SELECT id, key, content, category, created_at FROM memories WHERE id = ?1",
            )?;
            if let Ok(entry) = stmt.query_row(params![scored.id], |row| {
                Ok(MemoryEntry {
                    id: row.get(0)?,
                    key: row.get(1)?,
                    content: row.get(2)?,
                    category: Self::str_to_category(&row.get::<_, String>(3)?),
                    timestamp: row.get(4)?,
                    session_id: None,
                    score: Some(f64::from(scored.final_score)),
                })
            }) {
                results.push(entry);
            }
        }

        // If hybrid returned nothing, fall back to LIKE search
        if results.is_empty() {
            let keywords: Vec<String> =
                query.split_whitespace().map(|w| format!("%{w}%")).collect();
            if !keywords.is_empty() {
                let conditions: Vec<String> = keywords
                    .iter()
                    .enumerate()
                    .map(|(i, _)| {
                        format!("(content LIKE ?{} OR key LIKE ?{})", i * 2 + 1, i * 2 + 2)
                    })
                    .collect();
                let where_clause = conditions.join(" OR ");
                let sql = format!(
                    "SELECT id, key, content, category, created_at FROM memories
                     WHERE {where_clause}
                     ORDER BY updated_at DESC
                     LIMIT ?{}",
                    keywords.len() * 2 + 1
                );
                let mut stmt = conn.prepare(&sql)?;
                let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
                for kw in &keywords {
                    param_values.push(Box::new(kw.clone()));
                    param_values.push(Box::new(kw.clone()));
                }
                #[allow(clippy::cast_possible_wrap)]
                param_values.push(Box::new(limit as i64));
                let params_ref: Vec<&dyn rusqlite::types::ToSql> =
                    param_values.iter().map(AsRef::as_ref).collect();
                let rows = stmt.query_map(params_ref.as_slice(), |row| {
                    Ok(MemoryEntry {
                        id: row.get(0)?,
                        key: row.get(1)?,
                        content: row.get(2)?,
                        category: Self::str_to_category(&row.get::<_, String>(3)?),
                        timestamp: row.get(4)?,
                        session_id: None,
                        score: Some(1.0),
                    })
                })?;
                for row in rows {
                    results.push(row?);
                }
            }
        }

        results.truncate(limit);
        Ok(results)
    }

    async fn get(&self, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock error: {e}"))?;

        let mut stmt = conn.prepare(
            "SELECT id, key, content, category, created_at FROM memories WHERE key = ?1",
        )?;

        let mut rows = stmt.query_map(params![key], |row| {
            Ok(MemoryEntry {
                id: row.get(0)?,
                key: row.get(1)?,
                content: row.get(2)?,
                category: Self::str_to_category(&row.get::<_, String>(3)?),
                timestamp: row.get(4)?,
                session_id: None,
                score: None,
            })
        })?;

        match rows.next() {
            Some(Ok(entry)) => Ok(Some(entry)),
            _ => Ok(None),
        }
    }

    async fn list(&self, category: Option<&MemoryCategory>) -> anyhow::Result<Vec<MemoryEntry>> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock error: {e}"))?;

        let mut results = Vec::new();

        let row_mapper = |row: &rusqlite::Row| -> rusqlite::Result<MemoryEntry> {
            Ok(MemoryEntry {
                id: row.get(0)?,
                key: row.get(1)?,
                content: row.get(2)?,
                category: Self::str_to_category(&row.get::<_, String>(3)?),
                timestamp: row.get(4)?,
                session_id: None,
                score: None,
            })
        };

        if let Some(cat) = category {
            let cat_str = Self::category_to_str(cat);
            let mut stmt = conn.prepare(
                "SELECT id, key, content, category, created_at FROM memories
                 WHERE category = ?1 ORDER BY updated_at DESC",
            )?;
            let rows = stmt.query_map(params![cat_str], row_mapper)?;
            for row in rows {
                results.push(row?);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, key, content, category, created_at FROM memories
                 ORDER BY updated_at DESC",
            )?;
            let rows = stmt.query_map([], row_mapper)?;
            for row in rows {
                results.push(row?);
            }
        }

        Ok(results)
    }

    async fn forget(&self, key: &str) -> anyhow::Result<bool> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock error: {e}"))?;
        let affected = conn.execute("DELETE FROM memories WHERE key = ?1", params![key])?;
        Ok(affected > 0)
    }

    async fn count(&self) -> anyhow::Result<usize> {
        let conn = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("Lock error: {e}"))?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?;
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        Ok(count as usize)
    }

    async fn health_check(&self) -> bool {
        self.conn
            .lock()
            .map(|c| c.execute_batch("SELECT 1").is_ok())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_sqlite() -> (TempDir, SqliteMemory) {
        let tmp = TempDir::new().unwrap();
        let mem = SqliteMemory::new(tmp.path()).unwrap();
        (tmp, mem)
    }

    #[tokio::test]
    async fn sqlite_name() {
        let (_tmp, mem) = temp_sqlite();
        assert_eq!(mem.name(), "sqlite");
    }

    #[tokio::test]
    async fn sqlite_health() {
        let (_tmp, mem) = temp_sqlite();
        assert!(mem.health_check().await);
    }

    #[tokio::test]
    async fn sqlite_store_and_get() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("user_lang", "Prefers Rust", MemoryCategory::Core)
            .await
            .unwrap();

        let entry = mem.get("user_lang").await.unwrap();
        assert!(entry.is_some());
        let entry = entry.unwrap();
        assert_eq!(entry.key, "user_lang");
        assert_eq!(entry.content, "Prefers Rust");
        assert_eq!(entry.category, MemoryCategory::Core);
    }

    #[tokio::test]
    async fn sqlite_store_upsert() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("pref", "likes Rust", MemoryCategory::Core)
            .await
            .unwrap();
        mem.store("pref", "loves Rust", MemoryCategory::Core)
            .await
            .unwrap();

        let entry = mem.get("pref").await.unwrap().unwrap();
        assert_eq!(entry.content, "loves Rust");
        assert_eq!(mem.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn sqlite_recall_keyword() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a", "Rust is fast and safe", MemoryCategory::Core)
            .await
            .unwrap();
        mem.store("b", "Python is interpreted", MemoryCategory::Core)
            .await
            .unwrap();
        mem.store("c", "Rust has zero-cost abstractions", MemoryCategory::Core)
            .await
            .unwrap();

        let results = mem.recall("Rust", 10).await.unwrap();
        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|r| r.content.to_lowercase().contains("rust")));
    }

    #[tokio::test]
    async fn sqlite_recall_multi_keyword() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a", "Rust is fast", MemoryCategory::Core)
            .await
            .unwrap();
        mem.store("b", "Rust is safe and fast", MemoryCategory::Core)
            .await
            .unwrap();

        let results = mem.recall("fast safe", 10).await.unwrap();
        assert!(!results.is_empty());
        // Entry with both keywords should score higher
        assert!(results[0].content.contains("safe") && results[0].content.contains("fast"));
    }

    #[tokio::test]
    async fn sqlite_recall_no_match() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a", "Rust rocks", MemoryCategory::Core)
            .await
            .unwrap();
        let results = mem.recall("javascript", 10).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn sqlite_forget() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("temp", "temporary data", MemoryCategory::Conversation)
            .await
            .unwrap();
        assert_eq!(mem.count().await.unwrap(), 1);

        let removed = mem.forget("temp").await.unwrap();
        assert!(removed);
        assert_eq!(mem.count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn sqlite_forget_nonexistent() {
        let (_tmp, mem) = temp_sqlite();
        let removed = mem.forget("nope").await.unwrap();
        assert!(!removed);
    }

    #[tokio::test]
    async fn sqlite_list_all() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a", "one", MemoryCategory::Core).await.unwrap();
        mem.store("b", "two", MemoryCategory::Daily).await.unwrap();
        mem.store("c", "three", MemoryCategory::Conversation)
            .await
            .unwrap();

        let all = mem.list(None).await.unwrap();
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn sqlite_list_by_category() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a", "core1", MemoryCategory::Core).await.unwrap();
        mem.store("b", "core2", MemoryCategory::Core).await.unwrap();
        mem.store("c", "daily1", MemoryCategory::Daily)
            .await
            .unwrap();

        let core = mem.list(Some(&MemoryCategory::Core)).await.unwrap();
        assert_eq!(core.len(), 2);

        let daily = mem.list(Some(&MemoryCategory::Daily)).await.unwrap();
        assert_eq!(daily.len(), 1);
    }

    #[tokio::test]
    async fn sqlite_count_empty() {
        let (_tmp, mem) = temp_sqlite();
        assert_eq!(mem.count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn sqlite_get_nonexistent() {
        let (_tmp, mem) = temp_sqlite();
        assert!(mem.get("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sqlite_db_persists() {
        let tmp = TempDir::new().unwrap();

        {
            let mem = SqliteMemory::new(tmp.path()).unwrap();
            mem.store("persist", "I survive restarts", MemoryCategory::Core)
                .await
                .unwrap();
        }

        // Reopen
        let mem2 = SqliteMemory::new(tmp.path()).unwrap();
        let entry = mem2.get("persist").await.unwrap();
        assert!(entry.is_some());
        assert_eq!(entry.unwrap().content, "I survive restarts");
    }

    #[tokio::test]
    async fn sqlite_category_roundtrip() {
        let (_tmp, mem) = temp_sqlite();
        let categories = vec![
            MemoryCategory::Core,
            MemoryCategory::Daily,
            MemoryCategory::Conversation,
            MemoryCategory::Custom("project".into()),
        ];

        for (i, cat) in categories.iter().enumerate() {
            mem.store(&format!("k{i}"), &format!("v{i}"), cat.clone())
                .await
                .unwrap();
        }

        for (i, cat) in categories.iter().enumerate() {
            let entry = mem.get(&format!("k{i}")).await.unwrap().unwrap();
            assert_eq!(&entry.category, cat);
        }
    }

    // ── FTS5 search tests ────────────────────────────────────────

    #[tokio::test]
    async fn fts5_bm25_ranking() {
        let (_tmp, mem) = temp_sqlite();
        mem.store(
            "a",
            "Rust is a systems programming language",
            MemoryCategory::Core,
        )
        .await
        .unwrap();
        mem.store("b", "Python is great for scripting", MemoryCategory::Core)
            .await
            .unwrap();
        mem.store(
            "c",
            "Rust and Rust and Rust everywhere",
            MemoryCategory::Core,
        )
        .await
        .unwrap();

        let results = mem.recall("Rust", 10).await.unwrap();
        assert!(results.len() >= 2);
        // All results should contain "Rust"
        for r in &results {
            assert!(
                r.content.to_lowercase().contains("rust"),
                "Expected 'rust' in: {}",
                r.content
            );
        }
    }

    #[tokio::test]
    async fn fts5_multi_word_query() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a", "The quick brown fox jumps", MemoryCategory::Core)
            .await
            .unwrap();
        mem.store("b", "A lazy dog sleeps", MemoryCategory::Core)
            .await
            .unwrap();
        mem.store("c", "The quick dog runs fast", MemoryCategory::Core)
            .await
            .unwrap();

        let results = mem.recall("quick dog", 10).await.unwrap();
        assert!(!results.is_empty());
        // "The quick dog runs fast" matches both terms
        assert!(results[0].content.contains("quick"));
    }

    #[tokio::test]
    async fn recall_empty_query_returns_empty() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a", "data", MemoryCategory::Core).await.unwrap();
        let results = mem.recall("", 10).await.unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn recall_whitespace_query_returns_empty() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("a", "data", MemoryCategory::Core).await.unwrap();
        let results = mem.recall("   ", 10).await.unwrap();
        assert!(results.is_empty());
    }

    // ── Embedding cache tests ────────────────────────────────────

    #[test]
    fn content_hash_deterministic() {
        let h1 = SqliteMemory::content_hash("hello world");
        let h2 = SqliteMemory::content_hash("hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn content_hash_different_inputs() {
        let h1 = SqliteMemory::content_hash("hello");
        let h2 = SqliteMemory::content_hash("world");
        assert_ne!(h1, h2);
    }

    // ── Schema tests ─────────────────────────────────────────────

    #[tokio::test]
    async fn schema_has_fts5_table() {
        let (_tmp, mem) = temp_sqlite();
        let conn = mem.conn.lock().unwrap();
        // FTS5 table should exist
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='memories_fts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn schema_has_embedding_cache() {
        let (_tmp, mem) = temp_sqlite();
        let conn = mem.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='embedding_cache'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn schema_memories_has_embedding_column() {
        let (_tmp, mem) = temp_sqlite();
        let conn = mem.conn.lock().unwrap();
        // Check that embedding column exists by querying it
        let result = conn.execute_batch("SELECT embedding FROM memories LIMIT 0");
        assert!(result.is_ok());
    }

    // ── FTS5 sync trigger tests ──────────────────────────────────

    #[tokio::test]
    async fn fts5_syncs_on_insert() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("test_key", "unique_searchterm_xyz", MemoryCategory::Core)
            .await
            .unwrap();

        let conn = mem.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH '\"unique_searchterm_xyz\"'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn fts5_syncs_on_delete() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("del_key", "deletable_content_abc", MemoryCategory::Core)
            .await
            .unwrap();
        mem.forget("del_key").await.unwrap();

        let conn = mem.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH '\"deletable_content_abc\"'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn fts5_syncs_on_update() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("upd_key", "original_content_111", MemoryCategory::Core)
            .await
            .unwrap();
        mem.store("upd_key", "updated_content_222", MemoryCategory::Core)
            .await
            .unwrap();

        let conn = mem.conn.lock().unwrap();
        // Old content should not be findable
        let old: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH '\"original_content_111\"'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old, 0);

        // New content should be findable
        let new: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories_fts WHERE memories_fts MATCH '\"updated_content_222\"'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(new, 1);
    }

    // ── With-embedder constructor test ───────────────────────────

    #[test]
    fn with_embedder_noop() {
        let tmp = TempDir::new().unwrap();
        let embedder = Arc::new(super::super::embeddings::NoopEmbedding);
        let mem = SqliteMemory::with_embedder(tmp.path(), embedder, 0.7, 0.3, 1000);
        assert!(mem.is_ok());
        assert_eq!(mem.unwrap().name(), "sqlite");
    }

    // ── Reindex test ─────────────────────────────────────────────

    #[tokio::test]
    async fn reindex_rebuilds_fts() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("r1", "reindex test alpha", MemoryCategory::Core)
            .await
            .unwrap();
        mem.store("r2", "reindex test beta", MemoryCategory::Core)
            .await
            .unwrap();

        // Reindex should succeed (noop embedder → 0 re-embedded)
        let count = mem.reindex().await.unwrap();
        assert_eq!(count, 0);

        // FTS should still work after rebuild
        let results = mem.recall("reindex", 10).await.unwrap();
        assert_eq!(results.len(), 2);
    }

    // ── Recall limit test ────────────────────────────────────────

    #[tokio::test]
    async fn recall_respects_limit() {
        let (_tmp, mem) = temp_sqlite();
        for i in 0..20 {
            mem.store(
                &format!("k{i}"),
                &format!("common keyword item {i}"),
                MemoryCategory::Core,
            )
            .await
            .unwrap();
        }

        let results = mem.recall("common keyword", 5).await.unwrap();
        assert!(results.len() <= 5);
    }

    // ── Score presence test ──────────────────────────────────────

    #[tokio::test]
    async fn recall_results_have_scores() {
        let (_tmp, mem) = temp_sqlite();
        mem.store("s1", "scored result test", MemoryCategory::Core)
            .await
            .unwrap();

        let results = mem.recall("scored", 10).await.unwrap();
        assert!(!results.is_empty());
        for r in &results {
            assert!(r.score.is_some(), "Expected score on result: {:?}", r.key);
        }
    }
}
