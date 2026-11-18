//! La rampe de gain partagée par le headroom et le volume : un facteur par
//! trame, qui glisse vers sa cible en [`RAMPE_MS`] sans marche (donc sans
//! clic), et qui laisse le tampon INTACT au gain unité au repos.

/// Durée d'un changement de gain. 20 ms : celle du volume au rendu de #7264
/// (`RAMPE_DU_VOLUME_MS`) et de la rampe anti-« ploc » (#1590).
pub const RAMPE_MS: u32 = 20;

/// État de la rampe : quatre `f64`, ni allocation ni verrou.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Rampe {
    cible: f64,
    courant: f64,
    pas: f64,
    trames_de_rampe: f64,
}

impl Rampe {
    /// Une rampe AU REPOS au gain `gain` : l'ouverture d'un flux ne fond
    /// jamais depuis l'unité.
    pub(crate) fn au_repos(gain: f64) -> Self {
        Self {
            cible: gain,
            courant: gain,
            pas: 0.0,
            trames_de_rampe: 1.0,
        }
    }

    /// HORS TEMPS RÉEL : la longueur de la rampe pour une cadence.
    pub(crate) fn preparer(&mut self, cadence: u32) {
        self.trames_de_rampe = (f64::from(cadence.max(1)) * f64::from(RAMPE_MS) / 1000.0).max(1.0);
    }

    pub(crate) fn courant(&self) -> f64 {
        self.courant
    }

    pub(crate) fn cible(&self) -> f64 {
        self.cible
    }

    /// Une nouvelle cible arme une rampe depuis le gain atteint.
    pub(crate) fn viser(&mut self, cible: f64) {
        if cible != self.cible {
            self.cible = cible;
            self.pas = (cible - self.courant).abs() / self.trames_de_rampe;
        }
    }

    /// Saute à la cible (saut, piste non gapless) : rien n'est en vol.
    pub(crate) fn sauter(&mut self) {
        self.courant = self.cible;
        self.pas = 0.0;
    }

    /// TEMPS RÉEL. Applique le gain à `trames` trames de `canaux` canaux.
    /// Au repos à l'unité, le tampon n'est pas touché : c'est ce qui garde le
    /// bit-perfect (une multiplication par 1,0 serait exacte aussi, mais ne
    /// rien faire ne coûte rien et se prouve mieux).
    pub(crate) fn appliquer(&mut self, tampon: &mut [f64], trames: usize, canaux: usize) {
        let n = trames * canaux;
        if self.courant == self.cible {
            if self.courant != 1.0 {
                let g = self.courant;
                tampon[..n].iter_mut().for_each(|x| *x *= g);
            }
            return;
        }
        for trame in tampon[..n].chunks_exact_mut(canaux) {
            let g = self.courant;
            trame.iter_mut().for_each(|x| *x *= g);
            if self.courant != self.cible {
                self.courant = if self.cible > self.courant {
                    (self.courant + self.pas).min(self.cible)
                } else {
                    (self.courant - self.pas).max(self.cible)
                };
            }
        }
    }
}

/// dB → linéaire. Une valeur non finie vaut l'unité (0 dB) : un réglage
/// illisible ne coupe ni ne pousse jamais le son.
pub fn lineaire(db: f64) -> f64 {
    if db.is_finite() {
        10f64.powf(db / 20.0)
    } else {
        1.0
    }
}

/// Linéaire → dB (`-inf` pour 0).
pub fn decibels(lineaire: f64) -> f64 {
    20.0 * lineaire.log10()
}
