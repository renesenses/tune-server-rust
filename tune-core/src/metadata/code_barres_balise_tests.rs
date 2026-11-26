//! [rc14] Le code-barres lu dans de VRAIS fichiers étiquetés, par le chemin du
//! scan (`read_metadata`). Idées signalées par Xavier Joly (testeur).

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

fn code_lu(chemin: &std::path::Path) -> Option<String> {
    read_metadata(chemin)
        .expect("le fichier étiqueté doit se lire")
        .barcode
}

fn flac(champs: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
    let (d, f) = copie("test.flac", "a.flac");
    let mut vc = lofty::ogg::VorbisComments::new();
    vc.push("TITLE".into(), "Piste".into());
    for (k, v) in champs {
        vc.push((*k).into(), (*v).into());
    }
    vc.save_to_path(&f, WriteOptions::default()).unwrap();
    (d, f)
}

#[test]
fn code_barres_valide_longueurs_reconnues() {
    assert_eq!(code_barres_valide("40085423").as_deref(), Some("40085423"));
    assert_eq!(
        code_barres_valide("602547924865").as_deref(),
        Some("602547924865")
    );
    assert_eq!(
        code_barres_valide("0028947758419").as_deref(),
        Some("0028947758419")
    );
    assert_eq!(
        code_barres_valide("10028947758416").as_deref(),
        Some("10028947758416")
    );
    // Espaces et tirets de saisie retirés.
    assert_eq!(
        code_barres_valide(" 0 028947-75841 9 ").as_deref(),
        Some("0028947758419")
    );
}

#[test]
fn code_barres_valide_refuse_le_reste() {
    for brut in [
        "",
        "1234567",
        "123456789",
        "1234567890",
        "12345678901",
        "123456789012345",
        "ABC123456789",
        "0028947758419X",
        "00289477.58419",
        "００２８９４７７５８４１９",
    ] {
        assert_eq!(
            code_barres_valide(brut),
            None,
            "« {brut} » n'est pas un code-barres"
        );
    }
}

#[test]
fn flac_barcode() {
    let (_d, f) = flac(&[("BARCODE", "0028947758419")]);
    assert_eq!(code_lu(&f).as_deref(), Some("0028947758419"));
}

#[test]
fn flac_upc_et_ean() {
    let (_d, f) = flac(&[("UPC", "602547924865")]);
    assert_eq!(code_lu(&f).as_deref(), Some("602547924865"));
    let (_d, f) = flac(&[("EAN", "40085423")]);
    assert_eq!(code_lu(&f).as_deref(), Some("40085423"));
}

#[test]
fn flac_barcode_invalide_puis_upc_valide() {
    // Un BARCODE illisible ne masque pas un UPC valide du même fichier.
    let (_d, f) = flac(&[("BARCODE", "n/a"), ("UPC", "602547924865")]);
    assert_eq!(code_lu(&f).as_deref(), Some("602547924865"));
}

#[test]
fn flac_sans_code_ou_invalide() {
    let (_d, f) = flac(&[]);
    assert_eq!(code_lu(&f), None);
    let (_d, f) = flac(&[("BARCODE", "12345")]);
    assert_eq!(code_lu(&f), None);
}

#[test]
fn mp3_txxx_barcode() {
    use lofty::TextEncoding;
    use lofty::id3::v2::{ExtendedTextFrame, Frame, Id3v2Tag};
    use lofty::tag::Accessor;
    let (_d, f) = copie("test.mp3", "a.mp3");
    let mut tag = Id3v2Tag::new();
    tag.set_title("Piste".into());
    tag.insert(Frame::UserText(ExtendedTextFrame::new(
        TextEncoding::UTF8,
        "BARCODE".to_string(),
        "0028947758419".to_string(),
    )));
    tag.save_to_path(&f, WriteOptions::default()).unwrap();
    assert_eq!(code_lu(&f).as_deref(), Some("0028947758419"));
}

#[test]
fn m4a_atome_itunes_barcode() {
    use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst};
    use lofty::tag::Accessor;
    use std::borrow::Cow;
    let (_d, f) = copie("test.m4a", "a.m4a");
    let mut ilst = Ilst::new();
    ilst.set_title("Piste".into());
    ilst.insert(Atom::new(
        AtomIdent::Freeform {
            mean: Cow::Borrowed("com.apple.iTunes"),
            name: Cow::Borrowed("BARCODE"),
        },
        AtomData::UTF8("0028947758419".into()),
    ));
    ilst.save_to_path(&f, WriteOptions::default()).unwrap();
    assert_eq!(code_lu(&f).as_deref(), Some("0028947758419"));
}
