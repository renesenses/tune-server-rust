//! #1390 — « Albums / All tracks / Genres vus vides par Foobar2000 et WiiM,
//! alors qu'Emby et Serviio sont corrects ».
//!
//! Le fait mesuré n'est pas une bibliothèque vide : deux points de contrôle
//! voient le contenu, deux ne le voient pas. La différence est donc dans la
//! RÉPONSE, et elle se mesure ici sur la vraie route HTTP du serveur média —
//! `POST /ContentDirectory/control` —, pas sur les fonctions internes.
//!
//! Ces trois rubriques ne se remplissent pas par `Browse` : elles ne sont pas
//! les conteneurs du serveur (le point de contrôle les fabrique lui-même, ce
//! qui explique qu'elles portent les mêmes noms chez Emby, Serviio et Tune).
//! Elles se remplissent par `Search`, exactement comme le menu du ND8006
//! de #1777. Et le critère qu'un point de contrôle envoie n'est pas
//! `upnp:class derivedfrom "…"` tout nu : c'est la forme des exemples de la
//! spécification ContentDirectory:1, avec la clause d'existence
//! `and @refID exists false` qui exclut les objets de référence.
//!
//! Tune refusait cette clause — `evaluer_criteres` faisait tomber tout champ
//! commençant par `@` dans son bras « autre champ » — et répondait un SOAP
//! 708 servi en HTTP 500. Un client strict lit « ce dossier ne contient
//! rien » ; un client tolérant retente sans la clause et voit la
//! bibliothèque. C'est tout l'écart entre Foobar/WiiM et Emby/Serviio.
//!
//! Le fait de base tenu ici : **la réponse annonce autant d'éléments qu'elle
//! en transporte, et ce nombre n'est pas zéro sur une bibliothèque peuplée**.
//! Pas un code HTTP 200 : une réponse 200 qui porte un DIDL vide est
//! exactement le faux vert que ce ticket décrit.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::DbBackend;
use tune_core::db::models::{Album, Artist, Track};
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo::TrackRepo;
use tune_core::upnp_server::UpnpState;

/// Une bibliothèque assez peuplée pour que « vide » ne puisse pas être une
/// vérité : quatre artistes, douze albums répartis sur quatre genres,
/// soixante pistes.
fn etat_media() -> UpnpState {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);

    let artistes = ArtistRepo::with_backend(backend.clone());
    let albums = AlbumRepo::with_backend(backend.clone());
    let pistes = TrackRepo::with_backend(backend.clone());
    let genres = ["Jazz", "Rock", "Classical", "Electronic"];

    for a in 0..4usize {
        let artiste_id = artistes
            .create(&Artist::new(format!("Artiste {a}")))
            .unwrap();
        for d in 0..3usize {
            let mut album = Album::new(format!("Album {a}-{d}"));
            album.genre = Some(genres[(a + d) % genres.len()].to_string());
            album.year = Some(1960 + a as i32 * 3 + d as i32);
            album.artist_id = Some(artiste_id);
            album.artist_name = Some(format!("Artiste {a}"));
            let album_id = albums.create(&album).unwrap();
            for t in 0..5usize {
                let mut piste = Track::new(format!("Piste {a}-{d}-{t}"));
                piste.album_id = Some(album_id);
                piste.album_title = Some(format!("Album {a}-{d}"));
                piste.artist_id = Some(artiste_id);
                piste.artist_name = Some(format!("Artiste {a}"));
                piste.file_path = Some(format!("/musique/{a}-{d}-{t}.flac"));
                pistes.create(&piste).unwrap();
            }
        }
    }
    UpnpState::new(backend, 8888, Some("127.0.0.1".into()))
}

/// Le corps SOAP d'un `Search` tel qu'un point de contrôle l'envoie.
fn corps_search(conteneur: &str, criteres: &str, requested_count: u64) -> String {
    // Le critère voyage ÉCHAPPÉ dans l'enveloppe, comme sur le fil.
    let criteres = criteres.replace('"', "&quot;");
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
 <s:Body><u:Search xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
  <ContainerID>{conteneur}</ContainerID><SearchCriteria>{criteres}</SearchCriteria>
  <Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>{requested_count}</RequestedCount>
  <SortCriteria></SortCriteria>
 </u:Search></s:Body></s:Envelope>"#
    )
}

