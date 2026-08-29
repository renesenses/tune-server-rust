use axum::Json;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::state::AppState;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::track_repo::TrackRepo;

use super::artwork_cache_dir;

fn is_hex_hash(s: &str) -> bool {
    (s.len() == 32 || s.len() == 64) && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[derive(Deserialize)]
pub(super) struct ProxyQuery {
    url: String,
}

pub(super) async fn serve_artwork(Path(hash): Path<String>) -> impl IntoResponse {
    serve_artwork_from(&artwork_cache_dir(), &hash).await
}

/// Sert une entrée du cache de pochettes, répertoire donné.
///
/// Séparée de [`serve_artwork`] pour être éprouvée sans variable
/// d'environnement : `artwork_cache_dir()` lit `TUNE_ARTWORK_DIR`, qui est
/// commun à tout le processus et donc inutilisable depuis des tests parallèles.
///
/// La liste des extensions cherchées n'est plus écrite ici : c'est
/// [`tune_core::library::artwork::CACHE_EXTENSIONS`], la même que celle sous
/// laquelle l'écriture dépose ses fichiers. Deux listes séparées, c'était la
/// porte ouverte à un condensat annoncé en base et introuvable ici (#2567).
async fn serve_artwork_from(cache_dir: &std::path::Path, hash: &str) -> axum::response::Response {
    if let Some((path, mime)) = tune_core::library::artwork::find_cached(cache_dir, hash)
        && let Ok(data) = tokio::fs::read(&path).await
    {
        let mut headers = HeaderMap::new();
        headers.insert("Content-Type", HeaderValue::from_static(mime));
        headers.insert(
            "Cache-Control",
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
        headers.insert(
            "ETag",
            HeaderValue::from_str(&format!("\"{hash}\""))
                .unwrap_or(HeaderValue::from_static("\"artwork\"")),
        );
        return (StatusCode::OK, headers, data).into_response();
    }
    // Une pochette absente n'existait jusqu'ici que dans la console du testeur :
    // la route ne journalisait ni succès ni échec, et un 404 de pochette était
    // invisible côté serveur (#2567). Le condensat suffit à retrouver l'album
    // (`SELECT id FROM albums WHERE cover_path = …`).
    tracing::warn!(
        hash = %hash,
        cache_dir = %cache_dir.display(),
        "artwork_cache_miss — condensat annoncé sans fichier servable"
    );
    StatusCode::NOT_FOUND.into_response()
}

pub(super) async fn album_artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let album = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    if let Some(ref cover_path) = album.cover_path {
        if cover_path.starts_with("http") {
            return axum::response::Redirect::temporary(cover_path).into_response();
        }
        let hash = if is_hex_hash(cover_path) {
            cover_path.to_string()
        } else {
            tune_core::library::artwork::artwork_hash(cover_path)
        };
        return axum::response::Redirect::temporary(&format!("/api/v1/library/artwork/{hash}"))
            .into_response();
    }

    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let tracks = track_repo.list_by_album(id).unwrap_or_default();
    if let Some(track) = tracks.first()
        && let Some(ref file_path) = track.file_path
    {
        let cache_dir = artwork_cache_dir();
        if let Some(hash) =
            tune_core::library::artwork::get_or_extract(std::path::Path::new(file_path), &cache_dir)
        {
            return axum::response::Redirect::temporary(&format!("/api/v1/library/artwork/{hash}"))
                .into_response();
        }
    }

    StatusCode::NOT_FOUND.into_response()
}

pub(super) async fn upload_album_artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    if album_repo.get(id).ok().flatten().is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "album not found"})),
        )
            .into_response();
    }

    let mut image_data: Option<Vec<u8>> = None;
    let mut ext = "jpg".to_string();
    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "image" || name == "file" || name == "artwork" {
            if let Some(ct) = field.content_type() {
                if ct.contains("png") {
                    ext = "png".to_string();
                }
            }
            image_data = field.bytes().await.ok().map(|b| b.to_vec());
        }
    }

    let Some(data) = image_data else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no image provided"})),
        )
            .into_response();
    };

    let cache_dir = artwork_cache_dir();
    // Condensat de CONTENU, plus d'identité figée (#1444). Sous
    // `artwork_hash("album-upload-{id}")`, remplacer la pochette gardait la
    // même URL alors que la route sert `immutable, max-age=31536000` : les
    // clients affichaient l'ancienne image pendant un an — et si l'extension
    // changeait (`.png` après un `.jpg`), les deux fichiers coexistaient et
    // `find_cached` servait l'ancien `.jpg` pour toujours. Une image
    // différente obtient désormais forcément une adresse différente, et
    // l'écriture passe par `save_to_cache` (extension canonique, #2567).
    let hash = tune_core::library::artwork::content_hash(&data);
    if tune_core::library::artwork::save_to_cache(&data, &cache_dir, &hash, &ext).is_none() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to save image"})),
        )
            .into_response();
    }

    album_repo.force_update_cover_path(id, &hash).ok();

    // Return the updated album
    match album_repo.get(id) {
        Ok(Some(album)) => Json(json!({
            "album": album.to_json(),
            "hash": hash,
            "size": data.len(),
        }))
        .into_response(),
        _ => Json(json!({
            "album_id": id,
            "hash": hash,
            "size": data.len(),
        }))
        .into_response(),
    }
}

