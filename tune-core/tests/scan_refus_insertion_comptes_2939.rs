//! Un scan qui perd des pistes ne peut plus se déclarer sans erreur (#2939).
//!
//! ## Ce que le journal d'Alain Bonnel montrait
//!
//! Fil forum 1313, partage Windows UNC `\\192.168.0.2\music`, 58 360 fichiers.
//! Quatorze fichiers — un album entier, *As We Once Were* de The Anchoress —
//! ont été lus sans la moindre erreur de balise, puis refusés un par un par la
//! base :
//!
//! ```text
//! walker:     batched_scan_complete total=14 metadata_ok=14 metadata_failed=0
//! track_repo: track_insert_failed_in_batch … UNIQUE constraint failed: tracks.file_path   (×14)
//! ```
//!
//! Le résumé de fin de scan annonçait donc un scan **intégralement réussi**
//! pendant qu'un album entier n'entrait pas dans la bibliothèque. C'est pire
//! que le défaut lui-même : l'utilisateur ne sait pas qu'il lui manque de la
//! musique, et n'a aucune raison de chercher.
//!
//! `metadata_failed` ne mentait pas — il **répondait à côté**. Il compte des
//! LECTURES de fichier (`tune-core/src/scanner/walker.rs`, filtre
//! `f.metadata.is_none() && f.unsupported.is_none()`), jamais des écritures.
//! L'écriture, elle, est faite par la fermeture d'importation, qui ne rendait
//! rien : le parcours ne pouvait pas savoir.
//!
//! ## Ce que ce fichier tient
//!
//! 1. **Le cas d'Alain, reproduit sans son matériel** : quatorze vrais
//!    fichiers sur disque, une vraie base SQLite sur disque, quatorze lignes
//!    déjà présentes au même `file_path`, et la contrainte d'unicité qui
//!    refuse les quatorze insertions. Le résumé rendu par `scan_files_batched`
//!    DOIT porter les quatorze refus.
//! 2. **Le témoin** : le même scan sur une base vide entre en entier, et le
//!    résumé annonce toujours zéro échec. Un compteur alarmiste serait un
//!    autre défaut, pas un correctif.
//!
//! Les deux cas passent par les fonctions de PRODUCTION —
//! [`tune_core::scanner::walker::scan_files_batched`] et
//! [`tune_core::db::track_repo::TrackRepo::create_batch`] — jamais par une
//! transcription de leur logique.
//!
//! ## Au passage : les deux portées, désormais d'accord
//!
//! Ce fichier mesurait aussi l'écart de portée décrit par l'issue : la carte
//! de dédoublonnage était chargée par
//! `SELECT … FROM tracks WHERE source = 'local'`, tandis que la contrainte est
//! `file_path TEXT UNIQUE` sur toute la table, sans condition sur `source`. Il
//! l'épinglait tel quel — « la carte rend zéro entrée là où la table en a
//! quatorze » — parce qu'établir que l'écart est atteignable en base était le
//! préalable à toute décision sur la portée de la requête.
//!
//! **La décision est prise.** La carte couvre maintenant toute la table
//! (`TrackRepo::get_all_file_info_by_path`), et l'assertion est retournée : les
//! quatorze lignes DOIVENT être vues. Elle n'est pas supprimée — c'est elle qui
//! empêche la portée de se rétrécir à nouveau. Le raisonnement et le garde de
//! la décision d'écriture vivent dans
//! `tune-server/tests/portee_unicite_file_path_2939.rs`.
//!
//! Ce que ce fichier garde reste inchangé : présenter quatorze insertions que
//! la base refuse et vérifier que le RÉSUMÉ les porte. La voie d'écriture
//! réduite utilisée ici (`create_batch` seul, sans la décision du scan) reste
//! le moyen le plus direct de provoquer un refus réel.
//!
//! `autotests = false` dans `tune-core/Cargo.toml` — la cible `[[test]]` est
//! déclarée là-bas, sans quoi ce fichier ne serait jamais compilé.

use std::path::PathBuf;

