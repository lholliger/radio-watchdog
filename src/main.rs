use std::collections::HashMap;
use std::time::Duration;
use std::{env, io, thread};
use std::process::{Command, Stdio};
use chrono::Utc;
use dotenv::dotenv;
use tracing::{info, trace, warn, Level};
use utils::commander::Commander;
use utils::event::{NominalValue, AlertState};
use utils::nrsc::{generate_rtl_sdr_input, get_hd_radio_streams};
use utils::runningtotal::RunningTotal;
use utils::slack::SlackMessageSender;
mod utils;

fn main() -> io::Result<()> {
    dotenv().ok();

    let subscriber_level = match std::env::var("LOG_LEVEL").unwrap_or_default().as_str() {
        "TRACE" => Level::TRACE,
        "DEBUG" => Level::DEBUG,
        "INFO" => Level::INFO,
        "WARN" => Level::WARN,
        "ERROR" => Level::ERROR,
        _ => Level::INFO, // default if the environment variable is not set or invalid
    };

    tracing_subscriber::fmt().with_max_level(subscriber_level).init();

    for (key, value) in std::env::vars() {
        trace!("ENV {key}: {value}");
    }

    let sender = SlackMessageSender::new(
        env::var("SLACK_AUTH").expect("Could not find ENV SLACK_AUTH (Your Slack Authorization/oAuth)"),
        env::var("SLACK_ID").expect("Could not find ENV SLACK_ID (Slack channel ID)"),
        env::var("DRY_RUN").unwrap_or("false".to_string()).parse().unwrap_or(false)
    );

    // sender is all set up, we should start getting streams together
    // this weird part cant really be put into a function

    info!("Generating 2 nrsc5 streams");

    let input_stream = match env::var("TCP_HOST") {
        Ok(input) => {
            info!("TCP input provided, not generating own stream!");
            let port = env::var("TCP_PORT").unwrap_or("1234".to_string());
            Command::new("nc").args([input, port])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
        }
        Err(_) => {
            generate_rtl_sdr_input("91100000", env::var("SDR_GAIN").unwrap_or("5.0".to_string()).parse::<f32>().unwrap_or(5.0))
        }
    }.expect("Could not connect to the RTL-SDR TCP input stream!");

    let radio_streams = get_hd_radio_streams(input_stream).expect("Could not get radio streams, is nrsc5 installed and the SDR working?");
    // create the list of HD1 and HD2 channels that must be identical
    let hd1_streams = vec![
        Command::new("ffmpeg") 
        .args([
            "-re",
            "-i", "pipe:0",
            "-ar", "44100", "-ac", "2",
            "-f", "s16le",
            "-"
        ])
        .stdin(radio_streams.0)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?,
        Command::new("ffmpeg")
        .args([
            "-re",
            "-i", "http://hls.wrek.org/main/live.m3u8",
            "-ar", "44100", "-ac", "2", 
            "-f", "s16le",
            "-"
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?,
        Command::new("ffmpeg")
        .args([
            "-re",
            "-i", "https://streaming.wrek.org/main/128kb.mp3",
            "-ar", "44100", "-ac", "2", 
            "-f", "s16le",
            "-"
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?
    ];

    let hd2_streams = vec![
        Command::new("ffmpeg") 
        .args([
            "-re",
            "-i", "pipe:0",
            "-ar", "44100", "-ac", "2",
            "-f", "s16le",
            "-"
        ])
        .stdin(radio_streams.1)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?,
        Command::new("ffmpeg")
        .args([
            "-re",
            "-i", "http://hls.wrek.org/hd2/live.m3u8",
            "-ar", "44100", "-ac", "2", 
            "-f", "s16le",
            "-"
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?,
        Command::new("ffmpeg")
        .args([
            "-re",
            "-i", "https://streaming.wrek.org/hd2/128kb.mp3",
            "-ar", "44100", "-ac", "2", 
            "-f", "s16le",
            "-"
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?
    ];

    let silence = vec![
        Command::new("ffmpeg")
        .args([
            "-re",
            "-f", "lavfi",
            "-i", "anullsrc=r=44100:cl=stereo",
            "-f", "s16le",
            "-"
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?,
        Command::new("ffmpeg")
        .args([
            "-re",
            "-f", "lavfi",
            "-i", "anullsrc=r=44100:cl=stereo",
            "-f", "s16le",
            "-"
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?,
        Command::new("ffmpeg")
        .args([
            "-re",
            "-f", "lavfi",
            "-i", "anullsrc=r=44100:cl=stereo",
            "-f", "s16le",
            "-"
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?
    ];

    let window_size = 60.0;

    info!("Starting commander");
    let mut commander = Commander::new(vec![hd1_streams, hd2_streams, silence],
    
    vec![
        ("HD1".to_string(),
            vec!["Tower".to_string(), "HLS".to_string(), "MP3".to_string()]),
        ("HD2".to_string(),
            vec!["Tower".to_string(), "HLS".to_string(), "MP3".to_string()]),
        ("Silence".to_string(),
            vec!["Silence".to_string(), "Silence".to_string(), "Silence".to_string()])
    ], window_size);

    commander.begin_listening();

    info!("Commander is running");
    let segment_time = 10.0; // seconds, this may need some updates as the segment is kinda a rolling window
    let bins = (window_size / segment_time) as usize;
    let overlap_level = 70.0; // maximum allowed overlap average
    let desync_level = 20.0; // minimum desync level
    let non_respond_level = 30.0; // max number of seconds a channel can go without responding before it's considered dead
    
    let mut cross_channel_similarity: HashMap<(usize, usize), RunningTotal> = HashMap::new();
    let mut internal_channel_similarity: HashMap<usize, RunningTotal> = HashMap::new();
    let mut last_talk: HashMap<(usize, usize), NominalValue> = HashMap::new();

    let mut events: HashMap<String, NominalValue> = HashMap::new();

    loop {
        thread::sleep(Duration::from_secs(segment_time as u64)); // this time should be above the time of an HLS segment

        // this is for identifying which channels are similar
        let channel_sims = commander.get_all_channel_similarities();
        for channel in channel_sims {
            match cross_channel_similarity.get_mut(&channel.0) {
                Some(rt) => {
                    rt.add_values_checked(&channel.1);
                },
                None => {
                    cross_channel_similarity.insert(channel.0, RunningTotal::new(channel.1, bins, window_size));
                }
            }
        }


        // this is for identifying which streams in a channel are similar
        let channel_syncs = commander.get_all_channel_syncs();
        for channel in channel_syncs {
            match internal_channel_similarity.get_mut(&channel.0) {
                Some(rt) => {
                    rt.add_values_checked(&channel.1);
                },
                None => {
                    internal_channel_similarity.insert(channel.0, RunningTotal::new(channel.1, bins, window_size));
                }
            }
        }

        // we need to watch when a thread dies
        let last_talks = commander.get_all_channel_talks();
        let last_talks_labels = commander.get_all_channel_talks_labels();
        for channel in 0..last_talks.len() {
            let talk = last_talks[channel];
            let talk_label = last_talks_labels[channel].clone();
            let formatted_label = format!("{}-{} LAST TALK", talk_label.0, talk_label.1);
            let time_delta = (Utc::now()-talk.1).num_milliseconds() as f32 / 1000.0;
            match events.get_mut(&formatted_label) {
                Some(event) => {
                    event.update(time_delta);
                },
                None => {
                    events.insert(formatted_label.clone(), NominalValue::new(format!("{}/{} last audio data", talk_label.0, talk_label.1), (f32::NEG_INFINITY, non_respond_level)));
                }
            }
        }


        // now we need to alert which channels are overlapping
        // technically now we are averaging the average, so technically each avg is of previous 60 sec and then that is all averaged...
        // this allows for us to send less false alerts, but could take 2 minutes to send an alert theoretically
        for channel in cross_channel_similarity.iter() {
            let avg = channel.1.get_average();
            if avg.is_none() {
                continue;
            }
            let avg_val = avg.unwrap();
            let names = commander.ensure_different_across_channels_labels(channel.0.0, channel.0.1);
            for channel_check in 0..names.len() {
                let name = names[channel_check].clone();
                let formatted_internal = format!("{}-{} COMPARE {}-{}", name.0.0, name.0.1, name.1.0, name.1.1);
                let level = avg_val[channel_check].clone();
                match events.get_mut(&formatted_internal) {
                    Some(event) => {
                        event.update(level);
                    }
                    None => {
                        events.insert(formatted_internal, NominalValue::new(format!("{}/{} overlap with {}/{}", name.0.0, name.0.1, name.1.0, name.1.1), (f32::NEG_INFINITY, overlap_level)));
                    }
                }
            }
        }

        for channel in internal_channel_similarity.iter() {
            let avg = channel.1.get_average();
            if avg.is_none() {
                continue;
            }
            let avg_val = avg.unwrap();
            let names = commander.check_thread_similarity_labels(*channel.0);
            let label = commander.get_channel_label(*channel.0);
            for test in 0..names.len() {
                let name = names[test].clone();
                let level = avg_val[test].clone();
                let formatted_internal = format!("{} INTERNAL {} COMPARE {}", label, name.0, name.1);
                match events.get_mut(&formatted_internal) {
                    Some(event) => {
                        event.update(level);
                    }
                    None => {
                        events.insert(formatted_internal, NominalValue::new(format!("[{}] {} synchronization with {}", label, name.0, name.1), (desync_level, f32::INFINITY)));
                    }
                }
            }
        }

        for event in events.values_mut() {
            match event.alert_needed() {
                AlertState::NewFailing => {
                    warn!("New alert needed: {}", event.get_alert_string());
                    sender.send(format!("*Warning:* _A new value is out of the expected range!_ {}", event.get_alert_string()));
                    event.register_sent_alert();
                },
                AlertState::FailingReminderNeeded => {
                    warn!("Alert reminder needed: {}", event.get_alert_string());
                    sender.send(format!("*Reminder:* _A value is still of the expected range!_ {}", event.get_alert_string()));
                    event.register_sent_alert();
                },
                AlertState::NewPassing => {
                    warn!("Alert cleared: {}", event.get_passing_string());
                    sender.send(format!("*Success:* *Error fixed!* {}", event.get_passing_string()));
                    event.register_sent_alert();
                },
                AlertState::FailingAlertSent | AlertState::Passing => {
                    // nothing to do
                }
            }
        }
    }
}