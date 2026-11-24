//! Outils de mesure du flux DSD, partagés par les témoins et le banc
//! (`examples/banc_modulateur_dsd.rs`). Pas d'usage en production.
//!
//! - Spectre par FFT radix 2 (`f64`), fenêtre de Blackman-Harris à 7 termes
//!   (lobes secondaires sous −180 dB) : le bruit hors bande d'un flux à 1 bit
//!   est énorme (≈ −30 dB) et la raie du signal est à −6 dB ; ni l'un ni
//!   l'autre ne doit fuir dans la mesure du bruit en bande par la fenêtre.
//!   (Une fenêtre de Nuttall, −93 dB, plafonnait la mesure vers −115 dB sous
//!   la raie : c'était la fenêtre, pas le modulateur.)
//! - Toutes les puissances sont en dB relatifs à la puissance d'un sinus
//!   d'amplitude 1,0 (100 % de modulation) : le « 0 dB SACD » (50 %) y vaut
//!   −6,02 dB.

/// FFT complexe radix 2, sur place (`re.len()` puissance de deux).
pub fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    assert!(n.is_power_of_two() && im.len() == n);
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut longueur = 2;
    while longueur <= n {
        let angle = -2.0 * std::f64::consts::PI / longueur as f64;
        let moitie = longueur / 2;
        let (wr, wi) = (angle.cos(), angle.sin());
        for debut in (0..n).step_by(longueur) {
            let (mut cr, mut ci) = (1.0_f64, 0.0_f64);
            for k in 0..moitie {
                let (a, b) = (debut + k, debut + k + moitie);
                let tr = re[b] * cr - im[b] * ci;
                let ti = re[b] * ci + im[b] * cr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
        }
        longueur <<= 1;
    }
}

/// Les bits du canal `canal` d'un flux DSD emballé (MSB = plus ancien,
/// octets entrelacés), en ±1,0.
pub fn bits_du_canal(octets: &[u8], canaux: usize, canal: usize) -> Vec<f64> {
    octets
        .iter()
        .skip(canal)
        .step_by(canaux)
        .flat_map(|&o| {
            (0..8)
                .rev()
                .map(move |b| if (o >> b) & 1 == 1 { 1.0 } else { -1.0 })
        })
        .collect()
}

/// Spectre de puissance unilatéral normalisé : la somme des cases d'une raie
/// sinusoïdale d'amplitude A vaut A² / 2 ; la somme des cases d'un bruit vaut
/// sa puissance moyenne dans la bande.
pub fn spectre(x: &[f64]) -> Vec<f64> {
    assert!(x.len() >= 2);
    // La plus grande puissance de deux qui tient dans `x`.
    let n = 1usize << (usize::BITS - 1 - x.len().leading_zeros());
    const A: [f64; 7] = [
        0.271_051_400_693_42,
        0.433_297_939_234_48,
        0.218_122_999_543_11,
        0.065_925_446_388_03,
        0.010_811_742_098_37,
        0.000_776_584_825_22,
        0.000_013_887_217_35,
    ];
    let mut re = vec![0.0; n];
    let mut im = vec![0.0; n];
    let mut somme_w2 = 0.0;
    for (i, r) in re.iter_mut().enumerate() {
        let p = 2.0 * std::f64::consts::PI * i as f64 / n as f64;
        let w: f64 = A
            .iter()
            .enumerate()
            .map(|(k, &a)| (if k % 2 == 0 { a } else { -a }) * (k as f64 * p).cos())
            .sum();
        somme_w2 += w * w;
        *r = x[i] * w;
    }
    fft(&mut re, &mut im);
    let norme = 2.0 / (n as f64 * somme_w2);
    (0..=n / 2)
        .map(|k| (re[k] * re[k] + im[k] * im[k]) * norme)
        .collect()
}

pub fn db(puissance: f64) -> f64 {
    10.0 * (puissance.max(1e-300) / 0.5).log10()
}

/// Résultat d'une analyse.
#[derive(Debug, Clone, Copy)]
pub struct Analyse {
    /// Raie du signal (somme des cases à ±[`EXCLUSION_HZ`]), dB.
    pub signal_db: f64,
    /// Bruit et distorsion 20 Hz – 20 kHz, signal exclu, dB.
    pub bruit_bande_db: f64,
    /// Bruit 20 kHz – 100 kHz, dB.
    pub bruit_hors_bande_db: f64,
    /// Plus haute case isolée dans 20 Hz – 20 kHz, signal exclu, dB.
    pub raie_max_db: f64,
    /// Rapport signal / (bruit + distorsion) dans la bande, dB.
    pub sinad_db: f64,
}

/// Largeur exclue autour du signal : 200 Hz, et au moins
/// [`EXCLUSION_CASES`] cases (lobe principal de la fenêtre, ±7 cases).
pub const EXCLUSION_HZ: f64 = 200.0;
pub const EXCLUSION_CASES: f64 = 12.0;

/// Analyse `x` échantillonné à `fs`, signal attendu à `f0` (`None` : silence).
pub fn analyser(x: &[f64], fs: f64, f0: Option<f64>) -> Analyse {
    let p = spectre(x);
    let n = (p.len() - 1) * 2;
    let case = fs / n as f64;
    let exclusion = EXCLUSION_HZ.max(EXCLUSION_CASES * case);
    let mut signal = 0.0;
    let mut bande = 0.0;
    let mut hors = 0.0;
    let mut raie: f64 = 0.0;
    for (k, &pk) in p.iter().enumerate() {
        let f = k as f64 * case;
        let pres_du_signal = f0.is_some_and(|f0| (f - f0).abs() <= exclusion);
        if pres_du_signal {
            signal += pk;
        } else if (20.0..20_000.0).contains(&f) {
            bande += pk;
            raie = raie.max(pk);
        } else if (20_000.0..100_000.0).contains(&f) {
            hors += pk;
        }
    }
    Analyse {
        signal_db: db(signal),
        bruit_bande_db: db(bande),
        bruit_hors_bande_db: db(hors),
        raie_max_db: db(raie),
        sinad_db: db(signal) - db(bande),
    }
}

/// Un sinus PCM entrelacé (même signal sur tous les canaux).
pub fn sinus(fs: u32, f0: f64, amplitude: f64, trames: usize, canaux: usize) -> Vec<f64> {
    (0..trames)
        .flat_map(|i| {
            let v = amplitude * (2.0 * std::f64::consts::PI * f0 * i as f64 / f64::from(fs)).sin();
            std::iter::repeat_n(v, canaux)
        })
        .collect()
}

/// Modulateur de référence de manuel : ordre 2, NTF = (1 − z⁻¹)², sans
/// dither. Sert d'étalon bas au banc : le modulateur de Tune doit le battre
/// largement, sinon le banc ne discrimine rien.
pub fn reference_ordre_2(x: &[f64]) -> Vec<f64> {
    // Rétroaction de l'erreur : v = u − 2·e[n−1] + e[n−2], y = signe(v).
    let (mut e1, mut e2) = (0.0_f64, 0.0_f64);
    x.iter()
        .map(|&u| {
            let v = u - 2.0 * e1 + e2;
            let y = if v >= 0.0 { 1.0 } else { -1.0 };
            e2 = e1;
            e1 = y - v;
            y
        })
        .collect()
}
