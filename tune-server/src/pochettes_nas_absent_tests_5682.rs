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

/// Une piste FLAC `<dossier>/<n> - Piste.flac`, taguée `ALBUM = album`,
/// sans jaquette, datée d'hier.
fn piste_dans(dossier: &Path, album: &str, n: usize) -> PathBuf {
    use lofty::config::{ParseOptions, WriteOptions};
    use lofty::file::AudioFile;
    use lofty::flac::FlacFile;
    use lofty::ogg::VorbisComments;
    std::fs::create_dir_all(dossier).unwrap();
    let piste = dossier.join(format!("{n:03} - Piste.flac"));
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../tune-core/tests/fixtures/test.flac"),
        &piste,
    )
    .unwrap();
    let mut f = std::fs::File::open(&piste).unwrap();
    let mut flac = FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
    drop(f);
    let mut vc = VorbisComments::default();
    let titre = format!("Piste {n}");
    let numero = n.to_string();
    for (k, v) in [
        ("TITLE", titre.as_str()),
        ("ARTIST", "Didier"),
        ("ALBUMARTIST", "Didier"),
        ("ALBUM", album),
        ("TRACKNUMBER", numero.as_str()),
    ] {
        vc.insert(k.to_string(), v.to_string());
    }
    flac.set_vorbis_comments(vc);
    flac.save_to_path(&piste, WriteOptions::default()).unwrap();
    super::pochettes_disque_tests_5034::poser_jaquette(&piste, None);
    std::fs::File::options()
        .write(true)
        .open(&piste)
        .and_then(|f| {
            f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(86_400))
        })
        .unwrap();
    piste
}

/// Suite de #5854 — un montage IMBRIQUÉ tombé (`<racine>/Montage`, plus de
/// `SEUIL_SOUS_ARBRE_VIDE` pistes) laisse la racine répondre : ni
/// `missing_dirs` ni `racines_videes` ne le voient, et la passe de #5034
/// n'était gardée que par eux. La pochette d'un album tirée d'un `cover.jpg`
/// sous ce montage passait pour « source disparue » ; l'album, dont une
/// partie des pistes vit hors du montage, était relu sans son image et la
/// perdait. Ses pistes, elles, étaient conservées (`ProtegeIllisible`).
#[tokio::test]
async fn un_montage_imbrique_tombe_ne_retire_pas_la_pochette_des_albums_qu_il_porte() {
    let _seul = crate::routes::system::scan::serialiser_les_scans_de_test();
    let r = racine("montage-imbrique-5854");
    let montage = r.join("Montage");
    // Un album à cheval : un disque hors du montage, sans image ; l'autre
    // sous le montage, avec son `cover.jpg` — la source de la pochette.
    //
    // Le disque hors du montage a DEUX pistes, l'autre une seule : la fusion
    // des doublons conserve l'album le plus fourni, donc celui du disque hors
    // du montage, quel que soit l'ordre dans lequel le parcours rend les
    // dossiers. L'analyse complète relit alors ses pistes DANS l'album dont la
    // pochette vit sous le montage. À égalité, l'album conservé dépendait de
    // l'ordre de `readdir` : vert sur Shrek (l'import forcé créait un album
    // neuf), rouge en CI (#5862).
    let dossier_local = r.join("Local").join("Coffret CD1");
    let hors_montage = piste_dans(&dossier_local, "Coffret", 1);
    piste_dans(&dossier_local, "Coffret", 2);
    let dossier_cover = montage.join("Coffret CD2");
    piste_dans(&dossier_cover, "Coffret", 3);
    super::pochettes_disque_tests_5034::poser_cover(&dossier_cover, Some(COVER));
    // De quoi dépasser le seuil du montage imbriqué.
    let seuil = crate::routes::system::scan::SEUIL_SOUS_ARBRE_VIDE;
    for n in 1..=seuil {
        piste_dans(&montage.join("Remplissage"), "Remplissage", n);
    }
    let etat = etat_sur(&r);
    let db = etat.backend.clone();
    scan_manuel(&etat, false, None).await;
    let pistes = [hors_montage.as_path()];
    let attendu = vec![Some(content_hash(COVER))];
    assert_eq!(pochettes(&db, &pistes), attendu, "montage de l'épreuve");

    // Le montage tombe : son point de montage reste, VIDE, sous une racine
    // qui répond. Remis AVANT de conclure.
    let ailleurs = PathBuf::from(format!("{}-montage-absent", r.path().display()));
    std::fs::rename(&montage, &ailleurs).unwrap();
    std::fs::create_dir(&montage).unwrap();
    scan_manuel(&etat, false, None).await;
    let apres_rapide = pochettes(&db, &pistes);
    scan_manuel(&etat, true, None).await;
    let apres_complet = pochettes(&db, &pistes);
    scan_de_demarrage(&db).await;
    let apres_demarrage = pochettes(&db, &pistes);
    std::fs::remove_dir(&montage).unwrap();
    std::fs::rename(&ailleurs, &montage).unwrap();
    assert_eq!(
        apres_rapide, attendu,
        "analyse rapide, montage imbriqué absent : la pochette est retirée"
    );
    assert_eq!(
        apres_complet, attendu,
        "analyse complète, montage imbriqué absent : la pochette est retirée"
    );
    assert_eq!(
        apres_demarrage, attendu,
        "scan de démarrage, montage imbriqué absent : la pochette est retirée"
    );

    // Le montage revient : la règle de #5034 vit toujours sous lui.
    super::pochettes_disque_tests_5034::poser_cover(&dossier_cover, None);
    scan_de_demarrage(&db).await;
    assert_eq!(
        pochettes(&db, &pistes),
        vec![None],
        "#5034 : un `cover.jpg` supprimé d'un montage PRÉSENT suit"
    );
}
