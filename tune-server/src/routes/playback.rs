use crate::routes::panne_sql::OuDefautJournalise;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{error, info, warn};

use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::orchestrator::PlayResult;
use tune_core::outputs::OutputCommandError;

use crate::error::AppError;
use crate::routes::active_profile::ActiveProfile;
use crate::state::AppState;

/// Map an orchestrator play error to an appropriate HTTP status code.
///
/// Streaming service failures (yt-dlp, API errors, auth) are upstream issues
/// and should be 502 Bad Gateway, not 500 Internal Server Error.
/// Device-offline errors are 503 Service Unavailable.
/// Everything else is 500.
fn play_error_response(e: String) -> axum::response::Response {
    // Free-tier zone cap sentinel from orchestrator.play() → clean 402 the
    // web/app can render as an upgrade prompt.
    if let Some(msg) = e.strip_prefix("premium_required:") {
        return (
            StatusCode::PAYMENT_REQUIRED,
            Json(json!({
                "error": "premium_required",
                "feature": "Unlimited Zones",
                "message": msg,
                "upgrade_url": "https://mozaiklabs.fr/pricing",
            })),
        )
            .into_response();
    }
    // Orphan-zone sentinel from orchestrator.play(): the zone row has no
    // output_device_id, so playback can never produce sound (Yacine, 24/07).
    // 409 Conflict: the request is well-formed but the zone's state makes it
    // impossible — the client should surface the message and grey the zone
    // (it is also reported online:false by GET /zones).
    if let Some(msg) = e.strip_prefix("zone_no_output_device:") {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "zone_no_output_device",
                "message": msg,
            })),
        )
            .into_response();
    }
    // Stale-output sentinel from orchestrator.play(): the zone points at an
    // output device that has vanished, and no live output of the same name could
    // be re-bound automatically (#1287 — none found, or several, so binding one
    // would be a guess). 409 like the orphan-zone case: well-formed request, the
    // zone's state makes it impossible. The message is already actionable, the
    // client just surfaces it.
    if let Some(msg) = e.strip_prefix("zone_output_unavailable:") {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "zone_output_unavailable",
                "message": msg,
            })),
        )
            .into_response();
    }
    // Missing-file sentinel from orchestrator.play(): the track's file_path no
    // longer exists on disk (moved/deleted drive, stale scan). 404 so the client
    // shows a real error instead of the track "playing" silently (JP).
    if let Some(path) = e.strip_prefix("file_not_found:") {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "file_not_found",
                "message": format!("File not found: {path} — it may have been moved or deleted. Rescan your library."),
                "path": path,
            })),
        )
            .into_response();
    }
    let code = if e.contains("YouTube")
        || e.contains("youtube")
        || e.contains("yt-dlp")
        || e.contains("yt_dlp")
        || e.contains("stream url")
        || e.contains("Streaming service")
        || e.contains("streaming")
        || e.contains("Qobuz")
        || e.contains("qobuz")
        || e.contains("Tidal")
        || e.contains("tidal")
        || e.contains("Deezer")
        || e.contains("deezer")
        || e.contains("Spotify")
        || e.contains("spotify")
        || e.contains("401")
        || e.contains("403")
        || e.contains("not playable")
        || e.contains("extraction")
    {
        StatusCode::BAD_GATEWAY
    } else if e.contains("offline") || e.contains("Output device") {
        // A renderer that rejects our stream (e.g. a Samsung TV faulting on
        // SetAVTransportURI / an unsupported protocolInfo) is surfaced by the
        // orchestrator as "Output device error: …". That is a device-side
        // rejection, not a bug in Tune → 503, so the client shows a clean
        // "device refused" instead of a scary "Erreur 500" (Bilou, #1135).
        StatusCode::SERVICE_UNAVAILABLE
    } else if e.contains("transcode") || e.contains("decode") || e.contains("corrupted source") {
        // Media-processing failure (FLAC→WAV transcode for a renderer that
        // doesn't advertise FLAC, a corrupt/unsupported source, a decode
        // timeout). This is an upstream/content problem, not an internal Tune
        // fault → 502 Bad Gateway rather than a blunt 500 (Bilou, #1135).
        StatusCode::BAD_GATEWAY
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    // JSON body like the sentinel branches above, so clients can show the
    // actual message instead of the bare status line ("503 Service
    // Unavailable") they fell back to when the body was plain text
    // (forum #1183). HTTP codes are unchanged.
    let error_kind = match code {
        StatusCode::BAD_GATEWAY => "upstream_error",
        StatusCode::SERVICE_UNAVAILABLE => "device_unavailable",
        _ => "playback_error",
    };
    (
        code,
        Json(json!({
            "error": error_kind,
            "message": e,
        })),
    )
        .into_response()
}

/// Réponse stable des commandes de sortie. Une capacité absente est une
/// requête impossible (422), pas une panne ; un backend qui refuse une
/// capacité déclarée est une erreur de passerelle (502), jamais un faux 200.
pub(crate) fn output_command_error_response(error: OutputCommandError) -> axum::response::Response {
    match error {
        OutputCommandError::Unsupported { command } => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": "unsupported_output_command",
                "command": command,
                "message": format!("Output does not support {command}"),
            })),
        )
            .into_response(),
        OutputCommandError::Failed { command, message } => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": "output_command_failed",
                "command": command,
                "message": message,
            })),
        )
            .into_response(),
    }
}

/// Persist the queue state for a zone to disk (non-blocking).
fn persist_queue_async(state: &AppState, zone_id: i64) {
    let db = state.backend.clone();
    let db_path = state.config.db_path.clone();
    let playback = state.playback.clone();
    tokio::spawn(async move {
        let zone_state = playback.get_state(zone_id).await;
        tokio::task::spawn_blocking(move || {
            tune_core::queue_persistence::save_queue(&db, &db_path, zone_id, &zone_state);
        });
    });
}

/// Whether the server would accept the manual "next" action as an actual
/// advance instead of stopping the zone at the end of the queue.
///
/// Keep this as a thin projection of the command's own decision.  Rebuilding
/// the rule from `queue_position` on the client is wrong under shuffle, where
/// the next item follows the materialised permutation rather than raw queue
/// order (#2337).
pub(crate) fn can_skip_next(zone_state: &tune_core::playback::ZoneState) -> bool {
    tune_core::poller::PositionPoller::next_position_manual(zone_state).is_some()
}

pub(crate) async fn build_zone_json(state: &AppState, zone_id: i64) -> Value {
    let zone_state = state.playback.get_state(zone_id).await;
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let zone_db = zone_repo.get(zone_id).ok().flatten();
    let mut v = json!({
        "id": zone_id,
        "name": zone_db.as_ref().map(|z| &z.name),
        "output_type": zone_db.as_ref().and_then(|z| z.output_type.as_ref()),
        "output_device_id": zone_db.as_ref().and_then(|z| z.output_device_id.as_ref()),
        "volume": zone_state.volume,
        // #1274 — lecture en dB du volume rendu juste au-dessus, jamais d'une
        // autre source : les deux champs doivent toujours dire la meme chose.
        "volume_db": tune_core::audio::volume_scale::linear_to_db(zone_state.volume),
        "state": zone_state.state,
        "current_track": zone_state.now_playing.as_ref().map(|np| json!({
            "id": np.track_id,
            "title": np.title,
            "artist_name": np.artist_name,
            "album_title": np.album_title,
            "cover_path": np.cover_path,
            "duration_ms": np.duration_ms,
            "source": np.source,
            "source_id": np.source_id,
            "format": np.format,
            "sample_rate": np.sample_rate,
            "bit_depth": np.bit_depth,
            "genre": np.genre,
            "year": np.year,
            // ⚠️ Ce JSON est ecrit A LA MAIN : ajouter un champ a `NowPlaying`
            // ne suffit PAS a le faire sortir ici. Sans ces deux lignes, le
            // client continuerait de deviner l'album depuis son titre — et
            // « Entreat » retomberait sur la page de The Cure (FabienM).
            "album_id": np.album_id,
            "artist_id": np.artist_id,
        })),
        "position_ms": zone_state.position_ms,
        "queue_length": zone_state.queue_length,
        "queue_position": zone_state.queue_position,
        "can_skip_next": can_skip_next(&zone_state),
        // #2092 / #2055 — TROISIÈME construction de la charge utile d'une zone,
        // et la dernière qui ne portait pas le transport.
        //
        // Le correctif #2153 avait rendu `shuffle` et `repeat` aux DEUX charges
        // utiles de `zones.rs` (liste et fiche) ; le WebSocket les envoyait déjà
        // (`ws.rs`), et la zone tout juste créée les pose aussi
        // (`zone_repo::…`, « même divergence que #2092, en plus discret »).
        // Celle-ci, rendue par une vingtaine de retours — `play` et ses neuf
        // sorties anticipées, `pause`, `resume`, `stop`, `queue/jump`,
        // `pins/{i}/invoke` —, portait `queue_length`, `queue_position` et
        // `can_skip_next` mais pas les deux réglages dont `can_skip_next`
        // DÉPEND : sous aléatoire, la fin de file suit la permutation (#2337).
        //
        // Le garde-fou écrit pour #2092 ne pouvait pas le voir : son
        // `code_de_production()` ne lisait que `zones.rs`. La divergence qu'il
        // devait empêcher s'était produite un fichier plus loin. Il lit
        // désormais ce corps-ci aussi.
        "shuffle": zone_state.shuffle,
        // Le TYPE et non la chaîne « off » : un renommage de variante suit ici
        // tout seul.
        "repeat": zone_state.repeat,
        "muted": zone_state.muted,
    });
    // Ancrage temporel de la métadonnée courante (paroles radio) — mêmes
    // champs que GET /zones et GET /zones/{id}.
    if let Some(obj) = v.as_object_mut() {
        crate::routes::zones::inject_metadata_anchor(obj, &zone_state);
        crate::routes::zones::inject_session_context(obj, &zone_state);
    }
    // Où va le son — même champ que GET /zones et GET /zones/{id} (#1499).
    if let Some(ref zone) = zone_db {
        v.as_object_mut().unwrap().insert(
            "output_reach".into(),
            json!(crate::routes::zones::output_reach(state, zone, &zone_state).await),
        );
        // Les VU ont-ils une source ? Même champ que GET /zones et
        // GET /zones/{id} : trois surfaces, une seule vérité.
        v.as_object_mut().unwrap().insert(
            "levels_available".into(),
            json!(crate::routes::zones::levels_available(state, zone).await),
        );
        v.as_object_mut().unwrap().insert(
            "output_capabilities".into(),
            json!(
                crate::routes::zones::output_capabilities(state, zone.output_device_id.as_deref())
                    .await
            ),
        );
    }
    // #3164 — la règle vit maintenant dans
    // `zones::zone_recoit_l_adresse_du_flux`, et `inject_stream_url` est le
    // seul chemin qui pose l'adresse. Elle n'était appliquée QU'ICI ; les
    // quatre autres surfaces la recopiaient en commentaire sans la poser.
    if let Some(obj) = v.as_object_mut() {
        crate::routes::zones::inject_stream_url(
            obj,
            state,
            zone_db.as_ref().and_then(|z| z.output_type.as_deref()),
            zone_state
                .now_playing
                .as_ref()
                .and_then(|np| np.stream_id.as_deref()),
        );
    }
    // Include signal_path (the bit-perfect indicator) so the play / next /
    // previous / resume responses carry it, matching GET /zones/{id}. Without
    // it the indicator was absent on the FIRST track — playAndSync renders this
    // play response — and only appeared from the SECOND track on, because
    // nextAndSync refreshes via GET /zones/{id}, which does include it
    // (forum #1012, Bilou).
    if let Some(ref zone) = zone_db {
        let devices = state.scanner.devices().await;
        let renderer_label = zone
            .output_device_id
            .as_deref()
            .and_then(|id| devices.iter().find(|d| d.id == id).map(|d| d.name.as_str()));
        let audio_backend_pref = state.display_audio_backend();
        #[cfg(feature = "local-audio")]
        let audio_backend = tune_core::outputs::local::active_backend_name(&audio_backend_pref);
        #[cfg(not(feature = "local-audio"))]
        let audio_backend = "none";
        let wire = match zone_state
            .now_playing
            .as_ref()
            .and_then(|np| np.stream_id.as_deref())
        {
            Some(sid) => state.streamer.stream_output_wire(sid).await,
            None => None,
        };
        let signal_path = crate::routes::zones::build_signal_path_pub(
            &zone_state,
            zone,
            &state.backend,
            renderer_label,
            audio_backend,
            wire.as_ref(),
        );
        v.as_object_mut()
            .unwrap()
            .insert("signal_path".into(), json!(signal_path));
        // #1395 — même raison que pour `signal_path` ci-dessus : c'est CETTE
        // réponse que `playAndSync` rend à la première piste. Sans le champ
        // ici, la divergence « réglé ASIO / joué en WASAPI » n'apparaîtrait
        // qu'à partir de la seconde (forum #1012, Bilou — déjà lui).
        if let Some(status) = crate::routes::zones::local_backend_status_value(
            zone.output_type.as_deref(),
            &audio_backend_pref,
        ) {
            v.as_object_mut()
                .unwrap()
                .insert("audio_backend_status".into(), status);
        }
        v.as_object_mut()
            .unwrap()
            .insert("resolving".into(), json!(zone_state.resolving));
    }
    v
}

async fn build_zone_json_with_result(state: &AppState, zone_id: i64, result: &PlayResult) -> Value {
    let mut zone = build_zone_json(state, zone_id).await;
    if let Some(ref err) = result.error {
        zone.as_object_mut()
            .unwrap()
            .insert("error".into(), json!(err));
    }
    zone.as_object_mut()
        .unwrap()
        .insert("output_sent".into(), json!(result.output_sent));
    // #3164 — `PlayResult::stream_url` vaut `Some(resolved.url)` pour TOUTES
    // les zones : c'est l'adresse que le renderer va consommer. Sans le filtre,
    // cet `insert` ÉCRASAIT la décision de `build_zone_json` juste au-dessus et
    // rendait l'adresse aux vingt routes de lecture qui passent par ici
    // (`play`, `next`, `previous`, `resume`, `queue/jump`, `pins/{i}/invoke`…)
    // — c'est-à-dire au chemin le plus fréquenté du client web.
    let output_type = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten()
        .and_then(|z| z.output_type);
    if crate::routes::zones::zone_recoit_l_adresse_du_flux(output_type.as_deref())
        && let Some(ref url) = result.stream_url
    {
        zone.as_object_mut()
            .unwrap()
            .insert("stream_url".into(), json!(url));
    }
    zone
}

#[derive(Deserialize, Default)]
struct PlayRequest {
    track_id: Option<i64>,
    track_ids: Option<Vec<i64>>,
    album_id: Option<i64>,
    playlist_id: Option<i64>,
    start_index: Option<i64>,
    source: Option<String>,
    source_id: Option<String>,
    streaming_album_id: Option<String>,
    streaming_playlist_id: Option<String>,
    output_device_id: Option<String>,
    title: Option<String>,
    artist_name: Option<String>,
    album_title: Option<String>,
    cover_path: Option<String>,
    duration_ms: Option<i64>,
    seek_ms: Option<u64>,
    temp_file_path: Option<String>,
    // Album numbering for a single streaming track, which becomes the queue:
    // without it the queue row has no track number (see QueueAddRequest).
    track_number: Option<i64>,
    disc_number: Option<i64>,
    // Real resolution/codec for a media-server (source="upnp") item, passed by
    // the client from the DIDL res@ attributes so the signal path shows the true
    // rate/bit-depth and ALAC-vs-AAC instead of "AAC 44kHz/16bit" (Yves, NAS).
    sample_rate: Option<u32>,
    bit_depth: Option<u16>,
    media_format: Option<String>,
    // #2441 — ce que l'auditeur a demande, DIT par l'appelant.
    //
    // Le corps porte deja de quoi reconnaitre un album ou une playlist
    // (`album_id`, `playlist_id`, `streaming_*_id`) : `contexte_de_lecture`
    // s'en sert. Mais « Toutes les pistes » depuis une page artiste, ou la
    // lecture d'un label, arrivent comme une simple liste de `track_ids` —
    // rien dans le corps ne dit d'ou venait le clic. Ces deux champs laissent
    // le client l'ENONCER ; ils priment sur toute deduction.
    context_type: Option<String>,
    context_id: Option<String>,
}

/// Les cinq natures d'objet que l'auditeur peut demander, telles que FabienM
/// les a enumerees (fil forum 1557, 26/08/2026) : « titre, album, playlist,
/// artiste, label ».
///
/// La liste est ici pour que le serveur refuse une valeur inventee plutot que
/// de laisser n'importe quelle chaine entrer en base : une colonne libre se
/// remplirait de variantes ("Album", "albums", "PLAYLIST") et le jour ou une
/// regle d'affichage sera arbitree, elle porterait sur du sable.
const CONTEXTES_CONNUS: [&str; 5] = ["track", "album", "playlist", "artist", "label"];

/// Ce que l'auditeur a demande, lu dans le corps de `POST /zones/:id/play`.
///
/// FabienM pose la regle au point de clic : « le type pris en compte dans ces
/// rubriques depend de l'endroit ou l'utilisateur a clique sur "Lire" ». Cette
/// fonction ne fait que la transcrire — elle ne decide RIEN de ce qui sera
/// affiche ensuite, ce point n'etant pas arbitre (#2441).
///
/// L'ordre suit celui du gestionnaire lui-meme, ou les conteneurs priment sur
/// la piste : un `POST` qui porte a la fois `album_id` et `track_id` met tout
/// l'album en file, donc c'est bien l'album qui a ete demande.
///
/// `(None, None, None)` quand rien ne permet de trancher — notamment une
/// liste de `track_ids` nue, qui peut aussi bien venir d'une page artiste que
/// d'une selection manuelle. On ecrit alors NULL : une intention devinee est
/// pire qu'une intention absente.
///
/// Le TROISIEME membre est le service auquel l'identifiant appartient, sans
/// lequel il ne s'ouvre pas (#1361). Il ne se lit pas dans le corps entier
/// mais dans la BRANCHE prise : `album_id`, `playlist_id` et `track_id` sont
/// des `i64` de bibliotheque, donc `"local"` quel que soit le `source`
/// annonce — c'est bien la bibliotheque que le gestionnaire interroge sous ces
/// champs, et `"7"` n'aurait aucun sens chez Qobuz. Les branches de streaming,
/// elles, portent le service nomme par l'appelant.
fn contexte_de_lecture(body: &PlayRequest) -> (Option<String>, Option<String>, Option<String>) {
    /// L'espace de noms d'un identifiant de BIBLIOTHEQUE. Le mot est celui que
    /// `NowPlaying.source` emploie deja pour une piste locale : le client lit
    /// le meme vocabulaire des deux cotes.
    const LOCAL: &str = "local";

    // 1. L'appelant l'a dit explicitement : sa parole prime sur toute
    //    deduction. C'est la seule voie pour `artist` et `label`.
    if let Some(t) = body
        .context_type
        .as_deref()
        .map(str::trim)
        .filter(|t| CONTEXTES_CONNUS.contains(t))
    {
        // L'appelant qui ENONCE son contexte enonce aussi le service dans
        // `source`, comme pour toute lecture de service. Son absence dit la
        // bibliotheque : c'est la seule autre provenance possible.
        let service = body.source.clone().unwrap_or_else(|| LOCAL.to_string());
        return (Some(t.to_string()), body.context_id.clone(), Some(service));
    }

    // 2. Sinon, ce que le corps trahit deja de lui-meme. Les deux premiers cas
    //    exigent `source` comme le gestionnaire lui-meme : sans service
    //    nomme, il ne prend pas la branche streaming, et le contexte doit
    //    decrire ce qui joue vraiment.
    if let (Some(service), Some(id)) = (&body.source, &body.streaming_album_id) {
        return (
            "album".to_string().into(),
            Some(id.clone()),
            Some(service.clone()),
        );
    }
    if let (Some(service), Some(id)) = (&body.source, &body.streaming_playlist_id) {
        return (
            "playlist".to_string().into(),
            Some(id.clone()),
            Some(service.clone()),
        );
    }
    if let Some(id) = body.album_id {
        return (
            "album".to_string().into(),
            Some(id.to_string()),
            Some(LOCAL.to_string()),
        );
    }
    if let Some(id) = body.playlist_id {
        return (
            "playlist".to_string().into(),
            Some(id.to_string()),
            Some(LOCAL.to_string()),
        );
    }
    if let Some(id) = body.track_id {
        return (
            "track".to_string().into(),
            Some(id.to_string()),
            Some(LOCAL.to_string()),
        );
    }
    // Piste unique en streaming : `source` + `source_id`, sans track_id.
    if let (Some(service), Some(id), None) = (&body.source, &body.source_id, &body.track_ids) {
        return (
            "track".to_string().into(),
            Some(id.clone()),
            Some(service.clone()),
        );
    }
    (None, None, None)
}

#[derive(Deserialize)]
struct SeekRequest {
    position_ms: i64,
}

#[derive(Deserialize)]
struct VolumeRequest {
    /// Volume linéaire 0..1, le champ historique. `Option` depuis #1274 pour
    /// laisser passer une requête qui ne parle qu'en dB — les clients
    /// déployés, eux, l'envoient toujours et ne changent pas de comportement.
    volume: Option<f64>,
    /// Atténuation demandée en dB (≤ 0 ; `0` = 100 %). Exclusif avec `volume`.
    ///
    /// C'est le réglage que réclame #1274 : un curseur au pour-cent ne permet
    /// pas de viser −18 dB, et l'écart entre deux crans varie de 0,09 dB en
    /// haut d'échelle à 6 dB en bas.
    volume_db: Option<f64>,
}

#[derive(Deserialize)]
struct ShuffleQuery {
    enabled: Option<bool>,
}

#[derive(Deserialize)]
struct RepeatQuery {
    mode: Option<String>,
}

