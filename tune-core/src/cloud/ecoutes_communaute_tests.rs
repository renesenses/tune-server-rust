//! #6738 — les écoutes de la communauté, éprouvées contre un faux site.
//!
//! Chaque test passe par [`annoter`], le seul chemin que les routes
//! appellent, et lit ce que le faux site a REÇU. Chaque faux site a sa propre
//! adresse : le cache, rangé par adresse du site, ne passe pas d'un test à
//! l'autre.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::*;
use crate::cloud::ecoutes_au_compte::CLE_ACTIVATION;
use crate::db::migrations;
use crate::db::sqlite::SqliteDb;

const SERVEUR: &str = "serveur-c2";

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn reglages(b: &Arc<dyn DbBackend>) -> SettingsRepo {
    SettingsRepo::with_backend(b.clone())
}

#[derive(Default)]
struct FauxSite {
    /// Corps reçus, dans l'ordre.
    recus: Vec<Value>,
    /// Titre (ou, pour un artiste, nom) → écoutes rendues. Absent : sous le
    /// seuil.
    nombres: HashMap<String, i64>,
    /// Statut imposé ; `None` : 200.
    statut: Option<u16>,
    /// Retard de la réponse.
    retard_ms: u64,
}

type Site = Arc<Mutex<FauxSite>>;

async fn faux_site() -> (String, Site) {
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::routing::post;
    use axum::{Json, Router};

    async fn compter(State(s): State<Site>, Json(c): Json<Value>) -> (StatusCode, Json<Value>) {
        let (retard, statut, nombres) = {
            let mut s = s.lock().unwrap();
            s.recus.push(c.clone());
            (s.retard_ms, s.statut, s.nombres.clone())
        };
        if retard > 0 {
            tokio::time::sleep(Duration::from_millis(retard)).await;
        }
        if let Some(st) = statut {
            return (StatusCode::from_u16(st).unwrap(), Json(json!({})));
        }
        let items: Vec<Value> = c["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| {
                let nom = e
                    .get("title")
                    .or_else(|| e.get("artist"))
                    .and_then(Value::as_str)?;
                nombres
                    .get(nom)
                    .map(|n| json!({"id": e["id"], "listens": n}))
            })
            .collect();
        (StatusCode::OK, Json(json!({ "items": items })))
    }
    let etat: Site = Arc::new(Mutex::new(FauxSite::default()));
    let app = Router::new()
        .route(CHEMIN, post(compter))
        .with_state(etat.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });
    (format!("http://{adresse}"), etat)
}

fn relier_au_site(b: &Arc<dyn DbBackend>, url: &str) {
    let s = reglages(b);
    s.set("mozaik_base_url", url).unwrap();
    s.set("mozaik_access_token", "jeton-de-test").unwrap();
    s.set("server_id", SERVEUR).unwrap();
    s.set(
        crate::cloud::library_sync::CLE_JETON_DE_LIAISON,
        "liaison-de-test",
    )
    .unwrap();
}

fn envoi(b: &Arc<dyn DbBackend>, oui: bool) {
    reglages(b)
        .set(CLE_ACTIVATION, if oui { "true" } else { "false" })
        .unwrap();
}

fn http() -> reqwest::Client {
    crate::http::client::shared().clone()
}

fn recus(site: &Site) -> Vec<Value> {
    site.lock().unwrap().recus.clone()
}

fn nombres(site: &Site, paires: &[(&str, i64)]) {
    site.lock().unwrap().nombres = paires.iter().map(|(k, v)| (k.to_string(), *v)).collect();
}

fn piste(id: i64, titre: &str, artiste: Option<&str>, mbid: Option<&str>) -> Value {
    json!({
        "id": id,
        "title": titre,
        "artist_name": artiste,
        "musicbrainz_recording_id": mbid,
        "file_path": format!("/musique/secret/{id}.flac"),
        "album_id": 77,
    })
}

