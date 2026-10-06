//! AirPlay 2 output — wraps `airplay-daemon` as a subprocess (GPL isolation).
//!
//! Uses the same subprocess pattern as librespot for Spotify Connect.
//! The daemon binary reads JSON commands on stdin and emits JSON events on stdout.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};
use tracing::{debug, info, warn};

use crate::outputs::traits::{
    OutputCapabilities, OutputStatus, OutputTarget, PlayMedia, TransportState,
};

const DAEMON_BINARY: &str = "airplay-daemon";
const TRANSPORT_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

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
    muted: Arc<AtomicBool>,
    current_title: Arc<Mutex<Option<String>>>,
    current_artist: Arc<Mutex<Option<String>>>,
    process: Arc<Mutex<Option<DaemonProcess>>>,
    pairing: Arc<Mutex<PairingPhase>>,
    transport_command: Mutex<()>,
    pending_transport: Arc<Mutex<Option<PendingTransport>>>,
    /// Copie locale du flux en cours de lecture, que le daemon lit (#2169).
    /// Retiree a la piste suivante et a l'arret.
    copie_du_flux: Arc<Mutex<Option<PathBuf>>>,
}

/// Pairing progress for an AirPlay 2 receiver, updated by the daemon stdout
/// reader and awaited by the PIN-pairing methods. Most receivers accept the
/// hard-coded transient PIN (`3939`), but AirPlay-2-only TVs (Samsung, LG…) and
/// Apple TV require HomeKit PIN pairing: the receiver shows a 4-digit code the
/// user must type back (Bilou's Samsung S95, #1135).
#[derive(Clone, Debug, PartialEq)]
enum PairingPhase {
    Idle,
    /// The receiver is displaying a PIN; waiting for the user to submit it.
    PinRequested,
    /// Connected/paired successfully.
    Connected,
    Failed(String),
}

