//! #7619 (L3, L4) — BANC des routes de la Bibliothèque sur une COPIE de la
//! base d'un serveur (`TUNE_BANC_DB`). `#[ignore]`, à la main, en `--release` :
//!
//! ```text
//! TUNE_BANC_DB=/tmp/copie.db cargo test --release -p tune-server --lib \
//!   banc_l3l4_7619 -- --ignored --nocapture --test-threads=1
//! ```
//!
//! La copie est ouverte comme le serveur l'ouvre (migrations et passe de
//! démarrage comprises) : ne jamais le pointer sur une base de production.
//!
//! Il MESURE, sans rien garder : le premier appel (« froid »), puis la médiane
//! de 5 appels (« chaud »). Pour les facettes, « sans cache » rejoue le calcul
//! complet ([`super::facets::compter_les_facettes`]), « route » passe par le
//! cache des compteurs (L4).

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use crate::state::AppState;

async fn une_fois(app: &axum::Router, uri: &str) -> Duration {
    let debut = Instant::now();
    let resp = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "{uri}");
    let _ = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    debut.elapsed()
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

const OXYGEN11: &str =
    "genre,label,year,artist,format,sample_rate,bit_depth,composer,country,mood,source";
const BIBLIO6: &str = "genre,year,format,label,composer,artist";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "banc manuel sur une copie de base, --release"]
async fn banc_l3l4_7619() {
    let Ok(chemin) = std::env::var("TUNE_BANC_DB") else {
        eprintln!("TUNE_BANC_DB absent : banc sauté");
        return;
    };
    let state = AppState::new(&chemin, 0, Default::default()).unwrap();
    let app = crate::routes::router(state.clone());
    let p = "/api/v1";
    let mut uris: Vec<(String, String)> = vec![
        (
            "facettes_oxygen11".into(),
            format!("{p}/library/facets?fields={OXYGEN11}"),
        ),
        (
            "facettes_biblio6".into(),
            format!("{p}/library/facets?fields={BIBLIO6}"),
        ),
        (
            "facettes_biblio6_l0".into(),
            format!("{p}/library/facets?fields={BIBLIO6}&limit=0"),
        ),
        (
            "facettes_jazz".into(),
            format!("{p}/library/facets?fields={OXYGEN11}&genre=Jazz"),
        ),
    ];
    for q in ["beat", "bach", "miles%20davis", "jazz"] {
        uris.push((
            format!("search_{q}"),
            format!("{p}/search?q={q}&sources=local"),
        ));
        uris.push((
            format!("lib_search_{q}"),
            format!("{p}/library/search?q={q}"),
        ));
        uris.push((
            format!("oxygen_q_{q}"),
            format!("{p}/library/tracks?q={q}&limit=200"),
        ));
    }
    for (nom, uri) in &uris {
        let froid = une_fois(&app, uri).await;
        let mut d: Vec<Duration> = Vec::new();
        for _ in 0..5 {
            d.push(une_fois(&app, uri).await);
        }
        d.sort();
        eprintln!(
            "BANC route {nom:<22} froid={:>7.1} ms  chaud={:>7.1} ms",
            ms(froid),
            ms(d[2])
        );
    }
    // Les facettes SANS le cache : le calcul complet, à chaque fois.
    use super::facets::{FacetQuery, compter_les_facettes};
    for (nom, champs) in [("oxygen11", OXYGEN11), ("biblio6", BIBLIO6)] {
        let requete = || {
            let q: FacetQuery =
                serde_json::from_value(serde_json::json!({ "fields": champs })).unwrap();
            q.hydrate(None)
                .unwrap_or_else(|_| panic!("requête de facettes"))
        };
        let mut d: Vec<Duration> = Vec::new();
        for _ in 0..3 {
            let t = Instant::now();
            let _ = compter_les_facettes(&state, requete());
            d.push(t.elapsed());
        }
        d.sort();
        eprintln!(
            "BANC facettes_sans_cache {nom:<10} mediane={:>7.1} ms",
            ms(d[1])
        );
    }
}
