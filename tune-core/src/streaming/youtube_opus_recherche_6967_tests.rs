//! #6967 — flux Opus (repli AAC) et recherche paginée de YouTube Music.
//!
//! Aucun réseau : un serveur local tient le rôle d'InnerTube (`/search`,
//! `/player`). Les titres sont les éléments RÉELS de la playlist enregistrée
//! le 07/10/2026 (`tests/fixtures/youtube/playlist.json`), renumérotés pour
//! remplir plusieurs pages ; l'enveloppe des réponses de recherche (rayon,
//! jeton de suite) et les `adaptiveFormats` du lecteur sont RECONSTRUITS sur
//! la forme d'InnerTube, faute d'accès à YouTube depuis la machine d'essai.
//! Les deux formes de suite connues d'InnerTube sont jouées (`Forme`), faute
//! de savoir laquelle le vrai service rend aujourd'hui.
use std::sync::{Arc, Mutex as StdMutex};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};

use super::*;

/// Titres par réponse de recherche filtrée, comme InnerTube (une vingtaine).
const PAR_REPONSE: usize = 20;

fn elements_reels() -> Vec<Value> {
    let chemin = format!(
        "{}/tests/fixtures/youtube/playlist.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let brut: Value =
        serde_json::from_str(&std::fs::read_to_string(&chemin).expect(&chemin)).expect("json");
    brut["contents"]["twoColumnBrowseResultsRenderer"]["secondaryContents"]["sectionListRenderer"]
        ["contents"][0]["musicPlaylistShelfRenderer"]["contents"]
        .as_array()
        .expect("les titres de la playlist enregistrée")
        .clone()
}

/// L'élément réel `i % 4`, renuméroté : `vid000`, `vid001`… et « Titre N ».
fn titre(i: usize) -> Value {
    let reels = elements_reels();
    let mut e = reels[i % reels.len()].clone();
    let r = &mut e["musicResponsiveListItemRenderer"];
    let id = format!("vid{i:03}");
    r["playlistItemData"]["videoId"] = json!(id);
    r["overlay"]["musicItemThumbnailOverlayRenderer"]["content"]["musicPlayButtonRenderer"]["playNavigationEndpoint"]
        ["watchEndpoint"]["videoId"] = json!(id);
    let colonne =
        &mut r["flexColumns"][0]["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"][0];
    colonne["text"] = json!(format!("Titre {i}"));
    colonne["navigationEndpoint"]["watchEndpoint"]["videoId"] = json!(id);
    e
}

fn titres(de: usize, a: usize) -> Vec<Value> {
    (de..a).map(titre).collect()
}

fn suite(jeton: Option<String>) -> Value {
    match jeton {
        Some(j) => json!([{"nextContinuationData": {"continuation": j}}]),
        None => json!([]),
    }
}

/// Forme récente : le jeton est le dernier élément de la liste.
fn titres_et_element_de_suite(debut: usize, fin: usize, jeton: Option<String>) -> Vec<Value> {
    let mut elements = titres(debut, fin);
    if let Some(j) = jeton {
        elements.push(json!({"continuationItemRenderer": {"continuationEndpoint":
            {"continuationCommand": {"token": j, "request": "CONTINUATION_REQUEST_TYPE_SEARCH"}}}}));
    }
    elements
}

/// Première réponse filtrée `songs` : un rayon sans titre, et sa suite.
fn premiere_reponse_filtree(total: usize, forme: Forme) -> Value {
    let fin = PAR_REPONSE.min(total);
    let jeton = (fin < total).then(|| format!("suite-{fin}"));
    let rayon = if forme == Forme::Recente {
        json!({"contents": titres_et_element_de_suite(0, fin, jeton)})
    } else {
        json!({"contents": titres(0, fin), "continuations": suite(jeton)})
    };
    json!({"contents": {"tabbedSearchResultsRenderer": {"tabs": [{"tabRenderer": {"content":
    {"sectionListRenderer": {"contents": [{"musicShelfRenderer": rayon}]}}}}]}}})
}

/// Suite demandée par le corps : forme `appendContinuationItemsAction`.
fn reponse_de_suite_recente(debut: usize, total: usize) -> Value {
    let fin = (debut + PAR_REPONSE).min(total);
    let jeton = (fin < total).then(|| format!("suite-{fin}"));
    json!({"onResponseReceivedActions": [{"appendContinuationItemsAction": {
        "continuationItems": titres_et_element_de_suite(debut, fin, jeton),
    }}]})
}

fn reponse_de_suite(debut: usize, total: usize) -> Value {
    let fin = (debut + PAR_REPONSE).min(total);
    let jeton = (fin < total).then(|| format!("suite-{fin}"));
    json!({"continuationContents": {"musicShelfContinuation": {
        "contents": titres(debut, fin),
        "continuations": suite(jeton),
    }}})
}

/// Recherche sans filtre : un rayon « Songs » de trois titres (ce que la
/// recherche rendait seul avant #6967) et une carte d'album.
fn reponse_melangee() -> Value {
    json!({"contents": {"tabbedSearchResultsRenderer": {"tabs": [{"tabRenderer": {"content":
    {"sectionListRenderer": {"contents": [
        {"musicCardShelfRenderer": {
            "title": {"runs": [{"text": "Bewitched", "navigationEndpoint": {"browseEndpoint": {
                "browseId": "MPREb_album1",
                "browseEndpointContextSupportedConfigs": {"browseEndpointContextMusicConfig":
                    {"pageType": "MUSIC_PAGE_TYPE_ALBUM"}}}}}]},
            "subtitle": {"runs": [{"text": "Album • Laufey"}]},
        }},
        {"musicShelfRenderer": {
            "title": {"runs": [{"text": "Songs"}]},
            "contents": titres(0, 3),
        }},
    ]}}}}]}}})
}

/// `adaptiveFormats` sur le modèle d'InnerTube : vidéo 137, AAC 140, Opus
/// 249/250/251. `aac_seul` : la vidéo n'offre pas d'Opus.
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

/// La forme de suite que joue le faux InnerTube.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Forme {
    /// `nextContinuationData`, jeton rendu dans l'URL.
    Historique,
    /// `continuationItemRenderer`, jeton rendu dans le corps.
    Recente,
    /// Annonce `nextContinuationData` mais refuse le jeton dans l'URL (400) :
    /// seul le corps passe.
    UrlRefusee,
}

