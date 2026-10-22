//! #6545 — le partage communautaire des notes, éprouvé contre un faux site.
//!
//! Chaque test passe par [`passe`] ou [`agregats_d_album`], les seuls chemins
//! que la tâche de fond et la route appellent, et lit ce que le faux site a
//! REÇU.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::*;
use crate::db::migrations;
use crate::db::rating_repo::RatingRepo;
use crate::db::sqlite::SqliteDb;

const SERVEUR: &str = "serveur-a";

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    let b: Arc<dyn DbBackend> = Arc::new(db);
    b.execute_batch(
        "INSERT INTO artists (id, name) VALUES (20, 'Miles Davis'), (21, 'Bill Evans');\
         INSERT INTO albums (id, title, artist_id) VALUES (10, 'Kind of Blue', 20), \
                (11, 'Sunday at the Village Vanguard', 21);",
    )
    .unwrap();
    b
}

fn reglages(b: &Arc<dyn DbBackend>) -> SettingsRepo {
    SettingsRepo::with_backend(b.clone())
}

fn noter(b: &Arc<dyn DbBackend>, album: i64, profil: i64, note: i32, commentaire: Option<&str>) {
    RatingRepo::with_backend(b.clone())
        .rate_album(album, profil, note, commentaire)
        .unwrap();
}

#[derive(Default)]
struct FauxSite {
    /// (méthode, chemin, corps ou requête) reçus, dans l'ordre.
    recus: Vec<(String, String, Value)>,
    /// Réponses imposées aux PUT de notes, dans l'ordre ; vide : 200.
    reponses_notes: Vec<(u16, Value)>,
    /// Réponse aux agrégats ; `None` : 503.
    agregats: Option<Value>,
}

type Site = Arc<Mutex<FauxSite>>;

async fn faux_site() -> (String, Site) {
    use axum::extract::{Path, Query, State};
    use axum::http::StatusCode;
    use axum::routing::{get, post, put};
    use axum::{Json, Router};
    use std::collections::HashMap;

    async fn consentement(State(s): State<Site>, Json(c): Json<Value>) -> Json<Value> {
        let enabled = c["enabled"].as_bool().unwrap();
        s.lock()
            .unwrap()
            .recus
            .push(("PUT".into(), "consent".into(), c));
        Json(
            json!({"enabled": enabled, "consented_at": if enabled { json!("2026-10-20T10:00:00Z") } else { Value::Null }}),
        )
    }
    async fn notes(State(s): State<Site>, Json(c): Json<Value>) -> (StatusCode, Json<Value>) {
        let mut s = s.lock().unwrap();
        let n = c["ratings"].as_array().unwrap().len();
        s.recus.push(("PUT".into(), "ratings".into(), c));
        if !s.reponses_notes.is_empty() {
            let (st, corps) = s.reponses_notes.remove(0);
            return (StatusCode::from_u16(st).unwrap(), Json(corps));
        }
        (StatusCode::OK, Json(json!({"stored": n, "rejected": 0})))
    }
    async fn albums(
        State(s): State<Site>,
        Query(q): Query<HashMap<String, String>>,
    ) -> (StatusCode, Json<Value>) {
        let mut s = s.lock().unwrap();
        s.recus.push(("GET".into(), "albums".into(), json!(q)));
        match s.agregats.clone() {
            Some(v) => (StatusCode::OK, Json(v)),
            None => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({}))),
        }
    }
    async fn lier(Path(id): Path<String>) -> Json<Value> {
        Json(json!({"server_id": id, "token": "liaison-neuve"}))
    }
    let etat: Site = Arc::new(Mutex::new(FauxSite::default()));
    let app = Router::new()
        .route("/api/v1/community/consent", put(consentement))
        .route("/api/v1/community/ratings", put(notes))
        .route("/api/v1/community/albums", get(albums))
        .route("/api/v1/cloud-library/{id}/link", post(lier))
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

