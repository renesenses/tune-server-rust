//! #9158 (Rob) — témoins du réglage « Balises personnelles → tags Tune » :
//! validation, découpe des valeurs, puis de VRAIS conteneurs (FLAC, MP3, M4A)
//! copiés des fixtures, balisés par lofty, relus par la lecture étendue du
//! scan et posés en tags Tune dans une vraie base SQLite.
//!
//! Aucun témoin ne touche au miroir global ([`courante`]) : la lecture passe
//! le réglage explicitement (`read_extended_metadata_avec`).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;

use super::*;
use crate::db::backend::DbBackend;

fn reglage(json: serde_json::Value) -> TagsPerso {
    serde_json::from_value::<TagsPerso>(json)
        .unwrap()
        .valider()
        .unwrap()
}

fn p(paires: &[(&str, &str)]) -> Vec<(String, String)> {
    paires
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn la_saisie_est_mise_en_capitales_et_validee_9158() {
    let r = reglage(serde_json::json!({ "mappings": [
        { "key": " txxx:mood " },
        { "key": "TXXX:MOOD" },
        { "key": "----:com.apple.iTunes:Favori", "mode": "fixed", "tag": "  Coup  de cœur " },
    ]}));
    assert_eq!(r.mappings.len(), 2, "le doublon exact est écarté : {r:?}");
    assert_eq!(r.mappings[0].key, "TXXX:MOOD");
    assert_eq!(r.mappings[0].mode, Mode::Values);
    assert_eq!(r.mappings[1].tag.as_deref(), Some("Coup de cœur"));
    assert_eq!(r.cles(), vec!["MOOD".to_string(), "FAVORI".to_string()]);

    for (json, motif) in [
        (serde_json::json!({ "key": "" }), "vide"),
        (serde_json::json!({ "key": "HUMEUR É" }), "A-Z"),
        (serde_json::json!({ "key": "X".repeat(65) }), "dépasse"),
        (
            serde_json::json!({ "key": "FAVORI", "mode": "fixed" }),
            "tag fixe",
        ),
        (
            serde_json::json!({ "key": "FAVORI", "mode": "fixed", "tag": "  " }),
            "tag fixe",
        ),
        (serde_json::json!({ "key": "TXXX:" }), "pas de nom"),
    ] {
        let e = serde_json::from_value::<TagsPerso>(serde_json::json!({ "mappings": [json] }))
            .unwrap()
            .valider()
            .unwrap_err();
        assert!(e.contains(motif), "{e}");
    }
    let trop: Vec<_> = (0..=MAX_CORRESPONDANCES)
        .map(|i| serde_json::json!({ "key": format!("K{i}") }))
        .collect();
    assert!(
        serde_json::from_value::<TagsPerso>(serde_json::json!({ "mappings": trop }))
            .unwrap()
            .valider()
            .is_err()
    );
}

#[test]
fn chaque_valeur_devient_un_tag_et_le_mode_fixe_pose_son_tag_9158() {
    let r = reglage(serde_json::json!({ "mappings": [
        { "key": "MOOD" },
        { "key": "OCCASION", "prefix": "Occasion : " },
        { "key": "FAVORI", "mode": "fixed", "tag": "Favori de Rob" },
        { "key": "ABSENTE", "mode": "fixed", "tag": "Jamais" },
    ]}));
    let tags = r.tags_des_paires(&p(&[
        ("TITLE", "So What"),
        ("TXXX:mood", "Calme;Nuit ; ;calme"),
        // Une clé répétée (deux trames Vorbis) : chaque trame compte.
        ("MOOD", "Pluie"),
        ("----:com.apple.iTunes:OCCASION", "Dîner"),
        ("Favori", "1"),
        ("ABSENTE", "  ; "),
    ]));
    assert_eq!(
        tags,
        vec![
            "Calme".to_string(),
            "Nuit".to_string(),
            "Pluie".to_string(),
            "Occasion : Dîner".to_string(),
            "Favori de Rob".to_string(),
        ],
        "« calme » est un doublon de « Calme » ; une balise vide ne pose pas son tag fixe"
    );
    // Les trames ID3 à valeurs multiples (NUL relu en « ; ») aussi.
    assert_eq!(
        reglage(serde_json::json!({ "mappings": [{ "key": "MOOD" }] }))
            .tags_des_paires(&p(&[("TXXX:MOOD", "A ; B\u{0}C")])),
        vec!["A".to_string(), "B".to_string(), "C".to_string()]
    );
}

#[test]
fn les_valeurs_sont_bornees_et_nettoyees_9158() {
    let r = reglage(serde_json::json!({ "mappings": [{ "key": "MOOD" }] }));
    let longue = "x".repeat(MAX_LONGUEUR_TAG + 40);
    let tags = r.tags_des_paires(&p(&[("MOOD", &format!("a\tb\nc;{longue}"))]));
    assert_eq!(tags[0], "a b c");
    assert_eq!(tags[1].chars().count(), MAX_LONGUEUR_TAG);
    let beaucoup: String = (0..200).map(|i| format!("v{i};")).collect();
    assert_eq!(
        r.tags_des_paires(&p(&[("MOOD", &beaucoup)])).len(),
        MAX_TAGS_PAR_PISTE
    );
    // Aller-retour par la clé réservée.
    assert_eq!(decoder(&encoder(&tags)), tags);
    assert!(decoder("").is_empty());
}

#[test]
fn la_cle_reservee_n_est_jamais_rangee_dans_track_metadata_9158() {
    use crate::db::sqlite::SqliteDb;
    use crate::db::track_metadata_repo::TrackMetadataRepo;
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    db.execute_batch(
        "INSERT INTO artists (id, name) VALUES (1, 'A');
         INSERT INTO albums (id, title, artist_id) VALUES (1, 'B', 1);
         INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, disc_number, track_number, channels)
         VALUES (1, 'T', 1, 1, 1000, 1, 1, 2);",
    )
    .unwrap();
    let repo = TrackMetadataRepo::new(db);
    let meta = HashMap::from([
        ("mood".to_string(), "Calme".to_string()),
        (CLE_LUE.to_string(), "Calme".to_string()),
    ]);
    repo.set_batch_multi(&[(1, meta.clone())]).unwrap();
    repo.set_batch(1, &meta).unwrap();
    repo.set(1, CLE_LUE, "x").unwrap();
    let rangees = repo.get_all(1).unwrap();
    assert_eq!(rangees.get("mood").map(String::as_str), Some("Calme"));
    assert!(
        !rangees.contains_key(CLE_LUE),
        "la clé réservée a été rangée en base : {rangees:?}"
    );
}

