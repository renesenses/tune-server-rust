//! Changer de compte relié fait suivre le propriétaire de la licence — et
//! seulement dans les cas permis (`tune_core::cloud::proprietaire::
//! le_proprietaire_suit`).
//!
//! Le défaut : la déliaison efface les modules payants mais garde
//! `owner_profile_id`. Le compte relié ensuite résout un AUTRE profil, qui
//! n'était pas propriétaire : il ne réécrivait jamais les modules. Les
//! Réglages les montraient (ils lisent le compte relié), le serveur les
//! refusait (`module_not_owned`).
//!
//! Ces tests jouent la vraie ronde (autorisation puis retour) sur le vrai
//! routeur, contre un faux site à deux comptes.

use std::net::SocketAddr;

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::cloud::proprietaire::{CLE_PROPRIETAIRE, proprietaire};
use tune_core::db::profile_repo::ProfileRepo;
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

const ICI: &str = "127.0.0.1:53000";
const HOTE_ICI: (&str, &str) = ("host", "127.0.0.1:8888");
const LAN: &str = "192.168.1.50:53000";
const HOTE_LAN: (&str, &str) = ("host", "192.168.1.10:8888");
const MODULE: &str = "module-payant";

struct Banc {
    state: AppState,
    _dossier: tempfile::TempDir,
}

/// Un serveur neuf relié à un faux site : le code `a` donne le compte
/// `a@exemple.test` (qui possède le module payant), le code `b` le compte
/// `b@exemple.test`, avec `modules_b`.
async fn banc(modules_b: &[&str]) -> Banc {
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");
    let state = AppState::new(base.to_str().unwrap(), 0, Default::default()).unwrap();
    let site = faux_site(modules_b.iter().map(|m| m.to_string()).collect()).await;
    reglages(&state).set("mozaik_base_url", &site).unwrap();
    Banc {
        state,
        _dossier: dossier,
    }
}

