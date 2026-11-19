//! #9143 — le retrait du sas après une écoute complète, et seulement elle.

use super::*;
use crate::db::play_queue_repo::QueueInput;
use crate::db::tag_repo::StreamingTagItem;
use crate::db::{migrations, sqlite::SqliteDb, zone_repo::ZoneRepo};
use crate::library::regle_de_version::cle_reglages_du_profil;
use crate::playback::{NowPlaying, PlayState};

const PROFIL: i64 = 1;
const DUREE: i64 = 200_000;

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

/// Le sas du profil, avec l'option posée à `retrait`.
fn sas(db: &Arc<dyn DbBackend>, retrait: Option<bool>) -> i64 {
    let tag = TagRepo::with_backend(db.clone())
        .create("Écouter plus tard", None)
        .unwrap();
    let mut reglages = serde_json::json!({ CLE_ETIQUETTE: tag, "home_layout": ["a"] });
    if let Some(r) = retrait {
        reglages[CLE_RETRAIT_AUTO] = Value::Bool(r);
    }
    SettingsRepo::with_backend(db.clone())
        .set(&cle_reglages_du_profil(PROFIL), &reglages.to_string())
        .unwrap();
    tag
}

fn piste_qobuz(id: &str, album: &str) -> QueueInput {
    QueueInput::Streaming {
        source: "qobuz".into(),
        source_id: id.into(),
        title: id.into(),
        artist: "Artiste".into(),
        album: Some("Album".into()),
        cover_url: None,
        duration_ms: DUREE,
        track_number: None,
        disc_number: None,
        album_ref: Some(album.into()),
        artist_ref: None,
    }
}

