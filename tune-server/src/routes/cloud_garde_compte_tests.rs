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

    // Compte standard, même depuis la machine : licence refusée (403).
    for depuis in [LAN, ICI] {
        for (methode, uri, corps) in routes_licence() {
            let (statut, texte) = appel(&b.state, depuis, Some(&invite), methode, uri, corps).await;
            assert_eq!(
                statut,
                StatusCode::FORBIDDEN,
                "{depuis} {methode} {uri} : {texte}"
            );
        }
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

/// Les routes de licence (toujours administrateur ou cette machine).
fn routes_licence() -> Vec<(&'static str, &'static str, Option<Value>)> {
    routes_json()
        .into_iter()
        .filter(|(_, uri, _)| uri.contains("license"))
        .collect()
}

// ─── Multi-compte : un compte standard relie SON profil, jamais le serveur ──

#[tokio::test]
async fn compte_standard_ne_relie_que_son_profil() {
    let b = banc();
    let (_admin, invite) = activer_l_authentification(&b.state);
    let reglages = SettingsRepo::with_backend(b.state.backend.clone());
    // Le serveur est relié au compte du propriétaire.
    reglages
        .set("mozaik_access_token", "jeton-du-proprietaire")
        .unwrap();
    reglages
        .set("mozaik_user", r#"{"id":1,"email":"proprio@exemple.test","display_name":"P","is_admin":false,"avatar_url":null}"#)
        .unwrap();
    let id_invite = crate::auth::verify_jwt(&invite, "secret-de-banc")
        .unwrap()
        .sub
        .parse::<i64>()
        .unwrap();

    // Autorisation : acceptée, attente rangée sur le PROFIL, pas sur le serveur.
    let (statut, _) = appel(
        &b.state,
        LAN,
        Some(&invite),
        "GET",
        "/api/v1/cloud/sso/authorize",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::TEMPORARY_REDIRECT);
    assert!(
        reglages
            .get("mozaik_pkce_pending")
            .unwrap()
            .unwrap_or_default()
            .is_empty(),
        "l'attente du serveur n'est pas touchée"
    );
    assert!(
        reglages
            .get(&format!("mozaik_pkce_pending_profil_{id_invite}"))
            .unwrap()
            .is_some_and(|v| !v.is_empty())
    );

    // Une attente du SERVEUR (lancée par l'administrateur) n'est ni lue ni
    // consommée par le retour d'un compte standard.
    reglages
        .set(
            "mozaik_pkce_pending",
            r#"{"verifier":"v","challenge":"c","state":"etat-admin"}"#,
        )
        .unwrap();
    let (statut, page) = appel(
        &b.state,
        LAN,
        Some(&invite),
        "GET",
        "/api/v1/cloud/sso/callback?code=c&state=etat-admin",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{page}");
    assert!(
        reglages
            .get("mozaik_pkce_pending")
            .unwrap()
            .unwrap()
            .contains("etat-admin"),
        "l'attente de l'administrateur est intacte"
    );

    // Statut : le compte standard voit SA liaison (aucune).
    let (_, texte) = appel(
        &b.state,
        LAN,
        Some(&invite),
        "GET",
        "/api/v1/cloud/sso/status",
        None,
    )
    .await;
    let v = json_de(&texte);
    assert_eq!(v["scope"], "profile", "{texte}");
    assert_eq!(v["connected"], false);

    // Déliaison : son profil seulement ; le serveur reste relié.
    reglages
        .set(&format!("mozaik_compte_profil_{id_invite}"), r#"{"id":2,"email":"invite@exemple.test","display_name":"I","is_admin":false,"avatar_url":null}"#)
        .unwrap();
    let (statut, texte) = appel(
        &b.state,
        LAN,
        Some(&invite),
        "POST",
        "/api/v1/cloud/sso/disconnect",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{texte}");
    assert_eq!(json_de(&texte)["scope"], "profile");
    assert!(
        reglages
            .get(&format!("mozaik_compte_profil_{id_invite}"))
            .unwrap()
            .is_none()
    );
    assert_eq!(
        reglages.get("mozaik_access_token").unwrap().as_deref(),
        Some("jeton-du-proprietaire")
    );
    assert!(reglages.get("mozaik_user").unwrap().is_some());
}

#[tokio::test]
async fn sans_session_ni_machine_aucune_liaison_de_profil() {
    // Authentification coupée : pas de mode « profil », la machine seulement.
    let b = banc();
    let (statut, _) = appel(&b.state, LAN, None, "GET", "/api/v1/cloud/sso/status", None).await;
    assert_eq!(statut, StatusCode::OK);
    let (statut, _) = appel(
        &b.state,
        LAN,
        None,
        "POST",
        "/api/v1/cloud/sso/disconnect",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::FORBIDDEN);
}

// ─── Ronde SSO complète contre un faux mozaiklabs.fr ────────────────────────

/// Un faux mozaiklabs.fr : jeton, puis profil `is_admin: true` (l'équipe du
/// site) avec le courriel donné.
async fn faux_mozaiklabs(courriel: &'static str) -> String {
    use axum::routing::{get, post};
    let app = axum::Router::new()
        .route(
            "/oauth/token",
            post(|| async {
                axum::Json(json!({
                    "access_token": "jeton-tiers",
                    "refresh_token": "renouv-tiers",
                    "expires_in": 3600
                }))
            }),
        )
        .route(
            "/api/v1/user",
            get(move || async move {
                axum::Json(json!({
                    "id": 77,
                    "email": courriel,
                    "display_name": "Equipe Mozaiklabs",
                    "is_admin": true,
                    "avatar_url": null
                }))
            }),
        )
        .fallback(|| async { StatusCode::NOT_FOUND });
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });
    format!("http://{adresse}")
}

/// Lance une ronde complète depuis `depuis` ; rend (statut, Set-Cookie).
async fn ronde_complete(
    state: &AppState,
    depuis: &str,
    jeton: Option<&str>,
    cle_attente: &str,
) -> (StatusCode, Option<String>) {
    let (statut, _) = appel(
        state,
        depuis,
        jeton,
        "GET",
        "/api/v1/cloud/sso/authorize",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::TEMPORARY_REDIRECT);
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    let attente = reglages.get(cle_attente).unwrap().unwrap();
    let etat = json_de(&attente)["state"].as_str().unwrap().to_string();
    let adresse: SocketAddr = depuis.parse().unwrap();
    let mut req = Request::get(format!("/api/v1/cloud/sso/callback?code=c&state={etat}"))
        .extension(ConnectInfo(adresse));
    if let Some(j) = jeton {
        req = req.header(header::AUTHORIZATION, format!("Bearer {j}"));
    }
    let reponse = crate::routes::router(state.clone())
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let cookie = reponse
        .headers()
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (reponse.status(), cookie)
}

fn role_du_cookie(state: &AppState, cookie: &str) -> (i64, String) {
    let jwt = cookie
        .strip_prefix("tune_session=")
        .and_then(|r| r.split(';').next())
        .unwrap();
    let secret = SettingsRepo::with_backend(state.backend.clone())
        .get("jwt_secret")
        .unwrap()
        .unwrap();
    let c = crate::auth::verify_jwt(jwt, &secret).unwrap();
    (c.sub.parse().unwrap(), c.role)
}

fn est_admin_en_base(state: &AppState, id: i64) -> bool {
    ProfileRepo::with_backend(state.backend.clone())
        .get(id)
        .ok()
        .flatten()
        .is_some_and(|p| p.is_admin)
}

#[tokio::test]
async fn le_compte_du_site_ne_donne_jamais_le_role_admin() {
    let b = banc();
    let base = faux_mozaiklabs("equipe@mozaiklabs.test").await;
    SettingsRepo::with_backend(b.state.backend.clone())
        .set("mozaik_base_url", &base)
        .unwrap();
    // Deux profils : pas de rattachement au profil unique, un profil est créé.
    ProfileRepo::with_backend(b.state.backend.clone())
        .create("salon", Some("Salon"), None)
        .unwrap();
    let (statut, cookie) = ronde_complete(&b.state, ICI, None, "mozaik_pkce_pending").await;
    assert_eq!(statut, StatusCode::TEMPORARY_REDIRECT);
    let (profil, role) = role_du_cookie(&b.state, &cookie.expect("session émise"));
    assert_eq!(role, "user", "is_admin du site ne fait pas un admin local");
    assert!(!est_admin_en_base(&b.state, profil), "profil créé standard");
}

#[tokio::test]
async fn un_admin_local_relie_reste_admin() {
    let b = banc();
    let base = faux_mozaiklabs("proprio@exemple.test").await;
    SettingsRepo::with_backend(b.state.backend.clone())
        .set("mozaik_base_url", &base)
        .unwrap();
    // Installation à un seul profil (`default`, administrateur) : la ronde s'y
    // rattache, il reste administrateur.
    assert!(est_admin_en_base(&b.state, 1));
    let (statut, cookie) = ronde_complete(&b.state, ICI, None, "mozaik_pkce_pending").await;
    assert_eq!(statut, StatusCode::TEMPORARY_REDIRECT);
    let (profil, role) = role_du_cookie(&b.state, &cookie.expect("session émise"));
    assert_eq!(profil, 1);
    assert_eq!(role, "admin");
    assert!(est_admin_en_base(&b.state, 1));
}

#[tokio::test]
async fn ronde_d_un_compte_standard_ne_touche_pas_le_serveur() {
    let b = banc();
    let (_admin, invite) = activer_l_authentification(&b.state);
    let base = faux_mozaiklabs("invite@exemple.test").await;
    let reglages = SettingsRepo::with_backend(b.state.backend.clone());
    reglages.set("mozaik_base_url", &base).unwrap();
    reglages
        .set("mozaik_access_token", "jeton-du-proprietaire")
        .unwrap();
    let id_invite = crate::auth::verify_jwt(&invite, "secret-de-banc")
        .unwrap()
        .sub
        .parse::<i64>()
        .unwrap();
    let (statut, cookie) = ronde_complete(
        &b.state,
        LAN,
        Some(&invite),
        &format!("mozaik_pkce_pending_profil_{id_invite}"),
    )
    .await;
    assert_eq!(statut, StatusCode::TEMPORARY_REDIRECT);
    assert!(cookie.is_none(), "aucune nouvelle session, aucun rôle");
    assert_eq!(
        reglages.get("mozaik_access_token").unwrap().as_deref(),
        Some("jeton-du-proprietaire"),
        "la session cloud du serveur est intacte"
    );
    assert!(reglages.get("mozaik_user").unwrap().is_none());
    let lie = reglages
        .get(&format!("mozaik_compte_profil_{id_invite}"))
        .unwrap()
        .expect("compte rangé sur le profil");
    assert!(lie.contains("invite@exemple.test"));
    assert!(!lie.contains("jeton-tiers"), "aucun jeton conservé");
    assert!(!est_admin_en_base(&b.state, id_invite));
}

// ─── Autres envois au nom du compte lié : même garde ───────────────────────

fn routes_au_nom_du_compte() -> Vec<(&'static str, &'static str, Option<Value>)> {
    vec![
        ("GET", "/api/v1/support/tickets", None),
        ("POST", "/api/v1/support/tickets", Some(json!({}))),
        ("GET", "/api/v1/support/tickets/1", None),
        ("POST", "/api/v1/support/tickets/1/reply", Some(json!({}))),
        ("POST", "/api/v1/support/tickets/1/read", None),
        (
            "POST",
            "/api/v1/cloud/community/artist-image",
            Some(json!({})),
        ),
        ("POST", "/api/v1/cloud/community/covers", Some(json!({}))),
        (
            "POST",
            "/api/v1/cloud/community/covers/sync",
            Some(json!({})),
        ),
        ("POST", "/api/v1/cloud/telemetry/enable", None),
        ("POST", "/api/v1/cloud/telemetry/disable", None),
        (
            "POST",
            "/api/v1/system/telemetry",
            Some(json!({ "enabled": true })),
        ),
        ("POST", "/api/v1/cloud/plugins/x/vote", Some(json!({}))),
        // Décisions sur les propositions de métadonnées : poussées au cloud.
        (
            "POST",
            "/api/v1/library/proposals/1/decision",
            Some(json!({ "accept": true })),
        ),
        (
            "POST",
            "/api/v1/library/proposals/auto-apply",
            Some(json!({ "enabled": true })),
        ),
    ]
}

#[tokio::test]
async fn envois_au_nom_du_compte_refuses_hors_machine() {
    let b = banc();
    let reglages = SettingsRepo::with_backend(b.state.backend.clone());
    reglages
        .set(tune_core::cloud::telemetry::TELEMETRY_SETTING_KEY, "false")
        .unwrap();
    for (methode, uri, corps) in routes_au_nom_du_compte() {
        let (statut, texte) = appel(&b.state, LAN, None, methode, uri, corps).await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "{methode} {uri} : {texte}");
        assert_eq!(json_de(&texte)["reason"], "not_local", "{methode} {uri}");
    }
    assert_eq!(
        reglages
            .get(tune_core::cloud::telemetry::TELEMETRY_SETTING_KEY)
            .unwrap()
            .as_deref(),
        Some("false"),
        "la télémétrie n'a pas été rallumée"
    );
    // Authentification active, compte standard : refusé aussi.
    let (_admin, invite) = activer_l_authentification(&b.state);
    for (methode, uri, corps) in routes_au_nom_du_compte() {
        let (statut, texte) = appel(&b.state, LAN, Some(&invite), methode, uri, corps).await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "{methode} {uri} : {texte}");
    }
}

#[tokio::test]
async fn envois_au_nom_du_compte_acceptes_depuis_la_machine() {
    let b = banc();
    let reglages = SettingsRepo::with_backend(b.state.backend.clone());
    for (methode, uri, corps) in routes_au_nom_du_compte() {
        let (statut, texte) = appel(&b.state, ICI, None, methode, uri, corps).await;
        assert_ne!(statut, StatusCode::FORBIDDEN, "{methode} {uri} : {texte}");
    }
    let (statut, _) = appel(
        &b.state,
        ICI,
        None,
        "POST",
        "/api/v1/cloud/telemetry/enable",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        reglages
            .get(tune_core::cloud::telemetry::TELEMETRY_SETTING_KEY)
            .unwrap()
            .as_deref(),
        Some("true")
    );
}

/// Les envois au nom du compte (synchro de la bibliothèque, validation de la
/// licence, sauvegarde de la configuration dans le cloud).
fn envois_au_nom_du_compte() -> Vec<(&'static str, &'static str, Option<Value>)> {
    vec![
        ("POST", "/api/v1/cloud/license/validate", None),
        ("POST", "/api/v1/cloud/library-sync/trigger", None),
        ("POST", "/api/v1/cloud/library-sync/full-sync", None),
        (
            "POST",
            "/api/v1/cloud/library-sync/reconcile",
            Some(json!({ "apply": true })),
        ),
        (
            "POST",
            "/api/v1/system/config-backup/cloud/enable",
            Some(json!({ "passphrase": "phrase-de-banc-assez-longue" })),
        ),
        (
            "POST",
            "/api/v1/system/config-backup/cloud/backup-now",
            None,
        ),
        // Lectures avec le jeton du compte.
        ("GET", "/api/v1/system/config-backup/cloud/snapshots", None),
        ("GET", "/api/v1/audio-plugins/inconnu/catalog", None),
        // Résumé hebdomadaire : l'adresse et l'envoi par mozaiklabs.fr.
        (
            "POST",
            "/api/v1/digest/settings",
            Some(json!({ "email": "intrus@exemple.test" })),
        ),
        ("POST", "/api/v1/digest/send", None),
        // Proposition « Tune tested » : part avec le jeton et la clé.
        ("POST", "/api/v1/zones/987654/tune-tested", Some(json!({}))),
    ]
}

#[tokio::test]
async fn les_envois_au_nom_du_compte_restent_sur_cette_machine() {
    let b = banc();
    for (methode, uri, corps) in envois_au_nom_du_compte() {
        // Authentification coupée, autre appareil du réseau : refusé.
        let (statut, texte) = appel(&b.state, LAN, None, methode, uri, corps.clone()).await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "{methode} {uri} : {texte}");
        assert_eq!(json_de(&texte)["reason"], "not_local", "{methode} {uri}");
        // Cette machine : la garde laisse passer (la suite peut refuser pour
        // une autre raison — offre, réseau — mais pas « pas cette machine »).
        let (statut, texte) = appel(&b.state, ICI, None, methode, uri, corps).await;
        assert_ne!(
            json_de(&texte)["reason"],
            "not_local",
            "{methode} {uri} : {statut} {texte}"
        );
    }
}

