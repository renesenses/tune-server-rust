//! Dither TPDF et mise en forme du bruit par rétroaction d'erreur.
//!
//! Structure (Gerzon & Craven 1989 ; Lipshitz, Wannamaker & Vanderkooy 1991) :
//!
//! ```text
//!   v[n] = x[n] − Σ_{k=1..N} c_k · e[n−k]
//!   y[n] = Q( v[n] + d[n] )          d : TPDF ±1 LSB
//!   e[n] = y[n] − v[n]
//! ```
//!
//! d'où `Y(z) = X(z) + (1 − Σ c_k z^{−k}) · E(z)` : la fonction de transfert du
//! bruit (NTF) vaut `1 − C(z)`. Les coefficients `c_k` publiés ci-dessous sont
//! donnés dans cette convention (celle des articles et de la plupart des
//! implémentations libres) ; le banc trace la NTF réellement obtenue, ce qui
//! vérifie le signe.
//!
//! Références :
//! * S. P. Lipshitz, R. A. Wannamaker, J. Vanderkooy, « Minimally Audible
//!   Noise Shaping », *J. Audio Eng. Soc.* 39(11), 1991 — formeurs pondérés
//!   E (5ᵉ ordre) ; TPDF et moments de l'erreur.
//! * R. A. Wannamaker, « Psychoacoustically Optimal Noise Shaping »,
//!   *J. Audio Eng. Soc.* 40(7/8), 1992 — formeurs pondérés F, 3ᵉ et 9ᵉ ordre,
//!   et pondération E améliorée.
//! * M. A. Gerzon, P. G. Craven, « Optimal Noise Shaping and Dither of Digital
//!   Signals », AES Convention 87, 1989 — théorème du gain de bruit
//!   (`∫ log|NTF| = 0`).
//! * E. Terhardt, « Calculating virtual pitch », *Hearing Research* 1, 1979 —
//!   formule analytique du seuil d'audition absolue utilisée par le banc.
//!
//! Le dither est **déterministe** (graine explicite), comme celui de
//! `tune-plugin-audio-support::dither` : la règle `depuis_contenu` du serveur
//! s'applique telle quelle à l'intégration.

/// xorshift64* : simple, rapide, période 2⁶⁴ − 1, bien suffisant pour un
/// dither (il ne s'agit pas de cryptographie).
pub struct Prng(u64);

