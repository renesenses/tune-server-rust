//! #5685 (Marco Polo, fil 2118) — coffret Elgar de 30 CD, un dossier par
//! disque : le `cover.jpg` posé à la racine du coffret n'était jamais lu, la
//! vignette montrait l'image du CD 1.
//!
//! Décision de Bertrand du 03/10/2026 : « l'image du dossier d'abord ». Une
//! image posée dans le dossier — celui du disque, ou celui qui réunit les
//! disques — passe avant la jaquette intégrée ; une pochette téléversée n'est
//! jamais touchée.
//!
//! Chaque cas part d'un album DÉJÀ en base, illustré comme l'ancienne règle
//! l'illustrait, dont aucun fichier ne bouge : c'est la passe de fin de scan
//! (`suivre_les_fichiers_sources`, jouée par tous les scans) qui doit lui
//! faire prendre la bonne image.
use super::*;
use crate::db::models::{Album, Track};
use crate::db::track_repo::TrackRepo;
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::flac::FlacFile;
use lofty::ogg::OggPictureStorage;
use lofty::picture::{MimeType, Picture, PictureInformation, PictureType};
use std::sync::Arc;

const JAQUETTE: &[u8] = b"\xFF\xD8\xFF\xE0JAQUETTE-DU-CD1-5685";
const COFFRET: &[u8] = b"\xFF\xD8\xFF\xE0COVER-JPG-DU-COFFRET-5685";
const DISQUE: &[u8] = b"\xFF\xD8\xFF\xE0COVER-JPG-DU-DISQUE-5685";
const ARTISTE: &[u8] = b"\xFF\xD8\xFF\xE0FOLDER-JPG-DE-L-ARTISTE-5685";
const TELEVERSEE: &[u8] = b"\xFF\xD8\xFF\xE0TELEVERSEE-A-LA-MAIN-5685";

fn base() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Un vrai FLAC, avec (ou sans) jaquette intégrée.
fn flac(chemin: &Path, jaquette: Option<&[u8]>) {
    std::fs::create_dir_all(chemin.parent().unwrap()).unwrap();
    let gabarit = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test.flac");
    std::fs::copy(gabarit, chemin).unwrap();
    let mut f = std::fs::File::open(chemin).unwrap();
    let mut fl = FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
    drop(f);
    while !fl.pictures().is_empty() {
        fl.remove_picture(0);
    }
    if let Some(o) = jaquette {
        let pic = Picture::unchecked(o.to_vec())
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::Jpeg)
            .build();
        fl.insert_picture(pic, Some(PictureInformation::default()))
            .unwrap();
    }
    fl.save_to_path(chemin, WriteOptions::default()).unwrap();
}

/// Un album en base, ses pistes rangées dans l'ordre (disque, numéro).
fn album(db: &Arc<dyn DbBackend>, titre: &str, pistes: &[(&Path, i32)]) -> i64 {
    let aid = AlbumRepo::with_backend(db.clone())
        .create(&Album::new(titre.into()))
        .unwrap();
    for (i, (p, disque)) in pistes.iter().enumerate() {
        let mut t = Track::new(format!("{titre} {i}"));
        t.file_path = Some(p.to_string_lossy().into_owned());
        t.album_id = Some(aid);
        t.disc_number = *disque;
        t.track_number = i as i32 + 1;
        TrackRepo::with_backend(db.clone()).create(&t).unwrap();
    }
    aid
}

/// L'album illustré comme l'ancienne règle l'illustrait : la jaquette
/// intégrée de sa première piste.
fn illustre_par_la_jaquette(db: &Arc<dyn DbBackend>, aid: i64, piste: &Path) {
    AlbumRepo::with_backend(db.clone())
        .poser_pochette_du_disque(
            aid,
            &content_hash(JAQUETTE),
            SourcePochette::Integree,
            &piste.to_string_lossy(),
            empreinte_du_fichier(piste).as_deref(),
        )
        .unwrap();
}

fn pochette(db: &Arc<dyn DbBackend>, aid: i64) -> (Option<String>, Option<SourcePochette>) {
    let e = AlbumRepo::with_backend(db.clone())
        .etat_pochette(aid)
        .unwrap()
        .unwrap();
    (e.cover_path, e.source)
}

