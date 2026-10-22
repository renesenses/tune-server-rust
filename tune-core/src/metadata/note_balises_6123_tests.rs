//! #6123 — la note d'une piste, lue dans de VRAIS fichiers étiquetés.
//!
//! Chaque témoin copie une fixture, y écrit la balise de note telle que
//! l'écrit le logiciel cité, puis relit le fichier par le chemin du scan
//! (`read_metadata`). L'échelle elle-même est gardée dans `note.rs`.

use super::*;
use lofty::config::WriteOptions;
use lofty::tag::TagExt;

fn copie(fixture: &str, nom: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let source = format!("{}/tests/fixtures/{fixture}", env!("CARGO_MANIFEST_DIR"));
    let cible = dir.path().join(nom);
    std::fs::copy(&source, &cible).unwrap();
    (dir, cible)
}

fn note_lue(chemin: &std::path::Path) -> Option<u8> {
    read_metadata(chemin)
        .expect("le fichier étiqueté doit se lire")
        .rating
}

fn vorbis(chemin: &std::path::Path, champs: &[(&str, &str)]) {
    let mut vc = lofty::ogg::VorbisComments::new();
    vc.push("TITLE".into(), "Piste notée".into());
    for (k, v) in champs {
        vc.push((*k).into(), (*v).into());
    }
    vc.save_to_path(chemin, WriteOptions::default()).unwrap();
}

fn id3(chemin: &std::path::Path, remplir: impl FnOnce(&mut lofty::id3::v2::Id3v2Tag)) {
    use lofty::tag::Accessor;
    let mut tag = lofty::id3::v2::Id3v2Tag::new();
    tag.set_title("Piste notée".into());
    remplir(&mut tag);
    tag.save_to_path(chemin, WriteOptions::default()).unwrap();
}

fn popm(adresse: &str, octet: u8) -> lofty::id3::v2::Frame<'static> {
    lofty::id3::v2::Frame::Popularimeter(lofty::id3::v2::PopularimeterFrame::new(
        adresse.to_string(),
        octet,
        7,
    ))
}

/// Le cas du ticket : Vorbis `RATING` sur 0–5 (foobar2000, Roon).
#[test]
fn flac_rating_sur_cinq_6123() {
    let (_d, f) = copie("test.flac", "a.flac");
    vorbis(&f, &[("RATING", "4")]);
    assert_eq!(note_lue(&f), Some(4), "RATING=4 d'un FLAC n'est pas lu");
}

/// `RATING` sur 0–100 (MusicBee, MediaMonkey) : ÷ 20.
#[test]
fn flac_rating_sur_cent_6123() {
    let (_d, f) = copie("test.flac", "a.flac");
    vorbis(&f, &[("RATING", "60")]);
    assert_eq!(note_lue(&f), Some(3));
}

/// Un `RATING=0` explicite est une note : zéro étoile.
#[test]
fn flac_rating_zero_est_une_note_6123() {
    let (_d, f) = copie("test.flac", "a.flac");
    vorbis(&f, &[("RATING", "0")]);
    assert_eq!(note_lue(&f), Some(0));
}

/// `FMPS_RATING` (Quod Libet, Amarok) : lofty ne la connaît pas.
#[test]
fn flac_fmps_rating_6123() {
    let (_d, f) = copie("test.flac", "a.flac");
    vorbis(&f, &[("FMPS_RATING", "0.8")]);
    assert_eq!(
        note_lue(&f),
        Some(4),
        "FMPS_RATING=0.8 d'un FLAC n'est pas lu"
    );
}

/// `RATING` gagne sur `FMPS_RATING`.
#[test]
fn flac_rating_avant_fmps_6123() {
    let (_d, f) = copie("test.flac", "a.flac");
    vorbis(&f, &[("FMPS_RATING", "0.2"), ("RATING", "5")]);
    assert_eq!(note_lue(&f), Some(5));
}

/// Sans balise de note : rien d'inventé.
#[test]
fn flac_sans_note_6123() {
    let (_d, f) = copie("test.flac", "a.flac");
    vorbis(&f, &[("ARTIST", "Quelqu'un")]);
    assert_eq!(note_lue(&f), None);
}

/// Ogg Vorbis : le même `RATING`.
#[test]
fn ogg_rating_6123() {
    let (_d, f) = copie("test_vorbis.ogg", "a.ogg");
    vorbis(&f, &[("RATING", "2")]);
    assert_eq!(note_lue(&f), Some(2));
}

