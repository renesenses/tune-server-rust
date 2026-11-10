//! #7889 — « Lecture » après un redémarrage du serveur garde l'album ou la
//! playlist de la file.
//!
//! Le conteneur de session (`set_session_context`) ne vit qu'en mémoire : après
//! un redémarrage, la file de l'album est retrouvée en base, mais « Lecture »
//! (corps nu, ou `{track_id}` de la barre de transport) rangeait ensuite les
//! écoutes comme des titres isolés. Le redémarrage se simule par un second
//! `AppState::new` sur le même fichier de base ; seule la sortie audio est
//! factice (`MockOutput`), aucun son.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

struct Banc {
    dir: tempfile::TempDir,
    zone: i64,
}

fn wav(path: &std::path::Path) {
    let pcm = vec![0u8; 44100 * 4];
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&44100u32.to_le_bytes());
    w.extend_from_slice(&176400u32.to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(&pcm);
    std::fs::write(path, w).unwrap();
}

impl Banc {
    /// L'album 1 (pistes 1 à 3) sur une base EN FICHIER.
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let state = Self::demarrer_sans_sortie(&dir);
        let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
            .create("Salon 7889", Some("mock"), Some("mock-7889"))
            .unwrap();
        state
            .backend
            .execute_batch("INSERT INTO albums (id, title) VALUES (1, 'Album 7889');")
            .unwrap();
        for id in 1..=3i64 {
            let path = dir.path().join(format!("{id}.wav"));
            wav(&path);
            let mut track = tune_core::db::models::Track::new(format!("Titre {id}"));
            track.file_path = Some(path.to_string_lossy().into_owned());
            track.format = Some("wav".into());
            track.duration_ms = 1000;
            track.sample_rate = Some(44100);
            track.bit_depth = Some(16);
            track.album_id = Some(1);
            track.track_number = id as i32;
            tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone())
                .create(&track)
                .unwrap();
        }
        Self { dir, zone }
    }

    fn demarrer_sans_sortie(dir: &tempfile::TempDir) -> tune_server::state::AppState {
        let base = dir.path().join("tune.db");
        tune_server::state::AppState::new(base.to_str().unwrap(), 0, Default::default()).unwrap()
    }

    /// Un démarrage du serveur : un nouvel état sur la même base, et la sortie
    /// de la zone de nouveau découverte.
    async fn demarrer(&self) -> (tune_server::state::AppState, axum::Router) {
        let state = Self::demarrer_sans_sortie(&self.dir);
        state
            .outputs
            .lock()
            .await
            .register(Box::new(tune_core::outputs::mock::MockOutput::new(
                "mock-7889",
                "Sortie de banc",
            )));
        let app = tune_server::routes::router(state.clone());
        (state, app)
    }
}

async fn poster(app: &axum::Router, uri: &str, corps: Value) -> StatusCode {
    app.clone()
        .oneshot(
            Request::post(uri)
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn contexte(
    state: &tune_server::state::AppState,
    zone: i64,
) -> (Option<String>, Option<String>) {
    let etat = state.playback.get_state(zone).await;
    (etat.session_context_type, etat.session_context_id)
}

/// Lit l'album, attend que sa reprise d'écoute soit en base, puis Stop.
async fn lire_l_album_puis_stop(banc: &Banc) {
    let (state, app) = banc.demarrer().await;
    let zone = banc.zone;
    assert_eq!(
        poster(
            &app,
            &format!("/api/v1/zones/{zone}/play"),
            json!({"album_id": 1})
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        contexte(&state, zone).await,
        (Some("album".into()), Some("1".into()))
    );
    let debut = std::time::Instant::now();
    while tune_core::db::listening_resume::recent(&state.backend, Some(zone), 5)
        .unwrap()
        .iter()
        .all(|r| r.context_type != "album" || r.queue_row_ids.is_empty())
    {
        assert!(
            debut.elapsed() < std::time::Duration::from_secs(20),
            "la reprise d'écoute de l'album n'a jamais été écrite"
        );
        // Depuis #7852 la reprise n'est écrite qu'à la validation de l'écoute
        // (seuil min(30 s, 50 %)), par le sondeur, absent de ce banc : on joue
        // son tour avec une position au-delà du seuil de la piste d'une seconde.
        state.playback.update_position(zone, 900).await;
        let etat = state.playback.get_state(zone).await;
        state.orchestrator.valider_l_ecoute(&etat);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(
        poster(&app, &format!("/api/v1/zones/{zone}/stop"), json!({})).await,
        StatusCode::OK
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn la_relance_d_un_titre_apres_un_redemarrage_garde_l_album_7889() {
    let banc = Banc::new();
    lire_l_album_puis_stop(&banc).await;

    // Redémarrage : la session en mémoire est perdue, la file est en base.
    let (state, app) = banc.demarrer().await;
    let zone = banc.zone;
    assert_eq!(contexte(&state, zone).await, (None, None));
    assert_eq!(
        poster(
            &app,
            &format!("/api/v1/zones/{zone}/play"),
            json!({"track_id": 2})
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        contexte(&state, zone).await,
        (Some("album".into()), Some("1".into())),
        "🔴 après un redémarrage, « Lecture » a gardé la file de l'album mais pas son \
         conteneur : les écoutes suivantes s'inscrivent comme des titres isolés (#7889)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn lecture_nue_apres_un_redemarrage_garde_l_album_7889() {
    let banc = Banc::new();
    lire_l_album_puis_stop(&banc).await;

    let (state, app) = banc.demarrer().await;
    let zone = banc.zone;
    assert_eq!(
        poster(&app, &format!("/api/v1/zones/{zone}/play"), json!({})).await,
        StatusCode::OK
    );
    assert_eq!(
        contexte(&state, zone).await,
        (Some("album".into()), Some("1".into())),
        "🔴 après un redémarrage, « Lecture » nue a repris la file sans son album (#7889)"
    );
}

/// Garde : un titre qui n'est pas dans la file reste un titre isolé, même
/// avec une reprise d'album en base.
#[tokio::test(flavor = "multi_thread")]
async fn un_titre_hors_de_la_file_reste_isole_apres_un_redemarrage_7889() {
    let banc = Banc::new();
    lire_l_album_puis_stop(&banc).await;

    let (state, app) = banc.demarrer().await;
    let zone = banc.zone;
    // La file ne garde que la piste 1 : la reprise (pistes 1 à 3) ne la
    // décrit plus.
    state
        .backend
        .execute(
            "DELETE FROM queue_items WHERE zone_id = ? AND track_id <> 1",
            &[&zone as &dyn tune_core::db::backend::ToSqlValue],
        )
        .unwrap();
    assert_eq!(
        poster(
            &app,
            &format!("/api/v1/zones/{zone}/play"),
            json!({"track_id": 1})
        )
        .await,
        StatusCode::OK
    );
    assert_ne!(
        contexte(&state, zone).await.0.as_deref(),
        Some("album"),
        "une reprise qui ne décrit plus la file a été reprise"
    );
}
