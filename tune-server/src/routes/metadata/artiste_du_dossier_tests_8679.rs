//! #8679 — « Albums douteux » : l'artiste du dossier, montré puis écrit.
//!
//! Les épreuves passent par la ROUTE (`/metadata/doubtful` et
//! `/metadata/doubtful/artist-from-folder`) sur SQLite, et l'écriture en base
//! (`candidats` + `ecrire_en_base`, ce que la route appelle) sur un vrai
//! PostgreSQL quand `TUNE_TEST_PG_URL` est posée (`pg_8679_*`, étape dédiée de
//! `test-postgres.yml`).
use super::*;
use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt;

async fn par_la_route(
    state: &AppState,
    methode: &str,
    uri: &str,
    corps: &str,
) -> (StatusCode, Value) {
    let r = crate::routes::metadata::router()
        .with_state(state.clone())
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(corps.to_string()))
                .expect("requête"),
        )
        .await
        .expect("réponse");
    let code = r.status();
    let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .expect("corps");
    (code, serde_json::from_slice(&octets).unwrap_or(Value::Null))
}

fn entier(db: &Arc<dyn DbBackend>, sql: &str, params: &[&dyn ToSqlValue]) -> Option<i64> {
    db.query_one(sql, params)
        .expect(sql)
        .and_then(|l| l.first().and_then(|v| v.as_i64()))
}

fn inserer(db: &Arc<dyn DbBackend>, sql: &str, params: &[&dyn ToSqlValue]) -> i64 {
    entier(db, &format!("{sql} RETURNING id"), params).expect("id rendu")
}

/// La bibliothèque d'épreuve, marquée par `m` (les tables PostgreSQL sont
/// partagées entre bancs) :
///
/// * `floyd1`, `floyd2` — sans artiste, sous `…/Pink Floyd <m>/Animals <m>/`,
///   seules dans leur album ; l'artiste existe déjà, en CAPITALES ;
/// * `prince` — sans artiste, sous `…/Prince <m>/Best Of <m>/` ;
/// * `blakey` — sans artiste, sous `…/Art Blakey <m>/Best Of <m>/`, MÊME
///   album que `prince` (« Best Of » d'artiste inconnu) ;
/// * `muet` — sans artiste, rangée sous `/Musique/<album>/` : le dossier ne
///   dit rien ;
/// * `tenue` — un artiste déjà là : hors sujet.
struct Biblio {
    floyd1: i64,
    floyd2: i64,
    prince: i64,
    blakey: i64,
    muet: i64,
    tenue: i64,
    floyd_existant: i64,
    animals: i64,
    best_of: i64,
}

fn remplir(db: &Arc<dyn DbBackend>, m: &str) -> Biblio {
    let floyd_existant = inserer(
        db,
        "INSERT INTO artists (name) VALUES (?1)",
        &[&format!("PINK FLOYD {m}")],
    );
    let autre = inserer(
        db,
        "INSERT INTO artists (name) VALUES (?1)",
        &[&format!("Keith Jarrett {m}")],
    );
    let animals = inserer(
        db,
        "INSERT INTO albums (title) VALUES (?1)",
        &[&format!("Animals {m}")],
    );
    let best_of = inserer(
        db,
        "INSERT INTO albums (title) VALUES (?1)",
        &[&format!("Best Of {m}")],
    );
    let divers = inserer(
        db,
        "INSERT INTO albums (title) VALUES (?1)",
        &[&format!("Divers {m}")],
    );
    let piste = |album: i64, artiste: Option<i64>, chemin: String| {
        inserer(
            db,
            "INSERT INTO tracks (title, album_id, artist_id, source, file_path) \
             VALUES ('x', ?1, ?2, 'local', ?3)",
            &[&album, &artiste, &chemin],
        )
    };
    Biblio {
        floyd1: piste(
            animals,
            None,
            format!("/srv/{m}/Pink Floyd {m}/Animals {m}/01.flac"),
        ),
        floyd2: piste(
            animals,
            None,
            format!("/srv/{m}/Pink Floyd {m}/Animals {m}/CD2/02.flac"),
        ),
        prince: piste(
            best_of,
            None,
            format!("/srv/{m}/Prince {m}/Best Of {m}/01.m4a"),
        ),
        blakey: piste(
            best_of,
            None,
            format!("/srv/{m}/Art Blakey {m}/Best Of {m}/02.m4a"),
        ),
        muet: piste(divers, None, format!("/Musique/Divers {m}/01.flac")),
        tenue: piste(
            divers,
            Some(autre),
            format!("/srv/{m}/Autre {m}/Divers {m}/02.flac"),
        ),
        floyd_existant,
        animals,
        best_of,
    }
}

