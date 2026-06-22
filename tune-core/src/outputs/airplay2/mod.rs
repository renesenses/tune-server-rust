//! AirPlay 2 output — wraps `airplay-daemon` as a subprocess (GPL isolation).
//!
//! Uses the same subprocess pattern as librespot for Spotify Connect.
//! The daemon binary reads JSON commands on stdin and emits JSON events on stdout.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::outputs::traits::{OutputStatus, OutputTarget, PlayMedia, TransportState};

const DAEMON_BINARY: &str = "airplay-daemon";

pub struct Airplay2Output {
    name: String,
    device_id: String,
    host: String,
    port: u16,
    ap_device_id: String,
    playing: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    position_ms: Arc<AtomicU64>,
    duration_ms: Arc<AtomicU64>,
    volume: Arc<Mutex<f64>>,
    current_title: Arc<Mutex<Option<String>>>,
    current_artist: Arc<Mutex<Option<String>>>,
    process: Arc<Mutex<Option<DaemonProcess>>>,
}

struct DaemonProcess {
    child: Child,
    stdin: tokio::process::ChildStdin,
}

impl DaemonProcess {
    async fn send_cmd(&mut self, cmd: &serde_json::Value) -> Result<(), String> {
        let json = serde_json::to_string(cmd).map_err(|e| e.to_string())?;
        self.stdin
            .write_all(format!("{json}\n").as_bytes())
            .await
            .map_err(|e| format!("daemon stdin write failed: {e}"))
    }
}

impl Airplay2Output {
    pub fn new(
        name: String,
        host: String,
        port: u16,
        endpoint_id: String,
        ap_device_id: String,
    ) -> Self {
        let device_id = if endpoint_id.starts_with("airplay2:") {
            endpoint_id
        } else {
            format!("airplay2:{endpoint_id}")
        };
        Self {
            name,
            device_id,
            host,
            port,
            ap_device_id,
            playing: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            position_ms: Arc::new(AtomicU64::new(0)),
            duration_ms: Arc::new(AtomicU64::new(0)),
            volume: Arc::new(Mutex::new(1.0)),
            current_title: Arc::new(Mutex::new(None)),
            current_artist: Arc::new(Mutex::new(None)),
            process: Arc::new(Mutex::new(None)),
        }
    }

    async fn ensure_connected(&self) -> Result<(), String> {
        let mut proc = self.process.lock().await;
        if proc.is_some() {
            return Ok(());
        }

        let binary = find_daemon_binary();
        info!(binary = %binary, device = %self.name, "airplay2: starting daemon");

        let mut child = Command::new(&binary)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| format!("airplay-daemon spawn failed: {e}"))?;

        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;

        let mut daemon = DaemonProcess { child, stdin };

