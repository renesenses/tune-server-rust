//! rc13.2 — connexion au compte `default`.
//!
//! Le compte n'est « sans mot de passe » (première connexion, le mot de passe
//! saisi devient le sien) que si AUCUN hachage n'est enregistré, ni l'ancien
//! (`password_hash`, SHA-256) ni le nouveau (`password_hash_v2`, argon2).
//! Dès qu'un mot de passe existe, seul ce mot de passe ouvre la session, sur
//! cette machine comme depuis le réseau local, et un essai refusé ne change
//! rien à ce qui est enregistré. L'ancien hachage n'est converti qu'après
//! avoir été vérifié.
//!
//! Le refus porte `error` (inchangé, comparé par les clients) et un `message`
//! lisible, dans la langue de la requête.
//!
//! rc14 : le profil 1 s'appelle `admin` après la migration 158 (PG 124), et
//! `default` en reste l'alias à la connexion. Les scénarios saisissent
//! toujours `default` : ils prouvent l'alias ; `alias_default_du_profil_admin`
//! couvre `admin` et les bords.
//!
//! SQLite toujours ; PostgreSQL en plus quand `TUNE_TEST_PG_URL` est posée
//! (feature `postgres`).

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const CETTE_MACHINE: &str = "127.0.0.1:50000";
const MOT_DE_PASSE: &str = "proprio-1234";
const MAUVAIS: &str = "intrus-9999";

fn etat_sqlite() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

/// Les hachages enregistrés du compte `default` : (ancien, nouveau).
fn hachages(state: &AppState) -> (Option<String>, Option<String>) {
    let r = state
        .backend
        .query_one(
            "SELECT password_hash, password_hash_v2 FROM profiles WHERE id = 1",
            &[],
        )
        .unwrap()
        .expect("profil default");
    (
        r.first().and_then(|v| v.as_string()),
        r.get(1).and_then(|v| v.as_string()),
    )
}

fn poser_hachages(state: &AppState, ancien: Option<&str>, nouveau: Option<&str>) {
    let ancien = ancien.map(str::to_string);
    let nouveau = nouveau.map(str::to_string);
    state
        .backend
        .execute(
            "UPDATE profiles SET password_hash = ?, password_hash_v2 = ? WHERE id = 1",
            &[&ancien as &dyn ToSqlValue, &nouveau as &dyn ToSqlValue],
        )
        .unwrap();
}

fn poser_authentification(state: &AppState, active: bool) {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set("auth_enabled", if active { "true" } else { "false" })
        .unwrap();
}

fn sha256(mot: &str) -> String {
    format!("{:x}", Sha256::digest(mot.as_bytes()))
}

async fn appel(
    state: &AppState,
    adresse: &str,
    chemin: &str,
    corps: &str,
    langue: Option<&str>,
) -> (StatusCode, Value) {
    let pair: SocketAddr = adresse.parse().unwrap();
    let app: Router = tune_server::routes::router(state.clone()).layer(MockConnectInfo(pair));
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(chemin)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(l) = langue {
        req = req.header(header::ACCEPT_LANGUAGE, l);
    }
    let resp = app
        .oneshot(req.body(Body::from(corps.to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = serde_json::from_slice(&octets)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&octets).to_string()));
    (status, v)
}

async fn connexion(state: &AppState, adresse: &str, nom: &str, mot: &str) -> (StatusCode, Value) {
    let corps = serde_json::json!({ "username": nom, "password": mot }).to_string();
    appel(state, adresse, "/api/v1/auth/login", &corps, None).await
}

async fn activer(state: &AppState) {
    let corps = serde_json::json!({ "password": MOT_DE_PASSE }).to_string();
    let (st, v) = appel(
        state,
        CETTE_MACHINE,
        "/api/v1/auth/activation",
        &corps,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "activation : {v}");
    assert!(tune_server::auth::authentification_active(state));
}

/// Le mot de passe d'activation est-il encore celui du compte ?
fn mot_de_passe_enregistre(state: &AppState, mot: &str) -> bool {
    hachages(state)
        .1
        .as_deref()
        .is_some_and(|h| tune_server::auth::verify_password(mot, h))
}

fn refus_lisible(v: &Value) {
    assert_eq!(v["error"], "invalid credentials", "{v}");
    assert_eq!(v["message"], "Incorrect username or password", "{v}");
}

// ---------------------------------------------------------------------------
// Scénarios, communs aux deux moteurs. `lan` : une adresse du réseau local
// propre au scénario (le compteur d'échecs est par adresse).
// ---------------------------------------------------------------------------

