//! Le récepteur Spotify Connect de Tune : librespot en sous-processus.
//!
//! #6018 (Dimitri, fil 2192) — librespot était lancé avec `--backend pipe`,
//! mais personne ne lisait sa sortie standard : le PCM n'atteignait aucune
//! zone, et librespot se bloquait dès que le tuyau était plein. La
//! [`PompePcm`] lit désormais cette sortie EN PERMANENCE :
//!
//! * un consommateur attaché (une lecture Spotify lancée par Tune, voir
//!   `streaming::spotify_lecture`) reçoit le PCM avec contre-pression — c'est
//!   la sortie de la zone qui cadence librespot, comme le ferait une carte son ;
//! * sans consommateur, les octets sont jetés AU RYTHME DU TEMPS RÉEL : librespot
//!   ne se bloque plus, et ne traverse pas non plus les titres en accéléré.
//!
//! Le format est celui du backend `pipe` de librespot : PCM S16LE, 44,1 kHz,
//! stéréo. Aucun décodage, aucun ffmpeg : les octets passent tels quels.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, mpsc};
use tracing::{debug, info, warn};

const PCM_SAMPLE_RATE: u32 = 44100;
const PCM_CHANNELS: u16 = 2;
const PCM_BITS_PER_SAMPLE: u16 = 16;
const DEFAULT_BITRATE: u32 = 320;

/// Octets d'une trame stéréo 16 bits.
pub const OCTETS_PAR_TRAME: usize = 4;
/// Débit du PCM de librespot : 44 100 trames de 4 octets par seconde.
pub const OCTETS_PAR_SECONDE: usize = 44_100 * OCTETS_PAR_TRAME;
/// Taille d'un bloc remis au consommateur (trames entières).
const BLOC: usize = 32 * 1024;
/// Profondeur du canal du consommateur, en blocs (~1,5 s de PCM).
const CANAL: usize = 8;
/// Après `end_of_track`, la pompe vide le tuyau puis ferme le flux dès que
/// librespot s'est tu pendant ce délai.
const SILENCE_DE_FIN: Duration = Duration::from_millis(300);

/// Préfixe des lignes écrites sur stderr par le script `--onevent`.
pub const PREFIXE_EVENEMENT: &str = "TUNE_EVENT\t";

/// Un évènement de lecteur publié par librespot (`--onevent`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvenementLibrespot {
    /// `PLAYER_EVENT` : `track_changed`, `playing`, `paused`, `end_of_track`,
    /// `stopped`…
    pub evenement: String,
    /// `TRACK_ID` : l'identifiant base62 du titre, celui de l'API Web.
    pub piste: Option<String>,
    pub position_ms: Option<u64>,
    pub duree_ms: Option<u64>,
    pub titre: Option<String>,
    pub artistes: Option<String>,
    pub album: Option<String>,
    pub pochette: Option<String>,
}

impl EvenementLibrespot {
    /// L'évènement clôt-il le titre `piste` ?
    pub fn termine(&self) -> bool {
        matches!(self.evenement.as_str(), "end_of_track" | "stopped")
    }
}

/// Le script passé à `--onevent`. librespot l'exécute sans shell, avec les
/// variables d'environnement de l'évènement, et le laisse hériter de SA
/// sortie d'erreur — que Tune lit déjà. Une ligne par évènement, champs
/// séparés par des tabulations (retirées des valeurs).
pub fn script_onevent() -> String {
    // `c` : une valeur sur une ligne, sans tabulation. `l` : une liste
    // (une valeur par ligne chez librespot) jointe par des virgules.
    r#"#!/bin/sh
# Ecrit par Tune (#6018) : relaie un evenement de librespot sur stderr.
c() { printf '%s' "$1" | tr '\t\r\n' '   '; }
l() { printf '%s\n' "$1" | awk 'NF { if (n++) printf ", "; printf "%s", $0 }' | tr '\t\r' '  '; }
printf 'TUNE_EVENT\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
  "$(c "$PLAYER_EVENT")" "$(c "$TRACK_ID")" "$(c "$POSITION_MS")" "$(c "$DURATION_MS")" \
  "$(c "$NAME")" "$(l "$ARTISTS")" "$(c "$ALBUM")" "$(printf '%s\n' "$COVERS" | head -n 1 | tr '\t\r' '  ')" >&2
"#
    .to_string()
}

