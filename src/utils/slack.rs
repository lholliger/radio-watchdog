use tracing::{info, trace, warn};


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

    pub fn send(&self, message: String) -> bool {
        if self.dry_run {
            info!("DRY RUN: Sending Slack Message: {}", message);
            return true;
        }
        let client = reqwest::blocking::Client::new(); // TODO: dont block
    
        match client
        .post("https://slack.com/api/chat.postMessage")
        .header("User-Agent", "wrek-watchdog/1.0")
        .header("Authorization",format!("Bearer {}", self.authorization))
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "channel": self.channel_id,
            "text": message
        })).send() {
            Ok(pass) => {
                trace!("HTTP PASS: {:?}", pass.text());
                true
            },
            Err(err) => {
                warn!("HTTP ERROR: {:?}", err);
                false
            }
        }
    }
}