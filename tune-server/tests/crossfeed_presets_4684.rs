//! #4684 — préréglages nommés du crossfeed, `/api/v1/crossfeed/presets`, et
//! #4683 — les bornes publiées par `GET /zones/{id}/dsp`.
//!
//! Tout passe par `tune_server::routes::router(state)` — le routeur réel, son
//! préfixe `/api/v1` et son repli 404 — pour qu'une route écrite mais jamais
//! montée fasse rougir ce fichier.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! par sa cible `[[test]]` dans `tune-server/Cargo.toml`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

const CHEMIN: &str = "/api/v1/crossfeed/presets";

async fn etat(premium: bool) -> tune_server::state::AppState {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    // Migration passée, greffon installé et actif : l'état d'une installation
    // Premium ordinaire.
    settings
        .set(tune_core::audio::premium_plugins::MIGRATION, "complete")
        .unwrap();
    settings.set("plugin_crossfeed_installed", "true").unwrap();
    settings.set("plugin_crossfeed_enabled", "true").unwrap();
    state.license.set_account_premium(premium, None).await;
    state
}

async fn appel(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn post(app: &axum::Router, body: Value) -> (StatusCode, Value) {
    appel(
        app,
        Request::post(CHEMIN)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

async fn lister(app: &axum::Router) -> Vec<Value> {
    let (status, body) = appel(app, Request::get(CHEMIN).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["presets"].as_array().cloned().unwrap_or_default()
}

async fn supprimer(app: &axum::Router, id: &str) -> (StatusCode, Value) {
    appel(
        app,
        Request::delete(format!("{CHEMIN}/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

#[tokio::test]
async fn enregistrer_lister_supprimer() {
    let app = tune_server::routes::router(etat(true).await);
    assert!(lister(&app).await.is_empty(), "liste vide au départ");

    let (status, cree) = post(
        &app,
        json!({"name": " Salon ", "amount": 0.35, "delay_ms": 0.6}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{cree}");
    assert_eq!(cree["name"], "Salon", "nom rogné de ses espaces");
    assert_eq!(cree["amount"], 0.35);
    assert_eq!(cree["delay_ms"], 0.6);
    assert!(cree.get("enabled").is_none(), "un préréglage n'allume rien");
    let id = cree["id"].as_str().unwrap().to_string();

    let liste = lister(&app).await;
    assert_eq!(liste.len(), 1);
    assert_eq!(liste[0]["id"], id.as_str());

    let (status, _) = post(
        &app,
        json!({"name": "Casque", "amount": 0.2, "delay_ms": 0.3}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(lister(&app).await.len(), 2);

    let (status, body) = supprimer(&app, &id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let liste = lister(&app).await;
    assert_eq!(liste.len(), 1);
    assert_eq!(liste[0]["name"], "Casque");

    let (status, _) = supprimer(&app, &id).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "deuxième suppression");
}

/// Enregistrer deux fois le même nom met à jour, au lieu de laisser deux
/// « Salon » indiscernables.
#[tokio::test]
async fn meme_nom_met_a_jour_sans_doublon() {
    let app = tune_server::routes::router(etat(true).await);
    let (_, premier) = post(
        &app,
        json!({"name": "Salon", "amount": 0.3, "delay_ms": 0.5}),
    )
    .await;
    let (status, second) = post(
        &app,
        json!({"name": "salon", "amount": 0.45, "delay_ms": 0.7}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["id"], premier["id"], "même préréglage");
    let liste = lister(&app).await;
    assert_eq!(liste.len(), 1);
    assert_eq!(liste[0]["amount"], 0.45);
    assert_eq!(liste[0]["delay_ms"], 0.7);
}

/// Les valeurs sont bornées comme celles de la zone : un préréglage ne promet
/// pas ce que `PUT /zones/{id}/dsp` rognerait.
#[tokio::test]
async fn valeurs_bornees_et_nom_requis() {
    let app = tune_server::routes::router(etat(true).await);
    let (status, p) = post(
        &app,
        json!({"name": "Trop", "amount": 0.9, "delay_ms": 12.0}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(p["amount"], tune_core::audio::crossfeed::MAX_AMOUNT);
    assert_eq!(p["delay_ms"], tune_core::audio::crossfeed::MAX_DELAY_MS);

    let (status, _) = post(&app, json!({"name": "   ", "amount": 0.3, "delay_ms": 0.3})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "nom vide refusé");
    let long = "x".repeat(65);
    let (status, _) = post(&app, json!({"name": long, "amount": 0.3, "delay_ms": 0.3})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "nom trop long refusé");
    assert_eq!(lister(&app).await.len(), 1, "rien d'écrit par les refus");
}

/// Écrire demande le crossfeed Premium, comme `PUT /zones/{id}/dsp` ; lire
/// reste libre.
#[tokio::test]
async fn ecrire_demande_le_premium() {
    let app = tune_server::routes::router(etat(false).await);
    let (status, _) = post(
        &app,
        json!({"name": "Salon", "amount": 0.3, "delay_ms": 0.5}),
    )
    .await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED);
    assert!(lister(&app).await.is_empty());
}

/// …et le greffon installé : désinstallé, 409 `plugin_unavailable`.
#[tokio::test]
async fn ecrire_demande_le_greffon() {
    let state = etat(true).await;
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set("plugin_crossfeed_installed", "false")
        .unwrap();
    let app = tune_server::routes::router(state);
    let (status, body) = post(
        &app,
        json!({"name": "Salon", "amount": 0.3, "delay_ms": 0.5}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"], "plugin_unavailable");
}

/// #4683 — `GET /zones/{id}/dsp` publie les bornes que le serveur applique :
/// le bout du curseur d'un client est celui-là, pas une copie à lui.
#[tokio::test]
async fn get_dsp_publie_les_bornes_4683() {
    let app = tune_server::routes::router(etat(true).await);
    let (status, body) = appel(
        &app,
        Request::get("/api/v1/zones/1/dsp")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["crossfeed_limits"]["amount_max"], 0.5);
    assert_eq!(body["crossfeed_limits"]["delay_ms_max"], 5.0);
}
