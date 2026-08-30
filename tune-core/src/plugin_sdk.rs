use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::event_bus::{EventBus, TuneEvent};
use crate::license::{Feature, LicenseManager};
use crate::outputs::traits::OutputTarget;

/// The plugin ABI generation. A plugin declares the version it was built
/// against via [`TunePlugin::protocol_version`]; [`PluginLoader::setup_all`]
/// refuses to load a plugin whose major version differs.
///
/// Bump the major on any breaking change to [`TunePlugin`] or
/// [`PluginContext`]; bump the minor when adding a backward-compatible hook.
/// The Python host had the same constant (`PROTOCOL_VERSION`) but only
/// *warned* on mismatch — here it is enforced, because a Rust plugin that
/// disagrees about the trait layout is a crash, not a degraded feature.
pub const PLUGIN_PROTOCOL_VERSION: (u32, u32) = (1, 0);

/// A zone a plugin wants the host to create on its behalf.
///
/// Plugins that expose a virtual output (a visualiser, an EQ, a stats
/// exporter) generally
/// want a zone pointing at it to exist so the device is selectable in the UI
/// without the user hand-crafting one.
#[derive(Debug, Clone)]
pub struct ZoneRequest {
    pub name: String,
    pub output_type: String,
    pub device_id: String,
}

/// Everything a plugin asked the host to install during `setup`.
///
/// [`PluginContext`] only *collects* these — it holds no lock on the output
/// registry and never touches the axum router, so a plugin's `setup` can
/// never deadlock against the host's startup path. The host drains this
/// afterwards via [`PluginLoader::take_registrations`] and applies it all at
/// once, at a point where it knows the registry and router are free.
#[derive(Default)]
pub struct PluginRegistrations {
    pub outputs: Vec<Box<dyn OutputTarget>>,
    /// `(plugin name, router)`. The host derives the mount path from the
    /// name — plugins do not choose their own prefix. Requires the
    /// `plugin-http` feature.
    #[cfg(feature = "plugin-http")]
    pub routers: Vec<(String, axum::Router<()>)>,
    pub zones: Vec<ZoneRequest>,
}

impl PluginRegistrations {
    pub fn is_empty(&self) -> bool {
        #[cfg(feature = "plugin-http")]
        if !self.routers.is_empty() {
            return false;
        }
        self.outputs.is_empty() && self.zones.is_empty()
    }

    fn absorb(&mut self, other: PluginRegistrations) {
        self.outputs.extend(other.outputs);
        #[cfg(feature = "plugin-http")]
        self.routers.extend(other.routers);
        self.zones.extend(other.zones);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub enabled: bool,
    pub config_schema: serde_json::Value,
}

/// A compiled-in plugin that `setup_all` did not load — either an opt-in
/// plugin the user has not installed yet, or a default-on one they disabled.
///
/// Captured before the resident set is pruned so the plugin manager can still
/// list it (and offer "Install" / "Enable") instead of it vanishing entirely.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailablePluginInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub config_schema: serde_json::Value,
    /// `true` = dormant because it is opt-in and not yet installed (offer
    /// "Install"); `false` = a default-on plugin the user disabled (offer
    /// "Enable").
    pub opt_in: bool,
    /// Le module Premium exigé, s'il y en a un — le `display_name` de la
    /// [`Feature`], tel que la grille des modules l'affiche.
    ///
    /// Sans ce champ, le gestionnaire proposerait « Installer » à quelqu'un
    /// dont les routes seront refusées juste après : l'utilisateur redémarre,
    /// et n'obtient qu'un 402. Le porter ici permet d'afficher le cadenas AVANT
    /// le clic, comme le fait déjà la grille des modules.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_feature: Option<String>,
}

pub struct PluginContext {
    /// Base URL of this server's own HTTP API, e.g. `http://127.0.0.1:8080`.
    ///
    /// Usable only *after* startup finishes. The host sets plugins up while the
    /// listener is bound but not yet accepting, so an HTTP request to this URL
    /// from inside `setup` sits in the accept backlog until it times out. Read
    /// the library through [`PluginContext::db`] during setup and keep this for
    /// later, once events start arriving.
    pub api_base_url: String,
    pub data_dir: PathBuf,
    pub event_bus: Option<EventBus>,
    /// La licence du serveur, pour que le greffon ADAPTE sa réponse.
    ///
    /// ⚠️ POURQUOI ICI, ET PAS UN REFUS AUTOMATIQUE DANS L'HÔTE.
    ///
    /// Un garde monté par l'hôte devant les routes d'un greffon ne sait faire
    /// qu'une chose : ouvrir ou fermer. Or « Concerts » doit un jour servir une
    /// version RÉDUITE aux comptes gratuits, pas une porte close. Un refus
    /// câblé dans l'hôte serait alors à défaire.
    ///
    /// En donnant la licence au greffon, la décision « complet / réduit /
    /// refusé » tient dans UNE fonction, chez lui.
    ///
    /// `None` chez un hôte qui n'en fournit pas (tests, tune-cli) : le greffon
    /// se comporte alors comme SANS Premium, jamais l'inverse — une licence
    /// absente ne s'interprète pas en faveur du doute.
    pub license: Option<Arc<LicenseManager>>,
    plugin_name: String,
    db: Option<Arc<dyn DbBackend>>,
    /// Deferred registrations collected during `setup`. Interior mutability so
    /// plugins receive `&PluginContext` (not `&mut`) and can register from
    /// inside closures without fighting the borrow checker.
    registrations: StdMutex<PluginRegistrations>,
}