fn consentir(b: &Arc<dyn DbBackend>, oui: bool) {
    reglages(b)
        .set(CLE_PARTAGE, if oui { "true" } else { "false" })
        .unwrap();
}

fn http() -> reqwest::Client {
    crate::http::client::shared().clone()
}

fn recus(site: &Site) -> Vec<(String, String, Value)> {
    site.lock().unwrap().recus.clone()
}

#[test]
fn le_partage_est_desactive_par_defaut() {
    const { assert!(!PARTAGE_DEFAUT) };
    let b = base();
    assert!(!partage_active(&reglages(&b)));
    for v in ["false", "0", "peut-être"] {
        reglages(&b).set(CLE_PARTAGE, v).unwrap();
        assert!(!partage_active(&reglages(&b)), "{v}");
    }
    consentir(&b, true);
    assert!(partage_active(&reglages(&b)));
}

/// Sans consentement, RIEN ne part : ni le consentement, ni les notes, même
/// avec un compte relié et des notes en base.
#[tokio::test]
async fn sans_consentement_rien_ne_part() {
    let (url, site) = faux_site().await;
    let b = base();
    noter(&b, 10, 1, 5, Some("mon avis"));
    relier_au_site(&b, &url);

    assert_eq!(passe(&b, &http()).await, Issue::Rien("disabled"));
    consentir(&b, false);
    assert_eq!(passe(&b, &http()).await, Issue::Rien("disabled"));
    assert!(
        recus(&site).is_empty(),
        "le site a reçu : {:?}",
        recus(&site)
    );
}

/// Consentement donné : il part, puis l'instantané des notes, sans le
/// commentaire ni le numéro de profil ; un second passage ne renvoie rien.
#[tokio::test]
async fn le_consentement_puis_l_instantane_partent_une_fois() {
    let (url, site) = faux_site().await;
    let b = base();
    noter(&b, 10, 1, 5, Some("COMMENTAIRE-SECRET"));
    noter(&b, 10, 2, 3, None);
    noter(&b, 11, 1, 4, None);
    relier_au_site(&b, &url);
    consentir(&b, true);

    assert_eq!(
        passe(&b, &http()).await,
        Issue::Transmis {
            consentement: Some(true),
            notes: Some(3)
        }
    );
    let r = recus(&site);
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].1, "consent");
    assert_eq!(r[0].2, json!({"server_id": SERVEUR, "enabled": true}));
    assert_eq!(r[1].1, "ratings");
    let notes = r[1].2["ratings"].as_array().unwrap();
    assert_eq!(
        notes[0],
        json!({"profile": empreinte_de_profil(SERVEUR, 1), "artist": "Miles Davis",
               "album": "Kind of Blue", "rating": 5})
    );
    assert_eq!(notes[1]["profile"], json!(empreinte_de_profil(SERVEUR, 2)));
    assert_eq!(notes[2]["album"], json!("Sunday at the Village Vanguard"));
    let texte = r[1].2.to_string();
    assert!(
        !texte.contains("COMMENTAIRE-SECRET"),
        "le commentaire est parti : {texte}"
    );
    assert!(!texte.contains("album_id"), "l'identifiant local est parti");

    assert_eq!(passe(&b, &http()).await, Issue::Rien("unchanged"));
    assert_eq!(recus(&site).len(), 2, "rien ne doit repartir d'inchangé");
}

/// Changement et retrait d'une note : l'instantané suivant les porte.
#[tokio::test]
async fn un_changement_ou_un_retrait_renvoie_l_instantane() {
    let (url, site) = faux_site().await;
    let b = base();
    noter(&b, 10, 1, 5, None);
    noter(&b, 11, 1, 4, None);
    relier_au_site(&b, &url);
    consentir(&b, true);
    passe(&b, &http()).await;

    noter(&b, 10, 1, 2, None);
    assert!(matches!(
        passe(&b, &http()).await,
        Issue::Transmis { notes: Some(2), .. }
    ));
    RatingRepo::with_backend(b.clone())
        .delete_rating(11, 1)
        .unwrap();
    assert!(matches!(
        passe(&b, &http()).await,
        Issue::Transmis { notes: Some(1), .. }
    ));
    let r = recus(&site);
    assert_eq!(
        r.last().unwrap().2["ratings"],
        json!([{
        "profile": empreinte_de_profil(SERVEUR, 1), "artist": "Miles Davis",
        "album": "Kind of Blue", "rating": 2}])
    );
    assert_eq!(r[2].2["ratings"][0]["rating"], json!(2));
}

