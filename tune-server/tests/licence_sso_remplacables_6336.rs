//! #6336 (reliquat de #6173, suite de #6615) — la licence et le SSO éprouvés
//! contre un faux mozaiklabs.fr, sans appel réseau réel ni secret réel.
//!
//! #6615 tenait la licence et le SSO pour déjà redirigeables par le réglage
//! `mozaik_base_url`. C'est vrai de l'activation, de la validation de clé, de
//! l'échange PKCE, du profil de compte et de la déliaison. Deux appels n'en
//! tenaient pas compte et partaient vers une adresse en dur :
//!
//! - **le battement horaire** (`POST /api/v1/heartbeat`), qui porte le verdict
//!   de la licence à clé : palier confirmé, révocation, licence tenue par un
//!   autre serveur (`session_conflict`). Aucun banc ne pouvait le jouer ;
//! - **la liaison du serveur au compte** (`POST
//!   /api/v1/cloud-library/{server_id}/link`), lancée par la connexion SSO et
//!   par le battement de compte, jeton OAuth en tête.
//!
//! Les deux consultent maintenant `tune_core::services_factices::base_substituee`
//! sous le nom `MOZAIKLABS`. La substitution n'existe que sous la feature
//! `services-factices`, allumée par les seuls `[dev-dependencies]` de
//! tune-server (garde : `services_remplacables_6336.rs`) ; dans un binaire
//! publié, la racine reste `https://mozaiklabs.fr`.
//!
//! Le faux mozaiklabs est UN serveur axum sur `127.0.0.1`, tenu par son propre
//! fil pour tout le processus : la substitution est un état de processus, et
//! les tests tournent en parallèle. Il répond au battement selon la clé de
//! licence reçue, si bien que chaque test a son verdict sans se partager rien
//! d'autre.

use axum::body::Body;
use axum::extract::Path;
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::{Value, json};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::test_scratch::ScratchDir;
use tune_server::state::AppState;

const STATUS: &str = "/api/v1/cloud/license/status";
const AUTHORIZE: &str = "/api/v1/cloud/sso/authorize";

const CLE_CONFIRMEE: &str = "TUNE-6336-CONFIRMEE";
const CLE_CONFLIT: &str = "TUNE-6336-CONFLIT";
const CLE_EXPIREE: &str = "TUNE-6336-EXPIREE";
const AUTRE_SERVEUR: &str = "salon-6336";
const JETON_ACCES: &str = "jeton-acces-factice-6336";
const JETON_LIAISON: &str = "jeton-liaison-factice-6336";

/// Le battement part 10 s après le lancement de sa boucle : la marge couvre
/// ce délai, plus une machine de CI chargée.
const ATTENTE_MAX: Duration = Duration::from_secs(45);

// ── Le faux mozaiklabs ────────────────────────────────────────────────────

/// Ce que le faux a reçu : (chemin, en-tête Authorization, corps JSON).
static RECUS: Mutex<Vec<(String, Option<String>, Value)>> = Mutex::new(Vec::new());

fn noter(chemin: String, entetes: &HeaderMap, corps: Value) {
    let auth = entetes
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    RECUS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((chemin, auth, corps));
}

fn recus_sur(prefixe: &str) -> Vec<(String, Option<String>, Value)> {
    RECUS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|(c, _, _)| c.starts_with(prefixe))
        .cloned()
        .collect()
}

/// Le verdict du faux pour une clé : celui que mozaiklabs rendrait.
fn verdict_pour(cle: Option<&str>) -> Value {
    match cle {
        Some(CLE_CONFIRMEE) => json!({
            "license_valid": true,
            "license_tier": "premium",
            "license_expires_at": "2099-01-01T00:00:00Z",
        }),
        Some(CLE_CONFLIT) => json!({
            "session_conflict": true,
            "active_server": AUTRE_SERVEUR,
            "active_since": "2026-10-20T08:00:00Z",
        }),
        Some(CLE_EXPIREE) => json!({
            "license_valid": false,
            "license_tier": "premium",
            "license_expires_at": "2020-01-01T00:00:00Z",
        }),
        _ => json!({}),
    }
}

