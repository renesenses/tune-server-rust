//! #4384 (fil 1797) — un préampli ReplayGain changé dans les réglages EN
//! COURS D'ÉCOUTE doit atteindre tout de suite la sortie locale qui joue, et
//! donc le crête-mètre, qui lit le gain de rendu de la sortie à chaque
//! fenêtre.
//!
//! Avant ce correctif, `PATCH /system/config` écrivait la clé et rendait
//! `ok` : le facteur de la sortie n'était reposé qu'à la piste suivante. À
//! −6 dB, ni le son ni l'aiguille ne bougeaient.

use super::touche_le_replaygain;

#[test]
fn seules_les_cles_replaygain_declenchent_la_reapplication() {
    assert!(touche_le_replaygain(&[
        tune_core::audio::replaygain::PREAMP_KEY.to_string()
    ]));
    assert!(touche_le_replaygain(&[
        tune_core::audio::replaygain::MODE_KEY.to_string()
    ]));
    assert!(touche_le_replaygain(&[
        tune_core::audio::replaygain::PREVENT_CLIPPING_KEY.to_string()
    ]));
    assert!(!touche_le_replaygain(&["theme".to_string()]));
    assert!(!touche_le_replaygain(&[]));
}

#[cfg(feature = "local-audio")]
mod sortie_locale {
    use crate::state::AppState;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use serde_json::{Value, json};
    use tower::ServiceExt;
    use tune_core::db::settings_repo::SettingsRepo;
    use tune_core::db::track_metadata_repo::TrackMetadataRepo;
    use tune_core::db::track_repo::TrackRepo;
    use tune_core::db::zone_repo::ZoneRepo;
    use tune_core::outputs::local::LocalOutput;

    async fn patch(state: &AppState, corps: Value) -> (StatusCode, Value) {
        let response = crate::routes::router(state.clone())
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/api/v1/system/config")
                    .header("content-type", "application/json")
                    .body(Body::from(corps.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    /// Ce que lit le forwarder de niveaux pour cette zone, en dB.
    fn gain_lu_par_le_crete_metre_db(state: &AppState, zone_id: i64) -> f64 {
        let u = state.orchestrator.playback.gain_de_rendu_units(zone_id);
        20.0 * (f64::from(u) / 1000.0).log10()
    }

    #[tokio::test]
    async fn le_preampli_change_en_ecoutant_baisse_tout_de_suite_l_aiguille() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let orch = state.orchestrator.clone();
        let zone_id = ZoneRepo::with_backend(orch.db.clone())
            .create("Salon", Some("local"), Some("local:DAC"))
            .unwrap();
        orch.outputs
            .lock()
            .await
            .register(Box::new(LocalOutput::new("DAC".to_string())));

        // Une piste taguée à −3 dB, ReplayGain « piste », préampli 0 dB.
        let mut piste = tune_core::db::models::Track::new("Piste".into());
        piste.format = Some("flac".into());
        let tid = TrackRepo::with_backend(orch.db.clone())
            .create(&piste)
            .unwrap();
        TrackMetadataRepo::with_backend(orch.db.clone())
            .set(tid, "rg_track_gain", "-3.00 dB")
            .unwrap();
        let settings = SettingsRepo::with_backend(orch.db.clone());
        settings
            .set(tune_core::audio::replaygain::MODE_KEY, "track")
            .unwrap();
        settings
            .set(tune_core::audio::replaygain::PREAMP_KEY, "0")
            .unwrap();
        settings
            .set(tune_core::audio::replaygain::PREVENT_CLIPPING_KEY, "false")
            .unwrap();

        // Ce que fait le chemin de lecture au lancement de la piste.
        orch.playback
            .play(
                zone_id,
                tune_core::playback::NowPlaying {
                    track_id: Some(tid),
                    ..Default::default()
                },
            )
            .await;
        {
            let arc = orch.outputs.lock().await.get("local:DAC").unwrap();
            let sortie = arc.lock().await;
            let local = sortie.as_any().downcast_ref::<LocalOutput>().unwrap();
            local.set_replaygain_factor(tune_core::audio::replaygain::playback_factor(
                &orch.db, tid,
            ));
            orch.playback
                .brancher_le_gain_de_sortie(zone_id, local.gain_de_rendu());
        }
        let avant = gain_lu_par_le_crete_metre_db(&state, zone_id);
        assert!(
            (avant + 3.0).abs() < 0.05,
            "départ attendu à −3 dB, lu {avant}"
        );

        // Le geste du testeur : préampli à −6 dB, la piste continue.
        let (status, reponse) = patch(
            &state,
            json!({ tune_core::audio::replaygain::PREAMP_KEY: -6.0 }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let apres = gain_lu_par_le_crete_metre_db(&state, zone_id);
        assert!(
            (apres + 9.0).abs() < 0.05,
            "🔴 #4384 — préampli écrit mais pas appliqué : l'aiguille lit {apres} dB \
             au lieu de −9 dB jusqu'à la piste suivante"
        );
        assert_eq!(reponse["replaygain_applied_live_zones"], json!(1));

        // Un réglage sans rapport ne repousse rien et n'annonce rien.
        let (_, reponse) = patch(&state, json!({ "theme": "dark" })).await;
        assert!(reponse.get("replaygain_applied_live_zones").is_none());
    }
}
