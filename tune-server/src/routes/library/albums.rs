use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::routes::active_profile::ActiveProfile;
use crate::state::AppState;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use tune_core::db::models::Album;
use tune_core::db::profile_repo::ProfileRepo;
use tune_core::db::rating_repo::RatingRepo;
use tune_core::db::track_metadata_repo::TrackMetadataRepo;
use tune_core::db::track_repo::{TrackRepo, dedup_display_tracks};

use super::Pagination;

#[derive(Deserialize)]
pub(super) struct AlbumFilters {
    limit: Option<i64>,
    offset: Option<i64>,
    quality: Option<String>,
    format: Option<String>,
    sort: Option<String>,
    order: Option<String>,
    /// `?compilation=true` ne rend que les compilations, `false` que le reste,
    /// absent = tout (#1957).
    compilation: Option<bool>,
    /// `?include_hidden=true` rend AUSSI les albums masqués (#1391). Absent
    /// ou `false` : ils sont exclus — un client qui ignore le paramètre voit
    /// simplement l'album disparaître, sans rien changer chez lui.
    include_hidden: Option<bool>,
}

#[derive(Deserialize)]
pub(super) struct QuickFavQuery {
    profile_id: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct RateRequest {
    rating: i32,
    note: Option<String>,
    profile_id: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct RatingQuery {
    profile_id: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct LangQuery {
    lang: Option<String>,
}

pub(super) async fn list_albums(
    State(state): State<AppState>,
    Query(p): Query<AlbumFilters>,
) -> Json<Value> {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let limit = p.limit.unwrap_or(50);
    let offset = p.offset.unwrap_or(0);
    let sort = p.sort.as_deref().unwrap_or("added_at");
    let order = p.order.as_deref().unwrap_or("asc");
    let include_hidden = p.include_hidden.unwrap_or(false);
    // Le total suit la même exclusion que la liste, sinon la grille pagine
    // faux (#1391).
    let total = if include_hidden {
        repo.count().unwrap_or(0)
    } else {
        repo.count_visible().unwrap_or(0)
    };
    let items = match repo.list_filtered(
        limit,
        offset,
        sort,
        order,
        p.format.as_deref(),
        p.quality.as_deref(),
        p.compilation,
        include_hidden,
    ) {
        Ok(albums) => albums,
        Err(e) => {
            tracing::error!(
                error = %e,
                sort,
                order,
                limit,
                offset,
                total,
                "list_albums_query_failed — stats show {total} albums but query returned error"
            );
            Vec::new()
        }
    };
    let items: Vec<Value> = items
        .iter()
        .map(|a| {
            let mut j = a.to_json();
            if let Some(obj) = j.as_object_mut() {
                obj.remove("bio");
            }
            j
        })
        .collect();
    Json(json!({"items": items, "total": total, "limit": limit, "offset": offset}))
}

pub(super) async fn album_count(State(state): State<AppState>) -> Json<Value> {
    let count = AlbumRepo::with_backend(state.backend.clone())
        .count()
        .unwrap_or(0);
    Json(json!({ "count": count }))
}

/// `POST /library/albums/{id}/hide` — masque l'album (#1391).
///
/// Masquer n'est PAS supprimer : les fichiers restent intacts, `GET
/// /albums/{id}`, ses pistes et la lecture restent opérants (files d'attente
/// et playlists continuent de jouer) ; l'album sort des vues de découverte
/// (grilles, pistes, recherche, facettes). Réversible par DELETE. Idempotent.
pub(super) async fn hide_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let repo = tune_core::db::hidden_repo::HiddenRepo::with_backend(state.backend.clone());
    match repo.hide_album(id) {
        Ok(true) => Ok(Json(json!({"album_id": id, "hidden": true}))),
        Ok(false) => Err(AppError::not_found(format!("album {id} not found"))),
        Err(e) => Err(AppError::internal(e)),
    }
}

/// `DELETE /library/albums/{id}/hide` — démasque. Idempotent : démasquer un
/// album non masqué rend simplement `hidden: false`.
pub(super) async fn unhide_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let repo = tune_core::db::hidden_repo::HiddenRepo::with_backend(state.backend.clone());
    match repo.unhide_album(id) {
        Ok(_) => Ok(Json(json!({"album_id": id, "hidden": false}))),
        Err(e) => Err(AppError::internal(e)),
    }
}

/// `GET /library/albums/hidden` — la liste de révision : tout ce qui est
/// masqué, y compris les marqueurs momentanément orphelins (racine démontée),
/// rendus avec l'instantané d'identité pour rester démasquables.
pub(super) async fn list_hidden_albums(
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    let repo = tune_core::db::hidden_repo::HiddenRepo::with_backend(state.backend.clone());
    let items = repo.list_hidden_albums().map_err(AppError::internal)?;
    Ok(Json(json!({"total": items.len(), "items": items})))
}

#[derive(Deserialize)]
pub(super) struct CreateAlbumRequest {
    title: String,
    artist_id: Option<i64>,
}

/// Create an album by title (used by MetadataView when assigning tracks to a
/// new album name). Reuses an existing album with the same title if one exists.
pub(super) async fn create_album(
    State(state): State<AppState>,
    Json(body): Json<CreateAlbumRequest>,
) -> Result<impl IntoResponse, AppError> {
    let title = body.title.trim();
    if title.is_empty() {
        return Err(AppError::bad_request("title is required"));
    }
    let repo = AlbumRepo::with_backend(state.backend.clone());
    if let Ok(Some(existing)) = repo.get_by_title(title) {
        return Ok(Json(json!({ "id": existing.id, "title": existing.title })));
    }
    let album = Album {
        id: None,
        title: title.to_string(),
        artist_id: body.artist_id,
        artist_name: None,
        year: None,
        original_year: None,
        genre: None,
        genres: None,
        disc_count: None,
        track_count: None,
        cover_path: None,
        source: "local".to_string(),
        source_id: None,
        label: None,
        catalog_number: None,
        barcode: None,
        format: None,
        sample_rate: None,
        bit_depth: None,
        bio: None,
        musicbrainz_release_id: None,
        musicbrainz_release_group_id: None,
        release_date: None,
        original_date: None,
        added_at: None,
        // Un album créé à la main n'est pas une compilation : c'est le scan
        // qui lève ce drapeau, d'après les tags (#1957).
        is_compilation: false,
    };
    let id = repo
        .create(&album)
        .map_err(|e| AppError::internal(e.to_string()))?;
    Ok(Json(json!({ "id": id, "title": title })))
}

pub(super) async fn album_filters(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    // `LOWER(TRIM(...))`, sinon deux valeurs qui ne diffèrent que par la casse
    // font deux entrées — et l'écran les rend IDENTIQUES, puisqu'il met tout en
    // majuscules à l'affichage (`LibraryView.svelte`, `toUpperCase()`).
    //
    // C'est ainsi que « DSD » apparaissait deux fois dans les types de fichiers,
    // en deux lignes visuellement indiscernables : `dsd` et `DSD` en base
    // (Cyrille Moutia, #1612). Le chemin de scan actuel écrit bien en
    // minuscules, mais toute valeur venue d'ailleurs — une version antérieure,
    // un import — traverse sans être repliée.
    //
    // Le repli se fait ICI et pas dans `normalize_format` : le passthrough
    // sensible à la casse de cette fonction est délibéré et figé par un test
    // (`normalize_format_case_sensitivity` : « MPEG » ne doit pas devenir
    // « mp3 »). Le lever changerait le format écrit pour d'autres fichiers.
    //
    // `TRIM` en plus de `LOWER` : un espace de fin produit exactement le même
    // doublon invisible, pour la même raison.
    let formats: Vec<String> = state
        .backend
        .query_many(
            "SELECT DISTINCT LOWER(TRIM(format)) FROM albums \
             WHERE format IS NOT NULL AND TRIM(format) != '' \
             ORDER BY LOWER(TRIM(format))",
            &[],
        )
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| row.into_iter().next()?.as_string())
        .collect();
    let sample_rates: Vec<i64> = state
        .backend
        .query_many(
            "SELECT DISTINCT sample_rate FROM albums WHERE sample_rate IS NOT NULL ORDER BY sample_rate",
            &[],
        )
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| row.into_iter().next()?.as_i64())
        .collect();
    Ok(Json(
        json!({ "formats": formats, "sample_rates": sample_rates }),
    ))
}

