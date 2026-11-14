//! #7773 — une radio ne s'affiche plus deux fois dans les favoris.
//!
//! Cause : `radio_stations` ne porte aucune contrainte d'unicité (SQLite comme
//! PostgreSQL) et `POST /radios`, `POST /radios/import` et
//! `POST /radios/import/m3u` créaient une seconde ligne pour un flux déjà
//! présent. Les deux lignes ressortaient ensuite dans `GET /radios/favorites`
//! (Bilou, fil forum 2624).
//!
//! Le même scénario se joue sur SQLite (toujours) et sur une VRAIE base
//! PostgreSQL (`TUNE_TEST_PG_URL`, étape de `test-postgres.yml`) : variable
//! ABSENTE ⇒ saut annoncé ; variable POSÉE mais injoignable ⇒ le test TOMBE.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::radio_repo::{RadioRepo, RadioStation};
use tune_server::state::AppState;

/// Marque de nos lignes : sur PostgreSQL les tables sont partagées avec les
/// autres bancs, on ne compte et ne supprime que ce qui la porte.
const MARQUE: &str = "exemple-7773.test";
const FLUX: &str = "https://flux.exemple-7773.test/live";
/// Le même flux : hôte en majuscules, port HTTPS par défaut explicite.
const FLUX_AUTREMENT: &str = "https://FLUX.exemple-7773.test:443/live";
const FLUX_M3U: &str = "https://autre.exemple-7773.test/flux.mp3";
const FLUX_JSON: &str = "https://json.exemple-7773.test/flux.aac";

async fn envoyer(app: &axum::Router, requete: Request<Body>) -> (StatusCode, Value) {
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let status = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&octets).unwrap_or(json!(null)),
    )
}

async fn lire(app: &axum::Router, chemin: &str) -> Value {
    let (status, corps) = envoyer(app, Request::get(chemin).body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK, "{chemin}");
    corps
}

