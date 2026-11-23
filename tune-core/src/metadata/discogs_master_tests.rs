//! Fil 3012 — le master Discogs : sa forme rangée, puis de VRAIS fichiers
//! (FLAC, MP3) balisés par lofty et relus par la lecture du scan.

use super::*;
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use std::path::Path;

fn p(paires: &[(&str, &str)]) -> Vec<(String, String)> {
    paires
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn les_formes_d_un_master_se_rangent_en_chiffres() {
    for brut in [
        "12345",
        " 12345 ",
        "m12345",
        "M12345",
        "[m12345]",
        "012345",
        "https://www.discogs.com/master/12345-Led-Zeppelin-Physical-Graffiti",
        "https://www.discogs.com/fr/master/12345",
        "discogs.com/master/12345?x=1",
    ] {
        assert_eq!(normaliser(brut).as_deref(), Some("12345"), "{brut:?}");
    }
}

/// Contre-épreuve : ce qui ne dit pas un master n'est pas rangé — surtout une
/// RELEASE, qui grouperait au hasard.
#[test]
fn une_release_ou_un_texte_n_est_pas_un_master() {
    for brut in [
        "",
        "  ",
        "0",
        "m",
        "Physical Graffiti",
        "12a45",
        "r12345",
        "https://www.discogs.com/release/12345-Led-Zeppelin-Physical-Graffiti",
        "https://www.discogs.com/master/",
        "12345678901",
    ] {
        assert_eq!(normaliser(brut), None, "{brut:?}");
    }
}

#[test]
fn les_deux_graphies_de_balise_sont_lues_la_premiere_prime() {
    assert_eq!(
        parmi(&p(&[("TXXX:DISCOGS_MASTER_ID", "m42")])).as_deref(),
        Some("42")
    );
    assert_eq!(
        parmi(&p(&[(
            "----:com.apple.iTunes:DISCOGS_MASTER_RELEASE_ID",
            "43"
        )]))
        .as_deref(),
        Some("43")
    );
    assert_eq!(
        parmi(&p(&[
            ("DISCOGS_MASTER_RELEASE_ID", "43"),
            ("discogs_master_id", "42")
        ]))
        .as_deref(),
        Some("42")
    );
    // Contre-épreuve : l'identifiant de RELEASE n'est pas lu comme master.
    assert_eq!(parmi(&p(&[("DISCOGS_RELEASE_ID", "99")])), None);
    assert_eq!(parmi(&p(&[("DISCOGS_MASTER_ID", "n/a")])), None);
}

fn copie(fixture: &str, epreuve: &str) -> crate::test_scratch::ScratchFile {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    let ext = Path::new(fixture).extension().unwrap().to_str().unwrap();
    let cible =
        crate::test_scratch::scratch_file(&format!("discogs3012-{epreuve}"), &format!(".{ext}"));
    std::fs::copy(&source, &cible).expect("copie de la fixture");
    cible
}

fn flac_balise(epreuve: &str, paires: &[(&str, &str)]) -> crate::test_scratch::ScratchFile {
    use lofty::flac::FlacFile;
    use lofty::ogg::VorbisComments;
    let chemin = copie("test.flac", epreuve);
    let mut fh = std::fs::File::open(&*chemin).unwrap();
    let mut flac = FlacFile::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    if flac.vorbis_comments().is_none() {
        flac.set_vorbis_comments(VorbisComments::default());
    }
    let vc = flac.vorbis_comments_mut().unwrap();
    for (k, v) in paires {
        vc.push(k.to_string(), v.to_string());
    }
    flac.save_to_path(&*chemin, WriteOptions::default())
        .unwrap();
    chemin
}

#[test]
fn un_flac_rend_son_master_discogs_au_scan_3012() {
    let chemin = flac_balise(
        "flac",
        &[
            ("TITLE", "Custard Pie"),
            ("ARTIST", "Led Zeppelin"),
            ("DISCOGS_MASTER_ID", "m12345"),
        ],
    );
    let meta = super::super::read_extended_metadata(&chemin);
    assert_eq!(
        meta.get(CLE).map(String::as_str),
        Some("12345"),
        "DISCOGS_MASTER_ID non rangée en `{CLE}` : {meta:?}"
    );
}

/// Contre-épreuve : le même FLAC sans la balise ne rend aucun master.
#[test]
fn un_flac_sans_balise_ne_rend_aucun_master_3012() {
    let chemin = flac_balise(
        "flac-nu",
        &[("TITLE", "Custard Pie"), ("ARTIST", "Led Zeppelin")],
    );
    let meta = super::super::read_extended_metadata(&chemin);
    assert_eq!(meta.get(CLE), None, "{meta:?}");
}

#[test]
fn un_mp3_rend_son_txxx_discogs_master_3012() {
    use lofty::id3::v2::Id3v2Tag;
    use lofty::mpeg::MpegFile;
    let chemin = copie("test.mp3", "mp3");
    let mut fh = std::fs::File::open(&*chemin).unwrap();
    let mut mp3 = MpegFile::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    let mut tag = mp3.id3v2().cloned().unwrap_or_else(Id3v2Tag::new);
    tag.insert_user_text("DISCOGS_MASTER_ID".into(), "12345".into());
    mp3.set_id3v2(tag);
    mp3.save_to_path(&*chemin, WriteOptions::default()).unwrap();
    let meta = super::super::read_extended_metadata(&chemin);
    assert_eq!(meta.get(CLE).map(String::as_str), Some("12345"), "{meta:?}");
}
