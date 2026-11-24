//! Conception de la fonction de transfert du bruit (NTF) du modulateur.
//!
//! - **Zéros** sur le cercle unité, répartis dans la bande utile aux racines
//!   du polynôme de Legendre de l'ordre choisi : c'est la répartition qui
//!   minimise la puissance du bruit dans la bande (Schreier, « optimized NTF
//!   zeros »).
//! - **Pôles** d'un passe-haut de Butterworth du même ordre (transformation
//!   bilinéaire), dont la coupure est cherchée par dichotomie pour que le
//!   gain maximal du NTF hors bande vaille exactement `hinf` (critère de
//!   Lee : plus `hinf` est bas, plus le modulateur à 1 bit est stable, au
//!   prix d'un peu de bruit dans la bande).
//! - NTF(z) = N(z) / D(z), deux polynômes unitaires du même degré : NTF(∞) = 1,
//!   la réponse impulsionnelle commence par 1 et la boucle
//!   `H = NTF − 1 = (N − D) / D` est strictement causale.
//!
//! Tout est calculé en `f64`, une fois, à la construction (hors temps réel).

/// Nombre complexe minimal, pour la conception seulement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Complexe {
    pub re: f64,
    pub im: f64,
}

impl Complexe {
    pub const fn new(re: f64, im: f64) -> Self {
        Self { re, im }
    }
    pub fn polaire(module: f64, angle: f64) -> Self {
        Self::new(module * angle.cos(), module * angle.sin())
    }
    pub fn module(self) -> f64 {
        self.re.hypot(self.im)
    }
    pub fn plus(self, o: Self) -> Self {
        Self::new(self.re + o.re, self.im + o.im)
    }
    pub fn moins(self, o: Self) -> Self {
        Self::new(self.re - o.re, self.im - o.im)
    }
    pub fn fois(self, o: Self) -> Self {
        Self::new(
            self.re * o.re - self.im * o.im,
            self.re * o.im + self.im * o.re,
        )
    }
    pub fn sur(self, o: Self) -> Self {
        let d = o.re * o.re + o.im * o.im;
        Self::new(
            (self.re * o.re + self.im * o.im) / d,
            (self.im * o.re - self.re * o.im) / d,
        )
    }
    pub fn echelle(self, k: f64) -> Self {
        Self::new(self.re * k, self.im * k)
    }
}

/// Racines du polynôme de Legendre (nœuds de Gauss-Legendre sur [−1, 1]),
/// pour les ordres que le modulateur accepte.
pub(crate) fn noeuds_de_legendre(ordre: usize) -> Option<&'static [f64]> {
    const O5: [f64; 5] = [
        -0.906_179_845_938_664,
        -0.538_469_310_105_683_1,
        0.0,
        0.538_469_310_105_683_1,
        0.906_179_845_938_664,
    ];
    const O7: [f64; 7] = [
        -0.949_107_912_342_758_5,
        -0.741_531_185_599_394_4,
        -0.405_845_151_377_397_2,
        0.0,
        0.405_845_151_377_397_2,
        0.741_531_185_599_394_4,
        0.949_107_912_342_758_5,
    ];
    match ordre {
        5 => Some(&O5),
        7 => Some(&O7),
        _ => None,
    }
}

/// Un NTF conçu : zéros, pôles et coefficients des deux polynômes, du plus
/// haut degré (coefficient 1) au terme constant.
#[derive(Debug, Clone)]
pub struct Ntf {
    pub ordre: usize,
    pub osr: f64,
    pub hinf: f64,
    pub(crate) zeros: Vec<Complexe>,
    pub(crate) poles: Vec<Complexe>,
    /// N(z), `ordre + 1` coefficients, `numerateur[0] = 1`.
    pub numerateur: Vec<f64>,
    /// D(z), `ordre + 1` coefficients, `denominateur[0] = 1`.
    pub denominateur: Vec<f64>,
}

