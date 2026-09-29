//! #5454 (Fuccaro, fil 1317) — la pochette d'un album dont les pistes portent
//! des jaquettes DIFFÉRENTES est l'image portée par la MAJORITÉ des pistes ;
//! à égalité, celle de la première piste dans l'ordre du disque (disque, puis
//! numéro de piste). Décision de Bertrand du 29/09/2026.
//!
//! Avant : la première piste LUE par le scan donnait sa jaquette à l'album
//! (`pochette_disque::suivre_la_piste` sur un album sans pochette), et
//! `albums_with_cover` figeait ce choix pour tout le scan. Le single éponyme
//! *À partir de maintenant*, lu le premier, imposait son image à l'album de
//! Hallyday.
//!
//! Ces témoins passent par le trajet de PRODUCTION d'un lot de scan : import
//! en mode différé (#5202), lignes écrites en base, puis
//! `traiter_les_pochettes_differees` et `poser_les_pochettes_de_piste`. Les
//! jaquettes sont de vrais blocs PICTURE dans de vrais FLAC : `meta.cover_art`
//! est vide, comme en production, et tout est relu sur le disque.
use super::*;
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::flac::FlacFile;
use lofty::ogg::OggPictureStorage;
use lofty::picture::{MimeType, Picture, PictureInformation, PictureType};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::models::SourcePochette;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::track_repo::TrackRepo;
use tune_core::library::artwork::content_hash;
use tune_core::metadata::TrackMetadata;

/// La jaquette du single éponyme.
pub(super) const SINGLE: &[u8] = b"\xFF\xD8\xFF\xE0JAQUETTE-DU-SINGLE-A-PARTIR-DE-MAINTENANT-5454";
/// La jaquette de l'album, portée par les neuf autres pistes.
pub(super) const ALBUM: &[u8] = b"\xFF\xD8\xFF\xE0JAQUETTE-DE-L-ALBUM-A-PARTIR-DE-MAINTENANT-5454";
const TELEVERSEE: &[u8] = b"\xFF\xD8\xFF\xE0POCHETTE-TELEVERSEE-5454";

pub(super) fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    Arc::new(db)
}

fn gabarit() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../tune-core/tests/fixtures/test.flac")
}

/// Un vrai FLAC portant `jaquette` en bloc PICTURE.
fn flac_avec_jaquette(chemin: &Path, jaquette: &[u8]) {
    std::fs::copy(gabarit(), chemin).unwrap();
    let mut f = std::fs::File::open(chemin).unwrap();
    let mut flac = FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
    drop(f);
    while !flac.pictures().is_empty() {
        flac.remove_picture(0);
    }
    let pic = Picture::unchecked(jaquette.to_vec())
        .pic_type(PictureType::CoverFront)
        .mime_type(MimeType::Jpeg)
        .build();
    flac.insert_picture(pic, Some(PictureInformation::default()))
        .unwrap();
    flac.save_to_path(chemin, WriteOptions::default()).unwrap();
}

/// Une piste de l'album de Hallyday : ses balises (déjà lues, comme par la
/// lecture parallèle du scan : SANS image) et son fichier sur le disque.
pub(super) fn piste(dossier: &Path, numero: u32, titre: &str, jaquette: &[u8]) -> ScannedFile {
    let chemin = dossier.join(format!("{numero:02} - {titre}.flac"));
    flac_avec_jaquette(&chemin, jaquette);
    ScannedFile {
        path: chemin.to_string_lossy().into_owned(),
        metadata: Some(TrackMetadata {
            title: Some(titre.to_string()),
            artist: Some("Johnny Hallyday".into()),
            album: Some("À partir de maintenant".into()),
            album_artist: Some("Johnny Hallyday".into()),
            track_number: Some(numero),
            disc_number: Some(1),
            ..Default::default()
        }),
        unsupported: None,
        audio_hash: None,
        file_size: 4096,
        mtime: 1_700_000_000.0,
    }
}

