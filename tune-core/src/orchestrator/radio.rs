use super::*;

/// A public `play()` for the track ALREADY playing on a zone that arrives within
/// this window of the track's start is treated as a redundant controller
/// double-dispatch (a re-tap) and coalesced at the entry point — BEFORE any
/// Retard de départ délibéré sur une radio live, en secondes (#1628).
///
/// Trois secondes couvrent une frontière de segment entière (~8 s de cadence
/// observée, arrivées jusqu'à 100 ms en retard) tout en restant imperceptibles
/// au zapping. En dessous, la réserve se vide à la première irrégularité ;
/// au-dessus, on ferait attendre l'auditeur pour rien.
pub(super) const RADIO_PREBUFFER_SECS: u64 = 3;

/// Resuming a WEBRADIO after a pause longer than this is treated as a re-play
/// of the station (new upstream connection, new decode session, new stream URL
/// to the output) instead of resuming the paused pipeline. A radio stream is
/// LIVE: while the zone is paused its pipeline keeps ageing — the icecast
/// connection can die through debug-only exit paths, the output keeps
/// buffering an unbounded backlog, and OAAT packet timestamps fall behind the
/// endpoint clock by the whole pause — so a "resume" past a few seconds
/// renders silence with nothing in the logs (#1629, .42: 19 min pause → total
/// silence, volume changes ignored). Chosen ABOVE `DUPLICATE_NET_PLAY_WINDOW`
/// (12 s) so the re-play issued here can never be coalesced as a duplicate
/// net send; short pauses below the threshold keep today's working in-place
/// resume.
pub(super) const RADIO_RESUME_REPLAY_AFTER: std::time::Duration =
    std::time::Duration::from_secs(15);

/// Dire à l'auditeur pourquoi une station n'a pas joué.
///
/// Le décodage d'un flux radio tourne dans une tâche détachée : jusqu'ici son
/// échec ne laissait qu'un `warn!` dans les journaux. Côté interface la lecture
/// partait, la zone affichait la station, et il ne sortait rien — impossible de
/// distinguer « la station est morte » de « Tune est cassé » (issue #1960).
///
/// `fatal: true` est indispensable et non décoratif : le client web étouffe un
/// `zone.playback_error` reçu dans la fenêtre de grâce qui suit un ordre de
/// lecture (elle couvre les pré-transcodages HI-RES lents, #1146), SAUF s'il
/// est marqué fatal. Or une station morte échoue en moins d'une seconde,
/// c'est-à-dire en plein dans cette fenêtre : sans ce drapeau le message
/// afficherait « chargement… » puis plus rien du tout.
pub(super) fn emit_radio_playback_error(
    bus: &Option<Arc<EventBus>>,
    zone_id: i64,
    station: &str,
    error: &str,
) {
    let Some(bus) = bus else { return };
    if let Some(refus) = crate::audio::bitperfect_strict::RefusProfondeur::depuis_sentinelle(error)
    {
        bus.emit("zone.playback_error", refus.charge_utile(zone_id));
        return;
    }
    // Le flux répond, mais ce n'est pas de l'audio : on dit ce qui est arrivé
    // en clair plutôt que de recopier une erreur de décodeur.
    // #3973 — un refus « bit-perfect strict » porte son code et ses deux
    // fréquences : le client le traduit.
    if let Some(refus) = crate::audio::bitperfect_strict::RefusBitPerfect::depuis_sentinelle(error)
    {
        bus.emit(
            "zone.playback_error",
            crate::audio::bitperfect_strict::charge_utile_de_refus(zone_id, &refus),
        );
        return;
    }
    let message = if error.starts_with(RADIO_NOT_AUDIO) {
        format!(
            "« {station} » n'émet plus d'audio : le serveur renvoie une page web à la place du flux. La station a probablement changé d'adresse."
        )
    } else if let Some(raison) = error.strip_prefix(RADIO_HLS_UNSUPPORTED) {
        // Nommer HLS, et dire quoi faire. Avant #2307 l'auditeur recevait au
        // mieux le « radio probe failed: … » de symphonia — le nom d'un
        // sous-système qu'il n'a aucune raison de connaître, sur un défaut
        // qu'il ne peut pas corriger. Ici il apprend que sa station est bien
        // vivante, que c'est Tune qui ne sait pas la lire, et quoi demander.
        // #6357 — Tune lit le HLS ; ce refus ne vise plus que ce qu'il ne lit
        // pas encore (fMP4, DRM, AC-3…), et la raison est dite.
        let raison = raison.trim_start_matches(':').trim();
        format!(
            "« {station} » est diffusée en HLS (liste .m3u8) sous une forme que Tune ne lit pas encore ({raison}). Demandez à la station son adresse de flux directe (MP3 ou AAC), ou choisissez une autre station."
        )
    } else if error.starts_with(RADIO_HLS_INTERDIT) {
        format!(
            "« {station} » refuse l'accès à son flux depuis cette connexion (souvent une restriction géographique du diffuseur). Choisissez une autre adresse de la station, ou une autre station."
        )
    } else {
        format!("Impossible de lire la station « {station} » : {error}")
    };
    bus.emit(
        "zone.playback_error",
        serde_json::json!({
            "zone_id": zone_id,
            "error": message,
            "fatal": true,
        }),
    );
}

/// Préfixe des erreurs « le flux annoncé audio n'en est pas ». Il permet à
/// l'appelant de distinguer ce cas d'une panne réseau : une station remplacée
/// par une page web ne guérira pas en réessayant.
pub(crate) const RADIO_NOT_AUDIO: &str = "radio_not_audio";

