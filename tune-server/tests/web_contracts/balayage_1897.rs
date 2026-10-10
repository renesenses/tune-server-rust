//! Balayage de TOUTES les lectures cartographiées (#1897).
//!
//! `VAGUE_INITIALE` et les témoins voisins choisissent leurs routes à la main :
//! une route qui entre dans `docs/contrat-web.json` sans que personne ne l'y
//! ajoute n'est jamais jouée, et sa réponse peut dériver en silence. Ce
//! balayage part de la CARTE, pas d'une liste : chaque `GET` qu'elle décrit est
//! appelé sur le vrai routeur, avec une bibliothèque et une zone témoins, et sa
//! réponse est confrontée aux champs que le client lit.
//!
//! Trois issues seulement, et une seule est tolérée en silence :
//! - **200 + champs présents** : conforme ;
//! - **200 + champ absent / mauvaise forme** : divergence, ROUGE, sauf entrée
//!   nommée dans `DIVERGENCES_TOLEREES` avec sa cause ;
//! - **autre statut, délai, liste vide** : la route n'a rien prouvé. C'est
//!   compté et affiché, jamais compté comme conforme. Le balayage exige un
//!   plancher de routes réellement prouvées pour qu'un routeur qui répondrait
//!   404 partout ne passe pas au vert.
use super::{CARTE_WEB, CarteContrats, respecte_contrat};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;
use tower::ServiceExt;
use tune_core::db::{
    album_repo::AlbumRepo,
    artist_repo::ArtistRepo,
    models::{Album, Artist, Track},
    track_repo::TrackRepo,
    zone_repo::ZoneRepo,
};

/// Divergences connues, NOMMÉES : (méthode, route de la carte, cause).
///
/// Toutes sont des déclarations FAUSSES du client, corrigées côté web : la
/// carte vit ici mais décrit le client, elle ne change qu'à sa régénération.
/// Une entrée dont la route redevient conforme ou quitte la carte est
/// signalée (`::warning::`) pour être retirée — sans rougir : la régénération
/// de la carte au gel d'une release ne doit pas être bloquée par une dette
/// PAYÉE.
///
/// Vide depuis la carte régénérée au client web de #5247 (qui contient
/// web#2007) : les sept dettes nommées jusque-là y étaient toutes payées.
const DIVERGENCES_TOLEREES: &[(&str, &str, &str)] = &[];

/// Plancher de routes réellement prouvées (200 + champs vérifiés). Mesuré à
/// l'écriture du balayage (75), puis à 81 avec la carte régénérée au client
/// web de #5247 : les quatre lectures de la découverte YouTube Music (accueil,
/// tendances, ambiances, contenu d'une ambiance) y sont prouvées contre le
/// faux YouTube. Il ne doit que monter.
const PLANCHER_PROUVEES: usize = 81;

/// Chaînes de requête sans lesquelles la route refuse (400) et ne prouve rien.
const REQUETES: &[(&str, &str)] = &[("/library/search", "q=balayage")];

struct Temoins {
    zone: i64,
    artiste: i64,
    album: i64,
    piste: i64,
}

/// Le `params` d'une ambiance que le faux YouTube connaît (« Chill », tiré de
/// la réponse enregistrée `moods.json`).
const AMBIANCE_TEMOIN: &str = "ggMPOg1uX1JOQWZFeDByc2Jm";

/// Faux YouTube Music LOCAL pour le balayage (#5247).
///
/// Les routes de découverte YouTube ne sont plus des talons : elles appellent
/// InnerTube. Le balayage joue CHAQUE lecture de la carte ; sans ce faux, il
/// partirait sur le réseau depuis la CI. Le faux rejoue les réponses réelles
/// enregistrées (`tune-core/tests/fixtures/youtube/`), si bien que les routes
/// sont PROUVÉES ici au lieu d'être comptées « non prouvées ».
async fn faux_youtube() -> String {
    use axum::{Json, routing::post};
    async fn browse(Json(corps): Json<Value>) -> axum::response::Response {
        use axum::response::IntoResponse;
        let nom = match corps["browseId"].as_str().unwrap_or("") {
            "FEmusic_home" => "home",
            "FEmusic_charts" => "charts_fr",
            "FEmusic_moods_and_genres" => "moods",
            "FEmusic_moods_and_genres_category" if corps["params"] == AMBIANCE_TEMOIN => {
                "mood_category"
            }
            _ => return StatusCode::BAD_REQUEST.into_response(),
        };
        let chemin = format!(
            "{}/../tune-core/tests/fixtures/youtube/{nom}.json",
            env!("CARGO_MANIFEST_DIR")
        );
        let texte = std::fs::read_to_string(&chemin).expect("fixture youtube");
        Json(serde_json::from_str::<Value>(&texte).expect("fixture json")).into_response()
    }
    let app = axum::Router::new().route("/youtubei/v1/browse", post(browse));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port local");
    let adresse = ecoute.local_addr().expect("adresse locale");
    tokio::spawn(async move { axum::serve(ecoute, app).await.expect("faux youtube") });
    format!("http://{adresse}")
}

