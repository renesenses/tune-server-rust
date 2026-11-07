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

const POCHETTE: &str = "https://example.org/pochette.jpg";

/// Lance un épisode puis une station de « Mes radios » sur une zone à
/// périphérique ; rend l'état et le routeur.
async fn ecouter_l_episode_et_la_station() -> (AppState, Router) {
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
            "cover_url": POCHETTE,
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
    (state, app)
}

#[tokio::test]
async fn l_historique_liste_l_episode_et_la_station_7965() {
    let (_state, app) = ecouter_l_episode_et_la_station().await;

    let h = appel(&app, "/library/history?limit=50", None).await;
    let items = h["items"].as_array().expect("items");
    let podcasts: Vec<_> = items.iter().filter(|i| i["source"] == "podcast").collect();
    let radios: Vec<_> = items.iter().filter(|i| i["source"] == "radio").collect();
    assert_eq!(podcasts.len(), 1, "l'épisode est dans l'historique : {h}");
    assert_eq!(podcasts[0]["title"], "Épisode 42");
    assert_eq!(podcasts[0]["artist_name"], "Les Pieds sur terre");
    assert_eq!(podcasts[0]["context_type"], "podcast");
    assert_eq!(
        podcasts[0]["cover_url"], POCHETTE,
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

/// Toutes les autres statistiques d'écoute (`/stats/listening`, `/dashboard`,
/// statistiques du profil, résumé hebdomadaire) ne comptent que l'épisode :
/// la ligne de la station n'entre ni dans un total ni dans un classement.
#[tokio::test]
async fn les_statistiques_d_ecoute_ignorent_la_station_7965() {
    let (state, app) = ecouter_l_episode_et_la_station().await;
    state.license.set_account_premium(true, None).await;
    let annee = chrono::Utc::now().format("%Y").to_string();

    let s = appel(&app, "/stats/listening?limit=10", None).await;
    assert_eq!(s["totals"]["total_listens"], 1, "{s}");
    assert_eq!(s["totals"]["unique_tracks"], 1, "{s}");
    let w = appel(
        &app,
        &format!("/stats/listening/wrapped?year={annee}"),
        None,
    )
    .await;
    assert_eq!(w["total_listens"], 1, "{w}");
    assert_eq!(w["unique_tracks"], 1, "{w}");
    assert_eq!(w["busiest_day_plays"], 1, "{w}");
    let carte = appel(&app, "/stats/listening/heatmap", None).await;
    let jours = appel(&app, "/stats/listening/history?period=week", None).await;
    let dw = appel(&app, &format!("/dashboard/wrapped?year={annee}"), None).await;
    assert_eq!(dw["total_listens"], 1, "{dw}");
    assert_eq!(dw["unique_tracks"], 1, "{dw}");
    let dt = appel(&app, "/dashboard/top-tracks", None).await;
    let dh = appel(&app, "/dashboard/listening-history?days=7", None).await;
    let ds = appel(&app, "/dashboard/stats", None).await;
    let p = appel(&app, "/profiles/1/stats", None).await;
    assert_eq!(p["listening"]["total_listens"], 1, "{p}");
    // Le résumé lit `listened_at < maintenant` à la seconde : une heure plus
    // tôt, les deux lignes sont dans sa semaine.
    let il_y_a_une_heure = (chrono::Utc::now() - chrono::Duration::hours(1))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    state
        .backend
        .execute(
            "UPDATE listen_history SET listened_at = ?",
            &[&il_y_a_une_heure as &dyn tune_core::db::backend::ToSqlValue],
        )
        .unwrap();
    let r = tune_core::digest::generate_digest(&state.backend).unwrap();
    assert_eq!(r.total_plays, 1, "{r:?}");

    for (route, v) in [
        ("/stats/listening", s.clone()),
        ("/stats/listening/wrapped", w.clone()),
        ("/dashboard/wrapped", dw),
        ("/dashboard/top-tracks", dt),
        ("/profiles/1/stats", p),
        ("résumé hebdomadaire", serde_json::to_value(&r).unwrap()),
    ] {
        assert!(
            !v.to_string().contains("\"FIP\""),
            "{route} nomme la station : {v}"
        );
    }
    for (route, v) in [
        ("/stats/listening/heatmap", &carte["heatmap"]),
        ("/stats/listening/history", &jours["history"]),
    ] {
        let plays: i64 = v
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["plays"].as_i64().unwrap())
            .sum();
        assert_eq!(plays, 1, "{route} compte la station : {v}");
    }
    let plays_7j: i64 = dh
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["play_count"].as_i64().unwrap())
        .sum();
    assert_eq!(
        plays_7j, 1,
        "/dashboard/listening-history compte la station : {dh}"
    );
    assert_eq!(
        ds["total_listens"], 1,
        "/dashboard/stats compte la station : {ds}"
    );
    assert_eq!(
        ds["unique_tracks"], 1,
        "/dashboard/stats compte la station : {ds}"
    );
}
