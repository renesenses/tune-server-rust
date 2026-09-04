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
    OutputCapabilities, OutputDspMetrics, OutputRingStarvation, OutputSignalPathStatus,
    OutputStatus, OutputTarget, RingStarvation, TransportState,
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
            Self::Unknown => {
                "le périphérique a refusé tous les formats proposés. Choisissez une autre \
                 sortie dans les réglages de la zone"
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Audio host selection (WASAPI vs ASIO on Windows)
// ---------------------------------------------------------------------------

/// Select the cpal host based on the requested backend.
///
/// - `"asio"`: use the ASIO host (requires `asio` cargo feature; Windows only).
///   Falls back to WASAPI, with a warning, if the host cannot be opened or
///   exposes no output device.
/// - `"wasapi"`: use the default host (WASAPI on Windows)
/// - `"auto"` (default): use WASAPI directly. **`auto` never probes ASIO.**
/// - anything else: treated like `"wasapi"`.
///
/// `auto` used to try ASIO first; it no longer does, since #199. Probing an
/// ASIO driver can make it call `abort()` and take the whole process down
/// without a trace, so the only way to reach ASIO is to ask for it by name.
/// Getting ASIO therefore takes a deliberate setting — see
/// [`crate::config::LOCAL_AUDIO_BACKEND_ENV`]. A machine whose ASIO drivers
/// are detected and listed by `/audio/asio-devices` is still playing through
/// WASAPI as long as the backend is left on `auto`: detecting is not playing.
///
/// On non-Windows platforms, always returns `cpal::default_host()`.
pub fn select_host(backend: &str) -> cpal::Host {
    let backend_lower = backend.to_lowercase();

    #[cfg(all(target_os = "windows", feature = "asio"))]
    {
        #[cfg(all(target_os = "windows", feature = "asio"))]
        super::asio_exclusive::ensure_com_initialized();
        match backend_lower.as_str() {
            "asio" => match cpal::host_from_id(cpal::HostId::Asio) {
                Ok(host) => {
                    let device_count = host.output_devices().map(|d| d.count()).unwrap_or(0);
                    let (active, fallback) = asio_outcome(Some(device_count));
                    if fallback.is_none() {
                        info!(
                            backend = "asio",
                            devices = device_count,
                            "local_audio_host_selected"
                        );
                        note_observed_backend(active, fallback);
                        return host;
                    }
                    warn!(
                        fallback_reason = LocalBackendFallback::AsioNoDevices.code(),
                        "local_audio_asio_no_devices — ASIO host OK but no output devices found, falling back to WASAPI"
                    );
                    note_observed_backend(active, fallback);
                    return cpal::default_host();
                }
                Err(e) => {
                    let (active, fallback) = asio_outcome(None);
                    warn!(
                        error = %e,
                        fallback_reason = LocalBackendFallback::AsioHostUnavailable.code(),
                        "local_audio_asio_host_unavailable — check ASIO driver installation"
                    );
                    info!(backend = "wasapi", "local_audio_host_fallback");
                    note_observed_backend(active, fallback);
                    return cpal::default_host();
                }
            },
            "auto" => {
                // Auto mode uses WASAPI directly — ASIO drivers can call
                // abort() when probed, crashing the process silently.
                // Users who want ASIO must set TUNE_LOCAL_AUDIO_BACKEND=asio
                // (the canonical name; the older TUNE_AUDIO_BACKEND is still
                // honoured as a fallback, but should not be recommended).
                info!(backend = "wasapi", "local_audio_host_selected_auto");
                note_observed_backend("WASAPI", None);
                return cpal::default_host();
            }
            _ => {
                info!(backend = "wasapi", "local_audio_host_selected");
                note_observed_backend("WASAPI", None);
                return cpal::default_host();
            }
        }
    }

    #[cfg(not(all(target_os = "windows", feature = "asio")))]
    {
        // Le membre de la famille qui n'enregistrait RIEN. Un binaire Windows
        // construit sans la feature `asio`, ou une bibliothèque migrée sur un
        // serveur Linux/macOS avec `local_audio_backend=asio` déjà persisté,
        // ouvrait le host par défaut sans laisser la moindre trace côté API :
        // le sélecteur continuait d'afficher ASIO, la lecture sortait ailleurs,
        // et le seul indice vivait dans une ligne WARN.
        let (active, fallback) = unsupported_outcome(&backend_lower);
        if let Some(reason) = fallback {
            warn!(
                fallback_reason = reason.code(),
                "local_audio_asio_requested_but_not_available — \
                 ASIO requires Windows and the `asio` cargo feature"
            );
        }
        note_observed_backend(active, fallback);
        cpal::default_host()
    }
}

/// Backend réellement retenu par le dernier `select_host`, quand il diffère de
/// ce qui était demandé.
///
/// `select_host` peut retomber sur WASAPI en silence : pilote ASIO absent, ou
/// installé mais sans périphérique de sortie parce qu'une autre application le
/// tient déjà — un pilote ASIO ne s'ouvre que dans un seul processus. Jusqu'ici
/// rien ne remontait cette bascule : l'interface continuait d'annoncer le
/// backend *demandé*, si bien qu'un utilisateur ayant choisi ASIO se voyait
/// confirmer « ASIO » alors que le son sortait en WASAPI (signalement Bilou).
static OBSERVED_BACKEND: std::sync::RwLock<Option<ObservedBackend>> = std::sync::RwLock::new(None);

/// Ce que le dernier `select_host` a réellement ouvert, et pourquoi il n'a pas
/// pu honorer la demande quand c'est le cas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ObservedBackend {
    name: &'static str,
    fallback_reason: Option<LocalBackendFallback>,
}

/// Le PÉRIPHÉRIQUE réellement ouvert par la dernière lecture locale, face à
/// celui que la zone demandait.
///
/// Frère jumeau d'[`OBSERVED_BACKEND`], et pour la même raison : le serveur
/// SAVAIT déjà ce qu'il avait ouvert — `WasapiExclusiveOutput::opened_device_name`
/// existe depuis #2207 — mais sa seule lecture était une ligne de journal
/// (`wasapi_exclusive_playing`). Aucun client n'a jamais pu voir l'écart.
///
/// Or l'écart existe : sur Windows, le chemin exclusif WASAPI appelle
/// `GetDefaultAudioEndpoint` quand la résolution par nom échoue, et le chemin
/// cpal partagé retombe explicitement sur le périphérique système
/// (`audio_device_not_found_falling_back_to_default`). Une zone réglée sur un
/// DAC peut donc jouer sur les haut-parleurs, sans que rien ne le dise.
///
/// ⚠️ Ce verrou porte la DERNIÈRE ouverture observée et n'est pas effacé à
/// l'arrêt — exactement comme `OBSERVED_BACKEND`. C'est pour cela que le nom
/// demandé est mémorisé **au même instant** que le nom ouvert : la paire reste
/// cohérente entre elle même si le réglage de la zone change ensuite.
static OBSERVED_DEVICE: std::sync::RwLock<Option<ObservedDevice>> = std::sync::RwLock::new(None);

/// Ce que la dernière ouverture de périphérique a demandé, et ce qu'elle a eu.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ObservedDevice {
    backend: &'static str,
    requested: String,
    /// Vide quand rien n'a été ouvert — voir [`LocalDeviceStatus::opened`].
    opened: String,
    opened_id: Option<String>,
    reason: Option<LocalDeviceFallback>,
}

/// La CADENCE de la dernière ouverture partagée : celle de la source, celle
/// réellement ouverte, et le motif de l'écart quand il y en a un.
///
/// #3233 — même famille que [`OBSERVED_DEVICE`], et pour la même raison : une
/// décision qui change ce qui part au DAC ne doit pas rester dans le seul
/// journal. Pierre M (fil 1043) lit « DSD64 » sur son écran pendant que Tune a
/// choisi d'ouvrir ailleurs ; sans ce verrou, il faut ses journaux pour le
/// savoir.
///
/// ⚠️ Ne concerne que le chemin cpal **partagé**. Les chemins exclusifs
/// (WASAPI exclusif, ASIO, hog CoreAudio) n'arbitrent pas : ils ouvrent à la
/// cadence de la source ou échouent, donc ils n'écrivent rien ici.
///
/// ⚠️ Comme `OBSERVED_DEVICE`, ce verrou porte la DERNIÈRE ouverture observée
/// et n'est pas effacé à l'arrêt.
static OBSERVED_RATE: std::sync::RwLock<Option<ObservedRate>> = std::sync::RwLock::new(None);

/// Ce que la dernière ouverture partagée a demandé comme cadence, et ce qu'elle
/// a ouvert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ObservedRate {
    source_sample_rate: u32,
    opened_sample_rate: u32,
    reason: Option<LocalRateFallback>,
    evidence_measured: bool,
}

/// Nom du backend tel que `select_host` l'a observé, ou `"local"` faute d'avoir
/// encore ouvert quoi que ce soit. Sert à étiqueter l'ouverture d'un
/// périphérique par le chemin cpal, qui ne connaît que la variante cpal.
fn observed_backend_name() -> &'static str {
    OBSERVED_BACKEND
        .read()
        .ok()
        .and_then(|g| *g)
        .map(|o| o.name)
        .unwrap_or("local")
}

/// Enregistre le périphérique réellement ouvert. **Appelé par chaque chemin
/// d'ouverture** : cpal partagé, WASAPI exclusif, ASIO exclusif, CoreAudio
/// exclusif.
///
/// `opened_id` vaut `None` quand le backend n'expose aucun identifiant stable
/// (ASIO et CoreAudio exclusif : l'`AudioDeviceID` de CoreAudio est un entier
/// réattribué au redémarrage, ce n'est pas une identité). Un champ absent est
/// honnête ; un champ inventé ne l'est pas.
fn note_opened_device(
    backend: &'static str,
    requested: &str,
    opened: &str,
    opened_id: Option<&str>,
) {
    note_device_outcome(backend, requested, opened, opened_id, None);
}

/// Enregistre une ouverture qui n'a **pas** honoré la demande, avec son motif.
///
/// `opened` vide = rien n'a été ouvert du tout (refus). Sinon, quelque chose a
/// bien joué, mais pas ce que la zone nommait.
fn note_device_outcome(
    backend: &'static str,
    requested: &str,
    opened: &str,
    opened_id: Option<&str>,
    reason: Option<LocalDeviceFallback>,
) {
    if let Ok(mut slot) = OBSERVED_DEVICE.write() {
        *slot = Some(ObservedDevice {
            backend,
            requested: requested.to_string(),
            opened: opened.to_string(),
            opened_id: opened_id.filter(|id| !id.is_empty()).map(str::to_string),
            reason,
        });
    }
}

/// Pourquoi la sortie locale ne tourne pas sur le backend demandé.
///
/// #1395 — le nom du backend actif ne suffit pas. Bilou règle sa zone « Ce PC /
/// Hauts Parleurs » sur ASIO, la lecture sort en WASAPI, et la seule trace du
/// basculement est une ligne `local_audio_asio_no_devices` dans le journal : il
/// a fallu qu'il poste une capture de ses logs sur le forum pour que quiconque
/// sache pourquoi. Le motif existe côté serveur ; il n'était simplement remonté
/// nulle part.
///
/// Les codes sont **stables** et destinés à la machine (le client les traduit),
/// sur le modèle de `runtime_reasons` du chemin du signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalBackendFallback {
    /// L'hôte ASIO s'ouvre mais n'expose **aucune** sortie. Cas de Bilou : un
    /// pilote ASIO ne s'ouvre que dans un seul processus, donc une autre
    /// application qui le tient le fait disparaître de l'énumération.
    AsioNoDevices,
    /// L'hôte ASIO ne s'ouvre pas du tout — pilote absent ou non enregistré.
    AsioHostUnavailable,
    /// ASIO a été demandé sur un binaire qui ne peut pas l'honorer : hors
    /// Windows, ou Windows compilé sans la feature `asio`. Connu à la
    /// compilation, donc affirmable sans avoir ouvert le moindre périphérique.
    AsioUnsupportedBuild,
}

impl LocalBackendFallback {
    /// Code stable, celui que porte la charge utile JSON et les journaux.
    pub fn code(self) -> &'static str {
        match self {
            Self::AsioNoDevices => "asio_no_devices",
            Self::AsioHostUnavailable => "asio_host_unavailable",
            Self::AsioUnsupportedBuild => "asio_unsupported_build",
        }
    }

    /// Phrase courte, dans la langue du chemin du signal — le serveur y écrit
    /// déjà ses `detail` en français (`runtime_signal_reason_detail`).
    pub fn detail(self) -> &'static str {
        match self {
            Self::AsioNoDevices => {
                "ASIO demandé : pilote présent mais aucune sortie exposée \
                 (une autre application le tient peut-être) — repli WASAPI"
            }
            Self::AsioHostUnavailable => {
                "ASIO demandé : pilote ASIO introuvable ou non ouvrable — repli WASAPI"
            }
            Self::AsioUnsupportedBuild => {
                "ASIO demandé : cette version du serveur n'embarque pas ASIO — \
                 sortie par le backend natif de la plateforme"
            }
        }
    }

    /// Toutes les variantes. Sert la contre-épreuve permanente : un motif
    /// ajouté sans être câblé fait tomber le test qui parcourt cette liste.
    pub const ALL: [Self; 3] = [
        Self::AsioNoDevices,
        Self::AsioHostUnavailable,
        Self::AsioUnsupportedBuild,
    ];
}

/// Pourquoi la zone ne joue pas sur le périphérique qu'elle NOMME.
///
/// Frère de [`LocalBackendFallback`], et volontairement bâti sur le même
/// modèle : un `code()` stable pour la machine, un `detail()` en clair pour un
/// écran sans table de traduction. Ce n'est pas un troisième canal — les deux
/// motifs voyagent dans le **même** [`LocalBackendStatus`], l'un sur le
/// backend, l'autre sur le périphérique.
///
/// #3230 — Jean Valjean règle sa zone sur « Haut-parleurs », un nom WASAPI.
/// `select_host("asio")` élit l'hôte ASIO dès qu'il expose une sortie, la
/// résolution cherche « Haut-parleurs » parmi les seules sorties ASIO, ne le
/// trouve pas, et ouvre **le périphérique ASIO par défaut**. Le son part
/// ailleurs, ou nulle part, et rien ne le dit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalDeviceFallback {
    /// Le nom mémorisé par la zone vient d'un AUTRE hôte que celui qui est
    /// ouvert. Aucun appariement n'est possible : un nom WASAPI ne désigne
    /// aucune sortie ASIO. La demande est **refusée**, pas détournée.
    ForeignHost,
    /// Le nom vient bien de cet hôte (ou son origine est inconnue) mais aucune
    /// sortie ne le porte plus : débranché, renommé, routage macOS changé.
    /// C'est le cas historique de #2207 — on ouvre le périphérique système et
    /// on le DIT.
    NotFoundFellBackToDefault,
}

impl LocalDeviceFallback {
    /// Code stable, celui que porte la charge utile JSON et les journaux.
    ///
    /// Il doit rester **identique** à la représentation `serde` de la variante,
    /// comme pour [`LocalBackendFallback`] : un client qui lit le JSON et un
    /// journal qui lit `code()` doivent parler du même motif. Le test
    /// `chaque_motif_de_repli_de_peripherique_est_cable` tient cette égalité.
    pub fn code(self) -> &'static str {
        match self {
            Self::ForeignHost => "foreign_host",
            Self::NotFoundFellBackToDefault => "not_found_fell_back_to_default",
        }
    }

    /// Phrase courte, dans la langue du chemin du signal.
    pub fn detail(self) -> &'static str {
        match self {
            Self::ForeignHost => {
                "le périphérique enregistré par la zone vient d'un autre hôte audio \
                 que celui qui est ouvert — rien n'a été ouvert plutôt que de jouer \
                 sur un périphérique que la zone n'a jamais désigné"
            }
            Self::NotFoundFellBackToDefault => {
                "le périphérique enregistré par la zone est introuvable \
                 (débranché, renommé) — lecture sur la sortie système"
            }
        }
    }

    /// Toutes les variantes. Sert la contre-épreuve permanente : un motif
    /// ajouté sans être câblé fait tomber le test qui parcourt cette liste.
    pub const ALL: [Self; 2] = [Self::ForeignHost, Self::NotFoundFellBackToDefault];
}

/// Ce que la sortie locale fait vraiment, à côté de ce qu'on lui a demandé.
///
/// Additif : `active` reprend exactement ce que rend [`active_backend_name`],
/// les deux autres champs sont nouveaux. Un client qui ne les lit pas voit le
/// même écran qu'avant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalBackendStatus {
    /// Backend réellement ouvert : `"ASIO"`, `"WASAPI"`, `"CoreAudio"`, `"ALSA"`.
    pub active: &'static str,
    /// Ce que le réglage demandait, normalisé en minuscules (`"asio"`, `"auto"`…).
    pub requested: String,
    /// `true` dès que l'actif ne correspond pas au demandé.
    pub fell_back: bool,
    /// Pourquoi, quand le serveur le sait. `None` = aucun repli constaté.
    pub fallback_reason: Option<LocalBackendFallback>,
    /// La même chose en clair, pour un écran qui n'a pas de table de traduction.
    pub fallback_detail: Option<&'static str>,
    /// Le PÉRIPHÉRIQUE réellement ouvert, face à celui qui était demandé.
    ///
    /// `None` = aucune ouverture observée depuis le démarrage (rien n'a encore
    /// joué en local), ou backend incapable de dire ce qu'il a ouvert. Absent
    /// plutôt que faux : c'est la seule réponse honnête.
    ///
    /// ⚠️ **À ne pas confondre avec `fell_back`**, qui parle du BACKEND
    /// (ASIO → WASAPI). Les deux replis sont indépendants : une zone peut
    /// tourner sur le backend demandé et sur un autre périphérique.
    pub device: Option<LocalDeviceStatus>,
    /// La CADENCE réellement ouverte, face à celle de la source (#3233).
    ///
    /// `None` = aucune ouverture partagée observée depuis le démarrage, ou
    /// sortie exclusive (qui n'arbitre pas). Troisième repli indépendant des
    /// deux autres : une zone peut jouer sur le bon backend, le bon
    /// périphérique, et à une autre cadence que la source.
    pub rate: Option<LocalRateStatus>,
}

/// Ce que la sortie locale a réellement OUVERT, face à ce que la zone
/// demandait — la moitié manquante de [`LocalBackendStatus`].
///
/// #2207 : le chemin exclusif WASAPI appelle `GetDefaultAudioEndpoint` dès que
/// la résolution par nom échoue, et le chemin cpal partagé retombe sur le
/// périphérique système. Une zone réglée sur un DAC peut donc jouer sur les
/// haut-parleurs. Le serveur le savait — deux accesseurs, une ligne de journal
/// — mais aucun écran ne pouvait le dire. **La zone doit dire la vérité, pas la
/// consigne.**
///
/// Ce type ne CORRIGE pas la résolution : il la rend visible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LocalDeviceStatus {
    /// Le backend qui a ouvert ce périphérique (`"WASAPI"`, `"ASIO"`,
    /// `"CoreAudio"`, `"ALSA"`).
    pub backend: &'static str,
    /// Le nom demandé au moment de l'ouverture. `"default"` = périphérique
    /// système, demandé explicitement — ce n'est pas un repli.
    pub requested: String,
    /// Le nom réellement ouvert, tel que le pilote le rend.
    ///
    /// **Vide** quand rien n'a été ouvert : c'est le cas d'un refus
    /// ([`LocalDeviceFallback::ForeignHost`]), où l'honnêteté impose de ne
    /// nommer aucun périphérique plutôt que d'en nommer un que la zone n'a
    /// jamais désigné. `reason` porte alors le pourquoi.
    pub opened: String,
    /// Identifiant d'endpoint quand le backend en expose un de stable (WASAPI,
    /// cpal). `None` pour ASIO et CoreAudio exclusif : ils n'en ont pas.
    pub opened_id: Option<String>,
    /// `true` dès que les deux noms diffèrent — c'est LE fait à montrer.
    pub differs: bool,
    /// Pourquoi la zone ne joue pas sur le périphérique qu'elle nomme.
    /// `None` = le périphérique demandé a bien été celui ouvert.
    ///
    /// Même vocabulaire que `fallback_reason` du backend : un code stable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<LocalDeviceFallback>,
    /// La même chose en clair, comme `fallback_detail` pour le backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<&'static str>,
}

impl LocalDeviceStatus {
    /// Un `"default"` demandé n'est jamais un écart : l'utilisateur a demandé
    /// « le périphérique système », il l'a eu. Partout ailleurs, deux noms
    /// différents sont un écart, même sans motif connu.
    fn from_observed(observed: ObservedDevice) -> Self {
        let differs = observed.reason.is_some()
            || (observed.requested != "default" && observed.requested != observed.opened);
        Self {
            backend: observed.backend,
            requested: observed.requested,
            opened: observed.opened,
            opened_id: observed.opened_id,
            differs,
            reason: observed.reason,
            detail: observed.reason.map(LocalDeviceFallback::detail),
        }
    }
}

/// Pourquoi la sortie locale partagée n'a **pas** ouvert à la cadence de la
/// source.
///
/// #3233 — Pierre M (fil 1043, 14/07/2026) : « DSD : le temps défile, pas de
/// son ». Un DSD64 décode à 176 400 Hz ; le chemin partagé ouvrait à cette
/// cadence dès que l'énumération de cpal la « retenait », sans regarder ce que
/// cette réponse valait. Sur WASAPI elle ne vaut rien (voir
/// [`sample_rate_evidence`]) : la liste est fabriquée, la branche était donc
/// toujours prise, `needs_resample` restait faux et rubato ne tournait jamais.
///
/// Les codes sont **stables** et destinés à la machine (le client les traduit),
/// exactement comme [`LocalBackendFallback`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalRateFallback {
    /// Le périphérique « retient » la cadence, mais **rien ne l'a vérifiée** :
    /// hôte dont l'énumération est fabriquée (WASAPI), ou PCM ALSA qui passe
    /// par un greffon rééchantillonneur. Tune refuse de fonder l'ouverture sur
    /// une capacité supposée et convertit lui-même.
    CapabilitiesUnverified,
    /// Le périphérique ne retient pas la cadence : l'écart est constaté, pas
    /// supposé. C'est le comportement de toujours, nommé.
    RateNotSupported,
}

impl LocalRateFallback {
    /// Code stable, celui que porte la charge utile JSON et les journaux.
    pub fn code(self) -> &'static str {
        match self {
            Self::CapabilitiesUnverified => "capabilities_unverified",
            Self::RateNotSupported => "rate_not_supported",
        }
    }

    /// Phrase courte, dans la langue du chemin du signal — le serveur y écrit
    /// déjà ses `detail` en français (`runtime_signal_reason_detail`).
    pub fn detail(self) -> &'static str {
        match self {
            Self::CapabilitiesUnverified => {
                "Cadence source annoncée par le périphérique mais jamais vérifiée : \
                 ouverture à la cadence du périphérique et rééchantillonnage par Tune"
            }
            Self::RateNotSupported => {
                "Cadence source non retenue par le périphérique : ouverture à la \
                 cadence du périphérique et rééchantillonnage par Tune"
            }
        }
    }

    /// Toutes les variantes. Sert la contre-épreuve permanente : un motif
    /// ajouté sans être câblé fait tomber le test qui parcourt cette liste.
    pub const ALL: [Self; 2] = [Self::CapabilitiesUnverified, Self::RateNotSupported];
}

