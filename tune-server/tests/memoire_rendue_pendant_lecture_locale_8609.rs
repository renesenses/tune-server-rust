//! #8609 — la purge de cinq minutes a lieu PENDANT une lecture locale, et le
//! journal le dit (`memoire_rendue_a_froid … pendant_lecture_locale=true`).
//!
//! Fil 2167 : un testeur écoute toute la journée sur sa carte son (ALSA).
//! Avant le correctif, `purger_a_froid` refusait la purge dès qu'une sortie
//! locale jouait : 505 Mo → 5 Go en dix heures, et pas une seule ligne
//! `memoire_rendue_a_froid` dans son journal.
//!
//! Binaire à lui seul, un seul test : l'abonné `tracing` est GLOBAL (voir
//! `panne_sql_journalisee.rs`), et le scénario mesure la mémoire du processus.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_purge_tourne_et_se_journalise_pendant_une_lecture_locale() {
    use std::sync::{Arc, Mutex};
    use tune_core::db::backend::DbBackend;
    use tune_core::db::sqlite::SqliteDb;
    use tune_server::memoire_a_froid::{DecisionDePurge, purger_a_froid, une_sortie_locale_joue};

    #[derive(Clone, Default)]
    struct Journal(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Journal {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
        type Writer = Journal;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }
    let journal = Journal::default();
    tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .init();

    // Une sortie LOCALE en lecture — le cas du fil 2167.
    let db = SqliteDb::open_in_memory().unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db.clone());
    db.execute_batch(
        "CREATE TABLE zones (id INTEGER PRIMARY KEY, name TEXT, output_type TEXT,
                             group_id INTEGER, last_play_state TEXT);
         INSERT INTO zones (id, name, output_type, group_id, last_play_state) VALUES
             (18, 'Eversolo DMP-A6', 'local', NULL, 'playing');",
    )
    .unwrap();
    assert!(une_sortie_locale_joue(&backend));

    // De la mémoire libre GARDÉE par les arènes : plusieurs fils allouent et
    // libèrent des blocs sous le seuil `mmap`, et chacun garde un bouchon au
    // haut de son tas (le scénario de `memoire_a_froid.rs`). ~200 Mo.
    const FILS: usize = 8;
    const BLOCS: usize = 400;
    const TAILLE: usize = 64 * 1024;
    let bouchons: Vec<Vec<u8>> = std::thread::scope(|s| {
        let fils: Vec<_> = (0..FILS)
            .map(|_| {
                s.spawn(|| {
                    let mut blocs: Vec<Vec<u8>> = (0..BLOCS).map(|_| vec![1u8; TAILLE]).collect();
                    let bouchon = vec![2u8; 512];
                    blocs.clear();
                    drop(blocs);
                    bouchon
                })
            })
            .collect();
        fils.into_iter().map(|f| f.join().unwrap()).collect()
    });
    let tas = tune_core::memoire_rendue::etat_du_tas();
    let libre = tas.expect("glibc >= 2.33 sur la machine de test").libre_mb;
    assert!(
        libre >= tune_server::memoire_a_froid::SEUIL_LIBRE_PENDANT_LECTURE_MB,
        "le scénario doit laisser de la mémoire libre gardée : {libre} Mo"
    );

    let (decision, purge) = purger_a_froid(&backend, tas)
        .await
        .expect("la purge doit avoir lieu pendant une lecture locale (#8609)");
    drop(bouchons);
    assert_eq!(decision, DecisionDePurge::PendantLectureLocale);
    // « Rendu » veut dire : le RSS a BAISSÉ. Sous `mlockall`, la glibc dit
    // « rendu » sans que rien ne revienne — la ligne doit alors le dire.
    if !purge.sous_mlock() {
        assert!(
            purge.rendu_mb() >= 100,
            "~200 Mo libres gardés : le RSS devait baisser d'au moins 100 Mo : {purge:?}"
        );
    }

    let texte = String::from_utf8_lossy(&journal.0.lock().unwrap()).into_owned();
    let ligne = texte
        .lines()
        .find(|l| l.contains("memoire_rendue_a_froid"))
        .unwrap_or_else(|| {
            panic!("aucune ligne memoire_rendue_a_froid dans le journal :\n{texte}")
        });
    assert!(
        ligne.contains("pendant_lecture_locale=true"),
        "la ligne doit dire que la purge a eu lieu pendant la lecture locale : {ligne}"
    );
    assert!(ligne.contains("tas_libre_avant_mb="), "{ligne}");
    assert!(ligne.contains("memoire_verrouillee_mb="), "{ligne}");
    assert!(
        ligne.contains(&format!("rendu={}", purge.rendu()))
            && ligne.contains(&format!("rendu_mb={}", purge.rendu_mb())),
        "la ligne doit porter la baisse MESURÉE du RSS : {ligne}"
    );
}