#[test]
fn les_cles_libres_se_distinguent_des_connues_9158() {
    assert_eq!(
        nom_utilisable("id3v2", "TXXX:Mood Rob").as_deref(),
        Some("MOOD ROB")
    );
    assert_eq!(nom_utilisable("id3v2", "TIT2"), None);
    assert_eq!(
        nom_utilisable("mp4", "----:com.apple.iTunes:FAVORI").as_deref(),
        Some("FAVORI")
    );
    assert_eq!(nom_utilisable("mp4", "\u{a9}nam"), None);
    assert_eq!(
        nom_utilisable("vorbis", "Occasion").as_deref(),
        Some("OCCASION")
    );
    assert_eq!(nom_utilisable("id3v1", "TITLE"), None);
    assert!(est_cle_connue("TITLE"));
    assert!(est_cle_connue("MUSICBRAINZ ALBUM ID"));
    assert!(est_cle_connue("REPLAYGAIN_TRACK_GAIN"));
    assert!(!est_cle_connue("OCCASION"));
}

// ---------------------------------------------------------------------------
// Vrais conteneurs, vraie base.
// ---------------------------------------------------------------------------

fn copie(fixture: &str, epreuve: &str) -> crate::test_scratch::ScratchFile {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(fixture);
    let ext = Path::new(fixture).extension().unwrap().to_str().unwrap();
    let cible =
        crate::test_scratch::scratch_file(&format!("tagsperso9158-{epreuve}"), &format!(".{ext}"));
    std::fs::copy(&source, &cible).expect("copie de la fixture");
    cible
}

fn flac(epreuve: &str, paires: &[(&str, &str)]) -> crate::test_scratch::ScratchFile {
    let chemin = copie("test.flac", epreuve);
    reecrire_flac(&chemin, paires);
    chemin
}

