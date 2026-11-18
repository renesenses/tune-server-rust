//! Quelle étape finale pour une chaîne résolue (Q9) : quantification PCM
//! (TPDF, mise en forme du bruit ou aucun dither) ou modulateur DSD.
//!
//! - TPDF et `off` : [`crate::tess::quantification`] (T03, `QuantificationPcm`),
//!   branchée par l'exécuteur ;
//! - `noise_shaped` : [`super::QuantificationMiseEnForme`] (ce lot) ;
//! - DSD : le modulateur sigma-delta est le lot T11b. Son STATUT est fixé
//!   par le contrat (§ 4) : étape `dsd_modulator`, effective dès que
//!   l'upsampling vers DSD l'est, volume juste avant lui
//!   (`before_dsd_modulator`), sortie `SortieFinale::Dsd`.

use crate::tess::chaine::{ChaineTess, Dither, TauxDsd, Upsampling};
use crate::tess::resolution::{CodeEtape, PlacementVolume, Resolution};

/// L'étape finale que l'hôte doit monter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EtapeFinaleChoisie {
    /// Quantification PCM. `dither` faux = `off` (arrondi nu) ;
    /// `mise_en_forme` vrai = `noise_shaped`.
    Pcm { dither: bool, mise_en_forme: bool },
    /// Modulateur sigma-delta (T11b), précédé du volume.
    ModulateurDsd { taux: TauxDsd },
}

/// Choisit l'étape finale d'après la résolution (qui dit si l'upsampling
/// vers DSD est effectif) et le réglage `quantization.dither`.
///
/// Une valeur de dither inconnue (chaîne d'un serveur plus récent) retombe
/// sur TPDF, le défaut du contrat : jamais d'arrondi nu par accident.
pub fn choisir_etape_finale(chaine: &ChaineTess, resolution: &Resolution) -> EtapeFinaleChoisie {
    let dsd = resolution
        .etapes
        .iter()
        .any(|e| e.code == CodeEtape::DsdModulator && e.effective);
    if dsd && let Upsampling::Dsd { rate, .. } = &chaine.fixed.upsampling {
        return EtapeFinaleChoisie::ModulateurDsd { taux: *rate };
    }
    match chaine.fixed.quantization.dither {
        Dither::Off => EtapeFinaleChoisie::Pcm {
            dither: false,
            mise_en_forme: false,
        },
        Dither::NoiseShaped => EtapeFinaleChoisie::Pcm {
            dither: true,
            mise_en_forme: true,
        },
        Dither::Tpdf | Dither::Unknown => EtapeFinaleChoisie::Pcm {
            dither: true,
            mise_en_forme: false,
        },
    }
}

/// La place du volume que l'hôte doit monter, `None` quand le volume
/// logiciel n'est pas appliqué (appareil, fixe, hors TESS, DSD natif).
pub fn placement_du_volume(resolution: &Resolution) -> Option<PlacementVolume> {
    resolution.volume.placement
}
