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

/// Au plus quatre pochettes par collection : la mosaïque du client en compte
/// quatre, en chercher davantage serait du trafic pour rien.
const POCHETTES_MAX: usize = 4;

/// Garde-fou sur la requête : une collection peut porter des milliers d'albums,
/// et on n'a besoin que des premiers de CHACUNE. On ne demande donc jamais plus
/// que ce plafond d'identifiants, toutes collections confondues.
const POCHETTES_IDS_MAX: usize = 600;

/// Combien d'albums lire par collection pour y trouver quatre pochettes
/// DISTINCTES.
///
/// Seize fois la cible, et non quatre : un album peut n'avoir aucune pochette —
/// il ne consomme alors pas de case mais bien une place dans cette fenêtre — et
/// plusieurs albums d'une même édition partagent la leur. Une fenêtre trop
/// courte rendrait deux ou trois pochettes là où la collection en a dix, sans
/// que rien ne le signale : la mosaïque cyclerait, ce qui reste crédible à
/// l'œil.
///
/// Ce n'est pas une garantie. Une collection dont les soixante-quatre premiers
/// albums partagent trois pochettes en montrera trois — « si possible », a dit
/// Bertrand, et c'est bien la limite du possible sans lire la collection
/// entière.
const POCHETTES_FENETRE: usize = POCHETTES_MAX * 16;

pub(super) async fn list_collections(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut data = settings
        .get("collections")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
        .unwrap_or_default();

    // Pochettes de mosaïque, jointes ICI plutôt que réclamées album par album.
    //
    // Le nouveau client dessine la pochette d'une collection comme une mosaïque
    // des pochettes qu'elle contient. La collection ne porte que des
    // `album_ids` : sans ce champ, le client irait chercher chaque album un par
    // un, pour n'en garder que quatre.
    //
    // UNE SEULE requête pour toutes les collections : on rassemble les premiers
    // identifiants de chacune, on lit leurs pochettes d'un coup, puis on
    // recompose en respectant l'ordre PROPRE à chaque collection.
    //
    // Champ ADDITIF : `covers` s'ajoute, rien n'est retiré. Un client qui
    // l'ignore ne voit aucune différence.
    let mut voulus: Vec<i64> = Vec::new();
    for c in &data {
        if let Some(ids) = c.get("album_ids").and_then(|v| v.as_array()) {
            // Fenêtre large : voir `POCHETTES_FENETRE`.
            for id in ids
                .iter()
                .filter_map(|v| v.as_i64())
                .take(POCHETTES_FENETRE)
            {
                if voulus.len() >= POCHETTES_IDS_MAX {
                    break;
                }
                if !voulus.contains(&id) {
                    voulus.push(id);
                }
            }
        }
    }

    // id -> (chemin de pochette, cle d'ALBUM : artiste + titre)
    let mut par_album: std::collections::HashMap<i64, (String, String)> =
        std::collections::HashMap::new();
    if !voulus.is_empty() {
        // Les identifiants viennent de `as_i64` : ce sont des entiers, jamais du
        // texte. Les insérer directement ne peut donc pas porter d'injection, et
        // évite d'avoir à lier un nombre variable de paramètres.
        let liste = voulus
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            // Artiste et titre en DEUX colonnes : la clé se compose côté Rust.
            // Les concaténer en SQL demanderait un séparateur, donc un caractère
            // de contrôle échappé — et `\u{{…}}` est lu au lexage, avant
            // `format!`, ce qui ne compile pas.
            "SELECT al.id, al.cover_path, LOWER(COALESCE(ar.name, '')) AS artiste, \
             LOWER(COALESCE(al.title, '')) AS titre \
             FROM albums al LEFT JOIN artists ar ON ar.id = al.artist_id \
             WHERE al.id IN ({liste}) \
             AND al.cover_path IS NOT NULL AND al.cover_path <> ''"
        );
        if let Ok(rows) = state.backend.query_many(&sql, &[]) {
            for r in &rows {
                if let (Some(id), Some(c), Some(a), Some(t)) = (
                    r.first().and_then(|v| v.as_i64()),
                    r.get(1).and_then(|v| v.as_string()),
                    r.get(2).and_then(|v| v.as_string()),
                    r.get(3).and_then(|v| v.as_string()),
                ) {
                    // Séparateur non imprimable : aucun nom ne le contient, donc
                    // « A » + « BC » ne peut pas se confondre avec « AB » + « C ».
                    par_album.insert(id, (c, format!("{a}\u{1f}{t}")));
                }
            }
        }
    }

    for c in &mut data {
        let mut vues: Vec<String> = Vec::new();
        let mut cles: Vec<String> = Vec::new();
        if let Some(ids) = c.get("album_ids").and_then(|v| v.as_array()) {
            for id in ids.iter().filter_map(|v| v.as_i64()) {
                let Some((cover, cle)) = par_album.get(&id) else {
                    continue;
                };
                // Dédoublonnage sur l'ALBUM d'abord : un coffret est stocké
                // comme plusieurs albums, chacun avec son propre fichier de
                // pochette en cache — quatre chemins, une seule image (coffret
                // Górecki, collection « Classique », 02/09/2026).
                if cles.iter().any(|k| k == cle) {
                    continue;
                }
                // Puis sur le CHEMIN : deux albums distincts peuvent malgré
                // tout partager une pochette.
                if vues.iter().any(|v| v == cover) {
                    continue;
                }
                cles.push(cle.clone());
                vues.push(cover.clone());
                if vues.len() == POCHETTES_MAX {
                    break;
                }
            }
        }
        if let Some(obj) = c.as_object_mut() {
            obj.insert("covers".to_string(), json!(vues));
        }
    }

    Json(json!(data))
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
    let album_ids: Vec<i64> = collection
        .get("album_ids")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default();
    let album_repo = AlbumRepo::with_backend(state.backend.clone());
    let mut albums: Vec<tune_core::db::models::Album> = album_ids
        .iter()
        .filter_map(|&aid| album_repo.get(aid).ok().flatten())
        .collect();
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
