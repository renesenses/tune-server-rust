//! #7933 — après une retouche des balises par un éditeur externe, le
//! surveillant de fichiers (`reimporter_fichier_surveillant`) relit ARTIST
//! comme le scan complet (`TrackImporter::import`, #3179) : la piste garde son
//! artiste, l'album garde l'ALBUMARTIST, et le MBID de l'invité ne devient pas
//! celui de l'artiste d'album.
//!
//! Avant, le surveillant donnait à la piste l'artiste de l'album dès que
//! l'album n'était pas une compilation ; seule une analyse complète remettait
//! la bonne valeur.
use super::reimporter_fichier_surveillant;
use super::surveillant_retouche_tests_4896::{baliser, coffret_indexe, flac_8_canaux};
use std::sync::Arc;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::DbBackend;
use tune_core::db::track_repo::TrackRepo;
use tune_core::scanner::watcher::{ChangeType, FileChange};

const MBID_INVITE: &str = "8a1e6b0c-7933-4b3a-9d1e-000000007933";

/// `(artiste de la piste, artiste de l'album, MBID de l'artiste de l'album)`.
fn artistes_en_base(db: &Arc<dyn DbBackend>, chemin: &str) -> (String, String, Option<String>) {
    let track = TrackRepo::with_backend(db.clone())
        .get_by_path(chemin)
        .unwrap()
        .expect("piste en base");
    let artistes = ArtistRepo::with_backend(db.clone());
    let album = AlbumRepo::with_backend(db.clone())
        .get(track.album_id.expect("un album"))
        .unwrap()
        .unwrap();
    let artiste = |id: Option<i64>| artistes.get(id.expect("un artiste")).unwrap().unwrap();
    let de_l_album = artiste(album.artist_id);
    (
        artiste(track.artist_id).name,
        de_l_album.name,
        de_l_album.musicbrainz_id,
    )
}

#[test]
fn le_surveillant_relit_l_artiste_de_la_piste_apres_retouche_7933() {
    let dossier = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        "artiste-de-piste-7933",
    );
    let db =
        tune_core::db::sqlite::SqliteDb::open(&dossier.join("tune.db").to_string_lossy()).unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let (_racine, pistes) = coffret_indexe(&db, "7933");
    let maintenant = std::time::SystemTime::now();

    // Un éditeur de balises pose un invité sur la première piste
    // (ARTIST ≠ ALBUMARTIST, MBID de l'invité seul) et ALBUMARTIST = ARTIST
    // sur la seconde.
    baliser(
        &pistes[0],
        &[
            ("TITLE", "Speak To Me"),
            ("ARTIST", "David Gilmour"),
            ("ALBUMARTIST", "Pink Floyd"),
            ("ALBUM", "The Dark Side Of The Moon"),
            ("TRACKNUMBER", "1"),
            ("MUSICBRAINZ_ARTISTID", MBID_INVITE),
        ],
        maintenant,
    );
    baliser(
        &pistes[1],
        &[
            ("TITLE", "Breathe"),
            ("ARTIST", "Pink Floyd"),
            ("ALBUMARTIST", "Pink Floyd"),
            ("ALBUM", "The Dark Side Of The Moon"),
            ("TRACKNUMBER", "2"),
        ],
        maintenant,
    );
    for piste in &pistes {
        reimporter_fichier_surveillant(
            &db,
            &FileChange {
                change_type: ChangeType::Modified,
                path: piste.to_string_lossy().into_owned(),
            },
            true,
        );
    }

    let invite = pistes[0].to_string_lossy().into_owned();
    let (piste, album, mbid_album) = artistes_en_base(&db, &invite);
    assert_eq!(
        piste, "David Gilmour",
        "#7933 : après la retouche, la piste doit porter son ARTIST, pas l'artiste de l'album"
    );
    assert_eq!(album, "Pink Floyd", "l'album garde son ALBUMARTIST");
    assert_ne!(
        mbid_album.as_deref(),
        Some(MBID_INVITE),
        "#7933 : le MBID de l'invité ne devient pas celui de l'artiste d'album"
    );

    // TÉMOIN — ARTIST = ALBUMARTIST : la piste reste sous l'artiste d'album.
    let (piste, album, _) = artistes_en_base(&db, &pistes[1].to_string_lossy());
    assert_eq!(
        (piste.as_str(), album.as_str()),
        ("Pink Floyd", "Pink Floyd")
    );

    // Le scan complet relit le même fichier de la même façon : les deux
    // chemins ne divergent plus.
    let (lus, _) = tune_core::scanner::walker::scan_files_parallel(&pistes[..1], true, None);
    let mut imp = crate::scan_import::TrackImporter::new(
        db.clone(),
        true,
        dossier.join("cache"),
        crate::scan_import::PorteeDuScan::TOUT,
    );
    let (au_scan, _) = imp.import(&lus[0]).expect("import du scan");
    let nom_au_scan = ArtistRepo::with_backend(db.clone())
        .get(au_scan.artist_id.expect("un artiste"))
        .unwrap()
        .unwrap()
        .name;
    assert_eq!(nom_au_scan, "David Gilmour", "le scan complet lit ARTIST");
}

/// Un fichier déposé dans une bibliothèque neuve : l'artiste d'album naît
/// pendant la relecture du surveillant. Le MBID de l'invité ne doit pas lui
/// être donné (le dépôt rapproche d'abord par MBID, #3179).
#[test]
fn le_surveillant_ne_donne_pas_le_mbid_de_l_invite_a_l_album_7933() {
    let dossier = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        "mbid-invite-7933",
    );
    let db =
        tune_core::db::sqlite::SqliteDb::open(&dossier.join("tune.db").to_string_lossy()).unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    let db: Arc<dyn DbBackend> = Arc::new(db);
    let album_dir = dossier.join("Fritz Reiner").join("Scheherazade");
    std::fs::create_dir_all(&album_dir).unwrap();
    let deposer = |nom: &str, artiste: &str, mbid: &str| -> String {
        let piste = album_dir.join(nom);
        std::fs::write(&piste, flac_8_canaux()).unwrap();
        baliser(
            &piste,
            &[
                ("TITLE", nom),
                ("ARTIST", artiste),
                ("ALBUMARTIST", "Fritz Reiner"),
                ("ALBUM", "Scheherazade"),
                ("MUSICBRAINZ_ARTISTID", mbid),
            ],
            std::time::SystemTime::now(),
        );
        let chemin = piste.to_string_lossy().into_owned();
        reimporter_fichier_surveillant(
            &db,
            &FileChange {
                change_type: ChangeType::Added,
                path: chemin.clone(),
            },
            true,
        );
        chemin
    };

    let invite = deposer("01.flac", "Chicago Symphony Orchestra", MBID_INVITE);
    let (piste, album, mbid_album) = artistes_en_base(&db, &invite);
    assert_eq!(
        (piste.as_str(), album.as_str()),
        ("Chicago Symphony Orchestra", "Fritz Reiner")
    );
    assert_eq!(
        mbid_album, None,
        "#7933 : le MBID de l'invité ne doit pas identifier l'artiste d'album"
    );
}
