use super::*;

pub fn build_signal_path_pub(
    ps: &ZoneState,
    zone: &Zone,
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    renderer_label: Option<&str>,
    audio_backend: &str,
    wire: Option<&StreamInfo>,
) -> Option<Value> {
    build_signal_path(ps, zone, backend, renderer_label, audio_backend, wire)
}

/// #1395 — sur la ZONE, dire quel backend de sortie locale tourne vraiment,
/// lequel était demandé, et pourquoi ils diffèrent.
///
/// Le chemin du signal nomme déjà le backend ACTIF dans son étape Transport
/// (« ASIO (exclusive) », « WASAPI »…), et il dit vrai depuis #1414. Ce qui
/// manquait, c'est le terme de comparaison : Bilou règle « Ce PC / Hauts
/// Parleurs » sur ASIO, lit « WASAPI », et ne peut pas savoir si son réglage
/// n'a pas pris ou si le serveur a basculé. Le motif du basculement existait
/// — `local_audio_asio_no_devices` — mais seulement dans le journal ; il a
/// fallu qu'il en poste une capture pour que le fil avance.
///
/// `None` pour toute zone qui n'est pas une sortie locale : un renderer DLNA
/// ou Chromecast n'a rien à voir avec ASIO, et lui accrocher un motif de repli
/// serait exactement l'annonce fantôme que #2053 et #1315 ont déjà coûtée.
/// `None` aussi quand la sortie locale n'est pas compilée.
#[cfg(feature = "local-audio")]
pub fn local_backend_status_value(output_type: Option<&str>, requested: &str) -> Option<Value> {
    // Même convention que `build_signal_path` : une zone sans `output_type`
    // est une sortie locale.
    if output_type.unwrap_or("local") != "local" {
        return None;
    }
    serde_json::to_value(tune_core::outputs::local::active_backend_status(requested)).ok()
}

/// Variante sans sortie locale compilée : il n'y a aucun backend à décrire.
#[cfg(not(feature = "local-audio"))]
pub fn local_backend_status_value(_output_type: Option<&str>, _requested: &str) -> Option<Value> {
    None
}

/// Build the `signal_path` object for a zone's current playback.
/// Returns `None` when the zone is not playing.
///
/// `audio_backend` is the active audio backend name ("ASIO", "WASAPI",
/// "CoreAudio", "ALSA") used for local zones' signal path display.
///
/// `wire` décrit ce qui part RÉELLEMENT sur le fil pour la session en cours
/// (`AudioStreamer::stream_output_wire`) : conteneur, fréquence, profondeur.
/// `None` quand il n'y a pas de session vivante (sortie locale, avant démarrage).
///
/// C'est la source de vérité, et elle prime sur toute déduction. Cette fonction
/// rejouait les règles de l'orchestrateur pour deviner ce qui était servi ; à
/// chaque évolution du chemin audio il fallait répliquer la règle ici, et un
/// oubli faisait mentir l'affichage. Le renderer, lui, affiche ce qu'il reçoit
/// — d'où les écarts constatés par Yves sur darTZeel LHC-208 et Eversolo
/// DMP-A10, tous deux en passthrough natif.
/// Is a WAV/LPCM wire feed to a DLNA/OpenHome renderer bit-perfect?
///
/// Three cases share the WAV wire: a native WAV source (passthrough), the
/// zone forcing 16-bit LPCM (`dlna_lpcm`), or a FLAC/ALAC source that the
/// orchestrator fell back to WAV for. A **native WAV** source is sent
/// byte-for-byte at any bit depth, so it is always bit-perfect. The FLAC/ALAC→WAV
/// fallback is plain 16-bit LPCM unless `dlna_wav24` preserves the full 24 bits,
/// so it is bit-perfect only when the source already fits 16 bits or the 24-bit
/// override is on.
/// L'égaliseur de cette zone modifie-t-il réellement le signal ?
///
/// Miroir exact de `Orchestrator::load_eq_processor` : mode PURE d'abord — il
/// court-circuite tout traitement, donc un profil enregistré n'y change rien —
/// puis profil activé ET gains audibles. Sans ce miroir, l'indicateur
/// bit-perfect et le chemin audio répondraient à deux questions différentes.
pub(super) fn active_zone_eq_profile(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    zone_id: i64,
) -> Option<tune_core::audio::eq::EqProfile> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());
    // PURE : le PCM atteint la sortie intact, l'égaliseur n'est jamais construit.
    if tune_core::audio::audiophile::zone_enabled(backend, zone_id) {
        return None;
    }
    let profile = settings
        .get(&format!("zone_{zone_id}_eq_profile"))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<tune_core::audio::eq::EqProfile>(&s).ok())?;
    if !profile.enabled {
        return None;
    }
    // 44100/2 n'est qu'une sonde : is_enabled() dépend des gains, pas du débit.
    tune_core::audio::eq::EqProcessor::new(&profile, 44100, 2)
        .is_enabled()
        .then_some(profile)
}

pub(super) fn zone_eq_alters_signal(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    zone_id: i64,
) -> bool {
    active_zone_eq_profile(backend, zone_id).is_some()
}

/// Description du traitement EQ réellement configuré, y compris le headroom
/// automatique. Le limiteur est nommé comme absent : le pré-gain réserve la
/// marge des boosts, il ne faut plus confondre l'EQ avec une protection de crête.
pub(super) fn zone_eq_step_description(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    zone_id: i64,
) -> Option<String> {
    let profile = active_zone_eq_profile(backend, zone_id)?;
    let left = profile.automatic_headroom_db(0);
    let right = profile.automatic_headroom_db(1);
    if (left - right).abs() < 0.01 {
        Some(format!(
            "EQ actif (pré-gain auto {left:.1} dB, sans limiteur)"
        ))
    } else {
        Some(format!(
            "EQ actif (pré-gain auto G {left:.1} dB / D {right:.1} dB, sans limiteur)"
        ))
    }
}

