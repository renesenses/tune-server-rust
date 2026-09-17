//! Bridges the standalone SDK plugins to Tune's installed-plugin catalogue.
//! The implementations depend only on the SDK; DB, routes and licence are host
//! adapters. Existing screens/API remain valid during source migration.
use crate::state::AppState;
use async_trait::async_trait;
use serde_json::{Value, json};
use tune_core::plugin_sdk::{PluginContext, PluginLoader, TunePlugin};
struct PremiumAudio {
    id: &'static str,
}
#[async_trait]
impl TunePlugin for PremiumAudio {
    fn name(&self) -> &str {
        self.id
    }
    fn version(&self) -> &str {
        "0.1.0"
    }
    fn description(&self) -> &str {
        match self.id {
            "equalizer" => "Égaliseur : profil, graphique, paramétrique, presets et AutoEq",
            "crossfeed" => "Crossfeed casque : intensité et retard, état conservé à chaud",
            "converter" => "Convertisseur audio : codecs de l'hôte, métadonnées et exports",
            _ => "Dé-ploc : silence en tête/queue et passages par zéro, FLAC/WAV",
        }
    }
    fn default_enabled(&self) -> bool {
        false
    }
    fn config_schema(&self) -> Value {
        descriptor(self.id)
    }
    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        let raw = match self.id {
            "equalizer" => include_str!("../../sdk/tune-plugin-equalizer/manifest.json"),
            "crossfeed" => include_str!("../../sdk/tune-plugin-crossfeed/manifest.json"),
            "converter" => include_str!("../../sdk/tune-plugin-converter/manifest.json"),
            "declick" => include_str!("../../sdk/tune-plugin-declick/manifest.json"),
            _ => return Err("unknown premium audio plugin".into()),
        };
        let manifest: tune_plugin_sdk::manifest::Manifest =
            serde_json::from_str(raw).map_err(|e| e.to_string())?;
        let capabilities = tune_plugin_sdk::manifest::reference_host_capabilities();
        manifest
            .negotiate(&capabilities)
            .map_err(|e| format!("SDK negotiation: {e:?}"))?;
        let info = descriptor(self.id);
        ctx.register_router(axum::Router::new().route(
            "/",
            axum::routing::get(move || {
                let info = info.clone();
                async move { axum::Json(info) }
            }),
        ));
        Ok(())
    }
    async fn teardown(&mut self) -> Result<(), String> {
        Ok(())
    }
}
pub fn descriptor(id: &str) -> Value {
    let (view, endpoints, entitlement, kind) = match id {
        "equalizer" => (
            "equalizer",
            vec![
                "/api/v1/eq",
                "/api/v1/zones/{zone}/eq",
                "/api/v1/zones/{zone}/dsp",
            ],
            "dsp_eq",
            "dsp",
        ),
        "crossfeed" => (
            "crossfeed",
            vec!["/api/v1/zones/{zone}/dsp"],
            "crossfeed",
            "dsp",
        ),
        "converter" => (
            "converter",
            vec!["/api/v1/converter"],
            "batch_converter",
            "batch",
        ),
        "declick" => ("declick", vec!["/api/v1/declick"], "declick", "batch"),
        _ => return Value::Null,
    };
    json!({"sdk": {"major":0,"minor":1}, "kind":kind, "premium":tune_core::audio::premium_plugins::requires_premium(id), "entitlement":entitlement, "configuration_version":1,"native_loaded":tune_plugin_native::provider(id).is_some(),"activation_error":tune_plugin_native::failure(id),
        "ui":{"view":view,"zone_scoped":kind=="dsp","endpoints":endpoints,"levels_event":"playback.audio_levels","levels_premium":false},
        "lifecycle":{"disable":"pending_restart","uninstall":"preserve_configuration","running_jobs":"finish_or_explicit_cancel"}})
}
pub async fn register(loader: &PluginLoader, state: &AppState) {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    if let Err(e) = tune_core::audio::premium_plugins::migrate_for_account(
        &settings,
        state.license.is_premium().await,
    ) {
        tracing::error!(error=%e, "premium_audio_migration_failed");
        return;
    }
    crate::native_audio::load_installed(&settings);
    for id in tune_core::audio::premium_plugins::IDS {
        loader.register(Box::new(PremiumAudio { id })).await;
    }
}
pub fn require_installed(state: &AppState, id: &str) -> Result<(), axum::response::Response> {
    use axum::response::IntoResponse;
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    if tune_core::audio::premium_plugins::enabled(&settings, id) {
        Ok(())
    } else {
        Err((axum::http::StatusCode::CONFLICT,axum::Json(json!({"error":"plugin_unavailable","plugin":id,"message":"Ce plugin doit être installé et activé."}))).into_response())
    }
}

pub async fn require_entitlement(
    state: &AppState,
    id: &str,
) -> Result<(), axum::response::Response> {
    if tune_core::audio::premium_plugins::contains(id) {
        crate::premium_guard::require_premium(&state.license, crate::native_audio::feature(id))
            .await
    } else {
        Ok(())
    }
}
