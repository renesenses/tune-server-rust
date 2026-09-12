use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rubato::{
    Async, FixedAsync, SincInterpolationParameters, SincInterpolationType, WindowFunction,
    calculate_cutoff,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::traits::{
    AudioSpec, BlocPcm, FormatOuvert, OutputCapabilities, OutputDspMetrics, OutputRingStarvation,
    OutputSignalPathStatus, OutputStatus, OutputTarget, PuitsDEchantillons, RingStarvation,
    TransformationsReelles, TransportState,
};
#[cfg(any(target_os = "windows", test))]
use super::traits::{OutputDspState, OutputSampleTransport, OutputSignalReason, OutputVolumeState};
use crate::poller::TRACK_END_NOTIFY;

/// Why a device refused to open, as far as the backend string lets us tell.
///
/// Two audiences need this, and they need different words: the log is read by
/// us, in English, alongside the raw backend error; the toast is read by
/// someone whose music just didn't start, in their language, and must say what
/// to *do*. Classifying once and rendering twice keeps the two from drifting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenFailure {
    /// The sound server is not reachable, or the account may not open the
    /// device at all.
    ServerUnreachable,
    /// The device disappeared between selection and playback.
    DeviceGone,
    /// Another application holds the device exclusively.
    Busy,
    /// Le backend a dit « indisponible » sans dire pourquoi.
    ///
    /// cpal 0.17.3 replie SIX errno distincts — `ENOENT`, `EPERM`, `ENODEV`,
    /// `ENOTSUPP`, `EBUSY`, `EAGAIN` — sur un seul
    /// `BuildStreamError::DeviceNotAvailable`
    /// (`cpal-0.17.3/src/host/alsa/mod.rs:358-363`, et le même bloc à 458-463
    /// pour l'énumération), dont le `Display` est toujours la même phrase :
    /// « The requested device is no longer available. For example, it has been
    /// unplugged. »
    ///
    /// Le motif est donc DÉTRUIT avant d'arriver ici : ni [`Self::DeviceGone`]
    /// (`no such device`) ni [`Self::Busy`] (`busy` / `in use`) ne peuvent plus
    /// être atteints sur ALSA, et le cas tombait dans [`Self::Unknown`], dont
    /// la phrase — « le périphérique a refusé tous les formats proposés » —
    /// accuse le FORMAT alors que le périphérique n'a jamais été ouvert et que
    /// changer de format n'y changera rien.
    ///
    /// Mesuré sur le relevé de Belkadi Yacine (#3575) : DIX échecs, tous avec
    /// cette phrase et pas un autre motif, sur un DAC que l'énumération
    /// retrouvait à chaque tour (`local_audio_devices_enumerated count=6`, dix
    /// fois en 43 min). Nommer l'ambiguïté vaut mieux que la trancher au
    /// hasard.
    IndisponibleMotifPerdu,
    /// Nothing matched — say so plainly rather than guess.
    Unknown,
}

/// Classify a device-open error.
///
/// The backend strings are written for driver authors, not for the person whose
/// music stopped: ALSA reports an unreachable PipeWire daemon as
/// `Host is down (112)`, which reads like a network fault and sends everyone
/// looking at the wrong layer. It cost a full morning with Yacine on 8 Aug
/// 2026 — and the real cause turned out to be a third thing again, an account
/// missing from the `audio` group on a machine driven over SSH, where logind
/// grants no device ACL because there is no local seat. Both faults surface as
/// the same string, hence the deliberately broad wording of that arm.
///
/// Matching is loose on purpose: cpal wraps the backend message and the wording
/// varies by platform, so anything unrecognised falls through to `Unknown`
/// rather than to a confident wrong answer.
fn classify_open_failure(err: &str) -> OpenFailure {
    let e = err.to_ascii_lowercase();
    if e.contains("host is down")
        || e.contains("connection refused")
        || e.contains("permission denied")
        || e.contains("access denied")
    {
        OpenFailure::ServerUnreachable
    } else if e.contains("no longer available") {
        // La phrase de repli de cpal. Elle doit être testée AVANT les motifs
        // fins : ceux-ci cherchent des mots que ce message ne porte pas, si
        // bien que sans cette branche le cas le plus fréquent sur ALSA tombait
        // dans `Unknown`.
        OpenFailure::IndisponibleMotifPerdu
    } else if e.contains("no such device") || e.contains("no such file") {
        OpenFailure::DeviceGone
    } else if e.contains("busy") || e.contains("in use") {
        OpenFailure::Busy
    } else {
        OpenFailure::Unknown
    }
}

impl OpenFailure {
    /// English, for the log, next to the raw backend error.
    fn log_hint(self) -> &'static str {
        match self {
            Self::ServerUnreachable => {
                "the sound server (PipeWire/PulseAudio) is unreachable, or this account \
                 cannot open the device — check that Tune runs in the owning user session \
                 and that the account is in the `audio` group"
            }
            Self::DeviceGone => {
                "the device is gone — a USB DAC unplugged or powered off since it was selected"
            }
            Self::Busy => "the device is held exclusively by another application",
            Self::IndisponibleMotifPerdu => {
                "the backend collapsed the errno: cpal maps ENOENT/EPERM/ENODEV/EBUSY/EAGAIN \
                 onto one `DeviceNotAvailable`, so the device is either GONE or already HELD \
                 by an exclusive opener — including a previous stream of ours that has not \
                 released the PCM yet — and cpal no longer says which"
            }
            Self::Unknown => {
                "the device refused every format offered — it may be unavailable or misconfigured"
            }
        }
    }

    /// French, for the toast. Says what to do, not what failed internally.
    fn user_message(self) -> &'static str {
        match self {
            Self::ServerUnreachable => {
                "le service audio ne répond pas, ou Tune n'a pas le droit d'ouvrir ce \
                 périphérique. Vérifiez que le serveur audio est démarré et que le compte \
                 qui exécute Tune appartient au groupe « audio »"
            }
            Self::DeviceGone => {
                "le périphérique n'est plus là. Vérifiez qu'il est allumé et connecté, \
                 puis choisissez-le à nouveau dans les réglages de la zone"
            }
            Self::Busy => {
                "un autre programme utilise déjà ce périphérique en exclusivité. \
                 Fermez-le, puis relancez la lecture"
            }
            Self::IndisponibleMotifPerdu => {
                "ce périphérique n'a pas pu être ouvert : il est soit débranché ou éteint, \
                 soit déjà utilisé en exclusivité. Vérifiez qu'il est allumé et connecté, \
                 fermez l'application qui l'utilise, puis relancez la lecture"
            }
            Self::Unknown => {
                "le périphérique a refusé tous les formats proposés. Choisissez une autre \
                 sortie dans les réglages de la zone"
            }
        }
    }
}

/// Budget d'attente accordé au fil de lecture PRÉCÉDENT pour rendre le PCM.
///
/// `stop()` accepte déjà d'attendre 2 000 ms sa sortie, puis le DÉTACHE
/// (`local_audio_stop_thread_detached`). Ce budget-ci s'ajoute à celui-là, et
/// il est délibérément court : quelqu'un vient d'appuyer sur Lecture.
pub(crate) const BUDGET_RELACHE_PERIPHERIQUE_MS: u64 = 1_500;

/// Pas entre deux vérifications de la sentinelle du fil précédent.
pub(crate) const PALIER_RELACHE_PERIPHERIQUE_MS: u64 = 50;

/// Que faire quand notre PROPRE fil de lecture précédent n'a peut-être pas
/// fini de rendre le périphérique.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RelacheDuPeripherique {
    /// Le fil précédent a rendu le PCM : ouvrir maintenant.
    Libre,
    /// Il le tient encore, et le budget n'est pas épuisé : repasser plus tard.
    Attendre { apres_ms: u64 },
    /// Il le tient encore, mais le budget est épuisé : ouvrir quand même, et
    /// le DIRE. On n'ajoute pas une panne d'attente à une panne d'ouverture —
    /// et l'ouverture peut très bien réussir, le fil détaché ayant pu rendre
    /// le PCM entre deux réveils.
    ForcerEtLeDire,
}

/// #3575 — le périphérique que Tune se prend à lui-même.
///
/// Depuis `ee4ec884` (« préférer le PCM matériel `hw:` au greffon qui accepte
/// tout », 02/09/2026, première version publiée qui le porte : **v0.9.132**,
/// mesuré par `git tag --contains`), une sortie locale Linux n'ouvre plus un
/// greffon
/// PARTAGEABLE (`sysdefault:`, `front:`, PipeWire) mais le PCM matériel
/// `hw:CARD=…,DEV=…`, qui n'accepte **qu'un seul ouvreur**.
///
/// Le ticket dit « après la mise à jour 0.9.140 » : c'est la version que
/// Belkadi Yacine exécutait, pas nécessairement celle qui a introduit le
/// défaut. De quelle version il venait n'est pas mesuré, et rien ici ne le
/// suppose.
///
/// Or `play_url` enchaîne `stop()` puis une ouverture 50 ms plus tard, et
/// `stop()` ne garantit RIEN : il attend la sortie du fil précédent 2 000 ms,
/// puis le détache s'il est encore là — « the thread will exit on its own once
/// the blocking read returns », dit son propre commentaire. Ce fil détaché
/// tient toujours le flux cpal, donc le PCM. Tant que le greffon était
/// partageable le recouvrement passait inaperçu ; sur `hw:` il rend `EBUSY`,
/// que cpal replie sur « The requested device is no longer available »
/// ([`OpenFailure::IndisponibleMotifPerdu`]), et la zone s'arrête.
///
/// Relevé de Belkadi Yacine (#3575), **13 ouvertures instrumentées sur 13, zéro
/// contre-exemple** : les DIX échecs portent `device_default_sr=None` — la
/// sonde `default_output_config()` avait déjà pris le refus sur le MÊME PCM
/// quelques millisecondes plus tôt — et les TROIS réussites portent une cadence
/// par défaut réellement lue. Le processus sain
/// rejoue d'ailleurs l'échec à 12:14:38, 2,7 s après avoir ouvert le même
/// `alsa:hw:CARD=2,DEV=0`, puis réussit à 12:15:00 : ce n'est ni un
/// périphérique mort ni un renommage, c'est un RECOUVREMENT.
///
/// L'attente n'est accordée que si l'on peut NOMMER le teneur — notre propre
/// fil. Contre un périphérique réellement débranché, ou tenu par un autre
/// programme, attendre ne ferait que retarder le message.
pub(crate) fn decider_la_relache_du_peripherique(
    fil_precedent_encore_vivant: bool,
    attendu_ms: u64,
    budget_ms: u64,
) -> RelacheDuPeripherique {
    if !fil_precedent_encore_vivant {
        return RelacheDuPeripherique::Libre;
    }
    if attendu_ms >= budget_ms {
        return RelacheDuPeripherique::ForcerEtLeDire;
    }
    RelacheDuPeripherique::Attendre {
        apres_ms: PALIER_RELACHE_PERIPHERIQUE_MS.min(budget_ms - attendu_ms),
    }
}

/// Dit « ce fil vit encore » aussi longtemps qu'il existe.
///
/// Déclarée en PREMIER dans le fil de lecture, elle est donc détruite en
/// DERNIER : la sentinelle ne retombe qu'après le `Drop` du flux cpal, c'est-
/// à-dire après la fermeture effective du PCM. L'ordre est ce qui fait la
/// preuve — une sentinelle qui retomberait avant le flux annoncerait un
/// périphérique libre qui ne l'est pas.
pub(crate) struct SentinelleDuFilDeLecture(pub(crate) Arc<AtomicBool>);

impl Drop for SentinelleDuFilDeLecture {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// #3575 — dire QUI tient le PCM, au moment exact où nous n'arrivons pas à
/// l'ouvrir.
///
/// [`SentinelleDuFilDeLecture`] et [`decider_la_relache_du_peripherique`]
/// (v0.9.145) ne connaissent qu'un seul teneur possible : **notre propre fil
/// précédent**. Leur auteur l'a écrit dans la PR #3753 — un PCM tenu par une
/// **instance précédente du processus**, par un descripteur qui aurait survécu
/// à l'`execv` de mise à jour, ou par un tout autre programme (Lyrion/LMS,
/// `aplay`, un PipeWire en accès direct) leur est invisible. C'est pourtant
/// l'hypothèse centrale du ticket, celle qui expliquerait « imprenable pour
/// toute la vie du processus ».
///
/// Depuis le 07/09/2026 la même observation est redemandée à chaque tour et
/// n'arrive jamais, parce qu'elle exige d'être prise **avant** le redémarrage
/// qui l'efface :
///
/// ```text
/// fuser -v /dev/snd/*
/// ps -ef | grep -c "[t]une-server"
/// ```
///
/// Cette fonction la prend toute seule, à la milliseconde où le refus tombe.
/// Elle **n'ouvre rien, ne ferme rien, ne tue personne et n'attend pas** : sur
/// une P0 de sortie audio, une rustine qui « libère » un PCM rendrait muette la
/// chaîne d'un testeur. Elle écrit une ligne, et c'est tout.
///
/// Elle ne s'exécute que sur le motif [`OpenFailure::IndisponibleMotifPerdu`],
/// celui où cpal a replié `EBUSY` sur « no longer available » : ailleurs, le
/// motif est connu et il n'y a pas de teneur à chercher.
///
/// ⚠️ Un `teneurs=aucun_teneur_visible` **n'est pas** la preuve que personne ne
/// tient le nœud : `/proc/<pid>/fd` d'un autre compte n'est pas lisible sans
/// privilège. Il dit « je n'ai vu personne », et c'est déjà une information que
/// l'on n'avait pas.
#[cfg(target_os = "linux")]
fn journaliser_les_teneurs_du_pcm(endpoint_id: &str, device_name: &str) {
    use crate::audio::pcm_teneur;
    let Some((noeud, teneurs)) = pcm_teneur::relever_les_teneurs(endpoint_id) else {
        // PCM partageable, `/proc/asound/cards` absent, endpoint illisible :
        // il n'y a rien à dire, et taire vaut mieux que designer un coupable.
        return;
    };
    warn!(
        device = %device_name,
        endpoint_id,
        noeud = %noeud.chemin(),
        teneurs = %pcm_teneur::resume_des_teneurs(&teneurs),
        teneur_etranger = pcm_teneur::un_teneur_etranger(&teneurs),
        "local_audio_pcm_holder_probe — qui tient le PCM que nous ne pouvons pas ouvrir (#3575)"
    );
}

mod etat_backend;
pub use etat_backend::*;

mod parc;
pub use parc::*;

// R6 bis (#2219) : les trois bras exclusifs de `play_url`, un module chacun,
// sous le MÊME `cfg` que le bloc qu'ils remplacent. Le bras cpal partagé reste
// dans `play_url` (fil de R1/R7).
#[cfg(all(target_os = "windows", feature = "asio"))]
mod bras_asio;
#[cfg(target_os = "macos")]
mod bras_coreaudio;
#[cfg(target_os = "windows")]
mod bras_wasapi;

// REF-8 (#2219) : le trait backend minimal et son premier implémenteur, CPAL
// partagé. Le bras CPAL de `play_url` l'appelle : ouvrir, puits, démarrer,
// observer, drainer.
mod backend;
use backend::{BackendCpal, BackendLocal, DemandeDOuverture, Puits};

// ---------------------------------------------------------------------------
// Gapless: pending next track for seamless chaining
// ---------------------------------------------------------------------------

/// Stores the next track's metadata for gapless playback.
/// When the current track reaches clean HTTP EOF and this is set,
/// the playback thread chains directly into the next track without
/// closing/reopening the audio device.
#[derive(Clone)]
struct PendingNextMedia {
    url: String,
    title: Option<String>,
    artist: Option<String>,
    duration_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// LocalOutput — streams audio from an HTTP URL to a local audio device
// ---------------------------------------------------------------------------

pub struct LocalOutput {
    device_name: String,
    device_id: String,
    /// Stable backend endpoint captured at discovery. The public registry ID
    /// remains compatible (`local:<display name>`), while exclusive WASAPI
    /// opens this exact IMMDevice instead of resolving the name again.
    ///
    /// Toutes plateformes désormais : `find_device_with_fallback` le consulte
    /// **avant** le nom, seule façon de survivre à un renommage (#2269) et de
    /// ne pas confondre deux périphériques homonymes (#2272).
    endpoint_id: Option<String>,
    /// L'hôte audio qui a énuméré `device_name` (`AudioDevice::backend`).
    ///
    /// #3230 : le nom seul ne dit pas d'où il vient. Une zone née d'une
    /// énumération WASAPI garde un nom WASAPI ; si la lecture ouvre ensuite
    /// l'hôte ASIO — ce que `select_host("asio")` fait dès qu'ASIO expose une
    /// sortie — ce nom ne désigne plus rien, et le repli envoyait le son sur
    /// le périphérique ASIO par défaut. `None` = origine inconnue (sortie
    /// recréée à la volée, zone d'avant ce correctif) : on ne refuse rien.
    origin_host: Option<String>,
    playing: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    /// What the playback callbacks actually multiply by: the user volume
    /// scaled by the ReplayGain factor. Composing here means the dozen places
    /// that read a volume in the render loops need no knowledge of ReplayGain.
    volume: Arc<AtomicU32>,
    /// The volume the user asked for, in milli-units — what the UI shows and
    /// what mute restores. Kept apart from `volume` so a ReplayGain
    /// attenuation never looks like the slider moved on its own.
    user_volume: Arc<AtomicU32>,
    /// ReplayGain factor for the current track, in milli-units (1000 = 1.0).
    rg_factor: Arc<AtomicU32>,
    /// Volume stored before mute, so unmute can restore it
    pre_mute_volume: Arc<AtomicU32>,
    muted: Arc<AtomicBool>,
    /// Playback position in milliseconds (updated by the streaming thread)
    position_ms: Arc<AtomicU64>,
    /// Offset added to position_ms when stream was seeked (the decoded stream
    /// starts at byte 0 but represents audio from seek_offset_ms onward).
    seek_offset_ms: Arc<AtomicU64>,
    /// One-shot start position supplied by play_media() for recreated seek
    /// streams. play_url() consumes this after stop() clears the old state.
    pending_start_position_ms: AtomicU64,
    /// When true, the audio consumer should NOT skip bytes based on
    /// seek_offset_ms because the decoder already produced a seeked stream.
    /// seek_offset_ms is still used for position reporting (progress bar).
    stream_pre_seeked: AtomicBool,
    /// Track duration in milliseconds
    duration_ms: Arc<AtomicU64>,
    current_uri: Arc<std::sync::Mutex<Option<String>>>,
    track_title: Arc<std::sync::Mutex<Option<String>>>,
    track_artist: Arc<std::sync::Mutex<Option<String>>>,
    stop_tx: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
    /// Handle to the playback thread so `stop()` can wait for it to exit.
    play_thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Vrai tant que le DERNIER fil de lecture lancé n'a pas rendu son flux
    /// cpal — donc tant qu'il peut tenir le PCM exclusif (#3575).
    ///
    /// `play_thread` ne répond pas à cette question : `stop()` le `take()` puis
    /// DÉTACHE le fil quand il déborde des 2 000 ms, et le handle disparaît
    /// alors qu'un flux cpal bien vivant tient encore `hw:CARD=…`.
    sentinelle_du_fil: std::sync::Mutex<Option<Arc<AtomicBool>>>,
    /// When true (and on macOS), use CoreAudio exclusive/hog mode for
    /// bit-perfect output, bypassing the system mixer.
    exclusive_mode: bool,
    /// Audio backend preference: "auto", "wasapi", or "asio" (Windows only).
    audio_backend: String,
    /// Set by stop() to immediately silence the cpal callback, even if
    /// the playback thread hasn't exited yet.  Prevents overlapping audio
    /// when switching tracks and the old thread is still draining.
    ///
    /// IMPORTANT: This is replaced with a fresh Arc on each new play_url()
    /// call, so that resetting it to `false` for the new stream does NOT
    /// accidentally un-silence the old stream's callback (which keeps its
    /// own clone of the previous Arc).
    force_silent: std::sync::Mutex<Arc<AtomicBool>>,
    play_generation: Arc<AtomicU64>,
    /// Set by the playback thread when it reaches end-of-stream naturally
    /// (i.e. the HTTP source was fully consumed, not stopped by stop()).
    ///
    /// When true, `get_status()` reports the track as still Playing but
    /// with position_ms past the track end, so the poller's
    /// `position_past_end` path fires and triggers auto_next — bypassing
    /// the gapless-guard window that would otherwise delay (or swallow)
    /// the track-end signal when the thread is detached before draining.
    ///
    /// Cleared on every `play_url()` and `stop()` call.
    track_ended_naturally: Arc<AtomicBool>,
    /// The play-generation that set `track_ended_naturally = true`.
    ///
    /// When the playback thread signals natural end-of-stream, it also
    /// stores its own `my_generation` here.  `get_status()` only honours
    /// the flag when the generation matches the *current*
    /// `play_generation`, preventing a detached old thread from
    /// contaminating the new track's status.
    track_ended_generation: Arc<AtomicU64>,
    /// Pending next track for gapless playback.  Set by `set_next_media()`,
    /// consumed by the playback thread when the current track reaches EOF.
    next_media: Arc<std::sync::Mutex<Option<PendingNextMedia>>>,
    /// La boucle d'enchaînement du fil de lecture a-t-elle rendu les armes ?
    ///
    /// `supports_internal_gapless()` était une **capacité statique**
    /// (`!exclusive_mode`) : le chemin cpal partagé affirmait savoir enchaîner
    /// tout seul, y compris longtemps après que sa boucle se soit arrêtée.
    /// C'est exactement le défaut corrigé sur OAAT par #1323 — « la boucle de
    /// flux ne disait pas au poller qu'elle était morte » — et il vit aussi
    /// ici.
    ///
    /// Le fil de lecture abandonne l'enchaînement par six chemins : aucune
    /// piste suivante en réserve, HTTP en erreur, HTTP injoignable, en-tête
    /// vide, **flux suivant qui n'est pas du WAV**
    /// (`local_audio_gapless_next_not_wav_falling_back`), ou piste chaînée qui
    /// n'atteint pas une fin propre. Après chacun d'eux le fil draine puis
    /// sort : plus rien ne peut enchaîner. Le poller, lui, relit la capacité
    /// pendant qu'il attend et y lit toujours `true`.
    ///
    /// Cette réponse devient donc une **sonde vivante** : une boucle terminée
    /// ne peut plus rien enchaîner, quoi qu'elle ait su faire une seconde plus
    /// tôt. Remis à zéro par `play_url()`, qui démarre un fil neuf.
    chain_exhausted: Arc<AtomicBool>,
    /// Zone equalizer for the zone currently playing on this output, applied
    /// BEFORE the room-correction convolver — the same order as the transcoded
    /// path (`transcode_source_to_file`: ReplayGain → EQ → convolver).
    ///
    /// A local (cpal/ASIO/WASAPI) zone never takes the temp-file transcode
    /// path — `use_file_transcode_for` requires a network output — and the
    /// streaming pipe it does take never ran the `EqProcessor`. The equalizer
    /// was therefore applied NOWHERE on a local output: profile saved, curve
    /// drawn, zero effect on the DAC (Jean Marie, forum #1416, deux zones
    /// `local:` dans ses journaux ; même famille que #1216 / #1168 / Diretta).
    /// Set per-play by the orchestrator, exactly like `crossfeed`.
    eq: Arc<std::sync::Mutex<Option<super::super::audio::eq::EqProcessor>>>,
    /// Format effectivement résolu pour le flux en cours : (taux, canaux),
    /// empaquetés dans un seul u32 (taux sur 24 bits, canaux sur 8).
    ///
    /// Un `EqProcessor` se construit POUR un couple (taux, canaux) — d'où sa
    /// reconstruction à chaque lecture. Sans mémoire de ce couple, personne ne
    /// pouvait en rebâtir un pendant la lecture : bouger un curseur écrivait le
    /// profil, le serveur répondait 200, et le son ne changeait pas avant la
    /// piste suivante (#1725). Or c'est exactement ainsi qu'on règle un
    /// égaliseur — musique en cours, à l'oreille.
    ///
    /// 0 = aucun flux en cours ; `current_format()` renvoie alors `None`.
    current_format: Arc<AtomicU32>,
    /// Taps et cadence de l'IR choisie, conservés même entre deux pistes.
    /// L'instance FFT ci-dessous n'est qu'un dérivé du format courant.
    convolver_config:
        Arc<std::sync::Mutex<Option<super::super::audio::convolver::ConvolverConfig>>>,
    convolver: Arc<std::sync::Mutex<Option<super::super::audio::convolver::Convolver>>>,
    /// PURE (audiophile) bypass for the zone currently playing on this output.
    /// When set, the playback loop skips the room-correction convolver so the
    /// signal path stays bit-perfect. Set per-play by the orchestrator.
    pure_bypass: Arc<AtomicBool>,
    /// Optional headphone crossfeed effect, applied AFTER the convolver on the
    /// local (DAC) output only. Gated by the same `pure_bypass` (skipped in
    /// PURE) and only when the stream is stereo. Set per-play by the
    /// orchestrator via `set_crossfeed`.
    crossfeed: Arc<std::sync::Mutex<Option<super::super::audio::crossfeed::CrossfeedProcessor>>>,
    /// Repli mono de la zone en cours de lecture sur cette sortie (#2362).
    ///
    /// Quand il est armé, la chaîne somme `M = (L + R) / 2` et réémet `M` sur
    /// les DEUX voies stéréo, **en dernier** — après l'égaliseur, le convolveur
    /// et le crossfeed, qui ont tous besoin de leur contexte stéréo pour
    /// travailler. La duplication tombe donc juste avant l'adaptation au
    /// périphérique, et le contrat du DAC (deux canaux) ne change pas.
    ///
    /// Défaut `false` : sans geste de l'utilisateur, le comportement est
    /// strictement celui d'avant. Posé par piste par l'orchestrateur, comme
    /// `pure_bypass` et `crossfeed`, et rafraîchissable en vol
    /// (`refresh_zone_mono_downmix`).
    ///
    /// Ce n'est PAS du bit-perfect, et c'est assumé : le panneau « Chemin du
    /// signal » affiche l'étape « Mono » et le verdict tombe.
    mono_downmix: Arc<AtomicBool>,
    /// Durée, en millisecondes, de la rampe de gain anti-« ploc » appliquée à la
    /// pause, à la reprise et à l'arrêt (#1590).
    ///
    /// `0` = coupure franche, c'est-à-dire le comportement d'avant #1590 au bit
    /// près. Posée par piste par l'orchestrateur comme `pure_bypass` et
    /// `mono_downmix` ; l'orchestrateur y met déjà `0` pour une zone PURE.
    ///
    /// Ce n'est **pas** le seul verrou : les callbacks relisent aussi
    /// `dop_active` et `pure_bypass` à chaque tampon, parce qu'un DoP se
    /// découvre en cours de piste et que le mode PURE se bascule en vol. Le
    /// verdict est tranché en un point unique,
    /// [`crate::audio::soft_mute::armed_ms`].
    soft_mute_ms: Arc<AtomicU32>,
    /// True while the PCM currently flowing through this output is a **DoP**
    /// (DSD over PCM) payload, as detected on the bytes themselves by
    /// [`is_dop_pcm`].
    ///
    /// DoP is not audio: it is a DSD bitstream smuggled inside 24-bit PCM
    /// frames, recognised by the receiving DAC through a marker byte. Any
    /// arithmetic on those samples — an equalizer biquad, the convolver, the
    /// crossfeed — rewrites the marker, the DAC stops seeing DoP, and it
    /// **mutes**. That is the whole point of the marker: a DAC must fall silent
    /// rather than blast a DSD bitstream at the speakers as if it were PCM.
    ///
    /// Held on the output rather than kept local to the feed loop so the
    /// transition can be logged exactly once (a support log then says whether a
    /// track played as DoP), and so the remaining half of the problem has a
    /// place to hang: the **volume multiply** in the render callbacks destroys
    /// the marker in the same way, which is why DoP only ever survives at 100 %
    /// with ReplayGain off. That one is older and independent of the DSP chain,
    /// and it changes what the volume slider does on a DSD track — it is
    /// tracked separately rather than smuggled in here.
    dop_active: Arc<AtomicBool>,
    /// Dernier contrat réellement observé avant le callback du backend.
    ///
    /// Les boucles Windows savent déjà si elles publient des mots entiers
    /// natifs ou repassent par f32, et si volume/DSP ont modifié le buffer.
    /// Cette case rend ce verdict lisible hors du fil de rendu au lieu de le
    /// perdre après le journal `windows_exclusive_signal_contract`.
    signal_path_status: Arc<std::sync::Mutex<Option<OutputSignalPathStatus>>>,
    /// Set by the playback thread when the audio device refuses to open, so
    /// the poller can stop the zone and tell the user on the very next tick
    /// instead of waiting out the stall heuristics. Cleared on every
    /// `play_url()` — a failure belongs to the track that provoked it, and
    /// must never travel to the next one.
    open_failure: Arc<std::sync::Mutex<Option<String>>>,
    /// Combien de fois le rappel audio a manqué de données depuis le début du
    /// flux, et combien d'échantillons sont partis en zéros (#3205).
    ///
    /// Le même `Arc` est confié à l'anneau de CHAQUE backend au moment où il
    /// est créé, quelle que soit la branche empruntée ; il survit donc aux
    /// replis (rate de repli, cascade entière) parce qu'il appartient à la
    /// sortie, pas au flux.
    starvation: Arc<RingStarvation>,
}

/// What the render callbacks multiply every sample by, in thousandths.
///
/// `dop` is not one more attenuation to fold in — it *replaces* the whole
/// computation with unity, and that is the point. A DoP stream is a DSD
/// bitstream wrapped in 24-bit PCM whose top byte carries the alternating
/// `0x05`/`0xFA` marker (`audio::dsd_to_dop`). Any factor other than exactly
/// 1.0 rewrites that byte, the DAC stops recognising DoP, and it **mutes** —
/// so a DSD track survived only at 100 % with ReplayGain off, and neither the
/// slider nor the ReplayGain tag said why (Tades, #1408 → #1735).
///
/// Unity is exact here rather than merely close: a 24-bit integer sample is
/// representable to the bit in an f32 mantissa, so skipping the multiply
/// returns the marker byte untouched.
///
/// The consequence is deliberate and must be surfaced in the UI: **on a DSD
/// track the volume slider does nothing.** Silently inert is a better failure
/// than silently mute, but it is still a failure until the interface says so.
fn effective_volume_units(user_units: u32, rg_units: u32, dop: bool) -> u32 {
    if dop {
        return 1000;
    }
    let user = user_units as f64 / 1000.0;
    let rg = rg_units as f64 / 1000.0;
    // Clamped to unity: above it, a ReplayGain boost would push peaks past
    // full scale and the user, who never touched the slider, would hear
    // distortion appear out of nowhere.
    ((user * rg).clamp(0.0, 1.0) * 1000.0).round() as u32
}

/// Reporte une bascule DoP sur le facteur que lisent les callbacks de rendu.
///
/// Appelée depuis les trois boucles d'alimentation, dont celle du bras ASIO,
/// qui vit sous `#[cfg(all(target_os = "windows", feature = "asio"))]` et n'est
/// donc **pas compilée ailleurs que sous Windows**. Garder le calcul ici plutôt
/// que répété dans les trois boucles le fait type-checker et tester sur toutes
/// les plateformes ; le bras ASIO n'en garde qu'un appel. Sans cela, une faute
/// de frappe à cet endroit ne se découvrirait qu'au build Windows de la
/// release — `asio` fait partie des features livrées (`release.yml`).
fn sync_volume_to_dop(
    volume: &AtomicU32,
    user_volume: &AtomicU32,
    rg_factor: &AtomicU32,
    dop: bool,
) {
    volume.store(
        effective_volume_units(
            user_volume.load(Ordering::SeqCst),
            rg_factor.load(Ordering::SeqCst),
            dop,
        ),
        Ordering::SeqCst,
    );
}

impl LocalOutput {
    pub fn new(device_name: String) -> Self {
        Self::with_options(device_name, false, "auto")
    }

    /// Recompute what the render callbacks multiply by: user volume ×
    /// ReplayGain factor.
    ///
    /// The product is clamped to unity. Going above it would push a track
    /// whose ReplayGain asks for a boost past full scale on peaks — and the
    /// user, who never touched the slider, would hear distortion appear out of
    /// nowhere. `gain_factor` already refuses to clip against the tagged peak;
    /// this is the second, unconditional guard for a track with no peak tag.
    /// Set the ReplayGain factor for the track about to play (1.0 = untouched).
    /// Inherent twin of the trait method so the orchestrator can call it on a
    /// downcast `LocalOutput` without importing `OutputTarget`.
    pub fn set_replaygain_factor(&self, factor: f64) {
        let f = (factor.clamp(0.0, 4.0) * 1000.0).round() as u32;
        self.rg_factor.store(f, Ordering::SeqCst);
        self.recompute_effective_volume();
    }

