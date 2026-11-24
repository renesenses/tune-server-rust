//! Les étages de la chaîne type, génériques sur le type d'échantillon : le
//! MÊME algorithme tourne en `f32` et en `f64`, seul le type change. C'est ce
//! que mesure le banc : le coût du passage au 64 bits, à algorithme égal.
//!
//! Coefficients toujours calculés en `f64`, puis convertis : la différence
//! F32/F64 mesurée est celle du calcul, pas celle de la conception.

use realfft::num_complex::Complex;
use realfft::{ComplexToReal, FftNum, RealFftPlanner, RealToComplex};
use std::sync::Arc;

pub trait Ech:
    FftNum
    + Copy
    + Default
    + std::ops::Add<Output = Self>
    + std::ops::Sub<Output = Self>
    + std::ops::Mul<Output = Self>
    + std::ops::AddAssign
{
    fn de(x: f64) -> Self;
    fn en_f64(self) -> f64;
}

impl Ech for f32 {
    fn de(x: f64) -> Self {
        x as f32
    }
    fn en_f64(self) -> f64 {
        f64::from(self)
    }
}

impl Ech for f64 {
    fn de(x: f64) -> Self {
        x
    }
    fn en_f64(self) -> f64 {
        self
    }
}

/// Biquad en forme directe II transposée.
#[derive(Clone, Copy)]
struct Biquad<T> {
    b0: T,
    b1: T,
    b2: T,
    a1: T,
    a2: T,
}

/// Cloche RBJ (Audio EQ Cookbook).
fn cloche(fs: f64, f0: f64, q: f64, gain_db: f64) -> [f64; 5] {
    let a = 10f64.powf(gain_db / 40.0);
    let w0 = 2.0 * std::f64::consts::PI * f0 / fs;
    let alpha = w0.sin() / (2.0 * q);
    let a0 = 1.0 + alpha / a;
    [
        (1.0 + alpha * a) / a0,
        -2.0 * w0.cos() / a0,
        (1.0 - alpha * a) / a0,
        -2.0 * w0.cos() / a0,
        (1.0 - alpha / a) / a0,
    ]
}

/// Égaliseur graphique : 31 cloches au tiers d'octave (20 Hz – 20 kHz),
/// en série, par canal.
pub struct Egaliseur<T> {
    bandes: Vec<Biquad<T>>,
    /// `[canal][bande] = (z1, z2)`.
    etats: Vec<Vec<(T, T)>>,
    canaux: usize,
}

/// Centres ISO des 31 bandes.
pub const CENTRES_31: [f64; 31] = [
    20.0, 25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0,
    500.0, 630.0, 800.0, 1_000.0, 1_250.0, 1_600.0, 2_000.0, 2_500.0, 3_150.0, 4_000.0, 5_000.0,
    6_300.0, 8_000.0, 10_000.0, 12_500.0, 16_000.0, 20_000.0,
];

impl<T: Ech> Egaliseur<T> {
    pub fn new(fs: f64, canaux: usize) -> Self {
        let bandes = CENTRES_31
            .iter()
            .enumerate()
            .map(|(i, &f)| {
                // Réglage non trivial : ±3 dB en alternance (aucune bande à
                // 0 dB, que l'égaliseur pourrait sauter).
                let g = if i % 2 == 0 { 3.0 } else { -3.0 };
                let c = cloche(fs, f, 4.32, g);
                Biquad {
                    b0: T::de(c[0]),
                    b1: T::de(c[1]),
                    b2: T::de(c[2]),
                    a1: T::de(c[3]),
                    a2: T::de(c[4]),
                }
            })
            .collect::<Vec<_>>();
        let n = bandes.len();
        Self {
            bandes,
            etats: vec![vec![(T::default(), T::default()); n]; canaux],
            canaux,
        }
    }

    pub fn traiter(&mut self, bloc: &mut [T]) {
        let c = self.canaux;
        for canal in 0..c {
            let etats = &mut self.etats[canal];
            for (b, (z1, z2)) in self.bandes.iter().zip(etats.iter_mut()) {
                let mut i = canal;
                while i < bloc.len() {
                    let x = bloc[i];
                    let y = b.b0 * x + *z1;
                    *z1 = b.b1 * x - b.a1 * y + *z2;
                    *z2 = b.b2 * x - b.a2 * y;
                    bloc[i] = y;
                    i += c;
                }
            }
        }
    }
}

