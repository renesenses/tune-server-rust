//! #9143 — le retrait du sas après une écoute complète, et seulement elle.

use super::*;
use crate::db::play_queue_repo::QueueInput;
use crate::db::settings_repo::SettingsRepo;
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
        media_format: None,
        sample_rate: None,
        bit_depth: None,
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
    deposer_service(db, tag, "qobuz", album);
}

fn deposer_service(db: &Arc<dyn DbBackend>, tag: i64, source: &str, album: &str) {
    TagRepo::with_backend(db.clone())
        .tag_streaming_item(
            tag,
            &StreamingTagItem {
                item_type: "album".into(),
                source: source.into(),
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

/// Joue `positions` jusqu'au bout et rend la dernière suite.
fn jouer(
    sas_r: &SasRetrait,
    db: &Arc<dyn DbBackend>,
    zone: i64,
    source: &str,
    album: &str,
    positions: &[i64],
) -> Option<Suite> {
    let mut fin = None;
    for p in positions {
        let e = etat(db, zone, source, album, *p, 100);
        fin = sas_r.noter(db, &e, true).unwrap();
    }
    fin
}

fn verification(fin: Option<Suite>) -> VerificationService {
    match fin {
        Some(Suite::VerifierService(v)) => v,
        autre => panic!("vérification de service attendue, reçu {autre:?}"),
    }
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
    let s = SasRetrait::new();

    // Piste 1 au-delà de 90 %, deux fois (le sondeur repasse), piste 2 à sa
    // fin naturelle : pas encore fini.
    let e = etat(&db, zone, "qobuz", "ALB", 0, 91);
    assert_eq!(s.noter(&db, &e, false).unwrap(), None);
    assert_eq!(s.noter(&db, &e, false).unwrap(), None);
    let e = etat(&db, zone, "qobuz", "ALB", 1, 40);
    assert_eq!(s.noter(&db, &e, true).unwrap(), None);
    assert!(dans_le_sas_qobuz(&db, tag, "ALB"));

    // Dernière piste à 95 % : reste le total du service, 3 pistes.
    let e = etat(&db, zone, "qobuz", "ALB", 2, 95);
    let v = verification(s.noter(&db, &e, false).unwrap());
    assert_eq!(v.pistes_achevees, 3);
    let fin = s.conclure_service(&db, &v, Some(3)).unwrap();
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
    let s = SasRetrait::new();

    // Piste 1 zappée à 50 %, pistes 2 et 3 jouées en entier.
    let e = etat(&db, zone, "qobuz", "ALB", 0, 50);
    assert_eq!(s.noter(&db, &e, false).unwrap(), None);
    assert_eq!(jouer(&s, &db, zone, "qobuz", "ALB", &[1, 2]), None);
    assert!(
        dans_le_sas_qobuz(&db, tag, "ALB"),
        "écoute partielle : l'album ne devait pas sortir du sas"
    );
}

#[test]
fn un_disque_seul_d_un_album_de_service_ne_le_retire_pas() {
    // « Lire le disque 2 » : la file ne porte que les 3 pistes du disque,
    // l'album en compte 6 chez Qobuz.
    let (db, zone, tag) = album_qobuz(Some(true));
    let s = SasRetrait::new();
    let v = verification(jouer(&s, &db, zone, "qobuz", "ALB", &[0, 1, 2]));
    assert_eq!(s.conclure_service(&db, &v, Some(6)).unwrap(), None);
    assert!(
        dans_le_sas_qobuz(&db, tag, "ALB"),
        "un disque sur deux a vidé le sas"
    );
    // Total inconnu (service muet) : rien non plus.
    assert_eq!(s.conclure_service(&db, &v, None).unwrap(), None);
    assert!(dans_le_sas_qobuz(&db, tag, "ALB"));
}

#[test]
fn album_d_une_seule_ligne_traite_une_seule_fois() {
    let (db, zone) = base();
    PlayQueueRepo::with_backend(db.clone())
        .append(zone, &[piste_qobuz("s1", "SINGLE")])
        .unwrap();
    let tag = sas(&db, Some(true));
    deposer_qobuz(&db, tag, "SINGLE");
    let s = SasRetrait::new();

    let e = etat(&db, zone, "qobuz", "SINGLE", 0, 92);
    let v = verification(s.noter(&db, &e, false).unwrap());
    assert!(s.conclure_service(&db, &v, Some(1)).unwrap().is_some());
    assert!(!dans_le_sas_qobuz(&db, tag, "SINGLE"));

    // Les tours suivants (93 %, 99 %, fin naturelle, répétition) : plus
    // rien, et plus aucune lecture en base.
    let avant = s.lectures();
    for (pourcent, naturel) in [(93, false), (99, false), (100, true)] {
        let e = etat(&db, zone, "qobuz", "SINGLE", 0, pourcent);
        assert_eq!(s.noter(&db, &e, naturel).unwrap(), None);
    }
    assert_eq!(
        s.lectures(),
        avant,
        "la piste unique est réexaminée à chaque tour"
    );
    // Une seconde vérification arrivée en retard ne retraite pas.
    assert_eq!(s.conclure_service(&db, &v, Some(1)).unwrap(), None);
}

#[test]
fn file_reecrite_en_cours_de_route_repart_de_zero() {
    let (db, zone, tag) = album_qobuz(Some(true));
    let s = SasRetrait::new();
    jouer(&s, &db, zone, "qobuz", "ALB", &[0, 1]);
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
    assert_eq!(jouer(&s, &db, zone, "qobuz", "ALB", &[2]), None);
    assert!(dans_le_sas_qobuz(&db, tag, "ALB"));
}

#[test]
fn option_absente_ou_eteinte_ne_retire_rien_et_ne_lit_pas_la_file() {
    for retrait in [None, Some(false)] {
        let (db, zone, tag) = album_qobuz(retrait);
        let s = SasRetrait::new();
        for p in [0, 1, 2] {
            for pourcent in [90, 95, 99] {
                let e = etat(&db, zone, "qobuz", "ALB", p, pourcent);
                assert_eq!(s.noter(&db, &e, false).unwrap(), None, "option {retrait:?}");
            }
            let e = etat(&db, zone, "qobuz", "ALB", p, 100);
            assert_eq!(s.noter(&db, &e, true).unwrap(), None, "option {retrait:?}");
        }
        assert_eq!(
            s.lectures(),
            1,
            "option {retrait:?} : une seule lecture des réglages, aucune de la file"
        );
        assert!(dans_le_sas_qobuz(&db, tag, "ALB"), "option {retrait:?}");
    }
}

#[test]
fn le_changement_de_reglage_est_pris_en_compte() {
    let (db, zone, tag) = album_qobuz(Some(false));
    let s = SasRetrait::new();
    assert_eq!(jouer(&s, &db, zone, "qobuz", "ALB", &[0, 1, 2]), None);
    // L'option est allumée par l'écran.
    let reglages = serde_json::json!({ CLE_ETIQUETTE: tag, CLE_RETRAIT_AUTO: true });
    SettingsRepo::with_backend(db.clone())
        .set(&cle_reglages_du_profil(PROFIL), &reglages.to_string())
        .unwrap();
    s.reglages_modifies();
    // « Lire » de nouveau : cette fois l'album compte.
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
    let v = verification(jouer(&s, &db, zone, "qobuz", "ALB", &[0, 1, 2]));
    assert!(s.conclure_service(&db, &v, Some(3)).unwrap().is_some());
    assert!(!dans_le_sas_qobuz(&db, tag, "ALB"));
}

#[test]
fn sans_profil_de_session_rien_n_est_touche() {
    let (db, zone, tag) = album_qobuz(Some(true));
    let s = SasRetrait::new();
    for p in [0, 1, 2] {
        let mut e = etat(&db, zone, "qobuz", "ALB", p, 100);
        e.session_profile_id = None;
        assert_eq!(s.noter(&db, &e, true).unwrap(), None);
    }
    assert_eq!(s.lectures(), 0);
    assert!(dans_le_sas_qobuz(&db, tag, "ALB"));
}

#[test]
fn une_autre_etiquette_du_meme_album_reste_intacte() {
    let (db, zone, tag) = album_qobuz(Some(true));
    let autre = TagRepo::with_backend(db.clone())
        .create("Favoris du dimanche", None)
        .unwrap();
    deposer_qobuz(&db, autre, "ALB");
    let s = SasRetrait::new();
    let v = verification(jouer(&s, &db, zone, "qobuz", "ALB", &[0, 1, 2]));
    assert!(s.conclure_service(&db, &v, Some(3)).unwrap().is_some());
    assert!(!dans_le_sas_qobuz(&db, tag, "ALB"));
    assert!(
        dans_le_sas_qobuz(&db, autre, "ALB"),
        "le retrait a touché une autre étiquette"
    );
}

#[test]
fn la_source_rangee_avec_une_autre_casse_est_bien_retiree() {
    let (db, zone) = base();
    PlayQueueRepo::with_backend(db.clone())
        .append(zone, &[piste_qobuz("a1", "ALB")])
        .unwrap();
    let tag = sas(&db, Some(true));
    deposer_service(&db, tag, "Qobuz", "ALB");
    let s = SasRetrait::new();
    let v = verification(jouer(&s, &db, zone, "qobuz", "ALB", &[0]));
    assert_eq!(
        s.conclure_service(&db, &v, Some(1))
            .unwrap()
            .map(|f| f.retire),
        Some(true)
    );
    assert!(
        !dans_le_sas_qobuz(&db, tag, "ALB"),
        "« Qobuz » resté dans le sas"
    );
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
    let s = SasRetrait::new();
    let e = etat(&db, zone, "local", "1", 0, 92);
    assert_eq!(s.noter(&db, &e, false).unwrap(), None);
    assert!(dans_le_sas_local(&db, tag));
    let e = etat(&db, zone, "local", "1", 1, 30);
    let fin = s.noter(&db, &e, true).unwrap();
    assert!(matches!(
        fin,
        Some(Suite::Complete(EcouteComplete { retire: true, .. }))
    ));
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
    let s = SasRetrait::new();
    assert_eq!(jouer(&s, &db, zone, "local", "1", &[0]), None);
    assert!(dans_le_sas_local(&db, tag));
}

#[test]
fn hors_session_album_rien_n_est_compte() {
    let (db, zone, tag) = album_qobuz(Some(true));
    let s = SasRetrait::new();
    for position in [0, 1, 2] {
        let mut e = etat(&db, zone, "qobuz", "ALB", position, 100);
        e.session_context_type = Some("playlist".into());
        assert_eq!(s.noter(&db, &e, true).unwrap(), None);
    }
    assert_eq!(s.lectures(), 0);
    assert!(dans_le_sas_qobuz(&db, tag, "ALB"));
}