/// Remplace les commentaires Vorbis du FLAC par `paires`.
fn reecrire_flac(chemin: &Path, paires: &[(&str, &str)]) {
    use lofty::flac::FlacFile;
    use lofty::ogg::VorbisComments;
    let mut fh = std::fs::File::open(chemin).unwrap();
    let mut f = FlacFile::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    let mut vc = VorbisComments::default();
    for (k, v) in paires {
        vc.push(k.to_string(), v.to_string());
    }
    f.set_vorbis_comments(vc);
    f.save_to_path(chemin, WriteOptions::default()).unwrap();
}

fn mp3(epreuve: &str, txxx: &[(&str, &str)]) -> crate::test_scratch::ScratchFile {
    use lofty::id3::v2::Id3v2Tag;
    use lofty::mpeg::MpegFile;
    let chemin = copie("test.mp3", epreuve);
    let mut fh = std::fs::File::open(&*chemin).unwrap();
    let mut f = MpegFile::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    let mut tag = f.id3v2().cloned().unwrap_or_else(Id3v2Tag::new);
    use lofty::tag::Accessor;
    tag.set_title("Titre".into());
    tag.set_artist("Rob".into());
    for (k, v) in txxx {
        tag.insert_user_text(k.to_string(), v.to_string());
    }
    f.set_id3v2(tag);
    f.save_to_path(&*chemin, WriteOptions::default()).unwrap();
    chemin
}

fn m4a(epreuve: &str, libres: &[(&str, &str)]) -> crate::test_scratch::ScratchFile {
    use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst, Mp4File};
    use std::borrow::Cow;
    let chemin = copie("test.m4a", epreuve);
    let mut fh = std::fs::File::open(&*chemin).unwrap();
    let mut f = Mp4File::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    let mut ilst = f.ilst().cloned().unwrap_or_else(Ilst::default);
    for (nom, v) in libres {
        ilst.insert(Atom::new(
            AtomIdent::Freeform {
                mean: Cow::Borrowed("com.apple.iTunes"),
                name: Cow::Owned(nom.to_string()),
            },
            AtomData::UTF8(v.to_string()),
        ));
    }
    f.set_ilst(ilst);
    f.save_to_path(&*chemin, WriteOptions::default()).unwrap();
    chemin
}

fn rob() -> TagsPerso {
    reglage(serde_json::json!({ "mappings": [
        { "key": "MOOD" },
        { "key": "TXXX:FAVORI", "mode": "fixed", "tag": "Favori" },
    ]}))
}

fn lus(meta: &HashMap<String, String>) -> Option<Vec<String>> {
    meta.get(CLE_LUE).map(|v| decoder(v))
}

fn lire(chemin: &Path, r: &TagsPerso) -> HashMap<String, String> {
    crate::metadata::read_extended_metadata_avec(
        chemin,
        &crate::metadata::balises_classique::BalisesClassique::default(),
        r,
    )
}

#[test]
fn le_scan_lit_les_balises_libres_des_trois_conteneurs_9158() {
    let f = flac(
        "flac",
        &[
            ("TITLE", "So What"),
            ("ARTIST", "Miles"),
            ("MOOD", "Calme;Nuit"),
            ("FAVORI", "oui"),
        ],
    );
    assert_eq!(
        lus(&lire(&f, &rob())),
        Some(vec!["Calme".into(), "Nuit".into(), "Favori".into()]),
        "FLAC : MOOD et FAVORI non repris"
    );
    let m = mp3("mp3", &[("MOOD", "Pluie"), ("FAVORI", "1")]);
    assert_eq!(
        lus(&lire(&m, &rob())),
        Some(vec!["Pluie".into(), "Favori".into()]),
        "MP3 : TXXX non repris"
    );
    let a = m4a("m4a", &[("MOOD", "Soleil"), ("FAVORI", "x")]);
    assert_eq!(
        lus(&lire(&a, &rob())),
        Some(vec!["Soleil".into(), "Favori".into()]),
        "M4A : atomes libres non repris"
    );
    // Sans réglage : pas de clé, la lecture d'avant est intacte.
    let sans = lire(&f, &TagsPerso::default());
    assert!(!sans.contains_key(CLE_LUE), "{sans:?}");
    assert_eq!(
        sans,
        crate::metadata::read_extended_metadata_selon(
            &f,
            &crate::metadata::balises_classique::BalisesClassique::default()
        )
    );
    // Réglé mais rien dans le fichier : clé présente et vide (c'est ce qui
    // retire les tags que le scan avait posés).
    let vide = flac("vide", &[("TITLE", "x"), ("ARTIST", "y")]);
    assert_eq!(lus(&lire(&vide, &rob())), Some(vec![]));
}

