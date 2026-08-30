//! Les concerts, de l'extérieur, maintenant qu'ils sont un plugin natif (#2363).
//!
//! Compilé seulement avec `--features concerts`. Ces tests exercent le vrai
//! câblage : l'arm de `plugins::register_builtin_plugins` construit
//! `tune_concerts::ConcertsPlugin`, `plugins::init` l'installe, et le routeur
//! qu'il contribue est monté sous `/api/v1/ext/concerts` — le préfixe vient de
//! `name()`, le plugin ne le choisit pas.
//!
//! Trois choses sont gardées ici, et chacune correspond à un piège déjà payé
//! ailleurs dans ce dépôt :
//!
//! 1. **La route de l'ancien cœur a bien disparu.** `GET /system/concerts`
//!    répondait dans tous les serveurs. Si l'extraction l'avait laissée en
//!    place, on aurait deux portes pour la même donnée — exactement le défaut
//!    corrigé côté cloud dans le même chantier.
//! 2. **Le plugin reste hors catalogue.** Aucun écran ne consomme ses routes ;
//!    l'offrir à l'installation vendrait une fonction que rien n'expose
//!    (#2090). Ce test échouera le jour où quelqu'un rebranchera
//!    `catalogued()` — et c'est le but : il faudra alors que l'écran existe.
//! 3. **Le corps d'erreur ne contient pas de phrase anglaise.** L'ancien
//!    handler rendait `{"error": "concerts: HTTP 500"}`, qu'une interface
//!    traduite en 11 langues aurait affichée telle quelle.
#![cfg(feature = "concerts")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::use_scratch_plugin_data_dir;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

/// Construit l'app avec le plugin chargé, sur un serveur PREMIUM.
///
/// Le greffon appartient au module « Concerts » : sur un compte gratuit ses
/// routes répondent 402 avant d'atteindre le moindre handler. Les tests qui
/// portent sur le contrat d'erreur du greffon doivent donc partir d'un serveur
/// qui a le droit de s'en servir — le cas gratuit a son propre test.
async fn app_avec_concerts_premium(state: &AppState) -> axum::Router {
    state.license.set_account_premium(true, None).await;
    app_avec_concerts(state).await
}

/// Construit l'app avec le plugin chargé par le vrai chemin d'enregistrement.
async fn app_avec_concerts(state: &AppState) -> axum::Router {
    use_scratch_plugin_data_dir();

    // Opt-in comme dj, karaoke et bandcamp : `default_enabled()` rend false et
    // `setup_all` le laisse dormant tant que `plugin_concerts_installed` n'est
    // pas posé.
    SettingsRepo::with_backend(state.backend.clone())
        .set("plugin_concerts_installed", "true")
        .expect("marquer concerts installé");

    let routers = tune_server::plugins::init(state, "http://127.0.0.1:0", vec![]).await;

    assert!(
        routers.iter().any(|(name, _)| name == "concerts"),
        "le greffon concerts doit contribuer un routeur monté sous son name()"
    );

    tune_server::routes::router_with_plugins(state.clone(), routers)
}

async fn get_json(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps = serde_json::from_slice(&octets).unwrap_or(Value::Null);
    (statut, corps)
}

#[tokio::test]
async fn la_route_du_coeur_a_disparu() {
    let state = new_state();
    let app = app_avec_concerts(&state).await;

    let (statut, _) = get_json(&app, "/api/v1/system/concerts").await;
    assert_eq!(
        statut,
        StatusCode::NOT_FOUND,
        "GET /system/concerts doit avoir disparu du cœur : la lecture vit \
         désormais sous /api/v1/ext/concerts/upcoming"
    );
}

#[tokio::test]
async fn le_routeur_est_monte_sous_son_nom() {
    let state = new_state();
    let app = app_avec_concerts_premium(&state).await;

    // Sans `instance_id`, le plugin répond sans jamais appeler le cloud : le
    // test ne dépend d'aucun réseau.
    let (statut, corps) = get_json(&app, "/api/v1/ext/concerts/upcoming").await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(corps["concerts"], serde_json::json!([]));
    assert_eq!(
        corps["code"], "concerts.no_instance_id",
        "un serveur sans instance_id doit le dire par un code, pas par une \
         liste vide muette"
    );
}