pub(super) async fn recent_albums(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(50);
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let items = repo.list_recent(limit).unwrap_or_default();
    let items: Vec<Value> = items.iter().map(|a| a.to_json()).collect();
    Json(json!(items))
}

pub(super) async fn get_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    match repo.get(id) {
        Ok(Some(album)) => {
            let mut j = album.to_json();
            if let (Some(obj), Ok(Some(prov))) = (j.as_object_mut(), repo.bio_provenance(id)) {
                obj.insert("bio_provenance".into(), prov);
            }
            // Dynamic Range, when the files carry the tag (#303, #1418). Absent
            // from the payload rather than null when untagged, so a client can
            // simply test for the key instead of distinguishing "no tag" from
            // "measured zero" — DR0 is a real value.
            if let (Some(obj), Ok(Some(dr))) = (j.as_object_mut(), repo.dynamic_range(id)) {
                obj.insert("dynamic_range".into(), Value::String(dr));
            }
            Json(j).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub(super) async fn album_tracks(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(f): Query<AlbumFilters>,
) -> Json<Value> {
    let repo = TrackRepo::with_backend(state.backend.clone());
    // When the library grid has an active quality/format filter, the client
    // forwards it here so the album detail shows only the matching tracks
    // (Sergio: a Hi-Res/FLAC filter must not reveal the album's MP3/44.1
    // tracks). With no filter this is identical to list_by_album.
    let items = dedup_display_tracks(
        repo.list_by_album_filtered(id, f.format.as_deref(), f.quality.as_deref())
            .unwrap_or_default(),
    );

    // GROUPING (#2130) : lu au scan et rangé dans `track_metadata`, il n'était
    // jusqu'ici ressorti par aucune route. Une seule requête indexée par album
    // suffit ; le tag étant absent de l'immense majorité des bibliothèques,
    // elle ne ramène le plus souvent aucune ligne et la clé `grouping` reste
    // absente du JSON — ce qui laisse le contrat inchangé pour les clients qui
    // ne la connaissent pas.
    let track_ids: Vec<i64> = items.iter().filter_map(|t| t.id).collect();
    let grouping = TrackMetadataRepo::with_backend(state.backend.clone())
        .get_key_for_tracks("grouping", &track_ids)
        .unwrap_or_default();

    Json(json!(attach_grouping(items, &grouping)))
}

/// Recopie le tag GROUPING sur les pistes sérialisées d'un album.
///
/// La clé n'est ajoutée que pour les pistes qui en portent réellement une
/// (`get_key_for_tracks` a déjà écarté les valeurs vides) : une piste sans
/// GROUPING sort exactement comme avant, sans champ supplémentaire.
fn attach_grouping(
    items: Vec<tune_core::db::models::Track>,
    grouping: &std::collections::HashMap<i64, String>,
) -> Vec<Value> {
    items
        .into_iter()
        .map(|t| {
            let track_id = t.id;
            let mut v = serde_json::to_value(&t).unwrap_or_default();
            if let (Some(track_id), Some(obj)) = (track_id, v.as_object_mut()) {
                if let Some(g) = grouping.get(&track_id) {
                    obj.insert("grouping".into(), json!(g));
                }
            }
            v
        })
        .collect()
}

pub(super) async fn quick_fav_album(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(id): Path<i64>,
    Query(q): Query<QuickFavQuery>,
) -> Json<Value> {
    let profile_id = q.profile_id.unwrap_or_else(|| profile.id());
    let repo = ProfileRepo::with_backend(state.backend.clone());
    let is_fav = repo.is_favorite(profile_id, "album", id).unwrap_or(false);
    if is_fav {
        repo.remove_favorite(profile_id, "album", id).ok();
    } else {
        repo.add_favorite(profile_id, "album", id).ok();
    }
    Json(json!({"is_favorite": !is_fav, "album_id": id}))
}

pub(super) async fn rate_album(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(id): Path<i64>,
    Json(body): Json<RateRequest>,
) -> impl IntoResponse {
    let repo = RatingRepo::with_backend(state.backend.clone());
    let profile_id = body.profile_id.unwrap_or_else(|| profile.id());
    match repo.rate_album(id, profile_id, body.rating, body.note.as_deref()) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e.to_string()).into_response(),
    }
}