// --- La base ---------------------------------------------------------------

fn base() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    db.execute_batch(
        "INSERT INTO artists (id, name) VALUES (1, 'Rob');
         INSERT INTO albums (id, title, artist_id) VALUES (10, 'Album', 1);
         INSERT INTO albums (id, title, artist_id) VALUES (20, 'Autre', 1);
         INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, disc_number, track_number, channels, file_path)
           VALUES (1, 'Un', 10, 1, 1000, 1, 1, 2, '/m/1.flac'),
                  (2, 'Deux', 10, 1, 1000, 1, 2, 2, '/m/2.mp3'),
                  (3, 'Trois', 20, 1, 1000, 1, 1, 2, '/m/3.m4a');",
    )
    .unwrap();
    Arc::new(db)
}

/// Les noms de tags posés sur `(item_type, item_id)`, triés.
fn tags_de(b: &Arc<dyn DbBackend>, item_type: &str, item_id: i64) -> Vec<String> {
    let mut v: Vec<String> = crate::db::tag_repo::TagRepo::with_backend(b.clone())
        .tags_for_item(item_type, item_id)
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    v.sort();
    v
}

fn compte(b: &Arc<dyn DbBackend>, sql: &str) -> i64 {
    b.query_many(sql, &[]).unwrap()[0][0].as_i64().unwrap()
}

#[test]
fn de_bout_en_bout_piste_album_idempotence_retrait_et_tags_manuels_9158() {
    use crate::db::tag_repo::TagRepo;
    use crate::db::tags_perso_repo::ecrire_lot;
    let b = base();
    let tags = TagRepo::with_backend(b.clone());
    // Un tag posé À LA MAIN avant tout scan, sur la piste 1, du même nom
    // qu'une valeur du fichier (casse différente), et un autre sans rapport.
    let calme = tags.create("calme", None).unwrap();
    let a_ecouter = tags.create("À écouter", None).unwrap();
    tags.tag_item(calme, "track", 1).unwrap();
    tags.tag_item(a_ecouter, "track", 1).unwrap();
    tags.tag_item(a_ecouter, "album", 10).unwrap();

    let f1 = flac(
        "e2e-1",
        &[
            ("TITLE", "Un"),
            ("ARTIST", "Rob"),
            ("MOOD", "Calme;Nuit"),
            ("FAVORI", "1"),
        ],
    );
    let f2 = mp3("e2e-2", &[("MOOD", "Nuit;Pluie"), ("FAVORI", "1")]);
    let f3 = m4a("e2e-3", &[("MOOD", "Soleil")]);
    let scanner = |r: &TagsPerso| {
        let lot = vec![(1, lire(&f1, r)), (2, lire(&f2, r)), (3, lire(&f3, r))];
        ecrire_lot(&b, &lot)
    };

    let premier = scanner(&rob());
    // Tri par octets : capitales, puis minuscules, puis « À ».
    assert_eq!(
        tags_de(&b, "track", 1),
        vec!["Favori", "Nuit", "calme", "À écouter"]
    );
    assert_eq!(tags_de(&b, "track", 2), vec!["Favori", "Nuit", "Pluie"]);
    assert_eq!(tags_de(&b, "track", 3), vec!["Soleil"]);
    // Album 10 : Nuit et Favori sur ses DEUX pistes ; Calme / Pluie non.
    assert_eq!(
        tags_de(&b, "album", 10),
        vec!["Favori", "Nuit", "À écouter"],
        "l'album prend les tags communs à toutes ses pistes, et garde le manuel"
    );
    assert_eq!(
        tags_de(&b, "album", 20),
        vec!["Soleil"],
        "une piste seule : l'album suit"
    );
    // « calme » existant a servi : pas de second tag « Calme ».
    assert_eq!(
        compte(&b, "SELECT COUNT(*) FROM tags WHERE lower(name) = 'calme'"),
        1
    );
    assert!(premier.ajoutes > 0);

    // IDEMPOTENCE : un rescan identique n'écrit rien, aucun doublon.
    let avant = compte(&b, "SELECT COUNT(*) FROM item_tags");
    let tags_avant = compte(&b, "SELECT COUNT(*) FROM tags");
    let second = scanner(&rob());
    assert_eq!(
        second,
        crate::db::tags_perso_repo::Bilan::default(),
        "rescan non idempotent"
    );
    assert_eq!(compte(&b, "SELECT COUNT(*) FROM item_tags"), avant);
    assert_eq!(compte(&b, "SELECT COUNT(*) FROM tags"), tags_avant);

    // RETRAIT : Calme et FAVORI quittent le fichier 1. Le « calme » MANUEL de
    // la piste 1 reste (jamais marqué) ; Favori part de la piste 1 et donc de
    // l'album ; À écouter (manuel) ne bouge pas.
    reecrire_flac(&f1, &[("TITLE", "Un"), ("ARTIST", "Rob"), ("MOOD", "Nuit")]);
    let retrait = scanner(&rob());
    assert!(retrait.retires >= 2, "{retrait:?}");
    assert_eq!(tags_de(&b, "track", 1), vec!["Nuit", "calme", "À écouter"]);
    assert_eq!(tags_de(&b, "album", 10), vec!["Nuit", "À écouter"]);
    assert_eq!(tags_de(&b, "track", 2), vec!["Favori", "Nuit", "Pluie"]);

    // Une pose du scan reprise À LA MAIN devient manuelle : elle survit au
    // retrait de la balise.
    let pluie = tags.get_by_name("Pluie").unwrap().unwrap().id.unwrap();
    tags.tag_item(pluie, "track", 2).unwrap();
    // Réglage vidé de MOOD : seul FAVORI reste lu.
    let favori_seul = reglage(
        serde_json::json!({ "mappings": [{ "key": "FAVORI", "mode": "fixed", "tag": "Favori" }] }),
    );
    scanner(&favori_seul);
    assert_eq!(tags_de(&b, "track", 1), vec!["calme", "À écouter"]);
    assert_eq!(
        tags_de(&b, "track", 2),
        vec!["Favori", "Pluie"],
        "le Pluie repris à la main a été retiré par le scan"
    );
    assert_eq!(tags_de(&b, "track", 3), Vec::<String>::new());
    assert_eq!(tags_de(&b, "album", 10), vec!["À écouter"]);

    // Un fichier illisible (lecture vide) ne retire rien.
    let avant = compte(&b, "SELECT COUNT(*) FROM item_tags");
    ecrire_lot(&b, &[(2, HashMap::new())]);
    assert_eq!(compte(&b, "SELECT COUNT(*) FROM item_tags"), avant);
}

