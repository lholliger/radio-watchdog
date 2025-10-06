use std::{collections::HashMap, fs};

use clap::Parser;
use serde::Deserialize;
use std::sync::Arc;
use tracing::{debug, error, info, Level};
use utils::{audiorouter::AudioRouter, commandprocessor::CommandHolder, comparator::StreamComparator, slack::SlackMessageSender, webserver::WebServer, alertmanager::AlertManager};
mod utils;

#[derive(Parser, Debug)]
#[command(name = "watchdog")]
#[command(about = "Audio stream monitoring and comparison tool", long_about = None)]
struct Args {
    /// Path to the configuration file
    #[arg(short, long, default_value = "config.yaml")]
    config: String,

    /// Dry run mode - don't send Slack messages, print to terminal instead
    #[arg(long, default_value = "false")]
    dry_run: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct Config {
    slack_channel: String,
    slack_auth: String,
    silence: bool,
    sdrs: Option<HashMap<String, SDR>>,
    channels: HashMap<String, Channel>,
    #[serde(default = "default_buffer_duration")]
    buffer_duration: f32,
    #[serde(default = "default_comparison_duration")]
    comparison_duration: f32,
    #[serde(default = "default_min_buffer_duration")]
    min_buffer_duration: f32,
    #[serde(default = "default_match_threshold")]
    match_threshold: f32, // Percentage (0-100) for within-channel matching
    #[serde(default = "default_divergence_threshold")]
    divergence_threshold: f32, // Percentage (0-100) for cross-channel divergence
    #[serde(default = "default_web_port")]
    web_port: u16, // Port for web status server
}

fn default_buffer_duration() -> f32 { 120.0 }
fn default_comparison_duration() -> f32 { 5.0 }
fn default_min_buffer_duration() -> f32 { 30.0 }
fn default_match_threshold() -> f32 { 85.0 }
fn default_divergence_threshold() -> f32 { 50.0 }
fn default_web_port() -> u16 { 3000 }

#[derive(Debug, Clone, Deserialize)]
struct Channel {
    streams: HashMap<String, Stream>
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
enum StreamType {
    Web, // FFmpeg-compatible stream
    NRSC, // stream via nrsc, which needs an input from an RTL-SDR
    FM // TODO, however it is just an input from an RTL-SDR
}

#[derive(Debug, Clone, Deserialize)]
struct Stream {
    r#type: StreamType,
    host: String,
    path: String
}

#[derive(Debug, Clone, Deserialize)]
struct SDR {
    host: String, // could be local, or could be something we netcat in to
    port: u16
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    let subscriber_level = match std::env::var("LOGLEVEL").unwrap_or("INFO".to_string()).to_ascii_uppercase().as_str() {
        "TRACE" => Level::TRACE,
        "DEBUG" => Level::DEBUG,
        "INFO" => Level::INFO,
        "WARN" => Level::WARN,
        "ERROR" => Level::ERROR,
        _ => Level::INFO, // default if the environment variable is not set or invalid
    };

    tracing_subscriber::fmt().with_max_level(subscriber_level).init();

    info!("Loading configuration from: {}", args.config);

    let config_text = fs::read_to_string(&args.config);
    if config_text.is_err() {
        error!("Error reading config file: {}", args.config);
        return;
    }
    let config: Config = match serde_yaml::from_str(&config_text.expect("Could not decode YAML to string")) {
        Ok(config) => config,
        Err(e) => {
            error!("Error parsing config.yaml: {}", e);
            return;
        }
    };

    debug!("Using config: {:?}", config);

    // lets set up slack
    let slack = Arc::new(SlackMessageSender::new(config.slack_auth, config.slack_channel, args.dry_run));

    // Set up alert manager
    let alert_manager = Arc::new(AlertManager::new(slack.clone(), 10)); // 10 minute reminders
    alert_manager.clone().start_alert_loop().await;

    let mut router = AudioRouter::new();

    info!("Configuration: buffer_duration={}s, comparison_duration={}s, min_buffer_duration={}s",
          config.buffer_duration, config.comparison_duration, config.min_buffer_duration);
    info!("Thresholds: match_threshold={:.1}%, divergence_threshold={:.1}%",
          config.match_threshold, config.divergence_threshold);

    // Add silence detection channel if enabled
    if config.silence {
        info!("Silence detection enabled, adding silence reference channel");
        router.add_stream(
            &"silence".to_string(),
            &"silence".to_string(),
            config.buffer_duration,
            CommandHolder::new("ffmpeg", vec![
                "-loglevel", "error",
                "-re",
                "-f", "lavfi",
                "-i", "anullsrc=r=44100:cl=stereo",
                "-f", "s16le",
                "-"
            ], None)
        ).await;
    }

    // we need to do some sanity checks
    for channel in config.channels {
        for stream in channel.1.streams {
            match stream.1.r#type {
                StreamType::FM | StreamType::NRSC => {
                    match config.sdrs {
                        None => {
                            error!("Channel {} stream {} needs an SDR yet none are defined!", channel.0, stream.0);
                            return;
                        }
                        Some(ref sdrs) => match sdrs.get(&stream.1.host) {
                            None => {
                                error!("Channel {} stream {} needs an SDR yet {} is not defined!", channel.0, stream.0, stream.1.host);
                                return;
                            }
                            Some(sdr) => {
                                debug!("Channel {} stream {} is using SDR {} on host {}", channel.0, stream.0, stream.1.host, sdr.host);
                            }
                        }
                    }
                    // okay, now we can start creating the stream TODO
                },
                StreamType::Web => {
                    let stream_name = format!("{}-{}", channel.0, stream.0);
                    let url = format!("{}/{}", stream.1.host, stream.1.path);
                    debug!("Adding web stream {} for {}", stream_name, url);
                    router.add_stream(&stream_name, &channel.0, config.buffer_duration, CommandHolder::new("ffmpeg", vec![
                        "-loglevel", "error",
                        "-re",
                        "-i", &url,
                        "-ar", "44100",
                        "-ac", "2",
                        "-f", "s16le",
                        "-"
                    ], None)).await;
                }
            }
        }
    }

    // Convert router to Arc for sharing across tasks
    let router = Arc::new(router);

    // Start the supervisor to monitor stream health
    info!("Starting AudioRouter supervisor");
    router.start_supervisor().await;

    // Start the comparator to check stream similarity
    info!("Starting StreamComparator");
    let comparator = StreamComparator::new(
        router.clone(),
        config.comparison_duration,
        config.min_buffer_duration,
        config.match_threshold,
        config.divergence_threshold
    ).with_alert_manager(alert_manager.clone());
    comparator.start_comparison_loop().await;

    // Start the web server
    info!("Starting web server on port {}", config.web_port);
    let web_server = WebServer::new(router.clone(), comparator.get_results());
    tokio::spawn(async move {
        web_server.start(config.web_port).await;
    });

    // Keep the application running
    info!("Watchdog is now running. Press Ctrl+C to stop.");
    info!("Web interface available at http://localhost:{}", config.web_port);
    tokio::signal::ctrl_c().await.expect("Failed to listen for Ctrl+C");
    info!("Shutting down...");
}
