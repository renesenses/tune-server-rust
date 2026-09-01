use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::traits::{OutputCapabilities, OutputStatus, OutputTarget, PlayMedia, TransportState};

const FRAMES_PER_PACKET: usize = 352;
const SAMPLE_RATE: u32 = 44100;
const CHANNELS: u16 = 2;
const BYTES_PER_SAMPLE: usize = 2;
const BYTES_PER_FRAME: usize = CHANNELS as usize * BYTES_PER_SAMPLE;
const BYTES_PER_PACKET: usize = FRAMES_PER_PACKET * BYTES_PER_FRAME;
const RTP_HEADER_SIZE: usize = 12;

#[derive(Debug, Default)]
struct PositionRtpAirplay {
    sequence: u16,
    timestamp: u32,
}

/// Prochain couple sequence/timestamp que la boucle RTP emettra.
///
/// `pause()` le lit pour construire le FLUSH. Le couple est reserve avant
/// l'envoi UDP : meme si FLUSH croise un datagramme deja en vol, il designe
/// donc toujours la frontiere qui suit ce datagramme, jamais `0/0` (#2247).
#[derive(Debug)]
struct CurseurRtpAirplay {
    position: std::sync::Mutex<PositionRtpAirplay>,
    marquer_prochain: AtomicBool,
}

impl Default for CurseurRtpAirplay {
    fn default() -> Self {
        Self {
            position: std::sync::Mutex::new(PositionRtpAirplay::default()),
            // Le premier paquet apres RECORD porte aussi le marqueur RTP.
            marquer_prochain: AtomicBool::new(true),
        }
    }
}

impl CurseurRtpAirplay {
    fn reinitialiser(&self) {
        *self
            .position
            .lock()
            .unwrap_or_else(|empoisonne| empoisonne.into_inner()) = PositionRtpAirplay::default();
        self.marquer_prochain.store(true, Ordering::SeqCst);
    }

    fn prochain(&self) -> (u16, u32) {
        let position = self
            .position
            .lock()
            .unwrap_or_else(|empoisonne| empoisonne.into_inner());
        (position.sequence, position.timestamp)
    }

    fn reserver(&self, trames: u32, paused: &AtomicBool) -> Option<(u16, u32, bool)> {
        let mut position = self
            .position
            .lock()
            .unwrap_or_else(|empoisonne| empoisonne.into_inner());
        // Deuxieme lecture SOUS le meme verrou que `pause()` utilise via
        // `prochain()`. Si Pause croise la frontiere d'un paquet, soit celui-ci
        // est deja reserve et FLUSH vise le suivant, soit il ne part pas.
        if paused.load(Ordering::SeqCst) {
            return None;
        }
        let reservee = (position.sequence, position.timestamp);
        position.sequence = position.sequence.wrapping_add(1);
        position.timestamp = position.timestamp.wrapping_add(trames);
        let marqueur = self.marquer_prochain.swap(false, Ordering::SeqCst);
        Some((reservee.0, reservee.1, marqueur))
    }

    fn marquer_reprise(&self) {
        self.marquer_prochain.store(true, Ordering::SeqCst);
    }
}

/// Horloge du cadenceur RTP, distincte du temps media.
///
/// La position RTP ne bouge pas pendant une pause. Son origine murale doit en
/// revanche avancer de la duree de pause, sinon toutes les echeances tombees
/// dans le passe sont servies en rafale a la reprise (#2247).
#[derive(Debug)]
struct HorlogeCadencementRtp {
    origine: tokio::time::Instant,
    debut_pause: Option<tokio::time::Instant>,
}

impl HorlogeCadencementRtp {
    fn new(origine: tokio::time::Instant) -> Self {
        Self {
            origine,
            debut_pause: None,
        }
    }

    fn entrer_pause(&mut self, maintenant: tokio::time::Instant) -> bool {
        if self.debut_pause.is_some() {
            return false;
        }
        self.debut_pause = Some(maintenant);
        true
    }

    fn sortir_pause(&mut self, maintenant: tokio::time::Instant) -> Option<std::time::Duration> {
        let debut = self.debut_pause.take()?;
        let duree = maintenant.saturating_duration_since(debut);
        self.origine += duree;
        Some(duree)
    }

    fn echeance(&self, trames: u64) -> tokio::time::Instant {
        self.origine + std::time::Duration::from_micros(trames * 1_000_000 / SAMPLE_RATE as u64)
    }
}