pub(super) async fn proxy_artwork(
    State(state): State<AppState>,
    Query(q): Query<ProxyQuery>,
) -> impl IntoResponse {
    match state.http_client.get(&q.url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let content_type = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("image/jpeg")
                .to_string();
            match resp.bytes().await {
                Ok(data) => {
                    let mut headers = HeaderMap::new();
                    headers.insert(
                        "Content-Type",
                        HeaderValue::from_str(&content_type)
                            .unwrap_or(HeaderValue::from_static("image/jpeg")),
                    );
                    headers.insert(
                        "Cache-Control",
                        HeaderValue::from_static("public, max-age=86400"),
                    );
                    (StatusCode::OK, headers, data.to_vec()).into_response()
                }
                Err(_) => StatusCode::BAD_GATEWAY.into_response(),
            }
        }
        _ => StatusCode::BAD_GATEWAY.into_response(),
    }
}

pub(super) async fn enrich_album_artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let album = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "album not found"})),
            )
                .into_response();
        }
    };

    // Skip if album already has a non-empty cover
    if album.cover_path.as_ref().is_some_and(|p| !p.is_empty()) {
        return Json(json!({"enriched": false, "reason": "album already has cover art"}))
            .into_response();
    }

    // Step 1: Determine MBID — use existing or search MusicBrainz by artist+title
    let mbid = match album
        .musicbrainz_release_id
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        Some(id) => Some(id.to_string()),
        None => {
            let artist = album.artist_name.as_deref().unwrap_or("");
            if !artist.is_empty() && !album.title.is_empty() {
                let found =
                    tune_core::library::artwork::search_musicbrainz_release(artist, &album.title)
                        .await;
                if let Some(ref discovered_mbid) = found {
                    // Store the discovered MBID on the album for future use
                    state.backend.execute(
                        "UPDATE albums SET musicbrainz_release_id = ? WHERE id = ? AND (musicbrainz_release_id IS NULL OR musicbrainz_release_id = '')",
                        &[discovered_mbid as &dyn tune_core::db::backend::ToSqlValue, &id as &dyn tune_core::db::backend::ToSqlValue],
                    ).ok();
                    tracing::info!(
                        album_id = id,
                        mbid = %discovered_mbid,
                        album = %album.title,
                        artist = %artist,
                        "enrich_album_artwork_mbid_discovered"
                    );
                }
                found
            } else {
                None
            }
        }
    };

    let Some(ref mbid_val) = mbid else {
        return Json(json!({
            "enriched": false,
            "reason": "no MusicBrainz release ID and could not find one by artist/title"
        }))
        .into_response();
    };

    // Step 2: Fetch cover from Cover Art Archive
    match tune_core::library::artwork::fetch_cover_art(mbid_val).await {
        Some(data) => {
            let cache_dir = artwork_cache_dir();
            let hash = tune_core::library::artwork::artwork_hash(mbid_val);
            if tune_core::library::artwork::save_to_cache(&data, &cache_dir, &hash, "jpg").is_some()
            {
                repo.update_cover_path(id, &hash).ok();
                Json(json!({"enriched": true, "hash": hash, "size": data.len(), "mbid": mbid_val}))
                    .into_response()
            } else {
                Json(json!({"enriched": false, "reason": "failed to save to cache"}))
                    .into_response()
            }
        }
        None => {
            Json(json!({"enriched": false, "reason": "no cover art found on Cover Art Archive"}))
                .into_response()
        }
    }
}

