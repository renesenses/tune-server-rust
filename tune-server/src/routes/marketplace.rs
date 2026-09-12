use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use tracing::{info, warn};

use tune_core::cloud::plugins::{MarketplacePlugin, PluginMarketplace};
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Feature;

use crate::routes::cloud_error;
use crate::state::AppState;
use tune_http_types::panne_sql::OuDefautJournalise;

// ---------------------------------------------------------------------------
// Plugin artifact signatures (audit item 8)
// ---------------------------------------------------------------------------

/// Trusted **minisign** public key for marketplace plugin artifacts.
///
/// A WASM plugin runs inside this server, so a compromised marketplace pushing
/// a malicious artifact is code execution. The same construction as the signed
/// self-update (`system/update.rs`), with a key of its own: an update key
/// compromise and a plugin key compromise should not imply one another, and
/// the plugin key has to be usable by whatever signs artifacts in the
/// marketplace repo.
///
/// ROLLOUT: empty until the marketplace actually signs. While it is empty,
/// enforcement is impossible and [`signature_enforced`] stays false whatever
/// the setting says.
const PLUGIN_PUBLIC_KEY: &str = "";

/// Settings key gating enforcement. Default **false**: the marketplace does
/// not sign anything yet, so refusing unsigned artifacts would break every
/// install today.
const REQUIRE_SIGNATURE_SETTING: &str = "plugin_signature_required";

/// Whether an unsigned or badly-signed artifact must be refused.
///
/// Requires *both* an embedded key and the operator opting in. Without the key
/// there is nothing to verify against, and silently "enforcing" with no key
/// would be security theatre — the worst outcome, since the UI would claim
/// plugins are verified.
fn signature_enforced(settings: &SettingsRepo) -> bool {
    if PLUGIN_PUBLIC_KEY.is_empty() {
        return false;
    }
    settings
        .get(REQUIRE_SIGNATURE_SETTING)
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// Verify a downloaded artifact against its detached minisign signature.
///
/// Verification runs whenever a key and a signature are both present, even
/// when enforcement is off — so a mismatch is visible in the logs during
/// rollout. Only the *consequence* of a failure depends on the setting.
async fn verify_plugin_signature(
    marketplace: &PluginMarketplace,
    settings: &SettingsRepo,
    plugin_name: &str,
    bytes: &[u8],
) -> Result<(), String> {
    let enforced = signature_enforced(settings);

    if PLUGIN_PUBLIC_KEY.is_empty() {
        // Nothing to verify against. Never fatal: enforcement is already
        // impossible, and failing here would brick installs on every build
        // shipped before the marketplace signs.
        return Ok(());
    }

    let signature = match marketplace.download_signature(plugin_name).await {
        Ok(Some(sig)) => sig,
        Ok(None) => {
            if enforced {
                return Err(format!(
                    "plugin '{plugin_name}' is not signed and signature verification is required"
                ));
            }
            warn!(plugin = %plugin_name, "marketplace_plugin_unsigned");
            return Ok(());
        }
        Err(e) => {
            // A transport failure is not proof of absence. Refusing here when
            // enforcement is on is the safe reading; downgrading it to "no
            // signature" would let anyone who can break the connection strip
            // the check.
            if enforced {
                return Err(format!("could not fetch plugin signature: {e}"));
            }
            warn!(plugin = %plugin_name, error = %e, "marketplace_signature_fetch_failed");
            return Ok(());
        }
    };

    let pk = minisign_verify::PublicKey::from_base64(PLUGIN_PUBLIC_KEY)
        .map_err(|e| format!("invalid embedded plugin public key: {e}"))?;
    let sig = minisign_verify::Signature::decode(&signature)
        .map_err(|e| format!("invalid plugin signature: {e}"))?;

    match pk.verify(bytes, &sig, false) {
        Ok(()) => {
            info!(plugin = %plugin_name, "marketplace_plugin_signature_verified");
            Ok(())
        }
        Err(_) => {
            let msg = format!("plugin '{plugin_name}' signature does not match the trusted key");
            if enforced {
                Err(msg)
            } else {
                // Loud, but not fatal while the setting is off: this is
                // exactly what the rollout period is for.
                warn!(plugin = %plugin_name, "marketplace_plugin_signature_mismatch");
                Ok(())
            }
        }
    }
}

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

/// La clef de `settings` où vit la liste des greffons installés.
const SETTINGS_KEY_INSTALLED: &str = "marketplace_installed";

/// Read installed plugin records from the settings table.
///
/// Rend `Err` sur panne de base ou JSON illisible — la même primitive que le
/// Developer API (#2795). La liste vide ne veut plus dire qu'une chose :
/// aucun greffon installé.
fn installed_plugins(settings: &SettingsRepo) -> Result<Vec<InstalledRecord>, String> {
    settings.get_json_list(SETTINGS_KEY_INSTALLED)
}

/// Une panne de stockage se dit (#2795) : un `installed` annoncé sur une
/// écriture perdue, c'est le « 200 pour rien » que la #2132 vient de retirer à
/// `POST /plugins/{nom}/install`.
fn panne_de_stockage(quoi: &str, erreur: String) -> axum::response::Response {
    warn!(quoi, erreur = %erreur, "marketplace_stockage_en_echec");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "storage_failure", "detail": erreur })),
    )
        .into_response()
}

