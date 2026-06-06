use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::traits::{OutputStatus, OutputTarget, PlayMedia, TransportState};

const FRAMES_PER_PACKET: usize = 352;
const SAMPLE_RATE: u32 = 44100;
const CHANNELS: u16 = 2;
const BYTES_PER_SAMPLE: usize = 2;
const BYTES_PER_FRAME: usize = CHANNELS as usize * BYTES_PER_SAMPLE;
const BYTES_PER_PACKET: usize = FRAMES_PER_PACKET * BYTES_PER_FRAME;
const RTP_HEADER_SIZE: usize = 12;

pub struct AirplayOutput {
    name: String,
    device_id: String,
    host: String,
    port: u16,
    playing: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    position_ms: Arc<AtomicU64>,
    duration_ms: Arc<AtomicU64>,
    current_title: Arc<Mutex<Option<String>>>,
    current_artist: Arc<Mutex<Option<String>>>,
    current_uri: Arc<Mutex<Option<String>>>,
    stop_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    rtsp_session: Arc<Mutex<Option<RtspSession>>>,
}

struct RtspSession {
    stream: tokio::net::TcpStream,
    cseq: u32,
    session_id: Option<String>,
    server_port: u16,
    timing_port: u16,
}

impl AirplayOutput {
    pub fn new(name: String, device_id: String, host: String, port: u16) -> Self {
        Self {
            name,
            device_id,
            host,
            port,
            playing: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            position_ms: Arc::new(AtomicU64::new(0)),
            duration_ms: Arc::new(AtomicU64::new(0)),
            current_title: Arc::new(Mutex::new(None)),
            current_artist: Arc::new(Mutex::new(None)),
            current_uri: Arc::new(Mutex::new(None)),
            stop_tx: Arc::new(Mutex::new(None)),
            rtsp_session: Arc::new(Mutex::new(None)),
        }
    }
}

impl RtspSession {
    async fn connect(host: &str, port: u16) -> Result<Self, String> {
        let stream = tokio::net::TcpStream::connect((host, port))
            .await
            .map_err(|e| format!("airplay connect {host}:{port}: {e}"))?;
        Ok(Self {
            stream,
            cseq: 0,
            session_id: None,
            server_port: 0,
            timing_port: 0,
        })
    }

    async fn send_request(
        &mut self,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Result<(u32, Vec<(String, String)>, String), String> {
        use tokio::io::AsyncWriteExt;

        self.cseq += 1;
        let cseq = self.cseq;

        let mut req = format!("{method} {uri} RTSP/1.0\r\nCSeq: {cseq}\r\n");
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        if let Some(b) = body {
            req.push_str(&format!("Content-Length: {}\r\n", b.len()));
        }
        req.push_str("\r\n");
        if let Some(b) = body {
            req.push_str(b);
        }

        self.stream
            .write_all(req.as_bytes())
            .await
            .map_err(|e| format!("rtsp write: {e}"))?;

        let mut reader = BufReader::new(&mut self.stream);
        let mut status_line = String::new();
        reader
            .read_line(&mut status_line)
            .await
            .map_err(|e| format!("rtsp read status: {e}"))?;

        let status_code: u32 = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let mut resp_headers = Vec::new();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .await
                .map_err(|e| format!("rtsp read header: {e}"))?;
            let line = line.trim_end().to_string();
            if line.is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                let k = k.trim().to_string();
                let v = v.trim().to_string();
                if k.eq_ignore_ascii_case("Content-Length") {
                    content_length = v.parse().unwrap_or(0);
                }
                resp_headers.push((k, v));
            }
        }

        let mut body_buf = vec![0u8; content_length];
        if content_length > 0 {
            reader
                .read_exact(&mut body_buf)
                .await
                .map_err(|e| format!("rtsp read body: {e}"))?;
        }
        let body_str = String::from_utf8_lossy(&body_buf).to_string();

