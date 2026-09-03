//! #1279 — coller une clé de licence valide doit débloquer Premium **du premier
//! coup**.
//!
//! Alex Campbell (licence Lifetime, WhatsApp 2026-08-06) : « When I apply the
//! license key it tells me its invalid so I refresh the screen then validate the
//! key but then it says Premium ». Le panneau « Tune Premium License » du client
//! web n'appelle qu'une route quand on colle une clé : `POST
//! /cloud/license/activate`. Celle-ci se contentait de `set_license_key`, qui
//! depuis c15dcc61 range la clé **en attente** (palier Free) et n'appelle
//! personne. La réponse annonçait donc Free pour une clé parfaitement valide, et
//! toutes les fonctions restaient cadenassées : il fallait actionner « Valider »
//! pour déclencher l'aller-retour manquant. La route jumelle `POST
//! /system/license` avait, elle, reçu l'appel à `validate_stored_license` dans ce
//! même commit.
//!
//! Ces tests partent d'un état **froid** — base neuve, aucune clé, palier Free —
//! et n'exercent que le PREMIER appel. Le serveur de licences est bouchonné en
//! local (`mozaik_base_url`), jamais mozaiklabs.fr.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const CLE: &str = "TUNE-LIFETIME-1279-ALEX";
const ACTIVATE: &str = "/api/v1/cloud/license/activate";
const STATUS: &str = "/api/v1/cloud/license/status";

/// Base de travail jetable, à un chemin unique par test — jamais un nom fixe
/// dans `temp_dir()`, deux tests en parallèle se marcheraient dessus.
///
/// Le garde est rendu avec le chemin : tant qu'il vit, la base vit ; quand il
/// sort de portée — panique comprise — le dossier disparaît. La composition à
/// la main laissait un `tune-i1279-*` par exécution (#3030).
fn scratch_db() -> (tune_core::test_scratch::ScratchDir, std::path::PathBuf) {
    let dir = tune_core::test_scratch::scratch_dir("tune-i1279");
    let db = dir.join("library.db");
    (dir, db)
}

/// Verdict que rend le faux serveur de licences.
#[derive(Clone)]
enum Verdict {
    /// Clé reconnue, abonnement premium.
    Premium,
    /// Clé refusée — aucun droit ne doit être accordé.
    Refusee,
}

/// Faux mozaiklabs.fr, sur une socket locale éphémère.
///
/// Un vrai serveur HTTP tenu par `axum::serve`, pas une socket qu'on ferme à la
/// main : un bouchon qui coupe la connexion rend un RST et fabrique un test
/// instable. Rend l'URL de base et le compteur d'appels reçus.
async fn faux_serveur_de_licences(verdict: Verdict) -> (String, Arc<AtomicUsize>) {
    let appels = Arc::new(AtomicUsize::new(0));
    let compteur = appels.clone();
    let app = axum::Router::new().route(
        "/api/v1/license/validate",
        axum::routing::post(move || {
            let compteur = compteur.clone();
            let verdict = verdict.clone();
            async move {
                compteur.fetch_add(1, Ordering::SeqCst);
                axum::Json(match verdict {
                    Verdict::Premium => json!({
                        "license_valid": true,
                        "license_tier": "premium",
                        "license_expires_at": "2099-01-01T00:00:00Z",
                    }),
                    Verdict::Refusee => json!({ "license_valid": false }),
                })
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    (format!("http://{addr}"), appels)
}

/// Un serveur Tune **froid** : base neuve, aucune clé, palier Free, et le
/// serveur de licences redirigé vers le bouchon.
fn serveur_froid(base_url: &str) -> (axum::Router, tune_core::test_scratch::ScratchDir) {
    let (base, db_path) = scratch_db();
    let state = AppState::new(db_path.to_str().unwrap(), 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("mozaik_base_url", base_url).unwrap();
    assert_eq!(
        settings.get("license_key").ok().flatten(),
        None,
        "l'état de départ doit être froid : aucune clé enregistrée"
    );
    (tune_server::routes::router(state), base)
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

async fn activer(app: &axum::Router, cle: &str) -> (StatusCode, Value) {
    let req = Request::post(ACTIVATE)
        .header("content-type", "application/json")
        .body(Body::from(json!({ "license_key": cle }).to_string()))
        .unwrap();
    appeler(app, req).await
}

/// Le cœur de #1279 : premier appel, état froid, clé valide ⇒ Premium tout de
/// suite. Aucun second essai, aucun rafraîchissement.
#[tokio::test]
async fn le_premier_essai_suffit_a_debloquer_premium() {
    let (base, appels) = faux_serveur_de_licences(Verdict::Premium).await;
    let (app, _base) = serveur_froid(&base);

    let (status, body) = activer(&app, CLE).await;

    assert_eq!(status, StatusCode::OK, "réponse : {body}");
    assert_eq!(
        body["tier"], "premium",
        "le premier essai rend encore Free — c'est ce que le panneau affiche « invalide » : {body}"
    );
    assert_eq!(body["status"], "activated", "réponse : {body}");
    assert_eq!(
        appels.load(Ordering::SeqCst),
        1,
        "l'activation doit valider en ligne dans son propre aller-retour"
    );
}

/// Deuxième symptôme du même ticket : « it says Premium but all the locks are
/// still showing ». Les cadenas du panneau lisent `features[*].enabled` de
/// `/cloud/license/status` ; après ce seul et unique appel d'activation, plus un
/// seul ne doit être fermé.
#[tokio::test]
async fn apres_le_premier_essai_plus_aucune_fonction_n_est_cadenassee() {
    let (base, _) = faux_serveur_de_licences(Verdict::Premium).await;
    let (app, _base) = serveur_froid(&base);

    let (status, _) = activer(&app, CLE).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = appeler(&app, Request::get(STATUS).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tier"], "premium", "statut : {body}");

    let features = body["features"].as_object().expect("features absent");
    assert!(!features.is_empty(), "aucune fonction décrite : {body}");
    let cadenassees: Vec<&String> = features
        .iter()
        .filter(|(_, f)| f["enabled"] != json!(true))
        .map(|(nom, _)| nom)
        .collect();
    assert!(
        cadenassees.is_empty(),
        "cadenas encore fermés sans second rafraîchissement : {cadenassees:?}"
    );
}

/// La contrepartie, qui doit rester vraie : une clé que le serveur refuse ne
/// débloque RIEN. C15dcc61 a supprimé ce passe-droit, l'activation immédiate ne
/// le réintroduit pas.
#[tokio::test]
async fn une_cle_refusee_ne_debloque_toujours_rien() {
    let (base, appels) = faux_serveur_de_licences(Verdict::Refusee).await;
    let (app, _base) = serveur_froid(&base);

    let (status, body) = activer(&app, "PAS-UNE-VRAIE-CLE").await;

    assert_eq!(status, StatusCode::OK, "réponse : {body}");
    assert_eq!(body["tier"], "free", "clé refusée promue : {body}");
    assert_eq!(body["status"], "pending", "réponse : {body}");
    assert_eq!(appels.load(Ordering::SeqCst), 1);

    let (_, statut) = appeler(&app, Request::get(STATUS).body(Body::empty()).unwrap()).await;
    assert_eq!(statut["tier"], "free", "statut : {statut}");
}
