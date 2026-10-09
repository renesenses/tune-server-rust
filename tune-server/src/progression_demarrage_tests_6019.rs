//! #6019 (Tades, fil 2195, Tune OS sur Fedora) — pendant une analyse, l'écran
//! Réglages › Bibliothèque n'affiche que le badge « analyse en cours », sans
//! aucun chiffre, alors que le serveur Windows montre « Parcours des
//! dossiers : 31 755 fichiers repérés ».
//!
//! Le badge vient du sondage de `scan/status` ; les chiffres, du SEUL
//! évènement `library.scan.progress`. Le scan MANUEL annonce sa phase
//! `indexing` dès le départ puis pendant le parcours (#2203). Le scan de
//! DÉMARRAGE — celui que Tune OS lance à chaque mise sous tension — parcourait
//! puis `stat`ait toute la bibliothèque sans rien émettre : sur trois montages
//! réseau, plusieurs minutes de badge sans chiffre. Pire, une bibliothèque
//! inchangée n'émettait AUCUNE progression de tout le scan.
//!
//! L'épreuve joue le VRAI `spawn_auto_scan` sur une base de FICHIER (une base
//! `:memory:` rendrait le préfiltre aveugle), au second démarrage d'une
//! bibliothèque déjà indexée et inchangée.
//!
//! Contre-épreuve : remettre `&mut |_| {}` au parcours et retirer les deux
//! émissions `indexing` de `spawn_auto_scan` fait tomber
//! `le_scan_de_demarrage_annonce_son_parcours_6019`.
use super::pochettes_disque_tests_5034::album_dans;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::event_bus::EventBus;

/// Un scan de démarrage sur `bus`, jusqu'à sa fin — même attente que #5371 :
/// il se retire en silence quand un autre essai tient le droit de scanner.
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
async fn le_scan_de_demarrage_annonce_son_parcours_6019() {
    let base = tune_core::test_scratch::scratch_dir("progression-6019-base");
    let racine = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        "progression-6019-musique",
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

    // Premier démarrage : un album de deux pistes, indexé.
    album_dans(&racine, "Jolivet", None, None);
    scan_de_demarrage_sur(&db, &Arc::new(EventBus::new())).await;

    // Second démarrage, rien n'a changé : le cas de chaque mise sous tension.
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe();
    scan_de_demarrage_sur(&db, &bus).await;

    let mut indexation = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if ev.event_type == "library.scan.progress" && ev.data["phase"] == "indexing" {
            indexation.push(ev.data);
        }
    }
    assert!(
        !indexation.is_empty(),
        "#6019 : le scan de démarrage n'a annoncé aucune progression `indexing` — \
         l'écran n'affiche que le badge « analyse en cours », sans chiffre"
    );
    assert!(
        indexation.iter().any(|p| p["scanned"].as_i64() == Some(2)),
        "#6019 : les 2 fichiers repérés par le parcours doivent être annoncés \
         avant la passe `stat` : {indexation:?}"
    );
}