/// Le corps SOAP d'un `Browse` tel qu'un point de contrôle l'envoie.
fn corps_browse(object_id: &str, requested_count: u64) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
 <s:Body><u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
  <ObjectID>{object_id}</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag>
  <Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>{requested_count}</RequestedCount>
  <SortCriteria></SortCriteria>
 </u:Browse></s:Body></s:Envelope>"#
    )
}

/// Poste le corps sur la VRAIE route du serveur média et rend `(statut, corps)`.
async fn poster(action: &str, corps: String) -> (StatusCode, String) {
    let routeur = tune_server::routes::upnp_media_server::standalone_router(etat_media());
    let requete = Request::post("/ContentDirectory/control")
        .header("content-type", "text/xml; charset=\"utf-8\"")
        .header(
            "SOAPACTION",
            format!("\"urn:schemas-upnp-org:service:ContentDirectory:1#{action}\""),
        )
        .body(Body::from(corps))
        .unwrap();
    let reponse = routeur.oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), 16 * 1024 * 1024)
        .await
        .unwrap();
    (statut, String::from_utf8(octets.to_vec()).unwrap())
}

/// Le texte brut d'une balise de la réponse.
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

/// Le nombre d'éléments RÉELLEMENT transportés par le DIDL-Lite de `<Result>`,
/// lu comme le lit un point de contrôle : extrait, déséchappé, puis compté.
fn elements_transportes(soap: &str) -> usize {
    let didl = quick_xml::escape::unescape(&champ(soap, "Result"))
        .expect("le <Result> doit se déséchapper")
        .into_owned();
    let mut lecteur = quick_xml::Reader::from_str(&didl);
    let mut tampon = Vec::new();
    let mut n = 0usize;
    loop {
        match lecteur.read_event_into(&mut tampon) {
            Ok(quick_xml::events::Event::Start(e)) | Ok(quick_xml::events::Event::Empty(e)) => {
                let nom = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                if nom == "container" || nom == "item" {
                    n += 1;
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(e) => panic!("DIDL mal formé : {e} — {didl}"),
            _ => {}
        }
        tampon.clear();
    }
    n
}

/// Les trois rubriques du ticket, avec la classe DIDL que le point de contrôle
/// nomme pour chacune.
const RUBRIQUES: [(&str, &str); 3] = [
    ("Albums", "object.container.album.musicAlbum"),
    ("All tracks", "object.item.audioItem.musicTrack"),
    ("Genres", "object.container.genre.musicGenre"),
];

/// Ce que tout point de contrôle colle derrière son critère de classe.
const CLAUSE_EXISTENCE: &str = " and @refID exists false";

/// LE test de #1390 — ROUGE avant le correctif.
///
/// Avant : HTTP 500, `<errorCode>708</errorCode>`, aucun élément.
/// Après : `<u:SearchResponse>`, autant d'éléments que `TotalMatches` annonce,
/// et ce nombre n'est pas zéro.
#[tokio::test]
async fn la_recherche_canonique_d_un_point_de_controle_remplit_les_trois_rubriques() {
    for (rubrique, classe) in RUBRIQUES {
        let criteres = format!("upnp:class derivedfrom \"{classe}\"{CLAUSE_EXISTENCE}");
        let (statut, soap) = poster("Search", corps_search("0", &criteres, 0)).await;

        assert_eq!(
            statut,
            StatusCode::OK,
            "« {rubrique} » : le point de contrôle reçoit {statut} pour son \
             critère canonique — il n'a rien d'autre à afficher que « Empty \
             Folder ». Critère : {criteres}\n{soap}"
        );
        assert!(
            !soap.contains("<s:Fault>"),
            "« {rubrique} » : fault SOAP sur le critère canonique {criteres}\n{soap}"
        );

        let transportes = elements_transportes(&soap);
        assert!(
            transportes > 0,
            "« {rubrique} » s'ouvre VIDE sur une bibliothèque de douze albums \
             et soixante pistes : {soap}"
        );
        assert_eq!(
            champ(&soap, "TotalMatches"),
            transportes.to_string(),
            "« {rubrique} » : TotalMatches ne dit pas le nombre d'éléments \
             réellement présents dans le DIDL — un point de contrôle strict \
             croit le compteur, pas le corps\n{soap}"
        );
        assert_eq!(
            champ(&soap, "NumberReturned"),
            transportes.to_string(),
            "« {rubrique} » : NumberReturned ne dit pas le nombre d'éléments \
             réellement rendus\n{soap}"
        );
    }
}

/// `@refID exists true` demande les objets de RÉFÉRENCE. Tune n'en publie
/// aucun : la réponse est une liste vide — jamais une faute, la règle déjà
/// tenue pour les classes inconnues.
#[tokio::test]
async fn demander_les_objets_de_reference_rend_une_liste_vide_sans_faute() {
    let criteres =
        "upnp:class derivedfrom \"object.item.audioItem.musicTrack\" and @refID exists true";
    let (statut, soap) = poster("Search", corps_search("0", criteres, 0)).await;
    assert_eq!(statut, StatusCode::OK, "{soap}");
    assert!(!soap.contains("<s:Fault>"), "{soap}");
    assert_eq!(elements_transportes(&soap), 0, "{soap}");
    assert_eq!(champ(&soap, "TotalMatches"), "0", "{soap}");
}

/// Ce qu'on évalue, on l'annonce — et réciproquement (#2312). `@refID` entre
/// dans `SearchCaps` en même temps qu'il devient évaluable.
#[tokio::test]
async fn les_capacites_annoncent_la_clause_d_existence() {
    let corps = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
 <s:Body><u:GetSearchCapabilities xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"/></s:Body>
</s:Envelope>"#;
    let (statut, soap) = poster("GetSearchCapabilities", corps.to_string()).await;
    assert_eq!(statut, StatusCode::OK, "{soap}");
    let caps = champ(&soap, "SearchCaps");
    assert!(
        caps.contains("@refID"),
        "un client qui lit les capacités avant de chercher n'apprend pas que \
         la clause d'existence est acceptée : {caps}"
    );
}

// ---------------------------------------------------------------------------
// Témoins — verts des deux côtés du correctif
// ---------------------------------------------------------------------------

/// Témoin 1 : le critère SANS clause d'existence marchait déjà (#1777) et doit
/// continuer. Si celui-ci bougeait, le correctif aurait déplacé le défaut au
/// lieu de le supprimer.
#[tokio::test]
async fn temoin_la_recherche_sans_clause_d_existence_reste_servie() {
    for (rubrique, classe) in RUBRIQUES {
        let criteres = format!("upnp:class derivedfrom \"{classe}\"");
        let (statut, soap) = poster("Search", corps_search("0", &criteres, 0)).await;
        assert_eq!(statut, StatusCode::OK, "« {rubrique} » : {soap}");
        let transportes = elements_transportes(&soap);
        assert!(transportes > 0, "« {rubrique} » : {soap}");
        assert_eq!(
            champ(&soap, "TotalMatches"),
            transportes.to_string(),
            "« {rubrique} » : {soap}"
        );
    }
}

/// Témoin 2 : le parcours par `Browse`, lui, n'a jamais été en cause — c'est
/// « on voit le dossier, la jaquette et la musique » du fil. `RequestedCount=0`
/// veut dire « tout », et le compteur annoncé est celui du corps.
#[tokio::test]
async fn temoin_le_browse_a_requested_count_zero_rend_tout_et_l_annonce() {
    for conteneur in ["albums", "genres", "artists"] {
        let (statut, soap) = poster("Browse", corps_browse(conteneur, 0)).await;
        assert_eq!(statut, StatusCode::OK, "{conteneur} : {soap}");
        let transportes = elements_transportes(&soap);
        assert!(
            transportes > 0,
            "le conteneur {conteneur} s'ouvre vide : {soap}"
        );
        assert_eq!(
            champ(&soap, "NumberReturned"),
            transportes.to_string(),
            "{conteneur} : {soap}"
        );
        assert_eq!(
            champ(&soap, "TotalMatches"),
            transportes.to_string(),
            "{conteneur} : RequestedCount=0 doit rendre l'ensemble complet, \
             donc annoncer autant que le corps transporte\n{soap}"
        );
    }
}
