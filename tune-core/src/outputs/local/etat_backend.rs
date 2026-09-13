use super::*;

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
        crate::outputs::asio_exclusive::ensure_com_initialized();
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
pub(super) static OBSERVED_BACKEND: std::sync::RwLock<Option<ObservedBackend>> =
    std::sync::RwLock::new(None);

/// Ce que le dernier `select_host` a réellement ouvert, et pourquoi il n'a pas
/// pu honorer la demande quand c'est le cas.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ObservedBackend {
    pub(super) name: &'static str,
    pub(super) fallback_reason: Option<LocalBackendFallback>,
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
pub(super) static OBSERVED_DEVICE: std::sync::RwLock<Option<ObservedDevice>> =
    std::sync::RwLock::new(None);

/// Ce que la dernière ouverture de périphérique a demandé, et ce qu'elle a eu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ObservedDevice {
    pub(super) backend: &'static str,
    pub(super) requested: String,
    /// Vide quand rien n'a été ouvert — voir [`LocalDeviceStatus::opened`].
    pub(super) opened: String,
    pub(super) opened_id: Option<String>,
    pub(super) reason: Option<LocalDeviceFallback>,
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
pub(super) static OBSERVED_RATE: std::sync::RwLock<Option<ObservedRate>> =
    std::sync::RwLock::new(None);

/// Ce que la dernière ouverture partagée a demandé comme cadence, et ce qu'elle
/// a ouvert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ObservedRate {
    pub(super) source_sample_rate: u32,
    pub(super) opened_sample_rate: u32,
    pub(super) reason: Option<LocalRateFallback>,
    pub(super) evidence_measured: bool,
}

/// Nom du backend tel que `select_host` l'a observé, ou `"local"` faute d'avoir
/// encore ouvert quoi que ce soit. Sert à étiqueter l'ouverture d'un
/// périphérique par le chemin cpal, qui ne connaît que la variante cpal.
pub(super) fn observed_backend_name() -> &'static str {
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
pub(super) fn note_opened_device(
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
pub(super) fn note_device_outcome(
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
    pub(super) fn from_observed(observed: ObservedDevice) -> Self {
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
    pub(super) fn from_observed(observed: ObservedRate) -> Self {
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
pub(super) fn note_rate_decision(observed: ObservedRate) {
    if let Ok(mut slot) = OBSERVED_RATE.write() {
        *slot = Some(observed);
    }
}

/// Enregistre le backend réellement ouvert, et le motif du repli s'il y en a un.
/// Appelé par `select_host` seul, sur **toutes** les cibles.
pub(super) fn note_observed_backend(
    name: &'static str,
    fallback_reason: Option<LocalBackendFallback>,
) {
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
pub(super) fn asio_outcome(
    asio_devices: Option<usize>,
) -> (&'static str, Option<LocalBackendFallback>) {
    match asio_devices {
        Some(n) if n > 0 => ("ASIO", None),
        Some(_) => ("WASAPI", Some(LocalBackendFallback::AsioNoDevices)),
        None => ("WASAPI", Some(LocalBackendFallback::AsioHostUnavailable)),
    }
}

/// Issue d'une demande sur une cible qui n'embarque **pas** ASIO.
#[cfg_attr(all(target_os = "windows", feature = "asio"), allow(dead_code))]
pub(super) fn unsupported_outcome(
    requested_lower: &str,
) -> (&'static str, Option<LocalBackendFallback>) {
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
pub(super) fn platform_default_backend_name() -> &'static str {
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
pub(super) fn backend_display_name(observed: Option<&'static str>, backend: &str) -> &'static str {
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
pub(super) fn backend_status(
    observed: Option<ObservedBackend>,
    observed_device: Option<ObservedDevice>,
    requested: &str,
) -> LocalBackendStatus {
    backend_status_with_rate(observed, observed_device, None, requested)
}

/// Même isolement pour le statut complet : aucune lecture de l'état global,
/// aucun périphérique ouvert, donc jouable sur n'importe quelle plateforme.
pub(super) fn backend_status_with_rate(
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
