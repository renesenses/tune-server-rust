//! Le pont utilise uniquement la clé configurée, y compris lors du passage WebSocket.
use axum::http::StatusCode;
use tokio_tungstenite::{connect_async, tungstenite::Error};
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

#[tokio::test]
async fn le_pont_9413_refuse_une_cle_vide_ou_fausse_et_accepte_la_bonne() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let app = axum::Router::new()
        .nest("/ws/bridge", tune_server::routes::bridge::router())
        .with_state(state);
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    let serveur = tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
    for (configuree, fournie) in [("", ""), ("cle-du-banc", ""), ("cle-du-banc", "autre")] {
        settings.set("api_key", configuree).unwrap();
        let url = format!("ws://{adresse}/ws/bridge?api_key={fournie}");
        assert!(
            matches!(connect_async(url).await, Err(Error::Http(r)) if r.status()==StatusCode::UNAUTHORIZED)
        );
    }
    let (_, reponse) = connect_async(format!("ws://{adresse}/ws/bridge?api_key=cle-du-banc"))
        .await
        .unwrap();
    assert_eq!(reponse.status(), StatusCode::SWITCHING_PROTOCOLS);
    serveur.abort();
}
