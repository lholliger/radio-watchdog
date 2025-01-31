use std::{process::Child, sync::{Arc, Mutex}, thread};
use std::io::Read;
use chrono::{DateTime, Utc};
use rusty_chromaprint::{match_fingerprints, Configuration, Fingerprinter};
use tracing::{debug, error, trace, warn};

#[derive(Debug)]
pub struct Commander {
    streams: Vec<Vec<Child>>, // each vec should be identical, then all others different
    labels: Vec<(String, Vec<String>)>,
    audio_data: Vec<Vec<Arc<Mutex<Vec<u32>>>>>,
    last_packet: Vec<Vec<Arc<Mutex<DateTime<Utc>>>>>,
    recorded_values: usize
}


impl Commander {
    pub fn new(streams: Vec<Vec<Child>>, labels: Vec<(String, Vec<String>)>, duration: f32) -> Commander {
        let c = Commander {
            streams,
            labels,
            audio_data: vec![],
            last_packet: vec![],
            recorded_values: (duration / Configuration::preset_test1().item_duration_in_seconds()) as usize, // take seconds needed, divide by samples
        };
        c
    }

    pub fn duration(&self) -> f32 {
        self.recorded_values as f32 * Configuration::preset_test1().item_duration_in_seconds()
    }
    
    pub fn begin_listening(&mut self) {
        let mut channel_num = 0;
        let record_size = self.recorded_values.clone();
        while !self.streams.is_empty() {
            let stream = self.streams.remove(0);
            let mut fingerprints = vec![];
            let mut last_packets = vec![];
            let mut stream_num = 0;
            for child in stream {
                let fingerprint_content: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
                let last_packet: Arc<Mutex<DateTime<Utc>>> = Arc::new(Mutex::new(Utc::now()));
                let cloned_for_thread = fingerprint_content.clone();
                let packet_cloned_for_thread = last_packet.clone();
                let channel_name = self.labels.get(channel_num).unwrap().0.clone();
                let stream_name = self.labels.get(channel_num).unwrap().1.get(stream_num).unwrap().clone();
                thread::spawn(move || {
                    let mut stdout = child.stdout.unwrap();
                    let mut fingerprinter = Fingerprinter::new(&Configuration::preset_test1());
                    fingerprinter.start(44100, 2).unwrap();
                    let mut read_buf = [0; 176400];
                    loop {
                        if let Ok(bytes_read) = stdout.read(&mut read_buf) {
                            //trace!("[{}-{}] Read {} bytes from stdout", channel_name, stream_name, bytes_read);
                            if bytes_read == 0 { break; }
                            let samples = unsafe {
                                std::slice::from_raw_parts(
                                    read_buf.as_ptr() as *const i16,
                                    bytes_read / 2
                                )
                            };
                            fingerprinter.consume(samples);
                            let mut fingerprint_content = cloned_for_thread.lock().unwrap();
                            fingerprint_content.clear();
                            fingerprint_content.extend(fingerprinter.fingerprint().to_vec());
                            if fingerprint_content.len() > record_size{ // we also need to note in other systems since the running total expects this to always increase
                                let start = fingerprint_content.len() - record_size;
                                *fingerprint_content = fingerprint_content.split_off(start);
                            }
                            let mut last_timestamp = packet_cloned_for_thread.lock().unwrap();
                            *last_timestamp = Utc::now();
                        }
                    }
                    error!("Thread died: {}-{}", channel_name, stream_name);
                    // TODO: a slack message should be sent here
                });
                debug!("Spawned thread!");
                fingerprints.push(fingerprint_content);
                last_packets.push(last_packet);
                stream_num += 1;
            }
            self.audio_data.push(fingerprints);
            self.last_packet.push(last_packets);
            channel_num += 1;
        }
    }

