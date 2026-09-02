//! Plugin runtime, from the outside: what a plugin's routes look like once the
//! host has mounted them, and what `/api/v1/plugins` says about them.
//!
//! The interesting assertions here are the security ones. `register_router`
//! hands a plugin control over a path segment inside `/api/v1`, and the auth
//! middleware's public-path allowlist is built from *substring* matches — so
//! "plugin routes inherit auth" is a claim that has to be tested, not assumed.

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::routing::get;
use serde_json::Value;
use tower::ServiceExt;

use crate::use_scratch_plugin_data_dir;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};
use tune_server::state::AppState;

const SECRET: &str = "test-jwt-secret";

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

/// A plugin router with a plain route and one whose path deliberately collides
/// with the auth middleware's allowlist.
fn victim_router() -> axum::Router<()> {
    axum::Router::new()
        .route("/ping", get(|| async { "pong" }))
        .route("/auth/login", get(|| async { "secret" }))
        .route("/system/health", get(|| async { "secret" }))
}

fn app_with_plugin(state: &AppState, name: &str) -> axum::Router {
    tune_server::routes::router_with_plugins(
        state.clone(),
        vec![(name.to_string(), victim_router())],
    )
}

fn enable_auth(state: &AppState) {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("auth_enabled", "true").unwrap();
    settings.set("jwt_secret", SECRET).unwrap();
}

async fn status_of(app: &axum::Router, path: &str, token: Option<&str>) -> StatusCode {
    let mut req = Request::get(path);
    if let Some(tok) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {tok}"));
    }
    app.clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

async fn body_of(app: &axum::Router, path: &str) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

async fn post_json(app: &axum::Router, path: &str, body: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(path)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// The two settings keys `install`/`update` write, read straight back from the
/// database — the only place that decides what the next boot does.
fn reglages_d_installation(state: &AppState, name: &str) -> (Option<String>, Option<String>) {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    (
        settings
            .get(&format!("plugin_{name}_installed"))
            .ok()
            .flatten(),
        settings
            .get(&format!("plugin_{name}_enabled"))
            .ok()
            .flatten(),
    )
}

// ---------------------------------------------------------------------------
// Mounting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plugin_router_is_mounted_under_ext_plugin_name() {
    let state = new_state();
    let app = app_with_plugin(&state, "demo");

    let (status, body) = body_of(&app, "/api/v1/ext/demo/ping").await;
    assert_eq!(status, StatusCode::OK, "plugin route should serve");
    assert_eq!(body, "pong");
}