/// À quelle cadence la sortie locale partagée a réellement ouvert, face à celle
/// de la source — et pourquoi, quand les deux diffèrent.
///
/// Additif, comme [`LocalDeviceStatus`] : un client qui ne lit pas ce champ voit
/// le même écran qu'avant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LocalRateStatus {
    /// Cadence du flux décodé (176 400 Hz pour un DSD64, 352 800 pour un
    /// DSD128/256/512).
    pub source_sample_rate: u32,
    /// Cadence à laquelle le flux cpal a été ouvert.
    pub opened_sample_rate: u32,
    /// `true` dès que les deux diffèrent : Tune convertit, ce n'est plus le
    /// train d'échantillons de la source qui part au DAC.
    pub resampled: bool,
    /// Pourquoi, quand la conversion est une DÉCISION de Tune. `None` = aucune
    /// conversion, ou aucune décision à justifier.
    pub reason: Option<LocalRateFallback>,
    /// La même chose en clair, pour un écran sans table de traduction.
    pub detail: Option<&'static str>,
    /// La liste de cadences sur laquelle la décision s'est appuyée avait-elle
    /// été **mesurée** ? Faux sur WASAPI et sur les greffons ALSA (#2862,
    /// #1655). C'est le fait qui distingue les deux motifs.
    pub evidence_measured: bool,
}

impl LocalRateStatus {
    fn from_observed(observed: ObservedRate) -> Self {
        Self {
            source_sample_rate: observed.source_sample_rate,
            opened_sample_rate: observed.opened_sample_rate,
            resampled: observed.opened_sample_rate != observed.source_sample_rate,
            reason: observed.reason,
            detail: observed.reason.map(LocalRateFallback::detail),
            evidence_measured: observed.evidence_measured,
        }
    }
}

/// Enregistre la cadence réellement ouverte par le chemin cpal **partagé**.
///
/// Appelé une fois par ouverture, juste après [`decide_local_rate_opening`] :
/// la décision et sa trace ne se séparent pas.
fn note_rate_decision(observed: ObservedRate) {
    if let Ok(mut slot) = OBSERVED_RATE.write() {
        *slot = Some(observed);
    }
}

/// Enregistre le backend réellement ouvert, et le motif du repli s'il y en a un.
/// Appelé par `select_host` seul, sur **toutes** les cibles.
fn note_observed_backend(name: &'static str, fallback_reason: Option<LocalBackendFallback>) {
    if let Ok(mut slot) = OBSERVED_BACKEND.write() {
        *slot = Some(ObservedBackend {
            name,
            fallback_reason,
        });
    }
}

/// Issue d'une demande `asio` sur une cible qui embarque ASIO.
///
/// `asio_devices` : `None` = l'hôte ASIO ne s'ouvre pas ; `Some(0)` = il
/// s'ouvre mais n'expose aucune sortie ; `Some(n > 0)` = ASIO joue.
///
/// Isolée de cpal exprès : la branche appelante vit sous
/// `#[cfg(all(target_os = "windows", feature = "asio"))]` et ne peut être
/// exécutée ni sur macOS ni sur Linux. La décision, elle, se joue partout.
#[cfg_attr(not(all(target_os = "windows", feature = "asio")), allow(dead_code))]
fn asio_outcome(asio_devices: Option<usize>) -> (&'static str, Option<LocalBackendFallback>) {
    match asio_devices {
        Some(n) if n > 0 => ("ASIO", None),
        Some(_) => ("WASAPI", Some(LocalBackendFallback::AsioNoDevices)),
        None => ("WASAPI", Some(LocalBackendFallback::AsioHostUnavailable)),
    }
}

/// Issue d'une demande sur une cible qui n'embarque **pas** ASIO.
#[cfg_attr(all(target_os = "windows", feature = "asio"), allow(dead_code))]
fn unsupported_outcome(requested_lower: &str) -> (&'static str, Option<LocalBackendFallback>) {
    let active = platform_default_backend_name();
    if requested_lower == "asio" {
        (active, Some(LocalBackendFallback::AsioUnsupportedBuild))
    } else {
        (active, None)
    }
}

/// Le backend qu'ouvre `cpal::default_host()` sur cette plateforme.
///
/// Sur Windows+ASIO, seul [`unsupported_outcome`] — lui-même inerte sur cette
/// cible — s'en sert : d'où le `dead_code` autorisé plutôt qu'un `cfg` de plus.
#[cfg_attr(all(target_os = "windows", feature = "asio"), allow(dead_code))]
fn platform_default_backend_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "WASAPI"
    }
    #[cfg(target_os = "macos")]
    {
        "CoreAudio"
    }
    #[cfg(target_os = "linux")]
    {
        "ALSA"
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        "default"
    }
}

/// Nom du backend audio à afficher.
///
/// Ce qui a été *observé* prime sur ce qui a été *demandé* : c'est la seule
/// réponse qui corresponde à ce que l'utilisateur entend réellement.
pub fn active_backend_name(backend: &str) -> &'static str {
    backend_display_name(
        OBSERVED_BACKEND
            .read()
            .ok()
            .and_then(|g| *g)
            .map(|o| o.name),
        backend,
    )
}

/// Ce que la sortie locale fait, ce qu'on lui a demandé, et l'écart s'il existe.
///
/// C'est la réponse à #1395 : `active_backend_name` disait déjà la vérité sur le
/// backend, mais un utilisateur qui lit « WASAPI » alors qu'il a réglé « ASIO »
/// n'a toujours aucun moyen de savoir s'il s'est trompé de réglage ou si le
/// serveur a basculé — ni pourquoi.
pub fn active_backend_status(requested: &str) -> LocalBackendStatus {
    backend_status_with_rate(
        OBSERVED_BACKEND.read().ok().and_then(|g| *g),
        OBSERVED_DEVICE.read().ok().and_then(|g| g.clone()),
        OBSERVED_RATE.read().ok().and_then(|g| *g),
        requested,
    )
}

/// Règle d'arbitrage entre observé et demandé, isolée pour être testable sans
/// toucher à l'état global ni ouvrir un périphérique.
fn backend_display_name(observed: Option<&'static str>, backend: &str) -> &'static str {
    if let Some(observed) = observed {
        return observed;
    }
    #[cfg(all(target_os = "windows", feature = "asio"))]
    {
        match backend.to_lowercase().as_str() {
            "asio" => "ASIO",
            _ => "WASAPI",
        }
    }
    #[cfg(not(all(target_os = "windows", feature = "asio")))]
    {
        let _ = backend;
        platform_default_backend_name()
    }
}

/// Même isolement pour le statut complet : aucune lecture de l'état global,
/// aucun périphérique ouvert, donc jouable sur n'importe quelle plateforme.
///
/// Raccourci des tests de la famille #1395, qui n'ont rien à dire de la
/// cadence : c'est [`backend_status_with_rate`] sans observation de cadence.
#[cfg(test)]
fn backend_status(
    observed: Option<ObservedBackend>,
    observed_device: Option<ObservedDevice>,
    requested: &str,
) -> LocalBackendStatus {
    backend_status_with_rate(observed, observed_device, None, requested)
}

/// Même isolement pour le statut complet : aucune lecture de l'état global,
/// aucun périphérique ouvert, donc jouable sur n'importe quelle plateforme.
fn backend_status_with_rate(
    observed: Option<ObservedBackend>,
    observed_device: Option<ObservedDevice>,
    observed_rate: Option<ObservedRate>,
    requested: &str,
) -> LocalBackendStatus {
    let requested_lower = requested.to_lowercase();
    let active = backend_display_name(observed.map(|o| o.name), requested);

    let fallback_reason = match observed {
        // Une OBSERVATION est autoritaire, y compris quand elle ne porte aucun
        // motif : `select_host` a ouvert un périphérique et sait ce qu'il a
        // ouvert. Retomber sur la déduction ici rajouterait un motif à un
        // backend qui joue — la faute exactement inverse de celle qu'on
        // corrige, et c'est ce test qui l'a attrapée.
        Some(o) => o.fallback_reason,
        // Sans observation, un seul motif est affirmable, parce qu'il est
        // décidé à la COMPILATION : un binaire sans ASIO ne pourra jamais
        // honorer « asio ». On n'en devine aucun autre.
        None => (requested_lower == "asio" && !asio_available())
            .then_some(LocalBackendFallback::AsioUnsupportedBuild),
    };

    // L'écart se voit sans motif : un réglage sur « asio » et un actif
    // « WASAPI » suffisent à le dire, même quand la cause n'est pas connue
    // (réglage changé, flux pas encore rouvert).
    let fell_back = fallback_reason.is_some()
        || !(requested_lower == "auto"
            || requested_lower.is_empty()
            || requested_lower.eq_ignore_ascii_case(active));

    LocalBackendStatus {
        active,
        requested: requested_lower,
        fell_back,
        fallback_reason,
        fallback_detail: fallback_reason.map(LocalBackendFallback::detail),
        device: observed_device.map(LocalDeviceStatus::from_observed),
        rate: observed_rate.map(LocalRateStatus::from_observed),
    }
}

/// Returns `true` if this build includes ASIO support.
pub fn asio_available() -> bool {
    cfg!(all(target_os = "windows", feature = "asio"))
}

/// Un choix de backend audio local, tel que le sélecteur de l'interface doit
/// le proposer : la valeur à persister dans `local_audio_backend`, et un
/// libellé technique (des noms propres — pas de traduction à faire côté
/// client, hormis « Auto »).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BackendChoice {
    pub value: &'static str,
    pub label: &'static str,
}

/// Les backends de sortie locale réellement sélectionnables sur CETTE machine.
///
/// #1268 ([Forum HiFi], Lapinou sous Debian puis Benjithom sous Fedora) : le
/// sélecteur « Backend audio » du client web proposait Auto/WASAPI/ASIO — deux
/// technologies Windows — parce que ces trois `<option>` étaient écrites en
/// dur et que le serveur n'exposait nulle part la liste vraie. La voici,
/// calculée à la compilation par plateforme, pour que l'interface n'ait plus
/// rien à deviner.
///
/// `auto` est toujours présent et toujours premier : c'est le défaut, et c'est
/// aussi le repli de [`select_host`] pour toute valeur inconnue — y compris
/// une valeur Windows persistée avant qu'une bibliothèque ne migre vers une
/// machine Linux.
pub fn supported_backends() -> &'static [BackendChoice] {
    #[cfg(all(target_os = "windows", feature = "asio"))]
    {
        &[
            BackendChoice {
                value: "auto",
                label: "Auto (WASAPI)",
            },
            BackendChoice {
                value: "wasapi",
                label: "WASAPI",
            },
            BackendChoice {
                value: "asio",
                label: "ASIO (bit-perfect)",
            },
        ]
    }
    #[cfg(all(target_os = "windows", not(feature = "asio")))]
    {
        &[
            BackendChoice {
                value: "auto",
                label: "Auto (WASAPI)",
            },
            BackendChoice {
                value: "wasapi",
                label: "WASAPI",
            },
        ]
    }
    #[cfg(target_os = "macos")]
    {
        &[BackendChoice {
            value: "auto",
            label: "Auto (CoreAudio)",
        }]
    }
    #[cfg(target_os = "linux")]
    {
        &[BackendChoice {
            value: "auto",
            label: "Auto (ALSA)",
        }]
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        &[BackendChoice {
            value: "auto",
            label: "Auto",
        }]
    }
}

/// Cette valeur de `local_audio_backend` correspond-elle à un backend
/// sélectionnable sur cette machine ? Sert au repli d'affichage : une valeur
/// Windows persistée sur un serveur Linux ne doit pas laisser le sélecteur
/// sur un choix qui n'existe plus ([`select_host`] jouera de toute façon via
/// le host par défaut de la plateforme).
pub fn backend_value_is_supported(value: &str) -> bool {
    supported_backends()
        .iter()
        .any(|b| b.value.eq_ignore_ascii_case(value))
}

/// List ASIO audio output devices specifically.
///
/// On Windows with the `asio` feature enabled, this enumerates devices using
/// the ASIO host (bypassing WASAPI).  On other platforms or without the `asio`
/// feature, returns an empty list.
///
/// Each returned `AsioDeviceInfo` includes the driver name, supported sample
/// rates, max channels, and whether it's the default ASIO device.
pub fn list_asio_devices() -> Vec<AsioDeviceInfo> {
    #[cfg(all(target_os = "windows", feature = "asio"))]
    {
        use std::sync::Mutex as StdMutex;

        // Last successful enumeration. Served verbatim while an exclusive stream
        // owns the ASIO device, so listing never re-opens a driver that is
        // already locked for playback.
        static ASIO_DEVICE_CACHE: StdMutex<Option<Vec<AsioDeviceInfo>>> = StdMutex::new(None);

        let enumerate = || -> Vec<AsioDeviceInfo> {
            super::asio_exclusive::ensure_com_initialized();
            let host = match cpal::host_from_id(cpal::HostId::Asio) {
                Ok(h) => h,
                Err(e) => {
                    warn!(error = %e, "asio_device_enumeration_failed — no ASIO host available");
                    return Vec::new();
                }
            };

            let default_name = host
                .default_output_device()
                .and_then(|d| d.description().ok())
                .map(|desc| desc.name().to_string())
                .unwrap_or_default();

            let mut devices = Vec::new();
            match host.output_devices() {
                Ok(output_devices) => {
                    for device in output_devices {
                        let name = device
                            .description()
                            .map(|desc| desc.name().to_string())
                            .unwrap_or_else(|_| "Unknown ASIO Device".into());

                        let is_default = name == default_name;

                        let (max_channels, sample_rates) = match device.supported_output_configs() {
                            Ok(configs) => {
                                let mut max_ch = 0u16;
                                let mut rates = Vec::new();
                                for config in configs {
                                    max_ch = max_ch.max(config.channels());
                                    let min = config.min_sample_rate();
                                    let max = config.max_sample_rate();
                                    for &rate in &[
                                        44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000,
                                        705600, 768000,
                                    ] {
                                        if rate >= min && rate <= max && !rates.contains(&rate) {
                                            rates.push(rate);
                                        }
                                    }
                                }
                                rates.sort();
                                (max_ch, rates)
                            }
                            Err(_) => {
                                // ASIO drivers usually enumerate correctly, but fall
                                // back to conservative defaults if they don't.
                                (2, vec![44100, 48000, 96000, 192000])
                            }
                        };

                        info!(
                            name = %name,
                            is_default,
                            max_channels,
                            sample_rates = ?sample_rates,
                            "asio_device_found"
                        );

                        devices.push(AsioDeviceInfo {
                            name,
                            is_default,
                            max_channels,
                            sample_rates,
                            exclusive: true, // ASIO is always exclusive
                        });
                    }
                }
                Err(e) => {
                    warn!(error = %e, "asio_output_devices_enumeration_failed");
                }
            }

            devices
        };

        // Probe the driver ONLY when no exclusive stream currently owns it.
        // Re-opening the single-instance ASIO driver while a zone is playing
        // churns it — on SOtM Diretta it never finishes locking (endless
        // connect → getBufferSize → disconnect cycles, never reaching
        // createBuffers/start). When the device is busy, serve the cache.
        match super::asio_exclusive::try_with_asio_device_lock(enumerate) {
            Some(devices) => {
                *ASIO_DEVICE_CACHE.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(devices.clone());
                devices
            }
            None => {
                let cached = ASIO_DEVICE_CACHE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .unwrap_or_default();
                debug!(
                    cached_devices = cached.len(),
                    "asio_device_enumeration_skipped_playback_active"
                );
                cached
            }
        }
    }

    #[cfg(not(all(target_os = "windows", feature = "asio")))]
    {
        Vec::new()
    }
}

/// Information about an ASIO audio device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AsioDeviceInfo {
    /// ASIO driver name (e.g. "RME Babyface Pro FS ASIO").
    pub name: String,
    /// Whether this is the default ASIO output device.
    pub is_default: bool,
    /// Maximum number of output channels supported.
    pub max_channels: u16,
    /// Supported sample rates (Hz).
    pub sample_rates: Vec<u32>,
    /// ASIO devices are always in exclusive mode.
    pub exclusive: bool,
}

// ---------------------------------------------------------------------------
// Device enumeration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioDevice {
    pub name: String,
    /// Stable backend endpoint identifier (for example the IMMDevice ID on
    /// WASAPI), captured during discovery and reused when playback opens.
    #[serde(default)]
    pub endpoint_id: String,
    pub is_default: bool,
    pub max_channels: u16,
    pub sample_rates: Vec<u32>,
    /// `sample_rates` a-t-il été confronté au matériel ?
    ///
    /// Faux sur WASAPI, où cpal fabrique la liste sans rien demander au pilote
    /// (#2862) : l'écran ne doit pas présenter ces cadences comme une capacité
    /// constatée. Voir [`sample_rate_evidence`].
    ///
    /// `serde(default)` rend `true` : les enregistrements écrits avant ce champ
    /// ne peuvent plus être requalifiés, et le champ n'est de toute façon
    /// jamais persisté — il n'existe que sur le fil de `GET
    /// /api/v1/devices/audio`.
    #[serde(default = "sample_rates_measured_default")]
    pub sample_rates_measured: bool,
    /// The audio backend this device was enumerated from.
    #[serde(default)]
    pub backend: String,
    /// De quoi distinguer deux sorties qui portent le MÊME nom (#2272).
    ///
    /// Marco Polo voit deux « Haut-Parleurs » et ne peut pas dire lequel est
    /// lequel. Le suffixe `(2)` que pose [`disambiguate_display_name`] est un
    /// rang d'énumération, pas une identité : il peut changer d'un démarrage à
    /// l'autre, et il ne nomme rien. Ce champ porte le nom du CONTRÔLEUR
    /// derrière la sortie — « Topping D10s », « Realtek High Definition
    /// Audio » — c'est-à-dire ce qu'Audirvana affiche et que Tune jetait.
    ///
    /// `None` quand rien de distinctif n'est disponible, et `None` est alors
    /// ABSENT de la charge utile (`skip_serializing_if`) plutôt que publié
    /// comme chaîne vide : un renseignement manquant ne doit pas se faire
    /// passer pour un renseignement.
    ///
    /// **Ce champ ne remplace pas `name` et ne le modifie pas.** Le nom
    /// d'affichage reste mot pour mot celui d'avant, suffixe `(n)` compris,
    /// parce que c'est LUI que les zones ont mémorisé et que [`resolve_device`]
    /// le reconstruit à l'identique (étape 2, via
    /// [`disambiguate_display_name`]). Renommer les périphériques renverrait
    /// toutes les zones existantes sur `NotFound` — le défaut que Jean Marie a
    /// vécu sur macOS (#3185). L'écran compose ; le serveur ne renomme pas.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hardware_detail: Option<String>,
}

fn sample_rates_measured_default() -> bool {
    true
}

/// Le renseignement qui distingue deux sorties homonymes, ou rien (#2272).
///
/// ## Pourquoi la règle reçoit tout en paramètres
///
/// Les trois plateformes ne rapportent pas la même chose, et deux d'entre
/// elles ne se compilent pas sur la machine de compilation. La RÈGLE est donc
/// une fonction pure, éprouvable partout. La COLLECTE, elle, n'a même pas
/// besoin d'un `cfg` : cpal 0.17 la fait déjà, dans le `DeviceDescription` que
/// [`list_audio_devices_uncached`] obtenait puis jetait après n'en avoir lu
/// que le seul `name()`.
///
/// ## Ce que chaque plateforme met dans ces paramètres
///
/// - **Windows / WASAPI** — `driver` porte
///   `DEVPKEY_DeviceInterface_FriendlyName`, que cpal lit lui-même
///   (`host/wasapi/device.rs`, `builder.driver(iface_name)`). C'est exactement
///   la propriété que réclame #2272 : le nom du contrôleur, « Topping D10s ».
///   Et c'est bien là que le défaut mord, parce que cpal choisit
///   `DEVPKEY_Device_DeviceDesc` comme `name` — « Haut-Parleurs », générique
///   par construction, identique pour deux DAC différents.
/// - **Linux / ALSA** — `driver` porte le PCM (`hw:CARD=…`), que `endpoint_id`
///   porte DÉJÀ. Il ne distingue rien de plus, et la règle l'écarte : Linux
///   retombe mot pour mot sur le comportement d'avant, dédoublonnage PipeWire
///   compris.
/// - **macOS / CoreAudio** — cpal 0.17.3 ne renseigne ni `manufacturer` ni
///   `driver` (`host/coreaudio/macos/device.rs::description` ne pose que le
///   nom, la direction et le cas `Aggregate`). La règle rend `None` sans rien
///   casser. `kAudioDevicePropertyModelUID` reste donc à collecter.
///
/// `manufacturer` passe avant `driver` : aucun backend de cpal 0.17.3 ne le
/// renseigne aujourd'hui — `grep manufacturer src/host/` ne rend rien — mais
/// c'est le champ dont la sémantique est exactement celle qu'on cherche, et le
/// jour où un backend le remplit il doit gagner sans qu'on y revienne.
///
/// ## Les deux motifs de refus
///
/// 1. **Vide.** Une chaîne blanche n'est pas un renseignement.
/// 2. **Déjà connu de l'appelant.** Un candidat que le nom d'affichage ou
///    l'identifiant d'endpoint contient déjà n'ajoute rien. C'est ce qui écarte
///    le PCM ALSA, et ce qui empêche d'écrire « Haut-Parleurs » à côté de
///    « Haut-Parleurs ».
pub fn hardware_detail(
    manufacturer: Option<&str>,
    driver: Option<&str>,
    display_name: &str,
    endpoint_id: &str,
) -> Option<String> {
    let deja_connu = |candidat: &str| {
        let candidat = candidat.to_lowercase();
        display_name.to_lowercase().contains(&candidat)
            || endpoint_id.to_lowercase().contains(&candidat)
    };
    [manufacturer, driver]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|candidat| !candidat.is_empty() && !deja_connu(candidat))
        .map(str::to_string)
}

/// La même règle, branchée sur ce que cpal rend.
///
/// Un SEUL endroit lit un `DeviceDescription` pour cette question, et il reste
/// éprouvable sans matériel : `cpal::DeviceDescriptionBuilder` est public, si
/// bien que les épreuves fabriquent mot pour mot les descriptions que WASAPI,
/// ALSA et CoreAudio rendent.
fn hardware_detail_from_description(
    description: &cpal::DeviceDescription,
    display_name: &str,
    endpoint_id: &str,
) -> Option<String> {
    hardware_detail(
        description.manufacturer(),
        description.driver(),
        display_name,
        endpoint_id,
    )
}

/// Une variante ALSA d'un même nom de périphérique, telle que l'énumération la
/// rend.
///
/// Le NOM n'est pas un champ : c'est la clef de regroupement, identique pour
/// tous les membres d'un groupe. La règle ne le lit jamais — elle départage des
/// variantes dont on sait déjà qu'elles portent le même nom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlsaVariant {
    /// Le PCM ALSA : `hw:CARD=X,DEV=0`, `sysdefault:CARD=X`, `dmix:CARD=X`…
    /// C'est la SEULE chose qui distingue ces variantes entre elles.
    pub endpoint_id: String,
    /// Voies annoncées par l'énumération — pas forcément par le matériel.
    pub max_channels: u16,
    /// Cadences annoncées par l'énumération — pas forcément par le matériel.
    pub sample_rates: Vec<u32>,
}

