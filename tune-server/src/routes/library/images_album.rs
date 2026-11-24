//! Les images du dossier d'un album : scans du livret, dos, disque… (fil
//! forum 3050).
//!
//! Le livret PDF (#6481, `livrets.rs`) ne couvre qu'une partie de ce que les
//! éditions rangent à côté des pistes : bien des albums n'ont pas de PDF mais
//! un dossier `Scans/` ou `Artwork/` plein de JPEG. Deux routes :
//!
//! - `GET /library/albums/{id}/images` — la liste,
//!   `{"images": [{"id": "9f…", "name": "back.jpg", "folder": "Scans",
//!   "size": 123456, "mime": "image/jpeg"}]}` ;
//! - `GET /library/albums/{id}/images/{image_id}[?size=200]` — l'image, ou sa
//!   vignette JPEG quand `size` est donné.
//!
//! # Ce que la seconde route accepte de servir — et rien d'autre
//!
//! Le client ne transmet JAMAIS de chemin ni de nom de fichier : il transmet
//! l'identifiant que la liste lui a donné. L'identifiant est un condensat
//! (SHA-256 tronqué) de l'album et du chemin, calculé par le serveur ; il ne
//! se décode pas, il se RETROUVE dans la liste que le serveur recalcule.
//!
//! 1. L'identifiant est 16 hexadécimaux, rien d'autre.
//! 2. Les seuls dossiers regardés sont ceux de l'ALBUM, déduits des pistes en
//!    base (comme pour le livret), plus leurs sous-dossiers usuels (`Scans`,
//!    `Artwork`, `Covers`…) sur UN niveau. Un sous-dossier qui est un lien
//!    symbolique n'est pas suivi.
//! 3. L'identifiant doit figurer dans cette liste : une entrée de l'un de ces
//!    dossiers, fichier ordinaire (un lien symbolique n'en est pas un), à
//!    l'extension d'image connue, sous la taille plafond.
//! 4. Avant d'ouvrir, le chemin canonique du fichier doit avoir pour parent le
//!    chemin canonique de son dossier, et ce dossier doit être un dossier de
//!    l'album ou un enfant direct de l'un d'eux.
//!
//! Tout refus rend 404, sans dire lequel des contrôles a refusé.
use std::collections::BTreeSet;
use std::path::{Path as CheminFs, PathBuf};

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::livrets::{chemins_des_pistes, dossiers_des_pistes};
use super::{artwork_cache_dir, refus};
use crate::state::AppState;

/// Au-delà, ce n'est plus un scan de livret : on ne charge pas un fichier
/// arbitraire en mémoire pour le servir.
const TAILLE_MAX_IMAGE: u64 = 64 * 1024 * 1024;

/// Un dossier de musique en vrac ne doit pas produire une liste sans fin.
const NOMBRE_MAX_IMAGES: usize = 300;

/// Extensions servies, et leur type MIME. Pas de SVG (du script dans une
/// image servie depuis l'origine du serveur), pas de TIFF ni de BMP (que les
/// navigateurs n'affichent pas).
const EXTENSIONS: &[(&str, &str)] = &[
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("png", "image/png"),
    ("webp", "image/webp"),
    ("gif", "image/gif"),
];

/// Sous-dossiers d'un album où les éditions rangent leurs images. Comparés
/// sans casse.
const SOUS_DOSSIERS_USUELS: &[&str] = &[
    "scans", "scan", "artwork", "artworks", "art", "covers", "cover", "images", "booklet",
    "livret", "pictures", "photos",
];

/// Une image du dossier d'un album.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ImageAlbum {
    /// Condensat de l'album et du chemin : la seule chose que le client
    /// renvoie.
    pub id: String,
    pub nom: String,
    /// Le sous-dossier usuel (`Scans`…) d'où elle vient, `None` pour une
    /// image rangée avec les pistes.
    pub sous_dossier: Option<String>,
    pub taille: u64,
    pub mime: &'static str,
    pub chemin: PathBuf,
    /// Le dossier de l'album (dossier de pistes) dont elle relève.
    pub racine: PathBuf,
}

