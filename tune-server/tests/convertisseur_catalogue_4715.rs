//! #4715 — « Playlists converter » au catalogue des extensions, PREMIUM,
//! pour la v0.9.166.
//!
//! Décision de Bertrand du 25/09/2026 : le greffon WASM devient installable
//! et reste payant (`manifest.premium = true`, srv#4740). Il est déclaré comme
//! les greffons premium compilés : un BADGE (`premium: true` sur sa fiche de
//! `GET /plugins` et de `GET /plugins/{nom}`) et un VERROU (402 sur ses routes,
//! et sur `enable`/`install`/`update`, pour un compte sans Premium).
//!
//! Deux chemins l'amènent sur le disque d'un serveur, et ce fichier prouve les
//! deux jusqu'aux routes que les onglets du client web appellent
//! (`tune-web-client`, `src/lib/api.ts`,
//! `CONVERTISSEUR = ${BASE}/plugins/playlists-converter`) :
//!
//! 1. **Livré avec le serveur, comme Party.** `release.yml` et `docker.yml`
//!    copient `tests/fixtures/plugins/playlists-converter/{main.wasm,
//!    manifest.json}` dans `plugins/playlists-converter/`, à côté du binaire.
//!    Le premier essai reproduit exactement cette disposition.
//! 2. **Depuis la boutique** (`GET /marketplace/plugins`, puis
//!    `POST /marketplace/plugins/{slug}/install`). La boutique est mozaiklabs.fr ;
//!    l'essai joue une boutique de banc (`TUNE_MARKETPLACE_URL`) qui sert la
//!    fiche PAYANTE et l'archive telles que mozaiklabs doit les publier, et ne
//!    touche jamais la vraie.
//!
//! Dans les deux cas, une fois Premium, `GET /api/v1/plugins` rend la fiche
//! avec les TROIS conditions dont `convertisseurCharge` (web,
//! `stores/convertisseurPlaylists.ts`) fait dépendre l'affichage des onglets —
//! `installed`, `enabled`, `loaded` —, et les routes répondent.
#![cfg(feature = "plugins-wasm")]

use std::io::Write;
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_server::state::AppState;

const ID: &str = "playlists-converter";

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/plugins/playlists-converter")
}

