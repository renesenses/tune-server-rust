//! #4907 — choisir le répertoire de lecture quand la même musique vit dans
//! plusieurs dossiers de musique.
//!
//! | route | rôle |
//! |---|---|
//! | `GET /library/repertoires/ordre` | l'ordre effectif des dossiers de musique |
//! | `PUT /library/repertoires/ordre` | le régler (`{"ordre": [...]}`) |
//! | `GET /library/albums/{id}/repertoire-prefere` | le répertoire préféré d'un album |
//! | `PUT /library/albums/{id}/repertoire-prefere` | le poser (`{"racine": "..."}`) |
//! | `DELETE /library/albums/{id}/repertoire-prefere` | le retirer : retour à la règle par défaut |
//!
//! La règle elle-même vit dans `tune_core::library::exemplaires` ; ces
//! routes ne font que lire et écrire les deux réglages qu'elle consulte.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::library::exemplaires as ex;

use crate::state::AppState;

fn ordre_actuel(state: &AppState) -> Value {
    let db = &*state.backend;
    let dossiers = ex::dossiers_de_musique(db);
    let regle = ex::ordre_regle(db);
    json!({
        "ordre": ex::ordre_effectif(&dossiers, &regle),
        "music_dirs": dossiers,
        "regle": regle,
    })
}

/// `GET /library/repertoires/ordre` — l'ordre EFFECTIF (celui que la lecture
/// applique à qualité égale), les dossiers configurés, et le réglage brut.
pub(super) async fn ordre_get(State(state): State<AppState>) -> Json<Value> {
    Json(ordre_actuel(&state))
}

#[derive(Deserialize)]
pub(super) struct CorpsOrdre {
    ordre: Vec<String>,
}

/// `PUT /library/repertoires/ordre` — classe les dossiers de musique. Chaque
/// entrée doit être un dossier de musique configuré ; un dossier omis garde
/// sa place relative, après ceux que la liste classe.
pub(super) async fn ordre_put(
    State(state): State<AppState>,
    Json(corps): Json<CorpsOrdre>,
) -> Response {
    let dossiers = ex::dossiers_de_musique(&*state.backend);
    let mut ordre: Vec<String> = Vec::new();
    let mut inconnus: Vec<String> = Vec::new();
    for d in &corps.ordre {
        let d = tune_core::scanner::walker::normalize_path(d);
        if !dossiers.contains(&d) {
            inconnus.push(d);
        } else if !ordre.contains(&d) {
            ordre.push(d);
        }
    }
    if !inconnus.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "ces chemins ne sont pas des dossiers de musique configurés",
                "inconnus": inconnus,
                "music_dirs": dossiers,
            })),
        )
            .into_response();
    }
    let valeur = serde_json::to_string(&ordre).unwrap_or_else(|_| "[]".into());
    if let Err(e) = SettingsRepo::with_backend(state.backend.clone())
        .set(ex::CLE_ORDRE_DES_REPERTOIRES, &valeur)
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    Json(ordre_actuel(&state)).into_response()
}

fn album_connu(state: &AppState, id: i64) -> Result<(), Response> {
    match AlbumRepo::with_backend(state.backend.clone()).get(id) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(StatusCode::NOT_FOUND.into_response()),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()),
    }
}

fn preference(state: &AppState, id: i64) -> Value {
    json!({
        "album_id": id,
        "racine": ex::racine_preferee(&*state.backend, id),
    })
}

/// `GET /library/albums/{id}/repertoire-prefere` — `racine: null` quand
/// l'album suit la règle par défaut.
pub(super) async fn preference_get(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    if let Err(r) = album_connu(&state, id) {
        return r;
    }
    Json(preference(&state, id)).into_response()
}

#[derive(Deserialize)]
pub(super) struct CorpsPreference {
    racine: String,
}

/// `PUT /library/albums/{id}/repertoire-prefere` — la racine doit être un
/// dossier de musique configuré.
pub(super) async fn preference_put(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(corps): Json<CorpsPreference>,
) -> Response {
    if let Err(r) = album_connu(&state, id) {
        return r;
    }
    let racine = tune_core::scanner::walker::normalize_path(&corps.racine);
    let dossiers = ex::dossiers_de_musique(&*state.backend);
    if !dossiers.contains(&racine) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "cette racine n'est pas un dossier de musique configuré",
                "racine": racine,
                "music_dirs": dossiers,
            })),
        )
            .into_response();
    }
    if let Err(e) = ex::poser_racine_preferee(&*state.backend, id, &racine) {
        return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response();
    }
    tracing::info!(album_id = id, racine = %racine, "album_repertoire_prefere_pose");
    Json(preference(&state, id)).into_response()
}

/// `DELETE /library/albums/{id}/repertoire-prefere` — retour à la règle par
/// défaut. `retire` dit si une préférence existait.
pub(super) async fn preference_delete(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    if let Err(r) = album_connu(&state, id) {
        return r;
    }
    match ex::retirer_racine_preferee(&*state.backend, id) {
        Ok(retire) => {
            let mut v = preference(&state, id);
            v["retire"] = json!(retire);
            Json(v).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}
