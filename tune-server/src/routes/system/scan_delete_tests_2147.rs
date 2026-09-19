use super::{BilanSuppressionDuScan, supprimer_pistes_du_scan};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use tune_core::db::backend::DbBackend;
use tune_core::db::models::Track;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo::TrackRepo;

#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<u8>>>);

impl Write for Journal {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn verifier_suppressions(refuses: &[usize]) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let repo = TrackRepo::with_backend(db.clone());
    let ids: Vec<i64> = (0..3)
        .map(|i| {
            let mut piste = Track::new(format!("piste {i}"));
            piste.file_path = Some(format!("/fixture-2147/{i}.flac"));
            repo.create(&piste).unwrap()
        })
        .collect();
    let ids_refuses: Vec<i64> = refuses.iter().map(|&i| ids[i]).collect();
    for id in &ids_refuses {
        db.execute_batch(&format!(
            "CREATE TRIGGER refuser_piste_{id} BEFORE DELETE ON tracks \
             WHEN OLD.id = {id} BEGIN SELECT RAISE(ABORT, 'fixture_2147_delete_refused'); END;"
        ))
        .unwrap();
    }

    let journal = Journal::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(journal.clone())
        .finish();
    let bilan = tracing::subscriber::with_default(subscriber, || {
        supprimer_pistes_du_scan(&repo, ids, "fixture")
    });
    assert_eq!(
        bilan,
        BilanSuppressionDuScan {
            removed: 3 - ids_refuses.len() as i64,
            db_delete_failed: ids_refuses.len() as i64,
        },
        "une suppression refusée doit être comptée séparément, et les autres tentées"
    );
    let restantes: Vec<i64> = db
        .query_many("SELECT id FROM tracks ORDER BY id", &[])
        .unwrap()
        .into_iter()
        .map(|row| row[0].as_i64().unwrap())
        .collect();
    assert_eq!(restantes, ids_refuses, "seules les pistes refusées restent");
    let journal = String::from_utf8(journal.0.lock().unwrap().clone()).unwrap();
    let erreurs: Vec<&str> = journal
        .lines()
        .filter(|line| line.contains("scan_track_delete_failed"))
        .collect();
    assert_eq!(erreurs.len(), ids_refuses.len());
    for id in ids_refuses {
        let ligne = erreurs
            .iter()
            .find(|line| line.contains(&format!("track_id={id} ")))
            .expect("chaque refus nomme sa piste");
        assert!(ligne.contains("fixture_2147_delete_refused"));
        assert!(ligne.contains("scan=\"fixture\""));
    }
}

#[test]
fn toutes_les_suppressions_reussissent() {
    verifier_suppressions(&[]);
}

#[test]
fn un_refus_n_empeche_pas_la_suppression_suivante() {
    verifier_suppressions(&[0, 2]);
}

#[test]
fn tous_les_refus_restent_visibles() {
    verifier_suppressions(&[0, 1, 2]);
}