/// Le ReplayGain modifie-t-il réellement le signal de cette zone — et comment ?
///
/// Miroir de `Orchestrator::zone_replaygain_changes_audio`, pour la même
/// raison que `zone_eq_alters_signal` : sans lui, le panneau annoncerait
/// « Bit-Perfect » pendant qu'un gain multiplie chaque échantillon (même
/// famille d'écart que l'EQ ignoré du verdict, #1548/#1559 — ici #1627).
/// Mode PURE d'abord : le gain n'y est jamais appliqué, donc jamais d'étape.
/// Ensuite le facteur EFFECTIF (tags + pré-ampli + anti-écrêtage) : un mode
/// « track » sans tag stocké ne change rien au signal et n'affiche donc rien.
///
/// Retourne la description de l'étape (« ReplayGain (track, -4.2 dB, tags du
/// fichier) ») quand le gain s'applique, `None` sinon. La granularité affichée
/// est celle qui a FOURNI la valeur : en mode album sans tags d'album, c'est le
/// gain de piste qui joue, et c'est lui qu'on nomme.
///
/// La PROVENANCE est le reste de #1627 : le panneau disait ce qui s'applique et
/// de combien, jamais d'où ça vient. « Tune utilise-t-il mes tags rsgain ? »
/// (#1382) se répondait alors partout sauf à l'endroit où la question se pose.
pub(super) fn zone_replaygain_step(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    zone_id: i64,
    track_id: Option<i64>,
) -> Option<ReplayGainStep> {
    use tune_core::audio::replaygain::{
        GainSource, ReplayGainSettings, gain_factor, stored_gain_detail, stored_gain_source,
    };
    // PURE : le PCM atteint la sortie intact, le gain n'est jamais appliqué.
    if tune_core::audio::audiophile::zone_enabled(backend, zone_id) {
        return None;
    }
    let tid = track_id?;
    let settings = ReplayGainSettings::load(backend);
    let (gain, source) = stored_gain_detail(backend, tid, settings.mode)?;
    let factor = gain_factor(gain, settings);
    // Même seuil que l'orchestrateur (`zone_replaygain_changes_audio`).
    if (factor - 1.0).abs() <= 1e-6 {
        return None;
    }
    // Le dB affiché est celui qui multiplie réellement les échantillons
    // (pré-ampli et anti-écrêtage compris), pas le tag brut.
    let applied_db = 20.0 * factor.log10();
    let label = match source {
        tune_core::audio::replaygain::ReplayGainMode::Album => "album",
        _ => "track",
    };
    // La provenance porte sur la granularité qui a fourni la valeur, pas sur
    // le mode demandé. Une base illisible ne doit rien inventer : on retombe
    // sur la description d'avant, sans mention d'origine.
    let origin = stored_gain_source(backend, tid, source);
    let description = match origin {
        Some(src) => format!(
            "ReplayGain ({label}, {applied_db:+.1} dB, {})",
            src.label_fr()
        ),
        None => format!("ReplayGain ({label}, {applied_db:+.1} dB)"),
    };
    Some(ReplayGainStep {
        description,
        granularity: label,
        source: origin.map(GainSource::as_str),
    })
}

/// L'étape ReplayGain du chemin du signal, description ET faits bruts.
///
/// Les deux champs structurés sont ADDITIFS : le client qui ne lit que
/// `description` continue de fonctionner à l'identique.
pub(super) struct ReplayGainStep {
    description: String,
    /// `"track"` ou `"album"` — celle qui a fourni la valeur.
    granularity: &'static str,
    /// `"file_tags"` ou `"analysis"`, absent si la base n'a pas répondu.
    source: Option<&'static str>,
}

/// La zone replie-t-elle sa sortie LOCALE en mono — et si oui, que dire ?
///
/// Miroir exact de ce que l'orchestrateur pousse à la sortie locale
/// (`PlaybackOrchestrator::zone_mono_downmix_with`, PURE compris), et restreint aux
/// sorties locales : c'est le seul chemin où le repli est appliqué, et une
/// étape affichée sur une zone DLNA décrirait un traitement qui n'a pas lieu.
///
/// Sans ce miroir, le panneau annoncerait un chemin intouché pendant que chaque
/// échantillon est réécrit — la faute exacte de #1548/#1559 (égaliseur oublié
/// du verdict) et de #1627 (ReplayGain). Ici la transformation est réelle et
/// doit APPARAÎTRE : #2825 vient de corriger le cas inverse, où le volume
/// logiciel prétendait à tort dégrader.
pub(super) fn zone_mono_downmix_step(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    zone_id: i64,
    output_type: &str,
) -> Option<String> {
    if output_type != "local" {
        return None;
    }
    tune_core::orchestrator::PlaybackOrchestrator::zone_mono_downmix_with(backend, zone_id)
        .then(|| "Sortie mono : (G + D) / 2 sur les deux voies".to_string())
}

/// La famille DSD que désigne une cadence brute (2,8 MHz → DSD64, etc.).
///
/// Une seule table pour la ligne Source et pour l'étage de sortie : elles
/// nommaient la même chose à deux endroits, et rien ne garantissait qu'elles
/// disent la même chose.
pub(super) fn dsd_family_name(sample_rate: i32) -> &'static str {
    match sample_rate {
        r if r >= 22_000_000 => "DSD512",
        r if r >= 11_000_000 => "DSD256",
        r if r >= 5_000_000 => "DSD128",
        _ => "DSD64",
    }
}

/// « DSD128 5.6 MHz » — un DSD se dit par sa famille et sa cadence en MHz,
/// jamais en kHz/bit.
pub(super) fn dsd_resolution_label(sample_rate: i32) -> String {
    format!(
        "{name} {mhz:.1} MHz",
        name = dsd_family_name(sample_rate),
        mhz = sample_rate as f64 / 1_000_000.0
    )
}

/// Ces chiffres décrivent-ils du DSD ? 1 bit, ou une cadence en MHz.
///
/// Aucun PCM n'atteint le mégahertz (768 kHz est le maximum du marché) et
/// aucun conteneur PCM ne porte 1 bit : les deux tests sont sans recouvrement
/// possible avec du PCM légitime.
pub(super) fn is_dsd_resolution(sample_rate: i32, bit_depth: i32) -> bool {
    // `== 1` et non `<= 1` : une profondeur de 0 est une valeur MANQUANTE, pas
    // du DSD, et la traiter comme telle inventerait un « DSD64 0.0 MHz ».
    bit_depth == 1 || sample_rate >= 1_000_000
}

/// Libellé d'un étage de SORTIE — le garde-fou structurel du #1315.
///
/// « FLAC 5644kHz/1bit » est un libellé IMPOSSIBLE : aucun FLAC ne transporte
/// du 1 bit à 5,6 MHz. Il s'affichait pourtant sur l'Eversolo DMP-A6 de
/// Stéphane Villerio, parce que les deux moitiés de la ligne n'ont pas la même
/// origine — le nom du conteneur est DEVINÉ (`dlna_transcode_target`, une
/// cible statique), les chiffres viennent du FIL, c'est-à-dire de ce qui part
/// vraiment.
///
/// Quand les deux se contredisent, ce sont les chiffres qui gagnent : c'est
/// déjà la règle du reste de `build_signal_path` (« le fil prime »), et c'est
/// la seule moitié qui soit une mesure. Le libellé impossible ne peut donc
/// plus sortir d'ici, quelle que soit la cible qu'on lui passe.
pub(super) fn output_stage_label(container: &str, sample_rate: i32, bit_depth: i32) -> String {
    if is_dsd_resolution(sample_rate, bit_depth) {
        if !container.starts_with("DSD") {
            tracing::warn!(
                container,
                sample_rate,
                bit_depth,
                "signal_path_libelle_impossible_ecarte — une résolution DSD \
                 annoncée sous un conteneur PCM ; le fil tranche (#1315)"
            );
        }
        return dsd_resolution_label(sample_rate);
    }
    if sample_rate >= 1000 {
        format!(
            "{container} {sr}kHz/{bit_depth}bit",
            sr = sample_rate / 1000
        )
    } else {
        format!("{container} {sample_rate}Hz/{bit_depth}bit")
    }
}