impl PluginContext {
    pub fn new(api_base_url: &str, data_dir: PathBuf) -> Self {
        Self {
            api_base_url: api_base_url.to_string(),
            data_dir,
            event_bus: None,
            license: None,
            plugin_name: String::new(),
            db: None,
            registrations: StdMutex::new(PluginRegistrations::default()),
        }
    }

    pub fn with_event_bus(mut self, bus: EventBus) -> Self {
        self.event_bus = Some(bus);
        self
    }

    pub fn with_db(mut self, db: Arc<dyn DbBackend>) -> Self {
        self.db = Some(db);
        self
    }

    pub fn with_license(mut self, license: Arc<LicenseManager>) -> Self {
        self.license = Some(license);
        self
    }

    /// Ce module est-il ouvert sur ce serveur ?
    ///
    /// Rend `false` quand l'hôte ne fournit pas de licence : voir la note sur
    /// [`PluginContext::license`]. Une absence ne vaut pas une autorisation.
    pub async fn feature_licensed(&self, feature: Feature) -> bool {
        match &self.license {
            Some(license) => license.check_feature(feature).await,
            None => false,
        }
    }

    pub fn with_plugin_name(mut self, name: &str) -> Self {
        self.plugin_name = name.to_string();
        self
    }

    /// Read a plugin-specific setting from the database.
    ///
    /// Keys are stored under the prefix `plugin_{name}_{key}` in the
    /// settings table, matching the convention used by the REST routes.
    pub fn get_config(&self, key: &str) -> Option<String> {
        let db = self.db.as_ref()?;
        let repo = SettingsRepo::with_backend(Arc::clone(db));
        let full_key = format!("plugin_{}_{}", self.plugin_name, key);
        repo.get(&full_key).ok().flatten()
    }

    /// Write a plugin-specific setting to the database.
    ///
    /// Keys are stored under the prefix `plugin_{name}_{key}`.
    pub fn set_config(&self, key: &str, value: &str) -> Result<(), String> {
        let db = self.db.as_ref().ok_or("no database backend")?;
        let repo = SettingsRepo::with_backend(Arc::clone(db));
        let full_key = format!("plugin_{}_{}", self.plugin_name, key);
        repo.set(&full_key, value)
    }

    /// Emit an event through the event bus (if available).
    pub fn emit_event(&self, event_type: &str, data: Value) {
        if let Some(bus) = &self.event_bus {
            bus.emit(event_type, data);
        }
    }

    /// The database backend, for plugins that need to query the library
    /// directly (e.g. a plugin skipping albums already in the library).
    pub fn db(&self) -> Option<Arc<dyn DbBackend>> {
        self.db.clone()
    }

    /// The name this plugin was registered under. Also the leaf of `data_dir`.
    pub fn plugin_name(&self) -> &str {
        &self.plugin_name
    }

    /// Expose an audio output. The host registers it with the
    /// `OutputRegistry` after `setup` returns, keyed on `device_id()`.
    ///
    /// This is the Rust counterpart of the Python host's
    /// `register_output_type`. The shape differs deliberately: Python
    /// registered a *factory* keyed by type name and instantiated one output
    /// per zone, whereas `OutputRegistry` is keyed by `device_id`, so a plugin
    /// registers concrete instances. A plugin wanting N outputs calls this N
    /// times.
    pub fn register_output(&self, output: Box<dyn OutputTarget>) {
        match self.registrations.lock() {
            Ok(mut reg) => reg.outputs.push(output),
            Err(_) => self.warn_registration_lost("output"),
        }
    }

    /// Expose HTTP routes. The host mounts them under
    /// `/api/v1/ext/{plugin_name}`, behind the same auth, analytics and
    /// body-limit layers as the rest of `/api/v1`.
    ///
    /// The plugin does **not** choose its own prefix — deliberately. The
    /// Python host let plugins mount anywhere, which let a plugin shadow a
    /// core route (or another plugin's) with no diagnostic. Deriving the
    /// namespace from the plugin name makes collisions impossible and keeps
    /// plugin routes obvious in a request log.
    ///
    /// The router is `Router<()>`: plugins capture their own state in
    /// closures rather than sharing the host's `AppState`, which keeps
    /// `tune-core` free of any dependency on `tune-server`'s types.
    #[cfg(feature = "plugin-http")]
    pub fn register_router(&self, router: axum::Router<()>) {
        match self.registrations.lock() {
            Ok(mut reg) => reg.routers.push((self.plugin_name.clone(), router)),
            Err(_) => self.warn_registration_lost("router"),
        }
    }

