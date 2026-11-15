//! #8325 — l'œuvre et le mouvement lus au scan, dans de VRAIS conteneurs
//! (FLAC, MP3, M4A) copiés des fixtures et balisés par lofty, relus par la
//! lecture du scan (`read_extended_metadata`).

use std::path::Path;

use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;

fn copie(fixture: &str, epreuve: &str) -> crate::test_scratch::ScratchFile {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    let ext = Path::new(fixture).extension().unwrap().to_str().unwrap();
    let cible =
        crate::test_scratch::scratch_file(&format!("oeuvre8325-{epreuve}"), &format!(".{ext}"));
    std::fs::copy(&source, &cible).expect("copie de la fixture");
    cible
}

#[test]
fn un_flac_rend_son_oeuvre_et_son_mouvement_8325() {
    use lofty::flac::FlacFile;
    use lofty::ogg::VorbisComments;
    let chemin = copie("test.flac", "flac");
    let mut fh = std::fs::File::open(&*chemin).unwrap();
    let mut flac = FlacFile::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    if flac.vorbis_comments().is_none() {
        flac.set_vorbis_comments(VorbisComments::default());
    }
    let vc = flac.vorbis_comments_mut().unwrap();
    vc.push("WORK".into(), "Symphonie n° 9 en ré mineur, op. 125".into());
    vc.push("MOVEMENTNAME".into(), "IV. Presto".into());
    flac.save_to_path(&*chemin, WriteOptions::default())
        .unwrap();

    let meta = super::read_extended_metadata(&chemin);
    assert_eq!(
        meta.get("work").map(String::as_str),
        Some("Symphonie n° 9 en ré mineur, op. 125"),
        "#8325 — WORK (Vorbis) non lu au scan : {meta:?}"
    );
    assert_eq!(
        meta.get("movement").map(String::as_str),
        Some("IV. Presto"),
        "#8325 — MOVEMENTNAME (Vorbis) non lu au scan : {meta:?}"
    );
}

#[test]
fn un_mp3_rend_son_oeuvre_et_son_mouvement_8325() {
    use lofty::TextEncoding;
    use lofty::id3::v2::{ExtendedTextFrame, Frame, FrameId, Id3v2Tag, TextInformationFrame};
    use lofty::mpeg::MpegFile;
    use std::borrow::Cow;
    let chemin = copie("test.mp3", "mp3");
    let mut fh = std::fs::File::open(&*chemin).unwrap();
    let mut mp3 = MpegFile::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    let mut tag = mp3.id3v2().cloned().unwrap_or_else(Id3v2Tag::new);
    tag.insert(Frame::UserText(ExtendedTextFrame::new(
        TextEncoding::UTF8,
        "WORK",
        "Messe en si mineur, BWV 232",
    )));
    tag.insert(Frame::Text(TextInformationFrame::new(
        FrameId::Valid(Cow::Borrowed("MVNM")),
        TextEncoding::UTF8,
        "Kyrie eleison".to_string(),
    )));
    mp3.set_id3v2(tag);
    mp3.save_to_path(&*chemin, WriteOptions::default()).unwrap();

    let meta = super::read_extended_metadata(&chemin);
    assert_eq!(
        meta.get("work").map(String::as_str),
        Some("Messe en si mineur, BWV 232"),
        "#8325 — TXXX:WORK (ID3v2) non lu au scan : {meta:?}"
    );
    assert_eq!(
        meta.get("movement").map(String::as_str),
        Some("Kyrie eleison"),
        "#8325 — MVNM (ID3v2) non lu au scan : {meta:?}"
    );
}

#[test]
fn un_m4a_rend_son_oeuvre_et_son_mouvement_8325() {
    use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst, Mp4File};
    let chemin = copie("test.m4a", "m4a");
    let mut fh = std::fs::File::open(&*chemin).unwrap();
    let mut m4a = Mp4File::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    let mut ilst = m4a.ilst().cloned().unwrap_or_else(Ilst::default);
    let code = |c: [u8; 4], v: &str| Atom::new(AtomIdent::Fourcc(c), AtomData::UTF8(v.to_string()));
    ilst.insert(code(*b"\xa9wrk", "Les Quatre Saisons"));
    ilst.insert(code(*b"\xa9mvn", "L'Hiver : I. Allegro non molto"));
    m4a.set_ilst(ilst);
    m4a.save_to_path(&*chemin, WriteOptions::default()).unwrap();

    let meta = super::read_extended_metadata(&chemin);
    assert_eq!(
        meta.get("work").map(String::as_str),
        Some("Les Quatre Saisons"),
        "#8325 — ©wrk (MP4) non lu au scan : {meta:?}"
    );
    assert_eq!(
        meta.get("movement").map(String::as_str),
        Some("L'Hiver : I. Allegro non molto"),
        "#8325 — ©mvn (MP4) non lu au scan : {meta:?}"
    );
}

