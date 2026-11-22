//! #9362 (fil forum 3000, accord de Bertrand du 22/11/2026, rc15) — trier une
//! playlist par compositeur, date d'ajout ou fréquence d'échantillonnage.
//!
//! `GET /playlists/{id}/tracks` rend, sur CHAQUE ligne, `composer`,
//! `sample_rate`, `bit_depth` et `playlist_added_at` (la date d'ajout de la
//! ligne à CETTE playlist, secondes Unix), `null` quand ils sont inconnus :
//! une ligne d'avant la migration 158, un titre de service dont le service ne
//! dit rien, une ligne introuvable.
//!
//! Contre-épreuve : sans `champs_de_tri`, `playlist_added_at` manque sur
//! toutes les lignes et le premier témoin rougit.
use axum::body::{Body, to_bytes};
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::playlist_repo::{EntryContent, PlaylistRepo, ServiceEntry};

use crate::state::AppState;

async fn lignes(state: &AppState, pid: i64) -> Vec<Value> {
    let rep = super::router()
        .with_state(state.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/{pid}/tracks"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(rep.status().is_success(), "GET tracks : {}", rep.status());
    let corps = to_bytes(rep.into_body(), 4 * 1024 * 1024).await.unwrap();
    serde_json::from_slice(&corps).unwrap()
}

#[tokio::test]
async fn chaque_ligne_porte_compositeur_frequence_et_date_d_ajout_9362() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    state
        .backend
        .execute(
            "INSERT INTO tracks (id, title, file_path, composer, sample_rate, bit_depth) \
             VALUES (10, 'Gymnopédie n° 1', '/m/satie.flac', 'Erik Satie', 96000, 24), \
                    (11, 'La Javanaise', '/m/javanaise.flac', NULL, 44100, 16)",
            &[],
        )
        .unwrap();
    let repo = PlaylistRepo::with_backend(state.backend.clone());
    let pid = repo.create("Classique", None, 1).unwrap();
    let avant = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    repo.add_entries_deduped(
        pid,
        &[
            EntryContent::Local(10),
            EntryContent::Local(11),
            EntryContent::Service(ServiceEntry {
                source: "bandcamp".into(),
                source_id: "bc-1".into(),
                title: "Titre Bandcamp".into(),
                artist: None,
                album: None,
                album_source_id: None,
                duration_ms: Some(1000),
                cover_url: None,
            }),
        ],
        None,
    )
    .unwrap();
    // Une ligne d'avant la migration 158 : date d'ajout inconnue.
    let ancienne = repo.get_entries(pid).unwrap()[1].id;
    state
        .backend
        .execute(
            "UPDATE playlist_tracks SET added_at = NULL WHERE id = ?",
            &[&ancienne as &dyn ToSqlValue],
        )
        .unwrap();

    let v = lignes(&state, pid).await;
    assert_eq!(v.len(), 3);
    for l in &v {
        for cle in ["composer", "sample_rate", "bit_depth", "playlist_added_at"] {
            assert!(
                l.as_object().unwrap().contains_key(cle),
                "`{cle}` absent de la ligne {l}"
            );
        }
    }
    assert_eq!(v[0]["composer"], "Erik Satie");
    assert_eq!(v[0]["sample_rate"], 96000);
    assert_eq!(v[0]["bit_depth"], 24);
    assert!(
        v[0]["playlist_added_at"]
            .as_i64()
            .is_some_and(|d| d >= avant),
        "date d'ajout de la piste ajoutée : {}",
        v[0]
    );
    assert_eq!(v[1]["composer"], Value::Null, "pas de compositeur : null");
    assert_eq!(v[1]["sample_rate"], 44100);
    assert_eq!(
        v[1]["playlist_added_at"],
        Value::Null,
        "ligne d'avant la migration : date inconnue, jamais inventée"
    );
    assert_eq!(v[2]["source"], "bandcamp");
    assert_eq!(v[2]["composer"], Value::Null);
    assert_eq!(v[2]["sample_rate"], Value::Null);
    assert!(v[2]["playlist_added_at"].as_i64().is_some());
}
