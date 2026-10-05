//! #5682 (fil 2115, Tune OS, NAS en SMB) — à chaque mise en service, le NAS
//! arrivait après le scan de démarrage. Les pistes étaient CONSERVÉES
//! (`auto_scan_root_went_empty`), mais la passe de #5034 « la pochette suit
//! son fichier » voyait chaque fichier source « disparu » et retirait la
//! pochette de presque tous les albums ; seule une nouvelle analyse les
//! rendait (`backfill_embedded_covers_done filled=104` sur 145 albums).
//!
//! Joué sur de VRAIS FLAC, par les VRAIES passes (scan de démarrage, scan
//! manuel), la racine rendue injoignable de deux façons : point de montage
//! VIDE (le partage n'est pas monté dessus) et racine ABSENTE.

use super::pochettes_disque_tests_5034::{
    COVER, JAQUETTE, album_dans, etat_sur, racine, scan_de_demarrage, scan_manuel,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::backend::DbBackend;
use tune_core::db::track_repo::TrackRepo;
use tune_core::library::artwork::content_hash;

fn pochettes(db: &Arc<dyn DbBackend>, pistes: &[&Path]) -> Vec<Option<String>> {
    let repo = AlbumRepo::with_backend(db.clone());
    pistes
        .iter()
        .map(|p| {
            let aid = TrackRepo::with_backend(db.clone())
                .get_by_path(&p.to_string_lossy())
                .unwrap()
                .expect("piste indexée")
                .album_id
                .expect("album");
            repo.get(aid).unwrap().unwrap().cover_path
        })
        .collect()
}

#[tokio::test]
async fn une_racine_injoignable_au_scan_ne_retire_aucune_pochette_5682() {
    let _seul = crate::routes::system::scan::serialiser_les_scans_de_test();
    let r = racine("nas-absent-5682");
    // Une jaquette intégrée, une pochette de dossier : les deux sources que
    // suit la règle de #5034.
    let (_d1, p1) = album_dans(&r, "Album integre", Some(JAQUETTE), None);
    let (d2, p2) = album_dans(&r, "Album dossier", None, Some(COVER));
    let etat = etat_sur(&r);
    let db = etat.backend.clone();
    scan_manuel(&etat, false, None).await;
    let pistes = [p1[0].as_path(), p2[0].as_path()];
    let attendu = vec![Some(content_hash(JAQUETTE)), Some(content_hash(COVER))];
    assert_eq!(pochettes(&db, &pistes), attendu, "montage de l'épreuve");

    // 1. Point de montage VIDE : le NAS n'est pas encore monté dessus.
    // Hors de la racine le temps de l'épreuve, remis AVANT de conclure : un
    // rouge ne doit rien laisser traîner sous le dossier courant.
    let hors = PathBuf::from(format!("{}-hors-montage", r.path().display()));
    std::fs::rename(r.join("Didier"), &hors).unwrap();
    scan_de_demarrage(&db).await;
    let apres_demarrage = pochettes(&db, &pistes);
    scan_manuel(&etat, false, None).await;
    let apres_manuel = pochettes(&db, &pistes);
    std::fs::rename(&hors, r.join("Didier")).unwrap();
    assert_eq!(
        apres_demarrage, attendu,
        "scan de démarrage, point de montage vide : les pochettes sont retirées"
    );
    assert_eq!(
        apres_manuel, attendu,
        "scan manuel, point de montage vide : les pochettes sont retirées"
    );

    // 2. Racine ABSENTE.
    let ailleurs = PathBuf::from(format!("{}-absente", r.path().display()));
    std::fs::rename(r.path(), &ailleurs).unwrap();
    scan_de_demarrage(&db).await;
    let apres_demarrage = pochettes(&db, &pistes);
    scan_manuel(&etat, false, None).await;
    let apres_manuel = pochettes(&db, &pistes);
    std::fs::rename(&ailleurs, r.path()).unwrap();
    assert_eq!(
        apres_demarrage, attendu,
        "scan de démarrage, racine absente : les pochettes sont retirées"
    );
    assert_eq!(
        apres_manuel, attendu,
        "scan manuel, racine absente : les pochettes sont retirées"
    );

    // 3. Contre-épreuve : le NAS revient, et la règle de #5034 vit toujours.
    // Une jaquette ôtée dans Mp3tag, un `cover.jpg` supprimé : retirés.
    for p in &p1 {
        super::pochettes_disque_tests_5034::poser_jaquette(p, None);
    }
    super::pochettes_disque_tests_5034::poser_cover(&d2, None);
    scan_de_demarrage(&db).await;
    assert_eq!(
        pochettes(&db, &pistes),
        vec![None, None],
        "#5034 : une pochette dont le fichier source a quitté un disque JOIGNABLE suit"
    );
}
