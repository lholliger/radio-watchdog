use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::net::TcpStream;
use tracing::{info, error, debug, warn};

pub struct SdrManager {
    host: String,
    port: u16,
    frequency: u32,
    size: u32,
    gain: f32,
    process: Arc<Mutex<Option<Child>>>,
}

impl SdrManager {
    pub fn new(host: String, port: u16, frequency: u32, size: u32, gain: f32) -> Self {
        Self {
            host,
            port,
            frequency,
            size,
            gain,
            process: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn spawn(&self) -> Result<(), String> {
        let mut process_lock = self.process.lock().await;

        if process_lock.is_some() {
            return Err("rtl_tcp process is already running".to_string());
        }

        // Check if the port is already in use
        let addr = format!("{}:{}", self.host, self.port);
        match TcpStream::connect(&addr).await {
            Ok(_) => {
                debug!("Port {}:{} is already in use, skipping rtl_tcp spawn", self.host, self.port);
                return Err(format!("Port {}:{} is already in use", self.host, self.port));
            }
            Err(_) => {
                debug!("Port {}:{} is available, proceeding with spawn", self.host, self.port);
            }
        }

        info!(
            "Spawning rtl_tcp on {}:{} with frequency={}, size={}, gain={}",
            self.host, self.port, self.frequency, self.size, self.gain
        );

        // Build the rtl_tcp command
        // rtl_tcp -a 0.0.0.0 -p <port> -f <frequency> -s <size> -g <gain>
        let mut cmd = Command::new("rtl_tcp");
        cmd.arg("-a").arg(&self.host)
            .arg("-p").arg(self.port.to_string())
            .arg("-f").arg(self.frequency.to_string())
            .arg("-s").arg(self.size.to_string())
            .arg("-g").arg(self.gain.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        debug!("Executing command: rtl_tcp -a {} -p {} -f {} -s {} -g {}",
            self.host, self.port, self.frequency, self.size, self.gain);

        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id();
                info!("Spawned rtl_tcp process (PID: {:?}), verifying it's accepting connections...", pid);

                // Store the child process
                *process_lock = Some(child);
                // Drop the lock before the async sleep/retry loop
                drop(process_lock);

                // Wait a bit and verify the port is accepting connections
                let addr = format!("{}:{}", self.host, self.port);
                let max_retries = 10;
                let mut connected = false;

                for attempt in 1..=max_retries {
                    tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;

                    match TcpStream::connect(&addr).await {
                        Ok(_) => {
                            info!("Successfully verified rtl_tcp is accepting connections on {}", addr);
                            connected = true;
                            break;
                        }
                        Err(_) => {
                            debug!("Connection attempt {}/{} failed, retrying...", attempt, max_retries);
                        }
                    }
                }

                if !connected {
                    // Check if the process is still running
                    let mut process_lock = self.process.lock().await;
                    if let Some(ref mut child) = *process_lock {
                        match child.try_wait() {
                            Ok(Some(status)) => {
                                error!("rtl_tcp process (PID: {:?}) exited with status: {}. Is an SDR connected?", pid, status);
                                *process_lock = None;
                                return Err(format!("rtl_tcp process exited (status: {})", status));
                            }
                            Ok(None) => {
                                // Process is still running but not accepting connections
                                error!("rtl_tcp process (PID: {:?}) is running but not accepting connections on {}", pid, addr);
                                child.kill().ok();
                                child.wait().ok();
                                *process_lock = None;
                                return Err(format!("rtl_tcp not accepting connections on {}", addr));
                            }
                            Err(e) => {
                                error!("Failed to check rtl_tcp process status: {}", e);
                                *process_lock = None;
                                return Err(format!("Failed to verify rtl_tcp status: {}", e));
                            }
                        }
                    } else {
                        return Err("rtl_tcp process disappeared".to_string());
                    }
                }

                Ok(())
            }
            Err(e) => {
                error!("Failed to spawn rtl_tcp: {}", e);
                Err(format!("Failed to spawn rtl_tcp: {}", e))
            }
        }
    }

    pub async fn stop(&self) -> Result<(), String> {
        let mut process_lock = self.process.lock().await;

        if let Some(mut child) = process_lock.take() {
            info!("Stopping rtl_tcp process (PID: {:?})", child.id());
            match child.kill() {
                Ok(_) => {
                    let _ = child.wait();
                    info!("Successfully stopped rtl_tcp process");
                    Ok(())
                }
                Err(e) => {
                    error!("Failed to kill rtl_tcp process: {}", e);
                    Err(format!("Failed to kill rtl_tcp process: {}", e))
                }
            }
        } else {
            Err("No rtl_tcp process is running".to_string())
        }
    }

    pub async fn is_running(&self) -> bool {
        self.process.lock().await.is_some()
    }
}

impl Drop for SdrManager {
    fn drop(&mut self) {
        // Attempt to kill the process if it's still running
        if let Ok(mut process_lock) = self.process.try_lock() {
            if let Some(mut child) = process_lock.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}
