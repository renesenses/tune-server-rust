//! #4836 (Dominique Pamingle, fil 1899) — « mes labels sont sous LABEL (TPUB),
//! mes balises sont bien remplies, mais ne s'affichent pas ».
//!
//! Première moitié du défaut, en amont de l'album : la LECTURE. lofty range la
//! trame ID3v2 `TPUB` sous `ItemKey::Publisher` — sa table déclare
//! `"TPUB" => Publisher | Label`, et la lecture retient la PREMIÈRE variante —
//! et le commentaire Vorbis `PUBLISHER` sous `Publisher` aussi. Le scan ne
//! demandait que `ItemKey::Label` : un MP3 (ou AIFF, WAV+ID3) étiqueté par
//! Mp3tag n'avait donc JAMAIS de label, même sur la piste.
//!
//! Les épreuves ouvrent de VRAIS fichiers (les gabarits du dépôt), écrits par
//! lofty, et relisent par les fonctions de production `read_metadata` et
//! `read_extended_metadata`.

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
    // L'extension reste en dernier : lofty choisit son analyseur dessus.
    let copie =
        crate::test_scratch::scratch_file(&format!("label4836-{epreuve}"), &format!("-{nom}"));
    std::fs::copy(&source, &copie).expect("copie du gabarit");
    copie
}

/// Le cas du fil 1899 : un MP3 dont le label est dans `TPUB`.
#[test]
fn un_mp3_porte_son_label_tpub_jusqu_a_la_piste_4836() {
    let chemin = gabarit("test.mp3", "tpub");
    {
        let mut fichier = lofty::read_from_path(&chemin).expect("lecture du gabarit");
        let tag = fichier.primary_tag_mut().expect("tag ID3v2 du gabarit");
        // `ItemKey::Label` s'écrit `TPUB` en ID3v2 (table de lofty) — c'est
        // aussi ce qu'écrit l'éditeur de tags de Tune (`tag_writer`).
        tag.insert(TagItem::new(
            ItemKey::Label,
            ItemValue::Text("Nuclear Blast".into()),
        ));
        tag.save_to_path(&chemin, WriteOptions::default())
            .expect("écriture du tag");
    }
    // Témoin de montage : la trame écrite est bien `TPUB`, relue par le
    // lecteur ID3v2 de Tune lui-même, pas par lofty.
    let brut = read_dsf_id3v2_raw(&chemin, Some(0), Id3ReadSite::LeadingProbe, false)
        .and_then(|raw| parse_id3v2_tag(&raw));
    assert_eq!(
        brut.as_ref().and_then(|t| t.label()),
        Some("Nuclear Blast"),
        "montage : le gabarit doit porter une trame TPUB"
    );

    let meta = read_metadata(&chemin).expect("lecture du MP3");
    assert_eq!(
        meta.label.as_deref(),
        Some("Nuclear Blast"),
        "#4836 — un label étiqueté TPUB doit atteindre `tracks.label`"
    );
    let etendu = read_extended_metadata(&chemin);
    assert_eq!(
        etendu.get("label").map(String::as_str),
        Some("Nuclear Blast"),
        "#4836 — la fiche étendue de la piste lit le même label"
    );
}

fn flac_avec(epreuve: &str, cle: &str, valeur: &str) -> crate::test_scratch::ScratchFile {
    let chemin = gabarit("test.flac", epreuve);
    let mut fh = std::fs::File::open(&*chemin).expect("ouverture du gabarit");
    let mut flac = FlacFile::read_from(&mut fh, ParseOptions::new()).expect("lecture FLAC");
    drop(fh);
    if flac.vorbis_comments().is_none() {
        flac.set_vorbis_comments(VorbisComments::default());
    }
    flac.vorbis_comments_mut()
        .expect("bloc Vorbis Comment")
        .insert(cle.to_string(), valeur.to_string());
    flac.save_to_path(&*chemin, WriteOptions::default())
        .expect("écriture du tag");
    chemin
}

/// Un FLAC dont l'étiqueteur écrit `PUBLISHER` (Mp3tag, champ « Publisher »).
#[test]
fn un_flac_porte_son_label_publisher_jusqu_a_la_piste_4836() {
    let chemin = flac_avec("publisher", "PUBLISHER", "ECM Records");
    let meta = read_metadata(&chemin).expect("lecture du FLAC");
    assert_eq!(
        meta.label.as_deref(),
        Some("ECM Records"),
        "#4836 — `PUBLISHER` est un label : il doit atteindre `tracks.label`"
    );
}

/// TÉMOIN VERT — `LABEL` en Vorbis était déjà lu, et le reste. Et quand les
/// deux sont présents, c'est `LABEL` qui l'emporte : le repli ne doit pas
/// déloger une valeur explicitement étiquetée comme label.
#[test]
fn un_flac_label_reste_lu_et_prime_sur_publisher_4836() {
    let chemin = flac_avec("label", "LABEL", "Blue Note");
    let meta = read_metadata(&chemin).expect("lecture du FLAC");
    assert_eq!(meta.label.as_deref(), Some("Blue Note"));

    let mut fh = std::fs::File::open(&*chemin).unwrap();
    let mut flac = FlacFile::read_from(&mut fh, ParseOptions::new()).unwrap();
    drop(fh);
    flac.vorbis_comments_mut()
        .unwrap()
        .insert("PUBLISHER".to_string(), "Universal".to_string());
    flac.save_to_path(&*chemin, WriteOptions::default())
        .unwrap();
    let meta = read_metadata(&chemin).expect("lecture du FLAC");
    assert_eq!(
        meta.label.as_deref(),
        Some("Blue Note"),
        "LABEL prime sur PUBLISHER"
    );
}
