//! #5616 (décision de Bertrand du 05/10/2026) — le type de sortie écrit dans
//! les fichiers (`RELEASETYPE` et ses variantes, Picard) est LU au scan.
//!
//! Les épreuves ouvrent de VRAIS fichiers (les gabarits du dépôt), écrits par
//! lofty, et relisent par la fonction de production `read_metadata`.

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
        crate::test_scratch::scratch_file(&format!("type5616-{epreuve}"), &format!("-{nom}"));
    std::fs::copy(&source, &copie).expect("copie du gabarit");
    copie
}

/// Un FLAC dont le bloc Vorbis porte ces champs (un `push` par valeur, comme
/// Picard écrit `RELEASETYPE=album` puis `RELEASETYPE=live`).
fn flac_avec(epreuve: &str, champs: &[(&str, &str)]) -> crate::test_scratch::ScratchFile {
    let chemin = gabarit("test.flac", epreuve);
    let mut fh = std::fs::File::open(&*chemin).expect("ouverture du gabarit");
    let mut flac = FlacFile::read_from(&mut fh, ParseOptions::new()).expect("lecture FLAC");
    drop(fh);
    if flac.vorbis_comments().is_none() {
        flac.set_vorbis_comments(VorbisComments::default());
    }
    let vc = flac.vorbis_comments_mut().expect("bloc Vorbis Comment");
    for (cle, valeur) in champs {
        vc.push((*cle).to_string(), (*valeur).to_string());
    }
    flac.save_to_path(&*chemin, WriteOptions::default())
        .expect("écriture du tag");
    chemin
}

/// Un fichier dont le tag principal reçoit `MusicBrainzReleaseType` par la
/// clé générique : lofty l'écrit `TXXX:MusicBrainz Album Type` en ID3v2 et
/// `----:com.apple.iTunes:MusicBrainz Album Type` en MP4.
fn generique_avec(nom: &str, epreuve: &str, valeur: &str) -> crate::test_scratch::ScratchFile {
    let chemin = gabarit(nom, epreuve);
    let mut fichier = lofty::read_from_path(&chemin).expect("lecture du gabarit");
    let tag = fichier.primary_tag_mut().expect("tag principal du gabarit");
    tag.insert(TagItem::new(
        ItemKey::MusicBrainzReleaseType,
        ItemValue::Text(valeur.into()),
    ));
    tag.save_to_path(&chemin, WriteOptions::default())
        .expect("écriture du tag");
    chemin
}

#[test]
fn un_flac_releasetype_ep_est_lu_5616() {
    let chemin = flac_avec("ep", &[("RELEASETYPE", "ep")]);
    let meta = read_metadata(&chemin).expect("lecture du FLAC");
    assert_eq!(meta.release_type.as_deref(), Some("ep"));
}

#[test]
fn un_flac_album_et_live_en_deux_champs_est_un_album_5616() {
    let chemin = flac_avec("live", &[("RELEASETYPE", "album"), ("RELEASETYPE", "live")]);
    let meta = read_metadata(&chemin).expect("lecture du FLAC");
    assert_eq!(meta.release_type.as_deref(), Some("album"));
}

#[test]
fn un_flac_album_point_virgule_live_est_un_album_5616() {
    let chemin = flac_avec("combine", &[("RELEASETYPE", "single; live")]);
    let meta = read_metadata(&chemin).expect("lecture du FLAC");
    assert_eq!(meta.release_type.as_deref(), Some("single"));
}

/// TÉMOIN — sans balise de type, rien n'est inventé.
#[test]
fn un_flac_sans_balise_de_type_reste_inconnu_5616() {
    let chemin = flac_avec("sans", &[("LABEL", "ECM")]);
    let meta = read_metadata(&chemin).expect("lecture du FLAC");
    assert_eq!(meta.release_type, None);
}

#[test]
fn un_mp3_txxx_musicbrainz_album_type_est_lu_5616() {
    let chemin = generique_avec("test.mp3", "mp3", "single");
    let meta = read_metadata(&chemin).expect("lecture du MP3");
    assert_eq!(meta.release_type.as_deref(), Some("single"));
}

#[test]
fn un_m4a_musicbrainz_album_type_est_lu_5616() {
    let chemin = generique_avec("test.m4a", "m4a", "ep");
    let meta = read_metadata(&chemin).expect("lecture du M4A");
    assert_eq!(meta.release_type.as_deref(), Some("ep"));
}

// ── Section « Live » (Bertrand, 05/10/2026) : les types SECONDAIRES ──────

#[test]
fn un_flac_album_point_virgule_live_porte_le_secondaire_live() {
    let chemin = flac_avec("live-un-champ", &[("RELEASETYPE", "album;live")]);
    let meta = read_metadata(&chemin).expect("lecture du FLAC");
    assert_eq!(meta.release_type.as_deref(), Some("album"));
    assert_eq!(meta.release_secondary_types.as_deref(), Some("live"));
}

#[test]
fn un_flac_album_puis_live_en_deux_champs_porte_le_secondaire_live() {
    let chemin = flac_avec(
        "live-deux-champs",
        &[
            ("RELEASETYPE", "album"),
            ("RELEASETYPE", "live"),
            ("RELEASETYPE", "remix"),
        ],
    );
    let meta = read_metadata(&chemin).expect("lecture du FLAC");
    assert_eq!(meta.release_type.as_deref(), Some("album"));
    assert_eq!(meta.release_secondary_types.as_deref(), Some("live;remix"));
}

#[test]
fn un_mp3_txxx_album_point_virgule_live_porte_le_secondaire_live() {
    let chemin = generique_avec("test.mp3", "mp3-live", "album;live");
    let meta = read_metadata(&chemin).expect("lecture du MP3");
    assert_eq!(meta.release_type.as_deref(), Some("album"));
    assert_eq!(meta.release_secondary_types.as_deref(), Some("live"));
}

#[test]
fn un_m4a_ep_live_porte_le_secondaire_live() {
    let chemin = generique_avec("test.m4a", "m4a-live", "ep; live");
    let meta = read_metadata(&chemin).expect("lecture du M4A");
    assert_eq!(meta.release_type.as_deref(), Some("ep"));
    assert_eq!(meta.release_secondary_types.as_deref(), Some("live"));
}

/// TÉMOIN — un type primaire seul ne fabrique aucun secondaire.
#[test]
fn un_flac_album_seul_n_a_aucun_type_secondaire() {
    let chemin = flac_avec("album-seul", &[("RELEASETYPE", "album")]);
    let meta = read_metadata(&chemin).expect("lecture du FLAC");
    assert_eq!(meta.release_type.as_deref(), Some("album"));
    assert_eq!(meta.release_secondary_types, None);
}
