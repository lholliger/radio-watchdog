use std::net::TcpStream;
use std::process::{Child, ChildStdout, Command, Stdio};
use std::io::{self, BufReader, Read, Write, BufRead};
use std::thread;
use std::time::Duration;
use os_pipe::pipe;
use tracing::{info, trace, warn};

fn ensure_safe_ber(input: &str) -> (bool, Option<f32>) {
    let pieces: Vec<&str> = input.split(" ").collect();
    if pieces.len() < 5 {
        return (true, None);
    }
    if pieces.get(1) == Some(&"BER:") {
        let ber = pieces.get(2).unwrap();
        let ber_cleaned = ber.replace(",", "");
        let as_num = ber_cleaned.parse::<f32>().unwrap();
        if as_num > 0.05 { // TODO: figure out what an actual bad value is
            return (false, Some(as_num));
        }
    }
    return (true, None);
}

pub fn generate_rtl_sdr_input(frequency: &str, gain: f32) -> Result<Child, io::Error> {
    info!("Listening on frequency {} with gain {}", frequency, gain);
    let gain_str = gain.to_string();
    let command = ["-f", frequency, "-s", "1488375", "-g", gain_str.as_str(), "-"];
    trace!("rtl_tcp command: {:?}", command);
    let rtl_sdr_tcp_internal = Command::new("rtl_tcp")
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
        .args(command).spawn()?;


        let tcp_internal = rtl_sdr_tcp_internal.stderr.unwrap();
        let mut output_reader = BufReader::new(tcp_internal);
            
        thread::spawn(move || {
            loop {
                if let Some(line) = output_reader.by_ref().lines().next() {
                    if let Ok(line) = line {
                        trace!(line);
                    }
                }
            }
    });

    // wait until TCP socket opens
    let max_retries = 20;
    let mut retries = 0;
    loop {
        match TcpStream::connect("localhost:1234") {
            Ok(_) => break,
            Err(_) => {
                trace!("Failed to connect to rtl_tcp. Retrying ({} of {})...", retries, max_retries);
                if retries >= max_retries {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "Failed to connect to rtl_tcp after maximum retries"
                    ));
                }
                thread::sleep(Duration::from_millis(100));
                retries += 1;
            }
        }
    }

    Command::new("nc").args(["localhost", "1234"])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
}

pub fn get_hd_radio_streams(input_data: Child) -> Result<(ChildStdout, ChildStdout), io::Error> {
    // Get stdout handle from rtl_sdr to pipe to nrsc5 commands 
    let mut rtl_stdout = input_data.stdout.ok_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "Could not get rtl_sdr stdout")
    })?;

    // Create pipe for splitting rtl_sdr output
    let (reader1, mut  writer1) = pipe()?;
    let (reader2, mut writer2) = pipe()?;

    // Spawn thread to copy rtl_sdr output to both pipes
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        loop {
            match rtl_stdout.read(&mut buf) {
                Ok(0) => break, // EOF
                Ok(n) => {
                    writer1.write_all(&buf[..n]).unwrap();
                    writer2.write_all(&buf[..n]).unwrap();
                }
                Err(_) => break,
            }
        }
        warn!("rtl_sdr output ended unexpectedly");
        let stderr_reader = BufReader::new(input_data.stderr.unwrap());
        thread::spawn(move || {
            stderr_reader.lines().for_each(|line| {
                if let Ok(line) = line {
                    warn!(line);
                }
            })
        });
    });

    // Split rtl_sdr output to both nrsc5 commands using tee
    let nrsc5_hd1 = Command::new("nrsc5")
        .args(["0", "-r", "-", "-o", "-"]) 
        .stdin(Stdio::from(reader1))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let nrsc5_hd2 = Command::new("nrsc5")
        .args(["1", "-r", "-", "-o", "-"]) 
        .stdin(Stdio::from(reader2))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stderr_hd1 = nrsc5_hd1.stderr.unwrap();
    let stderr_hd2 = nrsc5_hd2.stderr.unwrap();
    let mut hd1_reader = BufReader::new(stderr_hd1);
    let mut hd2_reader = BufReader::new(stderr_hd2);
        
    thread::spawn(move || {
        loop {
            if let Some(line) = hd1_reader.by_ref().lines().next() {
                if let Ok(line) = line {
                    trace!(line);
                    let ber = ensure_safe_ber(&line);
                    if ber.0 == false && ber.1.is_some() {
                        warn!("BER for HD1 is in a bad state: {}", ber.1.unwrap());
                    }
                    if line.ends_with("Lost synchronization") {
                        warn!("HD1 AIR as lost synchronization!");
                    } else if line.ends_with("Synchronized") {
                        warn!("HD1 AIR Signal is synchronized");
                    }
                }
            }
    
            if let Some(line) = hd2_reader.by_ref().lines().next() {
                if let Ok(line) = line {
                    trace!(line);
                    let ber = ensure_safe_ber(&line);
                    if ber.0 == false && ber.1.is_some() {
                        warn!("BER for HD2 is in a bad state: {}", ber.1.unwrap());
                    }
                    if line.ends_with("Lost synchronization") {
                        warn!("WARNING: HD2 AIR has lost synchronization!");
                    } else if line.ends_with("Synchronized") {
                        warn!("HD2 AIR Signal is synchronized");
                    }
                }
            }
        }
    });

    Ok((nrsc5_hd1.stdout.unwrap(), nrsc5_hd2.stdout.unwrap()))
}