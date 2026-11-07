//! Import actuel par la vraie route HTTP, état isolé et aucun appareil.
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::{settings_repo::SettingsRepo, zone_repo::ZoneRepo};

async fn importer(state: &crate::state::AppState, body: Value, apercu: bool) -> Value {
    let app = crate::routes::router(state.clone());
    let uri = if apercu {
        "/api/v1/system/config/import?dry_run=true"
    } else {
        "/api/v1/system/config/import"
    };
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("Content-Type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(status, StatusCode::OK, "import : {body}");
    body
}
fn fichier(zones: Value) -> Value {
    json!({"format":"tune-config","format_version":2,"settings":{},"zones":zones})
}
#[tokio::test]
async fn limport_http_ancien_ne_pose_pas_un_trim_par_numero_6774() {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let id = ZoneRepo::with_backend(state.backend.clone())
        .create("Marantz", Some("dlna"), Some("uuid:http-6774"))
        .unwrap();
    let cle = format!("zone_{id}_gain_trim_db");
    let mut body = serde_json::Map::new();
    body.insert(cle.clone(), json!("8.5"));
    body.insert("theme".into(), json!("sombre"));
    let reponse = importer(&state, Value::Object(body), false).await;
    let s = SettingsRepo::with_backend(state.backend.clone());
    assert_eq!(
        s.get(&cle).unwrap(),
        None,
        "#6774 HTTP : trim d'un ancien fichier posé par numéro : {reponse}"
    );
    assert_eq!(s.get("theme").unwrap().as_deref(), Some("sombre"));
    assert!(!reponse["warnings"].as_array().unwrap().is_empty());
}
#[tokio::test]
async fn limport_http_sans_identite_ne_remonte_pas_le_volume_et_lapercu_necrit_rien_6774() {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let id = repo.create("Salon", Some("dlna"), None).unwrap();
    repo.update_volume(id, 20.0).unwrap();
    let s = SettingsRepo::with_backend(state.backend.clone());
    let cle = format!("zone_{id}_gain_trim_db");
    s.set(&cle, "8.5").unwrap();
    let body = fichier(
        json!([{"id":9999,"name":"Salon","output_type":"dlna","volume":100,"settings":{"zone_{id}_gain_trim_db":"12","zone_{id}_audiophile":{"enabled":true,"lock_volume":true}}}]),
    );
    let apercu = importer(&state, body.clone(), true).await;
    assert_eq!(apercu["dry_run"], true);
    assert_eq!(repo.get(id).unwrap().unwrap().volume, 20.0);
    assert_eq!(
        s.get(&cle).unwrap().as_deref(),
        Some("8.5"),
        "l'aperçu ne doit rien écrire"
    );
    let r = importer(&state, body, false).await;
    assert_eq!(
        repo.get(id).unwrap().unwrap().volume,
        20.0,
        "#6774 HTTP : volume remonté sans identité : {r}"
    );
    assert_eq!(
        s.get(&cle).unwrap(),
        None,
        "#6774 HTTP : trim non neutralisé : {r}"
    );
    assert_eq!(s.get(&format!("zone_{id}_audiophile")).unwrap(), None);
    assert!(!r["warnings"].as_array().unwrap().is_empty());
}
#[tokio::test]
async fn limport_http_par_mac_garde_la_zone_et_son_identifiant_actuel_6774() {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let id = repo
        .create("Nom actuel", Some("dlna"), Some("uuid:http-actuel-6774"))
        .unwrap();
    repo.set_identity(id, "192.0.2.50", Some("AA:BB:CC:11:22:33"))
        .unwrap();
    let r = importer(&state,fichier(json!([{"id":9999,"name":"Ancien nom","output_type":"dlna","output_device_id":"uuid:http-ancien-6774","mac":"aabbcc112233","settings":{"zone_{id}_gain_trim_db":"-3"}}])),false).await;
    assert_eq!(
        r["zones_added"], 0,
        "#6774 HTTP : même MAC devenue une autre zone : {r}"
    );
    assert_eq!(
        repo.get(id).unwrap().unwrap().output_device_id.as_deref(),
        Some("uuid:http-actuel-6774")
    );
    assert_eq!(
        SettingsRepo::with_backend(state.backend.clone())
            .get(&format!("zone_{id}_gain_trim_db"))
            .unwrap()
            .as_deref(),
        Some("-3")
    );
}