/// Le `Content-Type` d'un flux radio dit-il, sans ambiguïté, que ce n'est PAS
/// de l'audio ?
///
/// Le cas qui motive ce contrôle (issue #1960) : BBC Radio 3 a retiré son flux,
/// `stream.live.vc.bbcmedia.co.uk/bbc_radio_three` redirige vers
/// `www.bbc.co.uk` et répond **200 OK** en `text/html`. Rien n'échoue — le
/// décodeur reçoit du HTML, ne trouve pas de piste audio, et l'auditeur n'a que
/// du silence sans le moindre message. Un 404 se voit ; un 200 en HTML, non.
///
/// Volontairement une LISTE NOIRE, pas une liste blanche : les serveurs
/// Icecast/Shoutcast annoncent tout et n'importe quoi (`application/octet-stream`,
/// `application/ogg`, `audio/aacp`, parfois rien du tout), et refuser un flux
/// sur un type inconnu ferait taire des stations qui marchent. On ne rejette
/// donc que ce qui ne peut en aucun cas être un flux audio.
///
/// Renvoie `Some(étiquette)` — le type normalisé, à afficher — quand le flux
/// n'est pas de l'audio ; `None` dans tous les autres cas, y compris un
/// en-tête absent ou illisible.
///
/// DEUX appelants, depuis #3578, et c'est le second qui manquait :
///
///   * la LECTURE, ici même (`decode_radio_stream_to_pcm`) — le verdict arrive
///     quand l'auditeur a déjà cliqué ;
///   * l'ENREGISTREMENT, dans `tune-server` (`routes::radios::sonder_le_flux`,
///     appelée par `create_radio` et `update_radio`) — Belkadi Yacine avait
///     collé `https://radioparadise.com/listen/channels/main-mix`, la page
///     d'écoute et non le flux ; Tune l'a acceptée sans un mot et ne le lui a
///     appris qu'à la première lecture (fil forum 1698).
///
/// `pub` pour ce second appelant : cette fonction est le juge, et les deux
/// chemins doivent rendre le MÊME verdict — deux listes noires qui divergent
/// feraient refuser à l'enregistrement une station qui joue, ou l'inverse.
pub fn non_audio_content_type(content_type: &str) -> Option<String> {
    // `text/html; charset=UTF-8` → `text/html`
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if ct.is_empty() {
        return None;
    }
    const NEVER_AUDIO: [&str; 5] = [
        "text/html",
        "application/xhtml+xml",
        "text/css",
        "application/json",
        "image/",
    ];
    // Une entrée terminée par `/` est un préfixe de famille (`image/`), les
    // autres sont des types exacts.
    NEVER_AUDIO
        .iter()
        .any(|bad| {
            if bad.ends_with('/') {
                ct.starts_with(bad)
            } else {
                ct == *bad
            }
        })
        .then_some(ct)
}

/// Préfixe des erreurs « cette station est diffusée en HLS » (#2307).
///
/// Distinct de [`RADIO_NOT_AUDIO`] parce que le remède n'est pas le même : une
/// station en `text/html` est morte ou a changé d'adresse, une station en HLS
/// est bien vivante et c'est Tune qui ne sait pas la lire. Confondre les deux
/// enverrait l'auditeur chercher une adresse de remplacement qui n'existe pas.
///
/// #6357 — Tune lit désormais le HLS (`crate::radio_hls`). Ce préfixe ne
/// nomme plus que ce qu'il ne lit pas : segments fMP4/CMAF, plages d'octets,
/// DRM, AC-3/E-AC-3, LATM, liste sans variante AAC ou MP3.
pub(crate) const RADIO_HLS_UNSUPPORTED: &str = "radio_hls_unsupported";

/// #6357 — préfixe des erreurs « le diffuseur refuse l'accès » (403, 451 sur
/// une liste ou un segment HLS) : la BBC sert sa variante `uk` en 403 hors du
/// Royaume-Uni. Définitif comme [`RADIO_NOT_AUDIO`] : trente reconnexions ne
/// lèveront pas une restriction géographique. Aucun contournement n'est tenté.
pub(crate) const RADIO_HLS_INTERDIT: &str = "radio_flux_interdit";

/// Un échec qui ne guérira pas en réessayant : page web, HLS illisible,
/// accès refusé. La boucle de reconnexion le rend tout de suite, et la
/// relance automatique du sondeur se désarme (#3756).
pub(crate) fn echec_radio_definitif(erreur: &str) -> bool {
    erreur.starts_with(RADIO_NOT_AUDIO)
        || erreur.starts_with(RADIO_HLS_UNSUPPORTED)
        || erreur.starts_with(RADIO_HLS_INTERDIT)
}

/// Cette station est-elle publiée en HLS ?
///
/// HLS n'est pas un format de conteneur qu'il suffirait d'ajouter au décodeur :
/// c'est un PROTOCOLE. Un `.m3u8` est un manifeste qui liste des segments à
/// télécharger l'un après l'autre, et qu'il faut re-télécharger périodiquement
/// tant que le direct dure. Depuis #6357, une station reconnue ici passe par
/// le chargeur de `crate::radio_hls` au lieu du GET unique des flux Icecast.
///
/// Deux signaux, tous deux SANS AMBIGUÏTÉ — c'est délibérément étroit, parce
/// qu'un faux positif rendrait muette une station qui marche aujourd'hui :
///
///   * l'extension `.m3u8` du chemin, paramètres et ancre retirés ;
///   * le type MIME `application/vnd.apple.mpegurl`, le type ENREGISTRÉ de HLS
///     (RFC 8216) — le seul cas où une URL sans extension se dénonce.
///
/// Volontairement ABSENTS : `audio/x-mpegurl`, `audio/mpegurl` et
/// `application/x-mpegurl`, que les serveurs servent aussi pour une simple
/// playlist `.m3u`. Une `.m3u` est déréférencée en amont par
/// `resolve_playlist_url` ; quand ce déréférencement échoue (réseau), le
/// décodeur la reçoit telle quelle — et l'annoncer « HLS » serait un
/// diagnostic FAUX sur le chemin le plus fréquenté. Mieux vaut se taire sur
/// ces types-là que mentir.
///
/// Ce contrôle vit à part de [`non_audio_content_type`] et ne le modifie pas :
/// cette liste noire répond à une autre question (« le serveur a-t-il rendu une
/// page web ? ») et son témoin exige justement que les types `mpegurl` la
/// traversent. Les deux gardes sont indépendantes.
///
/// `content_type` peut être vide : c'est le mode « avant le réseau », où seule
/// l'extension parle.
pub(crate) fn is_hls_manifest(url: &str, content_type: &str) -> bool {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    if path.ends_with(".m3u8") {
        return true;
    }
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("application/vnd.apple.mpegurl")
}

/// #5716 — le codec reconnu par la sonde de décodage est retenu dans
/// `radio_stations`, pour la station d'adresse `station_url` (le `source_id`
/// d'une lecture radio EST l'adresse de la station). Les stations livrées avec
/// Tune ont `codec = NULL` : sans cette écriture, aucun écran ne pouvait dire si
/// une station est en AAC ou en MP3. Un codec déjà connu n'est jamais remplacé.
pub(crate) fn retenir_le_codec_de_la_station(
    db: &Arc<dyn crate::db::backend::DbBackend>,
    session: &crate::http::streamer::StreamSession,
    station_url: Option<&str>,
) {
    let Some(station_url) = station_url.filter(|u| !u.is_empty()) else {
        return;
    };
    let db = db.clone();
    let station_url = station_url.to_string();
    session.au_premier_codec_radio(Box::new(move |codec| {
        let repo = crate::db::radio_repo::RadioRepo::with_backend(db);
        match repo.remplir_le_codec_observe(&station_url, codec) {
            Ok(0) => {}
            Ok(_) => tracing::info!(url = %station_url, codec, "radio_codec_observe_retenu"),
            Err(error) => {
                tracing::warn!(url = %station_url, codec, %error, "radio_codec_observe_non_retenu")
            }
        }
    }));
}

