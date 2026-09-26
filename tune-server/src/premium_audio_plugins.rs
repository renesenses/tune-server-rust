//! Bridges the standalone SDK plugins to Tune's installed-plugin catalogue.
//! The implementations depend only on the SDK; DB, routes and licence are host
//! adapters. Existing screens/API remain valid during source migration.
//!
//! L'égaliseur est un greffon facultatif (v0.9.156) : son état d'installation,
//! la proposition d'installation et la présence d'une configuration existante
//! sont servis au client par [`state_fields`], à la racine des fiches du
//! catalogue et de [`descriptor_with_state`].
use crate::state::AppState;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use tune_core::audio::premium_plugins;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::plugin_sdk::{PluginContext, PluginLoader, TunePlugin};
struct PremiumAudio {
    id: &'static str,
    backend: Arc<dyn DbBackend>,
}
impl PremiumAudio {
    fn settings(&self) -> SettingsRepo {
        SettingsRepo::with_backend(self.backend.clone())
    }
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
            "crossfeed" => "Crossfeed casque : intensité, retard et ombre de la tête, à chaud",
            "converter" => "Convertisseur audio : codecs de l'hôte, métadonnées et exports",
            _ => "Dé-ploc : silence en tête/queue et passages par zéro, FLAC/WAV",
        }
    }
    fn default_enabled(&self) -> bool {
        false
    }
    fn config_schema(&self) -> Value {
        descriptor_with_state(&self.settings(), self.id)
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
        // L'état (installé, proposé, configuration existante) se lit à chaque
        // requête : il change quand l'utilisateur installe depuis le catalogue.
        let id = self.id;
        let backend = self.backend.clone();
        ctx.register_router(axum::Router::new().route(
            "/",
            axum::routing::get(move || {
                let info = descriptor_with_state(&SettingsRepo::with_backend(backend.clone()), id);
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
            // `/crossfeed` : les préréglages nommés (#4684).
            vec!["/api/v1/zones/{zone}/dsp", "/api/v1/crossfeed"],
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
    json!({"sdk": {"major":0,"minor":1}, "kind":kind, "premium":premium_plugins::requires_premium(id), "entitlement":entitlement, "configuration_version":1,"native_loaded":tune_plugin_native::provider(id).is_some(),"activation_error":tune_plugin_native::failure(id),
        "ui":{"view":view,"zone_scoped":kind=="dsp","endpoints":endpoints,"levels_event":"playback.audio_levels","levels_premium":false},
        "lifecycle":{"disable":"pending_restart","uninstall":"preserve_configuration","running_jobs":"finish_or_explicit_cancel"}})
}
/// Les trois champs d'état d'un greffon audio premium, lus en base :
/// `installed`, `install_proposed`, `existing_configuration`. `None` pour tout
/// identifiant hors des quatre. C'est le contrat du client web pour la fiche
/// `GET /api/v1/plugins/{id}` (à la racine) et pour [`descriptor_with_state`].
pub fn state_fields(settings: &SettingsRepo, id: &str) -> Option<serde_json::Map<String, Value>> {
    if !premium_plugins::contains(id) {
        return None;
    }
    let mut fields = serde_json::Map::new();
    fields.insert(
        "installed".into(),
        Value::Bool(premium_plugins::installed(settings, id)),
    );
    fields.insert(
        "install_proposed".into(),
        Value::Bool(premium_plugins::install_proposed(settings, id)),
    );
    fields.insert(
        "existing_configuration".into(),
        Value::Bool(premium_plugins::existing_configuration(settings, id)),
    );
    Some(fields)
}
/// Pose les champs de [`state_fields`] à la racine d'une fiche de catalogue
/// (objet JSON). Sans effet pour un greffon qui n'est pas audio premium.
pub fn annotate(settings: &SettingsRepo, id: &str, card: &mut Value) {
    if let (Some(fields), Some(object)) = (state_fields(settings, id), card.as_object_mut()) {
        object.extend(fields);
    }
}
/// [`descriptor`] complété par l'état en base (voir [`state_fields`]).
pub fn descriptor_with_state(settings: &SettingsRepo, id: &str) -> Value {
    let mut info = descriptor(id);
    annotate(settings, id, &mut info);
    info
}
/// La migration tourne ICI, avant `native_audio::load_installed` et avant
/// l'enregistrement des greffons — donc avant `setup_all`, avant le routeur
/// HTTP et avant toute reprise de lecture (`bootstrap::run` : « greffons »
/// précède la liaison du port et l'auto-reprise des zones locales).
pub async fn register(loader: &PluginLoader, state: &AppState) {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Err(e) =
        premium_plugins::migrate_for_account(&settings, state.license.is_premium().await)
    {
        tracing::error!(error=%e, "premium_audio_migration_failed");
        return;
    }
    crate::native_audio::load_installed(&settings);
    for id in premium_plugins::IDS {
        loader
            .register(Box::new(PremiumAudio {
                id,
                backend: state.backend.clone(),
            }))
            .await;
    }
}
pub fn require_installed(state: &AppState, id: &str) -> Result<(), axum::response::Response> {
    use axum::response::IntoResponse;
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let actif = if tune_core::audio::premium_plugins::contains(id) {
        tune_core::audio::premium_plugins::enabled(&settings, id)
    } else {
        tune_core::audio::natifs_tiers::actif(&settings, id)
    };
    if actif {
        Ok(())
    } else {
        Err((axum::http::StatusCode::CONFLICT,axum::Json(json!({"error":"plugin_unavailable","plugin":id,"message":"Ce plugin doit être installé et activé."}))).into_response())
    }
}

pub async fn require_entitlement(
    state: &AppState,
    id: &str,
) -> Result<(), axum::response::Response> {
    // Les quatre emplacements, et tout greffon natif tiers présent ou chargé :
    // ce dernier exige toujours le Premium (`native_audio::feature`).
    if tune_core::audio::premium_plugins::contains(id)
        || crate::native_audio::is_third_party(id)
        || tune_plugin_native::provider(id).is_some()
    {
        crate::premium_guard::require_premium(&state.license, crate::native_audio::feature(id))
            .await
    } else {
        Ok(())
    }
}

/// Bandeau « Réinstaller » (#4861) : les greffons payants absents, pour un
/// compte qui a le droit de les installer, que l'utilisateur n'a pas refusés.
/// Le droit est jugé par la même porte que la route d'installation
/// (`check_feature` du droit de chaque greffon) : un compte Free ne reçoit
/// jamais rien. « Réinstaller » passe par `POST /plugins/{id}/install`.
async fn reinstall_suggestion_ids(state: &AppState) -> Result<Vec<&'static str>, String> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut ids = Vec::new();
    for id in premium_plugins::reinstall_candidates(&settings)? {
        if state
            .license
            .check_feature(crate::native_audio::feature(id))
            .await
        {
            ids.push(id);
        }
    }
    Ok(ids)
}

fn reinstall_suggestion_failed(error: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    tracing::error!(%error, "premium_audio_reinstall_suggestion_failed");
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(json!({"error":"reinstall_suggestion_failed","detail":error})),
    )
        .into_response()
}

/// `GET /api/v1/plugins/premium-audio/reinstall-suggestion` → `{"ids": [...]}`.
pub async fn reinstall_suggestion(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match reinstall_suggestion_ids(&state).await {
        Ok(ids) => axum::Json(json!({ "ids": ids })).into_response(),
        Err(e) => reinstall_suggestion_failed(e),
    }
}

#[derive(serde::Deserialize)]
pub struct DismissRequest {
    ids: Vec<String>,
}

/// `POST /api/v1/plugins/premium-audio/reinstall-suggestion/dismiss`
/// `{"ids": [...]}` : « Ignorer ». Le refus est mémorisé pour ces greffons ;
/// la réponse porte la suggestion relue (`{"ids": [...]}`).
pub async fn dismiss_reinstall_suggestion(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::Json(body): axum::Json<DismissRequest>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Err(e) = premium_plugins::dismiss_reinstall(&settings, &body.ids) {
        return reinstall_suggestion_failed(e);
    }
    match reinstall_suggestion_ids(&state).await {
        Ok(ids) => axum::Json(json!({ "ids": ids })).into_response(),
        Err(e) => reinstall_suggestion_failed(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn settings_en_memoire() -> SettingsRepo {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        SettingsRepo::with_backend(Arc::new(db))
    }

    /// Les greffons rangent leur état sous `TUNE_PLUGINS_DATA_DIR` ; sans lui,
    /// `setup_all` créerait `plugins/data` dans l'arbre de travail.
    /// Fait supprimer `chemin` quand le processus se termine — même motif que
    /// `tests/plugin_contracts.rs` : `Drop` ne s'exécute pas sur un `static`,
    /// `atexit` se déclenche à la sortie de `libtest`, suite réussie ou non.
    #[cfg(unix)]
    fn menage_a_la_sortie_du_processus(chemin: std::path::PathBuf) {
        static CHEMIN: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        extern "C" fn balayer() {
            if let Some(chemin) = CHEMIN.get() {
                let _ = std::fs::remove_dir_all(chemin);
            }
        }
        if CHEMIN.set(chemin).is_ok() {
            // Safety: `atexit` n'est appelé qu'une fois (`OnceLock::set` ne rend
            // `Ok` qu'au premier passage) et `balayer` ne lit que `CHEMIN`.
            unsafe {
                libc::atexit(balayer);
            }
        }
    }
    #[cfg(not(unix))]
    fn menage_a_la_sortie_du_processus(_chemin: std::path::PathBuf) {}

    // Le dossier doit survivre à tous les tests du binaire, donc à toute portée.
    // tmp-autorise: repris par `menage_a_la_sortie_du_processus`, pas abandonné.
    static DOSSIER_DE_DONNEES: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    fn dossier_de_donnees_jetable() {
        DOSSIER_DE_DONNEES.get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            menage_a_la_sortie_du_processus(dir.path().to_path_buf());
            // Safety : seule écriture de ces variables dans ce processus, faite
            // avant la construction de l'AppState qui les lit (même motif que
            // tests/plugin_contracts.rs).
            unsafe {
                std::env::set_var("TUNE_PLUGINS_DATA_DIR", dir.path());
                std::env::set_var("TUNE_AUDIO_PLUGINS_DIR", dir.path().join("audio"));
            }
            dir
        });
    }

    async fn appel(app: &axum::Router, methode: &str, chemin: &str) -> (StatusCode, Value) {
        appel_avec_corps(app, methode, chemin, json!({})).await
    }

    async fn appel_avec_corps(
        app: &axum::Router,
        methode: &str,
        chemin: &str,
        corps: Value,
    ) -> (StatusCode, Value) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(methode)
                    .uri(chemin)
                    .header("content-type", "application/json")
                    .body(Body::from(corps.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[test]
    fn descriptor_with_state_porte_les_trois_champs_pour_les_quatre_greffons() {
        let s = settings_en_memoire();
        s.set("zone_1_eq_profile", "existing-profile").unwrap();
        premium_plugins::migrate_for_account(&s, false).unwrap();
        for id in premium_plugins::IDS {
            let info = descriptor_with_state(&s, id);
            assert!(info["installed"].is_boolean(), "{id}: {info}");
            assert!(info["install_proposed"].is_boolean(), "{id}: {info}");
            assert!(info["existing_configuration"].is_boolean(), "{id}: {info}");
            // Le descripteur d'origine reste entier.
            assert_eq!(info["entitlement"], descriptor(id)["entitlement"]);
        }
        let eq = descriptor_with_state(&s, "equalizer");
        assert_eq!(eq["installed"], false);
        assert_eq!(eq["install_proposed"], true);
        assert_eq!(eq["existing_configuration"], true);
        assert!(descriptor("equalizer").get("install_proposed").is_none());
        assert!(state_fields(&s, "bandcamp").is_none());
    }

    #[test]
    fn annotate_ne_touche_pas_une_fiche_hors_audio_premium() {
        let s = settings_en_memoire();
        let mut fiche = json!({"name":"dj","installed":true});
        annotate(&s, "dj", &mut fiche);
        assert_eq!(fiche, json!({"name":"dj","installed":true}));
    }

    /// Le contrat du client web : `GET /api/v1/plugins/equalizer` porte
    /// `installed`, `install_proposed`, `existing_configuration` à la RACINE,
    /// et l'installation en un geste (`POST …/install`) éteint la proposition.
    #[tokio::test]
    async fn la_fiche_de_l_egaliseur_porte_l_etat_a_la_racine_et_l_installation_l_eteint() {
        dossier_de_donnees_jetable();
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let settings = SettingsRepo::with_backend(state.backend.clone());
        settings
            .set(
                "zone_1_eq_profile",
                r#"{"enabled":true,"bass_gain_db":4.0}"#,
            )
            .unwrap();
        let routers = crate::plugins::init(&state, "http://127.0.0.1:0", vec![]).await;
        assert!(
            !premium_plugins::enabled(&settings, "equalizer"),
            "l'égaliseur tourne d'office après le démarrage"
        );
        let app = crate::routes::router_with_plugins(state.clone(), routers);

        let (status, fiche) = appel(&app, "GET", "/api/v1/plugins/equalizer").await;
        assert_eq!(status, StatusCode::OK, "{fiche}");
        assert_eq!(fiche["installed"], false, "{fiche}");
        assert_eq!(fiche["install_proposed"], true, "{fiche}");
        assert_eq!(fiche["existing_configuration"], true, "{fiche}");

        let (_, catalogue) = appel(&app, "GET", "/api/v1/plugins").await;
        let eq = catalogue
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "equalizer")
            .expect("l'égaliseur doit rester visible dans le catalogue");
        assert_eq!(eq["installed"], false, "{eq}");
        assert_eq!(eq["install_proposed"], true, "{eq}");

        let (status, reponse) = appel(&app, "POST", "/api/v1/plugins/equalizer/install").await;
        assert_eq!(status, StatusCode::OK, "{reponse}");
        let (_, fiche) = appel(&app, "GET", "/api/v1/plugins/equalizer").await;
        assert_eq!(fiche["installed"], true, "{fiche}");
        assert_eq!(fiche["install_proposed"], false, "{fiche}");
        assert_eq!(fiche["existing_configuration"], true, "{fiche}");
        assert!(premium_plugins::enabled(&settings, "equalizer"));
        assert_eq!(
            settings.get("zone_1_eq_profile").unwrap().as_deref(),
            Some(r#"{"enabled":true,"bass_gain_db":4.0}"#),
            "le réglage doit rester intact"
        );
    }

    /// #4861, témoin de bout en bout : premier démarrage vu comme Free, puis
    /// l'utilisateur retire deux greffons payants par les routes réelles
    /// (désinstallation native, désinstallation du catalogue) et en désactive
    /// un troisième ; au retour du Premium puis au démarrage suivant, ses
    /// choix tiennent. Un greffon qu'il n'a pas touché revient installé et
    /// actif.
    #[tokio::test]
    async fn temoin_4861_desinstallation_explicite_respectee_au_retour_du_premium() {
        dossier_de_donnees_jetable();
        for desactive in [false, true] {
            let state = AppState::new(":memory:", 0, Default::default()).unwrap();
            let settings = SettingsRepo::with_backend(state.backend.clone());
            let routers = crate::plugins::init(&state, "http://127.0.0.1:0", vec![]).await;
            assert!(!state.license.is_premium().await);
            for id in ["crossfeed", "converter", "declick"] {
                assert!(
                    !premium_plugins::enabled(&settings, id),
                    "{id} actif en Free"
                );
            }
            let app = crate::routes::router_with_plugins(state.clone(), routers);
            let (status, reponse) =
                appel(&app, "POST", "/api/v1/audio-plugins/crossfeed/uninstall").await;
            assert_eq!(status, StatusCode::OK, "{reponse}");
            let (status, reponse) = appel(&app, "DELETE", "/api/v1/plugins/converter").await;
            assert_eq!(status, StatusCode::OK, "{reponse}");
            if desactive {
                let (status, reponse) =
                    appel(&app, "POST", "/api/v1/plugins/declick/disable").await;
                assert_eq!(status, StatusCode::OK, "{reponse}");
            }

            // Retour du Premium par la licence, puis démarrage suivant.
            state
                .license
                .update_from_server(tune_core::license::Tier::Premium, None)
                .await;
            premium_plugins::migrate_for_account(&settings, state.license.is_premium().await)
                .unwrap();

            assert!(
                !premium_plugins::installed(&settings, "crossfeed"),
                "désinstallation native annulée"
            );
            assert!(!premium_plugins::enabled(&settings, "crossfeed"));
            assert!(
                !premium_plugins::installed(&settings, "converter"),
                "désinstallation du catalogue annulée"
            );
            assert!(!premium_plugins::enabled(&settings, "converter"));
            assert_eq!(
                premium_plugins::enabled(&settings, "declick"),
                !desactive,
                "declick (désactivé={desactive})"
            );
        }
    }

    const SUGGESTION: &str = "/api/v1/plugins/premium-audio/reinstall-suggestion";
    const IGNORER: &str = "/api/v1/plugins/premium-audio/reinstall-suggestion/dismiss";

    /// Une installation touchée par l'ancienne migration (#4861) : les trois
    /// greffons payants écrits `false`/`false`, sans retenue — l'état exact
    /// qu'écrit aussi la désinstallation native. Le compte est Premium si
    /// `premium`.
    async fn installation_touchee(premium: bool) -> (AppState, SettingsRepo, axum::Router) {
        dossier_de_donnees_jetable();
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let settings = SettingsRepo::with_backend(state.backend.clone());
        let routers = crate::plugins::init(&state, "http://127.0.0.1:0", vec![]).await;
        if premium {
            state
                .license
                .update_from_server(tune_core::license::Tier::Premium, None)
                .await;
        }
        for id in ["crossfeed", "converter", "declick"] {
            settings
                .set(&format!("plugin_{id}_installed"), "false")
                .unwrap();
            settings
                .set(&format!("plugin_{id}_enabled"), "false")
                .unwrap();
            settings
                .delete(&format!("premium_audio_plugins_withheld_{id}"))
                .unwrap();
        }
        let app = crate::routes::router_with_plugins(state.clone(), routers);
        (state, settings, app)
    }

    async fn proposes(app: &axum::Router) -> Value {
        let (status, reponse) = appel(app, "GET", SUGGESTION).await;
        assert_eq!(status, StatusCode::OK, "{reponse}");
        reponse["ids"].clone()
    }

    /// Témoin 1 : Premium sans les trois greffons payants → les trois proposés,
    /// jamais l'égaliseur (gratuit, seulement proposé par le catalogue).
    #[tokio::test]
    async fn temoin_4861_bandeau_premium_sans_les_trois_greffons() {
        let (_state, _settings, app) = installation_touchee(true).await;
        assert_eq!(
            proposes(&app).await,
            json!(["crossfeed", "converter", "declick"])
        );
    }

    /// Témoin 2 : un compte Free ne reçoit jamais rien.
    #[tokio::test]
    async fn temoin_4861_bandeau_rien_pour_un_compte_free() {
        let (state, _settings, app) = installation_touchee(false).await;
        assert!(!state.license.is_premium().await);
        assert_eq!(proposes(&app).await, json!([]));
    }

    /// Témoin 3 : « Ignorer » est mémorisé, greffon par greffon ; une
    /// réinstallation puis une désinstallation ultérieures ne rouvrent pas le
    /// bandeau.
    #[tokio::test]
    async fn temoin_4861_bandeau_ignorer_est_memorise() {
        let (_state, settings, app) = installation_touchee(true).await;
        let (status, reponse) =
            appel_avec_corps(&app, "POST", IGNORER, json!({"ids": ["crossfeed"]})).await;
        assert_eq!(status, StatusCode::OK, "{reponse}");
        assert_eq!(reponse["ids"], json!(["converter", "declick"]));
        assert_eq!(proposes(&app).await, json!(["converter", "declick"]));

        let (status, reponse) = appel_avec_corps(
            &app,
            "POST",
            IGNORER,
            json!({"ids": ["converter", "declick", "equalizer", "bandcamp"]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{reponse}");
        assert_eq!(reponse["ids"], json!([]));
        assert_eq!(proposes(&app).await, json!([]));

        let (status, reponse) = appel(&app, "POST", "/api/v1/plugins/crossfeed/install").await;
        assert_eq!(status, StatusCode::OK, "{reponse}");
        let (status, reponse) = appel(&app, "DELETE", "/api/v1/plugins/crossfeed").await;
        assert_eq!(status, StatusCode::OK, "{reponse}");
        assert!(!premium_plugins::installed(&settings, "crossfeed"));
        assert_eq!(
            proposes(&app).await,
            json!([]),
            "le bandeau refusé est revenu"
        );
    }

    /// Témoin 4 : un greffon installé n'est pas proposé.
    #[tokio::test]
    async fn temoin_4861_bandeau_greffon_installe_non_propose() {
        let (_state, settings, app) = installation_touchee(true).await;
        settings.set("plugin_converter_installed", "true").unwrap();
        settings.set("plugin_converter_enabled", "true").unwrap();
        assert_eq!(proposes(&app).await, json!(["crossfeed", "declick"]));
    }

    /// Témoin 5 : « Réinstaller » par la route d'installation EXISTANTE rend le
    /// greffon installé ET actif, et il sort du bandeau.
    #[tokio::test]
    async fn temoin_4861_reinstaller_par_la_route_existante() {
        let (_state, settings, app) = installation_touchee(true).await;
        assert_eq!(
            proposes(&app).await,
            json!(["crossfeed", "converter", "declick"])
        );
        for id in ["crossfeed", "converter", "declick"] {
            let (status, reponse) =
                appel(&app, "POST", &format!("/api/v1/plugins/{id}/install")).await;
            assert_eq!(status, StatusCode::OK, "{id}: {reponse}");
            assert!(
                premium_plugins::installed(&settings, id),
                "{id} non installé"
            );
            assert!(premium_plugins::enabled(&settings, id), "{id} inactif");
        }
        assert_eq!(proposes(&app).await, json!([]));
    }

    /// Greffon natif tiers : le droit qui l'ouvre est un droit Premium, jamais
    /// le droit gratuit de l'égaliseur, quel que soit son identifiant.
    #[tokio::test]
    async fn greffon_natif_tiers_le_droit_est_premium() {
        let droit = crate::native_audio::feature("greffon-tiers");
        assert_ne!(droit, tune_core::license::Feature::DspEq);
        assert!(
            tune_core::license::Feature::all_premium().contains(&droit),
            "{droit:?} hors des droits Premium"
        );
        assert_eq!(
            crate::native_audio::feature("equalizer"),
            tune_core::license::Feature::DspEq
        );
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        assert!(!state.license.check_feature(droit).await, "accordé en Free");
    }

    /// Greffon natif tiers, par les routes réelles : l'installation, le
    /// catalogue, les réglages de zone et les profils exigent le Premium ; un
    /// identifiant déjà porté par un greffon compilé est refusé ; en Premium,
    /// l'installation n'est plus arrêtée que par la signature.
    #[tokio::test]
    async fn greffon_natif_tiers_routes_gardees_par_le_premium() {
        dossier_de_donnees_jetable();
        let tiers = "greffon-tiers-routes";
        let dossier = crate::native_audio::root().join(tiers).join("versions");
        std::fs::create_dir_all(&dossier).unwrap();
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let settings = SettingsRepo::with_backend(state.backend.clone());
        let routers = crate::plugins::init(&state, "http://127.0.0.1:0", vec![]).await;
        let app = crate::routes::router_with_plugins(state.clone(), routers);
        assert!(!state.license.is_premium().await);

        let (status, reponse) = appel(
            &app,
            "POST",
            &format!("/api/v1/audio-plugins/{tiers}/install"),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::PAYMENT_REQUIRED,
            "install Free : {reponse}"
        );
        let (status, reponse) =
            appel(&app, "POST", &format!("/api/v1/plugins/{tiers}/enable")).await;
        assert_eq!(
            status,
            StatusCode::PAYMENT_REQUIRED,
            "enable Free : {reponse}"
        );
        assert!(
            settings
                .get(&format!("plugin_{tiers}_enabled"))
                .unwrap()
                .is_none(),
            "drapeau écrit malgré le refus"
        );
        let (status, reponse) = appel_avec_corps(
            &app,
            "PUT",
            &format!("/api/v1/audio-plugins/{tiers}/zones/1"),
            json!({"enabled": true}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::PAYMENT_REQUIRED,
            "zone Free : {reponse}"
        );
        let (status, reponse) = appel_avec_corps(
            &app,
            "POST",
            &format!("/api/v1/audio-plugins/{tiers}/profiles"),
            json!({"name": "Salon", "settings": {"enabled": true}}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::PAYMENT_REQUIRED,
            "profil Free : {reponse}"
        );
        // Lire reste libre.
        let (status, reponse) = appel(
            &app,
            "GET",
            &format!("/api/v1/audio-plugins/{tiers}/profiles"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{reponse}");
        assert_eq!(reponse["profiles"], json!([]));
        // Un identifiant inconnu n'a ni réglage ni profil.
        let (status, _) = appel(
            &app,
            "GET",
            "/api/v1/audio-plugins/absent-du-disque/profiles",
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        state
            .license
            .update_from_server(tune_core::license::Tier::Premium, None)
            .await;
        let (status, reponse) = appel(
            &app,
            "POST",
            &format!("/api/v1/audio-plugins/{tiers}/install"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{reponse}");
        assert_eq!(reponse["detail"], "missing detached signature", "{reponse}");
        // Premium mais greffon non chargé : pas de réglage de zone.
        let (status, reponse) = appel_avec_corps(
            &app,
            "PUT",
            &format!("/api/v1/audio-plugins/{tiers}/zones/1"),
            json!({"enabled": true}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{reponse}");
        let (status, reponse) = appel(&app, "GET", "/api/v1/audio-plugins").await;
        assert_eq!(status, StatusCode::OK, "état : {status} {reponse}");
        assert!(
            reponse["plugins"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["id"] == tiers && p["third_party"] == true),
            "greffon tiers absent de l'état : {reponse}"
        );
    }

    /// Un greffon natif tiers ne peut pas prendre le nom d'un greffon compilé
    /// dans ce serveur : ils partageraient les drapeaux `plugin_{id}_*`. Le jeu
    /// registré est posé à la main, pour ne dépendre d'aucune feature.
    #[tokio::test]
    async fn greffon_natif_tiers_refuse_le_nom_d_un_greffon_compile() {
        dossier_de_donnees_jetable();
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        state
            .plugin_names
            .set(vec!["greffon-compile-essai".to_string()])
            .unwrap();
        state
            .license
            .update_from_server(tune_core::license::Tier::Premium, None)
            .await;
        let app = crate::routes::router_with_plugins(state.clone(), vec![]);
        let (status, reponse) = appel(
            &app,
            "POST",
            "/api/v1/audio-plugins/greffon-compile-essai/install",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{reponse}");
        assert_eq!(
            reponse["detail"], "plugin id already used by another plugin",
            "{reponse}"
        );
        // Un nom libre n'est arrêté que par la signature.
        let (_, reponse) = appel(&app, "POST", "/api/v1/audio-plugins/greffon-libre/install").await;
        assert_eq!(reponse["detail"], "missing detached signature", "{reponse}");
    }
}
