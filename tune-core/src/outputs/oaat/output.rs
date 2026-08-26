use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use tokio::sync::Mutex;
#[cfg(feature = "oaat")]
use tracing::{debug, error, info, warn};

use crate::outputs::traits::{OutputStatus, OutputTarget, PlayMedia, TransportState};

#[cfg(feature = "oaat")]
use super::helpers::{
    BaseDeTempsOaat, StreamInfo, detect_and_parse, dsd_rate_from_sample_rate, duree_audio_envoyee,
    format_rate_display,
};

#[cfg(feature = "oaat")]
enum OaatCommand {
    Pause,
    Resume,
    SetVolume(u8),
    Mute(bool),
    Seek {
        position_ms: u64,
    },
    PrepareNext {
        url: String,
        /// Local `.dsf` path for native-DSD gapless: when set (and format-
        /// compatible) the native DSD loop opens it and swaps readers at EOF.
        /// None on the PCM/HTTP path, which prefetches `url` instead.
        file_path: Option<String>,
        title: String,
        artist: String,
        album: String,
        cover_url: Option<String>,
        duration_ms: u64,
    },
}

/// A native-DSD next track pre-opened during playback of the current track, so
/// the loop can swap to it at EOF with no gap (Xavier / Zicmu native DSD album).
#[cfg(feature = "oaat")]
struct PreparedDsdNext {
    reader: crate::audio::dsf::DsfStreamReader,
    title: String,
    artist: String,
    album: String,
    cover_url: Option<String>,
    duration_ms: u64,
}

#[cfg(feature = "oaat")]
struct NextTrackPrefetch {
    stream: futures_util::stream::BoxStream<'static, Result<bytes::Bytes, reqwest::Error>>,
    buf: Vec<u8>,
    info: StreamInfo,
    title: String,
    artist: String,
    album: String,
    cover_url: Option<String>,
    duration_ms: u64,
    same_format: bool,
}

/// Observable OAAT diagnostics — safe to read from any thread.
#[derive(Default)]
pub struct OaatDiagnostics {
    pub packets_sent: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub reconnects: AtomicU32,
    pub last_packet_epoch_ms: AtomicU64,
    pub format_desc: std::sync::Mutex<String>,
    pub connected: AtomicBool,
    pub is_flac: AtomicBool,
}

pub struct OaatOutput {
    name: String,
    device_id: String,
    host: String,
    port: u16,
    controller_id: String,
    stream_counter: Arc<AtomicU32>,
    playing: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    /// True while the output is streaming native DSD (raw DSF bits) to the
    /// endpoint. That playback loop does NOT consume a staged next track
    /// (`OaatCommand::PrepareNext` is a no-op on the DSD path), so the output
    /// cannot chain internally in this mode — `supports_internal_gapless()`
    /// reports false so the poller advances the queue at natural end instead of
    /// waiting forever for a transition that never comes (Xavier, Zicmu, native
    /// DSD album stall). Stays false for PCM/FLAC/HTTP playback, which keeps its
    /// working internal gapless. Set when the native DSD path commits; reset by
    /// `stop()` (called at the start of every `play_media`).
    native_dsd_active: Arc<AtomicBool>,
    /// True while the PCM/FLAC direct-file playback loop runs. That loop
    /// explicitly ignores `OaatCommand::PrepareNext` ("Seek/PrepareNext/etc.
    /// are not handled on the direct path") and ends with LAST_PACKET + Stop
    /// + return — no internal transition is possible there. While set,
    /// `supports_internal_gapless()` reports false so the poller advances the
    /// queue itself at natural end instead of waiting forever for a
    /// transition that never comes (« le morceau suivant ne démarre pas, le
    /// dernier est rejoué » — local→local sur zone OAAT, .18, 29/07). Reset
    /// by `stop()` (called at the start of every `play_media`).
    direct_pcm_active: Arc<AtomicBool>,
    /// Set by a playback loop when it ends with nothing to chain into — the
    /// direct-file loop AND the HTTP-stream loop. The poller re-reads
    /// `supports_internal_gapless()` while it waits for a transition, so
    /// flipping this releases it on the next tick instead of letting it sit out
    /// its guard.
    ///
    /// The HTTP loop used to leave it alone, and that is the second half of
    /// Xavier Joly's 83 seconds (#1323): once the loop was gone — end of the
    /// queue, prefetch that returned nothing, renegotiation refused, retries
    /// spent — the output still claimed it could transition internally, so the
    /// poller sat in `gapless_natural_end_waiting_for_transition` waiting on a
    /// task that no longer existed (34 s measured in his log, 16:34:06 →
    /// 16:34:40), then restarted the track from cold.
    chain_exhausted: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    position_ms: Arc<AtomicU64>,
    duration_ms: Arc<AtomicU64>,
    /// Start position (ms) for the next play_media call. Set by the
    /// orchestrator on seek-recreate; consumed (reset to 0) by play_media.
    /// Needed because the native DSD path reads the file directly and
    /// ignores the seek-positioned HTTP transcode URL.
    pending_start_ms: Arc<AtomicU64>,
    current_uri: Arc<Mutex<Option<String>>>,
    current_title: Arc<Mutex<Option<String>>>,
    current_artist: Arc<Mutex<Option<String>>>,
    stop_tx: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    #[cfg(feature = "oaat")]
    command_tx: Mutex<Option<tokio::sync::mpsc::Sender<OaatCommand>>>,
    /// Tâche de lecture en cours, gardée pour pouvoir l'ANNULER.
    ///
    /// `play_media` détache sa tâche, et celle-ci commence par une boucle de
    /// connexion de quinze tentatives. Sans ce garde, une nouvelle lecture
    /// démarrée pendant que la précédente tourne encore laisse DEUX boucles
    /// courir en parallèle — or l'endpoint OAAT n'accepte qu'un client, donc
    /// chacune chasse l'autre et le flux repart de zéro à chaque fois.
    ///
    /// Observé sur .42 le 2026-08-11 : `attempt=1` et `attempt=9` progressant
    /// à la même seconde, deux sockets vers le même endpoint, et un
    /// `format accepted` aussitôt suivi d'un nouveau `connected`. La piste
    /// reboucle sur ses premières secondes.
    play_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Le motif du dernier refus de negociation de format, en attente d'etre
    /// remis au poller.
    ///
    /// Refuser une contre-proposition qu'on ne sait pas honorer est le bon
    /// choix — envoyer des octets mal etiquetes produit vitesse fausse,
    /// entrelacement faux ou bruit. Mais la raison restait dans le journal
    /// serveur : cote utilisateur, la zone se taisait sans rien dire, et
    /// `play_media` avait deja rendu la main quand la tache asynchrone
    /// decouvrait le refus (#2294, JP Robbe).
    ///
    /// `take_output_failure()` est le canal deja ouvert pour ce cas exact — le
    /// poller le lit a chaque tick et emet `zone.playback_error` avec
    /// `fatal: true`, ce qui traverse jusqu'au client.
    refus_negociation: Arc<std::sync::Mutex<Option<String>>>,
    pub diag: Arc<OaatDiagnostics>,
}

/// Deposer un refus a destination du poller.
///
/// Le premier refus gagne : c'est celui qui a arrete la lecture, les suivants
/// n'en seraient que la consequence.
#[cfg(feature = "oaat")]
fn signaler_refus_negociation(
    depot: &Arc<std::sync::Mutex<Option<String>>>,
    refus: &RefusNegociation,
) {
    if let Ok(mut place) = depot.lock() {
        if place.is_none() {
            *place = Some(refus.raison.clone());
        }
    }
}

impl OaatOutput {
    pub fn new(name: String, host: String, port: u16, endpoint_id: String) -> Self {
        let device_id = if endpoint_id.starts_with("oaat:") {
            endpoint_id
        } else {
            format!("oaat:{endpoint_id}")
        };
        Self {
            name,
            device_id,
            host,
            port,
            controller_id: uuid::Uuid::new_v4().to_string(),
            stream_counter: Arc::new(AtomicU32::new(1)),
            playing: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            native_dsd_active: Arc::new(AtomicBool::new(false)),
            direct_pcm_active: Arc::new(AtomicBool::new(false)),
            chain_exhausted: Arc::new(AtomicBool::new(false)),
            refus_negociation: Arc::new(std::sync::Mutex::new(None)),
            // `volume_set` du protocole est en 0–100 (RFC), comme le
            // multiroom et l'endpoint : on stocke la meme echelle. L'ancien
            // 800, divise par 255 a la lecture, rapportait un volume de 314 %.
            volume: Arc::new(AtomicU32::new(100)),
            position_ms: Arc::new(AtomicU64::new(0)),
            duration_ms: Arc::new(AtomicU64::new(0)),
            pending_start_ms: Arc::new(AtomicU64::new(0)),
            current_uri: Arc::new(Mutex::new(None)),
            current_title: Arc::new(Mutex::new(None)),
            current_artist: Arc::new(Mutex::new(None)),
            stop_tx: Mutex::new(None),
            #[cfg(feature = "oaat")]
            command_tx: Mutex::new(None),
            play_task: Mutex::new(None),
            diag: Arc::new(OaatDiagnostics::default()),
        }
    }

    /// Arm a start position for the next play_media (orchestrator seek path).
    pub fn set_pending_start_position_ms(&self, ms: u64) {
        self.pending_start_ms.store(ms, Ordering::SeqCst);
    }

    /// Le flux part-il en DSD 1 bit, lu directement depuis le `.dsf` ?
    ///
    /// Sur ce chemin l'orchestrateur ne décode rien — c'est tout l'objet du
    /// correctif du blocage Zicmu : une URL de transcodage armée ici
    /// orphelinait un décodage que personne ne lit. Personne ne produit donc
    /// de fenêtre de niveaux, et les VU-mètres n'ont aucune source. Ce qui se
    /// mesure ailleurs ne se mesure pas ici, et l'écran doit pouvoir le dire.
    pub fn is_native_dsd_active(&self) -> bool {
        self.native_dsd_active.load(Ordering::Relaxed)
    }

    // These three exist only for `integration_test`, which is gated on
    // `all(test, feature = "oaat")`. Gating them on `test` alone — or not at
    // all — leaves them compiled but unused in a plain `cargo test`, which is
    // where the dead_code warnings came from. Matching the consumer's cfg
    // exactly is what keeps the build quiet in both configurations.

    /// Test-only: drive the native-DSD mode flag that gates
    /// `supports_internal_gapless()`.
    #[cfg(all(test, feature = "oaat"))]
    pub(crate) fn set_native_dsd_active_for_test(&self, active: bool) {
        self.native_dsd_active.store(active, Ordering::SeqCst);
    }

    /// Test-only: raise the flag the direct-file loop sets when it reaches an
    /// end with nothing to chain into.
    #[cfg(all(test, feature = "oaat"))]
    pub(crate) fn set_chain_exhausted_for_test(&self, exhausted: bool) {
        self.chain_exhausted.store(exhausted, Ordering::SeqCst);
    }

    /// Test-only: mark the direct-file playback path as active.
    #[cfg(all(test, feature = "oaat"))]
    pub(crate) fn set_direct_pcm_active_for_test(&self, active: bool) {
        self.direct_pcm_active.store(active, Ordering::SeqCst);
    }

    pub fn diagnostics_snapshot(&self) -> serde_json::Value {
        let playing = self.playing.load(Ordering::Relaxed);
        let last_pkt = self.diag.last_packet_epoch_ms.load(Ordering::Relaxed);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let stale_ms = if last_pkt > 0 && playing {
            now_ms.saturating_sub(last_pkt)
        } else {
            0
        };

        serde_json::json!({
            "device_id": self.device_id,
            "name": self.name,
            "host": self.host,
            "port": self.port,
            "controller_id": self.controller_id,
            "connected": self.diag.connected.load(Ordering::Relaxed),
            "playing": playing,
            "paused": self.paused.load(Ordering::Relaxed),
            "is_flac": self.diag.is_flac.load(Ordering::Relaxed),
            "format": *self.diag.format_desc.lock().unwrap(),
            "packets_sent": self.diag.packets_sent.load(Ordering::Relaxed),
            "bytes_sent": self.diag.bytes_sent.load(Ordering::Relaxed),
            "reconnects": self.diag.reconnects.load(Ordering::Relaxed),
            "position_ms": self.position_ms.load(Ordering::Relaxed),
            "duration_ms": self.duration_ms.load(Ordering::Relaxed),
            "last_packet_age_ms": stale_ms,
            "stall_detected": playing && !self.paused.load(Ordering::Relaxed) && stale_ms > 5000,
        })
    }

    fn endpoint_addr(&self) -> std::net::SocketAddr {
        format!("{}:{}", self.host, self.port).parse().unwrap()
    }
}

#[cfg(feature = "oaat")]
const FLAC_CHUNK_SIZE: usize = 4096;
#[cfg(feature = "oaat")]
const DSD_CHUNK_SIZE: usize = 4096;
#[cfg(feature = "oaat")]
const PCM_SAMPLES_PER_PACKET: usize = 480;
#[cfg(feature = "oaat")]
const MAX_RECONNECT_ATTEMPTS: u32 = 2;
/// Max HTTP Range-resume attempts after a mid-stream body-read error before the
/// OAAT HTTP fetch loop gives up on the track (per-track budget; reset once the
/// stream delivers data again).
#[cfg(feature = "oaat")]
const MAX_STREAM_RETRY_ATTEMPTS: u32 = 3;

/// How close to the declared duration a body error must land to count as the
/// end of the track rather than a failure.
///
/// The Content-Length of a progressive WAV transcode is predicted from the
/// library duration, so the real body runs short by the prediction error —
/// tens of milliseconds in practice. One second leaves ample room for that
/// while keeping a genuine late-track cut on the resume path, where it belongs.
const END_OF_TRACK_TOLERANCE_MS: u64 = 1_000;

#[cfg(feature = "oaat")]
async fn connect_and_setup(
    config: &oaat_controller::ControllerConfig,
    endpoint_addr: std::net::SocketAddr,
    device_name: &str,
    stream_id: &str,
    stream_info: &StreamInfo,
    refus_negociation: &Arc<std::sync::Mutex<Option<String>>>,
) -> Option<oaat_controller::ConnectedEndpoint> {
    use oaat_core::ChannelLayout;

    let mut endpoint = match oaat_controller::ConnectedEndpoint::connect(config, endpoint_addr)
        .await
    {
        Ok(ep) => {
            info!(device = %device_name, endpoint_name = %ep.info.endpoint_name, "oaat: reconnected");
            ep
        }
        Err(e) => {
            error!(device = %device_name, error = %e, "oaat: reconnect failed");
            return None;
        }
    };

    // Quick clock sync (2 exchanges instead of full 10)
    match tokio::time::timeout(std::time::Duration::from_secs(3), async {
        for seq in 0..2u16 {
            let _ = endpoint.clock_sync_once(seq).await;
        }
    })
    .await
    {
        Ok(()) => {}
        Err(_) => info!(device = %device_name, "oaat: reconnect clock sync skipped (timeout)"),
    }

    let ch = stream_info.channels.min(8) as u8;
    if let Err(e) = endpoint
        .propose_format(
            stream_id,
            stream_info.format,
            stream_info.sample_rate,
            ch,
            ChannelLayout::Stereo,
            stream_info.bits_per_sample as u8,
        )
        .await
    {
        error!(device = %device_name, error = %e, "oaat: reconnect format propose failed");
        return None;
    }

    // `propose_format` n'envoie pas de `dsd_rate` : le contrat n'en porte pas.
    let contrat = ContratPropose {
        stream_id: stream_id.to_string(),
        format: stream_info.format,
        sample_rate: stream_info.sample_rate,
        channels: ch,
        channel_layout: ChannelLayout::Stereo,
        bits_per_sample: stream_info.bits_per_sample,
        dsd_rate: None,
    };
    if let Err(refus) = attendre_accord_format(
        &mut endpoint,
        device_name,
        &contrat,
        PolitiqueAdaptation::ExacteSeulement,
        std::time::Duration::from_secs(3),
    )
    .await
    {
        error!(device = %device_name, raison = %refus.raison, "oaat: reconnect format negotiation failed");
        signaler_refus_negociation(refus_negociation, &refus);
        return None;
    }

    if let Err(e) = endpoint.send_play(stream_id).await {
        error!(device = %device_name, error = %e, "oaat: reconnect play failed");
        return None;
    }

    info!(device = %device_name, "oaat: reconnected and resumed");
    Some(endpoint)
}

