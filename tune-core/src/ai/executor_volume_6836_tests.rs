//! #6836 — l'outil `set_volume` de l'assistant (`/ai/query`) : aucun défaut,
//! baisse toujours permise, hausse par petits pas, et toujours par le point
//! de passage unique de l'orchestrateur (limites #6541, PURE, volume fixe).
//! Vu depuis la sortie factice de la zone.
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::*;
use crate::db::migrations::run_migrations;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;
use crate::http::streamer::AudioStreamer;
use crate::outputs::mock::MockOutput;
use crate::outputs::registry::OutputRegistry;
use crate::streaming::registry::ServiceRegistry;

const APPAREIL: &str = "dlna-salon-6836";

async fn banc(volume_initial: f64) -> (ToolExecutor, Arc<PlaybackOrchestrator>, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let playback = Arc::new(PlaybackManager::new());
    let orch = Arc::new(PlaybackOrchestrator::new(
        db.clone(),
        playback.clone(),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        None,
    ));
    let zone_id = ZoneRepo::with_backend(db.clone())
        .create("Salon 6836", Some("dlna"), Some(APPAREIL))
        .unwrap();
    ZoneRepo::with_backend(db.clone())
        .update_volume(zone_id, volume_initial * 100.0)
        .unwrap();
    orch.outputs.lock().await.register(Box::new(
        MockOutput::new(APPAREIL, "Salon").with_type("dlna"),
    ));
    playback.set_volume(zone_id, volume_initial).await;
    let ex = ToolExecutor::with_backend(db, orch.clone(), playback, zone_id);
    (ex, orch, zone_id)
}

async fn recus(orch: &PlaybackOrchestrator) -> Vec<f64> {
    let outputs = orch.outputs.lock().await;
    let out = outputs.get(APPAREIL).unwrap();
    let guard = out.lock().await;
    guard
        .as_any()
        .downcast_ref::<MockOutput>()
        .unwrap()
        .volume_calls()
        .await
}

async fn demander(ex: &mut ToolExecutor, entree: Value) -> Value {
    ex.execute("set_volume", entree).await
}

/// 🔴 avant / 🟢 après : un argument absent valait 0,5 — la zone à 20 %
/// montait à 50 %. Désormais : refus, rien n'est envoyé.
#[tokio::test]
async fn un_argument_absent_ou_invalide_est_refuse_sans_defaut() {
    for entree in [
        json!({}),
        json!({ "volume": null }),
        json!({ "volume": "0.5" }),
        json!({ "volume": 1.5 }),
        json!({ "volume": -0.1 }),
        json!({ "level": 0.5 }),
    ] {
        let (mut ex, orch, zone_id) = banc(0.20).await;
        let r = demander(&mut ex, entree.clone()).await;
        assert_eq!(
            r["error"], "invalid_volume",
            "#6836 : {entree} doit être refusé, reçu {r}"
        );
        assert!(
            recus(&orch).await.is_empty(),
            "#6836 : {entree} a commandé l'appareil"
        );
        assert_eq!(ex.playback.get_state(zone_id).await.volume, 0.20);
    }
}

/// 🔴 avant / 🟢 après : un saut de 20 % à 100 % est refusé, et la réponse
/// dit au modèle le plus haut permis.
#[tokio::test]
async fn un_saut_de_20_a_100_pour_cent_est_refuse() {
    let (mut ex, orch, zone_id) = banc(0.20).await;
    let r = demander(&mut ex, json!({ "volume": 1.0 })).await;
    assert_eq!(r["error"], "volume_raise_too_large", "{r}");
    assert!((r["max_volume_allowed"].as_f64().unwrap() - 0.23).abs() < 1e-9);
    assert!(recus(&orch).await.is_empty(), "rien n'est envoyé");
    assert_eq!(ex.playback.get_state(zone_id).await.volume, 0.20);
    // Juste au-dessus du pas : refusé aussi.
    let r = demander(&mut ex, json!({ "volume": 0.24 })).await;
    assert_eq!(r["error"], "volume_raise_too_large", "{r}");
}

/// Le plein volume doit rester inaccessible même quand il tient dans le
/// pas autorisé. DLNA/OpenHome arrondissent aussi 0,995 à SetVolume 100.
#[tokio::test]
async fn le_plein_volume_ne_part_pas_meme_dans_un_petit_pas() {
    for (initial, pas) in [
        (0.97, None),
        (0.98, None),
        (0.99, None),
        (0.90, Some("0.10")),
        (0.20, Some("1.0")),
    ] {
        for demande in [0.995, 0.999, 1.0] {
            let (mut ex, orch, zone_id) = banc(initial).await;
            let s = SettingsRepo::with_backend(ex.db.clone());
            s.set(&format!("zone_{zone_id}_volume_limit_safety"), "1.0")
                .unwrap();
            s.set(&format!("zone_{zone_id}_volume_limit_comfort"), "1.0")
                .unwrap();
            if let Some(pas) = pas {
                s.set(REGLAGE_HAUSSE_MAXIMALE, pas).unwrap();
            }
            let r = demander(&mut ex, json!({ "volume": demande })).await;
            let envoyes = recus(&orch).await;
            assert!(
                envoyes.iter().all(|v| (v * 100.0).round() < 100.0),
                "#6836 : commande de plein volume 100 envoyée depuis {initial} \
                 pour {demande}, pas {pas:?} : {envoyes:?}, réponse {r}"
            );
            assert!(
                r["error"].is_string() && envoyes.is_empty(),
                "une demande de plein volume doit être refusée sans commande : {r}, {envoyes:?}"
            );
            assert_eq!(ex.playback.get_state(zone_id).await.volume, initial);
        }
    }
}

