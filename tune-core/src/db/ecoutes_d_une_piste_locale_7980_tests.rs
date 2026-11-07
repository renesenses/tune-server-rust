//! #7980 — écouter un album sur Qobuz augmentait le compteur d'écoutes de
//! l'album LOCAL du même nom : le rapprochement titre + interprète de
//! [`sql::plays_for_tracks`] n'écartait que la radio.
//!
//! Ces témoins tiennent la règle « une écoute ne compte que pour une piste de
//! la même provenance » ([`sql::meme_provenance`]) sur SQLite ET sur
//! PostgreSQL (`pg_7980_…`, saute sans `TUNE_TEST_PG_URL`), aux quatre
//! endroits qui l'appliquent :
//!
//! - `plays_for_tracks` (colonne « # Plays » de la fiche d'un album) ;
//! - `track_plays` (`GET /library/history/tracks/{id}/plays`) ;
//! - `ecoutes_de_la_piste` et `derniere_ecoute_de_la_piste` (listes et
//!   collections intelligentes).
//!
//! Trois pistes de même titre et même interprète, une par provenance : un
//! fichier local, une piste UPnP de la bibliothèque, une piste Qobuz de
//! `tracks`. Chacune ne compte que ses propres écoutes : la piste UPnP garde
//! ses écoutes UPnP, la piste Qobuz ne reçoit pas les écoutes locales.
//!
//! Le tri « # écoutes » de la table des titres (`JOINTURE_ECOUTES`) est gardé
//! dans `track_repo/page_de_pistes.rs` (`i1470_…`, `i7980_…`).
//!
//! Contre-épreuves : remettre `h.source != 'radio'` dans
//! `sql::plays_for_tracks` fait rougir le témoin Qobuz (1 au lieu de 0) ;
//! remettre `COALESCE(h.source, 'local') = 'local'` y fait rougir le témoin
//! UPnP (0 au lieu de 1).

use super::*;
use crate::db::migrations;

const TITRE: &str = "Titre 7980";
const INTERPRETE: &str = "Interprète 7980";

/// Les trois pistes « Titre 7980 » d'« Interprète 7980 » : `base` locale,
/// `base + 1` UPnP, `base + 2` Qobuz.
fn poser_les_pistes(db: &Arc<dyn DbBackend>, base: i64) {
    db.execute(
        &format!("INSERT INTO artists (id, name) VALUES ({base}, '{INTERPRETE}')"),
        &[],
    )
    .unwrap();
    db.execute(
        &format!(
            "INSERT INTO albums (id, title, artist_id, track_count) \
             VALUES ({base}, 'Album 7980', {base}, 3)"
        ),
        &[],
    )
    .unwrap();
    for (decalage, chemin, source, source_id) in [
        (0, "'/musique/7980.flac'", "local", "NULL"),
        (1, "NULL", "upnp", "'uuid-nas|7980'"),
        (2, "NULL", "qobuz", "'7980001'"),
    ] {
        let id = base + decalage;
        db.execute(
            &format!(
                "INSERT INTO tracks (id, title, artist_id, album_id, track_number, \
                 file_path, source, source_id) \
                 VALUES ({id}, '{TITRE}', {base}, {base}, {}, {chemin}, '{source}', {source_id})",
                decalage + 1
            ),
            &[],
        )
        .unwrap();
    }
}

/// Une écoute écrite par le vrai chemin (`record`, `track_id` NULL), datée
/// ensuite pour que « la dernière » soit vérifiable.
fn ecouter(repo: &HistoryRepo, db: &Arc<dyn DbBackend>, source: &str, quand: &str) -> i64 {
    let id = repo
        .record(&ListenRecord {
            title: TITRE.into(),
            artist_name: Some(INTERPRETE.into()),
            source: source.into(),
            source_id: (source == "qobuz").then(|| "7980001".to_string()),
            duration_ms: 240_000,
            ..Default::default()
        })
        .unwrap();
    let sql = match db.engine() {
        Engine::Sqlite => "UPDATE listen_history SET listened_at = ? WHERE id = ?",
        Engine::Postgres => "UPDATE listen_history SET listened_at = $1 WHERE id = $2",
    };
    db.execute(sql, &[&quand as &dyn ToSqlValue, &id]).unwrap();
    id
}

/// Le compte et la dernière écoute de la piste `piste`, de provenance
/// `source`, lus par les trois chemins, qui doivent s'accorder.
fn comptes(db: &Arc<dyn DbBackend>, piste: i64, source: &str) -> (i64, Option<String>) {
    let repo = HistoryRepo::with_backend(db.clone());
    let colonne = repo.plays_for_tracks(&[piste]).unwrap()[&piste].clone();
    let ligne = db
        .query_one(
            &format!(
                "SELECT {}, {} FROM tracks t WHERE t.id = {piste}",
                sql::ecoutes_de_la_piste("t"),
                sql::derniere_ecoute_de_la_piste("t")
            ),
            &[],
        )
        .unwrap()
        .unwrap();
    let intelligentes = (
        ligne[0].as_i64().unwrap(),
        ligne.get(1).and_then(|v| v.as_string()),
    );
    assert_eq!(
        intelligentes, colonne,
        "#7980 : les listes intelligentes comptent comme la fiche ({source})"
    );
    assert_eq!(
        repo.track_plays(TITRE, Some(INTERPRETE), source).unwrap(),
        colonne.0,
        "#7980 : track_plays compte comme la fiche ({source})"
    );
    colonne
}

