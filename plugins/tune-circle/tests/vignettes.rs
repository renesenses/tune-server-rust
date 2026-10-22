//! Les vignettes des pochettes partagées avec le cercle (#6521).
//!
//! Contre un faux mozaiklabs local qui tient le contrat de site-mozaiklabs
//! (`GET|POST|DELETE /api/v1/cloud-library/{server_id}/thumbnails`, et la
//! lecture d'une vignette par un contact) : ce qui part, quand, et ce qui ne
//! part jamais.

mod commun;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine as _;
use serde_json::{Value, json};
use tune_circle::vignettes::{self, Vignettes};
use tune_core::cloud::library_sync::{CLE_JETON_DE_LIAISON, CLE_PARTAGE_DE_CERCLE};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::test_scratch::{ScratchDir, scratch_dir};

use commun::{JETON, appel, appel_avec_entetes, base};

const SERVEUR: &str = "srv-vignettes-6521";
const LIAISON: &str = "jeton-de-liaison-6521";
const CONTACT: &str = "7";

#[derive(Default)]
struct Faux {
    /// album_id → (sha256, source, octets)
    tenues: HashMap<i64, (String, Option<String>, Vec<u8>)>,
    manifestes: usize,
    /// Chaque POST reçu : ses éléments, tels quels.
    envois: Vec<Vec<Value>>,
    effacements: usize,
    /// Au-delà, `thumbnail_quota_exceeded`.
    quota: Option<usize>,
    /// Jetons vus sur chaque appel du serveur : (Bearer, X-Tune-Server-Token).
    jetons: Vec<(String, String)>,
}

type Partage = Arc<Mutex<Faux>>;

fn noter_jetons(f: &mut Faux, h: &HeaderMap) {
    let lire = |n: &str| {
        h.get(n)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    f.jetons
        .push((lire("authorization"), lire("x-tune-server-token")));
}

async fn manifeste(State(e): State<Partage>, h: HeaderMap) -> Response {
    let mut f = e.lock().unwrap();
    noter_jetons(&mut f, &h);
    f.manifestes += 1;
    let m: serde_json::Map<String, Value> = f
        .tenues
        .iter()
        .map(|(id, (sha, src, _))| (id.to_string(), json!({ "sha256": sha, "source": src })))
        .collect();
    axum::Json(json!({ "thumbnails": m })).into_response()
}

async fn recevoir(
    State(e): State<Partage>,
    h: HeaderMap,
    axum::Json(c): axum::Json<Value>,
) -> Response {
    let mut f = e.lock().unwrap();
    noter_jetons(&mut f, &h);
    let items = c["thumbnails"].as_array().cloned().unwrap_or_default();
    f.envois.push(items.clone());
    let (mut acceptes, mut refus) = (vec![], vec![]);
    for i in items {
        let id = i["album_id"].as_i64().unwrap();
        let sha = i["sha256"].as_str().unwrap().to_string();
        let src = i["source"].as_str().map(str::to_string);
        match i.get("data").and_then(Value::as_str) {
            Some(d) => {
                let o = base64::engine::general_purpose::STANDARD.decode(d).unwrap();
                assert_eq!(
                    vignettes::empreinte(&o),
                    sha,
                    "l'empreinte décrit les octets"
                );
                if f.quota
                    .is_some_and(|q| !f.tenues.contains_key(&id) && f.tenues.len() >= q)
                {
                    refus.push(json!({ "album_id": id, "error": "thumbnail_quota_exceeded" }));
                    continue;
                }
                f.tenues.insert(id, (sha, src, o));
                acceptes.push(id);
            }
            None => match f.tenues.get_mut(&id) {
                Some(t) if t.0 == sha => {
                    t.1 = src;
                    acceptes.push(id);
                }
                _ => refus.push(json!({ "album_id": id, "error": "data_required" })),
            },
        }
    }
    axum::Json(json!({ "accepted": acceptes, "rejected": refus })).into_response()
}

async fn effacer(State(e): State<Partage>, h: HeaderMap) -> Response {
    let mut f = e.lock().unwrap();
    noter_jetons(&mut f, &h);
    f.effacements += 1;
    let n = f.tenues.len();
    f.tenues.clear();
    axum::Json(json!({ "deleted": n })).into_response()
}

const IMAGE_DU_CONTACT: &[u8] = b"\xFF\xD8\xFF\xE0 octets-de-la-vignette";

async fn vignette_du_contact(Path((uid, album)): Path<(String, String)>, h: HeaderMap) -> Response {
    if uid != CONTACT || album != "10" {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": "not_found" })),
        )
            .into_response();
    }
    if album == "10"
        && h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok())
            != Some(&format!("Bearer {JETON}"))
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if h.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()) == Some("\"abc\"") {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, "\"abc\"")]).into_response();
    }
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            (header::ETAG, "\"abc\""),
        ],
        IMAGE_DU_CONTACT,
    )
        .into_response()
}

