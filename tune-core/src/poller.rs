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
pub(crate) mod decisions;

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
pub mod fsm;

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

mod fin_de_piste;

mod tick;

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
