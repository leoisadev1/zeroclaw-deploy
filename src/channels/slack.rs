use super::traits::{Channel, ChannelMessage};
use async_trait::async_trait;
use uuid::Uuid;

/// Slack channel — polls conversations.history via Web API
pub struct SlackChannel {
    bot_token: String,
    channel_id: Option<String>,
    client: reqwest::Client,
}

impl SlackChannel {
    pub fn new(bot_token: String, channel_id: Option<String>) -> Self {
        Self {
            bot_token,
            channel_id,
            client: reqwest::Client::new(),
        }
    }

    /// Get the bot's own user ID so we can ignore our own messages
    async fn get_bot_user_id(&self) -> Option<String> {
        let resp: serde_json::Value = self
            .client
            .get("https://slack.com/api/auth.test")
            .bearer_auth(&self.bot_token)
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;

        resp.get("user_id")
            .and_then(|u| u.as_str())
            .map(String::from)
    }
}

#[async_trait]
impl Channel for SlackChannel {
    fn name(&self) -> &str {
        "slack"
    }

    async fn send(&self, message: &str, channel: &str) -> anyhow::Result<()> {
        let body = serde_json::json!({
            "channel": channel,
            "text": message
        });

        self.client
            .post("https://slack.com/api/chat.postMessage")
            .bearer_auth(&self.bot_token)
            .json(&body)
            .send()
            .await?;

        Ok(())
    }

    async fn listen(&self, tx: tokio::sync::mpsc::Sender<ChannelMessage>) -> anyhow::Result<()> {
        let channel_id = self
            .channel_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("Slack channel_id required for listening"))?;

        let bot_user_id = self.get_bot_user_id().await.unwrap_or_default();
        let mut last_ts = String::new();

        tracing::info!("Slack channel listening on #{channel_id}...");

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;

            let mut params = vec![
                ("channel", channel_id.clone()),
                ("limit", "10".to_string()),
            ];
            if !last_ts.is_empty() {
                params.push(("oldest", last_ts.clone()));
            }

            let resp = match self
                .client
                .get("https://slack.com/api/conversations.history")
                .bearer_auth(&self.bot_token)
                .query(&params)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("Slack poll error: {e}");
                    continue;
                }
            };

            let data: serde_json::Value = match resp.json().await {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("Slack parse error: {e}");
                    continue;
                }
            };

            if let Some(messages) = data.get("messages").and_then(|m| m.as_array()) {
                // Messages come newest-first, reverse to process oldest first
                for msg in messages.iter().rev() {
                    let ts = msg.get("ts").and_then(|t| t.as_str()).unwrap_or("");
                    let user = msg
                        .get("user")
                        .and_then(|u| u.as_str())
                        .unwrap_or("unknown");
                    let text = msg.get("text").and_then(|t| t.as_str()).unwrap_or("");

                    // Skip bot's own messages
                    if user == bot_user_id {
                        continue;
                    }

                    // Skip empty or already-seen
                    if text.is_empty() || ts <= last_ts.as_str() {
                        continue;
                    }

                    last_ts = ts.to_string();

                    let channel_msg = ChannelMessage {
                        id: Uuid::new_v4().to_string(),
                        sender: channel_id.clone(),
                        content: text.to_string(),
                        channel: "slack".to_string(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                    };

                    if tx.send(channel_msg).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }

    async fn health_check(&self) -> bool {
        self.client
            .get("https://slack.com/api/auth.test")
            .bearer_auth(&self.bot_token)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slack_channel_name() {
        let ch = SlackChannel::new("xoxb-fake".into(), None);
        assert_eq!(ch.name(), "slack");
    }

    #[test]
    fn slack_channel_with_channel_id() {
        let ch = SlackChannel::new("xoxb-fake".into(), Some("C12345".into()));
        assert_eq!(ch.channel_id, Some("C12345".to_string()));
    }
}