#[tokio::test]
async fn plugin_namespace_is_derived_from_the_name_not_chosen() {
    let state = new_state();
    let app = app_with_plugin(&state, "demo");

    // The same router mounted under a different name must not answer.
    assert_eq!(
        status_of(&app, "/api/v1/ext/other/ping", None).await,
        StatusCode::NOT_FOUND
    );
    // Nor outside the /ext namespace.
    assert_eq!(
        status_of(&app, "/api/v1/ping", None).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn unknown_subpath_under_a_plugin_is_not_found() {
    let state = new_state();
    let app = app_with_plugin(&state, "demo");

    // Documents the `nest_service` consequence: the plugin router owns
    // everything under its prefix, so this is its 404, not `api_fallback`'s.
    assert_eq!(
        status_of(&app, "/api/v1/ext/demo/nope", None).await,
        StatusCode::NOT_FOUND
    );
}

// ---------------------------------------------------------------------------
// Auth — the reason plugin routes are nested inside /api/v1 at all
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plugin_routes_inherit_auth() {
    let state = new_state();
    enable_auth(&state);
    let app = app_with_plugin(&state, "demo");

    assert_eq!(
        status_of(&app, "/api/v1/ext/demo/ping", None).await,
        StatusCode::UNAUTHORIZED,
        "a plugin route must not be reachable without a token"
    );

    let token = tune_server::auth::sign_jwt(1, "admin", SECRET).unwrap();
    assert_eq!(
        status_of(&app, "/api/v1/ext/demo/ping", Some(&token)).await,
        StatusCode::OK,
        "a valid token must reach the plugin route"
    );
}

/// A plugin route whose path contains one of the middleware's public
/// substrings must still require a token. Without the `/ext/` guard in
/// `auth_middleware`, every assertion below returns 200 and any plugin — or
/// anyone who can name one — has an unauthenticated hole inside /api/v1.
#[tokio::test]
async fn plugin_routes_cannot_smuggle_past_the_public_path_allowlist() {
    let state = new_state();
    enable_auth(&state);
    let app = app_with_plugin(&state, "demo");

    for path in [
        "/api/v1/ext/demo/auth/login",
        "/api/v1/ext/demo/system/health",
    ] {
        assert_eq!(
            status_of(&app, path, None).await,
            StatusCode::UNAUTHORIZED,
            "{path} must not be treated as a public path"
        );
    }
}

/// Same hole reached through the plugin *name* rather than its routes.
#[tokio::test]
async fn a_plugin_named_auth_does_not_become_public() {
    let state = new_state();
    enable_auth(&state);
    let app = app_with_plugin(&state, "auth");

    assert_eq!(
        status_of(&app, "/api/v1/ext/auth/ping", None).await,
        StatusCode::UNAUTHORIZED
    );
}

/// The guard must not have broken the allowlist it sits in front of.
#[tokio::test]
async fn core_public_paths_are_still_public() {
    let state = new_state();
    enable_auth(&state);
    let app = app_with_plugin(&state, "demo");

    assert_eq!(
        status_of(&app, "/api/v1/system/health", None).await,
        StatusCode::OK
    );
    assert_eq!(
        status_of(&app, "/api/v1/system/version", None).await,
        StatusCode::OK
    );
    // And a normal core route is still protected.
    assert_eq!(
        status_of(&app, "/api/v1/zones", None).await,
        StatusCode::UNAUTHORIZED
    );
}

// ---------------------------------------------------------------------------
// Introspection — /api/v1/plugins must report reality
// ---------------------------------------------------------------------------

struct Loads;

#[async_trait]
impl TunePlugin for Loads {
    fn name(&self) -> &str {
        "loads"
    }
    fn version(&self) -> &str {
        "1.2.3"
    }
    fn description(&self) -> &str {
        "Sets up fine"
    }
    async fn setup(&mut self, _ctx: &PluginContext) -> Result<(), String> {
        Ok(())
    }
    async fn teardown(&mut self) -> Result<(), String> {
        Ok(())
    }
}

/// Registers a router, so the injection path can be checked end to end.
struct ServesRoutes;

#[async_trait]
impl TunePlugin for ServesRoutes {
    fn name(&self) -> &str {
        "injected"
    }
    fn version(&self) -> &str {
        "0.1.0"
    }
    fn description(&self) -> &str {
        "Built outside the tree and handed to init"
    }
    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        ctx.register_router(axum::Router::new().route("/ping", get(|| async { "pong" })));
        Ok(())
    }
    async fn teardown(&mut self) -> Result<(), String> {
        Ok(())
    }
}

struct FailsSetup;

#[async_trait]
impl TunePlugin for FailsSetup {
    fn name(&self) -> &str {
        "fails"
    }
    fn version(&self) -> &str {
        "0.0.1"
    }
    fn description(&self) -> &str {
        "Never finishes setup"
    }
    async fn setup(&mut self, _ctx: &PluginContext) -> Result<(), String> {
        Err("nope".into())
    }
    async fn teardown(&mut self) -> Result<(), String> {
        Ok(())
    }
}

/// `plugin_{name}_enabled = false` keeps a plugin out at boot, and it must not
/// linger in the REST view either.
///
/// Injected through `init`'s `extra` — the path an out-of-tree binary uses — to
/// pin down that such a plugin is governed by the same switch as a compiled-in
/// one rather than slipping past it.
#[tokio::test]
async fn the_enabled_setting_keeps_a_plugin_out() {
    use_scratch_plugin_data_dir();

    let state = new_state();
    SettingsRepo::with_backend(state.backend.clone())
        .set("plugin_loads_enabled", "false")
        .unwrap();

    tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![Box::new(Loads)]).await;

    let reported = state.plugin_info.get().map(|v| v.len()).unwrap_or_default();
    assert_eq!(reported, 0, "a disabled plugin must not be reported");
}

