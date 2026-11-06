//! #7965 — bout en bout par les routes : un épisode lancé sur une zone à
//! périphérique, puis une radio de « Mes radios », apparaissent dans
//! `GET /library/history` ; la radio une seule fois, au nom de la station.
//! Le tableau de bord des statistiques n'en compte que l'épisode.
//!
//! Aucun son : sortie factice (`MockOutput`), épisode et station sur le port
//! 9 local (discard).
use crate::state::AppState;
use axum::{Router, body::Body, http::Request};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::radio_repo::{RadioRepo, RadioStation};
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;

const APPAREIL: &str = "dlna:uuid-historique-7965";
const EPISODE: &str = "http://127.0.0.1:9/episode-7965.mp3";
const FLUX: &str = "http://127.0.0.1:9/fip-7965.mp3";

async fn appel(app: &Router, path: &str, body: Option<Value>) -> Value {
    let req = Request::builder()
        .uri(format!("/api/v1{path}"))
        .method(if body.is_some() { "POST" } else { "GET" })
        .header("content-type", "application/json")
        .body(body.map_or(Body::empty(), |v| Body::from(v.to_string())))
        .unwrap();
    let response =
        tokio::time::timeout(std::time::Duration::from_secs(30), app.clone().oneshot(req))
            .await
            .expect("la route ne doit pas rester bloquée")
            .unwrap();
    let statut = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(statut.is_success(), "{path}: {statut} {v}");
    v
}

#[tokio::test]
async fn l_historique_liste_l_episode_et_la_station_7965() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("Salon — témoin 7965", Some("dlna"), Some(APPAREIL))
        .unwrap();
    state.outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "Salon").with_type("dlna"),
    ));
    let station = RadioRepo::with_backend(state.backend.clone())
        .create(&RadioStation {
            id: None,
            name: "FIP".into(),
            url: FLUX.into(),
            homepage: None,
            logo_url: Some("https://example.org/logos/fip.png".into()),
            country: None,
            language: None,
            genre: None,
            codec: None,
            bitrate: None,
            is_favorite: false,
            last_played: None,
            play_count: 0,
        })
        .unwrap();
    let app = crate::routes::router(state.clone());

    // 1. Un épisode, sur la zone à périphérique : le chemin qui n'écrivait rien.
    let r = appel(
        &app,
        &format!("/podcasts/play/{zone}"),
        Some(json!({
            "audio_url": EPISODE,
            "title": "Épisode 42",
            "podcast_name": "Les Pieds sur terre",
            "podcast_id": "17",
            "cover_url": "https://example.org/pochette.jpg",
            "duration_ms": 1_800_000,
        })),
    )
    .await;
    assert_eq!(r["output_sent"], true, "{r}");
    // Le tour du sondeur au seuil min(30 s, 50 %) (#7852).
    state.playback.update_position(zone, 30_000).await;
    assert!(
        state
            .orchestrator
            .valider_l_ecoute(&state.playback.get_state(zone).await),
        "l'épisode joué doit avoir armé son écoute"
    );

    // 2. Une station de « Mes radios ».
    let r = appel(
        &app,
        &format!("/radios/{station}/play/{zone}"),
        Some(json!({})),
    )
    .await;
    assert_eq!(r["output_sent"], true, "{r}");

    let h = appel(&app, "/library/history?limit=50", None).await;
    let items = h["items"].as_array().expect("items");
    let podcasts: Vec<_> = items.iter().filter(|i| i["source"] == "podcast").collect();
    let radios: Vec<_> = items.iter().filter(|i| i["source"] == "radio").collect();
    assert_eq!(podcasts.len(), 1, "l'épisode est dans l'historique : {h}");
    assert_eq!(podcasts[0]["title"], "Épisode 42");
    assert_eq!(podcasts[0]["artist_name"], "Les Pieds sur terre");
    assert_eq!(podcasts[0]["context_type"], "podcast");
    assert!(
        podcasts[0]["cover_url"].is_string(),
        "la pochette de l'épisode : {h}"
    );
    assert_eq!(radios.len(), 1, "la station, une fois : {h}");
    assert_eq!(radios[0]["title"], "FIP");
    assert_eq!(radios[0]["cover_url"], "https://example.org/logos/fip.png");
    assert_eq!(h["total"], 2, "{h}");

    let mut d = appel(&app, "/library/history/dashboard?period=all", None).await;
    assert_eq!(d["totals"]["plays"], 1, "la radio ne compte pas : {d}");
    // Les sections propres aux radios, tenues à part (`radio_stations`,
    // `radio_ecoute`), la nomment ; aucun classement ni compteur d'écoute.
    let objet = d.as_object_mut().unwrap();
    objet.remove("top_radios");
    objet.remove("radio_listening");
    assert!(!d.to_string().contains("\"FIP\""), "{d}");
}