/// Applique l'EQ au PCM radio déjà décodé, avant sa quantification au format de sortie.
/// `None` est une identité stricte : les chemins sans EQ conservent exactement
/// les mêmes échantillons et ne paient aucun traitement supplémentaire.
pub(super) fn apply_radio_eq(
    eq: &mut Option<crate::audio::eq::EqProcessor>,
    interleaved: &mut [f32],
) {
    if let Some(eq) = eq.as_mut() {
        eq.process_interleaved(interleaved);
    }
}

// Radio streams from Radio France (FIP, etc.) periodically drop the upstream
// HTTP body (`request or response body error`) — Xavier's ~1h30 cutoffs.
// The old code ended the decode on such an error, tearing down the WAV
// session and relying on the poller auto-retry (~1min40 of silence). Instead
// we reconnect the upstream in place and keep feeding the SAME session, so
// the renderer never stops (a sub-second gap at worst). We give up only after
// MAX_RECONNECTS so a permanently-dead station still falls back to the poller.
const MAX_RECONNECTS: u32 = 30;

/// L'état porté d'une connexion à la suivante par la boucle `'reconnect` de
/// [`decode_radio_stream_to_pcm`] : les `let mut` d'avant la boucle, ni plus
/// ni moins. Chaque temps le reçoit par `&mut` et n'écrit que ce que le texte
/// d'origine écrivait au même endroit.
struct EtatRadio {
    first_chunk_sent: bool,
    pcm_buf: Vec<u8>,
    chunk_size: usize,
    reconnects: u32,
    // When the upstream last dropped, so we can measure how long the renderer
    // was starved during a reconnect (diagnostics: FIP silent-after-reconnect).
    dropped_at: Option<std::time::Instant>,
    // Format of the first successful connection. A reconnect that returns a
    // different rate/channel layout would feed PCM that doesn't match the WAV
    // header already sent to the renderer, so we bail to a fresh session instead.
    expected_format: Option<(u16, u32, u16)>,
    // Construit une seule fois au format réellement détecté, puis conservé à
    // travers les reconnexions compatibles : réinitialiser les biquads à chaque
    // coupure amont créerait un transitoire audible (#2063).
    radio_eq: Option<crate::audio::eq::EqProcessor>,
    // #4407 — le profil de départ n'est posé qu'UNE fois : après une relève
    // en vol qui a coupé l'égaliseur, une reconnexion ne doit pas remonter
    // l'ancien.
    eq_de_depart_pose: bool,
}

/// Les canaux et le contexte du décodeur, relevés une fois par
/// [`decode_radio_stream_to_pcm`] et lus par chaque temps.
struct CanauxRadio<'a> {
    tx: &'a tokio::sync::mpsc::Sender<Vec<u8>>,
    data_ready: &'a std::sync::Arc<tokio::sync::Notify>,
    session: &'a std::sync::Arc<crate::http::streamer::StreamSession>,
    eq_profile: &'a Option<crate::audio::eq::EqProfile>,
    /// #4407 — le poste où la route dépose un nouvel égaliseur, relevé au
    /// paquet suivant, sans refaire la session.
    en_vol: &'a Option<std::sync::Arc<super::eq_en_vol::EqEnVol>>,
    levels_tx: &'a Option<tokio::sync::mpsc::UnboundedSender<crate::audio::tap::RawWindow>>,
    rt: &'a tokio::runtime::Handle,
    /// #3973 — « bit-perfect strict » de la zone : une cadence que le décodeur
    /// devrait relever (`renderer_safe_wav_rate`) est refusée, pas convertie.
    strict_bitperfect: bool,
    sortie_locale: bool,
}

/// Ce que rend une connexion sondée : le lecteur de conteneur, le décodeur
/// et la piste audio retenue.
struct Sonde {
    format: Box<dyn symphonia::core::formats::FormatReader>,
    decoder: radio_ogg::DecodeurRadio,
    track_id: u32,
    source_channels: u16,
    source_sample_rate: u32,
    source_info: crate::http::streamer::RadioSourceInfo,
    /// #6357 — le PCM de la trame que la sonde HLS a déjà décodée pour
    /// connaître la vraie fréquence (HE-AAC) : servi avant le paquet suivant.
    pcm_en_attente: Option<(Vec<f32>, usize)>,
}

/// Le format de sortie décidé pour une connexion (voir `renderer_safe_wav_rate`).
struct SortieRadio {
    output_sample_rate: u32,
    output_bit_depth: u16,
    needs_resample: bool,
}

/// La suite que décide UNE connexion de la boucle `'reconnect`.
enum SuiteRadio {
    /// `continue 'reconnect` : la connexion a échoué ou l'amont a coupé, on
    /// retente sur la MÊME session.
    Reconnecter,
    /// Chacun des `return` du texte d'origine, avec sa valeur : `Err` quand
    /// la première connexion échoue ou que la station est une page web ou un
    /// manifeste HLS ; `Ok(())` quand le consommateur est parti, que le
    /// format a changé ou que `MAX_RECONNECTS` est dépassé.
    Rendre(Result<(), String>),
}

/// La suite que décide UN paquet de la boucle de décodage.
enum SuitePaquet {
    /// Fin naturelle de l'itération : le paquet est décodé et servi.
    PaquetServi,
    /// `continue` : le paquet appartient à une autre piste.
    AutrePiste,
    /// `continue` : trame indécodable, sautée.
    TrameSautee,
    /// `continue` : Ogg chaîné, un nouveau flux logique (le titre suivant)
    /// commence ; le décodeur est reconstruit sans quitter la connexion.
    FluxLogiqueSuivant,
    /// `break` : l'amont a terminé son flux (`Ok(None)`) — on se reconnecte.
    AmontTermine,
    /// `break` : l'amont a coupé (fin de fichier inattendue) — on se reconnecte.
    AmontCoupe,
    /// `break` : erreur de lecture d'un paquet (corps FIP) — on se reconnecte.
    ErreurDePaquet,
    /// `return Ok(())` : le canal est fermé ou l'envoi refusé, le
    /// consommateur est parti.
    ConsommateurParti,
}