/// `Coffret/CD1`, `Coffret/CD2`, chaque piste avec la jaquette du CD 1 (le
/// cas le plus courant chez Marco : l'image affichée était celle du CD 1).
fn coffret(racine: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let coffret = racine.join("Elgar - Collector Edition");
    let cd1 = coffret.join("CD1").join("01.flac");
    let cd2 = coffret.join("CD2").join("01.flac");
    flac(&cd1, Some(JAQUETTE));
    flac(&cd2, Some(JAQUETTE));
    (coffret, cd1, cd2)
}

#[test]
fn le_cover_jpg_a_la_racine_du_coffret_gagne_5685() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let (coffret, cd1, cd2) = coffret(&dir.path().join("musique"));
    std::fs::write(coffret.join("cover.jpg"), COFFRET).unwrap();
    // Le dossier du CD 1 porte aussi sa propre image : la racine passe avant.
    std::fs::write(cd1.parent().unwrap().join("folder.jpg"), DISQUE).unwrap();
    let db = base();
    let aid = album(&db, "Elgar", &[(&cd1, 1), (&cd2, 2)]);
    illustre_par_la_jaquette(&db, aid, &cd1);

    let reprises = suivre_les_fichiers_sources(&db, &cache, &[], false);

    assert_eq!(
        pochette(&db, aid),
        (Some(content_hash(COFFRET)), Some(SourcePochette::Dossier)),
        "le cover.jpg du coffret doit illustrer le coffret"
    );
    assert_eq!(reprises, 1);
    // Et la passe suivante n'y revient pas : l'image en place est la bonne.
    assert_eq!(suivre_les_fichiers_sources(&db, &cache, &[], false), 0);
}

#[test]
fn la_reprise_par_album_lit_aussi_la_racine_du_coffret_5685() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let (coffret, cd1, cd2) = coffret(&dir.path().join("musique"));
    std::fs::write(coffret.join("Cover.jpg"), COFFRET).unwrap();
    let db = base();
    let aid = album(&db, "Elgar", &[(&cd1, 1), (&cd2, 2)]);
    // « Rescanner la pochette » de l'album (route `/artwork/rescan`).
    assert!(matches!(
        reevaluer_l_album(&db, aid, &cache, true, None),
        Geste::Poser(_)
    ));
    assert_eq!(pochette(&db, aid).0, Some(content_hash(COFFRET)));
}

#[test]
fn l_image_du_dossier_du_disque_passe_avant_la_jaquette_5685() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let dossier = dir.path().join("musique").join("Album");
    let p1 = dossier.join("01.flac");
    let p2 = dossier.join("02.flac");
    flac(&p1, Some(JAQUETTE));
    flac(&p2, Some(JAQUETTE));
    std::fs::write(dossier.join("cover.jpg"), DISQUE).unwrap();
    let db = base();

    // Un album déjà en base, illustré par la jaquette : la passe de fin de
    // scan lui fait prendre l'image du dossier.
    let aid = album(&db, "Album", &[(&p1, 1), (&p2, 1)]);
    illustre_par_la_jaquette(&db, aid, &p1);
    suivre_les_fichiers_sources(&db, &cache, &[], false);
    assert_eq!(
        pochette(&db, aid),
        (Some(content_hash(DISQUE)), Some(SourcePochette::Dossier))
    );

    // Un album neuf : la première piste lue par le scan pose l'image du
    // dossier, pas sa jaquette.
    let autre = dir.path().join("musique").join("Neuf");
    let p3 = autre.join("01.flac");
    flac(&p3, Some(JAQUETTE));
    std::fs::write(autre.join("cover.jpg"), DISQUE).unwrap();
    let neuf = album(&db, "Neuf", &[(&p3, 1)]);
    let octets = (JAQUETTE.to_vec(), "image/jpeg".to_string());
    let suivi = suivre_la_piste(&db, neuf, &p3, Jaquette::Lue(&octets), &cache, false);
    assert!(suivi.posee && suivi.tranche);
    assert_eq!(pochette(&db, neuf).0, Some(content_hash(DISQUE)));
}

#[test]
fn la_jaquette_seule_ne_change_pas_5685() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let (_coffret, cd1, cd2) = coffret(&dir.path().join("musique"));
    let db = base();
    let aid = album(&db, "Elgar", &[(&cd1, 1), (&cd2, 2)]);
    illustre_par_la_jaquette(&db, aid, &cd1);

    assert_eq!(suivre_les_fichiers_sources(&db, &cache, &[], false), 0);
    assert_eq!(
        pochette(&db, aid),
        (Some(content_hash(JAQUETTE)), Some(SourcePochette::Integree))
    );
    // Relu en entier : toujours la jaquette.
    reevaluer_l_album(&db, aid, &cache, true, None);
    assert_eq!(pochette(&db, aid).0, Some(content_hash(JAQUETTE)));
}

