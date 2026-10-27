//! #6967 — flux Opus (repli AAC) et recherche paginée de YouTube Music.
//!
//! Aucun réseau dans les témoins : un serveur local tient le rôle d'InnerTube
//! (`/search`, `/player`) et rejoue des réponses RÉELLES, capturées le
//! 27/10/2026 sur `music.youtube.com` (recherche « Daft Punk », client
//! `WEB_REMIX`, corps identiques à ceux de Tune), dans `tests/fixtures/youtube/` :
//! - `recherche_titres_p1.json` : la recherche filtrée `songs`, 20 titres et
//!   une suite `nextContinuationData` ;
//! - `recherche_titres_p2.json`, `recherche_titres_p3.json` : ses deux suites,
//!   jeton rendu dans l'URL (`&ctoken=…&continuation=…&type=next`), 20 titres
//!   chacune, dont certains déjà vus (YouTube en répète d'une page à l'autre) ;
//! - `recherche_titres_p2_corps.json` : la première suite demandée par le
//!   corps (`{"context","continuation"}`) : YouTube l'accepte aussi, et la rend
//!   sous la même forme `continuationContents.musicShelfContinuation` ;
//! - `recherche_mixte.json` : la recherche sans filtre ;
//! - `lecteur_refus_android_music.json`, `lecteur_refus_tv_embarque.json` :
//!   `/player` des deux clients du chemin natif, tels que YouTube les rend
//!   aujourd'hui sans compte (`LOGIN_REQUIRED`, puis `ERROR`).
//!
//! Anonymisation : `responseContext` (dont `visitorData`), tous les
//! `trackingParams` / `clickTrackingParams` et les `menu` des titres (que Tune
//! ne lit pas) sont retirés ; les jetons de suite, opaques, deviennent
//! `SUITE-P2`, `SUITE-P3`, `SUITE-P4`. Le reste est tel que YouTube l'a rendu.
//!
//! Restent RECONSTRUITS, et dits comme tels : les `adaptiveFormats` du lecteur
//! (YouTube refuse `/player` et yt-dlp à la machine d'essai, « Sign in to
//! confirm you're not a bot »), et la forme récente `continuationItemRenderer`,
//! que YouTube ne rendait pas le 27/10/2026 mais que le code garde en défense.
//!
//! Les épreuves `preuve_reelle_6967_*`, ignorées par défaut, interrogent le
//! VRAI service :
//! `cargo test -p tune-core --lib -- --ignored --nocapture preuve_reelle_6967`
//! (`TUNE_YTDLP=/chemin/yt-dlp` pour la lecture et la version).
use std::sync::{Arc, Mutex as StdMutex};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use super::*;

/// Le 3e titre réel de la page 1 : le faux lecteur ne lui donne que l'AAC.
const AAC_SEUL: &str = "uSFXOBaqiR8";

