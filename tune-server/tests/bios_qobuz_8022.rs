//! #8022 — biographies d'artiste et notes d'album de la bibliothèque LOCALE
//! prises chez Qobuz, à la demande, pour un compte connecté.
//!
//! Par le VRAI routeur, contre une doublure axum de l'API Qobuz sur
//! `127.0.0.1` (`QobuzService::avec_bases_de_test`, feature
//! `services-factices`). Le site de biographies est injoignable (proxy
//! fermé) : la route passe donc à Qobuz. Aucun appel réseau réel ; jeton,
//! identifiants et textes sont factices.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::models::{Album, Artist, Track};
use tune_core::db::track_repo::TrackRepo;
use tune_core::streaming::StreamingService;
use tune_core::streaming::qobuz::{BasesQobuz, QobuzService};

type Etat = tune_server::state::AppState;

const NOM: &str = "Groupe Factice";
const UPC: &str = "0123456789012";
const ISRC: &str = "XXA0D2600001";

/// Ce que la doublure répond, et ce qu'elle a reçu.
#[derive(Clone, Default)]
struct Doublure {
    reponses: Arc<HashMap<&'static str, Value>>,
    appels: Arc<Mutex<Vec<String>>>,
}

async fn repondre(
    State(d): State<Doublure>,
    chemin: &'static str,
    q: HashMap<String, String>,
) -> Json<Value> {
    d.appels.lock().unwrap().push(format!(
        "{chemin}?{}",
        q.get("query")
            .or(q.get("artist_id"))
            .or(q.get("album_id"))
            .cloned()
            .unwrap_or_default()
    ));
    Json(d.reponses.get(chemin).cloned().unwrap_or(json!({})))
}

async fn doublure_qobuz(reponses: HashMap<&'static str, Value>) -> (String, Doublure) {
    let d = Doublure {
        reponses: Arc::new(reponses),
        appels: Arc::default(),
    };
    let app = Router::new()
        .route(
            "/album/search",
            get(
                |s: State<Doublure>, Query(q): Query<HashMap<String, String>>| {
                    repondre(s, "/album/search", q)
                },
            ),
        )
        .route(
            "/track/search",
            get(
                |s: State<Doublure>, Query(q): Query<HashMap<String, String>>| {
                    repondre(s, "/track/search", q)
                },
            ),
        )
        .route(
            "/artist/search",
            get(
                |s: State<Doublure>, Query(q): Query<HashMap<String, String>>| {
                    repondre(s, "/artist/search", q)
                },
            ),
        )
        .route(
            "/artist/get",
            get(
                |s: State<Doublure>, Query(q): Query<HashMap<String, String>>| {
                    repondre(s, "/artist/get", q)
                },
            ),
        )
        .route(
            "/album/get",
            get(
                |s: State<Doublure>, Query(q): Query<HashMap<String, String>>| {
                    repondre(s, "/album/get", q)
                },
            ),
        )
        .with_state(d.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    (format!("http://{adresse}"), d)
}

/// Un état dont le site de bios est injoignable et dont Qobuz pointe sur la
/// doublure — connecté (`jeton`) ou non.
async fn etat(base: &str, connecte: bool, langue_magasin: Option<&str>) -> (Router, Etat) {
    let mut state = Etat::new(":memory:", 0, Default::default()).unwrap();
    let ferme = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let adresse = ferme.local_addr().unwrap();
    drop(ferme);
    state.http_client = tune_core::http::client::builder()
        .proxy(reqwest::Proxy::all(format!("http://{adresse}")).unwrap())
        .build()
        .unwrap();
    let mut svc = QobuzService::avec_bases_de_test(
        "app-essai".into(),
        "secret-essai".into(),
        BasesQobuz {
            api: base.to_string(),
            proxy: base.to_string(),
            config_distante: base.to_string(),
        },
    );
    if connecte {
        assert!(svc.restore_tokens(&json!({
            "user_auth_token": "jeton-essai",
            "app_id": "app-essai",
            "langue_magasin": langue_magasin,
        })));
    }
    state.services.lock().await.register(Box::new(svc));
    let router = tune_server::routes::router(state.clone());
    (router, state)
}

/// L'artiste 1, et son album 1 (code-barres et/ou un titre à ISRC).
fn bibliotheque(state: &Etat, upc: Option<&str>, isrc: Option<&str>) {
    let id = ArtistRepo::with_backend(state.backend.clone())
        .create(&Artist::new(NOM.into()))
        .unwrap();
    assert_eq!(id, 1);
    let mut album = Album::new("Disque Factice".into());
    album.artist_id = Some(1);
    album.barcode = upc.map(str::to_string);
    let id_album = AlbumRepo::with_backend(state.backend.clone())
        .create(&album)
        .unwrap();
    assert_eq!(id_album, 1);
    let mut piste = Track::new("Titre Factice".into());
    piste.album_id = Some(1);
    piste.artist_id = Some(1);
    piste.file_path = Some("/musique/factice/01.flac".into());
    piste.isrc = isrc.map(str::to_string);
    TrackRepo::with_backend(state.backend.clone())
        .create(&piste)
        .unwrap();
}

async fn demander(app: &Router, chemin: &str, langue: &str) -> (StatusCode, Value) {
    let req = Request::get(chemin)
        .header(axum::http::header::ACCEPT_LANGUAGE, langue)
        .body(Body::empty())
        .unwrap();
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

fn reponses_artiste(langue_bio: Option<&str>) -> HashMap<&'static str, Value> {
    let mut biographie = json!({
        "content": "<p>Biographie factice &amp; <em>courte</em>.</p><p>Second paragraphe.</p>",
    });
    if let Some(l) = langue_bio {
        biographie["language"] = json!(l);
    }
    HashMap::from([
        (
            "/album/search",
            json!({"albums": {"items": [
                {"id": "alb-voisin", "upc": "0999999999999", "artist": {"id": 5, "name": "Autre"}},
                {"id": "alb-1", "upc": UPC, "artist": {"id": 77, "name": NOM}},
            ]}}),
        ),
        (
            "/artist/get",
            json!({"id": 77, "name": NOM, "biography": biographie}),
        ),
    ])
}

/// Règles 1, 3, 4, 6 : pas de bio dans la langue, compte connecté, album
/// retrouvé par son code-barres, artiste par l'album. La bio Qobuz est servie
/// avec sa langue, sa source et son lien, et rangée dans la fiche locale.
#[tokio::test]
async fn artiste_bio_qobuz_par_le_code_barres_de_son_album_8022() {
    let (base, d) = doublure_qobuz(reponses_artiste(Some("en"))).await;
    let (app, state) = etat(&base, true, Some("fr")).await;
    bibliotheque(&state, Some(UPC), None);

    let (status, body) = demander(&app, "/api/v1/library/artists/1/bio", "en").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["bio"].as_str(),
        Some("Biographie factice & courte.\n\nSecond paragraphe."),
        "{body}"
    );
    assert_eq!(body["source"], "qobuz", "{body}");
    assert_eq!(body["bio_provenance"]["source"], "qobuz", "{body}");
    assert_eq!(body["bio_provenance"]["lang"], "en", "{body}");
    assert_eq!(
        body["bio_provenance"]["source_url"], "https://open.qobuz.com/artist/77",
        "{body}"
    );
    assert!(body.get("repli").is_none(), "{body}");
    let prov = ArtistRepo::with_backend(state.backend.clone())
        .bio_provenance(1)
        .unwrap()
        .expect("provenance rangée");
    assert_eq!(prov["source"], "qobuz");
    assert_eq!(prov["lang"], "en");
    let appels = d.appels.lock().unwrap().clone();
    assert!(
        appels.iter().all(|a| !a.starts_with("/artist/search")),
        "l'artiste vient de l'album retrouvé, pas d'une recherche par nom : {appels:?}"
    );

    // Rouverte : servie depuis la fiche locale, sans rappeler Qobuz.
    let n = d.appels.lock().unwrap().len();
    let (_, encore) = demander(&app, "/api/v1/library/artists/1/bio", "en").await;
    assert_eq!(encore["bio_provenance"]["source"], "qobuz", "{encore}");
    assert_eq!(d.appels.lock().unwrap().len(), n, "Qobuz rappelé");
}

