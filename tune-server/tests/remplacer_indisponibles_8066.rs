//! #8066 — remplacer un titre de service indisponible dans une playlist
//! LOCALE, à sa place, sur de faux Qobuz et TIDAL. Le même scénario tourne
//! sur SQLite (suite ordinaire) et sur PostgreSQL (`TUNE_TEST_PG_URL`,
//! `test-postgres.yml`).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::streaming::remplacement_indisponibles::factice::{ServiceFactice, piste};
use tune_core::streaming::traits::StreamTrack;
use tune_server::state::AppState;

async fn appel(
    state: &AppState,
    methode: &str,
    uri: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let app = tune_server::routes::router(state.clone());
    let mut req = Request::builder().method(methode).uri(uri);
    let body = match corps {
        Some(c) => {
            req = req.header("content-type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let rep = app.oneshot(req.body(body).unwrap()).await.unwrap();
    let statut = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn ligne(service: &str, p: &StreamTrack) -> Value {
    json!({
        "source": service,
        "source_id": p.id,
        "title": p.title,
        "artist_name": p.artist,
        "album_title": p.album,
        "duration_ms": p.duration_ms,
    })
}

/// Les titres du service tels que la playlist les voyait : B (rang 1) et D
/// (rang 3) ont disparu ; la fiche du service les dit indisponibles.
fn anciens() -> Vec<StreamTrack> {
    vec![
        piste(
            "1",
            "Angel",
            "Massive Attack",
            "Mezzanine",
            379_000,
            Some(true),
        ),
        piste(
            "2",
            "Teardrop",
            "Massive Attack",
            "Mezzanine",
            330_000,
            Some(false),
        ),
        piste(
            "3",
            "Inertia Creeps",
            "Massive Attack",
            "Mezzanine",
            356_000,
            Some(true),
        ),
        piste(
            "4",
            "Dissolved Girl",
            "Massive Attack",
            "Mezzanine",
            366_000,
            Some(false),
        ),
        piste(
            "5",
            "Exchange",
            "Massive Attack",
            "Mezzanine",
            251_000,
            Some(false),
        ),
    ]
}

fn catalogue(jumeau_de_teardrop: bool) -> Vec<StreamTrack> {
    let mut c = anciens();
    c.push(piste(
        "20",
        "Teardrop",
        "Massive Attack",
        "Mezzanine",
        331_000,
        Some(true),
    ));
    if jumeau_de_teardrop {
        c.push(piste(
            "21",
            "Teardrop",
            "Massive Attack",
            "Mezzanine",
            330_000,
            Some(true),
        ));
    }
    c.push(piste(
        "40",
        "Dissolved Girl",
        "Massive Attack",
        "Mezzanine",
        366_000,
        Some(true),
    ));
    c.push(piste(
        "41",
        "Dissolved Girl",
        "Massive Attack",
        "Mezzanine",
        367_000,
        Some(true),
    ));
    c
}

/// Une playlist locale de cinq titres du service `service`. Rend son id et
/// les `entry_id` dans l'ordre.
async fn semer(state: &AppState, service: &str, nom: &str) -> (i64, Vec<i64>) {
    let (s, v) = appel(
        state,
        "POST",
        "/api/v1/playlists",
        Some(json!({"name": nom})),
    )
    .await;
    assert!(s.is_success(), "création : {s} {v}");
    let id = v["id"].as_i64().expect("id de playlist");
    let lignes: Vec<Value> = anciens().iter().map(|p| ligne(service, p)).collect();
    let (s, v) = appel(
        state,
        "POST",
        &format!("/api/v1/playlists/{id}/tracks"),
        Some(json!({"streaming_tracks": lignes})),
    )
    .await;
    assert!(s.is_success(), "ajout : {s} {v}");
    (
        id,
        entrees(state, id).await.iter().map(|(e, _)| *e).collect(),
    )
}

/// `(entry_id, source_id)` dans l'ordre de la playlist.
async fn entrees(state: &AppState, id: i64) -> Vec<(i64, String)> {
    let (s, v) = appel(
        state,
        "GET",
        &format!("/api/v1/playlists/{id}/tracks"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let lignes = v
        .as_array()
        .or_else(|| v["tracks"].as_array())
        .cloned()
        .unwrap_or_default();
    lignes
        .iter()
        .map(|l| {
            (
                l["entry_id"].as_i64().unwrap(),
                l["source_id"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

async fn enregistrer(state: &AppState, svc: ServiceFactice) {
    state.services.lock().await.register(Box::new(svc));
}

/// Rend les écarts constatés (vide = tout va bien).
async fn scenario(state: &AppState) -> Vec<String> {
    let mut ecarts = Vec::new();
    let mut verifier = |ok: bool, quoi: String| {
        if !ok {
            ecarts.push(quoi);
        }
    };

    for service in ["qobuz", "tidal"] {
        // 1. Candidat unique sûr → remplacement direct, à sa place.
        enregistrer(
            state,
            ServiceFactice::new(service, catalogue(false), vec![]),
        )
        .await;
        let (pl, e) = semer(state, service, &format!("direct {service}")).await;
        let (s, v) = appel(
            state,
            "GET",
            &format!("/api/v1/playlists/{pl}/tracks/{}/replacements", e[1]),
            None,
        )
        .await;
        verifier(
            s == StatusCode::OK && v["decision"] == "direct" && v["unavailable"] == true,
            format!("{service} candidats : {s} {v}"),
        );
        verifier(
            v["candidates"][0]["source_id"] == "20" && v["candidates"][0]["sure"] == true,
            format!("{service} candidat sûr : {v}"),
        );
        let (s, v) = appel(
            state,
            "POST",
            &format!("/api/v1/playlists/{pl}/tracks/{}/replace", e[1]),
            None,
        )
        .await;
        verifier(
            s == StatusCode::OK && v["position"] == 1,
            format!("{service} direct : {s} {v}"),
        );
        let apres = entrees(state, pl).await;
        verifier(
            apres.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>() == ["1", "20", "3", "4", "5"],
            format!("{service} position conservée : {apres:?}"),
        );
        verifier(
            apres[1].0 == e[1],
            format!("{service} même ligne (entry_id) : {apres:?} / {e:?}"),
        );

        // 2. Plusieurs candidats → choix, puis le choix est écrit.
        enregistrer(state, ServiceFactice::new(service, catalogue(true), vec![])).await;
        let (pl, e) = semer(state, service, &format!("choix {service}")).await;
        let (s, v) = appel(
            state,
            "POST",
            &format!("/api/v1/playlists/{pl}/tracks/{}/replace", e[1]),
            Some(json!({})),
        )
        .await;
        verifier(
            s == StatusCode::CONFLICT
                && v["code"] == "choix_requis"
                && v["candidates"].as_array().map(Vec::len) == Some(2),
            format!("{service} choix requis : {s} {v}"),
        );
        let inchange = entrees(state, pl).await;
        verifier(
            inchange[1].1 == "2",
            format!("{service} rien d'écrit sans choix : {inchange:?}"),
        );
        let (s, v) = appel(
            state,
            "POST",
            &format!("/api/v1/playlists/{pl}/tracks/{}/replace", e[1]),
            Some(json!({"source_id": "21"})),
        )
        .await;
        verifier(
            s == StatusCode::OK,
            format!("{service} choix écrit : {s} {v}"),
        );
        let apres = entrees(state, pl).await;
        verifier(
            apres[1] == (e[1], "21".to_string()),
            format!("{service} choix à sa place : {apres:?}"),
        );

        // 3. Aucun candidat.
        let (s, v) = appel(
            state,
            "POST",
            &format!("/api/v1/playlists/{pl}/tracks/{}/replace", e[4]),
            None,
        )
        .await;
        verifier(
            s == StatusCode::NOT_FOUND && v["code"] == "aucun_candidat",
            format!("{service} aucun : {s} {v}"),
        );

        // 4. Réparer la playlist : rapport et positions.
        enregistrer(
            state,
            ServiceFactice::new(service, catalogue(false), vec![]),
        )
        .await;
        let (pl, _) = semer(state, service, &format!("réparer {service}")).await;
        let (s, v) = appel(
            state,
            "POST",
            &format!("/api/v1/playlists/{pl}/repair"),
            None,
        )
        .await;
        verifier(
            s == StatusCode::OK && v["examined"] == 3,
            format!("{service} réparer : {s} {v}"),
        );
        verifier(
            v["replaced"][0]["position"] == 1 && v["replaced"][0]["new_source_id"] == "20",
            format!("{service} remplacés : {v}"),
        );
        verifier(
            v["to_choose"][0]["position"] == 3
                && v["to_choose"][0]["candidates"].as_array().map(Vec::len) == Some(2),
            format!("{service} à choisir : {v}"),
        );
        verifier(
            v["no_candidate"][0]["position"] == 4,
            format!("{service} sans candidat : {v}"),
        );
        let apres = entrees(state, pl).await;
        verifier(
            apres.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>() == ["1", "20", "3", "4", "5"],
            format!("{service} réparée, positions : {apres:?}"),
        );
    }

    // 5. Une ligne qui n'est pas un titre de service ne se remplace pas, et
    //    une ligne inconnue rend 404.
    let (pl, _) = semer(state, "qobuz", "garde").await;
    let (s, _) = appel(
        state,
        "POST",
        &format!("/api/v1/playlists/{pl}/tracks/999999/replace"),
        None,
    )
    .await;
    verifier(s == StatusCode::NOT_FOUND, format!("ligne inconnue : {s}"));
    ecarts
}

#[tokio::test(flavor = "multi_thread")]
async fn remplacer_les_indisponibles_sur_sqlite_8066() {
    let state = AppState::new(":memory:", 0, Default::default()).expect("AppState SQLite");
    let ecarts = scenario(&state).await;
    assert!(ecarts.is_empty(), "SQLite : {ecarts:#?}");
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_remplacer_les_indisponibles_8066() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
        return;
    };
    let config = tune_server::config::TuneConfig {
        database_url: Some(url),
        ..Default::default()
    };
    let state = AppState::new("", 0, config).expect("AppState sur PostgreSQL");
    let ecarts = scenario(&state).await;
    assert!(ecarts.is_empty(), "PostgreSQL : {ecarts:#?}");
}
