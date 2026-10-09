//! Le repli du WAL ne tient plus la connexion d'écriture.
//!
//! Retour de terrain : sur une machine virtuelle au disque lent, le
//! `COMMIT` d'un lot de scan a tenu la connexion d'écriture jusqu'à 5,4 s
//! (`ecriture_sqlite_detention_longue`). Le repli automatique du WAL tournait
//! dans ce `COMMIT`. Voir [`crate::db::replieur_wal`].
//!
//! ⚠️ Base de FICHIER obligatoire : une base en mémoire n'a pas de WAL.
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::sqlite::SqliteDb;

fn base_fichier(epreuve: &str) -> (crate::test_scratch::ScratchDir, Arc<SqliteDb>, String) {
    let dossier = crate::test_scratch::scratch_dir(&format!("replieur-wal-{epreuve}"));
    let chemin = dossier
        .join("tune-epreuve.db")
        .to_string_lossy()
        .into_owned();
    let db = SqliteDb::open(&chemin).expect("base de fichier");
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    (dossier, Arc::new(db), chemin)
}

fn inserer(db: &dyn DbBackend, i: usize) {
    let chemin = format!("/lot/{i:06}.flac");
    let titre = format!("Piste synthétique {i} — {}", "x".repeat(200));
    let p: [&dyn ToSqlValue; 2] = [&titre, &chemin];
    db.execute("INSERT INTO tracks (title, file_path) VALUES (?, ?)", &p)
        .expect("écriture");
}

fn autocheckpoint(db: &SqliteDb) -> i64 {
    db.connection()
        .lock()
        .unwrap()
        .query_row("PRAGMA wal_autocheckpoint", [], |l| l.get(0))
        .unwrap()
}

#[test]
fn la_connexion_d_ecriture_ne_replie_plus_elle_meme() {
    let (_d, db, _) = base_fichier("pragma");
    assert_eq!(
        autocheckpoint(&db),
        0,
        "la connexion d'écriture replie encore le WAL dans ses COMMIT"
    );
}

#[test]
fn le_wal_est_replie_par_le_fil_du_replieur() {
    let (_d, db, chemin) = base_fichier("fil");
    let taille = || std::fs::metadata(&chemin).map(|m| m.len()).unwrap_or(0);
    // Ce qui précède (schéma, migrations) peut déjà avoir été replié : on
    // part de l'état présent.
    std::thread::sleep(crate::db::replieur_wal::PERIODE * 2);
    let avant = taille();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    for i in 0..3000 {
        inserer(db.as_ref(), i);
    }
    db.execute_batch("COMMIT").unwrap();
    // Sans repli, ces pages restent dans le WAL et le fichier de la base ne
    // grossit pas.
    let limite = Instant::now() + crate::db::replieur_wal::PERIODE * 4;
    while taille() <= avant && Instant::now() < limite {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        taille() > avant,
        "le WAL n'a pas été replié dans la base ({} octets avant, {} après)",
        avant,
        taille()
    );
}

/// Mesure (pas une garde) : le plus long `COMMIT` d'un lot synthétique, et la
/// plus longue attente d'une lecture forte pendant ce temps, avec le repli
/// d'avant (sur la connexion d'écriture) et le repli à part.
/// `cargo test -p tune-core --lib replieur_wal -- --ignored --nocapture`.
#[test]
#[ignore = "mesure, à lancer à la main"]
fn mesure_du_plus_long_commit_avant_et_apres() {
    for tour in 0..3 {
        for (nom, ancien) in [
            ("avant (repli dans le COMMIT)", true),
            ("après (replieur)", false),
        ] {
            let (_d, sqlite, _) = base_fichier(&format!("mesure-{tour}-{ancien}"));
            if ancien {
                sqlite
                    .execute_batch("PRAGMA wal_autocheckpoint=1000")
                    .unwrap();
            }
            let db: Arc<dyn DbBackend> = sqlite.clone();
            let fini = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let lecteur = {
                let db = db.clone();
                let fini = fini.clone();
                std::thread::spawn(move || {
                    let mut pire = Duration::ZERO;
                    while !fini.load(std::sync::atomic::Ordering::Relaxed) {
                        let t = Instant::now();
                        db.query_one_strong("SELECT COUNT(*) FROM zones", &[])
                            .unwrap();
                        pire = pire.max(t.elapsed());
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    pire
                })
            };
            // 40 lots de 500 pistes, comme un scan.
            let debut = Instant::now();
            let mut plus_long_commit = Duration::ZERO;
            for lot in 0..40 {
                db.execute_batch("BEGIN IMMEDIATE").unwrap();
                for i in 0..500 {
                    inserer(db.as_ref(), lot * 500 + i);
                }
                let t = Instant::now();
                db.execute_batch("COMMIT").unwrap();
                plus_long_commit = plus_long_commit.max(t.elapsed());
            }
            let total = debut.elapsed();
            fini.store(true, std::sync::atomic::Ordering::Relaxed);
            let pire_lecture = lecteur.join().unwrap();
            eprintln!(
                "MESURE tour {tour} {nom} : 20000 pistes en {} ms ; plus long COMMIT {} ms ; \
                 pire attente d'une lecture forte {} ms",
                total.as_millis(),
                plus_long_commit.as_millis(),
                pire_lecture.as_millis()
            );
        }
    }
}
