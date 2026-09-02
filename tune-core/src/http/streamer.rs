use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::{Mutex, mpsc};
use tracing::info;

use crate::audio::wav::{
    build_wav_header_bounded_live as wav_header_bounded_live,
    build_wav_header_streaming as wav_header_streaming, build_wav_header_with_duration,
};

pub use crate::audio::wav::{LIVE_BOUNDED_DATA_SIZE, LIVE_BOUNDED_TOTAL_LEN};

pub const ICY_METAINT: usize = 16384;

/// Silence sur le fil au bout duquel une session est déclarée morte.
///
/// Trente minutes : c'est le seuil qui existait déjà, mais compté sur
/// l'INACTIVITÉ et non sur l'âge (#2536). Le garder à l'identique a deux
/// vertus. Une session orpheline — préparation gapless abandonnée, lecture
/// interrompue, renderer qui n'a jamais tiré un octet — n'a par construction
/// jamais d'activité : son inactivité se confond avec son âge et elle part à
/// la même minute qu'avant. Et le raccourcir serait une régression : une
/// lecture EN PAUSE ne tire plus rien du serveur, une borne courte la
/// ramasserait alors qu'elle survit une demi-heure aujourd'hui.
pub const SESSION_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1800);

/// Filet : durée de vie maximale d'une session, activité ou pas.
///
/// Sans lui, passer à l'inactivité ouvrirait une fuite : un consommateur qui
/// suinte quelques octets de temps en temps, ou un renderer qui garde sa
/// connexion ouverte sans jamais finir, épinglerait indéfiniment la session,
/// son fichier de pré-transcodage et sa connexion amont. Vingt-quatre heures
/// est hors d'atteinte de toute piste réelle — la plus longue ici est une
/// image de CD d'un seul tenant, ~80 min, et un acte d'opéra ou un set
/// enregistré restent loin sous la journée — donc ce plafond ne peut pas
/// couper une écoute, tout en garantissant que rien ne traîne plus d'un jour.
pub const SESSION_ABSOLUTE_CAP: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

/// A boxed, cloneable async closure that re-resolves a fresh signed CDN URL
/// for the track backing a proxy session.
///
/// Streaming services (Qobuz, Tidal, …) hand out short-TTL signed CDN URLs
/// (Qobuz embeds `etsp=<unix-expiry>` + `hmac=`; the TTL is ~60 min). On a
/// long Hi-Res track — or after a long pause — the URL expires mid-playback,
/// so a client-triggered `Range: bytes=N-` resume against the stored URL fails
/// at the connection/auth level (reqwest "error sending request", or 403/410).
/// Re-fetching the SAME expired URL can never succeed; the proxy layer instead
/// calls this to obtain a FRESH signed URL for the same file and resumes the
/// Range request byte-exact.
///
/// The future resolves to a fresh `https://…` CDN URL for the same track and
/// quality, or an error string. It is `Send` and captures only cheap
/// clones (an `Arc` registry handle + the service name / track id / quality),
/// so it can be invoked any number of times over the life of the session.
pub type ReresolveFn = std::sync::Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send>>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, Default)]
pub struct StreamInfo {
    pub format: String,
    pub mime_type: String,
    pub sample_rate: u32,
    pub bit_depth: u16,
    pub channels: u16,
    pub file_size: Option<u64>,
    pub duration_ms: Option<u64>,
    pub seek_ms: Option<u64>,
}

impl StreamInfo {
    /// Calculate the expected WAV file size from audio parameters and duration.
    /// Returns `44 + data_bytes` (WAV header + raw PCM data).
    ///
    /// `data_bytes` est **arrondi vers le bas sur une trame entière**. Une
    /// durée en millisecondes ne tombe presque jamais sur un nombre entier de
    /// trames : 192 705 ms en 44,1 kHz / 16 bits / stéréo donnent
    /// 192 705 × 176 400 / 1 000 = 33 993 162 octets, soit 8 498 290 trames
    /// **plus 2 octets**. Cette longueur est annoncée en `Content-Length` ; le
    /// client HTTP livre exactement ce nombre d'octets puis signale une fin de
    /// corps propre, et les 1 à 3 derniers octets ne sont pas de l'audio — ils
    /// sont une fraction de trame. Côté OAAT, `StreamingPcmByteAdapter::finish()`
    /// les retrouvait dans son reliquat et faisait échouer la piste APRÈS
    /// qu'elle a été jouée en entier, ce qui coupait la zone et annulait une
    /// transition gapless déjà prête (#3163, Steve Taylor, fil 1641 : reliquat
    /// de 2 octets à 21:57:32, puis de 1 octet à 22:09:29).
    ///
    /// Un octet de moins annoncé ne retire aucun audio : ces octets n'existent
    /// pas dans le flux décodé, la durée réelle diffère de toute façon de la
    /// durée en bibliothèque.
    pub fn wav_content_length(&self) -> Option<u64> {
        let dur = self.duration_ms?;
        if self.sample_rate == 0 || self.channels == 0 || self.bit_depth == 0 {
            return None;
        }
        let bytes_per_sample = self.bit_depth as u64 / 8;
        let frame_bytes = self.channels as u64 * bytes_per_sample;
        if frame_bytes == 0 {
            return None;
        }
        let data_bytes = dur * self.sample_rate as u64 * frame_bytes / 1000;
        Some(44 + data_bytes - data_bytes % frame_bytes)
    }
}

pub struct StreamSession {
    pub id: String,
    pub info: StreamInfo,
    pub tx: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    /// Keeps the channel open until the session is removed, even after the
    /// decoder drops its tx. Without this, the HTTP stream ends as soon as
    /// the decoder finishes, before ASIO/WASAPI has consumed all buffered data.
    _keep_alive_tx: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    rx: Mutex<mpsc::Receiver<Vec<u8>>>,
    pub file_path: Mutex<Option<String>>,
    /// On-the-fly M4A faststart map: when set, the file is served as
    /// `ftyp + patched-moov` (from memory) followed by the original mdat, so a
    /// DLNA renderer gets the metadata up front without seeking to the file end.
    pub faststart: std::sync::Mutex<Option<crate::audio::faststart::FaststartMap>>,
    pub proxy_url: Mutex<Option<String>>,
    /// Re-resolver for the proxied CDN URL. Set for streaming proxy sessions
    /// whose signed URL can expire (Qobuz/Tidal). When a proxied upstream
    /// request fails to send or returns 403/410 (expired signature), the proxy
    /// layer calls this to fetch a fresh signed URL and resumes byte-exact.
    /// `None` for local files, radio, and non-expiring sources.
    pub reresolve: Mutex<Option<ReresolveFn>>,
    pub track_title: Option<String>,
    pub track_artist: Option<String>,
    pub track_album: Option<String>,
    pub cover_url: Option<String>,
    pub bit_perfect: bool,
    pub is_radio: bool,
    /// True output format detected by the radio decoder once the upstream is
    /// probed. `info` contains only the bootstrap format used while the session
    /// is being created; every runtime consumer must use
    /// [`StreamSession::detected_output_format`] so a 48 kHz station is never
    /// reported as the 44.1 kHz bootstrap value. `0` means "not yet detected".
    pub detected_sample_rate: std::sync::atomic::AtomicU32,
    pub detected_channels: std::sync::atomic::AtomicU16,
    pub wav_header_included: std::sync::atomic::AtomicBool,
    /// L'en-tête WAV tel que le décodeur l'a émis — pour le REJOUER.
    ///
    /// Sur une session de conversion (DSD→WAV), l'en-tête part DANS le canal,
    /// comme premier chunk. Un canal ne se lit qu'une fois : seule la première
    /// connexion HTTP le recevait. Or un renderer qui sonde (DMP-A8 : trois
    /// requêtes — `bytes=0-`, `bytes=44-`, `bytes=0-`) consomme l'en-tête sur
    /// une connexion qu'il referme, puis joue sur une autre — qui recevait du
    /// PCM nu, sans RIFF. Le consommateur qui voit passer l'en-tête le range
    /// ici ; chaque nouvelle connexion partant de l'octet 0 le reçoit d'abord.
    pub wav_header_stash: std::sync::OnceLock<Vec<u8>>,
    pub created_at: Instant,
    pub bytes_sent: std::sync::atomic::AtomicU64,
    /// Comptabilité du ramasse-miettes — voir `cleanup_stale_sessions_with`.
    ///
    /// `bytes_sent` est monotone et alimenté par TOUS les chemins de sortie
    /// (fichier, radio, mandataire) dans `corps_compte` / `build_file_body` :
    /// c'est déjà la mesure d'activité du serveur, celle dont le poller et le
    /// chien de garde de sortie locale déduisent « personne n'écoute ». Le GC
    /// la relit à chaque balayage plutôt que d'ajouter un second compteur.
    /// `gc_seen_bytes` retient la valeur du balayage précédent ; dès qu'elle
    /// bouge, `gc_active_at_ms` est réarmé sur l'âge courant. L'inactivité
    /// vaut donc `created_at.elapsed() - gc_active_at_ms`. Deux entiers
    /// atomiques, parce que le verdict se rend dans un `retain` synchrone où
    /// l'on ne peut ni `await` ni prendre un `Mutex` tokio.
    pub gc_seen_bytes: std::sync::atomic::AtomicU64,
    /// Âge (ms depuis `created_at`) auquel le GC a vu `bytes_sent` bouger pour
    /// la dernière fois. `0` = jamais : l'inactivité se confond alors avec
    /// l'âge, et une session orpheline est reprise exactement comme avant.
    pub gc_active_at_ms: std::sync::atomic::AtomicU64,
    /// Number of HTTP requests currently streaming this session. The radio→WAV
    /// channel is single-consumer (`rx` behind a Mutex): a second concurrent
    /// reader would race the first for each PCM chunk and split the stream. This
    /// counter surfaces that case for diagnostics (a renderer that re-requests
    /// without closing its first connection — DMP-A8 FIP silent-after-reconnect).
    pub active_consumers: std::sync::atomic::AtomicU32,
    /// The local-output watchdog is armed at most once for this session. A
    /// session prepared for gapless remains unarmed until its URL is actually
    /// handed to the local output.
    pub consumer_watch_armed: std::sync::atomic::AtomicBool,
    /// Monotonic token identifying the CURRENT owner of the single-consumer
    /// radio PCM channel. A DLNA renderer that re-requests the radio stream
    /// (buffer refill / reconnect) without closing its first connection would
    /// otherwise have BOTH connections race `recv_chunk()` for each PCM chunk,
    /// splitting the audio between them → dropouts. Each new radio consumer
    /// calls `claim_channel_consumer()` to bump this and supersede the older
    /// connection, which then hands off the channel and ends. `0` = no consumer.
    pub consumer_epoch: std::sync::atomic::AtomicU64,
    /// Vrai dès que le canal PCM de cette session a été observé PLEIN, donc
    /// dès que le producteur a pris de l'avance sur le lecteur au moins une
    /// fois. Sans ce témoin, « canal vide » ne veut rien dire : au démarrage
    /// il l'est toujours, le temps que le décodeur prenne son avance.
    pub channel_was_full: std::sync::atomic::AtomicBool,
    /// Vrai une fois l'alerte `stream_producer_ran_dry` émise. Une seule
    /// ligne par session : elle dit un FAIT ponctuel — le producteur a cessé
    /// d'être en avance — et le répéter à chaque morceau noierait le journal.
    pub dry_alert_emitted: std::sync::atomic::AtomicBool,
    /// Wakes an older radio consumer the instant a newer one claims the channel
    /// so it releases the `rx` lock promptly instead of staying parked in
    /// `recv_chunk()`. Paired with `consumer_epoch`; see `claim_channel_consumer`.
    pub consumer_supersede: std::sync::Arc<tokio::sync::Notify>,
    pub first_request: std::sync::Arc<tokio::sync::Notify>,
    pub data_ready: std::sync::Arc<tokio::sync::Notify>,
    /// True once the PRODUCER task feeding this session (the radio decode
    /// thread) has exited — whatever the exit path: clean upstream EOF,
    /// `radio_reconnect_giving_up`, consumer dropped, error or panic. Several
    /// of those paths log only at `debug!`, so in production the producer can
    /// die with **no visible trace** while the session object stays alive.
    /// `Orchestrator::resume` reads this to detect that resuming a webradio
    /// would feed silence (nothing produces PCM anymore) and re-plays the
    /// station instead (#1629).
    pub producer_done: std::sync::atomic::AtomicBool,
}

