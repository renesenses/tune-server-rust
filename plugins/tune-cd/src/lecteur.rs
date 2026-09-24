//! L'abstraction « lecteur de disque ».
//!
//! Trois implémentations : Linux (ioctl sur `/dev/sr*`, `linux.rs`), simulée
//! (en mémoire, pour les tests, `simule.rs`), et plus tard macOS (volume AIFF
//! sous `/Volumes`) et Windows (`IOCTL_CDROM_RAW_READ`). Tout ce qui est
//! au-dessus — flux, identifiant, routes — ne connaît que ce trait.

use std::fmt;
use std::sync::Arc;

use serde::Serialize;

use crate::toc::Toc;

/// Ce que le lecteur dit de lui-même.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    /// Le périphérique n'existe pas (ou ne s'ouvre pas).
    AucunLecteur,
    /// Le lecteur est là, sans disque lisible (tiroir ouvert, vide, pas prêt).
    Vide,
    /// Un disque est inséré et prêt.
    Disque,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErreurCd {
    /// Plus de disque : tiroir ouvert, éjection. Ne se rejoue pas.
    AucunDisque,
    /// Échec de lecture d'une plage de secteurs. Se rejoue.
    Lecture { lba: u32, raison: String },
    /// Toute autre erreur (périphérique absent, TOC illisible…).
    Autre(String),
}

impl fmt::Display for ErreurCd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErreurCd::AucunDisque => write!(f, "aucun disque dans le lecteur"),
            ErreurCd::Lecture { lba, raison } => {
                write!(f, "lecture du secteur {lba} impossible : {raison}")
            }
            ErreurCd::Autre(r) => write!(f, "{r}"),
        }
    }
}

pub trait LecteurDisque: Send + Sync {
    /// Le chemin du périphérique, pour l'affichage (`/dev/sr0`).
    fn chemin(&self) -> String;
    /// Présence du lecteur et du disque. Doit rester bon marché : elle est
    /// interrogée chaque seconde pendant une lecture pour voir l'éjection.
    fn presence(&self) -> Presence;
    /// La table des pistes du disque inséré.
    fn lire_toc(&self) -> Result<Toc, ErreurCd>;
    /// Lit `nombre` secteurs audio bruts à partir de `lba` dans `sortie`, qui
    /// mesure exactement `nombre × 2 352` octets.
    fn lire_secteurs(&self, lba: u32, nombre: u32, sortie: &mut [u8]) -> Result<(), ErreurCd>;
}

/// Le lecteur du système, s'il y en a un que Tune sait lire.
///
/// `TUNE_CD_DEVICE` impose un périphérique ; sinon le premier `/dev/sr0..3`
/// présent, comme la détection de `/cd-rip/drives`.
pub fn lecteur_du_systeme() -> Option<Arc<dyn LecteurDisque>> {
    #[cfg(target_os = "linux")]
    {
        let chemin = std::env::var("TUNE_CD_DEVICE").ok().or_else(|| {
            (0..4)
                .map(|i| format!("/dev/sr{i}"))
                .find(|c| std::path::Path::new(c).exists())
        })?;
        Some(Arc::new(crate::linux::LecteurLinux::new(chemin)))
    }
    #[cfg(not(target_os = "linux"))]
    {
        // macOS et Windows : hors de cette PR (#4863).
        None
    }
}

/// La plateforme a-t-elle une implémentation ?
pub const fn plateforme_prise_en_charge() -> bool {
    cfg!(target_os = "linux")
}
