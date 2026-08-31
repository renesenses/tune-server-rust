pub mod auto_dj;
pub mod crossfade;
pub mod dj_player;
pub mod gapless;
pub mod queue;
// `radio_handler` a été retiré ici (#3018). C'était une SECONDE lecture des
// métadonnées radio, sans aucun appelant depuis sa création : un
// `RadioMetadataHandler` complet, avec sa propre structure `IcyMetadata`
// homonyme de la vraie, dont `fetch_icy_metadata` rendait `cover_url: None`
// sans condition. La lecture vivante est `crate::radio_metadata` — elle relit
// `visual` (Radio France), `cover` (Radio Paradise) et `StreamUrl` (ICY), et
// c'est `crate::poller::vignette_du_pas_radio` qui arbitre pochette du titre
// contre logo de la station.
//
// Pourquoi la suppression compte : le 30/08/2026, une réponse au fil forum 104
// se réclamant explicitement d'une lecture du code a affirmé au testeur
// Reivax66 que « rien n'est allé chercher la pochette du disque », huit jours
// après la livraison de #2109 et quatre heures après la publication de la
// v0.9.127 qui la contient. Ce fichier mort disait exactement cela, en Rust.
// Le garde `tests/pochette_radio_source_unique.rs` empêche qu'un deuxième
// réapparaisse.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, broadcast};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlayState {
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NowPlaying {
    pub track_id: Option<i64>,
    pub title: String,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub cover_path: Option<String>,
    pub duration_ms: i64,
    pub source: String,
    pub source_id: Option<String>,
    pub stream_id: Option<String>,
    pub format: Option<String>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub genre: Option<String>,
    pub year: Option<i32>,
    /// L'album et l'artiste de la piste, par IDENTIFIANT.
    ///
    /// Ils manquaient. Le client web devait donc DEVINER l'album depuis son
    /// titre : cliquer sur « Entreat (2010) » dans la lecture en cours lancait
    /// une recherche sur « Entreat » et atterrissait sur la page de The Cure,
    /// pas sur celle de l'album (FabienM, v0.9.102). `Track` les porte tous
    /// deux, `from_track` les jetait.
    ///
    /// `Option`, et `#[serde(default)]` : une piste en streaming ou une radio
    /// n'a pas d'entree en bibliotheque, et un client plus ancien ne les envoie
    /// pas.
    #[serde(default)]
    pub album_id: Option<i64>,
    #[serde(default)]
    pub artist_id: Option<i64>,
    /// Débit CONSTANT du flux source, en kbit/s, quand la source le nomme
    /// elle-même. `None` veut dire « on ne sait pas » — et rien n'est affiché.
    ///
    /// Il existe pour le 128 kbit/s de Bandcamp (#2074) : la qualité était
    /// annoncée sur l'écran Bandcamp — « un flux à 128 kbit/s doit être
    /// annoncé comme tel partout où il apparaît »,
    /// `plugins/tune-bandcamp/src/lib.rs` — puis se perdait au passage en
    /// zone, où le chemin du signal n'affichait plus qu'un « MP3 »
    /// indiscernable d'un 320. Un fichier local n'en a pas besoin : sa
    /// résolution réelle est déjà lue au scan.
    ///
    /// `#[serde(default)]` : un client plus ancien ne l'envoie pas.
    #[serde(default)]
    pub bitrate_kbps: Option<u32>,
}

