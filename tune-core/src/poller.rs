use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use tokio::sync::{Mutex, Notify};
use tokio::time::Duration;
use tracing::{debug, info, warn};

/// Global notify used to wake the poller immediately when a local audio
/// output reaches end-of-stream.  Without this, the poller only discovers
/// `track_ended_naturally` on the next 1-second tick, introducing an
/// average 500 ms gap between tracks on local output.
pub static TRACK_END_NOTIFY: LazyLock<Arc<Notify>> = LazyLock::new(|| Arc::new(Notify::new()));

/// v0.9 rc.2 — when set (`TUNE_POLLER_FSM_SHADOW=1`), the poller runs the FSM
/// `fsm::classify_stopped` in shadow alongside the imperative `Stopped` arm and
/// logs any divergence. The FSM never acts; the legacy arm stays authoritative.
static POLLER_FSM_SHADOW: LazyLock<bool> = LazyLock::new(|| {
    matches!(
        std::env::var("TUNE_POLLER_FSM_SHADOW").as_deref(),
        Ok("1") | Ok("true")
    )
});

use crate::db::zone_repo::ZoneRepo;
use crate::orchestrator::PlaybackOrchestrator;
use crate::outputs::registry::OutputRegistry;
use crate::outputs::traits::{
    OutputDspMetrics, OutputSignalPathStatus, OutputStatus, OutputTarget, TransportState,
};
use crate::playback::{PlayState, PlaybackManager, RepeatMode};