fn capture(nom: &str) -> Value {
    let chemin = format!(
        "{}/tests/fixtures/youtube/{nom}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(&chemin).expect(&chemin)).expect("json")
}

/// Les éléments d'une réponse filtrée réelle : le rayon de la première page,
/// ou `musicShelfContinuation` d'une suite.
fn elements_de(reponse: &Value) -> Vec<Value> {
    let suite = &reponse["continuationContents"]["musicShelfContinuation"]["contents"];
    if let Some(e) = suite.as_array() {
        return e.clone();
    }
    reponse["contents"]["tabbedSearchResultsRenderer"]["tabs"][0]["tabRenderer"]["content"]
        ["sectionListRenderer"]["contents"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|s| s["musicShelfRenderer"]["contents"].as_array())
        .expect("le rayon de titres")
        .clone()
}

/// Les `videoId` attendus, lus directement dans les captures (pas par le code
/// essayé), dans l'ordre de YouTube et sans doublon.
fn ids_attendus(captures: &[&str]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for nom in captures {
        for e in elements_de(&capture(nom)) {
            let id = e["musicResponsiveListItemRenderer"]["playlistItemData"]["videoId"]
                .as_str()
                .expect("videoId")
                .to_string();
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}

/// Forme récente, reconstruite : les titres réels, puis l'élément de suite.
fn avec_element_de_suite(mut elements: Vec<Value>, jeton: Option<&str>) -> Vec<Value> {
    if let Some(j) = jeton {
        elements.push(json!({"continuationItemRenderer": {"continuationEndpoint":
            {"continuationCommand": {"token": j, "request": "CONTINUATION_REQUEST_TYPE_SEARCH"}}}}));
    }
    elements
}

/// La première page réelle, remise dans la forme récente.
fn premiere_page_recente() -> Value {
    let elements = avec_element_de_suite(
        elements_de(&capture("recherche_titres_p1")),
        Some("SUITE-P2"),
    );
    json!({"contents": {"tabbedSearchResultsRenderer": {"tabs": [{"tabRenderer": {"content":
    {"sectionListRenderer": {"contents": [{"musicShelfRenderer": {"contents": elements}}]}}}}]}}})
}

/// Une suite réelle, remise dans la forme récente
/// (`onResponseReceivedActions` → `appendContinuationItemsAction`).
fn suite_recente(nom: &str, jeton: Option<&str>) -> Value {
    json!({"onResponseReceivedActions": [{"appendContinuationItemsAction": {
        "continuationItems": avec_element_de_suite(elements_de(&capture(nom)), jeton),
    }}]})
}

/// `adaptiveFormats` RECONSTRUITS sur le modèle d'InnerTube : vidéo 137,
/// AAC 140, Opus 249/250/251. `aac_seul` : la vidéo n'offre pas d'Opus.
fn reponse_lecteur(video_id: &str, aac_seul: bool) -> Value {
    let url = |itag: u32| format!("https://cdn.invalide/videoplayback?id={video_id}&itag={itag}");
    let mut formats = vec![
        json!({"itag": 137, "mimeType": "video/mp4; codecs=\"avc1.640028\"", "bitrate": 4_000_000, "url": url(137)}),
        json!({"itag": 140, "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"", "bitrate": 130_000, "averageBitrate": 129_502, "url": format!("{}&mime=audio%2Fmp4", url(140))}),
    ];
    if !aac_seul {
        formats.push(json!({"itag": 249, "mimeType": "audio/webm; codecs=\"opus\"", "bitrate": 56_000, "averageBitrate": 51_000, "url": format!("{}&mime=audio%2Fwebm", url(249))}));
        formats.push(json!({"itag": 251, "mimeType": "audio/webm; codecs=\"opus\"", "bitrate": 140_000, "averageBitrate": 128_810, "url": format!("{}&mime=audio%2Fwebm", url(251))}));
        formats.push(json!({"itag": 250, "mimeType": "audio/webm; codecs=\"opus\"", "bitrate": 72_000, "averageBitrate": 66_000, "url": format!("{}&mime=audio%2Fwebm", url(250))}));
    }
    json!({
        "playabilityStatus": {"status": "OK"},
        "streamingData": {"adaptiveFormats": formats},
    })
}

/// Ce que joue le faux InnerTube.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Forme {
    /// Les captures telles quelles : `nextContinuationData`, jeton dans l'URL.
    Reelle,
    /// Les titres réels dans l'enveloppe récente : jeton dans le corps.
    Recente,
    /// Captures réelles, mais le jeton dans l'URL est refusé (400) : seul le
    /// corps passe, et rend la capture `recherche_titres_p2_corps`.
    UrlRefusee,
    /// La recherche filtrée échoue (500) : reste la recherche sans filtre.
    SansFiltre,
    /// `/player` rend les refus réels du 27/10/2026.
    LecteurRefuse,
}

#[derive(Clone)]
struct Faux {
    forme: Forme,
    /// Chaque appel, lu tel quel : `search:songs`, `search:mixte`,
    /// `search:url:SUITE-P2`, `search:corps:SUITE-P2`, `player:<client>:<id>`.
    appels: Arc<StdMutex<Vec<String>>>,
}

async fn faux_search(
    State(f): State<Faux>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    Json(corps): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    if let Some(jeton) = q.get("ctoken") {
        f.appels.lock().unwrap().push(format!("search:url:{jeton}"));
        if f.forme == Forme::UrlRefusee {
            return Err(StatusCode::BAD_REQUEST);
        }
        return match jeton.as_str() {
            "SUITE-P2" => Ok(Json(capture("recherche_titres_p2"))),
            "SUITE-P3" => Ok(Json(capture("recherche_titres_p3"))),
            // Au-delà de la capture : le faux service s'arrête là.
            _ => Err(StatusCode::NOT_FOUND),
        };
    }
    if let Some(jeton) = corps["continuation"].as_str() {
        f.appels
            .lock()
            .unwrap()
            .push(format!("search:corps:{jeton}"));
        assert!(
            corps["query"].is_null() && corps["context"].is_object(),
            "suite dans le corps : le contexte et le jeton seuls, {corps}"
        );
        return match (f.forme, jeton) {
            (Forme::Recente, "SUITE-P2") => {
                Ok(Json(suite_recente("recherche_titres_p2", Some("SUITE-P3"))))
            }
            (Forme::Recente, "SUITE-P3") => Ok(Json(suite_recente("recherche_titres_p3", None))),
            (_, "SUITE-P2") => Ok(Json(capture("recherche_titres_p2_corps"))),
            (_, "SUITE-P3") => Ok(Json(capture("recherche_titres_p3"))),
            _ => Err(StatusCode::NOT_FOUND),
        };
    }
    if corps["params"].is_string() {
        f.appels.lock().unwrap().push("search:songs".into());
        match f.forme {
            Forme::SansFiltre => Err(StatusCode::INTERNAL_SERVER_ERROR),
            Forme::Recente => Ok(Json(premiere_page_recente())),
            _ => Ok(Json(capture("recherche_titres_p1"))),
        }
    } else {
        f.appels.lock().unwrap().push("search:mixte".into());
        Ok(Json(capture("recherche_mixte")))
    }
}

async fn faux_player(State(f): State<Faux>, Json(corps): Json<Value>) -> Json<Value> {
    let id = corps["videoId"].as_str().unwrap_or("").to_string();
    let client = corps["context"]["client"]["clientName"]
        .as_str()
        .unwrap_or("")
        .to_string();
    f.appels
        .lock()
        .unwrap()
        .push(format!("player:{client}:{id}"));
    if f.forme == Forme::LecteurRefuse {
        return Json(match client.as_str() {
            "ANDROID_MUSIC" => capture("lecteur_refus_android_music"),
            _ => capture("lecteur_refus_tv_embarque"),
        });
    }
    Json(reponse_lecteur(&id, id == AAC_SEUL))
}

async fn service(forme: Forme) -> (YouTubeService, Arc<StdMutex<Vec<String>>>) {
    let appels = Arc::new(StdMutex::new(Vec::new()));
    let faux = Faux {
        forme,
        appels: appels.clone(),
    };
    let app = Router::new()
        .route("/search", post(faux_search))
        .route("/player", post(faux_player))
        .with_state(faux);
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(ecoute, app).await });
    let base = format!("http://{adresse}");
    (YouTubeService::avec_bases(&base, &base), appels)
}

fn ids(pistes: &[StreamTrack]) -> Vec<String> {
    pistes.iter().map(|p| p.id.clone()).collect()
}

// ── Choix du flux ──────────────────────────────────────────────────────

#[test]
fn le_selecteur_de_yt_dlp_demande_l_opus_avant_l_aac() {
    let choix: Vec<&str> = SELECTEUR_FORMAT_YTDLP.split('/').collect();
    assert_eq!(
        choix[0], "bestaudio[acodec=opus][protocol=https]",
        "Opus progressif d'abord : {SELECTEUR_FORMAT_YTDLP}"
    );
    let aac = choix
        .iter()
        .position(|c| c.contains("m4a"))
        .expect("l'AAC reste en repli");
    assert!(aac > 0);
}

#[test]
fn la_sortie_de_yt_dlp_dit_le_format_retenu() {
    let opus = analyser_sortie_ytdlp(
        "https://rr1---sn-x.googlevideo.com/videoplayback?itag=251&mime=audio%2Fwebm\n\
         {\"User-Agent\": \"Mozilla/5.0\"}\n\
         tune-format=251,opus,129.476\n",
    )
    .expect("une URL");
    assert!(opus.url.starts_with("https://rr1---sn-x.googlevideo.com/"));
    assert_eq!(opus.headers.len(), 1);
    let f = opus.format.as_ref().expect("le format retenu");
    assert_eq!(
        (f.id.as_str(), f.acodec.as_str(), f.kbps),
        ("251", "opus", Some(129))
    );
    assert_eq!(
        youtube_mime_codec(&opus.url, opus.format.as_ref()).1,
        "opus"
    );
    assert_eq!(
        qualite_youtube("opus", opus.format.as_ref()).bitrate,
        Some(129_000)
    );

    // Repli AAC : le format retenu fait foi, même sur une URL sans `mime=`.
    let aac = analyser_sortie_ytdlp("https://cdn.invalide/x\ntune-format=140,mp4a.40.2,NA\n")
        .expect("une URL");
    assert_eq!(
        aac.url, "https://cdn.invalide/x",
        "la ligne de format n'est pas l'URL"
    );
    let f = aac.format.as_ref().expect("le format retenu");
    assert_eq!(f.kbps, None, "`NA` : débit inconnu");
    assert_eq!(
        youtube_mime_codec(&aac.url, aac.format.as_ref()),
        ("audio/mp4", "m4a")
    );
    assert_eq!(
        qualite_youtube("m4a", aac.format.as_ref()).bitrate,
        Some(128_000)
    );
}

#[test]
fn le_lecteur_natif_prend_l_opus_et_se_replie_sur_l_aac() {
    let avec_opus = reponse_lecteur("a", false);
    let (url, f) = choisir_format_audio(
        avec_opus["streamingData"]["adaptiveFormats"]
            .as_array()
            .unwrap(),
    )
    .expect("un flux audio");
    assert_eq!(
        (f.id.as_str(), f.acodec.as_str()),
        ("251", "opus"),
        "Opus au plus haut débit, pas l'AAC 140 : {url}"
    );
    assert_eq!(youtube_mime_codec(&url, Some(&f)).1, "opus");

    let aac_seul = reponse_lecteur("b", true);
    let (url, f) = choisir_format_audio(
        aac_seul["streamingData"]["adaptiveFormats"]
            .as_array()
            .unwrap(),
    )
    .expect("le repli AAC");
    assert_eq!((f.id.as_str(), f.acodec.as_str()), ("140", "mp4a.40.2"));
    assert_eq!(youtube_mime_codec(&url, Some(&f)).1, "m4a");

    // Un format chiffré (sans `url`) n'est pas jouable ici.
    let chiffre =
        [json!({"itag": 251, "mimeType": "audio/webm; codecs=\"opus\"", "signatureCipher": "s=…"})];
    assert!(choisir_format_audio(&chiffre).is_none());
}

#[test]
fn le_lecteur_natif_ecarte_l_opus_segmente_et_prefere_sans_drc() {
    // Opus servi seulement par segments (OTF) : écarté, comme
    // `[protocol=https]` le fait côté yt-dlp ; l'AAC progressif joue.
    let segmente = [
        json!({"itag": 251, "type": "FORMAT_STREAM_TYPE_OTF", "mimeType": "audio/webm; codecs=\"opus\"", "bitrate": 140_000, "url": "https://cdn.invalide/otf-251"}),
        json!({"itag": 140, "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"", "bitrate": 130_000, "url": "https://cdn.invalide/140"}),
    ];
    let (url, f) = choisir_format_audio(&segmente).expect("le repli AAC");
    assert_eq!(f.id, "140", "un Opus segmenté ne doit pas passer : {url}");

    // Deux 251, l'un à dynamique compressée (DRC) et de débit plus haut :
    // le 251 d'origine passe devant.
    let drc = [
        json!({"itag": 251, "isDrc": true, "mimeType": "audio/webm; codecs=\"opus\"", "bitrate": 150_000, "url": "https://cdn.invalide/251-drc"}),
        json!({"itag": 251, "mimeType": "audio/webm; codecs=\"opus\"", "bitrate": 140_000, "url": "https://cdn.invalide/251"}),
    ];
    let (url, _) = choisir_format_audio(&drc).expect("un Opus");
    assert_eq!(
        url, "https://cdn.invalide/251",
        "la version DRC a été prise"
    );
}

#[tokio::test]
async fn le_lecteur_natif_refuse_par_youtube_rend_la_main_a_yt_dlp() {
    // Réponses réelles du 27/10/2026 : ANDROID_MUSIC `LOGIN_REQUIRED`, puis le
    // client TV embarqué `ERROR`. Le chemin natif doit essayer les deux et
    // échouer en le disant, pour que `extract_audio_url` passe à yt-dlp.
    let (yt, appels) = service(Forme::LecteurRefuse).await;
    let erreur = yt
        .extract_audio_url_native(AAC_SEUL)
        .await
        .expect_err("aucun flux ne doit sortir d'un refus");
    assert!(
        erreur.contains("YouTube is no longer supported"),
        "la raison réelle du dernier refus : {erreur}"
    );
    assert_eq!(
        appels.lock().unwrap().clone(),
        [
            format!("player:ANDROID_MUSIC:{AAC_SEUL}"),
            format!("player:TVHTML5_SIMPLY_EMBEDDED_PLAYER:{AAC_SEUL}"),
        ]
    );
}

// ── Recherche paginée ──────────────────────────────────────────────────

#[tokio::test]
async fn la_recherche_rend_plus_d_une_page() {
    let (yt, appels) = service(Forme::Reelle).await;
    let attendus = ids_attendus(&[
        "recherche_titres_p1",
        "recherche_titres_p2",
        "recherche_titres_p3",
    ]);
    assert_eq!(attendus.len(), 46, "60 titres réels, dont 14 répétés");

    let p1 = yt.search_page("Daft Punk", 30, 0).await.expect("page 1");
    assert_eq!(
        ids(&p1.results.tracks),
        attendus[..30],
        "plus que les 20 titres de la première réponse"
    );
    assert_eq!(
        p1.results.tracks[0].title,
        "Instant Crush (feat. Julian Casablancas)"
    );
    assert!(
        p1.results.tracks[0].artist.starts_with("Daft Punk"),
        "artiste lu dans l'élément réel : {}",
        p1.results.tracks[0].artist
    );
    assert!(p1.has_more, "46 titres lus : il en reste après 30");

    let p2 = yt.search_page("Daft Punk", 30, 30).await.expect("page 2");
    assert_eq!(
        ids(&p2.results.tracks),
        attendus[30..],
        "la page 2 reprend où la 1 s'arrête, sans doublon"
    );
    assert!(!p2.has_more, "au-delà de la capture, rien");
    assert!(
        p2.results.albums.is_empty(),
        "les albums ne se répètent pas"
    );

    // Le chemin non paginé profite aussi des titres filtrés.
    assert_eq!(yt.search("Daft Punk", 100).await.unwrap().tracks.len(), 46);

    let appels = appels.lock().unwrap().clone();
    for attendu in ["search:url:SUITE-P2", "search:url:SUITE-P3"] {
        assert!(
            appels.iter().any(|a| a == attendu),
            "{attendu} : {appels:?}"
        );
    }
    assert!(
        !appels.iter().any(|a| a == "search:corps:SUITE-P2"),
        "la forme réelle passe du premier coup : {appels:?}"
    );
}

#[tokio::test]
async fn la_page_suivante_continue_le_parcours_sans_repartir_de_zero() {
    // Sur le vrai service (27/10/2026, « Daft Punk »), chaque nouvelle
    // recherche rend un ordre différent : une page 2 qui relance la recherche
    // depuis le début répétait des titres de la page 1. La page 2 doit
    // reprendre le parcours de la page 1, sans redemander la première réponse.
    let (yt, appels) = service(Forme::Reelle).await;
    let p1 = yt.search_page("Daft Punk", 30, 0).await.expect("page 1");
    let p2 = yt.search_page("Daft Punk", 30, 30).await.expect("page 2");
    let appels = appels.lock().unwrap().clone();
    assert_eq!(
        appels.iter().filter(|a| *a == "search:songs").count(),
        1,
        "la recherche est repartie de zéro pour la page 2 : {appels:?}"
    );
    assert_eq!(
        appels
            .iter()
            .filter(|a| *a == "search:url:SUITE-P2")
            .count(),
        1,
        "la première suite a été redemandée : {appels:?}"
    );
    let vus = ids(&p1.results.tracks);
    assert!(
        p2.results.tracks.iter().all(|t| !vus.contains(&t.id)),
        "la page 2 répète la page 1"
    );
}

#[tokio::test]
async fn la_suite_recente_rend_son_jeton_dans_le_corps() {
    let (yt, appels) = service(Forme::Recente).await;
    let attendus = ids_attendus(&[
        "recherche_titres_p1",
        "recherche_titres_p2",
        "recherche_titres_p3",
    ]);

    let p1 = yt.search_page("Daft Punk", 30, 0).await.expect("page 1");
    assert_eq!(
        ids(&p1.results.tracks),
        attendus[..30],
        "la suite récente n'a pas été lue"
    );
    assert!(p1.has_more);
    let p2 = yt.search_page("Daft Punk", 30, 30).await.expect("page 2");
    assert_eq!(ids(&p2.results.tracks), attendus[30..]);
    assert!(!p2.has_more, "plus d'élément de suite : fin de liste");

    let appels = appels.lock().unwrap().clone();
    assert!(
        appels.iter().any(|a| a == "search:corps:SUITE-P2")
            && appels.iter().any(|a| a == "search:corps:SUITE-P3"),
        "{appels:?}"
    );
    assert!(
        !appels.iter().any(|a| a.starts_with("search:url:")),
        "jeton récent envoyé dans l'URL : {appels:?}"
    );
}

#[tokio::test]
async fn une_suite_refusee_dans_l_url_se_redemande_dans_le_corps() {
    let (yt, appels) = service(Forme::UrlRefusee).await;
    let attendus = ids_attendus(&["recherche_titres_p1", "recherche_titres_p2_corps"]);

    let p1 = yt.search_page("Daft Punk", 30, 0).await.expect("page 1");
    assert_eq!(
        ids(&p1.results.tracks),
        attendus[..30],
        "la suite s'est arrêtée au premier refus"
    );
    assert!(p1.has_more);

    let appels = appels.lock().unwrap().clone();
    let url = appels.iter().position(|a| a == "search:url:SUITE-P2");
    let corps = appels.iter().position(|a| a == "search:corps:SUITE-P2");
    assert!(
        matches!((url, corps), (Some(u), Some(c)) if u < c),
        "l'URL d'abord, puis le corps : {appels:?}"
    );
}

#[tokio::test]
async fn sans_titres_filtres_la_premiere_page_garde_la_recherche_sans_filtre() {
    let (yt, _) = service(Forme::SansFiltre).await;
    let p1 = yt.search_page("Daft Punk", 30, 0).await.expect("page 1");
    let seule = yt.parse_search_results(&capture("recherche_mixte")).tracks;
    assert_eq!(
        ids(&p1.results.tracks),
        ids(&seule),
        "les titres de la recherche sans filtre, comme avant #6967"
    );
    assert!(!p1.has_more);
}

// ── Passer d'un titre à l'autre depuis une recherche ───────────────────

#[tokio::test]
async fn passer_d_un_titre_a_l_autre_depuis_la_recherche() {
    let (yt, appels) = service(Forme::Reelle).await;
    let page = yt.search_page("Daft Punk", 10, 0).await.expect("recherche");
    let pistes = &page.results.tracks;
    assert_eq!(pistes[2].id, AAC_SEUL, "le 3e titre réel de la capture");

    // Lecture du 1er titre, puis suivant, puis suivant (AAC seul), puis
    // retour au 1er : chaque titre a SON flux, le bon codec, aucune erreur.
    let mut vus = Vec::new();
    for piste in [&pistes[0], &pistes[1], &pistes[2], &pistes[0]] {
        let flux = yt
            .get_track_url(&piste.id, None)
            .await
            .unwrap_or_else(|e| panic!("{} : {e}", piste.id));
        assert!(
            flux.url.contains(&format!("id={}&", piste.id)),
            "{} a reçu le flux d'un autre titre : {}",
            piste.id,
            flux.url
        );
        vus.push((
            piste.id.clone(),
            flux.quality.codec.clone(),
            flux.mime_type.clone(),
        ));
    }
    let (un, deux) = (pistes[0].id.clone(), pistes[1].id.clone());
    assert_eq!(
        vus,
        [
            (un.clone(), "opus".into(), "audio/webm".into()),
            (deux, "opus".into(), "audio/webm".into()),
            (AAC_SEUL.into(), "m4a".into(), "audio/mp4".into()),
            (un.clone(), "opus".into(), "audio/webm".into()),
        ] as [(String, String, String); 4]
    );
    // Le retour au 1er titre vient du cache : un seul `/player` pour lui.
    let appels = appels.lock().unwrap().clone();
    assert_eq!(
        appels
            .iter()
            .filter(|a| a.starts_with("player:") && a.ends_with(&format!(":{un}")))
            .count(),
        1,
        "{appels:?}"
    );
}

// ── Épreuves sur le VRAI service (ignorées par défaut) ─────────────────

/// Trois recherches réelles, deux pages chacune.
#[tokio::test]
#[ignore = "réseau : interroge le vrai YouTube Music"]
async fn preuve_reelle_6967_recherche_paginee() {
    let yt = YouTubeService::new();
    for q in ["Daft Punk", "Miles Davis", "Laufey"] {
        let p1 = yt
            .search_page(q, 30, 0)
            .await
            .unwrap_or_else(|e| panic!("« {q} » page 1 : {e}"));
        let p2 = yt
            .search_page(q, 30, 30)
            .await
            .unwrap_or_else(|e| panic!("« {q} » page 2 : {e}"));
        println!(
            "« {q} » page 1 : {} titres, encore : {} ; page 2 : {} titres, encore : {} ; albums {}, artistes {}, playlists {}",
            p1.results.tracks.len(),
            p1.has_more,
            p2.results.tracks.len(),
            p2.has_more,
            p1.results.albums.len(),
            p1.results.artists.len(),
            p1.results.playlists.len(),
        );
        for t in p1
            .results
            .tracks
            .iter()
            .take(3)
            .chain(p2.results.tracks.iter().take(2))
        {
            println!("    {} | {} | {}", t.id, t.title, t.artist);
        }
        assert_eq!(p1.results.tracks.len(), 30, "« {q} » : page 1 incomplète");
        assert!(p1.has_more, "« {q} » : la suite n'est pas annoncée");
        assert!(!p2.results.tracks.is_empty(), "« {q} » : page 2 vide");
        let vus = ids(&p1.results.tracks);
        assert!(
            p2.results.tracks.iter().all(|t| !vus.contains(&t.id)),
            "« {q} » : la page 2 répète la page 1"
        );
    }
}

/// Trois titres réels : format retenu, puis les 4 premiers kio du flux.
#[tokio::test]
#[ignore = "réseau : vrai YouTube, et yt-dlp désigné par TUNE_YTDLP"]
async fn preuve_reelle_6967_lecture() {
    if let Ok(chemin) = std::env::var("TUNE_YTDLP") {
        crate::ytdlp::set_binary(chemin.into());
    }
    let yt = YouTubeService::new();
    let page = yt.search_page("Daft Punk", 5, 0).await.expect("recherche");
    let client = crate::http::client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    let mut echecs = Vec::new();
    for piste in page.results.tracks.iter().take(3) {
        let flux = match yt.get_track_url(&piste.id, None).await {
            Ok(flux) => flux,
            Err(e) => {
                println!("{} ({}) : {e}", piste.id, piste.title);
                echecs.push(piste.id.clone());
                continue;
            }
        };
        let mut req = client.get(&flux.url).header("Range", "bytes=0-4095");
        for (nom, valeur) in &flux.headers {
            req = req.header(nom.as_str(), valeur.as_str());
        }
        let reponse = req.send().await.expect("flux");
        let statut = reponse.status().as_u16();
        let type_ = reponse
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let octets = reponse.bytes().await.unwrap_or_default();
        let entete: Vec<String> = octets.iter().take(8).map(|o| format!("{o:02x}")).collect();
        println!(
            "{} ({}) : codec {}, {} kbit/s, HTTP {statut}, {type_}, {} octets, début {}",
            piste.id,
            piste.title,
            flux.quality.codec,
            flux.quality.bitrate.unwrap_or(0) / 1000,
            octets.len(),
            entete.join(" "),
        );
        assert!(matches!(statut, 200 | 206), "{} : HTTP {statut}", piste.id);
        match flux.quality.codec.as_str() {
            "opus" => assert!(
                octets.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]),
                "WebM attendu"
            ),
            "m4a" => assert_eq!(octets.get(4..8), Some(&b"ftyp"[..]), "MP4 attendu"),
            autre => panic!("{} : codec inattendu {autre}", piste.id),
        }
    }
    assert!(echecs.is_empty(), "titres non résolus : {echecs:?}");
}

/// La version installée de yt-dlp, comparée à la dernière publiée.
#[tokio::test]
#[ignore = "réseau : lit la dernière publication de yt-dlp sur GitHub"]
async fn preuve_reelle_6967_version_ytdlp() {
    let installee = match std::env::var("TUNE_YTDLP") {
        Ok(chemin) => crate::ytdlp::version_of(std::path::Path::new(&chemin)).await,
        Err(_) => None,
    };
    let publiee = crate::ytdlp::derniere_version_publiee().await;
    let etat = crate::ytdlp::etat_de_version(
        installee.as_deref(),
        publiee.as_deref().map_err(|e| e.as_str()),
    );
    println!("{etat:?}");
    assert!(
        etat.publiee.is_some(),
        "dernière version illisible : {etat:?}"
    );
}