/// Le témoin des DEUX conditions : sans compte relié, ou envoi des écoutes
/// coupé, rien ne part — et aucun champ n'est inventé.
#[tokio::test]
async fn sans_compte_ou_envoi_coupe_aucun_appel() {
    let (url, site) = faux_site().await;
    nombres(&site, &[("So What", 128)]);
    let b = base();
    let mut items = vec![piste(812, "So What", Some("Miles Davis"), None)];

    // Envoi activé, mais aucun compte.
    envoi(&b, true);
    annoter(&b, &http(), Genre::Piste, &mut items).await;
    assert!(recus(&site).is_empty(), "sans compte : {:?}", recus(&site));

    // Compte relié, envoi jamais activé (défaut), puis coupé.
    let b = base();
    relier_au_site(&b, &url);
    annoter(&b, &http(), Genre::Piste, &mut items).await;
    envoi(&b, false);
    annoter(&b, &http(), Genre::Piste, &mut items).await;
    assert!(recus(&site).is_empty(), "envoi coupé : {:?}", recus(&site));

    // Compte relié sans jeton de liaison : pas relié.
    let b = base();
    relier_au_site(&b, &url);
    reglages(&b)
        .delete(crate::cloud::library_sync::CLE_JETON_DE_LIAISON)
        .unwrap();
    envoi(&b, true);
    annoter(&b, &http(), Genre::Piste, &mut items).await;
    assert!(recus(&site).is_empty(), "sans liaison : {:?}", recus(&site));
    assert!(items[0].get(CHAMP).is_none());
}

/// Un lot : ce qui part (positions, type, MBID, noms ; rien de local), et le
/// champ posé seulement là où le site a rendu un nombre.
#[tokio::test]
async fn un_lot_pose_le_champ_seulement_au_dessus_du_seuil() {
    let (url, site) = faux_site().await;
    nombres(&site, &[("So What", 128)]);
    let b = base();
    relier_au_site(&b, &url);
    envoi(&b, true);
    let mut items = vec![
        piste(
            812,
            "So What",
            Some("Miles Davis"),
            Some("8A2B3C4D-0000-4000-8000-000000000001"),
        ),
        piste(813, "Freddie Freeloader", Some("Miles Davis"), None),
        // Un titre sans artiste ne nomme rien : il ne part pas.
        piste(814, "Intro", None, None),
    ];
    annoter(&b, &http(), Genre::Piste, &mut items).await;

    assert_eq!(items[0][CHAMP], json!(128));
    assert!(
        items[1].get(CHAMP).is_none(),
        "sous le seuil : absent, ni null ni 0"
    );
    assert!(items[2].get(CHAMP).is_none());
    let r = recus(&site);
    assert_eq!(r.len(), 1);
    assert_eq!(
        r[0],
        json!({
            "server_id": SERVEUR,
            "items": [
                {"id": "0", "type": "recording", "mbid": "8a2b3c4d-0000-4000-8000-000000000001",
                 "artist": "Miles Davis", "title": "So What"},
                {"id": "1", "type": "recording", "artist": "Miles Davis",
                 "title": "Freddie Freeloader"},
            ]
        })
    );
    let texte = r[0].to_string();
    for local in ["812", "813", "secret", "77"] {
        assert!(!texte.contains(local), "« {local} » est parti : {texte}");
    }
}

/// Le cache : une liste revue ne redemande rien, absences comprises.
#[tokio::test]
async fn une_liste_revue_ne_redemande_rien() {
    let (url, site) = faux_site().await;
    nombres(&site, &[("So What", 128)]);
    let b = base();
    relier_au_site(&b, &url);
    envoi(&b, true);
    let liste = vec![
        piste(1, "So What", Some("Miles Davis"), None),
        piste(2, "Blue in Green", Some("Miles Davis"), None),
    ];
    let mut premiere = liste.clone();
    annoter(&b, &http(), Genre::Piste, &mut premiere).await;
    let mut seconde = liste.clone();
    annoter(&b, &http(), Genre::Piste, &mut seconde).await;

    assert_eq!(recus(&site).len(), 1, "un seul appel pour deux affichages");
    assert_eq!(seconde[0][CHAMP], json!(128));
    assert!(seconde[1].get(CHAMP).is_none());
}

