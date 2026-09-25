//! `/library/collection-folders` — ranger les collections dans un arbre de
//! dossiers (#4853, Gros Bidon, fil 1907 ; décision de Bertrand du 24/09/2026 :
//! arbre, profondeur maximale 3).
//!
//! Routes ADDITIVES : `/library/collections` et `/library/smart-collections`
//! gardent leur forme — les applis iOS, macOS et Android continuent de voir
//! une liste plate. Le modèle et ses refus vivent dans
//! [`tune_core::db::collection_folder_repo`].
//!
//! ⚠️ Une collection est désignée par la PAIRE `(kind, id)` : `collection`
//! (réglage JSON `collections`) ou `smart` (`smart_collections`). Les deux
//! espaces d'identifiants se recouvrent (l'id 1 est « favorites » ET
//! « 💎 Audiophile » sur le .18).
//!
//! # Collection supprimée
//!
//! Double filet :
//! * `DELETE /library/collections/{id}` retire la ligne de rangement — l'id
//!   d'une collection simple est `max + 1`, donc RÉUTILISÉ : sans ce
//!   nettoyage, la collection créée après la suppression de la dernière
//!   hériterait de son dossier ;
//! * à la lecture, une ligne dont la collection n'existe plus est ignorée
//!   (les collections intelligentes, dont l'id n'est jamais réutilisé, sont
//!   supprimées par une autre caisse). Le `GET` ne purge rien.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::AppError;
use crate::state::AppState;
use tune_core::db::collection_folder_repo::{
    CollectionFolder, CollectionFolderRepo, FolderError, KINDS, MAX_DEPTH, kind_valide,
};

use super::collections::dossiers_stockes;

fn repo(state: &AppState) -> CollectionFolderRepo {
    CollectionFolderRepo::with_backend(state.backend.clone())
}

fn refus(e: FolderError) -> AppError {
    match e {
        FolderError::Invalid(m) => AppError::bad_request(m),
        FolderError::NotFound(m) => AppError::not_found(m),
        FolderError::Conflict(m) => AppError::conflict(m),
        FolderError::Db(m) => {
            tracing::error!("collection-folders: {m}");
            AppError::internal("dossiers de collections illisibles")
        }
    }
}

fn erreur_base(e: String) -> AppError {
    refus(FolderError::Db(e))
}

/// Une collection telle que l'arbre la rend : de quoi l'afficher sans
/// recroiser `/library/collections` ni `/library/smart-collections`.
fn resume(kind: &str, c: &Value) -> Value {
    json!({
        "kind": kind,
        "id": c.get("id").cloned().unwrap_or(Value::Null),
        "name": c.get("name").cloned().unwrap_or(Value::Null),
        "description": c.get("description").cloned().unwrap_or(Value::Null),
        "icon": c.get("icon").cloned().unwrap_or(Value::Null),
        "color": c.get("color").cloned().unwrap_or(Value::Null),
    })
}

/// Toutes les collections existantes, dans l'ordre de leurs listes plates :
/// les simples dans l'ordre du réglage, puis les intelligentes par nom.
fn collections_existantes(state: &AppState) -> Result<Vec<(String, i64, Value)>, AppError> {
    let mut toutes = Vec::new();
    for c in dossiers_stockes(state) {
        if let Some(id) = c.get("id").and_then(|v| v.as_i64()) {
            toutes.push(("collection".to_string(), id, resume("collection", &c)));
        }
    }
    let lignes = state
        .backend
        .query_many(
            "SELECT id, name, description, icon, color FROM smart_collections ORDER BY name",
            &[],
        )
        .map_err(erreur_base)?;
    for r in lignes {
        let Some(id) = r.first().and_then(|v| v.as_i64()) else {
            continue;
        };
        let c = json!({
            "id": id,
            "name": r.get(1).and_then(|v| v.as_string()),
            "description": r.get(2).and_then(|v| v.as_string()),
            "icon": r.get(3).and_then(|v| v.as_string()),
            "color": r.get(4).and_then(|v| v.as_string()),
        });
        toutes.push(("smart".to_string(), id, resume("smart", &c)));
    }
    Ok(toutes)
}