pub(super) fn decode_radio_stream_to_pcm(
    url: String,
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    data_ready: std::sync::Arc<tokio::sync::Notify>,
    session: std::sync::Arc<crate::http::streamer::StreamSession>,
    // Le profil voyage sans coefficients : le taux/canaux réels ne sont connus
    // qu'après la sonde Symphonia. `None` garde le PCM historique à l'identique
    // (notamment la sortie locale, qui applique déjà son propre EQ).
    eq_profile: Option<crate::audio::eq::EqProfile>,
    // Pur observateur : les VU-mètres. Un flux radio est décodé live (pas de
    // fichier), donc le décodage-pour-niveaux des pistes locales ne s'applique
    // pas — on tappe ici le PCM déjà décodé. `None` = pas de bus (tests).
    levels_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::audio::tap::RawWindow>>,
    // #3973 — « bit-perfect strict » de la zone qui écoute (faux hors zone :
    // serveur de médias).
    strict_bitperfect: bool,
    // #4407 — le poste de relève de l'égaliseur de CE flux. `None` : le
    // profil de départ reste celui du flux entier (sortie locale, Bandcamp).
    en_vol: Option<std::sync::Arc<super::eq_en_vol::EqEnVol>>,
) -> Result<(), String> {
    decode_radio_stream_to_pcm_avec_profondeur(
        url,
        tx,
        data_ready,
        session,
        eq_profile,
        levels_tx,
        strict_bitperfect,
        en_vol,
        false,
    )
}

/// Seule la sortie locale relève le PCM à 24 bits quand la source le demande.
/// Les autres appelants conservent leur contrat réseau 16 bits (#1654).
#[allow(clippy::too_many_arguments)]
pub(super) fn decode_radio_stream_to_pcm_avec_profondeur(
    url: String,
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    data_ready: std::sync::Arc<tokio::sync::Notify>,
    session: std::sync::Arc<crate::http::streamer::StreamSession>,
    eq_profile: Option<crate::audio::eq::EqProfile>,
    levels_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::audio::tap::RawWindow>>,
    strict_bitperfect: bool,
    en_vol: Option<std::sync::Arc<super::eq_en_vol::EqEnVol>>,
    sortie_locale: bool,
) -> Result<(), String> {
    // #6357 — le HLS ne s'arrête plus ici (#2307 le refusait avant le
    // moindre octet de réseau) : `sonder_la_station` le reconnaît et passe
    // par le chargeur de segments. Le reste du chemin — format de sortie,
    // volume, EQ, reconnexions — est celui de toutes les radios.
    let rt =
        tokio::runtime::Handle::try_current().map_err(|_| "no tokio runtime for radio decode")?;

    let mut etat = EtatRadio {
        first_chunk_sent: false,
        pcm_buf: Vec::with_capacity(65536),
        chunk_size: 32768,
        reconnects: 0,
        dropped_at: None,
        expected_format: None,
        radio_eq: None,
        eq_de_depart_pose: false,
    };
    let canaux = CanauxRadio {
        tx: &tx,
        data_ready: &data_ready,
        session: &session,
        eq_profile: &eq_profile,
        en_vol: &en_vol,
        levels_tx: &levels_tx,
        rt: &rt,
        strict_bitperfect,
        sortie_locale,
    };

    'reconnect: loop {
        match une_connexion(&mut etat, &url, &canaux) {
            SuiteRadio::Reconnecter => continue 'reconnect,
            SuiteRadio::Rendre(resultat) => return resultat,
        }
    }
}

/// Le corps d'UNE itération de `'reconnect` : connexion et sonde, format de
/// sortie, boucle de décodage, reprise après coupure. Chaque sortie de
/// boucle du texte d'origine est une variante de [`SuiteRadio`] ou de
/// [`SuitePaquet`] ; aucune condition n'a changé.
fn une_connexion(etat: &mut EtatRadio, url: &str, canaux: &CanauxRadio<'_>) -> SuiteRadio {
    // ---- Connect + probe + build decoder ----
    let mut sonde = match se_connecter_et_sonder(etat, url) {
        Ok(sonde) => sonde,
        Err(suite) => return suite,
    };

    let sortie = match preparer_la_sortie(etat, url, canaux, &sonde) {
        Ok(sortie) => sortie,
        Err(suite) => return suite,
    };

    // When this connection started streaming. A healthy station streams for
    // minutes between periodic upstream drops; only a permanently-dead
    // station fails in rapid succession. Used below to reset the reconnect
    // counter after a good stretch (see the drop handler).
    let connected_at = std::time::Instant::now();

    // ---- Decode loop ----
    loop {
        match decoder_un_paquet(etat, canaux, &mut sonde, &sortie) {
            SuitePaquet::PaquetServi
            | SuitePaquet::AutrePiste
            | SuitePaquet::TrameSautee
            | SuitePaquet::FluxLogiqueSuivant => {
                continue;
            }
            SuitePaquet::AmontTermine | SuitePaquet::AmontCoupe | SuitePaquet::ErreurDePaquet => {
                break;
            }
            SuitePaquet::ConsommateurParti => return SuiteRadio::Rendre(Ok(())),
        }
    }

    reprendre_apres_coupure(etat, url, canaux, connected_at)
}

/// Premier temps : la station est sondée ; sur échec, la décision de
/// réessayer ou de rendre est celle du texte d'origine (échec initial,
/// page web ou HLS, plafond de reconnexions).
fn se_connecter_et_sonder(etat: &mut EtatRadio, url: &str) -> Result<Sonde, SuiteRadio> {
    use tracing::warn;

    let setup = sonder_la_station(url);

    match setup {
        Ok(v) => Ok(v),
        Err(e) => {
            if etat.reconnects == 0 {
                // Initial connection failed — fail fast (bad URL, etc.)
                return Err(SuiteRadio::Rendre(Err(e)));
            }
            // Une station remplacée par une page web ne redeviendra pas un
            // flux audio en réessayant trente fois : on remonte l'erreur
            // tout de suite pour qu'elle soit DITE, au lieu de quinze
            // secondes de silence suivies d'un abandon muet.
            // Ni un HLS illisible (#2307), ni un accès refusé (#6357).
            if echec_radio_definitif(&e) {
                return Err(SuiteRadio::Rendre(Err(e)));
            }
            etat.reconnects += 1;
            if etat.reconnects > MAX_RECONNECTS {
                warn!(url = %url, error = %e, "radio_reconnect_giving_up");
                return Err(SuiteRadio::Rendre(Ok(())));
            }
            warn!(url = %url, error = %e, attempt = etat.reconnects, "radio_reconnect_setup_failed_retrying");
            std::thread::sleep(std::time::Duration::from_millis(500));
            Err(SuiteRadio::Reconnecter)
        }
    }
}