/// A plugin handed to `init` from outside the tree loads on equal terms with a
/// compiled-in one: same setup, same snapshot, same mount.
#[tokio::test]
async fn an_injected_plugin_loads_and_mounts_its_router() {
    use_scratch_plugin_data_dir();

    let state = new_state();
    let routers =
        tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![Box::new(ServesRoutes)])
            .await;

    assert_eq!(routers.len(), 1, "the injected plugin contributed a router");
    assert_eq!(routers[0].0, "injected", "mount name comes from the plugin");

    let names: Vec<&str> = state
        .plugin_info
        .get()
        .expect("init publishes the snapshot")
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(names, vec!["injected"]);

    // End to end: the router it registered actually serves, under its own name.
    let app = tune_server::routes::router_with_plugins(state.clone(), routers);
    let (status, body) = body_of(&app, "/api/v1/ext/injected/ping").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "pong");
}

/// A plugin whose setup failed must not be advertised as installed and enabled.
/// `setup_all` drops it from the loader, and the snapshot `plugins::init`
/// publishes inherits that — this pins both halves down together.
#[tokio::test]
async fn rest_reports_only_plugins_that_actually_loaded() {
    use_scratch_plugin_data_dir();

    let state = new_state();
    let routers = tune_server::plugins::init(
        &state,
        "http://127.0.0.1:0",
        vec![Box::new(Loads), Box::new(FailsSetup)],
    )
    .await;
    // Ni « loads » ni « failssetup » ne monte de routeur ; on ne peut pas
    // exiger la liste vide, car dj et karaoke — compilés dans le jeu livré —
    // en contribuent quand ils sont installés.
    assert!(
        !routers
            .iter()
            .any(|(n, _)| n == "loads" || n == "failssetup"),
        "aucun de ces deux greffons n'enregistre de routeur"
    );

    let names: Vec<&str> = state
        .plugin_info
        .get()
        .expect("init publishes the snapshot")
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert!(names.contains(&"loads"), "loads doit être chargé");
    assert!(
        !names.contains(&"failssetup"),
        "a failed setup must not be reported (chargés : {names:?})"
    );

    // And the REST view agrees.
    let app = tune_server::routes::router(state.clone());
    let (status, body) = body_of(&app, "/api/v1/plugins").await;
    assert_eq!(status, StatusCode::OK);
    let list: Value = serde_json::from_str(&body).unwrap();
    // Même raison qu'au-dessus : on isole « loads » au lieu de compter les
    // entrées SDK, dont dj et karaoke font partie dans le jeu livré.
    let entries = list.as_array().unwrap();
    let loads = entries
        .iter()
        .find(|p| p["type"] == "sdk" && p["name"] == "loads")
        .unwrap_or_else(|| panic!("« loads » absent de /api/v1/plugins : {entries:?}"));
    assert_eq!(loads["version"], "1.2.3");
    assert_eq!(loads["url"], "/api/v1/ext/loads");
    assert!(
        !entries.iter().any(|p| p["name"] == "failssetup"),
        "un setup en échec ne doit pas être publié"
    );
}

/// An opt-in plugin (like DJ/Karaoke): compiled in but dormant until the user
/// installs it from the plugin manager.
struct OptIn;

