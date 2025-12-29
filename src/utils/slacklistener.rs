use std::sync::Arc;
use tracing::{info, warn, error, debug, trace};
use serde::{Deserialize, Serialize};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

use super::slack::SlackMessageSender;
use super::audiorouter::AudioRouter;

#[derive(Debug, Deserialize)]
struct SocketModeEnvelope {
    envelope_id: String,
    #[serde(rename = "type")]
    event_type: String,
    payload: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct EventPayload {
    event: Option<SlackEvent>,
}

#[derive(Debug, Deserialize)]
struct SlackEvent {
    #[serde(rename = "type")]
    event_type: String,
    text: Option<String>,
    channel: Option<String>,
    user: Option<String>,
    ts: Option<String>,
    bot_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct AckMessage {
    envelope_id: String,
}

pub struct SlackListener {
    app_token: String,
    bot_user_id: String,
    slack_sender: Arc<SlackMessageSender>,
    audio_router: Arc<AudioRouter>,
    dry_run: bool,
}

impl SlackListener {
    pub fn new(
        app_token: String,
        bot_user_id: String,
        slack_sender: Arc<SlackMessageSender>,
        audio_router: Arc<AudioRouter>,
        dry_run: bool,
    ) -> Self {
        SlackListener {
            app_token,
            bot_user_id,
            slack_sender,
            audio_router,
            dry_run,
        }
    }

    async fn get_websocket_url(&self) -> Result<String, String> {
        let client = reqwest::Client::new();
        let response = client
            .post("https://slack.com/api/apps.connections.open")
            .header("Authorization", format!("Bearer {}", self.app_token))
            .header("Content-Type", "application/json")
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {}", e))?;

        if !response.status().is_success() {
            return Err(format!("HTTP {} response", response.status()));
        }

        let json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse JSON: {}", e))?;

        if let Some(ok) = json.get("ok") {
            if !ok.as_bool().unwrap_or(false) {
                let error = json.get("error").and_then(|e| e.as_str()).unwrap_or("unknown");
                return Err(format!("Slack API error: {}", error));
            }
        }

        json.get("url")
            .and_then(|u| u.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| "No URL in response".to_string())
    }

    pub async fn start(&mut self) {
        if self.dry_run {
            info!("DRY RUN: SlackListener would connect to Slack Socket Mode");
            return;
        }

        info!("Starting Slack Socket Mode listener");

        loop {
            // Get WebSocket URL from Slack API
            let ws_url = match self.get_websocket_url().await {
                Ok(url) => url,
                Err(e) => {
                    error!("Failed to get WebSocket URL: {}", e);
                    warn!("Retrying in 10 seconds...");
                    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                    continue;
                }
            };

            info!("Connecting to WebSocket URL: {}", ws_url);
            match connect_async(&ws_url).await {
                Ok((ws_stream, _)) => {
                    info!("Connected to Slack Socket Mode");
                    let (mut write, mut read) = ws_stream.split();

                    while let Some(msg) = read.next().await {
                        //trace!("msg: {:?}", msg);
                        match msg {
                            Ok(Message::Text(text)) => {
                                debug!("Received message: {}", text);

                                if let Ok(envelope) = serde_json::from_str::<SocketModeEnvelope>(&text) {
                                    // Send acknowledgment
                                    let ack = AckMessage {
                                        envelope_id: envelope.envelope_id.clone(),
                                    };
                                    if let Ok(ack_json) = serde_json::to_string(&ack) {
                                        if let Err(e) = write.send(Message::Text(ack_json)).await {
                                            error!("Failed to send ack: {:?}", e);
                                        }
                                    }

                                    // Handle the event
                                    if envelope.event_type == "events_api" {
                                        if let Some(payload) = envelope.payload {
                                            if let Ok(event_payload) = serde_json::from_value::<EventPayload>(payload) {
                                                if let Some(event) = event_payload.event {
                                                    self.handle_event(event).await;
                                                }
                                            }
                                        }
                                    } else if envelope.event_type == "hello" {
                                        info!("Received hello from Slack");
                                    }
                                }
                            }
                            Ok(Message::Close(_)) => {
                                warn!("WebSocket connection closed by Slack");
                                break;
                            }
                            Err(e) => {
                                error!("WebSocket error: {:?}", e);
                                break;
                            }
                            _ => {}
                        }
                    }

                    warn!("WebSocket connection ended, reconnecting in 5 seconds...");
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                }
                Err(e) => {
                    error!("Failed to connect to Slack: {:?}", e);
                    warn!("Retrying in 10 seconds...");
                    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;
                }
            }
        }
    }