/// Une zone en lecture, session « album » `album` de `source`, à la ligne
/// `position` de sa file, arrivée à `pourcent` % de la piste.
fn etat(
    db: &Arc<dyn DbBackend>,
    zone: i64,
    source: &str,
    album: &str,
    position: i64,
    pourcent: i64,
) -> ZoneState {
    let ligne = PlayQueueRepo::with_backend(db.clone())
        .get_ordered(zone)
        .unwrap()
        .into_iter()
        .find(|e| e.position == position)
        .unwrap();
    ZoneState {
        zone_id: zone,
        state: PlayState::Playing,
        position_ms: DUREE * pourcent / 100,
        queue_position: position,
        session_profile_id: Some(PROFIL),
        session_context_type: Some("album".into()),
        session_context_id: Some(album.into()),
        session_context_source: Some(source.into()),
        now_playing: Some(NowPlaying {
            track_id: ligne.track_id,
            title: ligne.title.clone().unwrap_or_default(),
            source: ligne.source.clone().unwrap_or_else(|| "local".into()),
            source_id: ligne.source_id.clone(),
            duration_ms: DUREE,
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn deposer_qobuz(db: &Arc<dyn DbBackend>, tag: i64, album: &str) {
    TagRepo::with_backend(db.clone())
        .tag_streaming_item(
            tag,
            &StreamingTagItem {
                item_type: "album".into(),
                source: "qobuz".into(),
                source_id: album.into(),
                title: Some("Album".into()),
                artist: None,
                album: None,
                cover_url: None,
                created_at: None,
            },
        )
        .unwrap();
}

fn dans_le_sas_qobuz(db: &Arc<dyn DbBackend>, tag: i64, album: &str) -> bool {
    TagRepo::with_backend(db.clone())
        .streaming_items_by_tag(tag, "album")
        .unwrap()
        .iter()
        .any(|i| i.source_id == album)
}

/// File : trois pistes de l'album Qobuz « ALB », déposé dans le sas.
fn album_qobuz(retrait: Option<bool>) -> (Arc<dyn DbBackend>, i64, i64) {
    let (db, zone) = base();
    PlayQueueRepo::with_backend(db.clone())
        .append(
            zone,
            &[
                piste_qobuz("a1", "ALB"),
                piste_qobuz("a2", "ALB"),
                piste_qobuz("a3", "ALB"),
            ],
        )
        .unwrap();
    let tag = sas(&db, retrait);
    deposer_qobuz(&db, tag, "ALB");
    (db, zone, tag)
}

#[test]
fn seuil_de_piste_achevee() {
    assert!(!piste_achevee(179_999, 200_000));
    assert!(piste_achevee(180_000, 200_000));
    assert!(piste_achevee(200_000, 200_000));
    assert!(
        !piste_achevee(0, 0),
        "durée inconnue : seule la fin naturelle compte"
    );
}

#[test]
fn ecoute_complete_d_un_album_de_service_le_retire_du_sas() {
    let (db, zone, tag) = album_qobuz(Some(true));
    let mut suivis = Suivis::new();

    // Piste 1 au-delà de 90 %, piste 2 à sa fin naturelle : pas encore fini.
    let e = etat(&db, zone, "qobuz", "ALB", 0, 91);
    assert_eq!(
        noter_piste_achevee(&db, &mut suivis, &e, false).unwrap(),
        None
    );
    // Le sondeur repasse au-delà du seuil : rien ne bouge.
    assert_eq!(
        noter_piste_achevee(&db, &mut suivis, &e, false).unwrap(),
        None
    );
    let e = etat(&db, zone, "qobuz", "ALB", 1, 40);
    assert_eq!(
        noter_piste_achevee(&db, &mut suivis, &e, true).unwrap(),
        None
    );
    assert!(dans_le_sas_qobuz(&db, tag, "ALB"));

    // Dernière piste à 95 % : l'écoute est complète, l'album sort du sas.
    let e = etat(&db, zone, "qobuz", "ALB", 2, 95);
    let fin = noter_piste_achevee(&db, &mut suivis, &e, false).unwrap();
    assert_eq!(
        fin,
        Some(EcouteComplete {
            album: AlbumEcoute::Service {
                source: "qobuz".into(),
                id: "ALB".into()
            },
            retire: true
        })
    );
    assert!(
        !dans_le_sas_qobuz(&db, tag, "ALB"),
        "album écouté en entier resté dans « Écouter plus tard »"
    );
}

#[test]
fn ecoute_partielle_ne_retire_rien() {
    let (db, zone, tag) = album_qobuz(Some(true));
    let mut suivis = Suivis::new();

    // Piste 1 zappée à 50 %, pistes 2 et 3 jouées en entier.
    let e = etat(&db, zone, "qobuz", "ALB", 0, 50);
    assert_eq!(
        noter_piste_achevee(&db, &mut suivis, &e, false).unwrap(),
        None
    );
    for position in [1, 2] {
        let e = etat(&db, zone, "qobuz", "ALB", position, 100);
        assert_eq!(
            noter_piste_achevee(&db, &mut suivis, &e, true).unwrap(),
            None
        );
    }
    assert!(
        dans_le_sas_qobuz(&db, tag, "ALB"),
        "écoute partielle : l'album ne devait pas sortir du sas"
    );
}

#[test]
fn file_reecrite_en_cours_de_route_repart_de_zero() {
    let (db, zone, tag) = album_qobuz(Some(true));
    let mut suivis = Suivis::new();
    for position in [0, 1] {
        let e = etat(&db, zone, "qobuz", "ALB", position, 100);
        noter_piste_achevee(&db, &mut suivis, &e, true).unwrap();
    }
    // « Lire » de nouveau sur l'album : nouvelles lignes, nouvelle session.
    let q = PlayQueueRepo::with_backend(db.clone());
    q.clear(zone).unwrap();
    q.append(
        zone,
        &[
            piste_qobuz("a1", "ALB"),
            piste_qobuz("a2", "ALB"),
            piste_qobuz("a3", "ALB"),
        ],
    )
    .unwrap();
    let e = etat(&db, zone, "qobuz", "ALB", 2, 100);
    assert_eq!(
        noter_piste_achevee(&db, &mut suivis, &e, true).unwrap(),
        None
    );
    assert!(dans_le_sas_qobuz(&db, tag, "ALB"));
}

#[test]
fn option_absente_ou_eteinte_ne_retire_rien() {
    for retrait in [None, Some(false)] {
        let (db, zone, tag) = album_qobuz(retrait);
        let mut suivis = Suivis::new();
        let mut fin = None;
        for position in [0, 1, 2] {
            let e = etat(&db, zone, "qobuz", "ALB", position, 100);
            fin = noter_piste_achevee(&db, &mut suivis, &e, true).unwrap();
        }
        assert_eq!(fin.map(|f| f.retire), Some(false), "option {retrait:?}");
        assert!(dans_le_sas_qobuz(&db, tag, "ALB"), "option {retrait:?}");
    }
}

/// Album local 1 de deux pistes (10, 11), déposé dans le sas.
fn album_local(db: &Arc<dyn DbBackend>) -> i64 {
    db.execute_batch(
        "INSERT INTO artists (id, name) VALUES (1, 'Rob');
         INSERT INTO albums (id, title, artist_id) VALUES (1, 'Salon', 1);
         INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, file_path, format)
           VALUES (10, 'Un', 1, 1, 200000, '/m/1.flac', 'flac'),
                  (11, 'Deux', 1, 1, 200000, '/m/2.flac', 'flac');",
    )
    .unwrap();
    let tag = sas(db, Some(true));
    TagRepo::with_backend(db.clone())
        .tag_item(tag, "album", 1)
        .unwrap();
    tag
}

fn dans_le_sas_local(db: &Arc<dyn DbBackend>, tag: i64) -> bool {
    TagRepo::with_backend(db.clone())
        .items_by_tag(tag, "album")
        .unwrap()
        .contains(&1)
}

#[test]
fn ecoute_complete_d_un_album_local_le_retire_du_sas() {
    let (db, zone) = base();
    let tag = album_local(&db);
    PlayQueueRepo::with_backend(db.clone())
        .append(
            zone,
            &[
                QueueInput::Local { track_id: 10 },
                QueueInput::Local { track_id: 11 },
            ],
        )
        .unwrap();
    let mut suivis = Suivis::new();
    let e = etat(&db, zone, "local", "1", 0, 92);
    assert_eq!(
        noter_piste_achevee(&db, &mut suivis, &e, false).unwrap(),
        None
    );
    assert!(dans_le_sas_local(&db, tag));
    let e = etat(&db, zone, "local", "1", 1, 30);
    let fin = noter_piste_achevee(&db, &mut suivis, &e, true).unwrap();
    assert_eq!(fin.map(|f| f.retire), Some(true));
    assert!(
        !dans_le_sas_local(&db, tag),
        "album local écouté en entier resté dans le sas"
    );
}

#[test]
fn album_local_lance_en_cours_de_route_n_est_pas_complet() {
    let (db, zone) = base();
    let tag = album_local(&db);
    // « Lire » depuis la piste 2 : la file ne porte que la piste 11.
    PlayQueueRepo::with_backend(db.clone())
        .append(zone, &[QueueInput::Local { track_id: 11 }])
        .unwrap();
    let mut suivis = Suivis::new();
    let e = etat(&db, zone, "local", "1", 0, 100);
    assert_eq!(
        noter_piste_achevee(&db, &mut suivis, &e, true).unwrap(),
        None
    );
    assert!(dans_le_sas_local(&db, tag));
}

#[test]
fn hors_session_album_rien_n_est_compte() {
    let (db, zone, tag) = album_qobuz(Some(true));
    let mut suivis = Suivis::new();
    for position in [0, 1, 2] {
        let mut e = etat(&db, zone, "qobuz", "ALB", position, 100);
        e.session_context_type = Some("playlist".into());
        assert_eq!(
            noter_piste_achevee(&db, &mut suivis, &e, true).unwrap(),
            None
        );
    }
    assert!(dans_le_sas_qobuz(&db, tag, "ALB"));
}
