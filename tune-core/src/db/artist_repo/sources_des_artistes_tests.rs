//! Témoins de [`super::ArtistRepo::sources_des_artistes`] — tune-web-client#1716.
//!
//! La table rendue doit être celle que le client calculait avec
//! `sourcesParArtiste(albums, pistes)` sur les listes `GET /library/albums` et
//! `GET /library/tracks` : mêmes clés de provenance, mêmes exclusions.

use super::*;
use crate::db::sqlite::SqliteDb;

fn base() -> SqliteDb {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    {
        let c = db.connection().lock().unwrap();
        c.execute_batch(
            "INSERT INTO artists (id, name) VALUES (1, 'Local Seul'), (2, 'Deux Sources'), \
               (3, 'Invité'), (4, 'Masqué'), (5, 'Doublé');
             INSERT INTO albums (id, title, artist_id, source, source_id) VALUES
               (10, 'A', 1, 'local', NULL),
               (20, 'B', 2, 'local', NULL),
               (21, 'C', 2, 'upnp', 'uuid:serveur-1|obj-21'),
               (22, 'D', 2, 'upnp', 'sans-barre'),
               (40, 'Caché', 4, 'local', NULL),
               (50, 'Même disque', 5, 'local', NULL),
               (51, 'Même disque', 5, 'upnp', 'uuid:serveur-2|obj-51');
             INSERT INTO hidden_items (item_type, item_id) VALUES ('album', 40);
             INSERT INTO tracks (id, title, album_id, artist_id, track_number, file_path, source, source_id, format) VALUES
               (100, 'a1', 10, 1, 1, '/m/a1.flac', 'local', NULL, 'flac'),
               (101, 'a1', 10, 1, 1, '/m/a1.mp3', 'local', NULL, 'mp3'),
               (102, 'a2', 10, 3, 2, '/m/a2.flac', 'local', NULL, 'flac'),
               (200, 'b1', 20, 2, 1, '/m/b1.flac', 'local', NULL, 'flac'),
               (210, 'c1', 21, 2, 1, NULL, 'upnp', 'uuid:serveur-1|t210', 'flac'),
               (211, 'c2', 21, 3, 2, NULL, 'upnp', 'uuid:serveur-1|t211', 'flac'),
               (400, 'x', 40, 4, 1, '/m/x.flac', 'local', NULL, 'flac'),
               (510, 'y', 51, 5, 1, NULL, 'upnp', 'uuid:serveur-2|t510', 'flac');",
        )
        .unwrap();
    }
    db
}

fn presence(albums: i64, pistes: i64) -> PresenceDansUneSource {
    PresenceDansUneSource { albums, pistes }
}

#[test]
fn les_sources_reunissent_albums_et_pistes_avec_les_cles_du_client() {
    let repo = ArtistRepo::new(base());
    let t = repo.sources_des_artistes(&[1, 2, 3, 4, 5]).unwrap();

    // La copie MP3 de « a1 » est repliée (copie de moindre qualité) : une piste.
    assert_eq!(
        t[&1],
        BTreeMap::from([("local".to_string(), presence(1, 1))])
    );
    // `uuid:serveur-1|…` → `upnp:uuid:serveur-1` ; sans barre → `upnp`.
    assert_eq!(
        t[&2],
        BTreeMap::from([
            ("local".to_string(), presence(1, 1)),
            ("upnp".to_string(), presence(1, 0)),
            ("upnp:uuid:serveur-1".to_string(), presence(1, 1)),
        ])
    );
    // L'invité d'une compilation est de la source de ses PISTES, sans album.
    assert_eq!(
        t[&3],
        BTreeMap::from([
            ("local".to_string(), presence(0, 1)),
            ("upnp:uuid:serveur-1".to_string(), presence(0, 1)),
        ])
    );
    // Album masqué : ni lui ni ses pistes.
    assert!(!t.contains_key(&4));
    // L'album distant doublé par un local disparaît des listes, ses pistes aussi.
    assert_eq!(
        t[&5],
        BTreeMap::from([("local".to_string(), presence(1, 0))])
    );
}

#[test]
fn seuls_les_artistes_demandes_sont_rendus() {
    let repo = ArtistRepo::new(base());
    let t = repo.sources_des_artistes(&[1]).unwrap();
    assert_eq!(t.keys().copied().collect::<Vec<_>>(), vec![1]);
    assert!(repo.sources_des_artistes(&[]).unwrap().is_empty());
}