async fn activation_puis_mauvais_mot_de_passe_sur_cette_machine(state: &AppState, lan: &str) {
    activer(state).await;
    let (st, v) = connexion(state, CETTE_MACHINE, "default", MAUVAIS).await;
    assert_eq!(
        st,
        StatusCode::UNAUTHORIZED,
        "mauvais mot de passe accepté : {v}"
    );
    refus_lisible(&v);
    assert!(
        mot_de_passe_enregistre(state, MOT_DE_PASSE),
        "le mot de passe d'activation doit rester celui du compte"
    );
    assert!(
        !mot_de_passe_enregistre(state, MAUVAIS),
        "le mauvais mot de passe a été enregistré"
    );
    // Le vrai mot de passe ouvre toujours la session, ici et depuis le LAN.
    for adresse in [CETTE_MACHINE, lan] {
        let (st, v) = connexion(state, adresse, "default", MOT_DE_PASSE).await;
        assert_eq!(st, StatusCode::OK, "{adresse} : {v}");
        assert_eq!(v["role"], "admin");
    }
    // Un mot de passe vide n'ouvre rien non plus.
    let (st, _) = connexion(state, CETTE_MACHINE, "default", "").await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

async fn activation_puis_mauvais_mot_de_passe_depuis_le_lan(state: &AppState, lan: &str) {
    activer(state).await;
    let (st, v) = connexion(state, lan, "default", MAUVAIS).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
    refus_lisible(&v);
    assert!(mot_de_passe_enregistre(state, MOT_DE_PASSE));
    let (st, v) = connexion(state, lan, "default", MOT_DE_PASSE).await;
    assert_eq!(st, StatusCode::OK, "{v}");
}

/// Authentification coupée, compte vraiment sans mot de passe : comme avant.
/// Sur cette machine, le premier mot de passe saisi devient celui du compte ;
/// depuis le LAN, la session s'ouvre mais rien n'est enregistré.
async fn profil_sans_mot_de_passe_authentification_coupee(state: &AppState, lan: &str) {
    assert_eq!(hachages(state), (None, None));
    let (st, v) = connexion(state, lan, "default", "tiers-1234").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        hachages(state),
        (None, None),
        "rien n'est enregistré depuis le LAN"
    );
    let (st, v) = connexion(state, CETTE_MACHINE, "default", "premier-1234").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(mot_de_passe_enregistre(state, "premier-1234"));
    // Désormais il a un mot de passe : un autre est refusé, sans rien changer.
    let (st, v) = connexion(state, CETTE_MACHINE, "default", "second-1234").await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
    assert!(mot_de_passe_enregistre(state, "premier-1234"));
    let (st, _) = connexion(state, CETTE_MACHINE, "default", "premier-1234").await;
    assert_eq!(st, StatusCode::OK);
}

/// Base rc12 : seul l'ancien hachage SHA-256. Un mauvais mot de passe est
/// refusé partout sans rien convertir ; le bon est converti en argon2 et
/// l'ancien hachage effacé.
async fn base_rc12_ancien_hachage(state: &AppState, lan: &str) {
    poser_hachages(state, Some(&sha256("ancien-1234")), None);
    poser_authentification(state, true);
    for adresse in [CETTE_MACHINE, lan] {
        let (st, v) = connexion(state, adresse, "default", MAUVAIS).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{adresse} : {v}");
        refus_lisible(&v);
        assert_eq!(
            hachages(state),
            (Some(sha256("ancien-1234")), None),
            "{adresse} : rien ne doit être converti sans vérification"
        );
    }
    let (st, v) = connexion(state, lan, "default", "ancien-1234").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(
        mot_de_passe_enregistre(state, "ancien-1234"),
        "converti après vérification"
    );
    assert_eq!(
        hachages(state).0,
        None,
        "l'ancien hachage est effacé une fois converti"
    );
    let (st, _) = connexion(state, CETTE_MACHINE, "default", MAUVAIS).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert!(mot_de_passe_enregistre(state, "ancien-1234"));
}

/// Un ancien hachage VIDE à côté d'un nouveau : le compte a un mot de passe.
async fn ancien_hachage_vide_et_nouveau_hachage(state: &AppState, lan: &str) {
    let h = tune_server::auth::hash_password(MOT_DE_PASSE).unwrap();
    poser_hachages(state, Some(""), Some(&h));
    poser_authentification(state, true);
    for adresse in [CETTE_MACHINE, lan] {
        let (st, v) = connexion(state, adresse, "default", MAUVAIS).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{adresse} : {v}");
        assert!(mot_de_passe_enregistre(state, MOT_DE_PASSE));
    }
    // Authentification coupée : même règle, le compte a un mot de passe.
    poser_authentification(state, false);
    let (st, v) = connexion(state, CETTE_MACHINE, "default", MAUVAIS).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
    assert!(mot_de_passe_enregistre(state, MOT_DE_PASSE));
}

