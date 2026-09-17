//! #4194: the installed WASM list and uninstall must agree without a store record.
#![cfg(feature = "plugins-wasm")]
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

async fn call(app: &axum::Router, method: &str, path: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
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

struct PluginDirectory {
    dir: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
    previous_probe: Option<std::ffi::OsString>,
}
impl PluginDirectory {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("TUNE_PLUGINS_DIR");
        let previous_probe = std::env::var_os("TUNE_WASM_PROBE_SKIP");
        unsafe {
            std::env::set_var("TUNE_PLUGINS_DIR", dir.path());
            std::env::set_var("TUNE_WASM_PROBE_SKIP", "1");
        }
        Self {
            dir,
            previous,
            previous_probe,
        }
    }
    fn install(&self, id: &str) -> std::path::PathBuf {
        let plugin = self.dir.path().join(id);
        std::fs::create_dir_all(&plugin).unwrap();
        let fixture =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugins/party");
        let mut manifest: Value =
            serde_json::from_slice(&std::fs::read(fixture.join("manifest.json")).unwrap()).unwrap();
        manifest["id"] = json!(id);
        std::fs::write(plugin.join("manifest.json"), manifest.to_string()).unwrap();
        std::fs::copy(fixture.join("main.wasm"), plugin.join("main.wasm")).unwrap();
        plugin
    }
}
impl Drop for PluginDirectory {
    fn drop(&mut self) {
        unsafe {
            match &self.previous_probe {
                Some(value) => std::env::set_var("TUNE_WASM_PROBE_SKIP", value),
                None => std::env::remove_var("TUNE_WASM_PROBE_SKIP"),
            }
            match &self.previous {
                Some(value) => std::env::set_var("TUNE_PLUGINS_DIR", value),
                None => std::env::remove_var("TUNE_PLUGINS_DIR"),
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bundled_wasm_uninstall_4194_removes_the_listed_installation() {
    let _environment = crate::lock_environment();
    let directory = PluginDirectory::new();
    let plugin = directory.install("party");
    let unrelated = directory.install("other");
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("plugin_party_enabled", "false").unwrap();
    let app = tune_server::routes::router(state.clone());
    let (status, list) = call(&app, "GET", "/api/v1/plugins").await;
    assert_eq!(status, StatusCode::OK);
    let party = list
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "party")
        .unwrap();
    assert_eq!(party["installed"], true);
    assert_eq!(party["enabled"], false);
    assert_eq!(settings.get("marketplace_installed").unwrap(), None);
    let (status, body) = call(&app, "POST", "/api/v1/marketplace/plugins/party/uninstall").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "#4194: a WASM plugin listed as installed must uninstall without a marketplace record: {body}"
    );
    assert_eq!(body["status"], "uninstalled");
    assert_eq!(body["restart_required"], true);
    assert!(
        !plugin.exists(),
        "the advertised installation must actually be removed"
    );
    assert!(unrelated.exists(), "unrelated plugins must remain");
    assert_eq!(settings.get("plugin_party_enabled").unwrap(), None);
    let (_, list) = call(&app, "GET", "/api/v1/plugins").await;
    assert!(
        !list
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "party")
    );
    let (status, _) = call(&app, "POST", "/api/v1/marketplace/plugins/party/uninstall").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a second removal must not claim a new uninstall"
    );
    let restarted = AppState::new(":memory:", 0, Default::default()).unwrap();
    let (_, list) = call(
        &tune_server::routes::router(restarted),
        "GET",
        "/api/v1/plugins",
    )
    .await;
    assert!(
        !list
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "party")
    );
}

