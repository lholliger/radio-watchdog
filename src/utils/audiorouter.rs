use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tracing::{info, warn, error, debug};
use crate::utils::alertmanager::AlertManager;

use super::commandprocessor::{CommandHolder, StreamHealth};
use super::audiostream::{AudioStream, AudioStreamHealth};
use super::volumedetect::VolumeMetrics;

pub struct StreamInfo {
    command: CommandHolder,
    audio: AudioStream,
}

pub struct AudioRouter {
    streams: Arc<Mutex<HashMap<String, StreamInfo>>>,
    channels: HashMap<String, Vec<String>>, // channel -> list of stream names
    volume_metrics: Arc<Mutex<HashMap<String, VolumeMetrics>>>, // stream name -> volume metrics
    alert_manager: Option<Arc<AlertManager>>,
    minimum_max_volume_threshold: Option<f32>
}

impl AudioRouter {
    pub fn new() -> Self {
        AudioRouter {
            streams: Arc::new(Mutex::new(HashMap::new())),
            channels: HashMap::new(),
            volume_metrics: Arc::new(Mutex::new(HashMap::new())),
            alert_manager: None,
            minimum_max_volume_threshold: None
        }
    }

    pub fn with_alert_manager(mut self, alert_manager: Arc<AlertManager>, minimum_max_volume_threshold: f32) -> Self {
        self.alert_manager = Some(alert_manager);
        self.minimum_max_volume_threshold = Some(minimum_max_volume_threshold);
        self
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
                                    error!("Stream {} audio processing is dead, attempting respawn", name);
                                    if stream_info.command.respawn().await {
                                        info!("Stream {} successfully respawned due to dead audio", name);
                                    } else {
                                        error!("Stream {} failed to respawn (max restarts exceeded)", name);
                                    }
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

    pub async fn get_stream_volume(&self, stream_name: &str) -> Option<VolumeMetrics> {
        let metrics = self.volume_metrics.lock().await;
        metrics.get(stream_name).copied()
    }

    pub async fn get_all_stream_volumes(&self) -> HashMap<String, VolumeMetrics> {
        self.volume_metrics.lock().await.clone()
    }

    pub async fn start_volume_detection_loop(&self, interval_seconds: u64) {
        info!("Starting volume detection loop (interval: {}s)", interval_seconds);
        let streams = self.streams.clone();
        let volume_metrics = self.volume_metrics.clone();
        let alert_manager = self.alert_manager.clone();
        let minimum_max_volume_threshold = self.minimum_max_volume_threshold;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(interval_seconds)).await;

                let streams_lock = streams.lock().await;
                let stream_names: Vec<String> = streams_lock.keys().cloned().collect();
                drop(streams_lock);

                // Collect volume metrics for all streams
                let mut new_metrics = HashMap::new();
                for stream_name in stream_names {
                    let streams_lock = streams.lock().await;
                    if let Some(stream_info) = streams_lock.get(&stream_name) {
                        let metrics = stream_info.audio.get_volume_metrics().await;
                        new_metrics.insert(stream_name.clone(), metrics);
                        debug!("Stream '{}': mean={:.1} dB, max={:.1} dB",
                            stream_name, metrics.mean_volume, metrics.max_volume);
                        if let Some(ref am) = alert_manager {
                        let alert_id = format!("{}_{}", stream_name, "silence");
                        let is_error = metrics.max_volume < minimum_max_volume_threshold.unwrap();
                        let message = if is_error {
                                format!("Stream `{}` is silent ({:.1} dB, need ≥{:.1} dB)",
                                    stream_name, metrics.max_volume, minimum_max_volume_threshold.unwrap())
                            } else {
                                format!("Stream `{}` is playing normally again ({:.1} dB)",
                                    stream_name, metrics.max_volume)
                            };
                            am.update_alert(alert_id, is_error, message).await;
                        }
                    }
                    drop(streams_lock);
                }

                // Update stored metrics
                *volume_metrics.lock().await = new_metrics;
            }
        });
    }

    pub async fn get_all_streams(&self) -> Vec<(String, StreamHealth, super::audiostream::AudioStreamHealth)> {
        let streams = self.streams.lock().await;
        let mut result = Vec::new();

        for (name, stream_info) in streams.iter() {
            let cmd_health = stream_info.command.get_health().await;
            let audio_health = stream_info.audio.get_health().await;
            result.push((name.clone(), cmd_health, audio_health));
        }

        result
    }

    pub async fn restart_stream(&self, stream_name: &str) -> Result<(), String> {
        let mut streams = self.streams.lock().await;

        match streams.get_mut(stream_name) {
            Some(stream_info) => {
                info!("Restarting stream '{}' via command", stream_name);
                if stream_info.command.respawn().await {
                    Ok(())
                } else {
                    Err("Max restarts exceeded".to_string())
                }
            }
            None => Err(format!("Stream '{}' not found", stream_name)),
        }
    }
}