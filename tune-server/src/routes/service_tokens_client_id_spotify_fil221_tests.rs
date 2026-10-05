//! Fil 221 (Yan Tasset) — le Client ID Spotify se pose depuis l'écran.
//!
//! Il ne se lisait qu'au démarrage (`TUNE_SPOTIFY_CLIENT_ID`, sinon
//! `tune.toml`) et aucun écran ne permettait de le saisir : le serveur
//! tournait avec `"placeholder"`, l'URL d'autorisation portait
//! `client_id=placeholder` et Spotify répondait `invalid_client`.
//!
//! Ces témoins attaquent les ROUTES MONTÉES par `crate::routes::router`,
//! devant le vrai `SpotifyService` (aucun réseau : sans code, `authenticate`
//! ne fait que fabriquer l'URL d'autorisation). Le Client ID est factice.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

const CLIENT_ID: &str = "0123456789abcdef0123456789abcdef";

async fn app() -> (axum::Router, crate::state::AppState) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    // Point de départ connu, quel que soit l'environnement de la machine.
    remettre_placeholder(&state).await;
    (crate::routes::router(state.clone()), state)
}

async fn remettre_placeholder(state: &crate::state::AppState) {
    let svc = state.services.lock().await.get("spotify").unwrap();
    let mut garde = svc.write().await;
    garde
        .as_any_mut()
        .downcast_mut::<tune_core::streaming::spotify::SpotifyService>()
        .unwrap()
        .set_client_id("placeholder");
}

async fn appel(app: &axum::Router, methode: &str, chemin: &str, corps: Option<Value>) -> Value {
    let req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("content-type", "application/json")
        .body(match corps {
            Some(c) => Body::from(c.to_string()),
            None => Body::empty(),
        })
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let statut = resp.status();
    let brut = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&brut).to_string();
    assert_eq!(statut, StatusCode::OK, "{methode} {chemin} : {texte}");
    serde_json::from_str(&texte).unwrap()
}

async fn url_d_autorisation(state: &crate::state::AppState) -> Result<String, String> {
    let svc = state.services.lock().await.get("spotify").unwrap();
    let st = svc
        .write()
        .await
        .authenticate(&json!({}))
        .await
        .map_err(|e| e.to_string())?;
    Ok(st.verification_url.unwrap_or_default())
}

#[tokio::test]
async fn le_client_id_saisi_s_applique_a_chaud_et_se_lit_dans_system_env() {
    let (app, state) = app().await;

    let env = appel(&app, "GET", "/api/v1/system/env", None).await;
    assert_eq!(env["spotify_client_id_configure"], false, "{env}");
    let err = url_d_autorisation(&state).await.expect_err("placeholder");
    assert!(
        err.starts_with(tune_core::streaming::spotify::CLIENT_ID_ABSENT),
        "{err}"
    );

    let rep = appel(
        &app,
        "POST",
        "/api/v1/services/tokens/spotify",
        Some(json!({ "client_id": format!("  {CLIENT_ID} ") })),
    )
    .await;
    assert_eq!(rep["valid"], true, "{rep}");
    assert_eq!(rep["etat"], "enregistre", "{rep}");
    assert!(
        !rep.to_string().contains(CLIENT_ID),
        "la réponse ne renvoie pas la valeur : {rep}"
    );

    // À chaud : sans redémarrage, l'URL d'autorisation porte le Client ID.
    let url = url_d_autorisation(&state)
        .await
        .expect("URL d'autorisation");
    assert!(url.contains(&format!("client_id={CLIENT_ID}")), "{url}");

    let env = appel(&app, "GET", "/api/v1/system/env", None).await;
    assert_eq!(env["spotify_client_id_configure"], true, "{env}");
    let liste = appel(&app, "GET", "/api/v1/services/tokens", None).await;
    let carte = liste
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "spotify")
        .unwrap();
    assert_eq!(carte["spotify_client_id_configure"], true, "{carte}");

    // Persisté là où le démarrage le relit.
    let settings = SettingsRepo::with_backend(state.backend.clone());
    assert_eq!(
        settings.get("spotify_client_id").unwrap().as_deref(),
        Some(CLIENT_ID)
    );
}