#[derive(Clone)]
struct Faux {
    total: usize,
    forme: Forme,
    /// Chaque appel, lu tel quel : `search:songs`, `search:mixte`,
    /// `search:suite-20`, `player:vid001`…
    appels: Arc<StdMutex<Vec<String>>>,
}

async fn faux_search(
    State(f): State<Faux>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    Json(corps): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    let debut = |jeton: &str| -> usize { jeton.trim_start_matches("suite-").parse().unwrap() };
    if let Some(jeton) = q.get("ctoken") {
        f.appels.lock().unwrap().push(format!("search:{jeton}"));
        if f.forme == Forme::UrlRefusee {
            return Err(StatusCode::BAD_REQUEST);
        }
        return Ok(Json(reponse_de_suite(debut(jeton), f.total)));
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
        return Ok(Json(reponse_de_suite_recente(debut(jeton), f.total)));
    }
    if corps["params"].is_string() {
        f.appels.lock().unwrap().push("search:songs".into());
        Ok(Json(premiere_reponse_filtree(f.total, f.forme)))
    } else {
        f.appels.lock().unwrap().push("search:mixte".into());
        Ok(Json(reponse_melangee()))
    }
}

async fn faux_player(State(f): State<Faux>, Json(corps): Json<Value>) -> Json<Value> {
    let id = corps["videoId"].as_str().unwrap_or("").to_string();
    f.appels.lock().unwrap().push(format!("player:{id}"));
    // vid002 n'offre que de l'AAC : le repli.
    Json(reponse_lecteur(&id, id == "vid002"))
}

async fn service(total: usize) -> (YouTubeService, Arc<StdMutex<Vec<String>>>) {
    service_en_forme(total, Forme::Historique).await
}