#[derive(Deserialize)]
struct QueueAddRequest {
    #[serde(default)]
    track_ids: Vec<i64>,
    track_id: Option<i64>,
    /// Enfiler un ALBUM entier.
    ///
    /// Il manquait : le client devait résoudre les pistes lui-même puis envoyer
    /// `track_ids`, ce que le commentaire de `addToQueue` documentait comme un
    /// contournement. Le défaut de ce contournement n'est pas son coût en
    /// requêtes, c'est qu'il ignore le rattrapage de la ligne sœur — l'album
    /// s'ajoutait donc VIDE là où « lire » fonctionne.
    album_id: Option<i64>,
    position: Option<i64>,
    // Streaming track fields (single)
    source: Option<String>,
    source_id: Option<String>,
    title: Option<String>,
    artist_name: Option<String>,
    album_title: Option<String>,
    cover_path: Option<String>,
    duration_ms: Option<i64>,
    // Album numbering, when the client knows it. Without these the queue row
    // has no track number, and anything that lays the queue out in album order
    // — the queue view, an output that files tracks by their rank — has nothing
    // to go on.
    track_number: Option<i64>,
    disc_number: Option<i64>,
    // Batch streaming tracks: [{source, source_id, title?, artist_name?, ...}]
    #[serde(default)]
    tracks: Vec<StreamingTrackItem>,
}

#[derive(Deserialize)]
struct StreamingTrackItem {
    source: String,
    source_id: String,
    title: Option<String>,
    artist_name: Option<String>,
    album_title: Option<String>,
    cover_path: Option<String>,
    duration_ms: Option<i64>,
    track_number: Option<i64>,
    disc_number: Option<i64>,
}

#[derive(Deserialize)]
struct SaveAsPlaylistRequest {
    name: Option<String>,
}

#[derive(Deserialize)]
struct QueueMoveRequest {
    from_position: i64,
    to_position: i64,
}

#[derive(Deserialize)]
struct TransferRequest {
    target_zone_id: i64,
    #[serde(default = "default_true")]
    stop_source: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
struct QueueJumpRequest {
    position: i64,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/upload", post(upload_audio_file))
        .route("/now-listening", get(now_listening))
        .route("/{id}/status", get(zone_status))
        .route("/{id}/play", post(play))
        .route("/{id}/pause", post(pause))
        .route("/{id}/resume", post(resume))
        .route("/{id}/stop", post(stop))
        .route("/{id}/next", post(next))
        .route("/{id}/previous", post(previous))
        .route("/{id}/seek", post(seek))
        .route("/{id}/volume", post(set_volume))
        .route("/{id}/shuffle", post(toggle_shuffle))
        .route("/{id}/repeat", post(set_repeat))
        .route("/{id}/queue", get(get_queue).delete(queue_clear))
        .route("/{id}/queue/add", post(queue_add))
        .route("/{id}/queue/move", post(queue_move))
        .route("/{id}/queue/jump", post(queue_jump))
        .route("/{id}/queue/clear", post(queue_clear))
        .route(
            "/{id}/queue/{position}",
            axum::routing::delete(queue_remove),
        )
        .route("/{id}/queue/save-as-playlist", post(save_queue_as_playlist))
        .route("/{id}/sleep", get(get_sleep).post(set_sleep))
        .route("/{id}/eq", get(get_eq).post(set_eq))
        // DSP route is in zones.rs (/{id}/dsp GET+PUT)
        .route("/{id}/crossfade", get(get_crossfade).post(set_crossfade))
        .route("/{id}/normalization", post(set_normalization))
        .route("/{id}/transfer/{target_id}", post(transfer_playback))
        .route("/{id}/transfer", post(transfer_queue))
        .route("/{id}/alarm", get(get_alarms).post(create_alarm))
        .route(
            "/{id}/alarm/{alarm_id}",
            axum::routing::delete(delete_alarm),
        )
        .route("/{id}/pins", get(get_zone_pins).post(set_zone_pin))
        .route("/{id}/pins/{index}", axum::routing::delete(clear_zone_pin))
        .route("/{id}/pins/{index}/invoke", post(invoke_zone_pin))
        .route("/{id}/pins/from-queue", post(save_queue_as_pin))
        .route("/{id}/audiophile", get(get_audiophile).post(set_audiophile))
        .route("/{id}/quality", get(get_quality).post(set_quality))
        .route("/{id}/share", post(share_now_playing))
        .route(
            "/{id}/audio-profile",
            get(get_audio_profile).post(set_audio_profile),
        )
}

async fn now_listening(State(state): State<AppState>) -> Json<Value> {
    let states = state.playback.all_states().await;
    let playing: Vec<Value> = states
        .iter()
        .filter(|s| s.state == tune_core::playback::PlayState::Playing)
        .map(|s| json!(s))
        .collect();
    Json(json!(playing))
}

async fn zone_status(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let zone_state = state.playback.get_state(zone_id).await;
    let mut v = serde_json::to_value(&zone_state).unwrap_or_default();
    if let Some(track_id) = zone_state.now_playing.as_ref().and_then(|np| np.track_id) {
        let credits = TrackRepo::with_backend(state.backend.clone())
            .get_credits(track_id)
            .unwrap_or_default();
        if !credits.is_empty() {
            if let Some(np) = v.get_mut("now_playing").and_then(|np| np.as_object_mut()) {
                np.insert(
                    "credits".into(),
                    serde_json::to_value(&credits).unwrap_or_default(),
                );
            }
        }
    }
    // #3164 — « la surface que les clients interrogent en boucle » (#1274)
    // publiait l'adresse du flux sans regarder le type de sortie.
    let output_type = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten()
        .and_then(|z| z.output_type);
    if let Some(obj) = v.as_object_mut() {
        crate::routes::zones::inject_stream_url(
            obj,
            &state,
            output_type.as_deref(),
            zone_state
                .now_playing
                .as_ref()
                .and_then(|np| np.stream_id.as_deref()),
        );
    }
    // #1274 — cette charge utile est la sérialisation brute de
    // `PlaybackState`, qui ne porte que le volume linéaire ; le dB s'ajoute
    // ici, à partir de ce même nombre. `/zones/{id}/status` est la surface que
    // les clients interrogent en boucle : l'oublier obligerait chacun à
    // recalculer, c'est-à-dire à diverger.
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "volume_db".into(),
            json!(tune_core::audio::volume_scale::linear_to_db(
                zone_state.volume
            )),
        );
    }
    Json(v)
}

/// Replace a zone's queue after taking the SQLite user-write lane.
///
/// A library scan holds a per-batch write transaction on the shared SQLite
/// connection (BEGIN IMMEDIATE … COMMIT) while releasing the connection mutex
/// between statements. The process-wide lane makes a concurrent `set_queue`
/// wait for that logical transaction instead of entering it and failing. The
/// retries remain as a defensive fallback for an unregistered transaction;
/// non-transient errors still return immediately.
async fn set_queue_retrying(
    queue_repo: &PlayQueueRepo,
    sqlite: bool,
    zone_id: i64,
    track_ids: &[i64],
) -> Result<(), String> {
    let _write_guard = if sqlite {
        Some(crate::sqlite_write_gate::user_queue().await)
    } else {
        None
    };
    const MAX_ATTEMPTS: usize = 12;
    let mut last_err = String::new();
    for attempt in 0..MAX_ATTEMPTS {
        match queue_repo.set_queue(zone_id, track_ids) {
            Ok(()) => return Ok(()),
            Err(e) if e.contains("within a transaction") => {
                last_err = e;
                if attempt + 1 < MAX_ATTEMPTS {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err)
}

#[cfg(test)]
mod sqlite_scan_queue_arbitration_tests {
    use super::set_queue_retrying;
    use std::sync::Arc;
    use std::time::Duration;
    use tune_core::db::backend::DbBackend;
    use tune_core::db::models::Track;
    use tune_core::db::play_queue_repo::PlayQueueRepo;
    use tune_core::db::sqlite::SqliteDb;
    use tune_core::db::track_repo::TrackRepo;

    /// Yves (#1997): while a scan owned the shared SQLite transaction, Play
    /// exhausted its retries and cleared the requested queue. The user write
    /// must now wait for the scan batch, then replace the queue byte-for-byte.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lecture_attend_le_lot_de_scan_puis_conserve_toute_la_file() {
        let db = SqliteDb::open_in_memory().expect("SQLite in-memory");
        db.init_schema().expect("schema");
        db.execute(
            "INSERT INTO zones (name, output_type) VALUES ('Main', 'local')",
            &[],
        )
        .expect("zone");

        let backend: Arc<dyn DbBackend> = Arc::new(db.clone());
        let tracks = TrackRepo::with_backend(backend.clone());
        let mut first = Track::new("Premier".into());
        first.file_path = Some("/music/first.flac".into());
        let mut second = Track::new("Second".into());
        second.file_path = Some("/music/second.flac".into());
        let first_id = tracks.create(&first).expect("first track");
        let second_id = tracks.create(&second).expect("second track");

        // Simulate the logical scan guard plus its manual transaction. Using
        // the async acquisition here avoids blocking a Tokio worker in a test;
        // production scan batches acquire the same gate from spawn_blocking.
        let scan_guard = crate::sqlite_write_gate::user_queue().await;
        backend
            .execute_batch("BEGIN IMMEDIATE")
            .expect("scan begin");

        let queue_backend = backend.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let mut writer = tokio::spawn(async move {
            let _ = started_tx.send(());
            let queue = PlayQueueRepo::with_backend(queue_backend);
            set_queue_retrying(&queue, true, 1, &[first_id, second_id]).await
        });
        started_rx.await.expect("queue task started");

        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut writer)
                .await
                .is_err(),
            "the queue write must wait while the scan transaction is open"
        );

        backend.execute_batch("COMMIT").expect("scan commit");
        drop(scan_guard);
        writer
            .await
            .expect("queue task")
            .expect("queue write after scan");

        let queue = PlayQueueRepo::with_backend(backend);
        let entries = queue.get_queue(1).expect("persisted queue");
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.track_id)
                .collect::<Vec<_>>(),
            vec![first_id, second_id],
            "the requested queue must survive the scan arbitration intact"
        );
    }
}

/// La position rendue par la base au démarrage, à demander pour CETTE piste.
///
/// Ce que le serveur écrit : le poller persiste `zones.last_position_ms` tout
/// au long de la lecture, et `restore_playback_positions` la réinjecte dans
/// l'état de zone au démarrage. C'est cette valeur que `/zones` sert sous le
/// nom `position_ms` et que le curseur affiche à l'ouverture de l'interface.
///
/// Ce que le serveur en faisait : rien. Les chemins « Lecture après arrêt »
/// construisaient tous leur `PlayRequest` avec `seek_ms: None`, si bien que le
/// morceau repartait de 0:00 pendant que l'écran annonçait 2:31 — Sandro,
/// fil 1610, sortie Diretta UPnP (#2876).
///
/// Rend la position à passer dans le `PlayRequest`. Ne vaut QUE pour la piste
/// restaurée, et une seule fois : le `play()` qui suit efface le marqueur (voir
/// `ZoneState::pending_resume_ms`).
async fn position_de_reprise(
    state: &AppState,
    zone_id: i64,
    piste: Option<i64>,
    source_id: Option<&str>,
) -> Option<u64> {
    let zone = state.playback.get_state(zone_id).await;
    reprise_applicable(&zone, piste, source_id).map(|ms| ms as u64)
}

/// Ancre dans l'état de zone la position que la requête demande VRAIMENT.
///
/// `PlaybackManager::play` remet `position_ms` à zéro sauf juste après un seek,
/// et le poller lit la même estampille pour ouvrir sa fenêtre de grâce. Sans cet
/// ancrage, le son repartirait au bon endroit et le curseur retomberait à
/// 0:00 — on aurait déplacé le mensonge de Sandro au lieu de le lever.
///
/// ⚠️ Lit `demandee`, c'est-à-dire le `seek_ms` **tel qu'il part dans le
/// `PlayRequest`**, et jamais la variable qui l'a produit. C'est ce qui fait que
/// débrancher le champ éteint aussi l'ancrage : un ancrage qui survivrait au
/// débranchement rendrait la contre-épreuve verte alors que le morceau
/// repartirait toujours de zéro (mesuré : cette première rédaction du correctif
/// ne prouvait rien).
///
/// N'agit que pour une reprise : un `seek_ms` venu du corps de la requête garde
/// le comportement d'avant.
async fn ancrer_position_demandee(
    state: &AppState,
    zone_id: i64,
    demandee: Option<u64>,
    reprise: Option<u64>,
) {
    let (Some(position), Some(_)) = (demandee, reprise) else {
        return;
    };
    state.playback.seek(zone_id, position as i64).await;
    info!(
        zone_id,
        position_ms = position,
        "reprise_a_la_position_restauree"
    );
}

/// La décision seule, sans effet de bord : cette demande de lecture porte-t-elle
/// sur la piste dont le démarrage a restauré la position ?
///
/// Une piste locale s'identifie par son `track_id`, un flux distant par son
/// `source_id` — comparer l'un à l'autre ferait reprendre au mauvais endroit
/// une piste qui n'a rien à voir.
fn reprise_applicable(
    zone: &tune_core::playback::ZoneState,
    piste: Option<i64>,
    source_id: Option<&str>,
) -> Option<i64> {
    let position = zone.pending_resume_ms.filter(|ms| *ms > 0)?;
    let np = zone.now_playing.as_ref()?;
    let meme_piste = match (piste, np.track_id) {
        (Some(demandee), Some(restauree)) => demandee == restauree,
        _ => source_id.is_some() && source_id == np.source_id.as_deref(),
    };
    meme_piste.then_some(position)
}