use tune_core::db::models::Track;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo::TrackRepo;
use tune_core::scanner::walker::{EcrituresDuLot, SCAN_BATCH_SIZE, scan_files_batched};

/// Le lot d'Alain Bonnel : un album de quatorze pistes.
const PISTES_DE_L_ALBUM: usize = 14;

/// Les quatorze fichiers, posés sur un vrai disque à partir du FLAC de
/// référence du dépôt — le parcours lit de vraies balises, comme en
/// production. Rend les chemins dans l'ordre.
fn poser_l_album(racine: &std::path::Path) -> Vec<PathBuf> {
    let flac = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test.flac");
    let octets = std::fs::read(&flac).expect("le FLAC de référence du dépôt doit être lisible");
    let dossier = racine.join("The Anchoress").join("As We Once Were");
    std::fs::create_dir_all(&dossier).unwrap();
    (1..=PISTES_DE_L_ALBUM)
        .map(|n| {
            let chemin = dossier.join(format!("{n:02} - Piste {n}.flac"));
            std::fs::write(&chemin, &octets).unwrap();
            chemin
        })
        .collect()
}

/// Ouvre une base SQLite SUR DISQUE, initialise le schéma, la referme, puis la
/// rouvre : le schéma testé est celui qui a été écrit sur le disque, pas un
/// état de mémoire qui n'a jamais été relu.
fn base_sur_disque(chemin: &std::path::Path) -> SqliteDb {
    let url = chemin.to_string_lossy().into_owned();
    {
        let db = SqliteDb::open(&url).expect("ouverture de la base de travail");
        db.init_schema().expect("schéma initialisé");
    }
    SqliteDb::open(&url).expect("réouverture de la base fermée")
}

/// L'import que le serveur fait à chaque lot, réduit à ce que ce test mesure :
/// présenter les pistes du lot à `create_batch`, et rendre au parcours le
/// manque à écrire. C'est cette valeur de retour qui manquait (#2939).
fn importer_le_lot(
    repo: &TrackRepo,
    batch: &[tune_core::scanner::walker::ScannedFile],
) -> EcrituresDuLot {
    let a_inserer: Vec<Track> = batch
        .iter()
        .filter(|sf| sf.metadata.is_some())
        .map(|sf| {
            let mut piste = Track::new(
                sf.metadata
                    .as_ref()
                    .and_then(|m| m.title.clone())
                    .unwrap_or_else(|| "Sans titre".into()),
            );
            piste.file_path = Some(sf.path.clone());
            piste.file_size = Some(sf.file_size as i64);
            piste
        })
        .collect();
    let entrees = repo.create_batch(&a_inserer).unwrap_or(0);
    EcrituresDuLot::manque(a_inserer.len(), entrees)
}

