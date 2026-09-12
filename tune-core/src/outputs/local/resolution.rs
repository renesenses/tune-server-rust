use super::*;

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
    /// Retrouvé par NOM, puis ramené au PCM matériel de la même carte (#1655).
    ///
    /// `greffon` est le rang qu'un appariement par nom aurait rendu : un
    /// `dmix:`/`sysdefault:`/`plughw:`, c'est-à-dire un convertisseur
    /// logiciel. `retenu` est le `hw:` du même nom. Les deux rangs voyagent
    /// ensemble pour que la décision puisse être JOURNALISÉE au lieu d'être
    /// prise en silence.
    ByAlsaHardwarePcm { retenu: usize, greffon: usize },
}

impl DeviceMatch {
    pub(crate) fn index(self) -> usize {
        match self {
            Self::ByEndpointId(i) | Self::ByDisplayName(i) | Self::BySubstring(i) => i,
            Self::ByAlsaHardwarePcm { retenu, .. } => retenu,
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
            // 2 bis. Le nom ne distingue pas le PCM. Sur ALSA il en désigne une
            //        dizaine pour la même carte, et le premier énuméré est le
            //        plus souvent un greffon : voir `preferer_le_pcm_materiel`.
            return match preferer_le_pcm_materiel(&matchable, index) {
                Some(retenu) => DeviceResolution::Matched(DeviceMatch::ByAlsaHardwarePcm {
                    retenu,
                    greffon: index,
                }),
                None => DeviceResolution::Matched(DeviceMatch::ByDisplayName(index)),
            };
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

/// Ramener un appariement PAR NOM au PCM matériel de la même carte (#1655).
///
/// ## Pourquoi la résolution devait rattraper la découverte
///
/// La découverte regroupe les variantes ALSA homonymes et retient le `hw:`
/// ([`variante_alsa_candidate_l_emporte`], #3240). La RÉSOLUTION, elle,
/// travaille sur la liste BRUTE rendue par `host.output_devices()` : les dix
/// PCM de la carte y sont tous présents, et ils portent tous **le même nom**.
/// L'étape 2 rendait donc le PREMIER énuméré — `default`, `sysdefault:`,
/// `dmix:` — c'est-à-dire un greffon qui accepte tout et rééchantillonne.
/// `dmix` fixe la cadence de son esclave (`defaults.pcm.dmix.rate 48000`) :
/// c'est exactement le plafond à 48 kHz de GgB sur l'Eversolo DAC-Z8 (#1655),
/// remis en place par la résolution après que la découverte l'a écarté.
///
/// ## Quand ce chemin est réellement emprunté
///
/// L'étape 1 (identifiant d'endpoint) passe AVANT et reste souveraine : une
/// zone dont la sortie a été enregistrée depuis l'énumération fusionnée porte
/// déjà le `hw:`, et cette fonction ne change rien pour elle. Elle ne mord que
/// sur les appariements où l'endpoint est ABSENT — au premier rang
/// `recreate_local_and_play`, qui laisse délibérément `endpoint_id = None`
/// parce que le périphérique n'est pas énumérable au moment où il reconstruit
/// la sortie.
///
/// ## Ce qu'elle ne fait pas
///
/// - Elle ne sort jamais du groupe homonyme : seules les candidates portant le
///   **même `raw_name`** que celle retenue par le nom sont examinées. Deux DAC
///   distincts que Windows nomme tous deux « Haut-Parleurs » gardent donc leur
///   départage par rang `(n)`, intact.
/// - Elle ne s'applique qu'à ALSA *de fait* : [`alsa_pcm_is_direct_hardware`]
///   rend `false` pour tout identifiant WASAPI, ASIO ou CoreAudio, si bien
///   qu'aucune candidate n'y est « matérielle » et que la fonction rend `None`
///   sans rien changer.
/// - Elle ne re-décide pas des CAPACITÉS : la résolution ne les connaît pas.
///   Seul le critère 1 de l'ordre total de la découverte est rejoué.
///
/// Le départage entre plusieurs `hw:` homonymes prend le plus petit
/// identifiant — le même dernier cran que
/// [`variante_alsa_candidate_l_emporte`], pour que le vainqueur ne dépende pas
/// de l'ordre d'énumération d'alsa-lib.
///
/// Rend `None` quand il n'y a rien à corriger : candidate déjà matérielle, ou
/// aucune candidate matérielle dans le groupe homonyme.
pub(super) fn preferer_le_pcm_materiel(
    matchable: &[(usize, &DeviceIdentity)],
    retenu_par_le_nom: usize,
) -> Option<usize> {
    let choisie = matchable
        .iter()
        .find(|(index, _)| *index == retenu_par_le_nom)
        .map(|(_, candidate)| *candidate)?;
    if alsa_pcm_is_direct_hardware(&choisie.endpoint_id) {
        return None;
    }
    matchable
        .iter()
        .filter(|(_, candidate)| candidate.raw_name == choisie.raw_name)
        .filter(|(_, candidate)| alsa_pcm_is_direct_hardware(&candidate.endpoint_id))
        .min_by(|(_, a), (_, b)| a.endpoint_id.cmp(&b.endpoint_id))
        .map(|(index, _)| *index)
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
pub(super) fn disambiguate_display_name(
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
pub(super) fn is_null_sink(raw_name: &str) -> bool {
    raw_name.contains("Discard all samples") || raw_name.contains("Dummy")
}

/// Le nom demandé porte-t-il un suffixe de rang `(n)` posé par la découverte ?
pub(super) fn looks_disambiguated(requested: &str) -> bool {
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
pub(super) fn find_device_with_fallback(
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
        if let DeviceMatch::ByAlsaHardwarePcm { greffon, .. } = matched {
            // Une décision qui change ce qui sera OUVERT ne passe jamais en
            // silence (#3209, #1655). Même famille de marqueur que la
            // découverte, suffixée pour dire LEQUEL des deux chemins a
            // corrigé.
            info!(
                requested = %device_name,
                greffon_ecarte = %identities[greffon].endpoint_id,
                endpoint_retenu = %identities[index].endpoint_id,
                "local_audio_alsa_hardware_pcm_preferred_at_resolve"
            );
        }
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
pub(super) fn probe_device_fallback_caps(
    device: &cpal::Device,
    name: &str,
) -> (u16, Vec<u32>, bool) {
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
pub(super) fn alsa_pcm_name(endpoint_id: &str) -> &str {
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
pub(super) fn find_matching_config(
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
pub(super) fn adapt_channels(samples: &[f32], from_ch: u16, to_ch: u16) -> Vec<f32> {
    crate::audio::channels::adapt_channels_f32(samples, from_ch, to_ch).unwrap_or_else(|error| {
        warn!(from_ch, to_ch, error = %error, "local_channel_adaptation_rejected");
        Vec::new()
    })
}
