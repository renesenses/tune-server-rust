use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
// L'horloge de tokio, et non celle de `std` : hors test elles ne diffèrent
// pas, mais sous `start_paused` seule la première voit passer les 30 s d'un
// `setup()` coupé — la durée publiée par le gestionnaire doit les compter.
use tokio::time::Instant;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{info, warn};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::event_bus::{EventBus, TuneEvent};
use crate::license::{Feature, LicenseManager};
use crate::outputs::traits::{OutputProvider, OutputTarget};

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
    /// Providers that DISCOVER outputs, as opposed to `outputs`, which are
    /// fixed instances known at `setup` time. The host hands these to
    /// `spawn_output_providers`, which polls `discover()` for as long as the
    /// server runs.
    pub output_providers: Vec<Arc<dyn OutputProvider>>,
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
        self.outputs.is_empty() && self.output_providers.is_empty() && self.zones.is_empty()
    }

    fn absorb(&mut self, other: PluginRegistrations) {
        self.outputs.extend(other.outputs);
        self.output_providers.extend(other.output_providers);
        #[cfg(feature = "plugin-http")]
        self.routers.extend(other.routers);
        self.zones.extend(other.zones);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    pub name: String,
    /// Le nom à montrer — voir [`TunePlugin::display_name`]. Vide dans une
    /// fiche sérialisée avant son ajout : l'hôte retombe alors sur `name`.
    #[serde(default)]
    pub display_name: String,
    pub version: String,
    pub description: String,
    pub enabled: bool,
    pub config_schema: serde_json::Value,
    /// Le module Premium exigé, par son `display_name` — voir
    /// [`TunePlugin::required_feature`]. Absent pour un greffon libre.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_feature: Option<String>,
}

/// A compiled-in plugin that `setup_all` did not load — either an opt-in
/// plugin the user has not installed yet, or a default-on one they disabled.
///
/// Captured before the resident set is pruned so the plugin manager can still
/// list it (and offer "Install" / "Enable") instead of it vanishing entirely.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailablePluginInfo {
    pub name: String,
    /// Le nom à montrer — voir [`TunePlugin::display_name`].
    #[serde(default)]
    pub display_name: String,
    pub version: String,
    pub description: String,
    pub config_schema: serde_json::Value,
    /// `true` = dormant because it is opt-in and not yet installed (offer
    /// "Install"); `false` = a default-on plugin the user disabled (offer
    /// "Enable").
    pub opt_in: bool,
    /// Le module Premium exigé, par son `display_name` (« Concerts »).
    ///
    /// Sans ce champ, le gestionnaire proposerait « Installer » à quelqu'un
    /// dont les routes seront refusées juste après : il installe, redémarre,
    /// et n'obtient qu'un 402. Le porter ici permet d'afficher le cadenas
    /// AVANT le clic (tune-web-client, `stores/concerts.ts`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    /// La licence du serveur, pour qu'un greffon payant ADAPTE sa réponse.
    ///
    /// ⚠️ POURQUOI ICI, ET PAS UN REFUS MONTÉ PAR L'HÔTE DEVANT LES ROUTES.
    /// Un garde de l'hôte ne sait qu'ouvrir ou fermer. Or « Concerts » doit
    /// un jour servir une version RÉDUITE aux comptes gratuits : la décision
    /// « complet / réduit / refusé » doit donc tenir chez le greffon, en une
    /// seule fonction.
    ///
    /// `None` chez un hôte qui n'en fournit pas (tests, `tune-cli`) : le
    /// greffon se comporte alors comme SANS Premium, jamais l'inverse — voir
    /// [`PluginContext::feature_licensed`].
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

    /// Ce module Premium est-il ouvert sur ce serveur ?
    ///
    /// `false` quand l'hôte ne fournit pas de licence : une absence ne vaut
    /// jamais une autorisation.
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

    /// Expose a provider that DISCOVERS outputs, instead of a fixed instance.
    ///
    /// [`register_output`](Self::register_output) covers the case where the
    /// plugin already knows its devices at `setup` time. A network protocol
    /// does not: Diretta targets appear and disappear while the server runs,
    /// and the count is unknown until something answers on the wire. Calling
    /// `register_output` N times cannot express that — the host would freeze
    /// the device list at startup.
    ///
    /// The host hands providers to `spawn_output_providers`, which polls
    /// `discover()` for as long as the server runs and gives every discovered
    /// device the same zone lifecycle as built-in discovery: reconnect,
    /// auto-create, hidden zones.
    ///
    /// A paid provider should declare
    /// [`required_module`](OutputProvider::required_module). Returning an
    /// empty list when the module is not owned is correct but silent, and
    /// indistinguishable from a provider that is absent, mis-compiled, or on a
    /// network that does not answer — a beta tester reinstalled his whole
    /// system over exactly that ambiguity (#2392).
    ///
    /// `Arc`, not `Box`: the host keeps polling the provider from a background
    /// task, so it needs shared ownership rather than a value it consumes.
    pub fn register_output_provider(&self, provider: Arc<dyn OutputProvider>) {
        match self.registrations.lock() {
            Ok(mut reg) => reg.output_providers.push(provider),
            Err(_) => self.warn_registration_lost("output provider"),
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
    /// Le nom à MONTRER dans le gestionnaire des extensions (`display_name`
    /// de `GET /api/v1/plugins`), quand l'identifiant technique ne se lit pas
    /// (« entree-audio » → « Entrée audio », #5296).
    ///
    /// Par défaut, l'identifiant lui-même : rien ne change pour un greffon qui
    /// ne le surcharge pas. Le client web traduit les noms qu'il connaît ;
    /// celui-ci est le repli des clients qui ne les connaissent pas.
    fn display_name(&self) -> &str {
        self.name()
    }
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

    /// Le module Premium auquel ce greffon appartient, s'il en a un.
    ///
    /// ⚠️ LE PAYANT EST UNE PROPRIÉTÉ DU GREFFON, PAS D'UN CHEMIN D'URL. Un
    /// greffon libre ne surcharge pas cette méthode ; un greffon payant nomme
    /// son module, et le gestionnaire affiche le cadenas avant le clic
    /// ([`AvailablePluginInfo::required_feature`]).
    ///
    /// Cette déclaration ne FERME rien à elle seule, et l'hôte ne monte aucun
    /// garde devant les routes : le greffon décide lui-même, par la licence
    /// que lui remet [`PluginContext::license`], ce qu'il sert à qui. C'est ce
    /// qui lui permettra de servir un jour une version réduite aux comptes
    /// gratuits au lieu d'une porte close.
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

/// Durée maximale du `setup()` d'UN greffon au démarrage (#5403).
///
/// Sans borne, un greffon qui attend le réseau (un appareil éteint, un service
/// qui ne répond pas) tenait l'étape « greffons » du démarrage indéfiniment, et
/// avec elle tous les greffons suivants. Au-delà de cette durée, le greffon est
/// journalisé (`plugin_setup_timed_out`, avec sa durée) et n'est pas chargé ;
/// le démarrage continue. Il n'est pas oublié pour autant : il reste visible
/// dans le gestionnaire, en erreur « démarrage trop long »
/// ([`PluginSetupErrorReason::SetupTimeout`]), et
/// [`PluginLoader::retry_setup`] relance son `setup()` sous la même borne.
/// Trente secondes : la même
/// patience que la sonde de chargement d'un greffon WASM
/// (`tune-server/src/plugins_host.rs`), et des milliers de fois ce que coûte
/// un `setup()` sain, qui ne fait qu'enregistrer des sorties, des routes et des
/// zones.
///
/// Limite : la borne ne coupe qu'un `setup()` qui rend la main à l'exécuteur
/// (une attente `async`). Un `setup()` qui bloque le fil lui-même (appel
/// bloquant, `std::thread::sleep`) n'est pas interruptible de l'extérieur.
pub const PLUGIN_SETUP_TIMEOUT: Duration = Duration::from_secs(30);

/// Au-delà de cette durée, le chargement d'UN greffon est signalé par
/// `plugin_setup_slow` (#5370).
///
/// Un `setup()` sain ne fait qu'enregistrer des sorties, des routes et des
/// zones, et lire deux réglages : il se compte en millisecondes. Cinq secondes,
/// c'est plus de mille fois ce budget — assez large pour qu'un greffon qui
/// interroge un appareil du réseau local, sur un réseau lent ou une base
/// occupée par le scan, ne crie pas au loup — et assez court pour que la page
/// d'attente, qui se rafraîchit toutes les 3 s, ait déjà montré deux fois le
/// même greffon : c'est le moment où le testeur commence à se demander si Tune
/// est bloqué, et donc celui où le journal doit pouvoir lui répondre.
pub const PLUGIN_SETUP_SLOW_THRESHOLD: Duration = Duration::from_secs(5);

/// Pourquoi un greffon compilé est resté en erreur (#5403).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginSetupErrorReason {
    /// `setup()` a dépassé [`PLUGIN_SETUP_TIMEOUT`] : « démarrage trop long ».
    SetupTimeout,
    /// `setup()` a rendu une erreur, au démarrage ou lors d'un nouvel essai
    /// ([`PluginLoader::retry_setup`]). Le message du greffon suit, passé par
    /// [`public_setup_message`].
    SetupFailed,
}

impl PluginSetupErrorReason {
    /// Le code rendu par l'API du gestionnaire (`error_reason`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SetupTimeout => "setup_timeout",
            Self::SetupFailed => "setup_failed",
        }
    }
}