/// Règle 2 : sans compte Qobuz connecté, AUCUN appel.
#[tokio::test]
async fn sans_compte_qobuz_connecte_aucun_appel_8022() {
    let (base, d) = doublure_qobuz(reponses_artiste(Some("en"))).await;
    let (app, state) = etat(&base, false, None).await;
    bibliotheque(&state, Some(UPC), Some(ISRC));

    let (_, artiste) = demander(&app, "/api/v1/library/artists/1/bio", "en").await;
    let (_, album) = demander(&app, "/api/v1/library/albums/1/bio", "en").await;

    assert!(artiste["bio"].is_null(), "{artiste}");
    assert!(album["bio"].is_null(), "{album}");
    assert!(
        d.appels.lock().unwrap().is_empty(),
        "Qobuz interrogé sans compte connecté : {:?}",
        d.appels.lock().unwrap()
    );
}

/// Règle 3 : pas de correspondance sûre, rien. Le code-barres ne correspond
/// à aucun album rendu, et DEUX artistes Qobuz portent exactement le nom.
#[tokio::test]
async fn correspondance_non_sure_rien_8022() {
    let mut reponses = reponses_artiste(Some("en"));
    reponses.insert(
        "/album/search",
        json!({"albums": {"items": [
            {"id": "alb-voisin", "upc": "0999999999999", "artist": {"id": 77, "name": NOM}},
        ]}}),
    );
    reponses.insert(
        "/artist/search",
        json!({"artists": {"items": [
            {"id": 77, "name": NOM},
            {"id": 78, "name": "groupe factice"},
        ]}}),
    );
    let (base, d) = doublure_qobuz(reponses).await;
    let (app, state) = etat(&base, true, Some("fr")).await;
    bibliotheque(&state, Some(UPC), None);

    let (status, body) = demander(&app, "/api/v1/library/artists/1/bio", "en").await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        body["bio"].is_null(),
        "correspondance non sûre servie : {body}"
    );
    let appels = d.appels.lock().unwrap().clone();
    assert!(
        appels.iter().all(|a| !a.starts_with("/artist/get")),
        "une bio a été demandée sans correspondance sûre : {appels:?}"
    );
    assert!(
        ArtistRepo::with_backend(state.backend.clone())
            .bio_provenance(1)
            .unwrap()
            .is_none()
    );
}