/// L'album du signalement : la piste 1 est le single éponyme (image A), les
/// neuf autres portent la jaquette de l'album (image B).
fn album_de_hallyday(dossier: &Path) -> Vec<ScannedFile> {
    std::fs::create_dir_all(dossier).unwrap();
    (1..=10u32)
        .map(|n| {
            if n == 1 {
                piste(dossier, n, "À partir de maintenant", SINGLE)
            } else {
                piste(dossier, n, &format!("Titre {n}"), ALBUM)
            }
        })
        .collect()
}

/// Un scan : les fichiers de `ordre`, dans CET ordre de lecture, découpés en
/// lots de `taille_de_lot`. Chaque lot suit le trajet de production : import
/// différé, lignes écrites, puis pochettes après le « COMMIT ».
pub(super) fn scanner(
    db: &Arc<dyn DbBackend>,
    cache: &Path,
    ordre: &[ScannedFile],
    taille_de_lot: usize,
    complet: bool,
) {
    let mut imp = TrackImporter::new(db.clone(), true, cache.to_path_buf(), PorteeDuScan::TOUT)
        .with_force_artwork(complet)
        .avec_pochettes_differees();
    let pistes = TrackRepo::with_backend(db.clone());
    for (i, lot) in ordre.chunks(taille_de_lot).enumerate() {
        imp.begin_batch(lot);
        for f in lot {
            let (mut t, _) = imp.import(f).expect("import");
            match pistes.get_by_path(&f.path).unwrap() {
                Some(existante) => {
                    t.id = existante.id;
                    pistes.update_batch(std::slice::from_ref(&t)).unwrap();
                }
                None => {
                    pistes.create(&t).unwrap();
                }
            }
        }
        let jamais = || false;
        let mut lectures =
            crate::lecture_bornee::LecturesBornees::new(Duration::from_secs(30), &jamais);
        let a_poser = imp.traiter_les_pochettes_differees(&mut lectures, i);
        poser_les_pochettes_de_piste(db, &a_poser);
    }
}

/// La pochette de l'album des pistes de `fichiers`.
pub(super) fn pochette_album(db: &Arc<dyn DbBackend>, fichiers: &[ScannedFile]) -> Option<String> {
    let aid = TrackRepo::with_backend(db.clone())
        .get_by_path(&fichiers[0].path)
        .unwrap()
        .expect("piste indexée")
        .album_id
        .expect("album");
    AlbumRepo::with_backend(db.clone())
        .get(aid)
        .unwrap()
        .unwrap()
        .cover_path
}

/// La colonne BRUTE `tracks.cover_path` (la pochette PROPRE), sans le
/// `COALESCE` de la lecture.
pub(super) fn pochette_propre(db: &Arc<dyn DbBackend>, f: &ScannedFile) -> Option<String> {
    let p: [&dyn tune_core::db::backend::ToSqlValue; 1] = [&f.path];
    db.query_one_strong("SELECT cover_path FROM tracks WHERE file_path = ?", &p)
        .unwrap()
        .and_then(|cols| cols.first().and_then(|v| v.as_string()))
}

/// Ce qu'affiche chaque piste : `COALESCE(t.cover_path, al.cover_path)`.
pub(super) fn pochette_affichee(db: &Arc<dyn DbBackend>, f: &ScannedFile) -> Option<String> {
    TrackRepo::with_backend(db.clone())
        .get_by_path(&f.path)
        .unwrap()
        .expect("piste indexée")
        .cover_path
}

fn nommer(h: Option<&str>) -> &'static str {
    match h {
        None => "aucune",
        Some(h) if h == content_hash(SINGLE) => "single (A)",
        Some(h) if h == content_hash(ALBUM) => "album (B)",
        Some(h) if h == content_hash(TELEVERSEE) => "téléversée",
        Some(_) => "autre",
    }
}

