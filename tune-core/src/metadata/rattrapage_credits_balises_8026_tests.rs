//! #8026 — témoins SQLite du rattrapage des crédits des balises et du
//! rattachement des lignes `balises` à leur fiche. Jumeau PostgreSQL :
//! `tune-server/tests/pg_credits_balises_8026.rs`.

use std::sync::Arc;

use super::*;
use crate::db::backend::{DbBackend, ToSqlValue};

/// Une base de scan d'AVANT #7766 : des métadonnées étendues, aucune ligne
/// `balises`. La piste 8 a déjà ses lignes `balises` (scannée depuis).
fn base() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    db.execute_batch(
        "INSERT INTO artists (id, name) VALUES (1, 'Bill Evans');
         INSERT INTO artists (id, name) VALUES (2, 'Günter Hermanns');
         INSERT INTO artists (id, name) VALUES (3, 'Björk');
         INSERT INTO artists (id, name) VALUES (4, 'Bjork');
         INSERT INTO albums (id, title, artist_id) VALUES (1, 'Undercurrent', 1);
         INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, disc_number, track_number, channels)
         VALUES (7, 'My Funny Valentine', 1, 1, 300000, 1, 1, 2);
         INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, disc_number, track_number, channels)
         VALUES (8, 'Dream Gypsy', 1, 1, 300000, 1, 2, 2);
         INSERT INTO tracks (id, title, album_id, artist_id, duration_ms, disc_number, track_number, channels)
         VALUES (9, 'Romain', 1, 1, 300000, 1, 3, 2);
         INSERT INTO track_metadata (track_id, key, value) VALUES (7, 'composer', 'Richard Rodgers');
         INSERT INTO track_metadata (track_id, key, value) VALUES (7, 'performer', 'Bill Evans (piano); Jim Hall (guitar)');
         INSERT INTO track_metadata (track_id, key, value) VALUES (7, 'engineer', 'gunter hermanns');
         INSERT INTO track_metadata (track_id, key, value) VALUES (7, 'label', 'United Artists');
         INSERT INTO track_metadata (track_id, key, value) VALUES (8, 'producer', 'Alan Douglas');
         INSERT INTO track_metadata (track_id, key, value) VALUES (9, 'rg_track_gain', '-6.1');
         INSERT INTO track_credits (track_id, artist_name, role, position, source)
         VALUES (7, 'Alan Douglas', 'producer', 0, 'musicbrainz');
         INSERT INTO track_credits (track_id, artist_name, role, position, source)
         VALUES (8, 'Déjà Là', 'producer', 0, 'balises');",
    )
    .unwrap();
    Arc::new(db)
}