pub(super) async fn batch_enrich_artwork(State(state): State<AppState>) -> impl IntoResponse {
    let cache_dir = artwork_cache_dir();
    let db = state.backend.clone();

    // Check how many albums are missing covers
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let missing = album_repo.list_without_cover().unwrap_or_default();

    if missing.is_empty() {
        return Json(json!({
            "status": "skipped",
            "message": "all albums already have cover art",
            "missing": 0,
        }))
        .into_response();
    }

    // Store initial status
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    settings.set("artwork_enrich_status", "running").ok();
    settings
        .set(
            "artwork_enrich_result",
            &json!({"total": missing.len(), "enriched": 0, "status": "running"}).to_string(),
        )
        .ok();

    let task_guard = state.background_tasks.begin(
        "artwork",
        "Récupération des pochettes d'albums…",
        "enrichment",
    );
    tokio::spawn(async move {
        let _task_guard = task_guard; // ends the task when this future completes
        tune_core::library::artwork::batch_enrich_artwork(db, cache_dir).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "message": "batch artwork enrichment started",
            "albums_to_process": missing.len(),
        })),
    )
        .into_response()
}

pub(super) async fn batch_enrich_artwork_status(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let result = settings
        .get("artwork_enrich_result")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let still_missing = album_repo.list_without_cover().unwrap_or_default().len();

    Json(json!({
        "result": result,
        "albums_without_cover": still_missing,
    }))
}

/// Décompte des artistes que la passe d'enrichissement va **réellement**
/// traiter, ventilé par population.
///
/// Les quatre champs reprennent, une pour une, les quatre listes que
/// `batch_enrich_artist_artwork_inner` empile avant de boucler
/// (`tune-core/src/library/artwork.rs`) : c'est la seule façon d'annoncer un
/// total qui corresponde au travail lancé.
pub(super) struct ArtistesSansImage {
    /// MBID connu, aucune image en base (`list_without_image`).
    pub avec_mbid: usize,
    /// MBID connu, `image_path` posé mais le fichier de cache a disparu.
    pub cache_perdu_avec_mbid: usize,
    /// Aucun MBID, aucune image (`list_without_image_no_mbid`).
    pub sans_mbid: usize,
    /// Aucun MBID, `image_path` posé mais le fichier de cache a disparu.
    pub cache_perdu_sans_mbid: usize,
}

impl ArtistesSansImage {
    /// Tout artiste que l'utilisateur voit sans photo.
    ///
    /// Les deux termes « sans MBID » manquaient : `list_without_image` et
    /// `list_with_image_and_mbid` exigent l'une comme l'autre
    /// `musicbrainz_id != ''`. Sur une bibliothèque non étiquetée le total
    /// tombait donc à zéro alors que la passe traite tout le monde (#2184).
    pub fn total(&self) -> usize {
        self.avec_mbid + self.cache_perdu_avec_mbid + self.sans_mbid + self.cache_perdu_sans_mbid
    }

    /// Les images « fantômes » : la base annonce une photo, le fichier a disparu.
    pub fn cache_perdu(&self) -> usize {
        self.cache_perdu_avec_mbid + self.cache_perdu_sans_mbid
    }
}

