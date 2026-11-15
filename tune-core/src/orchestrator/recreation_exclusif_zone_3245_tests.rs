//! #3245 — la sortie locale RECRÉÉE à la volée suit le réglage de sa zone.
//!
//! #8348 a branché le réglage `zone_<id>_exclusive_mode` aux deux sites qui
//! enregistrent une sortie locale (démarrage, rescan), pas au troisième :
//! `recreate_local_and_play`, qui reconstruit la sortie quand elle manque au
//! registre (appareil éteint au démarrage, retiré puis retrouvé). Là, seul le
//! réglage de MACHINE était lu : la case cochée pour le DAC d'une zone
//! s'imposait à la sortie d'une autre zone réglée « partagé ».

use std::sync::Arc;

use tokio::sync::Mutex;

use super::PlaybackOrchestrator;
use crate::db::migrations::run_migrations;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;

fn orchestrateur() -> PlaybackOrchestrator {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    PlaybackOrchestrator::new(
        db,
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    )
}

/// Machine en exclusif (case cochée pour le DAC), deux zones locales : l'une
/// réglée « partagé », l'autre sans réglage propre.
fn deux_zones(orch: &PlaybackOrchestrator) -> (i64, i64) {
    let reglages = SettingsRepo::with_backend(orch.db.clone());
    reglages.set("local_audio_backend", "wasapi").unwrap();
    reglages.set("local_exclusive_mode", "true").unwrap();
    let zones = ZoneRepo::with_backend(orch.db.clone());
    let partagee = zones
        .create("Visio", Some("local"), Some("local:Haut-parleurs"))
        .unwrap();
    let dac = zones
        .create("DAC", Some("local"), Some("local:Pontus"))
        .unwrap();
    reglages
        .set(&crate::config::cle_exclusif_de_zone(partagee), "false")
        .unwrap();
    (partagee, dac)
}

#[test]
fn la_zone_reglee_partagee_l_emporte_sur_la_machine_a_la_recreation() {
    let orch = orchestrateur();
    let _ = deux_zones(&orch);
    let (exclusif_machine, backend) = orch.reglages_sortie_locale_avec(|_| None);
    assert!(exclusif_machine, "le banc suppose la machine en exclusif");

    let visio =
        orch.exclusif_de_la_sortie_recreee("local:Haut-parleurs", &backend, exclusif_machine);
    assert!(
        !visio.demande,
        "la zone « Visio » est réglée en partagé : sa sortie recréée ne doit \
         pas reprendre l'exclusif de la machine (#3245)"
    );
    assert!(visio.par_la_zone);

    let dac = orch.exclusif_de_la_sortie_recreee("local:Pontus", &backend, exclusif_machine);
    assert!(
        dac.demande && !dac.par_la_zone,
        "sans réglage de zone, la machine décide comme avant"
    );

    let inconnue =
        orch.exclusif_de_la_sortie_recreee("local:Sans zone", &backend, exclusif_machine);
    assert_eq!(
        (inconnue.demande, inconnue.par_la_zone),
        (true, false),
        "un appareil sans zone suit la machine"
    );
}

#[test]
fn une_zone_reglee_exclusive_l_obtient_meme_si_la_machine_est_partagee() {
    let orch = orchestrateur();
    let (_, dac) = deux_zones(&orch);
    let reglages = SettingsRepo::with_backend(orch.db.clone());
    reglages.set("local_exclusive_mode", "false").unwrap();
    reglages
        .set(&crate::config::cle_exclusif_de_zone(dac), "true")
        .unwrap();
    let (exclusif_machine, backend) = orch.reglages_sortie_locale_avec(|_| None);
    assert!(!exclusif_machine);
    let d = orch.exclusif_de_la_sortie_recreee("local:Pontus", &backend, exclusif_machine);
    assert!(d.demande && d.par_la_zone);
}

/// La contrainte de plateforme n'est pas levée par un réglage de zone : ASIO
/// reste exclusif par nature sous Windows (et seulement là, #1268).
#[test]
fn asio_reste_exclusif_par_nature_sous_windows() {
    let d = super::commun::exclusif_recree_selon_la_zone("asio", Some("false"), true);
    assert!(d.par_la_zone);
    assert_eq!(d.demande, cfg!(target_os = "windows"));
    let d = super::commun::exclusif_recree_selon_la_zone("wasapi", Some("false"), true);
    assert!(!d.demande);
    let d = super::commun::exclusif_recree_selon_la_zone("wasapi", None, true);
    assert!(d.demande && !d.par_la_zone);
}

/// Le vrai chemin : `recreate_local_and_play` construit et enregistre la
/// sortie. Son mode exclusif doit être celui de la ZONE.
#[cfg(feature = "local-audio")]
#[tokio::test]
async fn la_sortie_recreee_porte_le_mode_de_sa_zone() {
    let orch = orchestrateur();
    let (partagee, dac) = deux_zones(&orch);
    let media = crate::outputs::traits::PlayMedia {
        url: "http://127.0.0.1:9/absent-3245.flac",
        mime_type: "audio/flac",
        ..Default::default()
    };
    for (zone_id, device_id, attendu) in [
        (partagee, "local:Haut-parleurs", false),
        (dac, "local:Pontus", true),
    ] {
        // L'appareil n'existe pas : la lecture échoue, la sortie est
        // enregistrée AVANT l'essai, c'est elle qu'on lit.
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            orch.recreate_local_and_play(zone_id, device_id, &media, None),
        )
        .await;
        let sortie = orch
            .outputs
            .lock()
            .await
            .get(device_id)
            .expect("la sortie recréée est au registre");
        let sortie = sortie.lock().await;
        let locale = sortie
            .as_any()
            .downcast_ref::<crate::outputs::local::LocalOutput>()
            .expect("sortie locale");
        assert_eq!(
            locale.mode_exclusif(),
            attendu,
            "{device_id} : la sortie recréée doit porter le mode de sa zone (#3245)"
        );
    }
}
