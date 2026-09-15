use super::*;
use crate::streaming::{ServiceRegistry, test_service::TestService};
use std::sync::atomic::{AtomicUsize, Ordering};

struct SourcePlugin {
    name: &'static str,
    sources: Vec<&'static str>,
    fail: bool,
    stops: Arc<AtomicUsize>,
}
#[async_trait]
impl TunePlugin for SourcePlugin {
    fn name(&self) -> &str {
        self.name
    }
    fn version(&self) -> &str {
        "1"
    }
    fn description(&self) -> &str {
        "fixture"
    }
    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        for name in &self.sources {
            ctx.register_streaming_service(Box::new(TestService(name)))?;
        }
        #[cfg(feature = "plugin-http")]
        ctx.register_router(axum::Router::new());
        if self.fail {
            Err("setup refused after registration".into())
        } else {
            Ok(())
        }
    }
    async fn teardown(&mut self) -> Result<(), String> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
fn source(
    name: &'static str,
    sources: Vec<&'static str>,
    fail: bool,
    stops: &Arc<AtomicUsize>,
) -> Box<dyn TunePlugin> {
    Box::new(SourcePlugin {
        name,
        sources,
        fail,
        stops: stops.clone(),
    })
}

#[tokio::test]
async fn streaming_registration_is_deferred_and_drained_once() {
    let dir = tempfile::tempdir().unwrap();
    let stops = Arc::new(AtomicUsize::new(0));
    let mut loader = PluginLoader::new(dir.path().into());
    loader
        .register(source("one", vec!["first"], false, &stops))
        .await;
    assert!(loader.take_registrations().is_empty());
    assert_eq!(loader.setup_all("http://localhost:0").await, vec!["one"]);
    let mut reg = loader.take_registrations();
    #[cfg(feature = "plugin-http")]
    reg.routers.clear();
    assert!(!reg.is_empty(), "a source-only plugin must be installed");
    assert_eq!(reg.streaming_services.len(), 1);
    let original = reg.streaming_services[0].service.clone();
    let mut registry = ServiceRegistry::new();
    registry.register_plugins(reg.streaming_services).unwrap();
    assert!(Arc::ptr_eq(&original, &registry.get("first").unwrap()));
    assert!(loader.take_registrations().is_empty());
    loader.teardown_all().await;
    assert_eq!(stops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn streaming_conflict_discards_entire_plugin_and_tears_it_down() {
    for (reserved, names) in [
        (vec!["occupied".into()], vec!["fresh", "occupied"]),
        (vec![], vec!["fresh", "fresh"]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let stops = Arc::new(AtomicUsize::new(0));
        let mut loader = PluginLoader::new(dir.path().into());
        loader.reserve_streaming_services(reserved);
        loader
            .register(source("conflicting", names, false, &stops))
            .await;
        assert!(
            loader.setup_all("http://localhost:0").await.is_empty(),
            "colliding plugin must not be reported loaded"
        );
        assert!(
            loader.take_registrations().is_empty(),
            "collision must discard services AND routes"
        );
        assert_eq!(
            stops.load(Ordering::SeqCst),
            1,
            "failed installation must stop owned tasks"
        );
        assert!(loader.loaded_plugins().await.is_empty());
    }
}

#[tokio::test]
async fn streaming_setup_failure_does_not_reserve_a_source_for_the_next_plugin() {
    let dir = tempfile::tempdir().unwrap();
    let stops = Arc::new(AtomicUsize::new(0));
    let loader = PluginLoader::new(dir.path().into());
    loader
        .register(source("failed", vec!["shared"], true, &stops))
        .await;
    loader
        .register(source("owner", vec!["shared"], false, &stops))
        .await;
    loader
        .register(source("shadow", vec!["shared"], false, &stops))
        .await;
    assert_eq!(loader.setup_all("http://localhost:0").await, vec!["owner"]);
    assert_eq!(loader.take_registrations().streaming_services.len(), 1);
    assert_eq!(stops.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn streaming_duplicate_plugin_name_cannot_survive_as_a_loaded_owner() {
    let dir = tempfile::tempdir().unwrap();
    let stops = Arc::new(AtomicUsize::new(0));
    let loader = PluginLoader::new(dir.path().into());
    loader
        .register(source("same", vec!["first"], false, &stops))
        .await;
    loader
        .register(source("same", vec!["second"], false, &stops))
        .await;
    assert_eq!(loader.setup_all("http://localhost:0").await, vec!["same"]);
    assert_eq!(
        loader.plugin_count().await,
        1,
        "refused duplicate must not remain resident to receive events"
    );
    let registrations = loader.take_registrations();
    assert_eq!(registrations.streaming_services.len(), 1);
    assert_eq!(registrations.streaming_services[0].name, "first");
}

#[tokio::test]
async fn streaming_disabled_plugin_has_no_service_or_routes() {
    let dir = tempfile::tempdir().unwrap();
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    SettingsRepo::with_backend(db.clone())
        .set("plugin_disabled_enabled", "false")
        .unwrap();
    let loader = PluginLoader::new(dir.path().into()).with_db(db);
    loader
        .register(source(
            "disabled",
            vec!["unused"],
            false,
            &Arc::new(AtomicUsize::new(0)),
        ))
        .await;
    assert!(loader.setup_all("http://localhost:0").await.is_empty());
    assert!(loader.take_registrations().is_empty());
}

#[test]
fn streaming_registry_rejects_a_racing_collision_atomically() {
    let mut registry = ServiceRegistry::new();
    registry.register(Box::new(TestService("occupied")));
    let owner = registry.get("occupied").unwrap();
    let ctx = PluginContext::new("", PathBuf::new());
    ctx.register_streaming_service(Box::new(TestService("fresh")))
        .unwrap();
    ctx.register_streaming_service(Box::new(TestService("occupied")))
        .unwrap();
    assert!(
        registry
            .register_plugins(ctx.take_registrations().streaming_services)
            .is_err()
    );
    assert!(
        registry.get("fresh").is_none(),
        "refusing a batch must not leave its earlier entries installed"
    );
    assert!(
        Arc::ptr_eq(&owner, &registry.get("occupied").unwrap()),
        "a plugin must not replace an existing service"
    );
}
