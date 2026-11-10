//! #8239 — le journal de synchro après un scan, sur PostgreSQL.
//!
//! Les gardes SQLite vivent dans `cloud::library_sync` (module
//! `journal_apres_scan_8239_tests`). Celle-ci éprouve les MÊMES requêtes
//! (`CREATE TABLE … AS`, `IS DISTINCT FROM`, `NOT EXISTS`) sur une vraie base
//! PostgreSQL.
//!
//! Doctrine de `pg_insert_conditionnel_f7c250.rs` : `TUNE_TEST_PG_URL`
//! absente ⇒ saut annoncé ; posée mais injoignable ⇒ le test TOMBE. Les tables
//! sont partagées avec les autres bancs : on ne compte que nos propres lignes.

#[cfg(feature = "postgres")]
mod postgres {
    use std::sync::Arc;

    use tune_core::cloud::library_sync::{journaliser_ce_qui_a_change, preparer_le_releve};
    use tune_core::db::backend::{DbBackend, PostgresBackend};
    use tune_core::db::postgres::PostgresDb;

    const MARQUE: &str = "j8239";

    async fn backend(url: &str) -> Arc<dyn DbBackend> {
        let db = PostgresDb::connect(url)
            .await
            .unwrap_or_else(|e| panic!("TUNE_TEST_PG_URL posée mais injoignable ({url}) : {e}"));
        Arc::new(PostgresBackend::new(db.pool().clone()))
    }

    fn id(db: &Arc<dyn DbBackend>, sql: &str) -> i64 {
        db.query_one(sql, &[])
            .unwrap_or_else(|e| panic!("{sql} : {e}"))
            .unwrap_or_else(|| panic!("{sql} : aucune ligne"))[0]
            .as_i64()
            .unwrap()
    }

    fn en_attente(db: &Arc<dyn DbBackend>, ids: &[(&str, i64)]) -> Vec<(String, i64, String)> {
        let mut v = Vec::new();
        for (t, i) in ids {
            for r in db
                .query_many(
                    &format!(
                        "SELECT entity_type, entity_id, action FROM sync_changelog \
                         WHERE synced = 0 AND entity_type = '{t}' AND entity_id = {i}"
                    ),
                    &[],
                )
                .unwrap()
            {
                v.push((
                    r[0].as_string().unwrap(),
                    r[1].as_i64().unwrap(),
                    r[2].as_string().unwrap(),
                ));
            }
        }
        v
    }

    fn menage(db: &Arc<dyn DbBackend>) {
        db.execute_batch(&format!(
            "DELETE FROM tracks WHERE title LIKE '{MARQUE}-%';\
             DELETE FROM albums WHERE title LIKE '{MARQUE}-%';\
             DELETE FROM artists WHERE name LIKE '{MARQUE}-%'"
        ))
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pg_8239_seul_ce_qui_a_change_entre_au_journal() {
        let Some(url) = std::env::var("TUNE_TEST_PG_URL").ok() else {
            eprintln!("TUNE_TEST_PG_URL absente — épreuve PostgreSQL sautée");
            return;
        };
        let db = backend(&url).await;
        menage(&db);

        // Bibliothèque déjà poussée : un artiste, un album, une piste, leurs
        // entrées du journal toutes `synced = 1`.
        db.execute_batch(&format!(
            "INSERT INTO artists (name) VALUES ('{MARQUE}-artiste');\
             INSERT INTO albums (title, artist_id) \
             SELECT '{MARQUE}-album', id FROM artists WHERE name = '{MARQUE}-artiste';\
             INSERT INTO tracks (title, album_id, artist_id, file_path) \
             SELECT '{MARQUE}-p1', al.id, al.artist_id, '{MARQUE}/p1.flac' \
             FROM albums al WHERE al.title = '{MARQUE}-album'"
        ))
        .unwrap();
        let artiste = id(&db, &format!("SELECT id FROM artists WHERE name = '{MARQUE}-artiste'"));
        let album = id(&db, &format!("SELECT id FROM albums WHERE title = '{MARQUE}-album'"));
        let p1 = id(&db, &format!("SELECT id FROM tracks WHERE title = '{MARQUE}-p1'"));
        db.execute_batch(&format!(
            "INSERT INTO sync_changelog (entity_type, entity_id, action, synced) VALUES \
             ('artist', {artiste}, 'upsert', 1), ('album', {album}, 'upsert', 1), \
             ('track', {p1}, 'upsert', 1)"
        ))
        .unwrap();

        // Relevé repris de zéro : celui d'un passage précédent du banc
        // ignorerait les lignes posées ci-dessus.
        db.execute_batch(
            "DROP TABLE IF EXISTS sync_releve_pistes;\
             DROP TABLE IF EXISTS sync_releve_albums;\
             DROP TABLE IF EXISTS sync_releve_artistes",
        )
        .unwrap();

        // Scan qui ajoute une piste à l'album.
        preparer_le_releve(&db);
        db.execute_batch(&format!(
            "INSERT INTO tracks (title, album_id, artist_id, file_path) \
             VALUES ('{MARQUE}-p2', {album}, {artiste}, '{MARQUE}/p2.flac')"
        ))
        .unwrap();
        let p2 = id(&db, &format!("SELECT id FROM tracks WHERE title = '{MARQUE}-p2'"));
        journaliser_ce_qui_a_change(&db);

        let a_moi = [("artist", artiste), ("album", album), ("track", p1), ("track", p2)];
        let mut vu = en_attente(&db, &a_moi);
        vu.sort();
        assert_eq!(
            vu,
            vec![
                ("album".to_string(), album, "upsert".to_string()),
                ("track".to_string(), p2, "upsert".to_string()),
            ],
            "seuls la piste ajoutée et son album (nombre de pistes) entrent au journal"
        );

        // Scan qui retire la piste : elle part en `delete`.
        db.execute_batch(&format!(
            "UPDATE sync_changelog SET synced = 1 WHERE entity_type IN ('album','track') \
             AND entity_id IN ({album}, {p2})"
        ))
        .unwrap();
        db.execute_batch(&format!("DELETE FROM tracks WHERE id = {p2}")).unwrap();
        journaliser_ce_qui_a_change(&db);
        let mut vu = en_attente(&db, &a_moi);
        vu.sort();
        assert_eq!(
            vu,
            vec![
                ("album".to_string(), album, "upsert".to_string()),
                ("track".to_string(), p2, "delete".to_string()),
            ]
        );

        db.execute_batch(&format!(
            "DELETE FROM sync_changelog WHERE (entity_type = 'artist' AND entity_id = {artiste}) \
             OR (entity_type = 'album' AND entity_id = {album}) \
             OR (entity_type = 'track' AND entity_id IN ({p1}, {p2}))"
        ))
        .unwrap();
        menage(&db);
    }
}