/// Une hausse dans le pas passe ; une baisse passe toujours, même grande.
#[tokio::test]
async fn une_petite_hausse_et_toute_baisse_passent() {
    let (mut ex, orch, _zone_id) = banc(0.20).await;
    let r = demander(&mut ex, json!({ "volume": 0.23 })).await;
    assert_eq!(r["status"], "volume_set", "{r}");
    let r = demander(&mut ex, json!({ "volume": 0.02 })).await;
    assert_eq!(r["status"], "volume_set", "{r}");
    let r = demander(&mut ex, json!({ "volume": 0.0 })).await;
    assert_eq!(r["status"], "volume_set", "{r}");
    let envoyes = recus(&orch).await;
    assert_eq!(envoyes.len(), 3, "{envoyes:?}");
    assert!((envoyes[0] - 0.23).abs() < 1e-9 && (envoyes[1] - 0.02).abs() < 1e-9);
    assert_eq!(envoyes[2], 0.0);
}

/// La valeur provisoire est réglable sans recompiler ; une valeur illisible
/// rend le défaut de 3 points.
#[tokio::test]
async fn la_hausse_maximale_est_reglable() {
    let (mut ex, _orch, _zone_id) = banc(0.20).await;
    assert_eq!(hausse_maximale_par_appel(&ex.db), HAUSSE_MAXIMALE_PAR_APPEL);
    let s = SettingsRepo::with_backend(ex.db.clone());
    s.set(REGLAGE_HAUSSE_MAXIMALE, "0.10").unwrap();
    let r = demander(&mut ex, json!({ "volume": 0.30 })).await;
    assert_eq!(r["status"], "volume_set", "{r}");
    s.set(REGLAGE_HAUSSE_MAXIMALE, "beaucoup").unwrap();
    assert_eq!(hausse_maximale_par_appel(&ex.db), HAUSSE_MAXIMALE_PAR_APPEL);
    s.set(REGLAGE_HAUSSE_MAXIMALE, "7").unwrap();
    assert_eq!(hausse_maximale_par_appel(&ex.db), HAUSSE_MAXIMALE_PAR_APPEL);
}

/// L'appel passe par l'orchestrateur : la limite de sécurité #6541 borne
/// une hausse permise, le verrou PURE refuse une baisse, le volume fixe
/// n'envoie rien.
#[tokio::test]
async fn limites_pure_et_volume_fixe_s_appliquent_toujours() {
    // Sécurité à 21 % : 20 → 23 demandé, l'appareil reçoit 21 au plus.
    let (mut ex, orch, zone_id) = banc(0.20).await;
    SettingsRepo::with_backend(ex.db.clone())
        .set(&format!("zone_{zone_id}_volume_limit_safety"), "0.21")
        .unwrap();
    let r = demander(&mut ex, json!({ "volume": 0.23 })).await;
    assert_eq!(r["status"], "volume_set", "{r}");
    let envoyes = recus(&orch).await;
    assert!(
        envoyes.iter().all(|v| *v <= 0.21 + 1e-9),
        "#6541 : la sécurité n'a pas borné l'assistant : {envoyes:?}"
    );
    assert!(r["volume"].as_f64().unwrap() <= 0.21 + 1e-9, "{r}");

    // PURE verrouillé : une baisse est refusée par l'orchestrateur.
    let (mut ex, orch, zone_id) = banc(1.0).await;
    SettingsRepo::with_backend(ex.db.clone())
        .set(
            &format!("zone_{zone_id}_audiophile"),
            r#"{"enabled":true,"lock_volume":true}"#,
        )
        .unwrap();
    let r = demander(&mut ex, json!({ "volume": 0.5 })).await;
    assert_eq!(r["error"], "output_command_failed", "{r}");
    assert!(recus(&orch).await.is_empty(), "PURE : rien n'est envoyé");

    // Volume fixe : rien n'est envoyé, la zone reste à 100 %.
    let (mut ex, orch, zone_id) = banc(1.0).await;
    ZoneRepo::with_backend(ex.db.clone())
        .update_fixed_volume(zone_id, true)
        .unwrap();
    let r = demander(&mut ex, json!({ "volume": 0.5 })).await;
    assert_eq!(r["status"], "volume_set", "{r}");
    assert!(
        recus(&orch).await.is_empty(),
        "volume fixe : rien n'est envoyé"
    );
    assert_eq!(ex.playback.get_state(zone_id).await.volume, 1.0);
}

#[tokio::test]
async fn un_volume_absent_ne_devient_pas_50_meme_si_le_pas_le_permet_6836() {
    let (mut ex, orch, zone_id) = banc(0.48).await;
    let r = demander(&mut ex, json!({})).await;
    let envoyes = recus(&orch).await;
    assert!(
        envoyes.is_empty(),
        "#6836 : argument absent devenu commande par défaut à 50 % : {envoyes:?}, {r}"
    );
    assert_eq!(r["error"], "invalid_volume", "{r}");
    assert_eq!(ex.playback.get_state(zone_id).await.volume, 0.48);
}

#[tokio::test]
async fn un_pas_de_quatre_points_ne_part_pas_6836() {
    let (mut ex, orch, zone_id) = banc(0.20).await;
    let r = demander(&mut ex, json!({"volume":0.24})).await;
    let envoyes = recus(&orch).await;
    assert!(
        envoyes.is_empty(),
        "#6836 : hausse de quatre points envoyée au-delà du pas de trois : {envoyes:?}, {r}"
    );
    assert_eq!(r["error"], "volume_raise_too_large", "{r}");
    assert_eq!(ex.playback.get_state(zone_id).await.volume, 0.20);
}