async fn page_html() -> Response {
    ([(header::CONTENT_TYPE, "text/html")], "<html>oups</html>").into_response()
}

struct Banc {
    base: String,
    etat: Partage,
}

async fn demarrer() -> Banc {
    let etat: Partage = Arc::new(Mutex::new(Faux::default()));
    let app = Router::new()
        .route(
            "/api/v1/cloud-library/{server}/thumbnails",
            get(manifeste).post(recevoir).delete(effacer),
        )
        .route(
            "/api/v1/circle/contacts/{uid}/library/albums/{album}/thumbnail",
            get(vignette_du_contact),
        )
        .route(
            "/api/v1/circle/contacts/8/library/albums/{album}/thumbnail",
            get(page_html),
        )
        .with_state(etat.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Banc {
        base: format!("http://{adresse}"),
        etat,
    }
}

/// Un JPEG de `l`×`h`, couleur `c`, avec un segment EXIF qui porte un secret.
fn jpeg_avec_exif(l: u32, h: u32, c: u8) -> Vec<u8> {
    let img = image::RgbImage::from_pixel(l, h, image::Rgb([c, 40, 200]));
    let mut brut = std::io::Cursor::new(Vec::new());
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut brut, 90)
        .encode(img.as_raw(), l, h, image::ExtendedColorType::Rgb8)
        .unwrap();
    let brut = brut.into_inner();
    let mut charge = b"Exif\0\0GPS-SECRET-6521".to_vec();
    let mut o = vec![0xFF, 0xD8, 0xFF, 0xE1];
    o.extend_from_slice(&((charge.len() + 2) as u16).to_be_bytes());
    o.append(&mut charge);
    o.extend_from_slice(&brut[2..]);
    o
}

/// Le cache de pochettes d'un essai : un dossier à lui, supprimé à la fin de
/// l'essai, panique comprise (`tune_core::test_scratch`, garde #3030).
fn dossier_de_pochettes() -> ScratchDir {
    scratch_dir("tune-circle-vignettes-6521")
}

fn condensat(n: u32) -> String {
    format!("{n:064x}")
}

/// Pose la pochette d'un album dans le cache, sous un condensat.
fn poser_pochette(dossier: &std::path::Path, cond: &str, octets: &[u8]) {
    std::fs::write(dossier.join(format!("{cond}.jpg")), octets).unwrap();
}

/// Une base prête à envoyer : session, liaison, partage de cercle, et `n`
/// albums (id 10, 11…) chacun avec une piste et une pochette en cache.
fn bibliotheque(banc: &Banc, dossier: &std::path::Path, n: u32) -> Arc<dyn DbBackend> {
    let backend = base(&banc.base, Some(JETON));
    let s = SettingsRepo::with_backend(backend.clone());
    s.set("server_id", SERVEUR).unwrap();
    s.set(CLE_JETON_DE_LIAISON, LIAISON).unwrap();
    s.set(CLE_PARTAGE_DE_CERCLE, "true").unwrap();
    for i in 0..n {
        let id = 10 + i as i64;
        let cond = condensat(id as u32);
        poser_pochette(dossier, &cond, &jpeg_avec_exif(300, 300, i as u8));
        backend
            .execute_batch(&format!(
                "INSERT INTO albums (id, title, cover_path) VALUES ({id}, 'Album {id}', '{cond}');\
                 INSERT INTO tracks (id, title, album_id) VALUES ({id}, 'Piste {id}', {id});"
            ))
            .unwrap();
    }
    backend
}

