//! web#3195 — activer l'authentification depuis l'interface, sans ouvrir la
//! porte au réseau ni s'enfermer dehors. La règle est écrite en tête de
//! `tune_server::auth::activation`.
//!
//! Chaque test a sa propre adresse d'appelant : le compteur d'échecs de
//! connexion est global au processus.

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use std::net::SocketAddr;
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::auth::activation::{
    FICHIER_CODE, FICHIER_DE_SECOURS, appliquer_le_fichier_de_secours,
};
use tune_server::state::AppState;

const LOCAL: &str = "127.0.0.1:5555";

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn app(state: &AppState, pair: &str) -> Router {
    let pair: SocketAddr = pair.parse().unwrap();
    tune_server::routes::router(state.clone()).layer(MockConnectInfo(pair))
}

fn reglage(state: &AppState, cle: &str) -> Option<String> {
    SettingsRepo::with_backend(state.backend.clone())
        .get(cle)
        .unwrap()
}

fn active(state: &AppState) -> bool {
    reglage(state, "auth_enabled").as_deref() == Some("true")
}

fn dossier(state: &AppState) -> std::path::PathBuf {
    std::path::Path::new(&state.config.db_path)
        .parent()
        .unwrap()
        .to_path_buf()
}

async fn appel(
    app: &Router,
    methode: &str,
    chemin: &str,
    jeton: Option<&str>,
    corps: Option<Value>,
) -> (StatusCode, Value, Option<String>) {
    let mut req = Request::builder().method(methode).uri(chemin);
    if let Some(j) = jeton {
        req = req.header(header::AUTHORIZATION, format!("Bearer {j}"));
    }
    let req = match corps {
        Some(c) => req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(c.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let statut = resp.status();
    let cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = serde_json::from_slice(&octets)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&octets).into()));
    (statut, v, cookie)
}