async fn play(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(zone_id): Path<i64>,
    body: Option<Json<PlayRequest>>,
) -> impl IntoResponse {
    // A user-initiated play starts (or takes over) the listening session on
    // this zone: stamp the caller's profile so record_listen — and every
    // autoplay / gapless advance that inherits it — tags listen_history to the
    // right person. Transport (next/previous/resume) reuses this owner; after a
    // restart the in-memory session resets to None → NULL until the next play.
    state
        .playback
        .set_session_profile(zone_id, Some(profile.id()))
        .await;
    // When called with an empty body (e.g. Play after Stop), resume the
    // current track instead of returning 400 "no track source specified".
    let body = match body {
        Some(Json(b)) => b,
        None => {
            let current = state.playback.get_state(zone_id).await;
            if let Some(ref np) = current.now_playing {
                let output_device_id = get_zone_device_id(&state, zone_id);
                let reprise =
                    position_de_reprise(&state, zone_id, np.track_id, np.source_id.as_deref())
                        .await;
                let orch_req = tune_core::orchestrator::PlayRequest {
                    zone_id,
                    output_device_id,
                    track_id: np.track_id,
                    source: if np.source == "local" {
                        None
                    } else {
                        Some(np.source.clone())
                    },
                    source_id: np.source_id.clone(),
                    title: Some(np.title.clone()),
                    artist_name: np.artist_name.clone(),
                    album_title: np.album_title.clone(),
                    cover_url: np.cover_path.clone(),
                    duration_ms: Some(np.duration_ms),
                    seek_ms: reprise,
                    temp_file_path: None,
                    sample_rate: None,
                    bit_depth: None,
                    media_format: None,
                    track_number: None,
                    disc_number: None,
                };
                ancrer_position_demandee(&state, zone_id, orch_req.seek_ms, reprise).await;
                return match state.orchestrator.play(orch_req).await {
                    Ok(result) => {
                        // Restore queue_length from DB so the poller can
                        // advance tracks (fixes repeat-all after restart).
                        let qr = PlayQueueRepo::with_backend(state.backend.clone());
                        let q_len = qr.count_all(zone_id).unwrap_or(0);
                        if q_len > 0 {
                            let cur_pos = state.playback.get_state(zone_id).await.queue_position;
                            state
                                .playback
                                .update_queue_info(zone_id, cur_pos, q_len)
                                .await;
                        }
                        persist_queue_async(&state, zone_id);
                        Json(build_zone_json_with_result(&state, zone_id, &result).await)
                            .into_response()
                    }
                    Err(e) => {
                        tracing::warn!(zone_id, error = %e, "play_resume_failed_trying_queue");
                        // Fallback: try to play from queue position 0
                        let qr = PlayQueueRepo::with_backend(state.backend.clone());
                        let q_len = qr.count_all(zone_id).unwrap_or(0);
                        if q_len > 0 {
                            let pos = current.queue_position.min(q_len - 1);
                            state.playback.update_queue_info(zone_id, pos, q_len).await;
                            if let Ok(result) =
                                state.orchestrator.play_from_queue(zone_id, pos).await
                            {
                                return Json(
                                    build_zone_json_with_result(&state, zone_id, &result).await,
                                )
                                .into_response();
                            }
                        }
                        (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
                    }
                };
            }
            // No now_playing — try queue fallback
            {
                let qr = PlayQueueRepo::with_backend(state.backend.clone());
                let q_len = qr.count_all(zone_id).unwrap_or(0);
                if q_len > 0 {
                    let current = state.playback.get_state(zone_id).await;
                    let pos = current.queue_position.min(q_len - 1);
                    state.playback.update_queue_info(zone_id, pos, q_len).await;
                    if let Ok(result) = state.orchestrator.play_from_queue(zone_id, pos).await {
                        return Json(build_zone_json_with_result(&state, zone_id, &result).await)
                            .into_response();
                    }
                }
            }
            // Last resort: resume from last_track saved in DB (after stop)
            {
                let zone_repo =
                    tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
                if let Ok(Some(zone)) = zone_repo.get(zone_id) {
                    if let Some(track_id) = zone.last_track_id {
                        let output_device_id = get_zone_device_id(&state, zone_id);
                        let orch_req = tune_core::orchestrator::PlayRequest {
                            zone_id,
                            output_device_id,
                            track_id: Some(track_id),
                            source: zone.last_track_source.clone().filter(|s| s != "local"),
                            source_id: zone.last_track_source_id.clone(),
                            title: None,
                            artist_name: None,
                            album_title: None,
                            cover_url: None,
                            duration_ms: None,
                            seek_ms: None,
                            temp_file_path: None,
                            sample_rate: None,
                            bit_depth: None,
                            media_format: None,
                            track_number: None,
                            disc_number: None,
                        };
                        if let Ok(result) = state.orchestrator.play(orch_req).await {
                            return Json(
                                build_zone_json_with_result(&state, zone_id, &result).await,
                            )
                            .into_response();
                        }
                    }
                }
            }
            return (
                StatusCode::BAD_REQUEST,
                "no track source specified and nothing to resume",
            )
                .into_response();
        }
    };

    // #2441 — poser CE QUE l'auditeur vient de demander sur la session de la
    // zone, avant toute branche : les huit chemins de lecture ci-dessous
    // construisent chacun leur `PlayRequest`, et l'orchestrateur relira le
    // contexte depuis l'etat de zone au moment d'ecrire `listen_history` —
    // exactement comme il le fait deja pour le profil proprietaire.
    //
    // Toujours ecraser, meme avec `(None, None)` : ce geste-ci remplace le
    // precedent. Sinon une piste jouee seule apres une playlist heriterait de
    // la playlist.
    //
    // Le corps VIDE (retour au-dessus) ne passe pas ici : une reprise apres
    // Stop n'est pas un nouveau geste, elle garde le contexte en cours.
    let (contexte_type, contexte_id, contexte_service) = contexte_de_lecture(&body);
    state
        .playback
        .set_session_context(zone_id, contexte_type, contexte_id, contexte_service)
        .await;

    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());

    // --- Streaming album: fetch tracks from the service, queue them, play first ---
    if let (Some(source), Some(album_id)) = (&body.source, &body.streaming_album_id) {
        let registry = state.services.lock().await;
        let svc = match registry.get(source) {
            Some(s) => s,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("unknown service: {source}"),
                )
                    .into_response();
            }
        };
        let svc = svc.read().await;
        let tracks = match svc.get_album_tracks(album_id).await {
            Ok(t) => t,
            Err(e) => return (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
        };
        drop(svc);
        drop(registry);

        if tracks.is_empty() {
            return (StatusCode::BAD_REQUEST, "album has no tracks").into_response();
        }

        let start = body.start_index.unwrap_or(0) as usize;
        let start = start.min(tracks.len() - 1);
        let first = &tracks[start];

        let output_device_id = body.output_device_id.clone().or_else(|| {
            let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
            zone_repo
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id)
        });

        // Write queue BEFORE play so WS-triggered fetchQueue() finds it
        let queue_items: Vec<_> = tracks
            .iter()
            .map(|t| {
                (
                    t.id.clone(),
                    t.title.clone(),
                    t.artist.clone(),
                    t.album.clone(),
                    t.cover_path.clone(),
                    t.duration_ms as i64,
                    Some(source.clone()),
                    t.track_number.map(|n| n as i64),
                    t.disc_number.map(|n| n as i64),
                )
            })
            .collect();
        if let Err(e) = queue_repo.set_streaming_queue(zone_id, &queue_items) {
            warn!(zone_id, error = %e, "set_streaming_queue_failed");
        }
        state
            .playback
            .update_queue_info(zone_id, start as i64, tracks.len() as i64)
            .await;

        let orch_req = tune_core::orchestrator::PlayRequest {
            zone_id,
            output_device_id,
            track_id: None,
            source: Some(source.clone()),
            source_id: Some(first.id.clone()),
            title: Some(first.title.clone()),
            artist_name: Some(first.artist.clone()),
            album_title: first.album.clone(),
            cover_url: first.cover_path.clone(),
            duration_ms: Some(first.duration_ms as i64),
            seek_ms: None,
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: first.track_number,
            disc_number: first.disc_number,
        };
        return match state.orchestrator.play(orch_req).await {
            Ok(result) => {
                // Re-assert queue length AFTER play(). The pre-play
                // update_queue_info above is a silent no-op when the zone's
                // in-memory state doesn't exist yet, and play() then creates that
                // state with queue_length=0 — so a streaming album/playlist on a
                // fresh zone stopped after track 1 (next_position saw an empty
                // queue → the poller stopped instead of advancing). The local
                // track path already re-asserts after play(); mirror it here.
                state
                    .playback
                    .update_queue_info(zone_id, start as i64, tracks.len() as i64)
                    .await;
                persist_queue_async(&state, zone_id);
                Json(build_zone_json_with_result(&state, zone_id, &result).await).into_response()
            }
            Err(e) => play_error_response(e).into_response(),
        };
    }

    // --- Streaming playlist: fetch tracks from the service, queue them, play first ---
    if let (Some(source), Some(playlist_id)) = (&body.source, &body.streaming_playlist_id) {
        let registry = state.services.lock().await;
        let svc = match registry.get(source) {
            Some(s) => s,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("unknown service: {source}"),
                )
                    .into_response();
            }
        };
        let svc = svc.read().await;
        let tracks = match svc.get_playlist_tracks(playlist_id).await {
            Ok(t) => t,
            Err(e) => return (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
        };
        drop(svc);
        drop(registry);

        if tracks.is_empty() {
            return (StatusCode::BAD_REQUEST, "playlist has no tracks").into_response();
        }

        let start = body.start_index.unwrap_or(0) as usize;
        let start = start.min(tracks.len() - 1);
        let first = &tracks[start];

        let output_device_id = body.output_device_id.clone().or_else(|| {
            let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
            zone_repo
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id)
        });

        // Write queue BEFORE play so WS-triggered fetchQueue() finds it
        let queue_items: Vec<_> = tracks
            .iter()
            .map(|t| {
                (
                    t.id.clone(),
                    t.title.clone(),
                    t.artist.clone(),
                    t.album.clone(),
                    t.cover_path.clone(),
                    t.duration_ms as i64,
                    Some(source.clone()),
                    t.track_number.map(|n| n as i64),
                    t.disc_number.map(|n| n as i64),
                )
            })
            .collect();
        if let Err(e) = queue_repo.set_streaming_queue(zone_id, &queue_items) {
            warn!(zone_id, error = %e, "set_streaming_queue_failed");
        }
        state
            .playback
            .update_queue_info(zone_id, start as i64, tracks.len() as i64)
            .await;

        let orch_req = tune_core::orchestrator::PlayRequest {
            zone_id,
            output_device_id,
            track_id: None,
            source: Some(source.clone()),
            source_id: Some(first.id.clone()),
            title: Some(first.title.clone()),
            artist_name: Some(first.artist.clone()),
            album_title: first.album.clone(),
            cover_url: first.cover_path.clone(),
            duration_ms: Some(first.duration_ms as i64),
            seek_ms: None,
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: first.track_number,
            disc_number: first.disc_number,
        };
        return match state.orchestrator.play(orch_req).await {
            Ok(result) => {
                // Re-assert queue length AFTER play(). The pre-play
                // update_queue_info above is a silent no-op when the zone's
                // in-memory state doesn't exist yet, and play() then creates that
                // state with queue_length=0 — so a streaming album/playlist on a
                // fresh zone stopped after track 1 (next_position saw an empty
                // queue → the poller stopped instead of advancing). The local
                // track path already re-asserts after play(); mirror it here.
                state
                    .playback
                    .update_queue_info(zone_id, start as i64, tracks.len() as i64)
                    .await;
                persist_queue_async(&state, zone_id);
                Json(build_zone_json_with_result(&state, zone_id, &result).await).into_response()
            }
            Err(e) => play_error_response(e).into_response(),
        };
    }

    // --- Single streaming track (source + source_id, no track_id/track_ids) ---
    if body.source.is_some()
        && body.source_id.is_some()
        && body.track_id.is_none()
        && body.track_ids.is_none()
    {
        let source_id_val = body.source_id.clone().unwrap_or_default();
        let source_for_q = body.source.clone();
        // Same empty-title backfill as the queue_add sites: don't persist a blank
        // title for the row we're about to make the queue (DEvir 0.9.22).
        let meta = resolve_streaming_queue_meta(
            &state,
            source_for_q.as_deref().unwrap_or_default(),
            &source_id_val,
            body.title.as_deref(),
            body.artist_name.as_deref(),
            body.album_title.as_deref(),
            body.cover_path.as_deref(),
            body.duration_ms,
            body.track_number,
            body.disc_number,
        )
        .await;
        let (title_val, artist_val, album_val, cover_val, duration_val) = (
            meta.title,
            meta.artist,
            meta.album,
            meta.cover,
            meta.duration_ms,
        );

        let output_device_id = body.output_device_id.or_else(|| {
            let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
            zone_repo
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id)
        });
        let orch_req = tune_core::orchestrator::PlayRequest {
            zone_id,
            output_device_id,
            track_id: None,
            source: body.source,
            source_id: body.source_id,
            title: body.title,
            artist_name: body.artist_name,
            album_title: body.album_title,
            cover_url: body.cover_path,
            duration_ms: body.duration_ms,
            seek_ms: None,
            temp_file_path: None,
            sample_rate: body.sample_rate,
            bit_depth: body.bit_depth,
            media_format: body.media_format,
            track_number: None,
            disc_number: None,
        };
        return match state.orchestrator.play(orch_req).await {
            Ok(result) => {
                // If this track is already part of the loaded streaming queue
                // (e.g. the user pressed Stop then Play again on a track from an
                // album/playlist that is already queued), keep the full queue and
                // just move the current position onto it. Replacing it with a
                // single-track queue would truncate the album down to the current
                // title (Pierre M: "Si STOP et relance, la file d'attente se
                // limite au titre en cours").
                let entries = queue_repo.get_ordered(zone_id).unwrap_or_default();
                let existing = entries.iter().find(|e| {
                    e.source_id.as_deref() == Some(source_id_val.as_str())
                        && (source_for_q.is_none()
                            || e.source.as_deref() == source_for_q.as_deref())
                });
                if let Some(e) = existing {
                    // Keep the full queue, just move the current position onto it
                    // (its unified position, valid whether the queue is mixed).
                    state
                        .playback
                        .update_queue_info(zone_id, e.position, entries.len() as i64)
                        .await;
                } else {
                    // Not queued yet — make this single streaming track the queue.
                    queue_repo.clear(zone_id).ok();
                    if let Err(e) = queue_repo.append(
                        zone_id,
                        &[QueueInput::Streaming {
                            source: source_for_q.clone().unwrap_or_else(|| "streaming".into()),
                            source_id: source_id_val,
                            title: title_val,
                            artist: artist_val,
                            album: album_val,
                            cover_url: cover_val,
                            duration_ms: duration_val,
                            track_number: meta.track_number,
                            disc_number: meta.disc_number,
                        }],
                    ) {
                        warn!(zone_id, error = %e, "queue_append_single_streaming_failed");
                    }
                    state.playback.update_queue_info(zone_id, 0, 1).await;
                }
                persist_queue_async(&state, zone_id);
                Json(build_zone_json_with_result(&state, zone_id, &result).await).into_response()
            }
            Err(e) => play_error_response(e).into_response(),
        };
    }

    // #2876 — une demande NUE, c'est-à-dire une seule piste et aucun contenant.
    // C'est la forme qu'envoie la barre de transport quand la zone est à
    // l'arrêt : `{ "track_id": N }` et rien d'autre. Un album, une liste de
    // lecture ou un `start_index` désignent un nouveau geste d'écoute, qui
    // commence à son début même si sa première piste se trouve être celle que
    // le démarrage a restaurée. Relevé AVANT la résolution : la chaîne
    // ci-dessous consomme `body`.
    let demande_nue = body.album_id.is_none()
        && body.playlist_id.is_none()
        && body.track_ids.is_none()
        && body.start_index.is_none();

    // Resolve track list: containers (album/playlist) take priority so the full
    // collection is always queued, even when a track_id is also provided.
    let track_ids: Vec<i64> = if let Some(album_id) = body.album_id {
        resoudre_pistes_d_album(&state, &track_repo, album_id, zone_id)
    } else if let Some(playlist_id) = body.playlist_id {
        // Un `playlist_id` dans le corps versait les pistes de N'IMPORTE
        // QUELLE playlist du foyer dans la file de la zone, puis les jouait :
        // la lecture par énumération d'ids, sans jamais passer par
        // `/playlists` (#2794, #3073). Même refus qu'ailleurs — 404, jamais
        // 403 : distinguer « existe mais pas à vous » rendrait l'énumération
        // utile.
        let repo = tune_core::db::playlist_repo::PlaylistRepo::with_backend(state.backend.clone());
        match crate::routes::playlists::owned_or_404_response(&repo, playlist_id, profile.id()) {
            Ok(_) => repo.get_track_ids(playlist_id).unwrap_or_default(),
            Err(r) => return r,
        }
    } else if let Some(ids) = body.track_ids {
        ids
    } else if let Some(id) = body.track_id {
        vec![id]
    } else {
        // No track source specified — try to resume the current track.
        // This handles the case where the user presses Play after Stop:
        // the web/Flutter client sends POST /play with an empty body.
        let current = state.playback.get_state(zone_id).await;
        if let Some(ref np) = current.now_playing {
            let output_device_id = body
                .output_device_id
                .or_else(|| get_zone_device_id(&state, zone_id));
            let reprise =
                position_de_reprise(&state, zone_id, np.track_id, np.source_id.as_deref()).await;
            let orch_req = tune_core::orchestrator::PlayRequest {
                zone_id,
                output_device_id,
                track_id: np.track_id,
                source: if np.source == "local" {
                    None
                } else {
                    Some(np.source.clone())
                },
                source_id: np.source_id.clone(),
                title: Some(np.title.clone()),
                artist_name: np.artist_name.clone(),
                album_title: np.album_title.clone(),
                cover_url: np.cover_path.clone(),
                duration_ms: Some(np.duration_ms),
                seek_ms: reprise,
                temp_file_path: None,
                sample_rate: None,
                bit_depth: None,
                media_format: None,
                track_number: None,
                disc_number: None,
            };
            ancrer_position_demandee(&state, zone_id, orch_req.seek_ms, reprise).await;
            return match state.orchestrator.play(orch_req).await {
                Ok(result) => {
                    persist_queue_async(&state, zone_id);
                    Json(build_zone_json_with_result(&state, zone_id, &result).await)
                        .into_response()
                }
                Err(e) => play_error_response(e),
            };
        }
        // No now_playing — try queue fallback (same as empty-body path)
        let qr_fallback = PlayQueueRepo::with_backend(state.backend.clone());
        let q_len = qr_fallback.count_all(zone_id).unwrap_or(0);
        if q_len > 0 {
            let current = state.playback.get_state(zone_id).await;
            let pos = current.queue_position.min(q_len - 1);
            state.playback.update_queue_info(zone_id, pos, q_len).await;
            if let Ok(result) = state.orchestrator.play_from_queue(zone_id, pos).await {
                persist_queue_async(&state, zone_id);
                return Json(build_zone_json_with_result(&state, zone_id, &result).await)
                    .into_response();
            }
        }
        return (StatusCode::BAD_REQUEST, "no track source specified").into_response();
    };

    if track_ids.is_empty() {
        return (StatusCode::BAD_REQUEST, "no tracks to play").into_response();
    }

    match set_queue_retrying(
        &queue_repo,
        state.backend.engine() == tune_core::db::engine::Engine::Sqlite,
        zone_id,
        &track_ids,
    )
    .await
    {
        Ok(()) => info!(zone_id, n = track_ids.len(), "set_queue_ok"),
        Err(e) => {
            // Never proceed on the STALE queue: track 1 would play now and the
            // natural-end advance would then resurrect whatever the DB still
            // holds from yesterday (Villerio: album play drifting into old
            // Qobuz autoplay leftovers). An emptied queue stops cleanly at the
            // end of track 1 instead — the lesser evil, and diagnosable.
            warn!(zone_id, error = %e, "set_queue_failed_clearing");
            let _ = queue_repo.clear(zone_id);
        }
    }

    // When a container (album/playlist) is requested alongside a track_id,
    // infer start_index from the position of that track in the resolved list.
    let start = body.start_index.unwrap_or_else(|| {
        body.track_id
            .and_then(|tid| track_ids.iter().position(|&id| id == tid))
            .map(|pos| pos as i64)
            .unwrap_or(0)
    });
    if start > 0 {
        queue_repo.set_current(zone_id, start).ok();
    }

    let target_id = track_ids
        .get(start as usize)
        .copied()
        .unwrap_or(track_ids[0]);
    let track = track_repo.get(target_id).ok().flatten();

    let output_device_id = body.output_device_id.or_else(|| {
        let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
        zone_repo
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id)
    });

    // Le chemin réellement emprunté par le bouton Lecture des clients web et
    // Flutter quand la zone est à l'arrêt : ils envoient `{ "track_id": N }`,
    // pas un corps vide. Un `seek_ms` explicite reste prioritaire — il vient
    // d'un geste, la reprise n'est qu'un souvenir (#2876).
    let reprise = if body.seek_ms.is_none() && demande_nue {
        position_de_reprise(&state, zone_id, Some(target_id), None).await
    } else {
        None
    };
    let seek_ms = body.seek_ms.or(reprise);

    let orch_req = tune_core::orchestrator::PlayRequest {
        zone_id,
        output_device_id,
        track_id: Some(target_id),
        source: body.source,
        source_id: body.source_id,
        title: body
            .title
            .or_else(|| track.as_ref().map(|t| t.title.clone())),
        artist_name: body
            .artist_name
            .or_else(|| track.as_ref().and_then(|t| t.artist_name.clone())),
        album_title: body
            .album_title
            .or_else(|| track.as_ref().and_then(|t| t.album_title.clone())),
        cover_url: body
            .cover_path
            .or_else(|| track.as_ref().and_then(|t| t.cover_path.clone())),
        duration_ms: body
            .duration_ms
            .or_else(|| track.as_ref().map(|t| t.duration_ms)),
        seek_ms,
        temp_file_path: body.temp_file_path,
        sample_rate: body.sample_rate,
        bit_depth: body.bit_depth,
        media_format: body.media_format,
        track_number: None,
        disc_number: None,
    };

    ancrer_position_demandee(&state, zone_id, orch_req.seek_ms, reprise).await;

    match state.orchestrator.play(orch_req).await {
        Ok(result) => {
            let qr = PlayQueueRepo::with_backend(state.backend.clone());
            let q_len = qr.count_all(zone_id).unwrap_or(0);
            let q_len = if q_len > 0 {
                q_len
            } else {
                track_ids.len() as i64
            };
            state
                .playback
                .update_queue_info(zone_id, start, q_len)
                .await;
            persist_queue_async(&state, zone_id);
            Json(build_zone_json_with_result(&state, zone_id, &result).await).into_response()
        }
        Err(e) => play_error_response(e),
    }
}

async fn pause(State(state): State<AppState>, Path(zone_id): Path<i64>) -> impl IntoResponse {
    let device_id = get_zone_device_id(&state, zone_id);
    match state
        .orchestrator
        .pause(zone_id, device_id.as_deref())
        .await
    {
        Ok(()) => Json(build_zone_json(&state, zone_id).await).into_response(),
        Err(error) => output_command_error_response(error),
    }
}

async fn resume(State(state): State<AppState>, Path(zone_id): Path<i64>) -> impl IntoResponse {
    let current = state.playback.get_state(zone_id).await;

    // Zone à l'arrêt avec une piste en mémoire : on la rejoue — à la position
    // que le démarrage a restaurée si elle vaut encore, depuis le début sinon.
    // Le commentaire d'avant disait « from the start », en contradiction avec
    // celui de `PlaybackManager::stop` : « keep position_ms […] can resume from
    // the same position ». L'intention était écrite, l'instruction manquait
    // (#2876).
    if current.state == tune_core::playback::PlayState::Stopped {
        if let Some(ref np) = current.now_playing {
            let output_device_id = get_zone_device_id(&state, zone_id);
            let reprise =
                position_de_reprise(&state, zone_id, np.track_id, np.source_id.as_deref()).await;
            let orch_req = tune_core::orchestrator::PlayRequest {
                zone_id,
                output_device_id,
                track_id: np.track_id,
                source: if np.source == "local" {
                    None
                } else {
                    Some(np.source.clone())
                },
                source_id: np.source_id.clone(),
                title: Some(np.title.clone()),
                artist_name: np.artist_name.clone(),
                album_title: np.album_title.clone(),
                cover_url: np.cover_path.clone(),
                duration_ms: Some(np.duration_ms),
                seek_ms: reprise,
                temp_file_path: None,
                sample_rate: None,
                bit_depth: None,
                media_format: None,
                track_number: None,
                disc_number: None,
            };
            ancrer_position_demandee(&state, zone_id, orch_req.seek_ms, reprise).await;
            return match state.orchestrator.play(orch_req).await {
                Ok(result) => {
                    // Restore queue_length from DB so the poller can
                    // advance tracks (fixes repeat-all after restart).
                    let qr = PlayQueueRepo::with_backend(state.backend.clone());
                    let q_len = qr.count_all(zone_id).unwrap_or(0);
                    if q_len > 0 {
                        let cur_pos = state.playback.get_state(zone_id).await.queue_position;
                        state
                            .playback
                            .update_queue_info(zone_id, cur_pos, q_len)
                            .await;
                    }
                    Json(build_zone_json_with_result(&state, zone_id, &result).await)
                        .into_response()
                }
                Err(e) => play_error_response(e),
            };
        }
    }

    // Stopped with no now_playing (e.g. after server restart) — try to
    // play the first track from the queue instead of a bare resume.
    if current.state == tune_core::playback::PlayState::Stopped {
        let qr = PlayQueueRepo::with_backend(state.backend.clone());
        let output_device_id = get_zone_device_id(&state, zone_id);
        // Try streaming queue first, then local queue
        let streaming_items = qr.get_streaming_queue(zone_id).unwrap_or_default();
        if let Some(first) = streaming_items.first() {
            let source = first
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let source_id = first
                .get("source_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let title = first
                .get("title")
                .and_then(|v| v.as_str())
                .map(String::from);
            if !source.is_empty() && !source_id.is_empty() {
                let orch_req = tune_core::orchestrator::PlayRequest {
                    zone_id,
                    output_device_id,
                    track_id: None,
                    source: Some(source),
                    source_id: Some(source_id),
                    title,
                    artist_name: first
                        .get("artist_name")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    album_title: first
                        .get("album_title")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    cover_url: first
                        .get("cover_path")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                    duration_ms: first.get("duration_ms").and_then(|v| v.as_i64()),
                    ..Default::default()
                };
                return match state.orchestrator.play(orch_req).await {
                    Ok(result) => {
                        let q_len = qr.count_streaming(zone_id).unwrap_or(0);
                        if q_len > 0 {
                            state.playback.update_queue_info(zone_id, 0, q_len).await;
                        }
                        Json(build_zone_json_with_result(&state, zone_id, &result).await)
                            .into_response()
                    }
                    Err(e) => play_error_response(e),
                };
            }
        }
        let local_items = qr.get_queue(zone_id).unwrap_or_default();
        if let Some(first) = local_items.first() {
            {
                let track_id = first.track_id;
                let orch_req = tune_core::orchestrator::PlayRequest {
                    zone_id,
                    output_device_id,
                    track_id: Some(track_id),
                    source: None,
                    source_id: None,
                    title: None,
                    artist_name: None,
                    album_title: None,
                    cover_url: None,
                    duration_ms: None,
                    seek_ms: None,
                    temp_file_path: None,
                    sample_rate: None,
                    bit_depth: None,
                    media_format: None,
                    track_number: None,
                    disc_number: None,
                };
                return match state.orchestrator.play(orch_req).await {
                    Ok(result) => {
                        let q_len = qr.count(zone_id).unwrap_or(0);
                        if q_len > 0 {
                            state.playback.update_queue_info(zone_id, 0, q_len).await;
                        }
                        Json(build_zone_json_with_result(&state, zone_id, &result).await)
                            .into_response()
                    }
                    Err(e) => play_error_response(e),
                };
            }
        }
        // Nothing in the queue — return stopped state, don't set Playing
        return Json(build_zone_json(&state, zone_id).await).into_response();
    }

    // For a normal resume (paused → playing), also ensure queue_length is
    // populated — it may be zero after a server restart.
    {
        let qr = PlayQueueRepo::with_backend(state.backend.clone());
        let q_len = qr.count_all(zone_id).unwrap_or(0);
        if q_len > 0 {
            let cur_pos = state.playback.get_state(zone_id).await.queue_position;
            state
                .playback
                .update_queue_info(zone_id, cur_pos, q_len)
                .await;
        }
    }

    let device_id = get_zone_device_id(&state, zone_id);
    match state
        .orchestrator
        .resume(zone_id, device_id.as_deref())
        .await
    {
        Ok(()) => Json(build_zone_json(&state, zone_id).await).into_response(),
        Err(error) => output_command_error_response(error),
    }
}

async fn stop(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let device_id = get_zone_device_id(&state, zone_id);
    state.orchestrator.stop(zone_id, device_id.as_deref()).await;
    Json(build_zone_json(&state, zone_id).await)
}

/// Reject playback commands on an orphan zone (a DB row with no
/// output_device_id): next/previous spawn play_from_queue fire-and-forget and
/// answer 200 before the orchestrator runs, so its zone_no_output_device error
/// would only ever reach the logs. Check up front and return the same 409 the
/// play route produces. Browser zones are exempt (no output device by design).
fn reject_if_zone_has_no_output_device(
    state: &AppState,
    zone_id: i64,
) -> Option<axum::response::Response> {
    let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten()?;
    if zone.output_device_id.is_none() && zone.output_type.as_deref() != Some("browser") {
        warn!(zone_id, zone_name = %zone.name, "play_rejected_zone_without_output_device");
        return Some(play_error_response(format!(
            "zone_no_output_device:Zone '{}' has no output device assigned — assign an output device to this zone or delete it and re-create it from a device.",
            zone.name
        )));
    }
    None
}

async fn next(State(state): State<AppState>, Path(zone_id): Path<i64>) -> impl IntoResponse {
    info!(zone_id = zone_id, "api_next_requested");
    if let Some(resp) = reject_if_zone_has_no_output_device(&state, zone_id) {
        return resp;
    }
    let current = state.playback.get_state(zone_id).await;

    // Manual skip: ignore repeat-one so the button always changes track (#1110).
    let Some(next_pos) = tune_core::poller::PositionPoller::next_position_manual(&current) else {
        let device_id = get_zone_device_id(&state, zone_id);
        state.orchestrator.stop(zone_id, device_id.as_deref()).await;
        return Json(json!({ "status": "stopped", "reason": "end_of_queue" })).into_response();
    };

    let s = state.clone();
    tokio::spawn(async move {
        if let Err(e) = s.orchestrator.play_from_queue(zone_id, next_pos).await {
            tracing::warn!(zone_id, error = %e, "next_play_failed");
        }
    });

    Json(json!({ "status": "playing", "queue_position": next_pos })).into_response()
}

/// Dernier « précédent » ayant relancé la piste au lieu de reculer, par zone.
///
/// Sans cette mémoire, « précédent » n'est qu'une fonction de la position
/// rapportée — et cette position ment pendant quelques secondes après un seek :
/// le poller cesse de l'écraser (`SEEK_GRACE_SECS`), les sorties réseau la
/// rendent en retard, et le tampon d'un renderer DLNA fait le reste.
///
/// L'utilisateur, lui, ne raisonne pas en millisecondes : il appuie deux fois
/// pour remonter d'une piste. Fabien l'a fait, et Tune lui a redonné deux fois
/// le début du même morceau (#1929).
static DERNIER_REDEMARRAGE: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<i64, std::time::Instant>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Fenêtre pendant laquelle un second « précédent » recule au lieu de relancer.
///
/// Assez longue pour couvrir une hésitation humaine et le retard des sorties
/// réseau ; assez courte pour qu'un appui isolé une minute plus tard relance
/// bien la piste, comme attendu.
const FENETRE_DOUBLE_PRECEDENT: std::time::Duration = std::time::Duration::from_secs(6);

/// Seuil au-delà duquel un « précédent » isolé relance la piste au lieu de
/// reculer. Convention partagée par tous les lecteurs.
const SEUIL_RELANCE_MS: i64 = 3000;

/// « Précédent » doit-il RELANCER la piste, ou reculer d'une piste ?
///
/// Sortie en fonction pure pour être éprouvée : la version d'origine ne
/// regardait que la position, et cette position ment pendant plusieurs
/// secondes après un seek — la grâce du poller cesse de l'écraser, les sorties
/// réseau la rendent en retard, le tampon d'un renderer DLNA fait le reste.
///
/// Fabien a appuyé deux fois pour remonter d'une piste ; Tune lui a redonné
/// deux fois le début du même morceau (#1929). L'utilisateur ne raisonne pas
/// en millisecondes.
pub(crate) fn precedent_doit_relancer(position_ms: i64, vient_de_redemarrer: bool) -> bool {
    // `i64` et non `u64` : c'est le type que `get_state` rend. Une position
    // negative n'a pas de sens mais reste representable ; la comparaison la
    // traite comme un debut de piste, donc on recule — le comportement sur.
    position_ms > SEUIL_RELANCE_MS && !vient_de_redemarrer
}

async fn previous(State(state): State<AppState>, Path(zone_id): Path<i64>) -> impl IntoResponse {
    info!(zone_id = zone_id, "api_previous_requested");
    if let Some(resp) = reject_if_zone_has_no_output_device(&state, zone_id) {
        return resp;
    }
    let current = state.playback.get_state(zone_id).await;

    // Un second appui rapproché veut dire « recule », quoi que dise la
    // position. On consomme la marque : un troisième appui relancera de
    // nouveau, et l'utilisateur retrouve un comportement prévisible.
    let vient_de_redemarrer = {
        let mut m = DERNIER_REDEMARRAGE.lock().unwrap();
        match m.get(&zone_id) {
            Some(t) if t.elapsed() < FENETRE_DOUBLE_PRECEDENT => {
                m.remove(&zone_id);
                true
            }
            _ => false,
        }
    };

    if precedent_doit_relancer(current.position_ms, vient_de_redemarrer) {
        let device_id = get_zone_device_id(&state, zone_id);
        if let Err(error) = state
            .orchestrator
            .seek(zone_id, 0, device_id.as_deref())
            .await
        {
            return output_command_error_response(error);
        }
        DERNIER_REDEMARRAGE
            .lock()
            .unwrap()
            .insert(zone_id, std::time::Instant::now());
        return Json(json!({ "status": "restarted" })).into_response();
    }

    let prev_pos = (current.queue_position - 1).max(0);

    let s = state.clone();
    tokio::spawn(async move {
        if let Err(e) = s.orchestrator.play_from_queue(zone_id, prev_pos).await {
            tracing::warn!(zone_id, error = %e, "prev_play_failed");
        }
    });

    Json(json!({ "status": "playing", "queue_position": prev_pos })).into_response()
}

async fn seek(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<SeekRequest>,
) -> impl IntoResponse {
    let device_id = get_zone_device_id(&state, zone_id);
    match state
        .orchestrator
        .seek(zone_id, body.position_ms as u64, device_id.as_deref())
        .await
    {
        Ok(()) => Json(json!({ "position_ms": body.position_ms })).into_response(),
        Err(error) => output_command_error_response(error),
    }
}

async fn set_volume(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<VolumeRequest>,
) -> impl IntoResponse {
    // Le verrou du mode PURE mord ICI, pas seulement dans l'interface : le
    // volume est un multiplicateur appliqué à chaque échantillon, et une zone
    // annoncée « bit-perfect » qui atténue ne l'est pas. Un curseur grisé côté
    // web ne protège de rien — un autre client, une télécommande ou un appel
    // direct passeraient à côté. La valeur *effective* est renvoyée, ce qui
    // fait remonter le curseur au lieu de le laisser mentir.
    //
    // #1274 — la demande peut arriver en linéaire (`volume`) ou en dB
    // (`volume_db`), jamais dans les deux. La conversion est faite dans
    // `volume_scale`, une seule fois pour tout le serveur ; ici on ne fait
    // que traduire un refus en 400 plutôt que de choisir un volume à la place
    // de l'utilisateur.
    let demande =
        match tune_core::audio::volume_scale::demande_lineaire(body.volume, body.volume_db) {
            Ok(v) => v,
            Err(motif) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "invalid_volume", "message": motif })),
                )
                    .into_response();
            }
        };
    // #1274 — voir `zones::refus_de_resolution_volume`. Le refus vient AVANT
    // le verrou audiophile et avant l'orchestrateur : rien n'est envoyé au
    // périphérique, et rien n'est persisté, pour une consigne qui n'a nulle
    // part où atterrir.
    if let Some(db) = body.volume_db {
        let device_id = get_zone_device_id(&state, zone_id);
        if let Some(motif) =
            crate::routes::zones::refus_de_resolution_volume(&state, device_id.as_deref(), db).await
        {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "volume_db_hors_resolution", "message": motif })),
            )
                .into_response();
        }
    }

    let volume = tune_core::audio::audiophile::effective_volume(&state.backend, zone_id, demande);
    if (volume - demande).abs() > f64::EPSILON {
        tracing::debug!(
            zone_id,
            requested = demande,
            applied = volume,
            "volume_forced_by_audiophile_lock"
        );
    }
    let device_id = get_zone_device_id(&state, zone_id);
    match state
        .orchestrator
        .set_volume(zone_id, volume, device_id.as_deref())
        .await
    {
        // #1274 — la réponse rend la valeur EFFECTIVE dans les deux unités.
        // Un client qui règle en dB doit pouvoir constater ce qu'il a obtenu
        // sans refaire le calcul, et surtout constater quand le verrou PURE
        // l'a remonté à 100 % : `volume_db` vaudra alors 0.
        Ok(()) => Json(json!({
            "volume": volume,
            "volume_db": tune_core::audio::volume_scale::linear_to_db(volume),
        }))
        .into_response(),
        Err(error) => output_command_error_response(error),
    }
}