/// Upper bound on a single `get_status` poll (device lock + transport call).
/// The poller is ONE sequential task over every zone: an output whose
/// transport blocks with no socket timeout (rust_cast/Chromecast does raw
/// blocking I/O) would otherwise freeze end-of-track detection for ALL
/// zones — tracks end but nothing ever advances. Override with
/// `TUNE_POLLER_STATUS_TIMEOUT_SECS`; 0 disables the bound (pre-fix behavior).
static STATUS_POLL_TIMEOUT: LazyLock<Option<Duration>> = LazyLock::new(|| {
    let secs = std::env::var("TUNE_POLLER_STATUS_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(5);
    (secs > 0).then(|| Duration::from_secs(secs))
});

/// Lock the output and poll its status, bounded by `timeout`. On timeout the
/// in-flight transport call is abandoned (a blocking-pool thread may linger
/// until its socket dies) but the poller moves on to the next zone instead of
/// stalling every zone behind one dead device. The bound covers the lock
/// acquisition too: an orchestrator call hung inside the same output must not
/// stall the poller either.
/// Bounded status lock with the synchronous runtime
/// signal observation captured before releasing the output. Taking both under
/// one lock prevents the UI from combining a status from one track with the
/// signal contract of the next one.
async fn get_status_with_signal_path_bounded(
    output_arc: &Arc<Mutex<Box<dyn OutputTarget>>>,
    timeout: Option<Duration>,
) -> Result<
    (
        OutputStatus,
        Option<OutputSignalPathStatus>,
        Option<OutputDspMetrics>,
    ),
    String,
> {
    let poll = async {
        let output = output_arc.lock().await;
        let status = output.get_status().await?;
        Ok((status, output.signal_path_status(), output.dsp_metrics()))
    };
    match timeout {
        Some(t) => tokio::time::timeout(t, poll)
            .await
            .unwrap_or_else(|_| Err(format!("get_status timed out after {}s", t.as_secs()))),
        None => poll.await,
    }
}

pub type PollerMetricsMap = Arc<Mutex<HashMap<i64, ZonePollerMetrics>>>;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ZonePollerMetrics {
    pub total_polls: u64,
    pub total_errors: u64,
    pub consecutive_errors: u8,
    pub last_latency_ms: u32,
    pub max_latency_ms: u32,
    /// L'appareil annonce toujours jouer alors que la position est arrivee a la
    /// fin de la piste — ou l'a depassee — depuis [`DEPASSEMENT_DUREE_TICKS`]
    /// ticks consecutifs (#2493).
    ///
    /// Champ de CONSTAT, jamais de commande : rien dans le sondeur ne s'appuie
    /// dessus pour avancer ou arreter une piste. Il existe pour qu'un
    /// diagnostic de zone cesse d'affirmer une lecture normale quand Tune sait
    /// deja qu'elle ne l'est pas — soit la lecture est bloquee, soit la duree
    /// connue est fausse, et le sondeur ne peut pas trancher entre les deux.
    pub lecture_au_dela_de_la_duree: bool,
}

const POLL_INTERVAL_MS: u64 = 1000;
/// Plafond du recul sur une zone arrêtée : 2^5 = 32 ticks, soit ~32 s entre
/// deux tentatives quand l'appareil ne répond plus. Assez pour cesser de le
/// noyer, assez court pour repérer une lecture démarrée depuis sa façade.
const IDLE_BACKOFF_MAX_SHIFT: u8 = 5;
/// Cadence de sondage d'une zone **que Tune ne croit pas en lecture et dont
/// le renderer répond**, en secondes d'horloge murale.
///
/// Une zone qui ne joue rien n'est sondée que pour une seule raison : repérer
/// une lecture démarrée hors de Tune (façade de l'appareil, autre
/// application). Tout ce que la branche « repos » sait faire est verrouillé
/// derrière `status.state == TransportState::Playing` — l'adoption du volume
/// réglé sur l'appareil comme la reprise d'état l'exigent l'une et l'autre.
/// Face à un renderer qui répond `Stopped`, les trois actions SOAP d'un tick
/// ne produisent donc rien, et elles partaient chaque seconde, indéfiniment :
/// sur une installation qui se repose 20 h par jour pour 4 h de lecture,
/// c'est le premier poste de dépense UPnP, loin devant la lecture elle-même
/// (#2263).
///
/// **La pause était le trou laissé par ce premier correctif.** Une zone en
/// pause n'est pas `PlayState::Playing` : elle passe par la branche « repos »
/// exactement comme une zone arrêtée. Mais son renderer répond
/// `PAUSED_PLAYBACK`, que le recul comptait comme un « transport actif », et
/// la zone restait donc sondée à CHAQUE tick, sans fin — 3 actions SOAP par
/// seconde, 259 200 par tranche de 24 h, le prix exact qu'une zone arrêtée
/// payait avant. Or aucun consommateur de la branche ne regarde un statut en
/// pause : les deux seuls, l'adoption du volume et la reprise d'état, exigent
/// `Playing`. Le plein rythme y payait donc une lecture que personne ne fait.
///
/// Ce qui est échangé : le délai de détection d'une lecture — ou d'une
/// reprise — lancée depuis la façade de l'appareil passe de 1 s à 5 s au
/// pire. Dès que le renderer annonce `Playing` ou `Transitioning`, la cadence
/// repart à plein régime au tick suivant — reprise d'état, synchronisation du
/// volume et détection de conflit gardent exactement le rythme qu'elles ont
/// aujourd'hui.
///
/// Écrit en secondes, converti en ticks à la compilation. `POLL_INTERVAL_MS`
/// est le facteur de conversion implicite entre les garde-fous du sondeur
/// comptés en ticks et ceux comptés en horloge murale ; poser « 5 ticks » ici
/// aurait ajouté un garde-fou de plus à la première famille, celle qui se
/// désaccorde en silence le jour où la cadence devient réglable.
const IDLE_REPOS_POLL_SECS: u64 = 5;
/// [`IDLE_REPOS_POLL_SECS`] exprimé en ticks de sondeur. Jamais zéro : une
/// cadence de repos plus courte qu'un tick vaut « à chaque tick ».
const IDLE_REPOS_POLL_TICKS: u8 = {
    let ticks = (IDLE_REPOS_POLL_SECS * 1000).div_ceil(POLL_INTERVAL_MS);
    if ticks == 0 {
        1
    } else if ticks > u8::MAX as u64 {
        u8::MAX
    } else {
        ticks as u8
    }
};
const GAPLESS_WINDOW_MS: u64 = 30_000;

/// Au-dela de ce delai, une piste mise en attente n'est plus fiable et doit
/// etre repreparee.
///
/// `prepare_gapless` ouvre un flux pour la piste suivante ; ce flux attend
/// qu'on le lise et abandonne au bout de 300 s (`SEND_TIMEOUT_SECS`, decode.rs)
/// pour ne pas laisser un decodage tourner dans le vide. L'armement se faisant
/// dans les 30 dernieres secondes d'un morceau, une pause prise juste avant la
/// fin suffit a faire mourir la preparation pendant l'absence : au retour, le
/// morceau se termine, le renderer va chercher une adresse qui ne repond plus,
/// 0 octet part, et le poller finit par tuer la zone (Progman, 9 aout 2026 —
/// « Longue pause -> Reprise -> Fin de piste » casse l'enchainement en DLNA).
///
/// Confortablement sous les 300 s : repreparer coute un transcodage, se
/// tromper coute un blanc et une zone arretee.
const GAPLESS_STAGE_MAX_AGE_SECS: u64 = 200;

/// Fenêtre minimale entre deux relances automatiques « démarrage mort »
/// (#2394) sur une même zone. Assez longue pour qu'un second échec dans la
/// foulée signifie « l'appareil est vraiment planté, on coupe » ; assez
/// courte pour redonner sa chance à l'album suivant.
const DEAD_START_RETRY_COOLDOWN_SECS: u64 = 180;
const STOPPED_TICKS_THRESHOLD: u8 = 5;
/// Part du fichier qui doit avoir été servie pour qu'un `Stopped` annoncé par le
/// renderer puisse passer pour une fin de morceau. En dessous, il n'a pas pu
/// finir de jouer ce qu'il n'a pas reçu — il a calé.
///
/// Marge volontairement large : un FLAC est à débit variable, la dernière
/// fraction de temps ne correspond pas exactement à la même fraction d'octets.
const MIN_SERVED_PERCENT_FOR_NATURAL_END: u64 = 90;
/// Nombre de ticks (~1 s) pendant lesquels on refuse de conclure à une fin
/// naturelle sur un flux manifestement incomplet, laissant au renderer le temps
/// de reprendre sa lecture. Passé ce délai, la lecture a réellement échoué : on
/// arrête la zone bruyamment au lieu d'avancer en silence.
const STALL_DECLINE_MAX_TICKS: u8 = 10;
/// Consecutive ~1 s polls for which a DLNA renderer claims `Playing` while
/// neither its position nor Tune's served-byte counter moves.  The detector is
/// armed only after the normal load grace and after position has demonstrably
/// advanced once, so slow starts and renderers with no position support remain
/// outside its scope.
const PLAYING_STALL_THRESHOLD: u8 = 30;
/// Grace period (seconds) after a seek during which the poller does not
/// overwrite the in-memory position with the value reported by the output.
/// This prevents the progress bar from snapping back to the pre-seek
/// position while the local/cpal output restarts its stream.
const SEEK_GRACE_SECS: u64 = 3;
/// Extended grace period (seconds) for streaming seeks on network outputs
/// (Qobuz/Tidal via DLNA).  Seeking on a proxied stream recreates the
/// stream session and re-sends SetAVTransportURI+Play+Seek — the renderer
/// may report Stopped for several seconds while buffering the new stream.
/// During this window the poller must not accumulate stopped_ticks.
const SEEK_STREAMING_GRACE_SECS: u64 = 10;
/// After this many consecutive Stopped ticks without enough playback,
/// treat as playback failure and stop the zone (don't advance).
/// Increased from 6 to 15 to accommodate slow DLNA renderers (Shanling SCD1.3,
/// MPlayer-based) that report Stopped/position=0 while buffering.
const STOPPED_FAILURE_THRESHOLD: u8 = 30;
/// Grace period (seconds) after a new track is loaded (track_generation
/// changes).  During this window the poller suppresses stopped_ticks to
/// let the renderer buffer — especially important for streaming sources
/// that require transcoding (e.g. Tidal AAC→FLAC for DLNA) which can
/// take 5-15 seconds before the renderer receives any audio data.
const TRACK_LOAD_GRACE_SECS: u64 = 45;
const RADIO_POLL_INTERVAL_SECS: u64 = 15;
/// How many consecutive unplayable queue items the auto-advance will skip
/// before giving up and stopping the zone. One or two dead tracks in an album
/// is ordinary (a title pulled from the catalogue); a long run means something
/// systemic — expired credentials, no network — where retrying once per queued
/// item would just hammer the service.
const MAX_CONSECUTIVE_SKIPS: u32 = 25;
/// Grace period after SetNextAVTransportURI during which we treat Stopped
/// state and position resets as gapless transitions instead of track-end.
const GAPLESS_GUARD_SECS: u64 = 15;
/// After a stall-recovery restart (OAAT stall supervisor replays the current
/// track from 0), suppress a gapless position-reset auto-advance for this many
/// seconds. A genuine end-of-track transition cannot occur this soon after a
/// from-zero replay, so the window only ever swallows the phantom advance; the
/// natural-end fallback still advances tracks shorter than the window.
const RESTART_ADVANCE_SUPPRESS_SECS: u64 = 20;
/// Minimum fraction of track duration that must have been played before a
/// gapless transition is accepted.  Prevents false transitions when a
/// renderer (e.g. DMP-A8) reports state changes immediately after
/// SetNextAVTransportURI.
const MIN_PLAYED_FRACTION: f64 = 0.80;
/// A renderer's `ended_naturally` signal is only trusted once at least this
/// fraction of the (known) track duration has elapsed in WALL-CLOCK time — a
/// track physically cannot end at 1x playback before then. Guards against
/// renderers (Eversolo DMP-A8) that falsely report ended_naturally seconds into
/// a multi-minute track when their internal player chokes (Lavf range-hunting a
/// large FLAC), which would advance the queue prematurely.
const MIN_WALL_FRACTION_FOR_NATURAL_END: f64 = 0.5;
/// Minimum wall-clock seconds a track must have been playing before we accept
/// a gapless transition. Prevents false skips when a renderer fails to decode
/// and reports STOPPED after only a few seconds.
const MIN_TRACK_WALL_SECS: u64 = 30;
/// Minimum peak position (ms) required before accepting track-end when the
/// track duration is unknown (0).  Prevents false skips on slow renderers
/// (e.g. Shanling SCD1.3) that report duration=0 and briefly show Stopped
/// state while buffering.  60 seconds is long enough to avoid false positives
/// while still handling actual short tracks via the `is_short_track` path.
const MIN_PEAK_UNKNOWN_DURATION_MS: u64 = 60_000;
/// How often (in ticks) to persist the playback position to the database.
const POSITION_SAVE_INTERVAL_TICKS: u64 = 10;
/// When the output reports Playing but position >= track duration (track
/// effectively ended), wait this many ticks before advancing. This gives
/// the output time to drain its buffer and report Stopped naturally.
/// If it doesn't, this threshold forces the advance.
const POSITION_PAST_END_TICKS: u8 = 3;
/// Minimum consecutive failed status polls before the DLNA wall-clock poll-fail
/// fallback (`decisions::poll_failed_past_end`) will end the track. Requiring a
/// couple of failures avoids acting on a single transient SOAP blip.
const POLL_FAIL_END_MIN_ERRORS: u8 = 2;
/// Ticks de scrutation (~1 s) pendant lesquels la position doit rester collee a
/// la fin — ou au-dela — avant que Tune ose DIRE que l'etat annonce ne veut plus
/// rien dire (#2493).
///
/// Vingt fois [`POSITION_PAST_END_TICKS`] : tous les detecteurs de fin de piste
/// agissent en trois ticks. Ce compteur ne commence donc a compter que la ou ils
/// ont TOUS deja renonce, et n'atteint son seuil qu'une minute plus tard. Une
/// transition sans blanc, une reprise apres pause, une fin de piste normale sont
/// toutes reglees bien avant — le compteur repart de zero a chaque changement de
/// piste et des que la position quitte la zone de fin.
const DEPASSEMENT_DUREE_TICKS: u8 = 60;
/// After a gapless metadata advance (the poller called advance_queue_metadata
/// expecting the renderer to auto-transition), if the renderer stays Stopped
/// for this many ticks (after gapless_cooldown expires), force a play_from_queue.
/// This handles renderers that accept SetNextAVTransportURI but don't actually
/// auto-transition — the poller would otherwise get stuck forever.
const GAPLESS_STUCK_THRESHOLD: u8 = 2;
/// Grace period (seconds) after a user volume change during which the poller
/// does not overwrite the volume with the renderer-reported value. Prevents
/// the slider from bouncing back on DLNA renderers with latent GetVolume.
const VOLUME_GRACE_SECS: u64 = 5;

/// Délai au-delà duquel une lecture dont pas un octet n'a été tiré n'est plus
/// un démarrage lent mais un silence établi.
///
/// C'est le seuil que `output_reach` utilisait déjà pour dire
/// `browser_unattended` au client (`tune-server/src/routes/zones.rs`) : le
/// client web branche son `<audio>` sur `stream_url` dès qu'il a l'état de la
/// zone, et les premiers octets partent en une seconde ou deux. Douze secondes
/// laissent de la marge à un poste lent sans laisser l'utilisateur dans le noir.
///
/// Il vit ici, dans `tune-core`, parce que le poller et la vue des zones
/// doivent trancher au MÊME instant : deux seuils, et le serveur dirait à la
/// fois « personne ne reçoit ce son » et « en lecture ».
pub const DELAI_SILENCE_ETABLI: Duration = Duration::from_secs(12);

/// Pure decision predicates extracted **verbatim** from the poller `tick`
/// loop. They contain no I/O and no state mutation, so they can be unit-tested
/// against the *real* code path.
///
/// Rationale (v0.9 rc.1 filet): the previous `played_enough_*` tests
/// re-implemented the logic inline and therefore never exercised the actual
/// predicate — notably they omitted the `wall_elapsed >= MIN_TRACK_WALL_SECS`
/// guard. Extracting the predicates here makes the characterization tests
/// faithful and seeds the future poller state machine (Axe 2): the FSM
/// `transition` will call exactly these functions.
pub(crate) mod decisions {
    /// Qui tient réellement le renderer, d'après l'URI qu'il rapporte.
    ///
    /// Le terrain (24/08, DMP-A8 de Bertrand) : DEUX serveurs Tune avaient
    /// chacun une zone sur le même appareil, et le lecteur interne de
    /// l'Eversolo s'y ajoutait après un redémarrage. Chaque perdant échouait
    /// EN SILENCE — l'interface relançait la lecture toutes les quinze
    /// secondes, et un conflit d'appareil s'est déguisé en « bug DSD ».
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum TenueDuRenderer {
        /// Il joue notre flux — ou ne rapporte rien d'exploitable.
        LeNotre,
        /// Il joue le flux d'un AUTRE serveur Tune (l'hôte extrait de l'URI).
        AutreServeurTune(String),
        /// Il joue autre chose (BubbleUPnP, une autre application…).
        AutreApplication,
        /// URI vide mais transport actif : son propre lecteur interne
        /// (l'Eversolo restaure sa lecture locale après un redémarrage).
        LecteurInterne,
    }

    /// Décide qui tient le renderer.
    ///
    /// - `current_uri = None` → [`TenueDuRenderer::LeNotre`] : beaucoup de
    ///   renderers ne rapportent pas `TrackURI` — l'absence de signal n'est
    ///   pas une preuve, on ne crie pas sans preuve ;
    /// - URI vide → lecteur interne (ne se juge que transport actif, c'est à
    ///   l'appelant de ne demander qu'alors) ;
    /// - URI portant NOTRE `stream_id` → à nous ;
    /// - URI au motif `/stream/…` d'un flux Tune, sans notre id → un autre
    ///   serveur Tune, dont on extrait l'hôte pour le NOMMER à l'écran ;
    /// - toute autre URI → une autre application.
    pub fn qui_tient_le_renderer(
        current_uri: Option<&str>,
        notre_stream_id: Option<&str>,
    ) -> TenueDuRenderer {
        let Some(uri) = current_uri else {
            return TenueDuRenderer::LeNotre;
        };
        let uri = uri.trim();
        if uri.is_empty() {
            return TenueDuRenderer::LecteurInterne;
        }
        if let Some(sid) = notre_stream_id
            && !sid.is_empty()
            && uri.contains(sid)
        {
            return TenueDuRenderer::LeNotre;
        }
        if uri.contains("/stream/") {
            let hote = uri
                .strip_prefix("http://")
                .or_else(|| uri.strip_prefix("https://"))
                .and_then(|r| r.split('/').next())
                .unwrap_or("?")
                .to_string();
            return TenueDuRenderer::AutreServeurTune(hote);
        }
        TenueDuRenderer::AutreApplication
    }

    /// Retrouver, dans l'URI qu'un renderer déclare jouer, le `stream_id` de la
    /// session Tune qu'il tire.
    ///
    /// ── Pourquoi cette fonction existe (#2991) ──
    ///
    /// La reprise « le renderer joue, Tune ne le croyait pas » reconstruit un
    /// now-playing depuis la BASE (`last_track_source`, `last_track_source_id`)
    /// et pose `stream_id: None` — la base ne mémorise pas l'identifiant d'une
    /// session, qui est éphémère. Pour une piste locale c'est sans conséquence.
    /// Pour une RADIO, c'est la panne : `refresh_radio_metadata` recopie ce
    /// `None` dans chaque now-playing suivant, et la garde qui publie le titre
    /// vers le flux ne mord plus JAMAIS de toute la session. L'interface Tune,
    /// elle, continue d'être servie par `update_now_playing`, qui ne dépend pas
    /// du `stream_id` — d'où « dans Tune ça fonctionne, sur le lecteur réseau
    /// non » (Serge Asselin, Hifi Rose RS250A, fil 1529).
    ///
    /// L'identifiant n'est pas perdu : le renderer l'annonce lui-même, dans
    /// l'URI qu'il est en train de tirer. On ne l'invente pas, on le RELIT — et
    /// l'appelant vérifie ensuite auprès du gestionnaire de flux que cette
    /// session est bien l'une des NÔTRES (une URI `/stream/…` peut venir d'un
    /// autre Tune du réseau, cf. [`TenueDuRenderer::AutreServeurTune`]).
    ///
    /// La découpe `<id>.<ext>` est celle du serveur de flux lui-même
    /// ([`crate::http::streamer::extract_stream_id`]), appelée et non recopiée :
    /// le jour où la convention change, les deux bougent ensemble.
    pub fn stream_id_de_l_uri(current_uri: Option<&str>) -> Option<String> {
        let uri = current_uri?.trim();
        let apres = uri.rsplit_once("/stream/")?.1;
        // Un renderer peut recopier une chaîne de requête ou une ancre.
        let sans_suffixe = apres.split(['?', '#']).next().unwrap_or(apres);
        let id = crate::http::streamer::extract_stream_id(sans_suffixe);
        (!id.is_empty()).then(|| id.to_string())
    }

    use super::{
        DEAD_START_RETRY_COOLDOWN_SECS, GAPLESS_STAGE_MAX_AGE_SECS, GAPLESS_STUCK_THRESHOLD,
        GAPLESS_WINDOW_MS, MIN_PEAK_UNKNOWN_DURATION_MS, MIN_PLAYED_FRACTION, MIN_TRACK_WALL_SECS,
        MIN_WALL_FRACTION_FOR_NATURAL_END, POLL_FAIL_END_MIN_ERRORS, POLL_INTERVAL_MS,
        POSITION_PAST_END_TICKS, STOPPED_TICKS_THRESHOLD,
    };

    /// Margin (ms) added to the track duration before position-based
    /// end-of-track is accepted, to avoid clipping the last fraction of a
    /// second on renderers that report position slightly ahead of playback.
    pub const END_MARGIN_MS: u64 = 3000;

    /// Motif — stable, journalisé — de la branche qui a conclu « la piste est
    /// finie ». Un seul mot par branche, écrit tel quel dans `track_end_gap`.
    ///
    /// Ce ne sont PAS des étiquettes décoratives : chaque branche porte un
    /// plancher de silence différent (voir [`plancher_de_detection_ms`]), et
    /// c'est ce plancher qui décide si un blanc de 3-4 s est explicable par la
    /// détection ou s'il vient d'ailleurs (#2488).
    pub mod motif_fin {
        /// La sortie locale a signalé l'EOF elle-même (`ended_naturally`).
        pub const FIN_NATURELLE_LOCALE: &str = "local_ended_naturally";
        /// DSD sur DLNA : le pic de position a atteint la fin (#402).
        pub const DSD_DLNA_PIC_ATTEINT: &str = "dlna_dsd_reached_end";
        /// Le renderer annonce `Stopped` et le compteur a atteint son seuil.
        pub const FIN_NATURELLE_APRES_STOPPED: &str = "natural_end_after_stopped";
        /// Le renderer avait accepté `SetNext` mais n'a jamais transitionné.
        pub const AVANCE_GAPLESS_BLOQUEE: &str = "gapless_advance_stuck";
        /// La position a dépassé la fin sans que le renderer s'arrête.
        pub const POSITION_AU_DELA_DE_LA_FIN: &str = "position_past_end";
    }

    /// Plancher de silence, en millisecondes, imposé par la BRANCHE de
    /// détection qui a conclu — avant même que la résolution de la piste
    /// suivante ne commence.
    ///
    /// Pourquoi ce chiffre existe (#2488) : `playback_timing` démarre à
    /// `play_inner`, c'est-à-dire APRÈS que le sondeur a décidé d'avancer.
    /// Tout ce qui précède cette décision est aujourd'hui invisible dans le
    /// journal, alors que c'est structurellement le plus gros terme du blanc
    /// entre deux pistes. Cette fonction rend ce terme lisible sans avoir à
    /// relire la source : elle ne dérive que des constantes du sondeur.
    ///
    /// C'est un PLANCHER, pas une mesure : le temps réel s'y ajoute (un tick
    /// n'est pas aligné sur la fin du morceau, et le réseau a son mot à dire).
    ///
    /// Un motif inconnu rend `0` — on ne devine pas.
    pub fn plancher_de_detection_ms(motif: &str) -> u64 {
        let tick = POLL_INTERVAL_MS;
        match motif {
            // La sortie locale réveille le sondeur par `TRACK_END_NOTIFY`
            // au lieu d'attendre le tick : aucun plancher de sondage.
            motif_fin::FIN_NATURELLE_LOCALE => 0,
            // Il faut UN sondage pour observer que le pic a atteint la fin.
            motif_fin::DSD_DLNA_PIC_ATTEINT => tick,
            // Le compteur `stopped_ticks` doit atteindre son seuil.
            motif_fin::FIN_NATURELLE_APRES_STOPPED => STOPPED_TICKS_THRESHOLD as u64 * tick,
            // Compté depuis la fin de la temporisation gapless, qui s'ajoute
            // encore devant : le renderer a accepté `SetNext` puis n'a rien
            // fait, et on attend `GAPLESS_STUCK_THRESHOLD` sondages.
            motif_fin::AVANCE_GAPLESS_BLOQUEE => GAPLESS_STUCK_THRESHOLD as u64 * tick,
            // La position doit d'abord dépasser la fin de `END_MARGIN_MS`
            // — autant de silence déjà écoulé — puis tenir
            // `POSITION_PAST_END_TICKS` sondages.
            motif_fin::POSITION_AU_DELA_DE_LA_FIN => {
                END_MARGIN_MS + POSITION_PAST_END_TICKS as u64 * tick
            }
            _ => 0,
        }
    }

    /// Has enough of the current track been played to accept a track-end or
    /// gapless transition?
    ///
    /// - Known duration: `peak_position_ms >= MIN_PLAYED_FRACTION * duration`.
    /// - Unknown duration (`0`): `peak_position_ms >= MIN_PEAK_UNKNOWN_DURATION_MS`
    ///   (guards slow renderers that report duration 0 while buffering).
    ///
    /// Both branches additionally require `wall_elapsed >= MIN_TRACK_WALL_SECS`.
    /// Le renderer peut-il réellement avoir terminé le morceau ?
    ///
    /// Un `Stopped` au-delà de [`MIN_PLAYED_FRACTION`] est accepté comme une fin
    /// naturelle, en se fiant à la position qu'annonce le renderer. Or sur un
    /// réseau qui hoquette, il cale, cesse de récupérer le flux et annonce
    /// `Stopped` — Tune enchaînait alors sur la piste suivante, amputant la fin
    /// du morceau **sans laisser la moindre trace** (« Us And Them » de JP :
    /// 6:36 jouées sur 7:49).
    ///
    /// Les octets servis tranchent, indépendamment de ce que le renderer
    /// raconte : on ne finit pas de jouer un fichier qu'on n'a pas reçu.
    /// `total_bytes` à `None` (radio, flux décodé) ⇒ on ne juge pas.
    ///
    /// `seeked` neutralise le critère : après un saut dans le morceau, le
    /// renderer ne récupère que la portion restante, les octets servis sont donc
    /// légitimement incomplets et vetoraient une fin parfaitement normale.
    /// (`ZoneState::last_seek_at` est remis à zéro par `play()` à chaque
    /// changement de piste, il vaut donc bien « un saut a eu lieu sur CETTE
    /// piste ».)
    pub fn renderer_could_have_finished(
        bytes_sent: u64,
        total_bytes: Option<u64>,
        seeked: bool,
    ) -> bool {
        if seeked {
            return true;
        }
        match total_bytes {
            None | Some(0) => true,
            Some(total) => {
                bytes_sent.saturating_mul(100)
                    >= total.saturating_mul(super::MIN_SERVED_PERCENT_FOR_NATURAL_END)
            }
        }
    }

    pub fn played_enough(track_duration_ms: u64, peak_position_ms: u64, wall_elapsed: u64) -> bool {
        if track_duration_ms == 0 {
            peak_position_ms >= MIN_PEAK_UNKNOWN_DURATION_MS && wall_elapsed >= MIN_TRACK_WALL_SECS
        } else {
            // A track shorter than MIN_TRACK_WALL_SECS can never reach that much
            // wall-clock time even when played in full — cap the wall-time floor
            // at ~80% of the track's own duration so short tracks (e.g. a 27s
            // streaming variation) are still recognized as ending naturally.
            // Without this, sub-30s tracks never trigger natural-end, so
            // auto-advance and single-track Repeat All silently stop.
            let wall_floor = MIN_TRACK_WALL_SECS.min(track_duration_ms / 1000 * 4 / 5);
            peak_position_ms as f64 >= track_duration_ms as f64 * MIN_PLAYED_FRACTION
                && wall_elapsed >= wall_floor
        }
    }

    /// Whether a renderer's `ended_naturally` signal is plausible given elapsed
    /// wall-clock time. For a known-duration track it must have been playing at
    /// least `MIN_WALL_FRACTION_FOR_NATURAL_END` of its duration (you cannot end
    /// a 4-minute track in 35 seconds at 1x). Unknown duration keeps the original
    /// modest 5-second floor. Rejects the DMP-A8's spurious early ended_naturally.
    ///
    /// The premise — nothing finishes a track faster than 1x — only holds for a
    /// renderer. An output that reports `realtime: false` is exempt; see
    /// [`natural_end`], which is where that exemption is applied.
    pub fn ended_naturally_wall_ok(wall_elapsed: u64, track_duration_ms: u64) -> bool {
        if track_duration_ms == 0 {
            wall_elapsed >= 5
        } else {
            wall_elapsed as f64 * 1000.0
                >= track_duration_ms as f64 * MIN_WALL_FRACTION_FOR_NATURAL_END
        }
    }

    /// « Démarrage mort » (#2394) : l'échec de lecture DLNA où la piste n'a
    /// JAMAIS été tirée (0 octet servi). C'est le profil du pipeline Eversolo
    /// coincé — SOAP et HTTP vivants, lecture morte — que la relance guérit.
    /// Un décrochage en cours de lecture (octets déjà servis) n'en est pas un.
    ///
    /// `bytes_sent` est un compte MESURÉ : l'appelant ne doit l'appeler que
    /// quand la consommation est connue ([`super::fsm::ConsommationFlux::ASec`]).
    /// Un flux dont personne ne connaît le compteur n'est pas un démarrage
    /// mort — voir [`super::fsm::consommation_flux`].
    pub fn demarrage_mort(output_type: &str, bytes_sent: u64) -> bool {
        output_type == "dlna" && bytes_sent == 0
    }

    /// Is a `Playing`-but-dead watchdog meaningful for this sample?
    ///
    /// Every gate removes a known false positive: this is DLNA-only, Tune must
    /// own an actual realtime stream, startup and seek recovery must be over,
    /// the renderer must already have proven that it reports position, and a
    /// known track end is left to the normal end-of-track machinery.
    pub fn dlna_playing_stall_eligible(
        output_type: &str,
        tune_is_playing: bool,
        renderer_is_playing: bool,
        realtime: bool,
        has_stream_id: bool,
        in_seek_grace: bool,
        load_elapsed_secs: u64,
        peak_position_ms: u64,
        position_ms: u64,
        track_duration_ms: u64,
    ) -> bool {
        let near_known_end =
            track_duration_ms > 0 && position_ms.saturating_add(END_MARGIN_MS) >= track_duration_ms;
        output_type == "dlna"
            && tune_is_playing
            && renderer_is_playing
            && realtime
            && has_stream_id
            && !in_seek_grace
            && load_elapsed_secs >= super::TRACK_LOAD_GRACE_SECS
            && peak_position_ms >= 5_000
            && !near_known_end
    }

    /// Advance the consecutive-stall counter only when both independent signs
    /// of life are frozen. Any movement resets the full observation window.
    pub fn next_dlna_playing_stall_ticks(
        previous_ticks: u8,
        eligible: bool,
        previous_position_ms: u64,
        position_ms: u64,
        previous_bytes_sent: u64,
        bytes_sent: u64,
    ) -> u8 {
        if !eligible || position_ms > previous_position_ms || bytes_sent > previous_bytes_sent {
            0
        } else {
            previous_ticks.saturating_add(1)
        }
    }

    /// Une relance automatique après démarrage mort est-elle permise ?
    /// Au plus une par fenêtre : si la précédente date de moins de
    /// DEAD_START_RETRY_COOLDOWN_SECS, l'échec suivant coupe la zone comme
    /// avant — on ne martèle pas un appareil réellement planté ou éteint.
    pub fn relance_demarrage_mort_autorisee(derniere_il_y_a_secs: Option<u64>) -> bool {
        derniere_il_y_a_secs.is_none_or(|s| s > DEAD_START_RETRY_COOLDOWN_SECS)
    }

    /// Le verrou « suivant DSD sur DLNA » (#2394) tient-il encore ? Il ne
    /// tient que pour LA position de file constatée : si la file bouge (ajout,
    /// saut, avance), la position suivante change et on re-résout — au pire on
    /// perd UNE occasion d'armer le gapless (petit blanc), jamais une lecture.
    pub fn dsd_skip_latched(latch: Option<i64>, next_pos: Option<i64>) -> bool {
        latch.is_some() && latch == next_pos
    }

    /// Position dropped from `>30s` to `<5s` while a gapless transition was
    /// armed — a strong signal the renderer auto-advanced to the next track.
    pub fn position_reset(last_position_ms: u64, position_ms: u64, gapless_armed: bool) -> bool {
        last_position_ms > 30_000 && position_ms < 5_000 && gapless_armed
    }

    /// The `position_reset` fallback advances metadata only, assuming the
    /// renderer auto-transitioned internally — its position dropped to 0 because
    /// it is already playing the next track. That premise holds only for outputs
    /// that do internal gapless (DLNA). For a Chromecast / slimproto /
    /// exclusive-local output, a drop to 0 means the track ENDED (device went
    /// IDLE/FINISHED), not that it advanced — advancing metadata sends no `play`
    /// and steals the event from the natural-end path (Stopped branch →
    /// play_from_queue = real load), causing the endless 1-2s-then-zero loop
    /// (Rhorn, #1072). So the fallback only fires for internal-gapless outputs.
    /// It is also forbidden during the seek grace period: recreating the current
    /// stream can produce the same position-drop shape, but it never means the
    /// renderer moved to the next queue item (#2170).
    pub fn position_reset_fires(
        raw_position_reset: bool,
        can_internal_gapless: bool,
        in_seek_grace: bool,
    ) -> bool {
        raw_position_reset && can_internal_gapless && !in_seek_grace
    }

    /// A renderer can report the PREVIOUS session's position for the first
    /// seconds after a fresh Play (Villerio's DMP-A6: ~374s — yesterday's end
    /// position — reported 6s into a new start). That stale sample poisons the
    /// peak, triggers near-end gapless staging seconds into the track, and the
    /// snap back to the real position then reads as a phantom
    /// `position_reset` advance.
    ///
    /// A real position can never exceed the wall time actually elapsed (+15s
    /// margin for seek-restore/clock slack): `track_started_at` is folded by
    /// the seek/resume target (see the "Fold a NEW seek" baseline above), so
    /// `wall_elapsed` tracks the true 1x play position at every point in the
    /// track — not only the first few seconds. Any sample above that ceiling
    /// is therefore provably impossible and must be discarded outright,
    /// whenever it arrives.
    ///
    /// This used to be gated on `wall_elapsed_secs < 30`, which let a renderer
    /// that keeps reporting a stale near-end position for LONGER than 30s
    /// (Bertrand's DMP-A8, .18) poison the peak the instant the 30s grace
    /// lapsed: peak jumped to the fake near-end value, `played_enough` flipped
    /// true, and the very next honest snap-to-0 read as a `position_reset`
    /// advance ~30s into track 1 — the queue pointer ran ahead of the renderer
    /// and the Qobuz playlist appeared to "stop at the first track". Dropping
    /// the window makes the invariant hold for the whole track and cures both
    /// the poisoned-peak advance and the near-end gapless mis-staging.
    pub fn stale_start_position(wall_elapsed_secs: u64, position_ms: u64) -> bool {
        position_ms > wall_elapsed_secs * 1000 + 15_000
    }

    /// The peak position reached (near) the track's full duration, so the track
    /// has demonstrably finished — independent of the wall clock.
    ///
    /// The wall-clock guards (`played_enough`, `ended_naturally_wall_ok`) reset
    /// `track_started_at` on a gapless metadata advance, so when a local FLAC
    /// track (whose gapless pre-arm falls back — the next stream isn't WAV) ends
    /// a couple seconds later, `wall_elapsed` under-counts and those guards
    /// wrongly reject the real end. That stalled auto-advance for ~30s, which
    /// surfaced as tracks restarting/being skipped in a gapless album (Jean
    /// Valjean, local FLAC on WASAPI). When the peak has reached the duration
    /// the track is over regardless of the (unreliable) wall clock.
    pub fn peak_reached_end(track_duration_ms: u64, peak_position_ms: u64) -> bool {
        track_duration_ms > 0
            && peak_position_ms as f64 >= track_duration_ms as f64 * MIN_PLAYED_FRACTION
    }

    /// A DSD track on a DLNA renderer that has demonstrably reached its end.
    ///
    /// Gapless (`SetNextAVTransportURI`) is intentionally NOT armed when the next
    /// track is DSD on a DLNA renderer (`prepare_gapless` skips it — the renderer
    /// accepts SetNext for a DSD stream but never consumes it, so the album cuts
    /// after track 1; HiFi Rose RS130, Benjithom, #402). But DLNA `poll_status`
    /// never reports `ended_naturally`, so with gapless off the only end-of-track
    /// signal left is counting `STOPPED_TICKS_THRESHOLD` Stopped polls — a fixed
    /// ~5s inter-track gap for a DSD album (Benjithom, RS130). When the peak
    /// position has reached the track's end we already know it finished, so the
    /// poller can advance immediately instead of waiting out the counter. PCM/FLAC
    /// on DLNA keep their armed-gapless path and never reach this predicate; DSD on
    /// a local output keeps its internal gapless chain and is out of scope here.
    pub fn dlna_dsd_reached_end(
        output_type: &str,
        current_format: Option<&str>,
        track_duration_ms: u64,
        peak_position_ms: u64,
    ) -> bool {
        if output_type != "dlna" {
            return false;
        }
        let is_dsd = current_format.is_some_and(crate::playback::gapless::est_dsd);
        is_dsd && peak_reached_end(track_duration_ms, peak_position_ms)
    }

    /// After `STOPPED_TICKS_THRESHOLD` consecutive Stopped ticks, should this be
    /// treated as a natural track end (re-trigger play) rather than a playback
    /// failure (stop the zone)?
    ///
    /// `realtime` is [`OutputStatus::realtime`](tune_output_api::OutputStatus):
    /// `false` means the output does not consume the track at 1x — a recorder
    /// that writes the container to disk at network speed finishes a 5-minute
    /// track in a second or two. Every wall-clock plausibility guard here
    /// (`played_enough`'s floor, `ended_naturally_wall_ok`) assumes 1x playback,
    /// so for such an output `ended_naturally` + Stopped is taken at face value.
    /// Without this the queue advanced only after half of each track's DURATION
    /// had elapsed, and a rip ran at half of listening speed instead of network
    /// speed.
    ///
    /// # `peak_reached_end` était écrit pour Jean Valjean, et ne l'atteignait pas (#3229)
    ///
    /// [`peak_reached_end`] n'avait qu'UN appelant de production : la branche
    /// `status.ended_naturally` du bras `Stopped`, dont le commentaire dit
    /// lui-même « Local outputs (WASAPI/ALSA/CoreAudio) signal ended_naturally
    /// when the audio stream reaches EOF ». Or **DLNA ne rend JAMAIS
    /// `ended_naturally`** — c'est écrit noir sur blanc dans
    /// [`dlna_dsd_reached_end`], qui n'existe que pour cette raison. Le seul
    /// autre usage, [`dlna_dsd_reached_end`], est réservé au DSD.
    ///
    /// Le correctif écrit POUR le signalement de Jean Valjean ne mordait donc
    /// sur aucune zone DLNA en PCM/FLAC : là, la fin de piste retombe ici,
    /// après `STOPPED_TICKS_THRESHOLD` sondes `Stopped`. Et ici, `played_enough`
    /// exige un plancher d'horloge murale que l'avance gapless vient justement
    /// de fausser (elle remet `track_started_at`) — la vraie fin était rejetée,
    /// et la zone partait vers la branche d'ÉCHEC qui l'arrête.
    ///
    /// `peak_reached_end` est la même mesure que `played_enough` SANS ce
    /// plancher d'horloge, et elle reste plus étroite que lui sur les deux
    /// autres axes : elle exige une durée connue (`> 0`) et le même
    /// [`MIN_PLAYED_FRACTION`]. L'ajouter ici ne change RIEN au calendrier — on
    /// est toujours après les cinq sondes `Stopped` — mais change le VERDICT :
    /// une piste dont le pic a atteint sa fin est une fin, pas une panne.
    pub fn natural_end(
        played_enough: bool,
        repeat_active: bool,
        peak_position_ms: u64,
        ended_naturally: bool,
        wall_elapsed: u64,
        track_duration_ms: u64,
        realtime: bool,
    ) -> bool {
        let is_short_track =
            track_duration_ms > 0 && track_duration_ms < MIN_TRACK_WALL_SECS * 1000;
        let repeat_end = repeat_active && peak_position_ms > 5_000;
        played_enough
            // Le pic a atteint la fin : la piste est finie, quoi qu'en dise
            // l'horloge murale. C'est ce qui branche enfin le correctif sur le
            // chemin DLNA — le seul que Jean Valjean ait jamais emprunté.
            || peak_reached_end(track_duration_ms, peak_position_ms)
            || repeat_end
            || (ended_naturally
                && (!realtime || ended_naturally_wall_ok(wall_elapsed, track_duration_ms)))
            || (is_short_track && peak_position_ms as f64 >= track_duration_ms as f64 * 0.5)
    }

    /// Should the poller adopt the renderer-reported volume into the saved
    /// zone volume? Only when the reported value actually MOVED since the last
    /// poll (a real change on the device) AND it now differs from what we have
    /// stored. A renderer that keeps reporting a stale default (Fabien's
    /// Devialet stuck at 50%) reports the same value every tick, so `prev`
    /// never differs from `device` and the user's saved volume is preserved.
    pub fn should_adopt_device_volume(
        prev_device_vol: Option<f64>,
        device_vol: f64,
        db_vol: f64,
    ) -> bool {
        prev_device_vol.is_some_and(|prev| (device_vol - prev).abs() > 0.02)
            && (device_vol - db_vol).abs() > 0.02
    }

    /// The renderer now reports a duration that differs from the current
    /// track's by more than 2s — a signal that a gapless transition to the
    /// next track has occurred (only meaningful once gapless was armed).
    pub fn duration_changed(
        gapless_sent: bool,
        track_duration_ms: u64,
        reported_duration_ms: u64,
    ) -> bool {
        gapless_sent
            && track_duration_ms > 0
            && reported_duration_ms > 0
            && (reported_duration_ms as i64 - track_duration_ms as i64).unsigned_abs() > 2000
    }

    /// Does the reported position confirm we are genuinely at the end of the
    /// current track (or reset to the start of the next one)? Guarded by
    /// `played_enough` to reject false transitions on renderers (DMP-A8) that
    /// briefly report position < 5s right after SetNextAVTransportURI.
    pub fn position_confirms_transition(
        played_enough: bool,
        position_ms: u64,
        track_duration_ms: u64,
    ) -> bool {
        played_enough
            && (position_ms < 5000
                || (track_duration_ms > 0
                    && position_ms >= track_duration_ms.saturating_sub(GAPLESS_WINDOW_MS)))
    }

    /// Should `SetNextAVTransportURI` be sent now — i.e. playback has entered
    /// the final `GAPLESS_WINDOW_MS` of the track and gapless is not yet armed?
    ///
    /// Uses the renderer-reported duration when it is available, otherwise falls
    /// back to the queue-known duration (`queue_duration_ms`). The LMS UPnP
    /// bridge (Yacine/Jean-Pierre) reports `reported_duration_ms == 0`, so
    /// without the fallback gapless was never armed for it (0/196 advances). A
    /// well-behaved renderer reports its own duration and is unaffected.
    pub fn should_arm_gapless(
        gapless_sent: bool,
        reported_duration_ms: u64,
        queue_duration_ms: u64,
        position_ms: u64,
    ) -> bool {
        let effective_duration_ms = sane_current_duration(reported_duration_ms, queue_duration_ms);
        !gapless_sent
            && effective_duration_ms > GAPLESS_WINDOW_MS
            && position_ms >= effective_duration_ms - GAPLESS_WINDOW_MS
    }

    /// La piste mise en attente a-t-elle expire ?
    ///
    /// `age_secs` est le temps ecoule depuis `prepare_gapless`. Au-dela de
    /// `GAPLESS_STAGE_MAX_AGE_SECS`, le flux ouvert pour elle a ete abandonne
    /// cote serveur et l'adresse ne repond plus : il faut repreparer.
    pub fn gapless_stage_expired(gapless_sent: bool, age_secs: Option<u64>) -> bool {
        gapless_sent && age_secs.is_some_and(|a| a > GAPLESS_STAGE_MAX_AGE_SECS)
    }

    /// La preparation gapless deja acceptee par le renderer vise-t-elle encore
    /// la piste que la file designe MAINTENANT comme suivante ? (#3026)
    ///
    /// `armed_row_id` est l'identifiant de LIGNE de file (`queue_items.id`)
    /// realement passe a `SetNextAVTransportURI` ; `row_id_at_next` celui que
    /// `next_position` designe a cet instant. Une ligne, et non une position :
    /// « Lire ensuite » DECALE les positions — `insert_at` ouvre un trou par un
    /// `UPDATE position = position + 1`, il ne reecrit pas les lignes — donc la
    /// piste armee change de position sans changer de piste, et l'index+1
    /// designe alors quelqu'un d'autre.
    ///
    /// Ni l'identifiant de piste ni le titre ne feraient l'affaire : la meme
    /// piste peut occuper deux lignes de la file (journal Sandro du 01/09 :
    /// `Hold Me` ajoute deux fois, positions 4 puis 5), et ces deux lignes-la
    /// doivent rester distinctes.
    ///
    /// Sens de defaut : sans les DEUX identifiants on ne conclut rien. Un
    /// « perime » de trop desarmerait un enchainement valide, c'est-a-dire
    /// ferait payer un blanc a qui n'a rien demande.
    pub fn gapless_arm_outdated(armed_row_id: Option<i64>, row_id_at_next: Option<i64>) -> bool {
        match (armed_row_id, row_id_at_next) {
            (Some(arme), Some(suivant)) => arme != suivant,
            _ => false,
        }
    }

    /// The renderer-reported duration for the CURRENT track, sanitised against
    /// the queue-known (DB) duration.
    ///
    /// The renderer's own reported duration is normally authoritative and is
    /// trusted verbatim — a well-behaved renderer that reports a slightly (or
    /// even a few times) different value than the scanned duration is kept as-is
    /// on purpose. But some renderers report an *egregiously* wrong duration for
    /// the playing track — the HiFi Rose RS130 reports e.g. 17000 ms for a track
    /// that is really 174693 ms. Fed into the gapless-arming window that either
    /// armed SetNextAVTransportURI near t=0 (far too small) or never at all (far
    /// past the real end), cutting the album. Only when the reported value is
    /// off by more than 4x (or under a quarter) of a known DB duration — a gap
    /// no legitimate renderer/encoding difference produces — do we distrust it
    /// and use the DB duration. A `0` reported (LMS UPnP bridge) falls back to
    /// the DB as before; an unknown DB (0) means we can't judge, so keep the
    /// reported value.
    pub fn sane_current_duration(reported_ms: u64, db_ms: u64) -> u64 {
        let reported_is_egregious = db_ms > 0
            && reported_ms > 0
            && (reported_ms > db_ms.saturating_mul(4) || reported_ms < db_ms / 4);
        if reported_ms == 0 || reported_is_egregious {
            db_ms
        } else {
            reported_ms
        }
    }

    /// Position-based end-of-track: the output still reports Playing but the
    /// position has run past `duration + END_MARGIN_MS` (e.g. a local/cpal
    /// output draining its ring buffer). One tick's worth of the condition —
    /// the caller still requires `POSITION_PAST_END_TICKS` consecutive hits.
    pub fn past_end_reached(track_duration_ms: u64, played_enough: bool, position_ms: u64) -> bool {
        track_duration_ms > END_MARGIN_MS
            && played_enough
            && position_ms >= track_duration_ms.saturating_add(END_MARGIN_MS)
    }

    /// UN tick ou l'etat annonce se contredit lui-meme : la piste a une duree
    /// connue, et la position rapportee est arrivee a la fin — ou l'a depassee —
    /// alors que l'appareil dit toujours jouer (#2493, Tades : 1'46 « en
    /// lecture » depuis dix minutes sur une Serenade/upmpdcli).
    ///
    /// Ce predicat ne DECIDE rien : l'appelant s'en sert uniquement pour
    /// compter, puis pour le DIRE. Aucune piste n'est avancee ni arretee par ce
    /// chemin, et c'est delibere — la meme forme est produite par deux causes
    /// qu'aucune horloge ne sait distinguer :
    ///
    /// - la lecture est reellement bloquee (le renderer ment sur son etat) ;
    /// - la duree que Tune connait est FAUSSE (etiquette erronee, piste plus
    ///   longue que ce que le scan a mesure) et la lecture est parfaitement
    ///   valide.
    ///
    /// Couper dans le second cas amputerait une ecoute legitime. On se contente
    /// donc de cesser d'affirmer ce qu'on ne sait pas.
    ///
    /// Ce que le predicat refuse de qualifier :
    /// - `source_radio` : un flux de radio n'a pas de fin, sa position n'a rien
    ///   a depasser ;
    /// - `duree_connue_ms` nulle ou derisoire : sans duree il n'y a pas de
    ///   depassement possible — le seuil reprend celui de [`past_end_reached`]
    ///   ([`END_MARGIN_MS`]) pour que les deux parlent de la meme « fin ».
    ///
    /// La duree passee par l'appelant est la duree EFFECTIVE
    /// (`max(file d'attente, sane_current_duration)`) : une duree de base trop
    /// courte face a un renderer credible est deja elargie la, donc le cas « le
    /// scan a sous-estime la piste » ne remonte meme pas jusqu'ici.
    pub fn position_au_dela_de_la_duree(
        source_radio: bool,
        duree_connue_ms: u64,
        position_ms: u64,
    ) -> bool {
        !source_radio
            && duree_connue_ms > END_MARGIN_MS
            && position_ms.saturating_add(END_MARGIN_MS) >= duree_connue_ms
    }

    /// Position to persist for auto-resume. A position within `END_MARGIN_MS`
    /// of the track end means the track is effectively complete; persist 0 so a
    /// later auto-resume plays it from the start instead of seeking into the
    /// end zone — which, on an exclusive output, immediately trips the
    /// `reached_end_exclusive` past-end detector and (repeat=All) restarts the
    /// track at 0:00. Seen by DEvir on an ASIO Fireface with Tidal HI-RES whose
    /// real decoded duration (201.377 s) exceeds the rounded metadata (201.000 s),
    /// so the periodically-saved position (201215 ms) landed past `duration`.
    /// `duration_ms == 0` (unknown) persists the raw position unchanged.
    pub fn position_to_persist(position_ms: u64, duration_ms: u64) -> u64 {
        if duration_ms > 0 && position_ms.saturating_add(END_MARGIN_MS) >= duration_ms {
            0
        } else {
            position_ms
        }
    }

    /// Identity key of the currently playing track for the once-per-track
    /// scrobble latch (#1113).
    ///
    /// The latch used to be a plain boolean reset only when
    /// `track_generation` changed — but a gapless advance
    /// (`advance_queue_metadata`) deliberately does NOT bump the generation
    /// (the poller needs its cooldown intact), so every gapless-reached track
    /// (tracks 2, 4, 6… of an album) kept the latch stuck `true` and was never
    /// scrobbled. Keying the latch on the track's identity re-arms it on ANY
    /// path that changes what is playing — explicit play (generation bump),
    /// gapless metadata advance (queue position / track change), or the local
    /// output's internal chain — without per-call-site reset bookkeeping.
    ///
    /// `track_generation` still participates so repeat-one (same track, same
    /// queue position, new play) scrobbles each pass.
    pub fn scrobble_track_key(
        track_generation: u64,
        queue_position: i64,
        track_id: Option<i64>,
        title: &str,
        artist: Option<&str>,
    ) -> String {
        // Prefer the stable library id: mid-track metadata refinements
        // (cover/format updates) must not look like a new track.
        match track_id {
            Some(id) => format!("{track_generation}:{queue_position}:id={id}"),
            None => format!(
                "{track_generation}:{queue_position}:{title}\u{1f}{}",
                artist.unwrap_or_default()
            ),
        }
    }

    /// Should the poller dispatch a scrobble this tick?
    ///
    /// L'echeance de sondage d'une radio est-elle atteinte ?
    ///
    /// Ne prend PAS l'etat du transport en parametre, et c'est tout l'objet du
    /// correctif : le titre diffuse se lit sur une API externe ou dans le flux
    /// ICY, independamment de ce que fait le renderer. Seul le TEMPS ecoule
    /// commande — le tick du poller est a la seconde, l'API de la station non.
    pub fn radio_poll_due(since_last: std::time::Duration, interval_secs: u64) -> bool {
        since_last >= std::time::Duration::from_secs(interval_secs)
    }

    /// Faut-il rafraichir les metadonnees radio d'une zone qui n'a AUCUN
    /// peripherique de sortie ?
    ///
    /// Trois conditions, et la troisieme est celle qu'on oublie : la zone joue,
    /// la source est une radio, et l'etranglement est echu. Sans le dernier
    /// point on interrogerait l'API de la station a chaque tick du poller,
    /// c'est-a-dire toutes les secondes.
    ///
    /// Le cas `source != "radio"` n'est pas theorique : une zone navigateur qui
    /// joue un fichier local passe ici a chaque tick et ne doit declencher
    /// aucun appel reseau.
    pub fn deviceless_radio_refresh_due(
        is_playing: bool,
        source: Option<&str>,
        since_last_poll: std::time::Duration,
        interval_secs: u64,
    ) -> bool {
        is_playing && source == Some("radio") && radio_poll_due(since_last_poll, interval_secs)
    }

    /// Une lecture sans destination doit-elle cesser d'être annoncée ?
    ///
    /// Une zone sans périphérique de sortie prépare son flux, renonce à
    /// l'envoyer (`no_output_device_id_skipping_send_to_output`) et passe
    /// quand même en lecture : `orchestrator_play … output_sent=false`. Rien
    /// ne revenait ensuite sur cet état — la branche « pas de périphérique »
    /// du poller se termine par un `continue` — et la zone restait annoncée
    /// « en cours » indéfiniment, barre de progression comprise (#2630,
    /// journal de Pierre M).
    ///
    /// **Ce que ce prédicat ne regarde surtout pas : la présence d'un
    /// périphérique.** Une zone navigateur n'en a JAMAIS — sa sortie est
    /// l'onglet, qui tire `stream_url` lui-même. Une garde « pas de
    /// périphérique ⇒ pas de lecture » les rendrait toutes muettes ; c'est la
    /// régression exacte de `70401f2d`, réparée par #2657. Le seul fait
    /// observable est la CONSOMMATION du flux, et c'est la même preuve que
    /// `output_reach` utilise déjà pour dire `browser_unattended`.
    ///
    /// On n'abandonne donc que sur des faits POSITIFS :
    /// - le démarrage est daté (`last_play_started_at` est `#[serde(skip)]` :
    ///   après une restauration d'état il vaut `None`, et on ne conclut rien),
    ///   et il remonte à plus que [`DELAI_SILENCE_ETABLI`] ;
    /// - le streamer CONNAÎT ce flux et déclare `0` octet servi. `None`
    ///   (session inconnue, disparue, requête en échec) n'est pas une preuve
    ///   de silence : on laisse la lecture tranquille.
    ///
    /// Le sens du doute est celui de #2657 : on préfère taire un silence réel
    /// que d'en inventer un.
    pub fn lecture_sans_destination_abandonnee(
        depuis_le_demarrage: Option<std::time::Duration>,
        octets_servis: Option<u64>,
    ) -> bool {
        depuis_le_demarrage.is_some_and(|d| d >= super::DELAI_SILENCE_ETABLI)
            && octets_servis == Some(0)
    }

    /// L'autoplay doit-il chercher DANS LE SERVICE de la piste en cours ?
    ///
    /// Vrai des que l'ecoute vient d'un service de streaming. Le repli
    /// streaming existait deja (#1443) mais restait conditionne a « rien
    /// trouve en local » : chez qui possede une bibliotheque locale ET un
    /// abonnement, le generateur local repondait presque toujours quelque
    /// chose, et l'autoplay renvoyait donc des titres locaux au milieu d'une
    /// ecoute Qobuz (Sandro, 0.9.70).
    ///
    /// La source de la piste en cours est la meilleure expression de ce que
    /// l'auditeur ecoute : on la suit, et le local reste le filet.
    pub fn autoplay_prefers_streaming(source: Option<&str>) -> bool {
        matches!(source, Some(s) if !s.is_empty() && s != "local")
    }

    /// True when the playing track differs from the one already latched
    /// (`latched_key`) AND it has genuinely been listened past the Last.fm
    /// threshold (50% / 4 min, `should_scrobble`). Radio never scrobbles.
    pub fn should_dispatch_scrobble(
        latched_key: Option<&str>,
        current_key: &str,
        source: &str,
        duration_ms: i64,
        position_ms: i64,
    ) -> bool {
        source != "radio"
            && latched_key != Some(current_key)
            && crate::scrobble::should_scrobble(
                (duration_ms > 0).then_some(duration_ms),
                position_ms,
            )
    }

    /// Wall-clock end-of-track fallback for a DLNA renderer that reports no
    /// usable duration of its own (`reported_duration_ms == 0`) — the LMS UPnP
    /// bridge over a USB/Squeezebox DAC (Yacine/Jean-Pierre). Such a bridge
    /// never reports an advancing position past the end and never signals
    /// `ended_naturally`, so BOTH `past_end_reached` (needs a real position) and
    /// the Stopped-arm natural-end path stall — 0/196 auto-advances.
    ///
    /// When Tune knows the track length from the QUEUE (`queue_duration_ms`) and
    /// the wall clock (from `track_started_at`, folded on seek) says the whole
    /// track plus `END_MARGIN_MS` has elapsed while the renderer still claims to
    /// be Playing, the track has effectively ended. The caller still requires
    /// `POSITION_PAST_END_TICKS` consecutive hits (shared counter) so a single
    /// stray tick can't false-advance.
    ///
    /// Guards against regressing a well-behaved renderer:
    /// - `is_dlna`: only the DLNA output type (openhome/chromecast/bluos/local
    ///   keep their own paths).
    /// - `reported_duration_ms == 0`: a renderer that reports its own duration
    ///   uses the accurate position/duration path and never reaches here.
    /// - Only evaluated inside the `Playing` arm, so a Paused device is excluded.
    /// - The caller additionally gates on `!in_seek_grace`.
    ///
    /// It intentionally does NOT require the peak-position `played_enough` guard:
    /// the offending bridge freezes its reported position (often at 0), so a
    /// peak-based check would veto every real end. The wall clock — which only
    /// reaches `duration + margin` after that much real time has genuinely
    /// elapsed at 1x — is the sole reliable evidence here.
    pub fn wall_clock_past_end(
        is_dlna: bool,
        reported_duration_ms: u64,
        queue_duration_ms: u64,
        wall_elapsed_secs: u64,
    ) -> bool {
        is_dlna
            && reported_duration_ms == 0
            && queue_duration_ms > END_MARGIN_MS
            && wall_elapsed_secs.saturating_mul(1000)
                >= queue_duration_ms.saturating_add(END_MARGIN_MS)
    }

    /// Wall-clock end-of-track fallback for a **Chromecast** output.
    ///
    /// Cast tears down its media session the instant a track's byte stream ends
    /// and broadcasts the `idle_reason = FINISHED` transition only ONCE. The
    /// poller queries with a fresh-connect `GET_STATUS` every ~1 s and never
    /// listens for that broadcast, so it routinely misses the FINISHED window
    /// and then reads an EMPTY `entries` array — state=Stopped, position=0,
    /// `ended_naturally = false` — which the Stopped arm cannot distinguish from
    /// a mid-track blip. And if the receiver instead keeps claiming
    /// Playing/Buffering with its position frozen a little short of the known
    /// duration, the position paths (`reached_end_exclusive` needs position
    /// within 250 ms of duration, `past_end_reached` needs it *beyond*) never
    /// fire either. The album then stalls after track 1 on Chromecast while a
    /// DLNA renderer — which has BOTH this fallback and `poll_failed_past_end` —
    /// advances fine (Rhorn, Chromecast Audio, forum #1226; #648/#649 cured the
    /// 30-60 s stall but left this never-advances gap).
    ///
    /// Unlike the DLNA LMS-bridge fallback this KEEPS the `played_enough` (peak
    /// ≥ 80 %) guard: a Chromecast reports an honest advancing position while it
    /// plays, so the peak is trustworthy and gating on it means a genuine
    /// mid-track buffering stall (position frozen well before 80 %) can NOT
    /// false-advance. Fires only once Tune's own wall clock has passed the
    /// queue-known duration + margin, and the caller still requires
    /// `POSITION_PAST_END_TICKS` consecutive hits. A well-behaved Chromecast
    /// reaches the end via `reached_end_exclusive` a beat earlier, so this only
    /// takes over when the device's own end-of-track signal never lands.
    pub fn chromecast_wall_clock_past_end(
        output_type: &str,
        played_enough: bool,
        track_duration_ms: u64,
        wall_elapsed_secs: u64,
    ) -> bool {
        output_type == "chromecast"
            && played_enough
            && track_duration_ms > END_MARGIN_MS
            && wall_elapsed_secs.saturating_mul(1000)
                >= track_duration_ms.saturating_add(END_MARGIN_MS)
    }

    /// Fin de piste à l'horloge murale pour un renderer DLNA au poll SAIN qui
    /// gèle sa position dans la zone de fin sans jamais la dépasser ni passer
    /// STOPPED (Villerio, Eversolo DMP-A6, 25/08 : SetNext acquitté jamais
    /// honoré, PLAYING éternel, position figée exactement à la durée).
    /// `past_end_reached` exige la position AU-DELÀ de durée+marge,
    /// `wall_clock_past_end` exige une durée rapportée nulle, et
    /// `duration_changed` attend un changement qui ne vient pas : aucun ne
    /// couvre ce gel. Garde-fous : pic ≥ 80 % (le trajet jusqu'à la fin fut
    /// honnête), position ÉPINGLÉE en zone de fin (une vraie transition
    /// gapless la ramène près de zéro — pas de double-play), et l'horloge de
    /// Tune ayant réellement dépassé durée + marge à 1x. L'appelant exige
    /// toujours `POSITION_PAST_END_TICKS` coups consécutifs.
    pub fn dlna_frozen_at_end_wall_clock(
        is_dlna: bool,
        played_enough: bool,
        track_duration_ms: u64,
        position_ms: u64,
        wall_elapsed_secs: u64,
    ) -> bool {
        is_dlna
            && played_enough
            && track_duration_ms > END_MARGIN_MS
            && position_ms.saturating_add(2000) >= track_duration_ms
            && wall_elapsed_secs.saturating_mul(1000)
                >= track_duration_ms.saturating_add(END_MARGIN_MS)
    }

    /// Wall-clock end-of-track for a DLNA renderer whose status poll is FAILING
    /// outright — the LMS UPnP bridge's `GetPositionInfo` SOAP call errors, so
    /// `get_status` returns `Err` and Tune gets NO transport state, position, or
    /// duration from the renderer at all (Yacine/Jean-Pierre's Denafrips on
    /// Daphile: `soap_all_retries_failed action="GetPositionInfo"`).
    ///
    /// The decision is based purely on Tune's OWN wall clock versus the
    /// queue-known duration — it never touches renderer-reported values (there
    /// are none). Distinguishing "genuinely still playing" from "poll failing
    /// but the track really ended":
    /// - `tune_playing`: Tune's own intended state is `Playing` (NOT Paused or
    ///   Stopped). A user pause/stop through Tune flips this false, so a paused
    ///   track never advances. (`track_started_at` is set when the orchestrator
    ///   starts the track — generation change — so the wall clock counts real
    ///   elapsed play time, resetting on each track and on seek.)
    /// - `wall_elapsed_secs >= queue_duration + END_MARGIN_MS`: the whole track
    ///   plus margin has actually elapsed at 1x. Below that, still playing.
    /// - `consecutive_errors >= POLL_FAIL_END_MIN_ERRORS`: the poll is really
    ///   down, not a one-off blip.
    /// - `already_fired`: fire at most once per track (the caller sets a per-track
    ///   latch, cleared on track-generation change).
    ///
    /// A well-behaved DLNA renderer keeps answering `GetPositionInfo`, so its
    /// `consecutive_errors` stays 0 and this never triggers — no regression.
    pub fn poll_failed_past_end(
        is_dlna: bool,
        tune_playing: bool,
        queue_duration_ms: u64,
        wall_elapsed_secs: u64,
        consecutive_errors: u8,
        already_fired: bool,
    ) -> bool {
        is_dlna
            && tune_playing
            && !already_fired
            && consecutive_errors >= POLL_FAIL_END_MIN_ERRORS
            && queue_duration_ms > END_MARGIN_MS
            && wall_elapsed_secs.saturating_mul(1000)
                >= queue_duration_ms.saturating_add(END_MARGIN_MS)
    }
}

/// #2493 — Tades : « un morceau de Turapin (1'46) tourne depuis 10 minutes »
/// (HIFIMAN Serenade, pile UPnP `upmpdcli`, 26/08).
///
/// Ce que ces tests tiennent, et ce qu'ils refusent de tenir.
#[cfg(test)]
mod depassement_duree_tests;

/// Explicit poller state machine — **shadow model** of the `Stopped`-state
/// decision in `tick()` (v0.9 rc.2 step 2).
///
/// `classify_stopped` is a pure, exhaustive reproduction of the
/// `TransportState::Stopped` match arm's terminal decision. It composes the
/// `decisions` predicates and the poller thresholds. The caller supplies
/// I/O-derived facts (e.g. `stream_consuming`) as inputs so the function stays
/// pure and unit-testable.
///
/// Wiring plan: the live loop calls this in shadow mode behind the `poller_fsm`
/// flag and logs any divergence from the imperative arm before the FSM ever
/// becomes authoritative (per-zone flip). This is the seed that will replace
/// the 23-field `ZonePollState`.
pub mod fsm {
    use super::{
        GAPLESS_STUCK_THRESHOLD, POSITION_PAST_END_TICKS, STOPPED_FAILURE_THRESHOLD,
        STOPPED_TICKS_THRESHOLD, decisions,
    };

    /// Ce que le sondeur SAIT du flux d'une zone, au moment où la garde
    /// d'échec (#2394) décide de couper ou d'attendre.
    ///
    /// ── Pourquoi trois états et pas un compteur ──
    ///
    /// La garde lisait un `Option<u64>` et l'écrasait en `0` : `stream_id`
    /// absent → `0`, session inconnue du gestionnaire de flux →
    /// `unwrap_or(0)`. « Zéro octet servi » et « je ne sais pas » étaient donc
    /// LE MÊME CHIFFRE — et c'est ce chiffre qui arme `force_stop`. Une zone
    /// qui joue parfaitement mais dont le sondeur ignore le `stream_id` était
    /// indiscernable d'une zone à sec, et coupée au bout de trente secondes
    /// (DMP-A8, machine `.18`, #2394).
    ///
    /// L'échappatoire « le renderer joue mais n'annonce pas son état »
    /// (DMP-A10, LHC, Shanling…) était précisément désarmée par cette
    /// confusion : elle exige `octets > 0`, ce qu'un `0` d'ignorance ne donne
    /// jamais.
    ///
    /// Le `stream_id` manque pour des raisons ORDINAIRES, pas seulement en cas
    /// de panne : `advance_queue_metadata` — l'avance gapless, que le sondeur
    /// s'appelle à lui-même — pose `stream_id: None` dans le now-playing, et
    /// la reprise depuis la base en pose un aussi (cf. `stream_id_de_l_uri`,
    /// #2991 : la base ne mémorise pas un identifiant de session). Une lecture
    /// gapless sur un renderer DLNA passe donc structurellement en « inconnu »
    /// dès la deuxième piste de l'album.
    ///
    /// Vit ici, dans `fsm`, et non dans `decisions` : c'est le type d'un champ
    /// de [`StoppedInput`], et `decisions` est `pub(crate)` — un champ public
    /// portant un type privé ne serait ni nommable par une épreuve
    /// d'intégration, ni propre au regard de `private_interfaces`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ConsommationFlux {
        /// Le compteur est connu ET il avance : le renderer tire des octets.
        Consomme,
        /// Le compteur est connu et n'avance pas : la zone est réellement à
        /// sec. C'est le SEUL état qui autorise la coupure.
        ASec,
        /// Personne ne sait : pas de `stream_id`, ou session inconnue du
        /// gestionnaire de flux. On n'a mesuré RIEN — pas « rien servi ».
        Inconnue,
    }

    impl ConsommationFlux {
        /// Étiquette stable pour le journal et le diagnostic. Un état qu'on ne
        /// peut pas observer se reconfond avec zéro à la première occasion :
        /// celui-ci se lit dans `journalctl -u tune-server | grep consommation`.
        pub fn etiquette(self) -> &'static str {
            match self {
                ConsommationFlux::Consomme => "consomme",
                ConsommationFlux::ASec => "a_sec",
                ConsommationFlux::Inconnue => "inconnue",
            }
        }

        /// Le compteur a-t-il été MESURÉ ? Seul un compteur mesuré autorise la
        /// coupure de la zone et le verdict « démarrage mort ».
        pub fn est_mesuree(self) -> bool {
            !matches!(self, ConsommationFlux::Inconnue)
        }
    }

    /// Qualifier la consommation d'un flux à partir d'un compteur qui a le
    /// droit de ne pas savoir.
    ///
    /// `octets_servis` vient de `streamer_bytes_sent`, qui rend `None` pour un
    /// flux qu'il ne connaît pas ; l'appelant rend `None` aussi quand le
    /// now-playing n'a pas de `stream_id`. Les deux ignorances se valent et
    /// donnent [`ConsommationFlux::Inconnue`] — jamais `ASec`.
    ///
    /// `octets_precedents` est le dernier compte MESURÉ : un tour « inconnu »
    /// ne le remplace pas, sinon la reprise du `stream_id` relancerait la
    /// comparaison depuis un faux zéro et un flux stable passerait pour
    /// consommant.
    pub fn consommation_flux(
        octets_servis: Option<u64>,
        octets_precedents: u64,
    ) -> ConsommationFlux {
        match octets_servis {
            None => ConsommationFlux::Inconnue,
            Some(octets) if octets > 0 && octets > octets_precedents => ConsommationFlux::Consomme,
            Some(_) => ConsommationFlux::ASec,
        }
    }

    /// Terminal decision of one poll tick when the output reports Stopped.
    /// Each variant maps 1:1 to a branch of the `TransportState::Stopped` arm.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum StoppedOutcome {
        /// Tune is not playing on this zone — device Stopped is ignored.
        Ignore,
        /// Suppressed by the seek grace window.
        SuppressSeekGrace,
        /// Suppressed by the track-load grace window.
        SuppressLoadGrace,
        /// Suppressed by the post-gapless cooldown.
        SuppressCooldown,
        /// In the gapless guard but not enough played — ignore (false-skip guard).
        GuardStoppedIgnored,
        /// In the gapless guard, enough played — arm pending confirmation.
        GuardStoppedPending,
        /// Advance pending, renderer still stuck below threshold — keep waiting.
        StuckWaiting,
        /// Advance pending + stuck threshold reached — force track end.
        StuckForceEnd,
        /// Local output signalled natural EOF — track ended.
        LocalEndedNaturally,
        /// DSD-on-DLNA (gapless intentionally off) reached its end by peak
        /// position — advance now instead of waiting out the Stopped counter.
        DsdDlnaReachedEnd,
        /// Stopped-threshold reached + natural end, gapless armed — wait for transition.
        NaturalEndGaplessWaiting,
        /// Stopped-threshold reached + natural end, no gapless — advance track.
        NaturalEndAdvance,
        /// Failure threshold reached but the stream is still consuming — keep waiting.
        FailureWaitingConsuming,
        /// Seuil d'échec atteint, mais la consommation du flux est INCONNUE
        /// (pas de `stream_id`, ou session inconnue du gestionnaire de flux) —
        /// on attend (#2394). Couper une zone parce qu'on ne sait pas la
        /// mesurer est pire que le défaut qu'on croit prévenir.
        FailureWaitingUnknown,
        /// Failure threshold reached, stream idle — stop the zone.
        FailureStop,
        /// Below threshold, or above threshold without a natural end — accumulate.
        Waiting,
    }

    impl StoppedOutcome {
        /// Does this outcome conclude the track has ended (loop sets `track_ended`)?
        pub fn is_track_end(self) -> bool {
            matches!(
                self,
                StoppedOutcome::StuckForceEnd
                    | StoppedOutcome::LocalEndedNaturally
                    | StoppedOutcome::DsdDlnaReachedEnd
                    | StoppedOutcome::NaturalEndAdvance
            )
        }

        /// Does this outcome stop the zone (loop sets `force_stop`)?
        pub fn is_force_stop(self) -> bool {
            matches!(self, StoppedOutcome::FailureStop)
        }
    }

    /// Snapshot of the inputs the Stopped arm reads, taken BEFORE the arm
    /// mutates `ZonePollState`. Counters are pre-increment (the classifier
    /// applies the `+1` the arm would).
    #[derive(Debug, Clone, Copy)]
    pub struct StoppedInput {
        pub tune_is_playing: bool,
        pub tune_has_track: bool,
        pub in_seek_grace: bool,
        pub in_track_load_grace: bool,
        pub gapless_cooldown: u8,
        pub in_gapless_guard: bool,
        pub played_enough: bool,
        pub gapless_advance_pending: bool,
        pub gapless_stuck_ticks: u8,
        pub ended_naturally: bool,
        pub wall_elapsed: u64,
        pub track_duration_ms: u64,
        pub stopped_ticks: u8,
        pub natural_end: bool,
        pub gapless_sent: bool,
        /// `OutputStatus::realtime` — `false` for an output that finishes a
        /// track faster than 1x (a recorder), which exempts it from the
        /// wall-clock plausibility guard on `ended_naturally`.
        pub realtime: bool,
        /// Whether the output can transition internally (live probe). For an
        /// exclusive local output or the OAAT direct-file loop, `gapless_sent`
        /// is only a re-arm suppressor — no internal transition ever comes, so
        /// the natural end must advance instead of waiting (the actual branch
        /// already probes this; without it the shadow predicted
        /// NaturalEndGaplessWaiting on every OAAT direct-path track end).
        pub can_internal_gapless: bool,
        /// Ce que le sondeur SAIT du flux — trois états, pas un compteur
        /// (#2394). `Inconnue` ne coupe pas.
        pub consommation: ConsommationFlux,
        /// Precomputed `decisions::dlna_dsd_reached_end` for this zone/track — a
        /// DSD track on a DLNA renderer whose peak position reached the end.
        /// Gapless is intentionally off for a DSD next on DLNA, and DLNA never
        /// reports `ended_naturally`, so without this the track only ends after
        /// `STOPPED_TICKS_THRESHOLD` polls (~5s gap).
        pub dlna_dsd_reached_end: bool,
    }

    /// Pure reproduction of the `TransportState::Stopped` arm's decision tree.
    /// Branch order is significant and mirrors `tick()` exactly.
    pub fn classify_stopped(i: &StoppedInput) -> StoppedOutcome {
        use StoppedOutcome::*;
        if !i.tune_is_playing || !i.tune_has_track {
            return Ignore;
        }
        if i.in_seek_grace {
            return SuppressSeekGrace;
        }
        if i.in_track_load_grace {
            return SuppressLoadGrace;
        }
        if i.gapless_cooldown > 0 {
            return SuppressCooldown;
        }
        if i.in_gapless_guard {
            return if !i.played_enough {
                GuardStoppedIgnored
            } else {
                GuardStoppedPending
            };
        }
        if i.gapless_advance_pending {
            return if i.gapless_stuck_ticks.saturating_add(1) >= GAPLESS_STUCK_THRESHOLD {
                StuckForceEnd
            } else {
                StuckWaiting
            };
        }
        if i.ended_naturally
            && (i.played_enough
                || !i.realtime
                || decisions::ended_naturally_wall_ok(i.wall_elapsed, i.track_duration_ms))
        {
            return LocalEndedNaturally;
        }
        if i.dlna_dsd_reached_end {
            // DSD-on-DLNA: gapless intentionally off and DLNA never reports
            // ended_naturally, so the peak reaching the end is the earliest
            // reliable end-of-track signal — advance now, ~4s before the
            // Stopped counter would.
            return DsdDlnaReachedEnd;
        }
        // Fallthrough: the arm increments stopped_ticks, then branches on it.
        let stopped_ticks = i.stopped_ticks.saturating_add(1);
        if stopped_ticks >= STOPPED_TICKS_THRESHOLD {
            if i.natural_end {
                return if i.gapless_sent && i.can_internal_gapless {
                    NaturalEndGaplessWaiting
                } else {
                    NaturalEndAdvance
                };
            }
            if stopped_ticks >= STOPPED_FAILURE_THRESHOLD {
                return match i.consommation {
                    ConsommationFlux::Consomme => FailureWaitingConsuming,
                    ConsommationFlux::Inconnue => FailureWaitingUnknown,
                    ConsommationFlux::ASec => FailureStop,
                };
            }
            return Waiting;
        }
        Waiting
    }

    /// Decisions taken by the `Playing`/`Transitioning` arm. Unlike the Stopped
    /// arm (a single-outcome tree), the Playing arm performs a *sequence* of
    /// independent effects, so this is a bundle of flags, not one enum.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct PlayingDecision {
        /// (A) A gapless advance was pending and a next track exists — advance
        /// the queue metadata now.
        pub confirm_gapless_advance: bool,
        /// (B) A gapless transition to the next track was detected.
        pub transition_detected: bool,
        /// (C) Entered the final window, not armed, gapless enabled — arm SetNext.
        pub arm_gapless: bool,
        /// (D) Position ran past the end for POSITION_PAST_END_TICKS ticks —
        /// the arm sets track_ended.
        pub past_end_track_ended: bool,
    }

    /// Inputs read by the Playing arm, snapshot pre-mutation. `has_next` and
    /// `gapless_enabled` are supplied by the caller (queue lookup / zone config).
    #[derive(Debug, Clone, Copy)]
    pub struct PlayingInput {
        pub gapless_advance_pending: bool,
        pub has_next: bool,
        pub gapless_sent: bool,
        pub track_duration_ms: u64,
        pub reported_duration_ms: u64,
        pub played_enough: bool,
        pub position_ms: u64,
        pub past_end_ticks: u8,
        pub gapless_enabled: bool,
        /// The zone's output is a DLNA renderer — enables the wall-clock
        /// end-of-track fallback for renderers reporting no duration.
        pub is_dlna: bool,
        /// Seconds elapsed since `track_started_at` (folded on seek).
        pub wall_elapsed_secs: u64,
    }

    /// Pure reproduction of the `Playing`/`Transitioning` arm's decisions.
    /// Mirrors the arm's ordering: a detected transition (B) resets the
    /// past-end tick counter before (D) is evaluated.
    pub fn classify_playing(i: &PlayingInput) -> PlayingDecision {
        let confirm_gapless_advance = i.gapless_advance_pending && i.has_next;
        let transition_detected = decisions::duration_changed(
            i.gapless_sent,
            i.track_duration_ms,
            i.reported_duration_ms,
        ) && decisions::position_confirms_transition(
            i.played_enough,
            i.position_ms,
            i.track_duration_ms,
        );
        let arm_gapless = !transition_detected
            && decisions::should_arm_gapless(
                i.gapless_sent,
                i.reported_duration_ms,
                i.track_duration_ms,
                i.position_ms,
            )
            && i.gapless_enabled;
        // (B) resets past_end_ticks to 0 before (D) runs.
        let effective_past_end_ticks = if transition_detected {
            0
        } else {
            i.past_end_ticks
        };
        // (D) is reached either by a real position running past the end, or —
        // for a DLNA bridge that reports no position/duration — by the wall
        // clock passing the queue-known duration.
        let reached_end =
            decisions::past_end_reached(i.track_duration_ms, i.played_enough, i.position_ms)
                || decisions::wall_clock_past_end(
                    i.is_dlna,
                    i.reported_duration_ms,
                    i.track_duration_ms,
                    i.wall_elapsed_secs,
                )
                || decisions::dlna_frozen_at_end_wall_clock(
                    i.is_dlna,
                    i.played_enough,
                    i.track_duration_ms,
                    i.position_ms,
                    i.wall_elapsed_secs,
                );
        let past_end_track_ended =
            reached_end && effective_past_end_ticks.saturating_add(1) >= POSITION_PAST_END_TICKS;
        PlayingDecision {
            confirm_gapless_advance,
            transition_detected,
            arm_gapless,
            past_end_track_ended,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn base() -> StoppedInput {
            StoppedInput {
                tune_is_playing: true,
                tune_has_track: true,
                in_seek_grace: false,
                in_track_load_grace: false,
                gapless_cooldown: 0,
                in_gapless_guard: false,
                played_enough: false,
                gapless_advance_pending: false,
                gapless_stuck_ticks: 0,
                ended_naturally: false,
                realtime: true,
                wall_elapsed: 0,
                track_duration_ms: 0,
                stopped_ticks: 0,
                natural_end: false,
                gapless_sent: false,
                can_internal_gapless: true,
                // Base : compteur MESURÉ et à sec — c'est ce qui doit couper.
                consommation: ConsommationFlux::ASec,
                dlna_dsd_reached_end: false,
            }
        }

        #[test]
        fn volume_not_adopted_on_first_observation() {
            // No previous reading yet — never overwrite on the first poll.
            assert!(!super::super::decisions::should_adopt_device_volume(
                None, 0.5, 0.3
            ));
        }

        #[test]
        fn volume_not_adopted_when_device_reports_stale_default() {
            // Devialet keeps reporting 0.50 while the user saved 0.30 — the
            // value never moves, so the saved volume must be preserved (Fabien).
            assert!(!super::super::decisions::should_adopt_device_volume(
                Some(0.5),
                0.5,
                0.3
            ));
        }

        #[test]
        fn volume_adopted_on_real_device_change() {
            // The knob moved on the device (0.50 -> 0.62) and now differs from
            // the saved volume — adopt it.
            assert!(super::super::decisions::should_adopt_device_volume(
                Some(0.5),
                0.62,
                0.3
            ));
        }

        #[test]
        fn volume_not_adopted_when_change_matches_saved() {
            // Device moved but landed on what we already have stored.
            assert!(!super::super::decisions::should_adopt_device_volume(
                Some(0.5),
                0.62,
                0.62
            ));
        }

        #[test]
        fn ignore_when_tune_not_playing() {
            assert_eq!(
                classify_stopped(&StoppedInput {
                    tune_is_playing: false,
                    ..base()
                }),
                StoppedOutcome::Ignore
            );
            assert_eq!(
                classify_stopped(&StoppedInput {
                    tune_has_track: false,
                    ..base()
                }),
                StoppedOutcome::Ignore
            );
        }

        #[test]
        fn natural_end_advances_when_output_cannot_chain_internally() {
            // gapless_sent posé par le chemin « skip » (sortie exclusive ou
            // boucle directe OAAT) : pas de transition interne possible — la
            // fin naturelle doit avancer, pas attendre (divergence shadow-FSM
            // observée à chaque fin de piste locale OAAT, 29/07).
            let i = StoppedInput {
                natural_end: true,
                gapless_sent: true,
                can_internal_gapless: false,
                stopped_ticks: STOPPED_TICKS_THRESHOLD,
                ..base()
            };
            assert_eq!(classify_stopped(&i), StoppedOutcome::NaturalEndAdvance);

            // Avec transition interne possible, le comportement historique reste.
            let i2 = StoppedInput {
                can_internal_gapless: true,
                ..i
            };
            assert_eq!(
                classify_stopped(&i2),
                StoppedOutcome::NaturalEndGaplessWaiting
            );
        }

        #[test]
        fn grace_windows_suppress() {
            assert_eq!(
                classify_stopped(&StoppedInput {
                    in_seek_grace: true,
                    ..base()
                }),
                StoppedOutcome::SuppressSeekGrace
            );
            assert_eq!(
                classify_stopped(&StoppedInput {
                    in_track_load_grace: true,
                    ..base()
                }),
                StoppedOutcome::SuppressLoadGrace
            );
            assert_eq!(
                classify_stopped(&StoppedInput {
                    gapless_cooldown: 3,
                    ..base()
                }),
                StoppedOutcome::SuppressCooldown
            );
        }

        #[test]
        fn seek_grace_beats_load_grace() {
            let i = StoppedInput {
                in_seek_grace: true,
                in_track_load_grace: true,
                ..base()
            };
            assert_eq!(classify_stopped(&i), StoppedOutcome::SuppressSeekGrace);
        }

        #[test]
        fn gapless_guard_branches_on_played_enough() {
            assert_eq!(
                classify_stopped(&StoppedInput {
                    in_gapless_guard: true,
                    played_enough: false,
                    ..base()
                }),
                StoppedOutcome::GuardStoppedIgnored
            );
            assert_eq!(
                classify_stopped(&StoppedInput {
                    in_gapless_guard: true,
                    played_enough: true,
                    ..base()
                }),
                StoppedOutcome::GuardStoppedPending
            );
        }

        #[test]
        fn stuck_waits_then_forces_end() {
            // GAPLESS_STUCK_THRESHOLD = 2. pre=0 → +1=1 < 2 → wait.
            assert_eq!(
                classify_stopped(&StoppedInput {
                    gapless_advance_pending: true,
                    gapless_stuck_ticks: 0,
                    ..base()
                }),
                StoppedOutcome::StuckWaiting
            );
            // pre=1 → +1=2 >= 2 → force end.
            assert_eq!(
                classify_stopped(&StoppedInput {
                    gapless_advance_pending: true,
                    gapless_stuck_ticks: 1,
                    ..base()
                }),
                StoppedOutcome::StuckForceEnd
            );
        }

        #[test]
        fn local_ended_naturally_paths() {
            // played_enough qualifies.
            assert_eq!(
                classify_stopped(&StoppedInput {
                    ended_naturally: true,
                    played_enough: true,
                    ..base()
                }),
                StoppedOutcome::LocalEndedNaturally
            );
            // wall_elapsed >= 5 also qualifies.
            assert_eq!(
                classify_stopped(&StoppedInput {
                    ended_naturally: true,
                    wall_elapsed: 5,
                    ..base()
                }),
                StoppedOutcome::LocalEndedNaturally
            );
            // ended_naturally but too early and not played_enough → falls through.
            assert_ne!(
                classify_stopped(&StoppedInput {
                    ended_naturally: true,
                    wall_elapsed: 4,
                    ..base()
                }),
                StoppedOutcome::LocalEndedNaturally
            );
        }

        /// The recorder case, mirroring
        /// `natural_end_non_realtime_output_skips_the_wall_guard`: a
        /// `realtime: false` output that says the track is done is believed
        /// immediately, at the same inputs the DMP-A8 guard rejects.
        #[test]
        fn non_realtime_ended_naturally_is_immediate() {
            assert_eq!(
                classify_stopped(&StoppedInput {
                    ended_naturally: true,
                    played_enough: false,
                    wall_elapsed: 1,
                    track_duration_ms: 300_000,
                    realtime: false,
                    ..base()
                }),
                StoppedOutcome::LocalEndedNaturally
            );
            // Identical inputs from a renderer: still rejected.
            assert_ne!(
                classify_stopped(&StoppedInput {
                    ended_naturally: true,
                    played_enough: false,
                    wall_elapsed: 1,
                    track_duration_ms: 300_000,
                    realtime: true,
                    ..base()
                }),
                StoppedOutcome::LocalEndedNaturally
            );
        }

        #[test]
        fn dmp_a8_false_ended_naturally_rejected() {
            // DMP-A8 regression: renderer falsely reports ended_naturally ~35s
            // into a 4-minute (240s) track (played_enough false). Wall-clock is
            // far below MIN_WALL_FRACTION_FOR_NATURAL_END·duration → NOT an end.
            assert_ne!(
                classify_stopped(&StoppedInput {
                    ended_naturally: true,
                    played_enough: false,
                    wall_elapsed: 35,
                    track_duration_ms: 240_000,
                    ..base()
                }),
                StoppedOutcome::LocalEndedNaturally
            );
            // A genuine end near the track duration is still trusted.
            assert_eq!(
                classify_stopped(&StoppedInput {
                    ended_naturally: true,
                    played_enough: false,
                    wall_elapsed: 200,
                    track_duration_ms: 240_000,
                    ..base()
                }),
                StoppedOutcome::LocalEndedNaturally
            );
        }

        #[test]
        fn below_threshold_waits() {
            // STOPPED_TICKS_THRESHOLD = 5. pre=0 → +1=1 < 5 → waiting.
            assert_eq!(classify_stopped(&base()), StoppedOutcome::Waiting);
        }

        #[test]
        fn natural_end_advances_without_gapless() {
            let i = StoppedInput {
                stopped_ticks: 4,
                natural_end: true,
                gapless_sent: false,
                ..base()
            };
            assert_eq!(classify_stopped(&i), StoppedOutcome::NaturalEndAdvance);
            assert!(classify_stopped(&i).is_track_end());
        }

        #[test]
        fn natural_end_waits_when_gapless_armed() {
            let i = StoppedInput {
                stopped_ticks: 4,
                natural_end: true,
                gapless_sent: true,
                ..base()
            };
            assert_eq!(
                classify_stopped(&i),
                StoppedOutcome::NaturalEndGaplessWaiting
            );
            assert!(!classify_stopped(&i).is_track_end());
        }

        #[test]
        fn dsd_dlna_reached_end_advances_before_stopped_counter() {
            // DSD on DLNA reached its end (peak): advance now, at stopped_ticks=0,
            // without waiting out STOPPED_TICKS_THRESHOLD. This keeps the FSM
            // shadow model in sync with the imperative arm's fast path.
            let i = StoppedInput {
                stopped_ticks: 0,
                dlna_dsd_reached_end: true,
                ..base()
            };
            assert_eq!(classify_stopped(&i), StoppedOutcome::DsdDlnaReachedEnd);
            assert!(classify_stopped(&i).is_track_end());
        }

        #[test]
        fn dsd_dlna_end_yields_to_ended_naturally() {
            // A local-style ended_naturally still wins (branch order preserved).
            let i = StoppedInput {
                dlna_dsd_reached_end: true,
                ended_naturally: true,
                played_enough: true,
                ..base()
            };
            assert_eq!(classify_stopped(&i), StoppedOutcome::LocalEndedNaturally);
        }

        // #2394 — ces deux épreuves épinglaient le booléen `stream_consuming`,
        // qui ne connaissait que deux états. Elles ne sont pas supprimées :
        // elles disent MAINTENANT la même chose sur le troisième état près,
        // `ConsommationFlux`, et `octets_servis_inconnus_2394.rs` ajoute
        // l'épreuve du cas « inconnue » qu'un booléen ne pouvait pas exprimer.
        #[test]
        fn failure_stops_when_idle_past_failure_threshold() {
            // STOPPED_FAILURE_THRESHOLD = 30. pre=29 → +1=30, not natural, idle.
            // « À sec » = compteur MESURÉ qui n'avance pas : ça coupe toujours.
            let i = StoppedInput {
                stopped_ticks: 29,
                natural_end: false,
                consommation: ConsommationFlux::ASec,
                ..base()
            };
            assert_eq!(classify_stopped(&i), StoppedOutcome::FailureStop);
            assert!(classify_stopped(&i).is_force_stop());
        }

        #[test]
        fn failure_waits_when_stream_consuming() {
            let i = StoppedInput {
                stopped_ticks: 29,
                natural_end: false,
                consommation: ConsommationFlux::Consomme,
                ..base()
            };
            assert_eq!(
                classify_stopped(&i),
                StoppedOutcome::FailureWaitingConsuming
            );
            assert!(!classify_stopped(&i).is_force_stop());
        }

        /// #2394 — le cas que le booléen ne savait pas dire : consommation
        /// INCONNUE au seuil d'échec. La zone attend, elle n'est pas coupée.
        #[test]
        fn failure_waits_when_consumption_unknown() {
            let i = StoppedInput {
                stopped_ticks: 29,
                natural_end: false,
                consommation: ConsommationFlux::Inconnue,
                ..base()
            };
            assert_eq!(classify_stopped(&i), StoppedOutcome::FailureWaitingUnknown);
            assert!(!classify_stopped(&i).is_force_stop());
        }

        #[test]
        fn between_thresholds_waits() {
            // pre=10 → +1=11, >=5 but <30, not natural_end → Waiting.
            let i = StoppedInput {
                stopped_ticks: 10,
                natural_end: false,
                ..base()
            };
            assert_eq!(classify_stopped(&i), StoppedOutcome::Waiting);
        }

        fn pbase() -> PlayingInput {
            PlayingInput {
                gapless_advance_pending: false,
                has_next: true,
                gapless_sent: false,
                track_duration_ms: 300_000,
                reported_duration_ms: 300_000,
                played_enough: false,
                position_ms: 0,
                past_end_ticks: 0,
                gapless_enabled: true,
                is_dlna: false,
                wall_elapsed_secs: 0,
            }
        }

        #[test]
        fn playing_confirm_gapless_advance() {
            assert!(
                classify_playing(&PlayingInput {
                    gapless_advance_pending: true,
                    has_next: true,
                    ..pbase()
                })
                .confirm_gapless_advance
            );
            // pending but no next → no metadata advance.
            assert!(
                !classify_playing(&PlayingInput {
                    gapless_advance_pending: true,
                    has_next: false,
                    ..pbase()
                })
                .confirm_gapless_advance
            );
        }

        #[test]
        fn playing_transition_detected_requires_armed() {
            let armed = PlayingInput {
                gapless_sent: true,
                track_duration_ms: 200_000,
                reported_duration_ms: 210_000,
                played_enough: true,
                position_ms: 2_000,
                ..pbase()
            };
            assert!(classify_playing(&armed).transition_detected);
            // Not armed → duration_changed is false → no transition.
            assert!(
                !classify_playing(&PlayingInput {
                    gapless_sent: false,
                    ..armed
                })
                .transition_detected
            );
        }

        #[test]
        fn playing_arm_gapless_gated_by_enabled_and_not_transitioning() {
            let i = PlayingInput {
                gapless_sent: false,
                reported_duration_ms: 300_000,
                position_ms: 275_000,
                gapless_enabled: true,
                ..pbase()
            };
            let d = classify_playing(&i);
            assert!(d.arm_gapless && !d.transition_detected);
            // Disabled for the zone → don't arm.
            assert!(
                !classify_playing(&PlayingInput {
                    gapless_enabled: false,
                    ..i
                })
                .arm_gapless
            );
        }

        #[test]
        fn playing_past_end_advances_after_threshold() {
            // POSITION_PAST_END_TICKS = 3. pre=2 → +1=3 >= 3, past end reached.
            let i = PlayingInput {
                track_duration_ms: 240_000,
                played_enough: true,
                position_ms: 244_000,
                past_end_ticks: 2,
                ..pbase()
            };
            assert!(classify_playing(&i).past_end_track_ended);
            // pre=1 → +1=2 < 3 → not yet.
            assert!(
                !classify_playing(&PlayingInput {
                    past_end_ticks: 1,
                    ..i
                })
                .past_end_track_ended
            );
        }

        #[test]
        fn un_dmp_fige_a_la_duree_avance_par_horloge_murale() {
            // Villerio, DMP-A6, 25/08 22:24 : SetNext acquitté mais jamais
            // honoré ; le renderer reste PLAYING, position figée EXACTEMENT à
            // la durée (373000 — les stale_start_position du journal), durée
            // rapportée inchangée. past_end_reached exige position > durée+3s,
            // wall_clock_past_end exige durée rapportée nulle : aucun ne tire,
            // la piste ne finit jamais. L'horloge murale de Tune, elle, sait.
            let i = PlayingInput {
                track_duration_ms: 373_000,
                reported_duration_ms: 373_000,
                played_enough: true,
                position_ms: 373_000,
                past_end_ticks: 2,
                gapless_sent: true,
                is_dlna: true,
                wall_elapsed_secs: 380,
                ..pbase()
            };
            assert!(classify_playing(&i).past_end_track_ended);

            // Une VRAIE transition gapless à durées voisines remet la position
            // près de zéro : la position n'est plus épinglée en zone de fin,
            // le filet ne doit pas tirer (sinon double-play).
            assert!(
                !classify_playing(&PlayingInput {
                    position_ms: 3_000,
                    ..i
                })
                .past_end_track_ended
            );
            // Gel en PLEIN MILIEU de piste (vrai blocage réseau) : pic < 80 %,
            // on ne conclut pas une fin.
            assert!(
                !classify_playing(&PlayingInput {
                    played_enough: false,
                    position_ms: 180_000,
                    wall_elapsed_secs: 380,
                    ..i
                })
                .past_end_track_ended
            );
            // L'horloge n'a pas encore dépassé durée + marge : on attend.
            assert!(
                !classify_playing(&PlayingInput {
                    wall_elapsed_secs: 370,
                    ..i
                })
                .past_end_track_ended
            );
        }

        #[test]
        fn playing_transition_resets_past_end_counter() {
            // Past-end IS reached, but a detected transition resets the counter
            // to 0 before (D), so no past-end advance this tick.
            let i = PlayingInput {
                gapless_sent: true,
                track_duration_ms: 240_000,
                reported_duration_ms: 250_000,
                played_enough: true,
                position_ms: 244_000,
                past_end_ticks: 5,
                ..pbase()
            };
            let d = classify_playing(&i);
            assert!(d.transition_detected);
            assert!(!d.past_end_track_ended);
            // Without a transition, the pre-tick counter stands: 5+1 >= 3 → advance.
            let d2 = classify_playing(&PlayingInput {
                gapless_sent: false,
                ..i
            });
            assert!(!d2.transition_detected);
            assert!(d2.past_end_track_ended);
        }

        #[test]
        fn playing_dlna_wall_clock_past_end_advances() {
            // LMS UPnP bridge: renderer reports duration 0 and a frozen
            // position, but Tune knows the queue duration (300s) and the wall
            // clock has passed duration + margin. POSITION_PAST_END_TICKS = 3,
            // pre=2 → +1=3 → advance. played_enough is false (peak frozen), which
            // must NOT block the wall-clock fallback.
            let i = PlayingInput {
                is_dlna: true,
                reported_duration_ms: 0,
                track_duration_ms: 300_000,
                position_ms: 0,
                played_enough: false,
                wall_elapsed_secs: 304,
                past_end_ticks: 2,
                ..pbase()
            };
            assert!(classify_playing(&i).past_end_track_ended);
        }

        #[test]
        fn playing_dlna_wall_clock_negatives() {
            let armed = PlayingInput {
                is_dlna: true,
                reported_duration_ms: 0,
                track_duration_ms: 300_000,
                position_ms: 0,
                played_enough: false,
                wall_elapsed_secs: 304,
                past_end_ticks: 2,
                ..pbase()
            };
            // Queue duration unknown (0) → no wall-clock advance.
            assert!(
                !classify_playing(&PlayingInput {
                    track_duration_ms: 0,
                    ..armed
                })
                .past_end_track_ended
            );
            // Not enough wall time elapsed (< duration + margin) → no advance.
            assert!(
                !classify_playing(&PlayingInput {
                    wall_elapsed_secs: 120,
                    ..armed
                })
                .past_end_track_ended
            );
            // Not a DLNA renderer → fallback disabled entirely.
            assert!(
                !classify_playing(&PlayingInput {
                    is_dlna: false,
                    ..armed
                })
                .past_end_track_ended
            );
            // Renderer reports its own duration → uses the accurate path, the
            // wall-clock fallback is disabled (no regression for good renderers).
            assert!(
                !classify_playing(&PlayingInput {
                    reported_duration_ms: 300_000,
                    ..armed
                })
                .past_end_track_ended
            );
        }
    }
}