#[test]
fn une_pochette_televersee_n_est_jamais_ecrasee_5685() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let (coffret, cd1, cd2) = coffret(&dir.path().join("musique"));
    std::fs::write(coffret.join("cover.jpg"), COFFRET).unwrap();
    std::fs::write(cd1.parent().unwrap().join("cover.jpg"), DISQUE).unwrap();
    let db = base();
    let aid = album(&db, "Elgar", &[(&cd1, 1), (&cd2, 2)]);
    AlbumRepo::with_backend(db.clone())
        .force_update_cover_path(aid, &content_hash(TELEVERSEE), SourcePochette::Televersee)
        .unwrap();
    let attendu = (
        Some(content_hash(TELEVERSEE)),
        Some(SourcePochette::Televersee),
    );

    suivre_les_fichiers_sources(&db, &cache, &[], true);
    assert_eq!(pochette(&db, aid), attendu, "fin de scan");
    reevaluer_l_album(&db, aid, &cache, true, None);
    assert_eq!(pochette(&db, aid), attendu, "reprise par album");
    trancher_par_la_majorite(&db, aid, &cache, true);
    assert_eq!(pochette(&db, aid), attendu, "majorité");
    let octets = (JAQUETTE.to_vec(), "image/jpeg".to_string());
    suivre_la_piste(&db, aid, &cd1, Jaquette::Lue(&octets), &cache, true);
    assert_eq!(pochette(&db, aid), attendu, "piste relue");
}

/// Le dossier d'un ARTISTE n'est pas celui d'un coffret : son `folder.jpg`
/// est sa photo. Un album éclaté en deux dossiers d'artiste ne la prend pas
/// dès qu'un autre album vit sous le même dossier.
#[test]
fn le_dossier_commun_n_est_retenu_que_s_il_n_abrite_que_l_album_5685() {
    let dir = tempfile::tempdir().unwrap();
    let cache = dir.path().join("cache");
    let artiste = dir.path().join("musique").join("Elgar");
    let a = artiste.join("Symphonies").join("01.flac");
    let b = artiste.join("Symphonies (bonus)").join("01.flac");
    let autre = artiste.join("Enigma").join("01.flac");
    flac(&a, Some(JAQUETTE));
    flac(&b, Some(JAQUETTE));
    flac(&autre, None);
    std::fs::write(artiste.join("folder.jpg"), ARTISTE).unwrap();
    let db = base();
    let aid = album(&db, "Symphonies", &[(&a, 1), (&b, 2)]);
    album(&db, "Enigma", &[(&autre, 1)]);
    illustre_par_la_jaquette(&db, aid, &a);

    assert_eq!(suivre_les_fichiers_sources(&db, &cache, &[], false), 0);
    assert_eq!(pochette(&db, aid).0, Some(content_hash(JAQUETTE)));
}

/// La remontée est bornée : un, deux niveaux au-dessus des dossiers de
/// disques, jamais la racine du système de fichiers, jamais pour un album
/// d'un seul dossier.
#[test]
fn le_dossier_commun_est_borne_5685() {
    let p = |s: &str| PathBuf::from(s);
    assert_eq!(
        dossier_commun(&[p("/m/Coffret/CD1/01.flac"), p("/m/Coffret/CD2/01.flac")]),
        Some(p("/m/Coffret"))
    );
    assert_eq!(
        dossier_commun(&[
            p("/m/Coffret/CD1/FLAC/01.flac"),
            p("/m/Coffret/CD2/FLAC/01.flac")
        ]),
        Some(p("/m/Coffret")),
        "deux niveaux"
    );
    assert_eq!(
        dossier_commun(&[
            p("/m/Coffret/A/CD1/FLAC/01.flac"),
            p("/m/Coffret/B/CD2/FLAC/01.flac")
        ]),
        None,
        "trois niveaux : trop haut"
    );
    assert_eq!(
        dossier_commun(&[p("/m/Album/01.flac"), p("/m/Album/02.flac")]),
        None,
        "un seul dossier : l'image de ce dossier suffit"
    );
    assert_eq!(
        dossier_commun(&[p("/CD1/01.flac"), p("/CD2/01.flac")]),
        None,
        "jamais la racine du système de fichiers"
    );
    assert_eq!(
        dossier_commun(&[p("/m/Coffret/01.flac"), p("/m/Coffret 2/01.flac")]),
        Some(p("/m")),
        "comparaison par composants"
    );
}