async fn amorcer() -> (axum::Router, Temoins) {
    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("etat serveur isole");
    let faux = faux_youtube().await;
    etat.services.lock().await.register(Box::new(
        tune_core::streaming::youtube::YouTubeService::avec_bases(
            &format!("{faux}/youtubei/v1"),
            // L'API Data n'a pas de faux : rien ici ne doit l'appeler.
            "http://127.0.0.1:9/youtube/v3",
        ),
    ));
    let artistes = ArtistRepo::with_backend(etat.backend.clone());
    let albums = AlbumRepo::with_backend(etat.backend.clone());
    let pistes = TrackRepo::with_backend(etat.backend.clone());
    let zones = ZoneRepo::with_backend(etat.backend.clone());
    let artiste = artistes
        .create(&Artist::new("Artiste du balayage".into()))
        .expect("artiste temoin");
    let mut album = Album::new("Album du balayage".into());
    album.artist_id = Some(artiste);
    album.source = "local".into();
    let album = albums.create(&album).expect("album temoin");
    let mut piste = Track::new("Piste du balayage".into());
    piste.artist_id = Some(artiste);
    piste.album_id = Some(album);
    piste.file_path = Some("/fixture-balayage/album/piste.flac".into());
    piste.track_number = 1;
    piste.duration_ms = 60_000;
    piste.genre = Some("Jazz".into());
    let piste = pistes.create(&piste).expect("piste temoin");
    let zone = zones
        .create(
            "Zone du balayage",
            Some("browser"),
            Some("browser-balayage"),
        )
        .expect("zone temoin");
    (
        tune_server::routes::router(etat),
        Temoins {
            zone,
            artiste,
            album,
            piste,
        },
    )
}

/// Chemin réel d'une route de la carte, ou `None` quand un paramètre ne peut
/// pas être rempli par un témoin (la route n'est alors pas jouée — et c'est
/// compté).
fn chemin_reel(route: &str, t: &Temoins) -> Option<String> {
    let segments: Vec<&str> = route.trim_start_matches('/').split('/').collect();
    let mut sortie = Vec::with_capacity(segments.len());
    for (i, segment) in segments.iter().enumerate() {
        if *segment == "{}" {
            let id = match i.checked_sub(1).map(|p| segments[p]) {
                Some("zones") => t.zone.to_string(),
                Some("artists") => t.artiste.to_string(),
                Some("albums") => t.album.to_string(),
                Some("tracks") => t.piste.to_string(),
                Some("moods") => AMBIANCE_TEMOIN.to_string(),
                _ => return None,
            };
            sortie.push(id);
        } else if segment.contains("{}") {
            // Interpolation de chaîne de requête collée au segment.
            sortie.push(segment.replace("{}", ""));
        } else {
            sortie.push((*segment).to_string());
        }
    }
    let mut chemin = format!("/api/v1/{}", sortie.join("/"));
    if let Some((_, requete)) = REQUETES.iter().find(|(r, _)| *r == route) {
        chemin.push('?');
        chemin.push_str(requete);
    }
    Some(chemin)
}

#[derive(Debug)]
enum Issue {
    Conforme,
    Divergente(String),
    NonProuvee(String),
}

async fn jouer(app: &axum::Router, chemin: &str) -> Result<(StatusCode, Vec<u8>), String> {
    let requete = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap());
    let reponse = tokio::time::timeout(Duration::from_secs(5), requete)
        .await
        .map_err(|_| "delai depasse (5 s)".to_string())?
        .map_err(|e| format!("routeur en echec: {e}"))?;
    let statut = reponse.status();
    let octets = tokio::time::timeout(
        Duration::from_secs(5),
        axum::body::to_bytes(reponse.into_body(), usize::MAX),
    )
    .await
    .map_err(|_| "corps: delai depasse (5 s)".to_string())?
    .map_err(|e| format!("corps illisible: {e}"))?;
    Ok((statut, octets.to_vec()))
}