/// Backoff des sondages sur une zone **arrêtée**.
///
/// Le chemin « zone en lecture » recule déjà après un échec
/// (`ZonePollState::backoff_remaining`), mais celui des zones arrêtées — qui
/// sert à détecter une lecture démarrée hors de Tune — faisait `continue` sans
/// rien mémoriser : un appareil lent ou injoignable était donc re-sondé chaque
/// seconde, indéfiniment. Or `get_status_bounded` abandonne au bout de 5 s
/// pendant que la requête SOAP dessous garde son propre timeout de 10 s et ses
/// deux réessais — les appels s'empilaient sur un renderer qui les traite un par
/// un, jusqu'à ce qu'il ne réponde plus à rien, commande de lecture comprise
/// (Cyrus Stream X2 de JP : 1372 `GetPositionInfo` en échec, contre 3
/// `SetAVTransportURI`).
///
/// `poll_states` ne peut pas porter cet état : il est purgé à chaque tick pour
/// ne garder que les zones en lecture.
#[derive(Debug, Default, Clone)]
struct IdlePollBackoff {
    consecutive_errors: u8,
    remaining: u8,
    /// Comptabilité du journal — distincte du recul, qui lui n'est pas en cause.
    journal: JournalSondage,
}

/// Combien d'échecs consécutifs sont **détaillés** avant de passer au
/// récapitulatif (#2566).
///
/// Cinq lignes suffisent à établir la cause : l'erreur est la même à chaque
/// tour — c'est la définition d'une panne durable — et les quatre premières
/// donnent en plus la montée du recul (`skip_ticks` 2, 4, 8, 16), donc de quoi
/// vérifier qu'il fonctionne. Un échec isolé est le n° 1 : il reste dit.
pub const ECHECS_SONDAGE_DETAILLES: u32 = 5;

/// Ce que la comptabilité décide de faire d'un échec de plus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceEchecSondage {
    /// Sous le plafond : la ligne complète est émise.
    Detaille,
    /// Au-dessus du plafond, à un palier : une ligne de récapitulatif portant
    /// le TOTAL est émise à la place.
    Recapitulatif,
    /// Au-dessus du plafond, hors palier : rien n'est émis.
    Muet,
}

/// Comptabilité du journal d'un sondage qui échoue, tour après tour (#2566).
///
/// ## Les trois sites, et pourquoi un seul compteur
///
/// | site | fichier | ce qu'un appareil muet coûtait |
/// |---|---|---|
/// | zone au repos | `poller.rs`, branche « repos » | 1 ligne / 33 s — les 79 de Dimitri |
/// | zone **en lecture** | `poller.rs`, branche « lecture » | 1 ligne / 17 s, soit ~212 / h |
/// | HQPlayer | `tune-server/src/background.rs` | 1 ligne / 60 s, **sans aucun recul** |
///
/// Le premier a été borné en v0.9.129 ; le commit qui l'a fait nommait les deux
/// autres comme non traités, et ce sont eux que ce passage-ci rejoint. Le
/// compteur est le même pour les trois — c'est la même panne vue de trois
/// boucles —, mais **l'émission reste locale à chaque site** : `tracing` fige
/// la cible du module au point d'appel, et l'export de diagnostic compte par
/// module (`QUOTA_PAR_MODULE`, #1974). Émettre le bruit d'HQPlayer depuis
/// `tune_core::poller` l'aurait imputé au poller, c'est-à-dire au module qu'on
/// lit précisément quand une lecture ne démarre pas.
///
/// ## Le défaut mesuré
///
/// Dimitri, macOS, v0.9.115, fil 1577 : une zone Chromecast a produit **79
/// lignes `idle_poll_failed_backing_off` identiques**, une par tentative. Le
/// recul exponentiel n'est pas en cause — il plafonnait correctement à
/// `2^IDLE_BACKOFF_MAX_SHIFT` = 32 ticks, et l'extrait le montre
/// (`skip_ticks=32`, 33 s entre deux lignes). C'est le JOURNAL qui n'avait
/// aucun plafond : une ligne par tentative, indéfiniment.
///
/// Au rythme du recul saturé — 32 ticks sautés + 1 tick de tentative, à
/// `POLL_INTERVAL_MS` = 1000 ms — cela fait **une ligne toutes les 33 s et par
/// zone**, soit ~109 lignes par heure. Les 79 échecs de Dimitri couvrent
/// **41 min 16 s**. Un appareil laissé éteint une nuit de 8 h en produit
/// **~870**, et rien ne l'arrête : `consecutive_errors` est un `u8` qui sature
/// à 255 sans jamais cesser de journaliser.
///
/// L'export de diagnostic borne chaque module à un quart de la fenêtre
/// (`QUOTA_PAR_MODULE`, #1974) : 79 lignes prennent déjà un tiers du quota de
/// `tune_core::poller` — le module qu'on lit précisément quand une lecture ne
/// démarre pas.
///
/// ## Le patron, repris de #2890
///
/// Quelques lignes détaillées plafonnées, puis un récapitulatif portant le
/// total — comme `track_insert_failures_truncated` dans `db::track_repo`, et
/// `scan_walk_errors_truncated` dans `scanner::walker`. Une seule différence :
/// là-bas la boucle a une fin (500 pistes), ici elle n'en a pas. Le
/// récapitulatif est donc émis **aux paliers de doublement** — échecs 8, 16,
/// 32, 64, 128… — au lieu d'une fois en sortie de boucle. Une panne coûte
/// ainsi un nombre de lignes **logarithmique** en sa durée, et non linéaire :
/// 79 échecs → 9 lignes, 870 échecs → 12 lignes.
///
/// La fin de panne, elle, est un vrai événement ponctuel : `succes` émet le
/// récapitulatif de clôture avec le total, exactement comme #2890 en sortie de
/// lot.
///
/// ## Ce que cela ne change pas
///
/// Ni la cadence, ni le recul, ni le nombre de tentatives : **aucune décision
/// de sondage ne passe par ici**. Seul le volume du journal change. Un échec
/// isolé reste dit en entier, et un sondage qui réussit sans échec préalable
/// n'émet toujours rien.
#[derive(Debug, Default, Clone)]
pub struct JournalSondage {
    /// Échecs consécutifs, en `u32` : `IdlePollBackoff::consecutive_errors`
    /// est un `u8` qui sature à 255, ce qui rendrait le total faux et les
    /// paliers erratiques au-delà de deux heures de panne.
    echecs: u32,
}

