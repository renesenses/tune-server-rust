//! #4956 — « Genres --> ok mais tri sans ordre défini au début » (Jean
//! Valjean, Marantz ND8006, fil 1439, en 0.9.163 puis encore en 0.9.165).
//!
//! La 0.9.165 avait rangé les accents et les nombres (`comparer_naturel`).
//! Restaient EN TÊTE de liste les genres qui commencent par un signe
//! (« (Hip-Hop) », « 'Jazz ») et les jetons que `genre_counts` rend avec leur
//! espace de tête (« Rock, Blues » donne « Blues » précédé d'une espace) :
//! c'est « le début » de la liste que l'appareil montre dans le désordre.
//!
//! Décision de Bertrand (29/09/2026) : ordre alphabétique naturel —
//! ponctuation et symboles de tête ignorés, accents ramenés à la lettre de
//! base, casse ignorée ; ex æquo départagés de façon stable.
//!
//! Le témoin passe par la VRAIE route du serveur média,
//! `POST /ContentDirectory/control`, et lit le DIDL comme un point de
//! contrôle : `Browse` du rayon, et `Search` par classe — le chemin des
//! menus Artistes / Albums / Genres du ND8006 (#1777). Les mêmes règles
//! valent pour les rayons Artists, Albums et Playlists, qui triaient par le
//! `ORDER BY LOWER(…)` de la base (sur SQLite, `LOWER` ne replie que
//! l'ASCII : « Édith » après « Zazie »).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::DbBackend;
use tune_core::db::models::{Album, Artist, Track};
use tune_core::db::playlist_repo::PlaylistRepo;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo::TrackRepo;
use tune_core::upnp_server::UpnpState;

/// Une bibliothèque aux étiquettes réelles et piégeuses. Les albums sont
/// créés dans un ordre QUELCONQUE : l'ordre attendu ne peut pas venir de
/// l'ordre d'insertion.
fn etat_media() -> UpnpState {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);

    let artistes = ArtistRepo::with_backend(backend.clone());
    let albums = AlbumRepo::with_backend(backend.clone());
    let pistes = TrackRepo::with_backend(backend.clone());
    let listes = PlaylistRepo::with_backend(backend.clone());

    // (artiste, titre de l'album, genre)
    let bibliotheque = [
        ("Zazie", "Zen", "Rock, Blues"),
        ("(hed) p.e.", "(Inédit)", "(Hip-Hop)"),
        ("Édith Piaf", "Été indien", "Électro"),
        ("edith Crash", "été 85", "electro"),
        ("'Til Tuesday", "'Round Midnight", "'Jazz"),
        ("Aphex Twin", "Ambient Works", "Ambient"),
        ("2Pac", "2 Tone", "2 Tone"),
        ("10cc", "70s", "70s"),
        // Une borne de fin qui ne dépend d'aucun signe de tête.
        ("ZZ Top", "Zoo", "Zzz-garde"),
    ];
    let mut ids_pistes = Vec::new();
    for (i, (artiste, titre, genre)) in bibliotheque.iter().enumerate() {
        let artiste_id = artistes.create(&Artist::new(artiste.to_string())).unwrap();
        let mut album = Album::new(titre.to_string());
        album.genre = Some(genre.to_string());
        album.year = Some(2000);
        album.artist_id = Some(artiste_id);
        album.artist_name = Some(artiste.to_string());
        let album_id = albums.create(&album).unwrap();
        let mut piste = Track::new(format!("Piste {i}"));
        piste.album_id = Some(album_id);
        piste.album_title = Some(titre.to_string());
        piste.artist_id = Some(artiste_id);
        piste.artist_name = Some(artiste.to_string());
        piste.file_path = Some(format!("/musique/{i}.flac"));
        ids_pistes.push(pistes.create(&piste).unwrap());
    }
    for nom in ["Zen", "(Soirée)", "Été", "apéro"] {
        listes
            .create_with_tracks(nom, None, 1, &ids_pistes[..1])
            .unwrap();
    }
    UpnpState::new(backend, 8888, Some("127.0.0.1".into()))
}

fn corps_browse(object_id: &str, start: u64, requested_count: u64) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
 <s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
  <ObjectID>{object_id}</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag>
  <Filter>*</Filter><StartingIndex>{start}</StartingIndex><RequestedCount>{requested_count}</RequestedCount>
  <SortCriteria></SortCriteria>
 </u:Browse></s:Body></s:Envelope>"#
    )
}

/// `Search` par classe, avec la clause d'existence de la spécification.
fn corps_search(classe: &str) -> String {
    let criteres = format!("upnp:class derivedfrom \"{classe}\" and @refID exists false")
        .replace('"', "&quot;");
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
 <s:Body><u:Search xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
  <ContainerID>0</ContainerID><SearchCriteria>{criteres}</SearchCriteria>
  <Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>0</RequestedCount>
  <SortCriteria></SortCriteria>
 </u:Search></s:Body></s:Envelope>"#
    )
}