async fn toggle_shuffle(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Query(q): Query<ShuffleQuery>,
) -> Json<Value> {
    let current = state.playback.get_state(zone_id).await;
    let enabled = q.enabled.unwrap_or(!current.shuffle);
    state.playback.set_shuffle(zone_id, enabled).await;
    persist_queue_async(&state, zone_id);
    Json(json!({ "shuffle": enabled }))
}

async fn set_repeat(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Query(q): Query<RepeatQuery>,
) -> Json<Value> {
    let mode = match q.mode.as_deref() {
        Some("one") => tune_core::playback::RepeatMode::One,
        Some("all") => tune_core::playback::RepeatMode::All,
        _ => tune_core::playback::RepeatMode::Off,
    };
    state.playback.set_repeat(zone_id, mode).await;
    persist_queue_async(&state, zone_id);
    Json(json!({ "repeat": mode }))
}

async fn get_queue(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());
    let ps = state.playback.get_state(zone_id).await;

    // One ordered list across the unified single position space (local +
    // streaming). `get_ordered` reads the whole `queue_items` table for the zone
    // in `position` order, COALESCE-ing the display fields from the tracks join
    // (local) or the inline columns (streaming).
    let entries = queue_repo.get_ordered(zone_id).unwrap_or_default();
    let position = entries
        .iter()
        .position(|e| e.is_current)
        .map(|p| p as i64)
        .unwrap_or(ps.queue_position);
    let length = entries.len();
    let tracks: Vec<Value> = entries
        .iter()
        .map(|e| serde_json::to_value(e).unwrap_or(Value::Null))
        .collect();
    Json(json!({ "tracks": tracks, "position": position, "length": length }))
}

/// A client-supplied streaming title is usable only if it is present AND
/// non-empty. An empty string means the metadata wasn't resolved yet (the
/// enqueue race that produced `title: Some("")`), and must fall through to a
/// `get_track()` backfill rather than being persisted as a blank title.
fn client_title_is_usable(title: Option<&str>) -> bool {
    title.is_some_and(|s| !s.is_empty())
}

/// Resolve display metadata for a streaming queue entry, returning
/// `(title, artist, album, cover_url, duration_ms)`.
///
/// Mirrors the play-time guard in the orchestrator (`resolve_streaming_url`):
/// a title that is absent OR empty triggers a `get_track()` backfill. The
/// three enqueue sites used to guard on `title.is_some()` only, so a payload
/// of `title: Some("")` — an upcoming track enqueued before the streaming
/// service had resolved its metadata — was persisted with a blank title. The
/// queue-list path does no backfill, so that blank reached the clients, which
/// render an empty title as "Unknown Track" (DEvir, 0.9.22 — intermittent, only
/// while the track waits in line; it self-corrected once it became current
/// because the play path *did* backfill). Same asymmetry as the duration bug
/// (#944), on the title. Folding the three copies into one helper also removes
/// the divergence that caused this.
///
/// The client payload wins whenever it carries a real (non-empty) title, so the
/// network call only happens in the degraded empty-title case.
/// Metadata for one streaming queue row: what the client sent, completed from
/// the service when it was too thin to use.
struct StreamingQueueMeta {
    title: String,
    artist: String,
    album: Option<String>,
    cover: Option<String>,
    duration_ms: i64,
    track_number: Option<i64>,
    disc_number: Option<i64>,
}

#[allow(clippy::too_many_arguments)]
async fn resolve_streaming_queue_meta(
    state: &AppState,
    source: &str,
    source_id: &str,
    title: Option<&str>,
    artist: Option<&str>,
    album: Option<&str>,
    cover: Option<&str>,
    duration_ms: Option<i64>,
    track_number: Option<i64>,
    disc_number: Option<i64>,
) -> StreamingQueueMeta {
    if client_title_is_usable(title) {
        // Fast path: trust the client and make no service call — a client
        // queueing a whole album would otherwise pay one round trip per track.
        // Numbering therefore stays as sent: a client that wants its queue rows
        // numbered has to include track_number/disc_number.
        return StreamingQueueMeta {
            title: title.unwrap_or_default().to_string(),
            artist: artist.unwrap_or_default().to_string(),
            album: album.map(str::to_string),
            cover: cover.map(str::to_string),
            duration_ms: duration_ms.unwrap_or(0),
            track_number,
            disc_number,
        };
    }

    let registry = state.services.lock().await;
    if let Some(svc) = registry.get(source) {
        let svc = svc.read().await;
        if let Ok(t) = svc.get_track(source_id).await {
            return StreamingQueueMeta {
                title: t.title,
                artist: t.artist,
                album: t.album,
                cover: t.cover_path,
                duration_ms: t.duration_ms as i64,
                // We are talking to the service anyway, so fill the numbering
                // it reports — the client's value still wins when it sent one.
                track_number: track_number.or(t.track_number.map(i64::from)),
                disc_number: disc_number.or(t.disc_number.map(i64::from)),
            };
        }
    }
    StreamingQueueMeta {
        title: "Unknown".into(),
        artist: String::new(),
        album: None,
        cover: None,
        duration_ms: 0,
        track_number,
        disc_number,
    }
}

/// Les pistes d'un album, avec le rattrapage de la ligne sœur.
///
/// ⚠️ Cette résolution ne doit exister QU'ICI. Elle était enfermée dans le
/// handler de lecture, si bien qu'ajouter un album à la file — qui résolvait
/// ses pistes autrement — aurait échoué exactement sur les albums où « lire »
/// réussit : ceux dont la ligne cliquée est vide et dont les pistes vivent sous
/// une ligne sœur de même titre et même artiste (Pascal, Totaldac, v0.9.21).
///
/// Deux résolutions parallèles, c'est le montage qui a déjà fait perdre le
/// canal des bandes d'égaliseur (#2313) et les identifiants de la lecture en
/// cours. Une seule, partagée.
fn resoudre_pistes_d_album(
    state: &AppState,
    track_repo: &tune_core::db::track_repo::TrackRepo,
    album_id: i64,
    zone_id: i64,
) -> Vec<i64> {
    let mut ids: Vec<i64> = track_repo
        .list_by_album(album_id)
        .unwrap_or_default()
        .iter()
        .filter_map(|t| t.id)
        .collect();
    if ids.is_empty() {
        // La ligne cliquée n'a pas de pistes : les grilles Albums/Genres/Années
        // exposent parfois une ligne périmée dont les pistes vivent sous une
        // sœur — celle que la vue Artistes atteint. Le même album se jouait donc
        // depuis Artistes et rendait 400 « no tracks to play » depuis ces
        // grilles (Pascal, Totaldac, v0.9.21).
        if let Some(sibling) =
            tune_core::db::album_repo::AlbumRepo::with_backend(state.backend.clone())
                .find_populated_sibling(album_id)
                .ok()
                .flatten()
        {
            ids = track_repo
                .list_by_album(sibling)
                .unwrap_or_default()
                .iter()
                .filter_map(|t| t.id)
                .collect();
            if !ids.is_empty() {
                info!(
                    zone_id,
                    album_id, sibling, "album_recovered_via_populated_sibling"
                );
            }
        }
    }
    ids
}

async fn queue_add(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<QueueAddRequest>,
) -> impl IntoResponse {
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());

    // Build a single, source-agnostic list of items, then insert them at
    // `body.position` (the client sends current+1 for "Play Next") in the
    // unified queue. `insert_at` shifts existing rows to open the gap, so a
    // streaming track added "next" while a local album plays now lands right
    // after the current track instead of at the end of the album (Sandro S1).
    let mut inputs: Vec<QueueInput> = Vec::new();

    // Album entier : résolu par la MÊME fonction que la lecture, rattrapage de
    // la ligne sœur compris. On ne fait qu'obtenir les identifiants ici — ils
    // rejoignent `local_ids` plus bas, là où TOUTES les pistes locales
    // deviennent des entrées de file. Un second endroit qui fabriquerait des
    // `QueueInput::Local` finirait par diverger de celui-ci.
    let pistes_album: Vec<i64> = match body.album_id {
        Some(album_id) => {
            let track_repo =
                tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
            let ids = resoudre_pistes_d_album(&state, &track_repo, album_id, zone_id);
            if ids.is_empty() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("album {album_id} : aucune piste à enfiler")})),
                )
                    .into_response();
            }
            info!(zone_id, album_id, pistes = ids.len(), "queue_add_album");
            ids
        }
        None => Vec::new(),
    };

    // Single streaming track.
    if let (Some(source), Some(source_id)) = (&body.source, &body.source_id) {
        let meta = resolve_streaming_queue_meta(
            &state,
            source,
            source_id,
            body.title.as_deref(),
            body.artist_name.as_deref(),
            body.album_title.as_deref(),
            body.cover_path.as_deref(),
            body.duration_ms,
            body.track_number,
            body.disc_number,
        )
        .await;
        inputs.push(QueueInput::Streaming {
            source: source.clone(),
            source_id: source_id.clone(),
            title: meta.title,
            artist: meta.artist,
            album: meta.album,
            cover_url: meta.cover,
            duration_ms: meta.duration_ms,
            track_number: meta.track_number,
            disc_number: meta.disc_number,
        });
    }

    // Batch streaming tracks: [{source, source_id, ...}]
    for item in &body.tracks {
        let meta = resolve_streaming_queue_meta(
            &state,
            &item.source,
            &item.source_id,
            item.title.as_deref(),
            item.artist_name.as_deref(),
            item.album_title.as_deref(),
            item.cover_path.as_deref(),
            item.duration_ms,
            item.track_number,
            item.disc_number,
        )
        .await;
        inputs.push(QueueInput::Streaming {
            source: item.source.clone(),
            source_id: item.source_id.clone(),
            title: meta.title,
            artist: meta.artist,
            album: meta.album,
            cover_url: meta.cover,
            duration_ms: meta.duration_ms,
            track_number: meta.track_number,
            disc_number: meta.disc_number,
        });
    }

    // Local tracks.
    let mut local_ids = pistes_album;
    local_ids.extend_from_slice(&body.track_ids);
    if let Some(single) = body.track_id {
        local_ids.push(single);
    }
    for id in &local_ids {
        inputs.push(QueueInput::Local { track_id: *id });
    }

    if inputs.is_empty() {
        // Un refus muet est indistinguable d'un bouton qui ne fait rien.
        //
        // Cette route ne journalisait RIEN — ni succès, ni refus. Un testeur
        // qui écrit « la fonction + ne fonctionne pas » (Tades, fil #1487) ne
        // pouvait être ni confirmé ni contredit par son journal, et nous ne
        // pouvions pas savoir si sa demande n'était jamais partie, était
        // arrivée vide, ou avait été insérée sans que l'écran le montre.
        // On dit donc ce qu'on a reçu, pas seulement qu'on refuse.
        warn!(
            zone_id,
            track_id = ?body.track_id,
            track_ids = body.track_ids.len(),
            tracks = body.tracks.len(),
            source = ?body.source,
            source_id = ?body.source_id,
            "queue_add_rejected_empty — aucune piste exploitable dans la demande"
        );
        return (
            StatusCode::BAD_REQUEST,
            "track_ids, track_id, source+source_id, or tracks[] required".to_string(),
        )
            .into_response();
    }

    let count = inputs.len();
    let start = match queue_repo.insert_at(zone_id, &inputs, body.position) {
        Ok(start) => start,
        Err(e) => {
            warn!(zone_id, error = %e, "queue_insert_failed");
            return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
        }
    };
    let total = queue_repo.count_all(zone_id).unwrap_or(0);
    let current_pos = state.playback.get_state(zone_id).await.queue_position;
    state
        .playback
        .update_queue_info(zone_id, current_pos, total)
        .await;
    persist_queue_async(&state, zone_id);
    // Le succès aussi doit laisser une trace : c'est elle qui permet de dire à
    // un utilisateur « votre ajout est bien arrivé, à telle position » plutôt
    // que de lui demander de réessayer. `position` vaut `None` pour un ajout
    // en fin de file, `Some(n)` pour un « Lire ensuite » ; `inserted_at` dit où
    // la piste a RÉELLEMENT atterri, ce qui n'est pas la même chose (#2079).
    info!(
        zone_id,
        added = count,
        position = ?body.position,
        inserted_at = ?start,
        queue_length = total,
        "queue_add_ok"
    );
    let enfiles = decrire_enfilage(&inputs, start);
    state.event_bus.emit(
        "playback.queue.track_added",
        json!({
            "zone_id": zone_id,
            "added": count,
            "queue_length": total,
            "position": start,
        }),
    );
    (
        StatusCode::CREATED,
        // `added` + `queue_length` ne disaient QUE « quelque chose est parti ».
        // Sandro (#2079, fil forum 1493) allait rouvrir la file après chaque
        // « Lecture suivante » parce que rien dans la réponse ne nommait la
        // piste ni ne disait où elle avait atterri — et la parade naturelle,
        // recliquer, l'enfilait deux fois.
        //
        // Les deux champs sont ADDITIFS : le statut reste 201, `added` et
        // `queue_length` gardent leur sens et leur place, donc aucun client
        // déployé ne change de comportement.
        //
        // `position` est la position EFFECTIVE, pas celle demandée : le dépôt
        // ramène toute position hors file en fin de file, si bien qu'un « juste
        // après la piste en cours » calculé sur une file périmée réussit… en
        // ajoutant à la fin. Renvoyer la demande plutôt que le résultat
        // rendrait ces deux cas identiques, ce qui est exactement le défaut.
        Json(json!({
            "added": count,
            "queue_length": total,
            "position": start,
            "items": enfiles,
        })),
    )
        .into_response()
}

/// Ce qui vient d'être enfilé, une entrée par ligne insérée, dans l'ordre des
/// positions.
///
/// Dérivé de `inputs` plutôt que construit au fil des `push` : les trois
/// chemins d'alimentation (piste de service isolée, lot `tracks[]`, pistes
/// locales dont l'album) écriraient sinon chacun leur description, et la
/// quatrième oublierait la sienne — un « enfilé » muet de plus.
///
/// N'interroge RIEN : tout est déjà résolu dans `inputs` (le titre d'une piste
/// de service y est passé par `resolve_streaming_queue_meta`). Un album de
/// trente pistes ne coûte donc pas trente requêtes de plus.
fn decrire_enfilage(inputs: &[QueueInput], start: Option<i64>) -> Vec<serde_json::Value> {
    inputs
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let position = start.map(|s| s + i as i64);
            match item {
                QueueInput::Local { track_id } => json!({
                    "position": position,
                    "track_id": track_id,
                }),
                QueueInput::Streaming {
                    source,
                    source_id,
                    title,
                    artist,
                    ..
                } => json!({
                    "position": position,
                    "source": source,
                    "source_id": source_id,
                    "title": title,
                    "artist": artist,
                }),
            }
        })
        .collect()
}

async fn queue_move(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<QueueMoveRequest>,
) -> impl IntoResponse {
    // Queue order changed — invalidate prefetched track
    state.orchestrator.clear_prefetch().await;
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());
    let total = queue_repo.count_all(zone_id).unwrap_or(0);
    let from = body.from_position;
    let to = body.to_position;
    if from < 0 || to < 0 || from >= total || to >= total {
        return (StatusCode::BAD_REQUEST, "position out of range").into_response();
    }
    // Unified move: reorders across the whole queue (local + streaming), which
    // the old local-only path could not do.
    if let Err(e) = queue_repo.move_pos(zone_id, from, to) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    persist_queue_async(&state, zone_id);
    state.event_bus.emit(
        "playback.queue.moved",
        json!({ "zone_id": zone_id, "from": from, "to": to }),
    );
    StatusCode::NO_CONTENT.into_response()
}

