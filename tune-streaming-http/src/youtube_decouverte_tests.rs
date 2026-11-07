//! Routes YouTube Music de découverte et de bibliothèque (#1897, #5247).
//!
//! Le vrai `YouTubeService` parle à un faux YouTube LOCAL (127.0.0.1) qui
//! rejoue des réponses InnerTube réelles enregistrées le 07/10/2026
//! (`tune-core/tests/fixtures/youtube/`). Aucun appel ne quitte la machine.
//!
//! Les réponses de l'API Data v3 (`playlists?mine=true`, `videos?myRating=
//! like`) sont, elles, ÉCRITES d'après la documentation de Google : aucun
//! compte n'était disponible pour les enregistrer. Elles prouvent le chemin
//! et la forme, pas que le client OAuth actuel ouvre l'API Data (#5247).

use super::*;
use axum::body::to_bytes;
use axum::extract::RawQuery;
use axum::http::HeaderMap;
use std::sync::Mutex as StdMutex;
use tune_core::db::sqlite::SqliteDb;
use tune_core::streaming::youtube::YouTubeService;

const JETON: &str = "jeton-essai-5247";

#[derive(Clone, Copy, PartialEq)]
enum ModeData {
    Normal,
    /// Projet Google sans l'API Data activée : la réponse documentée.
    AccesNonConfigure,
}

#[derive(Clone)]
struct FauxYoutube {
    /// Corps des POST InnerTube reçus, et requêtes Data API (`chemin?requête`).
    vus: Arc<StdMutex<Vec<String>>>,
    illisible: bool,
    mode: ModeData,
}

fn fixture(nom: &str) -> Value {
    let chemin = format!(
        "{}/../tune-core/tests/fixtures/youtube/{nom}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(&chemin).expect(&chemin)).expect("json")
}

async fn innertube(
    axum::extract::State(f): axum::extract::State<FauxYoutube>,
    Json(corps): Json<Value>,
) -> Response {
    f.vus.lock().unwrap().push(corps.to_string());
    if f.illisible {
        return Json(json!({"contents": {"autreChose": {}}})).into_response();
    }
    // Le faux refuse un appel sans contexte client, comme InnerTube.
    if corps["context"]["client"]["clientName"] != "WEB_REMIX" {
        return (StatusCode::BAD_REQUEST, "contexte absent").into_response();
    }
    let nom = match corps["browseId"].as_str().unwrap_or("") {
        "FEmusic_home" => "home",
        // Le pays demandé est vérifié par les témoins dans `vus`.
        "FEmusic_charts" => "charts_fr",
        "FEmusic_moods_and_genres" => "moods",
        "FEmusic_moods_and_genres_category" if corps["params"] == "ggMPOg1uX1JOQWZFeDByc2Jm" => {
            "mood_category"
        }
        "VLRDCLAK5uy_nBE4bLuBHUXWZrF59ZrkPEToKt8M_I3Vc" => "playlist",
        _ => return (StatusCode::BAD_REQUEST, "browseId inconnu du faux").into_response(),
    };
    Json(fixture(nom)).into_response()
}

async fn data_api(
    axum::extract::State(f): axum::extract::State<FauxYoutube>,
    Path(endpoint): Path<String>,
    RawQuery(requete): RawQuery,
    entetes: HeaderMap,
) -> Response {
    let requete = requete.unwrap_or_default();
    f.vus.lock().unwrap().push(format!("{endpoint}?{requete}"));
    if entetes.get("authorization").and_then(|v| v.to_str().ok())
        != Some(&format!("Bearer {JETON}"))
    {
        return (StatusCode::UNAUTHORIZED, "jeton absent").into_response();
    }
    if f.mode == ModeData::AccesNonConfigure {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": {"code": 403,
                "message": "YouTube Data API v3 has not been used in project 1 before or it is disabled.",
                "errors": [{"reason": "accessNotConfigured", "domain": "usageLimits"}]}})),
        )
            .into_response();
    }
    match endpoint.as_str() {
        "playlists" if requete.contains("mine=true") => {
            // Deux pages : la seconde n'est lue que si `pageToken` est suivi.
            if requete.contains("pageToken=p2") {
                Json(json!({"items": [{"id": "PLdeux",
                    "snippet": {"title": "Deuxième", "channelTitle": "Moi", "thumbnails": {}},
                    "contentDetails": {"itemCount": 3}}]}))
                .into_response()
            } else {
                Json(json!({"nextPageToken": "p2", "items": [{"id": "PLun",
                    "snippet": {"title": "Ma playlist", "description": "à moi",
                        "channelTitle": "Moi",
                        "thumbnails": {"high": {"url": "https://i.ytimg.com/vi/x/hqdefault.jpg"}}},
                    "contentDetails": {"itemCount": 12}}]}))
                .into_response()
            }
        }
        "videos" if requete.contains("myRating=like") => Json(json!({"items": [
            {"id": "musique1", "snippet": {"title": "Un titre", "channelTitle": "Artiste - Topic",
                "categoryId": "10", "thumbnails": {}}, "contentDetails": {"duration": "PT3M5S"}},
            {"id": "tuto1", "snippet": {"title": "Un tutoriel", "channelTitle": "Chaîne",
                "categoryId": "27", "thumbnails": {}}, "contentDetails": {"duration": "PT10M"}}
        ]}))
        .into_response(),
        _ => (StatusCode::NOT_FOUND, "inconnu du faux").into_response(),
    }
}