/// Règle 3 (nom) : sans album retrouvé, un SEUL artiste Qobuz du nom exact.
#[tokio::test]
async fn artiste_par_nom_exact_unique_8022() {
    let mut reponses = reponses_artiste(Some("en"));
    reponses.insert("/album/search", json!({"albums": {"items": []}}));
    reponses.insert(
        "/artist/search",
        json!({"artists": {"items": [
            {"id": 77, "name": NOM},
            {"id": 79, "name": "Groupe Factice Tribute"},
        ]}}),
    );
    let (base, _d) = doublure_qobuz(reponses).await;
    let (app, state) = etat(&base, true, None).await;
    bibliotheque(&state, Some(UPC), None);

    let (_, body) = demander(&app, "/api/v1/library/artists/1/bio", "en").await;

    assert_eq!(body["bio_provenance"]["source"], "qobuz", "{body}");
}

/// Règle 4 : Qobuz n'annonce pas la langue de la bio — celle du magasin du
/// compte. Interface dans une autre langue : servie en repli, dite.
#[tokio::test]
async fn langue_du_magasin_a_defaut_et_repli_dit_8022() {
    let (base, _d) = doublure_qobuz(reponses_artiste(None)).await;
    let (app, state) = etat(&base, true, Some("fr")).await;
    bibliotheque(&state, Some(UPC), None);

    let (_, body) = demander(&app, "/api/v1/library/artists/1/bio", "en").await;

    assert_eq!(body["bio_provenance"]["lang"], "fr", "{body}");
    assert_eq!(body["repli"]["raison"], "langue_indisponible", "{body}");
    assert_eq!(body["repli"]["langue_disponible"], "fr", "{body}");
    let prov = ArtistRepo::with_backend(state.backend.clone())
        .bio_provenance(1)
        .unwrap()
        .expect("rangée avec la langue du magasin");
    assert_eq!(prov["lang"], "fr");
}

/// Note d'album par l'ISRC de ses titres (pas de code-barres).
#[tokio::test]
async fn album_note_qobuz_par_isrc_8022() {
    let reponses = HashMap::from([
        (
            "/track/search",
            json!({"tracks": {"items": [
                {"id": 1, "isrc": ISRC, "album": {"id": "alb-isrc", "artist": {"id": 77, "name": NOM}}},
                {"id": 2, "isrc": "XXA0D2600099", "album": {"id": "alb-autre", "artist": {"id": 5, "name": "Autre"}}},
            ]}}),
        ),
        (
            "/album/get",
            json!({
                "id": "alb-isrc",
                "title": "Disque Factice",
                "description": "<p>Note factice d&#39;album.</p>",
                "description_language": "fr",
                "tracks": {"items": [], "total": 0},
            }),
        ),
    ]);
    let (base, _d) = doublure_qobuz(reponses).await;
    let (app, state) = etat(&base, true, Some("en")).await;
    bibliotheque(&state, None, Some(ISRC));

    let (status, body) = demander(&app, "/api/v1/library/albums/1/bio", "fr").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["bio"].as_str(),
        Some("Note factice d'album."),
        "{body}"
    );
    assert_eq!(body["source"], "qobuz", "{body}");
    assert_eq!(body["bio_provenance"]["lang"], "fr", "{body}");
    assert_eq!(
        body["bio_provenance"]["source_url"], "https://open.qobuz.com/album/alb-isrc",
        "{body}"
    );
    let prov = AlbumRepo::with_backend(state.backend.clone())
        .bio_provenance(1)
        .unwrap()
        .expect("note rangée");
    assert_eq!(prov["source"], "qobuz");
}

/// Une bio d'une autre source déjà rangée n'est jamais écrasée par Qobuz.
#[tokio::test]
async fn une_bio_d_une_autre_source_n_est_pas_ecrasee_8022() {
    let (base, _d) = doublure_qobuz(reponses_artiste(Some("en"))).await;
    let (app, state) = etat(&base, true, None).await;
    bibliotheque(&state, Some(UPC), None);
    ArtistRepo::with_backend(state.backend.clone())
        .update_bio_full(
            1,
            "Bio Wikipédia factice.",
            "wikipedia",
            None,
            "CC-BY-SA-4.0",
            "fr",
        )
        .unwrap();

    let (_, body) = demander(&app, "/api/v1/library/artists/1/bio", "en").await;

    assert_eq!(body["bio_provenance"]["source"], "qobuz", "{body}");
    assert_eq!(body["bio_provenance"]["lang"], "en", "{body}");
    let prov = ArtistRepo::with_backend(state.backend.clone())
        .bio_provenance(1)
        .unwrap()
        .unwrap();
    assert_eq!(
        prov["source"], "wikipedia",
        "la bio Wikipédia a été écrasée"
    );
}