/// Compte les artistes sans image visible, MBID ou pas.
pub(super) fn compter_artistes_sans_image(
    artist_repo: &tune_core::db::artist_repo::ArtistRepo,
    cache_dir: &std::path::Path,
) -> ArtistesSansImage {
    let perdu = |image_path: &str| {
        !tune_core::library::artwork::cached_artwork_exists(cache_dir, image_path)
    };
    ArtistesSansImage {
        avec_mbid: artist_repo.list_without_image().unwrap_or_default().len(),
        cache_perdu_avec_mbid: artist_repo
            .list_with_image_and_mbid()
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, _, _, image_path)| perdu(image_path))
            .count(),
        sans_mbid: artist_repo
            .list_without_image_no_mbid()
            .unwrap_or_default()
            .len(),
        cache_perdu_sans_mbid: artist_repo
            .list_with_image_no_mbid()
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, _, image_path)| perdu(image_path))
            .count(),
    }
}

pub(super) async fn batch_enrich_artist_artwork(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let cache_dir = artwork_cache_dir();
    let db = state.backend.clone();

    // Count artists missing MBIDs (Phase 1 candidates)
    let artist_repo = tune_core::db::artist_repo::ArtistRepo::with_backend(state.backend.clone());
    let without_mbid = artist_repo.list_without_mbid().unwrap_or_default().len();

    // Le décompte annoncé doit couvrir la MÊME population que celle que les
    // phases vont traiter — y compris les artistes sans MBID, que toutes les
    // listes « with_mbid » excluent par construction.
    let sans_image = compter_artistes_sans_image(&artist_repo, &cache_dir);
    let broken_cache = sans_image.cache_perdu();

    if sans_image.total() == 0 && without_mbid == 0 {
        return Json(json!({
            "status": "skipped",
            "message": "all artists already have MBID and images",
            "missing": 0,
        }))
        .into_response();
    }

    // Store initial status
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    settings.set("artist_artwork_enrich_status", "running").ok();
    settings
        .set(
            "artist_artwork_enrich_result",
            &json!({"total": sans_image.total(), "enriched": 0, "without_mbid": without_mbid, "status": "running"}).to_string(),
        )
        .ok();

    let task_guard = state.background_tasks.begin(
        "artist_artwork",
        "Récupération des images d'artistes…",
        "enrichment",
    );
    let bg_tasks = state.background_tasks.clone();
    let poll_db = state.backend.clone();
    tokio::spawn(async move {
        let _task_guard = task_guard; // ends the task when this future completes

        // Mirror the enrichment's granular progress (written to the
        // `artist_artwork_enrich_result` setting by the two phases) into the
        // background-tasks registry, so the global indicator shows e.g.
        // "MusicBrainz 340/1183" instead of a bare "in progress" (grafts the
        // per-artist detail onto the presence-only task).
        let progress_poller = tokio::spawn(async move {
            let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(poll_db);
            for _ in 0..1200u32 {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                let Some(raw) = settings.get("artist_artwork_enrich_result").ok().flatten() else {
                    continue;
                };
                let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
                    continue;
                };
                if v.get("status").and_then(|s| s.as_str()) != Some("running") {
                    break;
                }
                let processed = v.get("processed").and_then(|n| n.as_u64()).unwrap_or(0);
                let total = v.get("total").and_then(|n| n.as_u64()).unwrap_or(0);
                let detail = match v.get("phase").and_then(|s| s.as_str()) {
                    Some("images") => "Images",
                    _ => "MusicBrainz",
                };
                bg_tasks.update_progress("artist_artwork", processed, total, detail);
            }
        });

        // Phase 1: Match artists without MBID by searching MusicBrainz
        let matched = tune_core::metadata::matcher::batch_match_artist_mbids(db.clone()).await;
        tracing::info!(matched, "batch_artist_mbid_phase_complete");

        // Phase 2: Fetch images for all artists with MBID but no image
        tune_core::library::artwork::batch_enrich_artist_artwork(db, cache_dir).await;

        progress_poller.abort();
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "message": "batch artist enrichment started (Phase 1: MBID matching, Phase 2: image fetch)",
            "artists_without_mbid": without_mbid,
            // Les images fantômes COMPTENT. Le travail vient d'être lancé pour
            // elles — `broken_cache` est justement ce qui empêche le « skipped »
            // ci-dessus — mais la réponse ne les annonçait pas, et l'interface,
            // lisant 0, affichait « Tous les artistes ont déjà une image » puis
            // cessait de suivre l'avancement. L'enrichissement tournait en fond
            // sans que rien ne le dise (Fabien, 11/08/2026 : bibliothèque
            // rescannée, aucune vignette d'artiste, ce message à l'écran).
            //
            // Du point de vue de l'utilisateur, un artiste dont le fichier a
            // disparu n'a pas d'image. C'est ce total-là qui doit être annoncé.
            //
            // Et il en va de même des artistes SANS MBID : `list_without_image`
            // comme `list_with_image_and_mbid` exigent toutes deux
            // `musicbrainz_id != ''`. Sur une bibliothèque non taguée — le cas
            // courant, ~8 % d'identification MusicBrainz — ces deux termes
            // valent zéro alors que la passe travaille des centaines
            // d'artistes en phase 3. Le client lit `artists_without_image === 0`,
            // annonce « Tous les artistes ont déjà une image » et cesse de
            // sonder : deux secondes, aucune image (Bruno Lescarret, #2184,
            // v0.9.44, 738 artistes Windows sans étiquette MusicBrainz).
            "artists_without_image": sans_image.total(),
            // Détaillé à part pour que l'interface puisse expliquer la
            // différence entre « jamais eu d'image » et « image perdue ».
            "artists_with_broken_image": broken_cache,
        })),
    )
        .into_response()
}