#[tokio::test]
async fn un_client_id_vide_ou_qui_n_en_est_pas_un_est_refuse() {
    let (app, state) = app().await;
    for mauvais in [
        "",
        "placeholder",
        "http://127.0.0.1:8888/api/v1/streaming/spotify/callback?code=abc",
        "abc def",
    ] {
        let rep = appel(
            &app,
            "POST",
            "/api/v1/services/tokens/spotify",
            Some(json!({ "client_id": mauvais })),
        )
        .await;
        assert_eq!(rep["valid"], false, "{mauvais:?} : {rep}");
        assert_eq!(rep["etat"], "refuse", "{mauvais:?} : {rep}");
    }
    let settings = SettingsRepo::with_backend(state.backend.clone());
    assert!(settings.get("spotify_client_id").unwrap().is_none());
    let env = appel(&app, "GET", "/api/v1/system/env", None).await;
    assert_eq!(env["spotify_client_id_configure"], false, "{env}");
}

/// L'ordre documenté : `TUNE_SPOTIFY_CLIENT_ID` l'emporte sur le réglage. On
/// le dit au lieu d'enregistrer une valeur que le démarrage ignorerait.
#[tokio::test]
async fn la_variable_d_environnement_l_emporte_et_on_le_dit() {
    let (_app, state) = app().await;
    let rep = super::enregistrer_client_id_spotify(
        &state,
        "fr",
        &json!({ "client_id": CLIENT_ID }),
        Some("fedcba9876543210fedcba9876543210"),
    )
    .await;
    assert_eq!(rep["valid"], false, "{rep}");
    assert_eq!(rep["etat"], "impose", "{rep}");
    assert!(
        rep["validation_message"]
            .as_str()
            .unwrap_or_default()
            .contains("TUNE_SPOTIFY_CLIENT_ID"),
        "{rep}"
    );
    let settings = SettingsRepo::with_backend(state.backend.clone());
    assert!(settings.get("spotify_client_id").unwrap().is_none());
}

/// « Supprimer » efface le réglage et rend, à chaud, l'état sans lui.
#[tokio::test]
async fn supprimer_revient_a_l_etat_sans_reglage() {
    let (app, state) = app().await;
    appel(
        &app,
        "POST",
        "/api/v1/services/tokens/spotify",
        Some(json!({ "client_id": CLIENT_ID })),
    )
    .await;
    let resp = app
        .clone()
        .oneshot(
            Request::delete("/api/v1/services/tokens/spotify")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    assert!(settings.get("spotify_client_id").unwrap().is_none());
    if std::env::var("TUNE_SPOTIFY_CLIENT_ID").is_err() {
        let env = appel(&app, "GET", "/api/v1/system/env", None).await;
        assert_eq!(env["spotify_client_id_configure"], false, "{env}");
    }
}

/// Le réglage survit au redémarrage : `AppState::new` le relit et construit
/// le service Spotify avec lui (il l'emporte sur `tune.toml`).
#[tokio::test]
async fn le_reglage_est_relu_au_demarrage_avant_tune_toml() {
    if std::env::var("TUNE_SPOTIFY_CLIENT_ID").is_ok() {
        return; // la variable l'emporte par construction
    }
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("tune.db");
    let chemin = chemin.to_str().unwrap();
    {
        let state = crate::state::AppState::new(chemin, 0, Default::default()).unwrap();
        SettingsRepo::with_backend(state.backend.clone())
            .set("spotify_client_id", CLIENT_ID)
            .unwrap();
    }
    let config = crate::config::TuneConfig {
        spotify_client_id: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()),
        ..Default::default()
    };
    let state = crate::state::AppState::new(chemin, 0, config).unwrap();
    let url = url_d_autorisation(&state)
        .await
        .expect("URL d'autorisation");
    assert!(
        url.contains(&format!("client_id={CLIENT_ID}")),
        "le réglage en base doit l'emporter sur tune.toml : {url}"
    );
}