/// ID3 `POPM` de Windows Media Player : 196 = 4 étoiles.
#[test]
fn mp3_popm_wmp_6123() {
    let (_d, f) = copie("test.mp3", "a.mp3");
    id3(&f, |t| {
        t.insert(popm("Windows Media Player 9 Series", 196));
    });
    assert_eq!(note_lue(&f), Some(4), "POPM 196 d'un MP3 n'est pas lu");
}

/// `POPM` à 1 : une vraie étoile (relue brute, lofty rend 1 pour 0 aussi).
#[test]
fn mp3_popm_une_etoile_6123() {
    let (_d, f) = copie("test.mp3", "a.mp3");
    id3(&f, |t| {
        t.insert(popm("MusicBee", 1));
    });
    assert_eq!(note_lue(&f), Some(1));
}

/// `POPM` à 0 : « note inconnue » selon la norme ID3, pas zéro étoile.
#[test]
fn mp3_popm_zero_reste_sans_note_6123() {
    let (_d, f) = copie("test.mp3", "a.mp3");
    id3(&f, |t| {
        t.insert(popm("MusicBee", 0));
    });
    assert_eq!(
        note_lue(&f),
        None,
        "un POPM à 0 (compteur d'écoutes seul) est devenu une étoile"
    );
}

/// `POPM` de Picard : 51 par étoile.
#[test]
fn mp3_popm_picard_6123() {
    let (_d, f) = copie("test.mp3", "a.mp3");
    id3(&f, |t| {
        t.insert(popm("users@musicbrainz.org", 153));
    });
    assert_eq!(note_lue(&f), Some(3));
}

/// `TXXX:RATING` (Mp3tag) : lofty ne le surface pas.
#[test]
fn mp3_txxx_rating_6123() {
    let (_d, f) = copie("test.mp3", "a.mp3");
    id3(&f, |t| {
        t.insert_user_text("RATING".into(), "3".into());
    });
    assert_eq!(note_lue(&f), Some(3), "TXXX:RATING d'un MP3 n'est pas lu");
}

/// `TXXX:FMPS_Rating` (Amarok, Clementine).
#[test]
fn mp3_txxx_fmps_6123() {
    let (_d, f) = copie("test.mp3", "a.mp3");
    id3(&f, |t| {
        t.insert_user_text("FMPS_Rating".into(), "0.4".into());
    });
    assert_eq!(note_lue(&f), Some(2));
}

/// MP4 : l'atome `rate`, en texte sur 0–100.
#[test]
fn m4a_rate_6123() {
    use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst};
    use lofty::tag::Accessor;
    let (_d, f) = copie("test.m4a", "a.m4a");
    let mut ilst = Ilst::new();
    ilst.set_title("Piste notée".into());
    ilst.insert(
        Atom::from_collection(
            AtomIdent::Fourcc(*b"rate"),
            vec![AtomData::UTF8("80".into())],
        )
        .unwrap(),
    );
    ilst.save_to_path(&f, WriteOptions::default()).unwrap();
    assert_eq!(note_lue(&f), Some(4), "l'atome MP4 `rate` n'est pas lu");
}

/// DSF : lofty ne connaît pas le format, le tag ID3 est lu par le lecteur
/// maison — son `POPM` aussi.
#[test]
fn dsf_popm_6123() {
    let mut tag = build_id3v2_tag(&[("TIT2", "Piste DSD")]);
    // Trame POPM v2.3 : adresse + NUL + octet + compteur sur 4 octets.
    let mut corps = b"Windows Media Player 9 Series\0".to_vec();
    corps.push(255);
    corps.extend_from_slice(&[0, 0, 0, 3]);
    let mut trame = b"POPM".to_vec();
    trame.extend_from_slice(&(corps.len() as u32).to_be_bytes());
    trame.extend_from_slice(&[0, 0]);
    trame.extend_from_slice(&corps);
    tag.extend_from_slice(&trame);
    // Taille du tag (synchsafe) recalculée avec la trame ajoutée.
    let taille = tag.len() - 10;
    tag[6..10].copy_from_slice(&[
        ((taille >> 21) & 0x7F) as u8,
        ((taille >> 14) & 0x7F) as u8,
        ((taille >> 7) & 0x7F) as u8,
        (taille & 0x7F) as u8,
    ]);
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("a.dsf");
    std::fs::write(&f, build_dsf_bytes(Some(&tag))).unwrap();
    let meta = read_metadata(&f).expect("le DSF doit se lire");
    assert_eq!(meta.title.as_deref(), Some("Piste DSD"), "repère : tag lu");
    assert_eq!(meta.rating, Some(5), "le POPM d'un DSF n'est pas lu");
}
