use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::broadcast::{self, Receiver, Sender};
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info, trace, warn};

/// Represents an RTL-SDR device connection via rtl_tcp
pub struct RtlTcpConnection {
    host: String,
    port: u16,
    stream: Option<TcpStream>,
}

impl RtlTcpConnection {
    pub fn new(host: String, port: u16) -> Self {
        RtlTcpConnection {
            host,
            port,
            stream: None,
        }
    }

    /// Connect to the rtl_tcp server
    pub async fn connect(&mut self) -> Result<(), std::io::Error> {
        let address = format!("{}:{}", self.host, self.port);
        info!("Connecting to rtl_tcp at {}", address);

        let mut retries = 0;
        let max_retries = 20;

        loop {
            match TcpStream::connect(&address).await {
                Ok(stream) => {
                    info!("Successfully connected to rtl_tcp at {}", address);
                    self.stream = Some(stream);
                    return Ok(());
                }
                Err(e) => {
                    if retries >= max_retries {
                        error!("Failed to connect to rtl_tcp at {} after {} retries", address, max_retries);
                        return Err(e);
                    }
                    trace!("Failed to connect to rtl_tcp at {} (attempt {}/{}): {}", address, retries + 1, max_retries, e);
                    retries += 1;
                    sleep(Duration::from_millis(500)).await;
                }
            }
        }
    }

    /// Read data from rtl_tcp and broadcast to multiple nrsc5 processes
    pub async fn start_reading(&mut self, broadcaster: Sender<Vec<u8>>) -> Result<(), std::io::Error> {
        if self.stream.is_none() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "Not connected to rtl_tcp",
            ));
        }

        let mut stream = self.stream.take().unwrap();
        info!("Starting to read from rtl_tcp and broadcast to nrsc5 processes");

        tokio::spawn(async move {
            let mut buffer = [0u8; 16384]; // 16KB buffer for IQ samples
            loop {
                match stream.read(&mut buffer).await {
                    Ok(0) => {
                        warn!("rtl_tcp connection closed (EOF)");
                        break;
                    }
                    Ok(n) => {
                        trace!("Read {} bytes from rtl_tcp", n);
                        // Broadcast to all subscribers (nrsc5 processes)
                        if broadcaster.send(buffer[..n].to_vec()).is_err() {
                            warn!("No active nrsc5 receivers");
                        }
                    }
                    Err(e) => {
                        error!("Error reading from rtl_tcp: {}", e);
                        break;
                    }
                }
            }
        });

        Ok(())
    }
}

/// Represents an nrsc5 process that decodes HD Radio
pub struct Nrsc5Process {
    program_number: String,
    child: Option<Child>,
    output_sender: Sender<Vec<u8>>,
}

impl Nrsc5Process {
    /// Create a new nrsc5 process
    pub fn new(program_number: &str) -> Self {
        let (tx, _) = broadcast::channel(1024);
        Nrsc5Process {
            program_number: program_number.to_string(),
            child: None,
            output_sender: tx,
        }
    }