    async fn handle_event(&mut self, event: SlackEvent) {
        // Skip messages from bots first
        if event.bot_id.is_some() {
            debug!("Skipping bot message");
            return;
        }

        let text = match event.text {
            Some(t) => t,
            None => return,
        };

        // Only respond to app mentions OR messages that mention the bot
        let is_app_mention = event.event_type == "app_mention";
        let bot_mention = format!("<@{}>", self.bot_user_id);
        let contains_bot_mention = text.contains(&bot_mention);

        if !is_app_mention && !contains_bot_mention {
            debug!("Ignoring event type: {} (no mention)", event.event_type);
            return;
        }

        // Skip messages from the bot itself, like me doing discord bots as a user account
        if let Some(user) = &event.user {
            if user == &self.bot_user_id {
                return;
            }
        }

        info!("Processing message: {}", text);

        // Parse command
        let response = self.parse_and_execute_command(&text).await;

        // Send response back to Slack
        if event.channel.is_some() {
            let message = format!("{}", response);
            self.slack_sender.send(message).await;
        }

        // special case
        if text.trim().to_lowercase().contains("yeller") {
            info!("Time to go out back...");
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            std::process::exit(0);
        }
    }

    async fn parse_and_execute_command(&self, text: &str) -> String {
        // Remove bot mention if present
        let cleaned_text = text
            .split_whitespace()
            .filter(|word| !word.starts_with("<@"))
            .collect::<Vec<_>>()
            .join(" ");

        let parts: Vec<&str> = cleaned_text.trim().split_whitespace().collect();

        if parts.is_empty() {
            return "Available commands: `status`, `list`, `restart <stream>`, `help`, `yeller`".to_string();
        }

        match parts[0].to_lowercase().as_str() {
            "help" => {
                "Here are the commands I learned!\n\
                • `status` - Show health of all streams\n\
                • `list` - List all stream names\n\
                • `restart <stream_name>` - Restart a specific stream\n\
                • `help` - Show this help message\n\
                • `yeller` - Bark bark!".to_string()
            }
            "status" => {
                self.get_status().await
            }
            "list" => {
                self.list_streams().await
            }
            "restart" => {
                if parts.len() < 2 {
                    return "Usage: `restart <stream_name>`".to_string();
                }
                let stream_name = parts[1];
                self.restart_stream(stream_name).await
            }
            "yeller" => {
                "Bark bark!".to_string()
            }
            _ => {
                format!("Woof? I don't know that command. Try `help` for available commands.")
            }
        }
    }

    async fn get_status(&self) -> String {
        let streams = self.audio_router.get_all_streams().await;

        if streams.is_empty() {
            return "No streams configured.".to_string();
        }

        let mut status_lines = vec!["*Stream Status:*".to_string()];
        for (name, cmd_health, audio_health) in streams {
            let status = format!(
                "• `{}`: Command={:?}, Audio={:?}",
                name, cmd_health, audio_health
            );
            status_lines.push(status);
        }

        status_lines.join("\n")
    }

    async fn list_streams(&self) -> String {
        let streams = self.audio_router.get_all_streams().await;

        if streams.is_empty() {
            return "No streams configured.".to_string();
        }

        let stream_names: Vec<String> = streams.iter().map(|(name, _, _)| format!("• `{}`", name)).collect();
        format!("*Configured Streams:*\n{}", stream_names.join("\n"))
    }

    async fn restart_stream(&self, stream_name: &str) -> String {
        match self.audio_router.restart_stream(stream_name).await {
            Ok(_) => format!("Successfully restarted stream `{}`", stream_name),
            Err(e) => format!("Failed to restart stream `{}`: {}", stream_name, e),
        }
    }
}
