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
        &[&format!(
            "http://127.0.0.1:{}/api/v1/library/tracks/100/audio",
            etat.config.port
        )],
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

// ---------------------------------------------------------------------------
// Revue de #9467 : les gardes du rattachement et de la confirmation.
// ---------------------------------------------------------------------------

/// Un banc minimal : une piste locale « Wonderwall » (id 100, album 10), et
/// une piste distante (id 200, album 20) de ce Tune, mise en favori. Le
/// témoin règle ce qui diffère.
struct Banc {
    etat: AppState,
    app: Router,
}

fn banc(
    distante_titre: &str,
    distante_artiste: Option<&str>,
    distante_album: Option<&str>,
    distante_duree: i64,
    url: Option<String>,
    homonyme: bool,
) -> Banc {
    banc_de(
        None,
        distante_titre,
        distante_artiste,
        distante_album,
        distante_duree,
        url,
        homonyme,
    )
}

/// Le même banc, la piste distante venant de `source_udn` (notre UDN si
/// `None`).
fn banc_de(
    source_udn: Option<&str>,
    distante_titre: &str,
    distante_artiste: Option<&str>,
    distante_album: Option<&str>,
    distante_duree: i64,
    url: Option<String>,
    homonyme: bool,
) -> Banc {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état isolé");
    let udn = source_udn
        .map(str::to_string)
        .unwrap_or_else(|| etat.upnp.as_ref().expect("état UPnP").uuid.clone());
    exec(
        &etat,
        "INSERT INTO artists (id, name) VALUES (1, 'Oasis'), (2, 'Inconnu')",
        &[],
    );
    exec(
        &etat,
        "INSERT INTO albums (id, title, artist_id, source) VALUES (10, 'Morning Glory', 1, 'local')",
        &[],
    );
    exec(
        &etat,
        "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, file_path, source) VALUES \
         (100, 'Wonderwall', 10, 1, 258000, '/m/mg/03.flac', 'local'), \
         (101, 'Champagne Supernova', 10, 1, 451000, '/m/mg/12.flac', 'local')",
        &[],
    );
    if homonyme {
        exec(
            &etat,
            "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, file_path, source) VALUES \
             (103, 'Wonderwall', 10, 1, 258000, '/m/copie/03.flac', 'local')",
            &[],
        );
    }
    let artiste = distante_artiste.map(|_| 1i64);
    let album_source = format!("{udn}|album");
    if let Some(titre) = distante_album {
        exec(
            &etat,
            "INSERT INTO albums (id, title, artist_id, source, source_id) VALUES (20, ?, 1, 'upnp', ?)",
            &[&titre, &album_source],
        );
    }
    let album = distante_album.map(|_| 20i64);
    let source_id = format!("{udn}|a");
    exec(
        &etat,
        "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, source, source_id) \
         VALUES (200, ?, ?, ?, ?, 'upnp', ?)",
        &[
            &distante_titre,
            &album,
            &artiste,
            &distante_duree,
            &source_id,
        ],
    );
    if let Some(url) = url {
        exec(
            &etat,
            "INSERT INTO track_metadata (track_id, key, value) VALUES (200, 'upnp_res_url', ?)",
            &[&url],
        );
    }
    exec(
        &etat,
        "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1, 'track', '200')",
        &[],
    );
    abonner(&etat, "soi", &udn, "this_server");
    let app = tune_server::routes::router(etat.clone());
    Banc { etat, app }
}