async fn faux_site(modules_b: Vec<String>) -> String {
    use axum::routing::{get, post};
    let app = axum::Router::new()
        .route(
            "/oauth/token",
            post(
                |axum::Form(f): axum::Form<std::collections::HashMap<String, String>>| async move {
                    let code = f.get("code").cloned().unwrap_or_default();
                    axum::Json(json!({
                        "access_token": format!("jeton-{code}"),
                        "refresh_token": format!("renouv-{code}"),
                        "expires_in": 3600
                    }))
                },
            ),
        )
        .route(
            "/api/v1/user",
            get(move |entetes: axum::http::HeaderMap| {
                let modules_b = modules_b.clone();
                async move {
                    let jeton = entetes
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .trim_start_matches("Bearer ")
                        .to_string();
                    let (id, qui, modules) = if jeton == "jeton-a" {
                        (101, "a", vec![MODULE.to_string()])
                    } else {
                        (102, "b", modules_b)
                    };
                    axum::Json(json!({
                        "id": id,
                        "email": format!("{qui}@exemple.test"),
                        "display_name": qui,
                        "is_admin": false,
                        "avatar_url": null,
                        "modules": modules,
                    }))
                }
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

fn reglages(state: &AppState) -> SettingsRepo {
    SettingsRepo::with_backend(state.backend.clone())
}

fn json_de(texte: &str) -> Value {
    serde_json::from_str(texte).unwrap_or(Value::Null)
}

async fn appel(
    state: &AppState,
    methode: &str,
    depuis: &str,
    uri: &str,
    hote: (&str, &str),
    jeton: Option<&str>,
) -> (StatusCode, String) {
    let adresse: SocketAddr = depuis.parse().unwrap();
    let mut requete = Request::builder()
        .method(methode)
        .uri(uri)
        .extension(ConnectInfo(adresse))
        .header(hote.0, hote.1);
    if let Some(j) = jeton {
        requete = requete.header(header::AUTHORIZATION, format!("Bearer {j}"));
    }
    let reponse = crate::routes::router(state.clone())
        .oneshot(requete.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = to_bytes(reponse.into_body(), usize::MAX).await.unwrap();
    (statut, String::from_utf8_lossy(&octets).into_owned())
}

/// Une ronde complète (autorisation, puis retour au bon `state` avec `code`).
async fn relier(
    state: &AppState,
    depuis: &str,
    hote: (&str, &str),
    jeton: Option<&str>,
    code: &str,
) {
    let (statut, page) = appel(
        state,
        "GET",
        depuis,
        "/api/v1/cloud/sso/authorize",
        hote,
        jeton,
    )
    .await;
    assert_eq!(
        statut,
        StatusCode::TEMPORARY_REDIRECT,
        "autorisation — {page}"
    );
    let attente = reglages(state)
        .get("mozaik_pkce_pending")
        .unwrap()
        .unwrap_or_default();
    let etat = json_de(&attente)["state"].as_str().unwrap().to_string();
    let (statut, page) = appel(
        state,
        "GET",
        depuis,
        &format!(
            "/api/v1/cloud/sso/callback?code={code}&state={}",
            urlencoding::encode(&etat)
        ),
        hote,
        jeton,
    )
    .await;
    assert_eq!(statut, StatusCode::TEMPORARY_REDIRECT, "retour — {page}");
}

async fn delier(state: &AppState, jeton: Option<&str>) {
    let (statut, texte) = appel(
        state,
        "POST",
        ICI,
        "/api/v1/cloud/sso/disconnect",
        HOTE_ICI,
        jeton,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "déliaison — {texte}");
}

fn profil_de(state: &AppState, courriel: &str) -> i64 {
    tune_core::cloud::proprietaire::profil_du_courriel(&state.backend, courriel)
        .unwrap_or_else(|| panic!("aucun profil pour {courriel}"))
}

fn courriel_relie(state: &AppState) -> String {
    let compte = reglages(state)
        .get("mozaik_user")
        .unwrap()
        .unwrap_or_default();
    json_de(&compte)["email"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// 🔴 → 🟢 Le défaut. A est propriétaire avec le module payant ; A est délié
/// depuis cette machine, puis cette machine relie B, qui possède aussi le
/// module : B doit l'obtenir tout de suite, et devenir propriétaire.
#[tokio::test]
async fn apres_deliaison_le_compte_suivant_obtient_ses_modules() {
    let b = banc(&[MODULE]).await;
    let s = reglages(&b.state);

    relier(&b.state, ICI, HOTE_ICI, None, "a").await;
    let profil_a = profil_de(&b.state, "a@exemple.test");
    assert_eq!(proprietaire(&s), Some(profil_a));
    assert!(b.state.license.has_module(MODULE).await, "A a son module");

    delier(&b.state, None).await;
    // La déliaison laisse un état cohérent : plus de compte, plus de module,
    // et la place du propriétaire gardée (rien à prendre tant qu'elle l'est).
    assert!(s.get("mozaik_user").unwrap().is_none());
    assert!(s.get("mozaik_access_token").unwrap().is_none());
    assert!(!b.state.license.has_module(MODULE).await);
    assert!(b.state.license.modules().await.is_empty());
    assert_eq!(proprietaire(&s), Some(profil_a));

    relier(&b.state, ICI, HOTE_ICI, None, "b").await;
    let profil_b = profil_de(&b.state, "b@exemple.test");
    assert_ne!(profil_a, profil_b);
    assert_eq!(courriel_relie(&b.state), "b@exemple.test");
    assert_eq!(
        proprietaire(&s),
        Some(profil_b),
        "la propriété n'a pas suivi le compte relié"
    );
    assert!(
        b.state.license.has_module(MODULE).await,
        "B possède le module mais le serveur le refuse (module_not_owned)"
    );
}

/// Témoin de non-régression : pendant que le compte du propriétaire est
/// relié, cette machine relie un autre compte. La liaison réussit, mais la
/// propriété ne bouge pas et l'état de licence du propriétaire reste.
#[tokio::test]
async fn un_autre_compte_relie_sans_deliaison_ne_prend_pas_la_propriete() {
    let b = banc(&[]).await;
    let s = reglages(&b.state);

    relier(&b.state, ICI, HOTE_ICI, None, "a").await;
    let profil_a = profil_de(&b.state, "a@exemple.test");

    relier(&b.state, ICI, HOTE_ICI, None, "b").await;
    assert_eq!(courriel_relie(&b.state), "b@exemple.test");
    assert_eq!(proprietaire(&s), Some(profil_a), "B a pris la propriété");
    assert!(
        b.state.license.has_module(MODULE).await,
        "le module du propriétaire est tombé"
    );
}

/// Témoin de non-régression : le réseau local ne délie pas, il ne fait donc
/// jamais suivre la propriété — même quand aucun compte n'est relié.
#[tokio::test]
async fn le_reseau_local_ne_prend_jamais_la_propriete() {
    let b = banc(&[MODULE]).await;
    let s = reglages(&b.state);

    relier(&b.state, ICI, HOTE_ICI, None, "a").await;
    let profil_a = profil_de(&b.state, "a@exemple.test");
    delier(&b.state, None).await;

    relier(&b.state, LAN, HOTE_LAN, None, "b").await;
    assert_eq!(courriel_relie(&b.state), "b@exemple.test");
    assert_eq!(proprietaire(&s), Some(profil_a), "le réseau local a pris la propriété");
    assert!(!b.state.license.has_module(MODULE).await);
}

/// Authentification active : un administrateur qui n'est PAS le propriétaire
/// relie un autre compte pendant que celui du propriétaire est relié — la
/// propriété ne bouge pas. Le propriétaire authentifié, lui, la fait suivre.
#[tokio::test]
async fn authentification_active_seul_le_proprietaire_fait_suivre() {
    let b = banc(&[MODULE]).await;
    let s = reglages(&b.state);
    s.set("jwt_secret", "secret-de-banc").unwrap();
    s.set("auth_enabled", "true").unwrap();
    let profils = ProfileRepo::with_backend(b.state.backend.clone());
    let admin = profils.create("admin", Some("Admin"), None).unwrap();
    let autre_admin = profils.create("admin2", Some("Admin 2"), None).unwrap();
    let jeton_admin = crate::auth::sign_jwt(admin, "admin", "secret-de-banc").unwrap();
    let jeton_autre = crate::auth::sign_jwt(autre_admin, "admin", "secret-de-banc").unwrap();

    // Le compte A est relié et son profil est l'administrateur `admin`.
    use tune_core::db::backend::ToSqlValue;
    b.state
        .backend
        .execute(
            "UPDATE profiles SET email = ? WHERE id = ?",
            &[&"a@exemple.test" as &dyn ToSqlValue, &admin as &dyn ToSqlValue],
        )
        .unwrap();
    relier(&b.state, ICI, HOTE_ICI, Some(&jeton_admin), "a").await;
    assert_eq!(proprietaire(&s), Some(admin));

    // Un autre administrateur relie B : pas de transfert.
    relier(&b.state, LAN, HOTE_LAN, Some(&jeton_autre), "b").await;
    assert_eq!(courriel_relie(&b.state), "b@exemple.test");
    assert_eq!(
        s.get(CLE_PROPRIETAIRE).unwrap().as_deref(),
        Some(admin.to_string().as_str()),
        "un administrateur non propriétaire a pris la propriété"
    );

    // Le propriétaire authentifié relie B à son tour : la propriété suit.
    relier(&b.state, LAN, HOTE_LAN, Some(&jeton_admin), "b").await;
    assert_eq!(proprietaire(&s), Some(profil_de(&b.state, "b@exemple.test")));
    assert!(b.state.license.has_module(MODULE).await);
}