/// Des ordres de lecture : celui du disque, l'inverse, le single au milieu,
/// le single en dernier, et des permutations pseudo-aléatoires fixes.
fn ordres_de_lecture(fichiers: &[ScannedFile]) -> Vec<(String, Vec<ScannedFile>)> {
    let mut ordres = vec![("disque".to_string(), fichiers.to_vec())];
    let mut inverse = fichiers.to_vec();
    inverse.reverse();
    ordres.push(("inverse".into(), inverse));
    let mut milieu = fichiers[1..].to_vec();
    milieu.insert(milieu.len() / 2, fichiers[0].clone());
    ordres.push(("single au milieu".into(), milieu));
    let mut dernier = fichiers[1..].to_vec();
    dernier.push(fichiers[0].clone());
    ordres.push(("single en dernier".into(), dernier));
    for graine in [7u64, 42, 1954] {
        let mut v = fichiers.to_vec();
        let mut x = graine;
        for i in (1..v.len()).rev() {
            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            v.swap(i, (x >> 33) as usize % (i + 1));
        }
        ordres.push((format!("permutation {graine}"), v));
    }
    ordres
}

/// LE FAIT de #5454 : dix pistes, un single à l'image A (la piste 1, lue la
/// première dans l'ordre du disque), neuf pistes à l'image B. La pochette de
/// l'album est B, QUEL QUE SOIT l'ordre de lecture, en un lot comme en
/// plusieurs ; le single garde sa jaquette pour lui, les neuf autres n'écrivent
/// rien et retombent sur celle de l'album.
///
/// Rouge contre le code d'avant dès que le single est lu le premier (ordre du
/// disque) : l'album prenait l'image A.
#[test]
fn dix_pistes_un_single_la_pochette_est_la_majoritaire_quel_que_soit_l_ordre_5454() {
    let tmp = tempfile::tempdir().unwrap();
    let fichiers = album_de_hallyday(&tmp.path().join("Johnny Hallyday - À partir de maintenant"));
    let mut ecarts = Vec::new();
    for (nom, ordre) in ordres_de_lecture(&fichiers) {
        for taille_de_lot in [100usize, 3] {
            let db = base();
            let cache = tmp.path().join(format!("cache-{nom}-{taille_de_lot}"));
            scanner(&db, &cache, &ordre, taille_de_lot, false);
            let album = pochette_album(&db, &fichiers);
            let single = pochette_propre(&db, &fichiers[0]);
            let autres: Vec<Option<String>> = fichiers[1..]
                .iter()
                .map(|f| pochette_propre(&db, f))
                .collect();
            let affichees: Vec<&str> = fichiers
                .iter()
                .map(|f| nommer(pochette_affichee(&db, f).as_deref()))
                .collect();
            if album.as_deref() != Some(content_hash(ALBUM).as_str())
                || single.as_deref() != Some(content_hash(SINGLE).as_str())
                || autres.iter().any(Option::is_some)
            {
                ecarts.push(format!(
                    "ordre « {nom} », lots de {taille_de_lot} : album = {}, single = {}, \
                     pochettes propres des 9 autres = {:?}, affichage = {affichees:?}",
                    nommer(album.as_deref()),
                    nommer(single.as_deref()),
                    autres
                        .iter()
                        .map(|a| nommer(a.as_deref()))
                        .collect::<Vec<_>>(),
                ));
            }
        }
    }
    assert!(
        ecarts.is_empty(),
        "#5454 : la pochette de l'album doit être l'image de la MAJORITÉ des pistes (B), \
         le single gardant la sienne (A) :\n{}",
        ecarts.join("\n")
    );
}

