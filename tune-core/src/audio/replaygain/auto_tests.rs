//! Témoins du mode ReplayGain Auto (#8968, PR 4).

use super::*;
use crate::audio::replaygain::{
    MODE_KEY, PisteRg, ReplayGainMode, ReplayGainSettings, facteur_de_zone,
};
use crate::db::play_queue_repo::QueueInput;
use crate::db::settings_repo::SettingsRepo;
use crate::db::track_metadata_repo::TrackMetadataRepo;

fn locale(track_id: i64, album_id: i64) -> LigneDeFile {
    LigneDeFile {
        track_id: Some(track_id),
        source: Some("local".into()),
        album_id: Some(album_id),
        ..Default::default()
    }
}

fn service(source: &str, id: &str, album_ref: Option<&str>) -> LigneDeFile {
    LigneDeFile {
        source: Some(source.into()),
        source_id: Some(id.into()),
        album_ref: album_ref.map(str::to_string),
        ..Default::default()
    }
}

/// La décision sur une file entière, la piste au rang `rang`.
fn sur_la_file(file: &[LigneDeFile], courant: i64, piste: PisteRg<'_>) -> ReplayGainMode {
    decider(piste, false, false, Some(courant), |r| {
        file.get(usize::try_from(r).ok()?).cloned()
    })
    .0
}

/// 🔴 Un album lu dans l'ordre : gain d'ALBUM sur CHAQUE piste, première et
/// dernière comprises — aucune frontière ne saute de niveau.
#[test]
fn un_album_lu_dans_l_ordre_garde_le_gain_d_album_de_bout_en_bout() {
    let file: Vec<_> = (1..=5).map(|t| locale(t, 7)).collect();
    for (rang, t) in (1..=5).enumerate() {
        assert_eq!(
            sur_la_file(&file, rang as i64, PisteRg::bibliotheque(t)),
            ReplayGainMode::Album,
            "la piste {t} de l'album sauterait au gain de piste"
        );
    }
}

/// La contre-épreuve : des pistes d'albums tous différents gardent le gain de
/// PISTE. Sans elle, un `Auto` qui rendrait toujours `Album` serait vert.
#[test]
fn une_liste_d_albums_differents_garde_le_gain_de_piste() {
    let file: Vec<_> = (1..=4).map(|t| locale(t, 100 + t)).collect();
    for (rang, t) in (1..=4).enumerate() {
        assert_eq!(
            sur_la_file(&file, rang as i64, PisteRg::bibliotheque(t)),
            ReplayGainMode::Track,
            "la piste {t} d'un album isolé recevrait le gain d'album"
        );
    }
}

/// Une liste mêlée : les deux pistes consécutives d'un même album ont le gain
/// d'album, la piste isolée entre deux autres albums le gain de piste.
#[test]
fn une_liste_melee_juge_chaque_piste_par_ses_voisines() {
    let file = vec![locale(1, 10), locale(2, 10), locale(3, 20), locale(4, 30)];
    assert_eq!(
        sur_la_file(&file, 0, PisteRg::bibliotheque(1)),
        ReplayGainMode::Album
    );
    assert_eq!(
        sur_la_file(&file, 1, PisteRg::bibliotheque(2)),
        ReplayGainMode::Album
    );
    assert_eq!(
        sur_la_file(&file, 2, PisteRg::bibliotheque(3)),
        ReplayGainMode::Track
    );
    assert_eq!(
        sur_la_file(&file, 3, PisteRg::bibliotheque(4)),
        ReplayGainMode::Track
    );
}

/// La piste ARMÉE pour l'enchaînement (rang suivant) est jugée à SA place
/// dans la file, pas à celle de la piste en cours.
#[test]
fn la_piste_armee_est_jugee_a_sa_place() {
    let file = vec![locale(1, 10), locale(2, 20), locale(3, 20)];
    // La piste 1 joue (rang 0), la piste 2 est armée : sa voisine 3 est du
    // même album.
    assert_eq!(
        sur_la_file(&file, 0, PisteRg::bibliotheque(2)),
        ReplayGainMode::Album
    );
    // Une piste qui n'est ni en cours ni suivante : gain de piste.
    assert_eq!(
        decider(PisteRg::bibliotheque(3), false, false, Some(0), |r| file
            .get(r as usize)
            .cloned()),
        (ReplayGainMode::Track, MotifAuto::HorsDeLaFile)
    );
}

