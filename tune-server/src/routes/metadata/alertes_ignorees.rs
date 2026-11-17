//! #8958 — « Albums douteux » : marquer une alerte comme normale, revoir et
//! annuler les alertes ignorées.
//!
//! * `GET /metadata/doubtful/ignored` — la liste des alertes ignorées, avec de
//!   quoi les reconnaître (titre, album, chemin) ;
//! * `POST /metadata/doubtful/ignored` `{"track_id": 12, "reason": "very_short"}`
//!   — marque l'alerte de cette piste comme normale. Le SERVEUR choisit la
//!   portée (piste ou album) selon la nature de l'alerte
//!   ([`tune_core::db::alertes_douteuses_ignorees::portee_pour`]) ;
//! * `DELETE /metadata/doubtful/ignored/{scope}/{target_id}/{reason}` — annule.
//!
//! Les deux écritures sont réservées à l'administrateur, comme
//! « Écrire l'artiste du dossier » sur le même écran : le choix vaut pour
//! toute la bibliothèque (voir le module de `tune_core`).

use std::collections::HashSet;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::alertes_douteuses_ignorees::{self as ignorees, Portee};
use tune_core::db::backend::ToSqlValue;
use tune_core::db::models::Track;

use crate::state::AppState;

/// Les alertes ignorées, en ensemble, pour retirer leurs raisons d'une piste.
pub(super) struct Ignorees(HashSet<(&'static str, i64, String)>);

impl Ignorees {
    pub(super) fn charger(db: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>) -> Ignorees {
        let lignes = ignorees::lister(db).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "alertes_ignorees_lecture");
            Vec::new()
        });
        Ignorees(
            lignes
                .into_iter()
                .map(|a| (a.portee.as_str(), a.cible_id, a.raison))
                .collect(),
        )
    }

    /// L'alerte `raison` de la piste `t` est-elle ignorée (piste ou album) ?
    pub(super) fn contient(&self, t: &Track, raison: &str) -> bool {
        let piste = t.id.is_some_and(|id| self.0.contains(&("track", id, raison.to_string())));
        let album = t
            .album_id
            .is_some_and(|id| self.0.contains(&("album", id, raison.to_string())));
        piste || album
    }
}

/// La portée sous laquelle l'alerte `raison` de `t` serait mémorisée.
pub(super) fn portee_de(t: &Track, raison: &str) -> Portee {
    ignorees::portee_pour(raison, t.album_id, t.album_title.as_deref())
}

fn maintenant() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn erreur(code: StatusCode, message: impl Into<String>) -> axum::response::Response {
    (code, Json(json!({"error": message.into()}))).into_response()
}

fn raison_connue(raison: &str) -> bool {
    ignorees::RAISONS.contains(&raison)
}

/// Ce qui permet de reconnaître une alerte ignorée dans la liste.
fn decrire(state: &AppState, a: &ignorees::AlerteIgnoree) -> Value {
    let db = &state.backend;
    let id = &a.cible_id as &dyn ToSqlValue;
    let texte = |v: Option<&tune_core::db::backend::SqlValue>| v.and_then(|v| v.as_string());
    let mut v = json!({
        "scope": a.portee.as_str(),
        "target_id": a.cible_id,
        "reason": a.raison,
        "ignored_at": a.ignoree_le,
    });
    match a.portee {
        Portee::Piste => {
            if let Ok(Some(l)) = db.query_one(
                "SELECT t.title, t.file_path, al.title, ar.name FROM tracks t \
                 LEFT JOIN albums al ON t.album_id = al.id \
                 LEFT JOIN artists ar ON t.artist_id = ar.id WHERE t.id = ?",
                &[id],
            ) {
                v["title"] = json!(texte(l.first()));
                v["file_path"] = json!(texte(l.get(1)));
                v["album_title"] = json!(texte(l.get(2)));
                v["artist_name"] = json!(texte(l.get(3)));
            }
        }
        Portee::Album => {
            if let Ok(Some(l)) = db.query_one(
                "SELECT al.title, (SELECT COUNT(*) FROM tracks t WHERE t.album_id = al.id), \
                 (SELECT MIN(t.file_path) FROM tracks t WHERE t.album_id = al.id) \
                 FROM albums al WHERE al.id = ?",
                &[id],
            ) {
                v["album_title"] = json!(texte(l.first()));
                v["track_count"] = json!(l.get(1).and_then(|x| x.as_i64()).unwrap_or(0));
                v["file_path"] = json!(texte(l.get(2)));
            }
        }
    }
    v
}

pub(super) async fn lister(State(state): State<AppState>) -> impl IntoResponse {
    // Un choix fait sur une piste ou un album supprimé ne doit pas passer à
    // l'élément qui reprendrait son identifiant.
    if let Err(e) = ignorees::purger_orphelines(&state.backend) {
        tracing::warn!(error = %e, "alertes_ignorees_purge");
    }
    match ignorees::lister(&state.backend) {
        Ok(lignes) => {
            let items: Vec<Value> = lignes.iter().map(|a| decrire(&state, a)).collect();
            Json(json!({"total": items.len(), "items": items})).into_response()
        }
        Err(e) => {
            tracing::warn!(error = %e, "alertes_ignorees_lecture");
            erreur(StatusCode::INTERNAL_SERVER_ERROR, e)
        }
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct Demande {
    track_id: i64,
    reason: String,
}

pub(super) async fn ignorer(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(d): Json<Demande>,
) -> impl IntoResponse {
    if !raison_connue(&d.reason) {
        return erreur(StatusCode::BAD_REQUEST, format!("unknown reason: {}", d.reason));
    }
    let piste = match tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone())
        .get(d.track_id)
    {
        Ok(Some(t)) => t,
        Ok(None) => return erreur(StatusCode::NOT_FOUND, "track not found"),
        Err(e) => return erreur(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let portee = portee_de(&piste, &d.reason);
    let cible = match portee {
        Portee::Album => piste.album_id.unwrap_or(d.track_id),
        Portee::Piste => d.track_id,
    };
    match ignorees::ignorer(&state.backend, portee, cible, &d.reason, maintenant()) {
        Ok(ajoutee) => Json(json!({
            "status": "ok",
            "scope": portee.as_str(),
            "target_id": cible,
            "reason": d.reason,
            "added": ajoutee,
        }))
        .into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "alertes_ignorees_ecriture");
            erreur(StatusCode::INTERNAL_SERVER_ERROR, e)
        }
    }
}

pub(super) async fn retablir(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Path((scope, target_id, reason)): Path<(String, i64, String)>,
) -> impl IntoResponse {
    let Some(portee) = Portee::depuis(&scope) else {
        return erreur(StatusCode::BAD_REQUEST, format!("unknown scope: {scope}"));
    };
    if !raison_connue(&reason) {
        return erreur(StatusCode::BAD_REQUEST, format!("unknown reason: {reason}"));
    }
    match ignorees::retablir(&state.backend, portee, target_id, &reason) {
        Ok(true) => Json(json!({"status": "ok", "removed": true})).into_response(),
        Ok(false) => erreur(StatusCode::NOT_FOUND, "not ignored"),
        Err(e) => {
            tracing::warn!(error = %e, "alertes_ignorees_ecriture");
            erreur(StatusCode::INTERNAL_SERVER_ERROR, e)
        }
    }
}