#[async_trait::async_trait]
impl OutputTarget for OaatOutput {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "oaat"
    }

    /// OAAT genuinely chains tracks internally in BOTH modes, so it always
    /// reports true (the poller must arm gapless for the transition to fire):
    /// - PCM/FLAC/HTTP: prefetch the next track's URL and swap streams (see
    ///   `OaatCommand::PrepareNext` on the HTTP path, "oaat: gapless transition").
    /// - native DSD: open the next track's local `.dsf` and swap `DsfStreamReader`
    ///   at EOF without tearing the connection down ("oaat: gapless transition
    ///   (native DSD)"). If the next track isn't a compatible native DSD file,
    ///   the loop ends cleanly and the poller's natural-end fallback advances.
    ///
    /// The answer is a live probe, not a static capability: a loop that has
    /// ENDED can no longer transition, whatever it was capable of a second
    /// earlier. Both loops raise `chain_exhausted` when they leave with nothing
    /// staged (end of queue, next track not local, format change refused,
    /// decode failure, retries spent), which is what returns the queue to the
    /// poller's natural-end advance — the guarantee that #1006 was about, kept
    /// intact, and what stops the poller waiting on a dead task (#1323).
    fn supports_internal_gapless(&self) -> bool {
        !self.chain_exhausted.load(Ordering::Relaxed)
    }

    /// True for the two paths that chain by opening the NEXT track's local file
    /// rather than consuming a transcode URL:
    ///
    /// - **native DSD**, which streams raw DSD bits and cannot read the
    ///   orchestrator's DSD->PCM transcode URL at all (an armed URL would orphan
    ///   a decode nobody reads — `dsd_streaming_send_timeout`, the Xavier/Zicmu
    ///   stall);
    /// - **direct PCM/FLAC file playback**, which decodes whole files to a PCM
    ///   buffer and swaps buffers at EOF.
    ///
    /// Returning true tells the poller's `prepare_gapless` to resolve the next
    /// track as a LOCAL FILE and stage it via `set_next_media(file_path=..)`.
    /// False for the HTTP-stream path, which keeps the URL-prefetch gapless.
    fn prefers_local_file_gapless(&self) -> bool {
        self.native_dsd_active.load(Ordering::Relaxed)
            || self.direct_pcm_active.load(Ordering::Relaxed)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    #[cfg(feature = "oaat")]
    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        use oaat_controller::{ConnectedEndpoint, ControllerConfig};
        use oaat_core::ChannelLayout;
        use oaat_core::format::AudioFormat;
        use oaat_core::wire::PacketFlags;

        self.stop().await.ok();
        tokio::time::sleep(std::time::Duration::from_millis(2000)).await;

        let url = media.url.to_owned();
        let file_path = media.file_path.map(|s| s.to_owned());
        let title = media.title.unwrap_or("Unknown").to_owned();
        let artist = media.artist.unwrap_or("Unknown").to_owned();
        let album = media.album.unwrap_or("").to_owned();
        let cover_url = media.cover_url.map(|s| s.to_owned());
        let track_duration_ms = media.duration_ms.unwrap_or(0);
        // Armed by the orchestrator when a seek recreates the stream; the
        // native DSD path uses it to start reading the file at that offset.
        let start_position_ms = self.pending_start_ms.swap(0, Ordering::SeqCst);

        *self.current_uri.lock().await = Some(url.clone());
        *self.current_title.lock().await = Some(title.clone());
        *self.current_artist.lock().await = Some(artist.clone());
        self.duration_ms.store(track_duration_ms, Ordering::SeqCst);

        info!(device = %self.name, url = %url, title = %title, "oaat: play_media");

        let endpoint_addr = self.endpoint_addr();
        let playing = self.playing.clone();
        let paused = self.paused.clone();
        let native_dsd_active = self.native_dsd_active.clone();
        let direct_pcm_active = self.direct_pcm_active.clone();
        let chain_exhausted = self.chain_exhausted.clone();
        // Une nouvelle lecture efface le refus de la precedente : un motif
        // perime tuerait la piste suivante, qui n'y est pour rien.
        if let Ok(mut place) = self.refus_negociation.lock() {
            *place = None;
        }
        let refus_negociation = self.refus_negociation.clone();
        let position_ms = self.position_ms.clone();
        let duration_ms_arc = self.duration_ms.clone();
        let current_title = self.current_title.clone();
        let current_artist = self.current_artist.clone();
        let current_uri = self.current_uri.clone();
        let diag = self.diag.clone();
        let device_name = self.name.clone();
        let controller_id = self.controller_id.clone();
        let stream_num = self.stream_counter.fetch_add(1, Ordering::SeqCst);

        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        *self.stop_tx.lock().await = Some(stop_tx);

        let (command_tx, mut command_rx) = tokio::sync::mpsc::channel::<OaatCommand>(32);
        *self.command_tx.lock().await = Some(command_tx);

        playing.store(true, Ordering::SeqCst);
        paused.store(false, Ordering::SeqCst);
        position_ms.store(start_position_ms, Ordering::SeqCst);

        // Anchor the stall clock to this play's start. The stall supervisor
        // flags a zone stalled when `last_packet_age_ms > 30s`, but that field
        // otherwise keeps the PREVIOUS track's packet timestamp until the first
        // packet of THIS play flows. A zone idle > 30s (e.g. starting a radio
        // after a pause) was therefore flagged stalled on the very first
        // supervisor tick and restarted mid-startup — tearing down the
        // freshly-created stream session before the endpoint had finished
        // connecting and fetched it (404), which is the audible cut (FIP on
        // .18). Resetting here starts the 30s budget from play, giving the
        // endpoint the full window to deliver its first packet; a genuinely
        // dead startup still trips the supervisor 30s later.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        self.diag
            .last_packet_epoch_ms
            .store(now_ms, Ordering::SeqCst);

        // Une lecture précédente peut encore tourner : sa boucle de connexion
        // fait quinze tentatives, et rien ne l'arrête. Deux boucles en
        // parallèle sur un endpoint mono-client se volent la connexion, et la
        // piste reboucle sur ses premières secondes (#1475). On annule donc
        // l'ancienne AVANT d'en lancer une nouvelle.
        if let Some(previous) = self.play_task.lock().await.take() {
            if !previous.is_finished() {
                debug!(device = %self.name, "oaat: annulation de la lecture precedente");
                previous.abort();
            }
        }

        let task = tokio::spawn(async move {
            use futures_util::StreamExt;

            debug!(device = %device_name, url = %url, "oaat: play_media spawned");

            let config = ControllerConfig {
                controller_id,
                controller_name: "Tune Server".into(),
                features: vec![],
                clock_port: super::helpers::oaat_clock_port(),
                tls: false,
            };

            // Connect with retry — each attempt has a 3s timeout to avoid
            // hanging on TCP SYN timeout (127s on Linux) when the endpoint
            // is restarting its listener after a track stop.
            let mut endpoint: Option<ConnectedEndpoint> = None;
            for attempt in 1..=15u32 {
                info!(device = %device_name, addr = %endpoint_addr, attempt, "oaat: connecting");
                match tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    ConnectedEndpoint::connect(&config, endpoint_addr),
                )
                .await
                {
                    Ok(Ok(ep)) => {
                        info!(device = %device_name, endpoint_name = %ep.info.endpoint_name, "oaat: connected");
                        endpoint = Some(ep);
                        break;
                    }
                    Ok(Err(e)) => {
                        if attempt < 15 {
                            let delay = 500 + 300 * attempt as u64;
                            info!(device = %device_name, error = %e, attempt, delay_ms = delay, "oaat: connect retry");
                            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                        } else {
                            error!(device = %device_name, error = %e, "oaat: connect failed after 15 attempts");
                            playing.store(false, Ordering::SeqCst);
                            return;
                        }
                    }
                    Err(_) => {
                        if attempt < 15 {
                            let delay = 500 + 300 * attempt as u64;
                            info!(device = %device_name, attempt, delay_ms = delay, "oaat: connect timed out, retry");
                            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                        } else {
                            error!(device = %device_name, "oaat: connect timed out after 15 attempts");
                            playing.store(false, Ordering::SeqCst);
                            return;
                        }
                    }
                }
            }
            let mut endpoint = endpoint.unwrap();

            // Clock sync with timeout
            match tokio::time::timeout(
                std::time::Duration::from_secs(2),
                endpoint.clock_sync_bootstrap(),
            )
            .await
            {
                Ok(Ok(())) => info!(device = %device_name, "oaat: clock sync ok"),
                Ok(Err(e)) => {
                    info!(device = %device_name, error = %e, "oaat: clock sync failed, continuing")
                }
                Err(_) => info!(device = %device_name, "oaat: clock sync timed out, continuing"),
            }

            let http_client = crate::http::client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default();

            // Fetch & detect format
            let stream_id = format!("tune-{stream_num}");

            // If we have a local file path and it's a format we can parse
            // natively (WAV, FLAC, DSF), read directly instead of HTTP.
            // Compressed formats (MP3, AAC, ALAC, etc.) fall through to
            // HTTP streaming where the orchestrator already decoded them to WAV.
            if let Some(ref fp) = file_path {
                debug!("reading file directly: {fp}");
                let direct_ok = 'direct: {
                    // Fast path for native DSD: avoid loading multi-hundred-MB
                    // DSF files into RAM when the endpoint can take DSD_U8.
                    let is_dsf = std::path::Path::new(fp)
                        .extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| e.eq_ignore_ascii_case("dsf"));
                    if is_dsf && endpoint.info.capabilities.dsd_max_rate.is_some() {
                        let dsf_info = match crate::audio::dsf::parse_dsf(fp) {
                            Ok(i) => i,
                            Err(e) => {
                                debug!("native DSD parse_dsf failed: {e}");
                                break 'direct false;
                            }
                        };
                        let dsd_mult = dsd_rate_from_sample_rate(dsf_info.sample_rate);
                        if let (Some(m), Some(max)) =
                            (dsd_mult, endpoint.info.capabilities.dsd_max_rate)
                        {
                            if m > max {
                                debug!(
                                    dsd = m,
                                    max, "DSD rate exceeds endpoint max, falling back to PCM"
                                );
                                break 'direct false;
                            }
                        }
                        let mut reader =
                            match crate::audio::dsf::DsfStreamReader::open(fp, dsf_info.clone()) {
                                Ok(r) => r,
                                Err(e) => {
                                    debug!("DsfStreamReader open failed: {e}");
                                    break 'direct false;
                                }
                            };

                        let cur_format = AudioFormat::DsdU8;
                        let cur_sample_rate = dsf_info.sample_rate;
                        let ch = dsf_info.channels.min(8) as u8;
                        let layout = ChannelLayout::Stereo;
                        let fmt_str = format_rate_display(cur_sample_rate, 1, cur_format);

                        info!(
                            device = %device_name,
                            sample_rate = cur_sample_rate,
                            channels = ch,
                            dsd_rate = ?dsd_mult,
                            "oaat: native DSD streaming"
                        );

                        // Mark native-DSD mode so supports_internal_gapless()
                        // reports false: the poller then skips prepare_gapless
                        // (no orphaned transcode) and advances the queue at
                        // natural end instead of waiting for an internal
                        // transition this path cannot perform. NOT cleared at
                        // completion below — the poller reads it AFTER the track
                        // stops to decide the end-of-track branch; stop() (called
                        // at the start of the next play_media) resets it.
                        native_dsd_active.store(true, Ordering::SeqCst);

                        if let Err(e) = endpoint
                            .send_message(&oaat_core::Message::FormatPropose(
                                oaat_core::message::FormatPropose {
                                    stream_id: stream_id.clone(),
                                    format: cur_format,
                                    sample_rate: cur_sample_rate,
                                    channels: ch,
                                    channel_layout: layout,
                                    bits_per_sample: 1,
                                    dsd_rate: dsd_mult,
                                },
                            ))
                            .await
                        {
                            error!(device = %device_name, error = %e, "oaat: DSD format propose failed");
                            playing.store(false, Ordering::SeqCst);
                            return;
                        }

                        // Lire la reponse AVANT de jouer (#2282).
                        let contrat = ContratPropose {
                            stream_id: stream_id.clone(),
                            format: cur_format,
                            sample_rate: cur_sample_rate,
                            channels: ch,
                            channel_layout: layout,
                            bits_per_sample: 1,
                            // Ce chemin pose son `FormatPropose` a la main et y
                            // met bien un multiplicateur DSD : exiger `None` en
                            // face refusait une contre-proposition DSD64
                            // identique a la proposition DSD64 (#2283).
                            dsd_rate: dsd_mult,
                        };
                        if let Err(refus) = attendre_accord_format(
                            &mut endpoint,
                            &device_name,
                            &contrat,
                            PolitiqueAdaptation::ExacteSeulement,
                            std::time::Duration::from_secs(5),
                        )
                        .await
                        {
                            error!(device = %device_name, raison = %refus.raison, "oaat: DSD format non accepte, on ne joue pas");
                            signaler_refus_negociation(&refus_negociation, &refus);
                            playing.store(false, Ordering::SeqCst);
                            return;
                        }

                        endpoint
                            .send_metadata(oaat_core::message::TrackMetadata {
                                title: title.clone(),
                                artist: artist.clone(),
                                album: album.clone(),
                                duration_ms: track_duration_ms,
                                artwork_url: cover_url.clone(),
                                format: Some(fmt_str),
                            })
                            .await
                            .ok();

                        if let Err(e) = endpoint.send_play(&stream_id).await {
                            error!(device = %device_name, error = %e, "oaat: DSD play failed");
                            playing.store(false, Ordering::SeqCst);
                            return;
                        }

                        diag.connected.store(true, Ordering::SeqCst);

                        // Prefer file header duration when the PlayMedia
                        // metadata left duration at 0 (disables the seek bar).
                        if duration_ms_arc.load(Ordering::Relaxed) == 0 && dsf_info.sample_rate > 0
                        {
                            let d_ms = dsf_info.total_samples * 1000 / dsf_info.sample_rate as u64;
                            duration_ms_arc.store(d_ms, Ordering::SeqCst);
                        }

                        // Seek support: the orchestrator implements seek by
                        // recreating the stream with a start position (it
                        // never sends an in-stream Seek command). Position
                        // the DSF reader here, otherwise every seek restarts
                        // playback at 0:00 while the UI shows the seek point.
                        let mut initial_bits: u64 = 0;
                        if start_position_ms > 0 {
                            let target_bpc = (start_position_ms as f64 * cur_sample_rate as f64
                                / 1000.0
                                / 8.0) as usize;
                            match tokio::task::block_in_place(|| {
                                reader.seek_to_bytes_per_channel(target_bpc)
                            }) {
                                Ok(reached_bpc) => {
                                    initial_bits = reached_bpc as u64 * 8;
                                    info!(
                                        device = %device_name,
                                        start_position_ms,
                                        reached_bpc,
                                        "oaat: native DSD starting at seek position"
                                    );
                                }
                                Err(e) => {
                                    warn!(device = %device_name, error = %e, "oaat: native DSD start seek failed");
                                }
                            }
                        }

                        let mut stream_start_ns = super::helpers::now_ns() + 500_000_000;
                        let mut start = std::time::Instant::now()
                            - std::time::Duration::from_nanos(
                                (initial_bits as f64 / cur_sample_rate as f64 * 1e9) as u64,
                            );
                        let mut pause_offset = std::time::Duration::ZERO;
                        let mut sample_offset: u64 = initial_bits;
                        // PTS must be relative to the first packet actually
                        // sent: the endpoint only schedules playback when the
                        // first PTS is < 5 s ahead of its clock, so encoding
                        // the absolute seek offset into the PTS would break
                        // scheduling and confuse the drift servo.
                        let mut pts_bits_base: u64 = initial_bits;
                        if initial_bits > 0 {
                            position_ms.store(
                                initial_bits * 1000 / cur_sample_rate.max(1) as u64,
                                Ordering::SeqCst,
                            );
                        }
                        let mut pending: Vec<u8> = Vec::new();
                        let mut first = true;
                        let mut eof = false;
                        let mut stopped = false;
                        let ch_usize = ch.max(1) as usize;
                        // Prepared-next for gapless: the next track's `.dsf` is
                        // opened during the current track (on PrepareNext) and
                        // swapped in at EOF with no teardown.
                        let mut next_dsd: Option<PreparedDsdNext> = None;

                        'track: loop {
                            while playing.load(Ordering::Relaxed) && !stopped {
                                if stop_rx.try_recv().is_ok() {
                                    stopped = true;
                                    break;
                                }
                                while let Ok(cmd) = command_rx.try_recv() {
                                    match cmd {
                                        OaatCommand::SetVolume(level) => {
                                            endpoint.send_volume(level).await.ok();
                                        }
                                        OaatCommand::Mute(muted) => {
                                            endpoint.send_mute(muted).await.ok();
                                        }
                                        OaatCommand::Pause => {
                                            paused.store(true, Ordering::SeqCst);
                                            pause_offset = start.elapsed();
                                            endpoint
                                                .send_message(&oaat_core::Message::Pause(
                                                    oaat_core::message::Pause {
                                                        stream_id: stream_id.clone(),
                                                    },
                                                ))
                                                .await
                                                .ok();
                                            info!(device = %device_name, "oaat: DSD paused");
                                        }
                                        OaatCommand::Resume => {
                                            paused.store(false, Ordering::SeqCst);
                                            start = std::time::Instant::now() - pause_offset;
                                            endpoint.send_play(&stream_id).await.ok();
                                            info!(device = %device_name, "oaat: DSD resumed");
                                        }
                                        OaatCommand::Seek {
                                            position_ms: seek_pos,
                                        } => {
                                            endpoint
                                                .send_message(&oaat_core::Message::Seek(
                                                    oaat_core::message::Seek {
                                                        stream_id: stream_id.clone(),
                                                        position_ms: seek_pos,
                                                    },
                                                ))
                                                .await
                                                .ok();
                                            // Bytes per channel at seek point (8 DSD bits / byte).
                                            let target_bpc = (seek_pos as f64
                                                * cur_sample_rate as f64
                                                / 1000.0
                                                / 8.0)
                                                as usize;
                                            match tokio::task::block_in_place(|| {
                                                reader.seek_to_bytes_per_channel(target_bpc)
                                            }) {
                                                Ok(reached_bpc) => {
                                                    pending.clear();
                                                    eof = false;
                                                    sample_offset = reached_bpc as u64 * 8;
                                                    let elapsed_eq =
                                                        std::time::Duration::from_millis(seek_pos);
                                                    start = std::time::Instant::now() - elapsed_eq;
                                                    pause_offset = std::time::Duration::ZERO;
                                                    position_ms.store(seek_pos, Ordering::SeqCst);
                                                    // Re-anchor PTS at the seek point:
                                                    // the endpoint disarmed its servo on
                                                    // the Seek message, so post-seek
                                                    // packets restart a fresh timeline.
                                                    pts_bits_base = sample_offset;
                                                    stream_start_ns =
                                                        super::helpers::now_ns() + 200_000_000;
                                                    info!(
                                                        device = %device_name,
                                                        seek_pos,
                                                        reached_bpc,
                                                        "oaat: DSD seek complete"
                                                    );
                                                }
                                                Err(e) => {
                                                    warn!(device = %device_name, error = %e, "oaat: DSD seek failed");
                                                }
                                            }
                                        }
                                        OaatCommand::PrepareNext {
                                            file_path,
                                            title,
                                            artist,
                                            album,
                                            cover_url,
                                            duration_ms,
                                            ..
                                        } => {
                                            // Pre-open the next `.dsf` for a gapless
                                            // swap at EOF — only if it is a local
                                            // file that is format-compatible with the
                                            // current DSD stream (same rate/channels).
                                            // Otherwise leave next_dsd None so the
                                            // track ends cleanly and the poller
                                            // advances (small gap across the boundary).
                                            if next_dsd.is_none() {
                                                if let Some(fp) = file_path {
                                                    next_dsd = tokio::task::block_in_place(|| {
                                                        open_next_dsd(
                                                            &fp,
                                                            cur_sample_rate,
                                                            ch,
                                                            title,
                                                            artist,
                                                            album,
                                                            cover_url,
                                                            duration_ms,
                                                        )
                                                    });
                                                    match next_dsd {
                                                        Some(_) => info!(
                                                            device = %device_name,
                                                            "oaat: native DSD next track prepared (gapless ready)"
                                                        ),
                                                        None => info!(
                                                            device = %device_name,
                                                            "oaat: native DSD next not gapless-compatible, will advance at end"
                                                        ),
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }

                                while paused.load(Ordering::Relaxed)
                                    && playing.load(Ordering::Relaxed)
                                {
                                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                    if stop_rx.try_recv().is_ok() {
                                        stopped = true;
                                        break;
                                    }
                                    // Drain commands so Resume/Seek/Stop arrive while paused.
                                    while let Ok(cmd) = command_rx.try_recv() {
                                        match cmd {
                                            OaatCommand::Resume => {
                                                paused.store(false, Ordering::SeqCst);
                                                start = std::time::Instant::now() - pause_offset;
                                                endpoint.send_play(&stream_id).await.ok();
                                                info!(device = %device_name, "oaat: DSD resumed");
                                            }
                                            OaatCommand::Pause => {}
                                            OaatCommand::SetVolume(level) => {
                                                endpoint.send_volume(level).await.ok();
                                            }
                                            OaatCommand::Mute(muted) => {
                                                endpoint.send_mute(muted).await.ok();
                                            }
                                            OaatCommand::Seek {
                                                position_ms: seek_pos,
                                            } => {
                                                endpoint
                                                    .send_message(&oaat_core::Message::Seek(
                                                        oaat_core::message::Seek {
                                                            stream_id: stream_id.clone(),
                                                            position_ms: seek_pos,
                                                        },
                                                    ))
                                                    .await
                                                    .ok();
                                                let target_bpc = (seek_pos as f64
                                                    * cur_sample_rate as f64
                                                    / 1000.0
                                                    / 8.0)
                                                    as usize;
                                                if let Ok(reached_bpc) =
                                                    tokio::task::block_in_place(|| {
                                                        reader.seek_to_bytes_per_channel(target_bpc)
                                                    })
                                                {
                                                    pending.clear();
                                                    eof = false;
                                                    sample_offset = reached_bpc as u64 * 8;
                                                    start = std::time::Instant::now()
                                                        - std::time::Duration::from_millis(
                                                            seek_pos,
                                                        );
                                                    pause_offset = std::time::Duration::ZERO;
                                                    position_ms.store(seek_pos, Ordering::SeqCst);
                                                    pts_bits_base = sample_offset;
                                                    stream_start_ns =
                                                        super::helpers::now_ns() + 200_000_000;
                                                }
                                            }
                                            OaatCommand::PrepareNext { .. } => {}
                                        }
                                    }
                                }
                                if stopped || !playing.load(Ordering::Relaxed) {
                                    break;
                                }

                                while pending.len() < DSD_CHUNK_SIZE && !eof {
                                    let chunk = match tokio::task::block_in_place(|| {
                                        reader.next_chunk()
                                    }) {
                                        Ok(Some(c)) => c,
                                        Ok(None) => {
                                            eof = true;
                                            break;
                                        }
                                        Err(e) => {
                                            warn!(device = %device_name, error = %e, "oaat: DSD read error");
                                            eof = true;
                                            break;
                                        }
                                    };
                                    pending.extend_from_slice(&chunk);
                                }

                                if pending.is_empty() {
                                    break;
                                }

                                let mut take = DSD_CHUNK_SIZE.min(pending.len());
                                take -= take % ch_usize;
                                if take == 0 {
                                    break;
                                }
                                let payload: Vec<u8> = pending.drain(..take).collect();
                                let bits_this = (take / ch_usize) * 8;
                                let pts_ns = stream_start_ns
                                    + (sample_offset.saturating_sub(pts_bits_base) as f64
                                        / cur_sample_rate as f64
                                        * 1e9) as u64;
                                let flags = if first {
                                    first = false;
                                    PacketFlags::FIRST_PACKET
                                } else {
                                    PacketFlags::empty()
                                };

                                if endpoint
                                    .send_audio(
                                        stream_num,
                                        cur_format,
                                        pts_ns,
                                        sample_offset,
                                        &payload,
                                        flags,
                                    )
                                    .await
                                    .is_err()
                                {
                                    break;
                                }

                                sample_offset += bits_this as u64;
                                diag.packets_sent.fetch_add(1, Ordering::Relaxed);
                                diag.bytes_sent
                                    .fetch_add(payload.len() as u64, Ordering::Relaxed);
                                // Feed the stall watchdog: the supervisor restarts
                                // the zone when last_packet_age_ms > 30 s, and this
                                // field otherwise keeps the epoch of the previous
                                // (HTTP) playback.
                                diag.last_packet_epoch_ms.store(
                                    std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_millis() as u64,
                                    Ordering::Relaxed,
                                );
                                position_ms.store(
                                    sample_offset * 1000 / cur_sample_rate.max(1) as u64,
                                    Ordering::SeqCst,
                                );

                                let expected = std::time::Duration::from_nanos(
                                    (sample_offset as f64 / cur_sample_rate as f64 * 1e9) as u64,
                                );
                                let elapsed = start.elapsed();
                                if expected > elapsed {
                                    tokio::time::sleep(expected - elapsed).await;
                                }

                                if eof && pending.is_empty() {
                                    break;
                                }
                            }

                            // The current track's packet loop ended. If it ended
                            // naturally (EOF, not a user stop / seek-recreate / send
                            // failure) and a compatible next `.dsf` is staged, swap
                            // readers and keep the SAME OAAT stream alive: send
                            // LAST_PACKET to close the track, then continue streaming
                            // the next track (FIRST_PACKET, offsets reset). The poller
                            // sees the position reset to 0 while still Playing and
                            // advances the queue metadata (gapless_position_reset), the
                            // same mechanism the PCM/HTTP path relies on. No compatible
                            // next → fall through to a clean stop and let the poller's
                            // natural-end fallback advance.
                            if !stopped && playing.load(Ordering::Relaxed) && eof {
                                if let Some(next) = next_dsd.take() {
                                    endpoint
                                        .send_audio(
                                            stream_num,
                                            cur_format,
                                            0,
                                            sample_offset,
                                            &[],
                                            PacketFlags::LAST_PACKET,
                                        )
                                        .await
                                        .ok();

                                    info!(
                                        device = %device_name,
                                        title = %next.title,
                                        "oaat: gapless transition (native DSD)"
                                    );

                                    reader = next.reader;
                                    sample_offset = 0;
                                    pts_bits_base = 0;
                                    pending.clear();
                                    first = true;
                                    eof = false;
                                    pause_offset = std::time::Duration::ZERO;
                                    stream_start_ns = super::helpers::now_ns() + 500_000_000;
                                    start = std::time::Instant::now();
                                    position_ms.store(0, Ordering::SeqCst);
                                    duration_ms_arc.store(next.duration_ms, Ordering::SeqCst);

                                    *current_title.lock().await = Some(next.title.clone());
                                    *current_artist.lock().await = Some(next.artist.clone());
                                    *current_uri.lock().await = Some(String::new());

                                    let fmt_str =
                                        format_rate_display(cur_sample_rate, 1, cur_format);
                                    endpoint
                                        .send_metadata(oaat_core::message::TrackMetadata {
                                            title: next.title,
                                            artist: next.artist,
                                            album: next.album,
                                            duration_ms: next.duration_ms,
                                            artwork_url: next.cover_url,
                                            format: Some(fmt_str),
                                        })
                                        .await
                                        .ok();

                                    continue 'track;
                                }
                            }
                            break 'track;
                        } // end 'track loop

                        endpoint
                            .send_audio(
                                stream_num,
                                cur_format,
                                0,
                                sample_offset,
                                &[],
                                PacketFlags::LAST_PACKET,
                            )
                            .await
                            .ok();
                        endpoint.send_stop(&stream_id).await.ok();
                        playing.store(false, Ordering::SeqCst);
                        diag.connected.store(false, Ordering::SeqCst);
                        info!(
                            device = %device_name,
                            bits = sample_offset,
                            "oaat: native DSD playback complete"
                        );
                        break 'direct true;
                    }

                    // Identifier le format AVANT de tout lire.
                    //
                    // `tokio::fs::read` chargeait le fichier ENTIER en mémoire,
                    // puis regardait ce que c'était, puis le jetait si ce
                    // n'était pas du WAV — c'est-à-dire presque toujours : le
                    // FLAC part en HTTP (voir plus bas), le DSD aussi, les
                    // formats compressés aussi.
                    //
                    // Mesuré sur .42 avec un DSD128 de 868 Mo : QUINZE SECONDES
                    // de silence entre `play_media` et la bascule sur le flux
                    // HTTP, et un pic mémoire de presque un gigaoctet sur un
                    // mini PC. L'utilisateur monte le volume, n'entend rien, et
                    // met en pause avant que la conversion ne démarre.
                    //
                    // `detect_and_parse` n'a besoin que de 92 octets pour
                    // reconnaître RIFF/WAVE, fLaC ou DSD. On lit donc un
                    // en-tête, et on ne charge la suite que si ça vaut la peine.
                    let taille_entete = super::helpers::ENTETE_DETECTION;
                    let entete = {
                        use tokio::io::AsyncReadExt;
                        match tokio::fs::File::open(fp).await {
                            Ok(mut f) => {
                                let mut e = vec![0u8; taille_entete];
                                match f.read(&mut e).await {
                                    Ok(n) => {
                                        e.truncate(n);
                                        e
                                    }
                                    Err(err) => {
                                        debug!("header read failed, falling back to HTTP: {err}");
                                        break 'direct false;
                                    }
                                }
                            }
                            Err(e) => {
                                debug!("file open failed, falling back to HTTP: {e}");
                                break 'direct false;
                            }
                        }
                    };

                    // Seul le WAV est réellement jouable par ce chemin. Tout le
                    // reste finit sur le flux HTTP que l'orchestrateur sert
                    // déjà — autant s'en apercevoir maintenant.
                    if !super::helpers::entete_est_wav(&entete) {
                        debug!(
                            "format non lisible en direct (en-tete: {:?}), bascule immediate sur le flux HTTP",
                            String::from_utf8_lossy(&entete[..entete.len().min(4)])
                        );
                        break 'direct false;
                    }

                    let file_data = match tokio::fs::read(fp).await {
                        Ok(d) => d,
                        Err(e) => {
                            debug!("file read failed, falling back to HTTP: {e}");
                            break 'direct false;
                        }
                    };

                    let mut buf = file_data;
                    let si = match detect_and_parse(&mut buf) {
                        Some(info) => info,
                        None => {
                            debug!("format not natively supported, falling back to HTTP stream");
                            break 'direct false;
                        }
                    };

                    debug!(
                        "OAAT-DEBUG: file format {:?} {}Hz {}bit {}ch, {} bytes",
                        si.format,
                        si.sample_rate,
                        si.bits_per_sample,
                        si.channels,
                        buf.len()
                    );

                    let is_flac = si.format == AudioFormat::Flac;
                    let is_dsd = si.format.is_dsd();

                    // DSD needs conversion to PCM — let the orchestrator handle
                    // it via HTTP streaming rather than sending raw DSD bits
                    // (endpoint has no DSD capability, or file is not .dsf).
                    if is_dsd {
                        debug!("DSD file, falling back to HTTP stream for PCM conversion");
                        break 'direct false;
                    }

                    // Le FLAC ne se lit pas en direct : il n'y a pas de chemin
                    // pour ça ici. L'orchestrateur transcode déjà le FLAC en
                    // WAV pour OAAT (`oaat_needs_wav`), en natif, et le sert
                    // sur la session HTTP dont l'endpoint a reçu l'URL juste
                    // avant. On s'y replie, comme les six autres sorties de ce
                    // bloc.
                    //
                    // Cette branche appelait un `ffmpeg` externe que Tune ne
                    // livre plus. Sur une machine qui n'en a pas, l'appel
                    // échouait en une milliseconde et la sortie abandonnait
                    // par un `return` — sans repli, alors que le flux HTTP
                    // était prêt. Côté utilisateur : `state=playing`, `pos=0`
                    // figé, aucun son, aucune erreur.
                    if is_flac {
                        debug!("FLAC: falling back to HTTP stream (native transcode)");
                        break 'direct false;
                    }

                    let (source_pcm, source_format, source_sample_rate, source_bits, source_ch) = (
                        buf,
                        si.format,
                        si.sample_rate,
                        si.bits_per_sample,
                        si.channels.min(8) as u8,
                    );
                    let source_layout = match disposition_oaat(source_ch) {
                        Ok(layout) => layout,
                        Err(raison) => {
                            error!(device = %device_name, %raison, "oaat: disposition PCM source invalide");
                            playing.store(false, Ordering::SeqCst);
                            return;
                        }
                    };
                    if let Err(e) = endpoint
                        .propose_format(
                            &stream_id,
                            source_format,
                            source_sample_rate,
                            source_ch,
                            source_layout,
                            source_bits as u8,
                        )
                        .await
                    {
                        error!(device = %device_name, error = %e, "oaat: format propose failed");
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }

                    // Lire la reponse AVANT de jouer (#2282) : ce chemin
                    // proposait puis lancait la lecture sans jamais la
                    // consulter.
                    let contrat = ContratPropose {
                        stream_id: stream_id.clone(),
                        format: source_format,
                        sample_rate: source_sample_rate,
                        channels: source_ch,
                        channel_layout: source_layout,
                        bits_per_sample: source_bits,
                        // `propose_format` n'envoie pas de `dsd_rate`.
                        dsd_rate: None,
                    };
                    let contrat_negocie = match attendre_accord_format(
                        &mut endpoint,
                        &device_name,
                        &contrat,
                        PolitiqueAdaptation::PcmEntier,
                        std::time::Duration::from_secs(5),
                    )
                    .await
                    {
                        Ok(contrat) => contrat,
                        Err(refus) => {
                            error!(device = %device_name, raison = %refus.raison, "oaat: format non accepte, on ne joue pas");
                            signaler_refus_negociation(&refus_negociation, &refus);
                            playing.store(false, Ordering::SeqCst);
                            return;
                        }
                    };

                    let mut adaptateur = match construire_adaptateur_pcm(&contrat, &contrat_negocie)
                    {
                        Ok(adaptateur) => adaptateur,
                        Err(raison) => {
                            let refus = RefusNegociation {
                                stream_id: stream_id.clone(),
                                raison: format!("adaptation PCM négociée impossible : {raison}"),
                                reconnectable: false,
                            };
                            signaler_refus_negociation(&refus_negociation, &refus);
                            playing.store(false, Ordering::SeqCst);
                            return;
                        }
                    };
                    let mut pcm_data = match adaptateur.push(&source_pcm) {
                        Ok(pcm) => pcm,
                        Err(raison) => {
                            error!(device = %device_name, %raison, "oaat: adaptation PCM directe échouée");
                            playing.store(false, Ordering::SeqCst);
                            return;
                        }
                    };
                    match adaptateur.finish() {
                        Ok(fin) => pcm_data.extend(fin),
                        Err(raison) => {
                            error!(device = %device_name, %raison, "oaat: fin adaptation PCM directe invalide");
                            playing.store(false, Ordering::SeqCst);
                            return;
                        }
                    }

                    let cur_format = contrat_negocie.format;
                    let cur_sample_rate = contrat_negocie.sample_rate;
                    let cur_bits = contrat_negocie.bits_per_sample;
                    let ch = contrat_negocie.channels;
                    let bytes_per_frame = (cur_bits as usize / 8) * ch as usize;
                    let packet_size = PCM_SAMPLES_PER_PACKET * bytes_per_frame;
                    if packet_size == 0 || bytes_per_frame == 0 {
                        error!(device = %device_name, "oaat: zero packet size, cannot stream");
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                    let fmt_str = format_rate_display(cur_sample_rate, cur_bits, cur_format);

                    endpoint
                        .send_metadata(oaat_core::message::TrackMetadata {
                            title: title.clone(),
                            artist: artist.clone(),
                            album: album.clone(),
                            duration_ms: track_duration_ms,
                            artwork_url: cover_url.clone(),
                            format: Some(fmt_str),
                        })
                        .await
                        .ok();

                    if let Err(e) = endpoint.send_play(&stream_id).await {
                        error!(device = %device_name, error = %e, "oaat: play failed");
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }

                    diag.connected.store(true, Ordering::SeqCst);
                    // Direct PCM playback chains internally: the next local file
                    // is staged while this one plays, and swapped in at EOF.
                    // `direct_pcm_active` makes the poller stage a local FILE
                    // (a transcode URL would be useless here).
                    direct_pcm_active.store(true, Ordering::SeqCst);
                    info!(device = %device_name, bytes = pcm_data.len(), "oaat: direct file playback");

                    let mut offset = 0usize;
                    let mut sample_offset: u64 = 0;
                    // Absolute PTS anchor: frame 0 presents at now + lead (RFC 6.4).
                    let mut stream_start_ns = super::helpers::now_ns() + 500_000_000;
                    let mut start = std::time::Instant::now();
                    let mut staged_next: Option<super::helpers::StagedDirectTrack> = None;
                    let mut staged_rx: Option<
                        tokio::sync::oneshot::Receiver<Option<super::helpers::StagedDirectTrack>>,
                    > = None;

                    'direct_tracks: loop {
                        if !playing.load(Ordering::Relaxed) {
                            break 'direct_tracks;
                        }

                        // End of the current track. If a next one is staged in a
                        // matching format, close the track with LAST_PACKET and
                        // swap buffers WITHOUT tearing the session down — the
                        // same move the native DSD path makes. The poller sees
                        // the position reset to 0 while still Playing and
                        // advances the queue metadata.
                        //
                        // Before this, the direct path always stopped here: the
                        // poller then had to notice the end, stop the zone,
                        // reconnect, resync the clock and re-convert the next
                        // file — six to seven seconds of silence between two
                        // tracks of the same album (Xavier, 8 Aug 2026).
                        if offset >= pcm_data.len() {
                            // A user stop breaks out below without ever
                            // reaching here, so anything staged at this point
                            // belongs to a track that ended on its own.
                            let next = staged_next.take().filter(|n| {
                                super::helpers::staged_track_matches(
                                    n,
                                    cur_format,
                                    cur_sample_rate,
                                    cur_bits,
                                    ch,
                                )
                            });
                            match next {
                                Some(next) => {
                                    endpoint
                                        .send_audio(
                                            stream_num,
                                            cur_format,
                                            0,
                                            sample_offset,
                                            &[],
                                            PacketFlags::LAST_PACKET,
                                        )
                                        .await
                                        .ok();

                                    info!(
                                        device = %device_name,
                                        title = %next.title,
                                        "oaat: gapless transition (direct file)"
                                    );

                                    endpoint
                                        .send_metadata(oaat_core::message::TrackMetadata {
                                            title: next.title.clone(),
                                            artist: next.artist.clone(),
                                            album: next.album.clone(),
                                            duration_ms: next.duration_ms,
                                            artwork_url: next.cover_url.clone(),
                                            format: Some(format_rate_display(
                                                cur_sample_rate,
                                                cur_bits,
                                                cur_format,
                                            )),
                                        })
                                        .await
                                        .ok();

                                    pcm_data = next.pcm;
                                    offset = 0;
                                    sample_offset = 0;
                                    stream_start_ns = super::helpers::now_ns() + 500_000_000;
                                    start = std::time::Instant::now();
                                    position_ms.store(0, Ordering::SeqCst);
                                    duration_ms_arc.store(next.duration_ms, Ordering::SeqCst);
                                    *current_title.lock().await = Some(next.title.clone());
                                    *current_artist.lock().await = Some(next.artist.clone());
                                    continue 'direct_tracks;
                                }
                                None => {
                                    // Nothing to chain into. Tell the poller so it
                                    // advances on its next tick (it re-reads
                                    // supports_internal_gapless while waiting)
                                    // instead of sitting out its guard.
                                    chain_exhausted.store(true, Ordering::SeqCst);
                                    break 'direct_tracks;
                                }
                            }
                        }

                        if stop_rx.try_recv().is_ok() {
                            chain_exhausted.store(true, Ordering::SeqCst);
                            break 'direct_tracks;
                        }
                        // Unlike the HTTP-stream path (which polls command_rx in a
                        // select!), this direct-file loop only reacts to stop/pause
                        // via atomics. Drain command_rx here so live SetVolume/Mute
                        // (which reach the endpoint only via send_volume/send_mute)
                        // are actually forwarded — otherwise mid-track volume changes
                        // have no audible effect on OAAT zones.
                        while let Ok(cmd) = command_rx.try_recv() {
                            match cmd {
                                OaatCommand::SetVolume(level) => {
                                    endpoint.send_volume(level).await.ok();
                                }
                                OaatCommand::Mute(muted) => {
                                    endpoint.send_mute(muted).await.ok();
                                }
                                OaatCommand::Pause => paused.store(true, Ordering::SeqCst),
                                OaatCommand::Resume => paused.store(false, Ordering::SeqCst),
                                // Stage the next track so we can chain into it
                                // at EOF instead of tearing the session down.
                                // Decoding runs on the blocking pool: a 190 MB
                                // FLAC takes over a second through ffmpeg, which
                                // inline would be an audible dropout.
                                OaatCommand::PrepareNext {
                                    title,
                                    artist,
                                    album,
                                    cover_url,
                                    duration_ms,
                                    file_path: Some(next_path),
                                    ..
                                } => {
                                    if staged_rx.is_none() && staged_next.is_none() {
                                        let (tx, rx) = tokio::sync::oneshot::channel();
                                        staged_rx = Some(rx);
                                        let dev = device_name.clone();
                                        let cible = contrat_negocie.clone();
                                        tokio::task::spawn_blocking(move || {
                                            let staged = super::helpers::stage_direct_track(
                                                &next_path,
                                                title,
                                                artist,
                                                album,
                                                cover_url,
                                                duration_ms,
                                            )
                                            .and_then(|piste| {
                                                match adapter_piste_directe_gapless(piste, &cible) {
                                                    Ok(piste) => Some(piste),
                                                    Err(raison) => {
                                                        debug!(device = %dev, path = %next_path, %raison, "oaat: direct next track cannot satisfy negotiated format");
                                                        None
                                                    }
                                                }
                                            });
                                            if staged.is_none() {
                                                debug!(device = %dev, path = %next_path, "oaat: direct next track not stageable");
                                            }
                                            let _ = tx.send(staged);
                                        });
                                    }
                                }
                                // Seek and a next track without a local path are
                                // not handled on the direct path.
                                _ => {}
                            }
                        }

                        // Collect the staged track without ever blocking the
                        // packet cadence.
                        if let Some(rx) = staged_rx.as_mut() {
                            match rx.try_recv() {
                                Ok(res) => {
                                    staged_rx = None;
                                    staged_next = res;
                                }
                                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {}
                                Err(_) => staged_rx = None,
                            }
                        }
                        while paused.load(Ordering::Relaxed) {
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            if stop_rx.try_recv().is_ok() {
                                break;
                            }
                        }

                        let chunk_bytes = packet_size.min(pcm_data.len() - offset);
                        let chunk_samples = chunk_bytes / bytes_per_frame;
                        let payload = &pcm_data[offset..offset + chunk_bytes];
                        let pts_ns = stream_start_ns
                            + (sample_offset as f64 / cur_sample_rate as f64 * 1e9) as u64;
                        let flags = if offset == 0 {
                            PacketFlags::FIRST_PACKET
                        } else {
                            PacketFlags::empty()
                        };

                        if endpoint
                            .send_audio(
                                stream_num,
                                cur_format,
                                pts_ns,
                                sample_offset,
                                payload,
                                flags,
                            )
                            .await
                            .is_err()
                        {
                            break;
                        }

                        offset += chunk_bytes;
                        sample_offset += chunk_samples as u64;
                        diag.packets_sent.fetch_add(1, Ordering::Relaxed);
                        diag.bytes_sent
                            .fetch_add(chunk_bytes as u64, Ordering::Relaxed);
                        // Feed the stall watchdog (see native DSD path above).
                        diag.last_packet_epoch_ms.store(
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as u64,
                            Ordering::Relaxed,
                        );
                        position_ms.store(
                            sample_offset * 1000 / cur_sample_rate as u64,
                            Ordering::Relaxed,
                        );

                        let expected = std::time::Duration::from_nanos(
                            (sample_offset as f64 / cur_sample_rate as f64 * 1e9) as u64,
                        );
                        let elapsed = start.elapsed();
                        if expected > elapsed {
                            tokio::time::sleep(expected - elapsed).await;
                        }
                    }

                    endpoint
                        .send_audio(
                            stream_num,
                            cur_format,
                            0,
                            sample_offset,
                            &[],
                            PacketFlags::LAST_PACKET,
                        )
                        .await
                        .ok();
                    endpoint.send_stop(&stream_id).await.ok();
                    playing.store(false, Ordering::SeqCst);
                    diag.connected.store(false, Ordering::SeqCst);
                    info!(
                        device = %device_name,
                        samples = sample_offset,
                        "oaat: direct file playback complete"
                    );
                    true
                };
                if direct_ok {
                    return;
                }
                debug!(device = %device_name, "oaat: falling back to HTTP stream for {url}");
            }

            debug!(device = %device_name, url = %url, "oaat: fetching audio stream");
            let resp = match http_client.get(&url).send().await {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => {
                    error!(device = %device_name, status = %r.status(), url = %url, "oaat: HTTP error");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
                Err(e) => {
                    error!(device = %device_name, error = %e, url = %url, "oaat: fetch failed");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
            };

            let mut stream: futures_util::stream::BoxStream<
                '_,
                Result<bytes::Bytes, reqwest::Error>,
            > = Box::pin(resp.bytes_stream());
            let mut buf = Vec::new();

            while buf.len() < 128 {
                match stream.next().await {
                    Some(Ok(chunk)) => buf.extend_from_slice(&chunk),
                    _ => {
                        error!(device = %device_name, "oaat: stream ended before header");
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            }

            // Detect WAV or FLAC
            let si = match detect_and_parse(&mut buf) {
                Some(info) => info,
                None => {
                    let sig: Vec<u8> = buf.iter().take(12).copied().collect();
                    error!(device = %device_name, signature = %format!("{sig:02x?}"), "oaat: unsupported stream format");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
            };

            let source_is_flac = si.format == AudioFormat::Flac;

            // Un flux FLAC ne peut pas partir tel quel : le decoupage UDP
            // corrompt les frontieres de trame. Ce cas ne doit plus se
            // presenter — l'orchestrateur transcode le FLAC en WAV pour OAAT
            // (`oaat_needs_wav`) avant meme de creer la session — et le
            // decodage de secours passait par un `ffmpeg` externe que Tune ne
            // livre plus, donc il echouait de toute facon.
            //
            // On refuse explicitement plutot que d'emettre un flux qu'on sait
            // corrompu : une erreur nommee vaut mieux qu'un grésillement dont
            // personne ne trouvera la cause.
            if source_is_flac {
                error!(
                    device = %device_name,
                    "oaat: flux FLAC inattendu sur le chemin HTTP, lecture abandonnee"
                );
                playing.store(false, Ordering::SeqCst);
                return;
            }
            if si.format.is_dsd() {
                error!(device = %device_name, "oaat: flux DSD inattendu sur le chemin HTTP PCM");
                playing.store(false, Ordering::SeqCst);
                return;
            }

            let mut source_format = si.format;
            let mut source_sample_rate = si.sample_rate;
            let mut source_bits = si.bits_per_sample;
            let mut source_channels = si.channels.min(8) as u8;
            let mut source_layout = match disposition_oaat(source_channels) {
                Ok(layout) => layout,
                Err(raison) => {
                    error!(device = %device_name, %raison, "oaat: disposition source invalide");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
            };
            let mut source_data_offset = si.data_offset;

            let track_duration_ms = if track_duration_ms > 0 {
                track_duration_ms
            } else {
                si.duration_ms
            };
            duration_ms_arc.store(track_duration_ms, Ordering::SeqCst);

            info!(
                device = %device_name,
                sample_rate = source_sample_rate, bits = source_bits, channels = source_channels,
                format = %source_format,
                "oaat: audio format detected"
            );

            // Format negotiation (use send_message directly to include dsd_rate)
            if let Err(e) = endpoint
                .send_message(&oaat_core::Message::FormatPropose(
                    oaat_core::message::FormatPropose {
                        stream_id: stream_id.clone(),
                        format: source_format,
                        sample_rate: source_sample_rate,
                        channels: source_channels,
                        channel_layout: source_layout,
                        bits_per_sample: source_bits as u8,
                        dsd_rate: si.dsd_rate,
                    },
                ))
                .await
            {
                error!(device = %device_name, error = %e, "oaat: format propose failed");
                playing.store(false, Ordering::SeqCst);
                return;
            }

            let contrat = ContratPropose {
                stream_id: stream_id.clone(),
                format: source_format,
                sample_rate: source_sample_rate,
                channels: source_channels,
                channel_layout: source_layout,
                bits_per_sample: source_bits,
                dsd_rate: si.dsd_rate,
            };
            let mut verdict = attendre_accord_format(
                &mut endpoint,
                &device_name,
                &contrat,
                PolitiqueAdaptation::PcmEntier,
                std::time::Duration::from_secs(5),
            )
            .await;

            // Une fermeture de socket pendant le handshake avait un repli
            // utile : reconnecter une fois. On le conserve, mais la seconde
            // réponse traverse exactement le même validateur que la première.
            // L'ancien repli acceptait n'importe quel FormatAccept, y compris
            // celui d'un autre flux, et perdait dsd_rate (#2283).
            if verdict.as_ref().is_err_and(|refus| refus.reconnectable) {
                warn!(device = %device_name, "oaat: endpoint closed during negotiation, reconnecting");
                tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                match ConnectedEndpoint::connect(&config, endpoint_addr).await {
                    Ok(ep) => {
                        endpoint = ep;
                        endpoint.clock_sync_bootstrap().await.ok();
                        if let Err(e) = endpoint
                            .send_message(&oaat_core::Message::FormatPropose(
                                oaat_core::message::FormatPropose {
                                    stream_id: contrat.stream_id.clone(),
                                    format: contrat.format,
                                    sample_rate: contrat.sample_rate,
                                    channels: contrat.channels,
                                    channel_layout: contrat.channel_layout,
                                    bits_per_sample: contrat.bits_per_sample as u8,
                                    dsd_rate: contrat.dsd_rate,
                                },
                            ))
                            .await
                        {
                            error!(device = %device_name, error = %e, "oaat: reconnect format propose failed");
                            playing.store(false, Ordering::SeqCst);
                            return;
                        }
                        verdict = attendre_accord_format(
                            &mut endpoint,
                            &device_name,
                            &contrat,
                            PolitiqueAdaptation::PcmEntier,
                            std::time::Duration::from_secs(5),
                        )
                        .await;
                    }
                    Err(e) => {
                        error!(device = %device_name, error = %e, "oaat: reconnect failed");
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            }

            let contrat_negocie = match verdict {
                Ok(contrat) => contrat,
                Err(refus) => {
                    signaler_refus_negociation(&refus_negociation, &refus);
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
            };

            let mut adaptateur_pcm = match construire_adaptateur_pcm(&contrat, &contrat_negocie) {
                Ok(adaptateur) => adaptateur,
                Err(raison) => {
                    let refus = RefusNegociation {
                        stream_id: stream_id.clone(),
                        raison: format!("adaptation PCM négociée impossible : {raison}"),
                        reconnectable: false,
                    };
                    signaler_refus_negociation(&refus_negociation, &refus);
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
            };
            buf = match adaptateur_pcm.push(&buf) {
                Ok(adaptee) => adaptee,
                Err(raison) => {
                    error!(device = %device_name, %raison, "oaat: premier bloc PCM invalide");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
            };

            let mut cur_format = contrat_negocie.format;
            let mut cur_sample_rate = contrat_negocie.sample_rate;
            let mut cur_bits = contrat_negocie.bits_per_sample;
            let mut cur_channels = contrat_negocie.channels;
            let mut ch = cur_channels;
            let mut layout = contrat_negocie.channel_layout;
            let mut bytes_per_frame = (cur_bits as usize / 8) * cur_channels as usize;
            let mut packet_size = PCM_SAMPLES_PER_PACKET * bytes_per_frame;
            let mut source_bytes_per_frame = (source_bits as usize / 8) * source_channels as usize;
            let mut is_flac = false;
            let mut is_dsd = false;
            let mut uses_byte_offset = false;

            // Metadata + Play
            let fmt_str = format_rate_display(cur_sample_rate, cur_bits, cur_format);
            endpoint
                .send_metadata(oaat_core::message::TrackMetadata {
                    title,
                    artist,
                    album,
                    duration_ms: track_duration_ms,
                    artwork_url: cover_url,
                    format: Some(fmt_str),
                })
                .await
                .ok();

            if let Err(e) = endpoint.send_play(&stream_id).await {
                error!(device = %device_name, error = %e, "oaat: send_play failed");
                playing.store(false, Ordering::SeqCst);
                return;
            }
            diag.connected.store(true, Ordering::SeqCst);
            diag.is_flac.store(is_flac, Ordering::SeqCst);
            diag.packets_sent.store(0, Ordering::SeqCst);
            diag.bytes_sent.store(0, Ordering::SeqCst);
            *diag.format_desc.lock().unwrap() =
                format_rate_display(cur_sample_rate, cur_bits, cur_format);
            info!(device = %device_name, "oaat: streaming started");

            // Build StreamInfo for reconnection
            let mut cur_stream_info = StreamInfo {
                sample_rate: cur_sample_rate,
                channels: cur_channels as u16,
                bits_per_sample: cur_bits,
                format: cur_format,
                duration_ms: track_duration_ms,
                dsd_rate: None,
                // Les Range portent sur le flux SOURCE, avant adaptation.
                data_offset: source_data_offset,
            };

            // Streaming loop
            let mut sample_offset: u64 = 0;
            // Absolute PTS anchor: frame 0 presents at now + lead (RFC 6.4).
            let mut stream_start_ns = super::helpers::now_ns() + 500_000_000;
            let mut byte_offset: u64 = 0;
            // Position VRAIE d'un flux FLAC, lue dans ses en-têtes de trames.
            // Le calcul par octets reste JUSTE pour le DSD (débit constant) —
            // il était faux pour le FLAC, à débit variable (#2214).
            let mut trames_flac = super::helpers::CompteurDeTramesFlac::new();
            let mut start = std::time::Instant::now();
            let mut pause_offset = std::time::Duration::ZERO;
            let mut reconnect_attempts: u32 = 0;
            // Mid-stream HTTP body-read errors (reqwest "error decoding response
            // body") on Tune's own /stream endpoint were treated as end-of-track:
            // the loop broke, reported "playback complete" a few seconds in, and
            // the zone then sat in stopped_early_waiting ~30s before
            // playback_failure_stopping_zone (Xavier / Zicmu, PCM HTTP fallback).
            // Instead, resume from the current byte offset via an HTTP Range
            // request, a bounded number of times, before giving up.
            let mut stream_retry_attempts: u32 = 0;

            let mut next_track: Option<NextTrackPrefetch> = None;
            let mut prefetch_rx: Option<tokio::sync::oneshot::Receiver<Option<NextTrackPrefetch>>> =
                None;

            let mut watchdog = tokio::time::interval(std::time::Duration::from_secs(10));
            watchdog.tick().await; // skip first immediate tick

            // An explicit stop is the ONE exit that must not raise
            // `chain_exhausted`: `stop()` is what clears the flag before the
            // next `play_media`, and this task's tail can still be running when
            // it does — raising it here would leak onto the track that follows
            // and disarm its gapless. Every other exit means the chain is over.
            let mut exited_on_stop = false;

            loop {
                tokio::select! {
                    // `biased` : l'ordre des bras est l'ordre de priorité. Sans
                    // lui, le tirage aléatoire peut servir le bras `stream` à
                    // chaque itération : un corps HTTP entièrement bufferisé ne
                    // laisse qu'une poignée d'itérations, et le résultat du
                    // préchargement reste dans `prefetch_rx` pendant que l'EOF
                    // est consommé — mesuré en direct (#1358) : résultat prêt
                    // 2 s avant l'EOF, jamais lu, transition sautée. Les bras
                    // commande/prefetch sont des événements rares et courts :
                    // les servir d'abord n'affame jamais le flux.
                    biased;

                    _ = &mut stop_rx => {
                        debug!(device = %device_name, "oaat: stop signal");
                        exited_on_stop = true;
                        break;
                    }

                    // Watchdog: detect stall (playing but no packets for 10s)
                    _ = watchdog.tick() => {
                        if playing.load(Ordering::Relaxed) && !paused.load(Ordering::Relaxed) {
                            let last = diag.last_packet_epoch_ms.load(Ordering::Relaxed);
                            let now = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as u64;
                            if last > 0 && now.saturating_sub(last) > 10_000 {
                                warn!(device = %device_name, stale_ms = now - last, "oaat: watchdog — stall detected, attempting reconnect");
                                diag.reconnects.fetch_add(1, Ordering::Relaxed);
                                match connect_and_setup(
                                    &config,
                                    endpoint_addr,
                                    &device_name,
                                    &stream_id,
                                    &cur_stream_info,
                                    &refus_negociation,
                                ).await {
                                    Some(new_ep) => {
                                        endpoint = new_ep;
                                        diag.connected.store(true, Ordering::SeqCst);
                                        info!(device = %device_name, "oaat: watchdog reconnected");
                                    }
                                    None => {
                                        warn!(device = %device_name, "oaat: watchdog reconnect failed");
                                    }
                                }
                            }
                        }
                    }

                    result = async {
                        match prefetch_rx.as_mut() {
                            Some(rx) => rx.await.ok(),
                            None => std::future::pending().await,
                        }
                    } => {
                        prefetch_rx = None;
                        if let Some(Some(prefetch)) = result {
                            next_track = settle_prefetch(
                                &mut endpoint, prefetch, &stream_id, &device_name,
                                cur_format, cur_sample_rate, ch, layout, cur_bits,
                            )
                            .await;
                        }
                    }

                    Some(cmd) = command_rx.recv() => {
                        match cmd {
                            OaatCommand::Pause => {
                                paused.store(true, Ordering::SeqCst);
                                pause_offset = start.elapsed();
                                endpoint.send_message(&oaat_core::Message::Pause(oaat_core::message::Pause {
                                    stream_id: stream_id.clone(),
                                })).await.ok();
                                info!(device = %device_name, "oaat: paused");
                            }
                            OaatCommand::Resume => {
                                paused.store(false, Ordering::SeqCst);
                                start = std::time::Instant::now() - pause_offset;
                                endpoint.send_play(&stream_id).await.ok();
                                info!(device = %device_name, "oaat: resumed");
                            }
                            OaatCommand::SetVolume(level) => { endpoint.send_volume(level).await.ok(); }
                            OaatCommand::Mute(muted) => { endpoint.send_mute(muted).await.ok(); }
                            OaatCommand::Seek { position_ms: seek_pos } => {
                                // Tell endpoint about seek
                                endpoint.send_message(&oaat_core::Message::Seek(oaat_core::message::Seek {
                                    stream_id: stream_id.clone(),
                                    position_ms: seek_pos,
                                })).await.ok();

                                {
                                    // Le Range vise les octets SOURCE. Utiliser
                                    // cadence/profondeur CIBLES après un
                                    // FormatCounter sauterait ou rejouerait une
                                    // autre portion du morceau.
                                    let bytes_per_sec = source_sample_rate as u64
                                        * source_bytes_per_frame as u64;
                                    let data_byte = seek_pos * bytes_per_sec / 1000;
                                    let frame_align = source_bytes_per_frame as u64;
                                    let aligned = (data_byte / frame_align) * frame_align;
                                    let file_offset = aligned + source_data_offset as u64;

                                    let range = format!("bytes={file_offset}-");
                                    match http_client.get(&url).header("Range", &range).send().await {
                                        Ok(resp) if resp.status().is_success() || resp.status().as_u16() == 206 => {
                                            let source = ContratPropose {
                                                stream_id: stream_id.clone(),
                                                format: source_format,
                                                sample_rate: source_sample_rate,
                                                channels: source_channels,
                                                channel_layout: source_layout,
                                                bits_per_sample: source_bits,
                                                dsd_rate: None,
                                            };
                                            let cible = ContratPropose {
                                                stream_id: stream_id.clone(),
                                                format: cur_format,
                                                sample_rate: cur_sample_rate,
                                                channels: cur_channels,
                                                channel_layout: layout,
                                                bits_per_sample: cur_bits,
                                                dsd_rate: None,
                                            };
                                            let Ok(nouvel_adaptateur) = construire_adaptateur_pcm(&source, &cible) else {
                                                error!(device = %device_name, "oaat: impossible de réinitialiser l adaptation PCM après seek");
                                                break;
                                            };
                                            adaptateur_pcm = nouvel_adaptateur;
                                            stream = Box::pin(resp.bytes_stream());
                                            buf.clear();
                                            byte_offset = 0;
                                            sample_offset = seek_pos
                                                .saturating_mul(cur_sample_rate as u64)
                                                / 1000;
                                            let elapsed_eq = std::time::Duration::from_millis(seek_pos);
                                            start = std::time::Instant::now() - elapsed_eq;
                                            pause_offset = std::time::Duration::ZERO;
                                            position_ms.store(seek_pos, Ordering::SeqCst);
                                            info!(device = %device_name, seek_pos, file_offset, "oaat: seek complete");
                                        }
                                        Ok(resp) => warn!(device = %device_name, status = %resp.status(), "oaat: seek Range failed"),
                                        Err(e) => warn!(device = %device_name, error = %e, "oaat: seek request failed"),
                                    }
                                }
                            }
                            OaatCommand::PrepareNext { url, title, artist, album, cover_url, duration_ms, file_path: _ } => {
                                let client = http_client.clone();
                                let dev = device_name.clone();
                                let cur_fmt = cur_format;
                                let cur_rate = cur_sample_rate;
                                let cur_bps = cur_bits;
                                let cur_ch = cur_channels;
                                let (tx, rx) = tokio::sync::oneshot::channel();
                                prefetch_rx = Some(rx);
                                tokio::spawn(async move {
                                    let _ = tx.send(prefetch_next_track(&client, &dev, &url, title, artist, album, cover_url, duration_ms, cur_fmt, cur_rate, cur_bps, cur_ch).await);
                                });
                            }
                        }
                    }

                    chunk = stream.next() => {
                        match chunk {
                            Some(Ok(data)) => {
                                // Fresh data flowing again: clear the per-track
                                // retry budget so isolated transient blips don't
                                // accumulate toward the cap over a long track.
                                stream_retry_attempts = 0;
                                match adaptateur_pcm.push(&data) {
                                    Ok(adaptee) => buf.extend(adaptee),
                                    Err(raison) => {
                                        let refus = RefusNegociation {
                                            stream_id: stream_id.clone(),
                                            raison: format!("payload PCM source invalide : {raison}"),
                                            reconnectable: false,
                                        };
                                        signaler_refus_negociation(&refus_negociation, &refus);
                                        error!(device = %device_name, %raison, "oaat: adaptation PCM interrompue");
                                        break;
                                    }
                                }
                            }
                            Some(Err(e)) => {
                                // First: is this the END of the track rather than
                                // a failure? A progressive WAV transcode declares
                                // a Content-Length predicted from the library
                                // duration, so the body always ends slightly
                                // short of it and reqwest reports a decode error
                                // instead of a clean EOF — exactly when the track
                                // finishes. Read as a mid-stream failure it sends
                                // us into a Range resume that cannot succeed, and
                                // we leave by a path that skips the gapless
                                // transition entirely (Xavier, 7 Aug 2026).
                                //
                                // Only on the PCM path: there `sample_offset`
                                // counts frames, so elapsed time is exact. FLAC
                                // and DSD track compressed BYTES, which carry no
                                // reliable mapping to milliseconds — they keep
                                // the resume path unchanged.
                                if !uses_byte_offset && cur_sample_rate > 0 && bytes_per_frame > 0 {
                                    let pending_frames = (buf.len() / bytes_per_frame) as u64;
                                    let received_ms = (sample_offset + pending_frames) * 1000
                                        / cur_sample_rate as u64;
                                    // The track duration comes from the LIBRARY
                                    // metadata, not from the stream header. A
                                    // progressive transcode writes its WAV header
                                    // before it knows the length, with the
                                    // 0x7FFF_FFFF sentinel — which reads back as a
                                    // 3.4-hour track, so comparing against it could
                                    // never match and this whole guard was inert
                                    // (Xavier Joly, 8 Aug 2026: the ALAC session
                                    // failed exactly as before the fix).
                                    // `duration_ms` falls back to the header only
                                    // when the metadata has nothing.
                                    // `parse_wav` already reports 0 for a
                                    // sentinel data size, so the header is a
                                    // safe fallback when metadata has nothing.
                                    let track_ms = duration_ms_arc.load(Ordering::Relaxed);
                                    let declared_ms = if track_ms > 0 {
                                        track_ms
                                    } else {
                                        cur_stream_info.duration_ms
                                    };
                                    if super::helpers::body_error_is_track_end(
                                        received_ms,
                                        declared_ms,
                                        END_OF_TRACK_TOLERANCE_MS,
                                    ) {
                                        info!(
                                            device = %device_name,
                                            received_ms,
                                            declared_ms,
                                            "oaat: body ended at track duration, treating as end of track"
                                        );
                                        // Hand the loop a finished stream so the
                                        // normal end-of-track arm runs unchanged:
                                        // flush the buffer, send LAST_PACKET, then
                                        // take the gapless transition.
                                        stream = Box::pin(futures_util::stream::empty());
                                        continue;
                                    }
                                }

                                // Genuine mid-stream body-read error. Rather than
                                // ending the track (and wedging the zone ~30s in
                                // stopped_early_waiting), resume the fetch from
                                // the byte we've streamed so far via HTTP Range.
                                if stream_retry_attempts >= MAX_STREAM_RETRY_ATTEMPTS {
                                    error!(device = %device_name, error = %e, attempts = stream_retry_attempts, "oaat: stream error, retries exhausted");
                                    break;
                                }
                                stream_retry_attempts += 1;
                                // Bytes already SENT define the resume point; drop
                                // any received-but-unsent bytes (they'll be re-
                                // fetched) so we neither duplicate nor lose audio.
                                let source_frames = ((sample_offset as u128
                                    * source_sample_rate as u128)
                                    / cur_sample_rate.max(1) as u128)
                                    as u64;
                                let data_bytes = source_frames
                                    .saturating_mul(source_bytes_per_frame as u64);
                                let file_offset = data_bytes + source_data_offset as u64;
                                let range = format!("bytes={file_offset}-");
                                warn!(device = %device_name, error = %e, attempt = stream_retry_attempts, file_offset, "oaat: stream error, resuming via Range");
                                // Brief backoff to let a transient upstream hiccup clear.
                                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                                match http_client.get(&url).header("Range", &range).send().await {
                                    Ok(resp) if resp.status().is_success() || resp.status().as_u16() == 206 => {
                                        let source = ContratPropose {
                                            stream_id: stream_id.clone(),
                                            format: source_format,
                                            sample_rate: source_sample_rate,
                                            channels: source_channels,
                                            channel_layout: source_layout,
                                            bits_per_sample: source_bits,
                                            dsd_rate: None,
                                        };
                                        let cible = ContratPropose {
                                            stream_id: stream_id.clone(),
                                            format: cur_format,
                                            sample_rate: cur_sample_rate,
                                            channels: cur_channels,
                                            channel_layout: layout,
                                            bits_per_sample: cur_bits,
                                            dsd_rate: None,
                                        };
                                        let Ok(nouvel_adaptateur) = construire_adaptateur_pcm(&source, &cible) else {
                                            error!(device = %device_name, "oaat: impossible de réinitialiser l adaptation PCM après reprise HTTP");
                                            break;
                                        };
                                        adaptateur_pcm = nouvel_adaptateur;
                                        stream = Box::pin(resp.bytes_stream());
                                        buf.clear();
                                        info!(device = %device_name, file_offset, "oaat: stream resumed after body error");
                                    }
                                    Ok(resp) => {
                                        error!(device = %device_name, status = %resp.status(), "oaat: stream resume Range rejected, ending track");
                                        break;
                                    }
                                    Err(re) => {
                                        error!(device = %device_name, error = %re, "oaat: stream resume request failed, ending track");
                                        break;
                                    }
                                }
                            }
                            None => {
                                match adaptateur_pcm.finish() {
                                    Ok(fin) => buf.extend(fin),
                                    Err(raison) => {
                                        let refus = RefusNegociation {
                                            stream_id: stream_id.clone(),
                                            raison: format!("fin de payload PCM invalide : {raison}"),
                                            reconnectable: false,
                                        };
                                        signaler_refus_negociation(&refus_negociation, &refus);
                                        error!(device = %device_name, %raison, "oaat: adaptation PCM incomplète à l EOF");
                                        break;
                                    }
                                }
                                // L'EOF peut griller la politesse à un
                                // préchargement encore en vol : la chaîne
                                // PrepareNext → fetch → oneshot est asynchrone,
                                // et rien ne garantit que le bras `prefetch_rx`
                                // ait été servi avant que le flux ne s'épuise
                                // (#1358, mesuré : résultat prêt 2 s avant
                                // l'EOF et jamais consommé). La fin de piste
                                // est précisément le moment où la piste
                                // suivante compte : on attend le résultat —
                                // borné, un fetch local répond en ms et un
                                // vrai échec rend None de toute façon.
                                if next_track.is_none()
                                    && let Some(rx) = prefetch_rx.take()
                                    && let Ok(Ok(Some(prefetch))) = tokio::time::timeout(
                                        std::time::Duration::from_secs(3),
                                        rx,
                                    )
                                    .await
                                {
                                    next_track = settle_prefetch(
                                        &mut endpoint, prefetch, &stream_id, &device_name,
                                        cur_format, cur_sample_rate, ch, layout, cur_bits,
                                    )
                                    .await;
                                }
                                // Flush remaining buffer
                                while buf.len() >= bytes_per_frame && playing.load(Ordering::Relaxed) {
                                    let chunk_bytes = packet_size.min(buf.len());
                                    let chunk_bytes = chunk_bytes
                                        - (chunk_bytes % bytes_per_frame);
                                    let payload: Vec<u8> = buf.drain(..chunk_bytes).collect();
                                    if is_flac {
                                        // La position du paquet est celle de la
                                        // dernière trame FLAC qui y commence —
                                        // pas un prorata d'octets (#2214).
                                        trames_flac.avaler(&payload);
                                        if trames_flac.est_synchronise() {
                                            sample_offset = trames_flac.position_samples();
                                        }
                                    }
                                    let pts_ns = if is_dsd {
                                        stream_start_ns + (byte_offset as f64 / (cur_sample_rate as f64 * bytes_per_frame as f64) * 1e9) as u64
                                    } else {
                                        stream_start_ns + (sample_offset as f64 / cur_sample_rate as f64 * 1e9) as u64
                                    };
                                    let _ = endpoint.send_audio(stream_num, cur_format, pts_ns, sample_offset, &payload, PacketFlags::empty()).await;
                                    if uses_byte_offset { byte_offset += payload.len() as u64; }
                                    else { sample_offset += (payload.len() / bytes_per_frame) as u64; }
                                    position_ms.store(
                                        if is_dsd { byte_offset * 1000 / (cur_sample_rate as u64 * bytes_per_frame as u64).max(1) }
                                        else { sample_offset * 1000 / cur_sample_rate.max(1) as u64 },
                                        Ordering::Relaxed,
                                    );
                                }

                                // Signal end of current track
                                endpoint.send_audio(stream_num, cur_format, 0, sample_offset, &[], PacketFlags::LAST_PACKET).await.ok();

                                // Gapless transition
                                if let Some(next) = next_track.take() {
                                    info!(device = %device_name, title = %next.title, "oaat: gapless transition");

                                    // A format change tears the endpoint's output
                                    // down (`configure()` sets started=false there),
                                    // so the swapped-in track needs its own Play —
                                    // see below.
                                    let renegotiated = !next.same_format;
                                    let next_source_channels = next.info.channels.min(8) as u8;
                                    let next_source_layout = match disposition_oaat(next_source_channels) {
                                        Ok(layout) => layout,
                                        Err(raison) => {
                                            error!(device = %device_name, %raison, "oaat: disposition source gapless invalide");
                                            break;
                                        }
                                    };
                                    let next_source = ContratPropose {
                                        stream_id: stream_id.clone(),
                                        format: next.info.format,
                                        sample_rate: next.info.sample_rate,
                                        channels: next_source_channels,
                                        channel_layout: next_source_layout,
                                        bits_per_sample: next.info.bits_per_sample,
                                        dsd_rate: None,
                                    };
                                    let next_target = if renegotiated {
                                        if let Err(e) = endpoint.propose_format(
                                            &stream_id,
                                            next_source.format,
                                            next_source.sample_rate,
                                            next_source.channels,
                                            next_source.channel_layout,
                                            next_source.bits_per_sample as u8,
                                        ).await {
                                            error!(device = %device_name, error = %e, "oaat: re-negotiate failed");
                                            break;
                                        }
                                        // Un `FormatCounter` etait traite comme un
                                        // `FormatAccept` et ses valeurs jetees : on
                                        // reinstallait celles de la piste suivante,
                                        // donc on renvoyait un format que l'endpoint
                                        // venait de refuser (#2239).
                                        //
                                        // `propose_format` n'envoie PAS de `dsd_rate` :
                                        // le contrat reellement propose ici n'en porte
                                        // donc pas, et une contre-proposition qui en
                                        // pose un ajoute bien une contrainte (#2283).
                                        match attendre_accord_format(
                                            &mut endpoint,
                                            &device_name,
                                            &next_source,
                                            PolitiqueAdaptation::PcmEntier,
                                            std::time::Duration::from_secs(5),
                                        )
                                        .await
                                        {
                                            Ok(cible) => cible,
                                            Err(refus) => {
                                                error!(
                                                    device = %device_name,
                                                    raison = %refus.raison,
                                                    "oaat: contre-proposition non honorable en gapless, fin de chaine"
                                                );
                                                signaler_refus_negociation(&refus_negociation, &refus);
                                                break;
                                            }
                                        }
                                    } else {
                                        next_source.clone()
                                    };

                                    let mut next_adapter = match construire_adaptateur_pcm(
                                        &next_source,
                                        &next_target,
                                    ) {
                                        Ok(adapter) => adapter,
                                        Err(raison) => {
                                            let refus = RefusNegociation {
                                                stream_id: stream_id.clone(),
                                                raison: format!("adaptation gapless impossible : {raison}"),
                                                reconnectable: false,
                                            };
                                            signaler_refus_negociation(&refus_negociation, &refus);
                                            error!(device = %device_name, %raison, "oaat: préparation gapless refusée");
                                            break;
                                        }
                                    };
                                    let next_buf = match next_adapter.push(&next.buf) {
                                        Ok(buf) => buf,
                                        Err(raison) => {
                                            error!(device = %device_name, %raison, "oaat: premier bloc gapless PCM invalide");
                                            break;
                                        }
                                    };

                                    source_format = next_source.format;
                                    source_sample_rate = next_source.sample_rate;
                                    source_bits = next_source.bits_per_sample;
                                    source_channels = next_source.channels;
                                    source_layout = next_source.channel_layout;
                                    source_data_offset = next.info.data_offset;
                                    source_bytes_per_frame =
                                        (source_bits as usize / 8) * source_channels as usize;
                                    cur_format = next_target.format;
                                    cur_sample_rate = next_target.sample_rate;
                                    cur_bits = next_target.bits_per_sample;
                                    cur_channels = next_target.channels;
                                    ch = cur_channels;
                                    layout = next_target.channel_layout;
                                    bytes_per_frame =
                                        (cur_bits as usize / 8) * cur_channels as usize;
                                    packet_size = PCM_SAMPLES_PER_PACKET * bytes_per_frame;
                                    is_flac = false;
                                    is_dsd = false;
                                    uses_byte_offset = false;
                                    adaptateur_pcm = next_adapter;
                                    cur_stream_info = StreamInfo {
                                        sample_rate: cur_sample_rate,
                                        channels: cur_channels as u16,
                                        bits_per_sample: cur_bits,
                                        format: cur_format,
                                        duration_ms: next.duration_ms,
                                        dsd_rate: None,
                                        data_offset: source_data_offset,
                                    };

                                    *current_title.lock().await = Some(next.title.clone());
                                    *current_artist.lock().await = Some(next.artist.clone());
                                    *current_uri.lock().await = Some(String::new());
                                    duration_ms_arc.store(next.duration_ms, Ordering::SeqCst);

                                    let fmt_str = format_rate_display(cur_sample_rate, cur_bits, cur_format);
                                    endpoint.send_metadata(oaat_core::message::TrackMetadata {
                                        title: next.title, artist: next.artist, album: next.album,
                                        duration_ms: next.duration_ms, artwork_url: next.cover_url,
                                        format: Some(fmt_str),
                                    }).await.ok();

                                    // Restart the stream after a renegotiation.
                                    // Proposing a format makes the endpoint tear
                                    // its output down and clear `started`; from
                                    // then on it DROPS every packet until a Play
                                    // arrives. We kept streaming without one, so
                                    // any gapless transition that changed format
                                    // killed the sound for good — position still
                                    // advancing, endpoint silent, nothing in
                                    // either log saying why (.18, 8 Aug 2026:
                                    // silent from 15:37:12, the first 44.1->96 kHz
                                    // transition). Same order as a fresh session:
                                    // format, metadata, Play, then packets.
                                    if renegotiated {
                                        if let Err(e) = endpoint.send_play(&stream_id).await {
                                            error!(device = %device_name, error = %e, "oaat: play after re-negotiation failed");
                                            break;
                                        }
                                        info!(device = %device_name, sample_rate = cur_sample_rate, bits = cur_bits, "oaat: stream restarted after format change");
                                    }

                                    sample_offset = 0;
                                    byte_offset = 0;
                                    position_ms.store(0, Ordering::SeqCst);
                                    stream_start_ns = super::helpers::now_ns() + 500_000_000;
                                    start = std::time::Instant::now();
                                    buf = next_buf;
                                    stream = next.stream;
                                    continue;
                                }
                                break;
                            }
                        }

                        // Send buffered packets
                        while buf.len() >= packet_size
                            && playing.load(Ordering::Relaxed)
                            && !paused.load(Ordering::Relaxed)
                        {
                            let payload: Vec<u8> = buf.drain(..packet_size).collect();
                            if is_flac {
                                // Position réelle depuis les en-têtes de trames
                                // (#2214) — et `sample_offset` cesse d'être figé
                                // à zéro sur ce chemin.
                                trames_flac.avaler(&payload);
                                if trames_flac.est_synchronise() {
                                    sample_offset = trames_flac.position_samples();
                                }
                            }
                            let pts_ns = if is_dsd {
                                stream_start_ns + (byte_offset as f64 / (cur_sample_rate as f64 * bytes_per_frame as f64) * 1e9) as u64
                            } else {
                                stream_start_ns + (sample_offset as f64 / cur_sample_rate as f64 * 1e9) as u64
                            };
                            let flags = if sample_offset == 0 && byte_offset == 0 {
                                PacketFlags::FIRST_PACKET
                            } else {
                                PacketFlags::empty()
                            };

                            match endpoint.send_audio(stream_num, cur_format, pts_ns, sample_offset, &payload, flags).await {
                                Ok(()) => {
                                    reconnect_attempts = 0;
                                    diag.packets_sent.fetch_add(1, Ordering::Relaxed);
                                    diag.bytes_sent.fetch_add(payload.len() as u64, Ordering::Relaxed);
                                    diag.last_packet_epoch_ms.store(
                                        std::time::SystemTime::now()
                                            .duration_since(std::time::UNIX_EPOCH)
                                            .unwrap_or_default()
                                            .as_millis() as u64,
                                        Ordering::Relaxed,
                                    );
                                }
                                Err(_) => {
                                    // Reconnection mid-stream
                                    if reconnect_attempts >= MAX_RECONNECT_ATTEMPTS {
                                        error!(device = %device_name, "oaat: send_audio failed, max reconnects reached");
                                        break;
                                    }
                                    reconnect_attempts += 1;
                                    diag.reconnects.fetch_add(1, Ordering::Relaxed);
                                    warn!(device = %device_name, attempt = reconnect_attempts, "oaat: send_audio failed, reconnecting");

                                    // Put payload back
                                    let mut restored = payload;
                                    restored.extend_from_slice(&buf);
                                    buf = restored;

                                    match connect_and_setup(
                                        &config,
                                        endpoint_addr,
                                        &device_name,
                                        &stream_id,
                                        &cur_stream_info,
                                        &refus_negociation,
                                    ).await {
                                        Some(new_ep) => {
                                            endpoint = new_ep;
                                            info!(device = %device_name, "oaat: reconnected, resuming stream");
                                            continue;
                                        }
                                        None => {
                                            error!(device = %device_name, "oaat: reconnection failed");
                                            break;
                                        }
                                    }
                                }
                            }

                            if sample_offset == 0 && byte_offset == 0 {
                                info!(device = %device_name, payload_bytes = payload.len(), "oaat: first audio packet sent");
                            }

                            if uses_byte_offset {
                                byte_offset += payload.len() as u64;
                            } else {
                                sample_offset += PCM_SAMPLES_PER_PACKET as u64;
                            }
                            position_ms.store(
                                if is_dsd { byte_offset * 1000 / (cur_sample_rate as u64 * bytes_per_frame as u64).max(1) }
                                else { sample_offset * 1000 / cur_sample_rate.max(1) as u64 },
                                Ordering::Relaxed,
                            );

                            // Real-time pacing — skip for first 50 packets to pre-fill endpoint buffer
                            let packet_num = if uses_byte_offset {
                                byte_offset / packet_size as u64
                            } else {
                                sample_offset / PCM_SAMPLES_PER_PACKET as u64
                            };
                            if packet_num > 50 {
                                // Le cadencement aussi : caler l'envoi FLAC sur
                                // les octets compressés envoyait un morceau très
                                // compressé trop vite, et un peu compressé trop
                                // lentement (#2214). Les samples réels cadencent ;
                                // le DSD garde les octets, son débit est constant.
                                let base_de_temps = if is_dsd {
                                    BaseDeTempsOaat::OctetsADebitConstant
                                } else {
                                    BaseDeTempsOaat::Samples
                                };
                                let expected = duree_audio_envoyee(
                                    base_de_temps,
                                    sample_offset,
                                    byte_offset,
                                    cur_sample_rate,
                                    bytes_per_frame,
                                );
                                let elapsed = start.elapsed();
                                if expected > elapsed {
                                    tokio::time::sleep(expected - elapsed).await;
                                }
                            }
                        }
                    }
                }
            }

            // The loop is over: this task will never take another gapless
            // transition, so the output must stop claiming it can. Said before
            // the first `.await` of the tail, so the poller learns it on its
            // very next tick rather than after `send_stop` has round-tripped.
            //
            // Xavier Joly, 7 Aug 2026 (#1323): the loop left by the stream-error
            // path at 16:34:01 and the poller, still told "a transition is
            // coming", logged `gapless_natural_end_waiting_for_transition` at
            // 16:34:06 and waited until 16:34:40 before giving up — the next
            // movement then restarted from cold at 16:35:24. Correcting the
            // end-of-track detection alone would not have closed that gap: ANY
            // exit without a transition opens it.
            if !exited_on_stop {
                chain_exhausted.store(true, Ordering::SeqCst);
            }

            endpoint.send_stop(&stream_id).await.ok();
            playing.store(false, Ordering::SeqCst);
            diag.connected.store(false, Ordering::SeqCst);
            let duration_s = start.elapsed().as_secs_f64();
            let packets = if uses_byte_offset {
                byte_offset / FLAC_CHUNK_SIZE as u64
            } else {
                sample_offset / PCM_SAMPLES_PER_PACKET as u64
            };
            info!(device = %device_name, samples = sample_offset, packets, duration_s = format!("{duration_s:.1}"), "oaat: playback complete");
        });
        *self.play_task.lock().await = Some(task);

        Ok(())
    }

    #[cfg(not(feature = "oaat"))]
    async fn play_media(&self, _media: &PlayMedia<'_>) -> Result<(), String> {
        Err("OAAT support not compiled (enable 'oaat' feature)".into())
    }

    async fn pause(&self) -> Result<(), String> {
        self.paused.store(true, Ordering::SeqCst);
        #[cfg(feature = "oaat")]
        if let Some(tx) = self.command_tx.lock().await.as_ref() {
            let _ = tx.send(OaatCommand::Pause).await;
        }
        info!(device = %self.name, "oaat: pause");
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.paused.store(false, Ordering::SeqCst);
        #[cfg(feature = "oaat")]
        if let Some(tx) = self.command_tx.lock().await.as_ref() {
            let _ = tx.send(OaatCommand::Resume).await;
        }
        info!(device = %self.name, "oaat: resume");
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        if let Some(tx) = self.stop_tx.lock().await.take() {
            let _ = tx.send(());
        }
        // Le signal ci-dessus ne suffit pas pendant la phase de connexion : la
        // boucle des quinze tentatives ne l'écoute pas, c'est un `for` avec des
        // pauses. Un `stop` était donc journalisé pendant que la boucle
        // continuait à réclamer l'endpoint jusqu'à quarante secondes — et
        // volait la connexion à la lecture suivante (#1475).
        if let Some(task) = self.play_task.lock().await.take() {
            if !task.is_finished() {
                task.abort();
            }
        }
        #[cfg(feature = "oaat")]
        {
            self.command_tx.lock().await.take();
        }
        self.playing.store(false, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
        // Leave native-DSD mode. play_media() calls stop() before starting the
        // next track, so this resets the flag between tracks; the native DSD
        // path re-sets it if the next track is also native DSD.
        self.native_dsd_active.store(false, Ordering::SeqCst);
        self.direct_pcm_active.store(false, Ordering::SeqCst);
        self.chain_exhausted.store(false, Ordering::SeqCst);
        *self.current_uri.lock().await = None;
        info!(device = %self.name, "oaat: stop");
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        #[cfg(feature = "oaat")]
        if let Some(tx) = self.command_tx.lock().await.as_ref() {
            let _ = tx.send(OaatCommand::Seek { position_ms }).await;
        }
        info!(device = %self.name, position_ms, "oaat: seek");
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        // 0–100, l'echelle du protocole (identique a multiroom.rs). Le *255
        // precedent saturait l'endpoint des ~40 % dans l'interface : le RPi
        // plafonne a 100 (amixer), donc tout ce qui depassait sortait a fond,
        // et une zone OAAT seule jouait bien plus fort qu'une sortie locale.
        let level = (volume.clamp(0.0, 1.0) * 100.0).round() as u8;
        self.volume.store(level as u32, Ordering::SeqCst);
        #[cfg(feature = "oaat")]
        if let Some(tx) = self.command_tx.lock().await.as_ref() {
            let _ = tx.send(OaatCommand::SetVolume(level)).await;
        }
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        if muted {
            self.volume.store(0, Ordering::SeqCst);
        }
        #[cfg(feature = "oaat")]
        if let Some(tx) = self.command_tx.lock().await.as_ref() {
            let _ = tx.send(OaatCommand::Mute(muted)).await;
        }
        Ok(())
    }

    #[cfg(feature = "oaat")]
    async fn set_next_url(
        &self,
        url: &str,
        _mime_type: &str,
        title: Option<&str>,
        artist: Option<&str>,
    ) -> Result<(), String> {
        if let Some(tx) = self.command_tx.lock().await.as_ref() {
            tx.send(OaatCommand::PrepareNext {
                url: url.to_owned(),
                file_path: None,
                title: title.unwrap_or("Unknown").to_owned(),
                artist: artist.unwrap_or("Unknown").to_owned(),
                album: String::new(),
                cover_url: None,
                duration_ms: 0,
            })
            .await
            .map_err(|e| format!("channel closed: {e}"))?;
            info!(device = %self.name, title = ?title, "oaat: next track queued");
        }
        Ok(())
    }

    #[cfg(feature = "oaat")]
    async fn set_next_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        if let Some(tx) = self.command_tx.lock().await.as_ref() {
            tx.send(OaatCommand::PrepareNext {
                url: media.url.to_owned(),
                // Carry the local path so the native DSD loop can open the next
                // `.dsf` directly (raw DSD gapless). The PCM/HTTP path leaves
                // this None and prefetches the URL instead.
                file_path: media.file_path.map(|s| s.to_owned()),
                title: media.title.unwrap_or("Unknown").to_owned(),
                artist: media.artist.unwrap_or("Unknown").to_owned(),
                album: media.album.unwrap_or("").to_owned(),
                cover_url: media.cover_url.map(|s| s.to_owned()),
                duration_ms: media.duration_ms.unwrap_or(0),
            })
            .await
            .map_err(|e| format!("channel closed: {e}"))?;
            info!(device = %self.name, title = ?media.title, "oaat: next track queued");
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
            volume: self.volume.load(Ordering::Relaxed) as f64 / 100.0,
            muted: self.volume.load(Ordering::Relaxed) == 0,
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

    /// Le motif du refus de negociation, remis UNE fois au poller, qui en fait
    /// un `zone.playback_error` `fatal: true` — donc un message a l'ecran au
    /// lieu d'une zone muette (#2294).
    fn take_output_failure(&self) -> Option<String> {
        self.refus_negociation
            .lock()
            .ok()
            .and_then(|mut s| s.take())
    }

    async fn is_available(&self) -> bool {
        true
    }

    fn diagnostics_json(&self) -> Option<serde_json::Value> {
        Some(self.diagnostics_snapshot())
    }
}

/// Open the next track's local `.dsf` for a native-DSD gapless transition, but
/// only if it is format-compatible with the currently streaming track (same DSD
/// sample rate and channel count). Returns None for a non-`.dsf` file, a parse
/// error, or any format mismatch — the caller then ends the current track
/// cleanly and lets the poller's natural-end fallback advance the queue (a small
/// gap across the format boundary, never a stall). Blocking file I/O: call from
/// within `tokio::task::block_in_place`.
#[cfg(feature = "oaat")]
fn open_next_dsd(
    file_path: &str,
    expect_sample_rate: u32,
    expect_channels: u8,
    title: String,
    artist: String,
    album: String,
    cover_url: Option<String>,
    duration_ms: u64,
) -> Option<PreparedDsdNext> {
    let is_dsf = std::path::Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("dsf"));
    if !is_dsf {
        return None;
    }
    let info = crate::audio::dsf::parse_dsf(file_path).ok()?;
    if info.sample_rate != expect_sample_rate {
        return None;
    }
    if (info.channels.min(8) as u8) != expect_channels {
        return None;
    }
    let reader = crate::audio::dsf::DsfStreamReader::open(file_path, info.clone()).ok()?;
    Some(PreparedDsdNext {
        reader,
        title,
        artist,
        album,
        cover_url,
        duration_ms,
    })
}

#[cfg(feature = "oaat")]
/// Le contrat de format effectivement PROPOSE a l'endpoint.
///
/// Les chemins DSD et de reconnexion exigent toujours une réponse identique.
/// Les chemins PCM initial, direct et gapless peuvent désormais rendre un
/// contrat cible différent, mais seulement après avoir construit le pipeline
/// qui produira réellement ses octets. Le contrat rendu par la négociation est
/// donc celui du PAYLOAD, jamais une simple étiquette (#2239).
///
/// Regrouper les champs en une structure n'est pas cosmetique. La comparaison
/// porte sur TOUT ce qui se negocie ; tant qu'elle etait une liste d'arguments
/// positionnels, il manquait toujours un champ et le compilateur ne pouvait
/// rien dire (`channels` et `channel_layout` omis d'abord, puis `dsd_rate` et
/// `stream_id` — #2283, JP Robbe).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ContratPropose {
    pub stream_id: String,
    pub format: oaat_core::format::AudioFormat,
    pub sample_rate: u32,
    pub channels: u8,
    pub channel_layout: oaat_core::format::ChannelLayout,
    pub bits_per_sample: u16,
    pub dsd_rate: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolitiqueAdaptation {
    ExacteSeulement,
    PcmEntier,
}

fn profondeur_pcm_entiere(contrat: &ContratPropose) -> Result<u16, String> {
    use oaat_core::format::AudioFormat;

    let attendue = match contrat.format {
        AudioFormat::PcmS16le => 16,
        AudioFormat::PcmS24le => 24,
        AudioFormat::PcmS32le => 32,
        autre => {
            return Err(format!(
                "le codec {autre:?} ne passe pas par le convertisseur PCM entier"
            ));
        }
    };
    if contrat.bits_per_sample != attendue {
        return Err(format!(
            "le codec {:?} exige {attendue} bits, pas {}",
            contrat.format, contrat.bits_per_sample
        ));
    }
    if contrat.sample_rate == 0 {
        return Err("la cadence PCM vaut zéro".into());
    }
    if contrat.channels == 0 || contrat.channels > 8 {
        return Err(format!(
            "le nombre de canaux PCM {} est hors de la plage OAAT 1..=8",
            contrat.channels
        ));
    }
    if contrat.channel_layout.channel_count() != contrat.channels {
        return Err(format!(
            "la disposition {:?} décrit {} canaux, pas {}",
            contrat.channel_layout,
            contrat.channel_layout.channel_count(),
            contrat.channels
        ));
    }
    if contrat.dsd_rate.is_some() {
        return Err("un contrat PCM ne peut pas porter de multiplicateur DSD".into());
    }
    Ok(attendue)
}

fn construire_adaptateur_pcm(
    source: &ContratPropose,
    cible: &ContratPropose,
) -> Result<crate::audio::decode::StreamingPcmByteAdapter, String> {
    let source_bits = profondeur_pcm_entiere(source)?;
    let cible_bits = profondeur_pcm_entiere(cible)?;
    crate::audio::decode::StreamingPcmByteAdapter::new(
        source_bits,
        source.channels as u32,
        source.sample_rate,
        cible_bits,
        cible.channels as u32,
        cible.sample_rate,
    )
}

fn disposition_oaat(channels: u8) -> Result<oaat_core::format::ChannelLayout, String> {
    use oaat_core::format::ChannelLayout;
    match channels {
        1 => Ok(ChannelLayout::Mono),
        2 => Ok(ChannelLayout::Stereo),
        3 => Ok(ChannelLayout::TwoPointOne),
        4 => Ok(ChannelLayout::Quad),
        6 => Ok(ChannelLayout::FivePointOne),
        8 => Ok(ChannelLayout::SevenPointOne),
        _ => Err(format!(
            "aucune disposition OAAT non ambiguë pour {channels} canaux"
        )),
    }
}

pub(crate) fn adapter_piste_directe_gapless(
    mut piste: super::helpers::StagedDirectTrack,
    cible: &ContratPropose,
) -> Result<super::helpers::StagedDirectTrack, String> {
    let source = ContratPropose {
        stream_id: cible.stream_id.clone(),
        format: piste.format,
        sample_rate: piste.sample_rate,
        channels: piste.channels,
        channel_layout: disposition_oaat(piste.channels)?,
        bits_per_sample: piste.bits_per_sample,
        dsd_rate: None,
    };
    let mut adaptateur = construire_adaptateur_pcm(&source, cible)?;
    let mut pcm = adaptateur.push(&piste.pcm)?;
    pcm.extend(adaptateur.finish()?);
    piste.pcm = pcm;
    piste.format = cible.format;
    piste.sample_rate = cible.sample_rate;
    piste.bits_per_sample = cible.bits_per_sample;
    piste.channels = cible.channels;
    Ok(piste)
}

impl ContratPropose {
    fn depuis_contre_proposition(contre: &oaat_core::message::FormatCounter) -> Self {
        Self {
            stream_id: contre.stream_id.clone(),
            format: contre.format,
            sample_rate: contre.sample_rate,
            channels: contre.channels,
            channel_layout: contre.channel_layout,
            bits_per_sample: contre.bits_per_sample as u16,
            dsd_rate: contre.dsd_rate,
        }
    }

    /// Le premier champ par lequel une contre-proposition s'ecarte du contrat,
    /// nomme et chiffre — `None` si elle le decrit exactement.
    ///
    /// Rendre le champ fautif plutot qu'un booleen sert deux fois : le journal
    /// dit ce qui cloche, et l'utilisateur recoit une raison lisible au lieu
    /// d'une zone qui se tait (#2294).
    pub(crate) fn premier_ecart(
        &self,
        contre: &oaat_core::message::FormatCounter,
    ) -> Option<String> {
        if self.format != contre.format {
            return Some(format!(
                "codec {:?} propose contre {:?} contre-propose",
                self.format, contre.format
            ));
        }
        if self.sample_rate != contre.sample_rate {
            return Some(format!(
                "cadence {} Hz proposee contre {} Hz contre-proposee",
                self.sample_rate, contre.sample_rate
            ));
        }
        if self.channels != contre.channels {
            return Some(format!(
                "{} canaux proposes contre {} contre-proposes",
                self.channels, contre.channels
            ));
        }
        if self.channel_layout != contre.channel_layout {
            return Some(format!(
                "disposition {:?} proposee contre {:?} contre-proposee",
                self.channel_layout, contre.channel_layout
            ));
        }
        if self.bits_per_sample != contre.bits_per_sample as u16 {
            return Some(format!(
                "{} bits proposes contre {} contre-proposes",
                self.bits_per_sample, contre.bits_per_sample
            ));
        }
        // `dsd_rate` se COMPARE, il ne s'exige pas absent. Le rendre
        // obligatoirement `None` refusait une contre-proposition DSD64
        // rigoureusement identique a la proposition DSD64 : les trois chemins
        // qui posent un `FormatPropose` a la main envoient bien un
        // multiplicateur (#2283, JP Robbe).
        if self.dsd_rate != contre.dsd_rate {
            return Some(format!(
                "DSD {:?} propose contre {:?} contre-propose",
                self.dsd_rate, contre.dsd_rate
            ));
        }
        None
    }
}

/// Ce qui est arrive pendant l'attente d'une reponse a la proposition.
///
/// Le silence et la fermeture sont des issues a part entiere, pas des cas
/// « autres » : les distinguer permet de les tester sans horloge.
pub(crate) enum ReponseNegociation<'a> {
    Recue(&'a oaat_controller::EndpointResponse),
    Fermee,
    Timeout,
}

/// Un refus de negociation, avec de quoi le remonter jusqu'a l'utilisateur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RefusNegociation {
    pub stream_id: String,
    pub raison: String,
    /// Une fermeture du canal de réponse peut être réparée par une nouvelle
    /// connexion. Un refus explicite, une contre-proposition incompatible ou
    /// une réponse étrangère ne doivent jamais être rejoués aveuglément.
    pub reconnectable: bool,
}

impl std::fmt::Display for RefusNegociation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.raison)
    }
}

/// La decision de negociation, isolee de tout reseau et de toute horloge.
///
/// Elle vivait dans un `match` au milieu d'une tache asynchrone : impossible a
/// tester autrement qu'en lisant le texte source, ce que faisait mon test de
/// #2291 — il restait vert quand on remplacait le bras `FormatReject => Err`
/// par `Ok(())` (#2297, JP Robbe). Fonction pure, donc verifiable sur les huit
/// issues : accord, accord d'un autre flux, contre-proposition identique,
/// contre-proposition ecartee, refus, reponse hors sujet, fermeture, silence.
pub(crate) fn juger_reponse(
    contrat: &ContratPropose,
    reponse: ReponseNegociation<'_>,
    politique: PolitiqueAdaptation,
) -> Result<ContratPropose, RefusNegociation> {
    let refus = |raison: String, reconnectable: bool| {
        Err(RefusNegociation {
            stream_id: contrat.stream_id.clone(),
            raison,
            reconnectable,
        })
    };
    let flux_etranger = |recu: &str| {
        format!(
            "reponse pour le flux {recu} alors qu'on negociait {} — une reponse \
             en retard prise pour la bonne decale toute la suite",
            contrat.stream_id
        )
    };

    match reponse {
        ReponseNegociation::Timeout => refus(
            "aucune reponse a la proposition de format (delai depasse)".into(),
            false,
        ),
        ReponseNegociation::Fermee => refus("endpoint ferme pendant la negociation".into(), true),
        ReponseNegociation::Recue(recue) => match recue {
            oaat_controller::EndpointResponse::FormatAccept(fa) => {
                if fa.stream_id != contrat.stream_id {
                    refus(flux_etranger(&fa.stream_id), false)
                } else {
                    Ok(contrat.clone())
                }
            }
            oaat_controller::EndpointResponse::FormatCounter(fc) => {
                if fc.stream_id != contrat.stream_id {
                    refus(flux_etranger(&fc.stream_id), false)
                } else {
                    let cible = ContratPropose::depuis_contre_proposition(fc);
                    let Some(ecart) = contrat.premier_ecart(fc) else {
                        return Ok(cible);
                    };
                    match politique {
                        PolitiqueAdaptation::ExacteSeulement => refus(
                            format!(
                                "l'endpoint demande un autre format sur un chemin sans \
                                 convertisseur actif : {ecart}"
                            ),
                            false,
                        ),
                        PolitiqueAdaptation::PcmEntier => {
                            match construire_adaptateur_pcm(contrat, &cible) {
                                Ok(_) => Ok(cible),
                                Err(cause) => refus(
                                    format!(
                                        "contre-proposition non produisible : {ecart} ; {cause}"
                                    ),
                                    false,
                                ),
                            }
                        }
                    }
                }
            }
            oaat_controller::EndpointResponse::FormatReject(fr) => {
                if fr.stream_id != contrat.stream_id {
                    refus(flux_etranger(&fr.stream_id), false)
                } else {
                    refus(
                        format!("format refuse par l'endpoint : {}", fr.reason),
                        false,
                    )
                }
            }
            autre => refus(
                format!("reponse inattendue pendant la negociation de format : {autre:?}"),
                false,
            ),
        },
    }
}

/// Attendre la reponse a une proposition de format, et dire si on peut jouer.
///
/// Deux chemins — DSD natif et PCM direct — proposaient un format puis
/// appelaient `send_play` **sans jamais lire `response_rx`**. Trois
/// consequences, toutes silencieuses (JP Robbe, #2282) :
///
/// - un `FormatReject` etait ignore : Tune lancait la lecture alors que
///   l'endpoint venait de dire non ;
/// - un `FormatCounter` etait ignore : le payload source partait sans
///   adaptation, comme dans #2239 ;
/// - la reponse non consommee restait dans `response_rx` et pouvait etre prise
///   pour la reponse d'une negociation ULTERIEURE.
///
/// Ce troisieme point est le plus vicieux : le decalage survit a la piste qui
/// l'a cause, et ne se voit donc pas la ou il est ne.
///
/// Tout le jugement est delegue a `juger_reponse` : ici il ne reste que
/// l'attente et la trace.
async fn attendre_accord_format(
    endpoint: &mut oaat_controller::ConnectedEndpoint,
    device_name: &str,
    contrat: &ContratPropose,
    politique: PolitiqueAdaptation,
    delai: std::time::Duration,
) -> Result<ContratPropose, RefusNegociation> {
    let recue = tokio::time::timeout(delai, endpoint.response_rx.recv()).await;

    let verdict = match &recue {
        Ok(Some(reponse)) => juger_reponse(contrat, ReponseNegociation::Recue(reponse), politique),
        Ok(None) => juger_reponse(contrat, ReponseNegociation::Fermee, politique),
        Err(_) => juger_reponse(contrat, ReponseNegociation::Timeout, politique),
    };

    if let Err(refus) = &verdict {
        error!(
            device = %device_name,
            stream_id = %refus.stream_id,
            raison = %refus.raison,
            "oaat: negociation de format refusee"
        );
    }
    verdict
}

async fn settle_prefetch(
    endpoint: &mut oaat_controller::ConnectedEndpoint,
    mut prefetch: NextTrackPrefetch,
    stream_id: &str,
    device_name: &str,
    cur_format: oaat_core::format::AudioFormat,
    cur_sample_rate: u32,
    ch: u8,
    layout: oaat_core::ChannelLayout,
    cur_bits: u16,
) -> Option<NextTrackPrefetch> {
    if prefetch.same_format {
        info!(device = %device_name, title = %prefetch.title, "oaat: next track prefetched (gapless ready)");
        if let Ok(()) = endpoint
            .prepare_next_track(
                stream_id,
                cur_format,
                cur_sample_rate,
                ch,
                layout,
                cur_bits as u8,
            )
            .await
        {
            match tokio::time::timeout(
                std::time::Duration::from_secs(2),
                endpoint.response_rx.recv(),
            )
            .await
            {
                Ok(Some(oaat_controller::EndpointResponse::NextTrackReady(_))) => {
                    info!(device = %device_name, "oaat: gapless confirmed");
                }
                Ok(Some(oaat_controller::EndpointResponse::NextTrackReformat(_))) => {
                    prefetch.same_format = false;
                }
                _ => {}
            }
        }
    } else {
        info!(device = %device_name, title = %prefetch.title, "oaat: next track prefetched (format change)");
    }
    Some(prefetch)
}

async fn prefetch_next_track(
    client: &reqwest::Client,
    device_name: &str,
    url: &str,
    title: String,
    artist: String,
    album: String,
    cover_url: Option<String>,
    duration_ms: u64,
    cur_format: oaat_core::format::AudioFormat,
    cur_sample_rate: u32,
    cur_bits: u16,
    cur_channels: u8,
) -> Option<NextTrackPrefetch> {
    use futures_util::StreamExt;

    let resp = match client.get(url).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            error!(device = %device_name, status = %r.status(), "oaat: next track HTTP error");
            return None;
        }
        Err(e) => {
            error!(device = %device_name, error = %e, "oaat: next track fetch failed");
            return None;
        }
    };

    let mut stream = resp.bytes_stream();
    let mut buf = Vec::new();

    while buf.len() < 128 {
        match stream.next().await {
            Some(Ok(chunk)) => buf.extend_from_slice(&chunk),
            _ => {
                error!(device = %device_name, "oaat: next track stream ended before header");
                return None;
            }
        }
    }

    let si = match detect_and_parse(&mut buf) {
        Some(info) => info,
        None => {
            error!(device = %device_name, "oaat: next track format unsupported");
            return None;
        }
    };

    let duration_ms = if duration_ms > 0 {
        duration_ms
    } else {
        si.duration_ms
    };
    let same_format = si.format == cur_format
        && si.sample_rate == cur_sample_rate
        && si.bits_per_sample == cur_bits
        && si.channels.min(8) as u8 == cur_channels;

    info!(
        device = %device_name, title = %title,
        format = %si.format, sample_rate = si.sample_rate, bits = si.bits_per_sample,
        same_format, "oaat: next track prefetched"
    );

    Some(NextTrackPrefetch {
        stream: stream.boxed(),
        buf,
        info: si,
        title,
        artist,
        album,
        cover_url,
        duration_ms,
        same_format,
    })
}