/// Le GET, les contrôles de type, la sonde Symphonia et le décodeur : le
/// texte de la fermeture `setup` d'origine, tel quel.
fn sonder_la_station(url: &str) -> Result<Sonde, String> {
    use std::io::Read;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::io::{MediaSourceStream, ReadOnlySource};
    use tracing::info;

    // #6357 — une liste `.m3u8` passe par le chargeur HLS, avant tout GET
    // de flux continu.
    if is_hls_manifest(url, "") {
        return sonder_hls(url, None);
    }

    // No total timeout for infinite radio streams
    let response = crate::http::client::blocking_builder()
        .timeout(None)
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .and_then(|c| c.get(url).send())
        .map_err(|e| format!("radio HTTP fetch failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("radio HTTP error: {}", response.status()));
    }
    // Le type réellement reçu, tracé à CHAQUE connexion : c'est la
    // seule façon de savoir, la prochaine fois qu'une station meurt,
    // ce que son serveur a répondu (issue #1960).
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    // Une station peut disparaître en répondant 200 : la BBC redirige
    // son ancien flux vers sa page d'accueil. Sans ce contrôle, le
    // décodeur avale du HTML, échoue plus loin sur un message obscur
    // (« no audio track found ») et l'auditeur n'a que du silence.
    if let Some(bad) = non_audio_content_type(&content_type) {
        return Err(format!(
            "{RADIO_NOT_AUDIO}: le serveur a répondu « {bad} » au lieu d'un flux audio"
        ));
    }
    info!(url = %url, content_type = %content_type, "radio_local_decode_stream_connected");

    // #6357 — une URL sans extension peut servir une liste HLS : sous son
    // type enregistré, ou sous un type `mpegurl` que les serveurs donnent
    // aussi aux simples `.m3u`. On regarde les premiers octets : `#EXTM3U`
    // suivi de balises HLS, c'est une liste à charger ; sinon les octets lus
    // sont rendus tels quels au décodeur, et rien ne change pour un flux
    // audio mal étiqueté.
    let mut response = response;
    let mut tete = Vec::new();
    if is_hls_manifest(url, &content_type) || type_mpegurl(&content_type) {
        let finale = response.url().to_string();
        (&mut response)
            .take(64 * 1024)
            .read_to_end(&mut tete)
            .map_err(|e| format!("radio HTTP fetch failed: {e}"))?;
        let texte = String::from_utf8_lossy(&tete);
        if texte.trim_start_matches('\u{feff}').starts_with("#EXTM3U")
            && (texte.contains("#EXT-X-TARGETDURATION") || texte.contains("#EXT-X-STREAM-INF"))
        {
            let mut texte = texte.into_owned();
            // Une liste plus longue que la tête lue : on lit le reste, borné.
            if tete.len() == 64 * 1024 {
                let mut reste = String::new();
                let _ = response.take(2 * 1024 * 1024).read_to_string(&mut reste);
                texte.push_str(&reste);
            }
            return sonder_hls(url, Some((finale, texte)));
        }
    }

    let source = ReadOnlySource::new(std::io::Cursor::new(tete).chain(response));
    let mss = MediaSourceStream::new(Box::new(source), Default::default());

    let mut hint = Hint::new();
    let lower = url.to_lowercase();
    let path_part = lower.split('?').next().unwrap_or(&lower);
    if path_part.ends_with(".mp3") {
        hint.with_extension("mp3");
    } else if path_part.ends_with(".aac") || path_part.ends_with(".m4a") {
        hint.with_extension("aac");
    } else if path_part.ends_with(".ogg")
        || path_part.ends_with(".oga")
        || path_part.ends_with(".opus")
        || ogg_par_le_type(&content_type)
    {
        // Un montage Icecast Ogg n'a pas toujours d'extension (`/live`,
        // `/vorbis`) : son type MIME le dit (`application/ogg`,
        // `audio/ogg; codecs=opus`, `audio/opus`).
        hint.with_extension("ogg");
    } else if path_part.ends_with(".flac") {
        hint.with_extension("flac");
    } else {
        hint.with_extension("mp3");
    }

    let format = sonder_le_conteneur(&hint, mss)?;
    let (track_id, audio_params) = piste_audio(&*format)?;
    let source_channels = audio_params
        .channels
        .as_ref()
        .map(|c| c.count() as u16)
        .unwrap_or(2);
    let source_sample_rate = audio_params.sample_rate.unwrap_or(44100);

    // Opus : libopus, symphonia ne le décode pas (voir `radio_ogg`).
    let decoder = radio_ogg::DecodeurRadio::pour(&audio_params)?;
    let source_info = source_info_de(&audio_params);

    Ok(Sonde {
        source_info,
        format,
        decoder,
        track_id,
        source_channels,
        source_sample_rate,
        pcm_en_attente: None,
    })
}

/// Les types que les serveurs donnent aux listes `.m3u` comme aux `.m3u8`.
fn type_mpegurl(content_type: &str) -> bool {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(
        essence.as_str(),
        "application/x-mpegurl" | "audio/x-mpegurl" | "audio/mpegurl"
    )
}

fn sonder_le_conteneur(
    hint: &symphonia::core::formats::probe::Hint,
    mss: symphonia::core::io::MediaSourceStream<'static>,
) -> Result<Box<dyn symphonia::core::formats::FormatReader>, String> {
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::meta::MetadataOptions;
    symphonia::default::get_probe()
        .probe(
            hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| format!("radio probe failed: {e}"))
}

/// La piste audio par défaut et ses paramètres.
fn piste_audio(
    format: &dyn symphonia::core::formats::FormatReader,
) -> Result<(u32, symphonia::core::codecs::audio::AudioCodecParameters), String> {
    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::formats::TrackType;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or("radio stream: no audio track found")?;
    let params = match &track.codec_params {
        Some(CodecParameters::Audio(params)) => params.clone(),
        _ => return Err("radio stream: no audio codec parameters".into()),
    };
    Ok((track.id, params))
}

/// Ce que la sonde dit de la source, pour le chemin du signal.
fn source_info_de(
    audio_params: &symphonia::core::codecs::audio::AudioCodecParameters,
) -> crate::http::streamer::RadioSourceInfo {
    use symphonia::core::codecs::audio::well_known::*;
    let source_format = match audio_params.codec {
        CODEC_ID_MP3 => Some("mp3"),
        CODEC_ID_AAC => Some("aac"),
        CODEC_ID_FLAC => Some("flac"),
        CODEC_ID_ALAC => Some("alac"),
        CODEC_ID_VORBIS => Some("ogg"),
        CODEC_ID_OPUS => Some("opus"),
        _ => None,
    };
    crate::http::streamer::RadioSourceInfo {
        channels: audio_params
            .channels
            .as_ref()
            .and_then(|c| u16::try_from(c.count()).ok())
            .filter(|n| *n > 0),
        format: source_format,
        sample_rate: audio_params.sample_rate.filter(|rate| *rate > 0),
        // Lossy codecs do not have a source PCM bit depth. The 16-bit WAV
        // quantization belongs to the output, not to the MP3/AAC station.
        bit_depth: matches!(audio_params.codec, CODEC_ID_FLAC | CODEC_ID_ALAC)
            .then_some(audio_params.bits_per_sample)
            .flatten()
            .and_then(|bits| u16::try_from(bits).ok())
            .filter(|bits| *bits > 0),
        ..Default::default()
    }
}

/// Combien de paquets la sonde HLS lit au plus pour obtenir une première
/// trame décodée (une trame vide ou ratée en tête de flux n'est pas rare).
const PAQUETS_DE_SONDE_HLS: usize = 16;

/// #6357 — une station HLS : la source `radio_hls` (liste, variante,
/// segments, démultiplexage) rend un flux ADTS ou MP3 continu, poussé dans
/// symphonia comme le corps d'une station Icecast.
///
/// L'AAC est décodé par `rusty_aac`, qui reconstruit le SBR et le PS. Le
/// piège (étude #6363 §4.4) : en ADTS, un HE-AAC se signale IMPLICITEMENT —
/// l'en-tête dit « AAC-LC à 24 kHz », la vraie sortie est à 48 kHz. Le
/// format de sortie étant décidé avant la boucle de décodage
/// (`preparer_la_sortie`), la première trame est décodée ICI, et c'est sa
/// fréquence et ses canaux qui font foi ; son PCM est gardé pour être servi
/// en premier. Sans cela, l'en-tête WAV annoncerait 24 kHz sur du PCM à
/// 48 kHz : la musique jouerait à demi-vitesse.
fn sonder_hls(url: &str, deja_lue: Option<(String, String)>) -> Result<Sonde, String> {
    use symphonia::core::codecs::audio::well_known::CODEC_ID_AAC;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::io::{MediaSourceStream, ReadOnlySource};
    use tracing::{info, warn};

    let source =
        crate::radio_hls::HlsSource::ouvrir(url, deja_lue, crate::radio_hls::Reglages::default())?;
    let infos = source.infos.clone();
    let mss = MediaSourceStream::new(Box::new(ReadOnlySource::new(source)), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(match infos.charge {
        crate::radio_hls::Charge::Adts => "aac",
        crate::radio_hls::Charge::Mp3 => "mp3",
    });
    let mut format = sonder_le_conteneur(&hint, mss)?;
    let (track_id, audio_params) = piste_audio(&*format)?;
    let mut source_info = source_info_de(&audio_params);
    let coeur_cadence = audio_params.sample_rate.unwrap_or(44100);
    let coeur_canaux = audio_params
        .channels
        .as_ref()
        .map(|c| c.count() as u16)
        .unwrap_or(2);

    let mut decoder = None;
    let mut pcm_en_attente = None;
    let (mut source_sample_rate, mut source_channels) = (coeur_cadence, coeur_canaux);
    if audio_params.codec == CODEC_ID_AAC {
        match radio_ogg::DecodeurRadio::aac_pur_rust(&audio_params)
            .and_then(|mut d| premiere_trame(&mut *format, track_id, &mut d).map(|pcm| (d, pcm)))
        {
            Ok((d, (pcm, canaux))) => {
                source_sample_rate = d.cadence_decodee().unwrap_or(coeur_cadence);
                source_channels = u16::try_from(canaux).unwrap_or(coeur_canaux);
                pcm_en_attente = Some((pcm, canaux));
                decoder = Some(d);
            }
            Err(e) => {
                // Repli : symphonia décode au moins le cœur AAC-LC. Moins
                // bien (sans SBR), mais du son plutôt que rien.
                warn!(url = %url, error = %e, "hls_aac_repli_symphonia");
            }
        }
    }
    let decoder = match decoder {
        Some(d) => d,
        None => radio_ogg::DecodeurRadio::pour(&audio_params)?,
    };
    let sbr = source_sample_rate > coeur_cadence;
    let v2 = infos
        .codecs
        .as_deref()
        .is_some_and(|c| c.to_ascii_lowercase().contains("mp4a.40.29"));
    source_info.transport = Some("hls");
    source_info.profil = match (audio_params.codec == CODEC_ID_AAC, sbr, v2) {
        (false, _, _) => None,
        (true, true, true) => Some("HE-AACv2"),
        (true, true, false) => Some("HE-AAC"),
        (true, false, _) if pcm_en_attente.is_some() => Some("AAC-LC"),
        // Le repli symphonia ne sait pas dire si le flux porte du SBR.
        (true, false, _) => None,
    };
    source_info.debit_kbps = infos.debit_kbps;
    source_info.sample_rate = Some(source_sample_rate);
    source_info.channels = Some(source_channels).filter(|n| *n > 0);
    info!(
        url = %url,
        profil = ?source_info.profil,
        coeur_hz = coeur_cadence,
        sortie_hz = source_sample_rate,
        canaux = source_channels,
        debit_kbps = ?infos.debit_kbps,
        "radio_hls_sonde"
    );

    Ok(Sonde {
        source_info,
        format,
        decoder,
        track_id,
        source_channels,
        source_sample_rate,
        pcm_en_attente,
    })
}

/// Décode des paquets jusqu'à la première trame non vide : son PCM et ses
/// canaux. Le décodeur garde, dans `cadence_decodee`, la vraie fréquence.
fn premiere_trame(
    format: &mut dyn symphonia::core::formats::FormatReader,
    track_id: u32,
    decodeur: &mut radio_ogg::DecodeurRadio,
) -> Result<(Vec<f32>, usize), String> {
    let mut pcm = Vec::new();
    let mut derniere_erreur = String::from("aucune trame décodée");
    for _ in 0..PAQUETS_DE_SONDE_HLS {
        let paquet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => return Err("flux terminé avant la première trame".into()),
            Err(e) => return Err(format!("lecture du premier paquet : {e}")),
        };
        if paquet.track_id != track_id {
            continue;
        }
        match decodeur.decoder(&paquet, &mut pcm) {
            Ok(canaux) if !pcm.is_empty() => return Ok((pcm, canaux)),
            Ok(_) => {}
            Err(e) => derniere_erreur = e,
        }
    }
    Err(derniere_erreur)
}

/// Deuxième temps : le format attendu est vérifié, le taux de sortie
/// décidé, l'EQ construit une seule fois, le format publié à la session, et
/// la coupure mesurée.
fn preparer_la_sortie(
    etat: &mut EtatRadio,
    url: &str,
    canaux: &CanauxRadio<'_>,
    sonde: &Sonde,
) -> Result<SortieRadio, SuiteRadio> {
    use tracing::{info, warn};

    let source_channels = sonde.source_channels;
    let source_sample_rate = sonde.source_sample_rate;
    let output_bit_depth =
        if canaux.sortie_locale && sonde.source_info.bit_depth.is_some_and(|bits| bits > 16) {
            24
        } else {
            16
        };

    // Guard against a reconnect changing the audio format underneath the
    // WAV header already advertised to the renderer.
    match etat.expected_format {
        None => {
            etat.expected_format = Some((source_channels, source_sample_rate, output_bit_depth))
        }
        Some((ch, sr, bd))
            if (ch, sr, bd) != (source_channels, source_sample_rate, output_bit_depth) =>
        {
            warn!(
                url = %url,
                expected_ch = ch, expected_sr = sr,
                got_ch = source_channels, got_sr = source_sample_rate,
                expected_bits = bd, got_bits = output_bit_depth,
                "radio_reconnect_format_changed_bailing"
            );
            return Err(SuiteRadio::Rendre(Ok(())));
        }
        _ => {}
    }

    // Renderer-safe output rate: HE-AAC/aacPlus decodes at its AAC-LC core
    // rate (e.g. 22050 Hz) which many DLNA renderers reject as silence. We
    // upsample sub-44.1 kHz streams to 44100 Hz; 44.1/48 kHz+ pass through.
    let output_sample_rate = renderer_safe_wav_rate(source_sample_rate);
    // #3973 — le site « décodage radio » de la règle bit-perfect : relever la
    // cadence EST une conversion. Strict ⇒ refuser, en le disant.
    if let Some(refus) = crate::audio::bitperfect_strict::decision_bitperfect(
        source_sample_rate,
        output_sample_rate,
        canaux.strict_bitperfect,
    )
    .refus()
    {
        warn!(
            url = %url,
            requested_sr = refus.demandee_hz,
            output_sr = refus.sortie_hz,
            "radio_bitperfect_strict_refused"
        );
        return Err(SuiteRadio::Rendre(Err(refus.sentinelle())));
    }
    let needs_resample = output_sample_rate != source_sample_rate;
    if let Some(refus) = crate::audio::bitperfect_strict::refus_reduction_profondeur(
        sonde.source_info.bit_depth,
        output_bit_depth,
        canaux.strict_bitperfect,
    ) {
        return Err(SuiteRadio::Rendre(Err(refus.sentinelle())));
    }
    // Un morceau se termine sur une trame entière : 32 768 n'est pas
    // divisible par les six octets d'une trame stéréo 24 bits.
    let frame_bytes = usize::from(source_channels) * usize::from(output_bit_depth / 8);
    etat.chunk_size = (32768 / frame_bytes).max(1) * frame_bytes;
    if !etat.eq_de_depart_pose {
        etat.eq_de_depart_pose = true;
        etat.radio_eq = canaux.eq_profile.as_ref().and_then(|profile| {
            let eq =
                crate::audio::eq::EqProcessor::new(profile, output_sample_rate, source_channels);
            if eq.is_enabled() { Some(eq) } else { None }
        });
    }

    canaux.session.publish_radio_source(sonde.source_info);

    // Publish the OUTPUT format so the HTTP handler advertises the WAV rate
    // that matches the PCM we actually feed (FIP is 48000 → advertised as
    // is; Morow HE-AAC is 22050 → advertised as the resampled 44100). Set
    // BEFORE first_chunk so the header, emitted after data_ready, is right.
    canaux.session.publish_detected_pcm_format(
        output_sample_rate,
        source_channels,
        output_bit_depth,
    );

    // Measure the reconnect gap: how long the session went without fresh
    // PCM. A long gap can starve the renderer's HTTP read.
    let gap_ms = etat.dropped_at.take().map(|t| t.elapsed().as_millis());
    info!(
        channels = source_channels,
        sample_rate = source_sample_rate,
        output_sample_rate = output_sample_rate,
        source_bit_depth = ?sonde.source_info.bit_depth,
        output_bit_depth,
        resampled = needs_resample,
        reconnect = etat.reconnects,
        gap_ms = ?gap_ms,
        "radio_local_decode_started"
    );
    if let Some(g) = gap_ms
        && g > 2000
    {
        warn!(
            gap_ms = g,
            reconnect = etat.reconnects,
            "radio_reconnect_gap_long — renderer may have been starved"
        );
    }

    Ok(SortieRadio {
        output_sample_rate,
        output_bit_depth,
        needs_resample,
    })
}

/// Troisième temps, le corps d'UNE itération de la boucle de décodage : un
/// paquet lu, décodé, rééchantillonné, égalisé, quantifié en PCM 16 ou 24 bits, puis servi
/// par morceaux de `chunk_size` octets. `pcm_buf` traverse les itérations et
/// les reconnexions.
fn decoder_un_paquet(
    etat: &mut EtatRadio,
    canaux: &CanauxRadio<'_>,
    sonde: &mut Sonde,
    sortie: &SortieRadio,
) -> SuitePaquet {
    use symphonia::core::audio::conv::IntoSample;
    use tracing::{debug, warn};

    if canaux.tx.is_closed() {
        debug!("radio_local_decode_channel_closed_before_packet");
        return SuitePaquet::ConsommateurParti;
    }
    // #6357 — la trame que la sonde HLS a déjà décodée passe en premier.
    let (mut interleaved, channels) = if let Some(en_attente) = sonde.pcm_en_attente.take() {
        en_attente
    } else {
        let packet = match sonde.format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => {
                debug!("radio_local_decode_stream_ended_upstream");
                return SuitePaquet::AmontTermine; // upstream ended — reconnect
            }
            Err(symphonia::core::errors::Error::IoError(ref e))
                if e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                debug!("radio_local_decode_eof");
                return SuitePaquet::AmontCoupe; // upstream dropped — reconnect
            }
            Err(symphonia::core::errors::Error::ResetRequired) => {
                // Ogg chaîné : le titre suivant ouvre un nouveau flux logique.
                // Avant, ce cas tombait dans la branche d'erreur ci-dessous et
                // coûtait un GET complet à chaque changement de titre.
                return match radio_ogg::piste_du_flux_suivant(
                    &*sonde.format,
                    sonde.source_channels,
                    sonde.source_sample_rate,
                ) {
                    Some((track_id, decoder)) => {
                        sonde.track_id = track_id;
                        sonde.decoder = decoder;
                        let titre = radio_ogg::titre_du_flux_logique(&mut *sonde.format);
                        tracing::info!(titre = ?titre, "radio_ogg_flux_logique_suivant");
                        SuitePaquet::FluxLogiqueSuivant
                    }
                    None => SuitePaquet::ErreurDePaquet,
                };
            }
            Err(e) => {
                // FIP-style upstream body error — reconnect in place.
                warn!(error = %e, "radio_local_decode_packet_error");
                return SuitePaquet::ErreurDePaquet;
            }
        };

        if packet.track_id != sonde.track_id {
            return SuitePaquet::AutrePiste;
        }

        // Decoded audio as interleaved samples before output quantization.
        let mut interleaved: Vec<f32> = Vec::new();
        let channels = match sonde.decoder.decoder(&packet, &mut interleaved) {
            Ok(c) => c,
            Err(e) => {
                debug!(error = %e, "radio_local_decode_frame_skip");
                return SuitePaquet::TrameSautee;
            }
        };
        // #6357 — la fréquence décodée ne doit pas quitter celle de l'en-tête
        // WAV déjà servi (un HE-AAC qui perdrait son SBR en route) : on se
        // reconnecte, et la garde `expected_format` décide.
        if let Some(cadence) = sonde.decoder.cadence_decodee()
            && cadence != sonde.source_sample_rate
        {
            warn!(
                attendu = sonde.source_sample_rate,
                recu = cadence,
                "radio_cadence_decodee_changee"
            );
            return SuitePaquet::ErreurDePaquet;
        }
        (interleaved, channels)
    };

    // Upsample low-rate (HE-AAC 22050) PCM to the renderer-safe rate
    // before packing the output PCM, so the bytes match the advertised WAV
    // header. No-op (single move) when the stream is already 44.1/48.
    if sortie.needs_resample {
        interleaved = crate::audio::simple_resample(
            &interleaved,
            sonde.source_sample_rate,
            sortie.output_sample_rate,
            channels as u16,
        );
    }

    // Le WAV servi à OAAT/DLNA/navigateur doit porter le son promis par
    // le profil de zone. Le traitement se fait en f32 avant quantification, comme
    // les autres chemins DSP, et les VU observent ainsi le signal final.
    // #4407 — un égaliseur déposé en vol prend la relève à CE paquet :
    // même session, même en-tête WAV, aucun nouveau `SetAVTransportURI`.
    if let Some(poste) = canaux.en_vol.as_ref() {
        poste.relever(
            &mut etat.radio_eq,
            sortie.output_sample_rate,
            channels as u16,
        );
    }
    apply_radio_eq(&mut etat.radio_eq, &mut interleaved);

    let mut packet_buf: Vec<u8> =
        Vec::with_capacity(interleaved.len() * usize::from(sortie.output_bit_depth / 8));
    if sortie.output_bit_depth == 24 {
        for sample in &interleaved {
            let s32: i32 = (*sample).into_sample();
            packet_buf.extend_from_slice(&s32.to_le_bytes()[1..]);
        }
    } else {
        for sample in &interleaved {
            let s16: i16 = (*sample).into_sample();
            packet_buf.extend_from_slice(&s16.to_le_bytes());
        }
    }

    etat.pcm_buf.extend_from_slice(&packet_buf);

    while etat.pcm_buf.len() >= etat.chunk_size {
        let chunk: Vec<u8> = etat.pcm_buf.drain(..etat.chunk_size).collect();
        // VU-mètres : tappe le PCM au format de sortie avant de le servir (canal
        // séparé, non bloquant — n'affecte pas le flux du renderer).
        if let Some(ltx) = canaux.levels_tx {
            crate::audio::tap::send_windowed_pcm(
                ltx,
                &chunk,
                sortie.output_bit_depth,
                channels as u16,
                sortie.output_sample_rate,
            );
        }
        if canaux.rt.block_on(canaux.tx.send(chunk)).is_err() {
            debug!("radio_local_decode_consumer_dropped");
            return SuitePaquet::ConsommateurParti;
        }
        if !etat.first_chunk_sent {
            etat.first_chunk_sent = true;
            canaux.data_ready.notify_one();
        }
    }

    SuitePaquet::PaquetServi
}