/// Le candidat doit-il remplacer la variante retenue ? (#3209, #1655)
///
/// ## Pourquoi « la plus riche » était le défaut lui-même
///
/// `snd_device_name_hint` expose une carte sous une dizaine de PCM qui
/// partagent tous la même première ligne de description — c'est ce qui force le
/// regroupement par nom. Seul `hw:` atteint le pilote ; tous les autres passent
/// par un greffon (`plug`, `dmix`, `sysdefault`, `front`…) qui **accepte tout**.
///
/// Interroger un greffon cadence par cadence rend donc « oui » partout, et voie
/// par voie jusqu'à 32 pour un DAC stéréo. Le greffon annonçait ainsi des
/// capacités **plus riches que le matériel**, gagnait le départage, et imposait
/// son identité : Tune publiait « 44,1 → 384 kHz mesurées » puis ouvrait un
/// `dmix` verrouillé à 48 kHz (`defaults.pcm.dmix.rate 48000`). Un FLAC 44,1
/// était rééchantillonné en silence (GgB, Eversolo DAC-Z8 sous Fedora, #1655 ;
/// audit #3209 : « rien ne guide vers `hw:` »).
///
/// **Une capacité annoncée par un greffon n'est pas une capacité mesurée, et ne
/// doit jamais gagner un départage contre le matériel.**
///
/// ## L'ordre total appliqué
///
/// 1. **Le PCM matériel d'abord**, quelles que soient les capacités annoncées.
/// 2. À classe égale seulement, la variante la plus riche (voies, puis nombre
///    de cadences) — le comportement d'avant, intact.
/// 3. À capacités égales, le `pcm_id` le plus petit. Sans ce dernier cran, la
///    variante retenue serait la **première énumérée**, donc dépendante de
///    l'ordre d'alsa-lib.
///
/// Ces trois critères forment un ordre total : le vainqueur ne dépend pas de
/// l'ordre du parcours.
///
/// ## Ce que cette règle ne change PAS
///
/// Elle ne change ni le nombre de périphériques publiés (le regroupement par
/// nom reste entier — 43 fantômes → 48 zones chez JeromeQ, Ubuntu 24.04), ni
/// le périphérique d'une zone déjà configurée : la résolution
/// ([`resolve_device`]) travaille sur la liste BRUTE de `output_devices()`,
/// jamais sur cette liste fusionnée, et apparie d'abord par `endpoint_id`. Une
/// zone qui a mémorisé `sysdefault:…` continue donc d'ouvrir `sysdefault:…`.
/// Seules les zones créées ensuite héritent du PCM matériel.
///
/// Quand aucune variante du groupe n'est un `hw:` — le cas d'une machine où
/// PipeWire est le seul chemin praticable — le critère 1 ne départage rien et
/// le comportement d'avant s'applique mot pour mot.
fn variante_alsa_candidate_l_emporte(retenue: &AlsaVariant, candidate: &AlsaVariant) -> bool {
    let retenue_materielle = alsa_pcm_is_direct_hardware(&retenue.endpoint_id);
    let candidate_materielle = alsa_pcm_is_direct_hardware(&candidate.endpoint_id);
    if candidate_materielle != retenue_materielle {
        return candidate_materielle;
    }
    if candidate.max_channels != retenue.max_channels {
        return candidate.max_channels > retenue.max_channels;
    }
    if candidate.sample_rates.len() != retenue.sample_rates.len() {
        return candidate.sample_rates.len() > retenue.sample_rates.len();
    }
    candidate.endpoint_id < retenue.endpoint_id
}

/// Laquelle de ces variantes homonymes doit être retenue ? Indice, ou `None`
/// si la liste est vide.
///
/// Fonction PURE : aucun appel à alsa-lib, aucun périphérique, aucune variable
/// d'environnement. Le `cfg` et l'interrogation du pilote restent du câblage,
/// sur le patron de `resolve_local_audio_backend` — pour que la règle soit
/// vérifiable sans matériel. Voir `variante_alsa_candidate_l_emporte` pour
/// l'ordre appliqué et ce qu'il ne change pas.
pub fn retenir_variante_alsa(variantes: &[AlsaVariant]) -> Option<usize> {
    let mut gagnante: Option<usize> = None;
    for (index, variante) in variantes.iter().enumerate() {
        match gagnante {
            None => gagnante = Some(index),
            Some(courante) => {
                if variante_alsa_candidate_l_emporte(&variantes[courante], variante) {
                    gagnante = Some(index);
                }
            }
        }
    }
    gagnante
}

/// Regroupe deux variantes Linux qui représentent le même nom de périphérique.
///
/// PipeWire/ALSA peut exposer plusieurs entrées homonymes avec des capacités
/// différentes. La variante retenue doit rester un tout : son identité, ses
/// capacités **et ce que vaut la liste de cadences** ne peuvent pas provenir de
/// trois entrées différentes.
///
/// Le départage lui-même est délégué à [`variante_alsa_candidate_l_emporte`] —
/// une seule règle, éprouvable sans matériel.
#[cfg(any(target_os = "linux", test))]
fn merge_linux_duplicate_variant(
    existing: &mut AudioDevice,
    candidate_endpoint_id: String,
    candidate_is_default: bool,
    candidate_max_channels: u16,
    candidate_sample_rates: Vec<u32>,
    candidate_sample_rates_measured: bool,
    candidate_hardware_detail: Option<String>,
) -> bool {
    let retenue = AlsaVariant {
        endpoint_id: existing.endpoint_id.clone(),
        max_channels: existing.max_channels,
        sample_rates: existing.sample_rates.clone(),
    };
    let candidate = AlsaVariant {
        endpoint_id: candidate_endpoint_id,
        max_channels: candidate_max_channels,
        sample_rates: candidate_sample_rates,
    };
    let bascule = variante_alsa_candidate_l_emporte(&retenue, &candidate);
    if bascule {
        // L'identité bascule avec les capacités. Conserver l'endpoint de la
        // première variante ferait rouvrir en lecture un autre périphérique
        // que celui dont on vient de publier les capacités.
        existing.endpoint_id = candidate.endpoint_id;
        existing.max_channels = candidate.max_channels;
        existing.sample_rates = candidate.sample_rates;
        // Et la PREUVE bascule avec elles : `sample_rates_measured` avait été
        // calculé pour l'endpoint de la première variante. Le laisser en place
        // faisait présenter les cadences d'un `hw:` comme non mesurées — ou,
        // pire, celles d'un `dmix:` comme mesurées (#1655).
        existing.sample_rates_measured = candidate_sample_rates_measured;
        // Et le renseignement matériel avec elles (#2272) : il désigne le
        // contrôleur de l'endpoint retenu. Le laisser en arrière l'accrocherait
        // au greffon qu'on vient précisément d'écarter.
        existing.hardware_detail = candidate_hardware_detail;
    }
    if candidate_is_default {
        existing.is_default = true;
    }
    bascule
}

static SCAN_GUARD: std::sync::Mutex<Option<(std::time::Instant, Vec<AudioDevice>)>> =
    std::sync::Mutex::new(None);
const SCAN_COOLDOWN_SECS: u64 = 5;

/// List audio devices using the default host.
pub fn list_audio_devices() -> Vec<AudioDevice> {
    list_audio_devices_with_backend("auto")
}

/// Ce que doit faire une énumération de périphériques quand le pilote ASIO —
/// qui ne s'ouvre qu'UNE fois, tous processus confondus — est déjà pris.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsioEnumerationPlan {
    /// Interroger le matériel : aucun pilote ASIO n'est en jeu, ou il est libre.
    Probe,
    /// Servir le dernier inventaire connu sans toucher au pilote.
    ServeCache,
}

/// #1267 — l'énumération générique doit-elle s'écarter du pilote ASIO ?
///
/// Le pilote ASIO ne supporte qu'un seul ouvreur. Le rouvrir pour DRESSER LA
/// LISTE pendant qu'une session exclusive tente de le verrouiller le fait
/// tourner en rond — `connect → getBufferSize → disconnect`, sans jamais
/// atteindre `createBuffers`/`start` : la sortie ne se verrouille JAMAIS.
/// C'est le symptôme rapporté par `zaurux` sur la sortie Diretta ASIO, et la
/// panne déjà observée sur le Diretta SOtM.
///
/// [`list_asio_devices`] se gardait déjà (cf. `try_with_asio_device_lock`).
/// L'autre porte, celle-ci, ne se gardait pas — et c'est elle qu'empruntent la
/// page Diagnostic, `/devices/audio` et le rescan à chaud. La page Diagnostic
/// est précisément celle qu'on ouvre quand la sortie refuse de se verrouiller :
/// elle rouvrait le pilote et entretenait la panne qu'elle devait documenter.
///
/// Seule la valeur `asio` ouvre le host ASIO : `auto` passe par WASAPI (cf.
/// [`select_host`]), et toute autre valeur également.
pub fn plan_audio_enumeration(backend: &str, asio_device_busy: bool) -> AsioEnumerationPlan {
    if asio_device_busy && backend.eq_ignore_ascii_case("asio") {
        AsioEnumerationPlan::ServeCache
    } else {
        AsioEnumerationPlan::Probe
    }
}

/// Une session de lecture exclusive tient-elle le pilote ASIO ?
///
/// Toujours `false` là où il n'y a pas d'ASIO : macOS, Linux, et Windows
/// compilé sans la fonctionnalité `asio`.
fn asio_device_busy() -> bool {
    #[cfg(all(target_os = "windows", feature = "asio"))]
    {
        super::asio_exclusive::asio_device_is_busy()
    }
    #[cfg(not(all(target_os = "windows", feature = "asio")))]
    {
        false
    }
}

/// List audio devices using the specified backend preference.
/// Protected by a global Mutex + 5s cache to prevent concurrent ASIO
/// driver enumeration which crashes on Windows (non-reentrant COM STA).
pub fn list_audio_devices_with_backend(backend: &str) -> Vec<AudioDevice> {
    // Avant tout : ne pas rouvrir un pilote ASIO qu'une lecture exclusive est
    // en train de verrouiller (#1267). Le cooldown de 5 s ci-dessous ne suffit
    // pas — passé ce délai il relance un balayage complet en pleine session.
    if plan_audio_enumeration(backend, asio_device_busy()) == AsioEnumerationPlan::ServeCache {
        debug!(
            backend = %backend,
            "local_audio_enumeration_skipped_asio_device_busy"
        );
        return cached_audio_devices();
    }
    let mut guard = SCAN_GUARD.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((last_scan, ref cached)) = *guard {
        if last_scan.elapsed().as_secs() < SCAN_COOLDOWN_SECS {
            debug!("local_audio_scan_cached");
            return cached.clone();
        }
    }
    let result = list_audio_devices_uncached(backend);
    *guard = Some((std::time::Instant::now(), result.clone()));
    result
}

/// Return the last cached device list WITHOUT triggering a fresh enumeration.
///
/// Enumerating WASAPI devices probes each device's supported formats, which can
/// invalidate an active render stream and kill playback on Windows (DEvir). So
/// while a local stream is playing we serve this cache instead of re-scanning.
/// Returns an empty list if nothing has been enumerated yet this session.
pub fn cached_audio_devices() -> Vec<AudioDevice> {
    SCAN_GUARD
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|(_, devices)| devices.clone())
        .unwrap_or_default()
}

fn list_audio_devices_uncached(backend: &str) -> Vec<AudioDevice> {
    let host = select_host(backend);
    let host_name = host.id().name();
    let default_name = host
        .default_output_device()
        .and_then(|d| d.description().ok())
        .map(|desc| desc.name().to_string())
        .unwrap_or_default();

    info!(
        host = %host_name,
        default_device = %default_name,
        "local_audio_enumerating_devices"
    );

    let mut devices: Vec<AudioDevice> = Vec::new();
    let mut seen_names = std::collections::HashSet::new();
    // Signature = (raw name, caps). Windows WASAPI can list the same physical
    // endpoint (onboard "HDA ..." codecs) more than once with an identical name
    // AND identical capabilities; those true duplicates are collapsed so they
    // don't spawn a phantom second zone (Elie).
    // On Linux, PipeWire re-exposes the SAME physical output many times with
    // *different* reported capabilities (e.g. "ALC255 Analog" as 2ch/48k, then
    // 32ch/384k, then a stereo fallback), so the (name, caps) signature above
    // never collapses them and each variant becomes a phantom zone (JeromeQ:
    // 43 devices → 48 zones on Ubuntu 24.04). Collapse by NAME instead, keeping
    // the richest-capability variant. Maps raw device name → index into `devices`.
    #[cfg(target_os = "linux")]
    let mut linux_by_name: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    match host.output_devices() {
        Ok(output_devices) => {
            for device in output_devices {
                let description = device.description().ok();
                let raw_name = description
                    .as_ref()
                    .map(|desc| desc.name().to_string())
                    .unwrap_or_else(|| "Unknown".into());
                let endpoint_id = device.id().map(|id| id.to_string()).unwrap_or_default();
                // #2272 — ce que cpal sait déjà du CONTRÔLEUR, et que cette
                // énumération jetait en ne lisant que le nom. Calculé sur le nom
                // BRUT, avant toute désambiguïsation : le suffixe `(n)` vient de
                // nous et n'a rien à dire sur le matériel.
                let hardware_detail = description.as_ref().and_then(|desc| {
                    hardware_detail_from_description(desc, &raw_name, &endpoint_id)
                });

                // Skip ALSA null/dummy sinks that produce no audio
                if is_null_sink(&raw_name) {
                    debug!(device = %raw_name, "local_audio_device_skipped_null_sink");
                    continue;
                }

                let (max_channels, sample_rates, caps_reliable) =
                    match device.supported_output_configs() {
                        Ok(configs) => {
                            let mut max_ch = 0u16;
                            let mut rates = Vec::new();
                            for config in configs {
                                max_ch = max_ch.max(config.channels());
                                let min = config.min_sample_rate();
                                let max = config.max_sample_rate();
                                for &rate in
                                    &[44100, 48000, 88200, 96000, 176400, 192000, 352800, 384000]
                                {
                                    if rate >= min && rate <= max && !rates.contains(&rate) {
                                        rates.push(rate);
                                    }
                                }
                            }
                            rates.sort();

                            // PipeWire's ALSA plugin can return Ok but with an
                            // empty iterator — treat it like an error and fall
                            // through to the fallback probe below.
                            if max_ch == 0 || rates.is_empty() {
                                debug!(
                                    device = %raw_name,
                                    "local_audio_device_supported_configs_empty"
                                );
                                probe_device_fallback_caps(&device, &raw_name)
                            } else {
                                // Ces capacités viennent bien d'une énumération —
                                // ce qui ne veut PAS dire qu'elles ont été
                                // mesurées : sur WASAPI l'énumération est
                                // fabriquée (#2862, voir `sample_rate_evidence`).
                                // `caps_reliable` répond seulement « pas la
                                // supposition de dernier recours », ce qui reste
                                // vrai ici et suffit au dédoublonnage Linux.
                                (max_ch, rates, true)
                            }
                        }
                        Err(_) => {
                            debug!(
                                device = %raw_name,
                                "local_audio_device_supported_configs_failed"
                            );
                            probe_device_fallback_caps(&device, &raw_name)
                        }
                    };

                let is_default = raw_name == default_name;
                // Ce que vaut la liste qu'on s'apprête à publier. Sur WASAPI
                // elle n'a jamais été confrontée au matériel (#2862) ; sur ALSA
                // elle ne vaut que si le PCM interrogé EST le matériel, et pas
                // un `dmix:`/`plughw:` qui accepte tout (#1655). Et une liste
                // SUPPOSÉE (`caps_reliable = false`) n'a jamais rien mesuré —
                // ce drapeau était calculé puis jeté.
                let rates_evidence =
                    sample_rate_evidence_for_device(&host_name, &endpoint_id, caps_reliable);

                // Collapse duplicates. On Linux PipeWire lists the same physical
                // output repeatedly with varying caps, so collapse by NAME and
                // keep the richest-capability variant (else 43 phantom devices →
                // 48 zones, JeromeQ on Ubuntu 24.04). On Windows/macOS two real
                // DACs can share a name but differ in caps, so collapse only exact
                // (name, caps) duplicates and disambiguate the rest.
                #[cfg(target_os = "linux")]
                {
                    if let Some(&idx) = linux_by_name.get(&raw_name) {
                        let ancien_endpoint = devices[idx].endpoint_id.clone();
                        let ancien_materiel = alsa_pcm_is_direct_hardware(&ancien_endpoint);
                        let bascule = merge_linux_duplicate_variant(
                            &mut devices[idx],
                            endpoint_id,
                            is_default,
                            max_channels,
                            sample_rates,
                            rates_evidence.is_measured(),
                            hardware_detail,
                        );
                        let retenu_materiel =
                            alsa_pcm_is_direct_hardware(&devices[idx].endpoint_id);
                        if bascule && retenu_materiel && !ancien_materiel {
                            // Chemin bit-perfect : ce groupe publiera désormais
                            // le PCM du DAC au lieu d'un greffon qui accepte
                            // tout. Une décision qui change ce qui sera OUVERT
                            // ne passe jamais en silence (#3209, #1655).
                            info!(
                                device = %raw_name,
                                greffon_ecarte = %ancien_endpoint,
                                endpoint_retenu = %devices[idx].endpoint_id,
                                "local_audio_alsa_hardware_pcm_preferred"
                            );
                        }
                        debug!(
                            device = %raw_name,
                            retained_endpoint_id = %devices[idx].endpoint_id,
                            bascule,
                            retenu_materiel,
                            "local_audio_device_collapsed_pipewire_duplicate"
                        );
                        continue;
                    }
                }
                #[cfg(not(target_os = "linux"))]
                {
                    // Windows/macOS: do NOT collapse — always disambiguate below.
                    // Two genuinely different physical devices can share BOTH the
                    // name AND the caps: Alain's Ugreen card and his USB DAC both
                    // enumerate as "Speakers" with identical reliable caps, so the
                    // old (name, caps) collapse dropped the DAC entirely and it
                    // could never get a zone (#1084) — even after #654, because
                    // its caps are real, not the assumed fallback. cpal exposes no
                    // unique WASAPI endpoint id to tell a true duplicate from two
                    // same-named devices, so keep every entry and disambiguate
                    // ("Speakers (2)"), restoring the pre-0.8.314 behaviour Alain
                    // had on 0.8.307. A rare truly-duplicated onboard endpoint
                    // then merely shows twice (harmless — both select the same
                    // output) instead of a real device silently vanishing.
                }

                // Disambiguate duplicate device names (common on Windows WASAPI
                // where multiple USB DACs all show as "Haut-Parleurs").
                let name = disambiguate_display_name(&raw_name, &mut seen_names);

                info!(
                    device = %name,
                    endpoint_id = %endpoint_id,
                    is_default,
                    max_channels,
                    sample_rates = ?sample_rates,
                    sample_rates_measured = rates_evidence.is_measured(),
                    "local_audio_device_found"
                );

                devices.push(AudioDevice {
                    name,
                    endpoint_id,
                    is_default,
                    max_channels,
                    sample_rates,
                    sample_rates_measured: rates_evidence.is_measured(),
                    backend: host_name.to_string(),
                    hardware_detail,
                });
                #[cfg(target_os = "linux")]
                linux_by_name.insert(raw_name.clone(), devices.len() - 1);
            }
        }
        Err(e) => {
            warn!(error = %e, host = %host_name, "local_audio_output_devices_enumeration_failed");
        }
    }

    if devices.is_empty() {
        log_no_devices_diagnostics(&host_name);
    } else {
        info!(count = devices.len(), "local_audio_devices_enumerated");
    }

    devices
}

