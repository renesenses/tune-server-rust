//! #8437 (Jon, fil 2757) — de bout en bout dans l'orchestrateur : une zone
//! LOCALE groupée avec une enceinte AirPlay confie sa piste à sa sortie avec
//! son `sync_delay_ms` comme retard de départ. Sans réglage, ou hors groupe
//! mixte, la sortie part tout de suite (comportement d'avant). Aucun son :
//! l'URL ne répond pas, la sortie n'ouvre aucun périphérique.

use std::sync::Arc;

use tokio::sync::Mutex;

use super::PlaybackOrchestrator;
use crate::db::migrations::run_migrations;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::AudioStreamer;
use crate::outputs::local::LocalOutput;
use crate::outputs::registry::OutputRegistry;
use crate::outputs::traits::{OutputTarget, PlayMedia};
use crate::playback::PlaybackManager;
use crate::streaming::registry::ServiceRegistry;

const NOM: &str = "DAC+ 8437";
const LOCALE: &str = "local:DAC+ 8437";
const AIRPLAY: &str = "airplay2:salon-8437";
const URL_MORTE: &str = "http://127.0.0.1:9/8437.wav";

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

fn piste() -> PlayMedia<'static> {
    PlayMedia {
        url: URL_MORTE,
        mime_type: "audio/wav",
        title: Some("Piste 8437"),
        duration_ms: Some(180_000),
        sample_rate: Some(44_100),
        bit_depth: Some(16),
        channels: Some(2),
        ..Default::default()
    }
}

/// Le Pi de Jon : zone locale (DAC+) au réglage `delai_ms`, zone AirPlay, et
/// le groupe des deux si `groupees`. Rend le retard de départ que la sortie
/// locale a reçu pour la piste.
async fn retard_recu(delai_ms: i32, groupees: bool) -> u64 {
    let orch = orchestrateur();
    let zones = ZoneRepo::with_backend(orch.db.clone());
    let pi = zones
        .create("Pi DAC+", Some("local"), Some(LOCALE))
        .unwrap();
    let salon = zones
        .create("Salon AirPlay", Some("airplay2"), Some(AIRPLAY))
        .unwrap();
    zones.update_sync_delay(pi, delai_ms).unwrap();
    if groupees {
        SettingsRepo::with_backend(orch.db.clone())
            .set(
                "zone_groups",
                &serde_json::json!([{ "id": 1, "leader_id": pi, "zone_ids": [pi, salon] }])
                    .to_string(),
            )
            .unwrap();
    }
    orch.outputs
        .lock()
        .await
        .register(Box::new(LocalOutput::with_options(
            NOM.into(),
            false,
            "auto",
        )));

    let (envoyee, erreur) = orch
        .send_to_output(LOCALE, &piste(), None, false, pi, None)
        .await;
    assert!(envoyee, "la piste doit partir : {erreur:?}");

    let arc = orch.outputs.lock().await.get(LOCALE).unwrap();
    let sortie = arc.lock().await;
    let locale = sortie.as_any().downcast_ref::<LocalOutput>().unwrap();
    let retard = locale.retard_de_depart_ms();
    locale.stop().await.ok();
    retard
}

/// ⭐ Le cas de Jon : 2000 ms réglés sur la zone du Pi, groupée avec
/// l'AirPlay. La sortie locale part 2000 ms plus tard.
#[tokio::test]
async fn la_sortie_locale_d_un_groupe_mixte_part_avec_son_decalage() {
    assert_eq!(retard_recu(2000, true).await, 2000);
}

/// Contre-épreuve : sans réglage, le groupe mixte part comme avant.
#[tokio::test]
async fn sans_reglage_la_sortie_locale_part_tout_de_suite() {
    assert_eq!(retard_recu(0, true).await, 0);
}

/// Contre-épreuve : hors groupe, le réglage ne retarde rien.
#[tokio::test]
async fn hors_groupe_le_reglage_ne_retarde_rien() {
    assert_eq!(retard_recu(2000, false).await, 0);
}