fn collection_existe(state: &AppState, kind: &str, id: i64) -> Result<bool, AppError> {
    match kind {
        "collection" => Ok(dossiers_stockes(state)
            .iter()
            .any(|c| c.get("id").and_then(|v| v.as_i64()) == Some(id))),
        "smart" => Ok(state
            .backend
            .query_one("SELECT id FROM smart_collections WHERE id = ?", &[&id])
            .map_err(erreur_base)?
            .is_some()),
        _ => Ok(false),
    }
}

fn verifier_kind(kind: &str) -> Result<(), AppError> {
    if kind_valide(kind) {
        Ok(())
    } else {
        Err(AppError::bad_request(format!(
            "sorte de collection inconnue : « {kind} » — sortes admises : {}",
            KINDS.join(", ")
        )))
    }
}

fn dossier_json(f: &CollectionFolder) -> Value {
    json!({
        "id": f.id,
        "name": f.name,
        "parent_id": f.parent_id,
        "position": f.position,
    })
}

/// `GET /library/collection-folders` — l'arbre entier.
///
/// ```json
/// { "max_depth": 3,
///   "folders": [ { "id", "name", "parent_id", "position", "depth",
///                  "folders": [...], "collections": [...] } ],
///   "collections": [ { "kind", "id", "name", "description", "icon", "color",
///                      "folder_id", "position" } ] }
/// ```
///
/// La racine rend TOUTES les collections non rangées dans un dossier : celles
/// rangées explicitement à la racine d'abord (dans leur ordre), puis les
/// autres dans l'ordre de leurs listes plates, `position: null`.
pub(super) async fn tree(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let r = repo(&state);
    let folders = r.list_folders().map_err(erreur_base)?;
    let items = r.list_items().map_err(erreur_base)?;
    let existantes = collections_existantes(&state)?;

    let par_cle: HashMap<(String, i64), &Value> = existantes
        .iter()
        .map(|(k, id, v)| ((k.clone(), *id), v))
        .collect();
    let mut rangees: HashMap<(String, i64), (Option<i64>, i64)> = HashMap::new();
    for i in &items {
        // Ligne orpheline (collection supprimée) : ignorée, pas purgée.
        if par_cle.contains_key(&(i.kind.clone(), i.collection_id)) {
            rangees.insert((i.kind.clone(), i.collection_id), (i.folder_id, i.position));
        }
    }
    // Une ligne qui vise un dossier disparu retombe à la racine.
    let ids_dossiers: std::collections::HashSet<i64> = folders.iter().map(|f| f.id).collect();

    let mut par_dossier: HashMap<Option<i64>, Vec<(i64, Value)>> = HashMap::new();
    let mut non_rangees: Vec<Value> = Vec::new();
    for (kind, id, v) in &existantes {
        match rangees.get(&(kind.clone(), *id)) {
            Some((folder_id, pos)) if folder_id.is_none_or(|f| ids_dossiers.contains(&f)) => {
                let mut c = (*v).clone();
                c["folder_id"] = json!(folder_id);
                c["position"] = json!(pos);
                par_dossier.entry(*folder_id).or_default().push((*pos, c));
            }
            _ => {
                let mut c = (*v).clone();
                c["folder_id"] = Value::Null;
                c["position"] = Value::Null;
                non_rangees.push(c);
            }
        }
    }
    for liste in par_dossier.values_mut() {
        liste.sort_by_key(|(p, _)| *p);
    }

    fn construire(
        parent: Option<i64>,
        depth: usize,
        folders: &[CollectionFolder],
        par_dossier: &mut HashMap<Option<i64>, Vec<(i64, Value)>>,
    ) -> Vec<Value> {
        let mut enfants: Vec<&CollectionFolder> =
            folders.iter().filter(|f| f.parent_id == parent).collect();
        enfants.sort_by_key(|f| (f.position, f.id));
        enfants
            .into_iter()
            .map(|f| {
                let mut d = dossier_json(f);
                // Garde contre un cycle écrit hors du dépôt : on s'arrête.
                let sous = if depth < folders.len() {
                    construire(Some(f.id), depth + 1, folders, par_dossier)
                } else {
                    Vec::new()
                };
                let cols: Vec<Value> = par_dossier
                    .remove(&Some(f.id))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(_, c)| c)
                    .collect();
                d["depth"] = json!(depth);
                d["folders"] = json!(sous);
                d["collections"] = json!(cols);
                d
            })
            .collect()
    }
    let arbre = construire(None, 1, &folders, &mut par_dossier);
    let mut racine: Vec<Value> = par_dossier
        .remove(&None)
        .unwrap_or_default()
        .into_iter()
        .map(|(_, c)| c)
        .collect();
    racine.extend(non_rangees);

    Ok(Json(json!({
        "max_depth": MAX_DEPTH,
        "folders": arbre,
        "collections": racine,
    })))
}