impl StreamSession {
    /// Une session-CANAL : la conversion à la volée (DSD→WAV), dont les octets
    /// ne se lisent qu'une fois. Ni fichier sur disque (Range réel), ni
    /// mandataire (Range relayé en amont), ni radio (déjà annoncée live).
    /// C'est le cas où annoncer la seekabilité est un mensonge.
    pub async fn is_channel(&self) -> bool {
        !self.is_radio
            && self.file_path.lock().await.is_none()
            && self.proxy_url.lock().await.is_none()
    }

    /// Publish the PCM format actually produced by the radio decoder.
    ///
    /// Channels are stored first and the sample rate is the release marker.
    /// Readers that acquire a non-zero sample rate therefore never observe a
    /// new rate paired with the old channel count.
    pub fn publish_detected_output_format(&self, sample_rate: u32, channels: u16) {
        if sample_rate == 0 || channels == 0 {
            return;
        }
        self.detected_channels
            .store(channels, std::sync::atomic::Ordering::Relaxed);
        self.detected_sample_rate
            .store(sample_rate, std::sync::atomic::Ordering::Release);
    }

    /// PCM format actually produced by the radio decoder, once known.
    pub fn detected_output_format(&self) -> Option<(u32, u16)> {
        let sample_rate = self
            .detected_sample_rate
            .load(std::sync::atomic::Ordering::Acquire);
        if sample_rate == 0 {
            return None;
        }
        let channels = self
            .detected_channels
            .load(std::sync::atomic::Ordering::Relaxed);
        (channels != 0).then_some((sample_rate, channels))
    }

    fn effective_output_info(&self) -> StreamInfo {
        let mut info = self.info.clone();
        if self.is_radio {
            if let Some((sample_rate, channels)) = self.detected_output_format() {
                info.sample_rate = sample_rate;
                info.channels = channels;
            }
        }
        info
    }

