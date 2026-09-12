use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use tune_core::db::tag_repo::{
    StreamingTagItem, TagRepo, is_taggable_item_type, item_type_rejette,
};

use crate::routes::active_profile::ActiveProfile;
use crate::state::AppState;

#[derive(Deserialize)]
struct CreateTag {
    name: String,
    color: Option<String>,
}

#[derive(Deserialize)]
struct UpdateTag {
    name: Option<String>,
    color: Option<String>,
}

#[derive(Deserialize)]
struct AddTagItem {
    item_type: String,
    item_id: i64,
}

#[derive(Deserialize)]
struct BatchTagRequest {
    item_type: String,
    item_ids: Vec<i64>,
}

/// Poser une etiquette sur un objet de STREAMING (#3699).
///
/// La designation est la PAIRE `source` + `source_id`, jamais un entier. Les
/// quatre champs d'affichage sont l'INSTANTANE pose a l'etiquetage — meme
/// forme que `POST /profiles/{id}/favorites/streaming/add`, y compris ses
/// `alias` : les ecrans envoient deja des objets qui nomment `artist_name`,
/// `album_title` et `cover_path`, et rien ne justifie de leur demander une
/// seconde forme.
#[derive(Deserialize)]
struct AddStreamingTagItem {
    item_type: String,
    #[serde(alias = "service")]
    source: String,
    #[serde(alias = "service_id", alias = "id")]
    source_id: String,
    title: Option<String>,
    #[serde(alias = "artist_name")]
    artist: Option<String>,
    #[serde(alias = "album_title")]
    album: Option<String>,
    #[serde(alias = "cover_path", alias = "cover")]
    cover_url: Option<String>,
}

/// Retirer une etiquette d'un objet de streaming.
///
/// Corps de requete et non segments de chemin : un `source_id` de Bandcamp
/// peut porter une barre oblique, et un chemin la couperait en deux. Meme
/// choix que `POST /profiles/{id}/favorites/streaming/remove`.
#[derive(Deserialize)]
struct RemoveStreamingTagItem {
    item_type: String,
    #[serde(alias = "service")]
    source: String,
    #[serde(alias = "service_id", alias = "id")]
    source_id: String,
}

/// `GET /tags/for-streaming?item_type=&source=&source_id=`.
#[derive(Deserialize)]
struct StreamingItemQuery {
    item_type: String,
    #[serde(alias = "service")]
    source: String,
    #[serde(alias = "service_id")]
    source_id: String,
}

#[derive(Deserialize)]
struct TagSearchQuery {
    q: Option<String>,
    item_type: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_tags).post(create_tag))
        .route("/search", get(search_tags))
        .route("/{id}", get(get_tag).put(update_tag).delete(delete_tag))
        .route("/{id}/items", get(list_tag_items).post(add_tag_item))
        .route("/{id}/items/batch", post(batch_tag_items))
        .route("/{id}/items/batch-remove", post(batch_untag_items))
        .route(
            "/{id}/items/{item_type}/{item_id}",
            axum::routing::delete(remove_tag_item),
        )
        // Etiquetage par SOURCE (#3699) — l'espace d'identifiants du
        // streaming, a cote de l'espace local ci-dessus. Deux `POST` plutot
        // qu'un `POST` et un `DELETE` de chemin : la designation voyage dans
        // le corps, parce qu'un `source_id` peut contenir une barre oblique.
        .route("/{id}/streaming-items", post(add_streaming_tag_item))
        .route(
            "/{id}/streaming-items/remove",
            post(remove_streaming_tag_item),
        )
        .route("/{id}/albums", get(list_tag_albums))
        .route("/{id}/tracks", get(list_tag_tracks))
        .route("/{id}/artists", get(list_tag_artists))
        .route("/{id}/playlists", get(list_tag_playlists))
        .route("/for/{item_type}/{item_id}", get(tags_for_item))
        // La jumelle de `/for/…` pour l'espace du streaming. En parametres de
        // requete et non en segments, meme raison que ci-dessus.
        .route("/for-streaming", get(tags_for_streaming_item))
}

async fn list_tags(State(state): State<AppState>, Query(q): Query<TagSearchQuery>) -> Json<Value> {
    let repo = TagRepo::with_backend(state.backend.clone());
    let items = repo
        .list_with_counts(q.item_type.as_deref())
        .unwrap_or_default();
    Json(json!(items))
}