    fn recompute_effective_volume(&self) {
        let v = effective_volume_units(
            self.user_volume.load(Ordering::SeqCst),
            self.rg_factor.load(Ordering::SeqCst),
            self.dop_active.load(Ordering::Relaxed),
        );
        self.volume.store(v, Ordering::SeqCst);
    }

    /// Create a new `LocalOutput` with explicit exclusive-mode control.
    pub fn new_with_exclusive(device_name: String, exclusive_mode: bool) -> Self {
        Self::with_options(device_name, exclusive_mode, "auto")
    }

    /// Create a new `LocalOutput` with full control over exclusive mode and
    /// audio backend selection.
    pub fn with_options(device_name: String, exclusive_mode: bool, audio_backend: &str) -> Self {
        Self::with_options_and_endpoint(device_name, None, exclusive_mode, audio_backend)
    }

    /// Rattacher cette sortie à l'hôte audio qui a énuméré son nom.
    ///
    /// À appeler partout où le nom vient d'un [`AudioDevice`] : sans cette
    /// étiquette, un nom ne porte rien et la résolution ne peut pas refuser un
    /// hôte étranger (#3230). Une chaîne vide est traitée comme « inconnu ».
    ///
    /// # Elle RECTIFIE aussi le backend (#1770)
    ///
    /// Connaître l'hôte d'origine, c'est savoir sous quel hôte ce nom est
    /// ouvrable — et donc pouvoir refuser d'en ouvrir un autre. La règle est
    /// dans [`crate::config::openable_local_backend`], avec le détail de ce
    /// qu'elle répare.
    ///
    /// Elle est appliquée ICI, à la construction, et non chez les appelants :
    /// c'est le seul endroit où l'origine est connue, et le recensement des
    /// sites d'enregistrement est un PLANCHER, jamais un plafond. Un site
    /// ajouté demain qui étiquette correctement son origine est corrigé sans
    /// rien avoir à savoir de cette règle ; un site qui ne l'étiquette pas
    /// n'est pas corrigé — et c'est ce que garde
    /// `les_deux_sites_d_enregistrement_local_etiquettent_l_hote_d_origine`
    /// dans `tune-server/src/background.rs`.
    #[must_use]
    pub fn with_origin_host(mut self, origin_host: &str) -> Self {
        self.origin_host = (!origin_host.is_empty()).then(|| origin_host.to_string());
        self.audio_backend =
            crate::config::openable_local_backend(&self.audio_backend, self.origin_host.as_deref());
        self
    }

    /// Le backend sous lequel cette sortie sera OUVERTE.
    ///
    /// Ce n'est pas forcément le réglage `local_audio_backend` : quand l'hôte
    /// d'origine est connu, [`Self::with_origin_host`] l'a rectifié (#1770).
    /// C'est cette valeur-là que consomment `select_host`, la branche ASIO
    /// exclusive et [`crate::outputs::OutputTarget::is_available`].
    pub fn audio_backend(&self) -> &str {
        &self.audio_backend
    }

    /// L'hôte audio qui a énuméré le nom que porte cette sortie, s'il est
    /// connu (#3230).
    pub fn origin_host(&self) -> Option<&str> {
        self.origin_host.as_deref()
    }

    /// Create a local output bound to the stable backend endpoint discovered
    /// alongside its display name.
    pub fn with_options_and_endpoint(
        device_name: String,
        endpoint_id: Option<String>,
        exclusive_mode: bool,
        audio_backend: &str,
    ) -> Self {
        let device_id = format!("local:{device_name}");
        Self {
            device_name,
            device_id,
            endpoint_id,
            origin_host: None,
            playing: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            volume: Arc::new(AtomicU32::new(1000)),
            user_volume: Arc::new(AtomicU32::new(1000)),
            rg_factor: Arc::new(AtomicU32::new(1000)),
            pre_mute_volume: Arc::new(AtomicU32::new(1000)),
            muted: Arc::new(AtomicBool::new(false)),
            position_ms: Arc::new(AtomicU64::new(0)),
            seek_offset_ms: Arc::new(AtomicU64::new(0)),
            pending_start_position_ms: AtomicU64::new(0),
            stream_pre_seeked: AtomicBool::new(false),
            duration_ms: Arc::new(AtomicU64::new(0)),
            current_uri: Arc::new(std::sync::Mutex::new(None)),
            track_title: Arc::new(std::sync::Mutex::new(None)),
            track_artist: Arc::new(std::sync::Mutex::new(None)),
            stop_tx: std::sync::Mutex::new(None),
            play_thread: std::sync::Mutex::new(None),
            sentinelle_du_fil: std::sync::Mutex::new(None),
            exclusive_mode,
            audio_backend: audio_backend.to_string(),
            play_generation: Arc::new(AtomicU64::new(0)),
            force_silent: std::sync::Mutex::new(Arc::new(AtomicBool::new(false))),
            track_ended_naturally: Arc::new(AtomicBool::new(false)),
            track_ended_generation: Arc::new(AtomicU64::new(0)),
            next_media: Arc::new(std::sync::Mutex::new(None)),
            chain_exhausted: Arc::new(AtomicBool::new(false)),
            eq: Arc::new(std::sync::Mutex::new(None)),
            current_format: Arc::new(AtomicU32::new(0)),
            convolver_config: Arc::new(std::sync::Mutex::new(None)),
            convolver: Arc::new(std::sync::Mutex::new(None)),
            pure_bypass: Arc::new(AtomicBool::new(false)),
            mono_downmix: Arc::new(AtomicBool::new(false)),
            // Désarmée tant que l'orchestrateur n'a pas posé la valeur de la
            // zone : une sortie construite hors chemin de lecture se comporte
            // exactement comme avant #1590.
            soft_mute_ms: Arc::new(AtomicU32::new(0)),
            crossfeed: Arc::new(std::sync::Mutex::new(None)),
            dop_active: Arc::new(AtomicBool::new(false)),
            signal_path_status: Arc::new(std::sync::Mutex::new(None)),
            open_failure: Arc::new(std::sync::Mutex::new(None)),
            starvation: Arc::new(RingStarvation::new()),
        }
    }

    /// Install (or clear with `None`) the zone equalizer for the zone about to
    /// play on this output. Set per-play by the orchestrator, mirroring
    /// `set_crossfeed`: the orchestrator passes `None` when the zone has no
    /// enabled EQ profile, when the profile is inaudible, or when the zone is
    /// in PURE mode (`load_eq_processor` already returns `None` in all three
    /// cases, so PURE stays bit-perfect without a second guard here).
    ///
    /// Rebuilt at each play so the biquad coefficients match the resolved
    /// stream's sample rate and channel count, and so a profile edited between
    /// two tracks takes effect on the next one.
    pub fn set_eq(&self, eq: Option<super::super::audio::eq::EqProcessor>) {
        *self.eq.lock().unwrap() = eq;
    }

    /// Remplacer l'égaliseur **pendant** la lecture, en emportant l'historique
    /// des filtres pour que le changement s'entende sans claquer.
    ///
    /// La boucle de lecture relit ce mutex à chaque paquet : en remplacer le
    /// contenu suffit à changer le son en vol. Ce qu'il ne faut pas faire, en
    /// revanche, c'est jeter l'historique des biquads — un filtre dont l'état
    /// retombe brutalement à zéro produit une discontinuité, donc un clic, et
    /// un curseur qu'on fait glisser en produirait un par cran. Voir
    /// `EqProcessor::inherit_state_from`.
    ///
    /// Distinct de [`Self::set_eq`] à dessein : au début d'une piste il n'y a
    /// pas d'historique à conserver, et celui de la piste précédente serait
    /// faux.
    pub fn replace_eq_live(&self, eq: Option<super::super::audio::eq::EqProcessor>) {
        let mut emplacement = self.eq.lock().unwrap();
        match (eq, emplacement.as_ref()) {
            (Some(mut neuf), Some(precedent)) => {
                neuf.inherit_state_from(precedent);
                *emplacement = Some(neuf);
            }
            (suivant, _) => *emplacement = suivant,
        }
    }

    pub fn has_eq(&self) -> bool {
        self.eq.lock().unwrap().is_some()
    }

    /// Format du flux en cours, `(taux, canaux)`, ou `None` si rien ne joue.
    ///
    /// Sert à rebâtir un `EqProcessor` aux bons coefficients SANS attendre la
    /// piste suivante (#1725).
    pub fn current_format(&self) -> Option<(u32, u16)> {
        let empaquete = self.current_format.load(Ordering::Relaxed);
        if empaquete == 0 {
            return None;
        }
        let taux = empaquete >> 8;
        let canaux = (empaquete & 0xFF) as u16;
        if taux == 0 || canaux == 0 {
            return None;
        }
        Some((taux, canaux))
    }

    /// Déclarer un format « en cours » sans lecture réelle — **tests
    /// uniquement**.
    ///
    /// `current_format` n'est écrit que par les trois boucles d'alimentation,
    /// qui exigent un périphérique audio ouvert. Or les rafraîchisseurs à chaud
    /// (`refresh_zone_eq`, `refresh_zone_crossfeed`, `refresh_zone_pure_dsp`)
    /// s'arrêtent net sur un format inconnu : sans ce point d'entrée, leur
    /// corps utile n'est atteignable par aucun test sans matériel, et c'est
    /// précisément le corps qui décide si le son change.
    #[cfg(test)]
    pub(crate) fn declare_current_format_for_test(&self, taux: u32, canaux: u16) {
        self.current_format
            .store(Self::pack_format(taux, canaux), Ordering::Relaxed);
    }

    /// Force l'état de la boucle d'enchaînement, pour les tests : c'est la
    /// seule façon d'atteindre la sonde sans carte son. Même point d'entrée que
    /// `OaatOutput::set_chain_exhausted_for_test` (#1323).
    #[cfg(test)]
    pub(crate) fn set_chain_exhausted_for_test(&self, exhausted: bool) {
        self.chain_exhausted.store(exhausted, Ordering::SeqCst);
    }

    /// Lecture du drapeau PURE, pour les tests : c'est lui que `apply_local_dsp`
    /// consulte, donc lui qui dit si l'égaliseur installé travaille encore.
    #[cfg(test)]
    pub(crate) fn pure_bypass_for_test(&self) -> bool {
        self.pure_bypass.load(Ordering::Relaxed)
    }

    /// Facteur ReplayGain courant en millièmes, pour les tests. Il n'est PAS
    /// couvert par le drapeau PURE : c'est une multiplication faite dans les
    /// callbacks de rendu, hors de `apply_local_dsp`.
    #[cfg(test)]
    pub(crate) fn replaygain_units_for_test(&self) -> u32 {
        self.rg_factor.load(Ordering::SeqCst)
    }

    /// Empaquette `(taux, canaux)` pour [`Self::current_format`]. Un taux
    /// au-delà de 16,7 MHz déborderait les 24 bits — il n'en existe pas, mais
    /// on préfère annoncer « pas de flux » qu'un taux tronqué.
    pub(crate) fn pack_format(taux: u32, canaux: u16) -> u32 {
        if taux == 0 || taux > 0x00FF_FFFF || canaux == 0 || canaux > 255 {
            return 0;
        }
        (taux << 8) | (canaux as u32)
    }

    pub fn set_convolver_ir(&self, path: &str) -> Result<(), String> {
        let config = super::super::audio::convolver::ConvolverConfig::from_wav(path)?;
        *self.convolver_config.lock().unwrap() = Some(config.clone());
        let current_format = self.current_format();
        let active = match current_format {
            Some((sample_rate, channels)) => {
                match config.build_for(1024, sample_rate, channels as usize) {
                    Ok(convolver) => Some(convolver),
                    Err(error) => {
                        // La configuration reste mémorisée pour une prochaine
                        // piste compatible, mais l'ancien moteur ne doit jamais
                        // continuer à corriger le format courant.
                        *self.convolver.lock().unwrap() = None;
                        return Err(error);
                    }
                }
            }
            None => None,
        };
        *self.convolver.lock().unwrap() = active;
        tracing::info!(
            path,
            device = %self.device_name,
            ir_sample_rate = config.sample_rate(),
            ir_channels = config.source_channels(),
            active = current_format.is_some(),
            "convolver_ir_set"
        );
        Ok(())
    }

    pub fn clear_convolver(&self) {
        *self.convolver_config.lock().unwrap() = None;
        *self.convolver.lock().unwrap() = None;
        tracing::info!(device = %self.device_name, "convolver_cleared");
    }

    /// Enable/disable PURE (audiophile) bypass of the room-correction convolver
    /// for the zone currently playing on this output. Set per-play by the
    /// orchestrator so a bit-perfect (PURE) zone skips convolution while other
    /// zones on the same output keep it.
    pub fn set_pure_bypass(&self, bypass: bool) {
        self.pure_bypass.store(bypass, Ordering::Relaxed);
    }

    /// Armer (ou désarmer) le repli mono de la zone qui joue sur cette sortie
    /// (#2362). Posé par l'orchestrateur, exactement comme `set_pure_bypass`.
    ///
    /// Un simple `store` suffit et se fait aussi bien en début de piste qu'en
    /// pleine lecture : contrairement au crossfeed ou à l'égaliseur, le repli
    /// n'a AUCUN état à emporter — pas de ligne à retard, pas d'historique de
    /// biquad. Il n'y a donc pas de `replace_..._live` séparé, et la bascule
    /// ne peut pas claquer.
    pub fn set_mono_downmix(&self, mono: bool) {
        self.mono_downmix.store(mono, Ordering::Relaxed);
    }

    /// Le repli mono est-il armé sur cette sortie ?
    pub fn has_mono_downmix(&self) -> bool {
        self.mono_downmix.load(Ordering::Relaxed)
    }

    /// Régler la durée de la rampe anti-« ploc » de la zone qui joue sur cette
    /// sortie (#1590). `0` désarme et rétablit la coupure franche.
    ///
    /// Comme `set_mono_downmix`, un `store` suffit et se fait aussi bien en
    /// début de piste qu'en pleine lecture : la rampe n'a pas d'état à
    /// reconstruire, et [`crate::audio::soft_mute::SoftMuteRamp::arm`] ne
    /// recalcule son incrément que si la durée a changé.
    ///
    /// La valeur est bornée ici aussi, et pas seulement chez l'appelant : c'est
    /// la sortie qui doit garantir qu'un réglage aberrant ne rend pas la pause
    /// molle.
    pub fn set_soft_mute_ms(&self, ms: u32) {
        self.soft_mute_ms.store(
            ms.min(crate::audio::soft_mute::SOFT_MUTE_MAX_MS),
            Ordering::Relaxed,
        );
    }

    /// Durée de rampe **réellement applicable** en cet instant, gardes
    /// bit-perfect comprises. C'est ce que lisent les callbacks et `stop()`.
    fn armed_soft_mute_ms(&self) -> u32 {
        crate::audio::soft_mute::armed_ms(
            self.soft_mute_ms.load(Ordering::Relaxed),
            self.dop_active.load(Ordering::Relaxed),
            self.pure_bypass.load(Ordering::Relaxed),
            self.exclusive_mode,
        )
    }

    /// La porte que les callbacks de rendu relisent à chaque tampon.
    fn soft_mute_gate(&self) -> crate::audio::soft_mute::SoftMuteGate {
        crate::audio::soft_mute::SoftMuteGate::new(
            self.soft_mute_ms.clone(),
            self.dop_active.clone(),
            self.pure_bypass.clone(),
            self.exclusive_mode,
        )
    }

    /// Install (or clear with `None`) the headphone crossfeed processor for the
    /// zone about to play on this output. Set per-play by the orchestrator,
    /// mirroring `set_pure_bypass`: the orchestrator passes `None` when the zone
    /// has crossfeed disabled or is in PURE mode. Applied in the playback loop
    /// after the convolver, only for stereo streams.
    pub fn set_crossfeed(&self, cf: Option<super::super::audio::crossfeed::CrossfeedProcessor>) {
        *self.crossfeed.lock().unwrap() = cf;
    }

    /// Remplacer le crossfeed **pendant** la lecture, en emportant les lignes à
    /// retard pour que le changement s'entende sans claquer.
    ///
    /// Jumeau de [`Self::replace_eq_live`], et pour la même raison : la boucle
    /// de lecture relit ce mutex à chaque paquet, donc en remplacer le contenu
    /// suffit à changer le son en vol — mais une ligne à retard qui repart à
    /// zéro fait chuter le terme croisé au silence, ce qui s'entend comme un
    /// clic. Voir `CrossfeedProcessor::inherit_state_from`.
    ///
    /// Distinct de [`Self::set_crossfeed`] à dessein : au début d'une piste il
    /// n'y a pas d'historique à conserver, et celui de la piste précédente
    /// serait faux.
    pub fn replace_crossfeed_live(
        &self,
        cf: Option<super::super::audio::crossfeed::CrossfeedProcessor>,
    ) {
        let mut emplacement = self.crossfeed.lock().unwrap();
        match (cf, emplacement.as_ref()) {
            (Some(mut neuf), Some(precedent)) => {
                neuf.inherit_state_from(precedent);
                *emplacement = Some(neuf);
            }
            (suivant, _) => *emplacement = suivant,
        }
    }

    pub fn has_crossfeed(&self) -> bool {
        self.crossfeed.lock().unwrap().is_some()
    }

    pub fn has_convolver(&self) -> bool {
        self.convolver_config.lock().unwrap().is_some()
    }

    /// Le mode exclusif / bit-perfect est-il disponible sur CETTE cible ?
    ///
    /// Le verdict vient de [`exclusive_mode_support`], à qui la plateforme est
    /// **passée** : sans cela la décision Windows ne serait compilée que sous
    /// Windows et aucun test joué ailleurs ne pourrait la contredire — même
    /// raison que pour [`sample_rate_evidence`] (#2862), même angle mort que
    /// #1837 et #2056. Ce site est le seul à lire la valeur réelle de la
    /// machine.
    pub fn supports_exclusive_mode() -> bool {
        exclusive_mode_support(std::env::consts::OS, cfg!(feature = "asio")).any()
    }

    pub fn set_pending_start_position_ms(&self, position_ms: u64) {
        self.pending_start_position_ms
            .store(position_ms, Ordering::SeqCst);
    }

    /// Signal that the producer actually emitted a pre-seeked stream: the
    /// consumer must NOT byte-skip seek_offset_ms again (double seek, #1518).
    /// Since b3a4a79f BOTH transcode arms (local file and Qobuz/Tidal
    /// streaming) feed seek_s to the decoder, so the orchestrator always
    /// passes `true` here. `false` remains meaningful only for a producer
    /// that genuinely starts at 0s.
    pub fn set_producer_seeked(&self, seeked: bool) {
        self.stream_pre_seeked.store(seeked, Ordering::SeqCst);
    }

    /// Consumer-side view of the pre-seeked flag (regression test for #1518).
    pub fn producer_seeked(&self) -> bool {
        self.stream_pre_seeked.load(Ordering::SeqCst)
    }
}

/// Un fil de lecture qui sort de sa boucle d'enchaînement doit-il déclarer la
/// chaîne épuisée ?
///
/// Oui dans tous les cas — une boucle terminée ne peut plus rien enchaîner —
/// **sauf un** : celui où une lecture plus récente l'a supplanté. Là, le
/// drapeau appartient déjà au fil suivant, et le lever le priverait de son
/// gapless pour toute la durée de son morceau.
///
/// Deux façons de reconnaître ce cas, et il faut les deux :
///
/// - `supplante` (`force_silent`) — `stop()` a fait taire ce fil ;
/// - la **génération** a bougé — un `play_url()` est passé.
///
/// La génération seule ne suffit pas : `play_url()` remet la sonde à zéro
/// **après** avoir incrémenté la génération, précisément pour qu'aucun ancien
/// fil ne puisse relever le drapeau derrière lui. Un fil dont la génération est
/// encore la courante est bien le fil en titre, et son épuisement compte.
pub(crate) fn doit_declarer_chaine_epuisee(
    supplante: bool,
    generation_courante: u64,
    ma_generation: u64,
) -> bool {
    !supplante && generation_courante == ma_generation
}

/// Ring buffer shared between the HTTP reader thread and the audio callback.
///
/// Also used by `coreaudio_exclusive` on macOS for bit-perfect output.
pub struct RingBuf {
    /// Les cases vivent dans des `UnsafeCell` : c'est la SEULE façon légale de
    /// muter à travers un `&self`. Les atomiques ci-dessous ordonnent les
    /// curseurs, ils ne rendent pas la mutation licite — un `Box<[f32]>` écrit
    /// via `as_ptr() as *mut f32` est un comportement indéfini au sens du
    /// modèle mémoire de Rust, quelle que soit la rigueur des curseurs, et le
    /// compilateur est en droit d'optimiser en conséquence (#2204).
    buf: Box<[UnsafeCell<f32>]>,
    /// Write position (HTTP thread writes here)
    write: AtomicU64,
    /// Read position (audio callback reads here)
    read: AtomicU64,
    /// Compteur de famine (#3205), partagé avec la sortie qui possède cet
    /// anneau.
    ///
    /// Il est porté par l'ANNEAU et non par chaque rappel parce que l'anneau
    /// est le seul objet que TOUS les backends partagent : cpal partagé,
    /// repli entier, chemin compressé, CoreAudio exclusif, ASIO et WASAPI
    /// exclusif reçoivent tous ce même `Arc`. Compter dans le drain couvre
    /// donc les six d'un seul geste, sans toucher à la signature d'un seul
    /// backend, et rend impossible l'oubli d'un site futur.
    starvation: Arc<RingStarvation>,
}

/// Integer SPSC ring used by Windows exclusive backends when the source must
/// cross the callback boundary without touching floating point.
///
/// Every sample is left-aligned in an `i32`: 16-bit words occupy bits 31..16,
/// 24-bit words bits 31..8, and 32-bit words use the whole value. This is the
/// representation expected by an ASIO I32 callback and lets WASAPI recover
/// the original little-endian word by copying the high 2/3/4 bytes.
#[cfg(any(target_os = "windows", test))]
pub(crate) struct NativePcmRing {
    buf: Box<[UnsafeCell<i32>]>,
    write: AtomicU64,
    read: AtomicU64,
    /// Même compteur de famine que `RingBuf` (#3205) : les backends exclusifs
    /// Windows drainent cet anneau-ci.
    starvation: Arc<RingStarvation>,
}

// SAFETY: same strict SPSC contract and Acquire/Release cursor discipline as
// `RingBuf`; the only difference is the `i32` cell payload.
#[cfg(any(target_os = "windows", test))]
unsafe impl Send for NativePcmRing {}
#[cfg(any(target_os = "windows", test))]
unsafe impl Sync for NativePcmRing {}

#[cfg(any(target_os = "windows", test))]
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
impl NativePcmRing {
    pub(crate) fn new(capacity: usize) -> Self {
        Self::new_metered(capacity, Arc::new(RingStarvation::new()))
    }

    /// Jumeau de `RingBuf::new_metered` (#3205).
    pub(crate) fn new_metered(capacity: usize, starvation: Arc<RingStarvation>) -> Self {
        Self {
            buf: (0..capacity)
                .map(|_| UnsafeCell::new(0i32))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            write: AtomicU64::new(0),
            read: AtomicU64::new(0),
            starvation,
        }
    }

    pub(crate) fn capacity(&self) -> usize {
        self.buf.len()
    }

    pub(crate) fn available(&self) -> usize {
        let w = self.write.load(Ordering::Acquire);
        let r = self.read.load(Ordering::Acquire);
        w.wrapping_sub(r) as usize
    }

    pub(crate) fn clear(&self) {
        self.read.store(0, Ordering::SeqCst);
        self.write.store(0, Ordering::SeqCst);
        for cell in self.buf.iter() {
            // SAFETY: producer-only reset before a callback can consume this
            // freshly-created ring.
            unsafe { *cell.get() = 0 };
        }
    }

    pub(crate) fn push(&self, samples: &[i32]) -> usize {
        let cap = self.capacity();
        let w = self.write.load(Ordering::Acquire);
        let r = self.read.load(Ordering::Acquire);
        let free = cap - w.wrapping_sub(r) as usize;
        let n = samples.len().min(free);
        for (i, sample) in samples[..n].iter().enumerate() {
            let idx = (w as usize + i) % cap;
            // SAFETY: unique producer, and `i < free` selects a cell the
            // consumer has already released.
            unsafe { *self.buf[idx].get() = *sample };
        }
        self.write.store(w + n as u64, Ordering::Release);
        n
    }

    pub(crate) fn pop(&self, out: &mut [i32]) -> usize {
        self.pop_mapped(out, |sample| sample)
    }

    /// Drain directly into a backend-owned callback buffer while converting
    /// each native word in place.  Keeping the mapping inside the ring avoids
    /// the temporary `Vec` that ASIO used to allocate on every audio period.
    pub(crate) fn pop_mapped<T>(&self, out: &mut [T], mut map: impl FnMut(i32) -> T) -> usize {
        let w = self.write.load(Ordering::Acquire);
        let r = self.read.load(Ordering::Acquire);
        let n = out.len().min(w.wrapping_sub(r) as usize);
        let cap = self.capacity();
        for (i, target) in out[..n].iter_mut().enumerate() {
            let idx = (r as usize + i) % cap;
            // SAFETY: unique consumer, and the producer published this cell
            // before advancing `write` with Release.
            *target = map(unsafe { *self.buf[idx].get() });
        }
        self.read.store(r + n as u64, Ordering::Release);
        self.starvation.record(out.len(), n);
        n
    }

    /// Drain native left-aligned words straight into a WASAPI byte buffer.
    /// Returns the number of bytes written; any remaining device buffer is
    /// silence-filled by the caller. No scratch allocation occurs here.
    pub(crate) fn pop_pcm_bytes(&self, out: &mut [u8], bit_depth: u16) -> usize {
        let bytes_per_sample = usize::from(bit_depth / 8);
        if !matches!(bit_depth, 16 | 24 | 32) {
            return 0;
        }

        let w = self.write.load(Ordering::Acquire);
        let r = self.read.load(Ordering::Acquire);
        let available = w.wrapping_sub(r) as usize;
        let count = available.min(out.len() / bytes_per_sample);
        let cap = self.capacity();
        for i in 0..count {
            let idx = (r as usize + i) % cap;
            // SAFETY: same SPSC publication contract as `pop_mapped`.
            let native = unsafe { *self.buf[idx].get() }.to_le_bytes();
            let offset = i * bytes_per_sample;
            out[offset..offset + bytes_per_sample].copy_from_slice(&native[4 - bytes_per_sample..]);
        }
        self.read.store(r + count as u64, Ordering::Release);
        // Compté en ÉCHANTILLONS comme partout ailleurs, pas en octets : le
        // chiffre doit se comparer d'un backend à l'autre (#3205).
        self.starvation.record(out.len() / bytes_per_sample, count);
        count * bytes_per_sample
    }
}

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WasapiEndpoint {
    pub(crate) id: String,
    pub(crate) name: String,
}

/// Convert the frame count returned after
/// `AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED` to a 100 ns WASAPI duration, with the
/// rounding formula prescribed by Microsoft. Kept outside the COM layer so
/// the arithmetic contract remains testable on every CI platform (#2208).
#[cfg(any(target_os = "windows", test))]
pub(crate) fn wasapi_aligned_duration_100ns(frames: u32, sample_rate: u32) -> Result<i64, String> {
    if frames == 0 || sample_rate == 0 {
        return Err(format!(
            "Taille WASAPI alignée invalide : {frames} frames à {sample_rate} Hz"
        ));
    }
    let numerator = u64::from(frames) * 10_000_000 + u64::from(sample_rate) / 2;
    i64::try_from(numerator / u64::from(sample_rate))
        .map_err(|_| "Durée WASAPI alignée hors domaine i64".to_string())
}

#[cfg(any(target_os = "windows", test))]
pub(crate) const AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED_HRESULT: i32 = 0x88890019u32 as i32;

#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WasapiInitDecision {
    Ready,
    RetryWithAlignedBuffer,
    Fail(i32),
}

#[cfg(any(target_os = "windows", test))]
pub(crate) fn wasapi_init_decision(hr: i32) -> WasapiInitDecision {
    match hr {
        0 => WasapiInitDecision::Ready,
        AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED_HRESULT => WasapiInitDecision::RetryWithAlignedBuffer,
        other => WasapiInitDecision::Fail(other),
    }
}

/// Resolve the exact endpoint requested by a zone. Display names are
/// disambiguated with the same `(2)`, `(3)` convention as discovery, while a
/// stable endpoint ID bypasses name matching entirely.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn select_wasapi_endpoint(
    requested: &str,
    default_id: Option<&str>,
    candidates: &[WasapiEndpoint],
) -> Result<WasapiEndpoint, String> {
    if requested.eq_ignore_ascii_case("default") {
        let id =
            default_id.ok_or_else(|| "WASAPI ne signale aucun endpoint par défaut".to_string())?;
        return candidates
            .iter()
            .find(|candidate| candidate.id == id)
            .cloned()
            .ok_or_else(|| format!("L'endpoint WASAPI par défaut « {id} » n'est plus présent"));
    }

    let requested_id = requested
        .strip_prefix("WASAPI:")
        .or_else(|| requested.strip_prefix("wasapi:"))
        .unwrap_or(requested);
    if let Some(candidate) = candidates
        .iter()
        .find(|candidate| candidate.id == requested_id)
    {
        return Ok(candidate.clone());
    }

    let mut occurrences = std::collections::HashMap::<&str, usize>::new();
    for candidate in candidates {
        let occurrence = occurrences.entry(candidate.name.as_str()).or_default();
        *occurrence += 1;
        let display_name = if *occurrence == 1 {
            candidate.name.clone()
        } else {
            format!("{} ({})", candidate.name, *occurrence)
        };
        if display_name.eq_ignore_ascii_case(requested) {
            return Ok(candidate.clone());
        }
    }

    let available = candidates
        .iter()
        .map(|candidate| format!("{} [{}]", candidate.name, candidate.id))
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "Endpoint WASAPI demandé introuvable : « {requested} ». Disponibles : {available}"
    ))
}

// SAFETY: SPSC strict. Un seul producteur appelle `push`/`clear`, un seul
// consommateur appelle `pop`. `write` n'est écrit que par le producteur et
// `read` que par le consommateur ; le couple Acquire/Release fait que le
// consommateur ne lit une case qu'après l'écriture qui l'a remplie, et que le
// producteur ne réécrit une case qu'après la lecture qui l'a libérée. Aucune
// case n'est donc jamais lue et écrite en même temps.
//
// `UnsafeCell` n'est pas `Sync` : sans ces deux lignes, `Arc<RingBuf>` ne
// traverserait plus les frontières de threads. Elles remplacent une hypothèse
// tacite par une hypothèse écrite.
unsafe impl Send for RingBuf {}
unsafe impl Sync for RingBuf {}

impl RingBuf {
    pub fn new(capacity: usize) -> Self {
        Self::new_metered(capacity, Arc::new(RingStarvation::new()))
    }

    /// Le compteur partagé de cet anneau, pour le rappel d'ERREUR du backend.
    ///
    /// Le rappel d'erreur ne touche pas l'anneau — il n'a rien à y lire — mais
    /// il doit écrire dans le MÊME compteur, sans quoi la sous-alimentation du
    /// pilote et la famine de l'anneau se retrouveraient dans deux relevés que
    /// rien ne joint (#3205).
    pub fn starvation(&self) -> Arc<RingStarvation> {
        self.starvation.clone()
    }

