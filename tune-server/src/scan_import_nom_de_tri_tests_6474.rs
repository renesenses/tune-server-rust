//! #6474 (René Burmand Johannesson, fil 2313) — « Ideally, Tune should use
//! existing metadata such as ARTISTSORT / sort-name information where
//! available ».
//!
//! La liste des artistes trie déjà sur `artists.sort_name` (#4956), mais le
//! scan ne le remplissait que depuis ALBUMARTISTSORT, et seulement pour un
//! artiste qu'il CRÉAIT. ARTISTSORT (le nom de tri de l'interprète de la
//! piste) était lu, puis perdu. Ces témoins passent par l'importeur du scan
//! (`TrackImporter::import`), sur de vrais fichiers.
use super::*;
use std::path::{Path, PathBuf};
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::sqlite::SqliteDb;
use tune_core::metadata::TrackMetadata;

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    Arc::new(db)
}

fn gabarit() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tune-core/tests/fixtures/test.flac")
}

fn fichier(dossier: &Path, nom: &str, meta: TrackMetadata) -> ScannedFile {
    std::fs::create_dir_all(dossier).unwrap();
    let chemin = dossier.join(nom);
    std::fs::copy(gabarit(), &chemin).unwrap();
    ScannedFile {
        path: chemin.to_string_lossy().into_owned(),
        metadata: Some(meta),
        unsupported: None,
        audio_hash: None,
        file_size: 4096,
        mtime: 1_700_000_000.0,
    }
}

fn importer(db: &Arc<dyn DbBackend>, cache: &Path, fichiers: &[ScannedFile]) {
    let mut imp = TrackImporter::new(db.clone(), false, cache.to_path_buf(), PorteeDuScan::TOUT);
    imp.begin_batch(fichiers);
    for f in fichiers {
        imp.import(f).expect("import");
    }
}

fn nom_de_tri(db: &Arc<dyn DbBackend>, nom: &str) -> Option<String> {
    ArtistRepo::with_backend(db.clone())
        .get_by_name(nom)
        .unwrap()
        .unwrap_or_else(|| panic!("artiste {nom} absent"))
        .sort_name
}

/// Sans ALBUMARTIST ni ALBUMARTISTSORT : l'artiste de l'album est celui de
/// la piste, et son ARTISTSORT devient le nom de tri.
#[test]
fn artistsort_donne_le_nom_de_tri_de_l_artiste_6474() {
    let tmp = tempfile::tempdir().unwrap();
    let db = base();
    let f = fichier(
        &tmp.path().join("Kind of Blue"),
        "01.flac",
        TrackMetadata {
            title: Some("So What".into()),
            artist: Some("Miles Davis".into()),
            artist_sort: Some("Davis, Miles".into()),
            album: Some("Kind of Blue".into()),
            track_number: Some(1),
            ..Default::default()
        },
    );
    importer(&db, &tmp.path().join("cache"), &[f]);
    assert_eq!(
        nom_de_tri(&db, "Miles Davis").as_deref(),
        Some("Davis, Miles")
    );
}

/// Une compilation : l'ARTISTSORT de chaque piste va à SON interprète, jamais
/// à « Various Artists ».
#[test]
fn artistsort_d_une_compilation_va_a_l_interprete_6474() {
    let tmp = tempfile::tempdir().unwrap();
    let db = base();
    let dossier = tmp.path().join("Jazz Ballads");
    let piste = |n: u32, artiste: &str, tri: &str| {
        fichier(
            &dossier,
            &format!("{n:02}.flac"),
            TrackMetadata {
                title: Some(format!("Titre {n}")),
                artist: Some(artiste.into()),
                artist_sort: Some(tri.into()),
                album: Some("Jazz Ballads".into()),
                album_artist: Some("Various Artists".into()),
                compilation: Some(true),
                track_number: Some(n),
                ..Default::default()
            },
        )
    };
    let fichiers = [
        piste(1, "Bill Evans", "Evans, Bill"),
        piste(2, "Chet Baker", "Baker, Chet"),
    ];
    importer(&db, &tmp.path().join("cache"), &fichiers);
    assert_eq!(
        nom_de_tri(&db, "Bill Evans").as_deref(),
        Some("Evans, Bill")
    );
    assert_eq!(
        nom_de_tri(&db, "Chet Baker").as_deref(),
        Some("Baker, Chet")
    );
    assert_eq!(nom_de_tri(&db, "Various Artists"), None);
}

/// Un artiste DÉJÀ en base sans nom de tri (scanné avant que ses fichiers
/// portent ARTISTSORT) le reçoit à la relecture.
#[test]
fn un_artiste_deja_en_base_recoit_son_nom_de_tri_6474() {
    let tmp = tempfile::tempdir().unwrap();
    let db = base();
    ArtistRepo::with_backend(db.clone())
        .get_or_create("Joni Mitchell", None, None)
        .unwrap();
    let f = fichier(
        &tmp.path().join("Blue"),
        "01.flac",
        TrackMetadata {
            title: Some("All I Want".into()),
            artist: Some("Joni Mitchell".into()),
            artist_sort: Some("Mitchell, Joni".into()),
            album: Some("Blue".into()),
            album_artist: Some("Joni Mitchell".into()),
            track_number: Some(1),
            ..Default::default()
        },
    );
    importer(&db, &tmp.path().join("cache"), &[f]);
    assert_eq!(
        nom_de_tri(&db, "Joni Mitchell").as_deref(),
        Some("Mitchell, Joni")
    );
}