/// Le fil porte-t-il du DSD BRUT — le fichier .dsf/.dff tel quel ?
///
/// Miroir du `dsd_passthrough` de l'orchestrateur, et il manquait. `zones.rs`
/// mire l'ALAC et l'AAC ; le commentaire du miroir ALAC dit lui-même qu'il a
/// été ajouté pour tuer une étape fantôme « ALAC→FLAC » (#1131). Le même
/// fantôme existait pour le DSD : `needs_transcode_for_output` restait vrai et
/// le panneau annonçait une conversion vers FLAC pendant que l'orchestrateur
/// envoyait le .dsf brut (#1315, Yves Corbat / Stéphane Villerio, DMP-A6).
///
/// La décision elle-même (`should_dsd_passthrough`) dépend d'un sondage SOAP
/// asynchrone que ce constructeur synchrone ne peut pas rejouer — et la
/// rejouer serait un septième miroir à maintenir. On lit donc ce que la
/// session sert VRAIMENT : le passthrough crée sa session avec l'extension
/// source et un MIME DSD (`orchestrator.rs`, branche « Standard passthrough:
/// serve the raw file »), là où toutes les autres branches produisent du
/// `wav`/`flac`. C'est le même principe que `wire_wav`, et il est plus fort
/// qu'un miroir : il constate au lieu de deviner.
pub(super) fn wire_carries_raw_dsd(wire: Option<&StreamInfo>) -> bool {
    wire.is_some_and(|w| {
        tune_core::orchestrator::est_source_dsd(Some(&w.format))
            || tune_core::orchestrator::est_dsd_brut(&w.mime_type)
    })
}

pub(super) fn wav_wire_bit_perfect(
    is_lossless: bool,
    source_is_wav: bool,
    dlna_wav24: bool,
    bit_depth: i32,
) -> bool {
    is_lossless && (source_is_wav || dlna_wav24 || bit_depth <= 16)
}

/// Le fil est-il intact, du point de vue du VERDICT affiché ?
///
/// La sonde Windows (#2205/#2233) est autoritaire sur ce qui a atteint le ring,
/// et son `bit_perfect` vaut `reasons.is_empty()` : le volume logiciel y figure
/// au même rang que le DSP ou le transport flottant. Or `build_signal_path`
/// applique depuis #1627 la règle inverse — « Volume is excluded, it's a user
/// preference, not a signal degradation » — et l'applique encore, deux cents
/// lignes plus bas, à toutes les autres sorties et à toutes les autres
/// plateformes (macOS, Linux et le navigateur ne publient aucune sonde, donc
/// `unwrap_or(true)`).
///
/// Conséquence vécue (#2053) : sous Windows, descendre le curseur à 85 % sur un
/// FLAC sans égaliseur, sans ReplayGain et sans plafond de fréquence suffisait à
/// faire tomber le verdict — et le client n'a qu'un seul mot pour dire « pas
/// bit-perfect » : **« Transcodé »**. Un testeur qui n'a touché que son volume
/// lisait donc qu'on transcodait sa musique.
///
/// On ne relève JAMAIS le verdict du producteur en promesse de pureté : seul le
/// cas où le volume est la SEULE cause est neutralisé. Une raison de plus (DSP,
/// transport flottant, état indéterminé) et le verdict reste négatif. La cause
/// n'est pas effacée pour autant : elle reste dans `runtime_reasons` et dans le
/// détail de l'étape Transport.
pub(super) fn runtime_transport_is_intact(status: &OutputSignalPathStatus) -> bool {
    status.bit_perfect
        || (!status.reasons.is_empty()
            && status
                .reasons
                .iter()
                .all(|reason| matches!(reason, OutputSignalReason::SoftwareVolume)))
}

pub(super) fn runtime_signal_reason_detail(status: &OutputSignalPathStatus) -> Option<String> {
    let details: Vec<&str> = status
        .reasons
        .iter()
        .map(|reason| match reason {
            OutputSignalReason::FloatTransport => "Transport flottant imposé par le callback",
            OutputSignalReason::DspApplied => "DSP appliqué",
            OutputSignalReason::DspStateUnknown => "État DSP indéterminé",
            OutputSignalReason::SoftwareVolume => "Volume logiciel appliqué",
        })
        .collect();
    (!details.is_empty()).then(|| details.join(" ; "))
}

/// Le nom PRÉSENTABLE d'un transport que le `match` de `build_signal_path` ne
/// nomme pas par un bras.
///
/// Le bras par défaut rendait le second membre du tuple — la chaîne BRUTE de
/// la colonne `zones.output_type` — comme nom de transport. Le panneau d'Alex
/// Campbell affichait donc « hqplayer » en minuscules là où toutes les autres
/// sorties affichent « DLNA/UPnP », « BluOS » ou « CoreAudio » (#2189).
///
/// Les types INCONNUS gardent leur chaîne : un greffon hors dépôt enregistre
/// le nom qu'il veut, et aucune règle de mise en forme ne saurait deviner sa
/// capitalisation. Inventer un libellé serait pire que de rendre le sien.
pub(super) fn libelle_de_transport(output_type: &str) -> &str {
    match output_type {
        "hqplayer" => "HQPlayer",
        "diretta" => "Diretta",
        autre => autre,
    }
}