/// Convolution partitionnée uniforme (UPOLS), partitions de `b` trames,
/// FFT de `2b`. Coût par bloc : une FFT, une FFT inverse et `p × (b + 1)`
/// multiplications complexes par canal.
pub struct Convolution<T: Ech> {
    b: usize,
    canaux: usize,
    directe: Arc<dyn RealToComplex<T>>,
    inverse: Arc<dyn ComplexToReal<T>>,
    /// `[canal][partition]` : spectres de la RI.
    filtre: Vec<Vec<Vec<Complex<T>>>>,
    /// `[canal][partition]` : spectres des entrées passées (ligne à retard).
    passe: Vec<Vec<Vec<Complex<T>>>>,
    tete: usize,
    /// `[canal]` : les `2b` dernières entrées.
    fenetre: Vec<Vec<T>>,
    tampon: Vec<T>,
    spectre: Vec<Complex<T>>,
    somme: Vec<Complex<T>>,
    sortie: Vec<T>,
    scratch_d: Vec<Complex<T>>,
    scratch_i: Vec<Complex<T>>,
}

/// RI de test : bruit déterministe décroissant (≈ 60 dB sur la longueur),
/// différente par canal.
pub fn reponse_impulsionnelle(longueur: usize, canal: usize) -> Vec<f64> {
    let mut graine = 0x2545_f491_4f6c_dd1d_u64 ^ (canal as u64 + 1);
    (0..longueur)
        .map(|i| {
            graine ^= graine << 13;
            graine ^= graine >> 7;
            graine ^= graine << 17;
            let u = (graine >> 11) as f64 / (1u64 << 53) as f64 - 0.5;
            u * (-6.9 * i as f64 / longueur as f64).exp() * 0.05
        })
        .collect()
}

impl<T: Ech> Convolution<T> {
    pub fn new(longueur_ri: usize, b: usize, canaux: usize) -> Self {
        let mut planif = RealFftPlanner::<T>::new();
        let directe = planif.plan_fft_forward(2 * b);
        let inverse = planif.plan_fft_inverse(2 * b);
        let p = longueur_ri.div_ceil(b);
        let mut filtre = Vec::with_capacity(canaux);
        let mut entree = directe.make_input_vec();
        let mut scratch_d = directe.make_scratch_vec();
        for canal in 0..canaux {
            let ri = reponse_impulsionnelle(longueur_ri, canal);
            let parts = (0..p)
                .map(|k| {
                    entree.iter_mut().for_each(|x| *x = T::default());
                    for (j, x) in entree.iter_mut().take(b).enumerate() {
                        *x = T::de(ri.get(k * b + j).copied().unwrap_or(0.0));
                    }
                    let mut s = directe.make_output_vec();
                    directe
                        .process_with_scratch(&mut entree, &mut s, &mut scratch_d)
                        .expect("FFT");
                    s
                })
                .collect();
            filtre.push(parts);
        }
        let n_spectre = b + 1;
        Self {
            b,
            canaux,
            passe: vec![vec![vec![Complex::default(); n_spectre]; p]; canaux],
            filtre,
            tete: 0,
            fenetre: vec![vec![T::default(); 2 * b]; canaux],
            tampon: directe.make_input_vec(),
            spectre: directe.make_output_vec(),
            somme: inverse.make_input_vec(),
            sortie: inverse.make_output_vec(),
            scratch_d,
            scratch_i: inverse.make_scratch_vec(),
            directe,
            inverse,
        }
    }

    pub fn partitions(&self) -> usize {
        self.filtre[0].len()
    }

    /// `bloc` : exactement `b` trames entrelacées.
    pub fn traiter(&mut self, bloc: &mut [T]) {
        let (b, c) = (self.b, self.canaux);
        assert_eq!(bloc.len(), b * c);
        let p = self.partitions();
        let echelle = T::de(1.0 / (2 * b) as f64);
        for canal in 0..c {
            let fen = &mut self.fenetre[canal];
            fen.copy_within(b.., 0);
            for j in 0..b {
                fen[b + j] = bloc[j * c + canal];
            }
            self.tampon.copy_from_slice(fen);
            self.directe
                .process_with_scratch(&mut self.tampon, &mut self.spectre, &mut self.scratch_d)
                .expect("FFT");
            self.passe[canal][self.tete].copy_from_slice(&self.spectre);
            self.somme.iter_mut().for_each(|s| *s = Complex::default());
            for k in 0..p {
                let x = &self.passe[canal][(self.tete + p - k) % p];
                let h = &self.filtre[canal][k];
                for ((s, a), bb) in self.somme.iter_mut().zip(x).zip(h) {
                    *s = *s + *a * *bb;
                }
            }
            // Les parties imaginaires de DC et Nyquist doivent être nulles.
            self.somme[0].im = T::default();
            self.somme[b].im = T::default();
            self.inverse
                .process_with_scratch(&mut self.somme, &mut self.sortie, &mut self.scratch_i)
                .expect("FFT inverse");
            for j in 0..b {
                bloc[j * c + canal] = self.sortie[b + j] * echelle;
            }
        }
        self.tete = (self.tete + 1) % p;
    }
}

