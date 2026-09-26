//! #4896 (Didier, fil 1904) — le SCAN, manuel ou de démarrage, gardait lui
//! aussi l'ancien titre d'album après une retouche de balises.
//!
//! Comme le surveillant avant #4942, le scan reconnaît l'album à son DOSSIER
//! (`get_or_create_for_folder`) : il relisait les pistes retouchées, les
//! rattachait à la ligne album existante, et n'en reprenait jamais le titre ni
//! l'artiste. Le correctif applique la MÊME règle que le surveillant
//! (`realigner_albums_sur_les_balises`), en fin de scan et après la purge.
//!
//! Ces épreuves exécutent les VRAIS scans : `spawn_library_scan` (le bouton
//! « Scanner » et `/scan`) sur un `AppState` en mémoire, et `spawn_auto_scan`
//! (le scan de démarrage), sur de vrais FLAC posés sous une racine de musique.
use super::surveillant_retouche_tests_4896::{TITRES, albums_en_base, baliser, flac_8_canaux};
use crate::state::AppState;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime};
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::track_repo::TrackRepo;

const TITRE_FINAL: &str = "The Dark Side Of The Moon";
const ARTISTE_FINAL: &str = "Pink Floyd & James Guthrie";

/// Le coffret de Didier sur le disque, SANS balise ALBUM ni ALBUMARTIST,
/// daté d'hier. Racine sous le dossier courant : `is_tune_temp_file` écarte
/// tout ce qui vit sous le dossier temporaire du système.
fn coffret_sur_disque(epreuve: &str) -> (tune_core::test_scratch::ScratchDir, Vec<PathBuf>) {
    let racine = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        &format!("scan-4896-{epreuve}"),
    );
    let dossier = racine
        .join("Pink Floyd")
        .join("1973 - The Dark Side Of The Moon (50th Anniversary)")
        .join("Multichannel 7.1");
    std::fs::create_dir_all(&dossier).unwrap();
    let hier = SystemTime::now() - Duration::from_secs(86_400);
    let mut pistes = Vec::new();
    for (i, titre) in TITRES.iter().enumerate() {
        let piste = dossier.join(format!("0{} - {titre}.flac", i + 1));
        std::fs::write(&piste, flac_8_canaux()).unwrap();
        let n = (i + 1).to_string();
        baliser(
            &piste,
            &[
                ("TITLE", titre),
                ("ARTIST", "Pink Floyd"),
                ("TRACKNUMBER", &n),
            ],
            hier,
        );
        pistes.push(piste);
    }
    (racine, pistes)
}

/// Mp3tag pose ALBUM et ALBUMARTIST ; la date de modification change.
fn retoucher(pistes: &[PathBuf]) {
    for (i, piste) in pistes.iter().enumerate() {
        let n = (i + 1).to_string();
        baliser(
            piste,
            &[
                ("TITLE", TITRES[i]),
                ("ARTIST", "Pink Floyd"),
                ("ALBUMARTIST", ARTISTE_FINAL),
                ("ALBUM", TITRE_FINAL),
                ("TRACKNUMBER", &n),
            ],
            SystemTime::now(),
        );
    }
}

fn declarer_la_racine(db: &Arc<dyn DbBackend>, racine: &Path) {
    SettingsRepo::with_backend(db.clone())
        .set(
            "music_dirs",
            &serde_json::to_string(&[racine.to_string_lossy()]).unwrap(),
        )
        .unwrap();
}

fn etat(racine: &Path) -> AppState {
    let etat = AppState::new(":memory:", 0, Default::default()).expect("AppState en mémoire");
    declarer_la_racine(&etat.backend, racine);
    etat
}