pub(super) async fn get_album_rating(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(id): Path<i64>,
    Query(q): Query<RatingQuery>,
) -> impl IntoResponse {
    let repo = RatingRepo::with_backend(state.backend.clone());
    let profile_id = q.profile_id.unwrap_or_else(|| profile.id());
    match repo.get_rating(id, profile_id) {
        Ok(Some(r)) => Json(json!(r)).into_response(),
        Ok(None) => Json(json!({ "rating": null, "album_id": id })).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

pub(super) async fn top_rated_albums(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    let limit = p.limit.unwrap_or(20);
    let repo = RatingRepo::with_backend(state.backend.clone());
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let top = repo.top_rated(limit).unwrap_or_default();

    let items: Vec<Value> = top
        .iter()
        .filter_map(|(album_id, avg_rating, count)| {
            let album = album_repo.get(*album_id).ok()??;
            Some(json!({
                "album": album,
                "avg_rating": avg_rating,
                "rating_count": count,
            }))
        })
        .collect();

    Json(json!(items))
}

pub(super) async fn recommendations(
    State(state): State<AppState>,
    Query(p): Query<Pagination>,
) -> Json<Value> {
    // Return recently added albums the user hasn't listened to
    let limit = p.limit.unwrap_or(20);
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let items = repo.list_recent(limit).unwrap_or_default();
    let items: Vec<Value> = items.iter().map(|a| a.to_json()).collect();
    Json(json!({ "albums": items }))
}

pub(super) async fn album_bio(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<LangQuery>,
) -> impl IntoResponse {
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let album = match album_repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let lang = q.lang.as_deref().unwrap_or("fr");

    // Prefer a locally-enriched bio (with provenance/attribution) over the
    // community proxy.
    //
    // MAIS seulement si elle est dans la bonne langue (#1849, Dimitri). La
    // route artiste a recu ce garde-fou en #2126 ; la route album, que le
    // ticket cite pourtant dans sa portee, ne l'a jamais eu. Elle rendait donc
    // la bio stockee sans regarder ni sa langue ni celle qu'on demande, et le
    // `lang` ci-dessus ne servait qu'a nommer l'entree de cache -- il n'etait
    // meme JAMAIS atteint pour un album qui possedait deja une bio.
    //
    // L'enrichissement des bios d'album prend la langue de CELUI QUI LE LANCE
    // (`routes/system/enrich.rs`, `lang_from_header` puis
    // `batch_enrich_album_bios(.., &lang)`) : une bibliotheque enrichie depuis
    // une interface en francais stocke tout en francais, et le ressert ensuite
    // a tout le monde.
    let prov = album_repo.bio_provenance(id).ok().flatten();
    let bio_lang = prov
        .as_ref()
        .and_then(|p| p.get("lang").and_then(|v| v.as_str()));
    let stored_ok = super::artists::langue_convient(bio_lang, lang);

    if let Some(ref bio) = album.bio {
        if !bio.is_empty() && stored_ok {
            return Json(json!({
                "album": album.title,
                "bio": bio,
                "bio_provenance": prov,
            }))
            .into_response();
        }
    }
    // Community album-bio API is keyed by NAME (title + artist) and generated
    // on demand by the cloud — NO MusicBrainz id required, so it works for
    // every album. The old code proxied to the artist-MBID endpoint (wrong URL
    // too) and failed for the whole library, which has no MBIDs (#bios).
    let artist_name = album.artist_name.clone().or_else(|| {
        album.artist_id.and_then(|aid| {
            ArtistRepo::with_backend(state.backend.clone())
                .get(aid)
                .ok()
                .flatten()
                .map(|a| a.name)
        })
    });
    let artist_q = artist_name.as_deref().unwrap_or("");
    let cache_key = format!("cache:albumbio:{}:{artist_q}:{lang}", album.title);
    if let Some(cached) = super::api_cache_get(&state.backend, &cache_key) {
        return Json(cached).into_response();
    }
    match state
        .http_client
        .get("https://mozaiklabs.fr/api/v1/albums/bio")
        // `lang` est transmis au site, qui ne le recevait pas : il ne servait
        // qu'a nommer l'entree de cache. Meme reserve que pour la route
        // artiste (#2126) : l'effet depend de `site-mozaiklabs`. S'il ignore le
        // parametre, il rendra la meme langue qu'avant -- sans regression, mais
        // sans gain non plus.
        .query(&[
            ("title", album.title.as_str()),
            ("artist", artist_q),
            ("lang", lang),
        ])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let data: Value = resp.json().await.unwrap_or(json!({}));
            let out = json!({
                "album": album.title,
                "bio": data.get("bio").cloned().unwrap_or(Value::Null),
                "source": data.get("source").cloned().unwrap_or(Value::Null),
            });
            if out.get("bio").map(|b| !b.is_null()).unwrap_or(false) {
                super::api_cache_set(&state.backend, &cache_key, &out);
            }
            Json(out).into_response()
        }
        _ => repli_sur_la_bio_stockee(&album.title, album.bio.as_deref(), prov),
    }
}