#[derive(Deserialize)]
pub(super) struct CreateFolderBody {
    name: String,
    #[serde(default)]
    parent_id: Option<i64>,
}

/// `POST /library/collection-folders` — `{name, parent_id?}`.
pub(super) async fn create_folder(
    State(state): State<AppState>,
    Json(body): Json<CreateFolderBody>,
) -> Result<impl IntoResponse, AppError> {
    let f = repo(&state)
        .create_folder(&body.name, body.parent_id)
        .map_err(refus)?;
    Ok((StatusCode::CREATED, Json(dossier_json(&f))))
}

#[derive(Deserialize)]
pub(super) struct RenameFolderBody {
    name: String,
}

/// `PATCH /library/collection-folders/{id}` — `{name}`.
pub(super) async fn rename_folder(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<RenameFolderBody>,
) -> Result<Json<Value>, AppError> {
    let f = repo(&state).rename_folder(id, &body.name).map_err(refus)?;
    Ok(Json(dossier_json(&f)))
}

/// Corps d'un déplacement. `parent_id` / `folder_id` à `null` — ou absent —
/// désigne la racine : pour réordonner sans changer de parent, le client
/// renvoie le parent actuel avec la nouvelle `position`.
#[derive(Deserialize)]
pub(super) struct MoveFolderBody {
    parent_id: Option<i64>,
    #[serde(default)]
    position: Option<i64>,
}

/// `POST /library/collection-folders/{id}/move` — `{parent_id: n|null, position?}`.
pub(super) async fn move_folder(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<MoveFolderBody>,
) -> Result<Json<Value>, AppError> {
    let f = repo(&state)
        .move_folder(id, body.parent_id, body.position)
        .map_err(refus)?;
    Ok(Json(dossier_json(&f)))
}

/// `DELETE /library/collection-folders/{id}` — le contenu remonte au parent ;
/// aucune collection n'est supprimée.
pub(super) async fn delete_folder(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, AppError> {
    repo(&state).delete_folder(id).map_err(refus)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub(super) struct PlaceItemBody {
    folder_id: Option<i64>,
    #[serde(default)]
    position: Option<i64>,
}

/// `POST /library/collection-folders/items/{kind}/{id}` —
/// `{folder_id: n|null, position?}` : range, déplace ou réordonne.
pub(super) async fn place_item(
    State(state): State<AppState>,
    Path((kind, id)): Path<(String, i64)>,
    Json(body): Json<PlaceItemBody>,
) -> Result<Json<Value>, AppError> {
    verifier_kind(&kind)?;
    if !collection_existe(&state, &kind, id)? {
        return Err(AppError::not_found(format!(
            "collection {kind} {id} introuvable"
        )));
    }
    let item = repo(&state)
        .place_item(&kind, id, body.folder_id, body.position)
        .map_err(refus)?;
    Ok(Json(json!(item)))
}

/// `DELETE /library/collection-folders/items/{kind}/{id}` — la collection
/// retourne à la racine. 204 même si elle n'était pas rangée.
pub(super) async fn remove_item(
    State(state): State<AppState>,
    Path((kind, id)): Path<(String, i64)>,
) -> Result<StatusCode, AppError> {
    verifier_kind(&kind)?;
    repo(&state).remove_item(&kind, id).map_err(erreur_base)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Retire du rangement une collection SIMPLE supprimée (voir l'en-tête).
/// Un échec est journalisé sans faire échouer la suppression : la lecture
/// filtre de toute façon la ligne orpheline.
pub(super) fn oublier_collection_simple(state: &AppState, id: i64) {
    if let Err(e) = repo(state).remove_item("collection", id) {
        tracing::warn!("collection {id} supprimée, rangement non retiré: {e} (#4853)");
    }
}
