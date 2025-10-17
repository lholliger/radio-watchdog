use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::RwLock;
use rusty_chromaprint::{match_fingerprints, Configuration};
use tracing::{info, error, debug};
use super::audiorouter::AudioRouter;
use super::alertmanager::AlertManager;

#[derive(Clone, Debug)]
pub struct ComparisonResult {
    pub stream1: String,
    pub stream2: String,
    pub similarity_percent: f32,
    pub is_within_channel: bool,
    pub is_error: bool,
    pub offset_seconds: Option<f32>, // Time offset between streams (only for within-channel)
}

pub struct StreamComparator {
    router: Arc<AudioRouter>,
    window_size: usize,
    min_match_duration: f32, // minimum similarity duration in seconds
    min_buffer_size: usize, // minimum fingerprint buffer before comparisons start
    match_threshold: f32, // percentage threshold for within-channel matching
    divergence_threshold: f32, // percentage threshold for cross-channel divergence
    pub comparison_results: Arc<RwLock<Vec<ComparisonResult>>>,
    alert_manager: Option<Arc<AlertManager>>,
}

impl StreamComparator {
    pub fn new(
        router: Arc<AudioRouter>,
        comparison_duration: f32,
        min_buffer_duration: f32,
        match_threshold: f32,
        divergence_threshold: f32
    ) -> Self {
        let window_size = (comparison_duration / Configuration::preset_test1().item_duration_in_seconds()) as usize;
        let min_buffer_size = (min_buffer_duration / Configuration::preset_test1().item_duration_in_seconds()) as usize;

        StreamComparator {
            router,
            window_size,
            min_match_duration: comparison_duration * (match_threshold / 100.0),
            min_buffer_size,
            match_threshold,
            divergence_threshold,
            comparison_results: Arc::new(RwLock::new(Vec::new())),
            alert_manager: None,
        }
    }

    pub fn with_alert_manager(mut self, alert_manager: Arc<AlertManager>) -> Self {
        self.alert_manager = Some(alert_manager);
        self
    }

    pub fn get_results(&self) -> Arc<RwLock<Vec<ComparisonResult>>> {
        self.comparison_results.clone()
    }

