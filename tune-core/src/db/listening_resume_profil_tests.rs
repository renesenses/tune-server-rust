//! « Reprendre l'écoute » par profil : une reprise datée d'un profil
//! n'appartient qu'à lui ; une reprise sans profil se rattache à l'historique
//! de son contexte.
use super::*;
use crate::db::{migrations, sqlite::SqliteDb, zone_repo::ZoneRepo};
use crate::orchestrator::ContexteEcoute;

fn base() -> (Arc<dyn DbBackend>, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let zone = ZoneRepo::with_backend(db.clone())
        .create("Salon", Some("browser"), None)
        .unwrap();
    (db, zone)
}

fn reprise(cle: &str, nature: &str, id: &str, profil: Option<i64>, quand: &str) -> Resume {
    Resume {
        resume_key: cle.into(),
        profile_id: profil,
        context_type: nature.into(),
        context_id: id.into(),
        source: "local".into(),
        title: format!("Titre {id}"),
        last_track_title: "t".into(),
        cover_path: None,
        album_id: None,
        position: 0,
        shuffled: false,
        queue_row_ids: vec![],
        position_ms: 0,
        updated_at: quand.into(),
        items: vec![],
    }
}

fn ecoute(db: &Arc<dyn DbBackend>, nature: &str, id: &str, profil: i64) {
    db.execute(
        "INSERT INTO listen_history (title, source, listened_at, context_type, context_id, profile_id) \
         VALUES ('t', 'local', '2026-11-01T10:00:00Z', ?, ?, ?)",
        &[&nature as &dyn ToSqlValue, &id, &profil],
    )
    .unwrap();
}

#[test]
fn chaque_profil_ne_voit_que_ses_reprises() {
    let (db, zone) = base();
    save(
        &db,
        zone,
        &reprise("k1", "playlist", "11", Some(1), "2026-11-01T10:00:00Z"),
    )
    .unwrap();
    save(
        &db,
        zone,
        &reprise("k2", "track", "22", Some(2), "2026-11-01T11:00:00Z"),
    )
    .unwrap();
    let du_1: Vec<String> = recent_du_profil(&db, None, 10, 1)
        .unwrap()
        .into_iter()
        .map(|r| r.resume_key)
        .collect();
    assert_eq!(du_1, vec!["k1".to_string()]);
    let du_2: Vec<String> = recent_du_profil(&db, None, 10, 2)
        .unwrap()
        .into_iter()
        .map(|r| r.resume_key)
        .collect();
    assert_eq!(du_2, vec!["k2".to_string()]);
    // La clé chargée garde son profil (la route de reprise s'en sert).
    let k2 = load(&db, "k2").unwrap().unwrap();
    assert!(!est_au_profil(&db, &k2, 1));
    assert!(est_au_profil(&db, &k2, 2));
}

#[test]
fn une_reprise_sans_profil_suit_l_historique_de_son_contexte() {
    let (db, zone) = base();
    save(
        &db,
        zone,
        &reprise("ancienne", "album", "A", None, "2026-11-01T10:00:00Z"),
    )
    .unwrap();
    save(
        &db,
        zone,
        &reprise("orpheline", "album", "B", None, "2026-11-01T09:00:00Z"),
    )
    .unwrap();
    ecoute(&db, "album", "A", 2);
    let a = load(&db, "ancienne").unwrap().unwrap();
    assert!(
        !est_au_profil(&db, &a, 1),
        "écoutée par le profil 2 seulement"
    );
    assert!(est_au_profil(&db, &a, 2));
    let b = load(&db, "orpheline").unwrap().unwrap();
    assert!(
        est_au_profil(&db, &b, 1),
        "personne de connu : reste visible"
    );
}

#[test]
fn la_rangee_reste_pleine_malgre_les_reprises_des_autres() {
    let (db, zone) = base();
    for i in 0..15 {
        save(
            &db,
            zone,
            &reprise(
                &format!("autre-{i}"),
                "album",
                &format!("X{i}"),
                Some(2),
                &format!("2026-11-02T10:{i:02}:00Z"),
            ),
        )
        .unwrap();
    }
    for i in 0..3 {
        save(
            &db,
            zone,
            &reprise(
                &format!("mien-{i}"),
                "album",
                &format!("M{i}"),
                Some(1),
                &format!("2026-11-01T10:{i:02}:00Z"),
            ),
        )
        .unwrap();
    }
    assert_eq!(recent_du_profil(&db, None, 3, 1).unwrap().len(), 3);
}

fn piste(id: &str) -> QueueInput {
    QueueInput::Streaming {
        source: "qobuz".into(),
        source_id: id.into(),
        title: id.into(),
        artist: "Artiste".into(),
        album: Some("Album".into()),
        cover_url: None,
        duration_ms: 360000,
        track_number: None,
        disc_number: None,
        album_ref: None,
        artist_ref: None,
        media_format: None,
        sample_rate: None,
        bit_depth: None,
    }
}

/// Une réécriture sans profil de session (rafraîchissement de la file,
/// renderer piloté de l'extérieur) garde le profil de la reprise.
#[test]
fn une_reecriture_sans_profil_garde_le_proprietaire() {
    let (db, zone) = base();
    PlayQueueRepo::with_backend(db.clone())
        .append(zone, &[piste("4")])
        .unwrap();
    let c = ContexteEcoute {
        nature: Some("track"),
        id: Some("4"),
        rang: Some(0),
        service: Some("qobuz"),
        titre: Some("Titre"),
        pochette: None,
    };
    started_du_profil(&db, zone, &c, "qobuz", Some("4"), "4", None, None, Some(7)).unwrap();
    assert_eq!(recent(&db, None, 10).unwrap()[0].profile_id, Some(7));
    started(&db, zone, &c, "qobuz", Some("4"), "4", None, None).unwrap();
    assert_eq!(
        recent(&db, None, 10).unwrap()[0].profile_id,
        Some(7),
        "le rafraîchissement a rendu la reprise à tout le foyer"
    );
    // Une session d'un autre profil la reprend.
    started_du_profil(&db, zone, &c, "qobuz", Some("4"), "4", None, None, Some(8)).unwrap();
    assert_eq!(recent(&db, None, 10).unwrap()[0].profile_id, Some(8));
}
