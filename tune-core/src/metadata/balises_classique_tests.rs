//! Réglage « Balises du classique » (#8325, ESL63P) : les défauts lisent
//! exactement ce qu'on lisait avant, une balise libre (`TXXX:CHORUS`) est
//! prise, BAND peut être ignoré. Sur de VRAIS conteneurs copiés des fixtures.
//!
//! ⚠️ Aucun témoin ne pose un réglage NON défaut dans le miroir global
//! (`ecrire`, `charger`) : les autres témoins du binaire lisent par
//! `read_extended_metadata`, qui le consulte. Tout passe par
//! `read_extended_metadata_selon`.

use std::collections::HashMap;
use std::path::Path;

use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;

use super::*;
use crate::metadata::read_extended_metadata_selon;

fn copie(fixture: &str, epreuve: &str) -> crate::test_scratch::ScratchFile {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    let ext = Path::new(fixture).extension().unwrap().to_str().unwrap();
    let cible = crate::test_scratch::scratch_file(
        &format!("balises-classique-{epreuve}"),
        &format!(".{ext}"),
    );
    std::fs::copy(&source, &cible).expect("copie de la fixture");
    cible
}

fn flac(epreuve: &str, paires: &[(&str, &str)]) -> crate::test_scratch::ScratchFile {
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
    chemin
}

/// Un MP3 aux trames TXXX données, plus TPE3 (chef) si `chef`.
fn mp3(
    epreuve: &str,
    txxx: &[(&str, &str)],
    chef: Option<&str>,
) -> crate::test_scratch::ScratchFile {
    use lofty::TextEncoding;
    use lofty::id3::v2::{ExtendedTextFrame, Frame, FrameId, Id3v2Tag, TextInformationFrame};
    use lofty::mpeg::MpegFile;
    use std::borrow::Cow;
    let chemin = copie("test.mp3", epreuve);
    let mut fh = std::fs::File::open(&*chemin).unwrap();
    let mut f = MpegFile::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    let mut tag = f.id3v2().cloned().unwrap_or_else(Id3v2Tag::new);
    for (k, v) in txxx {
        tag.insert(Frame::UserText(ExtendedTextFrame::new(
            TextEncoding::UTF8,
            (*k).to_string(),
            (*v).to_string(),
        )));
    }
    if let Some(c) = chef {
        tag.insert(Frame::Text(TextInformationFrame::new(
            FrameId::Valid(Cow::Borrowed("TPE3")),
            TextEncoding::UTF8,
            c.to_string(),
        )));
    }
    f.set_id3v2(tag);
    f.save_to_path(&*chemin, WriteOptions::default()).unwrap();
    chemin
}

fn champs(m: &HashMap<String, String>) -> [Option<&str>; 4] {
    ["conductor", "ensemble", "work", "movement"].map(|k| m.get(k).map(String::as_str))
}

fn reglage(json: serde_json::Value) -> BalisesClassique {
    serde_json::from_value::<Saisie>(json)
        .unwrap()
        .valider()
        .unwrap()
}

// ---------------------------------------------------------------------------
// Les défauts = le comportement d'avant.
// ---------------------------------------------------------------------------

#[test]
fn les_defauts_reprennent_les_listes_en_dur() {
    let d = BalisesClassique::default();
    assert_eq!(d.conductor, vec!["CONDUCTOR"]);
    assert_eq!(d.ensemble, vec!["ENSEMBLE", "ORCHESTRA", "CHOIR"]);
    assert_eq!(d.work, vec!["WORK"]);
    assert_eq!(d.movement, vec!["MOVEMENTNAME"]);
    assert_eq!(d.band, ModeBand::Classical);
    assert_eq!(
        serde_json::to_value(&d).unwrap(),
        serde_json::json!({
            "conductor": ["CONDUCTOR"],
            "ensemble": ["ENSEMBLE", "ORCHESTRA", "CHOIR"],
            "work": ["WORK"],
            "movement": ["MOVEMENTNAME"],
            "band": "classical",
        })
    );
    // Un corps vide = les défauts ; le miroir part sur les défauts.
    assert_eq!(reglage(serde_json::json!({})), d);
    assert_eq!(*courante(), d);
}

