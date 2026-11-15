//! renesenses/tune-web-client#2759 — `GET /profiles/{id}/favorites/streaming`
//! rend, pour chaque piste, l'identifiant de son album chez le service sous
//! `album_id_service`, le nom que lisent déjà la file et l'historique.
//!
//! Le témoin qui garde : un favori Bandcamp, dont l'identifiant est une URL de
//! flux signée, reçoit l'adresse de sa page album rangée par la lecture (fil
//! 2121), et la liste la rend — sous une AUTRE signature que celle de la file.

use super::*;
use crate::db::migrations::run_migrations;
use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
use crate::db::zone_repo::ZoneRepo;
use crate::favorites_sort::Sens;

const CHEMIN: &str = "https://t4.bcbits.com/stream/e43be2a9/mp3-128/29192493";
const PAGE: &str = "https://artiste.bandcamp.com/album/disque";

fn signee(ts: u64) -> String {
    format!("{CHEMIN}?p=0&ts={ts}&t=abcd&token={ts}_x")
}

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    Arc::new(db)
}

/// La piste Bandcamp passée en file avec sa page album, comme le fait la
/// lecture.
fn en_file(db: &Arc<dyn DbBackend>, source_id: &str) {
    let zone = ZoneRepo::with_backend(db.clone())
        .create("Salon", Some("chromecast"), Some("cast-1"))
        .unwrap();
    PlayQueueRepo::with_backend(db.clone())
        .append(
            zone,
            &[QueueInput::Streaming {
                source: "bandcamp".into(),
                source_id: source_id.into(),
                title: "Meet Her At The Love Parade".into(),
                artist: "Framewerk".into(),
                album: None,
                cover_url: None,
                duration_ms: 300_000,
                track_number: None,
                disc_number: None,
                album_ref: Some(PAGE.into()),
                artist_ref: None,
            }],
        )
        .unwrap();
}

#[test]
fn un_favori_bandcamp_rend_sa_page_album_2759() {
    let db = base();
    en_file(&db, &signee(1_790_782_809));
    let repo = StreamingFavoritesRepo::with_backend(db.clone());
    repo.add(
        1,
        "track",
        "bandcamp",
        &signee(1_791_020_000),
        Some("Meet Her At The Love Parade"),
        Some("Framewerk"),
        None,
        None,
    )
    .unwrap();

    let favoris = repo.list(1, Some("track")).unwrap();
    assert_eq!(favoris.len(), 1);
    assert_eq!(favoris[0].album_id_service.as_deref(), Some(PAGE));

    // Le JSON de la route : la clé est là, avec la page.
    let json = serde_json::to_value(&favoris).unwrap();
    assert_eq!(json[0]["album_id_service"], PAGE, "{json}");
}

#[test]
fn le_champ_est_null_sans_album_connu_et_hors_piste_2759() {
    let db = base();
    let repo = StreamingFavoritesRepo::with_backend(db.clone());
    repo.add(
        1,
        "track",
        "youtube",
        "dQw4w9WgXcQ",
        Some("t"),
        None,
        None,
        None,
    )
    .unwrap();
    repo.add(
        1,
        "album",
        "qobuz",
        "0060253780941",
        Some("a"),
        None,
        None,
        None,
    )
    .unwrap();
    // Un identifiant d'album posé sur un favori ALBUM n'est pas rendu.
    db.execute(
        "UPDATE streaming_favorites SET album_ref = 'x' WHERE item_type = 'album'",
        &[],
    )
    .unwrap();
    let json = serde_json::to_value(repo.list(1, None).unwrap()).unwrap();
    for f in json.as_array().unwrap() {
        let o = f.as_object().unwrap();
        assert!(o.contains_key("album_id_service"), "{f}");
        assert!(o["album_id_service"].is_null(), "{f}");
    }
}

#[test]
fn qobuz_l_album_du_service_est_pose_une_fois_et_lu_au_tri_manuel_2759() {
    let db = base();
    let repo = StreamingFavoritesRepo::with_backend(db.clone());
    repo.add(1, "track", "qobuz", "71", Some("A"), None, None, None)
        .unwrap();
    repo.add(1, "track", "qobuz", "72", Some("B"), None, None, None)
        .unwrap();
    repo.completer_album_du_service(1, "track", "qobuz", "71", "tj9je5zd70wsc")
        .unwrap();
    // Seulement là où rien n'est posé ; vide = rien.
    repo.completer_album_du_service(1, "track", "qobuz", "71", "autre")
        .unwrap();
    repo.completer_album_du_service(1, "track", "qobuz", "72", "  ")
        .unwrap();
    repo.reorder(
        1,
        "track",
        &[("qobuz".into(), "72".into()), ("qobuz".into(), "71".into())],
    )
    .unwrap();

    // Le tri manuel relit les lignes avec `position` APRÈS `album_ref` : le
    // rang doit rester juste et l'album à sa place.
    let tri = TriFavoris {
        cle: CleDeTri::Manuel,
        sens: Sens::Croissant,
    };
    let favoris = repo.list_sorted(1, Some("track"), tri).unwrap();
    let vus: Vec<(&str, Option<&str>)> = favoris
        .iter()
        .map(|f| (f.service_id.as_str(), f.album_id_service.as_deref()))
        .collect();
    assert_eq!(vus, vec![("72", None), ("71", Some("tj9je5zd70wsc"))]);
}
