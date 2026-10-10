//! Fil 1881 — le surveillant de fichiers (`build_track_from_metadata`) range
//! un fichier `ALBUMARTIST = Various`, `COMPILATION=0` comme le scan par
//! lots : l'album n'est pas une compilation (C1), il prend l'artiste neutre
//! « Various Artists », et la piste GARDE son ARTIST.
//!
//! Avant, l'album naissait sous un artiste « Various » et la piste prenait
//! cet artiste-là.

use std::sync::Arc;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;
use tune_core::metadata::TrackMetadata;
use tune_core::scanner::walker::ScannedFile;

fn fichier(chemin: &str, artiste: &str, album_artiste: &str, tag: Option<bool>) -> ScannedFile {
    ScannedFile {
        path: chemin.to_string(),
        metadata: Some(TrackMetadata {
            title: Some(format!("{artiste} — titre")),
            artist: Some(artiste.to_string()),
            album: Some("Nuits Jazz".to_string()),
            album_artist: Some(album_artiste.to_string()),
            track_number: Some(1),
            compilation: tag,
            ..Default::default()
        }),
        unsupported: None,
        audio_hash: Some("hash-1881".into()),
        file_size: 4096,
        mtime: 1_700_000_000.0,
    }
}

/// `(artiste d'album, compilation, artiste de la piste)`.
fn ranger(sf: &ScannedFile) -> (String, bool, String) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let artistes = ArtistRepo::with_backend(backend.clone());
    let albums = AlbumRepo::with_backend(backend);
    let (piste, album_id) =
        crate::auto_scan::build_track_from_metadata(sf, &artistes, &albums).expect("une piste");
    let album = albums.get(album_id.expect("un album")).unwrap().unwrap();
    let nom = |id: Option<i64>| artistes.get(id.expect("un artiste")).unwrap().unwrap().name;
    (
        nom(album.artist_id),
        album.is_compilation,
        nom(piste.artist_id),
    )
}

#[test]
fn le_surveillant_garde_l_artiste_de_la_piste_sous_un_album_artist_generique_1881() {
    for graphie in ["Various", "VA", "Divers"] {
        assert_eq!(
            ranger(&fichier(
                "/m/nuits/01.flac",
                "Miles Davis",
                graphie,
                Some(false)
            )),
            ("Various Artists".into(), false, "Miles Davis".into()),
            "ALBUMARTIST = « {graphie} », COMPILATION=0"
        );
    }
    // TÉMOIN — un vrai artiste d'album : inchangé.
    assert_eq!(
        ranger(&fichier(
            "/m/reiner/01.flac",
            "Chicago Symphony Orchestra",
            "Fritz Reiner",
            Some(false)
        )),
        ("Fritz Reiner".into(), false, "Fritz Reiner".into())
    );
}
