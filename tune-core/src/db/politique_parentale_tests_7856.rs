//! Témoins du contrôle parental, étape 1 (#7856), sur SQLite et PostgreSQL :
//! sans politique rien ne change, un artiste bloqué disparaît des listes,
//! une radio ou un podcast hors liste blanche aussi, le profil supervisé n'a
//! aucun droit sur sa politique, une révision périmée est refusée.

use super::*;
use crate::db::backend::DbBackend;
use serde_json::json;
use std::sync::Arc;

const ADMIN: i64 = 1;
const ENFANT: i64 = 2;
const AUTRE: i64 = 3;

fn politique() -> PolitiqueParentale {
    PolitiqueParentale {
        artistes_bloques: vec![
            ArtisteBloque {
                id: Some(7),
                nom: Some("Groupe Interdit".into()),
            },
            ArtisteBloque {
                id: None,
                nom: Some("  Autre   INTERDIT ".into()),
            },
        ],
        radios_autorisees: Some(vec!["12".into(), "https://flux.example/permis".into()]),
        podcasts_autorises: Some(vec!["https://rss.example/permis.xml".into()]),
        plages_horaires: vec![PlageHoraire {
            jours: vec![1, 2, 3],
            debut: "08:00".into(),
            fin: "19:30".into(),
        }],
        budget_quotidien_minutes: Some(90),
        budget_hebdomadaire_minutes: Some(600),
    }
}

/// Ce que chaque moteur doit tenir, avec les tables déjà posées.
fn verifier(db: Arc<dyn DbBackend>) {
    for (id, admin) in [(ADMIN, 1), (ENFANT, 0), (AUTRE, 0)] {
        db.execute(
            "INSERT INTO profiles (id, username, is_admin) VALUES (?, ?, ?)",
            &[&id as &dyn ToSqlValue, &format!("p{id}"), &admin],
        )
        .unwrap();
    }

    assert!(est_administrateur(&db, ADMIN).unwrap());
    assert!(!est_administrateur(&db, ENFANT).unwrap());

    // Sans politique : rien n'est lu, et seul l'administrateur peut en poser.
    assert!(
        lire(&db, ENFANT).unwrap().is_none(),
        "aucune politique par défaut"
    );
    assert!(peut_gerer(&db, ADMIN, ENFANT).unwrap());
    assert!(
        !peut_gerer(&db, AUTRE, ENFANT).unwrap(),
        "un profil ordinaire ne pose rien"
    );
    assert!(
        !peut_gerer(&db, ENFANT, ENFANT).unwrap(),
        "personne ne gère sa propre politique"
    );

    // Création, puis relecture à l'identique.
    let p = politique();
    assert_eq!(
        poser(&db, ENFANT, ADMIN, 0, &p).unwrap(),
        Ecriture::Faite(1)
    );
    let lue = lire(&db, ENFANT).unwrap().expect("politique relue");
    assert_eq!((lue.superviseur_id, lue.revision), (ADMIN, 1));
    assert_eq!(lue.politique, p);

    // Le profil supervisé n'a aucun droit, ni sur la sienne ni sur une autre.
    assert!(
        !peut_gerer(&db, ENFANT, ENFANT).unwrap(),
        "le profil supervisé ne gère pas sa politique"
    );
    assert!(
        !peut_gerer(&db, ENFANT, AUTRE).unwrap(),
        "un profil supervisé ne supervise personne"
    );
    assert!(
        peut_gerer(&db, ADMIN, ENFANT).unwrap(),
        "le superviseur garde la main"
    );

    // Révision optimiste : une seconde création et une révision périmée sont
    // refusées sans rien écrire.
    let mut p2 = p.clone();
    p2.budget_quotidien_minutes = Some(30);
    assert_eq!(
        poser(&db, ENFANT, ADMIN, 0, &p2).unwrap(),
        Ecriture::Conflit,
        "création en double"
    );
    assert_eq!(
        poser(&db, ENFANT, ADMIN, 1, &p2).unwrap(),
        Ecriture::Faite(2)
    );
    assert_eq!(
        poser(&db, ENFANT, ADMIN, 1, &p).unwrap(),
        Ecriture::Conflit,
        "révision périmée"
    );
    let lue = lire(&db, ENFANT).unwrap().unwrap();
    assert_eq!(
        (lue.revision, lue.politique.budget_quotidien_minutes),
        (2, Some(30))
    );
    assert_eq!(
        retirer(&db, ENFANT, 1).unwrap(),
        Ecriture::Conflit,
        "retrait périmé"
    );

    // Le filtre construit depuis la politique RELUE de la base.
    let filtre = Filtre::depuis(&lue.politique);
    let mut albums = json!({"items": [
        {"id": 1, "title": "Permis", "artist_id": 3, "artist_name": "Libre"},
        {"id": 2, "title": "Bloqué par id", "artist_id": 7, "artist_name": "Renommé"},
        {"id": 3, "title": "Bloqué par nom", "artist_id": 9, "artist_name": "autre interdit"}
    ], "total": 3});
    assert_eq!(filtre.filtrer(&mut albums, Liste::Bibliotheque), 2);
    assert_eq!(albums["items"].as_array().unwrap().len(), 1);
    assert_eq!(albums["items"][0]["title"], "Permis");
    assert_eq!(albums["total"], 1);

    assert_eq!(retirer(&db, ENFANT, 2).unwrap(), Ecriture::Faite(0));
    assert!(lire(&db, ENFANT).unwrap().is_none());
}