/// Ressert la bio d'album stockee quand la langue demandee n'a rien donne.
///
/// « Une biographie en francais vaut mieux qu'un panneau vide » -- #1849 le dit,
/// et c'est juste : refuser une bio dans la mauvaise langue serait remplacer un
/// defaut par un pire. La provenance part avec elle, donc le client sait dans
/// quelle langue elle est et peut le signaler. Pendant du
/// `repli_sur_la_bio_stockee` de la route artiste, dont seule la cle JSON
/// (`album` au lieu de `artist`) differe.
fn repli_sur_la_bio_stockee(
    titre: &str,
    bio: Option<&str>,
    prov: Option<Value>,
) -> axum::response::Response {
    match bio.filter(|b| !b.is_empty()) {
        Some(b) => Json(json!({
            "album": titre,
            "bio": b,
            "bio_provenance": prov,
        }))
        .into_response(),
        None => Json(json!({"album": titre, "bio": null})).into_response(),
    }
}

pub(super) async fn album_similar(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let album = match album_repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let mbid = if let Some(aid) = album.artist_id {
        let artist_repo = ArtistRepo::with_backend(state.backend.clone());
        artist_repo
            .get(aid)
            .ok()
            .flatten()
            .and_then(|a| a.musicbrainz_id)
    } else {
        None
    };
    let Some(mbid) = mbid else {
        return Json(json!({"album": album.title, "artists": []})).into_response();
    };
    match state
        .http_client
        .get(format!("https://mozaiklabs.fr/api/{mbid}/similar"))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            let data: Value = resp.json().await.unwrap_or(json!({}));
            Json(data).into_response()
        }
        _ => Json(json!({"mbid": mbid, "artists": []})).into_response(),
    }
}

