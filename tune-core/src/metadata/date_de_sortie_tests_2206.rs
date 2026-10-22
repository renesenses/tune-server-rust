//! web#2206 (Jean Valjean, fil 2300) — « Serait-il possible d'avoir un tri
//! par date de sortie dans la bibliothèque pour l'onglet Albums ? »
//!
//! Le tri existait côté serveur et à l'écran, mais `release_date` n'était lu
//! que dans RELEASEDATE / TDRL — balises que presque personne n'écrit : 0
//! album sur 4 255 en portait une sur le .18. La date complète vit d'ordinaire
//! dans DATE (Vorbis) / TDRC (ID3v2.4), où MusicBrainz Picard écrit la date
//! de sortie de l'édition ; seule son année en était retenue. Elle sert
//! désormais de repli quand elle porte au moins un mois.
//!
//! Les épreuves ouvrent de VRAIS fichiers (les gabarits du dépôt), écrits par
//! lofty, et relisent par la fonction de production `read_metadata`.

use super::*;
use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::tag::{ItemKey, ItemValue, TagExt, TagItem};

fn gabarit(nom: &str, epreuve: &str) -> crate::test_scratch::ScratchFile {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(nom);
    let copie =
        crate::test_scratch::scratch_file(&format!("sortie2206-{epreuve}"), &format!("-{nom}"));
    std::fs::copy(&source, &copie).expect("copie du gabarit");
    copie
}

/// Écrit les balises données (et retire les dates du gabarit).
fn baliser(chemin: &Path, balises: &[(ItemKey, &str)]) {
    let mut fichier = lofty::read_from_path(chemin).expect("lecture du gabarit");
    let tag = match fichier.primary_tag_mut() {
        Some(t) => t,
        None => {
            let ty = fichier.primary_tag_type();
            fichier.insert_tag(lofty::tag::Tag::new(ty));
            fichier.primary_tag_mut().expect("tag")
        }
    };
    for k in [ItemKey::RecordingDate, ItemKey::ReleaseDate] {
        tag.remove_key(k);
    }
    for (k, v) in balises {
        tag.insert(TagItem::new(*k, ItemValue::Text((*v).into())));
    }
    tag.save_to_path(chemin, WriteOptions::default())
        .expect("écriture du tag");
}

#[test]
fn la_date_complete_de_date_devient_la_date_de_sortie_flac_2206() {
    let chemin = gabarit("test.flac", "flac");
    baliser(&chemin, &[(ItemKey::RecordingDate, "1975-09-12")]);
    let m = read_metadata(&chemin).expect("lecture");
    assert_eq!(m.release_date.as_deref(), Some("1975-09-12"));
    assert_eq!(m.year, Some(1975), "l'année reste lue");
}

#[test]
fn la_date_complete_de_tdrc_devient_la_date_de_sortie_mp3_2206() {
    let chemin = gabarit("test.mp3", "mp3");
    baliser(&chemin, &[(ItemKey::RecordingDate, "1975-09")]);
    let m = read_metadata(&chemin).expect("lecture");
    assert_eq!(m.release_date.as_deref(), Some("1975-09"));
}

/// TÉMOIN — une année seule n'invente pas de date complète.
#[test]
fn une_annee_seule_ne_donne_pas_de_date_de_sortie_2206() {
    let chemin = gabarit("test.flac", "annee");
    baliser(&chemin, &[(ItemKey::RecordingDate, "1975")]);
    let m = read_metadata(&chemin).expect("lecture");
    assert_eq!(m.release_date, None);
    assert_eq!(m.year, Some(1975));
}

/// RELEASEDATE, quand elle est écrite, garde la priorité.
#[test]
fn releasedate_garde_la_priorite_2206() {
    let chemin = gabarit("test.flac", "priorite");
    baliser(
        &chemin,
        &[
            (ItemKey::RecordingDate, "1975-09-12"),
            (ItemKey::ReleaseDate, "2011-09-26"),
        ],
    );
    let m = read_metadata(&chemin).expect("lecture");
    assert_eq!(m.release_date.as_deref(), Some("2011-09-26"));
}

#[test]
fn date_precise_ne_rend_que_les_dates_a_mois_2206() {
    use super::balises_brutes::date_precise;
    assert_eq!(date_precise("1975-09-12").as_deref(), Some("1975-09-12"));
    assert_eq!(date_precise(" 1975-09 ").as_deref(), Some("1975-09"));
    assert_eq!(
        date_precise("1975-09-12T07:00:00Z").as_deref(),
        Some("1975-09-12")
    );
    assert_eq!(date_precise("1975"), None);
    assert_eq!(date_precise("1975-13"), None);
    assert_eq!(date_precise("0000-01-01"), None);
    assert_eq!(date_precise("75-09-12"), None);
    assert_eq!(date_precise(""), None);
    // Un jour invalide garde le mois, sans inventer de jour.
    assert_eq!(date_precise("1975-09-00").as_deref(), Some("1975-09"));
}
