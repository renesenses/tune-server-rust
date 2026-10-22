//! #6124 — témoins de `balises_brutes` : les dérivations (date originale,
//! version) sur des paires, puis de VRAIS conteneurs (FLAC, MP3, M4A) copiés
//! des fixtures et balisés par lofty, relus par les fonctions de production.

use super::*;
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;

fn p(paires: &[(&str, &str)]) -> Vec<(String, String)> {
    paires
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn le_nom_de_cle_retire_le_prefixe_du_conteneur() {
    assert_eq!(nom_de_cle("TXXX:originalyear"), "ORIGINALYEAR");
    assert_eq!(nom_de_cle("----:com.apple.iTunes:VERSION"), "VERSION");
    assert_eq!(nom_de_cle("Version"), "VERSION");
    assert_eq!(nom_de_cle("TDOR"), "TDOR");
    assert_eq!(nom_de_cle("originalreleasedate"), "ORIGINALRELEASEDATE");
}

#[test]
fn le_jour_original_gagne_sur_l_annee_quel_que_soit_l_ordre() {
    let picard = p(&[("ORIGINALYEAR", "1971"), ("ORIGINALDATE", "1971-03-05")]);
    assert_eq!(date_originale_parmi(&picard).as_deref(), Some("1971-03-05"));
    assert_eq!(
        date_originale_parmi(&p(&[("ORIGINALRELEASEDATE", " 1959-08-17 ")])).as_deref(),
        Some("1959-08-17")
    );
    assert_eq!(
        date_originale_parmi(&p(&[("TXXX:originalyear", "1965")])).as_deref(),
        Some("1965")
    );
    assert_eq!(
        date_originale_parmi(&p(&[("----:com.apple.iTunes:ORIGINALYEAR", "1977")])).as_deref(),
        Some("1977")
    );
    assert_eq!(
        date_originale_parmi(&p(&[("TDOR", "1969-10-10"), ("TORY", "1969")])).as_deref(),
        Some("1969-10-10")
    );
}

#[test]
fn une_date_sans_annee_n_est_pas_une_date_originale() {
    for brut in ["", "inconnue", "0000", "19", "mars 1971"] {
        assert_eq!(
            date_originale_parmi(&p(&[("ORIGINALDATE", brut)])),
            None,
            "{brut:?}"
        );
    }
    // Une date invalide n'empêche pas une année valide d'une autre clé.
    assert_eq!(
        date_originale_parmi(&p(&[("ORIGINALDATE", "?"), ("ORIGINALYEAR", "1984")])).as_deref(),
        Some("1984")
    );
    // Une autre date n'est pas la date ORIGINALE.
    assert_eq!(date_originale_parmi(&p(&[("DATE", "2009")])), None);
}

#[test]
fn la_version_se_lit_dans_tous_les_conteneurs() {
    for cle in [
        "VERSION",
        "TXXX:VERSION",
        "----:com.apple.iTunes:VERSION",
        "Version",
    ] {
        assert_eq!(
            version_parmi(&p(&[("TITLE", "x"), (cle, " Remaster 2009 ")])).as_deref(),
            Some("Remaster 2009"),
            "{cle}"
        );
    }
    assert_eq!(version_parmi(&p(&[("VERSION", "  ")])), None);
    assert_eq!(version_parmi(&p(&[("ENCODER_VERSION", "1.4")])), None);
    assert_eq!(
        version_parmi(&p(&[("VERSION", "Live\u{0}1975")])).as_deref(),
        Some("Live 1975"),
        "un caractère de contrôle devient une espace"
    );
    let longue = "x".repeat(PLAFOND_VERSION + 50);
    assert_eq!(
        version_parmi(&p(&[("VERSION", &longue)])).map(|v| v.chars().count()),
        Some(PLAFOND_VERSION)
    );
}

// ---------------------------------------------------------------------------
// Vrais conteneurs.
// ---------------------------------------------------------------------------

fn copie(fixture: &str, epreuve: &str) -> crate::test_scratch::ScratchFile {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    let ext = Path::new(fixture).extension().unwrap().to_str().unwrap();
    let cible =
        crate::test_scratch::scratch_file(&format!("balises6124-{epreuve}"), &format!(".{ext}"));
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

/// Le cas du forum : `ORIGINALRELEASEDATE` et `VERSION` dans un FLAC. lofty
/// écarte les deux ; le scan doit les ranger.
#[test]
fn un_flac_rend_sa_date_originale_et_sa_version_au_scan_6124() {
    let chemin = flac_balise(
        "flac",
        &[
            // Une balise qui dit quelque chose : sans titre ni artiste, la
            // lecture principale prend le repli « sans balise » (#4412).
            ("TITLE", "So What"),
            ("ARTIST", "Miles Davis"),
            ("ORIGINALRELEASEDATE", "1959-08-17"),
            ("VERSION", "Remaster 2009"),
        ],
    );
    let meta = super::super::read_extended_metadata(&chemin);
    assert_eq!(
        meta.get("original_date").map(String::as_str),
        Some("1959-08-17"),
        "#6124 — ORIGINALRELEASEDATE n'est pas rangée en `original_date` : {meta:?}"
    );
    assert_eq!(
        meta.get("version").map(String::as_str),
        Some("Remaster 2009"),
        "#6124 — VERSION n'est pas rangée en `version` : {meta:?}"
    );

    // La lecture de l'album (date et année originales) la voit aussi.
    let m = super::super::try_read_metadata(&chemin).expect("lecture FLAC");
    assert_eq!(m.original_date.as_deref(), Some("1959-08-17"));
    assert_eq!(m.original_year, Some(1959));

    // Et la liste brute la montre sous sa clé d'origine.
    let b = lire_balises_du_fichier(&chemin).expect("balises du FLAC");
    assert_eq!(b.format.as_deref(), Some("FLAC"));
    for (cle, valeur) in [
        ("ORIGINALRELEASEDATE", "1959-08-17"),
        ("VERSION", "Remaster 2009"),
    ] {
        assert!(
            b.balises
                .iter()
                .any(|x| x.bloc == "vorbis" && x.cle == cle && x.valeur == valeur),
            "{cle} absente de la liste brute : {:?}",
            b.balises
        );
    }
    assert!(!b.liste_tronquee);
}

#[test]
fn le_jour_d_origine_d_un_flac_picard_gagne_sur_son_annee_6124() {
    let chemin = flac_balise(
        "picard",
        &[("ORIGINALYEAR", "1971"), ("ORIGINALDATE", "1971-03-05")],
    );
    let meta = super::super::read_extended_metadata(&chemin);
    assert_eq!(
        meta.get("original_date").map(String::as_str),
        Some("1971-03-05"),
        "{meta:?}"
    );
}

#[test]
fn un_mp3_rend_ses_txxx_6124() {
    use lofty::id3::v2::Id3v2Tag;
    use lofty::mpeg::MpegFile;
    let chemin = copie("test.mp3", "mp3");
    let mut fh = std::fs::File::open(&*chemin).unwrap();
    let mut mp3 = MpegFile::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    let mut tag = mp3.id3v2().cloned().unwrap_or_else(Id3v2Tag::new);
    tag.insert_user_text("ORIGINALYEAR".into(), "1965".into());
    tag.insert_user_text("VERSION".into(), "Mono".into());
    mp3.set_id3v2(tag);
    mp3.save_to_path(&*chemin, WriteOptions::default()).unwrap();

    let meta = super::super::read_extended_metadata(&chemin);
    assert_eq!(
        meta.get("original_date").map(String::as_str),
        Some("1965"),
        "#6124 — TXXX:ORIGINALYEAR non lue : {meta:?}"
    );
    assert_eq!(
        meta.get("version").map(String::as_str),
        Some("Mono"),
        "#6124 — TXXX:VERSION non lue : {meta:?}"
    );
    let b = lire_balises_du_fichier(&chemin).expect("balises du MP3");
    assert!(
        b.balises
            .iter()
            .any(|x| x.bloc == "id3v2" && x.cle == "TXXX:VERSION" && x.valeur == "Mono"),
        "{:?}",
        b.balises
    );
}

#[test]
fn un_m4a_rend_ses_atomes_libres_6124() {
    use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst, Mp4File};
    use std::borrow::Cow;
    let chemin = copie("test.m4a", "m4a");
    let mut fh = std::fs::File::open(&*chemin).unwrap();
    let mut m4a = Mp4File::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    let mut ilst = m4a.ilst().cloned().unwrap_or_else(Ilst::default);
    let libre = |nom: &'static str, v: &str| {
        Atom::new(
            AtomIdent::Freeform {
                mean: Cow::Borrowed("com.apple.iTunes"),
                name: Cow::Borrowed(nom),
            },
            AtomData::UTF8(v.to_string()),
        )
    };
    ilst.insert(libre("ORIGINALYEAR", "1977"));
    ilst.insert(libre("VERSION", "Deluxe"));
    m4a.set_ilst(ilst);
    m4a.save_to_path(&*chemin, WriteOptions::default()).unwrap();

    let meta = super::super::read_extended_metadata(&chemin);
    assert_eq!(
        meta.get("original_date").map(String::as_str),
        Some("1977"),
        "{meta:?}"
    );
    assert_eq!(
        meta.get("version").map(String::as_str),
        Some("Deluxe"),
        "{meta:?}"
    );
    let b = lire_balises_du_fichier(&chemin).expect("balises du M4A");
    assert!(
        b.balises
            .iter()
            .any(|x| x.bloc == "mp4" && x.cle == "----:com.apple.iTunes:VERSION"),
        "{:?}",
        b.balises
    );
}

#[test]
fn la_liste_brute_est_plafonnee_6124() {
    let longue = "a".repeat(PLAFOND_VALEUR + 100);
    let mut paires: Vec<(String, String)> = vec![("LYRICS".into(), longue)];
    for i in 0..PLAFOND_BALISES + 20 {
        paires.push((format!("X{i}"), "v".into()));
    }
    let refs: Vec<(&str, &str)> = paires
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let chemin = flac_balise("plafond", &refs);
    let b = lire_balises_du_fichier(&chemin).expect("balises");
    assert_eq!(b.balises.len(), PLAFOND_BALISES);
    assert!(b.liste_tronquee, "la coupe de la liste doit se dire");
    let paroles = b
        .balises
        .iter()
        .find(|x| x.cle == "LYRICS")
        .expect("LYRICS");
    assert_eq!(paroles.valeur.chars().count(), PLAFOND_VALEUR);
    assert!(paroles.tronquee, "la coupe d'une valeur doit se dire");
}

#[test]
fn un_fichier_absent_est_une_erreur_pas_une_liste_vide() {
    assert!(lire_balises_du_fichier(Path::new("/nulle/part/absent.flac")).is_err());
}