struct DaemonProcess {
    child: Option<Child>,
    stdin: Box<dyn AsyncWrite + Unpin + Send>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransportConfirmation {
    Paused,
    Playing,
}

struct PendingTransport {
    expected: TransportConfirmation,
    response: oneshot::Sender<Result<(), String>>,
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
            muted: Arc::new(AtomicBool::new(false)),
            current_title: Arc::new(Mutex::new(None)),
            current_artist: Arc::new(Mutex::new(None)),
            process: Arc::new(Mutex::new(None)),
            pairing: Arc::new(Mutex::new(PairingPhase::Idle)),
            transport_command: Mutex::new(()),
            pending_transport: Arc::new(Mutex::new(None)),
            copie_du_flux: Arc::new(Mutex::new(None)),
        }
    }

    fn start_event_reader<R>(&self, mut reader: R)
    where
        R: AsyncBufRead + Unpin + Send + 'static,
    {
        let playing = self.playing.clone();
        let paused = self.paused.clone();
        let position_ms = self.position_ms.clone();
        let device_name = self.name.clone();
        let pairing = self.pairing.clone();
        let pending_transport = self.pending_transport.clone();

        tokio::spawn(async move {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        playing.store(false, Ordering::SeqCst);
                        paused.store(false, Ordering::SeqCst);
                        if let Some(pending) = pending_transport.lock().await.take() {
                            let _ = pending.response.send(Err("daemon exited".into()));
                        }
                        *pairing.lock().await = PairingPhase::Failed("daemon exited".into());
                        break;
                    }
                    Ok(_) => {
                        let Ok(ev) = serde_json::from_str::<serde_json::Value>(&line) else {
                            continue;
                        };
                        let event = ev["event"].as_str().unwrap_or("");
                        let confirmation = match event {
                            "pin_requested" => {
                                *pairing.lock().await = PairingPhase::PinRequested;
                                info!(device = %device_name, "airplay2: receiver is showing a pairing PIN");
                                None
                            }
                            "connected" | "paired" => {
                                *pairing.lock().await = PairingPhase::Connected;
                                debug!(device = %device_name, "airplay2: connected/paired");
                                None
                            }
                            "playing" => {
                                playing.store(true, Ordering::SeqCst);
                                paused.store(false, Ordering::SeqCst);
                                debug!(device = %device_name, "airplay2: playing");
                                Some(TransportConfirmation::Playing)
                            }
                            "paused" => {
                                playing.store(true, Ordering::SeqCst);
                                paused.store(true, Ordering::SeqCst);
                                debug!(device = %device_name, "airplay2: paused");
                                Some(TransportConfirmation::Paused)
                            }
                            "stopped" | "disconnected" => {
                                playing.store(false, Ordering::SeqCst);
                                paused.store(false, Ordering::SeqCst);
                                debug!(device = %device_name, "airplay2: stopped");
                                None
                            }
                            "status" => {
                                if let Some(pos) = ev["position_s"].as_f64() {
                                    position_ms.store((pos * 1000.0) as u64, Ordering::Relaxed);
                                }
                                None
                            }
                            "error" => {
                                let msg = ev["message"].as_str().unwrap_or("unknown").to_string();
                                let pending = pending_transport.lock().await.take();
                                if let Some(pending) = pending {
                                    let _ = pending.response.send(Err(msg.clone()));
                                } else {
                                    *pairing.lock().await = PairingPhase::Failed(msg.clone());
                                }
                                warn!(device = %device_name, error = %msg, "airplay2: daemon error");
                                None
                            }
                            _ => None,
                        };

                        if let Some(confirmation) = confirmation {
                            let pending = {
                                let mut slot = pending_transport.lock().await;
                                if slot
                                    .as_ref()
                                    .is_some_and(|pending| pending.expected == confirmation)
                                {
                                    slot.take()
                                } else {
                                    None
                                }
                            };
                            if let Some(pending) = pending {
                                let _ = pending.response.send(Ok(()));
                            }
                        }
                    }
                    Err(error) => {
                        playing.store(false, Ordering::SeqCst);
                        paused.store(false, Ordering::SeqCst);
                        let msg = format!("daemon read error: {error}");
                        if let Some(pending) = pending_transport.lock().await.take() {
                            let _ = pending.response.send(Err(msg.clone()));
                        }
                        *pairing.lock().await = PairingPhase::Failed(msg);
                        break;
                    }
                }
            }
        });
    }

    /// Spawn the daemon, wait for `ready`, and start the single background stdout
    /// reader that updates playback state AND pairing progress. Does NOT connect.
    async fn spawn_daemon(&self) -> Result<DaemonProcess, String> {
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

        self.start_event_reader(reader);

        Ok(DaemonProcess {
            child: Some(child),
            stdin: Box::new(stdin),
        })
    }

    /// Current pairing progress as a short string for the API/client to poll:
    /// `idle` | `pin_requested` | `connected` | `failed:<msg>`. Cheap (a quick
    /// lock), so it never blocks on the 30s human-paced pairing flow.
    pub async fn pairing_status(&self) -> String {
        match self.pairing.lock().await.clone() {
            PairingPhase::Idle => "idle".to_string(),
            PairingPhase::PinRequested => "pin_requested".to_string(),
            PairingPhase::Connected => "connected".to_string(),
            PairingPhase::Failed(e) => format!("failed:{e}"),
        }
    }

    async fn ensure_connected(&self) -> Result<(), String> {
        let mut proc = self.process.lock().await;
        if proc.is_some() {
            return Ok(());
        }
        *self.pairing.lock().await = PairingPhase::Idle;
        let mut daemon = self.spawn_daemon().await?;
        // Transient pairing (hard-coded PIN 3939) — works for HomePod-class and
        // already-paired receivers. PIN-only receivers need start_pin_pairing().
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

    /// Begin HomeKit PIN pairing: spawn the daemon (if needed) and ask the
    /// receiver to display its 4-digit code. Returns as soon as the command is
    /// sent; the client polls `pairing_status()` until `pin_requested`, then
    /// calls `submit_pin` with the code. (#1135)
    pub async fn start_pin_pairing(&self) -> Result<(), String> {
        let mut proc = self.process.lock().await;
        *self.pairing.lock().await = PairingPhase::Idle;
        if proc.is_none() {
            *proc = Some(self.spawn_daemon().await?);
        }
        proc.as_mut()
            .unwrap()
            .send_cmd(&serde_json::json!({
                "cmd": "pair_pin_start",
                "ip": self.host,
                "port": self.port,
            }))
            .await
    }

    /// Finish PIN pairing with the code the user read off the receiver, then
    /// connect. Returns as soon as the command is sent; the client polls
    /// `pairing_status()` until `connected` (or `failed:*`). On success the
    /// daemon persists the pairing identity so later sessions skip the PIN. (#1135)
    pub async fn submit_pin(&self, pin: &str) -> Result<(), String> {
        let mut proc = self.process.lock().await;
        let daemon = proc
            .as_mut()
            .ok_or("airplay2: no pairing in progress — call start_pin_pairing first")?;
        *self.pairing.lock().await = PairingPhase::Idle;
        daemon
            .send_cmd(&serde_json::json!({
                "cmd": "connect",
                "ip": self.host,
                "port": self.port,
                "device_id": self.ap_device_id,
                "pin": pin,
            }))
            .await
    }

    async fn send(&self, cmd: &serde_json::Value) -> Result<(), String> {
        let mut proc = self.process.lock().await;
        if let Some(daemon) = proc.as_mut() {
            daemon.send_cmd(cmd).await
        } else {
            Err("daemon not running".into())
        }
    }

    async fn send_transport_command(
        &self,
        command: &'static str,
        expected: TransportConfirmation,
    ) -> Result<(), String> {
        let _command_guard = self.transport_command.lock().await;
        let (response, receiver) = oneshot::channel();
        *self.pending_transport.lock().await = Some(PendingTransport { expected, response });

        if let Err(error) = self.send(&serde_json::json!({"cmd": command})).await {
            self.pending_transport.lock().await.take();
            return Err(error);
        }

        match tokio::time::timeout(TRANSPORT_COMMAND_TIMEOUT, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(format!(
                "airplay2: daemon response channel closed after {command}"
            )),
            Err(_) => {
                self.pending_transport.lock().await.take();
                Err(format!(
                    "airplay2: daemon did not confirm {command} within {} ms",
                    TRANSPORT_COMMAND_TIMEOUT.as_millis()
                ))
            }
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

    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(true, true, false, true, true, false)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        // Le daemon n'ouvre qu'un FICHIER : il faut le lui fournir AVANT de
        // le lancer, et refuser tot ce qu'il ne saura pas lire (#2169).
        let lisible = chemin_lisible_par_le_daemon(media, &self.device_id).await?;
        if let Err(error) = self.ensure_connected().await {
            lisible.oublier().await;
            return Err(error);
        }

        let title = media.title.unwrap_or("Unknown");
        let artist = media.artist.unwrap_or("Unknown");
        *self.current_title.lock().await = Some(title.to_owned());
        *self.current_artist.lock().await = Some(artist.to_owned());
        self.duration_ms
            .store(media.duration_ms.unwrap_or(0), Ordering::SeqCst);
        self.position_ms.store(0, Ordering::SeqCst);

        // AirPlay 2 lit le flux que le SERVEUR a decide d'envoyer, jamais le
        // fichier d'origine.
        //
        // `media.file_path` porte `tracks.file_path` — le fichier brut de la
        // bibliotheque. L'orchestrateur le renseigne pour TOUTE piste locale
        // (`local_file_path`, orchestrator.rs) sans regarder la sortie : c'est
        // une commodite pour les sorties qui savent lire un fichier
        // elles-memes (OAAT), pas une instruction de lecture.
        //
        // Le preferer jetait tout le traitement serveur. Car ce traitement est
        // bel et bien produit pour une zone AirPlay 2 :
        // `pull_output_needs_dsp_transcode` (orchestrator.rs) est une liste
        // NEGATIVE — ni locale, ni OAAT, ni pousseuse d'URI, ni navigateur —
        // et `airplay2` y tombe. Des qu'un egaliseur, une correction de piece
        // ou un ReplayGain sont armes sur la zone, `eq_forces_transcode` vaut
        // donc vrai : le serveur decode, filtre, gaine, reencode vers un
        // temporaire, ouvre une session et en publie l'adresse dans
        // `media.url` — en sautant meme le cache de transcodage, puisque l'EQ
        // n'entre pas dans sa clef. Cette ligne ignorait tout cela et rejouait
        // le fichier d'origine.
        //
        // Ecran « egaliseur actif », zero effet audible : exactement #1216
        // (Beoplay A9), une quatrieme fois apres les zones navigateur et les
        // sorties PULL type Diretta.
        //
        // Le daemon recoit deja une adresse HTTP sur toute source NON locale —
        // Qobuz, Tidal, radio, podcast, Bandcamp, televersement — ou
        // `file_path` vaut None. Lui en donner une pour une piste locale ne
        // lui demande donc rien de nouveau, et aligne AirPlay 2 sur les autres
        // sorties POUSSEES du depot — DLNA, OpenHome, Chromecast, BluOS,
        // Squeezebox, Slimproto, AirPlay 1, HQPlayer, pont — qui ne lisent
        // toutes que `media.url`. Seul OAAT lit encore le fichier lui-meme,
        // et c'est assume : il le fait derriere ses propres gardes
        // (`prefers_local_file_gapless`, en-tete WAV ou `.dsf` natif).
        //
        // ... sauf que le daemon ne sait PAS lire une adresse HTTP (#2169).
        // Sa commande `play` passe `path` tel quel a `AudioDecoder::open`, qui
        // fait un `File::open` : une adresse y devient un nom de fichier
        // introuvable, d'ou « decode error: ... Failed to open file: No such
        // file or directory (os error 2) » sur TOUTE piste, locale comme
        // Tidal. Le flux est donc d'abord recopie dans un fichier local, et
        // c'est ce fichier que le daemon ouvre : le traitement serveur de
        // #1216 est conserve, puisque c'est bien le flux servi qui est copie.
        let path = lisible.chemin.to_string_lossy().into_owned();
        if let Err(error) = self
            .send(&serde_json::json!({
                "cmd": "play",
                "path": path,
            }))
            .await
        {
            lisible.oublier().await;
            return Err(error);
        }
        let precedente = std::mem::replace(
            &mut *self.copie_du_flux.lock().await,
            lisible.copie.then_some(lisible.chemin),
        );
        if let Some(ancienne) = precedente {
            tokio::fs::remove_file(&ancienne).await.ok();
        }

        self.playing.store(true, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
        info!(device = %self.name, title = %title, "airplay2: play_media");
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.send_transport_command("pause", TransportConfirmation::Paused)
            .await?;
        info!(device = %self.name, "airplay2: pause");
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.send_transport_command("resume", TransportConfirmation::Playing)
            .await?;
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
        if let Some(mut daemon) = proc.take()
            && let Some(child) = daemon.child.as_mut()
        {
            child.kill().await.ok();
        }
        drop(proc);
        if let Some(copie) = self.copie_du_flux.lock().await.take() {
            tokio::fs::remove_file(&copie).await.ok();
        }
        info!(device = %self.name, "airplay2: stop");
        Ok(())
    }

    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Err("AirPlay 2 does not support seeking".into())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        let volume = volume.clamp(0.0, 1.0);
        self.send(&serde_json::json!({
            "cmd": "volume",
            "level": volume,
        }))
        .await?;
        *self.volume.lock().await = volume;
        self.muted.store(false, Ordering::SeqCst);
        Ok(())
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
            current_uri: None,
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
        tokio::net::TcpStream::connect(format!("{}:{}", self.host, self.port))
            .await
            .is_ok()
    }
}

/// Taille au-dela de laquelle la copie locale du flux est abandonnee.
///
/// Une piste DSD512 de vingt minutes transcodee en PCM tient largement
/// dessous ; un flux qui la depasse n'a pas de fin et remplirait le disque.
const COPIE_DU_FLUX_MAX_OCTETS: u64 = 4 * 1024 * 1024 * 1024;

/// Ce que le daemon recevra dans `path`.
struct CheminLisible {
    chemin: PathBuf,
    /// `true` quand Tune a fabrique ce fichier et doit le retirer ensuite.
    copie: bool,
}

impl CheminLisible {
    /// Retire la copie si la lecture n'a finalement pas ete lancee.
    async fn oublier(self) {
        if self.copie {
            tokio::fs::remove_file(&self.chemin).await.ok();
        }
    }
}

fn est_une_adresse_http(url: &str) -> bool {
    let debut = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    debut.starts_with("http://") || debut.starts_with("https://")
}

/// Dossier des copies locales de flux : la racine de travail du compte qui
/// execute (#4770), jamais un nom fixe partage sous le dossier temporaire.
fn dossier_des_copies() -> PathBuf {
    crate::chemins_de_travail::racine_de_travail("tune-airplay2")
}

/// Extension qui sert d'indice de format au daemon (`Hint::with_extension`).
fn extension_du_flux(mime_type: &str, url: &str) -> &'static str {
    let mime = mime_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match mime.as_str() {
        "audio/flac" | "audio/x-flac" => return "flac",
        "audio/wav" | "audio/wave" | "audio/x-wav" | "audio/vnd.wave" => return "wav",
        "audio/mpeg" | "audio/mp3" => return "mp3",
        "audio/aac" | "audio/aacp" => return "aac",
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => return "m4a",
        "audio/ogg" | "audio/vorbis" | "audio/opus" => return "ogg",
        "audio/aiff" | "audio/x-aiff" => return "aiff",
        _ => {}
    }
    let chemin = url.split(['?', '#']).next().unwrap_or("");
    let ext = Path::new(chemin)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("flac") => "flac",
        Some("wav") => "wav",
        Some("mp3") => "mp3",
        Some("aac") => "aac",
        Some("m4a" | "mp4" | "alac") => "m4a",
        Some("ogg" | "oga" | "opus") => "ogg",
        Some("aif" | "aiff") => "aiff",
        _ => "bin",
    }
}

