//! L'étage fixe **Volume** : le volume logiciel de la zone, **en flottant
//! tout au bout** (Q9), dans le fil de RENDU, après l'anneau.
//!
//! # Pourquoi au rendu (#7264)
//!
//! Pondérer AVANT l'anneau faisait attendre chaque geste toute la profondeur
//! de l'anneau (deux secondes en WASAPI exclusif). Ici, la consigne est relue
//! à CHAQUE bloc rendu, puis glisse en [`super::RAMPE_MS`] : un geste
//! s'entend au bloc suivant du pilote, quelle que soit la profondeur de
//! l'anneau. Délai borné : une période de pilote + la rampe (mesuré par
//! `latence_du_volume_mesuree`, dans les épreuves).
//!
//! # Ce que la consigne contient
//!
//! Le gain linéaire RENDU : volume de l'utilisateur × compensation × part
//! positive du ReplayGain, **raboté à l'unité** ([`super::BilanDeNiveau::gain_de_rendu`]).
//! Il n'est jamais plus fort que la pleine échelle.
//!
//! # Bit-perfect
//!
//! Au gain unité et au repos, l'étage ne touche pas un échantillon. Un volume
//! nul rend des zéros exacts (pas de dither sur le silence, voir l'étape
//! finale).
//!
//! # Exception DSD (Q9)
//!
//! Vers DSD, le même étage se place juste avant le modulateur
//! ([`PlacementVolume::BeforeDsdModulator`]) ; seule sa place change.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::rampe::Rampe;
use crate::tess::etage::{Disposition, ErreurEtage, EtageTess, FormatFlux, verifier_tampon};
use crate::tess::resolution::PlacementVolume;

/// Code d'étape de l'étage (celui de `CodeEtape::Volume`).
pub const ID_VOLUME: &str = "volume";

/// La consigne partagée entre la commande de volume (n'importe quel fil) et
/// le fil de rendu : un `f64` dans un atomique. Ni verrou ni allocation.
#[derive(Debug)]
pub struct ConsigneVolume(AtomicU64);

impl ConsigneVolume {
    pub fn new(gain: f64) -> Arc<Self> {
        let c = Arc::new(Self(AtomicU64::new(1f64.to_bits())));
        c.poser(gain);
        c
    }

    /// Pose le gain linéaire rendu, borné à `[0, 1]`. Une valeur non finie
    /// est ignorée (la consigne précédente reste).
    pub fn poser(&self, gain: f64) {
        if gain.is_finite() {
            self.0
                .store(gain.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        }
    }

    pub fn lire(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }
}

/// L'étage Volume. Voir le module.
#[derive(Debug)]
pub struct EtageVolume {
    consigne: Arc<ConsigneVolume>,
    placement: PlacementVolume,
    rampe: Rampe,
    canaux: usize,
    max_trames: usize,
    prepare: bool,
    bypass: bool,
}

impl EtageVolume {
    pub fn new(consigne: Arc<ConsigneVolume>, placement: PlacementVolume) -> Self {
        let gain = consigne.lire();
        Self {
            consigne,
            placement,
            rampe: Rampe::au_repos(gain),
            canaux: 0,
            max_trames: 0,
            prepare: false,
            bypass: false,
        }
    }

    pub fn placement(&self) -> PlacementVolume {
        self.placement
    }

    /// Le gain appliqué à la prochaine trame.
    pub fn gain_courant(&self) -> f64 {
        self.rampe.courant()
    }
}

impl EtageTess for EtageVolume {
    fn id(&self) -> &str {
        ID_VOLUME
    }

    fn preparer(
        &mut self,
        entree: FormatFlux,
        max_trames: usize,
    ) -> Result<Disposition, ErreurEtage> {
        if entree.channels == 0 {
            return Err(ErreurEtage::FormatNonPrisEnCharge {
                channels: entree.channels,
                sample_rate_hz: entree.sample_rate_hz,
            });
        }
        self.canaux = usize::from(entree.channels);
        self.max_trames = max_trames;
        self.rampe.preparer(entree.sample_rate_hz);
        // Un flux qui s'ouvre part de la consigne courante, sans rampe.
        self.rampe.viser(self.consigne.lire());
        self.rampe.sauter();
        self.prepare = true;
        Ok(Disposition::sur_place(entree, max_trames, 0))
    }

    fn traiter_sur_place(&mut self, tampon: &mut [f64], trames: usize) -> Result<(), ErreurEtage> {
        if !self.prepare {
            return Err(ErreurEtage::NonPrepare);
        }
        if trames > self.max_trames {
            return Err(ErreurEtage::BlocTropGrand {
                trames,
                max: self.max_trames,
            });
        }
        verifier_tampon(tampon.len(), trames, self.canaux as u16)?;
        if self.bypass {
            return Ok(());
        }
        // Relue à CHAQUE bloc : c'est tout le remède de #7264.
        self.rampe.viser(self.consigne.lire());
        self.rampe.appliquer(tampon, trames, self.canaux);
        Ok(())
    }

    fn reinitialiser(&mut self) {
        self.rampe.viser(self.consigne.lire());
        self.rampe.sauter();
    }

    fn bypass(&self) -> bool {
        self.bypass
    }

    /// Le volume est un réglage de ZONE : « Bypass all » ne le contourne pas
    /// (contrat § 3). Seul l'hôte le contourne, quand le volume n'est pas
    /// logiciel (appareil, fixe) ; sans latence, l'entrée sort telle quelle.
    fn definir_bypass(&mut self, bypass: bool) {
        self.bypass = bypass;
    }
}