    /// Anneau dont la famine est comptée dans un compteur PARTAGÉ avec la
    /// sortie, seul moyen pour `/api/v1/system/diagnostics` de lire ce que le
    /// rappel a vécu.
    pub fn new_metered(capacity: usize, starvation: Arc<RingStarvation>) -> Self {
        Self {
            buf: (0..capacity)
                .map(|_| UnsafeCell::new(0.0f32))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            write: AtomicU64::new(0),
            read: AtomicU64::new(0),
            starvation,
        }
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Number of samples available to read
    pub fn available(&self) -> usize {
        let w = self.write.load(Ordering::Acquire);
        let r = self.read.load(Ordering::Acquire);
        (w.wrapping_sub(r)) as usize
    }

    /// Reset the ring buffer: zero out the underlying storage and reset
    /// the read/write cursors.  Called on track change to ensure no stale
    /// PCM data from a previous track leaks into the new one.
    pub fn clear(&self) {
        // Reset cursors first so the reader sees an empty buffer
        self.read.store(0, Ordering::SeqCst);
        self.write.store(0, Ordering::SeqCst);
        // Zero-fill the underlying storage to eliminate stale samples.
        // Safety: single-threaded clear (called before the cpal callback
        // starts reading from a freshly created ring buffer).
        for cell in self.buf.iter() {
            // SAFETY: appelé par le producteur seul, curseurs déjà remis à
            // zéro — aucun lecteur ne peut viser une case non écrite.
            unsafe { *cell.get() = 0.0 };
        }
    }

    /// Push samples into the ring buffer. Returns number actually written.
    pub fn push(&self, samples: &[f32]) -> usize {
        let cap = self.capacity();
        let w = self.write.load(Ordering::Acquire);
        let r = self.read.load(Ordering::Acquire);
        let free = cap - (w.wrapping_sub(r)) as usize;
        let n = samples.len().min(free);
        for i in 0..n {
            let idx = (w as usize + i) % cap;
            // SAFETY: producteur unique, case libre (i < free), index borné.
            unsafe { *self.buf[idx].get() = samples[i] };
        }
        self.write.store(w + n as u64, Ordering::Release);
        n
    }

    /// Read samples from the ring buffer into `out`. Returns number actually read.
    pub fn pop(&self, out: &mut [f32]) -> usize {
        self.pop_mapped(out, |sample| sample)
    }

    /// Drain and transform directly into the device callback's native slice.
    /// This is deliberately generic and allocation-free so integer ASIO
    /// callbacks do not need a floating-point scratch `Vec` per period.
    pub(crate) fn pop_mapped<T>(&self, out: &mut [T], mut map: impl FnMut(f32) -> T) -> usize {
        let w = self.write.load(Ordering::Acquire);
        let r = self.read.load(Ordering::Acquire);
        let avail = (w.wrapping_sub(r)) as usize;
        let n = out.len().min(avail);
        let cap = self.capacity();
        for i in 0..n {
            let idx = (r as usize + i) % cap;
            // SAFETY: consommateur unique, case publiée par le Release de
            // `push` que le Acquire ci-dessus a observé.
            out[i] = map(unsafe { *self.buf[idx].get() });
        }
        self.read.store(r + n as u64, Ordering::Release);
        // #3205 : `n < out.len()` ICI, c'est exactement le `read < data.len()`
        // que les rappels comblent avec des zéros. Trois atomiques `Relaxed`,
        // rien d'autre — voir le contrat sur `RingStarvation`.
        self.starvation.record(out.len(), n);
        n
    }
}

#[cfg(test)]
mod ringbuf_tests;

/// Pourquoi le décodage d'un flux compressé n'a rien rendu (#3270).
///
/// `decode_compressed_stream` rendait `None` pour QUATRE causes distinctes, et
/// le fil de lecture n'en tirait qu'un `warn!` : la zone s'arrêtait, le
/// sondeur ne recevait rien, et l'écran restait muet. Le motif nommé est ce
/// qui permet à `record_compressed_decode_failure` de dire à l'utilisateur
/// laquelle des quatre s'est produite.
///
/// Même forme que [`WindowsExclusivePcmError`] : un événement de journal
/// stable pour la fouille, une phrase française pour l'écran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompressedDecodeFailure {
    /// Aucun démultiplexeur de symphonia n'a reconnu le conteneur.
    ContainerUnrecognised,
    /// Conteneur lisible, mais il ne porte aucune piste audio exploitable.
    NoAudioTrack,
    /// La piste audio existe ; son codec n'a pas de décodeur ici.
    CodecUnsupported,
    /// Le décodage a tourné et n'a produit aucun échantillon (flux tronqué).
    NoSamplesDecoded,
}

impl CompressedDecodeFailure {
    fn log_event(self) -> &'static str {
        match self {
            Self::ContainerUnrecognised => "local_audio_decode_container_unrecognised",
            Self::NoAudioTrack => "local_audio_decode_no_audio_track",
            Self::CodecUnsupported => "local_audio_decode_codec_unsupported",
            Self::NoSamplesDecoded => "local_audio_decode_no_samples",
        }
    }

    fn user_message(self, device: &str) -> String {
        let reason = match self {
            Self::ContainerUnrecognised => "aucun décodeur n'a reconnu le format de ce flux",
            Self::NoAudioTrack => "le flux ne contient aucune piste audio lisible",
            Self::CodecUnsupported => "le codec de cette piste n'est pas pris en charge",
            Self::NoSamplesDecoded => {
                "le décodage n'a produit aucun échantillon, le flux est tronqué ou vide"
            }
        };
        format!(
            "Sortie « {device} » : impossible de décoder la piste, {reason}. La lecture a été arrêtée avant l'ouverture du périphérique ; choisissez une autre version du fichier ou vérifiez qu'il n'est pas endommagé"
        )
    }
}

/// Decode a compressed audio stream (FLAC, MP3, AAC, etc.) into f32 samples using symphonia.
///
/// Rend `Err(motif)` plutôt que `None` : l'appelant doit pouvoir DIRE pourquoi
/// il s'arrête (#3270), et un `Option` ne portait rien à dire.
fn decode_compressed_stream(data: &[u8]) -> Result<(u16, u32, Vec<f32>), CompressedDecodeFailure> {
    use std::io::Cursor;
    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;

    let cursor = Cursor::new(data.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());
    let hint = Hint::new();

    let mut format: Box<dyn symphonia::core::formats::FormatReader> =
        symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|_| CompressedDecodeFailure::ContainerUnrecognised)?;

    let track = format
        .default_track(TrackType::Audio)
        .ok_or(CompressedDecodeFailure::NoAudioTrack)?;
    let audio_params = match &track.codec_params {
        Some(CodecParameters::Audio(params)) => params.clone(),
        _ => return Err(CompressedDecodeFailure::NoAudioTrack),
    };
    let track_id = track.id;
    let sample_rate = audio_params.sample_rate.unwrap_or(44100);
    let channels = audio_params
        .channels
        .as_ref()
        .map(|c| c.count() as u16)
        .unwrap_or(2);

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .map_err(|_| CompressedDecodeFailure::CodecUnsupported)?;

    let mut all_samples: Vec<f32> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(_) => break,
        };

        if packet.track_id != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue,
        };

        // Convert decoded audio to interleaved f32 samples
        let mut packet_samples: Vec<f32> = Vec::new();
        decoded.copy_to_vec_interleaved::<f32>(&mut packet_samples);
        all_samples.extend_from_slice(&packet_samples);
    }

    if all_samples.is_empty() {
        return Err(CompressedDecodeFailure::NoSamplesDecoded);
    }

    info!(
        channels,
        sample_rate,
        samples = all_samples.len(),
        "local_audio_decoded_compressed_stream"
    );

    Ok((channels, sample_rate, all_samples))
}

/// WAV format tag constants.
const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// Parse a WAV header and return (channels, sample_rate, bit_depth, data_offset).
///
/// Handles PCM (format tag 1), IEEE Float (3), and WAVE_FORMAT_EXTENSIBLE
/// (0xFFFE).  For EXTENSIBLE, the actual sub-format is checked and
/// `wValidBitsPerSample` is used instead of the container size.
///
/// The `bit_depth` returned is the *effective* bit depth for PCM
/// interpretation, et il est **toujours** l'un de `0`, `16`, `24`, `32` :
///   - PCM entier : la largeur du CONTENEUR (`nBlockAlign / nChannels`),
///     validée par [`pcm_container_bit_depth`] ; tout autre conteneur rend
///     `None` et part au décodeur symphonia ;
///   - IEEE Float 32-bit: returns 0 as a sentinel so `pcm_bytes_to_f32`
///     uses the float path.
///
/// Cet ensemble fermé est un contrat, pas une commodité : `bytes_per_sample`,
/// `frame_bytes` et toutes les conversions d'échantillons du fichier
/// n'énumèrent que ces valeurs, et leurs branches par défaut se contredisent
/// (bruit ici, silence là).
/// Whether a failed header read should be retried rather than treated as a hard
/// failure. When a gapless/next track's transcode session has just started, its
/// WAV header isn't emitted yet, so the first reads return `TimedOut`/
/// `WouldBlock`. The pre-#522 code `break`-ed on any error, abandoning the chain
/// and skipping track 2 in a gapless album (Alain #981). Retrying on these
/// transient kinds — while a real error (broken pipe, etc.) still fails fast —
/// is what aligns the gapless path with the direct `play_url` path.
fn header_read_should_retry(kind: std::io::ErrorKind) -> bool {
    matches!(
        kind,
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

/// Profondeur PCM entière que le reste du fichier sait réellement décoder,
/// déduite du CONTENEUR (`nBlockAlign / nChannels`) et non des bits annoncés.
///
/// Tout ce qui suit — `bytes_per_sample`, `frame_bytes`, l'alignement des
/// trames, [`pcm_bytes_to_f32`], [`pcm_bytes_to_native_i32`],
/// [`native_i32_to_pcm_bytes`], [`f32_to_native_i32`],
/// [`NativePcmRing::pop_pcm_bytes`] — n'énumère que 16, 24 et 32 bits (plus le
/// sentinelle 0 pour le flottant). Une profondeur en dehors de cet ensemble
/// n'est donc pas « moins précise » : elle est **incohérente**, et de deux
/// façons opposées selon le chemin.
///
/// - `pcm_bytes_to_f32` retombe sur la lecture 16 bits : elle consomme deux
///   octets par échantillon là où l'appelant en a compté `bit_depth / 8`.
///   Chaque trame est alors lue au mauvais décalage, et la sortie locale rend
///   du **bruit blanc avec la musique derrière** — exactement le symptôme
///   d'un désaccord de format sur une chaîne numérique.
/// - `pcm_bytes_to_native_i32` et `f32_to_native_i32` rendent un `Vec` vide :
///   le chemin exclusif Windows, lui, rend du **silence**.
///
/// Un conteneur nul (`nBlockAlign < nChannels`, en-tête corrompu) est le pire
/// des cas : il produit `0`, qui est précisément le sentinelle « IEEE float
/// 32 bits ». Du PCM entier serait alors réinterprété comme des flottants —
/// du bruit à pleine échelle vers un amplificateur.
///
/// On refuse donc l'en-tête plutôt que de le mal décoder. `None` renvoie le
/// flux au décodeur symphonia, ce que ce fichier fait déjà pour le flottant
/// 64 bits qu'il ne sait pas porter non plus.
fn pcm_container_bit_depth(block_align: u16, channels: u16) -> Option<u16> {
    if channels == 0 {
        return None;
    }
    match block_align / channels {
        2 => Some(16),
        3 => Some(24),
        4 => Some(32),
        _ => None,
    }
}

fn parse_wav_header(header: &[u8]) -> Option<(u16, u32, u16, usize)> {
    if header.len() < 44 {
        return None;
    }
    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return None;
    }

    // Walk chunks to find "fmt " and "data"
    let mut offset = 12;
    let mut channels = 2u16;
    let mut sample_rate = 44100u32;
    let mut bit_depth = 16u16;
    let mut data_offset = None;

    while offset + 8 <= header.len() {
        let chunk_id = &header[offset..offset + 4];
        let chunk_size = u32::from_le_bytes([
            header[offset + 4],
            header[offset + 5],
            header[offset + 6],
            header[offset + 7],
        ]) as usize;

        if chunk_id == b"fmt " && offset + 8 + chunk_size <= header.len() {
            let fmt = &header[offset + 8..];
            let format_tag = u16::from_le_bytes([fmt[0], fmt[1]]);
            channels = u16::from_le_bytes([fmt[2], fmt[3]]);
            sample_rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
            let block_align = u16::from_le_bytes([fmt[12], fmt[13]]);
            // `wBitsPerSample` n'est plus lu : c'est une ANNONCE, pas un pas
            // d'avancement. Seul `nBlockAlign` dit ce que le flux fait
            // réellement, et c'est lui que [`pcm_container_bit_depth`] valide.

            match format_tag {
                WAVE_FORMAT_PCM => {
                    // Use nBlockAlign to determine the actual byte width per
                    // sample, which may differ from wBitsPerSample / 8 in
                    // edge cases (e.g. 20-bit in 24-bit container).
                    //
                    // `.min(32)` mentait sur le pas d'avancement : un conteneur
                    // de 8 octets était annoncé 32 bits et lu à la moitié de sa
                    // largeur, et un conteneur nul produisait le sentinelle
                    // flottant. Voir [`pcm_container_bit_depth`].
                    bit_depth = pcm_container_bit_depth(block_align, channels)?;
                }
                WAVE_FORMAT_IEEE_FLOAT => {
                    // Signal to pcm_bytes_to_f32 that the data is already
                    // IEEE float.  We use 0 as a sentinel value.
                    if channels > 0 {
                        let container_bytes = block_align / channels;
                        // 32-bit float -> sentinel 0; 64-bit float -> unsupported
                        if container_bytes == 4 {
                            bit_depth = 0; // sentinel: IEEE float 32-bit
                        } else {
                            // 64-bit float — cannot handle, fall through to
                            // compressed decode path
                            return None;
                        }
                    } else {
                        return None;
                    }
                }
                WAVE_FORMAT_EXTENSIBLE => {
                    // EXTENSIBLE: wBitsPerSample is the container size.
                    // wValidBitsPerSample at fmt[18..19] is the actual depth.
                    // The sub-format GUID at fmt[24..40] tells us PCM vs Float.
                    if chunk_size >= 40 {
                        let valid_bits = u16::from_le_bytes([fmt[18], fmt[19]]);
                        // Sub-format GUID first two bytes indicate the format
                        // (same as format_tag for standard formats).
                        let sub_format = u16::from_le_bytes([fmt[24], fmt[25]]);
                        if sub_format == WAVE_FORMAT_IEEE_FLOAT {
                            if channels > 0 && block_align / channels == 4 {
                                bit_depth = 0; // sentinel: IEEE float 32-bit
                            } else {
                                return None; // 64-bit float unsupported
                            }
                        } else {
                            // PCM sub-format : c'est le CONTENEUR qui donne le
                            // pas d'avancement, pas la précision valide.
                            //
                            // `wBitsPerSample` est la taille du conteneur et
                            // `wValidBitsPerSample` la précision réellement
                            // portée — Microsoft distingue explicitement les
                            // deux (WAVEFORMATEXTENSIBLE). Rendre la précision
                            // valide faisait avancer la lecture de
                            // `bit_depth / 8` octets : pour 24 bits valides
                            // dans un conteneur de 32, trois octets là où le
                            // flux en fait quatre. L'alignement des trames
                            // était faux dès le premier échantillon (#2234).
                            //
                            // Lire au conteneur n'est pas qu'un rattrapage
                            // d'alignement, c'est aussi numériquement exact :
                            // les bits valides sont cadrés à gauche, donc un
                            // échantillon `v` sur 24 bits vaut `v << 8` dans
                            // son conteneur de 32, et `(v << 8) / 2^31` est
                            // rigoureusement `v / 2^23` — la même valeur
                            // normalisée qu'une lecture 24 bits alignée.
                            //
                            // `valid_bits` reste lu : il ne sert plus au pas,
                            // mais un conteneur plus étroit que la précision
                            // annoncée signale un en-tête incohérent, et on
                            // suit alors le conteneur, qui est ce que le flux
                            // fait réellement.
                            //
                            // Les bornes ouvertes `0..=2 => 16` et `_ => 32`
                            // rattrapaient un conteneur absurde en ANNONÇANT un
                            // pas que le flux ne fait pas : un conteneur d'un
                            // octet lu par pas de deux, un conteneur de huit lu
                            // par pas de quatre. L'alignement des trames est
                            // faux dès le premier échantillon, et la sortie
                            // locale rend du bruit. Un conteneur hors 2/3/4
                            // octets n'est pas rattrapable ici : on rend `None`
                            // et symphonia le décode.
                            let container_bytes = block_align / channels.max(1);
                            debug_assert!(
                                valid_bits <= container_bytes * 8,
                                "wValidBitsPerSample > conteneur : en-tête incohérent"
                            );
                            bit_depth = pcm_container_bit_depth(block_align, channels)?;
                        }
                    } else {
                        // Truncated EXTENSIBLE — fall back to container size.
                        // `wBitsPerSample` n'est ici qu'une annonce : elle peut
                        // valoir 20 ou 0, que rien en aval ne sait décoder.
                        // C'est `nBlockAlign` qui dit ce que le flux fait.
                        bit_depth = pcm_container_bit_depth(block_align, channels)?;
                    }
                }
                _ => {
                    // Unknown format tag — let compressed decode handle it
                    return None;
                }
            }
        } else if chunk_id == b"data" {
            data_offset = Some(offset + 8);
            break;
        }

        offset += 8 + chunk_size;
        // Chunks are word-aligned
        if !chunk_size.is_multiple_of(2) {
            offset += 1;
        }
    }

    data_offset.map(|d| (channels, sample_rate, bit_depth, d))
}

/// Frames that must carry a valid, alternating DoP marker before a buffer is
/// treated as DoP.
///
/// The marker is one byte out of three, so a single frame would match ordinary
/// PCM once every ~256 samples. Requiring 32 consecutive frames — with the
/// marker *alternating* and identical across channels — puts a false positive
/// past 1 in 2^250 while still fitting in the smallest chunk the feed loops
/// ever assemble.
const DOP_DETECT_FRAMES: usize = 32;

/// True when `bytes` — interleaved 24-bit little-endian PCM — actually carries
/// a **DoP** (DSD over PCM) payload.
///
/// DoP packs 16 DSD bits into the low two bytes of each 24-bit sample and
/// stamps the top byte with a marker that alternates `0x05` / `0xFA` from one
/// frame to the next, identically on every channel (see
/// `audio::dsd_to_dop::DsdToDoP::feed`). That marker is the *only* thing that
/// tells a DAC it is being handed DSD and not audio — and it is exactly what
/// any sample-domain processing destroys.
///
/// Sniffing the bytes is how DoP is meant to be recognised: the DAC at the far
/// end of the cable does precisely this, and it is why the detection lives here
/// rather than being threaded down from the resolver. Any path that produces
/// DoP is covered, now and later, with nothing to keep in sync.
///
/// Only ever true for 24-bit streams — DoP has no other carrier.
pub(crate) fn is_dop_pcm(bytes: &[u8], bit_depth: u16, channels: u16) -> bool {
    if bit_depth != 24 || channels == 0 {
        return false;
    }
    let ch = channels as usize;
    let frame_bytes = 3 * ch;
    if bytes.len() < frame_bytes * DOP_DETECT_FRAMES {
        return false;
    }
    let mut prev: Option<u8> = None;
    for f in 0..DOP_DETECT_FRAMES {
        let base = f * frame_bytes;
        // The marker is the top byte of the 24-bit little-endian sample, and it
        // is the SAME on every channel of a frame. A stereo PCM signal that
        // happened to hit 0x05 on the left would have to hit it on the right in
        // the same frame too.
        let marker = bytes[base + 2];
        if marker != 0x05 && marker != 0xFA {
            return false;
        }
        for c in 1..ch {
            if bytes[base + 3 * c + 2] != marker {
                return false;
            }
        }
        // Strict alternation. Which of the two values a buffer starts on
        // depends on where the chunk boundary fell, so only the alternation is
        // asserted, never the starting value.
        if prev.is_some_and(|p| p == marker) {
            return false;
        }
        prev = Some(marker);
    }
    true
}

/// [`is_dop_pcm`], appelé sur un bloc qui porte son propre format.
///
/// Le seul appel du fichier qui ne puisse plus intervertir la profondeur et le
/// nombre de canaux : les deux sortent du même [`AudioSpec`], dans cet ordre,
/// et ils n'y sont pas du même type. Les deux autres appels — les branches
/// exclusives Windows — prennent encore leurs `u16` nus ; c'est leur tour dans
/// une tranche suivante, et ils sont hors du chemin du puits.
///
/// Seuls les octets ALIGNÉS sont sondés : le marqueur DoP est l'octet haut d'un
/// mot 24 bits, et un reliquat de trame coupée le placerait au mauvais endroit.
fn bloc_est_porteur_dop(bloc: &BlocPcm<'_>) -> bool {
    let spec = bloc.spec();
    is_dop_pcm(
        bloc.octets_alignes(),
        spec.profondeur().bits_declares(),
        spec.canaux(),
    )
}

/// Classification stable du porteur PCM pendant une piste.
///
/// Un flux 24 bits reste en quarantaine jusqu'a ce que 32 trames permettent
/// de conclure. Une fois la decision prise, elle est conservee pour toute la
/// piste : re-sonder chaque chunk faisait repasser un vrai DoP en PCM des qu'un
/// tampon court arrivait, et remettait alors volume et DSP dans le trajet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalPcmKind {
    Awaiting24BitProbe,
    Pcm,
    Dop,
}

impl LocalPcmKind {
    fn for_bit_depth(bit_depth: u16) -> Self {
        if bit_depth == 24 {
            Self::Awaiting24BitProbe
        } else {
            Self::Pcm
        }
    }

    fn is_awaiting_probe(self) -> bool {
        self == Self::Awaiting24BitProbe
    }
}

struct ProcessedLocalPcm {
    samples: Vec<f32>,
    source_frames: u64,
    dop: bool,
}

/// Frontiere unique entre les octets PCM recus et les rings flottants locaux.
///
/// Le tampon initial lu avec l'en-tete WAV et les lectures suivantes doivent
/// passer ici sans exception. La fonction conserve les octets 24 bits tant
/// que la sonde DoP n'est pas concluante, synchronise le volume avant de
/// rendre le premier echantillon au caller, puis applique exactement la meme
/// chaine DSP a tous les chunks PCM. L'adaptation de canaux et le resampling
/// restent ensuite propres au backend.
struct LocalPcmProcessor<'a> {
    eq: &'a std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: &'a std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: &'a std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: &'a AtomicBool,
    mono_downmix: &'a AtomicBool,
    dop_active: &'a AtomicBool,
    volume: &'a AtomicU32,
    user_volume: &'a AtomicU32,
    rg_factor: &'a AtomicU32,
}

impl LocalPcmProcessor<'_> {
    /// `spec` remplace le triplet `(frame_bytes, bit_depth, channels)` que
    /// cette fonction recevait nu.
    ///
    /// Ce n'est pas un regroupement de confort : `frame_bytes` était le SEUL
    /// des trois à ne pas être une étiquette — il en était la conséquence, et
    /// rien n'obligeait l'appelant à le recalculer quand les deux autres
    /// changeaient. Il n'est plus passé du tout : [`AudioSpec`] le déduit.
    /// Quant à `bit_depth` et `channels`, tous deux `u16` et voisins, leur
    /// interversion compilait ; elle est maintenant une erreur de type.
    fn process_pcm_chunk(
        &self,
        staged: &mut Vec<u8>,
        spec: AudioSpec,
        kind: &mut LocalPcmKind,
    ) -> Option<ProcessedLocalPcm> {
        // `bloc` tient les octets ET leur format : le découpage en trames ne
        // peut plus être fait avec une autre largeur que celle de `spec`.
        let bloc = spec.bloc(staged);
        let aligned_len = bloc.octets_alignes().len();
        let source_frames = bloc.trames() as u64;
        if aligned_len == 0 {
            return None;
        }

        if kind.is_awaiting_probe() {
            // `.max(1)` a disparu avec le besoin : `AudioSpec` refuse zéro
            // canal à la construction, parce que ce serait un diviseur nul.
            let probe_bytes = DOP_DETECT_FRAMES * spec.canaux() as usize * 3;
            if aligned_len < probe_bytes {
                // Ne rien convertir ni publier : l'octet marqueur n'existe
                // plus comme tel une fois le mot 24 bits passe en f32.
                return None;
            }
            *kind = if bloc_est_porteur_dop(&bloc) {
                LocalPcmKind::Dop
            } else {
                LocalPcmKind::Pcm
            };
        }

        let dop = *kind == LocalPcmKind::Dop;
        if self.dop_active.swap(dop, Ordering::SeqCst) != dop {
            info!(dop, "local_audio_dop_stream_state_changed");
            sync_volume_to_dop(self.volume, self.user_volume, self.rg_factor, dop);
        }

        let mut samples =
            pcm_bytes_to_f32(bloc.octets_alignes(), spec.profondeur().bits_declares());
        apply_local_dsp(
            &mut samples,
            self.eq,
            self.convolver,
            self.crossfeed,
            self.pure_bypass,
            self.mono_downmix,
            spec.canaux(),
            dop,
        );
        staged.drain(..aligned_len);

        Some(ProcessedLocalPcm {
            samples,
            source_frames,
            dop,
        })
    }
}

fn report_incomplete_local_pcm_probe(kind: LocalPcmKind, pending_bytes: usize) {
    if kind.is_awaiting_probe() && pending_bytes > 0 {
        warn!(pending_bytes, "local_audio_24bit_dop_probe_incomplete");
    }
}

/// Why a Windows exclusive backend that still crosses an `f32` ring refused
/// a 24-bit stream.
///
/// The temporary refusal is intentional: WASAPI and ASIO reconstruct integer
/// words from `f32` in their render callbacks. That route is not byte-perfect
/// (#2205), so allowing a detected DoP carrier through it would knowingly hand
/// corrupted DSD to the DAC. Until those backends have a raw integer ring, the
/// only safe behaviour is to fail before a sample reaches the callback.
#[cfg(any(target_os = "windows", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsExclusivePcmError {
    DopUnsupported,
    DopCheckIncomplete,
}

#[cfg(any(target_os = "windows", test))]
impl WindowsExclusivePcmError {
    fn log_event(self) -> &'static str {
        match self {
            Self::DopUnsupported => "windows_exclusive_dop_rejected_before_float_transport",
            Self::DopCheckIncomplete => "windows_exclusive_24bit_rejected_incomplete_dop_probe",
        }
    }

    fn user_message(self, backend: &str, device: &str) -> String {
        let reason = match self {
            Self::DopUnsupported => "un flux DoP a été détecté",
            Self::DopCheckIncomplete => {
                "le flux 24 bits est trop court pour exclure la présence de DoP"
            }
        };
        format!(
            "Sortie « {device} » : {reason}. Le transport {backend} exclusif actuel passe par une conversion flottante et ne peut pas garantir les bits DSD ; la lecture a été refusée avant l'envoi au périphérique. Choisissez une sortie bit-perfect compatible"
        )
    }
}

#[cfg(any(target_os = "windows", test))]
fn record_windows_exclusive_pcm_refusal(
    error: WindowsExclusivePcmError,
    backend: &str,
    device: &str,
    failure_slot: &std::sync::Mutex<Option<String>>,
) {
    warn!(
        backend,
        device,
        refusal_event = error.log_event(),
        reason = ?error,
        "windows_exclusive_pcm_refused"
    );
    if let Ok(mut slot) = failure_slot.lock() {
        *slot = Some(error.user_message(backend, device));
    }
}

#[cfg(any(target_os = "windows", target_os = "macos", test))]
fn record_exclusive_open_failure(
    backend: &str,
    requested_device: &str,
    error: &str,
    failure_slot: &std::sync::Mutex<Option<String>>,
) {
    warn!(
        backend,
        requested_device, error, "exclusive_open_failed_without_fallback"
    );
    if let Ok(mut slot) = failure_slot.lock() {
        *slot = Some(format!(
            "Sortie « {requested_device} » : l'ouverture {backend} exclusive a échoué ({error}). Aucun repli vers un autre endpoint ou vers le mode partagé n'a été effectué"
        ));
    }
}

/// Pourquoi le chemin cpal PARTAGÉ n'a ouvert aucun périphérique.
///
/// `find_device_with_fallback` ne rend `None` que dans UN cas : le
/// périphérique réglé sur la zone est introuvable ET l'hôte n'expose aucune
/// sortie par défaut sur laquelle se rabattre — c'est
/// `audio_device_not_found_no_default_available`, la seule des quatre issues
/// de cette fonction qui n'ouvre rien. Dès qu'un repli existe on passe par
/// `audio_device_not_found_falling_back_to_default` et la lecture continue.
///
/// Les deux consommateurs de ce `None` — le flux WAV, donc la bibliothèque
/// locale, et le flux compressé décodé — s'arrêtaient sans rien dire, alors
/// que le MÊME refus sur les chemins EXCLUSIFS est nommé depuis toujours par
/// [`record_exclusive_open_failure`]. C'était une incohérence, pas un manque,
/// et elle portait sur le chemin le plus emprunté de tous.
///
/// Passe par `failure_slot`, c'est-à-dire par `take_output_failure()` : le
/// canal que le poller draine à chaque tick pour émettre `zone.playback_error`
/// avec `fatal: true`. Aucun second canal n'est ouvert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SharedDeviceResolution {
    /// Chemin WAV/PCM — celui de la bibliothèque locale.
    WavStreamNotFound,
    /// Chemin compressé, décodé par symphonia puis rendu en cpal partagé.
    CompressedStreamNotFound,
}

impl SharedDeviceResolution {
    /// Les deux évènements historiques sont CONSERVÉS tels quels : un journal
    /// déjà récolté sur le terrain continue de les trouver.
    fn log_event(self) -> &'static str {
        match self {
            Self::WavStreamNotFound => "audio_device_not_found_no_fallback",
            Self::CompressedStreamNotFound => "audio_device_not_found_compressed",
        }
    }

    fn user_message(self, device: &str) -> String {
        let flux = match self {
            Self::WavStreamNotFound => "le flux PCM",
            Self::CompressedStreamNotFound => "le flux décodé",
        };
        format!(
            "Sortie « {device} » : le périphérique est introuvable et le système n'expose aucune sortie par défaut sur laquelle se rabattre ; {flux} n'a été envoyé nulle part. Rebranchez le périphérique ou choisissez une autre sortie pour cette zone"
        )
    }
}

/// Une période du rappel entier local **partagé** (cpal shared), hors de la
/// fermeture pour être mesurable.
///
/// #2218 — ce rappel gardait un `Vec<f32>` capturé et le faisait grandir quand
/// la période s'allongeait : `scratch.resize(n, 0.0)` appelait l'allocateur
/// **dans la période audio**, sur le chemin qu'emprunte tout DAC refusant le
/// flottant. `pop_mapped` écrit la conversion directement dans le tampon fourni
/// par le backend : plus rien à dimensionner ni à faire grandir ici.
///
/// La rampe anti-« ploc » (#1590) garde son contrat mot pour mot : son facteur
/// multiplie l'échantillon **flottant**, avant la conversion, jamais le mot
/// entier du DAC — voir [`SoftMuteRamp::apply_mapped`].
///
/// [`SoftMuteRamp::apply_mapped`]: crate::audio::soft_mute::SoftMuteRamp::apply_mapped
#[allow(clippy::too_many_arguments)]
fn render_local_shared_integer_callback<T>(
    ring: &RingBuf,
    volume: &AtomicU32,
    paused: &AtomicBool,
    silent: &AtomicBool,
    data_started: &AtomicBool,
    ramp: &mut crate::audio::soft_mute::SoftMuteRamp,
    armed_ms: u32,
    min_buffer_samples: usize,
    zero: T,
    output: &mut [T],
) -> usize
where
    T: Copy,
    f32: symphonia::core::audio::conv::IntoSample<T>,
{
    use symphonia::core::audio::conv::IntoSample;
    // Rampe anti-« ploc » (#1590). Ce chemin sert les DAC qui refusent le
    // flottant : la rampe y est armée par la même porte, donc toujours
    // désarmée sur DoP, en PURE et en sortie exclusive.
    ramp.arm(armed_ms);
    let silence = paused.load(Ordering::Relaxed) || silent.load(Ordering::Relaxed);
    if ramp.begin(silence) == crate::audio::soft_mute::Rendering::Silent {
        output.fill(zero);
        return 0;
    }
    if !data_started.load(Ordering::Acquire) {
        if ring.available() < min_buffer_samples {
            output.fill(zero);
            return 0;
        }
        data_started.store(true, Ordering::Release);
    }
    let v = volume.load(Ordering::Relaxed) as f32 / 1000.0;
    let read = ramp.apply_mapped(v, |s| s.into_sample(), |map| ring.pop_mapped(output, map));
    output[read..].fill(zero);
    read
}

