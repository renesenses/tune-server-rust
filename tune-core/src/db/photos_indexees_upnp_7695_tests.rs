//! #7695 — la passe ne retire QUE les photos et vidéos indexées, et une seule
//! fois.
//!
//! Le banc : des photos DNG et JPEG d'un partage Windows (par le format, ou
//! par la seule extension de l'URL), une vraie piste UPnP en FLAC, une piste
//! UPnP `.mp4` en `audio/mp4`, une piste UPnP sans format ni extension, une
//! piste locale, et une photo qu'un favori référence. Et le revers : des
//! pistes Matroska audio (`audio/x-matroska` → `matroska`, URL `.mkv`), une
//! piste FLAC dont l'URL finit par une extension vidéo — elles restent.

use std::sync::Arc;

use super::*;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::sqlite::SqliteDb;

const UDN: &str = "uuid:9e933823-7695-0000-0000-000000000000";

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn album(db: &Arc<dyn DbBackend>, titre: &str) -> i64 {
    let p: [&dyn ToSqlValue; 2] = [&titre, &format!("{UDN}|{titre}")];
    db.execute_returning_id(
        "INSERT INTO albums (title, source, source_id) VALUES (?, 'upnp', ?)",
        &p,
    )
    .unwrap()
}

/// Une ligne `tracks` et son URL de lecture, telle que l'indexation l'écrit.
fn ligne(
    db: &Arc<dyn DbBackend>,
    titre: &str,
    source: &str,
    file_path: Option<&str>,
    format: Option<&str>,
    url: Option<&str>,
    album_id: Option<i64>,
) -> i64 {
    let source_id = format!("{UDN}|{titre}");
    let p: [&dyn ToSqlValue; 6] = [&titre, &source, &source_id, &file_path, &format, &album_id];
    let id = db
        .execute_returning_id(
            "INSERT INTO tracks (title, source, source_id, file_path, format, album_id, \
             duration_ms) VALUES (?, ?, ?, ?, ?, ?, 0)",
            &p,
        )
        .unwrap();
    if let Some(v) = url {
        let p: [&dyn ToSqlValue; 2] = [&id, &v];
        db.execute(
            "INSERT INTO track_metadata (track_id, key, value) VALUES (?, 'upnp_res_url', ?)",
            &p,
        )
        .unwrap();
    }
    id
}

fn existe(db: &Arc<dyn DbBackend>, table: &str, id: i64) -> bool {
    let p: [&dyn ToSqlValue; 1] = [&id];
    db.query_one(&format!("SELECT id FROM {table} WHERE id = ?"), &p)
        .unwrap()
        .is_some()
}

#[test]
fn i7695_la_passe_retire_les_photos_et_rien_d_autre() {
    let db = base();
    let pellicule = album(&db, "Pellicule");
    let disque = album(&db, "Disque");
    let dng = ligne(
        &db,
        "IMG_0042",
        "upnp",
        None,
        Some("adobe-dng"),
        Some("http://192.0.2.7:10243/WMPNSSv4/1/0_e0.dng"),
        Some(pellicule),
    );
    // Sans format rangé : l'extension de l'URL, casse et requête comprises.
    let jpeg = ligne(
        &db,
        "IMG_0043",
        "upnp",
        None,
        None,
        Some("http://192.0.2.7:10243/WMPNSSv4/1/IMG_0043.JPG?albumArt=true"),
        Some(pellicule),
    );
    let flac = ligne(
        &db,
        "Piste audio",
        "upnp",
        None,
        Some("flac"),
        Some("http://192.0.2.7:10243/WMPNSSv4/2/1.flac"),
        Some(disque),
    );
    let mp4 = ligne(
        &db,
        "Piste ALAC",
        "upnp",
        None,
        Some("mp4"),
        Some("http://192.0.2.7:10243/WMPNSSv4/2/2.mp4"),
        Some(disque),
    );
    let muette = ligne(
        &db,
        "Piste muette",
        "upnp",
        None,
        None,
        Some("http://192.0.2.7:10243/res/3"),
        None,
    );
    // Matroska audio : `audio/x-matroska` est rangé `matroska`, servi en `.mkv`.
    let matroska = ligne(
        &db,
        "Piste Matroska",
        "upnp",
        None,
        Some("matroska"),
        Some("http://192.0.2.9:8888/res/4.mkv"),
        Some(disque),
    );
    // Un format audio rangé l'emporte sur une extension vidéo de l'URL.
    let flac_avi = ligne(
        &db,
        "Piste FLAC en .avi",
        "upnp",
        None,
        Some("flac"),
        Some("http://192.0.2.9:8888/res/5.avi"),
        Some(disque),
    );
    let locale = ligne(
        &db,
        "Locale",
        "local",
        Some("/m/pochette.jpg"),
        Some("jpeg"),
        None,
        None,
    );
    let favorite = ligne(
        &db,
        "IMG_0044",
        "upnp",
        None,
        Some("adobe-dng"),
        Some("http://192.0.2.7:10243/WMPNSSv4/1/0_e1.dng"),
        None,
    );
    let p: [&dyn ToSqlValue; 1] = [&favorite.to_string()];
    db.execute(
        "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1, 'track', ?)",
        &p,
    )
    .unwrap();

    let bilan = retirer_les_photos_indexees(&db).unwrap();

    assert!(!existe(&db, "tracks", dng), "la photo DNG reste une piste");
    assert!(!existe(&db, "tracks", jpeg), "la photo JPG reste une piste");
    assert!(existe(&db, "tracks", flac), "une piste FLAC a été retirée");
    assert!(
        existe(&db, "tracks", mp4),
        "une piste audio .mp4 a été retirée"
    );
    assert!(
        existe(&db, "tracks", matroska),
        "une piste audio Matroska (format matroska, URL .mkv) a été retirée"
    );
    assert!(
        existe(&db, "tracks", flac_avi),
        "une piste au format flac a été retirée pour l'extension de son URL"
    );
    assert!(
        existe(&db, "tracks", muette),
        "une piste sans format ni extension a été retirée"
    );
    assert!(
        existe(&db, "tracks", locale),
        "une ligne locale a été retirée"
    );
    assert!(
        existe(&db, "tracks", favorite),
        "une photo favorite a été retirée"
    );
    assert!(
        !existe(&db, "albums", pellicule),
        "l'album vidé par la passe reste en place"
    );
    assert!(
        existe(&db, "albums", disque),
        "un album qui garde ses pistes a été retiré"
    );

    // Le bilan, une fois chaque ligne vérifiée : un échec nomme la ligne.
    assert_eq!(
        bilan,
        Bilan {
            retirees: 2,
            gardees_liees: vec![favorite],
            albums_vides_retires: 1,
        },
        "deux photos retirées, la photo favorite laissée, l'album « Pellicule » vidé"
    );

    // Idempotente : la seconde passe ne retire rien de plus.
    let encore = retirer_les_photos_indexees(&db).unwrap();
    assert_eq!(
        encore.retirees, 0,
        "la seconde passe retire encore quelque chose"
    );
    assert!(existe(&db, "tracks", flac) && existe(&db, "tracks", mp4));
    assert!(existe(&db, "tracks", matroska) && existe(&db, "tracks", flac_avi));
}

#[test]
fn i7695_une_base_sans_photo_ne_change_pas() {
    let db = base();
    let locale = ligne(
        &db,
        "Locale",
        "local",
        Some("/m/a.flac"),
        Some("flac"),
        None,
        None,
    );
    assert_eq!(retirer_les_photos_indexees(&db).unwrap(), Bilan::default());
    assert!(existe(&db, "tracks", locale));
}