        Ok((status_code, resp_headers, body_str))
    }

    async fn announce(&mut self) -> Result<(), String> {
        let sdp = format!(
            "v=0\r\n\
             o=iTunes 1 O IN IP4 127.0.0.1\r\n\
             s=iTunes\r\n\
             c=IN IP4 127.0.0.1\r\n\
             t=0 0\r\n\
             m=audio 0 RTP/AVP 96\r\n\
             a=rtpmap:96 L16/{SAMPLE_RATE}/{CHANNELS}\r\n\
             a=fmtp:96 {FRAMES_PER_PACKET} 0 {BYTES_PER_SAMPLE} 40 10 14 {CHANNELS} 255 0 0 {SAMPLE_RATE}\r\n"
        );

        let (code, _, _) = self
            .send_request(
                "ANNOUNCE",
                "rtsp://127.0.0.1/1",
                &[("Content-Type", "application/sdp")],
                Some(&sdp),
            )
            .await?;

        if code != 200 {
            return Err(format!("ANNOUNCE failed: {code}"));
        }
        Ok(())
    }

    async fn setup(&mut self, local_port: u16) -> Result<(), String> {
        let transport = format!(
            "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port={};timing_port={}",
            local_port + 1,
            local_port + 2
        );

        let (code, headers, _) = self
            .send_request(
                "SETUP",
                "rtsp://127.0.0.1/1",
                &[("Transport", &transport)],
                None,
            )
            .await?;

        if code != 200 {
            return Err(format!("SETUP failed: {code}"));
        }

        for (k, v) in &headers {
            if k.eq_ignore_ascii_case("Session") {
                self.session_id = Some(v.clone());
            }
            if k.eq_ignore_ascii_case("Transport") {
                for param in v.split(';') {
                    if let Some(port_str) = param.strip_prefix("server_port=") {
                        self.server_port = port_str.parse().unwrap_or(0);
                    }
                    if let Some(port_str) = param.strip_prefix("timing_port=") {
                        self.timing_port = port_str.parse().unwrap_or(0);
                    }
                }
            }
        }

        Ok(())
    }

    async fn record(&mut self) -> Result<(), String> {
        let mut headers = vec![("Range", "npt=0-"), ("RTP-Info", "seq=0;rtptime=0")];
        let session_id = self.session_id.clone().unwrap_or_default();
        if !session_id.is_empty() {
            headers.push(("Session", &session_id));
        }
        let (code, _, _) = self
            .send_request("RECORD", "rtsp://127.0.0.1/1", &headers, None)
            .await?;

        if code != 200 {
            return Err(format!("RECORD failed: {code}"));
        }
        Ok(())
    }

    async fn set_volume_rtsp(&mut self, volume_db: f64) -> Result<(), String> {
        let body = format!("volume: {volume_db:.1}\r\n");
        let mut headers = vec![("Content-Type", "text/parameters")];
        let session_id = self.session_id.clone().unwrap_or_default();
        if !session_id.is_empty() {
            headers.push(("Session", &session_id));
        }
        let (code, _, _) = self
            .send_request("SET_PARAMETER", "rtsp://127.0.0.1/1", &headers, Some(&body))
            .await?;

        if code != 200 {
            debug!(code, "airplay_set_volume_response");
        }
        Ok(())
    }

    async fn teardown(&mut self) -> Result<(), String> {
        let mut headers: Vec<(&str, &str)> = Vec::new();
        let session_id = self.session_id.clone().unwrap_or_default();
        if !session_id.is_empty() {
            headers.push(("Session", &session_id));
        }
        let _ = self
            .send_request("TEARDOWN", "rtsp://127.0.0.1/1", &headers, None)
            .await;
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), String> {
        let mut headers = vec![("RTP-Info", "seq=0;rtptime=0")];
        let session_id = self.session_id.clone().unwrap_or_default();
        if !session_id.is_empty() {
            headers.push(("Session", &session_id));
        }
        let _ = self
            .send_request("FLUSH", "rtsp://127.0.0.1/1", &headers, None)
            .await;
        Ok(())
    }
}

fn linear_to_airplay_db(volume: f64) -> f64 {
    if volume <= 0.0 {
        -144.0
    } else if volume >= 1.0 {
        0.0
    } else {
        30.0 * (volume.ln() / std::f64::consts::LN_10)
    }
}

fn build_rtp_packet(seq: u16, timestamp: u32, ssrc: u32, audio: &[u8]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(RTP_HEADER_SIZE + audio.len());
    // V=2, P=0, X=0, CC=0, M=0, PT=96
    pkt.push(0x80);
    pkt.push(96);
    pkt.extend_from_slice(&seq.to_be_bytes());
    pkt.extend_from_slice(&timestamp.to_be_bytes());
    pkt.extend_from_slice(&ssrc.to_be_bytes());
    pkt.extend_from_slice(audio);
    pkt
}

