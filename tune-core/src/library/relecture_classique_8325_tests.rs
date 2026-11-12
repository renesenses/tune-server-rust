//! #8325 — la relecture unique des balises classiques.

use super::*;
use crate::db::sqlite::SqliteDb;

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    db.execute_batch(
        "INSERT INTO tracks (id, title, file_path) VALUES \
           (1, 'A', '/m/a.flac'), (2, 'B', '/m/b.flac'), (3, 'C', '/m/c.flac'), \
           (4, 'Radio', NULL); \
         INSERT INTO track_metadata (track_id, key, value) VALUES (3, 'work', 'Œuvre de l''IA');",
    )
    .unwrap();
    Arc::new(db)
}

/// Le fichier A porte une œuvre et un ensemble, B un mouvement, C rien.
fn fichiers(p: &Path) -> Vec<(&'static str, String)> {
    match p.to_str().unwrap() {
        "/m/a.flac" => vec![
            ("work", "Requiem".into()),
            ("ensemble", "Wiener Philharmoniker".into()),
        ],
        "/m/b.flac" => vec![("movement", "Dies irae".into())],
        _ => vec![],
    }
}

fn cle(b: &Arc<dyn DbBackend>, piste: i64, cle: &str) -> Option<String> {
    TrackMetadataRepo::with_backend(b.clone())
        .get_all(piste)
        .unwrap()
        .get(cle)
        .cloned()
}

#[test]
fn la_passe_lit_par_lots_reprend_au_curseur_et_ne_tourne_qu_une_fois_8325() {
    let b = base();
    assert!(!est_faite(&b));
    assert_eq!(restantes(&b), 3, "la radio sans fichier n'est pas lue");

    let l = traiter_lot_suivant(&b, 2, fichiers).unwrap();
    assert_eq!((l.lues, l.ecrites, l.fini), (2, 2, false));
    assert_eq!(cle(&b, 1, "work").as_deref(), Some("Requiem"));
    assert_eq!(
        cle(&b, 1, "ensemble").as_deref(),
        Some("Wiener Philharmoniker")
    );
    assert_eq!(cle(&b, 2, "movement").as_deref(), Some("Dies irae"));
    assert_eq!(restantes(&b), 1, "le curseur a avancé");

    // Un « redémarrage » : la passe repart au curseur, pas au début.
    let l = traiter_lot_suivant(&b, 2, |p| {
        assert_eq!(p, Path::new("/m/c.flac"), "relu une piste déjà traitée");
        fichiers(p)
    })
    .unwrap();
    assert_eq!((l.lues, l.ecrites, l.fini), (1, 0, false));
    assert_eq!(
        cle(&b, 3, "work").as_deref(),
        Some("Œuvre de l'IA"),
        "un fichier muet n'efface pas l'œuvre posée par l'assistant"
    );

    let l = traiter_lot_suivant(&b, 2, fichiers).unwrap();
    assert!(l.fini && est_faite(&b));

    // Une fois close, plus aucune lecture, même si une piste arrive.
    b.execute(
        "INSERT INTO tracks (id, title, file_path) VALUES (9, 'Neuve', '/m/n.flac')",
        &[],
    )
    .unwrap();
    let l = traiter_lot_suivant(&b, 2, |_| panic!("la passe close relit un fichier")).unwrap();
    assert_eq!((l.lues, l.fini), (0, true));
}
