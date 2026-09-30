//! #5552 (LANDES Philippe, fil 2063, ticket 201) — « Arrêter » ne stoppait pas
//! le scan de démarrage.
//!
//! `spawn_auto_scan` appelait `scan_files_batched`, qui délègue au parcours
//! avec un arrêt `|| false` : seul le rappel du lot lisait la demande, APRÈS
//! que le parcours eut lu les balises et l'empreinte des fichiers du lot. Les
//! écritures cessaient, la lecture de toute la bibliothèque continuait. Et la
//! route `scan/cancel` écrivait `scan_status = idle` sur-le-champ, levant la
//! garde de mise à jour pendant que le parcours tournait encore.
//!
//! L'épreuve joue le VRAI `spawn_auto_scan` et la VRAIE route `scan/cancel`.
//! Trois dossiers de 400 fichiers `.flac` illisibles : trois lots, puisque les
//! lots suivent les dossiers (#3232). Pour que la demande d'arrêt tombe à coup
//! sûr AVANT la lecture du deuxième lot, l'épreuve tient la porte d'écriture
//! SQLite (`sqlite_write_gate`) : le scan lit son premier lot, puis s'y
//! bloque, à l'entrée de sa transaction. Aucune course : quel que soit
//! l'instant où l'arrêt arrive, il arrive avant le lot 2.
//!
//! Ce que le scan a LU se lit dans `library.scan.completed` :
//! `metadata_ok + metadata_failed + metadata_timeout`. Sans le correctif, les
//! 1 200 fichiers y sont ; avec, au plus le premier lot.
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::event_bus::EventBus;

const DOSSIERS: usize = 3;
const FICHIERS_PAR_DOSSIER: usize = 400;

fn bibliotheque_illisible(racine: &std::path::Path) {
    for d in 0..DOSSIERS {
        let dossier = racine.join(format!("Dossier {d}"));
        std::fs::create_dir_all(&dossier).unwrap();
        for f in 0..FICHIERS_PAR_DOSSIER {
            std::fs::write(dossier.join(format!("{f:04}.flac")), b"pas du flac").unwrap();
        }
    }
}

