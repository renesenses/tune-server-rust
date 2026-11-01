use super::*;
use crate::db::{migrations, sqlite::SqliteDb, zone_repo::ZoneRepo};

const URL: &str = "https://podcast.invalid/episode.mp3";
const DUREE: i64 = 3_600_000;

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

fn episode() -> crate::playback::NowPlaying {
    crate::playback::NowPlaying {
        title: "Épisode".into(),
        album_title: Some("Podcast".into()),
        source: "podcast".into(),
        source_id: Some(URL.into()),
        duration_ms: DUREE,
        ..Default::default()
    }
}

fn reprises(db: &Arc<dyn DbBackend>, zone: i64) -> Vec<Resume> {
    recent(db, Some(zone), 80).unwrap()
}

#[test]
fn un_episode_lu_jusqu_a_la_fin_quitte_reprendre_l_ecoute_7415() {
    let (db, zone) = base();
    podcast_started(&db, zone, &episode(), 600_000).unwrap();
    assert_eq!(reprises(&db, zone).len(), 1);
    // Dans les 30 dernières secondes.
    podcast_position_avec_duree(&db, zone, URL, DUREE - 20_000, DUREE).unwrap();
    assert!(
        reprises(&db, zone).is_empty(),
        "épisode fini : reprise retirée"
    );
    // Le tic du sondeur ne la réinscrit pas tant que le lecteur est en fin.
    podcast_started(&db, zone, &episode(), DUREE - 10_000).unwrap();
    assert!(reprises(&db, zone).is_empty());
    // Au-dessus de 95 %.
    podcast_started(&db, zone, &episode(), 600_000).unwrap();
    podcast_position_avec_duree(&db, zone, URL, DUREE / 100 * 96, DUREE).unwrap();
    assert!(reprises(&db, zone).is_empty(), "95 % franchis");
}

#[test]
fn un_episode_arrete_avant_la_fin_garde_sa_position_7415() {
    let (db, zone) = base();
    podcast_started(&db, zone, &episode(), 600_000).unwrap();
    podcast_position_avec_duree(&db, zone, URL, 1_800_000, DUREE).unwrap();
    let r = reprises(&db, zone);
    assert_eq!(r.len(), 1, "à mi-épisode la reprise reste");
    assert_eq!(r[0].position_ms, 1_800_000);
    assert_eq!(rewind(r[0].position_ms, DEFAULT_REWIND), 1_790_000);
    // Une durée inconnue ne conclut rien.
    podcast_position_avec_duree(&db, zone, URL, 3_599_000, 0).unwrap();
    assert_eq!(reprises(&db, zone).len(), 1);
}

#[test]
fn un_episode_termine_puis_relu_revient_comme_une_nouvelle_reprise_7415() {
    let (db, zone) = base();
    podcast_started(&db, zone, &episode(), 600_000).unwrap();
    let ancienne = reprises(&db, zone)[0].resume_key.clone();
    podcast_position_avec_duree(&db, zone, URL, DUREE - 1_000, DUREE).unwrap();
    assert!(reprises(&db, zone).is_empty());
    // Relu depuis le début.
    podcast_started(&db, zone, &episode(), 5_000).unwrap();
    let r = reprises(&db, zone);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].position_ms, 5_000);
    assert_ne!(r[0].resume_key, ancienne, "nouvelle reprise, nouvelle clé");
}

#[test]
fn la_fin_de_file_retire_l_episode_par_son_adresse_7415() {
    let (db, zone) = base();
    podcast_started(&db, zone, &episode(), 600_000).unwrap();
    assert_eq!(
        podcast_termine(&db, zone, "https://autre.invalid/x.mp3").unwrap(),
        0
    );
    assert_eq!(reprises(&db, zone).len(), 1);
    assert_eq!(podcast_termine(&db, zone, URL).unwrap(), 1);
    assert!(reprises(&db, zone).is_empty());
}
