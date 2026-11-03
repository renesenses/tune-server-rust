//! #6474 (René, fil 2313) — ARTISTSORT est lu dans `TrackMetadata` : c'est
//! lui qui remplit le nom de tri de l'artiste de la piste au scan. Épreuves
//! sur de vrais fichiers (gabarits du dépôt), relus par `read_metadata`.

use super::*;
use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::tag::{ItemKey, ItemValue, TagExt, TagItem};

fn gabarit(nom: &str, epreuve: &str) -> crate::test_scratch::ScratchFile {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(nom);
    let copie =
        crate::test_scratch::scratch_file(&format!("tri6474-{epreuve}"), &format!("-{nom}"));
    std::fs::copy(&source, &copie).expect("copie du gabarit");
    copie
}

fn avec_artistsort(chemin: &Path, valeur: &str) {
    let mut fichier = lofty::read_from_path(chemin).expect("lecture du gabarit");
    let tag = fichier.primary_tag_mut().expect("tag du gabarit");
    tag.insert(TagItem::new(
        ItemKey::TrackArtistSortOrder,
        ItemValue::Text(valeur.into()),
    ));
    tag.save_to_path(chemin, WriteOptions::default())
        .expect("écriture du tag");
}

#[test]
fn artistsort_d_un_flac_est_lu_6474() {
    let chemin = gabarit("test.flac", "flac");
    avec_artistsort(&chemin, "Davis, Miles");
    let m = read_metadata(&chemin).expect("lecture");
    assert_eq!(m.artist_sort.as_deref(), Some("Davis, Miles"));
}

#[test]
fn tsop_d_un_mp3_est_lu_6474() {
    let chemin = gabarit("test.mp3", "mp3");
    avec_artistsort(&chemin, "Bowie, David");
    let m = read_metadata(&chemin).expect("lecture");
    assert_eq!(m.artist_sort.as_deref(), Some("Bowie, David"));
}

/// TÉMOIN — sans la balise, rien n'est inventé.
#[test]
fn sans_artistsort_aucun_nom_de_tri_6474() {
    let chemin = gabarit("test.flac", "sans");
    let m = read_metadata(&chemin).expect("lecture");
    assert_eq!(m.artist_sort, None);
}
