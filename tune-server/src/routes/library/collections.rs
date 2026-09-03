use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::state::AppState;
use tune_core::db::album_repo::AlbumRepo;

use super::album_order::{CollectionSort, sort_albums};
use super::now_iso_utc;

#[derive(Deserialize)]
pub(super) struct CreateCollectionBody {
    name: String,
    description: Option<String>,
    /// Nom d’icône libre, rendu par le client. Envoyé par
    /// `createCollection(name, description, icon, color)` depuis toujours.
    icon: Option<String>,
    /// Couleur de la pastille du dossier, `#RGB` ou `#RRGGBB` (#3044).
    color: Option<String>,
}

/// Mise à jour partielle : seuls les champs présents sont écrasés. Les
/// dossiers créés avant #3044 n’ont pas de couleur et aucun écran ne
/// permettait de leur en donner une — `api.updateCollection` existait côté
/// client sans route en face.
#[derive(Deserialize, Default)]
pub(super) struct UpdateCollectionBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    color: Option<String>,
}

/// `col.color` est injecté tel quel dans un attribut `style` du client
/// (`style="background:{col.color}"`). On n’accepte donc que la forme rendue
/// par un `<input type="color">` : `#RGB` ou `#RRGGBB`. Tout le reste est
/// refusé à la porte plutôt que stocké puis recraché dans du CSS.
fn couleur_valide(couleur: &str) -> bool {
    let Some(chiffres) = couleur.strip_prefix('#') else {
        return false;
    };
    matches!(chiffres.len(), 3 | 6) && chiffres.chars().all(|c| c.is_ascii_hexdigit())
}

/// Refuse une couleur hors format ; `None` reste `None`.
fn verifier_couleur(couleur: &Option<String>) -> Result<(), AppError> {
    match couleur {
        Some(c) if !couleur_valide(c) => Err(AppError::bad_request(
            "color doit être au format #RGB ou #RRGGBB",
        )),
        _ => Ok(()),
    }
}

#[derive(Deserialize)]
pub(super) struct CollectionAlbumPath {
    id: i64,
    album_id: i64,
}

#[derive(Deserialize, Default)]
pub(super) struct CollectionAlbumsQuery {
    /// `artist` (défaut), `title`, `year`, ou `added` pour l'ordre d'ajout.
    sort: Option<String>,
}

/// Les identifiants stockés d'un dossier, tels quels.
fn ids_stockes(collection: &Value) -> Vec<i64> {
    collection
        .get("album_ids")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default()
}

/// Partage la liste d'appartenance d'un dossier : ce qui désigne encore un
/// album, et ce qui est MORT.
///
/// 🔴 #3285 (Lulu) — la tuile comptait `album_ids` (la liste d'appartenance
/// servie telle quelle) et l'en-tête comptait ce que la route avait rendu. Un
/// dossier est une simple liste d'identifiants rangée dans le réglage
/// `collections` ; RIEN ne la nettoie quand un album disparaît (suppression,
/// rescan qui réattribue un id, changement de chemin). L'écart affiché était
/// donc exactement le nombre d'identifiants orphelins — jamais nommé nulle
/// part. Le seuil « au-delà de 100 albums » du signalement n'était qu'une
/// corrélation avec la taille : il n'y a aucun `LIMIT` sur ce chemin.
///
/// Une ERREUR de base n'est pas une absence : elle remonte en 500, journalisée.
fn partager_ids(repo: &AlbumRepo, album_ids: &[i64]) -> Result<(Vec<i64>, Vec<i64>), AppError> {
    let vivants = repo.ids_existants(album_ids).map_err(|e| {
        tracing::error!("collections: existence des albums illisible: {e}");
        AppError::internal("lecture des albums impossible")
    })?;
    let (encore_la, morts): (Vec<i64>, Vec<i64>) = album_ids
        .iter()
        .copied()
        .partition(|id| vivants.contains(id));
    Ok((encore_la, morts))
}