/// Lit une ligne écrite par [`script_onevent`].
pub fn lire_evenement_tune(ligne: &str) -> Option<EvenementLibrespot> {
    let reste = ligne.strip_prefix(PREFIXE_EVENEMENT)?;
    let mut champs = reste.split('\t').map(|c| {
        let c = c.trim();
        (!c.is_empty()).then(|| c.to_string())
    });
    let mut suivant = || champs.next().flatten();
    let evenement = suivant()?;
    Some(EvenementLibrespot {
        evenement,
        piste: suivant(),
        position_ms: suivant().and_then(|v| v.parse().ok()),
        duree_ms: suivant().and_then(|v| v.parse().ok()),
        titre: suivant(),
        artistes: suivant(),
        album: suivant(),
        pochette: suivant(),
    })
}

/// La pompe du PCM de librespot. Voir l'en-tête du module.
pub struct PompePcm {
    consommateur: std::sync::Mutex<Option<Consommateur>>,
    generation: AtomicU64,
    fin_demandee: AtomicBool,
}

struct Consommateur {
    generation: u64,
    piste: Option<String>,
    tx: mpsc::Sender<Vec<u8>>,
}

impl PompePcm {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            consommateur: std::sync::Mutex::new(None),
            generation: AtomicU64::new(0),
            fin_demandee: AtomicBool::new(false),
        })
    }

    /// Attache un consommateur, qui REMPLACE le précédent (son canal se
    /// ferme). `piste` : le titre attendu, pour que la fin d'un AUTRE titre
    /// ne ferme pas ce flux-ci.
    pub fn attacher(&self, piste: Option<String>) -> mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = mpsc::channel(CANAL);
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.fin_demandee.store(false, Ordering::SeqCst);
        if let Ok(mut c) = self.consommateur.lock() {
            *c = Some(Consommateur {
                generation,
                piste,
                tx,
            });
        }
        rx
    }

    /// Détache le consommateur courant : son canal se ferme.
    pub fn detacher(&self) {
        if let Ok(mut c) = self.consommateur.lock() {
            *c = None;
        }
    }

    fn detacher_si(&self, generation: u64) {
        if let Ok(mut c) = self.consommateur.lock()
            && c.as_ref().is_some_and(|c| c.generation == generation)
        {
            *c = None;
        }
    }

    pub fn a_un_consommateur(&self) -> bool {
        self.consommateur
            .lock()
            .map(|c| c.is_some())
            .unwrap_or(false)
    }

    /// librespot annonce la fin d'un titre. Elle ne vaut que pour le
    /// consommateur qui attend CE titre.
    pub fn signaler_fin(&self, piste: Option<&str>) {
        let concerne = self
            .consommateur
            .lock()
            .map(|c| {
                c.as_ref()
                    .is_some_and(|c| c.piste.is_none() || c.piste.as_deref() == piste)
            })
            .unwrap_or(false);
        if concerne {
            self.fin_demandee.store(true, Ordering::SeqCst);
        }
    }

    /// Lance la lecture de `lecteur` (la sortie standard de librespot).
    pub fn lancer<R>(self: &Arc<Self>, lecteur: R) -> tokio::task::JoinHandle<()>
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let pompe = self.clone();
        tokio::spawn(async move {
            let mut lecteur = lecteur;
            let mut tampon = vec![0u8; BLOC];
            let mut reste: Vec<u8> = Vec::with_capacity(BLOC + OCTETS_PAR_TRAME);
            loop {
                let lu = match tokio::time::timeout(SILENCE_DE_FIN, lecteur.read(&mut tampon)).await
                {
                    // librespot se tait : si la fin du titre attendu est
                    // annoncée, tout ce qu'il a écrit est passé — on ferme.
                    Err(_) => {
                        if pompe.fin_demandee.swap(false, Ordering::SeqCst) {
                            debug!("librespot_pompe_fin_du_titre");
                            pompe.detacher();
                        }
                        continue;
                    }
                    Ok(Ok(0)) => {
                        info!("librespot_pompe_sortie_fermee");
                        pompe.detacher();
                        break;
                    }
                    Ok(Err(e)) => {
                        warn!(erreur = %e, "librespot_pompe_lecture_impossible");
                        pompe.detacher();
                        break;
                    }
                    Ok(Ok(n)) => n,
                };
                reste.extend_from_slice(&tampon[..lu]);
                // Des trames entières seulement : un bloc coupé au milieu
                // d'un échantillon décalerait tout le reste du flux.
                let entier = reste.len() - reste.len() % OCTETS_PAR_TRAME;
                if entier == 0 {
                    continue;
                }
                let bloc: Vec<u8> = reste.drain(..entier).collect();
                let cible = pompe
                    .consommateur
                    .lock()
                    .ok()
                    .and_then(|c| c.as_ref().map(|c| (c.generation, c.tx.clone())));
                match cible {
                    // Contre-pression : la sortie de la zone cadence librespot.
                    Some((generation, tx)) => {
                        if tx.send(bloc).await.is_err() {
                            pompe.detacher_si(generation);
                        }
                    }
                    // Personne n'écoute : jeté au rythme d'une carte son.
                    None => {
                        let micros = bloc.len() as u64 * 1_000_000 / OCTETS_PAR_SECONDE as u64;
                        tokio::time::sleep(Duration::from_micros(micros)).await;
                    }
                }
            }
        })
    }
}