/// Un hachage illisible (`password_hash_v2 = "garbage"`) ou fait d'espaces est
/// un mot de passe enregistré : aucune session, rien d'enregistré, sur cette
/// machine comme depuis le LAN, authentification active ou coupée.
async fn hachages_illisibles(state: &AppState, lan: &str) {
    for (ancien, nouveau) in [
        (None, Some("garbage")),
        (None, Some("   ")),
        (Some("   "), None),
    ] {
        poser_hachages(state, ancien, nouveau);
        let attendu = (ancien.map(str::to_string), nouveau.map(str::to_string));
        for active in [true, false] {
            poser_authentification(state, active);
            let adresses: &[&str] = if active {
                &[CETTE_MACHINE, lan]
            } else {
                &[CETTE_MACHINE]
            };
            for adresse in adresses {
                for mot in [MAUVAIS, "garbage", "   "] {
                    let (st, v) = connexion(state, adresse, "default", mot).await;
                    assert_eq!(
                        st,
                        StatusCode::UNAUTHORIZED,
                        "{ancien:?}/{nouveau:?}, auth {active}, {adresse}, « {mot} » : {v}"
                    );
                    assert_eq!(
                        hachages(state),
                        attendu,
                        "{ancien:?}/{nouveau:?}, auth {active}, {adresse} : rien ne doit être enregistré"
                    );
                }
            }
        }
    }
}

/// Un autre profil sans mot de passe n'ouvre jamais de session par ce chemin.
async fn autre_profil_sans_mot_de_passe(state: &AppState, lan: &str) {
    state
        .backend
        .execute(
            "INSERT INTO profiles (username, display_name) VALUES ('salon', 'Salon')",
            &[],
        )
        .unwrap();
    activer(state).await;
    for adresse in [CETTE_MACHINE, lan] {
        let (st, v) = connexion(state, adresse, "salon", MAUVAIS).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{adresse} : {v}");
    }
    let (st, v) = connexion(state, CETTE_MACHINE, "inconnu", MAUVAIS).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
    refus_lisible(&v);
}

// ---------------------------------------------------------------------------
// SQLite
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sqlite_activation_puis_mauvais_mot_de_passe_sur_cette_machine() {
    activation_puis_mauvais_mot_de_passe_sur_cette_machine(&etat_sqlite(), "192.168.1.61:50000")
        .await;
}

#[tokio::test]
async fn sqlite_activation_puis_mauvais_mot_de_passe_depuis_le_lan() {
    activation_puis_mauvais_mot_de_passe_depuis_le_lan(&etat_sqlite(), "192.168.1.62:50000").await;
}

#[tokio::test]
async fn sqlite_profil_sans_mot_de_passe_authentification_coupee() {
    profil_sans_mot_de_passe_authentification_coupee(&etat_sqlite(), "192.168.1.63:50000").await;
}

#[tokio::test]
async fn sqlite_base_rc12_ancien_hachage() {
    base_rc12_ancien_hachage(&etat_sqlite(), "192.168.1.64:50000").await;
}

#[tokio::test]
async fn sqlite_ancien_hachage_vide_et_nouveau_hachage() {
    ancien_hachage_vide_et_nouveau_hachage(&etat_sqlite(), "192.168.1.65:50000").await;
}

#[tokio::test]
async fn sqlite_hachages_illisibles() {
    hachages_illisibles(&etat_sqlite(), "192.168.1.67:50000").await;
}

#[tokio::test]
async fn sqlite_autre_profil_sans_mot_de_passe() {
    autre_profil_sans_mot_de_passe(&etat_sqlite(), "192.168.1.66:50000").await;
}

