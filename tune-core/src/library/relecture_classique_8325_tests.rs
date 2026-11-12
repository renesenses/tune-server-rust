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
fn fichiers(p: &Path) -> Option<Lecture> {
    Some(match p.to_str().unwrap() {
        "/m/a.flac" => Lecture::de(&[("work", "Requiem"), ("ensemble", "Wiener Philharmoniker")]),
        "/m/b.flac" => Lecture::de(&[("movement", "Dies irae")]),
        _ => Lecture::default(),
    })
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

/// ESL63P — après une modification du réglage « Balises du classique »
/// (BAND ignoré), la passe repart de zéro et retire l'ensemble lu sous BAND,
/// mais jamais une valeur que le fichier ne porte pas (assistant IA).
#[test]
fn la_relecture_apres_reglage_retire_ce_que_le_reglage_ne_lit_plus() {
    let b = base();
    b.execute_batch(
        "INSERT INTO track_metadata (track_id, key, value) VALUES \
           (1, 'ensemble', 'Berliner Philharmoniker'), (2, 'ensemble', 'Chœur de la communauté');",
    )
    .unwrap();
    // La première passe est faite.
    while !traiter_lot_suivant(&b, 10, |_| Some(Lecture::default()))
        .unwrap()
        .fini
    {}
    assert!(est_faite(&b));

    reinitialiser(&b, &["BAND".to_string()]).unwrap();
    assert!(!est_faite(&b));
    assert_eq!(restantes(&b), 3, "le curseur repart de zéro");

    let lecture = |p: &Path| -> Option<Lecture> {
        let mut l = Lecture::default();
        if p == Path::new("/m/a.flac") {
            l.brutes = vec![("BAND".into(), "Berliner Philharmoniker".into())];
            l.meta.insert("conductor".into(), "Karajan".into());
        }
        Some(l)
    };
    while !traiter_lot_suivant(&b, 10, lecture).unwrap().fini {}
    assert_eq!(
        cle(&b, 1, "ensemble"),
        None,
        "BAND ignoré : l'ensemble lu sous BAND part"
    );
    assert_eq!(cle(&b, 1, "conductor").as_deref(), Some("Karajan"));
    assert_eq!(
        cle(&b, 2, "ensemble").as_deref(),
        Some("Chœur de la communauté"),
        "une valeur que le fichier ne porte pas reste"
    );
    assert_eq!(cle(&b, 3, "work").as_deref(), Some("Œuvre de l'IA"));
    let chefs = b
        .query_many(
            "SELECT artist_name FROM track_credits WHERE track_id = 1 AND role = 'conductor' AND source = 'balises'",
            &[],
        )
        .unwrap();
    assert_eq!(chefs.len(), 1, "les crédits balises sont réécrits");
    let settings = SettingsRepo::with_backend(b.clone());
    assert_eq!(
        settings.get(CLE_RECONFIGURATION).unwrap(),
        None,
        "close, la reconfiguration s'efface"
    );
}

/// Un fichier illisible (partage absent) n'efface rien.
#[test]
fn un_fichier_illisible_n_efface_rien_pendant_la_relecture() {
    let b = base();
    b.execute_batch(
        "INSERT INTO track_metadata (track_id, key, value) VALUES (1, 'ensemble', 'X');",
    )
    .unwrap();
    reinitialiser(&b, &["BAND".to_string()]).unwrap();
    while !traiter_lot_suivant(&b, 10, |_| None).unwrap().fini {}
    assert_eq!(cle(&b, 1, "ensemble").as_deref(), Some("X"));
}