async fn service_en_forme(
    total: usize,
    forme: Forme,
) -> (YouTubeService, Arc<StdMutex<Vec<String>>>) {
    let appels = Arc::new(StdMutex::new(Vec::new()));
    let faux = Faux {
        total,
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

// ── Recherche paginée ──────────────────────────────────────────────────

#[tokio::test]
async fn la_recherche_rend_plus_d_une_page() {
    let (yt, appels) = service(55).await;

    let p1 = yt.search_page("laufey", 30, 0).await.expect("page 1");
    assert_eq!(
        p1.results.tracks.len(),
        30,
        "plus que les 3 titres du rayon « Songs »"
    );
    assert_eq!(p1.results.tracks[0].id, "vid000");
    assert_eq!(p1.results.tracks[0].title, "Titre 0");
    assert_eq!(
        p1.results.tracks[0].artist, "Laufey",
        "artiste lu dans l'élément réel"
    );
    assert!(p1.has_more, "55 titres : il en reste après 30");
    assert_eq!(
        p1.results.albums.len(),
        1,
        "albums de la recherche sans filtre"
    );

    let p2 = yt.search_page("laufey", 30, 30).await.expect("page 2");
    assert_eq!(
        ids(&p2.results.tracks),
        (30..55).map(|i| format!("vid{i:03}")).collect::<Vec<_>>()
    );
    assert!(!p2.has_more, "dernière page");
    assert!(
        p2.results.albums.is_empty(),
        "les albums ne se répètent pas"
    );

    let p3 = yt.search_page("laufey", 30, 60).await.expect("au-delà");
    assert!(p3.results.tracks.is_empty() && !p3.has_more);

    // Le chemin non paginé profite aussi des titres filtrés.
    assert_eq!(yt.search("laufey", 100).await.unwrap().tracks.len(), 55);

    let appels = appels.lock().unwrap().clone();
    assert!(
        appels.contains(&"search:suite-20".to_string()),
        "{appels:?}"
    );
    assert!(
        appels.contains(&"search:suite-40".to_string()),
        "{appels:?}"
    );
}

#[tokio::test]
async fn la_suite_recente_rend_son_jeton_dans_le_corps() {
    let (yt, appels) = service_en_forme(55, Forme::Recente).await;

    let p1 = yt.search_page("laufey", 30, 0).await.expect("page 1");
    assert_eq!(
        ids(&p1.results.tracks),
        (0..30).map(|i| format!("vid{i:03}")).collect::<Vec<_>>(),
        "la suite récente n'a pas été lue"
    );
    assert!(p1.has_more);
    let p2 = yt.search_page("laufey", 30, 30).await.expect("page 2");
    assert_eq!(p2.results.tracks.len(), 25);
    assert!(!p2.has_more);

    let appels = appels.lock().unwrap().clone();
    assert!(
        appels.contains(&"search:corps:suite-20".to_string())
            && appels.contains(&"search:corps:suite-40".to_string()),
        "{appels:?}"
    );
    assert!(
        !appels.iter().any(|a| a == "search:suite-20"),
        "jeton récent envoyé dans l'URL : {appels:?}"
    );
}

#[tokio::test]
async fn une_suite_refusee_dans_l_url_se_redemande_dans_le_corps() {
    let (yt, appels) = service_en_forme(55, Forme::UrlRefusee).await;

    let p1 = yt.search_page("laufey", 30, 0).await.expect("page 1");
    assert_eq!(
        p1.results.tracks.len(),
        30,
        "la suite s'est arrêtée au premier refus"
    );
    assert!(p1.has_more);

    let appels = appels.lock().unwrap().clone();
    let url = appels.iter().position(|a| a == "search:suite-20");
    let corps = appels.iter().position(|a| a == "search:corps:suite-20");
    assert!(
        matches!((url, corps), (Some(u), Some(c)) if u < c),
        "l'URL d'abord, puis le corps : {appels:?}"
    );
}

#[tokio::test]
async fn sans_titres_filtres_la_premiere_page_garde_le_rayon_songs() {
    let (yt, _) = service(0).await;
    let p1 = yt.search_page("rien", 30, 0).await.expect("page 1");
    assert_eq!(ids(&p1.results.tracks), ["vid000", "vid001", "vid002"]);
    assert!(!p1.has_more);
}

// ── Passer d'un titre à l'autre depuis une recherche ───────────────────

#[tokio::test]
async fn passer_d_un_titre_a_l_autre_depuis_la_recherche() {
    let (yt, appels) = service(25).await;
    let page = yt.search_page("laufey", 10, 0).await.expect("recherche");
    let pistes = &page.results.tracks;

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
    assert_eq!(
        vus,
        [
            ("vid000".into(), "opus".into(), "audio/webm".into()),
            ("vid001".into(), "opus".into(), "audio/webm".into()),
            ("vid002".into(), "m4a".into(), "audio/mp4".into()),
            ("vid000".into(), "opus".into(), "audio/webm".into()),
        ] as [(String, String, String); 4]
    );
    // Le retour au 1er titre vient du cache : un seul `/player` pour lui.
    let appels = appels.lock().unwrap().clone();
    assert_eq!(
        appels.iter().filter(|a| *a == "player:vid000").count(),
        1,
        "{appels:?}"
    );
}
