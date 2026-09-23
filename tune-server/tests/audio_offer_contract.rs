//! Business policy oracle, deliberately independent of manifest declarations.
//! Exercises the production startup and HTTP router without a native package.
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::{settings_repo::SettingsRepo, zone_repo::ZoneRepo};
use tune_server::state::AppState;

/// Own only the UUID directory produced by this test's real HTTP job. Clean
/// even if polling times out or an assertion panics; never remove other jobs.
struct JobOutputCleanup(std::path::PathBuf);
impl Drop for JobOutputCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn request(app: &axum::Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn audio_offer_free_eq_and_premium_four_survive_real_startup() {
    crate::use_scratch_plugin_data_dir();
    // An entitlement inversion in ANY shipping manifest must fail this witness.
    for (raw, expected) in [
        (
            include_str!("../../sdk/tune-plugin-equalizer/manifest.json"),
            "free",
        ),
        (
            include_str!("../../sdk/tune-plugin-crossfeed/manifest.json"),
            "crossfeed",
        ),
        (
            include_str!("../../sdk/tune-plugin-converter/manifest.json"),
            "batch_converter",
        ),
        (
            include_str!("../../sdk/tune-plugin-declick/manifest.json"),
            "declick",
        ),
    ] {
        let manifest: Value = serde_json::from_str(raw).unwrap();
        assert_eq!(
            manifest["entitlement"], expected,
            "commercial manifest policy: {}",
            manifest["id"]
        );
    }
    for premium in [false, true] {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        state.license.set_account_premium(premium, None).await;
        let zone = ZoneRepo::with_backend(state.backend.clone())
            .create("Headphones", Some("local"), Some("local:fixture"))
            .unwrap();
        let settings = SettingsRepo::with_backend(state.backend.clone());
        let key = format!("zone_{zone}_crossfeed");
        let saved = r#"{"enabled":true,"amount":0.37,"delay_ms":0.65}"#;
        settings.set(&key, saved).unwrap(); // A pre-SDK user's configuration.
        // The equalizer is optional since v0.9.156: install it as the catalogue
        // route would, so the Free tier still gets it in this witness.
        settings.set("plugin_equalizer_installed", "true").unwrap();
        let routers = tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![]).await;
        let loaded = state.plugin_info.get().unwrap();
        for id in ["equalizer", "crossfeed", "converter", "declick"] {
            assert_eq!(
                loaded.iter().any(|p| p.name == id),
                premium || id == "equalizer",
                "default delivery: {id}, premium={premium}"
            );
        }
        assert!(
            tune_plugin_native::provider("equalizer").is_none(),
            "fixture must prove bundled EQ with no package"
        );
        let app = tune_server::routes::router_with_plugins(state.clone(), routers);
        let eq =
            json!({"enabled":true,"bands":[{"freq":1000.0,"gain":3.0,"q":1.41,"type":"peak"}]});
        let (status, response) = request(
            &app,
            "POST",
            &format!("/api/v1/zones/{zone}/eq"),
            eq.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "EQ: {response}");
        let (status, response) = request(
            &app,
            "POST",
            "/api/v1/eq/presets",
            json!({"name":"Offer witness","bands":eq["bands"]}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "preset: {response}");
        let dsp = format!("/api/v1/zones/{zone}/dsp");
        let (_, response) = request(&app, "GET", &dsp, Value::Null).await;
        assert_eq!(response["crossfeed_status"]["requested"], true);
        assert_eq!(response["crossfeed_status"]["effective"], premium);
        if !premium {
            assert_eq!(response["crossfeed_status"]["reason"], "premium_required");
        }
        let (status, _) = request(
            &app,
            "PUT",
            &dsp,
            json!({"crossfeed":{"enabled":true,"amount":0.2,"delay_ms":0.4}}),
        )
        .await;
        assert_eq!(
            status,
            if premium {
                StatusCode::OK
            } else {
                StatusCode::PAYMENT_REQUIRED
            }
        );
        if !premium {
            assert_eq!(
                settings.get(&key).unwrap().as_deref(),
                Some(saved),
                "hard cut must retain the exact old settings"
            );
            state.license.set_account_premium(true, None).await;
            let (status, response) =
                request(&app, "POST", "/api/v1/plugins/crossfeed/install", json!({})).await;
            assert_eq!(status, StatusCode::OK, "reactivation: {response}");
            let (_, response) = request(&app, "GET", &dsp, Value::Null).await;
            assert_eq!(response["crossfeed_status"]["effective"], true);
            assert_eq!(response["crossfeed"]["amount"], 0.37);
            assert_eq!(settings.get(&key).unwrap().as_deref(), Some(saved));
            state.license.set_account_premium(false, None).await;
        }
        // A real, tiny PCM WAV; Premium must start and complete both jobs.
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("offer.wav");
        let mut wav = Vec::new();
        let data: Vec<u8> = (0..1024)
            .flat_map(|i| (if i % 16 < 8 { 12000_i16 } else { -12000_i16 }).to_le_bytes())
            .collect();
        wav.extend(b"RIFF");
        wav.extend((36 + data.len() as u32).to_le_bytes());
        wav.extend(b"WAVEfmt ");
        wav.extend(16_u32.to_le_bytes());
        wav.extend(1_u16.to_le_bytes());
        wav.extend(1_u16.to_le_bytes());
        wav.extend(48000_u32.to_le_bytes());
        wav.extend(96000_u32.to_le_bytes());
        wav.extend(2_u16.to_le_bytes());
        wav.extend(16_u16.to_le_bytes());
        wav.extend(b"data");
        wav.extend((data.len() as u32).to_le_bytes());
        wav.extend(data);
        std::fs::write(&input, wav).unwrap();
        for tool in ["converter", "declick"] {
            let (status, response) = request(&app, "POST", &format!("/api/v1/{tool}/start"), json!({"sources":[{"path":input}],"format":"wav","options":{"output_format":"wav"}})).await;
            assert_eq!(
                status,
                if premium {
                    StatusCode::CREATED
                } else {
                    StatusCode::PAYMENT_REQUIRED
                },
                "{tool}: {response}"
            );
            if premium {
                let id = response["job_id"].as_str().unwrap();
                uuid::Uuid::parse_str(id).unwrap();
                let output = if tool == "converter" {
                    "tune-convert"
                } else {
                    "tune-declick"
                };
                // La racine est PROPRE À L'UTILISATEUR depuis #4770 : la
                // composer à la main ici referait le défaut corrigé, et le
                // ménage porterait sur un dossier qui n'existe pas. On
                // demande donc son chemin au même code que la route.
                // tmp-autorise: dossier créé par la route réelle, repris et nettoyé par Drop, même sur panique.
                let racine = tune_core::chemins_de_travail::racine_de_travail(output);
                let _cleanup = JobOutputCleanup(racine.join(id));
                let result = tokio::time::timeout(std::time::Duration::from_secs(15), async {
                    loop {
                        let (_, result) = request(
                            &app,
                            "GET",
                            &format!("/api/v1/{tool}/status/{id}"),
                            Value::Null,
                        )
                        .await;
                        if result["status"] != "running" {
                            break result;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                })
                .await
                .unwrap();
                assert_eq!(result["status"], "completed", "{tool}: {result}");
                assert_eq!(result["completed"], 1, "{tool}: {result}");
            }
        }
    }
}