#[tokio::test]
async fn chaque_lecture_cartographiee_rend_les_champs_que_le_web_lit() {
    let carte: CarteContrats = serde_json::from_str(CARTE_WEB).expect("carte contrat web");
    let (app, temoins) = amorcer().await;

    let mut issues: BTreeMap<String, Issue> = BTreeMap::new();
    for contrat in carte.routes.iter().filter(|c| c.methode == "GET") {
        if contrat.champs_obligatoires.is_empty() || contrat.route.starts_with("/ext/") {
            continue;
        }
        let cle = format!("GET {}", contrat.route);
        let Some(chemin) = chemin_reel(&contrat.route, &temoins) else {
            issues.insert(cle, Issue::NonProuvee("parametre sans temoin".into()));
            continue;
        };
        let issue = match jouer(&app, &chemin).await {
            Err(e) => Issue::NonProuvee(e),
            Ok((statut, _)) if statut != StatusCode::OK => {
                Issue::NonProuvee(format!("statut {statut}"))
            }
            Ok((_, octets)) => match serde_json::from_slice::<Value>(&octets) {
                Err(e) => Issue::Divergente(format!(
                    "200 sans JSON ({e}) : {}",
                    String::from_utf8_lossy(&octets)
                        .chars()
                        .take(120)
                        .collect::<String>()
                )),
                Ok(payload) => match respecte_contrat(&payload, contrat) {
                    Ok(()) => Issue::Conforme,
                    Err(e) if e.contains("tableau vide") => Issue::NonProuvee("liste vide".into()),
                    Err(e) => Issue::Divergente(format!(
                        "{e} ; recu {}",
                        payload.to_string().chars().take(200).collect::<String>()
                    )),
                },
            },
        };
        // Plusieurs contrats peuvent viser la même route (deux fonctions du
        // client, deux types) : le pire l'emporte, et deux divergences se
        // CUMULENT — en garder une seule cachait la seconde (`/plugins` :
        // `InstalledPlugin` masquait `MergedPlugin`).
        let fusion = match (issues.remove(&cle), issue) {
            (None, nouvelle) => nouvelle,
            (Some(Issue::Divergente(a)), Issue::Divergente(b)) => {
                Issue::Divergente(format!("{a}\n    + {b}"))
            }
            (Some(Issue::Divergente(a)), _) => Issue::Divergente(a),
            (_, Issue::Divergente(b)) => Issue::Divergente(b),
            (Some(ancienne), _) => ancienne,
        };
        issues.insert(cle, fusion);
    }

    let mut prouvees = 0;
    let mut rouges = Vec::new();
    for (cle, issue) in &issues {
        match issue {
            Issue::Conforme => prouvees += 1,
            Issue::NonProuvee(motif) => eprintln!("BALAYAGE|non-prouvee|{cle}|{motif}"),
            Issue::Divergente(motif) => {
                eprintln!("BALAYAGE|divergente|{cle}|{motif}");
                let toleree = DIVERGENCES_TOLEREES
                    .iter()
                    .any(|(m, r, _)| format!("{m} {r}") == *cle);
                if !toleree {
                    rouges.push(format!("{cle} : {motif}"));
                }
            }
        }
    }
    for (m, r, cause) in DIVERGENCES_TOLEREES {
        let cle = format!("{m} {r}");
        if !matches!(issues.get(&cle), Some(Issue::Divergente(_))) {
            eprintln!(
                "::warning::{cle} n'est plus divergente ({cause}) : retirer la tolerance \
                 de DIVERGENCES_TOLEREES"
            );
        }
    }
    eprintln!(
        "BALAYAGE|bilan|{} routes, {prouvees} prouvees, {} divergentes",
        issues.len(),
        issues
            .values()
            .filter(|i| matches!(i, Issue::Divergente(_)))
            .count()
    );
    assert!(
        rouges.is_empty(),
        "reponses qui ne donnent pas au web les champs qu'il lit :\n{}",
        rouges.join("\n")
    );
    assert!(
        prouvees >= PLANCHER_PROUVEES,
        "{prouvees} routes prouvees, plancher {PLANCHER_PROUVEES} : le balayage ne prouve plus rien"
    );
}