/// L'ÉGALITÉ : deux images portées chacune par deux pistes. La première piste
/// dans l'ordre du disque (disque, puis numéro de piste) l'emporte — ici la
/// piste 1 du disque 1, même lue en dernier, et même quand un disque 2 la
/// précède à la lecture.
#[test]
fn a_egalite_la_premiere_piste_du_disque_l_emporte_5454() {
    let tmp = tempfile::tempdir().unwrap();
    let dossier = tmp.path().join("Egalite");
    std::fs::create_dir_all(&dossier).unwrap();
    let mut fichiers = vec![
        piste(&dossier, 1, "Un", ALBUM),
        piste(&dossier, 2, "Deux", SINGLE),
        piste(&dossier, 3, "Trois", SINGLE),
        piste(&dossier, 4, "Quatre", ALBUM),
    ];
    // Deux disques : la piste 4 devient la piste 1 du disque 2 — elle vient
    // APRÈS toutes celles du disque 1 dans l'ordre du disque.
    if let Some(m) = fichiers[3].metadata.as_mut() {
        m.disc_number = Some(2);
        m.track_number = Some(1);
    }
    let mut ecarts = Vec::new();
    for (nom, ordre) in ordres_de_lecture(&fichiers).into_iter().chain([(
        "B en dernier".to_string(),
        vec![
            fichiers[1].clone(),
            fichiers[2].clone(),
            fichiers[3].clone(),
            fichiers[0].clone(),
        ],
    )]) {
        let db = base();
        let cache = tmp.path().join(format!("cache-{nom}"));
        scanner(&db, &cache, &ordre, 100, false);
        let album = pochette_album(&db, &fichiers);
        let propres: Vec<&str> = fichiers
            .iter()
            .map(|f| nommer(pochette_propre(&db, f).as_deref()))
            .collect();
        if album.as_deref() != Some(content_hash(ALBUM).as_str())
            || propres != ["aucune", "single (A)", "single (A)", "aucune"]
        {
            ecarts.push(format!(
                "ordre « {nom} » : album = {}, pochettes propres = {propres:?}",
                nommer(album.as_deref())
            ));
        }
    }
    assert!(
        ecarts.is_empty(),
        "#5454 : à égalité (2 contre 2), la jaquette de la PREMIÈRE piste du disque (B, \
         disque 1 piste 1) doit l'emporter :\n{}",
        ecarts.join("\n")
    );
}

/// Une pochette TÉLÉVERSÉE reste prioritaire : ni la majorité, ni une
/// Analyse complète qui lit le single en premier n'y touchent. Les pistes à
/// la jaquette majoritaire n'écrivent rien — elles montrent la pochette
/// téléversée — et le single garde la sienne.
#[test]
fn une_pochette_televersee_reste_prioritaire_5454() {
    let tmp = tempfile::tempdir().unwrap();
    let fichiers = album_de_hallyday(&tmp.path().join("Televersee"));
    let db = base();
    let cache = tmp.path().join("cache");
    scanner(&db, &cache, &fichiers, 100, false);
    let aid = TrackRepo::with_backend(db.clone())
        .get_by_path(&fichiers[0].path)
        .unwrap()
        .unwrap()
        .album_id
        .unwrap();
    AlbumRepo::with_backend(db.clone())
        .force_update_cover_path(aid, &content_hash(TELEVERSEE), SourcePochette::Televersee)
        .unwrap();
    for complet in [false, true] {
        // Le single lu en premier, puis les autres dans le désordre.
        let mut ordre = fichiers.clone();
        ordre[1..].reverse();
        scanner(&db, &cache, &ordre, 100, complet);
        let affichees: Vec<&str> = fichiers
            .iter()
            .map(|f| nommer(pochette_affichee(&db, f).as_deref()))
            .collect();
        assert_eq!(
            (
                nommer(pochette_album(&db, &fichiers).as_deref()),
                affichees[0]
            ),
            ("téléversée", "single (A)"),
            "complet = {complet} : la pochette téléversée doit rester celle de l'album, le \
             single gardant la sienne. Affichage : {affichees:?}"
        );
        assert!(
            affichees[1..].iter().all(|a| *a == "téléversée"),
            "complet = {complet} : les pistes à la jaquette majoritaire retombent sur la \
             pochette téléversée. Affichage : {affichees:?}"
        );
    }
}
