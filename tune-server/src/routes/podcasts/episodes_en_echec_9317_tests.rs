//! #9317 — `GET /podcasts/episodes/{id}` quand le flux de l'éditeur échoue.
//!
//! Forum (relevé du 21/11) : la liste des épisodes affichait « Aucun
//! épisode » alors que le flux RSS répondait 404. La route répondait 200
//! `{"podcast_id", "error"}` ; le client web (`getPodcastEpisodes`) fait
//! `data.episodes || data` et avalait l'erreur.
//!
//! Le banc : un faux éditeur local qui répond 404 à toute demande, joint par
//! les deux chemins de la route (adresse du flux en paramètre, et abonnement
//! enregistré).
//!
//! Contre-épreuve : remettre `Ok(Json(json!({"podcast_id": …, "error": e})))`
//! dans l'une des deux branches d'erreur de `podcast_episodes` fait tomber le
//! témoin correspondant (statut 200 au lieu de 502).

use super::CODE_FLUX_EN_ECHEC;
use crate::state::AppState;
use axum::{body::Body, http::Request, http::StatusCode};
use serde_json::Value;
use tower::ServiceExt;

/// Un faux éditeur qui répond `404 Not Found` à chaque requête.
async fn faux_editeur_en_404() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move {
        while let Ok((mut flux, _)) = ecoute.accept().await {
            let mut tampon = [0u8; 4096];
            let _ = flux.read(&mut tampon).await;
            let _ = flux
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\ncontent-type: text/html\r\ncontent-length: 9\r\nconnection: close\r\n\r\nnot found",
                )
                .await;
        }
    });
    base
}

async fn get(state: &AppState, requete: &str) -> (StatusCode, Value) {
    let rep = crate::routes::router(state.clone())
        .oneshot(Request::get(requete).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&octets)
        .unwrap_or_else(|e| panic!("{requete} : corps non JSON ({e})"));
    (statut, corps)
}

fn verifier_l_echec(statut: StatusCode, corps: &Value) {
    assert_eq!(
        statut,
        StatusCode::BAD_GATEWAY,
        "#9317 : un flux en 404 chez l'éditeur ne doit pas répondre 200 : {corps}"
    );
    assert_eq!(corps["code"], CODE_FLUX_EN_ECHEC, "{corps}");
    let message = corps["error"].as_str().unwrap_or_default();
    assert!(
        message.contains("404"),
        "#9317 : le message habituel (`error`) dit le statut de l'éditeur : {corps}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_flux_en_404_par_son_adresse_repond_502_9317() {
    let base = faux_editeur_en_404().await;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let flux = format!("{base}/flux-9317.xml");
    let (statut, corps) = get(
        &state,
        &format!(
            "/api/v1/podcasts/episodes/emission-9317?feed_url={}",
            urlencoding::encode(&flux)
        ),
    )
    .await;
    verifier_l_echec(statut, &corps);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_abonnement_dont_le_flux_est_en_404_repond_502_9317() {
    use tune_core::db::backend::ToSqlValue;
    let base = faux_editeur_en_404().await;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let flux = format!("{base}/abonnement-9317.xml");
    let id = state
        .backend
        .execute_returning_id(
            "INSERT INTO podcast_subscriptions (feed_url, title) VALUES (?, ?)",
            &[
                &flux as &dyn ToSqlValue,
                &"Émission 9317" as &dyn ToSqlValue,
            ],
        )
        .expect("abonnement enregistré");
    let (statut, corps) = get(&state, &format!("/api/v1/podcasts/episodes/{id}")).await;
    verifier_l_echec(statut, &corps);
}