/// Démarre le faux YouTube et rend un état dont le service « youtube » le vise.
async fn etat(
    illisible: bool,
    mode: ModeData,
    jeton: bool,
) -> (StreamingHttpState, Arc<StdMutex<Vec<String>>>) {
    let vus = Arc::new(StdMutex::new(Vec::new()));
    let faux = FauxYoutube {
        vus: vus.clone(),
        illisible,
        mode,
    };
    let app = Router::new()
        .route("/youtubei/v1/browse", post(innertube))
        .route("/youtube/v3/{endpoint}", get(data_api))
        .with_state(faux);
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(ecoute, app).await.unwrap() });

    let mut yt = YouTubeService::avec_bases(
        &format!("http://{adresse}/youtubei/v1"),
        &format!("http://{adresse}/youtube/v3"),
    );
    if jeton {
        yt = yt.avec_jeton_pour_essai(JETON);
    }
    let mut registre = ServiceRegistry::new();
    registre.register(Box::new(yt));
    // Schéma complet : la route des tendances lit le réglage du pays.
    let db = SqliteDb::open_in_memory().expect("sqlite en memoire");
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    (
        StreamingHttpState::new(
            backend,
            Arc::new(Mutex::new(registre)),
            Arc::new(EventBus::new()),
        ),
        vus,
    )
}

async fn corps(r: Response) -> (StatusCode, Value, String) {
    let statut = r.status();
    let octets = to_bytes(r.into_body(), usize::MAX).await.unwrap();
    let texte = String::from_utf8_lossy(&octets).to_string();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
        texte,
    )
}