#[async_trait]
impl TunePlugin for OptIn {
    fn name(&self) -> &str {
        "optin"
    }
    fn version(&self) -> &str {
        "2.0.0"
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

/// An opt-in plugin does not run by default, but `/api/v1/plugins` still lists
/// it as `installed:false` so the manager renders an "Install" button.
#[tokio::test]
async fn opt_in_plugin_is_listed_as_installable_not_loaded() {
    use_scratch_plugin_data_dir();

    let state = new_state();
    tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![Box::new(OptIn)]).await;

    // Not among the loaded plugins…
    //
    // On cherche « optin » nommément plutôt que de compter : le jeu de
    // fonctionnalités LIVRÉ compile aussi dj, karaoke et plugins-wasm, qui
    // s'ajoutent aux deux listes. Compter revenait à supposer qu'aucun greffon
    // n'est compilé — vrai sous le `--features oaat` de la CI, faux sous ce
    // qu'on publie, et c'est pourquoi ces tests échouaient sans que personne
    // ne le voie.
    let loaded: Vec<&str> = state
        .plugin_info
        .get()
        .map(|v| v.iter().map(|p| p.name.as_str()).collect())
        .unwrap_or_default();
    assert!(
        !loaded.contains(&"optin"),
        "an opt-in plugin must not load by default (chargés : {loaded:?})"
    );
    // …but surfaced as available so the manager can still offer "Install".
    let available = state.plugin_available.get().expect("init publishes it");
    let optin = available
        .iter()
        .find(|p| p.name == "optin")
        .expect("optin doit être proposé à l'installation");
    assert!(optin.opt_in);

    let app = tune_server::routes::router(state.clone());
    let (status, body) = body_of(&app, "/api/v1/plugins").await;
    assert_eq!(status, StatusCode::OK);
    let list: Value = serde_json::from_str(&body).unwrap();
    let entry = list
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "optin")
        .expect("the opt-in plugin is listed even while dormant");
    assert_eq!(entry["type"], "sdk");
    assert_eq!(entry["installed"], false, "not installed → Install button");
    assert_eq!(entry["enabled"], false);
    assert_eq!(entry["url"], "/api/v1/ext/optin");
}

/// Once installed (`plugin_{name}_installed=true`), the same opt-in plugin
/// loads on the next boot and reports as installed + enabled.
#[tokio::test]
async fn opt_in_plugin_loads_after_install() {
    use_scratch_plugin_data_dir();

    let state = new_state();
    SettingsRepo::with_backend(state.backend.clone())
        .set("plugin_optin_installed", "true")
        .unwrap();
    tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![Box::new(OptIn)]).await;

    let names: Vec<&str> = state
        .plugin_info
        .get()
        .expect("init publishes the snapshot")
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert!(
        names.contains(&"optin"),
        "an installed opt-in plugin loads (chargés : {names:?})"
    );
    // « plus rien de dormant » ne vaut que pour CE greffon : dj et karaoke,
    // compilés dans le jeu livré, restent légitimement dormants ici puisqu'on
    // ne les a pas installés.
    let dormant: Vec<&str> = state
        .plugin_available
        .get()
        .map(|v| v.iter().map(|p| p.name.as_str()).collect())
        .unwrap_or_default();
    assert!(
        !dormant.contains(&"optin"),
        "optin ne doit plus être dormant une fois installé (dormants : {dormant:?})"
    );

    let app = tune_server::routes::router(state.clone());
    let (_status, body) = body_of(&app, "/api/v1/plugins").await;
    let list: Value = serde_json::from_str(&body).unwrap();
    let entry = list
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "optin")
        .expect("listed");
    assert_eq!(entry["installed"], true);
    assert_eq!(entry["enabled"], true);
}

/// `dj` et `karaoke` sont compilés dans TOUS les binaires publiés — les six
/// lignes de build de `release.yml` les listent avec `bandcamp`. Ils ne
/// doivent pourtant plus figurer dans le gestionnaire : aucun écran du client
/// ne sait les atteindre, et proposer « Installer » sur une fonction que rien
/// n'expose ne rend rien à l'utilisateur (#2090).
///
/// Le test s'exécute sous le jeu de fonctionnalités livré. Sans `--features
/// dj,karaoke` les deux greffons ne sont pas enregistrés du tout, et le test
/// passerait sans rien prouver : on l'annonce dans le message d'échec plutôt
/// que de laisser croire à une preuve.
#[tokio::test]
async fn dj_and_karaoke_are_not_offered_in_the_plugin_manager() {
    use_scratch_plugin_data_dir();

    let state = new_state();
    tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![Box::new(OptIn)]).await;

    let app = tune_server::routes::router(state.clone());
    let (status, body) = body_of(&app, "/api/v1/plugins").await;
    assert_eq!(status, StatusCode::OK);
    let list: Value = serde_json::from_str(&body).unwrap();
    let names: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["name"].as_str())
        .collect();

    for hidden in ["dj", "karaoke"] {
        assert!(
            !names.contains(&hidden),
            "« {hidden} » ne doit plus être proposé par le gestionnaire (listés : {names:?})"
        );
    }

    // Contre-épreuve dans le même souffle : le filtre retire les greffons hors
    // catalogue, pas les greffons dormants en général. Un opt-in ordinaire,
    // dormant pour exactement la même raison, reste proposé.
    assert!(
        names.contains(&"optin"),
        "un opt-in catalogué doit rester proposé (listés : {names:?})"
    );
}

