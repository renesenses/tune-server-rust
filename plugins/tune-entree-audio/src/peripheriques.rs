//! Ce que le greffon demande au système : lister les entrées, en démarrer une
//! au format natif, et relire sa fréquence nominale courante.
//!
//! Un trait, pour que le contrôleur, le lecteur et les routes se prouvent
//! sans matériel (`simule.rs`). L'implémentation réelle (`systeme.rs`, cpal)
//! n'existe qu'avec la feature `capture`.

use std::sync::Arc;

use serde::Serialize;
use tune_core::source_pcm::FormatPcm;

use crate::anneau::Anneau;

/// Une entrée audio telle que `/entrees` la décrit.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DescriptionEntree {
    /// Le nom affiché, et la clé de `/jouer` (l'`id` est accepté aussi).
    pub nom: String,
    pub id: String,
    pub canaux: u16,
    /// Fréquences que le périphérique accepte (bornes des plages annoncées).
    pub frequences: Vec<u32>,
    /// Sa fréquence courante : celle que la capture prendra.
    pub frequence_courante: Option<u32>,
    /// Formats d'échantillons que le pilote rend (`f32`, `i16`, `i24`…).
    pub formats: Vec<String>,
    /// Profondeur du format PHYSIQUE, quand le système la dit (CoreAudio).
    pub bits_physiques: Option<u16>,
    /// Profondeur servie à la zone.
    pub bits_servis: u16,
    pub par_defaut: bool,
    /// Périphérique VIRTUEL (Loopback Audio, BlackHole…), quand le système
    /// le dit (transport CoreAudio `Virtual`) ; `None` : inconnu. Aucune
    /// entrée n'est écartée pour autant.
    pub virtuelle: Option<bool>,
}

/// Arrête une capture (le flux du pilote vit dans son propre fil).
pub trait Arret: Send {
    fn arreter(self: Box<Self>);
}

/// Une capture démarrée : son format servi, et de quoi l'arrêter.
pub struct CaptureDemarree {
    pub nom: String,
    pub format: FormatPcm,
    pub arret: Box<dyn Arret>,
}

pub trait Peripheriques: Send + Sync {
    /// `coreaudio`, `alsa`, `wasapi`, `simule`, ou `aucune` (capture non
    /// compilée).
    fn pile(&self) -> &'static str;
    fn lister(&self) -> Result<Vec<DescriptionEntree>, String>;
    /// Le format servi SI l'on démarrait `entree` maintenant.
    fn format_natif(&self, entree: &str) -> Result<(String, FormatPcm), String>;
    /// Démarre la capture de `entree` à son format natif ; le rappel du
    /// pilote pousse dans `anneau`, qu'il ferme en `Fin::Erreur` si le
    /// périphérique faillit. `anneau` doit avoir été créé au format rendu par
    /// [`Self::format_natif`].
    fn demarrer(&self, entree: &str, anneau: Arc<Anneau>) -> Result<CaptureDemarree, String>;
    /// Sa fréquence nominale ACTUELLE (un S/PDIF peut en changer).
    fn frequence_courante(&self, entree: &str) -> Option<u32>;
}

/// Sans la feature `capture` : le greffon répond, et dit pourquoi il ne
/// capte rien.
pub struct AucunePile;

pub const CAPTURE_NON_COMPILEE: &str =
    "la capture audio n'est pas compilée dans ce serveur (feature local-audio absente)";

impl Peripheriques for AucunePile {
    fn pile(&self) -> &'static str {
        "aucune"
    }
    fn lister(&self) -> Result<Vec<DescriptionEntree>, String> {
        Err(CAPTURE_NON_COMPILEE.into())
    }
    fn format_natif(&self, _: &str) -> Result<(String, FormatPcm), String> {
        Err(CAPTURE_NON_COMPILEE.into())
    }
    fn demarrer(&self, _: &str, _: Arc<Anneau>) -> Result<CaptureDemarree, String> {
        Err(CAPTURE_NON_COMPILEE.into())
    }
    fn frequence_courante(&self, _: &str) -> Option<u32> {
        None
    }
}

/// Les périphériques du système : cpal avec la feature `capture`.
pub fn du_systeme() -> Arc<dyn Peripheriques> {
    #[cfg(feature = "capture")]
    {
        Arc::new(crate::systeme::Systeme)
    }
    #[cfg(not(feature = "capture"))]
    {
        Arc::new(AucunePile)
    }
}