/// Un greffon compilé dont le `setup()` n'a pas abouti, et qu'on peut
/// réessayer (#5403).
///
/// Avant, un greffon coupé à [`PLUGIN_SETUP_TIMEOUT`] disparaissait du
/// gestionnaire exactement comme un greffon en échec : rien ne disait qu'il
/// existait, ni pourquoi il ne tournait pas.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSetupError {
    pub name: String,
    /// Le nom à montrer — voir [`TunePlugin::display_name`].
    #[serde(default)]
    pub display_name: String,
    pub version: String,
    pub description: String,
    pub config_schema: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_feature: Option<String>,
    pub reason: PluginSetupErrorReason,
    /// Durée du dernier essai, mesurée comme `plugin_loaded` (#5370).
    pub duration_ms: u64,
    /// La borne appliquée à cet essai.
    pub timeout_ms: u64,
    /// Le message d'un `setup()` en échec, tronqué et expurgé
    /// ([`public_setup_message`]) ; absent pour une coupure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// L'état des `setup()` que le gestionnaire doit pouvoir lire à tout moment.
///
/// Partagé par [`PluginLoader::setup_report`] : l'hôte le lit SANS prendre le
/// verrou du chargeur, qu'un nouvel essai tient jusqu'à la borne.
#[derive(Debug, Clone, Default)]
pub struct PluginSetupReport {
    /// Greffons en erreur, dans l'ordre d'enregistrement.
    pub errors: Vec<PluginSetupError>,
    /// Greffons chargés après coup par un nouvel essai réussi : ils
    /// n'étaient pas dans l'instantané publié au démarrage.
    pub loaded_after_retry: Vec<PluginInfo>,
}

