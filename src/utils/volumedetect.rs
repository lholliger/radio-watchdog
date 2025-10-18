use std::sync::Arc;
use std::collections::VecDeque;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{broadcast::Receiver, Mutex};
use std::process::Stdio;
use tracing::{warn, trace, error};

#[derive(Debug, Clone, Copy)]
pub struct VolumeMetrics {
    pub mean_volume: f32,
    pub max_volume: f32,
}

impl Default for VolumeMetrics {
    fn default() -> Self {
        VolumeMetrics {
            mean_volume: -100.0, // Very quiet default
            max_volume: -100.0,
        }
    }
}

pub struct VolumeDetector {
    buffer: Arc<Mutex<VecDeque<u8>>>,
    buffer_duration: f32,
}

impl VolumeDetector {
    pub fn new(mut input: Receiver<Vec<u8>>, buffer_duration: f32) -> Self {
        // Calculate max buffer size: 44100 Hz * 2 channels * 2 bytes/sample * duration
        let max_buffer_size = (44100.0 * 2.0 * 2.0 * buffer_duration) as usize;

        let buffer = Arc::new(Mutex::new(VecDeque::with_capacity(max_buffer_size)));
        let thread_buffer = buffer.clone();

        // Spawn a task to continuously fill the circular buffer
        tokio::spawn(async move {
            loop {
                match input.recv().await {
                    Ok(data) => {
                        let mut buf = thread_buffer.lock().await;

                        // Add new data to buffer
                        buf.extend(data.iter());

                        // Trim buffer if it exceeds max size
                        while buf.len() > max_buffer_size {
                            buf.pop_front();
                        }
                    },
                    Err(e) => {
                        warn!("VolumeDetector input closed: {:?}", e);
                        break;
                    }
                }
            }
        });

        VolumeDetector {
            buffer,
            buffer_duration,
        }
    }

    /// Analyzes the current buffered audio and returns volume metrics
    /// This spawns ffmpeg on-demand to analyze the sliding window
    pub async fn get_metrics(&self) -> VolumeMetrics {
        let buffer_snapshot = {
            let buf = self.buffer.lock().await;
            Vec::from_iter(buf.iter().copied())
        };

        // If buffer is empty or too small, return default
        if buffer_snapshot.len() < 1024 {
            return VolumeMetrics::default();
        }

        // Spawn ffmpeg to analyze the buffered audio
        let mut child = match Command::new("ffmpeg")
            .args(&[
                "-f", "s16le",              // Input format: signed 16-bit little-endian PCM
                "-ar", "44100",              // Sample rate
                "-ac", "2",                  // 2 channels (stereo)
                "-i", "pipe:0",              // Read from stdin
                "-af", "volumedetect",       // Apply volume detect filter
                "-f", "null",                // No output file
                "-",                         // Output to null
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                error!("Failed to spawn ffmpeg for volume detection: {:?}", e);
                return VolumeMetrics::default();
            }
        };

        // Write buffered data to ffmpeg stdin
        if let Some(mut stdin) = child.stdin.take() {
            if let Err(e) = stdin.write_all(&buffer_snapshot).await {
                error!("Failed to write buffer to ffmpeg: {:?}", e);
                return VolumeMetrics::default();
            }
            drop(stdin); // Close stdin to signal end of input
        }

        // Parse stderr for volume metrics
        let mut mean_vol: Option<f32> = None;
        let mut max_vol: Option<f32> = None;

        if let Some(stderr) = child.stderr.take() {
            let reader = tokio::io::BufReader::new(stderr);
            let mut lines = reader.lines();

            while let Ok(Some(line)) = lines.next_line().await {
                trace!("ffmpeg volumedetect: {}", line);

                // Parse mean_volume line
                if line.contains("mean_volume:") {
                    if let Some(value_str) = line.split("mean_volume:").nth(1) {
                        let value_str = value_str.trim().trim_end_matches(" dB");
                        if let Ok(value) = value_str.parse::<f32>() {
                            mean_vol = Some(value);
                        }
                    }
                }

                // Parse max_volume line
                if line.contains("max_volume:") {
                    if let Some(value_str) = line.split("max_volume:").nth(1) {
                        let value_str = value_str.trim().trim_end_matches(" dB");
                        if let Ok(value) = value_str.parse::<f32>() {
                            max_vol = Some(value);
                        }
                    }
                }
            }
        }

        // Wait for process to complete
        let _ = child.wait().await;

        // Return parsed metrics or default
        match (mean_vol, max_vol) {
            (Some(mean), Some(max)) => {
                trace!("Volume metrics: mean={} dB, max={} dB", mean, max);
                VolumeMetrics {
                    mean_volume: mean,
                    max_volume: max,
                }
            },
            _ => {
                warn!("Failed to parse volume metrics from ffmpeg output");
                VolumeMetrics::default()
            }
        }
    }
}
