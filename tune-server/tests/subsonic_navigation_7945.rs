//! #7945 — Subsonic étape 2 : navigation, recherche et lecture sur `/rest`.
//!
//! Bout en bout, par le vrai routeur du serveur, sur une petite bibliothèque
//! d'exemple : deux artistes, deux albums, trois pistes, chacune adossée à un
//! vrai fichier temporaire de 100 octets. Chaque témoin lit le corps rendu au
//! client, en JSON et en XML :
//!
//! - chaque méthode répond, avec des identifiants stables et relisibles ;
//! - `stream` honore `Range` (206, `Content-Range`, octets exacts), refuse
//!   une plage hors fichier (416) et sert le fichier entier sans plage ;
//! - `download` sert le fichier entier en pièce jointe ;
//! - réglage éteint : 404 ; licence gratuite : code 50, sur chaque méthode ;
//! - un profil supervisé ne voit ni ne lit un artiste bloqué (#7856).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::cles_clients::ClesClientsRepo;
use tune_core::db::politique_parentale::{self as pp, ArtisteBloque, PolitiqueParentale};

use tune_server::state::AppState;

const ENFANT: i64 = 2;

/// Les méthodes de l'étape 2, avec des paramètres valables.
const METHODES: &[&str] = &[
    "getArtists",
    "getArtist?id=ar-7",
    "getAlbum?id=al-70",
    "getSong?id=tr-700",
    "getAlbumList2?type=alphabeticalByName",
    "search3?query=a",
    "getGenres",
    "getSongsByGenre?genre=Rock",
    "getRandomSongs",
    "getScanStatus",
    "getCoverArt?id=al-70",
    "stream?id=tr-700",
    "download?id=tr-700",
];

struct Banc {
    state: AppState,
    app: axum::Router,
    cle: String,
    cle_enfant: String,
    _dossier: tempfile::TempDir,
}

/// Le contenu du fichier de la piste `id` : 100 octets reconnaissables.
fn octets(id: i64) -> Vec<u8> {
    (0..100u8).map(|i| i.wrapping_add(id as u8)).collect()
}

fn bibliotheque(state: &AppState, dossier: &std::path::Path) {
    let db = &state.backend;
    let sql = |s: &str, p: &[&dyn ToSqlValue]| {
        db.execute(s, p).unwrap();
    };
    sql(
        "INSERT INTO profiles (id, username, display_name, is_admin) VALUES (2, 'enfant', 'Enfant', 0)",
        &[],
    );
    // (artiste, nom, album, titre, année, genre, pistes)
    let jeu: [(i64, &str, i64, &str, i64, &str, &[i64]); 2] = [
        (
            7,
            "Groupe Interdit",
            70,
            "Album Bleu",
            1999,
            "Rock",
            &[700, 701],
        ),
        (8, "The Libres", 80, "Album Vert", 2010, "Jazz", &[800]),
    ];
    for (artiste, nom, album, titre, annee, genre, pistes) in jeu {
        sql(
            "INSERT INTO artists (id, name) VALUES (?, ?)",
            &[&artiste, &nom],
        );
        sql(
            "INSERT INTO albums (id, title, artist_id, year, genre, cover_path) VALUES (?, ?, ?, ?, ?, ?)",
            &[
                &album,
                &titre,
                &artiste,
                &annee,
                &genre,
                &format!("https://pochettes.example/{album}.jpg"),
            ],
        );
        for (n, piste) in pistes.iter().enumerate() {
            let chemin = dossier.join(format!("{piste}.flac"));
            std::fs::write(&chemin, octets(*piste)).unwrap();
            sql(
                "INSERT INTO tracks (id, title, artist_id, album_id, file_path, format, \
                 file_size, duration_ms, track_number, disc_number, year, genre, sample_rate, \
                 bit_depth, channels) VALUES (?, ?, ?, ?, ?, 'flac', 100, 180000, ?, 1, ?, ?, \
                 44100, 16, 2)",
                &[
                    piste,
                    &format!("Titre {piste}"),
                    &artiste,
                    &album,
                    &chemin.to_string_lossy().to_string(),
                    &((n + 1) as i64),
                    &annee,
                    &genre,
                ],
            );
        }
    }
}

