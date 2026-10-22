//! #6171 — les canaux de la carte qu'une zone ASIO utilise :
//! `/output-channels/zones/{id}`.
//!
//! - `GET` : le réglage (`output_channels`, numéros à partir de 1, ou `null`).
//! - `PUT {"output_channels": [3, 4]}` : le valide (entiers de 1 à 512, sans
//!   doublon) et l'enregistre dans `zone_{id}_output_channels`. `[]` ou `null`
//!   valent un effacement.
//! - `DELETE` : l'efface — la zone revient aux premiers canaux.
//!
//! Le canal `i` de la piste sort sur le canal `output_channels[i]` de la
//! carte. Seul le mode ASIO exclusif l'applique, à l'ouverture du pilote :
//! un changement se fait entendre à la piste suivante ou en relançant la
//! lecture. Le détail est dans `tune_core::outputs::canaux_de_carte_6171`.
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::outputs::canaux_de_carte_6171::{self as cc, CanauxDeCarte};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/zones/{id}",
        get(lire_zone).put(ecrire_zone).delete(effacer_zone),
    )
}

fn forme(id: i64, canaux: Option<&CanauxDeCarte>) -> Value {
    json!({
        "zone_id": id,
        "output_channels": canaux.map(|c| c.numeros()),
        "card_channels_opened": canaux.map(|c| c.largeur()),
        "applies_to": "asio_exclusive",
        "applies_on": "next_open",
        "limits": { "min": 1, "max": cc::NUMERO_MAX },
    })
}

fn refus(code: &str, detail: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": detail, "code": code })),
    )
        .into_response()
}

async fn lire_zone(State(state): State<AppState>, Path(id): Path<i64>) -> Json<Value> {
    Json(forme(id, CanauxDeCarte::lire(&state.backend, id).as_ref()))
}

async fn ecrire_zone(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(corps): Json<Value>,
) -> Response {
    let Some(valeur) = corps.get("output_channels") else {
        return refus(
            "output_channels_missing",
            "le champ output_channels est attendu".into(),
        );
    };
    let canaux = match CanauxDeCarte::depuis_json(valeur) {
        Ok(c) => c,
        Err(e) => return refus("output_channels_invalid", e.to_string()),
    };
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let cle = cc::cle_zone(id);
    let ecrit = match &canaux {
        Some(c) => settings.set(&cle, &json!(c.numeros()).to_string()),
        None => settings.delete(&cle),
    };
    if let Err(e) = ecrit {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e })),
        )
            .into_response();
    }
    Json(forme(id, canaux.as_ref())).into_response()
}

async fn effacer_zone(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let _ = SettingsRepo::with_backend(state.backend.clone()).delete(&cc::cle_zone(id));
    Json(forme(id, None)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn appeler(app: &Router, methode: &str, corps: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method(methode)
            .uri("/zones/7")
            .header("content-type", "application/json");
        let corps = match corps {
            Some(c) => Body::from(c.to_string()),
            None => {
                req = req.header("content-length", "0");
                Body::empty()
            }
        };
        let rep = app.clone().oneshot(req.body(corps).unwrap()).await.unwrap();
        let statut = rep.status();
        let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            statut,
            serde_json::from_slice(&octets).unwrap_or(Value::Null),
        )
    }

    /// Le réglage s'écrit, se relit par la route ET par le cœur (ce que le
    /// bras ASIO lira), se refuse mal formé et s'efface.
    #[tokio::test]
    async fn le_reglage_s_ecrit_se_relit_et_s_efface() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let app = router().with_state(state.clone());

        let (s, v) = appeler(&app, "GET", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["output_channels"], Value::Null, "{v}");

        let (s, v) = appeler(&app, "PUT", Some(json!({ "output_channels": [3, 4] }))).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["output_channels"], json!([3, 4]));
        assert_eq!(v["card_channels_opened"], json!(4));
        assert_eq!(
            CanauxDeCarte::lire(&state.backend, 7).map(|c| c.numeros()),
            Some(vec![3, 4])
        );
        let (_, v) = appeler(&app, "GET", None).await;
        assert_eq!(v["output_channels"], json!([3, 4]));

        for mauvais in [
            json!({}),
            json!({ "output_channels": "3-4" }),
            json!({ "output_channels": [0] }),
            json!({ "output_channels": [3, 3] }),
        ] {
            let (s, _) = appeler(&app, "PUT", Some(mauvais.clone())).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{mauvais}");
        }
        assert!(
            CanauxDeCarte::lire(&state.backend, 7).is_some(),
            "un refus ne touche à rien"
        );

        let (s, v) = appeler(&app, "PUT", Some(json!({ "output_channels": [] }))).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["output_channels"], Value::Null);
        assert!(CanauxDeCarte::lire(&state.backend, 7).is_none());

        appeler(&app, "PUT", Some(json!({ "output_channels": [17, 18] }))).await;
        let (s, _) = appeler(&app, "DELETE", None).await;
        assert_eq!(s, StatusCode::OK);
        assert!(CanauxDeCarte::lire(&state.backend, 7).is_none());
    }
}