/// 250 éléments : deux lots, 200 puis 50, jamais plus que le site n'accepte.
#[tokio::test]
async fn les_lots_ne_depassent_jamais_deux_cents() {
    let (url, site) = faux_site().await;
    nombres(&site, &[("Titre 249", 3)]);
    let b = base();
    relier_au_site(&b, &url);
    envoi(&b, true);
    let mut items: Vec<Value> = (0..250)
        .map(|i| piste(i, &format!("Titre {i}"), Some("Artiste"), None))
        .collect();
    annoter(&b, &http(), Genre::Piste, &mut items).await;

    let tailles: Vec<usize> = recus(&site)
        .iter()
        .map(|c| c["items"].as_array().unwrap().len())
        .collect();
    assert_eq!(tailles, [200, 50]);
    assert_eq!(items[249][CHAMP], json!(3));
}

/// Site en panne : aucun champ, et plus d'appel pendant le silence.
#[tokio::test]
async fn apres_un_echec_le_site_n_est_plus_appele() {
    let (url, site) = faux_site().await;
    site.lock().unwrap().statut = Some(503);
    let b = base();
    relier_au_site(&b, &url);
    envoi(&b, true);
    let mut items = vec![piste(1, "So What", Some("Miles Davis"), None)];
    annoter(&b, &http(), Genre::Piste, &mut items).await;
    annoter(&b, &http(), Genre::Piste, &mut items).await;

    assert_eq!(recus(&site).len(), 1);
    assert!(items[0].get(CHAMP).is_none());
}

/// Un site lent ne retient pas la liste plus de [`ATTENTE_MAX`] ; sa réponse
/// tardive remplit le cache pour l'affichage suivant, sans second appel.
#[tokio::test]
async fn un_site_lent_ne_retient_pas_la_liste() {
    let (url, site) = faux_site().await;
    nombres(&site, &[("So What", 128)]);
    site.lock().unwrap().retard_ms = 1500;
    let b = base();
    relier_au_site(&b, &url);
    envoi(&b, true);
    let liste = vec![piste(1, "So What", Some("Miles Davis"), None)];

    let debut = Instant::now();
    let mut premiere = liste.clone();
    annoter(&b, &http(), Genre::Piste, &mut premiere).await;
    assert!(
        debut.elapsed() < ATTENTE_MAX + Duration::from_millis(500),
        "la liste a attendu {:?}",
        debut.elapsed()
    );
    assert!(premiere[0].get(CHAMP).is_none());

    // Pendant que le lot est en vol, un autre affichage ne le redemande pas.
    let mut pendant = liste.clone();
    annoter(&b, &http(), Genre::Piste, &mut pendant).await;

    tokio::time::sleep(Duration::from_millis(1800)).await;
    let mut ensuite = liste.clone();
    annoter(&b, &http(), Genre::Piste, &mut ensuite).await;
    assert_eq!(ensuite[0][CHAMP], json!(128));
    assert_eq!(recus(&site).len(), 1);
}

/// Album et artiste : leur type, leurs champs.
#[tokio::test]
async fn albums_et_artistes_partent_sous_leur_type() {
    let (url, site) = faux_site().await;
    nombres(&site, &[("Kind of Blue", 5400), ("Miles Davis", 43100)]);
    let b = base();
    relier_au_site(&b, &url);
    envoi(&b, true);
    let mut album = json!({"id": 57, "title": "Kind of Blue", "artist_name": "Miles Davis",
                           "musicbrainz_release_id": "ABC"});
    annoter_un(&b, &http(), Genre::Album, &mut album).await;
    let mut artiste = json!({"id": 9, "name": "Miles Davis", "musicbrainz_id": null});
    annoter_un(&b, &http(), Genre::Artiste, &mut artiste).await;
    let mut page = json!({"items": [{"id": 58, "title": "Inconnu", "artist_name": "X"}]});
    annoter_liste(&b, &http(), Genre::Album, page.get_mut("items")).await;

    assert_eq!(album[CHAMP], json!(5400));
    assert_eq!(artiste[CHAMP], json!(43100));
    assert!(page["items"][0].get(CHAMP).is_none());
    let r = recus(&site);
    assert_eq!(
        r[0]["items"],
        json!([{"id": "0", "type": "release", "mbid": "abc", "artist": "Miles Davis",
                "title": "Kind of Blue"}])
    );
    assert_eq!(
        r[1]["items"],
        json!([{"id": "0", "type": "artist", "artist": "Miles Davis"}])
    );
}