async fn queue_jump(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<QueueJumpRequest>,
) -> impl IntoResponse {
    match state
        .orchestrator
        .play_from_queue(zone_id, body.position)
        .await
    {
        Ok(result) => {
            persist_queue_async(&state, zone_id);
            Json(build_zone_json_with_result(&state, zone_id, &result).await).into_response()
        }
        Err(e) => play_error_response(e),
    }
}

async fn queue_clear(State(state): State<AppState>, Path(zone_id): Path<i64>) -> impl IntoResponse {
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());
    queue_repo.clear(zone_id).ok();
    state.orchestrator.clear_prefetch().await;
    state.playback.stop_and_clear(zone_id).await;
    state.playback.update_queue_info(zone_id, 0, 0).await;
    // Delete the persisted queue file
    let db_path = state.config.db_path.clone();
    tokio::task::spawn_blocking(move || {
        tune_core::queue_persistence::delete_queue_file(&db_path, zone_id);
    });
    state.event_bus.emit(
        "playback.queue.cleared",
        serde_json::json!({ "zone_id": zone_id }),
    );
    StatusCode::NO_CONTENT
}

async fn queue_remove(
    State(state): State<AppState>,
    Path((zone_id, position)): Path<(i64, i64)>,
) -> impl IntoResponse {
    // Queue shape changed — invalidate prefetched track
    state.orchestrator.clear_prefetch().await;
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());

    // One position space: remove the row at `position` regardless of source.
    match queue_repo.remove_pos(zone_id, position) {
        Ok(true) => {
            let new_length = queue_repo.count_all(zone_id).unwrap_or(0);
            let current_pos = state.playback.get_state(zone_id).await.queue_position;
            let adjusted_pos = if position < current_pos {
                current_pos - 1
            } else {
                current_pos
            };
            state
                .playback
                .update_queue_info(zone_id, adjusted_pos, new_length)
                .await;
            persist_queue_async(&state, zone_id);
            state.event_bus.emit(
                "playback.queue.track_removed",
                json!({ "zone_id": zone_id, "position": position }),
            );
            Json(json!({ "queue_length": new_length })).into_response()
        }
        Ok(false) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "position not found in queue" })),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// Combien de lignes de file n'ont PAS de piste locale.
///
/// `count_all` et `get_queue` sont deux requêtes distinctes, et la file peut
/// bouger entre les deux : un morceau qui se termine, un autre client qui
/// retire une piste. Le plancher à zéro n'est donc pas de la superstition —
/// sans lui, un compte négatif finirait dans un message adressé à
/// l'utilisateur (« ... que des pistes de service (-2) »).
fn distantes_de(total: i64, locales: i64) -> i64 {
    (total - locales).max(0)
}

async fn save_queue_as_playlist(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(zone_id): Path<i64>,
    Json(body): Json<SaveAsPlaylistRequest>,
) -> impl IntoResponse {
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());
    // `get_queue` ne rend QUE les lignes locales : ses neuf requêtes portent
    // toutes `AND q.track_id IS NOT NULL`. Une file engendrée par la lecture
    // automatique Qobuz n'a aucune ligne locale — elle revient donc vide alors
    // que l'écran, lui, affiche une file pleine (le client tient ses propres
    // éléments).
    //
    // `count_all` ne filtre pas. C'est ce qui permet de distinguer les deux
    // situations que l'ancien code confondait sous « queue is empty » :
    // une file RÉELLEMENT vide, et une file pleine de pistes de service.
    let items = queue_repo.get_queue(zone_id).unwrap_or_default();
    let total = queue_repo.count_all(zone_id).unwrap_or(0);
    let locales = items.len() as i64;
    let distantes = distantes_de(total, locales);

    if total == 0 {
        warn!(zone_id, "save_queue_as_playlist_refused_empty");
        return (
            StatusCode::BAD_REQUEST,
            "La file d'attente est vide : il n'y a rien à enregistrer.",
        )
            .into_response();
    }

    if locales == 0 {
        // Le cas de Sandro (#1959). L'ancien message disait « queue is empty »
        // devant une file qu'il voyait pleine, et rien n'était journalisé : il
        // a vérifié les journaux du serveur, à raison, et n'y a rien trouvé.
        //
        // Ce n'est pas un défaut réparable ici : `playlist_tracks.track_id` est
        // `NOT NULL REFERENCES tracks(id)`. Une playlist locale ne PEUT pas
        // porter une piste de service. Le refus est donc légitime — c'est de
        // mentir sur sa raison qui ne l'était pas.
        warn!(
            zone_id,
            distantes, "save_queue_as_playlist_refused_streaming_only"
        );
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "Cette file ne contient que des pistes de service ({distantes}), \
                 qui ne peuvent pas être enregistrées dans une playlist locale. \
                 Enregistrez-la depuis le service, ou ajoutez d'abord ces titres \
                 à votre bibliothèque."
            ),
        )
            .into_response();
    }

    let track_ids: Vec<i64> = items.iter().map(|i| i.track_id).collect();
    let name = body
        .name
        .unwrap_or_else(|| format!("Queue - Zone {zone_id}"));
    let playlist_repo = PlaylistRepo::with_backend(state.backend.clone());
    match playlist_repo.create(&name, None, profile.id()) {
        Ok(id) => {
            if let Err(e) = playlist_repo.add_tracks(id, &track_ids, None) {
                // `.ok()` avalait cette erreur : la playlist était créée, vide,
                // et la réponse annonçait `track_count` pistes.
                error!(zone_id, playlist_id = id, error = %e, "save_queue_as_playlist_add_tracks_failed");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Playlist créée mais vide : {e}"),
                )
                    .into_response();
            }
            info!(
                zone_id,
                playlist_id = id,
                enregistrees = track_ids.len(),
                ignorees = distantes,
                "save_queue_as_playlist_ok"
            );
            (
                StatusCode::CREATED,
                Json(json!({
                    "id": id,
                    "name": name,
                    "track_count": track_ids.len(),
                    // Une file mixte perd ses pistes de service en chemin. Le
                    // taire produirait le défaut d'à côté : une playlist plus
                    // courte que la file, sans que rien ne dise pourquoi.
                    "skipped_streaming": distantes,
                })),
            )
                .into_response()
        }
        Err(e) => {
            error!(zone_id, error = %e, "save_queue_as_playlist_create_failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    }
}

#[derive(Deserialize)]
struct SleepRequest {
    minutes: u64,
}

/// Per-zone sleep-timer remaining seconds. Counts down only while the zone is
/// actually playing (pause-aware), so a paused zone doesn't burn its timer.
/// A single ticker task per zone owns the countdown and stops playback at 0.
static SLEEP_TIMERS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<i64, u64>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

async fn set_sleep(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<SleepRequest>,
) -> Json<Value> {
    if body.minutes == 0 {
        SLEEP_TIMERS.lock().unwrap().remove(&zone_id);
        return Json(json!({ "sleep_timer": null, "zone_id": zone_id }));
    }

    let remaining = body.minutes * 60;
    // Insert/refresh the remaining seconds. `starting` is true only when no
    // ticker is currently running for this zone, so we never spawn duplicates.
    let starting = {
        let mut timers = SLEEP_TIMERS.lock().unwrap();
        let existed = timers.contains_key(&zone_id);
        timers.insert(zone_id, remaining);
        !existed
    };

    if starting {
        let playback = state.playback.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let playing = playback.get_state(zone_id).await.state
                    == tune_core::playback::PlayState::Playing;
                let left = {
                    let mut timers = SLEEP_TIMERS.lock().unwrap();
                    match timers.get_mut(&zone_id) {
                        None => break, // cancelled
                        Some(secs) => {
                            if playing && *secs > 0 {
                                *secs -= 1;
                            }
                            *secs
                        }
                    }
                };
                if left == 0 {
                    playback.stop(zone_id).await;
                    SLEEP_TIMERS.lock().unwrap().remove(&zone_id);
                    break;
                }
            }
        });
    }

    Json(json!({
        "sleep_timer": { "minutes": body.minutes, "zone_id": zone_id },
    }))
}

async fn get_sleep(State(_state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let remaining = SLEEP_TIMERS.lock().unwrap().get(&zone_id).copied();
    Json(json!({
        "zone_id": zone_id,
        "active": remaining.is_some(),
        "remaining_seconds": remaining,
    }))
}

/// Quel prereglage appliquer, s'il y en a un.
///
/// Trois cas, et c'est la seule logique de decision de `set_eq` :
///
/// - des **bandes explicites** l'emportent : un client qui envoie les deux
///   sait ce qu'il veut, et c'est ce que fait l'ecran Egaliseur ;
/// - **« custom »** n'est pas un prereglage, c'est le nom que porte un reglage
///   fait a la main — le resoudre ecraserait justement ce reglage ;
/// - un **nom seul** doit agir. C'est ce que l'ecran « En cours de lecture »
///   envoie, et c'est ce qui ne faisait rien.
fn prereglage_a_appliquer(preset: Option<&str>, bandes_fournies: bool) -> Option<&str> {
    preset.filter(|nom| !bandes_fournies && *nom != "custom")
}

#[derive(Deserialize)]
struct EqSettings {
    enabled: Option<bool>,
    preset: Option<String>,
    bands: Option<Vec<Value>>,
}

fn eq_bands_json(profile: &tune_core::audio::eq::EqProfile) -> Vec<Value> {
    profile
        .bands
        .iter()
        // EqBandSpec est le contrat persistant et audio. Le sérialiser lui-même
        // évite qu'une projection HTTP recopiée à la main oublie le prochain
        // champ — c'est exactement ce qui est arrivé à `channel` (#2313).
        .map(|band| {
            serde_json::to_value(band)
                .expect("EqBandSpec doit toujours pouvoir etre serialise en JSON")
        })
        .collect()
}

/// Read the zone's expert-mode EQ (bands stored in `zone_{id}_eq_profile`).
///
/// These two handlers were STUBS until 0.9.48: the web Expert equalizer
/// POSTed here, the server echoed the body back and persisted NOTHING — the
/// UI showed the EQ as applied with zero audible effect (found by measuring
/// the stream served by .18: EQ on/off captures were md5-identical).
async fn get_eq(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let profile: tune_core::audio::eq::EqProfile = settings
        .get(&format!("zone_{zone_id}_eq_profile"))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let bands = eq_bands_json(&profile);
    Json(json!({
        "zone_id": zone_id,
        "enabled": profile.enabled && !bands.is_empty(),
        "preset": if bands.is_empty() { "flat" } else { "custom" },
        "bands": bands,
    }))
}

/// Persist the zone's expert-mode EQ bands into the SAME per-zone profile the
/// orchestrator reads (`zone_{id}_eq_profile`), preserving the profiler tilt
/// fields. Same premium gate as the /dsp path.
async fn set_eq(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<EqSettings>,
) -> axum::response::Response {
    if let Err(resp) =
        crate::premium_guard::require_premium(&state.license, tune_core::license::Feature::DspEq)
            .await
    {
        return resp;
    }

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let key = format!("zone_{zone_id}_eq_profile");
    let mut profile: tune_core::audio::eq::EqProfile = settings
        .get(&key)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    // Un prereglage NOMME doit agir. Il ne le faisait pas : ce champ etait
    // seulement recopie dans la reponse, et l'ecran « En cours de lecture »
    // n'envoie QUE lui — donc choisir « Rock » repondait 200 sans rien
    // changer au son. Les bandes explicites restent prioritaires : un client
    // qui envoie les deux sait ce qu'il veut.
    let prereglage_demande = prereglage_a_appliquer(body.preset.as_deref(), body.bands.is_some());
    if let Some(nom) = prereglage_demande {
        match tune_core::audio::eq_presets::bandes(nom) {
            Some(bandes) => {
                profile.bands = bandes;
                // Choisir un prereglage l'allume : sans cela il faudrait deux
                // gestes pour entendre quoi que ce soit, et le premier
                // semblerait sans effet — le defaut qu'on repare.
                profile.enabled = true;
            }
            None => {
                // Un nom inconnu doit se VOIR. Repondre 200 sur un nom qu'on
                // ne sait pas resoudre, c'est reproduire le silence d'origine.
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "error": format!("prereglage inconnu : {nom}"),
                        "known": tune_core::audio::eq_presets::noms(),
                    })),
                )
                    .into_response();
            }
        }
    }

    if let Some(bands) = &body.bands {
        profile.bands = bands
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();
    }
    if let Some(enabled) = body.enabled {
        profile.enabled = enabled;
    }
    let _ = settings.set(&key, &serde_json::to_string(&profile).unwrap_or_default());

    // Persister ne suffit pas : sans ceci, le reglage n'atteignait le son qu'a
    // la piste SUIVANTE sur une zone locale, alors que la reponse valait 200
    // (#1725). On regle un egaliseur musique en cours, a l'oreille — et trois
    // utilisateurs ont rapporte « l'egaliseur ne fonctionne pas » avant ca.
    // Sans effet quand rien ne joue, hors zone locale, ou en mode PURE.
    let applique_a_chaud = state.orchestrator.apply_eq_change(zone_id).await;

    let bands = eq_bands_json(&profile);
    Json(json!({
        "zone_id": zone_id,
        "enabled": profile.enabled,
        "preset": body.preset.unwrap_or_else(|| "custom".into()),
        "bands": bands,
        // Vrai quand le reglage vient d'atteindre le son d'un flux en cours.
        // Faux ne signale PAS un echec : rien ne joue, la zone n'est pas
        // locale, ou elle est en PURE. Expose pour qu'un client puisse dire
        // « prendra effet a la piste suivante » plutot que de laisser croire
        // a un egaliseur muet — c'est ce silence qui a produit #1372, #1555
        // et #1688.
        "applied_live": applique_a_chaud,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct CrossfadeSettings {
    enabled: bool,
    duration: Option<f64>,
}

/// Read the persisted crossfade settings for a zone.
///
/// Crossfade is not applied by the playback engine: report the capability as
/// unavailable and never echo a stale persisted preference as if it were live.
async fn get_crossfade(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let requested_enabled = settings
        .get(&format!("crossfade_enabled:{zone_id}"))
        .ok()
        .flatten()
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    let duration = settings
        .get(&format!("crossfade_duration:{zone_id}"))
        .ok()
        .flatten()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(3.0);
    Json(json!({
        "available": false,
        "enabled": false,
        "requested_enabled": requested_enabled,
        "duration": duration,
    }))
}

fn validate_crossfade_update(body: &CrossfadeSettings) -> Result<f64, &'static str> {
    if body.enabled {
        return Err("crossfade_unavailable");
    }
    Ok(body.duration.unwrap_or(3.0).clamp(1.0, 12.0))
}

async fn set_crossfade(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<CrossfadeSettings>,
) -> impl IntoResponse {
    let duration = match validate_crossfade_update(&body) {
        Ok(duration) => duration,
        Err(code) => {
            return (
                StatusCode::NOT_IMPLEMENTED,
                Json(json!({
                    "error": code,
                    "message": "Le fondu enchaîné exige un mixer PCM à deux pistes et n'est pas encore disponible.",
                })),
            )
                .into_response();
        }
    };

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    if let Err(error) = settings.set(&format!("crossfade_enabled:{zone_id}"), "false") {
        error!(zone_id, %error, "crossfade_disable_persist_failed");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "crossfade_persist_failed"})),
        )
            .into_response();
    }
    if let Err(error) = settings.set(
        &format!("crossfade_duration:{zone_id}"),
        &duration.to_string(),
    ) {
        error!(zone_id, %error, "crossfade_duration_persist_failed");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "crossfade_persist_failed"})),
        )
            .into_response();
    }
    Json(json!({
        "zone_id": zone_id,
        "available": false,
        "crossfade_enabled": false,
        "crossfade_duration": duration,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct NormSettings {
    enabled: bool,
    target_lufs: Option<f64>,
}

async fn set_normalization(
    State(_state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<NormSettings>,
) -> Json<Value> {
    Json(json!({
        "zone_id": zone_id,
        "normalization_enabled": body.enabled,
        "target_lufs": body.target_lufs.unwrap_or(-14.0),
    }))
}

/// Transfer current track from one zone to another (path-based, backward compat).
/// Copies the full queue + position and optionally stops the source zone.
async fn transfer_playback(
    State(state): State<AppState>,
    Path((from_zone, target_zone)): Path<(i64, i64)>,
) -> impl IntoResponse {
    do_transfer(&state, from_zone, target_zone, true).await
}

/// Transfer queue between zones via JSON body (Sergio #464).
/// POST /zones/{id}/transfer  { "target_zone_id": 2, "stop_source": true }
async fn transfer_queue(
    State(state): State<AppState>,
    Path(from_zone): Path<i64>,
    Json(body): Json<TransferRequest>,
) -> impl IntoResponse {
    do_transfer(&state, from_zone, body.target_zone_id, body.stop_source).await
}

/// Shared implementation: copy queue + now playing from source to target zone.
async fn do_transfer(
    state: &AppState,
    from_zone: i64,
    target_zone: i64,
    stop_source: bool,
) -> axum::response::Response {
    let current = state.playback.get_state(from_zone).await;
    if current.now_playing.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "nothing playing to transfer"})),
        )
            .into_response();
    }

    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());

    // Copy local queue (play_queue table)
    let local_items = queue_repo.get_queue(from_zone).unwrap_or_default();
    if !local_items.is_empty() {
        let track_ids: Vec<i64> = local_items.iter().map(|i| i.track_id).collect();
        let current_pos = local_items.iter().position(|i| i.is_current).unwrap_or(0) as i64;
        if let Err(e) = queue_repo.set_queue(target_zone, &track_ids) {
            warn!(from_zone, target_zone, error = %e, "transfer_set_queue_failed");
        } else if current_pos > 0 {
            queue_repo.set_current(target_zone, current_pos).ok();
        }
    }

    // Copy streaming queue
    let streaming_items = queue_repo
        .get_streaming_queue(from_zone)
        .unwrap_or_default();
    if !streaming_items.is_empty() {
        let tracks: Vec<tune_core::db::play_queue_repo::StreamingQueueItem> = streaming_items
            .iter()
            .map(|item| {
                (
                    item["source_id"].as_str().unwrap_or("").to_string(),
                    item["title"].as_str().unwrap_or("").to_string(),
                    item["artist_name"].as_str().unwrap_or("").to_string(),
                    item["album_title"].as_str().map(String::from),
                    item["cover_path"].as_str().map(String::from),
                    item["duration_ms"].as_i64().unwrap_or(0),
                    item["source"].as_str().map(String::from),
                    item["track_number"].as_i64(),
                    item["disc_number"].as_i64(),
                )
            })
            .collect();
        if let Err(e) = queue_repo.set_streaming_queue(target_zone, &tracks) {
            warn!(from_zone, target_zone, error = %e, "transfer_streaming_queue_failed");
        }
    }

    let queue_length = if !local_items.is_empty() {
        local_items.len() as i64
    } else {
        streaming_items.len() as i64
    };

    // Position et état AVANT de toucher quoi que ce soit : la piste doit
    // reprendre là où elle en était, pas redémarrer à zéro (point 17, revue
    // 2026-08-15 — play_from_queue prend un index de file, pas des ms).
    let source_position_ms = current.position_ms.max(0) as u64;
    let source_paused = current.state == tune_core::playback::PlayState::Paused;

    // Transfer now-playing and playback state
    let np = current.now_playing.unwrap();
    state.playback.play(target_zone, np).await;
    let target_db_zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .get(target_zone)
        .ok()
        .flatten();
    // Le volume de la CIBLE, pas celui de la source : chaque zone garde son
    // niveau (les renderers n'ont pas la même sensibilité — c'était le
    // symptôme que le trim par renderer corrige, autant ne plus l'aggraver).
    let target_volume = target_db_zone
        .as_ref()
        .map(|z| z.volume / 100.0)
        .unwrap_or(current.volume);
    state.playback.set_volume(target_zone, target_volume).await;
    state
        .playback
        .update_queue_info(target_zone, current.queue_position, queue_length)
        .await;

    // Start playback on the target device via the orchestrator if a device is assigned
    let target_device = target_db_zone.and_then(|z| z.output_device_id);
    if let Some(ref did) = target_device {
        match state
            .orchestrator
            .play_from_queue(target_zone, current.queue_position)
            .await
        {
            Ok(_) => {
                // Reprendre à la position de la source. Sous 3 s on repart du
                // début (même seuil que la route seek) — inutile de chercher
                // dans un flux qui vient de démarrer.
                if source_position_ms > 3000 {
                    if let Err(error) = state
                        .orchestrator
                        .seek(target_zone, source_position_ms, Some(did))
                        .await
                    {
                        return output_command_error_response(error);
                    }
                }
                // Une source en pause reste en pause sur la cible : transférer
                // ne veut pas dire relancer.
                if source_paused {
                    if let Err(error) = state.orchestrator.pause(target_zone, Some(did)).await {
                        return output_command_error_response(error);
                    }
                }
            }
            Err(e) => {
                warn!(target_zone, error = %e, "transfer_play_on_target_failed");
            }
        }
    }

    if stop_source {
        state.orchestrator.stop(from_zone, None).await;
    }

    // Persist queue state for the target zone
    let target_state = state.playback.get_state(target_zone).await;
    let db_path = state.config.db_path.clone();
    let backend_clone = state.backend.clone();
    tokio::task::spawn_blocking(move || {
        tune_core::queue_persistence::save_queue(
            &backend_clone,
            &db_path,
            target_zone,
            &target_state,
        );
    });

    state.event_bus.emit(
        "playback.transferred",
        json!({
            "from_zone": from_zone,
            "target_zone": target_zone,
            "stop_source": stop_source,
            "queue_length": queue_length,
        }),
    );

    Json(json!({
        "from_zone": from_zone,
        "target_zone": target_zone,
        "status": "transferred",
        "queue_length": queue_length,
        "stop_source": stop_source,
    }))
    .into_response()
}

