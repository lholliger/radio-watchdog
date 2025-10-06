use tracing::{debug, info, trace, warn};


pub struct SlackMessageSender {
    authorization: String,
    channel_id: String,
    dry_run: bool,
}

impl SlackMessageSender {
    pub fn new(auth: String, channel: String, dry_run: bool) -> SlackMessageSender {
        if dry_run {
            warn!("Running in DRY RUN mode, no slack messages will be sent!");
        }
        SlackMessageSender {
            authorization: auth,
            channel_id: channel,
            dry_run
        }
    }

    pub async fn send(&self, message: String) -> bool {
        if self.dry_run {
            info!("DRY RUN: Sending Slack Message: {}", message);
            return true;
        }

        let json_payload = serde_json::json!({
            "channel": self.channel_id,
            "text": message
        });
        
        let json_str = serde_json::to_string(&json_payload).unwrap();


        let client = reqwest::Client::new()
            .post("https://slack.com/api/chat.postMessage")
            .header("User-Agent", "wrek-watchdog/1.0")
            .header("Authorization", format!("Bearer {}", self.authorization))
            .header("Content-Type", "application/json")
            .body(json_str)
            .send()
            .await;
        match client {
            Ok(res) => {
                if res.status().is_success() {
                    debug!("Slack message sent successfully!");
                    return true;
                } else {
                    warn!("Failed to send Slack message: {:?}", res.text().await);
                    return false;
                }
            },
            Err(e) => {
                warn!("Failed to send slack message: {:?}", e);
                return false;
            }
        }
    }
}