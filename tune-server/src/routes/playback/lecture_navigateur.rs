//! `POST /zones/{id}/browser-playing` — le LECTEUR d'une zone navigateur dit
//! qu'il joue (#6066).
//!
//! Écoute à distance sur iPhone (1.0.0-rc3, zone « Ce téléphone ») : le
//! serveur annonçait « en écoute » et écrivait l'historique dès qu'un octet
//! du flux partait — une sonde de plage AVPlayer (`bytes=0-1`) suffisait, et
//! rien ne jouait. La preuve vient désormais du client qui joue la zone : son
//! lecteur appelle cette route quand il joue réellement (évènement `playing`
//! du `<audio>`, `timeControlStatus == .playing` d'AVPlayer), avec le flux
//! qu'il joue et la position de son horloge de lecture.
//!
//! Corps : `{"stream_id": "…", "position_ms": 1234}`. `stream_id` est celui
//! de `stream_url` (`/stream/<stream_id>.<ext>`).
//!
//! Réponses : 200 `{"status": "confirmed" | "nothing_pending" |
//! "not_playing_yet"}` ; 404 zone inconnue ; 409 `not_a_browser_zone` pour
//! une zone qui a un appareil de sortie (le sondeur y relève l'état réel).
//!
//! Une fois qu'un lecteur a parlé pour une zone, des octets tirés ne
//! confirment plus rien à sa place (voir
//! `tune_core::orchestrator::confirmation_navigateur_6066`).

use super::*;
use tune_core::db::zone_repo::ZoneRepo;

#[derive(Debug, Deserialize)]
pub(super) struct LectureNavigateurRequest {
    stream_id: String,
    #[serde(default)]
    position_ms: i64,
}

pub(super) async fn signaler_lecture_navigateur(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(corps): Json<LectureNavigateurRequest>,
) -> axum::response::Response {
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten();
    let Some(zone) = zone else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "zone_not_found" })),
        )
            .into_response();
    };
    if zone.output_type.as_deref() != Some("browser") {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "not_a_browser_zone" })),
        )
            .into_response();
    }
    let signal = state
        .orchestrator
        .signaler_lecture_navigateur(zone_id, &corps.stream_id, corps.position_ms)
        .await;
    info!(
        zone_id,
        stream_id = %corps.stream_id,
        position_ms = corps.position_ms,
        signal = signal.code(),
        "browser_player_signal"
    );
    Json(json!({ "status": signal.code() })).into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use serde_json::Value;
    use tower::ServiceExt;

    async fn appeler(app: &axum::Router, zone_id: i64, corps: &str) -> (StatusCode, Value) {
        let requete = Request::builder()
            .method("POST")
            .uri(format!("/api/v1/zones/{zone_id}/browser-playing"))
            .header("content-type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap();
        let reponse = app.clone().oneshot(requete).await.unwrap();
        let statut = reponse.status();
        let octets = axum::body::to_bytes(reponse.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            statut,
            serde_json::from_slice(&octets).unwrap_or(Value::Null),
        )
    }

    fn zone(state: &crate::state::AppState, id: i64, type_de_sortie: &str) {
        state
            .backend
            .execute(
                "INSERT INTO zones (id,name,output_type) VALUES (?1,?2,?3)",
                &[&id, &"Ce téléphone", &type_de_sortie],
            )
            .unwrap();
    }

    /// La route est branchée et parle à l'orchestrateur : une zone navigateur
    /// sans annonce en attente répond `nothing_pending`, pas 404.
    #[tokio::test]
    async fn la_route_repond_pour_une_zone_navigateur_6066() {
        let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        zone(&state, 6_066, "browser");
        let app = crate::routes::router(state.clone());
        let (statut, v) = appeler(&app, 6_066, r#"{"stream_id":"s1","position_ms":1500}"#).await;
        assert_eq!(statut, StatusCode::OK, "POST /browser-playing : {v}");
        assert_eq!(v["status"], "nothing_pending", "{v}");
    }

    /// Une zone à appareil n'a pas à se faire dire qu'elle joue : le sondeur y
    /// relève l'état réel de la sortie.
    #[tokio::test]
    async fn une_zone_a_appareil_est_refusee_6066() {
        let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        zone(&state, 6_067, "dlna");
        let app = crate::routes::router(state.clone());
        let (statut, v) = appeler(&app, 6_067, r#"{"stream_id":"s1","position_ms":1500}"#).await;
        assert_eq!(statut, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["error"], "not_a_browser_zone");

        let (statut, _) = appeler(&app, 999_999, r#"{"stream_id":"s1","position_ms":1}"#).await;
        assert_eq!(statut, StatusCode::NOT_FOUND);
    }
}