/// Cherche librespot : `TUNE_LIBRESPOT`, puis le `PATH`, puis les dossiers
/// d'installation usuels. Un serveur lancé par launchd n'a pas
/// `/opt/homebrew/bin` dans son `PATH` (#6018, point 3).
pub fn chemin_du_binaire() -> Option<PathBuf> {
    let explicite = std::env::var_os("TUNE_LIBRESPOT").map(PathBuf::from);
    let mut candidats = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        candidats.push(Path::new(&home).join(".cargo/bin"));
    }
    chercher_librespot(
        explicite.as_deref(),
        std::env::var_os("PATH").as_deref(),
        &candidats,
    )
}

/// Le cœur pur de [`chemin_du_binaire`].
pub fn chercher_librespot(
    explicite: Option<&Path>,
    path: Option<&std::ffi::OsStr>,
    dossiers_usuels: &[PathBuf],
) -> Option<PathBuf> {
    let nom = if cfg!(windows) {
        "librespot.exe"
    } else {
        "librespot"
    };
    if let Some(e) = explicite.filter(|e| est_executable(e)) {
        return Some(e.to_path_buf());
    }
    path.into_iter()
        .flat_map(std::env::split_paths)
        .chain(dossiers_usuels.iter().cloned())
        .map(|d| d.join(nom))
        .find(|c| est_executable(c))
}

