//! Décision de Bertrand du 19/11/2026 (étude de la spec de Rémi, § B.2) — un
//! changement de réglage du suréchantillonneur s'applique tout de suite : le
//! flux qui joue est relancé à la position en cours, avec un plancher propre
//! à ce geste, différé plutôt qu'abandonné.
//!
//! Sorties factices, aucun son : la relance elle-même (arrêt, `play` à la
//! position) est celle de la bascule PURE (#8422), déjà éprouvée ; ces
//! témoins tiennent la DÉCISION de relancer et le plancher.

use super::PlaybackOrchestrator;
use super::dsp::{PorteeDuReglage, RaisonDeRelance};
use crate::audio::surechantillonnage as su;
use crate::db::settings_repo::SettingsRepo;
use std::sync::Arc;

const SORTIE: &str = "DAC USB 19-11";

fn orchestrateur() -> Arc<PlaybackOrchestrator> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
    Arc::new(PlaybackOrchestrator::new(
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
    ))
}

fn verdict() -> su::VerdictMachine {
    let mut results = Vec::new();
    for f in su::greffon::OFFERED_FILTERS {
        for (de, vers) in [(44_100, 352_800), (48_000, 384_000)] {
            results.push(su::MesureMachine {
                filter: f.to_string(),
                from_hz: de,
                to_hz: vers,
                realtime_factor: 10.0,
                admitted: true,
            });
        }
    }
    su::VerdictMachine {
        simd_path: "test".into(),
        margin: 3.0,
        results,
        measured_at: 0,
    }
}

/// Une zone sur une sortie locale enregistrée, greffon installé et machine
/// mesurée, réglage DÉSACTIVÉ au départ (la licence absente vaut Premium).
async fn zone_locale(orch: &PlaybackOrchestrator) -> i64 {
    let zone_id = crate::db::zone_repo::ZoneRepo::with_backend(orch.db.clone())
        .create("Salon", Some("local"), Some(&format!("local:{SORTIE}")))
        .unwrap();
    orch.outputs
        .lock()
        .await
        .register(Box::new(crate::outputs::local::LocalOutput::new(
            SORTIE.to_string(),
        )));
    let s = SettingsRepo::with_backend(orch.db.clone());
    s.set("plugin_upsampler_installed", "true").unwrap();
    s.set("plugin_upsampler_enabled", "true").unwrap();
    s.set(crate::audio::premium_plugins::MIGRATION, "complete")
        .unwrap();
    su::enregistrer_le_verdict(&s, &verdict()).unwrap();
    zone_id
}

fn regler(orch: &PlaybackOrchestrator, zone_id: i64, reglage: serde_json::Value) {
    su::enregistrer_les_reglages(
        &SettingsRepo::with_backend(orch.db.clone()),
        zone_id,
        &reglage,
    )
    .unwrap();
}

async fn jouer(orch: &PlaybackOrchestrator, zone_id: i64) {
    orch.playback
        .play(
            zone_id,
            crate::playback::NowPlaying {
                title: "Piste".into(),
                source: "local".into(),
                duration_ms: 240_000,
                ..Default::default()
            },
        )
        .await;
}

fn relances_programmees(orch: &PlaybackOrchestrator, zone_id: i64) -> u64 {
    orch.eq_replay_gen
        .lock()
        .unwrap()
        .get(&zone_id)
        .copied()
        .unwrap_or(0)
}

#[tokio::test]
async fn activer_pendant_la_lecture_relance_le_flux_tout_de_suite() {
    let orch = orchestrateur();
    let zone_id = zone_locale(&orch).await;
    orch.refresh_zone_upsampler(zone_id).await;
    jouer(&orch, zone_id).await;

    regler(&orch, zone_id, serde_json::json!({"enabled": true}));
    let portee = orch.apply_upsampler_change_portee(zone_id).await;

    assert_eq!(portee, PorteeDuReglage::Relance);
    assert_eq!(
        relances_programmees(&orch, zone_id),
        1,
        "une relance programmée"
    );

    // Et dans l'autre sens : désactiver relance aussi (comparaison A/B).
    regler(&orch, zone_id, serde_json::json!({"enabled": false}));
    assert_eq!(
        orch.apply_upsampler_change_portee(zone_id).await,
        PorteeDuReglage::Relance
    );
    assert_eq!(relances_programmees(&orch, zone_id), 2);
}

