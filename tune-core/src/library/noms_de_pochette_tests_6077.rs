//! #6077 — pochettes de dossier que Tune ne voyait pas alors qu'Audirvana les
//! affiche : `Cover.JPG` sur un volume sensible à la casse, `AlbumArt_*`
//! (Windows Media Player), `*.webp`. La liste fixe `FOLDER_COVER_NAMES` ne
//! les contenait pas.

use std::path::Path;

use super::{find_folder_cover, image_de_pochette_dans, rang_de_pochette};
use crate::library::pochette_disque::est_une_image_de_pochette;

fn dossier_avec(noms: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    for n in noms {
        std::fs::write(dir.path().join(n), b"IMG").unwrap();
    }
    std::fs::write(dir.path().join("01 - piste.flac"), b"").unwrap();
    dir
}

fn trouvee(noms: &[&str]) -> Option<String> {
    let dir = dossier_avec(noms);
    find_folder_cover(&dir.path().join("01 - piste.flac"))
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
}

#[test]
fn cover_en_majuscules_mixtes_est_trouvee_6077() {
    // Ni `Cover.jpg` ni `COVER.JPG` : `Cover.JPG`, absent de la liste fixe.
    assert_eq!(trouvee(&["Cover.JPG"]).as_deref(), Some("Cover.JPG"));
    assert_eq!(trouvee(&["fOlDeR.JpEg"]).as_deref(), Some("fOlDeR.JpEg"));
}

#[test]
fn albumart_de_windows_media_player_est_trouvee_6077() {
    assert_eq!(
        trouvee(&["AlbumArt_{4A0C5B1E-1234-4C1D-9E7A-0123456789AB}_Large.jpg"]).as_deref(),
        Some("AlbumArt_{4A0C5B1E-1234-4C1D-9E7A-0123456789AB}_Large.jpg")
    );
    assert_eq!(
        trouvee(&["AlbumArtSmall.jpg"]).as_deref(),
        Some("AlbumArtSmall.jpg")
    );
    // La grande avant la petite.
    assert_eq!(
        trouvee(&[
            "AlbumArtSmall.jpg",
            "AlbumArt_{X}_Small.jpg",
            "AlbumArt_{X}_Large.jpg"
        ])
        .as_deref(),
        Some("AlbumArt_{X}_Large.jpg")
    );
}

#[test]
fn pochette_webp_est_trouvee_6077() {
    assert_eq!(trouvee(&["cover.webp"]).as_deref(), Some("cover.webp"));
    assert_eq!(trouvee(&["Folder.WEBP"]).as_deref(), Some("Folder.WEBP"));
}

#[test]
fn priorite_des_noms_conservee_6077() {
    // cover > folder > front > album > AlbumArt, puis jpg > jpeg > png > webp.
    assert_eq!(
        trouvee(&[
            "AlbumArtSmall.jpg",
            "album.png",
            "front.jpg",
            "Folder.jpg",
            "COVER.webp"
        ])
        .as_deref(),
        Some("COVER.webp")
    );
    assert_eq!(
        trouvee(&["cover.webp", "cover.png", "Cover.JPG"]).as_deref(),
        Some("Cover.JPG")
    );
    assert_eq!(
        trouvee(&["AlbumArtSmall.jpg", "front.png"]).as_deref(),
        Some("front.png")
    );
}

#[test]
fn les_autres_images_restent_ignorees_6077() {
    for n in [
        "artist.jpg",
        "back.jpg",
        "scan.png",
        "cover.jpg.part",
        "cover.gif",
        "cover",
        "booklet.webp",
    ] {
        assert!(rang_de_pochette(n).is_none(), "{n}");
        assert!(
            !est_une_image_de_pochette(Path::new(&format!("/m/A/{n}"))),
            "{n}"
        );
    }
    assert_eq!(trouvee(&["back.jpg", "scan.png"]), None);
}

#[test]
fn un_dossier_nomme_comme_une_pochette_est_ignore_6077() {
    let dir = dossier_avec(&[]);
    std::fs::create_dir(dir.path().join("Cover.jpg")).unwrap();
    assert_eq!(image_de_pochette_dans(dir.path()), None);
}

#[test]
fn le_surveillant_relaie_les_nouveaux_noms_6077() {
    for n in [
        "Cover.JPG",
        "AlbumArt_{X}_Large.jpg",
        "AlbumArtSmall.JPG",
        "cover.webp",
    ] {
        assert!(
            est_une_image_de_pochette(Path::new(&format!("/m/A/{n}"))),
            "{n}"
        );
    }
}