/// Temporary file guard that deletes the file on drop.
struct TempFileGuard(std::path::PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Convert a URL to a local file path for native decoding.
/// - Bare paths (`/path/to/file`) are returned as-is.
/// - `file:///path/to/file` URLs have the scheme stripped.
/// - HTTP(S) URLs are downloaded to a temporary file (cleaned up on drop).
async fn url_to_local_path(url: &str) -> Result<(String, Option<TempFileGuard>), String> {
    if let Some(path) = url.strip_prefix("file://") {
        return Ok((path.to_string(), None));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        let resp = reqwest::get(url)
            .await
            .map_err(|e| format!("download {url}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("download {url}: HTTP {}", resp.status()));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("download body: {e}"))?;

        let tmp_dir = std::env::temp_dir();
        let id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp_path = tmp_dir.join(format!("tune_airplay_{id}.pcm"));
        std::fs::write(&tmp_path, &bytes).map_err(|e| format!("write tmp: {e}"))?;
        let path_str = tmp_path.to_string_lossy().to_string();
        return Ok((path_str, Some(TempFileGuard(tmp_path))));
    }
    // Assume bare file path
    Ok((url.to_string(), None))
}

#[async_trait::async_trait]
impl OutputTarget for AirplayOutput {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "airplay"
    }

    fn host(&self) -> Option<&str> {
        Some(&self.host)
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        self.stop().await.ok();

        // Establish RTSP session
        let mut session = RtspSession::connect(&self.host, self.port).await?;

        // Bind UDP socket for RTP
        let udp = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("udp bind: {e}"))?;
        let local_port = udp.local_addr().map(|a| a.port()).unwrap_or(6000);

        session.announce().await?;
        session.setup(local_port).await?;
        session.record().await?;

        let server_port = session.server_port;
        let target_addr = format!("{}:{}", self.host, server_port);
        udp.connect(&target_addr)
            .map_err(|e| format!("udp connect {target_addr}: {e}"))?;
        udp.set_nonblocking(true)
            .map_err(|e| format!("udp nonblocking: {e}"))?;

        *self.rtsp_session.lock().await = Some(session);

        // Store metadata
        *self.current_title.lock().await = media.title.map(String::from);
        *self.current_artist.lock().await = media.artist.map(String::from);
        *self.current_uri.lock().await = Some(media.url.to_string());
        self.position_ms.store(0, Ordering::SeqCst);
        self.duration_ms.store(0, Ordering::SeqCst);
        self.playing.store(true, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);

        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        *self.stop_tx.lock().await = Some(stop_tx);

        let url = media.url.to_string();
        let playing = self.playing.clone();
        let paused = self.paused.clone();
        let position_ms = self.position_ms.clone();
        let name = self.name.clone();

        tokio::spawn(async move {
            let result =
                stream_to_airplay(&url, udp, &playing, &paused, &position_ms, &mut stop_rx).await;

            if let Err(e) = result {
                warn!(device = %name, error = %e, "airplay_stream_error");
            }

            playing.store(false, Ordering::SeqCst);
            info!(device = %name, "airplay_stream_ended");
        });

        info!(device = %self.name, url = media.url, "airplay_play");
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.paused.store(true, Ordering::SeqCst);
        if let Some(ref mut session) = *self.rtsp_session.lock().await {
            session.flush().await.ok();
        }
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.paused.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        if let Some(tx) = self.stop_tx.lock().await.take() {
            let _ = tx.send(());
        }
        if let Some(ref mut session) = *self.rtsp_session.lock().await {
            session.teardown().await.ok();
        }
        *self.rtsp_session.lock().await = None;
        self.playing.store(false, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
        info!(device = %self.name, "airplay_stop");
        Ok(())
    }

    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Err("seek not supported on AirPlay".into())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        let db = linear_to_airplay_db(volume);
        if let Some(ref mut session) = *self.rtsp_session.lock().await {
            session.set_volume_rtsp(db).await?;
        }
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        let db = if muted { -144.0 } else { 0.0 };
        if let Some(ref mut session) = *self.rtsp_session.lock().await {
            session.set_volume_rtsp(db).await?;
        }
        Ok(())
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
            volume: 1.0,
            muted: false,
            current_uri: self.current_uri.lock().await.clone(),
            track_title: self.current_title.lock().await.clone(),
            track_artist: self.current_artist.lock().await.clone(),
        })
    }

    async fn is_available(&self) -> bool {
        tokio::net::TcpStream::connect((&*self.host, self.port))
            .await
            .is_ok()
    }
}

