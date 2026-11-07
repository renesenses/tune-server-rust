//! #7980 — écouter un album sur Qobuz augmentait le compteur d'écoutes de
//! l'album LOCAL du même nom : le rapprochement titre + interprète de
//! [`sql::plays_for_tracks`] n'écartait que la radio.
//!
//! Ces témoins tiennent la règle « une piste de la bibliothèque ne compte que
//! ses écoutes locales » sur SQLite ET sur PostgreSQL (`pg_7980_…`, saute
//! sans `TUNE_TEST_PG_URL`), aux quatre endroits qui la recopient :
//!
//! - `plays_for_tracks` (colonne « # Plays » de la fiche d'un album) ;
//! - `track_plays` (`GET /library/history/tracks/{id}/plays`) ;
//! - `ecoutes_de_la_piste` et `derniere_ecoute_de_la_piste` (listes et
//!   collections intelligentes).
//!
//! Le tri « # écoutes » de la table des titres (`JOINTURE_ECOUTES`) est gardé
//! dans `track_repo/page_de_pistes.rs` (`i1470_…`, `i7980_…`).
//!
//! Contre-épreuve : remettre `h.source != 'radio'` dans
//! `sql::plays_for_tracks` fait rougir le témoin Qobuz (1 au lieu de 0).

use super::*;
use crate::db::migrations;

const TITRE: &str = "Titre 7980";
const INTERPRETE: &str = "Interprète 7980";

/// La piste locale `id` « Titre 7980 » d'« Interprète 7980 ».
fn poser_la_piste_locale(db: &Arc<dyn DbBackend>, id: i64) {
    db.execute(
        &format!("INSERT INTO artists (id, name) VALUES ({id}, '{INTERPRETE}')"),
        &[],
    )
    .unwrap();
    db.execute(
        &format!(
            "INSERT INTO albums (id, title, artist_id, track_count) \
             VALUES ({id}, 'Album 7980', {id}, 1)"
        ),
        &[],
    )
    .unwrap();
    db.execute(
        &format!(
            "INSERT INTO tracks (id, title, artist_id, album_id, file_path, source) \
             VALUES ({id}, '{TITRE}', {id}, {id}, '/musique/7980.flac', 'local')"
        ),
        &[],
    )
    .unwrap();
}

/// Une écoute écrite par le vrai chemin (`record`, `track_id` NULL), datée
/// ensuite pour que « la dernière » soit vérifiable.
fn ecouter(repo: &HistoryRepo, db: &Arc<dyn DbBackend>, source: &str, quand: &str) -> i64 {
    let id = repo
        .record(&ListenRecord {
            title: TITRE.into(),
            artist_name: Some(INTERPRETE.into()),
            source: source.into(),
            source_id: (source != "local").then(|| "7980001".to_string()),
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

fn plays(repo: &HistoryRepo, piste: i64) -> (i64, Option<String>) {
    repo.plays_for_tracks(&[piste]).unwrap()[&piste].clone()
}

/// Le compte et la date que lisent les listes intelligentes, par les
/// sous-requêtes corrélées de `ecoutes_de_la_piste`.
fn intelligentes(db: &Arc<dyn DbBackend>, piste: i64) -> (i64, Option<String>) {
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
    (
        ligne[0].as_i64().unwrap(),
        ligne.get(1).and_then(|v| v.as_string()),
    )
}

fn verifier(db: &Arc<dyn DbBackend>, piste: i64) {
    let repo = HistoryRepo::with_backend(db.clone());

    // 1. Une écoute Qobuz du même titre, du même interprète.
    ecouter(&repo, db, "qobuz", "2026-11-02T10:00:00Z");
    assert_eq!(
        plays(&repo, piste),
        (0, None),
        "#7980 : une écoute Qobuz ne doit pas compter pour la piste locale du même nom"
    );
    assert_eq!(
        repo.track_plays(TITRE, Some(INTERPRETE)).unwrap(),
        0,
        "#7980 : track_plays ne compte pas l'écoute Qobuz"
    );
    assert_eq!(
        intelligentes(db, piste),
        (0, None),
        "#7980 : les listes intelligentes ne comptent pas l'écoute Qobuz"
    );

    // 2. Une écoute locale : +1, et c'est sa date, pas celle de Qobuz.
    ecouter(&repo, db, "local", "2026-11-01T09:00:00Z");
    let une_locale = (1, Some("2026-11-01T09:00:00Z".to_string()));
    assert_eq!(
        plays(&repo, piste),
        une_locale,
        "#7980 : une écoute locale compte +1, datée d'elle-même"
    );
    assert_eq!(repo.track_plays(TITRE, Some(INTERPRETE)).unwrap(), 1);
    assert_eq!(intelligentes(db, piste), une_locale);

    // 3. Une ligne ancienne, écrite avant le défaut de la colonne : source
    //    NULL, locale pour la bibliothèque (`track_repo::est_local`).
    let ancienne = ecouter(&repo, db, "local", "2026-10-01T08:00:00Z");
    db.execute(
        &format!("UPDATE listen_history SET source = NULL WHERE id = {ancienne}"),
        &[],
    )
    .unwrap();
    assert_eq!(
        plays(&repo, piste).0,
        2,
        "#7980 : une écoute ancienne à source NULL reste une écoute locale"
    );
    assert_eq!(repo.track_plays(TITRE, Some(INTERPRETE)).unwrap(), 2);
    assert_eq!(intelligentes(db, piste).0, 2);

    // 4. La radio reste exclue, et l'écoute Qobuz garde son propre compte
    //    par service et identifiant (#8062).
    ecouter(&repo, db, "radio", "2026-11-03T10:00:00Z");
    assert_eq!(plays(&repo, piste).0, 2, "la radio ne compte pas");
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
fn sqlite_seules_les_ecoutes_locales_comptent_pour_une_piste_locale_7980() {
    let base = SqliteDb::open_in_memory().unwrap();
    base.init_schema().unwrap();
    migrations::run_migrations(&base).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(base);
    poser_la_piste_locale(&db, 1);
    verifier(&db, 1);
}

#[cfg(feature = "postgres")]
#[tokio::test(flavor = "multi_thread")]
async fn pg_7980_seules_les_ecoutes_locales_comptent_pour_une_piste_locale() {
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
            "DELETE FROM tracks WHERE id = 7980".to_string(),
            "DELETE FROM albums WHERE id = 7980".to_string(),
            "DELETE FROM artists WHERE id = 7980".to_string(),
        ] {
            db.execute(&sql, &[]).unwrap();
        }
    };
    nettoyer(&db);
    poser_la_piste_locale(&db, 7980);
    verifier(&db, 7980);
    nettoyer(&db);
}