/// Développe Π (z − r) en coefficients réels (les racines viennent par paires
/// conjuguées, la partie imaginaire résiduelle est un arrondi).
fn developper(racines: &[Complexe]) -> Vec<f64> {
    let mut c = vec![Complexe::new(1.0, 0.0)];
    for &r in racines {
        let mut suivant = vec![Complexe::new(0.0, 0.0); c.len() + 1];
        for (i, &ci) in c.iter().enumerate() {
            suivant[i] = suivant[i].plus(ci);
            suivant[i + 1] = suivant[i + 1].moins(ci.fois(r));
        }
        c = suivant;
    }
    c.into_iter().map(|x| x.re).collect()
}

/// Pôles du passe-haut de Butterworth d'ordre `ordre`, coupure numérique
/// `wc` (radians par échantillon), par transformation bilinéaire.
fn poles_butterworth_passe_haut(ordre: usize, wc: f64) -> Vec<Complexe> {
    let wa = 2.0 * (wc / 2.0).tan();
    let un = Complexe::new(1.0, 0.0);
    (0..ordre)
        .map(|k| {
            let angle = std::f64::consts::PI * (2 * k + ordre + 1) as f64 / (2 * ordre) as f64;
            let s_bas = Complexe::polaire(1.0, angle);
            let s_haut = Complexe::new(wa, 0.0).sur(s_bas);
            un.plus(s_haut.echelle(0.5))
                .sur(un.moins(s_haut.echelle(0.5)))
        })
        .collect()
}

/// |NTF(e^{jω})|.
pub(crate) fn module_en(zeros: &[Complexe], poles: &[Complexe], w: f64) -> f64 {
    let z = Complexe::polaire(1.0, w);
    let mut h = Complexe::new(1.0, 0.0);
    for (&zz, &pp) in zeros.iter().zip(poles) {
        h = h.fois(z.moins(zz)).sur(z.moins(pp));
    }
    h.module()
}

/// Gain maximal du NTF sur [0, π] (grille de 4 096 points).
pub(crate) fn gain_maximal(zeros: &[Complexe], poles: &[Complexe]) -> f64 {
    const POINTS: usize = 4096;
    (0..=POINTS)
        .map(|i| {
            module_en(
                zeros,
                poles,
                std::f64::consts::PI * i as f64 / POINTS as f64,
            )
        })
        .fold(0.0, f64::max)
}

impl Ntf {
    /// Conçoit le NTF d'ordre `ordre` (5 ou 7) pour un taux de
    /// suréchantillonnage `osr` (la bande utile est [0, π / osr]) et un gain
    /// maximal hors bande `hinf` (> 1).
    pub fn concevoir(ordre: usize, osr: f64, hinf: f64) -> Option<Self> {
        let noeuds = noeuds_de_legendre(ordre)?;
        if !(osr.is_finite() && osr >= 8.0 && hinf.is_finite() && hinf > 1.0) {
            return None;
        }
        let bande = std::f64::consts::PI / osr;
        let zeros: Vec<Complexe> = noeuds
            .iter()
            .map(|&x| Complexe::polaire(1.0, x * bande))
            .collect();
        // Dichotomie sur la coupure : le gain hors bande croît avec elle.
        let (mut bas, mut haut) = (1e-5_f64, 3.0_f64);
        for _ in 0..80 {
            let milieu = 0.5 * (bas + haut);
            if gain_maximal(&zeros, &poles_butterworth_passe_haut(ordre, milieu)) > hinf {
                haut = milieu;
            } else {
                bas = milieu;
            }
        }
        let poles = poles_butterworth_passe_haut(ordre, bas);
        if poles.iter().any(|p| p.module() >= 1.0) {
            return None;
        }
        let numerateur = developper(&zeros);
        let denominateur = developper(&poles);
        Some(Self {
            ordre,
            osr,
            hinf,
            zeros,
            poles,
            numerateur,
            denominateur,
        })
    }

    /// |NTF| à la pulsation `w` (radians par échantillon).
    pub fn module_en(&self, w: f64) -> f64 {
        module_en(&self.zeros, &self.poles, w)
    }

    /// Gain maximal réel du NTF conçu (≈ `hinf`).
    pub fn gain_maximal(&self) -> f64 {
        gain_maximal(&self.zeros, &self.poles)
    }

    /// Le plus grand module des pôles (< 1 : NTF stable).
    pub fn rayon_des_poles(&self) -> f64 {
        self.poles.iter().map(|p| p.module()).fold(0.0, f64::max)
    }
}