pub struct AirplayOutput {
    name: String,
    device_id: String,
    host: String,
    port: u16,
    playing: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    position_ms: Arc<AtomicU64>,
    duration_ms: Arc<AtomicU64>,
    volume: Arc<Mutex<f64>>,
    muted: Arc<AtomicBool>,
    current_title: Arc<Mutex<Option<String>>>,
    current_artist: Arc<Mutex<Option<String>>>,
    current_uri: Arc<Mutex<Option<String>>>,
    stop_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
    rtsp_session: Arc<Mutex<Option<RtspSession>>>,
    rtp: Arc<CurseurRtpAirplay>,
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
            volume: Arc::new(Mutex::new(1.0)),
            muted: Arc::new(AtomicBool::new(false)),
            current_title: Arc::new(Mutex::new(None)),
            current_artist: Arc::new(Mutex::new(None)),
            current_uri: Arc::new(Mutex::new(None)),
            stop_tx: Arc::new(Mutex::new(None)),
            rtsp_session: Arc::new(Mutex::new(None)),
            rtp: Arc::new(CurseurRtpAirplay::default()),
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
            return Err(setup_failure_message(code));
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

    async fn record(&mut self, sequence: u16, timestamp: u32) -> Result<(), String> {
        let rtp_info = entete_rtp_info(sequence, timestamp);
        let mut headers = vec![("Range", "npt=0-"), ("RTP-Info", rtp_info.as_str())];
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
            return Err(format!("AirPlay SET_PARAMETER volume failed: {code}"));
        }
        Ok(())
    }

    async fn teardown(&mut self) -> Result<(), String> {
        let mut headers: Vec<(&str, &str)> = Vec::new();
        let session_id = self.session_id.clone().unwrap_or_default();
        if !session_id.is_empty() {
            headers.push(("Session", &session_id));
        }
        let (code, _, _) = self
            .send_request("TEARDOWN", "rtsp://127.0.0.1/1", &headers, None)
            .await
            .map_err(|error| format!("AirPlay TEARDOWN request failed: {error}"))?;
        if !(200..300).contains(&code) {
            return Err(format!("AirPlay TEARDOWN refused by device: {code}"));
        }
        Ok(())
    }

    async fn flush(&mut self, sequence: u16, timestamp: u32) -> Result<(), String> {
        let rtp_info = entete_rtp_info(sequence, timestamp);
        let mut headers = vec![("RTP-Info", rtp_info.as_str())];
        let session_id = self.session_id.clone().unwrap_or_default();
        if !session_id.is_empty() {
            headers.push(("Session", &session_id));
        }
        let (code, _, _) = self
            .send_request("FLUSH", "rtsp://127.0.0.1/1", &headers, None)
            .await?;
        if code != 200 {
            return Err(format!("FLUSH failed: {code}"));
        }
        Ok(())
    }
}

fn entete_rtp_info(sequence: u16, timestamp: u32) -> String {
    format!("seq={sequence};rtptime={timestamp}")
}

fn setup_failure_message(code: u32) -> String {
    if code == 403 {
        return "AirPlay connection refused by the device (403): it may still be in use by \
                another sender or require pairing; stop other playback, verify AirPlay \
                access, then retry"
            .to_string();
    }
    format!("AirPlay SETUP failed: {code}")
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

fn build_rtp_packet(seq: u16, timestamp: u32, ssrc: u32, audio: &[u8], marqueur: bool) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(RTP_HEADER_SIZE + audio.len());
    // V=2, P=0, X=0, CC=0, M selon la frontiere RECORD/FLUSH, PT=96.
    pkt.push(0x80);
    pkt.push(if marqueur { 0xe0 } else { 96 });
    pkt.extend_from_slice(&seq.to_be_bytes());
    pkt.extend_from_slice(&timestamp.to_be_bytes());
    pkt.extend_from_slice(&ssrc.to_be_bytes());
    pkt.extend_from_slice(audio);
    pkt
}