impl JournalSondage {
    /// Un échec de plus. Rend ce qu'il faut en dire.
    ///
    /// Publique parce que c'est la **décision** partagée par les trois sites :
    /// chacun l'interroge, puis émet sa propre ligne, avec ses propres champs
    /// et sous sa propre cible de module.
    pub fn compter_echec(&mut self) -> TraceEchecSondage {
        self.echecs = self.echecs.saturating_add(1);
        if self.echecs <= ECHECS_SONDAGE_DETAILLES {
            TraceEchecSondage::Detaille
        } else if self.echecs.is_power_of_two() {
            TraceEchecSondage::Recapitulatif
        } else {
            TraceEchecSondage::Muet
        }
    }

    /// Total d'échecs consécutifs en cours.
    pub fn echecs(&self) -> u32 {
        self.echecs
    }

    /// Un échec de plus, et la trace qui convient est **émise**.
    ///
    /// C'est le point d'émission réel du sondeur : le garde
    /// `tests/journal_sondage_repos.rs` appelle cette fonction-ci, pas une
    /// copie, et compte les lignes que `tracing` reçoit.
    pub fn echec(
        &mut self,
        zone_id: i64,
        device: &str,
        error: &dyn std::fmt::Display,
        skip_ticks: u8,
    ) {
        match self.compter_echec() {
            TraceEchecSondage::Detaille => debug!(
                zone_id,
                device = %device,
                error = %error,
                consecutive_errors = self.echecs,
                skip_ticks,
                "idle_poll_failed_backing_off"
            ),
            TraceEchecSondage::Recapitulatif => debug!(
                zone_id,
                device = %device,
                error = %error,
                echecs = self.echecs,
                detaillees = ECHECS_SONDAGE_DETAILLES,
                skip_ticks,
                "idle_poll_still_failing"
            ),
            TraceEchecSondage::Muet => {}
        }
    }

    /// Le sondage repasse. Émet la clôture de panne s'il y en avait une à
    /// clore, et rien du tout sinon — un sondage qui a toujours réussi ne doit
    /// pas changer d'un iota.
    pub fn succes(&mut self, zone_id: i64, device: &str) {
        if let Some(echecs) = self.cloturer() {
            debug!(
                zone_id,
                device = %device,
                echecs,
                "idle_poll_recovered"
            );
        }
    }

    /// Le sondage repasse : remet le compteur à zéro et rend le total à
    /// annoncer, ou `None` s'il n'y a **rien à clore**.
    ///
    /// Rien à clore, c'est le cas de l'écrasante majorité des tours : un
    /// sondage qui a toujours réussi, et un échec isolé déjà dit en entier. La
    /// clôture n'existe que pour la panne qu'on a cessé de détailler — sans
    /// elle, plafonner masquerait l'ampleur, et un plafond deviendrait une
    /// censure.
    pub fn cloturer(&mut self) -> Option<u32> {
        let echecs = std::mem::take(&mut self.echecs);
        (echecs > ECHECS_SONDAGE_DETAILLES).then_some(echecs)
    }

    /// Un échec de plus sur une zone **en lecture**, et la trace qui convient
    /// est émise.
    ///
    /// Jumelle de [`Self::echec`], et volontairement pas une paramétrisation de
    /// celle-ci : le nom d'un évènement `tracing` est figé au point d'appel,
    /// avec sa cible et son niveau. Le rendre variable ferait de
    /// `poll_failed_backing_off` et `idle_poll_failed_backing_off` un seul et
    /// même point d'appel, indiscernables dans un filtre par cible — pour
    /// n'économiser que l'invocation d'une macro.
    ///
    /// Les champs de la ligne détaillée sont **inchangés** (`zone_id`,
    /// `device`, `error`, `backoff`) : c'est le texte que les journaux déjà
    /// versés portent, et qu'on relit en cherchant une panne.
    pub fn echec_lecture(
        &mut self,
        zone_id: i64,
        device: &str,
        error: &dyn std::fmt::Display,
        backoff: u8,
    ) {
        match self.compter_echec() {
            TraceEchecSondage::Detaille => debug!(
                zone_id,
                device = %device,
                error = %error,
                backoff,
                "poll_failed_backing_off"
            ),
            TraceEchecSondage::Recapitulatif => debug!(
                zone_id,
                device = %device,
                error = %error,
                echecs = self.echecs,
                detaillees = ECHECS_SONDAGE_DETAILLES,
                backoff,
                "poll_still_failing"
            ),
            TraceEchecSondage::Muet => {}
        }
    }

    /// Le sondage d'une zone en lecture repasse. Muet s'il n'avait pas cessé
    /// de parler.
    pub fn succes_lecture(&mut self, zone_id: i64, device: &str) {
        if let Some(echecs) = self.cloturer() {
            debug!(
                zone_id,
                device = %device,
                echecs,
                "poll_recovered"
            );
        }
    }
}

impl IdlePollBackoff {
    /// Faut-il sauter ce tick ? Consomme un tick de recul le cas échéant.
    fn should_skip(&mut self) -> bool {
        if self.remaining > 0 {
            self.remaining -= 1;
            true
        } else {
            false
        }
    }

    /// Sondage réussi. La suite dépend de ce que l'appareil a répondu.
    ///
    /// Le plein rythme est réservé aux états que la branche « repos » sait
    /// EXPLOITER, pas à ceux qu'on appellerait volontiers « actifs » :
    ///
    /// - `Playing` — reprise d'état, adoption du volume et détection de
    ///   conflit s'y déclenchent, et elles y sont toutes conditionnées ;
    /// - `Transitioning` — transitoire par définition : le freiner
    ///   retarderait l'état qui va suivre, une seconde plus tard.
    ///
    /// `Stopped` et `Paused` ne peuvent rien produire de plus au tick suivant
    /// qu'à celui-ci : la zone retombe à la cadence de repos
    /// [`IDLE_REPOS_POLL_TICKS`] jusqu'à ce que l'appareil bouge (#2263).
    ///
    /// La pause était restée du côté « actif » au motif que la ralentir
    /// ralentirait aussi la reprise d'état et l'adoption du volume. Le motif
    /// ne tenait pas : ces deux-là exigent `status.state == Playing` et ne
    /// font donc RIEN d'un statut en pause. Une zone laissée en pause était
    /// sondée une fois par seconde, sans fin, pour rien.
    fn record_success(&mut self, etat: TransportState) {
        self.consecutive_errors = 0;
        self.remaining = match etat {
            TransportState::Stopped | TransportState::Paused => IDLE_REPOS_POLL_TICKS - 1,
            TransportState::Playing | TransportState::Transitioning => 0,
        };
    }

    /// Sondage en échec : recul exponentiel, plafonné.
    fn record_failure(&mut self) {
        self.consecutive_errors = self.consecutive_errors.saturating_add(1);
        self.remaining = 1u8 << self.consecutive_errors.min(IDLE_BACKOFF_MAX_SHIFT);
    }
}

struct ZonePollState {
    gapless_sent: bool,
    stopped_ticks: u8,
    /// Ticks consecutifs ou le renderer rapporte une URI qui n'est pas la
    /// notre. Trois d'affilee avant de parler : une transition de piste peut
    /// montrer un instant l'URI precedente.
    tenue_etrangere_ticks: u8,
    /// Le conflit a deja ete signale pour cette generation de piste : on ne
    /// harcele pas l'utilisateur a chaque tick.
    tenue_signalee: bool,
    /// Ticks to ignore Stopped state after a gapless advance, so the
    /// poller doesn't re-send play_from_queue to a renderer that already
    /// transitioned via SetNextAVTransportURI.
    gapless_cooldown: u8,
    /// Consecutive poll failures — used for exponential backoff.
    /// After N failures, skip 2^min(N,4) ticks before retrying.
    consecutive_errors: u8,
    backoff_remaining: u8,
    /// Comptabilité du journal (#2566), sans effet sur les deux champs
    /// ci-dessus : le recul et le compte d'erreurs sont tenus par le site
    /// d'appel, avant elle, et ce sont eux que lisent `poll_failed_past_end`
    /// et l'arrêt de zone. Voir [`JournalSondage`].
    journal: JournalSondage,
    total_polls: u64,
    total_errors: u64,
    last_latency_ms: u32,
    max_latency_ms: u32,
    last_radio_poll: Instant,
    /// When SetNextAVTransportURI was sent — used to guard against
    /// false track-end detection during gapless transitions on renderers
    /// like Eversolo DMP-A6 that briefly report Stopped or reset position.
    gapless_sent_at: Option<Instant>,
    /// Last polled position in milliseconds — used to detect position
    /// resets (jumps from >30s to <5s) that signal a gapless transition.
    last_position_ms: u64,
    /// Peak position reached in the current track — high-water mark used
    /// to verify that enough of the track was actually played before
    /// accepting a gapless transition.
    peak_position_ms: u64,
    /// Identity key (`decisions::scrobble_track_key`) of the track already
    /// scrobbled, latched once it crosses the Last.fm threshold (50% / 4 min)
    /// so it scrobbles exactly once. A plain boolean here was only reset on
    /// `track_generation` changes, which gapless advances skip — so every
    /// gapless-reached track was silently dropped (#1113). The identity key
    /// re-arms on any track change regardless of the advance path.
    scrobbled_key: Option<String>,
    /// Tick counter for throttling DB position saves.
    ticks_since_db_save: u64,
    /// When the current track started playing (wall clock).
    /// Used to reject false gapless transitions that happen too soon.
    track_started_at: Option<Instant>,
    /// The `ZoneState::last_seek_at` instant we last folded into
    /// `track_started_at`. A user seek moves the play position without moving
    /// the wall clock, which starves every wall-clock guard downstream
    /// (`played_enough`, `ended_naturally_wall_ok`): seek to the end right
    /// after start and the real track end is rejected as a spurious
    /// renderer signal — playback just stops instead of advancing (DEvir,
    /// v0.9.0-rc4). On each NEW seek we rewind `track_started_at` by the seek
    /// target so `wall_elapsed` matches "played at 1x from the start" again.
    last_seek_seen: Option<Instant>,
    /// Tracks the `ZoneState::track_generation` we last observed.
    /// When the generation changes (new track started via `play()`),
    /// we reset all per-track state so stale values from the previous
    /// track cannot trigger false gapless advances or premature track ends.
    track_generation: u64,
    /// When the orchestrator loaded the current track (track_generation changed).
    /// Used for the startup grace period — DLNA renderers report Stopped while
    /// buffering a new stream, especially after transcoding delays.
    track_loaded_at: Instant,
    /// Counts ticks where the output reports Playing but position_ms has
    /// reached or exceeded the known track duration.  After
    /// POSITION_PAST_END_TICKS consecutive ticks in this state, the poller
    /// treats the track as ended even though the output hasn't reported
    /// Stopped.  This handles local/cpal outputs where the playback thread
    /// may be slow to set `playing = false`.
    past_end_ticks: u8,
    /// Set to true after `gapless_natural_end_advancing_metadata` — the poller
    /// advanced metadata expecting the renderer to auto-transition.  If the
    /// renderer stays Stopped after gapless_cooldown expires, this flag lets
    /// the poller detect the stuck state and force a play_from_queue.
    gapless_advance_pending: bool,
    /// Counts Stopped ticks after gapless_cooldown expires while
    /// gapless_advance_pending is true.  When this reaches
    /// GAPLESS_STUCK_THRESHOLD, the poller gives up on the gapless
    /// transition and forces play_from_queue.
    gapless_stuck_ticks: u8,
    last_bytes_sent: u64,
    /// Consecutive `Playing` polls with neither renderer position nor served
    /// bytes progressing. See `decisions::dlna_playing_stall_eligible`.
    playing_stall_ticks: u8,
    /// Ticks CONSECUTIFS ou la position rapportee est a la fin de la piste — ou
    /// au-dela — alors que l'appareil annonce toujours jouer. Voir
    /// [`decisions::position_au_dela_de_la_duree`] et [`DEPASSEMENT_DUREE_TICKS`].
    ///
    /// Compte dans le bras `Playing` uniquement : une zone en pause n'y passe
    /// pas, donc une pause de vingt minutes ne gonfle pas ce compteur — c'est
    /// precisement ce que l'horloge murale (`track_started_at`, jamais repliee
    /// a la reprise) ne sait pas faire. Remis a zero des que la position quitte
    /// la zone de fin, et a chaque changement de piste.
    depassement_duree_ticks: u8,
    /// Latch par piste : l'incoherence a deja ete DITE une fois (journal +
    /// metrique de zone). Sans lui la boucle ecrirait la meme ligne chaque
    /// seconde pendant tout le temps que dure le blocage.
    depassement_duree_signale: bool,
    /// Ticks pendant lesquels on a refusé de conclure à une fin naturelle parce
    /// que le flux servi était manifestement incomplet (voir
    /// STALL_DECLINE_MAX_TICKS). Remis à zéro à chaque changement de piste.
    stall_declines: u8,
    radio_stopped_ticks: u8,
    /// Last position (ms) the renderer reported on the previous radio poll.
    /// An advancing position means the renderer is actually streaming even
    /// when it (mis)reports TransportState=Stopped for a live source — the
    /// Yamaha R-N2000A does this on MP3 ICEcast streams (AAC plays fine).
    last_radio_position_ms: u64,
    /// Last volume the renderer reported (0.0–1.0) on a previous poll. Used to
    /// distinguish a real external volume change (the value moved) from a
    /// renderer that persistently reports a stale default (e.g. Devialet at
    /// 50%), which must not overwrite the user's saved volume.
    last_device_volume: Option<f64>,
    /// Per-track latch for the DLNA poll-fail wall-clock end-of-track fallback
    /// (`decisions::poll_failed_past_end`). The Err poll branch can't remove the
    /// poll state (it holds a live borrow), so this ensures the fallback fires at
    /// most once per track. Cleared on every track-generation change.
    wall_clock_end_fired: bool,
    /// Instrumentation latch (#1239): last `should_arm_gapless` decision we
    /// emitted in the `gapless_arm_trace` INFO line for the current track. The
    /// trace fires only when this value flips (arming window opens/closes) —
    /// `None` on a fresh track forces one line per track — so it never spams at
    /// the ~1 s tick rate. Read-only diagnostic; drives no playback decision.
    gapless_arm_logged: Option<bool>,
    /// Verrou par piste (#2394) : la position de file pour laquelle
    /// `prepare_gapless` a constaté « suivant DSD sur DLNA, gapless refusé ».
    /// Sans lui, la fenêtre d'armement re-résout la piste suivante À CHAQUE
    /// tick — création puis destruction d'une session fichier par seconde
    /// pendant toute la fin d'une piste DSD (constaté sur DMP-A8, 96
    /// occurrences en 2 h). On ne peut PAS poser `gapless_sent = true` comme
    /// pour la sortie exclusive : sur DLNA, ce drapeau active les détecteurs
    /// de transition (durée/position) et le DMP-A8 rapporte des durées
    /// inexactes — fausse transition garantie. Cleared au changement de
    /// génération et à chaque transition, comme `gapless_arm_logged`.
    gapless_dsd_skip_pos: Option<i64>,
    /// La LIGNE de file (`queue_items.id`) que le renderer a ACCEPTEE comme
    /// piste suivante, et la position qu'elle occupait alors (#3026).
    ///
    /// L'armement ne laissait aucune trace de ce qu'il avait arme. A la
    /// transition, le poller avancait donc sur `next_position()` — l'index+1
    /// COURANT — qui n'est plus la piste armee des que la file a bouge entre
    /// les deux. Un « Lire ensuite » dans les 30 dernieres secondes suffit : le
    /// renderer joue ce qu'on lui a envoye, l'ecran nomme l'inseree, et le
    /// compteur de fin de piste adopte la duree de l'INSEREE — d'ou la coupure
    /// de l'audio reellement en cours (`dlna_frozen_end=true`, journal Sandro
    /// du 01/09 a 14:23:10).
    gapless_armed: Option<ArmedNext>,
}

impl ZonePollState {
    /// Etat de sondage neuf pour une zone qui vient d'entrer en lecture.
    ///
    /// Etait construit en ligne, champ par champ, a un seul endroit. Il en
    /// faut desormais deux — la zone avec peripherique et celle sans — et
    /// recopier vingt-neuf champs est le genre de chose qui diverge en
    /// silence.
    fn new(track_generation: u64) -> Self {
        Self {
            gapless_sent: false,
            stopped_ticks: 0,
            tenue_etrangere_ticks: 0,
            tenue_signalee: false,
            gapless_cooldown: 0,
            consecutive_errors: 0,
            backoff_remaining: 0,
            journal: JournalSondage::default(),
            total_polls: 0,
            total_errors: 0,
            last_latency_ms: 0,
            max_latency_ms: 0,
            last_radio_poll: Instant::now(),
            gapless_sent_at: None,
            last_position_ms: 0,
            peak_position_ms: 0,
            scrobbled_key: None,
            ticks_since_db_save: 0,
            track_started_at: None,
            last_seek_seen: None,
            track_generation: track_generation,
            track_loaded_at: Instant::now(),
            past_end_ticks: 0,
            gapless_advance_pending: false,
            gapless_stuck_ticks: 0,
            last_bytes_sent: 0,
            playing_stall_ticks: 0,
            depassement_duree_ticks: 0,
            depassement_duree_signale: false,
            stall_declines: 0,
            radio_stopped_ticks: 0,
            last_radio_position_ms: 0,
            last_device_volume: None,
            wall_clock_end_fired: false,
            gapless_arm_logged: None,
            gapless_dsd_skip_pos: None,
            gapless_armed: None,
        }
    }
}

/// Issue de `prepare_gapless` : distinguer « rien à armer / échec (re-tenter
/// au prochain tick) » de « suivant DSD sur DLNA (inutile de re-tenter pour
/// cette position — verrou `gapless_dsd_skip_pos`, #2394) ».
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GaplessPrep {
    /// Le renderer a accepte la piste suivante. Porte la LIGNE de file
    /// reellement envoyee (`None` si la file n'a pas su la rendre) : c'est
    /// elle, et non l'index, qui decide ou avancer a la transition (#3026).
    Armed(Option<ArmedNext>),
    DsdNextSkipped,
    NotArmed,
}

/// Ce que le renderer a ACCEPTE comme piste suivante — a distinguer de ce que
/// la file designe comme suivante : les deux divergent des qu'on insere (#3026).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ArmedNext {
    /// `queue_items.id`. Stable quand `insert_at` decale les positions.
    row_id: i64,
    /// La position occupee AU MOMENT de l'armement. Journalisee seule : elle
    /// dit de combien la file a glisse sous l'armement.
    position: i64,
}

/// Retrouver en base la station qui joue, a partir du `source_id` du
/// now-playing.
///
/// Le `source_id` d'une radio n'a pas UNE forme mais deux, et c'est tout le
/// probleme : l'identifiant numerique de la ligne, ou l'URL du flux.
pub(crate) fn station_du_now_playing(
    repo: &crate::db::radio_repo::RadioRepo,
    source_id: &str,
) -> Option<crate::db::radio_repo::RadioStation> {
    if let Ok(id) = source_id.parse::<i64>() {
        if let Ok(Some(station)) = repo.get(id) {
            return Some(station);
        }
    }
    // Et par URL de flux, qui est ce que le chemin de lecture normal écrit :
    // `POST /radios/{id}/play/{zone_id}` pose `source_id: Some(radio.url)`.
    // Ne chercher que par identifiant numérique revenait à ne jamais chercher.
    repo.get_by_url(source_id).ok().flatten()
}