/// La racine du faux, démarré une fois pour tout le binaire, et déclarée comme
/// racine mozaiklabs du processus.
fn faux_mozaiklabs() -> &'static str {
    static BASE: OnceLock<String> = OnceLock::new();
    let base = BASE.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime du faux mozaiklabs");
            rt.block_on(async move {
                let app = axum::Router::new()
                    .route(
                        "/api/v1/heartbeat",
                        axum::routing::post(
                            |h: HeaderMap, axum::Json(corps): axum::Json<Value>| async move {
                                let verdict = verdict_pour(corps["license_key"].as_str());
                                noter("/api/v1/heartbeat".into(), &h, corps);
                                axum::Json(verdict)
                            },
                        ),
                    )
                    .route(
                        "/api/v1/cloud-library/{server_id}/link",
                        axum::routing::post(
                            |Path(server_id): Path<String>, h: HeaderMap| async move {
                                noter(
                                    format!("/api/v1/cloud-library/{server_id}/link"),
                                    &h,
                                    Value::Null,
                                );
                                axum::Json(
                                    json!({ "token": JETON_LIAISON, "server_id": server_id }),
                                )
                            },
                        ),
                    )
                    .route(
                        "/oauth/token",
                        axum::routing::post(|| async {
                            axum::Json(json!({
                                "access_token": JETON_ACCES,
                                "refresh_token": "jeton-rafraichissement-factice-6336",
                                "expires_in": 2_592_000,
                            }))
                        }),
                    )
                    .route(
                        "/api/v1/user",
                        axum::routing::get(|| async {
                            axum::Json(json!({
                                "id": 6336,
                                "email": "essai-6336@example.invalid",
                                "display_name": "Essai 6336",
                                "is_admin": false,
                                "avatar_url": null,
                                "premium": true,
                                "license_expires_at": "2099-01-01T00:00:00Z",
                                "qobuz_proxy_first": false,
                                "modules": [],
                            }))
                        }),
                    );
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                tx.send(format!("http://{}", listener.local_addr().unwrap()))
                    .unwrap();
                axum::serve(listener, app).await.ok();
            });
        });
        rx.recv().expect("adresse du faux mozaiklabs")
    });
    tune_core::services_factices::substituer_base(tune_core::services_factices::MOZAIKLABS, base);
    base
}

// ── Outils communs ────────────────────────────────────────────────────────

/// Un serveur Tune neuf, dont TOUT mozaiklabs est le faux : le réglage
/// `mozaik_base_url` pour ce qui le lit déjà, la substitution pour le reste.
fn serveur(dossier: &ScratchDir) -> (axum::Router, AppState) {
    let base = faux_mozaiklabs();
    let db = dossier.join("tune.db");
    let state = AppState::new(db.to_str().unwrap(), 0, Default::default()).expect("état isolé");
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("mozaik_base_url", base).unwrap();
    settings.set("mozaik_client_id", "tune-server").unwrap();
    (tune_server::routes::router(state.clone()), state)
}

/// Lancer la VRAIE boucle du battement. Elle n'envoie sa charge que si la
/// télémétrie est acceptée : le réglage le dit, et l'environnement ne doit pas
/// l'avoir refusée pour tout le processus.
fn lancer_le_battement(state: &AppState) {
    assert!(
        tune_core::cloud::telemetry::TelemetryReporter::is_enabled(),
        "TUNE_TELEMETRY refuse la télémétrie pour ce processus : le battement \
         ne partirait pas, et ce banc ne prouverait rien"
    );
    SettingsRepo::with_backend(state.backend.clone())
        .set(tune_core::cloud::telemetry::TELEMETRY_SETTING_KEY, "true")
        .unwrap();
    tune_server::background::spawn_heartbeat(state);
}