/// rc14 — le profil 1 s'appelle `admin` ; `default` en reste l'alias.
async fn alias_default_du_profil_admin(state: &AppState, lan: &str) {
    let nom_du_profil_1 = |state: &AppState| -> String {
        state
            .backend
            .query_one("SELECT username FROM profiles WHERE id = 1", &[])
            .unwrap()
            .unwrap()[0]
            .as_string()
            .unwrap()
    };
    assert_eq!(
        nom_du_profil_1(state),
        "admin",
        "base migrée : profil 1 = admin"
    );
    assert_eq!(hachages(state), (None, None));

    // Première connexion sous `admin`, sur cette machine : revendiquée.
    let (st, v) = connexion(state, CETTE_MACHINE, "admin", "premier-1234").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["user_id"], 1);
    assert_eq!(v["username"], "admin");
    assert_eq!(v["role"], "admin");
    assert!(mot_de_passe_enregistre(state, "premier-1234"));

    // `default` ouvre le même compte, ici et depuis le LAN, et la réponse
    // porte le vrai nom.
    for adresse in [CETTE_MACHINE, lan] {
        let (st, v) = connexion(state, adresse, "default", "premier-1234").await;
        assert_eq!(st, StatusCode::OK, "{adresse} : {v}");
        assert_eq!(v["user_id"], 1, "{v}");
        assert_eq!(v["username"], "admin", "{v}");
        assert_eq!(v["role"], "admin", "{v}");
    }
    let (st, v) = connexion(state, lan, "default", MAUVAIS).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
    assert!(mot_de_passe_enregistre(state, "premier-1234"));

    // Un nouveau compte ne capte pas l'alias.
    let (st, v) = appel(
        state,
        CETTE_MACHINE,
        "/api/v1/auth/register",
        r#"{"username":"default","password":"capteur-1234"}"#,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "{v}");

    // Profil 1 renommé par l'utilisateur : son nom est gardé, `default` reste
    // son alias (mot de passe exigé)…
    state
        .backend
        .execute(
            "UPDATE profiles SET username = 'bertrand' WHERE id = 1",
            &[],
        )
        .unwrap();
    let (st, v) = connexion(state, lan, "default", "premier-1234").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["user_id"], 1, "{v}");
    assert_eq!(v["username"], "bertrand", "{v}");
    // … mais il n'est plus revendicable sans mot de passe, comme avant la rc14.
    poser_hachages(state, None, None);
    let (st, v) = connexion(state, CETTE_MACHINE, "default", "autre-1234").await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(hachages(state), (None, None));

    // Profil 1 resté `default` (nom `admin` déjà pris ailleurs) : revendicable
    // comme avant.
    state
        .backend
        .execute("UPDATE profiles SET username = 'default' WHERE id = 1", &[])
        .unwrap();
    let (st, v) = connexion(state, CETTE_MACHINE, "default", "second-1234").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["username"], "default", "{v}");
    assert!(mot_de_passe_enregistre(state, "second-1234"));

    state
        .backend
        .execute(
            "UPDATE profiles SET username = 'admin', display_name = 'Admin' WHERE id = 1",
            &[],
        )
        .unwrap();
}

#[tokio::test]
async fn sqlite_alias_default_du_profil_admin() {
    alias_default_du_profil_admin(&etat_sqlite(), "192.168.1.68:50000").await;
}

/// Le message suit la langue de la requête.
#[tokio::test]
async fn le_refus_parle_la_langue_de_la_requete() {
    let state = etat_sqlite();
    activer(&state).await;
    let corps = serde_json::json!({ "username": "default", "password": MAUVAIS }).to_string();
    let (st, v) = appel(
        &state,
        CETTE_MACHINE,
        "/api/v1/auth/login",
        &corps,
        Some("fr-FR,fr;q=0.9"),
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(v["error"], "invalid credentials");
    assert_eq!(v["message"], "Identifiant ou mot de passe incorrect", "{v}");
    assert_eq!(v["message_key"], "api.auth.invalidCredentials");
}

// ---------------------------------------------------------------------------
// PostgreSQL : une seule base pour tous les scénarios, remise à zéro entre eux.
// ---------------------------------------------------------------------------

#[cfg(feature = "postgres")]
fn remettre_a_zero(state: &AppState) {
    state
        .backend
        .execute("DELETE FROM profiles WHERE id <> 1", &[])
        .unwrap();
    state
        .backend
        .execute(
            "UPDATE profiles SET username = 'admin', display_name = 'Admin' WHERE id = 1",
            &[],
        )
        .unwrap();
    poser_hachages(state, None, None);
    poser_authentification(state, false);
}

// Le moteur PostgreSQL bloque dans `AppState` : runtime multi-thread.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn postgres_memes_scenarios() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let config = tune_server::config::TuneConfig {
        database_url: Some(url),
        ..Default::default()
    };
    let state = AppState::new("", 0, config).expect("AppState sur PostgreSQL");
    remettre_a_zero(&state);
    activation_puis_mauvais_mot_de_passe_sur_cette_machine(&state, "10.0.0.71:50000").await;
    remettre_a_zero(&state);
    activation_puis_mauvais_mot_de_passe_depuis_le_lan(&state, "10.0.0.72:50000").await;
    remettre_a_zero(&state);
    profil_sans_mot_de_passe_authentification_coupee(&state, "10.0.0.73:50000").await;
    remettre_a_zero(&state);
    base_rc12_ancien_hachage(&state, "10.0.0.74:50000").await;
    remettre_a_zero(&state);
    ancien_hachage_vide_et_nouveau_hachage(&state, "10.0.0.75:50000").await;
    remettre_a_zero(&state);
    autre_profil_sans_mot_de_passe(&state, "10.0.0.76:50000").await;
    remettre_a_zero(&state);
    hachages_illisibles(&state, "10.0.0.77:50000").await;
    remettre_a_zero(&state);
    alias_default_du_profil_admin(&state, "10.0.0.78:50000").await;
    remettre_a_zero(&state);
}