// ---------------------------------------------------------------------------
// Retour de Rob (19/11) : GROUPING répété, séparateurs, plusieurs noms,
// graphies standard MP3 (TIT1 / GRP1 / TMOO, NUL ID3v2.4) et M4A (©grp).
// ---------------------------------------------------------------------------

fn r_sep(cle: &str, sep: Option<&str>) -> TagsPerso {
    let mut c = serde_json::json!({ "key": cle });
    if let Some(s) = sep {
        c["separator"] = serde_json::json!(s);
    }
    reglage(serde_json::json!({ "mappings": [c] }))
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[test]
fn rob_grouping_repete_dans_un_flac_donne_trois_tags_9158() {
    let f = flac(
        "rob-repete",
        &[
            ("TITLE", "t"),
            ("ARTIST", "a"),
            ("GROUPING", "Roots"),
            ("GROUPING", "Pop & Rock"),
            ("GROUPING", " Americana "),
            ("GROUPING", "roots"),
        ],
    );
    assert_eq!(
        lus(&lire(&f, &r_sep("GROUPING", None))),
        Some(s(&["Roots", "Pop & Rock", "Americana"])),
        "champs répétés : chaque trame est un tag, bords nettoyés, doublon ignoré"
    );
}

#[test]
fn rob_separateurs_reglables_9158() {
    let un =
        |v: &str, sep: Option<&str>| r_sep("GROUPING", sep).tags_des_paires(&p(&[("GROUPING", v)]));
    let attendu = s(&["Roots", "Pop & Rock", "Americana"]);
    assert_eq!(
        un("Roots;Pop & Rock; Americana", None),
        attendu,
        "; par défaut"
    );
    assert_eq!(un("Roots|Pop & Rock|Americana", Some("|")), attendu);
    assert_eq!(un("Roots/Pop & Rock/Americana", Some("/")), attendu);
    assert_eq!(
        un("Roots // Pop & Rock // Americana", Some("//")),
        attendu,
        "personnalisé"
    );
    assert_eq!(
        un("Roots|Pop & Rock", Some("")),
        s(&["Roots|Pop & Rock"]),
        "aucun séparateur : la valeur entière"
    );
    assert_eq!(
        un("Roots;Pop & Rock", Some("|")),
        s(&["Roots;Pop & Rock"]),
        "le séparateur réglé REMPLACE le ; par défaut"
    );
    // Séparateur ET champs répétés ensemble.
    assert_eq!(
        r_sep("GROUPING", Some("|")).tags_des_paires(&p(&[
            ("GROUPING", "Roots|Pop & Rock"),
            ("GROUPING", "Americana|roots")
        ])),
        attendu
    );
    // NUL (valeurs multiples ID3v2.4) est toujours découpé, même sans séparateur.
    assert_eq!(un("Roots\u{0}Pop & Rock\u{0}Americana", Some("")), attendu);
    // Validation.
    for (sep, motif) in [("123456789", "dépasse"), ("\t", "contrôle")] {
        let e = serde_json::from_value::<TagsPerso>(serde_json::json!({
            "mappings": [{ "key": "GROUPING", "separator": sep }]
        }))
        .unwrap()
        .valider()
        .unwrap_err();
        assert!(e.contains(motif), "{e}");
    }
    // `;` saisi = le défaut, rangé absent.
    assert_eq!(r_sep("GROUPING", Some(";")).mappings[0].separator, None);
    assert_eq!(
        r_sep("GROUPING", Some("")).mappings[0].separator.as_deref(),
        Some("")
    );
}

#[test]
fn rob_plusieurs_noms_de_champs_9158() {
    let r = reglage(serde_json::json!({ "mappings": [
        { "key": "GROUPING" }, { "key": "MOOD" }, { "key": "STYLE" },
        { "key": "ROONTRACKTAG", "separator": "|" }, { "key": "ROONALBUMTAG" },
    ]}));
    let f = flac(
        "rob-noms",
        &[
            ("TITLE", "t"),
            ("ARTIST", "a"),
            ("GROUPING", "Roots"),
            ("MOOD", "Calme"),
            ("STYLE", "Folk"),
            ("ROONTRACKTAG", "Live|Acoustique"),
            ("ROONALBUMTAG", "Coup de cœur"),
        ],
    );
    assert_eq!(
        lus(&lire(&f, &r)),
        Some(s(&[
            "Roots",
            "Calme",
            "Folk",
            "Live",
            "Acoustique",
            "Coup de cœur"
        ]))
    );
    assert_eq!(
        SUGGESTIONS,
        ["GROUPING", "MOOD", "STYLE", "ROONTRACKTAG", "ROONALBUMTAG"]
    );
}

fn mp3_trames(
    epreuve: &str,
    texte: &[(&str, &str)],
    txxx: &[(&str, &str)],
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
    use lofty::tag::Accessor;
    tag.set_title("Titre".into());
    tag.set_artist("Rob".into());
    for (id, v) in texte {
        tag.insert(Frame::Text(TextInformationFrame::new(
            FrameId::Valid(Cow::Owned(id.to_string())),
            TextEncoding::UTF8,
            v.to_string(),
        )));
    }
    for (k, v) in txxx {
        tag.insert(Frame::UserText(ExtendedTextFrame::new(
            TextEncoding::UTF8,
            k.to_string(),
            v.to_string(),
        )));
    }
    f.set_id3v2(tag);
    f.save_to_path(&*chemin, WriteOptions::default()).unwrap();
    chemin
}

#[test]
fn rob_mp3_txxx_nul_grp1_tit1_et_tmoo_9158() {
    let r = reglage(serde_json::json!({ "mappings": [{ "key": "GROUPING" }, { "key": "MOOD" }] }));
    // TXXX:GROUPING à valeurs multiples ID3v2.4 (NUL).
    let a = mp3_trames(
        "rob-txxx",
        &[],
        &[("GROUPING", "Roots\u{0}Pop & Rock\u{0}Americana")],
    );
    assert_eq!(
        lus(&lire(&a, &r)),
        Some(s(&["Roots", "Pop & Rock", "Americana"]))
    );
    // GRP1 (iTunes 12.5+) : le regroupement ; TIT1 est alors l'ŒUVRE.
    let b = mp3_trames(
        "rob-grp1",
        &[("GRP1", "Roots"), ("TIT1", "Sonate"), ("TMOO", "Calme")],
        &[],
    );
    assert_eq!(
        lus(&lire(&b, &r)),
        Some(s(&["Roots", "Calme"])),
        "GRP1 et TMOO lus, TIT1 écarté quand GRP1 est là"
    );
    // TIT1 seul : le regroupement classique.
    let c = mp3_trames("rob-tit1", &[("TIT1", "Roots;Americana")], &[]);
    assert_eq!(lus(&lire(&c, &r)), Some(s(&["Roots", "Americana"])));
}

#[test]
fn rob_m4a_grp_et_atome_libre_a_valeurs_multiples_9158() {
    use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst, Mp4File};
    use std::borrow::Cow;
    let chemin = copie("test.m4a", "rob-m4a");
    let mut fh = std::fs::File::open(&*chemin).unwrap();
    let mut f = Mp4File::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    let mut ilst = f.ilst().cloned().unwrap_or_else(Ilst::default);
    ilst.insert(Atom::new(
        AtomIdent::Fourcc(*b"\xa9grp"),
        AtomData::UTF8("Roots|Americana".into()),
    ));
    let humeur = Atom::from_collection(
        AtomIdent::Freeform {
            mean: Cow::Borrowed("com.apple.iTunes"),
            name: Cow::Borrowed("MOOD"),
        },
        vec![
            AtomData::UTF8("Calme".into()),
            AtomData::UTF8("Nuit".into()),
        ],
    )
    .expect("atome à deux valeurs");
    ilst.insert(humeur);
    f.set_ilst(ilst);
    f.save_to_path(&*chemin, WriteOptions::default()).unwrap();
    let r = reglage(serde_json::json!({ "mappings": [
        { "key": "GROUPING", "separator": "|" }, { "key": "MOOD" },
    ]}));
    assert_eq!(
        lus(&lire(&chemin, &r)),
        Some(s(&["Roots", "Americana", "Calme", "Nuit"]))
    );
    // L'aperçu les compte sous le nom à saisir.
    assert_eq!(
        nom_utilisable("mp4", "\u{a9}grp").as_deref(),
        Some("GROUPING")
    );
    assert_eq!(nom_utilisable("id3v2", "GRP1").as_deref(), Some("GROUPING"));
    assert_eq!(nom_utilisable("id3v2", "TMOO").as_deref(), Some("MOOD"));
    assert!(
        !est_cle_connue("GROUPING") && !est_cle_connue("MOOD"),
        "proposées, pas masquées"
    );
}

