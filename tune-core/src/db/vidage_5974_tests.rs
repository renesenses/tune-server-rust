//! #5974 — « Vider la bibliothèque » ne doit jamais annoncer un vidage qu'il
//! n'a pas fait (JP Borderies, Windows, 1.0.0-rc2, fil 2171).
//!
//! `TrackRepo::delete_all` lançait `DELETE FROM albums` et `DELETE FROM
//! artists` sous `let _ =` : un échec était avalé, la transaction validée, la
//! route répondait `ok: true`. Les pistes partaient, les albums restaient —
//! titre et `folder_path` compris — et le scan complet suivant y rattachait
//! les fichiers de ces dossiers (`get_or_create_for_folder_with_track`
//! reprend l'album du dossier sans toucher à son titre).
//!
//! L'échec n'est pas théorique en rc2 : `albums_fts` y retirait une ligne
//! par ses VALEURS, l'artiste étant relu au moment du retrait. Un artiste
//! renommé après l'indexation de son album, et `DELETE FROM albums` répond
//! « database disk image is malformed » (#5919, corrigé en rc4 pour l'index ;
//! le vidage, lui, avalait toujours l'erreur). Le banc remonte cet index-là,
//! recopié de la rc2.

use super::backend::{DbBackend, ToSqlValue};
use super::sqlite::SqliteDb;
use super::track_repo::TrackRepo;
use std::sync::Arc;

/// `albums_fts` tel que la 1.0.0-rc2 le posait : contenu externe, retrait
/// par valeurs, artiste recalculé.
const ALBUMS_FTS_RC2: &str = "\
DROP TRIGGER IF EXISTS albums_fts_insert;
DROP TRIGGER IF EXISTS albums_fts_update;
DROP TRIGGER IF EXISTS albums_fts_delete;
DROP TABLE IF EXISTS albums_fts;
CREATE VIRTUAL TABLE albums_fts USING fts5(
    title, artist_name, genre,
    tokenize='unicode61 remove_diacritics 2',
    content='albums', content_rowid='id'
);
CREATE TRIGGER albums_fts_insert AFTER INSERT ON albums BEGIN
    INSERT INTO albums_fts(rowid, title, artist_name, genre)
    VALUES (new.id, new.title,
            (SELECT name FROM artists WHERE id = new.artist_id),
            new.genre);
END;
CREATE TRIGGER albums_fts_delete AFTER DELETE ON albums BEGIN
    INSERT INTO albums_fts(albums_fts, rowid, title, artist_name, genre)
    VALUES ('delete', old.id, old.title,
            (SELECT name FROM artists WHERE id = old.artist_id),
            old.genre);
END;";

fn compte(db: &Arc<dyn DbBackend>, table: &str) -> i64 {
    db.query_one(&format!("SELECT COUNT(*) FROM {table}"), &[])
        .unwrap()
        .and_then(|r| r.first()?.as_i64())
        .unwrap_or(-1)
}

/// Une base à l'index rc2, quarante albums de quarante artistes, une piste
/// chacun ; puis les artistes renommés (ARTISTSORT, fusion, enrichissement :
/// tout ce qui réécrit `artists.name` sans réindexer `albums_fts`).
fn base_rc2_aux_artistes_renommes() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    db.execute_batch(ALBUMS_FTS_RC2).unwrap();
    for i in 0..40i64 {
        let nom = format!("Artiste {i}");
        let titre = format!("Album retiré {i}");
        let dossier = format!("C:\\Musique\\Album {i}");
        let chemin = format!("{dossier}\\01.flac");
        db.execute(
            "INSERT INTO artists (id, name) VALUES (?, ?)",
            &[&i as &dyn ToSqlValue, &nom],
        )
        .unwrap();
        db.execute(
            "INSERT INTO albums (id, title, artist_id, folder_path) VALUES (?, ?, ?, ?)",
            &[&i as &dyn ToSqlValue, &titre, &i, &dossier],
        )
        .unwrap();
        db.execute(
            "INSERT INTO tracks (title, album_id, artist_id, file_path) VALUES ('t', ?, ?, ?)",
            &[&i as &dyn ToSqlValue, &i, &chemin],
        )
        .unwrap();
    }
    db.execute("UPDATE artists SET name = name || ' (renommé)'", &[])
        .unwrap();
    db
}

/// Le témoin. Sans le correctif : `Ok(40)`, les 40 albums restent en base.
#[test]
fn un_vidage_qui_echoue_sur_les_albums_ne_se_dit_pas_reussi_5974() {
    let db = base_rc2_aux_artistes_renommes();
    let resultat = TrackRepo::with_backend(db.clone()).delete_all();
    let albums_restants = compte(&db, "albums");
    assert!(
        resultat.is_err(),
        "delete_all a répondu {resultat:?} alors que {albums_restants} album(s) \
         sont restés en base : le vidage s'annonce réussi sans l'être (#5974)"
    );
}

/// Et l'échec n'emporte rien : la transaction est annulée, la bibliothèque
/// reste telle qu'elle était, pistes comprises — pas de pistes vidées sous
/// des albums conservés.
#[test]
fn un_vidage_refuse_laisse_la_bibliotheque_entiere_5974() {
    let db = base_rc2_aux_artistes_renommes();
    let _ = TrackRepo::with_backend(db.clone()).delete_all();
    assert_eq!(
        (
            compte(&db, "tracks"),
            compte(&db, "albums"),
            compte(&db, "artists")
        ),
        (40, 40, 40),
        "un vidage refusé ne doit laisser ni pistes orphelines d'albums, ni \
         albums sans pistes"
    );
}

/// Non-régression : sur une base saine, le vidage vide tout et compte les
/// pistes.
#[test]
fn un_vidage_sain_vide_pistes_albums_et_artistes_5974() {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    db.execute("INSERT INTO artists (id, name) VALUES (1, 'A')", &[])
        .unwrap();
    db.execute(
        "INSERT INTO albums (id, title, artist_id) VALUES (1, 'X', 1)",
        &[],
    )
    .unwrap();
    db.execute(
        "INSERT INTO tracks (title, album_id, artist_id, file_path) VALUES ('t', 1, 1, '/m/x.flac')",
        &[],
    )
    .unwrap();
    let n = TrackRepo::with_backend(db.clone()).delete_all().unwrap();
    assert_eq!(n, 1);
    assert_eq!(
        (
            compte(&db, "tracks"),
            compte(&db, "albums"),
            compte(&db, "artists")
        ),
        (0, 0, 0)
    );
}
