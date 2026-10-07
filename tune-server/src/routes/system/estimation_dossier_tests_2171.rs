//! Fil forum 2171 — le COMPTAGE qui précède l'ajout d'un dossier, vu de la
//! route. Mêmes gardes que l'explorateur (`browse_dirs`) : il ne lit rien que
//! celui-ci ne pourrait lister, et il exige l'administrateur.
//!
//! Tests de bibliothèque et non d'intégration : `tests/explorateur_dossiers.rs`
//! n'est compilé que dans `server_contracts`.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

const SECRET: &str = "test-jwt-secret";

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

async fn compter(state: &AppState, requete: &str, porteur: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::get(requete);
    if let Some(b) = porteur {
        req = req.header(header::AUTHORIZATION, format!("Bearer {b}"));
    }
    let reponse = crate::routes::router(state.clone())
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let corps = to_bytes(reponse.into_body(), 1 << 20).await.unwrap();
    (
        statut,
        serde_json::from_slice(&corps).unwrap_or(Value::Null),
    )
}

/// Le comptage refuse les arbres système, les `..` et les chemins relatifs,
/// comme l'explorateur, et ne rend alors aucun nombre.
#[tokio::test]
async fn le_comptage_refuse_ce_que_l_explorateur_refuse() {
    let state = new_state();
    for chemin in ["/etc", "/proc", "/tmp/../etc", "Musique"] {
        let (statut, corps) = compter(
            &state,
            &format!("/api/v1/system/browse-dirs/estimate?path={chemin}"),
            None,
        )
        .await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "laissé passer : {chemin}");
        assert!(corps.get("audio_files").is_none(), "{corps}");
    }
}

/// Le comptage exige l'administrateur, comme l'ajout qu'il précède.
#[tokio::test]
async fn le_comptage_exige_l_administrateur() {
    let state = new_state();
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set("auth_enabled", "true").unwrap();
    s.set("jwt_secret", SECRET).unwrap();
    let jeton = crate::auth::sign_jwt(2, "user", SECRET).unwrap();
    let (statut, _) = compter(
        &state,
        "/api/v1/system/browse-dirs/estimate?path=/tmp",
        Some(&jeton),
    )
    .await;
    assert_eq!(statut, StatusCode::FORBIDDEN);
}

/// Sans chemin, rien n'est compté : la route ne part jamais d'une racine
/// implicite.
#[tokio::test]
async fn le_comptage_sans_chemin_est_refuse() {
    let state = new_state();
    let (statut, _) = compter(&state, "/api/v1/system/browse-dirs/estimate", None).await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
}

/// Le chemin nominal : les fichiers audio du scan sont comptés, le reste non.
#[cfg(unix)]
#[tokio::test]
async fn le_comptage_rend_le_nombre_de_fichiers_audio() {
    let state = new_state();
    // `/tmp` et non `temp_dir()` : sous macOS, `/private/var` est un arbre
    // système et le refus tomberait pour la mauvaise raison.
    let base = tune_core::test_scratch::scratch_dir_in("/tmp", "tune-estimation-route-2171");
    let album = base.join("Album");
    std::fs::create_dir_all(&album).unwrap();
    for f in ["01.flac", "02.dsf", "cover.jpg"] {
        std::fs::write(album.join(f), b"x").unwrap();
    }
    let (statut, corps) = compter(
        &state,
        &format!(
            "/api/v1/system/browse-dirs/estimate?path={}",
            base.display()
        ),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["audio_files"], 2, "{corps}");
    assert_eq!(corps["folders"], 1, "{corps}");
    assert_eq!(corps["complete"], true, "{corps}");
    assert_eq!(corps["drive_root"], false, "{corps}");
}