    /// Ask the host to create a zone bound to one of this plugin's outputs,
    /// if no zone already targets that `device_id`.
    /// The host only creates the zone if one of this plugin's outputs actually
    /// claimed `device_id` — a zone pointing at a device the plugin does not
    /// own would either be orphaned or, worse, drive somebody else's device.
    pub fn register_zone(&self, name: &str, output_type: &str, device_id: &str) {
        match self.registrations.lock() {
            Ok(mut reg) => reg.zones.push(ZoneRequest {
                name: name.to_string(),
                output_type: output_type.to_string(),
                device_id: device_id.to_string(),
            }),
            Err(_) => self.warn_registration_lost("zone"),
        }
    }

    /// A poisoned registrations mutex means an earlier registration panicked
    /// mid-push. Say so loudly: silently dropping the registration leaves a
    /// plugin convinced it registered something that then never appears, which
    /// is a thoroughly miserable thing to debug.
    fn warn_registration_lost(&self, kind: &str) {
        warn!(
            plugin_name = %self.plugin_name,
            kind,
            "plugin_registration_lost — registrations mutex poisoned"
        );
    }

    /// Drain what this plugin registered. Called by the loader once `setup`
    /// has returned successfully; a plugin whose setup failed is never
    /// drained, so its half-built outputs are dropped rather than installed.
    fn take_registrations(&self) -> PluginRegistrations {
        self.registrations
            .lock()
            .map(|mut r| std::mem::take(&mut *r))
            .unwrap_or_default()
    }
}