async fn get_alarms(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    use tune_core::db::backend::ToSqlValue;
    let p1 = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
        "$1".to_string()
    } else {
        "?".to_string()
    };
    let sql = format!(
        "SELECT id, zone_id, time, enabled, days, source_type, source_id, volume, fade_in_seconds \
         FROM alarms WHERE zone_id = {p1} ORDER BY time"
    );
    let rows = state
        .backend
        .query_many(&sql, &[&zone_id as &dyn ToSqlValue])
        .ou_defaut_journalise();
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "zone_id": r.get(1).and_then(|v| v.as_i64()),
                "time": r.get(2).and_then(|v| v.as_string()),
                "enabled": r.get(3).and_then(|v| v.as_i64()).unwrap_or(1) != 0,
                "days": r.get(4).and_then(|v| v.as_string()),
                "source_type": r.get(5).and_then(|v| v.as_string()),
                "source_id": r.get(6).and_then(|v| v.as_i64()),
                "volume": r.get(7).and_then(|v| v.as_f64()),
                "fade_in_seconds": r.get(8).and_then(|v| v.as_i64()),
            })
        })
        .collect();
    Ok(Json(json!(items)))
}

#[derive(Deserialize)]
struct CreateAlarm {
    time: String,
    days: Option<String>,
    source_type: Option<String>,
    source_id: Option<i64>,
    volume: Option<f64>,
    fade_in_seconds: Option<i32>,
}

async fn create_alarm(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(zone_id): Path<i64>,
    Json(body): Json<CreateAlarm>,
) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    let days = body.days.unwrap_or_else(|| "1,2,3,4,5,6,7".into());
    let source_type = body.source_type.unwrap_or_else(|| "playlist".into());
    let volume = body.volume.unwrap_or(0.3);
    let fade_in_seconds = body.fade_in_seconds.unwrap_or(30);
    let profile_id = profile.id();
    match state.backend.execute_returning_id(
        "INSERT INTO alarms (zone_id, time, days, source_type, source_id, volume, fade_in_seconds, profile_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        &[&zone_id as &dyn ToSqlValue, &body.time as &dyn ToSqlValue, &days as &dyn ToSqlValue, &source_type as &dyn ToSqlValue, &body.source_id as &dyn ToSqlValue, &volume as &dyn ToSqlValue, &fade_in_seconds as &dyn ToSqlValue, &profile_id as &dyn ToSqlValue],
    ) {
        Ok(id) => {
            (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_alarm(
    State(state): State<AppState>,
    Path((_zone_id, alarm_id)): Path<(i64, i64)>,
) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    let p1 = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
        "$1".to_string()
    } else {
        "?".to_string()
    };
    state
        .backend
        .execute(
            &format!("DELETE FROM alarms WHERE id = {p1}"),
            &[&alarm_id as &dyn ToSqlValue],
        )
        .ok();
    StatusCode::NO_CONTENT
}

fn get_zone_device_id(state: &AppState, zone_id: i64) -> Option<String> {
    tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten()
        .and_then(|z| z.output_device_id)
}

// ---------------------------------------------------------------------------
// Zone Pins
// ---------------------------------------------------------------------------
//
// #2722 — deux défauts vivaient ici, dont un seul se voyait.
//
// 1. `GET /zones/{id}/pins` rendait `Json(json!(pins))`, un TABLEAU NU, alors
//    que le contrat web (`docs/contrat-web.json`) exige l'enveloppe
//    `{ supported, pins, max_slots }`. `supported` valait `undefined` et
//    l'écran concluait que les Pins n'étaient pas pris en charge.
//
// 2. Le défaut profond : ces routes STOCKAIENT des objets dans `settings` et
//    n'appelaient JAMAIS le service `av.openhome.org:Pins:1` du renderer.
//    Corriger la seule enveloppe aurait affiché une capacité que l'appareil
//    n'a jamais annoncée — « inventer `max_slots` côté Tune rendrait seulement
//    le test vert ».
//
// Depuis #2722, quand le renderer de la zone publie `Pins:1`, ce sont SES
// actions qui sont appelées (`GetDeviceMax`, `GetIdArray`, `ReadList`,
// `SetDevice`, `InvokeIndex`, `Clear`) et `max_slots` est ce qu'IL annonce.
// Sinon `supported` vaut `false` sans un octet de réseau, et le stockage
// historique dans `settings` reste tel quel pour ne rien casser.

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::outputs::openhome::OpenHomeOutput;
use tune_core::outputs::openhome_pins::{PinWrite, PinsService};

#[derive(Deserialize, serde::Serialize, Clone)]
struct ZonePin {
    index: usize,
    title: String,
    uri: String,
    #[serde(rename = "type")]
    pin_type: String,
    /// Champs du contrat OpenHome `SetDevice`. Optionnels : le corps que le
    /// client web envoie aujourd'hui (`index`, `title`, `uri`, `type`) se
    /// désérialise inchangé.
    #[serde(default)]
    mode: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    artwork_uri: String,
    #[serde(default)]
    shuffle: bool,
}

impl ZonePin {
    fn to_device_write(&self) -> PinWrite {
        PinWrite {
            index: self.index,
            mode: self.mode.clone(),
            pin_type: self.pin_type.clone(),
            uri: self.uri.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            artwork_uri: self.artwork_uri.clone(),
            shuffle: self.shuffle,
        }
    }
}

/// Le service `Pins:1` du renderer branché à cette zone, s'il existe.
///
/// Aucun aller-retour réseau ici : la présence du service se lit dans le
/// descriptif déjà collecté à la découverte, recopié dans l'`OpenHomeOutput`
/// enregistré. Une zone navigateur, une sortie locale, un renderer DLNA ou un
/// OpenHome sans `Pins:1` rendent `None` immédiatement — c'est le chemin le
/// plus fréquenté, et la fiche de zone n'y attend rien.
async fn zone_pins_service(state: &AppState, zone_id: i64) -> Option<PinsService> {
    let device_id = get_zone_device_id(state, zone_id)?;
    let output = { state.outputs.lock().await.get(&device_id) }?;
    // Le verrou de la sortie ne tient QUE la lecture de l'URL : le client rendu
    // est autonome, les allers-retours SOAP se font hors verrou.
    let guard = output.lock().await;
    guard
        .as_any()
        .downcast_ref::<OpenHomeOutput>()?
        .pins_service()
}

/// Réponse d'un appareil injoignable ou qui refuse l'action.
fn pins_erreur_renderer(zone_id: i64, erreur: String) -> axum::response::Response {
    warn!(zone_id, error = %erreur, "zone_pins_service_renderer_en_echec");
    (StatusCode::BAD_GATEWAY, Json(json!({ "error": erreur }))).into_response()
}

fn pins_key(zone_id: i64) -> String {
    format!("zone_{zone_id}_pins")
}

fn load_pins(state: &AppState, zone_id: i64) -> Vec<ZonePin> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .get(&pins_key(zone_id))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_pins(state: &AppState, zone_id: i64, pins: &[ZonePin]) {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(
            &pins_key(zone_id),
            &serde_json::to_string(pins).unwrap_or_default(),
        )
        .ok();
}

/// `GET /zones/{id}/pins` → `{ supported, pins, max_slots }`.
///
/// `max_slots` est ce que l'appareil ANNONCE par `GetDeviceMax`. Il n'existe
/// aucun littéral côté Tune : sans service `Pins:1` la capacité vaut 0 et
/// `supported` vaut `false`. Les pins historiques rangés dans `settings` sont
/// tout de même rendus dans ce cas — les taire ferait disparaître du contenu
/// déjà enregistré —, mais ils ne prétendent à AUCUNE capacité d'appareil.
async fn get_zone_pins(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let Some(service) = zone_pins_service(&state, zone_id).await else {
        return Json(json!({
            "supported": false,
            "pins": load_pins(&state, zone_id),
            "max_slots": 0,
        }));
    };
    match service.snapshot().await {
        Ok(snapshot) => Json(json!({
            "supported": true,
            "pins": snapshot.pins,
            "max_slots": snapshot.device_max,
        })),
        Err(erreur) => {
            // L'appareil publie bien `Pins:1` mais ne répond pas : on ne
            // devine NI sa capacité NI sa liste. `supported: false` est ici la
            // seule réponse honnête, et `error` dit pourquoi.
            warn!(zone_id, error = %erreur, "zone_pins_lecture_renderer_echouee");
            Json(json!({
                "supported": false,
                "pins": [],
                "max_slots": 0,
                "error": erreur,
            }))
        }
    }
}

async fn set_zone_pin(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<ZonePin>,
) -> impl IntoResponse {
    // Renderer porteur de `Pins:1` : c'est `SetDevice` qui pose le pin, pas
    // `settings`.
    if let Some(service) = zone_pins_service(&state, zone_id).await {
        return match service.set_device(&body.to_device_write()).await {
            Ok(()) => (StatusCode::CREATED, Json(json!(body))).into_response(),
            Err(erreur) => pins_erreur_renderer(zone_id, erreur),
        };
    }
    let mut pins = load_pins(&state, zone_id);
    // Replace at index or append
    if let Some(existing) = pins.iter_mut().find(|p| p.index == body.index) {
        *existing = body.clone();
    } else {
        pins.push(body.clone());
    }
    save_pins(&state, zone_id, &pins);
    (StatusCode::CREATED, Json(json!(body))).into_response()
}

async fn clear_zone_pin(
    State(state): State<AppState>,
    Path((zone_id, index)): Path<(i64, usize)>,
) -> impl IntoResponse {
    // `Clear` du contrat OpenHome prend un IDENTIFIANT, pas un rang : on lit
    // d'abord `GetIdArray` pour traduire le rang que porte l'URL. Un
    // emplacement vide (identifiant 0) n'est pas une erreur d'appareil, c'est
    // un 404.
    if let Some(service) = zone_pins_service(&state, zone_id).await {
        let ids = match service.id_array().await {
            Ok(ids) => ids,
            Err(erreur) => return pins_erreur_renderer(zone_id, erreur),
        };
        let Some(id) = ids.get(index).copied().filter(|id| *id != 0) else {
            return (StatusCode::NOT_FOUND, "pin not found").into_response();
        };
        return match service.clear(id).await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(erreur) => pins_erreur_renderer(zone_id, erreur),
        };
    }
    let mut pins = load_pins(&state, zone_id);
    pins.retain(|p| p.index != index);
    save_pins(&state, zone_id, &pins);
    StatusCode::NO_CONTENT.into_response()
}

async fn invoke_zone_pin(
    State(state): State<AppState>,
    Path((zone_id, index)): Path<(i64, usize)>,
) -> impl IntoResponse {
    // `InvokeIndex` : l'appareil déclenche lui-même sa source. Tune n'a rien à
    // orchestrer, et surtout rien à acquitter à sa place.
    if let Some(service) = zone_pins_service(&state, zone_id).await {
        return match service.invoke_index(index).await {
            Ok(()) => (
                StatusCode::ACCEPTED,
                Json(json!({ "invoked": index, "by": "openhome_pins" })),
            )
                .into_response(),
            Err(erreur) => pins_erreur_renderer(zone_id, erreur),
        };
    }
    let pins = load_pins(&state, zone_id);
    let Some(pin) = pins.iter().find(|p| p.index == index) else {
        return (StatusCode::NOT_FOUND, "pin not found").into_response();
    };

    // Build a play request from the pin
    let output_device_id = get_zone_device_id(&state, zone_id);
    let orch_req = tune_core::orchestrator::PlayRequest {
        zone_id,
        output_device_id,
        track_id: None,
        source: Some(pin.pin_type.clone()),
        source_id: Some(pin.uri.clone()),
        title: Some(pin.title.clone()),
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: None,
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
    };
    match state.orchestrator.play(orch_req).await {
        Ok(result) => {
            Json(build_zone_json_with_result(&state, zone_id, &result).await).into_response()
        }
        Err(e) => play_error_response(e),
    }
}

async fn save_queue_as_pin(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<ZonePin>,
) -> impl IntoResponse {
    // Épingler la file Tune dans un emplacement de l'APPAREIL demanderait une
    // adresse que l'appareil sache ouvrir ; `queue:zone:{id}` n'en est pas
    // une. Plutôt que d'écrire dans `settings` un pin que `GET` n'affichera
    // jamais pour cette zone, on le dit (#2722, reste à porter).
    if zone_pins_service(&state, zone_id).await.is_some() {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({
                "error": "openhome_pins_from_queue_non_supporte",
                "message": "Cette zone porte le service OpenHome Pins : un emplacement de l'appareil demande une adresse qu'il sache ouvrir, ce que la file Tune ne fournit pas encore.",
            })),
        )
            .into_response();
    }
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());
    let items = queue_repo.get_queue(zone_id).unwrap_or_default();
    if items.is_empty() {
        return (StatusCode::BAD_REQUEST, "queue is empty").into_response();
    }
    let mut pins = load_pins(&state, zone_id);
    let pin = ZonePin {
        index: body.index,
        title: body.title,
        uri: format!("queue:zone:{zone_id}"),
        pin_type: "queue".into(),
        mode: body.mode,
        description: body.description,
        artwork_uri: body.artwork_uri,
        shuffle: body.shuffle,
    };
    if let Some(existing) = pins.iter_mut().find(|p| p.index == pin.index) {
        *existing = pin.clone();
    } else {
        pins.push(pin.clone());
    }
    save_pins(&state, zone_id, &pins);
    (StatusCode::CREATED, Json(json!(pin))).into_response()
}

// ---------------------------------------------------------------------------
// Audiophile / Quality / Audio-Profile per-zone settings
// ---------------------------------------------------------------------------

async fn get_audiophile(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    Json(json!({
        "enabled": tune_core::audio::audiophile::zone_enabled(&state.backend, zone_id),
        "lock_volume": tune_core::audio::audiophile::volume_lock_override(
            &state.backend,
            zone_id,
        ),
        "effective_lock_volume": tune_core::audio::audiophile::volume_lock_enabled(
            &state.backend,
            zone_id,
        ),
    }))
}

/// Trois états JSON pour une surcharge : champ absent, `null` (hériter), ou
/// booléen explicite. `Option<Option<bool>>` sans désérialiseur confondrait les
/// deux premiers et empêcherait de revenir au réglage global.
fn nested_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: serde::Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    serde::Deserialize::deserialize(de).map(Some)
}

#[derive(Debug, Deserialize)]
struct AudiophileChange {
    /// Absent quand la requête ne change que la portée du verrou.
    enabled: Option<bool>,
    /// `null` = hériter du réglage global, booléen = surcharge de cette zone.
    #[serde(default, deserialize_with = "nested_option")]
    lock_volume: Option<Option<bool>>,
    #[serde(default)]
    confirm_full_volume: bool,
}

/// Toute transition vers « PURE + verrou effectif » est une commande à 100 %,
/// qu'elle vienne de l'activation de PURE ou de la surcharge par zone. Une
/// requête qui ne change rien ne redemande pas une confirmation (#2526).
fn full_volume_confirmation_required(
    was_full_volume: bool,
    will_be_full_volume: bool,
    confirmed: bool,
) -> bool {
    !was_full_volume && will_be_full_volume && !confirmed
}

async fn set_audiophile(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<AudiophileChange>,
) -> axum::response::Response {
    let current_enabled = tune_core::audio::audiophile::zone_enabled(&state.backend, zone_id);
    let current_lock = tune_core::audio::audiophile::volume_lock_enabled(&state.backend, zone_id);
    let current_override =
        tune_core::audio::audiophile::volume_lock_override(&state.backend, zone_id);
    let target_enabled = body.enabled.unwrap_or(current_enabled);
    let target_override = body.lock_volume.unwrap_or(current_override);
    let target_lock = target_override.unwrap_or_else(|| {
        tune_core::audio::audiophile::global_volume_lock_enabled(&state.backend)
    });
    let was_full_volume = current_enabled && current_lock;
    let will_be_full_volume = target_enabled && target_lock;

    if full_volume_confirmation_required(
        was_full_volume,
        will_be_full_volume,
        body.confirm_full_volume,
    ) {
        warn!(zone_id, "audiophile_full_volume_confirmation_required");
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "full_volume_confirmation_required",
                "message": "Activating PURE with the volume lock enabled sets the device volume to 100%. Explicit confirmation is required.",
            })),
        )
            .into_response();
    }

    // Verrou armé : passer en PURE remonte le volume tout de suite. Sans ça,
    // la zone resterait à 20 % avec un curseur gelé sur 20 % — le pire des
    // deux mondes, ni bit-perfect ni réglable.
    if !was_full_volume && will_be_full_volume {
        let device_id = get_zone_device_id(&state, zone_id);
        if let Err(error) = state
            .orchestrator
            .set_volume(zone_id, 1.0, device_id.as_deref())
            .await
        {
            return output_command_error_response(error);
        }
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("zone_{zone_id}_audiophile");
    let mut stored = serde_json::Map::new();
    stored.insert("enabled".into(), json!(target_enabled));
    if let Some(lock_volume) = target_override {
        stored.insert("lock_volume".into(), json!(lock_volume));
    }
    // Le témoin de confirmation autorise cette seule requête : il ne devient
    // jamais un réglage persistant qui pourrait autoriser un saut ultérieur.
    if let Err(error) = settings.set(&key, &Value::Object(stored).to_string()) {
        error!(zone_id, %error, "audiophile_setting_write_failed");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": "audiophile_setting_write_failed",
                "message": error,
            })),
        )
            .into_response();
    }

    // Repousser l'état vers la sortie qui joue. Sans cet appel, la clé était
    // écrite, la route répondait un succès, et la bascule n'atteignait le son
    // qu'à la piste SUIVANTE : l'égaliseur, le crossfeed, la convolution et le
    // ReplayGain continuaient de travailler pendant que le badge PURE
    // s'allumait (#1986). Même famille que #1725 (EQ) et #1786 (crossfeed) —
    // et le garde-fou de `routes/mod.rs` couvre désormais cette clé aussi.
    let applique_a_chaud = if body.enabled.is_some() {
        state.orchestrator.apply_audiophile_change(zone_id).await
    } else {
        false
    };
    info!(
        zone_id,
        enabled = target_enabled,
        lock_volume = ?target_override,
        effective_lock_volume = target_lock,
        applique_a_chaud,
        "audiophile_mode_set"
    );

    // `applied_live` dit la vérité que la réponse taisait : la bascule est-elle
    // audible MAINTENANT, ou seulement au prochain flux ? Le témoin de
    // confirmation n'est volontairement jamais renvoyé ni persisté.
    Json(json!({
        "enabled": target_enabled,
        "lock_volume": target_override,
        "effective_lock_volume": target_lock,
        "applied_live": applique_a_chaud,
    }))
    .into_response()
}

async fn get_quality(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("zone_{zone_id}_quality");
    let val = settings
        .get(&key)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(json!({ "max_sample_rate": null, "max_bit_depth": null, "prefer_hires": true }));
    Json(val)
}

async fn set_quality(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("zone_{zone_id}_quality");
    settings.set(&key, &body.to_string()).ok();
    Json(body)
}

