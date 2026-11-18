//! Activation de l'authentification depuis l'interface (`/auth/activation`).
//!
//! Tant que l'authentification est coupée, celui qui l'active choisit le mot
//! de passe du compte administrateur : seulement depuis cette machine, ou avec
//! le code à usage unique écrit dans le journal du serveur. Une fois active,
//! l'écran ne recrée plus d'administrateur.

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const CETTE_MACHINE: &str = "127.0.0.1:50000";
const AUTRE_HOTE_DU_LAN: &str = "192.168.1.50:50000";

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn active(state: &AppState) -> bool {
    tune_server::auth::authentification_active(state)
}

/// Le mot de passe enregistré du compte administrateur (profil 1).
fn mot_de_passe_admin(state: &AppState) -> Option<String> {
    state
        .backend
        .query_one("SELECT password_hash_v2 FROM profiles WHERE id = 1", &[])
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_string()))
        .filter(|h| !h.is_empty())
}

async fn appel(
    state: &AppState,
    adresse: &str,
    methode: Method,
    chemin: &str,
    jeton: Option<&str>,
    corps: Option<&str>,
) -> (StatusCode, Value) {
    let pair: SocketAddr = adresse.parse().unwrap();
    let app: Router = tune_server::routes::router(state.clone()).layer(MockConnectInfo(pair));
    let mut req = Request::builder().method(methode).uri(chemin);
    if let Some(j) = jeton {
        req = req.header(header::AUTHORIZATION, format!("Bearer {j}"));
    }
    let body = match corps {
        Some(c) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = serde_json::from_slice(&octets)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&octets).to_string()));
    (status, v)
}

/// Ce que l'exploitant lit sur la console : le code en cours, émis s'il le
/// faut. Aucune route ne le rend.
fn code_de_la_console(state: &AppState) -> String {
    let _ = tune_server::auth::emettre_code_activation(state);
    tune_server::auth::code_activation_en_cours(state).expect("un code en cours")
}

async fn activer(state: &AppState, adresse: &str, corps: &str) -> (StatusCode, Value) {
    appel(
        state,
        adresse,
        Method::POST,
        "/api/v1/auth/activation",
        None,
        Some(corps),
    )
    .await
}

#[tokio::test]
async fn l_etat_dit_si_un_code_est_necessaire() {
    let state = new_state();
    let (st, v) = appel(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::GET,
        "/api/v1/auth/activation",
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["auth_enabled"], false);
    assert_eq!(v["from_this_machine"], false);
    assert_eq!(v["code_required"], true);
    assert_eq!(v["admin_username"], "default");
    let (_, v) = appel(
        &state,
        CETTE_MACHINE,
        Method::GET,
        "/api/v1/auth/activation",
        None,
        None,
    )
    .await;
    assert_eq!(v["from_this_machine"], true);
    assert_eq!(v["code_required"], false);
}