/// Retrait du consentement : le retrait part (et rien d'autre) ; à la
/// réactivation, le consentement ET tout l'instantané repartent.
#[tokio::test]
async fn retirer_le_consentement_le_dit_au_site_et_la_reprise_renvoie_tout() {
    let (url, site) = faux_site().await;
    let b = base();
    noter(&b, 10, 1, 5, None);
    relier_au_site(&b, &url);
    consentir(&b, true);
    passe(&b, &http()).await;

    consentir(&b, false);
    assert_eq!(
        passe(&b, &http()).await,
        Issue::Transmis {
            consentement: Some(false),
            notes: None
        }
    );
    assert_eq!(
        recus(&site)[2].2,
        json!({"server_id": SERVEUR, "enabled": false})
    );
    assert_eq!(passe(&b, &http()).await, Issue::Rien("disabled"));
    assert_eq!(recus(&site).len(), 3, "désactivé, plus rien ne part");

    consentir(&b, true);
    assert_eq!(
        passe(&b, &http()).await,
        Issue::Transmis {
            consentement: Some(true),
            notes: Some(1)
        }
    );
    assert_eq!(recus(&site).len(), 5);
}

/// Un échec des notes ne note pas l'empreinte : le même instantané repart.
#[tokio::test]
async fn un_echec_des_notes_les_renvoie_au_passage_suivant() {
    let (url, site) = faux_site().await;
    let b = base();
    noter(&b, 10, 1, 5, None);
    relier_au_site(&b, &url);
    consentir(&b, true);
    site.lock()
        .unwrap()
        .reponses_notes
        .push((500, json!({"error": "boom"})));

    assert!(matches!(passe(&b, &http()).await, Issue::Echec(_)));
    assert_eq!(reglages(&b).get(CLE_EMPREINTE_NOTES).unwrap(), None);
    assert!(matches!(
        passe(&b, &http()).await,
        Issue::Transmis {
            consentement: None,
            notes: Some(1)
        }
    ));
}

/// Le site a perdu le consentement (403 `consent_required`) : il est
/// retransmis au passage suivant, puis les notes.
#[tokio::test]
async fn un_consentement_perdu_par_le_site_est_retransmis() {
    let (url, site) = faux_site().await;
    let b = base();
    noter(&b, 10, 1, 5, None);
    relier_au_site(&b, &url);
    consentir(&b, true);
    site.lock()
        .unwrap()
        .reponses_notes
        .push((403, json!({"error": "consent_required"})));

    assert_eq!(
        passe(&b, &http()).await,
        Issue::Echec("consent_required".into())
    );
    assert!(matches!(
        passe(&b, &http()).await,
        Issue::Transmis {
            consentement: Some(true),
            notes: Some(1)
        }
    ));
    let chemins: Vec<String> = recus(&site).into_iter().map(|r| r.1).collect();
    assert_eq!(chemins, ["consent", "ratings", "consent", "ratings"]);
}

