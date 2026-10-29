//! #6459 — le vrai Scan complet répare une ancienne fiche de disque CUE,
//! puis retire la fiche devenue vide. Aucune donnée du testeur n'est utilisée.
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::flac::FlacFile;
use lofty::ogg::VorbisComments;
use std::path::Path;
use std::time::Duration;
use tune_core::db::{album_repo::AlbumRepo, settings_repo::SettingsRepo, track_repo::TrackRepo};

const ALBUM: &str = "Parution de couverture 6459";
const ARTISTE: &str = "Compositeur de couverture 6459";

fn disque(dossier: &Path, numero: usize) {
    std::fs::create_dir_all(dossier).unwrap();
    let image = dossier.join("image.flac");
    std::fs::write(
        &image,
        include_bytes!("../../tune-core/tests/fixtures/flac/ref_16_44100_stereo.flac"),
    )
    .unwrap();
    let mut fichier = std::fs::File::open(&image).unwrap();
    let mut flac = FlacFile::read_from(&mut fichier, ParseOptions::new()).unwrap();
    drop(fichier);
    let mut tags = VorbisComments::default();
    for (nom, valeur) in [
        ("ALBUM", ALBUM.to_string()),
        ("ALBUMARTIST", ARTISTE.to_string()),
        ("ARTIST", "Soliste de couverture 6459".to_string()),
        ("TITLE", format!("Extrait {numero}")),
        ("DISCNUMBER", numero.to_string()),
        ("TRACKNUMBER", "1".to_string()),
    ] {
        tags.insert(nom.to_string(), valeur);
    }
    flac.set_vorbis_comments(tags);
    flac.save_to_path(&image, WriteOptions::default()).unwrap();
    std::fs::write(
        dossier.join("album.cue"),
        format!(
            "TITLE \"{ALBUM}\"\nFILE \"image.flac\" WAVE\n  TRACK 01 AUDIO\n    TITLE \"Extrait {numero}\"\n    INDEX 01 00:00:00\n"
        ),
    )
    .unwrap();
}

async fn scan_complet(etat: crate::state::AppState) {
    let mut evenements = etat.event_bus.subscribe();
    assert!(crate::routes::system::scan::spawn_library_scan(etat.clone(), true, None).await);
    let fin = tune_core::event_types::EventType::ScanComplete.as_str();
    let rapport = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            match evenements.recv().await {
                Ok(ev) if ev.event_type == fin => break ev.data,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("bus du scan fermé : {e}"),
            }
        }
    })
    .await
    .expect("le Scan complet du banc 6459 doit terminer");
    assert_ne!(rapport["cancelled"], true, "{rapport}");
    crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// Le verrou commun sérialise les vrais scans des différents bancs du serveur.
#[allow(clippy::await_holding_lock)]
async fn le_scan_complet_retire_la_fiche_vide_d_un_ancien_disque_cue_6459() {
    let _seul = crate::routes::system::scan::serialiser_les_scans_de_test_sans_bloquer().await;
    crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
    let base = tune_core::test_scratch::scratch_dir("cue-heritage-6459-base");
    let musique = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        "cue-heritage-6459-musique",
    );
    let parution = musique.join(ALBUM);
    for numero in [1, 2] {
        disque(&parution.join(format!("CD{numero}")), numero);
    }
    let etat = crate::state::AppState::new(
        &base.join("tune.db").to_string_lossy(),
        0,
        Default::default(),
    )
    .unwrap();
    let db = etat.backend.clone();
    let reglages = SettingsRepo::with_backend(db.clone());
    reglages
        .set(
            "music_dirs",
            &serde_json::to_string(&[musique.to_string_lossy()]).unwrap(),
        )
        .unwrap();
    reglages.set("enrich_on_scan", "false").unwrap();
    reglages.set("quality_split", "false").unwrap();

    scan_complet(etat.clone()).await;
    let pistes = TrackRepo::with_backend(db.clone());
    let albums = AlbumRepo::with_backend(db.clone());
    let chemin = parution
        .join("CD1/image.flac")
        .to_string_lossy()
        .into_owned();
    let premiere = pistes
        .get_by_path(&chemin)
        .unwrap()
        .expect("premier disque indexé");
    let album = premiere.album_id.unwrap();
    assert_eq!(
        albums.get(album).unwrap().unwrap().artist_name.as_deref(),
        Some(ARTISTE)
    );

    // État hérité d'un ancien rattachement à CD1 : la fiche de parution
    // existe toujours grâce à CD2, mais la piste de CD1 est sur une autre fiche.
    // L'artiste reste référencé par la parution ; un titre distinct évite
    // qu'une fusion de doublons masque la purge de cette ancienne fiche.
    let artiste = albums.get(album).unwrap().unwrap().artist_id.unwrap();
    // Création directe : l'API actuelle normalise CD1 vers la parution,
    // alors que le banc reproduit une ligne écrite avant cette normalisation.
    let mut fiche = tune_core::db::models::Album::new("Ancienne fiche du disque 6459".into());
    fiche.artist_id = Some(artiste);
    let ancien = albums.create(&fiche).unwrap();
    albums
        .set_folder_path(ancien, &parution.join("CD1").to_string_lossy())
        .unwrap();
    assert_ne!(
        ancien, album,
        "le banc doit créer une vraie ancienne fiche distincte"
    );
    db.execute(
        "UPDATE tracks SET album_id = ?1 WHERE id = ?2",
        &[&ancien, &premiere.id.unwrap()],
    )
    .unwrap();
    let avant = db
        .query_one(
            "SELECT COUNT(*) FROM tracks WHERE album_id = ?1",
            &[&ancien],
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        avant[0].as_i64(),
        Some(1),
        "l'ancienne fiche porte réellement la piste avant le scan"
    );

    scan_complet(etat).await;
    let apres = pistes.get_by_path(&chemin).unwrap().unwrap();
    assert_eq!(
        apres.album_id,
        Some(album),
        "#6459 : la piste CUE doit revenir à la parution"
    );
    assert!(
        albums.get(ancien).unwrap().is_none(),
        "#6459 : après le Scan complet, l'ancienne fiche de disque devenue vide reste visible dans la bibliothèque"
    );
    assert_eq!(
        albums.get(album).unwrap().unwrap().artist_name.as_deref(),
        Some(ARTISTE)
    );
}