/// Rend un chemin que `airplay-daemon` sait ouvrir (#2169).
///
/// Le daemon (`crates/airplay-daemon`, commande `play`) ne lit que des
/// fichiers locaux : `AudioDecoder::open(path)` fait un `File::open`. Une
/// adresse HTTP est donc recopiee dans un fichier local au prealable ; un
/// chemin local passe tel quel.
async fn chemin_lisible_par_le_daemon(
    media: &PlayMedia<'_>,
    device_id: &str,
) -> Result<CheminLisible, String> {
    if !est_une_adresse_http(media.url) {
        return Ok(CheminLisible {
            chemin: PathBuf::from(media.url),
            copie: false,
        });
    }
    if media.live_stream {
        return Err("airplay2: live streams (internet radio) cannot be played \
                    through the AirPlay 2 sender, which only reads complete \
                    files; use the AirPlay (1) output of this speaker instead"
            .into());
    }
    let dossier = dossier_des_copies();
    copier_le_flux(
        media.url,
        &dossier,
        device_id,
        extension_du_flux(media.mime_type, media.url),
    )
    .await
    .map(|chemin| CheminLisible {
        chemin,
        copie: true,
    })
}

/// Recopie le flux `url` dans `dossier`, morceau par morceau, et rend le
/// chemin du fichier obtenu.
async fn copier_le_flux(
    url: &str,
    dossier: &Path,
    device_id: &str,
    extension: &str,
) -> Result<PathBuf, String> {
    use std::sync::atomic::AtomicU64;
    static NUMERO: AtomicU64 = AtomicU64::new(0);

    tokio::fs::create_dir_all(dossier).await.map_err(|e| {
        format!(
            "airplay2: cannot create the folder for the local copy of the stream ({}): {e}",
            dossier.display()
        )
    })?;
    let appareil: String = device_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let chemin = dossier.join(format!(
        "{appareil}-{}-{}.{extension}",
        std::process::id(),
        NUMERO.fetch_add(1, Ordering::Relaxed)
    ));

    let copie = async {
        let mut reponse = crate::http::client::long_timeout()
            .get(url)
            .send()
            .await
            .map_err(|e| crate::http::client::decrire_erreur_http(&e))?;
        let statut = reponse.status();
        if !statut.is_success() {
            return Err(format!("HTTP {statut}"));
        }
        let mut fichier = tokio::fs::File::create(&chemin)
            .await
            .map_err(|e| format!("cannot create {}: {e}", chemin.display()))?;
        let mut total: u64 = 0;
        while let Some(morceau) = reponse
            .chunk()
            .await
            .map_err(|e| crate::http::client::decrire_erreur_http(&e))?
        {
            total += morceau.len() as u64;
            if total > COPIE_DU_FLUX_MAX_OCTETS {
                return Err(format!(
                    "stream exceeds {} bytes, it looks endless",
                    COPIE_DU_FLUX_MAX_OCTETS
                ));
            }
            fichier
                .write_all(&morceau)
                .await
                .map_err(|e| format!("cannot write {}: {e}", chemin.display()))?;
        }
        fichier
            .flush()
            .await
            .map_err(|e| format!("cannot write {}: {e}", chemin.display()))?;
        if total == 0 {
            return Err("the server sent an empty stream".to_string());
        }
        debug!(octets = total, chemin = %chemin.display(), "airplay2: stream copied for the daemon");
        Ok(())
    }
    .await;

    match copie {
        Ok(()) => Ok(chemin),
        Err(cause) => {
            tokio::fs::remove_file(&chemin).await.ok();
            Err(format!(
                "airplay2: cannot make a local copy of the stream for the AirPlay 2 sender: {cause}"
            ))
        }
    }
}

