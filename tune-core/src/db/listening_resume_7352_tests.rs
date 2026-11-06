use super::*;
use crate::db::{
    contexte_de_file::{self, ContexteDeFile},
    migrations,
    sqlite::SqliteDb,
    zone_repo::ZoneRepo,
};
use crate::playback::{NowPlaying, PlayState, ZoneState};

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
fn piste(source: &str, id: &str) -> QueueInput {
    QueueInput::Streaming {
        source: source.into(),
        source_id: id.into(),
        title: id.into(),
        artist: "Artiste".into(),
        album: Some("Album".into()),
        cover_url: Some(format!("{source}.jpg")),
        duration_ms: 360000,
        track_number: None,
        disc_number: None,
        album_ref: None,
        artist_ref: None,
    }
}
fn contexte<'a>(nature: &'a str, source: &'a str, id: &'a str) -> ContexteEcoute<'a> {
    ContexteEcoute {
        nature: Some(nature),
        id: Some(id),
        rang: Some(0),
        service: Some(source),
        titre: Some("Conteneur"),
        pochette: Some("service.jpg"),
    }
}
#[test]
fn titre_seul_sans_marqueur_courant_garde_la_position_7352() {
    for source in ["qobuz", "tidal"] {
        let (db, zone) = base();
        let q = PlayQueueRepo::with_backend(db.clone());
        q.append(zone, &[piste(source, "4")]).unwrap();
        assert!(q.get_ordered(zone).unwrap().iter().all(|e| !e.is_current));
        started(
            &db,
            zone,
            &contexte("track", source, "4"),
            source,
            Some("4"),
            "4",
            None,
            None,
        )
        .unwrap();
        let mut state = ZoneState {
            zone_id: zone,
            state: PlayState::Playing,
            queue_position: 0,
            position_ms: 42000,
            now_playing: Some(NowPlaying {
                source: source.into(),
                source_id: Some("4".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        track_position(&db, &state).unwrap();
        assert_eq!(
            recent(&db, Some(zone), 10).unwrap()[0].position_ms,
            42000,
            "{source} : la position réellement confirmée doit survivre sans is_current"
        );
        state.now_playing.as_mut().unwrap().source_id = Some("ancien-flux".into());
        state.position_ms = 99000;
        track_position(&db, &state).unwrap();
        assert_eq!(
            recent(&db, Some(zone), 10).unwrap()[0].position_ms,
            42000,
            "une identité étrangère ne doit pas déplacer la reprise"
        );
    }
}
#[test]
fn tete_heritee_et_suite_explicite_gardent_le_meme_tirage_7352() {
    for source in ["qobuz", "tidal"] {
        for nature in ["album", "playlist"] {
            let (db, zone) = base();
            let q = PlayQueueRepo::with_backend(db.clone());
            q.append(zone, &[piste(source, "trois")]).unwrap();
            mark_current_shuffled(&db, zone).unwrap();
            started(
                &db,
                zone,
                &contexte(nature, source, "conteneur"),
                source,
                Some("trois"),
                "trois",
                None,
                None,
            )
            .unwrap();
            let head = contexte_de_file::dernier_identifiant(&db, zone);
            q.append(zone, &[piste(source, "un"), piste(source, "deux")])
                .unwrap();
            contexte_de_file::poser_apres(
                &db,
                zone,
                head,
                &ContexteDeFile {
                    nature: nature.into(),
                    id: Some("conteneur".into()),
                    service: Some(source.into()),
                    ..Default::default()
                },
            )
            .unwrap();
            // La première piste reste la tête de /play, la suite vient de queue/add.
            started(
                &db,
                zone,
                &contexte(nature, source, "conteneur"),
                source,
                Some("trois"),
                "trois",
                None,
                None,
            )
            .unwrap();
            q.set_current_pos(zone, 1).unwrap();
            started(
                &db,
                zone,
                &contexte(nature, source, "conteneur"),
                source,
                Some("un"),
                "un",
                None,
                None,
            )
            .unwrap();
            let r = recent(&db, Some(zone), 10).unwrap().remove(0);
            assert_eq!(
                r.position, 1,
                "{source}/{nature} : ne pas retirer la tête déjà prouvée du tirage"
            );
            assert_eq!(r.items.len(), 3);
            assert_eq!(r.queue_row_ids[0], head);
            assert!(r.shuffled);
            restore(&db, zone, &r).unwrap();
            let rows = q.get_ordered(zone).unwrap();
            assert_eq!(
                rows.iter()
                    .map(|e| e.source_id.as_deref().unwrap())
                    .collect::<Vec<_>>(),
                ["trois", "un", "deux"]
            );
            assert!(rows[1].is_current);
        }
    }
}
#[test]
fn une_playlist_ajoutee_ne_recupere_pas_le_prefixe_d_un_autre_contexte_7352() {
    let (db, zone) = base();
    let q = PlayQueueRepo::with_backend(db.clone());
    q.append(zone, &[piste("qobuz", "album-avant")]).unwrap();
    started(
        &db,
        zone,
        &contexte("album", "qobuz", "autre-album"),
        "qobuz",
        Some("album-avant"),
        "avant",
        None,
        None,
    )
    .unwrap();
    let head = contexte_de_file::dernier_identifiant(&db, zone);
    q.append(zone, &[piste("qobuz", "playlist-apres")]).unwrap();
    contexte_de_file::poser_apres(
        &db,
        zone,
        head,
        &ContexteDeFile {
            nature: "playlist".into(),
            id: Some("autre-playlist".into()),
            service: Some("qobuz".into()),
            ..Default::default()
        },
    )
    .unwrap();
    q.set_current_pos(zone, 1).unwrap();
    started(
        &db,
        zone,
        &contexte("playlist", "qobuz", "autre-playlist"),
        "qobuz",
        Some("playlist-apres"),
        "apres",
        None,
        None,
    )
    .unwrap();
    let r = recent(&db, Some(zone), 10)
        .unwrap()
        .into_iter()
        .find(|r| r.context_type == "playlist")
        .unwrap();
    assert_eq!(
        r.items.len(),
        1,
        "une reprise de playlist ne doit pas hériter du préfixe d'album"
    );
    assert_eq!(r.position, 0);
    assert_ne!(r.queue_row_ids[0], head);
}

#[test]
fn intention_de_melange_de_service_sans_marqueur_courant_7352() {
    for source in ["qobuz", "tidal"] {
        let (db, zone) = base();
        let q = PlayQueueRepo::with_backend(db.clone());
        q.append(zone, &[piste(source, "trois")]).unwrap();
        mark_current_shuffled(&db, zone).unwrap();
        assert!(
            recent(&db, Some(zone), 10).unwrap().is_empty(),
            "pas de fausse écoute avant confirmation"
        );
        started(
            &db,
            zone,
            &contexte("playlist", source, "conteneur"),
            source,
            Some("trois"),
            "trois",
            None,
            None,
        )
        .unwrap();
        assert!(
            recent(&db, Some(zone), 10).unwrap()[0].shuffled,
            "{source} : le mélange du client doit survivre sans is_current"
        );
    }
}

/// #7401 — un titre de playlist de service lancé par /play (contexte de
/// session « track »), dont le client enfile lui-même la suite sans la
/// nommer : la reprise doit garder la suite, pas un seul titre qui s'arrête.
#[test]
fn titre_lance_par_play_garde_la_suite_enfilee_7401() {
    for source in ["qobuz", "tidal"] {
        let (db, zone) = base();
        let q = PlayQueueRepo::with_backend(db.clone());
        // La tête, posée par /play : aucune ligne de contexte explicite.
        q.append(zone, &[piste(source, "un")]).unwrap();
        q.set_current_pos(zone, 0).unwrap();
        let titre = contexte("track", source, "un");
        started(&db, zone, &titre, source, Some("un"), "un", None, None).unwrap();
        // La suite, enfilée par queue/add sans contexte (plusieurs titres :
        // aucun `titre_seul`), puis l'actualisation qui suit l'ajout.
        q.append(zone, &[piste(source, "deux"), piste(source, "trois")])
            .unwrap();
        started(&db, zone, &titre, source, Some("un"), "un", None, None).unwrap();
        // Le titre suivant démarre : même geste, même reprise.
        q.set_current_pos(zone, 1).unwrap();
        started(&db, zone, &titre, source, Some("deux"), "deux", None, None).unwrap();
        let r = recent(&db, Some(zone), 10).unwrap().remove(0);
        assert_eq!(r.context_type, "track");
        assert_eq!(
            r.items.len(),
            3,
            "{source} : la reprise d'un titre joué avec sa suite ne doit pas \
             se réduire à ce titre seul (#7401 : reprise puis arrêt)"
        );
        assert_eq!(r.position, 1, "{source} : le titre en cours, pas la tête");
        let state = ZoneState {
            zone_id: zone,
            state: PlayState::Playing,
            queue_position: 1,
            position_ms: 42000,
            now_playing: Some(NowPlaying {
                source: source.into(),
                source_id: Some("deux".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        track_position(&db, &state).unwrap();
        let r = recent(&db, Some(zone), 10).unwrap().remove(0);
        assert_eq!(
            r.position_ms, 42000,
            "{source} : la position du titre en cours doit être relevée"
        );
        restore(&db, zone, &r).unwrap();
        let rows = q.get_ordered(zone).unwrap();
        assert_eq!(
            rows.iter()
                .map(|e| e.source_id.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["un", "deux", "trois"],
            "{source} : la file restaurée porte toute la suite"
        );
        assert!(rows[1].is_current);
        // Après la reprise, la file restaurée nomme ce titre : elle reste
        // entière au démarrage suivant.
        started(&db, zone, &titre, source, Some("deux"), "deux", None, None).unwrap();
        assert_eq!(recent(&db, Some(zone), 10).unwrap()[0].items.len(), 3);
    }
}

/// #7401 — la garde inverse : un titre ajouté SEUL (`titre_seul`) n'entre pas
/// dans la reprise d'un autre titre, et sa propre reprise reste d'un titre.
#[test]
fn titre_ajoute_seul_reste_hors_de_la_suite_7401() {
    let (db, zone) = base();
    let q = PlayQueueRepo::with_backend(db.clone());
    q.append(zone, &[piste("qobuz", "tete"), piste("qobuz", "suite")])
        .unwrap();
    let avant = contexte_de_file::dernier_identifiant(&db, zone);
    q.append(zone, &[piste("qobuz", "isole")]).unwrap();
    contexte_de_file::poser_apres(&db, zone, avant, &ContexteDeFile::titre_seul()).unwrap();
    q.set_current_pos(zone, 0).unwrap();
    started(
        &db,
        zone,
        &contexte("track", "qobuz", "tete"),
        "qobuz",
        Some("tete"),
        "tete",
        None,
        None,
    )
    .unwrap();
    let r = recent(&db, Some(zone), 10).unwrap().remove(0);
    assert_eq!(
        r.items.len(),
        2,
        "la suite du geste, sans le titre ajouté seul"
    );
    q.set_current_pos(zone, 2).unwrap();
    started(
        &db,
        zone,
        &ContexteEcoute {
            nature: Some("track"),
            id: None,
            rang: Some(2),
            service: None,
            titre: None,
            pochette: None,
        },
        "qobuz",
        Some("isole"),
        "isole",
        None,
        None,
    )
    .unwrap();
    let seul = recent(&db, Some(zone), 10)
        .unwrap()
        .into_iter()
        .find(|r| r.context_id == "isole")
        .expect("le titre ajouté seul a sa reprise");
    assert_eq!(seul.items.len(), 1, "un titre ajouté seul reste seul");
}