fn est_executable(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

pub struct LibrespotDaemon {
    device_name: String,
    binary_path: Option<String>,
    bitrate: u32,
    process: Mutex<Option<Child>>,
    /// Le processus courant a-t-il été connecté au compte par un jeton ?
    connecte_par_jeton: AtomicBool,
    pompe: Arc<PompePcm>,
    /// Le dossier du script `--onevent`, propre à ce processus Tune.
    dossier_script: std::sync::Mutex<Option<tempfile::TempDir>>,
}

impl LibrespotDaemon {
    pub fn new(device_name: String, binary_path: Option<String>, bitrate: Option<u32>) -> Self {
        Self {
            device_name,
            binary_path,
            bitrate: bitrate.unwrap_or(DEFAULT_BITRATE),
            process: Mutex::new(None),
            connecte_par_jeton: AtomicBool::new(false),
            pompe: PompePcm::new(),
            dossier_script: std::sync::Mutex::new(None),
        }
    }

    pub fn pompe(&self) -> Arc<PompePcm> {
        self.pompe.clone()
    }

    fn binaire(&self) -> String {
        self.binary_path
            .clone()
            .or_else(|| chemin_du_binaire().map(|p| p.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "librespot".into())
    }

    /// Écrit le script `--onevent` (Unix). Le chemin ne doit contenir aucun
    /// espace : librespot découpe la commande sur les blancs.
    fn preparer_le_script(&self) -> Option<String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut garde = self.dossier_script.lock().ok()?;
            if garde.is_none() {
                // Un dossier PROPRE à ce processus : Shrek et les machines
                // partagées refusent un chemin temporaire à nom fixe.
                *garde = tempfile::Builder::new()
                    .prefix("tune-librespot-")
                    .tempdir()
                    .ok();
            }
            let chemin = garde.as_ref()?.path().join("onevent.sh");
            let texte = chemin.to_string_lossy().into_owned();
            if texte.contains(char::is_whitespace) {
                warn!(chemin = %texte, "librespot_onevent_chemin_avec_espace");
                return None;
            }
            std::fs::write(&chemin, script_onevent()).ok()?;
            std::fs::set_permissions(&chemin, std::fs::Permissions::from_mode(0o700)).ok()?;
            Some(texte)
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    pub async fn start<F>(&self, on_event: F) -> Result<(), String>
    where
        F: Fn(String, Option<String>) + Send + 'static,
    {
        self.demarrer(None, on_event).await
    }

    /// Démarre librespot ; avec un `jeton` OAuth, il se connecte au compte et
    /// devient pilotable par l'API Web (`/me/player/play?device_id=`).
    pub async fn demarrer<F>(&self, jeton: Option<&str>, on_event: F) -> Result<(), String>
    where
        F: Fn(String, Option<String>) + Send + 'static,
    {
        let bitrate = self.bitrate.to_string();
        let mut args: Vec<String> = vec![
            "--name".into(),
            self.device_name.clone(),
            "--bitrate".into(),
            bitrate,
            "--backend".into(),
            "pipe".into(),
            "--device-type".into(),
            "speaker".into(),
            "--disable-audio-cache".into(),
        ];
        // La file de Tune enchaîne les titres : Spotify n'en ajoute pas.
        args.extend(["--autoplay".into(), "off".into()]);
        if let Some(script) = self.preparer_le_script() {
            args.extend(["--onevent".into(), script]);
        }
        let binaire = self.binaire();
        let mut commande = Command::new(&binaire);
        // Le jeton OAuth ne passe JAMAIS par les arguments, que `ps` montre à
        // tous les comptes de la machine : librespot lit chaque option aussi
        // dans `LIBRESPOT_<OPTION>`, et masque celle-ci dans ses traces.
        match jeton {
            Some(jeton) => commande.env("LIBRESPOT_ACCESS_TOKEN", jeton),
            None => commande.env_remove("LIBRESPOT_ACCESS_TOKEN"),
        };
        let mut proc = commande
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    "librespot est introuvable : l'installer (macOS : brew install librespot) \
                     ou indiquer son chemin dans TUNE_LIBRESPOT. Spotify Premium est exigé."
                        .to_string()
                } else {
                    format!("librespot start ({binaire}): {e}")
                }
            })?;

        // #6018 — la sortie standard EST le son : elle est lue en permanence.
        if let Some(stdout) = proc.stdout.take() {
            self.pompe.lancer(stdout);
        }

        if let Some(stderr) = proc.stderr.take() {
            let pompe = self.pompe.clone();
            tokio::spawn(async move {
                let reader = tokio::io::BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(ev) = lire_evenement_tune(&line) {
                        info!(
                            event = %ev.evenement,
                            track_id = ?ev.piste,
                            titre = ?ev.titre,
                            "librespot_evenement"
                        );
                        if ev.termine() {
                            pompe.signaler_fin(ev.piste.as_deref());
                        }
                        on_event(ev.evenement, ev.piste);
                    } else if let Some((event, track_id)) = parse_librespot_event(&line) {
                        on_event(event, track_id);
                    } else {
                        debug!(line = %line, "librespot_stderr");
                    }
                }
            });
        }
        self.connecte_par_jeton
            .store(jeton.is_some(), Ordering::SeqCst);

        info!(device = %self.device_name, "librespot_started");
        *self.process.lock().await = Some(proc);
        Ok(())
    }

    pub async fn stop(&self) {
        let mut proc = self.process.lock().await;
        if let Some(mut child) = proc.take() {
            let _ = child.kill().await;
            info!("librespot_stopped");
        }
        self.connecte_par_jeton.store(false, Ordering::SeqCst);
    }

    pub async fn is_running(&self) -> bool {
        let mut proc = self.process.lock().await;
        match proc.as_mut() {
            Some(child) => child.try_wait().ok().flatten().is_none(),
            None => false,
        }
    }

    /// librespot tourne-t-il, connecté au compte par un jeton ?
    pub async fn connecte_au_compte(&self) -> bool {
        self.connecte_par_jeton.load(Ordering::SeqCst) && self.is_running().await
    }

    pub fn pcm_spec() -> (u32, u16, u16) {
        (PCM_SAMPLE_RATE, PCM_CHANNELS, PCM_BITS_PER_SAMPLE)
    }
}

