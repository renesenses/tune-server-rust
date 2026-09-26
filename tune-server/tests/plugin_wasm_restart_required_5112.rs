//! #5112 — `enable`/`disable` d'un greffon WASM : `restart_required` doit dire
//! ce que le serveur qui tourne fera VRAIMENT.
//!
//! Le registre WASM (`state.wasm_plugins`) est un `OnceLock` rempli une seule
//! fois au démarrage par `load_wasm_plugins` : il n'y a ni chargement ni
//! déchargement à chaud. Donc :
//!
//! - greffon **chargé** : `enable` ⇒ `false` (il tourne déjà), `disable` ⇒
//!   `true` (il continue de tourner, ses routes montées, jusqu'au redémarrage) ;
//! - greffon **non chargé** (désactivé au démarrage) : `enable` ⇒ `true` (rien
//!   ne le monte avant le redémarrage), `disable` ⇒ `false`.
//!
//! Avant le correctif, `greffon_charge` ne regardait que les greffons
//! COMPILÉS : un WASM chargé passait pour absent, et l'écran (`PluginsV2.svelte`,
//! `act()`) réclamait un redémarrage pour rien après `enable`.
#![cfg(feature = "plugins-wasm")]

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const ID: &str = "party";

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugins/party")
}

async fn post(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(path)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Rend les variables d'environnement de l'essai à la sortie.
struct Environnement(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl Drop for Environnement {
    fn drop(&mut self) {
        for (k, v) in &self.0 {
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}

/// Démarre un serveur dont le dossier de greffons contient Party ; `active`
/// pose `plugin_party_enabled` AVANT le chargement, comme une base existante.
async fn demarrer(dossier: &Path, active: bool) -> (AppState, axum::Router, Environnement) {
    let greffons = dossier.join("plugins");
    let cible = greffons.join(ID);
    std::fs::create_dir_all(&cible).unwrap();
    for fichier in ["main.wasm", "manifest.json"] {
        std::fs::copy(fixture().join(fichier), cible.join(fichier)).unwrap();
    }
    let env = Environnement(
        ["TUNE_PLUGINS_DIR", "TUNE_WASM_PROBE_SKIP"]
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect(),
    );
    unsafe {
        std::env::set_var("TUNE_PLUGINS_DIR", &greffons);
        std::env::set_var("TUNE_WASM_PROBE_SKIP", "1");
    }
    let db = dossier.join("tune.db");
    let state = AppState::new(db.to_str().unwrap(), 0, Default::default()).unwrap();
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            &format!("plugin_{ID}_enabled"),
            if active { "true" } else { "false" },
        )
        .unwrap();
    tune_server::plugins_host::load_wasm_plugins(&state).await;
    assert_eq!(
        state
            .wasm_plugins
            .get()
            .is_some_and(|r| r.get(ID).is_some()),
        active,
        "banc : Party doit être chargé si et seulement s'il est actif au démarrage"
    );
    let app = tune_server::routes::router(state.clone());
    (state, app, env)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wasm_deja_charge_enable_ne_demande_pas_de_redemarrage_5112() {
    let _environnement = crate::lock_environment();
    let dossier = tempfile::tempdir().unwrap();
    let (_state, app, _env) = demarrer(dossier.path(), true).await;

    let (status, corps) = post(&app, &format!("/api/v1/plugins/{ID}/enable")).await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(
        corps["restart_required"], false,
        "Party est déjà chargé : l'activer ne demande aucun redémarrage — {corps}"
    );

    // Désactiver ne le décharge pas : ses routes restent montées jusqu'au
    // prochain démarrage.
    let (status, corps) = post(&app, &format!("/api/v1/plugins/{ID}/disable")).await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(
        corps["restart_required"], true,
        "Party tourne encore : le désactiver attend un redémarrage — {corps}"
    );

    // Le réactiver aussitôt : il n'a jamais cessé de tourner.
    let (_, corps) = post(&app, &format!("/api/v1/plugins/{ID}/enable")).await;
    assert_eq!(corps["restart_required"], false, "{corps}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wasm_non_charge_enable_demande_un_redemarrage_5112() {
    let _environnement = crate::lock_environment();
    let dossier = tempfile::tempdir().unwrap();
    let (_state, app, _env) = demarrer(dossier.path(), false).await;

    // Aucun chargement à chaud : le registre est figé au démarrage.
    let (status, corps) = post(&app, &format!("/api/v1/plugins/{ID}/enable")).await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(
        corps["restart_required"], true,
        "Party n'est pas chargé : seul un redémarrage le monte — {corps}"
    );

    let (status, corps) = post(&app, &format!("/api/v1/plugins/{ID}/disable")).await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(
        corps["restart_required"], false,
        "Party n'est pas chargé : le désactiver ne change rien — {corps}"
    );
}
