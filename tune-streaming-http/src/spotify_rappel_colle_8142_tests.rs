//! renesenses/tune-server-rust#8142 — `POST /streaming/spotify/auth` avec
//! `callback_url` : une adresse collée sans `code`, un refus d'accès
//! (`error=access_denied`) et un `state` étranger à la demande en cours
//! rendent chacun un 400 avec son propre `code`, et non plus un texte brut que
//! l'écran traduisait en « Le serveur n'a pas pu traiter la demande ».
//!
//! Le vrai `SpotifyService`, par le vrai routeur ; l'échange de jeton du cas
//! normal va vers une doublure locale, jamais vers Spotify.

use super::*;
use std::sync::Mutex as VerrouSimple;
use tower::ServiceExt;
use tune_core::db::sqlite::SqliteDb;
use tune_core::streaming::spotify::{
    CODE_AUTORISATION_REFUSEE, CODE_CALLBACK_SANS_CODE, CODE_STATE_INCONNU, SpotifyService,
};

const RETOUR: &str = "http://127.0.0.1:8888/api/v1/streaming/spotify/callback";

fn app(svc: SpotifyService) -> Router {
    let backend: Arc<dyn DbBackend> =
        Arc::new(SqliteDb::open_in_memory().expect("sqlite en memoire"));
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(svc));
    let etat = StreamingHttpState::new(
        backend,
        Arc::new(Mutex::new(registre)),
        Arc::new(EventBus::new()),
    );
    Router::new().nest("/api/v1/streaming", router().with_state(etat))
}

async fn auth(app: &Router, corps: Value) -> (StatusCode, Value) {
    let r = app
        .clone()
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/api/v1/streaming/spotify/auth")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(corps.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = r.status();
    let octets = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
    let corps = serde_json::from_slice(&octets)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&octets).into_owned()));
    (statut, corps)
}

/// Ouvre une autorisation (« Se connecter ») et rend le `state` de son lien.
async fn ouvrir(app: &Router) -> String {
    let (statut, corps) = auth(app, json!({})).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let lien = corps["verification_url"]
        .as_str()
        .unwrap_or_else(|| panic!("lien d'autorisation attendu : {corps}"));
    let requete = lien.split_once('?').expect("lien avec requete").1;
    requete
        .split('&')
        .find_map(|p| p.strip_prefix("state="))
        .expect("le lien porte un state")
        .to_owned()
}

fn colle(adresse: String) -> Value {
    json!({ "callback_url": adresse })
}

fn assert_refus(statut: StatusCode, corps: &Value, code: &str) {
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
    assert_eq!(
        corps["code"], code,
        "le refus doit porter le code {code}, pas un texte brut : {corps}"
    );
    assert_eq!(corps["service"], "spotify", "{corps}");
    assert!(
        corps["error"].as_str().is_some_and(|m| !m.is_empty()),
        "{corps}"
    );
}

fn service() -> SpotifyService {
    SpotifyService::with_config(Some("client-essai"), None, 8888)
}

/// Le cas du testeur : l'adresse de redirection seule, recopiée du tableau de
/// bord Spotify, sans `?code=`.
#[tokio::test]
async fn adresse_sans_code_rend_spotify_callback_sans_code() {
    let app = app(service());
    let state = ouvrir(&app).await;
    for adresse in [
        RETOUR.to_owned(),
        format!("{RETOUR}?state={state}"),
        format!("{RETOUR}?code=&state={state}"),
    ] {
        let (statut, corps) = auth(&app, colle(adresse.clone())).await;
        assert_refus(statut, &corps, CODE_CALLBACK_SANS_CODE);
    }
}

/// « Annuler » sur la page de consentement Spotify.
#[tokio::test]
async fn acces_refuse_rend_spotify_autorisation_refusee() {
    let app = app(service());
    let state = ouvrir(&app).await;
    let (statut, corps) = auth(
        &app,
        colle(format!("{RETOUR}?error=access_denied&state={state}")),
    )
    .await;
    assert_refus(statut, &corps, CODE_AUTORISATION_REFUSEE);
}

/// Une adresse d'une autre tentative, et une adresse d'avant un redémarrage
/// (aucune tentative en mémoire).
#[tokio::test]
async fn state_etranger_rend_spotify_state_inconnu() {
    let app_ouverte = app(service());
    ouvrir(&app_ouverte).await;
    let (statut, corps) = auth(
        &app_ouverte,
        colle(format!("{RETOUR}?code=abc&state=autre-tentative")),
    )
    .await;
    assert_refus(statut, &corps, CODE_STATE_INCONNU);

    let app_redemarree = app(service());
    let (statut, corps) = auth(
        &app_redemarree,
        colle(format!("{RETOUR}?code=abc&state=d-avant-le-redemarrage")),
    )
    .await;
    assert_refus(statut, &corps, CODE_STATE_INCONNU);
}

/// Doublure des comptes Spotify : rend un jeton pour tout code reçu, et
/// garde les codes reçus.
async fn doublure() -> (String, Arc<VerrouSimple<Vec<String>>>) {
    let recus = Arc::new(VerrouSimple::new(Vec::new()));
    let pour_jeton = recus.clone();
    let doublure = Router::new()
        .route(
            "/api/token",
            post(move |corps: String| {
                let recus = pour_jeton.clone();
                async move {
                    let code = corps
                        .split('&')
                        .find_map(|p| p.strip_prefix("code="))
                        .unwrap_or_default()
                        .to_owned();
                    recus.lock().unwrap().push(code);
                    Json(json!({
                        "access_token": "jeton-essai",
                        "refresh_token": "renouveau-essai",
                        "expires_in": 3600,
                        "scope": "user-read-private",
                    }))
                }
            }),
        )
        .route(
            "/v1/me",
            get(|| async { Json(json!({"display_name": "Essai", "id": "u-essai"})) }),
        );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(ecoute, doublure).await.unwrap() });
    (base, recus)
}

/// Le parcours normal ne change pas : l'adresse complète, avec `code` et le
/// `state` de la demande en cours, aboutit à l'échange de jeton et connecte.
#[tokio::test]
async fn adresse_complete_connecte_comme_avant() {
    let (base, recus) = doublure().await;
    let app = app(service().avec_doublure(&base));
    let state = ouvrir(&app).await;
    let (statut, corps) = auth(
        &app,
        colle(format!("{RETOUR}?code=code-essai&state={state}")),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["authenticated"], true, "{corps}");
    assert_eq!(corps["username"], "Essai", "{corps}");
    assert_eq!(*recus.lock().unwrap(), vec!["code-essai".to_owned()]);
}
