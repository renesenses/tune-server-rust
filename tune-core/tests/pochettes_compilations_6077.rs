//! Reconnaissance des pochettes d'album par le vrai calcul d'empreinte (#6077).
use tune_core::scanner::compilation::folder_cover_fingerprint;

fn jpeg(seed: u8) -> Vec<u8> {
    let image = image::RgbImage::from_fn(32, 32, |x, y| {
        image::Rgb([
            ((x * u32::from(seed) + y * 7) % 256) as u8,
            (y * 7) as u8,
            32,
        ])
    });
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 90)
        .encode_image(&image)
        .unwrap();
    bytes
}

#[test]
fn les_noms_affiches_donnent_aussi_une_empreinte() {
    for name in ["CoVeR.JpG", "AlbumArt_{test}_Large.jpg", "album.png"] {
        let dir = tempfile::tempdir().unwrap();
        // L'empreinte décode le contenu, pas seulement son extension.
        std::fs::write(dir.path().join(name), jpeg(7)).unwrap();
        assert!(
            folder_cover_fingerprint(dir.path().to_str().unwrap()).is_some(),
            "{name}"
        );
    }
}

#[test]
fn une_vraie_pochette_webp_donne_une_empreinte() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("cover.webp"),
        include_bytes!("fixtures/cover_6077.webp"),
    )
    .unwrap();
    assert!(
        folder_cover_fingerprint(dir.path().to_str().unwrap()).is_some(),
        "WebP accepté comme pochette mais sans empreinte"
    );
}

#[test]
fn la_priorite_et_le_repli_apres_fichier_vide_restent_stables() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("cover.jpg"), jpeg(7)).unwrap();
    std::fs::write(dir.path().join("folder.jpg"), jpeg(31)).unwrap();
    let expected = folder_cover_fingerprint(dir.path().to_str().unwrap()).unwrap();
    std::fs::write(dir.path().join("AlbumArt_extra.jpg"), jpeg(31)).unwrap();
    assert!(
        folder_cover_fingerprint(dir.path().to_str().unwrap())
            .unwrap()
            .matches(&expected)
    );
    let reference = tempfile::tempdir().unwrap();
    std::fs::write(reference.path().join("cover.jpg"), jpeg(31)).unwrap();
    let fallback = folder_cover_fingerprint(reference.path().to_str().unwrap()).unwrap();
    assert!(
        !fallback.matches(&expected),
        "les deux témoins représentent des images différentes"
    );
    std::fs::write(dir.path().join("cover.jpg"), []).unwrap();
    assert!(
        folder_cover_fingerprint(dir.path().to_str().unwrap())
            .unwrap()
            .matches(&fallback)
    );
}