/// Quatrième temps, après que la boucle de décodage a cassé : le
/// consommateur est-il encore là, le compteur de reconnexions est-il à
/// remettre à zéro, le plafond est-il atteint ; sinon, on se reconnecte.
fn reprendre_apres_coupure(
    etat: &mut EtatRadio,
    url: &str,
    canaux: &CanauxRadio<'_>,
    connected_at: std::time::Instant,
) -> SuiteRadio {
    use tracing::{info, warn};

    // Inner loop broke because the upstream stream dropped (not tx closed).
    // Reconnect and keep feeding the SAME session (pcm_buf carries over).
    if canaux.tx.is_closed() {
        return SuiteRadio::Rendre(Ok(()));
    }
    // MAX_RECONNECTS guards against a *permanently dead* station (rapid
    // back-to-back failures) — not against a healthy station's periodic
    // upstream drops. FIP-style streams drop the body roughly every ~6 min,
    // so a cumulative counter hit 30 at ~3h and cut a good listen (Xavier
    // #1212, a regression of #382). Reset the counter after any sustained
    // good stretch so a normal long listen is never capped, while a dead
    // station (each connection dies in <60s) still burns through
    // MAX_RECONNECTS in seconds and correctly falls back to the poller.
    if connected_at.elapsed() >= std::time::Duration::from_secs(60) {
        etat.reconnects = 0;
    }
    etat.reconnects += 1;
    if etat.reconnects > MAX_RECONNECTS {
        warn!(url = %url, reconnects = etat.reconnects, "radio_reconnect_giving_up");
        return SuiteRadio::Rendre(Ok(()));
    }
    etat.dropped_at = Some(std::time::Instant::now());
    info!(url = %url, attempt = etat.reconnects, "radio_upstream_dropped_reconnecting");
    std::thread::sleep(std::time::Duration::from_millis(500));
    SuiteRadio::Reconnecter
}

/// Le type MIME d'un flux Ogg : `application/ogg`, `audio/ogg`,
/// `audio/ogg; codecs=opus`, `audio/opus`, `audio/vorbis`.
fn ogg_par_le_type(content_type: &str) -> bool {
    let ct = content_type.to_ascii_lowercase();
    let essence = ct.split(';').next().unwrap_or("").trim();
    matches!(
        essence,
        "application/ogg" | "audio/ogg" | "audio/opus" | "audio/vorbis" | "audio/x-ogg"
    )
}

#[path = "radio_ogg.rs"]
mod radio_ogg;

#[cfg(test)]
#[path = "radio_source_tests.rs"]
mod radio_source_tests;