fn sqlite() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE profiles (id INTEGER PRIMARY KEY, username TEXT, is_admin INTEGER DEFAULT 0)",
    )
    .unwrap();
    db.execute_batch(SQL_TABLE).unwrap();
    Arc::new(db)
}

#[test]
fn politique_parentale_7856_sqlite() {
    verifier(sqlite());
}

/// La vraie passe de migration SQLite pose la table, et la rejouer ne casse
/// rien.
#[test]
fn politique_parentale_7856_table_posee_par_les_migrations() {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    assert!(lire(&db, 1).unwrap().is_none());
    assert_eq!(
        poser(&db, 2, 1, 0, &politique()).unwrap(),
        Ecriture::Faite(1)
    );
}

/// Table absente (création échouée au démarrage) : l'erreur se reconnaît, et
/// se distingue d'une politique présente mais illisible.
#[test]
fn politique_parentale_7856_table_absente_reconnue() {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.execute_batch(
        "CREATE TABLE profiles (id INTEGER PRIMARY KEY, username TEXT, is_admin INTEGER DEFAULT 0)",
    )
    .unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let e = lire(&db, ENFANT).expect_err("sans table, la lecture échoue");
    assert!(
        table_absente(&e),
        "erreur de table absente non reconnue : {e}"
    );
    assert!(table_absente(
        r#"error returned from database: relation "politiques_parentales" does not exist"#
    ));

    db.execute_batch(SQL_TABLE).unwrap();
    db.execute(
        "INSERT INTO politiques_parentales (profile_id, superviseur_id, revision, politique_json, maj_le) \
         VALUES (2, 1, 1, '{illisible', '')",
        &[],
    )
    .unwrap();
    let e = lire(&db, ENFANT).expect_err("une politique illisible est une erreur");
    assert!(
        !table_absente(&e),
        "une politique illisible n'est pas une table absente : {e}"
    );
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_7856_politique_parentale_postgres() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT: TUNE_TEST_PG_URL absent");
        return;
    };
    // Une seule connexion : les tables temporaires masquent celles de la base
    // de test et disparaissent avec elle.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .unwrap();
    let ddl = SQL_TABLE.replace("CREATE TABLE IF NOT EXISTS", "CREATE TEMPORARY TABLE");
    sqlx::raw_sql("CREATE TEMPORARY TABLE profiles (id BIGINT PRIMARY KEY, username TEXT, is_admin SMALLINT DEFAULT 0)")
        .execute(&pool)
        .await
        .unwrap();
    // `AssertSqlSafe` : `ddl` vient de la constante `SQL_TABLE`, aucune entrée.
    sqlx::raw_sql(sqlx::AssertSqlSafe(ddl))
        .execute(&pool)
        .await
        .unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
    verifier(db);
    pool.close().await;
}