/// Retiré du catalogue n'est pas retiré du binaire : DJ se charge encore si on
/// pose son réglage d'installation à la main, et son routeur est monté comme
/// avant. C'est la différence entre cesser de promettre et supprimer.
///
/// Et ce qui TOURNE reste listé. Le filtre ne porte que sur le catalogue —
/// l'offre faite à qui n'a rien installé. Masquer un greffon déjà installé le
/// rendrait indésinstallable, et mentirait sur l'état de la machine.
#[cfg(feature = "dj")]
#[tokio::test]
async fn uncatalogued_dj_still_loads_when_installed_by_hand() {
    use_scratch_plugin_data_dir();

    let state = new_state();
    SettingsRepo::with_backend(state.backend.clone())
        .set("plugin_dj_installed", "true")
        .expect("marquer DJ installé");

    let routers = tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![]).await;
    assert!(
        routers.iter().any(|(name, _)| name == "dj"),
        "DJ doit encore contribuer son routeur (montés : {:?})",
        routers.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );

    let app = tune_server::routes::router(state.clone());
    let (_status, body) = body_of(&app, "/api/v1/plugins").await;
    let list: Value = serde_json::from_str(&body).unwrap();
    let entry = list
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "dj")
        .expect("un greffon qui tourne reste listé, hors catalogue ou non");
    assert_eq!(entry["enabled"], true);
}

// ---------------------------------------------------------------------------
// Installation — un nom qui ne désigne rien ne doit rien écrire (#2132)
// ---------------------------------------------------------------------------

/// Un greffon hors catalogue, comme DJ et Karaoké : compilé, dormant, et que
/// le gestionnaire ne propose PAS. Il ne figure ni dans `plugin_info` ni dans
/// `plugin_available` — c'est précisément ce qui le rend utile ici.
struct Uncatalogued;

#[async_trait]
impl TunePlugin for Uncatalogued {
    fn name(&self) -> &str {
        "horscatalogue"
    }
    fn version(&self) -> &str {
        "3.0.0"
    }
    fn description(&self) -> &str {
        "Compilé, dormant, jamais proposé"
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

/// Le défaut du ticket : `POST /plugins/{nom}/install` répondait
/// « installed » + « restart_required » pour N'IMPORTE quel nom, en posant
/// deux réglages que le démarrage ne peut satisfaire — le catalogue distant
/// sert encore « Synchronized Lyrics » (`platforms: "python"`), et un testeur
/// a « bien mis le plugin » sans aucun résultat.
///
/// La preuve est prise **dans la base**, pas sur le code de retour : ce sont
/// `plugin_{nom}_installed` / `_enabled` qui décidaient de la promesse.
#[tokio::test]
async fn un_nom_de_greffon_inconnu_n_ecrit_rien_et_le_dit() {
    use_scratch_plugin_data_dir();

    let state = new_state();
    tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![Box::new(OptIn)]).await;
    let app = tune_server::routes::router(state.clone());

    let (status, body) = post_json(&app, "/api/v1/plugins/lyrics/install", "{}").await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "un nom que ce binaire ne porte pas doit être refusé (corps : {body})"
    );
    assert_eq!(body["error"], "plugin_inconnu");
    assert_eq!(body["name"], "lyrics");

    assert_eq!(
        reglages_d_installation(&state, "lyrics"),
        (None, None),
        "aucun réglage ne doit être posé pour un greffon qui ne peut pas charger"
    );
}

