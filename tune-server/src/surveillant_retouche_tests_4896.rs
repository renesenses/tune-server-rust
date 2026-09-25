//! #4896 (Didier, fil 1904, Windows 11) — balises retouchées dans Mp3tag
//! APRÈS l'indexation, Tune lancé : l'album gardait le nom de son dossier
//! jusqu'à une réindexation manuelle.
//!
//! Ces épreuves jouent la VRAIE voie du surveillant
//! ([`reimporter_fichier_surveillant`] puis
//! [`realigner_albums_sur_les_balises`], dans l'ordre de la boucle de
//! `spawn_file_watcher`) sur de vrais FLAC, indexés d'abord par le scan
//! (`TrackImporter`), puis retouchés sur le disque.
//!
//! Les événements sont ceux que `notify` 7.0.0 fabrique sous Windows
//! (`src/windows.rs`, `handle_event`) :
//! - `FILE_ACTION_MODIFIED` → `Modify(Any)` → `ChangeType::Modified` : écriture
//!   en place (FLAC au remplissage suffisant) ;
//! - `FILE_ACTION_REMOVED` puis `FILE_ACTION_ADDED` → `Remove` puis `Create` :
//!   fichier remplacé par un déplacement depuis un autre dossier. La fusion du
//!   lot garde le dernier événement du chemin : `ChangeType::Added`.
//!
//! La traduction événement → `ChangeType` est éprouvée à part, dans
//! `tune-core/src/scanner/watcher.rs`.
use super::{realigner_albums_sur_les_balises, reimporter_fichier_surveillant};
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::flac::FlacFile;
use lofty::ogg::VorbisComments;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::backend::DbBackend;
use tune_core::db::track_repo::TrackRepo;
use tune_core::scanner::watcher::{ChangeType, FileChange};

fn crc8(octets: &[u8]) -> u8 {
    let mut c = 0u8;
    for &o in octets {
        c ^= o;
        for _ in 0..8 {
            c = if c & 0x80 != 0 {
                (c << 1) ^ 0x07
            } else {
                c << 1
            };
        }
    }
    c
}

fn crc16(octets: &[u8]) -> u16 {
    let mut c = 0u16;
    for &o in octets {
        c ^= u16::from(o) << 8;
        for _ in 0..8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x8005
            } else {
                c << 1
            };
        }
    }
    c
}

/// Un FLAC 7.1 (8 canaux, 24 bits, 48 kHz) de 4 096 échantillons de silence —
/// le gabarit de `coffret_multicanal_tests_4846`, décodé par symphonia.
fn flac_8_canaux() -> Vec<u8> {
    let mut v = b"fLaC".to_vec();
    v.extend_from_slice(&[0x80, 0, 0, 34]);
    v.extend_from_slice(&4096u16.to_be_bytes());
    v.extend_from_slice(&4096u16.to_be_bytes());
    v.extend_from_slice(&[0; 6]);
    let champs: u64 = (48_000u64 << 44) | (7 << 41) | (23 << 36) | 4096;
    v.extend_from_slice(&champs.to_be_bytes());
    v.extend_from_slice(&[0; 16]);
    let mut trame = vec![0xFF, 0xF8, 0xCA, 0x7C, 0x00];
    trame.push(crc8(&trame));
    for _ in 0..8 {
        trame.extend_from_slice(&[0x00, 0, 0, 0]);
    }
    let c = crc16(&trame);
    trame.extend_from_slice(&c.to_be_bytes());
    v.extend(trame);
    v
}

/// Remplace les commentaires Vorbis du fichier, puis avance sa date de
/// modification : l'option Mp3tag « sans modifier l'horodatage » est
/// DÉCOCHÉE chez Didier (capture vue).
fn baliser(piste: &Path, tags: &[(&str, &str)], mtime: std::time::SystemTime) {
    let mut fh = std::fs::File::open(piste).expect("ouverture");
    let mut flac = FlacFile::read_from(&mut fh, ParseOptions::new()).expect("lecture FLAC");
    drop(fh);
    let mut vc = VorbisComments::default();
    for (k, v) in tags {
        vc.insert(k.to_string(), v.to_string());
    }
    flac.set_vorbis_comments(vc);
    flac.save_to_path(piste, WriteOptions::default())
        .expect("écriture des tags");
    std::fs::File::options()
        .write(true)
        .open(piste)
        .and_then(|f| f.set_modified(mtime))
        .expect("date de modification");
}