async fn poster(app: &axum::Router, chemin: &str, corps: Value) -> (StatusCode, Value) {
    envoyer(
        app,
        Request::post(chemin)
            .header("Content-Type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await
}

async fn poster_m3u(app: &axum::Router, contenu: &str) -> Value {
    let (status, corps) = envoyer(
        app,
        Request::post("/api/v1/radios/import/m3u")
            .header("Content-Type", "application/octet-stream")
            .body(Body::from(contenu.to_string()))
            .unwrap(),
    )
    .await;
    assert!(status.is_success(), "import M3U : {status} {corps}");
    corps
}

/// Nos stations rendues par une route (tableau JSON).
fn marquees(liste: &Value) -> Vec<&Value> {
    liste
        .as_array()
        .expect("tableau de stations")
        .iter()
        .filter(|s| {
            s["stream_url"]
                .as_str()
                .unwrap_or_default()
                .contains(MARQUE)
        })
        .collect()
}

/// Nos lignes en base, doublons compris.
fn lignes(state: &AppState, fragment: &str) -> Vec<RadioStation> {
    RadioRepo::with_backend(state.backend.clone())
        .list()
        .unwrap()
        .into_iter()
        .filter(|s| s.url.to_ascii_lowercase().contains(fragment))
        .collect()
}

fn menage(state: &AppState) {
    let repo = RadioRepo::with_backend(state.backend.clone());
    for s in lignes(state, MARQUE) {
        repo.delete(s.id.unwrap()).unwrap();
    }
}

fn station(nom: &str, url: &str) -> RadioStation {
    RadioStation {
        id: None,
        name: nom.to_string(),
        url: url.to_string(),
        homepage: None,
        logo_url: None,
        country: None,
        language: None,
        genre: None,
        codec: None,
        bitrate: None,
        is_favorite: true,
        last_played: None,
        play_count: 0,
    }
}

/// Une base d'avant le correctif : deux lignes favorites pour le même flux.
fn doublon_existant(state: &AppState) -> (i64, i64) {
    let repo = RadioRepo::with_backend(state.backend.clone());
    let a = repo.create(&station("Radio 7773", FLUX)).unwrap();
    let b = repo.create(&station("Radio 7773", FLUX_AUTREMENT)).unwrap();
    assert_ne!(a, b, "la base accepte bien deux lignes du même flux");
    (a, b)
}

async fn scenario(state: AppState) {
    menage(&state);
    let app = tune_server::routes::router(state.clone());

    // 1. Lecture : une base qui porte déjà le doublon n'affiche la radio
    //    qu'une fois, dans les favoris comme dans « Mes radios ».
    let (a, _b) = doublon_existant(&state);
    for chemin in [
        "/api/v1/radios/favorites",
        "/api/v1/radios?favorite=true",
        "/api/v1/home/radio-picks",
        "/api/v1/radios",
    ] {
        let liste = lire(&app, chemin).await;
        assert_eq!(
            marquees(&liste).len(),
            1,
            "{chemin} : la même radio apparaît deux fois : {:?}",
            marquees(&liste)
        );
    }

    // 2. Écriture : « Nouvelle station » sur un flux déjà présent rend la
    //    station existante, sans seconde ligne.
    let (status, corps) = poster(
        &app,
        "/api/v1/radios",
        json!({"name": "Radio 7773 bis", "stream_url": FLUX}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "flux déjà présent : {corps}");
    assert_eq!(
        corps["id"].as_i64(),
        Some(a),
        "la station existante est rendue"
    );
    assert_eq!(
        lignes(&state, "flux.exemple-7773.test").len(),
        2,
        "aucune ligne de plus"
    );

    state
        .annuaire_radios
        .write()
        .unwrap()
        .push(tune_server::routes::radios::StationAnnuaire {
            name: "Radio 7773 annuaire".into(),
            stream_url: FLUX.into(),
            external_id: Some("S7773".into()),
            ..Default::default()
        });
    let (status, _) = poster(
        &app,
        "/api/v1/radios/directory/S7773/add",
        json!({"favorite": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "ajout par l'identifiant d'annuaire");
    assert_eq!(
        lignes(&state, "flux.exemple-7773.test").len(),
        2,
        "l'ajout par identifiant ne recrée pas la station ajoutée par URL"
    );

    // 3. Le cœur vaut pour les deux lignes du même flux : retirer la radio
    //    visible des favoris ne fait pas réapparaître sa jumelle.
    let (status, _) = poster(
        &app,
        &format!("/api/v1/radios/{a}/favorite"),
        json!({"favorite": false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let favoris = lire(&app, "/api/v1/radios/favorites").await;
    assert!(
        marquees(&favoris).is_empty(),
        "la jumelle est restée en favori : {:?}",
        marquees(&favoris)
    );

    // 4. Import M3U rejoué (et ligne répétée dans le fichier) : une station.
    let m3u = format!(
        "#EXTM3U\n#EXTINF:-1,Autre 7773\n{FLUX_M3U}\n#EXTINF:-1,Autre 7773 (copie)\n{}\n",
        FLUX_M3U.replace("autre.exemple-7773.test", "AUTRE.exemple-7773.test:443")
    );
    let premier = poster_m3u(&app, &m3u).await;
    assert_eq!(premier["imported"], 1, "{premier}");
    assert_eq!(premier["already_present"], 1, "{premier}");
    let second = poster_m3u(&app, &m3u).await;
    assert_eq!(second["imported"], 0, "{second}");
    assert_eq!(second["already_present"], 2, "{second}");
    assert_eq!(lignes(&state, "autre.exemple-7773.test").len(), 1);

    // 5. Import JSON rejoué : une station.
    let lot = json!({"stations": [{"name": "Json 7773", "stream_url": FLUX_JSON}]});
    let (status, premier) = poster(&app, "/api/v1/radios/import", lot.clone()).await;
    assert!(status.is_success(), "{status} {premier}");
    assert_eq!(premier["imported"], 1, "{premier}");
    let (status, second) = poster(&app, "/api/v1/radios/import", lot).await;
    assert!(status.is_success(), "{status} {second}");
    assert_eq!(second["imported"], 0, "{second}");
    assert_eq!(second["already_present"], 1, "{second}");
    assert_eq!(lignes(&state, "json.exemple-7773.test").len(), 1);

    // 6. Un nom identique n'autorise pas à cacher des flux différents.
    let variantes = [
        "https://distinct.exemple-7773.test/live",
        "https://distinct.exemple-7773.test/Live",
        "https://distinct.exemple-7773.test/live/",
        "http://distinct.exemple-7773.test/live",
        "https://distinct.exemple-7773.test/live?token=A",
        "https://distinct.exemple-7773.test/live?token=a",
    ];
    let repo = RadioRepo::with_backend(state.backend.clone());
    for url in variantes {
        assert!(
            repo.create_unique(&station("Même nom", url)).unwrap().1,
            "le flux distinct doit être créé : {url}"
        );
    }
    let favoris = lire(&app, "/api/v1/radios/favorites").await;
    assert_eq!(
        marquees(&favoris)
            .iter()
            .filter(|r| r["stream_url"].as_str().unwrap().contains("distinct."))
            .count(),
        variantes.len(),
        "aucun flux distinct ne doit être masqué"
    );

    // 7. Quatre connexions concurrentes : une seule insertion validée.
    let barriere = std::sync::Arc::new(std::sync::Barrier::new(4));
    let mut jobs = Vec::new();
    for n in 0..4 {
        let backend = state.backend.clone();
        let barriere = barriere.clone();
        jobs.push(tokio::task::spawn_blocking(move || {
            let repo = RadioRepo::with_backend(backend);
            barriere.wait();
            repo.create_unique(&station(
                &format!("Concurrent {n}"),
                "https://concurrent.exemple-7773.test/live",
            ))
            .unwrap()
        }));
    }
    let mut ids = std::collections::HashSet::new();
    let mut creations = 0;
    for job in jobs {
        let (id, cree) = job.await.unwrap();
        ids.insert(id);
        creations += usize::from(cree);
    }
    assert_eq!(
        ids.len(),
        1,
        "#7773 : des ajouts simultanés ont créé plusieurs radios"
    );
    assert_eq!(creations, 1, "une seule requête doit annoncer une création");
    assert_eq!(lignes(&state, "concurrent.exemple-7773.test").len(), 1);

    menage(&state);
}

#[tokio::test(flavor = "multi_thread")]
async fn sqlite_une_radio_n_apparait_qu_une_fois_7773() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    scenario(state).await;
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_une_radio_n_apparait_qu_une_fois_7773() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT ANNONCÉ : TUNE_TEST_PG_URL absente, épreuve PostgreSQL #7773 non jouée");
        return;
    };
    let config = tune_server::config::TuneConfig {
        database_url: Some(url),
        ..Default::default()
    };
    let state = AppState::new("", 0, config).expect("AppState sur PostgreSQL");
    scenario(state).await;
}
