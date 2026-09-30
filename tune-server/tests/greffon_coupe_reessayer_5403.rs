//! #5403 — un greffon dont le `setup()` dépasse la borne reste VISIBLE dans le
//! gestionnaire, en erreur « démarrage trop long », et `POST
//! /plugins/{name}/retry` relance son `setup()` sous la même borne.
//!
//! Avant, un greffon coupé disparaissait de `GET /api/v1/plugins` exactement
//! comme un greffon en échec : rien ne disait qu'il existait.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::use_scratch_plugin_data_dir;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};
use tune_server::state::AppState;

/// La borne du test : assez courte pour ne pas attendre 30 s, assez longue
/// pour qu'un `setup()` sain passe sans peine sur une machine chargée.
const BORNE: Duration = Duration::from_millis(300);

/// Pend à ses `bloquants` premiers `setup()`, puis charge.
struct PendPuisCharge {
    nom: &'static str,
    essais: Arc<AtomicUsize>,
    bloquants: usize,
}

#[async_trait]
impl TunePlugin for PendPuisCharge {
    fn name(&self) -> &str {
        self.nom
    }
    fn version(&self) -> &str {
        "5.4.3"
    }
    fn description(&self) -> &str {
        "greffon qui attend un appareil du réseau"
    }
    async fn setup(&mut self, _ctx: &PluginContext) -> Result<(), String> {
        if self.essais.fetch_add(1, Ordering::SeqCst) < self.bloquants {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
    async fn teardown(&mut self) -> Result<(), String> {
        Ok(())
    }
}

async fn requete(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
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

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    requete(app, Request::get(path).body(Body::empty()).unwrap()).await
}

async fn post(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    requete(app, Request::post(path).body(Body::empty()).unwrap()).await
}

/// Les fiches de `GET /api/v1/plugins` qui portent ce nom.
async fn fiches(app: &axum::Router, nom: &str) -> Vec<Value> {
    let (status, liste) = get(app, "/api/v1/plugins").await;
    assert_eq!(status, StatusCode::OK);
    liste
        .as_array()
        .expect("la liste des greffons")
        .iter()
        .filter(|f| f["name"] == nom)
        .cloned()
        .collect()
}

fn assert_en_erreur_trop_long(fiche: &Value) {
    assert_eq!(fiche["status"], "error", "{fiche}");
    assert_eq!(fiche["error_reason"], "setup_timeout", "{fiche}");
    assert_eq!(fiche["loaded"], false, "{fiche}");
    assert_eq!(fiche["installed"], true, "{fiche}");
    assert_eq!(
        fiche["setup_timeout_ms"],
        BORNE.as_millis() as u64,
        "{fiche}"
    );
    let duree = fiche["setup_duration_ms"]
        .as_u64()
        .unwrap_or_else(|| panic!("durée absente : {fiche}"));
    assert!(
        duree >= BORNE.as_millis() as u64,
        "durée {duree} < borne : {fiche}"
    );
    assert!(
        fiche["retry_url"]
            .as_str()
            .is_some_and(|u| u.ends_with("/retry")),
        "{fiche}"
    );
}

async fn demarrer(extra: Vec<Box<dyn TunePlugin>>) -> (AppState, axum::Router) {
    use_scratch_plugin_data_dir();
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    state.plugins.lock().await.set_setup_timeout(BORNE);
    let routers = tune_server::plugins::init(&state, "http://127.0.0.1:0", extra).await;
    let app = tune_server::routes::router_with_plugins(state.clone(), routers);
    (state, app)
}

#[tokio::test]
async fn un_greffon_coupe_reste_visible_en_erreur_puis_se_reessaie_5403() {
    let essais = Arc::new(AtomicUsize::new(0));
    let (_state, app) = demarrer(vec![Box::new(PendPuisCharge {
        nom: "attend-5403",
        essais: Arc::clone(&essais),
        bloquants: 1,
    })])
    .await;

    // Visible, en erreur « démarrage trop long », avec sa durée.
    let avant = fiches(&app, "attend-5403").await;
    assert_eq!(
        avant.len(),
        1,
        "le greffon coupé doit rester dans le gestionnaire (#5403) : {avant:?}"
    );
    assert_en_erreur_trop_long(&avant[0]);
    let (status, detail) = get(&app, "/api/v1/plugins/attend-5403").await;
    assert_eq!(status, StatusCode::OK);
    assert_en_erreur_trop_long(&detail);

    // Réessayer : le greffon répond cette fois.
    let (status, reponse) = post(&app, "/api/v1/plugins/attend-5403/retry").await;
    assert_eq!(status, StatusCode::OK, "{reponse}");
    assert_eq!(reponse["status"], "loaded", "{reponse}");
    assert_eq!(reponse["loaded"], true, "{reponse}");
    assert_eq!(
        reponse["restart_required"], false,
        "sans routeur, rien n'attend le redémarrage : {reponse}"
    );
    assert_eq!(
        essais.load(Ordering::SeqCst),
        2,
        "un setup() de plus, pas deux"
    );

    let apres = fiches(&app, "attend-5403").await;
    assert_eq!(apres.len(), 1, "{apres:?}");
    assert_ne!(apres[0]["status"], "error", "{}", apres[0]);
    assert_eq!(apres[0]["enabled"], true, "{}", apres[0]);
    let (_, detail) = get(&app, "/api/v1/plugins/attend-5403").await;
    assert_eq!(detail["status"], "loaded", "{detail}");

    // Plus en erreur : un second essai n'a rien à relancer.
    let (status, _) = post(&app, "/api/v1/plugins/attend-5403/retry").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn reessayer_un_greffon_qui_pend_encore_le_laisse_en_erreur_5403() {
    let (_state, app) = demarrer(vec![Box::new(PendPuisCharge {
        nom: "muet-5403",
        essais: Arc::new(AtomicUsize::new(0)),
        bloquants: usize::MAX,
    })])
    .await;

    let (status, reponse) = post(&app, "/api/v1/plugins/muet-5403/retry").await;
    assert_eq!(status, StatusCode::OK, "{reponse}");
    assert_en_erreur_trop_long(&reponse);
    assert_eq!(reponse["restart_required"], false, "{reponse}");

    let toujours = fiches(&app, "muet-5403").await;
    assert_eq!(
        toujours.len(),
        1,
        "une seule fiche, mise à jour : {toujours:?}"
    );
    assert_en_erreur_trop_long(&toujours[0]);

    // Un nom qui n'est pas en erreur : rien à relancer.
    let (status, _) = post(&app, "/api/v1/plugins/aucun-5403/retry").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Échoue à son premier `setup()`, avec un secret dans le message, puis charge.
struct EchoueUneFois {
    essais: Arc<AtomicUsize>,
}

#[async_trait]
impl TunePlugin for EchoueUneFois {
    fn name(&self) -> &str {
        "echoue-5403"
    }
    fn version(&self) -> &str {
        "5.4.3"
    }
    fn description(&self) -> &str {
        "greffon dont le premier setup() échoue"
    }
    async fn setup(&mut self, _ctx: &PluginContext) -> Result<(), String> {
        if self.essais.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err("service injoignable (password=hunter2)".into());
        }
        Ok(())
    }
    async fn teardown(&mut self) -> Result<(), String> {
        Ok(())
    }
}

/// #5403 (décision du 29/09) — un greffon dont le `setup()` ÉCHOUE au
/// démarrage est visible aussi, en `setup_failed`, avec le message du greffon
/// expurgé, et Réessayer le charge.
#[tokio::test]
async fn un_greffon_en_echec_reste_visible_avec_son_message_puis_se_reessaie_5403() {
    let essais = Arc::new(AtomicUsize::new(0));
    let (_state, app) = demarrer(vec![Box::new(EchoueUneFois {
        essais: Arc::clone(&essais),
    })])
    .await;

    let avant = fiches(&app, "echoue-5403").await;
    assert_eq!(avant.len(), 1, "l'échec doit rester visible : {avant:?}");
    let f = &avant[0];
    assert_eq!(f["status"], "error", "{f}");
    assert_eq!(f["error_reason"], "setup_failed", "{f}");
    assert_eq!(f["loaded"], false, "{f}");
    let message = f["error_message"].as_str().unwrap_or_else(|| panic!("{f}"));
    assert!(message.starts_with("service injoignable"), "{message}");
    assert!(!message.contains("hunter2"), "secret publié : {message}");

    let (status, reponse) = post(&app, "/api/v1/plugins/echoue-5403/retry").await;
    assert_eq!(status, StatusCode::OK, "{reponse}");
    assert_eq!(reponse["status"], "loaded", "{reponse}");
    assert_eq!(essais.load(Ordering::SeqCst), 2);
    let apres = fiches(&app, "echoue-5403").await;
    assert_eq!(apres.len(), 1, "{apres:?}");
    assert_ne!(apres[0]["status"], "error", "{}", apres[0]);
}
