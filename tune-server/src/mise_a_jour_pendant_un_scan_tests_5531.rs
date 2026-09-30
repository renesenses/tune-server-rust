//! #5531 (Tades, fil 2058 ; Thierry CLEMONT) — mettre à jour pendant un scan.
//!
//! Décision de Bertrand du 30/09/2026 : « forcer » la mise à jour pendant un
//! scan doit **arrêter, installer, relancer**. Le scan est arrêté proprement
//! (l'arrêt de #5552), un repère persistant « scan à reprendre » est posé, et
//! au démarrage suivant le scan reprend en INCRÉMENTAL, même sans
//! `auto_scan`, puis le repère est effacé. Sans `force`, le refus reste.
//!
//! Épreuve de bout en bout sur le VRAI `spawn_auto_scan`, la VRAIE garde de
//! la route d'installation (`refus_du_scan`, `arreter_le_scan_pour_installer`)
//! et la VRAIE décision du démarrage (`scan_au_demarrage`). La route
//! d'installation elle-même n'est pas appelée : elle irait chercher une
//! version sur GitHub et remplacerait le binaire de test.
//!
//! Base de FICHIER : sur `:memory:` le préfiltre ne verrait aucune piste
//! connue, et « incrémental » ne se mesurerait pas.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::event_bus::EventBus;

use super::pochettes_disque_tests_5034::album_dans;

const DOSSIERS: usize = 3;
const FICHIERS_PAR_DOSSIER: usize = 400;