/// Refus du magasin de greffons, rendu au client.
///
/// Hors limite atteinte, la forme d'origine est conservée mot pour mot :
/// `{"error": "<code>", "detail": "<texte technique>"}` au statut d'avant.
/// Sur un 429 le code machine devient `rate_limited` — plus précis que
/// `install_failed` : ce n'est pas l'installation qui a échoué, c'est le nuage
/// qui refuse pour l'instant — et le corps porte le délai et un message dans la
/// langue de l'interface (#2178). `detail` reste présent dans les deux cas.
fn refus_du_magasin(
    code: &str,
    err: &tune_core::cloud::refusal::CloudError,
    headers: &HeaderMap,
) -> axum::response::Response {
    if err.is_rate_limited() {
        return cloud_error::reponse(
            err,
            headers,
            StatusCode::BAD_GATEWAY,
            json!({ "detail": err.to_string() }),
        );
    }
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({ "error": code, "detail": err.to_string() })),
    )
        .into_response()
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
    // Catalogue public : une panne de base ne doit pas effacer la vitrine.
    // On dégrade le seul champ concerné (`installed`) et on le DIT — ce chemin
    // n'écrit rien, il ne peut donc pas perdre la liste.
    let installed = installed_plugins(&settings).ou_defaut_journalise();

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
            let installed = installed_plugins(&settings).ou_defaut_journalise();
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
    headers: HeaderMap,
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

            // Authenticate before anything touches disk: a WASM plugin runs
            // inside this process, so unverified bytes are code execution.
            let settings = SettingsRepo::with_backend(state.backend.clone());
            if let Err(e) =
                verify_plugin_signature(&marketplace, &settings, &plugin.name, &data).await
            {
                warn!(slug = %slug, error = %e, "marketplace_plugin_signature_rejected");
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "error": "signature_invalid", "detail": e })),
                )
                    .into_response();
            }

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

            // Track installation in settings (`settings` was bound above for
            // the signature check).
            //
            // L'ORDRE compte et ne doit pas s'inverser : la liste d'abord, les
            // drapeaux par greffon ensuite. `load_wasm_plugins` ne charge au
            // démarrage que ce que `plugin_<id>_installed` autorise — écrire ce
            // drapeau avant la liste ferait charger, après un échec, un greffon
            // que l'écran ne montre nulle part.
            let enregistrement = InstalledRecord {
                slug: slug.clone(),
                version: plugin.version.clone(),
                plugin_id: Some(plugin_id.clone()),
            };
            let a_remplacer = slug.clone();
            if let Err(e) = settings.update_json_list::<InstalledRecord, _, _>(
                SETTINGS_KEY_INSTALLED,
                move |installed| {
                    // Remove old entry if upgrading.
                    installed.retain(|r| r.slug != a_remplacer);
                    installed.push(enregistrement);
                    Ok(())
                },
            ) {
                // L'archive est sur le disque mais aucun drapeau ne l'autorise :
                // elle reste inerte au prochain démarrage. On ne l'efface pas —
                // sur une mise à jour, `persist_wasm_archive` a écrasé le
                // répertoire de la version précédente, et le supprimer
                // désinstallerait un greffon que personne n'a demandé à retirer.
                return panne_de_stockage("installation", e);
            }

            // Also set the per-plugin installed/enabled keys for compat with
            // the existing /plugins routes. Keyed on the manifest id — that is
            // what `load_wasm_plugins` checks at startup.
            let key = format!("plugin_{plugin_id}_installed");
            if let Err(e) = settings.set(&key, "true") {
                return panne_de_stockage("drapeau_installe", e);
            }
            let enabled_key = format!("plugin_{plugin_id}_enabled");
            if let Err(e) = settings.set(&enabled_key, "true") {
                return panne_de_stockage("drapeau_actif", e);
            }

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
        Err(e) => refus_du_magasin("install_failed", &e, &headers),
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

    // Retrait et persistance dans la même transaction : le répertoire n'est
    // effacé qu'après, et seulement si l'enregistrement a bien disparu.
    let a_retirer = slug.clone();
    let plugin_id = match settings.update_json_list::<InstalledRecord, _, _>(
        SETTINGS_KEY_INSTALLED,
        move |installed| {
            let avant = installed.len();
            let id = installed
                .iter()
                .find(|r| r.slug == a_retirer)
                .and_then(|r| r.plugin_id.clone());
            installed.retain(|r| r.slug != a_retirer);
            Ok((installed.len() != avant).then_some(id))
        },
    ) {
        Ok(Some(id)) => id,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "plugin_not_installed", "slug": slug })),
            )
                .into_response();
        }
        Err(e) => return panne_de_stockage("desinstallation", e),
    };

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

