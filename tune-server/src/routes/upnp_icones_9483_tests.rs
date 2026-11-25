//! #9483 — chaque `<url>` d'icône des descriptions UPnP (MediaServer et
//! MediaRenderer) répond 200 avec le bon type, sur le VRAI routeur,
//! authentification active et sans jeton.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use tower::ServiceExt;

use crate::state::AppState;
use tune_core::upnp_icones::{icone_au_chemin, urls_des_icones};

const BASE: &str = "http://10.0.0.2:8888";

fn etat_sous_authentification() -> (tempfile::TempDir, AppState) {
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");
    let state = AppState::new(
        base.to_str().unwrap(),
        0,
        crate::config::TuneConfig::default(),
    )
    .unwrap();
    let reglages = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    reglages.set("auth_enabled", "true").unwrap();
    reglages.set("jwt_secret", "secret-de-l-epreuve").unwrap();
    (dossier, state)
}

async fn verifier_les_icones(state: &AppState, description: &str, quoi: &str) {
    let urls = urls_des_icones(description);
    assert!(
        urls.len() >= 2,
        "{quoi} : la description n'annonce pas d'icônes ({urls:?})"
    );
    let mut types = std::collections::BTreeSet::new();
    for url in urls {
        let chemin = url.strip_prefix(BASE).unwrap_or(&url).to_string();
        let reponse = crate::routes::router(state.clone())
            .oneshot(Request::get(&chemin).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(reponse.status(), StatusCode::OK, "{quoi} : {url}");
        let mime = reponse
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let attendue = icone_au_chemin(&chemin).expect("icône connue");
        assert_eq!(mime, attendue.mime, "{quoi} : {url}");
        let corps = to_bytes(reponse.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&corps[..], attendue.octets, "{quoi} : {url}");
        types.insert(mime);
    }
    assert!(types.contains("image/png") && types.contains("image/jpeg"));
}

#[tokio::test]
async fn les_icones_du_lecteur_upnp_sont_servies() {
    let (_d, state) = etat_sous_authentification();
    let xml = tune_core::upnp_renderer::renderer_description_xml("Salon", "uuid:z1", BASE, 3);
    verifier_les_icones(&state, &xml, "MediaRenderer").await;
}

#[tokio::test]
async fn les_icones_du_serveur_de_medias_sont_servies() {
    let (_d, state) = etat_sous_authentification();
    let upnp = tune_core::upnp_server::UpnpState::new(
        state.backend.clone(),
        8888,
        Some("10.0.0.2".into()),
    );
    let xml = tune_core::upnp_server::build_device_description(&upnp);
    verifier_les_icones(&state, &xml, "MediaServer").await;
}

#[tokio::test]
async fn l_ancienne_adresse_icon_png_repond_aussi() {
    let (_d, state) = etat_sous_authentification();
    let reponse = crate::routes::router(state)
        .oneshot(Request::get("/icon.png").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(reponse.status(), StatusCode::OK);
    assert_eq!(
        reponse.headers().get(header::CONTENT_TYPE).unwrap(),
        "image/png"
    );
}