fn parse_librespot_event(line: &str) -> Option<(String, Option<String>)> {
    let lower = line.to_lowercase();
    if !lower.contains("player_event") && !lower.contains("event") {
        return None;
    }

    let events = [
        "playing",
        "started",
        "changed",
        "track_changed",
        "session_connected",
        "stopped",
        "session_disconnected",
        "end_of_track",
        "paused",
    ];

    let event = events.iter().find(|e| lower.contains(*e))?;

    let track_id = line.find("track_id").and_then(|pos| {
        let after = &line[pos..];
        let start = after
            .find(|c: char| {
                c.is_alphanumeric()
                    && c != 't'
                    && c != 'r'
                    && c != 'a'
                    && c != 'c'
                    && c != 'k'
                    && c != '_'
                    && c != 'i'
                    && c != 'd'
            })
            .or_else(|| after.find('=').map(|p| p + 1))
            .or_else(|| after.find(':').map(|p| p + 1))?;
        let trimmed = after[start..].trim_start_matches(|c: char| !c.is_alphanumeric());
        let end = trimmed
            .find(|c: char| !c.is_alphanumeric())
            .unwrap_or(trimmed.len());
        if end > 0 {
            Some(trimmed[..end].to_string())
        } else {
            None
        }
    });

    Some((event.to_string(), track_id))
}

pub struct SpotifyConnectManager {
    daemon: Arc<LibrespotDaemon>,
    enabled: Mutex<bool>,
    zone_id: Mutex<Option<i64>>,
    device_name: String,
    relay_port: u16,
}

impl SpotifyConnectManager {
    pub fn new(device_name: String, relay_port: u16) -> Self {
        Self::avec_binaire(device_name, relay_port, None)
    }

    /// Comme [`Self::new`], avec un binaire imposé (tests : un faux librespot).
    pub fn avec_binaire(device_name: String, relay_port: u16, binaire: Option<String>) -> Self {
        Self {
            daemon: Arc::new(LibrespotDaemon::new(device_name.clone(), binaire, None)),
            enabled: Mutex::new(false),
            zone_id: Mutex::new(None),
            device_name,
            relay_port,
        }
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// La pompe du PCM de librespot (une seule par récepteur).
    pub fn pompe(&self) -> Arc<PompePcm> {
        self.daemon.pompe()
    }

    /// S'assure que librespot tourne ET qu'il est connecté au compte, pour
    /// que l'API Web puisse le désigner. Un récepteur lancé en Zeroconf seul
    /// (réglage « Spotify Connect ») est redémarré avec le jeton.
    pub async fn assurer_le_recepteur_connecte(&self, jeton: &str) -> Result<(), String> {
        if self.daemon.connecte_au_compte().await {
            return Ok(());
        }
        self.daemon.stop().await;
        self.daemon
            .demarrer(Some(jeton), |event, track_id| {
                info!(event = %event, track_id = ?track_id, "spotify_connect_event");
            })
            .await
    }

    pub async fn enable(&self, zone_id: i64) -> Result<(), String> {
        *self.zone_id.lock().await = Some(zone_id);
        *self.enabled.lock().await = true;
        if self.daemon.is_running().await {
            info!(zone_id, "spotify_connect_enabled_already_running");
            return Ok(());
        }
        self.daemon
            .start(|event, track_id| {
                info!(event = %event, track_id = ?track_id, "spotify_connect_event");
            })
            .await?;
        info!(zone_id, "spotify_connect_enabled");
        Ok(())
    }

    pub async fn disable(&self) {
        *self.enabled.lock().await = false;
        *self.zone_id.lock().await = None;
        self.daemon.stop().await;
        info!("spotify_connect_disabled");
    }

    pub async fn is_enabled(&self) -> bool {
        *self.enabled.lock().await
    }

    pub fn stream_url(&self, server_ip: &str) -> String {
        format!(
            "http://{}:{}/spotify-connect/stream.wav",
            server_ip, self.relay_port
        )
    }

    pub async fn status(&self) -> serde_json::Value {
        let enabled = *self.enabled.lock().await;
        let zone_id = *self.zone_id.lock().await;
        let running = self.daemon.is_running().await;
        serde_json::json!({
            "enabled": enabled,
            "device_name": &self.device_name,
            "zone_id": zone_id,
            "active": running,
            "binary_available": binary_available(),
        })
    }
}

pub fn binary_available() -> bool {
    chemin_du_binaire().is_some()
}

pub fn build_wav_header() -> Vec<u8> {
    let byte_rate = PCM_SAMPLE_RATE * PCM_CHANNELS as u32 * (PCM_BITS_PER_SAMPLE as u32 / 8);
    let block_align = PCM_CHANNELS * (PCM_BITS_PER_SAMPLE / 8);
    let mut header = Vec::with_capacity(44);
    header.extend_from_slice(b"RIFF");
    header.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // unknown size
    header.extend_from_slice(b"WAVE");
    header.extend_from_slice(b"fmt ");
    header.extend_from_slice(&16u32.to_le_bytes()); // chunk size
    header.extend_from_slice(&1u16.to_le_bytes()); // PCM
    header.extend_from_slice(&PCM_CHANNELS.to_le_bytes());
    header.extend_from_slice(&PCM_SAMPLE_RATE.to_le_bytes());
    header.extend_from_slice(&byte_rate.to_le_bytes());
    header.extend_from_slice(&block_align.to_le_bytes());
    header.extend_from_slice(&PCM_BITS_PER_SAMPLE.to_le_bytes());
    header.extend_from_slice(b"data");
    header.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()); // unknown size
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_size() {
        let h = build_wav_header();
        assert_eq!(h.len(), 44);
        assert_eq!(&h[0..4], b"RIFF");
        assert_eq!(&h[8..12], b"WAVE");
    }