/// Repli entier, HORS de toute branche : les DEUX chemins de sortie locale
/// s'en servent.
///
/// Il vivait à l'intérieur du chemin PCM, où seul celui-ci pouvait l'appeler.
/// La branche compressée — celle qu'emprunte TOUT flux non-WAV, donc toute
/// piste servie par un serveur multimédia (`source=upnp`), une radio en FLAC,
/// un podcast, Bandcamp — n'avait aucun repli et abandonnait à la première
/// erreur (#3618, Belkadi Yacine, DENAFRIPS Terminator II :
/// « Sample format 'f32' is not supported by hardware in any endianness »).
/// Le remède était déjà écrit dans ce fichier ; il n'était pas branché.
///
/// Bit-perfect USB DACs (XMOS/Totaldac, Nagra, …) frequently reject
/// float and only accept integer PCM: cpal's f32 build_output_stream
/// then fails with "Sample format 'f32' is not supported by hardware".
/// This builds the same stream in an integer format instead, converting
/// the f32 ring-buffer samples on the fly (reuses symphonia's IntoSample,
/// as orchestrator.rs already does). Only used as a fallback after both
/// f32 attempts fail, so the f32 happy path is untouched (Pascal, XMOS
/// USB Audio 2.0 → Totaldac).
fn build_int_stream<T>(
    device: &cpal::Device,
    cfg: &cpal::StreamConfig,
    ring_cb: Arc<RingBuf>,
    vol_cb: Arc<AtomicU32>,
    paused_cb: Arc<AtomicBool>,
    silent_cb: Arc<AtomicBool>,
    ds_cb: Arc<AtomicBool>,
    min_buf: usize,
    device_gone: Arc<AtomicBool>,
    soft_mute_cb: crate::audio::soft_mute::SoftMuteGate,
) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample + Send + 'static,
    f32: symphonia::core::audio::conv::IntoSample<T>,
{
    use symphonia::core::audio::conv::IntoSample;
    let zero: T = 0.0f32.into_sample();
    let mut ramp_cb = soft_mute_cb.ramp(cfg.sample_rate, cfg.channels);
    // Prélevé AVANT la fermeture de rendu, qui consomme `ring_cb` (#3205).
    let famine_cb = ring_cb.starvation();
    device.build_output_stream(
        cfg,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            render_local_shared_integer_callback(
                &ring_cb,
                &vol_cb,
                &paused_cb,
                &silent_cb,
                &ds_cb,
                &mut ramp_cb,
                soft_mute_cb.armed_ms(),
                min_buf,
                zero,
                data,
            );
        },
        make_stream_error_cb(device_gone, famine_cb),
        None,
    )
}

/// Le pendant `f32` de [`build_int_stream`] pour le chemin « flux compressé ».
///
/// Extrait tel quel de la branche compressée, sans changer une ligne du rappel
/// de rendu : le chemin heureux — celui de l'immense majorité des DAC — reste
/// exactement ce qu'il était. Ce qui change est qu'il devient UNE tentative
/// parmi d'autres au lieu d'être la seule (#3618).
#[allow(clippy::too_many_arguments)]
fn build_compressed_f32_stream(
    device: &cpal::Device,
    cfg: &cpal::StreamConfig,
    ring_cb: Arc<RingBuf>,
    vol_cb: Arc<AtomicU32>,
    paused_cb: Arc<AtomicBool>,
    silent_cb: Arc<AtomicBool>,
    ds_cb: Arc<AtomicBool>,
    min_buf: usize,
    device_gone: Arc<AtomicBool>,
    soft_mute_cb: crate::audio::soft_mute::SoftMuteGate,
) -> Result<cpal::Stream, cpal::BuildStreamError> {
    let mut ramp_cb = soft_mute_cb.ramp(cfg.sample_rate, cfg.channels);
    // Prélevé AVANT la fermeture de rendu, qui consomme `ring_cb` (#3205).
    let famine_cb = ring_cb.starvation();
    device.build_output_stream(
        cfg,
        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            // Rampe anti-« ploc » (#1590) : au lieu de sauter de l'amplitude
            // courante à zéro, le gain glisse sur quelques dizaines de
            // millisecondes. `arm(0)` — DoP, PURE, sortie exclusive — rend
            // exactement la coupure franche d'avant.
            ramp_cb.arm(soft_mute_cb.armed_ms());
            let silence = paused_cb.load(Ordering::Relaxed) || silent_cb.load(Ordering::Relaxed);
            if ramp_cb.begin(silence) == crate::audio::soft_mute::Rendering::Silent {
                data.fill(0.0);
                return;
            }
            // Wait for a minimum amount of data before starting to read from
            // the ring buffer. This prevents the audio device from playing
            // stale/garbage samples during track transitions.
            if !ds_cb.load(Ordering::Acquire) {
                if ring_cb.available() < min_buf {
                    data.fill(0.0);
                    return;
                }
                ds_cb.store(true, Ordering::Release);
            }
            let read = ring_cb.pop(data);
            let v = vol_cb.load(Ordering::Relaxed) as f32 / 1000.0;
            ramp_cb.apply(&mut data[..read], v);
            if read < data.len() {
                data[read..].fill(0.0);
            }
        },
        make_stream_error_cb(device_gone, famine_cb),
        None,
    )
}

/// Le format d'échantillon d'une tentative d'ouverture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormatDeSortie {
    F32,
    I32,
    I16,
}

impl FormatDeSortie {
    pub(crate) fn nom(self) -> &'static str {
        match self {
            FormatDeSortie::F32 => "f32",
            FormatDeSortie::I32 => "i32",
            FormatDeSortie::I16 => "i16",
        }
    }
}

/// L'ordre dans lequel on tente d'ouvrir une sortie locale.
///
/// **C'est la règle du chemin PCM, extraite pour que la branche compressée
/// puisse enfin l'emprunter** (#3618). Elle était écrite en dur dans un bloc de
/// `play_url`, donc inatteignable depuis l'autre branche — et une piste servie
/// par un serveur multimédia (`source=upnp`) arrive TOUJOURS par l'autre
/// branche, parce que `orchestrator/commun.rs` l'envoie sur `resolve_direct`,
/// qui rend l'URL inchangée : pas d'en-tête WAV, donc `parse_wav_header` rend
/// `None`.
///
/// L'ordre est : `f32` d'abord, aux deux cadences — le chemin heureux reste
/// intact et n'essaie rien de nouveau — puis la cascade entière `i32`/`i16`,
/// cadence choisie puis cadence source. Les DAC USB bit-perfect
/// (XMOS/Totaldac, Nagra, DENAFRIPS) refusent fréquemment le flottant :
/// « Sample format 'f32' is not supported by hardware in any endianness ».
///
/// La cadence source n'est ajoutée que si elle diffère : inutile de tenter
/// deux fois exactement la même ouverture.
pub(crate) fn cascade_de_formats(
    principal: &cpal::StreamConfig,
    source: &cpal::StreamConfig,
) -> Vec<(cpal::StreamConfig, FormatDeSortie)> {
    let mut cadences = vec![principal.clone()];
    if source.sample_rate != principal.sample_rate || source.channels != principal.channels {
        cadences.push(source.clone());
    }
    let mut tentatives = Vec::with_capacity(cadences.len() * 3);
    for c in &cadences {
        tentatives.push((c.clone(), FormatDeSortie::F32));
    }
    for c in &cadences {
        for f in [FormatDeSortie::I32, FormatDeSortie::I16] {
            tentatives.push((c.clone(), f));
        }
    }
    tentatives
}

/// Tente les ouvertures dans l'ordre et rend la PREMIÈRE acceptée.
///
/// Générique sur ce qu'ouvre `ouvrir` : la production y passe une fermeture qui
/// appelle `build_compressed_f32_stream` / `build_int_stream::<i32>` /
/// `::<i16>`, les épreuves y passent un périphérique factice qui note ce qu'on
/// lui demande. Toutes les erreurs sont conservées : sans la première, on ne
/// peut pas classer la panne ni la nommer à l'écran.
pub(crate) fn ouvrir_premier_format_accepte<S, E, F>(
    tentatives: &[(cpal::StreamConfig, FormatDeSortie)],
    mut ouvrir: F,
) -> Result<(S, cpal::StreamConfig, FormatDeSortie), Vec<E>>
where
    F: FnMut(&cpal::StreamConfig, FormatDeSortie) -> Result<S, E>,
{
    let mut echecs = Vec::new();
    for (cfg, format) in tentatives {
        match ouvrir(cfg, *format) {
            Ok(s) => return Ok((s, cfg.clone(), *format)),
            Err(e) => echecs.push(e),
        }
    }
    Err(echecs)
}

fn record_shared_device_not_found(
    error: SharedDeviceResolution,
    requested_device: &str,
    failure_slot: &std::sync::Mutex<Option<String>>,
) {
    warn!(
        requested = %requested_device,
        refusal_event = error.log_event(),
        "shared_device_not_found_without_fallback"
    );
    if let Ok(mut slot) = failure_slot.lock() {
        *slot = Some(error.user_message(requested_device));
    }
}

/// Le périphérique s'est OUVERT puis a cessé de tirer l'audio : dire lequel,
/// et où la lecture s'est arrêtée.
///
/// Distinct de [`record_exclusive_open_failure`] parce que la cause l'est :
/// là-bas rien n'a jamais été envoyé, ici le rappel de rendu a accepté
/// l'ouverture puis s'est tu. Vu de l'utilisateur les deux se ressemblent —
/// « ça ne joue pas » — mais le geste diffère (rebrancher/rallumer contre
/// choisir une autre sortie), et c'est ce que dit le message.
///
/// `frozen_position_ms` n'est pas décoratif : c'est la position à laquelle
/// l'écran est resté figé, donc le seul chiffre qui relie ce que le testeur
/// voit à ce que le journal dit. Sur un anneau exclusif dimensionné à deux
/// secondes d'audio, il vaut 2000 — le « figée à 2 s » du constat.
///
/// Passe par `failure_slot`, c'est-à-dire par `take_output_failure()` : le
/// canal que le poller draine déjà à chaque tick pour émettre
/// `zone.playback_error` avec `fatal: true`. Aucun second canal n'est ouvert.
fn record_feed_stall_failure(
    backend: &str,
    device: &str,
    frozen_position_ms: u64,
    failure_slot: &std::sync::Mutex<Option<String>>,
) {
    warn!(
        backend,
        device,
        frozen_position_ms,
        stall_timeout_secs = FEED_STALL_TIMEOUT.as_secs(),
        "output_feed_stall_consumer_dead"
    );
    if let Ok(mut slot) = failure_slot.lock() {
        *slot = Some(format!(
            "Sortie « {device} » : le périphérique a accepté l'ouverture {backend} puis a cessé de recevoir l'audio ; la lecture est restée figée à {frozen_position_ms} ms. {}",
            OpenFailure::DeviceGone.user_message()
        ));
    }
}

/// Le DÉCODAGE a échoué : la zone ne jouera pas, et c'est le seul endroit qui
/// sait pourquoi (#3270).
///
/// Quatrième membre de la famille `record_*` de ce fichier, et pour la même
/// raison que les trois autres : `failure_slot` est le canal que
/// `take_output_failure()` draine à chaque tick du sondeur, qui émet alors
/// `zone.playback_error` avec `fatal: true`. Sans cet appel il ne restait
/// qu'un `warn!` dans le journal du serveur — invisible depuis l'écran.
///
/// Contrairement aux trois autres, la panne n'est PAS celle du périphérique :
/// il n'a jamais été ouvert. La sortie est nommée quand même, parce que c'est
/// par elle que l'utilisateur désigne la zone qui s'est tue.
fn record_compressed_decode_failure(
    error: CompressedDecodeFailure,
    device: &str,
    failure_slot: &std::sync::Mutex<Option<String>>,
) {
    warn!(
        device,
        refusal_event = error.log_event(),
        reason = ?error,
        "local_audio_decode_compressed_failed"
    );
    if let Ok(mut slot) = failure_slot.lock() {
        *slot = Some(error.user_message(device));
    }
}

/// Last preparation step before the f32 ring used by Windows exclusive
/// backends.
///
/// `must_classify_24_bit` is true until the first complete 32-frame probe has
/// ruled out DoP. Returning `Ok(None)` quarantines those initial bytes: the
/// caller must keep them in its raw-byte `leftover` buffer and must not feed
/// the ring. Every later, sufficiently large 24-bit chunk is checked too, so a
/// malformed stream cannot switch to DoP unnoticed at a chunk boundary.
#[cfg(any(target_os = "windows", test))]
#[allow(clippy::too_many_arguments)]
fn prepare_windows_exclusive_pcm(
    bytes: &[u8],
    bit_depth: u16,
    channels: u16,
    must_classify_24_bit: bool,
    eq: &std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: &std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: &std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: &AtomicBool,
    mono_downmix: &AtomicBool,
) -> Result<Option<Vec<f32>>, WindowsExclusivePcmError> {
    let probe_bytes = DOP_DETECT_FRAMES * channels.max(1) as usize * 3;
    if bit_depth == 24 && must_classify_24_bit && bytes.len() < probe_bytes {
        return Ok(None);
    }
    if bit_depth == 24 && is_dop_pcm(bytes, bit_depth, channels) {
        return Err(WindowsExclusivePcmError::DopUnsupported);
    }

    let mut samples = pcm_bytes_to_f32(bytes, bit_depth);
    apply_local_dsp(
        &mut samples,
        eq,
        convolver,
        crossfeed,
        pure_bypass,
        mono_downmix,
        channels,
        false,
    );
    Ok(Some(samples))
}

/// At EOF, an initial 24-bit probe that never reached 32 frames is not proof
/// of PCM. Failing closed avoids treating a tiny DoP payload as ordinary audio.
#[cfg(any(target_os = "windows", test))]
fn finish_windows_exclusive_probe(
    bit_depth: u16,
    must_classify_24_bit: bool,
    pending_bytes: usize,
) -> Result<(), WindowsExclusivePcmError> {
    if bit_depth == 24 && must_classify_24_bit && pending_bytes > 0 {
        Err(WindowsExclusivePcmError::DopCheckIncomplete)
    } else {
        Ok(())
    }
}

/// Consume every complete frame currently staged in `leftover`, but only
/// after the shared DoP/DSP preparation step has authorised it.
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn feed_windows_exclusive_leftover(
    leftover: &mut Vec<u8>,
    frame_bytes: usize,
    bit_depth: u16,
    channels: u16,
    must_classify_24_bit: &mut bool,
    eq: &std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: &std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: &std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: &AtomicBool,
    mono_downmix: &AtomicBool,
    ring: &RingBuf,
    stop_rx: &std::sync::mpsc::Receiver<()>,
    paused: &AtomicBool,
    force_silent: &AtomicBool,
) -> Result<u64, WindowsExclusivePcmError> {
    let aligned_len = (leftover.len() / frame_bytes) * frame_bytes;
    if aligned_len == 0 {
        return Ok(0);
    }
    let Some(samples) = prepare_windows_exclusive_pcm(
        &leftover[..aligned_len],
        bit_depth,
        channels,
        *must_classify_24_bit,
        eq,
        convolver,
        crossfeed,
        pure_bypass,
        mono_downmix,
    )?
    else {
        // The raw bytes remain staged until the first 24-bit probe reaches a
        // conclusive length. In particular, no f32 sample has been produced.
        return Ok(0);
    };

    *must_classify_24_bit = false;
    feed_ring_abortable(ring, &samples, stop_rx, paused, Some(force_silent));
    leftover.drain(..aligned_len);
    Ok((aligned_len / frame_bytes) as u64)
}

#[cfg(target_os = "windows")]
struct NativeFeedOutcome {
    frames: u64,
    dop: bool,
    bit_perfect: bool,
}

#[cfg(target_os = "windows")]
#[derive(Clone, Copy)]
enum WindowsExclusiveRingRef<'a> {
    Float(&'a RingBuf),
    Native(&'a NativePcmRing),
}

#[cfg(target_os = "windows")]
impl WindowsExclusiveRingRef<'_> {
    fn capacity(self) -> usize {
        match self {
            Self::Float(ring) => ring.capacity(),
            Self::Native(ring) => ring.capacity(),
        }
    }

    fn available(self) -> usize {
        match self {
            Self::Float(ring) => ring.available(),
            Self::Native(ring) => ring.available(),
        }
    }
}

/// Integer twin of [`feed_windows_exclusive_leftover`]. The producer resolves
/// DoP, DSP and volume before it publishes left-aligned words; the backend
/// callback can therefore remain a pure native serializer.
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn feed_windows_native_exclusive_leftover(
    leftover: &mut Vec<u8>,
    frame_bytes: usize,
    bit_depth: u16,
    channels: u16,
    must_classify_24_bit: &mut bool,
    dop_latched: &mut bool,
    volume_units: u32,
    eq: &std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: &std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: &std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: &AtomicBool,
    mono_downmix: &AtomicBool,
    ring: &NativePcmRing,
    stop_rx: &std::sync::mpsc::Receiver<()>,
    paused: &AtomicBool,
    force_silent: &AtomicBool,
) -> Option<NativeFeedOutcome> {
    let aligned_len = (leftover.len() / frame_bytes) * frame_bytes;
    if aligned_len == 0 {
        return None;
    }
    let prepared = prepare_windows_native_pcm(
        &leftover[..aligned_len],
        bit_depth,
        channels,
        *must_classify_24_bit,
        *dop_latched,
        volume_units,
        eq,
        convolver,
        crossfeed,
        pure_bypass,
        mono_downmix,
    )?;

    *must_classify_24_bit = false;
    *dop_latched = prepared.dop;
    feed_native_ring_abortable(ring, &prepared.samples, stop_rx, paused, Some(force_silent));
    leftover.drain(..aligned_len);
    Some(NativeFeedOutcome {
        frames: (aligned_len / frame_bytes) as u64,
        dop: prepared.dop,
        bit_perfect: prepared.bit_perfect,
    })
}

/// Route staged bytes to the callback representation selected from the
/// driver's advertised native format. The legacy float route remains
/// fail-closed for DoP; the native route carries DoP and identity PCM exactly.
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn feed_selected_windows_exclusive_leftover(
    leftover: &mut Vec<u8>,
    frame_bytes: usize,
    bit_depth: u16,
    channels: u16,
    must_classify_24_bit: &mut bool,
    dop_latched: &mut bool,
    volume_units: u32,
    eq: &std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: &std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: &std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: &AtomicBool,
    mono_downmix: &AtomicBool,
    ring: WindowsExclusiveRingRef<'_>,
    stop_rx: &std::sync::mpsc::Receiver<()>,
    paused: &AtomicBool,
    force_silent: &AtomicBool,
) -> Result<Option<NativeFeedOutcome>, WindowsExclusivePcmError> {
    match ring {
        WindowsExclusiveRingRef::Native(ring) => Ok(feed_windows_native_exclusive_leftover(
            leftover,
            frame_bytes,
            bit_depth,
            channels,
            must_classify_24_bit,
            dop_latched,
            volume_units,
            eq,
            convolver,
            crossfeed,
            pure_bypass,
            mono_downmix,
            ring,
            stop_rx,
            paused,
            force_silent,
        )),
        WindowsExclusiveRingRef::Float(ring) => {
            let frames = feed_windows_exclusive_leftover(
                leftover,
                frame_bytes,
                bit_depth,
                channels,
                must_classify_24_bit,
                eq,
                convolver,
                crossfeed,
                pure_bypass,
                mono_downmix,
                ring,
                stop_rx,
                paused,
                force_silent,
            )?;
            Ok((frames > 0).then_some(NativeFeedOutcome {
                frames,
                dop: false,
                bit_perfect: false,
            }))
        }
    }
}

#[cfg(target_os = "windows")]
#[allow(clippy::too_many_arguments)]
fn feed_selected_windows_exclusive_tail(
    ring: WindowsExclusiveRingRef<'_>,
    mut samples: Vec<f32>,
    bit_depth: u16,
    volume_units: u32,
    stop_rx: &std::sync::mpsc::Receiver<()>,
    paused: &AtomicBool,
    force_silent: &AtomicBool,
) {
    match ring {
        WindowsExclusiveRingRef::Float(ring) => {
            feed_ring_abortable(ring, &samples, stop_rx, paused, Some(force_silent));
        }
        WindowsExclusiveRingRef::Native(ring) => {
            let volume = volume_units as f32 / 1000.0;
            if volume != 1.0 {
                for sample in &mut samples {
                    *sample *= volume;
                }
            }
            let native = f32_to_native_i32(&samples, bit_depth);
            feed_native_ring_abortable(ring, &native, stop_rx, paused, Some(force_silent));
        }
    }
}

/// Reconstruire l'instance FFT à partir de la configuration persistante pour
/// le format SOURCE que le DSP va réellement recevoir.
///
/// En cas d'incompatibilité, l'ancienne instance est retirée : continuer avec
/// un moteur bâti pour une autre cadence ou un autre nombre de canaux serait
/// une correction acoustique fausse. Le flux audio, lui, peut continuer sans
/// convolveur et le journal donne l'action à effectuer (#2210).
fn rebuild_local_convolver(
    config: &std::sync::Mutex<Option<crate::audio::convolver::ConvolverConfig>>,
    active: &std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    sample_rate: u32,
    channels: u16,
) -> Result<bool, String> {
    let config = config
        .lock()
        .map_err(|_| "verrou de configuration du convolveur empoisonné".to_string())?
        .clone();
    let next = match config {
        Some(config) => match config.build_for(1024, sample_rate, channels as usize) {
            Ok(convolver) => Some(convolver),
            Err(error) => {
                if let Ok(mut current) = active.lock() {
                    *current = None;
                }
                return Err(error);
            }
        },
        None => None,
    };
    let enabled = next.is_some();
    *active
        .lock()
        .map_err(|_| "verrou du convolveur actif empoisonné".to_string())? = next;
    Ok(enabled)
}

/// Apply the local-output built-in DSP chain to an interleaved f32 buffer,
/// in place, at the three playback-loop feed sites.
///
/// Order matches the signal flow: zone **equalizer** first, then the
/// room-correction **convolver**, then the headphone **crossfeed**. All three
/// are skipped when `pure_bypass` is set (PURE / audiophile zone →
/// bit-perfect). Crossfeed additionally requires a stereo stream
/// (`channels == 2`); on non-stereo it is left untouched. Uses the same
/// try-lock pattern as the convolver so a contended lock never blocks audio.
///
/// They are skipped just as hard when `dop` is set: the buffer is then a DSD
/// bitstream wearing PCM's clothes, and filtering it would strip the marker
/// that makes the DAC play it at all — the user hears nothing (Tades, forum
/// #1408 : « pas de son quand j'active égaliseur ou crossfeed », hors mode
/// PURE). PURE zones never hit this because they bypass everything anyway;
/// the silence was reserved for people who had asked for processing.
///
/// EQ-before-convolver is the order the transcoded path already uses
/// (`transcode_source_to_file`), so a zone hears the same chain whether it
/// plays on the DAC or through a network renderer.
#[inline]
/// Frontière de piste : le DSP à état ne doit rien porter d'une piste à l'autre.
///
/// Le convolveur est installé une fois (`set_convolver_ir`) et vit aussi
/// longtemps que la sortie. Sa file de sortie, sa ligne à retard et son overlap
/// gardaient donc la queue de la piste précédente, qui repartait dans la
/// suivante — et ni un seek ni un arrêt n'établissaient de frontière
/// (JP Robbe, revue de #2268).
///
/// Appelé depuis `play_url`, le seul point par lequel passe un DÉBUT de piste.
/// Une transition gapless ne passe pas par là, et c'est voulu : l'audio y est
/// continu, le convolveur doit garder son état.
fn reset_local_dsp(convolver: &std::sync::Mutex<Option<crate::audio::convolver::Convolver>>) {
    if let Ok(mut c) = convolver.lock()
        && let Some(conv) = c.as_mut()
    {
        conv.reset();
    }
}

/// Fin de piste : rendre ce que le convolveur retient encore.
///
/// Une convolution par blocs garde `latency_frames()` trames en réserve — c'est
/// le prix de sa latence, et sans ce drainage elles ne partent jamais au
/// périphérique. Les échantillons rendus traversent le crossfeed comme les
/// autres, pour que la queue sonne comme le reste.
fn flush_local_dsp(
    convolver: &std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: &std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: &AtomicBool,
    mono_downmix: &AtomicBool,
    channels: u16,
    dop: bool,
) -> Vec<f32> {
    // Mêmes exemptions qu'`apply_local_dsp` : ce que la chaîne n'a pas traité,
    // elle n'a rien à en rendre.
    if dop || pure_bypass.load(Ordering::Relaxed) {
        return Vec::new();
    }
    let mut queue = match convolver.lock() {
        Ok(mut c) => match c.as_mut() {
            Some(conv) => conv.flush(),
            None => return Vec::new(),
        },
        Err(_) => return Vec::new(),
    };
    if queue.is_empty() {
        return queue;
    }
    if channels == 2
        && let Ok(mut cf) = crossfeed.lock()
        && let Some(c) = cf.as_mut()
    {
        c.process_interleaved(&mut queue);
    }
    // Même ordre que `apply_local_dsp` : sans ceci la queue du convolveur
    // sortirait en stéréo pendant que le corps de la piste sort en mono, et
    // l'auditeur à une seule enceinte entendrait la fin de chaque piste
    // s'appauvrir (#2362).
    if channels == 2 && mono_downmix.load(Ordering::Relaxed) {
        crate::audio::channels::fold_stereo_to_mono_in_place(&mut queue);
    }
    queue
}

fn apply_local_dsp(
    samples: &mut [f32],
    eq: &std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: &std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: &std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: &AtomicBool,
    mono_downmix: &AtomicBool,
    channels: u16,
    dop: bool,
) {
    if dop || pure_bypass.load(Ordering::Relaxed) {
        return;
    }
    if let Ok(mut e) = eq.lock() {
        if let Some(ref mut p) = *e {
            p.process_interleaved(samples);
        }
    }
    if let Ok(mut conv) = convolver.lock() {
        if let Some(ref mut c) = *conv {
            c.process_interleaved(samples);
        }
    }
    // Crossfeed is a headphone (local DAC) effect and only makes sense on a
    // stereo stream — the difference-based algorithm needs L/R pairs.
    if channels == 2 {
        if let Ok(mut cf) = crossfeed.lock() {
            if let Some(ref mut c) = *cf {
                c.process_interleaved(samples);
            }
        }
    }
    // Repli mono EN DERNIER (#2362) : les trois traitements ci-dessus ont tous
    // besoin de leur contexte stéréo — le crossfeed travaille sur la
    // DIFFÉRENCE des voies et n'aurait plus rien à traiter après la somme, le
    // convolveur applique une IR par canal, l'égaliseur des gains par canal.
    // La duplication tombe donc juste avant l'adaptation au périphérique.
    if channels == 2 && mono_downmix.load(Ordering::Relaxed) {
        crate::audio::channels::fold_stereo_to_mono_in_place(samples);
    }
}

/// Convert raw PCM bytes to f32 samples.
///
/// `bit_depth` semantics:
///   - 16: signed 16-bit little-endian integer
///   - 24: signed 24-bit little-endian integer (3 bytes per sample)
///   - 32: signed 32-bit little-endian integer
///   -  0: IEEE 754 32-bit float (already in [-1, 1] range)
fn pcm_bytes_to_f32(bytes: &[u8], bit_depth: u16) -> Vec<f32> {
    match bit_depth {
        0 => {
            // IEEE Float 32-bit — reinterpret bytes as f32 directly
            bytes
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect()
        }
        16 => bytes
            .chunks_exact(2)
            .map(|c| {
                let sample = i16::from_le_bytes([c[0], c[1]]);
                sample as f32 / 32768.0
            })
            .collect(),
        24 => bytes
            .chunks_exact(3)
            .map(|c| {
                let sample =
                    ((c[0] as i32) | ((c[1] as i32) << 8) | ((c[2] as i32) << 16)) << 8 >> 8; // sign-extend
                sample as f32 / 8388608.0
            })
            .collect(),
        32 => bytes
            .chunks_exact(4)
            .map(|c| {
                let sample = i32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                sample as f32 / 2147483648.0
            })
            .collect(),
        _ => {
            // Fall back to 16-bit
            bytes
                .chunks_exact(2)
                .map(|c| {
                    let sample = i16::from_le_bytes([c[0], c[1]]);
                    sample as f32 / 32768.0
                })
                .collect()
        }
    }
}

/// Decode little-endian signed PCM into the left-aligned integer words used by
/// [`NativePcmRing`]. No arithmetic is performed: every source bit keeps the
/// same relative position and the unused low bits are zero.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn pcm_bytes_to_native_i32(bytes: &[u8], bit_depth: u16) -> Vec<i32> {
    match bit_depth {
        16 => bytes
            .chunks_exact(2)
            .map(|c| i32::from(i16::from_le_bytes([c[0], c[1]])) << 16)
            .collect(),
        24 => bytes
            .chunks_exact(3)
            .map(|c| {
                let word = ((c[0] as i32) | ((c[1] as i32) << 8) | ((c[2] as i32) << 16)) << 8 >> 8;
                word << 8
            })
            .collect(),
        32 => bytes
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
        _ => Vec::new(),
    }
}

/// Write left-aligned native words back to their exact 16/24/32-bit PCM byte
/// representation. This is the WASAPI callback's final serialization step and
/// also the inverse used by the backend-boundary countertests.
#[cfg(any(target_os = "windows", test))]
pub(crate) fn native_i32_to_pcm_bytes(samples: &[i32], bit_depth: u16, out: &mut [u8]) -> usize {
    let bytes_per_sample = usize::from(bit_depth / 8);
    if !matches!(bit_depth, 16 | 24 | 32) {
        return 0;
    }
    let count = samples.len().min(out.len() / bytes_per_sample);
    for (i, sample) in samples[..count].iter().enumerate() {
        let native = sample.to_le_bytes();
        let source = &native[4 - bytes_per_sample..];
        let offset = i * bytes_per_sample;
        out[offset..offset + bytes_per_sample].copy_from_slice(source);
    }
    count * bytes_per_sample
}

#[cfg(any(target_os = "windows", test))]
fn f32_to_native_i32(samples: &[f32], bit_depth: u16) -> Vec<i32> {
    let (scale, max, shift) = match bit_depth {
        16 => (32_768.0, i16::MAX as f64, 16),
        24 => (8_388_608.0, 8_388_607.0, 8),
        32 => (2_147_483_648.0, i32::MAX as f64, 0),
        _ => return Vec::new(),
    };
    let min = -scale;
    samples
        .iter()
        .map(|sample| {
            let word = (f64::from(*sample) * scale).round().clamp(min, max) as i64;
            (word << shift) as i32
        })
        .collect()
}

#[cfg(any(target_os = "windows", test))]
fn local_dsp_is_identity(
    eq: &std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: &std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: &std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: &AtomicBool,
    mono_downmix: &AtomicBool,
) -> bool {
    if pure_bypass.load(Ordering::Relaxed) {
        return true;
    }
    // Le repli mono compte comme les autres (#2362) : il RÉÉCRIT chaque
    // échantillon. Sans lui ici, le producteur Windows prendrait la branche
    // « octets source conservés » et le repli ne serait jamais appliqué — le
    // réglage serait accepté et resterait sans effet.
    !mono_downmix.load(Ordering::Relaxed)
        && eq.lock().is_ok_and(|guard| guard.is_none())
        && convolver.lock().is_ok_and(|guard| guard.is_none())
        && crossfeed.lock().is_ok_and(|guard| guard.is_none())
}

#[cfg(any(target_os = "windows", test))]
fn local_dsp_runtime_state(
    eq: &std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: &std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: &std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: &AtomicBool,
    mono_downmix: &AtomicBool,
    dop: bool,
) -> OutputDspState {
    if dop {
        return OutputDspState::BypassedDop;
    }
    if pure_bypass.load(Ordering::Relaxed) {
        return OutputDspState::BypassedPure;
    }
    // Le repli mono est une vraie transformation : il doit APPARAÎTRE dans le
    // verdict, sans quoi le panneau annoncerait un chemin intouché pendant que
    // chaque échantillon est réécrit (#2362, famille de #1548/#1559/#1627).
    if mono_downmix.load(Ordering::Relaxed) {
        return OutputDspState::Applied;
    }
    let (Ok(eq), Ok(convolver), Ok(crossfeed)) = (eq.lock(), convolver.lock(), crossfeed.lock())
    else {
        return OutputDspState::Unknown;
    };
    if eq.is_some() || convolver.is_some() || crossfeed.is_some() {
        OutputDspState::Applied
    } else {
        OutputDspState::Inactive
    }
}