/// Force re-fetch of artist images for EVERY artist with an MBID, ignoring the
/// "already has an image" guard. For libraries where image_path is set to
/// stale/broken entries that never render, so the normal pass skips them.
pub(super) async fn force_refetch_artist_artwork(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let cache_dir = artwork_cache_dir();
    let db = state.backend.clone();

    let artist_repo = tune_core::db::artist_repo::ArtistRepo::with_backend(state.backend.clone());
    let total_artists = artist_repo
        .list_all_id_name_mbid()
        .unwrap_or_default()
        .len();

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    settings.set("artist_artwork_enrich_status", "running").ok();
    settings
        .set(
            "artist_artwork_enrich_result",
            &json!({"total": total_artists, "enriched": 0, "status": "running", "force": true})
                .to_string(),
        )
        .ok();

    let task_guard = state.background_tasks.begin(
        "artist_artwork",
        "Récupération forcée des images d'artistes…",
        "enrichment",
    );
    tokio::spawn(async move {
        let _task_guard = task_guard; // ends the task when this future completes
        // Phase 1: ensure MBIDs are matched, then force re-fetch everyone.
        let matched = tune_core::metadata::matcher::batch_match_artist_mbids(db.clone()).await;
        tracing::info!(matched, "force_artist_mbid_phase_complete");
        tune_core::library::artwork::batch_refetch_artist_artwork(db, cache_dir).await;
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({
            "status": "accepted",
            "message": "forced artist artwork re-fetch started (all artists)",
            "artists": total_artists,
        })),
    )
        .into_response()
}

pub(super) async fn batch_enrich_artist_artwork_status(
    State(state): State<AppState>,
) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let result = settings
        .get("artist_artwork_enrich_result")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    let artist_repo = tune_core::db::artist_repo::ArtistRepo::with_backend(state.backend.clone());
    // Même population que la réponse 202 : le client se sert de ce nombre comme
    // condition d'arrêt (`artistImgRemaining === 0` ⇒ « terminé »). Le limiter
    // aux artistes porteurs d'un MBID faisait conclure « terminé » au premier
    // sondage sur toute bibliothèque non taguée.
    let still_missing = compter_artistes_sans_image(&artist_repo, &artwork_cache_dir()).total();

    Json(json!({
        "result": result,
        "artists_without_image": still_missing,
    }))
}