/// Les agrégats : lus au site par artiste et album, rendus tels quels, gardés
/// en cache ; un site injoignable ne l'est pas ; sans compte, rien.
#[tokio::test]
async fn les_agregats_d_un_album_sont_lus_puis_gardes_en_cache() {
    let (url, site) = faux_site().await;
    let b = base();

    assert_eq!(
        agregats_d_album(&b, &http(), 10).await,
        json!({"available": false, "reason": "account_not_linked"})
    );
    relier_au_site(&b, &url);
    assert_eq!(
        agregats_d_album(&b, &http(), 999).await["available"],
        json!(false)
    );

    // Site en panne : indisponible, et rien n'est mis en cache.
    assert_eq!(
        agregats_d_album(&b, &http(), 10).await,
        json!({"available": false, "reason": "site_unavailable"})
    );

    site.lock().unwrap().agregats = Some(json!({
        "album_key": "abc",
        "rating": {"average": 4.2, "count": 37},
        "listens": {"count": 1240, "listeners": 12},
    }));
    let attendu = json!({
        "available": true,
        "rating": {"average": 4.2, "count": 37},
        "listens": {"count": 1240, "listeners": 12},
    });
    assert_eq!(agregats_d_album(&b, &http(), 10).await, attendu);
    assert_eq!(agregats_d_album(&b, &http(), 10).await, attendu);
    let lectures: Vec<Value> = recus(&site)
        .into_iter()
        .filter(|r| r.1 == "albums")
        .map(|r| r.2)
        .collect();
    assert_eq!(
        lectures.len(),
        2,
        "une panne, puis UNE lecture mise en cache"
    );
    assert_eq!(
        lectures[1],
        json!({"server_id": SERVEUR, "artist": "Miles Davis", "album": "Kind of Blue"})
    );
}

/// Sous le seuil, le site ne rend rien : l'interface reçoit des `null`.
#[tokio::test]
async fn sous_le_seuil_les_agregats_sont_nuls() {
    let (url, site) = faux_site().await;
    let b = base();
    relier_au_site(&b, &url);
    site.lock().unwrap().agregats =
        Some(json!({"album_key": "abc", "rating": null, "listens": null}));
    assert_eq!(
        agregats_d_album(&b, &http(), 11).await,
        json!({"available": true, "rating": null, "listens": null})
    );
}

// ───────────── #6545 — le même parcours sur une VRAIE base PostgreSQL ─────────────
//
// Les témoins ci-dessus tournent sur SQLite, qui avale ce que PostgreSQL
// refuse : un `LIMIT ?` lié en entier, un `CAST(… AS BIGINT)` sur une colonne
// déjà entière, une jointure sur des colonnes de types différents, l'upsert
// des notes. Le parcours ci-dessous passe par les SEULS chemins que la tâche
// de fond et la route appellent (`instantane_des_notes`, `passe`,
// `agregats_d_album`) ; il est joué sur SQLite (contre-épreuve du moteur) et
// sur PostgreSQL, contre le schéma que les migrations ont posé (étape
// « Apply PG migrations » de `test-postgres.yml`), dans des tables
// TEMPORAIRES copiées des vraies : rien n'est écrit dans la base partagée.

/// Artistes et albums du parcours, posés par des `INSERT` portables.
fn semer(b: &Arc<dyn DbBackend>) {
    for (id, nom) in [(20_i64, "Miles Davis"), (21, "Bill Evans")] {
        b.execute(
            "INSERT INTO artists (id, name) VALUES (?, ?)",
            &[&id as &dyn ToSqlValue, &nom],
        )
        .unwrap();
    }
    for (id, titre, artiste) in [
        (10_i64, "Kind of Blue", 20_i64),
        (11, "Sunday at the Village Vanguard", 21),
    ] {
        b.execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (?, ?, ?)",
            &[&id as &dyn ToSqlValue, &titre, &artiste],
        )
        .unwrap();
    }
}