async fn stream_to_airplay(
    url: &str,
    udp: UdpSocket,
    playing: &AtomicBool,
    paused: &AtomicBool,
    position_ms: &AtomicU64,
    stop_rx: &mut tokio::sync::oneshot::Receiver<()>,
) -> Result<(), String> {
    // Resolve URL to a local file path (downloading HTTP URLs if needed)
    let (local_path, _tmp_guard) = url_to_local_path(url).await?;

    // Decode the entire file natively to PCM i16 at AirPlay sample rate, stereo
    let decoded = tokio::task::spawn_blocking({
        let path = local_path.clone();
        move || {
            crate::audio::decode::decode_to_pcm(
                &path,
                Some(SAMPLE_RATE),
                Some(CHANNELS as u32),
                0.0,
                0.0,
            )
        }
    })
    .await
    .map_err(|e| format!("decode join: {e}"))?
    .map_err(|e| format!("native decode: {e}"))?;

    if decoded.samples_i32.is_empty() {
        return Err("decoded audio is empty".into());
    }

    // Convert i32 samples to i16, then to big-endian bytes for AirPlay RTP
    let pcm_be: Vec<u8> = decoded
        .samples_i32
        .iter()
        .flat_map(|&s| {
            let s16 = match decoded.bit_depth {
                24 => (s >> 8) as i16,
                32 => (s >> 16) as i16,
                _ => s as i16,
            };
            s16.to_be_bytes()
        })
        .collect();

    let ssrc: u32 = rand_random();
    let mut seq: u16 = 0;
    let mut timestamp: u32 = 0;
    let mut total_frames: u64 = 0;
    let mut offset: usize = 0;

    let udp = tokio::net::UdpSocket::from_std(udp).map_err(|e| format!("tokio udp: {e}"))?;
    let start_time = tokio::time::Instant::now();

    while offset + BYTES_PER_PACKET <= pcm_be.len() {
        // Check for stop signal (non-blocking)
        if stop_rx.try_recv().is_ok() {
            break;
        }

        if !playing.load(Ordering::Relaxed) {
            break;
        }

        while paused.load(Ordering::Relaxed) {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if !playing.load(Ordering::Relaxed) {
                return Ok(());
            }
        }

        let audio_buf = &pcm_be[offset..offset + BYTES_PER_PACKET];
        let pkt = build_rtp_packet(seq, timestamp, ssrc, audio_buf);
        if let Err(e) = udp.send(&pkt).await {
            debug!(error = %e, "airplay_rtp_send_error");
        }

        seq = seq.wrapping_add(1);
        timestamp = timestamp.wrapping_add(FRAMES_PER_PACKET as u32);
        total_frames += FRAMES_PER_PACKET as u64;
        offset += BYTES_PER_PACKET;
        position_ms.store(total_frames * 1000 / SAMPLE_RATE as u64, Ordering::Relaxed);

        // Pace to real-time: sleep until the next packet is due
        let target = start_time
            + std::time::Duration::from_micros(total_frames * 1_000_000 / SAMPLE_RATE as u64);
        tokio::time::sleep_until(target).await;
    }

    Ok(())
}

fn rand_random() -> u32 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u32)
        .unwrap_or(42)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_conversion() {
        assert_eq!(linear_to_airplay_db(0.0), -144.0);
        assert_eq!(linear_to_airplay_db(1.0), 0.0);
        let half = linear_to_airplay_db(0.5);
        assert!(half < -5.0 && half > -15.0);
    }

    #[test]
    fn rtp_packet_format() {
        let audio = vec![0u8; BYTES_PER_PACKET];
        let pkt = build_rtp_packet(42, 12345, 0xDEADBEEF, &audio);
        assert_eq!(pkt.len(), RTP_HEADER_SIZE + BYTES_PER_PACKET);
        assert_eq!(pkt[0], 0x80);
        assert_eq!(pkt[1], 96);
        assert_eq!(u16::from_be_bytes([pkt[2], pkt[3]]), 42);
    }
}