/// Compose le verdict exact que le producteur remet au callback Windows.
///
/// Le callback entier ne fait que sérialiser les mots ; le callback flottant
/// ne peut jamais garantir leurs bits. Sur le chemin entier, volume et DSP
/// décident si le producteur peut conserver les octets source ou doit faire un
/// aller-retour en espace flottant. DoP force les deux contournements à
/// l'unité : toucher son marqueur rendrait le flux illisible par le DAC.
#[cfg(any(target_os = "windows", test))]
#[allow(clippy::too_many_arguments)]
fn windows_signal_path_status(
    native_transport: bool,
    dop: bool,
    volume_units: u32,
    eq: &std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: &std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: &std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: &AtomicBool,
    mono_downmix: &AtomicBool,
) -> OutputSignalPathStatus {
    let sample_transport = if native_transport {
        OutputSampleTransport::NativeInteger
    } else {
        OutputSampleTransport::Float
    };
    let dsp = local_dsp_runtime_state(eq, convolver, crossfeed, pure_bypass, mono_downmix, dop);
    let volume = if dop {
        OutputVolumeState::BypassedDop
    } else if volume_units == 1000 {
        OutputVolumeState::Unity
    } else {
        OutputVolumeState::Applied
    };

    let mut reasons = Vec::new();
    if !native_transport {
        reasons.push(OutputSignalReason::FloatTransport);
    }
    match dsp {
        OutputDspState::Applied => reasons.push(OutputSignalReason::DspApplied),
        OutputDspState::Unknown => reasons.push(OutputSignalReason::DspStateUnknown),
        OutputDspState::Inactive | OutputDspState::BypassedPure | OutputDspState::BypassedDop => {}
    }
    if volume == OutputVolumeState::Applied {
        reasons.push(OutputSignalReason::SoftwareVolume);
    }

    OutputSignalPathStatus {
        bit_perfect: reasons.is_empty(),
        sample_transport,
        dsp,
        volume,
        reasons,
    }
}

#[cfg(any(target_os = "windows", test))]
#[allow(clippy::too_many_arguments)]
fn publish_windows_signal_path_status(
    slot: &std::sync::Mutex<Option<OutputSignalPathStatus>>,
    observed_bit_perfect: bool,
    native_transport: bool,
    dop: bool,
    volume_units: u32,
    eq: &std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: &std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: &std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: &AtomicBool,
    mono_downmix: &AtomicBool,
) -> OutputSignalPathStatus {
    let mut status = windows_signal_path_status(
        native_transport,
        dop,
        volume_units,
        eq,
        convolver,
        crossfeed,
        pure_bypass,
        mono_downmix,
    );
    // Le verdict du producteur est autoritaire : il a choisi la branche raw
    // ou flottante pour CE buffer. La lecture des verrous ci-dessus décrit
    // l'état courant et peut croiser une mise à jour à chaud juste après ce
    // choix ; elle ne doit jamais transformer un verdict négatif en promesse.
    status.bit_perfect = observed_bit_perfect;
    if !observed_bit_perfect && status.reasons.is_empty() {
        status.reasons.push(OutputSignalReason::DspStateUnknown);
    }
    if let Ok(mut current) = slot.lock() {
        *current = Some(status.clone());
    }
    status
}

#[cfg(any(target_os = "windows", test))]
struct PreparedNativePcm {
    samples: Vec<i32>,
    dop: bool,
    bit_perfect: bool,
}

/// Prepare PCM for an integer backend ring.
///
/// DoP and identity PCM take the raw branch and never become floats. Ordinary
/// PCM that actually requests volume or DSP is processed in sample space and
/// quantized once, before the integer ring; it is explicitly marked as not
/// bit-perfect so the callback never has to guess which contract it received.
#[cfg(any(target_os = "windows", test))]
#[allow(clippy::too_many_arguments)]
fn prepare_windows_native_pcm(
    bytes: &[u8],
    bit_depth: u16,
    channels: u16,
    must_classify_24_bit: bool,
    dop_latched: bool,
    volume_units: u32,
    eq: &std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: &std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: &std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: &AtomicBool,
    mono_downmix: &AtomicBool,
) -> Option<PreparedNativePcm> {
    let probe_bytes = DOP_DETECT_FRAMES * channels.max(1) as usize * 3;
    if bit_depth == 24 && must_classify_24_bit && bytes.len() < probe_bytes {
        return None;
    }

    let dop = dop_latched || (bit_depth == 24 && is_dop_pcm(bytes, bit_depth, channels));
    let bit_perfect = dop
        || (volume_units == 1000
            && local_dsp_is_identity(eq, convolver, crossfeed, pure_bypass, mono_downmix));
    let samples = if bit_perfect {
        pcm_bytes_to_native_i32(bytes, bit_depth)
    } else {
        let mut float = pcm_bytes_to_f32(bytes, bit_depth);
        apply_local_dsp(
            &mut float,
            eq,
            convolver,
            crossfeed,
            pure_bypass,
            mono_downmix,
            channels,
            false,
        );
        let volume = volume_units as f32 / 1000.0;
        if volume != 1.0 {
            for sample in &mut float {
                *sample *= volume;
            }
        }
        f32_to_native_i32(&float, bit_depth)
    };

    Some(PreparedNativePcm {
        samples,
        dop,
        bit_perfect,
    })
}

// ───────────────────────────────────────────────────────────────────────────
// La frontière producteur → puits du chemin CPAL partagé.
//
// Au-dessus de cette ligne : des octets, un format source, un DSP. En dessous :
// des mots flottants entrelacés au format du périphérique, et rien d'autre.
// C'est la seule frontière que `play_url` traverse pour faire du son, et elle
// est désormais NOMMÉE — condition pour que #2211 (vrai fondu enchaîné) et
// #2218 (puits de capture) aient un endroit où se brancher.
// ───────────────────────────────────────────────────────────────────────────

/// L'état de conversion source → sortie du chemin partagé.
///
/// La même suite de gestes était recopiée à QUATRE endroits de `play_url` —
/// amorce de la piste initiale, boucle de lecture, amorce de la piste
/// enchaînée, boucle de la piste enchaînée : décoder ce qui est aligné,
/// refuser un porteur DoP que ce chemin détruirait, adapter les canaux,
/// rééchantillonner, écrire. Un seul exemplaire les tient désormais.
///
/// L'ordre des deux conversions n'est pas indifférent : l'adaptation de canaux
/// vient AVANT le rééchantillonnage, parce que le rééchantillonneur est
/// construit pour les canaux de `sortie`.
///
/// R5 de #2219 — le format source tient maintenant en UN champ, `spec`, et le
/// format de sortie en un autre, `sortie`.
///
/// Ce qui a disparu, et pourquoi :
///
/// * `sample_rate`, `channels`, `bit_depth` → [`AudioSpec`], dont les champs
///   sont privés et dans une autre caisse. Les trois étiquettes ne se posent
///   plus qu'ensemble, et la profondeur n'est plus un `u16` qui s'intervertit
///   avec les canaux ;
/// * `frame_bytes` → **supprimé**. Ce n'était pas une étiquette mais sa
///   conséquence, recalculée à la main à chaque changement de format. La
///   frontière gapless posait quatre affectations de suite dont celle-là ;
///   oublier la quatrième, c'était lire un flux 24 bits par trames de 16 et
///   décaler toute la piste — et cela compilait ;
/// * `output_ch` → [`FormatOuvert`], le type que T8 (#3961) a déjà posé pour
///   dire ce que le périphérique a réellement ouvert. Le puits le publie déjà
///   sous ce nom ; l'étage n'avait aucune raison d'en tenir une deuxième
///   version en pièces détachées ;
/// * `needs_channel_adapt` → **déduit** de `sortie` et de `spec`. Ses deux
///   seules affectations étaient le même `output_ch != channels`.
///
/// `needs_resample` reste un champ, et ce n'est pas un oubli : ce n'est pas une
/// conséquence du format mais une **décision**. Quand la construction du
/// rééchantillonneur échoue, la frontière gapless le repose à `false` alors que
/// les deux cadences diffèrent toujours — le déduire effacerait ce repli.
struct EtageDeConversion<'a> {
    pcm: LocalPcmProcessor<'a>,
    /// Octets reçus de l'amont, pas encore alignés sur une trame source.
    en_attente: Vec<u8>,
    resampler: Option<Async<f32>>,
    resample_leftover: Vec<f32>,
    pcm_kind: LocalPcmKind,
    /// Le format de la SOURCE : celui dans lequel `en_attente` se lit.
    spec: AudioSpec,
    /// Le format réellement OUVERT par le périphérique, à l'autre bout.
    sortie: FormatOuvert,
    needs_resample: bool,
}

/// Ce qu'une poussée vers le puits a produit.
///
/// REF-7 (#2219) : `pub(super)`, parce que c'est le verdict que rend
/// [`Etage::pousser`] et que les étages des bras Windows le rendent aussi.
pub(super) enum PousseeVersLePuits {
    /// Rien d'aligné à décoder pour l'instant : il faut lire davantage.
    RienAPousser,
    /// Bloc poussé, `trames_source` trames consommées à l'entrée.
    Poussee { trames_source: u64 },
    /// Le puits a cessé de consommer (rappel mort, périphérique arraché).
    /// Les trames sont rendues quand même : les appelants d'amorçage les
    /// comptaient déjà sans regarder le verdict, et ce compte est la position.
    PuitsMort { trames_source: u64 },
    /// Porteur DoP refusé (#3233) : le fil doit se démonter sans rien servir.
    PorteurDopRefuse,
}

impl EtageDeConversion<'_> {
    /// Cadence de la source, en hertz.
    fn sample_rate(&self) -> u32 {
        self.spec.cadence()
    }

    /// Canaux de la source.
    fn channels(&self) -> u16 {
        self.spec.canaux()
    }

    /// Profondeur de la source, telle qu'un en-tête WAV la déclare.
    fn bit_depth(&self) -> u16 {
        self.spec.profondeur().bits_declares()
    }

    /// Faut-il adapter les canaux ? **Déduit**, jamais rangé : la source et la
    /// sortie se comparent, il n'y a rien à tenir à jour.
    fn needs_channel_adapt(&self) -> bool {
        self.sortie.canaux != self.spec.canaux()
    }

    /// Décode ce qui est aligné dans le tampon d'attente.
    ///
    /// `process_pcm_chunk` rend déjà `None` quand rien n'est aligné : la garde
    /// `aligned_len == 0` que les quatre copies posaient avant l'appel était le
    /// même calcul, fait deux fois.
    fn decoder(&mut self) -> Option<ProcessedLocalPcm> {
        self.pcm
            .process_pcm_chunk(&mut self.en_attente, self.spec, &mut self.pcm_kind)
    }

    /// Adaptation de canaux puis rééchantillonnage, dans cet ordre et lui seul.
    fn convertir(&mut self, mut mots: Vec<f32>) -> Vec<f32> {
        if self.needs_channel_adapt() {
            mots = adapt_channels(&mots, self.spec.canaux(), self.sortie.canaux);
        }
        if self.needs_resample {
            mots = rubato_resample_chunk(
                &mut self.resampler,
                &mots,
                self.sortie.canaux,
                false,
                &mut self.resample_leftover,
            );
        }
        mots
    }

    /// Le DSP touche-t-il les échantillons de cette piste ? La MÊME décision
    /// qu'`apply_local_dsp`, lue au lieu d'être appliquée : rien sous DoP ni
    /// en contournement pur ; sinon un égaliseur, un convolveur, un crossfeed
    /// (stéréo seulement) ou le repli mono (stéréo seulement) posés.
    fn dsp_actif(&self) -> bool {
        fn pose<T>(m: &std::sync::Mutex<Option<T>>) -> bool {
            m.lock().map(|g| g.is_some()).unwrap_or(false)
        }
        if self.pcm.dop_active.load(Ordering::Relaxed)
            || self.pcm.pure_bypass.load(Ordering::Relaxed)
        {
            return false;
        }
        let stereo = self.spec.canaux() == 2;
        pose(self.pcm.eq)
            || pose(self.pcm.convolver)
            || (stereo && pose(self.pcm.crossfeed))
            || (stereo && self.pcm.mono_downmix.load(Ordering::Relaxed))
    }
}

