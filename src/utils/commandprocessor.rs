use std::{process::Stdio, sync::Arc, time::Duration};
use chrono::{DateTime, Utc};
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast::{self, Receiver, Sender};
use tokio::sync::Mutex;
use tokio::process::Command;
use tracing::{error, trace, warn, info};
use tokio::io::AsyncReadExt;

#[derive(Debug, Clone, PartialEq)]
pub enum StreamHealth {
    Running,
    Stalled,
    Dead
}

#[derive(Debug)]
pub struct CommandHolder {
    last_message: Arc<Mutex<DateTime<Utc>>>,
    health: Arc<Mutex<StreamHealth>>,
    command: String,
    args: Vec<String>,
    output: Sender<Vec<u8>>,
    input: Option<Receiver<Vec<u8>>>,
    restart_count: Arc<Mutex<u32>>,
    stall_timeout: Duration,
    start_time: DateTime<Utc>,
}

impl CommandHolder {
    pub fn new(command: &str, args: Vec<&str>, input: Option<Receiver<Vec<u8>>>) -> Self {
        let broadcast = broadcast::channel(1024);
        let mut cmd = CommandHolder {
            last_message: Arc::new(Mutex::new(Utc::now())),
            health: Arc::new(Mutex::new(StreamHealth::Running)),
            command: command.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            output: broadcast.0,
            input,
            restart_count: Arc::new(Mutex::new(0)),
            stall_timeout: Duration::from_secs(30),
            start_time: Utc::now(),
        };

        cmd.spawn();
        cmd.start_watchdog();

        cmd
    }

    pub fn get_reader(&self) -> broadcast::Receiver<Vec<u8>> {
        return self.output.subscribe();
    }

    pub async fn get_health(&self) -> StreamHealth {
        self.health.lock().await.clone()
    }

    pub async fn get_restart_count(&self) -> u32 {
        *self.restart_count.lock().await
    }

    pub fn get_uptime(&self) -> chrono::Duration {
        Utc::now().signed_duration_since(self.start_time)
    }

    fn spawn(&mut self) { 
        let mut body = Command::new(self.command.clone())
            .args(self.args.as_slice())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()).spawn().expect("Could not spawn command");

            if let Some(mut stdin) = body.stdin.take() {
                trace!("Applying input to stdin if exists");
                if let Some(mut input) = self.input.take() {
                    tokio::spawn(async move {
                        trace!("Starting stdin from input loop");
                        loop {
                            let data = input.recv().await;
                            if let Ok(bytes ) = data {
                                match stdin.write(&bytes).await {
                                    Ok(_) => (),
                                    Err(e) => {
                                        error!("Could not write data: {:?}", e);
                                    }
                                }
                            }
                        }
                    });
                }
            }

            if let Some(mut stdout) = body.stdout.take() {
                let tx = self.output.clone();
                let last_msg = self.last_message.clone();
                let health = self.health.clone();
                tokio::spawn(async move {
                    let mut buffer = [0u8; 176400]; // Match old implementation buffer size
                    loop {
                        match stdout.read(&mut buffer).await {
                            Ok(n) if n == 0 => {
                                warn!("Process stdout closed (EOF)");
                                *health.lock().await = StreamHealth::Dead;
                                break;
                            },
                            Ok(n) => {
                                *last_msg.lock().await = Utc::now();
                                *health.lock().await = StreamHealth::Running;
                                let _ = tx.send(buffer[..n].to_vec());
                            }
                            Err(e) => {
                                error!("Error reading stdout: {:?}", e);
                                *health.lock().await = StreamHealth::Dead;
                                break;
                            }
                        }
                    }
                });
            }

            // Capture stderr for debugging
            if let Some(mut stderr) = body.stderr.take() {
                let cmd_name = self.command.clone();
                tokio::spawn(async move {
                    let mut buffer = [0u8; 1024];
                    loop {
                        match stderr.read(&mut buffer).await {
                            Ok(n) if n == 0 => break,
                            Ok(n) => {
                                let stderr_str = String::from_utf8_lossy(&buffer[..n]);
                                trace!("[{} stderr] {}", cmd_name, stderr_str.trim());
                            }
                            Err(_) => break,
                        }
                    }
                });
            }
    }

    fn start_watchdog(&self) {
        let last_msg = self.last_message.clone();
        let health = self.health.clone();
        let timeout = self.stall_timeout;
        let restart_count = self.restart_count.clone();
        let command = self.command.clone();

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;

                let current_health = health.lock().await.clone();
                let last = *last_msg.lock().await;
                let elapsed = Utc::now().signed_duration_since(last);

                match current_health {
                    StreamHealth::Running => {
                        if elapsed.num_seconds() > timeout.as_secs() as i64 {
                            warn!("Stream {} stalled (no data for {}s)", command, elapsed.num_seconds());
                            *health.lock().await = StreamHealth::Stalled;
                        } else if *restart_count.lock().await != 0 {
                            info!("Stream {} recovered, resetting restart count", command);
                            *restart_count.lock().await = 0;
                        }
                    },
                    StreamHealth::Stalled => {
                        // Continue monitoring, may recover
                        if elapsed.num_seconds() <= timeout.as_secs() as i64 {
                            info!("Stream {} recovered from stall", command);
                            *health.lock().await = StreamHealth::Running;
                        }
                    },
                    StreamHealth::Dead => {
                        let count = *restart_count.lock().await;
                        *restart_count.lock().await += 1;
                        warn!("Stream {} died, attempting restart {}", command, count + 1);
                        // Note: actual respawn needs to be handled by supervisor
                        // since we can't call spawn() from here (no &mut self)
                    }
                }
            }
        });
    }

    pub async fn respawn(&mut self) -> bool {
        let count = self.get_restart_count().await;
        tokio::time::sleep(Duration::from_secs((30 * count).into())).await;
        info!("Respawning command: {} {}", self.command, self.args.join(" "));
        *self.last_message.lock().await = Utc::now();
        *self.health.lock().await = StreamHealth::Running;
        self.spawn();
        true
    }
}