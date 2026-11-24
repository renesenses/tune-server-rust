//! Fil du forum 3028, suite — les données d'un Tune qui s'est déjà indexé.
//!
//! Le premier correctif empêche une NOUVELLE auto-indexation. Une installation
//! touchée garde pourtant ses doublons : une source abonnée à son propre
//! serveur, et des pistes `upnp` qui reflètent chaque piste locale, parfois
//! mises en favori ou en playlist entre-temps.
//!
//! Trois témoins, tous par l'API réelle :
//!
//! 1. `GET /network/media-servers` dit `this_server` : vrai pour notre
//!    annonce, faux pour un serveur tiers.
//! 2. Une source déjà abonnée à ce Tune passe par `run_one` vers l'état
//!    REFUSÉ `this_server`, DÉSACTIVÉE (plus de relance horaire).
//! 3. Le retrait (`POST /network/library-sources`, action `remove`, en deux
//!    temps) rattache favoris et playlists aux pistes locales équivalentes,
//!    puis retire les pistes et l'album `upnp`. CONTRE-TÉMOIN : aucune piste
//!    ni aucun album local n'est touché.
//!
//! CONTRE-ÉPREUVE (mesurée) : sur le lot sans ces correctifs, les trois
//! rougissent (champ absent, source `unavailable` et toujours active, action
//! inconnue).

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_core::db::media_server_repo::{MediaServerRepo, ObservationServeurRecue};
use tune_server::state::AppState;

const TIERS: &str = "uuid:0b1c2d3e-0000-4000-8000-0000000a3e51";

async fn appeler(
    app: &Router,
    methode: &str,
    chemin: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("content-type", "application/json");
    let requete = match corps {
        Some(c) => requete.body(Body::from(c.to_string())),
        None => requete.body(Body::empty()),
    }
    .unwrap();
    let reponse = app.clone().oneshot(requete).await.expect("routeur");
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .expect("corps");
    (
        statut,
        serde_json::from_slice(&octets)
            .unwrap_or_else(|_| json!({ "brut": String::from_utf8_lossy(&octets).to_string() })),
    )
}

fn exec(etat: &AppState, sql: &str, params: &[&dyn ToSqlValue]) {
    etat.backend.execute(sql, params).expect(sql);
}

fn entier(etat: &AppState, sql: &str, params: &[&dyn ToSqlValue]) -> i64 {
    etat.backend
        .query_one(sql, params)
        .expect(sql)
        .and_then(|r| r[0].as_i64())
        .unwrap_or(-1)
}

fn observer(etat: &AppState, udn: &str, hote: &str, port: u16, location: &str) {
    MediaServerRepo::with_backend(etat.backend.clone())
        .enregistrer_observation(&ObservationServeurRecue {
            udn: udn.into(),
            name: "Mac-de-test.fritz.box".into(),
            manufacturer: Some("Tune".into()),
            model: Some("Tune Server".into()),
            device_type: "urn:schemas-upnp-org:device:MediaServer:1".into(),
            location: location.into(),
            content_directory_url: Some(format!("{location}/control")),
            host: Some(hote.into()),
            port: Some(i64::from(port)),
            max_age_secs: Some(1800),
        })
        .expect("observation");
}

/// Une source abonnée, telle que `synchronisation_upnp` la persiste.
fn abonner(etat: &AppState, cle: &str, udn: &str, statut: &str) {
    let corps = json!({
        "key": cle, "udn": udn, "container": "0", "name": "Mac-de-test.fritz.box",
        "enabled": true, "status": statut, "last_attempt": 0, "last_success": null,
        "report": {}, "generation": "", "pending": []
    })
    .to_string();
    exec(
        etat,
        "INSERT INTO upnp_library_sources (source_key, udn, container, state_json) VALUES (?, ?, '0', ?)",
        &[&cle, &udn, &corps],
    );
}

async fn source(app: &Router, cle: &str) -> Value {
    let (_, corps) = appeler(app, "GET", "/api/v1/network/library-sources", None).await;
    corps["items"]
        .as_array()
        .and_then(|v| v.iter().find(|s| s["key"] == cle).cloned())
        .unwrap_or(Value::Null)
}

