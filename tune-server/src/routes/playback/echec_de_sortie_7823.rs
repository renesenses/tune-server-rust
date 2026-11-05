//! Une pause ou une reprise refusée par l'appareil dit quelle zone et pourquoi
//! (#7823).
//!
//! `output_command_error_response` rend `{"error":"output_command_failed",
//! "message":<texte brut de la sortie>}`, sans `message_key` : le client web
//! en conclut que la phrase n'est pas traduite et affiche « Le serveur n'a pas
//! pu traiter la demande. », le texte brut replié dans les détails. Devant un
//! récepteur AirPlay 2 muet, c'était tout ce que voyait le testeur.
//!
//! Pour pause et reprise, le refus suit maintenant le contrat de #7077
//! (`crate::message_api`) : une phrase traduite qui nomme la zone et la cause,
//! sa clé, les paramètres, et la cause brute dans `cause`. Les champs `error`
//! et `command` restent ceux d'avant.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tune_core::outputs::OutputCommandError;

use crate::state::AppState;

/// Le refus d'une commande de sortie sur la zone `zone_id`.
pub(super) fn reponse_d_echec_de_zone(
    state: &AppState,
    zone_id: i64,
    error: OutputCommandError,
) -> Response {
    let OutputCommandError::Failed { command, message } = error else {
        return super::output_command_error_response(error);
    };
    let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten()
        .map(|z| z.name)
        .unwrap_or_else(|| format!("#{zone_id}"));
    let mut corps = crate::message_api::corps_refus(
        "output_command_failed",
        "api.playback.zoneOutputCommandFailed",
        &[
            ("zone", zone),
            ("command", command.to_string()),
            ("cause", message.clone()),
        ],
    );
    corps["command"] = json!(command);
    corps["cause"] = json!(message);
    (StatusCode::BAD_GATEWAY, Json(corps)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use serde_json::Value;
    use tower::ServiceExt;
    use tune_core::db::zone_repo::ZoneRepo;
    use tune_core::outputs::{OutputCapabilities, OutputStatus, OutputTarget};
    use tune_core::playback::NowPlaying;

    const CAUSE: &str = "AirPlay 2 receiver \"ShairportSync\" is not responding: \
                         setup failed: Operation timed out. Start playback again to reconnect it.";

    /// Un récepteur dont la session est tombée : il refuse pause et reprise
    /// comme le fait la sortie AirPlay 2 après un setup échoué.
    struct RecepteurMuet;

    #[async_trait::async_trait]
    impl OutputTarget for RecepteurMuet {
        fn name(&self) -> &str {
            "ShairportSync"
        }
        fn device_id(&self) -> &str {
            "airplay2-muet-7823"
        }
        fn output_type(&self) -> &str {
            "airplay2"
        }
        fn capabilities(&self) -> OutputCapabilities {
            OutputCapabilities::v1(true, true, false, true, true, false)
        }
        async fn pause(&self) -> Result<(), String> {
            Err(CAUSE.into())
        }
        async fn resume(&self) -> Result<(), String> {
            Err(CAUSE.into())
        }
        async fn stop(&self) -> Result<(), String> {
            Ok(())
        }
        async fn seek(&self, _position_ms: u64) -> Result<(), String> {
            Err("unsupported".into())
        }
        async fn set_volume(&self, _volume: f64) -> Result<(), String> {
            Ok(())
        }
        async fn set_mute(&self, _muted: bool) -> Result<(), String> {
            Ok(())
        }
        async fn get_status(&self) -> Result<OutputStatus, String> {
            Ok(OutputStatus::default())
        }
        async fn is_available(&self) -> bool {
            true
        }
    }

    async fn zone_sur_recepteur_muet() -> (AppState, i64) {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let zid = ZoneRepo::with_backend(state.backend.clone())
            .create(
                "Salon AirPlay",
                Some("airplay2"),
                Some("airplay2-muet-7823"),
            )
            .unwrap();
        state.outputs.lock().await.register(Box::new(RecepteurMuet));
        state
            .playback
            .play(
                zid,
                NowPlaying {
                    title: "Fixture".into(),
                    source: "local".into(),
                    duration_ms: 300_000,
                    ..Default::default()
                },
            )
            .await;
        (state, zid)
    }

    async fn appel(state: &AppState, uri: &str, langue: &str) -> (StatusCode, Value) {
        let reponse = crate::routes::router(state.clone())
            .oneshot(
                Request::post(uri)
                    .header("accept-language", langue)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let statut = reponse.status();
        let octets = to_bytes(reponse.into_body(), 1024 * 1024).await.unwrap();
        (statut, serde_json::from_slice(&octets).unwrap())
    }

    #[tokio::test]
    async fn une_pause_refusee_nomme_la_zone_et_la_cause_7823() {
        let (state, zid) = zone_sur_recepteur_muet().await;
        let (statut, corps) = appel(&state, &format!("/api/v1/zones/{zid}/pause"), "fr").await;

        assert_eq!(statut, StatusCode::BAD_GATEWAY, "{corps}");
        assert_eq!(corps["error"], "output_command_failed");
        assert_eq!(corps["command"], "pause");
        assert_eq!(
            corps["message_key"], "api.playback.zoneOutputCommandFailed",
            "sans message_key, l'interface affiche « Le serveur n'a pas pu traiter la demande » : {corps}"
        );
        let message = corps["message"].as_str().unwrap();
        assert!(
            message.contains("Salon AirPlay"),
            "la phrase doit nommer la zone : {message}"
        );
        assert!(
            message.contains("setup failed: Operation timed out"),
            "la phrase doit dire la cause : {message}"
        );
        assert_eq!(corps["cause"], CAUSE);
        assert_eq!(corps["params"]["zone"], "Salon AirPlay");
        assert_eq!(
            message,
            crate::message_api::texte_en(
                "fr",
                "api.playback.zoneOutputCommandFailed",
                &[
                    ("zone", "Salon AirPlay".into()),
                    ("command", "pause".into()),
                    ("cause", CAUSE.into()),
                ],
            ),
            "la phrase doit être dans la langue de l'interface"
        );
    }

    #[tokio::test]
    async fn une_reprise_refusee_nomme_la_zone_et_la_cause_7823() {
        let (state, zid) = zone_sur_recepteur_muet().await;
        state.playback.pause(zid).await;
        let (statut, corps) = appel(&state, &format!("/api/v1/zones/{zid}/resume"), "en").await;

        assert_eq!(statut, StatusCode::BAD_GATEWAY, "{corps}");
        assert_eq!(corps["command"], "resume");
        assert_eq!(corps["message_key"], "api.playback.zoneOutputCommandFailed");
        let message = corps["message"].as_str().unwrap();
        assert!(message.contains("Salon AirPlay"), "{message}");
        assert!(message.contains("setup failed"), "{message}");
    }
}