pub(super) async fn rescan_album_artwork(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let track_repo = TrackRepo::with_backend(state.backend.clone());
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let tracks = track_repo.list_by_album(id).unwrap_or_default();
    if tracks.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no tracks in album"})),
        )
            .into_response();
    }
    let cache_dir = artwork_cache_dir();
    let mut found_hash: Option<String> = None;
    for track in &tracks {
        if let Some(ref file_path) = track.file_path {
            if let Some(hash) = tune_core::library::artwork::get_or_extract(
                std::path::Path::new(file_path),
                &cache_dir,
            ) {
                found_hash = Some(hash);
                break;
            }
        }
    }
    if let Some(ref hash) = found_hash {
        album_repo.force_update_cover_path(id, hash).ok();
    }
    Json(json!({
        "album_id": id,
        "rescanned_tracks": tracks.len(),
        "artwork_found": found_hash.is_some(),
        "hash": found_hash,
    }))
    .into_response()
}

pub(super) async fn rescan_all_artwork(State(state): State<AppState>) -> impl IntoResponse {
    let cache_dir = artwork_cache_dir();
    let backend = state.backend.clone();

    tokio::spawn(async move {
        let albums: Vec<i64> = backend
            .query_many("SELECT id FROM albums", &[])
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| row.first().and_then(|v| v.as_i64()))
            .collect();

        let track_repo = TrackRepo::with_backend(backend.clone());
        let album_repo = AlbumRepo::with_backend(backend);
        let mut updated = 0i32;
        for album_id in &albums {
            let tracks = track_repo.list_by_album(*album_id).unwrap_or_default();
            for track in &tracks {
                if let Some(ref file_path) = track.file_path {
                    if let Some(hash) = tune_core::library::artwork::get_or_extract(
                        std::path::Path::new(file_path),
                        &cache_dir,
                    ) {
                        album_repo.force_update_cover_path(*album_id, &hash).ok();
                        updated += 1;
                        break;
                    }
                }
            }
        }
        tracing::info!(updated, total = albums.len(), "rescan_all_artwork done");
    });

    (
        StatusCode::ACCEPTED,
        Json(json!({"status": "accepted", "message": "artwork rescan started"})),
    )
}

#[cfg(test)]
mod tests_decompte_artistes {
    use super::*;
    use std::sync::Arc;
    use tune_core::db::artist_repo::ArtistRepo;
    use tune_core::db::backend::DbBackend;
    use tune_core::db::models::Artist;

