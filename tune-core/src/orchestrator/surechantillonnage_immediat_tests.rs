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

/// Ce qu'aurait laissé l'ouverture du flux qui joue : les réglages lus alors.
async fn flux_ouvert_avec(orch: &PlaybackOrchestrator, reglage: Option<serde_json::Value>) {
    let reglages = reglage.map(|r| su::greffon::settings(&r).unwrap());
    let arc = orch
        .outputs
        .lock()
        .await
        .get(&format!("local:{SORTIE}"))
        .unwrap();
    let sortie = arc.lock().await;
    sortie
        .as_any()
        .downcast_ref::<crate::outputs::local::LocalOutput>()
        .unwrap()
        .simuler_l_ouverture_du_surechantillonneur(reglages);
}

fn derniere_relance(orch: &PlaybackOrchestrator, zone_id: i64) -> Option<std::time::Instant> {
    orch.eq_replay_last.lock().unwrap().get(&zone_id).copied()
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
    flux_ouvert_avec(&orch, None).await;

    regler(&orch, zone_id, serde_json::json!({"enabled": true}));
    let portee = orch.apply_upsampler_change_portee(zone_id).await;

    assert_eq!(portee, PorteeDuReglage::Relance);
    assert_eq!(
        relances_programmees(&orch, zone_id),
        1,
        "une relance programmée"
    );

    // La relance a rouvert le flux avec le greffon ; puis dans l'autre sens :
    // désactiver relance aussi (comparaison A/B).
    flux_ouvert_avec(&orch, Some(serde_json::json!({"enabled": true}))).await;
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
    flux_ouvert_avec(&orch, Some(serde_json::json!({"enabled": true}))).await;

    let precision = serde_json::json!({"enabled": true, "filter": "precision"});
    regler(&orch, zone_id, precision.clone());
    assert_eq!(
        orch.apply_upsampler_change_portee(zone_id).await,
        PorteeDuReglage::Relance
    );
    assert_eq!(relances_programmees(&orch, zone_id), 1);

    // Le flux rouvert avec ce filtre, le même réglage réécrit : rien ne
    // change dans le son, aucune coupure.
    flux_ouvert_avec(&orch, Some(precision)).await;
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
    flux_ouvert_avec(&orch, Some(serde_json::json!({"enabled": true}))).await;
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

/// Relecture du 20/11 (4) — la comparaison se fait avec les réglages du flux
/// qui JOUE : un changement fait en pause, puis un PUT identique après la
/// reprise, relance encore (le flux repris porte toujours l'ancien réglage).
#[tokio::test]
async fn change_en_pause_puis_put_identique_apres_reprise_relance() {
    let orch = orchestrateur();
    let zone_id = zone_locale(&orch).await;
    orch.refresh_zone_upsampler(zone_id).await;
    jouer(&orch, zone_id).await;
    flux_ouvert_avec(&orch, None).await;
    orch.playback.pause(zone_id).await;

    regler(&orch, zone_id, serde_json::json!({"enabled": true}));
    assert_eq!(
        orch.apply_upsampler_change_portee(zone_id).await,
        PorteeDuReglage::PisteSuivante
    );
    orch.playback.resume(zone_id).await;
    assert_eq!(
        orch.apply_upsampler_change_portee(zone_id).await,
        PorteeDuReglage::Relance,
        "le flux repris joue encore sans le greffon : « appliqué » serait faux"
    );
}

/// Relecture du 20/11 (1 et 5) — un réglage d'égaliseur qui REMPLACE un
/// changement du suréchantillonneur en attente hérite de sa règle : sous le
/// plancher, la relance est différée, jamais abandonnée.
#[tokio::test(start_paused = true)]
async fn une_demande_d_egaliseur_qui_remplace_le_surechantillonneur_est_differee() {
    let orch = orchestrateur();
    let zone_id = zone_locale(&orch).await;
    orch.refresh_zone_upsampler(zone_id).await;
    jouer(&orch, zone_id).await;
    flux_ouvert_avec(&orch, None).await;
    // Une relance vient d'avoir lieu : le plancher de 5 s de l'égaliseur
    // abandonnerait la demande suivante.
    let avant = std::time::Instant::now();
    orch.eq_replay_last.lock().unwrap().insert(zone_id, avant);

    regler(&orch, zone_id, serde_json::json!({"enabled": true}));
    assert_eq!(
        orch.apply_upsampler_change_portee(zone_id).await,
        PorteeDuReglage::Relance
    );
    // L'égaliseur arrive pendant l'anti-rebond : sa demande remplace la nôtre.
    assert!(orch.schedule_replay_pour(zone_id, RaisonDeRelance::Reglage));
    assert_eq!(
        orch.raison_effective(zone_id, RaisonDeRelance::Reglage),
        RaisonDeRelance::Surechantillonnage
    );
    let mut relancee = false;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if derniere_relance(&orch, zone_id) != Some(avant) {
            relancee = true;
            break;
        }
    }
    assert!(relancee, "la relance a eu lieu, différée et non abandonnée");
    assert!(
        !orch
            .surech_replay_en_attente
            .lock()
            .unwrap()
            .contains(&zone_id),
        "servie par cette relance"
    );
}

/// Relecture du 20/11 (2) — une seule relance à la fois par zone : tant
/// qu'une relance tient le verrou de la zone, la suivante attend, et son
/// plancher part du début de la relance.
#[tokio::test(start_paused = true)]
async fn deux_relances_ne_se_chevauchent_jamais() {
    let orch = orchestrateur();
    let zone_id = zone_locale(&orch).await;
    orch.refresh_zone_upsampler(zone_id).await;
    jouer(&orch, zone_id).await;
    flux_ouvert_avec(&orch, None).await;

    let verrou = orch.verrou_de_relance(zone_id);
    let relance_en_cours = verrou.lock().await;
    regler(&orch, zone_id, serde_json::json!({"enabled": true}));
    assert_eq!(
        orch.apply_upsampler_change_portee(zone_id).await,
        PorteeDuReglage::Relance
    );
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    assert_eq!(
        derniere_relance(&orch, zone_id),
        None,
        "la seconde relance attend que la première ait fini"
    );
    drop(relance_en_cours);
    let mut lancee = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if derniere_relance(&orch, zone_id).is_some() {
            lancee = true;
            break;
        }
    }
    assert!(lancee, "le plancher est posé dès le début de la relance");
}
