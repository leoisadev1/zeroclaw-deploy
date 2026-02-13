use crate::channels::traits::{Channel, ChannelMessage};
use async_trait::async_trait;
use directories::UserDirs;
use tokio::sync::mpsc;

/// iMessage channel using macOS `AppleScript` bridge.
/// Polls the Messages database for new messages and sends replies via `osascript`.
#[derive(Clone)]
pub struct IMessageChannel {
    allowed_contacts: Vec<String>,
    poll_interval_secs: u64,
}

impl IMessageChannel {
    pub fn new(allowed_contacts: Vec<String>) -> Self {
        Self {
            allowed_contacts,
            poll_interval_secs: 3,
        }
    }

    fn is_contact_allowed(&self, sender: &str) -> bool {
        if self.allowed_contacts.iter().any(|u| u == "*") {
            return true;
        }
        self.allowed_contacts.iter().any(|u| {
            u.eq_ignore_ascii_case(sender)
        })
    }
}

#[async_trait]
impl Channel for IMessageChannel {
    fn name(&self) -> &str {
        "imessage"
    }

    async fn send(&self, message: &str, target: &str) -> anyhow::Result<()> {
        let escaped_msg = message.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            r#"tell application "Messages"
    set targetService to 1st account whose service type = iMessage
    set targetBuddy to participant "{target}" of targetService
    send "{escaped_msg}" to targetBuddy
end tell"#
        );

        let output = tokio::process::Command::new("osascript")
            .arg("-e")
            .arg(&script)
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("iMessage send failed: {stderr}");
        }

        Ok(())
    }

    async fn listen(&self, tx: mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        tracing::info!("iMessage channel listening (AppleScript bridge)...");

        // Query the Messages SQLite database for new messages
        // The database is at ~/Library/Messages/chat.db
        let db_path = UserDirs::new()
            .map(|u| u.home_dir().join("Library/Messages/chat.db"))
            .ok_or_else(|| anyhow::anyhow!("Cannot find home directory"))?;

        if !db_path.exists() {
            anyhow::bail!(
                "Messages database not found at {}. Ensure Messages.app is set up and Full Disk Access is granted.",
                db_path.display()
            );
        }

        // Track the last ROWID we've seen
        let mut last_rowid = get_max_rowid(&db_path).await.unwrap_or(0);

        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(self.poll_interval_secs)).await;

            let new_messages = fetch_new_messages(&db_path, last_rowid).await;

            match new_messages {
                Ok(messages) => {
                    for (rowid, sender, text) in messages {
                        if rowid > last_rowid {
                            last_rowid = rowid;
                        }

                        if !self.is_contact_allowed(&sender) {
                            continue;
                        }

                        if text.trim().is_empty() {
                            continue;
                        }

                        let msg = ChannelMessage {
                            id: rowid.to_string(),
                            sender: sender.clone(),
                            content: text,
                            channel: "imessage".to_string(),
                            timestamp: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs(),
                        };

                        if tx.send(msg).await.is_err() {
                            return Ok(());
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("iMessage poll error: {e}");
                }
            }
        }
    }

    async fn health_check(&self) -> bool {
        if !cfg!(target_os = "macos") {
            return false;
        }

        let db_path = UserDirs::new()
            .map(|u| u.home_dir().join("Library/Messages/chat.db"))
            .unwrap_or_default();

        db_path.exists()
    }
}

/// Get the current max ROWID from the messages table
async fn get_max_rowid(db_path: &std::path::Path) -> anyhow::Result<i64> {
    let output = tokio::process::Command::new("sqlite3")
        .arg(db_path)
        .arg("SELECT MAX(ROWID) FROM message WHERE is_from_me = 0;")
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let rowid = stdout.trim().parse::<i64>().unwrap_or(0);
    Ok(rowid)
}

/// Fetch messages newer than `since_rowid`
async fn fetch_new_messages(
    db_path: &std::path::Path,
    since_rowid: i64,
) -> anyhow::Result<Vec<(i64, String, String)>> {
    let query = format!(
        "SELECT m.ROWID, h.id, m.text \
         FROM message m \
         JOIN handle h ON m.handle_id = h.ROWID \
         WHERE m.ROWID > {since_rowid} \
         AND m.is_from_me = 0 \
         AND m.text IS NOT NULL \
         ORDER BY m.ROWID ASC \
         LIMIT 20;"
    );

    let output = tokio::process::Command::new("sqlite3")
        .arg("-separator")
        .arg("|")
        .arg(db_path)
        .arg(&query)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("sqlite3 query failed: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut results = Vec::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(3, '|').collect();
        if parts.len() == 3 {
            if let Ok(rowid) = parts[0].parse::<i64>() {
                results.push((rowid, parts[1].to_string(), parts[2].to_string()));
            }
        }
    }

    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_with_contacts() {
        let ch = IMessageChannel::new(vec!["+1234567890".into()]);
        assert_eq!(ch.allowed_contacts.len(), 1);
        assert_eq!(ch.poll_interval_secs, 3);
    }

    #[test]
    fn creates_with_empty_contacts() {
        let ch = IMessageChannel::new(vec![]);
        assert!(ch.allowed_contacts.is_empty());
    }

    #[test]
    fn wildcard_allows_anyone() {
        let ch = IMessageChannel::new(vec!["*".into()]);
        assert!(ch.is_contact_allowed("+1234567890"));
        assert!(ch.is_contact_allowed("random@icloud.com"));
        assert!(ch.is_contact_allowed(""));
    }

    #[test]
    fn specific_contact_allowed() {
        let ch = IMessageChannel::new(vec!["+1234567890".into(), "user@icloud.com".into()]);
        assert!(ch.is_contact_allowed("+1234567890"));
        assert!(ch.is_contact_allowed("user@icloud.com"));
    }

    #[test]
    fn unknown_contact_denied() {
        let ch = IMessageChannel::new(vec!["+1234567890".into()]);
        assert!(!ch.is_contact_allowed("+9999999999"));
        assert!(!ch.is_contact_allowed("hacker@evil.com"));
    }

    #[test]
    fn contact_case_insensitive() {
        let ch = IMessageChannel::new(vec!["User@iCloud.com".into()]);
        assert!(ch.is_contact_allowed("user@icloud.com"));
        assert!(ch.is_contact_allowed("USER@ICLOUD.COM"));
    }

    #[test]
    fn empty_allowlist_denies_all() {
        let ch = IMessageChannel::new(vec![]);
        assert!(!ch.is_contact_allowed("+1234567890"));
        assert!(!ch.is_contact_allowed("anyone"));
    }

    #[test]
    fn name_returns_imessage() {
        let ch = IMessageChannel::new(vec![]);
        assert_eq!(ch.name(), "imessage");
    }

    #[test]
    fn wildcard_among_others_still_allows_all() {
        let ch = IMessageChannel::new(vec!["+111".into(), "*".into(), "+222".into()]);
        assert!(ch.is_contact_allowed("totally-unknown"));
    }

    #[test]
    fn contact_with_spaces_exact_match() {
        let ch = IMessageChannel::new(vec!["  spaced  ".into()]);
        assert!(ch.is_contact_allowed("  spaced  "));
        assert!(!ch.is_contact_allowed("spaced"));
    }
}
