use super::*;

/// Le rang qu'il faut écrire dans `listen_history`, sachant l'état de la zone.
///
/// Toute la subtilité est le tirage aléatoire. En lecture séquentielle, le
/// rang est ce qui permet de reprendre la playlist à la piste 7. En lecture
/// aléatoire, il ne désigne plus rien de reproductible : la permutation
/// (`shuffle_order`) est régénérée à chaque activation, donc « position 7 »
/// dans le tirage d'hier tombera sur une autre piste demain. L'arbitrage rendu
/// sur #2441 est de RE-TIRER plutôt que de faire semblant de rejouer le même
/// tirage — ce que cette fonction inscrit à l'écriture, faute de quoi il
/// faudrait redevenir l'état « aléatoire » de la zone au moment où l'accueil
/// s'affiche, ce qui n'existe plus.
///
/// Un `None` en base se relit donc « rouvre au début », que ce soit parce
/// qu'on re-tire ou parce que la ligne est antérieure à la migration 94.
pub fn rang_a_retenir(shuffle: bool, queue_position: i64) -> Option<i64> {
    if shuffle || queue_position < 0 {
        None
    } else {
        Some(queue_position)
    }
}

/// Le forçage WAV d'une zone s'applique-t-il à CETTE source ?
///
/// « Forcer le WAV » (`dlna_lpcm` / `dlna_wav24`) existe pour contourner le
/// décodeur ALAC du renderer — le LHC-56 de Yves claque au démarrage sur de
/// l'ALAC direct. L'appliquer aussi aux sources FLAC est un dommage
/// collatéral, jamais l'objectif.
///
/// Tant que les deux réglages s'excluaient, « Forcer le WAV » l'emportait en
/// silence sur « FLAC natif » : un FLAC partait en WAV sans que rien ne
/// l'explique, et l'utilisateur en déduisait que Tune gardait en mémoire les
/// réglages du morceau précédent (forum #1437). Les deux cases décrivent en
/// réalité deux sources différentes et peuvent coexister : l'ALAC part en WAV,
/// le FLAC reste du FLAC.
///
/// L'exception exige l'opt-in `dlna_native_flac`. Sans lui, une source FLAC
/// continue de suivre le forçage — ce dont ont besoin les renderers qui ne
/// savent pas lire le FLAC.
pub fn wav_override_applies(
    force_wav_requested: bool,
    source_is_flac: bool,
    native_flac_opt_in: bool,
) -> bool {
    force_wav_requested && !(source_is_flac && native_flac_opt_in)
}

/// Warm-cache for Tidal/Qobuz HI-RES DASH transcodes is opt-in: it changes the
/// file served on the HI-RES streaming path (cache-hit → a previously-finished
/// transcode instead of a fresh one), so it stays OFF until validated on a real
/// DLNA renderer. Enable with `TUNE_DASH_WARM_CACHE=1`.
/// Facteur linéaire d'un trim de gain par renderer (`zone_{id}_gain_trim_db`).
/// Clampe à ±12 dB — au-delà, on harmonise plus rien, on casse.
pub(super) fn gain_trim_factor(trim_db: f64) -> f64 {
    10f64.powf(trim_db.clamp(-12.0, 12.0) / 20.0)
}

