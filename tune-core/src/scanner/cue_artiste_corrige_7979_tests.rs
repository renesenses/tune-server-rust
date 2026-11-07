//! #7979 — fil forum 2664 : le nom de l'artiste corrigé dans la feuille CUE
//! (et les balises) d'un album déjà en bibliothèque.
//!
//! 1. Accents ajoutés (« Bireli Lagrene » → « Biréli Lagrène ») : rien ne
//!    changeait, ni sur les pistes ni sur l'album.
//! 2. Espace ajouté (« BireliLagrene » → « Bireli Lagrene ») : les pistes
//!    prenaient le bon nom, l'album gardait l'ancien.
//!
//! Chaque témoin part d'un album déjà analysé sous l'ancien nom, puis relit la
//! feuille corrigée : sur une base vierge le nom serait bon du premier coup.

use super::tests::{base, ecrire_wav, racines};
use super::*;
use std::fs;

fn feuille(artiste: &str) -> String {
    format!(
        "PERFORMER \"{artiste}\"\nTITLE \"Gipsy Project\"\nFILE \"image.wav\" WAVE\n  TRACK 01 AUDIO\n    TITLE \"Blues Clair\"\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    TITLE \"Swing 42\"\n    INDEX 01 00:01:00\n"
    )
}

/// Le nom d'artiste de l'album du dossier et ceux de ses pistes.
fn noms(db: &Arc<dyn DbBackend>) -> (String, Vec<String>) {
    let album = db
        .query_one(
            "SELECT a.name FROM albums al JOIN artists a ON a.id = al.artist_id",
            &[],
        )
        .unwrap()
        .expect("un album");
    let pistes = db
        .query_many(
            "SELECT DISTINCT a.name FROM tracks t JOIN artists a ON a.id = t.artist_id",
            &[],
        )
        .unwrap()
        .iter()
        .map(|l| l[0].as_string().unwrap())
        .collect();
    (album[0].as_string().unwrap(), pistes)
}

/// Analyse le dossier sous l'ancien nom, corrige la feuille, réanalyse.
fn corriger(ancien: &str, nouveau: &str) -> Arc<dyn DbBackend> {
    let d = tempfile::TempDir::new().unwrap();
    let dossier = d.path().join("Lagrene - Gipsy Project");
    fs::create_dir_all(&dossier).unwrap();
    ecrire_wav(&dossier.join("image.wav"), 4_000);
    let db = base();

    fs::write(dossier.join("album.cue"), feuille(ancien)).unwrap();
    inventorier_et_ecrire(
        db.clone(),
        std::slice::from_ref(&dossier),
        &racines(d.path()),
    );
    assert_eq!(
        noms(&db),
        (ancien.to_string(), vec![ancien.to_string()]),
        "état de départ"
    );

    fs::write(dossier.join("album.cue"), feuille(nouveau)).unwrap();
    inventorier_et_ecrire(db.clone(), &[dossier], &racines(d.path()));
    db
}

#[test]
fn cue_7979_les_accents_ajoutes_gagnent_pistes_et_album() {
    let db = corriger("Bireli Lagrene", "Biréli Lagrène");
    let (album, pistes) = noms(&db);
    assert_eq!(
        pistes,
        vec!["Biréli Lagrène".to_string()],
        "#7979 : les pistes gardent le nom sans accents"
    );
    assert_eq!(
        album, "Biréli Lagrène",
        "#7979 : l'album garde le nom sans accents"
    );
}

#[test]
fn cue_7979_l_espace_ajoute_gagne_aussi_l_album() {
    let db = corriger("BireliLagrene", "Bireli Lagrene");
    let (album, pistes) = noms(&db);
    assert_eq!(pistes, vec!["Bireli Lagrene".to_string()]);
    assert_eq!(
        album, "Bireli Lagrene",
        "#7979 : l'album garde l'artiste de sa première analyse"
    );
    // L'ancienne fiche n'est plus portée par rien : le ménage de fin
    // d'analyse la retire.
    ArtistRepo::with_backend(db.clone())
        .cleanup_orphans()
        .unwrap();
    let restants = db
        .query_many("SELECT name FROM artists ORDER BY name", &[])
        .unwrap()
        .iter()
        .map(|l| l[0].as_string().unwrap())
        .collect::<Vec<_>>();
    assert!(
        !restants.iter().any(|n| n == "BireliLagrene"),
        "l'ancienne fiche survit : {restants:?}"
    );
}

/// Une feuille qui ne nomme plus personne ne change pas l'artiste de
/// l'album : seul un vrai `PERFORMER` réaligne, jamais le repli « Unknown
/// Artist » (#6459) ni l'artiste que la lecture des balises devine d'après
/// le nom du dossier.
#[test]
fn cue_7979_une_feuille_sans_interprete_ne_touche_pas_l_album() {
    let d = tempfile::TempDir::new().unwrap();
    let dossier = d.path().join("Lagrene - Gipsy Project");
    fs::create_dir_all(&dossier).unwrap();
    ecrire_wav(&dossier.join("image.wav"), 4_000);
    let db = base();
    fs::write(dossier.join("album.cue"), feuille("Biréli Lagrène")).unwrap();
    inventorier_et_ecrire(
        db.clone(),
        std::slice::from_ref(&dossier),
        &racines(d.path()),
    );
    let sans_interprete = feuille("Biréli Lagrène").replace("PERFORMER \"Biréli Lagrène\"\n", "");
    fs::write(dossier.join("album.cue"), sans_interprete).unwrap();
    inventorier_et_ecrire(db.clone(), &[dossier], &racines(d.path()));
    assert_eq!(noms(&db).0, "Biréli Lagrène");
}
