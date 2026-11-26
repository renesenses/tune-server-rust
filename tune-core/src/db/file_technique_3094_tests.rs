use super::*;
use crate::db::backend::DbBackend;
use crate::db::{migrations, sqlite::SqliteDb, zone_repo::ZoneRepo};
use std::sync::Arc;

fn base() -> (PlayQueueRepo, i64) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let zone = ZoneRepo::with_backend(db.clone())
        .create("Salon", Some("local"), Some("local:SmartDX1"))
        .unwrap();
    (PlayQueueRepo::with_backend(db), zone)
}

fn piste(source: &str, id: &str) -> QueueInput {
    QueueInput::Streaming {
        media_format: None,
        sample_rate: None,
        bit_depth: None,
        source: source.into(),
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

fn ids(repo: &PlayQueueRepo, zone: i64) -> Vec<String> {
    repo.get_ordered(zone)
        .unwrap()
        .into_iter()
        .map(|e| e.source_id.unwrap_or_default())
        .collect()
}

/// Ce que fait la mesure : la file est remplacée par le balayage.
fn poser_le_balayage(repo: &PlayQueueRepo, zone: i64, mesure: &str) {
    let ligne = (
        mesure.to_string(),
        format!("Calibration — balayage de mesure {mesure}"),
        String::new(),
        None,
        None,
        20_000,
        Some("calibration".to_string()),
        None,
        None,
    );
    repo.set_streaming_queue(zone, &[ligne]).unwrap();
}

#[test]
fn la_file_d_avant_est_rendue_et_le_balayage_retire_3094() {
    let (repo, zone) = base();
    repo.append(
        zone,
        &[
            piste("qobuz", "a"),
            piste("qobuz", "b"),
            piste("qobuz", "c"),
        ],
    )
    .unwrap();
    repo.set_current_pos(zone, 1).unwrap();

    let avant = instantane(&repo, zone).unwrap();
    assert_eq!(avant.courant, Some(1));
    poser_le_balayage(&repo, zone, "m3-1");
    assert_eq!(ids(&repo, zone), ["m3-1"]);

    let r = retirer_les_lignes_techniques(&repo, zone, |_| true).unwrap();
    assert_eq!(r.retirees, [0]);
    assert_eq!(r.longueur, 0);
    assert_eq!(restaurer(&repo, zone, &avant).unwrap(), Some((1, 3)));
    assert_eq!(ids(&repo, zone), ["a", "b", "c"]);
    let courant: Vec<bool> = repo
        .get_ordered(zone)
        .unwrap()
        .iter()
        .map(|e| e.is_current)
        .collect();
    assert_eq!(
        courant,
        [false, true, false],
        "le curseur revient sur « b »"
    );
}

#[test]
fn une_file_posee_pendant_la_mesure_n_est_pas_ecrasee_3094() {
    let (repo, zone) = base();
    repo.append(zone, &[piste("qobuz", "a")]).unwrap();
    let avant = instantane(&repo, zone).unwrap();
    poser_le_balayage(&repo, zone, "m3-1");
    // L'auditeur enfile un titre pendant la mesure.
    repo.append(zone, &[piste("tidal", "z")]).unwrap();

    retirer_les_lignes_techniques(&repo, zone, |_| true).unwrap();
    assert_eq!(restaurer(&repo, zone, &avant).unwrap(), None);
    assert_eq!(ids(&repo, zone), ["z"]);
}

#[test]
fn seules_les_lignes_techniques_partent_et_le_curseur_suit_3094() {
    let (repo, zone) = base();
    repo.append(
        zone,
        &[
            piste("qobuz", "a"),
            piste("calibration", "m1-1"),
            piste("qobuz", "b"),
            piste("calibration", "m2-1"),
            piste("qobuz", "c"),
        ],
    )
    .unwrap();
    // Le curseur sur « b » : il reste sur « b ».
    repo.set_current_pos(zone, 2).unwrap();
    // Un prédicat qui dirait oui à tout ne retire pourtant que les balayages.
    let r = retirer_les_lignes_techniques(&repo, zone, |_| true).unwrap();
    assert_eq!(r.retirees, [1, 3]);
    assert_eq!(r.courant, Some(1));
    assert_eq!(r.longueur, 3);
    assert_eq!(ids(&repo, zone), ["a", "b", "c"]);
    assert_eq!(
        r.position_apres(1),
        1,
        "la ligne ôtée cède sa place à « b »"
    );
    assert_eq!(r.position_apres(4), 2);
}

#[test]
fn le_curseur_sur_un_balayage_passe_a_la_ligne_suivante_3094() {
    let (repo, zone) = base();
    repo.append(
        zone,
        &[
            piste("qobuz", "a"),
            piste("calibration", "m1-1"),
            piste("qobuz", "b"),
        ],
    )
    .unwrap();
    repo.set_current_pos(zone, 1).unwrap();
    let r = retirer_les_lignes_techniques(&repo, zone, |e| e.source_id.as_deref() == Some("m1-1"))
        .unwrap();
    assert_eq!(r.courant, Some(1));
    let courante = repo
        .get_ordered(zone)
        .unwrap()
        .into_iter()
        .find(|e| e.is_current)
        .and_then(|e| e.source_id);
    assert_eq!(courante.as_deref(), Some("b"));
}

#[test]
fn sans_ligne_technique_rien_ne_bouge_3094() {
    let (repo, zone) = base();
    repo.append(zone, &[piste("qobuz", "a"), piste("qobuz", "b")])
        .unwrap();
    repo.set_current_pos(zone, 1).unwrap();
    let r = retirer_les_lignes_techniques(&repo, zone, |_| true).unwrap();
    assert!(r.retirees.is_empty());
    assert_eq!((r.courant, r.longueur), (Some(1), 2));
    assert_eq!(ids(&repo, zone), ["a", "b"]);
}