async fn apercu_du_retrait(b: &Banc) -> Value {
    let (statut, corps) = appeler(
        &b.app,
        "POST",
        "/api/v1/network/library-sources",
        Some(json!({ "key": "soi", "action": "remove" })),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    corps
}

async fn confirmer(b: &Banc, generation: &str, compte: u64) -> (StatusCode, Value) {
    appeler(
        &b.app,
        "POST",
        "/api/v1/network/library-sources",
        Some(
            json!({ "key": "soi", "action": "remove", "generation": generation, "count": compte }),
        ),
    )
    .await
}

fn favoris_sur_le_local(b: &Banc) -> i64 {
    entier(
        &b.etat,
        "SELECT COUNT(*) FROM favorites WHERE item_type = 'track' \
         AND CAST(item_id AS TEXT) IN ('100', '101', '103', '104')",
        &[],
    )
}

/// Aperçu puis confirmation : la piste distante n'est rattachée à RIEN, le cas
/// est annoncé, et aucun favori n'arrive sur une piste locale.
async fn aucun_rattachement(b: Banc, cas: &str) {
    let apercu = apercu_du_retrait(&b).await;
    assert_eq!(
        apercu["removal"]["liens_rattaches"],
        json!(0),
        "{cas} : {apercu}"
    );
    assert_eq!(
        apercu["removal"]["liens_sans_equivalent"],
        json!(1),
        "{cas} : le cas doit être annoncé — {apercu}"
    );
    let generation = apercu["generation"].as_str().unwrap().to_string();
    let (statut, fait) = confirmer(&b, &generation, 1).await;
    assert_eq!(statut, StatusCode::OK, "{cas} : {fait}");
    assert_eq!(favoris_sur_le_local(&b), 0, "{cas} : aucun rattachement");
    assert_eq!(
        entier(
            &b.etat,
            "SELECT COUNT(*) FROM tracks WHERE source = 'local'",
            &[]
        ),
        entier(&b.etat, "SELECT COUNT(*) FROM tracks", &[]),
        "{cas} : le local reste"
    );
}

fn notre_url(etat_port: u16, id: i64) -> String {
    format!("http://127.0.0.1:{etat_port}/api/v1/library/tracks/{id}/audio")
}

#[tokio::test(flavor = "multi_thread")]
async fn homonymes_aucun_rattachement() {
    let b = banc(
        "Wonderwall",
        Some("Oasis"),
        Some("Morning Glory"),
        258000,
        None,
        true,
    );
    aucun_rattachement(b, "homonymes").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn duree_au_dela_de_deux_secondes_aucun_rattachement() {
    let b = banc(
        "Wonderwall",
        Some("Oasis"),
        Some("Morning Glory"),
        261000,
        None,
        false,
    );
    aucun_rattachement(b, "durée +3 s").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn titre_seul_aucun_rattachement() {
    // Ni artiste, ni album, ni durée connus, côté distant comme sur une piste
    // locale de même titre : le titre seul ne prouve rien.
    let b = banc("Wonderwall", None, None, 0, None, false);
    exec(
        &b.etat,
        "INSERT INTO tracks (id, title, duration_ms, file_path, source) VALUES \
         (104, 'Wonderwall', 0, '/m/vrac/wonderwall.flac', 'local')",
        &[],
    );
    aucun_rattachement(b, "titre seul").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn url_d_un_tune_tiers_aucun_rattachement() {
    // Mêmes étiquettes que la piste locale 100, mais l'URL nomme la piste 100
    // d'un AUTRE Tune : c'est une autre bibliothèque.
    let b = banc(
        "Wonderwall",
        Some("Oasis"),
        Some("Morning Glory"),
        258000,
        Some("http://192.0.2.77:8888/api/v1/library/tracks/100/audio".into()),
        false,
    );
    // Cet autre Tune est CONNU : il figure au registre sous son propre UDN.
    observer(
        &b.etat,
        "uuid:00000000-aaaa-bbbb-cccc-0000000000a7",
        "192.0.2.77",
        8888,
        "http://192.0.2.77:8888/upnp/description.xml",
    );
    aucun_rattachement(b, "Tune tiers connu").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hote_inconnu_et_piste_concordante_unique_rattache() {
    // L'URL porte un nom d'hôte que nous ne reconnaissons pas (IP DHCP
    // changée, port redirigé, nom au lieu de l'IP) et qu'aucun serveur du
    // registre ne porte : c'est le plus souvent nous-mêmes. L'identifiant de
    // l'URL est ignoré, la piste locale concordante unique est retenue.
    let b = banc(
        "Wonderwall",
        Some("Oasis"),
        Some("Morning Glory"),
        258000,
        Some("http://mac-de-test.fritz.box:8888/api/v1/library/tracks/999/audio".into()),
        false,
    );
    let apercu = apercu_du_retrait(&b).await;
    assert_eq!(apercu["removal"]["liens_rattaches"], json!(1), "{apercu}");
    assert_eq!(
        apercu["removal"]["liens_sans_equivalent"],
        json!(0),
        "{apercu}"
    );
    let generation = apercu["generation"].as_str().unwrap().to_string();
    let (statut, fait) = confirmer(&b, &generation, 1).await;
    assert_eq!(statut, StatusCode::OK, "{fait}");
    assert_eq!(
        entier(
            &b.etat,
            "SELECT COUNT(*) FROM favorites WHERE item_type = 'track' AND CAST(item_id AS TEXT) = '100'",
            &[]
        ),
        1,
        "le favori suit la piste locale concordante"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn l_ancien_retrait_par_serveur_n_efface_pas_les_favoris_de_ce_tune() {
    // `DELETE /media-servers/{udn}/bibliotheque` (#4624) efface les favoris
    // sans les rattacher. Sur notre propre UDN, il est refusé (409) et renvoie
    // vers l'action `remove` ; le favori reste.
    let b = banc(
        "Wonderwall",
        Some("Oasis"),
        Some("Morning Glory"),
        258000,
        None,
        false,
    );
    let udn = b.etat.upnp.as_ref().unwrap().uuid.clone();
    let (statut, corps) = appeler(
        &b.app,
        "DELETE",
        &format!("/api/v1/network/media-servers/{udn}/bibliotheque?pistes=1"),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::CONFLICT, "{corps}");
    assert_eq!(corps["code"], json!("this_server_use_remove"), "{corps}");
    assert_eq!(
        entier(
            &b.etat,
            "SELECT COUNT(*) FROM favorites WHERE CAST(item_id AS TEXT) = '200'",
            &[]
        ),
        1,
        "le favori n'est pas effacé"
    );
    assert_eq!(
        entier(
            &b.etat,
            "SELECT COUNT(*) FROM tracks WHERE source = 'upnp'",
            &[]
        ),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn notre_url_mais_autres_metadonnees_aucun_rattachement() {
    // L'URL est bien la nôtre, mais elle nomme la piste 101 (« Champagne
    // Supernova ») alors que la distante dit « Wonderwall » : on ne suit ni
    // l'URL ni les étiquettes.
    let etat_port = AppState::new(":memory:", 0, Default::default())
        .unwrap()
        .config
        .port;
    let b = banc(
        "Wonderwall",
        Some("Oasis"),
        Some("Morning Glory"),
        258000,
        Some(notre_url(etat_port, 101)),
        false,
    );
    aucun_rattachement(b, "notre URL, autres métadonnées").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn la_confirmation_refuse_un_compte_change_un_apercu_expire_et_un_rejeu() {
    // Compte différent : 409, rien n'est retiré.
    let b = banc(
        "Wonderwall",
        Some("Oasis"),
        Some("Morning Glory"),
        258000,
        None,
        false,
    );
    let apercu = apercu_du_retrait(&b).await;
    let generation = apercu["generation"].as_str().unwrap().to_string();
    let (statut, corps) = confirmer(&b, &generation, 7).await;
    assert_eq!(statut, StatusCode::CONFLICT, "compte changé : {corps}");
    assert_eq!(
        entier(
            &b.etat,
            "SELECT COUNT(*) FROM tracks WHERE source = 'upnp'",
            &[]
        ),
        1
    );

    // Favori ajouté entre l'aperçu et la confirmation : le plan a changé, 409,
    // et AUCUN lien n'est déplacé ni effacé.
    exec(
        &b.etat,
        "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1, 'album', '20')",
        &[],
    );
    let (statut, corps) = confirmer(&b, &generation, 1).await;
    assert_eq!(statut, StatusCode::CONFLICT, "favori ajouté : {corps}");
    assert_eq!(favoris_sur_le_local(&b), 0, "rien n'a bougé");
    assert_eq!(
        entier(
            &b.etat,
            "SELECT COUNT(*) FROM favorites WHERE CAST(item_id AS TEXT) IN ('200', '20')",
            &[]
        ),
        2,
        "rien n'est effacé"
    );

    // Aperçu expiré : 409.
    let apercu = apercu_du_retrait(&b).await;
    let generation = apercu["generation"].as_str().unwrap().to_string();
    let vieux = entier(&b.etat, "SELECT COUNT(*) FROM upnp_library_sources", &[]);
    assert_eq!(vieux, 1);
    let etat_json = b
        .etat
        .backend
        .query_one("SELECT state_json FROM upnp_library_sources", &[])
        .unwrap()
        .unwrap()[0]
        .as_string()
        .unwrap();
    let mut v: Value = serde_json::from_str(&etat_json).unwrap();
    v["last_attempt"] = json!(v["last_attempt"].as_i64().unwrap() - 7200);
    let texte = v.to_string();
    exec(
        &b.etat,
        "UPDATE upnp_library_sources SET state_json = ?",
        &[&texte],
    );
    let (statut, corps) = confirmer(&b, &generation, 1).await;
    assert_eq!(statut, StatusCode::CONFLICT, "aperçu expiré : {corps}");

    // Rejeu : une confirmation valide, puis la même une seconde fois.
    let apercu = apercu_du_retrait(&b).await;
    let generation = apercu["generation"].as_str().unwrap().to_string();
    let (statut, corps) = confirmer(&b, &generation, 1).await;
    assert_eq!(statut, StatusCode::OK, "confirmation : {corps}");
    let (statut, corps) = confirmer(&b, &generation, 1).await;
    assert_eq!(statut, StatusCode::CONFLICT, "rejeu : {corps}");
}

#[tokio::test(flavor = "multi_thread")]
async fn notre_serveur_annonce_en_ipv6_est_reconnu() {
    // Une annonce de CE Tune sur la boucle IPv6, sous un autre UDN (base
    // remise à zéro) : l'adresse seule doit le reconnaître.
    let etat = AppState::new(":memory:", 0, Default::default()).expect("état isolé");
    let port = etat.config.port;
    let id = "uuid:5e1f0000-0000-4000-8000-0000000036a6";
    etat.media_servers.lock().await.insert(
        id.into(),
        tune_core::discovery::ssdp::MediaServerInfo {
            id: id.into(),
            name: "Mac-de-test.fritz.box".into(),
            manufacturer: "Tune".into(),
            model: "Tune Server".into(),
            location: format!("http://[::1]:{port}/upnp/description.xml"),
            content_directory_url: "http://127.0.0.1:9/control".into(),
            host: "::1".into(),
            port,
            last_seen: std::time::Instant::now(),
            max_age: std::time::Duration::from_secs(1800),
        },
    );
    let app = tune_server::routes::router(etat.clone());
    let (statut, corps) = appeler(
        &app,
        "POST",
        &format!("/api/v1/network/media-servers/{id}/indexer"),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["code"], json!("this_server"), "{corps}");
}

const AUTRE_TUNE: &str = "uuid:00000000-aaaa-bbbb-cccc-0000000000b9";
const ANCIEN_UDN: &str = "uuid:5e1f0000-0000-4000-8000-0000000a1d00";

#[tokio::test(flavor = "multi_thread")]
async fn autre_tune_a_l_hote_perime_aucun_rattachement() {
    // La source est un AUTRE Tune ; son URL porte un hôte périmé (IP changée
    // par DHCP) qu'aucun serveur du registre ne porte, et une piste locale
    // a exactement les mêmes étiquettes. Ce n'est pas nous : aucune.
    let b = banc_de(
        Some(AUTRE_TUNE),
        "Wonderwall",
        Some("Oasis"),
        Some("Morning Glory"),
        258000,
        Some("http://192.0.2.99:8888/api/v1/library/tracks/100/audio".into()),
        false,
    );
    aucun_rattachement(b, "autre Tune, hôte périmé").await;
}

async fn supprimer_par_serveur(b: &Banc, udn: &str) -> (StatusCode, Value) {
    appeler(
        &b.app,
        "DELETE",
        &format!("/api/v1/network/media-servers/{udn}/bibliotheque?pistes=1"),
        None,
    )
    .await
}

fn favori_200(b: &Banc) -> i64 {
    entier(
        &b.etat,
        "SELECT COUNT(*) FROM favorites WHERE CAST(item_id AS TEXT) = '200'",
        &[],
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn ancien_udn_au_registre_a_notre_adresse_rend_409() {
    let b = banc_de(
        Some(ANCIEN_UDN),
        "Wonderwall",
        Some("Oasis"),
        Some("Morning Glory"),
        258000,
        None,
        false,
    );
    let port = b.etat.config.port;
    observer(
        &b.etat,
        ANCIEN_UDN,
        "127.0.0.1",
        port,
        &tune_core::upnp_server::advert_location("127.0.0.1", port),
    );
    let (statut, corps) = supprimer_par_serveur(&b, ANCIEN_UDN).await;
    assert_eq!(statut, StatusCode::CONFLICT, "{corps}");
    assert_eq!(favori_200(&b), 1, "le favori reste");
}

#[tokio::test(flavor = "multi_thread")]
async fn ancien_udn_disparu_du_registre_reconnu_par_ses_url_rend_409() {
    // L'ancien UDN n'est plus au registre ; ses pistes ont été servies par
    // NOTRE route de lecture : c'est nous, le retrait par serveur est refusé.
    let port = AppState::new(":memory:", 0, Default::default())
        .unwrap()
        .config
        .port;
    let b = banc_de(
        Some(ANCIEN_UDN),
        "Wonderwall",
        Some("Oasis"),
        Some("Morning Glory"),
        258000,
        Some(notre_url(port, 100)),
        false,
    );
    let (statut, corps) = supprimer_par_serveur(&b, ANCIEN_UDN).await;
    assert_eq!(statut, StatusCode::CONFLICT, "{corps}");
    assert_eq!(favori_200(&b), 1, "le favori reste");
}
