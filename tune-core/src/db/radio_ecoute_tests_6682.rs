//! Témoins de #6682 — le temps d'écoute radio par station, à part des
//! classements. Aucun son, aucun réseau : des états de zone fabriqués, une
//! base SQLite sur disque.

use super::*;
use crate::db::history_repo::HistoryRepo;
use crate::db::sqlite::SqliteDb;
use crate::playback::NowPlaying;

fn zone(id: i64, etat: PlayState, source: &str, url: &str, nom: &str) -> ZoneState {
    ZoneState {
        zone_id: id,
        state: etat,
        now_playing: Some(NowPlaying {
            title: "Titre ICY du moment".into(),
            album_title: Some(nom.into()),
            source: source.into(),
            source_id: Some(url.into()),
            ..Default::default()
        }),
        session_profile_id: Some(2),
        ..Default::default()
    }
}

const FIP: &str = "http://icecast.radiofrance.fr/fip-hifi.aac";
const TSF: &str = "http://tsfjazz.ice.infomaniak.ch/tsfjazz-high.mp3";

#[test]
fn le_temps_s_accumule_tant_que_la_meme_station_joue_6682() {
    let mut e = Echantillonneur::default();
    let t0 = Instant::now();
    let z = [zone(1, PlayState::Playing, "radio", FIP, "FIP")];
    let premier = e.observer(&z, t0);
    assert_eq!(premier.len(), 1);
    assert!(
        premier[0].nouvelle_session,
        "la première vue ouvre une session"
    );
    assert_eq!(premier[0].ms, 0);
    assert_eq!(
        premier[0].station_name, "FIP",
        "le nom de la station, pas le titre ICY"
    );
    assert_eq!(premier[0].profile_id, 2);

    let suite = e.observer(&z, t0 + Duration::from_secs(15));
    assert_eq!(suite.len(), 1);
    assert!(!suite[0].nouvelle_session);
    assert_eq!(suite[0].ms, 15_000);
}

#[test]
fn pause_titre_local_et_ecart_trop_long_ne_comptent_pas_6682() {
    let mut e = Echantillonneur::default();
    let t0 = Instant::now();
    e.observer(&[zone(1, PlayState::Playing, "radio", FIP, "FIP")], t0);
    // En pause : rien, et la session est close.
    assert!(
        e.observer(
            &[zone(1, PlayState::Paused, "radio", FIP, "FIP")],
            t0 + Duration::from_secs(15)
        )
        .is_empty()
    );
    let reprise = e.observer(
        &[zone(1, PlayState::Playing, "radio", FIP, "FIP")],
        t0 + Duration::from_secs(30),
    );
    assert!(
        reprise[0].nouvelle_session,
        "la reprise ouvre une nouvelle session"
    );
    // Un titre de la bibliothèque n'est pas une radio.
    assert!(
        e.observer(
            &[zone(1, PlayState::Playing, "local", "/m/a.flac", "Album")],
            t0
        )
        .is_empty()
    );
    // Un écart de plus d'une minute (veille de la machine) n'est pas compté.
    let mut e = Echantillonneur::default();
    e.observer(&[zone(1, PlayState::Playing, "radio", FIP, "FIP")], t0);
    assert!(
        e.observer(
            &[zone(1, PlayState::Playing, "radio", FIP, "FIP")],
            t0 + Duration::from_secs(3600),
        )
        .is_empty(),
        "une heure de veille comptée comme une heure d'écoute"
    );
}

#[test]
fn changer_de_station_ouvre_une_session_ailleurs_6682() {
    let mut e = Echantillonneur::default();
    let t0 = Instant::now();
    e.observer(&[zone(1, PlayState::Playing, "radio", FIP, "FIP")], t0);
    let apres = e.observer(
        &[zone(1, PlayState::Playing, "radio", TSF, "TSF Jazz")],
        t0 + Duration::from_secs(15),
    );
    assert_eq!(apres.len(), 1);
    assert_eq!(apres[0].station_url, TSF);
    assert!(apres[0].nouvelle_session);
    assert_eq!(apres[0].ms, 0, "le temps de FIP n'est pas versé à TSF Jazz");
}