#[tokio::test]
async fn changer_de_filtre_relance_et_reecrire_le_meme_reglage_ne_relance_pas() {
    let orch = orchestrateur();
    let zone_id = zone_locale(&orch).await;
    regler(&orch, zone_id, serde_json::json!({"enabled": true}));
    orch.refresh_zone_upsampler(zone_id).await;
    jouer(&orch, zone_id).await;

    regler(
        &orch,
        zone_id,
        serde_json::json!({"enabled": true, "filter": "precision"}),
    );
    assert_eq!(
        orch.apply_upsampler_change_portee(zone_id).await,
        PorteeDuReglage::Relance
    );
    assert_eq!(relances_programmees(&orch, zone_id), 1);

    // Le même réglage, réécrit : rien ne change dans le son, aucune coupure.
    assert_eq!(
        orch.apply_upsampler_change_portee(zone_id).await,
        PorteeDuReglage::Immediate
    );
    assert_eq!(
        relances_programmees(&orch, zone_id),
        1,
        "aucune relance de plus"
    );
}

#[tokio::test]
async fn rien_ne_joue_ou_zone_en_pause_ne_relance_jamais() {
    let orch = orchestrateur();
    let zone_id = zone_locale(&orch).await;
    regler(&orch, zone_id, serde_json::json!({"enabled": true}));
    assert_eq!(
        orch.apply_upsampler_change_portee(zone_id).await,
        PorteeDuReglage::RienNeJoue,
        "rien ne joue : la prochaine lecture partira avec ce réglage"
    );

    jouer(&orch, zone_id).await;
    orch.playback.pause(zone_id).await;
    regler(&orch, zone_id, serde_json::json!({"enabled": false}));
    assert_eq!(
        orch.apply_upsampler_change_portee(zone_id).await,
        PorteeDuReglage::PisteSuivante,
        "#7556 : une relance remettrait la zone en lecture"
    );
    assert_eq!(relances_programmees(&orch, zone_id), 0);
}

#[test]
fn le_surechantillonneur_a_son_plancher_court_et_differe() {
    assert_eq!(
        RaisonDeRelance::Surechantillonnage.plancher_ms(),
        PlaybackOrchestrator::UPSAMPLER_REPLAY_FLOOR_MS
    );
    assert!(
        PlaybackOrchestrator::UPSAMPLER_REPLAY_FLOOR_MS < PlaybackOrchestrator::EQ_REPLAY_FLOOR_MS,
        "un A/B rapide n'attend pas les 5 s de l'égaliseur"
    );
    // Anti-rafale : jamais sous la durée d'une relance (≈ 1 s) plus
    // l'anti-rebond.
    assert!(
        PlaybackOrchestrator::UPSAMPLER_REPLAY_FLOOR_MS
            >= PlaybackOrchestrator::EQ_REPLAY_DEBOUNCE_MS + 1_000
    );
    assert!(RaisonDeRelance::Surechantillonnage.differe_sous_le_plancher());
    // Les autres gestes gardent leur règle : 5 s, et abandon sous le plancher.
    for r in [RaisonDeRelance::Reglage, RaisonDeRelance::BasculePure] {
        assert_eq!(r.plancher_ms(), PlaybackOrchestrator::EQ_REPLAY_FLOOR_MS);
        assert!(!r.differe_sous_le_plancher());
    }
    assert_eq!(
        RaisonDeRelance::Surechantillonnage.code(),
        "upsampler_change"
    );
}
