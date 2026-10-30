use super::*;
use crate::db::{migrations, sqlite::SqliteDb, zone_repo::ZoneRepo};

fn base() -> (Arc<dyn DbBackend>, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let zone = ZoneRepo::with_backend(db.clone())
        .create("Recette muette", Some("browser"), None)
        .unwrap();
    (db, zone)
}

fn piste(id: &str) -> QueueInput {
    QueueInput::Streaming {
        source: "qobuz".into(),
        source_id: id.into(),
        title: id.into(),
        artist: "Artiste".into(),
        album: Some("Album".into()),
        cover_url: None,
        duration_ms: 1000,
        track_number: None,
        disc_number: None,
        album_ref: None,
        artist_ref: None,
    }
}

fn reprise(zone: i64, id: &str, lignes: Vec<i64>) -> Resume {
    Resume {
        resume_key: format!("cle-{id}"),
        context_type: "album".into(),
        context_id: id.into(),
        source: "qobuz".into(),
        title: id.into(),
        last_track_title: id.into(),
        cover_path: None,
        album_id: None,
        position: 0,
        shuffled: false,
        queue_row_ids: lignes,
        position_ms: 0,
        updated_at: format!("2026-10-30T10:00:0{zone}Z"),
        items: vec![],
    }
}

#[test]
fn une_ecoute_terminee_quitte_reprendre_l_ecoute_7415() {
    let (db, zone) = base();
    let q = PlayQueueRepo::with_backend(db.clone());
    q.append(zone, &[piste("a1"), piste("a2"), piste("b1")])
        .unwrap();
    let file = q.get_ordered(zone).unwrap();
    let ids: Vec<i64> = file.iter().map(|e| e.id).collect();
    // L'album A (lignes 0 et 1) a été interrompu ; l'album B (ligne 2) est
    // celui qui s'achève.
    save(&db, zone, &reprise(zone, "A", vec![ids[0], ids[1]])).unwrap();
    save(&db, zone, &reprise(zone, "B", vec![ids[2]])).unwrap();
    db.execute(
        "UPDATE queue_items SET is_current = 1 WHERE id = ?",
        &[&ids[2] as &dyn ToSqlValue],
    )
    .unwrap();

    assert_eq!(terminee(&db, zone).unwrap(), 1);

    let restantes: Vec<String> = recent(&db, Some(zone), 10)
        .unwrap()
        .into_iter()
        .map(|r| r.context_id)
        .collect();
    assert_eq!(restantes, vec!["A".to_string()], "B est fini, A reste");
}

#[test]
fn file_vide_ne_retire_rien_7415() {
    let (db, zone) = base();
    save(&db, zone, &reprise(zone, "A", vec![1])).unwrap();
    assert_eq!(terminee(&db, zone).unwrap(), 0);
    assert_eq!(recent(&db, Some(zone), 10).unwrap().len(), 1);
}