async fn call(app: &axum::Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path);
    let body = if body.is_null() {
        Body::empty()
    } else {
        req = req.header("content-type", "application/json");
        Body::from(body.to_string())
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Pose les variables d'environnement de l'essai et les rend à la sortie.
struct Environnement {
    anciennes: Vec<(&'static str, Option<std::ffi::OsString>)>,
}
impl Environnement {
    fn poser(valeurs: &[(&'static str, String)]) -> Self {
        let anciennes = valeurs
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        for (k, v) in valeurs {
            unsafe { std::env::set_var(k, v) };
        }
        Self { anciennes }
    }
}
impl Drop for Environnement {
    fn drop(&mut self) {
        for (k, v) in &self.anciennes {
            unsafe {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }
}

/// Démarre un serveur sur `db` : chargement des greffons wasm, puis routeur.
/// C'est la séquence de `run.rs`, réduite à ce que le greffon touche.
async fn demarrer(db: &Path) -> (AppState, axum::Router) {
    let state = AppState::new(db.to_str().unwrap(), 0, Default::default()).unwrap();
    tune_server::plugins_host::load_wasm_plugins(&state).await;
    let app = tune_server::routes::router(state.clone());
    (state, app)
}

/// La fiche de `GET /plugins` : badge Premium, et les trois conditions des
/// onglets web.
async fn fiche_du_greffon(app: &axum::Router) -> Value {
    let (status, liste) = call(app, "GET", "/api/v1/plugins", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let fiche = liste
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == ID)
        .unwrap_or_else(|| panic!("{ID} absent de GET /plugins : {liste}"))
        .clone();
    assert_eq!(fiche["type"], "wasm", "{fiche}");
    assert_eq!(
        fiche["premium"], true,
        "le badge, comme les fiches compilées : {fiche}"
    );
    // Les trois conditions de `convertisseurCharge` (web).
    assert_eq!(fiche["installed"], true, "{fiche}");
    assert_eq!(fiche["enabled"], true, "{fiche}");
    assert_eq!(
        fiche["loaded"], true,
        "chargé : ses routes sont montées — {fiche}"
    );
    assert_eq!(fiche["compatible"], true, "{fiche}");
    let (status, detail) = call(app, "GET", &format!("/api/v1/plugins/{ID}"), Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        detail["premium"], true,
        "le badge sur le détail aussi : {detail}"
    );
    fiche
}

/// Sans Premium : le verrou, sur les routes des onglets comme sur les gestes
/// du gestionnaire.
async fn sans_premium_tout_est_verrouille(app: &axum::Router) {
    let base = format!("/api/v1/plugins/{ID}");
    for (methode, route) in [("GET", "/lots"), ("GET", "/snapshots"), ("GET", "/liens")] {
        let (status, corps) = call(app, methode, &format!("{base}{route}"), Value::Null).await;
        assert_eq!(
            status,
            StatusCode::PAYMENT_REQUIRED,
            "{route} sans licence : {corps}"
        );
        assert_eq!(corps["error"], "premium_required", "{corps}");
        assert_eq!(corps["code"], "plugin_marketplace", "{corps}");
    }
    for geste in ["enable", "install", "update"] {
        let (status, corps) = call(app, "POST", &format!("{base}/{geste}"), json!({})).await;
        assert_eq!(
            status,
            StatusCode::PAYMENT_REQUIRED,
            "{geste} sans licence : {corps}"
        );
        assert_eq!(corps["error"], "premium_required", "{corps}");
    }
}

/// Avec Premium : ce que les onglets appellent répond.
async fn avec_premium_les_onglets_repondent(app: &axum::Router) {
    let base = format!("/api/v1/plugins/{ID}");
    let (status, corps) = call(app, "GET", &format!("{base}/lots"), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "GET /lots : {corps}");
    assert_eq!(corps["count"], 0, "{corps}");
    assert!(corps["lots"].is_array(), "{corps}");

    let (status, corps) = call(app, "GET", &format!("{base}/snapshots"), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "GET /snapshots : {corps}");
    assert!(corps["playlists"].is_array(), "{corps}");
    assert!(corps["retention_par_playlist"].is_number(), "{corps}");

    let (status, corps) = call(app, "GET", &format!("{base}/liens"), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "GET /liens : {corps}");

    // L'aperçu : aucun service n'est connecté sur ce banc, le greffon le dit
    // lui-même — mais c'est bien LUI qui répond, ni la garde (402), ni l'hôte
    // (404 « plugin not found »).
    let (status, corps) = call(
        app,
        "POST",
        &format!("{base}/apercu"),
        json!({ "source_service": "tidal", "cible_service": "qobuz", "playlists": ["pl-1"] }),
    )
    .await;
    assert_ne!(status, StatusCode::PAYMENT_REQUIRED, "{corps}");
    assert_ne!(status, StatusCode::NOT_FOUND, "{corps}");
    assert!(
        corps["error"].is_string() || corps["lot"].is_object(),
        "{corps}"
    );

    let (status, corps) = call(app, "POST", &format!("{base}/enable"), json!({})).await;
    assert_eq!(status, StatusCode::OK, "enable avec Premium : {corps}");
}

/// Chemin 1 — la disposition exacte des paquets publiés (`release.yml`,
/// `docker.yml`) : le fixture copié sous `plugins/playlists-converter/`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn livre_comme_party_le_greffon_porte_badge_et_verrou_4715() {
    let _environnement = crate::lock_environment();
    let dossier = tempfile::tempdir().unwrap();
    let greffons = dossier.path().join("plugins");
    let cible = greffons.join(ID);
    std::fs::create_dir_all(&cible).unwrap();
    for fichier in ["main.wasm", "manifest.json"] {
        std::fs::copy(fixture().join(fichier), cible.join(fichier)).unwrap();
    }
    let _env = Environnement::poser(&[
        ("TUNE_PLUGINS_DIR", greffons.display().to_string()),
        ("TUNE_WASM_PROBE_SKIP", "1".into()),
    ]);

    let (state, app) = demarrer(&dossier.path().join("tune.db")).await;
    fiche_du_greffon(&app).await;
    sans_premium_tout_est_verrouille(&app).await;
    state.license.set_account_premium(true, None).await;
    avec_premium_les_onglets_repondent(&app).await;
}

/// La fiche que mozaiklabs.fr doit servir dans `GET /api/v1/plugins`.
///
/// `slug` ET `name` valent l'identifiant du manifeste : c'est ce que
/// `ajouterLaBoutique` (web, `lib/catalogueGreffons.ts`) compare pour ne pas
/// montrer deux fois le greffon quand il est déjà livré avec le serveur.
/// `price` > 0 = payant : `is_free_plugin` refuse alors l'installation sans
/// Premium. `platforms: "wasm"` = seule plateforme que
/// `MarketplacePlugin::is_installable` garde.
fn fiche_boutique() -> Value {
    json!({
        "name": ID,
        "slug": ID,
        "display_name": "Playlists converter",
        "description": "Transférer une playlist d'un service à un autre, par lot, avec aperçu obligatoire",
        "author": "MozAIk Labs",
        "version": "0.3.0",
        "category": "integrations",
        "install_type": "store",
        "platforms": "wasm",
        "min_tune_version": "0.9.166",
        "price": 4.99
    })
}

/// L'archive que mozaiklabs.fr doit servir à `/api/v1/plugins/{name}/download` :
/// un zip qui porte `manifest.json` et `main.wasm` à sa racine.
fn archive_boutique() -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    for fichier in ["manifest.json", "main.wasm"] {
        zip.start_file(fichier, options).unwrap();
        zip.write_all(&std::fs::read(fixture().join(fichier)).unwrap())
            .unwrap();
    }
    zip.finish().unwrap().into_inner()
}

async fn boutique_de_banc() -> String {
    use axum::routing::get;
    let archive = archive_boutique();
    let app = axum::Router::new()
        .route(
            "/api/v1/plugins",
            get(|| async { axum::Json(json!([fiche_boutique()])) }),
        )
        .route(
            "/api/v1/plugins/{name}/download",
            get(
                move |axum::extract::Path(name): axum::extract::Path<String>| {
                    let archive = archive.clone();
                    async move {
                        if name == ID {
                            (StatusCode::OK, archive)
                        } else {
                            (StatusCode::NOT_FOUND, Vec::new())
                        }
                    }
                },
            ),
        )
        // Pas de signature publiée : la vérification n'est pas exigée par
        // défaut (`plugin_signature_required`), comme pour Party aujourd'hui.
        .route(
            "/api/v1/plugins/{name}/download.minisig",
            get(|| async { StatusCode::NOT_FOUND }),
        );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });
    format!("http://{adresse}")
}

/// Chemin 2 — le catalogue des extensions : la boutique propose la fiche
/// payante ; sans Premium l'installation est refusée et rien n'est écrit ;
/// avec Premium elle passe, et au démarrage suivant les routes des onglets
/// répondent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn depuis_le_catalogue_le_greffon_s_installe_avec_premium_4715() {
    let _environnement = crate::lock_environment();
    let dossier = tempfile::tempdir().unwrap();
    let greffons = dossier.path().join("plugins");
    std::fs::create_dir_all(&greffons).unwrap();
    let boutique = boutique_de_banc().await;
    let _env = Environnement::poser(&[
        ("TUNE_PLUGINS_DIR", greffons.display().to_string()),
        ("TUNE_WASM_PROBE_SKIP", "1".into()),
        ("TUNE_MARKETPLACE_URL", boutique),
    ]);
    let db = dossier.path().join("tune.db");

    // Premier démarrage : rien sur le disque, la boutique propose la fiche.
    let (state, app) = demarrer(&db).await;
    let (status, liste) = call(&app, "GET", "/api/v1/plugins", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !liste.as_array().unwrap().iter().any(|p| p["name"] == ID),
        "pas encore installé : {liste}"
    );
    let (status, catalogue) = call(&app, "GET", "/api/v1/marketplace/plugins", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let fiche = catalogue["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["slug"] == ID)
        .unwrap_or_else(|| panic!("fiche absente du catalogue : {catalogue}"))
        .clone();
    assert_eq!(fiche["installed"], false, "{fiche}");
    assert_eq!(fiche["price"], 4.99, "payant : {fiche}");

    let installer = format!("/api/v1/marketplace/plugins/{ID}/install");
    // Sans Premium : refusé, et RIEN sur le disque.
    let (status, corps) = call(&app, "POST", &installer, Value::Null).await;
    assert_eq!(status, StatusCode::PAYMENT_REQUIRED, "{corps}");
    assert!(!greffons.join(ID).exists(), "un refus n'écrit rien");

    // Avec Premium : installé.
    state.license.set_account_premium(true, None).await;
    let (status, corps) = call(&app, "POST", &installer, Value::Null).await;
    assert_eq!(status, StatusCode::OK, "installation refusée : {corps}");
    assert_eq!(corps["status"], "installed", "{corps}");
    assert_eq!(corps["plugin_id"], ID, "{corps}");
    assert_eq!(corps["restart_required"], true, "{corps}");
    assert!(greffons.join(ID).join("main.wasm").is_file());
    assert!(greffons.join(ID).join("manifest.json").is_file());
    drop(app);
    drop(state);

    // Redémarrage sur la même base (la licence y est notée) : le registre wasm
    // ne se remplit qu'au boot.
    let (_state, app) = demarrer(&db).await;
    fiche_du_greffon(&app).await;
    avec_premium_les_onglets_repondent(&app).await;
    let (_, catalogue) = call(&app, "GET", "/api/v1/marketplace/plugins", Value::Null).await;
    let fiche = catalogue["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["slug"] == ID)
        .unwrap();
    assert_eq!(fiche["installed"], true, "{fiche}");
}
