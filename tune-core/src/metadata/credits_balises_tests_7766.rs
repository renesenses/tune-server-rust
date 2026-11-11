//! #7766 — témoins des crédits lus dans les balises : la conversion pure,
//! puis de VRAIS conteneurs (FLAC, MP3, M4A) copiés des fixtures et balisés
//! par lofty, relus par la lecture du scan, puis l'écriture en base qui ne
//! touche qu'aux lignes `balises`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;

use super::credits_balises::{
    completer_par_les_cles_brutes, ecrire_lot, lignes_des_balises, nom_et_instrument,
};
use super::credits_mb::LigneCredit;
use crate::db::backend::{DbBackend, ToSqlValue};

fn m(paires: &[(&str, &str)]) -> HashMap<String, String> {
    paires
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Les crédits sous la forme « rôle|nom|instrument », triés : l'ordre
/// d'écriture n'est pas ce que ces témoins gardent.
fn triplets(lignes: &[LigneCredit]) -> Vec<String> {
    let mut v: Vec<String> = lignes
        .iter()
        .map(|l| {
            format!(
                "{}|{}|{}",
                l.role,
                l.artist_name,
                l.instrument.as_deref().unwrap_or("")
            )
        })
        .collect();
    v.sort();
    v
}

#[test]
fn le_nom_et_l_instrument_se_coupent_a_la_derniere_parenthese() {
    assert_eq!(
        nom_et_instrument(" Bill Evans (piano) "),
        ("Bill Evans".into(), Some("piano".into()))
    );
    assert_eq!(
        nom_et_instrument("Orchestre (de chambre) de Paris (violon)"),
        (
            "Orchestre (de chambre) de Paris".into(),
            Some("violon".into())
        )
    );
    assert_eq!(
        nom_et_instrument("Miles Davis"),
        ("Miles Davis".into(), None)
    );
    assert_eq!(nom_et_instrument("(piano)"), ("(piano)".into(), None));
}

#[test]
fn chaque_cle_de_credit_devient_une_ligne_de_son_role_7766() {
    let lignes = lignes_des_balises(&m(&[
        ("composer", "Johann Sebastian Bach; Antonio Vivaldi"),
        ("conductor", "Herbert von Karajan"),
        ("ensemble", "Berliner Philharmoniker"),
        (
            "performer",
            "Bill Evans (grand piano); Paul Chambers (double bass)",
        ),
        ("producer", "Teo Macero"),
        ("engineer", "Rudy Van Gelder"),
        ("mixer", "Bob Clearmountain"),
        ("djmixer", "Ben Watt"),
        ("arranger", "Gil Evans"),
        ("lyricist", "Boris Vian"),
        ("writer", "Boris Vian"),
        ("remixer", "Four Tet"),
        // Pas une clé de crédit : ignorée.
        ("label", "Columbia"),
    ]));
    assert_eq!(
        triplets(&lignes),
        vec![
            "arranger|Gil Evans|",
            "composer|Antonio Vivaldi|",
            "composer|Johann Sebastian Bach|",
            "conductor|Herbert von Karajan|",
            "engineer|Rudy Van Gelder|",
            "mixer|Ben Watt|",
            "mixer|Bob Clearmountain|",
            "performer|Berliner Philharmoniker|",
            "performer|Bill Evans|piano",
            "performer|Paul Chambers|bass",
            "producer|Teo Macero|",
            "remixer|Four Tet|",
            // Parolier et auteur : un seul crédit `writer` (ID3 TEXT).
            "writer|Boris Vian|",
        ],
        "{lignes:?}"
    );
}

#[test]
fn ni_various_artists_ni_coupure_sur_l_esperluette_7766() {
    let lignes = lignes_des_balises(&m(&[
        (
            "composer",
            "Various Artists; Simon & Garfunkel;  ; Simon & Garfunkel",
        ),
        ("producer", "SIMON & GARFUNKEL"),
    ]));
    assert_eq!(
        triplets(&lignes),
        vec!["composer|Simon & Garfunkel|", "producer|SIMON & GARFUNKEL|"],
        "{lignes:?}"
    );
}

/// « A; B; B & A » : deux personnes, la troisième entrée ne fait que les
/// réunir. Le découpage est celui d'`artist_split` (« feat. » coupe aussi).
#[test]
fn une_reformulation_des_personnes_citees_n_est_pas_un_credit_7766() {
    let lignes = lignes_des_balises(&m(&[
        (
            "composer",
            "John Lennon; Paul McCartney; Paul McCartney & John Lennon",
        ),
        (
            "performer",
            "Ella Fitzgerald (vocals) feat. Louis Armstrong (trumpet)",
        ),
        ("producer", "Simon & Garfunkel; Roy Halee"),
    ]));
    assert_eq!(
        triplets(&lignes),
        vec![
            "composer|John Lennon|",
            "composer|Paul McCartney|",
            "performer|Ella Fitzgerald|vocals",
            "performer|Louis Armstrong|trumpet",
            "producer|Roy Halee|",
            "producer|Simon & Garfunkel|",
        ],
        "#7766 — « A; B; B & A » doit donner deux personnes : {lignes:?}"
    );
}

#[test]
fn la_tmcl_et_l_ensemble_bruts_completent_les_cles_7766() {
    let mut meta = m(&[("performer", "Jim Hall (guitar)")]);
    completer_par_les_cles_brutes(
        &mut meta,
        &[
            ("TMCL".into(), "guitar: Jim Hall ; piano: Bill Evans".into()),
            ("TXXX:ORCHESTRA".into(), "London Symphony Orchestra".into()),
            (
                "----:com.apple.iTunes:ARRANGER".into(),
                "Quincy Jones".into(),
            ),
            // Un ARRANGER Vorbis est déjà lu par lofty : pas de second passage.
            ("ARRANGER".into(), "Quelqu'un d'autre".into()),
        ],
    );
    assert_eq!(
        meta.get("performer").map(String::as_str),
        Some("Jim Hall (guitar); Bill Evans (piano)")
    );
    assert_eq!(
        meta.get("ensemble").map(String::as_str),
        Some("London Symphony Orchestra")
    );
    assert_eq!(
        meta.get("arranger").map(String::as_str),
        Some("Quincy Jones")
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
        crate::test_scratch::scratch_file(&format!("credits7766-{epreuve}"), &format!(".{ext}"));
    std::fs::copy(&source, &cible).expect("copie de la fixture");
    cible
}

#[test]
fn un_flac_rend_tous_ses_credits_au_scan_7766() {
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
    for (k, v) in [
        ("TITLE", "Concerto"),
        ("ARTIST", "Berliner Philharmoniker"),
        ("COMPOSER", "Johann Sebastian Bach"),
        ("COMPOSER", "Antonio Vivaldi"),
        ("CONDUCTOR", "Herbert von Karajan"),
        ("ENSEMBLE", "Berliner Philharmoniker"),
        ("PERFORMER", "Anne-Sophie Mutter (violin)"),
        ("PERFORMER", "Yo-Yo Ma (cello)"),
        ("PRODUCER", "Michel Glotz"),
        ("ENGINEER", "Günter Hermanns"),
        ("ARRANGER", "Ottorino Respighi"),
        ("MIXER", "Klaus Hiemann"),
    ] {
        vc.push(k.to_string(), v.to_string());
    }
    flac.save_to_path(&*chemin, WriteOptions::default())
        .unwrap();

    let meta = super::read_extended_metadata(&chemin);
    assert_eq!(
        triplets(&lignes_des_balises(&meta)),
        vec![
            "arranger|Ottorino Respighi|",
            "composer|Antonio Vivaldi|",
            "composer|Johann Sebastian Bach|",
            "conductor|Herbert von Karajan|",
            "engineer|Günter Hermanns|",
            "mixer|Klaus Hiemann|",
            "performer|Anne-Sophie Mutter|violin",
            "performer|Berliner Philharmoniker|",
            "performer|Yo-Yo Ma|cello",
            "producer|Michel Glotz|",
        ],
        "#7766 — crédits du FLAC perdus ou mal lus : {meta:?}"
    );
}

#[test]
fn un_mp3_rend_sa_tipl_et_sa_tmcl_au_scan_7766() {
    use lofty::TextEncoding;
    use lofty::id3::v2::{Frame, FrameId, Id3v2Tag, KeyValueFrame, TextInformationFrame};
    use lofty::mpeg::MpegFile;
    use std::borrow::Cow;
    let chemin = copie("test.mp3", "mp3");
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
    let paires = |id: &'static str, p: &[(&str, &str)]| {
        Frame::KeyValue(KeyValueFrame::new(
            FrameId::Valid(Cow::Borrowed(id)),
            TextEncoding::UTF8,
            p.iter()
                .map(|(k, v)| (Cow::Owned(k.to_string()), Cow::Owned(v.to_string())))
                .collect(),
        ))
    };
    tag.insert(texte("TIT2", "Undercurrent"));
    tag.insert(texte("TPE1", "Bill Evans"));
    tag.insert(texte("TCOM", "Johnny Green"));
    tag.insert(texte("TPE3", "Gil Evans"));
    tag.insert(paires(
        "TIPL",
        &[("producer", "Alan Douglas"), ("engineer", "Phil Ramone")],
    ));
    tag.insert(paires(
        "TMCL",
        &[("guitar", "Jim Hall"), ("piano", "Bill Evans")],
    ));
    mp3.set_id3v2(tag);
    mp3.save_to_path(&*chemin, WriteOptions::default()).unwrap();

    let meta = super::read_extended_metadata(&chemin);
    assert_eq!(
        triplets(&lignes_des_balises(&meta)),
        vec![
            "composer|Johnny Green|",
            "conductor|Gil Evans|",
            "engineer|Phil Ramone|",
            "performer|Bill Evans|piano",
            "performer|Jim Hall|guitar",
            "producer|Alan Douglas|",
        ],
        "#7766 — crédits ID3 (TCOM, TPE3, TIPL, TMCL) perdus : {meta:?}"
    );
}

#[test]
fn un_m4a_rend_ses_atomes_de_credit_au_scan_7766() {
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
    let code = |c: [u8; 4], v: &str| Atom::new(AtomIdent::Fourcc(c), AtomData::UTF8(v.to_string()));
    ilst.insert(code(*b"\xa9nam", "Thriller"));
    ilst.insert(code(*b"\xa9ART", "Michael Jackson"));
    ilst.insert(code(*b"\xa9wrt", "Rod Temperton"));
    ilst.insert(libre("PRODUCER", "Quincy Jones"));
    ilst.insert(libre("ENGINEER", "Bruce Swedien"));
    ilst.insert(libre("CONDUCTOR", "Jerry Hey"));
    ilst.insert(libre("ARRANGER", "Jerry Hey"));
    m4a.set_ilst(ilst);
    m4a.save_to_path(&*chemin, WriteOptions::default()).unwrap();

    let meta = super::read_extended_metadata(&chemin);
    assert_eq!(
        triplets(&lignes_des_balises(&meta)),
        vec![
            "arranger|Jerry Hey|",
            "composer|Rod Temperton|",
            "conductor|Jerry Hey|",
            "engineer|Bruce Swedien|",
            "producer|Quincy Jones|",
        ],
        "#7766 — crédits MP4 (©wrt, atomes libres) perdus : {meta:?}"
    );
}

// ---------------------------------------------------------------------------
// Écriture en base.
// ---------------------------------------------------------------------------

fn base() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    db.execute_batch(
        "INSERT INTO artists (id, name) VALUES (1, 'Bill Evans');
         INSERT INTO albums (id, title, artist_id) VALUES (1, 'Undercurrent', 1);
         INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, disc_number, track_number, channels)
         VALUES (7, 'My Funny Valentine', 1, 1, 300000, 1, 1, 2);
         INSERT INTO track_credits (track_id, artist_name, role, position, source)
         VALUES (7, 'Alan Douglas', 'producer', 0, 'musicbrainz');
         INSERT INTO track_credits (track_id, artist_name, role, position)
         VALUES (7, 'Ancien Crédit', 'engineer', 1);",
    )
    .unwrap();
    Arc::new(db)
}

