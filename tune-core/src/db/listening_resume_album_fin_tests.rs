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
        duration_ms: 200_000,
        track_number: None,
        disc_number: None,
        album_ref: None,
        artist_ref: None,
    }
}

fn np(id: &str) -> crate::playback::NowPlaying {
    crate::playback::NowPlaying {
        title: id.into(),
        source: "qobuz".into(),
        source_id: Some(id.into()),
        duration_ms: 200_000,
        ..Default::default()
    }
}

fn courante(db: &Arc<dyn DbBackend>, ligne: i64) {
    db.execute(
        "UPDATE queue_items SET is_current = 1 WHERE id = ?",
        &[&ligne as &dyn ToSqlValue],
    )
    .unwrap();
}

/// File : a1 a2 (album A) puis x1 x2 (suite). Reprise A avec ou sans lignes.
fn prepare(avec_lignes: bool) -> (Arc<dyn DbBackend>, i64, Vec<i64>) {
    let (db, zone) = base();
    let q = PlayQueueRepo::with_backend(db.clone());
    q.append(zone, &[piste("a1"), piste("a2"), piste("x1"), piste("x2")])
        .unwrap();
    let ids: Vec<i64> = q.get_ordered(zone).unwrap().iter().map(|e| e.id).collect();
    let r = Resume {
        resume_key: "cle-A".into(),
        context_type: "album".into(),
        context_id: "A".into(),
        source: "qobuz".into(),
        title: "A".into(),
        last_track_title: "a1".into(),
        cover_path: None,
        album_id: None,
        position: 0,
        shuffled: false,
        queue_row_ids: if avec_lignes {
            vec![ids[0], ids[1]]
        } else {
            vec![]
        },
        position_ms: 0,
        updated_at: "2026-10-31T10:00:00Z".into(),
        items: vec![piste("a1"), piste("a2")],
    };
    save(&db, zone, &r).unwrap();
    (db, zone, ids)
}

fn nb(db: &Arc<dyn DbBackend>, zone: i64) -> usize {
    recent(db, Some(zone), 10).unwrap().len()
}

#[test]
fn cas1_la_file_continue_apres_l_album_la_reprise_part_7415() {
    let (db, zone, ids) = prepare(true);
    courante(&db, ids[1]); // a2, la file continue avec x1 x2
    assert_eq!(
        contexte_fini(&db, zone, &np("a2"), 199_000, true).unwrap(),
        1
    );
    assert_eq!(nb(&db, zone), 0);
}

#[test]
fn cas2_repetition_active_fin_par_les_95_pour_cent_7415() {
    let (db, zone, ids) = prepare(true);
    courante(&db, ids[1]);
    // Pas de fin naturelle (répétition) : le tic du sondeur à 96 % suffit.
    assert_eq!(
        contexte_fini(&db, zone, &np("a2"), 192_000, false).unwrap(),
        1
    );
    assert_eq!(nb(&db, zone), 0);
}

#[test]
fn cas2b_dans_les_30_dernieres_secondes_7415() {
    let (db, zone, ids) = prepare(true);
    courante(&db, ids[1]);
    assert_eq!(
        contexte_fini(&db, zone, &np("a2"), 175_000, false).unwrap(),
        1
    );
}

#[test]
fn cas4_ligne_finale_absente_des_lignes_memorisees_7415() {
    let (db, zone, ids) = prepare(false);
    courante(&db, ids[1]);
    assert_eq!(
        contexte_fini(&db, zone, &np("a2"), 199_000, true).unwrap(),
        1
    );
    assert_eq!(nb(&db, zone), 0);
}

#[test]
fn un_album_interrompu_avant_sa_derniere_piste_reste_7415() {
    let (db, zone, ids) = prepare(true);
    courante(&db, ids[0]); // a1 se termine, a2 reste à jouer
    assert_eq!(
        contexte_fini(&db, zone, &np("a1"), 199_000, true).unwrap(),
        0
    );
    assert_eq!(
        contexte_fini(&db, zone, &np("a1"), 199_000, false).unwrap(),
        0
    );
    // a2 à mi-piste : rien non plus.
    courante(&db, ids[1]);
    assert_eq!(
        contexte_fini(&db, zone, &np("a2"), 100_000, false).unwrap(),
        0
    );
    assert_eq!(nb(&db, zone), 1);
}

#[test]
fn un_album_relu_depuis_le_debut_revient_7415() {
    let (db, zone, ids) = prepare(true);
    courante(&db, ids[1]);
    contexte_fini(&db, zone, &np("a2"), 199_000, true).unwrap();
    assert_eq!(nb(&db, zone), 0);
    // Relu depuis le début : l'annonce de démarrage réécrit une reprise neuve.
    let mut neuve = recent(&db, Some(zone), 10).unwrap();
    assert!(neuve.is_empty());
    let (db2, zone2, _) = prepare(true);
    neuve = recent(&db2, Some(zone2), 10).unwrap();
    assert_eq!(neuve.len(), 1);
}

#[test]
fn cas4b_reprise_sans_pistes_memorisees_mais_avec_ses_lignes_7415() {
    let (db, zone, ids) = prepare(true);
    let r = recent(&db, Some(zone), 10).unwrap();
    assert_eq!(r.len(), 1);
    courante(&db, ids[1]);
    // Sans identité de piste exploitable, la ligne finale suffit.
    let mut sans = r[0].clone();
    sans.items.clear();
    save(&db, zone, &sans).unwrap();
    assert_eq!(
        contexte_fini(&db, zone, &np("zz"), 199_000, true).unwrap(),
        1
    );
}