pub(super) async fn merge_duplicate_albums_route(
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    // Pick engine-specific aggregate and placeholder helpers.
    let (group_concat_expr, p1, p2) = match state.backend.engine() {
        Engine::Postgres => (
            PostgresDialect.group_concat(&PostgresDialect.placeholder(1), ","),
            PostgresDialect.placeholder(1),
            PostgresDialect.placeholder(2),
        ),
        Engine::Sqlite => (
            SqliteDialect.group_concat("id", ","),
            SqliteDialect.placeholder(1),
            SqliteDialect.placeholder(2),
        ),
    };

    // Case-insensitive grouping: LOWER(title) catches duplicates that differ
    // only by case (e.g. "The Dark Side of the Moon" vs "The Dark Side Of The Moon").
    let dupes_sql = format!(
        "SELECT LOWER(title), {group_concat_expr} FROM albums WHERE source = 'local' GROUP BY LOWER(title) HAVING COUNT(id) > 1"
    );
    let dupes: Vec<(String, String)> = state
        .backend
        .query_many(&dupes_sql, &[])
        .unwrap_or_default()
        .into_iter()
        .filter_map(|row| {
            let title = row.first()?.as_string()?;
            let ids = row.get(1)?.as_string()?;
            Some((title, ids))
        })
        .collect();

    let mut deleted = 0i64;
    for (_title, ids_str) in &dupes {
        let ids: Vec<i64> = ids_str.split(',').filter_map(|s| s.parse().ok()).collect();
        if ids.len() < 2 {
            continue;
        }
        let mut best_id = ids[0];
        let mut best_count = 0i64;
        let count_sql = format!("SELECT COUNT(id) FROM tracks WHERE album_id = {p1}");
        for &aid in &ids {
            let cnt: i64 = state
                .backend
                .query_one(&count_sql, &[&aid as &dyn ToSqlValue])
                .ok()
                .flatten()
                .and_then(|row| row.into_iter().next()?.as_i64())
                .unwrap_or(0);
            if cnt > best_count {
                best_count = cnt;
                best_id = aid;
            }
        }
        let update_sql = format!("UPDATE tracks SET album_id = {p1} WHERE album_id = {p2}");
        let delete_sql = format!("DELETE FROM albums WHERE id = {p1}");
        for &aid in &ids {
            if aid != best_id {
                state
                    .backend
                    .execute(
                        &update_sql,
                        &[&best_id as &dyn ToSqlValue, &aid as &dyn ToSqlValue],
                    )
                    .ok();
                state
                    .backend
                    .execute(&delete_sql, &[&aid as &dyn ToSqlValue])
                    .ok();
                deleted += 1;
            }
        }
    }
    state
        .backend
        .execute_batch(
            "UPDATE albums SET track_count = (SELECT COUNT(t.id) FROM tracks t WHERE t.album_id = albums.id)"
        )
        .ok();
    Ok(Json(json!({ "merged": deleted })))
}

