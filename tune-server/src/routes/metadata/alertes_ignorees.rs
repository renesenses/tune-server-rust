//! #8958 — « Albums douteux » : marquer une alerte comme normale, revoir et
//! annuler les alertes ignorées.
//!
//! * `GET /metadata/doubtful/ignored` — la liste des alertes ignorées, avec de
//!   quoi les reconnaître (titre, album, chemin). Lecture SEULE : un choix
//!   dont la clé ne correspond plus à rien est gardé (fichiers momentanément
//!   absents) et signalé `found: false` ; il n'est retiré qu'en fin de scan
//!   complet sain.
//! * `POST /metadata/doubtful/ignored` `{"track_id": 12, "reason": "very_short"}`
//!   — marque l'alerte de cette piste comme normale. 400 si la raison ne
//!   s'applique pas à la piste. Le SERVEUR choisit la portée et la clé
//!   stable ([`tune_core::db::alertes_douteuses_ignorees`]).
//! * `DELETE /metadata/doubtful/ignored` `{"scope", "key", "title", "reason"}`
//!   — annule (les champs tels que la liste les rend).
//!
//! Les deux écritures sont réservées à l'administrateur, comme
//! « Écrire l'artiste du dossier » sur le même écran ; `/metadata/doubtful`
//! dit à l'écran s'il peut les offrir (`can_ignore`).

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::alertes_douteuses_ignorees::{self as ignorees, AlerteIgnoree, Portee};
use tune_core::db::backend::{DbBackend, SqlValue, ToSqlValue};
use tune_core::db::models::Track;

use crate::state::AppState;

/// Les alertes ignorées, pour retirer leurs raisons d'une piste.
pub(super) struct Ignorees(Vec<AlerteIgnoree>);

impl Ignorees {
    pub(super) fn charger(db: &std::sync::Arc<dyn DbBackend>) -> Ignorees {
        Ignorees(ignorees::lister(db).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "alertes_ignorees_lecture");
            Vec::new()
        }))
    }

    /// L'alerte `raison` de la piste `t` est-elle ignorée (piste ou album) ?
    pub(super) fn contient(&self, t: &Track, raison: &str) -> bool {
        self.0.iter().any(|a| a.couvre(t, raison))
    }
}

/// La portée sous laquelle l'alerte `raison` de `t` serait mémorisée (sans
/// le repli « album sans dossier exploitable → piste », rare, que le POST
/// tranche et rend).
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

fn texte(v: Option<&SqlValue>) -> Option<String> {
    v.and_then(|v| v.as_string())
}

/// Ce que la liste rend d'une alerte ignorée ; la clé, le titre et la
/// portée servent tels quels à l'annulation.
fn decrire(db: &std::sync::Arc<dyn DbBackend>, a: &AlerteIgnoree) -> Value {
    let mut v = json!({
        "scope": a.portee.as_str(),
        "key": a.cle,
        "title": a.titre,
        "reason": a.raison,
        "ignored_at": a.ignoree_le,
        "label": a.libelle,
        "found": false,
    });
    match a.portee {
        Portee::Piste => {
            let sql = format!(
                "SELECT t.id, t.title, al.title, {chemin} FROM tracks t \
                 LEFT JOIN albums al ON t.album_id = al.id \
                 WHERE {cle} = ? LIMIT 1",
                chemin = "COALESCE(NULLIF(t.file_path, ''), t.cue_media_path)",
                cle = ignorees::SQL_CLE_PISTE,
            );
            if let Ok(Some(l)) = db.query_one(&sql, &[&a.cle as &dyn ToSqlValue]) {
                v["found"] = json!(true);
                v["track_id"] = json!(l.first().and_then(|x| x.as_i64()));
                v["track_title"] = json!(texte(l.get(1)));
                v["album_title"] = json!(texte(l.get(2)));
                v["file_path"] = json!(texte(l.get(3)));
            }
        }
        Portee::Album => {
            // Les pistes sous le dossier, de cet album : la règle du filtre.
            let sql = "SELECT t.album_id, COALESCE(NULLIF(t.file_path, ''), t.cue_media_path) \
                 FROM tracks t JOIN albums al ON t.album_id = al.id WHERE al.title = ?";
            let lignes = db
                .query_many(sql, &[&a.titre as &dyn ToSqlValue])
                .unwrap_or_default();
            let dedans: Vec<(Option<i64>, String)> = lignes
                .iter()
                .filter_map(|l| {
                    let c = texte(l.get(1))?;
                    ignorees::sous_le_dossier(&c, &a.cle)
                        .then(|| (l.first().and_then(|x| x.as_i64()), c))
                })
                .collect();
            if !dedans.is_empty() {
                v["found"] = json!(true);
                v["album_id"] = json!(dedans[0].0);
                v["track_count"] = json!(dedans.len());
            }
            v["album_title"] = json!(a.titre);
            v["file_path"] = json!(a.cle);
        }
    }
    v
}

