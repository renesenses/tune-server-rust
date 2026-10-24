//! #6399 — « Lire à partir d'ici » / « Tout lire » sur une liste de service
//! ne fait plus GROSSIR la file à chaque lancement.
//!
//! Le geste du client (`lectureEnMasse.ts`, `lireListeDepuis`) : lancer le
//! titre de tête (`queue_follows: true`), enfiler la suite en fin de file,
//! puis remettre les titres d'avant en tête (`position: 0`). Le journal de
//! Nguyen Thanh Ha (fil 2289) le montre exactement :
//! `file_de_lecture_ecrite branche="titre_seul_deja_en_file" longueur=24530`,
//! puis `queue_add_ok added=4999 … queue_length=29529`, puis
//! `added=200 position=Some(0)`.
//!
//! Le titre de tête figurait déjà dans la file (le lancement d'avant l'y avait
//! mis) : la route GARDAIT la file, et la suite s'y ajoutait. Chaque
//! lancement ajoutait la liste entière.
//!
//! Contre-épreuve : rendre `Garder` à une tête de liste dans
//! `file_pour_un_titre_de_service` ; `relancer_la_meme_liste_ne_fait_pas_grossir_la_file_6399`
//! rougit sur la longueur de la file.

use super::*;
use std::sync::Arc;
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;

fn ligne(id: usize) -> QueueInput {
    QueueInput::Streaming {
        source: "qobuz".into(),
        source_id: format!("q{id}"),
        title: format!("Titre {id}"),
        artist: "Artiste".into(),
        album: None,
        cover_url: None,
        duration_ms: 200_000,
        track_number: None,
        disc_number: None,
        album_ref: None,
        artist_ref: None,
    }
}

/// Une base sur disque (pas `:memory:`, dont les lectures ne voient pas
/// toujours les écritures) et une zone.
fn banc() -> (PlayQueueRepo, i64, tune_core::test_scratch::ScratchDir) {
    let dir = tune_core::test_scratch::scratch_dir("file-6399");
    let chemin = dir.path().join("tune.db");
    let db = SqliteDb::open(chemin.to_str().unwrap()).unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let zid = tune_core::db::zone_repo::ZoneRepo::with_backend(backend.clone())
        .create("HQPlayer", Some("hqplayer"), None)
        .unwrap();
    (PlayQueueRepo::with_backend(backend), zid, dir)
}

/// Ce que la route `POST /zones/{id}/play` fait de la file pour un titre de
/// service, branche par branche (voir `play`, après `orchestrator.play`), puis
/// ce que le client enfile derrière.
fn lire_a_partir_d_ici(repo: &PlayQueueRepo, zid: i64, liste: &[usize], index: usize) {
    let tete = format!("q{}", liste[index]);
    let lecture = repo.get_ordered(zid);
    match file_pour_un_titre_de_service(
        lecture.as_deref().map_err(String::as_str),
        Some("qobuz"),
        &tete,
        true,
    ) {
        FileDejaChargee::Garder { .. } | FileDejaChargee::NePasToucher => {}
        FileDejaChargee::RemplacerParCeTitre => {
            repo.clear(zid).unwrap();
            repo.append(zid, &[ligne(liste[index])]).unwrap();
        }
    }
    let suite: Vec<QueueInput> = liste[index + 1..].iter().map(|&i| ligne(i)).collect();
    repo.insert_at_bilan(zid, &suite, None).unwrap();
    let avant: Vec<QueueInput> = liste[..index].iter().map(|&i| ligne(i)).collect();
    repo.insert_at_bilan(zid, &avant, Some(0)).unwrap();
}

/// La file factice : une liste de 60 titres de service, relancée trois fois
/// depuis trois rangs. La file doit porter la liste, une fois, à chaque fois.
#[test]
fn relancer_la_meme_liste_ne_fait_pas_grossir_la_file_6399() {
    let (repo, zid, _dir) = banc();
    let liste: Vec<usize> = (1..=60).collect();
    for index in [10, 25, 40] {
        lire_a_partir_d_ici(&repo, zid, &liste, index);
        let file = repo.get_ordered(zid).unwrap();
        assert_eq!(
            file.len(),
            liste.len(),
            "la file grossit à chaque « Lire à partir d'ici » (lancement depuis le rang {index})"
        );
        let ids: Vec<String> = file.iter().filter_map(|e| e.source_id.clone()).collect();
        let attendu: Vec<String> = liste.iter().map(|i| format!("q{i}")).collect();
        assert_eq!(ids, attendu, "la file porte la liste, dans son ordre");
    }
}

/// La relance de la barre de transport (Stop puis Play, sans
/// `queue_follows`) GARDE la file : #2569 n'est pas défait.
#[test]
fn la_relance_nue_garde_la_file_2569() {
    let (repo, zid, _dir) = banc();
    let lignes: Vec<QueueInput> = (1..=5).map(ligne).collect();
    repo.append(zid, &lignes).unwrap();
    let lecture = repo.get_ordered(zid);
    let decision = file_pour_un_titre_de_service(
        lecture.as_deref().map_err(String::as_str),
        Some("qobuz"),
        "q3",
        false,
    );
    assert!(
        matches!(
            decision,
            FileDejaChargee::Garder {
                position: 2,
                longueur: 5
            }
        ),
        "{decision:?}"
    );
}

/// Une base illisible ne fait rien effacer, même pour une tête de liste.
#[test]
fn une_base_illisible_ne_fait_rien_effacer_6399() {
    assert_eq!(
        file_pour_un_titre_de_service(Err("database is locked"), Some("qobuz"), "q1", true),
        FileDejaChargee::NePasToucher
    );
}