    fn base_memoire() -> Arc<dyn DbBackend> {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    /// Crée un artiste et pose son MBID / son `image_path` s'il y en a un.
    fn artiste(repo: &ArtistRepo, nom: &str, mbid: Option<&str>, image: Option<&str>) {
        let id = repo.create(&Artist::new(nom.into())).unwrap();
        if let Some(m) = mbid {
            repo.update_mbid(id, m).unwrap();
        }
        if let Some(i) = image {
            repo.update_image(id, i, "test").unwrap();
        }
    }

    /// Le cas Bruno Lescarret (#2184) : bibliothèque Windows sans la moindre
    /// étiquette MusicBrainz. Toutes les listes « avec MBID » rendent zéro, et
    /// c'est pourtant tout le travail de la passe.
    #[test]
    fn une_bibliotheque_sans_mbid_ne_compte_pas_zero_artiste_sans_image() {
        let backend = base_memoire();
        let repo = ArtistRepo::with_backend(backend);
        for nom in ["Ange", "Magma", "Gong"] {
            artiste(&repo, nom, None, None);
        }
        let cache = tempfile::tempdir().unwrap();

        let compte = compter_artistes_sans_image(&repo, cache.path());

        assert_eq!(compte.avec_mbid, 0, "aucun artiste n'a de MBID");
        assert_eq!(
            compte.sans_mbid, 3,
            "les trois sont sans MBID et sans image"
        );
        assert_eq!(
            compte.total(),
            3,
            "un artiste sans MBID et sans image est un artiste SANS IMAGE : \
             en annoncer 0 fait afficher « tous les artistes ont déjà une image » \
             et arrête le suivi au bout de deux secondes (#2184)"
        );
    }

    /// Image « fantôme » sans MBID : la base annonce une photo, le fichier de
    /// cache a disparu. La phase 2 la remet en file (`list_with_image_no_mbid`),
    /// donc elle doit être annoncée.
    #[test]
    fn une_image_fantome_sans_mbid_compte_comme_manquante() {
        let backend = base_memoire();
        let repo = ArtistRepo::with_backend(backend);
        artiste(
            &repo,
            "Heldon",
            None,
            Some("cafecafecafecafecafecafecafecafe"),
        );
        let cache = tempfile::tempdir().unwrap();

        let compte = compter_artistes_sans_image(&repo, cache.path());

        assert_eq!(compte.cache_perdu_sans_mbid, 1);
        assert_eq!(compte.total(), 1);
    }

    /// Garde-fou inverse : une photo réellement présente en cache ne doit
    /// jamais être recomptée comme manquante, MBID ou pas.
    #[test]
    fn une_image_presente_en_cache_ne_compte_pas() {
        let backend = base_memoire();
        let repo = ArtistRepo::with_backend(backend);
        let cache = tempfile::tempdir().unwrap();
        let hash = "aaaabbbbccccddddeeeeffff00001111";
        std::fs::write(cache.path().join(format!("{hash}.jpg")), b"jpeg").unwrap();
        artiste(&repo, "Pulsar", None, Some(hash));
        artiste(
            &repo,
            "Shylock",
            Some("11111111-2222-3333-4444-555555555555"),
            Some(hash),
        );

        let compte = compter_artistes_sans_image(&repo, cache.path());

        assert_eq!(compte.total(), 0, "les deux photos sont bien en cache");
        assert_eq!(compte.cache_perdu(), 0);
    }

    /// Les quatre populations à la fois, pour que le total ne puisse pas être
    /// juste « par hasard » sur un seul terme.
    #[test]
    fn le_total_couvre_les_quatre_populations() {
        let backend = base_memoire();
        let repo = ArtistRepo::with_backend(backend);
        let cache = tempfile::tempdir().unwrap();
        let present = "99998888777766665555444433332222";
        std::fs::write(cache.path().join(format!("{present}.jpg")), b"jpeg").unwrap();

        artiste(&repo, "avec mbid, sans image", Some("mbid-a"), None);
        artiste(
            &repo,
            "avec mbid, image fantome",
            Some("mbid-b"),
            Some("00000000000000000000000000000000"),
        );
        artiste(&repo, "sans mbid, sans image", None, None);
        artiste(
            &repo,
            "sans mbid, image fantome",
            None,
            Some("11111111111111111111111111111111"),
        );
        // Témoin : ne doit compter dans aucun terme.
        artiste(&repo, "servi", Some("mbid-c"), Some(present));

        let compte = compter_artistes_sans_image(&repo, cache.path());

        assert_eq!(compte.avec_mbid, 1);
        assert_eq!(compte.cache_perdu_avec_mbid, 1);
        assert_eq!(compte.sans_mbid, 1);
        assert_eq!(compte.cache_perdu_sans_mbid, 1);
        assert_eq!(compte.total(), 4, "quatre artistes sans photo visible");
        assert_eq!(compte.cache_perdu(), 2);
    }
}

// ---------------------------------------------------------------------------
// #2567 — ce que la base annonce, la route doit le servir.
//
// Le client web ne fabrique pas l'identifiant qu'il demande : il recopie
// `cover_path` tel que le serveur le lui a rendu, et le pose derrière
// `/api/v1/library/artwork/`. Un condensat annoncé que cette route ne trouve
// pas, c'est l'image de remplacement à l'écran — et, jusqu'ici, rien du tout
// dans le journal du serveur.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests_service_pochette {
    use super::*;

    fn ecrire(dir: &std::path::Path, nom: &str, octets: &[u8]) {
        std::fs::write(dir.join(nom), octets).unwrap();
    }

