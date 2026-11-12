//! Réglage « Balises du classique » (#8325, ESL63P) :
//! `GET` / `PUT /api/v1/system/settings/classical-tags`.
//!
//! Le réglage, sa validation et son stockage vivent dans
//! `tune_core::metadata::balises_classique`. Une modification relance la
//! relecture de fond des balises classiques
//! (`routes::library::relecture_classique::relancer`), pas un scan.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use tune_core::metadata::balises_classique as bc;

use crate::state::AppState;

fn reponse(reglage: &bc::BalisesClassique, relecture: bool) -> Json<Value> {
    Json(json!({
        "classical_tags": reglage,
        "defaults": bc::BalisesClassique::default(),
        "standard_names": {
            "conductor": bc::STANDARD_CHEF,
            "work": bc::STANDARD_OEUVRE,
            "movement": bc::STANDARD_MOUVEMENT,
        },
        "max_tags_per_role": bc::MAX_BALISES_PAR_ROLE,
        "reread_started": relecture,
    }))
}

pub(super) async fn get_classical_tags(State(state): State<AppState>) -> Json<Value> {
    reponse(&bc::lire(&state.backend), false)
}

/// Pose le réglage. Réservé à l'administrateur : il change la lecture de
/// toute la bibliothèque. Un rôle absent du corps reprend sa valeur par
/// défaut ; `{}` revient donc aux valeurs par défaut.
pub(super) async fn set_classical_tags(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(saisie): Json<bc::Saisie>,
) -> Response {
    let backend = state.backend.clone();
    let ecrit = tokio::task::spawn_blocking(move || bc::ecrire(&backend, saisie)).await;
    match ecrit {
        Ok(Ok((ancien, nouveau))) => {
            let relecture = ancien != nouveau
                && crate::routes::library::relecture_classique::relancer(&state, ancien).await;
            reponse(&nouveau, relecture).into_response()
        }
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e, "code": "invalid_classical_tags" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    //! Aucun témoin ne pose un réglage NON défaut : le miroir est global au
    //! binaire de test, et d'autres témoins y scannent des fichiers.
    use super::*;

    #[tokio::test]
    async fn le_get_rend_les_defauts_et_le_put_invalide_est_refuse() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let Json(v) = get_classical_tags(State(state.clone())).await;
        assert_eq!(v["classical_tags"], v["defaults"]);
        assert_eq!(
            v["classical_tags"]["ensemble"],
            json!(["ENSEMBLE", "ORCHESTRA", "CHOIR"])
        );
        assert_eq!(v["classical_tags"]["band"], "classical");

        let saisie: bc::Saisie = serde_json::from_value(json!({ "ensemble": ["CHŒUR"] })).unwrap();
        let r = set_classical_tags(
            crate::auth::RequireAdmin,
            State(state.clone()),
            Json(saisie),
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);

        // Les défauts reposés : aucune relecture (rien n'a changé).
        let r = set_classical_tags(
            crate::auth::RequireAdmin,
            State(state.clone()),
            Json(bc::Saisie::default()),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let corps = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&corps).unwrap();
        assert_eq!(v["reread_started"], false);
    }

    #[test]
    fn le_demarrage_charge_le_reglage_avant_le_scan() {
        let source = include_str!("../../bootstrap.rs");
        let charge = source
            .find("balises_classique::charger(&state.backend)")
            .expect("le démarrage ne charge plus le réglage « Balises du classique »");
        let scan = source
            .find("crate::auto_scan::spawn_auto_scan(")
            .expect("scan de démarrage introuvable");
        assert!(
            charge < scan,
            "le réglage doit être chargé AVANT le scan de démarrage"
        );
    }
}