#[tokio::test(flavor = "multi_thread")]
async fn la_liste_des_serveurs_dit_this_server() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état isolé");
    let notre_udn = etat.upnp.as_ref().expect("état UPnP").uuid.clone();
    let port = etat.config.port;
    observer(
        &etat,
        &notre_udn,
        "127.0.0.1",
        port,
        &tune_core::upnp_server::advert_location("127.0.0.1", port),
    );
    observer(
        &etat,
        TIERS,
        "192.0.2.20",
        9790,
        "http://192.0.2.20:9790/desc/device.xml",
    );
    let app = tune_server::routes::router(etat.clone());
    let (statut, corps) = appeler(&app, "GET", "/api/v1/network/media-servers", None).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let items = corps["items"].as_array().expect("items");
    let de = |udn: &str| {
        items
            .iter()
            .find(|i| i["id"] == udn)
            .unwrap_or_else(|| panic!("{udn} absent : {corps}"))["this_server"]
            .clone()
    };
    assert_eq!(de(&notre_udn), json!(true), "{corps}");
    assert_eq!(de(TIERS), json!(false), "{corps}");
}

#[tokio::test(flavor = "multi_thread")]
async fn une_source_abonnee_a_ce_tune_est_refusee_et_desactivee() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état isolé");
    let notre_udn = etat.upnp.as_ref().expect("état UPnP").uuid.clone();
    abonner(&etat, "soi", &notre_udn, "ready");
    let app = tune_server::routes::router(etat.clone());

    // « Synchroniser » relance `run_one`, comme la boucle horaire.
    let (statut, corps) = appeler(
        &app,
        "POST",
        "/api/v1/network/library-sources",
        Some(json!({ "key": "soi", "action": "sync" })),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let mut vue = Value::Null;
    for _ in 0..100 {
        vue = source(&app, "soi").await;
        if !matches!(vue["status"].as_str(), Some("pending") | Some("running")) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(vue["status"], json!("this_server"), "{vue}");
    assert_eq!(vue["enabled"], json!(false), "plus de relance : {vue}");
    assert_eq!(
        entier(
            &etat,
            "SELECT COUNT(*) FROM tracks WHERE source = 'upnp'",
            &[]
        ),
        0
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn le_retrait_rattache_les_liens_et_epargne_le_local() {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état isolé");
    let udn = etat.upnp.as_ref().expect("état UPnP").uuid.clone();
    let p = |s: &str| format!("{udn}|{s}");

    // La bibliothèque LOCALE : un album, deux pistes, plus un album sans
    // reflet qui sert de contre-témoin.
    exec(
        &etat,
        "INSERT INTO artists (id, name) VALUES (1, 'Oasis')",
        &[],
    );
    exec(
        &etat,
        "INSERT INTO albums (id, title, artist_id, source) VALUES \
         (10, 'Morning Glory', 1, 'local'), (11, 'Definitely Maybe', 1, 'local')",
        &[],
    );
    exec(
        &etat,
        "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, file_path, source) VALUES \
         (100, 'Wonderwall', 10, 1, 258000, '/m/mg/03.flac', 'local'), \
         (101, 'Champagne Supernova', 10, 1, 451000, '/m/mg/12.flac', 'local'), \
         (102, 'Supersonic', 11, 1, 283000, '/m/dm/02.flac', 'local')",
        &[],
    );
    // Le REFLET indexé depuis ce Tune : même album, mêmes pistes, en `upnp`,
    // plus une piste sans équivalent local.
    exec(
        &etat,
        "INSERT INTO albums (id, title, artist_id, source, source_id) VALUES (20, 'Morning Glory', 1, 'upnp', ?)",
        &[&p("album")],
    );
    exec(
        &etat,
        "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, source, source_id) VALUES \
         (200, 'Wonderwall', 20, 1, 258400, 'upnp', ?), \
         (201, 'Champagne Supernova', 20, 1, 451000, 'upnp', ?), \
         (202, 'Piste fantome', 20, 1, 100000, 'upnp', ?)",
        &[&p("a"), &p("b"), &p("c")],
    );
    // 200 est rattachée par son URL de lecture, 201 par ses métadonnées.
    exec(
        &etat,
        "INSERT INTO track_metadata (track_id, key, value) VALUES (200, 'upnp_res_url', ?)",
        &[&"http://192.0.2.10:8888/api/v1/library/tracks/100/audio"],
    );
    // Les liens de l'utilisateur, posés sur le reflet.
    exec(
        &etat,
        "INSERT INTO favorites (profile_id, item_type, item_id) VALUES \
         (1, 'track', '200'), (1, 'album', '20'), (1, 'track', '202')",
        &[],
    );
    exec(
        &etat,
        "INSERT INTO playlists (id, name, profile_id) VALUES (7, 'Soir', 1)",
        &[],
    );
    exec(
        &etat,
        "INSERT INTO playlist_tracks (playlist_id, track_id, position) VALUES (7, 201, 0)",
        &[],
    );
    abonner(&etat, "soi", &udn, "this_server");
    let app = tune_server::routes::router(etat.clone());

    // 1. L'aperçu : rien n'est écrit.
    let (statut, apercu) = appeler(
        &app,
        "POST",
        "/api/v1/network/library-sources",
        Some(json!({ "key": "soi", "action": "remove" })),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{apercu}");
    assert_eq!(apercu["status"], json!("removal"), "{apercu}");
    assert_eq!(apercu["removal"]["pistes"], json!(3), "{apercu}");
    // Favori 200, playlist 201 : rattachés. Favori 202 et l'album (dont une
    // piste n'a pas d'équivalent) : sans équivalent, annoncés.
    assert_eq!(apercu["removal"]["liens_rattaches"], json!(2), "{apercu}");
    assert_eq!(
        apercu["removal"]["liens_sans_equivalent"],
        json!(2),
        "{apercu}"
    );
    assert_eq!(
        entier(
            &etat,
            "SELECT COUNT(*) FROM tracks WHERE source = 'upnp'",
            &[]
        ),
        3,
        "l'aperçu ne retire rien"
    );
    let generation = apercu["generation"]
        .as_str()
        .expect("génération")
        .to_string();

    // 2. La confirmation, avec le compte annoncé.
    let (statut, fait) = appeler(
        &app,
        "POST",
        "/api/v1/network/library-sources",
        Some(json!({ "key": "soi", "action": "remove", "generation": generation, "count": 3 })),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{fait}");
    assert_eq!(fait["status"], json!("removed"), "{fait}");

    // Le reflet est parti, la source aussi.
    assert_eq!(
        entier(
            &etat,
            "SELECT COUNT(*) FROM tracks WHERE source = 'upnp'",
            &[]
        ),
        0
    );
    assert_eq!(
        entier(
            &etat,
            "SELECT COUNT(*) FROM albums WHERE source = 'upnp'",
            &[]
        ),
        0
    );
    assert_eq!(
        entier(&etat, "SELECT COUNT(*) FROM upnp_library_sources", &[]),
        0
    );
    // Les liens ont suivi la piste locale.
    assert_eq!(
        entier(
            &etat,
            "SELECT COUNT(*) FROM favorites WHERE item_type = 'track' AND CAST(item_id AS TEXT) = '100'",
            &[]
        ),
        1,
        "le favori de piste est rattaché à la piste locale"
    );
    assert_eq!(
        entier(
            &etat,
            "SELECT track_id FROM playlist_tracks WHERE playlist_id = 7",
            &[]
        ),
        101,
        "l'entrée de playlist désigne la piste locale"
    );
    // CONTRE-TÉMOIN : le local est intact.
    assert_eq!(
        entier(
            &etat,
            "SELECT COUNT(*) FROM tracks WHERE id IN (100, 101, 102) AND source = 'local' AND file_path IS NOT NULL",
            &[]
        ),
        3,
        "aucune piste locale touchée"
    );
    assert_eq!(
        entier(
            &etat,
            "SELECT COUNT(*) FROM albums WHERE id IN (10, 11) AND source = 'local'",
            &[]
        ),
        2,
        "aucun album local touché"
    );
    assert_eq!(
        entier(
            &etat,
            "SELECT COUNT(*) FROM tracks WHERE album_id = 10",
            &[]
        ),
        2
    );
}