/// Ce que rend [`PluginLoader::retry_setup`].
pub enum PluginRetryOutcome {
    /// Aucun greffon de ce nom n'est en erreur.
    NotInError,
    /// Un essai est déjà en cours pour ce greffon.
    InProgress,
    /// Chargé. Ce qu'il a enregistré revient à l'hôte, qui l'installe.
    Loaded {
        duration_ms: u64,
        registrations: PluginRegistrations,
    },
    /// Toujours en erreur, avec le motif et la durée de ce nouvel essai.
    StillInError(PluginSetupError),
}

/// L'issue d'UN `setup()`, borné et mesuré.
enum SetupRun {
    Loaded(PluginRegistrations, u64),
    Failed(String, u64),
    TimedOut(u64),
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
    /// Borne du `setup()` de chaque greffon ; [`PLUGIN_SETUP_TIMEOUT`] sauf en test.
    setup_timeout: Duration,
    /// Seuil de `plugin_setup_slow` ; [`PLUGIN_SETUP_SLOW_THRESHOLD`] sauf en test.
    slow_setup_threshold: Duration,
    /// Greffons coupés à la borne, gardés pour un nouvel essai (#5403). Hors
    /// du jeu résident : ils ne reçoivent aucun événement.
    parked: StdMutex<Vec<Box<dyn TunePlugin>>>,
    /// Voir [`PluginSetupReport`].
    setup_report: Arc<StdMutex<PluginSetupReport>>,
    /// L'adresse passée à `setup_all`, reprise par un nouvel essai.
    api_base_url: StdMutex<String>,
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
            setup_timeout: PLUGIN_SETUP_TIMEOUT,
            slow_setup_threshold: PLUGIN_SETUP_SLOW_THRESHOLD,
            parked: StdMutex::new(Vec::new()),
            setup_report: Arc::new(StdMutex::new(PluginSetupReport::default())),
            api_base_url: StdMutex::new(String::new()),
        }
    }

    /// Change la borne du `setup()` de chaque greffon (#5403).
    pub fn with_setup_timeout(mut self, timeout: Duration) -> Self {
        self.set_setup_timeout(timeout);
        self
    }

    /// [`with_setup_timeout`](Self::with_setup_timeout) sur un chargeur déjà
    /// rangé dans l'état de l'hôte (les tests du serveur ne dorment pas 30 s).
    pub fn set_setup_timeout(&mut self, timeout: Duration) {
        self.setup_timeout = timeout;
    }

    /// Change le seuil de `plugin_setup_slow` (les tests ne dorment pas 5 s).
    pub fn with_slow_setup_threshold(mut self, threshold: Duration) -> Self {
        self.slow_setup_threshold = threshold;
        self
    }

    pub fn with_event_bus(mut self, bus: EventBus) -> Self {
        self.event_bus = Some(bus);
        self
    }

    pub fn with_db(mut self, db: Arc<dyn DbBackend>) -> Self {
        self.db = Some(db);
        self
    }

    /// La licence remise à chaque greffon par [`PluginContext::license`].
    pub fn with_license(mut self, license: Arc<LicenseManager>) -> Self {
        self.license = Some(license);
        self
    }

    pub async fn register(&self, plugin: Box<dyn TunePlugin>) {
        self.plugins.lock().await.push(plugin);
    }

    pub async fn setup_all(&self, api_base_url: &str) -> Vec<String> {
        self.setup_all_observed(api_base_url, &|_| {}).await
    }

    /// [`setup_all`](Self::setup_all), en annonçant chaque greffon à
    /// `on_plugin` AVANT de le charger.
    ///
    /// #5370 — la page d'attente du démarrage ne disait que « greffons », sans
    /// dire lequel ; l'hôte s'en sert pour nommer le greffon en cours. Chaque
    /// greffon laisse en outre une ligne de journal portant sa durée
    /// (`duration_ms`), et un avertissement `plugin_setup_slow` au-delà de
    /// [`PLUGIN_SETUP_SLOW_THRESHOLD`] : c'est ce qui manquait pour savoir
    /// lequel avait pris le temps.
    pub async fn setup_all_observed(
        &self,
        api_base_url: &str,
        on_plugin: &(dyn Fn(&str) + Send + Sync),
    ) -> Vec<String> {
        let mut loaded = Vec::new();
        let mut unloaded: Vec<AvailablePluginInfo> = Vec::new();
        let mut errors: Vec<PluginSetupError> = Vec::new();
        std::fs::create_dir_all(&self.data_root).ok();
        *self.api_base_url.lock().unwrap_or_else(|e| e.into_inner()) = api_base_url.to_string();

        let mut plugins = self.plugins.lock().await;
        for plugin in plugins.iter_mut() {
            let name = plugin.name().to_string();
            on_plugin(&name);
            // Mesuré DEPUIS la lecture des réglages : si la base est occupée
            // (scan en cours sur une grande bibliothèque), c'est là que le
            // temps passe, et il doit se voir dans la durée du greffon.
            let started = Instant::now();

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
                    let duration_ms = self.measure_setup(&name, started, "dormant");
                    info!(plugin_name = %name, opt_in, duration_ms, "plugin_dormant_not_loaded");
                    // Hors catalogue : le greffon reste compilé, testé et
                    // chargeable à la main, mais le gestionnaire ne le propose
                    // pas. Proposer d'installer une chose qu'aucun écran ne
                    // sait atteindre est un défaut en soi (#2090).
                    if plugin.catalogued() {
                        unloaded.push(AvailablePluginInfo {
                            name: name.clone(),
                            display_name: plugin.display_name().to_string(),
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

            let ctx = self.plugin_context(api_base_url, &name);

            // #5403 et #5370 ensemble : chaque `setup()` est borné ET mesuré.
            // La durée part dans le journal quel que soit le sort du greffon,
            // la lenteur est signalée au-delà du seuil, et une coupure est
            // journalisée avec sa durée — voir `run_setup`.
            match self.run_setup(plugin, &ctx, &name, started).await {
                SetupRun::Loaded(reg, _duration_ms) => {
                    if let Ok(mut acc) = self.registrations.lock() {
                        acc.absorb(reg);
                    }
                    loaded.push(name);
                }
                // #5403 (décision du 29/09) — un échec reste visible lui aussi,
                // avec le message du greffon, et se réessaie.
                SetupRun::Failed(message, duration_ms) => {
                    errors.push(self.setup_error(
                        &**plugin,
                        PluginSetupErrorReason::SetupFailed,
                        duration_ms,
                        Some(message),
                    ));
                }
                SetupRun::TimedOut(duration_ms) => {
                    errors.push(self.setup_error(
                        &**plugin,
                        PluginSetupErrorReason::SetupTimeout,
                        duration_ms,
                        None,
                    ));
                }
            }
        }

        // Drop refused/failed/disabled plugins from the resident set: they
        // would otherwise show up as loaded in /api/v1/plugins and keep
        // receiving every event via on_event on half-built state — the very
        // hazard setup registrations are dropped for (review #907).
        //
        // #5403 — un greffon COUPÉ à la borne, ou dont le `setup()` a échoué,
        // quitte le jeu résident (même danger), mais il n'est pas détruit : il
        // attend un nouvel essai.
        let mut parked = Vec::new();
        for plugin in std::mem::take(&mut *plugins) {
            if loaded.iter().any(|n| n == plugin.name()) {
                plugins.push(plugin);
            } else if errors.iter().any(|e| e.name == plugin.name()) {
                parked.push(plugin);
            }
        }
        *self.parked.lock().unwrap_or_else(|e| e.into_inner()) = parked;
        *self.setup_report.lock().unwrap_or_else(|e| e.into_inner()) = PluginSetupReport {
            errors,
            loaded_after_retry: Vec::new(),
        };

        if let Ok(mut slot) = self.unloaded.lock() {
            *slot = unloaded;
        }

        loaded
    }

    /// La durée d'un greffon depuis `started`, en millisecondes, avec
    /// l'avertissement `plugin_setup_slow` au-delà du seuil (#5370).
    fn measure_setup(&self, name: &str, started: Instant, outcome: &str) -> u64 {
        let elapsed = started.elapsed();
        let slow_threshold = self.slow_setup_threshold;
        if elapsed > slow_threshold {
            warn!(
                plugin_name = %name,
                duration_ms = elapsed.as_millis() as u64,
                threshold_ms = slow_threshold.as_millis() as u64,
                outcome,
                "plugin_setup_slow"
            );
        }
        elapsed.as_millis() as u64
    }

    /// Le contexte remis au `setup()` du greffon `name`.
    fn plugin_context(&self, api_base_url: &str, name: &str) -> PluginContext {
        let data_dir = self.data_root.join(name);
        std::fs::create_dir_all(&data_dir).ok();

        let mut ctx = PluginContext::new(api_base_url, data_dir).with_plugin_name(name);
        if let Some(bus) = &self.event_bus {
            ctx = ctx.with_event_bus(bus.clone());
        }
        if let Some(db) = &self.db {
            ctx = ctx.with_db(Arc::clone(db));
        }
        if let Some(license) = &self.license {
            ctx = ctx.with_license(Arc::clone(license));
        }
        ctx
    }

    /// UN `setup()`, borné par [`PLUGIN_SETUP_TIMEOUT`] (#5403) et mesuré
    /// depuis `started` (#5370). Sert au démarrage et au nouvel essai : les
    /// deux chemins écrivent les mêmes lignes de journal.
    async fn run_setup(
        &self,
        plugin: &mut Box<dyn TunePlugin>,
        ctx: &PluginContext,
        name: &str,
        started: Instant,
    ) -> SetupRun {
        match tokio::time::timeout(self.setup_timeout, plugin.setup(ctx)).await {
            Err(_elapsed) => {
                // Même précaution qu'un échec : ctx n'est pas vidé, ce que le
                // greffon a enregistré à moitié est abandonné.
                let duration_ms = self.measure_setup(name, started, "timed_out");
                warn!(
                    plugin_name = %name,
                    duration_ms,
                    timeout_ms = self.setup_timeout.as_millis() as u64,
                    "plugin_setup_timed_out"
                );
                SetupRun::TimedOut(duration_ms)
            }
            Ok(Ok(())) => {
                let duration_ms = self.measure_setup(name, started, "loaded");
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
                    duration_ms,
                    "plugin_loaded"
                );
                SetupRun::Loaded(reg, duration_ms)
            }
            Ok(Err(e)) => {
                // Deliberately not draining ctx here: a plugin that failed
                // halfway may have registered an output backed by
                // half-initialised state. Dropping it is the safe move.
                let duration_ms = self.measure_setup(name, started, "failed");
                warn!(plugin_name = %name, error = %e, duration_ms, "plugin_setup_failed");
                SetupRun::Failed(e, duration_ms)
            }
        }
    }

    fn setup_error(
        &self,
        plugin: &dyn TunePlugin,
        reason: PluginSetupErrorReason,
        duration_ms: u64,
        message: Option<String>,
    ) -> PluginSetupError {
        PluginSetupError {
            name: plugin.name().to_string(),
            display_name: plugin.display_name().to_string(),
            version: plugin.version().to_string(),
            description: plugin.description().to_string(),
            config_schema: plugin.config_schema(),
            required_feature: plugin
                .required_feature()
                .map(|f| f.display_name().to_string()),
            reason,
            duration_ms,
            timeout_ms: self.setup_timeout.as_millis() as u64,
            message: message.map(|m| public_setup_message(&m)),
        }
    }

    /// L'état des `setup()` en erreur, partagé avec l'hôte (#5403).
    ///
    /// Le même `Arc` pour toute la vie du chargeur : l'hôte peut le prendre
    /// une fois, à la construction, et le lire sans jamais verrouiller le
    /// chargeur.
    pub fn setup_report(&self) -> Arc<StdMutex<PluginSetupReport>> {
        Arc::clone(&self.setup_report)
    }

    /// Relance le `setup()` d'un greffon en erreur, sous la même borne
    /// ([`PLUGIN_SETUP_TIMEOUT`]) et avec la même mesure que le démarrage
    /// (#5403).
    ///
    /// Réussi, le greffon rejoint le jeu résident (il reçoit désormais les
    /// événements) et ce qu'il a enregistré revient à l'appelant, qui
    /// l'installe. Sinon il reste en erreur, avec le motif et la durée de ce
    /// nouvel essai.
    ///
    /// Le greffon quitte la réserve pendant l'essai : un second appel
    /// simultané rend [`PluginRetryOutcome::InProgress`]. Un appelant qui
    /// abandonnerait le futur en cours de route perdrait donc le greffon
    /// jusqu'au prochain démarrage — l'hôte le fait tourner dans une tâche à
    /// part pour cette raison.
    pub async fn retry_setup(&self, name: &str) -> PluginRetryOutcome {
        let mut plugin = {
            let mut parked = self.parked.lock().unwrap_or_else(|e| e.into_inner());
            match parked.iter().position(|p| p.name() == name) {
                Some(i) => parked.remove(i),
                None => {
                    let report = self.setup_report.lock().unwrap_or_else(|e| e.into_inner());
                    return if report.errors.iter().any(|e| e.name == name) {
                        PluginRetryOutcome::InProgress
                    } else {
                        PluginRetryOutcome::NotInError
                    };
                }
            }
        };

        info!(plugin_name = %name, "plugin_setup_retry");
        let api_base_url = self
            .api_base_url
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let ctx = self.plugin_context(&api_base_url, name);
        let started = Instant::now();
        let run = self.run_setup(&mut plugin, &ctx, name, started).await;

        let (reason, duration_ms, message) = match run {
            SetupRun::Loaded(registrations, duration_ms) => {
                let info = plugin_info_of(&*plugin);
                self.plugins.lock().await.push(plugin);
                let mut report = self.setup_report.lock().unwrap_or_else(|e| e.into_inner());
                report.errors.retain(|e| e.name != name);
                report.loaded_after_retry.push(info);
                return PluginRetryOutcome::Loaded {
                    duration_ms,
                    registrations,
                };
            }
            SetupRun::Failed(e, duration_ms) => {
                (PluginSetupErrorReason::SetupFailed, duration_ms, Some(e))
            }
            SetupRun::TimedOut(duration_ms) => {
                (PluginSetupErrorReason::SetupTimeout, duration_ms, None)
            }
        };

        let error = self.setup_error(&*plugin, reason, duration_ms, message);
        self.parked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(plugin);
        let mut report = self.setup_report.lock().unwrap_or_else(|e| e.into_inner());
        match report.errors.iter_mut().find(|e| e.name == name) {
            Some(slot) => *slot = error.clone(),
            None => report.errors.push(error.clone()),
        }
        PluginRetryOutcome::StillInError(error)
    }

    /// Démarre la distribution des événements si elle ne tourne pas encore :
    /// l'hôte ne la lance au démarrage que si un greffon a chargé, et un
    /// nouvel essai réussi peut être le premier (#5403).
    pub fn ensure_event_dispatch(&mut self) {
        if self.event_dispatch_handle.is_none() {
            self.start_event_dispatch();
        }
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
            .map(|p| plugin_info_of(&**p))
            .collect()
    }

    pub async fn plugin_count(&self) -> usize {
        self.plugins.lock().await.len()
    }

    /// Every plugin name this build carries, as registered.
    ///
    /// Call **before** [`setup_all`](Self::setup_all): that method retains only
    /// what loaded, so afterwards this list is the loaded set instead of the
    /// compiled-in one.
    ///
    /// Neither [`loaded_plugins`](Self::loaded_plugins) nor
    /// [`unloaded_plugins`](Self::unloaded_plugins) can answer "does this
    /// server carry a plugin by that name?". The first drops the dormant ones,
    /// and the second drops the uncatalogued ones on top
    /// ([`TunePlugin::catalogued`]) — DJ and Karaoke are in neither, yet both
    /// still load when `plugin_{name}_installed` is set (#2090). A caller that
    /// has to decide whether a name means anything here needs the whole set.
    pub async fn registered_names(&self) -> Vec<String> {
        self.plugins
            .lock()
            .await
            .iter()
            .map(|p| p.name().to_string())
            .collect()
    }
}

/// Longueur maximale, en caractères, du message d'échec publié (#5403).
pub const PLUGIN_SETUP_MESSAGE_MAX_CHARS: usize = 200;

/// Le message d'un `setup()` en échec, tel que le gestionnaire peut le montrer
/// (#5403).
///
/// Le texte vient du greffon et part vers l'écran de quiconque ouvre le
/// gestionnaire : il ne doit porter ni secret ni pavé. Sont remplacés par
/// `***` les identifiants d'une URL (`scheme://user:pass@`) et la valeur qui
/// suit un mot-clé sensible (`token`, `password`, `secret`, `api_key`,
/// `authorization`, `bearer`…). Les retours à la ligne et les contrôles
/// deviennent des espaces, puis le tout est coupé à
/// [`PLUGIN_SETUP_MESSAGE_MAX_CHARS`] caractères.
pub fn public_setup_message(raw: &str) -> String {
    use std::sync::LazyLock;
    static URL_CREDENTIALS: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?i)([a-z][a-z0-9+.-]*://)[^/\s@]+@").unwrap());
    static SENSITIVE: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r#"(?i)(access[_-]?token|refresh[_-]?token|token|password|passwd|pwd|secret|client[_-]?secret|api[_-]?key|apikey|authorization|bearer|cookie)(\s*[=:]\s*|\s+)((?:(?:bearer|basic)\s+)?(?:"[^"]*"|'[^']*'|[^\s,;&]+))"#,
        )
        .unwrap()
    });

    let flat: String = raw
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    let flat = URL_CREDENTIALS.replace_all(&flat, "${1}***@");
    let flat = SENSITIVE.replace_all(&flat, "${1}${2}***");
    if flat.chars().count() > PLUGIN_SETUP_MESSAGE_MAX_CHARS {
        let mut cut: String = flat.chars().take(PLUGIN_SETUP_MESSAGE_MAX_CHARS).collect();
        cut.push('…');
        cut
    } else {
        flat.into_owned()
    }
}