/// Un serveur Premium, accès Subsonic allumé, la bibliothèque d'exemple, une
/// clé pour l'administrateur et une pour un profil supervisé.
async fn banc() -> Banc {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state.clone());
    state.license.set_account_premium(true, None).await;
    let dossier = tempfile::tempdir().unwrap();
    bibliotheque(&state, dossier.path());
    let (s, _, corps) = brut(
        &app,
        Request::builder()
            .method("PUT")
            .uri("/api/v1/subsonic/reglage")
            .header("Content-Type", "application/json")
            .body(Body::from(json!({"enabled": true}).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{corps:?}");
    let cles = ClesClientsRepo::with_backend(state.backend.clone());
    let cle = cles.creer(1, "Symfonium").unwrap().secret;
    let cle_enfant = cles.creer(ENFANT, "Tablette").unwrap().secret;
    Banc {
        state,
        app,
        cle,
        cle_enfant,
        _dossier: dossier,
    }
}

async fn brut(
    app: &axum::Router,
    req: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let entetes = resp.headers().clone();
    let corps = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, entetes, corps)
}

fn adresse(methode: &str, cle: &str, f: &str) -> String {
    let sep = if methode.contains('?') { '&' } else { '?' };
    format!("/rest/{methode}{sep}apiKey={cle}&f={f}")
}

async fn get(
    app: &axum::Router,
    chemin: &str,
    range: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut req = Request::builder().method("GET").uri(chemin);
    if let Some(r) = range {
        req = req.header("Range", r);
    }
    brut(app, req.body(Body::empty()).unwrap()).await
}

/// La réponse JSON, son enveloppe ouverte et son statut contrôlé.
async fn rest(b: &Banc, methode: &str) -> Value {
    rest_avec(&b.app, methode, &b.cle).await
}

async fn rest_avec(app: &axum::Router, methode: &str, cle: &str) -> Value {
    let chemin = adresse(methode, cle, "json");
    let (s, _, corps) = get(app, &chemin, None).await;
    let texte = String::from_utf8_lossy(&corps);
    assert_eq!(s, StatusCode::OK, "{chemin} : {texte}");
    let v: Value = serde_json::from_slice(&corps)
        .unwrap_or_else(|_| panic!("{chemin} : pas du JSON : {texte}"));
    v["subsonic-response"].clone()
}

fn ok(r: &Value) -> &Value {
    assert_eq!(r["status"], "ok", "réponse en échec : {r}");
    r
}

fn code_d_erreur(r: &Value) -> i64 {
    assert_eq!(r["status"], "failed", "refus attendu, reçu : {r}");
    r["error"]["code"].as_i64().unwrap()
}

async fn xml(b: &Banc, methode: &str) -> String {
    let chemin = adresse(methode, &b.cle, "xml");
    let (s, entetes, corps) = get(&b.app, &chemin, None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        entetes["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/xml"),
        "{chemin} : {:?}",
        entetes["content-type"]
    );
    String::from_utf8(corps).unwrap()
}

fn ids(liste: &Value) -> Vec<String> {
    liste
        .as_array()
        .unwrap_or_else(|| panic!("pas un tableau : {liste}"))
        .iter()
        .map(|o| o["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn la_navigation_rend_des_identifiants_stables_et_relisibles_7945() {
    let b = banc().await;

    let r = rest(&b, "getArtists").await;
    let artistes = &ok(&r)["artists"];
    assert_eq!(artistes["ignoredArticles"], "The El La Los Las Le Les");
    let index = artistes["index"].as_array().unwrap();
    let lettres: Vec<&str> = index.iter().map(|i| i["name"].as_str().unwrap()).collect();
    // « The Libres » se range à L, l'article ignoré.
    assert_eq!(lettres, ["G", "L"], "{artistes}");
    assert_eq!(ids(&index[0]["artist"]), ["ar-7"]);
    assert_eq!(index[0]["artist"][0]["albumCount"], 1);

    let r = rest(&b, "getArtist?id=ar-7").await;
    let artiste = &ok(&r)["artist"];
    assert_eq!(artiste["name"], "Groupe Interdit");
    assert_eq!(ids(&artiste["album"]), ["al-70"]);

    let r = rest(&b, "getAlbum?id=al-70").await;
    let album = &ok(&r)["album"];
    assert_eq!(album["name"], "Album Bleu");
    assert_eq!(album["artistId"], "ar-7");
    assert_eq!(album["songCount"], 2);
    assert_eq!(album["duration"], 360);
    assert_eq!(album["year"], 1999);
    assert_eq!(album["coverArt"], "al-70");
    assert_eq!(ids(&album["song"]), ["tr-700", "tr-701"]);
    let premiere = &album["song"][0];
    for (cle, attendu) in [
        ("parent", json!("al-70")),
        ("albumId", json!("al-70")),
        ("artistId", json!("ar-7")),
        ("isDir", json!(false)),
        ("suffix", json!("flac")),
        ("contentType", json!("audio/flac")),
        ("size", json!(100)),
        ("duration", json!(180)),
        ("track", json!(1)),
        ("discNumber", json!(1)),
        ("samplingRate", json!(44100)),
        ("bitDepth", json!(16)),
        ("channelCount", json!(2)),
        ("type", json!("music")),
        ("mediaType", json!("song")),
    ] {
        assert_eq!(
            premiere[cle], attendu,
            "champ {cle} de la piste : {premiere}"
        );
    }
    assert!(
        premiere.get("path").is_none(),
        "le chemin du disque ne sort pas"
    );

    // L'identifiant rendu par getAlbum se relit par getSong, à l'identique,
    // et un second appel rend les mêmes identifiants.
    let r = rest(&b, "getSong?id=tr-700").await;
    assert_eq!(ok(&r)["song"], *premiere);
    let r2 = rest(&b, "getAlbum?id=al-70").await;
    assert_eq!(r2["album"]["song"], album["song"]);
}

#[tokio::test]
async fn les_reponses_xml_ont_la_forme_subsonic_7945() {
    let b = banc().await;
    let x = xml(&b, "getArtists").await;
    assert!(
        x.contains(
            r#"<artists ignoredArticles="The El La Los Las Le Les"><index name="G"><artist "#
        ),
        "{x}"
    );
    assert!(x.contains(r#"id="ar-7""#), "{x}");
    let x = xml(&b, "getAlbum?id=al-70").await;
    assert!(
        x.contains(r#"<album "#) && x.contains(r#"id="al-70""#),
        "{x}"
    );
    assert!(
        x.contains(r#"<song "#) && x.contains(r#"id="tr-701""#),
        "{x}"
    );
    let x = xml(&b, "search3?query=vert").await;
    assert!(x.contains("<searchResult3>"), "{x}");
    assert!(x.contains(r#"id="al-80""#), "{x}");
    let x = xml(&b, "getGenres").await;
    assert!(
        x.contains(r#"<genre value="Jazz" songCount="1" albumCount="1"/>"#)
            || x.contains(r#"<genre albumCount="1" songCount="1" value="Jazz"/>"#),
        "{x}"
    );
    let x = xml(&b, "getScanStatus").await;
    assert!(
        x.contains("<scanStatus ") && x.contains(r#"count="3""#),
        "{x}"
    );
}

#[tokio::test]
async fn les_listes_d_albums_suivent_leur_type_7945() {
    let b = banc().await;
    let liste = |r: Value| ids(&ok(&r)["albumList2"]["album"]);
    assert_eq!(
        liste(rest(&b, "getAlbumList2?type=alphabeticalByName").await),
        ["al-70", "al-80"]
    );
    assert_eq!(
        liste(rest(&b, "getAlbumList2?type=alphabeticalByName&size=1&offset=1").await),
        ["al-80"]
    );
    assert_eq!(
        liste(rest(&b, "getAlbumList2?type=newest").await),
        ["al-80", "al-70"]
    );
    assert_eq!(
        liste(rest(&b, "getAlbumList2?type=byYear&fromYear=2000&toYear=2020").await),
        ["al-80"]
    );
    assert_eq!(
        liste(rest(&b, "getAlbumList2?type=byYear&fromYear=2020&toYear=1990").await),
        ["al-80", "al-70"]
    );
    assert_eq!(
        liste(rest(&b, "getAlbumList2?type=byGenre&genre=rock").await),
        ["al-70"]
    );
    let mut hasard = liste(rest(&b, "getAlbumList2?type=random").await);
    hasard.sort();
    assert_eq!(hasard, ["al-70", "al-80"]);
    assert!(liste(rest(&b, "getAlbumList2?type=starred").await).is_empty());
    assert_eq!(code_d_erreur(&rest(&b, "getAlbumList2").await), 10);
    assert_eq!(
        code_d_erreur(&rest(&b, "getAlbumList2?type=byYear").await),
        10
    );
}

#[tokio::test]
async fn recherche_genres_et_hasard_7945() {
    let b = banc().await;
    let r = rest(&b, "search3?query=interdit").await;
    let res = &ok(&r)["searchResult3"];
    assert_eq!(ids(&res["artist"]), ["ar-7"]);
    assert_eq!(ids(&res["album"]), ["al-70"]);
    assert_eq!(ids(&res["song"]), ["tr-700", "tr-701"]);

    // Requête vide : toute la bibliothèque, page par page (synchronisation
    // des clients comme Symfonium).
    let r = rest(&b, "search3?query=%22%22&songCount=2&songOffset=2").await;
    assert_eq!(ids(&ok(&r)["searchResult3"]["song"]), ["tr-800"]);
    assert_eq!(code_d_erreur(&rest(&b, "search3").await), 10);

    let r = rest(&b, "getGenres").await;
    let genres = ok(&r)["genres"]["genre"].as_array().unwrap().clone();
    assert_eq!(
        genres,
        [
            json!({"value": "Jazz", "songCount": 1, "albumCount": 1}),
            json!({"value": "Rock", "songCount": 2, "albumCount": 1}),
        ]
    );
    let r = rest(&b, "getSongsByGenre?genre=Rock").await;
    assert_eq!(ids(&ok(&r)["songsByGenre"]["song"]), ["tr-700", "tr-701"]);

    let r = rest(&b, "getRandomSongs?size=10").await;
    let mut tirage = ids(&ok(&r)["randomSongs"]["song"]);
    tirage.sort();
    assert_eq!(tirage, ["tr-700", "tr-701", "tr-800"]);
    let r = rest(&b, "getRandomSongs?genre=Jazz").await;
    assert_eq!(ids(&ok(&r)["randomSongs"]["song"]), ["tr-800"]);
    let r = rest(&b, "getRandomSongs?fromYear=1990&toYear=2000").await;
    let mut tirage = ids(&ok(&r)["randomSongs"]["song"]);
    tirage.sort();
    assert_eq!(tirage, ["tr-700", "tr-701"]);

    let r = rest(&b, "getScanStatus").await;
    assert_eq!(ok(&r)["scanStatus"]["count"], 3);
    assert_eq!(r["scanStatus"]["scanning"], false);
}

#[tokio::test]
async fn un_identifiant_inconnu_ou_d_un_autre_genre_rend_le_code_70_7945() {
    let b = banc().await;
    for methode in [
        "getArtist?id=ar-999",
        "getArtist?id=al-70",
        "getAlbum?id=al-999",
        "getAlbum?id=ar-7",
        "getSong?id=tr-999",
        "getSong?id=700",
        "getCoverArt?id=xx-1",
        "stream?id=al-70",
        "download?id=tr-999",
    ] {
        assert_eq!(code_d_erreur(&rest(&b, methode).await), 70, "{methode}");
    }
    for methode in [
        "getArtist",
        "getAlbum",
        "getSong",
        "getCoverArt",
        "stream",
        "download",
        "getSongsByGenre",
    ] {
        assert_eq!(code_d_erreur(&rest(&b, methode).await), 10, "{methode}");
    }
}

#[tokio::test]
async fn stream_honore_range_en_206_7945() {
    let b = banc().await;
    let chemin = adresse("stream?id=tr-701", &b.cle, "json");

    let (s, entetes, corps) = get(&b.app, &chemin, Some("bytes=0-9")).await;
    assert_eq!(
        s,
        StatusCode::PARTIAL_CONTENT,
        "stream doit répondre 206 à une plage, reçu {s}"
    );
    assert_eq!(entetes["content-range"], "bytes 0-9/100");
    assert_eq!(entetes["content-length"], "10");
    assert_eq!(entetes["content-type"], "audio/flac");
    assert_eq!(corps, octets(701)[..10]);

    let (s, entetes, corps) = get(&b.app, &chemin, Some("bytes=90-")).await;
    assert_eq!(s, StatusCode::PARTIAL_CONTENT);
    assert_eq!(entetes["content-range"], "bytes 90-99/100");
    assert_eq!(corps, octets(701)[90..]);

    let (s, entetes, _) = get(&b.app, &chemin, Some("bytes=200-300")).await;
    assert_eq!(s, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(entetes["content-range"], "bytes */100");

    // Sans plage, le fichier d'origine entier ; les paramètres de
    // transcodage sont ignorés.
    let chemin_mp3 = adresse("stream?id=tr-701&format=mp3&maxBitRate=128", &b.cle, "json");
    let (s, entetes, corps) = get(&b.app, &chemin_mp3, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(entetes["accept-ranges"], "bytes");
    assert_eq!(entetes["content-type"], "audio/flac");
    assert_eq!(corps, octets(701));
}

#[tokio::test]
async fn download_sert_le_fichier_entier_en_piece_jointe_7945() {
    let b = banc().await;
    let (s, entetes, corps) =
        get(&b.app, &adresse("download?id=tr-800", &b.cle, "json"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(entetes["content-disposition"], "attachment");
    assert_eq!(corps, octets(800));
}

#[tokio::test]
async fn get_cover_art_sert_la_pochette_de_l_album_7945() {
    let b = banc().await;
    for id in ["al-80", "tr-800"] {
        let (s, entetes, _) = get(
            &b.app,
            &adresse(&format!("getCoverArt?id={id}&size=300"), &b.cle, "json"),
            None,
        )
        .await;
        assert_eq!(s, StatusCode::TEMPORARY_REDIRECT, "{id}");
        assert_eq!(
            entetes["location"], "https://pochettes.example/80.jpg",
            "{id}"
        );
    }
    // Un artiste sans image : introuvable, dans l'enveloppe Subsonic.
    assert_eq!(code_d_erreur(&rest(&b, "getCoverArt?id=ar-8").await), 70);
}

#[tokio::test]
async fn reglage_eteint_chaque_methode_repond_404_7945() {
    let b = banc().await;
    let (s, _, _) = brut(
        &b.app,
        Request::builder()
            .method("PUT")
            .uri("/api/v1/subsonic/reglage")
            .header("Content-Type", "application/json")
            .body(Body::from(json!({"enabled": false}).to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    for methode in METHODES {
        let vue = match methode.split_once('?') {
            Some((m, q)) => format!("{m}.view?{q}"),
            None => format!("{methode}.view"),
        };
        for chemin in [
            adresse(methode, &b.cle, "json"),
            adresse(&vue, &b.cle, "xml"),
        ] {
            let (s, _, _) = get(&b.app, &chemin, None).await;
            assert_eq!(s, StatusCode::NOT_FOUND, "{chemin} réglage éteint");
        }
    }
}

#[tokio::test]
async fn sans_premium_chaque_methode_rend_le_code_50_7945() {
    let b = banc().await;
    b.state.license.set_account_premium(false, None).await;
    for methode in METHODES {
        let r = rest(&b, methode).await;
        assert_eq!(code_d_erreur(&r), 50, "{methode} sans Premium : {r}");
        let (s, _, corps) = get(&b.app, &adresse(methode, &b.cle, "json"), Some("bytes=0-9")).await;
        assert_eq!(s, StatusCode::OK, "{methode}");
        assert_ne!(
            corps,
            octets(700)[..10],
            "{methode} a servi des octets sans Premium"
        );
    }
}

#[tokio::test]
async fn un_profil_supervise_ne_voit_ni_ne_lit_un_artiste_bloque_7945() {
    let b = banc().await;
    let politique = PolitiqueParentale {
        artistes_bloques: vec![ArtisteBloque {
            id: Some(7),
            nom: None,
        }],
        ..Default::default()
    };
    pp::poser(&b.state.backend, ENFANT, 1, 0, &politique).unwrap();
    let enfant = |m: &'static str| rest_avec(&b.app, m, &b.cle_enfant);

    let r = enfant("getArtists").await;
    let index = ok(&r)["artists"]["index"].as_array().unwrap().clone();
    let vus: Vec<String> = index.iter().flat_map(|i| ids(&i["artist"])).collect();
    assert_eq!(vus, ["ar-8"], "artiste bloqué servi par getArtists : {r}");

    let r = enfant("search3?query=").await;
    let res = &ok(&r)["searchResult3"];
    assert_eq!(ids(&res["artist"]), ["ar-8"], "{res}");
    assert_eq!(ids(&res["album"]), ["al-80"], "{res}");
    assert_eq!(ids(&res["song"]), ["tr-800"], "{res}");

    let r = enfant("getAlbumList2?type=alphabeticalByName").await;
    assert_eq!(ids(&ok(&r)["albumList2"]["album"]), ["al-80"]);
    let r = enfant("getRandomSongs").await;
    assert_eq!(ids(&ok(&r)["randomSongs"]["song"]), ["tr-800"]);
    let r = enfant("getSongsByGenre?genre=Rock").await;
    assert!(ids(&ok(&r)["songsByGenre"]["song"]).is_empty());

    for methode in [
        "getArtist?id=ar-7",
        "getAlbum?id=al-70",
        "getSong?id=tr-700",
        "getCoverArt?id=al-70",
        "stream?id=tr-700",
        "download?id=tr-700",
    ] {
        assert_eq!(
            code_d_erreur(&enfant(methode).await),
            70,
            "{methode} servi au profil supervisé"
        );
    }

    // L'administrateur, lui, voit toujours tout.
    let r = rest(&b, "getArtist?id=ar-7").await;
    assert_eq!(ok(&r)["artist"]["id"], "ar-7");
}
