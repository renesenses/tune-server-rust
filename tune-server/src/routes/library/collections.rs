use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::state::AppState;
use tune_core::db::album_repo::AlbumRepo;

use super::album_order::{CollectionOrder, CollectionSort, sort_albums};
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
    /// `artist` (défaut), `title`, `year`, `release_date`, `added_at` (date
    /// d'ajout à la bibliothèque), ou `added` pour l'ordre d'ajout au dossier.
    sort: Option<String>,
    /// `asc` (défaut) ou `desc` — sur la clé principale seulement, les
    /// valeurs manquantes restant en dernier (Bertrand, 16/09/2026).
    order: Option<String>,
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

/// Champ STOCKÉ (jamais servi tel quel) : le titre et l'artiste de chaque
/// album AU MOMENT DU RANGEMENT, indexés par identifiant.
///
/// 🔴 #901 (Lulu) — la liste des albums manquants ne sert à rien si elle est
/// une suite de numéros. Or ces albums ne sont plus en base : leur titre n'est
/// lisible NULLE PART au moment où on voudrait l'afficher. Le seul instant où
/// il l'est encore, c'est quand l'album est rangé (ou quand le dossier est
/// ouvert alors qu'il vit encore). On l'écrit donc là, et pas ailleurs.
const ETIQUETTES: &str = "album_labels";

/// Le nom d'un album, tel qu'il est au moment où on le regarde.
fn etiquette_de(album: &tune_core::db::models::Album) -> Value {
    json!({ "title": album.title, "artist": album.artist_name })
}

/// Les étiquettes conservées d'un dossier, indexées par identifiant d'album.
fn etiquettes_stockees(collection: &Value) -> serde_json::Map<String, Value> {
    collection
        .get(ETIQUETTES)
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default()
}

/// La liste des albums manquants — NOMMÉE quand on a su garder le nom.
///
/// `title`/`artist` valent `null` pour un album rangé avant que ce champ
/// n'existe et mort sans que le dossier ait jamais été rouvert entre-temps :
/// il ne reste alors que son identifiant, et on ne l'invente pas.
fn albums_manquants(collection: &Value, morts: &[i64]) -> Vec<Value> {
    let etiquettes = etiquettes_stockees(collection);
    morts
        .iter()
        .map(|id| {
            let e = etiquettes.get(&id.to_string());
            let champ = |nom: &str| {
                e.and_then(|e| e.get(nom))
                    .filter(|v| !v.is_null())
                    .cloned()
                    .unwrap_or(Value::Null)
            };
            json!({ "id": id, "title": champ("title"), "artist": champ("artist") })
        })
        .collect()
}

/// Rend un dossier tel qu'il est SERVI : `album_ids` réduit aux albums encore
/// présents, et les identifiants morts dits à voix haute.
///
/// 🔴 #901 — `orphan_album_ids` portait, MALGRÉ SON NOM, un nombre. Il porte
/// désormais ce que son nom annonce : la LISTE des identifiants. Le nombre
/// reste publié, sous un nom qui le dit — `orphan_album_count` — et
/// `orphan_albums` donne le détail nommé.
///
/// ⚠️ Compatibilité : un client d'avant #901 lit `orphan_album_ids` derrière
/// un `typeof === 'number'` (CollectionsV2.svelte, v0.9.161). Une liste lui
/// fait donc masquer la mention — il ne plante pas, n'affiche ni `NaN` ni un
/// compte faux : il retombe exactement sur l'écran d'avant v0.9.161. C'est le
/// prix assumé pour que la clé cesse de mentir sur son contenu.
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
    let manquants = albums_manquants(collection, &morts);
    let mut servi = collection.clone();
    if let Some(obj) = servi.as_object_mut() {
        obj.insert("album_count".into(), json!(vivants.len()));
        obj.insert("orphan_album_ids".into(), json!(morts));
        obj.insert("orphan_album_count".into(), json!(morts.len()));
        obj.insert("orphan_albums".into(), json!(manquants));
        obj.insert("album_ids".into(), json!(vivants));
        // Les étiquettes sont une RÉSERVE, pas une donnée d'écran : un
        // dossier de 2 000 albums doublerait la réponse pour rien.
        obj.remove(ETIQUETTES);
    }
    Ok(servi)
}

