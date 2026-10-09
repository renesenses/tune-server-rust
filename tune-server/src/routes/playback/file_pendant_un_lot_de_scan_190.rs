//! Ticket 190 — « Lire » pendant un scan n'attend plus la fin du lot.
//!
//! Le terrain (hôte lent, bibliothèque en cours de scan) : une écriture de
//! file a attendu plus d'une minute, d'autres plusieurs dizaines de secondes,
//! toutes libérées dans la même seconde. Pendant que le lot de scan tenait la
//! porte, l'auditeur a rappuyé sur Lire ; ses demandes sont parties ensemble
//! à la fin du lot, en rafale, et l'orchestrateur a dû les écarter une à une.
//!
//! La cession entre deux fichiers (#5202) existait déjà, mais seulement pour
//! les écrivains de la CONNEXION : une écriture de file attend la PORTE avant
//! de demander la connexion, et la porte restait tenue tout le lot.
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::set_queue_retrying;
use tune_core::db::backend::DbBackend;
use tune_core::db::models::Track;
use tune_core::db::play_queue_repo::PlayQueueRepo;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo::TrackRepo;

/// Durée du faux lot de scan : bien plus que l'attente tolérée ci-dessous.
const DUREE_DU_LOT: Duration = Duration::from_secs(6);

/// Ce qu'une écriture de file peut attendre : le travail de quelques
/// fichiers, pas un lot.
const ATTENTE_TOLEREE: Duration = Duration::from_secs(2);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lire_pendant_un_lot_de_scan_passe_a_la_prochaine_cession_190() {
    let db = SqliteDb::open_in_memory().expect("SQLite en mémoire");
    db.init_schema().expect("schéma");
    db.execute(
        "INSERT INTO zones (name, output_type) VALUES ('Salon', 'local')",
        &[],
    )
    .expect("zone");
    let backend: Arc<dyn DbBackend> = Arc::new(db.clone());
    let pistes = TrackRepo::with_backend(backend.clone());
    let mut ids = Vec::new();
    for i in 0..8 {
        let mut t = Track::new(format!("Piste {i}"));
        t.file_path = Some(format!("/music/album/{i:02}.flac"));
        ids.push(pistes.create(&t).expect("piste"));
    }

    // Le lot de scan, comme en production : porte, BEGIN IMMEDIATE, puis un
    // fichier toutes les 50 ms avec son point de cession, et une écriture
    // du lot à chaque fichier.
    let (pret_tx, pret_rx) = tokio::sync::oneshot::channel();
    let lot_backend = backend.clone();
    let lot = tokio::task::spawn_blocking(move || {
        let mut porte = Some(crate::sqlite_write_gate::scan_batch());
        lot_backend
            .execute_batch("BEGIN IMMEDIATE")
            .expect("BEGIN du lot");
        tune_core::db::tx_holder::declarer("scan:lot");
        let _ = pret_tx.send(());
        let debut = Instant::now();
        let mut fichiers = 0;
        while debut.elapsed() < DUREE_DU_LOT {
            crate::sqlite_write_gate::ceder_le_lot(lot_backend.as_ref(), &mut porte, "scan:lot");
            let cle = format!("lot_190_{fichiers}");
            lot_backend
                .execute(
                    "INSERT INTO zones (name, output_type) VALUES (?1, 'local')",
                    &[&cle],
                )
                .expect("écriture du lot");
            fichiers += 1;
            std::thread::sleep(Duration::from_millis(50));
        }
        tune_core::db::tx_holder::liberer();
        lot_backend.execute_batch("COMMIT").expect("COMMIT du lot");
        drop(porte);
        fichiers
    });
    pret_rx.await.expect("le lot a ouvert sa transaction");

    let debut = Instant::now();
    let file = PlayQueueRepo::with_backend(backend.clone());
    let issue = set_queue_retrying(&file, true, 1, &ids)
        .await
        .expect("la file s'écrit");
    let attendu = debut.elapsed();
    assert_eq!(issue.inserted, ids.len());
    assert!(
        attendu < ATTENTE_TOLEREE,
        "l'écriture de file a attendu {attendu:?} : elle doit passer à la \
         prochaine cession du lot, pas à sa fin ({DUREE_DU_LOT:?})"
    );
    assert!(
        !lot.is_finished(),
        "le banc ne prouve rien si le lot était déjà fini"
    );

    let fichiers = lot.await.expect("fin du lot");
    // Rien de perdu des deux côtés : la file entière, et chaque écriture du
    // lot, avant et après la cession.
    let entrees = PlayQueueRepo::with_backend(backend.clone())
        .get_queue(1)
        .expect("file relue");
    assert_eq!(entrees.iter().map(|e| e.track_id).collect::<Vec<_>>(), ids);
    let ecrites = backend
        .query_one(
            "SELECT COUNT(*) FROM zones WHERE name LIKE 'lot_190_%'",
            &[],
        )
        .expect("compte")
        .expect("une ligne");
    assert_eq!(
        format!("{:?}", ecrites[0]),
        format!("{:?}", tune_core::db::backend::SqlValue::Int(fichiers))
    );
}

/// Sans écriture de file en attente, la cession reste celle de la connexion :
/// le lot garde sa porte et sa transaction.
#[test]
fn sans_file_en_attente_le_lot_garde_sa_porte_190() {
    let db = SqliteDb::open_in_memory().expect("SQLite en mémoire");
    db.init_schema().expect("schéma");
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let mut porte = Some(crate::sqlite_write_gate::scan_batch());
    backend.execute_batch("BEGIN IMMEDIATE").expect("BEGIN");
    let a_cede = crate::sqlite_write_gate::ceder_le_lot(backend.as_ref(), &mut porte, "scan:lot");
    assert!(!a_cede || crate::sqlite_write_gate::file_en_attente());
    assert!(porte.is_some(), "la porte reste tenue par le lot");
    backend.execute_batch("COMMIT").expect("COMMIT");
}