/// Les éléments d'artistes, de radios et de podcasts, dans les formes de
/// réponse des routes filtrées.
#[test]
fn politique_parentale_7856_filtre_des_listes() {
    let filtre = Filtre::depuis(&politique());

    // Liste des artistes : les éléments SONT des artistes.
    let mut artistes = json!({"items": [
        {"id": 7, "name": "Nouveau nom"}, {"id": 8, "name": "Groupe interdit"}, {"id": 9, "name": "Libre"}
    ], "total": 3});
    assert_eq!(filtre.filtrer(&mut artistes, Liste::Artistes), 2);
    assert_eq!(artistes["items"], json!([{"id": 9, "name": "Libre"}]));

    // Recherche : artistes, albums et pistes, y compris d'un service (nom
    // seulement : son `id` n'est pas un identifiant de bibliothèque).
    let mut recherche = json!({
        "artists": [{"id": 7, "name": "Libre", "source": "qobuz"}, {"id": 1, "name": "GROUPE INTERDIT", "source": "qobuz"}],
        "albums": [{"id": 4, "artist_id": 7, "artist_name": "x"}],
        "tracks": [{"id": 5, "artist": "Groupe Interdit"}, {"id": 6, "artist": "Libre"}]
    });
    assert_eq!(filtre.filtrer(&mut recherche, Liste::Bibliotheque), 3);
    assert_eq!(
        recherche["artists"],
        json!([{"id": 7, "name": "Libre", "source": "qobuz"}])
    );
    assert_eq!(recherche["albums"], json!([]));
    assert_eq!(recherche["tracks"], json!([{"id": 6, "artist": "Libre"}]));

    // Radios : seules celles de la liste blanche, par id ou par flux ; les
    // flux d'une station gardée ne sont pas touchés.
    let mut radios = json!([
        {"id": 12, "name": "Permise", "stream_url": "https://flux.example/autre"},
        {"id": 13, "name": "Par flux", "stream_url": "https://flux.example/permis"},
        {"id": 14, "name": "Hors liste", "stream_url": "https://flux.example/non"}
    ]);
    assert_eq!(filtre.filtrer(&mut radios, Liste::Radios), 1);
    assert_eq!(radios.as_array().unwrap().len(), 2);
    // Recherche : `items` (Mes radios) et `annuaire` ; une fiche d'annuaire
    // gardée par son flux garde ses flux.
    let mut recherche_radio = json!({"items": [{"id": 14, "stream_url": "x"}],
    "annuaire": [
        {"external_id": "rm-1", "name": "Hors liste", "stream_url": "https://flux.example/non"},
        {"external_id": "rm-2", "name": "Permise", "stream_url": "https://flux.example/permis",
         "streams": [{"url": "https://flux.example/autre"}]}
    ]});
    assert_eq!(filtre.filtrer(&mut recherche_radio, Liste::Radios), 2);
    assert_eq!(recherche_radio["annuaire"][0]["external_id"], "rm-2");
    assert_eq!(
        recherche_radio["annuaire"][0]["streams"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Annuaire : seuls `items` sont des stations ; les pays, les genres et
    // tout autre tableau ou objet de la réponse ne sont pas touchés, même
    // quand leurs objets portent un `id` ou une `url`.
    let mut annuaire = json!({"total": 2, "items": [
            {"external_id": "rm-3", "stream_url": "https://flux.example/non"},
            {"external_id": "rm-4", "my_radio_id": 12, "stream_url": "https://flux.example/z"}
        ],
        "countries": [{"code": "FR", "count": 2}],
        "genres": [{"id": "jazz", "count": 1}],
        "attribution": {"name": "x", "url": "https://annuaire.example"},
        "liens": [{"id": 99, "url": "https://ailleurs.example"}]});
    assert_eq!(filtre.filtrer(&mut annuaire, Liste::Radios), 1);
    assert_eq!(annuaire["items"].as_array().unwrap().len(), 1);
    assert_eq!(annuaire["items"][0]["external_id"], "rm-4");
    assert_eq!(annuaire["total"], 1);
    assert_eq!(annuaire["genres"].as_array().unwrap().len(), 1);
    assert_eq!(annuaire["liens"].as_array().unwrap().len(), 1);
    assert_eq!(annuaire["attribution"]["url"], "https://annuaire.example");

    // Podcasts : par adresse du flux.
    let mut podcasts = json!([{"id": 1, "feed_url": "https://rss.example/permis.xml"}, {"id": 2, "feed_url": "https://rss.example/autre.xml"}]);
    assert_eq!(filtre.filtrer(&mut podcasts, Liste::Podcasts), 1);
    assert_eq!(podcasts[0]["id"], 1);
    // Découverte : `curated` et `top` ; les genres ne sont pas des podcasts.
    let mut decouverte = json!({
        "curated": [{"feed_url": "https://rss.example/autre.xml"}],
        "top": [{"feed_url": "https://rss.example/permis.xml"}, {"url": "https://rss.example/x.xml"}],
        "genres": [{"id": 1, "name": "Arts", "url": "https://genres.example"}]
    });
    assert_eq!(filtre.filtrer(&mut decouverte, Liste::Podcasts), 2);
    assert_eq!(decouverte["top"].as_array().unwrap().len(), 1);
    assert_eq!(decouverte["genres"].as_array().unwrap().len(), 1);

    // Sans liste blanche, rien n'est restreint.
    let libre = Filtre::depuis(&PolitiqueParentale::default());
    let mut radios = json!([{"id": 14, "stream_url": "x"}]);
    assert_eq!(libre.filtrer(&mut radios, Liste::Radios), 0);
}

#[test]
fn politique_parentale_7856_validation() {
    assert!(politique().valider().is_ok());
    let mut p = politique();
    p.plages_horaires[0].jours = vec![0];
    assert_eq!(p.valider(), Err("plage_horaire_jour_invalide"));
    let mut p = politique();
    p.plages_horaires[0].fin = "25:00".into();
    assert_eq!(p.valider(), Err("plage_horaire_heure_invalide"));
    let mut p = politique();
    p.plages_horaires[0].fin = "07:00".into();
    assert_eq!(p.valider(), Err("plage_horaire_vide"));
    let mut p = politique();
    p.budget_quotidien_minutes = Some(24 * 60 + 1);
    assert_eq!(p.valider(), Err("budget_quotidien_invalide"));
    let mut p = politique();
    p.artistes_bloques.push(ArtisteBloque {
        id: None,
        nom: Some("  ".into()),
    });
    assert_eq!(p.valider(), Err("artiste_bloque_sans_id_ni_nom"));
    assert!(serde_json::from_value::<PolitiqueParentale>(json!({"inconnu": 1})).is_err());
}