impl NowPlaying {
    /// Canonical mapping from a library [`Track`](crate::db::models::Track) row
    /// to a local `NowPlaying`.
    ///
    /// Centralises the audio-metadata fields (`format`, `sample_rate`,
    /// `bit_depth`, `genre`, `year`) so every "now playing" surface reports the
    /// **source** resolution — the file's real depth (16/24) from the library
    /// row — rather than the transcoded output format. Local playback forces a
    /// 32-bit WAV to the DAC; without this, the label flickered "32-bit then
    /// correct to 16" on the first tracks (see `play_inner` and the gapless
    /// `advance_queue_metadata` path).
    ///
    /// `source`/`source_id`/`cover_path` are taken verbatim from the row; callers
    /// that need URL-resolved cover art or a zone-derived source override those
    /// fields on the returned value. `stream_id` is always `None` (local rows are
    /// not streamed sessions).
    pub fn from_track(track: &crate::db::models::Track) -> Self {
        Self {
            track_id: track.id,
            title: track.title.clone(),
            artist_name: track.artist_name.clone(),
            album_title: track.album_title.clone(),
            cover_path: track.cover_path.clone(),
            duration_ms: track.duration_ms,
            source: track.source.clone(),
            source_id: track.source_id.clone(),
            stream_id: None,
            format: track.format.clone(),
            sample_rate: track.sample_rate.map(|v| v as u32),
            bit_depth: track.bit_depth.map(|v| v as u32),
            genre: track.genre.clone(),
            year: track.year,
            album_id: track.album_id,
            artist_id: track.artist_id,
            // Une piste de la bibliothèque n'annonce pas de débit : sa
            // résolution réelle est lue au scan et déjà portée ci-dessus. Le
            // champ existe pour les flux distants qui, eux, nomment leur
            // encodage (#2074).
            bitrate_kbps: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneState {
    pub zone_id: i64,
    pub state: PlayState,
    pub now_playing: Option<NowPlaying>,
    pub position_ms: i64,
    /// Position rendue par la BASE au démarrage, en attente d'être jouée (#2876).
    ///
    /// `zones.last_position_ms` est écrit tout au long de la lecture par le
    /// poller, puis réinjecté au démarrage par `restore_playback_positions` :
    /// c'est ce qui fait que le curseur affiche déjà le bon endroit à
    /// l'ouverture de l'interface. Mais aucun chemin de lecture ne s'en
    /// servait, et « Lecture » repartait à 0:00 alors que l'écran annonçait
    /// 2:31 (Sandro, fil 1610, sortie Diretta UPnP).
    ///
    /// Ce marqueur porte cette position-là, et elle seule : posé UNIQUEMENT par
    /// [`PlaybackManager::restore_position`], effacé par le premier
    /// [`PlaybackManager::play`] réel. Il ne dit donc rien de la position
    /// conservée par un Stop en cours de session — celle-là garde son
    /// comportement d'aujourd'hui, sans quoi un clic sur la même piste depuis
    /// la bibliothèque (ou une file arrivée à son terme, dont la position vaut
    /// la durée) se serait mis à sauter en avant.
    ///
    /// Interne au serveur : aucun client ne le lit.
    #[serde(skip)]
    pub pending_resume_ms: Option<i64>,
    pub volume: f64,
    pub muted: bool,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    pub queue_position: i64,
    pub queue_length: i64,
    /// Materialised shuffle order: a permutation of the queue indices
    /// `[0, queue_length)`. Empty when shuffle is off. Regenerated when shuffle
    /// is enabled or the queue length changes, and re-synced on every position
    /// update. `next_position` follows this order so shuffle plays every track
    /// exactly once per cycle (repeat-off stops at the end; repeat-all loops).
    #[serde(skip)]
    pub shuffle_order: Vec<usize>,
    /// Current index into `shuffle_order` (-1 before the first track). The next
    /// shuffle track is `shuffle_order[shuffle_index + 1]`.
    #[serde(skip)]
    pub shuffle_index: i64,
    /// La piste est en cours de résolution : Tune a accepté la demande mais
    /// n'a pas encore d'URL jouable.
    ///
    /// Ajouté pour YouTube, où l'extraction peut prendre très longtemps. Mesuré
    /// chez un testeur (forum #1359) : **32 secondes** entre le clic et le
    /// premier son, parce que les deux surfaces natives sont refusées par
    /// YouTube (« Sign in to confirm you're not a bot ») et que tout repart sur
    /// `yt-dlp`. Pendant ce temps l'écran ne montrait rien, et il a signalé
    /// « la lecture ne se lance pas » — ce qui, de son point de vue, était vrai.
    ///
    /// Champ ADDITIF volontairement : `PlayState` est traité dans 77 `match`,
    /// dont dix-huit dans le poller qui pilote la fin de piste et l'enchaînement.
    /// Y ajouter une variante aurait obligé à trancher son cas partout, pour un
    /// besoin d'affichage. Ce booléen ne modifie aucune décision de lecture.
    #[serde(default)]
    pub resolving: bool,
    /// La zone sert un flux DoP en ce moment — donc **son curseur de volume ne
    /// fait rien**.
    ///
    /// Le serveur épingle le volume à l'unité tant que dure le DoP : tout autre
    /// facteur réécrit le marqueur du flux, le DAC quitte le mode DSD et se
    /// coupe (#1735). Le client a besoin de le savoir pour le dire, sans quoi
    /// on aura remplacé un silence inexpliqué par une commande morte
    /// inexpliquée.
    ///
    /// Recopié du `OutputStatus` de la sortie, qui le **détecte sur les octets**
    /// (`is_dop_pcm`). Ce n'est pas le mode DSD de la zone : celui-ci dit ce qui
    /// a été demandé, pas ce qui part sur le fil — le plafond « Fréquence max »
    /// peut faire retomber en PCM sans rien annoncer. Déduire l'un de l'autre
    /// est exactement ce qui a fait mentir le chemin du signal (#1595).
    ///
    /// Additif, comme `resolving` : ne modifie aucune décision de lecture.
    #[serde(default)]
    pub dop_active: bool,
    /// Verdict réellement observé à la frontière du backend de sortie.
    ///
    /// Interne au serveur : les routes le fondent dans `signal_path`, contrat
    /// public déjà consommé par les clients. `None` conserve le calcul
    /// historique pour les sorties qui ne publient pas encore cette sonde.
    #[serde(skip)]
    pub output_signal_path: Option<crate::outputs::traits::OutputSignalPathStatus>,
    /// Compteurs DSP observés par la sortie pendant la piste courante.
    #[serde(skip)]
    pub output_dsp_metrics: Option<crate::outputs::traits::OutputDspMetrics>,
    /// Monotonically increasing counter bumped on each `play()` call.
    /// The poller uses this to detect track changes and reset its state
    /// (peak_position, gapless flags, etc.) so stale data from the
    /// previous track cannot trigger false advances.
    #[serde(default)]
    pub track_generation: u64,
    /// Monotonic play-request counter, bumped only when a new play is issued
    /// for this zone (`bump_generation`). Unlike `track_generation` — which the
    /// poller also bumps on recovery — this changes ONLY on an actual new play,
    /// so the orchestrator can detect that a newer play superseded an in-flight
    /// one (slow resolve) and skip sending a second, overlapping stream.
    #[serde(default)]
    pub play_seq: u64,
    /// Timestamp of the last seek operation.  The poller checks this and
    /// suppresses stale position updates from the output for a brief grace
    /// period so the UI doesn't snap back to the pre-seek position.
    #[serde(skip)]
    pub last_seek_at: Option<Instant>,
    /// Timestamp of the last user-initiated volume change.  The poller
    /// ignores renderer-reported volume for a grace period to prevent
    /// the slider from bouncing back on slow DLNA renderers.
    #[serde(skip)]
    pub last_volume_set_at: Option<Instant>,
    /// Timestamp of the last stall-recovery restart of this zone, stamped by
    /// the OAAT stall supervisor. The supervisor replays the CURRENT track
    /// from 0; the poller reads this to suppress a gapless position-reset
    /// auto-advance for a brief window afterwards, so that from-zero replay is
    /// not misread as a real track transition (which would run now-playing one
    /// track ahead of the audio — Xavier, OAAT Tune Endpoint).
    #[serde(skip)]
    pub last_restart_at: Option<Instant>,
    /// Wall-clock instant the CURRENT track was last (re)started via `play()`.
    /// Read by the orchestrator to coalesce a redundant controller
    /// double-dispatch: a `play()` for the track already playing that arrives a
    /// few seconds after it started is a re-tap and must NOT re-send
    /// SetAVTransportURI+Play, which restarts a network renderer from byte 0
    /// (Revox S100 "plays ~10s then jumps to 0" — #1271). A deliberate replay of
    /// the same track lands far later (older timestamp) and is untouched.
    #[serde(skip)]
    pub last_play_started_at: Option<Instant>,
    /// Profile that owns the current listening session on this zone. Set by
    /// user-initiated play handlers (from the `X-Profile-Id` header) and by the
    /// alarm scheduler; read by the orchestrator when writing `listen_history`.
    /// Autoplay / gapless advances reuse the zone without touching it, so they
    /// inherit the session owner for free. `None` → the listen is tagged NULL
    /// (server-initiated, no owner) rather than misattributed to a person.
    #[serde(default)]
    pub session_profile_id: Option<i64>,
    /// Ce que l'auditeur a demande en lancant cette session : le TYPE de
    /// l'objet sur lequel il a clique « Lire » (`track`, `album`, `playlist`,
    /// `artist`, `label`) et son identifiant. Pose par le gestionnaire de
    /// `POST /zones/:id/play` a partir du corps de la requete ; relu par
    /// l'orchestrateur au moment d'ecrire `listen_history`.
    ///
    /// Meme mecanique que `session_profile_id`, et pour la meme raison : les
    /// avances automatiques (autoplay, gapless, file d'attente) reutilisent la
    /// zone sans y toucher, donc elles heritent du contexte — la deuxieme
    /// piste d'une playlist reste une ecoute « playlist ».
    ///
    /// `None` = l'appelant n'a pas dit d'ou venait le geste. On ecrit NULL
    /// plutot que de deviner : une intention inventee est pire qu'une absence.
    #[serde(default)]
    pub session_context_type: Option<String>,
    #[serde(default)]
    pub session_context_id: Option<String>,
    /// Instant de la dernière mise en pause (`None` hors pause). Pour une
    /// RADIO, l'orchestrateur compare cet instant à un seuil à la reprise :
    /// un flux live continue de se périmer pendant la pause (connexion
    /// icecast, tampon de la sortie, horodatage des paquets), et au-delà de
    /// quelques secondes la reprise doit REJOUER la station comme au premier
    /// lancement plutôt que de reprendre un pipeline mort (#1629).
    #[serde(skip)]
    pub paused_at: Option<Instant>,
    /// Horloge murale (epoch ms UTC) du dernier changement de métadonnée
    /// titre/artiste du now-playing. Pour une radio, c'est l'instant où le
    /// serveur a détecté le changement de morceau dans le flux (ICY / API
    /// livemeta) : le client s'en sert comme ancrage temporel des paroles
    /// synchronisées (position ≈ maintenant − metadata_changed_at). Stampé au
    /// `play()` et dans `update_now_playing` uniquement quand titre/artiste
    /// changent réellement — le rappel périodique d'une métadonnée identique
    /// ne doit pas faire repartir l'ancrage de zéro.
    #[serde(default)]
    pub metadata_changed_at_ms: Option<i64>,
    /// Instant où Tune a CONSTATÉ que la lecture de cette zone navigateur
    /// n'était reçue par aucun onglet — pas l'instant présent, celui du
    /// constat.
    ///
    /// `output_reach` ne pouvait dire `browser_unattended` que d'une zone
    /// en `Playing` : la valeur retombait à `"ok"` dès que la lecture
    /// cessait, et le bandeau qui porte cette explication disparaissait avec
    /// elle. Or la lecture cesse précisément à cet instant-là — soit parce
    /// que l'utilisateur arrête une zone muette, soit parce que le poller
    /// l'abandonne au MÊME seuil que celui qui déclenche le bandeau
    /// (`DELAI_SILENCE_ETABLI`, #2630). Le seul message qui explique le
    /// silence s'effaçait donc au geste qu'il était censé prévenir : Pierre M
    /// l'a vu passer et l'a rapporté de travers, ce qui a détourné
    /// l'instruction de #2571 pendant plusieurs échanges (#2588).
    ///
    /// Horodater le CONSTAT sépare la durée du défaut de la durée de son
    /// affichage. Effacé par `play()` : une nouvelle lecture rouvre la
    /// question, la réponse d'avant ne vaut plus.
    ///
    /// `#[serde(skip)]`, comme `last_play_started_at` : après une
    /// restauration d'état on ne conclut rien.
    #[serde(skip)]
    pub browser_unattended_at: Option<Instant>,
}

/// Vrai quand la nouvelle métadonnée now-playing change d'identité
/// (titre ou artiste) par rapport à l'ancienne — c'est CE changement qui
/// redémarre l'ancrage temporel des paroles, pas un simple rafraîchissement.
fn metadata_identity_changed(old: Option<&NowPlaying>, new: &NowPlaying) -> bool {
    old.is_none_or(|o| o.title != new.title || o.artist_name != new.artist_name)
}

/// Epoch UTC en millisecondes (horloge murale du serveur).
pub(crate) fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl ZoneState {
    /// Âge (ms) de la dernière métadonnée now-playing, calculé sur l'horloge
    /// du serveur. Les routes zone l'exposent tel quel : le client pose son
    /// ancrage local (`maintenant_client − âge`) sans jamais comparer horloge
    /// client et horloge serveur.
    pub fn metadata_age_ms(&self) -> Option<i64> {
        self.metadata_changed_at_ms
            .map(|ts| (epoch_ms() - ts).max(0))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RepeatMode {
    Off,
    One,
    All,
}

impl Default for ZoneState {
    fn default() -> Self {
        Self {
            zone_id: 0,
            state: PlayState::Stopped,
            now_playing: None,
            resolving: false,
            dop_active: false,
            output_signal_path: None,
            output_dsp_metrics: None,
            position_ms: 0,
            pending_resume_ms: None,
            volume: 0.5,
            muted: false,
            shuffle: false,
            repeat: RepeatMode::Off,
            queue_position: 0,
            queue_length: 0,
            shuffle_order: Vec::new(),
            shuffle_index: -1,
            track_generation: 0,
            play_seq: 0,
            paused_at: None,
            last_seek_at: None,
            last_volume_set_at: None,
            last_restart_at: None,
            last_play_started_at: None,
            session_profile_id: None,
            session_context_type: None,
            session_context_id: None,
            metadata_changed_at_ms: None,
            browser_unattended_at: None,
        }
    }
}

/// Build a materialised shuffle order: a Fisher-Yates permutation of
/// `[0, length)` with `current` moved to index 0, so the first advance goes to
/// a different track than the one playing. Seeded from the wall clock via a
/// xorshift64 PRNG (no `rand` crate dependency).
pub(crate) fn generate_shuffle_order(length: usize, current: usize) -> Vec<usize> {
    if length == 0 {
        return Vec::new();
    }
    let mut order: Vec<usize> = (0..length).collect();
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1;
    // Fisher-Yates using a xorshift64 PRNG.
    for i in (1..length).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let j = (seed % (i as u64 + 1)) as usize;
        order.swap(i, j);
    }
    if current < length {
        if let Some(pos) = order.iter().position(|&x| x == current) {
            order.swap(0, pos);
        }
    }
    order
}

#[derive(Debug, Clone, Serialize)]
pub struct PlaybackEvent {
    pub event: String,
    pub zone_id: i64,
    pub data: serde_json::Value,
}

pub struct PlaybackManager {
    zones: Arc<Mutex<HashMap<i64, ZoneState>>>,
    event_tx: broadcast::Sender<PlaybackEvent>,
    /// Inhibition de veille agrégée : une zone qui s'arrête ne la relâche pas
    /// tant qu'une autre joue encore (#2108).
    sleep_inhibitor: crate::system_sleep::SystemSleepInhibitor,
    /// Un [`crate::audio::tap::ZoneTap`] par zone — le tap PCM que le
    /// forwarder de niveaux alimente et que les plugins d'analyse consomment.
    /// Verrou synchrone : accès courts, jamais tenus à travers un await.
    zone_taps: std::sync::Mutex<HashMap<i64, Arc<crate::audio::tap::ZoneTap>>>,
    /// Génération des forwarders de niveaux, par zone. Un forwarder capture
    /// la valeur au spawn et s'arrête dès qu'elle change. Bumpée par l'avance
    /// gapless : contrairement à `play_seq`, elle invalide les niveaux sans
    /// toucher à la sémantique des lectures en cours.
    levels_gens: std::sync::Mutex<HashMap<i64, Arc<std::sync::atomic::AtomicU64>>>,
}

impl Default for PlaybackManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaybackManager {
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(256);
        Self {
            zones: Arc::new(Mutex::new(HashMap::new())),
            event_tx,
            sleep_inhibitor: crate::system_sleep::SystemSleepInhibitor::new(),
            zone_taps: std::sync::Mutex::new(HashMap::new()),
            levels_gens: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// La génération de niveaux d'une zone (créée au premier accès).
    pub fn levels_gen(&self, zone_id: i64) -> Arc<std::sync::atomic::AtomicU64> {
        self.levels_gens
            .lock()
            .expect("levels_gens lock")
            .entry(zone_id)
            .or_default()
            .clone()
    }

    /// Invalide tous les forwarders de niveaux vivants de la zone.
    pub fn bump_levels_gen(&self, zone_id: i64) {
        self.levels_gen(zone_id)
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Le tap PCM d'une zone (créé au premier accès). Voir
    /// [`crate::audio::tap`] pour le contrat.
    pub fn zone_tap(&self, zone_id: i64) -> Arc<crate::audio::tap::ZoneTap> {
        self.zone_taps
            .lock()
            .expect("zone_taps lock")
            .entry(zone_id)
            .or_default()
            .clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<PlaybackEvent> {
        self.event_tx.subscribe()
    }

    pub async fn get_state(&self, zone_id: i64) -> ZoneState {
        let zones = self.zones.lock().await;
        zones.get(&zone_id).cloned().unwrap_or(ZoneState {
            zone_id,
            ..Default::default()
        })
    }

    /// Restore a saved playback position into the zone state.
    /// Called on startup to remember where playback left off.
    pub async fn restore_position(&self, zone_id: i64, position_ms: i64, np: NowPlaying) {
        let mut zones = self.zones.lock().await;
        let state = zones.entry(zone_id).or_insert_with(|| ZoneState {
            zone_id,
            ..Default::default()
        });
        // On tient une URL jouable : la recherche est finie.
        state.resolving = false;
        state.position_ms = position_ms;
        // #2876 — armer la reprise. Sans ce marqueur, la position ci-dessus
        // n'est plus qu'un affichage : les chemins « Lecture après arrêt »
        // construisent leur `PlayRequest` avec `seek_ms: None` et le morceau
        // repart de zéro. Voir `ZoneState::pending_resume_ms`.
        state.pending_resume_ms = (position_ms > 0).then_some(position_ms);
        state.now_playing = Some(np);
        state.state = PlayState::Stopped;
    }

    /// La position rendue par la base au démarrage, tant qu'elle n'a pas été
    /// jouée. Voir [`ZoneState::pending_resume_ms`].
    pub async fn pending_resume_ms(&self, zone_id: i64) -> Option<i64> {
        self.zones
            .lock()
            .await
            .get(&zone_id)
            .and_then(|s| s.pending_resume_ms)
    }

    pub async fn all_states(&self) -> Vec<ZoneState> {
        let zones = self.zones.lock().await;
        zones.values().cloned().collect()
    }

    pub async fn bump_generation(&self, zone_id: i64) -> u64 {
        let mut zones = self.zones.lock().await;
        let state = zones.entry(zone_id).or_insert_with(|| ZoneState {
            zone_id,
            ..Default::default()
        });
        state.track_generation = state.track_generation.wrapping_add(1);
        state.play_seq = state.play_seq.wrapping_add(1);
        state.play_seq
    }

    /// Current play-request sequence for a zone (0 if never played). Compared
    /// against the value captured at play start to detect that a newer play
    /// superseded an in-flight one before it sends output.
    pub async fn current_play_seq(&self, zone_id: i64) -> u64 {
        self.zones
            .lock()
            .await
            .get(&zone_id)
            .map(|s| s.play_seq)
            .unwrap_or(0)
    }

    /// Marque la zone comme « en cours de résolution », ou lève le drapeau.
    ///
    /// Levé avant une extraction potentiellement longue (YouTube via `yt-dlp` :
    /// 32 s mesurées), retombé dès que `play()` s'exécute — c'est-à-dire dès
    /// qu'une URL jouable existe. N'influence aucune décision de lecture : ce
    /// drapeau ne sert qu'à ce que l'interface puisse dire « je cherche »
    /// plutôt que de rester muette.
    pub async fn set_resolving(&self, zone_id: i64, value: bool) {
        let mut zones = self.zones.lock().await;
        zones
            .entry(zone_id)
            .or_insert_with(|| ZoneState {
                zone_id,
                ..Default::default()
            })
            .resolving = value;
    }

    /// Reporte l'état DoP lu sur la sortie dans l'état de zone servi au client.
    ///
    /// Appelée à chaque tour du poller, sur les deux chemins (zone au repos et
    /// zone en lecture) : un flux peut basculer en DoP ou en sortir d'une piste
    /// à l'autre sans que la zone change d'état, et le curseur de volume doit
    /// suivre dans les deux sens.
    pub async fn set_dop_active(&self, zone_id: i64, value: bool) {
        let mut zones = self.zones.lock().await;
        zones
            .entry(zone_id)
            .or_insert_with(|| ZoneState {
                zone_id,
                ..Default::default()
            })
            .dop_active = value;
    }

    /// Reporte le contrat réellement constaté par le backend dans l'état de
    /// lecture dont les routes construisent le chemin du signal.
    pub async fn set_output_signal_path(
        &self,
        zone_id: i64,
        value: Option<crate::outputs::traits::OutputSignalPathStatus>,
    ) {
        let mut zones = self.zones.lock().await;
        zones
            .entry(zone_id)
            .or_insert_with(|| ZoneState {
                zone_id,
                ..Default::default()
            })
            .output_signal_path = value;
    }

    pub async fn set_output_dsp_metrics(
        &self,
        zone_id: i64,
        value: Option<crate::outputs::traits::OutputDspMetrics>,
    ) {
        let mut zones = self.zones.lock().await;
        zones
            .entry(zone_id)
            .or_insert_with(|| ZoneState {
                zone_id,
                ..Default::default()
            })
            .output_dsp_metrics = value;
    }

    pub async fn play(&self, zone_id: i64, np: NowPlaying) {
        let mut zones = self.zones.lock().await;
        let state = zones.entry(zone_id).or_insert_with(|| ZoneState {
            zone_id,
            ..Default::default()
        });
        // A seek recreates the stream, which routes through play(). In that
        // case the orchestrator has already set position_ms to the seek target
        // (via playback.seek()) just before this call, so resetting to 0 here
        // would make the progress bar snap back to the start of the track and
        // stay there until the seek grace period ends. Detect a recent seek and
        // preserve the seeked position; only a genuine track change (no recent
        // seek) resets to 0.
        let is_recent_seek = state
            .last_seek_at
            .map(|t| t.elapsed().as_secs() < 5)
            .unwrap_or(false);
        // La recherche est finie : on tient une URL jouable, c'est tout l'objet
        // de cet appel. Sans cette ligne le drapeau levé par l'orchestrateur
        // avant `resolve_stream` n'était JAMAIS abaissé sur le chemin qui
        // réussit — seuls deux chemins d'erreur le faisaient. Le commentaire de
        // `play_inner` affirmait pourtant « le drapeau retombe dans play() » :
        // l'intention était écrite, l'instruction manquait, et la zone restait
        // annoncée « recherche en cours » pendant toute la lecture.
        state.resolving = false;
        state.state = PlayState::Playing;
        // Le verdict appartient au flux qui l'a produit. Tant que le backend
        // n'a pas observé le premier buffer du nouveau flux, mieux vaut
        // annoncer « non observé » que réutiliser la promesse de la piste
        // précédente.
        state.output_signal_path = None;
        state.paused_at = None;
        if !is_recent_seek {
            state.position_ms = 0;
        }
        // La position restaurée au démarrage est à usage unique : ce flux-ci
        // l'a consommée (si l'appelant l'a demandée) ou l'a rendue caduque (il
        // joue autre chose). Dans les deux cas elle ne vaut plus (#2876).
        state.pending_resume_ms = None;
        // Stamp the (re)start instant so the orchestrator can coalesce a
        // redundant controller double-dispatch of this same track (#1271).
        state.last_play_started_at = Some(Instant::now());
        // Une nouvelle lecture rouvre la question « quelqu'un reçoit-il ce
        // son ? » : le constat de silence précédent ne décrit plus rien
        // (#2588).
        state.browser_unattended_at = None;
        // np is no longer read after this — the event payload is built from
        // `state` via now_playing_event_data() below — so move instead of clone.
        state.now_playing = Some(np);
        // Nouveau morceau → nouvel ancrage temporel de métadonnée.
        state.metadata_changed_at_ms = Some(epoch_ms());
        state.track_generation = state.track_generation.wrapping_add(1);
        // Preserve last_seek_at if a seek just happened (< 5s ago) — the
        // orchestrator recreates the stream during seek, which calls play().
        // Clearing it here would remove the seek grace period from the poller.
        if !is_recent_seek {
            state.last_seek_at = None;
        }

        let data = now_playing_event_data(state);
        self.sync_sleep_inhibition(&zones);
        self.emit(PlaybackEvent {
            event: "started".into(),
            zone_id,
            data,
        });
    }

    pub async fn pause(&self, zone_id: i64) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.state = PlayState::Paused;
            state.paused_at = Some(Instant::now());
        }
        self.sync_sleep_inhibition(&zones);
        self.emit(PlaybackEvent {
            event: "paused".into(),
            zone_id,
            data: serde_json::json!({}),
        });
    }

    pub async fn resume(&self, zone_id: i64) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.state = PlayState::Playing;
            state.paused_at = None;
        }
        self.sync_sleep_inhibition(&zones);
        self.emit(PlaybackEvent {
            event: "resumed".into(),
            zone_id,
            data: serde_json::json!({}),
        });
    }

    pub async fn stop(&self, zone_id: i64) {
        let mut zones = self.zones.lock().await;
        let data = if let Some(state) = zones.get_mut(&zone_id) {
            // Un arrêt met fin à toute recherche en cours : sans cela, une
            // lecture interrompue pendant `resolve_stream` laissait la zone
            // annoncée « recherche en cours » alors qu'elle est à l'arrêt.
            state.resolving = false;
            state.state = PlayState::Stopped;
            state.output_signal_path = None;
            state.paused_at = None;
            state.last_seek_at = None;
            // Keep position_ms and now_playing so the UI shows where
            // playback left off and can resume from the same position.
            now_playing_event_data(state)
        } else {
            serde_json::json!({})
        };
        self.sync_sleep_inhibition(&zones);
        self.emit(PlaybackEvent {
            event: "stopped".into(),
            zone_id,
            data,
        });
    }

    /// Stop playback and clear the now_playing metadata entirely.
    /// Used when the queue is cleared — there is nothing to resume.
    pub async fn stop_and_clear(&self, zone_id: i64) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.resolving = false;
            state.state = PlayState::Stopped;
            state.paused_at = None;
            state.now_playing = None;
            state.position_ms = 0;
            // La file est vide : il n'y a plus rien à reprendre (#2876).
            state.pending_resume_ms = None;
            state.metadata_changed_at_ms = None;
        }
        self.sync_sleep_inhibition(&zones);
        self.emit(PlaybackEvent {
            event: "stopped".into(),
            zone_id,
            data: serde_json::json!({}),
        });
    }

    fn sync_sleep_inhibition(&self, zones: &HashMap<i64, ZoneState>) {
        self.sleep_inhibitor.set_active(
            zones
                .values()
                .any(|state| state.state == PlayState::Playing),
        );
    }

    /// Stamp a stall-recovery restart for this zone. Called by the OAAT stall
    /// supervisor right after it replays the current track, so the poller can
    /// suppress a phantom gapless auto-advance triggered by the from-zero
    /// position drop (which would otherwise put now-playing one track ahead of
    /// the audio).
    pub async fn mark_restart(&self, zone_id: i64) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.last_restart_at = Some(Instant::now());
        }
    }
    /// Consigner — ou lever — le constat « aucun onglet ne reçoit le son de
    /// cette zone ».
    ///
    /// `true` horodate le constat À CET INSTANT ; tant qu'il reste vrai, la
    /// marque est rafraîchie, si bien qu'elle date toujours du DERNIER instant
    /// où le silence a été observé, et non du premier. `false` la lève : la
    /// zone est de nouveau reçue, il n'y a plus rien à expliquer.
    ///
    /// Deux appelants, une seule chose dite : la vue des zones, qui ne peut
    /// annoncer `browser_unattended` à un client sans que le serveur en garde
    /// trace, et le poller, qui abandonne la lecture au même seuil et
    /// l'arrêterait sinon sans laisser d'explication derrière lui (#2588).
    pub async fn note_browser_unattended(&self, zone_id: i64, unattended: bool) {
        let mut zones = self.zones.lock().await;
        let state = zones.entry(zone_id).or_insert_with(|| ZoneState {
            zone_id,
            ..Default::default()
        });
        state.browser_unattended_at = unattended.then(Instant::now);
    }

    pub async fn seek(&self, zone_id: i64, position_ms: i64) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.position_ms = position_ms;
            state.last_seek_at = Some(Instant::now());
        }
        self.emit(PlaybackEvent {
            event: "seek".into(),
            zone_id,
            data: serde_json::json!({ "position_ms": position_ms }),
        });
    }

    pub async fn set_volume(&self, zone_id: i64, volume: f64) {
        let mut zones = self.zones.lock().await;
        let state = zones.entry(zone_id).or_insert_with(|| ZoneState {
            zone_id,
            ..Default::default()
        });
        state.volume = volume.clamp(0.0, 1.0);
        self.emit(PlaybackEvent {
            event: "volume".into(),
            zone_id,
            data: serde_json::json!({ "volume": volume }),
        });
    }

    pub async fn mark_volume_changed(&self, zone_id: i64) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.last_volume_set_at = Some(Instant::now());
        }
    }

    pub async fn set_mute(&self, zone_id: i64, muted: bool) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.muted = muted;
        }
        self.emit(PlaybackEvent {
            event: "muted".into(),
            zone_id,
            data: serde_json::json!({ "muted": muted }),
        });
    }

    /// `entry` pour la même raison que [`Self::update_queue_info`] : les trois
    /// appels de `restore_queue_metadata` (longueur, répétition, aléatoire) se
    /// suivent au démarrage, sur une zone qui n'est pas encore en mémoire. Les
    /// laisser en `get_mut` reviendrait à corriger un des trois et laisser ses
    /// deux sœurs jeter la valeur restaurée en silence.
    pub async fn set_shuffle(&self, zone_id: i64, enabled: bool) {
        {
            let mut zones = self.zones.lock().await;
            let state = zones.entry(zone_id).or_insert_with(|| ZoneState {
                zone_id,
                ..Default::default()
            });
            state.shuffle = enabled;
            if enabled {
                // Build a fresh order around the currently playing track so
                // the next advance goes to a different track.
                state.shuffle_order = generate_shuffle_order(
                    state.queue_length.max(0) as usize,
                    state.queue_position.max(0) as usize,
                );
                state.shuffle_index = if state.shuffle_order.is_empty() {
                    -1
                } else {
                    0
                };
            } else {
                state.shuffle_order.clear();
                state.shuffle_index = -1;
            }
        }
        self.emit(PlaybackEvent {
            event: "shuffle".into(),
            zone_id,
            data: serde_json::json!({ "enabled": enabled }),
        });
    }

    /// `entry` : voir [`Self::update_queue_info`]. Le mode de répétition
    /// restauré au démarrage tombait dans le vide sur une zone pas encore en
    /// mémoire, alors que le journal annonçait la restauration.
    pub async fn set_repeat(&self, zone_id: i64, mode: RepeatMode) {
        {
            let mut zones = self.zones.lock().await;
            zones
                .entry(zone_id)
                .or_insert_with(|| ZoneState {
                    zone_id,
                    ..Default::default()
                })
                .repeat = mode;
        }
        self.emit(PlaybackEvent {
            event: "repeat".into(),
            zone_id,
            data: serde_json::json!({ "mode": mode }),
        });
    }

    /// Record which profile owns the current listening session on this zone.
    /// Uses `entry` so a handler can stamp the session BEFORE the first
    /// `play()` creates the zone (otherwise the first track would tag NULL).
    /// No event is emitted — this is internal attribution, not UI state.
    pub async fn set_session_profile(&self, zone_id: i64, profile_id: Option<i64>) {
        let mut zones = self.zones.lock().await;
        zones
            .entry(zone_id)
            .or_insert_with(|| ZoneState {
                zone_id,
                ..Default::default()
            })
            .session_profile_id = profile_id;
    }

    /// Enregistrer ce que l'auditeur a demande en lancant cette session.
    ///
    /// Meme `entry` que `set_session_profile`, et pour la meme raison : le
    /// gestionnaire pose le contexte AVANT que le premier `play()` ne cree la
    /// zone, sinon la premiere piste — la seule dont l'intention soit connue —
    /// partirait sans contexte.
    ///
    /// Ecrase toujours, y compris avec `None` : un nouveau geste de lecture
    /// remplace le precedent. Sans cela, jouer une piste isolee apres une
    /// playlist laisserait la piste marquee « playlist ».
    ///
    /// Aucun evenement emis — c'est de l'attribution interne, pas de l'etat
    /// d'interface.
    pub async fn set_session_context(
        &self,
        zone_id: i64,
        context_type: Option<String>,
        context_id: Option<String>,
    ) {
        let mut zones = self.zones.lock().await;
        let z = zones.entry(zone_id).or_insert_with(|| ZoneState {
            zone_id,
            ..Default::default()
        });
        z.session_context_type = context_type;
        z.session_context_id = context_id;
    }

    pub async fn update_position(&self, zone_id: i64, position_ms: i64) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            state.position_ms = position_ms;
        }
    }

    /// Longueur et position de la file pour cette zone.
    ///
    /// `entry` et non `get_mut` : la zone n'est PAS forcément déjà en mémoire
    /// quand on lui annonce sa file. Au démarrage, `restore_queue_metadata`
    /// appelle cette fonction AVANT que quoi que ce soit n'ait créé l'état de
    /// zone — et `restore_playback_positions`, qui le crée d'habitude, saute
    /// justement les zones dont la dernière piste venait d'un service (pas de
    /// `last_track_id`) ou dont la piste a disparu de la bibliothèque. La
    /// longueur restaurée partait alors à la poubelle en silence, la zone
    /// démarrait avec `queue_length = 0`, et `next_position()` — qui rend
    /// `None` dès que la file est vide — concluait « file terminée » à la fin
    /// du premier morceau : le sondeur arrêtait la zone alors que l'écran, lui,
    /// lit la file en base et affichait toujours les pistes suivantes.
    ///
    /// C'est ce piège exact que deux appelants de `routes/playback.rs`
    /// contournaient déjà chacun dans leur coin, en réaffirmant la longueur
    /// APRÈS `play()` (« silent no-op when the zone's in-memory state doesn't
    /// exist yet »). Leurs sœurs — `Orchestrator::play_from_queue`, le repli
    /// « Lire » sans corps de requête, `queue_add` — ne l'avaient pas. On
    /// bouche le trou une fois, à la source.
    pub async fn update_queue_info(&self, zone_id: i64, position: i64, length: i64) {
        let mut zones = self.zones.lock().await;
        let state = zones.entry(zone_id).or_insert_with(|| ZoneState {
            zone_id,
            ..Default::default()
        });
        state.queue_position = position;
        state.queue_length = length;
        if state.shuffle {
            let len = length.max(0) as usize;
            let pos = position.max(0) as usize;
            if len == 0 {
                state.shuffle_order.clear();
                state.shuffle_index = -1;
            } else if state.shuffle_order.len() != len {
                // Queue length changed (tracks added/removed, or the order
                // was lost across a restart — it is not persisted). Rebuild
                // around the current track.
                state.shuffle_order = generate_shuffle_order(len, pos);
                state.shuffle_index = 0;
            } else if let Some(idx) = state.shuffle_order.iter().position(|&p| p == pos) {
                // Sync the cursor to the track now playing so the next
                // advance follows the order from here.
                state.shuffle_index = idx as i64;
            } else {
                // Position not in the order (shouldn't happen) — rebuild.
                state.shuffle_order = generate_shuffle_order(len, pos);
                state.shuffle_index = 0;
            }
        }
    }

    pub fn emit_position(&self, zone_id: i64, position_ms: i64) {
        self.emit(PlaybackEvent {
            event: "position".into(),
            zone_id,
            data: serde_json::json!({ "position_ms": position_ms }),
        });
    }

    /// Update the NowPlaying metadata for a zone without resetting position.
    /// Used for radio streams where the track info changes while playing.
    pub async fn update_now_playing(&self, zone_id: i64, np: NowPlaying) {
        let mut zones = self.zones.lock().await;
        if let Some(state) = zones.get_mut(&zone_id) {
            // Ancrage temporel : re-stampé uniquement quand titre/artiste
            // changent vraiment. Un rafraîchissement de la même métadonnée
            // (poll ICY périodique) ne doit pas remettre les paroles à zéro.
            if metadata_identity_changed(state.now_playing.as_ref(), &np) {
                state.metadata_changed_at_ms = Some(epoch_ms());
            }
            state.now_playing = Some(np);
        }
        let data = zones
            .get(&zone_id)
            .map(now_playing_event_data)
            .unwrap_or_else(|| serde_json::json!({}));
        self.emit(PlaybackEvent {
            event: "track_changed".into(),
            zone_id,
            data,
        });
    }

    fn emit(&self, event: PlaybackEvent) {
        let _ = self.event_tx.send(event);
    }
}