/// Sur les schémas du testeur HCFR et les graphies de conteneur, la lecture
/// selon les défauts rend EXACTEMENT ce que rendait la lecture d'avant (les
/// valeurs attendues sont celles des témoins #8325 / #7766, inchangés).
#[test]
fn les_defauts_lisent_exactement_comme_avant() {
    let d = BalisesClassique::default();
    let cas: Vec<(crate::test_scratch::ScratchFile, [Option<&str>; 4])> = vec![
        (
            flac(
                "def-hcfr2",
                &[
                    ("COMPOSER", "Ludwig van Beethoven"),
                    ("WORK", "Symphonie n° 9"),
                    ("MOVEMENTNAME", "IV. Presto"),
                    ("CONDUCTOR", "Herbert von Karajan"),
                    ("BAND", "Berliner Philharmoniker"),
                    ("ALBUMARTIST", "Berliner Philharmoniker"),
                    ("ARTIST", "Berliner Philharmoniker"),
                ],
            ),
            [
                Some("Herbert von Karajan"),
                Some("Berliner Philharmoniker"),
                Some("Symphonie n° 9"),
                Some("IV. Presto"),
            ],
        ),
        (
            flac(
                "def-hcfr3",
                &[
                    ("COMPOSER", "Gustav Mahler"),
                    ("CONDUCTOR", "Claudio Abbado"),
                    ("BAND", "Lucerne Festival Orchestra"),
                    ("ALBUMARTIST", "Lucerne Festival Orchestra"),
                    ("CHOIR", "Orfeón Donostiarra"),
                ],
            ),
            [
                Some("Claudio Abbado"),
                Some("Orfeón Donostiarra; Lucerne Festival Orchestra"),
                None,
                None,
            ],
        ),
        (
            flac(
                "def-pop",
                &[
                    ("ARTIST", "Pink Floyd"),
                    ("ALBUMARTIST", "Pink Floyd"),
                    ("BAND", "Pink Floyd"),
                    ("CHORUS", "Pas lu par défaut"),
                ],
            ),
            [None, None, None, None],
        ),
        (
            mp3(
                "def-mp3",
                &[
                    ("WORK", "Messe en si mineur"),
                    ("ORCHESTRA", "English Baroque Soloists"),
                    ("CHORUS", "Pas lu"),
                ],
                Some("John Eliot Gardiner"),
            ),
            [
                Some("John Eliot Gardiner"),
                Some("English Baroque Soloists"),
                Some("Messe en si mineur"),
                None,
            ],
        ),
    ];
    for (chemin, attendu) in &cas {
        let selon = read_extended_metadata_selon(chemin, &d);
        assert_eq!(champs(&selon), *attendu, "{selon:?}");
        // Et la lecture du scan (miroir global, aux défauts) est la même,
        // clé pour clé.
        assert_eq!(super::super::read_extended_metadata(chemin), selon);
    }
}

// ---------------------------------------------------------------------------
// Une balise libre, BAND ignoré, un nom standard retiré.
// ---------------------------------------------------------------------------

#[test]
fn une_balise_perso_txxx_chorus_est_un_ensemble() {
    let r = reglage(serde_json::json!({
        "ensemble": ["ENSEMBLE", "ORCHESTRA", "CHOIR", "txxx:chorus"],
    }));
    assert!(
        r.ensemble.contains(&"TXXX:CHORUS".to_string()),
        "mise en capitales"
    );

    let m = read_extended_metadata_selon(
        &mp3("chorus-mp3", &[("CHORUS", "Monteverdi Choir")], None),
        &r,
    );
    assert_eq!(
        m.get("ensemble").map(String::as_str),
        Some("Monteverdi Choir"),
        "{m:?}"
    );
    // La même clé en Vorbis : le préfixe TXXX: ne compte pas.
    let m =
        read_extended_metadata_selon(&flac("chorus-flac", &[("CHORUS", "RIAS Kammerchor")]), &r);
    assert_eq!(
        m.get("ensemble").map(String::as_str),
        Some("RIAS Kammerchor"),
        "{m:?}"
    );
    // Crédit écrit comme un ensemble (performer sans instrument).
    let lignes = super::super::credits_balises::lignes_des_balises(&m);
    assert!(
        lignes
            .iter()
            .any(|l| l.artist_name == "RIAS Kammerchor" && l.role == "performer")
    );
}

#[test]
fn band_ignore_ou_seulement_s_il_differe() {
    let schema2 = [
        ("COMPOSER", "Ludwig van Beethoven"),
        ("WORK", "Symphonie n° 9"),
        ("CONDUCTOR", "Herbert von Karajan"),
        ("BAND", "Berliner Philharmoniker"),
        ("ALBUMARTIST", "Berliner Philharmoniker"),
    ];
    let chemin = flac("band-schema2", &schema2);
    let ignore = reglage(serde_json::json!({ "band": "ignored" }));
    assert_eq!(
        read_extended_metadata_selon(&chemin, &ignore).get("ensemble"),
        None
    );
    let differe = reglage(serde_json::json!({ "band": "if_different" }));
    assert_eq!(
        read_extended_metadata_selon(&chemin, &differe).get("ensemble"),
        None,
        "BAND = artiste d'album : plus d'exception classique"
    );

    let autre = flac(
        "band-differe",
        &[
            ("BAND", "Wiener Philharmoniker"),
            ("ALBUMARTIST", "Karl Böhm"),
        ],
    );
    assert_eq!(
        read_extended_metadata_selon(&autre, &differe)
            .get("ensemble")
            .map(String::as_str),
        Some("Wiener Philharmoniker")
    );
    assert_eq!(
        read_extended_metadata_selon(&autre, &ignore).get("ensemble"),
        None
    );
}

