//! #6542 — réglages de la sortie Devialet AIR (expérimentale).
//!
//! `GET  /devialet-air/reglages` : l'option, l'indice de la source AIR et le
//! plafond de volume.
//! `POST /devialet-air/reglages` : `{ "enabled": bool, "source_index": 0..14 }`,
//! chaque champ facultatif.
//!
//! Le réglage est relu à chaque usage : désactiver coupe l'écoute au passage
//! suivant du fournisseur et refuse toute lecture ou commande sur-le-champ.
//! Activer fait apparaître les amplis au passage suivant (au plus 60 s).

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::info;

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::outputs::devialet_air::reglages;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/reglages", get(lire).post(regler))
}

fn etat(state: &AppState) -> Value {
    json!({
        "enabled": reglages::activee(&state.backend),
        "experimental": true,
        "source_index": reglages::indice_source(&state.backend),
        "source_index_default": reglages::INDICE_SOURCE_PAR_DEFAUT,
        "source_index_max": reglages::INDICE_SOURCE_MAX,
        "volume_ceiling_db": reglages::PLAFOND_DB,
        "max_sample_rate": 96_000,
    })
}

async fn lire(State(state): State<AppState>) -> Json<Value> {
    Json(etat(&state))
}

#[derive(Deserialize)]
struct Corps {
    enabled: Option<bool>,
    source_index: Option<i64>,
}

async fn regler(State(state): State<AppState>, Json(corps): Json<Corps>) -> impl IntoResponse {
    let r = SettingsRepo::with_backend(state.backend.clone());
    if let Some(i) = corps.source_index {
        if !(0..=i64::from(reglages::INDICE_SOURCE_MAX)).contains(&i) {
            return crate::message_api::refus(
                StatusCode::BAD_REQUEST,
                "source_index_out_of_range",
                "api.devialet.sourceIndexRange",
                &[("max", reglages::INDICE_SOURCE_MAX.to_string())],
            );
        }
        if let Err(e) = r.set(reglages::CLE_INDICE_SOURCE, &i.to_string()) {
            return erreur(e);
        }
    }
    if let Some(on) = corps.enabled
        && let Err(e) = r.set(reglages::CLE_ACTIVEE, if on { "true" } else { "false" })
    {
        return erreur(e);
    }
    let v = etat(&state);
    info!(enabled = %v["enabled"], source_index = %v["source_index"], "devialet_air_reglages");
    Json(v).into_response()
}

fn erreur(e: String) -> axum::response::Response {
    crate::message_api::refus_avec_cause(
        StatusCode::INTERNAL_SERVER_ERROR,
        "setting_not_saved",
        "api.settings.notSaved",
        &[],
        e,
    )
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::state::AppState;

    async fn appel(
        app: &axum::Router,
        methode: &str,
        chemin: &str,
        corps: Option<Value>,
    ) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(methode)
            .uri(chemin)
            .header("content-type", "application/json")
            .body(
                corps
                    .map(|c| Body::from(c.to_string()))
                    .unwrap_or_else(Body::empty),
            )
            .unwrap();
        let rep = app.clone().oneshot(req).await.unwrap();
        let statut = rep.status();
        let octets = axum::body::to_bytes(rep.into_body(), 64 * 1024)
            .await
            .unwrap();
        (
            statut,
            serde_json::from_slice(&octets).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn air_6542_reglages_desactives_par_defaut_puis_persistes() {
        let state = AppState::new(":memory:", 0, Default::default()).expect("état");
        let db = state.backend.clone();
        let app = crate::routes::router_with_plugins(state, Vec::new());
        let (statut, corps) = appel(&app, "GET", "/api/v1/devialet-air/reglages", None).await;
        assert_eq!(statut, StatusCode::OK);
        assert_eq!(corps["enabled"], false, "désactivée par défaut : {corps}");
        assert_eq!(corps["source_index"], 9);
        assert_eq!(corps["volume_ceiling_db"], -10.0);

        let (statut, corps) = appel(
            &app,
            "POST",
            "/api/v1/devialet-air/reglages",
            Some(serde_json::json!({"enabled": true, "source_index": 13})),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
        assert_eq!(corps["enabled"], true);
        assert_eq!(corps["source_index"], 13);
        assert!(tune_core::outputs::devialet_air::reglages::activee(&db));

        let (statut, _) = appel(
            &app,
            "POST",
            "/api/v1/devialet-air/reglages",
            Some(serde_json::json!({"source_index": 15})),
        )
        .await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "hors bornes refusé");

        let (_, corps) = appel(
            &app,
            "POST",
            "/api/v1/devialet-air/reglages",
            Some(serde_json::json!({"enabled": false})),
        )
        .await;
        assert_eq!(corps["enabled"], false);
        assert_eq!(corps["source_index"], 13, "l'indice n'a pas bougé");
        assert!(!tune_core::outputs::devialet_air::reglages::activee(&db));
    }
}