pub(super) fn build_signal_path(
    ps: &ZoneState,
    zone: &Zone,
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    renderer_label: Option<&str>,
    audio_backend: &str,
    wire: Option<&StreamInfo>,
) -> Option<Value> {
    if ps.state == PlayState::Stopped {
        return None;
    }

    let np = ps.now_playing.as_ref()?;

    let Source {
        output_container,
        wire_sample_rate,
        wire_bit_depth,
        source_format,
        is_dsd,
        sample_rate,
        bit_depth,
        format_name,
        is_lossless,
    } = decrire_la_source(np, backend, wire);

    let output_type = zone.output_type.as_deref().unwrap_or("local");
    // Pour une sortie locale qui sait observer son dernier callback, le réel
    // prime sur toute déduction depuis les réglages. Les autres sorties
    // conservent le calcul historique jusqu'à ce qu'elles publient leur propre
    // sonde via le contrat additif d'OutputTarget.
    let runtime_signal_path = (output_type == "local")
        .then(|| ps.output_signal_path.as_ref())
        .flatten();

    // Determine if DSP is active.
    //
    // Deux sources, et il faut les DEUX : la colonne dsp_preset_id/dsp_enabled
    // de la zone, et le profil d'égaliseur `zone_{id}_eq_profile`. C'est ce
    // dernier qu'écrit le panneau EQ de « Lecture en cours » et que lit le
    // chemin audio (`Orchestrator::zone_has_active_eq`) — l'indicateur ne le
    // consultait pas.
    //
    // Conséquence : Tune pouvait afficher « Bit-Perfect » alors qu'un
    // égaliseur modifiait réellement le signal. Pour un logiciel dont c'est
    // l'argument central, promettre une pureté qu'on ne tient pas est le pire
    // des deux sens possibles de l'erreur (signalement Bilou).
    let zid = zone.id.unwrap_or(0);
    let configured_dsp_enabled = ZoneRepo::with_backend(backend.clone())
        .get_dsp_config(zid)
        .map(|(preset_id, enabled)| enabled && preset_id.is_some())
        .unwrap_or(false)
        || zone_eq_alters_signal(&backend, zid);
    let dsp_enabled = runtime_signal_path
        .map(|status| status.dsp == OutputDspState::Applied)
        .unwrap_or(configured_dsp_enabled);
    let eq_step_description = zone_eq_step_description(&backend, zid);

    // ReplayGain effectivement appliqué à la piste en cours (#1627) : même
    // traitement que l'EQ — une étape dans le chemin, et le verdict bit-perfect
    // en tient compte. `None` en PURE, en mode off, ou sans gain stocké.
    let replaygain_step = zone_replaygain_step(&backend, zid, np.track_id);

    // Sortie mono (#2362) : sortie locale seulement, jamais en PURE. C'est une
    // vraie transformation — elle réécrit chaque échantillon — donc elle porte
    // une étape et fait tomber le verdict bit-perfect, comme le ReplayGain.
    let mono_downmix_step = zone_mono_downmix_step(&backend, zid, output_type);

    // Volume at 100% means no software volume adjustment.
    // Fixed-volume zones always output at full volume (bit-perfect).
    //
    // La valeur affichée est `zone.volume` (la base), PAS `ps.volume` : c'est
    // elle que la page expose comme curseur (GET /zones/{id}). `ps.volume` est
    // une copie mémoire qui ment dans deux cas : jamais initialisée depuis la
    // base au démarrage (0,5 par défaut pour une zone locale/navigateur —
    // seules les zones réseau sont resemées à la découverte), et modifiée par
    // les régleurs internes (alarmes, minuterie de sommeil, IA) qui n'écrivaient
    // pas la base. Résultat : le panneau bit-perfect affichait « Volume 20 % »
    // face à un curseur ailleurs, jusqu'à ce qu'on touche le volume — le PUT
    // réécrit alors les deux sources (#1504 Jean Valjean, même symptôme
    // Bebelalu55 #1480). Une seule source pour les deux affichages.
    let ui_volume = (zone.volume / 100.0).clamp(0.0, 1.0);
    let volume_full = zone.fixed_volume || ui_volume >= 1.0 || ui_volume <= 0.0; // 0.0 means no software vol set

    // Transcode exotic formats (AIFF, DSD, WavPack, APE, ALAC) for network outputs.
    // FLAC, WAV, MP3, AAC are natively supported and pass through without transcoding.
    //
    // ⚠️ Cette liste ÉTAIT recopiée ici. `orchestrator.rs` porte pourtant, en
    // toutes lettres, « l'unique exemplaire de cette liste » — et cette
    // quatrième copie avait déjà dérivé : cinq types au lieu de six,
    // `slimproto` manquant. Une zone Slimproto était donc « réseau » pour le
    // chemin audio (qui lui applique les forçages WAV/LPCM et le plafond
    // 16 bits) et « inconnue » pour le panneau, qui la déclarait non
    // bit-perfect sans jamais lire ces réglages. Le miroir suit désormais la
    // décision, par la MÊME fonction (#2189, même faute que #3183).
    let is_network_output = tune_core::orchestrator::is_network_output_type(Some(output_type));
    // Passthrough DSD natif : l'orchestrateur sert le .dsf/.dff brut au
    // renderer (`orchestrator.rs` `dsd_passthrough`). Constaté sur le fil, pas
    // deviné — cf. `wire_carries_raw_dsd`. Sans ce miroir, une piste DSD128
    // envoyée telle quelle à un Eversolo DMP-A6 s'affichait « DSD128 5.6 MHz →
    // FLAC 5644kHz/1bit » : un transcodage qui n'a pas lieu, vers un conteneur
    // qui ne peut pas exister (#1315).
    let dsd_passthrough = is_dsd && is_network_output && wire_carries_raw_dsd(wire);
    // Un égaliseur ARMÉ n'atteint pas un flux DSD servi BRUT hors sortie
    // locale, et l'orchestrateur s'en abstient DÉLIBÉRÉMENT : convertir du DSD
    // natif en PCM pour y passer un EQ serait une dégradation décidée à la
    // place de l'auditeur. Les deux gardes sont explicites côté audio —
    // `pull_output_needs_dsp_transcode` rend `false` sur `AudioFormat::Dsd`,
    // et `eq_forces_transcode` est gardé par `!dsd_passthrough`.
    //
    // Le panneau, lui, annonçait l'étape DSP sur la seule foi du RÉGLAGE en
    // base (`configured_dsp_enabled`) : un traitement qui n'a pas lieu, plus
    // un verdict bit-perfect qu'il faisait tomber alors que le fil est intact.
    // C'est la faute de #1315 et #2053 — ne pas annoncer ce qui n'a pas lieu —
    // et c'est le versant visible du signalement d'Eric (#1393, renderer
    // Diretta et PC vu comme zone DLNA) : des réglages sans effet, et rien qui
    // le dise.
    //
    // `is_network_output` n'est PAS la bonne borne : une sortie PULL hors
    // dépôt (`diretta`) va chercher le .dsf elle-même sans être « réseau » au
    // sens de ce fichier, et c'est justement la zone du signalement. Le fil
    // est CONSTATÉ (`wire_carries_raw_dsd`), pas déduit.
    //
    // La sortie LOCALE est exclue : elle a sa sonde d'exécution, qui dit déjà
    // « DSP contourné pour DoP » quand c'est le cas, et qui est plus juste que
    // toute déduction faite ici.
    let dsd_brut_hors_sortie_locale =
        is_dsd && output_type != "local" && wire_carries_raw_dsd(wire);
    // « Armé » et « appliqué » ne sont pas la même chose.
    let dsp_applique = dsp_enabled && !dsd_brut_hors_sortie_locale;
    let dsp_contourne_par_le_dsd = dsp_enabled && dsd_brut_hors_sortie_locale;
    // ALAC native passthrough (opt-in per zone): the orchestrator serves the ALAC
    // file straight to a renderer that decodes it (bit-perfect, no FLAC transcode).
    // Mirror the orchestrator's condition (see orchestrator.rs `alac_passthrough`)
    // so the signal path does not show a phantom ALAC→FLAC transcode step when the
    // wire is really ALAC (forum #1131: DartZeel DAC displays ALAC at the right
    // resolution, yet the signal path claimed an ALAC→FLAC transcode).
    // A zone forced to serve WAV/LPCM (`dlna_lpcm`) always transcodes, so it takes
    // precedence over ALAC passthrough — matching the orchestrator.
    let zone_id = zone.id.unwrap_or(0);
    // `!dsd_passthrough` : même précédence que l'orchestrateur, où le forçage
    // WAV ne peut pas s'appliquer à un flux DSD servi brut (`dlna_needs_wav`
    // exige `will_be_flac`, faux dès que `needs_transcode_for_output` tombe).
    // Sans cette garde, une zone cochée « LPCM » annoncerait du WAV sur un fil
    // qui porte du DSD.
    let dlna_lpcm = is_network_output
        && !dsd_passthrough
        && ZoneRepo::with_backend(backend.clone()).get_dlna_lpcm(zone_id);
    // Zone opt-in 16-bit cap (Ruark R3, #1137): mirrors the orchestrator so the
    // signal path shows a real 16-bit downconvert instead of a phantom
    // bit-perfect passthrough when the source is hi-res.
    let dlna_cap_16bit = is_network_output
        && bit_depth > 16
        && ZoneRepo::with_backend(backend.clone()).get_dlna_cap_16bit(zone_id);
    // Zone opt-in: serve genuine 24-bit WAV (audio/L24) instead of the 16-bit
    // LPCM fallback. Mirrors orchestrator.rs `dlna_wav24` so the signal path
    // shows a lossless 24-bit WAV wire (not a phantom 16-bit truncation).
    let dlna_wav24 = is_network_output
        && bit_depth > 16
        && ZoneRepo::with_backend(backend.clone()).get_dlna_wav24(zone_id);
    // Même règle que l'orchestrateur, par la MÊME fonction : sur une source
    // FLAC dont la zone demande le FLAC natif, le forçage WAV ne s'applique pas
    // — il vise le décodeur ALAC du renderer. Sans ce miroir, le chemin du
    // signal annoncerait un transcodage vers WAV là où le fil porte du FLAC,
    // c'est-à-dire exactement le genre d'affichage inventé que ce dépôt traque.
    let source_is_flac = source_format == Some(AudioFormat::Flac);
    let native_flac_opt_in =
        is_network_output && ZoneRepo::with_backend(backend.clone()).get_dlna_native_flac(zone_id);
    let dlna_lpcm = tune_core::orchestrator::wav_override_applies(
        dlna_lpcm,
        source_is_flac,
        native_flac_opt_in,
    );
    let dlna_wav24 = tune_core::orchestrator::wav_override_applies(
        dlna_wav24,
        source_is_flac,
        native_flac_opt_in,
    );
    let alac_passthrough = source_format == Some(AudioFormat::Alac)
        && is_network_output
        && !dlna_lpcm
        && !dlna_wav24
        && !dlna_cap_16bit
        && ZoneRepo::with_backend(backend.clone()).get_alac_passthrough(zone_id);
    // Miroir de la condition AAC de l'orchestrateur (voir orchestrator.rs).
    let aac_passthrough = source_format == Some(AudioFormat::Aac)
        && is_network_output
        && !dlna_lpcm
        && !dlna_wav24
        && ZoneRepo::with_backend(backend.clone()).get_aac_passthrough(zone_id);
    let needs_transcode_for_output = is_network_output
        && !dsd_passthrough
        && !alac_passthrough
        && !aac_passthrough
        && source_format
            .as_ref()
            .is_some_and(|f| f.needs_transcode_for_dlna());
    // OAAT transcodes everything to WAV except WAV itself
    let is_oaat = output_type == "oaat";
    let oaat_transcodes = is_oaat
        && source_format
            .as_ref()
            .is_some_and(|f| *f != AudioFormat::Wav);

    // The renderer may be served WAV/LPCM even for a FLAC/ALAC source when it
    // does not advertise `audio/flac` (`orchestrator::dlna_needs_wav`, decided
    // by async SOAP negotiation this synchronous builder cannot replay). Trust
    // the live session's real container over the static transcode-target guess
    // so the path shows "ALAC → WAV" instead of a phantom "ALAC → FLAC" (Sevy,
    // LHC-52). Only "wav" changes the verdict; anything else keeps prior logic.
    let wire_wav = output_container.is_some_and(|c| c.eq_ignore_ascii_case("wav"));

    let (transport_bit_perfect, transport_desc, output_format_name) = match output_type {
        "dlna" | "openhome" => {
            if wire_wav || dlna_lpcm || dlna_wav24 {
                // Renderer served WAV/LPCM, not FLAC — the signal path must say
                // so (a renderer showing "WAV/PCM" otherwise contradicted Tune's
                // "→ FLAC" label, LHC). Three causes, same wire: the zone forces
                // 16-bit LPCM (`dlna_lpcm`) or genuine 24-bit WAV (`dlna_wav24`),
                // or the renderer doesn't advertise `audio/flac` and the
                // orchestrator fell back to WAV (`dlna_needs_wav`) — detected here
                // from the live session's real container (`wire_wav`), which the
                // synchronous builder cannot renegotiate. The plain LPCM fallback
                // is 16-bit (audio/L16), bit-perfect only when the lossless source
                // already fits 16 bits (Sevy, #1137). The opt-in `dlna_wav24` path
                // preserves the full 24-bit source, so it stays bit-perfect.
                // A *native* WAV source is served byte-for-byte (WAV never
                // transcodes for DLNA), so it is bit-perfect at any depth
                // regardless of `dlna_wav24` — which only governs the FLAC/ALAC→WAV
                // fallback (Sandro/Progman: WAV 24-bit direct showed red without it).
                let wav_bit_perfect = wav_wire_bit_perfect(
                    is_lossless,
                    matches!(source_format, Some(AudioFormat::Wav)),
                    dlna_wav24,
                    bit_depth,
                );
                (wav_bit_perfect, "DLNA/UPnP", "WAV")
            } else if needs_transcode_for_output || dlna_cap_16bit {
                // Cap forces a 16-bit FLAC downconvert (not bit-perfect) even for
                // an otherwise-direct FLAC source (Ruark R3, #1137).
                let target = source_format
                    .map(|f| f.dlna_transcode_target())
                    .unwrap_or(AudioFormat::Flac);
                (false, "DLNA/UPnP", target.display_name())
            } else {
                // FLAC, WAV, MP3, AAC → passthrough (bit-perfect for lossless)
                (true, "DLNA/UPnP", format_name)
            }
        }
        "oaat" => {
            // Lossless PCM → WAV preserves every bit, but DSD → WAV is a domain
            // conversion (1-bit sigma-delta decimated to multi-bit PCM), so it is
            // NOT bit-perfect even though DSD counts as a lossless *format*.
            (
                (is_lossless && !is_dsd) || !oaat_transcodes,
                "OAAT",
                if oaat_transcodes { "WAV" } else { format_name },
            )
        }
        // AirPlay 1 comme AirPlay 2 : le protocole impose de l'ALAC 44,1/16.
        // La conversion a lieu POUR DE VRAI, le verdict `false` est donc juste
        // — c'est le LIBELLÉ qui manquait : sans ce bras, une zone AirPlay 2
        // (créée par `discovery_setup.rs`, `(Some(Box::new(ap2)), "airplay2")`)
        // tombait dans le fourre-tout et affichait « airplay2 » en minuscules
        // comme nom de transport (#2189).
        "airplay" => (false, "AirPlay", "ALAC"),
        "airplay2" => (false, "AirPlay 2", "ALAC"),
        "chromecast" => {
            if needs_transcode_for_output {
                let target = source_format.unwrap().dlna_transcode_target();
                (false, "Chromecast", target.display_name())
            } else {
                (false, "Chromecast", format_name)
            }
        }
        "bluos" => {
            if needs_transcode_for_output {
                let target = source_format.unwrap().dlna_transcode_target();
                (false, "BluOS", target.display_name())
            } else {
                (true, "BluOS", format_name)
            }
        }
        // `slimproto` EST le protocole Squeezebox, et l'orchestrateur les
        // traite déjà à l'identique (`is_network_output_type` les liste tous
        // les deux). Le panneau, lui, ne nommait que `squeezebox` : une zone
        // créée par le serveur Slimproto (`tune-core/src/slimproto/mod.rs`,
        // `get_or_create(&player_name, Some("slimproto"), …)`) tombait dans le
        // fourre-tout et sortait « non bit-perfect » quoi qu'il arrive (#2189).
        "squeezebox" | "slimproto" => {
            let transport = if output_type == "slimproto" {
                "Slimproto"
            } else {
                "Squeezebox"
            };
            if needs_transcode_for_output {
                let target = source_format.unwrap().dlna_transcode_target();
                (false, transport, target.display_name())
            } else {
                (true, transport, format_name)
            }
        }
        "browser" => (true, "Browser", format_name),
        "local" => {
            // Show the actual audio backend (ASIO / WASAPI / CoreAudio / ALSA)
            let transport = match audio_backend {
                "ASIO" => "ASIO (exclusive)",
                "WASAPI" => "WASAPI",
                "CoreAudio" => "CoreAudio",
                "ALSA" => "ALSA",
                other => other,
            };
            (
                runtime_signal_path
                    .map(runtime_transport_is_intact)
                    .unwrap_or(true),
                transport,
                format_name,
            )
        }
        // Tout le reste est une sortie PULL : elle va CHERCHER le flux
        // elle-même et reçoit nos octets TELS QUELS — `hqplayer`, `diretta`,
        // et tout greffon hors dépôt. Ce bras rendait `false`
        // INCONDITIONNELLEMENT, et son second membre — la chaîne brute de la
        // base — servait de nom de transport.
        //
        // Alex Campbell (Tune 0.9.98, Linux, sortie HQPlayer, fil 1524) :
        // « When playing local **or streaming** music files to HQPlayer, Tune
        // is reporting that it is transcoding. » Le « local OU streaming » est
        // le fait qui tranche : le symptôme est inconditionnel, ce qu'aucune
        // règle dépendant du format ne produirait. Une zone HQPlayer était
        // déclarée non bit-perfect sur un FLAC 44,1/16 servi octet pour octet,
        // sans EQ ni ReplayGain, sans qu'aucun transcodage n'ait lieu (#2189).
        //
        // Le verdict n'est plus écrit ici : il est LU du chemin audio, par la
        // fonction que celui-ci utilise pour décider
        // (`orchestrator::is_pull_dsp_output_type`, extraite de
        // `pull_output_needs_dsp_transcode`). Sur ces sorties le transport ne
        // touche aucun échantillon ; le seul traitement possible est celui que
        // cette même fonction force — EQ, correction de pièce, ReplayGain — et
        // il est déjà compté plus bas par `dsp_applique` et `replaygain_step`.
        // Le verdict global retombe donc à `false` dès qu'un égaliseur est
        // armé, exactement là où le transcodage a réellement lieu.
        other => (
            tune_core::orchestrator::is_pull_dsp_output_type(Some(other)),
            libelle_de_transport(other),
            format_name,
        ),
    };

    // Detect sample rate capping (DSD excluded — the DSD→PCM transcode
    // already handles rate conversion; showing a separate resampler step
    // would be misleading since sample_rate here is the DSD MHz rate).
    let resampling_active = !is_dsd
        && zone
            .max_sample_rate
            .is_some_and(|max| (sample_rate as u32) > max);

    // Overall bit-perfect: lossless source + no transcoding + no DSP + no
    // resampling + no ReplayGain. Volume is excluded — it's a user preference,
    // not a signal degradation. ReplayGain, lui, multiplie chaque échantillon :
    // l'orchestrateur le traite déjà comme l'EQ (`zone_replaygain_changes_audio`
    // force le chemin transcodé), le verdict doit dire la même chose (#1627).
    // + le repli mono (#2362) : sommer les deux voies et les réémettre
    // identiques réécrit chaque échantillon. Une zone qui l'active n'est PAS
    // bit-perfect, et le panneau doit le dire — c'est exactement la promesse
    // que #1548/#1559 (EQ) et #1627 (ReplayGain) avaient laissé mentir.
    // `dsp_applique`, et non `dsp_enabled` : un EQ armé qu'un flux DSD brut
    // met hors de portée ne touche AUCUN échantillon. Le faire tomber le
    // verdict serait mentir dans l'autre sens (#1393).
    let bit_perfect = is_lossless
        && transport_bit_perfect
        && !dsp_applique
        && !resampling_active
        && replaygain_step.is_none()
        && mono_downmix_step.is_none();

    // Débit de la SOURCE, annoncé seulement quand elle le nomme elle-même.
    //
    // C'est le message que voit l'utilisateur pour le mp3-128 de Bandcamp
    // (#2074). La règle écrite dans le plugin — « un flux à 128 kbit/s doit
    // être annoncé comme tel PARTOUT où il apparaît »
    // (`plugins/tune-bandcamp/src/lib.rs`) — n'était tenue que sur l'écran
    // Bandcamp. Passée en zone, la même piste s'affichait « MP3 44kHz/16bit »,
    // exactement comme un 320 : le seul public de ce logiciel est celui qui
    // règle sa chaîne au bit près, et c'est précisément à lui que la
    // différence était cachée.
    //
    // Filtré sur le verdict avec perte : un débit sur un FLAC n'aurait aucun
    // sens, et un album Bandcamp ACHETÉ en lossless ne doit surtout pas
    // hériter du chiffre de l'extrait.
    let bitrate_label = np
        .bitrate_kbps
        .filter(|kbps| *kbps > 0 && !is_lossless)
        .map(|kbps| format!(" {kbps} kbit/s"))
        .unwrap_or_default();

    // Build steps
    let source_desc = if is_dsd {
        // DSD rates are in MHz range — display as e.g. "DSD64 2.8 MHz" or "DSD128 5.6 MHz"
        dsd_resolution_label(sample_rate)
    } else if sample_rate >= 1000 {
        format!(
            "{format_name}{bitrate_label} {sr}kHz/{bit_depth}bit",
            sr = sample_rate / 1000
        )
    } else {
        format!("{format_name}{bitrate_label} {sample_rate}Hz/{bit_depth}bit")
    };

    let mut steps = vec![json!({
        "name": "Source",
        "description": source_desc,
        "bit_perfect": true,
    })];

    // Decoder step. Skipped for DSD: the Source already reads e.g.
    // "DSD64 2.8 MHz" and the DSD→PCM/FLAC conversion is shown by the Transcoder
    // step, so a bare "DSD64" decoder line was just a confusing duplicate.
    if !is_dsd {
        steps.push(json!({
            "name": "Decoder",
            "description": format_name,
            "bit_perfect": is_lossless,
        }));
    }

    // Transcoding step (only if transcoding occurs). Include the zone-forced
    // WAV/LPCM (dlna_lpcm), 16-bit-cap (dlna_cap_16bit) and async WAV-fallback
    // (wire_wav) paths: all re-encode the stream, so the step must appear even
    // when the source format itself wouldn't need transcoding for DLNA (a FLAC
    // source with LPCM/cap-16, or an ALAC/FLAC source to a renderer that fell
    // back to WAV) — otherwise the path claimed a bit-perfect passthrough that
    // isn't happening (LHC: renderer shows WAV 16/44 while Tune showed ALAC→FLAC;
    // Sevy: LHC-52 served WAV while Tune showed ALAC→FLAC).
    let wire_transcode = wire_wav && !matches!(format_name, "WAV");
    let transcode_active = needs_transcode_for_output
        || oaat_transcodes
        // AirPlay 2 encode en ALAC 44,1/16 comme AirPlay 1 : l'étape est la
        // même, et elle manquait ici aussi (#2189).
        || matches!(output_type, "airplay" | "airplay2")
        || dlna_lpcm
        || dlna_wav24
        || dlna_cap_16bit
        || wire_transcode;
    if transcode_active {
        // OAAT lossless PCM → WAV preserves all audio data, but DSD → WAV is a
        // lossy domain conversion (see the "oaat" transport arm above). A DLNA
        // WAV/LPCM output likewise preserves the samples only when the source
        // already fits the 16-bit LPCM cap — unless the zone opted into genuine
        // 24-bit WAV (`dlna_wav24`), which keeps the full depth.
        let wav_output = wire_wav || dlna_lpcm || dlna_wav24;
        let transcode_lossless = (is_oaat && is_lossless && !is_dsd)
            || (wav_output && is_lossless && (dlna_wav24 || bit_depth <= 16));
        // Reflect the OUTPUT resolution the renderer actually receives: 24-bit
        // for the opt-in 24-bit WAV path, 16-bit when the zone caps to 16-bit OR
        // serves the plain LPCM fallback (audio/L16 is 16-bit), and the
        // max-sample-rate cap when set.
        //
        // Quand le fil renseigne ces valeurs, elles PRIMENT : elles décrivent
        // ce que le renderer reçoit, là où les règles ci-dessous ne font que
        // rejouer les décisions de l'orchestrateur et prennent du retard à
        // chaque évolution du chemin audio.
        let out_bit_depth = wire_bit_depth.map(|v| v as i32).unwrap_or(if dlna_wav24 {
            bit_depth.min(24)
        } else if dlna_cap_16bit || wav_output {
            bit_depth.min(16)
        } else {
            bit_depth
        });
        let out_sample_rate = wire_sample_rate.map(|v| v as i32).unwrap_or_else(|| {
            zone.max_sample_rate
                .map(|m| (sample_rate as u32).min(m) as i32)
                .unwrap_or(sample_rate)
        });
        // Garde-fou #1315 : le nom du conteneur est deviné, les chiffres sont
        // mesurés. Une résolution DSD ne peut donc pas sortir d'ici sous un
        // nom de conteneur PCM, quelle que soit la cible de transcodage.
        let out_desc = output_stage_label(output_format_name, out_sample_rate, out_bit_depth);
        steps.push(json!({
            "name": "Transcoder",
            "description": format!("{source_desc} \u{2192} {out_desc}"),
            "bit_perfect": transcode_lossless,
        }));
    }

    // Resampler step (when zone max_sample_rate caps the output)
    if resampling_active {
        let max_sr = zone.max_sample_rate.unwrap();
        let src_khz = sample_rate / 1000;
        let dst_khz = max_sr / 1000;
        steps.push(json!({
            "name": "Resampler",
            "description": format!("{src_khz}kHz \u{2192} {dst_khz}kHz"),
            "bit_perfect": false,
        }));
    }

    // ReplayGain step — placé avant Volume/DSP, comme dans le chemin réel
    // (le gain est appliqué avant l'égaliseur, orchestrator.rs). Jamais en
    // PURE, jamais en mode off, jamais sans gain stocké : l'étape n'existe
    // que quand un facteur ≠ 1 multiplie réellement les échantillons.
    if let Some(rg) = &replaygain_step {
        steps.push(json!({
            "name": "ReplayGain",
            "description": rg.description,
            "bit_perfect": false,
            // Additifs (#1627) : la description reste le libellé prêt à
            // afficher, ces deux champs permettent au client de composer le
            // sien (icône, traduction) sans analyser une chaîne française.
            "granularity": rg.granularity,
            "gain_source": rg.source,
        }));
    }

    // La sonde locale tranche si le gain a réellement été appliqué. Pour les
    // sorties sans sonde, conserver l'affichage historique fondé sur le
    // réglage de zone.
    if let Some(runtime) = runtime_signal_path {
        match runtime.volume {
            // Le même fait ne peut pas être peint de deux couleurs selon la
            // plateforme : la branche sans sonde (macOS, Linux, navigateur,
            // toutes les sorties réseau, quelques lignes plus bas) marque déjà
            // l'étape Volume comme intacte, parce que le volume est une
            // préférence et non une dégradation. L'étape reste affichée, avec
            // son pourcentage : rien n'est caché, seule la couleur cesse de
            // contredire le verdict (#2053).
            OutputVolumeState::Applied => steps.push(json!({
                "name": "Volume",
                "description": format!("Volume logiciel {}%", (ui_volume * 100.0).round() as i32),
                "bit_perfect": true,
            })),
            OutputVolumeState::BypassedDop => steps.push(json!({
                "name": "Volume",
                "description": "Volume contourné pour DoP",
                "bit_perfect": true,
            })),
            OutputVolumeState::Unity => {}
        }
    } else if !volume_full {
        steps.push(json!({
            "name": "Volume",
            "description": format!("Volume {}%", (ui_volume * 100.0).round() as i32),
            "bit_perfect": true,
        }));
    }

    // L'état d'exécution distingue traitement et contournement. C'est le
    // reliquat commun de #2205/#2233 : un réglage enregistré ne disait pas ce
    // qui avait effectivement atteint le ring Windows.
    let dsp_metrics = ps.output_dsp_metrics.map(|metrics| {
        json!({
            "eq_overs": metrics.eq_overs,
            "eq_non_finite_samples": metrics.eq_non_finite_samples,
        })
    });
    if let Some(runtime) = runtime_signal_path {
        let dsp_step = match runtime.dsp {
            OutputDspState::Applied => Some((
                eq_step_description.as_deref().unwrap_or("DSP appliqué"),
                false,
            )),
            OutputDspState::BypassedPure => Some(("DSP contourné par PURE", true)),
            OutputDspState::BypassedDop => Some(("DSP contourné pour DoP", true)),
            OutputDspState::Unknown => Some(("État DSP indéterminé", false)),
            OutputDspState::Inactive => None,
        };
        if let Some((description, intact)) = dsp_step {
            steps.push(json!({
                "name": "DSP",
                "description": description,
                "bit_perfect": intact,
                "metrics": dsp_metrics.clone(),
            }));
        }
    } else if dsp_contourne_par_le_dsd {
        // DIRE le contournement plutôt que de le taire. L'auditeur a un
        // égaliseur ARMÉ et n'entend rien changer : c'est exactement ce qu'Eric
        // a signalé (#1393). Faire disparaître l'étape le laisserait devant le
        // même curseur inerte, sans explication ; l'annoncer « actif » serait
        // le mensonge que #1315 et #2053 ont déjà coûté. On dit donc les deux
        // choses : il y a un DSP, et il ne s'applique pas ici.
        //
        // `bit_perfect: true` — le fil porte le DSD tel quel, rien n'y a
        // touché. Même convention que « DSP contourné pour DoP », que la sonde
        // de la sortie locale publie déjà.
        steps.push(json!({
            "name": "DSP",
            "description": "DSP contourné (DSD natif servi brut)",
            "bit_perfect": true,
        }));
    } else if dsp_applique {
        steps.push(json!({
            "name": "DSP",
            "description": eq_step_description.as_deref().unwrap_or("EQ/DSP active"),
            "bit_perfect": false,
        }));
    }

    // Étape « Mono » (#2362) — APRÈS le DSP et juste avant le transport, parce
    // que c'est exactement là qu'elle a lieu dans la chaîne : le repli tombe en
    // dernier dans `apply_local_dsp`, après l'égaliseur, le convolveur et le
    // crossfeed, qui ont tous besoin de leur contexte stéréo.
    //
    // `bit_perfect: false` sans hésitation : la profondeur et la fréquence sont
    // conservées, mais le CONTENU des deux voies est remplacé par leur demi-
    // somme. Ce n'est pas une préférence d'écoute comme le volume, c'est une
    // transformation du signal — et l'utilisateur qui la demande a le droit de
    // savoir ce qu'il échange.
    if let Some(desc) = &mono_downmix_step {
        steps.push(json!({
            "name": "Mono",
            "description": desc,
            "bit_perfect": false,
        }));
    }

    // Transport step
    steps.push(json!({
        "name": "Transport",
        "description": transport_desc,
        "bit_perfect": transport_bit_perfect,
        "detail": runtime_signal_path.and_then(runtime_signal_reason_detail),
    }));

    let renderer_name = renderer_label
        .or(zone.output_device_id.as_deref())
        .unwrap_or(output_type);
    steps.push(json!({
        "name": "Renderer",
        "description": renderer_name,
        "bit_perfect": transport_bit_perfect,
    }));

    // Build summary
    let bp_label = if bit_perfect { " (bit-perfect)" } else { "" };
    let summary = if transcode_active {
        format!(
            "{format_name} \u{2192} {output_format_name} transcode \u{2192} {transport_desc}{bp_label}"
        )
    } else {
        format!("{format_name} \u{2192} {transport_desc}{bp_label}")
    };

    Some(json!({
        "bit_perfect": bit_perfect,
        // Whether the *source* is a lossless format (FLAC, ALAC, WAV, DSD, …).
        // Distinct from bit_perfect: a lossless source transcoded to another
        // lossless container (DSD→FLAC, ALAC→FLAC for a DLNA renderer) is not
        // bit-perfect but is still lossless — the UI must not call it "lossy".
        "lossless": is_lossless,
        "summary": summary,
        "steps": steps,
        "runtime_observed": runtime_signal_path.is_some(),
        "runtime_reasons": runtime_signal_path.map(|status| &status.reasons),
        "dsp_metrics": dsp_metrics,
    }))
}

