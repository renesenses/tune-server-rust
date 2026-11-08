//! #8088 — le marquage explicite de la balise, lu au scan par
//! `read_metadata` sur de VRAIS conteneurs copiés des fixtures et balisés par
//! lofty : FLAC (`ITUNESADVISORY`, relu brut), MP3 (`TXXX:ITUNESADVISORY`),
//! M4A (atome `rtng`). Un fichier sans balise ne dit rien.

use super::*;
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;

#[test]
fn les_valeurs_de_la_balise_8088() {
    for (brut, attendu) in [
        ("1", Some(true)),
        (" 4 ", Some(true)),
        ("Explicit", Some(true)),
        ("0", Some(false)),
        ("2", Some(false)),
        ("clean", Some(false)),
        ("", None),
        ("3", None),
        ("oui", None),
    ] {
        assert_eq!(explicite_depuis_valeur(brut), attendu, "{brut:?}");
    }
}

fn copie(fixture: &str, epreuve: &str) -> crate::test_scratch::ScratchFile {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    let ext = Path::new(fixture).extension().unwrap().to_str().unwrap();
    let cible =
        crate::test_scratch::scratch_file(&format!("explicite8088-{epreuve}"), &format!(".{ext}"));
    std::fs::copy(&source, &cible).expect("copie de la fixture");
    cible
}

fn flac(epreuve: &str, valeur: Option<&str>) -> crate::test_scratch::ScratchFile {
    use lofty::flac::FlacFile;
    use lofty::ogg::VorbisComments;
    let chemin = copie("test.flac", epreuve);
    let mut fh = std::fs::File::open(&*chemin).unwrap();
    let mut f = FlacFile::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    if f.vorbis_comments().is_none() {
        f.set_vorbis_comments(VorbisComments::default());
    }
    let vc = f.vorbis_comments_mut().unwrap();
    vc.push("TITLE".into(), "Piste".into());
    if let Some(v) = valeur {
        vc.push("ITUNESADVISORY".into(), v.into());
    }
    f.save_to_path(&*chemin, WriteOptions::default()).unwrap();
    chemin
}

#[test]
fn un_flac_dit_son_marquage_au_scan_8088() {
    for (epreuve, valeur, attendu) in [
        ("flac-1", Some("1"), Some(true)),
        ("flac-2", Some("2"), Some(false)),
        ("flac-rien", None, None),
    ] {
        let chemin = flac(epreuve, valeur);
        let meta = read_metadata(&chemin).expect("métadonnées du FLAC");
        assert_eq!(
            meta.explicit, attendu,
            "{epreuve} : ITUNESADVISORY={valeur:?}"
        );
    }
}

#[test]
fn un_mp3_dit_son_marquage_au_scan_8088() {
    use lofty::id3::v2::Id3v2Tag;
    use lofty::mpeg::MpegFile;
    let chemin = copie("test.mp3", "mp3");
    let mut fh = std::fs::File::open(&*chemin).unwrap();
    let mut mp3 = MpegFile::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    let mut tag = mp3.id3v2().cloned().unwrap_or_else(Id3v2Tag::new);
    tag.insert_user_text("ITUNESADVISORY".into(), "1".into());
    mp3.set_id3v2(tag);
    mp3.save_to_path(&*chemin, WriteOptions::default()).unwrap();
    let meta = read_metadata(&chemin).expect("métadonnées du MP3");
    assert_eq!(meta.explicit, Some(true), "TXXX:ITUNESADVISORY=1 non lue");
}

#[test]
fn un_m4a_dit_son_marquage_au_scan_8088() {
    use lofty::mp4::{AdvisoryRating, Ilst, Mp4File};
    let chemin = copie("test.m4a", "m4a");
    let mut fh = std::fs::File::open(&*chemin).unwrap();
    let mut m4a = Mp4File::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    let mut ilst = m4a.ilst().cloned().unwrap_or_else(Ilst::default);
    ilst.set_advisory_rating(AdvisoryRating::Explicit);
    m4a.set_ilst(ilst);
    m4a.save_to_path(&*chemin, WriteOptions::default()).unwrap();
    let meta = read_metadata(&chemin).expect("métadonnées du M4A");
    assert_eq!(meta.explicit, Some(true), "atome rtng=1 non lu");
}