fn artiste_de_piste(db: &Arc<dyn DbBackend>, id: i64) -> Option<i64> {
    entier(db, "SELECT artist_id FROM tracks WHERE id = ?1", &[&id])
}

fn artiste_d_album(db: &Arc<dyn DbBackend>, id: i64) -> Option<i64> {
    entier(db, "SELECT artist_id FROM albums WHERE id = ?1", &[&id])
}

fn nom_d_artiste(db: &Arc<dyn DbBackend>, id: i64) -> String {
    db.query_one("SELECT name FROM artists WHERE id = ?1", &[&id])
        .expect("artiste")
        .and_then(|l| l.first().and_then(|v| v.as_string()))
        .unwrap_or_default()
}

/// L'écriture en base, telle que la route l'appelle — commune à SQLite et
/// PostgreSQL.
fn eprouver_sur(db: &Arc<dyn DbBackend>, m: &str) {
    let b = remplir(db, m);
    let ids = [b.floyd1, b.floyd2, b.prince, b.blakey, b.muet, b.tenue];
    let cands = candidats(db, Some(&ids)).expect("candidates");
    let mut vus: Vec<(i64, String)> = cands
        .iter()
        .map(|c| (c.track_id, c.artiste.clone()))
        .collect();
    vus.sort();
    let mut attendus = vec![
        (b.floyd1, format!("Pink Floyd {m}")),
        (b.floyd2, format!("Pink Floyd {m}")),
        (b.prince, format!("Prince {m}")),
        (b.blakey, format!("Art Blakey {m}")),
    ];
    attendus.sort();
    assert_eq!(
        vus, attendus,
        "#8679 : seules les pistes sans artiste dont le dossier parle sont candidates \
         (dossier de disque sauté, « Musique » muet, piste tenue écartée)"
    );

    let bilan = ecrire_en_base(db, &cands);
    assert_eq!(bilan.pistes, 4, "bilan : {bilan:?}");
    assert_eq!(bilan.echecs, 0, "bilan : {bilan:?}");
    assert_eq!(
        bilan.artistes_existants, 1,
        "« Pink Floyd » existait (autre casse)"
    );
    assert_eq!(bilan.artistes_crees, 2, "Prince et Art Blakey créés");

    // Rattachées à l'artiste EXISTANT, pas à un homographe neuf.
    assert_eq!(artiste_de_piste(db, b.floyd1), Some(b.floyd_existant));
    assert_eq!(artiste_de_piste(db, b.floyd2), Some(b.floyd_existant));
    // L'album d'un seul artiste le prend ; l'album partagé par deux dossiers
    // d'artistes différents reste sans artiste.
    assert_eq!(artiste_d_album(db, b.animals), Some(b.floyd_existant));
    assert_eq!(bilan.albums, 1, "bilan : {bilan:?}");
    assert_eq!(
        artiste_d_album(db, b.best_of),
        None,
        "« Best Of » attribué au premier venu"
    );
    let prince = artiste_de_piste(db, b.prince).expect("Prince posé");
    assert_eq!(nom_d_artiste(db, prince), format!("Prince {m}"));
    assert_ne!(artiste_de_piste(db, b.blakey), Some(prince));
    // Rien d'autre n'a bougé.
    assert_eq!(artiste_de_piste(db, b.muet), None);

    // Plus aucune candidate parmi elles : elles sortent de la raison.
    assert!(candidats(db, Some(&ids)).expect("relecture").is_empty());
}

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).expect("état")
}

#[test]
fn l_ecriture_en_base_relie_l_artiste_existant_et_l_album_d_un_seul_artiste_8679() {
    let state = etat();
    eprouver_sur(&state.backend, "sqlite");
}

