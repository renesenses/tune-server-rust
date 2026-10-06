//! #4805 — les crédits MusicBrainz d'un album sont lus JUSTE APRÈS son
//! identification, dans la release que l'identification a gardée en base.
//!
//! **Hermétique : aucun appel réel à MusicBrainz.** Une doublure locale
//! (127.0.0.1, port éphémère) sert la recherche et la release, compte chaque
//! requête, et remplace la base MusicBrainz par
//! `remplacer_la_base_musicbrainz`. Le limiteur partagé reste en place.
//!
//! Les témoins :
//! 1. le pilote de lot pose le pressage, puis les crédits arrivent SANS
//!    requête de plus (la release est relue en base) ;
//! 2. le bouton « Ré-identifier » qui CHANGE le pressage fait de même ;
//! 3. un pressage retrouvé à l'identique (`unchanged`) ne relance rien ;
//! 4. le réglage `credits_auto_enabled = "false"` coupe le déclenchement.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Path, Query};
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use tower::ServiceExt;

/// Les requêtes reçues par la doublure : `recherche`, `release/<id>`.
type Compteur = Arc<Mutex<HashMap<String, usize>>>;

/// La base MusicBrainz remplacée est globale : les témoins passent l'un
/// après l'autre.
static SERIE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn nb(c: &Compteur, cle: &str) -> usize {
    c.lock().unwrap().get(cle).copied().unwrap_or(0)
}

/// La release, avec ce que lit l'identification (pistes, artistes, labels)
/// et ce que lit la passe des crédits (relations d'enregistrement).
fn reponse_release(id: &str) -> Option<Value> {
    if id != "rel-kob" {
        return None;
    }
    Some(json!({
        "id": "rel-kob",
        "title": "Kind of Blue",
        "status": "Official",
        "artist-credit": [{ "name": "Miles Davis", "joinphrase": "",
                            "artist": { "id": "a-miles", "name": "Miles Davis" } }],
        "release-group": { "id": "rg-kob" },
        "label-info": [],
        "media": [{ "position": 1, "track-count": 1, "tracks": [{
            "position": 1,
            "number": "1",
            "title": "So What",
            "length": 545000,
            "recording": {
                "id": "rec-so-what",
                "title": "So What",
                "length": 545000,
                "relations": [{
                    "type": "instrument",
                    "attributes": ["piano"],
                    "artist": { "id": "a-bill-evans", "name": "Bill Evans" }
                }]
            }
        }]}]
    }))
}

fn reponse_recherche() -> Value {
    json!({ "releases": [{
        "id": "rel-kob",
        "score": 100,
        "title": "Kind of Blue",
        "status": "Official",
        "track-count": 1,
        "artist-credit": [{ "name": "Miles Davis", "joinphrase": "" }],
        "release-group": { "id": "rg-kob" },
        "media": [{ "track-count": 1 }],
    }]})
}

