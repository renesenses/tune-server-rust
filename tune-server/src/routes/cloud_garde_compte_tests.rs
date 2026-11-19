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