async fn appeler(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn statut(app: &axum::Router) -> Value {
    let (code, corps) = appeler(app, Request::get(STATUS).body(Body::empty()).unwrap()).await;
    assert_eq!(code, StatusCode::OK, "{corps}");
    corps
}

/// Sonder `/cloud/license/status` jusqu'à ce que `condition` tienne, ou
/// échouer en montrant le dernier statut et ce que le faux a reçu.
async fn attendre_le_statut(
    app: &axum::Router,
    quoi: &str,
    condition: impl Fn(&Value) -> bool,
) -> Value {
    let debut = std::time::Instant::now();
    loop {
        let corps = statut(app).await;
        if condition(&corps) {
            return corps;
        }
        if debut.elapsed() > ATTENTE_MAX {
            panic!(
                "{quoi} : non atteint en {ATTENTE_MAX:?}. Dernier statut : tier={}, \
                 session_conflict={}. Battements reçus par le faux mozaiklabs : {}",
                corps["tier"],
                corps["session_conflict"],
                recus_sur("/api/v1/heartbeat").len()
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Le battement reçu par le faux pour une clé donnée.
fn battement_de(cle: &str) -> Option<Value> {
    recus_sur("/api/v1/heartbeat")
        .into_iter()
        .map(|(_, _, corps)| corps)
        .find(|c| c["license_key"] == cle)
}

// ── Licence : le verdict porté par le battement ───────────────────────────

/// Une clé en attente (palier Free) devient Premium par le seul battement :
/// la boucle de production joint le faux, et son verdict se lit par le vrai
/// routeur. Le faux a reçu la clé et le `server_id` de CE serveur.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_battement_confirme_une_cle_par_le_faux_mozaiklabs() {
    let dossier = tune_core::test_scratch::scratch_dir("tune-6336-battement-confirme");
    let (app, state) = serveur(&dossier);
    state.license.set_license_key(CLE_CONFIRMEE).await.unwrap();
    assert_eq!(
        statut(&app).await["tier"],
        "free",
        "départ : clé en attente"
    );

    lancer_le_battement(&state);

    let corps = attendre_le_statut(&app, "palier premium confirmé par le battement", |s| {
        s["tier"] == "premium"
    })
    .await;
    assert_eq!(corps["expires_at"], "2099-01-01T00:00:00Z", "{corps}");

    let recu = battement_de(CLE_CONFIRMEE).expect("le faux n'a reçu aucun battement pour la clé");
    let server_id = SettingsRepo::with_backend(state.backend.clone())
        .get("server_id")
        .unwrap()
        .expect("le battement crée le server_id");
    assert_eq!(recu["server_id"], server_id.as_str(), "battement : {recu}");
}

/// La licence tenue par un autre serveur : le battement pose le conflit de
/// session, le palier effectif retombe en Free, et le statut dit où elle est.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_battement_signale_une_licence_tenue_par_un_autre_serveur() {
    let dossier = tune_core::test_scratch::scratch_dir("tune-6336-battement-conflit");
    let (app, state) = serveur(&dossier);
    state.license.set_license_key(CLE_CONFLIT).await.unwrap();
    state
        .license
        .update_from_server(
            tune_core::license::Tier::Premium,
            Some("2099-01-01T00:00:00Z".into()),
        )
        .await;
    assert_eq!(statut(&app).await["tier"], "premium", "départ : premium");

    lancer_le_battement(&state);

    let corps = attendre_le_statut(&app, "conflit de session posé par le battement", |s| {
        !s["session_conflict"].is_null()
    })
    .await;
    assert_eq!(
        corps["session_conflict"]["active_server"], AUTRE_SERVEUR,
        "{corps}"
    );
    assert_eq!(
        corps["tier"], "free",
        "une licence tenue ailleurs ne donne pas Premium ici : {corps}"
    );
}

/// Une révocation autoritaire (expiration passée) portée par le battement fait
/// retomber une clé Premium en Free.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_battement_applique_une_expiration_autoritaire() {
    let dossier = tune_core::test_scratch::scratch_dir("tune-6336-battement-expiree");
    let (app, state) = serveur(&dossier);
    state.license.set_license_key(CLE_EXPIREE).await.unwrap();
    state
        .license
        .update_from_server(
            tune_core::license::Tier::Premium,
            Some("2099-01-01T00:00:00Z".into()),
        )
        .await;
    assert_eq!(statut(&app).await["tier"], "premium", "départ : premium");

    lancer_le_battement(&state);

    attendre_le_statut(&app, "retour en Free après expiration", |s| {
        s["tier"] == "free"
    })
    .await;
    assert!(
        battement_de(CLE_EXPIREE).is_some(),
        "le verdict doit venir du faux mozaiklabs"
    );
}

// ── SSO : la liaison du serveur au compte ─────────────────────────────────

/// La connexion SSO complète (`/sso/authorize` puis `/sso/callback`), sur un
/// serveur qui a déjà son `server_id` : la liaison au compte part vers le faux,
/// jeton OAuth en tête, et le jeton de liaison rendu est rangé.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_connexion_sso_lie_le_serveur_au_compte_chez_le_faux() {
    let dossier = tune_core::test_scratch::scratch_dir("tune-6336-sso-liaison");
    let (app, state) = serveur(&dossier);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let server_id =
        tune_core::cloud::telemetry::TelemetryReporter::get_or_create_server_id(&settings);

    let req = Request::get(AUTHORIZE)
        .header("host", "127.0.0.1:8888")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
    let pending = settings
        .get("mozaik_pkce_pending")
        .unwrap()
        .expect("aucune session PKCE en attente");
    let pkce: Value = serde_json::from_str(&pending).unwrap();
    let csrf = pkce["state"].as_str().expect("state PKCE absent");
    let req = Request::get(format!(
        "/api/v1/cloud/sso/callback?code=code-6336&state={csrf}"
    ))
    .header("host", "127.0.0.1:8888")
    .body(Body::empty())
    .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::TEMPORARY_REDIRECT,
        "la connexion doit aboutir"
    );

    // La liaison part en tâche de fond : l'attendre.
    let chemin = format!("/api/v1/cloud-library/{server_id}/link");
    let debut = std::time::Instant::now();
    while settings
        .get(tune_core::cloud::library_sync::CLE_JETON_DE_LIAISON)
        .unwrap()
        .is_none()
    {
        assert!(
            debut.elapsed() < ATTENTE_MAX,
            "la liaison n'a jamais été rangée ; liaisons reçues par le faux : {:?}",
            recus_sur(&chemin)
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        settings
            .get(tune_core::cloud::library_sync::CLE_JETON_DE_LIAISON)
            .unwrap()
            .as_deref(),
        Some(JETON_LIAISON)
    );
    let liaisons = recus_sur(&chemin);
    assert_eq!(liaisons.len(), 1, "{liaisons:?}");
    assert_eq!(
        liaisons[0].1.as_deref(),
        Some(format!("Bearer {JETON_ACCES}").as_str()),
        "la liaison présente le jeton OAuth du compte"
    );
}
