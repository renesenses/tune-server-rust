//! Suréchantillonnage PCM → cadence DSD, en `f64`.
//!
//! Deux étages, parce qu'un seul filtre à × 256 coûterait des milliers de
//! coefficients par échantillon :
//!
//! 1. **polyphase** × `l1` (au plus 8) : sinus cardinal fenêtré par Kaiser
//!    (β = 11, environ −110 dB en bande coupée), bande passante jusqu'à
//!    0,45 × Fs d'entrée, bande coupée dès 0,55 × Fs : les images du spectre
//!    audio sont rejetées ;
//! 2. **linéaire** × `l2` (le reste du rapport) : à 8 × Fs, les images
//!    restantes sont autour de 8 × Fs, très loin de la bande audio ; la
//!    réponse en sinus cardinal au carré les atténue encore d'environ 50 dB,
//!    et la bande audio n'en est pas touchée de plus de 0,03 dB (20 kHz à
//!    352,8 kHz).
//!
//! Aucune allocation dans [`Interpolateur::produire`] : historiques et tampons
//! sont réservés à la construction.

/// Coefficients par phase du filtre polyphase.
pub const COEFFICIENTS_PAR_PHASE: usize = 80;

/// Bêta de la fenêtre de Kaiser (≈ 110 dB d'atténuation).
const BETA_KAISER: f64 = 11.0;

/// Fonction de Bessel modifiée de première espèce, ordre 0 (série).
fn bessel_i0(x: f64) -> f64 {
    let mut somme = 1.0;
    let mut terme = 1.0;
    let demi = x / 2.0;
    for k in 1..200 {
        terme *= (demi / k as f64) * (demi / k as f64);
        somme += terme;
        if terme < 1e-17 * somme {
            break;
        }
    }
    somme
}

/// Filtre prototype passe-bas de `l1 × COEFFICIENTS_PAR_PHASE` coefficients,
/// coupure à 0,5 × Fs d'entrée, gain `l1` (compense les zéros insérés).
pub(crate) fn prototype(l1: usize) -> Vec<f64> {
    let n = l1 * COEFFICIENTS_PAR_PHASE;
    let centre = (n - 1) as f64 / 2.0;
    let fc = 0.5 / l1 as f64; // en cycles par échantillon de sortie
    let i0_beta = bessel_i0(BETA_KAISER);
    (0..n)
        .map(|i| {
            let t = i as f64 - centre;
            let sinc = if t == 0.0 {
                2.0 * fc
            } else {
                (2.0 * std::f64::consts::PI * fc * t).sin() / (std::f64::consts::PI * t)
            };
            let r = t / centre;
            let fenetre = bessel_i0(BETA_KAISER * (1.0 - r * r).max(0.0).sqrt()) / i0_beta;
            sinc * fenetre * l1 as f64
        })
        .collect()
}

/// Suréchantillonneur d'un flux entrelacé, canal par canal.
#[derive(Debug, Clone)]
pub struct Interpolateur {
    canaux: usize,
    l1: usize,
    l2: usize,
    /// Coefficients rangés par phase : `phases[p][k] = h[p + k·l1]`.
    phases: Vec<[f64; COEFFICIENTS_PAR_PHASE]>,
    /// Historique circulaire, doublé pour lire d'un seul tenant :
    /// `2 × COEFFICIENTS_PAR_PHASE` par canal.
    historique: Vec<f64>,
    position: usize,
    /// Dernier échantillon à `l1 × Fs`, par canal (étage linéaire).
    precedent: Vec<f64>,
    tampon_l1: Vec<f64>,
}

impl Interpolateur {
    /// `rapport` = cadence DSD / cadence PCM, puissance de deux ≥ 8.
    pub fn new(canaux: usize, rapport: usize) -> Option<Self> {
        if canaux == 0 || rapport < 8 || !rapport.is_power_of_two() {
            return None;
        }
        let l1 = rapport.min(8);
        let l2 = rapport / l1;
        let h = prototype(l1);
        let phases = (0..l1)
            .map(|p| {
                let mut c = [0.0; COEFFICIENTS_PAR_PHASE];
                for (k, ck) in c.iter_mut().enumerate() {
                    *ck = h[p + k * l1];
                }
                c
            })
            .collect();
        Some(Self {
            canaux,
            l1,
            l2,
            phases,
            historique: vec![0.0; canaux * 2 * COEFFICIENTS_PAR_PHASE],
            position: 0,
            precedent: vec![0.0; canaux],
            tampon_l1: vec![0.0; l1],
        })
    }

    pub fn rapport(&self) -> usize {
        self.l1 * self.l2
    }

    /// Retard de groupe, en échantillons de sortie (cadence DSD).
    pub fn retard(&self) -> f64 {
        // Polyphase : (N − 1) / 2 échantillons à l1 × Fs ; linéaire : un
        // échantillon à l1 × Fs.
        let n = (self.l1 * COEFFICIENTS_PAR_PHASE) as f64;
        ((n - 1.0) / 2.0 + 1.0) * self.l2 as f64
    }

    /// Remet les historiques à zéro (nouvelle piste, saut).
    pub fn reinitialiser(&mut self) {
        self.historique.fill(0.0);
        self.precedent.fill(0.0);
        self.position = 0;
    }

    /// Pousse la trame `trame` (un échantillon par canal) dans les
    /// historiques. À appeler une fois par trame, avant [`Self::produire`].
    pub fn pousser(&mut self, trame: &[f64]) {
        let t = COEFFICIENTS_PAR_PHASE;
        self.position = if self.position == 0 {
            t - 1
        } else {
            self.position - 1
        };
        for (c, &x) in trame.iter().take(self.canaux).enumerate() {
            let base = c * 2 * t;
            self.historique[base + self.position] = x;
            self.historique[base + self.position + t] = x;
        }
    }

    /// Les `rapport()` échantillons du canal `canal` pour la trame poussée.
    pub fn produire(&mut self, canal: usize, sortie: &mut [f64]) {
        let t = COEFFICIENTS_PAR_PHASE;
        let base = canal * 2 * t + self.position;
        let fenetre = &self.historique[base..base + t];
        for (p, y) in self.tampon_l1.iter_mut().enumerate() {
            *y = self.phases[p].iter().zip(fenetre).map(|(h, x)| h * x).sum();
        }
        let l2 = self.l2;
        let pas = 1.0 / l2 as f64;
        let mut precedent = self.precedent[canal];
        for (p, &courant) in self.tampon_l1.iter().enumerate() {
            let tranche = &mut sortie[p * l2..(p + 1) * l2];
            let ecart = courant - precedent;
            for (j, y) in tranche.iter_mut().enumerate() {
                *y = precedent + ecart * (j as f64 * pas);
            }
            precedent = courant;
        }
        self.precedent[canal] = precedent;
    }
}
