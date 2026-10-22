//! #6500 — la passe ne retire QUE les stations indexées, et une seule fois.
//!
//! Le banc reproduit ce que le `.18` contenait le 19/10/2026 : des stations
//! `radio/<n>` d'un autre Tune, une vraie piste UPnP de durée nulle
//! (`track/<n>`), une piste locale, et une station qu'un favori référence.

use std::sync::Arc;

use super::*;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::sqlite::SqliteDb;

const UDN: &str = "uuid:9e933823-0000-0000-0000-000000000000";

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Une ligne `tracks` et son instantané UPnP, telle que l'indexation l'écrit.
fn ligne(
    db: &Arc<dyn DbBackend>,
    titre: &str,
    source: &str,
    file_path: Option<&str>,
    objet: Option<&str>,
    url: Option<&str>,
) -> i64 {
    let source_id = format!("{UDN}|{titre}");
    let p: [&dyn ToSqlValue; 4] = [&titre, &source, &source_id, &file_path];
    let id = db
        .execute_returning_id(
            "INSERT INTO tracks (title, source, source_id, file_path, duration_ms) \
             VALUES (?, ?, ?, ?, 0)",
            &p,
        )
        .unwrap();
    for (cle, valeur) in [("upnp_object_id", objet), ("upnp_res_url", url)] {
        if let Some(v) = valeur {
            let p: [&dyn ToSqlValue; 3] = [&id, &cle, &v];
            db.execute(
                "INSERT INTO track_metadata (track_id, key, value) VALUES (?, ?, ?)",
                &p,
            )
            .unwrap();
        }
    }
    id
}

fn existe(db: &Arc<dyn DbBackend>, id: i64) -> bool {
    let p: [&dyn ToSqlValue; 1] = [&id];
    db.query_one("SELECT id FROM tracks WHERE id = ?", &p)
        .unwrap()
        .is_some()
}

fn station(db: &Arc<dyn DbBackend>, n: i64, nom: &str) -> i64 {
    ligne(
        db,
        nom,
        "upnp",
        None,
        Some(&format!("radio/{n}")),
        Some(&format!(
            "http://192.0.2.15:8888/api/v1/radios/{n}/audio.wav"
        )),
    )
}

#[test]
fn i6500_la_passe_retire_les_stations_et_rien_d_autre() {
    let db = base();
    let fip = station(&db, 74, "FIP");
    let kexp = station(&db, 75, "KEXP");
    // Une vraie piste UPnP de durée nulle — il y en a sept sur le `.18`.
    let piste_upnp = ligne(
        &db,
        "Mariam Matrem",
        "upnp",
        None,
        Some("track/12"),
        Some("http://192.0.2.15:8888/api/v1/library/tracks/12/audio"),
    );
    // Une piste locale, même sans instantané.
    let locale = ligne(&db, "Locale", "local", Some("/m/a.flac"), None, None);
    // Une station au mauvais rayon : l'URL ne passe pas par les radios.
    let ambigue = ligne(
        &db,
        "radio/ sans route",
        "upnp",
        None,
        Some("radio/9"),
        Some("http://192.0.2.15:8888/api/v1/library/tracks/9/audio"),
    );
    // Une station que l'utilisateur a mise en favori : laissée.
    let favorite = station(&db, 76, "TSF Jazz");
    let p: [&dyn ToSqlValue; 1] = [&favorite.to_string()];
    db.execute(
        "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1, 'track', ?)",
        &p,
    )
    .unwrap();
    // L'appartenance de la synchronisation, qui ne doit pas survivre.
    db.execute(
        "INSERT INTO upnp_library_sources (source_key, udn, container, state_json) \
         VALUES ('s', 'u', '0', '{}')",
        &[],
    )
    .unwrap();
    for id in [fip, piste_upnp] {
        let p: [&dyn ToSqlValue; 1] = [&id];
        db.execute(
            "INSERT INTO upnp_library_members (source_key, track_id, generation) \
             VALUES ('s', ?, 'g')",
            &p,
        )
        .unwrap();
    }

    let bilan = retirer_les_radios_indexees(&db).unwrap();

    assert_eq!(
        bilan,
        Bilan {
            retirees: 2,
            gardees_liees: vec![favorite]
        },
        "deux stations retirées, la station favorite laissée"
    );
    assert!(
        !existe(&db, fip),
        "la station FIP reste une piste de bibliothèque"
    );
    assert!(
        !existe(&db, kexp),
        "la station KEXP reste une piste de bibliothèque"
    );
    assert!(
        existe(&db, piste_upnp),
        "une vraie piste UPnP de durée nulle a été retirée"
    );
    assert!(existe(&db, locale), "une piste locale a été retirée");
    assert!(
        existe(&db, ambigue),
        "une ligne sans la route des radios a été retirée"
    );
    assert!(existe(&db, favorite), "une station favorite a été retirée");
    let p: [&dyn ToSqlValue; 1] = [&fip];
    assert!(
        db.query_one("SELECT 1 FROM upnp_library_members WHERE track_id = ?", &p)
            .unwrap()
            .is_none(),
        "l'appartenance de la station retirée survit"
    );
    assert!(
        db.query_one("SELECT 1 FROM track_metadata WHERE track_id = ?", &p)
            .unwrap()
            .is_none(),
        "l'instantané de la station retirée survit"
    );

    let p: [&dyn ToSqlValue; 1] = [&piste_upnp];
    assert!(
        db.query_one("SELECT 1 FROM upnp_library_members WHERE track_id = ?", &p)
            .unwrap()
            .is_some(),
        "l'appartenance d'une vraie piste UPnP a été retirée"
    );

    // Idempotente : la seconde passe ne retire rien de plus.
    let encore = retirer_les_radios_indexees(&db).unwrap();
    assert_eq!(
        encore.retirees, 0,
        "la seconde passe retire encore quelque chose"
    );
    assert!(existe(&db, piste_upnp) && existe(&db, locale) && existe(&db, favorite));
}

#[test]
fn i6500_une_base_sans_station_ne_change_pas() {
    let db = base();
    let locale = ligne(&db, "Locale", "local", Some("/m/a.flac"), None, None);
    assert_eq!(retirer_les_radios_indexees(&db).unwrap(), Bilan::default());
    assert!(existe(&db, locale));
}