/// La mise à jour écrit le MÊME réglage que l'installation : laisser ce
/// chemin ouvert rouvrait le trou par le bouton « Mettre à jour ».
#[tokio::test]
async fn la_mise_a_jour_refuse_aussi_un_nom_inconnu() {
    use_scratch_plugin_data_dir();

    let state = new_state();
    tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![Box::new(OptIn)]).await;
    let app = tune_server::routes::router(state.clone());

    let (status, body) = post_json(&app, "/api/v1/plugins/karaoke-python/update", "{}").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "corps : {body}");
    assert_eq!(body["error"], "plugin_inconnu");
    assert_eq!(
        reglages_d_installation(&state, "karaoke-python"),
        (None, None)
    );
}

/// Témoin : le garde-fou n'a pas fermé la porte à ce qui s'installe vraiment.
/// Un opt-in catalogué passe, et les deux réglages sont bien posés.
#[tokio::test]
async fn un_greffon_compile_s_installe_toujours() {
    use_scratch_plugin_data_dir();

    let state = new_state();
    tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![Box::new(OptIn)]).await;
    let app = tune_server::routes::router(state.clone());

    let (status, body) = post_json(&app, "/api/v1/plugins/optin/install", "{}").await;
    assert_eq!(status, StatusCode::OK, "corps : {body}");
    assert_eq!(body["status"], "installed");
    assert_eq!(body["restart_required"], true);

    assert_eq!(
        reglages_d_installation(&state, "optin"),
        (Some("true".into()), Some("true".into())),
        "l'installation d'un greffon réel doit toujours poser ses deux réglages"
    );
}

/// Témoin anti-régression de #2090 : « retiré du catalogue » n'est pas
/// « condamné ». Un greffon hors catalogue n'apparaît dans AUCUNE des deux
/// listes publiées par `init` — un garde-fou bâti sur elles l'aurait refusé —
/// et il doit pourtant rester installable par qui le nomme.
#[tokio::test]
async fn un_greffon_hors_catalogue_reste_installable_par_son_nom() {
    use_scratch_plugin_data_dir();

    let state = new_state();
    tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![Box::new(Uncatalogued)]).await;

    let charges: Vec<&str> = state
        .plugin_info
        .get()
        .map(|v| v.iter().map(|p| p.name.as_str()).collect())
        .unwrap_or_default();
    let proposes: Vec<&str> = state
        .plugin_available
        .get()
        .map(|v| v.iter().map(|p| p.name.as_str()).collect())
        .unwrap_or_default();
    assert!(
        !charges.contains(&"horscatalogue") && !proposes.contains(&"horscatalogue"),
        "le témoin ne prouve rien s'il figure dans l'une des deux listes \
         (chargés : {charges:?}, proposés : {proposes:?})"
    );

    let app = tune_server::routes::router(state.clone());
    let (status, body) = post_json(&app, "/api/v1/plugins/horscatalogue/install", "{}").await;
    assert_eq!(
        status,
        StatusCode::OK,
        "un greffon compilé mais hors catalogue reste installable nommément (corps : {body})"
    );
    assert_eq!(
        reglages_d_installation(&state, "horscatalogue"),
        (Some("true".into()), Some("true".into()))
    );
}

// ---------------------------------------------------------------------------
// Le second catalogue : la clef de réglages `plugins` (#2132)
// ---------------------------------------------------------------------------

/// Les fiches que porte la clef de réglages `plugins`.
///
/// Cette clef n'est écrite NULLE PART dans ce dépôt — `git grep 'set("plugins"'
/// ne rend rien, `get("plugins")` rend trois lectures. Ce qu'elle contient
/// vient donc de la table `settings` d'avant, celle du Tune écrit en Python
/// que la migration conserve, et le serveur la rendait telle quelle : aucun
/// `type`, aucun `platforms`, rien qui distingue une fiche vivante d'une fiche
/// morte. Les noms et libellés repris ici sont ceux du relevé du catalogue en
/// ligne du 29/08/2026 (« Synchronized Lyrics », « Karaoké », « Mode DJ »).
///
/// `horscatalogue` est le **témoin vivant** : un greffon que ce binaire porte
/// réellement, dont la fiche doit traverser le tri sans une virgule de
/// changement. Les trois autres ne désignent rien ici — et surtout pas les
/// greffons `dj` / `karaoke` compilés dans le jeu livré, dont les noms sont
/// `dj` et `karaoke` tout court.
const FICHES_HERITEES: &str = r#"[
  {"name":"lyrics","display_name":"Synchronized Lyrics","version":"0.1.0",
   "installed":true,"enabled":true},
  {"name":"karaoke-python","display_name":"Karaoké","version":"0.1.0",
   "installed":true,"enabled":true},
  {"name":"dj-mode","display_name":"Mode DJ","version":"0.1.0",
   "installed":false,"enabled":false},
  {"name":"horscatalogue","display_name":"Compilé, dormant, jamais proposé",
   "version":"3.0.0","installed":true,"enabled":true}
]"#;

