//! #4907 — témoin 4 de bout en bout : l'exemplaire préféré injoignable, la
//! lecture PART quand même, depuis l'exemplaire suivant.
//!
//! Mesuré sur ce que rend `resolve_local_track` — la fonction que `play()`
//! appelle — et non sur la règle seule : sans le branchement dans
//! `resolve_local.rs`, la piste échoue en `file_not_found:` alors qu'une copie
//! est là.

use std::sync::Arc;

use tokio::sync::Mutex;

use super::{PlayRequest, PlaybackOrchestrator};
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;

fn orchestrateur() -> PlaybackOrchestrator {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    )
}

fn requete(zone_id: i64, track_id: i64) -> PlayRequest {
    PlayRequest {
        zone_id,
        output_device_id: None,
        track_id: Some(track_id),
        source: Some("local".into()),
        source_id: None,
        title: None,
        artist_name: None,
        album_title: None,
        cover_url: None,
        duration_ms: None,
        seek_ms: None,
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
    }
}

#[tokio::test]
async fn le_nas_eteint_la_piste_part_depuis_la_copie_locale() {
    let orch = orchestrateur();
    let dossier = tempfile::tempdir().unwrap();
    let nas = dossier.path().join("nas");
    let local = dossier.path().join("local");
    let sur_le_nas = nas.join("Kind of Blue/01.flac");
    let en_local = local.join("Kind of Blue/01.flac");
    std::fs::create_dir_all(en_local.parent().unwrap()).unwrap();
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac"),
        &en_local,
    )
    .unwrap();
    // Le NAS est ÉTEINT : son dossier n'existe pas.
    let (sur_le_nas, en_local) = (
        sur_le_nas.to_string_lossy().into_owned(),
        en_local.to_string_lossy().into_owned(),
    );
    let dirs = serde_json::to_string(&[
        nas.to_string_lossy().into_owned(),
        local.to_string_lossy().into_owned(),
    ])
    .unwrap();
    let db = &orch.db;
    let p: [&dyn ToSqlValue; 1] = [&dirs];
    db.execute(
        "INSERT INTO settings (key, value, updated_at) VALUES ('music_dirs', ?, '')",
        &p,
    )
    .unwrap();
    db.execute_batch(
        "INSERT INTO artists (id, name) VALUES (1, 'Miles Davis');
         INSERT INTO albums (id, title, artist_id) VALUES (1, 'Kind of Blue', 1);",
    )
    .unwrap();
    let p: [&dyn ToSqlValue; 1] = [&sur_le_nas];
    db.execute(
        "INSERT INTO tracks (id, title, album_id, artist_id, file_path, format, \
         duration_ms, sample_rate, bit_depth, channels) \
         VALUES (4907, 'So What', 1, 1, ?, 'flac', 300000, 44100, 16, 2)",
        &p,
    )
    .unwrap();
    // Le NAS est aussi le répertoire PRÉFÉRÉ de l'album.
    let preferee = nas.to_string_lossy().into_owned();
    crate::library::exemplaires::poser_racine_preferee(&**db, 1, &preferee).unwrap();
    let p: [&dyn ToSqlValue; 1] = [&en_local];
    db.execute(
        "INSERT INTO track_copies (track_id, file_path, format, sample_rate, bit_depth) \
         VALUES (4907, ?, 'flac', 44100, 16)",
        &p,
    )
    .unwrap();
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Ce PC", Some("browser"), None)
        .unwrap();

    let refus = orch
        .resolve_local_track(&requete(zone_id, 4907))
        .await
        .err();
    assert!(
        refus.is_none(),
        "le NAS éteint, la piste doit partir depuis la copie locale ; elle a rendu : {}",
        refus.unwrap_or_default()
    );
    let lu = crate::library::exemplaires::exemplaire_lu(4907)
        .expect("la lecture doit noter l'exemplaire ouvert");
    assert_eq!(lu.chemin, en_local, "c'est la copie locale qui est lue");
    assert!(lu.repli, "et c'est un REPLI : le préféré ne répondait pas");
}