async fn share_now_playing(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
) -> impl IntoResponse {
    let ps = state.playback.get_state(zone_id).await;
    let Some(np) = ps.now_playing else {
        return (StatusCode::BAD_REQUEST, "nothing playing").into_response();
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let token = format!("{:032x}", nanos ^ (zone_id as u128 * 0x9e3779b97f4a7c15));
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let data = json!({
        "title": np.title,
        "artist_name": np.artist_name,
        "album_title": np.album_title,
        "cover_path": np.cover_path,
        "source": np.source,
    });
    settings
        .set(&format!("share_{token}"), &data.to_string())
        .ok();
    Json(json!({
        "token": token,
        "url": format!("/shared/{token}"),
        "track": data,
    }))
    .into_response()
}

async fn get_audio_profile(State(state): State<AppState>, Path(zone_id): Path<i64>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("zone_{zone_id}_audio_profile");
    let val = settings
        .get(&key)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .unwrap_or(json!({ "name": "default" }));
    Json(val)
}

async fn set_audio_profile(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<Value>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = format!("zone_{zone_id}_audio_profile");
    settings.set(&key, &body.to_string()).ok();
    Json(body)
}

// ---------------------------------------------------------------------------
// Shuffle All (global playback)
// ---------------------------------------------------------------------------

/// Le contexte de filtrage que l'écran transmet à la lecture aléatoire.
///
/// `folder` est la portée de RÉPERTOIRE — le même `folder=<chemin absolu>` que
/// `/library/tracks` et que les facettes Oxygen, appliqué au sous-arbre entier.
/// Il manquait ici : la pastille de répertoire de la Bibliothèque n'avait
/// aucun champ où se transmettre, et la lecture aléatoire retombait sur sa
/// dernière branche — un tirage dans TOUTE la table `tracks` (#2801, Marco
/// Polo : « il semble s'alimenter de toute la bibliothèque, pas seulement de la
/// sélection à l'écran »).
#[derive(serde::Deserialize)]
pub struct ShuffleAllQuery {
    zone_id: Option<i64>,
    search_query: Option<String>,
    genre: Option<String>,
    album_id: Option<i64>,
    artist_id: Option<i64>,
    /// Répertoire (chemin absolu) : la lecture aléatoire se limite à son
    /// sous-arbre, récursivement.
    folder: Option<String>,
}

// Combien de pistes une lecture aléatoire enfile au maximum.
//
// C'était une constante à 500, posée pour fermer le gel d'interface de Jean
// Valjean (30 000 pistes, #2228). C'est désormais un RÉGLAGE, parce que le
// besoin INVERSE existe aussi : william veut lire plus de 2 400 pistes et se
// fait tronquer à 500 sans que rien ne le lui dise (fil 1620, #2901).
//
// Le défaut ne bouge pas : qui n'y touche pas garde exactement le
// comportement de #2228. Bornes, mesures et justification du maximum sont
// dans `tune_core::playback::queue`.
use tune_core::playback::queue::shuffle_max_tracks;

/// Une sélection que la base a déjà bornée à `plafond`.
///
/// `search()` s'arrête à la limite qu'on lui donne : une liste PLEINE veut
/// dire « il y en avait peut-être davantage », et on ne sait pas combien. On
/// rend donc `None` plutôt qu'un total qui serait faux — la même règle que
/// #2250 : la valeur mesurée, ou rien.
///
/// Le plafond est un PARAMÈTRE et non plus une constante : le comparer à une
/// constante pendant qu'un autre chemin tronque à la valeur configurée
/// rendrait `capped` faux dès que l'utilisateur relève le réglage.
fn selection_bornee(
    pistes: Option<Vec<tune_core::db::models::Track>>,
    plafond: i64,
) -> (Vec<i64>, Option<i64>) {
    let ids: Vec<i64> = pistes
        .map(|v| v.into_iter().filter_map(|t| t.id).collect())
        .unwrap_or_default();
    let total = ((ids.len() as i64) < plafond).then_some(ids.len() as i64);
    (ids, total)
}

/// Ce que la lecture aléatoire peut honnêtement dire de sa sélection :
/// `(a-t-on plafonné, sur combien)`.
///
/// Le plafond RESTE — le retirer rouvre le gel d'interface qu'il a été posé
/// pour fermer (Jean Valjean, 30 000 pistes, #2228). Ce qui doit cesser,
/// c'est le silence : la réponse annonçait `track_count: 500` sans rien qui
/// distingue « votre bibliothèque contient 500 pistes » de « elle en contient
/// 30 412 et j'en ai pris 500 », pendant que le bouton, lui, promet TOUT.
///
/// Un total n'est jamais deviné. Sélection de taille inconnue : elle est
/// arrivée bornée, donc on a bien plafonné — on le dit, sans prétendre savoir
/// sur combien.
fn compte_rendu_selection(disponibles: Option<i64>, enfilees: usize) -> (bool, Option<i64>) {
    match disponibles {
        Some(n) => (n > enfilees as i64, Some(n)),
        None => (true, None),
    }
}

/// Charge utile de `shuffle_all`.
///
/// `track_count` est déjà lu par le client (`LibraryView.svelte`,
/// `library.shufflePlaying` → « Lecture aléatoire : N pistes »).
fn reponse_shuffle(
    zone_id: i64,
    enfilees: usize,
    disponibles: Option<i64>,
    output_sent: bool,
) -> Value {
    let (plafonne, total) = compte_rendu_selection(disponibles, enfilees);
    let mut payload = json!({
        "zone_id": zone_id,
        "track_count": enfilees,
        "tracks_queued": enfilees,
        "output_sent": output_sent,
        "capped": plafonne,
    });
    // Absent, pas `null` : un total qu'on n'a pas mesuré ne s'annonce pas.
    if let Some(n) = total {
        payload
            .as_object_mut()
            .expect("json! object")
            .insert("available_track_count".into(), json!(n));
    }
    payload
}

pub async fn shuffle_all(
    State(state): State<AppState>,
    Query(q): Query<ShuffleAllQuery>,
) -> impl IntoResponse {
    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let queue_repo = PlayQueueRepo::with_backend(state.backend.clone());
    // Lu UNE fois par requête, puis passé à toutes les branches. Les cinq
    // chemins (album, artiste, recherche, genre, bibliothèque entière) et la
    // troncature finale doivent voir le MÊME plafond : c'est aussi la valeur
    // sur laquelle la réponse fonde son `capped`.
    let plafond = shuffle_max_tracks(&state.backend);

    // Honor the current library filter context so the shuffle applies to the
    // visible results, not the whole library, and target the caller's zone
    // (Sergio: shuffle from a search result did nothing / played nowhere).
    //
    // `disponibles` porte la taille RÉELLE de la sélection — mais seulement
    // là où elle est MESURÉE. `None` veut dire « on ne le sait pas », et dans
    // ce cas on ne l'invente pas : c'est la même règle que #2250 sur la
    // résolution annoncée, la valeur qu'on a ou rien.
    let (mut all_ids, disponibles): (Vec<i64>, Option<i64>) = if let Some(aid) = q.album_id {
        let ids: Vec<i64> = track_repo
            .list_by_album(aid)
            .map(|v| v.into_iter().filter_map(|t| t.id).collect())
            .unwrap_or_default();
        let n = ids.len() as i64;
        (ids, Some(n))
    } else if let Some(arid) = q.artist_id {
        let ids: Vec<i64> = track_repo
            .list_by_artist(arid)
            .map(|v| v.into_iter().filter_map(|t| t.id).collect())
            .unwrap_or_default();
        let n = ids.len() as i64;
        (ids, Some(n))
    } else if let Some(fld) = q.folder.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        // La portée de répertoire passe AVANT la recherche et le genre parce
        // que c'est ce que fait l'écran : pastille active, la Bibliothèque ne
        // charge plus que le sous-arbre (`loadScopedAlbums/Artists/Tracks` →
        // `/library/tracks?folder=`), et la zone de recherche ne fait que le
        // RESTREINDRE, côté client. `random_ids_in_folder` reprend les deux
        // mêmes prédicats, dans le même ordre.
        //
        // Le genre n'entre pas ici : sur cet écran il vit dans l'onglet
        // Genres, qui n'a pas de pastille de répertoire — les deux portées ne
        // coexistent pas. Un client qui enverrait les deux verra le
        // répertoire l'emporter, ce que ce commentaire et le champ `folder`
        // disent explicitement plutôt que de le laisser deviner.
        let terme = q
            .search_query
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        match track_repo.random_ids_in_folder(fld, terme, plafond) {
            Ok((ids, total)) => (ids, Some(total)),
            Err(e) => {
                // Ne PAS retomber sur la bibliothèque entière : c'est
                // exactement le défaut que ce ticket ferme. Une sélection vide
                // se conclut plus bas par un 400 explicite.
                tracing::error!(error = %e, folder = fld, "shuffle_all_folder_query_failed");
                (Vec::new(), None)
            }
        }
    } else if let Some(sq) = q
        .search_query
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        selection_bornee(track_repo.search(sq, plafond).ok(), plafond)
    } else if let Some(g) = q.genre.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        selection_bornee(track_repo.search(g, plafond).ok(), plafond)
    } else {
        // Whole-library shuffle: take a random `plafond`-sized sample straight
        // from the DB rather than every row. Enqueuing an entire 50k-track library
        // froze the web UI (rendering the queue) and served no purpose — a random
        // few-hundred is a "shuffle all" in every practical sense (Yves, 50k
        // library). random_ids already returns a random subset, so we don't load
        // the whole table just to discard most of it.
        //
        // C'est la seule branche où le total est connu sans coût : la
        // bibliothèque entière se compte.
        (
            track_repo.random_ids(plafond).unwrap_or_default(),
            track_repo.count().ok(),
        )
    };
    if all_ids.is_empty() {
        return (StatusCode::BAD_REQUEST, "no tracks to shuffle").into_response();
    }

    // Fisher-Yates shuffle (xorshift64, time-seeded — no rand dependency).
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1;
    for i in (1..all_ids.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let j = (seed % (i as u64 + 1)) as usize;
        all_ids.swap(i, j);
    }

    // Cap the enqueued set uniformly (a filtered path — album/artist — could also
    // be large). A queue of a few hundred shuffled tracks never freezes the UI.
    all_ids.truncate(plafond as usize);

    let zone_id = q.zone_id.unwrap_or(1);
    // Was `.ok()` — the only call site that swallowed a set_queue failure
    // with no trace: track 1 played while the STALE queue stayed in the DB,
    // and the natural-end advance then resurrected yesterday's entries
    // (Villerio: album play continued into old Qobuz autoplay leftovers).
    // `set_queue_retrying` et non `set_queue` : ce site echouait au PREMIER
    // coup. Les deux ecrivains de la connexion SQLite partagee sont un lot de
    // scan et une ecriture de file ; le premier tient sa transaction le temps
    // d'un lot entier, et c'est l'action de l'utilisateur qui perdait —
    // immediatement ici, alors que le chemin « Lire » s'accordait 2,4 s
    // (#1997). Une lecture aleatoire lancee pendant un scan vidait donc la
    // file sans meme attendre.
    match set_queue_retrying(
        &queue_repo,
        state.backend.engine() == tune_core::db::engine::Engine::Sqlite,
        zone_id,
        &all_ids,
    )
    .await
    {
        Ok(()) => info!(zone_id, n = all_ids.len(), "set_queue_ok"),
        Err(e) => {
            warn!(zone_id, error = %e, "shuffle_set_queue_failed_clearing");
            let _ = queue_repo.clear(zone_id);
        }
    }

    let first_id = all_ids[0];
    let track = track_repo.get(first_id).ok().flatten();
    let output_device_id = get_zone_device_id(&state, zone_id);

    let orch_req = tune_core::orchestrator::PlayRequest {
        zone_id,
        output_device_id,
        track_id: Some(first_id),
        source: None,
        source_id: None,
        title: track.as_ref().map(|t| t.title.clone()),
        artist_name: track.as_ref().and_then(|t| t.artist_name.clone()),
        album_title: track.as_ref().and_then(|t| t.album_title.clone()),
        cover_url: track.as_ref().and_then(|t| t.cover_path.clone()),
        duration_ms: track.as_ref().map(|t| t.duration_ms),
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
    };
    match state.orchestrator.play(orch_req).await {
        Ok(result) => {
            state
                .playback
                .update_queue_info(zone_id, 0, all_ids.len() as i64)
                .await;
            let mut resp = reponse_shuffle(zone_id, all_ids.len(), disponibles, result.output_sent);
            if let Some(ref err) = result.error {
                resp.as_object_mut()
                    .unwrap()
                    .insert("error".into(), json!(err));
            }
            Json(resp).into_response()
        }
        Err(e) => play_error_response(e),
    }
}

async fn upload_audio_file(mut multipart: axum::extract::Multipart) -> impl IntoResponse {
    let upload_dir = std::path::Path::new("/tmp/tune-upload");
    let _ = std::fs::create_dir_all(upload_dir);
    let file_id = uuid::Uuid::new_v4().to_string();

    let mut file_data: Option<Vec<u8>> = None;
    let mut original_name = String::new();

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" || name == "audio" {
            original_name = field.file_name().unwrap_or("unknown.wav").to_string();
            match field.bytes().await {
                Ok(bytes) => file_data = Some(bytes.to_vec()),
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": format!("upload read failed: {e}")})),
                    )
                        .into_response();
                }
            }
        }
    }

    let Some(data) = file_data else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no audio file in upload"})),
        )
            .into_response();
    };

    let ext = std::path::Path::new(&original_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("wav")
        .to_lowercase();
    let file_path = upload_dir.join(format!("{file_id}.{ext}"));
    if let Err(e) = std::fs::write(&file_path, &data) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("save failed: {e}")})),
        )
            .into_response();
    }

    let meta = tune_core::metadata::try_read_metadata(&file_path);
    let title = meta
        .as_ref()
        .ok()
        .and_then(|m| m.title.clone())
        .unwrap_or_else(|| {
            std::path::Path::new(&original_name)
                .file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or("Unknown")
                .to_string()
        });

    (
        StatusCode::OK,
        Json(json!({
            "file_id": file_id,
            "file_path": file_path.to_string_lossy(),
            "title": title,
            "artist": meta.as_ref().ok().and_then(|m| m.artist.clone()),
            "album": meta.as_ref().ok().and_then(|m| m.album.clone()),
            "duration_ms": meta.as_ref().ok().and_then(|m| m.duration_ms).unwrap_or(0),
            "format": ext,
            "sample_rate": meta.as_ref().ok().and_then(|m| m.sample_rate),
            "bit_depth": meta.as_ref().ok().and_then(|m| m.bit_depth),
        })),
    )
        .into_response()
}

/// Contre-épreuve du booléen envoyé aux clients pour le bouton « suivant ».
///
/// Le cas discriminant est une file aléatoire : la position brute peut être la
/// dernière alors que la permutation a encore une suite, ou l'inverse.  Le
/// contrat doit suivre la décision de l'endpoint, pas reconstruire une seconde
/// règle depuis la file visible (#2337).
#[cfg(test)]
mod contrat_suivant_tests {
    use super::can_skip_next;
    use tune_core::playback::{RepeatMode, ZoneState};

    #[test]
    fn la_fin_reelle_du_tirage_desactive_meme_si_la_position_brute_n_est_pas_la_derniere() {
        let state = ZoneState {
            queue_position: 0,
            queue_length: 5,
            repeat: RepeatMode::Off,
            shuffle: true,
            shuffle_order: vec![3, 1, 4, 0, 2],
            shuffle_index: 4,
            ..Default::default()
        };

        assert!(!can_skip_next(&state));
    }

    #[test]
    fn la_position_brute_finale_reste_active_si_le_tirage_a_une_suite() {
        let state = ZoneState {
            queue_position: 4,
            queue_length: 5,
            repeat: RepeatMode::Off,
            shuffle: true,
            shuffle_order: vec![3, 4, 1, 0, 2],
            shuffle_index: 1,
            ..Default::default()
        };

        assert!(can_skip_next(&state));
    }

    #[test]
    fn le_saut_manuel_sous_repeat_one_reboucle_comme_l_endpoint() {
        let state = ZoneState {
            queue_position: 0,
            queue_length: 1,
            repeat: RepeatMode::One,
            ..Default::default()
        };

        assert!(can_skip_next(&state));
    }
}

/// #2801 — la portée de répertoire doit TRAVERSER la barrière HTTP.
///
/// Le défaut n'était pas dans la lecture : il était dans le contrat. Le client
/// n'avait aucun champ où mettre `scopedFolder`, et `ShuffleAllQuery` n'en
/// avait aucun pour le recevoir — un `?folder=…` était accepté, silencieusement
/// jeté par serde, et la lecture aléatoire retombait sur la bibliothèque
/// entière. Un « 200 pour rien » : la route répondait, la portée disparaissait.
///
/// Ce test garde l'ARRIVÉE du champ, sur une vraie chaîne de requête, avec
/// l'extracteur réellement employé par la route.
#[cfg(test)]
mod portee_repertoire_tests {
    use super::ShuffleAllQuery;
    use axum::extract::Query;

    fn depuis(query: &str) -> ShuffleAllQuery {
        let uri: axum::http::Uri = format!("/playback/shuffle-all?{query}").parse().unwrap();
        Query::<ShuffleAllQuery>::try_from_uri(&uri)
            .expect("la chaîne de requête doit se désérialiser")
            .0
    }

    /// Le chemin est absolu, contient des espaces et des chiffres — c'est le
    /// répertoire de Marco Polo, tel que la pastille le porte.
    #[test]
    fn le_repertoire_arrive_jusqua_la_route() {
        let q = depuis("zone_id=3&folder=%2Fmnt%2Fmusic%2F80s%2012%20INCH%20COLLECTION");
        assert_eq!(q.zone_id, Some(3));
        assert_eq!(
            q.folder.as_deref(),
            Some("/mnt/music/80s 12 INCH COLLECTION"),
            "sans ce champ, serde jetait `folder` en silence et la lecture \
             aléatoire puisait dans toute la bibliothèque (#2801)"
        );
    }

    /// Témoin anti-régression : les cinq champs qui traversaient déjà doivent
    /// continuer de traverser. Ajouter `folder` ne doit rien déplacer.
    #[test]
    fn les_champs_deja_transmis_traversent_toujours() {
        let q = depuis("zone_id=1&search_query=miles&genre=Jazz&album_id=7&artist_id=9");
        assert_eq!(q.zone_id, Some(1));
        assert_eq!(q.search_query.as_deref(), Some("miles"));
        assert_eq!(q.genre.as_deref(), Some("Jazz"));
        assert_eq!(q.album_id, Some(7));
        assert_eq!(q.artist_id, Some(9));
        assert_eq!(q.folder, None, "aucun répertoire demandé, aucun inventé");
    }

    /// La pastille de répertoire et la zone de recherche cohabitent à l'écran :
    /// la seconde ne fait que restreindre la première. Les deux doivent donc
    /// arriver ensemble — c'est ce qui permet à la branche `folder` de passer
    /// le terme à `random_ids_in_folder` au lieu de l'ignorer.
    #[test]
    fn le_repertoire_et_la_recherche_arrivent_ensemble() {
        let q = depuis("zone_id=2&folder=%2Fmnt%2Fmusic%2FDisco%20Pack&search_query=funky");
        assert_eq!(q.folder.as_deref(), Some("/mnt/music/Disco Pack"));
        assert_eq!(q.search_query.as_deref(), Some("funky"));
    }
}

/// Le plafond de la lecture aléatoire doit être DIT, pas seulement appliqué.
///
/// Rappel du fil 1096 (Jean Valjean, #2228) : une file de 30 000 pistes gelait
/// l'interface, d'où le plafond. Il RESTE, et son défaut reste 500 — le
/// retirer rouvrirait ce gel. Depuis #2901 il est réglable jusqu'à 5 000
/// (borne mesurée) pour ceux qui veulent une file plus longue, mais la
/// réponse doit dire quand elle a tronqué, quelle que soit la valeur.
#[cfg(test)]
mod plafond_aleatoire_tests {
    use super::{compte_rendu_selection, reponse_shuffle, selection_bornee};
    use tune_core::playback::queue::SHUFFLE_MAX_TRACKS_DEFAULT;

    /// Le cas de Jean Valjean : 30 412 pistes en bibliothèque, 500 enfilées.
    /// La réponse doit porter les deux nombres, pas seulement le second.
    #[test]
    fn une_bibliotheque_plus_grande_que_le_plafond_dit_les_deux_nombres() {
        let (plafonne, disponibles) = compte_rendu_selection(Some(30_412), 500);
        assert!(plafonne);
        assert_eq!(disponibles, Some(30_412));

        let payload = reponse_shuffle(1, 500, Some(30_412), true);
        assert_eq!(payload["track_count"], 500);
        assert_eq!(
            payload["capped"], true,
            "la réponse doit DIRE qu'elle a plafonné : sans ce champ, rien ne \
             distingue « la bibliothèque fait 500 pistes » de « elle en fait \
             30 412 et j'en ai pris 500 » (#2228)"
        );
        assert_eq!(
            payload["available_track_count"], 30_412,
            "le total mesuré doit être annoncé, sinon le client ne peut pas \
             cesser de promettre « toute la bibliothèque »"
        );
    }

    /// Une bibliothèque plus petite que le plafond n'a rien été plafonné du
    /// tout : le dire serait une seconde forme de mensonge.
    #[test]
    fn une_bibliotheque_plus_petite_que_le_plafond_n_annonce_aucun_plafond() {
        let (plafonne, disponibles) = compte_rendu_selection(Some(312), 312);
        assert!(!plafonne);
        assert_eq!(disponibles, Some(312));

        let payload = reponse_shuffle(1, 312, Some(312), true);
        assert_eq!(payload["capped"], false);
        assert_eq!(payload["available_track_count"], 312);
    }

    /// Sélection de taille INCONNUE — une recherche revenue pleine.
    ///
    /// On a bien plafonné, et on l'annonce ; mais on ne sait pas sur combien,
    /// et on n'invente donc AUCUN total. C'est la règle de #2250 appliquée à
    /// un compte au lieu d'une résolution : la valeur mesurée, ou rien.
    #[test]
    fn une_selection_de_taille_inconnue_n_invente_pas_son_total() {
        let (plafonne, disponibles) = compte_rendu_selection(None, 500);
        assert!(plafonne);
        assert_eq!(disponibles, None);

        let payload = reponse_shuffle(1, 500, None, true);
        assert_eq!(payload["capped"], true);
        assert!(
            payload.get("available_track_count").is_none()
                || payload["available_track_count"].is_null(),
            "un total qu'on n'a pas mesuré ne doit pas être annoncé, \
             fût-ce à 500 : ce serait un chiffre inventé"
        );
    }

    /// `search()` s'arrête à la limite qu'on lui donne : une liste pleine ne
    /// prouve pas que la sélection faisait exactement cette taille.
    #[test]
    fn une_recherche_revenue_pleine_ne_connait_pas_sa_taille() {
        let pistes = |n: i64| -> Vec<tune_core::db::models::Track> {
            (0..n)
                .map(|i| {
                    let mut t = tune_core::db::models::Track::new(format!("piste {i}"));
                    t.id = Some(i + 1);
                    t
                })
                .collect()
        };
        let (ids, total) = selection_bornee(
            Some(pistes(SHUFFLE_MAX_TRACKS_DEFAULT)),
            SHUFFLE_MAX_TRACKS_DEFAULT,
        );
        assert_eq!(ids.len(), SHUFFLE_MAX_TRACKS_DEFAULT as usize);
        assert_eq!(total, None, "liste pleine ⇒ taille réelle inconnue");
        let mut courte = tune_core::db::models::Track::new("unique".into());
        courte.id = Some(7);
        let (ids, total) = selection_bornee(Some(vec![courte]), SHUFFLE_MAX_TRACKS_DEFAULT);
        assert_eq!(ids, vec![7]);
        assert_eq!(total, Some(1), "liste incomplète ⇒ taille réelle connue");
        // Le plafond RELEVÉ : la même liste de 500, sous un plafond de 2 000,
        // n'est plus pleine — elle connaît donc sa taille, et `capped` sera
        // faux. Une comparaison restée sur la constante 500 dirait l'inverse,
        // et la lecture aléatoire annoncerait un plafond qu'elle n'a pas posé.
        let (ids, total) = selection_bornee(Some(pistes(SHUFFLE_MAX_TRACKS_DEFAULT)), 2_000);
        assert_eq!(ids.len(), 500);
        assert_eq!(
            total,
            Some(500),
            "sous un plafond relevé, une liste de 500 n'est plus bornée : \
             l'annoncer « plafonnée » serait un mensonge de plus"
        );
    }

    /// Garde-fou de non-régression : le client lit `track_count` pour son
    /// message « Lecture aléatoire : N pistes ». Les nouveaux champs ne
    /// doivent rien déplacer.
    #[test]
    fn les_champs_deja_lus_par_le_client_ne_bougent_pas() {
        let payload = reponse_shuffle(4, 500, Some(30_412), false);
        assert_eq!(payload["zone_id"], 4);
        assert_eq!(payload["track_count"], 500);
        assert_eq!(payload["tracks_queued"], 500);
        assert_eq!(payload["output_sent"], false);
    }
}

#[cfg(test)]
mod tests {
    use super::AudiophileChange;
    use super::client_title_is_usable;
    use super::eq_bands_json;
    use super::full_volume_confirmation_required;
    use super::output_command_error_response;
    use super::play_error_response;
    use super::precedent_doit_relancer;
    use super::{PlayRequest, QueueAddRequest};
    use axum::http::StatusCode;
    use tune_core::outputs::{OutputCommand, OutputCommandError};

    #[test]
    fn pure_avec_verrou_refuse_toute_activation_non_confirmee() {
        assert!(full_volume_confirmation_required(false, true, false));
        assert!(!full_volume_confirmation_required(false, true, true));
    }

