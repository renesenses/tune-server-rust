//! #7026 — titres phares d'un service : total de la communauté, écoutes de ce
//! serveur, tri. Le site est un faux site local, l'historique une base en
//! mémoire.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::*;
use crate::cloud::ecoutes_au_compte::CLE_ACTIVATION;
use crate::cloud::ecoutes_communaute::CHEMIN;
use crate::db::history_repo::ListenRecord;
use crate::db::migrations;
use crate::db::settings_repo::SettingsRepo;
use crate::db::sqlite::SqliteDb;

const ISRC_NOTHING: &str = "USABC2600001";

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

type Recus = Arc<Mutex<Vec<Value>>>;

/// Le faux site : 500 écoutes pour l'ISRC de « Nothing Compares 2 U » — mais
/// seulement si la durée l'accompagne, comme les rend le service —, 40 pour
/// « Could We » ; le reste est sous le seuil (absent).
async fn faux_site() -> (String, Recus) {
    use axum::extract::State;
    use axum::routing::post;
    use axum::{Json, Router};

    async fn compter(State(r): State<Recus>, Json(c): Json<Value>) -> Json<Value> {
        r.lock().unwrap().push(c.clone());
        let items: Vec<Value> = c["items"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| {
                let n = if e["isrc"] == json!(ISRC_NOTHING) && e["duration_ms"].is_u64() {
                    500
                } else if e["title"] == json!("Could We") {
                    40
                } else {
                    return None;
                };
                Some(json!({"id": e["id"], "listens": n}))
            })
            .collect();
        Json(json!({ "items": items }))
    }
    let recus: Recus = Arc::default();
    let app = Router::new()
        .route(CHEMIN, post(compter))
        .with_state(recus.clone());
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });
    (format!("http://{adresse}"), recus)
}

fn relier(b: &Arc<dyn DbBackend>, url: &str) {
    let s = SettingsRepo::with_backend(b.clone());
    s.set("mozaik_base_url", url).unwrap();
    s.set("mozaik_access_token", "jeton-de-test").unwrap();
    s.set("server_id", "serveur-7026").unwrap();
    s.set(
        crate::cloud::library_sync::CLE_JETON_DE_LIAISON,
        "liaison-de-test",
    )
    .unwrap();
    s.set(CLE_ACTIVATION, "true").unwrap();
}

fn piste(id: &str, titre: &str, duree: u64, isrc: Option<&str>) -> Value {
    json!({
        "source_id": id, "title": titre, "artist_name": "Cat Power",
        "album_title": "Jukebox", "duration_ms": duree, "isrc": isrc,
    })
}

/// Les titres phares tels que le service les rend, dans SON ordre.
fn titres_du_service() -> Vec<Value> {
    vec![
        piste("q-1", "Try Me", 139_000, None),
        piste("q-2", "Could We", 131_000, None),
        piste("q-3", "Nothing Compares 2 U", 357_000, Some(ISRC_NOTHING)),
        piste("q-5", "Try Me", 179_000, None),
    ]
}

fn ecouter(
    b: &Arc<dyn DbBackend>,
    source: &str,
    id: Option<&str>,
    titre: &str,
    artiste: &str,
    duree: i64,
) {
    HistoryRepo::with_backend(b.clone())
        .record(&ListenRecord {
            title: titre.into(),
            artist_name: Some(artiste.into()),
            source: source.into(),
            source_id: id.map(Into::into),
            duration_ms: duree,
            ..Default::default()
        })
        .unwrap();
}

/// L'historique de ce serveur : « Could We » écouté deux fois chez Qobuz sous
/// un autre libellé, « Try Me » une fois en fichier (1,5 s d'écart), une
/// radio et un homonyme d'un autre interprète qui ne comptent pas.
fn historique(b: &Arc<dyn DbBackend>) {
    ecouter(
        b,
        "qobuz",
        Some("q-2"),
        "Could We (Edit)",
        "Cat Power",
        131_000,
    );
    ecouter(
        b,
        "qobuz",
        Some("q-2"),
        "Could We (Edit)",
        "Cat Power",
        131_000,
    );
    ecouter(b, "local", None, "TRY ME!", "cat power", 140_500);
    ecouter(
        b,
        "radio",
        None,
        "Nothing Compares 2 U",
        "Cat Power",
        357_000,
    );
    ecouter(
        b,
        "local",
        None,
        "Nothing Compares 2 U",
        "Sinéad O'Connor",
        357_000,
    );
}

fn ordre(items: &[Value]) -> Vec<&str> {
    items
        .iter()
        .map(|v| v["source_id"].as_str().unwrap())
        .collect()
}