fn lignes(b: &Arc<dyn DbBackend>, track_id: i64) -> Vec<String> {
    let mut v: Vec<String> = b
        .query_many(
            "SELECT COALESCE(source, 'NULL'), role, artist_name, COALESCE(instrument, ''), \
                    COALESCE(CAST(artist_id AS TEXT), '-') \
             FROM track_credits WHERE track_id = ?",
            &[&track_id as &dyn ToSqlValue],
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

/// TÉMOIN ROUGE avant #8026 : une piste scannée avant #7766 n'avait aucune
/// ligne `balises` tant que son fichier ne changeait pas.
#[test]
fn la_musique_deja_scannee_recoit_ses_credits_des_balises_8026() {
    let b = base();
    let bilan = rattraper_les_credits_des_balises(&b).unwrap();
    assert_eq!(
        lignes(&b, 7),
        vec![
            "balises|composer|Richard Rodgers||-",
            "balises|engineer|gunter hermanns||-",
            "balises|performer|Bill Evans|piano|-",
            "balises|performer|Jim Hall|guitar|-",
            "musicbrainz|producer|Alan Douglas||-",
        ],
        "#8026 — la piste déjà scannée n'a pas reçu ses crédits de balises"
    );
    assert_eq!(
        lignes(&b, 8),
        vec!["balises|producer|Déjà Là||-"],
        "une piste qui a déjà ses lignes `balises` n'est pas réécrite"
    );
    assert!(lignes(&b, 9).is_empty(), "rien à écrire sans clé de crédit");
    assert_eq!(
        bilan,
        Bilan {
            deja_fait: false,
            pistes: 1,
            lignes: 4
        }
    );

    // Le marqueur est posé : la passe ne se rejoue pas.
    b.execute(
        "DELETE FROM track_credits WHERE track_id = 7 AND source = 'balises'",
        &[],
    )
    .unwrap();
    let encore = rattraper_les_credits_des_balises(&b).unwrap();
    assert!(encore.deja_fait, "{encore:?}");
    assert_eq!(lignes(&b, 7), vec!["musicbrainz|producer|Alan Douglas||-"]);
}

/// TÉMOIN ROUGE avant #8026 : la page artiste ne retrouvait une ligne
/// `balises` que par le nom IDENTIQUE à celui de la fiche.
#[test]
fn une_ligne_des_balises_rejoint_sa_fiche_sans_casse_ni_accents_8026() {
    let b = base();
    b.execute_batch(
        "INSERT INTO track_credits (track_id, artist_name, role, position, source)
         VALUES (9, 'bjork', 'performer', 0, 'balises');
         INSERT INTO track_credits (track_id, artist_name, role, position, source)
         VALUES (9, 'Inconnu Total', 'performer', 1, 'balises');
         INSERT INTO track_credits (track_id, artist_name, role, position, source)
         VALUES (9, 'gunter hermanns', 'producer', 2, 'musicbrainz');",
    )
    .unwrap();
    rattraper_les_credits_des_balises(&b).unwrap();
    let n = rattacher_les_fiches(&b).unwrap();
    assert_eq!(
        lignes(&b, 7),
        vec![
            "balises|composer|Richard Rodgers||-",
            "balises|engineer|gunter hermanns||2",
            "balises|performer|Bill Evans|piano|1",
            "balises|performer|Jim Hall|guitar|-",
            "musicbrainz|producer|Alan Douglas||-",
        ],
        "#8026 — « gunter hermanns » ne rejoint pas la fiche « Günter Hermanns »"
    );
    assert_eq!(
        lignes(&b, 9),
        vec![
            "balises|performer|Inconnu Total||-",
            // Deux fiches « Björk » et « Bjork » portent la clé de « bjork » :
            // aucune n'est choisie.
            "balises|performer|bjork||-",
            // Une autre source n'est jamais touchée.
            "musicbrainz|producer|gunter hermanns||-",
        ],
    );
    assert_eq!(n, 2);
    // Idempotente : rien de plus au second passage.
    assert_eq!(rattacher_les_fiches(&b).unwrap(), 0);
}

#[test]
fn le_nom_identique_l_emporte_sur_la_cle_8026() {
    let b = base();
    b.execute(
        "INSERT INTO track_credits (track_id, artist_name, role, position, source) \
         VALUES (9, 'Bjork', 'performer', 0, 'balises')",
        &[],
    )
    .unwrap();
    rattacher_les_fiches(&b).unwrap();
    assert_eq!(lignes(&b, 9), vec!["balises|performer|Bjork||4"]);
}

/// #8026 — TÉMOIN ROUGE avant le correctif : un FLAC qui cite deux
/// orchestres en deux trames ENSEMBLE (et une ORCHESTRA) ne créditait que la
/// première, `find_vorbis_comment` s'arrêtant à la première trame.
#[test]
fn un_flac_a_plusieurs_ensembles_les_credite_tous_8026() {
    use lofty::config::{ParseOptions, WriteOptions};
    use lofty::file::AudioFile;
    use lofty::flac::FlacFile;
    use lofty::ogg::VorbisComments;
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test.flac");
    let chemin = crate::test_scratch::scratch_file("credits8026-ensembles", ".flac");
    std::fs::copy(&source, &*chemin).expect("copie de la fixture");
    let mut fh = std::fs::File::open(&*chemin).unwrap();
    let mut flac = FlacFile::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    if flac.vorbis_comments().is_none() {
        flac.set_vorbis_comments(VorbisComments::default());
    }
    let vc = flac.vorbis_comments_mut().unwrap();
    for (k, v) in [
        ("TITLE", "Gloria"),
        ("ENSEMBLE", "Les Arts Florissants"),
        ("ENSEMBLE", "Maîtrise de Radio France"),
        ("ORCHESTRA", "Orchestre de chambre de Paris"),
        ("ORCHESTRA", "Les Arts Florissants"),
    ] {
        vc.push(k.to_string(), v.to_string());
    }
    flac.save_to_path(&*chemin, WriteOptions::default())
        .unwrap();

    let meta = crate::metadata::read_extended_metadata(&chemin);
    let mut ensembles: Vec<String> = credits_balises::lignes_des_balises(&meta)
        .into_iter()
        .filter(|l| l.role == "performer")
        .map(|l| l.artist_name)
        .collect();
    ensembles.sort();
    assert_eq!(
        ensembles,
        vec![
            "Les Arts Florissants",
            "Maîtrise de Radio France",
            "Orchestre de chambre de Paris",
        ],
        "#8026 — un ensemble cité dans une seconde trame est perdu : {meta:?}"
    );
}