#[tokio::test]
async fn un_tiers_du_lan_sans_code_est_refuse() {
    let state = new_state();
    let (st, v) = activer(&state, AUTRE_HOTE_DU_LAN, r#"{"password":"tiers-1234"}"#).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"], "activation_code_required", "{v}");
    assert!(!active(&state));
    assert_eq!(mot_de_passe_admin(&state), None);
    // Relayé (vu de 127.0.0.1, émis ailleurs) : pas « cette machine ».
    let app = tune_server::routes::router(state.clone()).layer(MockConnectInfo(
        CETTE_MACHINE.parse::<SocketAddr>().unwrap(),
    ));
    let req = Request::post("/api/v1/auth/activation")
        .header(header::CONTENT_TYPE, "application/json")
        .header("x-tune-relais", "1")
        .body(Body::from(r#"{"password":"tiers-1234"}"#))
        .unwrap();
    let st = app.oneshot(req).await.unwrap().status();
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(!active(&state));
}

#[tokio::test]
async fn un_code_faux_est_refuse_et_cinq_essais_detruisent_le_code() {
    let state = new_state();
    // La route écrit le code dans le journal et ne le rend pas.
    let (st, v) = appel(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::POST,
        "/api/v1/auth/activation/code",
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(
        v.get("code").is_none(),
        "le code ne sort jamais par l'API : {v}"
    );

    // Ce que l'exploitant lit dans le journal.
    let code = code_de_la_console(&state);
    let (st, v) = activer(
        &state,
        AUTRE_HOTE_DU_LAN,
        r#"{"password":"tiers-1234","code":"AAAAAAAAAA"}"#,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"], "activation_code_invalid", "{v}");
    assert!(!active(&state));
    for _ in 1..tune_server::auth::ESSAIS_CODE_ACTIVATION {
        activer(
            &state,
            AUTRE_HOTE_DU_LAN,
            r#"{"password":"tiers-1234","code":"BBBBBBBBBB"}"#,
        )
        .await;
    }
    // Le code juste ne vaut plus rien après cinq essais faux.
    let corps = format!(r#"{{"password":"proprio-1234","code":"{code}"}}"#);
    let (st, v) = activer(&state, AUTRE_HOTE_DU_LAN, &corps).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert!(!active(&state));
    assert_eq!(mot_de_passe_admin(&state), None);
}

#[tokio::test]
async fn un_mot_de_passe_trop_court_ne_consomme_pas_le_code() {
    let state = new_state();
    let code = code_de_la_console(&state);
    let corps = format!(r#"{{"password":"court","code":"{code}"}}"#);
    let (st, v) = activer(&state, AUTRE_HOTE_DU_LAN, &corps).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    let corps = format!(
        r#"{{"password":"proprio-1234","code":"{}"}}"#,
        code.to_lowercase()
    );
    let (st, v) = activer(&state, AUTRE_HOTE_DU_LAN, &corps).await;
    assert_eq!(st, StatusCode::OK, "{v}");
}

#[tokio::test]
async fn le_code_juste_active_et_ne_sert_qu_une_fois() {
    let state = new_state();
    let code = code_de_la_console(&state);
    let corps = format!(r#"{{"password":"proprio-1234","code":"{code}"}}"#);
    let (st, v) = activer(&state, AUTRE_HOTE_DU_LAN, &corps).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["auth_enabled"], true);
    assert_eq!(v["role"], "admin");
    assert!(active(&state));
    let jeton = v["token"].as_str().unwrap().to_string();

    // Le jeton rendu est celui d'un administrateur.
    let (st, v) = appel(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::GET,
        "/api/v1/auth/me",
        Some(&jeton),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // Le mot de passe choisi ouvre la session administrateur.
    let (st, v) = appel(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::POST,
        "/api/v1/auth/login",
        None,
        Some(r#"{"username":"default","password":"proprio-1234"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["role"], "admin");

    // Une fois active, l'écran ne recrée plus d'administrateur : sans jeton
    // 401, avec le jeton administrateur 409, même code, même machine.
    let (st, _) = activer(&state, AUTRE_HOTE_DU_LAN, &corps).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, _) = activer(&state, CETTE_MACHINE, r#"{"password":"autre-1234"}"#).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    let (st, v) = appel(
        &state,
        CETTE_MACHINE,
        Method::POST,
        "/api/v1/auth/activation",
        Some(&jeton),
        Some(r#"{"password":"autre-1234"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], "auth_already_enabled");
    let (st, _) = appel(
        &state,
        CETTE_MACHINE,
        Method::POST,
        "/api/v1/auth/activation/code",
        Some(&jeton),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT);
    // L'état reste lisible sans jeton.
    let (st, v) = appel(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::GET,
        "/api/v1/auth/activation",
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["auth_enabled"], true);
    assert!(v["admin_username"].is_null());
}

#[tokio::test]
async fn depuis_cette_machine_aucun_code_n_est_demande() {
    for adresse in [CETTE_MACHINE, "[::1]:50000"] {
        let state = new_state();
        let (st, v) = activer(&state, adresse, r#"{"password":"proprio-1234"}"#).await;
        assert_eq!(st, StatusCode::OK, "{adresse} : {v}");
        assert!(active(&state));
        assert!(mot_de_passe_admin(&state).is_some());
    }
}

#[tokio::test]
async fn les_jetons_emis_avant_l_activation_ne_valent_plus_rien() {
    let state = new_state();
    // Tout est ouvert : un tiers du LAN se fabrique un jeton et une clé.
    let (st, v) = appel(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::POST,
        "/api/v1/auth/token",
        None,
        Some("{}"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let jeton_ancien = v["token"].as_str().unwrap().to_string();
    let (st, v) = appel(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::POST,
        "/api/v1/auth/login",
        None,
        Some(r#"{"username":"default","password":"tiers-1234"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let session_ancienne = v["token"].as_str().unwrap().to_string();
    let (st, v) = appel(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::POST,
        "/api/v1/auth/api-key",
        None,
        Some("{}"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let cle = v["key"].as_str().unwrap().to_string();

    let (st, v) = activer(&state, CETTE_MACHINE, r#"{"password":"proprio-1234"}"#).await;
    assert_eq!(st, StatusCode::OK, "{v}");

    for j in [&jeton_ancien, &session_ancienne] {
        let (st, _) = appel(
            &state,
            AUTRE_HOTE_DU_LAN,
            Method::GET,
            "/api/v1/zones",
            Some(j),
            None,
        )
        .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }
    let app = tune_server::routes::router(state.clone()).layer(MockConnectInfo(
        AUTRE_HOTE_DU_LAN.parse::<SocketAddr>().unwrap(),
    ));
    let req = Request::get("/api/v1/zones")
        .header(header::AUTHORIZATION, format!("ApiKey {cle}"))
        .header("x-api-key", cle.as_str())
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(req).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        SettingsRepo::with_backend(state.backend.clone())
            .get("api_key")
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn le_profil_default_n_est_pas_reclame_depuis_le_lan() {
    let state = new_state();
    let (st, v) = appel(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::POST,
        "/api/v1/auth/login",
        None,
        Some(r#"{"username":"default","password":"tiers-1234"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        mot_de_passe_admin(&state),
        None,
        "le mot de passe du tiers n'est pas enregistré"
    );
}

#[tokio::test]
async fn auth_config_n_active_que_depuis_cette_machine() {
    let state = new_state();
    for corps in [
        r#"{"auth_enabled":true}"#,
        r#"{"jwt_secret":"secret-du-tiers"}"#,
    ] {
        let (st, v) = appel(
            &state,
            AUTRE_HOTE_DU_LAN,
            Method::POST,
            "/api/v1/auth/config",
            None,
            Some(corps),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{corps} : {v}");
        assert!(!active(&state));
    }
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    assert_ne!(
        reglages.get("jwt_secret").unwrap().as_deref(),
        Some("secret-du-tiers")
    );
    let (st, v) = appel(
        &state,
        CETTE_MACHINE,
        Method::POST,
        "/api/v1/auth/config",
        None,
        Some(r#"{"auth_enabled":true}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(active(&state));
}

fn existe(state: &AppState, nom: &str) -> bool {
    state
        .backend
        .query_one(
            "SELECT COUNT(*) FROM profiles WHERE username = ?",
            &[&nom as &dyn tune_core::db::backend::ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(0)
        > 0
}

#[tokio::test]
async fn un_tiers_du_lan_ne_cree_plus_de_compte_la_machine_si() {
    let state = new_state();
    let corps = r#"{"username":"intrus","password":"tiers-1234"}"#;
    let (st, v) = appel(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::POST,
        "/api/v1/auth/register",
        None,
        Some(corps),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["reason"], "not_local");
    assert!(!existe(&state, "intrus"));
    let (st, v) = appel(
        &state,
        CETTE_MACHINE,
        Method::POST,
        "/api/v1/auth/register",
        None,
        Some(r#"{"username":"famille","password":"famille-1234"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    assert!(existe(&state, "famille"));
}

/// Un compte créé avant l'activation (par l'ancien serveur, ou depuis la
/// machine) est listé ; l'activation peut le supprimer, jamais l'administrateur.
#[tokio::test]
async fn l_activation_supprime_les_comptes_non_reconnus_et_garde_l_admin() {
    let state = new_state();
    for nom in ["etranger", "famille"] {
        let corps = format!(r#"{{"username":"{nom}","password":"compte-1234"}}"#);
        let (st, v) = appel(
            &state,
            CETTE_MACHINE,
            Method::POST,
            "/api/v1/auth/register",
            None,
            Some(&corps),
        )
        .await;
        assert_eq!(st, StatusCode::CREATED, "{v}");
    }
    // Un profil d'écoute sans mot de passe n'est pas un compte : non listé.
    tune_core::db::profile_repo::ProfileRepo::with_backend(state.backend.clone())
        .create("salon", None, None)
        .unwrap();

    let (_, v) = appel(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::GET,
        "/api/v1/auth/activation",
        None,
        None,
    )
    .await;
    let comptes = v["accounts"].as_array().unwrap().clone();
    let noms: Vec<&str> = comptes
        .iter()
        .filter_map(|c| c["username"].as_str())
        .collect();
    assert_eq!(noms, ["default", "etranger", "famille"], "{v}");
    let admin = comptes.iter().find(|c| c["username"] == "default").unwrap();
    assert_eq!(admin["role"], "admin");
    assert_eq!(admin["deletable"], false);
    assert!(comptes.iter().all(|c| c.get("created_at").is_some()));
    let id_etranger = comptes
        .iter()
        .find(|c| c["username"] == "etranger")
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let id_admin = admin["id"].as_i64().unwrap();

    // L'administrateur ne se supprime jamais.
    let corps = format!(r#"{{"password":"proprio-1234","delete_accounts":[{id_admin}]}}"#);
    let (st, v) = activer(&state, CETTE_MACHINE, &corps).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"], "account_not_deletable");
    assert!(!active(&state));

    let corps = format!(r#"{{"password":"proprio-1234","delete_accounts":[{id_etranger}]}}"#);
    let (st, v) = activer(&state, CETTE_MACHINE, &corps).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["deleted_accounts"], serde_json::json!([id_etranger]));
    assert!(active(&state));
    assert!(!existe(&state, "etranger"));
    assert!(existe(&state, "famille"));
    assert!(existe(&state, "default"));
    assert!(existe(&state, "salon"));
    let (st, _) = appel(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::POST,
        "/api/v1/auth/login",
        None,
        Some(r#"{"username":"default","password":"proprio-1234"}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
}

/// Un nouvel appel ne remplace pas un code valable et ne remet pas ses essais
/// à zéro ; au plus une émission par minute ; échecs comptés par adresse.
#[tokio::test]
async fn le_code_en_cours_n_est_pas_remplace_et_les_echecs_comptent_par_adresse() {
    let state = new_state();
    let demander = || {
        appel(
            &state,
            AUTRE_HOTE_DU_LAN,
            Method::POST,
            "/api/v1/auth/activation/code",
            None,
            None,
        )
    };
    let (st, v) = demander().await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "code_written_to_server_console");
    let code_a = tune_server::auth::code_activation_en_cours(&state).unwrap();
    let faux = r#"{"password":"tiers-1234","code":"BBBBBBBBBB"}"#;
    for _ in 0..3 {
        let (st, _) = activer(&state, AUTRE_HOTE_DU_LAN, faux).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
    }
    // Redemander ne remplace rien et ne rend pas d'essais.
    let (st, v) = demander().await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "code_already_pending");
    assert_eq!(
        tune_server::auth::code_activation_en_cours(&state).as_deref(),
        Some(code_a.as_str())
    );
    for _ in 0..2 {
        activer(&state, AUTRE_HOTE_DU_LAN, faux).await;
    }
    assert_eq!(
        tune_server::auth::code_activation_en_cours(&state),
        None,
        "5 essais : détruit"
    );
    let juste_a = format!(r#"{{"password":"tiers-1234","code":"{code_a}"}}"#);
    let (st, _) = activer(&state, AUTRE_HOTE_DU_LAN, &juste_a).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    // Moins d'une minute après : pas de nouveau code.
    let (st, v) = demander().await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{v}");

    // Nouveau code (délai oublié pour l'épreuve) : l'adresse fautive a déjà
    // 6 échecs ; deux de plus et elle est bloquée, même avec le bon code.
    tune_server::auth::oublier_le_delai_d_emission(&state);
    let code_b = code_de_la_console(&state);
    for _ in 0..2 {
        activer(&state, AUTRE_HOTE_DU_LAN, faux).await;
    }
    let juste_b = format!(r#"{{"password":"proprio-1234","code":"{code_b}"}}"#);
    let (st, v) = activer(&state, AUTRE_HOTE_DU_LAN, &juste_b).await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{v}");
    assert_eq!(v["error"], "activation_attempts_exceeded");
    assert!(!active(&state));
    // Une autre adresse, avec le bon code, active.
    let (st, v) = activer(&state, "192.168.1.77:50000", &juste_b).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(active(&state));
}

/// Une liste de comptes invalide ne consomme pas le code.
#[tokio::test]
async fn une_liste_de_comptes_invalide_ne_brule_pas_le_code() {
    let state = new_state();
    let code = code_de_la_console(&state);
    let corps = format!(r#"{{"password":"proprio-1234","code":"{code}","delete_accounts":[1]}}"#);
    let (st, v) = activer(&state, AUTRE_HOTE_DU_LAN, &corps).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(
        tune_server::auth::code_activation_en_cours(&state).as_deref(),
        Some(code.as_str())
    );
}

/// Les comptes liés à Mozaiklabs (SSO) sont listés ; le propriétaire de la
/// licence ne se supprime pas.
#[tokio::test]
async fn la_revue_montre_les_comptes_sso_et_protege_le_proprietaire_de_la_licence() {
    let state = new_state();
    use tune_core::db::backend::ToSqlValue;
    for courriel in ["proprio@example.org", "ami@example.org"] {
        state
            .backend
            .execute(
                "INSERT INTO profiles (username, display_name, email) VALUES (?, ?, ?)",
                &[
                    &courriel as &dyn ToSqlValue,
                    &courriel as &dyn ToSqlValue,
                    &courriel as &dyn ToSqlValue,
                ],
            )
            .unwrap();
    }
    let id_de = |nom: &str| {
        state
            .backend
            .query_one(
                "SELECT id FROM profiles WHERE username = ?",
                &[&nom as &dyn ToSqlValue],
            )
            .unwrap()
            .unwrap()[0]
            .as_i64()
            .unwrap()
    };
    let proprio = id_de("proprio@example.org");
    let ami = id_de("ami@example.org");
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            tune_core::cloud::proprietaire::CLE_PROPRIETAIRE,
            &proprio.to_string(),
        )
        .unwrap();
    let (_, v) = appel(
        &state,
        CETTE_MACHINE,
        Method::GET,
        "/api/v1/auth/activation",
        None,
        None,
    )
    .await;
    let comptes = v["accounts"].as_array().unwrap();
    let fiche = |id: i64| comptes.iter().find(|c| c["id"] == id).cloned().unwrap();
    assert_eq!(fiche(proprio)["kind"], "sso");
    assert_eq!(fiche(proprio)["license_owner"], true);
    assert_eq!(fiche(proprio)["deletable"], false);
    assert_eq!(fiche(ami)["deletable"], true);
    let corps = format!(r#"{{"password":"proprio-1234","delete_accounts":[{proprio}]}}"#);
    let (st, v) = activer(&state, CETTE_MACHINE, &corps).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    let corps = format!(r#"{{"password":"proprio-1234","delete_accounts":[{ami}]}}"#);
    let (st, v) = activer(&state, CETTE_MACHINE, &corps).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(!existe(&state, "ami@example.org"));
    assert!(existe(&state, "proprio@example.org"));
}

/// L'activation ferme les WebSocket ouvertes pendant que tout était ouvert.
#[tokio::test]
async fn l_activation_ferme_les_websockets() {
    let state = new_state();
    let mut abonne = state.fermeture_websockets.subscribe();
    assert!(!abonne.has_changed().unwrap());
    let (st, _) = activer(&state, CETTE_MACHINE, r#"{"password":"proprio-1234"}"#).await;
    assert_eq!(st, StatusCode::OK);
    assert!(abonne.has_changed().unwrap());
}

/// Une connexion SSO commencée avant l'activation ne se termine pas après ;
/// une connexion en attente depuis plus de dix minutes non plus.
#[tokio::test]
async fn la_connexion_sso_en_attente_ne_survit_ni_a_l_activation_ni_au_temps() {
    let reglages = |s: &AppState| SettingsRepo::with_backend(s.backend.clone());
    let etat_pkce = |s: &AppState| -> Option<String> {
        reglages(s)
            .get("mozaik_pkce_pending")
            .unwrap()
            .filter(|v| !v.is_empty())
            .and_then(|v| serde_json::from_str::<Value>(&v).ok())
            .and_then(|v| v["state"].as_str().map(str::to_string))
    };
    // 1. Commencée avant l'activation.
    let state = new_state();
    let (st, _) = appel(
        &state,
        CETTE_MACHINE,
        Method::GET,
        "/api/v1/cloud/sso/authorize",
        None,
        None,
    )
    .await;
    assert!(st.is_redirection(), "{st}");
    let etat = etat_pkce(&state).expect("connexion en attente");
    let (st, v) = activer(&state, CETTE_MACHINE, r#"{"password":"proprio-1234"}"#).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(etat_pkce(&state), None);
    // Même avec la session administrateur (le retour exige la session qui a
    // lancé la liaison), l'attente d'avant l'activation n'existe plus.
    let admin = v["token"].as_str().unwrap().to_string();
    let chemin = format!("/api/v1/cloud/sso/callback?code=x&state={etat}");
    let (st, v) = appel(&state, AUTRE_HOTE_DU_LAN, Method::GET, &chemin, Some(&admin), None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(v.to_string().contains("no pending SSO session"), "{v}");

    // 2. Plus de dix minutes d'attente.
    let state = new_state();
    appel(
        &state,
        CETTE_MACHINE,
        Method::GET,
        "/api/v1/cloud/sso/authorize",
        None,
        None,
    )
    .await;
    let etat = etat_pkce(&state).expect("connexion en attente");
    let vieux = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 11 * 60;
    reglages(&state)
        .set("mozaik_pkce_pending_at", &vieux.to_string())
        .unwrap();
    let chemin = format!("/api/v1/cloud/sso/callback?code=x&state={etat}");
    let (st, v) = appel(&state, CETTE_MACHINE, Method::GET, &chemin, None, None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert!(v.to_string().contains("no pending SSO session"), "{v}");
}

/// Compte standard (authentification active) : sa liaison de profil en
/// attente expire aussi au bout de dix minutes, et l'activation n'en laisse
/// aucune.
#[tokio::test]
async fn la_connexion_sso_d_un_profil_en_attente_expire() {
    let state = new_state();
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    reglages.set("auth_enabled", "true").unwrap();
    reglages.set("jwt_secret", "secret-de-l-epreuve").unwrap();
    let jeton = tune_server::auth::sign_jwt(2, "user", "secret-de-l-epreuve").unwrap();
    let (st, _) = appel(&state, AUTRE_HOTE_DU_LAN, Method::GET, "/api/v1/cloud/sso/authorize", Some(&jeton), None).await;
    assert!(st.is_redirection(), "{st}");
    let pkce = reglages.get("mozaik_pkce_pending_profil_2").unwrap().expect("attente du profil 2");
    let etat = serde_json::from_str::<Value>(&pkce).unwrap()["state"].as_str().unwrap().to_string();
    assert!(reglages.get("mozaik_pkce_pending_profil_2_at").unwrap().is_some(), "attente datée");
    let vieux = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 11 * 60;
    reglages.set("mozaik_pkce_pending_profil_2_at", &vieux.to_string()).unwrap();
    let chemin = format!("/api/v1/cloud/sso/callback?code=x&state={etat}");
    let (st, v) = appel(&state, AUTRE_HOTE_DU_LAN, Method::GET, &chemin, Some(&jeton), None).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert!(v.to_string().contains("no pending SSO session"), "{v}");
    assert_eq!(reglages.get("mozaik_pkce_pending_profil_2_at").unwrap(), None);
}

/// L'activation efface aussi les liaisons de PROFIL en attente.
#[tokio::test]
async fn l_activation_efface_les_attentes_sso_des_profils() {
    let state = new_state();
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    for cle in [
        "mozaik_pkce_pending_profil_7",
        "mozaik_pkce_pending_profil_7_at",
        "mozaik_redirect_uri_pending_profil_7",
    ] {
        reglages.set(cle, "x").unwrap();
    }
    let (st, _) = activer(&state, CETTE_MACHINE, r#"{"password":"proprio-1234"}"#).await;
    assert_eq!(st, StatusCode::OK);
    for cle in [
        "mozaik_pkce_pending_profil_7",
        "mozaik_pkce_pending_profil_7_at",
        "mozaik_redirect_uri_pending_profil_7",
    ] {
        assert_eq!(reglages.get(cle).unwrap(), None, "{cle}");
    }
}

/// Derrière un mandataire inverse (connexion vue de 127.0.0.1), la connexion
/// n'est plus « l'opérateur local » : limite de tentatives appliquée, profil
/// `default` sans mot de passe refusé une fois l'authentification active.
#[tokio::test]
async fn derriere_un_mandataire_la_connexion_n_est_pas_l_operateur_local() {
    let state = new_state();
    let app = tune_server::routes::router(state.clone()).layer(MockConnectInfo(
        CETTE_MACHINE.parse::<SocketAddr>().unwrap(),
    ));
    let connexion = |corps: &'static str| {
        Request::post("/api/v1/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-forwarded-for", "203.0.113.9")
            .body(Body::from(corps))
            .unwrap()
    };
    // Authentification active, profil `default` sans mot de passe.
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set("auth_enabled", "true").unwrap();
    s.set("jwt_secret", "secret-de-l-epreuve").unwrap();
    let st = app
        .clone()
        .oneshot(connexion(
            r#"{"username":"default","password":"tiers-1234"}"#,
        ))
        .await
        .unwrap()
        .status();
    assert_eq!(
        st,
        StatusCode::UNAUTHORIZED,
        "default sans mot de passe via mandataire"
    );
    // La limite s'applique (10 échecs, puis 429).
    let mut dernier = StatusCode::OK;
    for _ in 0..11 {
        dernier = app
            .clone()
            .oneshot(connexion(
                r#"{"username":"inconnu","password":"faux-faux-1"}"#,
            ))
            .await
            .unwrap()
            .status();
    }
    assert_eq!(dernier, StatusCode::TOO_MANY_REQUESTS);
}

/// À l'activation, le jeton du pont (écoute à distance) change : le lien
/// d'accès obtenu pendant que tout était ouvert ne sert plus. Même chose par
/// `POST /auth/config` depuis cette machine.
#[tokio::test]
async fn l_activation_renouvelle_le_jeton_du_pont() {
    for par_la_config in [false, true] {
        let state = new_state();
        let reglages = SettingsRepo::with_backend(state.backend.clone());
        reglages.set("bridge_enabled", "true").unwrap();
        reglages
            .set("bridge_token", "ANCIEN-jeton-de-pont-FAUX")
            .unwrap();
        let (st, v) = if par_la_config {
            appel(
                &state,
                CETTE_MACHINE,
                Method::POST,
                "/api/v1/auth/config",
                None,
                Some(r#"{"auth_enabled":true}"#),
            )
            .await
        } else {
            activer(&state, CETTE_MACHINE, r#"{"password":"proprio-1234"}"#).await
        };
        assert_eq!(st, StatusCode::OK, "{v}");
        let jeton = reglages.get("bridge_token").unwrap().unwrap_or_default();
        assert!(!jeton.is_empty(), "un jeton de pont reste posé");
        assert_ne!(jeton, "ANCIEN-jeton-de-pont-FAUX", "config={par_la_config}");
        if !par_la_config {
            let admin = v["token"].as_str().unwrap().to_string();
            let (_, lien) = appel(
                &state,
                CETTE_MACHINE,
                Method::GET,
                "/api/v1/cloud/bridge/access-link",
                Some(&admin),
                None,
            )
            .await;
            assert!(
                !lien.to_string().contains("ANCIEN-jeton-de-pont-FAUX"),
                "{lien}"
            );
        }
    }
    // Sans pont configuré, rien n'est inventé.
    let state = new_state();
    activer(&state, CETTE_MACHINE, r#"{"password":"proprio-1234"}"#).await;
    assert_eq!(
        SettingsRepo::with_backend(state.backend.clone())
            .get("bridge_token")
            .unwrap(),
        None
    );
}
