use super::*;

// ---------------------------------------------------------------------------
// Device enumeration
// ---------------------------------------------------------------------------

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
            crate::outputs::asio_exclusive::ensure_com_initialized();
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
        match crate::outputs::asio_exclusive::try_with_asio_device_lock(enumerate) {
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

pub(super) fn sample_rates_measured_default() -> bool {
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
pub(super) fn hardware_detail_from_description(
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
/// Elle ne change pas le nombre de périphériques publiés : le regroupement par
/// nom reste entier — 43 fantômes → 48 zones chez JeromeQ, Ubuntu 24.04.
///
/// ## ⚠️ Ce qu'une version antérieure de ce commentaire affirmait, et qui est FAUX
///
/// Il disait : « une zone qui a mémorisé `sysdefault:…` continue d'ouvrir
/// `sysdefault:…` ; seules les zones créées ensuite héritent du PCM
/// matériel », et cette phrase a circulé comme une consigne à donner aux
/// testeurs — supprimer la zone et la recréer. **Aucune zone ne mémorise de
/// PCM.** La table `zones` ne porte aucune colonne d'endpoint (`db/sqlite.rs`,
/// `CREATE TABLE zones`) : elle ne retient que `output_device_id =
/// "local:«nom d'affichage»"`. L'identifiant d'endpoint est RECALCULÉ à chaque
/// démarrage et à chaque balayage à chaud, depuis cette liste-ci, par les deux
/// seuls sites de production qui construisent une sortie locale
/// (`tune-server/src/startup.rs` et `tune-server/src/background.rs`, via
/// `with_options_and_endpoint`). Une zone existante hérite donc du `hw:` **au
/// premier redémarrage**, sans qu'on ait à la supprimer ni à la recréer.
///
/// Ce qui restait vrai : la RÉSOLUTION ([`resolve_device`]) travaille sur la
/// liste BRUTE de `output_devices()`, jamais sur cette liste fusionnée. Quand
/// l'endpoint est connu elle apparie dessus et retrouve le `hw:` ; quand il est
/// ABSENT — `recreate_local_and_play` le laisse délibérément vide — elle
/// appariait par NOM, et les dix PCM de la carte portent le même. Elle rendait
/// alors le premier énuméré, un greffon : le plafond de #1655 rentrait par la
/// porte de derrière. C'est ce que [`preferer_le_pcm_materiel`] corrige.
///
/// Quand aucune variante du groupe n'est un `hw:` — le cas d'une machine où
/// PipeWire est le seul chemin praticable — le critère 1 ne départage rien et
/// le comportement d'avant s'applique mot pour mot.
pub(super) fn variante_alsa_candidate_l_emporte(
    retenue: &AlsaVariant,
    candidate: &AlsaVariant,
) -> bool {
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
pub(super) fn merge_linux_duplicate_variant(
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

pub(super) static SCAN_GUARD: std::sync::Mutex<Option<(std::time::Instant, Vec<AudioDevice>)>> =
    std::sync::Mutex::new(None);

pub(super) const SCAN_COOLDOWN_SECS: u64 = 5;

/// Le dernier inventaire PUBLIÉ, lisible sans jamais attendre l'énumération en
/// cours.
///
/// 🔴 #3730 — [`SCAN_GUARD`] est tenu pendant TOUTE la durée de
/// [`list_audio_devices_uncached`], c'est-à-dire pendant l'énumération WASAPI
/// complète : elle sonde les formats de chaque point de sortie, et c'est
/// exactement l'opération que ce fichier documente comme capable de tuer un
/// flux en cours. Tant qu'elle dure, quiconque prend ce même verrou attend.
///
/// [`cached_audio_devices`] le prenait — pour LIRE. Ce n'était pas gênant
/// tant qu'elle n'était appelée que depuis des tâches de fond. Depuis #3322,
/// elle est sur le chemin CHAUD de l'API : `output_capabilities`
/// (`tune-server/src/routes/zones.rs`) l'appelle pour chaque charge utile de
/// zone, donc `GET /zones` **une fois par zone**, `GET /zones/{id}`, la
/// charge utile WebSocket, et la réponse de `POST /zones/{id}/play`. Ce sont
/// des gestionnaires `async` : le verrou est bloquant, il n'y a pas de
/// `spawn_blocking`, et le client web interroge `GET /zones` en boucle. Une
/// énumération lente — le cas ordinaire sur Windows, où le rescan la relance
/// toutes les 120 s dès que rien ne joue — gare donc autant de fils de
/// l'ordonnanceur qu'il y a de requêtes en vol.
///
/// Ce second dépôt rompt le couplage : l'énumérateur y RANGE son résultat
/// (verrou pris le temps d'une affectation), les lecteurs l'y PRENNENT. Aucun
/// lecteur ne peut plus attendre un balayage matériel.
///
/// `Mutex` et non `RwLock` : mêmes bornes que le verrou voisin, et la section
/// critique se réduit à un clone.
pub(super) static DERNIER_PARC: std::sync::Mutex<Vec<AudioDevice>> =
    std::sync::Mutex::new(Vec::new());

/// Ranger l'inventaire fraîchement énuméré, à la vue des lecteurs.
///
/// Appelée par le seul site qui produit un inventaire neuf, juste après
/// l'énumération et AVANT que [`SCAN_GUARD`] ne soit relâché : un lecteur qui
/// arrive entre les deux voit l'ancien parc — jamais un parc vide, jamais un
/// parc à moitié écrit.
pub(super) fn publier_le_parc(parc: &[AudioDevice]) {
    *DERNIER_PARC.lock().unwrap_or_else(|e| e.into_inner()) = parc.to_vec();
}

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
pub(super) fn asio_device_busy() -> bool {
    #[cfg(all(target_os = "windows", feature = "asio"))]
    {
        crate::outputs::asio_exclusive::asio_device_is_busy()
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
    // Publier AVANT de relâcher `SCAN_GUARD` : le parc devient lisible sans
    // attendre, et les lecteurs n'ont jamais à prendre le verrou d'énumération
    // (#3730).
    publier_le_parc(&result);
    *guard = Some((std::time::Instant::now(), result.clone()));
    result
}

/// Return the last cached device list WITHOUT triggering a fresh enumeration.
///
/// Enumerating WASAPI devices probes each device's supported formats, which can
/// invalidate an active render stream and kill playback on Windows (DEvir). So
/// while a local stream is playing we serve this cache instead of re-scanning.
/// Returns an empty list if nothing has been enumerated yet this session.
///
/// 🔴 #3730 — lit [`DERNIER_PARC`] et NON [`SCAN_GUARD`]. Le second est tenu
/// pendant toute l'énumération : le prendre pour lire faisait attendre
/// l'appelant aussi longtemps que le balayage matériel. Depuis #3322 cette
/// fonction est sur le chemin chaud de l'API — voir [`DERNIER_PARC`] pour la
/// liste des routes concernées et le mécanisme complet.
pub fn cached_audio_devices() -> Vec<AudioDevice> {
    DERNIER_PARC
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

pub(super) fn list_audio_devices_uncached(backend: &str) -> Vec<AudioDevice> {
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
pub(super) fn log_no_devices_diagnostics(host_name: &str) {
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