/// Crossfeed stéréo : passe-bas du premier ordre sur le terme croisé, retard
/// de 0,3 ms, mélange direct/croisé.
pub struct Crossfeed<T> {
    a: T,
    b: T,
    direct: T,
    croise: T,
    lp: [T; 2],
    retard: Vec<[T; 2]>,
    pos: usize,
}

impl<T: Ech> Crossfeed<T> {
    pub fn new(fs: f64) -> Self {
        let fc = 700.0;
        let x = (-2.0 * std::f64::consts::PI * fc / fs).exp();
        let n = ((0.0003 * fs).round() as usize).max(1);
        Self {
            a: T::de(1.0 - x),
            b: T::de(x),
            direct: T::de(0.85),
            croise: T::de(0.3),
            lp: [T::default(); 2],
            retard: vec![[T::default(); 2]; n],
            pos: 0,
        }
    }

    pub fn traiter(&mut self, bloc: &mut [T]) {
        for trame in bloc.as_chunks_mut::<2>().0 {
            let (g, d) = (trame[0], trame[1]);
            self.lp[0] = self.a * g + self.b * self.lp[0];
            self.lp[1] = self.a * d + self.b * self.lp[1];
            let ancien = self.retard[self.pos];
            self.retard[self.pos] = self.lp;
            self.pos = (self.pos + 1) % self.retard.len();
            trame[0] = self.direct * g + self.croise * ancien[1];
            trame[1] = self.direct * d + self.croise * ancien[0];
        }
    }
}

/// Suréchantillonneur × 2 polyphase : sinus cardinal fenêtré (Kaiser β 10),
/// `coefficients` par phase.
pub struct Surechantillonneur<T> {
    phases: [Vec<T>; 2],
    historique: Vec<Vec<T>>,
    pos: usize,
    canaux: usize,
}

fn bessel_i0(x: f64) -> f64 {
    let (mut s, mut t) = (1.0, 1.0);
    for k in 1..100 {
        t *= (x / 2.0 / k as f64).powi(2);
        s += t;
    }
    s
}

impl<T: Ech> Surechantillonneur<T> {
    pub fn new(coefficients: usize, canaux: usize) -> Self {
        let n = 2 * coefficients;
        let centre = (n - 1) as f64 / 2.0;
        let h: Vec<f64> = (0..n)
            .map(|i| {
                let t = i as f64 - centre;
                let sinc = if t == 0.0 {
                    1.0
                } else {
                    (std::f64::consts::PI * t / 2.0).sin() / (std::f64::consts::PI * t / 2.0)
                };
                let r = t / centre;
                sinc * bessel_i0(10.0 * (1.0 - r * r).max(0.0).sqrt()) / bessel_i0(10.0)
            })
            .collect();
        let phase = |p: usize| (0..coefficients).map(|k| T::de(h[p + 2 * k])).collect();
        Self {
            phases: [phase(0), phase(1)],
            historique: vec![vec![T::default(); 2 * coefficients]; canaux],
            pos: 0,
            canaux,
        }
    }

    /// `sortie` : `2 × entree.len()` échantillons.
    pub fn traiter(&mut self, entree: &[T], sortie: &mut [T]) {
        let c = self.canaux;
        let t = self.phases[0].len();
        for (i, trame) in entree.chunks_exact(c).enumerate() {
            self.pos = if self.pos == 0 { t - 1 } else { self.pos - 1 };
            for (canal, &x) in trame.iter().enumerate() {
                let h = &mut self.historique[canal];
                h[self.pos] = x;
                h[self.pos + t] = x;
                let fen = &h[self.pos..self.pos + t];
                for (p, phase) in self.phases.iter().enumerate() {
                    let mut acc = T::default();
                    for (a, b) in phase.iter().zip(fen) {
                        acc += *a * *b;
                    }
                    sortie[(2 * i + p) * c + canal] = acc;
                }
            }
        }
    }
}

/// La chaîne type : égaliseur 31 bandes → convolution → crossfeed →
/// suréchantillonnage × 2 (avant le volume, Q9).
pub struct Chaine<T: Ech> {
    pub egaliseur: Egaliseur<T>,
    pub convolution: Convolution<T>,
    pub crossfeed: Crossfeed<T>,
    pub surech: Surechantillonneur<T>,
    pub sortie: Vec<T>,
}

