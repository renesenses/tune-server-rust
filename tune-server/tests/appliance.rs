//! Appliance mode endpoints (WiFi via nmcli).
//!
//! Single test function on purpose: the appliance gate and the nmcli binary
//! are driven by process-wide env vars (TUNE_APPLIANCE / TUNE_NMCLI_BIN),
//! so the scenarios must run sequentially inside one test.
#![cfg(unix)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

fn make_app() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    tune_server::routes::router(state)
}

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(json!(null)))
}

async fn post_json(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(json!(null)))
}

/// Le garde du dossier est rendu AVEC le chemin du bouchon : sans lui, le
/// dossier serait supprimé à la sortie de cette fonction et le bouchon
/// disparaîtrait avant que `nmcli` ne soit appelé (#3030).
fn write_nmcli_stub() -> (tune_core::test_scratch::ScratchDir, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tune_core::test_scratch::scratch_dir("tune-appliance-test");
    let path = dir.join("nmcli-stub.sh");
    let script = r#"#!/bin/bash
args="$*"
case "$args" in
  *"device wifi list"*)
    printf ' :Livebox-1234:78:WPA2\n*:My\\:Net:64:WPA2\n :Livebox-1234:40:WPA2\n'
    ;;
  *"device wifi connect BadNet"*)
    echo "Error: Connection activation failed: Secrets were required, but not provided." >&2
    exit 4
    ;;
  *"device wifi connect"*)
    echo "Device 'wlan0' successfully activated with 'abcd-1234'."
    ;;
  *"DEVICE,TYPE,STATE,CONNECTION device"*)
    printf 'enp1s0:ethernet:connected:Wired connection 1\nwlan0:wifi:disconnected:\nlo:loopback:unmanaged:\n'
    ;;
  *"connection delete"*)
    echo "Connection 'Old-Net' (abcd) successfully deleted."
    ;;
  *)
    echo "unexpected args: $args" >&2
    exit 1
    ;;
esac
"#;
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (dir, path)
}

