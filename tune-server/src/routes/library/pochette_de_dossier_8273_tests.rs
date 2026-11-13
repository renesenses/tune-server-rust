//! Discussion #8273, point 1 — pochettes de la vue « Répertoires ».

use super::*;
use axum::body::to_bytes;

struct Banc {
    state: AppState,
    racine: PathBuf,
    cache: PathBuf,
    _dossier: tempfile::TempDir,
}

async fn banc() -> Banc {
    let dossier = tempfile::tempdir().unwrap();
    let racine = dossier.path().join("musique");
    let cache = dossier.path().join("cache");
    std::fs::create_dir_all(&racine).unwrap();
    std::fs::create_dir_all(&cache).unwrap();
    let base = dossier.path().join("tune.db");
    let state = AppState::new(base.to_str().unwrap(), 0, Default::default()).unwrap();
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set(
            "music_dirs",
            &serde_json::to_string(&[racine.to_str().unwrap()]).unwrap(),
        )
        .unwrap();
    Banc {
        state,
        racine,
        cache,
        _dossier: dossier,
    }
}

/// Une image PNG carrée de `cote` pixels.
fn png(cote: u32) -> Vec<u8> {
    let img = image::RgbImage::from_pixel(cote, cote, image::Rgb([200, 40, 90]));
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

fn cote_de(octets: &[u8]) -> u32 {
    let img = image::load_from_memory(octets).unwrap();
    img.width().max(img.height())
}

async fn demander(
    b: &Banc,
    chemin: &Path,
    size: Option<&str>,
    etag: Option<&str>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut h = HeaderMap::new();
    if let Some(e) = etag {
        h.insert(header::IF_NONE_MATCH, HeaderValue::from_str(e).unwrap());
    }
    let r = servir(
        &b.state,
        &b.cache,
        &h,
        chemin.to_str().unwrap().to_string(),
        size,
    )
    .await;
    let statut = r.status();
    let entetes = r.headers().clone();
    let corps = to_bytes(r.into_body(), usize::MAX).await.unwrap().to_vec();
    (statut, entetes, corps)
}

#[test]
fn la_case_est_toujours_une_vignette() {
    assert_eq!(case_demandee(None), 200);
    assert_eq!(case_demandee(Some("abc")), 200);
    assert_eq!(case_demandee(Some("40")), 80);
    assert_eq!(case_demandee(Some("80")), 80);
    assert_eq!(case_demandee(Some("150")), 200);
    // Au-delà de la plus grande case : la plus grande, jamais l'original.
    assert_eq!(case_demandee(Some("5000")), 400);
}

/// `folder.jpg` (ici un PNG de 1 200 px) posé dans le dossier : la réponse est
/// une VIGNETTE de la case demandée, mise en cache, et revalidée par ETag.
#[tokio::test]
async fn l_image_du_dossier_est_servie_en_vignette_et_mise_en_cache() {
    let b = banc().await;
    let album = b.racine.join("Artiste").join("Album");
    std::fs::create_dir_all(&album).unwrap();
    let original = png(1200);
    std::fs::write(album.join("folder.png"), &original).unwrap();

    let (statut, entetes, corps) = demander(&b, &album, Some("80"), None).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(entetes[header::CONTENT_TYPE], "image/jpeg");
    assert!(cote_de(&corps) <= 80, "une vignette, pas l'image entière");
    assert!(corps.len() < original.len());
    let etag = entetes[header::ETAG].to_str().unwrap().to_string();
    assert!(etag.ends_with("-w80\""), "{etag}");
    assert!(
        std::fs::read_dir(b.cache.join("resized").join("80"))
            .unwrap()
            .flatten()
            .any(|e| e.path().extension().is_some_and(|x| x == "jpg")),
        "la vignette est gardée dans le cache des pochettes"
    );

    // Même ETag : 304, sans corps.
    let (statut, _, corps) = demander(&b, &album, Some("80"), Some(&etag)).await;
    assert_eq!(statut, StatusCode::NOT_MODIFIED);
    assert!(corps.is_empty());

    // Une autre case est une autre vignette.
    let (statut, entetes, corps) = demander(&b, &album, Some("200"), None).await;
    assert_eq!(statut, StatusCode::OK);
    assert!(cote_de(&corps) <= 200 && cote_de(&corps) > 80);
    assert_ne!(entetes[header::ETAG].to_str().unwrap(), etag);
}

/// `cover.jpg` passe avant `front.jpg`, comme au scanner.
#[tokio::test]
async fn cover_passe_avant_front() {
    let b = banc().await;
    let d = b.racine.join("Album");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("front.png"), png(300)).unwrap();
    std::fs::write(d.join("cover.png"), png(300)).unwrap();
    let source = source_de_la_pochette(&b.state, d.to_str().unwrap()).unwrap();
    assert_eq!(source, SourcePochette::Image(d.join("cover.png")));
}

