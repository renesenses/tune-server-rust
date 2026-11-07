//! #8088 — témoins de la case « Masquer les contenus explicites » : le champ
//! de la politique, le marquage lu sur les objets (services, balises), les
//! marques de la bibliothèque locale en base (SQLite et PostgreSQL) et le
//! filtrage des listes.

use super::*;
use crate::db::backend::DbBackend;
use crate::db::models::Track;
use crate::db::track_repo::TrackRepo;
use serde_json::json;
use std::sync::Arc;

/// Une politique enregistrée avant #8088 (sans le champ) se relit, case
/// décochée ; la case fait l'aller-retour.
#[test]
fn le_champ_de_la_politique_8088() {
    let ancienne: PolitiqueParentale =
        serde_json::from_value(json!({"artistes_bloques": []})).unwrap();
    assert!(!ancienne.masquer_explicite);
    let p: PolitiqueParentale = serde_json::from_value(json!({"masquer_explicite": true})).unwrap();
    assert!(p.masquer_explicite);
    assert_eq!(p.valider(), Ok(()));
    let relu: PolitiqueParentale =
        serde_json::from_value(serde_json::to_value(&p).unwrap()).unwrap();
    assert_eq!(relu, p);
}

#[test]
fn le_marquage_explicite_des_objets_8088() {
    let o = |v: Value| v.as_object().unwrap().clone();
    for oui in [
        json!({"explicit": true}),
        json!({"explicit": 1}),
        json!({"explicit": "1"}),
        json!({"parental_warning": true}),
    ] {
        assert!(est_marque_explicite(&o(oui.clone())), "{oui}");
    }
    for non in [
        json!({}),
        json!({"explicit": false}),
        json!({"explicit": 0}),
        json!({"explicit": null}),
        json!({"parental_warning": false}),
        json!({"explicit": "2"}),
    ] {
        assert!(!est_marque_explicite(&o(non.clone())), "{non}");
    }
}

fn marques() -> MarquesLocales {
    MarquesLocales {
        pistes: [11, 12].into_iter().collect(),
        albums: [5].into_iter().collect(),
    }
}

/// Les formes de réponse des routes filtrées : pistes et albums des services
/// (marqués par le service), pistes et albums locaux (marqués en base),
/// lignes de playlist (`track_id`), à toute profondeur.
#[test]
fn le_filtre_retire_les_contenus_explicites_8088() {
    let m = marques();
    let mut service = json!({
        "tracks": [
            {"source_id": "1", "title": "Explicite", "explicit": true},
            {"source_id": "2", "title": "Propre", "explicit": false},
            {"source_id": "3", "title": "Sans marquage"}
        ],
        "albums": [
            {"source_id": "a1", "title": "Album explicite", "explicit": true},
            {"source_id": "a2", "title": "Album sans marquage"}
        ],
        "total": 5
    });
    assert_eq!(m.filtrer(&mut service), 2);
    assert_eq!(service["tracks"].as_array().unwrap().len(), 2);
    assert_eq!(service["albums"][0]["source_id"], "a2");
    assert_eq!(service["total"], 3);

    let mut locale = json!({
        "items": [
            {"id": 5, "title": "Album local explicite", "track_count": 10},
            {"id": 6, "title": "Album local propre", "track_count": 8,
             "tracks": [
                {"id": 11, "album_id": 6, "title": "Piste explicite", "duration_ms": 1},
                {"id": 13, "album_id": 6, "title": "Piste propre", "duration_ms": 1}
             ]}
        ],
        "total": 2
    });
    assert_eq!(m.filtrer(&mut locale), 2);
    assert_eq!(locale["items"].as_array().unwrap().len(), 1);
    assert_eq!(locale["items"][0]["id"], 6);
    assert_eq!(locale["items"][0]["tracks"][0]["id"], 13);

    // Recherche : un album local sous « albums », sans `track_count`.
    let mut recherche = json!({"albums": [{"id": 5, "title": "x"}, {"id": 7, "title": "y"}]});
    assert_eq!(m.filtrer(&mut recherche), 1);
    assert_eq!(recherche["albums"][0]["id"], 7);

    // Playlist : les lignes nomment la piste par `track_id`.
    let mut playlist = json!({"tracks": [
        {"id": 900, "track_id": 12, "title": "Ligne explicite"},
        {"id": 901, "track_id": 14, "title": "Ligne propre"}
    ]});
    assert_eq!(m.filtrer(&mut playlist), 1);
    assert_eq!(playlist["tracks"][0]["track_id"], 14);
}

/// Un identifiant numérique d'un service n'est pas un identifiant de
/// bibliothèque : seul le marquage du service compte pour lui.
#[test]
fn un_objet_de_service_ne_se_confond_pas_avec_la_bibliotheque_8088() {
    let m = marques();
    let mut v = json!({"items": [
        {"id": 11, "source": "qobuz", "album_id": 3, "title": "Homonyme"},
        {"id": 5, "source": "tidal", "track_count": 3, "title": "Homonyme"}
    ]});
    assert_eq!(m.filtrer(&mut v), 0);
}