const VARIANT_PATTERNS: &[&str] = &[
    "deluxe",
    "remastered",
    "remaster",
    "anniversary",
    "expanded",
    "special edition",
    "collector",
    "bonus track",
    "super deluxe",
    "legacy edition",
    "platinum edition",
];

fn strip_variant_suffix(title: &str) -> String {
    let lower = title.to_lowercase();
    for pat in VARIANT_PATTERNS {
        if let Some(pos) = lower.find(pat) {
            let prefix = title[..pos]
                .trim_end_matches(|c: char| c == '(' || c == '[' || c == '-' || c == ' ');
            if !prefix.is_empty() {
                return prefix.to_string();
            }
        }
    }
    title.to_string()
}

pub(super) async fn albums_grouped(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let repo = AlbumRepo::with_backend(state.backend.clone());

    // Group by MusicBrainz release group ID
    let mbid_groups = repo.list_release_groups().unwrap_or_default();

    let mut groups: Vec<Value> = mbid_groups
        .iter()
        .map(|(gid, albums)| {
            let original = &albums[0];
            json!({
                "group_id": gid,
                "method": "musicbrainz",
                "original": original.to_json(),
                "variants": albums[1..].iter().map(|a| a.to_json()).collect::<Vec<_>>(),
                "count": albums.len(),
            })
        })
        .collect();

    // Group by title similarity (regex) for albums without MBID
    let all_albums = repo.list(5000, 0).unwrap_or_default();
    let grouped_ids: std::collections::HashSet<i64> = mbid_groups
        .iter()
        .flat_map(|(_, albums)| albums.iter().filter_map(|a| a.id))
        .collect();

    let ungrouped: Vec<_> = all_albums
        .iter()
        .filter(|a| a.id.is_some() && !grouped_ids.contains(&a.id.unwrap()))
        .collect();

    let mut title_map: std::collections::HashMap<String, Vec<&tune_core::db::models::Album>> =
        std::collections::HashMap::new();
    for album in &ungrouped {
        let base = strip_variant_suffix(&album.title);
        title_map.entry(base).or_default().push(album);
    }

    for (base_title, albums) in &title_map {
        if albums.len() > 1 {
            groups.push(json!({
                "group_id": base_title,
                "method": "title_similarity",
                "original": albums[0].to_json(),
                "variants": albums[1..].iter().map(|a| a.to_json()).collect::<Vec<_>>(),
                "count": albums.len(),
            }));
        }
    }

    Ok(Json(json!({
        "groups": groups,
        "total_groups": groups.len(),
    })))
}