/// Platform-correct daemon filename (`airplay-daemon` or `airplay-daemon.exe`).
fn daemon_exe_name() -> String {
    format!("{DAEMON_BINARY}{}", std::env::consts::EXE_SUFFIX)
}

/// A daemon candidate counts only if it is a real, non-empty file. The arm64
/// Docker image ships a **0-byte placeholder** (`touch dist/arm64/airplay-daemon`)
/// so AirPlay 2 is meant to fall back to legacy AirPlay 1 there. An existence-only
/// check would pick that empty file, then fail to exec it at playback time —
/// breaking AirPlay with NO fallback (worse than legacy). Requiring a non-zero
/// size makes `daemon_available()` correctly report "no daemon" on arm64. (#700)
fn is_usable_daemon(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.len() > 0)
        .unwrap_or(false)
}

/// Resolve the daemon binary given the directory of the running executable.
/// Pure (no PATH lookup) so it can be unit-tested. Checks, in order:
///   1. next to the tune-server executable — how the release archives bundle it,
///      wherever the user extracted the zip/tar;
///   2. well-known absolute install locations (Docker image, manual installs);
///   3. the current working directory (legacy behaviour).
///
/// Returns None if not found on disk (caller then falls back to a PATH probe).
fn resolve_daemon_path(exe_dir: Option<&std::path::Path>, exe_name: &str) -> Option<String> {
    if let Some(dir) = exe_dir {
        let candidate = dir.join(exe_name);
        if is_usable_daemon(&candidate) {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    for abs in [
        format!("/usr/local/bin/{exe_name}"),
        format!("/opt/tune-server/{exe_name}"),
    ] {
        if is_usable_daemon(std::path::Path::new(&abs)) {
            return Some(abs);
        }
    }
    if is_usable_daemon(std::path::Path::new(exe_name)) {
        return Some(exe_name.to_string());
    }
    None
}

/// PATH lookup using the platform locator (`where` on Windows, `which` elsewhere).
fn which_daemon(exe_name: &str) -> Option<String> {
    let locator = if cfg!(windows) { "where" } else { "which" };
    let output = std::process::Command::new(locator)
        .arg(exe_name)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    (!path.is_empty()).then_some(path)
}

fn find_daemon_binary() -> String {
    let exe_name = daemon_exe_name();
    let exe = std::env::current_exe().ok();
    let exe_dir = exe.as_deref().and_then(|p| p.parent());
    resolve_daemon_path(exe_dir, &exe_name)
        .or_else(|| which_daemon(&exe_name))
        .unwrap_or(exe_name)
}

/// Check if the airplay-daemon binary is available on this system.
pub fn daemon_available() -> bool {
    let exe_name = daemon_exe_name();
    let exe = std::env::current_exe().ok();
    let exe_dir = exe.as_deref().and_then(|p| p.parent());
    resolve_daemon_path(exe_dir, &exe_name).is_some() || which_daemon(&exe_name).is_some()
}

#[cfg(test)]
mod transport_tests {
    use super::*;

    fn output_for_test() -> Airplay2Output {
        Airplay2Output::new(
            "AirPlay 2 test".into(),
            "127.0.0.1".into(),
            7000,
            "fixture".into(),
            "00:11:22:33:44:55".into(),
        )
    }

    async fn install_fake_daemon(
        output: &Airplay2Output,
        fail_resume: bool,
    ) -> Arc<Mutex<Vec<String>>> {
        let (server_stdin, daemon_stdin) = tokio::io::duplex(4096);
        let (mut daemon_stdout, server_stdout) = tokio::io::duplex(4096);

        *output.process.lock().await = Some(DaemonProcess {
            child: None,
            stdin: Box::new(server_stdin),
        });
        output.start_event_reader(BufReader::new(server_stdout));

        let commands = Arc::new(Mutex::new(Vec::new()));
        let recorded = commands.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(daemon_stdin).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let command: serde_json::Value = serde_json::from_str(&line).unwrap();
                let name = command["cmd"].as_str().unwrap().to_string();
                recorded.lock().await.push(name.clone());

                let event = match name.as_str() {
                    "play" => serde_json::json!({"event": "playing"}),
                    "pause" => serde_json::json!({"event": "paused"}),
                    "resume" if fail_resume => serde_json::json!({
                        "event": "error",
                        "message": "resume failed: fixture",
                    }),
                    "resume" => serde_json::json!({"event": "playing"}),
                    other => serde_json::json!({
                        "event": "error",
                        "message": format!("unexpected command: {other}"),
                    }),
                };
                let line = format!("{}\n", serde_json::to_string(&event).unwrap());
                daemon_stdout.write_all(line.as_bytes()).await.unwrap();
            }
        });

        commands
    }

    fn media_for_test() -> PlayMedia<'static> {
        PlayMedia {
            url: "/tmp/fixture.flac",
            mime_type: "audio/flac",
            title: Some("Fixture"),
            ..Default::default()
        }
    }

    /// Un faux daemon qui note la charge JSON ENTIERE, pas seulement le nom.
    ///
    /// `install_fake_daemon` ne retient que `cmd` : il ne peut donc rien dire
    /// du `path`, qui est precisement ce que la garde ci-dessous mesure.
    async fn daemon_qui_note_les_charges(
        output: &Airplay2Output,
    ) -> Arc<Mutex<Vec<serde_json::Value>>> {
        let (server_stdin, daemon_stdin) = tokio::io::duplex(4096);
        let (mut daemon_stdout, server_stdout) = tokio::io::duplex(4096);

        *output.process.lock().await = Some(DaemonProcess {
            child: None,
            stdin: Box::new(server_stdin),
        });
        output.start_event_reader(BufReader::new(server_stdout));

        let charges = Arc::new(Mutex::new(Vec::new()));
        let notees = charges.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(daemon_stdin).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let command: serde_json::Value = serde_json::from_str(&line).unwrap();
                notees.lock().await.push(command.clone());
                let event = serde_json::json!({"event": "playing"});
                let line = format!("{}\n", serde_json::to_string(&event).unwrap());
                daemon_stdout.write_all(line.as_bytes()).await.unwrap();
            }
        });

        charges
    }

    /// Un serveur HTTP local qui sert `contenu` sur `/stream/sess.flac`,
    /// comme une session de flux du serveur. Rend l'adresse du flux.
    async fn serveur_de_flux(contenu: &'static [u8]) -> String {
        let app = axum::Router::new().route(
            "/stream/sess.flac",
            axum::routing::get(move || async move { contenu }),
        );
        let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let adresse = ecoute.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(ecoute, app).await.ok();
        });
        format!("http://{adresse}/stream/sess.flac")
    }

    async fn commande_play(
        charges: &Arc<Mutex<Vec<serde_json::Value>>>,
    ) -> Option<serde_json::Value> {
        // `play_media` n'attend AUCUNE confirmation : il ecrit sur le tube et
        // rend la main aussitot. On laisse au faux daemon le temps de noter
        // la ligne, sans dormir plus qu'il ne faut.
        for _ in 0..200 {
            let play = charges
                .lock()
                .await
                .iter()
                .find(|c| c["cmd"] == "play")
                .cloned();
            if play.is_some() {
                return play;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        None
    }

    /// Le daemon recoit un FICHIER qu'il peut ouvrir, qui porte le FLUX servi
    /// par le serveur, et jamais le fichier d'origine (#2169, #1216).
    ///
    /// `airplay-daemon` passe `path` a `AudioDecoder::open`, soit un
    /// `File::open`. Le `std::fs::read` ci-dessous rejoue exactement ce geste.
    /// Avant le correctif, `path` valait l'adresse HTTP du flux : ce read
    /// echouait sur « No such file or directory (os error 2) », l'erreur
    /// meme que le daemon renvoie pour chaque piste, locale ou Tidal.
    ///
    /// `file_path` est renseigne comme le fait l'orchestrateur pour toute
    /// piste locale : si la sortie repasse au fichier d'origine, le
    /// traitement serveur (egaliseur, ReplayGain) est jete, et la garde tombe
    /// aussi.
    #[tokio::test]
    async fn play_media_donne_au_daemon_un_fichier_qu_il_sait_ouvrir_et_qui_porte_le_flux() {
        const CONTENU: &[u8] = b"fLaC-flux-traite-par-le-serveur";
        const ORIGINE: &str = "/srv/musique/album/piste-egalisee.flac";
        let flux = serveur_de_flux(CONTENU).await;

        let output = output_for_test();
        let charges = daemon_qui_note_les_charges(&output).await;

        output
            .play_media(&PlayMedia {
                url: &flux,
                mime_type: "audio/flac",
                title: Some("Fixture"),
                file_path: Some(ORIGINE),
                ..Default::default()
            })
            .await
            .unwrap();

        let play = commande_play(&charges)
            .await
            .expect("aucune commande play envoyee au daemon");
        let path = play["path"].as_str().expect("path absent").to_string();

        assert_ne!(
            path, ORIGINE,
            "AirPlay 2 a rejoue le fichier d'origine : tout le DSP est jete (#1216)"
        );
        let lu = std::fs::read(&path).unwrap_or_else(|e| {
            panic!("le daemon ne pourra pas ouvrir `{path}` ({e}) : c'est #2169")
        });
        assert_eq!(
            lu, CONTENU,
            "le fichier remis au daemon doit porter le flux servi"
        );
        assert!(
            path.ends_with(".flac"),
            "l'extension guide le daemon : {path}"
        );

        output.stop().await.unwrap();
        assert!(
            !std::path::Path::new(&path).exists(),
            "la copie du flux doit etre retiree a l'arret"
        );
    }

    /// La piste suivante remplace la copie precedente, qui est retiree.
    #[tokio::test]
    async fn la_piste_suivante_retire_la_copie_precedente() {
        let flux = serveur_de_flux(b"piste").await;
        let output = output_for_test();
        let charges = daemon_qui_note_les_charges(&output).await;
        let media = PlayMedia {
            url: &flux,
            mime_type: "audio/flac",
            ..Default::default()
        };

        output.play_media(&media).await.unwrap();
        let premiere = commande_play(&charges).await.unwrap()["path"]
            .as_str()
            .unwrap()
            .to_string();
        charges.lock().await.clear();
        output.play_media(&media).await.unwrap();
        let seconde = commande_play(&charges).await.unwrap()["path"]
            .as_str()
            .unwrap()
            .to_string();

        assert_ne!(premiere, seconde);
        assert!(!std::path::Path::new(&premiere).exists());
        assert!(std::path::Path::new(&seconde).exists());
        output.stop().await.unwrap();
    }

    /// Un flux sans fin (radio) est refuse avec un message qui dit quoi faire,
    /// au lieu de partir vers le daemon et d'y echouer en « os error 2 ».
    #[tokio::test]
    async fn un_flux_en_direct_est_refuse_clairement() {
        let output = output_for_test();
        let charges = daemon_qui_note_les_charges(&output).await;

        let erreur = output
            .play_media(&PlayMedia {
                url: "http://127.0.0.1:9/radio",
                mime_type: "audio/mpeg",
                live_stream: true,
                ..Default::default()
            })
            .await
            .unwrap_err();

        assert!(erreur.contains("live streams"), "{erreur}");
        assert!(erreur.contains("AirPlay (1)"), "{erreur}");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(charges.lock().await.iter().all(|c| c["cmd"] != "play"));
    }

    /// Un flux injoignable rend une erreur qui nomme la copie locale, pas
    /// l'« os error 2 » du daemon.
    #[tokio::test]
    async fn un_flux_injoignable_donne_une_erreur_claire() {
        let output = output_for_test();
        let _charges = daemon_qui_note_les_charges(&output).await;
        let erreur = output
            .play_media(&PlayMedia {
                url: "http://127.0.0.1:9/stream/absent.flac",
                mime_type: "audio/flac",
                ..Default::default()
            })
            .await
            .unwrap_err();
        assert!(erreur.contains("local copy of the stream"), "{erreur}");
        assert!(
            !erreur.contains("127.0.0.1"),
            "pas d'adresse dans l'erreur : {erreur}"
        );
    }

    #[test]
    fn l_extension_suit_le_type_mime_puis_l_adresse() {
        assert_eq!(extension_du_flux("audio/flac", "http://h/x"), "flac");
        assert_eq!(
            extension_du_flux("audio/wav; rate=44100", "http://h/x"),
            "wav"
        );
        assert_eq!(extension_du_flux("", "http://h/s/1.mp3?t=2"), "mp3");
        assert_eq!(
            extension_du_flux("application/octet-stream", "http://h/s"),
            "bin"
        );
        assert!(est_une_adresse_http("HTTP://h/x"));
        assert!(!est_une_adresse_http("/srv/musique/x.flac"));
    }

    #[tokio::test]
    async fn play_pause_resume_commands_are_confirmed_by_the_daemon() {
        let output = output_for_test();
        let commands = install_fake_daemon(&output, false).await;

        assert!(output.capabilities().can_pause);
        assert!(output.capabilities().can_resume);

        output.play_media(&media_for_test()).await.unwrap();
        output.pause().await.unwrap();
        assert_eq!(
            output.get_status().await.unwrap().state,
            TransportState::Paused
        );

        output.resume().await.unwrap();
        assert_eq!(
            output.get_status().await.unwrap().state,
            TransportState::Playing
        );
        assert_eq!(*commands.lock().await, vec!["play", "pause", "resume"]);
    }

    #[tokio::test]
    async fn resume_error_keeps_the_published_state_paused() {
        let output = output_for_test();
        let commands = install_fake_daemon(&output, true).await;

        output.play_media(&media_for_test()).await.unwrap();
        output.pause().await.unwrap();
        let error = output.resume().await.unwrap_err();

        assert!(error.contains("resume failed: fixture"));
        assert_eq!(
            output.get_status().await.unwrap().state,
            TransportState::Paused
        );
        assert_eq!(*commands.lock().await, vec!["play", "pause", "resume"]);
    }
}

#[cfg(test)]
mod daemon_path_tests {
    use super::*;

    #[test]
    fn resolves_daemon_bundled_next_to_executable() {
        // The primary native-install path: the daemon sits in the same directory
        // as the tune-server binary, wherever the archive was extracted.
        let dir = crate::test_scratch::scratch_dir("tune_daemon_test");
        let exe_name = daemon_exe_name();
        let bin = dir.join(&exe_name);
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();

        let found = resolve_daemon_path(Some(&dir), &exe_name);
        assert_eq!(found.as_deref(), Some(bin.to_string_lossy().as_ref()));

        // No exe dir + not in CWD/system dirs → None (caller falls back to PATH).
        std::fs::remove_file(&bin).unwrap();
        assert_eq!(resolve_daemon_path(Some(&dir), &exe_name), None);
    }

    #[test]
    fn empty_placeholder_is_not_resolved() {
        // arm64 Docker ships a 0-byte placeholder so AirPlay 2 falls back to
        // legacy. An existence-only check would pick it and fail to exec (#700):
        // a zero-length candidate must be treated as "no daemon".
        let dir = crate::test_scratch::scratch_dir("tune_daemon_empty_test");
        let exe_name = daemon_exe_name();
        let bin = dir.join(&exe_name);
        std::fs::write(&bin, b"").unwrap(); // 0 bytes, like `touch`

        assert!(!is_usable_daemon(&bin));
        assert_eq!(resolve_daemon_path(Some(&dir), &exe_name), None);
    }

    #[test]
    fn daemon_name_has_platform_exe_suffix() {
        let name = daemon_exe_name();
        assert!(name.starts_with("airplay-daemon"));
        assert!(name.ends_with(std::env::consts::EXE_SUFFIX));
    }
}
