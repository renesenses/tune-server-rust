//! #6729 (Tades, fil 2368, message 8842) — « analyse en cours » restait
//! affiché après la fin d'un scan ciblé. Le geste à reproduire : arrêter les
//! scans de bibliothèque en cours, PUIS lancer le scan ciblé d'un dossier.
//!
//! L'épreuve joue le VRAI scan de démarrage, la VRAIE route `scan/cancel`,
//! puis le VRAI scan ciblé (`spawn_library_scan` avec un chemin, le même que
//! `POST /system/scan?path=`). Après la fin annoncée du scan ciblé, ni le
//! réglage `scan_status`, ni le droit de scanner (ce que `scan/status` publie)
//! ne doivent plus dire « scanning », et aucun autre scan ne doit être reparti.
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::event_bus::EventBus;

use super::arret_du_scan_de_demarrage_tests_5552::{demarrer_jusqu_a_la_lecture, suivant};

fn bibliotheque(racine: &std::path::Path) -> std::path::PathBuf {
    for d in 0..3 {
        let dossier = racine.join(format!("Dossier {d}"));
        std::fs::create_dir_all(&dossier).unwrap();
        for f in 0..400 {
            std::fs::write(dossier.join(format!("{f:04}.flac")), b"pas du flac").unwrap();
        }
    }
    let cible = racine.join("Billy Joel");
    std::fs::create_dir_all(&cible).unwrap();
    for f in 0..12 {
        std::fs::write(cible.join(format!("{f:02}.flac")), b"pas du flac").unwrap();
    }
    cible
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[allow(clippy::await_holding_lock)]
async fn arreter_puis_scanner_un_dossier_laisse_le_statut_au_repos_6729() {
    let _seul = crate::routes::system::scan::serialiser_les_scans_de_test_sans_bloquer().await;
    crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;

    let base = tune_core::test_scratch::scratch_dir("arret-cible-6729-base");
    let racine = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        "arret-cible-6729-musique",
    );
    let cible = bibliotheque(racine.path());
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

    // 1. Le scan de démarrage tourne ; « Arrêter », par la vraie route.
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    let (fini, porte) = demarrer_jusqu_a_la_lecture(&db, &bus, &mut rx).await;
    // Le statut nomme le scan qui tient le droit : celui du démarrage.
    let en_cours = crate::routes::system::scan::decrire_le_scan_en_cours("scanning");
    assert_eq!(en_cours["origin"], "startup", "{en_cours}");
    let _ = crate::routes::system::scan::scan_cancel(axum::extract::State(etat.clone())).await;
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
    while suivant(&mut rx).is_some() {}

    // 2. Le scan ciblé du dossier, jusqu'à sa fin annoncée.
    let mut rx_etat = etat.event_bus.subscribe();
    let chemin = tune_core::scanner::walker::normalize_path(&cible.to_string_lossy());
    assert!(
        crate::routes::system::scan::spawn_library_scan(etat.clone(), false, Some(chemin)).await,
        "le scan ciblé doit partir une fois le scan de démarrage arrêté"
    );
    let fin = tune_core::event_types::EventType::ScanComplete.as_str();
    let rapport = loop {
        match tokio::time::timeout(Duration::from_secs(120), rx_etat.recv())
            .await
            .expect("le scan ciblé n'a pas annoncé sa fin")
        {
            Ok(ev) if ev.event_type == fin => break ev.data,
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(e) => panic!("bus fermé : {e}"),
        }
    };
    assert_ne!(
        rapport["cancelled"], true,
        "le scan ciblé ne doit pas hériter de l'arrêt : {rapport}"
    );
    crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;

    // 3. Le statut publié est au repos, et le reste : plus aucun détenteur.
    assert!(
        crate::routes::system::scan::decrire_le_scan_en_cours("idle").is_null(),
        "#6729 : un scan est encore annoncé après la fin du scan ciblé"
    );
    assert_eq!(
        reglages.get("scan_status").unwrap().as_deref(),
        Some("idle"),
        "#6729 : `scan_status` dit encore « scanning » après la fin du scan ciblé"
    );
    assert!(
        !crate::routes::system::scan::droit_de_scanner_tenu(),
        "#6729 : un scan tient encore le droit après la fin du scan ciblé"
    );
}
