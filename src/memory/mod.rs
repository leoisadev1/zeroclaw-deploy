pub mod markdown;
pub mod sqlite;
pub mod traits;

pub use markdown::MarkdownMemory;
pub use sqlite::SqliteMemory;
pub use traits::Memory;
#[allow(unused_imports)]
pub use traits::{MemoryCategory, MemoryEntry};

use crate::config::MemoryConfig;
use std::path::Path;

/// Factory: create the right memory backend from config
pub fn create_memory(
    config: &MemoryConfig,
    workspace_dir: &Path,
) -> anyhow::Result<Box<dyn Memory>> {
    match config.backend.as_str() {
        "sqlite" => Ok(Box::new(SqliteMemory::new(workspace_dir)?)),
        "markdown" | "none" => Ok(Box::new(MarkdownMemory::new(workspace_dir))),
        other => {
            tracing::warn!("Unknown memory backend '{other}', falling back to markdown");
            Ok(Box::new(MarkdownMemory::new(workspace_dir)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn factory_sqlite() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "sqlite".into(),
            auto_save: true,
        };
        let mem = create_memory(&cfg, tmp.path()).unwrap();
        assert_eq!(mem.name(), "sqlite");
    }

    #[test]
    fn factory_markdown() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "markdown".into(),
            auto_save: true,
        };
        let mem = create_memory(&cfg, tmp.path()).unwrap();
        assert_eq!(mem.name(), "markdown");
    }

    #[test]
    fn factory_none_falls_back_to_markdown() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "none".into(),
            auto_save: true,
        };
        let mem = create_memory(&cfg, tmp.path()).unwrap();
        assert_eq!(mem.name(), "markdown");
    }

    #[test]
    fn factory_unknown_falls_back_to_markdown() {
        let tmp = TempDir::new().unwrap();
        let cfg = MemoryConfig {
            backend: "redis".into(),
            auto_save: true,
        };
        let mem = create_memory(&cfg, tmp.path()).unwrap();
        assert_eq!(mem.name(), "markdown");
    }
}
