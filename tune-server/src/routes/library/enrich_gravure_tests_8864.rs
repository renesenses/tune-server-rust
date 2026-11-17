//! #8864 : la gravure réelle du DR ne doit pas effacer une tentative.
use super::*;
use std::time::{Duration, UNIX_EPOCH};
use tune_core::db::enrich_tentatives::{self, Issue};
use tune_core::db::track_metadata_repo::TrackMetadataRepo;

const T0: i64 = 1_800_000_000;

fn selection(s: &AppState, now: i64) -> Vec<(i64, bool)> {
    s.backend
        .query_many(&sql_candidats_enrichissement(&s.backend, now), &[])
        .unwrap()
        .iter()
        .map(|r| (r[0].as_i64().unwrap(), est_un_reessai(r)))
        .collect()
}

async fn eprouver_gravure(s: AppState, issue: Issue) {
    let dir =
        tune_core::test_scratch::scratch_dir_in(std::env::current_dir().unwrap(), "enrich-8864");
    let path = dir.path().join("enrichie.flac");
    std::fs::copy(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tune-core/tests/fixtures/test.flac"),
        &path,
    )
    .unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        .unwrap();
    let chemin = path.to_string_lossy().into_owned();
    let (size, mtime) = tune_core::audio::iso9660::taille_et_mtime(&path).unwrap();
    s.backend.execute(
        "INSERT INTO tracks (id, title, file_path, file_size, file_mtime, source) VALUES (1, 'DR synthétique', ?1, ?2, ?3, 'local')",
        &[&chemin, &(size as i64), &mtime]).unwrap();
    assert_eq!(selection(&s, T0), vec![(1, false)]);
    enrich_tentatives::noter(&s.backend, 1, issue, T0).unwrap();
    assert!(selection(&s, T0).is_empty());
    let repo = TrackMetadataRepo::with_backend(s.backend.clone());
    repo.set(1, "dr_track", "12").unwrap();
    repo.set(1, "dr_source", "analysis").unwrap();
    crate::routes::ecriture_fichiers::activer_pour_test(&s.backend);
    let response = crate::routes::library::graver_dr::lancer(State(s.clone())).await;
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    let mut written = false;
    for _ in 0..200 {
        let Json(status) = crate::routes::library::graver_dr::statut(State(s.clone())).await;
        if status["written"] == 1 {
            written = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(written, "la passe réelle doit avoir gravé le DR");
    assert_ne!(
        tune_core::audio::iso9660::taille_et_mtime(&path).unwrap(),
        (size, mtime)
    );
    assert!(crate::auto_scan::fichier_conforme_a_la_base(
        &s.backend, &chemin
    ));
    assert!(
        selection(&s, T0).is_empty(),
        "la gravure DR interne remet une piste déjà tentée dans les restantes : {:?}",
        selection(&s, T0)
    );
    if issue == Issue::EchecTemporaire {
        assert_eq!(
            selection(&s, T0 + 3600),
            vec![(1, true)],
            "le 503 doit redevenir un réessai, pas une nouvelle piste"
        );
        let rows = selection(&s, T0 + 3600);
        let total = rows.iter().filter(|(_, retry)| !retry).count();
        assert_eq!(
            releve_enrichissement("running", "8864", 0, 0, total, 0, 0, 1)["remaining"],
            0
        );
    }
    // Changement extérieur : le vrai scan doit relire cette nouvelle date.
    tune_core::db::settings_repo::SettingsRepo::with_backend(s.backend.clone())
        .set(
            "music_dirs",
            &serde_json::to_string(&[dir.to_string_lossy()]).unwrap(),
        )
        .unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(UNIX_EPOCH + Duration::from_secs(1_750_000_000))
        .unwrap();
    scanner(&s).await;
    assert_eq!(
        selection(&s, T0),
        vec![(1, false)],
        "une modification extérieure relue par le scan doit redevenir candidate"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sqlite_gravure_conserve_enrichissement_8864() {
    let _data = crate::premium_audio_plugins::tests::dossier_de_donnees_jetable();
    for issue in [Issue::Fait, Issue::Introuvable, Issue::EchecTemporaire] {
        eprouver_gravure(
            AppState::new(":memory:", 0, Default::default()).unwrap(),
            issue,
        )
        .await;
    }
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_gravure_conserve_enrichissement_8864() {
    use std::sync::Arc;
    use tune_core::db::backend::{DbBackend, PostgresBackend};
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT PostgreSQL #8864 : TUNE_TEST_PG_URL absent");
        return;
    };
    let _data = crate::premium_audio_plugins::tests::dossier_de_donnees_jetable();
    let pg = tune_core::db::postgres::PostgresDb::connect(&url)
        .await
        .unwrap();
    let admin: Arc<dyn DbBackend> = Arc::new(PostgresBackend::new(pg.pool().clone()));
    for issue in [Issue::Fait, Issue::Introuvable, Issue::EchecTemporaire] {
        let schema = format!("essai_8864_{}", uuid::Uuid::new_v4().simple());
        admin
            .execute(&format!("CREATE SCHEMA {schema}"), &[])
            .unwrap();
        let sep = if url.contains('?') { '&' } else { '?' };
        let config = crate::config::TuneConfig {
            database_url: Some(format!("{url}{sep}options=-csearch_path%3D{schema}")),
            ..Default::default()
        };
        let state = AppState::new(":memory:", 0, config).unwrap();
        eprouver_gravure(state, issue).await;
        admin
            .execute(&format!("DROP SCHEMA {schema} CASCADE"), &[])
            .unwrap();
    }
    pg.pool().close().await;
}

async fn scanner(s: &AppState) {
    let _seul = crate::routes::system::scan::serialiser_les_scans_de_test_sans_bloquer().await;
    crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
    let mut rx = s.event_bus.subscribe();
    assert!(crate::routes::system::scan::spawn_library_scan(s.clone(), false, None).await);
    let fin = tune_core::event_types::EventType::ScanComplete.as_str();
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            match rx.recv().await {
                Ok(ev) if ev.event_type == fin => break,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("bus fermé : {e}"),
            }
        }
    })
    .await
    .expect("le vrai scan doit finir");
    crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
}
