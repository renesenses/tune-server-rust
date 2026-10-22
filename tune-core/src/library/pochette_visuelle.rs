//! D'après MetaRust, de Xavier Joly (code offert à Tune le 05/10/2026).
//!
//! Décider une pochette par son VISUEL, et non par ses octets (#4805, idée 7
//! de l'analyse de MetaRust).
//!
//! Deux fichiers différents montrent souvent la même pochette : une
//! numérisation à 1 400 pixels et la vignette de 500 pixels du Cover Art
//! Archive, ou deux encodages JPEG du même visuel. Comparer les octets les
//! dit « différents », et remplacer l'une par l'autre peut faire perdre de la
//! définition. L'empreinte visuelle (dHash 64 bits) les dit « mêmes ».
//!
//! # La règle (reprise de `cover.rs::decide_cover`)
//!
//! * pas de pochette en place, ou illisible : on pose la nouvelle ;
//! * mêmes octets : on garde ;
//! * même visuel (distance de Hamming ≤ [`MEME_VISUEL`]) : on garde, SAUF si
//!   la nouvelle est au moins [`PLUS_DEFINIE`] fois plus définie (en pixels) ;
//! * visuel différent : [`Decision::Differente`]. MetaRust remplaçait ;
//!   Tune laisse l'APPELANT trancher, parce que la réponse dépend du geste :
//!   une demande explicite de nouvelle pochette remplace, une amélioration
//!   automatique ne remplace jamais un autre visuel.
//!
//! # Repris
//!
//! `dhash64` et la règle de `decide_cover`, tels quels (seuils 8 et 1,4).
//! Le chargement, l'écriture dans les fichiers et le cas DSF de MetaRust ne
//! sont pas repris : Tune lit la pochette dans son cache et n'écrit rien dans
//! les fichiers audio.

use std::path::Path;

use image::imageops::FilterType;
use image::{DynamicImage, GenericImageView};

/// Distance de Hamming au-dessous de laquelle deux dHash montrent le même
/// visuel (MetaRust `DHASH_SAME`).
pub const MEME_VISUEL: u32 = 8;

/// Une nouvelle pochette du même visuel ne remplace que si elle a au moins
/// 1,4 fois plus de pixels (MetaRust `SHARPER_RATIO`).
pub const PLUS_DEFINIE: f64 = 1.4;

/// Ce que l'empreinte visuelle conclut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Aucune pochette en place, ou elle ne se décode pas.
    Poser,
    /// Même visuel, et la nouvelle est au moins 1,4 fois plus définie.
    PlusNette {
        avant: (u32, u32),
        apres: (u32, u32),
    },
    /// Mêmes octets, ou même visuel sans gain de définition.
    Garder { distance: u32 },
    /// Un autre visuel. L'appelant tranche.
    Differente { distance: u32 },
}

impl Decision {
    /// Le mot stable des réponses d'API.
    pub fn code(&self) -> &'static str {
        match self {
            Decision::Poser => "poser",
            Decision::PlusNette { .. } => "plus_nette",
            Decision::Garder { .. } => "meme_visuel",
            Decision::Differente { .. } => "visuel_different",
        }
    }
}

/// L'empreinte visuelle d'une image : 9 × 8 en niveaux de gris, un bit par
/// paire de pixels voisins (MetaRust `dhash64`).
pub fn dhash64(img: &DynamicImage) -> u64 {
    let small = img.resize_exact(9, 8, FilterType::Triangle).grayscale();
    let mut hash = 0u64;
    let mut bit = 0u32;
    for y in 0..8 {
        for x in 0..8 {
            let a = small.get_pixel(x, y)[0];
            let b = small.get_pixel(x + 1, y)[0];
            if a > b {
                hash |= 1u64 << bit;
            }
            bit += 1;
        }
    }
    hash
}

/// Décide entre la pochette en place (`existante`, ses octets) et une
/// `candidate` (MetaRust `decide_cover`).
///
/// Une candidate qui ne se décode pas n'est pas une pochette : elle est
/// gardée hors jeu ([`Decision::Garder`]) dès qu'une pochette lisible est en
/// place. MetaRust la posait (« binaire différent ») ; Tune ne remplace pas
/// une image lisible par une image illisible.
pub fn decider(existante: Option<&[u8]>, candidate: &[u8]) -> Decision {
    let Some(existante) = existante else {
        return Decision::Poser;
    };
    if existante == candidate {
        return Decision::Garder { distance: 0 };
    }
    let Some(a) = image::load_from_memory(existante).ok() else {
        return Decision::Poser;
    };
    let Some(b) = image::load_from_memory(candidate).ok() else {
        return Decision::Garder { distance: 64 };
    };
    let distance = (dhash64(&a) ^ dhash64(&b)).count_ones();
    if distance > MEME_VISUEL {
        return Decision::Differente { distance };
    }
    let (aw, ah) = a.dimensions();
    let (bw, bh) = b.dimensions();
    let ap = u64::from(aw) * u64::from(ah);
    let bp = u64::from(bw) * u64::from(bh);
    if bp > ap && (bp as f64) >= (ap as f64) * PLUS_DEFINIE {
        Decision::PlusNette {
            avant: (aw, ah),
            apres: (bw, bh),
        }
    } else {
        Decision::Garder { distance }
    }
}

