//! #7694 — la règle des dossiers miroirs (#4907) ne rattache plus au miroir
//! qu'un fichier de même qualité. Témoins au niveau de
//! [`AlbumRepo::get_or_create_for_folder_de_qualite`] ; le trajet complet du
//! scan est joué dans `tune-server` (`scan_import_editions_miroirs_tests_7694`).
use std::sync::Arc;

use super::album_repo::{AlbumRepo, QualiteDuFichier};
use super::artist_repo::ArtistRepo;
use super::backend::{DbBackend, ToSqlValue};
use super::migrations::run_migrations;
use super::sqlite::SqliteDb;
use crate::library::quality::score_qualite;

const DSD: &str = "/m/DSD/Art Blakey/The African Beat";
const CD: &str = "/m/CD/Art Blakey/The African Beat";
const TITRE: &str = "The African Beat";

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let json = serde_json::to_string(&["/m/DSD", "/m/CD"]).unwrap();
    let params: [&dyn ToSqlValue; 1] = [&json];
    db.execute(
        "INSERT INTO settings (key, value, updated_at) VALUES ('music_dirs', ?, '') \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        &params,
    )
    .unwrap();
    db
}

fn piste(db: &dyn DbBackend, album: i64, chemin: &str, format: &str, sr: i32, bd: i32) {
    let params: [&dyn ToSqlValue; 5] = [&album, &chemin, &format, &sr, &bd];
    db.execute(
        "INSERT INTO tracks (title, album_id, disc_number, track_number, \
         file_path, format, sample_rate, bit_depth, duration_ms) \
         VALUES ('Titre', ?, 1, 1, ?, ?, ?, ?, 1000)",
        &params,
    )
    .unwrap();
}

fn rien_vu(_: &str) -> Vec<(bool, i64)> {
    Vec::new()
}

fn qualite(format: &str, sr: i64, bd: i64) -> QualiteDuFichier<'static> {
    QualiteDuFichier {
        score: score_qualite(Some(format), Some(sr), Some(bd)),
        vues_dans_le_scan: &rien_vu,
    }
}

fn resoudre(repo: &AlbumRepo, dossier: &str, artiste: i64, q: Option<QualiteDuFichier<'_>>) -> i64 {
    repo.get_or_create_for_folder_de_qualite(
        dossier,
        TITRE,
        artiste,
        Some(1962),
        None,
        Some(1),
        Some(1),
        q,
    )
    .unwrap()
    .id
    .unwrap()
}

/// L'édition CD rangée au miroir d'une édition DSD déjà en base : un autre
/// album.
#[test]
fn le_miroir_d_une_autre_qualite_est_une_autre_edition() {
    let db = base();
    let repo = AlbumRepo::with_backend(db.clone());
    let artiste = ArtistRepo::with_backend(db.clone())
        .get_or_create("Art Blakey", None, None)
        .unwrap()
        .id
        .unwrap();

    let a_dsd = resoudre(&repo, DSD, artiste, Some(qualite("dsf", 2_822_400, 1)));
    piste(&*db, a_dsd, &format!("{DSD}/01.dsf"), "dsf", 2_822_400, 1);

    let a_cd = resoudre(&repo, CD, artiste, Some(qualite("flac", 44_100, 16)));
    assert_ne!(a_dsd, a_cd, "l'édition CD a été rattachée au miroir DSD");
}

/// Une bibliothèque déjà réunie à tort (pistes CD rangées dans l'album DSD)
/// se sépare : seule la qualité des pistes SOUS le dossier miroir compte.
#[test]
fn un_album_deja_reuni_a_tort_se_separe_au_scan_suivant() {
    let db = base();
    let repo = AlbumRepo::with_backend(db.clone());
    let artiste = ArtistRepo::with_backend(db.clone())
        .get_or_create("Art Blakey", None, None)
        .unwrap()
        .id
        .unwrap();

    let a_dsd = resoudre(&repo, DSD, artiste, None);
    piste(&*db, a_dsd, &format!("{DSD}/01.dsf"), "dsf", 2_822_400, 1);
    piste(&*db, a_dsd, &format!("{CD}/01.flac"), "flac", 44_100, 16);

    let a_cd = resoudre(&repo, CD, artiste, Some(qualite("flac", 44_100, 16)));
    assert_ne!(
        a_dsd, a_cd,
        "les pistes CD déjà dans l'album DSD l'ont gardé réuni"
    );
}

/// Non-régression #4907 : même qualité, ou qualité du fichier inconnue, ou
/// rien de connu sous le miroir : le miroir est le même album.
#[test]
fn le_miroir_de_meme_qualite_reste_le_meme_album_4907() {
    let db = base();
    let repo = AlbumRepo::with_backend(db.clone());
    let artiste = ArtistRepo::with_backend(db.clone())
        .get_or_create("Art Blakey", None, None)
        .unwrap()
        .id
        .unwrap();

    let a = resoudre(&repo, DSD, artiste, None);
    assert_eq!(
        a,
        resoudre(&repo, CD, artiste, Some(qualite("flac", 44_100, 16))),
        "rien de connu sous le miroir : la règle #4907 joue"
    );

    let db = base();
    let repo = AlbumRepo::with_backend(db.clone());
    let artiste = ArtistRepo::with_backend(db.clone())
        .get_or_create("Art Blakey", None, None)
        .unwrap()
        .id
        .unwrap();
    let a = resoudre(&repo, DSD, artiste, None);
    piste(&*db, a, &format!("{DSD}/01.flac"), "flac", 44_100, 16);
    assert_eq!(
        a,
        resoudre(&repo, CD, artiste, Some(qualite("flac", 44_100, 16)))
    );

    let db = base();
    let repo = AlbumRepo::with_backend(db.clone());
    let artiste = ArtistRepo::with_backend(db.clone())
        .get_or_create("Art Blakey", None, None)
        .unwrap()
        .id
        .unwrap();
    let a = resoudre(&repo, DSD, artiste, None);
    piste(&*db, a, &format!("{DSD}/01.dsf"), "dsf", 2_822_400, 1);
    assert_eq!(
        a,
        resoudre(&repo, CD, artiste, None),
        "qualité inconnue : comme avant"
    );
}
