//! #7470 : sous Docker, « Désinstaller » l'Assistant IA, greffon NATIF compilé
//! dans le binaire, redemandait un redémarrage puis le laissait en place. Le
//! nettoyage générique effaçait `plugin_<id>_enabled`, dont l'absence veut dire
//! « activé » : après redémarrage le greffon se rechargeait. Désinstaller un
//! greffon natif actif d'office le désactive durablement, sans toucher un
//! fichier, et la fiche repasse en « disponible ».
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::plugin_sdk::{AvailablePluginInfo, PluginInfo};
use tune_server::state::AppState;

async fn call(app: &axum::Router, method: &str, path: &str) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(if method == "POST" {
            Body::from("{}")
        } else {
            Body::empty()
        })
        .unwrap();
    req.headers_mut()
        .insert("accept-language", HeaderValue::from_static("fr"));
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn fiche(liste: &Value, nom: &str) -> Option<Value> {
    liste
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == nom)
        .cloned()
}

fn info(nom: &str, opt_in: bool) -> PluginInfo {
    PluginInfo {
        name: nom.to_string(),
        display_name: nom.to_string(),
        version: "1".into(),
        description: "d".into(),
        enabled: true,
        config_schema: serde_json::json!({}),
        required_feature: None,
        opt_in,
        web_theme: None,
    }
}

/// Contre-épreuve : retirer `natif_actif_par_defaut` de `delete_plugin` fait
/// rougir ce test, l'état persistant redevenant « activé » (clé absente).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desinstaller_un_greffon_natif_le_desactive_durablement() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    state
        .plugin_info
        .set(vec![info("assistant-ia", false), info("dj", true)])
        .unwrap();
    state
        .plugin_names
        .set(vec!["assistant-ia".into(), "dj".into()])
        .unwrap();
    let app = tune_server::routes::router(state);

    let (_, liste) = call(&app, "GET", "/api/v1/plugins").await;
    assert_eq!(fiche(&liste, "assistant-ia").unwrap()["installed"], true);

    let (statut, corps) = call(&app, "DELETE", "/api/v1/plugins/assistant-ia").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["status"], "uninstalled");
    assert_eq!(corps["disabled"], true);
    // Il tourne encore : un redémarrage est dû, et le message le dit sans
    // parler de fichier.
    assert_eq!(corps["restart_required"], true);
    assert!(corps["message"].as_str().unwrap().contains("intégré"));

    // Ce que lit la porte du démarrage : désactivé, pas « absent = activé ».
    assert_eq!(
        settings
            .get("plugin_assistant-ia_enabled")
            .unwrap()
            .as_deref(),
        Some("false")
    );
    assert_eq!(
        settings
            .get("plugin_assistant-ia_hidden")
            .unwrap()
            .as_deref(),
        Some("true")
    );

    // Par le marché aussi (même nom en `slug`), sans 404.
    let (statut, corps) = call(
        &app,
        "POST",
        "/api/v1/marketplace/plugins/assistant-ia/uninstall",
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["disabled"], true);

    // Un greffon à installer à la demande garde son comportement : ni masqué,
    // ni désactivé par ce chemin.
    let (statut, _) = call(&app, "DELETE", "/api/v1/plugins/dj").await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(settings.get("plugin_dj_hidden").unwrap(), None);
}

/// Après le redémarrage, le greffon dormant est listé « disponible » (pas
/// « installé » : le bouton Désinstaller muet), et Installer le rétablit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apres_redemarrage_la_fiche_est_disponible_et_installer_la_retablit() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    state
        .plugin_available
        .set(vec![AvailablePluginInfo {
            name: "assistant-ia".into(),
            display_name: "assistant-ia".into(),
            version: "1".into(),
            description: "d".into(),
            config_schema: serde_json::json!({}),
            opt_in: false,
            required_feature: None,
        }])
        .unwrap();
    state.plugin_names.set(vec!["assistant-ia".into()]).unwrap();
    // L'état laissé par la désinstallation, tel qu'il survit au redémarrage.
    settings
        .set("plugin_assistant-ia_enabled", "false")
        .unwrap();
    settings.set("plugin_assistant-ia_hidden", "true").unwrap();
    let app = tune_server::routes::router(state);

    let (_, liste) = call(&app, "GET", "/api/v1/plugins").await;
    let f = fiche(&liste, "assistant-ia").expect("toujours listé");
    assert_eq!(f["installed"], false, "{f}");
    assert_eq!(f["enabled"], false);

    // Désinstaller un greffon déjà dormant ne plante pas et ne demande rien.
    let (statut, corps) = call(&app, "DELETE", "/api/v1/plugins/assistant-ia").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["restart_required"], false);

    let (statut, _) = call(&app, "POST", "/api/v1/plugins/assistant-ia/install").await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(settings.get("plugin_assistant-ia_hidden").unwrap(), None);
    assert_eq!(
        settings
            .get("plugin_assistant-ia_enabled")
            .unwrap()
            .as_deref(),
        Some("true")
    );
}