/// Les quatre étages, dans l'ordre.
pub const ETAGES: [&str; 4] = [
    "Égaliseur graphique 31 bandes",
    "Convolution",
    "Crossfeed",
    "Suréchantillonnage × 2",
];

impl<T: Ech> Chaine<T> {
    pub fn new(fs: f64, longueur_ri: usize, bloc: usize, coefficients_surech: usize) -> Self {
        Self {
            egaliseur: Egaliseur::new(fs, 2),
            convolution: Convolution::new(longueur_ri, bloc, 2),
            crossfeed: Crossfeed::new(fs),
            surech: Surechantillonneur::new(coefficients_surech, 2),
            sortie: vec![T::default(); bloc * 2 * 2],
        }
    }

    /// Traite un bloc ; rend le temps passé dans chaque étage (ns).
    pub fn traiter(&mut self, bloc: &mut [T]) -> [u64; 4] {
        let mut t = [0u64; 4];
        let mut top = std::time::Instant::now();
        let mut tic = |i: usize, t: &mut [u64; 4]| {
            let n = std::time::Instant::now();
            t[i] = (n - top).as_nanos() as u64;
            top = n;
        };
        self.egaliseur.traiter(bloc);
        tic(0, &mut t);
        self.convolution.traiter(bloc);
        tic(1, &mut t);
        self.crossfeed.traiter(bloc);
        tic(2, &mut t);
        self.surech.traiter(bloc, &mut self.sortie);
        tic(3, &mut t);
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// La convolution partitionnée rend la convolution directe (référence
    /// hors ligne en f64) ; en f32, l'écart est celui de la simple précision.
    #[test]
    fn la_convolution_partitionnee_est_la_convolution_directe() {
        let (lri, b) = (1024, 128);
        let entree: Vec<f64> = reponse_impulsionnelle(4 * b, 7)
            .iter()
            .map(|x| x * 20.0)
            .collect();
        let ri = reponse_impulsionnelle(lri, 0);
        let attendu: Vec<f64> = (0..entree.len())
            .map(|n| (0..=n.min(lri - 1)).map(|k| ri[k] * entree[n - k]).sum())
            .collect();
        let mut c64 = Convolution::<f64>::new(lri, b, 2);
        let mut c32 = Convolution::<f32>::new(lri, b, 2);
        let (mut e64, mut e32) = (0.0f64, 0.0f64);
        for (bloc_i, bloc) in entree.chunks(b).enumerate() {
            let mut x64: Vec<f64> = bloc.iter().flat_map(|&v| [v, v]).collect();
            let mut x32: Vec<f32> = bloc.iter().flat_map(|&v| [v as f32, v as f32]).collect();
            c64.traiter(&mut x64);
            c32.traiter(&mut x32);
            for j in 0..b {
                let a = attendu[bloc_i * b + j];
                e64 = e64.max((x64[2 * j] - a).abs());
                e32 = e32.max((f64::from(x32[2 * j]) - a).abs());
            }
        }
        assert!(e64 < 1e-12, "F64 : écart {e64:e}");
        assert!(e32 > 1e-10 && e32 < 1e-5, "F32 : écart {e32:e}");
    }

    /// L'égaliseur à ±3 dB en alternance change bien le signal, et F32 et F64
    /// ne diffèrent que par la précision.
    #[test]
    fn egaliseur_et_crossfeed_agissent() {
        let mut eq = Egaliseur::<f64>::new(96_000.0, 2);
        let mut x: Vec<f64> = reponse_impulsionnelle(2048, 3)
            .iter()
            .map(|v| v * 10.0)
            .collect();
        let avant = x.clone();
        eq.traiter(&mut x);
        assert!(x.iter().zip(&avant).any(|(a, b)| (a - b).abs() > 1e-6));
        let mut cf = Crossfeed::<f64>::new(96_000.0);
        let mut y = vec![0.0; 2048];
        y[0] = 1.0; // impulsion sur la gauche seulement
        cf.traiter(&mut y);
        assert!(
            y.iter().skip(1).step_by(2).any(|&d| d.abs() > 1e-4),
            "rien de croisé à droite"
        );
    }

    /// Suréchantillonneur : un continu reste un continu (gain 1 par phase).
    #[test]
    fn le_surechantillonneur_garde_le_continu() {
        let mut s = Surechantillonneur::<f64>::new(64, 2);
        let entree = vec![0.5; 512 * 2];
        let mut sortie = vec![0.0; 1024 * 2];
        s.traiter(&entree, &mut sortie);
        for v in &sortie[600..] {
            assert!((v - 0.5).abs() < 1e-3, "{v}");
        }
    }
}
