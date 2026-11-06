//! #7378 / #7398 — l'aléatoire sur « Ce téléphone » (zone `browser`).
//!
//! Sevy (rc8.1) puis Yves (rc9, 30/10) : en voiture, la file affichée est
//! mélangée mais la lecture suivrait l'ordre de l'album. L'appli iPhone ne
//! décide rien : à la fin d'une piste comme au bouton « Suivant » de CarPlay,
//! elle appelle `POST /zones/{id}/next`, puis relit la zone. Ces tests jouent
//! ce parcours par le routeur complet, sur une zone `browser` dont les pistes
//! sont des WAV muets temporaires : aucune sortie, aucun son.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::time::{Duration, Instant};
use tower::ServiceExt;
use tune_core::db::models::Track;
use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::ZoneRepo;

async fn appeler(app: &axum::Router, methode: &str, uri: &str) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method(methode)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = to_bytes(reponse.into_body(), 1 << 22).await.unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn wav_muet() -> Vec<u8> {
    let mut wav = b"RIFF".to_vec();
    wav.extend_from_slice(&(36u32 + 176_400).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&44_100u32.to_le_bytes());
    wav.extend_from_slice(&176_400u32.to_le_bytes());
    wav.extend_from_slice(&4u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&176_400u32.to_le_bytes());
    wav.resize(44 + 176_400, 0);
    wav
}

/// Une zone `browser` (« Ce téléphone ») dont la file compte `n` pistes dans
/// l'ordre de l'album, la première en lecture.
async fn banc(n: usize) -> (axum::Router, crate::state::AppState, i64, tempfile::TempDir) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .create("iPhone d'Yves", Some("browser"), None)
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut ids = Vec::new();
    for i in 1..=n {
        let path = dir.path().join(format!("piste-{i}.wav"));
        std::fs::write(&path, wav_muet()).unwrap();
        let mut track = Track::new(format!("Piste {i}"));
        track.file_path = Some(path.to_string_lossy().into_owned());
        track.format = Some("wav".into());
        track.duration_ms = 1000;
        track.sample_rate = Some(44_100);
        track.bit_depth = Some(16);
        track.channels = 2;
        ids.push(
            TrackRepo::with_backend(state.backend.clone())
                .create(&track)
                .unwrap(),
        );
    }
    PlayQueueRepo::with_backend(state.backend.clone())
        .set_queue(zone, &ids)
        .unwrap();
    state.orchestrator.play_from_queue(zone, 0).await.unwrap();
    let app = crate::routes::router(state.clone());
    (app, state, zone, dir)
}

async fn file_relue(app: &axum::Router, zone: i64) -> (Vec<String>, i64) {
    let (statut, v) = appeler(app, "GET", &format!("/api/v1/zones/{zone}/queue")).await;
    assert_eq!(statut, StatusCode::OK, "{v}");
    let titres = v["tracks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap_or_default().to_string())
        .collect();
    (titres, v["position"].as_i64().unwrap_or(-1))
}

/// Le titre en cours, tel que `GET /zones/{id}` le rend à l'appli, une fois
/// qu'il n'est plus `avant` (la lecture du suivant part en tâche détachée).
async fn titre_apres(app: &axum::Router, zone: i64, avant: &str) -> String {
    let fin = Instant::now() + Duration::from_secs(5);
    loop {
        let (_, v) = appeler(app, "GET", &format!("/api/v1/zones/{zone}")).await;
        let titre = v["current_track"]["title"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if titre != avant && !titre.is_empty() {
            return titre;
        }
        assert!(
            Instant::now() < fin,
            "le titre n'a pas changé après « Suivant » : {v}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Aléatoire activé pendant la première piste, puis « Suivant » jusqu'au
/// bout : chaque titre joué est celui que la file relue annonce au rang
/// suivant, et l'ensemble n'est pas l'ordre de l'album.
#[tokio::test]
async fn suivant_sur_ce_telephone_suit_la_file_melangee_7378() {
    let n = 12;
    let (app, _state, zone, _dir) = banc(n).await;
    let (statut, v) = appeler(
        &app,
        "POST",
        &format!("/api/v1/zones/{zone}/shuffle?enabled=true"),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{v}");
    assert_eq!(v["queue_reordered"], true, "{v}");

    let (file, position) = file_relue(&app, zone).await;
    assert_eq!(position, 0);
    assert_eq!(file[0], "Piste 1");
    let album: Vec<String> = (1..=n).map(|i| format!("Piste {i}")).collect();
    assert_ne!(file, album, "la file relue n'est pas mélangée");

    let mut joues = vec![file[0].clone()];
    for rang in 1..n {
        let (statut, v) = appeler(&app, "POST", &format!("/api/v1/zones/{zone}/next")).await;
        assert_eq!(statut, StatusCode::OK, "{v}");
        assert_eq!(v["queue_position"], rang as i64, "{v}");
        let titre = titre_apres(&app, zone, joues.last().unwrap()).await;
        assert_eq!(
            titre, file[rang],
            "rang {rang} : le téléphone reçoit {titre}, la file annonçait {} ({file:?})",
            file[rang]
        );
        joues.push(titre);
    }
    assert_eq!(joues, file);
}
