//! #7148 — les routes de la sentinelle de streaming, réservées à
//! l'administrateur.
//!
//! - `GET  /api/v1/sentinelle/streaming` : configuration, binaire, historique ;
//! - `POST /api/v1/sentinelle/streaming/sonder` : un tour, tout de suite
//!   (passé si une zone joue) ;
//! - `PUT  /api/v1/sentinelle/streaming/config` : références, radio, exclus.
//!
//! Le binaire se nomme dans chaque réponse (`binaire.sha`) : c'est ce SHA
//! que le minuteur du .18 marque `sentinelle/streaming` sur GitHub, et que le
//! contrôleur de release exige. Il vient de `TUNE_GIT_SHA` à la COMPILATION ;
//! absent, il vaut `null` et aucun statut ne peut être publié — une porte
//! fermée plutôt qu'un SHA deviné.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use serde_json::{Value, json};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::orchestrator::sentinelle_streaming::{
    ConfigSentinelle, RapportSentinelle, lire_l_historique,
};

use crate::auth::RequireAdmin;
use crate::state::AppState;

/// Le SHA du commit dont ce binaire est construit, s'il a été donné (40
/// caractères hexadécimaux), sinon `None`.
pub fn sha_du_binaire() -> Option<&'static str> {
    sha_valide(option_env!("TUNE_GIT_SHA"))
}

fn sha_valide(brut: Option<&'static str>) -> Option<&'static str> {
    brut.map(str::trim)
        .filter(|s| s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()))
}

fn binaire() -> Value {
    json!({
        "version": tune_core::version(),
        "sha": sha_du_binaire(),
    })
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/streaming", get(etat))
        .route("/streaming/sonder", post(sonder))
        .route("/streaming/config", put(regler))
}

async fn etat(_admin: RequireAdmin, State(state): State<AppState>) -> Response {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let historique = lire_l_historique(&settings);
    Json(json!({
        "binaire": binaire(),
        "config": ConfigSentinelle::lire(&settings),
        "dernier": historique.iter().find(|r| r.statut != "passe"),
        "historique": historique,
    }))
    .into_response()
}

async fn sonder(_admin: RequireAdmin, State(state): State<AppState>) -> Response {
    let rapport: RapportSentinelle = state.orchestrator.sentinelle_streaming().await;
    let mut corps = serde_json::to_value(&rapport).unwrap_or_else(|_| json!({}));
    corps["binaire"] = binaire();
    Json(corps).into_response()
}

async fn regler(
    _admin: RequireAdmin,
    State(state): State<AppState>,
    Json(config): Json<ConfigSentinelle>,
) -> Response {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    match config.ecrire(&settings) {
        Ok(()) => Json(json!({ "config": config })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::sha_valide;

    #[test]
    fn seul_un_sha_complet_nomme_le_binaire() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(sha_valide(Some(sha)), Some(sha));
        assert_eq!(sha_valide(None), None);
        assert_eq!(sha_valide(Some("")), None);
        assert_eq!(
            sha_valide(Some("c818e07ec")),
            None,
            "un SHA court est refusé"
        );
        assert_eq!(
            sha_valide(Some("zz23456789abcdef0123456789abcdef01234567")),
            None
        );
    }
}
