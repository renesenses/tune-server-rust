//! #5919 / #5371 — « analyse rapide systématique à chaque démarrage ».
//!
//! Rapport du 06/10/2026 (1.0.0-rc2, Linux) : `auto_scan_pre_filter_complete
//! total=17659 changed=2217` à chaque démarrage, et les fichiers relus étaient
//! ceux d'un coffret que le scan venait de reclasser en compilation
//! (`album_reclasse_en_compilation … ancien_titre=Some("Disc 13")`).
//!
//! Cause : l'index plein texte retirait l'ancienne ligne d'une piste par ses
//! VALEURS, recalculées au moment du retrait ; un album renommé rendait un
//! titre jamais indexé, SQLite répondait « database disk image is
//! malformed », et l'`UPDATE` de la piste échouait — `file_mtime` jamais
//! écrit, fichier « changé » au démarrage suivant, pour toujours.
//!
//! Épreuve sur le VRAI scan de démarrage, vrais FLAC, base de FICHIER.
use super::pochettes_disque_tests_5034::{album_dans, racine};
use crate::state::AppState;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::event_bus::EventBus;

/// Un scan de démarrage qui a VRAIMENT tourné ; rend son `library.scan.started`.
async fn scan_de_demarrage(db: &Arc<dyn DbBackend>) -> serde_json::Value {
    let reglages = SettingsRepo::with_backend(db.clone());
    let debut = Instant::now();
    loop {
        reglages.set("scan_started_at", "0").unwrap();
        let bus = Arc::new(EventBus::new());
        let mut rx = bus.subscribe();
        let fini = crate::auto_scan::spawn_auto_scan(db.clone(), bus);
        while !fini.load(Ordering::Acquire) {
            assert!(debut.elapsed() < Duration::from_secs(300), "scan sans fin");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
        if reglages.get("scan_started_at").unwrap().as_deref() != Some("0") {
            let mut depart = None;
            while let Some(ev) = super::arret_du_scan_de_demarrage_tests_5552::suivant(&mut rx) {
                if ev.event_type == "library.scan.started" {
                    depart = Some(ev.data);
                }
            }
            return depart.expect("le scan a tourné sans annoncer son départ");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_album_renomme_n_est_pas_relu_a_chaque_demarrage_5919() {
    let r = racine("album-renomme-5919");
    let musique = r.join("musique");
    let (_, pistes) = album_dans(&musique, "Disc 13", None, None);
    let etat = AppState::new(&r.join("tune.db").to_string_lossy(), 0, Default::default()).unwrap();
    let db = etat.backend.clone();
    SettingsRepo::with_backend(db.clone())
        .set(
            "music_dirs",
            &serde_json::to_string(&[musique.to_string_lossy()]).unwrap(),
        )
        .unwrap();
    let premier = scan_de_demarrage(&db).await;
    assert_eq!(
        premier["to_scan"], 2,
        "premier scan : tout est neuf : {premier}"
    );

    // Le geste du reclassement en compilation (ou d'un coffret composé, d'une
    // édition) : l'album change de titre, ses pistes restent.
    db.execute_batch("UPDATE albums SET title = 'The History Of Classical Music'")
        .unwrap();
    // Les fichiers sont retouchés : le scan suivant doit les relire, et
    // ÉCRIRE leur nouvelle date.
    let retouche = SystemTime::now() - Duration::from_secs(3_600);
    for p in &pistes {
        std::fs::File::options()
            .write(true)
            .open(p)
            .unwrap()
            .set_modified(retouche)
            .unwrap();
    }
    let relu = scan_de_demarrage(&db).await;
    assert_eq!(relu["to_scan"], 2, "les deux fichiers retouchés : {relu}");
    let repo = TrackRepo::with_backend(db.clone());
    let disque = tune_core::audio::iso9660::taille_et_mtime(&pistes[0])
        .unwrap()
        .1;
    assert_eq!(
        repo.get_by_path(&pistes[0].to_string_lossy())
            .unwrap()
            .and_then(|t| t.file_mtime),
        Some(disque),
        "#5919 : la date relue doit être ÉCRITE, même quand l'album a changé de titre"
    );

    let ensuite = scan_de_demarrage(&db).await;
    assert_eq!(
        ensuite["to_scan"], 0,
        "#5919 : plus rien à relire au démarrage suivant : {ensuite}"
    );
}
