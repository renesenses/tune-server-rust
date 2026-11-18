//! Relier / délier le compte Mozaiklabs et poser / retirer la licence : la
//! garde « administrateur ou cette machine », rejouée par le VRAI routeur
//! (`crate::routes::router`, préfixe `/api/v1`), l'adresse de l'appelant posée
//! en extension `ConnectInfo` comme le fait le service de production.
//!
//! - authentification coupée, appel du réseau local → 403 `not_local` ;
//! - authentification coupée, appel de cette machine → accepté ;
//! - authentification active, administrateur → accepté (même du réseau) ;
//! - authentification active, compte standard → 403.

use std::net::SocketAddr;

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::profile_repo::ProfileRepo;
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

const LAN: &str = "192.168.1.50:51000";
const ICI: &str = "127.0.0.1:51000";
const CLE: &str = "TUNE-AAAA-BBBB-CCCC-WXYZ";

struct Banc {
    state: AppState,
    _dossier: tempfile::TempDir,
}

fn banc() -> Banc {
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");
    let state = AppState::new(base.to_str().unwrap(), 0, Default::default()).unwrap();
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    // Aucun réseau : tout appel sortant vers mozaiklabs échoue tout de suite.
    reglages
        .set("mozaik_base_url", "http://127.0.0.1:9")
        .unwrap();
    Banc {
        state,
        _dossier: dossier,
    }
}

/// Authentification active ; rend (jeton admin, jeton standard).
fn activer_l_authentification(state: &AppState) -> (String, String) {
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    reglages.set("jwt_secret", "secret-de-banc").unwrap();
    reglages.set("auth_enabled", "true").unwrap();
    let profils = ProfileRepo::with_backend(state.backend.clone());
    let admin = profils.create("admin", Some("Admin"), None).unwrap();
    let invite = profils.create("invite", Some("Invité"), None).unwrap();
    (
        crate::auth::sign_jwt(admin, "admin", "secret-de-banc").unwrap(),
        crate::auth::sign_jwt(invite, "user", "secret-de-banc").unwrap(),
    )
}

