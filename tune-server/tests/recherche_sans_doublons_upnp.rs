//! La RECHERCHE de bibliothèque ne rend pas le double UPnP d'un album local.
//!
//! ## Ce qui a été mesuré
//!
//! Le 23/09/2026 sur le `.18`, `GET /library/search?q=Abacab` rendait l'album
//! local (id 10420) ET son double UPnP (id 16421), alors que la grille de
//! `/library/albums` masque le second depuis #4146 (« le LOCAL est
//! prioritaire, et on n'affiche PAS l'autre », Bertrand, 14/09/2026).
//!
//! ## Ce que cette épreuve garde
//!
//! | # | Propriété |
//! |---|---|
//! | 1 | un album local + son double UPnP : la recherche ne rend QUE le local — albums, pistes, totaux, labels |
//! | 2 | un album UPnP SANS double local reste trouvé, lui et sa piste |
//!
//! Le banc porte les deux natures de distant, comme celui de #4146 : sans
//! l'orphelin, « rien d'`upnp` ne sort » passerait ; sans le doublé, « tout
//! d'`upnp` sort » passerait aussi.
//!
//! On lit le CORPS JSON de la route montée, jamais une condition SQL : un test
//! qui rejouerait le prédicat le recopierait au lieu de le garder.
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::models::{Album, Track};
use tune_core::db::track_repo::TrackRepo;
use tune_server::state::AppState;

/// La saisie : elle attrape les TROIS albums du banc par leur titre.
const SAISIE: &str = "Abacab";
/// Même titre, même artiste des deux côtés : le doublon de la mesure du .18.
const DOUBLE_TITRE: &str = "Abacab";
const DOUBLE_ARTISTE: &str = "Genesis";
/// Un album distant sans contrepartie locale : il doit rester trouvé.
const ORPHELIN_TITRE: &str = "Abacab Revisited";
const ORPHELIN_ARTISTE: &str = "The Musical Box";
/// Label porté par le local ET par son double : un seul album visible.
const LABEL: &str = "Charisma Abacab";

struct Banc {
    album_local: i64,
    album_double: i64,
    album_orphelin: i64,
    piste_locale: i64,
    piste_double: i64,
    piste_orpheline: i64,
}

fn poser_le_banc(etat: &AppState) -> Banc {
    let artistes = ArtistRepo::with_backend(etat.backend.clone());
    let albums = AlbumRepo::with_backend(etat.backend.clone());
    let pistes = TrackRepo::with_backend(etat.backend.clone());
    let creer = |titre: &str, artiste: &str, source: &str, label: Option<&str>| -> (i64, i64) {
        let artiste_id = artistes
            .get_or_create(artiste, None, None)
            .unwrap_or_else(|e| panic!("artiste {artiste} : {e}"))
            .id;
        let mut album = Album::new(titre.to_string());
        album.artist_id = artiste_id;
        album.source = source.to_string();
        album.track_count = Some(1);
        album.label = label.map(str::to_string);
        if source != "local" {
            album.source_id = Some(format!("upnp:{titre}"));
        }
        let album_id = albums
            .create(&album)
            .unwrap_or_else(|e| panic!("album {titre} ({source}) : {e}"));
        let mut piste = Track::new(format!("{titre} (piste)"));
        piste.album_id = Some(album_id);
        piste.artist_id = artiste_id;
        piste.source = source.to_string();
        if source == "local" {
            piste.file_path = Some(format!("/musique/{titre}.flac"));
        } else {
            piste.source_id = Some(format!("upnp:{titre}:1"));
        }
        let piste_id = pistes
            .create(&piste)
            .unwrap_or_else(|e| panic!("piste de {titre} ({source}) : {e}"));
        (album_id, piste_id)
    };
    let (album_local, piste_locale) = creer(DOUBLE_TITRE, DOUBLE_ARTISTE, "local", Some(LABEL));
    let (album_double, piste_double) = creer(DOUBLE_TITRE, DOUBLE_ARTISTE, "upnp", Some(LABEL));
    let (album_orphelin, piste_orpheline) = creer(ORPHELIN_TITRE, ORPHELIN_ARTISTE, "upnp", None);
    Banc {
        album_local,
        album_double,
        album_orphelin,
        piste_locale,
        piste_double,
        piste_orpheline,
    }
}

