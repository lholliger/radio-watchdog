use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tracing::{info, warn, error};
use super::commandprocessor::{CommandHolder, StreamHealth};
use super::audiostream::{AudioStream, AudioStreamHealth};

pub struct StreamInfo {
    command: CommandHolder,
    audio: AudioStream,
}

pub struct AudioRouter {
    streams: Arc<Mutex<HashMap<String, StreamInfo>>>,
    channels: HashMap<String, Vec<String>>, // channel -> list of stream names
}

impl AudioRouter {
    pub fn new() -> Self {
        AudioRouter {
            streams: Arc::new(Mutex::new(HashMap::new())),
            channels: HashMap::new(),
        }
    }

    pub async fn add_stream(&mut self, stream_name: &String, channel_name: &String, buffer_duration: f32, command_holder: CommandHolder) {
        // Create channel if not exists
        if !self.channels.contains_key(channel_name) {
            self.channels.insert(channel_name.to_string(), vec![]);
        }

        // Add stream to channel
        if let Some(streams) = self.channels.get_mut(channel_name) {
            streams.push(stream_name.to_string());
        }

        // Create AudioStream from CommandHolder (uses a reader from it)
        let reader = command_holder.get_reader();
        let audio = AudioStream::new(reader, buffer_duration);
        let stream_info = StreamInfo {
            command: command_holder,
            audio,
        };

        // Store stream
        self.streams.lock().await.insert(stream_name.clone(), stream_info);
    }

    pub async fn start_supervisor(&self) {
        info!("Starting AudioRouter supervisor");
        let streams = self.streams.clone();

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;

                let mut streams_lock = streams.lock().await;
                for (name, stream_info) in streams_lock.iter_mut() {
                    let cmd_health = stream_info.command.get_health().await;
                    let audio_health = stream_info.audio.get_health().await;

                    match cmd_health {
                        StreamHealth::Dead => {
                            error!("Stream {} command is dead, attempting respawn", name);
                            if stream_info.command.respawn().await {
                                info!("Stream {} successfully respawned", name);
                            } else {
                                error!("Stream {} failed to respawn (max restarts exceeded)", name);
                            }
                        },
                        StreamHealth::Stalled => {
                            warn!("Stream {} command is stalled", name);
                        },
                        StreamHealth::Running => {
                            match audio_health {
                                AudioStreamHealth::Dead => {
                                    error!("Stream {} audio processing is dead", name);
                                },
                                AudioStreamHealth::Degraded => {
                                    warn!("Stream {} audio processing degraded", name);
                                },
                                AudioStreamHealth::NoData => {
                                    warn!("Stream {} has no audio data yet", name);
                                },
                                AudioStreamHealth::Running => {
                                    // All good
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    pub async fn get_stream_fingerprint(&self, stream_name: &str) -> Option<Vec<u32>> {
        let streams = self.streams.lock().await;
        if let Some(stream_info) = streams.get(stream_name) {
            Some(stream_info.audio.get_fingerprint().await)
        } else {
            None
        }
    }

    pub async fn get_stream_health(&self, stream_name: &str) -> Option<(StreamHealth, AudioStreamHealth)> {
        let streams = self.streams.lock().await;
        if let Some(stream_info) = streams.get(stream_name) {
            let cmd_health = stream_info.command.get_health().await;
            let audio_health = stream_info.audio.get_health().await;
            Some((cmd_health, audio_health))
        } else {
            None
        }
    }

    pub async fn get_stream_uptime(&self, stream_name: &str) -> Option<chrono::Duration> {
        let streams = self.streams.lock().await;
        if let Some(stream_info) = streams.get(stream_name) {
            Some(stream_info.command.get_uptime())
        } else {
            None
        }
    }

    pub fn get_channel_streams(&self, channel_name: &str) -> Option<Vec<String>> {
        self.channels.get(channel_name).cloned()
    }

    pub fn get_all_channels(&self) -> Vec<String> {
        self.channels.keys().cloned().collect()
    }
}