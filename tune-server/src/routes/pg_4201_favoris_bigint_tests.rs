//! #4201 — sur une base PostgreSQL 18 migrée, `favorites.item_id` est BIGINT,
//! et `f.item_id = CAST(albums.id AS TEXT)` y rend « operator does not exist:
//! bigint = text » : le premier import complet finissait `partial`. Chaque
//! requête UPnP qui cherche un favori est jouée ici sur une VRAIE base, dans un
//! schéma à elle dont `favorites.item_id` est BIGINT, puis dans un second où il
//! est TEXT (`PG_FULL_SCHEMA`). Variable `TUNE_TEST_PG_URL` absente : saut
//! annoncé ; posée mais injoignable : le test tombe.

use std::sync::Arc;

use tune_core::db::backend::DbBackend;

async fn schema_d_essai(url: &str, schema: &str, type_item_id: &str) -> Arc<dyn DbBackend> {
    let racine = tune_core::db::postgres::PostgresDb::connect(url)
        .await
        .expect("connexion PostgreSQL");
    let admin: Arc<dyn DbBackend> = Arc::new(tune_core::db::backend::PostgresBackend::new(
        racine.pool().clone(),
    ));
    tokio::task::block_in_place(|| {
        admin
            .execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"), &[])
            .unwrap();
        admin
            .execute(&format!("CREATE SCHEMA {schema}"), &[])
            .unwrap();
    });
    let sep = if url.contains('?') { '&' } else { '?' };
    let pg = tune_core::db::postgres::PostgresDb::connect(&format!(
        "{url}{sep}options=-csearch_path%3D{schema}"
    ))
    .await
    .expect("connexion au schéma d'essai");
    let db: Arc<dyn DbBackend> = Arc::new(tune_core::db::backend::PostgresBackend::new(
        pg.pool().clone(),
    ));
    tokio::task::block_in_place(|| {
        for ddl in [
            "CREATE TABLE albums (id BIGINT PRIMARY KEY, title TEXT, source TEXT, source_id TEXT)"
                .to_string(),
            "CREATE TABLE tracks (id BIGINT PRIMARY KEY, album_id BIGINT, source TEXT, \
             source_id TEXT)"
                .to_string(),
            "CREATE TABLE playlist_tracks (playlist_id BIGINT, track_id BIGINT)".to_string(),
            format!(
                "CREATE TABLE favorites (profile_id BIGINT DEFAULT 1, item_type TEXT, \
                 item_id {type_item_id})"
            ),
            // Album 1 : distant, vide, EN FAVORI. Album 2 : distant, vide, libre.
            "INSERT INTO albums VALUES (1, 'gardé', 'upnp', 'uuid:nas|a1'), \
             (2, 'retiré', 'upnp', 'uuid:nas|a2'), (3, 'avec piste', 'upnp', 'uuid:nas|a3')"
                .to_string(),
            "INSERT INTO tracks VALUES (10, 3, 'upnp', 'uuid:nas|t10'), \
             (11, 3, 'upnp', 'uuid:nas|t11')"
                .to_string(),
            "INSERT INTO favorites (item_type, item_id) VALUES ('album', 1), ('track', 10)"
                .to_string(),
        ] {
            db.execute(&ddl, &[])
                .unwrap_or_else(|e| panic!("{ddl} : {e}"));
        }
    });
    db
}

async fn jouer(type_item_id: &str, schema: &str) {
    let Ok(url) = std::env::var("TUNE_TEST_PG_URL") else {
        eprintln!("SAUT: TUNE_TEST_PG_URL absent — épreuve PostgreSQL #4201 sautée");
        return;
    };
    let db = schema_d_essai(&url, schema, type_item_id).await;
    tokio::task::block_in_place(|| {
        // Indexation : retrait des albums distants vides (« bigint = text » du 29/10).
        let filtre = "uuid:nas|%".to_string();
        let retires = db
            .execute(
                &super::super::indexation_upnp::sql_retrait_des_albums_vides("$1"),
                &[&filtre as &dyn tune_core::db::backend::ToSqlValue],
            )
            .unwrap_or_else(|e| {
                panic!("item_id {type_item_id} : retrait des albums vides en échec : {e}")
            });
        assert_eq!(retires, 1, "seul l'album vide SANS favori part");

        // Synchronisation : liens utilisateur d'une piste (favori de piste,
        // favori de l'album de la piste).
        let lie = db
            .query_one(&super::liens_utilisateur_sql("10,11"), &[])
            .unwrap_or_else(|e| panic!("item_id {type_item_id} : liens utilisateur : {e}"));
        assert!(lie.is_some(), "la piste 10 est en favori");
        let libre = db
            .query_one(&super::liens_utilisateur_sql("11"), &[])
            .unwrap_or_else(|e| panic!("item_id {type_item_id} : liens utilisateur : {e}"));
        assert!(libre.is_none(), "la piste 11 n'a aucun lien");

        // Synchronisation : retrait d'un album distant vidé — refusé en favori.
        let n = db
            .execute(&super::sql_retrait_d_un_album_distant_vide(), &[&1i64])
            .unwrap_or_else(|e| panic!("item_id {type_item_id} : retrait d'album : {e}"));
        assert_eq!(n, 0, "l'album 1 est en favori : il reste");

        // Retrait UPnP : le compte des liens.
        let sql = format!(
            "SELECT COUNT(*) FROM tracks t WHERE {}",
            super::super::retrait_upnp::lien_favori("t")
        );
        db.query_one(&sql, &[])
            .unwrap_or_else(|e| panic!("item_id {type_item_id} : compte des favoris : {e}"));

        // tune-core : radios et photos indexées depuis un serveur UPnP.
        for (nom, sql) in [
            (
                "radios LIEE",
                tune_core::db::radios_indexees_upnp_6500::LIEE,
            ),
            (
                "photos LIEE",
                tune_core::db::photos_indexees_upnp_7695::LIEE,
            ),
        ] {
            let lie = db
                .query_one(sql, &[&10i64])
                .unwrap_or_else(|e| panic!("item_id {type_item_id} : {nom} : {e}"));
            assert!(lie.is_some(), "{nom} : la piste 10 est en favori");
        }
        let n = db
            .execute(
                tune_core::db::photos_indexees_upnp_7695::ALBUM_VIDE,
                &[&1i64],
            )
            .unwrap_or_else(|e| panic!("item_id {type_item_id} : photos ALBUM_VIDE : {e}"));
        assert_eq!(n, 0, "photos : l'album 1 est en favori, il reste");
    });
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_4201_favoris_item_id_bigint() {
    jouer("BIGINT", "essai_4201_bigint").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pg_4201_favoris_item_id_text() {
    jouer("TEXT", "essai_4201_text").await;
}
