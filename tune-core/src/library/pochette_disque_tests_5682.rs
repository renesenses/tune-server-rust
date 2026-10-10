//! #5682 (fil 2115) — une pochette tirée du disque n'est pas retirée quand
//! c'est le SUPPORT qui manque (NAS pas encore monté), seulement quand son
//! fichier a quitté un disque joignable (#5034). Les vraies passes sont
//! éprouvées dans `tune-server/src/pochettes_nas_absent_tests_5682.rs`.
use super::*;
use crate::db::models::{Album, Track};
use std::sync::Arc;

fn base() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Un album d'une piste, sa pochette tirée du `cover.jpg` posé à côté.
fn album_au_cover(db: &Arc<dyn DbBackend>, dossier: &Path) -> i64 {
    std::fs::create_dir_all(dossier).unwrap();
    let piste = dossier.join("01.flac");
    std::fs::write(&piste, b"pas un vrai flac").unwrap();
    let cover = dossier.join("cover.jpg");
    std::fs::write(&cover, b"\xFF\xD8\xFF\xE0COVER-5682").unwrap();
    let repo = AlbumRepo::with_backend(db.clone());
    let album = repo.create(&Album::new("Album du NAS".into())).unwrap();
    let mut t = Track::new("Un".into());
    t.album_id = Some(album);
    t.format = Some("flac".into());
    t.duration_ms = 200_000;
    t.file_path = Some(piste.to_string_lossy().into_owned());
    crate::db::track_repo::TrackRepo::with_backend(db.clone())
        .create(&t)
        .unwrap();
    repo.poser_pochette_du_disque(
        album,
        "cd5682",
        SourcePochette::Dossier,
        &cover.to_string_lossy(),
        empreinte_du_fichier(&cover).as_deref(),
    )
    .unwrap();
    album
}

fn pochette(db: &Arc<dyn DbBackend>, album: i64) -> Option<String> {
    AlbumRepo::with_backend(db.clone())
        .etat_pochette(album)
        .unwrap()
        .unwrap()
        .cover_path
}

#[test]
fn aucune_piste_joignable_la_pochette_est_gardee_5682() {
    let db = base();
    let disque = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let dossier = disque.path().join("Musique").join("Album");
    let album = album_au_cover(&db, &dossier);

    // Le partage n'est pas monté : tout l'arbre a disparu.
    let ailleurs = disque.path().join("hors-montage");
    std::fs::rename(disque.path().join("Musique"), &ailleurs).unwrap();
    assert_eq!(
        reevaluer_l_album(&db, album, cache.path(), false, None),
        Geste::Garder,
        "aucune piste joignable : c'est le support qui manque, pas la pochette"
    );
    assert_eq!(
        suivre_les_fichiers_sources(&db, cache.path(), &[], &[], false),
        0
    );
    assert_eq!(pochette(&db, album).as_deref(), Some("cd5682"));

    // Contre-épreuve (#5034) : le support est revenu, `cover.jpg` a été
    // supprimé par l'utilisateur — la pochette suit son fichier.
    std::fs::rename(&ailleurs, disque.path().join("Musique")).unwrap();
    std::fs::remove_file(dossier.join("cover.jpg")).unwrap();
    assert_eq!(
        suivre_les_fichiers_sources(&db, cache.path(), &[], &[], false),
        1
    );
    assert_eq!(pochette(&db, album), None, "#5034 : retirée");
}

#[test]
fn un_dossier_en_erreur_de_parcours_n_est_pas_juge_5682() {
    let db = base();
    let disque = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let dossier = disque.path().join("Album");
    let album = album_au_cover(&db, &dossier);
    std::fs::remove_file(dossier.join("cover.jpg")).unwrap();
    let exclus = [dossier.to_string_lossy().into_owned()];
    assert_eq!(
        suivre_les_fichiers_sources(&db, cache.path(), &[], &exclus, false),
        0,
        "un dossier que le parcours n'a pas pu lire n'a pas été vu"
    );
    assert_eq!(pochette(&db, album).as_deref(), Some("cd5682"));
}

#[test]
fn le_suivi_ne_conclut_que_sur_des_racines_qui_ont_repondu_5682() {
    let r = vec!["/mnt/192.168.1.5_Musique".to_string()];
    assert!(le_suivi_peut_conclure(false, &[], &[]));
    assert!(!le_suivi_peut_conclure(true, &[], &[]), "scan annulé");
    assert!(!le_suivi_peut_conclure(false, &r, &[]), "racine absente");
    assert!(!le_suivi_peut_conclure(false, &[], &r), "racine vidée");
}
