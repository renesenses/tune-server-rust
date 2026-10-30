//! #6077 (régis, forum-hifi 41831 #770) — « il manque pas mal d'illustrations
//! d'albums alors qu'elles apparaissent dans Audirvana ».
//!
//! Décision de Bertrand du 30/10/2026 : en DERNIER recours seulement (ni
//! jaquette intégrée, ni image de dossier reconnue, ni pochette en ligne), la
//! plus grande image du dossier de l'album devient sa pochette ; les images
//! minuscules sont ignorées.
use super::*;
use crate::db::models::{Album, Track};
use crate::db::track_repo::TrackRepo;
use crate::library::artwork::{COTE_MINIMAL_DERNIER_RECOURS, plus_grande_image_des_dossiers};
use std::sync::Arc;

fn base() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Une vraie image `largeur` × `hauteur`, encodée selon l'extension.
fn image(chemin: &Path, largeur: u32, hauteur: u32) {
    std::fs::create_dir_all(chemin.parent().unwrap()).unwrap();
    image::RgbImage::from_pixel(largeur, hauteur, image::Rgb([120, 30, 200]))
        .save(chemin)
        .unwrap();
}

fn album_avec_une_piste(db: &Arc<dyn DbBackend>, piste: &Path) -> i64 {
    let aid = AlbumRepo::with_backend(db.clone())
        .create(&Album::new("Album de régis".into()))
        .unwrap();
    std::fs::create_dir_all(piste.parent().unwrap()).unwrap();
    std::fs::write(piste, b"fLaC").unwrap();
    let mut t = Track::new("Piste 1".into());
    t.file_path = Some(piste.to_string_lossy().into_owned());
    t.album_id = Some(aid);
    t.track_number = 1;
    TrackRepo::with_backend(db.clone()).create(&t).unwrap();
    aid
}

/// Un dossier « à la Audirvana » : aucun nom reconnu, une vignette, une photo
/// d'artiste plus grande que tout, un fichier AppleDouble.
fn dossier_sans_nom_reconnu(dossier: &Path) {
    image(&dossier.join("vignette.png"), 120, 120);
    image(&dossier.join("Livret page 2.jpg"), 500, 500);
    image(&dossier.join("Ma Pochette Scannee.png"), 900, 900);
    image(&dossier.join("artist.jpg"), 1500, 1500);
    std::fs::write(dossier.join("._Ma Pochette Scannee.png"), b"\0\x05\x16\x07").unwrap();
    std::fs::write(dossier.join("notes.txt"), b"rien").unwrap();
}

#[test]
fn la_plus_grande_image_est_retenue_6077() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("Album");
    dossier_sans_nom_reconnu(&d);
    // Aucune image de dossier n'est reconnue par son nom : c'est bien le cas
    // que les sources précédentes laissent sans pochette.
    assert!(crate::library::artwork::image_de_pochette_dans(&d).is_none());
    assert_eq!(
        plus_grande_image_des_dossiers(&[d.as_path()]),
        Some(d.join("Ma Pochette Scannee.png")),
        "la plus grande image (hors photo d'artiste et fichier caché) doit être retenue"
    );
}

#[test]
fn une_image_minuscule_n_est_jamais_une_pochette_6077() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("Album");
    let petit = COTE_MINIMAL_DERNIER_RECOURS - 1;
    image(&d.join("logo.png"), 1200, petit);
    image(&d.join("icone.jpg"), petit, petit);
    assert_eq!(plus_grande_image_des_dossiers(&[d.as_path()]), None);
}

#[test]
fn l_album_sans_pochette_prend_la_plus_grande_image_6077() {
    let db = base();
    let tmp = tempfile::tempdir().unwrap();
    let cache = tmp.path().join("cache");
    let d = tmp.path().join("musique/Album");
    let aid = album_avec_une_piste(&db, &d.join("01.flac"));
    dossier_sans_nom_reconnu(&d);

    assert!(poser_pochette_de_dernier_recours(&db, aid, &cache));
    let etat = AlbumRepo::with_backend(db.clone())
        .etat_pochette(aid)
        .unwrap()
        .unwrap();
    let attendue = std::fs::read(d.join("Ma Pochette Scannee.png")).unwrap();
    assert_eq!(
        etat.cover_path.as_deref(),
        Some(content_hash(&attendue).as_str())
    );
    assert_eq!(etat.source, Some(SourcePochette::Dossier));
    assert_eq!(
        etat.fichier.as_deref(),
        Some(d.join("Ma Pochette Scannee.png").to_string_lossy().as_ref()),
        "le fichier source est écrit : la pochette suit le disque (#5034)"
    );
    assert!(find_cached(&cache, etat.cover_path.as_deref().unwrap()).is_some());
}

#[test]
fn un_album_deja_illustre_n_est_pas_touche_6077() {
    let db = base();
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path().join("musique/Album");
    let aid = album_avec_une_piste(&db, &d.join("01.flac"));
    dossier_sans_nom_reconnu(&d);
    let repo = AlbumRepo::with_backend(db.clone());
    repo.update_cover_path(aid, "pochette-en-ligne", SourcePochette::Fournisseur)
        .unwrap();

    assert!(!poser_pochette_de_dernier_recours(
        &db,
        aid,
        &tmp.path().join("cache")
    ));
    assert_eq!(
        pochettes_de_dernier_recours(&db, &tmp.path().join("cache")),
        0
    );
    let etat = repo.etat_pochette(aid).unwrap().unwrap();
    assert_eq!(etat.cover_path.as_deref(), Some("pochette-en-ligne"));
    assert_eq!(etat.source, Some(SourcePochette::Fournisseur));
}