async fn attendre(fini: &Arc<AtomicBool>, quoi: &str) {
    let debut = Instant::now();
    while !fini.load(Ordering::Acquire) {
        assert!(
            debut.elapsed() < Duration::from_secs(180),
            "{quoi} : jamais terminé"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
}

/// Un scan de démarrage qui a VRAIMENT tourné, jusqu'à sa fin. Retiré parce
/// qu'un autre essai tenait le droit de scanner, il n'a pas posé
/// `scan_started_at` : on recommence (même garde que l'épreuve de #5371).
async fn scan_complet_sur(
    db: &Arc<dyn DbBackend>,
    reglages: &SettingsRepo,
    bus: &Arc<EventBus>,
    quoi: &str,
) {
    loop {
        reglages.set("scan_started_at", "0").unwrap();
        let fini = crate::auto_scan::spawn_auto_scan(db.clone(), bus.clone());
        attendre(&fini, quoi).await;
        if reglages.get("scan_started_at").unwrap().as_deref() != Some("0") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn scan_complet(db: &Arc<dyn DbBackend>, reglages: &SettingsRepo, quoi: &str) {
    scan_complet_sur(db, reglages, &Arc::new(EventBus::new()), quoi).await;
}

fn dernier(
    rx: &mut tokio::sync::broadcast::Receiver<tune_core::event_bus::TuneEvent>,
    type_: &str,
) -> Option<serde_json::Value> {
    let mut vu = None;
    while let Some(ev) = super::arret_du_scan_de_demarrage_tests_5552::suivant(rx) {
        if ev.event_type == type_ {
            vu = Some(ev.data);
        }
    }
    vu
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// Le verrou des scans d'épreuve est tenu à travers les `.await` à dessein :
// c'est lui qui tient les autres scans d'épreuve à l'écart.
#[allow(clippy::await_holding_lock)]
async fn forcer_la_mise_a_jour_arrete_le_scan_pose_le_repere_et_reprend_en_incremental_5531() {
    let _seul = crate::routes::system::scan::serialiser_les_scans_de_test_sans_bloquer().await;
    crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;

    let base = tune_core::test_scratch::scratch_dir("maj-5531-base");
    let racine = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        "maj-5531-musique",
    );
    let etat = crate::state::AppState::new(
        &base.path().join("tune-epreuve.db").to_string_lossy(),
        0,
        Default::default(),
    )
    .expect("AppState sur base de fichier");
    let db: Arc<dyn DbBackend> = etat.backend.clone();
    let reglages = SettingsRepo::with_backend(db.clone());
    reglages
        .set(
            "music_dirs",
            &serde_json::to_string(&[racine.path().to_string_lossy()]).unwrap(),
        )
        .unwrap();

    // 1. Une bibliothèque déjà indexée : un album de deux pistes.
    album_dans(racine.path(), "Deja indexe", None, None);
    scan_complet(&db, &reglages, "premier scan").await;

    // 2. Beaucoup de fichiers neufs : le scan suivant est long.
    for d in 0..DOSSIERS {
        let dossier = racine.path().join(format!("Neuf {d}"));
        std::fs::create_dir_all(&dossier).unwrap();
        for f in 0..FICHIERS_PAR_DOSSIER {
            std::fs::write(dossier.join(format!("{f:04}.flac")), b"pas du flac").unwrap();
        }
    }

    // Le scan lit son premier lot puis se bloque à la porte d'écriture.
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let (fini, porte) = super::arret_du_scan_de_demarrage_tests_5552::demarrer_jusqu_a_la_lecture(
        &db, &bus, &mut rx,
    )
    .await;
    assert!(crate::routes::system::scan::droit_de_scanner_tenu());

    // 3. SANS `force` : le refus reste, le scan continue, aucun repère.
    let refus = crate::routes::system::update::refus_du_scan(&db, false)
        .expect("#5531 : sans `force`, un scan en cours doit toujours différer la mise à jour");
    assert_eq!(refus.status(), axum::http::StatusCode::CONFLICT);
    let corps: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(refus.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(corps["reason"], "scan_in_progress", "{corps}");
    assert_eq!(
        corps["force_available"], true,
        "le refus doit dire qu'on peut forcer : {corps}"
    );
    assert!(
        crate::routes::system::scan::droit_de_scanner_tenu(),
        "sans force, rien n'est arrêté"
    );
    assert!(!crate::routes::system::scan::reprise_du_scan_demandee(&db));

    // 4. AVEC `force` : la garde ne refuse plus, et l'arrêt a lieu.
    assert!(
        crate::routes::system::update::refus_du_scan(&db, true).is_none(),
        "#5531 : `force` doit lever la garde du scan"
    );
    let arret = {
        let db = db.clone();
        tokio::spawn(async move {
            crate::routes::system::update::arreter_le_scan_pour_installer(
                &db,
                Duration::from_secs(60),
                Duration::from_millis(20),
            )
            .await
        })
    };
    // Le scan est bloqué à la porte : l'arrêt ne peut pas aboutir tant
    // qu'elle est tenue. On la rend une fois la demande d'arrêt partie.
    let debut = Instant::now();
    while !crate::routes::system::scan::reprise_du_scan_demandee(&db) {
        assert!(
            debut.elapsed() < Duration::from_secs(30),
            "le repère n'a jamais été posé"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(porte);
    assert!(
        arret.await.unwrap(),
        "le scan n'a pas rendu la main dans le délai de la mise à jour"
    );
    assert!(
        !crate::routes::system::scan::droit_de_scanner_tenu(),
        "l'installation ne doit partir qu'une fois le scan RÉELLEMENT arrêté"
    );
    attendre(&fini, "scan arrêté").await;
    assert_eq!(
        reglages.get("scan_status").unwrap().as_deref(),
        Some("idle")
    );
    let fin = dernier(&mut rx, "library.scan.completed").expect("fin du scan arrêté");
    assert_eq!(fin["cancelled"], true, "{fin}");
    assert!(
        crate::routes::system::scan::reprise_du_scan_demandee(&db),
        "#5531 : le scan arrêté ne doit pas effacer le repère de reprise"
    );

    // 5. Redémarrage SANS `auto_scan` : la reprise est la seule exception.
    assert!(
        crate::auto_scan::scan_au_demarrage(false, &db),
        "#5531 : le repère doit relancer le scan au démarrage, même sans auto_scan"
    );
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    scan_complet_sur(&db, &reglages, &bus, "scan repris").await;
    let depart = {
        // `dernier` vide la file : relire depuis un abonné neuf n'est pas
        // possible, on garde donc le départ en parcourant la file une fois.
        let mut depart = None;
        let mut fin = None;
        while let Some(ev) = super::arret_du_scan_de_demarrage_tests_5552::suivant(&mut rx) {
            match ev.event_type.as_str() {
                "library.scan.started" => depart = Some(ev.data),
                "library.scan.completed" => fin = Some(ev.data),
                _ => {}
            }
        }
        let fin = fin.expect("le scan repris doit aller au bout");
        assert_ne!(
            fin["cancelled"], true,
            "le scan repris ne doit pas être arrêté : {fin}"
        );
        depart.expect("le scan repris doit annoncer son départ")
    };
    // Incrémental : les deux pistes déjà rangées ne sont pas relues.
    assert!(
        depart["unchanged"].as_u64().unwrap_or(0) >= 2,
        "#5531 : la reprise doit sauter les fichiers inchangés (incrémental) : {depart}"
    );
    assert!(
        !crate::routes::system::scan::reprise_du_scan_demandee(&db),
        "#5531 : le scan repris allé au bout doit effacer le repère"
    );
    assert!(
        !crate::auto_scan::scan_au_demarrage(false, &db),
        "repère effacé : sans auto_scan, plus de scan au démarrage"
    );
}