#[test]
fn retirer_un_nom_standard_coupe_sa_lecture_et_un_nom_libre_le_remplace() {
    let chemin = flac(
        "chef-libre",
        &[
            ("CONDUCTOR", "Herbert von Karajan"),
            ("DIRIGENT", "Karl Böhm"),
            ("WORK", "Requiem"),
            ("WERK", "Requiem KV 626"),
        ],
    );
    let r = reglage(serde_json::json!({
        "conductor": ["DIRIGENT"],
        "work": ["WERK", "WORK"],
    }));
    let m = read_extended_metadata_selon(&chemin, &r);
    assert_eq!(m.get("conductor").map(String::as_str), Some("Karl Böhm"));
    // La lecture standard passe d'abord ; le nom libre ne sert qu'à défaut.
    assert_eq!(m.get("work").map(String::as_str), Some("Requiem"));
    let r = reglage(serde_json::json!({ "work": ["WERK"] }));
    let m = read_extended_metadata_selon(&chemin, &r);
    assert_eq!(m.get("work").map(String::as_str), Some("Requiem KV 626"));
    assert_eq!(
        m.get("conductor").map(String::as_str),
        Some("Herbert von Karajan")
    );
}

// ---------------------------------------------------------------------------
// Validation et stockage.
// ---------------------------------------------------------------------------

#[test]
fn la_saisie_est_validee() {
    let err = |j: serde_json::Value| {
        serde_json::from_value::<Saisie>(j)
            .unwrap()
            .valider()
            .unwrap_err()
    };
    assert!(err(serde_json::json!({ "ensemble": ["CHŒUR"] })).contains("A-Z"));
    assert!(err(serde_json::json!({ "ensemble": ["  "] })).contains("vide"));
    assert!(err(serde_json::json!({ "ensemble": ["BAND"] })).contains("BAND"));
    assert!(err(serde_json::json!({ "work": ["X".repeat(65)] })).contains("64"));
    let trop: Vec<String> = (0..17).map(|i| format!("K{i}")).collect();
    assert!(err(serde_json::json!({ "conductor": trop })).contains("16"));
    // Doublons écartés, sans casse ni préfixe.
    let r = reglage(serde_json::json!({ "ensemble": ["chorus", "TXXX:CHORUS", "CHORUS"] }));
    assert_eq!(r.ensemble, vec!["CHORUS"]);
    // Une liste vide est permise : le rôle n'est plus lu.
    assert!(
        reglage(serde_json::json!({ "movement": [] }))
            .movement
            .is_empty()
    );
}

#[test]
fn le_reglage_se_lit_en_base_et_un_json_casse_rend_les_defauts() {
    use crate::db::sqlite::SqliteDb;
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let b: Arc<dyn DbBackend> = Arc::new(db);
    assert_eq!(lire(&b), BalisesClassique::default());
    let s = SettingsRepo::with_backend(b.clone());
    s.set(CLE_REGLAGE, r#"{"band":"ignored","ensemble":["CHORUS"]}"#)
        .unwrap();
    let r = lire(&b);
    assert_eq!(
        (r.band, r.ensemble.clone()),
        (ModeBand::Ignored, vec!["CHORUS".to_string()])
    );
    assert_eq!(
        r.conductor,
        vec!["CONDUCTOR"],
        "un rôle absent garde son défaut"
    );
    s.set(CLE_REGLAGE, "pas du json").unwrap();
    assert_eq!(lire(&b), BalisesClassique::default());
    // Écrire les défauts efface la clé (et laisse le miroir aux défauts).
    let (_, apres) = ecrire(&b, Saisie::default()).unwrap();
    assert_eq!(apres, BalisesClassique::default());
    assert_eq!(s.get(CLE_REGLAGE).unwrap(), None);
}

#[test]
fn les_cles_candidates_couvrent_band_et_les_graphies_standard() {
    let c = BalisesClassique::default().cles_candidates();
    for k in [
        "BAND",
        "CONDUCTOR",
        "TPE3",
        "ENSEMBLE",
        "ORCHESTRA",
        "CHOIR",
        "WORK",
        "MOVEMENTNAME",
    ] {
        assert!(c.contains(&k.to_string()), "{k} manque : {c:?}");
    }
}