/// Le type MIME d'un nom d'image servi, `None` pour tout le reste.
pub(crate) fn mime_image(nom: &str) -> Option<&'static str> {
    if nom.is_empty() || nom.len() > 255 || nom.starts_with('.') {
        return None;
    }
    let (_, ext) = nom.rsplit_once('.')?;
    let ext = ext.to_ascii_lowercase();
    EXTENSIONS
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, mime)| *mime)
}

/// L'identifiant d'une image : 16 hexadécimaux du SHA-256 de l'album et du
/// chemin. Lié à l'album : l'identifiant d'une image d'un album n'ouvre rien
/// au nom d'un autre.
pub(crate) fn identifiant_image(album_id: i64, chemin: &CheminFs) -> String {
    let mut h = Sha256::new();
    h.update(album_id.to_le_bytes());
    h.update([0u8]);
    h.update(chemin.as_os_str().as_encoded_bytes());
    h.finalize()[..8]
        .iter()
        .map(|o| format!("{o:02x}"))
        .collect()
}

/// L'identifiant a-t-il la forme qu'on émet ? (Rien d'autre n'est cherché.)
pub(crate) fn identifiant_valide(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Les images directes d'un dossier : fichiers ordinaires (un lien symbolique
/// est écarté, `file_type` ne le suit pas), à l'extension connue, sous le
/// plafond de taille.
fn images_du_dossier(
    album_id: i64,
    dossier: &CheminFs,
    racine: &CheminFs,
    sous_dossier: Option<&str>,
) -> Vec<ImageAlbum> {
    let Ok(entrees) = std::fs::read_dir(dossier) else {
        return Vec::new();
    };
    let mut ici: Vec<ImageAlbum> = entrees
        .filter_map(Result::ok)
        .filter_map(|e| {
            if !e.file_type().ok()?.is_file() {
                return None;
            }
            let nom = e.file_name().into_string().ok()?;
            let mime = mime_image(&nom)?;
            let taille = e.metadata().ok()?.len();
            if taille == 0 || taille > TAILLE_MAX_IMAGE {
                return None;
            }
            let chemin = e.path();
            Some(ImageAlbum {
                id: identifiant_image(album_id, &chemin),
                nom,
                sous_dossier: sous_dossier.map(str::to_string),
                taille,
                mime,
                chemin,
                racine: racine.to_path_buf(),
            })
        })
        .collect();
    ici.sort_by_key(|i| i.nom.to_lowercase());
    ici
}

/// Les sous-dossiers usuels d'un dossier d'album : vrais dossiers (un lien
/// symbolique n'est pas suivi), au nom de la liste, triés.
fn sous_dossiers_usuels(dossier: &CheminFs) -> Vec<(String, PathBuf)> {
    let Ok(entrees) = std::fs::read_dir(dossier) else {
        return Vec::new();
    };
    let mut sous: Vec<(String, PathBuf)> = entrees
        .filter_map(Result::ok)
        .filter_map(|e| {
            if !e.file_type().ok()?.is_dir() {
                return None;
            }
            let nom = e.file_name().into_string().ok()?;
            let bas = nom.to_lowercase();
            SOUS_DOSSIERS_USUELS
                .contains(&bas.as_str())
                .then(|| (nom, e.path()))
        })
        .collect();
    sous.sort_by_key(|(n, _)| n.to_lowercase());
    sous
}

/// Les images de ces dossiers d'album : d'abord celles rangées avec les
/// pistes, puis celles des sous-dossiers usuels, un seul niveau. Un même
/// fichier n'apparaît qu'une fois ; la liste est plafonnée.
pub(crate) fn images_des_dossiers(album_id: i64, dossiers: &BTreeSet<PathBuf>) -> Vec<ImageAlbum> {
    let mut vus = BTreeSet::new();
    let mut directes = Vec::new();
    let mut rangees = Vec::new();
    for dossier in dossiers {
        directes.extend(images_du_dossier(album_id, dossier, dossier, None));
        for (nom, sous) in sous_dossiers_usuels(dossier) {
            if dossiers.contains(&sous) {
                continue;
            }
            rangees.extend(images_du_dossier(album_id, &sous, dossier, Some(&nom)));
        }
    }
    directes
        .into_iter()
        .chain(rangees)
        .filter(|i| vus.insert(i.chemin.clone()))
        .take(NOMBRE_MAX_IMAGES)
        .collect()
}

/// L'image `id` de ces dossiers, si elle passe TOUS les contrôles de
/// l'en-tête.
pub(crate) fn image_a_servir(
    album_id: i64,
    dossiers: &BTreeSet<PathBuf>,
    id: &str,
) -> Option<ImageAlbum> {
    if !identifiant_valide(id) {
        return None;
    }
    let image = images_des_dossiers(album_id, dossiers)
        .into_iter()
        .find(|i| i.id == id)?;
    let racine = image.racine.canonicalize().ok()?;
    let dossier = image.chemin.parent()?.canonicalize().ok()?;
    let fichier = image.chemin.canonicalize().ok()?;
    if fichier.parent()? != dossier {
        return None;
    }
    // Le dossier de l'image est celui de l'album, ou un enfant direct.
    if dossier != racine && dossier.parent()? != racine {
        return None;
    }
    let meta = std::fs::symlink_metadata(&image.chemin).ok()?;
    if !meta.file_type().is_file() || meta.len() > TAILLE_MAX_IMAGE {
        return None;
    }
    Some(image)
}

fn introuvable() -> Response {
    refus(
        StatusCode::NOT_FOUND,
        "image_introuvable",
        "aucune image de cet identifiant pour cet album".to_string(),
    )
}

/// `GET /library/albums/{id}/images` — les images du dossier de l'album.
/// Un album sans image (ou inconnu) rend une liste vide.
pub(super) async fn images_album(
    State(state): State<AppState>,
    Path(album_id): Path<i64>,
) -> Json<serde_json::Value> {
    let st = state.clone();
    let images = tokio::task::spawn_blocking(move || {
        images_des_dossiers(
            album_id,
            &dossiers_des_pistes(chemins_des_pistes(&st, album_id)),
        )
    })
    .await
    .unwrap_or_default();
    Json(json!({
        "images": images
            .iter()
            .map(|i| json!({
                "id": i.id,
                "name": i.nom,
                "folder": i.sous_dossier,
                "size": i.taille,
                "mime": i.mime,
            }))
            .collect::<Vec<_>>(),
    }))
}

#[derive(Deserialize)]
pub(super) struct RequeteImage {
    size: Option<String>,
}

/// `GET /library/albums/{id}/images/{image_id}` — l'image, ou avec `?size=`
/// sa vignette JPEG dans la case fermée de `THUMB_SIZES` (80 à 400 px).
pub(super) async fn image_album(
    State(state): State<AppState>,
    Path((album_id, id)): Path<(i64, String)>,
    Query(requete): Query<RequeteImage>,
) -> Response {
    let taille = requete.size.as_deref().and_then(|s| s.parse::<u32>().ok());
    servir_image(&state, &artwork_cache_dir(), album_id, id, taille).await
}

/// Le cœur de [`image_album`], répertoire des vignettes donné (testable sans
/// variable d'environnement).
pub(crate) async fn servir_image(
    state: &AppState,
    cache_dir: &CheminFs,
    album_id: i64,
    id: String,
    taille: Option<u32>,
) -> Response {
    let st = state.clone();
    let lu = tokio::task::spawn_blocking(move || {
        let image = image_a_servir(
            album_id,
            &dossiers_des_pistes(chemins_des_pistes(&st, album_id)),
            &id,
        )?;
        let octets = std::fs::read(&image.chemin).ok()?;
        let mtime = std::fs::metadata(&image.chemin)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Some((image, octets, mtime))
    })
    .await
    .ok()
    .flatten();
    let Some((image, octets, mtime)) = lu else {
        return introuvable();
    };
    if let Some(case) = taille.and_then(tune_core::library::artwork::thumb_bucket) {
        // Clé de cache : l'identifiant ET l'état du fichier — un scan
        // remplacé sous le même nom ne ressert pas l'ancienne vignette.
        let cle = format!("album-image-{}-{}-{}", image.id, image.taille, mtime);
        if let Some(vignette) = vignette(cache_dir, &cle, case, octets.clone()).await {
            return reponse_image(vignette, "image/jpeg", &image.nom);
        }
    }
    reponse_image(octets, image.mime, &image.nom)
}

/// Vignette depuis le cache des pochettes, ou fabriquée puis rangée. `None` =
/// servir l'original (format non décodable, image déjà plus petite).
async fn vignette(
    cache_dir: &CheminFs,
    cle: &str,
    case: u32,
    original: Vec<u8>,
) -> Option<Vec<u8>> {
    let chemin = tune_core::library::artwork::thumb_path(cache_dir, case, cle);
    if let Ok(deja) = tokio::fs::read(&chemin).await {
        return Some(deja);
    }
    let (dir, cle) = (cache_dir.to_path_buf(), cle.to_string());
    tokio::task::spawn_blocking(move || {
        let v = tune_core::library::artwork::make_thumbnail(&original, case)?;
        tune_core::library::artwork::store_thumbnail(&dir, case, &cle, &v);
        Some(v)
    })
    .await
    .ok()
    .flatten()
}

fn reponse_image(octets: Vec<u8>, mime: &'static str, nom: &str) -> Response {
    let encode: String = nom
        .bytes()
        .map(|o| {
            if o.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&o) {
                (o as char).to_string()
            } else {
                format!("%{o:02X}")
            }
        })
        .collect();
    let disposition = HeaderValue::from_str(&format!("inline; filename*=UTF-8''{encode}"))
        .unwrap_or_else(|_| HeaderValue::from_static("inline"));
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(mime)),
            (header::CONTENT_DISPOSITION, disposition),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=300"),
            ),
        ],
        octets,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dossier_de_test(nom: &str) -> tune_core::test_scratch::ScratchDir {
        tune_core::test_scratch::scratch_dir(&format!("images-album-3050-{nom}"))
    }

    /// Un PNG valide de `cote` × `cote` pixels.
    fn png(cote: u32) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(cote, cote, image::Rgb([200, 30, 30]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    fn noms(images: &[ImageAlbum]) -> Vec<String> {
        images
            .iter()
            .map(|i| match &i.sous_dossier {
                Some(s) => format!("{s}/{}", i.nom),
                None => i.nom.clone(),
            })
            .collect()
    }

    #[test]
    fn seules_les_extensions_d_image_sont_retenues_sans_casse() {
        assert_eq!(mime_image("front.JPG"), Some("image/jpeg"));
        assert_eq!(mime_image("Back.Jpeg"), Some("image/jpeg"));
        assert_eq!(mime_image("cd.PNG"), Some("image/png"));
        assert_eq!(mime_image("inlay.webp"), Some("image/webp"));
        assert_eq!(mime_image("anim.GIF"), Some("image/gif"));
        for refuse in [
            "",
            ".cache.jpg",
            "logo.svg",
            "scan.tif",
            "scan.bmp",
            "livret.pdf",
            "01.flac",
            "jpg",
            "image.jpg.exe",
        ] {
            assert_eq!(mime_image(refuse), None, "{refuse:?}");
        }
    }

    #[test]
    fn les_images_du_dossier_et_des_sous_dossiers_usuels_sont_listees() {
        let tmp = dossier_de_test("liste");
        let racine = tmp.path();
        let album = racine.join("Album");
        std::fs::create_dir_all(album.join("Scans").join("Profond")).unwrap();
        std::fs::create_dir_all(album.join("artwork")).unwrap();
        std::fs::create_dir_all(album.join("Divers")).unwrap();
        std::fs::write(album.join("01.flac"), b"x").unwrap();
        std::fs::write(album.join("folder.JPG"), b"jpg").unwrap();
        std::fs::write(album.join("Livret.pdf"), b"%PDF").unwrap();
        std::fs::write(album.join(".cache.jpg"), b"x").unwrap();
        std::fs::write(album.join("vide.png"), b"").unwrap();
        std::fs::write(album.join("Scans").join("Back.jpeg"), b"x").unwrap();
        std::fs::write(album.join("Scans").join("cd.png"), b"x").unwrap();
        std::fs::write(album.join("Scans").join("Profond").join("p.jpg"), b"x").unwrap();
        std::fs::write(album.join("artwork").join("inlay.WEBP"), b"x").unwrap();
        std::fs::write(album.join("Divers").join("autre.jpg"), b"x").unwrap();
        std::fs::write(racine.join("voisin.jpg"), b"x").unwrap();
        let dossiers = dossiers_des_pistes([album.join("01.flac").to_string_lossy()]);
        let images = images_des_dossiers(7, &dossiers);
        assert_eq!(
            noms(&images),
            vec![
                "folder.JPG",
                "artwork/inlay.WEBP",
                "Scans/Back.jpeg",
                "Scans/cd.png",
            ]
        );
        assert!(images.iter().all(|i| identifiant_valide(&i.id)));
    }

    #[test]
    fn un_album_par_disque_montre_les_scans_du_dossier_de_l_album() {
        let tmp = dossier_de_test("disques");
        let album = tmp.path().join("Coffret");
        std::fs::create_dir_all(album.join("CD1")).unwrap();
        std::fs::create_dir_all(album.join("CD2")).unwrap();
        std::fs::create_dir_all(album.join("Covers")).unwrap();
        std::fs::write(album.join("CD1").join("01.flac"), b"x").unwrap();
        std::fs::write(album.join("CD2").join("01.flac"), b"x").unwrap();
        std::fs::write(album.join("cover.jpg"), b"x").unwrap();
        std::fs::write(album.join("CD2").join("cd2.png"), b"x").unwrap();
        std::fs::write(album.join("Covers").join("box.jpg"), b"x").unwrap();
        let dossiers = dossiers_des_pistes([
            album.join("CD1").join("01.flac").to_string_lossy(),
            album.join("CD2").join("01.flac").to_string_lossy(),
        ]);
        assert_eq!(
            noms(&images_des_dossiers(1, &dossiers)),
            vec!["cover.jpg", "cd2.png", "Covers/box.jpg"]
        );
    }

    #[test]
    fn l_identifiant_est_lie_a_l_album_et_ne_se_forge_pas() {
        let tmp = dossier_de_test("garde");
        let racine = tmp.path();
        let album = racine.join("Album");
        std::fs::create_dir_all(album.join("Scans")).unwrap();
        std::fs::write(album.join("01.flac"), b"x").unwrap();
        std::fs::write(album.join("Scans").join("back.jpg"), b"back").unwrap();
        std::fs::write(racine.join("voisin.jpg"), b"voisin").unwrap();
        std::fs::write(racine.join("secret.txt"), b"secret").unwrap();
        let dossiers = dossiers_des_pistes([album.join("01.flac").to_string_lossy()]);
        let liste = images_des_dossiers(5, &dossiers);
        assert_eq!(liste.len(), 1);
        let id = liste[0].id.clone();

        let servie = image_a_servir(5, &dossiers, &id).expect("le scan de l'album");
        assert_eq!(std::fs::read(&servie.chemin).unwrap(), b"back");

        // Le même identifiant ne sert rien au nom d'un autre album.
        assert!(image_a_servir(6, &dossiers, &id).is_none());

        // L'identifiant d'un fichier hors du dossier, calculé comme le serveur
        // le ferait : il n'est pas dans la liste, rien ne sort.
        let forge_voisin = identifiant_image(5, &racine.join("voisin.jpg"));
        let forge_secret = identifiant_image(5, &racine.join("secret.txt"));
        let forge_point = identifiant_image(5, &album.join("..").join("voisin.jpg"));
        for tentative in [
            forge_voisin.as_str(),
            forge_secret.as_str(),
            forge_point.as_str(),
            "../voisin.jpg",
            "Scans/back.jpg",
            "back.jpg",
            "/etc/passwd",
            "..%2F..%2Fetc%2Fpasswd",
            "0000000000000000",
            &id.to_uppercase(),
            &format!("{id}/../x"),
        ] {
            assert!(
                image_a_servir(5, &dossiers, tentative).is_none(),
                "{tentative:?} ne doit rien servir"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn un_lien_symbolique_n_est_ni_liste_ni_suivi() {
        let tmp = dossier_de_test("liens");
        let racine = tmp.path();
        let album = racine.join("Album");
        let ailleurs = racine.join("Ailleurs");
        std::fs::create_dir_all(&album).unwrap();
        std::fs::create_dir_all(&ailleurs).unwrap();
        std::fs::write(album.join("01.flac"), b"x").unwrap();
        std::fs::write(ailleurs.join("prive.jpg"), b"prive").unwrap();
        // Un fichier-lien, et un sous-dossier usuel qui est un lien.
        std::os::unix::fs::symlink(ailleurs.join("prive.jpg"), album.join("lien.jpg")).unwrap();
        std::os::unix::fs::symlink(&ailleurs, album.join("Scans")).unwrap();
        let dossiers = dossiers_des_pistes([album.join("01.flac").to_string_lossy()]);
        assert!(images_des_dossiers(1, &dossiers).is_empty());
        for chemin in [
            album.join("lien.jpg"),
            album.join("Scans").join("prive.jpg"),
        ] {
            let forge = identifiant_image(1, &chemin);
            assert!(image_a_servir(1, &dossiers, &forge).is_none(), "{chemin:?}");
        }
    }

    #[tokio::test]
    async fn les_routes_listent_et_servent_les_images_et_leurs_vignettes() {
        let tmp = dossier_de_test("routes");
        let racine = tmp.path();
        let album = racine.join("Kind of Blue");
        std::fs::create_dir_all(album.join("Scans")).unwrap();
        std::fs::write(album.join("01.flac"), b"x").unwrap();
        let grand = png(1000);
        std::fs::write(album.join("Scans").join("Livret 01.png"), &grand).unwrap();
        std::fs::write(racine.join("voisin.jpg"), b"voisin").unwrap();
        let cache = tmp.path().join("cache");
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let piste = album.join("01.flac").to_string_lossy().into_owned();
        state
            .backend
            .execute_batch(&format!(
                "INSERT INTO albums (id, title) VALUES (64, 'Kind of Blue');
                 INSERT INTO tracks (id, title, file_path, album_id) VALUES (6401, 'So What', '{}', 64);",
                piste.replace('\'', "''")
            ))
            .unwrap();

        let Json(liste) = images_album(State(state.clone()), Path(64)).await;
        let images = liste["images"].as_array().unwrap();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0]["name"], "Livret 01.png");
        assert_eq!(images[0]["folder"], "Scans");
        assert_eq!(images[0]["mime"], "image/png");
        assert_eq!(images[0]["size"], grand.len() as u64);
        let id = images[0]["id"].as_str().unwrap().to_string();
        let Json(vide) = images_album(State(state.clone()), Path(999)).await;
        assert_eq!(vide, json!({"images": []}));

        // L'original.
        let r = servir_image(&state, &cache, 64, id.clone(), None).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()[header::CONTENT_TYPE], "image/png");
        assert_eq!(r.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        let corps = axum::body::to_bytes(r.into_body(), 1 << 24).await.unwrap();
        assert_eq!(&corps[..], &grand[..]);

        // La vignette : JPEG, dans la case de 200 px.
        let r = servir_image(&state, &cache, 64, id.clone(), Some(150)).await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers()[header::CONTENT_TYPE], "image/jpeg");
        let corps = axum::body::to_bytes(r.into_body(), 1 << 24).await.unwrap();
        let v = image::load_from_memory(&corps).unwrap();
        assert_eq!(v.width().max(v.height()), 200);
        // Une taille démesurée ne choisit aucune allocation : l'original.
        let r = servir_image(&state, &cache, 64, id.clone(), Some(100_000)).await;
        assert_eq!(r.headers()[header::CONTENT_TYPE], "image/png");

        let forge = identifiant_image(64, &racine.join("voisin.jpg"));
        for tentative in [forge, "../voisin.jpg".into(), "voisin.jpg".into()] {
            let r = servir_image(&state, &cache, 64, tentative.clone(), None).await;
            assert_eq!(r.status(), StatusCode::NOT_FOUND, "{tentative:?}");
        }
        // L'image d'un album n'est pas servie au nom d'un autre album.
        let r = servir_image(&state, &cache, 999, id, None).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }
}