fn titres(sections: &Value) -> Vec<String> {
    sections
        .as_array()
        .expect("sections : un tableau")
        .iter()
        .map(|s| s["title"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn accueil_rend_les_rayons_reels() {
    let (etat, vus) = etat(false, ModeData::Normal, false).await;
    let (statut, v, _) = corps(youtube_home(State(etat.clone())).await).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        titres(&v["sections"]),
        ["Quick picks", "Today's biggest hits", "Throwback"]
    );
    let piste = &v["sections"][0]["items"][0];
    assert_eq!(piste["kind"], "track");
    assert_eq!(piste["id"], "V-uIp-WuD60");
    assert!(
        piste["cover_path"]
            .as_str()
            .unwrap()
            .starts_with("https://")
    );

    // Trente minutes de cache : le second appel ne retourne pas chez YouTube.
    let (statut, _, _) = corps(youtube_home(State(etat)).await).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(vus.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn tendances_du_pays_demande() {
    let (etat, vus) = etat(false, ModeData::Normal, false).await;
    let q = Query(ChartsQuery {
        country: Some("fr".into()),
    });
    let (statut, v, texte) = corps(youtube_charts(State(etat), q).await).await;
    assert_eq!(statut, StatusCode::OK, "{texte}");
    assert_eq!(v["country"], "FR");
    assert_eq!(v["country_source"], "request");
    assert_eq!(titres(&v["sections"]), ["Video charts", "Top artists"]);
    assert_eq!(v["sections"][0]["items"][0]["title"], "Trending 20 France");
    assert_eq!(v["sections"][1]["items"][0]["kind"], "artist");
    assert!(vus.lock().unwrap()[0].contains(r#""selectedValues":["FR"]"#));
}

/// Le RÉGLAGE explicite du pays l'emporte sur la langue du navigateur que le
/// web envoie en `?country=`.
#[tokio::test]
async fn le_reglage_du_pays_l_emporte_sur_la_requete() {
    let (etat, vus) = etat(false, ModeData::Normal, false).await;
    SettingsRepo::with_backend(etat.backend.clone())
        .set(
            tune_core::streaming::youtube_decouverte::CLE_PAYS_TENDANCES,
            "DE",
        )
        .unwrap();
    let q = Query(ChartsQuery {
        country: Some("FR".into()),
    });
    let r = youtube_charts(State(etat), q).await;
    assert!(
        r.headers().get(axum::http::header::CACHE_CONTROL).is_none(),
        "pas de cache navigateur : la même URL change de pays avec le réglage"
    );
    let (statut, v, texte) = corps(r).await;
    assert_eq!(statut, StatusCode::OK, "{texte}");
    assert_eq!(v["country"], "DE");
    assert_eq!(v["country_source"], "setting");
    assert!(vus.lock().unwrap()[0].contains(r#""selectedValues":["DE"]"#));
}

#[tokio::test]
async fn un_pays_invalide_est_refuse_sans_appeler_youtube() {
    let (etat, vus) = etat(false, ModeData::Normal, false).await;
    let q = Query(ChartsQuery {
        country: Some("../x".into()),
    });
    let (statut, _, _) = corps(youtube_charts(State(etat), q).await).await;
    assert_eq!(statut, StatusCode::NOT_FOUND);
    assert!(vus.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ambiances_puis_leur_contenu() {
    let (etat, _) = etat(false, ModeData::Normal, false).await;
    let (statut, v, _) = corps(youtube_moods(State(etat.clone())).await).await;
    assert_eq!(statut, StatusCode::OK);
    // Un TABLEAU, la forme que lisent le web et Flutter.
    let groupes = v.as_array().expect("tableau");
    assert_eq!(groupes[0]["title"], "Moods & moments");
    assert_eq!(groupes[0]["items"][1]["title"], "Chill");
    let params = groupes[0]["items"][1]["params"]
        .as_str()
        .unwrap()
        .to_string();

    let (statut, v, texte) = corps(youtube_mood(State(etat), Path(params)).await).await;
    assert_eq!(statut, StatusCode::OK, "{texte}");
    let p = &v["sections"][0]["items"][0];
    assert_eq!(p["kind"], "playlist");
    assert_eq!(p["title"], "Coffee Shop Blend");
}

/// L'ouverture d'une playlist de rayon mène à sa liste de titres, avec son
/// vrai titre (l'en-tête avait migré ; il valait « Unknown »).
#[tokio::test]
async fn une_playlist_de_rayon_s_ouvre_sur_ses_titres() {
    let (etat, _) = etat(false, ModeData::Normal, false).await;
    let id = "VLRDCLAK5uy_nBE4bLuBHUXWZrF59ZrkPEToKt8M_I3Vc".to_string();
    let (statut, v, texte) =
        corps(service_playlist(State(etat.clone()), Path(("youtube".into(), id.clone()))).await)
            .await;
    assert_eq!(statut, StatusCode::OK, "{texte}");
    assert_eq!(v["name"], "Coffee Shop Blend");
    assert!(v["cover_path"].as_str().unwrap().starts_with("https://"));

    let (statut, v, _) =
        corps(service_playlist_tracks(State(etat), Path(("youtube".into(), id))).await).await;
    assert_eq!(statut, StatusCode::OK);
    let pistes = v.as_array().expect("tableau de titres");
    assert_eq!(pistes.len(), 4);
    assert!(
        pistes
            .iter()
            .all(|p| !p["source_id"].as_str().unwrap().is_empty())
    );
    assert!(
        pistes
            .iter()
            .all(|p| !p["title"].as_str().unwrap().is_empty())
    );
}

/// Une réponse 200 que Tune ne sait plus lire est une ERREUR, pas une liste
/// vide : c'est exactement le mensonge des anciens talons.
#[tokio::test]
async fn une_reponse_illisible_rend_une_erreur_franche() {
    let (etat, _) = etat(true, ModeData::Normal, false).await;
    let (statut, _, texte) = corps(youtube_home(State(etat.clone())).await).await;
    assert_eq!(statut, StatusCode::BAD_GATEWAY);
    assert!(texte.contains("nothing recognised"), "{texte}");
    let (statut, _, _) = corps(youtube_moods(State(etat)).await).await;
    assert_eq!(statut, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn bibliotheque_sans_compte_le_dit() {
    let (etat, vus) = etat(false, ModeData::Normal, false).await;
    let (statut, _, texte) = corps(youtube_library(State(etat.clone())).await).await;
    assert_eq!(statut, StatusCode::BAD_GATEWAY);
    assert!(texte.contains("no YouTube account connected"), "{texte}");
    let (statut, _, _) = corps(service_playlists(State(etat), Path("youtube".into())).await).await;
    assert_eq!(statut, StatusCode::BAD_GATEWAY);
    assert!(vus.lock().unwrap().is_empty(), "aucun appel sans jeton");
}

#[tokio::test]
async fn bibliotheque_du_compte_playlists_et_titres_aimes() {
    let (etat, vus) = etat(false, ModeData::Normal, true).await;
    let (statut, v, texte) = corps(youtube_library(State(etat)).await).await;
    assert_eq!(statut, StatusCode::OK, "{texte}");
    let playlists = v["playlists"].as_array().unwrap();
    assert_eq!(playlists.len(), 2, "les deux pages sont lues");
    assert_eq!(playlists[0]["source_id"], "PLun");
    assert_eq!(playlists[0]["name"], "Ma playlist");
    assert_eq!(playlists[0]["track_count"], 12);
    let titres = v["tracks"].as_array().unwrap();
    assert_eq!(titres.len(), 1, "seule la catégorie Musique : {titres:?}");
    assert_eq!(titres[0]["source_id"], "musique1");
    assert_eq!(titres[0]["artist_name"], "Artiste");
    assert_eq!(titres[0]["duration_ms"], 185_000);
    let vus = vus.lock().unwrap();
    assert!(
        vus.iter()
            .any(|r| r.starts_with("playlists?") && r.contains("pageToken=p2"))
    );
}

#[tokio::test]
async fn bibliotheque_refusee_par_google_rend_le_motif() {
    let (etat, _) = etat(false, ModeData::AccesNonConfigure, true).await;
    let (statut, _, texte) = corps(youtube_library(State(etat.clone())).await).await;
    assert_eq!(statut, StatusCode::BAD_GATEWAY);
    assert!(texte.contains("403 accessNotConfigured"), "{texte}");
    let (statut, _, texte) =
        corps(service_playlists(State(etat), Path("youtube".into())).await).await;
    assert_eq!(statut, StatusCode::BAD_GATEWAY);
    assert!(texte.contains("accessNotConfigured"), "{texte}");
}