/// Demande un code et le lit dans le fichier, comme le propriétaire.
async fn lire_un_code(app: &Router, state: &AppState) -> String {
    let (st, v, _) = appel(
        app,
        "POST",
        "/api/v1/auth/activation-code",
        None,
        Some(json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let chemin = v["file"].as_str().unwrap().to_string();
    assert_eq!(
        std::path::Path::new(&chemin),
        dossier(state).join(FICHIER_CODE)
    );
    assert!(
        v.get("code").is_none(),
        "le code ne doit jamais sortir par HTTP : {v}"
    );
    let texte = std::fs::read_to_string(&chemin).unwrap();
    texte
        .lines()
        .map(str::trim)
        .find(|l| l.len() == 12 && l.chars().all(|c| c.is_ascii_alphanumeric()))
        .expect("code dans le fichier")
        .to_string()
}

fn creer(nom: &str, mdp: &str, code: Option<&str>) -> Value {
    json!({ "username": nom, "password": mdp, "confirm": true, "create_admin": true, "activation_code": code })
}

// ---------------------------------------------------------------------------

/// Depuis le réseau, sans code : refus, et rien n'est activé.
#[tokio::test]
async fn reseau_sans_code_refuse() {
    let state = new_state();
    let app = app(&state, "192.168.1.41:4000");
    let (st, v, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/enable",
        None,
        Some(creer("bob", "motdepasse-1", None)),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"], "activation_code_required");
    assert!(!active(&state));
    // Ni profil créé.
    let (_, cfg, _) = appel(&app, "GET", "/api/v1/auth/config", None, None).await;
    assert_eq!(cfg["admin_with_password"], false);
}

/// Depuis le réseau, code FAUX : refus.
#[tokio::test]
async fn reseau_code_faux_refuse() {
    let state = new_state();
    let app = app(&state, "192.168.1.42:4000");
    let _ = lire_un_code(&app, &state).await;
    let (st, v, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/enable",
        None,
        Some(creer("bob", "motdepasse-1", Some("AAAAAAAAAAAA"))),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"], "activation_code_invalid");
    assert!(!active(&state));
}

/// Le flux complet depuis le réseau : code lu sur la machine, admin créé,
/// authentification active, session ouverte (pas de verrouillage dehors),
/// code consommé.
#[tokio::test]
async fn reseau_avec_code_cree_l_admin_active_et_ouvre_la_session() {
    let state = new_state();
    let app = app(&state, "192.168.1.43:4000");
    let code = lire_un_code(&app, &state).await;
    // Un secret posé avant (serveur ouvert) doit être renouvelé.
    SettingsRepo::with_backend(state.backend.clone())
        .set("jwt_secret", "secret-d-avant")
        .unwrap();

    let (st, v, cookie) = appel(
        &app,
        "POST",
        "/api/v1/auth/enable",
        None,
        Some(creer("bob", "motdepasse-1", Some(&code.to_lowercase()))),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(active(&state));
    assert_eq!(v["user"]["role"], "admin");
    assert!(cookie.unwrap().starts_with("tune_session="));
    assert_ne!(
        reglage(&state, "jwt_secret").as_deref(),
        Some("secret-d-avant")
    );
    assert!(
        !dossier(&state).join(FICHIER_CODE).exists(),
        "code consommé"
    );

    // La session rendue ouvre une route d'administrateur.
    let jeton = v["token"].as_str().unwrap().to_string();
    let (st, _, _) = appel(&app, "GET", "/api/v1/auth/api-key", Some(&jeton), None).await;
    assert_eq!(st, StatusCode::OK);
    // Contre-épreuve : sans jeton, la même route est fermée.
    let (st, _, _) = appel(&app, "GET", "/api/v1/auth/api-key", None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    // Et l'administrateur se reconnecte avec son mot de passe.
    let (st, v, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username":"bob","password":"motdepasse-1"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["role"], "admin");
}

/// Un code ne sert qu'une fois.
#[tokio::test]
async fn le_code_ne_sert_qu_une_fois() {
    let state = new_state();
    let app = app(&state, "192.168.1.44:4000");
    let code = lire_un_code(&app, &state).await;
    let (st, _, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/enable",
        None,
        Some(creer("bob", "motdepasse-1", Some(&code))),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    // L'admin coupe (session d'admin), puis on rejoue le même code.
    let (_, v, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username":"bob","password":"motdepasse-1"})),
    )
    .await;
    let jeton = v["token"].as_str().unwrap().to_string();
    let (st, _, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/config",
        Some(&jeton),
        Some(json!({"auth_enabled": false})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, v, _) = appel(&app, "POST", "/api/v1/auth/enable", None, Some(json!({"username":"bob","password":"motdepasse-1","confirm":true,"activation_code":code}))).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"], "activation_code_invalid");
}

/// Activation sans administrateur avec mot de passe : 409 explicite.
#[tokio::test]
async fn activation_sans_admin_avec_mot_de_passe_409() {
    let state = new_state();
    let app = app(&state, LOCAL);
    // `default` est administrateur mais sans mot de passe.
    let (st, v, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/enable",
        None,
        Some(json!({"username":"default","password":"x","confirm":true})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], "admin_password_required");
    let (st, v, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/enable",
        None,
        Some(json!({"username":"inconnu","password":"x","confirm":true})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    // Par l'ancienne route aussi, même depuis cette machine.
    let (st, v, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/config",
        None,
        Some(json!({"auth_enabled": true})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert!(!active(&state));
}

/// Depuis cette machine : pas de code, mais confirmation et admin requis ;
/// donner un mot de passe à `default` puis activer.
#[tokio::test]
async fn cette_machine_sans_code_mais_avec_confirmation() {
    let state = new_state();
    let app = app(&state, LOCAL);
    let (st, v, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/enable",
        None,
        Some(json!({"username":"default","password":"motdepasse-1","create_admin":true})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"], "confirmation_required");
    assert!(!active(&state));

    let (st, v, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/enable",
        None,
        Some(creer("default", "court", None)),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"], "password_too_short");

    let (st, v, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/enable",
        None,
        Some(creer("default", "motdepasse-1", None)),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(active(&state));
}

/// Admin existant : mauvais mot de passe refusé, bon accepté ; un profil non
/// administrateur ne peut pas activer.
#[tokio::test]
async fn admin_existant_et_profil_ordinaire() {
    let state = new_state();
    let app = app(&state, LOCAL);
    // Un admin avec mot de passe, puis un compte ordinaire.
    let (st, _, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/enable",
        None,
        Some(creer("alice", "motdepasse-1", None)),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, v, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username":"alice","password":"motdepasse-1"})),
    )
    .await;
    let jeton = v["token"].as_str().unwrap().to_string();
    let (st, _, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/register",
        Some(&jeton),
        Some(json!({"username":"carole","password":"motdepasse-2"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let (st, _, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/config",
        Some(&jeton),
        Some(json!({"auth_enabled": false})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(!active(&state));

    let base = json!({"confirm": true});
    let mut m = base.clone();
    m["username"] = json!("alice");
    m["password"] = json!("pas-le-bon");
    let (st, v, _) = appel(&app, "POST", "/api/v1/auth/enable", None, Some(m)).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
    let mut m = base.clone();
    m["username"] = json!("carole");
    m["password"] = json!("motdepasse-2");
    let (st, v, _) = appel(&app, "POST", "/api/v1/auth/enable", None, Some(m)).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"], "admin_required");
    assert!(!active(&state));
    let mut m = base.clone();
    m["username"] = json!("alice");
    m["password"] = json!("motdepasse-1");
    let (st, v, _) = appel(&app, "POST", "/api/v1/auth/enable", None, Some(m)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(active(&state));
}

/// La route d'écriture : authentification coupée, le réseau ne peut ni
/// l'activer ni poser le secret des jetons.
#[tokio::test]
async fn auth_config_refuse_le_reseau_quand_coupee() {
    let state = new_state();
    let app_local = app(&state, LOCAL);
    let (st, _, _) = appel(
        &app_local,
        "POST",
        "/api/v1/auth/enable",
        None,
        Some(creer("alice", "motdepasse-1", None)),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, v, _) = appel(
        &app_local,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username":"alice","password":"motdepasse-1"})),
    )
    .await;
    let jeton = v["token"].as_str().unwrap().to_string();
    let (st, _, _) = appel(
        &app_local,
        "POST",
        "/api/v1/auth/config",
        Some(&jeton),
        Some(json!({"auth_enabled": false})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let secret_avant = reglage(&state, "jwt_secret");

    let reseau = app(&state, "192.168.1.45:4000");
    let (st, v, _) = appel(
        &reseau,
        "POST",
        "/api/v1/auth/config",
        None,
        Some(json!({"auth_enabled": true})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"], "use_enable_flow");
    let (st, _, _) = appel(
        &reseau,
        "POST",
        "/api/v1/auth/config",
        None,
        Some(json!({"jwt_secret": "impose"})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(!active(&state));
    assert_eq!(reglage(&state, "jwt_secret"), secret_avant);

    // Contre-épreuve : depuis cette machine, l'admin avec mot de passe
    // existant, la même requête active.
    let (st, v, _) = appel(
        &app_local,
        "POST",
        "/api/v1/auth/config",
        None,
        Some(json!({"auth_enabled": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(active(&state));
}

/// Désactiver : un administrateur authentifié seulement.
#[tokio::test]
async fn desactiver_reserve_a_l_admin() {
    let state = new_state();
    let app = app(&state, LOCAL);
    let (st, v, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/enable",
        None,
        Some(creer("alice", "motdepasse-1", None)),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let admin = v["token"].as_str().unwrap().to_string();
    let (st, _, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/register",
        Some(&admin),
        Some(json!({"username":"carole","password":"motdepasse-2"})),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED);
    let (_, v, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username":"carole","password":"motdepasse-2"})),
    )
    .await;
    let ordinaire = v["token"].as_str().unwrap().to_string();

    let (st, _, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/config",
        None,
        Some(json!({"auth_enabled": false})),
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/config",
        Some(&ordinaire),
        Some(json!({"auth_enabled": false})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(active(&state));
    // Déjà active : /auth/enable ne refait rien (et exige une session).
    let (st, _, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/enable",
        Some(&admin),
        Some(creer("eve", "motdepasse-3", None)),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);

    let (st, _, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/config",
        Some(&admin),
        Some(json!({"auth_enabled": false})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(!active(&state));
}

/// Serveur ouvert : une connexion de `default` depuis le réseau ne fixe plus
/// son mot de passe (sinon l'administrateur serait pris d'avance).
#[tokio::test]
async fn le_reseau_ne_fixe_pas_le_mot_de_passe_de_default() {
    let state = new_state();
    let reseau = app(&state, "192.168.1.46:4000");
    let (st, _, _) = appel(
        &reseau,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username":"default","password":"pris-d-avance"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, cfg, _) = appel(&reseau, "GET", "/api/v1/auth/config", None, None).await;
    assert_eq!(cfg["admin_with_password"], false, "{cfg}");
    assert_eq!(cfg["this_machine"], false);
    // Témoin : depuis cette machine, la première connexion le fixe.
    let local = app(&state, LOCAL);
    let (st, _, _) = appel(
        &local,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(json!({"username":"default","password":"choisi-ici"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (_, cfg, _) = appel(&local, "GET", "/api/v1/auth/config", None, None).await;
    assert_eq!(cfg["admin_with_password"], true);
    assert_eq!(cfg["this_machine"], true);
}

/// GET /auth/config, auth active : un anonyme ne voit pas le dossier de la base.
#[tokio::test]
async fn le_dossier_n_est_pas_montre_a_un_anonyme() {
    let state = new_state();
    let app = app(&state, LOCAL);
    let (_, cfg, _) = appel(&app, "GET", "/api/v1/auth/config", None, None).await;
    assert!(cfg["data_dir"].is_string(), "{cfg}");
    let (_, v, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/enable",
        None,
        Some(creer("alice", "motdepasse-1", None)),
    )
    .await;
    let jeton = v["token"].as_str().unwrap().to_string();
    let (_, cfg, _) = appel(&app, "GET", "/api/v1/auth/config", None, None).await;
    assert!(cfg["data_dir"].is_null(), "{cfg}");
    let (_, cfg, _) = appel(&app, "GET", "/api/v1/auth/config", Some(&jeton), None).await;
    assert!(cfg["data_dir"].is_string(), "{cfg}");
}

/// Voie de secours : le fichier coupe l'authentification et disparaît.
#[tokio::test]
async fn le_fichier_de_secours_coupe_l_authentification() {
    let state = new_state();
    let app = app(&state, LOCAL);
    let (st, _, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/enable",
        None,
        Some(creer("alice", "motdepasse-1", None)),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    // Témoin : sans fichier, rien ne change.
    assert!(!appliquer_le_fichier_de_secours(&state));
    assert!(active(&state));
    let fichier = dossier(&state).join(FICHIER_DE_SECOURS);
    std::fs::write(&fichier, "").unwrap();
    assert!(appliquer_le_fichier_de_secours(&state));
    assert!(!active(&state));
    assert!(!fichier.exists());
}

/// Tune OS : un fichier de secours sur une partition en lecture seule (la
/// carte SD) ne coupe l'authentification qu'UNE fois, pas à chaque démarrage.
#[cfg(unix)]
#[tokio::test]
async fn fichier_de_secours_ineffacable_applique_une_seule_fois() {
    use std::os::unix::fs::PermissionsExt;
    let state = new_state();
    let app = app(&state, LOCAL);
    let (st, _, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/enable",
        None,
        Some(creer("alice", "motdepasse-1", None)),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let boot = tempfile::tempdir().unwrap();
    let fichier = boot.path().join(FICHIER_DE_SECOURS);
    std::fs::write(&fichier, "").unwrap();
    std::fs::set_permissions(boot.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    let dossiers = vec![boot.path().to_path_buf()];

    assert!(appliquer_le_fichier_de_secours_dans(&state, &dossiers));
    assert!(!active(&state));
    assert!(fichier.exists(), "lecture seule : le fichier reste");
    // L'admin réactive ; au démarrage suivant, le même fichier ne coupe plus.
    SettingsRepo::with_backend(state.backend.clone())
        .set("auth_enabled", "true")
        .unwrap();
    assert!(!appliquer_le_fichier_de_secours_dans(&state, &dossiers));
    assert!(active(&state));
    // Contre-épreuve : un NOUVEAU fichier (autre contenu) coupe de nouveau.
    std::fs::set_permissions(boot.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(&fichier, "encore").unwrap();
    std::fs::set_permissions(boot.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
    assert!(appliquer_le_fichier_de_secours_dans(&state, &dossiers));
    assert!(!active(&state));
    std::fs::set_permissions(boot.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
}
