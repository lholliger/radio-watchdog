use std::sync::Arc;
use axum::{
    extract::State,
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use chrono::Utc;
use maud::{html, Markup};
use tracing::info;

use super::audiorouter::AudioRouter;
use super::audiostream::AudioStreamHealth;
use super::commandprocessor::StreamHealth;
use super::comparator::ComparisonResult;
use super::volumedetect::VolumeMetrics;
use tokio::sync::RwLock;

fn format_duration(duration: chrono::Duration) -> String {
    let secs = duration.num_seconds();
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;

    if days > 0 {
        format!("{}d {}h {}m {}s", days, hours, minutes, seconds)
    } else if hours > 0 {
        format!("{}h {}m {}s", hours, minutes, seconds)
    } else if minutes > 0 {
        format!("{}m {}s", minutes, seconds)
    } else {
        format!("{}s", seconds)
    }
}

pub struct WebServer {
    router: Arc<AudioRouter>,
    comparison_results: Arc<RwLock<Vec<ComparisonResult>>>,
}

impl WebServer {
    pub fn new(router: Arc<AudioRouter>, comparison_results: Arc<RwLock<Vec<ComparisonResult>>>) -> Self {
        WebServer { router, comparison_results }
    }

    pub async fn start(self, port: u16) {
        let server = Arc::new(self);
        let app = Router::new()
            .route("/", get(status_page))
            .with_state(server);

        let addr = format!("0.0.0.0:{}", port);
        info!("Starting web server on {}", addr);

        let listener = tokio::net::TcpListener::bind(&addr)
            .await
            .expect("Failed to bind web server");

        axum::serve(listener, app)
            .await
            .expect("Failed to start web server");
    }
}

async fn status_page(State(server): State<Arc<WebServer>>) -> impl IntoResponse {
    let router = &server.router;
    let channels = router.get_all_channels();
    let mut channel_data = Vec::new();

    // Fetch all volume metrics at once
    let volume_metrics = router.get_all_stream_volumes().await;

    for channel_name in channels {
        if let Some(stream_names) = router.get_channel_streams(&channel_name) {
            let mut streams = Vec::new();

            for stream_name in stream_names {
                if let Some((cmd_health, audio_health)) = router.get_stream_health(&stream_name).await {
                    let uptime = router.get_stream_uptime(&stream_name).await;
                    let volume = volume_metrics.get(&stream_name).copied();
                    streams.push((stream_name, cmd_health, audio_health, uptime, volume));
                }
            }

            channel_data.push((channel_name, streams));
        }
    }

    let comparison_results = server.comparison_results.read().await.clone();

    let html = render_status_page(channel_data, comparison_results);
    Html(html.into_string())
}

fn render_status_page(
    channels: Vec<(String, Vec<(String, StreamHealth, AudioStreamHealth, Option<chrono::Duration>, Option<VolumeMetrics>)>)>,
    comparison_results: Vec<ComparisonResult>
) -> Markup {
    html! {
        (maud::DOCTYPE)
        html {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Watchdog Status" }
                style {
                    r#"
                    body {
                        font-family: sans-serif;
                        max-width: 1200px;
                        margin: 0 auto;
                        padding: 20px;
                        background: #1a1a1a;
                        color: #e0e0e0;
                    }
                    h1 {
                        color: #fff;
                        border-bottom: 2px solid #444;
                        padding-bottom: 10px;
                    }
                    h2 {
                        color: #fff;
                        margin-top: 30px;
                    }
                    .channel {
                        background: #2a2a2a;
                        border-radius: 8px;
                        padding: 20px;
                        margin: 20px 0;
                        border: 1px solid #444;
                    }
                    .stream {
                        background: #333;
                        padding: 15px;
                        margin: 10px 0;
                        border-radius: 5px;
                        display: flex;
                        justify-content: space-between;
                        align-items: center;
                    }
                    .stream-name {
                        font-weight: bold;
                        font-size: 1.1em;
                    }
                    .status {
                        display: flex;
                        gap: 15px;
                    }
                    .badge {
                        padding: 5px 12px;
                        border-radius: 4px;
                        font-size: 0.9em;
                        font-weight: 500;
                    }
                    .badge.running {
                        background: #2d5016;
                        color: #7fd13b;
                    }
                    .badge.stalled {
                        background: #5a3f00;
                        color: #ffa726;
                    }
                    .badge.dead, .badge.degraded {
                        background: #5c1c1c;
                        color: #ff6b6b;
                    }
                    .badge.nodata {
                        background: #3a3a3a;
                        color: #999;
                    }
                    .timestamp {
                        color: #888;
                        font-size: 0.9em;
                        margin-top: 10px;
                    }
                    table {
                        width: 100%;
                        border-collapse: collapse;
                        margin-top: 20px;
                        background: #2a2a2a;
                        border-radius: 8px;
                        overflow: hidden;
                    }
                    th, td {
                        padding: 12px;
                        text-align: left;
                        border-bottom: 1px solid #444;
                    }
                    th {
                        background: #333;
                        font-weight: 600;
                        color: #fff;
                    }
                    tr:last-child td {
                        border-bottom: none;
                    }
                    tr.error {
                        background: #3a1f1f;
                    }
                    tr.ok {
                        background: #1f2a1f;
                    }
                    .similarity {
                        font-weight: bold;
                    }
                    .similarity.good {
                        color: #7fd13b;
                    }
                    .similarity.bad {
                        color: #ff6b6b;
                    }
                    "#
                }
            }
            body {
                h1 { "🐕 Watchdog Status" }
                p.timestamp { "Last updated: " (Utc::now().format("%Y-%m-%d %H:%M:%S UTC")) }

                h2 { "Cross-Comparison Results" }

                @if !comparison_results.is_empty() {
                    div.channel {
                        h3 { "Within-Channel Comparisons" }
                        table {
                            thead {
                                tr {
                                    th { "Stream 1" }
                                    th { "Stream 2" }
                                    th { "Similarity" }
                                    th { "Offset" }
                                    th { "Status" }
                                }
                            }
                            tbody {
                                @for result in comparison_results.iter().filter(|r| r.is_within_channel) {
                                    tr class=@if result.is_error { "error" } @else { "ok" } {
                                        td { (result.stream1) }
                                        td { (result.stream2) }
                                        td class=({format!("similarity {}", if result.is_error { "bad" } else { "good" })}) {
                                            (format!("{:.1}%", result.similarity_percent))
                                        }
                                        td {
                                            @if let Some(offset) = result.offset_seconds {
                                                (format!("{:.2}s", offset))
                                            } @else {
                                                "-"
                                            }
                                        }
                                        td {
                                            @if result.is_error {
                                                span.badge.dead { "⚠ Diverging" }
                                            } @else {
                                                span.badge.running { "✓ Matching" }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        h3 style="margin-top: 30px;" { "Cross-Channel Comparisons" }
                        table {
                            thead {
                                tr {
                                    th { "Stream 1" }
                                    th { "Stream 2" }
                                    th { "Similarity" }
                                    th { "Status" }
                                }
                            }
                            tbody {
                                @for result in comparison_results.iter().filter(|r| !r.is_within_channel) {
                                    tr class=@if result.is_error { "error" } @else { "ok" } {
                                        td { (result.stream1) }
                                        td { (result.stream2) }
                                        td class=({format!("similarity {}", if result.is_error { "bad" } else { "good" })}) {
                                            (format!("{:.1}%", result.similarity_percent))
                                        }
                                        td {
                                            @if result.is_error {
                                                span.badge.dead { "⚠ Collision" }
                                            } @else {
                                                span.badge.running { "✓ Different" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                } @else {
                    p style="color: #888;" { "Waiting for comparison data..." }
                }

                h2 { "Stream Status" }

                @for (channel_name, streams) in channels {
                    @if channel_name != "silence" {
                        div.channel {
                            h2 { "Channel: " (channel_name) }

                        @for (stream_name, cmd_health, audio_health, uptime, volume) in streams {
                            div.stream {
                                div {
                                    div.stream-name { (stream_name) }
                                    @if let Some(uptime) = uptime {
                                        div style="color: #888; font-size: 0.85em; margin-top: 5px;" {
                                            "Uptime: " (format_duration(uptime))
                                        }
                                    }
                                    @if let Some(vol) = volume {
                                        div style="color: #888; font-size: 0.85em; margin-top: 3px;" {
                                            "Mean: " (format!("{:.1}", vol.mean_volume)) " dB | "
                                            "Max: " (format!("{:.1}", vol.max_volume)) " dB"
                                        }
                                    }
                                }
                                div.status {
                                    @match cmd_health {
                                        StreamHealth::Running => span.badge.running { "Running" },
                                        StreamHealth::Stalled => span.badge.stalled { "Stalled" },
                                        StreamHealth::Dead => span.badge.dead { "Dead" },
                                    }
                                    @match audio_health {
                                        AudioStreamHealth::Running => span.badge.running { "Audio OK" },
                                        AudioStreamHealth::NoData => span.badge.nodata { "Buffering" },
                                        AudioStreamHealth::Degraded => span.badge.degraded { "Degraded" },
                                        AudioStreamHealth::Dead => span.badge.dead { "Audio Dead" },
                                    }
                                }
                            }
                        }
                        }
                    }
                }
            }
        }
    }
}