fn verifier(db: &Arc<dyn DbBackend>, base: i64) {
    let repo = HistoryRepo::with_backend(db.clone());
    let (locale, upnp, qobuz) = (base, base + 1, base + 2);
    let date = |d: &str| Some(d.to_string());

    // 1. Une écoute Qobuz du même titre, du même interprète.
    ecouter(&repo, db, "qobuz", "2026-11-02T10:00:00Z");
    assert_eq!(
        comptes(db, locale, "local"),
        (0, None),
        "#7980 : une écoute Qobuz ne doit pas compter pour la piste locale du même nom"
    );
    assert_eq!(comptes(db, upnp, "upnp"), (0, None));
    assert_eq!(
        comptes(db, qobuz, "qobuz"),
        (1, date("2026-11-02T10:00:00Z")),
        "#7980 : l'écoute Qobuz compte pour la piste Qobuz de la bibliothèque"
    );

    // 2. Une écoute locale : +1 pour le fichier local, datée d'elle-même, et
    //    rien pour la piste Qobuz du même nom.
    ecouter(&repo, db, "local", "2026-11-01T09:00:00Z");
    assert_eq!(
        comptes(db, locale, "local"),
        (1, date("2026-11-01T09:00:00Z")),
        "#7980 : une écoute locale compte +1, datée d'elle-même"
    );
    assert_eq!(
        comptes(db, qobuz, "qobuz").0,
        1,
        "#7980 : une écoute locale ne doit pas compter pour la piste Qobuz du même nom"
    );
    assert_eq!(
        comptes(db, upnp, "upnp"),
        (0, None),
        "#7980 : une écoute locale ne doit pas compter pour la piste UPnP du même nom"
    );

    // 3. Une écoute UPnP : la piste UPnP de la bibliothèque la garde.
    ecouter(&repo, db, "upnp", "2026-11-04T10:00:00Z");
    assert_eq!(
        comptes(db, upnp, "upnp"),
        (1, date("2026-11-04T10:00:00Z")),
        "#7980 : une piste UPnP de la bibliothèque compte ses écoutes UPnP"
    );
    assert_eq!(comptes(db, locale, "local").0, 1);
    assert_eq!(comptes(db, qobuz, "qobuz").0, 1);

    // 4. Des lignes anciennes, source NULL ou vide : locales.
    for quand in ["2026-10-01T08:00:00Z", "2026-10-02T08:00:00Z"] {
        ecouter(&repo, db, "local", quand);
    }
    db.execute(
        "UPDATE listen_history SET source = NULL \
         WHERE listened_at = '2026-10-01T08:00:00Z'",
        &[],
    )
    .unwrap();
    db.execute(
        "UPDATE listen_history SET source = '' \
         WHERE listened_at = '2026-10-02T08:00:00Z'",
        &[],
    )
    .unwrap();
    assert_eq!(
        comptes(db, locale, "local").0,
        3,
        "#7980 : une écoute ancienne à source NULL ou vide reste une écoute locale"
    );
    assert_eq!(comptes(db, qobuz, "qobuz").0, 1);
    assert_eq!(comptes(db, upnp, "upnp").0, 1);

    // 5. La radio reste exclue partout, et l'écoute Qobuz garde son propre
    //    compte par service et identifiant (#8062).
    ecouter(&repo, db, "radio", "2026-11-03T10:00:00Z");
    assert_eq!(comptes(db, locale, "local").0, 3, "la radio ne compte pas");
    assert_eq!(comptes(db, upnp, "upnp").0, 1);
    assert_eq!(comptes(db, qobuz, "qobuz").0, 1);
    assert_eq!(
        repo.ecoutes_de_service_par_identifiant("qobuz", &["7980001"])
            .unwrap()
            .get("7980001")
            .map(|e| e.0),
        Some(1),
        "l'écoute Qobuz compte pour la piste Qobuz"
    );
}

#[test]
fn sqlite_chaque_piste_ne_compte_que_les_ecoutes_de_sa_provenance_7980() {
    let base = SqliteDb::open_in_memory().unwrap();
    base.init_schema().unwrap();
    migrations::run_migrations(&base).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(base);
    poser_les_pistes(&db, 1);
    verifier(&db, 1);
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_7980_chaque_piste_ne_compte_que_les_ecoutes_de_sa_provenance() {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT : TUNE_TEST_PG_URL non posée");
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
    let nettoyer = |db: &Arc<dyn DbBackend>| {
        for sql in [
            format!("DELETE FROM listen_history WHERE title = '{TITRE}'"),
            "DELETE FROM tracks WHERE id BETWEEN 7980 AND 7982".to_string(),
            "DELETE FROM albums WHERE id = 7980".to_string(),
            "DELETE FROM artists WHERE id = 7980".to_string(),
        ] {
            db.execute(&sql, &[]).unwrap();
        }
    };
    nettoyer(&db);
    poser_les_pistes(&db, 7980);
    verifier(&db, 7980);
    nettoyer(&db);
}