async fn parcours_communautaire(b: Arc<dyn DbBackend>) {
    semer(&b);
    noter(&b, 10, 1, 5, Some("COMMENTAIRE-SECRET"));
    noter(&b, 10, 2, 3, None);
    noter(&b, 11, 1, 4, None);

    // L'instantané, lu tel quel : trié par album puis profil, sans
    // commentaire ni identifiant local.
    assert_eq!(
        instantane_des_notes(&b, SERVEUR).unwrap(),
        vec![
            json!({"profile": empreinte_de_profil(SERVEUR, 1), "artist": "Miles Davis",
                   "album": "Kind of Blue", "rating": 5}),
            json!({"profile": empreinte_de_profil(SERVEUR, 2), "artist": "Miles Davis",
                   "album": "Kind of Blue", "rating": 3}),
            json!({"profile": empreinte_de_profil(SERVEUR, 1), "artist": "Bill Evans",
                   "album": "Sunday at the Village Vanguard", "rating": 4}),
        ]
    );

    let (url, site) = faux_site().await;
    relier_au_site(&b, &url);
    assert_eq!(passe(&b, &http()).await, Issue::Rien("disabled"));
    assert!(recus(&site).is_empty(), "sans consentement, rien ne part");

    consentir(&b, true);
    assert_eq!(
        passe(&b, &http()).await,
        Issue::Transmis {
            consentement: Some(true),
            notes: Some(3)
        }
    );
    let texte = recus(&site)[1].2.to_string();
    assert!(!texte.contains("COMMENTAIRE-SECRET"), "{texte}");
    // L'empreinte de l'instantané, relue en base : rien ne repart.
    assert_eq!(passe(&b, &http()).await, Issue::Rien("unchanged"));

    // Changement (upsert) puis retrait d'une note.
    noter(&b, 10, 1, 2, None);
    assert_eq!(
        passe(&b, &http()).await,
        Issue::Transmis {
            consentement: None,
            notes: Some(3)
        }
    );
    noter(&b, 11, 1, 0, None);
    assert_eq!(
        passe(&b, &http()).await,
        Issue::Transmis {
            consentement: None,
            notes: Some(2)
        }
    );
    assert_eq!(
        recus(&site).last().unwrap().2["ratings"][0]["rating"],
        json!(2)
    );

    // Retrait du consentement, puis reprise : tout repart.
    consentir(&b, false);
    assert_eq!(
        passe(&b, &http()).await,
        Issue::Transmis {
            consentement: Some(false),
            notes: None
        }
    );
    assert_eq!(reglages(&b).get(CLE_EMPREINTE_NOTES).unwrap(), None);
    consentir(&b, true);
    assert_eq!(
        passe(&b, &http()).await,
        Issue::Transmis {
            consentement: Some(true),
            notes: Some(2)
        }
    );

    // La route : l'artiste et l'album lus en base partent au site.
    assert_eq!(
        agregats_d_album(&b, &http(), 999).await,
        json!({"available": false, "reason": "album_not_found"})
    );
    site.lock().unwrap().agregats = Some(json!({
        "album_key": "abc",
        "rating": {"average": 4.2, "count": 37},
        "listens": {"count": 1240, "listeners": 12},
    }));
    assert_eq!(
        agregats_d_album(&b, &http(), 11).await["available"],
        json!(true)
    );
    let lecture = recus(&site)
        .into_iter()
        .rfind(|r| r.1 == "albums")
        .unwrap()
        .2;
    assert_eq!(
        lecture,
        json!({"server_id": SERVEUR, "artist": "Bill Evans",
               "album": "Sunday at the Village Vanguard"})
    );
}

/// Le parcours sur SQLite : la contre-épreuve du moteur.
#[tokio::test]
async fn sqlite_6545_parcours_communautaire() {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    parcours_communautaire(Arc::new(db)).await;
}

/// Le parcours sur PostgreSQL (`TUNE_TEST_PG_URL`, migrations appliquées).
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_6545_parcours_communautaire() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT: TUNE_TEST_PG_URL absent");
        return;
    };
    // Une seule connexion : les tables temporaires n'existent que pour elle.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let b: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
    // Le schéma RÉEL : chaque copie échoue si la migration n'a pas posé la
    // table. `album_ratings` garde son type de note (entier, PG 013) et sa
    // contrainte d'unicité (album, profil), dont l'upsert a besoin.
    for table in ["artists", "albums", "album_ratings", "settings"] {
        b.execute(
            &format!("CREATE TEMP TABLE {table} (LIKE public.{table} INCLUDING ALL)"),
            &[],
        )
        .unwrap();
    }
    parcours_communautaire(b).await;
    pool.close().await;
}