fn vignettes_de(banc: &Banc, backend: &Arc<dyn DbBackend>, dossier: &std::path::Path) -> Vignettes {
    Vignettes::new(
        backend.clone(),
        dossier.to_path_buf(),
        &format!("{}/api/v1/cloud-library", banc.base),
    )
    .sans_pause()
}

fn envoyes_avec_donnees(f: &Faux) -> usize {
    f.envois
        .iter()
        .flatten()
        .filter(|i| i.get("data").is_some())
        .count()
}

// 1. Le défaut et les conditions --------------------------------------------

#[test]
fn le_reglage_est_actif_par_defaut_et_seul_false_le_coupe() {
    let backend = base("http://127.0.0.1:9", None);
    let s = SettingsRepo::with_backend(backend);
    assert!(
        vignettes::reglage_actif(&s),
        "décision de Bertrand du 19/10 : activé par défaut"
    );
    s.set(vignettes::CLE_REGLAGE, "false").unwrap();
    assert!(!vignettes::reglage_actif(&s));
    s.set(vignettes::CLE_REGLAGE, "true").unwrap();
    assert!(vignettes::reglage_actif(&s));
}

#[tokio::test]
async fn sans_partage_de_cercle_rien_ne_part() {
    let banc = demarrer().await;
    let dossier = dossier_de_pochettes();
    let backend = bibliotheque(&banc, &dossier, 3);
    SettingsRepo::with_backend(backend.clone())
        .set(CLE_PARTAGE_DE_CERCLE, "false")
        .unwrap();
    let bilan = vignettes_de(&banc, &backend, &dossier).passage().await;
    assert!(bilan.rien);
    let f = banc.etat.lock().unwrap();
    assert_eq!(
        (f.manifestes, f.envois.len()),
        (0, 0),
        "aucun appel au cloud"
    );
}

#[tokio::test]
async fn reglage_coupe_rien_ne_part() {
    let banc = demarrer().await;
    let dossier = dossier_de_pochettes();
    let backend = bibliotheque(&banc, &dossier, 3);
    SettingsRepo::with_backend(backend.clone())
        .set(vignettes::CLE_REGLAGE, "false")
        .unwrap();
    let bilan = vignettes_de(&banc, &backend, &dossier).passage().await;
    assert!(bilan.rien);
    let f = banc.etat.lock().unwrap();
    assert_eq!((f.manifestes, f.envois.len(), f.effacements), (0, 0, 0));
}

// 2. Ce qui part -------------------------------------------------------------

#[tokio::test]
async fn chaque_album_part_en_vignette_carree_sans_exif_avec_son_empreinte() {
    let banc = demarrer().await;
    let dossier = dossier_de_pochettes();
    let backend = bibliotheque(&banc, &dossier, 2);
    // Un album à pochette DISTANTE (service) et un sans pochette : rien.
    backend
        .execute_batch(
            "INSERT INTO albums (id, title, cover_path) VALUES (50, 'Qobuz', 'https://static.qobuz.com/x.jpg');\
             INSERT INTO tracks (id, title, album_id) VALUES (50, 'q', 50);\
             INSERT INTO albums (id, title) VALUES (51, 'Sans pochette');\
             INSERT INTO tracks (id, title, album_id) VALUES (51, 's', 51);",
        )
        .unwrap();

    let bilan = vignettes_de(&banc, &backend, &dossier).passage().await;
    assert_eq!((bilan.envoyees, bilan.refusees), (2, 0));

    let f = banc.etat.lock().unwrap();
    let mut ids: Vec<i64> = f.tenues.keys().copied().collect();
    ids.sort();
    assert_eq!(ids, vec![10, 11]);
    for (sha, source, octets) in f.tenues.values() {
        assert_eq!(&vignettes::empreinte(octets), sha);
        let lue = image::load_from_memory(octets).unwrap();
        assert_eq!((lue.width(), lue.height()), (160, 160));
        assert!(
            !octets.windows(4).any(|w| w == b"Exif"),
            "aucun EXIF ne part"
        );
        assert!(!octets.windows(10).any(|w| w == b"GPS-SECRET"));
        assert!(octets.len() < 32 * 1024);
        assert_eq!(source.as_ref().map(String::len), Some(64));
    }
    // Ni chemin, ni condensat local, ni titre dans ce qui part.
    let brut = serde_json::to_string(&f.envois).unwrap();
    assert!(!brut.contains(&condensat(10)) && !brut.contains("Album 10"));
    // Chaque appel porte la session ET le jeton de liaison.
    assert!(
        f.jetons
            .iter()
            .all(|(b, l)| b == &format!("Bearer {JETON}") && l == LIAISON)
    );
}