#[tokio::test]
async fn le_corps_ne_porte_aucune_phrase_anglaise() {
    let state = new_state();
    let app = app_avec_concerts_premium(&state).await;

    let (_, corps) = get_json(&app, "/api/v1/ext/concerts/upcoming").await;
    assert!(
        corps.get("error").is_none(),
        "le corps ne doit plus porter de champ `error` : l'ancien handler y \
         mettait une chaîne technique anglaise qu'une interface traduite en 11 \
         langues affichait telle quelle. Le contrat est un `code` stable."
    );
    let code = corps["code"].as_str().unwrap_or_default();
    assert!(
        code.starts_with("concerts."),
        "le code doit être préfixé par le domaine, trouvé : {code:?}"
    );
}

#[tokio::test]
async fn le_greffon_reste_hors_catalogue_tant_qu_aucun_ecran_ne_l_appelle() {
    use tune_core::plugin_sdk::TunePlugin;

    let state = new_state();
    let greffon = tune_concerts::ConcertsPlugin::new(tune_concerts::HostServices {
        backend: state.backend.clone(),
    });

    assert!(
        !greffon.default_enabled(),
        "le greffon doit être opt-in, comme dj, karaoke et bandcamp"
    );
    assert!(
        !greffon.catalogued(),
        "le greffon ne doit PAS être offert au catalogue tant qu'aucun écran \
         ne consomme ses routes : proposer « Installer » sur une fonction que \
         rien n'expose fait redémarrer l'utilisateur pour rien (#2090). \
         Rebrancher ce test le jour où l'écran existe."
    );
}

#[tokio::test]
async fn la_tache_periodique_s_arrete_avec_le_greffon() {
    use tune_core::plugin_sdk::{PluginContext, TunePlugin};

    use_scratch_plugin_data_dir();
    let state = new_state();
    let mut greffon = tune_concerts::ConcertsPlugin::new(tune_concerts::HostServices {
        backend: state.backend.clone(),
    });

    let ctx = PluginContext::new("http://127.0.0.1:0", std::env::temp_dir());
    greffon.setup(&ctx).await.expect("setup");

    // Le cœur ne gardait aucune poignée sur cette tâche : `tokio::spawn` et
    // plus rien. Un plugin qu'on arrête doit emporter sa tâche, sinon elle
    // survit à son propriétaire et continue d'appeler le cloud.
    greffon.teardown().await.expect("teardown");
}

/// ⚠️ LE PORTILLON PORTE SUR LES ROUTES, PAS SUR LE CHARGEMENT.
///
/// Le greffon se charge même sans Premium — sinon le gestionnaire ne pourrait
/// pas l'annoncer à qui ne l'a pas encore. C'est la route qui refuse, avec le
/// corps exact de `require_premium` : le client sait déjà le reconnaître comme
/// un refus d'offre et non comme une panne (`estRefusPremium`).
#[tokio::test]
async fn un_compte_gratuit_recoit_un_refus_d_offre_pas_une_panne() {
    let state = new_state();
    let app = app_avec_concerts(&state).await;

    let (statut, corps) = get_json(&app, "/api/v1/ext/concerts/upcoming").await;

    assert_eq!(
        statut,
        StatusCode::PAYMENT_REQUIRED,
        "un compte gratuit doit recevoir 402, pas une liste vide qui se lirait \
         « il n'y a aucun concert »"
    );
    assert_eq!(
        corps["error"], "premium_required",
        "le corps doit être celui de require_premium, sans quoi l'écran devrait \
         apprendre une deuxième forme de refus — donc en oublier une"
    );
    assert_eq!(corps["feature"], "Concerts");
}

/// Le module se déclare, pour que le gestionnaire montre le cadenas AVANT le
/// clic. Sans ça, l'utilisateur installe, redémarre, et n'obtient qu'un 402.
#[test]
fn le_greffon_declare_son_module() {
    use tune_core::license::Feature;
    use tune_core::plugin_sdk::TunePlugin;

    let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    let greffon = tune_concerts::ConcertsPlugin::new(tune_concerts::HostServices {
        backend: std::sync::Arc::new(db),
    });

    assert_eq!(greffon.required_feature(), Some(Feature::Concerts));
}