    fn get_similarity_time_between_fingerprints(&self, fingerprint1: Vec<u32> , fingerprint2: Vec<u32>) -> Option<f32> {
        if let (Some(_), Some(_)) = (fingerprint1.first(), fingerprint2.first()) {
            if fingerprint1.len() < self.recorded_values || fingerprint2.len() < self.recorded_values {
                trace!("{} and {} fingerprint is too short, need {}", fingerprint1.len(), fingerprint2.len(), self.recorded_values);
                return None;  // we must have a full fingerprint to compare
            }
            let similarity = match_fingerprints(
                &fingerprint1,
                &fingerprint2,
                &Configuration::preset_test1()
            ).unwrap();
            //info!("{}) Similarity between stream {} and {}: {:?}", thread, i, j, similarity);
            let mut similar_time = 0.0;
            for similar in similarity {
                similar_time += similar.duration(&Configuration::preset_test1());
            }
            Some(similar_time)
        } else {
            None
        }
    }

    pub fn check_thread_similarity(&self, thread: usize) -> Vec<Option<f32>> {
        let thread_audio = self.audio_data.get(thread).expect("Could not select thread!");
        //let thread_name = &self.labels.get(thread).unwrap().0;
        let mut outputs = vec![];
        for i in 0..thread_audio.len() {
            for j in (i + 1)..thread_audio.len() {
                let fingerprint1 = thread_audio[i].lock().unwrap();
                let fingerprint2 = thread_audio[j].lock().unwrap();

                //let fp1_name = self.labels.get(thread).unwrap().1.get(i).unwrap();
                //let fp2_name = self.labels.get(thread).unwrap().1.get(j).unwrap();

                match self.get_similarity_time_between_fingerprints(
                    fingerprint1.to_vec(),
                    fingerprint2.to_vec()) {
                        Some(similarity) => {
                            outputs.push(Some(similarity));
                            debug!("{}) Similarity time between stream {} and {}: {:?}", thread, i, j, similarity);
                        },
                        None => {
                            outputs.push(None);
                            debug!("{}) Could not compare fingerprints {} and {}", thread, i, j);
                        }
                    }
            }
        }
        return outputs;
    }

    pub fn check_thread_similarity_labels(&self, thread: usize) -> Vec<(String, String)> {
        let thread_audio = self.audio_data.get(thread).expect("Could not select thread!");
        let mut outputs = vec![];
        for i in 0..thread_audio.len() {
            for j in (i + 1)..thread_audio.len() {
                let fp1_name = self.labels.get(thread).unwrap().1.get(i).unwrap();
                let fp2_name = self.labels.get(thread).unwrap().1.get(j).unwrap();
                outputs.push((fp1_name.clone(), fp2_name.clone()));
            }
        }
        outputs
    }

    pub fn ensure_different_across_channels(&self, thread1: usize, thread2: usize) -> Vec<Option<f32>> {
        let one_thread = self.audio_data.get(thread1).unwrap();
        let two_thread = self.audio_data.get(thread2).unwrap();
        if one_thread.len() != two_thread.len() {
            warn!("{} and {} have different number of channels. Not continuing", thread1, thread2);
            return vec![]
        }
        let mut outputs = vec![];
        for i in 0..one_thread.len() {
            let fingerprint1 = one_thread[i].lock().unwrap();
            let fingerprint2 = two_thread[i].lock().unwrap();

            match self.get_similarity_time_between_fingerprints(
                fingerprint1.to_vec(),
                fingerprint2.to_vec()) {
                    Some(similarity) => {
                        outputs.push(Some(similarity));
                        debug!("Similarity time between stream {}-{} and {}-{}: {:?}", thread1, i, thread2, i, similarity);
                    },
                    None => {
                        outputs.push(None);
                        debug!("Similarity time between stream {}-{} and {}-{} could not be found yet", thread1, i, thread2, i);
                        
                    }
                }
        }
        return outputs;
    }

