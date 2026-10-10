//! Ticket 201 (LANDES Philippe, fil 2063, #5552) — 20 284 fichiers sur 20 348
//! relus au démarrage. Depuis #5223 (v0.9.167) la date de modification est
//! comparée avec sa fraction de seconde ; les lignes écrites avant portent une
//! date tronquée à la seconde, et tout fichier dont la date a une fraction
//! paraissait modifié.
//!
//! Décision de Bertrand (30/09/2026) : une date enregistrée SANS fraction,
//! égale à la partie entière de la date du disque, à taille égale, est tenue
//! pour inchangée, et la date précise est réécrite sans relire le fichier.
//! Toute autre différence déclenche la relecture.
//!
//! Épreuve sur le VRAI scan de démarrage, vrais FLAC, base de fichier.
use super::pochettes_disque_tests_5034::{album_dans, racine};
use crate::state::AppState;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;
use tune_core::event_bus::EventBus;

/// Date du disque, fraction comprise : 1 750 000 000,25 s.
const SECONDES: u64 = 1_750_000_000;
const PRECISE: f64 = 1_750_000_000.25;

fn dater(path: &Path) {
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(
            SystemTime::UNIX_EPOCH + Duration::from_secs(SECONDES) + Duration::from_millis(250),
        )
        .unwrap();
}

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
async fn une_date_tronquee_a_taille_egale_n_est_pas_relue_et_devient_precise_5552() {
    let r = racine("date-arrondie-5552");
    let musique = r.join("musique");
    let (_, a) = album_dans(&musique, "Arrondie", None, None);
    let (_, b) = album_dans(&musique, "Seconde", None, None);
    for f in a.iter().chain(b.iter()) {
        dater(f);
    }
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
        premier["to_scan"], 4,
        "premier scan : tout est neuf : {premier}"
    );

    let repo = TrackRepo::with_backend(db.clone());
    let ligne = |p: &Path| repo.get_by_path(&p.to_string_lossy()).unwrap().unwrap();
    let taille = |p: &Path| std::fs::metadata(p).unwrap().len() as i64;
    let tronquee = SECONDES as f64;
    // 1. date tronquée, même taille : la ligne d'une version ≤ 0.9.166.
    repo.update_mtime_and_size(&a[0].to_string_lossy(), tronquee, taille(&a[0]))
        .unwrap();
    // 2. date tronquée, taille différente.
    repo.update_mtime_and_size(&a[1].to_string_lossy(), tronquee, taille(&a[1]) + 1)
        .unwrap();
    // 3. vraie différence d'une seconde (entière elle aussi), même taille.
    repo.update_mtime_and_size(&b[0].to_string_lossy(), tronquee - 1.0, taille(&b[0]))
        .unwrap();
    // 4. b[1] : inchangée, date précise — le témoin.

    let depart = scan_de_demarrage(&db).await;
    assert_eq!(
        depart["to_scan"], 2,
        "ticket 201 : seules la taille différente et la vraie seconde d'écart se relisent \
         (une date tronquée à taille égale ne doit PAS être relue) : {depart}"
    );
    assert_eq!(depart["unchanged"], 2, "{depart}");
    assert_eq!(
        ligne(&a[0]).file_mtime,
        Some(PRECISE),
        "ticket 201 : la date tronquée doit être réécrite précise, sans relecture"
    );
    // Relues : leur ligne porte la date et la taille du disque.
    assert_eq!(ligne(&a[1]).file_size, Some(taille(&a[1])));
    assert_eq!(ligne(&b[0]).file_mtime, Some(PRECISE));

    // Le scan suivant ne relit plus rien.
    let ensuite = scan_de_demarrage(&db).await;
    assert_eq!(ensuite["to_scan"], 0, "{ensuite}");
}