/// La fiche d'un greffon résident, telle que le gestionnaire la liste.
fn plugin_info_of(p: &dyn TunePlugin) -> PluginInfo {
    PluginInfo {
        name: p.name().to_string(),
        display_name: p.display_name().to_string(),
        version: p.version().to_string(),
        description: p.description().to_string(),
        enabled: true,
        config_schema: p.config_schema(),
        required_feature: p.required_feature().map(|f| f.display_name().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outputs::traits::ProviderContext;
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

    /// Greffon dont le `setup()` attend quelque chose qui ne vient jamais
    /// (un appareil du réseau éteint, un service muet) — #5403.
    struct HangingPlugin;

    #[async_trait]
    impl TunePlugin for HangingPlugin {
        fn name(&self) -> &str {
            "hanging"
        }
        fn version(&self) -> &str {
            "0.0.1"
        }
        fn description(&self) -> &str {
            "Never finishes its setup"
        }
        async fn setup(&mut self, _ctx: &PluginContext) -> Result<(), String> {
            std::future::pending::<()>().await;
            Ok(())
        }
        async fn teardown(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    /// #5403 — un `setup()` qui ne rend jamais la main est coupé à
    /// [`PLUGIN_SETUP_TIMEOUT`] : le démarrage continue, le greffon suivant se
    /// charge, et celui qui pendait n'est pas résident.
    ///
    /// Horloge en pause : les 30 s passent sans qu'on les attende. La borne
    /// extérieure (une heure) ne sert qu'à rendre un ROUGE lisible au lieu d'un
    /// test qui pend quand la borne du chargeur manque.
    #[tokio::test(start_paused = true)]
    async fn un_setup_qui_pend_est_coupe_et_le_demarrage_continue_5403() {
        let dir = tempfile::tempdir().unwrap();
        let loader = PluginLoader::new(dir.path().to_path_buf());
        loader.register(Box::new(HangingPlugin)).await;
        loader.register(Box::new(TestPlugin::new())).await;

        let debut = tokio::time::Instant::now();
        let loaded = tokio::time::timeout(
            Duration::from_secs(3600),
            loader.setup_all("http://localhost:8888"),
        )
        .await
        .expect("setup_all doit rendre la main malgré un setup() qui pend (#5403)");
        let ecoule = debut.elapsed();

        assert_eq!(loaded, vec!["test-plugin"]);
        assert_eq!(loader.plugin_count().await, 1);
        assert!(
            ecoule >= PLUGIN_SETUP_TIMEOUT && ecoule < PLUGIN_SETUP_TIMEOUT * 2,
            "coupé à la borne, pas avant ni bien après : {ecoule:?}"
        );
    }

    /// Greffon qui pend à ses `bloquants` premiers `setup()`, puis charge :
    /// l'appareil du réseau qui finit par répondre (#5403).
    struct PendPuisCharge {
        essais: Arc<std::sync::atomic::AtomicUsize>,
        bloquants: usize,
    }

    #[async_trait]
    impl TunePlugin for PendPuisCharge {
        fn name(&self) -> &str {
            "pend-puis-charge"
        }
        fn version(&self) -> &str {
            "0.2.0"
        }
        fn description(&self) -> &str {
            "Hangs, then loads"
        }
        async fn setup(&mut self, _ctx: &PluginContext) -> Result<(), String> {
            let n = self
                .essais
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < self.bloquants {
                std::future::pending::<()>().await;
            }
            Ok(())
        }
        async fn teardown(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    fn rapport(loader: &PluginLoader) -> PluginSetupReport {
        loader.setup_report().lock().unwrap().clone()
    }

    /// #5403 — un greffon coupé à la borne ne disparaît plus : il reste dans
    /// le rapport, en erreur « démarrage trop long », avec sa durée et la
    /// borne. Il n'est pas résident pour autant.
    #[tokio::test(start_paused = true)]
    async fn un_greffon_coupe_reste_en_erreur_avec_sa_duree_5403() {
        let dir = tempfile::tempdir().unwrap();
        let loader = PluginLoader::new(dir.path().to_path_buf());
        loader.register(Box::new(HangingPlugin)).await;
        loader.register(Box::new(FailingPlugin)).await;
        loader.register(Box::new(TestPlugin::new())).await;

        let loaded = loader.setup_all("http://localhost:8888").await;
        assert_eq!(loaded, vec!["test-plugin"]);
        assert_eq!(
            loader.plugin_count().await,
            1,
            "le greffon coupé n'est pas résident"
        );

        let r = rapport(&loader);
        assert_eq!(
            r.errors.len(),
            2,
            "le greffon coupé ET le greffon en échec doivent rester visibles : {:?}",
            r.errors
        );
        let e = &r.errors[0];
        assert_eq!(e.name, "hanging");
        assert_eq!(e.reason, PluginSetupErrorReason::SetupTimeout);
        assert_eq!(e.reason.as_str(), "setup_timeout");
        assert_eq!(e.timeout_ms, PLUGIN_SETUP_TIMEOUT.as_millis() as u64);
        assert!(
            e.duration_ms >= e.timeout_ms,
            "la durée mesurée doit compter la coupure : {} ms",
            e.duration_ms
        );
        assert!(e.message.is_none());
        assert!(r.loaded_after_retry.is_empty());
    }

    /// Échoue à ses `echecs` premiers `setup()`, avec un secret dans le
    /// message, puis charge.
    struct EchoueNFois {
        essais: Arc<std::sync::atomic::AtomicUsize>,
        echecs: usize,
    }

    #[async_trait]
    impl TunePlugin for EchoueNFois {
        fn name(&self) -> &str {
            "echoue"
        }
        fn version(&self) -> &str {
            "0.3.0"
        }
        fn description(&self) -> &str {
            "Fails, then loads"
        }
        async fn setup(&mut self, _ctx: &PluginContext) -> Result<(), String> {
            let n = self
                .essais
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < self.echecs {
                return Err(format!(
                    "appareil injoignable\nhttp://admin:hunter2@192.168.1.9/api token=abc123 {}",
                    "x".repeat(400)
                ));
            }
            Ok(())
        }
        async fn teardown(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    /// #5403 (décision du 29/09) — un `setup()` qui ÉCHOUE au démarrage reste
    /// visible lui aussi : `setup_failed`, avec le message du greffon, tronqué
    /// et sans secret. Réessayer le recharge.
    #[tokio::test]
    async fn un_greffon_en_echec_reste_visible_et_se_reessaie_5403() {
        let dir = tempfile::tempdir().unwrap();
        let essais = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let loader = PluginLoader::new(dir.path().to_path_buf());
        loader
            .register(Box::new(EchoueNFois {
                essais: Arc::clone(&essais),
                echecs: 1,
            }))
            .await;

        assert!(loader.setup_all("http://localhost:8888").await.is_empty());
        assert_eq!(
            loader.plugin_count().await,
            0,
            "pas résident après un échec"
        );
        let r = rapport(&loader);
        assert_eq!(
            r.errors.len(),
            1,
            "l'échec doit rester visible : {:?}",
            r.errors
        );
        let e = &r.errors[0];
        assert_eq!(e.name, "echoue");
        assert_eq!(e.reason, PluginSetupErrorReason::SetupFailed);
        assert_eq!(e.reason.as_str(), "setup_failed");
        let m = e.message.as_deref().expect("le message du greffon");
        assert!(m.starts_with("appareil injoignable http://"), "{m}");
        assert!(
            !m.contains("hunter2") && !m.contains("abc123"),
            "secret publié : {m}"
        );
        assert!(
            m.chars().count() <= PLUGIN_SETUP_MESSAGE_MAX_CHARS + 1,
            "tronqué : {} caractères",
            m.chars().count()
        );

        assert!(matches!(
            loader.retry_setup("echoue").await,
            PluginRetryOutcome::Loaded { .. }
        ));
        assert_eq!(loader.plugin_count().await, 1);
        assert!(rapport(&loader).errors.is_empty());
    }

    #[test]
    fn le_message_publie_est_expurge_et_borne_5403() {
        assert_eq!(public_setup_message("ok\n  court"), "ok court");
        assert_eq!(
            public_setup_message("GET https://u:p4ss@h.example/x refusé"),
            "GET https://***@h.example/x refusé"
        );
        for (brut, secret) in [
            ("password=s3cr3t", "s3cr3t"),
            ("api_key: \"k-123\"", "k-123"),
            ("Authorization: Bearer eyJhbGciOi", "eyJhbGciOi"),
            ("my_token=zz9", "zz9"),
            ("client_secret 'qq'", "qq"),
        ] {
            let m = public_setup_message(brut);
            assert!(!m.contains(secret), "{brut} → {m}");
            assert!(m.contains("***"), "{brut} → {m}");
        }
        let long = public_setup_message(&"é".repeat(500));
        assert_eq!(long.chars().count(), PLUGIN_SETUP_MESSAGE_MAX_CHARS + 1);
        assert!(long.ends_with('…'));
    }

    /// #5403 — Réessayer relance le `setup()` sous la même borne : le greffon
    /// qui répond enfin rejoint le jeu résident et quitte les erreurs.
    #[tokio::test(start_paused = true)]
    async fn reessayer_charge_le_greffon_qui_repond_enfin_5403() {
        let dir = tempfile::tempdir().unwrap();
        let essais = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let loader = PluginLoader::new(dir.path().to_path_buf());
        loader
            .register(Box::new(PendPuisCharge {
                essais: Arc::clone(&essais),
                bloquants: 1,
            }))
            .await;

        assert!(loader.setup_all("http://localhost:8888").await.is_empty());
        assert_eq!(rapport(&loader).errors.len(), 1);

        match loader.retry_setup("pend-puis-charge").await {
            PluginRetryOutcome::Loaded { .. } => {}
            _ => panic!("le nouvel essai devait charger le greffon"),
        }
        assert_eq!(essais.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(loader.plugin_count().await, 1, "résident après l'essai");
        let r = rapport(&loader);
        assert!(r.errors.is_empty(), "plus en erreur : {:?}", r.errors);
        assert_eq!(r.loaded_after_retry.len(), 1);
        assert_eq!(r.loaded_after_retry[0].name, "pend-puis-charge");
        assert_eq!(r.loaded_after_retry[0].version, "0.2.0");

        assert!(matches!(
            loader.retry_setup("pend-puis-charge").await,
            PluginRetryOutcome::NotInError
        ));
        assert!(matches!(
            loader.retry_setup("inconnu").await,
            PluginRetryOutcome::NotInError
        ));
    }

    /// #5403 — un nouvel essai qui pend encore est coupé à la MÊME borne, et
    /// le greffon reste en erreur, réessayable.
    #[tokio::test(start_paused = true)]
    async fn reessayer_un_greffon_qui_pend_encore_le_laisse_en_erreur_5403() {
        let dir = tempfile::tempdir().unwrap();
        let loader = PluginLoader::new(dir.path().to_path_buf());
        loader.register(Box::new(HangingPlugin)).await;
        loader.setup_all("http://localhost:8888").await;

        let debut = tokio::time::Instant::now();
        let issue = tokio::time::timeout(Duration::from_secs(3600), loader.retry_setup("hanging"))
            .await
            .expect("le nouvel essai doit être borné (#5403)");
        let ecoule = debut.elapsed();
        match issue {
            PluginRetryOutcome::StillInError(e) => {
                assert_eq!(e.reason, PluginSetupErrorReason::SetupTimeout);
                assert!(e.duration_ms >= PLUGIN_SETUP_TIMEOUT.as_millis() as u64);
            }
            _ => panic!("un greffon qui pend encore doit rester en erreur"),
        }
        assert!(
            ecoule >= PLUGIN_SETUP_TIMEOUT && ecoule < PLUGIN_SETUP_TIMEOUT * 2,
            "même borne qu'au démarrage : {ecoule:?}"
        );
        assert_eq!(loader.plugin_count().await, 0);
        assert_eq!(
            rapport(&loader).errors.len(),
            1,
            "une seule fiche, mise à jour"
        );
        // Rendu à la réserve : un troisième essai est possible.
        assert!(matches!(
            loader.retry_setup("hanging").await,
            PluginRetryOutcome::StillInError(_)
        ));
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

    // ─── register_output_provider ────────────────────────────────────────
    //
    // `register_output` couvre le cas où le plugin CONNAÎT ses appareils au
    // moment du `setup`. Un protocole réseau, non : les cibles Diretta
    // apparaissent et disparaissent pendant que le serveur tourne, et leur
    // nombre est inconnu tant que rien n'a répondu sur le fil.
    //
    // Ces tests prouvent le COMPORTEMENT ; le câblage côté hôte est gardé
    // séparément par `tune-server/tests/plugin_output_provider_seam.rs`.

    struct FournisseurDeTest {
        module: Option<&'static str>,
    }

    #[async_trait]
    impl OutputProvider for FournisseurDeTest {
        fn provider_name(&self) -> &str {
            "fournisseur-de-test"
        }

        fn required_module(&self) -> Option<&str> {
            self.module
        }

        async fn discover(&self, _ctx: &ProviderContext) -> Vec<Box<dyn OutputTarget>> {
            Vec::new()
        }
    }

    struct PluginFournisseur;

    #[async_trait]
    impl TunePlugin for PluginFournisseur {
        fn name(&self) -> &str {
            "plugin-fournisseur"
        }
        fn version(&self) -> &str {
            "0.1.0"
        }
        fn description(&self) -> &str {
            "Déclare un fournisseur de sorties, pas une sortie fixe"
        }
        async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
            ctx.register_output_provider(Arc::new(FournisseurDeTest {
                module: Some("diretta"),
            }));
            Ok(())
        }
        async fn teardown(&mut self) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn un_fournisseur_declare_est_bien_collecte() {
        let dir = tempfile::tempdir().unwrap();
        let loader = PluginLoader::new(dir.path().to_path_buf());
        loader.register(Box::new(PluginFournisseur)).await;
        loader.setup_all("http://localhost:8888").await;

        let reg = loader.take_registrations();

        assert_eq!(
            reg.output_providers.len(),
            1,
            "le fournisseur déclaré dans setup() n'est pas ressorti de take_registrations"
        );
        assert_eq!(
            reg.output_providers[0].provider_name(),
            "fournisseur-de-test"
        );
        // Le module payant doit traverser : c'est lui qui permet au serveur de
        // dire « au repos faute d'habilitation » plutôt que de se taire (#2392).
        assert_eq!(reg.output_providers[0].required_module(), Some("diretta"));
        // Un fournisseur n'est PAS une sortie : il ne doit pas atterrir dans le
        // registre des sorties, où il serait figé au démarrage.
        assert!(reg.outputs.is_empty());
    }

    #[tokio::test]
    async fn un_fournisseur_seul_ne_rend_pas_les_enregistrements_vides() {
        // `is_empty()` décide si l'hôte se donne la peine d'installer quoi que
        // ce soit. L'oublier ferait silencieusement sauter l'installation d'un
        // plugin qui ne déclare QU'un fournisseur — le cas de Diretta.
        let dir = tempfile::tempdir().unwrap();
        let loader = PluginLoader::new(dir.path().to_path_buf());
        loader.register(Box::new(PluginFournisseur)).await;
        loader.setup_all("http://localhost:8888").await;

        assert!(!loader.take_registrations().is_empty());
    }

    #[tokio::test]
    async fn un_plugin_dont_le_setup_echoue_ne_laisse_aucun_fournisseur() {
        // Même règle que pour les sorties : un plugin à moitié monté ne doit
        // pas voir son fournisseur installé.
        let dir = tempfile::tempdir().unwrap();
        let loader = PluginLoader::new(dir.path().to_path_buf());
        loader.register(Box::new(FailingPlugin)).await;
        loader.setup_all("http://localhost:8888").await;

        assert!(loader.take_registrations().output_providers.is_empty());
    }
}
