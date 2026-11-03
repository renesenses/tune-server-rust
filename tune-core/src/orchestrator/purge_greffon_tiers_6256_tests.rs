//! #6256 — désinstaller un greffon natif tiers ne rebâtit que l'étage casque
//! des zones qui l'utilisent. Deux zones locales jouent ; seule la première a
//! le greffon coché. La seconde garde son étage tel quel : le rebâtir ne lui
//! retirerait rien et pourrait s'entendre (un petit claquement).
//!
//! Aucune bibliothèque native n'est chargée et aucun son ne sort : les
//! sorties locales ne sont jamais ouvertes, seul leur format courant est
//! déclaré pour qu'elles passent pour « en lecture ».
use super::PlaybackOrchestrator;
use crate::db::settings_repo::SettingsRepo;
use crate::outputs::local::LocalOutput;
use std::sync::Arc;

fn orchestrateur() -> PlaybackOrchestrator {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    )
}

/// Une zone sur la sortie locale `local:<nom>`, en lecture à 48 kHz, avec un
/// étage casque déjà en place.
async fn zone_qui_joue(orch: &PlaybackOrchestrator, nom: &str) -> i64 {
    let zone_id = crate::db::zone_repo::ZoneRepo::with_backend(orch.db.clone())
        .create(nom, Some("local"), Some(&format!("local:{nom}")))
        .unwrap();
    orch.outputs
        .lock()
        .await
        .register(Box::new(LocalOutput::new(nom.to_string())));
    avec_sortie(orch, nom, |local| {
        local.declare_current_format_for_test(48_000, 2);
        local.set_crossfeed(Some(crate::audio::crossfeed::CrossfeedProcessor::new(
            48_000, 0.3, 0.3,
        )));
    })
    .await;
    zone_id
}

async fn avec_sortie<T>(
    orch: &PlaybackOrchestrator,
    nom: &str,
    f: impl FnOnce(&LocalOutput) -> T,
) -> T {
    let arc = orch
        .outputs
        .lock()
        .await
        .get(&format!("local:{nom}"))
        .unwrap();
    let sortie = arc.lock().await;
    f(sortie.as_any().downcast_ref::<LocalOutput>().unwrap())
}

#[tokio::test]
async fn la_purge_ne_rebatit_que_les_zones_du_greffon_6256() {
    let orch = orchestrateur();
    let id = "greffon-essai-6256-zones";
    let avec = zone_qui_joue(&orch, "AvecGreffon").await;
    let sans = zone_qui_joue(&orch, "SansGreffon").await;
    let s = SettingsRepo::with_backend(orch.db.clone());
    s.set(&format!("plugin_{id}_installed"), "true").unwrap();
    s.set(&format!("plugin_{id}_enabled"), "true").unwrap();
    s.set(
        &crate::audio::natifs_tiers::cle_de_zone(avec, id),
        r#"{"enabled":true}"#,
    )
    .unwrap();
    // Un réglage décoché ne fait pas d'une zone une utilisatrice.
    s.set(
        &crate::audio::natifs_tiers::cle_de_zone(sans, id),
        r#"{"enabled":false}"#,
    )
    .unwrap();

    // Ce que fait la route : la liste est lue avant la désinstallation, puis
    // chaque zone retenue est rebâtie.
    let zones = orch.zones_qui_utilisent_le_greffon_tiers(id).await;
    assert_eq!(
        zones,
        [avec],
        "seule la zone qui a le greffon coché doit être rebâtie"
    );
    s.set(&format!("plugin_{id}_installed"), "false").unwrap();
    for zone in zones {
        assert!(orch.rebatir_l_etage_casque_en_direct(zone).await);
    }

    // La zone du greffon est rebâtie : sans crossfeed réglé ni greffon, son
    // étage casque est vidé.
    assert!(
        !avec_sortie(&orch, "AvecGreffon", |l| l.has_crossfeed()).await,
        "la zone du greffon doit être rebâtie sans lui"
    );
    // L'autre zone n'est pas touchée : son étage casque est toujours là. Si
    // elle avait été rebâtie, il aurait été vidé lui aussi (aucun crossfeed
    // n'est réglé pour elle en base).
    assert!(
        avec_sortie(&orch, "SansGreffon", |l| l.has_crossfeed()).await,
        "une zone qui n'utilise pas le greffon ne doit pas être rebâtie"
    );
}

/// Une zone dont le greffon n'est pas demandé (désactivé) n'est pas retenue
/// par ses réglages ; sans étage vivant qui le porte, aucune ne l'est.
#[tokio::test]
async fn un_greffon_desactive_ne_retient_aucune_zone_par_ses_reglages_6256() {
    let orch = orchestrateur();
    let id = "greffon-essai-6256-desactive";
    let zone = zone_qui_joue(&orch, "Desactive").await;
    let s = SettingsRepo::with_backend(orch.db.clone());
    s.set(&format!("plugin_{id}_installed"), "true").unwrap();
    s.set(&format!("plugin_{id}_enabled"), "false").unwrap();
    s.set(
        &crate::audio::natifs_tiers::cle_de_zone(zone, id),
        r#"{"enabled":true}"#,
    )
    .unwrap();
    assert!(
        orch.zones_qui_utilisent_le_greffon_tiers(id)
            .await
            .is_empty()
    );
}
