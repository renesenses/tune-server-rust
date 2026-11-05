//! #7849 — les notes personnelles en Markdown sur un artiste, un album ou une
//! piste.
//!
//! - `GET /library/notes?item_type=&q=&limit=` : les notes du profil, les plus
//!   récentes d'abord, filtrées par type et par texte ;
//! - `GET /library/notes/{item_type}/{item_id}` : une note, 404 sans elle ;
//! - `PUT /library/notes/{item_type}/{item_id}` `{ "texte_md": "…" }` : crée
//!   ou remplace ; un texte vide supprime ;
//! - `DELETE /library/notes/{item_type}/{item_id}`.
//!
//! Le profil est celui de l'appelant (`ActiveProfile`), jamais un paramètre :
//! une note est privée à son profil. Le texte est gardé brut ; le client web
//! l'assainit au rendu.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::notes_personnelles::{self as notes, TypeDObjet};

use crate::error::AppError;
use crate::routes::active_profile::ActiveProfile;
use crate::state::AppState;

/// Plafond d'une note : de quoi écrire plusieurs pages, pas un fichier.
const TAILLE_MAX: usize = 64 * 1024;
const LIMITE_PAR_DEFAUT: i64 = 200;
const LIMITE_MAX: i64 = 1000;

fn type_d_objet(mot: &str) -> Result<TypeDObjet, AppError> {
    TypeDObjet::lire(mot).ok_or_else(|| {
        AppError::bad_request(format!(
            "type d'objet inconnu « {mot} » : artist, album ou track"
        ))
    })
}

#[derive(Deserialize)]
pub(super) struct ListeQuery {
    item_type: Option<String>,
    q: Option<String>,
    limit: Option<i64>,
}

/// GET /library/notes
pub(super) async fn list_notes(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Query(params): Query<ListeQuery>,
) -> Result<Json<Value>, AppError> {
    let genre = params
        .item_type
        .as_deref()
        .filter(|t| !t.is_empty())
        .map(type_d_objet)
        .transpose()?;
    let limite = params
        .limit
        .unwrap_or(LIMITE_PAR_DEFAUT)
        .clamp(1, LIMITE_MAX);
    let liste = notes::lister(
        state.backend.as_ref(),
        profile.id(),
        genre,
        params.q.as_deref(),
        limite,
    )
    .map_err(AppError::internal)?;
    Ok(Json(json!({
        "count": liste.len(),
        "notes": liste,
    })))
}

/// GET /library/notes/{item_type}/{item_id}
pub(super) async fn get_note(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path((item_type, item_id)): Path<(String, i64)>,
) -> Result<Json<Value>, AppError> {
    let genre = type_d_objet(&item_type)?;
    let note = notes::lire(state.backend.as_ref(), profile.id(), genre, item_id)
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::not_found("aucune note sur cet objet"))?;
    Ok(Json(json!(note)))
}

#[derive(Deserialize)]
pub(super) struct NoteBody {
    texte_md: String,
}

/// PUT /library/notes/{item_type}/{item_id}
pub(super) async fn put_note(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path((item_type, item_id)): Path<(String, i64)>,
    Json(body): Json<NoteBody>,
) -> Result<Json<Value>, AppError> {
    let genre = type_d_objet(&item_type)?;
    if body.texte_md.len() > TAILLE_MAX {
        return Err(AppError::bad_request(format!(
            "note trop longue : {} octets au plus",
            TAILLE_MAX
        )));
    }
    let ecrite = notes::ecrire(
        state.backend.as_ref(),
        profile.id(),
        genre,
        item_id,
        &body.texte_md,
    )
    .map_err(AppError::internal)?;
    Ok(Json(match ecrite {
        Some(note) => json!(note),
        None => json!({ "item_type": genre, "item_id": item_id, "deleted": true }),
    }))
}

/// DELETE /library/notes/{item_type}/{item_id}
pub(super) async fn delete_note(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path((item_type, item_id)): Path<(String, i64)>,
) -> Result<Json<Value>, AppError> {
    let genre = type_d_objet(&item_type)?;
    let existait = notes::supprimer(state.backend.as_ref(), profile.id(), genre, item_id)
        .map_err(AppError::internal)?;
    Ok(Json(json!({
        "item_type": genre,
        "item_id": item_id,
        "deleted": existait,
    })))
}
