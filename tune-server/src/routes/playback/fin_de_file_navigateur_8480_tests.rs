//! #8480 — la lecture automatique « titres similaires » sur une zone
//! NAVIGATEUR (lecteur du client web, « Ce téléphone » des applis).
//!
//! Le lecteur dit la fin de sa piste par `POST /zones/{id}/next`
//! (`browserAudio.ts`, évènement `ended`). Sur la dernière piste de la file,
//! la route arrêtait la zone : la lecture automatique n'était jamais jouée.
//! Ces tests jouent ce parcours par le routeur complet, sur des WAV muets
//! temporaires : aucune sortie, aucun son, aucun réseau.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::models::Track;
use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::db::zone_repo::{AutoplayMode, ZoneRepo};
use tune_core::playback::PlayState;

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

/// Une zone `browser` dont la file ne compte QUE la piste 1, en lecture ; la
/// piste 2, même genre, attend dans la bibliothèque. `duree_ms` est la durée
/// annoncée de la piste 1.
async fn banc(
    mode: AutoplayMode,
    duree_ms: i64,
) -> (axum::Router, crate::state::AppState, i64, tempfile::TempDir) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    // Aucun réseau : l'API d'enrichissement est injoignable.
    SettingsRepo::with_backend(state.backend.clone())
        .set("artist_enrichment_api", "http://127.0.0.1:9")
        .unwrap();
    let zones = ZoneRepo::with_backend(state.backend.clone());
    let zone = zones
        .create("Ce navigateur", Some("browser"), None)
        .unwrap();
    zones.update_autoplay_mode(zone, mode).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut ids = Vec::new();
    for i in 1..=2 {
        let path = dir.path().join(format!("piste-{i}.wav"));
        std::fs::write(&path, wav_muet()).unwrap();
        let mut track = Track::new(format!("Piste {i}"));
        track.file_path = Some(path.to_string_lossy().into_owned());
        track.format = Some("wav".into());
        track.duration_ms = if i == 1 { duree_ms } else { 1000 };
        track.sample_rate = Some(44_100);
        track.bit_depth = Some(16);
        track.channels = 2;
        track.genre = Some("Jazz".into());
        ids.push(
            TrackRepo::with_backend(state.backend.clone())
                .create(&track)
                .unwrap(),
        );
    }
    PlayQueueRepo::with_backend(state.backend.clone())
        .set_queue(zone, &ids[..1])
        .unwrap();
    state.orchestrator.play_from_queue(zone, 0).await.unwrap();
    let app = crate::routes::router(state.clone());
    (app, state, zone, dir)
}

/// La piste 1 (1 s) est finie, le lecteur appelle `next` : la lecture
/// automatique enchaîne la piste 2 au lieu d'arrêter la zone.
#[tokio::test]
async fn fin_de_la_derniere_piste_lance_les_titres_similaires_8480() {
    let (app, state, zone, _dir) = banc(AutoplayMode::Similar, 1000).await;
    let (statut, v) = appeler(&app, "POST", &format!("/api/v1/zones/{zone}/next")).await;
    assert_eq!(statut, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "playing", "la zone s'est arrêtée : {v}");
    let etat = state.playback.get_state(zone).await;
    assert_eq!(etat.state, PlayState::Playing, "{etat:?}");
    assert_eq!(etat.queue_position, 1, "{etat:?}");
    assert_eq!(
        etat.now_playing.as_ref().map(|np| np.title.as_str()),
        Some("Piste 2"),
        "{etat:?}"
    );
    assert_eq!(
        PlayQueueRepo::with_backend(state.backend.clone())
            .count_all(zone)
            .unwrap(),
        2
    );
}

/// Sans lecture automatique, la fin de file arrête la zone, comme avant.
#[tokio::test]
async fn fin_de_file_sans_lecture_automatique_arrete_8480() {
    let (app, state, zone, _dir) = banc(AutoplayMode::Off, 1000).await;
    let (statut, v) = appeler(&app, "POST", &format!("/api/v1/zones/{zone}/next")).await;
    assert_eq!(statut, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "stopped", "{v}");
    assert_eq!(
        state.playback.get_state(zone).await.state,
        PlayState::Stopped
    );
    assert_eq!(
        PlayQueueRepo::with_backend(state.backend.clone())
            .count_all(zone)
            .unwrap(),
        1
    );
}

/// « Suivant » pressé au début d'une dernière piste de 10 min : ce n'est pas
/// une fin de piste, la zone s'arrête comme avant, rien n'est ajouté.
#[tokio::test]
async fn suivant_au_milieu_de_la_derniere_piste_arrete_8480() {
    let (app, state, zone, _dir) = banc(AutoplayMode::Similar, 600_000).await;
    let (statut, v) = appeler(&app, "POST", &format!("/api/v1/zones/{zone}/next")).await;
    assert_eq!(statut, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "stopped", "{v}");
    assert_eq!(
        state.playback.get_state(zone).await.state,
        PlayState::Stopped
    );
    assert_eq!(
        PlayQueueRepo::with_backend(state.backend.clone())
            .count_all(zone)
            .unwrap(),
        1
    );
}