    pub async fn start_comparison_loop(&self) {
        info!("Starting fingerprint comparison loop (window: {} items, min match: {}s, min buffer: {} items)",
              self.window_size, self.min_match_duration, self.min_buffer_size);
        let router = self.router.clone();
        let window_size = self.window_size;
        let min_match = self.min_match_duration;
        let min_buffer = self.min_buffer_size;
        let match_threshold = self.match_threshold;
        let divergence_threshold = self.divergence_threshold;
        let results = self.comparison_results.clone();
        let alert_manager = self.alert_manager.clone();

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;

                let mut new_results = Vec::new();

                // Compare streams within each channel (should be identical)
                for channel_name in router.get_all_channels() {
                    if let Some(stream_names) = router.get_channel_streams(&channel_name) {
                        let channel_results = Self::compare_channel_streams(&router, &channel_name, &stream_names, window_size, min_match, min_buffer, match_threshold).await;
                        new_results.extend(channel_results);
                    }
                }

                // Compare across channels (should be different)
                // This includes comparing real channels against the silence channel
                let mut channels = router.get_all_channels();
                channels.sort();
                for i in 0..channels.len() {
                    for j in (i + 1)..channels.len() {
                        let cross_results = Self::compare_across_channels(&router, &channels[i], &channels[j], window_size, min_buffer, divergence_threshold).await;
                        new_results.extend(cross_results);
                    }
                }

                // Update alert manager if configured
                if let Some(ref am) = alert_manager {
                    for result in &new_results {
                        let alert_id = format!("{}_{}", result.stream1, result.stream2);
                        let message = if result.is_within_channel {
                            if result.is_error {
                                format!("Streams `{}` and `{}` are diverging ({:.1}% similar, need ≥{:.1}%)",
                                    result.stream1, result.stream2, result.similarity_percent, match_threshold)
                            } else {
                                format!("Streams `{}` and `{}` are matching ({:.1}% similar)",
                                    result.stream1, result.stream2, result.similarity_percent)
                            }
                        } else {
                            if result.is_error {
                                format!("Streams `{}` and `{}` are colliding ({:.1}% similar, need <{:.1}%)",
                                    result.stream1, result.stream2, result.similarity_percent, divergence_threshold)
                            } else {
                                format!("Streams `{}` and `{}` are different ({:.1}% similar)",
                                    result.stream1, result.stream2, result.similarity_percent)
                            }
                        };
                        am.update_alert(alert_id, result.is_error, message).await;
                    }
                }

                // Update results
                *results.write().await = new_results;
            }
        });
    }

    async fn compare_channel_streams(
        router: &AudioRouter,
        channel_name: &str,
        stream_names: &[String],
        window_size: usize,
        min_match_duration: f32,
        min_buffer_size: usize,
        match_threshold: f32
    ) -> Vec<ComparisonResult> {
        let mut results = Vec::new();
        if stream_names.len() < 2 {
            return results; // Nothing to compare
        }

        let mut fingerprints: HashMap<String, Vec<u32>> = HashMap::new();

        // Collect fingerprints from all streams
        for stream_name in stream_names {
            if let Some(fp) = router.get_stream_fingerprint(stream_name).await {
                if fp.len() >= min_buffer_size {
                    fingerprints.insert(stream_name.clone(), fp);
                } else {
                    debug!("Stream {} fingerprint buffering ({}/{} items)", stream_name, fp.len(), min_buffer_size);
                }
            }
        }

        if fingerprints.len() < 2 {
            return results; // Not enough data to compare
        }

        // Compare each pair
        let mut streams: Vec<_> = fingerprints.keys().cloned().collect();
        streams.sort();
        for i in 0..streams.len() {
            for j in (i + 1)..streams.len() {
                let fp1 = &fingerprints[&streams[i]];
                let fp2 = &fingerprints[&streams[j]];

                if let Some((similar_time, offset)) = Self::get_similarity_time(fp1, fp2, window_size) {
                    let total_duration = fp1.len() as f32 * Configuration::preset_test1().item_duration_in_seconds();
                    let similarity_percent = (similar_time / total_duration) * 100.0;

                    let is_error = similarity_percent < match_threshold;

                    // Order streams alphabetically for consistent display
                    let (stream1, stream2, final_offset) = if streams[i] < streams[j] {
                        (streams[i].clone(), streams[j].clone(), offset)
                    } else {
                        (streams[j].clone(), streams[i].clone(), -offset)
                    };

                    if is_error {
                        error!(
                            "DIVERGENCE in channel '{}': '{}' vs '{}' only {:.1}% similar (need {:.1}%), offset: {:.2}s",
                            channel_name, stream1, stream2, similarity_percent, match_threshold, final_offset
                        );
                    } else {
                        info!(
                            "Channel '{}': '{}' vs '{}' {:.1}% similar, offset: {:.2}s ✓",
                            channel_name, stream1, stream2, similarity_percent, final_offset
                        );
                    }

                    results.push(ComparisonResult {
                        stream1,
                        stream2,
                        similarity_percent,
                        is_within_channel: true,
                        is_error,
                        offset_seconds: Some(final_offset),
                    });
                } else {
                    debug!("Channel '{}': Could not compare '{}' and '{}'", channel_name, streams[i], streams[j]);
                }
            }
        }

        results
    }

    async fn compare_across_channels(
        router: &AudioRouter,
        channel1: &str,
        channel2: &str,
        window_size: usize,
        min_buffer_size: usize,
        divergence_threshold: f32
    ) -> Vec<ComparisonResult> {
        let mut results = Vec::new();
        let streams1 = router.get_channel_streams(channel1);
        let streams2 = router.get_channel_streams(channel2);

        if streams1.is_none() || streams2.is_none() {
            return results;
        }

        let streams1 = streams1.unwrap();
        let streams2 = streams2.unwrap();

        // Compare each stream from channel1 against each stream from channel2
        for stream1_name in &streams1 {
            for stream2_name in &streams2 {
                let fp1 = router.get_stream_fingerprint(stream1_name).await;
                let fp2 = router.get_stream_fingerprint(stream2_name).await;

                if let (Some(fp1), Some(fp2)) = (fp1, fp2) {
                    if fp1.len() >= min_buffer_size && fp2.len() >= min_buffer_size {
                        if let Some((similar_time, _offset)) = Self::get_similarity_time(&fp1, &fp2, window_size) {
                            let total_duration = fp1.len() as f32 * Configuration::preset_test1().item_duration_in_seconds();
                            let similarity_percent = (similar_time / total_duration) * 100.0;

                            // For different channels, we want LOW similarity (under divergence threshold)
                            let is_error = similarity_percent > divergence_threshold;

                            // Order streams alphabetically for consistent display
                            let (stream1, stream2) = if stream1_name < stream2_name {
                                (stream1_name.clone(), stream2_name.clone())
                            } else {
                                (stream2_name.clone(), stream1_name.clone())
                            };

                            if is_error {
                                error!(
                                    "COLLISION: '{}' and '{}' are too similar ({:.1}% match, should be <{:.1}%)",
                                    stream1, stream2, similarity_percent, divergence_threshold
                                );
                            } else {
                                debug!(
                                    "Cross-channel: '{}' and '{}' are different ({:.1}% match) ✓",
                                    stream1, stream2, similarity_percent
                                );
                            }

                            results.push(ComparisonResult {
                                stream1,
                                stream2,
                                similarity_percent,
                                is_within_channel: false,
                                is_error,
                                offset_seconds: None, // Offset not relevant for cross-channel
                            });
                        }
                    }
                }
            }
        }

        results
    }

    fn get_similarity_time(fp1: &[u32], fp2: &[u32], window_size: usize) -> Option<(f32, f32)> {
        if fp1.len() < window_size || fp2.len() < window_size {
            return None;
        }

        let matches = match_fingerprints(
            fp1,
            fp2,
            &Configuration::preset_test1()
        ).ok()?;

        let mut total_similar_time = 0.0;
        let mut avg_offset = 0.0;
        let mut match_count = 0;

        for m in matches.iter() {
            total_similar_time += m.duration(&Configuration::preset_test1());
            // Calculate offset: positive means fp2 is ahead of fp1
            avg_offset += (m.offset2 as f32 - m.offset1 as f32) * Configuration::preset_test1().item_duration_in_seconds();
            match_count += 1;
        }

        if match_count > 0 {
            avg_offset /= match_count as f32;
        }

        Some((total_similar_time, avg_offset))
    }
}
