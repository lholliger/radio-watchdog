use tracing::{trace, warn};


pub struct SlackMessageSender {
    authorization: String,
    channel_id: String
}

impl SlackMessageSender {
    pub fn new(auth: String, channel: String) -> SlackMessageSender {
        SlackMessageSender {
            authorization: auth,
            channel_id: channel
        }
    }

    pub fn send(&self, message: String) -> bool {
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