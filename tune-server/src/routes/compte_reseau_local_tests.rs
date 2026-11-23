//! rc14 — relier le compte mozaiklabs.fr depuis le réseau local, sans
//! authentification, même règle que la mise à jour en rc13.1
//! (`auth::AdminOuReseauLocalPourLaMiseAJour`).
//!
//! Ce que ces tests tiennent, sur le VRAI routeur (`crate::routes::router`) :
//!
//! 1. réseau local, sans authentification : `GET /cloud/sso/authorize` lance la
//!    liaison, le retour passe la garde (le contrôle du `state` tranche ensuite),
//!    et `GET /cloud/sso/status` montre le compte relié (le menu du compte et
//!    sa photo en dépendent) ;
//! 2. relais distant, mandataire inverse, CGNAT/Tailscale, adresse publique :
//!    refusés, avec une phrase traduite qui nomme l'action, et aucune liaison
//!    n'est préparée ; le statut n'y montre toujours que « relié » ;
//! 3. authentification active, sans jeton : refusé, depuis le réseau local
//!    comme depuis la machine ;
//! 4. la déliaison reste réservée à cette machine (inchangé).

use std::net::SocketAddr;

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

const AUTORISER: &str = "/api/v1/cloud/sso/authorize";
const STATUT: &str = "/api/v1/cloud/sso/status";
const ICI: &str = "127.0.0.1:52000";
const COMPTE: &str = r#"{"id":1,"email":"proprio@exemple.test","display_name":"P","is_admin":false,"avatar_url":null}"#;

struct Banc {
    state: AppState,
    _dossier: tempfile::TempDir,
}

fn banc() -> Banc {
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");
    let state = AppState::new(base.to_str().unwrap(), 0, Default::default()).unwrap();
    // Aucun réseau : tout appel sortant vers mozaiklabs échoue tout de suite.
    SettingsRepo::with_backend(state.backend.clone())
        .set("mozaik_base_url", "http://127.0.0.1:9")
        .unwrap();
    Banc {
        state,
        _dossier: dossier,
    }
}

fn reglages(state: &AppState) -> SettingsRepo {
    SettingsRepo::with_backend(state.backend.clone())
}

fn attente_du_serveur(state: &AppState) -> String {
    reglages(state)
        .get("mozaik_pkce_pending")
        .unwrap()
        .unwrap_or_default()
}

