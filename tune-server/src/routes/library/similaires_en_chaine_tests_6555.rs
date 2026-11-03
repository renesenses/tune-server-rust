//! #6555 (Bilou, fil 2341) — des similaires EN CHAÎNE depuis la fiche d'un
//! artiste de la bibliothèque.
//!
//! Le cloud est simulé par un petit serveur local, branché par le réglage
//! `artist_enrichment_api` — le même que lisent `auto_dj` et
//! `/metadata/artists/{id}/similar` (#1730). Ses réponses ont la forme réelle
//! de `https://mozaiklabs.fr/api/v1/artists/…`, relevée le 19/10/2026.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query};
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::state::AppState;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::models::Artist;
use tune_core::db::settings_repo::SettingsRepo;

const MBID_RADIOHEAD: &str = "a74b1b7f-71a5-4011-9441-d0b5e4122711";
const MBID_PORTISHEAD: &str = "8f6bd1e4-fbe1-4f50-aa9b-94c450ec0f11";

fn fiche(mbid: &str, nom: &str, voisins: &[&str]) -> Value {
    json!({"data": {
        "musicbrainz_id": mbid,
        "name": nom,
        "similar_artists": voisins
            .iter()
            .map(|v| json!({"name": v, "reason": format!("proche de {nom}")}))
            .collect::<Vec<_>>(),
        "members": [],
    }})
}

/// Le cloud simulé, et le nombre de recherches par NOM qu'il a reçues.
async fn cloud() -> (String, Arc<AtomicUsize>) {
    let recherches = Arc::new(AtomicUsize::new(0));
    let compte = recherches.clone();
    let app = Router::new()
        .route(
            "/api/v1/artists/search",
            get(
                move |Query(q): Query<std::collections::HashMap<String, String>>| {
                    let compte = compte.clone();
                    async move {
                        compte.fetch_add(1, Ordering::SeqCst);
                        let nom = q.get("q").cloned().unwrap_or_default();
                        let data = if nom.eq_ignore_ascii_case("Portishead") {
                            json!([{"name": "Portishead", "musicbrainz_id": MBID_PORTISHEAD}])
                        } else {
                            json!([])
                        };
                        axum::Json(json!({"data": data, "count": 1}))
                    }
                },
            ),
        )
        .route(
            "/api/v1/artists/{mbid}",
            get(|Path(mbid): Path<String>| async move {
                match mbid.as_str() {
                    MBID_RADIOHEAD => {
                        axum::Json(fiche(MBID_RADIOHEAD, "Radiohead", &["Portishead", "Muse"]))
                            .into_response()
                    }
                    MBID_PORTISHEAD => axum::Json(fiche(
                        MBID_PORTISHEAD,
                        "Portishead",
                        &["Massive Attack", "Tricky"],
                    ))
                    .into_response(),
                    _ => (StatusCode::NOT_FOUND, "{}").into_response(),
                }
            }),
        )
        .route(
            "/api/v1/artists/{mbid}/similar",
            get(|Path(mbid): Path<String>| async move {
                match mbid.as_str() {
                    MBID_PORTISHEAD => axum::Json(fiche(
                        MBID_PORTISHEAD,
                        "Portishead",
                        &["Massive Attack", "Tricky"],
                    ))
                    .into_response(),
                    _ => (StatusCode::NOT_FOUND, "{}").into_response(),
                }
            }),
        );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    (format!("http://{adresse}/api/v1/artists"), recherches)
}

struct Banc {
    app: Router,
    radiohead: i64,
    portishead: i64,
    inconnu: i64,
    recherches: Arc<AtomicUsize>,
}

async fn banc() -> Banc {
    let (base, recherches) = cloud().await;
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    SettingsRepo::with_backend(state.backend.clone())
        .set("artist_enrichment_api", &base)
        .unwrap();
    let repo = ArtistRepo::with_backend(state.backend.clone());
    let mut rh = Artist::new("Radiohead".into());
    rh.musicbrainz_id = Some(MBID_RADIOHEAD.into());
    let radiohead = repo.create(&rh).unwrap();
    // Le voisin, en bibliothèque SANS MBID — neuf artistes locaux sur dix.
    let portishead = repo.create(&Artist::new("Portishead".into())).unwrap();
    let inconnu = repo
        .create(&Artist::new("Groupe de garage".into()))
        .unwrap();
    Banc {
        app: crate::routes::router(state),
        radiohead,
        portishead,
        inconnu,
        recherches,
    }
}

async fn lire(app: &Router, chemin: &str) -> (StatusCode, Value) {
    let reponse = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// Les noms des similaires, sous la forme que la fiche web lit
/// (`normaliserMetadonnees` : `data` ou l'objet à plat).
fn voisins(v: &Value) -> Vec<String> {
    let d = v.get("data").unwrap_or(v);
    d.get("similar_artists")
        .and_then(|s| s.as_array())
        .map(|s| {
            s.iter()
                .filter_map(|x| x["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// LE TÉMOIN — la fiche de Radiohead montre ses similaires ; celle de
/// Portishead, ouverte depuis eux, montre LES SIENS.
#[tokio::test]
async fn la_fiche_d_un_voisin_montre_ses_propres_similaires_6555() {
    let b = banc().await;

    let (statut, rh) = lire(
        &b.app,
        &format!("/api/v1/library/artists/{}/metadata", b.radiohead),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        voisins(&rh),
        vec!["Portishead", "Muse"],
        "la fiche de départ doit montrer ses similaires : {rh}"
    );

    let (statut, ph) = lire(
        &b.app,
        &format!("/api/v1/library/artists/{}/metadata", b.portishead),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        voisins(&ph),
        vec!["Massive Attack", "Tricky"],
        "la fiche du VOISIN (sans MBID, résolu par son nom) doit montrer SES similaires : {ph}"
    );
}

/// La route sœur `/similar`, qui interrogeait la même adresse morte.
#[tokio::test]
async fn la_route_similar_rend_les_voisins_6555() {
    let b = banc().await;
    let (statut, v) = lire(
        &b.app,
        &format!("/api/v1/library/artists/{}/similar", b.portishead),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    let noms: Vec<&str> = v["artists"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|x| x["name"].as_str())
        .collect();
    assert_eq!(noms, vec!["Massive Attack", "Tricky"], "{v}");
}

/// Un artiste inconnu du cloud garde sa fiche locale, sans erreur ; et sa
/// résolution manquée est gardée : une seconde ouverture ne relance pas la
/// recherche.
#[tokio::test]
async fn un_artiste_inconnu_du_cloud_garde_sa_fiche_et_ne_relance_pas_la_recherche_6555() {
    let b = banc().await;
    let chemin = format!("/api/v1/library/artists/{}/metadata", b.inconnu);
    let (statut, v) = lire(&b.app, &chemin).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(v["name"], "Groupe de garage", "{v}");
    assert!(voisins(&v).is_empty());
    let avant = b.recherches.load(Ordering::SeqCst);
    assert_eq!(avant, 1, "une recherche par le nom");
    let _ = lire(&b.app, &chemin).await;
    assert_eq!(
        b.recherches.load(Ordering::SeqCst),
        avant,
        "la résolution manquée est gardée"
    );
}

/// Un artiste absent : 404, comme avant.
#[tokio::test]
async fn artiste_absent_404_6555() {
    let b = banc().await;
    let (statut, _) = lire(&b.app, "/api/v1/library/artists/999999/metadata").await;
    assert_eq!(statut, StatusCode::NOT_FOUND);
}
