//! #7765 — `track_credits.source` : la migration 136 et son attribution
//! unique, l'écriture MusicBrainz qui ne purge que ses lignes, l'import Roon
//! qui complète une piste créditée par ses seules balises.
//!
//! Le témoin Discogs vit dans `discogs_propositions` (il rejoue son
//! transport enregistré), ceux des passes du serveur dans
//! `tune-server/src/routes/library/credits.rs`, le jumeau PostgreSQL dans
//! `tune-server/tests/pg_credits_source_7765.rs`.

use std::sync::Arc;

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::metadata::credits_mb::LigneCredit;
use crate::metadata::credits_release::ecrire_credits_piste;
use crate::metadata::source_des_credits::{BALISES, DISCOGS, MUSICBRAINZ, ROON};

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    Arc::new(db)
}

fn colonnes(db: &SqliteDb, table: &str) -> Vec<String> {
    let conn = db.connection().lock().unwrap();
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    let rows = stmt.query_map([], |r| r.get::<_, String>(1)).unwrap();
    rows.map(|r| r.unwrap()).collect()
}

/// `(nom, source)` des crédits d'une piste, dans l'ordre d'écriture.
fn credits(b: &Arc<dyn DbBackend>, track_id: i64) -> Vec<(String, Option<String>)> {
    b.query_many(
        "SELECT artist_name, source FROM track_credits WHERE track_id = ? ORDER BY id",
        &[&track_id as &dyn ToSqlValue],
    )
    .unwrap()
    .iter()
    .map(|r| {
        (
            r.first().and_then(|v| v.as_string()).unwrap_or_default(),
            r.get(1).and_then(|v| v.as_string()),
        )
    })
    .collect()
}

fn ligne(nom: &str) -> LigneCredit {
    LigneCredit {
        artist_name: nom.into(),
        role: "performer".into(),
        instrument: None,
        artist_mbid: None,
    }
}

/// Base ANCIENNE : `track_credits` sans `source`,
/// avec trois crédits déjà écrits — un sur une piste importée de Roon, un
/// pourvu d'un MBID d'artiste (passe MusicBrainz), un sans rien. La 136 les
/// attribue `roon`, `musicbrainz` et NULL, une seule fois.
#[test]
fn la_migration_136_pose_la_source_et_attribue_une_fois_les_lignes_existantes() {
    let neuve = SqliteDb::open_in_memory().unwrap();
    neuve.init_schema().unwrap();
    run_migrations(&neuve).unwrap();
    assert!(
        colonnes(&neuve, "track_credits")
            .iter()
            .any(|c| c == "source"),
        "base neuve : `track_credits.source` manque"
    );

    // `track_credits` telle que la 107 la laissait (avec `artist_mbid`, sans
    // `source`) et `track_metadata` telle que la 34 la créait, remplies AVANT
    // le schéma : toutes les migrations se jouent ensuite, la 136 comprise.
    let ancienne = SqliteDb::open_in_memory().unwrap();
    ancienne
        .execute_batch(
            "CREATE TABLE track_credits (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                track_id INTEGER NOT NULL,
                artist_id INTEGER,
                artist_name TEXT NOT NULL,
                role TEXT DEFAULT 'performer',
                instrument TEXT,
                position INTEGER DEFAULT 0,
                artist_mbid TEXT
             );
             CREATE TABLE track_metadata (
                track_id INTEGER NOT NULL,
                key TEXT NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY (track_id, key)
             );
             INSERT INTO track_metadata (track_id, key, value) VALUES (1, 'credits_source', 'roon');
             INSERT INTO track_credits (track_id, artist_name, role, artist_mbid) VALUES
                (1, 'David Eugene Edwards', 'composer', NULL),
                (2, 'Neil Young', 'composer', '75167b8b-44e4-407b-9d35-effe87b223cf'),
                (3, 'Inconnu', 'performer', '');",
        )
        .unwrap();
    assert!(
        !colonnes(&ancienne, "track_credits")
            .iter()
            .any(|c| c == "source"),
        "le banc n'a pas reconstitué une base d'avant la 136"
    );
    ancienne.init_schema().unwrap();
    run_migrations(&ancienne).unwrap();
    assert!(
        colonnes(&ancienne, "track_credits")
            .iter()
            .any(|c| c == "source"),
        "base ancienne : `track_credits.source` manque après la 136"
    );

    let b: Arc<dyn DbBackend> = Arc::new(ancienne);
    let source_de = |track_id: i64| credits(&b, track_id)[0].1.clone();
    assert_eq!(
        source_de(1).as_deref(),
        Some(ROON),
        "piste 1 « Hutterite Mile » : marquée `credits_source = roon`, son crédit doit être `roon`"
    );
    assert_eq!(
        source_de(2).as_deref(),
        Some(MUSICBRAINZ),
        "piste 2 « Helpless » : crédit avec MBID d'artiste, il doit être `musicbrainz`"
    );
    assert_eq!(
        source_de(3),
        None,
        "piste 3 « Sans origine » : ni Roon ni MBID, la source reste inconnue"
    );

    // La jumelle PG existe, est enregistrée au rang 101 et marque ce numéro.
    let sql_pg = include_str!("../../migrations/postgres/101_credits_source.sql");
    // `PG_MIGRATIONS` n'existe que sous la fonctionnalité `postgres` : on lit
    // son entrée dans le source, blancs retirés, pour ne dépendre ni de la mise
    // en forme ni de la fonctionnalité.
    let migrations: String = include_str!("../db/migrations.rs")
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    assert!(
        migrations.contains(
            "(101,\"credits_source\",include_str!(\"../../migrations/postgres/101_credits_source.sql\"),)"
        ),
        "101_credits_source.sql n'est pas enregistrée au rang 101 dans PG_MIGRATIONS"
    );
    assert!(sql_pg.contains("VALUES (101, 'credits_source')"));
    for attendu in [
        "ADD COLUMN IF NOT EXISTS source TEXT",
        "ON track_credits(track_id, source)",
        "m.key = 'credits_source' AND m.value = 'roon') THEN 'roon'",
        "WHEN artist_mbid IS NOT NULL AND artist_mbid <> '' THEN 'musicbrainz'",
        "WHERE source IS NULL;",
    ] {
        assert!(
            sql_pg.contains(attendu),
            "la jumelle PG ne contient pas « {attendu} »"
        );
    }
}

