//! rc14 — la session renouvelée tant qu'elle sert (`POST /auth/refresh`).
//!
//! Le jeton d'accès reste de 24 h ; la connexion « rester connecté »
//! (`remember`, session longue de #9445) rend aussi un jeton de
//! renouvellement opaque (30 jours glissants, rotation à chaque usage,
//! réutilisation = famille révoquée). Ces épreuves passent par le VRAI
//! routeur, authentification active. Le dépôt lui-même (horloge avancée,
//! SQLite et PostgreSQL) est éprouvé dans
//! `tune_core::db::jetons_de_renouvellement`.

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode, header};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt;

use tune_core::cloud::ENTETE_RELAIS;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const DISTANT: &str = "203.0.113.7:44444";
const LOCAL: &str = "127.0.0.1:5555";
const SECRET: &str = "secret-de-l-epreuve-rc14";
const MOT_DE_PASSE: &str = "mot-de-passe-ana";

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn activer_l_authentification(state: &AppState) {
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set("auth_enabled", "true").unwrap();
    s.set("jwt_secret", SECRET).unwrap();
}

/// Un compte avec mot de passe (argon2), administrateur ou non.
fn compte(state: &AppState, nom: &str, admin: bool) -> i64 {
    let hachage = tune_server::auth::hash_password(MOT_DE_PASSE).unwrap();
    let admin = i64::from(admin);
    state
        .backend
        .execute_returning_id(
            "INSERT INTO profiles (username, display_name, password_hash_v2, is_admin) VALUES (?, ?, ?, ?)",
            &[&nom as &dyn ToSqlValue, &nom, &hachage, &admin],
        )
        .unwrap()
}

fn app(state: &AppState, pair: &str) -> Router {
    let pair: SocketAddr = pair.parse().unwrap();
    tune_server::routes::router(state.clone()).layer(MockConnectInfo(pair))
}

async fn appel(
    app: &Router,
    methode: &str,
    chemin: &str,
    jeton: Option<&str>,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    appel_avec(app, methode, chemin, jeton, corps, false).await
}

async fn appel_avec(
    app: &Router,
    methode: &str,
    chemin: &str,
    jeton: Option<&str>,
    corps: Option<Value>,
    relaye: bool,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(methode).uri(chemin);
    if let Some(t) = jeton {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    if relaye {
        req = req.header(ENTETE_RELAIS, "1");
    }
    let body = match corps {
        Some(c) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let statut = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = serde_json::from_slice(&octets)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&octets).into()));
    (statut, v)
}