        // Wait for "ready" event
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| e.to_string())?;
        if !line.contains("\"ready\"") {
            return Err(format!("daemon did not send ready: {line}"));
        }

        // Spawn stdout reader to update position
        let playing = self.playing.clone();
        let position_ms = self.position_ms.clone();
        let device_name = self.name.clone();
        tokio::spawn(async move {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        if let Ok(ev) = serde_json::from_str::<serde_json::Value>(&line) {
                            let event = ev["event"].as_str().unwrap_or("");
                            match event {
                                "playing" => {
                                    playing.store(true, Ordering::SeqCst);
                                    debug!(device = %device_name, "airplay2: playing");
                                }
                                "stopped" | "disconnected" => {
                                    playing.store(false, Ordering::SeqCst);
                                    debug!(device = %device_name, "airplay2: stopped");
                                }
                                "status" => {
                                    if let Some(pos) = ev["position_s"].as_f64() {
                                        position_ms.store((pos * 1000.0) as u64, Ordering::Relaxed);
                                    }
                                }
                                "error" => {
                                    let msg = ev["message"].as_str().unwrap_or("unknown");
                                    warn!(device = %device_name, error = %msg, "airplay2: daemon error");
                                }
                                _ => {}
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Send connect command
        daemon
            .send_cmd(&serde_json::json!({
                "cmd": "connect",
                "ip": self.host,
                "port": self.port,
                "device_id": self.ap_device_id,
                "pin": "3939",
            }))
            .await?;

        *proc = Some(daemon);
        info!(device = %self.name, "airplay2: daemon connected");
        Ok(())
    }

    async fn send(&self, cmd: &serde_json::Value) -> Result<(), String> {
        let mut proc = self.process.lock().await;
        if let Some(daemon) = proc.as_mut() {
            daemon.send_cmd(cmd).await
        } else {
            Err("daemon not running".into())
        }
    }
}

#[async_trait::async_trait]
impl OutputTarget for Airplay2Output {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "airplay2"
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        self.ensure_connected().await?;

        let title = media.title.unwrap_or("Unknown");
        let artist = media.artist.unwrap_or("Unknown");
        *self.current_title.lock().await = Some(title.to_owned());
        *self.current_artist.lock().await = Some(artist.to_owned());
        self.duration_ms
            .store(media.duration_ms.unwrap_or(0), Ordering::SeqCst);
        self.position_ms.store(0, Ordering::SeqCst);

        // The daemon plays from a file path or URL
        let path = media.file_path.unwrap_or(media.url);
        self.send(&serde_json::json!({
            "cmd": "play",
            "path": path,
        }))
        .await?;

        self.playing.store(true, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
        info!(device = %self.name, title = %title, "airplay2: play_media");
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.paused.store(true, Ordering::SeqCst);
        self.send(&serde_json::json!({"cmd": "stop"})).await?;
        info!(device = %self.name, "airplay2: pause");
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.paused.store(false, Ordering::SeqCst);
        info!(device = %self.name, "airplay2: resume");
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        self.playing.store(false, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
        self.send(&serde_json::json!({"cmd": "stop"})).await.ok();
        self.send(&serde_json::json!({"cmd": "disconnect"}))
            .await
            .ok();

        // Kill daemon process
        let mut proc = self.process.lock().await;
        if let Some(mut daemon) = proc.take() {
            daemon.child.kill().await.ok();
        }
        info!(device = %self.name, "airplay2: stop");
        Ok(())
    }

    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Err("AirPlay 2 does not support seeking".into())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        *self.volume.lock().await = volume;
        self.send(&serde_json::json!({
            "cmd": "volume",
            "level": volume,
        }))
        .await
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        let vol = if muted {
            0.0
        } else {
            *self.volume.lock().await
        };
        self.send(&serde_json::json!({
            "cmd": "volume",
            "level": vol,
        }))
        .await
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let state = if self.playing.load(Ordering::Relaxed) {
            if self.paused.load(Ordering::Relaxed) {
                TransportState::Paused
            } else {
                TransportState::Playing
            }
        } else {
            TransportState::Stopped
        };

        Ok(OutputStatus {
            state,
            position_ms: self.position_ms.load(Ordering::Relaxed),
            duration_ms: self.duration_ms.load(Ordering::Relaxed),
            volume: *self.volume.lock().await,
            muted: false,
            current_uri: None,
            track_title: self.current_title.lock().await.clone(),
            track_artist: self.current_artist.lock().await.clone(),
            ended_naturally: false,
        })
    }

    async fn is_available(&self) -> bool {
        tokio::net::TcpStream::connect(format!("{}:{}", self.host, self.port))
            .await
            .is_ok()
    }
}

fn find_daemon_binary() -> String {
    // Check common locations
    let candidates = [
        "airplay-daemon",
        "/usr/local/bin/airplay-daemon",
        "/opt/tune-server/airplay-daemon",
    ];
    for path in &candidates {
        if std::path::Path::new(path).exists() {
            return path.to_string();
        }
    }
    // Try `which`
    if let Ok(output) = std::process::Command::new("which")
        .arg(DAEMON_BINARY)
        .output()
    {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return path;
        }
    }
    DAEMON_BINARY.to_string()
}

/// Check if the airplay-daemon binary is available on this system.
pub fn daemon_available() -> bool {
    let binary = find_daemon_binary();
    std::path::Path::new(&binary).exists()
        || std::process::Command::new("which")
            .arg(DAEMON_BINARY)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
}