/// Rend un dossier tel qu'il est SERVI : `album_ids` réduit aux albums encore
/// présents, et le nombre d'identifiants morts dit à voix haute.
///
/// ⚠️ La liste STOCKÉE n'est pas touchée. Un `GET` ne purge rien : un album
/// peut manquer parce qu'un disque n'est pas monté ou qu'un scan est en cours,
/// et une purge sur simple lecture détruirait un rangement fait à la main sans
/// que personne l'ait demandé. On signale, on ne détruit pas.
fn dossier_servi(repo: &AlbumRepo, collection: &Value) -> Result<Value, AppError> {
    let (vivants, morts) = partager_ids(repo, &ids_stockes(collection))?;
    if !morts.is_empty() {
        tracing::warn!(
            "dossier {:?}: {} identifiant(s) d'album sans album en base: {morts:?} (#3285)",
            collection.get("id"),
            morts.len()
        );
    }
    let mut servi = collection.clone();
    if let Some(obj) = servi.as_object_mut() {
        obj.insert("album_count".into(), json!(vivants.len()));
        obj.insert("orphan_album_ids".into(), json!(morts.len()));
        obj.insert("album_ids".into(), json!(vivants));
    }
    Ok(servi)
}

pub(super) async fn list_collections(
    State(state): State<AppState>,
) -> Result<Json<Value>, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let data: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
        .unwrap_or_default();
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let servis = data
        .iter()
        .map(|c| dossier_servi(&album_repo, c))
        .collect::<Result<Vec<Value>, AppError>>()?;
    Ok(Json(json!(servis)))
}

pub(super) async fn create_collection(
    State(state): State<AppState>,
    Json(body): Json<CreateCollectionBody>,
) -> Result<impl IntoResponse, AppError> {
    verifier_couleur(&body.color)?;
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let id = collections
        .iter()
        .filter_map(|c| c.get("id").and_then(|v| v.as_i64()))
        .max()
        .unwrap_or(0)
        + 1;

    let collection = json!({
        "id": id,
        "name": body.name,
        "description": body.description,
        "icon": body.icon,
        "color": body.color,
        "album_ids": [],
        "created_at": now_iso_utc(),
    });
    collections.push(collection.clone());
    settings
        .set("collections", &serde_json::to_string(&collections)?)
        .ok();

    Ok((StatusCode::CREATED, Json(collection)))
}