impl Prng {
    pub fn nouveau(graine: u64) -> Self {
        Self((graine ^ 0x9E37_79B9_7F4A_7C15) | 1)
    }
    #[inline]
    pub fn suivant(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// Uniforme sur `[0, 1)`, 53 bits.
    #[inline]
    pub fn uniforme(&mut self) -> f64 {
        (self.suivant() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
    /// Triangulaire sur `(−1, 1)` : différence de deux uniformes.
    #[inline]
    pub fn tpdf(&mut self) -> f64 {
        self.uniforme() - self.uniforme()
    }
}

/// Un formeur de bruit : ses coefficients `c_k` et son origine.
#[derive(Clone, Debug, PartialEq)]
pub struct Formeur {
    pub id: &'static str,
    pub nom: &'static str,
    /// Cadences pour lesquelles la courbe a été optimisée ; vide = toutes
    /// (formeurs géométriques, sans hypothèse psychoacoustique).
    pub cadences: &'static [u32],
    pub coefficients: Vec<f64>,
    pub reference: &'static str,
}

impl Formeur {
    pub fn ordre(&self) -> usize {
        self.coefficients.len()
    }
    /// Gain de puissance TOTAL du bruit : `1 + Σ c_k²` (le bruit déplacé
    /// hors de la bande sensible s'ajoute, il ne disparaît pas).
    pub fn gain_de_bruit_total_db(&self) -> f64 {
        10.0 * (1.0 + self.coefficients.iter().map(|c| c * c).sum::<f64>()).log10()
    }
    pub fn convient(&self, cadence: u32) -> bool {
        self.cadences.is_empty() || self.cadences.contains(&cadence)
    }
}

const CD: &[u32] = &[44_100, 48_000];

/// Le catalogue : d'abord les formeurs géométriques d'ordre croissant
/// (NTF = (1 − z⁻¹)ⁿ, coefficients binomiaux), valables à toute cadence ;
/// puis les formeurs psychoacoustiques publiés pour 44,1 kHz.
pub fn formeurs() -> Vec<Formeur> {
    vec![
        Formeur {
            id: "ordre1",
            nom: "Passe-haut d'ordre 1 (1 − z⁻¹)",
            cadences: &[],
            coefficients: vec![1.0],
            reference: "Gerzon & Craven 1989 (forme élémentaire)",
        },
        Formeur {
            id: "ordre2",
            nom: "Passe-haut d'ordre 2 (1 − z⁻¹)²",
            cadences: &[],
            coefficients: vec![2.0, -1.0],
            reference: "Gerzon & Craven 1989 (forme élémentaire)",
        },
        Formeur {
            id: "ordre3",
            nom: "Passe-haut d'ordre 3 (1 − z⁻¹)³",
            cadences: &[],
            coefficients: vec![3.0, -3.0, 1.0],
            reference: "coefficients binomiaux, calculés",
        },
        Formeur {
            id: "ordre4",
            nom: "Passe-haut d'ordre 4 (1 − z⁻¹)⁴",
            cadences: &[],
            coefficients: vec![4.0, -6.0, 4.0, -1.0],
            reference: "coefficients binomiaux, calculés",
        },
        Formeur {
            id: "wannamaker3f",
            nom: "Pondéré F, ordre 3 (Wannamaker 1992)",
            cadences: CD,
            coefficients: vec![1.623, -0.982, 0.109],
            reference: "Wannamaker, JAES 40(7/8), 1992, tableau des filtres F",
        },
        Formeur {
            id: "lipshitz5e",
            nom: "Pondéré E, ordre 5 (Lipshitz et al. 1991)",
            cadences: CD,
            coefficients: vec![2.033, -2.165, 1.959, -1.590, 0.6149],
            reference: "Lipshitz, Wannamaker & Vanderkooy, JAES 39(11), 1991",
        },
        Formeur {
            id: "wannamaker9f",
            nom: "Pondéré F, ordre 9 (Wannamaker 1992)",
            cadences: CD,
            coefficients: vec![
                2.412, -3.370, 3.937, -4.174, 3.353, -2.205, 1.281, -0.569, 0.0847,
            ],
            reference: "Wannamaker, JAES 40(7/8), 1992, tableau des filtres F",
        },
        Formeur {
            id: "wannamaker9e",
            nom: "Pondéré E amélioré, ordre 9 (Wannamaker 1992)",
            cadences: CD,
            coefficients: vec![
                2.847, -4.685, 6.214, -7.184, 6.639, -5.032, 3.263, -1.632, 0.4191,
            ],
            reference: "Wannamaker, JAES 40(7/8), 1992, pondération E améliorée",
        },
    ]
}

pub fn formeur_par_id(id: &str) -> Option<Formeur> {
    formeurs().into_iter().find(|f| f.id == id)
}

/// Les formeurs proposables pour une cadence de sortie.
pub fn formeurs_pour(cadence: u32) -> Vec<Formeur> {
    formeurs().into_iter().filter(|f| f.convient(cadence)).collect()
}

/// Réduction de profondeur avec dither TPDF et, en option, mise en forme du
/// bruit. Les échantillons sont en pleine échelle `[−1, 1)`.
pub struct Quantificateur {
    bits: u32,
    pas: f64,
    tpdf: bool,
    c: Vec<f64>,
    /// Anneaux d'erreur, un par canal, `ordre` cases chacun.
    e: Vec<f64>,
    idx: Vec<usize>,
    canaux: usize,
    prng: Prng,
}

impl Quantificateur {
    pub fn nouveau(
        bits: u32,
        tpdf: bool,
        formeur: Option<&Formeur>,
        canaux: usize,
        graine: u64,
    ) -> Self {
        assert!((2..=32).contains(&bits) && canaux >= 1);
        let c = formeur.map_or_else(Vec::new, |f| f.coefficients.clone());
        let ordre = c.len().max(1);
        Self {
            bits,
            pas: 2f64.powi(-(bits as i32 - 1)),
            tpdf,
            c,
            e: vec![0.0; canaux * ordre],
            idx: vec![0; canaux],
            canaux,
            prng: Prng::nouveau(graine),
        }
    }

    pub fn bits(&self) -> u32 {
        self.bits
    }
    pub fn pas(&self) -> f64 {
        self.pas
    }

    /// Quantifie un échantillon d'un canal. Rend la valeur sur la grille.
    #[inline]
    pub fn quantifier(&mut self, x: f64, canal: usize) -> f64 {
        let n = self.c.len();
        let mut v = x;
        if n > 0 {
            let base = canal * n;
            let idx = self.idx[canal];
            let mut retro = 0.0;
            for k in 0..n {
                retro += self.c[k] * self.e[base + (idx + n - k) % n];
            }
            v -= retro;
        }
        let d = if self.tpdf { self.prng.tpdf() } else { 0.0 };
        let q = ((v + d * self.pas) / self.pas).round() * self.pas;
        let y = q.clamp(-1.0, 1.0 - self.pas);
        if n > 0 {
            let base = canal * n;
            // Erreur bornée : un écrêtage ne doit pas emballer la boucle.
            let err = (y - v).clamp(-2.0 * self.pas, 2.0 * self.pas);
            let idx = (self.idx[canal] + 1) % n;
            self.e[base + idx] = err;
            self.idx[canal] = idx;
        }
        y
    }

    /// Quantifie un bloc entrelacé en place.
    pub fn traiter(&mut self, bloc: &mut [f64]) {
        let c = self.canaux;
        for trame in bloc.chunks_exact_mut(c) {
            for (ch, x) in trame.iter_mut().enumerate() {
                *x = self.quantifier(*x, ch);
            }
        }
    }
}

/// Module de la NTF `|1 − Σ c_k e^{−jωk}|` en dB, sur `points` fréquences de
/// 10 Hz à `cadence / 2` (espacement logarithmique).
pub fn ntf_db(coefficients: &[f64], cadence: u32, points: usize) -> Vec<(f64, f64)> {
    let fmin = 10.0_f64;
    let fmax = cadence as f64 / 2.0;
    (0..points)
        .map(|i| {
            let f = fmin * (fmax / fmin).powf(i as f64 / (points - 1) as f64);
            let w = 2.0 * std::f64::consts::PI * f / cadence as f64;
            let mut re = 1.0;
            let mut im = 0.0;
            for (k, c) in coefficients.iter().enumerate() {
                let a = w * (k + 1) as f64;
                re -= c * a.cos();
                im += c * a.sin();
            }
            (f, 20.0 * re.hypot(im).max(1e-30).log10())
        })
        .collect()
}

/// Seuil d'audition absolue (Terhardt 1979), en dB SPL, pour `f_hz`.
pub fn ath_terhardt_db(f_hz: f64) -> f64 {
    let f = (f_hz / 1000.0).max(0.02);
    3.64 * f.powf(-0.8) - 6.5 * (-0.6 * (f - 3.3).powi(2)).exp() + 1e-3 * f.powi(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prng_uniforme_centre() {
        let mut p = Prng::nouveau(42);
        let n = 200_000;
        let m: f64 = (0..n).map(|_| p.uniforme()).sum::<f64>() / n as f64;
        assert!((m - 0.5).abs() < 0.005, "{m}");
        let v: f64 = (0..n).map(|_| p.tpdf().powi(2)).sum::<f64>() / n as f64;
        assert!((v - 1.0 / 6.0).abs() < 0.005, "variance TPDF {v}");
    }

    #[test]
    fn ntf_ordre1_vaut_zero_en_continu_et_6db_a_nyquist() {
        let c = ntf_db(&[1.0], 48_000, 512);
        assert!(c[0].1 < -50.0, "{}", c[0].1);
        assert!((c[511].1 - 6.02).abs() < 0.05);
    }

    #[test]
    fn formeurs_publies_stables() {
        for f in formeurs() {
            // Le gain total reste raisonnable (< 30 dB) : un signe inversé
            // donnerait une NTF énorme en basses fréquences.
            assert!(f.gain_de_bruit_total_db() < 30.0, "{}", f.id);
            let ntf = ntf_db(&f.coefficients, 44_100, 256);
            assert!(ntf[0].1 < 10.0, "{} : NTF {} dB en continu", f.id, ntf[0].1);
        }
    }

    #[test]
    fn deterministe() {
        let f = formeur_par_id("wannamaker9f").unwrap();
        let mut a = Quantificateur::nouveau(16, true, Some(&f), 2, 7);
        let mut b = Quantificateur::nouveau(16, true, Some(&f), 2, 7);
        for i in 0..1000 {
            let x = (i as f64 * 0.01).sin() * 0.3;
            assert_eq!(a.quantifier(x, i % 2), b.quantifier(x, i % 2));
        }
    }
}