/// Les quatre noms semés, et eux seuls. On ne compte JAMAIS la liste entière :
/// le jeu de fonctionnalités livré compile aussi `dj`, `karaoke` et
/// `plugins-wasm`, qui ajoutent leurs propres entrées — compter le tout
/// reviendrait à supposer un jeu de features, faux hors CI (cf. le
/// commentaire de `opt_in_plugin_is_listed_as_installable_not_loaded`).
const SEMES: [&str; 4] = ["lyrics", "karaoke-python", "dj-mode", "horscatalogue"];

fn semer_les_fiches_heritees(state: &AppState) {
    SettingsRepo::with_backend(state.backend.clone())
        .set("plugins", FICHES_HERITEES)
        .unwrap();
}

/// Les fiches héritées effectivement rendues, dans l'ordre.
fn fiches_heritees_rendues(liste: &Value) -> Vec<&Value> {
    liste
        .as_array()
        .expect("la liste des greffons est un tableau")
        .iter()
        .filter(|f| f["name"].as_str().is_some_and(|n| SEMES.contains(&n)))
        .collect()
}

async fn delete_json(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::delete(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Le défaut du ticket, pris sur un fait de base : le CONTENU de la liste
/// rendue par le gestionnaire, jamais un code HTTP.
///
/// `GET /api/v1/plugins` recopiait la clef `plugins` telle quelle en tête de
/// sa réponse. « Synchronized Lyrics » — la fiche que cherche exactement un
/// utilisateur qui veut des paroles — y était donc encore PROPOSÉE, sur un
/// serveur qui ne peut rien en faire.
#[tokio::test]
async fn le_gestionnaire_ne_propose_plus_les_fiches_de_l_ere_python() {
    use_scratch_plugin_data_dir();
    let state = new_state();
    tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![Box::new(Uncatalogued)]).await;
    semer_les_fiches_heritees(&state);

    let app = tune_server::routes::router(state.clone());
    let (status, body) = body_of(&app, "/api/v1/plugins").await;
    assert_eq!(status, StatusCode::OK);
    let liste: Value = serde_json::from_str(&body).unwrap();

    let rendues = fiches_heritees_rendues(&liste);
    let noms: Vec<&str> = rendues.iter().filter_map(|f| f["name"].as_str()).collect();
    assert_eq!(
        noms,
        vec!["horscatalogue"],
        "aucune fiche de l'ère Python ne doit rester proposée, et la seule \
         fiche vivante doit rester là (rendues : {noms:?})"
    );

    // Témoin vert : la fiche gardée sort INCHANGÉE. Un identifiant qui bouge
    // casse les installations existantes ; un libellé qui bouge casse la
    // reconnaissance à l'écran.
    let gardee = rendues[0];
    let semee: Value = serde_json::from_str(FICHES_HERITEES).unwrap();
    let attendue = semee
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "horscatalogue")
        .unwrap();
    assert_eq!(
        gardee, attendue,
        "la fiche vivante doit traverser le tri telle quelle"
    );
}