#[async_trait]
pub trait TunePlugin: Send + Sync {
    fn name(&self) -> &str;
    fn version(&self) -> &str;
    fn description(&self) -> &str;
    fn config_schema(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    /// Whether this plugin runs unless explicitly turned off (`true`, the
    /// default) or stays dormant until the user installs it on demand
    /// (`false`, "opt-in").
    ///
    /// An opt-in plugin is still compiled into the binary, but `setup_all`
    /// skips it until `plugin_{name}_installed == "true"` — so it surfaces in
    /// the plugin manager as an available, not-yet-installed entry rather than
    /// running by default. DJ and Karaoke override this to `false` (#917
    /// follow-up): niche modes users shouldn't pay for unless they ask.
    fn default_enabled(&self) -> bool {
        true
    }

    /// Whether the plugin manager should OFFER this plugin to the user.
    ///
    /// `true` (the default) puts a dormant plugin in the catalogue, where it
    /// renders as an "Install" button. `false` keeps it out of the catalogue:
    /// it stays compiled, stays tested, and still loads if
    /// `plugin_{name}_installed` is set by hand — but the manager stops
    /// promising it.
    ///
    /// [`default_enabled`](Self::default_enabled) cannot express this.
    /// Returning `false` there makes a plugin opt-in, which is precisely what
    /// makes it *visible* as installable. A plugin whose routes answer but
    /// that no screen in the client can reach needs the opposite: present,
    /// dormant, and silent — otherwise the manager offers an install that
    /// changes nothing the user can see (#2090).
    fn catalogued(&self) -> bool {
        true
    }

    /// Le module Premium que ce greffon exige, s'il en exige un.
    ///
    /// ⚠️ LE PAYANT EST UNE PROPRIÉTÉ DU GREFFON, PAS D'UN CHEMIN D'URL.
    ///
    /// La tentation serait de garder la route du greffon là où l'hôte la monte,
    /// avec un `require_premium` écrit en dur. Ce serait un piège pour le
    /// suivant : le jour où un greffon PUBLIC arrive — et c'est prévu — il
    /// faudrait défaire ce câblage au lieu de simplement ne rien déclarer.
    ///
    /// Ici, un greffon gratuit n'implémente pas cette méthode et ses routes
    /// répondent à tout le monde. Un greffon payant nomme son module, et l'hôte
    /// refuse ses routes avec un **402** au corps identique à celui de
    /// `require_premium` — le client sait déjà le reconnaître comme un refus
    /// d'offre et non comme une panne (`estRefusPremium`, tune-web-client).
    ///
    /// Le refus porte sur les ROUTES, jamais sur le chargement : un greffon
    /// payant se charge quand même, sinon le gestionnaire ne pourrait pas
    /// l'annoncer à qui n'a pas encore Premium.
    fn required_feature(&self) -> Option<Feature> {
        None
    }

    /// The [`PLUGIN_PROTOCOL_VERSION`] this plugin was built against.
    ///
    /// Defaults to the version compiled into the SDK the plugin links, which
    /// is correct for in-tree plugins. Override only to pin an older
    /// generation deliberately.
    ///
    /// With plugins compiled in, the default can never disagree: there is
    /// exactly one `tune-core` in the dependency graph, so a plugin's constant
    /// *is* the server's. An out-of-tree plugin pinning a semver-incompatible
    /// `tune-core` would fail to compile against this loader rather than be
    /// refused at runtime. So today the gate only fires on a deliberate
    /// override — it is scaffolding for `libloading`, where two generations
    /// can genuinely coexist in one process.
    fn protocol_version(&self) -> (u32, u32) {
        PLUGIN_PROTOCOL_VERSION
    }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String>;
    async fn teardown(&mut self) -> Result<(), String>;

    /// Called when the event bus emits an event.
    /// Override to react to playback, library, or system events.
    async fn on_event(&mut self, _event: &TuneEvent) {}

    /// Read plugin-specific configuration from the context data_dir.
    fn read_config(&self, ctx: &PluginContext) -> serde_json::Value {
        let path = ctx.data_dir.join("config.json");
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Write plugin-specific configuration to the context data_dir.
    fn write_config(&self, ctx: &PluginContext, config: &serde_json::Value) -> Result<(), String> {
        let path = ctx.data_dir.join("config.json");
        let json = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
        std::fs::write(path, json).map_err(|e| e.to_string())
    }
}

pub struct PluginLoader {
    plugins: Arc<tokio::sync::Mutex<Vec<Box<dyn TunePlugin>>>>,
    data_root: PathBuf,
    event_bus: Option<EventBus>,
    db: Option<Arc<dyn DbBackend>>,
    license: Option<Arc<LicenseManager>>,
    event_dispatch_handle: Option<tokio::task::JoinHandle<()>>,
    /// Registrations accumulated across every plugin's `setup`, awaiting
    /// collection by the host.
    registrations: StdMutex<PluginRegistrations>,
    /// Compiled-in plugins `setup_all` skipped (opt-in-not-installed or
    /// disabled), kept so the plugin manager can still surface them.
    unloaded: StdMutex<Vec<AvailablePluginInfo>>,
}

impl PluginLoader {
    pub fn new(data_root: PathBuf) -> Self {
        Self {
            plugins: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            data_root,
            event_bus: None,
            db: None,
            license: None,
            event_dispatch_handle: None,
            registrations: StdMutex::new(PluginRegistrations::default()),
            unloaded: StdMutex::new(Vec::new()),
        }
    }

    pub fn with_event_bus(mut self, bus: EventBus) -> Self {
        self.event_bus = Some(bus);
        self
    }

    pub fn with_db(mut self, db: Arc<dyn DbBackend>) -> Self {
        self.db = Some(db);
        self
    }

    pub fn with_license(mut self, license: Arc<LicenseManager>) -> Self {
        self.license = Some(license);
        self
    }

    pub async fn register(&self, plugin: Box<dyn TunePlugin>) {
        self.plugins.lock().await.push(plugin);
    }

    pub async fn setup_all(&self, api_base_url: &str) -> Vec<String> {
        let mut loaded = Vec::new();
        let mut unloaded: Vec<AvailablePluginInfo> = Vec::new();
        std::fs::create_dir_all(&self.data_root).ok();

        let mut plugins = self.plugins.lock().await;
        for plugin in plugins.iter_mut() {
            let name = plugin.name().to_string();

            // Enable / install gate. A compiled-in plugin can be turned off
            // without recompiling (`plugin_{name}_enabled=false`, review #907).
            // An opt-in plugin (`default_enabled()==false`, e.g. DJ/Karaoke)
            // additionally stays dormant until the user installs it
            // (`plugin_{name}_installed=true`) — so it surfaces in the plugin
            // manager as an available entry rather than running by default
            // (#917 follow-up). Skipped plugins are captured for the manager.
            if let Some(db) = &self.db {
                let settings = SettingsRepo::with_backend(Arc::clone(db));
                let enabled = settings
                    .get(&format!("plugin_{name}_enabled"))
                    .ok()
                    .flatten();
                let installed = settings
                    .get(&format!("plugin_{name}_installed"))
                    .ok()
                    .flatten()
                    .as_deref()
                    == Some("true");
                let opt_in = !plugin.default_enabled();
                let dormant = enabled.as_deref() == Some("false") || (opt_in && !installed);
                if dormant {
                    info!(plugin_name = %name, opt_in, "plugin_dormant_not_loaded");
                    // Hors catalogue : le greffon reste compilé, testé et
                    // chargeable à la main, mais le gestionnaire ne le propose
                    // pas. Proposer d'installer une chose qu'aucun écran ne
                    // sait atteindre est un défaut en soi (#2090).
                    if plugin.catalogued() {
                        unloaded.push(AvailablePluginInfo {
                            name: name.clone(),
                            version: plugin.version().to_string(),
                            description: plugin.description().to_string(),
                            config_schema: plugin.config_schema(),
                            opt_in,
                            required_feature: plugin
                                .required_feature()
                                .map(|f| f.display_name().to_string()),
                        });
                    } else {
                        info!(plugin_name = %name, "plugin_hors_catalogue");
                    }
                    continue;
                }
            }

            // ABI gate. A plugin built against a different major generation
            // disagrees about the trait layout, so refuse it outright rather
            // than let it fault at the first dispatch.
            let (want_major, want_minor) = plugin.protocol_version();
            let (have_major, have_minor) = PLUGIN_PROTOCOL_VERSION;
            if want_major != have_major {
                warn!(
                    plugin_name = %name,
                    plugin_protocol = format!("{want_major}.{want_minor}"),
                    server_protocol = format!("{have_major}.{have_minor}"),
                    "plugin_protocol_incompatible"
                );
                continue;
            }
            if want_minor > have_minor {
                warn!(
                    plugin_name = %name,
                    plugin_protocol = format!("{want_major}.{want_minor}"),
                    server_protocol = format!("{have_major}.{have_minor}"),
                    "plugin_protocol_newer_than_server"
                );
                continue;
            }

            let data_dir = self.data_root.join(&name);
            std::fs::create_dir_all(&data_dir).ok();

            let mut ctx = PluginContext::new(api_base_url, data_dir).with_plugin_name(&name);
            if let Some(bus) = &self.event_bus {
                ctx = ctx.with_event_bus(bus.clone());
            }
            if let Some(db) = &self.db {
                ctx = ctx.with_db(Arc::clone(db));
            }
            if let Some(license) = &self.license {
                ctx = ctx.with_license(Arc::clone(license));
            }

            match plugin.setup(&ctx).await {
                Ok(()) => {
                    let reg = ctx.take_registrations();
                    #[cfg(feature = "plugin-http")]
                    let router_count = reg.routers.len();
                    #[cfg(not(feature = "plugin-http"))]
                    let router_count = 0usize;
                    info!(
                        plugin_name = %name,
                        version = %plugin.version(),
                        outputs = reg.outputs.len(),
                        routers = router_count,
                        zones = reg.zones.len(),
                        "plugin_loaded"
                    );
                    if let Ok(mut acc) = self.registrations.lock() {
                        acc.absorb(reg);
                    }
                    loaded.push(name);
                }
                Err(e) => {
                    // Deliberately not draining ctx here: a plugin that failed
                    // halfway may have registered an output backed by
                    // half-initialised state. Dropping it is the safe move.
                    warn!(plugin_name = %name, error = %e, "plugin_setup_failed");
                }
            }
        }

        // Drop refused/failed/disabled plugins from the resident set: they
        // would otherwise show up as loaded in /api/v1/plugins and keep
        // receiving every event via on_event on half-built state — the very
        // hazard setup registrations are dropped for (review #907).
        plugins.retain(|p| loaded.iter().any(|n| n == p.name()));

        if let Ok(mut slot) = self.unloaded.lock() {
            *slot = unloaded;
        }

        loaded
    }

    /// Compiled-in plugins `setup_all` skipped (opt-in-not-installed or
    /// disabled). The plugin manager lists these alongside the loaded ones so
    /// a dormant plugin stays installable/enable-able instead of vanishing.
    pub fn unloaded_plugins(&self) -> Vec<AvailablePluginInfo> {
        self.unloaded.lock().map(|v| v.clone()).unwrap_or_default()
    }

    /// Take everything the loaded plugins asked the host to install.
    ///
    /// Call once, after [`setup_all`](Self::setup_all). Returns an empty set
    /// on subsequent calls.
    pub fn take_registrations(&self) -> PluginRegistrations {
        self.registrations
            .lock()
            .map(|mut r| std::mem::take(&mut *r))
            .unwrap_or_default()
    }

    /// Le module Premium exigé par chaque greffon chargé, par nom.
    ///
    /// Les enregistrements ne portent que `(nom, routeur)` : le nom est tout ce
    /// que le greffon transmet, et c'est voulu — il ne choisit ni son préfixe
    /// d'URL ni son garde. L'hôte recolle ici l'exigence au routeur, juste
    /// avant de le monter.
    pub async fn required_features(&self) -> std::collections::HashMap<String, Feature> {
        self.plugins
            .lock()
            .await
            .iter()
            .filter_map(|p| p.required_feature().map(|f| (p.name().to_string(), f)))
            .collect()
    }

    /// Start dispatching EventBus events to all loaded plugins.
    ///
    /// Spawns a background task that subscribes to the event bus and forwards
    /// every event to each plugin's `on_event` callback.  Call this **after**
    /// `setup_all`.  The dispatch task runs until `teardown_all` is called.
    pub fn start_event_dispatch(&mut self) {
        let bus = match &self.event_bus {
            Some(b) => b.clone(),
            None => return,
        };

        let plugins = Arc::clone(&self.plugins);
        let mut rx = bus.subscribe();

        let handle = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let mut locked = plugins.lock().await;
                        for plugin in locked.iter_mut() {
                            plugin.on_event(&event).await;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "plugin_event_dispatch_lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        });

        self.event_dispatch_handle = Some(handle);
    }

    pub async fn teardown_all(&mut self) {
        // Stop the dispatch task first.
        if let Some(handle) = self.event_dispatch_handle.take() {
            handle.abort();
            let _ = handle.await;
        }

        let mut plugins = self.plugins.lock().await;
        for plugin in plugins.iter_mut().rev() {
            let name = plugin.name().to_string();
            if let Err(e) = plugin.teardown().await {
                warn!(plugin_name = %name, error = %e, "plugin_teardown_failed");
            }
        }
        plugins.clear();
    }

    pub async fn loaded_plugins(&self) -> Vec<PluginInfo> {
        self.plugins
            .lock()
            .await
            .iter()
            .map(|p| PluginInfo {
                name: p.name().to_string(),
                version: p.version().to_string(),
                description: p.description().to_string(),
                enabled: true,
                config_schema: p.config_schema(),
            })
            .collect()
    }

    pub async fn plugin_count(&self) -> usize {
        self.plugins.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct TestPlugin {
        setup_called: bool,
        teardown_called: bool,
    }

    impl TestPlugin {
        fn new() -> Self {
            Self {
                setup_called: false,
                teardown_called: false,
            }
        }
    }

    #[async_trait]
    impl TunePlugin for TestPlugin {
        fn name(&self) -> &str {
            "test-plugin"
        }
        fn version(&self) -> &str {
            "0.1.0"
        }
        fn description(&self) -> &str {
            "A test plugin"
        }
        async fn setup(&mut self, _ctx: &PluginContext) -> Result<(), String> {
            self.setup_called = true;
            Ok(())
        }
        async fn teardown(&mut self) -> Result<(), String> {
            self.teardown_called = true;
            Ok(())
        }
    }

    struct FailingPlugin;

    #[async_trait]
    impl TunePlugin for FailingPlugin {
        fn name(&self) -> &str {
            "failing"
        }
        fn version(&self) -> &str {
            "0.0.1"
        }
        fn description(&self) -> &str {
            "Always fails"
        }
        async fn setup(&mut self, _ctx: &PluginContext) -> Result<(), String> {
            Err("setup error".into())
        }
        async fn teardown(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    /// Plugin that records every event it receives.
    struct EventRecorderPlugin {
        events: Arc<tokio::sync::Mutex<Vec<String>>>,
    }

    impl EventRecorderPlugin {
        fn new(events: Arc<tokio::sync::Mutex<Vec<String>>>) -> Self {
            Self { events }
        }
    }

    #[async_trait]
    impl TunePlugin for EventRecorderPlugin {
        fn name(&self) -> &str {
            "event-recorder"
        }
        fn version(&self) -> &str {
            "0.1.0"
        }
        fn description(&self) -> &str {
            "Records events for testing"
        }
        async fn setup(&mut self, _ctx: &PluginContext) -> Result<(), String> {
            Ok(())
        }
        async fn teardown(&mut self) -> Result<(), String> {
            Ok(())
        }
        async fn on_event(&mut self, event: &TuneEvent) {
            self.events.lock().await.push(event.event_type.clone());
        }
    }

    #[tokio::test]
    async fn loader_setup_and_teardown() {
        let dir = tempfile::tempdir().unwrap();
        let mut loader = PluginLoader::new(dir.path().to_path_buf());
        loader.register(Box::new(TestPlugin::new())).await;

        let loaded = loader.setup_all("http://localhost:8888").await;
        assert_eq!(loaded, vec!["test-plugin"]);
        assert_eq!(loader.plugin_count().await, 1);

        let info = loader.loaded_plugins().await;
        assert_eq!(info[0].name, "test-plugin");
        assert_eq!(info[0].version, "0.1.0");

        loader.teardown_all().await;
        assert_eq!(loader.plugin_count().await, 0);
    }

    #[tokio::test]
    async fn failing_plugin_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let loader = PluginLoader::new(dir.path().to_path_buf());
        loader.register(Box::new(FailingPlugin)).await;
        loader.register(Box::new(TestPlugin::new())).await;

        let loaded = loader.setup_all("http://localhost:8888").await;
        assert_eq!(loaded, vec!["test-plugin"]);

        // The failed plugin must not linger: not reported as loaded, and no
        // longer resident to receive events on half-built state.
        let infos = loader.loaded_plugins().await;
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name, "test-plugin");
        assert_eq!(loader.plugin_count().await, 1);
    }

    /// Opt-in plugin: dormant until explicitly installed (like DJ/Karaoke).
    struct OptInPlugin;

    #[async_trait]
    impl TunePlugin for OptInPlugin {
        fn name(&self) -> &str {
            "opt-in"
        }
        fn version(&self) -> &str {
            "0.1.0"
        }
        fn description(&self) -> &str {
            "Dormant until installed"
        }
        fn default_enabled(&self) -> bool {
            false
        }
        async fn setup(&mut self, _ctx: &PluginContext) -> Result<(), String> {
            Ok(())
        }
        async fn teardown(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    fn memory_db() -> Arc<dyn DbBackend> {
        use crate::db::sqlite::SqliteDb;
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    #[tokio::test]
    async fn opt_in_plugin_dormant_until_installed() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let loader = PluginLoader::new(dir.path().to_path_buf()).with_db(Arc::clone(&db));
        loader.register(Box::new(OptInPlugin)).await;

        // Not installed → not loaded, but still surfaced as available/opt-in.
        let loaded = loader.setup_all("http://localhost:8888").await;
        assert!(loaded.is_empty(), "opt-in plugin must not load by default");
        assert!(loader.loaded_plugins().await.is_empty());
        let available = loader.unloaded_plugins();
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].name, "opt-in");
        assert!(
            available[0].opt_in,
            "must be flagged opt-in, not just disabled"
        );
    }

    #[tokio::test]
    async fn opt_in_plugin_loads_once_installed() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        SettingsRepo::with_backend(Arc::clone(&db))
            .set("plugin_opt-in_installed", "true")
            .unwrap();
        let loader = PluginLoader::new(dir.path().to_path_buf()).with_db(Arc::clone(&db));
        loader.register(Box::new(OptInPlugin)).await;

        let loaded = loader.setup_all("http://localhost:8888").await;
        assert_eq!(loaded, vec!["opt-in"]);
        assert!(loader.unloaded_plugins().is_empty());
    }

    /// Same as [`OptInPlugin`], but kept out of the catalogue (like DJ and
    /// Karaoke since #2090).
    struct UncataloguedPlugin;

    #[async_trait]
    impl TunePlugin for UncataloguedPlugin {
        fn name(&self) -> &str {
            "hors-catalogue"
        }
        fn version(&self) -> &str {
            "0.1.0"
        }
        fn description(&self) -> &str {
            "Compiled, but never offered"
        }
        fn default_enabled(&self) -> bool {
            false
        }
        fn catalogued(&self) -> bool {
            false
        }
        async fn setup(&mut self, _ctx: &PluginContext) -> Result<(), String> {
            Ok(())
        }
        async fn teardown(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    /// Mutation de `opt_in_plugin_dormant_until_installed` : deux greffons
    /// dormants pour la même raison, un seul catalogué. Le second doit
    /// disparaître du catalogue, et seulement lui — sinon `catalogued()` ne
    /// filtrerait rien, ou filtrerait tout.
    #[tokio::test]
    async fn uncatalogued_dormant_plugin_is_not_offered() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        let loader = PluginLoader::new(dir.path().to_path_buf()).with_db(Arc::clone(&db));
        loader.register(Box::new(OptInPlugin)).await;
        loader.register(Box::new(UncataloguedPlugin)).await;

        let loaded = loader.setup_all("http://localhost:8888").await;
        assert!(loaded.is_empty(), "les deux sont opt-in et non installés");

        let available: Vec<String> = loader
            .unloaded_plugins()
            .iter()
            .map(|p| p.name.clone())
            .collect();
        assert_eq!(
            available,
            vec!["opt-in".to_string()],
            "seul le greffon catalogué doit être proposé (proposés : {available:?})"
        );
    }

    /// Hors catalogue ≠ hors service : poser `plugin_{name}_installed` à la
    /// main le charge quand même. Le greffon cesse d'être promis, il ne cesse
    /// pas d'exister — c'est ce qui distingue le retrait du catalogue de la
    /// suppression pure et simple.
    #[tokio::test]
    async fn uncatalogued_plugin_still_loads_when_installed_by_hand() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        SettingsRepo::with_backend(Arc::clone(&db))
            .set("plugin_hors-catalogue_installed", "true")
            .unwrap();
        let loader = PluginLoader::new(dir.path().to_path_buf()).with_db(Arc::clone(&db));
        loader.register(Box::new(UncataloguedPlugin)).await;

        let loaded = loader.setup_all("http://localhost:8888").await;
        assert_eq!(loaded, vec!["hors-catalogue"]);
        assert!(loader.unloaded_plugins().is_empty());
    }

    #[tokio::test]
    async fn default_on_plugin_disabled_is_available_not_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        let db = memory_db();
        SettingsRepo::with_backend(Arc::clone(&db))
            .set("plugin_test-plugin_enabled", "false")
            .unwrap();
        let loader = PluginLoader::new(dir.path().to_path_buf()).with_db(Arc::clone(&db));
        loader.register(Box::new(TestPlugin::new())).await;

        let loaded = loader.setup_all("http://localhost:8888").await;
        assert!(
            loaded.is_empty(),
            "explicitly disabled plugin must not load"
        );
        let available = loader.unloaded_plugins();
        assert_eq!(available.len(), 1);
        assert_eq!(available[0].name, "test-plugin");
        assert!(
            !available[0].opt_in,
            "a disabled default-on plugin is not opt-in"
        );
    }

    #[test]
    fn plugin_context_basic() {
        let ctx = PluginContext::new("http://localhost", PathBuf::from("/tmp/test"));
        assert_eq!(ctx.api_base_url, "http://localhost");
        assert!(ctx.event_bus.is_none());
    }

    #[tokio::test]
    async fn empty_loader() {
        let loader = PluginLoader::new(PathBuf::from("/tmp"));
        assert_eq!(loader.plugin_count().await, 0);
        assert!(loader.loaded_plugins().await.is_empty());
    }

    #[test]
    fn plugin_context_emit_event() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let ctx = PluginContext::new("http://localhost", PathBuf::from("/tmp")).with_event_bus(bus);

        ctx.emit_event("test.event", json!({"key": "value"}));

        let event = rx.try_recv().unwrap();
        assert_eq!(event.event_type, "test.event");
        assert_eq!(event.data["key"], "value");
    }

    #[test]
    fn plugin_context_config_with_db() {
        use crate::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);

        let ctx = PluginContext::new("http://localhost", PathBuf::from("/tmp"))
            .with_plugin_name("myplugin")
            .with_db(Arc::clone(&backend));

        assert!(ctx.get_config("volume").is_none());

        ctx.set_config("volume", "80").unwrap();
        assert_eq!(ctx.get_config("volume").unwrap(), "80");

        // Verify key is namespaced in the DB.
        let repo = SettingsRepo::with_backend(backend);
        assert_eq!(repo.get("plugin_myplugin_volume").unwrap().unwrap(), "80");
    }

