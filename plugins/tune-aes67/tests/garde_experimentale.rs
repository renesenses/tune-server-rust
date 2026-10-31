//! Garde-fou EXPÉRIMENTAL de la Sortie AES67 (#7336, décision de Bertrand du
//! 31/10/2026 : la rc10 l'embarque en expérimental, DÉSACTIVÉE par défaut).
//!
//! Le réglage « Expérimental › Sortie AES67 (Dante/Ravenna) » du client web
//! pose `plugin_aes67_installed` (route `POST /api/v1/plugins/aes67/install`)
//! et l'ôte (`DELETE /api/v1/plugins/aes67`). Tant qu'il n'est pas coché, la
//! porte de démarrage du chargeur laisse le greffon DORMANT : son `setup`
//! n'est jamais appelé, donc aucun socket PTP, SAP ni RTP, aucun fil, aucune
//! sortie, aucune zone, aucune route.
//!
//! Ce fichier ne contient qu'UN test, exprès : il compte les sockets et les
//! fils du processus (`/proc/self`), et un autre test lancé en parallèle dans
//! le même binaire fausserait le compte.

use std::sync::Arc;

use tune_aes67::Aes67Plugin;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::LicenseManager;
use tune_core::plugin_sdk::PluginLoader;

fn base() -> Arc<dyn DbBackend> {
    let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Sockets ouverts par le processus (Linux : `/proc/self/fd` → `socket:[…]`).
#[cfg(target_os = "linux")]
fn sockets_ouverts() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| std::fs::read_link(e.path()).ok())
        .filter(|cible| cible.to_string_lossy().starts_with("socket:"))
        .count()
}

/// Noms des fils du processus qui appartiennent au greffon (`aes67-…`).
#[cfg(target_os = "linux")]
fn fils_aes67() -> Vec<String> {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| std::fs::read_to_string(e.path().join("comm")).ok())
        .map(|n| n.trim().to_string())
        .filter(|n| n.starts_with("aes67"))
        .collect()
}

async fn chargeur(db: &Arc<dyn DbBackend>, dossier: &std::path::Path) -> PluginLoader {
    let licence = Arc::new(LicenseManager::new(db.clone()));
    licence.set_account_premium(true, None).await;
    let loader = PluginLoader::new(dossier.to_path_buf())
        .with_db(db.clone())
        .with_license(licence);
    loader.register(Box::new(Aes67Plugin::new())).await;
    loader
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desactivee_par_defaut_aucun_socket_aucun_fil_aucune_sortie() {
    let dossier = tempfile::tempdir().unwrap();
    let db = base();
    // Un flux est même déclaré : s'il suffisait à réveiller le greffon, la
    // garde ne tiendrait qu'à l'absence de flux.
    let reglages = SettingsRepo::with_backend(db.clone());
    reglages
        .set(
            "plugin_aes67_flux",
            r#"[{"id":"salon","nom":"Salon","adresse":"239.69.1.10"}]"#,
        )
        .unwrap();

    #[cfg(target_os = "linux")]
    let avant = sockets_ouverts();

    // 1. Réglage NON coché (base neuve) : dormant.
    let loader = chargeur(&db, dossier.path()).await;
    let charges = loader.setup_all("http://127.0.0.1:1").await;
    assert!(
        !charges.iter().any(|n| n == "aes67"),
        "désactivée par défaut, la sortie AES67 ne doit pas se charger : {charges:?}"
    );
    let reg = loader.take_registrations();
    assert!(
        reg.output_providers.is_empty() && reg.outputs.is_empty() && reg.zones.is_empty(),
        "désactivée : aucune sortie ni zone"
    );
    assert!(reg.routers.is_empty(), "désactivée : aucune route");
    #[cfg(target_os = "linux")]
    {
        assert_eq!(sockets_ouverts(), avant, "désactivée : aucun socket ouvert");
        assert!(fils_aes67().is_empty(), "désactivée : aucun fil aes67");
        // Contre-épreuve de l'instrument : un socket ouvert SE VOIT.
        let temoin = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        assert_eq!(
            sockets_ouverts(),
            avant + 1,
            "le compteur de sockets voit un socket"
        );
        drop(temoin);
        assert_eq!(sockets_ouverts(), avant);
    }

    // 2. Décoché explicitement après avoir été coché (`enabled=false`) : dormant.
    reglages.set("plugin_aes67_installed", "true").unwrap();
    reglages.set("plugin_aes67_enabled", "false").unwrap();
    let loader = chargeur(&db, dossier.path()).await;
    let charges = loader.setup_all("http://127.0.0.1:1").await;
    assert!(!charges.iter().any(|n| n == "aes67"), "{charges:?}");
    assert!(loader.take_registrations().is_empty());

    // 3. CONTRE-ÉPREUVE : réglage coché → le greffon se charge et déclare sa
    // sortie et ses routes. PTP coupé ici : ses ports 319/320 sont
    // privilégiés et le test ne tourne pas en root.
    reglages.set("plugin_aes67_enabled", "true").unwrap();
    reglages.set("plugin_aes67_ptp_actif", "false").unwrap();
    let loader = chargeur(&db, dossier.path()).await;
    let charges = loader.setup_all("http://127.0.0.1:1").await;
    assert!(
        charges.iter().any(|n| n == "aes67"),
        "cochée, la sortie AES67 doit se charger : {charges:?}"
    );
    let reg = loader.take_registrations();
    assert_eq!(
        reg.output_providers.len(),
        1,
        "cochée : le fournisseur de sorties est là"
    );
    assert_eq!(
        reg.routers.len(),
        1,
        "cochée : les routes /api/v1/ext/aes67 sont là"
    );
    assert_eq!(reg.routers[0].0, "aes67");
}
