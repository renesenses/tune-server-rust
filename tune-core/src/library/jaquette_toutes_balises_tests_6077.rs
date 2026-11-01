//! #6077 (régis, forum-hifi 41831 #770) — des pochettes qu'Audirvana affiche
//! manquaient dans Tune. Côté jaquette INTÉGRÉE, `extract_cover_art` ne lisait
//! que la balise principale du fichier, et en prenait la première image.
//!
//! Chaque témoin part d'un vrai FLAC (`tests/fixtures/test.flac`) relu par
//! lofty, comme au scan.
use super::*;
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::flac::FlacFile;
use lofty::ogg::{OggPictureStorage, VorbisComments};
use lofty::picture::{MimeType, Picture, PictureInformation, PictureType};
use lofty::tag::{Accessor, Tag, TagExt, TagType};

const FACE: &[u8] = b"\xFF\xD8\xFF\xE0FACE-AVANT-6077";
const DOS: &[u8] = b"\xFF\xD8\xFF\xE0DOS-DU-BOITIER-6077";

fn image(octets: &[u8], genre: PictureType) -> Picture {
    Picture::unchecked(octets.to_vec())
        .pic_type(genre)
        .mime_type(MimeType::Jpeg)
        .build()
}

/// Un vrai FLAC : balise Vorbis avec un titre, et les images données dans
/// l'ordre, en blocs PICTURE.
fn flac(chemin: &Path, images: &[Picture]) {
    let gabarit = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test.flac");
    std::fs::copy(gabarit, chemin).unwrap();
    let mut f = std::fs::File::open(chemin).unwrap();
    let mut fl = FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
    drop(f);
    while !fl.pictures().is_empty() {
        fl.remove_picture(0);
    }
    let mut vc = VorbisComments::default();
    vc.set_title("Titre 6077".into());
    fl.set_vorbis_comments(vc);
    for p in images {
        fl.insert_picture(p.clone(), Some(PictureInformation::default()))
            .unwrap();
    }
    fl.save_to_path(chemin, WriteOptions::default()).unwrap();
}

#[test]
fn la_jaquette_de_l_en_tete_id3v2_d_un_flac_est_lue_6077() {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("01 - piste.flac");
    // FLAC sans aucun bloc PICTURE, balise Vorbis présente…
    flac(&chemin, &[]);
    // … précédé d'un en-tête ID3v2 qui porte la pochette (APIC).
    let mut id3 = Tag::new(TagType::Id3v2);
    id3.push_picture(image(FACE, PictureType::CoverFront));
    let mut octets = Vec::new();
    id3.dump_to(&mut octets, WriteOptions::default()).unwrap();
    octets.extend(std::fs::read(&chemin).unwrap());
    std::fs::write(&chemin, octets).unwrap();

    // Contrôle du montage : lofty voit bien les deux balises, la Vorbis en
    // balise principale et sans image.
    let lu = lofty::read_from_path(&chemin).unwrap();
    {
        use lofty::file::TaggedFileExt;
        assert_eq!(lu.primary_tag_type(), TagType::VorbisComments);
        assert!(lu.primary_tag().unwrap().pictures().is_empty());
        assert!(lu.tag(TagType::Id3v2).is_some());
    }

    let (octets, mime) = extract_cover_art(&chemin).expect("la jaquette ID3v2 doit être lue");
    assert_eq!(octets, FACE);
    assert_eq!(mime, "image/jpeg");
}

#[test]
fn la_face_avant_passe_avant_le_dos_range_en_premier_6077() {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("01 - piste.flac");
    flac(
        &chemin,
        &[
            image(DOS, PictureType::CoverBack),
            image(FACE, PictureType::CoverFront),
        ],
    );
    let (octets, _) = extract_cover_art(&chemin).expect("jaquette");
    assert_eq!(
        octets, FACE,
        "le dos du boîtier ne doit pas devenir la pochette"
    );
}

#[test]
fn sans_face_avant_la_premiere_image_reste_retenue_6077() {
    let dir = tempfile::tempdir().unwrap();
    let chemin = dir.path().join("01 - piste.flac");
    flac(
        &chemin,
        &[
            image(DOS, PictureType::Other),
            image(FACE, PictureType::CoverBack),
        ],
    );
    let (octets, _) = extract_cover_art(&chemin).expect("jaquette");
    assert_eq!(octets, DOS);
}