    pub fn new(id: String, info: StreamInfo, bit_perfect: bool, buffer_size: usize) -> Self {
        let (tx, rx) = mpsc::channel(buffer_size);
        let keep_alive = tx.clone();
        Self {
            id,
            info,
            tx: Mutex::new(Some(tx)),
            _keep_alive_tx: Mutex::new(Some(keep_alive)),
            rx: Mutex::new(rx),
            file_path: Mutex::new(None),
            faststart: std::sync::Mutex::new(None),
            proxy_url: Mutex::new(None),
            reresolve: Mutex::new(None),
            track_title: None,
            track_artist: None,
            track_album: None,
            cover_url: None,
            bit_perfect,
            is_radio: false,
            detected_sample_rate: std::sync::atomic::AtomicU32::new(0),
            detected_channels: std::sync::atomic::AtomicU16::new(0),
            wav_header_included: std::sync::atomic::AtomicBool::new(false),
            wav_header_stash: std::sync::OnceLock::new(),
            created_at: Instant::now(),
            bytes_sent: std::sync::atomic::AtomicU64::new(0),
            gc_seen_bytes: std::sync::atomic::AtomicU64::new(0),
            gc_active_at_ms: std::sync::atomic::AtomicU64::new(0),
            active_consumers: std::sync::atomic::AtomicU32::new(0),
            consumer_watch_armed: std::sync::atomic::AtomicBool::new(false),
            consumer_epoch: std::sync::atomic::AtomicU64::new(0),
            channel_was_full: std::sync::atomic::AtomicBool::new(false),
            dry_alert_emitted: std::sync::atomic::AtomicBool::new(false),
            consumer_supersede: std::sync::Arc::new(tokio::sync::Notify::new()),
            first_request: std::sync::Arc::new(tokio::sync::Notify::new()),
            data_ready: std::sync::Arc::new(tokio::sync::Notify::new()),
            producer_done: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub async fn recv_chunk(&self) -> Option<Vec<u8>> {
        self.rx.lock().await.recv().await
    }

    /// Register a new HTTP consumer of the single-consumer PCM channel and
    /// return its epoch token. Vaut pour TOUTE session adossée au canal —
    /// radio comme conversion finie (DSD→WAV). Le canal ne se lit qu'une
    /// fois : deux connexions simultanées se VOLENT les chunks, et le
    /// renderer n'entend qu'une fraction du signal. Any consumer registered earlier is SUPERSEDED:
    /// its epoch no longer matches (so `is_current_channel_consumer` returns
    /// false) and it is woken via `consumer_supersede` so it drops its pending
    /// `recv_chunk()` (releasing the `rx` lock) and ends — handing the channel
    /// to this newest connection instead of racing it for each PCM chunk.
    ///
    /// The consumer loop must use the lost-wakeup-free pattern: subscribe to
    /// `consumer_supersede` (create + `enable()` the `Notified`) BEFORE calling
    /// `is_current_channel_consumer`, so the epoch bump (which happens-before the
    /// notify) is observed either as a wake or as a stale epoch. That, together
    /// with a `biased` select that polls the supersede branch first, guarantees
    /// a superseded consumer stops WITHOUT pulling a further chunk (no split,
    /// no loss, no duplication at the swap).
    pub fn claim_channel_consumer(&self) -> u64 {
        use std::sync::atomic::Ordering::SeqCst;
        let epoch = self.consumer_epoch.fetch_add(1, SeqCst) + 1;
        // Wake any older consumer currently parked so it re-checks its epoch.
        self.consumer_supersede.notify_waiters();
        epoch
    }

    /// True while `epoch` (from `claim_channel_consumer`) is still the current
    /// channel owner. Becomes false once a newer consumer claims the channel.
    pub fn is_current_channel_consumer(&self, epoch: u64) -> bool {
        self.consumer_epoch
            .load(std::sync::atomic::Ordering::SeqCst)
            == epoch
    }

    pub async fn close_sender(&self) {
        self.tx.lock().await.take();
        self._keep_alive_tx.lock().await.take();
    }

    /// Current fill of the mpsc channel backing this session as
    /// `(buffered_messages, max_messages)`, or `None` once the channel has been
    /// closed (its keep-alive sender was dropped by `close_sender`) — the caller
    /// can no longer measure fill and should stop waiting on it.
    ///
    /// `buffered_messages = max_capacity - capacity`: the chunks a producer has
    /// pushed that no HTTP reader has consumed yet. Used ONLY by the initial
    /// DLNA prebuffer barrier (#1259) to tell how much audio has accumulated in
    /// a transcode/radio channel before `Play` is sent. Read-only — it never
    /// touches the stream itself.
    pub async fn channel_fill(&self) -> Option<(usize, usize)> {
        let guard = self._keep_alive_tx.lock().await;
        guard.as_ref().map(|tx| {
            let max = tx.max_capacity();
            (max.saturating_sub(tx.capacity()), max)
        })
    }

    /// Enregistre un remplissage observé du canal et dit s'il faut ALERTER.
    ///
    /// Rend `true` la PREMIÈRE fois que le canal, après avoir été observé
    /// PLEIN, est trouvé VIDE — et jamais ensuite. C'est le seul instant qui
    /// se lit sans ambiguïté : le producteur avait pris de l'avance, il ne
    /// l'a plus. Un canal vide au DÉMARRAGE ne dit rien (il l'est toujours,
    /// le temps que le décodeur démarre), et c'est pourquoi la condition
    /// n'est pas « vide » mais « vide après avoir été plein ».
    ///
    /// C'est la trace symétrique de `local_audio_slow_read` (outputs/local.rs)
    /// : celle-ci dit qu'on a attendu, celle-là dit QUI faisait attendre. Une
    /// attente de la sortie locale SANS cette ligne dans la même fenêtre
    /// disculpe le producteur — le canal était plein, les octets étaient là.
    pub fn note_channel_fill(&self, buffered: usize, max: usize) -> bool {
        use std::sync::atomic::Ordering::Relaxed;
        if max > 0 && buffered >= max {
            self.channel_was_full.store(true, Relaxed);
            return false;
        }
        if buffered > 0 || !self.channel_was_full.load(Relaxed) {
            return false;
        }
        !self.dry_alert_emitted.swap(true, Relaxed)
    }
}

/// Type alias for the shared sessions map, used by both core and server.
pub type SharedSessions = Arc<Mutex<HashMap<String, Arc<StreamSession>>>>;

// ---------------------------------------------------------------------------
// Métadonnées ICY en direct — canal latéral entre le poller et le flux
// ---------------------------------------------------------------------------
//
// Le bloc ICY envoyé à un renderer était construit UNE FOIS, à la connexion,
// puis réémis à l'identique toutes les 16 Ko. Un Marantz branché sur Radio
// Paradise affichait donc éternellement le morceau qui passait à l'instant du
// branchement (signalé sur le forum, 10 août).
//
// Le titre courant existe pourtant : le poller l'obtient déjà via
// `radio_metadata::fetch_radio_metadata` et met à jour le now-playing de la
// zone — c'est ce que l'interface web affiche. Mais les deux sous-systèmes
// s'ignorent : la session de flux n'a pas de zone, et le poller n'a pas les
// sessions.
//
// Plutôt que de faire traverser `SharedSessions` au constructeur du poller —
// qui n'en a besoin que pour ça — ce registre étroit fait le lien, par
// `stream_id`, la seule clé que les deux connaissent déjà. Il est délibérément
// minuscule : une entrée par flux radio en cours.
//
// ── Pourquoi la POCHETTE voyage ici aussi ──
//
// Le registre ne portait que l'artiste et le titre. La pochette du bloc ICY
// était lue sur `StreamSession::cover_url` — un champ posé à `None` par
// `StreamSession::new` et écrit NULLE PART ailleurs du dépôt, exactement comme
// `track_title` et `track_artist` l'étaient avant #2161. `StreamUrl='…'` n'a
// donc jamais quitté ce serveur : `build_icy_metadata` ne l'ajoute que sur un
// `Some`, et l'argument était toujours `None`.
//
// Le poller, lui, connaît la pochette du morceau courant : il la calcule par
// `vignette_du_pas_radio` et la pose dans le now-playing — c'est pour ça que
// l'interface Tune la voit changer pendant que l'écran du renderer reste sur
// la première (Serge Asselin, Hifi Rose RS250A, fil 1529). Elle emprunte donc
// le même canal étroit que le titre, par le même `stream_id`.
static RADIO_NOW: std::sync::LazyLock<std::sync::Mutex<HashMap<String, RadioNow>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Le morceau qui passe à l'antenne sur un flux radio, tel que le poller le
/// connaît et tel que le bloc ICY doit le rendre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RadioNow {
    pub artist: Option<String>,
    pub title: String,
    /// La pochette du morceau courant quand la station la donne, le logo de la
    /// station sinon — c'est déjà l'arbitrage rendu par `vignette_du_pas_radio`
    /// côté poller, et on ne le refait pas ici.
    pub cover: Option<String>,
}

/// Publier le titre courant d'un flux radio. Appelé par le poller quand il
/// détecte un changement de morceau.
pub fn publish_radio_now(
    stream_id: &str,
    artist: Option<String>,
    title: String,
    cover: Option<String>,
) {
    if let Ok(mut map) = RADIO_NOW.lock() {
        map.insert(
            stream_id.to_string(),
            RadioNow {
                artist,
                title,
                cover,
            },
        );
    }
}

/// Lire le titre courant d'un flux radio, s'il a été publié.
pub fn radio_now(stream_id: &str) -> Option<RadioNow> {
    RADIO_NOW.lock().ok()?.get(stream_id).cloned()
}

/// Oublier un flux terminé — sans quoi le registre grossirait indéfiniment.
pub fn forget_radio_now(stream_id: &str) {
    if let Ok(mut map) = RADIO_NOW.lock() {
        map.remove(stream_id);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Par où le renderer apprend qu'un morceau a changé (#2991)
// ─────────────────────────────────────────────────────────────────────────────
//
// `publish_radio_now` ci-dessus DÉPOSE le titre courant. Il ne dit rien de ce
// qui va le RELIRE. Or le seul canal qui puisse porter un changement de morceau
// jusqu'à un renderer DLNA en cours de flux est la fenêtre ICY, et elle ne
// s'ouvre que si trois choses se rencontrent :
//
//   1. le now-playing de la zone porte un `stream_id` — sans lui, le dépôt
//      ci-dessus n'a même pas lieu (la garde du poller) ;
//   2. la requête du renderer a été servie par la branche « flux » de
//      `handle_stream` — les branches fichier et mandataire n'insèrent AUCUN
//      bloc ICY, quoi que le renderer ait demandé ;
//   3. le renderer a demandé les métadonnées en cours de flux (en-tête ICY de
//      requête) et la fenêtre `icy-metaint` lui a été accordée.
//
// Quand l'une des trois manque, tout continue de fonctionner CÔTÉ TUNE — le
// now-playing de l'interface est mis à jour par un autre chemin — et l'écran du
// lecteur réseau reste figé sur le premier morceau. C'est exactement le tableau
// décrit par Serge Asselin (Hifi Rose RS250A, fil 1529) : « dans Tune ça
// fonctionne, mais sur le RS250A l'écran demeure avec la pochette et le titre de
// la première écoute ».
//
// On ne pouvait pas trancher laquelle des trois mordait : rien ne les
// rapprochait. Ce registre le fait, par le `stream_id`, la seule clé que le
// gestionnaire de flux et le poller partagent déjà — même patron que
// `RADIO_NOW`, et la même durée de vie (voir `remove_session`).

/// La branche de `handle_stream` qui a servi la requête. Seule [`VOIE_FLUX`]
/// sait découper le corps et y insérer des blocs ICY (`decoupe_icy`) ; les deux
/// autres recopient des octets et ne peuvent, par construction, porter aucune
/// mise à jour de métadonnées.
pub const VOIE_FLUX: &str = "flux";
/// Branche fichier (`serve_file`) : Range, `Content-Length`, aucun ICY.
pub const VOIE_FICHIER: &str = "fichier";
/// Branche mandataire (`proxy_stream`) : recopie de l'amont, aucun ICY.
pub const VOIE_MANDATAIRE: &str = "mandataire";

/// Ce qui a été négocié avec le renderer sur une session de flux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanalIcy {
    /// Le renderer a demandé les métadonnées en cours de flux (en-tête ICY de
    /// requête ; le gestionnaire de flux le lit sous le nom `wants_icy`).
    pub demande: bool,
    /// La fenêtre `icy-metaint` lui a effectivement été annoncée.
    pub accorde: bool,
    /// La branche qui l'a servi (une des trois constantes ci-dessus).
    pub voie: &'static str,
}

static ICY_NEGOCIE: std::sync::LazyLock<std::sync::Mutex<HashMap<String, CanalIcy>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Noter ce qui a été négocié pour une session. Appelé par le gestionnaire de
/// flux sur les TROIS branches — y compris celles qui ne portent pas d'ICY :
/// « aucune trace » et « servi par une voie sans ICY » sont deux diagnostics
/// différents, et c'est justement celui-là qu'on n'avait pas.
pub fn note_icy_channel(stream_id: &str, demande: bool, accorde: bool, voie: &'static str) {
    if let Ok(mut map) = ICY_NEGOCIE.lock() {
        map.insert(
            stream_id.to_string(),
            CanalIcy {
                demande,
                accorde,
                voie,
            },
        );
    }
}

/// Relire ce qui a été négocié, si un renderer s'est connecté à ce flux.
pub fn icy_channel(stream_id: &str) -> Option<CanalIcy> {
    ICY_NEGOCIE.lock().ok()?.get(stream_id).copied()
}

/// Oublier une session terminée (même vie que [`forget_radio_now`]).
pub fn forget_icy_channel(stream_id: &str) {
    if let Ok(mut map) = ICY_NEGOCIE.lock() {
        map.remove(stream_id);
    }
}

/// Le verdict : par où — ou par où PAS — le changement de morceau atteint le
/// renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanalRadio {
    /// Fenêtre ICY accordée : les blocs partent, l'écran peut suivre.
    Icy,
    /// Servi par une branche qui n'insère aucun bloc ICY (fichier, mandataire).
    VoieSansIcy,
    /// Le renderer n'a pas demandé les métadonnées en cours de flux.
    IcyNonDemande,
    /// Demandé, mais la session ne remplissait pas les conditions d'ouverture.
    IcyRefuse,
    /// Un `stream_id` existe, mais aucun renderer ne s'y est encore connecté.
    AucuneConnexion,
    /// Le now-playing ne porte AUCUN `stream_id` : rien n'est même publié.
    SansStreamId,
}

impl CanalRadio {
    /// Le changement de morceau atteint-il réellement le renderer ?
    pub fn atteint_le_renderer(self) -> bool {
        matches!(self, CanalRadio::Icy)
    }

    /// Le libellé qui part au journal. Écrit pour être lu SEUL : une seule
    /// ligne doit suffire à savoir laquelle des branches mord.
    ///
    /// L'en-tête de requête n'y est pas épelé : le garde #3018
    /// (`tune-core/tests/pochette_radio_source_unique.rs`) réserve cette chaîne
    /// à `src/radio_metadata.rs`, pour qu'une recherche dans le code ne trouve
    /// qu'UN endroit où l'on lit les métadonnées radio. Le gestionnaire de flux
    /// le nomme, lui, dans `icy_metadata_negotiated`.
    pub fn libelle(self) -> &'static str {
        match self {
            CanalRadio::Icy => "icy — fenêtre accordée, les blocs partent",
            CanalRadio::VoieSansIcy => {
                "aucun — servi par une voie qui n'insère aucun bloc ICY (fichier ou mandataire)"
            }
            CanalRadio::IcyNonDemande => {
                "aucun — le renderer n'a pas demandé les métadonnées en cours de flux"
            }
            CanalRadio::IcyRefuse => "aucun — métadonnées demandées, fenêtre ICY refusée",
            CanalRadio::AucuneConnexion => "aucun — aucun renderer connecté à ce flux",
            CanalRadio::SansStreamId => "aucun — le now-playing ne porte pas de stream_id",
        }
    }
}

/// Trancher, pour le `stream_id` d'un now-playing radio, par où le changement
/// de morceau va passer.
///
/// Fonction PURE au sens qui compte ici : elle ne lit que les deux registres et
/// se prouve donc sans réseau, sans renderer et sans Hifi Rose. C'est elle que
/// le poller appelle, et c'est elle que les épreuves appellent.
pub fn canal_radio(stream_id: Option<&str>) -> CanalRadio {
    let Some(sid) = stream_id else {
        return CanalRadio::SansStreamId;
    };
    match icy_channel(sid) {
        None => CanalRadio::AucuneConnexion,
        Some(c) if c.accorde => CanalRadio::Icy,
        Some(c) if c.voie != VOIE_FLUX => CanalRadio::VoieSansIcy,
        Some(c) if c.demande => CanalRadio::IcyRefuse,
        Some(_) => CanalRadio::IcyNonDemande,
    }
}

pub struct AudioStreamer {
    sessions: Arc<Mutex<HashMap<String, Arc<StreamSession>>>>,
    port: u16,
}