/// Rejouée sur une base déjà à jour, la migration ne réattribue rien : une
/// ligne NULL après la 136 le reste.
#[test]
fn un_redemarrage_ne_reattribue_pas_les_sources() {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    db.execute_batch(
        "INSERT INTO tracks (id, title, file_path) VALUES (2, 'Helpless', '/m/2.flac');
         INSERT INTO track_credits (track_id, artist_name, role, artist_mbid, source) VALUES
            (2, 'Neil Young', 'composer', '75167b8b-44e4-407b-9d35-effe87b223cf', NULL);",
    )
    .unwrap();
    run_migrations(&db).unwrap();
    let b: Arc<dyn DbBackend> = Arc::new(db);
    assert_eq!(
        credits(&b, 2)[0].1,
        None,
        "piste 2 « Helpless » : l'attribution a été rejouée au redémarrage"
    );
}

/// LE témoin de la purge : la passe MusicBrainz réécrit ses crédits sans
/// toucher à ceux lus dans les balises ni à ceux d'une autre source ; elle
/// emporte ses propres lignes et les lignes de source inconnue.
#[test]
fn la_ligne_balises_survit_a_la_reecriture_musicbrainz() {
    let b = base();
    b.execute(
        "INSERT INTO tracks (id, title, file_path) VALUES (40, 'So What', '/m/40.flac')",
        &[],
    )
    .unwrap();
    b.execute_batch(
        "INSERT INTO track_credits (track_id, artist_name, role, source) VALUES
            (40, 'Miles Davis', 'performer', 'balises'),
            (40, 'Bill Evans', 'performer', 'discogs'),
            (40, 'Ancienne ligne', 'performer', NULL);",
    )
    .unwrap();

    for passe in 1..=2 {
        let ecrites = ecrire_credits_piste(
            &b,
            40,
            MUSICBRAINZ,
            &[ligne("Paul Chambers"), ligne("Jimmy Cobb")],
        );
        assert_eq!(ecrites, 2, "passe {passe}");
    }
    let lus = credits(&b, 40);
    assert!(
        lus.contains(&("Miles Davis".into(), Some(BALISES.into()))),
        "piste 40 « So What » : la réécriture MusicBrainz a effacé le crédit lu dans les balises : {lus:?}"
    );
    assert!(
        lus.contains(&("Bill Evans".into(), Some(DISCOGS.into()))),
        "piste 40 « So What » : la réécriture MusicBrainz a effacé un crédit Discogs : {lus:?}"
    );
    assert!(
        !lus.iter().any(|(n, _)| n == "Ancienne ligne"),
        "piste 40 « So What » : une ligne de source inconnue doit être remplacée : {lus:?}"
    );
    let mb: Vec<&str> = lus
        .iter()
        .filter(|(_, s)| s.as_deref() == Some(MUSICBRAINZ))
        .map(|(n, _)| n.as_str())
        .collect();
    assert_eq!(
        mb,
        vec!["Paul Chambers", "Jimmy Cobb"],
        "piste 40 « So What » : deux écritures identiques doivent laisser une seule fois les lignes MusicBrainz"
    );
    assert_eq!(lus.len(), 4, "{lus:?}");
}

/// L'import Roon complète une piste qui n'a que des crédits de balises, et
/// écrit `roon` ; la ligne de balises reste.
#[test]
fn l_import_roon_complete_une_piste_creditee_par_ses_seules_balises() {
    use crate::library::pont_roon::ExportRoon;
    use crate::library::pont_roon_import::appliquer;
    let b = base();
    b.execute_batch(
        "INSERT INTO artists (id, name) VALUES (1, '16 Horsepower');
         INSERT INTO albums (id, title, artist_id) VALUES (1, 'Folklore', 1);
         INSERT INTO tracks (id, title, album_id, artist_id, track_number, file_path, source) VALUES
            (10, 'Hutterite Mile', 1, 1, 1, '/m/1.flac', 'local');
         INSERT INTO track_credits (track_id, artist_name, role, source) VALUES
            (10, 'Pascal Humbert', 'performer', 'balises');",
    )
    .unwrap();
    let export = ExportRoon::lire(
        r#"{"source":"roon","artistes":[{"nom":"16 Horsepower","albums":[
          {"titre":"Folklore","pistes":[
            {"titre":"1. Hutterite Mile","credits":"16 Horsepower, David Eugene Edwards"}]}]}]}"#,
    )
    .unwrap();
    let r = appliquer(&b, &export, false, None);
    assert_eq!(
        (r.credits_deja_presents, r.credits_ecrits),
        (0, 1),
        "piste 10 « Hutterite Mile » : ses seuls crédits de balises la font passer pour déjà créditée : {r:?}"
    );
    let lus = credits(&b, 10);
    assert!(
        lus.contains(&("David Eugene Edwards".into(), Some(ROON.into()))),
        "piste 10 : le crédit importé de Roon doit porter `roon` : {lus:?}"
    );
    assert!(
        lus.contains(&("Pascal Humbert".into(), Some(BALISES.into()))),
        "piste 10 : la ligne de balises a disparu : {lus:?}"
    );
}