/// La fiche d'un album ou d'une piste explicite est refusée, pas vidée ; une
/// enveloppe de liste ne l'est jamais.
#[test]
fn la_fiche_explicite_est_reconnue_8088() {
    let m = marques();
    assert!(m.racine_explicite(&json!({"source_id": "a1", "explicit": true, "tracks": []})));
    assert!(m.racine_explicite(&json!({"id": 5, "title": "x", "track_count": 3})));
    assert!(m.racine_explicite(&json!({"id": 11, "album_id": 5, "duration_ms": 3})));
    assert!(!m.racine_explicite(&json!({"id": 6, "title": "x", "track_count": 3})));
    assert!(!m.racine_explicite(&json!({"items": [{"explicit": true}], "total": 1})));
    assert!(!m.racine_explicite(&json!([{"explicit": true}])));
}

fn piste(db: &Arc<dyn DbBackend>, chemin: &str, album: Option<i64>, explicit: Option<bool>) -> i64 {
    let mut t = Track::new(chemin.into());
    t.file_path = Some(chemin.into());
    t.source = "local".into();
    t.album_id = album;
    t.explicit = explicit;
    TrackRepo::with_backend(db.clone()).create(&t).unwrap()
}

/// Ce que chaque moteur doit tenir : le marquage s'écrit, se relit, alimente
/// les marques et la garde de lecture ; une écriture hors scan le garde ; le
/// scan l'écrit tel que le fichier le dit.
fn verifier_les_marques(db: Arc<dyn DbBackend>) {
    db.execute(
        "INSERT INTO albums (id, title) VALUES (5, 'A'), (6, 'B')",
        &[],
    )
    .unwrap();
    let explicite = piste(&db, "/m/e.flac", Some(5), Some(true));
    let propre = piste(&db, "/m/p.flac", Some(6), Some(false));
    let sans = piste(&db, "/m/s.flac", Some(6), None);
    let repo = TrackRepo::with_backend(db.clone());
    assert_eq!(repo.get(explicite).unwrap().unwrap().explicit, Some(true));
    assert_eq!(repo.get(propre).unwrap().unwrap().explicit, Some(false));
    assert_eq!(repo.get(sans).unwrap().unwrap().explicit, None);

    let m = marques_explicites_locales(&db).unwrap();
    assert_eq!(m.pistes, [explicite].into_iter().collect());
    assert_eq!(m.albums, [5].into_iter().collect());
    assert!(piste_locale_explicite(&db, explicite).unwrap());
    assert!(!piste_locale_explicite(&db, propre).unwrap());
    assert!(!piste_locale_explicite(&db, sans).unwrap());
    assert!(!piste_locale_explicite(&db, 999_999).unwrap());

    // Une écriture qui reconstruit la ligne sans relire la balise la garde.
    let mut t = repo.get(explicite).unwrap().unwrap();
    t.explicit = None;
    t.title = "Retouchée".into();
    repo.update(&t).unwrap();
    assert_eq!(repo.get(explicite).unwrap().unwrap().explicit, Some(true));
    // Le scan l'écrit tel que le fichier le dit : balise retirée, marque
    // retirée.
    repo.update_batch(std::slice::from_ref(&t)).unwrap();
    assert_eq!(repo.get(explicite).unwrap().unwrap().explicit, None);
    assert!(marques_explicites_locales(&db).unwrap().pistes.is_empty());
}

#[test]
fn les_marques_locales_8088_sqlite() {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    verifier_les_marques(Arc::new(db));
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_8088_marques_explicites_postgres() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT: TUNE_TEST_PG_URL absent");
        return;
    };
    let pool = sqlx::PgPool::connect(&url).await.expect("connexion PG");
    for sql in crate::db::postgres::ENSURE_TABLES
        .iter()
        .chain(crate::db::postgres::ENSURE_COLUMNS.iter())
    {
        let _ = sqlx::raw_sql(*sql).execute(&pool).await;
    }
    crate::db::migrations::run_pg_migrations(&pool)
        .await
        .unwrap_or_else(|e| panic!("run_pg_migrations : {e}"));
    let db: Arc<dyn DbBackend> = Arc::new(crate::db::backend::PostgresBackend::new(pool.clone()));
    let db2 = db.clone();
    tokio::task::spawn_blocking(move || {
        db2.execute(
            "TRUNCATE TABLE tracks, albums RESTART IDENTITY CASCADE",
            &[],
        )
        .unwrap();
        verifier_les_marques(db2);
    })
    .await
    .unwrap();
    let typ: String = sqlx::query_scalar(
        "SELECT data_type FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = 'tracks' \
         AND column_name = 'explicit'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(typ, "bigint", "PG 107 : `tracks.explicit` doit être BIGINT");
    let index: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_indexes WHERE tablename = 'tracks' \
         AND indexname = 'idx_tracks_explicit'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        index, 1,
        "PG 107 : l'index partiel des pistes explicites manque"
    );
    pool.close().await;
}