/// Le scan MANUEL, jusqu'à sa fin annoncée. Le droit de scanner est global au
/// processus : un autre essai peut le tenir, on attend qu'il revienne.
async fn scan_manuel(etat: &AppState) {
    // Le droit de scanner et le compteur de scans sont des globales de
    // processus : voir `serialiser_les_scans_de_test` (#5034).
    let _seul = crate::routes::system::scan::serialiser_les_scans_de_test_sans_bloquer().await;
    let mut rx = etat.event_bus.subscribe();
    let debut = Instant::now();
    while !crate::routes::system::scan::spawn_library_scan(etat.clone(), false, None).await {
        assert!(
            debut.elapsed() < Duration::from_secs(120),
            "le droit de scanner n'est jamais revenu"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let fin = tune_core::event_types::EventType::ScanComplete.as_str();
    loop {
        match tokio::time::timeout(Duration::from_secs(120), rx.recv())
            .await
            .expect("le scan manuel n'a pas annoncé sa fin")
        {
            Ok(ev) if ev.event_type == fin => {
                crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
                return;
            }
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(e) => panic!("bus d'événements fermé : {e}"),
        }
    }
}

/// Le scan de DÉMARRAGE. Il se retire en silence quand un autre scan tient le
/// droit : `scan_started_at`, qu'il ne pose qu'une fois le droit acquis, dit
/// s'il a vraiment tourné.
async fn scan_de_demarrage(db: &Arc<dyn DbBackend>) {
    let _seul = crate::routes::system::scan::serialiser_les_scans_de_test_sans_bloquer().await;
    let reglages = SettingsRepo::with_backend(db.clone());
    let debut = Instant::now();
    loop {
        reglages.set("scan_started_at", "0").unwrap();
        let fini = crate::auto_scan::spawn_auto_scan(
            db.clone(),
            Arc::new(tune_core::event_bus::EventBus::new()),
        );
        while !fini.load(Ordering::Acquire) {
            assert!(
                debut.elapsed() < Duration::from_secs(120),
                "scan de démarrage sans fin"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if reglages.get("scan_started_at").unwrap().as_deref() != Some("0") {
            crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
            return;
        }
        assert!(
            debut.elapsed() < Duration::from_secs(120),
            "le droit de scanner n'est jamais revenu"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn attendu() -> Vec<(String, String)> {
    TITRES
        .iter()
        .map(|_| (TITRE_FINAL.to_string(), ARTISTE_FINAL.to_string()))
        .collect()
}

fn album_de(db: &Arc<dyn DbBackend>, piste: &Path) -> i64 {
    TrackRepo::with_backend(db.clone())
        .get_by_path(&piste.to_string_lossy())
        .unwrap()
        .expect("piste indexée")
        .album_id
        .expect("album")
}

/// LE TÉMOIN — scan manuel : retouche ALBUM/ALBUMARTIST sur un dossier déjà
/// indexé, puis « Scanner » : l'album suit, et c'est la MÊME ligne.
#[tokio::test]
async fn le_scan_manuel_renomme_l_album_d_un_dossier_retouche_4896() {
    let (racine, pistes) = coffret_sur_disque("manuel");
    let etat = etat(&racine);
    let db = etat.backend.clone();
    scan_manuel(&etat).await;
    let avant = albums_en_base(&db, &pistes);
    assert_ne!(avant, attendu(), "montage : l'album part d'un titre faux");
    let aid = album_de(&db, &pistes[0]);

    retoucher(&pistes);
    scan_manuel(&etat).await;
    assert_eq!(
        albums_en_base(&db, &pistes),
        attendu(),
        "#4896 — après la retouche et un scan manuel, l'album suit ses balises"
    );
    assert_eq!(
        album_de(&db, &pistes[0]),
        aid,
        "la même ligne album, renommée"
    );
}

/// LE TÉMOIN — scan de démarrage : Tune arrêté pendant la retouche.
#[tokio::test]
async fn le_scan_de_demarrage_renomme_l_album_d_un_dossier_retouche_4896() {
    let (racine, pistes) = coffret_sur_disque("demarrage");
    let db = super::surveillant_retouche_tests_4896::base();
    declarer_la_racine(&db, &racine);
    scan_de_demarrage(&db).await;
    assert_ne!(albums_en_base(&db, &pistes), attendu(), "montage");

    retoucher(&pistes);
    scan_de_demarrage(&db).await;
    assert_eq!(
        albums_en_base(&db, &pistes),
        attendu(),
        "#4896 — après la retouche et un redémarrage, l'album suit ses balises"
    );
}

/// Un éditeur qui RENOMME les fichiers en retouchant les balises : le scan
/// insère les nouveaux chemins et purge les anciens. La reprise attend la
/// purge ; avant elle, les lignes mortes rendraient le dossier non unanime.
#[tokio::test]
async fn le_scan_manuel_suit_une_retouche_qui_renomme_les_fichiers_4896() {
    let (racine, pistes) = coffret_sur_disque("renomme");
    let etat = etat(&racine);
    let db = etat.backend.clone();
    scan_manuel(&etat).await;
    let renommees: Vec<PathBuf> = pistes
        .iter()
        .enumerate()
        .map(|(i, p)| p.with_file_name(format!("Pink Floyd - 0{} - {}.flac", i + 1, TITRES[i])))
        .collect();
    for (a, b) in pistes.iter().zip(&renommees) {
        std::fs::rename(a, b).unwrap();
    }
    retoucher(&renommees);
    scan_manuel(&etat).await;
    assert_eq!(albums_en_base(&db, &renommees), attendu());
}

/// CONTRE-TÉMOIN — un dossier retouché à MOITIÉ ne décide rien.
#[tokio::test]
async fn le_scan_manuel_laisse_un_dossier_retouche_a_moitie_4896() {
    let (racine, pistes) = coffret_sur_disque("moitie");
    let etat = etat(&racine);
    let db = etat.backend.clone();
    scan_manuel(&etat).await;
    let avant = albums_en_base(&db, &pistes);
    // ALBUM posé sur UNE piste, même artiste d'album : le dossier ne devient pas
    // une compilation (deux ALBUMARTIST distincts l'en feraient une, #3232),
    // il est seulement retouché à moitié.
    baliser(
        &pistes[0],
        &[
            ("TITLE", TITRES[0]),
            ("ARTIST", "Pink Floyd"),
            ("ALBUMARTIST", "Pink Floyd"),
            ("ALBUM", TITRE_FINAL),
            ("TRACKNUMBER", "1"),
        ],
        SystemTime::now(),
    );
    scan_manuel(&etat).await;
    assert_eq!(
        albums_en_base(&db, &pistes[1..]),
        avant[1..].to_vec(),
        "la piste non retouchée garde son album"
    );
    assert_ne!(albums_en_base(&db, &pistes[1..]), attendu()[1..].to_vec());
}

/// CONTRE-TÉMOIN — un titre corrigé À LA MAIN dans Tune n'est pas défait.
#[tokio::test]
async fn le_scan_manuel_ne_defait_pas_un_titre_edite_a_la_main_4896() {
    let (racine, pistes) = coffret_sur_disque("main");
    let etat = etat(&racine);
    let db = etat.backend.clone();
    scan_manuel(&etat).await;
    let aid = album_de(&db, &pistes[0]);
    AlbumRepo::with_backend(db.clone())
        .force_update_title(aid, "Mon titre")
        .unwrap();
    tune_core::db::album_metadata_repo::AlbumMetadataRepo::with_backend(db.clone())
        .marquer_edition_manuelle(aid, &["title"])
        .unwrap();
    retoucher(&pistes);
    scan_manuel(&etat).await;
    let apres = albums_en_base(&db, &pistes);
    assert_eq!(apres[0].0, "Mon titre", "le titre tenu à la main reste");
    assert_eq!(
        apres[0].1, ARTISTE_FINAL,
        "l'artiste, lui, n'était pas tenu à la main : il suit"
    );
}

/// CONTRE-TÉMOIN — une compilation garde sa ligne : son titre peut être celui
/// du dossier, et ses pistes ne s'accordent pas sur un artiste.
#[tokio::test]
async fn le_scan_manuel_ne_touche_pas_une_compilation_4896() {
    let (racine, pistes) = coffret_sur_disque("compilation");
    let artistes = ["Pink Floyd", "Roger Waters"];
    let baliser_compilation = |album: &str, date: SystemTime| {
        for (i, piste) in pistes.iter().enumerate() {
            let n = (i + 1).to_string();
            baliser(
                piste,
                &[
                    ("TITLE", TITRES[i]),
                    ("ARTIST", artistes[i]),
                    ("ALBUM", album),
                    ("COMPILATION", "1"),
                    ("TRACKNUMBER", &n),
                ],
                date,
            );
        }
    };
    baliser_compilation("Hits", SystemTime::now() - Duration::from_secs(86_400));
    let etat = etat(&racine);
    let db = etat.backend.clone();
    scan_manuel(&etat).await;
    let aid = album_de(&db, &pistes[0]);
    let album = AlbumRepo::with_backend(db.clone())
        .get(aid)
        .unwrap()
        .unwrap();
    assert!(album.is_compilation, "montage : c'est une compilation");
    let avant = albums_en_base(&db, &pistes);

    // Les balises relues désavouent la ligne (autre ALBUM) : c'est la fin de
    // scan elle-même, `BalisesVuesParAlbum::realigner`, qui est éprouvée ici.
    // (Un second scan ne dirait rien de la règle : l'import range déjà une
    // compilation retouchée sous une autre ligne, hors de ce correctif.)
    baliser_compilation("Hits, volume 2", SystemTime::now());
    let mut vues = crate::auto_scan::BalisesVuesParAlbum::default();
    for piste in &pistes {
        vues.noter(
            Some(aid),
            tune_core::metadata::read_metadata(piste).as_ref(),
        );
    }
    assert_eq!(vues.realigner(&db), 0, "aucun album soumis à la reprise");
    assert_eq!(
        albums_en_base(&db, &pistes),
        avant,
        "une compilation n'est jamais reprise sur les balises"
    );
}
