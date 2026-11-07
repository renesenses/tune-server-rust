//! #6737 — relecture réelle d'une base qui contient déjà l'ancien repli.
use super::surveillant_retouche_tests_4896::flac_8_canaux;
use crate::scan_import::{PorteeDuScan, TrackImporter};
use std::time::Duration;
use tune_core::db::{
    album_repo::AlbumRepo, artist_repo::ArtistRepo, settings_repo::SettingsRepo,
    track_repo::TrackRepo,
};
use tune_core::scanner::walker::ScannedFile;

const MONTAGE: &str = "192.168.1.20_Musique";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn le_scan_complet_retire_le_montage_avec_separation_par_qualite_6737() {
    verifier_le_cycle(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn le_scan_complet_retire_le_montage_sans_separation_par_qualite_6737() {
    verifier_le_cycle(false).await;
}

#[allow(clippy::await_holding_lock)]
async fn verifier_le_cycle(quality_split: bool) {
    let _seul = crate::routes::system::scan::serialiser_les_scans_de_test_sans_bloquer().await;
    crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
    let base = tune_core::test_scratch::scratch_dir("montage-6737-base");
    let musique = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        "montage-6737-musique",
    );
    let dossier = musique.join(MONTAGE).join("Mon Album");
    std::fs::create_dir_all(&dossier).unwrap();
    let chemin = dossier.join("01 - Titre.flac");
    std::fs::write(&chemin, flac_8_canaux()).unwrap();
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
    reglages
        .set(
            "quality_split",
            if quality_split { "true" } else { "false" },
        )
        .unwrap();
    // État hérité : le lecteur avait emprunté l'artiste au nom du montage.
    // Les fichiers sur disque n'ont jamais contenu cette balise.
    let mut meta = tune_core::metadata::read_metadata(&chemin).unwrap();
    meta.artist = Some(MONTAGE.into());
    meta.album_artist = None;
    meta.artist_from_path = true;
    let ancien = ScannedFile {
        path: chemin.to_string_lossy().into_owned(),
        metadata: Some(meta),
        unsupported: None,
        audio_hash: None,
        file_size: std::fs::metadata(&chemin).unwrap().len(),
        mtime: 0.0,
    };
    let mut import = TrackImporter::new(
        db.clone(),
        quality_split,
        base.join("cache"),
        PorteeDuScan::TOUT,
    )
    .avec_pochettes_differees();
    import.begin_batch(std::slice::from_ref(&ancien));
    let (piste, _) = import.import(&ancien).unwrap();
    let pistes = TrackRepo::with_backend(db.clone());
    assert_eq!(pistes.create_batch(&[piste]).unwrap(), 1);
    let avant = pistes.get_by_path(&ancien.path).unwrap().unwrap();
    assert_eq!(
        ArtistRepo::with_backend(db.clone())
            .get(avant.artist_id.unwrap())
            .unwrap()
            .unwrap()
            .name,
        MONTAGE,
        "le témoin doit partir du faux artiste déjà enregistré"
    );
    let ancien_album = AlbumRepo::with_backend(db.clone())
        .get(avant.album_id.unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        ancien_album.artist_name.as_deref(),
        Some(MONTAGE),
        "le témoin doit aussi partir d'un album attribué au montage"
    );
    let mut rx = etat.event_bus.subscribe();
    assert!(crate::routes::system::scan::spawn_library_scan(etat.clone(), true, None).await);
    let fin = tune_core::event_types::EventType::ScanComplete.as_str();
    let rapport = tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            match rx.recv().await {
                Ok(ev) if ev.event_type == fin => break ev.data,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(e) => panic!("bus fermé : {e}"),
            }
        }
    })
    .await
    .expect("le scan complet doit annoncer sa fin");
    assert_ne!(rapport["cancelled"], true, "{rapport}");
    crate::routes::system::scan::attendre_que_le_droit_de_scanner_soit_libre().await;
    let apres = pistes
        .get_by_path(&ancien.path)
        .unwrap()
        .expect("la piste doit rester indexée");
    assert_eq!(apres.id, avant.id, "la relecture conserve la piste");
    let album = AlbumRepo::with_backend(db.clone())
        .get(apres.album_id.unwrap())
        .unwrap()
        .unwrap();
    assert_ne!(
        album.artist_name.as_deref(),
        Some(MONTAGE),
        "#6737 : après le Scan complet, l'album garde le faux artiste du montage"
    );
    assert_ne!(
        apres.artist_name.as_deref(),
        Some(MONTAGE),
        "#6737 : après le Scan complet, la piste garde le faux artiste du montage"
    );
    let restant = db
        .query_one(
            "SELECT COUNT(*) FROM artists WHERE name = ?1",
            &[&MONTAGE.to_string()],
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        restant[0].as_i64(),
        Some(0),
        "#6737 : l'ancien artiste de montage orphelin reste dans la bibliothèque"
    );
}