/// iTunes 12.5+ : l'œuvre en TIT1, le regroupement en GRP1. Avec GRP1, TIT1
/// est l'œuvre ; sans GRP1, TIT1 n'est qu'un regroupement.
fn mp3_tit1(epreuve: &str, grp1: Option<&str>) -> std::collections::HashMap<String, String> {
    use lofty::TextEncoding;
    use lofty::id3::v2::{Frame, FrameId, Id3v2Tag, TextInformationFrame};
    use lofty::mpeg::MpegFile;
    use std::borrow::Cow;
    let chemin = copie("test.mp3", epreuve);
    let mut fh = std::fs::File::open(&*chemin).unwrap();
    let mut mp3 = MpegFile::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    let mut tag = mp3.id3v2().cloned().unwrap_or_else(Id3v2Tag::new);
    let texte = |id: &'static str, v: &str| {
        Frame::Text(TextInformationFrame::new(
            FrameId::Valid(Cow::Borrowed(id)),
            TextEncoding::UTF8,
            v.to_string(),
        ))
    };
    tag.insert(texte("TIT1", "Le Sacre du printemps"));
    if let Some(g) = grp1 {
        tag.insert(texte("GRP1", g));
    }
    mp3.set_id3v2(tag);
    mp3.save_to_path(&*chemin, WriteOptions::default()).unwrap();
    super::read_extended_metadata(&chemin)
}

#[test]
fn un_mp3_itunes_rend_son_oeuvre_en_tit1_quand_grp1_est_la_8325() {
    let meta = mp3_tit1("mp3-tit1-grp1", Some("Ballets russes"));
    assert_eq!(
        meta.get("work").map(String::as_str),
        Some("Le Sacre du printemps"),
        "#8325 — TIT1 + GRP1 (iTunes 12.5+) : TIT1 est l'œuvre : {meta:?}"
    );
    let meta = mp3_tit1("mp3-tit1-seul", None);
    assert_eq!(
        meta.get("work"),
        None,
        "#8325 — TIT1 sans GRP1 n'est qu'un regroupement : {meta:?}"
    );
}

/// Un FLAC balisé des paires Vorbis données, relu par le scan.
fn flac(epreuve: &str, paires: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
    use lofty::flac::FlacFile;
    use lofty::ogg::VorbisComments;
    let chemin = copie("test.flac", epreuve);
    let mut fh = std::fs::File::open(&*chemin).unwrap();
    let mut f = FlacFile::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    let mut vc = VorbisComments::default();
    for (k, v) in paires {
        vc.push((*k).into(), (*v).into());
    }
    f.set_vorbis_comments(vc);
    f.save_to_path(&*chemin, WriteOptions::default()).unwrap();
    super::read_extended_metadata(&chemin)
}