/// Écrit les étiquettes d'un dossier SANS toucher au reste.
///
/// Relit le réglage juste avant d'écrire et ne modifie que `album_labels` du
/// dossier visé : un rangement fait entre-temps par un autre appel est donc
/// conservé. Rend `true` si quelque chose a été écrit.
fn conserver_etiquettes(
    settings: &tune_core::db::settings_repo::SettingsRepo,
    collection_id: i64,
    nouvelles: &serde_json::Map<String, Value>,
) -> bool {
    if nouvelles.is_empty() {
        return false;
    }
    let mut collections: Vec<Value> = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let Some(collection) = collections
        .iter_mut()
        .find(|c| c.get("id").and_then(|v| v.as_i64()) == Some(collection_id))
    else {
        return false;
    };
    let mut etiquettes = etiquettes_stockees(collection);
    let mut change = false;
    for (id, valeur) in nouvelles {
        if etiquettes.get(id) != Some(valeur) {
            etiquettes.insert(id.clone(), valeur.clone());
            change = true;
        }
    }
    if !change {
        return false;
    }
    if let Some(obj) = collection.as_object_mut() {
        obj.insert(ETIQUETTES.into(), Value::Object(etiquettes));
    }
    match serde_json::to_string(&collections) {
        Ok(s) => settings.set("collections", &s).is_ok(),
        Err(e) => {
            tracing::warn!("dossier {collection_id}: étiquettes non conservées: {e} (#901)");
            false
        }
    }
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
    // `AlbumRepo::get` laisse `added_at` à `None` (la colonne n'est pas dans
    // `select_album()`) : sans cette passe, le tri « date d'ajout » serait
    // un tri sur rien, et l'écran ne verrait jamais la date. Une seule
    // requête groupée pour tout le dossier (#3397). Un échec ne casse pas
    // la liste : elle sort sans date, et le journal le dit.
    match album_repo.added_at_by_ids(&album_ids) {
        Ok(par_id) => {
            for a in &mut albums {
                if let Some(id) = a.id {
                    a.added_at = par_id.get(&id).copied();
                }
            }
        }
        Err(e) => {
            tracing::warn!("dossier {id}: added_at_by_ids a échoué — liste sans date d'ajout: {e}")
        }
    }
    // #901 — RATTRAPAGE. Les albums vivants du dossier sont déjà chargés ici :
    // relever leur nom ne coûte pas une requête de plus. C'est le seul moyen
    // de nommer un jour les albums rangés AVANT que ce champ n'existe — quand
    // ils mourront, leur étiquette sera déjà là. Rien n'est écrit si rien n'a
    // changé, donc cette écriture ne se produit qu'une fois par dossier.
    let mut releve = serde_json::Map::new();
    for album in &albums {
        if let Some(aid) = album.id {
            releve.insert(aid.to_string(), etiquette_de(album));
        }
    }
    if conserver_etiquettes(&settings, id, &releve) {
        tracing::info!(
            "dossier {id}: {} étiquette(s) d'album conservées pour la liste des manquants (#901)",
            releve.len()
        );
    }
    sort_albums(
        &mut albums,
        CollectionSort::parse(query.sort.as_deref()),
        CollectionOrder::parse(query.order.as_deref()),
    );
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
    // #901 — le nom de l'album est lisible MAINTENANT, et seulement
    // maintenant : on le range avec lui. Un album introuvable ou une base
    // muette ne fait pas échouer le rangement, elle laisse l'étiquette vide.
    let etiquette = match AlbumRepo::with_backend(state.backend.clone()).get(path.album_id) {
        Ok(Some(album)) => Some(etiquette_de(&album)),
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(
                "dossier {}: album {} illisible, rangé sans étiquette: {e} (#901)",
                path.id,
                path.album_id
            );
            None
        }
    };
    if let Some(etiquette) = etiquette {
        let mut etiquettes = etiquettes_stockees(collection);
        etiquettes.insert(path.album_id.to_string(), etiquette);
        if let Some(obj) = collection.as_object_mut() {
            obj.insert(ETIQUETTES.into(), Value::Object(etiquettes));
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
    // #901 — un album sorti du dossier n'y « manque » plus : son étiquette
    // n'a plus de raison d'être gardée, et la réserve ne doit pas enfler.
    if let Some(etiquettes) = collection
        .get_mut(ETIQUETTES)
        .and_then(|v| v.as_object_mut())
    {
        etiquettes.remove(&path.album_id.to_string());
    }
    settings
        .set("collections", &serde_json::to_string(&collections)?)
        .ok();
    Ok(Json(
        json!({"removed": true, "collection_id": path.id, "album_id": path.album_id}),
    ))
}
