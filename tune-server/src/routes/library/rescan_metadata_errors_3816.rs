//! Contrat HTTP et événementiel réel, SQLite et FLAC locaux, sans réseau.
use crate::state::AppState;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::collections::HashMap;
use tower::ServiceExt;
use tune_core::db::{backend::ToSqlValue, track_metadata_repo::TrackMetadataRepo};

async fn requete(state: &AppState, methode: &str, uri: &str) -> (StatusCode, Value) {
    let response = crate::routes::router(state.clone())
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

async fn flac_etiquete(dir: &std::path::Path) -> std::path::PathBuf {
    let original = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../tune-core/tests/fixtures/test.flac");
    let cible = dir.join("source.flac");
    std::fs::copy(original, &cible).unwrap();
    let champs = HashMap::from([
        ("composer".into(), "António Carlos Jobim".into()),
        ("isrc".into(), "GBKRR0900301".into()),
        ("comment".into(), "24-96".into()),
    ]);
    tune_core::metadata::tag_writer::write_metadata_to_file(cible.to_str().unwrap(), &champs)
        .await
        .unwrap();
    cible
}

fn inscrire(state: &AppState, path: &std::path::Path) -> i64 {
    let path = path.to_str().unwrap();
    state
        .backend
        .execute(
            "INSERT INTO tracks (title, format, file_path) VALUES ('Témoin', 'flac', ?)",
            &[&path as &dyn ToSqlValue],
        )
        .unwrap();
    state.backend.last_insert_rowid()
}

async fn relecture(state: &AppState) -> Value {
    let mut events = state.event_bus.subscribe();
    let (status, _) = requete(state, "POST", "/api/v1/library/rescan-metadata").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let event = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let event = events.recv().await.expect("événement, sans perte");
            if event.event_type == "library.rescan_metadata.completed" {
                break event;
            }
        }
    })
    .await
    .expect("fin de la relecture bornée");
    let (status, stored) = requete(state, "GET", "/api/v1/library/rescan-metadata/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(stored["status"], "idle");
    assert_eq!(
        stored["result"], event.data,
        "HTTP et événement doivent porter le même bilan"
    );
    event.data
}

#[tokio::test]
async fn nominal_le_reliquat_remplit_le_magasin_sans_erreur() {
    let dir = tempfile::tempdir().unwrap();
    let path = flac_etiquete(dir.path()).await;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let id = inscrire(&state, &path);
    let bilan = relecture(&state).await;
    assert_eq!(
        bilan,
        json!({
            "total": 1, "updated": 1, "skipped": 0, "errors": 0,
            "extended_metadata_failed_batches": 0, "has_errors": false,
        })
    );
    let (status, metadata) = requete(
        &state,
        "GET",
        &format!("/api/v1/library/tracks/{id}/metadata"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(metadata["composer"], "António Carlos Jobim");
    assert_eq!(metadata["comment"], "24-96");
}

#[tokio::test]
async fn refus_sql_compte_un_lot_de_500_et_un_reliquat_pas_501_pistes() {
    let dir = tempfile::tempdir().unwrap();
    let source = flac_etiquete(dir.path()).await;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    // file_path est UNIQUE. 501 noms, un seul contenu FLAC sur disque.
    for i in 0..501 {
        let path = dir.path().join(format!("{i}.flac"));
        std::fs::hard_link(&source, &path).unwrap();
        inscrire(&state, &path);
    }
    state
        .backend
        .execute_batch(
            "CREATE TRIGGER refuser_etendues BEFORE INSERT ON track_metadata
         BEGIN SELECT RAISE(ABORT, 'refus SQL témoin 3816'); END;",
        )
        .unwrap();
    let bilan = relecture(&state).await;
    assert_eq!(
        bilan["extended_metadata_failed_batches"], 2,
        "un refus par lot, plein puis reliquat"
    );
    assert_eq!(
        bilan["has_errors"], true,
        "la perte des balises étendues doit dégrader le bilan"
    );
    // Ces compteurs gardent leur sens : mise à jour de la ligne tracks.
    assert_eq!(bilan["total"], 501);
    assert_eq!(bilan["updated"], 501);
    assert_eq!(bilan["skipped"], 0);
    assert_eq!(bilan["errors"], 0);
    let rows = state
        .backend
        .query_many("SELECT COUNT(*) FROM track_metadata", &[])
        .unwrap();
    assert_eq!(rows[0][0].as_i64(), Some(0));
}

#[tokio::test]
async fn refus_partiel_conserve_le_premier_champ_sans_inventer_un_nombre_de_pistes() {
    let dir = tempfile::tempdir().unwrap();
    let path = flac_etiquete(dir.path()).await;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let id = inscrire(&state, &path);
    // L'ordre HashMap est libre : accepter le premier champ, quel qu'il soit,
    // puis refuser le suivant. Le premier upsert reste durable.
    state
        .backend
        .execute_batch(
            "CREATE TRIGGER refuser_apres_premier BEFORE INSERT ON track_metadata
         WHEN (SELECT COUNT(*) FROM track_metadata) >= 1
         BEGIN SELECT RAISE(ABORT, 'refus après écriture partielle'); END;",
        )
        .unwrap();
    let bilan = relecture(&state).await;
    assert_eq!(bilan["extended_metadata_failed_batches"], 1);
    assert_eq!(bilan["has_errors"], true);
    assert_eq!(bilan["updated"], 1);
    assert_eq!(bilan["errors"], 0);
    let metadata = TrackMetadataRepo::with_backend(state.backend.clone())
        .get_all(id)
        .unwrap();
    assert_eq!(
        metadata.len(),
        1,
        "le refus ne doit ni annuler ni effacer le premier champ écrit"
    );
}

#[tokio::test]
async fn refus_de_piste_seul_active_has_errors_sans_inventer_un_refus_de_lot() {
    let dir = tempfile::tempdir().unwrap();
    let path = flac_etiquete(dir.path()).await;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    inscrire(&state, &path);
    state
        .backend
        .execute_batch(
            "CREATE TRIGGER refuser_piste BEFORE UPDATE ON tracks
         BEGIN SELECT RAISE(ABORT, 'refus de mise à jour de piste'); END;",
        )
        .unwrap();
    let bilan = relecture(&state).await;
    assert_eq!(
        bilan,
        json!({
            "total": 1, "updated": 0, "skipped": 0, "errors": 1,
            "extended_metadata_failed_batches": 0, "has_errors": true,
        })
    );
}