/// Lance le VRAI scan de démarrage, la porte d'écriture SQLite tenue, et rend
/// la main quand il entre dans la lecture des lots (`library.scan.started`,
/// émis après le préfiltre). Rend aussi la porte : tant que l'appelant la
/// tient, le scan lit son premier lot puis s'y bloque, à l'entrée de sa
/// transaction.
///
/// Les autres essais du binaire ne se sérialisent pas tous sur le verrou des
/// scans : l'un d'eux peut tenir le droit de scanner, et le scan de démarrage
/// se retire alors sans un mot (`auto_scan_skipped_already_scanning`). On le
/// voit à `fini` levé sans départ annoncé — porte tenue, un scan qui a
/// annoncé son départ ne peut pas finir. On RELÂCHE alors la porte avant de
/// recommencer : l'autre scan en a besoin pour écrire ses lots, et le lui
/// refuser pendant l'attente l'empêcherait de finir (vu sur Shrek :
/// `le_scan_indexe_une_image_iso_de_donnees_5299` expirait).
pub(super) async fn demarrer_jusqu_a_la_lecture(
    db: &Arc<dyn tune_core::db::backend::DbBackend>,
    bus: &Arc<EventBus>,
    rx: &mut tokio::sync::broadcast::Receiver<tune_core::event_bus::TuneEvent>,
) -> (
    Arc<std::sync::atomic::AtomicBool>,
    tokio::sync::MutexGuard<'static, ()>,
) {
    let debut = Instant::now();
    loop {
        crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
        let porte = crate::sqlite_write_gate::user_queue().await;
        let fini = crate::auto_scan::spawn_auto_scan(db.clone(), bus.clone());
        loop {
            match suivant(rx) {
                Some(ev) if ev.event_type == "library.scan.started" => return (fini, porte),
                Some(_) => continue,
                None => {}
            }
            if fini.load(Ordering::Acquire) {
                break;
            }
            assert!(
                debut.elapsed() < Duration::from_secs(300),
                "le scan de démarrage n'a jamais commencé à lire"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        drop(porte);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// L'événement suivant déjà émis, en sautant un retard du canal (`Lagged`)
/// au lieu de s'arrêter dessus. `None` : la file est vide.
pub(super) fn suivant(
    rx: &mut tokio::sync::broadcast::Receiver<tune_core::event_bus::TuneEvent>,
) -> Option<tune_core::event_bus::TuneEvent> {
    loop {
        match rx.try_recv() {
            Ok(ev) => return Some(ev),
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
            Err(_) => return None,
        }
    }
}

fn lus(rapport: &serde_json::Value) -> u64 {
    ["metadata_ok", "metadata_failed", "metadata_timeout"]
        .iter()
        .map(|k| rapport[*k].as_u64().unwrap_or(0))
        .sum()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// Le verrou des scans d'épreuve est tenu à travers les `.await` à dessein :
// c'est lui qui tient les autres scans d'épreuve à l'écart.
#[allow(clippy::await_holding_lock)]
async fn arreter_le_scan_de_demarrage_arrete_la_lecture_et_garde_scanning_jusqu_au_bout_5552() {
    let _seul = crate::routes::system::scan::serialiser_les_scans_de_test_sans_bloquer().await;
    crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;

    let base = tune_core::test_scratch::scratch_dir("arret-5552-base");
    // HORS de `temp_dir()` : le parcours y écarte tout, comme temporaires de
    // Tune (`is_tune_temp_file`).
    let racine = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        "arret-5552-musique",
    );
    bibliotheque_illisible(racine.path());
    let etat = crate::state::AppState::new(
        &base.path().join("tune-epreuve.db").to_string_lossy(),
        0,
        Default::default(),
    )
    .expect("AppState sur base de fichier");
    let db = etat.backend.clone();
    let reglages = SettingsRepo::with_backend(db.clone());
    reglages
        .set(
            "music_dirs",
            &serde_json::to_string(&[racine.path().to_string_lossy()]).unwrap(),
        )
        .unwrap();

    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();

    // La porte d'écriture est tenue : le scan lira son premier lot, puis
    // attendra à l'entrée de sa transaction.
    let (fini, porte) = demarrer_jusqu_a_la_lecture(&db, &bus, &mut rx).await;
    assert!(
        crate::routes::system::scan::droit_de_scanner_tenu(),
        "le scan de démarrage doit tenir le droit de scanner"
    );

    // « Arrêter », par la vraie route.
    let _ = crate::routes::system::scan::scan_cancel(axum::extract::State(etat.clone())).await;

    // Le scan tourne encore (bloqué à la porte) : `scan_status` ne doit PAS
    // dire `idle`, sinon la garde de mise à jour se lève sous un scan vivant.
    assert_eq!(
        reglages.get("scan_status").unwrap().as_deref(),
        Some("scanning"),
        "#5552 : « Arrêter » a écrit `idle` alors que le scan tient encore le droit de scanner"
    );
    assert!(!fini.load(Ordering::Acquire));

    drop(porte);

    let debut = Instant::now();
    while !fini.load(Ordering::Acquire) {
        assert!(
            debut.elapsed() < Duration::from_secs(120),
            "le scan de démarrage ne s'est jamais arrêté"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;

    let mut fin = None;
    while let Some(ev) = suivant(&mut rx) {
        if ev.event_type == "library.scan.completed" {
            fin = Some(ev.data);
        }
    }
    let fin = fin.expect("le scan arrêté doit annoncer sa fin (library.scan.completed)");
    let total = (DOSSIERS * FICHIERS_PAR_DOSSIER) as u64;
    let lus = lus(&fin);
    assert!(
        lus <= FICHIERS_PAR_DOSSIER as u64,
        "#5552 : {lus} fichiers lus sur {total} — après « Arrêter », le parcours a continué \
         de lire les lots suivants : {fin}"
    );
    assert_eq!(
        fin["cancelled"], true,
        "le rapport dit que le scan a été arrêté : {fin}"
    );

    // Arrêté pour de bon : le statut retombe, par le scan lui-même.
    assert_eq!(
        reglages.get("scan_status").unwrap().as_deref(),
        Some("idle")
    );
}