/// La liste montre le nom du dossier ; l'aperçu compte sans rien écrire ;
/// l'écriture fait sortir les pistes de la raison.
#[tokio::test]
async fn la_liste_montre_le_dossier_l_apercu_compte_puis_l_ecriture_vide_la_raison_8679() {
    let state = etat();
    let b = remplir(&state.backend, "route");

    let (code, v) = par_la_route(&state, "GET", "/doubtful?limit=100", "").await;
    assert_eq!(code, StatusCode::OK);
    let item = |id: i64| {
        v["items"]
            .as_array()
            .expect("items")
            .iter()
            .find(|i| i["id"] == id)
            .cloned()
            .unwrap_or(Value::Null)
    };
    assert_eq!(item(b.floyd1)["folder_artist"], "Pink Floyd route");
    assert!(
        item(b.floyd1)["reasons"]
            .as_array()
            .expect("raisons")
            .iter()
            .any(|r| r == "missing_artist")
    );
    assert_eq!(
        item(b.muet)["folder_artist"],
        Value::Null,
        "« Musique » ne dit rien"
    );

    // Aperçu de toute la liste : 4 pistes, 3 artistes, rien d'écrit.
    let (code, a) = par_la_route(
        &state,
        "POST",
        "/doubtful/artist-from-folder",
        r#"{"dry_run":true}"#,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{a}");
    assert_eq!(a["tracks"], 4, "{a}");
    assert_eq!(a["artists"][0]["name"], "Pink Floyd route");
    assert_eq!(a["artists"][0]["tracks"], 2);
    assert_eq!(a["artists"].as_array().map(Vec::len), Some(3));
    assert_eq!(a["file_writes_enabled"], false);
    assert_eq!(
        artiste_de_piste(&state.backend, b.floyd1),
        None,
        "l'aperçu a écrit"
    );

    // Une ligne seule.
    let (_, r) = par_la_route(
        &state,
        "POST",
        "/doubtful/artist-from-folder",
        &format!(r#"{{"track_ids":[{}]}}"#, b.prince),
    )
    .await;
    assert_eq!(r["updated"], 1, "{r}");
    assert_eq!(
        artiste_de_piste(&state.backend, b.floyd1),
        None,
        "une ligne a écrit toute la liste"
    );

    // Toute la liste.
    let (_, r) = par_la_route(&state, "POST", "/doubtful/artist-from-folder", "{}").await;
    assert_eq!(r["updated"], 3, "{r}");
    assert_eq!(r["artists_linked"], 1, "{r}");
    // Réglage décoché (défaut) : aucun fichier.
    assert_eq!(r["file_writes_enabled"], false);
    assert_eq!(r["files_scheduled"], 0);

    let (_, v) = par_la_route(&state, "GET", "/doubtful?limit=100", "").await;
    let encore: Vec<&Value> = v["items"]
        .as_array()
        .expect("items")
        .iter()
        .filter(|i| !i["folder_artist"].is_null())
        .collect();
    assert!(
        encore.is_empty(),
        "#8679 : encore proposées après écriture : {encore:?}"
    );
}

/// Le fichier n'est confié à l'écrivain QUE réglage coché.
#[tokio::test]
async fn le_fichier_n_est_ecrit_que_si_le_reglage_est_coche_8679() {
    let state = etat();
    let b = remplir(&state.backend, "reglage");
    crate::routes::ecriture_fichiers::activer_pour_test(&state.backend);
    let (_, r) = par_la_route(
        &state,
        "POST",
        "/doubtful/artist-from-folder",
        &format!(r#"{{"track_ids":[{},{}]}}"#, b.floyd1, b.floyd2),
    )
    .await;
    assert_eq!(r["file_writes_enabled"], true, "{r}");
    assert_eq!(r["files_scheduled"], 2, "{r}");
}

#[tokio::test]
async fn une_liste_vide_est_refusee_8679() {
    let state = etat();
    let (code, _) = par_la_route(
        &state,
        "POST",
        "/doubtful/artist-from-folder",
        r#"{"track_ids":[]}"#,
    )
    .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
}

/// Variable ABSENTE ⇒ saut annoncé ; POSÉE ⇒ la même épreuve sur le schéma
/// PostgreSQL migré, lignes marquées puis retirées.
#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_8679_l_artiste_du_dossier_s_ecrit_sur_postgresql() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT: TUNE_TEST_PG_URL absent — épreuve PostgreSQL #8679 sautée");
        return;
    };
    let pg = tune_core::db::postgres::PostgresDb::connect(&url)
        .await
        .expect("connexion PostgreSQL");
    tune_core::db::migrations::run_pg_migrations(pg.pool())
        .await
        .expect("migrations PostgreSQL");
    pg.ensure_schema().await;
    let db: Arc<dyn DbBackend> = Arc::new(tune_core::db::backend::PostgresBackend::new(
        pg.pool().clone(),
    ));
    let m = format!("pg8679-{}", std::process::id());
    let menage = |db: &Arc<dyn DbBackend>| {
        let motif = "%pg8679-%".to_string();
        let _ = db.execute("DELETE FROM tracks WHERE file_path LIKE ?1", &[&motif]);
        let _ = db.execute("DELETE FROM albums WHERE title LIKE ?1", &[&motif]);
        let _ = db.execute("DELETE FROM artists WHERE name LIKE ?1", &[&motif]);
    };
    tokio::task::block_in_place(|| {
        menage(&db);
        eprouver_sur(&db, &m);
        menage(&db);
    });
    pg.pool().close().await;
}