#[test]
fn le_tri_suit_le_total_et_garde_l_ordre_du_service_sans_total() {
    let mut items = vec![
        json!({"source_id": "a"}),
        json!({"source_id": "b", "play_count": 5}),
        json!({"source_id": "c"}),
        json!({"source_id": "d", "play_count": 50}),
        json!({"source_id": "e", "play_count": 5}),
    ];
    classer(&mut items);
    assert_eq!(ordre(&items), ["d", "b", "e", "a", "c"]);
}

#[test]
fn les_ecoutes_locales_se_rapprochent_par_identifiant_ou_par_noms_a_3_s() {
    let b = base();
    historique(&b);
    let mut items = titres_du_service();
    poser_ecoutes_locales(&b, "qobuz", &mut items);

    assert_eq!(
        items[0][CHAMP_LOCAL],
        json!(1),
        "Try Me 2:19 : le fichier à 1,5 s"
    );
    assert!(items[0][CHAMP_DERNIERE].is_string());
    assert_eq!(
        items[1][CHAMP_LOCAL],
        json!(2),
        "Could We : par l'identifiant Qobuz"
    );
    assert!(
        items[2].get(CHAMP_LOCAL).is_none(),
        "Nothing Compares 2 U : ni la radio ni l'homonyme ne comptent"
    );
    assert!(
        items[3].get(CHAMP_LOCAL).is_none(),
        "Try Me live (40 s de plus) n'est pas le fichier écouté"
    );
    assert!(
        items.iter().all(|v| v.get(CHAMP_TOTAL).is_none()),
        "le local n'est pas le total"
    );
}