    /// Plugin that exercises the whole registration surface.
    struct RegisteringPlugin {
        protocol: (u32, u32),
    }

    #[async_trait]
    impl TunePlugin for RegisteringPlugin {
        fn name(&self) -> &str {
            "registering"
        }
        fn version(&self) -> &str {
            "0.1.0"
        }
        fn description(&self) -> &str {
            "Registers an output, a router and a zone"
        }
        fn protocol_version(&self) -> (u32, u32) {
            self.protocol
        }
        async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
            ctx.register_output(Box::new(crate::outputs::mock::MockOutput::new(
                "plug:1", "Plugged",
            )));
            #[cfg(feature = "plugin-http")]
            ctx.register_router(axum::Router::new());
            ctx.register_zone("Plugged", "mock", "plug:1");
            Ok(())
        }
        async fn teardown(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    /// Registers an output and *then* fails — the host must not install it.
    struct FailsAfterRegistering;

    #[async_trait]
    impl TunePlugin for FailsAfterRegistering {
        fn name(&self) -> &str {
            "half-built"
        }
        fn version(&self) -> &str {
            "0.1.0"
        }
        fn description(&self) -> &str {
            "Registers then fails"
        }
        async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
            ctx.register_output(Box::new(crate::outputs::mock::MockOutput::new(
                "ghost:1", "Ghost",
            )));
            Err("blew up after registering".into())
        }
        async fn teardown(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn setup_collects_registrations() {
        let dir = tempfile::tempdir().unwrap();
        let loader = PluginLoader::new(dir.path().to_path_buf());
        loader
            .register(Box::new(RegisteringPlugin {
                protocol: PLUGIN_PROTOCOL_VERSION,
            }))
            .await;

        let loaded = loader.setup_all("http://localhost:8888").await;
        assert_eq!(loaded, vec!["registering"]);

        let reg = loader.take_registrations();
        assert_eq!(reg.outputs.len(), 1);
        assert_eq!(reg.outputs[0].device_id(), "plug:1");
        #[cfg(feature = "plugin-http")]
        {
            assert_eq!(reg.routers.len(), 1);
            // The name is stamped by the context, not chosen by the plugin.
            assert_eq!(reg.routers[0].0, "registering");
        }
        assert_eq!(reg.zones.len(), 1);
        assert_eq!(reg.zones[0].device_id, "plug:1");

        // Draining is one-shot.
        assert!(loader.take_registrations().is_empty());
    }

    #[tokio::test]
    async fn failed_setup_discards_its_registrations() {
        let dir = tempfile::tempdir().unwrap();
        let loader = PluginLoader::new(dir.path().to_path_buf());
        loader.register(Box::new(FailsAfterRegistering)).await;

        let loaded = loader.setup_all("http://localhost:8888").await;
        assert!(loaded.is_empty());
        // The output it managed to register before failing must not reach the
        // host — otherwise a broken plugin leaves a zombie device selectable
        // in the UI.
        assert!(loader.take_registrations().is_empty());
    }

    #[tokio::test]
    async fn incompatible_protocol_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let loader = PluginLoader::new(dir.path().to_path_buf());
        let (major, minor) = PLUGIN_PROTOCOL_VERSION;

        // Different major: refused.
        loader
            .register(Box::new(RegisteringPlugin {
                protocol: (major + 1, 0),
            }))
            .await;
        assert!(loader.setup_all("http://localhost:8888").await.is_empty());
        assert!(loader.take_registrations().is_empty());

        // Newer minor than the server implements: also refused, since the
        // plugin may call a hook this server does not have.
        let loader2 = PluginLoader::new(dir.path().to_path_buf());
        loader2
            .register(Box::new(RegisteringPlugin {
                protocol: (major, minor + 1),
            }))
            .await;
        assert!(loader2.setup_all("http://localhost:8888").await.is_empty());

        // Older minor: accepted.
        let loader3 = PluginLoader::new(dir.path().to_path_buf());
        loader3
            .register(Box::new(RegisteringPlugin {
                protocol: (major, minor),
            }))
            .await;
        assert_eq!(
            loader3.setup_all("http://localhost:8888").await,
            vec!["registering"]
        );
    }

    #[tokio::test]
    async fn event_dispatch_forwards_to_plugins() {
        let bus = EventBus::new();
        let events = Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));

        let dir = tempfile::tempdir().unwrap();
        let mut loader = PluginLoader::new(dir.path().to_path_buf()).with_event_bus(bus.clone());

        loader
            .register(Box::new(EventRecorderPlugin::new(Arc::clone(&events))))
            .await;
        loader.setup_all("http://localhost:8888").await;
        loader.start_event_dispatch();

        // Emit an event and give the dispatch task time to process it.
        bus.emit("playback.started", json!({}));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let recorded = events.lock().await;
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0], "playback.started");
    }
}