fn base() -> (Arc<dyn DbBackend>, crate::test_scratch::ScratchDir) {
    let dir = crate::test_scratch::scratch_dir("radio-ecoute-6682");
    let chemin = dir.path().join("tune.db");
    let db = SqliteDb::open(chemin.to_str().unwrap()).unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    (Arc::new(db), dir)
}

fn inc(zone: i64, profil: i64, url: &str, nom: &str, ms: i64, session: bool) -> Increment {
    Increment {
        zone_id: zone,
        profile_id: profil,
        station_url: url.into(),
        station_name: nom.into(),
        ms,
        nouvelle_session: session,
    }
}

/// Le tableau de bord rend le temps par station sur la période, sans rien
/// changer aux classements de titres et d'artistes.
#[test]
fn le_tableau_de_bord_rend_le_temps_par_station_a_part_6682() {
    let (backend, _dir) = base();
    backend
        .execute(
            &format!(
                "INSERT INTO radio_stations (id, name, url, logo_url) VALUES (90007, 'FIP', '{FIP}', 'https://x/fip.png')"
            ),
            &[],
        )
        .unwrap();
    let repo = RadioEcouteRepo::with_backend(backend.clone());
    let aujourd_hui = jour_de(chrono::Utc::now());
    let il_y_a_un_mois = jour_de(chrono::Utc::now() - chrono::Duration::days(40));
    repo.ajouter(&aujourd_hui, &inc(1, 2, FIP, "FIP", 0, true))
        .unwrap();
    repo.ajouter(&aujourd_hui, &inc(1, 2, FIP, "FIP", 600_000, false))
        .unwrap();
    repo.ajouter(&aujourd_hui, &inc(1, 2, TSF, "TSF Jazz", 0, true))
        .unwrap();
    repo.ajouter(&aujourd_hui, &inc(1, 2, TSF, "TSF Jazz", 120_000, false))
        .unwrap();
    repo.ajouter(
        &il_y_a_un_mois,
        &inc(1, 2, TSF, "TSF Jazz", 9_000_000, true),
    )
    .unwrap();
    // Une autre zone, sans propriétaire.
    repo.ajouter(&aujourd_hui, &inc(3, 0, FIP, "FIP", 60_000, true))
        .unwrap();

    let histo = HistoryRepo::with_backend(backend.clone());
    let d = histo.full_dashboard("7d", None, None, 10).unwrap();
    let vu: Vec<(String, i64, i64, Option<i64>)> = d
        .radio_listening
        .iter()
        .map(|e| {
            (
                e.station_name.clone(),
                e.listening_ms,
                e.sessions,
                e.radio_id,
            )
        })
        .collect();
    assert_eq!(
        vu,
        vec![
            ("FIP".into(), 660_000, 2, Some(90007)),
            ("TSF Jazz".into(), 120_000, 1, None),
        ],
        "le temps d'écoute radio par station, sur 7 jours"
    );
    assert_eq!(
        d.radio_listening[0].cover_url.as_deref(),
        Some("https://x/fip.png")
    );
    // Toute la période : l'écoute d'il y a un mois place TSF Jazz en tête.
    let tout = histo.full_dashboard("all", None, None, 10).unwrap();
    assert_eq!(tout.radio_listening[0].station_name, "TSF Jazz");
    // Par profil : l'écoute sans propriétaire n'est pas attribuée.
    let profil = histo.full_dashboard("7d", None, Some(2), 10).unwrap();
    assert_eq!(profil.radio_listening[0].listening_ms, 600_000);
    // Par zone.
    let z3 = histo.full_dashboard("7d", Some(3), None, 10).unwrap();
    assert_eq!(z3.radio_listening.len(), 1);
    // Les classements ne bougent pas : aucune radio dans les titres, artistes
    // ni totaux d'écoute.
    assert!(d.top_tracks.is_empty() && d.top_artists.is_empty());
    let json = serde_json::to_value(&d).unwrap();
    assert!(json["radio_listening"].is_array());
}

#[test]
fn la_fenetre_compte_des_jours_calendaires_6682() {
    let t = chrono::DateTime::parse_from_rfc3339("2026-10-23T08:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert_eq!(premier_jour(t, 1), "2026-10-23");
    assert_eq!(premier_jour(t, 7), "2026-10-17");
}