/// Relecture du 19/11 : une lecture brute RATÉE ne range rien (les tags du
/// scan restent) ; une lecture réussie sans la balise range une liste vide
/// (ils partent).
#[test]
fn une_lecture_ratee_ne_retire_pas_les_tags_du_scan_9158() {
    let r = reglage(serde_json::json!({ "mappings": [{ "key": "MOOD" }] }));
    assert_eq!(
        valeur_lue(&r, None),
        None,
        "fichier illisible : piste sautée"
    );
    assert_eq!(valeur_lue(&r, Some(&[])).as_deref(), Some("[]"));
    assert_eq!(
        decoder(&valeur_lue(&r, Some(&p(&[("MOOD", "Calme")]))).unwrap()),
        vec!["Calme".to_string()]
    );
    assert_eq!(valeur_lue(&TagsPerso::default(), Some(&[])), None);
}

/// NFC et caractères de format : une même valeur écrite autrement donne le
/// même tag.
#[test]
fn les_noms_de_tags_sont_nfc_et_sans_caractere_de_format_9158() {
    assert_eq!(nettoyer_nom_de_tag("Cafe\u{301}").as_deref(), Some("Café"));
    assert_eq!(
        nettoyer_nom_de_tag("\u{FEFF}Ca\u{200B}lme\u{200E}").as_deref(),
        Some("Calme")
    );
    assert_eq!(nettoyer_nom_de_tag("\u{200B}\u{FEFF}"), None);
    assert_eq!(cle_de_nom("CAFE\u{301}"), cle_de_nom("café"));
    let r = reglage(serde_json::json!({ "mappings": [{ "key": "MOOD" }] }));
    assert_eq!(
        r.tags_des_paires(&p(&[("MOOD", "Café;cafe\u{301};CAFÉ\u{200B}")])),
        vec!["Café".to_string()],
        "doublons sous toutes leurs formes"
    );
}