/// 🔴 L'aléatoire : gain de piste, même entre deux voisines tirées dans le
/// même album.
#[test]
fn en_aleatoire_c_est_toujours_le_gain_de_piste() {
    let file: Vec<_> = (1..=3).map(|t| locale(t, 7)).collect();
    let (mode, motif) = decider(PisteRg::bibliotheque(2), true, false, Some(1), |r| {
        file.get(r as usize).cloned()
    });
    assert_eq!((mode, motif), (ReplayGainMode::Track, MotifAuto::Aleatoire));
}

/// Les versions d'une même œuvre (#7617) ont des `album_id` distincts : ce
/// ne sont PAS le même album. Et un titre ressemblant ne suffit jamais.
#[test]
fn deux_versions_d_un_album_ne_sont_pas_le_meme_album() {
    assert!(!meme_album(&locale(1, 41), &locale(2, 42)));
    assert!(meme_album(&locale(1, 41), &locale(2, 41)));
    let sans_album = LigneDeFile {
        track_id: Some(3),
        ..Default::default()
    };
    assert!(!meme_album(&sans_album, &sans_album.clone()));
}

/// Titres de service : même source ET même référence d'album du service.
#[test]
fn les_titres_de_service_se_comparent_par_la_reference_d_album() {
    let a = service("qobuz", "1", Some("alb-9"));
    assert!(meme_album(&a, &service("Qobuz", "2", Some("alb-9"))));
    assert!(!meme_album(&a, &service("qobuz", "2", Some("alb-8"))));
    assert!(!meme_album(&a, &service("tidal", "2", Some("alb-9"))));
    assert!(!meme_album(&a, &service("qobuz", "2", None)));
    assert!(!meme_album(
        &service("qobuz", "1", None),
        &service("qobuz", "2", None)
    ));
    // Local contre service : jamais le même album.
    assert!(!meme_album(&locale(1, 9), &a));

    let file = vec![a.clone(), service("qobuz", "2", Some("alb-9"))];
    assert_eq!(
        sur_la_file(&file, 0, PisteRg::new(None, Some("qobuz"), Some("1"))),
        ReplayGainMode::Album
    );
}

/// Le réglage `auto` se relit, et les trois anciens ne bougent pas.
#[test]
fn le_reglage_auto_se_relit_sans_changer_les_autres() {
    assert_eq!(ReplayGainMode::from_setting("auto"), ReplayGainMode::Auto);
    assert_eq!(ReplayGainMode::from_setting(" AUTO "), ReplayGainMode::Auto);
    assert_eq!(ReplayGainMode::from_setting("track"), ReplayGainMode::Track);
    assert_eq!(ReplayGainMode::from_setting("album"), ReplayGainMode::Album);
    assert_eq!(ReplayGainMode::from_setting("off"), ReplayGainMode::Off);
}

/// Un `Auto` non résolu (sans zone) vaut le gain de piste.
#[test]
fn sans_zone_auto_vaut_le_gain_de_piste() {
    let backend = base();
    assert_eq!(
        granularite(
            &backend,
            None,
            PisteRg::bibliotheque(1),
            ReplayGainMode::Auto
        ),
        ReplayGainMode::Track
    );
    // Hors Auto, le mode passe tel quel.
    assert_eq!(
        granularite(
            &backend,
            Some(1),
            PisteRg::bibliotheque(1),
            ReplayGainMode::Album
        ),
        ReplayGainMode::Album
    );
}

fn base() -> std::sync::Arc<dyn crate::db::backend::DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    std::sync::Arc::new(db)
}

