//! #8325 — la relecture unique des balises classiques.

use super::*;
use crate::db::sqlite::SqliteDb;

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    db.execute_batch(
        "INSERT INTO tracks (id, title, file_path) VALUES \
           (1, 'A', '/m/a.flac'), (2, 'B', '/m/b.flac'), (3, 'C', '/m/c.flac'), \
           (4, 'Radio', NULL); \
         INSERT INTO track_metadata (track_id, key, value) VALUES (3, 'work', 'Œuvre de l''IA');",
    )
    .unwrap();
    Arc::new(db)
}

fn d() -> BalisesClassique {
    BalisesClassique::default()
}

fn carte(paires: &[(&str, &str)]) -> HashMap<String, String> {
    paires
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Le fichier A porte une œuvre et un ensemble, B un mouvement, C rien.
fn fichiers(p: &Path, _: &BalisesClassique) -> Option<HashMap<String, String>> {
    Some(match p.to_str().unwrap() {
        "/m/a.flac" => carte(&[("work", "Requiem"), ("ensemble", "Wiener Philharmoniker")]),
        "/m/b.flac" => carte(&[("movement", "Dies irae")]),
        _ => HashMap::new(),
    })
}

fn cle(b: &Arc<dyn DbBackend>, piste: i64, cle: &str) -> Option<String> {
    TrackMetadataRepo::with_backend(b.clone())
        .get_all(piste)
        .unwrap()
        .get(cle)
        .cloned()
}

#[test]
fn la_passe_lit_par_lots_reprend_au_curseur_et_ne_tourne_qu_une_fois_8325() {
    let b = base();
    assert!(!est_faite(&b));
    assert_eq!(restantes(&b), 3, "la radio sans fichier n'est pas lue");

    let l = traiter_lot_suivant(&b, 2, &d(), fichiers).unwrap();
    assert_eq!((l.lues, l.ecrites, l.fini), (2, 2, false));
    assert_eq!(cle(&b, 1, "work").as_deref(), Some("Requiem"));
    assert_eq!(
        cle(&b, 1, "ensemble").as_deref(),
        Some("Wiener Philharmoniker")
    );
    assert_eq!(cle(&b, 2, "movement").as_deref(), Some("Dies irae"));
    assert_eq!(restantes(&b), 1, "le curseur a avancé");

    // Un « redémarrage » : la passe repart au curseur, pas au début.
    let l = traiter_lot_suivant(&b, 2, &d(), |p, r| {
        assert_eq!(p, Path::new("/m/c.flac"), "relu une piste déjà traitée");
        fichiers(p, r)
    })
    .unwrap();
    assert_eq!((l.lues, l.ecrites, l.fini), (1, 0, false));
    assert_eq!(
        cle(&b, 3, "work").as_deref(),
        Some("Œuvre de l'IA"),
        "un fichier muet n'efface pas l'œuvre posée par l'assistant"
    );

    let l = traiter_lot_suivant(&b, 2, &d(), fichiers).unwrap();
    assert!(l.fini && est_faite(&b));

    // Une fois close, plus aucune lecture, même si une piste arrive.
    b.execute(
        "INSERT INTO tracks (id, title, file_path) VALUES (9, 'Neuve', '/m/n.flac')",
        &[],
    )
    .unwrap();
    let l = traiter_lot_suivant(&b, 2, &d(), |_, _| {
        panic!("la passe close relit un fichier")
    })
    .unwrap();
    assert_eq!((l.lues, l.fini), (0, true));
}

// ---------------------------------------------------------------------------
// ESL63P — la relance après une modification du réglage, sur de VRAIS
// fichiers lus par `lire` (la lecture de production).
// ---------------------------------------------------------------------------

fn copie(fixture: &str, epreuve: &str) -> crate::test_scratch::ScratchFile {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    let ext = Path::new(fixture).extension().unwrap().to_str().unwrap();
    let cible =
        crate::test_scratch::scratch_file(&format!("relance-esl63p-{epreuve}"), &format!(".{ext}"));
    std::fs::copy(&source, &cible).expect("copie de la fixture");
    cible
}

fn flac(epreuve: &str, paires: &[(&str, &str)]) -> crate::test_scratch::ScratchFile {
    use lofty::config::{ParseOptions, WriteOptions};
    use lofty::file::AudioFile;
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

/// Un MP3 iTunes : TIT1 SANS GRP1, donc un regroupement, pas une œuvre.
fn mp3_tit1_sans_grp1(epreuve: &str) -> crate::test_scratch::ScratchFile {
    use lofty::TextEncoding;
    use lofty::config::{ParseOptions, WriteOptions};
    use lofty::file::AudioFile;
    use lofty::id3::v2::{Frame, FrameId, Id3v2Tag, TextInformationFrame};
    use lofty::mpeg::MpegFile;
    use std::borrow::Cow;
    let chemin = copie("test.mp3", epreuve);
    let mut fh = std::fs::File::open(&*chemin).unwrap();
    let mut f = MpegFile::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    let mut tag = f.id3v2().cloned().unwrap_or_else(Id3v2Tag::new);
    tag.insert(Frame::Text(TextInformationFrame::new(
        FrameId::Valid(Cow::Borrowed("TIT1")),
        TextEncoding::UTF8,
        "Messe en si mineur".to_string(),
    )));
    f.set_id3v2(tag);
    f.save_to_path(&*chemin, WriteOptions::default()).unwrap();
    chemin
}

/// Une base dont les pistes 1… pointent sur `chemins`, et dont
/// `track_metadata` porte `valeurs` ; la première passe est close.
fn base_de(chemins: &[&Path], valeurs: &[(i64, &str, &str)]) -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let b: Arc<dyn DbBackend> = Arc::new(db);
    for (i, c) in chemins.iter().enumerate() {
        let id = i as i64 + 1;
        let c = c.to_str().unwrap().to_string();
        b.execute(
            "INSERT INTO tracks (id, title, file_path) VALUES (?, 'T', ?)",
            &[&id as &dyn ToSqlValue, &c as &dyn ToSqlValue],
        )
        .unwrap();
    }
    let meta = TrackMetadataRepo::with_backend(b.clone());
    for (id, k, v) in valeurs {
        meta.set(*id, k, v).unwrap();
    }
    SettingsRepo::with_backend(b.clone())
        .set(CLE_FAITE, "1")
        .unwrap();
    b
}

fn relire(b: &Arc<dyn DbBackend>, actuel: &BalisesClassique) {
    while !traiter_lot_suivant(b, 10, actuel, lire).unwrap().fini {}
}

fn reglage(json: serde_json::Value) -> BalisesClassique {
    serde_json::from_value::<crate::metadata::balises_classique::Saisie>(json)
        .unwrap()
        .valider()
        .unwrap()
}

/// Revue de #8604, scénario 1 : MP3 TIT1 sans GRP1 et une œuvre posée par
/// l'assistant IA. L'ancien réglage ne lisait AUCUNE œuvre dans ce fichier :
/// un PUT quelconque ne doit pas l'effacer.
#[test]
fn une_oeuvre_de_l_ia_sur_un_mp3_tit1_sans_grp1_reste() {
    let f = mp3_tit1_sans_grp1("tit1");
    let b = base_de(&[&f], &[(1, "work", "Messe en si mineur")]);
    reinitialiser(&b, &d()).unwrap();
    relire(&b, &reglage(serde_json::json!({ "band": "ignored" })));
    assert_eq!(cle(&b, 1, "work").as_deref(), Some("Messe en si mineur"));
    assert!(est_faite(&b));
}

/// Scénario 2 : piste NON classique, BAND = artiste d'album, et un ensemble
/// posé par la synchro communautaire (même nom). L'ancien réglage ne lisait
/// pas BAND ici : l'ensemble reste.
#[test]
fn un_ensemble_communautaire_egal_a_band_hors_classique_reste() {
    let f = flac(
        "pop",
        &[
            ("ARTIST", "Pink Floyd"),
            ("ALBUMARTIST", "Pink Floyd"),
            ("BAND", "Pink Floyd"),
        ],
    );
    let b = base_de(&[&f], &[(1, "ensemble", "Pink Floyd")]);
    reinitialiser(&b, &d()).unwrap();
    relire(&b, &reglage(serde_json::json!({ "band": "ignored" })));
    assert_eq!(cle(&b, 1, "ensemble").as_deref(), Some("Pink Floyd"));
}

/// Ce que l'ancien réglage lisait et que le nouveau ne lit plus part ; ce que
/// le nouveau lit est écrit ; les crédits `balises` suivent.
#[test]
fn band_ignore_retire_l_ensemble_que_l_ancien_reglage_lisait() {
    let classique = flac(
        "hcfr2",
        &[
            ("COMPOSER", "Ludwig van Beethoven"),
            ("CONDUCTOR", "Herbert von Karajan"),
            ("BAND", "Berliner Philharmoniker"),
            ("ALBUMARTIST", "Berliner Philharmoniker"),
        ],
    );
    let choeur = flac("chorus", &[("CHORUS", "RIAS Kammerchor")]);
    let b = base_de(
        &[&classique, &choeur],
        &[
            (1, "ensemble", "Berliner Philharmoniker"),
            (2, "ensemble", "Autre chose"),
        ],
    );
    reinitialiser(&b, &d()).unwrap();
    relire(
        &b,
        &reglage(serde_json::json!({
            "band": "ignored",
            "ensemble": ["ENSEMBLE", "ORCHESTRA", "CHOIR", "TXXX:CHORUS"],
        })),
    );
    assert_eq!(
        cle(&b, 1, "ensemble"),
        None,
        "lue sous BAND avant, plus maintenant"
    );
    assert_eq!(
        cle(&b, 1, "conductor").as_deref(),
        Some("Herbert von Karajan")
    );
    assert_eq!(cle(&b, 2, "ensemble").as_deref(), Some("RIAS Kammerchor"));
    let n = |sql: &str| b.query_many(sql, &[]).unwrap().len();
    assert_eq!(
        n(
            "SELECT 1 FROM track_credits WHERE track_id = 1 AND role = 'conductor' AND source = 'balises'"
        ),
        1
    );
    assert_eq!(
        SettingsRepo::with_backend(b.clone())
            .get(CLE_RECONFIGURATION)
            .unwrap(),
        None,
        "close, la relance s'efface"
    );
}

/// La relance est en BASE dès `reinitialiser` (appelé au PUT) : une autre
/// « instance » (redémarrage) la reprend, avec l'ancien réglage.
#[test]
fn la_relance_est_persistee_des_le_put() {
    let classique = flac(
        "persiste",
        &[
            ("COMPOSER", "Bach"),
            ("CONDUCTOR", "Karl Richter"),
            ("BAND", "Münchener Bach-Orchester"),
            ("ALBUMARTIST", "Münchener Bach-Orchester"),
        ],
    );
    let b = base_de(
        &[&classique],
        &[(1, "ensemble", "Münchener Bach-Orchester")],
    );
    reinitialiser(&b, &d()).unwrap();
    // « Redémarrage » : rien en mémoire, tout se relit en base.
    assert!(!est_faite(&b));
    assert_eq!(restantes(&b), 1);
    relire(&b, &reglage(serde_json::json!({ "band": "if_different" })));
    assert_eq!(cle(&b, 1, "ensemble"), None);
}

/// Un fichier introuvable (partage absent) n'efface rien.
#[test]
fn un_fichier_introuvable_n_efface_rien_pendant_la_relance() {
    let b = base_de(&[Path::new("/inexistant/x.flac")], &[(1, "ensemble", "X")]);
    reinitialiser(&b, &d()).unwrap();
    relire(&b, &reglage(serde_json::json!({ "band": "ignored" })));
    assert_eq!(cle(&b, 1, "ensemble").as_deref(), Some("X"));
}