#[tokio::test]
async fn seules_les_vignettes_nouvelles_ou_changees_repartent() {
    let banc = demarrer().await;
    let dossier = dossier_de_pochettes();
    let backend = bibliotheque(&banc, &dossier, 3);
    let v = vignettes_de(&banc, &backend, &dossier);
    assert_eq!(v.passage().await.envoyees, 3);

    // Même instance : rien de neuf, aucun envoi.
    let b = v.passage().await;
    assert!(b.rien, "{b:?}");
    // Un redémarrage relit le manifeste du cloud, et n'envoie rien non plus.
    let b = vignettes_de(&banc, &backend, &dossier).passage().await;
    assert!(b.rien, "{b:?}");
    {
        let f = banc.etat.lock().unwrap();
        assert_eq!(f.envois.len(), 1);
        assert_eq!(f.manifestes, 2);
    }

    // L'album 10 change de pochette (autre image) ; l'album 11 change de
    // condensat mais garde la MÊME image : seule sa source part.
    let neuf = condensat(1010);
    poser_pochette(&dossier, &neuf, &jpeg_avec_exif(250, 250, 222));
    let meme = condensat(1011);
    poser_pochette(&dossier, &meme, &jpeg_avec_exif(300, 300, 1));
    backend
        .execute_batch(&format!(
            "UPDATE albums SET cover_path = '{neuf}' WHERE id = 10;\
             UPDATE albums SET cover_path = '{meme}' WHERE id = 11;"
        ))
        .unwrap();
    let b = vignettes_de(&banc, &backend, &dossier).passage().await;
    assert_eq!((b.envoyees, b.sources_seules), (1, 1), "{b:?}");
    let f = banc.etat.lock().unwrap();
    let dernier = f.envois.last().unwrap();
    assert_eq!(dernier.len(), 2);
    let par_id: HashMap<i64, &Value> = dernier
        .iter()
        .map(|i| (i["album_id"].as_i64().unwrap(), i))
        .collect();
    assert!(par_id[&10].get("data").is_some());
    assert!(
        par_id[&11].get("data").is_none(),
        "même image : pas d'octets"
    );
    assert_eq!(envoyes_avec_donnees(&f), 4);
}

#[tokio::test]
async fn les_envois_partent_par_lots_bornes_et_le_quota_arrete_le_passage() {
    let banc = demarrer().await;
    let dossier = dossier_de_pochettes();
    let backend = bibliotheque(&banc, &dossier, 25);
    let b = vignettes_de(&banc, &backend, &dossier).passage().await;
    assert_eq!(b.envoyees, 25);
    {
        let f = banc.etat.lock().unwrap();
        let tailles: Vec<usize> = f.envois.iter().map(Vec::len).collect();
        assert_eq!(tailles, vec![vignettes::PAR_ENVOI, 5]);
    }

    let banc = demarrer().await;
    banc.etat.lock().unwrap().quota = Some(3);
    let dossier = dossier_de_pochettes();
    let backend = bibliotheque(&banc, &dossier, 45);
    let b = vignettes_de(&banc, &backend, &dossier).passage().await;
    assert_eq!((b.envoyees, b.refusees), (3, 17));
    assert_eq!(
        banc.etat.lock().unwrap().envois.len(),
        1,
        "le quota atteint, le passage s'arrête"
    );
}