fn lignes_en_base(b: &Arc<dyn DbBackend>) -> Vec<String> {
    let mut v: Vec<String> = b
        .query_many(
            "SELECT COALESCE(source, 'NULL'), role, artist_name, COALESCE(instrument, '') \
             FROM track_credits WHERE track_id = ?",
            &[&7i64 as &dyn ToSqlValue],
        )
        .unwrap()
        .into_iter()
        .map(|r| {
            r.iter()
                .map(|c| c.as_string().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect();
    v.sort();
    v
}

#[test]
fn le_scan_ecrit_ses_lignes_sans_toucher_aux_autres_sources_7766() {
    let b = base();
    let lot = vec![(
        7i64,
        m(&[
            ("composer", "Richard Rodgers"),
            ("performer", "Bill Evans (piano); Jim Hall (guitar)"),
            ("producer", "Alan Douglas"),
        ]),
    )];
    assert_eq!(ecrire_lot(&b, &lot), 4);
    let apres_un = lignes_en_base(&b);
    assert_eq!(
        apres_un,
        vec![
            "NULL|engineer|Ancien Crédit|",
            "balises|composer|Richard Rodgers|",
            "balises|performer|Bill Evans|piano",
            "balises|performer|Jim Hall|guitar",
            "balises|producer|Alan Douglas|",
            "musicbrainz|producer|Alan Douglas|",
        ],
        "#7766 — les lignes balises manquent, ou une autre source a été touchée"
    );

    // Un second scan du même fichier : exactement les mêmes lignes.
    ecrire_lot(&b, &lot);
    assert_eq!(
        lignes_en_base(&b),
        apres_un,
        "#7766 — le second scan double les crédits"
    );

    // Le fichier perd son producteur : seule la ligne balises s'en va.
    ecrire_lot(&b, &[(7, m(&[("composer", "Richard Rodgers")]))]);
    assert_eq!(
        lignes_en_base(&b),
        vec![
            "NULL|engineer|Ancien Crédit|",
            "balises|composer|Richard Rodgers|",
            "musicbrainz|producer|Alan Douglas|",
        ]
    );
}

/// #7766 — la lecture des pistes (`parse_credits`) coupe « Nom (instrument) »
/// comme l'écriture au scan, à la DERNIÈRE parenthèse : elle coupait à la
/// première et rendait « Orchestre » joué au « de chambre) de Paris (violon ».
#[test]
fn la_lecture_des_pistes_coupe_l_instrument_comme_le_scan_7766() {
    use lofty::tag::{ItemKey, ItemValue, Tag, TagItem, TagType};
    let mut tag = Tag::new(TagType::VorbisComments);
    tag.push(TagItem::new(
        ItemKey::Performer,
        ItemValue::Text("Orchestre (de chambre) de Paris (violon)".into()),
    ));
    let credits = super::parse_credits(&tag);
    let lus: Vec<(String, Option<String>)> = credits
        .into_iter()
        .filter(|c| c.role == "performer")
        .map(|c| (c.name, c.instrument))
        .collect();
    assert_eq!(
        lus,
        vec![(
            "Orchestre (de chambre) de Paris".to_string(),
            Some("violon".to_string())
        )],
        "#7766 — `parse_credits` ne découpe pas comme `nom_et_instrument`"
    );
}

/// #8325 — BAND est l'orchestre d'un classique, mais l'artiste d'album chez
/// foobar2000 / Mp3tag (TPE2) : il n'entre dans `ensemble` que s'il diffère
/// de l'artiste d'album, qui n'est jamais touché.
#[test]
fn band_n_est_un_ensemble_que_s_il_n_est_pas_l_artiste_d_album_8325() {
    use super::credits_balises::completer_par_band;
    let mut meta = m(&[]);
    completer_par_band(
        &mut meta,
        &[("BAND".into(), "Wiener Philharmoniker".into())],
        &[Some("Carlos Kleiber"), Some("Carlos Kleiber")],
    );
    assert_eq!(
        meta.get("ensemble").map(String::as_str),
        Some("Wiener Philharmoniker")
    );

    let mut meta = m(&[("ensemble", "Wiener Singverein")]);
    completer_par_band(
        &mut meta,
        &[("TXXX:band".into(), "Berliner Philharmoniker".into())],
        &[None, Some("Herbert von Karajan")],
    );
    assert_eq!(
        meta.get("ensemble").map(String::as_str),
        Some("Wiener Singverein; Berliner Philharmoniker"),
        "sans artiste d'album, BAND complète l'ensemble"
    );

    let mut meta = m(&[]);
    completer_par_band(
        &mut meta,
        &[("BAND".into(), "the beatles".into())],
        &[Some("The Beatles"), None],
    );
    assert!(
        !meta.contains_key("ensemble"),
        "BAND égal à l'artiste d'album n'est pas un ensemble : {meta:?}"
    );

    // Un disque de rock balisé BAND sans ALBUMARTIST : BAND est l'artiste
    // de la piste, pas un ensemble.
    let mut meta = m(&[]);
    completer_par_band(
        &mut meta,
        &[("BAND".into(), "Pink Floyd".into())],
        &[None, Some("Pink Floyd")],
    );
    assert!(
        !meta.contains_key("ensemble"),
        "BAND égal à l'artiste de la piste n'est pas un ensemble : {meta:?}"
    );
    assert!(!meta.contains_key("album_artist"));
}
