use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tracing::info;

use tune_core::cloud::plugins::{MarketplacePlugin, PluginMarketplace};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Feature;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/plugins", get(list_marketplace_plugins))
        .route("/plugins/installed", get(list_installed_plugins))
        .route("/plugins/{slug}", get(get_plugin_detail))
        .route("/plugins/{slug}/install", post(install_plugin))
        .route("/plugins/{slug}/uninstall", post(uninstall_plugin))
        .route("/plugins/{slug}/update", post(update_plugin))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read installed plugin records from the settings table.
fn installed_plugins(settings: &SettingsRepo) -> Vec<InstalledRecord> {
    let raw = settings
        .get("marketplace_installed")
        .ok()
        .flatten()
        .unwrap_or_else(|| "[]".to_string());
    serde_json::from_str::<Vec<InstalledRecord>>(&raw).unwrap_or_default()
}

fn save_installed(settings: &SettingsRepo, records: &[InstalledRecord]) {
    if let Ok(json) = serde_json::to_string(records) {
        settings.set("marketplace_installed", &json).ok();
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct InstalledRecord {
    slug: String,
    version: String,
    /// Manifest id = on-disk directory under `plugins_dir`. Optional so
    /// records written before real installs existed still parse.
    #[serde(default)]
    plugin_id: Option<String>,
}

/// Merge installed state into a catalog entry.
fn enrich(mut plugin: MarketplacePlugin, installed: &[InstalledRecord]) -> MarketplacePlugin {
    if let Some(rec) = installed.iter().find(|r| r.slug == plugin.slug) {
        plugin.installed = true;
        plugin.installed_version = Some(rec.version.clone());
    }
    plugin
}

/// Returns true when the plugin is free (price is None or 0).
fn is_free_plugin(plugin: &MarketplacePlugin) -> bool {
    plugin.price.map(|p| p <= 0.0).unwrap_or(true)
}

// ---------------------------------------------------------------------------
// GET /marketplace/plugins — Public. Browse catalog.
// ---------------------------------------------------------------------------

async fn list_marketplace_plugins(State(state): State<AppState>) -> Json<Value> {
    let marketplace = PluginMarketplace::default();
    let catalog = marketplace.list().await;

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let installed = installed_plugins(&settings);

    let plugins: Vec<MarketplacePlugin> =
        catalog.into_iter().map(|p| enrich(p, &installed)).collect();

    Json(json!({
        "plugins": plugins,
        "count": plugins.len(),
    }))
}

// ---------------------------------------------------------------------------
// GET /marketplace/plugins/{slug} — Public. Plugin detail.
// ---------------------------------------------------------------------------

async fn get_plugin_detail(
    Path(slug): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let marketplace = PluginMarketplace::default();

    match marketplace.detail(&slug).await {
        Some(plugin) => {
            let settings = SettingsRepo::with_backend(state.backend.clone());
            let installed = installed_plugins(&settings);
            let plugin = enrich(plugin, &installed);
            Json(json!(plugin)).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "plugin_not_found", "slug": slug })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// POST /marketplace/plugins/{slug}/install — Premium for paid plugins.
// Free plugins can be installed by everyone.
// ---------------------------------------------------------------------------

async fn install_plugin(
    Path(slug): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let marketplace = PluginMarketplace::default();

    // Fetch plugin info from marketplace to check price.
    let plugin = match marketplace.detail(&slug).await {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "plugin_not_found", "slug": slug })),
            )
                .into_response();
        }
    };

    // Gate: paid plugins require Premium.
    if !is_free_plugin(&plugin) {
        if let Err(resp) =
            crate::premium_guard::require_premium(&state.license, Feature::PluginMarketplace).await
        {
            return resp;
        }
    }

    // Download the plugin archive. The Laravel store keys the download route
    // on the package `name` (`Plugin::where('name', …)`), not the slug.
    match marketplace.download(&plugin.name).await {
        Ok(data) => {
            info!(slug = %slug, bytes = data.len(), "marketplace_plugin_downloaded");

            // Persist the archive where `load_wasm_plugins` scans at startup.
            // Before this, the downloaded bytes were dropped on the floor and
            // "installed" was nothing but a settings flag — the plugin never
            // existed on disk, which is why native builds had no plugins.
            let plugin_id = match crate::plugins::persist_wasm_archive(&data) {
                Ok(id) => id,
                Err(e) => {
                    return (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({ "error": "install_failed", "detail": e })),
                    )
                        .into_response();
                }
            };

            // Track installation in settings.
            let settings = SettingsRepo::with_backend(state.backend.clone());
            let mut installed = installed_plugins(&settings);
            // Remove old entry if upgrading.
            installed.retain(|r| r.slug != slug);
            installed.push(InstalledRecord {
                slug: slug.clone(),
                version: plugin.version.clone(),
                plugin_id: Some(plugin_id.clone()),
            });
            save_installed(&settings, &installed);

            // Also set the per-plugin installed/enabled keys for compat with
            // the existing /plugins routes. Keyed on the manifest id — that is
            // what `load_wasm_plugins` checks at startup.
            let key = format!("plugin_{plugin_id}_installed");
            settings.set(&key, "true").ok();
            let enabled_key = format!("plugin_{plugin_id}_enabled");
            settings.set(&enabled_key, "true").ok();

            Json(json!({
                "status": "installed",
                "slug": slug,
                "plugin_id": plugin_id,
                "version": plugin.version,
                "bytes": data.len(),
                // The wasm registry is a startup-published OnceLock; the
                // plugin loads on next boot.
                "restart_required": true,
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "install_failed", "detail": e })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// POST /marketplace/plugins/{slug}/uninstall — Remove a plugin.
// ---------------------------------------------------------------------------

async fn uninstall_plugin(
    Path(slug): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut installed = installed_plugins(&settings);
    let before = installed.len();
    let plugin_id = installed
        .iter()
        .find(|r| r.slug == slug)
        .and_then(|r| r.plugin_id.clone());
    installed.retain(|r| r.slug != slug);

    if installed.len() == before {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "plugin_not_installed", "slug": slug })),
        )
            .into_response();
    }

    save_installed(&settings, &installed);

    // Remove the on-disk wasm plugin, when this install wrote one. The loaded
    // instance (if any) lives until restart — the registry is a OnceLock.
    let mut removed_dir = false;
    if let Some(id) = &plugin_id {
        match crate::plugins::remove_wasm_dir(id) {
            Ok(removed) => removed_dir = removed,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": "uninstall_failed", "detail": e })),
                )
                    .into_response();
            }
        }
    }

    // Clean per-plugin settings keys (manifest id keys, plus legacy slug keys).
    for key_base in plugin_id.iter().chain(std::iter::once(&slug)) {
        settings
            .delete(&format!("plugin_{key_base}_installed"))
            .ok();
        settings.delete(&format!("plugin_{key_base}_enabled")).ok();
    }

    info!(slug = %slug, removed_dir, "marketplace_plugin_uninstalled");

    Json(json!({
        "status": "uninstalled",
        "slug": slug,
        "restart_required": removed_dir,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// GET /marketplace/plugins/installed — List installed plugins with status.
// ---------------------------------------------------------------------------

async fn list_installed_plugins(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let installed = installed_plugins(&settings);

    let plugins: Vec<Value> = installed
        .iter()
        .map(|rec| {
            // The startup loader keys the enabled switch on the manifest id.
            let key_base = rec.plugin_id.as_ref().unwrap_or(&rec.slug);
            let enabled_key = format!("plugin_{key_base}_enabled");
            let enabled = settings
                .get(&enabled_key)
                .ok()
                .flatten()
                .map(|v| v == "true")
                .unwrap_or(true);

            json!({
                "slug": rec.slug,
                "plugin_id": rec.plugin_id,
                "installed_version": rec.version,
                "enabled": enabled,
            })
        })
        .collect();

    Json(json!({
        "plugins": plugins,
        "count": plugins.len(),
    }))
}

// ---------------------------------------------------------------------------
// POST /marketplace/plugins/{slug}/update — Premium for paid plugins.
// ---------------------------------------------------------------------------

async fn update_plugin(
    Path(slug): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let installed = installed_plugins(&settings);

    // Must already be installed.
    let current_version = match installed.iter().find(|r| r.slug == slug) {
        Some(r) => r.version.clone(),
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "plugin_not_installed", "slug": slug })),
            )
                .into_response();
        }
    };

    // Fetch latest from marketplace.
    let marketplace = PluginMarketplace::default();
    let plugin = match marketplace.detail(&slug).await {
        Some(p) => p,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "plugin_not_found", "slug": slug })),
            )
                .into_response();
        }
    };

    // Already up to date?
    if plugin.version == current_version {
        return Json(json!({
            "status": "already_up_to_date",
            "slug": slug,
            "version": current_version,
        }))
        .into_response();
    }

    // Gate: paid plugins require Premium.
    if !is_free_plugin(&plugin) {
        if let Err(resp) =
            crate::premium_guard::require_premium(&state.license, Feature::PluginMarketplace).await
        {
            return resp;
        }
    }

    // Download new version. Keyed on the package name, like install.
    match marketplace.download(&plugin.name).await {
        Ok(data) => {
            // Overwrite the on-disk plugin with the new version.
            let plugin_id = match crate::plugins::persist_wasm_archive(&data) {
                Ok(id) => id,
                Err(e) => {
                    return (
                        StatusCode::BAD_GATEWAY,
                        Json(json!({ "error": "update_failed", "detail": e })),
                    )
                        .into_response();
                }
            };

            info!(
                slug = %slug,
                from = %current_version,
                to = %plugin.version,
                bytes = data.len(),
                "marketplace_plugin_updated"
            );

            // Update installed record.
            let mut installed = installed_plugins(&settings);
            installed.retain(|r| r.slug != slug);
            installed.push(InstalledRecord {
                slug: slug.clone(),
                version: plugin.version.clone(),
                plugin_id: Some(plugin_id),
            });
            save_installed(&settings, &installed);

            Json(json!({
                "status": "updated",
                "slug": slug,
                "from_version": current_version,
                "to_version": plugin.version,
                "restart_required": true,
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "error": "update_failed", "detail": e })),
        )
            .into_response(),
    }
}