// 3. Couper le réglage --------------------------------------------------------

#[tokio::test]
async fn couper_par_la_route_efface_chez_le_cloud_et_plus_rien_ne_part() {
    let banc = demarrer().await;
    let dossier = dossier_de_pochettes();
    let backend = bibliotheque(&banc, &dossier, 2);
    let v = Arc::new(vignettes_de(&banc, &backend, &dossier));
    assert_eq!(v.passage().await.envoyees, 2);

    let relais = Arc::new(tune_circle::relais::Relais::new(backend.clone()));
    let app = tune_circle::routes::router_des_vignettes(relais, v.clone());
    let r = appel(&app, "GET", "/artwork-sharing", None).await;
    assert_eq!(r.json(), json!({ "enabled": true, "default": true }));

    let r = appel(
        &app,
        "PUT",
        "/artwork-sharing",
        Some(json!({ "enabled": false })),
    )
    .await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.json(), json!({ "enabled": false, "purged": true }));
    {
        let f = banc.etat.lock().unwrap();
        assert_eq!(f.effacements, 1);
        assert!(f.tenues.is_empty());
    }
    let envois = banc.etat.lock().unwrap().envois.len();
    assert!(v.passage().await.rien);
    assert_eq!(
        banc.etat.lock().unwrap().envois.len(),
        envois,
        "coupé : rien ne part"
    );

    let r = appel(&app, "PUT", "/artwork-sharing", Some(json!({ "on": 1 }))).await;
    assert_eq!(r.statut, StatusCode::UNPROCESSABLE_ENTITY);

    // Rallumé : tout repart.
    appel(
        &app,
        "PUT",
        "/artwork-sharing",
        Some(json!({ "enabled": true })),
    )
    .await;
    assert_eq!(v.passage().await.envoyees, 2);
}

// 4. La lecture d'un contact ----------------------------------------------------

#[tokio::test]
async fn la_vignette_d_un_contact_est_relayee_avec_son_etag_et_304() {
    let banc = demarrer().await;
    let dossier = dossier_de_pochettes();
    let backend = bibliotheque(&banc, &dossier, 0);
    let relais = Arc::new(tune_circle::relais::Relais::new(backend.clone()));
    let app = tune_circle::routes::router_des_vignettes(
        relais,
        Arc::new(vignettes_de(&banc, &backend, &dossier)),
    );

    let r = appel(&app, "GET", "/contacts/7/library/albums/10/thumbnail", None).await;
    assert_eq!(r.statut, StatusCode::OK);
    assert_eq!(r.octets, IMAGE_DU_CONTACT);
    assert_eq!(r.entetes[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(r.entetes[header::ETAG], "\"abc\"");
    assert!(
        r.entetes[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .contains("private")
    );

    let r = appel_avec_entetes(
        &app,
        "GET",
        "/contacts/7/library/albums/10/thumbnail",
        None,
        &[("if-none-match", "\"abc\"")],
    )
    .await;
    assert_eq!(r.statut, StatusCode::NOT_MODIFIED);
    assert!(r.octets.is_empty());

    let r = appel(&app, "GET", "/contacts/7/library/albums/11/thumbnail", None).await;
    assert_eq!(r.statut, StatusCode::NOT_FOUND);

    // Autre chose qu'une image ne passe pas pour une image.
    let r = appel(&app, "GET", "/contacts/8/library/albums/10/thumbnail", None).await;
    assert_eq!(r.statut, StatusCode::SERVICE_UNAVAILABLE);
    assert!(!String::from_utf8_lossy(&r.octets).contains("<html>"));

    // Sans session : 412, rien ne part.
    SettingsRepo::with_backend(backend.clone())
        .delete("mozaik_access_token")
        .unwrap();
    let r = appel(&app, "GET", "/contacts/7/library/albums/10/thumbnail", None).await;
    assert_eq!(r.statut, StatusCode::PRECONDITION_FAILED);
}