/// Log detailed diagnostics when zero audio devices are found.
///
/// On Linux, checks for PipeWire and provides actionable guidance.
/// On other platforms, logs a simple warning.
fn log_no_devices_diagnostics(host_name: &str) {
    #[cfg(target_os = "linux")]
    {
        // Check if PipeWire is running (it provides ALSA compat layer)
        let pipewire_active = std::fs::read_to_string("/run/user/1000/pipewire-0").is_ok()
            || std::process::Command::new("pgrep")
                .args(["-x", "pipewire"])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);

        // Check if PulseAudio compat is running
        let pulseaudio_active = std::process::Command::new("pgrep")
            .args(["-x", "pipewire-pulse"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
            || std::process::Command::new("pgrep")
                .args(["-x", "pulseaudio"])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);

        // Check if ALSA devices are visible at kernel level
        let proc_asound_cards = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
        let kernel_cards: Vec<&str> = proc_asound_cards
            .lines()
            .filter(|l| l.contains('['))
            .collect();

        // Check if libasound is available
        let libasound_ok = std::path::Path::new("/usr/lib/x86_64-linux-gnu/libasound.so.2")
            .exists()
            || std::path::Path::new("/usr/lib/aarch64-linux-gnu/libasound.so.2").exists()
            || std::path::Path::new("/usr/lib/libasound.so.2").exists();

        // Check ALSA config for PipeWire PCM plugin
        let alsa_conf_has_pipewire =
            std::fs::read_to_string("/etc/alsa/conf.d/99-pipewire-default.conf")
                .or_else(|_| {
                    std::fs::read_to_string("/usr/share/alsa/alsa.conf.d/99-pipewire-default.conf")
                })
                .or_else(|_| {
                    std::fs::read_to_string("/usr/share/alsa/alsa.conf.d/50-pipewire.conf")
                })
                .map(|c| c.contains("pipewire"))
                .unwrap_or(false);

        warn!(
            host = %host_name,
            pipewire_active,
            pulseaudio_compat_active = pulseaudio_active,
            kernel_sound_cards = kernel_cards.len(),
            libasound_available = libasound_ok,
            alsa_pipewire_plugin = alsa_conf_has_pipewire,
            "local_audio_no_output_devices_found — \
             if PipeWire is active, ensure pipewire-alsa is installed \
             (provides the ALSA PCM plugin so cpal can see devices). \
             Install: sudo apt install pipewire-alsa (Debian/Ubuntu) \
             or pipewire-alsa (Fedora/Arch). \
             Also verify: aplay -l shows devices, \
             /proc/asound/cards lists sound cards."
        );

        if !kernel_cards.is_empty() {
            info!(
                cards = ?kernel_cards,
                "local_audio_kernel_sound_cards_detected — \
                 kernel sees sound hardware but cpal ({host_name}) returned zero devices"
            );
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        warn!(
            host = %host_name,
            "local_audio_no_output_devices_found"
        );
    }
}

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
    fn process_pcm_chunk(
        &self,
        staged: &mut Vec<u8>,
        frame_bytes: usize,
        bit_depth: u16,
        channels: u16,
        kind: &mut LocalPcmKind,
    ) -> Option<ProcessedLocalPcm> {
        let aligned_len = (staged.len() / frame_bytes) * frame_bytes;
        if aligned_len == 0 {
            return None;
        }

        if kind.is_awaiting_probe() {
            let probe_bytes = DOP_DETECT_FRAMES * channels.max(1) as usize * 3;
            if aligned_len < probe_bytes {
                // Ne rien convertir ni publier : l'octet marqueur n'existe
                // plus comme tel une fois le mot 24 bits passe en f32.
                return None;
            }
            *kind = if is_dop_pcm(&staged[..aligned_len], bit_depth, channels) {
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

        let mut samples = pcm_bytes_to_f32(&staged[..aligned_len], bit_depth);
        apply_local_dsp(
            &mut samples,
            self.eq,
            self.convolver,
            self.crossfeed,
            self.pure_bypass,
            self.mono_downmix,
            channels,
            dop,
        );
        staged.drain(..aligned_len);

        Some(ProcessedLocalPcm {
            samples,
            source_frames: (aligned_len / frame_bytes) as u64,
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
        #[cfg(not(target_os = "windows"))]
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

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

        let handle = std::thread::spawn(move || {
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

            let (mut channels, mut sample_rate, mut bit_depth, data_offset) = if let Some(parsed) =
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

                // Prefer device's default rate and resample if needed.
                // Same rationale as the WAV path: opening at the source
                // rate in shared mode is unreliable on macOS/Windows.
                let output_config = {
                    let default_cfg = device.default_output_config().ok().map(|c| c.config());
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

                let output_sr = output_config.sample_rate;
                let output_ch = output_config.channels;

                let ring_cap = (output_sr as usize) * (output_ch as usize) * 2;
                starvation.begin_stream(output_sr, output_ch);
                let ring = Arc::new(RingBuf::new_metered(ring_cap, starvation.clone()));
                ring.clear(); // Defensive: zero-fill before callback can read
                let ring_cb = ring.clone();
                let vol_cb = volume.clone();
                let paused_cb = paused.clone();
                let silent_cb = force_silent.clone();
                let soft_mute_cb = soft_mute.clone();
                let mut ramp_cb = soft_mute_cb.ramp(output_sr, output_ch);
                // Gate: output silence until enough real data has been buffered.
                // Prevents stale/garbage audio during track transitions.
                // Minimum: ~500ms of audio at the output sample rate.
                // (v0.8.97=20ms, v0.8.98=200ms — still too low for macOS
                // CoreAudio which can request 1024+ frame buffers.)
                let data_started = Arc::new(AtomicBool::new(false));
                let data_started_cb = data_started.clone();
                let min_buffer_samples = (output_sr as usize) * (output_ch as usize) / 2; // ~500ms

                let stream = match device.build_output_stream(
                    &output_config,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        // Rampe anti-« ploc » (#1590) : au lieu de sauter de
                        // l'amplitude courante à zéro, le gain glisse sur
                        // quelques dizaines de millisecondes. `arm(0)` — DoP,
                        // PURE, sortie exclusive — rend exactement la coupure
                        // franche d'avant.
                        ramp_cb.arm(soft_mute_cb.armed_ms());
                        let silence =
                            paused_cb.load(Ordering::Relaxed) || silent_cb.load(Ordering::Relaxed);
                        if ramp_cb.begin(silence) == crate::audio::soft_mute::Rendering::Silent {
                            data.fill(0.0);
                            return;
                        }
                        // Wait for a minimum amount of data before starting
                        // to read from the ring buffer. This prevents the
                        // audio device from playing stale/garbage samples
                        // during track transitions.
                        if !data_started_cb.load(Ordering::Acquire) {
                            if ring_cb.available() < min_buffer_samples {
                                data.fill(0.0);
                                return;
                            }
                            data_started_cb.store(true, Ordering::Release);
                        }
                        let read = ring_cb.pop(data);
                        let v = vol_cb.load(Ordering::Relaxed) as f32 / 1000.0;
                        ramp_cb.apply(&mut data[..read], v);
                        if read < data.len() {
                            data[read..].fill(0.0);
                        }
                    },
                    make_stream_error_cb(device_gone.clone()),
                    None,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!(error = %e, "audio_stream_build_failed_compressed");
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                };

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
            let mut frame_bytes = channels as usize * bytes_per_sample;

            // ------- Exclusive mode path (macOS only) -------
            #[cfg(target_os = "macos")]
            if exclusive_mode {
                use super::coreaudio_exclusive::ExclusiveOutput;

                info!(
                    device = %device_name,
                    sample_rate,
                    bit_depth,
                    channels,
                    "local_audio_exclusive_mode_active"
                );

                // Ring buffer: ~2 seconds of audio at source sample rate
                let ring_cap = (sample_rate as usize) * (channels as usize) * 2;
                starvation.begin_stream(sample_rate, channels);
                let ring = Arc::new(RingBuf::new_metered(ring_cap, starvation.clone()));
                ring.clear(); // Defensive: zero-fill before callback reads

                let exclusive = match ExclusiveOutput::new(
                    &device_name,
                    sample_rate,
                    bit_depth as u32,
                    channels as u32,
                    ring.clone(),
                    volume.clone(),
                    paused.clone(),
                ) {
                    Ok(ex) => ex,
                    Err(e) => {
                        record_exclusive_open_failure(
                            "CoreAudio",
                            &device_name,
                            &e.to_string(),
                            &open_failure,
                        );
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                };

                info!(device = %device_name, url = %url, "local_audio_exclusive_playing");
                // CoreAudio exclusif : `resolve_output_device` retombe sur le
                // périphérique système quand le nom stocké n'existe plus (DAC
                // débranché, renommé, routage macOS changé). `opened_id` reste
                // `None` : l'`AudioDeviceID` est un entier réattribué au
                // redémarrage, ce n'est pas une identité qu'on peut afficher.
                note_opened_device(
                    "CoreAudio",
                    &device_name,
                    &exclusive.format_info().device_name,
                    None,
                );

                // Feed audio data (no resampling needed -- hardware is set to source rate)
                let pcm_data = if data_offset < header_buf.len() {
                    header_buf[data_offset..].to_vec()
                } else {
                    Vec::new()
                };

                let mut total_frames_fed: u64 = 0;

                // Read and feed the rest of the stream
                let mut read_buf = vec![0u8; 65536];
                let mut leftover = pcm_data;
                let mut pcm_kind = LocalPcmKind::for_bit_depth(bit_depth);
                let pcm_processor = LocalPcmProcessor {
                    eq: &eq,
                    convolver: &convolver,
                    crossfeed: &crossfeed,
                    pure_bypass: &pure_bypass,
                    mono_downmix: &mono_downmix,
                    dop_active: &dop_active,
                    volume: &volume,
                    user_volume: &user_volume_ref,
                    rg_factor: &rg_factor_ref,
                };

                // Process leftover from header read
                // #3108 — le verdict de blocage était JETÉ aux trois sites de
                // ce chemin, seul de tous les chemins de lecture. Conséquence
                // exacte du constat : l'anneau exclusif tient deux secondes
                // d'audio (`ring_cap` ci-dessus), il se remplit une fois, le
                // rappel de rendu ne tire rien, et la position reste sur 2 000
                // ms pour toujours — sans un mot.
                let mut feed_stalled = false;
                if let Some(processed) = pcm_processor.process_pcm_chunk(
                    &mut leftover,
                    frame_bytes,
                    bit_depth,
                    channels,
                    &mut pcm_kind,
                ) {
                    if !feed_ring_abortable(
                        &ring,
                        &processed.samples,
                        &stop_rx,
                        &paused,
                        Some(&force_silent),
                    ) {
                        feed_stalled = true;
                    }
                    total_frames_fed += processed.source_frames;
                }

                let mut http_eof_excl = false;
                while !feed_stalled {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    if force_silent.load(Ordering::Relaxed) {
                        debug!("local_audio_exclusive_aborted_by_stop");
                        break;
                    }

                    let n = match reader.read(&mut read_buf) {
                        Ok(0) => {
                            http_eof_excl = true;
                            break;
                        }
                        Ok(n) => n,
                        Err(ref e)
                            if e.kind() == std::io::ErrorKind::TimedOut
                                || e.kind() == std::io::ErrorKind::WouldBlock =>
                        {
                            // Read timeout — check abort flag and retry
                            continue;
                        }
                        Err(e) => {
                            warn!(error = %e, "local_audio_exclusive_read_error");
                            http_eof_excl = true;
                            break;
                        }
                    };

                    leftover.extend_from_slice(&read_buf[..n]);

                    let aligned_len = (leftover.len() / frame_bytes) * frame_bytes;
                    if aligned_len == 0 {
                        continue;
                    }

                    let Some(processed) = pcm_processor.process_pcm_chunk(
                        &mut leftover,
                        frame_bytes,
                        bit_depth,
                        channels,
                        &mut pcm_kind,
                    ) else {
                        continue;
                    };

                    if !feed_ring_abortable(
                        &ring,
                        &processed.samples,
                        &stop_rx,
                        &paused,
                        Some(&force_silent),
                    ) {
                        feed_stalled = true;
                        break;
                    }

                    total_frames_fed += processed.source_frames;

                    let pos = (total_frames_fed as f64 / sample_rate as f64 * 1000.0) as u64
                        + seek_offset;
                    position_ms.store(pos, Ordering::Relaxed);
                }

                if feed_stalled {
                    // La piste n'a PAS fini : `http_eof_excl` reste faux, donc
                    // aucune fin naturelle n'est signalée et la file n'avance
                    // pas vers un morceau qui heurterait le même périphérique
                    // mort. Le seul mot dit à l'utilisateur part d'ici.
                    record_feed_stall_failure(
                        "CoreAudio",
                        &device_name,
                        position_ms.load(Ordering::Relaxed),
                        &open_failure,
                    );
                }

                if http_eof_excl {
                    report_incomplete_local_pcm_probe(pcm_kind, leftover.len());
                }

                // Fin de piste : rendre au périphérique ce que le convolveur
                // retient encore. Sans ça, `latency_frames()` trames restaient
                // dans le moteur et la fin de chaque piste était tronquée
                // (#2209, revue JP Robbe — la fonction etait morte).
                let queue = flush_local_dsp(
                    &convolver,
                    &crossfeed,
                    &pure_bypass,
                    &mono_downmix,
                    channels,
                    dop_active.load(Ordering::Relaxed),
                );
                if !queue.is_empty() && !feed_stalled {
                    feed_ring_abortable(&ring, &queue, &stop_rx, &paused, Some(&force_silent));
                    total_frames_fed += (queue.len() / channels.max(1) as usize) as u64;
                }

                // Signal natural track end BEFORE draining when the HTTP
                // stream reached EOF, so the orchestrator can detect
                // end-of-track even if force_silent is set during slow drain.
                if http_eof_excl {
                    track_ended_naturally.store(true, Ordering::SeqCst);
                    track_ended_generation.store(my_generation, Ordering::SeqCst);
                    TRACK_END_NOTIFY.notify_one();
                }

                // Wait for ring buffer to drain — JAMAIS sans fin (#3108).
                // Les chemins ASIO, WASAPI et partagé bornaient déjà leur
                // vidage ; celui-ci, seul, tournait tant que l'anneau n'était
                // pas vide. Face à un rappel de rendu mort il ne se vide
                // jamais : le fil restait vivant, la zone « en lecture », et le
                // réexamen des branchements gelé avec elle.
                let drain_deadline =
                    drain_deadline_for(ring.available(), sample_rate as u64, channels as u64);
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
                    if drain_started.elapsed() >= drain_deadline {
                        warn!(
                            device = %device_name,
                            remaining_samples = ring.available(),
                            "local_audio_exclusive_drain_timeout"
                        );
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }

                // ExclusiveOutput::drop() restores sample rate and releases hog mode
                drop(exclusive);
                if play_generation.load(Ordering::SeqCst) == my_generation {
                    playing.store(false, Ordering::SeqCst);
                }
                info!(
                    device = %device_name,
                    frames = total_frames_fed,
                    "local_audio_exclusive_stopped"
                );
                return;
            }

            // ------- Exclusive mode path (Windows ASIO) -------
            #[cfg(all(target_os = "windows", feature = "asio"))]
            if exclusive_mode && audio_backend == "asio" {
                use super::asio_exclusive::AsioExclusiveOutput;

                info!(
                    device = %device_name,
                    sample_rate,
                    bit_depth,
                    channels,
                    "local_audio_asio_exclusive_mode_active"
                );

                // Ring buffer: ~2 seconds of audio at source sample rate
                let ring_cap = (sample_rate as usize) * (channels as usize) * 2;
                starvation.begin_stream(sample_rate, channels);
                let float_ring = Arc::new(RingBuf::new_metered(ring_cap, starvation.clone()));
                let native_ring =
                    Arc::new(NativePcmRing::new_metered(ring_cap, starvation.clone()));
                float_ring.clear();
                native_ring.clear();

                let exclusive = match AsioExclusiveOutput::new(
                    &device_name,
                    sample_rate,
                    bit_depth as u32,
                    channels as u32,
                    float_ring.clone(),
                    native_ring.clone(),
                    volume.clone(),
                    paused.clone(),
                ) {
                    Ok(ex) => ex,
                    Err(e) => {
                        record_exclusive_open_failure(
                            "ASIO",
                            &device_name,
                            &e.to_string(),
                            &open_failure,
                        );
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                };

                let selected_ring = if exclusive.uses_native_transport() {
                    WindowsExclusiveRingRef::Native(&native_ring)
                } else {
                    WindowsExclusiveRingRef::Float(&float_ring)
                };
                if let Some(reason) = exclusive.bit_perfect_unavailable_reason() {
                    info!(
                        backend = "ASIO",
                        device = %device_name,
                        bit_perfect = false,
                        reason,
                        "windows_exclusive_signal_contract"
                    );
                }

                info!(device = %device_name, url = %url, "local_audio_asio_exclusive_playing");
                // ASIO exclusif : résolution par sous-chaîne, et `"default"`
                // prend le premier pilote listé. `opened_id` reste `None` :
                // ASIO n'expose aucun identifiant d'endpoint.
                note_opened_device("ASIO", &device_name, exclusive.opened_device_name(), None);

                // Feed audio data (no resampling needed -- hardware is set to source rate)
                let pcm_data = if data_offset < header_buf.len() {
                    header_buf[data_offset..].to_vec()
                } else {
                    Vec::new()
                };

                let mut total_frames_fed: u64 = 0;

                // Only skip bytes if the stream was NOT pre-seeked by the
                // decoder. When pre_seeked=true, the decoder already produced
                // audio starting at the seek position — skipping would discard
                // the entire stream (double-seek bug reported by DEvir).
                let skip_bytes_asio: u64 = if seek_offset > 0 && !pre_seeked {
                    let skip_frames = (seek_offset as f64 / 1000.0 * sample_rate as f64) as u64;
                    skip_frames * channels as u64 * bytes_per_sample as u64
                } else {
                    0
                };
                let mut skipped_bytes_asio: u64 = 0;

                // Read and feed the rest of the stream. The raw-byte staging
                // buffer is also the DoP quarantine: no initial 24-bit sample
                // may reach the f32 ring until 32 frames have ruled DoP out.
                let mut leftover: Vec<u8> = Vec::new();
                let mut must_classify_24_bit = bit_depth == 24;
                let mut dop_latched = false;
                let mut bit_perfect_state = None;

                // Track-local contract: never inherit the prior stream's DoP
                // state while the first 24-bit probe is still quarantined.
                if dop_active.swap(false, Ordering::SeqCst) {
                    sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, false);
                }

                if !pcm_data.is_empty() {
                    let discard = if skip_bytes_asio > skipped_bytes_asio {
                        ((skip_bytes_asio - skipped_bytes_asio) as usize).min(pcm_data.len())
                    } else {
                        0
                    };
                    skipped_bytes_asio += discard as u64;
                    leftover.extend_from_slice(&pcm_data[discard..]);
                }

                match feed_selected_windows_exclusive_leftover(
                    &mut leftover,
                    frame_bytes,
                    bit_depth,
                    channels,
                    &mut must_classify_24_bit,
                    &mut dop_latched,
                    volume.load(Ordering::SeqCst),
                    &eq,
                    &convolver,
                    &crossfeed,
                    &pure_bypass,
                    &mono_downmix,
                    selected_ring,
                    &stop_rx,
                    &paused,
                    &force_silent,
                ) {
                    Ok(Some(outcome)) => {
                        total_frames_fed += outcome.frames;
                        if dop_active.swap(outcome.dop, Ordering::SeqCst) != outcome.dop {
                            info!(dop = outcome.dop, "local_audio_dop_stream_state_changed");
                            sync_volume_to_dop(
                                &volume,
                                &user_volume_ref,
                                &rg_factor_ref,
                                outcome.dop,
                            );
                        }
                        let volume_units = volume.load(Ordering::SeqCst);
                        let runtime = publish_windows_signal_path_status(
                            &signal_path_status,
                            outcome.bit_perfect,
                            matches!(selected_ring, WindowsExclusiveRingRef::Native(_)),
                            outcome.dop,
                            volume_units,
                            &eq,
                            &convolver,
                            &crossfeed,
                            &pure_bypass,
                            &mono_downmix,
                        );
                        bit_perfect_state = Some(runtime.bit_perfect);
                        info!(
                            backend = "ASIO",
                            bit_perfect = runtime.bit_perfect,
                            dop = outcome.dop,
                            volume_units,
                            reasons = ?runtime.reasons,
                            "windows_exclusive_signal_contract"
                        );
                    }
                    Ok(None) => {}
                    Err(error) => {
                        record_windows_exclusive_pcm_refusal(
                            error,
                            "ASIO",
                            &device_name,
                            &open_failure,
                        );
                        force_silent.store(true, Ordering::SeqCst);
                        dop_active.store(false, Ordering::SeqCst);
                        sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, false);
                        drop(exclusive);
                        if play_generation.load(Ordering::SeqCst) == my_generation {
                            playing.store(false, Ordering::SeqCst);
                        }
                        return;
                    }
                }
                if !must_classify_24_bit && dop_active.swap(false, Ordering::SeqCst) {
                    info!(dop = false, "local_audio_dop_stream_state_changed");
                    sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, false);
                }

                let mut http_eof_asio = false;
                let mut last_data_at = std::time::Instant::now();
                // Pump thread: it owns the blocking HTTP read so the thread
                // that HOLDS THE ASIO DEVICE never blocks on the network.
                // Before this, stop() set force_silent but the device thread
                // sat in reader.read() until the HTTP session died as a side
                // effect of the NEXT play — it then released the ASIO lock
                // ~2.5s INTO the new play. Two repeats survived by timing;
                // the 3rd hit the wrong interleaving: silent output and the
                // poller oscillating at EOF (DEvir, Fireface ASIO, repeat-all,
                // v0.9.0). With the pump, the device thread polls a channel
                // (500ms) and honours stop within one tick; the pump thread
                // may linger in a blocked read but only owns the socket, and
                // exits when the receiver drops or the session closes.
                // Approximate depth of the pump→device channel. Incremented by
                // the pump before each send, decremented by the device loop on
                // each successful recv. A high steady depth means the device
                // thread is NOT draining (ring full / callback dead); a depth of
                // ~0 means the device is starved (EOF never latches). Surfaced in
                // the periodic `asio_exclusive_feed_stats` log (DEvir bug-22).
                let pump_depth = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let (pump_tx, pump_rx) =
                    std::sync::mpsc::sync_channel::<std::io::Result<Vec<u8>>>(64);
                {
                    let pump_depth = pump_depth.clone();
                    std::thread::spawn(move || {
                        let mut reader = reader;
                        let mut buf = vec![0u8; 65536];
                        loop {
                            match reader.read(&mut buf) {
                                Ok(0) => {
                                    pump_depth.fetch_add(1, Ordering::Relaxed);
                                    if pump_tx.send(Ok(Vec::new())).is_err() {
                                        pump_depth.fetch_sub(1, Ordering::Relaxed);
                                    }
                                    break;
                                }
                                Ok(n) => {
                                    pump_depth.fetch_add(1, Ordering::Relaxed);
                                    if pump_tx.send(Ok(buf[..n].to_vec())).is_err() {
                                        pump_depth.fetch_sub(1, Ordering::Relaxed);
                                        break; // receiver gone — playback stopped
                                    }
                                }
                                Err(e) => {
                                    let transient = matches!(
                                        e.kind(),
                                        std::io::ErrorKind::TimedOut
                                            | std::io::ErrorKind::WouldBlock
                                    );
                                    pump_depth.fetch_add(1, Ordering::Relaxed);
                                    if pump_tx.send(Err(e)).is_err() {
                                        pump_depth.fetch_sub(1, Ordering::Relaxed);
                                        break;
                                    }
                                    if !transient {
                                        break;
                                    }
                                }
                            }
                        }
                    });
                }
                let mut last_stats_at = std::time::Instant::now();
                let mut pcm_refusal = None;
                loop {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    if force_silent.load(Ordering::Relaxed) {
                        debug!("local_audio_asio_exclusive_aborted_by_stop");
                        break;
                    }

                    // Periodic health snapshot (~500ms) so a wedge is diagnosable
                    // from DEvir's next log: ring full + high pump_depth => the
                    // callback stopped draining; ring/pump ~empty => starved / EOF
                    // never latched (bug-22 / #789).
                    if last_stats_at.elapsed() >= std::time::Duration::from_millis(500) {
                        debug!(
                            ring_available = selected_ring.available(),
                            ring_capacity = selected_ring.capacity(),
                            total_frames_fed,
                            pump_depth = pump_depth.load(Ordering::Relaxed),
                            leftover_bytes = leftover.len(),
                            "asio_exclusive_feed_stats"
                        );
                        last_stats_at = std::time::Instant::now();
                    }

                    let recv = pump_rx.recv_timeout(std::time::Duration::from_millis(500));
                    if recv.is_ok() {
                        pump_depth.fetch_sub(1, Ordering::Relaxed);
                    }
                    let chunk = match recv {
                        Ok(Ok(data)) if data.is_empty() => {
                            http_eof_asio = true;
                            break;
                        }
                        Ok(Ok(data)) => {
                            last_data_at = std::time::Instant::now();
                            data
                        }
                        Ok(Err(ref e))
                            if e.kind() == std::io::ErrorKind::TimedOut
                                || e.kind() == std::io::ErrorKind::WouldBlock =>
                        {
                            // A streaming HTTP source (transcoded WAV over a
                            // keep-alive connection) may never return a clean
                            // EOF: after the last byte it just keeps timing out.
                            // Once the whole track has been fed AND the ring has
                            // fully drained (everything played), a sustained read
                            // idle means the track ended — signal EOF so the
                            // orchestrator can advance/repeat. Without this, the
                            // loop spins forever and end-of-track is never
                            // detected on exclusive ASIO outputs (DEvir: repeat
                            // never fired on a clean playthrough).
                            if total_frames_fed > 0
                                && leftover.is_empty()
                                && selected_ring.available() == 0
                                && last_data_at.elapsed() > std::time::Duration::from_secs(5)
                            {
                                info!("local_audio_asio_exclusive_stream_idle_eof");
                                http_eof_asio = true;
                                break;
                            }
                            continue;
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            // Same sustained-idle EOF heuristic as the
                            // transient-read-error arm above.
                            if total_frames_fed > 0
                                && leftover.is_empty()
                                && selected_ring.available() == 0
                                && last_data_at.elapsed() > std::time::Duration::from_secs(5)
                            {
                                info!("local_audio_asio_exclusive_stream_idle_eof");
                                http_eof_asio = true;
                                break;
                            }
                            continue;
                        }
                        Ok(Err(e)) => {
                            warn!(error = %e, "local_audio_asio_exclusive_read_error");
                            http_eof_asio = true;
                            break;
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            http_eof_asio = true;
                            break;
                        }
                    };
                    let n = chunk.len();

                    if skip_bytes_asio > 0 && skipped_bytes_asio < skip_bytes_asio {
                        let remaining = (skip_bytes_asio - skipped_bytes_asio) as usize;
                        if n <= remaining {
                            skipped_bytes_asio += n as u64;
                            continue;
                        }
                        skipped_bytes_asio = skip_bytes_asio;
                        leftover.extend_from_slice(&chunk[remaining..]);
                    } else {
                        leftover.extend_from_slice(&chunk);
                    }

                    match feed_selected_windows_exclusive_leftover(
                        &mut leftover,
                        frame_bytes,
                        bit_depth,
                        channels,
                        &mut must_classify_24_bit,
                        &mut dop_latched,
                        volume.load(Ordering::SeqCst),
                        &eq,
                        &convolver,
                        &crossfeed,
                        &pure_bypass,
                        &mono_downmix,
                        selected_ring,
                        &stop_rx,
                        &paused,
                        &force_silent,
                    ) {
                        Ok(Some(outcome)) => {
                            total_frames_fed += outcome.frames;
                            if dop_active.swap(outcome.dop, Ordering::SeqCst) != outcome.dop {
                                info!(dop = outcome.dop, "local_audio_dop_stream_state_changed");
                                sync_volume_to_dop(
                                    &volume,
                                    &user_volume_ref,
                                    &rg_factor_ref,
                                    outcome.dop,
                                );
                            }
                            let volume_units = volume.load(Ordering::SeqCst);
                            let runtime = publish_windows_signal_path_status(
                                &signal_path_status,
                                outcome.bit_perfect,
                                matches!(selected_ring, WindowsExclusiveRingRef::Native(_)),
                                outcome.dop,
                                volume_units,
                                &eq,
                                &convolver,
                                &crossfeed,
                                &pure_bypass,
                                &mono_downmix,
                            );
                            if bit_perfect_state != Some(runtime.bit_perfect) {
                                bit_perfect_state = Some(runtime.bit_perfect);
                                info!(
                                    backend = "ASIO",
                                    bit_perfect = runtime.bit_perfect,
                                    dop = outcome.dop,
                                    volume_units,
                                    reasons = ?runtime.reasons,
                                    "windows_exclusive_signal_contract"
                                );
                            }
                        }
                        Ok(None) => {}
                        Err(error) => {
                            pcm_refusal = Some(error);
                            break;
                        }
                    }

                    let pos = (total_frames_fed as f64 / sample_rate as f64 * 1000.0) as u64
                        + seek_offset;
                    position_ms.store(pos, Ordering::Relaxed);
                }

                if pcm_refusal.is_none() && http_eof_asio {
                    match selected_ring {
                        WindowsExclusiveRingRef::Float(_) => {
                            if let Err(error) = finish_windows_exclusive_probe(
                                bit_depth,
                                must_classify_24_bit,
                                leftover.len(),
                            ) {
                                pcm_refusal = Some(error);
                            }
                        }
                        WindowsExclusiveRingRef::Native(ring)
                            if must_classify_24_bit && !leftover.is_empty() =>
                        {
                            let aligned = (leftover.len() / frame_bytes) * frame_bytes;
                            let native = pcm_bytes_to_native_i32(&leftover[..aligned], bit_depth);
                            feed_native_ring_abortable(
                                ring,
                                &native,
                                &stop_rx,
                                &paused,
                                Some(&force_silent),
                            );
                            leftover.drain(..aligned);
                            total_frames_fed += (aligned / frame_bytes) as u64;
                            info!(
                                backend = "ASIO",
                                bytes = aligned,
                                bit_perfect = true,
                                "windows_exclusive_short_24bit_stream_forced_raw"
                            );
                        }
                        WindowsExclusiveRingRef::Native(_) => {}
                    }
                }
                if let Some(error) = pcm_refusal {
                    record_windows_exclusive_pcm_refusal(
                        error,
                        "ASIO",
                        &device_name,
                        &open_failure,
                    );
                    force_silent.store(true, Ordering::SeqCst);
                    dop_active.store(false, Ordering::SeqCst);
                    sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, false);
                    drop(exclusive);
                    if play_generation.load(Ordering::SeqCst) == my_generation {
                        playing.store(false, Ordering::SeqCst);
                    }
                    return;
                }

                // Fin de piste : rendre ce que le convolveur retient (#2209).
                let queue = flush_local_dsp(
                    &convolver,
                    &crossfeed,
                    &pure_bypass,
                    &mono_downmix,
                    channels,
                    dop_active.load(Ordering::Relaxed),
                );
                if !queue.is_empty() {
                    feed_selected_windows_exclusive_tail(
                        selected_ring,
                        queue,
                        bit_depth,
                        volume.load(Ordering::SeqCst),
                        &stop_rx,
                        &paused,
                        &force_silent,
                    );
                }

                // Signal natural track end BEFORE draining when the HTTP
                // stream reached EOF, so the orchestrator can detect
                // end-of-track even if force_silent is set during slow drain.
                if http_eof_asio {
                    track_ended_naturally.store(true, Ordering::SeqCst);
                    track_ended_generation.store(my_generation, Ordering::SeqCst);
                    TRACK_END_NOTIFY.notify_one();
                }

                // Wait for the ring to drain — but NEVER block forever. If the
                // ASIO render callback stops consuming (RME driver wedged after a
                // stop→start reopen at a Repeat loop point — DEvir bug-22, the
                // #789 regression), `ring.available()` never reaches 0 and this
                // loop used to spin indefinitely, stranding this thread AND the
                // process-wide ASIO_DEVICE_LOCK (held until `exclusive` is dropped
                // just below). Bound it two ways: a hard wall-clock deadline of
                // ~2× the ring's time-capacity, and a stall detector that bails if
                // `available()` has not decreased for ~1.5s.
                let ring_capacity_ms = if sample_rate > 0 && channels > 0 {
                    (selected_ring.capacity() as u64 * 1000)
                        / (sample_rate as u64 * channels as u64)
                } else {
                    0
                };
                let drain_deadline =
                    std::time::Duration::from_millis((ring_capacity_ms * 2).max(1000));
                let drain_started = std::time::Instant::now();
                let mut last_avail = selected_ring.available();
                let mut last_progress_at = std::time::Instant::now();
                loop {
                    if stop_rx.try_recv().is_ok() {
                        break;
                    }
                    if force_silent.load(Ordering::Relaxed) {
                        break;
                    }
                    let avail = selected_ring.available();
                    if avail == 0 {
                        break;
                    }
                    if avail < last_avail {
                        last_avail = avail;
                        last_progress_at = std::time::Instant::now();
                    } else if last_progress_at.elapsed() >= std::time::Duration::from_millis(1500) {
                        warn!(
                            device = %device_name,
                            ring_available = avail,
                            total_frames_fed,
                            "asio_drain_timeout"
                        );
                        break;
                    }
                    if drain_started.elapsed() >= drain_deadline {
                        warn!(
                            device = %device_name,
                            ring_available = avail,
                            elapsed_ms = drain_started.elapsed().as_millis() as u64,
                            "asio_drain_timeout"
                        );
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }

                // AsioExclusiveOutput::drop() releases the ASIO device and, with
                // it, the process-wide ASIO_DEVICE_LOCK. Both loops above are now
                // bounded and any panic unwinds through this owned local, so this
                // drop runs on EVERY exit path — the lock can never be stranded.
                drop(exclusive);
                if play_generation.load(Ordering::SeqCst) == my_generation {
                    playing.store(false, Ordering::SeqCst);
                }
                info!(
                    device = %device_name,
                    frames = total_frames_fed,
                    "local_audio_asio_exclusive_stopped"
                );
                return;
            }

            // ------- WASAPI Exclusive mode path (Windows, non-ASIO) -------
            #[cfg(target_os = "windows")]
            if exclusive_mode && audio_backend != "asio" {
                use super::wasapi_exclusive::WasapiExclusiveOutput;

                info!(
                    device = %device_name,
                    sample_rate,
                    bit_depth,
                    channels,
                    "local_audio_wasapi_exclusive_mode_active"
                );

                let ring_cap = (sample_rate as usize) * (channels as usize) * 2;
                starvation.begin_stream(sample_rate, channels);
                let ring = Arc::new(NativePcmRing::new_metered(ring_cap, starvation.clone()));
                ring.clear();

                match WasapiExclusiveOutput::new(
                    &device_name,
                    endpoint_id.as_deref(),
                    sample_rate,
                    bit_depth as u32,
                    channels as u32,
                    ring.clone(),
                    paused.clone(),
                ) {
                    Ok(mut wasapi) => {
                        if let Err(e) = wasapi.start() {
                            record_exclusive_open_failure(
                                "WASAPI",
                                &device_name,
                                &e,
                                &open_failure,
                            );
                            playing.store(false, Ordering::SeqCst);
                            return;
                        } else {
                            info!(
                                requested_device = %device_name,
                                device = %wasapi.opened_device_name(),
                                endpoint_id = %wasapi.opened_device_id(),
                                info = %wasapi.format_info(),
                                "wasapi_exclusive_playing"
                            );
                            // Ces deux accesseurs existaient depuis #2207 et
                            // n'avaient que cette ligne de journal pour
                            // lecteur. La zone les porte désormais.
                            note_opened_device(
                                "WASAPI",
                                &device_name,
                                wasapi.opened_device_name(),
                                Some(wasapi.opened_device_id()),
                            );

                            let pcm_data = if data_offset < header_buf.len() {
                                header_buf[data_offset..].to_vec()
                            } else {
                                Vec::new()
                            };

                            let mut total_frames_fed: u64 = 0;
                            let mut read_buf = vec![0u8; 65536];
                            let mut leftover = pcm_data;
                            let mut must_classify_24_bit = bit_depth == 24;
                            let mut dop_latched = false;
                            let mut bit_perfect_state = None;

                            // A new track never inherits the DoP/volume state
                            // of the previous one while its first 24-bit probe
                            // is still quarantined.
                            if dop_active.swap(false, Ordering::SeqCst) {
                                sync_volume_to_dop(
                                    &volume,
                                    &user_volume_ref,
                                    &rg_factor_ref,
                                    false,
                                );
                            }

                            if let Some(outcome) = feed_windows_native_exclusive_leftover(
                                &mut leftover,
                                frame_bytes,
                                bit_depth,
                                channels,
                                &mut must_classify_24_bit,
                                &mut dop_latched,
                                volume.load(Ordering::SeqCst),
                                &eq,
                                &convolver,
                                &crossfeed,
                                &pure_bypass,
                                &mono_downmix,
                                &ring,
                                &stop_rx,
                                &paused,
                                &force_silent,
                            ) {
                                total_frames_fed += outcome.frames;
                                if dop_active.swap(outcome.dop, Ordering::SeqCst) != outcome.dop {
                                    info!(
                                        dop = outcome.dop,
                                        "local_audio_dop_stream_state_changed"
                                    );
                                    sync_volume_to_dop(
                                        &volume,
                                        &user_volume_ref,
                                        &rg_factor_ref,
                                        outcome.dop,
                                    );
                                }
                                let volume_units = volume.load(Ordering::SeqCst);
                                let runtime = publish_windows_signal_path_status(
                                    &signal_path_status,
                                    outcome.bit_perfect,
                                    true,
                                    outcome.dop,
                                    volume_units,
                                    &eq,
                                    &convolver,
                                    &crossfeed,
                                    &pure_bypass,
                                    &mono_downmix,
                                );
                                if bit_perfect_state != Some(runtime.bit_perfect) {
                                    bit_perfect_state = Some(runtime.bit_perfect);
                                    info!(
                                        backend = "WASAPI",
                                        bit_perfect = runtime.bit_perfect,
                                        dop = outcome.dop,
                                        volume_units,
                                        reasons = ?runtime.reasons,
                                        "windows_exclusive_signal_contract"
                                    );
                                }
                            }
                            if !must_classify_24_bit && dop_active.swap(false, Ordering::SeqCst) {
                                info!(dop = false, "local_audio_dop_stream_state_changed");
                                sync_volume_to_dop(
                                    &volume,
                                    &user_volume_ref,
                                    &rg_factor_ref,
                                    false,
                                );
                            }

                            let mut http_eof_wasapi = false;
                            loop {
                                if stop_rx.try_recv().is_ok() {
                                    break;
                                }
                                if force_silent.load(Ordering::Relaxed) {
                                    debug!("local_audio_wasapi_exclusive_aborted_by_stop");
                                    break;
                                }

                                match reader.read(&mut read_buf) {
                                    Ok(0) => {
                                        http_eof_wasapi = true;
                                        break;
                                    }
                                    Ok(n) => {
                                        leftover.extend_from_slice(&read_buf[..n]);
                                        if let Some(outcome) =
                                            feed_windows_native_exclusive_leftover(
                                                &mut leftover,
                                                frame_bytes,
                                                bit_depth,
                                                channels,
                                                &mut must_classify_24_bit,
                                                &mut dop_latched,
                                                volume.load(Ordering::SeqCst),
                                                &eq,
                                                &convolver,
                                                &crossfeed,
                                                &pure_bypass,
                                                &mono_downmix,
                                                &ring,
                                                &stop_rx,
                                                &paused,
                                                &force_silent,
                                            )
                                        {
                                            total_frames_fed += outcome.frames;
                                            if dop_active.swap(outcome.dop, Ordering::SeqCst)
                                                != outcome.dop
                                            {
                                                info!(
                                                    dop = outcome.dop,
                                                    "local_audio_dop_stream_state_changed"
                                                );
                                                sync_volume_to_dop(
                                                    &volume,
                                                    &user_volume_ref,
                                                    &rg_factor_ref,
                                                    outcome.dop,
                                                );
                                            }
                                            let volume_units = volume.load(Ordering::SeqCst);
                                            let runtime = publish_windows_signal_path_status(
                                                &signal_path_status,
                                                outcome.bit_perfect,
                                                true,
                                                outcome.dop,
                                                volume_units,
                                                &eq,
                                                &convolver,
                                                &crossfeed,
                                                &pure_bypass,
                                                &mono_downmix,
                                            );
                                            if bit_perfect_state != Some(runtime.bit_perfect) {
                                                bit_perfect_state = Some(runtime.bit_perfect);
                                                info!(
                                                    backend = "WASAPI",
                                                    bit_perfect = runtime.bit_perfect,
                                                    dop = outcome.dop,
                                                    volume_units,
                                                    reasons = ?runtime.reasons,
                                                    "windows_exclusive_signal_contract"
                                                );
                                            }
                                        }

                                        let pos = (total_frames_fed as f64 / sample_rate as f64
                                            * 1000.0)
                                            as u64
                                            + seek_offset;
                                        position_ms.store(pos, Ordering::Relaxed);
                                    }
                                    Err(ref e)
                                        if e.kind() == std::io::ErrorKind::TimedOut
                                            || e.kind() == std::io::ErrorKind::WouldBlock =>
                                    {
                                        continue;
                                    }
                                    Err(e) => {
                                        warn!(error = %e, "local_audio_wasapi_exclusive_read_error");
                                        http_eof_wasapi = true;
                                        break;
                                    }
                                }
                            }

                            // Less than 32 initial 24-bit frames cannot be
                            // classified, but the integer ring can still carry
                            // them safely. Keep them raw and at unity rather
                            // than guessing PCM and applying sample arithmetic.
                            if http_eof_wasapi && must_classify_24_bit && !leftover.is_empty() {
                                let aligned = (leftover.len() / frame_bytes) * frame_bytes;
                                let native =
                                    pcm_bytes_to_native_i32(&leftover[..aligned], bit_depth);
                                feed_native_ring_abortable(
                                    &ring,
                                    &native,
                                    &stop_rx,
                                    &paused,
                                    Some(&force_silent),
                                );
                                leftover.drain(..aligned);
                                total_frames_fed += (aligned / frame_bytes) as u64;
                                info!(
                                    backend = "WASAPI",
                                    bytes = aligned,
                                    "windows_exclusive_short_24bit_stream_forced_raw"
                                );
                            }

                            // WASAPI exclusive now follows the same DSP tail
                            // contract as the other local PCM paths (#2209).
                            let queue = flush_local_dsp(
                                &convolver,
                                &crossfeed,
                                &pure_bypass,
                                &mono_downmix,
                                channels,
                                false,
                            );
                            if !queue.is_empty() {
                                let volume_factor = volume.load(Ordering::SeqCst) as f32 / 1000.0;
                                let mut queue = queue;
                                if volume_factor != 1.0 {
                                    for sample in &mut queue {
                                        *sample *= volume_factor;
                                    }
                                }
                                let native = f32_to_native_i32(&queue, bit_depth);
                                feed_native_ring_abortable(
                                    &ring,
                                    &native,
                                    &stop_rx,
                                    &paused,
                                    Some(&force_silent),
                                );
                            }

                            // Signal natural track end BEFORE draining when
                            // the HTTP stream reached EOF, so the orchestrator
                            // can detect end-of-track even if force_silent is
                            // set during slow drain (e.g. 44.1→192 kHz resample).
                            if http_eof_wasapi {
                                track_ended_naturally.store(true, Ordering::SeqCst);
                                track_ended_generation.store(my_generation, Ordering::SeqCst);
                                TRACK_END_NOTIFY.notify_one();
                            }

                            // Wait for ring buffer to drain
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
                                std::thread::sleep(std::time::Duration::from_millis(50));
                            }

                            wasapi.stop();
                            if play_generation.load(Ordering::SeqCst) == my_generation {
                                playing.store(false, Ordering::SeqCst);
                            }
                            info!(
                                device = %device_name,
                                frames = total_frames_fed,
                                "local_audio_wasapi_exclusive_stopped"
                            );
                            return;
                        }
                    }
                    Err(e) => {
                        record_exclusive_open_failure("WASAPI", &device_name, &e, &open_failure);
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                }
            }
            #[cfg(not(any(target_os = "macos", all(target_os = "windows", feature = "asio"))))]
            let _ = exclusive_mode;

            // ------- Open cpal device (shared mode) -------
            let host = select_host(&audio_backend);
            // Nom de la VARIANTE cpal ("Wasapi", "Alsa", "Asio", "CoreAudio").
            // `&'static str`, donc aucun emprunt sur `host`.
            let host_id_name: &'static str = host.id().name();
            let Some((device, fell_back)) = find_device_with_fallback(
                &host,
                &device_name,
                endpoint_id.as_deref(),
                origin_host.as_deref(),
            ) else {
                record_shared_device_not_found(
                    SharedDeviceResolution::WavStreamNotFound,
                    &device_name,
                    &open_failure,
                );
                playing.store(false, Ordering::SeqCst);
                return;
            };
            if fell_back {
                info!(
                    original = %device_name,
                    "audio_device_fallback_used_for_wav_stream"
                );
            }

            // Determine the output config for shared mode.
            //
            // Strategy: prefer the device's default/native sample rate and
            // resample with rubato when the source rate differs.  This is more
            // reliable than trying to open the device at the source rate:
            //
            // - On macOS, cpal's CoreAudio backend does NOT call
            //   `set_sample_rate` for output streams (only for input).  So
            //   `build_output_stream` at 96 kHz "succeeds" (CoreAudio inserts
            //   an internal converter), but the conversion is unreliable on
            //   many devices/macOS versions and produces white noise.
            //
            // - On Windows WASAPI shared mode, the system mixer runs at a
            //   fixed rate (usually 48 kHz); requesting a different rate may
            //   be rejected or silently mis-converted.
            //
            // By always opening at the device's native rate and doing our own
            // high-quality sinc resampling (rubato), we guarantee correct
            // output on all platforms.
            //
            // If the source rate happens to match the device rate, no
            // resampling occurs (zero overhead).
            let output_config = {
                // First, get the device's default config (reflects actual
                // operating rate on most platforms).
                let default_cfg = device.default_output_config().ok().map(|c| c.config());
                let default_sr = default_cfg.as_ref().map(|c| c.sample_rate);

                // Ce que l'énumération de cpal RÉPOND. Le filtre est
                // TAUTOLOGIQUE quand l'énumération est fabriquée :
                // `find_matching_config` recopie la cadence demandée dans le
                // `StreamConfig` qu'il rend, donc `c.sample_rate ==
                // sample_rate` est vrai par construction dès qu'une plage
                // quelconque a été retenue — et sur WASAPI toutes les plages
                // sont retenues sans test (#2862). Cette réponse n'est donc
                // plus qu'une ENTRÉE de la décision (#3233) : c'est
                // `decide_local_rate_opening` qui tranche, en regardant ce que
                // la réponse vaut. Elle n'est calculée que si le périphérique
                // n'est pas déjà à la bonne cadence — sur WASAPI l'énumération
                // déroule 147 formats, sur ASIO elle touche le pilote.
                let enumerated = if default_sr == Some(sample_rate) {
                    None
                } else {
                    find_matching_config(&device, channels, sample_rate)
                        .filter(|c| c.sample_rate == sample_rate)
                };
                // Sur ALSA, `endpoint_id` est le nom de PCM (`hw:CARD=…`,
                // `dmix:CARD=…`) : c'est LUI qui dit si le « oui » vient du
                // pilote ou d'un rééchantillonneur (#1655). Le journaliser
                // ici est la ligne qui manquait pour trancher un relevé de
                // terrain sans y retourner.
                let opened_endpoint_id = device.id().map(|id| id.to_string()).unwrap_or_default();
                let rate_evidence =
                    sample_rate_evidence_for_device(host_id_name, &opened_endpoint_id, true);
                let decision = decide_local_rate_opening(
                    sample_rate,
                    default_sr,
                    enumerated.is_some(),
                    rate_evidence,
                );

                // Ce que la décision ouvre RÉELLEMENT — la seule chose qu'on ait
                // le droit de remonter.
                let (chosen, opened_sr, reason) = match (decision, default_cfg, enumerated) {
                    // Le périphérique y est déjà : aucune conversion, et rien à
                    // régler côté matériel.
                    (LocalRateOpening::DeviceAlreadyAtSourceRate, Some(cfg), _) => {
                        // Le bras NOMINAL — et le plus frequent : le DAC est
                        // deja a la cadence de la source. Les trois autres bras
                        // journalisent `endpoint_id` depuis #1655 ; celui-ci ne
                        // disait rien, si bien qu'un releve de terrain n'aurait
                        // vu QUE les cas anormaux et aurait conclu de travers
                        // sur la part de `hw:` dans le parc (#3209).
                        info!(
                            source_sr = sample_rate,
                            backend = %host_id_name,
                            endpoint_id = %opened_endpoint_id,
                            rate_support_measured = rate_evidence.is_measured(),
                            "local_audio_open_device_already_at_source_rate"
                        );
                        (cfg, sample_rate, None)
                    }
                    // L'énumération est une MESURE et elle retient la cadence :
                    // on ouvre à la cadence de la source, exactement comme
                    // avant. Témoin du cas nominal.
                    //
                    // A DSD256 file decodes to 352.8kHz; on a DAC left at
                    // 44.1kHz by the OS the old code resampled 352.8k→44.1k in
                    // real time, the sinc resampler underran and no sound came
                    // out (Cyrille, FiiO K3 which natively supports 352.8kHz,
                    // iFi Neo iDSD).
                    (LocalRateOpening::AtSourceRateMeasured, _, Some(cfg)) => {
                        info!(
                            source_sr = sample_rate,
                            device_default_sr = ?default_sr,
                            backend = %host_id_name,
                            endpoint_id = %opened_endpoint_id,
                            rate_support_measured = rate_evidence.is_measured(),
                            "local_audio_open_at_source_rate_reported_supported"
                        );
                        // macOS: cpal's CoreAudio backend does NOT switch the
                        // device's hardware nominal rate for output streams (see
                        // the note above), so opening the cpal stream "at the
                        // source rate" leaves the DAC clocked at the OS rate and
                        // CoreAudio silently converts — which yields SILENCE for
                        // high-rate DSD→PCM (DSD128/256/512 all decode to
                        // 352.8kHz; only DSD64's 176.4k survived). We reach this
                        // branch precisely when the device SUPPORTS the source
                        // rate but its default differs, so set the hardware
                        // nominal rate explicitly (what the exclusive/hog path
                        // already does) — the DAC then actually clocks at
                        // 352.8kHz. Best-effort: if the device can't be
                        // resolved/set we fall through to today's behavior (no
                        // regression). Cyrille: iFi Neo iDSD / FiiO K3, DSD128+
                        // silent.
                        #[cfg(target_os = "macos")]
                        {
                            use coreaudio::audio_unit::macos_helpers;
                            if let Some(dev_id) =
                                macos_helpers::get_device_id_from_name(&device_name, false)
                            {
                                let want = cfg.sample_rate as f64;
                                match macos_helpers::set_device_sample_rate(dev_id, want) {
                                    Ok(_) => info!(
                                        device = %device_name,
                                        to = cfg.sample_rate,
                                        "local_audio_coreaudio_nominal_rate_set_shared"
                                    ),
                                    Err(e) => warn!(
                                        error = %e,
                                        wanted = cfg.sample_rate,
                                        "local_audio_coreaudio_set_rate_failed"
                                    ),
                                }
                            }
                        }
                        (cfg, sample_rate, None)
                    }
                    // On refuse la cadence de la source : rubato convertit. Une
                    // décision qui change ce qui part au DAC ne passe jamais en
                    // silence (#3209, #1655, #3233).
                    (
                        LocalRateOpening::ResampleToDeviceRate {
                            device_sample_rate,
                            reason,
                        },
                        Some(cfg),
                        _,
                    ) => {
                        warn!(
                            source_sr = sample_rate,
                            device_sr = device_sample_rate,
                            backend = %host_id_name,
                            endpoint_id = %opened_endpoint_id,
                            rate_support_measured = rate_evidence.is_measured(),
                            reason = reason.code(),
                            "local_audio_rate_mismatch_will_resample"
                        );
                        (cfg, device_sample_rate, Some(reason))
                    }
                    // Aucune cadence de périphérique connue : rien vers quoi
                    // rééchantillonner, on ouvre à la cadence de la source en
                    // dernier recours (PipeWire, etc.). Les bras `Some(cfg)`
                    // ci-dessus étant exhaustifs pour `default_cfg = Some(..)`,
                    // ce bras ne se prend qu'avec `default_cfg = None` — sauf
                    // `AtSourceRateMeasured` sans `enumerated`, que
                    // `decide_local_rate_opening` ne peut pas produire.
                    _ => {
                        let cfg = find_matching_config(&device, channels, sample_rate).unwrap_or(
                            cpal::StreamConfig {
                                channels,
                                sample_rate,
                                buffer_size: cpal::BufferSize::Default,
                            },
                        );
                        let opened = cfg.sample_rate;
                        info!(
                            source_sr = sample_rate,
                            opened_sr = opened,
                            backend = %host_id_name,
                            endpoint_id = %opened_endpoint_id,
                            "local_audio_rate_last_resort_no_device_default"
                        );
                        (cfg, opened, None)
                    }
                };
                note_rate_decision(ObservedRate {
                    source_sample_rate: sample_rate,
                    opened_sample_rate: opened_sr,
                    reason,
                    evidence_measured: rate_evidence.is_measured(),
                });
                chosen
            };

            // Build output stream at the chosen rate.
            let silent_cb_outer = force_silent.clone();
            // Gate: the cpal callback outputs silence until enough real data
            // has been buffered in the ring buffer.  This prevents stale or
            // garbage audio from reaching the DAC during track transitions.
            let data_started_shared = Arc::new(AtomicBool::new(false));
            let build_stream =
                |cfg: &cpal::StreamConfig,
                 ring_cb: Arc<RingBuf>,
                 vol_cb: Arc<AtomicU32>,
                 paused_cb: Arc<AtomicBool>,
                 _finished_cb: Arc<AtomicBool>,
                 silent_cb: Arc<AtomicBool>,
                 ds_cb: Arc<AtomicBool>,
                 min_buf: usize,
                 soft_mute_cb: crate::audio::soft_mute::SoftMuteGate| {
                    let mut ramp_cb = soft_mute_cb.ramp(cfg.sample_rate, cfg.channels);
                    device.build_output_stream(
                        cfg,
                        move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                            // Rampe anti-« ploc » (#1590) — voir le callback du
                            // chemin compressé pour le détail. `arm(0)` rétablit la
                            // coupure franche sur DoP, PURE et sortie exclusive.
                            ramp_cb.arm(soft_mute_cb.armed_ms());
                            let silence = paused_cb.load(Ordering::Relaxed)
                                || silent_cb.load(Ordering::Relaxed);
                            if ramp_cb.begin(silence) == crate::audio::soft_mute::Rendering::Silent
                            {
                                data.fill(0.0);
                                return;
                            }
                            // Wait for a minimum amount of data before starting
                            // to read from the ring buffer. This prevents the
                            // audio device from playing stale/garbage samples
                            // during track transitions.
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
                        make_stream_error_cb(device_gone.clone()),
                        None,
                    )
                };

            // Bit-perfect USB DACs (XMOS/Totaldac, Nagra, …) frequently reject
            // float and only accept integer PCM: cpal's f32 build_output_stream
            // then fails with "Sample format 'f32' is not supported by hardware".
            // This builds the same stream in an integer format instead, converting
            // the f32 ring-buffer samples on the fly (reuses symphonia's IntoSample,
            // as orchestrator.rs already does). Only used as a fallback after both
            // f32 attempts fail, so the f32 happy path is untouched (Pascal, XMOS
            // USB Audio 2.0 → Totaldac).
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
                let mut scratch: Vec<f32> = Vec::new();
                let mut ramp_cb = soft_mute_cb.ramp(cfg.sample_rate, cfg.channels);
                device.build_output_stream(
                    cfg,
                    move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                        let n = data.len();
                        // Rampe anti-« ploc » (#1590). Ce chemin sert les DAC
                        // qui refusent le flottant : la rampe y est armée par la
                        // même porte, donc toujours désarmée sur DoP, en PURE et
                        // en sortie exclusive.
                        ramp_cb.arm(soft_mute_cb.armed_ms());
                        let silence =
                            paused_cb.load(Ordering::Relaxed) || silent_cb.load(Ordering::Relaxed);
                        if ramp_cb.begin(silence) == crate::audio::soft_mute::Rendering::Silent {
                            data.fill(zero);
                            return;
                        }
                        if !ds_cb.load(Ordering::Acquire) {
                            if ring_cb.available() < min_buf {
                                data.fill(zero);
                                return;
                            }
                            ds_cb.store(true, Ordering::Release);
                        }
                        if scratch.len() < n {
                            scratch.resize(n, 0.0);
                        }
                        let buf = &mut scratch[..n];
                        let read = ring_cb.pop(buf);
                        let v = vol_cb.load(Ordering::Relaxed) as f32 / 1000.0;
                        // La rampe module le tampon f32 AVANT la conversion en
                        // mot entier : convertir puis multiplier ferait le
                        // produit dans le format du DAC, hors du contrat de
                        // `IntoSample`.
                        ramp_cb.apply(&mut buf[..read], v);
                        for (o, s) in data[..read].iter_mut().zip(&buf[..read]) {
                            *o = (*s).into_sample();
                        }
                        data[read..].fill(zero);
                    },
                    make_stream_error_cb(device_gone),
                    None,
                )
            }

            let finished_flag = Arc::new(AtomicBool::new(false));

            let ring_cap =
                (output_config.sample_rate as usize) * (output_config.channels as usize) * 2;
            starvation.begin_stream(output_config.sample_rate, output_config.channels);
            let ring_buf = Arc::new(RingBuf::new_metered(ring_cap, starvation.clone()));
            ring_buf.clear(); // Defensive: zero-fill before callback can read
            // Minimum buffer: ~200ms of audio before the callback starts reading.
            // sr * ch / 5 = 200ms of interleaved samples.
            let min_buffer =
                (output_config.sample_rate as usize) * (output_config.channels as usize) / 5;
            let stream_result = build_stream(
                &output_config,
                ring_buf.clone(),
                volume.clone(),
                paused.clone(),
                finished_flag.clone(),
                silent_cb_outer.clone(),
                data_started_shared.clone(),
                min_buffer,
                soft_mute.clone(),
            );

            let (stream, actual_config, ring) = match stream_result {
                Ok(s) => (s, output_config, ring_buf),
                Err(first_err) => {
                    // Last resort: try the source sample rate directly —
                    // some platforms (PipeWire) accept arbitrary rates.
                    let source_cfg = cpal::StreamConfig {
                        channels,
                        sample_rate,
                        buffer_size: cpal::BufferSize::Default,
                    };
                    let ring_cap_fb =
                        (source_cfg.sample_rate as usize) * (source_cfg.channels as usize) * 2;
                    starvation.begin_stream(source_cfg.sample_rate, source_cfg.channels);
                    let ring_fb = Arc::new(RingBuf::new_metered(ring_cap_fb, starvation.clone()));
                    ring_fb.clear();
                    data_started_shared.store(false, Ordering::SeqCst);
                    let min_buffer_fb =
                        (source_cfg.sample_rate as usize) * (source_cfg.channels as usize) / 2;
                    match build_stream(
                        &source_cfg,
                        ring_fb.clone(),
                        volume.clone(),
                        paused.clone(),
                        finished_flag.clone(),
                        silent_cb_outer.clone(),
                        data_started_shared.clone(),
                        min_buffer_fb,
                        soft_mute.clone(),
                    ) {
                        Ok(s) => {
                            info!(
                                source_sr = sample_rate,
                                "local_audio_fallback_to_source_rate"
                            );
                            (s, source_cfg, ring_fb)
                        }
                        Err(second_err) => {
                            // Both f32 attempts failed. The hardware likely rejects
                            // float (bit-perfect integer-only DAC). Cascade integer
                            // formats — i32 then i16 — at the chosen rate, then the
                            // source rate. First one the device accepts wins.
                            let mut candidates: Vec<cpal::StreamConfig> =
                                vec![output_config.clone()];
                            if source_cfg.sample_rate != output_config.sample_rate
                                || source_cfg.channels != output_config.channels
                            {
                                candidates.push(source_cfg.clone());
                            }
                            let mut built: Option<(
                                cpal::Stream,
                                cpal::StreamConfig,
                                Arc<RingBuf>,
                            )> = None;
                            'int_cascade: for cand in &candidates {
                                let cap =
                                    (cand.sample_rate as usize) * (cand.channels as usize) * 2;
                                let min_buf =
                                    (cand.sample_rate as usize) * (cand.channels as usize) / 5;
                                for is_i32 in [true, false] {
                                    starvation.begin_stream(cand.sample_rate, cand.channels);
                                    let r = Arc::new(RingBuf::new_metered(cap, starvation.clone()));
                                    r.clear();
                                    data_started_shared.store(false, Ordering::SeqCst);
                                    let res = if is_i32 {
                                        build_int_stream::<i32>(
                                            &device,
                                            cand,
                                            r.clone(),
                                            volume.clone(),
                                            paused.clone(),
                                            silent_cb_outer.clone(),
                                            data_started_shared.clone(),
                                            min_buf,
                                            device_gone.clone(),
                                            soft_mute.clone(),
                                        )
                                    } else {
                                        build_int_stream::<i16>(
                                            &device,
                                            cand,
                                            r.clone(),
                                            volume.clone(),
                                            paused.clone(),
                                            silent_cb_outer.clone(),
                                            data_started_shared.clone(),
                                            min_buf,
                                            device_gone.clone(),
                                            soft_mute.clone(),
                                        )
                                    };
                                    if let Ok(s) = res {
                                        info!(
                                            format = if is_i32 { "i32" } else { "i16" },
                                            sample_rate = cand.sample_rate,
                                            "local_audio_fallback_to_integer_format"
                                        );
                                        built = Some((s, cand.clone(), r));
                                        break 'int_cascade;
                                    }
                                }
                            }
                            match built {
                                Some(t) => t,
                                None => {
                                    // Every format was refused, so the fault is
                                    // the device itself, not the encoding. Name
                                    // the likely cause: the raw ALSA string
                                    // ("Host is down (112)" — Yacine) reads as a
                                    // network error and sends people hunting in
                                    // the wrong place, when it is what the
                                    // PipeWire ALSA plugin returns if it cannot
                                    // reach the daemon — typically a server
                                    // started outside the user session, or a
                                    // USB DAC that went away.
                                    let cause = classify_open_failure(&first_err.to_string());
                                    warn!(
                                        device = %device_name,
                                        first_error = %first_err,
                                        second_error = %second_err,
                                        hint = %cause.log_hint(),
                                        "audio_stream_build_failed_all_formats"
                                    );
                                    // Hand the poller something to say. Without
                                    // this the zone plays on in silence until the
                                    // stall heuristics fire ~73 s later, with no
                                    // message anywhere the user can see.
                                    if let Ok(mut slot) = open_failure.lock() {
                                        *slot = Some(format!(
                                            "Sortie « {device_name} » : {}.",
                                            cause.user_message()
                                        ));
                                    }
                                    playing.store(false, Ordering::SeqCst);
                                    return;
                                }
                            }
                        }
                    }
                }
            };

            let output_sr = actual_config.sample_rate;
            let output_ch = actual_config.channels;

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
            let mut skipped_bytes: u64 = 0;
            let mut needs_resample = output_sr != sample_rate;
            let mut needs_channel_adapt = output_ch != channels;

            // Create rubato sinc resampler once for the entire track.
            // Using FixedAsync::Input so we feed fixed-size input chunks.
            let mut resampler: Option<Async<f32>> = if needs_resample {
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
            // Buffer for resampler frame leftover: holds samples that don't
            // fill a complete resampler block, carried over to the next read.
            let mut resample_leftover: Vec<f32> = Vec::new();

            // Read and feed the rest of the stream
            let mut read_buf = vec![0u8; 65536];
            // Seed the leftover buffer with any unaligned remainder from the
            // initial header read so byte alignment is preserved across reads.
            // Previously the remainder was silently dropped, causing every
            // subsequent 24-bit sample to be read from the wrong byte offset
            // (white noise).
            let mut leftover = pcm_data;
            let mut pcm_kind = LocalPcmKind::for_bit_depth(bit_depth);
            let pcm_processor = LocalPcmProcessor {
                eq: &eq,
                convolver: &convolver,
                crossfeed: &crossfeed,
                pure_bypass: &pure_bypass,
                mono_downmix: &mono_downmix,
                dop_active: &dop_active,
                volume: &volume,
                user_volume: &user_volume_ref,
                rg_factor: &rg_factor_ref,
            };

            // Process leftover from header read
            if let Some(processed) = pcm_processor.process_pcm_chunk(
                &mut leftover,
                frame_bytes,
                bit_depth,
                channels,
                &mut pcm_kind,
            ) {
                let mut samples = processed.samples;

                // Diagnostic: log first few f32 samples and detect anomalies.
                // White noise manifests as high-amplitude random values in
                // what should be a gentle attack.
                if !samples.is_empty() {
                    let first_8: Vec<f32> = samples.iter().take(8).copied().collect();
                    let max_abs = samples
                        .iter()
                        .take(200)
                        .fold(0.0f32, |m, &s| m.max(s.abs()));
                    let non_zero = samples.iter().take(200).filter(|&&s| s != 0.0).count();
                    info!(
                        first_samples = ?first_8,
                        max_abs_200 = max_abs,
                        non_zero_in_200 = non_zero,
                        total_samples = samples.len(),
                        bit_depth,
                        frame_bytes,
                        dop = processed.dop,
                        "local_audio_initial_samples_diagnostic"
                    );
                }

                if needs_channel_adapt {
                    samples = adapt_channels(&samples, channels, output_ch);
                }
                if needs_resample {
                    samples = rubato_resample_chunk(
                        &mut resampler,
                        &samples,
                        output_ch,
                        false,
                        &mut resample_leftover,
                    );
                }
                feed_ring_abortable(&ring, &samples, &stop_rx, &paused, Some(&force_silent));
                total_frames_fed += processed.source_frames;
            }
            let mut total_bytes_read: u64 = 0;
            let mut first_data_logged = false;
            let stream_start = std::time::Instant::now();

            // Pre-fill the ring buffer before starting the cpal stream.
            // Target: ~500ms of audio so the first callback has enough data.
            let prefill_target = (output_sr as usize) * (output_ch as usize) / 5; // ~200ms
            let mut stream_started = false;

            // Check if initial header data was enough to meet the prefill target
            if ring.available() >= prefill_target {
                if let Err(e) = stream.play() {
                    warn!(error = %e, "audio_stream_play_failed");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
                stream_started = true;
                info!(
                    demarrage_ms = chrono_demarrage.elapsed().as_millis() as u64,

                    device = %device_name,
                    prefill_samples = ring.available(),
                    "local_audio_playing_after_prefill"
                );
            }

            // Tracks whether the HTTP read loop exited because the source
            // reached EOF (true) vs. a stop signal or read error (false).
            // Only when http_eof=true do we signal track_ended_naturally.
            let mut http_eof = false;

            loop {
                // Check for stop signal (non-blocking)
                if stop_rx.try_recv().is_ok() {
                    debug!(
                        total_bytes_read,
                        total_frames_fed, "local_audio_stopped_by_signal"
                    );
                    break;
                }
                // Check abort flag (set by stop() to force immediate exit)
                if force_silent.load(Ordering::Relaxed) {
                    debug!(
                        total_bytes_read,
                        total_frames_fed, "local_audio_stopped_by_abort_flag"
                    );
                    break;
                }
                // Device vanished mid-track (USB DAC unplugged, #1626): stop
                // reading — nobody will ever play these samples. http_eof stays
                // false so no natural track end is signalled (we must not chain
                // the queue onto a dead device).
                if device_gone.load(Ordering::Relaxed) {
                    warn!(
                        device = %device_name,
                        total_bytes_read,
                        "local_audio_stopped_device_lost"
                    );
                    break;
                }

                let read_start = std::time::Instant::now();
                let n = match reader.read(&mut read_buf) {
                    Ok(0) => {
                        debug!(
                            total_bytes_read,
                            total_frames_fed,
                            elapsed_ms = stream_start.elapsed().as_millis() as u64,
                            "local_audio_stream_eof"
                        );
                        http_eof = true;
                        break; // EOF
                    }
                    Ok(n) => n,
                    Err(ref e)
                        if e.kind() == std::io::ErrorKind::TimedOut
                            || e.kind() == std::io::ErrorKind::WouldBlock =>
                    {
                        // Read timeout — loop back to check abort flag
                        continue;
                    }
                    Err(e) => {
                        warn!(error = %e, total_bytes_read, "local_audio_read_error");
                        http_eof = true;
                        break;
                    }
                };
                let read_elapsed = read_start.elapsed();

                // Log first data arrival and any suspiciously slow reads
                if !first_data_logged {
                    info!(
                        bytes = n,
                        wait_ms = stream_start.elapsed().as_millis() as u64,
                        "local_audio_first_pcm_data_received"
                    );
                    first_data_logged = true;
                } else if read_elapsed.as_millis() > 5000 {
                    warn!(
                        bytes = n,
                        wait_ms = read_elapsed.as_millis() as u64,
                        total_bytes_read,
                        "local_audio_slow_read"
                    );
                }

                total_bytes_read += n as u64;

                // Seek skip: discard PCM bytes until we reach the seek offset
                if skip_bytes > 0 && skipped_bytes < skip_bytes {
                    let remaining_to_skip = (skip_bytes - skipped_bytes) as usize;
                    if n <= remaining_to_skip {
                        skipped_bytes += n as u64;
                        continue;
                    }
                    // Partial skip: some bytes to discard, rest to process
                    let start = remaining_to_skip;
                    skipped_bytes = skip_bytes;
                    leftover.extend_from_slice(&read_buf[start..n]);
                } else {
                    leftover.extend_from_slice(&read_buf[..n]);
                }

                let aligned_len = (leftover.len() / frame_bytes) * frame_bytes;
                if aligned_len == 0 {
                    continue;
                }

                let Some(processed) = pcm_processor.process_pcm_chunk(
                    &mut leftover,
                    frame_bytes,
                    bit_depth,
                    channels,
                    &mut pcm_kind,
                ) else {
                    continue;
                };
                let mut samples = processed.samples;

                // Detect all-zero samples (silence from decode failure)
                if !first_data_logged || total_frames_fed == 0 {
                    let non_zero = samples.iter().any(|&s| s != 0.0);
                    if !non_zero && !samples.is_empty() {
                        warn!(
                            sample_count = samples.len(),
                            "local_audio_first_samples_all_zero"
                        );
                    }
                }

                if needs_channel_adapt {
                    samples = adapt_channels(&samples, channels, output_ch);
                }
                if needs_resample {
                    samples = rubato_resample_chunk(
                        &mut resampler,
                        &samples,
                        output_ch,
                        false,
                        &mut resample_leftover,
                    );
                }

                let fed =
                    feed_ring_abortable(&ring, &samples, &stop_rx, &paused, Some(&force_silent));
                if !fed {
                    // Wedge: the render callback stopped draining the ring
                    // (dead stream after a USB DAC unplug on macOS, where no
                    // error callback fires, #1626). Without this check the loop
                    // stalled 5s on EVERY chunk while the position stood still.
                    warn!(
                        device = %device_name,
                        total_bytes_read,
                        "local_audio_stopped_feed_stall"
                    );
                    // …et sans celle-ci, il s'arrêtait SANS RIEN DIRE (#3108).
                    // `device_gone` ne comble pas le trou : sur macOS le rappel
                    // d'erreur cpal ne se déclenche jamais à l'arrachage, donc
                    // le seul témoin est le blocage qu'on vient de constater.
                    record_feed_stall_failure(
                        "CPAL",
                        &device_name,
                        position_ms.load(Ordering::Relaxed),
                        &open_failure,
                    );
                    break;
                }

                total_frames_fed += processed.source_frames;

                // Start the cpal stream once enough data has been pre-filled.
                // This ensures the audio device never pulls from an empty/sparse
                // ring buffer, eliminating white noise at track start.
                if !stream_started && ring.available() >= prefill_target {
                    if let Err(e) = stream.play() {
                        warn!(error = %e, "audio_stream_play_failed");
                        playing.store(false, Ordering::SeqCst);
                        return;
                    }
                    stream_started = true;
                    info!(
                    demarrage_ms = chrono_demarrage.elapsed().as_millis() as u64,

                        device = %device_name,
                        prefill_samples = ring.available(),
                        total_bytes_read,
                        elapsed_ms = stream_start.elapsed().as_millis() as u64,
                        "local_audio_playing_after_prefill"
                    );
                }

                // Update position
                let pos =
                    (total_frames_fed as f64 / sample_rate as f64 * 1000.0) as u64 + seek_offset;
                position_ms.store(pos, Ordering::Relaxed);
            }

            if http_eof {
                report_incomplete_local_pcm_probe(pcm_kind, leftover.len());
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
                if let Err(e) = stream.play() {
                    warn!(error = %e, "audio_stream_play_failed_final");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
                info!(
                    device = %device_name,
                    ring_available = ring.available(),
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

                info!(
                    new_sr,
                    new_ch,
                    new_bd,
                    prev_sr = sample_rate,
                    prev_ch = channels,
                    prev_bd = bit_depth,
                    "local_audio_gapless_next_track_format"
                );

                let prev_sr = sample_rate;
                let prev_ch = channels;
                let prev_needs_resample = needs_resample;
                let next_needs_resample = output_sr != new_sr;
                let convolver_format_changed = new_sr != prev_sr || new_ch != prev_ch;

                // Un moteur FFT est lié au format source. Avant de le remplacer,
                // rendre sa queue dans l'ANCIEN format et lui faire suivre la
                // même adaptation/rééchantillonnage que la piste qui se termine.
                // À format identique on ne touche à rien : son état fait partie
                // de la continuité gapless.
                if convolver_format_changed {
                    let mut queue = flush_local_dsp(
                        &convolver,
                        &crossfeed,
                        &pure_bypass,
                        &mono_downmix,
                        prev_ch,
                        dop_active.load(Ordering::Relaxed),
                    );
                    if !queue.is_empty() {
                        if needs_channel_adapt {
                            queue = adapt_channels(&queue, prev_ch, output_ch);
                        }
                        if prev_needs_resample {
                            queue = rubato_resample_chunk(
                                &mut resampler,
                                &queue,
                                output_ch,
                                false,
                                &mut resample_leftover,
                            );
                        }
                        feed_ring_abortable(&ring, &queue, &stop_rx, &paused, Some(&force_silent));
                    }
                }

                // À cadence source identique, le resampler fait partie du flux
                // continu : conserver son état et son leftover est nécessaire
                // au vrai gapless. On ne le draine que si le prochain flux
                // impose réellement une autre cadence (ou n'en a plus besoin).
                // Le vidage doit avoir lieu APRÈS validation de l'en-tête : le
                // faire dès qu'un `next_media` existe insérait du silence même
                // quand la requête suivante échouait.
                if prev_needs_resample && (new_sr != prev_sr || !next_needs_resample) {
                    let flushed = rubato_resample_chunk(
                        &mut resampler,
                        &[],
                        output_ch,
                        true,
                        &mut resample_leftover,
                    );
                    if !flushed.is_empty() {
                        feed_ring_abortable(
                            &ring,
                            &flushed,
                            &stop_rx,
                            &paused,
                            Some(&force_silent),
                        );
                    }
                }

                // Update source format variables for the new track
                sample_rate = new_sr;
                channels = new_ch;
                bit_depth = new_bd;
                let new_bps = if new_bd == 0 {
                    4
                } else {
                    (new_bd / 8) as usize
                };
                frame_bytes = new_ch as usize * new_bps;
                needs_channel_adapt = output_ch != new_ch;
                needs_resample = next_needs_resample;
                pcm_kind = LocalPcmKind::for_bit_depth(new_bd);
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
                if needs_resample && new_sr != prev_sr {
                    // Sample rate changed — flush old resampler residuals
                    resample_leftover.clear();
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
                    resampler = match Async::<f32>::new_sinc(
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
                            needs_resample = false;
                            None
                        }
                    };
                    resample_leftover.clear();
                } else if !needs_resample && resampler.is_some() {
                    resampler = None;
                    resample_leftover.clear();
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
                leftover.clear();
                http_eof = false;

                // Process initial PCM data from the header read
                let gapless_pcm = if new_data_offset < next_header.len() {
                    next_header[new_data_offset..].to_vec()
                } else {
                    Vec::new()
                };
                leftover.extend_from_slice(&gapless_pcm);
                if let Some(processed) = pcm_processor.process_pcm_chunk(
                    &mut leftover,
                    frame_bytes,
                    bit_depth,
                    channels,
                    &mut pcm_kind,
                ) {
                    let mut smp = processed.samples;
                    // Même frontière que la piste initiale : la piste chaînée
                    // conserve l'état du DSP mais prend une nouvelle décision
                    // PCM/DoP avant son premier échantillon (#2296/#2232).
                    if needs_channel_adapt {
                        smp = adapt_channels(&smp, channels, output_ch);
                    }
                    if needs_resample {
                        smp = rubato_resample_chunk(
                            &mut resampler,
                            &smp,
                            output_ch,
                            false,
                            &mut resample_leftover,
                        );
                    }
                    feed_ring_abortable(&ring, &smp, &stop_rx, &paused, Some(&force_silent));
                    total_frames_fed += processed.source_frames;
                }

                // Main read loop for the gapless-chained track
                let mut gapless_read_buf = vec![0u8; 65536];
                loop {
                    if stop_rx.try_recv().is_ok() || force_silent.load(Ordering::Relaxed) {
                        break;
                    }
                    // Device lost mid-chain (#1626): abort without signalling
                    // a natural end, like the main read loop above.
                    if device_gone.load(Ordering::Relaxed) {
                        warn!(
                            device = %device_name,
                            total_bytes_read,
                            "local_audio_gapless_stopped_device_lost"
                        );
                        http_eof = false;
                        break;
                    }
                    match next_reader.read(&mut gapless_read_buf) {
                        Ok(0) => {
                            debug!(
                                total_bytes_read,
                                total_frames_fed, "local_audio_gapless_track_eof"
                            );
                            http_eof = true;
                            break;
                        }
                        Ok(n) => {
                            total_bytes_read += n as u64;
                            leftover.extend_from_slice(&gapless_read_buf[..n]);
                            if (leftover.len() / frame_bytes) * frame_bytes == 0 {
                                continue;
                            }
                            let Some(processed) = pcm_processor.process_pcm_chunk(
                                &mut leftover,
                                frame_bytes,
                                bit_depth,
                                channels,
                                &mut pcm_kind,
                            ) else {
                                continue;
                            };
                            let mut smp = processed.samples;
                            if needs_channel_adapt {
                                smp = adapt_channels(&smp, channels, output_ch);
                            }
                            if needs_resample {
                                smp = rubato_resample_chunk(
                                    &mut resampler,
                                    &smp,
                                    output_ch,
                                    false,
                                    &mut resample_leftover,
                                );
                            }
                            let fed = feed_ring_abortable(
                                &ring,
                                &smp,
                                &stop_rx,
                                &paused,
                                Some(&force_silent),
                            );
                            if !fed {
                                // Dead consumer (see main loop, #1626).
                                warn!(
                                    device = %device_name,
                                    total_bytes_read,
                                    "local_audio_gapless_stopped_feed_stall"
                                );
                                // Même canal que la boucle principale (#3108) :
                                // une piste enchaînée qui meurt en silence est
                                // aussi muette qu'une première piste.
                                record_feed_stall_failure(
                                    "CPAL",
                                    &device_name,
                                    position_ms.load(Ordering::Relaxed),
                                    &open_failure,
                                );
                                http_eof = false;
                                break;
                            }
                            total_frames_fed += processed.source_frames;
                            let pos = (total_frames_fed as f64 / sample_rate as f64 * 1000.0)
                                as u64
                                + seek_offset;
                            position_ms.store(pos, Ordering::Relaxed);
                        }
                        Err(ref e)
                            if e.kind() == std::io::ErrorKind::TimedOut
                                || e.kind() == std::io::ErrorKind::WouldBlock =>
                        {
                            continue;
                        }
                        Err(e) => {
                            warn!(error = %e, "local_audio_gapless_read_error");
                            http_eof = true;
                            break;
                        }
                    }
                }

                if http_eof {
                    report_incomplete_local_pcm_probe(pcm_kind, leftover.len());
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
                let mut queue = flush_local_dsp(
                    &convolver,
                    &crossfeed,
                    &pure_bypass,
                    &mono_downmix,
                    channels,
                    dop_active.load(Ordering::Relaxed),
                );
                if !queue.is_empty() {
                    if needs_channel_adapt {
                        queue = adapt_channels(&queue, channels, output_ch);
                    }
                    if needs_resample {
                        // La queue est d'abord un bloc normal. `flush = true`
                        // ignore son argument `samples` et la jetterait.
                        queue = rubato_resample_chunk(
                            &mut resampler,
                            &queue,
                            output_ch,
                            false,
                            &mut resample_leftover,
                        );
                    }
                    feed_ring_abortable(&ring, &queue, &stop_rx, &paused, Some(&force_silent));
                }
            }

            // Flush the resampler: process any leftover frames + drain internal delay
            if http_eof
                && needs_resample
                && !force_silent.load(Ordering::Relaxed)
                && !device_gone.load(Ordering::Relaxed)
            {
                let flushed = rubato_resample_chunk(
                    &mut resampler,
                    &[],
                    output_ch,
                    true,
                    &mut resample_leftover,
                );
                if !flushed.is_empty() {
                    feed_ring_abortable(&ring, &flushed, &stop_rx, &paused, Some(&force_silent));
                }
            }

            // Signal that HTTP reading is done
            finished_flag.store(true, Ordering::SeqCst);

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
            let fed_position_ms = position_ms.load(Ordering::Relaxed);
            let mut drained_naturally = false;
            // NEVER drain forever: with a dead render callback (USB DAC hot-
            // unplugged, #1626 — on macOS no error callback ever fires) the
            // ring stays full and this loop used to spin until restart, keeping
            // the zone "Playing" and freezing the hotplug rescan. Deadline =
            // queued audio duration + 5s margin (same guard as the ASIO
            // exclusive path's asio_drain_timeout).
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
                let remaining = ring.available();
                if remaining == 0 {
                    drained_naturally = true;
                    break;
                }
                if device_gone.load(Ordering::Relaxed) || drain_started.elapsed() >= drain_deadline
                {
                    // No natural end: the tail was never actually played, and
                    // advancing the queue would immediately hit the same dead
                    // device.
                    warn!(
                        device = %device_name,
                        remaining_samples = remaining,
                        device_gone = device_gone.load(Ordering::Relaxed),
                        "local_audio_drain_timeout"
                    );
                    break;
                }
                // Report real playback: subtract the still-queued ring content
                // (interleaved f32 samples at the output rate/channels).
                if output_sr > 0 && output_ch > 0 {
                    let ring_ms =
                        (remaining as f64 / output_ch as f64 / output_sr as f64 * 1000.0) as u64;
                    position_ms.store(fed_position_ms.saturating_sub(ring_ms), Ordering::Relaxed);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }

            if http_eof && drained_naturally {
                position_ms.store(fed_position_ms, Ordering::Relaxed);
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

            drop(stream);
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
                        debug!("local_audio_stop_thread_detached — old stream exits in background");
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
) -> impl FnMut(cpal::StreamError) + Send + 'static {
    let mut last_warn: Option<std::time::Instant> = None;
    move |e: cpal::StreamError| {
        if matches!(e, cpal::StreamError::DeviceNotAvailable) {
            if !device_gone.swap(true, Ordering::SeqCst) {
                warn!(error = %e, "audio_stream_device_lost");
            }
            return;
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

/// Ce qu'une zone connaît d'un périphérique de sortie, tel que la découverte
/// CPAL l'a vu : l'identifiant d'endpoint stable du backend (vide sur les
/// hôtes qui n'en exposent aucun) et le nom **brut** rendu par le pilote,
/// avant toute désambiguïsation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceIdentity {
    pub(crate) endpoint_id: String,
    pub(crate) raw_name: String,
    /// L'hôte audio qui a ÉNUMÉRÉ ce périphérique (`"Wasapi"`, `"Asio"`,
    /// `"Alsa"`, `"CoreAudio"` — la variante cpal, telle que
    /// `cpal::Host::id().name()` la rend).
    ///
    /// #3230 : sans ce champ, un nom n'était rattaché à rien. « Haut-parleurs »
    /// est un nom WASAPI ; le chercher parmi des sorties ASIO n'a aucun sens,
    /// et échouer y renvoyait la zone sur le périphérique ASIO par défaut. Un
    /// nom porte désormais l'hôte dont il vient, et la résolution les apparie.
    pub(crate) host: String,
}

/// Par quoi une zone a été rattachée à son périphérique. Le rang porté par
/// chaque variante est l'indice dans la liste énumérée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeviceMatch {
    /// Retrouvé par identifiant d'endpoint stable. C'est le seul appariement
    /// qui survive à un renommage.
    ByEndpointId(usize),
    /// Retrouvé par nom d'affichage, avec la convention `(n)` de la
    /// découverte — donc en distinguant les homonymes.
    ByDisplayName(usize),
    /// Retrouvé par sous-chaîne, et par une seule candidate.
    BySubstring(usize),
}

impl DeviceMatch {
    pub(crate) fn index(self) -> usize {
        match self {
            Self::ByEndpointId(i) | Self::ByDisplayName(i) | Self::BySubstring(i) => i,
        }
    }
}

/// Le verdict de [`resolve_device`]. Trois issues, pas deux : « introuvable »
/// et « pas d'ici » n'appellent pas la même conduite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeviceResolution {
    /// Le périphérique de la zone a été retrouvé sur l'hôte ouvert.
    Matched(DeviceMatch),
    /// Le nom de la zone vient d'un AUTRE hôte. Aucun appariement n'est
    /// possible, et le repli sur le défaut serait un détournement : c'est
    /// exactement lui qui envoyait Jean Valjean sur une sortie ASIO qu'il
    /// n'avait jamais choisie (#3230). L'appelant doit **refuser**.
    ForeignHost {
        requested_host: String,
        open_host: String,
    },
    /// Le bon hôte, mais plus aucun périphérique de ce nom : débranché,
    /// renommé, routage macOS changé. L'appelant retombe sur la sortie
    /// système — en le disant (#2207).
    NotFound,
}

/// Résoudre le périphérique qu'une zone désigne.
///
/// Le nom d'affichage n'est **pas** une identité : il n'est ni unique (deux
/// DAC USB s'annoncent tous deux « Haut-Parleurs », #2272) ni stable (Windows
/// renomme l'endpoint au changement de taux d'échantillonnage, #2269).
/// L'ordre ci-dessous met donc l'identifiant d'endpoint stable capturé à la
/// découverte (#2207) devant le nom :
///
/// 1. **identifiant d'endpoint** — insensible au renommage comme à l'ordre
///    d'énumération ;
/// 2. **nom d'affichage** reconstruit avec la convention `(n)` **exacte** de
///    la découverte, si bien que « Haut-Parleurs (2) » atteint le second
///    homonyme et non le premier ;
/// 3. **sous-chaîne**, tolérance héritée pour les hôtes aux noms verbeux
///    (CoreAudio, PipeWire) — mais seulement si elle désigne **une seule**
///    candidate, et jamais pour un nom que Tune a lui-même suffixé d'un
///    `(n)` : ce suffixe vient de nous, pas du pilote, et le laisser glisser
///    par sous-chaîne est précisément ce qui envoyait le son sur le mauvais
///    DAC en silence.
///
/// [`DeviceResolution::NotFound`] veut dire « je ne sais pas », et non « prends
/// le premier venu » : l'appelant retombe alors sur la sortie par défaut, mais
/// en le **disant**.
///
/// # L'hôte, avant tout le reste (#3230)
///
/// Un nom de périphérique n'a de sens **que** rapporté à l'hôte qui l'a
/// énuméré. « Haut-parleurs » est un nom WASAPI ; aucune sortie ASIO ne le
/// porte, ne l'a jamais porté, et ne le portera jamais. Quand la zone sait de
/// quel hôte vient son nom (`requested_host`) et que cet hôte n'est pas celui
/// qui est ouvert, les trois étapes ci-dessous sont **sautées** et la demande
/// est refusée : c'est le seul verdict honnête, et c'est ce qui empêche le
/// repli de détourner la zone vers le périphérique par défaut d'un hôte
/// qu'elle n'a jamais choisi.
///
/// `requested_host = None` (origine inconnue : zone d'avant ce correctif, ou
/// sortie recréée à la volée) rend exactement le comportement d'avant. Une
/// machine à un seul hôte ne voit donc **aucune** différence : l'hôte
/// d'origine y est toujours celui qui est ouvert.
pub(crate) fn resolve_device(
    requested: &str,
    requested_endpoint_id: Option<&str>,
    requested_host: Option<&str>,
    open_host: &str,
    candidates: &[DeviceIdentity],
) -> DeviceResolution {
    // 0. L'hôte. Un nom qui vient d'ailleurs ne s'apparie à rien ici, et
    //    surtout ne doit pas glisser jusqu'au repli sur le défaut.
    //
    //    L'hôte ouvert est un PARAMÈTRE et non une déduction sur les
    //    candidates : une énumération vide — pilote ASIO happé par une autre
    //    application entre l'élection de l'hôte et l'ouverture — ne doit pas
    //    faire disparaître le refus. Un fait connu de l'appelant ne se redevine
    //    pas ici.
    if let Some(origin) = requested_host.filter(|h| !h.is_empty())
        && !open_host.is_empty()
        && !open_host.eq_ignore_ascii_case(origin)
    {
        return DeviceResolution::ForeignHost {
            requested_host: origin.to_string(),
            open_host: open_host.to_string(),
        };
    }

    // Seules les candidates du bon hôte sont appariables. Les rangs `(n)` sont
    // reconstruits sur cette même liste, comme la découverte les a calculés.
    let matchable: Vec<(usize, &DeviceIdentity)> = candidates
        .iter()
        .enumerate()
        .filter(
            |(_, candidate)| match requested_host.filter(|h| !h.is_empty()) {
                Some(origin) => {
                    candidate.host.is_empty() || candidate.host.eq_ignore_ascii_case(origin)
                }
                None => true,
            },
        )
        .collect();

    // 1. L'identifiant d'endpoint stable, quand la zone en connaît un. C'est
    //    le seul appariement qui traverse un renommage ou un réordonnancement.
    if let Some(endpoint_id) = requested_endpoint_id.filter(|id| !id.is_empty())
        && let Some(&(index, _)) = matchable
            .iter()
            .find(|(_, candidate)| candidate.endpoint_id == endpoint_id)
    {
        return DeviceResolution::Matched(DeviceMatch::ByEndpointId(index));
    }

    let search = requested.to_lowercase();

    // 2. Le nom d'affichage, reconstruit avec la convention de la découverte —
    //    c'est ce nom-là, suffixe compris, qui a été stocké dans la zone.
    let mut seen_names = std::collections::HashSet::new();
    for &(index, candidate) in &matchable {
        let display_name = disambiguate_display_name(&candidate.raw_name, &mut seen_names);
        if display_name.to_lowercase() == search {
            return DeviceResolution::Matched(DeviceMatch::ByDisplayName(index));
        }
    }

    // 3. Sous-chaîne. Un `(n)` qui n'a trouvé personne à l'étape 2 ne se
    //    rattrape pas ici : ce suffixe vient de nous, pas du pilote, et le
    //    laisser glisser renvoyait « Haut-Parleurs (2) » sur le premier
    //    « Haut-Parleurs » — le mauvais DAC, en silence (#2272).
    if looks_disambiguated(requested) {
        return DeviceResolution::NotFound;
    }
    let mut ambigus = matchable.iter().filter(|(_, candidate)| {
        let lower = candidate.raw_name.to_lowercase();
        lower.contains(&search) || search.contains(&lower)
    });
    match (ambigus.next(), ambigus.next()) {
        (Some(&(index, _)), None) => DeviceResolution::Matched(DeviceMatch::BySubstring(index)),
        // Deux candidates : choisir la première, c'est rejouer le même défaut
        // sous un autre nom. On préfère l'aveu d'ignorance.
        _ => DeviceResolution::NotFound,
    }
}

/// La convention de désambiguïsation des noms d'affichage, en **un seul**
/// endroit.
///
/// Plusieurs DAC USB s'annoncent souvent sous le même nom (« Haut-Parleurs »)
/// sous WASAPI. La découverte suffixe le second « (2) », le troisième « (3) »,
/// en sautant les rangs qu'un pilote occupe déjà de lui-même. La résolution
/// doit rejouer **exactement** ce calcul, puisque c'est son résultat qui a été
/// stocké dans la zone : un simple compteur d'occurrences, plus court à
/// écrire, diverge dès `["A", "A (2)", "A"]` — il donnerait « A (2) » au
/// troisième, qui volerait alors la zone du deuxième.
fn disambiguate_display_name(
    raw_name: &str,
    seen_names: &mut std::collections::HashSet<String>,
) -> String {
    let name = if seen_names.contains(raw_name) {
        let mut n = 2;
        loop {
            let candidate = format!("{raw_name} ({n})");
            if !seen_names.contains(&candidate) {
                break candidate;
            }
            n += 1;
        }
    } else {
        raw_name.to_string()
    };
    seen_names.insert(name.clone());
    name
}

/// Puits nuls d'ALSA, qui ne produisent aucun son. Écartés à la découverte
/// **et** à la résolution : les deux doivent voir exactement la même liste,
/// faute de quoi les rangs `(n)` qu'elles calculent peuvent diverger.
fn is_null_sink(raw_name: &str) -> bool {
    raw_name.contains("Discard all samples") || raw_name.contains("Dummy")
}

/// Le nom demandé porte-t-il un suffixe de rang `(n)` posé par la découverte ?
fn looks_disambiguated(requested: &str) -> bool {
    requested
        .rsplit_once(" (")
        .and_then(|(_, tail)| tail.strip_suffix(')'))
        .and_then(|rank| rank.parse::<u32>().ok())
        .is_some_and(|rank| rank >= 2)
}

/// Find an audio output device by name, falling back to the default device if
/// the requested device is not found.
///
/// On macOS (and USB DACs in general), device IDs/names can change between
/// reboots, reconnections, or macOS audio routing changes.  When the stored
/// zone `device_name` no longer matches any enumerated device, playback would
/// silently fail with no audio output.  This function prevents that by falling
/// back to the system default output device and logging a clear warning.
///
/// Returns `(device, fell_back)` where `fell_back` is `true` if the default
/// device was used instead of the requested one.
///
/// `origin_host` est l'hôte qui a ÉNUMÉRÉ le nom que porte la zone
/// (`AudioDevice::backend`). Quand il est connu et qu'il diffère de l'hôte
/// ouvert, la fonction rend `None` **sans repli** : c'est le refus de #3230.
/// `None` = origine inconnue, comportement d'avant.
fn find_device_with_fallback(
    host: &cpal::Host,
    device_name: &str,
    endpoint_id: Option<&str>,
    origin_host: Option<&str>,
) -> Option<(cpal::Device, bool)> {
    if device_name == "default" {
        return host.default_output_device().map(|d| {
            // Demander « default » et obtenir le périphérique système n'est pas
            // un écart — mais l'écran doit quand même pouvoir NOMMER ce qui a
            // été ouvert : « default » ne dit rien à personne.
            note_opened_device(
                observed_backend_name(),
                device_name,
                &d.description()
                    .map(|desc| desc.name().to_string())
                    .unwrap_or_else(|_| "unknown".into()),
                d.id().ok().map(|id| id.to_string()).as_deref(),
            );
            (d, false)
        });
    }

    // La même liste que la découverte, puits nuls écartés compris : c'est la
    // condition pour que les rangs `(n)` reconstruits ici soient ceux qui ont
    // été stockés dans la zone.
    // L'hôte qui énumère est celui qui a produit ces noms — c'est lui qu'un nom
    // « porte », et c'est cette étiquette-là que la résolution apparie.
    let open_host: &'static str = host.id().name();

    let (devices, identities): (Vec<cpal::Device>, Vec<DeviceIdentity>) = host
        .output_devices()
        .map(|devs| devs.collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|device| {
            let identity = DeviceIdentity {
                endpoint_id: device.id().map(|id| id.to_string()).unwrap_or_default(),
                raw_name: device
                    .description()
                    .map(|desc| desc.name().to_string())
                    .unwrap_or_else(|_| "Unknown".into()),
                host: open_host.to_string(),
            };
            (device, identity)
        })
        .filter(|(_, identity)| !is_null_sink(&identity.raw_name))
        .unzip();

    let resolution = resolve_device(
        device_name,
        endpoint_id,
        origin_host,
        open_host,
        &identities,
    );

    // Le nom vient d'un autre hôte : REFUS. Retomber sur le défaut de l'hôte
    // ouvert, c'est le détournement de #3230 — la zone se met à jouer sur un
    // périphérique qu'elle n'a jamais nommé, sans que rien ne le dise.
    if let DeviceResolution::ForeignHost {
        requested_host,
        open_host: opened,
    } = &resolution
    {
        warn!(
            requested = %device_name,
            requested_host = %requested_host,
            open_host = %opened,
            fallback_reason = LocalDeviceFallback::ForeignHost.code(),
            "audio_device_foreign_host_refused — \
             the device this zone remembers was enumerated by another audio host; \
             refusing to hijack the zone onto this host's default output"
        );
        note_device_outcome(
            observed_backend_name(),
            device_name,
            "",
            None,
            Some(LocalDeviceFallback::ForeignHost),
        );
        return None;
    }

    if let DeviceResolution::Matched(matched) = resolution {
        let index = matched.index();
        debug!(
            requested = %device_name,
            resolved = %identities[index].raw_name,
            endpoint_id = %identities[index].endpoint_id,
            matched_by = ?matched,
            "audio_device_resolved"
        );
        // Le nom RÉSOLU, pas le nom demandé : la résolution accepte les
        // correspondances approchées (endpoint id, rang `(n)`, casse), donc les
        // deux peuvent légitimement différer — et c'est précisément ce que
        // l'utilisateur doit voir plutôt que de le déduire d'un `debug!`.
        note_opened_device(
            observed_backend_name(),
            device_name,
            &identities[index].raw_name,
            Some(identities[index].endpoint_id.as_str()),
        );
        // `nth` plutôt qu'un clone : `cpal::Device` n'est pas clonable sur tous
        // les hôtes, et on n'a plus besoin des autres.
        return devices.into_iter().nth(index).map(|device| (device, false));
    }

    // Device not found — log available devices and fall back to default
    let available: Vec<String> = identities
        .iter()
        .map(|identity| format!("{} [{}]", identity.raw_name, identity.endpoint_id))
        .collect();

    if let Some(default_device) = host.default_output_device() {
        let default_name = default_device
            .description()
            .map(|desc| desc.name().to_string())
            .unwrap_or_else(|_| "unknown".into());
        warn!(
            requested = %device_name,
            requested_endpoint_id = endpoint_id.unwrap_or("<aucun>"),
            fallback = %default_name,
            available = ?available,
            "audio_device_not_found_falling_back_to_default — \
             the configured device is unavailable (unplugged, renamed, or \
             macOS audio routing changed); using the system default output \
             device instead"
        );
        // LE cas de #2207, rendu visible : la zone demandait un DAC, la lecture
        // part sur le périphérique système. `differs` vaudra `true`, et le
        // motif nomme désormais la cause plutôt que de la laisser deviner.
        note_device_outcome(
            observed_backend_name(),
            device_name,
            &default_name,
            default_device.id().ok().map(|id| id.to_string()).as_deref(),
            Some(LocalDeviceFallback::NotFoundFellBackToDefault),
        );
        Some((default_device, true))
    } else {
        warn!(
            requested = %device_name,
            requested_endpoint_id = endpoint_id.unwrap_or("<aucun>"),
            available = ?available,
            "audio_device_not_found_no_default_available"
        );
        None
    }
}

/// Probe a device's capabilities when `supported_output_configs()` fails or
/// returns an empty set (common with PipeWire's ALSA compatibility layer).
///
/// Strategy:
/// 1. Try `default_output_config()` — this often works even when enumeration
///    doesn't (PipeWire handles it at the session-manager level).
/// 2. If that also fails, assume conservative defaults: stereo, 44100+48000 Hz.
///    PipeWire will accept these and resample internally.
/// Probe a device's capabilities when `supported_output_configs()` is
/// unavailable. Returns `(max_channels, sample_rates, caps_reliable)`.
///
/// `caps_reliable` is true when the caps came from the device's real default
/// config, false when they are the last-resort assumed stereo guess. Callers
/// must NOT collapse two devices as duplicates on unreliable caps: a generic
/// "Haut-Parleurs" USB DAC and the onboard output both fall to the same assumed
/// `(2, [44100,48000])` on Windows, and collapsing would wrongly drop the DAC
/// (Alain, #1084).
fn probe_device_fallback_caps(device: &cpal::Device, name: &str) -> (u16, Vec<u32>, bool) {
    if let Ok(default_cfg) = device.default_output_config() {
        let cfg = default_cfg.config();
        let ch = cfg.channels;
        let sr = cfg.sample_rate;
        // The default config gives us one known-good rate.  Also include
        // the other standard rate (44100 or 48000) since PipeWire's
        // resampler handles both transparently.
        let mut rates = vec![sr];
        let peer = if sr == 48000 { 44100 } else { 48000 };
        if !rates.contains(&peer) {
            rates.push(peer);
        }
        rates.sort();
        info!(
            device = %name,
            channels = ch,
            default_sr = sr,
            rates = ?rates,
            "local_audio_device_fallback_via_default_config"
        );
        (ch, rates, true)
    } else {
        // Last resort: assume stereo 44100/48000.  PipeWire will accept
        // these through its ALSA PCM plugin even without enumeration.
        info!(
            device = %name,
            "local_audio_device_fallback_to_assumed_stereo_44100_48000"
        );
        (2, vec![44100, 48000], false)
    }
}

/// Ce que vaut la liste de cadences qu'une sortie locale annonce.
///
/// `supported_output_configs()` de cpal n'a pas le même sens selon l'hôte :
///
/// - **ALSA** interroge le pilote cadence par cadence (`hw_params.test_rate`)
///   et écarte celles qu'il refuse ;
/// - **ASIO** fait de même (`driver.can_sample_rate`, `continue` si non) ;
/// - **WASAPI** ne demande rien à personne. `is_format_supported` rend
///   `Ok(true)` sans regarder le format — commentaire d'origine dans
///   `cpal-0.17.3/src/host/wasapi/device.rs:192-200` : « Checking formats is
///   not needed for shared mode with auto-conversion, therefore this check has
///   been removed » — et `supported_formats()` déroule alors le produit
///   cartésien des 21 `COMMON_SAMPLE_RATES` par les 7 formats d'échantillon.
///   Chaque entrée est une plage ponctuelle (`min == max`), si bien que deux
///   DAC Windows différents reçoivent exactement la MÊME liste de 147 entrées.
///
/// Tune ne peut pas corriger cpal. Il peut cesser de présenter cette liste
/// comme une capacité constatée (#2862).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleRateEvidence {
    /// Le pilote a été interrogé, cadence par cadence.
    Measured,
    /// Aucune confrontation au matériel : la liste est une supposition.
    Unverified,
}

impl SampleRateEvidence {
    /// Vrai seulement quand la liste vient d'une interrogation du pilote.
    pub fn is_measured(self) -> bool {
        matches!(self, Self::Measured)
    }
}

/// L'énumération de cpal est-elle une MESURE, pour cet hôte ?
///
/// La plateforme est un **paramètre**, jamais un `cfg!` refermé dans le corps :
/// sinon la décision Windows ne serait compilée que sous Windows, et aucun test
/// joué sur Linux ne pourrait la contredire — l'angle mort de #1837 et #2056.
/// Un seul appelant passe la valeur réelle de la machine.
///
/// `backend` est ce que rend `cpal::HostId::name()`, c'est-à-dire le nom de la
/// **variante** (`"Wasapi"`, `"Alsa"`, `"Asio"`, `"CoreAudio"`) et non un
/// libellé d'affichage : `name()` est un `stringify!` sur l'identifiant de
/// variante. La comparaison est insensible à la casse pour ne pas dépendre de
/// ce détail.
pub fn sample_rate_evidence(backend: &str) -> SampleRateEvidence {
    match backend.to_ascii_lowercase().as_str() {
        "alsa" | "asio" | "coreaudio" | "jack" => SampleRateEvidence::Measured,
        // « wasapi » : cpal ne teste rien (voir ci-dessus). Et tout hôte
        // inconnu tombe ici volontairement — on ne prête pas une mesure à un
        // backend dont on ignore ce qu'il fait.
        _ => SampleRateEvidence::Unverified,
    }
}

/// Le nom de PCM ALSA porté par un `endpoint_id`, sans le préfixe d'hôte.
///
/// cpal rend `DeviceId` sous la forme `«hôte»:«pcm»` (`Display`, `cpal-0.17.3`
/// `src/lib.rs:255`), et le `pcm` d'ALSA est lui-même préfixé par son greffon
/// (`hw:CARD=…`, `dmix:CARD=…`). On ne retire donc QUE le préfixe d'hôte, et
/// seulement s'il est présent : certains enregistrements ne portent que le PCM.
fn alsa_pcm_name(endpoint_id: &str) -> &str {
    let Some((tete, reste)) = endpoint_id.split_once(':') else {
        return endpoint_id;
    };
    if tete.eq_ignore_ascii_case("alsa") {
        reste
    } else {
        endpoint_id
    }
}

/// Ce PCM ALSA parle-t-il au MATÉRIEL, ou à un convertisseur logiciel ?
///
/// `snd_device_name_hint` expose la même carte sous une dizaine de noms qui
/// partagent tous la même première ligne de description — c'est pourquoi le
/// dédoublonnage Linux les regroupe. Un seul de ces noms atteint le pilote sans
/// conversion : `hw:`. Tous les autres (`default`, `sysdefault:`, `plughw:`,
/// `dmix:`, `plug:`, `front:`, `iec958:`, `pipewire`, `pulse`, `jack`) passent
/// par un greffon qui ACCEPTE tout et rééchantillonne.
///
/// La distinction n'est pas cosmétique : `dmix` fixe la cadence de son esclave
/// (`defaults.pcm.dmix.rate 48000` dans `alsa.conf`). Interroger un tel PCM
/// cadence par cadence rend « oui » pour 44,1 → 384 kHz, mais c'est le
/// convertisseur qui répond, pas le DAC.
pub fn alsa_pcm_is_direct_hardware(endpoint_id: &str) -> bool {
    alsa_pcm_name(endpoint_id)
        .split(':')
        .next()
        .is_some_and(|greffon| greffon.eq_ignore_ascii_case("hw"))
}

/// Ce que vaut la liste de cadences d'UN périphérique — pas seulement de son hôte.
///
/// [`sample_rate_evidence`] répond pour l'hôte ; elle ne peut pas voir deux
/// faits qui, eux, sont propres au périphérique :
///
/// 1. **Le PCM interrogé n'est pas forcément le matériel.** Sur ALSA, cpal
///    interroge bien le pilote (`hw_params.test_rate`) — mais le « pilote »
///    d'un `dmix:` ou d'un `plughw:` est un rééchantillonneur logiciel. GgB
///    (#1655, Eversolo DAC-Z8) : l'écran annonce 44,1 → 384 kHz « mesurées »,
///    `local_audio_stream_config` note `output_sr=192000`, et
///    `/proc/asound/card0/stream0` montre l'endpoint USB à 48 kHz nominal.
///    C'est le greffon qui a dit oui.
/// 2. **La liste peut être une SUPPOSITION.** Quand l'énumération échoue,
///    [`probe_device_fallback_caps`] invente `(2, [44100, 48000])` et le
///    signale par `caps_reliable = false` — un drapeau que l'énumération
///    calculait puis jetait (`let _ = caps_reliable`).
///
/// Aucune de ces deux réserves ne change ce qui est JOUÉ : elles changent ce
/// que l'écran a le droit d'affirmer.
pub fn sample_rate_evidence_for_device(
    backend: &str,
    endpoint_id: &str,
    enumeration_answered: bool,
) -> SampleRateEvidence {
    if !enumeration_answered {
        return SampleRateEvidence::Unverified;
    }
    if backend.eq_ignore_ascii_case("alsa") && !alsa_pcm_is_direct_hardware(endpoint_id) {
        return SampleRateEvidence::Unverified;
    }
    sample_rate_evidence(backend)
}

/// À quelle cadence le chemin cpal **partagé** doit ouvrir le flux.
///
/// #3233 — Pierre M, fil 1043 : « DSD : le temps défile, pas de son ». La
/// décision se fondait sur `find_matching_config(..).filter(|c| c.sample_rate
/// == sample_rate)`, un filtre **tautologique** dès que l'énumération est
/// fabriquée : `find_matching_config` recopie la cadence demandée dans le
/// `StreamConfig` qu'il rend, donc l'égalité est vraie par construction. Sur
/// WASAPI, cpal retient les 21 `COMMON_SAMPLE_RATES` sans rien demander à
/// personne ([`sample_rate_evidence`]) — la branche était TOUJOURS prise, un
/// DSD64 décodé à 176 400 Hz était ouvert à 176 400 Hz quoi que sache faire
/// l'endpoint, `needs_resample` restait faux et rubato ne tournait jamais.
///
/// **#2862 a rendu la liste honnête ; il n'a pas changé la décision qui s'en
/// sert.** C'est ce que fait cette fonction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalRateOpening {
    /// Le périphérique **tourne déjà** à la cadence de la source : sa
    /// configuration par défaut le dit, et celle-là est un fait mesuré sur
    /// toutes les plateformes (`GetMixFormat` sur WASAPI). Aucune conversion,
    /// aucune preuve à réclamer.
    DeviceAlreadyAtSourceRate,
    /// L'énumération retient la cadence **et** elle est une MESURE (ALSA `hw:`,
    /// ASIO, CoreAudio) : on ouvre à la cadence de la source, comme avant. Le
    /// témoin du cas nominal.
    AtSourceRateMeasured,
    /// On n'ouvre pas à la cadence de la source : le flux est ouvert à celle du
    /// périphérique et rubato convertit. La conversion est une DÉCISION, elle
    /// est journalisée et remontée au client.
    ResampleToDeviceRate {
        device_sample_rate: u32,
        reason: LocalRateFallback,
    },
    /// Le périphérique n'annonce **aucune** cadence par défaut (PipeWire,
    /// énumération muette) : il n'y a rien vers quoi rééchantillonner. On ouvre
    /// à la cadence de la source en dernier recours — comportement de toujours,
    /// aucune régression.
    LastResortSourceRate,
}

/// La règle, isolée de cpal pour être éprouvée depuis n'importe quelle machine.
///
/// L'hôte n'est pas un `cfg!` : il entre par `evidence`, sur le modèle de
/// [`exclusive_mode_support`] et de [`sample_rate_evidence`]. Une décision
/// Windows enfermée dans un `cfg!` ne serait pas compilée sur Linux, et le test
/// qui l'interroge y serait vert pour la mauvaise raison (#1837, #2056).
///
/// `enumeration_accepts_source_rate` est ce que répond `find_matching_config`,
/// filtre compris. Cette réponse n'est plus SUFFISANTE : elle n'est prise au
/// mot que lorsque `evidence` dit qu'elle a été mesurée. Le drapeau n'est
/// consulté qu'à défaut de `device_default_rate == Some(source_sample_rate)`,
/// cas où l'appelant n'a même pas besoin d'énumérer.
///
/// **Ce qu'on renonce à faire, et pourquoi.** Sonder réellement l'endpoint
/// serait le plus juste, mais aucune sonde n'existe sur ce chemin : cpal a
/// retiré son `IsFormatSupported` en mode partagé (« Checking formats is not
/// needed for shared mode with auto-conversion »,
/// `cpal-0.17.3/src/host/wasapi/device.rs:192-200`) et
/// `build_output_stream` réussit de toute façon, puisque le flux est initialisé
/// avec `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`. Une sonde par ouverture serait
/// donc **elle aussi tautologique** — et coûteuse. On retient donc le seul fait
/// que le périphérique livre vraiment : sa cadence par défaut.
///
/// **Ce que ça coûte au bit-perfect.** Rien qui existait. En mode partagé
/// WASAPI le moteur de Windows reçoit `AUTOCONVERTPCM` et convertit lui-même
/// vers la cadence du mélangeur : ouvrir « à la cadence source » ne recadençait
/// pas le DAC, ça déplaçait seulement la conversion chez un convertisseur
/// opaque, non mesuré, et parfois muet. La conversion revient à rubato (sinc,
/// paramètres déjà réglés pour 176,4 → 48 kHz), et surtout elle devient
/// VISIBLE : journal dédié et [`LocalRateStatus`] remonté au client. Le vrai
/// bit-perfect Windows reste le mode exclusif / ASIO, chemins que cette
/// fonction ne touche pas.
pub fn decide_local_rate_opening(
    source_sample_rate: u32,
    device_default_rate: Option<u32>,
    enumeration_accepts_source_rate: bool,
    evidence: SampleRateEvidence,
) -> LocalRateOpening {
    if device_default_rate == Some(source_sample_rate) {
        return LocalRateOpening::DeviceAlreadyAtSourceRate;
    }
    if enumeration_accepts_source_rate && evidence.is_measured() {
        return LocalRateOpening::AtSourceRateMeasured;
    }
    let reason = if enumeration_accepts_source_rate {
        LocalRateFallback::CapabilitiesUnverified
    } else {
        LocalRateFallback::RateNotSupported
    };
    match device_default_rate {
        Some(device_sample_rate) => LocalRateOpening::ResampleToDeviceRate {
            device_sample_rate,
            reason,
        },
        None => LocalRateOpening::LastResortSourceRate,
    }
}

/// Quels chemins de sortie **exclusive** sont réellement COMPILÉS pour une
/// cible donnée.
///
/// Ce n'est pas une opinion : chaque champ correspond à un `#[cfg]` de ce
/// fichier, et à un seul.
///
/// | champ | branche | garde exacte |
/// |---|---|---|
/// | `coreaudio` | `coreaudio_exclusive::ExclusiveOutput` | `#[cfg(target_os = "macos")]` |
/// | `asio` | `asio_exclusive::AsioExclusiveOutput` | `#[cfg(all(target_os = "windows", feature = "asio"))]` |
/// | `wasapi` | `wasapi_exclusive::WasapiExclusiveOutput` | `#[cfg(target_os = "windows")]` — **sans condition de feature** |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExclusiveModeSupport {
    /// macOS : hog mode CoreAudio.
    pub coreaudio: bool,
    /// Windows compilé avec la feature `asio`.
    pub asio: bool,
    /// Windows, quelle que soit la feature `asio`.
    pub wasapi: bool,
}

impl ExclusiveModeSupport {
    /// Aucun chemin exclusif compilé — le cas de Linux.
    const AUCUN: Self = Self {
        coreaudio: false,
        asio: false,
        wasapi: false,
    };

    /// Au moins un chemin exclusif existe sur cette cible.
    pub fn any(self) -> bool {
        self.coreaudio || self.asio || self.wasapi
    }
}

/// Le mode exclusif est-il compilé, pour ce couple (système, feature `asio`) ?
///
/// La plateforme est un **paramètre**, jamais un `cfg!` refermé dans le corps —
/// même raison que [`sample_rate_evidence`] : une décision Windows enfermée
/// dans un `cfg!` n'est pas compilée sur Linux, et le test qui l'interroge y
/// serait vert pour la mauvaise raison (l'angle mort de #1837 et #2056). Un
/// seul appelant, [`LocalOutput::supports_exclusive_mode`], passe la valeur
/// réelle de la machine.
///
/// **#2868** : la règle précédente était
/// `cfg!(macos) || cfg!(all(windows, asio))`. Elle rendait `false` sur un
/// Windows bâti **sans** la feature `asio` — alors que la branche WASAPI
/// exclusive vit sous `#[cfg(target_os = "windows")]` seul et se prend dès que
/// `exclusive_mode && audio_backend != "asio"`. L'utilisateur se voyait donc
/// refuser une capacité que son binaire portait.
///
/// `target_os` est ce que rend `std::env::consts::OS`, c'est-à-dire le nom de
/// cible (`"windows"`, `"macos"`, `"linux"`), en minuscules. Un système inconnu
/// est classé sans mode exclusif : on ne prête pas un chemin à une cible dont
/// on n'a pas écrit la branche.
pub fn exclusive_mode_support(target_os: &str, asio_feature: bool) -> ExclusiveModeSupport {
    match target_os {
        "macos" => ExclusiveModeSupport {
            coreaudio: true,
            ..ExclusiveModeSupport::AUCUN
        },
        // La feature `asio` AJOUTE un chemin ; elle n'en conditionne aucun.
        // WASAPI exclusif est là dans les deux cas.
        "windows" => ExclusiveModeSupport {
            coreaudio: false,
            asio: asio_feature,
            wasapi: true,
        },
        // Linux inclus : `asio_feature` seule ne compile RIEN, sa garde exige
        // `target_os = "windows"` en plus.
        _ => ExclusiveModeSupport::AUCUN,
    }
}

/// Find a cpal StreamConfig that matches the desired channels and sample rate.
///
/// When `supported_output_configs()` fails (PipeWire ALSA compat), falls back
/// to `default_output_config()` and, as a last resort, returns a config with
/// the requested parameters directly — PipeWire will accept and resample.
fn find_matching_config(
    device: &cpal::Device,
    channels: u16,
    sample_rate: u32,
) -> Option<cpal::StreamConfig> {
    // Primary path: enumerate supported configs
    if let Ok(configs) = device.supported_output_configs() {
        let configs_vec: Vec<_> = configs.collect();
        if !configs_vec.is_empty() {
            for config in &configs_vec {
                if config.channels() >= channels
                    && config.min_sample_rate() <= sample_rate
                    && config.max_sample_rate() >= sample_rate
                {
                    return Some(cpal::StreamConfig {
                        channels: channels.min(config.channels()),
                        sample_rate,
                        buffer_size: cpal::BufferSize::Default,
                    });
                }
            }
            // Configs exist but none match the requested rate — let caller
            // handle with its own fallback logic (e.g. try source rate anyway).
            return None;
        }
        // Empty config list — fall through to fallback
    }

    // Fallback for PipeWire / broken ALSA enumeration:
    // Try default_output_config() which often works even when enumeration fails.
    if let Ok(default_cfg) = device.default_output_config() {
        let cfg = default_cfg.config();
        // If the default config's rate matches what we want, use it directly.
        // Otherwise return the default config — the caller will resample.
        if cfg.sample_rate == sample_rate && cfg.channels >= channels {
            return Some(cpal::StreamConfig {
                channels,
                sample_rate,
                buffer_size: cpal::BufferSize::Default,
            });
        }
        // Return default config even if rate differs — better than nothing.
        // Caller will set up resampling.
        return Some(cfg);
    }

    // Last resort: return the requested config directly.  PipeWire's ALSA
    // plugin accepts arbitrary configs and resamples/remixes internally.
    // This will fail on real ALSA without PipeWire, but the caller's
    // build_output_stream error handling covers that case.
    debug!(
        channels,
        sample_rate, "find_matching_config_using_direct_params_pipewire_fallback"
    );
    Some(cpal::StreamConfig {
        channels,
        sample_rate,
        buffer_size: cpal::BufferSize::Default,
    })
}

/// Adapt channel count between source and output through the single matrix in
/// `audio/channels`. Invalid or partial PCM is rejected as silence instead of
/// being partially remixed in the audio path.
fn adapt_channels(samples: &[f32], from_ch: u16, to_ch: u16) -> Vec<f32> {
    crate::audio::channels::adapt_channels_f32(samples, from_ch, to_ch).unwrap_or_else(|error| {
        warn!(from_ch, to_ch, error = %error, "local_channel_adaptation_rejected");
        Vec::new()
    })
}

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