/// Les octets de la pochette d'un album, lus dans le cache des pochettes.
/// `cover_path` est un condensat de contenu (32 ou 64 caractères
/// hexadécimaux) ; une URL distante ou une adresse inconnue rend `None`.
pub fn octets_en_cache(cache_dir: &Path, cover_path: &str) -> Option<Vec<u8>> {
    let hex = (cover_path.len() == 32 || cover_path.len() == 64)
        && cover_path.chars().all(|c| c.is_ascii_hexdigit());
    if !hex {
        return None;
    }
    let (chemin, _) = super::artwork::find_cached(cache_dir, cover_path)?;
    std::fs::read(chemin).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, Rgb, RgbImage};
    use std::io::Cursor;

    fn encoder(img: RgbImage, format: ImageFormat) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(img)
            .write_to(&mut out, format)
            .unwrap();
        out.into_inner()
    }

    /// Un visuel : moitié gauche `g`, moitié droite `d`, avec un dégradé
    /// vertical pour que le dHash ait des bits des deux valeurs.
    fn visuel(w: u32, h: u32, g: [u8; 3], d: [u8; 3]) -> RgbImage {
        RgbImage::from_fn(w, h, |x, y| {
            let base = if x < w / 2 { g } else { d };
            let t = (y * 60 / h.max(1)) as u8;
            Rgb([
                base[0].saturating_add(t),
                base[1].saturating_add(t),
                base[2].saturating_add(t),
            ])
        })
    }

    fn png(w: u32, h: u32, g: [u8; 3], d: [u8; 3]) -> Vec<u8> {
        encoder(visuel(w, h, g, d), ImageFormat::Png)
    }

    #[test]
    fn sans_pochette_on_pose() {
        assert_eq!(
            decider(None, &png(10, 10, [0; 3], [255; 3])),
            Decision::Poser
        );
    }

    #[test]
    fn memes_octets_on_garde() {
        let a = png(32, 32, [10, 20, 30], [200, 210, 220]);
        assert_eq!(decider(Some(&a), &a), Decision::Garder { distance: 0 });
    }

    /// Le cas qui motive le module : la même pochette en JPEG, plus petite.
    /// Les octets diffèrent ; on garde la grande.
    #[test]
    fn meme_visuel_moins_defini_on_garde() {
        let grande = png(600, 600, [20, 20, 20], [230, 230, 230]);
        let petite = encoder(
            visuel(300, 300, [20, 20, 20], [230, 230, 230]),
            ImageFormat::Jpeg,
        );
        assert!(matches!(
            decider(Some(&grande), &petite),
            Decision::Garder { distance } if distance <= MEME_VISUEL
        ));
    }

    /// La règle des 1,4 : 300² → 400² (×1,78) remplace ; 300² → 340² (×1,28)
    /// ne remplace pas.
    #[test]
    fn meme_visuel_plus_net_d_au_moins_1_4_on_remplace() {
        let base = png(300, 300, [20, 20, 20], [230, 230, 230]);
        let nette = png(400, 400, [20, 20, 20], [230, 230, 230]);
        assert_eq!(
            decider(Some(&base), &nette),
            Decision::PlusNette {
                avant: (300, 300),
                apres: (400, 400)
            }
        );
        let a_peine = png(340, 340, [20, 20, 20], [230, 230, 230]);
        assert!(matches!(
            decider(Some(&base), &a_peine),
            Decision::Garder { .. }
        ));
    }

    #[test]
    fn visuel_different_l_appelant_tranche() {
        let a = png(64, 64, [0, 0, 0], [255, 255, 255]);
        let b = png(64, 64, [255, 255, 255], [0, 0, 0]);
        assert!(matches!(
            decider(Some(&a), &b),
            Decision::Differente { distance } if distance > MEME_VISUEL
        ));
    }

    #[test]
    fn une_candidate_illisible_ne_remplace_pas_une_pochette_lisible() {
        let a = png(64, 64, [0, 0, 0], [255, 255, 255]);
        assert!(matches!(
            decider(Some(&a), b"pas une image"),
            Decision::Garder { .. }
        ));
        // Et une pochette en place illisible est remplacée.
        assert_eq!(decider(Some(b"abime"), &a), Decision::Poser);
    }

    #[test]
    fn le_cache_ne_se_lit_que_par_condensat() {
        let dossier = tempfile::tempdir().unwrap();
        let octets = png(8, 8, [1, 2, 3], [4, 5, 6]);
        let h = super::super::artwork::cache_fetched_image(&octets, dossier.path(), "png").unwrap();
        assert_eq!(
            octets_en_cache(dossier.path(), &h).as_deref(),
            Some(&octets[..])
        );
        assert!(octets_en_cache(dossier.path(), "https://exemple.org/c.jpg").is_none());
        assert!(octets_en_cache(dossier.path(), &"0".repeat(64)).is_none());
    }
}
