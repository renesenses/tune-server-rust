//! #5371 (Tades, fil 2025) — « 438 443 fichiers sur 41 166 — 100 % » pendant
//! une analyse en cours.
//!
//! Le scan de DÉMARRAGE comptait au numérateur de `library.scan.progress`
//! (`scanned = inserted + updated + skipped`) les fichiers inchangés écartés
//! avant lecture (`skipped` part de `pre_skipped`), mais les EXCLUAIT du
//! dénominateur (`total = files_to_scan.len()`). Le scan manuel, lui, les
//! compte des deux côtés.
//!
//! L'épreuve joue le VRAI `spawn_auto_scan` sur une base SQLite de FICHIER
//! (une base `:memory:` rendrait le préfiltre aveugle : tout paraîtrait
//! changé, `pre_skipped` vaudrait 0 et l'épreuve serait verte sans rien
//! garder) : un premier scan indexe un album, un second album est posé, le
//! second scan de démarrage ne relit que lui.
use super::pochettes_disque_tests_5034::album_dans;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::event_bus::EventBus;

/// Un scan de démarrage sur `bus`, jusqu'à sa fin. Il se retire en silence
/// quand un autre essai tient le droit de scanner : `scan_started_at`, posé
/// seulement une fois le droit acquis, dit s'il a vraiment tourné.
async fn scan_de_demarrage_sur(db: &Arc<dyn DbBackend>, bus: &Arc<EventBus>) {
    let reglages = SettingsRepo::with_backend(db.clone());
    let debut = Instant::now();
    loop {
        reglages.set("scan_started_at", "0").unwrap();
        let fini = crate::auto_scan::spawn_auto_scan(db.clone(), bus.clone());
        while !fini.load(Ordering::Acquire) {
            assert!(
                debut.elapsed() < Duration::from_secs(300),
                "scan de démarrage sans fin"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if reglages.get("scan_started_at").unwrap().as_deref() != Some("0") {
            crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
            return;
        }
        assert!(
            debut.elapsed() < Duration::from_secs(300),
            "le droit de scanner n'est jamais revenu"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn le_scan_de_demarrage_compte_les_inchanges_au_total_5371() {
    let base = tune_core::test_scratch::scratch_dir("compteur-5371-base");
    let racine = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        "compteur-5371-musique",
    );
    let etat = crate::state::AppState::new(
        &base.join("tune-epreuve.db").to_string_lossy(),
        0,
        Default::default(),
    )
    .expect("AppState sur base de fichier");
    let db = etat.backend.clone();
    SettingsRepo::with_backend(db.clone())
        .set(
            "music_dirs",
            &serde_json::to_string(&[racine.to_string_lossy()]).unwrap(),
        )
        .unwrap();

    // Premier démarrage : l'album de Tades, deux pistes, indexé.
    album_dans(&racine, "Deja indexe", None, None);
    scan_de_demarrage_sur(&db, &Arc::new(EventBus::new())).await;

    // Un second album arrive ; au démarrage suivant, deux fichiers sont
    // inchangés (écartés avant lecture) et deux sont neufs.
    album_dans(&racine, "Nouveau", None, None);
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    scan_de_demarrage_sur(&db, &bus).await;

    let mut progressions = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if ev.event_type == "library.scan.progress" && ev.data["phase"] == "files" {
            progressions.push(ev.data);
        }
    }
    assert!(
        !progressions.is_empty(),
        "le scan de démarrage n'a émis aucune progression `files`"
    );
    for p in &progressions {
        let scanned = p["scanned"].as_i64().expect("scanned");
        let total = p["total"].as_i64().expect("total");
        assert!(
            scanned <= total,
            "#5371 : {scanned} fichiers sur {total} — le numérateur dépasse le total : {p}"
        );
        assert_eq!(
            total, 4,
            "#5371 : le total doit compter les 2 fichiers inchangés ET les 2 neufs : {p}"
        );
    }
    let dernier = progressions.last().unwrap();
    assert_eq!(
        dernier["skipped"].as_i64(),
        Some(2),
        "les 2 fichiers inchangés sont comptés écartés : {dernier}"
    );
}