    pub fn ensure_different_across_channels_labels(&self, thread1: usize, thread2: usize) -> Vec<((String, String), (String, String))> {
        let one_thread = self.audio_data.get(thread1).unwrap();
        let two_thread = self.audio_data.get(thread2).unwrap();
        if one_thread.len() != two_thread.len() {
            warn!("{} and {} have different number of channels. Not continuing", thread1, thread2);
            return vec![]
        }
        let one_thread_name = self.labels.get(thread1).unwrap().0.clone();
        let two_thread_name = self.labels.get(thread2).unwrap().0.clone();
        let mut outputs = vec![];
        for i in 0..one_thread.len() {
            let fp1_name = self.labels.get(thread1).unwrap().1.get(i).unwrap();
            let fp2_name = self.labels.get(thread2).unwrap().1.get(i).unwrap();
            outputs.push(((one_thread_name.clone(), fp1_name.clone()), (two_thread_name.clone(), fp2_name.clone())));
        }
        return outputs;
    }

    pub fn get_channel_durations(&self, thread: usize) -> Vec<Option<f32>> {
        let thread_audio = self.audio_data.get(thread).expect("Could not select thread!");
        let thread_name = &self.labels.get(thread).unwrap().0;
        let mut outputs = vec![];
        for i in 0..thread_audio.len() {
                let fingerprint = thread_audio[i].lock().unwrap();
                let fp1_name = self.labels.get(thread).unwrap().1.get(i).unwrap();

                match self.get_similarity_time_between_fingerprints(
                    fingerprint.to_vec(),
                    fingerprint.to_vec()) {
                        Some(similarity) => {
                            outputs.push(Some(similarity));
                            debug!("{}) Duration of {}: {:?}", thread_name, fp1_name, similarity);
                        },
                        None => {
                            outputs.push(None);
                            debug!("{}) Duration of {} could not be determined", thread_name, fp1_name);
                        }
                    }
            }
        return outputs;
    }

    // honestly we may need to just have a more direct "Compare channels" option since we need to also do duration to get percentages, etc
    pub fn get_all_channel_similarities(&self) -> Vec<((usize, usize), Vec<Option<f32>>)> {
        let mut output = vec![];
        for i in 0..self.audio_data.len() {
            for j in (i + 1)..self.audio_data.len() {
                let diffs = self.ensure_different_across_channels(i, j);
                debug!("{} and {} have differences: {:?}", i, j, diffs);
                output.push(((i, j), diffs));
            }
        }
        return output;
    }

    pub fn get_all_channel_syncs(&self) -> Vec<(usize, Vec<Option<f32>>)> {
        let mut output = vec![];
        for i in 0..self.audio_data.len() {
            let diffs = self.check_thread_similarity(i);
            debug!("{} has internal sync: {:?}", i, diffs);
            output.push((i, diffs));
        }
        return output;
    }

    pub fn get_all_channel_talks(&self) -> Vec<((usize, usize), DateTime<Utc>)> {
        let channel_length = self.last_packet.get(0).unwrap().len();
        let mut output = vec![];
        for i in 0..self.last_packet.len() {
            for j in 0..channel_length {
                let talk_ex = self.last_packet[i][j].lock()
                .expect("Could not lock channel time")
                .clone();
                output.push(((i, j), talk_ex));
            }
        }
        output
    }

    pub fn get_all_channel_talks_labels(&self) -> Vec<(String, String)> {
        let channel_length = self.last_packet.get(0).unwrap().len();
        let mut output = vec![];
        for i in 0..self.last_packet.len() {
            let thread_name = self.labels.get(i).unwrap().0.clone();
            for j in 0..channel_length {
                let chan_name = self.labels.get(i).unwrap().1.get(j).unwrap();
                output.push((thread_name.clone(), chan_name.clone()));
            }
        }
        output
    }

    pub fn get_channel_label(&self, channel: usize) -> String { // TODO: make this an option
        self.labels.get(channel).expect("Channel is out of range").0.clone()
    }
}