/// `GET /api/v1/system/plugins` est l'alias historique, et il lisait la clef
/// `plugins` par lui-même : la fiche morte y revenait même une fois écartée de
/// `/plugins`. Ici la réponse ne contient QUE des fiches héritées, donc le
/// compte porte sur la liste entière.
#[tokio::test]
async fn l_alias_system_plugins_trie_comme_la_liste() {
    use_scratch_plugin_data_dir();
    let state = new_state();
    tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![Box::new(Uncatalogued)]).await;
    semer_les_fiches_heritees(&state);

    let app = tune_server::routes::router(state.clone());
    let (status, body) = body_of(&app, "/api/v1/system/plugins").await;
    assert_eq!(status, StatusCode::OK);
    let liste: Value = serde_json::from_str(&body).unwrap();
    let noms: Vec<&str> = liste
        .as_array()
        .expect("un tableau")
        .iter()
        .filter_map(|f| f["name"].as_str())
        .collect();
    assert_eq!(
        noms,
        vec!["horscatalogue"],
        "l'alias doit rendre exactement une fiche, la vivante (rendues : {noms:?})"
    );
}

/// Le sort de la fiche que l'utilisateur croit avoir installée.
///
/// Sur un serveur ≤ v0.9.124, un clic sur « Synchronized Lyrics » posait
/// `plugin_lyrics_installed=true` et `plugin_lyrics_enabled=true` puis
/// répondait « installé, redémarrage requis ». Le garde-fou d'`install` a
/// fermé la porte, mais ces deux réglages sont toujours dans les bases : la
/// fiche du greffon continuait de se dire « installed ».
///
/// Elle est désormais dite `unavailable` — et rien n'est détruit dans le dos
/// de l'utilisateur : les deux réglages restent en base, et `DELETE` reste la
/// sortie qui les enlève.
#[tokio::test]
async fn une_fiche_installee_avant_le_garde_fou_est_dite_indisponible() {
    use_scratch_plugin_data_dir();
    let state = new_state();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("plugin_lyrics_installed", "true").unwrap();
    settings.set("plugin_lyrics_enabled", "true").unwrap();
    tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![Box::new(OptIn)]).await;

    let app = tune_server::routes::router(state.clone());
    let (status, body) = body_of(&app, "/api/v1/plugins/lyrics").await;
    assert_eq!(status, StatusCode::OK);
    let fiche: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        fiche["status"], "unavailable",
        "un réglage hérité ne doit plus valoir confirmation d'installation : {fiche}"
    );
    assert_eq!(fiche["installed"], false);
    assert_eq!(fiche["enabled"], false);

    // Rien n'a été effacé : la trace de l'utilisateur survit à la lecture.
    assert_eq!(
        reglages_d_installation(&state, "lyrics"),
        (Some("true".into()), Some("true".into())),
        "la route de lecture ne doit rien écrire ni rien détruire"
    );

    // Et la sortie existe : le bouton « Désinstaller » enlève bien les deux
    // réglages morts.
    let (status, corps) = delete_json(&app, "/api/v1/plugins/lyrics").await;
    assert_eq!(status, StatusCode::OK, "corps : {corps}");
    assert_eq!(
        reglages_d_installation(&state, "lyrics"),
        (None, None),
        "la désinstallation doit rester la sortie d'une fiche morte"
    );
}

/// Témoin vert de l'autre côté : un greffon que ce binaire porte vraiment,
/// installé et en attente de redémarrage, reste annoncé « installé ». Le tri
/// ne touche pas ce qui charge.
#[tokio::test]
async fn un_greffon_reel_en_attente_de_redemarrage_reste_installe() {
    use_scratch_plugin_data_dir();
    let state = new_state();
    tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![Box::new(OptIn)]).await;
    // Installé APRÈS `init` : le greffon ne chargera qu'au prochain boot, donc
    // il ne figure pas dans l'instantané des chargés — c'est exactement la
    // situation où la fiche ne tient que par ses réglages.
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("plugin_optin_installed", "true").unwrap();
    settings.set("plugin_optin_enabled", "true").unwrap();

    let app = tune_server::routes::router(state.clone());
    let (status, body) = body_of(&app, "/api/v1/plugins/optin").await;
    assert_eq!(status, StatusCode::OK);
    let fiche: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        fiche["status"], "installed",
        "un greffon compilé installé doit rester annoncé installé : {fiche}"
    );
    assert_eq!(fiche["installed"], true);
    assert_eq!(fiche["enabled"], true);
}