    /// Spawn the nrsc5 process with input from rtl_tcp
    pub async fn spawn(&mut self, mut input: Receiver<Vec<u8>>) -> Result<(), std::io::Error> {
        info!("Spawning nrsc5 process for program {}", self.program_number);

        let mut child = Command::new("nrsc5")
            .args([
                &self.program_number,
                "-r", "-", // Read from stdin
                "-o", "-", // Output to stdout
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        // Handle stdin - write data from rtl_tcp
        if let Some(mut stdin) = child.stdin.take() {
            let program = self.program_number.clone();
            tokio::spawn(async move {
                trace!("Starting stdin writer for nrsc5 program {}", program);
                loop {
                    match input.recv().await {
                        Ok(data) => {
                            if let Err(e) = stdin.write_all(&data).await {
                                error!("Failed to write to nrsc5 program {} stdin: {}", program, e);
                                break;
                            }
                        }
                        Err(e) => {
                            error!("Failed to receive data for nrsc5 program {}: {}", program, e);
                            break;
                        }
                    }
                }
            });
        }

        // Handle stdout - broadcast audio output
        if let Some(mut stdout) = child.stdout.take() {
            let tx = self.output_sender.clone();
            let program = self.program_number.clone();
            tokio::spawn(async move {
                let mut buffer = [0u8; 8192];
                loop {
                    match stdout.read(&mut buffer).await {
                        Ok(0) => {
                            warn!("nrsc5 program {} stdout closed", program);
                            break;
                        }
                        Ok(n) => {
                            trace!("Read {} bytes from nrsc5 program {}", n, program);
                            let _ = tx.send(buffer[..n].to_vec());
                        }
                        Err(e) => {
                            error!("Error reading from nrsc5 program {} stdout: {}", program, e);
                            break;
                        }
                    }
                }
            });
        }

        // Handle stderr - log messages
        if let Some(mut stderr) = child.stderr.take() {
            let program = self.program_number.clone();
            tokio::spawn(async move {
                let mut buffer = [0u8; 1024];
                loop {
                    match stderr.read(&mut buffer).await {
                        Ok(0) => break,
                        Ok(n) => {
                            let stderr_str = String::from_utf8_lossy(&buffer[..n]);
                            for line in stderr_str.lines() {
                                // Check for important status messages
                                if line.contains("Lost synchronization") {
                                    warn!("nrsc5 program {} lost synchronization", program);
                                } else if line.contains("Synchronized") {
                                    info!("nrsc5 program {} synchronized", program);
                                } else if line.contains("BER:") {
                                    // Log bit error rate
                                    trace!("nrsc5 program {}: {}", program, line);
                                } else {
                                    trace!("nrsc5 program {} stderr: {}", program, line);
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        self.child = Some(child);
        Ok(())
    }

    /// Get a receiver for the audio output
    pub fn get_output_receiver(&self) -> Receiver<Vec<u8>> {
        self.output_sender.subscribe()
    }
}

/// Manages an SDR with multiple NRSC5 decoders
pub struct NrscManager {
    rtl_tcp: Arc<Mutex<RtlTcpConnection>>,
    nrsc5_processes: Arc<Mutex<HashMap<String, Nrsc5Process>>>,
    rtl_broadcaster: Sender<Vec<u8>>,
}

impl NrscManager {
    /// Create a new NRSC manager
    pub fn new(host: String, port: u16) -> Self {
        let (tx, _) = broadcast::channel(1024);
        NrscManager {
            rtl_tcp: Arc::new(Mutex::new(RtlTcpConnection::new(host, port))),
            nrsc5_processes: Arc::new(Mutex::new(HashMap::new())),
            rtl_broadcaster: tx,
        }
    }

    /// Initialize the connection and start reading from rtl_tcp
    pub async fn start(&self) -> Result<(), std::io::Error> {
        let mut rtl = self.rtl_tcp.lock().await;
        rtl.connect().await?;
        rtl.start_reading(self.rtl_broadcaster.clone()).await?;
        Ok(())
    }

    /// Add an nrsc5 decoder for a specific program number
    pub async fn add_program(&self, program_number: &str) -> Result<Receiver<Vec<u8>>, std::io::Error> {
        let mut processes = self.nrsc5_processes.lock().await;

        // Check if program already exists
        if let Some(existing) = processes.get(program_number) {
            debug!("Program {} already exists, returning new receiver", program_number);
            return Ok(existing.get_output_receiver());
        }

        // Create new nrsc5 process
        let mut nrsc5 = Nrsc5Process::new(program_number);
        let input_receiver = self.rtl_broadcaster.subscribe();
        nrsc5.spawn(input_receiver).await?;

        let output_receiver = nrsc5.get_output_receiver();
        processes.insert(program_number.to_string(), nrsc5);

        info!("Added nrsc5 decoder for program {}", program_number);
        Ok(output_receiver)
    }
}