pub(super) async fn get_collection(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let found = collections
        .iter()
        .find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(id));
    // ⚠️ #3285 — cette route rend la liste STOCKÉE, verbatim, et c'est
    // délibéré. C'est la route du « ce qui est rangé », celle sur laquelle
    // `put_ne_perd_pas_les_albums` (collections_couleur_persistee.rs) vérifie
    // qu'un PUT ne mange pas l'appartenance. Aucun écran n'en tire un compte :
    // `api.ts` n'a même pas de `getCollection` pour les dossiers manuels — la
    // tuile lit `getCollections`, l'en-tête lit `getCollectionAlbums`. Y
    // filtrer les identifiants morts ne réconcilierait donc aucun affichage et
    // ferait perdre le seul point d'observation de ce qui est réellement rangé.
    match found {
        Some(c) => Json(c.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `PUT /library/collections/{id}` — renomme un dossier, ou lui pose une
/// icône et une couleur (#3044). Mise à jour partielle : les albums déjà
/// rangés et les champs non fournis sont laissés intacts.
pub(super) async fn update_collection(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateCollectionBody>,
) -> Result<impl IntoResponse, AppError> {
    verifier_couleur(&body.color)?;
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let found = collections
        .iter_mut()
        .find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(id));
    let Some(collection) = found else {
        return Err(AppError::not_found("collection not found"));
    };
    let Some(obj) = collection.as_object_mut() else {
        return Err(AppError::internal("collection mal formée"));
    };
    if let Some(name) = body.name {
        obj.insert("name".into(), json!(name));
    }
    if let Some(description) = body.description {
        obj.insert("description".into(), json!(description));
    }
    if let Some(icon) = body.icon {
        obj.insert("icon".into(), json!(icon));
    }
    if let Some(color) = body.color {
        obj.insert("color".into(), json!(color));
    }
    let mise_a_jour = collection.clone();
    settings
        .set("collections", &serde_json::to_string(&collections)?)
        .ok();
    Ok(Json(mise_a_jour))
}

pub(super) async fn delete_collection(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let before = collections.len();
    collections.retain(|c| c.get("id").and_then(|v| v.as_i64()) != Some(id));
    if collections.len() == before {
        return Err(AppError::not_found("collection not found"));
    }
    settings
        .set("collections", &serde_json::to_string(&collections)?)
        .ok();
    Ok(StatusCode::NO_CONTENT)
}

/// Les albums d'un dossier.
///
/// L'ordre par défaut est alphabétique par artiste (puis année, puis titre),
/// et non plus l'ordre d'ajout : personne ne pouvait remettre un dossier en
/// ordre, faute d'endpoint de réordonnancement (Lulu/JLuc, fil 1591, #2675).
/// `?sort=added` rend l'ordre historique, pour un dossier monté comme une
/// séquence d'écoute. Le tri est fait en Rust — voir `album_order`.
///
/// La forme de la réponse reste un TABLEAU nu : `getCollectionAlbums`
/// (`api.ts`) la lit en `any[]` et l'écran compte `collectionAlbums.length`.
/// Le compte des identifiants morts est donc dit ailleurs — dans le journal,
/// et dans `orphan_album_ids` sur la liste des dossiers (#3285).
pub(super) async fn collection_albums(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<CollectionAlbumsQuery>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let found = collections
        .iter()
        .find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(id));
    let Some(collection) = found else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let album_ids: Vec<i64> = ids_stockes(collection);
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    // 🔴 #3285 — c'était `filter_map(|aid| album_repo.get(aid).ok().flatten())`.
    // Le `.ok()` avalait une ERREUR de base exactement comme une absence : une
    // lecture qui échoue rendait le même écran qu'un album supprimé, sans une
    // ligne de journal. Les trois cas sont maintenant séparés.
    let mut albums: Vec<tune_core::db::models::Album> = Vec::with_capacity(album_ids.len());
    let mut morts: Vec<i64> = Vec::new();
    for &aid in &album_ids {
        match album_repo.get(aid) {
            Ok(Some(album)) => albums.push(album),
            Ok(None) => morts.push(aid),
            Err(e) => {
                tracing::error!("dossier {id}: album {aid} illisible: {e} (#3285)");
                return AppError::internal("lecture des albums impossible").into_response();
            }
        }
    }
    if !morts.is_empty() {
        tracing::warn!(
            "dossier {id}: {} identifiant(s) d'album sans album en base: {morts:?} (#3285)",
            morts.len()
        );
    }
    sort_albums(&mut albums, CollectionSort::parse(query.sort.as_deref()));
    let albums: Vec<Value> = albums.iter().map(|a| a.to_json()).collect();
    Json(json!(albums)).into_response()
}

pub(super) async fn add_album_to_collection(
    State(state): State<AppState>,
    Path(path): Path<CollectionAlbumPath>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let found = collections
        .iter_mut()
        .find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(path.id));
    let Some(collection) = found else {
        return Err(AppError::not_found("collection not found"));
    };
    let album_ids = collection
        .get_mut("album_ids")
        .and_then(|v| v.as_array_mut());
    match album_ids {
        Some(arr) => {
            let already = arr.iter().any(|v| v.as_i64() == Some(path.album_id));
            if !already {
                arr.push(json!(path.album_id));
            }
        }
        None => {
            if let Some(obj) = collection.as_object_mut() {
                obj.insert("album_ids".into(), json!([path.album_id]));
            }
        }
    }
    settings
        .set("collections", &serde_json::to_string(&collections)?)
        .ok();
    Ok(Json(
        json!({"added": true, "collection_id": path.id, "album_id": path.album_id}),
    ))
}

pub(super) async fn remove_album_from_collection(
    State(state): State<AppState>,
    Path(path): Path<CollectionAlbumPath>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let found = collections
        .iter_mut()
        .find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(path.id));
    let Some(collection) = found else {
        return Err(AppError::not_found("collection not found"));
    };
    if let Some(arr) = collection
        .get_mut("album_ids")
        .and_then(|v| v.as_array_mut())
    {
        arr.retain(|v| v.as_i64() != Some(path.album_id));
    }
    settings
        .set("collections", &serde_json::to_string(&collections)?)
        .ok();
    Ok(Json(
        json!({"removed": true, "collection_id": path.id, "album_id": path.album_id}),
    ))
}