#[tokio::test]
async fn marketplace_uninstall_4194_accepts_manifest_id_and_store_slug() {
    let _environment = crate::lock_environment();
    let directory = PluginDirectory::new();
    for requested in ["party", "party-mode"] {
        let plugin = directory.install("party");
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let settings = SettingsRepo::with_backend(state.backend.clone());
        settings
            .set(
                "marketplace_installed",
                &json!([
                    {"slug":"party-mode","version":"0.1.0","plugin_id":"party"},
                    {"slug":"unrelated","version":"1.0.0"}
                ])
                .to_string(),
            )
            .unwrap();
        for key in [
            "plugin_party_enabled",
            "plugin_party_installed",
            "plugin_party-mode_enabled",
            "plugin_party-mode_installed",
        ] {
            settings.set(key, "true").unwrap();
        }
        let app = tune_server::routes::router(state);
        let (status, body) = call(
            &app,
            "POST",
            &format!("/api/v1/marketplace/plugins/{requested}/uninstall"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "#4194: uninstall must resolve manifest id and slug: {body}"
        );
        assert!(!plugin.exists());
        let (_, installed) = call(&app, "GET", "/api/v1/marketplace/plugins/installed").await;
        assert_eq!(installed["count"], 1);
        assert_eq!(installed["plugins"][0]["slug"], "unrelated");
        for key in [
            "plugin_party_enabled",
            "plugin_party_installed",
            "plugin_party-mode_enabled",
            "plugin_party-mode_installed",
        ] {
            assert_eq!(settings.get(key).unwrap(), None, "stale setting {key}");
        }
    }
}

#[tokio::test]
async fn bundled_uninstall_4194_preserves_files_when_storage_or_identity_is_invalid() {
    let _environment = crate::lock_environment();
    let directory = PluginDirectory::new();
    let plugin = directory.install("party");
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("marketplace_installed", "not JSON").unwrap();
    let app = tune_server::routes::router(state);
    let (status, _) = call(&app, "POST", "/api/v1/marketplace/plugins/party/uninstall").await;
    assert!(status.is_server_error());
    assert!(
        plugin.exists(),
        "an unreadable registry is not an absent registry"
    );
    settings.set("marketplace_installed", "[]").unwrap();
    let manifest_path = plugin.join("manifest.json");
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest["id"] = json!("somebody-else");
    std::fs::write(&manifest_path, manifest.to_string()).unwrap();
    let (status, _) = call(&app, "POST", "/api/v1/marketplace/plugins/party/uninstall").await;
    assert!(status.is_client_error() || status.is_server_error());
    assert!(
        plugin.exists(),
        "a mismatched manifest must never authorize removal"
    );
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/marketplace/plugins/unknown/uninstall",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(plugin.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loaded_wasm_uninstall_4194_legacy_record_requires_restart() {
    let _environment = crate::lock_environment();
    let directory = PluginDirectory::new();
    let plugin = directory.install("party");
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(
            "marketplace_installed",
            r#"[{"slug":"party","version":"0.1.0"}]"#,
        )
        .unwrap();
    tune_server::plugins_host::load_wasm_plugins(&state).await;
    assert!(state.wasm_plugins.get().unwrap().get("party").is_some());
    let app = tune_server::routes::router(state.clone());
    let (status, body) = call(&app, "POST", "/api/v1/marketplace/plugins/party/uninstall").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !plugin.exists(),
        "#4194: a legacy record without plugin_id must not leave the WASM installed"
    );
    assert_eq!(body["restart_required"], true);
    assert!(
        state.wasm_plugins.get().unwrap().get("party").is_some(),
        "removing files does not unload a running WASM instance"
    );
    let restarted = AppState::new(":memory:", 0, Default::default()).unwrap();
    tune_server::plugins_host::load_wasm_plugins(&restarted).await;
    assert!(
        restarted.wasm_plugins.get().unwrap().get("party").is_none(),
        "the removed plugin must not load on the next server start"
    );
}

/// #4265 : le disque reste une autorite meme sans enregistrement SDK.
#[tokio::test]
async fn i4265_la_fiche_wasm_conserve_le_verdict_du_manifeste_et_refuse_l_absence() {
    let _environment = crate::lock_environment();
    let directory = PluginDirectory::new();
    for (id, minimum, attendu) in [
        ("present-i4265", None, true),
        ("futur-i4265", Some("999.0.0"), false),
    ] {
        let plugin = directory.install(id);
        let path = plugin.join("manifest.json");
        let mut manifest: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        if let Some(minimum) = minimum {
            manifest["min_server_version"] = json!(minimum);
        } else {
            manifest
                .as_object_mut()
                .unwrap()
                .remove("min_server_version");
        }
        std::fs::write(path, manifest.to_string()).unwrap();
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let app = tune_server::routes::router(state);
        let (status, fiche) = call(&app, "GET", &format!("/api/v1/plugins/{id}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            fiche["compatible"], attendu,
            "le manifeste present doit garder son verdict"
        );
        assert!(fiche.get("reason").is_none());
    }
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state);
    let (_, fiche) = call(&app, "GET", "/api/v1/plugins/absent_i4265").await;
    assert_eq!(
        fiche["compatible"], false,
        "un dossier WASM ne rend pas un nom absent compatible"
    );
    assert_eq!(fiche["reason"], "not_compiled_into_this_server");

    // Un scan impossible ne doit pas transformer l'absence en compatibilite.
    let fichier = directory.dir.path().join("pas_un_dossier");
    std::fs::write(&fichier, "fixture").unwrap();
    unsafe {
        std::env::set_var("TUNE_PLUGINS_DIR", &fichier);
    }
    let (_, fiche) = call(&app, "GET", "/api/v1/plugins/absent_i4265").await;
    assert_eq!(
        fiche["compatible"], false,
        "un scan echoue ne doit pas promettre une compatibilite"
    );
    assert_eq!(fiche["reason"], "not_compiled_into_this_server");
}
