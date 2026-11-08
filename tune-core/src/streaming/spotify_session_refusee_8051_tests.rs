//! #8051 — Spotify restait « Connecté » avec un jeton que Spotify refusait.
//!
//! Doublure locale de `/api/token` : elle répond par le statut et le corps
//! donnés, comme Spotify à un renouvellement. Aucune requête ne part vers
//! Spotify.

use super::*;
use serde_json::json;

async fn service_avec_jeton_refuse(statut: u16, corps: serde_json::Value) -> SpotifyService {
    let app = axum::Router::new().route(
        "/api/token",
        axum::routing::post(move || {
            let corps = corps.clone();
            async move {
                (
                    axum::http::StatusCode::from_u16(statut).unwrap(),
                    axum::Json(corps),
                )
            }
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
    let mut svc =
        SpotifyService::with_config(Some("client-essai"), None, 8888).avec_doublure(&base);
    svc.access_token = Some("jeton-mort".into());
    svc.refresh_token = Some("renouvellement-revoque".into());
    // Une recherche a reçu un 401 : le prochain tic doit renouveler.
    svc.note_token_rejected();
    svc
}

#[tokio::test]
async fn renouvellement_refuse_met_fin_a_la_session_8051() {
    let mut svc = service_avec_jeton_refuse(
        400,
        json!({"error": "invalid_grant", "error_description": "Refresh token revoked"}),
    )
    .await;
    assert!(svc.auth_status().await.authenticated);

    // `Ok(true)` : le rafraîchisseur de `background.rs` consulte alors
    // `session_expired` et supprime la ligne de jetons.
    let issue = svc.refresh_if_needed().await;
    assert!(matches!(issue, Ok(true)), "{issue:?}");
    assert!(
        svc.session_expired(),
        "#8051 : un renouvellement refusé (invalid_grant) doit mettre fin à la session"
    );
    assert!(
        !svc.auth_status().await.authenticated,
        "#8051 : Spotify ne doit plus être annoncé connecté avec un jeton refusé"
    );
    assert!(svc.save_tokens().is_none());
}

#[tokio::test]
async fn refus_passager_garde_la_session_et_dit_son_motif_8051() {
    let mut svc = service_avec_jeton_refuse(
        503,
        json!({"error": {"status": 503, "message": "Service unavailable"}}),
    )
    .await;
    let erreur = svc
        .refresh_if_needed()
        .await
        .expect_err("un 503 au renouvellement est une erreur")
        .to_string();
    assert!(erreur.contains("503"), "{erreur}");
    assert!(erreur.contains("Service unavailable"), "{erreur}");
    assert!(!svc.session_expired());
    assert!(svc.auth_status().await.authenticated);
}

#[tokio::test]
async fn une_nouvelle_liaison_efface_le_refus_8051() {
    let mut svc = service_avec_jeton_refuse(401, json!({"error": "invalid_client"})).await;
    let _ = svc.refresh_if_needed().await;
    assert!(svc.session_expired());
    assert!(svc.restore_tokens(&json!({"access_token": "neuf", "refresh_token": "neuf"})));
    assert!(!svc.session_expired());
    assert!(svc.auth_status().await.authenticated);
}

#[test]
fn le_refus_definitif_se_distingue_du_passager_8051() {
    assert!(refus_de_renouvellement_definitif(400, "invalid_grant"));
    assert!(refus_de_renouvellement_definitif(401, ""));
    assert!(!refus_de_renouvellement_definitif(429, ""));
    assert!(!refus_de_renouvellement_definitif(503, ""));
}