pub(super) async fn lister(State(state): State<AppState>) -> impl IntoResponse {
    match ignorees::lister(&state.backend) {
        Ok(lignes) => {
            let items: Vec<Value> = lignes.iter().map(|a| decrire(&state.backend, a)).collect();
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

/// Les chemins des pistes d'un album (dossier de l'album).
fn chemins_de_l_album(db: &std::sync::Arc<dyn DbBackend>, album_id: i64) -> Vec<String> {
    db.query_many(
        "SELECT COALESCE(NULLIF(file_path, ''), cue_media_path) FROM tracks WHERE album_id = ?",
        &[&album_id as &dyn ToSqlValue],
    )
    .unwrap_or_default()
    .iter()
    .filter_map(|l| texte(l.first()))
    .collect()
}

pub(super) async fn ignorer(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(d): Json<Demande>,
) -> impl IntoResponse {
    if !ignorees::RAISONS.contains(&d.reason.as_str()) {
        return erreur(
            StatusCode::BAD_REQUEST,
            format!("unknown reason: {}", d.reason),
        );
    }
    let piste = match tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone())
        .get(d.track_id)
    {
        Ok(Some(t)) => t,
        Ok(None) => return erreur(StatusCode::NOT_FOUND, "track not found"),
        Err(e) => return erreur(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    if !ignorees::raisons_de(&piste).contains(&d.reason.as_str()) {
        return erreur(
            StatusCode::BAD_REQUEST,
            format!("reason does not apply to this track: {}", d.reason),
        );
    }
    let chemins = piste
        .album_id
        .map(|id| chemins_de_l_album(&state.backend, id))
        .unwrap_or_default();
    let Some(alerte) = ignorees::alerte_pour(&piste, &d.reason, &chemins, maintenant()) else {
        return erreur(StatusCode::BAD_REQUEST, "track has no stable key (no path)");
    };
    match ignorees::ignorer(&state.backend, &alerte) {
        Ok(ajoutee) => Json(json!({
            "status": "ok",
            "scope": alerte.portee.as_str(),
            "key": alerte.cle,
            "title": alerte.titre,
            "reason": alerte.raison,
            "added": ajoutee,
        }))
        .into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "alertes_ignorees_ecriture");
            erreur(StatusCode::INTERNAL_SERVER_ERROR, e)
        }
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct Annulation {
    scope: String,
    key: String,
    #[serde(default)]
    title: String,
    reason: String,
}

pub(super) async fn retablir(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(d): Json<Annulation>,
) -> impl IntoResponse {
    let Some(portee) = Portee::depuis(&d.scope) else {
        return erreur(
            StatusCode::BAD_REQUEST,
            format!("unknown scope: {}", d.scope),
        );
    };
    match ignorees::retablir(&state.backend, portee, &d.key, &d.title, &d.reason) {
        Ok(true) => Json(json!({"status": "ok", "removed": true})).into_response(),
        Ok(false) => erreur(StatusCode::NOT_FOUND, "not ignored"),
        Err(e) => {
            tracing::warn!(error = %e, "alertes_ignorees_ecriture");
            erreur(StatusCode::INTERNAL_SERVER_ERROR, e)
        }
    }
}
