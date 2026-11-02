//! #7195 — « Ma collection physique » de bout en bout dans le serveur, sur
//! deux démarrages de la MÊME base : proposé au catalogue, installé par la
//! route existante, puis actif sous `/api/v1/ext/collection-physique`.
//!
//! Le jeton personnel Discogs est saisi par la route des Clés d'API (scellé
//! dans le coffre) : le greffon dit qu'il existe, AUCUNE de ses réponses ne
//! le rend. Aucun appel réseau : rien ici ne lance de synchronisation (les
//! appels Discogs sont essayés dans le crate du greffon, sur réponses
//! enregistrées).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::use_scratch_plugin_data_dir;
use tune_server::state::AppState;

const JETON: &str = "PERSO-discogs-7195-ne-doit-jamais-sortir";

fn demarrer(base: &std::path::Path) -> AppState {
    AppState::new(base.to_str().unwrap(), 0, Default::default()).unwrap()
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Option<Value>,
) -> (StatusCode, String) {
    let req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("content-type", "application/json")
        .body(match (methode, corps) {
            (_, Some(c)) => Body::from(c.to_string()),
            ("POST", None) => Body::from("{}"),
            _ => Body::empty(),
        })
        .unwrap();
    let rep = app.clone().oneshot(req).await.unwrap();
    let code = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    (code, String::from_utf8_lossy(&octets).into_owned())
}

fn json_de(s: &str) -> Value {
    serde_json::from_str(s).unwrap_or(Value::Null)
}

async fn fiche(app: &axum::Router) -> Option<Value> {
    let (code, liste) = appel(app, "GET", "/api/v1/plugins", None).await;
    assert_eq!(code, StatusCode::OK);
    json_de(&liste)
        .as_array()
        .expect("la liste des greffons est un tableau")
        .iter()
        .find(|p| p["name"] == "collection-physique")
        .cloned()
}

#[tokio::test]
async fn la_collection_physique_est_proposee_s_installe_et_ne_rend_jamais_le_jeton() {
    use_scratch_plugin_data_dir();
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");

    // ── 1er démarrage : proposé, dormant.
    let etat = demarrer(&base);
    let routeurs = tune_server::plugins::init(&etat, "http://127.0.0.1:0", vec![]).await;
    assert!(
        !routeurs.iter().any(|(n, _)| n == "collection-physique"),
        "opt-in : il n'appelle pas Discogs sans qu'on l'ait installé"
    );
    let app = tune_server::routes::router_with_plugins(etat.clone(), routeurs);
    let f = fiche(&app)
        .await
        .expect("« collection-physique » doit être PROPOSÉ par le gestionnaire (#7195)");
    assert_eq!(f["installed"], false, "{f}");
    assert_eq!(f["premium"], false, "gratuit — {f}");
    assert_eq!(f["compatible"], true, "{f}");
    let (code, rep) = appel(
        &app,
        "POST",
        "/api/v1/plugins/collection-physique/install",
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{rep}");
    drop(app);
    drop(etat);

    // ── 2e démarrage : actif.
    let etat = demarrer(&base);
    let routeurs = tune_server::plugins::init(&etat, "http://127.0.0.1:0", vec![]).await;
    let app = tune_server::routes::router_with_plugins(etat.clone(), routeurs);
    let f = fiche(&app).await.expect("un greffon qui tourne est listé");
    assert_eq!(f["enabled"], true, "{f}");

    let racine = "/api/v1/ext/collection-physique";
    let (code, r) = appel(&app, "GET", &format!("{racine}/reglages"), None).await;
    assert_eq!(code, StatusCode::OK, "{r}");
    assert_eq!(json_de(&r)["jeton"], "aucun", "{r}");
    let (_, c) = appel(&app, "GET", &format!("{racine}/collection"), None).await;
    assert_eq!(json_de(&c)["statut"], "aucun_compte", "{c}");
    assert_eq!(
        json_de(&c)["attribution"]["texte"],
        "Data provided by Discogs"
    );

    // Le jeton personnel, par Réglages › Clés d'API : scellé.
    let (code, r) = appel(
        &app,
        "PUT",
        "/api/v1/system/api-keys/discogs",
        Some(json!({"value": JETON})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{r}");

    let (code, r) = appel(
        &app,
        "PUT",
        &format!("{racine}/reglages"),
        Some(json!({"nom_utilisateur": "exemple"})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{r}");
    assert_eq!(
        json_de(&r),
        json!({"nom_utilisateur": "exemple", "jeton": "personnel"}),
        "le greffon dit QUEL jeton existe, pas lequel"
    );
    for route in ["reglages", "etat", "collection"] {
        let (code, corps) = appel(&app, "GET", &format!("{racine}/{route}"), None).await;
        assert_eq!(code, StatusCode::OK, "{route} : {corps}");
        assert!(
            !corps.contains(JETON) && !corps.contains("ne-doit-jamais"),
            "le jeton ne sort par aucune route du greffon — {route} : {corps}"
        );
    }
    let (_, c) = appel(&app, "GET", &format!("{racine}/collection"), None).await;
    assert_eq!(
        json_de(&c)["statut"],
        "a_synchroniser",
        "compte retenu, liste pas encore lue — {c}"
    );
}
