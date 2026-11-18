//! Les **étages fixes** de TESS (lot T07), dans l'ordre réel de Q9 :
//!
//! ```text
//! Source → Headroom → greffons → Upsampling → Volume → Quantization → Output
//!                                              Volume → DsdModulator   (vers DSD)
//! ```
//!
//! - [`EtageHeadroom`] : headroom de TESS (auto −3 dB dès qu'un traitement
//!   est effectif, sinon 0 dB), ReplayGain et gain de zone, en un gain ;
//! - [`EtageVolume`] : le volume logiciel en `f64`, au RENDU, relu à chaque
//!   bloc (< 50 ms, #7264) ;
//! - [`BilanDeNiveau`] : headroom, réserves des greffons et compensation de
//!   niveau, chacun compté une fois ;
//! - [`QuantificationMiseEnForme`] : l'étape finale `noise_shaped` (TPDF et
//!   `off` sont la `QuantificationPcm` de T03) ;
//! - [`choisir_etape_finale`] : PCM ou modulateur DSD (T11b), d'après la
//!   résolution ;
//! - [`EtageUpsampling`] (T11a) : l'upsampling PCM en `f64`, changement de
//!   filtre à chaud, sans la réserve de 3 dB de #7614.
//!
//! Ce module ne branche RIEN dans la lecture : l'exécuteur de T03 monte ces
//! étages (`outputs/local.rs`), une fois fusionné.

mod finale;
mod headroom;
mod mise_en_forme;
mod niveau;
mod rampe;
mod upsampling;
mod volume;

#[cfg(test)]
mod epreuves_tests;

pub use finale::{EtapeFinaleChoisie, choisir_etape_finale, placement_du_volume};
pub use headroom::{EtageHeadroom, ID_HEADROOM};
pub use mise_en_forme::{QuantificationMiseEnForme, formeur_pour};
pub use niveau::{BilanDeNiveau, EntreesNiveau, GainDeRendu};
pub use rampe::{RAMPE_MS, decibels, lineaire};
pub use upsampling::{
    ChangementUpsampling, EtageUpsampling, ID_UPSAMPLING, MotifUpsampling, ReglagesUpsamplingPcm,
    negocier, planifier_changement, transport_porte_l_upsampling,
};
pub use volume::{ConsigneVolume, EtageVolume, ID_VOLUME};
