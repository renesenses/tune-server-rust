//! #6780 (fil 2333 puis 2386, Didier, rc7, ASIO) — le bouclier PURE « se
//! remplit une seconde puis se vide » avec un greffon DSP actif.
//!
//! Le correctif rc7 (#6544) fait suivre au verdict PURE de la sortie locale
//! les réglages ENREGISTRÉS de la zone (`pure_contourne_un_reglage`). Il
//! comptait l'égaliseur, la correction de pièce, le crossfeed, les greffons
//! natifs tiers et le repli mono — pas la réaffectation des canaux (greffon
//! `channel-remap`), que PURE coupe pourtant (`regle_effective_with` rend
//! `None` en PURE). Avec une règle de zone armée, le verdict restait « PURE
//! actif : aucun traitement armé, rien à contourner » : le client allume le
//! bouclier au clic, puis le vide à la publication suivante.
//!
//! Sorties factices, aucun son.

use super::PlaybackOrchestrator;
use crate::db::settings_repo::SettingsRepo;
use crate::outputs::traits::{OutputDspState, OutputTarget};
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

/// Une règle de zone 2 → 2 qui échange gauche et droite : armée, pas
/// l'identité.
const REGLE_ECHANGE: &str = r#"{"enabled":true,"inputs":2,"outputs":2,"gains_db":[[null,0.0],[0.0,null]],"normalize":false,"preset":null}"#;

/// Une zone locale qui joue (format déclaré), PURE éteint au départ.
async fn zone_locale_qui_joue(orch: &PlaybackOrchestrator) -> i64 {
    let zone_id = crate::db::zone_repo::ZoneRepo::with_backend(orch.db.clone())
        .create("USB DAC ASIO", Some("local"), Some("local:USB DAC ASIO"))
        .unwrap();
    orch.outputs
        .lock()
        .await
        .register(Box::new(crate::outputs::local::LocalOutput::new(
            "USB DAC ASIO".to_string(),
        )));
    let arc = orch.outputs.lock().await.get("local:USB DAC ASIO").unwrap();
    let sortie = arc.lock().await;
    let local = sortie
        .as_any()
        .downcast_ref::<crate::outputs::local::LocalOutput>()
        .unwrap();
    local.declare_current_format_for_test(44_100, 2);
    local.set_pure_bypass(false);
    zone_id
}

/// La vraie bascule PURE de l'orchestrateur, puis le verdict que lit le
/// sondeur.
async fn verdict_apres_la_bascule_pure(
    orch: &PlaybackOrchestrator,
    zone_id: i64,
) -> OutputDspState {
    SettingsRepo::with_backend(orch.db.clone())
        .set(&format!("zone_{zone_id}_audiophile"), r#"{"enabled":true}"#)
        .unwrap();
    assert!(orch.refresh_zone_pure_dsp(zone_id).await);
    let arc = orch.outputs.lock().await.get("local:USB DAC ASIO").unwrap();
    let sortie = arc.lock().await;
    let local = sortie
        .as_any()
        .downcast_ref::<crate::outputs::local::LocalOutput>()
        .unwrap();
    local.publier_le_contrat_pour_test();
    local.signal_path_status().unwrap().dsp
}

/// LE témoin : règle de réaffectation de zone armée, greffon activé, bascule
/// PURE — le verdict doit être « DSP contourné par PURE », pas « rien à
/// contourner ».
#[tokio::test]
async fn pure_contourne_la_reaffectation_de_zone_armee_6780() {
    let orch = orchestrateur();
    let zone_id = zone_locale_qui_joue(&orch).await;
    let s = SettingsRepo::with_backend(orch.db.clone());
    s.set("plugin_channel-remap_installed", "true").unwrap();
    s.set("plugin_channel-remap_enabled", "true").unwrap();
    s.set(
        &crate::audio::reaffectation_canaux::cle_de_zone(zone_id),
        REGLE_ECHANGE,
    )
    .unwrap();

    assert_eq!(
        verdict_apres_la_bascule_pure(&orch, zone_id).await,
        OutputDspState::BypassedPure,
        "#6780 : une réaffectation des canaux armée sur la zone est coupée \
         par PURE — le bouclier doit le dire (« DSP contourné par PURE »), \
         pas « rien à contourner »"
    );
}

/// Les cas où PURE n'a vraiment rien à contourner restent dits tels : greffon
/// désactivé, règle désarmée, règle identité.
#[tokio::test]
async fn une_reaffectation_inerte_reste_rien_a_contourner_6780() {
    let identite = r#"{"enabled":true,"inputs":2,"outputs":2,"gains_db":[[0.0,null],[null,0.0]],"normalize":false,"preset":null}"#;
    let desarmee = REGLE_ECHANGE.replace(r#""enabled":true"#, r#""enabled":false"#);
    for (greffon, regle, quoi) in [
        ("false", REGLE_ECHANGE, "greffon désactivé"),
        ("true", desarmee.as_str(), "règle désarmée"),
        ("true", identite, "matrice identité"),
    ] {
        let orch = orchestrateur();
        let zone_id = zone_locale_qui_joue(&orch).await;
        let s = SettingsRepo::with_backend(orch.db.clone());
        s.set("plugin_channel-remap_installed", "true").unwrap();
        s.set("plugin_channel-remap_enabled", greffon).unwrap();
        s.set(
            &crate::audio::reaffectation_canaux::cle_de_zone(zone_id),
            regle,
        )
        .unwrap();
        assert_eq!(
            verdict_apres_la_bascule_pure(&orch, zone_id).await,
            OutputDspState::PureSansObjet,
            "{quoi} : PURE n'a rien à contourner, il doit le dire"
        );
    }
}