async fn get(state: &AppState, depuis: &str, uri: &str, entetes: &[(&str, &str)]) -> (StatusCode, String) {
    let adresse: SocketAddr = depuis.parse().unwrap();
    let mut requete = Request::get(uri).extension(ConnectInfo(adresse));
    for (nom, valeur) in entetes {
        requete = requete.header(*nom, *valeur);
    }
    let reponse = crate::routes::router(state.clone())
        .oneshot(requete.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = to_bytes(reponse.into_body(), usize::MAX).await.unwrap();
    (statut, String::from_utf8_lossy(&octets).into_owned())
}

async fn post(state: &AppState, depuis: &str, uri: &str) -> StatusCode {
    let adresse: SocketAddr = depuis.parse().unwrap();
    crate::routes::router(state.clone())
        .oneshot(
            Request::post(uri)
                .extension(ConnectInfo(adresse))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

fn json_de(texte: &str) -> Value {
    serde_json::from_str(texte).unwrap_or(Value::Null)
}

/// 1. Le réseau local relie le compte : IPv4 privées, IPv6 unique locale,
/// lien local ; et le client web servi par le serveur, joint par son nom mDNS.
#[tokio::test]
async fn reseau_local_sans_authentification_la_liaison_passe() {
    for depuis in [
        "192.168.1.50:52000",
        "10.0.0.7:52000",
        "172.20.1.4:52000",
        "169.254.10.20:52000",
        "[fd12:3456:789a::5]:52000",
        "[fe80::1]:52000",
    ] {
        let b = banc();
        let (statut, page) = get(&b.state, depuis, AUTORISER, &[("host", "192.168.1.10:8888")]).await;
        assert_eq!(
            statut,
            StatusCode::TEMPORARY_REDIRECT,
            "{depuis} : la liaison depuis le réseau local doit passer la garde — {page}"
        );
        assert!(
            !attente_du_serveur(&b.state).is_empty(),
            "{depuis} : l'autorisation est préparée"
        );
    }

    let b = banc();
    let (statut, page) = get(
        &b.state,
        "192.168.1.50:52000",
        AUTORISER,
        &[("host", "tune.local:8888"), ("sec-fetch-site", "same-origin")],
    )
    .await;
    assert_eq!(statut, StatusCode::TEMPORARY_REDIRECT, "mDNS — {page}");

    // Le retour de mozaiklabs.fr (navigation inter-sites) passe la garde : le
    // contrôle du `state` tranche ensuite (400, pas 403), et rien ne se relie.
    let (statut, page) = get(
        &b.state,
        "192.168.1.50:52000",
        "/api/v1/cloud/sso/callback?code=c&state=faux",
        &[("host", "tune.local:8888"), ("sec-fetch-site", "cross-site")],
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "retour depuis le LAN — {page}");
    assert!(reglages(&b.state).get("mozaik_access_token").unwrap().is_none());
}

/// 1 bis. Le statut montre le compte relié au réseau local : le menu du compte
/// (nom, courriel, photo de profil) n'est plus « relié, sans détails ».
#[tokio::test]
async fn reseau_local_sans_authentification_le_statut_montre_le_compte() {
    let b = banc();
    let r = reglages(&b.state);
    r.set("mozaik_access_token", "jeton-du-proprietaire").unwrap();
    r.set("mozaik_user", COMPTE).unwrap();
    let (_, texte) = get(&b.state, "192.168.1.50:52000", STATUT, &[("host", "192.168.1.10:8888")]).await;
    let v = json_de(&texte);
    assert_eq!(v["connected"], true, "{texte}");
    assert_eq!(v["details_hidden"], false, "{texte}");
    assert_eq!(v["user"]["email"], "proprio@exemple.test", "{texte}");
}

/// 2. Hors du réseau local : refusé, phrase traduite, aucune liaison préparée.
#[tokio::test]
async fn hors_du_reseau_local_la_liaison_reste_refusee() {
    // CGNAT / Tailscale et adresse publique.
    for depuis in ["100.101.102.103:52000", "203.0.113.9:52000"] {
        let b = banc();
        let (statut, page) = get(
            &b.state,
            depuis,
            AUTORISER,
            &[("host", "192.168.1.10:8888"), ("accept-language", "fr-FR,fr;q=0.9")],
        )
        .await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "{depuis} — {page}");
        assert!(page.contains("<html"), "le refus est une page : {page}");
        assert!(
            page.contains("Relier le compte mozaiklabs.fr") && page.contains("réseau local"),
            "{depuis} : phrase française attendue — {page}"
        );
        assert!(attente_du_serveur(&b.state).is_empty(), "{depuis} : rien de préparé");
    }

    // Mandataire inverse local, qui exposerait le serveur sur Internet.
    let b = banc();
    let (statut, page) = get(
        &b.state,
        ICI,
        AUTORISER,
        &[("host", "127.0.0.1:8888"), ("x-forwarded-for", "203.0.113.9")],
    )
    .await;
    assert_eq!(statut, StatusCode::FORBIDDEN, "mandataire — {page}");
    assert!(attente_du_serveur(&b.state).is_empty(), "mandataire : rien de préparé");
    // Même derrière un mandataire du réseau local.
    let (statut, page) = get(
        &b.state,
        "192.168.1.2:52000",
        AUTORISER,
        &[("host", "tune.local:8888"), ("x-forwarded-for", "203.0.113.9")],
    )
    .await;
    assert_eq!(statut, StatusCode::FORBIDDEN, "mandataire du LAN — {page}");

    // Relais distant et écoute directe : rejoués depuis 127.0.0.1, marqués.
    for depuis in [ICI, "192.168.1.50:52000"] {
        let (statut, page) = get(
            &b.state,
            depuis,
            AUTORISER,
            &[(tune_core::cloud::ENTETE_RELAIS, "1"), ("host", "127.0.0.1:8888")],
        )
        .await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "relais depuis {depuis} — {page}");
    }
    assert!(attente_du_serveur(&b.state).is_empty(), "relais : rien de préparé");

    // Le statut, lui, ne montre que « relié » au mandataire.
    let r = reglages(&b.state);
    r.set("mozaik_access_token", "jeton-du-proprietaire").unwrap();
    r.set("mozaik_user", COMPTE).unwrap();
    for depuis in [ICI, "192.168.1.2:52000"] {
        let (_, texte) = get(
            &b.state,
            depuis,
            STATUT,
            &[("host", "tune.local:8888"), ("x-forwarded-for", "203.0.113.9")],
        )
        .await;
        let v = json_de(&texte);
        assert_eq!(v["details_hidden"], true, "mandataire {depuis} — {texte}");
        assert!(v["user"].is_null(), "mandataire {depuis} — {texte}");
    }
    let (_, texte) = get(&b.state, "100.101.102.103:52000", STATUT, &[("host", "192.168.1.10:8888")]).await;
    assert!(json_de(&texte)["user"].is_null(), "CGNAT — {texte}");
}

/// 3. Authentification active, sans jeton : refusé, réseau local ou machine.
/// (Inchangé : ce test est vert avant comme après, c'est voulu.)
#[tokio::test]
async fn authentification_active_sans_jeton_la_liaison_est_refusee() {
    let b = banc();
    let r = reglages(&b.state);
    r.set("jwt_secret", "secret-de-banc").unwrap();
    r.set("auth_enabled", "true").unwrap();
    for depuis in ["192.168.1.50:52000", ICI] {
        let (statut, page) = get(&b.state, depuis, AUTORISER, &[("host", "192.168.1.10:8888")]).await;
        assert_eq!(statut, StatusCode::UNAUTHORIZED, "sans jeton depuis {depuis} — {page}");
        assert!(attente_du_serveur(&b.state).is_empty(), "{depuis} : rien de préparé");
    }
}

/// 4. La déliaison n'est pas rouverte : elle reste à cette machine.
#[tokio::test]
async fn la_deliaison_reste_reservee_a_cette_machine() {
    let b = banc();
    assert_eq!(
        post(&b.state, "192.168.1.50:52000", "/api/v1/cloud/sso/disconnect").await,
        StatusCode::FORBIDDEN
    );
}

/// Les phrases du refus de liaison existent dans les seize langues.
#[test]
fn les_phrases_du_refus_de_liaison_existent_dans_les_seize_langues() {
    let catalogue: Value = serde_json::from_str(include_str!("../i18n_server.json")).unwrap();
    for l in [
        "fr", "en", "de", "es", "it", "zh", "ja", "ko", "ro", "sv", "hu", "vi", "nb", "pl", "el", "tr",
    ] {
        let t = catalogue["api.machine.reservedOutsideLanAccount"][l]
            .as_str()
            .unwrap_or_default();
        assert!(t.contains("{action}"), "reservedOutsideLanAccount/{l} : {t:?}");
        let a = catalogue["api.machine.action.account"][l].as_str().unwrap_or_default();
        assert!(!a.trim().is_empty(), "action.account/{l} absente");
    }
    assert_eq!(crate::auth::action_du_chemin("/api/v1/cloud/sso/authorize"), "account");
}