pub(super) fn dash_warm_cache_enabled() -> bool {
    std::env::var("TUNE_DASH_WARM_CACHE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// DIDL `res@duration` (ms) for a native passthrough stream served raw to a
/// network renderer (FLAC/ALAC/… sent as-is, not transcoded).
///
/// Prefer the file container's authoritative duration (`probed_secs`, read from
/// the FLAC STREAMINFO / lofty properties — the true playable length of the
/// bytes we serve) over the scanned `track.duration_ms`, which can be a few
/// seconds too long (recovered by a slow/fallback NAS scan, or drifted vs. the
/// real sample count). An over-long `res@duration` makes the gapless-queued
/// (SetNextAVTransportURI) track cut near EOF and lose its progress display on
/// the Marantz ND 8006 (#1132), because the renderer models the auto-advanced
/// track purely from the DIDL instead of re-probing the stream.
///
/// Falls back to the scanned duration when the probe failed or is non-positive
/// (e.g. a NAS read timeout), so we never blank the duration entirely.
pub(super) fn passthrough_didl_duration_ms(probed_secs: Option<f64>, scanned_ms: i64) -> i64 {
    probed_secs
        .filter(|s| s.is_finite() && *s > 0.0)
        .map(|s| (s * 1000.0).round() as i64)
        .filter(|&ms| ms > 0)
        .unwrap_or(scanned_ms)
}

/// Recover a local file's real duration (ms) at play time when the DB row has
/// none (`duration_ms <= 0`). DSD (`.dsf`/`.dff`) is computed from the header —
/// lofty (which `get_duration` uses) reports 0 for most DSD files, which is how
/// the 0 got into the DB in the first place — everything else falls back to
/// lofty. Returns `None` when no positive duration can be determined.
pub(super) async fn probe_local_duration_ms(
    file_path: &str,
    source_format: Option<AudioFormat>,
) -> Option<i64> {
    if source_format == Some(AudioFormat::Dsd) {
        let p = file_path.to_string();
        return tokio::task::spawn_blocking(move || {
            let dur = if p.to_ascii_lowercase().ends_with(".dff") {
                crate::audio::dff::parse_dff(&p)
                    .ok()
                    .and_then(|i| i.duration_ms())
            } else {
                crate::audio::dsf::parse_dsf(&p)
                    .ok()
                    .and_then(|i| i.duration_ms())
            };
            dur.map(|ms| ms as i64)
        })
        .await
        .ok()
        .flatten()
        .filter(|&ms| ms > 0);
    }
    crate::audio::analyzer::get_duration(file_path)
        .await
        .ok()
        .map(|s| (s * 1000.0) as i64)
        .filter(|&ms| ms > 0)
}

/// La commande de transport a-t-elle pu être exécutée malgré l'erreur remontée ?
///
/// Un timeout SOAP (voir [`crate::outputs::dlna::SOAP_TIMEOUT_PREFIX`]) ne prouve
/// rien : la requête a pu atteindre un renderer lent et être honorée, seule la
/// réponse a manqué. Un refus de connexion, lui, est concluant — rien n'est
/// parti. Ce prédicat décide si l'on conserve la session de flux.
pub(crate) fn command_may_have_landed(err: &str) -> bool {
    // `contains` et non `starts_with` : send_to_output enveloppe l'erreur de la
    // sortie dans « Output device error: {e} », le marqueur n'est donc jamais en
    // tête. Un test couvre précisément ce chemin — s'y fier plutôt qu'à la forme
    // supposée de la chaîne.
    err.contains(crate::outputs::dlna::SOAP_TIMEOUT_PREFIX)
}

/// Le flux qui part sur le fil est-il du DSD BRUT ?
///
/// `application/x-dsd` par défaut, ou le MIME que le lecteur a lui-même
/// annoncé pour le DSF/DFF (`audio/x-dsf`, `audio/dff`…) : le passthrough
/// reprend celui-là quand il existe, parce que certains renderers n'acceptent
/// que le MIME exact qu'ils publient. Aucun MIME PCM ne porte ces trois
/// lettres, donc la reconnaissance ne peut pas mordre à côté — un FLAC servi à
/// la même zone n'est jamais confondu avec un DSD.
pub fn est_dsd_brut(mime_type: &str) -> bool {
    let m = mime_type.to_ascii_lowercase();
    m.contains("dsd") || m.contains("dsf") || m.contains("dff")
}

/// Should a network output be fed from a pre-transcoded temp file (blocking,
/// Content-Length known) rather than a streaming session?
///
/// A temp file is required for renderers that reject chunked transfer
/// (darTZeel LHC-208 etc.): every non-WAV target (FLAC), and every WAV target a
/// renderer demands as raw LPCM (`dlna_needs_wav`). The exception is
/// `dsd_lpcm_streams` — a DSD source going out as WAV/LPCM with the
/// `dsd_lpcm_stream` toggle on: the streaming path already advertises an exact
/// Content-Length (`StreamInfo::wav_content_length`), so blocking to /tmp is
/// pointless and, on DSD256/512, fatal (the ~decode exceeds the 120s temp-file
/// timeout → the renderer plays silence).
///
/// `dsp_active` PRIME sur tout le reste. Le bras progressif appelle
/// `decode_to_pcm_streaming_seeked`, qui ne reçoit ni égaliseur, ni convolveur,
/// ni facteur ReplayGain : seul `transcode_source_to_file` les applique. Une
/// zone dont un traitement est actif doit donc repasser par le fichier, sans
/// quoi le traitement est perdu EN SILENCE — famille #1216, déjà corrigée pour
/// le passthrough réseau, le navigateur et les sorties PULL.
///
/// Kept a pure function so the decision matrix is unit-testable without an
/// orchestrator.
pub(super) fn use_file_transcode_for(
    is_network: bool,
    target_is_wav: bool,
    dlna_needs_wav: bool,
    dsd_lpcm_streams: bool,
    dsp_active: bool,
) -> bool {
    is_network && (!target_is_wav || (dlna_needs_wav && !dsd_lpcm_streams)) || dsp_active
}

/// Le bras streaming HTTPS doit-il PRÉ-TRANSCODER au lieu de servir les octets
/// du CDN verbatim ?
///
/// Deux raisons, et la seconde manquait : le renderer ne sait pas lire le MIME
/// amont (Denon, Marantz, Revox — pas d'`audio/flac` dans leur Sink), OU un
/// traitement de zone doit entrer dans le signal. Une session proxy ne décode
/// rien : égaliseur, convolveur et ReplayGain y sont perdus EN SILENCE. C'est
/// le bras que ni #1168 (navigateur), ni #1653 (sorties PULL), ni #2950 (bras
/// progressif de `play_inner`) n'atteignaient — aucun ne passe par ici.
///
/// Fonction pure : la matrice de décision se teste sans orchestrateur, comme
/// `use_file_transcode_for`.
pub(super) fn streaming_needs_pretranscode(renderer_supports_mime: bool, dsp_active: bool) -> bool {
    !renderer_supports_mime || dsp_active
}

/// Format d'encodage du pré-transcodage streaming.
///
/// WAV/LPCM quand le renderer a REFUSÉ le MIME amont : c'est le profil
/// `DLNA.ORG_PN=LPCM`, 16 bits seulement, d'où le plafond historique (#1137,
/// Ruark R3 / LHC-62 muets en 24 bits sous ce profil).
///
/// FLAC quand il l'accepte et que SEUL le traitement impose le pré-transcodage :
/// le plafond 16 bits n'a alors aucune raison d'être, et l'appliquer
/// dégraderait un Hi-Res 24 bits pour un simple égaliseur. C'est le même
/// arbitrage que le bras DASH, qui encode déjà en FLAC pleine profondeur quand
/// le renderer sait le lire.
pub(super) fn streaming_pretranscode_format(renderer_supports_mime: bool) -> &'static str {
    if renderer_supports_mime {
        "flac"
    } else {
        "wav"
    }
}

/// Les types de sortie qui poussent l'audio vers un appareil PAR LE RÉSEAU.
///
/// **L'unique exemplaire de cette liste.** Elle était recopiée à l'identique en
/// trois endroits — `is_push_uri_output_type`, `resolve_local_track` et
/// [`PlaybackOrchestrator::seek`] — et #2893 en réclamait une quatrième. Toutes
/// y passent désormais : un renderer ajouté ici l'est partout, alors qu'une
/// copie oubliée serait restée MUETTE (un morceau qui repart du début).
///
/// `pub` depuis #2189 : il existait une QUATRIÈME copie, hors de cette caisse
/// — `build_signal_path` (`tune-server/src/routes/zones.rs`) — et elle avait
/// déjà dérivé : cinq types au lieu de six, `slimproto` manquant. Le panneau
/// et le chemin audio répondaient donc à deux questions différentes sur la
/// même zone. Le miroir d'affichage appelle maintenant CETTE fonction.
pub fn is_network_output_type(output_type: Option<&str>) -> bool {
    matches!(
        output_type,
        Some("dlna")
            | Some("openhome")
            | Some("chromecast")
            | Some("bluos")
            | Some("squeezebox")
            | Some("slimproto")
    )
}

/// La sortie va CHERCHER le flux elle-même et reçoit donc nos octets **tels
/// quels** : `hqplayer`, `airplay2`, `diretta`, tout greffon hors dépôt.
///
/// C'est la troisième famille de [`pull_output_needs_dsp_transcode`], extraite
/// telle quelle — ni élargie, ni rétrécie. Elle existe séparément parce que le
/// panneau du chemin du signal en a besoin SANS les drapeaux d'exécution
/// (`is_local`, `is_oaat`, format source) : il n'a que le type de la zone.
///
/// La conséquence pour l'affichage est directe et c'est tout le sujet de
/// #2189 : sur ces sorties, le transport ne touche AUCUN échantillon, donc il
/// est bit-perfect. Le seul traitement qui puisse s'y appliquer est celui que
/// [`pull_output_needs_dsp_transcode`] force — EQ, correction de pièce,
/// ReplayGain — et le panneau le compte déjà à part. Le bras par défaut de
/// `build_signal_path` rendait `false` inconditionnellement : une zone
/// HQPlayer était déclarée « non bit-perfect » sur un FLAC 44,1/16 servi
/// octet pour octet (Alex Campbell, 0.9.98 Linux, fil 1524).
pub fn is_pull_dsp_output_type(output_type: Option<&str>) -> bool {
    output_type.is_some() && !is_network_output_type(output_type) && output_type != Some("browser")
}

/// Is `output_type` (from [`PlaybackOrchestrator::output_type_of`]) one of the
/// push-URI renderer types (DLNA/OpenHome/Chromecast/BluOS/Squeezebox/
/// Slimproto) that receives a URI and can restart playback from byte 0 on a
/// redundant `SetAVTransportURI` (or equivalent) — the failure mode the
/// duplicate-net-play coalescing in `play_inner` (#1129) guards against?
/// Pull-based outputs — `local`, and out-of-tree outputs like `oaat` and
/// `diretta` that fetch/stream audio themselves rather than being pushed a
/// URI — never exhibit it, so they must be excluded here rather than only via
/// a `device_id` naming convention (a pull output has no reason to prefix its
/// `device_id` with `"local:"`). Pure so it's unit-testable without an
/// orchestrator or a registered output.
/// True when an output receives the stream with none of our DSP applied to it,
/// so an active EQ / room correction / ReplayGain must force the transcode path
/// to be heard at all.
///
/// The push-URI renderers of [`is_push_uri_output_type`] and browser zones are
/// handled by their own flags. What this covers is the third family: a PULL
/// output that fetches the audio itself and is not built in — `diretta`, and
/// anything an out-of-tree plugin registers. It was silently absent from the
/// list, so the EQ was computed and thrown away (Eric, forum ; same hole as
/// #1216 on a Beoplay A9).
///
/// Excluded on purpose:
/// - `local` and `oaat`, which already transcode as soon as the source format
///   is known, so the DSP reaches them;
/// - an unknown format, which there is no safe way to transcode;
/// - DSD, because converting a native DSD stream to PCM in order to apply an
///   EQ would be a degradation decided on the listener's behalf.
pub(super) fn pull_output_needs_dsp_transcode(
    output_type: Option<&str>,
    is_local: bool,
    is_oaat: bool,
    source_format: Option<AudioFormat>,
) -> bool {
    // La part « type de sortie » vit dans [`is_pull_dsp_output_type`], d'où le
    // panneau du chemin du signal la lit aussi (#2189). Même ensemble, à la
    // lettre : `is_push_uri_output_type` n'est qu'un alias de
    // `is_network_output_type`, que la fonction extraite appelle.
    is_pull_dsp_output_type(output_type)
        && !is_local
        && !is_oaat
        && source_format.is_some()
        && source_format != Some(AudioFormat::Dsd)
}

/// Les sorties qui reçoivent une URI et peuvent repartir de l'octet 0 sur un
/// envoi redondant (Revox S100) — la porte de coalescence de #1129.
///
/// Même ensemble que [`is_network_output_type`], et ce n'est pas un hasard :
/// « pousser une URI » est précisément ce que fait une sortie réseau ici. La
/// liste ne vit donc qu'à UN endroit ; ce nom-ci reste pour que la porte #1129
/// se lise pour ce qu'elle est.
pub(super) fn is_push_uri_output_type(output_type: Option<&str>) -> bool {
    is_network_output_type(output_type)
}

/// Profondeur de bits admissible par un appareil de lecture, en sortie.
///
/// Plancher a 16 : en dessous, plus rien ne lit le PCM de facon fiable.
/// Plafond a 24 : c'est la limite de la quasi-totalite des lecteurs reseau —
/// le Marantz ND8006 de Jean Valjean affiche « format non supporte » et reste
/// muet devant un flux 32 bits, qu'il soit transcode en WAV ou envoye en FLAC
/// direct (#1610). Le 32 bits venait de `track.bit_depth`, donc du scan : le
/// format FLAC l'autorise, et rien ne le ramenait a une valeur jouable.
///
/// La regle etait deja appliquee a trois endroits, ecrite de trois facons
/// (`max(16).min(24)`, `min(24).max(16)`) — et oubliee au quatrieme. Une
/// fonction unique rend l'oubli impossible a reproduire.
pub(crate) fn cap_output_bit_depth(bit_depth: u16) -> u16 {
    bit_depth.clamp(16, 24)
}

/// Resolution ANNONCEE d'une piste : frequence ou profondeur, telle que le
/// client l'affiche.
///
/// `ligne` est la valeur de la ligne `tracks` (la source, ce que le scan a lu
/// dans le fichier). `resolu` est celle du `ResolvedStream` — pour une piste
/// locale c'est la resolution de SORTIE, et `resolve_local_track` la fabrique
/// quand la ligne se tait (`unwrap_or(44100)` / `unwrap_or(16)`, puis
/// `cap_output_bit_depth`). La substituer affiche un chiffre que personne n'a
/// mesure.
///
/// Le streaming, lui, n'a pas de ligne en bibliotheque : `resolu` y EST la
/// resolution de la source, et reste le repli legitime.
pub(crate) fn resolution_annoncee(
    ligne: Option<u32>,
    resolu: Option<u32>,
    source_locale: bool,
) -> Option<u32> {
    if source_locale {
        // La ligne, ou rien. Jamais un chiffre fabrique par la sortie.
        ligne
    } else {
        ligne.or(resolu)
    }
}

/// Faut-il emballer ce DSD en DoP (trames PCM 24 bits au seizième du débit) ?
///
/// - **Sortie locale** : « natif » et « dop » y mènent tous deux, une carte son
///   ne recevant pas de DSD autrement.
/// - **Renderer réseau** : uniquement sur choix EXPLICITE « dop ». En « auto »
///   ou « natif », c'est `should_dsd_passthrough` qui tranche entre l'envoi du
///   fichier tel quel et le transcodage.
///
/// Règle extraite en fonction libre parce que son absence côté réseau était
/// invisible : `"dop"` n'était comparé qu'à un seul endroit du dépôt, sous un
/// garde `is_local_output` (#1772).
/// Cadence et canaux à ANNONCER pour un flux DoP, le fichier faisant foi.
///
/// L'en-tête WAV et la charge utile doivent décrire la même chose. L'encodeur
/// (`decode_dsd_to_dop_streaming`) se construit sur ce que rend
/// `parse_dsf`/`parse_dff` ; annoncer la ligne `tracks` à la place revient à
/// parier que la base et le fichier concordent. Quand ils divergent d'un canal,
/// chaque mot de 24 bits est décalé, le marqueur DoP ne tombe plus sur l'octet
/// de poids fort, et le DAC joue le train DSD comme du PCM : du bruit blanc.
///
/// La base ne sert que de repli, pour un en-tête illisible — mieux vaut
/// diffuser avec des valeurs approximatives que refuser de lire.
pub(super) fn dop_wire_params(
    probe: Option<(u32, u32)>,
    db_rate: Option<u32>,
    db_channels: u32,
) -> (u32, u16) {
    let rate = probe
        .map(|(sr, _)| sr)
        .unwrap_or_else(|| db_rate.unwrap_or(2_822_400));
    let channels = probe
        .map(|(_, ch)| ch as u16)
        .unwrap_or(db_channels as u16)
        .max(2);
    (rate, channels)
}

/// Le conteneur peut-il porter plus de 16 bits alors que la base l'ignore ?
///
/// Seul l'ALAC est concerné : sa profondeur ne se lit ni dans les tags ni par
/// `lofty`, mais dans le cookie magique du fichier. Tous les autres conteneurs
/// renseignent la leur au scan, donc les sonder serait de l'E/S pour rien.
pub(super) fn conteneur_a_profondeur_cachee(fmt: Option<AudioFormat>) -> bool {
    matches!(fmt, Some(AudioFormat::Alac))
}

/// Profondeur réelle lue DANS le fichier, quand la base ne la connaît pas.
///
/// Rend `None` — donc « garder ce que dit la base » — dès que le conteneur
/// n'est pas concerné, que le fichier est illisible, ou que la sonde ne trouve
/// rien. Ne jamais échouer la lecture pour une profondeur : au pire on reste
/// sur le comportement d'avant.
pub(super) fn profondeur_sondee_si_la_base_ignore(
    file_path: &str,
    fmt: Option<AudioFormat>,
) -> Option<u16> {
    if !conteneur_a_profondeur_cachee(fmt) {
        return None;
    }
    let (_, bd) = crate::metadata::probe_m4a_props(std::path::Path::new(file_path))?;
    let bd = bd?;
    info!(
        path = %file_path,
        bit_depth = bd,
        "alac_bit_depth_probed_from_file_for_wav24"
    );
    Some(bd)
}

pub(crate) fn dop_requested(is_local: bool, is_network: bool, dsd_mode: &str) -> bool {
    (is_local && (dsd_mode == "native" || dsd_mode == "dop")) || (is_network && dsd_mode == "dop")
}

/// Cette piste est-elle du 1 bit (DSF/DFF) ? Le format vient de la base, tel
/// que le scan l'a écrit.
pub fn est_source_dsd(format: Option<&str>) -> bool {
    format.is_some_and(|f| matches!(f.to_ascii_lowercase().as_str(), "dsf" | "dff" | "dsd"))
}

/// Le fichier à décoder pour ré-alimenter les VU-mètres après une avance
/// gapless — `None` quand il n'y a rien à mesurer.
///
/// Le DSD en était exclu tout court (`file_path.filter(|_| !is_dsd)`), pour un
/// motif qui n'existe plus : `decode_to_pcm_streaming_with_levels` décode
/// DSF/DFF **en flux** depuis #1423, exactement comme le transcode de la
/// première lecture. L'exclusion laissait donc la zone SANS forwarder après
/// chaque enchaînement — `bump_levels_gen` venait de tuer le précédent — et
/// les aiguilles gelaient sur leur dernière valeur pour tout le reste de
/// l'album, alors que le FLAC de la même zone continuait de les animer
/// (#1541, Smart DX1 en `dsd_mode: pcm`).
///
/// Ce que le DSD garde en propre, c'est le PRIX : rendre du 1 bit en PCM coûte
/// cher, et sur le seul chemin qui ne mesure rien — OAAT en DSD natif, cf.
/// [`PlaybackOrchestrator::output_produces_levels`] — ce serait précisément le
/// décodage retiré pour débloquer Zicmu (`dsd_streaming_send_timeout`, #2280).
/// On ne le paie que si la sortie publie vraiment des niveaux ; les autres
/// formats gardent leur comportement, sans nouvelle condition.
pub(crate) fn fichier_a_mesurer_apres_avance(
    format: Option<&str>,
    file_path: Option<String>,
    la_sortie_mesure: bool,
) -> Option<String> {
    file_path.filter(|_| !est_source_dsd(format) || la_sortie_mesure)
}

/// Decode an infinite radio HTTP stream to PCM and send chunks through the
/// session channel.  Runs on a blocking thread (called via spawn_blocking).
///
/// Whether a prefetch buffer is too short to stand in for the whole track.
///
/// An UNKNOWN duration (`duration_ms == 0`) is treated as truncated. The 30s
/// prefetch mode buffers only the head of the track, and `duration_ms` is not
/// always populated for streaming queue items (Qobuz) — when it is 0 the old
/// `duration_ms > 0 && …` guard evaluated false and the partial buffer was
/// served to a DLNA renderer anyway. The renderer then stalls at the buffer's
/// end (Patricia Barber / Qobuz on an Eversolo DMP-A8: `bytes_sent=0`,
/// `peak_pos=30000`, zone force-stopped). Callers only consult this for network
/// outputs, so local gapless (which serves the buffer) is unaffected.
pub(super) fn prefetch_buffer_truncated(buffered_ms: u64, duration_ms: u64) -> bool {
    duration_ms == 0 || buffered_ms + 2000 < duration_ms
}

/// Uses symphonia with `ReadOnlySource` to handle the non-seekable HTTP stream.
/// Decodes packets progressively and converts to interleaved 16-bit PCM bytes.
/// The loop runs until the stream ends, the sender is dropped (stop), or an
/// unrecoverable error occurs.
/// Choose a renderer-safe WAV output sample rate for a decoded radio stream.
///
/// Most DLNA/UPnP renderers only lock onto 44.1/48 kHz (and higher standard
/// multiples). HE-AAC / aacPlus streams (Radio Morow: `morow_hi.aacp`) decode
/// at the AAC-LC core rate — typically 22050 Hz — because symphonia does not
/// apply the SBR extension that would double the rate to 44100. A 22050 Hz WAV
/// is reported as PLAYING yet emitted as SILENCE by many renderers (Yves,
/// LHC-60). We upsample any sub-44.1 kHz stream to 44100 Hz so sound is
/// guaranteed. Streams already at 44.1 kHz or above pass through unchanged, so
/// stations that already work incur no extra CPU and no quality change.
pub(crate) fn renderer_safe_wav_rate(source_rate: u32) -> u32 {
    if source_rate < 44_100 {
        44_100
    } else {
        source_rate
    }
}