impl AudioStreamer {
    pub fn new(port: u16) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            port,
        }
    }

    pub async fn create_session(
        &self,
        info: StreamInfo,
        bit_perfect: bool,
        buffer_size: usize,
    ) -> (
        String,
        mpsc::Sender<Vec<u8>>,
        std::sync::Arc<tokio::sync::Notify>,
    ) {
        let id = uuid::Uuid::new_v4().to_string();
        let session = StreamSession::new(id.clone(), info, bit_perfect, buffer_size);
        let tx = session
            .tx
            .lock()
            .await
            .take()
            .expect("freshly created session has tx");
        let data_ready = session.data_ready.clone();
        self.sessions
            .lock()
            .await
            .insert(id.clone(), Arc::new(session));
        info!(stream_id = %id, "stream_session_created");
        (id, tx, data_ready)
    }

    /// End the session's INPUT: drop the keep-alive sender so readers drain
    /// the buffered chunks and then see a real EOF. The keep-alive exists so
    /// DLNA reconnects survive mid-track, but for FINITE programme content the
    /// producer must call this when it is done writing — otherwise the HTTP
    /// body never ends and a pull output that relies on EOF for its internal
    /// gapless (OAAT) hangs at end of track: watchdog "stall", supervisor
    /// restarts the SAME track (silence puis « le dernier repart », Bertrand,
    /// .18, 28/07). Radio sessions are infinite by design and never call this.
    pub async fn end_session_input(&self, stream_id: &str) {
        let session = { self.sessions.lock().await.get(stream_id).cloned() };
        if let Some(s) = session {
            s.close_sender().await;
            info!(stream_id, "stream_session_input_ended");
        }
    }

    /// True when the producer task that feeds `stream_id` has exited (see
    /// `StreamSession::producer_done`), or when the session no longer exists
    /// at all — both mean nothing will ever produce another PCM chunk, so a
    /// paused webradio zone resuming on this session would render silence.
    /// Only meaningful for radio decode sessions; a proxy/file session never
    /// sets the flag and reports `false` while it exists.
    pub async fn radio_producer_done(&self, stream_id: &str) -> bool {
        let sessions = self.sessions.lock().await;
        match sessions.get(stream_id) {
            Some(s) => s.producer_done.load(std::sync::atomic::Ordering::Relaxed),
            None => true,
        }
    }

    pub async fn wait_data_ready(&self, stream_id: &str, timeout_ms: u64) -> bool {
        let session = {
            let sessions = self.sessions.lock().await;
            sessions.get(stream_id).cloned()
        };
        let Some(session) = session else {
            return false;
        };
        // A proxy session (Qobuz/Tidal direct pass-through) serves on demand: it
        // holds no buffered data until the renderer pulls the URL, so `data_ready`
        // is never notified. Waiting the full `timeout_ms` therefore always times
        // out and delays the gapless SetNext by that budget (5s). On short tracks
        // — opera recitatives on an OpenHome renderer (Luxman NT-07) — the next
        // track is armed too late and the renderer loops the current one. A proxy
        // is "ready" the moment it exists, so don't wait.
        if session.proxy_url.lock().await.is_some() {
            return true;
        }
        tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            session.data_ready.notified(),
        )
        .await
        .is_ok()
    }

    /// Initial-play prebuffer barrier for DLNA (#1259).
    ///
    /// Block until roughly `target_bytes` of audio has accumulated in the
    /// session's mpsc channel, so a DLNA renderer's clock does not start against
    /// a still-cold decode pipeline (~5s of micro-dropouts at track start —
    /// biblio/Qobuz/radio, macOS). Local/USB output already prefills its ring
    /// buffer before starting the DAC (`outputs/local.rs`); this reproduces that
    /// for the network path, which had NO server-side prebuffer.
    ///
    /// Returns `true` as soon as the target is reached, and `true` immediately
    /// for sessions that must NOT be waited on:
    ///   - proxy sessions (direct CDN passthrough, served on demand — never fill
    ///     the mpsc channel, so waiting would hang),
    ///   - file sessions (`serve_file`, already on disk, Range-seekable),
    ///   - unknown or already-closed sessions.
    /// Only genuine transcode/radio channel sessions are actually awaited.
    ///
    /// Returns `false` when `timeout` elapses first — the hard cap so a slow or
    /// very short source never freezes the start of playback. The caller sends
    /// `Play` regardless; a `false` just means "started with less than the full
    /// target buffered".
    pub async fn wait_prefill_ready(
        &self,
        stream_id: &str,
        target_bytes: u64,
        timeout: std::time::Duration,
    ) -> bool {
        let session = { self.sessions.lock().await.get(stream_id).cloned() };
        let Some(session) = session else {
            return true;
        };
        // serve_file / proxy passthrough: no mpsc channel to prefill. Blocking
        // here would wait the full timeout for data that never flows through the
        // channel — so treat them as ready immediately (this is the channel-only
        // gate that excludes the serve_file passthrough).
        if session.proxy_url.lock().await.is_some() || session.file_path.lock().await.is_some() {
            return true;
        }
        if target_bytes == 0 {
            return true;
        }
        // Chunk size used by the transcode / prefetch producers (exact for the
        // dominant WAV transcode path). Radio chunks may be smaller, so the byte
        // target is reached with fewer messages → a slightly shorter, still-safe
        // prebuffer. Only used to translate the byte target into channel
        // messages, the unit tokio's mpsc exposes.
        const ASSUMED_CHUNK_BYTES: u64 = 32768;
        let mut target_chunks = (target_bytes / ASSUMED_CHUNK_BYTES).max(1);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            match session.channel_fill().await {
                // Channel closed (producer done / gone): nothing more will be
                // buffered, so stop waiting rather than burn the whole timeout.
                None => return true,
                Some((buffered, max)) => {
                    // Clamp so a full channel always satisfies the target: a
                    // source with less than `target_bytes` total simply fills
                    // what it can; the timeout then caps the wait.
                    if max > 0 {
                        target_chunks = target_chunks.min(max as u64);
                    }
                    if buffered as u64 >= target_chunks {
                        return true;
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    pub async fn create_file_session(
        &self,
        info: StreamInfo,
        file_path: String,
        bit_perfect: bool,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let session = StreamSession::new(id.clone(), info, bit_perfect, 64);
        *session.file_path.lock().await = Some(file_path);
        // File is already written to disk — signal data_ready immediately so
        // gapless pre-buffer logic (poller::prepare_gapless → wait_stream_data_ready)
        // does not block for its full 5-second timeout waiting for data that will
        // never arrive via the mpsc channel.
        session.data_ready.notify_one();
        self.sessions
            .lock()
            .await
            .insert(id.clone(), Arc::new(session));
        info!(stream_id = %id, "file_session_created");
        id
    }

    /// Attach an on-the-fly M4A faststart map to a file session so it is served
    /// as `ftyp + patched-moov + mdat` (moov relocated to the front). Must be
    /// set before the renderer requests the stream.
    pub async fn set_faststart(&self, id: &str, map: crate::audio::faststart::FaststartMap) {
        if let Some(s) = self.sessions.lock().await.get(id) {
            *s.faststart.lock().unwrap_or_else(|e| e.into_inner()) = Some(map);
        }
    }

    /// Create a streaming session for a decoded radio stream (infinite,
    /// exempt from GC).  Same as `create_session` but sets `is_radio = true`
    /// so the stream handler skips coalescing and the GC retains the session.
    pub async fn create_radio_session(
        &self,
        info: StreamInfo,
        buffer_size: usize,
    ) -> (
        String,
        mpsc::Sender<Vec<u8>>,
        std::sync::Arc<tokio::sync::Notify>,
        Arc<StreamSession>,
    ) {
        let id = uuid::Uuid::new_v4().to_string();
        let mut session = StreamSession::new(id.clone(), info, false, buffer_size);
        session.is_radio = true;
        let tx = session
            .tx
            .lock()
            .await
            .take()
            .expect("freshly created session has tx");
        let data_ready = session.data_ready.clone();
        let session = Arc::new(session);
        self.sessions
            .lock()
            .await
            .insert(id.clone(), session.clone());
        info!(stream_id = %id, "radio_stream_session_created");
        (id, tx, data_ready, session)
    }

    pub async fn create_proxy_session(
        &self,
        info: StreamInfo,
        upstream_url: String,
        is_radio: bool,
    ) -> String {
        self.create_proxy_session_with_reresolve(info, upstream_url, is_radio, None)
            .await
    }

    /// Like `create_proxy_session` but also stores a re-resolver so the proxy
    /// layer can obtain a fresh signed CDN URL when the stored one expires
    /// mid-track (Qobuz/Tidal short-TTL signatures — #1136).
    pub async fn create_proxy_session_with_reresolve(
        &self,
        info: StreamInfo,
        upstream_url: String,
        is_radio: bool,
        reresolve: Option<ReresolveFn>,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let mut session = StreamSession::new(id.clone(), info, false, 128);
        session.is_radio = is_radio;
        *session.proxy_url.lock().await = Some(upstream_url);
        *session.reresolve.lock().await = reresolve;
        self.sessions
            .lock()
            .await
            .insert(id.clone(), Arc::new(session));
        info!(stream_id = %id, is_radio, "proxy_session_created");
        id
    }

    /// Check if a session is a proxy session (direct CDN URL forwarding)
    /// or a file session — both support HTTP Range-based seeking.
    /// Decoded/transcoded WAV sessions (mpsc channel) do NOT support Range seeking.
    pub async fn is_seekable_session(&self, stream_id: &str) -> bool {
        let sessions = self.sessions.lock().await;
        if let Some(session) = sessions.get(stream_id) {
            let has_proxy = session.proxy_url.lock().await.is_some();
            let has_file = session.file_path.lock().await.is_some();
            has_proxy || has_file
        } else {
            false
        }
    }

    pub async fn remove_session(&self, stream_id: &str) {
        // Close the channel senders BEFORE removing — this ensures the
        // radio decode thread's tx.send() fails promptly, stopping
        // the icecast download. Without this, radio streams continue
        // playing as "ghosts" after stop.
        {
            let sessions = self.sessions.lock().await;
            if let Some(session) = sessions.get(stream_id) {
                session.close_sender().await;
            }
        }
        let removed = self.sessions.lock().await.remove(stream_id);
        // Le registre des titres radio suit la vie de la session : sans ça il
        // grossirait d'une entree par flux ecoute, et pour toujours.
        forget_radio_now(stream_id);
        // Même vie pour le canal négocié (#2991) : une entrée par flux écouté.
        forget_icy_channel(stream_id);
        // Clean up temp transcode files created by the pre-transcode pipeline.
        // Only delete files under the system temp dir with the tune-transcode prefix
        // to avoid accidentally removing actual music files.
        if let Some(session) = removed {
            let fp = session.file_path.lock().await;
            if let Some(ref path) = *fp {
                if is_temp_transcode_file(path) {
                    if let Err(e) = std::fs::remove_file(path) {
                        info!(stream_id, path, error = %e, "temp_transcode_file_cleanup_failed");
                    } else {
                        info!(stream_id, path, "temp_transcode_file_cleaned_up");
                    }
                }
            }
        }
        info!(stream_id, "stream_session_removed");
    }

    pub fn get_stream_url(&self, stream_id: &str, server_ip: &str, ext: &str) -> String {
        format!("http://{server_ip}:{}/stream/{stream_id}.{ext}", self.port)
    }

    /// Real output container actually served for a live session
    /// (`StreamInfo.format`, e.g. "wav" / "flac"). The signal-path display uses
    /// it to show the TRANSCODED wire container instead of a statically-guessed
    /// transcode target: a DLNA renderer that does not advertise `audio/flac`
    /// is served WAV/LPCM even for a FLAC/ALAC source (negotiated async via
    /// `dlna_needs_wav`), which the synchronous `build_signal_path` cannot
    /// replay (Sevy, forum affichage-chemin-du-signal — showed "ALAC → FLAC"
    /// while the wire was WAV). Returns `None` for an unknown session.
    /// Format RÉELLEMENT servi sur le fil pour cette session — conteneur,
    /// fréquence, profondeur, canaux.
    ///
    /// C'est la seule source de vérité sur ce que le renderer reçoit : le
    /// chemin du signal déduisait auparavant ces valeurs en rejouant les règles
    /// de l'orchestrateur, si bien qu'il pouvait annoncer autre chose que ce
    /// qui partait vraiment (Yves : darTZeel LHC-208 et Eversolo DMP-A10 en
    /// passthrough natif affichent la bonne résolution, Tune non).
    pub async fn stream_output_wire(&self, stream_id: &str) -> Option<StreamInfo> {
        self.sessions
            .lock()
            .await
            .get(stream_id)
            .map(|s| s.effective_output_info())
    }

    pub fn sessions_state(&self) -> Arc<Mutex<HashMap<String, Arc<StreamSession>>>> {
        self.sessions.clone()
    }

    /// Taille totale du flux d'une session **fichier**, en octets.
    ///
    /// `info.file_size` s'il est renseigné, sinon la taille du fichier sur
    /// disque — la même source que l'en-tête `Content-Length` servi au renderer.
    /// Renvoie `None` pour une session sans fichier (radio, flux décodé à la
    /// volée) : on ne peut alors rien conclure sur ce qui a été consommé.
    ///
    /// Sert à distinguer « le renderer a fini le morceau » de « le renderer a
    /// calé » : on ne peut pas finir de jouer un fichier qu'on n'a pas reçu.
    pub async fn stream_total_bytes(&self, stream_id: &str) -> Option<u64> {
        let session = { self.sessions.lock().await.get(stream_id).cloned() }?;
        if session.is_radio {
            return None;
        }
        if let Some(sz) = session.info.file_size {
            return Some(sz);
        }
        let path = session.file_path.lock().await.clone()?;
        tokio::fs::metadata(path).await.ok().map(|m| m.len())
    }

    pub async fn stream_bytes_sent(&self, stream_id: &str) -> Option<u64> {
        let sessions = self.sessions.lock().await;
        sessions
            .get(stream_id)
            .map(|s| s.bytes_sent.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Reprendre les sessions mortes — sur leur SILENCE, pas sur leur âge.
    ///
    /// Une session qui débite des octets est vivante quel que soit son âge.
    /// Le critère précédent était l'âge absolu depuis `created_at`, jamais
    /// rafraîchi : une piste de plus de trente minutes — une image de CD d'un
    /// seul tenant, un acte d'opéra, un set — voyait sa session retirée de la
    /// table et son fichier de pré-transcodage effacé du disque en pleine
    /// lecture (#2536). Deux bornes désormais : [`SESSION_IDLE_TIMEOUT`] sur
    /// l'inactivité pour libérer, [`SESSION_ABSOLUTE_CAP`] en filet pour
    /// qu'aucune session ne devienne éternelle.
    ///
    /// La radio reste exemptée des deux, comme avant : elle est infinie par
    /// nature et son flux ne se rejoue pas.
    pub async fn cleanup_stale_sessions(&self) -> usize {
        self.cleanup_stale_sessions_with(SESSION_IDLE_TIMEOUT, SESSION_ABSOLUTE_CAP)
            .await
    }

    /// Same sweep with injectable bounds, so the two clocks can be exercised
    /// in milliseconds instead of half an hour.
    pub async fn cleanup_stale_sessions_with(
        &self,
        idle_timeout: std::time::Duration,
        absolute_cap: std::time::Duration,
    ) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        let mut sessions = self.sessions.lock().await;
        let before = sessions.len();
        // Collect temp files to clean up from stale sessions
        let mut temp_files_to_remove: Vec<String> = Vec::new();
        sessions.retain(|id, s| {
            if s.is_radio {
                return true;
            }
            let age = s.created_at.elapsed();
            // Relire l'activité : `bytes_sent` est monotone, donc « la valeur
            // a bougé depuis le balayage précédent » suffit à dater la
            // dernière fois que des octets sont partis.
            let sent = s.bytes_sent.load(Relaxed);
            if sent != s.gc_seen_bytes.swap(sent, Relaxed) {
                s.gc_active_at_ms.store(age.as_millis() as u64, Relaxed);
            }
            let idle = age.saturating_sub(std::time::Duration::from_millis(
                s.gc_active_at_ms.load(Relaxed),
            ));
            let reason = if idle > idle_timeout {
                "idle"
            } else if age > absolute_cap {
                "absolute_cap"
            } else {
                return true;
            };
            // Check for temp transcode file to clean up.
            // We can't .await inside retain, so use try_lock.
            if let Ok(fp) = s.file_path.try_lock() {
                if let Some(ref path) = *fp {
                    if is_temp_transcode_file(path) {
                        temp_files_to_remove.push(path.clone());
                    }
                }
            }
            info!(
                stream_id = %id,
                age_secs = age.as_secs(),
                idle_secs = idle.as_secs(),
                reason,
                "stale_session_removed"
            );
            false
        });
        let after = sessions.len();
        drop(sessions);
        // Clean up temp files outside the sessions lock
        for path in &temp_files_to_remove {
            if let Err(e) = std::fs::remove_file(path) {
                info!(path, error = %e, "stale_temp_transcode_file_cleanup_failed");
            } else {
                info!(path, "stale_temp_transcode_file_cleaned_up");
            }
        }
        before - after
    }
}

/// Remove leftover temp transcode files from /tmp on startup.
/// Called once when the server starts to clean up files from a previous
/// crash or unclean shutdown.
pub fn cleanup_leftover_transcode_files() {
    let tmp_dir = std::env::temp_dir();
    let entries = match std::fs::read_dir(&tmp_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut count = 0;
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            if name.starts_with("tune-transcode-")
                || name.starts_with("tune-aac-transcode-")
                || name.starts_with("tune-dash-transcode-")
                || name.starts_with("tune-faststart-")
            {
                if std::fs::remove_file(entry.path()).is_ok() {
                    count += 1;
                }
            }
        }
    }
    if count > 0 {
        info!(count, "leftover_transcode_files_cleaned_up");
    }
}

// ─── Helpers (framework-agnostic) ───────────────────────────────

pub fn extract_stream_id(raw: &str) -> &str {
    raw.split('.').next().unwrap_or(raw)
}

pub fn build_wav_header(
    channels: u16,
    sample_rate: u32,
    bit_depth: u16,
    duration_ms: Option<u64>,
) -> [u8; 44] {
    build_wav_header_with_duration(channels, sample_rate, bit_depth, duration_ms)
}

/// Build a WAV header for an infinite live radio stream, using the streaming
/// (`0xFFFF_FFFF`) indeterminate-length convention so a Lavf DLNA renderer
/// keeps reading until the connection closes instead of stopping after it has
/// buffered the finite `data` size.
pub fn build_wav_header_streaming(channels: u16, sample_rate: u32, bit_depth: u16) -> [u8; 44] {
    wav_header_streaming(channels, sample_rate, bit_depth)
}

/// Build a WAV header for a live radio stream served with the *file* contract
/// (`Content-Length` + `Range`) that chunked-hostile renderers require — the
/// darTZeel LHC-208 will not start without it (#1689).
pub fn build_wav_header_bounded_live(channels: u16, sample_rate: u32, bit_depth: u16) -> [u8; 44] {
    wav_header_bounded_live(channels, sample_rate, bit_depth)
}

/// Check if a file path is a temporary transcode file created by the
/// pre-transcode pipeline.  Only these files should be auto-deleted
/// when a session is removed — never actual music files.
///
/// Patterns:
/// - `tune-transcode-{uuid}.{ext}` — local file pre-transcode (FLAC/WAV target)
/// - `tune-aac-transcode-{uuid}.flac` — Tidal AAC→FLAC pre-transcode
/// - `tune-dash-transcode-{uuid}.flac` — Tidal DASH fMP4→FLAC pre-transcode
fn is_temp_transcode_file(path: &str) -> bool {
    let file_name = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    file_name.starts_with("tune-transcode-")
        || file_name.starts_with("tune-aac-transcode-")
        || file_name.starts_with("tune-dash-transcode-")
        || file_name.starts_with("tune-faststart-")
}

pub fn build_icy_metadata(
    artist: Option<&str>,
    title: Option<&str>,
    cover_url: Option<&str>,
) -> Vec<u8> {
    let mut parts = Vec::new();
    let stream_title = match (artist, title) {
        (Some(a), Some(t)) => Some(format!("{a} - {t}")),
        (Some(a), None) => Some(a.to_string()),
        (None, Some(t)) => Some(t.to_string()),
        (None, None) => None,
    };
    if let Some(st) = stream_title {
        parts.push(format!("StreamTitle='{st}';"));
    }
    if let Some(url) = cover_url {
        parts.push(format!("StreamUrl='{url}';"));
    }
    if parts.is_empty() {
        return vec![0u8];
    }
    let mut payload = parts.join("").into_bytes();
    let pad = (16 - payload.len() % 16) % 16;
    payload.resize(payload.len() + pad, 0);
    let len_byte = (payload.len() / 16).min(255) as u8;
    let mut block = vec![len_byte];
    block.extend_from_slice(&payload[..len_byte as usize * 16]);
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un canal VIDE ne prouve rien tant que le producteur n'a jamais été en
    /// avance : au démarrage il l'est toujours, le temps que le décodeur
    /// prenne son élan. `note_channel_fill` doit rester muet.
    #[test]
    fn un_canal_vide_qui_n_a_jamais_ete_plein_n_alerte_pas() {
        let session = StreamSession::new("s".into(), StreamInfo::default(), false, 4);
        assert!(!session.note_channel_fill(0, 4), "vide au démarrage");
        assert!(!session.note_channel_fill(1, 4), "un morceau en attente");
        assert!(
            !session.note_channel_fill(0, 4),
            "de nouveau vide, jamais plein"
        );
        assert!(!session.note_channel_fill(3, 4), "presque plein, pas plein");
        assert!(!session.note_channel_fill(0, 4), "toujours jamais plein");
    }

    /// Le seul instant qui se lit sans ambiguïté : le canal a été PLEIN, il
    /// est maintenant VIDE. Le producteur avait de l'avance, il ne l'a plus.
    /// Une seule ligne par session — le fait est ponctuel.
    #[test]
    fn un_canal_plein_puis_vide_alerte_une_seule_fois() {
        let session = StreamSession::new("s".into(), StreamInfo::default(), false, 4);
        assert!(!session.note_channel_fill(0, 4), "démarrage");
        assert!(!session.note_channel_fill(4, 4), "plein : rien à dire");
        assert!(
            session.note_channel_fill(0, 4),
            "plein puis vide : c'est LÀ que le producteur a lâché"
        );
        assert!(!session.note_channel_fill(0, 4), "une seule fois");
        assert!(!session.note_channel_fill(4, 4), "de nouveau plein");
        assert!(
            !session.note_channel_fill(0, 4),
            "et toujours une seule fois"
        );
    }

    /// Un canal de capacité nulle n'existe pas dans le dépôt, mais le verdict
    /// ne doit pas s'inventer un « plein » à partir d'un maximum de 0.
    #[test]
    fn une_capacite_nulle_ne_vaut_pas_plein() {
        let session = StreamSession::new("s".into(), StreamInfo::default(), false, 4);
        assert!(!session.note_channel_fill(0, 0));
        assert!(!session.note_channel_fill(0, 4));
    }

    #[tokio::test]
    async fn create_and_remove_session() {
        let streamer = AudioStreamer::new(8080);
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            file_size: None,
            duration_ms: None,
            ..Default::default()
        };
        let (id, _tx, _data_ready) = streamer.create_session(info, false, 128).await;
        assert!(!id.is_empty());
        streamer.remove_session(&id).await;
    }

    #[tokio::test]
    async fn radio_wire_uses_the_detected_output_format() {
        let streamer = AudioStreamer::new(8080);
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 44_100,
            bit_depth: 16,
            channels: 2,
            ..Default::default()
        };
        let (id, _tx, _data_ready, session) = streamer.create_radio_session(info, 256).await;

        assert_eq!(
            streamer.stream_output_wire(&id).await.unwrap().sample_rate,
            44_100
        );

        session.publish_detected_output_format(48_000, 1);
        let wire = streamer.stream_output_wire(&id).await.unwrap();
        assert_eq!(wire.sample_rate, 48_000);
        assert_eq!(wire.channels, 1);
    }

    #[tokio::test]
    async fn file_session() {
        let streamer = AudioStreamer::new(8080);
        let info = StreamInfo {
            format: "flac".into(),
            mime_type: "audio/flac".into(),
            sample_rate: 96000,
            bit_depth: 24,
            channels: 2,
            file_size: Some(50_000_000),
            duration_ms: None,
            ..Default::default()
        };
        let id = streamer
            .create_file_session(info, "/music/test.flac".into(), true)
            .await;
        let url = streamer.get_stream_url(&id, "192.168.1.18", "flac");
        assert!(url.contains(".flac"));
        streamer.remove_session(&id).await;
    }

    /// Le registre des titres radio rend le bloc ICY VIVANT.
    ///
    /// C'est tout l'objet du correctif : avant, le bloc etait construit une
    /// fois a la connexion et reemis a l'identique toutes les 16 Ko — un
    /// Marantz branche sur Radio Paradise affichait eternellement le morceau
    /// du moment ou il s'etait branche.
    #[test]
    fn radio_now_changes_the_icy_block_between_two_emissions() {
        let sid = "test-flux-radio-1";
        forget_radio_now(sid);

        // Rien de publie : le flux se rabat sur le bloc de la connexion.
        assert!(radio_now(sid).is_none());

        publish_radio_now(sid, Some("Pink Floyd".into()), "Time".into(), None);
        let n1 = radio_now(sid).expect("titre publie");
        let bloc1 = build_icy_metadata(n1.artist.as_deref(), Some(&n1.title), n1.cover.as_deref());

        // Le morceau suivant passe a l'antenne.
        publish_radio_now(sid, Some("Miles Davis".into()), "So What".into(), None);
        let n2 = radio_now(sid).expect("titre publie");
        let bloc2 = build_icy_metadata(n2.artist.as_deref(), Some(&n2.title), n2.cover.as_deref());

        assert_ne!(bloc1, bloc2, "le bloc ICY doit suivre le morceau courant");
        assert!(String::from_utf8_lossy(&bloc2).contains("Miles Davis - So What"));

        forget_radio_now(sid);
    }

    /// La POCHETTE doit suivre le morceau, pas rester sur la premiere.
    ///
    /// Elle etait lue sur `StreamSession::cover_url`, capture une fois a la
    /// connexion — un champ qui vaut TOUJOURS `None` : `StreamUrl='…'` ne
    /// partait donc jamais, et l'ecran du renderer gardait la premiere image
    /// (Serge Asselin, Hifi Rose RS250A, fil 1529 — #2161).
    #[test]
    fn radio_now_carries_the_cover_and_it_changes_with_the_track() {
        let sid = "test-flux-radio-pochette";
        forget_radio_now(sid);

        publish_radio_now(
            sid,
            Some("Pink Floyd".into()),
            "Time".into(),
            Some("https://img.radioparadise.com/covers/l/time.jpg".into()),
        );
        let n1 = radio_now(sid).expect("titre publie");
        let bloc1 = build_icy_metadata(n1.artist.as_deref(), Some(&n1.title), n1.cover.as_deref());
        let txt1 = String::from_utf8_lossy(&bloc1).to_string();
        assert!(
            txt1.contains("StreamUrl='https://img.radioparadise.com/covers/l/time.jpg'"),
            "la pochette publiee doit voyager dans le bloc ICY, or : {txt1}"
        );

        publish_radio_now(
            sid,
            Some("Miles Davis".into()),
            "So What".into(),
            Some("https://img.radioparadise.com/covers/l/sowhat.jpg".into()),
        );
        let n2 = radio_now(sid).expect("titre publie");
        let bloc2 = build_icy_metadata(n2.artist.as_deref(), Some(&n2.title), n2.cover.as_deref());
        let txt2 = String::from_utf8_lossy(&bloc2).to_string();
        assert!(
            txt2.contains("StreamUrl='https://img.radioparadise.com/covers/l/sowhat.jpg'"),
            "la pochette doit suivre le morceau suivant, or : {txt2}"
        );
        assert!(
            !txt2.contains("time.jpg"),
            "la pochette du morceau precedent ne doit plus figurer, or : {txt2}"
        );

        forget_radio_now(sid);
    }

    /// Un flux termine ne doit rien laisser derriere lui : sans ca le registre
    /// grossirait d'une entree par radio ecoutee, indefiniment.
    #[test]
    fn forgetting_a_stream_clears_its_entry() {
        let sid = "test-flux-radio-2";
        publish_radio_now(sid, None, "Un titre".into(), None);
        assert!(radio_now(sid).is_some());
        forget_radio_now(sid);
        assert!(radio_now(sid).is_none());
    }

    /// L'attribut `#[test]` manquait : la fonction compilait, ne s'exécutait
    /// jamais, et couvrait pourtant le seul bloc ICY qui parte vers l'écran
    /// d'un lecteur réseau (#3018). Ses deux voisines l'avaient.
    #[test]
    fn icy_metadata_block() {
        let block = build_icy_metadata(Some("Artist"), Some("Title"), None);
        assert!(block.len() > 1);
        let len_byte = block[0] as usize;
        assert_eq!(block.len(), 1 + len_byte * 16);
        let payload = std::str::from_utf8(&block[1..]).unwrap();
        assert!(payload.contains("StreamTitle='Artist - Title'"));
    }

    #[test]
    fn icy_metadata_empty() {
        let block = build_icy_metadata(None, None, None);
        assert_eq!(block, vec![0u8]);
    }

    #[test]
    fn icy_metadata_with_cover() {
        let block = build_icy_metadata(Some("A"), Some("T"), Some("http://example.com/cover.jpg"));
        let payload = String::from_utf8_lossy(&block[1..]);
        assert!(payload.contains("StreamUrl='http://example.com/cover.jpg'"));
    }

    #[tokio::test]
    async fn proxy_session() {
        let streamer = AudioStreamer::new(8080);
        let info = StreamInfo {
            format: "flac".into(),
            mime_type: "audio/flac".into(),
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            file_size: None,
            duration_ms: None,
            ..Default::default()
        };
        let id = streamer
            .create_proxy_session(info, "https://cdn.tidal.com/track.flac".into(), false)
            .await;
        assert!(!id.is_empty());
        streamer.remove_session(&id).await;
    }

    // A proxy session serves on demand and never notifies `data_ready`. The
    // gapless poller must NOT block on it (a 5s wait per transition armed the
    // next track too late → OpenHome/Luxman looped short opera tracks). It must
    // report ready immediately.
    #[tokio::test]
    async fn wait_data_ready_returns_immediately_for_proxy() {
        let streamer = AudioStreamer::new(8080);
        let info = StreamInfo {
            format: "flac".into(),
            mime_type: "audio/flac".into(),
            ..Default::default()
        };
        let id = streamer
            .create_proxy_session(info, "https://cdn.qobuz.com/track.flac".into(), false)
            .await;
        let t0 = std::time::Instant::now();
        let ready = streamer.wait_data_ready(&id, 5000).await;
        assert!(ready, "a proxy session must report data ready");
        assert!(
            t0.elapsed().as_millis() < 500,
            "must not wait the full timeout for a proxy"
        );
        streamer.remove_session(&id).await;
    }

    // A non-proxy session that never produces data still times out (unchanged).
    #[tokio::test]
    async fn wait_data_ready_times_out_for_non_proxy_without_data() {
        let streamer = AudioStreamer::new(8080);
        let info = StreamInfo {
            format: "flac".into(),
            mime_type: "audio/flac".into(),
            ..Default::default()
        };
        let (id, _tx, _dr) = streamer.create_session(info, false, 128).await;
        assert!(
            !streamer.wait_data_ready(&id, 100).await,
            "no data → times out"
        );
        streamer.remove_session(&id).await;
    }

    // Claiming a new radio consumer supersedes the previous one: only the
    // latest epoch is "current". This is the primitive that lets a DLNA
    // renderer's reconnect take over the single-consumer PCM channel instead of
    // racing the first connection for each chunk.
    #[tokio::test]
    async fn radio_consumer_claim_supersedes_previous() {
        let streamer = AudioStreamer::new(8080);
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            ..Default::default()
        };
        let (_id, _tx, _dr, session) = streamer.create_radio_session(info, 256).await;

        let e1 = session.claim_channel_consumer();
        assert!(session.is_current_channel_consumer(e1));
        let e2 = session.claim_channel_consumer();
        assert_ne!(e1, e2);
        assert!(
            !session.is_current_channel_consumer(e1),
            "the first consumer is superseded once a second claims the channel"
        );
        assert!(session.is_current_channel_consumer(e2));
    }

    // End-to-end hand-off: when a 2nd connection claims the channel, the 1st
    // must stop WITHOUT stealing any further chunk, and every chunk sent after
    // the hand-off must reach the 2nd connection only — no split, no loss, no
    // duplication. This mirrors the loop in `handle_stream`'s radio branch
    // (subscribe-then-check epoch + biased select on supersede).
    #[tokio::test]
    async fn radio_consumer_handoff_does_not_split_stream() {
        use std::sync::Arc;

        let streamer = AudioStreamer::new(8080);
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            ..Default::default()
        };
        let (_id, tx, _dr, session) = streamer.create_radio_session(info, 256).await;

        // Faithful copy of the handler's radio consumer loop. Returns every
        // chunk this connection actually pulled from the channel.
        async fn consume(
            session: Arc<StreamSession>,
            claimed: tokio::sync::oneshot::Sender<()>,
            out: mpsc::UnboundedSender<Vec<u8>>,
        ) -> Vec<Vec<u8>> {
            let my_epoch = session.claim_channel_consumer();
            let _ = claimed.send(()); // establish happens-before for the test
            let mut got = Vec::new();
            loop {
                let superseded = session.consumer_supersede.notified();
                tokio::pin!(superseded);
                superseded.as_mut().enable();
                if !session.is_current_channel_consumer(my_epoch) {
                    break;
                }
                tokio::select! {
                    biased;
                    _ = &mut superseded => continue,
                    maybe = session.recv_chunk() => match maybe {
                        Some(c) => {
                            let _ = out.send(c.clone());
                            got.push(c);
                        }
                        None => break,
                    }
                }
            }
            got
        }

        // Consumer 1 claims first.
        let (c1_ready_tx, c1_ready_rx) = tokio::sync::oneshot::channel();
        let (c1_out_tx, mut c1_out_rx) = mpsc::unbounded_channel();
        let h1 = tokio::spawn(consume(session.clone(), c1_ready_tx, c1_out_tx));
        c1_ready_rx.await.unwrap();

        // A chunk sent now must go to consumer 1.
        tx.send(vec![1u8]).await.unwrap();
        assert_eq!(c1_out_rx.recv().await.unwrap(), vec![1u8]);

        // Consumer 2 claims — supersedes consumer 1 (bump + notify happen-before
        // the send below via the oneshot await).
        let (c2_ready_tx, c2_ready_rx) = tokio::sync::oneshot::channel();
        let (c2_out_tx, mut c2_out_rx) = mpsc::unbounded_channel();
        let h2 = tokio::spawn(consume(session.clone(), c2_ready_tx, c2_out_tx));
        c2_ready_rx.await.unwrap();

        // Every chunk from here on must reach consumer 2, never consumer 1.
        tx.send(vec![2u8]).await.unwrap();
        assert_eq!(c2_out_rx.recv().await.unwrap(), vec![2u8]);
        tx.send(vec![3u8]).await.unwrap();
        assert_eq!(c2_out_rx.recv().await.unwrap(), vec![3u8]);

        // Close the channel so both loops end.
        drop(tx);
        session.close_sender().await;

        let c1_got = h1.await.unwrap();
        let c2_got = h2.await.unwrap();

        assert_eq!(
            c1_got,
            vec![vec![1u8]],
            "the superseded consumer must have pulled ONLY its pre-handoff chunk"
        );
        assert_eq!(
            c2_got,
            vec![vec![2u8], vec![3u8]],
            "the new consumer must receive every post-handoff chunk, in order"
        );
    }

    // #1259 prebuffer barrier: a proxy session has no mpsc channel to fill, so
    // waiting must return ready IMMEDIATELY (blocking would hang for the whole
    // timeout — and on a live proxy, forever).
    #[tokio::test]
    async fn wait_prefill_ready_returns_immediately_for_proxy() {
        let streamer = AudioStreamer::new(8080);
        let info = StreamInfo {
            format: "flac".into(),
            mime_type: "audio/flac".into(),
            ..Default::default()
        };
        let id = streamer
            .create_proxy_session(info, "https://cdn.qobuz.com/track.flac".into(), false)
            .await;
        let t0 = std::time::Instant::now();
        assert!(
            streamer
                .wait_prefill_ready(&id, 1_000_000, std::time::Duration::from_secs(5))
                .await
        );
        assert!(t0.elapsed().as_millis() < 500, "must not wait for a proxy");
        streamer.remove_session(&id).await;
    }

    // A file (serve_file) session is already on disk — excluded from prebuffer,
    // returns ready immediately.
    #[tokio::test]
    async fn wait_prefill_ready_returns_immediately_for_file() {
        let streamer = AudioStreamer::new(8080);
        let info = StreamInfo {
            format: "flac".into(),
            mime_type: "audio/flac".into(),
            ..Default::default()
        };
        let id = streamer
            .create_file_session(info, "/music/test.flac".into(), false)
            .await;
        let t0 = std::time::Instant::now();
        assert!(
            streamer
                .wait_prefill_ready(&id, 1_000_000, std::time::Duration::from_secs(5))
                .await
        );
        assert!(t0.elapsed().as_millis() < 500, "must not wait for a file");
        streamer.remove_session(&id).await;
    }

    // A channel session returns ready as soon as enough chunks are buffered.
    #[tokio::test]
    async fn wait_prefill_ready_reached_once_buffered() {
        let streamer = AudioStreamer::new(8080);
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            ..Default::default()
        };
        let (id, tx, _dr) = streamer.create_session(info, false, 256).await;
        // Target = 1 chunk (32768 bytes). Push two 32KB chunks; nobody consumes.
        tx.send(vec![0u8; 32768]).await.unwrap();
        tx.send(vec![0u8; 32768]).await.unwrap();
        assert!(
            streamer
                .wait_prefill_ready(&id, 32768, std::time::Duration::from_secs(2))
                .await,
            "target reached once >= 1 chunk is buffered"
        );
        drop(tx);
        streamer.remove_session(&id).await;
    }

    // A channel session that never buffers enough must time out (return false)
    // rather than block forever — the cap that keeps a slow/short source from
    // freezing the start of playback. The keep-alive sender holds the channel
    // open so channel_fill keeps reporting (never None) for the whole wait.
    #[tokio::test]
    async fn wait_prefill_ready_times_out_when_underfilled() {
        let streamer = AudioStreamer::new(8080);
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            ..Default::default()
        };
        let (id, _tx, _dr) = streamer.create_session(info, false, 256).await;
        // Never send anything; target needs many chunks → must time out.
        let t0 = std::time::Instant::now();
        assert!(
            !streamer
                .wait_prefill_ready(&id, 10_000_000, std::time::Duration::from_millis(150))
                .await,
            "underfilled channel must time out, not hang"
        );
        assert!(t0.elapsed().as_millis() >= 150);
        streamer.remove_session(&id).await;
    }

    #[test]
    fn stream_id_extraction() {
        assert_eq!(extract_stream_id("abc123.flac"), "abc123");
        assert_eq!(extract_stream_id("abc123"), "abc123");
    }

    #[test]
    fn wav_content_length_known_duration() {
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            file_size: None,
            duration_ms: Some(180_000),
            ..Default::default()
        };
        // 180s * 44100 * 2ch * 2bytes + 44 header
        let expected = 180 * 44100 * 2 * 2 + 44;
        assert_eq!(info.wav_content_length(), Some(expected));
    }

    #[test]
    fn wav_content_length_no_duration() {
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            file_size: None,
            duration_ms: None,
            ..Default::default()
        };
        assert_eq!(info.wav_content_length(), None);
    }

    #[test]
    fn wav_content_length_hires() {
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 96000,
            bit_depth: 24,
            channels: 2,
            file_size: None,
            duration_ms: Some(256_487),
            ..Default::default()
        };
        let expected = 256_487u64 * 96000 * 2 * 3 / 1000 + 44;
        assert_eq!(info.wav_content_length(), Some(expected));
    }

    /// La longueur annoncée doit tomber sur une TRAME entière (#3163).
    ///
    /// 192 705 ms est la durée en bibliothèque de « Culture Of Fear »
    /// (Thievery Corporation), la piste du fil 1641. En 44,1 kHz / 16 bits /
    /// stéréo la formule brute rend 33 993 162 octets, soit 8 498 290 trames
    /// **plus 2 octets** — exactement le reliquat de 2 octets du journal de
    /// Steve Taylor. Le client HTTP lit précisément ce `Content-Length`, donc
    /// ces 2 octets entrent dans l'adaptateur PCM et s'y retrouvent coincés :
    /// une fraction de trame n'est pas de l'audio.
    #[test]
    fn wav_content_length_tombe_sur_une_trame_entiere() {
        for (duration_ms, sample_rate, bit_depth, channels) in [
            (192_705u64, 44_100u32, 16u16, 2u16), // reliquat brut de 2 octets
            (224_774, 44_100, 16, 2),             // reliquat brut de 1 octet
            (192_705, 96_000, 24, 2),             // trame de 6 octets
            (1_001, 44_100, 32, 6),               // trame de 24 octets
        ] {
            let info = StreamInfo {
                format: "wav".into(),
                mime_type: "audio/wav".into(),
                sample_rate,
                bit_depth,
                channels,
                file_size: None,
                duration_ms: Some(duration_ms),
                ..Default::default()
            };
            let frame_bytes = channels as u64 * (bit_depth as u64 / 8);
            let annoncee = info.wav_content_length().expect("longueur connue");
            assert_eq!(
                (annoncee - 44) % frame_bytes,
                0,
                "{duration_ms} ms en {sample_rate}/{bit_depth}/{channels} : \
                 la longueur annoncée coupe au milieu d'une trame de {frame_bytes} octets"
            );
            let brute = duration_ms * sample_rate as u64 * frame_bytes / 1000;
            assert_eq!(
                annoncee,
                44 + brute - brute % frame_bytes,
                "seul le reliquat sous-trame doit disparaître, pas une trame entière"
            );
        }
    }

    /// Le témoin vert : une durée qui tombe déjà juste ne perd pas un octet.
    #[test]
    fn wav_content_length_ne_change_rien_quand_la_duree_tombe_juste() {
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 44_100,
            bit_depth: 16,
            channels: 2,
            file_size: None,
            duration_ms: Some(180_000),
            ..Default::default()
        };
        assert_eq!(info.wav_content_length(), Some(180 * 44100 * 2 * 2 + 44));
    }

    // ─── Péremption des sessions : activité, pas âge (#2536) ────────
    //
    // Les bornes réelles se comptent en dizaines de minutes ; les tests
    // injectent les leurs (`cleanup_stale_sessions_with`) et jouent sur des
    // centaines de millisecondes. Aucune horloge n'est simulée : `created_at`
    // est un `std::time::Instant`, que `tokio::time::pause()` n'atteint pas.
    // Les marges sont donc prises larges (au moins 3×) pour qu'une machine
    // chargée ne fasse pas basculer un verdict.

    fn info_de_test() -> StreamInfo {
        StreamInfo {
            format: "flac".into(),
            mime_type: "audio/flac".into(),
            sample_rate: 44_100,
            bit_depth: 16,
            channels: 2,
            ..Default::default()
        }
    }

    /// Simuler ce que fait le corps HTTP : servir des octets.
    async fn servir_des_octets(streamer: &AudioStreamer, id: &str) {
        let sessions = streamer.sessions_state();
        let guard = sessions.lock().await;
        if let Some(s) = guard.get(id) {
            s.bytes_sent
                .fetch_add(65_536, std::sync::atomic::Ordering::Relaxed);
        }
    }

    async fn session_presente(streamer: &AudioStreamer, id: &str) -> bool {
        streamer.sessions_state().lock().await.contains_key(id)
    }

    async fn patienter(ms: u64) {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    }

    /// Une image de CD d'une heure débite pendant des heures : son âge n'a
    /// jamais rien dit de son état. Tant que des octets partent, elle vit.
    #[tokio::test]
    async fn une_session_qui_debite_survit_a_son_age() {
        let streamer = AudioStreamer::new(8080);
        let (id, _tx, _ready) = streamer.create_session(info_de_test(), false, 8).await;

        let inactivite = std::time::Duration::from_millis(1_500);
        let plafond = std::time::Duration::from_secs(3_600);

        // 2 s de lecture continue, soit bien au-delà de la borne d'inactivité.
        for _ in 0..8 {
            servir_des_octets(&streamer, &id).await;
            patienter(250).await;
            streamer
                .cleanup_stale_sessions_with(inactivite, plafond)
                .await;
        }

        assert!(
            session_presente(&streamer, &id).await,
            "une session dont les octets partent encore ne doit jamais être ramassée"
        );
    }

    /// Le piège symétrique : une session abandonnée doit toujours partir.
    #[tokio::test]
    async fn une_session_muette_est_ramassee() {
        let streamer = AudioStreamer::new(8080);
        let (id, _tx, _ready) = streamer.create_session(info_de_test(), false, 8).await;

        patienter(900).await;
        let retires = streamer
            .cleanup_stale_sessions_with(
                std::time::Duration::from_millis(300),
                std::time::Duration::from_secs(3_600),
            )
            .await;

        assert_eq!(retires, 1, "une session qui n'a jamais servi un octet fuit");
        assert!(!session_presente(&streamer, &id).await);
    }

    /// Reprise après une pause : le compteur d'inactivité repart de zéro.
    #[tokio::test]
    async fn la_reprise_d_activite_relance_le_compteur() {
        let streamer = AudioStreamer::new(8080);
        let (id, _tx, _ready) = streamer.create_session(info_de_test(), false, 8).await;

        let inactivite = std::time::Duration::from_millis(1_000);
        let plafond = std::time::Duration::from_secs(3_600);

        // Silence court : sous la borne, la session reste.
        patienter(400).await;
        streamer
            .cleanup_stale_sessions_with(inactivite, plafond)
            .await;
        assert!(session_presente(&streamer, &id).await);

        // La lecture reprend.
        servir_des_octets(&streamer, &id).await;
        patienter(400).await;
        streamer
            .cleanup_stale_sessions_with(inactivite, plafond)
            .await;
        assert!(session_presente(&streamer, &id).await);

        // Nouveau silence, court lui aussi : l'âge dépasse la borne, pas
        // l'inactivité. C'est ici que l'ancien critère la ramassait.
        patienter(400).await;
        streamer
            .cleanup_stale_sessions_with(inactivite, plafond)
            .await;
        assert!(
            session_presente(&streamer, &id).await,
            "l'activité intermédiaire doit avoir remis le compteur à zéro"
        );

        // Silence long : cette fois elle part.
        patienter(1_400).await;
        let retires = streamer
            .cleanup_stale_sessions_with(inactivite, plafond)
            .await;
        assert_eq!(retires, 1);
        assert!(!session_presente(&streamer, &id).await);
    }

    /// Le filet : une session qui débite sans fin ne vit pas pour autant
    /// éternellement — mémoire, fichier temporaire et socket amont se libèrent.
    #[tokio::test]
    async fn le_plafond_absolu_ramasse_une_session_eternellement_active() {
        let streamer = AudioStreamer::new(8080);
        let (id, _tx, _ready) = streamer.create_session(info_de_test(), false, 8).await;

        let inactivite = std::time::Duration::from_secs(3_600);
        let plafond = std::time::Duration::from_millis(600);

        let mut retires = 0;
        for _ in 0..5 {
            servir_des_octets(&streamer, &id).await;
            patienter(200).await;
            retires += streamer
                .cleanup_stale_sessions_with(inactivite, plafond)
                .await;
        }

        assert_eq!(
            retires, 1,
            "le plafond absolu doit finir par reprendre une session sans fin"
        );
        assert!(!session_presente(&streamer, &id).await);
    }

    /// La radio reste hors du ramasse-miettes, comme avant.
    #[tokio::test]
    async fn la_radio_reste_exemptee_des_deux_bornes() {
        let streamer = AudioStreamer::new(8080);
        let (id, _tx, _ready, _session) = streamer.create_radio_session(info_de_test(), 8).await;

        patienter(300).await;
        let retires = streamer
            .cleanup_stale_sessions_with(
                std::time::Duration::from_millis(50),
                std::time::Duration::from_millis(50),
            )
            .await;

        assert_eq!(retires, 0);
        assert!(session_presente(&streamer, &id).await);
    }

    // ────────────── #2991 — par où le renderer apprend le changement ─────────
    //
    // Ces épreuves appellent `canal_radio`, la fonction que le poller appelle.
    // Chaque identifiant porte la clé de l'agent : les deux registres sont des
    // `static`, partagés par toutes les épreuves du même binaire.

    /// La branche que la garde du poller traversait EN SILENCE. Sans
    /// `stream_id`, rien n'est publié — et il faut que ça se sache.
    #[test]
    fn sans_stream_id_le_canal_le_dit_au_lieu_de_se_taire() {
        assert_eq!(canal_radio(None), CanalRadio::SansStreamId);
        assert!(!canal_radio(None).atteint_le_renderer());
        assert!(
            canal_radio(None).libelle().contains("stream_id"),
            "le libellé doit nommer la cause, il est lu SEUL au journal"
        );
    }

    /// Un `stream_id` connu du poller mais auquel aucun renderer ne s'est
    /// connecté ne prouve rien : ce n'est ni un succès ni un refus d'ICY.
    #[test]
    fn sans_connexion_le_canal_ne_conclut_pas_a_l_icy() {
        let sid = "i2991-a4f218-jamais-connecte";
        forget_icy_channel(sid);
        assert_eq!(canal_radio(Some(sid)), CanalRadio::AucuneConnexion);
    }

    /// Les deux hypothèses ouvertes du ticket, que rien ne distinguait :
    /// « le renderer n'a pas demandé l'ICY » et « il l'a demandé et on le lui a
    /// accordé ». Elles rendent maintenant deux verdicts différents.
    #[test]
    fn le_canal_distingue_l_icy_accorde_du_non_demande() {
        let accorde = "i2991-a4f218-accorde";
        note_icy_channel(accorde, true, true, VOIE_FLUX);
        assert_eq!(canal_radio(Some(accorde)), CanalRadio::Icy);
        assert!(canal_radio(Some(accorde)).atteint_le_renderer());

        let muet = "i2991-a4f218-non-demande";
        note_icy_channel(muet, false, false, VOIE_FLUX);
        assert_eq!(canal_radio(Some(muet)), CanalRadio::IcyNonDemande);
        assert!(!canal_radio(Some(muet)).atteint_le_renderer());

        let refuse = "i2991-a4f218-refuse";
        note_icy_channel(refuse, true, false, VOIE_FLUX);
        assert_eq!(canal_radio(Some(refuse)), CanalRadio::IcyRefuse);

        forget_icy_channel(accorde);
        forget_icy_channel(muet);
        forget_icy_channel(refuse);
    }

    /// Les branches fichier et mandataire ne découpent pas le corps : elles ne
    /// peuvent porter AUCUN bloc, même à un renderer qui a demandé l'ICY. Ce
    /// verdict-là ne doit pas être confondu avec un refus.
    #[test]
    fn une_voie_sans_decoupe_ne_porte_aucun_bloc_meme_si_l_icy_est_demande() {
        for voie in [VOIE_FICHIER, VOIE_MANDATAIRE] {
            let sid = format!("i2991-a4f218-voie-{voie}");
            note_icy_channel(&sid, true, false, voie);
            assert_eq!(
                canal_radio(Some(&sid)),
                CanalRadio::VoieSansIcy,
                "la voie « {voie} » n'insère aucun bloc ICY"
            );
            forget_icy_channel(&sid);
        }
    }

    /// Le registre suit la vie de la session, comme `RADIO_NOW` : sinon il
    /// grossirait d'une entrée par flux écouté, et un `stream_id` réémployé
    /// hériterait du verdict d'un autre appareil.
    #[tokio::test]
    async fn retirer_une_session_oublie_son_canal() {
        let streamer = AudioStreamer::new(8080);
        let (id, _tx, _ready, _session) = streamer.create_radio_session(info_de_test(), 8).await;
        note_icy_channel(&id, true, true, VOIE_FLUX);
        assert_eq!(canal_radio(Some(&id)), CanalRadio::Icy);

        streamer.remove_session(&id).await;
        assert_eq!(
            canal_radio(Some(&id)),
            CanalRadio::AucuneConnexion,
            "le canal négocié doit mourir avec la session"
        );
    }
}