/// Build the JSON payload for a now-playing WS event (`started` /
/// `track_changed` / `stopped`) from the full [`ZoneState`], so every one of
/// those events carries the same complete set of fields: the entire
/// [`NowPlaying`] (title, `track_id`, `format`, `sample_rate`, `bit_depth`, …)
/// plus `queue_position` / `queue_length` / `track_generation`. Each event used
/// to hand-write a different subset — `track_changed` (emitted on every gapless
/// advance) carried neither `track_id`, nor the quality fields, nor the queue
/// index — which forced the client to refetch the whole queue and delayed the
/// quality badge until a manual refresh (#1096, Benjithom).
fn now_playing_event_data(state: &ZoneState) -> serde_json::Value {
    let mut v = state
        .now_playing
        .as_ref()
        .and_then(|np| serde_json::to_value(np).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "queue_position".into(),
            serde_json::json!(state.queue_position),
        );
        obj.insert("queue_length".into(), serde_json::json!(state.queue_length));
        obj.insert(
            "track_generation".into(),
            serde_json::json!(state.track_generation),
        );
        // Ancrage temporel des paroles radio : instant (serveur) du dernier
        // changement titre/artiste + âge déjà calculé côté serveur, pour que
        // le client n'ait pas à comparer deux horloges différentes.
        if let Some(ts) = state.metadata_changed_at_ms {
            obj.insert("metadata_changed_at".into(), serde_json::json!(ts));
            obj.insert(
                "metadata_age_ms".into(),
                serde_json::json!((epoch_ms() - ts).max(0)),
            );
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_playing_event_data_carries_full_payload() {
        let mut state = ZoneState {
            zone_id: 1,
            state: PlayState::Playing,
            resolving: false,
            dop_active: false,
            output_signal_path: None,
            output_dsp_metrics: None,
            now_playing: Some(NowPlaying {
                track_id: Some(42),
                title: "Song".into(),
                bit_depth: Some(16),
                sample_rate: Some(44100),
                format: Some("flac".into()),
                ..Default::default()
            }),
            position_ms: 0,
            pending_resume_ms: None,
            volume: 1.0,
            muted: false,
            shuffle: false,
            repeat: RepeatMode::Off,
            queue_position: 3,
            queue_length: 100,
            shuffle_order: vec![],
            shuffle_index: -1,
            track_generation: 7,
            play_seq: 0,
            paused_at: None,
            last_seek_at: None,
            last_volume_set_at: None,
            last_restart_at: None,
            last_play_started_at: None,
            session_profile_id: None,
            session_context_type: None,
            session_context_id: None,
            metadata_changed_at_ms: None,
            browser_unattended_at: None,
        };
        let v = now_playing_event_data(&state);
        // Full NowPlaying is serialised…
        assert_eq!(v["track_id"], 42);
        assert_eq!(v["bit_depth"], 16);
        assert_eq!(v["format"], "flac");
        // …plus the queue index/length and generation.
        assert_eq!(v["queue_position"], 3);
        assert_eq!(v["queue_length"], 100);
        assert_eq!(v["track_generation"], 7);

        // With no now_playing it still reports the queue fields, never panics.
        state.now_playing = None;
        let empty = now_playing_event_data(&state);
        assert_eq!(empty["queue_position"], 3);
        assert!(empty.get("track_id").is_none());
    }

    #[test]
    fn metadata_identity_change_detection() {
        let np = |title: &str, artist: Option<&str>| NowPlaying {
            title: title.into(),
            artist_name: artist.map(|s| s.to_string()),
            source: "radio".into(),
            ..Default::default()
        };
        let old = np("So What", Some("Miles Davis"));
        // Pas d'ancien now-playing → changement.
        assert!(metadata_identity_changed(None, &old));
        // Même titre/artiste (poll ICY répété) → pas de changement.
        assert!(!metadata_identity_changed(
            Some(&old),
            &np("So What", Some("Miles Davis"))
        ));
        // Titre différent, artiste différent, ou artiste qui apparaît → changement.
        assert!(metadata_identity_changed(
            Some(&old),
            &np("Blue in Green", Some("Miles Davis"))
        ));
        assert!(metadata_identity_changed(
            Some(&old),
            &np("So What", Some("Bill Evans"))
        ));
        assert!(metadata_identity_changed(Some(&old), &np("So What", None)));
    }

    #[tokio::test]
    async fn update_now_playing_stamps_anchor_only_on_identity_change() {
        let pm = PlaybackManager::new();
        let np = |title: &str| NowPlaying {
            title: title.into(),
            artist_name: Some("FIP".into()),
            source: "radio".into(),
            ..Default::default()
        };
        pm.play(9, np("Premier titre")).await;
        let anchor1 = pm.get_state(9).await.metadata_changed_at_ms;
        assert!(anchor1.is_some(), "play() doit poser l'ancrage");

        // Même métadonnée re-poussée : l'ancrage NE bouge PAS.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        pm.update_now_playing(9, np("Premier titre")).await;
        assert_eq!(pm.get_state(9).await.metadata_changed_at_ms, anchor1);

        // Changement de titre : l'ancrage est re-stampé (>= précédent).
        pm.update_now_playing(9, np("Deuxième titre")).await;
        let anchor2 = pm.get_state(9).await.metadata_changed_at_ms;
        assert!(anchor2.unwrap() > anchor1.unwrap());

        // L'événement WS embarque l'ancrage + un âge calculé côté serveur.
        let state = pm.get_state(9).await;
        let data = now_playing_event_data(&state);
        assert_eq!(data["metadata_changed_at"], anchor2.unwrap());
        assert!(data["metadata_age_ms"].as_i64().unwrap() >= 0);
    }

    /// #2588 — le constat de silence tient à travers l'arrêt, et pas au-delà
    /// de la lecture suivante.
    #[tokio::test]
    async fn le_constat_de_silence_survit_a_larret_et_meurt_a_la_relecture() {
        let pm = PlaybackManager::new();
        let np = || NowPlaying {
            title: "Track".into(),
            ..Default::default()
        };
        pm.play(4, np()).await;
        assert!(
            pm.get_state(4).await.browser_unattended_at.is_none(),
            "une lecture qui démarre n'a encore rien à expliquer"
        );
        pm.note_browser_unattended(4, true).await;
        pm.stop(4).await;
        assert!(
            pm.get_state(4).await.browser_unattended_at.is_some(),
            "l'arrêt ne doit pas emporter l'explication du silence"
        );
        pm.play(4, np()).await;
        assert!(
            pm.get_state(4).await.browser_unattended_at.is_none(),
            "une nouvelle lecture rouvre la question"
        );
        // Et la vue peut lever le constat sans attendre l'échéance.
        pm.note_browser_unattended(4, true).await;
        pm.note_browser_unattended(4, false).await;
        assert!(pm.get_state(4).await.browser_unattended_at.is_none());
    }
    #[tokio::test]
    async fn set_shuffle_emits_event() {
        let pm = PlaybackManager::new();
        let mut rx = pm.subscribe();
        pm.set_shuffle(7, true).await;
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.event, "shuffle");
        assert_eq!(ev.zone_id, 7);
        assert_eq!(ev.data["enabled"], true);
    }

    #[tokio::test]
    async fn set_repeat_emits_event() {
        let pm = PlaybackManager::new();
        let mut rx = pm.subscribe();
        pm.set_repeat(3, RepeatMode::All).await;
        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.event, "repeat");
        assert_eq!(ev.zone_id, 3);
        assert_eq!(ev.data["mode"], "all");
    }

    /// #1924 (Tades, fil forum 1471) — « je vois bien la piste que je souhaite
    /// écouter dans la file d'attente et la lecture en cours est finie depuis
    /// cinq minutes », et rien ne part.
    ///
    /// Le scénario reproduit ici est celui du démarrage du serveur.
    /// `restore_queue_metadata` (tune-server/src/startup.rs) annonce à la zone
    /// sa longueur de file, son mode de répétition et son aléatoire — dans cet
    /// ordre — avant que quoi que ce soit n'ait créé son état en mémoire. Le
    /// créateur habituel, `restore_playback_positions`, saute justement les
    /// zones dont la dernière piste venait d'un service (pas de
    /// `last_track_id`) ou dont la piste a disparu de la bibliothèque.
    ///
    /// Les trois écritures se faisaient alors en `get_mut` : trois no-op
    /// silencieux, suivis d'un `info!` qui annonçait la restauration. La zone
    /// démarrait avec `queue_length = 0`, `next_position()` rendait `None` dès
    /// le premier `if queue_length == 0`, et le sondeur concluait « file
    /// terminée » à la fin du premier morceau — alors que l'écran, qui lit la
    /// file en base, affichait toujours les suivantes.
    ///
    /// Aucune horloge, aucune course : le test tient sur l'ordre des appels.
    #[tokio::test]
    async fn la_file_restauree_au_demarrage_survit_a_une_zone_pas_encore_en_memoire() {
        let pm = PlaybackManager::new();

        pm.update_queue_info(7, 0, 3).await;
        pm.set_repeat(7, RepeatMode::Off).await;
        pm.set_shuffle(7, false).await;

        let state = pm.get_state(7).await;
        assert_eq!(
            state.queue_length, 3,
            "la longueur restaurée doit atteindre l'état de zone, pas le vide"
        );
        assert_eq!(state.queue_position, 0);
        assert_eq!(
            crate::poller::PositionPoller::next_position(&state),
            Some(1),
            "sans la longueur, next_position rend None et le sondeur arrête la \
             zone à la fin du premier morceau, file pleine à l'écran"
        );
    }

    /// Les deux sœurs du même trio. Corriger `update_queue_info` seule aurait
    /// laissé la répétition et l'aléatoire restaurés tomber dans le vide sur
    /// une zone dont l'état n'existe pas encore.
    #[tokio::test]
    async fn repetition_et_aleatoire_restaures_atteignent_une_zone_pas_encore_en_memoire() {
        let pm = PlaybackManager::new();

        pm.set_repeat(9, RepeatMode::All).await;
        pm.set_shuffle(9, true).await;

        let state = pm.get_state(9).await;
        assert_eq!(
            state.repeat,
            RepeatMode::All,
            "le mode de répétition restauré doit tenir"
        );
        assert!(state.shuffle, "l'aléatoire restauré doit tenir");
    }

    #[test]
    fn generate_shuffle_order_is_a_permutation_with_current_first() {
        let order = generate_shuffle_order(10, 4);
        assert_eq!(order.len(), 10);
        // Current track sits at index 0 so the first advance moves away from it.
        assert_eq!(order[0], 4);
        // Every index 0..10 appears exactly once (a true permutation).
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn generate_shuffle_order_edge_cases() {
        assert!(generate_shuffle_order(0, 0).is_empty());
        assert_eq!(generate_shuffle_order(1, 0), vec![0]);
        // current out of range is ignored (no panic), still a full permutation.
        let order = generate_shuffle_order(3, 99);
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, vec![0, 1, 2]);
    }

    #[test]
    fn from_track_maps_all_metadata_fields() {
        let mut track = crate::db::models::Track::new("Blue in Green".into());
        track.id = Some(42);
        track.artist_name = Some("Miles Davis".into());
        track.album_title = Some("Kind of Blue".into());
        track.cover_path = Some("/covers/kob.jpg".into());
        track.duration_ms = 337_000;
        track.source = "local".into();
        track.source_id = Some("row-42".into());
        track.format = Some("flac".into());
        track.sample_rate = Some(44_100);
        track.bit_depth = Some(24);
        track.genre = Some("Jazz".into());
        track.year = Some(1959);

        let np = NowPlaying::from_track(&track);
        assert_eq!(np.track_id, Some(42));
        assert_eq!(np.title, "Blue in Green");
        assert_eq!(np.artist_name.as_deref(), Some("Miles Davis"));
        assert_eq!(np.album_title.as_deref(), Some("Kind of Blue"));
        assert_eq!(np.cover_path.as_deref(), Some("/covers/kob.jpg"));
        assert_eq!(np.duration_ms, 337_000);
        assert_eq!(np.source, "local");
        assert_eq!(np.source_id.as_deref(), Some("row-42"));
        // Local rows are not streamed sessions.
        assert_eq!(np.stream_id, None);
        assert_eq!(np.format.as_deref(), Some("flac"));
        // i32 → u32 casts on the audio-format fields.
        assert_eq!(np.sample_rate, Some(44_100));
        assert_eq!(np.bit_depth, Some(24));
        assert_eq!(np.genre.as_deref(), Some("Jazz"));
        assert_eq!(np.year, Some(1959));
    }

    #[test]
    fn from_track_preserves_source_over_output_bit_depth() {
        // The source depth (24) must survive verbatim — the constructor never
        // substitutes the 32-bit WAV output depth used for local DAC playback.
        let mut track = crate::db::models::Track::new("t".into());
        track.bit_depth = Some(24);
        track.sample_rate = Some(96_000);
        assert_eq!(NowPlaying::from_track(&track).bit_depth, Some(24));
        assert_eq!(NowPlaying::from_track(&track).sample_rate, Some(96_000));

        // Missing audio metadata maps to None, not a fabricated default.
        let bare = crate::db::models::Track::new("bare".into());
        let np = NowPlaying::from_track(&bare);
        assert_eq!(np.bit_depth, None);
        assert_eq!(np.sample_rate, None);
        assert_eq!(np.format, None);
    }

    /// Le drapeau de recherche doit retomber quand la lecture démarre.
    ///
    /// Il ne retombait QUE sur deux chemins d'erreur de `play_inner`. Sur le
    /// chemin qui réussit, rien ne l'abaissait : `play()` n'y touchait pas, et
    /// son `entry().or_insert_with()` ne réinitialise une zone existante pour
    /// personne. Une zone restait donc annoncée « recherche en cours » pendant
    /// toute sa lecture — au point que l'indication ne voulait plus rien dire,
    /// ce qui est pire que son absence.
    ///
    /// Le commentaire de `play_inner` affirmait pourtant : « Le drapeau retombe
    /// dans `play()`, dès qu'une URL jouable existe ». L'intention était écrite
    /// et l'instruction manquait ; seul un test pouvait faire la différence.
    #[tokio::test]
    async fn la_recherche_se_termine_quand_la_lecture_demarre() {
        let pm = super::PlaybackManager::new();
        pm.set_resolving(1, true).await;
        assert!(
            pm.get_state(1).await.resolving,
            "le drapeau doit être levé avant la résolution"
        );

        pm.play(1, super::NowPlaying::default()).await;

        assert!(
            !pm.get_state(1).await.resolving,
            "une lecture qui démarre signifie qu'une URL jouable existe : la \
             recherche est finie"
        );
    }

    /// Même exigence à l'arrêt : une lecture interrompue pendant la résolution
    /// laissait la zone à l'arrêt ET annoncée « recherche en cours ».
    #[tokio::test]
    async fn un_arret_met_fin_a_la_recherche() {
        let pm = super::PlaybackManager::new();
        pm.set_resolving(1, true).await;
        pm.stop(1).await;
        assert!(!pm.get_state(1).await.resolving);

        pm.set_resolving(2, true).await;
        pm.stop_and_clear(2).await;
        assert!(!pm.get_state(2).await.resolving);
    }

    /// #2108 — la garde de veille appartient à l'ensemble des zones, pas à la
    /// dernière commande reçue. Une pause ne doit jamais endormir le serveur
    /// si une autre zone continue de jouer.
    #[tokio::test]
    async fn inhibition_de_veille_suit_la_derniere_zone_qui_joue() {
        let pm = super::PlaybackManager::new();
        assert!(!pm.sleep_inhibitor.requested());

        pm.play(1, super::NowPlaying::default()).await;
        assert!(pm.sleep_inhibitor.requested());

        pm.play(2, super::NowPlaying::default()).await;
        pm.pause(1).await;
        assert!(
            pm.sleep_inhibitor.requested(),
            "la zone 2 joue encore : la pause de la zone 1 ne libère rien"
        );

        pm.stop(2).await;
        assert!(
            !pm.sleep_inhibitor.requested(),
            "la dernière zone active est arrêtée : la garde doit être libérée"
        );

        pm.resume(1).await;
        assert!(pm.sleep_inhibitor.requested());
        pm.stop_and_clear(1).await;
        assert!(!pm.sleep_inhibitor.requested());
    }

    /// #2876 — le marqueur de reprise vit exactement le temps qu'il faut.
    ///
    /// Posé par la restauration du démarrage, effacé par la première lecture
    /// réelle. Un Stop en cours de session ne le pose PAS : la position qu'il
    /// conserve sert l'affichage, et la faire rejouer ferait sauter en avant un
    /// clic sur la même piste depuis la bibliothèque, ou une file arrivée à son
    /// terme (dont la position vaut la durée).
    #[tokio::test]
    async fn le_marqueur_de_reprise_est_a_usage_unique() {
        let pm = super::PlaybackManager::new();
        let np = super::NowPlaying {
            track_id: Some(42),
            duration_ms: 300_000,
            ..Default::default()
        };

        pm.restore_position(1, 151_000, np.clone()).await;
        assert_eq!(
            pm.pending_resume_ms(1).await,
            Some(151_000),
            "la position rendue par la base doit être offerte à la première lecture (#2876)"
        );
        assert_eq!(pm.get_state(1).await.position_ms, 151_000);

        pm.play(1, np.clone()).await;
        assert_eq!(
            pm.pending_resume_ms(1).await,
            None,
            "le flux est parti : la position restaurée est consommée, pas rejouable"
        );

        // Témoin : un Stop en session conserve la position pour l'écran mais
        // n'arme aucune reprise.
        pm.stop(1).await;
        assert_eq!(pm.pending_resume_ms(1).await, None);

        // Une position nulle n'arme rien non plus.
        pm.restore_position(2, 0, np.clone()).await;
        assert_eq!(pm.pending_resume_ms(2).await, None);

        // File vidée : plus rien à reprendre.
        pm.restore_position(3, 90_000, np.clone()).await;
        assert_eq!(pm.pending_resume_ms(3).await, Some(90_000));
        pm.stop_and_clear(3).await;
        assert_eq!(pm.pending_resume_ms(3).await, None);

        // L'ancrage que fait la route avant de demander la lecture : sans le
        // `seek()`, `play()` remettrait le curseur à zéro et l'écran
        // recommencerait à mentir — dans l'autre sens cette fois.
        pm.restore_position(4, 151_000, np.clone()).await;
        pm.seek(4, 151_000).await;
        pm.play(4, np).await;
        assert_eq!(
            pm.get_state(4).await.position_ms,
            151_000,
            "le curseur doit suivre le son, pas retomber à 0:00 (#2876)"
        );
    }
}