#[tokio::test]
async fn le_total_de_la_communaute_classe_les_titres_phares() {
    let (url, recus) = faux_site().await;
    let b = base();
    relier(&b, &url);
    historique(&b);
    let mut items = titres_du_service();
    enrichir_et_classer(
        &b,
        &crate::http::client::shared().clone(),
        "qobuz",
        &mut items,
    )
    .await;

    assert_eq!(
        ordre(&items),
        ["q-3", "q-2", "q-1", "q-5"],
        "total décroissant, puis l'ordre du service"
    );
    assert_eq!(items[0][CHAMP_TOTAL], json!(500));
    assert_eq!(items[0][CHAMP_PORTEE], json!(PORTEE_COMMUNAUTE));
    assert_eq!(items[1][CHAMP_TOTAL], json!(40));
    assert_eq!(
        items[1][CHAMP_LOCAL],
        json!(2),
        "le local reste à côté du total"
    );
    assert!(items[2].get(CHAMP_TOTAL).is_none());
    assert!(items[2].get(CHAMP_PORTEE).is_none());
    // L'ISRC et la durée partent au site ; aucun identifiant de piste.
    let corps = recus.lock().unwrap()[0].clone();
    let nothing = corps["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["title"] == json!("Nothing Compares 2 U"))
        .unwrap()
        .clone();
    assert_eq!(nothing["isrc"], json!(ISRC_NOTHING));
    assert_eq!(nothing["duration_ms"], json!(357_000));
    assert!(
        !corps.to_string().contains("q-3"),
        "l'identifiant Qobuz est parti : {corps}"
    );
}

#[tokio::test]
async fn sans_compte_relie_l_ordre_du_service_est_garde() {
    let b = base();
    historique(&b);
    let mut items = titres_du_service();
    enrichir_et_classer(
        &b,
        &crate::http::client::shared().clone(),
        "qobuz",
        &mut items,
    )
    .await;

    // Sans total : les écoutes de ce serveur d'abord (#7107 : Could We 2,
    // Try Me 1), puis l'ordre du service.
    assert_eq!(ordre(&items), ["q-2", "q-1", "q-3", "q-5"]);
    assert!(items.iter().all(|v| v.get(CHAMP_TOTAL).is_none()));
    assert_eq!(
        items[0][CHAMP_LOCAL],
        json!(2),
        "le local se pose quand même"
    );
}

// ---------------------------------------------------------------------------
// #7416 — les titres de l'artiste les plus écoutés ici rejoignent la liste.
// ---------------------------------------------------------------------------

const KIWANUKA: &str = "Michael Kiwanuka";

fn piste_kiwanuka(id: &str, titre: &str, duree: u64) -> Value {
    json!({
        "source_id": id, "title": titre, "artist_name": KIWANUKA,
        "album_title": "Love & Hate", "duration_ms": duree,
    })
}

/// Le top Qobuz de la fiche bibliothèque de Kiwanuka (mesure du .18, 30/10) :
/// rien de l'album « Love & Hate ».
fn top_qobuz_kiwanuka() -> Vec<Value> {
    vec![
        piste_kiwanuka("q-10", "Summer Clothes", 265_000),
        piste_kiwanuka("q-11", "California", 238_000),
        piste_kiwanuka("q-12", "Stars", 201_000),
    ]
}

/// L'historique : « Love & Hate » cinq fois en TIDAL et deux fois en fichier
/// (même durée à 1 s près) ; « Summer Clothes » une fois en Qobuz (déjà dans
/// la liste) ; « Home Again » trois fois en fichier, piste retrouvée dans la
/// bibliothèque ; « Rule The World » quatre fois d'un fichier qui n'existe
/// plus (indésignable) ; un homonyme d'un autre interprète et une radio qui ne
/// comptent pas.
fn historique_kiwanuka(b: &Arc<dyn DbBackend>) -> i64 {
    for _ in 0..5 {
        ecouter(b, "tidal", Some("t-1"), "Love & Hate", KIWANUKA, 426_000);
    }
    for _ in 0..2 {
        ecouter(b, "local", None, "LOVE & HATE", KIWANUKA, 427_000);
    }
    ecouter(
        b,
        "qobuz",
        Some("q-10"),
        "Summer Clothes",
        KIWANUKA,
        265_000,
    );
    b.execute("INSERT INTO artists (name) VALUES (?1)", &[&KIWANUKA])
        .unwrap();
    b.execute(
        "INSERT INTO tracks (title, artist_id, duration_ms) \
         VALUES ('Home Again', (SELECT id FROM artists WHERE name = ?1), 230000)",
        &[&KIWANUKA],
    )
    .unwrap();
    let home_again = b
        .query_one("SELECT id FROM tracks WHERE title = 'Home Again'", &[])
        .unwrap()
        .unwrap()[0]
        .as_i64()
        .unwrap();
    for _ in 0..3 {
        ecouter(b, "local", None, "Home Again", KIWANUKA, 230_000);
    }
    for _ in 0..4 {
        ecouter(b, "local", None, "Rule The World", KIWANUKA, 300_000);
    }
    for _ in 0..50 {
        ecouter(
            b,
            "qobuz",
            Some("x-1"),
            "Love & Hate",
            "Quelqu'un d'autre",
            200_000,
        );
    }
    for _ in 0..10 {
        ecouter(b, "radio", None, "Cold Little Heart", KIWANUKA, 0);
    }
    home_again
}

fn titres(items: &[Value]) -> Vec<&str> {
    items.iter().map(|v| v["title"].as_str().unwrap()).collect()
}

#[tokio::test]
async fn un_titre_tres_ecoute_absent_du_service_rejoint_les_titres_phares() {
    let b = base();
    let home_again = historique_kiwanuka(&b);
    let mut items = top_qobuz_kiwanuka();
    ajouter_mes_plus_ecoutes(&b, "qobuz", &mut items);
    enrichir_et_classer(
        &b,
        &crate::http::client::shared().clone(),
        "qobuz",
        &mut items,
    )
    .await;

    assert_eq!(
        titres(&items),
        [
            "Love & Hate",
            "Home Again",
            "Summer Clothes",
            "California",
            "Stars"
        ],
        "classés par mes écoutes, toutes sources ; Summer Clothes une seule fois"
    );
    let love = &items[0];
    assert_eq!(love["source"], json!("tidal"), "désigné par son service");
    assert_eq!(love["source_id"], json!("t-1"));
    assert_eq!(
        love[CHAMP_LOCAL],
        json!(7),
        "5 écoutes TIDAL + 2 du fichier, pas l'homonyme"
    );
    let home = &items[1];
    assert_eq!(home["source"], json!("local"));
    assert_eq!(
        home["id"],
        json!(home_again),
        "le fichier se désigne par sa piste"
    );
    assert_eq!(home[CHAMP_LOCAL], json!(3));
    assert!(
        !titres(&items).contains(&"Rule The World"),
        "un fichier disparu ne se désigne plus : pas ajouté"
    );
    assert!(
        !titres(&items).contains(&"Cold Little Heart"),
        "la radio ne compte pas"
    );
}

#[test]
fn un_enregistrement_du_service_designe_le_titre_ajoute_avant_les_autres() {
    let b = base();
    ecouter(&b, "tidal", Some("t-1"), "Love & Hate", KIWANUKA, 426_000);
    ecouter(&b, "tidal", Some("t-1"), "Love & Hate", KIWANUKA, 426_000);
    ecouter(&b, "qobuz", Some("q-99"), "Love & Hate", KIWANUKA, 426_500);
    let mut items = top_qobuz_kiwanuka();
    ajouter_mes_plus_ecoutes(&b, "qobuz", &mut items);
    assert_eq!(items.len(), 4);
    assert_eq!(items[3]["source"], json!("qobuz"));
    assert_eq!(items[3]["source_id"], json!("q-99"));
}

#[test]
fn sans_titre_du_service_rien_n_est_ajoute() {
    let b = base();
    historique_kiwanuka(&b);
    let mut items: Vec<Value> = Vec::new();
    ajouter_mes_plus_ecoutes(&b, "qobuz", &mut items);
    assert!(items.is_empty(), "l'artiste n'est nommé par aucun titre");
}