async fn list_installed_plugins(State(state): State<AppState>) -> axum::response::Response {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    // Ici la liste vide EST la réponse : annoncer « aucun greffon installé »
    // sur une base illisible enverrait désinstaller puis réinstaller.
    let installed = match installed_plugins(&settings) {
        Ok(i) => i,
        Err(e) => return panne_de_stockage("liste_installes", e),
    };

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
    .into_response()
}

// ---------------------------------------------------------------------------
// POST /marketplace/plugins/{slug}/update — Premium for paid plugins.
// ---------------------------------------------------------------------------

async fn update_plugin(
    Path(slug): Path<String>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    // Une base illisible ne doit pas se lire « ce greffon n'est pas installé » :
    // ce 404-là envoie réinstaller par-dessus une installation saine.
    let installed = match installed_plugins(&settings) {
        Ok(i) => i,
        Err(e) => return panne_de_stockage("lecture_installes", e),
    };

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
            // Same gate as install: an update is just as good a delivery
            // vehicle for a malicious artifact, and it overwrites a plugin the
            // user already trusts.
            if let Err(e) =
                verify_plugin_signature(&marketplace, &settings, &plugin.name, &data).await
            {
                warn!(slug = %slug, error = %e, "marketplace_plugin_signature_rejected");
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({ "error": "signature_invalid", "detail": e })),
                )
                    .into_response();
            }

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

            // Update installed record.
            let enregistrement = InstalledRecord {
                slug: slug.clone(),
                version: plugin.version.clone(),
                plugin_id: Some(plugin_id),
            };
            let a_remplacer = slug.clone();
            if let Err(e) = settings.update_json_list::<InstalledRecord, _, _>(
                SETTINGS_KEY_INSTALLED,
                move |installed| {
                    installed.retain(|r| r.slug != a_remplacer);
                    installed.push(enregistrement);
                    Ok(())
                },
            ) {
                return panne_de_stockage("mise_a_jour", e);
            }

            // Après la persistance, jamais avant : une trace « mis à jour » sur
            // une écriture perdue fait chercher le défaut à l'endroit où il
            // n'est pas.
            info!(
                slug = %slug,
                from = %current_version,
                to = %plugin.version,
                bytes = data.len(),
                "marketplace_plugin_updated"
            );

            Json(json!({
                "status": "updated",
                "slug": slug,
                "from_version": current_version,
                "to_version": plugin.version,
                "restart_required": true,
            }))
            .into_response()
        }
        Err(e) => refus_du_magasin("update_failed", &e, &headers),
    }
}

#[cfg(test)]
mod signature_tests {
    use super::{PLUGIN_PUBLIC_KEY, REQUIRE_SIGNATURE_SETTING};

    /// Mirrors `signature_enforced`, which needs a `SettingsRepo` (and so a
    /// database) to call directly. The rule under test is the guard that keeps
    /// the rollout honest, not the settings lookup.
    fn enforced(key: &str, setting: Option<&str>) -> bool {
        if key.is_empty() {
            return false;
        }
        setting == Some("true")
    }

    /// The rollout invariant: with no embedded key there is nothing to verify
    /// against, so enforcement must stay off even if an operator flips the
    /// setting. Otherwise the UI would claim plugins are verified while every
    /// install either passes unchecked or fails for the wrong reason.
    #[test]
    fn enforcement_is_impossible_without_an_embedded_key() {
        assert!(!enforced("", Some("true")));
        assert!(!enforced("", None));
    }

    #[test]
    fn enforcement_requires_opting_in() {
        let key = "RWTestKeyMaterialNotUsedForVerification";
        assert!(
            !enforced(key, None),
            "default must not break installs today"
        );
        assert!(!enforced(key, Some("false")));
        assert!(enforced(key, Some("true")));
    }

    /// Guards the rollout state itself: while the key is empty this build
    /// cannot enforce anything, and the accompanying marketplace-side work is
    /// still outstanding. Filling the key should make this test fail, as a
    /// prompt to flip the default and update the docs.
    #[test]
    fn the_plugin_key_is_still_pending_marketplace_signing() {
        assert!(
            PLUGIN_PUBLIC_KEY.is_empty(),
            "a key is embedded — enable {REQUIRE_SIGNATURE_SETTING} by default and drop this test"
        );
    }
}