    #[test]
    fn parse_event_playing() {
        let line = "player_event: playing track_id=abc123";
        let (event, tid) = parse_librespot_event(line).unwrap();
        assert_eq!(event, "playing");
        assert!(tid.is_some());
    }

    #[test]
    fn parse_event_paused() {
        let line = "Event: paused";
        let (event, _) = parse_librespot_event(line).unwrap();
        assert_eq!(event, "paused");
    }

    #[test]
    fn parse_no_event() {
        assert!(parse_librespot_event("some random log line").is_none());
    }

    #[test]
    fn pcm_spec() {
        let (sr, ch, bd) = LibrespotDaemon::pcm_spec();
        assert_eq!(sr, 44100);
        assert_eq!(ch, 2);
        assert_eq!(bd, 16);
    }

    /// #6018 — le défaut du ticket : sans lecteur de stdout, librespot se
    /// bloque dès que le tuyau est plein. La pompe doit vider la sortie même
    /// sans consommateur — au rythme du temps réel, pas plus vite.
    #[tokio::test]
    async fn pompe_6018_vide_la_sortie_meme_sans_consommateur() {
        let (mut ecrivain, lecteur) = tokio::io::duplex(64 * 1024);
        let pompe = PompePcm::new();
        let _tache = pompe.lancer(lecteur);
        // Une seconde de PCM : bien plus que le tampon du tuyau (64 Kio).
        let octets = OCTETS_PAR_SECONDE;
        let debut = std::time::Instant::now();
        tokio::time::timeout(Duration::from_secs(5), async {
            use tokio::io::AsyncWriteExt;
            ecrivain.write_all(&vec![7u8; octets]).await.unwrap();
        })
        .await
        .expect("librespot ne doit plus se bloquer sur un tuyau plein (#6018)");
        // Jeté au rythme du temps réel : ~0,6 s pour vider ce qui dépasse
        // le tampon du tuyau.
        assert!(
            debut.elapsed() >= Duration::from_millis(300),
            "sans consommateur, la pompe ne doit pas avaler le flux en accéléré ({:?})",
            debut.elapsed()
        );
    }