/// Ce que la source est, avant tout ce que la sortie lui fait : le premier
/// bloc de `build_signal_path`, sorti tel quel (REF-4 phase 2, #2219). Les
/// champs sont les `let` que la suite de la fonction relit, sous leur nom.
struct Source<'w> {
    /// Conteneur réellement servi (None hors session : sortie locale, démarrage).
    output_container: Option<&'w str>,
    /// Fréquence et profondeur réellement émises, seulement si renseignées.
    wire_sample_rate: Option<u32>,
    wire_bit_depth: Option<u16>,
    source_format: Option<AudioFormat>,
    is_dsd: bool,
    sample_rate: i32,
    bit_depth: i32,
    format_name: &'static str,
    is_lossless: bool,
}

/// Lit la piste, le fil et la lecture en cours pour décrire la source.
fn decrire_la_source<'w>(
    np: &tune_core::playback::NowPlaying,
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    wire: Option<&'w StreamInfo>,
) -> Source<'w> {
    // Conteneur réellement servi (None hors session : sortie locale, démarrage).
    let output_container = wire.map(|w| w.format.as_str());
    // Fréquence et profondeur réellement émises. Une session fraîchement créée
    // peut encore porter des zéros (`StreamInfo::default`) : on ne retient que
    // des valeurs renseignées, sans quoi l'affichage annoncerait « 0kHz/0bit ».
    let wire_sample_rate = wire.map(|w| w.sample_rate).filter(|v| *v > 0);
    let wire_bit_depth = wire.map(|w| w.bit_depth).filter(|v| *v > 0);
    // A decoded live radio has no library row and its NowPlaying resolution is
    // only the bootstrap value chosen before the decoder opens the upstream.
    // Once the session publishes its detected PCM format, that observation is
    // authoritative for the source line too (France Musique: 48 kHz, not the
    // 44.1 kHz bootstrap value from session creation — #2427).
    let radio_wire_sample_rate = (np.source == "radio")
        .then_some(wire_sample_rate)
        .flatten()
        .map(|v| v as i32);
    let radio_wire_bit_depth = (np.source == "radio")
        .then_some(wire_bit_depth)
        .flatten()
        .map(|v| v as i32);

    // Look up track details for format/sample_rate/bit_depth
    let track = np.track_id.and_then(|tid| {
        TrackRepo::with_backend(backend.clone())
            .get(tid)
            .ok()
            .flatten()
    });

    let fmt_str = np
        .format
        .clone()
        .or_else(|| track.as_ref().and_then(|t| t.format.clone()))
        .unwrap_or_else(|| "flac".into());
    let source_format = AudioFormat::from_extension(&fmt_str);
    let is_dsd = matches!(fmt_str.as_str(), "dsd" | "dsf" | "dff");
    // For DSD files, prefer the track's original sample rate and bit depth
    // from the database (which represent the SOURCE format: e.g. 2822400 Hz
    // / 1-bit for DSD64) over the NowPlaying values, which may contain the
    // TRANSCODED PCM values (e.g. 176400 Hz / 24-bit) when the file was
    // converted for network output (DLNA, OpenHome, etc.).
    let sample_rate = if is_dsd {
        track
            .as_ref()
            .and_then(|t| t.sample_rate)
            .or_else(|| np.sample_rate.map(|v| v as i32))
            .unwrap_or(2_822_400)
    } else {
        radio_wire_sample_rate
            .or_else(|| np.sample_rate.map(|v| v as i32))
            .or_else(|| track.as_ref().and_then(|t| t.sample_rate))
            // Dernier recours quand ni la lecture en cours ni la base ne
            // savent : le fil, qui décrit ce qui part vraiment. Sans lui on
            // affichait 44100 en dur — une valeur inventée, affirmée avec le
            // même aplomb qu'une vraie mesure, et fausse dès que le fichier
            // était en Hi-Res (métadonnées non lues au scan).
            .or_else(|| wire_sample_rate.map(|v| v as i32))
            .unwrap_or(44100)
    };
    let bit_depth = if is_dsd {
        track
            .as_ref()
            .and_then(|t| t.bit_depth)
            .or_else(|| np.bit_depth.map(|v| v as i32))
            .unwrap_or(1)
    } else {
        radio_wire_bit_depth
            .or_else(|| np.bit_depth.map(|v| v as i32))
            .or_else(|| track.as_ref().and_then(|t| t.bit_depth))
            .or_else(|| wire_bit_depth.map(|v| v as i32))
            .unwrap_or(16)
    };

    let format_name = if is_dsd {
        dsd_family_name(sample_rate)
    } else if let Some(f) = source_format.as_ref() {
        f.display_name()
    } else {
        // A UPnP/NAS media-server source reports its codec as a MIME type or DLNA
        // profile (e.g. "audio/mp4", "AAC_ISO_320"), not a file extension, so
        // from_extension() returned None and the signal path showed "Unknown"
        // (Yves: NAS as source). Recognize the codec from the raw string instead.
        let l = fmt_str.to_lowercase();
        let is_m4a = l.contains("mp4") || l.contains("m4a") || l.contains("aac");
        if l.contains("alac") || (is_m4a && bit_depth >= 24) {
            // audio/mp4 (M4A) is ambiguous ALAC vs AAC — same container/MIME. A
            // DIDL res@bitsPerSample >= 24 means lossless ALAC, not lossy AAC
            // (Yves: NAS ALAC read 24-bit by the DartZeel but shown as AAC here).
            "ALAC"
        } else if is_m4a {
            "AAC"
        } else if l.contains("mp3") || l.contains("mpeg") {
            "MP3"
        } else if l.contains("flac") {
            "FLAC"
        } else if l.contains("wav") {
            "WAV"
        } else if l.contains("ogg") || l.contains("vorbis") {
            "OGG"
        } else if l.contains("opus") {
            "OPUS"
        } else {
            "Unknown"
        }
    };
    // For a media-server source (no from_extension AudioFormat) the lossless
    // verdict follows the recognized codec name, so a 24-bit ALAC is no longer
    // shown "Avec perte" (Yves).
    let is_lossless = source_format
        .as_ref()
        .map(|f| f.is_lossless())
        .unwrap_or_else(|| matches!(format_name, "ALAC" | "FLAC" | "WAV"));
    Source {
        output_container,
        wire_sample_rate,
        wire_bit_depth,
        source_format,
        is_dsd,
        sample_rate,
        bit_depth,
        format_name,
        is_lossless,
    }
}
