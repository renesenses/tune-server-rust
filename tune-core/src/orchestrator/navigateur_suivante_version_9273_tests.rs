//! #9273 — « Ce téléphone » : `browser-next` refusé en 409 à chaque piste.
//!
//! Quand la règle de version (#2264) fait jouer un autre exemplaire que la
//! ligne de file lancée (le 96/24 du même enregistrement, une version de
//! service…), `now_playing` porte la piste JOUÉE, et la ligne de file la
//! piste DEMANDÉE. La garde de la préparation comparait la ligne à la piste
//! jouée : elle y voyait une file changée (`browser_queue_changed`, 409) à
//! chaque piste substituée, et le téléphone n'avait jamais de suivante en
//! file (aucun enchaînement sans blanc).
//!
//! Le banc : une zone navigateur, « So What » en deux exemplaires LOCAUX du
//! même enregistrement (44,1/16 et 96/24) et un « So What (Live) », autre
//! enregistrement. Règle `local` : lancer le 44,1/16 joue le 96/24.
use super::super::PlaybackOrchestrator;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;
use std::sync::Arc;
use tokio::sync::Mutex;
/// 44,1/16, la piste qu'on lance.
const STANDARD: i64 = 1;
/// 96/24, le même enregistrement : celle que la règle fait jouer.
const HAUTE: i64 = 2;
/// Le live : un autre enregistrement, la suivante de la file.
const LIVE: i64 = 3;
async fn banc() -> (PlaybackOrchestrator, i64, tempfile::TempDir) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let orch = PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    );
    let dossier = tempfile::tempdir().unwrap();
    let mut chemins = Vec::new();
    for nom in ["standard.flac", "haute.flac", "live.flac"] {
        let chemin = dossier.path().join(nom);
        std::fs::copy(
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/test.flac"),
            &chemin,
        )
        .unwrap();
        chemins.push(chemin.to_string_lossy().into_owned());
    }
    orch.db
        .execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Miles Davis');
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'Kind of Blue', 1);
             INSERT INTO albums (id, title, artist_id) VALUES (2, 'Kind of Blue (Legacy)', 1);
             INSERT INTO albums (id, title, artist_id) VALUES (3, 'Newport 1958', 1);",
        )
        .unwrap();
    for (id, titre, album, duree, sr, bd, chemin) in [
        (
            STANDARD,
            "So What",
            1i64,
            300_000i64,
            44_100i64,
            16i64,
            &chemins[0],
        ),
        (HAUTE, "So What", 2, 300_500, 96_000, 24, &chemins[1]),
        (LIVE, "So What (Live)", 3, 300_200, 44_100, 16, &chemins[2]),
    ] {
        let p: [&dyn ToSqlValue; 7] = [&id, &titre, &album, &duree, &sr, &bd, chemin];
        orch.db
            .execute(
                "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, sample_rate, \
                 bit_depth, file_path, format, channels, source) \
                 VALUES (?, ?, ?, 1, ?, ?, ?, ?, 'flac', 2, 'local')",
                &p,
            )
            .unwrap();
    }
    let zone_id = ZoneRepo::with_backend(orch.db.clone())
        .create("Ce téléphone 9273", Some("browser"), None)
        .unwrap();
    crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone())
        .set(crate::library::regle_de_version::CLE_REGLE, "local")
        .unwrap();
    (orch, zone_id, dossier)
}
#[tokio::test]
async fn la_version_substituee_n_empeche_pas_de_preparer_la_suivante_9273() {
    let (orch, zone_id, _dossier) = banc().await;
    orch.persist_local_queue(zone_id, &[STANDARD, LIVE], 0);
    orch.playback.update_queue_info(zone_id, 0, 2).await;
    orch.play_from_queue(zone_id, 0).await.unwrap();
    let np = orch
        .playback
        .get_state(zone_id)
        .await
        .now_playing
        .expect("la zone joue");
    assert_eq!(
        np.track_id,
        Some(HAUTE),
        "prérequis : la règle `local` fait jouer le 96/24"
    );
    let courant = np.stream_id.expect("un flux navigateur");
    let suivante = orch
        .preparer_suivante_navigateur(zone_id, &courant)
        .await
        .unwrap_or_else(|e| {
            panic!("#9273 : la piste substituée par la règle de version refuse la suivante ({e})")
        })
        .expect("#9273 : la file a une suivante");
    assert_eq!(
        suivante.source_id.as_deref(),
        Some(LIVE.to_string().as_str()),
        "#9273 : la suivante est la ligne suivante de la file, telle quelle"
    );
}
/// Contre-épreuve : la garde de #5975 tient toujours. Une ligne de file qui
/// n'est ni la piste jouée ni la piste demandée est une file changée.
#[tokio::test]
async fn une_ligne_etrangere_reste_une_file_changee_9273() {
    let (orch, zone_id, _dossier) = banc().await;
    orch.persist_local_queue(zone_id, &[STANDARD, LIVE], 0);
    orch.playback.update_queue_info(zone_id, 0, 2).await;
    orch.play_from_queue(zone_id, 0).await.unwrap();
    let courant = orch
        .playback
        .get_state(zone_id)
        .await
        .now_playing
        .and_then(|np| np.stream_id)
        .expect("un flux navigateur");
    // La file est remplacée sous la piste qui joue (curseur inchangé).
    orch.persist_local_queue(zone_id, &[LIVE, STANDARD], 0);
    assert_eq!(
        orch.preparer_suivante_navigateur(zone_id, &courant)
            .await
            .err()
            .as_deref(),
        Some("browser_queue_changed")
    );
}