#[tokio::test]
async fn un_compte_standard_n_envoie_rien_au_nom_du_compte() {
    let b = banc();
    let (admin, invite) = activer_l_authentification(&b.state);
    for (methode, uri, corps) in envois_au_nom_du_compte() {
        let (statut, texte) =
            appel(&b.state, LAN, Some(&invite), methode, uri, corps.clone()).await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "{methode} {uri} : {texte}");
        let (statut, texte) = appel(&b.state, LAN, Some(&admin), methode, uri, corps).await;
        assert_ne!(statut, StatusCode::FORBIDDEN, "{methode} {uri} : {texte}");
    }
}

const COMPTE_DU_SERVEUR: &str = r#"{"id":1,"email":"proprio@exemple.test","display_name":"P","is_admin":false,"avatar_url":null}"#;
const COMPTE_D_UN_PROFIL: &str = r#"{"id":9,"email":"voisin@exemple.test","display_name":"V","is_admin":false,"avatar_url":null}"#;

fn poser_les_comptes_lies(state: &AppState) {
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    reglages
        .set("mozaik_access_token", "jeton-de-banc")
        .unwrap();
    reglages.set("mozaik_user", COMPTE_DU_SERVEUR).unwrap();
    // L'offre du compte et l'adresse du résumé : réservées comme le compte.
    reglages.set("mozaik_premium", "true").unwrap();
    reglages
        .set("mozaik_premium_expires", "2099-01-01")
        .unwrap();
    reglages
        .set("mozaik_modules", r#"["module-de-banc"]"#)
        .unwrap();
    reglages.set("digest_email", "resume@exemple.test").unwrap();
    reglages
        .set("mozaik_compte_profil_9", COMPTE_D_UN_PROFIL)
        .unwrap();
}

/// Lit les comptes liés tels que les rendent `GET /system/config` et
/// `GET /system/config/export`.
async fn comptes_lus(state: &AppState, depuis: &str, jeton: Option<&str>) -> (bool, Option<bool>) {
    let (statut, texte) = appel(state, depuis, jeton, "GET", "/api/v1/system/config", None).await;
    assert_eq!(statut, StatusCode::OK, "{texte}");
    let marques = [
        "proprio@exemple.test",
        "voisin@exemple.test",
        "mozaik_premium",
        "module-de-banc",
        "resume@exemple.test",
    ];
    let lu_config = marques.iter().any(|m| texte.contains(m));
    // Tout ou rien : qui relie le serveur voit TOUT.
    if lu_config {
        for m in marques {
            assert!(texte.contains(m), "{m} manque pour qui relie le serveur");
        }
    }
    let (statut, texte) = appel(
        state,
        depuis,
        jeton,
        "GET",
        "/api/v1/system/config/export",
        None,
    )
    .await;
    // `None` : export refusé (compte standard). Un export rendu sans les
    // comptes vaut `Some(false)`, distinct d'un refus.
    let lu_export = (statut == StatusCode::OK).then(|| marques.iter().any(|m| texte.contains(m)));
    (lu_config, lu_export)
}

#[tokio::test]
async fn les_comptes_lies_ne_sortent_pas_vers_un_autre_appareil() {
    let b = banc();
    poser_les_comptes_lies(&b.state);
    // Authentification coupée.
    assert_eq!(comptes_lus(&b.state, LAN, None).await, (false, Some(false)));
    assert_eq!(comptes_lus(&b.state, ICI, None).await, (true, Some(true)));
    // Statut du SSO : relié, sans le compte, pour un autre appareil.
    let (_, texte) = appel(&b.state, LAN, None, "GET", "/api/v1/cloud/sso/status", None).await;
    let v = json_de(&texte);
    assert_eq!(v["connected"], true, "{texte}");
    assert!(v["user"].is_null(), "{texte}");
    assert_eq!(v["details_hidden"], true, "{texte}");
    let (_, texte) = appel(&b.state, ICI, None, "GET", "/api/v1/cloud/sso/status", None).await;
    assert_eq!(json_de(&texte)["details_hidden"], false, "{texte}");
    assert_eq!(
        json_de(&texte)["user"]["email"],
        "proprio@exemple.test",
        "{texte}"
    );
}

#[tokio::test]
async fn un_compte_standard_ne_lit_pas_les_comptes_des_autres() {
    let b = banc();
    let (admin, invite) = activer_l_authentification(&b.state);
    poser_les_comptes_lies(&b.state);
    assert_eq!(
        comptes_lus(&b.state, LAN, Some(&invite)).await,
        (false, None)
    );
    assert_eq!(
        comptes_lus(&b.state, LAN, Some(&admin)).await,
        (true, Some(true))
    );
}

#[tokio::test]
async fn l_adresse_du_resume_ne_sort_pas_vers_un_autre_appareil() {
    let b = banc();
    b.state
        .license
        .set_account_premium(true, Some("2099-01-01T00:00:00Z".into()))
        .await;
    poser_les_comptes_lies(&b.state);
    let (_, texte) = appel(&b.state, LAN, None, "GET", "/api/v1/digest/settings", None).await;
    assert!(!texte.contains("resume@exemple.test"), "{texte}");
    // Cette machine la voit (le témoin n'est pas vert par un refus d'offre).
    let (_, texte) = appel(&b.state, ICI, None, "GET", "/api/v1/digest/settings", None).await;
    assert!(texte.contains("resume@exemple.test"), "{texte}");
}

#[tokio::test]
async fn la_liaison_ne_nomme_pas_un_profil_par_son_adresse() {
    let b = banc();
    let nom = tune_core::cloud::nom_de_profil::nom_libre(
        &b.state.backend,
        "",
        "proprio@exemple.test",
        None,
    );
    assert_eq!(nom, "proprio");
}

/// `PATCH /system/config` n'écrit pas, depuis un autre appareil sans
/// authentification, les réglages qui font envoyer au nom du compte.
#[tokio::test]
async fn les_reglages_d_envoi_au_nom_du_compte_restent_sur_cette_machine() {
    let b = banc();
    for corps in [
        json!({ "digest_email": "intrus@exemple.test" }),
        json!({ "digest_enabled": "true" }),
        json!({ "cloud_listens_upload_enabled": "true" }),
        json!({ "cloud_community_share_enabled": "true" }),
        json!({ "telemetry_enabled": "true" }),
        json!({ "community_contribution_enabled": "true" }),
        json!({ "community_measures_contribute_enabled": "true" }),
    ] {
        let (statut, texte) = appel(
            &b.state,
            LAN,
            None,
            "PATCH",
            "/api/v1/system/config",
            Some(corps.clone()),
        )
        .await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "{corps} : {texte}");
        assert_eq!(json_de(&texte)["reason"], "not_local", "{corps}");
        let (statut, texte) = appel(
            &b.state,
            ICI,
            None,
            "PATCH",
            "/api/v1/system/config",
            Some(corps.clone()),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{corps} : {texte}");
    }
    let reglages = SettingsRepo::with_backend(b.state.backend.clone());
    // Un réglage ordinaire passe toujours depuis le réseau.
    let (statut, texte) = appel(
        &b.state,
        LAN,
        None,
        "PATCH",
        "/api/v1/system/config",
        Some(json!({ "enrich_on_scan": "false" })),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{texte}");
    assert_eq!(
        reglages.get("enrich_on_scan").unwrap().as_deref(),
        Some("false")
    );
}

/// Un curseur ou un plancher d'envoi remis à zéro ferait tout renvoyer au
/// compte : « éteint » ne vaut que pour les interrupteurs, et ces réglages
/// ne s'écrivent pas depuis un autre appareil, même à zéro.
#[tokio::test]
async fn les_curseurs_d_envoi_ne_se_remettent_pas_a_zero_depuis_le_reseau() {
    let b = banc();
    let reglages = SettingsRepo::with_backend(b.state.backend.clone());
    for cle in [
        "cloud_listens_cursor",
        "cloud_listens_floor",
        "community_measures_upload_cursor",
    ] {
        reglages.set(cle, "4242").unwrap();
    }
    for corps in [
        json!({ "cloud_listens_cursor": 0 }),
        json!({ "cloud_listens_floor": "0" }),
        json!({ "community_measures_upload_cursor": "" }),
        json!({ "cloud_listens_cursor": null }),
    ] {
        let (statut, texte) = appel(
            &b.state,
            LAN,
            None,
            "PATCH",
            "/api/v1/system/config",
            Some(corps.clone()),
        )
        .await;
        assert_eq!(
            statut,
            StatusCode::FORBIDDEN,
            "{corps} : un curseur d'envoi s'écrit depuis le réseau : {texte}"
        );
    }
    for cle in [
        "cloud_listens_cursor",
        "cloud_listens_floor",
        "community_measures_upload_cursor",
    ] {
        assert_eq!(reglages.get(cle).unwrap().as_deref(), Some("4242"), "{cle}");
    }
}

/// Refuser, éteindre ou fermer la question passe depuis tout appareil (rien
/// ne part) ; la question n'est pas posée à un autre appareil.
#[tokio::test]
async fn refuser_l_envoi_reste_possible_depuis_un_autre_appareil() {
    let b = banc();
    for corps in [
        json!({ "cloud_listens_consent": "refuse" }),
        json!({ "cloud_listens_consent": "fermee" }),
        json!({ "cloud_listens_upload_enabled": false }),
        json!({ "digest_enabled": "false" }),
    ] {
        let (statut, texte) = appel(
            &b.state,
            LAN,
            None,
            "PATCH",
            "/api/v1/system/config",
            Some(corps.clone()),
        )
        .await;
        assert_ne!(statut, StatusCode::FORBIDDEN, "{corps} : {texte}");
    }
    poser_les_comptes_lies(&b.state);
    SettingsRepo::with_backend(b.state.backend.clone())
        .set("server_id", "serveur-de-banc")
        .unwrap();
    let (_, texte) = appel(&b.state, LAN, None, "GET", "/api/v1/system/config", None).await;
    assert_eq!(
        json_de(&texte)["cloud_listens_consent"]["linked"],
        false,
        "{texte}"
    );
    let (_, texte) = appel(&b.state, ICI, None, "GET", "/api/v1/system/config", None).await;
    assert_eq!(
        json_de(&texte)["cloud_listens_consent"]["linked"],
        true,
        "{texte}"
    );
}