/// 🔴 De bout en bout, sur une vraie base : le FACTEUR appliqué par la zone.
/// Album 1 = pistes 1, 2 (gain d'album −2 dB, gains de piste −6 et −9 dB),
/// album 2 = piste 3 (piste −4 dB, album −1 dB).
#[test]
fn le_facteur_de_zone_suit_la_file_en_mode_auto() {
    let backend = base();
    backend
        .execute_batch(
            "INSERT INTO zones (id, name, output_type) VALUES (896804, 'Auto', 'local');
             INSERT INTO artists (id, name) VALUES (1, 'A');
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'Un', 1), (2, 'Deux', 1);
             INSERT INTO tracks (id, title, artist_id, album_id, file_path, format, duration_ms)
               VALUES (1, 'a', 1, 1, '/m/1.flac', 'flac', 1000),
                      (2, 'b', 1, 1, '/m/2.flac', 'flac', 1000),
                      (3, 'c', 1, 2, '/m/3.flac', 'flac', 1000);",
        )
        .unwrap();
    let meta = TrackMetadataRepo::with_backend(backend.clone());
    for (t, piste, album) in [
        (1, "-6.00 dB", "-2.00 dB"),
        (2, "-9.00 dB", "-2.00 dB"),
        (3, "-4.00 dB", "-1.00 dB"),
    ] {
        meta.set(t, "rg_track_gain", piste).unwrap();
        meta.set(t, "rg_album_gain", album).unwrap();
        meta.set(t, "rg_track_peak", "0.5").unwrap();
        meta.set(t, "rg_album_peak", "0.5").unwrap();
    }
    SettingsRepo::with_backend(backend.clone())
        .set(MODE_KEY, "auto")
        .unwrap();
    assert_eq!(
        ReplayGainSettings::load(&backend).mode,
        ReplayGainMode::Auto
    );

    // Une zone à part : le registre de l'aléatoire est global au processus.
    let zone = 896_804;
    oublier_les_decisions(zone);
    let file = PlayQueueRepo::with_backend(backend.clone());
    file.append(
        zone,
        &[
            QueueInput::Local { track_id: 1 },
            QueueInput::Local { track_id: 2 },
            QueueInput::Local { track_id: 3 },
        ],
    )
    .unwrap();
    let db = |x: f64| 10f64.powf(x / 20.0);
    let proche = |a: f64, b: f64| (a - b).abs() < 1e-9;

    file.set_current_pos(zone, 0).unwrap();
    let f1 = facteur_de_zone(&backend, zone, PisteRg::bibliotheque(1));
    assert!(
        proche(f1, db(-2.0)),
        "piste 1 : gain d'album attendu, facteur {f1}"
    );
    // La piste 2, armée pendant que 1 joue : gain d'album (voisine 1).
    let f2 = facteur_de_zone(&backend, zone, PisteRg::bibliotheque(2));
    assert!(
        proche(f2, db(-2.0)),
        "piste 2 armée : gain d'album attendu, facteur {f2}"
    );

    file.set_current_pos(zone, 2).unwrap();
    let f3 = facteur_de_zone(&backend, zone, PisteRg::bibliotheque(3));
    assert!(
        proche(f3, db(-4.0)),
        "piste 3 isolée : gain de piste attendu, facteur {f3}"
    );

    // L'aléatoire armé : gain de piste, même au milieu de l'album.
    file.set_current_pos(zone, 0).unwrap();
    // Un NOUVEAU lancement (décisions oubliées) en aléatoire.
    oublier_les_decisions(zone);
    noter_aleatoire(zone, true);
    let f1_alea = facteur_de_zone(&backend, zone, PisteRg::bibliotheque(1));
    noter_aleatoire(zone, false);
    assert!(
        proche(f1_alea, db(-6.0)),
        "aléatoire : gain de piste attendu, facteur {f1_alea}"
    );
}

/// BASSE 5 — sans répétition de toute la file, la dernière ligne n'a pas de
/// suivante : la première ligne n'est pas « armée ».
#[test]
fn la_file_ne_reboucle_que_si_toute_la_file_se_repete() {
    let file = vec![locale(1, 7), locale(2, 7), locale(3, 9)];
    let a = |boucle| {
        decider(PisteRg::bibliotheque(1), false, boucle, Some(2), |r| {
            file.get(usize::try_from(r).ok()?).cloned()
        })
    };
    assert_eq!(a(false), (ReplayGainMode::Track, MotifAuto::HorsDeLaFile));
    assert_eq!(
        a(true),
        (ReplayGainMode::Album, MotifAuto::VoisineDuMemeAlbum)
    );
}

