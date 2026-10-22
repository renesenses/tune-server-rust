//! `GET /library/artists?provenances=1` — tune-web-client#1716.
//!
//! L'onglet Artistes du client web chargeait toute la bibliothèque (albums ET
//! pistes) pour savoir de quelles sources vient chaque artiste. Le serveur
//! rend désormais ces sources avec la page d'artistes.
//!
//! L'épreuve ne recopie pas le SQL. Elle refait, sur les corps JSON de
//! `GET /library/albums` et `GET /library/tracks`, le calcul que faisait le
//! client (`sourcesParArtiste` + `provenanceDe`, `provenanceBibliotheque.ts`),
//! et exige que `provenances` rende EXACTEMENT les mêmes ensembles. Le banc porte
//! un local, deux serveurs UPnP, un `source_id` sans UDN, un album masqué, un
//! distant doublé par un local et un invité de compilation : chacun fait
//! diverger une implémentation qui l'oublierait.

use std::collections::{BTreeMap, BTreeSet};

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

async fn get_json(app: &Router, chemin: &str) -> Value {
    let reponse = app
        .clone()
        .oneshot(Request::get(chemin).body(Body::empty()).unwrap())
        .await
        .unwrap_or_else(|e| panic!("{chemin} : routeur en échec : {e}"));
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        statut,
        StatusCode::OK,
        "{chemin} : {}",
        String::from_utf8_lossy(&octets)
    );
    serde_json::from_slice(&octets).unwrap()
}

/// `provenanceDe` du client, mot pour mot.
fn provenance_de(item: &Value) -> String {
    let source = item["source"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("local");
    if source != "upnp" {
        return source.to_string();
    }
    let sid = item["source_id"].as_str().unwrap_or("");
    let mut parts = sid.split('|');
    let udn = parts.next().unwrap_or("").trim();
    let identite = parts.next().unwrap_or("");
    if !udn.is_empty() && !identite.is_empty() {
        format!("upnp:{udn}")
    } else {
        "upnp".into()
    }
}

fn poser_le_banc(etat: &AppState) {
    let artistes = ArtistRepo::with_backend(etat.backend.clone());
    let albums = AlbumRepo::with_backend(etat.backend.clone());
    let pistes = TrackRepo::with_backend(etat.backend.clone());
    let artiste = |nom: &str| artistes.get_or_create(nom, None, None).unwrap().id.unwrap();
    let mut n = 0;
    let album = |titre: &str, a: i64, source: &str, sid: Option<&str>| -> i64 {
        let mut al = Album::new(titre.into());
        al.artist_id = Some(a);
        al.source = source.into();
        al.source_id = sid.map(Into::into);
        albums.create(&al).unwrap()
    };
    let mut piste = |album_id: i64, a: i64, source: &str, sid: Option<&str>| {
        n += 1;
        let mut t = Track::new(format!("Piste {n}"));
        t.album_id = Some(album_id);
        t.artist_id = Some(a);
        t.track_number = n;
        t.duration_ms = 60_000;
        t.source = source.into();
        t.source_id = sid.map(Into::into);
        if source == "local" {
            t.file_path = Some(format!("/musique/{n}.flac"));
        }
        pistes.create(&t).unwrap();
    };
    let local = artiste("Local Seul");
    let mixte = artiste("Deux Sources");
    let invite = artiste("Invité");
    let masque = artiste("Masqué");
    let double = artiste("Doublé");

    let a = album("Local", local, "local", None);
    piste(a, local, "local", None);
    piste(a, invite, "local", None);
    let b = album("Mixte local", mixte, "local", None);
    piste(b, mixte, "local", None);
    let c = album("Mixte UPnP 1", mixte, "upnp", Some("uuid:serveur-1|obj-c"));
    piste(c, mixte, "upnp", Some("uuid:serveur-1|t-c1"));
    piste(c, invite, "upnp", Some("uuid:serveur-1|t-c2"));
    let d = album("Mixte UPnP 2", mixte, "upnp", Some("uuid:serveur-2|obj-d"));
    piste(d, mixte, "upnp", Some("uuid:serveur-2|t-d1"));
    let e = album("Sans UDN", mixte, "upnp", Some("sans-barre"));
    piste(e, mixte, "upnp", Some("sans-barre-t"));
    let f = album("Caché", masque, "local", None);
    piste(f, masque, "local", None);
    let g = album("Même disque", double, "local", None);
    piste(g, double, "local", None);
    let h = album("Même disque", double, "upnp", Some("uuid:serveur-1|obj-h"));
    piste(h, double, "upnp", Some("uuid:serveur-1|t-h1"));
    etat.backend
        .execute(
            &format!("INSERT INTO hidden_items (item_type, item_id) VALUES ('album', {f})"),
            &[],
        )
        .unwrap();
}

#[tokio::test]
async fn les_sources_rendues_sont_celles_que_le_client_calculait() {
    let etat = AppState::new(":memory:", 0, Default::default()).unwrap();
    poser_le_banc(&etat);
    let app = tune_server::routes::router(etat);

    // Le calcul du client, sur les listes.
    let mut attendu: BTreeMap<i64, BTreeSet<String>> = BTreeMap::new();
    let albums = get_json(&app, "/api/v1/library/albums?limit=2000&offset=0").await;
    let pistes = get_json(&app, "/api/v1/library/tracks?limit=5000&offset=0").await;
    for item in albums["items"]
        .as_array()
        .unwrap()
        .iter()
        .chain(pistes["items"].as_array().unwrap())
    {
        if let Some(id) = item["artist_id"].as_i64() {
            attendu.entry(id).or_default().insert(provenance_de(item));
        }
    }
    assert!(
        attendu.values().any(|s| s.len() >= 3),
        "le banc doit porter un artiste à trois sources"
    );

    let page = get_json(
        &app,
        "/api/v1/library/artists?limit=2000&offset=0&provenances=1",
    )
    .await;
    let items = page["items"].as_array().unwrap();
    assert!(!items.is_empty());
    for a in items {
        let id = a["id"].as_i64().unwrap();
        let recu: BTreeSet<String> = a["provenances"]
            .as_object()
            .unwrap_or_else(|| panic!("artiste {id} sans `provenances` : {a}"))
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            recu,
            attendu.get(&id).cloned().unwrap_or_default(),
            "artiste {} ({id})",
            a["name"]
        );
    }
    // Les comptes : « Deux Sources » a un album et une piste chez serveur-1.
    let mixte = items.iter().find(|a| a["name"] == "Deux Sources").unwrap();
    assert_eq!(
        mixte["provenances"]["upnp:uuid:serveur-1"],
        serde_json::json!({"albums": 1, "pistes": 1})
    );
}

#[tokio::test]
async fn sans_le_parametre_la_reponse_est_celle_d_avant() {
    let etat = AppState::new(":memory:", 0, Default::default()).unwrap();
    poser_le_banc(&etat);
    let app = tune_server::routes::router(etat);
    let page = get_json(&app, "/api/v1/library/artists?limit=2000&offset=0").await;
    let items = page["items"].as_array().unwrap();
    assert!(!items.is_empty());
    assert!(items.iter().all(|a| a.get("provenances").is_none()));
}