#[tokio::test]
async fn appliance_endpoints_full_flow() {
    let _environment = crate::lock_environment();
    let app = make_app();

    // 1) Not an appliance: everything is 404, config flag is false.
    let (status, _) = get(&app, "/api/v1/appliance/status").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&app, "/api/v1/appliance/wifi/scan").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) =
        post_json(&app, "/api/v1/appliance/wifi/connect", json!({"ssid": "X"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, config) = get(&app, "/api/v1/system/config").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(config["appliance"], json!(false));

    // 2) Appliance mode with stubbed nmcli.
    let (_dossier_stub, stub) = write_nmcli_stub();
    unsafe {
        std::env::set_var("TUNE_APPLIANCE", "1");
        std::env::set_var("TUNE_NMCLI_BIN", &stub);
    }

    let (status, config) = get(&app, "/api/v1/system/config").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(config["appliance"], json!(true));

    let (status, body) = get(&app, "/api/v1/appliance/status").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["appliance"], json!(true));
    assert_eq!(body["ethernet_connected"], json!(true));
    assert_eq!(body["wifi_connected"], json!(false));
    assert_eq!(body["devices"].as_array().unwrap().len(), 2);

    let (status, body) = get(&app, "/api/v1/appliance/wifi/scan").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let networks = body["networks"].as_array().unwrap();
    assert_eq!(networks.len(), 2, "{body}");
    assert_eq!(networks[0]["ssid"], "Livebox-1234");
    assert_eq!(networks[0]["signal"], 78);
    assert_eq!(networks[1]["ssid"], "My:Net");
    assert_eq!(networks[1]["in_use"], json!(true));

    let (status, body) = post_json(
        &app,
        "/api/v1/appliance/wifi/connect",
        json!({"ssid": "Livebox-1234", "password": "secret123"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["connected"], json!(true));

    // Wrong password surfaces as a 400 with a clean message.
    let (status, body) = post_json(
        &app,
        "/api/v1/appliance/wifi/connect",
        json!({"ssid": "BadNet", "password": "wrong"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Invalid SSID rejected before any command runs.
    let (status, _) = post_json(
        &app,
        "/api/v1/appliance/wifi/connect",
        json!({"ssid": "bad\nssid"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, body) = post_json(
        &app,
        "/api/v1/appliance/wifi/forget",
        json!({"ssid": "Old-Net"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["forgotten"], json!(true));

    unsafe {
        std::env::remove_var("TUNE_APPLIANCE");
        std::env::remove_var("TUNE_NMCLI_BIN");
    }
}

/// `POST /appliance/shutdown` : la route existe VRAIMENT, et elle est gardée.
///
/// Le montage était jusqu'ici « éprouvé » par un test unitaire qui affichait le
/// routeur et cherchait le mot « Router » dedans — vrai d'un routeur vide. Il a
/// été retiré ; c'est cette requête-ci qui prouve le montage.
///
/// Et une route qui éteint la machine ne doit jamais partir sur la foi d'un
/// chemin : ni sans jeton, ni depuis un profil ordinaire. On lit ici les trois
/// refus AVANT que `require_appliance()` n'ait son mot à dire — l'ordre compte,
/// un 404 « mode appliance inactif » rendu à un anonyme divulguerait déjà la
/// nature de la machine.
///
/// Aucune variable d'environnement n'est posée : ces trois cas se jouent
/// entièrement avant la porte appliance. Le verrou d'environnement est pris
/// quand même — non pour écrire, mais pour garantir qu'aucun essai voisin ne
/// laisse `TUNE_APPLIANCE=1` pendant celui-ci : le cas 3 atteindrait alors le
/// corps du handler et lancerait un VRAI `systemctl poweroff` sur la machine
/// de test.
#[tokio::test]
async fn extinction_montee_et_gardee_par_l_auth() {
    use axum::http::header;
    use tune_core::db::settings_repo::SettingsRepo;

    let _environment = crate::lock_environment();
    assert!(
        !tune_server::routes::appliance::is_appliance(),
        "machine de test en mode appliance : cet essai éteindrait pour de vrai"
    );

    const SECRET: &str = "test-jwt-secret-2135";
    const ROUTE: &str = "/api/v1/appliance/shutdown";

    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("auth_enabled", "true").unwrap();
    settings.set("jwt_secret", SECRET).unwrap();

    async fn statut(state: &tune_server::state::AppState, jeton: Option<&str>) -> StatusCode {
        let app = tune_server::routes::router(state.clone());
        let mut req = Request::post(ROUTE);
        if let Some(j) = jeton {
            req = req.header(header::AUTHORIZATION, format!("Bearer {j}"));
        }
        app.oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    // 1) Anonyme : refusé net. Surtout pas 200, surtout pas 404.
    assert_eq!(
        statut(&state, None).await,
        StatusCode::UNAUTHORIZED,
        "une extinction sans jeton doit être refusée"
    );

    // 2) Jeton valide mais rôle ordinaire : refusé aussi. Un compte d'écoute
    //    n'éteint pas la machine des autres.
    let jeton_user = tune_server::auth::sign_jwt(2, "user", SECRET).unwrap();
    assert_eq!(
        statut(&state, Some(&jeton_user)).await,
        StatusCode::FORBIDDEN,
        "le rôle `user` n'a pas à éteindre l'appliance"
    );

    // 3) Admin, mais machine ordinaire : la route EXISTE (on l'a atteinte, ce
    //    n'est plus l'auth qui parle) et la porte appliance rend 404. C'est ce
    //    404-là qui prouve le montage : un chemin non monté rendrait 404 lui
    //    aussi, mais il aurait rendu 404 aux étapes 1 et 2 également.
    let jeton_admin = tune_server::auth::sign_jwt(1, "admin", SECRET).unwrap();
    assert_eq!(
        statut(&state, Some(&jeton_admin)).await,
        StatusCode::NOT_FOUND,
        "hors appliance la route ne s'exécute pas — mais elle est bien montée"
    );
}