#[test]
fn un_album_refuse_par_l_unicite_apparait_dans_le_resume_de_fin_de_scan() {
    let musique = tempfile::TempDir::new().unwrap();
    let base = tempfile::TempDir::new().unwrap();
    let fichiers = poser_l_album(musique.path());
    assert_eq!(fichiers.len(), PISTES_DE_L_ALBUM);

    let db = base_sur_disque(&base.path().join("tune.db"));
    let repo = TrackRepo::new(db.clone());

    // La situation d'Alain : les quatorze chemins EXISTENT déjà dans `tracks`,
    // mais sous une autre `source` que `local`.
    let deja_la: Vec<Track> = fichiers
        .iter()
        .enumerate()
        .map(|(i, chemin)| {
            let mut piste = Track::new(format!("Déjà là {i}"));
            piste.file_path = Some(chemin.to_string_lossy().into_owned());
            piste.source = "tidal".into();
            piste
        })
        .collect();
    assert_eq!(
        repo.create_batch(&deja_la).unwrap(),
        PISTES_DE_L_ALBUM,
        "sans ces quatorze lignes, la contrainte n'aurait rien à refuser et \
         le test ne mesurerait rien"
    );

    // Les deux portées, remises d'accord. Cette assertion épinglait l'écart :
    // elle exigeait ZÉRO entrée vue, parce que la carte était chargée par
    // `WHERE source = 'local'` et qu'une ligne `source = 'tidal'` au même
    // `file_path` lui était invisible — c'est cette invisibilité qui envoyait
    // le fichier à l'insertion, et l'insertion au refus. Elle est retournée,
    // pas retirée : c'est elle qui interdit à la portée de se rétrécir à
    // nouveau.
    let carte = repo.get_all_file_info_by_path().unwrap();
    let vues_par_la_carte = fichiers
        .iter()
        .filter(|c| carte.contains_key(c.to_string_lossy().as_ref()))
        .count();
    assert_eq!(
        vues_par_la_carte, PISTES_DE_L_ALBUM,
        "la carte a désormais la portée de la contrainte : elle voit les \
         quatorze lignes que `file_path TEXT UNIQUE` refuse de doubler"
    );

    // Le scan, par la fonction de production.
    let stats = scan_files_batched(&fichiers, false, SCAN_BATCH_SIZE, |batch, _, _| {
        importer_le_lot(&repo, &batch)
    });

    // Ce que le journal d'Alain montrait, et qui reste vrai : les balises se
    // sont toutes lues. Ce compteur-là répond à une autre question.
    assert_eq!(stats.total_files, PISTES_DE_L_ALBUM);
    assert_eq!(stats.metadata_ok, PISTES_DE_L_ALBUM);
    assert_eq!(
        stats.metadata_failed, 0,
        "la lecture des balises n'a jamais échoué : c'est bien pour ça que \
         `metadata_failed` ne pouvait pas rendre compte de la perte"
    );

    // Ce qui manquait, et qui est l'objet de ce garde.
    assert_eq!(
        stats.db_insert_failed, PISTES_DE_L_ALBUM,
        "quatorze insertions ont été refusées par la base : le résumé de fin \
         de scan DOIT les porter, sinon l'utilisateur n'a aucun moyen de \
         savoir qu'un album entier manque à sa bibliothèque"
    );
    assert_eq!(stats.db_update_failed, 0);
    assert!(
        stats.a_perdu_des_pistes(),
        "un scan qui perd quatorze pistes ne peut pas se déclarer sans erreur"
    );

    // Et la perte est réelle, pas seulement comptée : aucune des quatorze
    // pistes n'est entrée sous `source = 'local'`. Le comptage est explicite
    // depuis que la carte couvre toute la table — les quatorze lignes `tidal`
    // y sont, et elles ne sont pas des pistes locales.
    assert_eq!(
        repo.get_all_file_info_by_path()
            .unwrap()
            .values()
            .filter(|info| info.est_locale())
            .count(),
        0,
        "aucune piste locale n'a pu entrer — c'est bien une perte, pas un \
         faux positif du compteur"
    );
}

#[test]
fn un_scan_qui_reussit_annonce_toujours_zero_echec() {
    let musique = tempfile::TempDir::new().unwrap();
    let base = tempfile::TempDir::new().unwrap();
    let fichiers = poser_l_album(musique.path());

    // Même album, même code, mais une base où rien ne s'oppose à l'insertion.
    let db = base_sur_disque(&base.path().join("tune.db"));
    let repo = TrackRepo::new(db.clone());

    let stats = scan_files_batched(&fichiers, false, SCAN_BATCH_SIZE, |batch, _, _| {
        importer_le_lot(&repo, &batch)
    });

    assert_eq!(stats.metadata_ok, PISTES_DE_L_ALBUM);
    assert_eq!(
        stats.db_insert_failed, 0,
        "rendre le compteur alarmiste serait un autre défaut : un scan qui \
         se passe bien doit continuer d'annoncer zéro échec"
    );
    assert_eq!(stats.db_update_failed, 0);
    assert!(
        !stats.a_perdu_des_pistes(),
        "aucune piste n'a été perdue : le résumé ne doit rien signaler"
    );
    assert_eq!(
        repo.get_all_file_info_by_path()
            .unwrap()
            .values()
            .filter(|info| info.est_locale())
            .count(),
        PISTES_DE_L_ALBUM,
        "contre-épreuve du témoin : les quatorze pistes sont bien EN BASE, \
         sans quoi ce test serait vert contre rien"
    );
}
