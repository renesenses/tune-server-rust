//! #7067 : sous Docker, désinstaller un greffon fourni avec l'image
//! (`/app/plugins`, en lecture seule) échouait en « Permission denied
//! (os error 13) ». Le greffon fourni est désormais désactivé et masqué ; celui
//! de l'utilisateur, sous le dossier de données, est réellement supprimé.
#![cfg(all(feature = "plugins-wasm", unix))]
use axum::body::Body;
use axum::http::{HeaderValue, Request, StatusCode};
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

async fn call(app: &axum::Router, method: &str, path: &str, lang: &str) -> (StatusCode, Value) {
    // `install` lit un corps JSON (`{}` suffit) : sans lui, 415.
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
        .insert("accept-language", HeaderValue::from_str(lang).unwrap());
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

const VARIABLES: [&str; 3] = [
    "TUNE_PLUGINS_DIR",
    "TUNE_PLUGINS_USER_DIR",
    "TUNE_WASM_PROBE_SKIP",
];

struct Environnement {
    _tmp: tempfile::TempDir,
    fournis: PathBuf,
    utilisateur: PathBuf,
    avant: Vec<Option<std::ffi::OsString>>,
}
impl Environnement {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let fournis = tmp.path().join("app-plugins");
        let utilisateur = tmp.path().join("data-plugins");
        std::fs::create_dir_all(&fournis).unwrap();
        let avant = VARIABLES.iter().map(std::env::var_os).collect();
        unsafe {
            std::env::set_var("TUNE_PLUGINS_DIR", &fournis);
            std::env::set_var("TUNE_PLUGINS_USER_DIR", &utilisateur);
            std::env::set_var("TUNE_WASM_PROBE_SKIP", "1");
        }
        Self {
            _tmp: tmp,
            fournis,
            utilisateur,
            avant,
        }
    }
    fn greffon(&self, racine: &Path, id: &str) -> PathBuf {
        let dir = racine.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugins/party");
        let mut manifest: Value =
            serde_json::from_slice(&std::fs::read(fixture.join("manifest.json")).unwrap()).unwrap();
        manifest["id"] = json!(id);
        std::fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
        std::fs::copy(fixture.join("main.wasm"), dir.join("main.wasm")).unwrap();
        dir
    }
    fn dossiers_des_fournis(&self) -> Vec<PathBuf> {
        let mut dossiers = vec![self.fournis.clone()];
        let mut i = 0;
        while i < dossiers.len() {
            if let Ok(rd) = std::fs::read_dir(&dossiers[i]) {
                for e in rd.flatten() {
                    if e.path().is_dir() {
                        dossiers.push(e.path());
                    }
                }
            }
            i += 1;
        }
        dossiers
    }
    /// Comme `chown -R root:root /app && chmod -R 755 /app` vu par `tune`.
    fn figer_les_fournis(&self) {
        for d in self.dossiers_des_fournis().iter().rev() {
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o555)).unwrap();
        }
    }
    fn permissions_ignorees(&self) -> bool {
        std::fs::write(self.fournis.join("sonde"), b"x").is_ok()
    }
}
impl Drop for Environnement {
    fn drop(&mut self) {
        // Le dossier temporaire doit pouvoir être effacé.
        for d in self.dossiers_des_fournis() {
            let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755));
        }
        unsafe {
            for (nom, valeur) in VARIABLES.iter().zip(&self.avant) {
                match valeur {
                    Some(v) => std::env::set_var(nom, v),
                    None => std::env::remove_var(nom),
                }
            }
        }
    }
}

fn est_liste(liste: &Value, nom: &str) -> bool {
    liste.as_array().unwrap().iter().any(|p| p["name"] == nom)
}

/// Contre-épreuve : retirer, dans `remove_plugin_dir_in`, la sonde `removable`
/// (ou la branche `FourniEnLectureSeule` de `delete_plugin`) fait rougir ce
/// test sur le statut 500 et le message « Permission denied ».
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn desinstaller_un_greffon_fourni_en_lecture_seule_le_masque_au_lieu_d_echouer() {
    let _environnement = crate::lock_environment();
    let env = Environnement::new();
    let fourni = env.greffon(&env.fournis, "playlists-converter");
    env.figer_les_fournis();
    if env.permissions_ignorees() {
        eprintln!("processus root : cas non reproductible, essai ignoré");
        return;
    }
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let app = tune_server::routes::router(state);

    let (_, liste) = call(&app, "GET", "/api/v1/plugins", "fr").await;
    assert!(est_liste(&liste, "playlists-converter"));

    let (statut, corps) = call(&app, "DELETE", "/api/v1/plugins/playlists-converter", "fr").await;
    let texte = corps.to_string();
    assert_eq!(
        statut,
        StatusCode::OK,
        "désinstaller un greffon fourni ne doit pas échouer : {texte}"
    );
    assert_eq!(corps["status"], "uninstalled");
    assert_eq!(corps["hidden"], true);
    assert!(
        !texte.contains("os error") && !texte.contains("Permission denied") && !texte.contains('/'),
        "aucun chemin ni erreur système brute dans la réponse : {texte}"
    );
    assert!(
        corps["message"].as_str().unwrap().contains("fourni"),
        "message en français attendu : {texte}"
    );
    assert!(
        fourni.join("manifest.json").exists(),
        "l'image n'est pas touchée"
    );

    // Persistant : masqué de la liste, désactivé, et le nettoyage générique
    // n'a pas rétabli l'état « activé par défaut ».
    assert_eq!(
        settings
            .get("plugin_playlists-converter_hidden")
            .unwrap()
            .as_deref(),
        Some("true")
    );
    assert_eq!(
        settings
            .get("plugin_playlists-converter_enabled")
            .unwrap()
            .as_deref(),
        Some("false")
    );
    let (_, liste) = call(&app, "GET", "/api/v1/plugins", "fr").await;
    assert!(
        !est_liste(&liste, "playlists-converter"),
        "masqué de la liste"
    );

    // Même chose par le marché, et dans une autre langue.
    let (statut, corps) = call(
        &app,
        "POST",
        "/api/v1/marketplace/plugins/playlists-converter/uninstall",
        "de",
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert!(
        corps["message"].as_str().unwrap().contains("Plugin"),
        "{corps}"
    );

    // Le réinstaller le rétablit.
    let (statut, _) = call(
        &app,
        "POST",
        "/api/v1/plugins/playlists-converter/install",
        "fr",
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    let (_, liste) = call(&app, "GET", "/api/v1/plugins", "fr").await;
    assert!(
        est_liste(&liste, "playlists-converter"),
        "revenu après réinstallation"
    );
    assert_eq!(
        settings.get("plugin_playlists-converter_hidden").unwrap(),
        None
    );
    assert_eq!(
        settings
            .get("plugin_playlists-converter_enabled")
            .unwrap()
            .as_deref(),
        Some("true")
    );
}

/// Le greffon installé par l'utilisateur vit sous le dossier de données et se
/// supprime pour de bon, même quand le dossier des fournis est figé.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_greffon_de_l_utilisateur_est_supprime_pour_de_bon() {
    let _environnement = crate::lock_environment();
    let env = Environnement::new();
    env.greffon(&env.fournis, "playlists-converter");
    let perso = env.greffon(&env.utilisateur, "mon-greffon");
    env.figer_les_fournis();
    if env.permissions_ignorees() {
        return;
    }
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state);
    let (statut, corps) = call(&app, "DELETE", "/api/v1/plugins/mon-greffon", "en").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert!(corps.get("hidden").is_none(), "pas masqué : supprimé");
    assert!(!perso.exists(), "le dossier de l'utilisateur a disparu");
}