async fn appel(
    state: &AppState,
    depuis: &str,
    jeton: Option<&str>,
    methode: &str,
    uri: &str,
    corps: Option<Value>,
) -> (StatusCode, String) {
    let adresse: SocketAddr = depuis.parse().unwrap();
    let mut requete = Request::builder()
        .method(methode)
        .uri(uri)
        .extension(ConnectInfo(adresse));
    if let Some(j) = jeton {
        requete = requete.header(header::AUTHORIZATION, format!("Bearer {j}"));
    }
    let body = match corps {
        Some(c) => {
            requete = requete.header(header::CONTENT_TYPE, "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let reponse = crate::routes::router(state.clone())
        .oneshot(requete.body(body).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = to_bytes(reponse.into_body(), usize::MAX).await.unwrap();
    (statut, String::from_utf8_lossy(&octets).into_owned())
}

fn json_de(texte: &str) -> Value {
    serde_json::from_str(texte).unwrap_or(Value::Null)
}

/// Les routes JSON qui engagent le compte ou la licence.
fn routes_json() -> Vec<(&'static str, &'static str, Option<Value>)> {
    vec![
        (
            "POST",
            "/api/v1/cloud/license/activate",
            Some(json!({ "license_key": "" })),
        ),
        ("POST", "/api/v1/cloud/license/deactivate", None),
        ("POST", "/api/v1/system/license", Some(json!({ "key": "" }))),
        ("DELETE", "/api/v1/system/license", None),
        ("POST", "/api/v1/cloud/sso/disconnect", None),
    ]
}

#[tokio::test]
async fn reseau_local_sans_authentification_refuse_not_local() {
    let b = banc();
    b.state.license.set_license_key(CLE).await.unwrap();
    for (methode, uri, corps) in routes_json() {
        let (statut, texte) = appel(&b.state, LAN, None, methode, uri, corps).await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "{methode} {uri} : {texte}");
        let v = json_de(&texte);
        assert_eq!(v["error"], "auth_required", "{methode} {uri}");
        assert_eq!(v["reason"], "not_local", "{methode} {uri}");
    }
    // Rien n'a bougé : la clé est toujours là.
    let ls = b.state.license.license_state().await;
    assert_eq!(ls.license_key.as_deref(), Some(CLE));
}

#[tokio::test]
async fn reseau_local_sans_authentification_ne_relie_pas_de_compte() {
    let b = banc();
    let reglages = SettingsRepo::with_backend(b.state.backend.clone());

    let (statut, page) = appel(
        &b.state,
        LAN,
        None,
        "GET",
        "/api/v1/cloud/sso/authorize",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::FORBIDDEN, "{page}");
    assert!(page.contains("<html"), "le refus est une page : {page}");
    assert!(
        page.contains("localhost"),
        "la page dit quoi faire : {page}"
    );
    assert_eq!(
        reglages
            .get("mozaik_pkce_pending")
            .unwrap()
            .unwrap_or_default(),
        "",
        "aucune autorisation n'a été préparée"
    );

    // Une autorisation lancée depuis la machine reste en attente : un retour
    // présenté depuis le réseau ne doit ni l'accepter ni la consommer.
    let (statut, _) = appel(
        &b.state,
        ICI,
        None,
        "GET",
        "/api/v1/cloud/sso/authorize",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::TEMPORARY_REDIRECT);
    let attente = reglages.get("mozaik_pkce_pending").unwrap().unwrap();
    let etat = json_de(&attente)["state"].as_str().unwrap().to_string();
    let uri = format!("/api/v1/cloud/sso/callback?code=code-du-tiers&state={etat}");
    let (statut, page) = appel(&b.state, LAN, None, "GET", &uri, None).await;
    assert_eq!(statut, StatusCode::FORBIDDEN, "{page}");
    assert_eq!(
        reglages.get("mozaik_pkce_pending").unwrap().as_deref(),
        Some(attente.as_str()),
        "l'attente de la machine n'est pas consommée"
    );
    assert!(reglages.get("mozaik_access_token").unwrap().is_none());
}

#[tokio::test]
async fn requete_relayee_vue_de_la_machine_refusee() {
    let b = banc();
    let adresse: SocketAddr = ICI.parse().unwrap();
    let reponse = crate::routes::router(b.state.clone())
        .oneshot(
            Request::post("/api/v1/cloud/license/deactivate")
                .extension(ConnectInfo(adresse))
                .header(tune_core::cloud::ENTETE_RELAIS, "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reponse.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn cette_machine_sans_authentification_passe() {
    let b = banc();
    // Clé vide : la garde est passée, le gestionnaire répond 400 / 422 sans
    // aucun appel réseau.
    let (statut, texte) = appel(
        &b.state,
        ICI,
        None,
        "POST",
        "/api/v1/cloud/license/activate",
        Some(json!({ "license_key": "" })),
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{texte}");

    b.state.license.set_license_key(CLE).await.unwrap();
    let (statut, texte) = appel(
        &b.state,
        ICI,
        None,
        "POST",
        "/api/v1/cloud/license/deactivate",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{texte}");
    assert!(b.state.license.license_state().await.license_key.is_none());

    b.state.license.set_license_key(CLE).await.unwrap();
    let (statut, texte) = appel(
        &b.state,
        ICI,
        None,
        "DELETE",
        "/api/v1/system/license",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{texte}");

    let (statut, texte) = appel(
        &b.state,
        ICI,
        None,
        "POST",
        "/api/v1/cloud/sso/disconnect",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{texte}");

    // Retour du SSO depuis la machine : la garde passe, le contrôle du `state`
    // tranche ensuite (400, pas 403).
    let (statut, _) = appel(
        &b.state,
        ICI,
        None,
        "GET",
        "/api/v1/cloud/sso/authorize",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::TEMPORARY_REDIRECT);
    let (statut, page) = appel(
        &b.state,
        ICI,
        None,
        "GET",
        "/api/v1/cloud/sso/callback?code=c&state=faux",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{page}");
}

#[tokio::test]
async fn la_cle_en_clair_ne_sort_que_vers_la_machine() {
    let b = banc();
    b.state.license.set_license_key(CLE).await.unwrap();

    let (_, texte) = appel(
        &b.state,
        ICI,
        None,
        "GET",
        "/api/v1/cloud/license/status",
        None,
    )
    .await;
    assert_eq!(json_de(&texte)["license_key"], CLE);

    let (statut, texte) = appel(
        &b.state,
        LAN,
        None,
        "GET",
        "/api/v1/cloud/license/status",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    let masquee = json_de(&texte)["license_key"].as_str().unwrap().to_string();
    assert!(!masquee.contains("AAAA"), "{masquee}");
    assert!(masquee.ends_with("WXYZ"), "{masquee}");
}

#[tokio::test]
async fn authentification_active_admin_passe_standard_refuse() {
    let b = banc();
    let (admin, invite) = activer_l_authentification(&b.state);
    b.state.license.set_license_key(CLE).await.unwrap();

    // Compte standard, même depuis la machine : 403.
    for depuis in [LAN, ICI] {
        for (methode, uri, corps) in routes_json() {
            let (statut, texte) = appel(&b.state, depuis, Some(&invite), methode, uri, corps).await;
            assert_eq!(
                statut,
                StatusCode::FORBIDDEN,
                "{depuis} {methode} {uri} : {texte}"
            );
        }
        let (statut, page) = appel(
            &b.state,
            depuis,
            Some(&invite),
            "GET",
            "/api/v1/cloud/sso/authorize",
            None,
        )
        .await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "{page}");
        let (statut, page) = appel(
            &b.state,
            depuis,
            Some(&invite),
            "GET",
            "/api/v1/cloud/sso/callback?code=c&state=s",
            None,
        )
        .await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "{page}");
        let (_, texte) = appel(
            &b.state,
            depuis,
            Some(&invite),
            "GET",
            "/api/v1/cloud/license/status",
            None,
        )
        .await;
        assert_ne!(json_de(&texte)["license_key"], CLE);
    }
    assert_eq!(
        b.state.license.license_state().await.license_key.as_deref(),
        Some(CLE)
    );

    // Administrateur, du réseau local : accepté.
    let (statut, texte) = appel(
        &b.state,
        LAN,
        Some(&admin),
        "POST",
        "/api/v1/cloud/license/activate",
        Some(json!({ "license_key": "" })),
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{texte}");
    let (_, texte) = appel(
        &b.state,
        LAN,
        Some(&admin),
        "GET",
        "/api/v1/cloud/license/status",
        None,
    )
    .await;
    assert_eq!(json_de(&texte)["license_key"], CLE);
    let (statut, texte) = appel(
        &b.state,
        LAN,
        Some(&admin),
        "POST",
        "/api/v1/cloud/license/deactivate",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{texte}");
    let (statut, _) = appel(
        &b.state,
        LAN,
        Some(&admin),
        "GET",
        "/api/v1/cloud/sso/authorize",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::TEMPORARY_REDIRECT);
    let (statut, page) = appel(
        &b.state,
        LAN,
        Some(&admin),
        "GET",
        "/api/v1/cloud/sso/callback?code=c&state=faux",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{page}");
    let (statut, texte) = appel(
        &b.state,
        LAN,
        Some(&admin),
        "POST",
        "/api/v1/cloud/sso/disconnect",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{texte}");
}
