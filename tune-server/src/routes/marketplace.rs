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

/// Read installed plugin slugs + versions from the settings table.
fn installed_plugins(settings: &SettingsRepo) -> Vec<(String, String)> {
    let raw = settings
        .get("marketplace_installed")
        .ok()
        .flatten()
        .unwrap_or_else(|| "[]".to_string());
    serde_json::from_str::<Vec<InstalledRecord>>(&raw)
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.slug, r.version))
        .collect()
}

fn save_installed(settings: &SettingsRepo, list: &[(String, String)]) {
    let records: Vec<InstalledRecord> = list
        .iter()
        .map(|(s, v)| InstalledRecord {
            slug: s.clone(),
            version: v.clone(),
        })
        .collect();
    if let Ok(json) = serde_json::to_string(&records) {
        settings.set("marketplace_installed", &json).ok();
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct InstalledRecord {
    slug: String,
    version: String,
}

/// Merge installed state into a catalog entry.
fn enrich(mut plugin: MarketplacePlugin, installed: &[(String, String)]) -> MarketplacePlugin {
    if let Some((_, ver)) = installed.iter().find(|(s, _)| *s == plugin.slug) {
        plugin.installed = true;
        plugin.installed_version = Some(ver.clone());
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

    // Download the plugin archive.
    match marketplace.download(&slug).await {
        Ok(data) => {
            info!(slug = %slug, bytes = data.len(), "marketplace_plugin_downloaded");

            // Track installation in settings.
            let settings = SettingsRepo::with_backend(state.backend.clone());
            let mut installed = installed_plugins(&settings);
            // Remove old entry if upgrading.
            installed.retain(|(s, _)| *s != slug);
            installed.push((slug.clone(), plugin.version.clone()));
            save_installed(&settings, &installed);

            // Also set the per-plugin installed/enabled keys for compat with
            // the existing /plugins routes.
            let key = format!("plugin_{slug}_installed");
            settings.set(&key, "true").ok();
            let enabled_key = format!("plugin_{slug}_enabled");
            settings.set(&enabled_key, "true").ok();

            Json(json!({
                "status": "installed",
                "slug": slug,
                "version": plugin.version,
                "bytes": data.len(),
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
    installed.retain(|(s, _)| *s != slug);

    if installed.len() == before {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "plugin_not_installed", "slug": slug })),
        )
            .into_response();
    }

    save_installed(&settings, &installed);

    // Clean per-plugin settings keys.
    settings.delete(&format!("plugin_{slug}_installed")).ok();
    settings.delete(&format!("plugin_{slug}_enabled")).ok();

    info!(slug = %slug, "marketplace_plugin_uninstalled");

    Json(json!({ "status": "uninstalled", "slug": slug })).into_response()
}

// ---------------------------------------------------------------------------
// GET /marketplace/plugins/installed — List installed plugins with status.
// ---------------------------------------------------------------------------

async fn list_installed_plugins(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let installed = installed_plugins(&settings);

    let plugins: Vec<Value> = installed
        .iter()
        .map(|(slug, version)| {
            let enabled_key = format!("plugin_{slug}_enabled");
            let enabled = settings
                .get(&enabled_key)
                .ok()
                .flatten()
                .map(|v| v == "true")
                .unwrap_or(true);

            json!({
                "slug": slug,
                "installed_version": version,
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
    let current_version = match installed.iter().find(|(s, _)| *s == slug) {
        Some((_, v)) => v.clone(),
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

    // Download new version.
    match marketplace.download(&slug).await {
        Ok(data) => {
            info!(
                slug = %slug,
                from = %current_version,
                to = %plugin.version,
                bytes = data.len(),
                "marketplace_plugin_updated"
            );

            // Update installed record.
            let mut installed = installed_plugins(&settings);
            installed.retain(|(s, _)| *s != slug);
            installed.push((slug.clone(), plugin.version.clone()));
            save_installed(&settings, &installed);

            Json(json!({
                "status": "updated",
                "slug": slug,
                "from_version": current_version,
                "to_version": plugin.version,
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
