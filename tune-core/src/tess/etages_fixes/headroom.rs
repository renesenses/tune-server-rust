//! L'étage fixe **Headroom** (entrée de la chaîne, Q9) : headroom de TESS
//! (auto −3 dB ou manuel), ReplayGain et gain de zone, en UN gain `f64`
//! appliqué sur place.
//!
//! Le gain posé est le [`BilanDeNiveau::gain_d_entree_db`] : il ne dépasse
//! jamais 0 dB (la part positive du ReplayGain et du gain de zone part au
//! rendu, sous le rabot du volume, comme aujourd'hui). À 0 dB et au repos,
//! l'étage ne touche pas un échantillon.
//!
//! Un changement en cours de lecture (piste suivante, greffon coché qui arme
//! l'automatique) glisse en [`super::RAMPE_MS`] : pas de marche.
//!
//! [`BilanDeNiveau::gain_d_entree_db`]: super::BilanDeNiveau::gain_d_entree_db

use serde_json::Value;

use super::rampe::{Rampe, lineaire};
use crate::tess::etage::{Disposition, ErreurEtage, EtageTess, FormatFlux, verifier_tampon};

/// Code d'étape de l'étage (celui de `CodeEtape::Headroom`).
pub const ID_HEADROOM: &str = "headroom";

/// L'étage Headroom. Voir le module.
#[derive(Debug)]
pub struct EtageHeadroom {
    rampe: Rampe,
    canaux: usize,
    max_trames: usize,
    prepare: bool,
    bypass: bool,
}

impl EtageHeadroom {
    /// Un étage au gain `gain_db` (≤ 0 dB ; voir [`Self::definir_gain_db`]).
    pub fn new(gain_db: f64) -> Result<Self, ErreurEtage> {
        let mut e = Self {
            rampe: Rampe::au_repos(1.0),
            canaux: 0,
            max_trames: 0,
            prepare: false,
            bypass: false,
        };
        e.definir_gain_db(gain_db)?;
        e.rampe.sauter();
        Ok(e)
    }

    /// Change le gain (sans allocation). Le changement glisse en
    /// [`super::RAMPE_MS`]. Refusé au-dessus de 0 dB ou non fini : l'entrée
    /// de la chaîne ne pousse jamais.
    pub fn definir_gain_db(&mut self, gain_db: f64) -> Result<(), ErreurEtage> {
        if !gain_db.is_finite() || gain_db > 0.0 {
            return Err(ErreurEtage::Reglages(
                "headroom : gain d'entrée fini et ≤ 0 dB",
            ));
        }
        self.rampe.viser(lineaire(gain_db));
        Ok(())
    }

    /// Le gain linéaire visé.
    pub fn gain_vise(&self) -> f64 {
        self.rampe.cible()
    }
}

impl EtageTess for EtageHeadroom {
    fn id(&self) -> &str {
        ID_HEADROOM
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
        if !self.bypass {
            self.rampe.appliquer(tampon, trames, self.canaux);
        }
        Ok(())
    }

    fn reinitialiser(&mut self) {
        self.rampe.sauter();
    }

    fn bypass(&self) -> bool {
        self.bypass
    }

    /// Sans latence : contourné, l'étage rend son entrée telle quelle.
    fn definir_bypass(&mut self, bypass: bool) {
        self.bypass = bypass;
    }

    /// `{"db": x}` : le gain d'entrée, comme [`Self::definir_gain_db`].
    fn appliquer_reglages(&mut self, reglages: &Value) -> Result<(), ErreurEtage> {
        let db = reglages
            .get("db")
            .and_then(Value::as_f64)
            .ok_or(ErreurEtage::Reglages("headroom : champ « db » attendu"))?;
        self.definir_gain_db(db)
    }
}
