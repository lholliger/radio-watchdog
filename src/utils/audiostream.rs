use std::sync::Arc;

use rusty_chromaprint::{Configuration, Fingerprinter};
use tokio::sync::{broadcast::Receiver, Mutex};
use tracing::warn;
use chrono::{DateTime, Utc};
use super::volumedetect::{VolumeDetector, VolumeMetrics};

#[derive(Debug, Clone, PartialEq)]
pub enum AudioStreamHealth {
    Running,
    NoData,
    Degraded,
    Dead
}

pub struct AudioStream {
    output: Arc<Mutex<Vec<u32>>>, // fingerprint data
    health: Arc<Mutex<AudioStreamHealth>>,
    last_fingerprint_update: Arc<Mutex<DateTime<Utc>>>,
    volume_detector: VolumeDetector
}

impl AudioStream {
    pub fn new(mut input: Receiver<Vec<u8>>, buffer_duration: f32) -> Self {
        let output = Arc::new(Mutex::new(vec![]));
        let health = Arc::new(Mutex::new(AudioStreamHealth::NoData));
        let last_update = Arc::new(Mutex::new(Utc::now()));

        let thread_out = output.clone();
        let thread_health = health.clone();
        let thread_last_update = last_update.clone();

        // Create a second receiver for volume detection
        let volume_input = input.resubscribe();
        let volume_detector = VolumeDetector::new(volume_input, buffer_duration);

        let stream = AudioStream {
            output,
            health,
            last_fingerprint_update: last_update,
            volume_detector
        };

        // Calculate record size based on configured buffer duration
        // Each fingerprint item represents ~0.1238 seconds (from Configuration::preset_test1())
        let record_size = (buffer_duration / Configuration::preset_test1().item_duration_in_seconds()) as usize;

        // Capture runtime handle before spawning thread
        let rt = tokio::runtime::Handle::current();

        std::thread::spawn(move || {
            let mut fingerprinter = Fingerprinter::new(&Configuration::preset_test1());
            fingerprinter.start(44100, 2).unwrap();
            loop {
                let samples = match rt.block_on(input.recv()) {
                    Ok(data) => {
                        rt.block_on(async {
                            *thread_health.lock().await = AudioStreamHealth::Running;
                        });
                        unsafe {
                            std::slice::from_raw_parts(
                                data.as_ptr() as *const i16,
                                data.len() / 2
                            )
                        }
                    },
                    Err(e) => {
                        warn!("AudioStream input closed: {:?}", e);
                        rt.block_on(async {
                            *thread_health.lock().await = AudioStreamHealth::Dead;
                        });
                        break;
                    }
                };

                fingerprinter.consume(samples);
                let fingerprint = fingerprinter.fingerprint();

                if fingerprint.is_empty() {
                    // Empty fingerprints are normal at startup while buffering
                    rt.block_on(async {
                        *thread_health.lock().await = AudioStreamHealth::NoData;
                    });
                } else {
                    rt.block_on(async {
                        *thread_health.lock().await = AudioStreamHealth::Running;
                        *thread_last_update.lock().await = Utc::now();
                    });
                }

                rt.block_on(async {
                    let mut fingerprint_content = thread_out.lock().await;
                    fingerprint_content.clear();
                    fingerprint_content.extend(fingerprint.to_vec());
                    if fingerprint_content.len() > record_size {
                        let start = fingerprint_content.len() - record_size;
                        *fingerprint_content = fingerprint_content.split_off(start);
                    }
                });
            }
        });

        stream
    }

    pub async fn get_fingerprint(&self) -> Vec<u32> {
        self.output.lock().await.clone()
    }

    pub async fn get_health(&self) -> AudioStreamHealth {
        self.health.lock().await.clone()
    }

    pub async fn get_last_update(&self) -> DateTime<Utc> {
        *self.last_fingerprint_update.lock().await
    }

    pub async fn get_volume_metrics(&self) -> VolumeMetrics {
        self.volume_detector.get_metrics().await
    }
}