/// L'étage FLOTTANT : celui du chemin DSP, dont le puits range des `f32`.
impl Etage for EtageDeConversion<'_> {
    type Puits<'p> = dyn PuitsDEchantillons + 'p;
    type Bloc = ProcessedLocalPcm;

    fn recevoir(&mut self, octets: &[u8]) {
        self.en_attente.extend_from_slice(octets);
    }

    fn cadence_source(&self) -> u32 {
        self.sample_rate()
    }

    /// Le geste élémentaire du producteur : décoder, refuser un porteur DoP,
    /// convertir, écrire dans le puits.
    ///
    /// `observer` voit le bloc **avant** conversion — c'est là que vivent les
    /// diagnostics d'amorçage et la détection de silence, qui portent sur le
    /// PCM source et non sur ce qui sort du sinc.
    fn pousser(
        &mut self,
        puits: &mut (dyn PuitsDEchantillons + '_),
        refuser_le_porteur_dop: &mut dyn FnMut(bool, u32, u16) -> bool,
        observer: &mut dyn FnMut(&ProcessedLocalPcm),
    ) -> PousseeVersLePuits {
        let Some(bloc) = self.decoder() else {
            return PousseeVersLePuits::RienAPousser;
        };
        observer(&bloc);
        if refuser_le_porteur_dop(bloc.dop, self.sample_rate(), self.channels()) {
            return PousseeVersLePuits::PorteurDopRefuse;
        }
        let trames_source = bloc.source_frames;
        let mots = self.convertir(bloc.samples);
        if puits.ecrire(&mots) {
            PousseeVersLePuits::Poussee { trames_source }
        } else {
            PousseeVersLePuits::PuitsMort { trames_source }
        }
    }

    /// Rend la queue du DSP au puits, au format de la piste qui se TERMINE —
    /// celui que `self.spec` porte encore à l'instant de l'appel. Utilisé à
    /// une frontière gapless qui change de format et à la fin de la chaîne :
    /// sans ça, la queue du convolveur partirait au DAC convertie avec les
    /// paramètres du morceau suivant, ou pas du tout.
    ///
    /// REF-7 (#2219) : la queue est tirée ICI, par l'étage, qui tient déjà les
    /// références du DSP — `play_url` la lui passait toute faite, et c'était
    /// la même ligne recopiée à deux endroits, avec les mêmes arguments.
    fn rendre_la_queue_du_dsp(&mut self, puits: &mut (dyn PuitsDEchantillons + '_)) -> bool {
        let queue = flush_local_dsp(
            self.pcm.convolver,
            self.pcm.crossfeed,
            self.pcm.pure_bypass,
            self.pcm.mono_downmix,
            self.spec.canaux(),
            self.pcm.dop_active.load(Ordering::Relaxed),
        );
        // La queue est d'abord un bloc normal : elle traverse la MÊME
        // conversion que l'audio qui la précède. `flush = true` ignore son
        // argument `samples` et la jetterait.
        if queue.is_empty() {
            return true;
        }
        let mots = self.convertir(queue);
        puits.ecrire(&mots)
    }

    /// Vide le rééchantillonneur : le reliquat plus le délai interne du sinc.
    /// C'est à l'appelant de décider QUAND — fin de chaîne, ou frontière
    /// gapless qui change de cadence ; à cadence identique le rééchantillonneur
    /// fait partie du flux continu et ne se vide pas.
    fn vider(&mut self, puits: &mut (dyn PuitsDEchantillons + '_)) -> bool {
        let flushed = rubato_resample_chunk(
            &mut self.resampler,
            &[],
            self.sortie.canaux,
            true,
            &mut self.resample_leftover,
        );
        if flushed.is_empty() {
            return true;
        }
        puits.ecrire(&flushed)
    }

    fn transformations(&self) -> TransformationsReelles {
        TransformationsReelles::nouvelles(self.spec, self.sortie, self.dsp_actif())
    }
}

/// Un bloc que l'étage vient de décoder, tel que la boucle producteur le
/// regarde : elle ne connaît ni `f32` ni `i32`, elle demande seulement s'il y
/// a quelque chose dedans et si c'est du silence — c'est tout ce que son
/// diagnostic `local_audio_first_samples_all_zero` a besoin de savoir.
pub(super) trait BlocDecode {
    fn nb_echantillons(&self) -> usize;
    fn contient_un_echantillon_non_nul(&self) -> bool;
}

impl BlocDecode for ProcessedLocalPcm {
    fn nb_echantillons(&self) -> usize {
        self.samples.len()
    }

    fn contient_un_echantillon_non_nul(&self) -> bool {
        self.samples.iter().any(|&s| s != 0.0)
    }
}

/// REF-7 (#2219) — l'ÉTAGE, ce qui sépare les octets de la source du puits.
///
/// [`BoucleProducteur::tourner`] est générique sur ce trait et ne sait rien du
/// mot que le puits range : l'étage flottant ([`EtageDeConversion`]) décode
/// vers des `f32` et son puits est un [`PuitsDEchantillons`] ; l'étage des
/// bras Windows décode vers des mots natifs et son puits est un
/// [`super::traits::PuitsNatif`]. C'est le type associé `Puits` qui le dit,
/// et c'est lui qui rend la boucle indifférente à la route — sans un `match`
/// par bloc sur un puits qui n'est jamais de l'autre sorte.
///
/// Un étage sait QUATRE choses, et c'est tout ce que la boucle lui demande :
/// recevoir des octets source, les décoder et les pousser vers SON puits
/// (la garde DoP y est appelée AVANT la conversion, #3233), rendre la queue
/// de son DSP, vider son rééchantillonneur. Et une cinquième pour l'écran :
/// dire ce qu'il fait réellement au signal (REF-6b, #3987).
///
/// Déclaré APRÈS son premier implémenteur, et ce n'est pas un hasard : les
/// gardes de texte (`garde_de_site_porteur_dop_3233`, `dsp_track_boundary`)
/// coupent au PREMIER `fn pousser(` du fichier et doivent tomber sur le corps
/// réel, pas sur une signature sans corps.
pub(super) trait Etage {
    /// Le puits que cet étage alimente — `dyn PuitsDEchantillons + 'p` pour
    /// le chemin flottant, `dyn PuitsNatif + 'p` pour le chemin natif.
    ///
    /// `'p` est la durée de vie de l'objet : le puits d'un backend emprunte
    /// les témoins d'arrêt de `play_url` (`Puits<'a>`, #4009) et n'est donc
    /// jamais `'static` — un `type Puits = dyn …` sans durée de vie le serait,
    /// et forcerait tous les témoins à l'être aussi.
    type Puits<'p>: ?Sized;
    /// Le bloc décodé, tel que l'observateur d'amorçage le voit.
    type Bloc: BlocDecode;

    /// Range des octets SOURCE reçus de l'amont, pas encore alignés.
    fn recevoir(&mut self, octets: &[u8]);

    /// Cadence de la SOURCE, en hertz : c'est elle qui convertit les trames
    /// servies en position.
    fn cadence_source(&self) -> u32;

    /// Décode ce qui est aligné, refuse un porteur DoP, convertit, écrit.
    fn pousser(
        &mut self,
        puits: &mut Self::Puits<'_>,
        refuser_le_porteur_dop: &mut dyn FnMut(bool, u32, u16) -> bool,
        observer: &mut dyn FnMut(&Self::Bloc),
    ) -> PousseeVersLePuits;

    /// Rend la queue du DSP au puits, au format de la piste qui se termine.
    /// Rend `false` uniquement quand le puits a cessé de consommer.
    fn rendre_la_queue_du_dsp(&mut self, puits: &mut Self::Puits<'_>) -> bool;

    /// Vide le rééchantillonneur dans le puits. Même contrat de retour.
    fn vider(&mut self, puits: &mut Self::Puits<'_>) -> bool;

    /// Ce que cet étage fait RÉELLEMENT au signal, à cet instant.
    fn transformations(&self) -> TransformationsReelles;
}

/// Ce qui distingue la boucle de la piste initiale de celle d'une piste
/// enchaînée en gapless : **les noms d'événement journalisés**, rien d'autre.
///
/// Les deux boucles lisaient, décodaient, convertissaient et écrivaient
/// exactement pareil. Les garder séparées, c'était garder deux endroits où
/// corriger un bug de lecture — et #3108 avait déjà dû être corrigé deux fois.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RoleDeLaBoucle {
    PisteInitiale,
    PisteEnchainee,
}

/// Pourquoi la boucle producteur s'est arrêtée.
enum FinDeBoucle {
    /// L'amont a rendu EOF — ou une erreur de lecture, que les deux boucles
    /// traitaient déjà comme une fin de flux. La piste peut s'enchaîner.
    FinDeFlux,
    /// Arrêt demandé, périphérique perdu, ou puits mort : la piste ne s'est
    /// pas terminée d'elle-même et ne doit RIEN enchaîner.
    Interrompue,
    /// Porteur DoP refusé (#3233). Le fil doit se démonter, et c'est à
    /// l'appelant de baisser `playing` — sous la garde de génération, pour ne
    /// pas éteindre une lecture PLUS RÉCENTE qui nous a déjà supplantés.
    PorteurDopRefuse,
    /// Le fil doit se démonter immédiatement. `playing` a DÉJÀ été baissé par
    /// le rappel qui a échoué (`apres_ecriture`).
    Abandon,
}

/// La boucle qui tire les octets de l'amont et les pousse dans le puits.
///
/// **Sortie de `play_url`**, où elle existait en deux exemplaires. Elle ne
/// connaît ni cpal, ni anneau, ni périphérique : elle connaît un `Read`, un
/// [`EtageDeConversion`] et un [`PuitsDEchantillons`]. C'est ce qui permettra
/// de lui brancher un second puits sans la toucher.
struct BoucleProducteur<'a> {
    role: RoleDeLaBoucle,
    device_name: &'a str,
    cle_de_flux: Option<&'a str>,
    stop_rx: &'a std::sync::mpsc::Receiver<()>,
    force_silent: &'a AtomicBool,
    device_gone: &'a AtomicBool,
    position_ms: &'a AtomicU64,
    open_failure: &'a std::sync::Mutex<Option<String>>,
    /// Départ du chronomètre du flux. Fourni par l'appelant et non pris à
    /// l'entrée de la boucle : la piste initiale l'arme AVANT de tester le
    /// pré-remplissage déjà acquis, et les durées journalisées comptent à
    /// partir de là.
    debut_du_flux: std::time::Instant,
}

/// Les compteurs d'une piste, que la boucle fait avancer.
struct CompteursDePiste {
    total_bytes_read: u64,
    total_frames_fed: u64,
    seek_offset: u64,
    /// Octets PCM à jeter avant d'atteindre la position demandée. Zéro pour
    /// une piste enchaînée, qui repart toujours de son début.
    skip_bytes: u64,
    skipped_bytes: u64,
    /// Faux tant que l'arrivée des premières données n'a pas été journalisée.
    premiere_donnee_journalisee: bool,
}

impl BoucleProducteur<'_> {
    /// Tourne jusqu'à la fin du flux ou jusqu'à une interruption.
    ///
    /// `apres_ecriture` est appelé après chaque bloc réellement poussé : c'est
    /// par là que la piste initiale démarre le flux cpal une fois le
    /// pré-remplissage atteint. Il rend `false` pour demander l'abandon du fil.
    ///
    /// REF-7 (#2219) : générique sur l'[`Etage`]. La boucle ne sait pas si le
    /// puits range des `f32` ou des mots natifs — elle reçoit, pousse, compte
    /// et publie la position ; tout ce qui touche au mot est dans l'étage.
    #[allow(clippy::too_many_arguments)]
    fn tourner<E: Etage>(
        &self,
        amont: &mut dyn std::io::Read,
        tampon_de_lecture: &mut [u8],
        etage: &mut E,
        puits: &mut E::Puits<'_>,
        refuser_le_porteur_dop: &mut dyn FnMut(bool, u32, u16) -> bool,
        compteurs: &mut CompteursDePiste,
        apres_ecriture: &mut dyn FnMut(&CompteursDePiste) -> bool,
    ) -> FinDeBoucle {
        let initiale = self.role == RoleDeLaBoucle::PisteInitiale;
        let debut_du_flux = self.debut_du_flux;
        loop {
            // Arrêt demandé. La piste initiale nomme les deux témoins
            // séparément — c'est son journal de diagnostic historique ; la
            // piste enchaînée sort sans un mot, comme avant.
            if self.stop_rx.try_recv().is_ok() {
                if initiale {
                    debug!(
                        total_bytes_read = compteurs.total_bytes_read,
                        total_frames_fed = compteurs.total_frames_fed,
                        "local_audio_stopped_by_signal"
                    );
                }
                return FinDeBoucle::Interrompue;
            }
            if self.force_silent.load(Ordering::Relaxed) {
                if initiale {
                    debug!(
                        total_bytes_read = compteurs.total_bytes_read,
                        total_frames_fed = compteurs.total_frames_fed,
                        "local_audio_stopped_by_abort_flag"
                    );
                }
                return FinDeBoucle::Interrompue;
            }
            // Périphérique disparu en cours de piste (USB arraché, #1626) :
            // arrêter de lire, personne ne jouera jamais ces échantillons. Pas
            // de fin naturelle : on n'enchaîne pas la file sur un mort.
            if self.device_gone.load(Ordering::Relaxed) {
                // Les deux noms d'événement sont conservés MOT POUR MOT : ce
                // sont eux qu'on cherche dans les journaux d'un testeur, et une
                // réorganisation n'a pas à renommer ce qui se lit dehors.
                if initiale {
                    warn!(
                        device = %self.device_name,
                        total_bytes_read = compteurs.total_bytes_read,
                        "local_audio_stopped_device_lost"
                    );
                } else {
                    warn!(
                        device = %self.device_name,
                        total_bytes_read = compteurs.total_bytes_read,
                        "local_audio_gapless_stopped_device_lost"
                    );
                }
                return FinDeBoucle::Interrompue;
            }

            let debut_de_lecture = std::time::Instant::now();
            let lus = match amont.read(tampon_de_lecture) {
                Ok(0) => {
                    if initiale {
                        debug!(
                            total_bytes_read = compteurs.total_bytes_read,
                            total_frames_fed = compteurs.total_frames_fed,
                            elapsed_ms = debut_du_flux.elapsed().as_millis() as u64,
                            "local_audio_stream_eof"
                        );
                    } else {
                        debug!(
                            total_bytes_read = compteurs.total_bytes_read,
                            total_frames_fed = compteurs.total_frames_fed,
                            "local_audio_gapless_track_eof"
                        );
                    }
                    return FinDeBoucle::FinDeFlux;
                }
                Ok(n) => n,
                Err(ref e)
                    if e.kind() == std::io::ErrorKind::TimedOut
                        || e.kind() == std::io::ErrorKind::WouldBlock =>
                {
                    // Délai de lecture : reboucler pour revoir les témoins.
                    continue;
                }
                Err(e) => {
                    if initiale {
                        journaliser_erreur_de_lecture(
                            self.device_name,
                            self.cle_de_flux,
                            &e.to_string(),
                            compteurs.total_bytes_read,
                        );
                    } else {
                        warn!(error = %e, "local_audio_gapless_read_error");
                    }
                    // Les deux boucles traitaient déjà une erreur de lecture
                    // comme une fin de flux : la piste a joué ce qu'elle avait.
                    return FinDeBoucle::FinDeFlux;
                }
            };
            let duree_de_lecture = debut_de_lecture.elapsed();

            if initiale {
                if !compteurs.premiere_donnee_journalisee {
                    info!(
                        bytes = lus,
                        wait_ms = debut_du_flux.elapsed().as_millis() as u64,
                        "local_audio_first_pcm_data_received"
                    );
                    compteurs.premiere_donnee_journalisee = true;
                } else if duree_de_lecture.as_millis() > 5000 {
                    journaliser_lecture_lente(
                        self.device_name,
                        self.cle_de_flux,
                        lus,
                        duree_de_lecture.as_millis() as u64,
                        compteurs.total_bytes_read,
                    );
                }
            }

            compteurs.total_bytes_read += lus as u64;

            // Saut de position : jeter les octets PCM jusqu'à l'offset demandé.
            if compteurs.skip_bytes > 0 && compteurs.skipped_bytes < compteurs.skip_bytes {
                let reste_a_jeter = (compteurs.skip_bytes - compteurs.skipped_bytes) as usize;
                if lus <= reste_a_jeter {
                    compteurs.skipped_bytes += lus as u64;
                    continue;
                }
                compteurs.skipped_bytes = compteurs.skip_bytes;
                etage.recevoir(&tampon_de_lecture[reste_a_jeter..lus]);
            } else {
                etage.recevoir(&tampon_de_lecture[..lus]);
            }

            let premiere_donnee = compteurs.premiere_donnee_journalisee;
            let trames_deja_servies = compteurs.total_frames_fed;
            let pousse = etage.pousser(puits, refuser_le_porteur_dop, &mut |bloc| {
                // Silence total au démarrage : le signe d'un décodage qui a
                // échoué. Diagnostic de la piste initiale seule, comme avant.
                if initiale && (!premiere_donnee || trames_deja_servies == 0) {
                    let non_nul = bloc.contient_un_echantillon_non_nul();
                    if !non_nul && bloc.nb_echantillons() != 0 {
                        warn!(
                            sample_count = bloc.nb_echantillons(),
                            "local_audio_first_samples_all_zero"
                        );
                    }
                }
            });

            let trames_source = match pousse {
                PousseeVersLePuits::RienAPousser => continue,
                PousseeVersLePuits::PorteurDopRefuse => return FinDeBoucle::PorteurDopRefuse,
                PousseeVersLePuits::PuitsMort { .. } => {
                    // Blocage : le rappel de rendu ne draine plus (flux mort
                    // après un arrachage USB sur macOS, où AUCUN rappel
                    // d'erreur ne se déclenche, #1626). Sans cette sortie, la
                    // boucle attendait 5 s à CHAQUE bloc, position figée.
                    if initiale {
                        warn!(
                            device = %self.device_name,
                            total_bytes_read = compteurs.total_bytes_read,
                            "local_audio_stopped_feed_stall"
                        );
                    } else {
                        warn!(
                            device = %self.device_name,
                            total_bytes_read = compteurs.total_bytes_read,
                            "local_audio_gapless_stopped_feed_stall"
                        );
                    }
                    // …et sans celle-ci, il s'arrêtait SANS RIEN DIRE (#3108).
                    record_feed_stall_failure(
                        "CPAL",
                        self.device_name,
                        self.position_ms.load(Ordering::Relaxed),
                        self.open_failure,
                    );
                    return FinDeBoucle::Interrompue;
                }
                PousseeVersLePuits::Poussee { trames_source } => trames_source,
            };

            compteurs.total_frames_fed += trames_source;

            if !apres_ecriture(compteurs) {
                return FinDeBoucle::Abandon;
            }

            let position = (compteurs.total_frames_fed as f64 / etage.cadence_source() as f64
                * 1000.0) as u64
                + compteurs.seek_offset;
            self.position_ms.store(position, Ordering::Relaxed);
        }
    }
}

#[async_trait::async_trait]
impl OutputTarget for LocalOutput {
    fn name(&self) -> &str {
        &self.device_name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "local"
    }

    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(
            true,
            true,
            true,
            true,
            true,
            self.supports_internal_gapless(),
        )
        .with_linear_volume(1000)
    }

    /// Exclusive-mode playback (ASIO / WASAPI exclusive) uses a dedicated loop
    /// that returns at EOF without consuming the staged `next_media`, so it
    /// cannot chain internally — the poller must fall back to natural-end
    /// advance. Only the shared cpal path performs internal gapless chaining.
    ///
    /// Et « performe » se conjugue au présent : la réponse est une **sonde
    /// vivante**, pas une capacité gravée. Une boucle d'enchaînement qui s'est
    /// arrêtée ne peut plus rien enchaîner, et doit le dire — sans quoi le
    /// poller attend une transition d'un fil qui n'existe plus (`#1323` sur
    /// OAAT, `#1919` ici). Voir [`LocalOutput::chain_exhausted`].
    fn supports_internal_gapless(&self) -> bool {
        !self.exclusive_mode && !self.chain_exhausted.load(Ordering::Relaxed)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn set_next_url(
        &self,
        url: &str,
        _mime_type: &str,
        title: Option<&str>,
        artist: Option<&str>,
    ) -> Result<(), String> {
        *self.next_media.lock().unwrap() = Some(PendingNextMedia {
            url: url.to_string(),
            title: title.map(String::from),
            artist: artist.map(String::from),
            duration_ms: None,
        });
        debug!("local_audio_gapless_next_url_set");
        Ok(())
    }

    async fn set_next_media(&self, media: &super::traits::PlayMedia<'_>) -> Result<(), String> {
        *self.next_media.lock().unwrap() = Some(PendingNextMedia {
            url: media.url.to_string(),
            title: media.title.map(String::from),
            artist: media.artist.map(String::from),
            duration_ms: media.duration_ms,
        });
        info!(
            title = ?media.title,
            "local_audio_gapless_next_media_set"
        );
        Ok(())
    }

    async fn play_media(&self, media: &super::traits::PlayMedia<'_>) -> Result<(), String> {
        let result = self
            .play_url(media.url, media.mime_type, media.title, media.artist)
            .await;
        // Store duration AFTER play_url() because play_url() calls stop()
        // which resets duration_ms to 0.
        if let Some(dur) = media.duration_ms {
            self.duration_ms.store(dur, Ordering::SeqCst);
        }
        result
    }

    async fn play_url(
        &self,
        url: &str,
        _mime_type: &str,
        title: Option<&str>,
        artist: Option<&str>,
    ) -> Result<(), String> {
        // Chronomètre du « temps avant la première note » : play_url couvre
        // TOUT ce que l'utilisateur perçoit comme le chargement — arrêt de la
        // piste précédente, remise à zéro du DSP, ouverture du flux, décodage,
        // pré-remplissage, ouverture du périphérique. `playback_timing` de
        // l'orchestrateur s'arrête à l'envoi de l'ordre : il ne voyait rien de
        // tout ça (chantier lenteurs, 24/08).
        let chrono_demarrage = std::time::Instant::now();
        self.stop().await.ok();

        // Frontière de piste : le convolveur vit aussi longtemps que la sortie,
        // il ne doit pas verser la queue de la piste précédente dans celle-ci
        // (JP Robbe, revue de #2268). `play_url` est le seul point par lequel
        // passe un début de piste — une transition gapless ne passe pas par là,
        // et c'est voulu : l'audio y est continu.
        reset_local_dsp(&self.convolver);

        // Restore seek position after stop() cleared the old state.
        let start_position_ms = self.pending_start_position_ms.swap(0, Ordering::SeqCst);
        self.seek_offset_ms
            .store(start_position_ms, Ordering::SeqCst);
        self.position_ms.store(start_position_ms, Ordering::SeqCst);
        // stream_pre_seeked is set explicitly by set_producer_seeked()
        // from the orchestrator. Both transcode arms (local file AND
        // Qobuz/Tidal streaming) pre-seek the decoder, so the consumer
        // must not byte-skip the offset again (#1518).

        // Clear any staged gapless next — starting from scratch.
        *self.next_media.lock().unwrap() = None;
        // (`chain_exhausted` est remis à zéro plus bas, APRÈS l'incrément de
        // `play_generation` — voir le commentaire là-bas : le faire ici
        // laisserait une fenêtre où l'ancien fil peut relever le drapeau.)

        // Brief pause after stopping the old stream to allow the OS audio
        // subsystem (CoreAudio / WASAPI / ALSA) to fully release the device.
        // Without this, reopening the device immediately can cause the first
        // few hundred milliseconds of the new stream to contain stale data
        // from the previous session, perceived as white noise / static.
        //
        // On Windows, ASIO/WASAPI needs time to fully release the device.
        // ASIO exclusive is slower to release (~500ms for driver teardown).
        #[cfg(target_os = "windows")]
        {
            let delay = if self.audio_backend == "asio" {
                500
            } else {
                200
            };
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        // #3575 — sur Linux le PCM ouvert est `hw:CARD=…` depuis `ee4ec884` :
        // EXCLUSIF. Dormir 50 ms en espérant que le fil précédent ait fini
        // n'était pas une mesure, c'était un pari — et `stop()` vient
        // peut-être de le DÉTACHER sans qu'il ait rendu quoi que ce soit.
        // On demande donc à sa sentinelle, au lieu de le supposer.
        #[cfg(not(target_os = "windows"))]
        {
            let sentinelle = self.sentinelle_du_fil.lock().unwrap().clone();
            let mut attendu_ms: u64 = 0;
            loop {
                let vivant = sentinelle
                    .as_ref()
                    .is_some_and(|s| s.load(Ordering::SeqCst));
                match decider_la_relache_du_peripherique(
                    vivant,
                    attendu_ms,
                    BUDGET_RELACHE_PERIPHERIQUE_MS,
                ) {
                    RelacheDuPeripherique::Libre => {
                        if attendu_ms > 0 {
                            info!(
                                device = %self.device_name,
                                attendu_ms,
                                "local_audio_peripherique_relache_par_le_fil_precedent"
                            );
                        }
                        break;
                    }
                    RelacheDuPeripherique::Attendre { apres_ms } => {
                        if attendu_ms == 0 {
                            warn!(
                                device = %self.device_name,
                                budget_ms = BUDGET_RELACHE_PERIPHERIQUE_MS,
                                "local_audio_peripherique_encore_tenu_par_le_fil_precedent"
                            );
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(apres_ms)).await;
                        attendu_ms += apres_ms;
                    }
                    RelacheDuPeripherique::ForcerEtLeDire => {
                        warn!(
                            device = %self.device_name,
                            attendu_ms,
                            "local_audio_ouverture_forcee_le_fil_precedent_tient_encore"
                        );
                        break;
                    }
                }
            }
            // Le repos que l'ancien sommeil accordait au sous-système audio
            // reste dû : ALSA/CoreAudio veulent quelques dizaines de ms entre
            // la fermeture et la réouverture, sans quoi les premières trames
            // portent le résidu de la session précédente.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // Create a FRESH force_silent flag for the new stream.
        // The old stream's callback keeps its clone of the previous Arc
        // (which was set to true by stop()), so it stays silent.
        // This prevents the race where resetting force_silent would
        // accidentally un-silence the old cpal callback.
        let new_force_silent = Arc::new(AtomicBool::new(false));
        *self.force_silent.lock().unwrap() = new_force_silent.clone();
        let force_silent = new_force_silent;

        let my_generation = self.play_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let play_generation = self.play_generation.clone();

        // Clear the natural-end flag and generation for the new track.
        self.track_ended_naturally.store(false, Ordering::SeqCst);
        self.track_ended_generation.store(0, Ordering::SeqCst);
        // Un fil neuf a une boucle d'enchaînement intacte : la sonde repart de
        // zéro. **Après** l'incrément de `play_generation`, et c'est tout
        // l'intérêt : l'ancien fil ne lève son drapeau que s'il est encore la
        // génération courante. Remis à zéro AVANT l'incrément, il restait une
        // fenêtre — les 50 à 500 ms d'attente de libération du périphérique —
        // pendant laquelle l'ancien fil, toujours reconnu comme courant,
        // relevait le drapeau juste après l'effacement : le nouveau morceau
        // héritait alors d'une sonde éteinte et perdait son gapless pour toute
        // sa durée. Ici, l'ancien fil est soit déjà passé (on efface après
        // lui), soit périmé (générations différentes, il ne lève rien).
        self.chain_exhausted.store(false, Ordering::SeqCst);
        // A device-open failure belongs to the track that provoked it. Clearing
        // it here means a user who fixes the device and presses play again is
        // never stopped by the previous attempt's error.
        if let Ok(mut slot) = self.open_failure.lock() {
            *slot = None;
        }
        if let Ok(mut slot) = self.signal_path_status.lock() {
            *slot = None;
        }
        let open_failure = self.open_failure.clone();
        #[cfg(target_os = "windows")]
        let signal_path_status = self.signal_path_status.clone();
        let track_ended_naturally = self.track_ended_naturally.clone();
        let track_ended_generation = self.track_ended_generation.clone();

        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let device_name = self.device_name.clone();
        // Plus seulement pour WASAPI exclusif (#2207) : le chemin CPAL partagé
        // s'en sert désormais pour retrouver un périphérique renommé (#2269) et
        // pour ne pas confondre deux homonymes (#2272).
        let endpoint_id = self.endpoint_id.clone();
        // #3230 : l'hôte dont vient `device_name`. Sans lui, la résolution ne
        // peut pas distinguer « introuvable ici » de « n'a jamais été d'ici ».
        let origin_host = self.origin_host.clone();
        let url = url.to_string();
        let playing = self.playing.clone();
        let paused = self.paused.clone();
        let volume = self.volume.clone();
        // #3205 : le compteur de famine suit le flux dans le fil de lecture et
        // sera confié à l'anneau de la branche effectivement retenue.
        let starvation = self.starvation.clone();
        let position_ms = self.position_ms.clone();
        let mut seek_offset = self.seek_offset_ms.load(Ordering::SeqCst);
        let seek_offset_arc = self.seek_offset_ms.clone();
        let pre_seeked = self.stream_pre_seeked.load(Ordering::SeqCst);
        let duration_ms_arc = self.duration_ms.clone();
        let exclusive_mode = self.exclusive_mode;
        let audio_backend = self.audio_backend.clone();
        let eq = self.eq.clone();
        let current_format = self.current_format.clone();
        let convolver_config = self.convolver_config.clone();
        let convolver = self.convolver.clone();
        let pure_bypass = self.pure_bypass.clone();
        let mono_downmix = self.mono_downmix.clone();
        let crossfeed = self.crossfeed.clone();
        let dop_active = self.dop_active.clone();
        // Porte de la rampe anti-« ploc » (#1590). Une seule valeur clonable
        // plutôt que trois atomiques de plus dans des fermetures qui en portent
        // déjà huit.
        let soft_mute = self.soft_mute_gate();
        // Les deux composantes du volume effectif, pour pouvoir le recalculer
        // depuis la boucle d'alimentation quand le flux entre ou sort du DoP —
        // `recompute_effective_volume` est une méthode et n'est pas atteignable
        // depuis ce thread.
        let user_volume_ref = self.user_volume.clone();
        let rg_factor_ref = self.rg_factor.clone();
        // Arcs for gapless metadata updates from the playback thread
        let next_media_ref = self.next_media.clone();
        let chain_exhausted_ref = self.chain_exhausted.clone();
        let uri_ref = self.current_uri.clone();
        let title_ref = self.track_title.clone();
        let artist_ref = self.track_artist.clone();

        // Store metadata
        *self.current_uri.lock().unwrap() = Some(url.clone());
        *self.track_title.lock().unwrap() = title.map(String::from);
        *self.track_artist.lock().unwrap() = artist.map(String::from);

        playing.store(true, Ordering::SeqCst);
        paused.store(false, Ordering::SeqCst);
        position_ms.store(seek_offset, Ordering::SeqCst);
        // NOTE: duration_ms is NOT reset here — play_media() sets it before
        // calling play_url(), and resetting would wipe the known duration.
        // It is cleared in stop() instead.

        // Armée avant le `spawn` pour qu'aucune fenêtre ne s'ouvre entre la
        // publication de la sentinelle et le démarrage effectif du fil : un
        // `play_url` concurrent doit voir « vivant » dès maintenant.
        let sentinelle_vivante = Arc::new(AtomicBool::new(true));
        *self.sentinelle_du_fil.lock().unwrap() = Some(sentinelle_vivante.clone());
        let handle = std::thread::spawn(move || {
            // PREMIÈRE déclaration du fil, donc DERNIÈRE détruite : la
            // sentinelle ne retombe qu'après le `Drop` du flux cpal, c'est-à-
            // dire après la fermeture effective du PCM (#3575).
            let _sentinelle_du_fil = SentinelleDuFilDeLecture(sentinelle_vivante);
            // ------- HTTP fetch the audio stream -------
            // No total timeout — long tracks can stream for 30+ minutes.
            // The force_silent flag is checked at every loop iteration and
            // in feed_ring to abort promptly on stop().
            let response = match crate::http::client::blocking_builder()
                .timeout(None)
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .and_then(|client| client.get(&url).send())
            {
                Ok(r) => r,
                Err(e) => {
                    warn!(error = %e, url = %url, "local_audio_http_fetch_failed");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
            };

            if !response.status().is_success() && response.status().as_u16() != 206 {
                warn!(status = %response.status(), url = %url, "local_audio_http_error");
                playing.store(false, Ordering::SeqCst);
                return;
            }

            // Read first bytes to detect WAV header
            use std::io::Read;
            let mut reader = response;
            let mut header_buf = vec![0u8; 4096];
            let read_start = std::time::Instant::now();
            let header_read = loop {
                if force_silent.load(Ordering::Relaxed) {
                    debug!("local_audio_header_read_aborted");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
                match reader.read(&mut header_buf) {
                    Ok(n) => break n,
                    Err(ref e) if header_read_should_retry(e.kind()) => {
                        // Retry header read (stream not ready yet)
                        continue;
                    }
                    Err(e) => {
                        warn!(error = %e, "local_audio_header_read_failed");
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            };
            let read_elapsed = read_start.elapsed();
            debug!(
                header_bytes = header_read,
                elapsed_ms = read_elapsed.as_millis() as u64,
                "local_audio_first_read"
            );
            header_buf.truncate(header_read);

            // Set by the cpal stream error callback when the output device
            // vanishes mid-playback (USB DAC hot-unplugged, #1626). Checked by
            // the feed and drain loops below so the thread tears down cleanly
            // instead of waiting forever on a ring buffer nobody drains.
            let device_gone = Arc::new(AtomicBool::new(false));

            let (channels, sample_rate, bit_depth, data_offset) = if let Some(parsed) =
                parse_wav_header(&header_buf)
            {
                info!(
                    channels = parsed.0,
                    sample_rate = parsed.1,
                    bit_depth = parsed.2,
                    data_offset = parsed.3,
                    "local_audio_wav_header_parsed"
                );
                parsed
            } else {
                // No WAV header — this is a compressed stream (FLAC, MP3, AAC).
                // Read the rest of the stream, decode with symphonia, and play.
                info!("local_audio_non_wav_stream_detected_decoding");

                // Read the entire remaining stream
                let mut all_data = header_buf.clone();
                let mut buf = vec![0u8; 65536];
                loop {
                    if stop_rx.try_recv().is_ok() {
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                    if force_silent.load(Ordering::Relaxed) {
                        debug!("local_audio_compressed_read_aborted_by_stop");
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => all_data.extend_from_slice(&buf[..n]),
                        Err(ref e)
                            if e.kind() == std::io::ErrorKind::TimedOut
                                || e.kind() == std::io::ErrorKind::WouldBlock =>
                        {
                            // Read timeout — check abort flag and retry
                            continue;
                        }
                        Err(e) => {
                            warn!(error = %e, "local_audio_compressed_read_error");
                            break;
                        }
                    }
                }

                // Decode the compressed audio.
                //
                // #3270 : l'échec passe par `open_failure`, le canal que le
                // sondeur draine. Un `return` nu laissait la zone s'arrêter
                // sans que l'écran apprenne jamais pourquoi.
                let (dec_channels, dec_sample_rate, decoded_samples) =
                    match decode_compressed_stream(&all_data) {
                        Ok(decoded) => decoded,
                        Err(reason) => {
                            record_compressed_decode_failure(reason, &device_name, &open_failure);
                            playing.store(false, Ordering::SeqCst);
                            return;
                        }
                    };

                // Now play the decoded f32 samples using cpal shared mode
                let dec_ch = dec_channels;
                let dec_sr = dec_sample_rate;
                let decoded_len = decoded_samples.len();

                let host = select_host(&audio_backend);
                let Some((device, fell_back)) = find_device_with_fallback(
                    &host,
                    &device_name,
                    endpoint_id.as_deref(),
                    origin_host.as_deref(),
                ) else {
                    record_shared_device_not_found(
                        SharedDeviceResolution::CompressedStreamNotFound,
                        &device_name,
                        &open_failure,
                    );
                    playing.store(false, Ordering::SeqCst);
                    return;
                };
                if fell_back {
                    info!(
                        original = %device_name,
                        "audio_device_fallback_used_for_compressed_stream"
                    );
                }

                // Sur ALSA, `endpoint_id` EST le nom de PCM ouvert
                // (`hw:CARD=…` atteint le pilote ; `default`, `dmix:`,
                // `plughw:` passent par un greffon qui reechantillonne en
                // silence). Le chemin PCM le journalise depuis #1655 — pas
                // celui-ci, qui ouvre pourtant le meme peripherique. Un releve
                // de terrain y etait donc aveugle : il ne pouvait pas dire si
                // Tune avait ouvert le materiel ou un reechantillonneur
                // (#3209). Une ligne de journal, rien d'autre : le choix du
                // peripherique n'est pas touche ici.
                let opened_endpoint_id = device.id().map(|id| id.to_string()).unwrap_or_default();
                info!(
                    backend = %host.id().name(),
                    endpoint_id = %opened_endpoint_id,
                    "local_audio_compressed_open_endpoint"
                );
                // #3575 - meme nom que sur le chemin PCM : la garde de site lit
                // UN motif, pas deux, et un troisieme chemin d'echec ajoute
                // demain tombera dessus.
                #[cfg(target_os = "linux")]
                let pcm_ouvert = opened_endpoint_id.clone();

                // Prefer device's default rate and resample if needed.
                // Same rationale as the WAV path: opening at the source
                // rate in shared mode is unreliable on macOS/Windows.
                let output_config = {
                    let default_cfg = match device.default_output_config() {
                        Ok(c) => Some(c.config()),
                        Err(e) => {
                            // #3575 — `device_default_sr=None` était une ABSENCE, et
                            // une absence ne prouve rien : elle se lisait « ce
                            // périphérique n'annonce pas de cadence par défaut »
                            // alors qu'elle veut dire « on vient d'échouer à
                            // l'ouvrir ». Sur ALSA cette sonde ouvre le MÊME PCM que
                            // la lecture (`cpal-0.17.3/src/host/alsa/mod.rs:457`) :
                            // son échec EST le premier `EBUSY`, quelques
                            // millisecondes avant celui qui arrêtera la zone.
                            //
                            // Relevé de Belkadi Yacine, 13 ouvertures sur 13 :
                            // `None` sur les DIX échecs, une cadence réellement lue
                            // sur les TROIS réussites. La ligne, elle, manquait.
                            warn!(
                                device = %device_name,
                                error = %e,
                                "local_audio_default_config_probe_failed"
                            );
                            None
                        }
                    };
                    let default_sr = default_cfg.as_ref().map(|c| c.sample_rate);
                    if default_sr == Some(dec_sr) {
                        default_cfg.unwrap()
                    } else if let Some(cfg) = default_cfg {
                        info!(
                            source_sr = dec_sr,
                            device_sr = cfg.sample_rate,
                            "local_audio_compressed_rate_mismatch_will_resample"
                        );
                        cfg
                    } else {
                        find_matching_config(&device, dec_ch, dec_sr).unwrap_or(
                            cpal::StreamConfig {
                                channels: dec_ch,
                                sample_rate: dec_sr,
                                buffer_size: cpal::BufferSize::Default,
                            },
                        )
                    }
                };

                // Cadence SOURCE : le second candidat de la cascade. Certaines
                // plateformes (PipeWire) acceptent une cadence arbitraire là où
                // la cadence par défaut du périphérique est refusée.
                let source_config = cpal::StreamConfig {
                    channels: dec_ch,
                    sample_rate: dec_sr,
                    buffer_size: cpal::BufferSize::Default,
                };

                // Gate: output silence until enough real data has been buffered.
                // Prevents stale/garbage audio during track transitions.
                // Minimum: ~500ms of audio at the output sample rate.
                // (v0.8.97=20ms, v0.8.98=200ms — still too low for macOS
                // CoreAudio which can request 1024+ frame buffers.)
                let data_started = Arc::new(AtomicBool::new(false));

                // #3618 — la MÊME cascade que le chemin PCM, enfin branchée
                // ici. Un DAC bit-perfect qui refuse le flottant faisait
                // jusque-là échouer la première et unique tentative, et la
                // zone s'arrêtait sans cause affichée. Tout ce qui vient d'un
                // serveur multimédia arrive par cette branche.
                let tentatives = cascade_de_formats(&output_config, &source_config);
                let anneaux: std::cell::RefCell<Option<Arc<RingBuf>>> =
                    std::cell::RefCell::new(None);
                let ouverture = ouvrir_premier_format_accepte(&tentatives, |cfg, format| {
                    let cap = (cfg.sample_rate as usize) * (cfg.channels as usize) * 2;
                    let min_buf = (cfg.sample_rate as usize) * (cfg.channels as usize) / 2; // ~500ms
                    starvation.begin_stream(cfg.sample_rate, cfg.channels);
                    let r = Arc::new(RingBuf::new_metered(cap, starvation.clone()));
                    r.clear(); // Defensive: zero-fill before callback can read
                    data_started.store(false, Ordering::SeqCst);
                    let bati = match format {
                        FormatDeSortie::F32 => build_compressed_f32_stream(
                            &device,
                            cfg,
                            r.clone(),
                            volume.clone(),
                            paused.clone(),
                            force_silent.clone(),
                            data_started.clone(),
                            min_buf,
                            device_gone.clone(),
                            soft_mute.clone(),
                        ),
                        FormatDeSortie::I32 => build_int_stream::<i32>(
                            &device,
                            cfg,
                            r.clone(),
                            volume.clone(),
                            paused.clone(),
                            force_silent.clone(),
                            data_started.clone(),
                            min_buf,
                            device_gone.clone(),
                            soft_mute.clone(),
                        ),
                        FormatDeSortie::I16 => build_int_stream::<i16>(
                            &device,
                            cfg,
                            r.clone(),
                            volume.clone(),
                            paused.clone(),
                            force_silent.clone(),
                            data_started.clone(),
                            min_buf,
                            device_gone.clone(),
                            soft_mute.clone(),
                        ),
                    };
                    if bati.is_ok() {
                        *anneaux.borrow_mut() = Some(r);
                    }
                    bati
                });

                let (stream, actual_config, retenu) = match ouverture {
                    Ok(t) => t,
                    Err(echecs) => {
                        // Tous les formats ont été refusés : la faute est au
                        // périphérique, pas à l'encodage. Nommer la cause —
                        // et surtout RENSEIGNER `open_failure`, le canal que
                        // le sondeur draine à chaque tick. Sans lui la zone
                        // s'arrêtait en silence, sans cause à l'écran (#3618).
                        let premier = echecs
                            .first()
                            .map(|e| e.to_string())
                            .unwrap_or_else(|| "aucune tentative".to_string());
                        let cause = classify_open_failure(&premier);
                        // #3575 — quand cpal a DÉTRUIT le motif, aller chercher
                        // dans /proc qui tient le nœud PCM, au lieu d'attendre
                        // un `fuser -v /dev/snd/*` que personne ne tapera.
                        #[cfg(target_os = "linux")]
                        if cause == OpenFailure::IndisponibleMotifPerdu {
                            journaliser_les_teneurs_du_pcm(&pcm_ouvert, &device_name);
                        }
                        warn!(
                            device = %device_name,
                            tentatives = tentatives.len(),
                            first_error = %premier,
                            formats = %tentatives
                                .iter()
                                .map(|(_, f)| f.nom())
                                .collect::<Vec<_>>()
                                .join(","),
                            hint = %cause.log_hint(),
                            "audio_stream_build_failed_compressed"
                        );
                        if let Ok(mut slot) = open_failure.lock() {
                            *slot = Some(format!(
                                "Sortie « {device_name} » : {}.",
                                cause.user_message()
                            ));
                        }
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                };
                let ring = anneaux
                    .into_inner()
                    .expect("une ouverture réussie a toujours posé son anneau");
                if retenu != FormatDeSortie::F32 {
                    info!(
                        format = retenu.nom(),
                        sample_rate = actual_config.sample_rate,
                        "local_audio_fallback_to_integer_format"
                    );
                }
                let output_sr = actual_config.sample_rate;
                let output_ch = actual_config.channels;

                info!(
                    device = %device_name,
                    dec_sr,
                    dec_ch,
                    output_sr,
                    output_ch,
                    samples = decoded_len,
                    "local_audio_compressed_playing"
                );

                // Chaine DSP de la zone : egaliseur, correction de piece,
                // crossfeed.
                //
                // Ce chemin — flux compresse decode en bloc — ne l'appelait
                // PAS. Les trois sites d'`apply_local_dsp` etaient tous sur le
                // chemin PCM : sur un flux non-WAV, l'egaliseur, le convolveur
                // et le crossfeed n'agissaient nulle part (#1725, quatrieme
                // trou de la meme famille que #1216, #1168 et Diretta).
                //
                // AVANT l'adaptation de canaux et le reechantillonnage, et
                // c'est deliberé : l'orchestrateur construit l'`EqProcessor`
                // pour le couple (`media.sample_rate`, `media.channels`) —
                // c'est-a-dire (`dec_sr`, `dec_ch`). Appliquer apres coup des
                // biquads calcules pour 44,1 kHz a un flux ramene a 48 kHz
                // deplacerait toutes les frequences de coupure.
                let mut samples = decoded_samples;
                //
                // `dop = false` : le DoP voyage dans un conteneur PCM, donc un
                // flux DoP arrive en WAV et prend l'autre chemin. Ici les
                // echantillons sortent d'un decodeur (FLAC, MP3, AAC) sous
                // forme de f32 — `is_dop_pcm`, qui inspecte des octets PCM
                // bruts, n'a rien a y examiner.
                current_format.store(LocalOutput::pack_format(dec_sr, dec_ch), Ordering::Relaxed);
                match rebuild_local_convolver(&convolver_config, &convolver, dec_sr, dec_ch) {
                    Ok(true) => info!(
                        sample_rate = dec_sr,
                        channels = dec_ch,
                        "local_convolver_built_for_stream"
                    ),
                    Ok(false) => {}
                    Err(error) => warn!(
                        sample_rate = dec_sr,
                        channels = dec_ch,
                        error = %error,
                        "local_convolver_format_rejected"
                    ),
                }
                apply_local_dsp(
                    &mut samples,
                    &eq,
                    &convolver,
                    &crossfeed,
                    &pure_bypass,
                    &mono_downmix,
                    dec_ch,
                    false,
                );

                // Chemin d'un seul tenant : toute la piste vient de traverser le
                // DSP, la queue du convolveur peut donc etre ajoutee ici — elle
                // suivra la meme adaptation de canaux et le meme
                // reechantillonnage que le reste (#2209).
                let queue = flush_local_dsp(
                    &convolver,
                    &crossfeed,
                    &pure_bypass,
                    &mono_downmix,
                    dec_ch,
                    false,
                );
                samples.extend_from_slice(&queue);

                // Adapt channels and resample if needed (using rubato
                // sinc resampler for high-quality rate conversion)
                if dec_ch != output_ch {
                    samples = adapt_channels(&samples, dec_ch, output_ch);
                }
                //
                // Piste entiere en memoire : `rubato_resample_track` retire le
                // delai de groupe du sinc et rend exactement
                // `round(trames × ratio)`. La variante en flux le conservait,
                // et la duree/position calculees juste en dessous heritaient du
                // surplus a CHAQUE piste (#2246).
                if dec_sr != output_sr {
                    samples = rubato_resample_track(&samples, dec_sr, output_sr, output_ch);
                }

                // Pre-fill the ring buffer before starting the cpal stream.
                // For compressed streams all data is already decoded, so we
                // push as much as fits (~200ms or more) before calling play().
                let prefill_target = (output_sr as usize) * (output_ch as usize) / 5; // ~200ms
                let prefill_count = samples.len().min(prefill_target.max(ring.capacity() / 2));
                let initial_written = ring.push(&samples[..prefill_count]);

                if let Err(e) = stream.play() {
                    warn!(error = %e, "audio_stream_play_failed");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
                info!(
                    demarrage_ms = chrono_demarrage.elapsed().as_millis() as u64,

                    device = %device_name,
                    prefill_samples = initial_written,
                    "local_audio_compressed_playing_after_prefill"
                );

                // Feed remaining samples to ring buffer, updating position
                // progressively so the seek bar advances during playback.
                let total_output_samples = samples.len() as u64;
                let output_frames = total_output_samples / output_ch as u64;
                let output_duration_ms = (output_frames as f64 / output_sr as f64 * 1000.0) as u64;
                let mut fed_samples = initial_written as u64;

                if initial_written < samples.len() {
                    let chunk_size = (output_sr as usize) * (output_ch as usize) / 5; // ~200ms chunks
                    let remaining = &samples[initial_written..];
                    for chunk in remaining.chunks(chunk_size) {
                        if stop_rx.try_recv().is_ok() || force_silent.load(Ordering::Relaxed) {
                            break;
                        }
                        while paused.load(Ordering::Relaxed)
                            && !force_silent.load(Ordering::Relaxed)
                        {
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                        let fed = feed_ring_abortable(
                            &ring,
                            chunk,
                            &stop_rx,
                            &paused,
                            Some(&force_silent),
                        );
                        if !fed || device_gone.load(Ordering::Relaxed) {
                            // Consumer dead (USB DAC unplugged, #1626): stop
                            // feeding instead of stalling 5s on every chunk.
                            warn!(
                                device = %device_name,
                                "local_audio_compressed_feed_aborted_device_lost"
                            );
                            break;
                        }
                        fed_samples += chunk.len() as u64;
                        let fed_frames = fed_samples / output_ch as u64;
                        let pos =
                            (fed_frames as f64 / output_sr as f64 * 1000.0) as u64 + seek_offset;
                        position_ms
                            .store(pos.min(output_duration_ms + seek_offset), Ordering::Relaxed);
                    }
                }

                position_ms.store(output_duration_ms + seek_offset, Ordering::Relaxed);

                // Signal natural track end BEFORE draining so the
                // orchestrator can detect end-of-track even if a new play
                // command sets force_silent while the ring buffer is still
                // being consumed (e.g. resampling 44.1→192 kHz).
                // play_url() clears this flag for the next track.
                track_ended_naturally.store(true, Ordering::SeqCst);
                track_ended_generation.store(my_generation, Ordering::SeqCst);
                TRACK_END_NOTIFY.notify_one();

                // Wait for ring buffer to drain — but NEVER block forever: if
                // the render callback is dead (USB DAC unplugged, #1626) the
                // ring stays full and this loop used to spin until restart.
                // Deadline = queued audio duration + 5s margin, mirroring the
                // asio_drain_timeout guard of the exclusive path.
                let drain_deadline =
                    drain_deadline_for(ring.available(), output_sr as u64, output_ch as u64);
                let drain_started = std::time::Instant::now();
                loop {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    if force_silent.load(Ordering::Relaxed) {
                        break;
                    }
                    if ring.available() == 0 {
                        break;
                    }
                    if device_gone.load(Ordering::Relaxed)
                        || drain_started.elapsed() >= drain_deadline
                    {
                        warn!(
                            device = %device_name,
                            remaining_samples = ring.available(),
                            device_gone = device_gone.load(Ordering::Relaxed),
                            "local_audio_compressed_drain_timeout"
                        );
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }

                drop(stream);
                if play_generation.load(Ordering::SeqCst) == my_generation {
                    playing.store(false, Ordering::SeqCst);
                }
                info!(device = %device_name, "local_audio_compressed_stopped");
                return;
            };

            // Format definitif du flux PCM : c'est CE couple que voit
            // `apply_local_dsp`, donc celui auquel un EqProcessor doit etre
            // bati. Memorise ici pour qu'un profil modifie EN COURS de lecture
            // puisse etre applique tout de suite, au lieu d'attendre la piste
            // suivante (#1725). La branche compressee est sortie en `return`
            // juste au-dessus : elle ne passe pas par le DSP.
            current_format.store(
                LocalOutput::pack_format(sample_rate, channels),
                Ordering::Relaxed,
            );
            match rebuild_local_convolver(&convolver_config, &convolver, sample_rate, channels) {
                Ok(true) => info!(sample_rate, channels, "local_convolver_built_for_stream"),
                Ok(false) => {}
                Err(error) => warn!(
                    sample_rate,
                    channels,
                    error = %error,
                    "local_convolver_format_rejected"
                ),
            }

            // bit_depth == 0 is the sentinel for IEEE float 32-bit (4 bytes)
            let bytes_per_sample = if bit_depth == 0 {
                4
            } else {
                (bit_depth / 8) as usize
            };
            let frame_bytes = channels as usize * bytes_per_sample;

            // Le format source devient un TYPE, ici et une seule fois — AVANT
            // les branches de plateforme, parce que les quatre chemins en ont
            // besoin. Le poser plus bas, dans le seul chemin cpal partagé,
            // laissait le bras exclusif macOS passer des nombres nus : la
            // compilation macOS l'a dit au premier essai, et elle avait raison.
            //
            // Le refus est inatteignable en pratique — `parse_wav_header` ne
            // rend que 0, 16, 24 ou 32 bits, et un conteneur nul le fait déjà
            // échouer — mais il remplace deux fins de partie bien pires :
            // `bit_depth / 8` sur une profondeur inconnue rendait un nombre
            // d'octets faux (bruit blanc), et zéro canal faisait DIVISER PAR
            // ZÉRO le calcul d'alignement, qui abattait le fil de lecture.
            let Some(spec) = AudioSpec::depuis_entete(sample_rate, bit_depth, channels) else {
                warn!(
                    device = %device_name,
                    sample_rate,
                    bit_depth,
                    channels,
                    "local_audio_unsupported_source_format"
                );
                if let Ok(mut slot) = open_failure.lock() {
                    // #3270 : un `return` nu laisse la zone s'arrêter sans que
                    // l'écran apprenne jamais pourquoi.
                    *slot = Some(format!(
                        "« {device_name} » : ce flux annonce un format que Tune ne sait pas \
                         lire ({bit_depth} bits, {channels} canaux)."
                    ));
                }
                force_silent.store(true, Ordering::SeqCst);
                playing.store(false, Ordering::SeqCst);
                return;
            };

            // ------- Exclusive mode path (macOS only) -------
            #[cfg(target_os = "macos")]
            if exclusive_mode {
                // R6 bis (#2219) : le bras vit dans `local/bras_coreaudio.rs`.
                // Tout ce qu'il lisait ici lui est DÉPLACÉ — il est terminal.
                bras_coreaudio::jouer_via_coreaudio(bras_coreaudio::EntreesCoreAudio {
                    device_name,
                    url,
                    sample_rate,
                    bit_depth,
                    channels,
                    data_offset,
                    header_buf,
                    reader,
                    frame_bytes,
                    spec,
                    seek_offset,
                    my_generation,
                    starvation,
                    volume,
                    user_volume_ref,
                    rg_factor_ref,
                    paused,
                    playing,
                    force_silent,
                    stop_rx,
                    open_failure,
                    position_ms,
                    play_generation,
                    track_ended_naturally,
                    track_ended_generation,
                    eq,
                    convolver,
                    crossfeed,
                    pure_bypass,
                    mono_downmix,
                    dop_active,
                });
                return;
            }

            // ------- Exclusive mode path (Windows ASIO) -------
            #[cfg(all(target_os = "windows", feature = "asio"))]
            if exclusive_mode && audio_backend == "asio" {
                // R6 bis (#2219) : le bras vit dans `local/bras_asio.rs`. Tout
                // ce qu'il lisait ici lui est DÉPLACÉ — il est terminal.
                bras_asio::jouer_via_asio(bras_asio::EntreesAsio {
                    device_name,
                    url,
                    sample_rate,
                    bit_depth,
                    channels,
                    data_offset,
                    header_buf,
                    reader,
                    frame_bytes,
                    bytes_per_sample,
                    seek_offset,
                    pre_seeked,
                    my_generation,
                    starvation,
                    volume,
                    user_volume_ref,
                    rg_factor_ref,
                    paused,
                    playing,
                    force_silent,
                    stop_rx,
                    open_failure,
                    signal_path_status,
                    position_ms,
                    play_generation,
                    track_ended_naturally,
                    track_ended_generation,
                    eq,
                    convolver,
                    crossfeed,
                    pure_bypass,
                    mono_downmix,
                    dop_active,
                });
                return;
            }

            // ------- WASAPI Exclusive mode path (Windows, non-ASIO) -------
            #[cfg(target_os = "windows")]
            if exclusive_mode && audio_backend != "asio" {
                // R6 bis (#2219) : le bras vit dans `local/bras_wasapi.rs`.
                // Tout ce qu'il lisait ici lui est DÉPLACÉ — il est terminal.
                bras_wasapi::jouer_via_wasapi(bras_wasapi::EntreesWasapi {
                    device_name,
                    endpoint_id,
                    sample_rate,
                    bit_depth,
                    channels,
                    data_offset,
                    header_buf,
                    reader,
                    frame_bytes,
                    seek_offset,
                    my_generation,
                    starvation,
                    volume,
                    user_volume_ref,
                    rg_factor_ref,
                    paused,
                    playing,
                    force_silent,
                    stop_rx,
                    open_failure,
                    signal_path_status,
                    position_ms,
                    play_generation,
                    track_ended_naturally,
                    track_ended_generation,
                    eq,
                    convolver,
                    crossfeed,
                    pure_bypass,
                    mono_downmix,
                    dop_active,
                });
                return;
            }
            // ------- Open cpal device (shared mode) -------
            //
            // R8 (#2219) : la résolution du périphérique, la décision de
            // cadence et la cascade f32 → i32 → i16 vivent dans
            // `BackendCpal::ouvrir` (`local/backend.rs`). `play_url` ne crée
            // plus d'anneau et n'en passe plus d'`Arc` (D2) : il reçoit un
            // backend, lui demande son format, son puits et une observation.
            let demande = DemandeDOuverture {
                spec,
                device_name: &device_name,
                endpoint_id: endpoint_id.as_deref(),
                origin_host: origin_host.as_deref(),
                audio_backend: &audio_backend,
                exclusive: exclusive_mode,
                stop_rx: &stop_rx,
                paused: &paused,
                force_silent: &force_silent,
                volume: &volume,
                device_gone: &device_gone,
                starvation: &starvation,
                soft_mute: soft_mute.clone(),
                position_ms: &position_ms,
            };
            let mut backend = match BackendCpal::ouvrir(&demande) {
                Ok(backend) => backend,
                Err(refus) => {
                    refus.rapporter(&device_name, &open_failure);
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
            };
            let sortie = backend.format_ouvert();

            let output_sr = sortie.cadence;
            let output_ch = sortie.canaux;

            info!(
                device = %device_name,
                input_sr = sample_rate,
                input_bd = bit_depth,
                input_ch = channels,
                output_sr,
                output_ch,
                "local_audio_stream_config"
            );

            // DO NOT call stream.play() yet — we pre-fill the ring buffer
            // first to prevent CoreAudio from pulling uninitialized/empty
            // buffers in the first few callbacks.  The stream is started
            // after enough data has been buffered (~200ms).

            // ------- Feed audio data from HTTP stream to ring buffer -------
            let pcm_data = if data_offset < header_buf.len() {
                header_buf[data_offset..].to_vec()
            } else {
                Vec::new()
            };

            debug!(
                pcm_data_from_header = pcm_data.len(),
                header_buf_len = header_buf.len(),
                data_offset,
                "local_audio_initial_pcm_data"
            );

            let mut total_frames_fed: u64 = 0;
            let skip_bytes: u64 = if seek_offset > 0 && !pre_seeked {
                let skip_frames = (seek_offset as f64 / 1000.0 * sample_rate as f64) as u64;
                skip_frames * channels as u64 * bytes_per_sample as u64
            } else {
                0
            };
            let skipped_bytes: u64 = 0;
            let needs_resample = output_sr != sample_rate;
            // #3233 — Pierre M, fil 1043 : « DSD : le temps défile, pas de
            // son ». Un porteur DoP ne survit ni au sinc ni à l'adaptation de
            // canaux : le marqueur 0x05/0xFA alterne à CHAQUE trame, c'est un
            // carré à fs/2 (88,2 kHz pour un DoP DSD64) que le filtre annihile
            // (`audio::dsd_to_dop::DopRuptureChemin`). Les bras EXCLUSIFS
            // refusent déjà ce cas avant qu'un échantillon parte au DAC
            // (`WindowsExclusivePcmError::DopUnsupported`) ; le chemin partagé,
            // lui, le détruisait en silence. Depuis #3252 la branche
            // `ResampleToDeviceRate` est réellement prise sur WASAPI — le cas
            // est donc devenu ATTEIGNABLE, et il faut le nommer plutôt que de
            // servir au DAC un signal dont il ne reste rien.
            let mut refuser_le_porteur_dop = |dop: bool, src_sr: u32, src_ch: u16| -> bool {
                let Some(rupture) = crate::audio::dsd_to_dop::rupture_du_porteur_dop(
                    dop, src_sr, output_sr, src_ch, output_ch,
                ) else {
                    return false;
                };
                rupture.journaliser(&device_name);
                if let Ok(mut slot) = open_failure.lock() {
                    *slot = Some(rupture.message_utilisateur(&device_name));
                }
                force_silent.store(true, Ordering::SeqCst);
                dop_active.store(false, Ordering::SeqCst);
                sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, false);
                true
            };

            // Create rubato sinc resampler once for the entire track.
            // Using FixedAsync::Input so we feed fixed-size input chunks.
            let resampler: Option<Async<f32>> = if needs_resample {
                let ratio = output_sr as f64 / sample_rate as f64;
                // Adaptive resampler params based on conversion ratio:
                //   ratio ≤ 2.0 (e.g. 96kHz→48kHz): quality params, plenty of CPU budget
                //   ratio > 2.0 (e.g. 176.4kHz→48kHz, 192kHz→48kHz): lighter params
                //     to avoid real-time stuttering on Windows (still ~90dB SNR)
                let inv_ratio = 1.0 / ratio; // > 1.0 when downsampling
                let (sinc_len, oversampling_factor) = if inv_ratio > 2.0 {
                    (32_usize, 64_usize) // lighter: 176.4/192kHz → 48kHz
                } else {
                    (64_usize, 128_usize) // standard: 96kHz → 48kHz
                };
                let window = WindowFunction::BlackmanHarris2;
                let f_cutoff = calculate_cutoff(sinc_len, window);
                let params = SincInterpolationParameters {
                    sinc_len,
                    f_cutoff,
                    interpolation: SincInterpolationType::Linear,
                    oversampling_factor,
                    window,
                };
                info!(
                    from_sr = sample_rate,
                    to_sr = output_sr,
                    sinc_len,
                    oversampling_factor,
                    "rubato_resampler_adaptive_params"
                );
                match Async::<f32>::new_sinc(
                    ratio,
                    1.1,
                    &params,
                    1024,
                    output_ch as usize,
                    FixedAsync::Input,
                ) {
                    Ok(r) => {
                        info!(
                            from_sr = sample_rate,
                            to_sr = output_sr,
                            "rubato_resampler_created"
                        );
                        Some(r)
                    }
                    Err(e) => {
                        warn!(error = %e, "rubato_resampler_creation_failed");
                        None
                    }
                }
            } else {
                None
            };
            // Read and feed the rest of the stream
            let mut read_buf = vec![0u8; 65536];

            // ── La frontière producteur → puits ────────────────────────────
            //
            // `etage` tient tout ce qui transforme des octets source en mots de
            // sortie ; `puits` tient l'anneau et rien d'autre. Tout ce qui part
            // au DAC par ce chemin passe par `etage.pousser(&mut *puits, …)` —
            // il n'existe plus d'autre route.
            //
            // Le tampon d'attente est amorcé avec le reliquat non aligné de la
            // lecture d'en-tête : sans lui, chaque mot 24 bits suivant serait lu
            // au mauvais décalage d'octet (bruit blanc).
            let mut etage = EtageDeConversion {
                pcm: LocalPcmProcessor {
                    eq: &eq,
                    convolver: &convolver,
                    crossfeed: &crossfeed,
                    pure_bypass: &pure_bypass,
                    mono_downmix: &mono_downmix,
                    dop_active: &dop_active,
                    volume: &volume,
                    user_volume: &user_volume_ref,
                    rg_factor: &rg_factor_ref,
                },
                en_attente: pcm_data,
                resampler,
                // Reliquat du rééchantillonneur : les échantillons qui ne
                // remplissent pas un bloc complet, reportés sur la lecture
                // suivante.
                resample_leftover: Vec::new(),
                pcm_kind: LocalPcmKind::for_bit_depth(bit_depth),
                spec,
                sortie: FormatOuvert::new(output_sr, output_ch),
                needs_resample,
            };
            // R8 : le puits vient du backend (D1). CPAL partagé n'en fournit
            // qu'un, flottant — c'est le chemin DSP, le mot y est `f32` par
            // construction ; un puits natif ici n'est pas une erreur à
            // rapporter mais une impossibilité de type.
            let mut puits = match backend.puits() {
                Puits::Flottant(puits) => puits,
                Puits::Natif(_) => unreachable!("BackendCpal ne fournit qu'un puits flottant"),
            };

            // Process leftover from header read
            let amorce = etage.pousser(
                &mut *puits,
                &mut refuser_le_porteur_dop,
                &mut |bloc: &ProcessedLocalPcm| {
                    // Diagnostic: log first few f32 samples and detect
                    // anomalies. White noise manifests as high-amplitude random
                    // values in what should be a gentle attack.
                    if !bloc.samples.is_empty() {
                        let first_8: Vec<f32> = bloc.samples.iter().take(8).copied().collect();
                        let max_abs = bloc
                            .samples
                            .iter()
                            .take(200)
                            .fold(0.0f32, |m, &s| m.max(s.abs()));
                        let non_zero = bloc.samples.iter().take(200).filter(|&&s| s != 0.0).count();
                        info!(
                            first_samples = ?first_8,
                            max_abs_200 = max_abs,
                            non_zero_in_200 = non_zero,
                            total_samples = bloc.samples.len(),
                            bit_depth,
                            frame_bytes,
                            dop = bloc.dop,
                            "local_audio_initial_samples_diagnostic"
                        );
                    }
                },
            );
            match amorce {
                PousseeVersLePuits::PorteurDopRefuse => {
                    if play_generation.load(Ordering::SeqCst) == my_generation {
                        playing.store(false, Ordering::SeqCst);
                    }
                    return;
                }
                // L'amorce comptait ses trames SANS regarder le verdict de
                // l'écriture — un puits déjà mort à l'amorçage est constaté par
                // la boucle, pas ici. Conservé tel quel : la position rapportée
                // pour ce bloc ne doit pas changer.
                PousseeVersLePuits::Poussee { trames_source }
                | PousseeVersLePuits::PuitsMort { trames_source } => {
                    total_frames_fed += trames_source;
                }
                PousseeVersLePuits::RienAPousser => {}
            }
            // ── #3318 — LA CLÉ QUI MANQUAIT ────────────────────────────
            //
            // `local_audio_slow_read` et `local_audio_read_error` ne portaient
            // que des octets et des millisecondes : ni appareil, ni flux. Or
            // la ligne symétrique côté producteur — `stream_delivery_stall`
            // (`tune-stream-http/src/lib.rs`) — porte, elle, `stream_id`.
            //
            // Sans identifiant commun, la seule jointure possible entre « la
            // sortie a attendu » et « le flux interne n'a rien servi » était
            // l'HORODATAGE, et elle n'est valide que si une SEULE zone joue
            // pendant la fenêtre. Sur la machine de #3318 — un seul cœur,
            // 49 618 pistes, plusieurs sorties énumérées — ce n'est pas une
            // hypothèse qu'on puisse tenir, et c'est exactement pour ça que
            // le dossier était bloqué : les deux moitiés de la mesure
            // existaient et ne se joignaient pas.
            //
            // L'identifiant n'est pas à inventer : il est déjà dans l'URL que
            // ce fil est en train de tirer (`…/stream/<id>.<ext>`).
            // `stream_id_de_l_uri` est la découpe du serveur de flux
            // elle-même, appelée et non recopiée — le jour où la convention
            // change, les deux bougent ensemble.
            let cle_de_flux = crate::poller::decisions::stream_id_de_l_uri(Some(&url));
            let cle_de_flux = cle_de_flux.as_deref();

            let mut total_bytes_read: u64 = 0;
            let first_data_logged = false;
            let stream_start = std::time::Instant::now();

            // Pre-fill the ring buffer before starting the cpal stream.
            // Target: ~500ms of audio so the first callback has enough data.
            let prefill_target = (output_sr as usize) * (output_ch as usize) / 5; // ~200ms
            let mut stream_started = false;

            // Check if initial header data was enough to meet the prefill target
            if backend.observer().disponible >= prefill_target {
                if let Err(e) = backend.demarrer() {
                    warn!(error = %e, "audio_stream_play_failed");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
                stream_started = true;
                info!(
                    demarrage_ms = chrono_demarrage.elapsed().as_millis() as u64,

                    device = %device_name,
                    prefill_samples = backend.observer().disponible,
                    "local_audio_playing_after_prefill"
                );
            }

            // Tracks whether the HTTP read loop exited because the source
            // reached EOF (true) vs. a stop signal or read error (false).
            // Only when http_eof=true do we signal track_ended_naturally.
            let mut http_eof = false;

            // ── La boucle producteur, une seule pour les deux pistes ───────
            //
            // Elle ne sait rien de cpal : elle lit un `Read`, décode par
            // `etage` et écrit dans `puits`. La piste enchaînée en gapless,
            // plus bas, appelle EXACTEMENT la même boucle.
            let producteur = BoucleProducteur {
                role: RoleDeLaBoucle::PisteInitiale,
                device_name: &device_name,
                cle_de_flux,
                stop_rx: &stop_rx,
                force_silent: force_silent.as_ref(),
                device_gone: device_gone.as_ref(),
                position_ms: position_ms.as_ref(),
                open_failure: open_failure.as_ref(),
                debut_du_flux: stream_start,
            };
            let mut compteurs = CompteursDePiste {
                total_bytes_read,
                total_frames_fed,
                seek_offset,
                skip_bytes,
                skipped_bytes,
                premiere_donnee_journalisee: first_data_logged,
            };
            let fin = producteur.tourner(
                &mut reader,
                &mut read_buf,
                &mut etage,
                &mut *puits,
                &mut refuser_le_porteur_dop,
                &mut compteurs,
                &mut |compteurs| {
                    // Démarrer le flux cpal une fois le pré-remplissage atteint.
                    // Le périphérique ne tire ainsi jamais d'un anneau vide ou
                    // clairsemé — c'était le bruit blanc en début de piste.
                    if stream_started || backend.observer().disponible < prefill_target {
                        return true;
                    }
                    if let Err(e) = backend.demarrer() {
                        warn!(error = %e, "audio_stream_play_failed");
                        playing.store(false, Ordering::SeqCst);
                        return false;
                    }
                    stream_started = true;
                    info!(
                    demarrage_ms = chrono_demarrage.elapsed().as_millis() as u64,

                        device = %device_name,
                        prefill_samples = backend.observer().disponible,
                        total_bytes_read = compteurs.total_bytes_read,
                        elapsed_ms = stream_start.elapsed().as_millis() as u64,
                        "local_audio_playing_after_prefill"
                    );
                    true
                },
            );
            total_bytes_read = compteurs.total_bytes_read;
            total_frames_fed = compteurs.total_frames_fed;
            match fin {
                FinDeBoucle::FinDeFlux => http_eof = true,
                FinDeBoucle::Interrompue => {}
                FinDeBoucle::PorteurDopRefuse => {
                    if play_generation.load(Ordering::SeqCst) == my_generation {
                        playing.store(false, Ordering::SeqCst);
                    }
                    return;
                }
                FinDeBoucle::Abandon => return,
            }

            if http_eof {
                report_incomplete_local_pcm_probe(etage.pcm_kind, etage.en_attente.len());
            }

            // If the stream was never started (very short track or error),
            // start it now with whatever data we have.
            if !stream_started {
                // Empty stream: the source delivered zero audio bytes (a
                // superseded/aborted start — e.g. a rapid re-trigger of the same
                // track, seen in Philippe Vella's log as two orchestrator_play
                // ~330 ms apart). Starting the cpal stream on an empty ring
                // played audible silence while the transport kept advancing the
                // progress bar ("le son coupe, la barre continue"). Bail instead
                // so the orchestrator sees the track did not actually play,
                // rather than a phantom "playing" state on a silent output.
                if total_bytes_read == 0 {
                    warn!(
                        device = %device_name,
                        "local_audio_empty_stream_no_playback"
                    );
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
                if let Err(e) = backend.demarrer() {
                    warn!(error = %e, "audio_stream_play_failed_final");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
                info!(
                    device = %device_name,
                    ring_available = backend.observer().disponible,
                    "local_audio_playing_short_track_or_eof"
                );
            }

            // ---------------------------------------------------------------
            // Gapless continuation: when the current track reached clean EOF
            // and a next track was staged via set_next_media(), seamlessly
            // chain into the next track without closing the cpal stream.
            // The audio device stays open — zero gap between tracks.
            // ---------------------------------------------------------------
            while http_eof
                && !force_silent.load(Ordering::Relaxed)
                && !device_gone.load(Ordering::Relaxed)
            {
                let pending = next_media_ref.lock().unwrap().take();
                let Some(next) = pending else { break };

                track_ended_naturally.store(false, Ordering::SeqCst);
                track_ended_generation.store(0, Ordering::SeqCst);

                info!(
                    next_title = ?next.title,
                    next_url = %next.url,
                    "local_audio_gapless_chaining_next_track"
                );

                // La bascule des métadonnées et de la position a lieu PLUS BAS,
                // une fois le flux suivant confirmé chaînable (en-tête WAV lu).
                // Elle était faite ici, avant même la requête HTTP : sur un
                // enchaînement qui échouait ensuite — flux non-WAV, HTTP en
                // erreur, en-tête vide —, la sortie annonçait déjà « position 0
                // du morceau SUIVANT » alors qu'elle allait s'arrêter. Le
                // poller y lisait un `position_reset` de manuel (fin de piste →
                // 0, gapless armé) et déclenchait l'avance métadonnées seule,
                // qui n'envoie AUCUN `play` (#1919). Au passage, `position_ms`
                // remis à 0 devenait le `fed_position_ms` du drainage, qui
                // rapportait donc 0 au lieu de la fin du morceau.

                // Fetch the next track's HTTP stream
                let next_response = match crate::http::client::blocking_builder()
                    .timeout(None)
                    .connect_timeout(std::time::Duration::from_secs(10))
                    .build()
                    .and_then(|client| client.get(&next.url).send())
                {
                    Ok(r) if r.status().is_success() || r.status().as_u16() == 206 => r,
                    Ok(r) => {
                        warn!(
                            status = %r.status(),
                            url = %next.url,
                            "local_audio_gapless_http_error"
                        );
                        break;
                    }
                    Err(e) => {
                        warn!(
                            error = %e,
                            url = %next.url,
                            "local_audio_gapless_http_fetch_failed"
                        );
                        break;
                    }
                };

                // Read header bytes from the next track.
                // The next track's transcode session may have only just
                // been started, so its very first read can time out before
                // the WAV header is available. Retry on TimedOut/WouldBlock —
                // mirroring the initial-track header read above — instead of
                // aborting the gapless chain, which would skip the track.
                let mut next_reader = next_response;
                let mut next_header = vec![0u8; 4096];
                let nh_read = loop {
                    if force_silent.load(Ordering::Relaxed) {
                        break 0;
                    }
                    match next_reader.read(&mut next_header) {
                        Ok(n) => break n,
                        Err(ref e)
                            if e.kind() == std::io::ErrorKind::TimedOut
                                || e.kind() == std::io::ErrorKind::WouldBlock =>
                        {
                            // Stream not ready yet — wait for the producer.
                            continue;
                        }
                        Err(e) => {
                            warn!(error = %e, "local_audio_gapless_header_read_failed");
                            break 0;
                        }
                    }
                };
                if nh_read == 0 {
                    warn!("local_audio_gapless_header_read_empty");
                    break;
                }
                next_header.truncate(nh_read);

                // Parse the WAV header of the next track
                let Some((new_ch, new_sr, new_bd, new_data_offset)) =
                    parse_wav_header(&next_header)
                else {
                    // Not a WAV stream — cannot chain gaplessly.
                    // Fall through to normal end-of-track handling.
                    info!("local_audio_gapless_next_not_wav_falling_back");
                    break;
                };

                // Le format de la piste enchaînée devient un TYPE avant qu'une
                // seule de ses trames ne soit décodée, et AVANT que la queue du
                // DSP ne soit rendue : un refus ici laisse la piste courante se
                // terminer proprement, exactement comme un en-tête illisible.
                // Inatteignable en pratique — `parse_wav_header` ne rend que 0,
                // 16, 24 ou 32 bits et jamais zéro canal.
                let Some(nouvelle_spec) = AudioSpec::depuis_entete(new_sr, new_bd, new_ch) else {
                    info!("local_audio_gapless_next_not_wav_falling_back");
                    break;
                };

                info!(
                    new_sr,
                    new_ch,
                    new_bd,
                    prev_sr = etage.sample_rate(),
                    prev_ch = etage.channels(),
                    prev_bd = etage.bit_depth(),
                    "local_audio_gapless_next_track_format"
                );

                let prev_sr = etage.sample_rate();
                let prev_ch = etage.channels();
                let prev_needs_resample = etage.needs_resample;
                let next_needs_resample = output_sr != new_sr;
                let convolver_format_changed = new_sr != prev_sr || new_ch != prev_ch;

                // Un moteur FFT est lié au format source. Avant de le remplacer,
                // rendre sa queue dans l'ANCIEN format et lui faire suivre la
                // même adaptation/rééchantillonnage que la piste qui se termine.
                // À format identique on ne touche à rien : son état fait partie
                // de la continuité gapless.
                //
                // `etage` porte encore le format de la piste QUI SE TERMINE —
                // il n'est mis à jour que plus bas : `rendre_la_queue_du_dsp`
                // tire la queue avec les canaux de cette piste-là et lui
                // applique exactement son adaptation et son rééchantillonnage.
                if convolver_format_changed {
                    etage.rendre_la_queue_du_dsp(&mut *puits);
                }

                // À cadence source identique, le resampler fait partie du flux
                // continu : conserver son état et son leftover est nécessaire
                // au vrai gapless. On ne le draine que si le prochain flux
                // impose réellement une autre cadence (ou n'en a plus besoin).
                // Le vidage doit avoir lieu APRÈS validation de l'en-tête : le
                // faire dès qu'un `next_media` existe insérait du silence même
                // quand la requête suivante échouait.
                if prev_needs_resample && (new_sr != prev_sr || !next_needs_resample) {
                    etage.vider(&mut *puits);
                }

                // Update source format variables for the new track.
                // Le format source vit dans `etage` : c'est lui, et lui seul,
                // que la boucle producteur consulte pour décoder et convertir.
                // R5 : UNE affectation là où il y en avait SIX. Les trois
                // étiquettes ne se posent plus qu'ensemble, les octets par
                // trame en découlent, et l'adaptation de canaux se déduit de la
                // comparaison avec le format ouvert. Il n'y a plus de
                // quatrième ligne à oublier.
                etage.spec = nouvelle_spec;
                etage.needs_resample = next_needs_resample;
                etage.pcm_kind = LocalPcmKind::for_bit_depth(new_bd);
                let sample_rate = etage.sample_rate();
                let channels = etage.channels();
                current_format.store(
                    LocalOutput::pack_format(sample_rate, channels),
                    Ordering::Relaxed,
                );
                if convolver_format_changed {
                    match rebuild_local_convolver(
                        &convolver_config,
                        &convolver,
                        sample_rate,
                        channels,
                    ) {
                        Ok(true) => info!(
                            sample_rate,
                            channels, "local_convolver_rebuilt_for_gapless_stream"
                        ),
                        Ok(false) => {}
                        Err(error) => warn!(
                            sample_rate,
                            channels,
                            error = %error,
                            "local_convolver_gapless_format_rejected"
                        ),
                    }
                }

                // Recreate the resampler if the source sample rate changed
                if etage.needs_resample && new_sr != prev_sr {
                    // Sample rate changed — flush old resampler residuals
                    etage.resample_leftover.clear();
                    let ratio = output_sr as f64 / new_sr as f64;
                    let inv_ratio = 1.0 / ratio;
                    let (sinc_len, oversampling_factor) = if inv_ratio > 2.0 {
                        (32_usize, 64_usize)
                    } else {
                        (64_usize, 128_usize)
                    };
                    let window = WindowFunction::BlackmanHarris2;
                    let f_cutoff = calculate_cutoff(sinc_len, window);
                    let params = SincInterpolationParameters {
                        sinc_len,
                        f_cutoff,
                        interpolation: SincInterpolationType::Linear,
                        oversampling_factor,
                        window,
                    };
                    etage.resampler = match Async::<f32>::new_sinc(
                        ratio,
                        1.1,
                        &params,
                        1024,
                        output_ch as usize,
                        FixedAsync::Input,
                    ) {
                        Ok(r) => {
                            info!(
                                from_sr = new_sr,
                                to_sr = output_sr,
                                "local_audio_gapless_resampler_recreated"
                            );
                            Some(r)
                        }
                        Err(e) => {
                            warn!(error = %e, "local_audio_gapless_resampler_failed");
                            etage.needs_resample = false;
                            None
                        }
                    };
                    etage.resample_leftover.clear();
                } else if !etage.needs_resample && etage.resampler.is_some() {
                    etage.resampler = None;
                    etage.resample_leftover.clear();
                }

                // L'enchaînement est acquis : le flux suivant répond et porte un
                // en-tête WAV lisible. C'est seulement MAINTENANT qu'on publie
                // le morceau suivant — avant cette ligne, tout `break` laisse la
                // sortie décrire honnêtement le morceau qui vient de finir, et
                // le poller prend le chemin de fin naturelle (un vrai
                // `play_from_queue`) au lieu de l'avance métadonnées muette.
                *uri_ref.lock().unwrap() = Some(next.url.clone());
                *title_ref.lock().unwrap() = next.title.clone();
                *artist_ref.lock().unwrap() = next.artist.clone();
                if let Some(dur) = next.duration_ms {
                    duration_ms_arc.store(dur, Ordering::SeqCst);
                }
                // Reset position and seek offset for the new track.
                // The poller will see position drop from near-end to 0,
                // detect a gapless position reset, and call
                // advance_queue_metadata() — no stop/restart needed.
                seek_offset = 0;
                seek_offset_arc.store(0, Ordering::SeqCst);
                position_ms.store(0, Ordering::SeqCst);

                // Reset per-track counters
                total_frames_fed = 0;
                total_bytes_read = 0;
                etage.en_attente.clear();
                // `http_eof` n'est plus remis à zéro ici : la boucle producteur
                // le repose à CHAQUE sortie, sans exception. Le laisser ferait
                // croire qu'un chemin l'oublie.

                // Process initial PCM data from the header read
                let gapless_pcm = if new_data_offset < next_header.len() {
                    next_header[new_data_offset..].to_vec()
                } else {
                    Vec::new()
                };
                etage.en_attente.extend_from_slice(&gapless_pcm);
                // Même frontière que la piste initiale : la piste chaînée
                // conserve l'état du DSP mais prend une nouvelle décision
                // PCM/DoP avant son premier échantillon (#2296/#2232).
                // #3233 : le porteur DoP ne survit pas a ce chemin — refuser
                // AVANT que le premier echantillon parte au DAC.
                let amorce_enchainee =
                    etage.pousser(&mut *puits, &mut refuser_le_porteur_dop, &mut |_| {});
                match amorce_enchainee {
                    PousseeVersLePuits::PorteurDopRefuse => {
                        if play_generation.load(Ordering::SeqCst) == my_generation {
                            playing.store(false, Ordering::SeqCst);
                        }
                        return;
                    }
                    // Comme l'amorce de la piste initiale : les trames sont
                    // comptées sans regarder le verdict de l'écriture.
                    PousseeVersLePuits::Poussee { trames_source }
                    | PousseeVersLePuits::PuitsMort { trames_source } => {
                        total_frames_fed += trames_source;
                    }
                    PousseeVersLePuits::RienAPousser => {}
                }

                // Main read loop for the gapless-chained track.
                //
                // La MÊME boucle que la piste initiale : seul le rôle change,
                // et il ne choisit que les noms d'événement. C'est tout
                // l'intérêt — #3108 avait dû être corrigé DEUX fois parce que
                // ces deux boucles étaient deux copies.
                let producteur_enchaine = BoucleProducteur {
                    role: RoleDeLaBoucle::PisteEnchainee,
                    device_name: &device_name,
                    cle_de_flux,
                    stop_rx: &stop_rx,
                    force_silent: force_silent.as_ref(),
                    device_gone: device_gone.as_ref(),
                    position_ms: position_ms.as_ref(),
                    open_failure: open_failure.as_ref(),
                    debut_du_flux: std::time::Instant::now(),
                };
                let mut gapless_read_buf = vec![0u8; 65536];
                let mut compteurs_enchaines = CompteursDePiste {
                    total_bytes_read,
                    total_frames_fed,
                    seek_offset,
                    // Une piste enchaînée repart de son début : rien à jeter.
                    skip_bytes: 0,
                    skipped_bytes: 0,
                    premiere_donnee_journalisee: true,
                };
                let fin_enchainee = producteur_enchaine.tourner(
                    &mut next_reader,
                    &mut gapless_read_buf,
                    &mut etage,
                    &mut *puits,
                    &mut refuser_le_porteur_dop,
                    &mut compteurs_enchaines,
                    &mut |_| true,
                );
                total_bytes_read = compteurs_enchaines.total_bytes_read;
                total_frames_fed = compteurs_enchaines.total_frames_fed;
                match fin_enchainee {
                    FinDeBoucle::FinDeFlux => http_eof = true,
                    FinDeBoucle::Interrompue => http_eof = false,
                    FinDeBoucle::PorteurDopRefuse => {
                        if play_generation.load(Ordering::SeqCst) == my_generation {
                            playing.store(false, Ordering::SeqCst);
                        }
                        return;
                    }
                    FinDeBoucle::Abandon => return,
                }

                if http_eof {
                    report_incomplete_local_pcm_probe(etage.pcm_kind, etage.en_attente.len());
                }

                // If this track also reached clean EOF, loop back to check
                // for yet another gapless next track.  Otherwise, exit the
                // gapless loop and fall through to normal end handling.
                if !http_eof {
                    break;
                }
                info!("local_audio_gapless_track_finished_checking_next");
            }
            // ---------------------------------------------------------------
            // End of gapless continuation
            // ---------------------------------------------------------------

            // La boucle est finie : ce fil n'enchaînera plus rien, quelle qu'en
            // soit la raison (rien en réserve, HTTP en erreur, en-tête vide,
            // flux suivant non-WAV, piste chaînée sans fin propre). Le DIRE au
            // poller, qui relit la capacité pendant qu'il attend : tant qu'elle
            // vaut `true`, il attend une transition d'un fil qui n'existe plus
            // et l'avance métadonnées seule part sans aucun `play` (#1919 ;
            // même défaut que #1323 sur OAAT).
            //
            // Une seule sortie n'est PAS un épuisement : celle où une lecture
            // plus récente nous a supplantés (`force_silent`, ou génération qui
            // a bougé). Là c'est `play_url()` qui a déjà remis le drapeau à
            // zéro pour le fil suivant — le lever ici désarmerait SON gapless.
            if doit_declarer_chaine_epuisee(
                force_silent.load(Ordering::Relaxed),
                play_generation.load(Ordering::SeqCst),
                my_generation,
            ) {
                chain_exhausted_ref.store(true, Ordering::SeqCst);
                debug!("local_audio_gapless_chain_exhausted");
            }

            // La queue appartient à la FIN EFFECTIVE de la chaîne, pas au
            // simple fait qu'un prochain média ait été annoncé. Celui-ci peut
            // encore échouer en HTTP, être vide ou ne pas être un WAV : dans
            // ces cas `next_media_ref.is_some()` avait fait sauter le drainage
            // avant la boucle et la convolution restait tronquée (#2295/#2296).
            //
            // Ne rien rendre après Stop, abort ou perte du périphérique : la
            // queue est de l'audio et ne doit jamais ressusciter une lecture
            // interrompue.
            if http_eof
                && !force_silent.load(Ordering::Relaxed)
                && !device_gone.load(Ordering::Relaxed)
            {
                etage.rendre_la_queue_du_dsp(&mut *puits);
            }

            // Flush the resampler: process any leftover frames + drain internal delay
            if http_eof
                && etage.needs_resample
                && !force_silent.load(Ordering::Relaxed)
                && !device_gone.load(Ordering::Relaxed)
            {
                etage.vider(&mut *puits);
            }

            // Wait for the ring buffer to drain (real playback) before signalling
            // the natural track end. The HTTP thread finishes FEEDING all samples
            // well before the DAC has PLAYED them — up to ~2s at the output rate
            // (more when resampling 44.1→192). The old code signalled end + left
            // the reported position at the fed/decoded end BEFORE draining, so the
            // poller saw position past (DB) duration + margin while up to ~2s was
            // still queued in the ring, and advanced the queue early — cutting the
            // end of every track (JP Borderies, WASAPI/ASIO exclusive, VX248: log
            // showed ring_available ~1.4M f32 samples still queued at advance time).
            //
            // Fix: during the drain, report the PLAYED position (fed − what is
            // still queued in the ring) so the poller's position-past-end check
            // tracks real playback; only signal track_ended_naturally once the ring
            // is actually empty. If a new play/stop interrupts the drain
            // (force_silent/stop_rx), the queue already moved on (force_silent is
            // only set by a fresh play_url) — so we must NOT emit a natural end for
            // this superseded track.
            // NEVER drain forever: with a dead render callback (USB DAC hot-
            // unplugged, #1626 — on macOS no error callback ever fires) the
            // ring stays full and this loop used to spin until restart, keeping
            // the zone "Playing" and freezing the hotplug rescan. Deadline =
            // queued audio duration + 5s margin (same guard as the ASIO
            // exclusive path's asio_drain_timeout).
            //
            // R8 : la boucle de vidage vit dans le backend (`drainer`) ; ici
            // on ne calcule que sa borne, à partir de ce qu'il observe.
            let drain_deadline = drain_deadline_for(
                backend.observer().disponible,
                output_sr as u64,
                output_ch as u64,
            );
            let vidage = backend.drainer(drain_deadline);

            if http_eof && vidage.vide {
                position_ms.store(vidage.position_alimentee_ms, Ordering::Relaxed);
                track_ended_naturally.store(true, Ordering::SeqCst);
                track_ended_generation.store(my_generation, Ordering::SeqCst);
                TRACK_END_NOTIFY.notify_one();
                debug!(
                    total_bytes_read,
                    total_frames_fed, "local_audio_track_ended_naturally_post_drain"
                );
            }

            // Hand the poller something to say when the device disappeared
            // mid-playback (#1626) — same channel as the open-failure path, so
            // the zone shows a clear message instead of silently stopping.
            if device_gone.load(Ordering::Relaxed) {
                if let Ok(mut slot) = open_failure.lock() {
                    // Ne pas écraser un constat déjà posé : le blocage de
                    // l'anneau (#3108) dit la même panne AVEC la position où
                    // l'écran s'est figé, et il est arrivé le premier.
                    if slot.is_none() {
                        *slot = Some(format!(
                            "Sortie « {device_name} » : {}.",
                            OpenFailure::DeviceGone.user_message()
                        ));
                    }
                }
            }

            drop(backend);
            if play_generation.load(Ordering::SeqCst) == my_generation {
                playing.store(false, Ordering::SeqCst);
            } else {
                debug!("local_audio_stale_thread_skipping_playing_false");
            }
            info!(
                device = %device_name,
                frames = total_frames_fed,
                total_bytes_read,
                elapsed_ms = stream_start.elapsed().as_millis() as u64,
                "local_audio_stopped"
            );
        });

        *self.stop_tx.lock().unwrap() = Some(stop_tx);
        *self.play_thread.lock().unwrap() = Some(handle);
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.paused.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.paused.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        // Plus de flux, donc plus de format : sans cet oubli explicite,
        // `current_format()` decrirait encore la piste precedente et on
        // rebatirait un EqProcessor pour un flux mort (#1725).
        self.current_format.store(0, Ordering::Relaxed);
        // Immediately silence the cpal callback so no audio leaks while
        // we wait for the playback thread to exit.  This flag is also
        // checked by the I/O read loop and feed_ring, causing the thread
        // to exit promptly.
        self.force_silent
            .lock()
            .unwrap()
            .store(true, Ordering::SeqCst);
        // Laisser la rampe anti-« ploc » finir sa descente avant de relâcher le
        // flux (#1590). `force_silent` vient d'être armé : le callback est déjà
        // en train de descendre. Sans cette attente, le fil de lecture peut
        // détruire le flux cpal au milieu de la rampe et le clic revient — le
        // fondu à l'arrêt serait alors une loterie.
        //
        // L'attente est bornée par la rampe elle-même : nulle quand elle est
        // désarmée (DoP, PURE, sortie exclusive, réglage à zéro), nulle quand
        // rien ne joue, et jamais plus que `SOFT_MUTE_MAX_MS`. À la valeur par
        // défaut cela fait 20 ms, à comparer aux 2 000 ms que `stop()` accepte
        // déjà d'attendre juste après pour la sortie du fil.
        let drain_ms = crate::audio::soft_mute::stop_drain_ms(
            self.armed_soft_mute_ms(),
            self.playing.load(Ordering::SeqCst),
        );
        if drain_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(drain_ms)).await;
        }
        // Send the stop signal via channel (belt-and-suspenders with force_silent)
        if let Some(tx) = self.stop_tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
        // Unpause so the thread unblocks from pause-wait loops
        self.paused.store(false, Ordering::SeqCst);
        // Wait for the playback thread to exit so the cpal stream is
        // dropped (releasing the audio device) before a new track starts.
        // Even if the thread is slow to exit (blocked on HTTP I/O), the
        // force_silent flag ensures silence, and play_url() creates a
        // FRESH force_silent Arc so the old callback stays permanently muted.
        let old_handle = self.play_thread.lock().unwrap().take();
        if let Some(handle) = old_handle {
            let _ = tokio::task::spawn_blocking(move || {
                // Wait for the playback thread to exit. ASIO exclusive needs
                // the device fully released before reopening — use 2s timeout
                // instead of 500ms to avoid device contention on rapid seeks.
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
                loop {
                    if handle.is_finished() {
                        let _ = handle.join();
                        return;
                    }
                    if std::time::Instant::now() >= deadline {
                        // Detach — force_silent keeps the old callback silent
                        // so there is no audible overlap; the thread will exit
                        // on its own once the blocking read returns.
                        //
                        // #3575 — cette ligne était en `debug!`, donc INVISIBLE
                        // de tout relevé de terrain : les exports de journaux
                        // de Belkadi Yacine ne portent que de l'INFO et
                        // au-dessus (854 INFO, 59 WARN, 2 ERROR, ZÉRO debug).
                        // Son absence ne prouvait donc RIEN, et c'est pourtant
                        // elle qui départage les deux histoires : un fil
                        // détaché tient toujours le flux cpal, donc le PCM
                        // `hw:` EXCLUSIF, et la lecture suivante prend `EBUSY`
                        // — que cpal replie sur « no longer available ».
                        //
                        // Elle passe en `warn!` : détacher un fil de lecture
                        // n'est pas un événement de routine, c'est le renoncement
                        // à une garantie.
                        warn!(
                            attente_ms = 2000,
                            "local_audio_stop_thread_detached — le fil de lecture précédent \
                             n'a pas rendu la main : il tient peut-être encore le périphérique"
                        );
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            })
            .await;
        }
        self.playing.store(false, Ordering::SeqCst);
        self.position_ms.store(0, Ordering::SeqCst);
        self.seek_offset_ms.store(0, Ordering::SeqCst);
        self.duration_ms.store(0, Ordering::SeqCst);
        // Clear the natural-end flag and generation so stale signals from
        // the previous track do not affect the next track's end-detection cycle.
        self.track_ended_naturally.store(false, Ordering::SeqCst);
        self.track_ended_generation.store(0, Ordering::SeqCst);
        if let Ok(mut slot) = self.signal_path_status.lock() {
            *slot = None;
        }
        *self.next_media.lock().unwrap() = None;
        *self.current_uri.lock().unwrap() = None;
        *self.track_title.lock().unwrap() = None;
        *self.track_artist.lock().unwrap() = None;
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        // The local output plays from an HTTP stream consumed sequentially,
        // so true seek requires the orchestrator to restart the stream.
        // Store the seek offset so the new stream (which starts counting
        // frames from 0) reports the correct absolute position.
        self.seek_offset_ms.store(position_ms, Ordering::SeqCst);
        self.position_ms.store(position_ms, Ordering::SeqCst);
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        let v = (volume.clamp(0.0, 1.0) * 1000.0) as u32;
        self.user_volume.store(v, Ordering::SeqCst);
        self.recompute_effective_volume();
        if v > 0 {
            self.muted.store(false, Ordering::SeqCst);
        }
        Ok(())
    }

    fn set_replaygain_factor(&self, factor: f64) {
        LocalOutput::set_replaygain_factor(self, factor);
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        if muted {
            let current = self.user_volume.load(Ordering::SeqCst);
            if current > 0 {
                self.pre_mute_volume.store(current, Ordering::SeqCst);
            }
            self.user_volume.store(0, Ordering::SeqCst);
            self.muted.store(true, Ordering::SeqCst);
        } else {
            let restored = self.pre_mute_volume.load(Ordering::SeqCst);
            self.user_volume
                .store(if restored > 0 { restored } else { 1000 }, Ordering::SeqCst);
            self.muted.store(false, Ordering::SeqCst);
        }
        self.recompute_effective_volume();
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let duration_ms = self.duration_ms.load(Ordering::Relaxed);

        // When the playback thread has signalled natural end-of-stream
        // (track_ended_naturally=true) but is still alive (playing=true,
        // typically blocked in WASAPI's drop(stream)), report the track as
        // Playing with position past the end.  This causes the poller's
        // position_past_end path (TransportState::Playing branch) to fire
        // after POSITION_PAST_END_TICKS, triggering auto_next without
        // waiting for the thread to fully exit.
        //
        // Once the thread finishes and sets playing=false, this branch no
        // longer fires and the normal Stopped state is reported — allowing
        // the is_short_track fast-path in the poller's Stopped branch to
        // handle short tracks correctly.
        //
        // The flag is cleared by stop() and play_url() so it only applies
        // to the current track.
        if self.track_ended_naturally.load(Ordering::Relaxed)
            && self.playing.load(Ordering::Relaxed)
            && duration_ms > 0
            && self.track_ended_generation.load(Ordering::Relaxed)
                == self.play_generation.load(Ordering::Relaxed)
        {
            return Ok(OutputStatus {
                state: TransportState::Playing,
                position_ms: duration_ms.saturating_add(5000),
                duration_ms,
                volume: self.user_volume.load(Ordering::Relaxed) as f64 / 1000.0,
                muted: self.muted.load(Ordering::Relaxed),
                current_uri: self.current_uri.lock().unwrap().clone(),
                track_title: self.track_title.lock().unwrap().clone(),
                track_artist: self.track_artist.lock().unwrap().clone(),
                ended_naturally: true,
                // A renderer plays at 1x: keep the poller's wall-clock guards.
                realtime: true,
                dop_active: self.dop_active.load(Ordering::Relaxed),
            });
        }

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
            duration_ms,
            volume: self.user_volume.load(Ordering::Relaxed) as f64 / 1000.0,
            muted: self.muted.load(Ordering::Relaxed),
            current_uri: self.current_uri.lock().unwrap().clone(),
            track_title: self.track_title.lock().unwrap().clone(),
            track_artist: self.track_artist.lock().unwrap().clone(),
            ended_naturally: self.track_ended_naturally.load(Ordering::Relaxed),
            // A renderer plays at 1x: keep the poller's wall-clock guards.
            realtime: true,
            // Détecté sur les octets par `is_dop_pcm`, jamais déduit des
            // réglages de zone : c'est la seule valeur qui dise si le volume
            // est réellement épinglé à l'unité en ce moment (#1735).
            dop_active: self.dop_active.load(Ordering::Relaxed),
        })
    }

    fn take_output_failure(&self) -> Option<String> {
        self.open_failure.lock().ok().and_then(|mut s| s.take())
    }

    fn signal_path_status(&self) -> Option<OutputSignalPathStatus> {
        self.signal_path_status
            .lock()
            .ok()
            .and_then(|status| status.clone())
    }

    fn ring_starvation(&self) -> Option<OutputRingStarvation> {
        Some(self.starvation.snapshot())
    }
    fn dsp_metrics(&self) -> Option<OutputDspMetrics> {
        self.eq.lock().ok().and_then(|eq| {
            eq.as_ref().map(|processor| {
                let stats = processor.process_stats();
                OutputDspMetrics {
                    eq_overs: stats.overs,
                    eq_non_finite_samples: stats.non_finite_samples,
                }
            })
        })
    }

    async fn is_available(&self) -> bool {
        let name = self.device_name.clone();
        let backend = self.audio_backend.clone();
        // Probe on a blocking thread to avoid cpal blocking the async runtime
        tokio::task::spawn_blocking(move || {
            let host = select_host(&backend);
            if name == "default" {
                return host.default_output_device().is_some();
            }
            host.output_devices()
                .map(|devs| {
                    devs.into_iter().any(|d| {
                        d.description()
                            .map(|desc| {
                                let n = desc.name().to_string();
                                n == name || n.contains(&name)
                            })
                            .unwrap_or(false)
                    })
                })
                .unwrap_or(false)
        })
        .await
        .unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build the error callback for a shared-mode cpal stream.
///
/// A hot-unplugged USB DAC surfaces here — and used to be merely logged, which
/// left the feeding thread waiting forever on a ring buffer nobody drains
/// (issue #1626). Instead:
///
/// - `DeviceNotAvailable` (WASAPI raises it when the endpoint is invalidated)
///   flags `device_gone` so the feeding thread tears down, and is logged once.
/// - Any other error still flags nothing but is rate-limited to one log line
///   per second: cpal 0.17's ALSA `output_stream_worker` loops on error
///   (`error_callback(...); continue`), and with a dead fd `poll()` returns
///   immediately — unbounded logging floods the log at poll speed until the
///   stream is dropped.
///
/// On macOS CoreAudio the callback typically never fires on unplug (the
/// AudioUnit just stops rendering); the feed-stall and drain deadlines in the
/// playback thread cover that case.
fn make_stream_error_cb(
    device_gone: Arc<AtomicBool>,
    starvation: Arc<RingStarvation>,
) -> impl FnMut(cpal::StreamError) + Send + 'static {
    let mut last_warn: Option<std::time::Instant> = None;
    move |e: cpal::StreamError| {
        if matches!(e, cpal::StreamError::DeviceNotAvailable) {
            if !device_gone.swap(true, Ordering::SeqCst) {
                warn!(error = %e, "audio_stream_device_lost");
            }
            return;
        }
        // #3205 — le pilote n'a pas été servi à temps. On COMPTE avant de
        // journaliser : le `warn!` ci-dessous est plafonné à une ligne par
        // seconde, et ce plafond rendait la mesure impossible — une heure à
        // 5 000 sous-alimentations et une heure à 3 600 laissaient le même
        // journal. C'est ce chiffre qui décide du noyau `PREEMPT_RT` de
        // Tune OS, et la famine de l'anneau ne peut pas le voir : sur un XRun
        // cpal saute le rappel de données, donc l'anneau reste plein.
        if matches!(e, cpal::StreamError::BufferUnderrun) {
            starvation.record_driver_underrun();
        }
        if last_warn.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(1)) {
            warn!(error = %e, "audio_stream_error");
            last_warn = Some(std::time::Instant::now());
        }
    }
}

/// Seuil du détecteur de blocage : au-delà, le consommateur de l'anneau est
/// tenu pour mort. Très au-dessus de toute contre-pression normale (le rappel
/// de rendu vide un anneau plein en quelques périodes de tampon), donc jamais
/// atteint par une lecture saine.
const FEED_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Feed samples into the ring buffer, blocking (with sleep) when full.
/// Checks the stop signal, abort flag, and pause state periodically.
/// Returns immediately when abort is signaled or stop is received.
///
/// Returns `false` ONLY when the wedge detector tripped (the consumer stopped
/// draining the ring for ≥5s — dead render callback, e.g. unplugged USB DAC);
/// `true` otherwise, including stop/abort exits which the callers already
/// detect through their own checks.
fn feed_ring_abortable(
    ring: &RingBuf,
    samples: &[f32],
    stop_rx: &std::sync::mpsc::Receiver<()>,
    paused: &AtomicBool,
    abort: Option<&AtomicBool>,
) -> bool {
    feed_ring_abortable_with_stall_timeout(
        ring,
        samples,
        stop_rx,
        paused,
        abort,
        FEED_STALL_TIMEOUT,
    )
}

/// Le corps réel de [`feed_ring_abortable`], avec son seuil de blocage en
/// paramètre.
///
/// Le seuil est injecté pour UNE raison : le vérifier sans dormir cinq
/// secondes. Un test qui passe `Duration::ZERO` traverse exactement le même
/// code que la production — c'est la boucle de production, pas une réplique.
fn feed_ring_abortable_with_stall_timeout(
    ring: &RingBuf,
    samples: &[f32],
    stop_rx: &std::sync::mpsc::Receiver<()>,
    paused: &AtomicBool,
    abort: Option<&AtomicBool>,
    stall_timeout: std::time::Duration,
) -> bool {
    let mut offset = 0;
    // Wedge detector: if the render callback stops consuming, the ring stays
    // full and `ring.push` returns 0 forever. Bail after a sustained stall so
    // the device-owning thread can tear down (and release the ASIO device lock)
    // instead of blocking permanently (DEvir bug-22 / #789: the callback
    // quiesced at a Repeat loop point). The 5s threshold is far longer than any
    // normal back-pressure wait (the callback drains a full ring within a few
    // buffer periods), so this never trips during healthy playback.
    let mut last_progress_at = std::time::Instant::now();
    while offset < samples.len() {
        if stop_rx.try_recv().is_ok() {
            return true;
        }
        if abort.map_or(false, |a| a.load(Ordering::Relaxed)) {
            return true;
        }
        // If paused, wait without feeding
        while paused.load(Ordering::Relaxed) {
            if stop_rx.try_recv().is_ok() {
                return true;
            }
            if abort.map_or(false, |a| a.load(Ordering::Relaxed)) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            // A deliberate pause is not a stall.
            last_progress_at = std::time::Instant::now();
        }
        let written = ring.push(&samples[offset..]);
        offset += written;
        if written == 0 {
            if last_progress_at.elapsed() >= stall_timeout {
                warn!(
                    remaining_samples = samples.len() - offset,
                    "asio_feed_ring_stall_timeout"
                );
                return false;
            }
            // Ring buffer full — wait a bit
            std::thread::sleep(std::time::Duration::from_millis(5));
        } else {
            last_progress_at = std::time::Instant::now();
        }
    }
    true
}

/// Combien de temps accorder au vidage d'un anneau qui contient encore
/// `queued_samples` échantillons entrelacés.
///
/// Durée de l'audio en attente + 5 s de marge. Extrait des deux chemins
/// partagés qui la calculaient déjà en ligne (#1626) pour que le chemin
/// CoreAudio exclusif — le seul qui n'en avait AUCUNE — s'y raccroche sans
/// recopier l'arithmétique.
fn drain_deadline_for(
    queued_samples: usize,
    sample_rate: u64,
    channels: u64,
) -> std::time::Duration {
    std::time::Duration::from_millis(
        (queued_samples as u64 * 1000) / (sample_rate.max(1) * channels.max(1)) + 5000,
    )
}

#[cfg(target_os = "windows")]
fn feed_native_ring_abortable(
    ring: &NativePcmRing,
    samples: &[i32],
    stop_rx: &std::sync::mpsc::Receiver<()>,
    paused: &AtomicBool,
    abort: Option<&AtomicBool>,
) -> bool {
    let mut offset = 0;
    let mut last_progress_at = std::time::Instant::now();
    while offset < samples.len() {
        if stop_rx.try_recv().is_ok() || abort.is_some_and(|a| a.load(Ordering::Relaxed)) {
            return true;
        }
        while paused.load(Ordering::Relaxed) {
            if stop_rx.try_recv().is_ok() || abort.is_some_and(|a| a.load(Ordering::Relaxed)) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            last_progress_at = std::time::Instant::now();
        }
        let written = ring.push(&samples[offset..]);
        offset += written;
        if written == 0 {
            if last_progress_at.elapsed() >= std::time::Duration::from_secs(5) {
                warn!(
                    remaining_samples = samples.len() - offset,
                    "windows_native_feed_ring_stall_timeout"
                );
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        } else {
            last_progress_at = std::time::Instant::now();
        }
    }
    true
}

mod resolution;
pub use resolution::*;

/// Simple linear-interpolation resampler for rate conversion.
/// Kept as a fallback — the main path now uses rubato sinc resampling.
/// Implementation lives in `crate::audio`; only this file's tests still
/// reference it directly.
#[cfg(test)]
use crate::audio::simple_resample;

/// Rubato sinc resampling helpers. Implementation moved to
/// `crate::audio::resample` (#1525) so the file converter can share it
/// without the `local-audio` feature; re-exported for this pipeline's
/// existing call sites and tests.
pub(crate) use crate::audio::resample::{rubato_resample_chunk, rubato_resample_track};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod open_failure_tests;
/// #3575 — le PCM exclusif que Tune se prend a lui-meme.
///
/// Les fonctions eprouvees ici sont PURES et compilees sur toutes les cibles :
/// la decision d ouverture est sortie du fil `std::thread::spawn` justement
/// pour cela. La sentinelle, elle, est eprouvee sur son ORDRE de destruction,
/// qui est tout son contrat.
#[cfg(test)]
mod relache_peripherique_i3575;

/// #3270 — « la piste ne joue pas, et rien ne le dit ».
///
/// Le refus d'OUVERTURE avait son canal, le blocage d'APRÈS l'ouverture aussi.
/// Ce qui n'en avait aucun, c'est l'échec de DÉCODAGE : le flux compressé est
/// chargé en entier, symphonia refuse, et le fil rendait la main sur un
/// `warn!`, un drapeau à `false` et un `return` nu. Le périphérique n'ayant
/// jamais été ouvert, aucune des heuristiques du sondeur ne rattrapait la
/// zone — et cette branche n'est pas un vestige : Bandcamp, les podcasts,
/// l'UPnP et les fichiers téléversés y passent sans transcodage WAV.
#[cfg(test)]
mod decode_failure_tests;

/// #3108 — « la zone reste figée à 2 s, sans message ».
///
/// Le refus d'OUVERTURE avait déjà son canal (`record_exclusive_open_failure`).
/// Ce qui n'en avait aucun, c'est la panne d'APRÈS l'ouverture : le
/// périphérique accepte, puis son rappel de rendu se tait. L'anneau se remplit
/// une fois — deux secondes d'audio, par construction — et plus rien ne bouge.
///
/// Les trois fonctions éprouvées ici sont celles de la production, compilées
/// sur toutes les cibles. Aucune ne dort : le seuil de blocage est injecté.
#[cfg(test)]
mod feed_stall_tests;

#[cfg(test)]
mod backend_display_tests;

/// #1395 — le motif du repli, pas seulement son résultat.
///
/// Toutes les fonctions éprouvées ici sont **pures** et compilées sur toutes les
/// cibles : la branche ASIO de `select_host` vit sous
/// `#[cfg(all(target_os = "windows", feature = "asio"))]` et n'est exécutable ni
/// sur macOS, ni sur Linux, ni en CI. Sortir la décision de cpal est ce qui rend
/// la FAMILLE entière testable ailleurs que sur la machine du testeur.
#[cfg(test)]
mod backend_fallback_tests;

#[cfg(test)]
mod backends_supportes_tests;

#[cfg(test)]
mod format_courant_tests;

#[cfg(test)]
mod chemin_compresse_dsp_tests;

#[cfg(test)]
mod enumeration_asio_occupee_tests;

// ---------------------------------------------------------------------------
// #2272 — deux sorties locales homonymes, et rien pour dire laquelle est laquelle
//
// Marco Polo, forum du 2026-06-08 : « Voici comment Tune présente mes
// "haut-parleurs" locaux : comment savoir lequel est lequel ? » Deux DAC USB
// sous WASAPI s'annoncent tous deux « Haut-Parleurs » ; la découverte suffixait
// « (2) » au second — un rang d'énumération, qui évite qu'un périphérique
// disparaisse (#1084) mais ne nomme rien et peut changer au redémarrage.
//
// Ce qui manquait n'était pas une propriété système : cpal la lisait déjà. Le
// `DeviceDescription` que l'énumération obtenait portait
// `DEVPKEY_DeviceInterface_FriendlyName` dans son champ `driver` — et
// l'énumération n'en lisait que `name()`.
// ---------------------------------------------------------------------------

/// La garde de site : le renseignement est-il vraiment BRANCHÉ ?
///
/// Les épreuves ci-dessous exercent la règle et l'adaptateur, mais aucune ne
/// peut voir la seule chose qui reste : que l'énumération les APPELLE, et que
/// le périphérique qu'elle publie porte le résultat. Une règle juste, calculée
/// puis jetée, les laisserait toutes vertes. On relit donc la source — même
/// procédé et même raison que `position_publiee_guard` dans `poller.rs`.
#[cfg(test)]
mod renseignement_materiel_guard;

#[cfg(test)]
mod renseignement_materiel_tests;

/// #1770 — une zone créée à partir d'une énumération WASAPI alors qu'ASIO est
/// configuré ne pouvait JAMAIS jouer.
///
/// Ces essais construisent la sortie par l'EXPRESSION EXACTE des deux sites
/// d'enregistrement (`tune-server/src/startup.rs::register_local_outputs` et
/// `tune-server/src/background.rs::rescan_local_audio_devices`) et mesurent ce
/// que la sortie portera à l'ouverture. Ils ne rappellent aucune condition :
/// `LocalOutput::audio_backend()` est la valeur que lisent `select_host`, la
/// branche `exclusive_mode && audio_backend == "asio"` et `is_available`.
///
/// La branche ASIO exclusive elle-même vit sous
/// `#[cfg(all(target_os = "windows", feature = "asio"))]` : elle ne se compile
/// ni sur Shrek ni sur aucune porte de ce dépôt. Élargir ce `cfg` serait INERTE
/// ici — la caisse `cpal/asio` ne se lie pas hors Windows. C'est donc la valeur
/// D'ENTRÉE de cette branche qui est tenue, pas la branche.
#[cfg(test)]
mod zone_backend_asio_i1770;

// ───────────────────────────────────────────────────────────────────────────
// #3318 — les deux lignes de journal du fil de lecture de la sortie locale,
// écrites ici plutôt qu'en ligne, pour DEUX raisons :
//
// 1. elles vivent au fond d'un `std::thread::spawn` qu'aucun test unitaire ne
//    peut atteindre — ni périphérique ALSA, ni flux HTTP dans une épreuve ;
//    sorties, elles s'éprouvent ;
// 2. elles doivent porter la MÊME clé que `stream_delivery_stall`, sans quoi
//    les deux moitiés de la mesure restent inutilisables ensemble.
//
// `cle_de_correlation_i3318.rs` garde le contenu émis ET le fait que le fil
// de lecture les appelle bien avec la clé — « écrit mais pas branché » est
// précisément le défaut qui a laissé ce dossier en plan.
// ───────────────────────────────────────────────────────────────────────────

/// Ce qu'on écrit à la place d'un identifiant de flux quand l'URL n'en porte
/// pas.
///
/// Ce cas EXISTE et n'est pas un bug : la sortie locale sait aussi lire une
/// radio ou un fichier servi par un tiers, dont l'URL n'a pas la forme
/// `…/stream/<id>`. Un tiret est lisible dans un `grep` et ne se confond avec
/// aucun identifiant ; un champ absent, lui, se serait lu comme une ligne
/// d'une autre version.
pub(crate) const FLUX_INCONNU: &str = "-";

/// `local_audio_slow_read` — le fil de lecture a attendu ses octets.
///
/// `wait_ms` est la durée d'UN `reader.read()`, pas un cumul. Le seuil
/// d'émission (5 s) est chez l'appelant : cette fonction écrit ce qu'on lui
/// donne.
///
/// `stream_id` part en Display (`%`) et NON en Debug : `stream_delivery_stall`
/// rend `stream_id=e32c865e-…` sans guillemets, et une clé de jointure qui ne
/// s'écrit pas pareil des deux côtés ne se cherche pas d'un seul `grep`.
pub(crate) fn journaliser_lecture_lente(
    device: &str,
    stream_id: Option<&str>,
    bytes: usize,
    wait_ms: u64,
    total_bytes_read: u64,
) {
    warn!(
        device = %device,
        stream_id = %stream_id.unwrap_or(FLUX_INCONNU),
        bytes,
        wait_ms,
        total_bytes_read,
        "local_audio_slow_read — la sortie locale a attendu ses octets ; \
         `stream_id` joint cette ligne au `stream_delivery_stall` du flux \
         interne (#3318)"
    );
}

/// `local_audio_read_error` — le flux interne a rendu une erreur au lieu
/// d'octets, et le fil de lecture s'arrête là.
///
/// C'est la coupure FRANCHE du fil 1660 (« le flux s'interrompt complètement
/// et la lecture s'arrête net »), par opposition à l'attente de
/// [`journaliser_lecture_lente`]. Les deux sortent du même `reader.read()` :
/// c'est pourquoi elles portent la même clé.
pub(crate) fn journaliser_erreur_de_lecture(
    device: &str,
    stream_id: Option<&str>,
    erreur: &str,
    total_bytes_read: u64,
) {
    warn!(
        device = %device,
        stream_id = %stream_id.unwrap_or(FLUX_INCONNU),
        error = %erreur,
        total_bytes_read,
        "local_audio_read_error — le flux interne a rendu une erreur ; \
         `stream_id` joint cette ligne au `stream_delivery_stall` du flux \
         interne (#3318)"
    );
}

#[cfg(test)]
mod cle_de_correlation_i3318;

#[cfg(test)]
mod repli_format_compresse_i3618;

#[cfg(test)]
mod parc_lisible_sans_attendre_i3730;

#[cfg(test)]
mod pcm_materiel_a_la_resolution_i1655;

#[cfg(test)]
mod empreinte_du_puits_r1;

/// T8 de #2218 — le puits de capture branché sur une VRAIE piste.
///
/// R1 garde la conversion contre des relevés pris sur la version d'avant ; T1
/// garde le décodeur contre `flac -d`. Ce module relie les deux : une fixture
/// du banc jouée jusqu'au puits, et l'empreinte livrée comparée à celle du
/// décodeur de référence. Voir son en-tête pour ce qu'il ne couvre pas.
#[cfg(test)]
mod capture_bout_en_bout_2218;