async fn poster(state: &UpnpState, action: &str, corps: String) -> String {
    let routeur = tune_server::routes::upnp_media_server::standalone_router(state.clone());
    let requete = Request::post("/ContentDirectory/control")
        .header("content-type", "text/xml; charset=\"utf-8\"")
        .header(
            "SOAPACTION",
            format!("\"urn:schemas-upnp-org:service:ContentDirectory:1#{action}\""),
        )
        .body(Body::from(corps))
        .unwrap();
    let reponse = routeur.oneshot(requete).await.unwrap();
    assert_eq!(reponse.status(), StatusCode::OK);
    let octets = axum::body::to_bytes(reponse.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    String::from_utf8(octets.to_vec()).unwrap()
}

fn champ(soap: &str, balise: &str) -> String {
    let ouvrant = format!("<{balise}>");
    let fermant = format!("</{balise}>");
    let debut = soap
        .find(&ouvrant)
        .unwrap_or_else(|| panic!("réponse sans <{balise}> : {soap}"))
        + ouvrant.len();
    let fin = soap[debut..]
        .find(&fermant)
        .unwrap_or_else(|| panic!("réponse sans </{balise}> : {soap}"))
        + debut;
    soap[debut..fin].to_string()
}

/// Les `<dc:title>` des CONTENEURS du DIDL, dans l'ordre reçu, déséchappés
/// comme les lit un point de contrôle.
fn titres(soap: &str) -> Vec<String> {
    let didl = quick_xml::escape::unescape(&champ(soap, "Result"))
        .expect("le <Result> doit se déséchapper")
        .into_owned();
    didl.split("<dc:title>")
        .skip(1)
        .map(|morceau| {
            let brut = morceau
                .split("</dc:title>")
                .next()
                .unwrap_or_else(|| panic!("<dc:title> non refermé : {didl}"));
            quick_xml::escape::unescape(brut)
                .expect("un titre doit se déséchapper")
                .into_owned()
        })
        .collect()
}

/// L'ordre attendu des genres. « Blues » garde l'espace de tête que lui laisse
/// `genre_counts` (le jeton n'est pas rogné, sans quoi il n'ouvrirait plus
/// rien) ; il se range pourtant à B.
const GENRES_ATTENDUS: [&str; 10] = [
    "2 Tone",
    "70s",
    "Ambient",
    " Blues",
    // Ex æquo « electro » / « Électro » : départagés par le texte brut,
    // toujours dans le même ordre.
    "electro",
    "Électro",
    "(Hip-Hop)",
    "'Jazz",
    "Rock",
    "Zzz-garde",
];

#[tokio::test]
async fn les_genres_suivent_l_ordre_alphabetique_naturel_4956() {
    let state = etat_media();

    let parcours = poster(&state, "Browse", corps_browse("genres", 0, 0)).await;
    assert_eq!(
        titres(&parcours),
        GENRES_ATTENDUS,
        "Browse « genres » : {parcours}"
    );

    // La pagination suit le même ordre : la page 2 commence où la 1re finit.
    let page2 = poster(&state, "Browse", corps_browse("genres", 3, 3)).await;
    assert_eq!(titres(&page2), GENRES_ATTENDUS[3..6], "{page2}");
    assert_eq!(
        champ(&page2, "TotalMatches"),
        GENRES_ATTENDUS.len().to_string()
    );

    // Le menu Genres du ND8006 passe par `Search` : même ordre.
    let recherche = poster(
        &state,
        "Search",
        corps_search("object.container.genre.musicGenre"),
    )
    .await;
    assert_eq!(
        titres(&recherche),
        GENRES_ATTENDUS,
        "Search des genres : {recherche}"
    );
}

#[tokio::test]
async fn artistes_albums_et_listes_suivent_le_meme_ordre_4956() {
    let state = etat_media();
    let artistes_attendus = [
        "2Pac",
        "10cc",
        "Aphex Twin",
        "edith Crash",
        "Édith Piaf",
        "(hed) p.e.",
        "'Til Tuesday",
        "Zazie",
        "ZZ Top",
    ];
    let parcours = poster(&state, "Browse", corps_browse("artists", 0, 0)).await;
    assert_eq!(titres(&parcours), artistes_attendus, "{parcours}");
    let page = poster(&state, "Browse", corps_browse("artists", 2, 3)).await;
    assert_eq!(titres(&page), artistes_attendus[2..5], "{page}");
    assert_eq!(champ(&page, "TotalMatches"), "9");
    let recherche = poster(
        &state,
        "Search",
        corps_search("object.container.person.musicArtist"),
    )
    .await;
    assert_eq!(titres(&recherche), artistes_attendus, "{recherche}");

    let albums_attendus = [
        "2 Tone",
        "70s",
        "Ambient Works",
        "été 85",
        "Été indien",
        "(Inédit)",
        "'Round Midnight",
        "Zen",
        "Zoo",
    ];
    let parcours = poster(&state, "Browse", corps_browse("albums", 0, 0)).await;
    assert_eq!(titres(&parcours), albums_attendus, "{parcours}");
    let page = poster(&state, "Browse", corps_browse("albums", 4, 2)).await;
    assert_eq!(titres(&page), albums_attendus[4..6], "{page}");
    assert_eq!(champ(&page, "TotalMatches"), "9");
    let recherche = poster(
        &state,
        "Search",
        corps_search("object.container.album.musicAlbum"),
    )
    .await;
    assert_eq!(titres(&recherche), albums_attendus, "{recherche}");

    let parcours = poster(&state, "Browse", corps_browse("playlists", 0, 0)).await;
    assert_eq!(
        titres(&parcours),
        ["apéro", "Été", "(Soirée)", "Zen"],
        "{parcours}"
    );
}
