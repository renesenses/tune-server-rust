//! « Sauvegarder maintenant » refusé par le site pour sa taille (#7286,
//! #7288) : la route répond 413 avec la taille, la limite et quoi alléger,
//! au lieu d'un 502 `backup_failed` opaque.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::cloud::sauvegarde_config as sc;
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

/// Un faux site qui refuse tout dépôt avec ce corps.
async fn site_qui_refuse(corps: &'static str) -> (String, tokio::task::JoinHandle<()>) {
    use axum::{Router, routing::post};
    let app = Router::new().route(
        "/api/v1/config-backups",
        post(move || async move { (StatusCode::PAYLOAD_TOO_LARGE, corps) }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    let serveur = tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    (format!("http://{adresse}"), serveur)
}

async fn sauvegarder_maintenant(corps_du_site: &'static str) -> (StatusCode, Value) {
    let (base, serveur) = site_qui_refuse(corps_du_site).await;
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état en mémoire");
    etat.license.set_account_premium(true, None).await;
    let reglages = SettingsRepo::with_backend(etat.backend.clone());
    reglages.set("mozaik_base_url", &base).unwrap();
    reglages
        .set("mozaik_access_token", "jeton-de-test")
        .unwrap();
    reglages.set("server_id", "serveur-test").unwrap();
    sc::creer_la_cle(&reglages, "une phrase de passe longue").unwrap();
    etat.backend
        .execute(
            "INSERT INTO playlists (name, description) VALUES ('Du soir', NULL)",
            &[],
        )
        .unwrap();
    etat.backend
        .execute(
            "INSERT INTO playlist_tracks (playlist_id, position, source, source_id, title, artist) \
             VALUES (1, 0, 'qobuz', '42', 'Titre', 'Artiste')",
            &[],
        )
        .unwrap();

    let reponse = crate::routes::router(etat)
        .oneshot(
            Request::post("/api/v1/system/config-backup/cloud/backup-now")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    serveur.abort();
    let statut = reponse.status();
    let octets = to_bytes(reponse.into_body(), usize::MAX).await.unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn un_refus_pour_taille_rend_413_avec_la_limite_et_quoi_faire_7286() {
    let (statut, json) =
        sauvegarder_maintenant(r#"{"error":"payload_too_large","max_bytes":16777216}"#).await;
    assert_eq!(
        statut,
        StatusCode::PAYLOAD_TOO_LARGE,
        "#7286 : un refus de taille ne doit pas devenir un 502 backup_failed : {json}"
    );
    assert_eq!(json["error"], "payload_too_large", "{json}");
    assert!(
        json["payload_bytes"].as_u64().is_some_and(|n| n > 0),
        "{json}"
    );
    assert_eq!(json["max_bytes"], 16_777_216, "{json}");
    assert_eq!(json["largest_playlists"][0]["name"], "Du soir", "{json}");
    assert_eq!(json["largest_playlists"][0]["tracks"], 1, "{json}");
    let detail = json["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("limite : 16777216 octets"), "{detail}");
    assert!(detail.contains("Allégez-les"), "{detail}");
}

/// Un proxy qui refuse sans dire sa limite : la route ne l'invente pas.
#[tokio::test]
async fn un_refus_de_proxy_sans_limite_ne_l_invente_pas_7286() {
    let (statut, json) = sauvegarder_maintenant("<html>413 Request Entity Too Large</html>").await;
    assert_eq!(statut, StatusCode::PAYLOAD_TOO_LARGE, "{json}");
    assert!(json["max_bytes"].is_null(), "{json}");
    let detail = json["detail"].as_str().unwrap_or_default();
    assert!(detail.contains("limite non communiquée"), "{detail}");
}