/// Conversion réellement utilisée par le payload RTP L16.
///
/// Un downmix multicanal peut dépasser la pleine échelle 16 bits tout en
/// restant parfaitement valide en i32. Le cast final doit donc saturer : un
/// `as i16` reboucle et transforme un passage fort en distorsion franche.
///
/// Extraite pour être TESTABLE. Tant qu'elle vivait en ligne dans
/// `stream_to_airplay`, la contre-épreuve recopiait sa propre closure de clamp :
/// remettre `let s16 = brut as i16` dans la production laissait le test vert
/// (#2311, JP Robbe).
fn pcm_i32_to_l16_be(samples: &[i32], bit_depth: u16) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|&sample| {
            let scaled = match bit_depth {
                24 => sample >> 8,
                32 => sample >> 16,
                _ => sample,
            };
            (scaled.clamp(i16::MIN as i32, i16::MAX as i32) as i16).to_be_bytes()
        })
        .collect()
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
        // Client partagé (voir `crate::http::client`) : TLS webpki plutôt que
        // le vérificateur de plateforme, et un délai d'attente — une piste
        // entière transite ici, d'où `long_timeout`.
        let resp = crate::http::client::long_timeout()
            .get(url)
            .send()
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

    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(true, true, false, true, true, false)
    }

    fn host(&self) -> Option<&str> {
        Some(&self.host)
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        self.stop().await.ok();
        self.rtp.reinitialiser();

        // Establish RTSP session
        let mut session = RtspSession::connect(&self.host, self.port).await?;

        // Bind UDP socket for RTP
        let udp = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("udp bind: {e}"))?;
        let local_port = udp.local_addr().map(|a| a.port()).unwrap_or(6000);

        session.announce().await?;
        session.setup(local_port).await?;
        let (sequence, timestamp) = self.rtp.prochain();
        session.record(sequence, timestamp).await?;

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
        let rtp = self.rtp.clone();
        let name = self.name.clone();

        tokio::spawn(async move {
            let result = stream_to_airplay(
                &url,
                udp,
                &playing,
                &paused,
                &position_ms,
                &mut stop_rx,
                &rtp,
            )
            .await;

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
        let (sequence, timestamp) = self.rtp.prochain();
        if let Some(ref mut session) = *self.rtsp_session.lock().await {
            if let Err(raison) = session.flush(sequence, timestamp).await {
                self.paused.store(false, Ordering::SeqCst);
                return Err(raison);
            }
        }
        self.rtp.marquer_reprise();
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
        let teardown_error = {
            let mut session_slot = self.rtsp_session.lock().await;
            let error = if let Some(session) = session_slot.as_mut() {
                session.teardown().await.err()
            } else {
                None
            };
            *session_slot = None;
            error
        };
        if let Some(error) = teardown_error {
            warn!(device = %self.name, error = %error, "airplay_teardown_failed");
        }
        self.playing.store(false, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
        info!(device = %self.name, "airplay_stop");
        Ok(())
    }

    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Err("seek not supported on AirPlay".into())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        let volume = volume.clamp(0.0, 1.0);
        let db = linear_to_airplay_db(volume);
        let mut session = self.rtsp_session.lock().await;
        session
            .as_mut()
            .ok_or("AirPlay volume requires an active RTSP session")?
            .set_volume_rtsp(db)
            .await?;
        drop(session);
        *self.volume.lock().await = volume;
        self.muted.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        let volume = *self.volume.lock().await;
        let db = if muted {
            -144.0
        } else {
            linear_to_airplay_db(volume)
        };
        let mut session = self.rtsp_session.lock().await;
        session
            .as_mut()
            .ok_or("AirPlay mute requires an active RTSP session")?
            .set_volume_rtsp(db)
            .await?;
        self.muted.store(muted, Ordering::SeqCst);
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
            volume: *self.volume.lock().await,
            muted: self.muted.load(Ordering::Relaxed),
            current_uri: self.current_uri.lock().await.clone(),
            track_title: self.current_title.lock().await.clone(),
            track_artist: self.current_artist.lock().await.clone(),
            ended_naturally: false,
            // A renderer plays at 1x: keep the poller's wall-clock guards.
            realtime: true,
            // Aucune sortie hors la locale ne produit du DoP : le DSD y part
            // tel quel ou transcode, jamais empaquete dans du PCM 24 bits.
            dop_active: false,
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
    rtp: &CurseurRtpAirplay,
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

    // La session annonce un payload RTP L16 FIXE — 44,1 kHz, 16 bits, stéréo.
    // `decode_to_pcm` garantit désormais cadence et canaux (#2230). Les gardes
    // restent ici à la frontière RTP : si ce contrat régresse, AirPlay adapte
    // encore les canaux puis la cadence au lieu d'émettre un payload mensonger.
    let mut echantillons = decoded.samples_i32;
    if decoded.channels != CHANNELS as u32 {
        info!(
            de = decoded.channels,
            vers = CHANNELS,
            "airplay_downmix_vers_stereo"
        );
        echantillons =
            crate::audio::channels::to_stereo_i32(&echantillons, decoded.channels as u16);
    }
    if decoded.sample_rate != SAMPLE_RATE {
        info!(
            de = decoded.sample_rate,
            vers = SAMPLE_RATE,
            "airplay_reechantillonnage"
        );
        echantillons = crate::audio::resample::resample_i32(
            &echantillons,
            decoded.bit_depth,
            CHANNELS as u16,
            decoded.sample_rate,
            SAMPLE_RATE,
        );
    }

    // Convert i32 samples to i16, then to big-endian bytes for AirPlay RTP
    let pcm_be = pcm_i32_to_l16_be(&echantillons, decoded.bit_depth);

    let ssrc: u32 = rand_random();
    let udp = tokio::net::UdpSocket::from_std(udp).map_err(|e| format!("tokio udp: {e}"))?;

    diffuser_pcm(
        &pcm_be,
        &udp,
        playing,
        paused,
        position_ms,
        stop_rx,
        ssrc,
        rtp,
    )
    .await;

    Ok(())
}

/// La sortie des paquets RTP.
///
/// En production c'est la socket UDP. La contre-epreuve de #2310 a besoin de
/// COMPTER les paquets et de mesurer leur taille : un `UdpSocket` connecte ne
/// permet ni l'un ni l'autre — un datagramme trop grand y echoue en silence,
/// journalise en debug, exactement le symptome qu'on veut interdire.
trait SortieRtp {
    async fn envoyer(&self, pkt: &[u8]);
}

impl SortieRtp for tokio::net::UdpSocket {
    async fn envoyer(&self, pkt: &[u8]) {
        if let Err(e) = self.send(pkt).await {
            debug!(error = %e, "airplay_rtp_send_error");
        }
    }
}

/// Emet le PCM big-endian en paquets RTP, au rythme reel.
async fn diffuser_pcm<S: SortieRtp>(
    pcm_be: &[u8],
    sortie: &S,
    playing: &AtomicBool,
    paused: &AtomicBool,
    position_ms: &AtomicU64,
    stop_rx: &mut tokio::sync::oneshot::Receiver<()>,
    ssrc: u32,
    rtp: &CurseurRtpAirplay,
) {
    let mut total_frames: u64 = 0;
    let mut offset: usize = 0;
    let mut horloge = HorlogeCadencementRtp::new(tokio::time::Instant::now());

    // Pourquoi un booleen plutot qu'une seconde lecture du receiver :
    // `oneshot::Receiver::try_recv()` CONSOMME. La boucle rend le stop, puis
    // la garde du paquet final relisait le meme receiver, qui rendait alors
    // `Err(Closed)` — `!is_ok()` valait `true` PRECISEMENT apres un stop, et
    // le paquet final partait quand meme (#2310, JP Robbe). L'etat d'un
    // canal consommable ne se deduit pas deux fois : on le retient.
    let mut interrompu = false;

    'paquets: while offset + BYTES_PER_PACKET <= pcm_be.len() {
        // Check for stop signal (non-blocking)
        if stop_rx.try_recv().is_ok() {
            interrompu = true;
            break;
        }

        if !playing.load(Ordering::Relaxed) {
            interrompu = true;
            break;
        }

        if paused.load(Ordering::Relaxed) {
            horloge.entrer_pause(tokio::time::Instant::now());
        }
        while paused.load(Ordering::Relaxed) {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if stop_rx.try_recv().is_ok() {
                interrompu = true;
                break 'paquets;
            }
            if !playing.load(Ordering::Relaxed) {
                interrompu = true;
                break 'paquets;
            }
        }
        if let Some(duree) = horloge.sortir_pause(tokio::time::Instant::now()) {
            debug!(
                pause_ms = duree.as_millis(),
                "airplay_horloge_recalee_apres_pause"
            );
        }

        let audio_buf = &pcm_be[offset..offset + BYTES_PER_PACKET];
        let Some((seq, timestamp, marqueur)) = rtp.reserver(FRAMES_PER_PACKET as u32, paused)
        else {
            // Pause a croise la frontiere entre la garde ci-dessus et la
            // reservation. Revenir en tete garantit zero paquet apres FLUSH.
            continue 'paquets;
        };
        let pkt = build_rtp_packet(seq, timestamp, ssrc, audio_buf, marqueur);
        sortie.envoyer(&pkt).await;

        total_frames += FRAMES_PER_PACKET as u64;
        offset += BYTES_PER_PACKET;
        position_ms.store(total_frames * 1000 / SAMPLE_RATE as u64, Ordering::Relaxed);

        // Pace to real-time: sleep until the next packet is due
        let target = horloge.echeance(total_frames);
        tokio::time::sleep_until(target).await;
    }

    // Le dernier paquet PARTIEL. La boucle ci-dessus s'arrete des qu'il reste
    // moins d'un paquet plein, donc jusqu'a 351 trames — 8 ms a 44,1 kHz —
    // etaient jetees a chaque fin de piste (critere de #2237, releve par
    // JP Robbe).
    //
    // RTP n'impose pas une taille de charge utile fixe : un paquet plus court
    // est valide, et c'est ce que fait tout emetteur en fin de flux. On garde
    // l'alignement sur les trames, seule contrainte reelle du L16.
    // Un arret anticipe invalide l'hypothese « il reste moins d'un paquet » :
    // `reste` peut alors valoir TOUTE la piste non lue, et `trames` n'est
    // borne par rien. Trois conditions, aucune deduite : la boucle a bien
    // epuise ses paquets pleins, la lecture n'a pas ete interrompue, et il
    // reste de quoi remplir au moins une trame.
    let reste = pcm_be.len() - offset;
    if !interrompu
        && playing.load(Ordering::Relaxed)
        && reste >= BYTES_PER_FRAME
        && reste < BYTES_PER_PACKET
    {
        let trames = reste / BYTES_PER_FRAME;
        let fin = offset + trames * BYTES_PER_FRAME;
        // Une pause peut aussi tomber entre le dernier paquet plein et ce
        // paquet partiel. Attendre sa vraie reprise au lieu de jeter la fin.
        let reservation = loop {
            if let Some(reservation) = rtp.reserver(trames as u32, paused) {
                break Some(reservation);
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if stop_rx.try_recv().is_ok() || !playing.load(Ordering::Relaxed) {
                break None;
            }
        };
        if let Some((seq, timestamp, marqueur)) = reservation {
            let pkt = build_rtp_packet(seq, timestamp, ssrc, &pcm_be[offset..fin], marqueur);
            sortie.envoyer(&pkt).await;
            total_frames += trames as u64;
            position_ms.store(total_frames * 1000 / SAMPLE_RATE as u64, Ordering::Relaxed);
            debug!(trames, "airplay_dernier_paquet_partiel");
        }
    }
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn setup_403_explains_busy_device_and_pairing() {
        let message = setup_failure_message(403);
        assert!(message.contains("another sender"));
        assert!(message.contains("pairing"));
        assert!(message.contains("retry"));
    }

    #[test]
    fn setup_other_status_keeps_the_rtsp_code() {
        assert_eq!(setup_failure_message(453), "AirPlay SETUP failed: 453");
    }

    #[tokio::test]
    async fn teardown_refusal_is_returned_instead_of_discarded() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
            }
            socket
                .write_all(b"RTSP/1.0 403 Forbidden\r\nCSeq: 1\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8(request).unwrap()
        });

        let mut session = RtspSession::connect("127.0.0.1", address.port())
            .await
            .unwrap();
        session.session_id = Some("stale-session".into());
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), session.teardown())
            .await
            .expect("TEARDOWN response must not hang")
            .unwrap_err();
        let request = tokio::time::timeout(std::time::Duration::from_secs(1), server)
            .await
            .expect("fake RTSP server must finish")
            .unwrap();

        assert!(error.contains("TEARDOWN refused by device: 403"));
        assert!(request.starts_with("TEARDOWN rtsp://127.0.0.1/1 RTSP/1.0"));
        assert!(request.contains("Session: stale-session\r\n"));
    }

    #[test]
    fn volume_conversion() {
        assert_eq!(linear_to_airplay_db(0.0), -144.0);
        assert_eq!(linear_to_airplay_db(1.0), 0.0);
        let half = linear_to_airplay_db(0.5);
        assert!(half < -5.0 && half > -15.0);
    }

    #[test]
    fn pause_de_dix_secondes_decale_l_horloge_sans_rafale() {
        let origine = tokio::time::Instant::now();
        let une_trame_rtp = std::time::Duration::from_micros(
            FRAMES_PER_PACKET as u64 * 1_000_000 / SAMPLE_RATE as u64,
        );
        let mut horloge = HorlogeCadencementRtp::new(origine);

        // Le premier paquet est parti ; la boucle atteint son echeance puis
        // observe Pause. Dix secondes plus tard, elle envoie le paquet suivant
        // immediatement. Seule son echeance SUIVANTE doit rester a ~8 ms.
        let debut_pause = horloge.echeance(FRAMES_PER_PACKET as u64);
        assert!(horloge.entrer_pause(debut_pause));
        let reprise = debut_pause + std::time::Duration::from_secs(10);
        assert_eq!(
            horloge.sortir_pause(reprise),
            Some(std::time::Duration::from_secs(10))
        );

        let prochaine = horloge.echeance((2 * FRAMES_PER_PACKET) as u64);
        let intervalle = prochaine.duration_since(reprise);
        assert!(
            intervalle >= une_trame_rtp
                && intervalle <= une_trame_rtp + std::time::Duration::from_micros(1),
            "apres reprise, cadence attendue ~8 ms, obtenue {intervalle:?}"
        );
    }

    #[test]
    fn flush_et_marqueur_suivent_le_prochain_paquet_reel() {
        let rtp = CurseurRtpAirplay::default();
        let en_pause = AtomicBool::new(false);

        assert_eq!(rtp.prochain(), (0, 0));
        assert_eq!(
            rtp.reserver(FRAMES_PER_PACKET as u32, &en_pause),
            Some((0, 0, true))
        );
        assert_eq!(rtp.prochain(), (1, FRAMES_PER_PACKET as u32));
        let (sequence, timestamp) = rtp.prochain();
        assert_eq!(
            entete_rtp_info(sequence, timestamp),
            format!("seq=1;rtptime={FRAMES_PER_PACKET}")
        );

        assert_eq!(
            rtp.reserver(FRAMES_PER_PACKET as u32, &en_pause),
            Some((1, FRAMES_PER_PACKET as u32, false))
        );
        rtp.marquer_reprise();
        assert_eq!(
            rtp.reserver(FRAMES_PER_PACKET as u32, &en_pause),
            Some((2, (2 * FRAMES_PER_PACKET) as u32, true)),
            "le premier paquet apres FLUSH doit porter le marqueur RTP"
        );

        let pause = AtomicBool::new(true);
        assert_eq!(rtp.reserver(FRAMES_PER_PACKET as u32, &pause), None);
        assert_eq!(
            rtp.prochain(),
            (3, (3 * FRAMES_PER_PACKET) as u32),
            "une reservation croisee par Pause ne doit avancer aucun curseur"
        );
    }

    /// La sortie de test : on garde la taille de charge utile de chaque
    /// paquet reellement emis.
    #[derive(Default)]
    struct SortieDeTest(std::sync::Mutex<Vec<usize>>);

    impl SortieDeTest {
        fn charges(&self) -> Vec<usize> {
            self.0.lock().unwrap().clone()
        }
    }

    impl SortieRtp for SortieDeTest {
        async fn envoyer(&self, pkt: &[u8]) {
            self.0.lock().unwrap().push(pkt.len() - RTP_HEADER_SIZE);
        }
    }

    fn piste(paquets_pleins: usize, trames_en_plus: usize) -> Vec<u8> {
        vec![0u8; paquets_pleins * BYTES_PER_PACKET + trames_en_plus * BYTES_PER_FRAME]
    }

    /// Stop AVANT le premier paquet : plus un seul echantillon ne doit partir.
    ///
    /// C'est la contre-epreuve de #2310. `oneshot::Receiver::try_recv()` est
    /// CONSOMMABLE : la boucle rend le stop et sort ; le second appel, dans la
    /// garde du paquet final, rend alors `Err(Closed)`. `!is_ok()` valait donc
    /// `true` PRECISEMENT apres un stop, et tout le reste de la piste partait
    /// dans un unique datagramme — plusieurs megaoctets, apres le TEARDOWN.
    #[tokio::test]
    async fn stop_avant_le_premier_paquet_n_envoie_rien() {
        let pcm = piste(200, 0);
        let sortie = SortieDeTest::default();
        let playing = AtomicBool::new(true);
        let paused = AtomicBool::new(false);
        let position = AtomicU64::new(0);
        let rtp = CurseurRtpAirplay::default();
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();
        stop_tx.send(()).unwrap();

        diffuser_pcm(
            &pcm,
            &sortie,
            &playing,
            &paused,
            &position,
            &mut stop_rx,
            1,
            &rtp,
        )
        .await;

        assert_eq!(
            sortie.charges(),
            Vec::<usize>::new(),
            "apres un stop, zero paquet : le second try_recv rendait Err(Closed) \
             et ouvrait la garde du paquet final (#2310)"
        );
    }

    /// Stop en cours de piste : meme exigence, et surtout aucun datagramme
    /// hors norme. La piste fait 200 paquets ; le reste non lu ne doit jamais
    /// devenir UN paquet.
    #[tokio::test]
    async fn stop_en_cours_de_piste_ne_deverse_pas_le_reste() {
        let pcm = piste(200, 17);
        let sortie = SortieDeTest::default();
        let playing = AtomicBool::new(true);
        let paused = AtomicBool::new(false);
        let position = AtomicU64::new(0);
        let rtp = CurseurRtpAirplay::default();
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();

        // ~8 ms par paquet : le stop tombe apres quelques paquets, jamais
        // apres les 200.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            stop_tx.send(()).ok();
        });

        diffuser_pcm(
            &pcm,
            &sortie,
            &playing,
            &paused,
            &position,
            &mut stop_rx,
            1,
            &rtp,
        )
        .await;

        let charges = sortie.charges();
        assert!(
            charges.len() < 200,
            "le stop devait interrompre la piste, {} paquets emis",
            charges.len()
        );
        assert!(
            charges.iter().all(|c| *c == BYTES_PER_PACKET),
            "apres un stop, aucun paquet partiel ne doit partir : {:?} (#2310)",
            charges
                .iter()
                .filter(|c| **c != BYTES_PER_PACKET)
                .collect::<Vec<_>>()
        );
    }

    /// La boucle sort aussi quand `playing` tombe (Stop transport, perte du
    /// peripherique). La garde du paquet final ne relisait pas `playing` : le
    /// reste partait pareil, sans qu'aucun stop n'ait ete emis.
    #[tokio::test]
    async fn playing_a_faux_n_envoie_rien() {
        let pcm = piste(200, 0);
        let sortie = SortieDeTest::default();
        let playing = AtomicBool::new(false);
        let paused = AtomicBool::new(false);
        let position = AtomicU64::new(0);
        let rtp = CurseurRtpAirplay::default();
        let (_stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();

        diffuser_pcm(
            &pcm,
            &sortie,
            &playing,
            &paused,
            &position,
            &mut stop_rx,
            1,
            &rtp,
        )
        .await;

        assert_eq!(
            sortie.charges(),
            Vec::<usize>::new(),
            "playing = false : la garde du paquet final doit le revoir (#2310)"
        );
    }

    /// Sortie de pause par un `playing` a faux : rien non plus.
    #[tokio::test]
    async fn pause_puis_arret_n_envoie_rien_de_plus() {
        let pcm = piste(200, 0);
        let sortie = SortieDeTest::default();
        let playing = Arc::new(AtomicBool::new(true));
        let paused = Arc::new(AtomicBool::new(true));
        let position = AtomicU64::new(0);
        let rtp = CurseurRtpAirplay::default();
        let (_stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();

        let p = playing.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            p.store(false, Ordering::SeqCst);
        });

        diffuser_pcm(
            &pcm,
            &sortie,
            &playing,
            &paused,
            &position,
            &mut stop_rx,
            1,
            &rtp,
        )
        .await;

        assert_eq!(
            sortie.charges(),
            Vec::<usize>::new(),
            "en pause puis arret, aucun paquet ne doit partir (#2310)"
        );
    }

    /// Le signal Stop est le premier geste de `AirPlayOutput::stop`, avant le
    /// TEARDOWN RTSP et avant la bascule de `playing`. La boucle RTP doit donc
    /// le consommer même quand elle attend dans Pause : sinon sa tâche reste
    /// suspendue jusqu'à la fin du TEARDOWN ou jusqu'à une reprise qui ne
    /// viendra jamais.
    #[tokio::test]
    async fn stop_pendant_pause_termine_sans_attendre_reprise() {
        let pcm = piste(200, 0);
        let sortie = SortieDeTest::default();
        let playing = AtomicBool::new(true);
        let paused = AtomicBool::new(true);
        let position = AtomicU64::new(0);
        let rtp = CurseurRtpAirplay::default();
        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            stop_tx.send(()).ok();
        });

        tokio::time::timeout(
            std::time::Duration::from_millis(300),
            diffuser_pcm(
                &pcm,
                &sortie,
                &playing,
                &paused,
                &position,
                &mut stop_rx,
                1,
                &rtp,
            ),
        )
        .await
        .expect("Stop doit sortir de Pause sans attendre Resume ni playing=false");

        assert_eq!(sortie.charges(), Vec::<usize>::new());
        assert_eq!(position.load(Ordering::Relaxed), 0);
    }

    /// L'acquis de #2237 ne doit pas etre perdu : une fin NATURELLE emet bien
    /// le dernier paquet partiel, aligne sur les trames.
    #[tokio::test]
    async fn fin_naturelle_emet_le_dernier_paquet_partiel() {
        let pcm = piste(3, 100);
        let sortie = SortieDeTest::default();
        let playing = AtomicBool::new(true);
        let paused = AtomicBool::new(false);
        let position = AtomicU64::new(0);
        let rtp = CurseurRtpAirplay::default();
        let (_stop_tx, mut stop_rx) = tokio::sync::oneshot::channel();

        diffuser_pcm(
            &pcm,
            &sortie,
            &playing,
            &paused,
            &position,
            &mut stop_rx,
            1,
            &rtp,
        )
        .await;

        assert_eq!(
            sortie.charges(),
            vec![
                BYTES_PER_PACKET,
                BYTES_PER_PACKET,
                BYTES_PER_PACKET,
                100 * BYTES_PER_FRAME
            ],
            "fin naturelle : 3 paquets pleins puis les 100 trames restantes (#2237)"
        );
    }

    #[test]
    fn rtp_packet_format() {
        let audio = vec![0u8; BYTES_PER_PACKET];
        let pkt = build_rtp_packet(42, 12345, 0xDEADBEEF, &audio, false);
        assert_eq!(pkt.len(), RTP_HEADER_SIZE + BYTES_PER_PACKET);
        assert_eq!(pkt[0], 0x80);
        assert_eq!(pkt[1], 96);
        assert_eq!(u16::from_be_bytes([pkt[2], pkt[3]]), 42);

        let marque = build_rtp_packet(43, 12697, 0xDEADBEEF, &audio, true);
        assert_eq!(marque[1], 0xe0);
    }

    /// La contre-épreuve de JP Robbe sur #2281, cette fois branchée sur la
    /// PRODUCTION.
    ///
    /// Ma version d'origine vivait dans `channels.rs` et redéfinissait sa
    /// propre closure de clamp : JP a remis `let s16 = brut as i16` dans
    /// `airplay.rs` et le test est resté vert. Il prouvait que `clamp` sature —
    /// ce que personne ne conteste — jamais que le flux RTP l'appelle (#2311).
    ///
    /// Ici c'est `pcm_i32_to_l16_be`, la fonction réellement appelée par
    /// `stream_to_airplay`, qui est exercée.
    #[test]
    fn la_conversion_l16_de_production_reserve_le_headroom_du_downmix_51() {
        let plein = i16::MAX as i32; // source 16 bits à pleine échelle
        // fl, fr, centre, LFE, sl, sr — tous à fond.
        let stereo =
            crate::audio::channels::to_stereo_i32(&[plein, plein, plein, plein, plein, plein], 6);
        assert!(
            stereo[0] <= plein && stereo[0] > plein - 16,
            "la matrice doit réserver son headroom avant la conversion L16 : {}",
            stereo[0]
        );

        let l16 = pcm_i32_to_l16_be(&stereo, 16);
        assert_eq!(
            i16::from_be_bytes([l16[0], l16[1]]),
            i16::MAX,
            "le downmix plein niveau doit rester plein niveau sans dépendre du \
             saturateur de la conversion L16"
        );
        assert_eq!(i16::from_be_bytes([l16[2], l16[3]]), i16::MAX);
    }

    /// Et la profondeur d'origine doit être respectée : un 24 bits pleine
    /// échelle vaut i16::MAX une fois décalé, pas une saturation fortuite.
    #[test]
    fn la_conversion_l16_respecte_la_profondeur_source() {
        let plein24 = (1i32 << 23) - 1;
        let l16 = pcm_i32_to_l16_be(&[plein24, -plein24], 24);
        assert_eq!(i16::from_be_bytes([l16[0], l16[1]]), i16::MAX);
        // Décalage ARITHMÉTIQUE : -8388607 >> 8 arrondit vers moins l'infini,
        // donc -32768 et non -32767. C'est le comportement de la production.
        assert_eq!(i16::from_be_bytes([l16[2], l16[3]]), i16::MIN);
    }
}
