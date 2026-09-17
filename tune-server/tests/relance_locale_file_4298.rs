//! #4298 : une relance locale ne réduit pas une file existante à un titre.
//! Requêtes HTTP, vraie base et fichiers WAV temporaires ; seule la sortie
//! audio est factice. Inscrit dans server_contracts (autotests = false).
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::{play_queue_repo::PlayQueueRepo, zone_repo::ZoneRepo};
use tune_server::state::AppState;

struct Banc {
    state: AppState,
    app: axum::Router,
    zone: i64,
    _dir: tempfile::TempDir,
}

impl Banc {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let zone = ZoneRepo::with_backend(state.backend.clone())
            .create("File 4298", Some("mock"), Some("mock-4298"))
            .unwrap();
        state
            .outputs
            .lock()
            .await
            .register(Box::new(tune_core::outputs::mock::MockOutput::new(
                "mock-4298",
                "Sortie de banc",
            )));
        state
            .backend
            .execute_batch(
                "INSERT INTO albums (id, title) VALUES (1, 'Album');
             INSERT INTO playlists (id, name, profile_id) VALUES (1, 'Liste', 1);",
            )
            .unwrap();
        for id in 1..=4i64 {
            let path = dir.path().join(format!("{id}.wav"));
            let pcm = vec![0u8; 44100 * 4];
            let mut wav = Vec::new();
            wav.extend_from_slice(b"RIFF");
            wav.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
            wav.extend_from_slice(b"WAVEfmt ");
            wav.extend_from_slice(&16u32.to_le_bytes());
            wav.extend_from_slice(&1u16.to_le_bytes());
            wav.extend_from_slice(&2u16.to_le_bytes());
            wav.extend_from_slice(&44100u32.to_le_bytes());
            wav.extend_from_slice(&176400u32.to_le_bytes());
            wav.extend_from_slice(&4u16.to_le_bytes());
            wav.extend_from_slice(&16u16.to_le_bytes());
            wav.extend_from_slice(b"data");
            wav.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
            wav.extend_from_slice(&pcm);
            std::fs::write(&path, wav).unwrap();
            let mut track = tune_core::db::models::Track::new(format!("Titre {id}"));
            track.file_path = Some(path.to_string_lossy().into_owned());
            track.format = Some("wav".into());
            track.duration_ms = 1000;
            track.sample_rate = Some(44100);
            track.bit_depth = Some(16);
            track.album_id = Some(1);
            track.track_number = id as i32;
            assert_eq!(
                tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone())
                    .create(&track)
                    .unwrap(),
                id
            );
        }
        state.backend.execute_batch(
            "INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES (1, 2, 0), (1, 4, 1);"
        ).unwrap();
        let app = tune_server::routes::router(state.clone());
        let b = Self {
            state,
            app,
            zone,
            _dir: dir,
        };
        b.repo().set_queue(b.zone, &[1, 2, 3]).unwrap();
        b
    }

    fn repo(&self) -> PlayQueueRepo {
        PlayQueueRepo::with_backend(self.state.backend.clone())
    }

    async fn play(&self, body: Value) -> Value {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::post(format!("/api/v1/zones/{}/play", self.zone))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            status,
            StatusCode::OK,
            "lecture refusée: {}",
            String::from_utf8_lossy(&bytes)
        );
        serde_json::from_slice(&bytes).unwrap()
    }

    fn rows(&self) -> Vec<(i64, Option<i64>, i64)> {
        self.repo()
            .get_ordered(self.zone)
            .unwrap()
            .iter()
            .map(|e| (e.id, e.track_id, e.position))
            .collect()
    }

    async fn position(&self, position: i64, length: i64, track: i64) {
        let state = self.state.playback.get_state(self.zone).await;
        assert_eq!(
            state.queue_position, position,
            "position dans la file conservée"
        );
        assert_eq!(state.queue_length, length, "longueur annoncée");
        assert_eq!(
            state.now_playing.unwrap().track_id,
            Some(track),
            "titre demandé réellement démarré"
        );
        let rows = self.repo().get_ordered(self.zone).unwrap();
        let current: Vec<_> = rows
            .iter()
            .filter(|e| e.is_current)
            .map(|e| e.position)
            .collect();
        assert_eq!(
            current,
            vec![position],
            "une seule entrée courante, y compris en file mixte"
        );
    }
}

#[tokio::test]
async fn relance_locale_garde_les_lignes_et_le_rang() {
    let b = Banc::new().await;
    b.repo().set_current_pos(b.zone, 1).unwrap();
    let before = b.rows();
    b.play(json!({"track_id": 2})).await;
    assert_eq!(
        b.rows(),
        before,
        "#4298 : la relance a remplacé l'album par un seul titre"
    );
    b.position(1, 3, 2).await;
}

#[tokio::test]
async fn relance_locale_choisit_l_occurrence_courante_du_doublon() {
    let b = Banc::new().await;
    b.repo().set_queue(b.zone, &[2, 1, 2, 3]).unwrap();
    b.repo().set_current_pos(b.zone, 2).unwrap();
    let before = b.rows();
    b.play(json!({"track_id": 2})).await;
    assert_eq!(
        b.rows(),
        before,
        "les doublons intentionnels ne doivent pas disparaître"
    );
    b.position(2, 4, 2).await;
}

#[tokio::test]
async fn relance_locale_preserve_une_file_mixte_et_deplace_le_courant() {
    let b = Banc::new().await;
    // Une entrée de service en tête, précédemment courante ; aucun compte
    // ni résolution de service : seule la piste LOCALE 2 est jouée.
    b.state
        .backend
        .execute_batch(&format!(
            "UPDATE queue_items SET position = position + 1 WHERE zone_id = {};
         UPDATE queue_items SET is_current = 0 WHERE zone_id = {};
         INSERT INTO queue_items (zone_id, position, source, source_id, title, is_current)
         VALUES ({}, 0, 'service-de-banc', '2', 'Avant', 1);",
            b.zone, b.zone, b.zone
        ))
        .unwrap();
    let before = b.rows();
    b.play(json!({"track_id": 2})).await;
    assert_eq!(b.rows(), before, "la file mixte a été effacée");
    b.position(2, 4, 2).await;
}

#[tokio::test]
async fn piste_hors_file_remplace_encore_la_file() {
    let b = Banc::new().await;
    b.play(json!({"track_id": 4})).await;
    assert_eq!(
        b.rows().iter().map(|r| r.1).collect::<Vec<_>>(),
        vec![Some(4)]
    );
    b.position(0, 1, 4).await;
}

#[tokio::test]
async fn contenant_explicite_remplace_meme_si_le_titre_est_deja_present() {
    for (body, expected, position) in [
        (json!({"track_ids": [2]}), vec![2], 0),
        (json!({"track_id": 2, "start_index": 0}), vec![2], 0),
        (json!({"album_id": 1, "track_id": 2}), vec![1, 2, 3, 4], 1),
        (json!({"playlist_id": 1, "track_id": 2}), vec![2, 4], 0),
    ] {
        let b = Banc::new().await;
        b.play(body.clone()).await;
        assert_eq!(
            b.rows().iter().map(|r| r.1.unwrap()).collect::<Vec<_>>(),
            expected,
            "{body}"
        );
        b.position(position, expected.len() as i64, 2).await;
    }
}