pub(super) async fn album_completeness(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, AppError> {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let album = repo
        .get(id)
        .ok()
        .flatten()
        .ok_or(AppError::not_found("album not found"))?;

    let p1 = match state.backend.engine() {
        Engine::Postgres => PostgresDialect.placeholder(1),
        Engine::Sqlite => SqliteDialect.placeholder(1),
    };

    let actual_tracks: i64 = state
        .backend
        .query_one(
            &format!("SELECT COUNT(*) FROM tracks WHERE album_id = {p1}"),
            &[&id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|row| row.into_iter().next()?.as_i64())
        .unwrap_or(0);
    let expected_tracks = album.track_count.unwrap_or(0) as i64;

    // Check total_tracks from metadata tags
    let max_tag_total: i64 = state
        .backend
        .query_one(
            &format!("SELECT COALESCE(MAX(CAST(track_number AS INTEGER)), 0) FROM tracks WHERE album_id = {p1}"),
            &[&id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|row| row.into_iter().next()?.as_i64())
        .unwrap_or(0);

    let expected = if expected_tracks > 0 {
        expected_tracks
    } else {
        max_tag_total
    };
    let complete = expected > 0 && actual_tracks >= expected;
    let missing = if expected > actual_tracks {
        expected - actual_tracks
    } else {
        0
    };

    Ok(Json(json!({
        "album_id": id,
        "album_title": album.title,
        "actual_tracks": actual_tracks,
        "expected_tracks": expected,
        "missing": missing,
        "complete": complete,
        "completeness_pct": if expected > 0 { (actual_tracks as f64 / expected as f64 * 100.0).round() } else { 100.0 },
    })))
}

// ---------------------------------------------------------------------------
// PUT /albums/{id} — update album metadata (mirrors POST /metadata/albums/{id}/edit)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub(super) struct AlbumUpdate {
    title: Option<String>,
    artist_id: Option<i64>,
    artist_name: Option<String>,
    genre: Option<String>,
    year: Option<i32>,
    label: Option<String>,
}

pub(super) async fn update_album(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<AlbumUpdate>,
) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let mut album = match repo.get(id) {
        Ok(Some(a)) => a,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };

    if let Some(ref v) = body.title {
        album.title = v.clone();
    }
    if let Some(ref v) = body.genre {
        album.genre = Some(v.clone());
    }
    if let Some(v) = body.year {
        album.year = Some(v);
    }
    if let Some(ref v) = body.label {
        album.label = Some(v.clone());
    }
    // artist_id takes priority; fall back to artist_name resolution
    if let Some(aid) = body.artist_id {
        album.artist_id = Some(aid);
        // Refresh artist_name for the JSON response
        let artist_repo = ArtistRepo::with_backend(state.backend.clone());
        album.artist_name = artist_repo.get(aid).ok().flatten().map(|a| a.name);
    } else if let Some(ref name) = body.artist_name {
        let artist_repo = ArtistRepo::with_backend(state.backend.clone());
        if let Ok(Some(artist)) = artist_repo.get_by_name(name) {
            album.artist_id = artist.id;
            album.artist_name = Some(artist.name);
        } else if let Ok(artist) = artist_repo.get_or_create(name, None, None) {
            album.artist_id = artist.id;
            album.artist_name = Some(artist.name);
        }
    }

    repo.update(&album).ok();

    Json(album.to_json()).into_response()
}

// --- Album extended metadata endpoints ---

/// GET /api/v1/library/albums/{id}/metadata
/// Returns all extended metadata key-value pairs for an album.
pub(super) async fn album_metadata_get(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    use tune_core::db::album_metadata_repo::AlbumMetadataRepo;

    let repo = AlbumMetadataRepo::with_backend(state.backend.clone());
    match repo.get_all(id) {
        Ok(meta) => Json(json!(meta)).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// PUT /api/v1/library/albums/{id}/metadata
/// Batch-sets album-level extended metadata from a JSON object body.
/// DB only — propagating album fields into each track's file tags stays the
/// job of POST /library/write-tags {album_id}, so a save is never O(tracks).
pub(super) async fn album_metadata_put(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    use tune_core::db::album_metadata_repo::AlbumMetadataRepo;

    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    match album_repo.get(id) {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, "album not found").into_response(),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }

    let repo = AlbumMetadataRepo::with_backend(state.backend.clone());
    if let Err(e) = repo.set_batch(id, &body) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }

    Json(json!({"status": "ok", "fields": body.len()})).into_response()
}

#[derive(Deserialize)]
pub(super) struct BatchAlbumUpdate {
    album_ids: Vec<i64>,
    genre: Option<String>,
    year: Option<i32>,
    artist_id: Option<i64>,
    artist_name: Option<String>,
    label: Option<String>,
}

pub(super) async fn batch_update_albums(
    State(state): State<AppState>,
    Json(body): Json<BatchAlbumUpdate>,
) -> impl IntoResponse {
    let repo = AlbumRepo::with_backend(state.backend.clone());
    let artist_repo = ArtistRepo::with_backend(state.backend.clone());
    let mut updated = 0i64;

    let resolved_artist_id = if let Some(aid) = body.artist_id {
        Some(aid)
    } else if let Some(ref name) = body.artist_name {
        artist_repo
            .get_by_name(name)
            .ok()
            .flatten()
            .and_then(|a| a.id)
            .or_else(|| {
                artist_repo
                    .get_or_create(name, None, None)
                    .ok()
                    .and_then(|a| a.id)
            })
    } else {
        None
    };

    for &id in &body.album_ids {
        let mut album = match repo.get(id) {
            Ok(Some(a)) => a,
            _ => continue,
        };
        if let Some(ref g) = body.genre {
            album.genre = Some(g.clone());
        }
        if let Some(y) = body.year {
            album.year = Some(y);
        }
        if let Some(ref l) = body.label {
            album.label = Some(l.clone());
        }
        if let Some(aid) = resolved_artist_id {
            album.artist_id = Some(aid);
        }
        if repo.update(&album).is_ok() {
            updated += 1;
        }
    }

    Json(serde_json::json!({ "updated": updated, "total": body.album_ids.len() })).into_response()
}

#[cfg(test)]
mod tests_grouping {
    use super::attach_grouping;
    use std::collections::HashMap;
    use tune_core::db::models::Track;

    fn track(id: i64, title: &str) -> Track {
        let mut t = Track::new(title.to_string());
        t.id = Some(id);
        t
    }

    /// Sans GROUPING en base, la réponse d'un album est celle d'avant #2130 :
    /// aucune clé supplémentaire. C'est le cas mesuré sur les bibliothèques
    /// réelles, donc le cas qui doit rester gratuit.
    #[test]
    fn attach_grouping_leaves_tracks_untouched_when_absent() {
        let items = vec![track(1, "I. Allegro"), track(2, "II. Adagio")];
        let out = attach_grouping(items, &HashMap::new());
        assert_eq!(out.len(), 2);
        for v in &out {
            assert!(
                v.get("grouping").is_none(),
                "aucune clé grouping ne doit apparaître sans donnée"
            );
        }
        assert_eq!(out[0]["title"], "I. Allegro");
    }

    /// Une valeur en base ressort sur la piste concernée, et sur elle seule.
    #[test]
    fn attach_grouping_reports_the_value_on_the_right_track() {
        let items = vec![track(1, "I. Allegro"), track(2, "Bonus")];
        let mut map = HashMap::new();
        map.insert(2i64, "Titres bonus".to_string());
        let out = attach_grouping(items, &map);
        assert!(out[0].get("grouping").is_none());
        assert_eq!(out[1]["grouping"], "Titres bonus");
    }

    /// Une entrée pour une piste absente de l'album ne contamine personne.
    #[test]
    fn attach_grouping_ignores_unknown_track_ids() {
        let items = vec![track(1, "I. Allegro")];
        let mut map = HashMap::new();
        map.insert(99i64, "Autre album".to_string());
        let out = attach_grouping(items, &map);
        assert!(out[0].get("grouping").is_none());
    }
}