async fn search_tags(
    State(state): State<AppState>,
    Query(q): Query<TagSearchQuery>,
) -> Json<Value> {
    let repo = TagRepo::with_backend(state.backend.clone());
    let query = q.q.unwrap_or_default();
    if query.is_empty() {
        let tags = repo.list().unwrap_or_default();
        return Json(json!(tags));
    }
    let tags = repo.search(&query).unwrap_or_default();
    Json(json!(tags))
}

async fn create_tag(
    State(state): State<AppState>,
    Json(body): Json<CreateTag>,
) -> impl IntoResponse {
    let repo = TagRepo::with_backend(state.backend.clone());
    if let Ok(Some(existing)) = repo.get_by_name(&body.name) {
        return Json(json!({ "id": existing.id, "exists": true })).into_response();
    }
    match repo.create(&body.name, body.color.as_deref()) {
        Ok(id) => (StatusCode::CREATED, Json(json!({ "id": id }))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn get_tag(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = TagRepo::with_backend(state.backend.clone());
    match repo.get(id) {
        Ok(Some(tag)) => Json(json!(tag)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn update_tag(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateTag>,
) -> impl IntoResponse {
    let repo = TagRepo::with_backend(state.backend.clone());
    match repo.update(id, body.name.as_deref(), body.color.as_deref()) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_tag(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    let repo = TagRepo::with_backend(state.backend.clone());
    match repo.delete(id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// Les objets d'une etiquette, BRUTS, dans les deux espaces d'identifiants.
///
/// #3699 — cette route ne rendait que l'espace local : une paire
/// `{item_type, item_id}` par ligne. Les objets de streaming, qui n'ont pas
/// d'entier, en etaient absents, et un client qui compte ici ne voyait pas la
/// moitie de ce que `/tags` lui annonçait.
///
/// Les deux familles se distinguent sans ambiguite : une ligne locale porte
/// `item_id` et `source: null`, une ligne de streaming porte `item_id: null`
/// et la paire. **Jamais un entier seul** — l'identifiant 1 de deux espaces
/// differents ne designe pas le meme objet.
async fn list_tag_items(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let repo = TagRepo::with_backend(state.backend.clone());
    let items = repo.all_items_by_tag(id).unwrap_or_default();
    let mut items: Vec<Value> = items
        .into_iter()
        .map(|(item_type, item_id)| {
            json!({
                "item_type": item_type,
                "item_id": item_id,
                "source": Value::Null,
                "source_id": Value::Null,
            })
        })
        .collect();
    for s in repo.all_streaming_items_by_tag(id).unwrap_or_default() {
        items.push(json!({
            "item_type": s.item_type,
            "item_id": Value::Null,
            "source": s.source,
            "source_id": s.source_id,
        }));
    }
    Json(json!({"tag_id": id, "items": items}))
}

/// Pose une etiquette sur un objet designe par `source` + `source_id`.
async fn add_streaming_tag_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<AddStreamingTagItem>,
) -> impl IntoResponse {
    if !is_taggable_item_type(&body.item_type) {
        return (StatusCode::BAD_REQUEST, item_type_rejette(&body.item_type)).into_response();
    }
    let repo = TagRepo::with_backend(state.backend.clone());
    let item = StreamingTagItem {
        item_type: body.item_type,
        source: body.source,
        source_id: body.source_id,
        title: body.title,
        artist: body.artist,
        album: body.album,
        cover_url: body.cover_url,
    };
    match repo.tag_streaming_item(id, &item) {
        Ok(_) => StatusCode::CREATED.into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

/// Retire une etiquette d'un objet de streaming.
async fn remove_streaming_tag_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<RemoveStreamingTagItem>,
) -> impl IntoResponse {
    let repo = TagRepo::with_backend(state.backend.clone());
    match repo.untag_streaming_item(id, &body.item_type, &body.source, &body.source_id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// Les etiquettes posees sur un objet de streaming.
async fn tags_for_streaming_item(
    State(state): State<AppState>,
    Query(q): Query<StreamingItemQuery>,
) -> Json<Value> {
    let repo = TagRepo::with_backend(state.backend.clone());
    let tags = repo
        .tags_for_streaming_item(&q.item_type, &q.source, &q.source_id)
        .unwrap_or_default();
    Json(json!(tags))
}

async fn add_tag_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<AddTagItem>,
) -> impl IntoResponse {
    if !is_taggable_item_type(&body.item_type) {
        return (StatusCode::BAD_REQUEST, item_type_rejette(&body.item_type)).into_response();
    }
    let repo = TagRepo::with_backend(state.backend.clone());
    match repo.tag_item(id, &body.item_type, body.item_id) {
        Ok(_) => StatusCode::CREATED.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn batch_tag_items(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<BatchTagRequest>,
) -> impl IntoResponse {
    if !is_taggable_item_type(&body.item_type) {
        return (StatusCode::BAD_REQUEST, item_type_rejette(&body.item_type)).into_response();
    }
    let repo = TagRepo::with_backend(state.backend.clone());
    match repo.batch_tag(id, &body.item_type, &body.item_ids) {
        Ok(count) => Json(json!({"tagged": count})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn batch_untag_items(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<BatchTagRequest>,
) -> impl IntoResponse {
    let repo = TagRepo::with_backend(state.backend.clone());
    match repo.batch_untag(id, &body.item_type, &body.item_ids) {
        Ok(count) => Json(json!({"untagged": count})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn remove_tag_item(
    State(state): State<AppState>,
    Path((id, item_type, item_id)): Path<(i64, String, i64)>,
) -> impl IntoResponse {
    let repo = TagRepo::with_backend(state.backend.clone());
    match repo.untag_item(id, &item_type, item_id) {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

/// Les albums d'une etiquette — LOCAUX **et** de STREAMING (#3699).
///
/// ## Pourquoi la moitie streaming n'est pas hydratee
///
/// Les albums locaux se resolvent par `AlbumRepo::get`, et un identifiant
/// introuvable est simplement omis : c'est la regle deja ecrite pour les
/// playlists plus bas. Les albums de streaming, eux, se rendent depuis
/// l'INSTANTANE pose a l'etiquetage — `title`, `artist`, `cover_url` — et
/// cette route n'appelle **jamais** Qobuz, Tidal ou Bandcamp.
///
/// C'est le troisieme point du ticket, et c'est deliberé. Un album de
/// streaming peut disparaitre du catalogue : le service le retire, la licence
/// change, le compte est deconnecte. Un ecran qui hydraterait chaque ligne
/// aupres du service attendrait sur le premier `source_id` mort, et le
/// deuxieme album ne s'afficherait jamais. Ici la ligne s'affiche toujours ;
/// seule sa pochette degrade, comme une pochette morte.
///
/// La moitie streaming porte `id: null` et la paire `source` + `source_id` —
/// la forme que le type `Album` du client web declare deja. **Jamais un entier
/// seul** : l'identifiant 1 ne designe pas le meme objet selon l'espace d'ou
/// il vient.
async fn list_tag_albums(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let tag_repo = TagRepo::with_backend(state.backend.clone());
    let album_ids = tag_repo.items_by_tag(id, "album").unwrap_or_default();
    let album_repo = tune_core::db::album_repo::AlbumRepo::with_backend(state.backend.clone());
    let mut albums: Vec<Value> = album_ids
        .into_iter()
        .filter_map(|aid| album_repo.get(aid).ok().flatten())
        .map(|a| a.to_json())
        .collect();
    for s in tag_repo
        .streaming_items_by_tag(id, "album")
        .unwrap_or_default()
    {
        albums.push(json!({
            "id": Value::Null,
            "title": s.title.clone().unwrap_or_default(),
            "artist_name": s.artist,
            "cover_path": s.cover_url,
            "source": s.source,
            "source_id": s.source_id,
        }));
    }
    Json(json!({"tag_id": id, "albums": albums, "count": albums.len()}))
}

async fn list_tag_tracks(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let tag_repo = TagRepo::with_backend(state.backend.clone());
    let track_ids = tag_repo.items_by_tag(id, "track").unwrap_or_default();
    let track_repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    let mut tracks: Vec<Value> = track_ids
        .into_iter()
        .filter_map(|tid| track_repo.get(tid).ok().flatten())
        .map(|t| t.to_json())
        .collect();
    // Meme regle que pour les albums (#3699) : rendu depuis l'instantane,
    // sans aucun appel au service.
    for s in tag_repo
        .streaming_items_by_tag(id, "track")
        .unwrap_or_default()
    {
        tracks.push(json!({
            "id": Value::Null,
            "title": s.title.clone().unwrap_or_default(),
            "artist_name": s.artist,
            "album_title": s.album,
            "cover_path": s.cover_url,
            "source": s.source,
            "source_id": s.source_id,
        }));
    }
    Json(json!({"tag_id": id, "tracks": tracks, "count": tracks.len()}))
}

async fn list_tag_artists(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    let tag_repo = TagRepo::with_backend(state.backend.clone());
    let artist_ids = tag_repo.items_by_tag(id, "artist").unwrap_or_default();
    let artist_repo = tune_core::db::artist_repo::ArtistRepo::with_backend(state.backend.clone());
    let mut artists: Vec<Value> = artist_ids
        .into_iter()
        .filter_map(|aid| artist_repo.get(aid).ok().flatten())
        .map(|a| {
            json!({
                "id": a.id,
                "name": a.name,
                "image_path": a.image_path,
            })
        })
        .collect();
    // Meme regle que pour les albums (#3699).
    for s in tag_repo
        .streaming_items_by_tag(id, "artist")
        .unwrap_or_default()
    {
        artists.push(json!({
            "id": Value::Null,
            "name": s.title.clone().unwrap_or_default(),
            "image_path": s.cover_url,
            "source": s.source,
            "source_id": s.source_id,
        }));
    }
    Json(json!({"tag_id": id, "artists": artists, "count": artists.len()}))
}

/// Les playlists d'une étiquette.
///
/// Même forme que ses trois sœurs (`albums`, `tracks`, `artists`) : une
/// enveloppe `{tag_id, <pluriel>, count}`, les objets résolus par leur dépôt,
/// et un identifiant introuvable simplement **omis** — `items_by_tag` peut
/// désigner une playlist supprimée entre-temps, et une liste amputée vaut
/// mieux qu'une erreur qui masquerait les autres.
///
/// La playlist locale porte un `INTEGER PRIMARY KEY` : contrairement au label,
/// elle entre dans `item_tags` telle quelle. C'est la même frontière que celle
/// tracée par `favorite_facets_repo` pour les favoris.
///
/// Les étiquettes, elles, sont communes au foyer : sans filtre, cette route
/// résolvait le nom et le nombre de pistes des playlists des AUTRES profils
/// (#2794). Un profil ne peut étiqueter que ce qu'il voit, donc restreindre la
/// résolution à ses propres playlists ne lui retire rien.
async fn list_tag_playlists(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(id): Path<i64>,
) -> Json<Value> {
    let tag_repo = TagRepo::with_backend(state.backend.clone());
    let playlist_ids = tag_repo.items_by_tag(id, "playlist").unwrap_or_default();
    let playlist_repo =
        tune_core::db::playlist_repo::PlaylistRepo::with_backend(state.backend.clone());
    let mut playlists: Vec<Value> = playlist_ids
        .into_iter()
        .filter_map(|pid| {
            playlist_repo
                .get_for_profile(pid, profile.id())
                .ok()
                .flatten()
        })
        .map(|p| {
            json!({
                "id": p.id,
                "name": p.name,
                "description": p.description,
                "track_count": p.track_count,
            })
        })
        .collect();
    // Meme regle que pour les albums (#3699). Pas de filtre par profil sur
    // cette moitie : une playlist de streaming n'appartient a aucun profil de
    // Tune — c'est celle du service, et la cloison de #2794 porte sur les
    // playlists LOCALES des autres profils.
    for s in tag_repo
        .streaming_items_by_tag(id, "playlist")
        .unwrap_or_default()
    {
        playlists.push(json!({
            "id": Value::Null,
            "name": s.title.clone().unwrap_or_default(),
            "description": Value::Null,
            "track_count": Value::Null,
            "source": s.source,
            "source_id": s.source_id,
        }));
    }
    Json(json!({"tag_id": id, "playlists": playlists, "count": playlists.len()}))
}

async fn tags_for_item(
    State(state): State<AppState>,
    Path((item_type, item_id)): Path<(String, i64)>,
) -> Json<Value> {
    let repo = TagRepo::with_backend(state.backend.clone());
    let tags = repo.tags_for_item(&item_type, item_id).unwrap_or_default();
    Json(json!(tags))
}
