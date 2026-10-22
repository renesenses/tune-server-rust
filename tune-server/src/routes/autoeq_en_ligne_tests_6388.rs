//! #6388 (fil 2282) — la base AutoEq en ligne, par le ROUTEUR MONTÉ :
//! `GET /api/v1/eq/autoeq/search` et `POST /api/v1/eq/autoeq/import`, sur un
//! catalogue à faux réseau (aucun accès à GitHub pendant les essais).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::audio::autoeq_base::{CatalogueAutoEq, TelechargeurAutoEq};

const INDEX: &str = "# Index\n\
- [Sennheiser HD 650](./oratory1990/over-ear/Sennheiser%20HD%20650) by oratory1990\n\
- [Sennheiser HD 650](./crinacle/GRAS%2043AG-7%20over-ear/Sennheiser%20HD%20650) by crinacle on GRAS 43AG-7\n";

#[derive(Default)]
struct FauxReseau {
    coupe: AtomicBool,
    appels: AtomicUsize,
}

#[async_trait]
impl TelechargeurAutoEq for FauxReseau {
    async fn telecharger(&self, url: &str, _taille_max: usize) -> Result<String, String> {
        self.appels.fetch_add(1, Ordering::SeqCst);
        if self.coupe.load(Ordering::SeqCst) {
            return Err("réseau injoignable".into());
        }
        if url.ends_with("/INDEX.md") {
            Ok(INDEX.into())
        } else {
            Ok("Preamp: -6.1 dB\nFilter 1: ON LSC Fc 105 Hz Gain 6.4 dB Q 0.70\n".into())
        }
    }
}

fn app(reseau: &Arc<FauxReseau>) -> axum::Router {
    let mut state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state.autoeq = Arc::new(CatalogueAutoEq::new(
        "https://exemple.test/results",
        reseau.clone(),
        None,
    ));
    crate::routes::router(state)
}

async fn appel(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let statut = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn chercher(q: &str) -> Request<Body> {
    Request::get(format!(
        "/api/v1/eq/autoeq/search?q={}",
        urlencoding::encode(q)
    ))
    .body(Body::empty())
    .unwrap()
}

#[tokio::test]
async fn la_recherche_rend_chaque_source_du_casque_avec_son_chemin() {
    let reseau = Arc::new(FauxReseau::default());
    let (code, v) = appel(&app(&reseau), chercher("hd650")).await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["total"], 2);
    assert_eq!(
        v["results"][0],
        json!({
            "name": "Sennheiser HD 650",
            "source": "crinacle",
            "rig": "GRAS 43AG-7",
            "form": "GRAS 43AG-7 over-ear",
            "path": "crinacle/GRAS 43AG-7 over-ear/Sennheiser HD 650",
        })
    );
    assert_eq!(v["results"][1]["source"], "oratory1990");
}

#[tokio::test]
async fn une_requete_vide_ne_touche_pas_au_reseau() {
    let reseau = Arc::new(FauxReseau::default());
    let (code, v) = appel(&app(&reseau), chercher("  ")).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(v["total"], 0);
    assert_eq!(reseau.appels.load(Ordering::SeqCst), 0);
}

/// Sans réseau ni copie : 503 qui le dit, jamais une liste vide qui ferait
/// croire que le casque n'existe pas.
#[tokio::test]
async fn sans_reseau_la_recherche_dit_que_la_base_est_injoignable() {
    let reseau = Arc::new(FauxReseau::default());
    reseau.coupe.store(true, Ordering::SeqCst);
    let (code, v) = appel(&app(&reseau), chercher("hd 650")).await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    assert_eq!(v["error"], "autoeq_unreachable");
}

/// L'import passe les MÊMES portes que l'import d'un fichier (Premium,
/// greffon égaliseur) AVANT de télécharger quoi que ce soit.
#[tokio::test]
async fn l_import_passe_les_portes_avant_tout_telechargement() {
    let reseau = Arc::new(FauxReseau::default());
    let req = Request::post("/api/v1/eq/autoeq/import")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({"path": "oratory1990/over-ear/Sennheiser HD 650"}).to_string(),
        ))
        .unwrap();
    let (code, v) = appel(&app(&reseau), req).await;
    assert!(
        code == StatusCode::PAYMENT_REQUIRED || code == StatusCode::CONFLICT,
        "{code} {v}"
    );
    assert_eq!(reseau.appels.load(Ordering::SeqCst), 0);
}