    /// Un consommateur attaché reçoit les octets EXACTS, en trames entières.
    #[tokio::test]
    async fn pompe_6018_remet_le_pcm_au_consommateur() {
        let (mut ecrivain, lecteur) = tokio::io::duplex(64 * 1024);
        let pompe = PompePcm::new();
        let mut rx = pompe.attacher(Some("piste".into()));
        let _tache = pompe.lancer(lecteur);
        let envoi = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let donnees: Vec<u8> = (0..100_003u32).map(|i| (i % 251) as u8).collect();
            ecrivain.write_all(&donnees).await.unwrap();
            donnees
        });
        let mut recu = Vec::new();
        while recu.len() < 100_000 {
            let bloc = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .expect("le PCM doit arriver")
                .expect("canal ouvert");
            assert_eq!(bloc.len() % OCTETS_PAR_TRAME, 0, "trames entières");
            recu.extend_from_slice(&bloc);
        }
        let donnees = envoi.await.unwrap();
        assert_eq!(recu, donnees[..100_000], "octets exacts, dans l'ordre");
    }

    /// `end_of_track` du titre attendu : la pompe vide puis ferme le flux. La
    /// fin d'un AUTRE titre (celui qu'on vient de quitter) ne ferme rien.
    #[tokio::test]
    async fn pompe_6018_ferme_le_flux_a_la_fin_du_bon_titre() {
        let (mut ecrivain, lecteur) = tokio::io::duplex(64 * 1024);
        let pompe = PompePcm::new();
        let mut rx = pompe.attacher(Some("b".into()));
        let _tache = pompe.lancer(lecteur);
        {
            use tokio::io::AsyncWriteExt;
            ecrivain.write_all(&[1u8; 4000]).await.unwrap();
        }
        pompe.signaler_fin(Some("a"));
        let premier = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap();
        assert_eq!(premier.map(|b| b.len()), Some(4000));
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            pompe.a_un_consommateur(),
            "la fin d'un autre titre ne ferme rien"
        );
        pompe.signaler_fin(Some("b"));
        let fin = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("le flux doit se fermer après la fin du titre");
        assert!(fin.is_none());
    }

    /// Le script `--onevent` écrit une ligne que Tune relit.
    #[cfg(unix)]
    #[test]
    fn evenement_6018_le_script_relaie_les_variables_de_librespot() {
        let dossier = tempfile::tempdir().unwrap();
        let script = dossier.path().join("onevent.sh");
        std::fs::write(&script, script_onevent()).unwrap();
        let sortie = std::process::Command::new("/bin/sh")
            .arg(&script)
            .env("PLAYER_EVENT", "track_changed")
            .env("TRACK_ID", "4uLU6hMCjMI75M1A2tKUQC")
            .env("DURATION_MS", "213000")
            .env("NAME", "Never\tGonna")
            .env("ARTISTS", "Rick Astley\nAutre")
            .env("ALBUM", "Whenever")
            .env("COVERS", "https://i.scdn.co/a.jpg\nhttps://i.scdn.co/b.jpg")
            .output()
            .unwrap();
        let ligne = String::from_utf8(sortie.stderr).unwrap();
        let ev = lire_evenement_tune(ligne.trim_end()).expect("ligne relue");
        assert_eq!(ev.evenement, "track_changed");
        assert_eq!(ev.piste.as_deref(), Some("4uLU6hMCjMI75M1A2tKUQC"));
        assert_eq!(ev.duree_ms, Some(213_000));
        assert_eq!(ev.titre.as_deref(), Some("Never Gonna"));
        assert_eq!(ev.artistes.as_deref(), Some("Rick Astley, Autre"));
        assert_eq!(ev.album.as_deref(), Some("Whenever"));
        assert_eq!(ev.pochette.as_deref(), Some("https://i.scdn.co/a.jpg"));
        assert!(!ev.termine());
        let fin = lire_evenement_tune("TUNE_EVENT\tend_of_track\tabc\t\t\t\t\t\t").unwrap();
        assert!(fin.termine());
        assert_eq!(fin.position_ms, None);
        assert!(lire_evenement_tune("librespot: autre chose").is_none());
    }

    /// #6018, point 3 — sous launchd, `/opt/homebrew/bin` n'est pas dans le
    /// `PATH` : le binaire doit être trouvé quand même.
    #[cfg(unix)]
    #[test]
    fn binaire_6018_trouve_hors_du_path() {
        use std::os::unix::fs::PermissionsExt;
        let vide = tempfile::tempdir().unwrap();
        let brew = tempfile::tempdir().unwrap();
        let bin = brew.path().join("librespot");
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::ffi::OsString::from(vide.path());
        assert_eq!(
            chercher_librespot(None, Some(&path), &[brew.path().to_path_buf()]),
            Some(bin.clone()),
            "dossier usuel"
        );
        let path_brew = std::ffi::OsString::from(brew.path());
        assert_eq!(
            chercher_librespot(None, Some(&path_brew), &[]),
            Some(bin.clone()),
            "PATH"
        );
        assert_eq!(
            chercher_librespot(Some(&bin), None, &[]),
            Some(bin.clone()),
            "TUNE_LIBRESPOT"
        );
        assert_eq!(chercher_librespot(None, Some(&path), &[]), None);
    }
}
