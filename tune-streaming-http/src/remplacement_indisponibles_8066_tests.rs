//! #8066 — routes de remplacement dans une playlist DU SERVICE, sur de faux
//! Qobuz et TIDAL (aucun réseau, aucun compte réel).

use super::*;
use tune_core::db::sqlite::SqliteDb;
use tune_core::streaming::remplacement_indisponibles::factice::{ServiceFactice, piste};

/// La playlist : A (dispo), B (INDISPONIBLE, rang 1), C (dispo).
fn playlist() -> Vec<StreamTrack> {
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
    ]
}

fn republie() -> StreamTrack {
    piste(
        "20",
        "Teardrop",
        "Massive Attack",
        "Mezzanine",
        331_000,
        Some(true),
    )
}

fn app(svc: ServiceFactice) -> Router {
    let backend: Arc<dyn DbBackend> = Arc::new(SqliteDb::open_in_memory().expect("sqlite"));
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(svc));
    let etat = StreamingHttpState::new(
        backend,
        Arc::new(Mutex::new(registre)),
        Arc::new(EventBus::new()),
    );
    Router::new().nest("/api/v1/streaming", router().with_state(etat))
}

async fn appel(
    app: &Router,
    methode: &str,
    uri: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    use tower::ServiceExt;
    let mut req = axum::http::Request::builder().method(methode).uri(uri);
    let body = match corps {
        Some(c) => {
            req = req.header("content-type", "application/json");
            axum::body::Body::from(c.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let rep = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let statut = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn ids(svc_playlist: &std::sync::Mutex<Vec<StreamTrack>>) -> Vec<String> {
    svc_playlist
        .lock()
        .unwrap()
        .iter()
        .map(|p| p.id.clone())
        .collect()
}

#[tokio::test]
async fn un_seul_candidat_sur_remplace_directement_a_sa_place_8066() {
    for nom in ["qobuz", "tidal"] {
        let svc = ServiceFactice::new(nom, vec![republie()], playlist());
        let liste = svc.playlist.clone();
        let app = app(svc);
        let (s, v) = appel(
            &app,
            "GET",
            &format!("/api/v1/streaming/{nom}/playlists/pl/tracks/1/replacements"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{nom} {v}");
        assert_eq!(v["decision"], "direct", "{nom} {v}");
        assert_eq!(v["unavailable"], true);
        assert_eq!(v["candidates"][0]["source_id"], "20");
        assert_eq!(v["candidates"][0]["sure"], true);
        assert!(v["candidates"][0]["score"].as_f64().unwrap() >= 0.9);

        let (s, v) = appel(
            &app,
            "POST",
            &format!("/api/v1/streaming/{nom}/playlists/pl/tracks/1/replace"),
            Some(json!({"track_id": "2"})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{nom} {v}");
        assert_eq!(ids(&liste), ["1", "20", "3"], "{nom} : position conservée");
    }
}

#[tokio::test]
async fn plusieurs_candidats_demandent_un_choix_puis_le_choix_est_ecrit_8066() {
    for nom in ["qobuz", "tidal"] {
        let mut jumeau = republie();
        jumeau.id = "21".into();
        let svc = ServiceFactice::new(nom, vec![republie(), jumeau], playlist());
        let liste = svc.playlist.clone();
        let ecritures = svc.ecritures.clone();
        let app = app(svc);
        let (s, v) = appel(
            &app,
            "POST",
            &format!("/api/v1/streaming/{nom}/playlists/pl/tracks/1/replace"),
            Some(json!({"track_id": "2"})),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT, "{nom} {v}");
        assert_eq!(v["code"], "choix_requis");
        assert_eq!(v["candidates"].as_array().unwrap().len(), 2);
        assert_eq!(
            ecritures.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "rien d'écrit sans choix"
        );

        let (s, v) = appel(
            &app,
            "POST",
            &format!("/api/v1/streaming/{nom}/playlists/pl/tracks/1/replace"),
            Some(json!({"track_id": "2", "source_id": "21"})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{nom} {v}");
        assert_eq!(ids(&liste), ["1", "21", "3"]);
    }
}

#[tokio::test]
async fn aucun_candidat_8066() {
    let autre = piste(
        "30",
        "Teardrop",
        "José González",
        "Veneer",
        330_000,
        Some(true),
    );
    let svc = ServiceFactice::new("tidal", vec![autre], playlist());
    let app = app(svc);
    let (s, v) = appel(
        &app,
        "POST",
        "/api/v1/streaming/tidal/playlists/pl/tracks/1/replace",
        Some(json!({"track_id": "2"})),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(v["code"], "aucun_candidat");
    assert_eq!(v["decision"], "aucun");
}

#[tokio::test]
async fn une_playlist_non_modifiable_est_refusee_clairement_8066() {
    for (nom, modifiable) in [("qobuz", Some(false)), ("tidal", None)] {
        let mut svc = ServiceFactice::new(nom, vec![republie()], playlist());
        svc.modifiable = modifiable;
        let liste = svc.playlist.clone();
        let ecritures = svc.ecritures.clone();
        let app = app(svc);
        for (uri, corps) in [
            (
                format!("/api/v1/streaming/{nom}/playlists/pl/tracks/1/replace"),
                Some(json!({"track_id": "2"})),
            ),
            (format!("/api/v1/streaming/{nom}/playlists/pl/repair"), None),
        ] {
            let (s, v) = appel(&app, "POST", &uri, corps).await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{nom} {uri} {v}");
            assert_eq!(v["code"], "playlist_non_modifiable");
            assert!(
                v["error"].as_str().unwrap().contains("pas modifiable"),
                "{v}"
            );
        }
        assert_eq!(ecritures.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(ids(&liste), ["1", "2", "3"]);
    }
}

#[tokio::test]
async fn une_playlist_changee_n_est_pas_ecrite_8066() {
    let svc = ServiceFactice::new("qobuz", vec![republie()], playlist());
    let ecritures = svc.ecritures.clone();
    let app = app(svc);
    let (s, v) = appel(
        &app,
        "POST",
        "/api/v1/streaming/qobuz/playlists/pl/tracks/1/replace",
        Some(json!({"track_id": "3"})),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["code"], "ligne_modifiee");
    assert_eq!(ecritures.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[tokio::test]
async fn reparer_rend_le_rapport_et_garde_les_positions_8066() {
    let mut liste = playlist();
    // Rang 3 : indisponible, deux candidats sûrs. Rang 4 : indisponible, rien.
    liste.push(piste(
        "4",
        "Dissolved Girl",
        "Massive Attack",
        "Mezzanine",
        366_000,
        Some(false),
    ));
    liste.push(piste(
        "5",
        "Exchange",
        "Massive Attack",
        "Mezzanine",
        251_000,
        Some(false),
    ));
    let catalogue = vec![
        republie(),
        piste(
            "40",
            "Dissolved Girl",
            "Massive Attack",
            "Mezzanine",
            366_000,
            Some(true),
        ),
        piste(
            "41",
            "Dissolved Girl",
            "Massive Attack",
            "Mezzanine",
            367_000,
            Some(true),
        ),
    ];
    for nom in ["qobuz", "tidal"] {
        let svc = ServiceFactice::new(nom, catalogue.clone(), liste.clone());
        let pl = svc.playlist.clone();
        let app = app(svc);
        let (s, v) = appel(
            &app,
            "POST",
            &format!("/api/v1/streaming/{nom}/playlists/pl/repair"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{nom} {v}");
        assert_eq!(v["examined"], 3);
        assert_eq!(v["replaced"][0]["index"], 1);
        assert_eq!(v["replaced"][0]["new_source_id"], "20");
        assert_eq!(v["to_choose"][0]["index"], 3);
        assert_eq!(v["to_choose"][0]["candidates"].as_array().unwrap().len(), 2);
        assert_eq!(v["no_candidate"][0]["index"], 4);
        assert_eq!(
            ids(&pl),
            ["1", "20", "3", "4", "5"],
            "{nom} : positions conservées"
        );
    }
}