async fn corps_de(app: &Router, chemin: &str) -> Value {
    let reponse = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap_or_else(|e| panic!("{chemin} : routeur en échec : {e}"));
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap_or_else(|e| panic!("{chemin} : corps illisible : {e}"));
    assert_eq!(
        statut,
        StatusCode::OK,
        "{chemin} : statut {statut}, corps {}",
        String::from_utf8_lossy(&octets)
    );
    serde_json::from_slice(&octets).unwrap_or_else(|e| panic!("{chemin} : JSON illisible : {e}"))
}

fn ids(corps: &Value, cle: &str) -> Vec<i64> {
    let mut ids: Vec<i64> = corps[cle]
        .as_array()
        .unwrap_or_else(|| panic!("`{cle}` doit être un tableau — {corps}"))
        .iter()
        .filter_map(|v| v["id"].as_i64())
        .collect();
    ids.sort_unstable();
    ids
}

#[tokio::test]
async fn la_recherche_ne_rend_que_le_local_d_un_album_double() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    let banc = poser_le_banc(&etat);
    let app = tune_server::routes::router(etat);
    let corps = corps_de(&app, &format!("/api/v1/library/search?q={SAISIE}&limit=50")).await;

    let albums = ids(&corps, "albums");
    assert!(
        albums.contains(&banc.album_local),
        "l'album LOCAL doit être trouvé — albums rendus {albums:?}"
    );
    assert!(
        !albums.contains(&banc.album_double),
        "le double UPnP (id {}) d'un album local ne doit PAS sortir de la recherche, \
         comme il ne sort pas de la grille (#4146) — albums rendus {albums:?}",
        banc.album_double
    );
    assert_eq!(
        corps["totals"]["albums"].as_i64(),
        Some(albums.len() as i64),
        "le total d'albums doit compter ce que la page rend — {corps}"
    );

    let pistes = ids(&corps, "tracks");
    assert!(
        pistes.contains(&banc.piste_locale),
        "la piste LOCALE doit être trouvée — pistes rendues {pistes:?}"
    );
    assert!(
        !pistes.contains(&banc.piste_double),
        "la piste de l'album UPnP doublé (id {}) ne doit PAS sortir — pistes rendues {pistes:?}",
        banc.piste_double
    );
    assert_eq!(
        corps["totals"]["tracks"].as_i64(),
        Some(pistes.len() as i64),
        "le total de pistes doit compter ce que la page rend — {corps}"
    );

    let labels = corps["labels"]
        .as_array()
        .unwrap_or_else(|| panic!("`labels` doit être un tableau — {corps}"));
    let label = labels
        .iter()
        .find(|l| l["name"] == LABEL)
        .unwrap_or_else(|| panic!("le label {LABEL} doit être trouvé — {labels:?}"));
    assert_eq!(
        label["album_count"].as_i64(),
        Some(1),
        "le label ne compte que l'album visible, pas son double UPnP — {label}"
    );
}

#[tokio::test]
async fn un_album_upnp_sans_double_local_reste_trouve() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état serveur isolé");
    let banc = poser_le_banc(&etat);
    let app = tune_server::routes::router(etat);
    let corps = corps_de(&app, &format!("/api/v1/library/search?q={SAISIE}&limit=50")).await;

    let albums = ids(&corps, "albums");
    assert!(
        albums.contains(&banc.album_orphelin),
        "un album UPnP SANS contrepartie locale doit rester trouvé — albums rendus {albums:?}"
    );
    let pistes = ids(&corps, "tracks");
    assert!(
        pistes.contains(&banc.piste_orpheline),
        "la piste d'un album UPnP orphelin doit rester trouvée — pistes rendues {pistes:?}"
    );
    // Plancher : le banc attrape bien les trois albums par la saisie, sans
    // quoi « le double ne sort pas » pourrait n'être qu'une recherche vide.
    assert_eq!(
        albums.len(),
        2,
        "exactement le local et l'orphelin — albums rendus {albums:?}"
    );
}