/// Connexion : (jeton d'accès, jeton de renouvellement).
async fn connexion(app: &Router, nom: &str) -> (String, String) {
    let (st, v) = appel(
        app,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(serde_json::json!({"username": nom, "password": MOT_DE_PASSE, "remember": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "connexion : {v}");
    (
        v["token"].as_str().unwrap().to_string(),
        v["refresh_token"].as_str().unwrap().to_string(),
    )
}

async fn renouveler(app: &Router, jeton: &str) -> (StatusCode, Value) {
    appel(
        app,
        "POST",
        "/api/v1/auth/refresh",
        None,
        Some(serde_json::json!({ "refresh_token": jeton })),
    )
    .await
}

async fn moi(app: &Router, jeton: &str) -> StatusCode {
    appel(app, "GET", "/api/v1/auth/me", Some(jeton), None)
        .await
        .0
}

/// Recule l'horloge des jetons de renouvellement en base (consommation et
/// échéance) : l'API n'a pas d'horloge à avancer.
fn reculer(state: &AppState, colonne: &str, secondes: i64) {
    state
        .backend
        .execute(
            &format!(
                "UPDATE jetons_de_renouvellement SET {colonne} = {colonne} - ? WHERE {colonne} IS NOT NULL"
            ),
            &[&secondes as &dyn ToSqlValue],
        )
        .unwrap();
}

#[tokio::test]
async fn la_connexion_rend_un_jeton_de_renouvellement_sans_rien_changer_d_autre() {
    let state = etat();
    activer_l_authentification(&state);
    compte(&state, "ana", false);
    let app = app(&state, DISTANT);
    let (st, v) = appel(
        &app,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(serde_json::json!({"username": "ana", "password": MOT_DE_PASSE, "remember": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // Le contrat d'avant, intact : un ancien client lit `token` et
    // `expires_in` et ignore le reste.
    assert_eq!(v["token_type"], "Bearer");
    assert_eq!(v["expires_in"], 86400);
    assert_eq!(v["role"], "user");
    assert!(
        v["token"]
            .as_str()
            .is_some_and(|t| t.split('.').count() == 3)
    );
    // Le nouveau : jeton opaque et sa durée.
    assert!(
        v["refresh_token"]
            .as_str()
            .is_some_and(|t| t.starts_with("tune_rt_"))
    );
    assert_eq!(v["refresh_expires_in"], 30 * 86400);
    assert_eq!(
        moi(&app, v["token"].as_str().unwrap()).await,
        StatusCode::OK
    );

    // Sans `remember` (ancien client) : 24 h, pas de renouvellement.
    let (st, v) = appel(
        &app,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(serde_json::json!({"username": "ana", "password": MOT_DE_PASSE})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["expires_in"], 86400);
    assert!(v.get("refresh_token").is_none(), "{v}");
}

#[tokio::test]
async fn renouvellement_avec_rotation() {
    let state = etat();
    activer_l_authentification(&state);
    compte(&state, "ana", true);
    let app = app(&state, DISTANT);
    let (acces, rt) = connexion(&app, "ana").await;

    let (st, v) = renouveler(&app, &rt).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let nouvel_acces = v["token"].as_str().unwrap();
    let nouveau_rt = v["refresh_token"].as_str().unwrap();
    assert_ne!(nouveau_rt, rt, "le jeton de renouvellement tourne");
    assert_eq!(v["expires_in"], 86400);
    assert_eq!(v["refresh_expires_in"], 30 * 86400);
    assert_eq!(v["role"], "admin", "le rôle est relu sur le compte");
    assert_eq!(v["username"], "ana");
    assert_eq!(moi(&app, nouvel_acces).await, StatusCode::OK);
    // L'ancien jeton d'accès reste valide jusqu'à son échéance (même famille).
    assert_eq!(moi(&app, &acces).await, StatusCode::OK);

    // Réponse perdue (5G) : l'ancien jeton rejoué tout de suite rend le
    // MÊME successeur, et rien n'est révoqué.
    let (st, v) = renouveler(&app, &rt).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["refresh_token"], nouveau_rt, "rejeu idempotent");
    assert_eq!(
        moi(&app, v["token"].as_str().unwrap()).await,
        StatusCode::OK
    );
    let (st, v) = renouveler(&app, nouveau_rt).await;
    assert_eq!(st, StatusCode::OK, "le jeton tourné sert toujours : {v}");

    // Le renouvellement pose aussi le cookie de session du client web.
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/v1/auth/refresh")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::json!({"refresh_token": v["refresh_token"]}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let cookie = resp.headers()[header::SET_COOKIE].to_str().unwrap();
    assert!(cookie.starts_with("tune_session=") && cookie.contains("HttpOnly"));
}

#[tokio::test]
async fn reutiliser_un_ancien_jeton_revoque_toute_la_famille() {
    let state = etat();
    activer_l_authentification(&state);
    compte(&state, "ana", false);
    let app = app(&state, DISTANT);
    let (_, vole) = connexion(&app, "ana").await;
    let (_, v) = renouveler(&app, &vole).await;
    let acces_legitime = v["token"].as_str().unwrap().to_string();
    let rt_legitime = v["refresh_token"].as_str().unwrap().to_string();
    assert_eq!(moi(&app, &acces_legitime).await, StatusCode::OK);

    // Dix minutes plus tard, le voleur rejoue l'ancien jeton.
    reculer(&state, "remplace_le", 600);
    let (st, v) = renouveler(&app, &vole).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(v["code"], "refresh_token_reused");

    // Toute la famille est morte : le jeton de renouvellement légitime ET le
    // jeton d'accès qu'il avait obtenu.
    let (st, v) = renouveler(&app, &rt_legitime).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(v["code"], "refresh_token_revoked");
    assert_eq!(moi(&app, &acces_legitime).await, StatusCode::UNAUTHORIZED);

    // Une autre session du même compte n'est pas touchée.
    let (acces_autre, rt_autre) = connexion(&app, "ana").await;
    assert_eq!(moi(&app, &acces_autre).await, StatusCode::OK);
    assert_eq!(renouveler(&app, &rt_autre).await.0, StatusCode::OK);
}

/// Revue de sécurité (M) : le voleur renouvelle juste avant la victime.
/// Elle rejoue et reçoit le même successeur ; au tour suivant, le second
/// rejeu de la famille révoque la session — le voleur la perd aussi.
#[tokio::test]
async fn la_course_gagnee_par_le_voleur_revoque_la_session() {
    let state = etat();
    activer_l_authentification(&state);
    compte(&state, "ana", false);
    let app = app(&state, DISTANT);
    let (_, rt1) = connexion(&app, "ana").await;
    let (st, v) = renouveler(&app, &rt1).await;
    assert_eq!(st, StatusCode::OK);
    let rt2 = v["refresh_token"].as_str().unwrap().to_string();
    let (st, v) = renouveler(&app, &rt1).await;
    assert_eq!(st, StatusCode::OK, "rejeu de la victime : {v}");
    assert_eq!(v["refresh_token"], rt2.as_str());
    let (st, v) = renouveler(&app, &rt2).await;
    assert_eq!(st, StatusCode::OK);
    let rt3_voleur = v["refresh_token"].as_str().unwrap().to_string();
    let acces_voleur = v["token"].as_str().unwrap().to_string();
    let (st, v) = renouveler(&app, &rt2).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(v["code"], "refresh_token_reused");
    assert_eq!(
        renouveler(&app, &rt3_voleur).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(moi(&app, &acces_voleur).await, StatusCode::UNAUTHORIZED);
}

/// La révocation globale réutilisable (#9564 l'appellera) : toutes les
/// sessions longues et toutes les familles.
#[tokio::test]
async fn la_revocation_globale_coupe_toutes_les_sessions() {
    let state = etat();
    activer_l_authentification(&state);
    compte(&state, "ana", false);
    compte(&state, "bob", false);
    let app = app(&state, DISTANT);
    let (a_ana, rt_ana) = connexion(&app, "ana").await;
    let (a_bob, rt_bob) = connexion(&app, "bob").await;
    tune_server::auth::revoquer_toutes_les_sessions(&state.backend);
    for (a, rt) in [(&a_ana, &rt_ana), (&a_bob, &rt_bob)] {
        assert_eq!(moi(&app, a).await, StatusCode::UNAUTHORIZED);
        assert_eq!(renouveler(&app, rt).await.0, StatusCode::UNAUTHORIZED);
    }
    // La forme transactionnelle rend le même service.
    let (a_ana, rt_ana) = connexion(&app, "ana").await;
    state
        .backend
        .write_tx(&mut |tx| {
            tune_core::db::jetons_de_renouvellement::revoquer_toutes_les_sessions_tx(
                tx,
                tune_server::session_renouvelee::maintenant(),
            )
        })
        .unwrap();
    assert_eq!(moi(&app, &a_ana).await, StatusCode::UNAUTHORIZED);
    assert_eq!(renouveler(&app, &rt_ana).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn trente_jours_sans_usage_la_session_expire() {
    let state = etat();
    activer_l_authentification(&state);
    compte(&state, "ana", false);
    let app = app(&state, DISTANT);
    let (_, rt) = connexion(&app, "ana").await;
    reculer(&state, "expire_le", 30 * 86400);
    let (st, v) = renouveler(&app, &rt).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(v["code"], "refresh_token_expired");

    // Un jeton inconnu, un corps sans jeton.
    let (st, v) = renouveler(&app, "tune_rt_inconnu").await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(v["code"], "refresh_token_invalid");
    let (st, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/refresh",
        None,
        Some(serde_json::json!({})),
    )
    .await;
    assert!(st.is_client_error());
}

#[tokio::test]
async fn la_deconnexion_revoque_la_session_et_seulement_elle() {
    let state = etat();
    activer_l_authentification(&state);
    compte(&state, "ana", false);
    let app = app(&state, DISTANT);
    let (acces_telephone, rt_telephone) = connexion(&app, "ana").await;
    let (acces_web, rt_web) = connexion(&app, "ana").await;

    // Déconnexion du téléphone par son jeton d'accès (corps vide : ancien
    // client).
    let (st, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/logout",
        Some(&acces_telephone),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(
        renouveler(&app, &rt_telephone).await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(moi(&app, &acces_telephone).await, StatusCode::UNAUTHORIZED);
    // Le navigateur reste connecté.
    assert_eq!(moi(&app, &acces_web).await, StatusCode::OK);

    // Déconnexion par le seul jeton de renouvellement (jeton d'accès perdu).
    let (st, _) = appel(
        &app,
        "POST",
        "/api/v1/auth/logout",
        None,
        Some(serde_json::json!({ "refresh_token": rt_web })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(renouveler(&app, &rt_web).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(moi(&app, &acces_web).await, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn changer_le_mot_de_passe_revoque_les_sessions() {
    let state = etat();
    activer_l_authentification(&state);
    let id = compte(&state, "ana", false);
    let app = app(&state, DISTANT);
    let (_, rt) = connexion(&app, "ana").await;
    let nouveau = tune_server::auth::hash_password("un-autre-mot-de-passe").unwrap();
    state
        .backend
        .execute(
            "UPDATE profiles SET password_hash_v2 = ? WHERE id = ?",
            &[&nouveau as &dyn ToSqlValue, &id],
        )
        .unwrap();
    let (st, v) = renouveler(&app, &rt).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(v["code"], "refresh_token_revoked");
}

#[tokio::test]
async fn renouveler_les_secrets_revoque_toutes_les_sessions() {
    // Authentification coupée : l'opérateur de cette machine se connecte,
    // puis l'active — `renouveler_les_secrets`.
    let state = etat();
    compte(&state, "ana", true);
    let app = app(&state, LOCAL);
    let (_, rt) = connexion(&app, "ana").await;
    let (st, v) = appel(
        &app,
        "POST",
        "/api/v1/auth/config",
        None,
        Some(serde_json::json!({ "auth_enabled": true })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, v) = renouveler(&app, &rt).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "{v}");
    assert_eq!(v["code"], "refresh_token_revoked");

    // Un secret posé à la main par l'administrateur : idem.
    let (acces, rt) = connexion(&app, "ana").await;
    let (st, v) = appel(
        &app,
        "POST",
        "/api/v1/auth/config",
        Some(&acces),
        Some(serde_json::json!({ "jwt_secret": "un-secret-neuf" })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(renouveler(&app, &rt).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn jamais_de_jeton_en_clair_en_base() {
    let state = etat();
    activer_l_authentification(&state);
    compte(&state, "ana", false);
    let app = app(&state, DISTANT);
    let (_, rt1) = connexion(&app, "ana").await;
    let (_, v) = renouveler(&app, &rt1).await;
    let rt2 = v["refresh_token"].as_str().unwrap().to_string();
    let mut lignes = state
        .backend
        .query_many("SELECT * FROM jetons_de_renouvellement", &[])
        .unwrap();
    assert_eq!(lignes.len(), 2);
    lignes.extend(
        state
            .backend
            .query_many("SELECT * FROM settings", &[])
            .unwrap(),
    );
    for ligne in &lignes {
        for valeur in ligne {
            if let Some(texte) = valeur.as_string() {
                for rt in [&rt1, &rt2] {
                    assert!(
                        !texte.contains(&rt["tune_rt_".len()..]),
                        "jeton en clair en base : {texte}"
                    );
                }
            }
        }
    }
}

#[tokio::test]
async fn le_relais_transmet_le_renouvellement_comme_la_connexion() {
    let state = etat();
    activer_l_authentification(&state);
    compte(&state, "ana", false);
    let app = app(&state, LOCAL);
    let (st, v) = appel_avec(
        &app,
        "POST",
        "/api/v1/auth/login",
        None,
        Some(serde_json::json!({"username": "ana", "password": MOT_DE_PASSE, "remember": true})),
        true,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "connexion relayée : {v}");
    let rt = v["refresh_token"].as_str().unwrap();
    let (st, v) = appel_avec(
        &app,
        "POST",
        "/api/v1/auth/refresh",
        None,
        Some(serde_json::json!({ "refresh_token": rt })),
        true,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "renouvellement relayé : {v}");
    let rt = v["refresh_token"].as_str().unwrap();
    let (st, _) = appel_avec(
        &app,
        "POST",
        "/api/v1/auth/logout",
        None,
        Some(serde_json::json!({ "refresh_token": rt })),
        true,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "déconnexion relayée");
    assert_eq!(renouveler(&app, rt).await.0, StatusCode::UNAUTHORIZED);
}

/// Un jeton d'accès de la famille `fam` qui expire dans `reste` secondes
/// APRÈS la tolérance de 60 s de `jsonwebtoken`.
fn jeton_bientot_expire(sub: i64, fam: &str, reste: i64) -> String {
    let maintenant = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let claims = tune_server::auth::JwtClaims {
        sub: sub.to_string(),
        iat: (maintenant - 3600) as u64,
        exp: (maintenant - 60 + reste) as u64,
        role: "user".into(),
        sid: Some(fam.to_string()),
    };
    jsonwebtoken::encode(
        &jsonwebtoken::Header::default(),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(SECRET.as_bytes()),
    )
    .unwrap()
}

/// Lit la socket jusqu'à sa fermeture ou l'échéance : `true` si elle s'est
/// fermée.
async fn fermee_avant(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    delai: Duration,
) -> bool {
    let fin = tokio::time::Instant::now() + delai;
    loop {
        match tokio::time::timeout_at(fin, ws.next()).await {
            Err(_) => return false,
            Ok(None) | Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) => return true,
            Ok(Some(Ok(_))) => continue,
        }
    }
}

/// La WebSocket n'est PAS coupée à l'expiration du jeton qui l'a ouverte ;
/// elle l'est à la révocation de SA session, et pas d'une autre.
#[tokio::test(flavor = "multi_thread")]
async fn la_websocket_survit_a_l_expiration_mais_pas_a_la_revocation() {
    let state = etat();
    activer_l_authentification(&state);
    let id = compte(&state, "ana", false);
    let routeur = app(&state, DISTANT);
    let (acces, rt) = connexion(&routeur, "ana").await;
    let fam = tune_server::auth::verify_jwt(&acces, SECRET)
        .unwrap()
        .sid
        .expect("le jeton de connexion porte sa session");

    let serveur = axum::Router::new()
        .nest("/ws", tune_server::routes::ws::router())
        .with_state(state.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    let tache = tokio::spawn(async move { axum::serve(ecoute, serveur).await.unwrap() });

    // Poignée de main avec un jeton valide encore 2 s.
    let jeton = jeton_bientot_expire(id, &fam, 2);
    let (mut ws, reponse) =
        tokio_tungstenite::connect_async(format!("ws://{adresse}/ws?token={jeton}"))
            .await
            .unwrap();
    assert_eq!(reponse.status(), StatusCode::SWITCHING_PROTOCOLS);

    // Le jeton expire : l'API le refuse, la socket reste ouverte.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(moi(&routeur, &jeton).await, StatusCode::UNAUTHORIZED);
    // Et une nouvelle poignée de main avec ce jeton est refusée.
    assert!(
        tokio_tungstenite::connect_async(format!("ws://{adresse}/ws?token={jeton}"))
            .await
            .is_err()
    );
    // La révocation d'une AUTRE session ne la ferme pas.
    let (_, rt_autre) = connexion(&routeur, "ana").await;
    appel(
        &routeur,
        "POST",
        "/api/v1/auth/logout",
        None,
        Some(serde_json::json!({ "refresh_token": rt_autre })),
    )
    .await;
    ws.send(Message::Ping(vec![1].into())).await.unwrap();
    assert!(
        !fermee_avant(&mut ws, Duration::from_secs(2)).await,
        "une socket expirée mais non révoquée doit rester ouverte"
    );

    // La révocation de SA session la ferme.
    let (st, _) = appel(
        &routeur,
        "POST",
        "/api/v1/auth/logout",
        None,
        Some(serde_json::json!({ "refresh_token": rt })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        fermee_avant(&mut ws, Duration::from_secs(5)).await,
        "la révocation de la session doit fermer la socket"
    );
    tache.abort();
}