/// BASSE 4 — la file copiée sur une autre zone (transfert, suiveuse d'un
/// groupe) emporte l'aléatoire et la répétition.
#[test]
fn l_etat_de_zone_suit_la_file_copiee() {
    let (meneuse, suiveuse) = (896_811, 896_812);
    noter_aleatoire(meneuse, true);
    noter_boucle(meneuse, true);
    assert!(!aleatoire(suiveuse));
    copier_l_etat_de_zone(meneuse, suiveuse);
    assert!(
        aleatoire(suiveuse),
        "la suiveuse ignore l'aléatoire de la meneuse"
    );
    noter_aleatoire(meneuse, false);
    noter_aleatoire(suiveuse, false);
}

/// 🔴 MOYENNE 1+2 — la granularité est décidée au LANCEMENT et ne bouge
/// plus : A1 part en gain d'album (voisine A2) ; on insère X juste après A1
/// (« Lire ensuite », A1 n'a plus de voisine du même album), puis on touche
/// le préampli. Le gain reste celui de l'album, préampli compris, et le
/// chemin du signal dit toujours « album ».
#[test]
fn le_gain_ne_change_pas_en_cours_de_piste_quand_la_file_change() {
    let backend = base();
    backend
        .execute_batch(
            "INSERT INTO zones (id, name, output_type) VALUES (896805, 'Auto', 'local');
             INSERT INTO artists (id, name) VALUES (1, 'A');
             INSERT INTO albums (id, title, artist_id) VALUES (1, 'Un', 1), (2, 'Deux', 1);
             INSERT INTO tracks (id, title, artist_id, album_id, file_path, format, duration_ms)
               VALUES (1, 'a1', 1, 1, '/m/1.flac', 'flac', 1000),
                      (2, 'a2', 1, 1, '/m/2.flac', 'flac', 1000),
                      (9, 'x', 1, 2, '/m/9.flac', 'flac', 1000);",
        )
        .unwrap();
    let meta = TrackMetadataRepo::with_backend(backend.clone());
    for t in [1, 2, 9] {
        meta.set(t, "rg_track_gain", "-6.00 dB").unwrap();
        meta.set(t, "rg_album_gain", "-2.00 dB").unwrap();
        meta.set(t, "rg_track_peak", "0.5").unwrap();
        meta.set(t, "rg_album_peak", "0.5").unwrap();
    }
    let reglages = SettingsRepo::with_backend(backend.clone());
    reglages.set(MODE_KEY, "auto").unwrap();
    let zone = 896_805;
    oublier_les_decisions(zone);
    let file = PlayQueueRepo::with_backend(backend.clone());
    file.append(
        zone,
        &[
            QueueInput::Local { track_id: 1 },
            QueueInput::Local { track_id: 2 },
        ],
    )
    .unwrap();
    file.set_current_pos(zone, 0).unwrap();
    let db = |x: f64| 10f64.powf(x / 20.0);
    let a1 = PisteRg::bibliotheque(1);

    // Lancement : gain d'album.
    let f = facteur_de_zone(&backend, zone, a1);
    assert!(
        (f - db(-2.0)).abs() < 1e-9,
        "lancement : album attendu, {f}"
    );

    // « Lire ensuite » X : A1 n'a plus de voisine du même album.
    file.insert_at(zone, &[QueueInput::Local { track_id: 9 }], Some(1))
        .unwrap();
    assert_eq!(
        file.get_at(zone, 1).unwrap().and_then(|e| e.track_id),
        Some(9),
        "X n'est pas juste après A1 : le témoin ne mesure rien"
    );
    // Le préampli touché : le facteur est recalculé, la granularité reste.
    reglages
        .set(crate::audio::replaygain::PREAMP_KEY, "2")
        .unwrap();
    let f = facteur_de_zone(&backend, zone, a1);
    assert!(
        (f - db(0.0)).abs() < 1e-9,
        "le gain de A1 a changé en cours de piste : facteur {f} (album -2 dB + préampli 2 dB attendu)"
    );
    assert_eq!(
        crate::audio::replaygain::granularite_de_zone(&backend, zone, a1, ReplayGainMode::Auto),
        ReplayGainMode::Album,
        "le chemin du signal ne dirait plus la granularité appliquée"
    );

    // Le lancement SUIVANT d'A1 (décisions oubliées) relit la file : piste.
    oublier_les_decisions(zone);
    let f = facteur_de_zone(&backend, zone, a1);
    assert!((f - db(-4.0)).abs() < 1e-9, "relancé : piste attendu, {f}");
}
