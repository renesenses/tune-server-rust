//! #6232 (Didier, fil 2218) — « je dois avoir comme résultat tous les albums
//! dans lesquels le nom Genesis apparaît ».
//!
//! Didier retouche dans Mp3tag la balise ARTIST d'UNE piste d'un album des
//! Beatles : « The Beatles & Genesis ». Le scan donne à chaque piste d'un
//! album ordinaire l'artiste de l'ALBUM ; la balise n'atteignait donc aucune
//! colonne, et aucune règle ne pouvait la lire. Elle est désormais rangée dans
//! `track_metadata` sous [`CLE_INTERPRETE_DE_LA_PISTE`].
//!
//! Les épreuves ouvrent de VRAIS fichiers (les gabarits du dépôt), écrits par
//! lofty, et relisent par la fonction de production `read_extended_metadata`.

use super::*;
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::flac::FlacFile;
use lofty::ogg::VorbisComments;
use lofty::tag::{ItemKey, ItemValue, TagExt, TagItem};

fn gabarit(nom: &str, epreuve: &str) -> crate::test_scratch::ScratchFile {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(nom);
    let copie =
        crate::test_scratch::scratch_file(&format!("interprete6232-{epreuve}"), &format!("-{nom}"));
    std::fs::copy(&source, &copie).expect("copie du gabarit");
    copie
}

/// Le cas du fil 2218, sur un MP3 (Mp3tag écrit `TPE1`).
#[test]
fn la_balise_artist_d_un_mp3_est_gardee_6232() {
    let chemin = gabarit("test.mp3", "mp3");
    {
        let mut fichier = lofty::read_from_path(&chemin).expect("lecture du gabarit");
        let tag = fichier.primary_tag_mut().expect("tag ID3v2 du gabarit");
        tag.insert(TagItem::new(
            ItemKey::TrackArtist,
            ItemValue::Text("The Beatles & Genesis".into()),
        ));
        tag.save_to_path(&chemin, WriteOptions::default())
            .expect("écriture du tag");
    }
    let meta = read_extended_metadata(&chemin);
    assert_eq!(
        meta.get(CLE_INTERPRETE_DE_LA_PISTE).map(String::as_str),
        Some("The Beatles & Genesis"),
        "relevé : {meta:?}"
    );
}

/// Un FLAC qui porte DEUX trames ARTIST (un interprète par trame, l'usage
/// Vorbis) : les deux sont gardées, jointes par « ; », comme les crédits.
#[test]
fn toutes_les_trames_artist_d_un_flac_sont_gardees_6232() {
    let chemin = gabarit("test.flac", "flac");
    {
        let p: &Path = &chemin;
        let mut fh = std::fs::File::open(p).expect("ouverture du gabarit");
        let mut flac = FlacFile::read_from(&mut fh, ParseOptions::new()).expect("lecture FLAC");
        drop(fh);
        if flac.vorbis_comments().is_none() {
            flac.set_vorbis_comments(VorbisComments::default());
        }
        let vc = flac.vorbis_comments_mut().expect("bloc Vorbis Comment");
        let _ = vc.remove("ARTIST").count();
        vc.push("ARTIST".to_string(), "The Beatles".to_string());
        vc.push("ARTIST".to_string(), "Genesis".to_string());
        flac.save_to_path(p, WriteOptions::default())
            .expect("écriture du tag");
    }
    let meta = read_extended_metadata(&chemin);
    assert_eq!(
        meta.get(CLE_INTERPRETE_DE_LA_PISTE).map(String::as_str),
        Some("The Beatles; Genesis"),
        "relevé : {meta:?}"
    );
}
