//! #8481 — `GET`/`PUT /streaming/{service}/catalog-country` : le pays du
//! catalogue TIDAL choisi dans les Réglages, gardé en réglage et remis au
//! démarrage. Aucun appel à TIDAL ni à Qobuz : les routes ne lisent que l'état
//! du service.

use super::*;
use tune_core::db::sqlite::SqliteDb;
use tune_core::streaming::qobuz::QobuzService;
use tune_core::streaming::tidal::TidalService;

fn app() -> (Router, Arc<dyn DbBackend>, Arc<Mutex<ServiceRegistry>>) {
    let base = SqliteDb::open_in_memory().expect("sqlite en memoire");
    base.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&base).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(base);
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(TidalService::new()));
    registre.register(Box::new(QobuzService::new("app".into(), "secret".into())));
    let registre = Arc::new(Mutex::new(registre));
    let etat =
        StreamingHttpState::new(backend.clone(), registre.clone(), Arc::new(EventBus::new()));
    (
        Router::new().nest("/api/v1/streaming", router().with_state(etat)),
        backend,
        registre,
    )
}

async fn appeler(
    app: &Router,
    methode: &str,
    uri: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    use tower::ServiceExt;
    let mut req = axum::http::Request::builder().method(methode).uri(uri);
    let body = match corps {
        Some(c) => {
            req = req.header("content-type", "application/json");
            axum::body::Body::from(c.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let rep = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let statut = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

const URI: &str = "/api/v1/streaming/tidal/catalog-country";

#[tokio::test]
async fn choisir_pl_le_garde_et_le_rend_8481() {
    let (app, backend, _) = app();
    let (statut, corps) = appeler(&app, "GET", URI, None).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(corps["supported"], true);
    assert_eq!(corps["chosen_country"], Value::Null);

    let (statut, corps) = appeler(&app, "PUT", URI, Some(json!({"country": "pl"}))).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["chosen_country"], "PL");
    assert_eq!(corps["effective_country"], "PL");
    let settings = SettingsRepo::with_backend(backend.clone());
    assert_eq!(
        settings
            .get("streaming_tidal_catalog_country")
            .unwrap()
            .as_deref(),
        Some("PL")
    );

    // Retour au pays du compte : la ligne disparaît.
    let (statut, corps) = appeler(&app, "PUT", URI, Some(json!({"country": null}))).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(corps["chosen_country"], Value::Null);
    assert_eq!(
        settings.get("streaming_tidal_catalog_country").unwrap(),
        None
    );
}

#[tokio::test]
async fn un_code_invalide_est_refuse_8481() {
    let (app, backend, _) = app();
    let (statut, _) = appeler(&app, "PUT", URI, Some(json!({"country": "POL"}))).await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
    assert_eq!(
        SettingsRepo::with_backend(backend)
            .get("streaming_tidal_catalog_country")
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn qobuz_repond_non_pris_en_charge_8481() {
    let (app, _, _) = app();
    let uri = "/api/v1/streaming/qobuz/catalog-country";
    let (statut, corps) = appeler(&app, "GET", uri, None).await;
    assert_eq!(statut, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(corps["supported"], false);
    let (statut, _) = appeler(&app, "PUT", uri, Some(json!({"country": "PL"}))).await;
    assert_eq!(statut, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn le_choix_revient_au_demarrage_8481() {
    let (_, backend, registre) = app();
    SettingsRepo::with_backend(backend.clone())
        .set("streaming_tidal_catalog_country", "PL")
        .unwrap();
    let registre = registre.lock().await;
    registre.restore_all_tokens(&backend).await;
    let tidal = registre.get("tidal").unwrap();
    let pays = tidal.read().await.pays_du_catalogue().unwrap();
    assert_eq!(pays.choisi.as_deref(), Some("PL"));
    assert_eq!(pays.effectif, "PL");
}

#[tokio::test]
async fn un_code_bien_forme_mais_non_attribue_est_refuse_8481() {
    let (app, _, _) = app();
    for faux in ["XX", "PO", "ZZ"] {
        let (statut, corps) = appeler(&app, "PUT", URI, Some(json!({"country": faux}))).await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{faux}");
        assert!(
            corps["error"].as_str().unwrap_or("").contains("ISO 3166-1"),
            "message clair : {corps}"
        );
    }
}

#[tokio::test]
async fn changer_de_pays_purge_les_favoris_et_playlists_memorises_8481() {
    let (app, _, _) = app();
    memoriser_contenu_utilisateur("tidal", "favorites/tracks", json!([{"id": "1"}]));
    memoriser_contenu_utilisateur("tidal", "playlists", json!([{"id": "p"}]));
    assert!(contenu_utilisateur_frais("tidal", "favorites/tracks").is_some());
    let (statut, _) = appeler(&app, "PUT", URI, Some(json!({"country": "PL"}))).await;
    assert_eq!(statut, StatusCode::OK);
    assert!(contenu_utilisateur_frais("tidal", "favorites/tracks").is_none());
    assert!(contenu_utilisateur_frais("tidal", "playlists").is_none());
}