async fn doublure() -> Compteur {
    let compteur: Compteur = Arc::default();
    let (c1, c2) = (compteur.clone(), compteur.clone());
    let app = axum::Router::new()
        .route(
            "/release",
            axum::routing::get(move |Query(_q): Query<HashMap<String, String>>| {
                let c = c1.clone();
                async move {
                    *c.lock().unwrap().entry("recherche".into()).or_insert(0) += 1;
                    axum::Json(reponse_recherche())
                }
            }),
        )
        .route(
            "/release/{id}",
            axum::routing::get(move |Path(id): Path<String>| {
                let c = c2.clone();
                async move {
                    *c.lock()
                        .unwrap()
                        .entry(format!("release/{id}"))
                        .or_insert(0) += 1;
                    match reponse_release(&id) {
                        Some(v) => axum::Json(v).into_response(),
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                }
            }),
        )
        .route(
            "/recording",
            axum::routing::get(|| async { axum::Json(json!({ "recordings": [] })) }),
        );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    tune_core::metadata::musicbrainz_release::remplacer_la_base_musicbrainz(Some(format!(
        "http://{adresse}"
    )));
    compteur
}

/// Un album « Kind of Blue » d'une piste, pressage donné (ou aucun).
async fn etat(pressage: Option<&str>) -> tune_server::state::AppState {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    state.license.set_account_premium(true, None).await;
    for sql in [
        "INSERT INTO artists (id, name) VALUES (1, 'Miles Davis')",
        "INSERT INTO albums (id, title, source, artist_id) VALUES (1, 'Kind of Blue', 'local', 1)",
        "INSERT INTO tracks (id, title, album_id, artist_id, source, track_number, disc_number, \
         duration_ms) VALUES (10, 'So What', 1, 1, 'local', 1, 1, 545000)",
    ] {
        state.backend.execute(sql, &[]).unwrap();
    }
    if let Some(p) = pressage {
        state
            .backend
            .execute(
                &format!(
                    "UPDATE albums SET musicbrainz_release_id = '{p}', \
                     musicbrainz_release_group_id = 'rg-{p}' WHERE id = 1"
                ),
                &[],
            )
            .unwrap();
    }
    state
}

async fn appeler(app: &axum::Router, methode: &str, path: &str) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method(methode)
        .uri(path)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(requete).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn attendre_la_fin_du_lot(app: &axum::Router) -> Value {
    for _ in 0..240 {
        let (_, etat) = appeler(app, "GET", "/api/v1/library/identify-all/status").await;
        if etat["status"].as_str() != Some("running") {
            return etat;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("la passe n'a pas fini en 60 s");
}

/// Les crédits de la piste : `(artiste, instrument)`.
fn credits(state: &tune_server::state::AppState) -> Vec<(String, Option<String>)> {
    state
        .backend
        .query_many(
            "SELECT artist_name, instrument FROM track_credits WHERE track_id = 10 ORDER BY position",
            &[],
        )
        .unwrap()
        .into_iter()
        .map(|r| (r[0].as_string().unwrap_or_default(), r[1].as_string()))
        .collect()
}

fn curseur(state: &tune_server::state::AppState) -> Option<String> {
    state
        .backend
        .query_one("SELECT credits_mb_at FROM albums WHERE id = 1", &[])
        .unwrap()
        .and_then(|r| r[0].as_string())
}

/// Attend des crédits sur la piste, au plus 15 s.
async fn attendre_des_credits(state: &tune_server::state::AppState) -> bool {
    for _ in 0..60 {
        if !credits(state).is_empty() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

fn bill_evans_au_piano(state: &tune_server::state::AppState) -> bool {
    credits(state).contains(&("Bill Evans".to_string(), Some("piano".to_string())))
}

/// Témoin 1 — le pilote de lot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_lot_lit_les_credits_du_pressage_qu_il_pose() {
    let _serie = SERIE.lock().await;
    let compteur = doublure().await;
    let state = etat(None).await;
    let app = tune_server::routes::router(state.clone());

    let (status, corps) = appeler(&app, "POST", "/api/v1/library/identify-all").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{corps}");
    let fin = attendre_la_fin_du_lot(&app).await;
    assert_eq!(fin["identifies"], 1, "{fin}");

    assert!(
        attendre_des_credits(&state).await,
        "aucun crédit après l'identification"
    );
    assert!(bill_evans_au_piano(&state), "{:?}", credits(&state));
    assert!(curseur(&state).is_some(), "credits_mb_at doit être posé");
    // La release est relue en base : UNE seule lecture sur le réseau, celle
    // de l'identification.
    assert_eq!(nb(&compteur, "release/rel-kob"), 1);
}

/// Témoin 2 — le bouton « Ré-identifier » qui change de pressage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reidentifier_vers_un_autre_pressage_lit_ses_credits() {
    let _serie = SERIE.lock().await;
    let compteur = doublure().await;
    let state = etat(Some("rel-ancien")).await;
    let app = tune_server::routes::router(state.clone());

    let (status, corps) = appeler(&app, "POST", "/api/v1/library/albums/1/reidentify").await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(corps["verdict"], "reidentified", "{corps}");

    assert!(
        attendre_des_credits(&state).await,
        "aucun crédit après la ré-identification"
    );
    assert!(bill_evans_au_piano(&state), "{:?}", credits(&state));
    assert_eq!(nb(&compteur, "release/rel-kob"), 1);
}

/// Témoin 3 — le même pressage retrouvé ne relance rien.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn un_pressage_inchange_ne_relance_rien() {
    let _serie = SERIE.lock().await;
    let _compteur = doublure().await;
    let state = etat(Some("rel-kob")).await;
    let app = tune_server::routes::router(state.clone());

    let (status, corps) = appeler(&app, "POST", "/api/v1/library/albums/1/reidentify").await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(corps["verdict"], "unchanged", "{corps}");

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(credits(&state).is_empty(), "{:?}", credits(&state));
    assert_eq!(curseur(&state), None);
}

/// Témoin 4 — le réglage de la passe automatique des crédits coupe aussi
/// le déclenchement après l'identification.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn le_reglage_coupe_le_declenchement() {
    let _serie = SERIE.lock().await;
    let _compteur = doublure().await;
    let state = etat(None).await;
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set("credits_auto_enabled", "false")
        .unwrap();
    let app = tune_server::routes::router(state.clone());

    let (status, corps) = appeler(&app, "POST", "/api/v1/library/identify-all").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{corps}");
    let fin = attendre_la_fin_du_lot(&app).await;
    assert_eq!(fin["identifies"], 1, "{fin}");

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(credits(&state).is_empty(), "{:?}", credits(&state));
    assert_eq!(curseur(&state), None);
}