/// Les trois schémas classiques du testeur HCFR (ESL63P). Chez lui, %band%
/// est TOUJOURS l'orchestre ou l'ensemble, et l'artiste d'album vaut souvent
/// le même nom : en contexte classique (compositeur ET chef ou œuvre), BAND
/// compte comme ensemble même s'il égale l'artiste d'album.
#[test]
fn les_schemas_classiques_du_testeur_hcfr_8325() {
    // « %composer% / %work% - %artist% » : un soliste, pas d'ensemble.
    let m = flac(
        "hcfr-1",
        &[
            ("COMPOSER", "Johann Sebastian Bach"),
            ("WORK", "Variations Goldberg, BWV 988"),
            ("ARTIST", "Glenn Gould"),
            ("ALBUMARTIST", "Glenn Gould"),
        ],
    );
    assert_eq!(
        m.get("work").map(String::as_str),
        Some("Variations Goldberg, BWV 988")
    );
    assert_eq!(m.get("ensemble"), None, "schéma 1 : {m:?}");

    // « %composer% / %work% - %conductor% - %band% », BAND = artiste d'album.
    let m = flac(
        "hcfr-2",
        &[
            ("COMPOSER", "Ludwig van Beethoven"),
            ("WORK", "Symphonie n° 9"),
            ("CONDUCTOR", "Herbert von Karajan"),
            ("BAND", "Berliner Philharmoniker"),
            ("ALBUMARTIST", "Berliner Philharmoniker"),
            ("ARTIST", "Berliner Philharmoniker"),
        ],
    );
    assert_eq!(
        m.get("ensemble").map(String::as_str),
        Some("Berliner Philharmoniker"),
        "schéma 2 : {m:?}"
    );

    // « %composer% / %album% - %conductor% - %band% » : pas de WORK, le chef
    // suffit au contexte classique.
    let m = flac(
        "hcfr-3",
        &[
            ("COMPOSER", "Gustav Mahler"),
            ("ALBUM", "Symphonie n° 2"),
            ("CONDUCTOR", "Claudio Abbado"),
            ("BAND", "Lucerne Festival Orchestra"),
            ("ALBUMARTIST", "Lucerne Festival Orchestra"),
            ("CHOIR", "Orfeón Donostiarra"),
        ],
    );
    assert_eq!(
        m.get("ensemble").map(String::as_str),
        Some("Orfeón Donostiarra; Lucerne Festival Orchestra"),
        "schéma 3, chœur compris : {m:?}"
    );

    // « %artist% - %album% » hors classique : BAND = artiste d'album reste
    // l'artiste, pas un ensemble.
    let m = flac(
        "hcfr-pop",
        &[
            ("ARTIST", "Pink Floyd"),
            ("ALBUMARTIST", "Pink Floyd"),
            ("BAND", "Pink Floyd"),
        ],
    );
    assert_eq!(m.get("ensemble"), None, "hors classique : {m:?}");
}

/// CHOIR, le chœur : Vorbis, `TXXX:CHOIR` (MP3), `----:com.apple.iTunes:CHOIR`
/// (M4A), rangé avec l'ensemble.
#[test]
fn le_choeur_est_un_ensemble_dans_les_trois_conteneurs_8325() {
    let m = flac("choir-flac", &[("CHOIR", "Monteverdi Choir")]);
    assert_eq!(
        m.get("ensemble").map(String::as_str),
        Some("Monteverdi Choir")
    );

    {
        use lofty::TextEncoding;
        use lofty::id3::v2::{ExtendedTextFrame, Frame, Id3v2Tag};
        use lofty::mpeg::MpegFile;
        let chemin = copie("test.mp3", "choir-mp3");
        let mut fh = std::fs::File::open(&*chemin).unwrap();
        let mut mp3 = MpegFile::read_from(&mut fh, ParseOptions::new()).unwrap();
        drop(fh);
        let mut tag = mp3.id3v2().cloned().unwrap_or_else(Id3v2Tag::new);
        tag.insert(Frame::UserText(ExtendedTextFrame::new(
            TextEncoding::UTF8,
            "CHOIR",
            "Monteverdi Choir",
        )));
        mp3.set_id3v2(tag);
        mp3.save_to_path(&*chemin, WriteOptions::default()).unwrap();
        let m = super::read_extended_metadata(&chemin);
        assert_eq!(
            m.get("ensemble").map(String::as_str),
            Some("Monteverdi Choir"),
            "TXXX:CHOIR : {m:?}"
        );
    }
    {
        use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst, Mp4File};
        use std::borrow::Cow;
        let chemin = copie("test.m4a", "choir-m4a");
        let mut fh = std::fs::File::open(&*chemin).unwrap();
        let mut m4a = Mp4File::read_from(&mut fh, ParseOptions::new()).unwrap();
        drop(fh);
        let mut ilst = m4a.ilst().cloned().unwrap_or_else(Ilst::default);
        ilst.insert(Atom::new(
            AtomIdent::Freeform {
                mean: Cow::Borrowed("com.apple.iTunes"),
                name: Cow::Borrowed("CHOIR"),
            },
            AtomData::UTF8("Monteverdi Choir".to_string()),
        ));
        m4a.set_ilst(ilst);
        m4a.save_to_path(&*chemin, WriteOptions::default()).unwrap();
        let m = super::read_extended_metadata(&chemin);
        assert_eq!(
            m.get("ensemble").map(String::as_str),
            Some("Monteverdi Choir"),
            "----:CHOIR : {m:?}"
        );
    }
}