    /// Les orthographes que l'écriture a réellement produites sur le terrain :
    /// l'extension d'une `cover.jpeg` ou d'une `FOLDER.JPG` était recopiée
    /// telle quelle, et une pochette intégrée BMP était écrite `.bmp`.
    #[tokio::test]
    async fn toute_entree_de_cache_ecrite_est_servie() {
        let cache = tempfile::TempDir::new().unwrap();
        let cas: &[(&str, &str, &str)] = &[
            ("0000000000000000000000000000000a", "jpg", "image/jpeg"),
            ("0000000000000000000000000000000b", "jpeg", "image/jpeg"),
            ("0000000000000000000000000000000c", "JPG", "image/jpeg"),
            ("0000000000000000000000000000000d", "JPEG", "image/jpeg"),
            ("0000000000000000000000000000000e", "png", "image/png"),
            ("0000000000000000000000000000000f", "PNG", "image/png"),
            ("00000000000000000000000000000010", "webp", "image/webp"),
            ("00000000000000000000000000000011", "bmp", "image/bmp"),
        ];
        let mut echecs = Vec::new();
        for (hash, ext, mime) in cas {
            ecrire(cache.path(), &format!("{hash}.{ext}"), b"IMAGE");
            let reponse = serve_artwork_from(cache.path(), hash).await;
            let statut = reponse.status();
            let recu = reponse
                .headers()
                .get("Content-Type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            if statut != StatusCode::OK || recu != *mime {
                echecs.push(format!(
                    "{ext} → {statut} / {recu:?} (attendu 200 OK / {mime})"
                ));
            }
        }
        assert!(
            echecs.is_empty(),
            "{} orthographe(s) sur {} écrites dans le cache mais non servies (#2567) : {:?}",
            echecs.len(),
            cas.len(),
            echecs
        );
    }

    /// Une entrée adressée par le CONTENU (#1444) — SHA-256, 64 hexdigits —
    /// est servie exactement comme une entrée héritée en 32 : la route ne
    /// distingue pas les deux formes.
    #[tokio::test]
    async fn une_entree_adressee_par_le_contenu_est_servie() {
        let cache = tempfile::TempDir::new().unwrap();
        let hash = tune_core::library::artwork::content_hash(b"NOUVELLE-POCHETTE");
        assert_eq!(hash.len(), 64);
        ecrire(cache.path(), &format!("{hash}.jpg"), b"NOUVELLE-POCHETTE");
        let reponse = serve_artwork_from(cache.path(), &hash).await;
        assert_eq!(reponse.status(), StatusCode::OK);
    }

    /// Garde-fou : un condensat sans fichier reste un 404. Servir un octet de
    /// remplacement à sa place ferait croire à une pochette et empêcherait de
    /// jamais la reconstruire.
    #[tokio::test]
    async fn un_condensat_sans_fichier_reste_un_404() {
        let cache = tempfile::TempDir::new().unwrap();
        let reponse = serve_artwork_from(cache.path(), "8865c2f2e1a6f89c34ab584ec5b8e158").await;
        assert_eq!(reponse.status(), StatusCode::NOT_FOUND);
    }

    /// Garde-fou : l'ETag reste le condensat lui-même. Le changer invaliderait
    /// d'un coup la pochette déjà en cache dans chaque navigateur.
    #[tokio::test]
    async fn l_etag_reste_le_condensat() {
        let cache = tempfile::TempDir::new().unwrap();
        let hash = "8865c2f2e1a6f89c34ab584ec5b8e158";
        ecrire(cache.path(), &format!("{hash}.jpg"), b"IMAGE");
        let reponse = serve_artwork_from(cache.path(), hash).await;
        assert_eq!(reponse.status(), StatusCode::OK);
        assert_eq!(
            reponse.headers().get("ETag").unwrap().to_str().unwrap(),
            format!("\"{hash}\"")
        );
        assert_eq!(
            reponse
                .headers()
                .get("Cache-Control")
                .unwrap()
                .to_str()
                .unwrap(),
            "public, max-age=31536000, immutable"
        );
    }
}