/// Un dossier d'artiste sans image prend la pochette (en cache) de l'album
/// rangé dessous ; la réponse est la vignette de cette pochette.
#[tokio::test]
async fn sans_image_le_dossier_prend_la_pochette_de_son_album() {
    let b = banc().await;
    let artiste = b.racine.join("Artiste");
    let album = artiste.join("Album");
    std::fs::create_dir_all(&album).unwrap();
    let hash = "0123456789abcdef0123456789abcdef";
    std::fs::write(b.cache.join(format!("{hash}.png")), png(900)).unwrap();
    b.state
        .backend
        .execute_batch(&format!(
            "INSERT INTO artists (id, name) VALUES (1, 'Artiste');\
             INSERT INTO albums (id, title, artist_id, cover_path) VALUES (1, 'Album', 1, '{hash}');\
             INSERT INTO tracks (id, title, album_id, file_path, format, source, track_number) \
             VALUES (1, 'Piste', 1, '{}', 'flac', 'local', 1);",
            album.join("01.flac").to_str().unwrap()
        ))
        .unwrap();

    let source = source_de_la_pochette(&b.state, artiste.to_str().unwrap()).unwrap();
    assert_eq!(source, SourcePochette::Album(hash.to_string()));

    let (statut, entetes, corps) = demander(&b, &artiste, Some("128"), None).await;
    assert_eq!(statut, StatusCode::OK);
    assert!(cote_de(&corps) <= 128);
    assert_eq!(entetes[header::CACHE_CONTROL], CACHE_CONTROL);
}

/// Une pochette d'album distante n'est pas téléchargée pour une vignette.
#[tokio::test]
async fn une_pochette_distante_n_est_pas_retenue() {
    let b = banc().await;
    let album = b.racine.join("Distant");
    std::fs::create_dir_all(&album).unwrap();
    b.state
        .backend
        .execute_batch(&format!(
            "INSERT INTO albums (id, title, cover_path) VALUES (1, 'Distant', 'https://exemple.invalid/a.jpg');\
             INSERT INTO tracks (id, title, album_id, file_path, format, source, track_number) \
             VALUES (1, 'Piste', 1, '{}', 'flac', 'local', 1);",
            album.join("01.flac").to_str().unwrap()
        ))
        .unwrap();
    let (statut, _, _) = demander(&b, &album, None, None).await;
    assert_eq!(statut, StatusCode::NOT_FOUND);
}

/// Rien du tout : 404, le client garde son icône.
#[tokio::test]
async fn un_dossier_sans_rien_rend_404() {
    let b = banc().await;
    let d = b.racine.join("Vide");
    std::fs::create_dir_all(&d).unwrap();
    let (statut, _, corps) = demander(&b, &d, Some("80"), None).await;
    assert_eq!(statut, StatusCode::NOT_FOUND);
    assert!(corps.is_empty());
}

/// La vignette ne lit que ce que la vue ouvrirait : hors des dossiers
/// musicaux, ou par un détour `..`, rien.
#[tokio::test]
async fn hors_de_la_bibliotheque_rien_n_est_lu() {
    let b = banc().await;
    let dehors = b.racine.parent().unwrap().join("ailleurs");
    std::fs::create_dir_all(&dehors).unwrap();
    std::fs::write(dehors.join("cover.png"), png(300)).unwrap();
    let (statut, _, _) = demander(&b, &dehors, None, None).await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);

    let detour = b.racine.join("..").join("ailleurs");
    let (statut, _, _) = demander(&b, &detour, None, None).await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
}

/// Une image qu'on ne sait pas réduire n'est servie telle quelle que légère.
#[test]
fn une_image_lourde_non_reductible_n_est_pas_servie() {
    let d = tempfile::tempdir().unwrap();
    let cache = d.path().join("cache");
    // En-tête WebP (RIFF….WEBP) : reconnu, mais `image` ne le décode pas ici.
    let mut webp = b"RIFF\0\0\0\0WEBPVP8 ".to_vec();
    webp.resize(PLAFOND_ORIGINAL_SERVI + 1, 0);
    let lourde = d.path().join("cover.webp");
    std::fs::write(&lourde, &webp).unwrap();
    assert!(matches!(
        vignette_d_image(&cache, &lourde, 80, None),
        Vignette::Aucune
    ));
    webp.truncate(1024);
    let legere = d.path().join("folder.webp");
    std::fs::write(&legere, &webp).unwrap();
    match vignette_d_image(&cache, &legere, 80, None) {
        Vignette::Corps { mime, octets, .. } => {
            assert_eq!(mime, "image/webp");
            assert_eq!(octets.len(), 1024);
        }
        _ => panic!("une petite image non réductible est servie telle quelle"),
    }
}
