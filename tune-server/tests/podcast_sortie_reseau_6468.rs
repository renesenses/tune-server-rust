//! #6468 — un épisode de podcast joué sur un renderer réseau passe par le
//! mandataire de Tune, et un échec de la sortie est une erreur HTTP.
//!
//! FabienM, fil 2302 point 13 : impossible de lire un podcast sur sa zone
//! DLNA, puis « Pause rejected: SOAP fault (UPnP code 701) — transport du
//! renderer : STOPPED ». `play_episode` envoyait au renderer l'URL HTTPS
//! directe de l'hébergeur, que beaucoup de renderers DLNA n'ouvrent pas ;
//! l'échec revenait dans un `200`, que le client ne lisait pas.
//!
//! Sorties factices seulement (`MockOutput`), adresses sur un port fermé de
//! la boucle locale : rien ne sort, aucun son n'est produit.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;
use tune_server::state::AppState;

const EPISODE: &str = "https://media.podcast.test/le-sept-neuf/episode.mp3";

async fn banc(device_id: &str, output_type: &str) -> (AppState, axum::Router, i64) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon", Some(output_type), Some(device_id))
        .unwrap();
    state.outputs.lock().await.register(Box::new(
        MockOutput::new(device_id, "Sortie de banc").with_type(output_type),
    ));
    let app = tune_server::routes::router(state.clone());
    (state, app, zone)
}

async fn jouer(app: &axum::Router, zone: i64) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/podcasts/play/{zone}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"audio_url": EPISODE, "title": "Le sept neuf"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

async fn avec_la_sortie<R>(
    state: &AppState,
    device_id: &str,
    f: impl AsyncFnOnce(&MockOutput) -> R,
) -> R {
    let arc = state.outputs.lock().await.get(device_id).unwrap();
    let sortie = arc.lock().await;
    let mock = sortie
        .as_any()
        .downcast_ref::<MockOutput>()
        .expect("sortie factice");
    f(mock).await
}

#[tokio::test]
async fn un_episode_sur_une_sortie_dlna_recoit_l_url_du_mandataire_de_tune() {
    let (state, app, zone) = banc("dlna:uuid-salon-6468", "dlna").await;
    let (statut, corps) = jouer(&app, zone).await;
    assert_eq!(statut, StatusCode::OK, "corps = {corps}");
    assert_eq!(corps["output_sent"], true, "corps = {corps}");

    let url = avec_la_sortie(&state, "dlna:uuid-salon-6468", async |m| {
        m.last_play_url().await
    })
    .await
    .expect("la sortie DLNA a reçu une lecture");
    assert_ne!(
        url, EPISODE,
        "le renderer DLNA a reçu l'URL HTTPS directe de l'épisode, sans relais"
    );
    assert!(
        url.starts_with("http://") && url.contains("/stream/"),
        "le renderer doit recevoir une URL du mandataire de Tune en http://, pas {url}"
    );
    // L'URL relayée vise une session du mandataire qui sert bien l'épisode.
    let session = url
        .rsplit('/')
        .next()
        .and_then(|f| f.split('.').next())
        .unwrap()
        .to_string();
    assert!(
        state.streamer.is_seekable_session(&session).await,
        "la session {session} n'existe pas au mandataire"
    );
    assert_eq!(
        state
            .playback
            .get_state(zone)
            .await
            .now_playing
            .map(|np| np.source_id),
        Some(Some(EPISODE.to_string())),
        "la lecture en cours garde l'épisode d'origine comme identité"
    );
}

#[tokio::test]
async fn un_episode_sur_une_sortie_locale_garde_son_url() {
    let (state, app, zone) = banc("local:carte-6468", "local").await;
    let (statut, corps) = jouer(&app, zone).await;
    assert_eq!(statut, StatusCode::OK, "corps = {corps}");
    let url = avec_la_sortie(&state, "local:carte-6468", async |m| {
        m.last_play_url().await
    })
    .await;
    assert_eq!(
        url.as_deref(),
        Some(EPISODE),
        "la sortie locale ouvre l'URL elle-même : elle doit la recevoir inchangée"
    );
}

#[tokio::test]
async fn un_refus_de_la_sortie_est_une_erreur_http_lisible() {
    let (state, app, zone) = banc("dlna:uuid-refus-6468", "dlna").await;
    avec_la_sortie(&state, "dlna:uuid-refus-6468", async |m| {
        m.refuser_la_lecture(Some("SetAVTransportURI rejected"))
    })
    .await;
    let (statut, corps) = jouer(&app, zone).await;
    assert!(
        statut.is_server_error(),
        "un refus de la sortie doit être une erreur HTTP, pas {statut} (corps = {corps})"
    );
    assert_eq!(corps["code"], "podcast_sortie_refuse", "corps = {corps}");
    assert_eq!(corps["output_sent"], false, "corps = {corps}");
    assert!(
        corps["error"]
            .as_str()
            .is_some_and(|e| e.contains("SetAVTransportURI rejected")),
        "l'erreur doit dire le refus, corps = {corps}"
    );
    // La zone ne se croit pas en lecture : le geste suivant ne sera pas une
    // Pause refusée en 701.
    assert_ne!(
        state.playback.get_state(zone).await.state,
        tune_core::playback::PlayState::Playing,
        "la zone se croit en lecture alors que la sortie a refusé"
    );
}
