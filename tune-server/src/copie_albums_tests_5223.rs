//! #5223 : copie préallouée terminée à taille égale dans la même seconde.
//! Dates imposées, vrais FLAC et chemins de production ; pas de course au sommeil.
use super::pochettes_disque_tests_5034::{
    album_dans, lot_du_surveillant, racine, scan_de_demarrage, scan_manuel,
};
use crate::state::AppState;
use std::path::Path;
use std::time::{Duration, SystemTime};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;

fn dater(path: &Path, nanos: u64) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(
            SystemTime::UNIX_EPOCH
                + Duration::from_secs(1_750_000_000)
                + Duration::from_nanos(nanos),
        )
        .unwrap();
}

#[derive(Clone, Copy, Debug)]
enum Passe {
    Rapide,
    AncienneBase,
    Demarrage,
    Surveillant,
    NomDuDossier,
    Complet,
}

async fn copie(passe: Passe) {
    let r = racine(&format!("copie-5223-{passe:?}"));
    let musique = r.join("musique");
    let (dossier, pistes) = album_dans(&musique, "Double album", None, None);
    let (dossier, pistes) = if matches!(passe, Passe::NomDuDossier) {
        let nouveau = dossier.with_file_name("Coffret copie");
        std::fs::rename(&dossier, &nouveau).unwrap();
        let pistes = pistes
            .iter()
            .map(|p| nouveau.join(p.file_name().unwrap()))
            .collect::<Vec<_>>();
        (nouveau, pistes)
    } else {
        (dossier, pistes)
    };
    let cd1 = dossier.join("CD1");
    let cd2 = dossier.join("CD2");
    std::fs::create_dir_all(&cd1).unwrap();
    std::fs::create_dir_all(&cd2).unwrap();
    let saine = cd1.join(pistes[0].file_name().unwrap());
    std::fs::rename(&pistes[0], &saine).unwrap();
    let finie = std::fs::read(&pistes[1]).unwrap();
    let partielle = cd2.join("02 - Copie.flac");
    std::fs::rename(&pistes[1], &partielle).unwrap();
    // Une copie préallouée peut avoir sa longueur finale avant ses balises.
    std::fs::write(&partielle, vec![0; finie.len()]).unwrap();
    dater(&partielle, 125_000_000);
    let avant = std::fs::metadata(&partielle).unwrap().modified().unwrap();
    let etat = AppState::new(&r.join("tune.db").to_string_lossy(), 0, Default::default()).unwrap();
    SettingsRepo::with_backend(etat.backend.clone())
        .set(
            "music_dirs",
            &serde_json::to_string(&[musique.to_string_lossy()]).unwrap(),
        )
        .unwrap();
    if matches!(passe, Passe::NomDuDossier) {
        lot_du_surveillant(
            &etat.backend,
            &musique,
            std::slice::from_ref(&partielle),
            &[],
        );
        lot_du_surveillant(&etat.backend, &musique, std::slice::from_ref(&saine), &[]);
    } else {
        lot_du_surveillant(
            &etat.backend,
            &musique,
            &[saine.clone(), partielle.clone()],
            &[],
        );
    }
    let repo = TrackRepo::with_backend(etat.backend.clone());
    let provisoire = repo
        .get_by_path(&partielle.to_string_lossy())
        .unwrap()
        .unwrap();
    assert_ne!(
        provisoire.title, "Deux",
        "le témoin doit vraiment avoir lu le repli du fichier incomplet"
    );
    if matches!(passe, Passe::NomDuDossier) {
        assert_eq!(
            provisoire.album_title.as_deref(),
            Some("Coffret copie"),
            "le montage doit conserver le nom de dossier comme album provisoire"
        );
    }
    let taille = provisoire.file_size;
    // Même le repli sans balises doit rester stable tant que rien ne change.
    lot_du_surveillant(
        &etat.backend,
        &musique,
        std::slice::from_ref(&partielle),
        &[],
    );
    assert_eq!(
        repo.get_by_path(&partielle.to_string_lossy())
            .unwrap()
            .unwrap()
            .id,
        provisoire.id,
        "un fichier incomplet inchangé ne doit pas boucler"
    );
    if matches!(passe, Passe::AncienneBase) {
        // Valeur réellement écrite par les versions qui tronquaient à la seconde.
        repo.update_mtime_and_size(
            &partielle.to_string_lossy(),
            1_750_000_000.0,
            taille.unwrap(),
        )
        .unwrap();
    }
    std::fs::write(&partielle, finie).unwrap();
    dater(&partielle, 250_000_000);
    let apres = std::fs::metadata(&partielle).unwrap().modified().unwrap();
    assert_ne!(
        avant, apres,
        "le système de fichiers doit conserver les fractions de seconde"
    );
    assert_eq!(
        avant
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        apres
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    );
    assert_eq!(
        taille,
        Some(std::fs::metadata(&partielle).unwrap().len() as i64)
    );
    match passe {
        Passe::Rapide | Passe::AncienneBase => scan_manuel(&etat, false, None).await,
        Passe::Complet => scan_manuel(&etat, true, None).await,
        Passe::Demarrage => scan_de_demarrage(&etat.backend).await,
        Passe::Surveillant | Passe::NomDuDossier => lot_du_surveillant(
            &etat.backend,
            &musique,
            std::slice::from_ref(&partielle),
            &[],
        ),
    }
    let relue = repo
        .get_by_path(&partielle.to_string_lossy())
        .unwrap()
        .unwrap();
    assert_eq!(
        relue.title, "Deux",
        "#5223 : {passe:?} garde les balises de la copie incomplète"
    );
    assert_eq!(
        relue.album_title.as_deref(),
        Some("Double album"),
        "#5223 : {passe:?} garde le nom du dossier comme titre d'album"
    );
    let voisine = repo.get_by_path(&saine.to_string_lossy()).unwrap().unwrap();
    assert_eq!(
        relue.album_id, voisine.album_id,
        "#5223 : la piste reste détachée du double album après {passe:?}"
    );
    // L'événement suivant sans écriture ne doit ni supprimer ni recréer la piste.
    lot_du_surveillant(
        &etat.backend,
        &musique,
        std::slice::from_ref(&partielle),
        &[],
    );
    assert_eq!(
        repo.get_by_path(&partielle.to_string_lossy())
            .unwrap()
            .unwrap()
            .id,
        relue.id,
        "#5223 : un événement inchangé réimporte la piste en boucle"
    );
    let carte = repo.get_all_file_info_by_path().unwrap();
    assert!(
        !crate::routes::system::scan::file_needs_scan(&partielle, &carte),
        "#5223 : l'analyse rapide doit ignorer le fichier stabilisé"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copie_rapide_5223() {
    copie(Passe::Rapide).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copie_demarrage_5223() {
    copie(Passe::Demarrage).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copie_surveillant_5223() {
    copie(Passe::Surveillant).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copie_complet_temoin_5223() {
    copie(Passe::Complet).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copie_ancienne_base_5223() {
    copie(Passe::AncienneBase).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copie_nom_du_dossier_5223() {
    copie(Passe::NomDuDossier).await;
}