fn base() -> Arc<dyn DbBackend> {
    let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

const TITRES: [&str; 2] = ["Speak To Me", "Breathe"];

/// Le coffret de Didier, SANS balise ALBUM ni ALBUMARTIST, indexé par le scan
/// comme au premier lancement. Rend la racine (à garder vivante) et les pistes.
///
/// Racine sous le dossier courant et NON sous le dossier temporaire du
/// système : `is_tune_temp_file` écarte tout ce qui vit sous ce dernier.
fn coffret_indexe(
    db: &Arc<dyn DbBackend>,
    epreuve: &str,
) -> (tune_core::test_scratch::ScratchDir, Vec<PathBuf>) {
    let racine = tune_core::test_scratch::scratch_dir_in(
        std::env::current_dir().unwrap(),
        &format!("surveillant-4896-{epreuve}"),
    );
    let dossier = racine
        .join("Pink Floyd")
        .join("1973 - The Dark Side Of The Moon (50th Anniversary)")
        .join("Multichannel 7.1");
    std::fs::create_dir_all(&dossier).unwrap();
    let hier = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
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
    let (lus, _) = tune_core::scanner::walker::scan_files_parallel(&pistes, true, None);
    let mut imp = crate::scan_import::TrackImporter::new(
        db.clone(),
        true,
        racine.join("cache"),
        crate::scan_import::PorteeDuScan::TOUT,
    );
    let track_repo = TrackRepo::with_backend(db.clone());
    for sf in &lus {
        let (piste, _) = imp.import(sf).expect("import du scan");
        track_repo.create(&piste).expect("ligne de piste");
    }
    (racine, pistes)
}

/// Mp3tag pose ALBUM et ALBUMARTIST sur chaque piste, la date change.
fn retoucher_dans_mp3tag(pistes: &[PathBuf]) {
    retoucher_avec_artiste(pistes, "Pink Floyd");
}

/// Idem, avec l'artiste d'album donné.
fn retoucher_avec_artiste(pistes: &[PathBuf], artiste_d_album: &str) {
    let maintenant = std::time::SystemTime::now();
    for (i, (piste, titre)) in pistes.iter().zip(TITRES).enumerate() {
        let n = (i + 1).to_string();
        baliser(
            piste,
            &[
                ("TITLE", titre),
                ("ARTIST", "Pink Floyd"),
                ("ALBUMARTIST", artiste_d_album),
                ("ALBUM", "The Dark Side Of The Moon"),
                ("TRACKNUMBER", &n),
            ],
            maintenant,
        );
    }
}

/// Un lot du surveillant, dans l'ordre de la boucle de `spawn_file_watcher`.
fn lot_du_surveillant(db: &Arc<dyn DbBackend>, pistes: &[PathBuf], genre: ChangeType) {
    let mut albums = HashSet::new();
    for piste in pistes {
        let change = FileChange {
            change_type: genre.clone(),
            path: piste.to_string_lossy().into_owned(),
        };
        if let Some(aid) = reimporter_fichier_surveillant(db, &change, true) {
            albums.insert(aid);
        }
    }
    realigner_albums_sur_les_balises(db, &albums);
}

/// (titre de l'album, artiste d'album) de chaque piste, relus en base.
fn albums_en_base(db: &Arc<dyn DbBackend>, pistes: &[PathBuf]) -> Vec<(String, String)> {
    let track_repo = TrackRepo::with_backend(db.clone());
    let album_repo = AlbumRepo::with_backend(db.clone());
    pistes
        .iter()
        .map(|p| {
            let t = track_repo
                .get_by_path(&p.to_string_lossy())
                .unwrap()
                .expect("la piste est toujours en base");
            let a = album_repo
                .get(t.album_id.expect("album"))
                .unwrap()
                .expect("ligne album");
            (a.title, a.artist_name.unwrap_or_default())
        })
        .collect()
}

fn attendu() -> Vec<(String, String)> {
    TITRES
        .iter()
        .map(|_| {
            (
                "The Dark Side Of The Moon".to_string(),
                "Pink Floyd".to_string(),
            )
        })
        .collect()
}

/// Montage : sans ALBUM, le scan range bien le coffret sous un album qui
/// n'est PAS celui des balises à venir — c'est l'état de départ de Didier.
#[test]
fn montage_le_coffret_sans_balise_album_ne_porte_pas_le_titre_final_4896() {
    let db = base();
    let (_racine, pistes) = coffret_indexe(&db, "montage");
    let avant = albums_en_base(&db, &pistes);
    assert_ne!(avant, attendu(), "le montage doit partir d'un album faux");
    assert_eq!(avant[0], avant[1], "un seul album pour le dossier");
}

/// Windows, écriture EN PLACE : `FILE_ACTION_MODIFIED` → `Modify(Any)` →
/// `Modified`. Les pistes étaient relues ; la ligne album du dossier, elle,
/// restait figée (identité par dossier, `get_or_create_for_folder`).
#[test]
fn une_retouche_en_place_renomme_l_album_du_dossier_4896() {
    let db = base();
    let (_racine, pistes) = coffret_indexe(&db, "en-place");
    retoucher_dans_mp3tag(&pistes);
    lot_du_surveillant(&db, &pistes, ChangeType::Modified);
    assert_eq!(
        albums_en_base(&db, &pistes),
        attendu(),
        "#4896 — après la retouche Mp3tag, l'album suit ses balises ALBUM/ALBUMARTIST"
    );
}

/// Windows, fichier REMPLACÉ par déplacement : `FILE_ACTION_REMOVED` puis
/// `FILE_ACTION_ADDED` → la fusion garde `Added`. La branche « ajout » ne
/// supprimait pas l'ancienne ligne : la piste neuve n'entrait jamais.
#[test]
fn un_fichier_remplace_arrive_en_ajout_et_est_relu_4896() {
    let db = base();
    let (_racine, pistes) = coffret_indexe(&db, "remplace");
    let piste = &pistes[0];
    let maintenant = std::time::SystemTime::now();
    baliser(
        piste,
        &[
            ("TITLE", "Speak To Me (2023 Remix)"),
            ("ARTIST", "Pink Floyd"),
            ("TRACKNUMBER", "1"),
        ],
        maintenant,
    );
    lot_du_surveillant(&db, std::slice::from_ref(piste), ChangeType::Added);
    let track_repo = TrackRepo::with_backend(db.clone());
    let chemin = piste.to_string_lossy();
    let ligne = track_repo
        .get_by_path(&chemin)
        .unwrap()
        .expect("piste en base");
    assert_eq!(
        ligne.title, "Speak To Me (2023 Remix)",
        "#4896 — un « ajout » sur un chemin déjà indexé est un remplacement : la piste est relue"
    );
}

/// Garde : un « ajout » sur un fichier INCHANGÉ (même taille, même date) ne
/// relit rien — même garde que pour `Modified` (boucle macOS de Jean Marie).
#[test]
fn un_ajout_sur_un_fichier_inchange_ne_relit_rien_4896() {
    let db = base();
    let (_racine, pistes) = coffret_indexe(&db, "inchange");
    let track_repo = TrackRepo::with_backend(db.clone());
    let chemin = pistes[0].to_string_lossy().into_owned();
    let id_avant = track_repo.get_by_path(&chemin).unwrap().unwrap().id;
    lot_du_surveillant(&db, &pistes[..1], ChangeType::Added);
    let id_apres = track_repo.get_by_path(&chemin).unwrap().unwrap().id;
    assert_eq!(id_avant, id_apres, "la ligne n'a pas été recréée");
}

/// Garde : tant que le dossier n'est retouché qu'À MOITIÉ, l'album ne bouge
/// pas — une seule piste ne décide pas pour tout le dossier.
#[test]
fn un_dossier_retouche_a_moitie_garde_son_album_4896() {
    let db = base();
    let (_racine, pistes) = coffret_indexe(&db, "moitie");
    let avant = albums_en_base(&db, &pistes);
    retoucher_dans_mp3tag(&pistes[..1]);
    lot_du_surveillant(&db, &pistes[..1], ChangeType::Modified);
    let apres = albums_en_base(&db, &pistes);
    assert_eq!(apres[1], avant[1], "la piste non retouchée garde son album");
}

/// Garde : un titre d'album corrigé À LA MAIN dans Tune n'est pas défait.
#[test]
fn un_titre_edite_a_la_main_n_est_pas_defait_4896() {
    let db = base();
    let (_racine, pistes) = coffret_indexe(&db, "main");
    let aid = TrackRepo::with_backend(db.clone())
        .get_by_path(&pistes[0].to_string_lossy())
        .unwrap()
        .unwrap()
        .album_id
        .unwrap();
    AlbumRepo::with_backend(db.clone())
        .force_update_title(aid, "Mon titre")
        .unwrap();
    tune_core::db::album_metadata_repo::AlbumMetadataRepo::with_backend(db.clone())
        .marquer_edition_manuelle(aid, &["title"])
        .unwrap();
    retoucher_dans_mp3tag(&pistes);
    lot_du_surveillant(&db, &pistes, ChangeType::Modified);
    let apres = albums_en_base(&db, &pistes);
    assert_eq!(apres[0].0, "Mon titre", "le titre tenu à la main reste");
}

/// L'artiste d'album aussi : chez Didier, la ligne album portait comme
/// artiste le nom d'un dossier. Une ALBUMARTIST posée sur tout le dossier la
/// remplace.
#[test]
fn l_artiste_d_album_suit_aussi_les_balises_4896() {
    let db = base();
    let (_racine, pistes) = coffret_indexe(&db, "artiste");
    retoucher_avec_artiste(&pistes, "Pink Floyd & James Guthrie");
    lot_du_surveillant(&db, &pistes, ChangeType::Modified);
    for (titre, artiste) in albums_en_base(&db, &pistes) {
        assert_eq!(titre, "The Dark Side Of The Moon");
        assert_eq!(
            artiste, "Pink Floyd & James Guthrie",
            "#4896 — l'artiste d'album suit la balise ALBUMARTIST du dossier"
        );
    }
}