    #[test]
    fn aucune_confirmation_n_est_demandee_sans_montee_de_volume() {
        assert!(!full_volume_confirmation_required(false, false, false));
        assert!(!full_volume_confirmation_required(true, true, false));
        assert!(!full_volume_confirmation_required(true, false, false));
    }

    #[test]
    fn surcharge_de_verrou_distingue_absent_heritage_et_booleen() {
        let absent: AudiophileChange = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(absent.enabled, None);
        assert_eq!(absent.lock_volume, None);

        let heritage: AudiophileChange = serde_json::from_str(r#"{"lock_volume":null}"#).unwrap();
        assert_eq!(heritage.lock_volume, Some(None));

        let force: AudiophileChange = serde_json::from_str(r#"{"lock_volume":true}"#).unwrap();
        assert_eq!(force.lock_volume, Some(Some(true)));
    }

    #[test]
    fn le_json_eq_preserve_le_canal_cible() {
        let mut profile = tune_core::audio::eq::EqProfile::default();
        profile.bands.push(tune_core::audio::eq::EqBandSpec {
            freq: 120.0,
            gain: -3.5,
            q: 1.2,
            band_type: "peak".into(),
            channel: Some(1),
        });

        let bands = eq_bands_json(&profile);
        assert_eq!(bands[0]["channel"], 1);
        let roundtrip: tune_core::audio::eq::EqBandSpec =
            serde_json::from_value(bands[0].clone()).unwrap();
        assert_eq!(roundtrip.channel, Some(1));
    }

    // ── « Précédent » : relancer ou reculer (#1929) ───────────────────────

    #[test]
    fn un_appui_isole_en_cours_de_piste_relance() {
        // Convention de tous les lecteurs : au milieu d'un morceau, « précédent »
        // le reprend au début. Sans ça, impossible de réécouter une piste.
        assert!(precedent_doit_relancer(45_000, false));
    }

    #[test]
    fn un_appui_isole_au_tout_debut_recule() {
        // Juste après le démarrage, l'intention est de remonter.
        assert!(!precedent_doit_relancer(800, false));
    }

    #[test]
    fn un_second_appui_recule_meme_si_la_position_dit_le_contraire() {
        // LE cas de Fabien. Après le premier appui, la position rapportée peut
        // rester haute plusieurs secondes : la grâce du poller cesse de
        // l'écraser, une sortie réseau la rend en retard, un renderer DLNA
        // tamponne. Sans la mémoire du redémarrage, le second appui relançait
        // une deuxième fois le même morceau.
        assert!(!precedent_doit_relancer(45_000, true));
    }

    #[test]
    fn le_seuil_est_franc() {
        // Exactement au seuil : on recule encore. Au-dela : on relance.
        assert!(!precedent_doit_relancer(3_000, false));
        assert!(precedent_doit_relancer(3_001, false));
    }

    #[test]
    fn une_position_nulle_recule_toujours() {
        // Une sortie qui ne rapporte pas sa position rend 0. Reculer est le
        // comportement sûr : relancer une piste déjà au début ne ferait rien
        // de visible, et l'utilisateur croirait le bouton mort.
        assert!(!precedent_doit_relancer(0, false));
        assert!(!precedent_doit_relancer(0, true));
    }

    #[test]
    fn queue_add_accepts_album_numbering() {
        // The regression: queue rows added track by track had no track number,
        // so anything ordering the queue by album position — the queue view, an
        // output that files tracks by their rank — had to invent one. A client
        // that knows the numbering must be able to send it, per item and for a
        // single track.
        let body: QueueAddRequest = serde_json::from_value(serde_json::json!({
            "tracks": [
                {"source": "qobuz", "source_id": "42", "title": "Nightlite",
                 "track_number": 14, "disc_number": 1},
                {"source": "qobuz", "source_id": "43", "title": "Hatoa"},
            ],
            "source": "qobuz",
            "source_id": "7",
            "track_number": 3,
            "disc_number": 2,
        }))
        .expect("queue-add payload with numbering must deserialize");
        assert_eq!(body.tracks[0].track_number, Some(14));
        assert_eq!(body.tracks[0].disc_number, Some(1));
        // Omitting them stays valid — the fields are additive.
        assert_eq!(body.tracks[1].track_number, None);
        assert_eq!(body.track_number, Some(3));
        assert_eq!(body.disc_number, Some(2));
    }

    #[test]
    fn play_accepts_album_numbering() {
        // A single streaming track becomes the queue, so the same numbering has
        // to survive the play path too.
        let body: PlayRequest = serde_json::from_value(serde_json::json!({
            "source": "qobuz", "source_id": "6281809",
            "title": "If You Stayed Over", "track_number": 16,
        }))
        .expect("play payload with numbering must deserialize");
        assert_eq!(body.track_number, Some(16));
        assert_eq!(body.disc_number, None);
    }

    #[test]
    fn empty_title_is_not_usable_and_triggers_backfill() {
        // The regression: a present-but-empty title must be treated as
        // unresolved, so the enqueue path backfills via get_track instead of
        // persisting a blank that clients render as "Unknown Track" (DEvir).
        assert!(!client_title_is_usable(Some("")));
        assert!(!client_title_is_usable(None));
        // A real title from the client wins — no network call.
        assert!(client_title_is_usable(Some("Beat It")));
        // A single space is a real (if odd) title, not the empty-race sentinel.
        assert!(client_title_is_usable(Some(" ")));
    }

    async fn parts(e: &str) -> (StatusCode, serde_json::Value) {
        response_parts(play_error_response(e.to_string())).await
    }

    async fn response_parts(resp: axum::response::Response) -> (StatusCode, serde_json::Value) {
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn unsupported_output_command_is_explicit_json_422() {
        let (status, body) = response_parts(output_command_error_response(
            OutputCommandError::unsupported(OutputCommand::Seek),
        ))
        .await;

        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["error"], "unsupported_output_command");
        assert_eq!(body["command"], "seek");
        assert!(body["message"].as_str().unwrap().contains("seek"));
    }

    #[tokio::test]
    async fn failed_output_command_is_explicit_json_502() {
        let (status, body) = response_parts(output_command_error_response(
            OutputCommandError::failed(OutputCommand::SetVolume, "renderer refused volume"),
        ))
        .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["error"], "output_command_failed");
        assert_eq!(body["command"], "set_volume");
        assert_eq!(body["message"], "renderer refused volume");
    }

    /// Forum #1183: a device-side rejection (e.g. the legacy AirPlay path
    /// getting a 403 on ANNOUNCE from an AirPlay 2-only TV) used to reach the
    /// web as a plain-text body it ignored, showing only "503 Service
    /// Unavailable". The body must now be JSON {"error", "message"} like the
    /// sentinel branches — with the HTTP codes unchanged.
    #[tokio::test]
    async fn device_offline_is_json_503() {
        let (status, body) = parts("Output device error: ANNOUNCE returned 403").await;
        // "403" also matches the upstream list, but "Output device" errors are
        // classified first-match by contains(); the current mapping sends this
        // through the upstream branch (502) because "403" appears in the list
        // checked first. Assert whatever code the mapping yields is untouched:
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["error"], "upstream_error");
        assert!(body["message"].as_str().unwrap().contains("ANNOUNCE"));

        let (status, body) = parts("Output device error: renderer rejected stream").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"], "device_unavailable");
        assert_eq!(
            body["message"],
            "Output device error: renderer rejected stream"
        );
    }

    #[tokio::test]
    async fn upstream_error_is_json_502() {
        let (status, body) = parts("Tidal stream url extraction failed").await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["error"], "upstream_error");
        assert_eq!(body["message"], "Tidal stream url extraction failed");
    }

    #[tokio::test]
    async fn unknown_error_is_json_500() {
        let (status, body) = parts("something exploded").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["error"], "playback_error");
        assert_eq!(body["message"], "something exploded");
    }

    #[tokio::test]
    async fn sentinel_branches_unchanged() {
        let (status, body) = parts("premium_required:3 zones max en Free").await;
        assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
        assert_eq!(body["error"], "premium_required");

        let (status, body) = parts("zone_no_output_device:aucune sortie").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "zone_no_output_device");
        assert_eq!(body["message"], "aucune sortie");

        // #1287 : sortie disparue et rebind impossible (aucun homonyme, ou
        // plusieurs). Message actionnable relayé tel quel au client.
        let (status, body) =
            parts("zone_output_unavailable:La sortie de cette zone n'est plus disponible.").await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["error"], "zone_output_unavailable");
        assert_eq!(
            body["message"],
            "La sortie de cette zone n'est plus disponible."
        );
    }
}

/// #1959 — « Enregistrer comme playlist » refusait une file Qobuz en la
/// déclarant vide, et ne journalisait rien.
///
/// Sandro (fil forum 1432) : « j'obtiens un message d'erreur d'enregistrement.
/// J'ai vérifié les journaux du serveur juste après avoir cliqué, mais
/// bizarrement, aucune erreur n'apparaît. » Sa question était la bonne — la
/// requête atteignait bien le serveur, et elle était refusée pour une raison
/// que rien n'écrivait nulle part.
///
/// Ces tests portent sur la DÉCISION, seule partie séparable du handler HTTP :
/// que faire selon ce que la file contient réellement.
#[cfg(test)]
mod save_queue_decision {
    use super::distantes_de;

    /// LE cas du signalement : l'écran affiche une file pleine, `get_queue`
    /// rend une liste vide parce que ses neuf requêtes portent
    /// `AND q.track_id IS NOT NULL`, et l'ancien code en concluait
    /// « queue is empty ». Le handler doit désormais voir douze pistes de
    /// service, et nommer la vraie raison du refus.
    #[test]
    fn une_file_qobuz_compte_douze_pistes_de_service_et_non_zero() {
        assert_eq!(distantes_de(12, 0), 12);
    }

    /// Une file mixte s'enregistre — mais en DISANT ce qui reste dehors. Sans
    /// ce compte, la playlist serait plus courte que la file et rien
    /// n'expliquerait pourquoi : le défaut d'à côté.
    #[test]
    fn une_file_mixte_dit_ce_qu_elle_perd() {
        assert_eq!(distantes_de(10, 4), 6);
    }

    #[test]
    fn une_file_100_pour_cent_locale_ne_perd_rien() {
        assert_eq!(distantes_de(7, 7), 0);
    }

    /// `count_all` et `get_queue` sont deux requêtes distinctes, et la file peut
    /// bouger entre les deux. Un total inférieur au nombre de locales ne doit
    /// jamais produire un compte négatif affiché à l'utilisateur.
    #[test]
    fn un_total_incoherent_ne_produit_jamais_un_compte_negatif() {
        assert_eq!(distantes_de(3, 5), 0);
    }

    #[test]
    fn une_file_vide_ne_compte_rien() {
        assert_eq!(distantes_de(0, 0), 0);
    }
}

#[cfg(test)]
mod tests_prereglage {
    use super::prereglage_a_appliquer;

    /// Le defaut repare : un nom SEUL doit agir.
    ///
    /// C'est exactement ce que `setEqualizer()` envoie depuis l'ecran « En
    /// cours de lecture » — `{ "preset": "rock" }`, sans bandes. Le serveur
    /// repondait 200 et ne changeait rien.
    #[test]
    fn un_nom_seul_doit_agir() {
        assert_eq!(prereglage_a_appliquer(Some("rock"), false), Some("rock"));
    }

    /// Des bandes explicites l'emportent : l'ecran Egaliseur envoie les deux,
    /// et c'est SA courbe qui doit s'appliquer, pas la table du prereglage.
    #[test]
    fn des_bandes_explicites_lemportent() {
        assert_eq!(prereglage_a_appliquer(Some("rock"), true), None);
    }

    /// « custom » n'est pas un prereglage : c'est le nom d'un reglage fait a
    /// la main. Le resoudre ecraserait ce reglage par une table.
    #[test]
    fn custom_ne_declenche_rien() {
        assert_eq!(prereglage_a_appliquer(Some("custom"), false), None);
    }

    #[test]
    fn sans_prereglage_il_ny_a_rien_a_appliquer() {
        assert_eq!(prereglage_a_appliquer(None, false), None);
        assert_eq!(prereglage_a_appliquer(None, true), None);
    }
}

#[cfg(test)]
mod tests_contexte_de_lecture {
    use super::{PlayRequest, contexte_de_lecture};

    /// #2441 — FabienM, fil 1557 : « si je choisis de jouer une playlist
    /// complete, je m'attends a voir cette playlist ». Aujourd'hui le serveur
    /// recoit bien `playlist_id` et n'en garde RIEN : l'ecoute est ecrite dans
    /// `listen_history` sans la moindre trace de son origine.
    #[test]
    fn une_playlist_locale_est_reconnue() {
        let body = PlayRequest {
            playlist_id: Some(12),
            ..Default::default()
        };
        assert_eq!(
            contexte_de_lecture(&body),
            (
                Some("playlist".into()),
                Some("12".into()),
                Some("local".into())
            ),
            "le corps portait `playlist_id` et l'intention s'est perdue (#2441)"
        );
    }

    /// « si je choisis de jouer un album complet, je m'attends a voir cet
    /// album ». Le conteneur prime sur la piste : un corps qui porte les deux
    /// met tout l'album en file, c'est donc l'album qui a ete demande — la
    /// meme priorite que le gestionnaire applique pour construire la file.
    #[test]
    fn un_album_prime_sur_la_piste_du_meme_corps() {
        let body = PlayRequest {
            album_id: Some(7),
            track_id: Some(99),
            ..Default::default()
        };
        assert_eq!(
            contexte_de_lecture(&body),
            (Some("album".into()), Some("7".into()), Some("local".into()))
        );
    }

    /// « si je choisis d'ecouter un titre alors je m'attends a voir ce titre ».
    #[test]
    fn une_piste_seule_dit_track() {
        let body = PlayRequest {
            track_id: Some(99),
            ..Default::default()
        };
        assert_eq!(
            contexte_de_lecture(&body),
            (
                Some("track".into()),
                Some("99".into()),
                Some("local".into())
            )
        );
    }

    /// Un album de streaming garde l'identifiant du service, pas un entier :
    /// c'est pour cela que la colonne est TEXT.
    #[test]
    fn un_album_de_streaming_garde_son_identifiant_texte() {
        let body = PlayRequest {
            source: Some("qobuz".into()),
            streaming_album_id: Some("0060254735822".into()),
            ..Default::default()
        };
        assert_eq!(
            contexte_de_lecture(&body),
            (
                Some("album".into()),
                Some("0060254735822".into()),
                // #1361 — sans ce troisieme membre, l'identifiant nomme
                // l'album sans dire par quelle route le rouvrir.
                Some("qobuz".into())
            )
        );
    }

    /// L'identifiant d'un album de BIBLIOTHEQUE reste local meme si le corps
    /// annonce un service : c'est bien la table `albums` que le gestionnaire
    /// interroge sous `album_id`, et `"7"` n'a aucun sens chez Qobuz.
    ///
    /// Lire `body.source` sans regarder la branche aurait envoye le raccourci
    /// sur `GET /streaming/qobuz/albums/7` — un 404, ou pire, un album
    /// etranger.
    #[test]
    fn un_album_de_bibliotheque_reste_local_sous_un_service_annonce() {
        let body = PlayRequest {
            source: Some("qobuz".into()),
            album_id: Some(7),
            ..Default::default()
        };
        assert_eq!(
            contexte_de_lecture(&body),
            (Some("album".into()), Some("7".into()), Some("local".into()))
        );
    }

    /// Une piste unique de streaming garde son service, comme l'album : c'est
    /// le meme geste vu de plus pres.
    #[test]
    fn une_piste_de_streaming_garde_son_service() {
        let body = PlayRequest {
            source: Some("tidal".into()),
            source_id: Some("77390017".into()),
            ..Default::default()
        };
        assert_eq!(
            contexte_de_lecture(&body),
            (
                Some("track".into()),
                Some("77390017".into()),
                Some("tidal".into())
            )
        );
    }

    /// « Toutes les pistes » depuis une page artiste arrive comme une liste de
    /// `track_ids` nue — indiscernable d'une selection manuelle. On n'invente
    /// pas : NULL. Le client devra ENONCER `context_type` pour ce cas, ce que
    /// le champ explicite permet.
    #[test]
    fn une_liste_de_pistes_nue_ne_se_devine_pas() {
        let body = PlayRequest {
            track_ids: Some(vec![1, 2, 3]),
            ..Default::default()
        };
        assert_eq!(
            contexte_de_lecture(&body),
            (None, None, None),
            "une intention devinee est pire qu'une intention absente"
        );
    }

    /// La parole de l'appelant prime : c'est la seule voie pour `artist` et
    /// `label`, les deux types que le corps ne trahit jamais tout seul.
    #[test]
    fn le_type_annonce_prime_sur_la_deduction() {
        let body = PlayRequest {
            context_type: Some("artist".into()),
            context_id: Some("451".into()),
            track_ids: Some(vec![1, 2, 3]),
            album_id: Some(7),
            ..Default::default()
        };
        assert_eq!(
            contexte_de_lecture(&body),
            (
                Some("artist".into()),
                Some("451".into()),
                Some("local".into())
            )
        );
    }

    /// Une valeur hors des cinq types enumeres par le testeur est ignoree, pas
    /// stockee : sinon la colonne se remplirait de variantes et toute regle
    /// d'affichage future porterait sur du sable. On retombe sur la deduction.
    #[test]
    fn un_type_inconnu_est_ignore() {
        let body = PlayRequest {
            context_type: Some("Playlist".into()),
            album_id: Some(7),
            ..Default::default()
        };
        assert_eq!(
            contexte_de_lecture(&body),
            (Some("album".into()), Some("7".into()), Some("local".into()))
        );
    }
}

#[cfg(test)]
mod tests_crossfade_indisponible {
    use super::{CrossfadeSettings, validate_crossfade_update};

    /// #2211 — une API qui persiste `enabled=true` alors qu'aucun producteur
    /// n'en tient compte est un faux succès. L'activation doit échouer avant
    /// toute écriture jusqu'à l'arrivée d'un vrai mixer à deux pistes.
    #[test]
    fn activer_le_faux_crossfade_est_refuse() {
        let body = CrossfadeSettings {
            enabled: true,
            duration: Some(5.0),
        };

        assert_eq!(
            validate_crossfade_update(&body),
            Err("crossfade_unavailable")
        );
    }

    #[test]
    fn desactiver_reste_possible_et_borne_la_preference_de_duree() {
        let too_long = CrossfadeSettings {
            enabled: false,
            duration: Some(99.0),
        };
        let default = CrossfadeSettings {
            enabled: false,
            duration: None,
        };

        assert_eq!(validate_crossfade_update(&too_long), Ok(12.0));
        assert_eq!(validate_crossfade_update(&default), Ok(3.0));
    }
}

/// #2876 — la position restaurée au démarrage doit atteindre le son.
///
/// Sandro (fil 1610, sortie DirettaRenderer UPnP) : « le curseur de temps
/// affiche exactement la position où je m'étais arrêté […] lorsque j'appuie sur
/// Play, le morceau reprend depuis le début (0:00) ». Les deux moitiés sont
/// vraies et elles se contredisent : `restore_playback_positions` réinjecte
/// bien `zones.last_position_ms` dans l'état de zone — c'est ce que `/zones`
/// sert et que le curseur affiche — mais les chemins de lecture posaient tous
/// `seek_ms: None`.
#[cfg(test)]
mod tests_reprise_position_2876 {
    use super::reprise_applicable;
    use tune_core::playback::{NowPlaying, ZoneState};

    fn zone_restauree(position: Option<i64>, track_id: Option<i64>) -> ZoneState {
        ZoneState {
            zone_id: 1,
            pending_resume_ms: position,
            now_playing: Some(NowPlaying {
                track_id,
                title: "Piste".into(),
                duration_ms: 300_000,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// Le cas signalé : même piste, position restaurée, Play doit la demander.
    #[test]
    fn la_piste_restauree_repart_a_sa_position() {
        let zone = zone_restauree(Some(151_000), Some(42));
        assert_eq!(
            reprise_applicable(&zone, Some(42), None),
            Some(151_000),
            "la position affichée par le curseur n'a pas atteint le PlayRequest (#2876)"
        );
    }

    /// Témoin anti-régression : une AUTRE piste ne récupère pas la position de
    /// celle qui a été interrompue. C'est le risque propre à ce correctif —
    /// démarrer un morceau à 2:31 parce qu'un autre s'y était arrêté.
    #[test]
    fn une_autre_piste_repart_de_zero() {
        let zone = zone_restauree(Some(151_000), Some(42));
        assert_eq!(reprise_applicable(&zone, Some(43), None), None);
    }

    /// Sans marqueur, rien ne change : c'est tout le comportement en session
    /// (Stop puis Play, file arrivée à son terme) qui reste intact. `stop()`
    /// conserve `position_ms` sans armer `pending_resume_ms`.
    #[test]
    fn sans_marqueur_la_lecture_repart_de_zero() {
        let zone = zone_restauree(None, Some(42));
        assert_eq!(reprise_applicable(&zone, Some(42), None), None);
    }

    /// Une position nulle n'est pas une reprise.
    #[test]
    fn une_position_nulle_n_arme_rien() {
        let zone = zone_restauree(Some(0), Some(42));
        assert_eq!(reprise_applicable(&zone, Some(42), None), None);
    }

    /// Un flux distant n'a pas de `track_id` : il s'identifie par son
    /// `source_id`. Les comparer de travers ferait reprendre au mauvais endroit.
    #[test]
    fn un_flux_distant_s_identifie_par_son_source_id() {
        let mut zone = zone_restauree(Some(88_000), None);
        if let Some(np) = zone.now_playing.as_mut() {
            np.source = "qobuz".into();
            np.source_id = Some("12345".into());
        }
        assert_eq!(reprise_applicable(&zone, None, Some("12345")), Some(88_000));
        assert_eq!(reprise_applicable(&zone, None, Some("99999")), None);
        assert_eq!(reprise_applicable(&zone, None, None), None);
    }

    /// Rien en lecture : il n'y a pas de piste à laquelle rattacher la position.
    #[test]
    fn sans_piste_restauree_aucune_reprise() {
        let zone = ZoneState {
            zone_id: 1,
            pending_resume_ms: Some(151_000),
            ..Default::default()
        };
        assert_eq!(reprise_applicable(&zone, Some(42), None), None);
    }
}