/// Choisir la vignette d'un pas de radio.
///
/// « Un pas » et non « un morceau » : entre deux chansons il y a des
/// chroniques, des jingles, des flashs — des pas qui n'ont pas de pochette.
///
/// Deux pas, et deux seulement : la pochette du morceau quand la station la
/// donne (Bertrand : « mettre la pochette de l'album et non le logo de la
/// radio »), le logo de la station sinon. **Et rien après.** Se rabattre sur
/// la pochette COURANTE serait recycler celle du pas précédent : dès qu'un
/// titre en a posé une, `cover_path` la porte, et la chronique qui suit
/// l'hériterait puis la garderait. On n'illustre pas le journal de 13 h avec
/// la chanson d'avant : mieux vaut le micro générique qu'une pochette fausse.
///
/// Une chaîne vide ne compte pas pour une valeur — `Option::or` ne le voit
/// pas, `Some("")` gagne contre `None` et l'on publie une URL vide.
pub(crate) fn vignette_du_pas_radio(
    pochette_titre: Option<&str>,
    logo_station: Option<&str>,
) -> Option<String> {
    let renseigne = |v: Option<&str>| {
        v.map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    renseigne(pochette_titre).or_else(|| renseigne(logo_station))
}

pub struct PositionPoller {
    orchestrator: Arc<PlaybackOrchestrator>,
    playback: Arc<PlaybackManager>,
    outputs: Arc<Mutex<OutputRegistry>>,
    db: Arc<dyn crate::db::backend::DbBackend>,
    shared_metrics: PollerMetricsMap,
    event_bus: Option<Arc<crate::event_bus::EventBus>>,
    /// Horodatage de la dernière relance automatique après « démarrage mort »
    /// par zone (#2394). Vit HORS de ZonePollState : la relance recrée l'état
    /// de sondage, un drapeau dedans repartirait à zéro et bouclerait. Une
    /// relance au plus par fenêtre de DEAD_START_RETRY_COOLDOWN_SECS ; si la
    /// relance échoue à son tour, la zone est coupée comme avant.
    relances_demarrage_mort: Mutex<std::collections::HashMap<i64, Instant>>,
}

impl PositionPoller {
    pub fn new(
        orchestrator: Arc<PlaybackOrchestrator>,
        playback: Arc<PlaybackManager>,
        outputs: Arc<Mutex<OutputRegistry>>,
        db: Arc<dyn crate::db::backend::DbBackend>,
        shared_metrics: PollerMetricsMap,
    ) -> Self {
        Self {
            orchestrator,
            playback,
            outputs,
            db,
            shared_metrics,
            event_bus: None,
            relances_demarrage_mort: Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn with_event_bus(mut self, bus: Arc<crate::event_bus::EventBus>) -> Self {
        self.event_bus = Some(bus);
        self
    }

    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            info!("position_poller_started");
            let startup_at = Instant::now();
            let mut ticker = tokio::time::interval(Duration::from_millis(POLL_INTERVAL_MS));
            let notify = TRACK_END_NOTIFY.clone();
            let mut poll_states: HashMap<i64, ZonePollState> = HashMap::new();
            let mut idle_backoff: HashMap<i64, IdlePollBackoff> = HashMap::new();
            // Derniere valeur annoncee de `levels_available`, par zone.
            let mut niveaux_annonces: HashMap<i64, bool> = HashMap::new();

            loop {
                // Wake on either the regular 1-second tick OR an immediate
                // notification from a local output that finished a track.
                tokio::select! {
                    _ = ticker.tick() => {},
                    _ = notify.notified() => {},
                }
                self.tick(&mut poll_states, &mut idle_backoff, &startup_at)
                    .await;
                self.annoncer_bascule_des_niveaux(&mut niveaux_annonces)
                    .await;
            }
        })
    }

    /// Rafraichit le titre/interprete d'une zone qui joue une radio.
    ///
    /// Extrait de la boucle de sondage pour pouvoir servir DEUX appelants : la
    /// zone qui a un peripherique de sortie, et celle qui n'en a pas. La
    /// seconde n'etait servie par personne — voir l'appel dans `tick`.
    ///
    /// Le choix de la vignette et la recherche de la station vivent dans
    /// [`vignette_du_pas_radio`] et [`station_du_now_playing`] : les deux se
    /// prouvent hors reseau, ce que cette fonction-ci ne permet pas.
    async fn refresh_radio_metadata(&self, zone_id: i64, zone_state: &crate::playback::ZoneState) {
        // Radio metadata polling (title/artist from ICY or external)
        if let Some(ref np) = zone_state.now_playing {
            if np.source == "radio" {
                if let Some(ref source_id) = np.source_id {
                    // source_id is either a numeric radio DB id or the stream URL itself
                    // Le logo de la station sert de REPLI quand le titre en
                    // cours n'a pas de pochette. Il faut le relire ici et non
                    // reprendre `np.cover_path` : dès qu'un titre a posé sa
                    // pochette, `cover_path` la porte, et le titre suivant —
                    // une chronique, un jingle — hériterait de la pochette du
                    // précédent au lieu de revenir au logo.
                    let radio_repo =
                        crate::db::radio_repo::RadioRepo::with_backend(self.db.clone());
                    let mut logo_station: Option<String> = None;
                    let (station_name, stream_url) =
                        if let Some(station) = station_du_now_playing(&radio_repo, source_id) {
                            logo_station = station.logo_url.clone();
                            (station.name.clone(), station.url.clone())
                        } else {
                            // Station introuvable en base : on retombe sur
                            // `album_title`, qui porte le nom de la station et
                            // survit aux mises a jour (`np.title`, lui, prend
                            // le titre du morceau des le premier
                            // rafraichissement).
                            let name = np.album_title.clone().unwrap_or_else(|| np.title.clone());
                            (name, source_id.clone())
                        };

                    if let Some(meta) =
                        crate::radio_metadata::fetch_radio_metadata(&station_name, &stream_url)
                            .await
                    {
                        // La pochette du titre quand la station la donne, le
                        // logo sinon. Bertrand : « mettre la pochette de
                        // l'album et non le logo de la radio ».
                        let pochette = vignette_du_pas_radio(
                            meta.cover_url.as_deref(),
                            logo_station.as_deref(),
                        );
                        let title_changed = np.title != meta.title
                            || np.artist_name != meta.artist
                            || np.cover_path != pochette;
                        if title_changed {
                            let new_np = crate::playback::NowPlaying {
                                track_id: None,
                                title: meta.title,
                                artist_name: meta.artist,
                                album_title: Some(station_name.clone()),
                                cover_path: pochette,
                                duration_ms: 0,
                                source: "radio".into(),
                                source_id: np.source_id.clone(),
                                stream_id: np.stream_id.clone(),
                                ..Default::default()
                            };
                            // Le renderer, lui, ne lit pas le now-playing : il
                            // reçoit des blocs ICY dans le flux. On publie donc
                            // titre ET pochette là où le gestionnaire de flux
                            // saura les relire, sinon l'appareil reste figé sur
                            // le morceau qui passait à sa connexion (#2161).
                            //
                            // On lit ces trois valeurs SUR `new_np`, et non sur
                            // des copies prises plus haut : ce sont exactement
                            // celles que l'interface Tune va recevoir. Trois
                            // variables `*_for_icy` parallèles pouvaient diverger
                            // du now-playing sans qu'aucune épreuve ne le voie —
                            // et c'est cette classe d'écart silencieux entre le
                            // producteur et le consommateur qui a produit ce
                            // ticket. Ici, l'écart n'est plus représentable.
                            //
                            // ── Et ce que cette garde TAISAIT (#2991) ──
                            //
                            // Sans `stream_id`, le `if let` ci-dessous ne
                            // publiait rien — en silence. L'interface Tune, elle,
                            // était mise à jour deux lignes plus bas par
                            // `update_now_playing`, qui ne dépend pas du
                            // `stream_id`. « Dans Tune ça fonctionne, sur le
                            // RS250A non » est le symptôme EXACT de cet écart, et
                            // rien au journal ne permettait de le distinguer d'un
                            // renderer qui n'aurait pas demandé l'ICY. Deux causes
                            // opposées, une seule absence de trace.
                            //
                            // On lit donc le canal AVANT de publier, et on
                            // journalise dans TOUS les cas — y compris celui qui
                            // marche, sans quoi « pas de ligne » resterait
                            // ambigu.
                            let canal = crate::http::streamer::canal_radio(np.stream_id.as_deref());
                            if let Some(sid) = np.stream_id.as_deref() {
                                crate::http::streamer::publish_radio_now(
                                    sid,
                                    new_np.artist_name.clone(),
                                    new_np.title.clone(),
                                    new_np.cover_path.clone(),
                                );
                            }
                            // UNE ligne doit suffire à savoir laquelle des
                            // branches mord la prochaine fois qu'un testeur
                            // signale un écran figé. Même nom d'évènement dans
                            // les deux cas — seul le niveau change — pour qu'un
                            // `grep radio_refresh_channel` les ramène ensemble.
                            let sid_journal = np.stream_id.as_deref().unwrap_or("absent");
                            if canal.atteint_le_renderer() {
                                debug!(
                                    zone_id,
                                    station = %station_name,
                                    stream_id = sid_journal,
                                    canal = canal.libelle(),
                                    "radio_refresh_channel"
                                );
                            } else {
                                warn!(
                                    zone_id,
                                    station = %station_name,
                                    stream_id = sid_journal,
                                    canal = canal.libelle(),
                                    "radio_refresh_channel — le morceau a changé mais l'écran du \
                                     lecteur réseau ne l'apprendra pas"
                                );
                            }
                            self.playback.update_now_playing(zone_id, new_np).await;
                            debug!(zone_id, station = %station_name, "radio_metadata_updated");
                        }
                    }
                }
            }
        }
    }

    /// Annoncer quand une zone se met — ou cesse — de mesurer ses niveaux.
    ///
    /// `levels_available` est calcule a la demande sur les trois surfaces de
    /// zone, mais le drapeau qui le determine bascule TARD : la tache OAAT ne
    /// passe en DSD natif qu'apres l'arret, deux secondes d'attente, la
    /// connexion et la detection du `.dsf`. La reponse HTTP du play est deja
    /// partie, avec `levels_available: true` — le bon champ restait donc
    /// invisible pendant exactement la lecture concernee (JP Robbe, revue de
    /// #2220).
    ///
    /// On emet un `zone.updated` SANS donnees inline : le client refetch alors
    /// toutes ses zones (`App.svelte`, branche de repli des evenements
    /// `zone.*`). Pas de nouveau type d'evenement, pas de changement cote web.
    ///
    /// Le cout est nul pour les zones non-OAAT : `output_produces_levels`
    /// teste le prefixe `oaat:` avant de prendre le moindre verrou.
    pub(crate) async fn annoncer_bascule_des_niveaux(&self, dernier: &mut HashMap<i64, bool>) {
        let Some(ref bus) = self.event_bus else {
            return;
        };
        let zones = crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .list()
            .unwrap_or_default();
        let vus: std::collections::HashSet<i64> = zones.iter().filter_map(|z| z.id).collect();
        dernier.retain(|zone_id, _| vus.contains(zone_id));

        for zone in &zones {
            let Some(zone_id) = zone.id else { continue };
            let dispo = self
                .orchestrator
                .output_produces_levels(zone.output_device_id.as_deref())
                .await;
            match dernier.get(&zone_id) {
                Some(&precedent) if precedent == dispo => {}
                _ => {
                    // Premier passage compris : une zone deja en DSD natif au
                    // demarrage du serveur doit s'annoncer elle aussi.
                    if dernier.insert(zone_id, dispo) != Some(dispo) {
                        info!(
                            zone_id,
                            levels_available = dispo,
                            "levels_availability_changed"
                        );
                        bus.emit("zone.updated", serde_json::json!({ "zone_id": zone_id }));
                    }
                }
            }
        }
    }

    /// Une lecture que personne ne reçoit cesse d'être annoncée « en cours ».
    ///
    /// Rend `true` si la zone a été arrêtée. Appelée par le tick UNIQUEMENT
    /// dans la branche « zone sans périphérique de sortie » : c'est là, et
    /// seulement là, que l'état « en lecture » ne repose sur rien. Une zone
    /// avec périphérique a déjà ses propres chiens de garde
    /// (`output_reported_failure_stopping_zone`,
    /// `dlna_playing_without_progress_stopping_zone`, `demarrage_mort`).
    ///
    /// Le verdict lui-même est dans
    /// [`decisions::lecture_sans_destination_abandonnee`] — et il ne regarde
    /// PAS la présence d'un périphérique, seulement la consommation du flux.
    /// Une zone navigateur dont l'onglet joue sert des octets et n'est jamais
    /// touchée ici.
    async fn abandonner_lecture_sans_destination(
        &self,
        zone_state: &crate::playback::ZoneState,
    ) -> bool {
        let zone_id = zone_state.zone_id;
        let octets_servis = match zone_state
            .now_playing
            .as_ref()
            .and_then(|np| np.stream_id.as_deref())
        {
            Some(sid) => self.orchestrator.streamer_bytes_sent(sid).await,
            None => None,
        };
        if !decisions::lecture_sans_destination_abandonnee(
            zone_state.last_play_started_at.map(|t| t.elapsed()),
            octets_servis,
        ) {
            return false;
        }

        let zone = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten();
        let navigateur = zone.as_ref().and_then(|z| z.output_type.as_deref()) == Some("browser");
        // La zone navigateur et la zone orpheline produisent le même silence,
        // mais pas le même geste : ouvrir un onglet, ou attribuer une sortie.
        // Le message dit lequel. Le second reprend mot pour mot la sentinelle
        // que le client sait déjà traduire (`zone_no_output_device`).
        let message = if navigateur {
            "zone_browser_unattended:No browser tab is playing this zone — open Tune in a browser \
             on the computer that should play, or give this zone an output device."
                .to_string()
        } else {
            format!(
                "zone_no_output_device:Zone '{}' has no output device assigned — assign an output \
                 device to this zone or delete it and re-create it from a device.",
                zone.as_ref().map(|z| z.name.as_str()).unwrap_or("?")
            )
        };
        warn!(
            zone_id,
            navigateur,
            title = zone_state
                .now_playing
                .as_ref()
                .map(|np| np.title.as_str())
                .unwrap_or(""),
            "lecture_sans_destination_abandonnee"
        );
        if let Some(ref bus) = self.event_bus {
            // `fatal` : rien ne se rétablira tout seul, et la zone s'arrête
            // juste après — sans ce drapeau la fenêtre de grâce d'après-lecture
            // du client avalerait le message, et l'utilisateur n'aurait, une
            // fois de plus, que le silence.
            bus.emit(
                "zone.playback_error",
                serde_json::json!({
                    "zone_id": zone_id,
                    "error": message,
                    "fatal": true,
                }),
            );
        }
        // Horodater le constat AVANT l'arrêt : c'est l'arrêt lui-même qui
        // faisait retomber `output_reach` à `"ok"` et effaçait le bandeau
        // « aucun onglet ne reçoit le son » à l'instant où il devenait vrai
        // (#2588). La marque survit à l'arrêt ; la lecture suivante l'efface.
        if navigateur {
            self.orchestrator
                .note_browser_unattended(zone_id, true)
                .await;
        }
        // Sans identifiant d'appareil, `stop` prend son repli — celui que
        // #2658 vient de borner au périmètre de la zone. Avant elle, arrêter
        // une zone navigateur coupait la musique de TOUTES les autres.
        self.orchestrator.stop(zone_id, None).await;
        true
    }

    async fn tick(
        &self,
        poll_states: &mut HashMap<i64, ZonePollState>,
        idle_backoff: &mut HashMap<i64, IdlePollBackoff>,
        startup_at: &Instant,
    ) {
        let states = self.playback.all_states().await;

        poll_states.retain(|zone_id, _| {
            states
                .iter()
                .any(|s| s.zone_id == *zone_id && s.state == PlayState::Playing)
        });

        // Also poll stopped zones to detect externally-started playback and sync volume
        let all_zones = crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .list()
            .unwrap_or_default();

        // Ne pas laisser l'état de recul survivre à une zone supprimée.
        idle_backoff.retain(|zone_id, _| all_zones.iter().any(|z| z.id == Some(*zone_id)));

        for zone in &all_zones {
            let zone_id = zone.id.unwrap_or(0);
            if zone_id == 0 {
                continue;
            }
            let device_id = match zone.output_device_id.as_deref() {
                Some(id) if !id.is_empty() => id.to_string(),
                _ => continue,
            };

            let in_states = states
                .iter()
                .any(|s| s.zone_id == zone_id && s.state == PlayState::Playing);
            if in_states {
                continue;
            } // already handled below

            // Recul après échec : sans cela un appareil injoignable était sondé
            // chaque seconde sans fin (voir IdlePollBackoff).
            if idle_backoff.entry(zone_id).or_default().should_skip() {
                continue;
            }

            let status = {
                let output_arc = {
                    let outputs = self.outputs.lock().await;
                    match outputs.get(&device_id) {
                        Some(o) => o,
                        None => continue,
                    }
                };
                match get_status_with_signal_path_bounded(&output_arc, *STATUS_POLL_TIMEOUT).await {
                    Ok((s, signal_path, dsp_metrics)) => {
                        let b = idle_backoff.entry(zone_id).or_default();
                        b.record_success(s.state);
                        // Clôture de panne (#2566) : muette si le sondage
                        // n'avait jamais échoué.
                        b.journal.succes(zone_id, &device_id);
                        // Le curseur de volume est inerte tant que dure le DoP :
                        // l'état de zone doit le dire au client (#1735).
                        self.playback.set_dop_active(zone_id, s.dop_active).await;
                        self.playback
                            .set_output_signal_path(zone_id, signal_path)
                            .await;
                        self.playback
                            .set_output_dsp_metrics(zone_id, dsp_metrics)
                            .await;
                        s
                    }
                    Err(e) => {
                        let b = idle_backoff.entry(zone_id).or_default();
                        b.record_failure();
                        // Plafonné, sur le modèle de #2890 (#2566) : une panne
                        // durable dit sa cause quelques fois, puis se
                        // récapitule aux paliers de doublement. Le recul
                        // lui-même, lui, ne change pas d'un tick.
                        let skip_ticks = b.remaining;
                        b.journal.echec(zone_id, &device_id, &e, skip_ticks);
                        continue;
                    }
                }
            };

            // Sync volume from device only when playing AND the device
            // reports a significantly different volume from what we have in
            // memory.  Many DLNA renderers report a stale default (e.g. 50%)
            // right after playback starts, which would overwrite the user's
            // saved volume. Skip during the first 30s after startup to let
            // restore_zone_volumes take precedence over device defaults.
            let in_startup_grace = startup_at.elapsed().as_secs() < 30;
            let in_volume_grace = self
                .playback
                .get_state(zone_id)
                .await
                .last_volume_set_at
                .is_some_and(|t| t.elapsed().as_secs() < VOLUME_GRACE_SECS);
            if !zone.fixed_volume
                && !in_startup_grace
                && !in_volume_grace
                && status.volume > 0.001
                && status.volume < 0.999
                && status.state == TransportState::Playing
            {
                let db_vol = zone.volume / 100.0;
                let prev_device_vol = poll_states.get(&zone_id).and_then(|p| p.last_device_volume);
                // Edge-triggered: adopt the renderer's volume only when it
                // actually moved since the last poll (see decisions::
                // should_adopt_device_volume), so a stale default (Fabien's
                // Devialet stuck at 50%) can't overwrite the saved volume.
                if decisions::should_adopt_device_volume(prev_device_vol, status.volume, db_vol) {
                    self.playback.set_volume(zone_id, status.volume).await;
                    // #2886 — `as i32` TRONQUAIT : le volume adopte du renderer
                    // tombait a 0 sous 0,01 lineaire (-40 dB).
                    let vol_pct = status.volume * 100.0;
                    crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
                        .update_volume(zone_id, vol_pct)
                        .ok();
                }
                // Remember what the renderer reported so the next tick can
                // detect a genuine change.
                if let Some(ps) = poll_states.get_mut(&zone_id) {
                    ps.last_device_volume = Some(status.volume);
                }
            }

            // Recover playing state from device — only if Tune was actually
            // playing on this zone before (last_play_state == "playing" in DB).
            // Without this check, playback from other apps (Roon, Spotify
            // Connect, etc.) on a shared renderer (Sonos) would be captured
            // by Tune and trigger phantom queue playback when the other app stops.
            // Skip recovery during startup grace (30s) — the orchestrator may
            // still be sending play commands and the renderer reports Playing
            // before PlaybackManager is updated.
            // Re-read PlaybackManager state AFTER the device poll to avoid
            // the race where orchestrator.play() sets Playing between the
            // initial states read and the device poll response.
            let fresh_states = self.playback.all_states().await;
            let already_playing = fresh_states
                .iter()
                .any(|s| s.zone_id == zone_id && s.state == PlayState::Playing);

            // ── Le renderer nous appartient-il encore ? ──
            //
            // Deux serveurs Tune sur le même appareil, ou son lecteur interne
            // qui reprend la main après un redémarrage : chaque perdant
            // échouait EN SILENCE, l'interface relançait toutes les quinze
            // secondes, et un conflit d'appareil s'est déguisé en « bug DSD »
            // (DMP-A8, 24/08). On regarde l'URI que le renderer rapporte, et
            // on DIT à l'utilisateur qui tient l'appareil — une fois par
            // piste, après trois ticks concordants (une transition peut
            // montrer un instant l'URI précédente).
            if already_playing {
                let notre_stream_id = fresh_states
                    .iter()
                    .find(|s| s.zone_id == zone_id)
                    .and_then(|s| s.now_playing.as_ref())
                    .and_then(|np| np.stream_id.clone());
                if let Some(ps) = poll_states.get_mut(&zone_id) {
                    use decisions::TenueDuRenderer;
                    let verdict = decisions::qui_tient_le_renderer(
                        status.current_uri.as_deref(),
                        notre_stream_id.as_deref(),
                    );
                    // L'URI vide ne dit « lecteur interne » que si le
                    // transport est actif : un renderer arrêté a le droit de
                    // n'avoir rien chargé.
                    let etrangere = match &verdict {
                        TenueDuRenderer::LeNotre => false,
                        TenueDuRenderer::LecteurInterne => status.state == TransportState::Playing,
                        _ => true,
                    };
                    if etrangere {
                        ps.tenue_etrangere_ticks = ps.tenue_etrangere_ticks.saturating_add(1);
                        if ps.tenue_etrangere_ticks >= 3 && !ps.tenue_signalee {
                            ps.tenue_signalee = true;
                            let message = match &verdict {
                                TenueDuRenderer::AutreServeurTune(hote) => format!(
                                    "Cet appareil est tenu par un autre serveur Tune ({hote}).                                      Arrêtez la lecture sur ce serveur-là, ou choisissez un autre appareil."
                                ),
                                TenueDuRenderer::LecteurInterne => "L'appareil joue depuis sa propre                                      interface. Arrêtez la lecture sur l'appareil lui-même, puis relancez."
                                    .to_string(),
                                _ => "Cet appareil est tenu par une autre application.                                      Arrêtez-y la lecture, puis relancez."
                                    .to_string(),
                            };
                            warn!(
                                zone_id,
                                device = %device_id,
                                verdict = ?verdict,
                                uri = ?status.current_uri,
                                "renderer_tenu_par_un_tiers — la lecture demandée ne sortira pas"
                            );
                            if let Some(ref bus) = self.event_bus {
                                bus.emit(
                                    "zone.playback_error",
                                    serde_json::json!({
                                        "zone_id": zone_id,
                                        "error": message,
                                        // `fatal` : rien ne se rétablira tout
                                        // seul, l'utilisateur doit agir — le
                                        // message le lui dit.
                                        "fatal": true,
                                    }),
                                );
                            }
                        }
                    } else {
                        ps.tenue_etrangere_ticks = 0;
                    }
                }
            }
            if status.state == TransportState::Playing && !already_playing && !in_startup_grace {
                let last_state =
                    ZoneRepo::with_backend(self.db.clone()).get_last_play_state(zone_id);
                if last_state.as_deref() == Some("playing") {
                    // Re-resolve the zone's REAL last-played track from the
                    // persisted state instead of a bogus "Recovering..."
                    // placeholder with track_id = None. That placeholder (a) showed
                    // as a phantom track "that corresponds to nothing" and (b) made
                    // resume() replay a track it couldn't play, so pressing play
                    // after a zone switch did nothing (#729).
                    let zone = ZoneRepo::with_backend(self.db.clone())
                        .get(zone_id)
                        .ok()
                        .flatten();
                    let last_track_id = zone.as_ref().and_then(|z| z.last_track_id);
                    let last_source = zone
                        .as_ref()
                        .and_then(|z| z.last_track_source.clone())
                        .unwrap_or_else(|| "local".into());
                    let last_source_id = zone.as_ref().and_then(|z| z.last_track_source_id.clone());
                    // Prefer the persisted local track's real metadata; fall back
                    // to what the device reports. Never invent a placeholder title.
                    let db_track = last_track_id.and_then(|tid| {
                        crate::db::track_repo::TrackRepo::with_backend(self.db.clone())
                            .get(tid)
                            .ok()
                            .flatten()
                    });
                    let title = db_track
                        .as_ref()
                        .map(|t| t.title.clone())
                        .or_else(|| status.track_title.clone());

                    // #2991 — ce chemin posait `stream_id: None` en dur. Sur une
                    // RADIO, ce `None` se recopie ensuite dans chaque
                    // now-playing produit par `refresh_radio_metadata`, et le
                    // titre cesse définitivement d'être publié vers le flux :
                    // l'écran du lecteur réseau reste figé sur le premier
                    // morceau pendant que l'interface Tune, elle, suit.
                    //
                    // Le renderer annonce l'URI qu'il tire ; on y RELIT
                    // l'identifiant, puis on ne l'adopte que si le gestionnaire
                    // de flux connaît cette session — `streamer_bytes_sent`
                    // rend `None` pour un flux inconnu, ce qui écarte l'URI
                    // d'un AUTRE serveur Tune du réseau.
                    let stream_id_repris =
                        match decisions::stream_id_de_l_uri(status.current_uri.as_deref()) {
                            Some(sid)
                                if self.orchestrator.streamer_bytes_sent(&sid).await.is_some() =>
                            {
                                Some(sid)
                            }
                            _ => None,
                        };
                    // Only recover if we actually know what is playing — otherwise
                    // skip so a titleless device blip never surfaces as a phantom.
                    if let Some(title) = title {
                        let np = crate::playback::NowPlaying {
                            track_id: last_track_id,
                            title,
                            artist_name: db_track
                                .as_ref()
                                .and_then(|t| t.artist_name.clone())
                                .or_else(|| status.track_artist.clone()),
                            album_title: db_track.as_ref().and_then(|t| t.album_title.clone()),
                            cover_path: db_track.as_ref().and_then(|t| t.cover_path.clone()),
                            duration_ms: db_track
                                .as_ref()
                                .map(|t| t.duration_ms)
                                .unwrap_or(status.duration_ms as i64),
                            source: last_source,
                            source_id: last_source_id,
                            stream_id: stream_id_repris,
                            ..Default::default()
                        };
                        let stream_id_journal =
                            np.stream_id.as_deref().unwrap_or("absent").to_string();
                        self.playback.play(zone_id, np).await;
                        info!(
                            zone_id,
                            device = %device_id,
                            stream_id = %stream_id_journal,
                            "playback_recovered_from_device"
                        );
                    } else {
                        debug!(
                            zone_id,
                            device = %device_id,
                            "playback_recovery_skipped_unknown_track"
                        );
                    }
                } else {
                    debug!(
                        zone_id,
                        device = %device_id,
                        last_state = ?last_state,
                        "playback_recovery_skipped_not_tune_playback"
                    );
                }
            }
        }

        for zone_state in &states {
            if zone_state.state != PlayState::Playing {
                continue;
            }

            let zone_id = zone_state.zone_id;
            let device_id = match self.get_zone_device_id(zone_id) {
                Some(id) => id,
                // Pas de peripherique de sortie : une zone navigateur, par
                // conception (le client web tire `stream_url` lui-meme).
                //
                // Cette porte renvoyait tout le monde, et le rafraichissement
                // des metadonnees radio vit 400 lignes plus bas, dans le bloc
                // qui suit l'interrogation du peripherique. Consequence : sur
                // « Cet ordinateur », `fetch_radio_metadata` n'etait JAMAIS
                // appele. Pas d'echec, pas de trace — l'appel n'existait pas.
                //
                // Ce n'etait pas une regression : ca n'a jamais marche. D'ou
                // deux testeurs sur la meme station et la meme version avec
                // des resultats opposes (Jean Valjean sur une vraie sortie :
                // titre, interprete et paroles ; Bilou sur « Cet ordinateur » :
                // rien). Fil forum « Metadonnees radio disparues ? ».
                //
                // Le reste de la boucle (transport, gapless, fin de piste) n'a
                // effectivement rien a faire ici : on garde le `continue`.
                None => {
                    let ps = poll_states
                        .entry(zone_id)
                        .or_insert_with(|| ZonePollState::new(zone_state.track_generation));
                    // Meme etranglement que la zone avec peripherique : le tick
                    // est a la seconde, l'API de la station non.
                    if decisions::deviceless_radio_refresh_due(
                        zone_state.state == PlayState::Playing,
                        zone_state.now_playing.as_ref().map(|np| np.source.as_str()),
                        ps.last_radio_poll.elapsed(),
                        RADIO_POLL_INTERVAL_SECS,
                    ) {
                        ps.last_radio_poll = Instant::now();
                        self.refresh_radio_metadata(zone_id, zone_state).await;
                    }

                    // Zone navigateur : l'annonce « en écoute » que le
                    // démarrage a mise en attente part d'ICI, une fois
                    // constaté que l'onglet tire réellement le flux (#1998).
                    //
                    // Le démarrage ne peut pas trancher : sans périphérique de
                    // sortie, `output_sent` y vaut toujours faux, qu'on écoute
                    // ou non. Le seul fait observable est la consommation du
                    // flux, et elle n'apparaît qu'après coup — c'est
                    // exactement ce qu'une boucle de scrutation est là pour
                    // voir. L'orchestrateur ne fait rien tant qu'il n'a rien
                    // en attente pour ce flux, donc ce tick ne coûte qu'une
                    // comparaison sur toutes les autres zones.
                    if let Some(stream_id) = zone_state
                        .now_playing
                        .as_ref()
                        .and_then(|np| np.stream_id.as_deref())
                    {
                        self.orchestrator
                            .confirmer_lecture_navigateur(zone_id, stream_id)
                            .await;
                    }

                    // … et le versant symétrique : l'ABSENCE de preuve.
                    //
                    // #2657 a appris à cette branche à LIBÉRER l'annonce quand
                    // l'onglet tire le flux. Rien ne lui apprenait à RENONCER
                    // quand personne ne le tire : la zone restait « en
                    // lecture » pour toujours, barre de progression comprise,
                    // alors que le démarrage avait déjà renoncé à envoyer quoi
                    // que ce soit (`output_sent=false`, #2630).
                    if self.abandonner_lecture_sans_destination(zone_state).await {
                        poll_states.remove(&zone_id);
                    }
                    continue;
                }
            };

            let ps = poll_states
                .entry(zone_id)
                .or_insert_with(|| ZonePollState::new(zone_state.track_generation));

            // Detect track change: if the generation changed, the orchestrator
            // started a new track (via play() / play_from_queue / next / previous).
            // Reset all per-track poller state so stale values from the previous
            // track (peak_position, gapless flags, etc.) cannot cause false
            // gapless advances or premature track-end detection.
            //
            // Exception: if last_seek_at is recent (< 10s), this generation
            // change is from a seek (which recreates the stream), not a real
            // track change. In that case, preserve position state to avoid
            // the seek bar jumping back to 0.
            if ps.track_generation != zone_state.track_generation {
                let is_seek = zone_state
                    .last_seek_at
                    .map(|t| t.elapsed().as_secs() < 10)
                    .unwrap_or(false);

                if is_seek {
                    info!(
                        zone_id,
                        old_gen = ps.track_generation,
                        new_gen = zone_state.track_generation,
                        position_ms = zone_state.position_ms,
                        "poller_generation_changed_during_seek_preserving_position"
                    );
                } else {
                    info!(
                        zone_id,
                        old_gen = ps.track_generation,
                        new_gen = zone_state.track_generation,
                        "poller_track_generation_changed_resetting_state"
                    );
                    ps.last_position_ms = 0;
                    ps.peak_position_ms = 0;
                    ps.scrobbled_key = None;
                    ps.last_bytes_sent = 0;
                    ps.playing_stall_ticks = 0;
                    ps.stall_declines = 0;
                    ps.past_end_ticks = 0;
                    ps.track_started_at = Some(Instant::now());
                }
                ps.gapless_sent = false;
                ps.gapless_sent_at = None;
                ps.gapless_cooldown = 0;
                ps.stopped_ticks = 0;
                ps.track_generation = zone_state.track_generation;
                ps.tenue_etrangere_ticks = 0;
                ps.tenue_signalee = false;
                ps.track_loaded_at = Instant::now();
                ps.past_end_ticks = 0;
                ps.gapless_advance_pending = false;
                ps.gapless_stuck_ticks = 0;
                // Re-arm the DLNA poll-fail wall-clock fallback for the new track.
                ps.wall_clock_end_fired = false;
                // Le constat de depassement vaut pour UNE piste (#2493).
                ps.depassement_duree_ticks = 0;
                ps.depassement_duree_signale = false;
                // Force one gapless_arm_trace line at the start of the new track.
                ps.gapless_arm_logged = None;
                ps.gapless_dsd_skip_pos = None;
                ps.gapless_armed = None;
            }

            // Scrobble the current track once it has genuinely been listened past
            // the Last.fm threshold (50% or 4 min). Driven from a single place
            // that sees every track regardless of how it was reached (direct
            // play, gapless, prefetch) and uses the live position — unlike the old
            // play-start dispatch that scrobbled instantly on a skip and dropped
            // every prefetched track (Bilou, #1113). Radio and sub-30s / unknown
            // tracks are excluded by `should_dispatch_scrobble`. The latch is
            // keyed on the track's identity, so a gapless metadata advance —
            // which swaps now-playing WITHOUT bumping track_generation — re-arms
            // it for the new track (tracks 2, 4, 6… of an album, #1113).
            if zone_state.state == PlayState::Playing
                && let Some(np) = zone_state.now_playing.as_ref()
            {
                let key = decisions::scrobble_track_key(
                    zone_state.track_generation,
                    zone_state.queue_position,
                    np.track_id,
                    &np.title,
                    np.artist_name.as_deref(),
                );
                if decisions::should_dispatch_scrobble(
                    ps.scrobbled_key.as_deref(),
                    &key,
                    &np.source,
                    np.duration_ms,
                    zone_state.position_ms,
                ) {
                    self.orchestrator.dispatch_scrobble(
                        &np.title,
                        np.artist_name.as_deref(),
                        np.album_title.as_deref(),
                    );
                    ps.scrobbled_key = Some(key);
                }
            }

            if ps.backoff_remaining > 0 {
                ps.backoff_remaining -= 1;
                continue;
            }

            // Radio zones: throttle polling to every RADIO_POLL_INTERVAL_SECS.
            // Polling a DLNA renderer (especially DMP-A8) every second with 4
            // SOAP calls while it plays an infinite radio stream causes buffer
            // underruns, noise, and playback cuts.  Radio has no meaningful
            // position/duration — only transport state and metadata matter,
            // and those change slowly.
            let is_radio = zone_state
                .now_playing
                .as_ref()
                .map(|np| np.source == "radio")
                .unwrap_or(false);
            if is_radio
                && !decisions::radio_poll_due(
                    ps.last_radio_poll.elapsed(),
                    RADIO_POLL_INTERVAL_SECS,
                )
            {
                continue;
            }

            ps.total_polls += 1;
            let poll_start = Instant::now();

            // A push-based output that failed on its own thread reports it
            // here. Handle it before any status-based reasoning (Yacine,
            // 8 Aug 2026).
            //
            // Ce commentaire disait « les heuristiques de blocage plus bas
            // finiraient par arrêter la zone, en ~73 s ». MESURÉ FAUX sur
            // #3108 : `dlna_playing_stall_eligible` exige
            // `output_type == "dlna"`. Pour une sortie LOCALE, ce bloc n'est
            // pas le raccourci d'un filet plus lent — c'est le seul filet.
            // Une zone locale dont la position se fige n'est reprise par rien.
            {
                let output_arc = {
                    let outputs = self.outputs.lock().await;
                    match outputs.get(&device_id) {
                        Some(o) => o,
                        None => continue,
                    }
                };
                let failure = {
                    let output = output_arc.lock().await;
                    output.take_output_failure()
                };
                if let Some(msg) = failure {
                    warn!(
                        zone_id,
                        device = %device_id,
                        error = %msg,
                        "output_reported_failure_stopping_zone"
                    );
                    if let Some(ref bus) = self.event_bus {
                        // `fatal` tells the client this is not worth waiting
                        // out. It opens a 30 s grace window on every play so a
                        // slow HI-RES pre-transcode reads as "chargement…"
                        // rather than a failure (#1146) — but a device that
                        // refuses to open will never recover, and we now report
                        // it within a second, i.e. squarely inside that window.
                        // Without this flag the message would be swallowed and
                        // the user would be left with a spinner and nothing
                        // else — worse than the silence this whole change fixes.
                        bus.emit(
                            "zone.playback_error",
                            serde_json::json!({
                                "zone_id": zone_id,
                                "error": msg,
                                "fatal": true,
                            }),
                        );
                    }
                    poll_states.remove(&zone_id);
                    let device_id_ref = self.get_zone_device_id(zone_id);
                    self.orchestrator
                        .stop(zone_id, device_id_ref.as_deref())
                        .await;
                    continue;
                }
            }

            let status = {
                let output_arc = {
                    let outputs = self.outputs.lock().await;
                    match outputs.get(&device_id) {
                        Some(o) => o,
                        None => continue,
                    }
                };
                match get_status_with_signal_path_bounded(&output_arc, *STATUS_POLL_TIMEOUT).await {
                    Ok((s, signal_path, dsp_metrics)) => {
                        ps.consecutive_errors = 0;
                        // Clôture de panne (#2566) : muette si le sondage
                        // n'avait jamais cessé de répondre.
                        ps.journal.succes_lecture(zone_id, &device_id);
                        let latency = poll_start.elapsed().as_millis() as u32;
                        ps.last_latency_ms = latency;
                        if latency > ps.max_latency_ms {
                            ps.max_latency_ms = latency;
                        }
                        // Même report que sur le chemin « zone au repos » : un
                        // flux peut entrer ou sortir du DoP d'une piste à
                        // l'autre sans changement d'état de zone (#1735).
                        self.playback.set_dop_active(zone_id, s.dop_active).await;
                        self.playback
                            .set_output_signal_path(zone_id, signal_path)
                            .await;
                        self.playback
                            .set_output_dsp_metrics(zone_id, dsp_metrics)
                            .await;
                        s
                    }
                    Err(e) => {
                        ps.consecutive_errors = ps.consecutive_errors.saturating_add(1);
                        ps.total_errors += 1;
                        ps.backoff_remaining = 1u8 << ps.consecutive_errors.min(4);
                        // Les trois compteurs ci-dessus sont tenus AVANT, et le
                        // journal n'en touche aucun : une panne qui cesse
                        // d'être dite continue d'être comptée, donc le repli de
                        // fin de piste (`poll_failed_past_end`) et l'arrêt de
                        // zone décident sur exactement les mêmes chiffres
                        // qu'avant (#2566). Seul le volume du journal change :
                        // 1 ligne toutes les 17 s pour un appareil muet, sans
                        // fin, devient 5 lignes détaillées puis un récapitulatif
                        // aux paliers de doublement.
                        let backoff = ps.backoff_remaining;
                        ps.journal.echec_lecture(zone_id, &device_id, &e, backoff);

                        // Poll-fail end-of-track fallback for a DLNA renderer
                        // whose status poll errors outright (LMS UPnP bridge:
                        // GetPositionInfo SOAP fails). We get no state/position/
                        // duration, so end-of-track is decided purely on Tune's
                        // wall clock vs the queue-known duration. Guarded so a
                        // paused/stopped track (Tune not Playing), a mid-track
                        // blip (< a couple failures), a seek, or a re-fire can't
                        // false-advance. We can't remove the poll state here (it's
                        // borrowed), so a per-track latch prevents re-firing.
                        let is_dlna = all_zones
                            .iter()
                            .find(|z| z.id == Some(zone_id))
                            .and_then(|z| z.output_type.as_deref())
                            == Some("dlna");
                        let tune_playing = zone_state.state == PlayState::Playing;
                        let track_duration_ms = zone_state
                            .now_playing
                            .as_ref()
                            .map(|np| np.duration_ms as u64)
                            .unwrap_or(0);
                        let wall_elapsed = ps
                            .track_started_at
                            .map(|t| t.elapsed().as_secs())
                            .unwrap_or(0);
                        let in_seek_grace = zone_state
                            .last_seek_at
                            .map(|t| t.elapsed().as_secs() < SEEK_STREAMING_GRACE_SECS)
                            .unwrap_or(false);
                        if !in_seek_grace
                            && decisions::poll_failed_past_end(
                                is_dlna,
                                tune_playing,
                                track_duration_ms,
                                wall_elapsed,
                                ps.consecutive_errors,
                                ps.wall_clock_end_fired,
                            )
                        {
                            info!(
                                zone_id,
                                device = %device_id,
                                track_dur = track_duration_ms,
                                wall_secs = wall_elapsed,
                                consec_err = ps.consecutive_errors,
                                "dlna_poll_failed_wall_clock_advancing"
                            );
                            ps.wall_clock_end_fired = true;
                            self.handle_track_end(zone_id, zone_state).await;
                        }
                        continue;
                    }
                }
            };

            // Update last_radio_poll so the throttle gate works on next tick.
            if is_radio {
                ps.last_radio_poll = Instant::now();
            }

            // Radio zones: after the throttled poll, only check transport
            // state (is it still playing?) and do metadata polling.
            // Skip position tracking, gapless logic, and track-end detection
            // — none of that applies to infinite streams.
            if is_radio {
                // A renderer that is truly streaming reports an advancing
                // position even when it (mis)reports Stopped for a live source
                // — the Yamaha R-N2000A does this on MP3 ICEcast streams (AAC
                // plays fine). Only treat the radio as stopped if it's Stopped
                // AND the position is NOT advancing; otherwise the auto-retry
                // below keeps restarting a stream the renderer is happily
                // playing (Cyrille: TSF Jazz / Radio Classique cut every ~45s).
                let radio_position_advancing = status.position_ms > ps.last_radio_position_ms;
                ps.last_radio_position_ms = status.position_ms;
                let radio_stopped =
                    status.state == TransportState::Stopped && !radio_position_advancing;

                if !radio_stopped {
                    ps.radio_stopped_ticks = 0;
                    // Still playing — sync volume only.
                    let zone_fixed_volume = all_zones
                        .iter()
                        .find(|z| z.id == Some(zone_id))
                        .map(|z| z.fixed_volume)
                        .unwrap_or(false);
                    let in_vol_grace = zone_state
                        .last_volume_set_at
                        .is_some_and(|t| t.elapsed().as_secs() < VOLUME_GRACE_SECS);
                    // Edge-triggered like the main volume-sync path, so a radio
                    // renderer reporting a stale default can't keep resetting the
                    // saved volume (Fabien's Devialet Salon reverting to 50).
                    if !zone_fixed_volume
                        && !in_vol_grace
                        && status.volume < 0.999
                        && decisions::should_adopt_device_volume(
                            ps.last_device_volume,
                            status.volume,
                            zone_state.volume,
                        )
                    {
                        self.playback.set_volume(zone_id, status.volume).await;
                        // #2886 — `as i32` TRONQUAIT : le volume adopte du renderer
                        // tombait a 0 sous 0,01 lineaire (-40 dB).
                        let vol_pct = status.volume * 100.0;
                        let db = self.db.clone();
                        crate::db::zone_repo::ZoneRepo::with_backend(db)
                            .update_volume(zone_id, vol_pct)
                            .ok();
                    }
                    ps.last_device_volume = Some(status.volume);
                }

                // Le titre diffuse par une webradio NE DEPEND PAS de l'etat du
                // renderer : il se lit sur une API externe (Radio Paradise,
                // Radio France) ou dans le flux ICY, tous deux independants de
                // ce que fait l'appareil. Rien ne justifiait de conditionner
                // cette lecture a la bonne sante du transport.
                //
                // C'etait pourtant le cas : l'appel vivait dans le
                // `if !radio_stopped` ci-dessus, aux cotes de la synchro de
                // volume — qui, elle, a bien besoin d'un renderer en lecture.
                // Consequence : un renderer qui ne demarre pas figeait
                // l'affichage sur le nom de la station, et un bug de LECTURE se
                // deguisait en bug de METADONNEES. Bilou a ouvert deux fils
                // distincts pour un seul probleme (#1522, #1492).
                //
                // La garde sur le peripherique de sortie, elle, est tombee
                // avec #1536.
                self.refresh_radio_metadata(zone_id, zone_state).await;

                // Sync metrics and skip the rest of the loop (no gapless/track-end).
                self.shared_metrics.lock().await.insert(
                    zone_id,
                    ZonePollerMetrics {
                        total_polls: ps.total_polls,
                        total_errors: ps.total_errors,
                        consecutive_errors: ps.consecutive_errors,
                        last_latency_ms: ps.last_latency_ms,
                        max_latency_ms: ps.max_latency_ms,
                        // Chemin RADIO : un flux sans fin ne depasse aucune
                        // duree, et ce bras n'evalue meme pas le predicat
                        // (#2493). Constat toujours faux, par construction.
                        lecture_au_dela_de_la_duree: false,
                    },
                );

                if radio_stopped {
                    ps.radio_stopped_ticks = ps.radio_stopped_ticks.saturating_add(1);
                    if ps.radio_stopped_ticks >= 3 && ps.radio_stopped_ticks < 6 {
                        if zone_state.track_generation != ps.track_generation {
                            debug!(zone_id, "radio_auto_retry_skipped_generation_changed");
                            ps.radio_stopped_ticks = 0;
                        } else {
                            info!(zone_id, ticks = ps.radio_stopped_ticks, "radio_auto_retry");
                            let device_id_ref = self.get_zone_device_id(zone_id);
                            if let Some(ref did) = device_id_ref {
                                if let Some(ref np) = zone_state.now_playing {
                                    if let Some(ref sid) = np.source_id {
                                        let req = crate::orchestrator::PlayRequest {
                                            zone_id,
                                            output_device_id: Some(did.clone()),
                                            track_id: None,
                                            source: Some("radio".into()),
                                            source_id: Some(sid.clone()),
                                            title: Some(np.title.clone()),
                                            artist_name: np.artist_name.clone(),
                                            album_title: np.album_title.clone(),
                                            cover_url: np.cover_path.clone(),
                                            duration_ms: None,
                                            seek_ms: None,
                                            temp_file_path: None,
                                            sample_rate: None,
                                            bit_depth: None,
                                            media_format: None,
                                            track_number: None,
                                            disc_number: None,
                                        };
                                        // Reconnecting the *same* station — do
                                        // not add a duplicate listen-history row.
                                        match self.orchestrator.play_without_history(req).await {
                                            Ok(_) => {
                                                info!(zone_id, "radio_auto_retry_success");
                                                ps.radio_stopped_ticks = 0;
                                            }
                                            Err(e) => {
                                                warn!(zone_id, error = %e, "radio_auto_retry_failed")
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else if ps.radio_stopped_ticks >= 6 {
                        info!(
                            zone_id,
                            ticks = ps.radio_stopped_ticks,
                            "radio_renderer_stopped_giving_up"
                        );
                        poll_states.remove(&zone_id);
                        let device_id_ref = self.get_zone_device_id(zone_id);
                        self.orchestrator
                            .stop(zone_id, device_id_ref.as_deref())
                            .await;
                    } else {
                        debug!(
                            zone_id,
                            ticks = ps.radio_stopped_ticks,
                            "radio_transient_stopped_tolerating"
                        );
                    }
                }
                continue;
            }

            // Check whether we're in the seek grace period: after a seek the
            // in-memory position is authoritative and the output may still
            // report the old (pre-seek) position until the stream restarts.
            // During this window we skip overwriting position to prevent the
            // progress bar from snapping back.
            //
            // For streaming sources (Qobuz/Tidal) on network outputs (DLNA),
            // seeking recreates the entire stream session — the renderer may
            // report Stopped for several seconds while it buffers the new
            // stream.  Use a longer grace period to prevent the poller from
            // accumulating stopped_ticks and false-skipping to the next track.
            let is_streaming_seek = zone_state.now_playing.as_ref().is_some_and(|np| {
                np.source != "local"
                    && np.source != "radio"
                    && np.source != "podcast"
                    && np.stream_id.is_some()
            }) && all_zones
                .iter()
                .find(|z| z.id == Some(zone_id))
                .and_then(|z| z.output_type.as_deref())
                .is_some_and(|t| {
                    matches!(
                        t,
                        "dlna" | "openhome" | "chromecast" | "bluos" | "squeezebox"
                    )
                });
            let seek_grace_secs = if is_streaming_seek {
                SEEK_STREAMING_GRACE_SECS
            } else {
                SEEK_GRACE_SECS
            };
            let in_seek_grace = zone_state
                .last_seek_at
                .map(|t| t.elapsed().as_secs() < seek_grace_secs)
                .unwrap_or(false);

            // Fold a NEW seek into the wall-clock baseline: rewind
            // track_started_at by the seek target so wall_elapsed reads as if
            // the track had played at 1x from position 0. Without this, a
            // seek near the end leaves wall_elapsed at a few seconds, and the
            // anti-spurious guards (played_enough, ended_naturally_wall_ok)
            // veto the REAL track end — playback stops instead of advancing
            // to the next track (DEvir, v0.9.0-rc4). Instant identity makes
            // this once-per-seek; play() clears last_seek_at on track change.
            if let Some(seek_at) = zone_state.last_seek_at {
                if ps.last_seek_seen != Some(seek_at) {
                    ps.last_seek_seen = Some(seek_at);
                    let target = Duration::from_millis(zone_state.position_ms.max(0) as u64);
                    ps.track_started_at =
                        Instant::now().checked_sub(target).or(ps.track_started_at);
                }
            }

            if !in_seek_grace {
                // Clamp the reported position to the track duration so the UI
                // progress bar doesn't briefly overshoot past the end. The
                // output can report a position a few seconds past the duration
                // during the past-end / gapless window before the track
                // advances (reported by DEvir). Internal poller logic keeps
                // using the raw status.position_ms (peak, past-end detection).
                let dur = zone_state
                    .now_playing
                    .as_ref()
                    .map(|np| np.duration_ms as i64)
                    .unwrap_or(0);
                let reported = if dur > 0 {
                    (status.position_ms as i64).min(dur)
                } else {
                    status.position_ms as i64
                };
                // La garde de monotonie vit dans `update_position` : elle sait,
                // elle, quels chemins ont le droit d'abaisser le plancher (une
                // COMMANDE — déplacement, changement de piste, avance gapless —
                // et jamais une observation). Elle rend la position RETENUE, et
                // c'est celle-là qu'il faut émettre : émettre `reported` ferait
                // diverger l'évènement `position` de l'état servi par
                // `GET /zones`, et l'écran reculerait quand même (#3229).
                let publiee = self.playback.update_position(zone_id, reported).await;
                self.playback.emit_position(zone_id, publiee);
            }

            // Sync volume from device (skip if fixed_volume)
            let zone_fixed_volume = all_zones
                .iter()
                .find(|z| z.id == Some(zone_id))
                .map(|z| z.fixed_volume)
                .unwrap_or(false);
            let in_vol_grace2 = zone_state
                .last_volume_set_at
                .is_some_and(|t| t.elapsed().as_secs() < VOLUME_GRACE_SECS);
            if !zone_fixed_volume
                && !in_vol_grace2
                && status.volume > 0.001
                && status.volume < 0.999
                && decisions::should_adopt_device_volume(
                    ps.last_device_volume,
                    status.volume,
                    zone_state.volume,
                )
            {
                self.playback.set_volume(zone_id, status.volume).await;
                // #2886 — `as i32` TRONQUAIT : le volume adopte du renderer
                // tombait a 0 sous 0,01 lineaire (-40 dB).
                let vol_pct = status.volume * 100.0;
                let db = self.db.clone();
                crate::db::zone_repo::ZoneRepo::with_backend(db)
                    .update_volume(zone_id, vol_pct)
                    .ok();
            }
            // Edge-triggered like the stopped/radio paths: record the reported
            // volume so a renderer stuck at a persistent default (HiFi Rose
            // RS130 reporting 25) can't repeatedly clobber the saved volume.
            // This normal-playing path was never migrated to the #358 predicate
            // and still used level-triggered adoption → auto-reset to 25%
            // (Philippe).
            ps.last_device_volume = Some(status.volume);

            // --- Persist position to DB periodically ---
            ps.ticks_since_db_save += 1;
            if ps.ticks_since_db_save >= POSITION_SAVE_INTERVAL_TICKS {
                ps.ticks_since_db_save = 0;
                let np = zone_state.now_playing.as_ref();
                let track_id = np.and_then(|n| n.track_id);
                let source = np.map(|n| n.source.as_str());
                let source_id = np.and_then(|n| n.source_id.as_deref());
                // Don't persist a position within END_MARGIN of the end — a
                // resume there would seek into the end zone and (on exclusive
                // outputs) bounce via the past-end detector. See
                // decisions::position_to_persist.
                let dur_ms = np.map(|n| n.duration_ms as i64).unwrap_or(0).max(0) as u64;
                let save_position_ms =
                    decisions::position_to_persist(status.position_ms, dur_ms) as i64;
                ZoneRepo::with_backend(self.db.clone())
                    .save_playback_position(zone_id, save_position_ms, track_id, source, source_id)
                    .ok();
            }

            // Track the high-water mark for position — used to verify that
            // Discard provably-stale early samples BEFORE they poison anything:
            // some renderers report the previous session's position for the
            // first seconds after a fresh Play (DMP-A6 → near-end staging at
            // +6s then phantom position_reset advance). Skip the whole
            // position-driven logic for this tick; the next honest sample
            // resumes it, and past 30s of wall time the guard stands down.
            let wall_elapsed_now = ps
                .track_started_at
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            // A non-realtime output is exempt: "position far ahead of the wall
            // clock" is a ghost only if playback runs at 1x. A recorder that
            // finished the capture reports position = duration straight away, so
            // this `continue` skipped the whole end-of-track path on every tick
            // until the wall clock caught up with the track's length.
            if status.realtime
                && decisions::stale_start_position(wall_elapsed_now, status.position_ms)
            {
                debug!(
                    zone_id,
                    pos_ms = status.position_ms,
                    wall_s = wall_elapsed_now,
                    "stale_start_position_ignored"
                );
                continue;
            }

            // enough of the track was actually played before accepting a
            // gapless transition.  We update this BEFORE checking for resets
            // so the peak reflects the last known good position.
            if status.position_ms > ps.peak_position_ms {
                ps.peak_position_ms = status.position_ms;
            }

            let track_duration_ms = zone_state
                .now_playing
                .as_ref()
                .map(|np| np.duration_ms as u64)
                .unwrap_or(0);

            // Helper: has enough of the track been played?
            // When track_duration is known: peak_position_ms >= 80% of duration.
            // When track_duration is unknown (0): require peak_position_ms >= 60s
            // to avoid false skips on slow renderers (Shanling SCD1.3 etc.)
            // that report duration=0 and briefly show Stopped while buffering.
            let wall_elapsed = ps
                .track_started_at
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            let played_enough =
                decisions::played_enough(track_duration_ms, ps.peak_position_ms, wall_elapsed);

            // Detect position reset: position drops from >30s to <5s.
            // This is a strong signal that the renderer performed a gapless
            // transition (the new track starts from 0).
            //
            // Arm on `gapless_sent` (the boolean), NOT `gapless_sent_at`
            // (the timestamp). SetNextAVTransportURI is sent GAPLESS_WINDOW_MS
            // (30s) before the end, but `gapless_sent_at` expires after
            // GAPLESS_GUARD_SECS (15s) — i.e. ~15s BEFORE the track actually
            // ends. Using the timestamp therefore disarmed this detection for
            // the entire final stretch of every track, so a renderer that
            // transitions seamlessly (continuous Playing, no Stopped blip) and
            // plays consecutive tracks of similar duration was caught by
            // NEITHER position_reset (disarmed) NOR the duration_changed path
            // (needs >2s duration difference). Tune then stayed one track
            // behind: its UI showed track N restarting while the renderer
            // played track N+1, and when the phantom track N passed its
            // duration handle_track_end re-issued play → track N+1 restarted on
            // the renderer (forum #1019, Marantz ND8006). `gapless_sent` stays
            // true from SetNext until the transition is detected or the track
            // generation changes, so it covers the whole window.
            // Snapshot the real previous position BEFORE it is overwritten
            // below — the diagnostic `info!` further down must log the genuine
            // prior sample, not the just-stored current one (the old code read
            // `ps.last_position_ms` after the overwrite, so `prev_pos` was
            // always mis-logged equal to `new_pos`).
            let prev_position_ms = ps.last_position_ms;
            let mut position_reset =
                decisions::position_reset(ps.last_position_ms, status.position_ms, ps.gapless_sent);
            // Suppress this metadata-only advance fallback for outputs that don't
            // do internal gapless (Chromecast, slimproto, exclusive local): for
            // them a position drop to 0 means the track ENDED (device IDLE /
            // FINISHED), not that it auto-advanced. Firing here sends no `play`
            // and steals the event from the natural-end path (Stopped branch →
            // play_from_queue = real load), causing Rhorn's 1-2s-then-zero loop
            // (#1072). Compute can_internal_gapless only when a raw reset fires
            // (rare: end of track). Env-guarded for rollback.
            if position_reset && std::env::var("TUNE_DISABLE_CAST_ADVANCE_FIX").is_err() {
                let can_internal_gapless = {
                    let outputs = self.outputs.lock().await;
                    match outputs.get(&device_id) {
                        Some(arc) => arc.lock().await.supports_internal_gapless(),
                        None => false,
                    }
                };
                position_reset = decisions::position_reset_fires(
                    position_reset,
                    can_internal_gapless,
                    in_seek_grace,
                );
                if !position_reset {
                    if in_seek_grace {
                        info!(zone_id, "gapless_advance_suppressed_after_seek");
                    } else {
                        info!(zone_id, "position_reset_deferred_to_natural_end");
                    }
                }
            }
            ps.last_position_ms = status.position_ms;

            if position_reset {
                if !played_enough {
                    warn!(
                        zone_id,
                        peak_pos = ps.peak_position_ms,
                        track_dur = track_duration_ms,
                        "gapless_position_reset_ignored_not_enough_played"
                    );
                } else {
                    // Real time elapsed between arming SetNext (next-track URL
                    // resolved) and the renderer actually transitioning. Key
                    // metric for streaming URL/token expiry diagnosis.
                    let arm_to_advance_ms = ps
                        .gapless_sent_at
                        .map(|t| t.elapsed().as_millis() as u64)
                        .unwrap_or(0);
                    info!(
                        zone_id,
                        prev_pos = prev_position_ms,
                        new_pos = status.position_ms,
                        arm_to_advance_ms,
                        "gapless_position_reset_detected"
                    );
                    ps.gapless_sent = false;
                    ps.gapless_sent_at = None;
                    // Retire de l'etat, gardee sous la main : c'est elle qui
                    // dit ou avancer (#3026).
                    let arme_avant = ps.gapless_armed.take();
                    ps.stopped_ticks = 0;
                    ps.past_end_ticks = 0;
                    ps.peak_position_ms = 0;
                    ps.last_position_ms = 0;
                    ps.last_bytes_sent = 0;
                    ps.playing_stall_ticks = 0;
                    ps.stall_declines = 0;
                    ps.track_started_at = Some(Instant::now());
                    ps.gapless_advance_pending = false;
                    ps.gapless_stuck_ticks = 0;
                    // A stall-recovery restart (OAAT stall supervisor) replays
                    // the CURRENT track from 0. That from-zero position drop
                    // trips `position_reset` exactly like a real gapless
                    // transition — but the renderer is still on the SAME track,
                    // so advancing would run now-playing one track ahead of the
                    // audio ("ça avance mais joue le morceau précédent", Xavier,
                    // OAAT Tune Endpoint). Suppress the advance for a brief
                    // window after a restart; the state resets above still run,
                    // so the next genuine transition re-arms and advances
                    // normally.
                    let recently_restarted = zone_state
                        .last_restart_at
                        .map(|t| {
                            t.elapsed()
                                < std::time::Duration::from_secs(RESTART_ADVANCE_SUPPRESS_SECS)
                        })
                        .unwrap_or(false);
                    if recently_restarted {
                        info!(zone_id, "gapless_advance_suppressed_after_restart");
                    } else if let Some(next_pos) = self
                        .position_a_avancer(zone_id, zone_state, arme_avant)
                        .await
                    {
                        info!(zone_id, next_pos, "gapless_advance_on_position_reset");
                        if let Err(e) = self
                            .orchestrator
                            .advance_queue_metadata(zone_id, next_pos)
                            .await
                        {
                            warn!(zone_id, error = %e, "gapless_advance_failed");
                        }
                        ps.gapless_cooldown = 4;
                        // The identity-keyed latch re-arms by itself on the new
                        // track; clearing it here additionally covers gapless
                        // repeat-one, where the advanced track has the same
                        // identity as the latched one (#1113).
                        ps.scrobbled_key = None;
                    }
                }
            }

            // Clear expired guard
            if let Some(sent_at) = ps.gapless_sent_at {
                if sent_at.elapsed() > std::time::Duration::from_secs(GAPLESS_GUARD_SECS) {
                    debug!(zone_id, "gapless_guard_expired");
                    ps.gapless_sent_at = None;
                }
            }

            let in_gapless_guard = ps.gapless_sent_at.is_some();

            let mut track_ended = false;
            // Quelle branche a conclu « la piste est finie ». Journalisé tel
            // quel par `track_end_gap` au moment d'enchaîner (#2488) : sans
            // lui, le journal ne dit pas laquelle des cinq portes de sortie a
            // servi, et donc pas quel plancher de silence a été payé.
            let mut motif_fin_de_piste: &'static str = "";
            let mut force_stop = false;
            let mut force_stop_demarrage_mort = false;

            // Guard: if Tune's own playback state for this zone is Stopped
            // (or has no now_playing), ignore device state changes entirely.
            // This prevents phantom playback when another app (e.g. Roon)
            // plays on a shared renderer (e.g. Sonos) and then stops —
            // Tune would otherwise interpret the Stopped→Playing cycle as
            // its own track ending and auto-advance to the next queue item.
            let tune_is_playing =
                zone_state.state == PlayState::Playing || zone_state.state == PlayState::Paused;
            let tune_has_track = zone_state.now_playing.is_some();

            match status.state {
                TransportState::Stopped if !tune_is_playing || !tune_has_track => {
                    // Tune is not playing on this zone — ignore device Stopped.
                    ps.stopped_ticks = 0;
                    ps.playing_stall_ticks = 0;
                }
                TransportState::Stopped => {
                    ps.playing_stall_ticks = 0;
                    // During the seek grace period, the renderer may report
                    // Stopped while it buffers the new stream (especially for
                    // streaming seeks that recreate the session).  Suppress
                    // stopped_ticks to prevent false track-end detection.
                    // Not for a non-realtime output. The grace exists to let a
                    // renderer buffer, and its `peak < 5s` condition reads "no
                    // audio has come out yet" — but a recorder that finished the
                    // whole capture in two seconds legitimately never reports a
                    // position past 5s, so the grace held every track back for
                    // its full 45s and a rip crawled at ~46s per track.
                    let in_track_load_grace = status.realtime
                        && ps.track_loaded_at.elapsed().as_secs() < TRACK_LOAD_GRACE_SECS
                        && ps.peak_position_ms < 5_000;
                    // A DSD track on a DLNA renderer whose peak reached the end.
                    // Gapless is intentionally not armed for a DSD next on DLNA
                    // (prepare_gapless / #402) and DLNA never reports
                    // ended_naturally, so without this fast path the track only
                    // ends after STOPPED_TICKS_THRESHOLD polls — a fixed ~5s gap
                    // between DSD tracks (Benjithom, RS130).
                    let dlna_dsd_reached_end = decisions::dlna_dsd_reached_end(
                        all_zones
                            .iter()
                            .find(|z| z.id == Some(zone_id))
                            .and_then(|z| z.output_type.as_deref())
                            .unwrap_or(""),
                        zone_state
                            .now_playing
                            .as_ref()
                            .and_then(|np| np.format.as_deref()),
                        track_duration_ms,
                        ps.peak_position_ms,
                    );
                    // v0.9 rc.2 FSM shadow: snapshot the Stopped-arm inputs
                    // (pre-mutation) so classify_stopped can be compared to the
                    // arm's real outcome under TUNE_POLLER_FSM_SHADOW. Cheap
                    // (no I/O); the compare/log at the arm tail is flag-gated.
                    let mut fsm_in = fsm::StoppedInput {
                        tune_is_playing,
                        tune_has_track,
                        in_seek_grace,
                        in_track_load_grace,
                        gapless_cooldown: ps.gapless_cooldown,
                        in_gapless_guard,
                        played_enough,
                        gapless_advance_pending: ps.gapless_advance_pending,
                        gapless_stuck_ticks: ps.gapless_stuck_ticks,
                        ended_naturally: status.ended_naturally,
                        wall_elapsed,
                        track_duration_ms,
                        stopped_ticks: ps.stopped_ticks,
                        natural_end: decisions::natural_end(
                            played_enough,
                            matches!(zone_state.repeat, RepeatMode::One | RepeatMode::All),
                            ps.peak_position_ms,
                            status.ended_naturally,
                            wall_elapsed,
                            track_duration_ms,
                            status.realtime,
                        ),
                        gapless_sent: ps.gapless_sent,
                        realtime: status.realtime,
                        // Refined by the natural-end branch below (live probe),
                        // same late-update pattern as `consommation`.
                        can_internal_gapless: true,
                        // Rien n'a encore été mesuré sur ce tour : « inconnue »
                        // est le seul départ honnête (#2394). La branche du
                        // seuil d'échec, seule à interroger le gestionnaire de
                        // flux, la remplace par un verdict mesuré.
                        consommation: fsm::ConsommationFlux::Inconnue,
                        dlna_dsd_reached_end,
                    };
                    let mut fsm_actual: Option<fsm::StoppedOutcome>;
                    if in_seek_grace {
                        fsm_actual = Some(fsm::StoppedOutcome::SuppressSeekGrace);
                        ps.stopped_ticks = 0;
                        debug!(
                            zone_id,
                            seek_grace_secs = seek_grace_secs,
                            "seek_grace_suppressing_stopped_ticks"
                        );
                    } else if in_track_load_grace {
                        fsm_actual = Some(fsm::StoppedOutcome::SuppressLoadGrace);
                        ps.stopped_ticks = 0;
                        debug!(
                            zone_id,
                            elapsed = ps.track_loaded_at.elapsed().as_secs(),
                            grace = TRACK_LOAD_GRACE_SECS,
                            "track_load_grace_suppressing_stopped_ticks"
                        );
                    } else if ps.gapless_cooldown > 0 {
                        fsm_actual = Some(fsm::StoppedOutcome::SuppressCooldown);
                        ps.gapless_cooldown -= 1;
                        ps.stopped_ticks = 0;
                    } else if in_gapless_guard {
                        if !played_enough {
                            fsm_actual = Some(fsm::StoppedOutcome::GuardStoppedIgnored);
                            // Renderer reported Stopped during guard but not
                            // enough of the track was played — ignore to avoid
                            // false skip (DMP-A8 quirk).
                            debug!(
                                zone_id,
                                peak_pos = ps.peak_position_ms,
                                track_dur = track_duration_ms,
                                "gapless_guard_stopped_ignored_not_enough_played"
                            );
                        } else {
                            fsm_actual = Some(fsm::StoppedOutcome::GuardStoppedPending);
                            // During the gapless guard period, a Stopped state
                            // MAY mean the renderer transitioned via gapless.
                            // Don't advance metadata yet — wait for the renderer
                            // to report Playing (position reset) to confirm.
                            // If it stays Stopped, the stuck handler will force
                            // play_from_queue which handles metadata correctly.
                            info!(zone_id, "gapless_guard_stopped_pending_confirmation");
                            ps.gapless_sent = false;
                            ps.gapless_armed = None;
                            ps.gapless_sent_at = None;
                            ps.stopped_ticks = 0;
                            ps.peak_position_ms = 0;
                            ps.last_position_ms = 0;
                            ps.track_started_at = None;
                            ps.gapless_advance_pending = true;
                            ps.gapless_stuck_ticks = 0;
                            ps.gapless_cooldown = 4;
                        }
                    } else if ps.gapless_advance_pending {
                        // The poller advanced metadata expecting the renderer
                        // to auto-transition via gapless, but the renderer is
                        // still Stopped after the cooldown expired.  Count
                        // stuck ticks and force play_from_queue if the renderer
                        // doesn't pick up within GAPLESS_STUCK_THRESHOLD.
                        ps.gapless_stuck_ticks += 1;
                        if ps.gapless_stuck_ticks >= GAPLESS_STUCK_THRESHOLD {
                            fsm_actual = Some(fsm::StoppedOutcome::StuckForceEnd);
                            warn!(
                                zone_id,
                                stuck_ticks = ps.gapless_stuck_ticks,
                                "gapless_advance_stuck_forcing_play"
                            );
                            ps.gapless_advance_pending = false;
                            ps.gapless_stuck_ticks = 0;
                            ps.stopped_ticks = 0;
                            track_ended = true;
                            motif_fin_de_piste = decisions::motif_fin::AVANCE_GAPLESS_BLOQUEE;
                        } else {
                            fsm_actual = Some(fsm::StoppedOutcome::StuckWaiting);
                            debug!(
                                zone_id,
                                stuck_ticks = ps.gapless_stuck_ticks,
                                threshold = GAPLESS_STUCK_THRESHOLD,
                                "gapless_advance_waiting_for_renderer"
                            );
                        }
                    } else if status.ended_naturally
                        && (played_enough
                            || decisions::peak_reached_end(track_duration_ms, ps.peak_position_ms)
                            || decisions::ended_naturally_wall_ok(wall_elapsed, track_duration_ms))
                    {
                        fsm_actual = Some(fsm::StoppedOutcome::LocalEndedNaturally);
                        // Local outputs (WASAPI/ALSA/CoreAudio) signal
                        // ended_naturally when the audio stream reaches EOF.
                        // Skip the STOPPED_TICKS_THRESHOLD wait — we know
                        // the track is done, no need to accumulate 5s of
                        // stopped ticks.
                        info!(
                            zone_id,
                            wall_elapsed,
                            peak_pos = ps.peak_position_ms,
                            "local_output_ended_naturally_advancing"
                        );
                        track_ended = true;
                        motif_fin_de_piste = decisions::motif_fin::FIN_NATURELLE_LOCALE;
                    } else if dlna_dsd_reached_end {
                        fsm_actual = Some(fsm::StoppedOutcome::DsdDlnaReachedEnd);
                        // A DSD track on a DLNA renderer: gapless is intentionally
                        // not armed (prepare_gapless / #402) and DLNA never sets
                        // ended_naturally, so the only remaining end-of-track
                        // signal is STOPPED_TICKS_THRESHOLD polls = a fixed ~5s
                        // gap between DSD tracks (Benjithom, RS130). The peak
                        // reaching the end proves the track finished — advance
                        // now, ~4s sooner.
                        info!(
                            zone_id,
                            peak_pos = ps.peak_position_ms,
                            track_dur = track_duration_ms,
                            "dlna_dsd_reached_end_advancing"
                        );
                        ps.stopped_ticks = 0;
                        track_ended = true;
                        motif_fin_de_piste = decisions::motif_fin::DSD_DLNA_PIC_ATTEINT;
                    } else {
                        // Default for this block; overridden by the natural-end
                        // and failure sub-branches below.
                        fsm_actual = Some(fsm::StoppedOutcome::Waiting);
                        ps.stopped_ticks += 1;
                        if ps.stopped_ticks >= STOPPED_TICKS_THRESHOLD {
                            // When repeat mode is active (One or All) on DLNA,
                            // be more lenient about accepting track-end: if the
                            // renderer has reported Stopped and we've seen any
                            // meaningful playback (peak > 5s), treat it as a
                            // natural end so the poller re-triggers play instead
                            // of accumulating stopped_ticks until force_stop.
                            // (DEvir QA B-05: repeat mode doesn't work on DLNA)
                            let repeat_active =
                                matches!(zone_state.repeat, RepeatMode::One | RepeatMode::All);
                            let natural_end = decisions::natural_end(
                                played_enough,
                                repeat_active,
                                ps.peak_position_ms,
                                status.ended_naturally,
                                wall_elapsed,
                                track_duration_ms,
                                status.realtime,
                            );
                            // Not a warning for a non-realtime output: finishing
                            // a track in under 5s is its normal mode, not a
                            // renderer misreporting the end.
                            if status.ended_naturally
                                && status.realtime
                                && wall_elapsed < 5
                                && !played_enough
                            {
                                warn!(
                                    zone_id,
                                    wall_elapsed,
                                    peak_pos = ps.peak_position_ms,
                                    track_dur = track_duration_ms,
                                    "ended_naturally_rejected_too_early"
                                );
                            }
                            if natural_end {
                                // Only DLNA renderers auto-transition after
                                // SetNextAVTransportURI. For exclusive local
                                // outputs (ASIO / WASAPI exclusive) the near-end
                                // branch sets gapless_sent=true only to suppress
                                // re-arming — no SetNext is ever sent — so the
                                // "wait for transition" path below would hang
                                // forever and repeat/advance would never fire
                                // (DEvir: repeat fails on clean ASIO playback).
                                // Only wait when the output can actually
                                // transition internally; otherwise end normally.
                                let can_internal_gapless = {
                                    let outputs = self.outputs.lock().await;
                                    match outputs.get(&device_id) {
                                        Some(arc) => arc.lock().await.supports_internal_gapless(),
                                        None => false,
                                    }
                                };
                                fsm_in.can_internal_gapless = can_internal_gapless;
                                let awaiting_dlna_transition =
                                    ps.gapless_sent && can_internal_gapless;
                                if awaiting_dlna_transition {
                                    fsm_actual =
                                        Some(fsm::StoppedOutcome::NaturalEndGaplessWaiting);
                                    // Gapless was prepared via SetNextAVTransportURI.
                                    // Don't advance metadata yet — wait for the
                                    // renderer to confirm the transition by starting
                                    // to play (position reset detected in the Playing
                                    // handler).  If it stays Stopped after the
                                    // cooldown + stuck threshold, fall through to
                                    // play_from_queue which handles metadata itself.
                                    info!(zone_id, "gapless_natural_end_waiting_for_transition");
                                    ps.gapless_sent = false;
                                    ps.gapless_armed = None;
                                    ps.gapless_sent_at = None;
                                    ps.stopped_ticks = 0;
                                    ps.peak_position_ms = 0;
                                    ps.last_position_ms = 0;
                                    ps.track_started_at = None;
                                    ps.gapless_advance_pending = true;
                                    ps.gapless_stuck_ticks = 0;
                                    ps.gapless_cooldown = 4;
                                } else {
                                    // Avant d'accepter cette fin : le renderer
                                    // a-t-il vraiment reçu le morceau ? Sur un
                                    // réseau qui hoquette il cale, annonce
                                    // Stopped, et on tronquait la fin en
                                    // silence. Les octets servis tranchent.
                                    let sid = zone_state
                                        .now_playing
                                        .as_ref()
                                        .and_then(|np| np.stream_id.clone());
                                    let (sent, total) = match sid.as_deref() {
                                        Some(sid) => (
                                            self.orchestrator
                                                .streamer_bytes_sent(sid)
                                                .await
                                                .unwrap_or(0),
                                            self.orchestrator.streamer_total_bytes(sid).await,
                                        ),
                                        None => (0, None),
                                    };
                                    let seeked = zone_state.last_seek_at.is_some();
                                    if decisions::renderer_could_have_finished(sent, total, seeked)
                                    {
                                        fsm_actual = Some(fsm::StoppedOutcome::NaturalEndAdvance);
                                        ps.gapless_sent = false;
                                        ps.gapless_armed = None;
                                        track_ended = true;
                                        motif_fin_de_piste =
                                            decisions::motif_fin::FIN_NATURELLE_APRES_STOPPED;
                                    } else if ps.stall_declines < STALL_DECLINE_MAX_TICKS {
                                        // On laisse au renderer le temps de
                                        // reprendre : s'il repart, il repassera
                                        // Playing et cette branche disparaît.
                                        ps.stall_declines = ps.stall_declines.saturating_add(1);
                                        if ps.stall_declines == 1 {
                                            warn!(
                                                zone_id,
                                                peak_pos = ps.peak_position_ms,
                                                track_dur = track_duration_ms,
                                                bytes_sent = sent,
                                                bytes_total = total.unwrap_or(0),
                                                "renderer_stopped_on_incomplete_stream_waiting"
                                            );
                                        }
                                    } else {
                                        // La lecture a échoué : on arrête la
                                        // zone bruyamment plutôt que d'avancer
                                        // en faisant croire à une fin normale.
                                        warn!(
                                            zone_id,
                                            peak_pos = ps.peak_position_ms,
                                            track_dur = track_duration_ms,
                                            bytes_sent = sent,
                                            bytes_total = total.unwrap_or(0),
                                            "renderer_stalled_not_advancing_stopping_zone"
                                        );
                                        track_ended = false;
                                        force_stop = true;
                                    }
                                }
                            } else if ps.stopped_ticks >= STOPPED_FAILURE_THRESHOLD {
                                // Check if the stream is still being consumed
                                // (renderer actively fetching audio data). If so,
                                // don't kill — the renderer is playing but not
                                // reporting state (DMP-A10, LHC, Shanling, etc.).
                                // #2394 — le compteur a le droit de ne pas
                                // savoir. `stream_id` absent et session
                                // inconnue du gestionnaire de flux rendaient
                                // tous deux `0`, indiscernable d'« aucun octet
                                // servi » ; et c'est ce chiffre qui arme
                                // `force_stop`. Voir `fsm::consommation_flux`.
                                let stream_id = zone_state
                                    .now_playing
                                    .as_ref()
                                    .and_then(|np| np.stream_id.clone());
                                let octets_servis: Option<u64> = match stream_id.as_deref() {
                                    Some(sid) => self.orchestrator.streamer_bytes_sent(sid).await,
                                    None => None,
                                };
                                let consommation =
                                    fsm::consommation_flux(octets_servis, ps.last_bytes_sent);
                                // Un compteur inconnu n'écrase pas le dernier
                                // compte MESURÉ : sinon la reprise du
                                // `stream_id` ferait repartir la comparaison
                                // depuis un faux zéro.
                                if let Some(octets) = octets_servis {
                                    ps.last_bytes_sent = octets;
                                }
                                fsm_in.consommation = consommation;

                                if consommation == fsm::ConsommationFlux::Consomme {
                                    fsm_actual = Some(fsm::StoppedOutcome::FailureWaitingConsuming);
                                    if ps.stopped_ticks % 30 == 0 {
                                        debug!(
                                            zone_id,
                                            peak_pos = ps.peak_position_ms,
                                            wall_secs = wall_elapsed,
                                            bytes_sent = octets_servis.unwrap_or(0),
                                            consommation = consommation.etiquette(),
                                            "dlna_renderer_not_reporting_state_waiting"
                                        );
                                    }
                                } else if consommation == fsm::ConsommationFlux::Inconnue {
                                    // On ne mesure RIEN — ce n'est pas « rien
                                    // servi ». Couper ici couperait une zone
                                    // qui joue (une avance gapless pose
                                    // `stream_id: None`, cf. #2991). On attend,
                                    // et on le DIT : un état invisible se
                                    // reconfondrait avec zéro.
                                    fsm_actual = Some(fsm::StoppedOutcome::FailureWaitingUnknown);
                                    if ps.stopped_ticks % 30 == 0 {
                                        warn!(
                                            zone_id,
                                            peak_pos = ps.peak_position_ms,
                                            track_dur = track_duration_ms,
                                            wall_secs = wall_elapsed,
                                            consommation = consommation.etiquette(),
                                            has_stream_id = stream_id.is_some(),
                                            "octets_servis_inconnus_zone_non_coupee"
                                        );
                                    }
                                } else {
                                    let current_bytes = octets_servis.unwrap_or(0);
                                    fsm_actual = Some(fsm::StoppedOutcome::FailureStop);
                                    warn!(
                                        zone_id,
                                        peak_pos = ps.peak_position_ms,
                                        track_dur = track_duration_ms,
                                        wall_secs = wall_elapsed,
                                        bytes_sent = current_bytes,
                                        consommation = consommation.etiquette(),
                                        "playback_failure_stopping_zone"
                                    );
                                    track_ended = false;
                                    force_stop = true;
                                    // « Démarrage mort » (#2394) : la piste n'a
                                    // JAMAIS été tirée (0 octet servi) sur un
                                    // renderer DLNA — le profil du pipeline
                                    // Eversolo coincé, qui acquitte le Play et
                                    // ouvre ses connexions sans rien lire. Une
                                    // relance (précédée de Pause→Stop, son
                                    // libérateur connu) passe alors presque
                                    // toujours à la main. Distinct d'un
                                    // décrochage EN COURS de lecture
                                    // (bytes_sent > 0), qu'on ne rejoue pas.
                                    //
                                    // On n'arrive ici qu'avec un compteur
                                    // MESURÉ (`ASec`) : le `0` d'ignorance ne
                                    // déclenche plus la relance automatique
                                    // Pause→Stop→Play sur une zone qui joue.
                                    force_stop_demarrage_mort = decisions::demarrage_mort(
                                        all_zones
                                            .iter()
                                            .find(|z| z.id == Some(zone_id))
                                            .and_then(|z| z.output_type.as_deref())
                                            .unwrap_or(""),
                                        current_bytes,
                                    );
                                }
                            } else {
                                debug!(
                                    zone_id,
                                    peak_pos = ps.peak_position_ms,
                                    track_dur = track_duration_ms,
                                    wall_secs = wall_elapsed,
                                    stopped_ticks = ps.stopped_ticks,
                                    unknown_dur_min_peak = if track_duration_ms == 0 {
                                        MIN_PEAK_UNKNOWN_DURATION_MS
                                    } else {
                                        0
                                    },
                                    "stopped_early_waiting"
                                );
                            }
                        }
                    }
                    // v0.9 rc.2 — FSM shadow-compare (flag-gated, log only).
                    if *POLLER_FSM_SHADOW {
                        if let Some(actual) = fsm_actual {
                            let predicted = fsm::classify_stopped(&fsm_in);
                            if predicted != actual {
                                warn!(zone_id, ?predicted, ?actual, "poller_fsm_shadow_divergence");
                            }
                        }
                    }
                }
                TransportState::Playing | TransportState::Transitioning => {
                    ps.stopped_ticks = 0;
                    ps.gapless_cooldown = 0;
                    // v0.9 rc.2 FSM shadow: snapshot the Playing-arm inputs
                    // (pre-mutation). gapless_enabled is filled in the arm branch
                    // when it is actually read; default true matches the arm.
                    let fsm_has_next = Self::next_position(zone_state).is_some();
                    let output_type_str = all_zones
                        .iter()
                        .find(|z| z.id == Some(zone_id))
                        .and_then(|z| z.output_type.as_deref())
                        .unwrap_or("");
                    let is_dlna = output_type_str == "dlna";
                    // Un flux de radio n'a pas de fin : sa position ne peut rien
                    // depasser (#2493).
                    let source_est_radio = zone_state
                        .now_playing
                        .as_ref()
                        .is_some_and(|np| np.source == "radio");
                    let mut fsm_pin = fsm::PlayingInput {
                        gapless_advance_pending: ps.gapless_advance_pending,
                        has_next: fsm_has_next,
                        gapless_sent: ps.gapless_sent,
                        track_duration_ms,
                        reported_duration_ms: status.duration_ms,
                        played_enough,
                        position_ms: status.position_ms,
                        past_end_ticks: ps.past_end_ticks,
                        gapless_enabled: true,
                        is_dlna,
                        wall_elapsed_secs: wall_elapsed,
                    };
                    let mut fsm_pact = fsm::PlayingDecision {
                        confirm_gapless_advance: ps.gapless_advance_pending && fsm_has_next,
                        ..Default::default()
                    };
                    // Renderer started playing — gapless transition confirmed.
                    // NOW advance metadata (deferred from the Stopped handler
                    // to avoid showing the wrong track on renderers that don't
                    // actually auto-transition via SetNextAVTransportURI).
                    if ps.gapless_advance_pending {
                        ps.gapless_advance_pending = false;
                        ps.gapless_stuck_ticks = 0;
                        if let Some(next_pos) = Self::next_position(zone_state) {
                            info!(zone_id, next_pos, "gapless_confirmed_advancing_metadata");
                            if let Err(e) = self
                                .orchestrator
                                .advance_queue_metadata(zone_id, next_pos)
                                .await
                            {
                                warn!(zone_id, error = %e, "gapless_confirmed_advance_failed");
                            }
                            ps.gapless_cooldown = 4;
                            // Identity-keyed latch re-arms on the new track;
                            // clearing also covers gapless repeat-one (#1113).
                            ps.scrobbled_key = None;
                        }
                    }
                    if ps.track_started_at.is_none() {
                        ps.track_started_at = Some(Instant::now());
                    }

                    // Instrumentation (#1239): trace the gapless arming window for
                    // a realtime renderer that has a next track (BluOS reports
                    // honest secs/totlen). Recomputes should_arm_gapless read-only
                    // — it drives no decision — and logs ONLY when the armed state
                    // flips (window opens/closes) or once per track (the latch is
                    // reset to None on track change), so it never spams the ~1 s
                    // tick. Goal: later confirm why the arming window fails to open
                    // after a /Add. `reason` explains the current arm gate.
                    if status.realtime && fsm_has_next {
                        let armed = decisions::should_arm_gapless(
                            ps.gapless_sent,
                            status.duration_ms,
                            track_duration_ms,
                            status.position_ms,
                        );
                        if ps.gapless_arm_logged != Some(armed) {
                            let effective_duration_ms = decisions::sane_current_duration(
                                status.duration_ms,
                                track_duration_ms,
                            );
                            let reason = if ps.gapless_sent {
                                "already_armed"
                            } else if effective_duration_ms <= GAPLESS_WINDOW_MS {
                                "duration_le_window"
                            } else if status.position_ms
                                < effective_duration_ms.saturating_sub(GAPLESS_WINDOW_MS)
                            {
                                "before_arming_window"
                            } else {
                                "in_arming_window"
                            };
                            info!(
                                zone_id,
                                output = output_type_str,
                                armed,
                                reason,
                                reported_duration_ms = status.duration_ms,
                                queue_duration_ms = track_duration_ms,
                                effective_duration_ms,
                                position_ms = status.position_ms,
                                gapless_sent = ps.gapless_sent,
                                "gapless_arm_trace"
                            );
                            ps.gapless_arm_logged = Some(armed);
                        }
                    }

                    // Detect gapless transition: renderer reports a different
                    // duration than the current track AND the position confirms
                    // the track actually ended (near end or reset to start).
                    // Some DLNA renderers (DMP-A6/A8) report inaccurate durations
                    // from the start, so duration mismatch alone is insufficient.
                    let duration_changed = decisions::duration_changed(
                        ps.gapless_sent,
                        track_duration_ms,
                        status.duration_ms,
                    );
                    // Position must confirm we are actually at the end of the
                    // current track OR that the position has reset to the
                    // start of the next track.  The played_enough guard
                    // prevents false transitions when a renderer (DMP-A8)
                    // reports position < 5s immediately after SetNext.
                    let position_confirms_transition = decisions::position_confirms_transition(
                        played_enough,
                        status.position_ms,
                        track_duration_ms,
                    );
                    fsm_pact.transition_detected = duration_changed && position_confirms_transition;
                    if duration_changed && position_confirms_transition {
                        let arm_to_advance_ms = ps
                            .gapless_sent_at
                            .map(|t| t.elapsed().as_millis() as u64)
                            .unwrap_or(0);
                        info!(
                            zone_id,
                            renderer_dur = status.duration_ms,
                            track_dur = track_duration_ms,
                            peak_pos = ps.peak_position_ms,
                            arm_to_advance_ms,
                            "gapless_transition_detected"
                        );
                        ps.gapless_sent = false;
                        ps.gapless_sent_at = None;
                        // Voir `position_a_avancer` (#3026).
                        let arme_avant = ps.gapless_armed.take();
                        ps.peak_position_ms = 0;
                        ps.last_position_ms = 0;
                        ps.last_bytes_sent = 0;
                        ps.playing_stall_ticks = 0;
                        ps.stall_declines = 0;
                        ps.track_started_at = Some(Instant::now());
                        ps.stopped_ticks = 0;
                        ps.past_end_ticks = 0;
                        ps.gapless_advance_pending = false;
                        ps.gapless_stuck_ticks = 0;
                        // New track after a gapless advance (no generation bump):
                        // re-arm the once-per-track gapless_arm_trace line.
                        ps.gapless_arm_logged = None;
                        ps.gapless_dsd_skip_pos = None;
                        if let Some(next_pos) = self
                            .position_a_avancer(zone_id, zone_state, arme_avant)
                            .await
                        {
                            info!(zone_id, next_pos, "gapless_advance_metadata");
                            if let Err(e) = self
                                .orchestrator
                                .advance_queue_metadata(zone_id, next_pos)
                                .await
                            {
                                warn!(zone_id, error = %e, "gapless_advance_failed");
                            }
                            // Suppress handle_track_end for a few ticks — the
                            // renderer may briefly report Stopped during the
                            // gapless transition, which would otherwise send a
                            // redundant Stop+Play and cause an audible restart.
                            ps.gapless_cooldown = 4;
                            // Identity-keyed latch re-arms on the new track;
                            // clearing also covers gapless repeat-one (#1113).
                            ps.scrobbled_key = None;
                        } else {
                            self.handle_track_end(zone_id, zone_state).await;
                        }
                    } else if {
                        // Une preparation trop vieille ne vaut plus rien : son
                        // flux a expire cote serveur. On la jette pour que la
                        // condition ci-dessous rearme proprement, plutot que de
                        // laisser le renderer chercher une adresse morte.
                        if decisions::gapless_stage_expired(
                            ps.gapless_sent,
                            ps.gapless_sent_at.map(|t| t.elapsed().as_secs()),
                        ) {
                            info!(
                                zone_id,
                                age_secs = ps.gapless_sent_at.map(|t| t.elapsed().as_secs()),
                                "gapless_stage_expired_rearming"
                            );
                            ps.gapless_sent = false;
                            ps.gapless_sent_at = None;
                            ps.gapless_armed = None;
                        }
                        // « Lire ensuite » PENDANT la fenetre d'armement : la
                        // piste que le renderer a acceptee n'est plus celle que
                        // la file annonce comme suivante (#3026).
                        //
                        // Le geste explicite de l'utilisateur gagne. On desarme,
                        // et la condition ci-dessous re-arme DANS LE MEME TICK
                        // avec la piste inseree : un nouveau
                        // `SetNextAVTransportURI` part vers le renderer.
                        //
                        // Le blanc eventuel est celui que l'utilisateur a
                        // lui-meme provoque, et lui seul : tant que personne ne
                        // touche a la file, les deux identifiants sont egaux,
                        // rien n'est desarme, et l'enchainement sans blanc est
                        // intact. C'est tout l'ecart avec le faux correctif —
                        // desarmer des qu'on touche a la file — qui supprimerait
                        // le defaut en supprimant la fonctionnalite.
                        if ps.gapless_sent {
                            let suivant = Self::next_position(zone_state);
                            let ligne_au_suivant = suivant.and_then(|p| {
                                crate::db::play_queue_repo::PlayQueueRepo::with_backend(
                                    self.db.clone(),
                                )
                                .get_at(zone_id, p)
                                .ok()
                                .flatten()
                                .map(|e| e.id)
                            });
                            if decisions::gapless_arm_outdated(
                                ps.gapless_armed.map(|a| a.row_id),
                                ligne_au_suivant,
                            ) {
                                info!(
                                    zone_id,
                                    armed_row = ?ps.gapless_armed.map(|a| a.row_id),
                                    armed_pos = ?ps.gapless_armed.map(|a| a.position),
                                    next_pos = ?suivant,
                                    row_at_next = ?ligne_au_suivant,
                                    "gapless_rearm_queue_changed"
                                );
                                ps.gapless_sent = false;
                                ps.gapless_sent_at = None;
                                ps.gapless_armed = None;
                                // Une ligne de trace neuve pour le nouvel
                                // armement : sans cela le journal garderait
                                // « already_armed » et ne dirait pas ce qui
                                // vient d'etre renvoye.
                                ps.gapless_arm_logged = None;
                            }
                        }
                        decisions::should_arm_gapless(
                            ps.gapless_sent,
                            status.duration_ms,
                            track_duration_ms,
                            status.position_ms,
                        )
                    } {
                        // Only send SetNextAVTransportURI if gapless is enabled for this zone
                        let gapless_enabled = ZoneRepo::with_backend(self.db.clone())
                            .get(zone_id)
                            .ok()
                            .flatten()
                            .map(|z| z.gapless_enabled)
                            .unwrap_or(true);
                        fsm_pin.gapless_enabled = gapless_enabled;
                        fsm_pact.arm_gapless = gapless_enabled;
                        if gapless_enabled {
                            // Exclusive-mode local outputs (ASIO / WASAPI
                            // exclusive) can't chain internally. Detect that
                            // BEFORE prepare_gapless resolves the next URL —
                            // otherwise it downloads + transcodes the next track
                            // then discards it, and because prepare_gapless
                            // returns false, gapless_sent stays false and this
                            // branch re-fires every tick, re-downloading the same
                            // track in a tight loop (DEvir: repeat=one on ASIO
                            // Fireface, 55 wasted Qobuz downloads/min). Mark
                            // gapless_sent so we stop retrying; the natural-end
                            // fallback advances/repeats the queue.
                            let can_internal_gapless = {
                                let outputs = self.outputs.lock().await;
                                match outputs.get(&device_id) {
                                    Some(arc) => arc.lock().await.supports_internal_gapless(),
                                    None => true,
                                }
                            };
                            if !can_internal_gapless {
                                info!(zone_id, "gapless_skipped_exclusive_output");
                                ps.gapless_sent = true;
                                // Le drapeau est pose pour cesser de re-tenter,
                                // pas parce qu'une piste est partie : ne rien
                                // laisser croire le contraire (#3026).
                                ps.gapless_armed = None;
                            } else if decisions::dsd_skip_latched(
                                ps.gapless_dsd_skip_pos,
                                Self::next_position(zone_state),
                            ) {
                                // Suivant DSD sur DLNA, déjà constaté pour cette
                                // position : ne pas re-résoudre (donc re-créer puis
                                // détruire une session fichier) à chaque tick
                                // (spin 1 Hz, #2394). handle_track_end jouera la
                                // piste explicitement en fin de morceau.
                            } else {
                                match self.prepare_gapless(zone_id, zone_state, &device_id).await {
                                    GaplessPrep::Armed(arme) => {
                                        ps.gapless_sent_at = Some(Instant::now());
                                        ps.gapless_sent = true;
                                        // Ce que le renderer a ACCEPTE. Pose
                                        // apres `set_next_media` seulement :
                                        // un envoi refuse n'arme rien (#3026).
                                        ps.gapless_armed = arme;
                                    }
                                    GaplessPrep::DsdNextSkipped => {
                                        ps.gapless_dsd_skip_pos = Self::next_position(zone_state);
                                    }
                                    GaplessPrep::NotArmed => {}
                                }
                            }
                        } else {
                            debug!(zone_id, "gapless_disabled_for_zone");
                        }
                    }

                    // Position-based end-of-track detection: when the output
                    // still reports Playing but position has reached or exceeded
                    // the known track duration, the audio has effectively ended
                    // (e.g. local/cpal output draining its ring buffer).
                    // Wait POSITION_PAST_END_TICKS consecutive ticks to avoid
                    // cutting off the last fraction of a second of audio.
                    // Add a 3-second margin to avoid cutting off the end of
                    // tracks on DLNA renderers that report position slightly
                    // ahead of actual playback.
                    // Margin path (pure predicate, v0.9 extraction): position ran
                    // past duration + END_MARGIN_MS.
                    //
                    // Fix B (#1239): guard against an UNDER-scanned DB duration. A
                    // BluOS Node reports honest secs/totlen; when Tune's scanned
                    // (queue) duration is shorter than the real audio, the DB-only
                    // threshold fires early — the track is cut mid-play, which on
                    // BluOS triggers a /Clear + /Play and desyncs the now-playing
                    // metadata by one track. For a realtime renderer we widen the
                    // end-of-track threshold to max(queue, reported) BEFORE the
                    // margin, reusing `sane_current_duration` for reliability: it
                    // returns the DB duration when reported is 0 or egregiously off
                    // (RS130), so an absurd/absent report keeps the DB-only
                    // behavior and max() only ever widens for a trustworthy report
                    // that exceeds the DB scan. Non-realtime outputs are unchanged.
                    // This can only DELAY the position-based past-end, never block a
                    // track: the Stopped/natural-end and wall-clock paths remain the
                    // ultimate end-of-track guarantees for a renderer that stalls.
                    let effective_end_duration_ms = if status.realtime {
                        track_duration_ms.max(decisions::sane_current_duration(
                            status.duration_ms,
                            track_duration_ms,
                        ))
                    } else {
                        track_duration_ms
                    };
                    let past_end = decisions::past_end_reached(
                        effective_end_duration_ms,
                        played_enough,
                        status.position_ms,
                    );
                    // Exclusive local outputs (ASIO / WASAPI exclusive) cap the
                    // reported position at exactly the track duration and keep
                    // reporting Playing — their blocking HTTP read never sees a
                    // clean EOF at the loop point, so the +3s margin above never
                    // triggers. Treat "reached the very end (within 250ms) and
                    // held there for POSITION_PAST_END_TICKS ticks" as ended, so
                    // repeat/advance fires (DEvir: ASIO Fireface repeat never
                    // looped). Gated to exclusive outputs so DLNA — which can sit
                    // near the end legitimately — keeps the +3s margin above.
                    let reached_end_exclusive = !past_end
                        && !in_seek_grace
                        && track_duration_ms > decisions::END_MARGIN_MS
                        && played_enough
                        && status.position_ms + 250 >= track_duration_ms
                        && {
                            let outputs = self.outputs.lock().await;
                            match outputs.get(&device_id) {
                                Some(arc) => !arc.lock().await.supports_internal_gapless(),
                                None => false,
                            }
                        };
                    // Wall-clock fallback for a DLNA renderer (LMS UPnP bridge)
                    // that reports no duration of its own and never advances its
                    // position: treat the track as ended once the queue-known
                    // duration (plus margin) has elapsed on the wall clock.
                    // Guarded to `!in_seek_grace` on top of the helper's DLNA +
                    // reported-duration==0 gate (see wall_clock_past_end).
                    let wall_clock_past_end = !in_seek_grace
                        && decisions::wall_clock_past_end(
                            is_dlna,
                            status.duration_ms,
                            track_duration_ms,
                            wall_elapsed,
                        );
                    // Chromecast has no reliable end-of-track signal on a 1 Hz
                    // fresh-connect poll (FINISHED is a one-shot broadcast; a
                    // frozen near-end position dodges the position paths), and —
                    // unlike DLNA — no wall-clock fallback, so an album stalls
                    // after track 1 (Rhorn, forum #1226). Advance on Tune's own
                    // clock once the track's full duration has elapsed, still
                    // gated by played_enough (peak ≥ 80 %, honest on Cast) so a
                    // genuine mid-track buffering stall can't false-advance.
                    let chromecast_wall_clock_past_end = !in_seek_grace
                        && decisions::chromecast_wall_clock_past_end(
                            output_type_str,
                            played_enough,
                            track_duration_ms,
                            wall_elapsed,
                        );
                    // DMP-A6/A8 : PLAYING éternel, position gelée À la durée,
                    // poll sain — seul filet possible : l'horloge de Tune
                    // (même !in_seek_grace que ses deux voisins).
                    let dlna_frozen_end = !in_seek_grace
                        && decisions::dlna_frozen_at_end_wall_clock(
                            is_dlna,
                            played_enough,
                            track_duration_ms,
                            status.position_ms,
                            wall_elapsed,
                        );
                    if past_end
                        || reached_end_exclusive
                        || wall_clock_past_end
                        || chromecast_wall_clock_past_end
                        || dlna_frozen_end
                    {
                        ps.past_end_ticks += 1;
                        if ps.past_end_ticks >= POSITION_PAST_END_TICKS {
                            info!(
                                zone_id,
                                position_ms = status.position_ms,
                                track_dur = track_duration_ms,
                                wall_secs = wall_elapsed,
                                past_end_ticks = ps.past_end_ticks,
                                exclusive_end = reached_end_exclusive,
                                wall_clock_end = wall_clock_past_end,
                                cast_wall_clock_end = chromecast_wall_clock_past_end,
                                dlna_frozen_end,
                                "position_past_end_advancing"
                            );
                            track_ended = true;
                            fsm_pact.past_end_track_ended = true;
                            motif_fin_de_piste = decisions::motif_fin::POSITION_AU_DELA_DE_LA_FIN;
                        }
                    } else {
                        ps.past_end_ticks = 0;
                    }

                    // #2493 — Tades : « un morceau de 1'46 tourne depuis dix
                    // minutes » (Serenade/upmpdcli). Aucun des cinq detecteurs
                    // ci-dessus n'a agi, et la position montree a l'ecran est
                    // plafonnee a la duree : le testeur voit « 1:46 / 1:46, en
                    // lecture » indefiniment. Tune n'a alors plus le droit de
                    // presenter cet etat comme une lecture ordinaire.
                    //
                    // Ce bloc ne touche NI `track_ended` NI `force_stop`. La
                    // meme forme est produite par une lecture bloquee et par une
                    // duree fausse (etiquette erronee, piste reellement plus
                    // longue) : couper reviendrait a amputer une ecoute valide
                    // une fois sur deux. On DIT, on n'agit pas — voir
                    // `decisions::position_au_dela_de_la_duree`.
                    //
                    // Complementaire de #2116 juste en dessous, qui exclut
                    // explicitement la zone de fin (`near_known_end`) : celui-la
                    // couvre la position gelee AVANT la fin, celui-ci la
                    // position collee A la fin.
                    if decisions::position_au_dela_de_la_duree(
                        source_est_radio,
                        effective_end_duration_ms,
                        status.position_ms,
                    ) {
                        ps.depassement_duree_ticks = ps.depassement_duree_ticks.saturating_add(1);
                    } else {
                        ps.depassement_duree_ticks = 0;
                        ps.depassement_duree_signale = false;
                    }
                    if !track_ended
                        && ps.depassement_duree_ticks >= DEPASSEMENT_DUREE_TICKS
                        && !ps.depassement_duree_signale
                    {
                        ps.depassement_duree_signale = true;
                        // Les trois inconnues que le ticket reclamait faute de
                        // journal : la position est-elle figee, reboucle-t-elle,
                        // et des octets sont-ils encore servis ?
                        let octets_servis = match zone_state
                            .now_playing
                            .as_ref()
                            .and_then(|np| np.stream_id.as_deref())
                        {
                            Some(sid) => self.orchestrator.streamer_bytes_sent(sid).await,
                            None => None,
                        };
                        warn!(
                            zone_id,
                            output_type = output_type_str,
                            position_ms = status.position_ms,
                            peak_position_ms = ps.peak_position_ms,
                            duree_file_ms = track_duration_ms,
                            duree_rapportee_ms = status.duration_ms,
                            duree_effective_ms = effective_end_duration_ms,
                            wall_secs = wall_elapsed,
                            ticks = ps.depassement_duree_ticks,
                            ?octets_servis,
                            "lecture_annoncee_au_dela_de_la_duree"
                        );
                    }

                    // #2116: a renderer can acknowledge Play forever while
                    // producing no more sound. Only stop after two independent
                    // progress signals (renderer position and bytes served by
                    // Tune) have both remained frozen for the full observation
                    // window. The pure eligibility predicate deliberately
                    // excludes startup, seeks, unknown-position devices and a
                    // normal frozen-at-end transition.
                    let stream_id = zone_state
                        .now_playing
                        .as_ref()
                        .and_then(|np| np.stream_id.as_deref());
                    let playing_stall_eligible = !track_ended
                        && decisions::dlna_playing_stall_eligible(
                            output_type_str,
                            zone_state.state == PlayState::Playing,
                            status.state == TransportState::Playing,
                            status.realtime,
                            stream_id.is_some(),
                            in_seek_grace,
                            ps.track_loaded_at.elapsed().as_secs(),
                            ps.peak_position_ms,
                            status.position_ms,
                            track_duration_ms,
                        );
                    if playing_stall_eligible {
                        if let Some(current_bytes) = match stream_id {
                            Some(sid) => self.orchestrator.streamer_bytes_sent(sid).await,
                            None => None,
                        } {
                            let previous_bytes = ps.last_bytes_sent;
                            ps.playing_stall_ticks = decisions::next_dlna_playing_stall_ticks(
                                ps.playing_stall_ticks,
                                true,
                                prev_position_ms,
                                status.position_ms,
                                previous_bytes,
                                current_bytes,
                            );
                            ps.last_bytes_sent = current_bytes;
                            if ps.playing_stall_ticks >= PLAYING_STALL_THRESHOLD {
                                warn!(
                                    zone_id,
                                    position_ms = status.position_ms,
                                    peak_position_ms = ps.peak_position_ms,
                                    bytes_sent = current_bytes,
                                    stall_ticks = ps.playing_stall_ticks,
                                    wall_secs = wall_elapsed,
                                    "dlna_playing_without_progress_stopping_zone"
                                );
                                force_stop = true;
                            }
                        } else {
                            // No byte evidence means no conviction: a transient
                            // metrics lookup failure restarts the whole window.
                            ps.playing_stall_ticks = 0;
                        }
                    } else {
                        ps.playing_stall_ticks = 0;
                    }
                    // v0.9 rc.2 — FSM shadow-compare for the Playing arm.
                    if *POLLER_FSM_SHADOW {
                        let predicted = fsm::classify_playing(&fsm_pin);
                        if predicted != fsm_pact {
                            warn!(
                                zone_id,
                                ?predicted,
                                actual = ?fsm_pact,
                                "poller_fsm_shadow_divergence_playing"
                            );
                        }
                    }
                }
                TransportState::Paused => {
                    ps.stopped_ticks = 0;
                    ps.playing_stall_ticks = 0;
                }
            }

            // Sync metrics to shared map for external visibility
            self.shared_metrics.lock().await.insert(
                zone_id,
                ZonePollerMetrics {
                    total_polls: ps.total_polls,
                    total_errors: ps.total_errors,
                    consecutive_errors: ps.consecutive_errors,
                    last_latency_ms: ps.last_latency_ms,
                    max_latency_ms: ps.max_latency_ms,
                    lecture_au_dela_de_la_duree: ps.depassement_duree_signale,
                },
            );

            if force_stop {
                poll_states.remove(&zone_id);
                let device_id_ref = self.get_zone_device_id(zone_id);
                let relance = force_stop_demarrage_mort && {
                    let mut relances = self.relances_demarrage_mort.lock().await;
                    let autorisee = decisions::relance_demarrage_mort_autorisee(
                        relances.get(&zone_id).map(|t| t.elapsed().as_secs()),
                    );
                    if autorisee {
                        relances.insert(zone_id, Instant::now());
                    }
                    autorisee
                };
                if relance {
                    // Pause→Stop d'abord : le pipeline Eversolo coincé ACQUITTE
                    // les Stop sans les exécuter, seul Pause→Stop le libère
                    // (constaté par SOAP direct sur le DMP-A8). Best-effort :
                    // un appareil sain n'en souffre pas.
                    if let Err(error) = self
                        .orchestrator
                        .pause(zone_id, device_id_ref.as_deref())
                        .await
                    {
                        warn!(zone_id, error = %error, "demarrage_mort_pause_echouee");
                    }
                    self.orchestrator
                        .stop(zone_id, device_id_ref.as_deref())
                        .await;
                    let position = zone_state.queue_position;
                    match self.orchestrator.play_from_queue(zone_id, position).await {
                        Ok(_) => {
                            warn!(zone_id, position, "demarrage_mort_relance_automatique");
                        }
                        Err(e) => {
                            warn!(zone_id, position, error = %e, "demarrage_mort_relance_echouee");
                            self.orchestrator
                                .stop(zone_id, device_id_ref.as_deref())
                                .await;
                        }
                    }
                } else {
                    self.orchestrator
                        .stop(zone_id, device_id_ref.as_deref())
                        .await;
                }
            } else if track_ended {
                // #2488 — la moitié invisible du blanc entre deux pistes.
                //
                // `playback_timing` (orchestrator) démarre à `play_inner`,
                // donc APRÈS cette décision : tout ce que le sondeur a attendu
                // pour conclure « c'est fini » n'apparaît nulle part. Sur un
                // renderer réseau ce terme domine — de 0 ms (la sortie locale
                // réveille le sondeur) à plusieurs secondes selon la branche.
                // Une seule ligne, ici, au seul entonnoir d'avance côté
                // serveur, avec le nom de la branche et son plancher.
                let etat = poll_states.get(&zone_id);
                info!(
                    zone_id,
                    motif = motif_fin_de_piste,
                    plancher_ms = decisions::plancher_de_detection_ms(motif_fin_de_piste),
                    stopped_ticks = etat.map(|p| p.stopped_ticks).unwrap_or(0),
                    past_end_ticks = etat.map(|p| p.past_end_ticks).unwrap_or(0),
                    gapless_sent = etat.map(|p| p.gapless_sent).unwrap_or(false),
                    peak_pos = etat.map(|p| p.peak_position_ms).unwrap_or(0),
                    track_dur = track_duration_ms,
                    wall_secs = wall_elapsed,
                    output = all_zones
                        .iter()
                        .find(|z| z.id == Some(zone_id))
                        .and_then(|z| z.output_type.as_deref())
                        .unwrap_or(""),
                    "track_end_gap"
                );
                poll_states.remove(&zone_id);
                self.handle_track_end(zone_id, zone_state).await;
            }
        }
    }

    /// Next-track decision for AUTOMATIC advance (poller gapless auto-advance
    /// and prefetch): honours repeat-one by replaying the current position.
    pub fn next_position(zone_state: &crate::playback::ZoneState) -> Option<i64> {
        Self::next_position_inner(zone_state, false)
    }

    /// Next-track decision for a MANUAL skip (the `next` button). A manual skip
    /// is an explicit request for a *different* track, so it ignores repeat-one
    /// — treating it as repeat-all (advance, wrapping at the end) instead of
    /// replaying the current track. Matches Spotify/Apple Music: repeat-one only
    /// governs automatic end-of-track advance, never the next button (#1110).
    pub fn next_position_manual(zone_state: &crate::playback::ZoneState) -> Option<i64> {
        Self::next_position_inner(zone_state, true)
    }

    /// Next-track decision *after* the track at `failed_pos` turned out to be
    /// unplayable. Same rules as `next_position` (repeat, shuffle order), but
    /// evaluated as if `failed_pos` had just finished playing — so skipping a
    /// dead item keeps following the shuffle permutation instead of falling
    /// back to raw queue order.
    pub fn next_position_after(
        zone_state: &crate::playback::ZoneState,
        failed_pos: i64,
    ) -> Option<i64> {
        let mut as_if = zone_state.clone();
        as_if.queue_position = failed_pos;
        if as_if.shuffle && !as_if.shuffle_order.is_empty() {
            // Follow the permutation from the slot we just failed on, so the
            // walk cannot revisit it or lose its place in the cycle.
            match as_if
                .shuffle_order
                .iter()
                .position(|&i| i as i64 == failed_pos)
            {
                Some(idx) => as_if.shuffle_index = idx as i64,
                None => as_if.shuffle_index += 1,
            }
        }
        Self::next_position_inner(&as_if, false)
    }

    fn next_position_inner(zone_state: &crate::playback::ZoneState, manual: bool) -> Option<i64> {
        if zone_state.queue_length == 0 {
            return None;
        }
        // A manual skip overrides repeat-one (see next_position_manual). For the
        // automatic path `repeat` is unchanged, so behaviour is identical.
        let repeat = if manual && zone_state.repeat == RepeatMode::One {
            RepeatMode::All
        } else {
            zone_state.repeat
        };
        if repeat == RepeatMode::One {
            return Some(zone_state.queue_position);
        }

        // Shuffle follows a materialised order (a permutation of the queue
        // indices, generated when shuffle is enabled and kept in ZoneState).
        // Playing the order in sequence guarantees every track exactly once per
        // cycle — repeat-off stops at the end, repeat-all loops to the start.
        // This is the SINGLE next-track decision used by the poller's gapless
        // auto-advance, the manual `next` endpoint AND prefetch, so they stay
        // consistent (eric, #954). Previously shuffle under repeat-off was
        // ignored: it walked the queue by raw index and stopped at the raw end.
        if zone_state.shuffle && !zone_state.shuffle_order.is_empty() {
            let next_idx = zone_state.shuffle_index + 1;
            if next_idx < 0 {
                return zone_state.shuffle_order.first().map(|&i| i as i64);
            }
            if next_idx as usize >= zone_state.shuffle_order.len() {
                return match repeat {
                    RepeatMode::All => zone_state.shuffle_order.first().map(|&i| i as i64),
                    _ => None, // repeat-off: every track played once → stop
                };
            }
            return zone_state
                .shuffle_order
                .get(next_idx as usize)
                .map(|&i| i as i64);
        }

        // Non-shuffle (or shuffle order not yet materialised — falls back to
        // sequential until the next update_queue_info rebuilds the order).
        match repeat {
            RepeatMode::One => Some(zone_state.queue_position),
            RepeatMode::All => Some((zone_state.queue_position + 1) % zone_state.queue_length),
            RepeatMode::Off => {
                let next = zone_state.queue_position + 1;
                if next >= zone_state.queue_length {
                    None
                } else {
                    Some(next)
                }
            }
        }
    }

    async fn handle_track_end(&self, zone_id: i64, zone_state: &crate::playback::ZoneState) {
        // Diagnostic: capture now-playing info to help diagnose premature advance issues.
        let np_title = zone_state
            .now_playing
            .as_ref()
            .map(|np| np.title.as_str())
            .unwrap_or("unknown");
        let np_duration = zone_state
            .now_playing
            .as_ref()
            .map(|np| np.duration_ms)
            .unwrap_or(0);

        let device_id = self.get_zone_device_id(zone_id);

        let Some(next_pos) = Self::next_position(zone_state) else {
            // Queue ended — check if autoplay is enabled for this zone
            let autoplay_enabled = crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
                .get_autoplay_enabled(zone_id);

            if autoplay_enabled {
                let mut seed_track_id = zone_state.now_playing.as_ref().and_then(|np| np.track_id);
                let mut seed_artist = zone_state
                    .now_playing
                    .as_ref()
                    .and_then(|np| np.artist_name.clone());

                // File vide DÈS LE DÉPART : rien n'a joué, donc rien à
                // prolonger. C'était le cas d'un serveur qu'on rallume ou
                // d'une file qu'on vient d'effacer — le réglage « lecture
                // automatique » était activé et il ne se passait rien, la
                // seule trace étant un `autoplay_skipped_no_seed` en DEBUG.
                // On repart de la dernière écoute de LA ZONE, à défaut de la
                // maison : c'est la graine la plus proche de ce que
                // l'auditeur attend d'entendre.
                if seed_artist.is_none() && seed_track_id.is_none() {
                    // La radio par défaut se construit sur les DERNIERS TITRES
                    // écoutés, et non sur le seul dernier artiste : c'est la
                    // différence entre prolonger un morceau et proposer une
                    // radio. On demande leurs semblables à plusieurs artistes
                    // récents, et on choisit dans tout ce pool.
                    let radio =
                        crate::playback::auto_dj::radio_depuis_l_historique(&self.db, zone_id, 10)
                            .await;
                    let ids: Vec<i64> = radio
                        .iter()
                        .filter_map(|t| t["track_id"].as_i64())
                        .collect();
                    if !ids.is_empty() {
                        info!(
                            zone_id,
                            count = ids.len(),
                            "autoplay_radio_depuis_l_historique"
                        );
                        let queue_repo = crate::db::play_queue_repo::PlayQueueRepo::with_backend(
                            self.db.clone(),
                        );
                        if queue_repo.append_tracks(zone_id, &ids).is_ok() {
                            let new_pos = zone_state.queue_position + 1;
                            if let Err(e) =
                                self.orchestrator.play_from_queue(zone_id, new_pos).await
                            {
                                warn!(zone_id, error = %e, "autoplay_play_failed");
                                self.orchestrator.stop(zone_id, device_id.as_deref()).await;
                            }
                            return;
                        }
                    }

                    // La bibliothèque n'a rien rendu : on garde une graine pour
                    // les autres cartes de la chaîne — radio du service,
                    // genre/BPM — plutôt que de s'arrêter là.
                    if let Some(g) = crate::playback::auto_dj::graine_recente(&self.db, zone_id) {
                        info!(
                            zone_id,
                            artist = %g.artist_name.as_deref().unwrap_or(""),
                            "autoplay_graine_depuis_l_historique"
                        );
                        seed_track_id = g.track_id;
                        seed_artist = g.artist_name;
                    }
                }

                // « Radio artistes similaires » : la graine est le NOM d'artiste,
                // donc une écoute streaming (pas de track_id local) alimente
                // aussi l'autoplay. Repli sur le générateur genre/BPM local si
                // l'API d'enrichissement est injoignable ou ne matche rien dans
                // la bibliothèque (Tune doit marcher sans mozaiklabs.fr).
                // La source de l'ecoute en cours passe AVANT le generateur
                // local. Le repli streaming plus bas ne se declenchait que si
                // le local n'avait rien rendu — donc jamais, chez qui a une
                // bibliotheque locale garnie. L'autoplay enchainait alors des
                // titres locaux au milieu d'une ecoute Qobuz.
                // Le repli streaming plus bas est le MEME appel : sans ce
                // temoin il refaisait a l'identique le travail que la branche
                // preferee venait d'echouer — deux fois les memes appels
                // reseau, deux fois les memes lignes de log.
                let mut streaming_already_tried = false;
                let seed_source = zone_state.now_playing.as_ref().map(|np| np.source.clone());
                let seed_source_id = zone_state
                    .now_playing
                    .as_ref()
                    .and_then(|np| np.source_id.clone());
                if decisions::autoplay_prefers_streaming(seed_source.as_deref())
                    && let Some(ref artist) = seed_artist
                    && let Some(ref source) = seed_source
                {
                    let added = self
                        .autoplay_streaming_radio(
                            zone_id,
                            artist,
                            source,
                            seed_source_id.as_deref(),
                        )
                        .await;
                    if added > 0 {
                        let new_pos = zone_state.queue_position + 1;
                        info!(
                            zone_id,
                            added,
                            source = %source,
                            "autoplay_streaming_radio_started_preferred"
                        );
                        if let Err(e) = self.orchestrator.play_from_queue(zone_id, new_pos).await {
                            warn!(zone_id, error = %e, "autoplay_play_failed");
                            self.orchestrator.stop(zone_id, device_id.as_deref()).await;
                        }
                        return;
                    }
                    // Le service n'a rien rendu (hors catalogue, API muette) :
                    // on retombe sur le generateur local plutot que de laisser
                    // la file s'arreter en silence.
                    info!(zone_id, "autoplay_streaming_empty_falling_back_local");
                    streaming_already_tried = true;
                }

                let mut generated = Vec::new();
                if let Some(ref artist) = seed_artist {
                    info!(zone_id, artist = %artist, "autoplay_similar_artists_radio");
                    generated = crate::playback::auto_dj::generate_similar_artists_queue(
                        &self.db, artist, 10,
                    )
                    .await;
                }
                if generated.is_empty() {
                    if let Some(seed_id) = seed_track_id {
                        info!(
                            zone_id,
                            seed_track_id = seed_id,
                            "autoplay_generating_tracks"
                        );
                        generated = crate::playback::auto_dj::generate_queue(&self.db, seed_id, 10);
                    } else if seed_artist.is_none() {
                        debug!(zone_id, "autoplay_skipped_no_seed");
                    }
                }

                let track_ids: Vec<i64> = generated
                    .iter()
                    .filter_map(|t| t["track_id"].as_i64())
                    .collect();

                // Rien en local : la radio s'arrêtait là, en silence. Pour
                // quelqu'un qui écoute Qobuz sans bibliothèque locale, c'était
                // TOUJOURS le cas — la graine streaming était gérée, les
                // résultats ne pouvaient être que locaux. On va donc chercher
                // les artistes similaires dans le service de la piste en cours.
                if track_ids.is_empty()
                    && !streaming_already_tried
                    && let Some(ref artist) = seed_artist
                    && let Some(source) = zone_state
                        .now_playing
                        .as_ref()
                        .map(|np| np.source.clone())
                        .filter(|s| s != "local" && !s.is_empty())
                {
                    let added = self
                        .autoplay_streaming_radio(
                            zone_id,
                            artist,
                            &source,
                            seed_source_id.as_deref(),
                        )
                        .await;
                    if added > 0 {
                        let new_pos = zone_state.queue_position + 1;
                        info!(
                            zone_id,
                            added,
                            source = %source,
                            "autoplay_streaming_radio_started"
                        );
                        if let Err(e) = self.orchestrator.play_from_queue(zone_id, new_pos).await {
                            warn!(zone_id, error = %e, "autoplay_play_failed");
                            self.orchestrator.stop(zone_id, device_id.as_deref()).await;
                        }
                        return;
                    }
                }

                if !track_ids.is_empty() {
                    info!(
                        zone_id,
                        count = track_ids.len(),
                        "autoplay_tracks_generated"
                    );

                    // Append generated tracks to the play queue
                    let queue_repo =
                        crate::db::play_queue_repo::PlayQueueRepo::with_backend(self.db.clone());
                    if let Err(e) = queue_repo.append_tracks(zone_id, &track_ids) {
                        warn!(zone_id, error = %e, "autoplay_append_queue_failed");
                        self.orchestrator.stop(zone_id, device_id.as_deref()).await;
                        return;
                    }

                    // Emit autoplay_tracks_added event for UI updates
                    if let Some(ref bus) = self.event_bus {
                        bus.emit(
                            "playback.autoplay_tracks_added",
                            serde_json::json!({
                                "zone_id": zone_id,
                                "track_ids": track_ids,
                                "tracks": generated,
                                "seed_track_id": seed_track_id,
                                "seed_artist": seed_artist,
                            }),
                        );
                    }

                    // Play the first generated track (next position after current)
                    let new_pos = zone_state.queue_position + 1;
                    info!(zone_id, new_pos, "autoplay_starting_generated_track");
                    if let Err(e) = self.orchestrator.play_from_queue(zone_id, new_pos).await {
                        warn!(zone_id, error = %e, "autoplay_play_failed");
                        self.orchestrator.stop(zone_id, device_id.as_deref()).await;
                    }
                    return;
                }
                info!(zone_id, "autoplay_no_similar_tracks_found");
            }

            // Log the queue geometry so a "doesn't advance to next track" report
            // (Jean-Pierre) can be told apart at a glance: queue_len=1 means the
            // play truncated the queue to a single track (single-track play path),
            // whereas queue_len>1 with pos+1<len would be a genuine advance bug.
            info!(
                zone_id,
                queue_pos = zone_state.queue_position,
                queue_len = zone_state.queue_length,
                repeat = ?zone_state.repeat,
                "queue_ended"
            );
            self.orchestrator.stop(zone_id, device_id.as_deref()).await;
            return;
        };

        let is_repeat = matches!(zone_state.repeat, RepeatMode::One | RepeatMode::All);
        info!(
            zone_id,
            next_pos,
            repeat = ?zone_state.repeat,
            shuffle = zone_state.shuffle,
            is_repeat,
            title = %np_title,
            duration_ms = np_duration,
            queue_len = zone_state.queue_length,
            queue_pos = zone_state.queue_position,
            "auto_next"
        );
        // Skip tracks that cannot be played instead of ending the session. A
        // single unplayable streaming track — rights withdrawn, region block,
        // the service returning no URL at any format — used to stop the whole
        // queue: playing 11 albums to a zone died on one blocked track with
        // 108 items still queued, leaving nothing but a WARN behind. Walk
        // forward over the dead items, announce each one, and stop only when
        // the queue really is exhausted (or the failures look systemic).
        let mut attempt_pos = next_pos;
        let mut skipped = 0u32;
        loop {
            match self
                .orchestrator
                .play_from_queue(zone_id, attempt_pos)
                .await
            {
                Ok(_) => {
                    if skipped > 0 {
                        info!(
                            zone_id,
                            skipped,
                            next_pos = attempt_pos,
                            "auto_next_resumed_after_skips"
                        );
                    }
                    return;
                }
                Err(e) => {
                    warn!(zone_id, error = %e, pos = attempt_pos, "auto_next_failed");
                    if let Some(ref bus) = self.event_bus {
                        bus.emit(
                            "playback.track_skipped",
                            serde_json::json!({
                                "zone_id": zone_id,
                                "position": attempt_pos,
                                "reason": e.to_string(),
                            }),
                        );
                    }
                    skipped += 1;
                    // A run this long is not "one bad track" any more — an
                    // expired token or a dead network would otherwise have us
                    // hammer the service once per queued item.
                    if skipped >= MAX_CONSECUTIVE_SKIPS {
                        warn!(zone_id, skipped, "auto_next_skip_limit_reached");
                        break;
                    }
                    match Self::next_position_after(zone_state, attempt_pos) {
                        // Same slot again means repeat-one on a dead track:
                        // skipping would spin forever.
                        Some(p) if p != attempt_pos => attempt_pos = p,
                        _ => break,
                    }
                }
            }
        }
        self.orchestrator.stop(zone_id, device_id.as_deref()).await;
    }

    /// Resolve the next queue item's stream URL for gapless, with one bounded
    /// retry on transient failure (F1). Streaming next-track resolution can
    /// fail transiently (network blip, DASH parse, token refreshed mid-flight);
    /// previously a single failure silently abandoned gapless, producing an
    /// audible Stop+Play gap. The happy path is unchanged (first-try success).
    /// Ou avancer les metadonnees a la transition : la ou est passee la piste
    /// que le renderer a REELLEMENT acceptee (#3026).
    ///
    /// Cas courant — personne n'a touche a la file : la ligne armee EST celle
    /// que l'index designe, et cette fonction rend exactement `next_position`.
    /// Rien ne change, pour aucune sortie.
    ///
    /// Cas de la course : l'insertion tombe ENTRE deux sondages. Aucun tick n'a
    /// pu re-armer, le renderer joue donc toujours la piste armee — et c'est
    /// ELLE que l'ecran doit nommer. Avancer sur l'index+1 courant nommerait
    /// l'inseree : l'ecran ment, puis le compteur de fin de piste adopte la
    /// duree de l'inseree et coupe l'audio reel avant sa fin.
    ///
    /// L'insertion n'est pas perdue, elle garde sa ligne dans la file ; elle
    /// perd son tour. C'est le prix de la course, et il se paie une fois : des
    /// qu'un tick separe le geste de la transition — 15 s et 18 s dans les deux
    /// occurrences du journal — le re-armement rend son tour a l'inseree.
    async fn position_a_avancer(
        &self,
        zone_id: i64,
        zone_state: &crate::playback::ZoneState,
        arme: Option<ArmedNext>,
    ) -> Option<i64> {
        let par_index = Self::next_position(zone_state);
        let arme = arme?;
        let repo = crate::db::play_queue_repo::PlayQueueRepo::with_backend(self.db.clone());
        let ligne_au_suivant = par_index
            .and_then(|p| repo.get_at(zone_id, p).ok().flatten())
            .map(|e| e.id);
        if !decisions::gapless_arm_outdated(Some(arme.row_id), ligne_au_suivant) {
            return par_index;
        }
        // La ligne armee a glisse : la retrouver PAR SON IDENTITE.
        match repo
            .get_ordered(zone_id)
            .ok()
            .and_then(|file| file.into_iter().find(|e| e.id == arme.row_id))
        {
            Some(e) => {
                info!(
                    zone_id,
                    armed_row = arme.row_id,
                    armed_pos = arme.position,
                    now_pos = e.position,
                    by_index = ?par_index,
                    "gapless_advance_follows_armed_track"
                );
                Some(e.position)
            }
            // La piste armee a quitte la file : plus rien a suivre. On retombe
            // sur l'index, c'est-a-dire sur le comportement d'avant.
            None => par_index,
        }
    }

    async fn resolve_gapless_next(
        &self,
        zone_id: i64,
        next_pos: i64,
    ) -> Result<crate::orchestrator::ResolvedQueueItem, String> {
        match self
            .orchestrator
            .resolve_queue_item_url(zone_id, next_pos)
            .await
        {
            Ok(r) => Ok(r),
            Err(e) => {
                warn!(zone_id, error = %e, attempt = 1, "gapless_resolve_retry");
                self.orchestrator
                    .resolve_queue_item_url(zone_id, next_pos)
                    .await
            }
        }
    }

    async fn prepare_gapless(
        &self,
        zone_id: i64,
        zone_state: &crate::playback::ZoneState,
        device_id: &str,
    ) -> GaplessPrep {
        let Some(next_pos) = Self::next_position(zone_state) else {
            return GaplessPrep::NotArmed;
        };

        // L'identite de ce qu'on s'apprete a armer, lue AVANT de le resoudre :
        // une ligne de file, pas une position (#3026). C'est la seule trace de
        // ce que le renderer aura reellement accepte.
        let arme = crate::db::play_queue_repo::PlayQueueRepo::with_backend(self.db.clone())
            .get_at(zone_id, next_pos)
            .ok()
            .flatten()
            .map(|e| ArmedNext {
                row_id: e.id,
                position: next_pos,
            });

        // Local-file gapless (OAAT native DSD): the output reads the next
        // track's `.dsf` directly, so resolve it as a local file WITHOUT a
        // transcode session (no orphaned DSD->PCM decode / send-timeout stall)
        // and stage it via set_next_media(file_path=..). If the next item has no
        // local file (streaming track), don't arm — the natural-end fallback
        // advances the queue.
        let prefers_local_file = {
            let outputs = self.outputs.lock().await;
            match outputs.get(device_id) {
                Some(arc) => arc.lock().await.prefers_local_file_gapless(),
                None => false,
            }
        };
        if prefers_local_file {
            let t0 = Instant::now();
            match self
                .orchestrator
                .resolve_gapless_next_local_file(zone_id, next_pos)
                .await
            {
                Ok(resolved) if resolved.file_path.is_some() => {
                    let output_arc = {
                        let outputs = self.outputs.lock().await;
                        outputs.get(device_id).map(|a| a.clone())
                    };
                    let Some(output_arc) = output_arc else {
                        return GaplessPrep::NotArmed;
                    };
                    let output = output_arc.lock().await;
                    let media = crate::outputs::PlayMedia {
                        url: &resolved.url,
                        mime_type: &resolved.mime_type,
                        title: Some(&resolved.title),
                        artist: resolved.artist.as_deref(),
                        album: resolved.album.as_deref(),
                        cover_url: resolved.cover_url.as_deref(),
                        duration_ms: resolved.duration_ms,
                        file_size: resolved.file_size,
                        file_path: resolved.file_path.as_deref(),
                        sample_rate: resolved.sample_rate,
                        bit_depth: resolved.bit_depth,
                        channels: resolved.channels,
                        live_stream: false,
                        byte_seekable: true,
                        origin_url: None,
                        source: resolved.source.as_deref(),
                        source_id: resolved.source_id.as_deref(),
                        track_number: resolved.track_number,
                        disc_number: resolved.disc_number,
                    };
                    return match output.set_next_media(&media).await {
                        Ok(()) => {
                            info!(
                                zone_id,
                                title = %resolved.title,
                                resolve_ms = t0.elapsed().as_millis() as u64,
                                "gapless_next_set_local_file"
                            );
                            GaplessPrep::Armed(arme)
                        }
                        Err(e) => {
                            warn!(zone_id, error = %e, "gapless_set_next_local_file_failed");
                            GaplessPrep::NotArmed
                        }
                    };
                }
                Ok(_) => {
                    info!(zone_id, "gapless_local_file_skipped_no_local_next");
                    return GaplessPrep::NotArmed;
                }
                Err(e) => {
                    warn!(zone_id, error = %e, "gapless_local_file_resolve_failed");
                    return GaplessPrep::NotArmed;
                }
            }
        }

        // v0.9 gapless characterization: time the next-track resolution and
        // surface failures at warn. These paths were debug-only, so streaming
        // gapless instability (Tidal DASH download slowness, URL/token issues)
        // was invisible in production journald. Logging only — no behaviour change.
        let t0 = Instant::now();
        match self.resolve_gapless_next(zone_id, next_pos).await {
            Ok(resolved) => {
                let resolve_ms = t0.elapsed().as_millis() as u64;
                let is_streaming = resolved.stream_id.is_some();
                if let Some(ref sid) = resolved.stream_id {
                    let w0 = Instant::now();
                    if !self.orchestrator.wait_stream_data_ready(sid, 5000).await {
                        // The next track's transcode session produced no data
                        // within the 5s budget — common for Tidal Hi-Res DASH
                        // multi-segment downloads. A session that is merely SLOW
                        // is still armed: refusing here would put a gap between
                        // every Hi-Res track.
                        //
                        // Mais « pas encore » et « plus jamais » ne se
                        // distinguent pas dans `data_ready`. La seule question
                        // qui les separe est celle que `resume` pose deja
                        // (#2512) : la session existe-t-elle encore ? Le
                        // producteur d'un transcodage streaming la RETIRE
                        // desormais quand il meurt sans ecrire un octet — echec
                        // de telechargement CDN, voir
                        // `abandonner_la_session_de_transcodage`. S'enchainer
                        // sur une session disparue fige la sortie locale
                        // jusqu'au Stop (#3287, Gros Bidon, Qobuz en USB) : on
                        // n'arme pas, et la fin naturelle avance la file avec un
                        // petit blanc — jamais un gel.
                        let session_vivante = self.orchestrator.stream_session_alive(sid).await;
                        warn!(
                            zone_id,
                            resolve_ms,
                            waited_ms = w0.elapsed().as_millis() as u64,
                            session_vivante,
                            "gapless_data_ready_timeout"
                        );
                        if !session_vivante {
                            warn!(
                                zone_id,
                                stream_id = %sid,
                                "gapless_non_arme_session_disparue"
                            );
                            return GaplessPrep::NotArmed;
                        }
                    }
                }
                let output_arc = {
                    let outputs = self.outputs.lock().await;
                    outputs.get(device_id)
                };
                if let Some(output_arc) = output_arc {
                    let output = output_arc.lock().await;
                    // Exclusive-mode local outputs (ASIO / WASAPI exclusive) take
                    // a dedicated playback loop that returns at EOF without
                    // consuming the staged next_media — they cannot chain
                    // internally. Arming gapless for them orphans the staged
                    // track AND arms the poller guard, which suppresses the
                    // natural-end advance: a single-track Repeat queue never
                    // loops, and multi-track albums stall after each track
                    // (DEvir, ASIO Fireface USB). Skip arming; the natural-end
                    // fallback advances the queue (a small gap, never a stall).
                    if !output.supports_internal_gapless() {
                        info!(zone_id, "gapless_skipped_exclusive_output");
                        return GaplessPrep::NotArmed;
                    }
                    // DSD gapless guard for DLNA renderers (HiFi Rose RS130,
                    // Benjithom). They accept SetNextAVTransportURI for a DSD
                    // stream but never transition to it — the next stream is
                    // never consumed (bytes_sent stays 0) and the poller
                    // force-stops the zone after STOPPED_FAILURE_THRESHOLD ticks,
                    // i.e. "the album cuts after track 1". Don't arm gapless for a
                    // DSD next on DLNA; handle_track_end plays it explicitly at
                    // end-of-track instead (a small gap, never a cut). Local
                    // output keeps its internal DSD gapless chain untouched.
                    if output.output_type() == "dlna" {
                        let url_lc = resolved.url.to_lowercase();
                        let next_is_dsd = crate::playback::gapless::est_dsd(&resolved.mime_type)
                            || url_lc.ends_with(".dsf")
                            || url_lc.ends_with(".dff");
                        if next_is_dsd {
                            info!(
                                zone_id,
                                mime = %resolved.mime_type,
                                "gapless_skipped_dsd_next_dlna"
                            );
                            return GaplessPrep::DsdNextSkipped;
                        }
                    }
                    let media = crate::outputs::PlayMedia {
                        url: &resolved.url,
                        mime_type: &resolved.mime_type,
                        title: Some(&resolved.title),
                        artist: resolved.artist.as_deref(),
                        album: resolved.album.as_deref(),
                        cover_url: resolved.cover_url.as_deref(),
                        duration_ms: resolved.duration_ms,
                        file_size: resolved.file_size,
                        file_path: None,
                        sample_rate: resolved.sample_rate,
                        bit_depth: resolved.bit_depth,
                        channels: resolved.channels,
                        live_stream: false,
                        byte_seekable: true,
                        origin_url: None,
                        source: resolved.source.as_deref(),
                        source_id: resolved.source_id.as_deref(),
                        track_number: resolved.track_number,
                        disc_number: resolved.disc_number,
                    };
                    if let Err(e) = output.set_next_media(&media).await {
                        warn!(zone_id, error = %e, resolve_ms, "gapless_set_next_failed");
                        GaplessPrep::NotArmed
                    } else {
                        info!(
                            zone_id,
                            title = %resolved.title,
                            resolve_ms,
                            streaming = is_streaming,
                            "gapless_next_set"
                        );
                        GaplessPrep::Armed(arme)
                    }
                } else {
                    GaplessPrep::NotArmed
                }
            }
            Err(e) => {
                warn!(
                    zone_id,
                    error = %e,
                    resolve_ms = t0.elapsed().as_millis() as u64,
                    "gapless_resolve_failed"
                );
                GaplessPrep::NotArmed
            }
        }
    }

    /// Radio « artistes similaires » servie par le service de streaming.
    ///
    /// Le pendant streaming de `auto_dj::generate_similar_artists_queue`, qui
    /// ne sait produire que des pistes de la bibliothèque locale. Renvoie le
    /// nombre de pistes ajoutées — 0 si le service est absent, non authentifié,
    /// ou ne trouve rien : la radio se tait alors comme avant, sans casser la
    /// lecture.
    async fn autoplay_streaming_radio(
        &self,
        zone_id: i64,
        seed_artist: &str,
        source: &str,
        seed_source_id: Option<&str>,
    ) -> usize {
        let Some(service) = self.orchestrator.services.lock().await.get(source) else {
            warn!(zone_id, source, "autoplay_streaming_service_absent");
            return 0;
        };

        // Source 1 : l'API d'enrichissement. Elle ne repond que par MBID, et
        // une piste de streaming n'en transporte aucun — en pratique elle rend
        // toujours zero candidat sur une ecoute Qobuz (#1553).
        let names = crate::playback::auto_dj::similar_artist_names(&self.db, seed_artist, 20).await;
        let from_enrichment = !names.is_empty();

        // Source 2 : le service lui-meme. Deux appels reseau, pas un de plus.
        // On garde les IDENTIFIANTS de catalogue, pas seulement les noms : ils
        // permettent ensuite de demander « des titres DE cet artiste » plutot
        // que « des titres qui contiennent son nom ».
        let mut service_artists: Vec<crate::streaming::traits::StreamArtist> = Vec::new();
        if names.is_empty() {
            info!(
                zone_id,
                seed_artist, source, "autoplay_streaming_enrichment_empty_trying_service"
            );
            service_artists = crate::playback::auto_dj::service_similar_artists(
                seed_artist,
                20,
                |query| {
                    let service = service.clone();
                    async move {
                        let svc = service.read().await;
                        match svc.search(&query, 10).await {
                            Ok(res) => res.artists,
                            Err(e) => {
                                warn!(artist = %query, error = %e, "autoplay_streaming_artist_search_failed");
                                Vec::new()
                            }
                        }
                    }
                },
                |artist_id| {
                    let service = service.clone();
                    async move {
                        let svc = service.read().await;
                        match svc.get_similar_artists(&artist_id, 20).await {
                            Ok(artists) => artists,
                            Err(e) => {
                                warn!(artist_id = %artist_id, error = %e, "autoplay_streaming_similar_failed");
                                Vec::new()
                            }
                        }
                    }
                },
            )
            .await;
        }

        if names.is_empty() && service_artists.is_empty() {
            // Les DEUX sources sont muettes : c'est ici que la file s'arrete,
            // et c'est la ligne que doit trouver quiconque diagnostique un
            // « autoplay qui ne fait rien ».
            warn!(
                zone_id,
                seed_artist, source, "autoplay_streaming_no_similar_names_from_any_source"
            );
            return 0;
        }
        let candidates = if from_enrichment {
            names.len()
        } else {
            service_artists.len()
        };
        info!(
            zone_id,
            source, seed_artist, candidates, from_enrichment, "autoplay_streaming_candidates"
        );

        // Ne jamais reproposer ce qu'on vient d'entendre, ni ce qui est deja
        // dans la file : une radio qui rejoue la piste qui se termine n'est pas
        // une radio.
        let mut exclude: std::collections::HashSet<String> = std::collections::HashSet::new();
        if let Some(id) = seed_source_id {
            exclude.insert(id.to_string());
        }
        if let Ok(rows) = crate::db::play_queue_repo::PlayQueueRepo::with_backend(self.db.clone())
            .get_ordered(zone_id)
        {
            exclude.extend(rows.into_iter().filter_map(|r| r.source_id));
        }

        // Deux facons de transformer un voisin en piste jouable :
        //  - via l'API d'enrichissement on n'a qu'un NOM, donc une recherche ;
        //  - via le service on a son identifiant de catalogue, donc ses titres
        //    a lui. La recherche par nom reste le repli quand l'artiste n'a pas
        //    de titres exposes.
        let names_by_id: std::collections::HashMap<String, String> = service_artists
            .iter()
            .map(|a| (a.id.clone(), a.name.clone()))
            .collect();
        let keys: Vec<String> = if from_enrichment {
            names.clone()
        } else {
            service_artists.iter().map(|a| a.id.clone()).collect()
        };

        let found =
            crate::playback::auto_dj::streaming_tracks_for_artist_names(&keys, 10, &exclude, |key| {
                let service = service.clone();
                let artist_name = names_by_id.get(&key).cloned();
                async move {
                    let svc = service.read().await;
                    // Chemin identifiant : les titres DE l'artiste, sans
                    // ambiguite de titre homonyme.
                    if let Some(ref name) = artist_name {
                        match svc.get_artist_top_tracks(&key).await {
                            Ok(tracks) if !tracks.is_empty() => return tracks,
                            Ok(_) => {}
                            Err(e) => {
                                warn!(artist_id = %key, error = %e, "autoplay_streaming_top_tracks_failed");
                            }
                        }
                        return match svc.search(name, 5).await {
                            Ok(res) => res.tracks,
                            Err(e) => {
                                warn!(artist = %name, error = %e, "autoplay_streaming_search_failed");
                                Vec::new()
                            }
                        };
                    }
                    match svc.search(&key, 5).await {
                        Ok(res) => res.tracks,
                        Err(e) => {
                            warn!(artist = %key, error = %e, "autoplay_streaming_search_failed");
                            Vec::new()
                        }
                    }
                }
            })
            .await;
        if found.is_empty() {
            warn!(
                zone_id,
                source, candidates, "autoplay_streaming_no_playable_track"
            );
            return 0;
        }
        let items: Vec<crate::db::play_queue_repo::StreamingQueueItem> = found
            .iter()
            .map(|t| {
                (
                    t.id.clone(),
                    t.title.clone(),
                    t.artist.clone(),
                    t.album.clone(),
                    t.cover_path.clone(),
                    t.duration_ms as i64,
                    Some(source.to_string()),
                    t.track_number.map(|n| n as i64),
                    t.disc_number.map(|n| n as i64),
                )
            })
            .collect();
        let queue_repo = crate::db::play_queue_repo::PlayQueueRepo::with_backend(self.db.clone());
        if let Err(e) = queue_repo.append_streaming_queue(zone_id, &items) {
            warn!(zone_id, error = %e, "autoplay_streaming_append_failed");
            return 0;
        }
        if let Some(ref bus) = self.event_bus {
            bus.emit(
                "playback.autoplay_tracks_added",
                serde_json::json!({
                    "zone_id": zone_id,
                    "source": source,
                    "seed_artist": seed_artist,
                    "count": items.len(),
                }),
            );
        }
        items.len()
    }

    fn get_zone_device_id(&self, zone_id: i64) -> Option<String> {
        ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod status_timeout_tests;

#[cfg(test)]
mod cadence_de_repos_tests;
#[cfg(test)]
mod gapless_stage_expiry_tests;

/// Metadonnees radio sur une zone SANS peripherique de sortie.
///
/// Le poller quittait toute zone sans peripherique avant d'arriver au
/// rafraichissement des metadonnees : sur « Cet ordinateur », l'appel n'a
/// jamais existe. Ce n'etait donc pas une regression, et c'est pourquoi deux
/// testeurs sur la meme station et la meme version obtenaient des resultats
/// opposes — l'un sur une vraie sortie, l'autre sur le navigateur.
#[cfg(test)]
mod radio_metadata_deviceless_tests;

/// AutoPlay : suivre la source de ce qu'on ecoute (#1553).
///
/// Sandro, 0.9.70 : en ecoute Qobuz, l'autoplay enchainait des titres LOCAUX,
/// ou rien du tout. Le repli streaming existait (#1443) mais restait
/// conditionne a « rien trouve en local » — donc jamais chez qui possede une
/// bibliotheque garnie.
#[cfg(test)]
mod autoplay_source_tests;

#[cfg(all(test, feature = "oaat"))]
mod bascule_des_niveaux_tests {
    use super::PositionPoller;
    use crate::db::zone_repo::ZoneRepo;
    use crate::event_bus::EventBus;
    use crate::outputs::OutputRegistry;
    use crate::outputs::oaat::OaatOutput;
    use crate::playback::PlaybackManager;
    use crate::streaming::ServiceRegistry;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// La dette de test relevee par JP Robbe sur #2280 : rien ne couvrait
    /// `annoncer_bascule_des_niveaux`, ni son emission de `zone.updated`.
    ///
    /// Supprimer l'appel du poller ou le `bus.emit(...)` laissait les tests de
    /// #2280 VERTS, alors que le client ne refetch plus et que
    /// `levels_available` reste fige pendant exactement la lecture DSD.
    ///
    /// Le scenario est celui de son ticket, dans l'ordre : valeur initiale,
    /// bascule en DSD natif, re-poll sans changement, retour en PCM.
    #[tokio::test]
    async fn la_bascule_est_annoncee_une_fois_et_une_seule() {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);

        let device_id = "oaat:zicmu-test";
        let repo = ZoneRepo::with_backend(db.clone());
        let zone_id = repo.create("Zicmu", Some("oaat"), Some(device_id)).unwrap();

        let outputs = Arc::new(Mutex::new(OutputRegistry::new()));
        outputs.lock().await.register(Box::new(OaatOutput::new(
            "Zicmu".into(),
            "192.168.1.99".into(),
            9000,
            device_id.into(),
        )));

        let playback = Arc::new(PlaybackManager::new());
        let orchestrator = Arc::new(crate::orchestrator::PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            Arc::new(crate::http::streamer::AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            outputs.clone(),
            None,
        ));

        let bus = Arc::new(EventBus::new());
        let mut recu = bus.subscribe();
        let poller = PositionPoller::new(
            orchestrator,
            playback,
            outputs.clone(),
            db.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        )
        .with_event_bus(bus.clone());

        let compter = |recu: &mut tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>| {
            let mut n = 0;
            while let Ok(ev) = recu.try_recv() {
                if ev.event_type == "zone.updated"
                    && ev.data.get("zone_id").and_then(|v| v.as_i64()) == Some(zone_id)
                {
                    n += 1;
                }
            }
            n
        };

        let mut dernier: HashMap<i64, bool> = HashMap::new();

        // 1. Valeur initiale : une zone jamais annoncee doit l'etre, sinon une
        //    zone deja en DSD au demarrage resterait muette.
        poller.annoncer_bascule_des_niveaux(&mut dernier).await;
        assert_eq!(compter(&mut recu), 1, "la valeur initiale doit s'annoncer");
        assert_eq!(dernier.get(&zone_id), Some(&true), "en PCM, on mesure");

        // 2. Bascule en DSD natif : exactement un evenement.
        {
            let registre = outputs.lock().await;
            let arc = registre.get(device_id).unwrap();
            let sortie = arc.lock().await;
            sortie
                .as_any()
                .downcast_ref::<OaatOutput>()
                .unwrap()
                .set_native_dsd_active_for_test(true);
        }
        poller.annoncer_bascule_des_niveaux(&mut dernier).await;
        assert_eq!(compter(&mut recu), 1, "la bascule doit s'annoncer");
        assert_eq!(
            dernier.get(&zone_id),
            Some(&false),
            "en DSD natif, rien ne mesure"
        );

        // 3. Re-poll sans changement : aucun doublon.
        poller.annoncer_bascule_des_niveaux(&mut dernier).await;
        assert_eq!(compter(&mut recu), 0, "pas d'evenement sans changement");

        // 4. Retour en PCM : un second evenement.
        {
            let registre = outputs.lock().await;
            let arc = registre.get(device_id).unwrap();
            let sortie = arc.lock().await;
            sortie
                .as_any()
                .downcast_ref::<OaatOutput>()
                .unwrap()
                .set_native_dsd_active_for_test(false);
        }
        poller.annoncer_bascule_des_niveaux(&mut dernier).await;
        assert_eq!(compter(&mut recu), 1, "le retour en PCM doit s'annoncer");
    }
}

/// Le repli sur le logo de la station — #2421, fil forum 1508.
///
/// Belkadi Yacine : « pas de jaquettes lors de l'écoute des radios ». La
/// pochette du MORCEAU a été câblée depuis (74677e35, v0.9.97) ; ce qui
/// n'avait jamais marché, c'est le repli sur le logo de la station quand la
/// station ne donne rien — une chronique, un jingle, un flash info, ou une
/// station qui n'expose aucune API de now-playing.
#[cfg(test)]
mod repli_logo_station_tests;

/// Garde-fou #1998 : c'est le poller qui LIBÈRE l'annonce d'une zone
/// navigateur.
///
/// Le démarrage ne peut pas trancher pour une telle zone : sans périphérique de
/// sortie, `output_sent` y vaut toujours faux, qu'on écoute ou non. La preuve
/// — l'onglet tire réellement le flux — n'apparaît qu'après coup, donc dans la
/// boucle de scrutation. Si cet appel disparaît de la branche « zone sans
/// périphérique », plus aucune zone navigateur ne scrobble : c'est exactement
/// la régression pour laquelle ce ticket a été rouvert, et elle est silencieuse.
///
/// Relecture de source parce que la propriété tenue est un EMPLACEMENT dans une
/// boucle de plusieurs milliers de lignes. Même procédé, même raison que
/// `annonce_apres_sortie_guard` dans `orchestrator.rs`.
#[cfg(test)]
mod annonce_navigateur_guard;

/// #2630 — une lecture que personne ne reçoit cesse d'être annoncée.
///
/// Le journal de Pierre M, zone 987 : `no_output_device_id_skipping_send_to_output`
/// puis `orchestrator_play … output_sent=false`. Le serveur constate qu'il n'a
/// envoyé le titre nulle part, et l'annonce quand même. Rien ne revenait
/// ensuite sur cet état : la branche « pas de périphérique » du poller se
/// terminait par un `continue`.
///
/// La ligne de crête est ici : une zone navigateur n'a JAMAIS de périphérique
/// et joue pourtant pour de vrai. Le verdict ne porte donc pas sur l'appareil
/// mais sur la CONSOMMATION du flux — la preuve que #2657 a déjà introduite.
/// #3108 — le maillon 2 de la chaîne : le poller draine-t-il vraiment
/// `take_output_failure()`, et l'écran reçoit-il de quoi parler ?
///
/// Rien ne le couvrait. Le bloc qui émet `zone.playback_error` vivait dans
/// `tick()` sans un seul test : on pouvait en retirer le `bus.emit` et toute la
/// suite restait verte, pendant qu'un refus de sortie exclusive redevenait
/// muet. C'est exactement le motif du dépôt — « le champ que l'écran ne lit
/// pas » — et il faut une porte, pas une lecture de code.
///
/// La position n'est jamais observée en dormant : le banc pose l'état, appelle
/// `tick()` UNE fois, et compte ce qui en sort.
#[cfg(test)]
mod remontee_des_pannes_de_sortie_tests;

#[cfg(test)]
mod lecture_sans_destination_tests;

/// #2493 — garde-fou d'EMPLACEMENT : le constat de depassement DIT, il n'agit
/// pas.
///
/// La propriete tenue n'est pas une valeur, c'est une absence dans un bloc
/// precis d'une boucle de plusieurs milliers de lignes : entre l'appel a
/// `position_au_dela_de_la_duree` et la ligne de journal qu'il declenche, il ne
/// doit y avoir NI `track_ended = true` NI `force_stop = true`.
///
/// C'est tout l'arbitrage du ticket. La meme forme — position a la fin,
/// appareil qui dit jouer — est produite par une lecture bloquee ET par une
/// duree fausse (etiquette erronee, piste reellement plus longue). Aucune
/// horloge ne sait les distinguer, donc agir couperait une ecoute legitime une
/// fois sur deux. Le jour ou quelqu'un « complete » ce bloc par un saut de
/// piste, ce test doit l'arreter.
///
/// Meme procede, meme raison que `annonce_navigateur_guard` : relecture de
/// source parce que la propriete est un emplacement.
#[cfg(test)]
mod depassement_duree_nagit_pas_guard;

/// #3026 — « Lire ensuite » pendant la fenêtre gapless de 30 s.
///
/// Sandro, 0.9.127 Linux, Diretta Renderer, fil forum 1622. Deux occurrences à
/// 24 h d'intervalle, même forme exacte : `dlna_set_next` part, l'utilisateur
/// insère une piste 15 s (puis 18 s) plus tard, et **rien ne repart vers le
/// renderer** — le journal ne porte aucun `dlna_set_next`, `dlna_set_uri_ok` ni
/// `dlna_play` entre l'insertion et la transition. L'écran avance alors sur
/// l'index+1 courant — l'insérée — pendant que le renderer joue ce qu'on lui
/// avait envoyé. Le 01/09 la conséquence devient audible : le compteur de fin
/// de piste adopte la durée de l'INSÉRÉE et coupe le flux réellement en cours
/// (`position_past_end_advancing … dlna_frozen_end=true` à 14:23:10).
///
/// Ces tests mesurent LE FAIT DE BASE : **quelle piste part réellement au
/// renderer après le geste, et dans quel ordre**, sur un renderer factice qui
/// note ce qu'il reçoit. Aucun code HTTP n'y sert de preuve.
///
/// **Aucun `sleep`.** La fenêtre des trente secondes est atteinte en écrivant
/// la position du renderer et l'horloge de piste du poller — deux champs
/// d'état. Un test qui attendrait trente secondes est un test qu'on finirait
/// par désarmer.
#[cfg(test)]
mod lire_ensuite_dans_la_fenetre_gapless;

/// Garde-fou #2991 — les DEUX branchements que ce ticket a posés vivent chacun
/// à un EMPLACEMENT précis d'une boucle de dix mille lignes. Retirés, ils
/// restent compilés, testés et verts — et plus personne ne les appelle : le
/// registre du canal ne serait plus lu, et la reprise reposerait `None`. C'est
/// exactement la classe de régression silencieuse que ce ticket décrit.
///
/// Relecture de source, même procédé et même raison que
/// `annonce_navigateur_guard` ci-dessus.
#[cfg(test)]
mod canal_radio_guard;

/// #3229 — le sondeur doit ÉMETTRE la position qu'il a fait publier.
///
/// La garde de monotonie vit dans `PlaybackManager::update_position`, qui rend
/// la position retenue. Deux surfaces portent cette position jusqu'à l'écran, et
/// elles sont écrites l'une sous l'autre : l'état de zone (`GET /zones`) et
/// l'évènement `position`. Réémettre la valeur BRUTE au lieu de la valeur
/// retenue rendrait la garde inopérante là où elle compte le plus — le curseur
/// reculerait quand même, pendant que l'API dirait le contraire.
///
/// Aucune épreuve de comportement ne peut voir cet écart-là : les deux appels
/// réussissent, avec des valeurs différentes. On relit donc la source, même
/// procédé et même raison que `canal_radio_guard` ci-dessus.
#[cfg(test)]
mod position_publiee_guard;
