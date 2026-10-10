//! Crête vraie (*true peak*) selon ITU-R BS.1770-4/5, annexe 2 (#2713).
//!
//! # Ce que mesure ce module
//!
//! Le maximum, en valeur absolue, du signal CONTINU que représentent les
//! échantillons : il dépasse le plus grand échantillon dès que la crête tombe
//! entre deux instants d'échantillonnage. L'annexe 2 de BS.1770 l'estime en
//! suréchantillonnant le signal (insertion de zéros, puis filtre passe-bas
//! d'interpolation) et en prenant le plus grand échantillon suréchantillonné.
//!
//! # Le filtre, et pourquoi pas celui du tableau de l'annexe
//!
//! L'annexe donne UN jeu de coefficients (48 prises, 4 phases) « qui
//! satisferait les exigences » à 48 kHz, et son appendice 1 dit comment
//! choisir le rapport de suréchantillonnage : le pire écart, quand la crête
//! tombe à mi-chemin entre deux points suréchantillonnés, vaut
//! `20·log10(cos(π·f_norm / n))`, soit 0,688 dB à 4× et 0,169 dB à 8× pour
//! `f_norm` = 0,5 (tableau de l'appendice). À 4×, une sinusoïde de 16 kHz à
//! 48 kHz peut donc être sous-estimée de 0,30 dB par la grille seule, quel que
//! soit le filtre ; à 8×, de 0,075 dB.
//!
//! Tune vise 0,1 dB jusqu'à 16 kHz : il suréchantillonne donc **8× sous
//! 88,2 kHz** et **4× au-delà** — toujours au moins le rapport que l'annexe
//! demande (4× à 48 kHz, 2× à 96 kHz). Au-delà de 88,2 kHz, 4× garde le pire
//! écart sous 0,12 dB jusqu'à 20 kHz, et sous 0,15 dB pour un contenu
//! ultrasonore à 0,23·fs (40 kHz à 176,4 kHz, le débit du DSD64 converti).
//!
//! Le tableau de l'annexe n'existe qu'en 4 phases : il ne se décline pas à 8×.
//! Le filtre est donc conçu ici, selon la méthode que l'appendice décrit :
//! sinus cardinal coupé à la fréquence de Nyquist d'origine, fenêtré par une
//! fenêtre de Kaiser (β = 7), [`PRISES`] = 16 prises par phase. Sa coupure à
//! exactement `fs/2` en fait un filtre de Nyquist : la phase 0 rend les
//! échantillons d'origine, qui sont donc pris tels quels et la phase 0 n'est
//! pas calculée. Chaque phase est normalisée à un gain unité en continu.
//!
//! Mesuré par les tests de ce module, le filtre seul s'écarte de moins de
//! 0,02 dB du signal continu jusqu'à 0,42·fs ; le reste de l'écart est celui
//! de la grille, borné par la formule de l'appendice. Le filtre de l'annexe,
//! rejoué sur les mêmes signaux ([`CreteVraie::annexe_2_bs1770`]), dépasse
//! +0,2 dB à fs/4 (ondulation de sa bande passante) et −0,29 dB à fs/3.
//!
//! # Coût, et l'économie qui ne change rien au résultat
//!
//! 7 phases × 16 prises = 112 multiplications par échantillon et par canal à
//! 44,1 kHz (3 × 16 = 48 au-delà de 88,2 kHz). La plupart ne servent à
//! rien : un point interpolé vaut au plus `Σ|h| · max|x|` sur sa fenêtre. Par blocs de [`BLOC`] fenêtres, si cette
//! borne ne dépasse pas la crête déjà trouvée, le bloc est sauté — aucun de
//! ses points ne pouvait la faire monter. Le résultat est donc EXACTEMENT
//! celui du calcul complet, au bit près, et ne dépend pas du découpage des
//! appels (gardé par les tests). Sur de la musique, la crête est atteinte tôt
//! et presque tous les blocs sont sautés.
//!
//! # Bords
//!
//! Comme un indicateur en continu (et comme `libebur128`) : l'histoire du
//! filtre part de zéros — un départ brutal se mesure tel qu'un convertisseur
//! le reconstruirait — et rien n'est ajouté après le dernier échantillon : les
//! 7 derniers intervalles ne sont représentés que par leurs échantillons.

/// Prises par phase du filtre de Tune.
pub const PRISES: usize = 16;

/// Paramètre de la fenêtre de Kaiser du filtre de Tune.
const BETA: f64 = 7.0;

/// Fenêtres examinées d'un bloc avant de décider si on les calcule.
const BLOC: usize = 32;

/// Le rapport de suréchantillonnage pour une fréquence d'échantillonnage :
/// 8× sous 88,2 kHz, 4× au-delà. Voir l'en-tête du module.
pub fn facteur_de_surechantillonnage(sample_rate: usize) -> usize {
    if sample_rate < 88_200 { 8 } else { 4 }
}

/// Fonction de Bessel modifiée de première espèce, d'ordre 0 (série entière).
fn bessel_i0(x: f64) -> f64 {
    let mut somme = 1.0;
    let mut terme = 1.0;
    let mut k = 1.0;
    loop {
        terme *= (x / (2.0 * k)) * (x / (2.0 * k));
        somme += terme;
        if terme < 1e-17 * somme {
            return somme;
        }
        k += 1.0;
    }
}

/// Les phases 1 à `facteur − 1` du filtre de Tune, à la suite, [`PRISES`]
/// coefficients chacune, appliqués à une fenêtre rangée du plus ancien au plus
/// récent échantillon.
///
/// La phase `p` interpole le point situé à `p / facteur` d'intervalle après
/// l'échantillon `PRISES/2 − 1` de la fenêtre.
fn phases_kaiser(facteur: usize) -> Vec<f64> {
    let demi = (PRISES / 2) as f64;
    let i0_beta = bessel_i0(BETA);
    let mut tout = Vec::with_capacity((facteur - 1) * PRISES);
    for p in 1..facteur {
        let mut phase = [0.0f64; PRISES];
        for (j, c) in phase.iter_mut().enumerate() {
            let tau = p as f64 / facteur as f64 + (demi - 1.0) - j as f64;
            let sinc = if tau == 0.0 {
                1.0
            } else {
                let x = std::f64::consts::PI * tau;
                x.sin() / x
            };
            let r = tau / demi;
            let fenetre = bessel_i0(BETA * (1.0 - r * r).max(0.0).sqrt()) / i0_beta;
            *c = sinc * fenetre;
        }
        let gain: f64 = phase.iter().sum();
        tout.extend(phase.iter().map(|c| c / gain));
    }
    tout
}

/// Les coefficients de l'annexe 2 de BS.1770-4 (« order 48, 4-phase, FIR
/// interpolating »), recopiés du tableau, phase par phase.
const ANNEXE_2: [[f64; 12]; 4] = [
    [
        0.0017089843750,
        0.0109863281250,
        -0.0196533203125,
        0.0332031250000,
        -0.0594482421875,
        0.1373291015625,
        0.9721679687500,
        -0.1022949218750,
        0.0476074218750,
        -0.0266113281250,
        0.0148925781250,
        -0.0083007812500,
    ],
    [
        -0.0291748046875,
        0.0292968750000,
        -0.0517578125000,
        0.0891113281250,
        -0.1665039062500,
        0.4650878906250,
        0.7797851562500,
        -0.2003173828125,
        0.1015625000000,
        -0.0582275390625,
        0.0330810546875,
        -0.0189208984375,
    ],
    [
        -0.0189208984375,
        0.0330810546875,
        -0.0582275390625,
        0.1015625000000,
        -0.2003173828125,
        0.7797851562500,
        0.4650878906250,
        -0.1665039062500,
        0.0891113281250,
        -0.0517578125000,
        0.0292968750000,
        -0.0291748046875,
    ],
    [
        -0.0083007812500,
        0.0148925781250,
        -0.0266113281250,
        0.0476074218750,
        -0.1022949218750,
        0.9721679687500,
        0.1373291015625,
        -0.0594482421875,
        0.0332031250000,
        -0.0196533203125,
        0.0109863281250,
        0.0017089843750,
    ],
];

/// Accumulateur de crête vraie, en continu, sur des échantillons entrelacés
/// normalisés (`[-1, 1]`, mais rien n'est borné : un over reste un over).
///
/// Le résultat englobe le pic d'échantillon : chaque échantillon d'origine y
/// participe. Il ne dépend pas du découpage des appels à [`Self::nourrir`].
#[derive(Debug, Clone)]
pub struct CreteVraie {
    /// Les phases calculées, à la suite, `prises` coefficients chacune.
    phases: Vec<f64>,
    prises: usize,
    /// `max_p Σ|h_p|`, majoré d'une marge d'arrondi : la borne d'un point
    /// interpolé est `borne · max|x|` sur sa fenêtre.
    borne: f64,
    /// Par canal : les `prises − 1` derniers échantillons, puis ceux de
    /// l'appel en cours.
    canaux: Vec<Vec<f64>>,
    crete: f64,
    /// Sauter les blocs qui ne peuvent rien changer. Toujours vrai hors du
    /// témoin [`crete_vraie_sans_economie`].
    economie: bool,
}

impl CreteVraie {
    /// Le filtre de Tune, au rapport que demande `sample_rate`.
    pub fn new(sample_rate: usize, channels: usize) -> Self {
        Self::avec_phases(
            phases_kaiser(facteur_de_surechantillonnage(sample_rate)),
            PRISES,
            channels,
        )
    }

    /// Le filtre du tableau de l'annexe 2 de BS.1770-4, tel quel (4×, toutes
    /// fréquences). Référence des tests et du banc : la mesure de Tune n'en
    /// dépend pas.
    pub fn annexe_2_bs1770(channels: usize) -> Self {
        Self::avec_phases(ANNEXE_2.iter().flatten().copied().collect(), 12, channels)
    }

    fn avec_phases(phases: Vec<f64>, prises: usize, channels: usize) -> Self {
        let borne = phases
            .chunks_exact(prises)
            .map(|p| p.iter().map(|c| c.abs()).sum::<f64>())
            .fold(0.0f64, f64::max)
            * (1.0 + 1e-9);
        Self {
            phases,
            prises,
            borne,
            canaux: vec![vec![0.0; prises - 1]; channels],
            crete: 0.0,
            economie: true,
        }
    }

    /// Nourrir des échantillons entrelacés (`channels` canaux, trames
    /// complètes).
    pub fn nourrir(&mut self, entrelaces: &[f64]) {
        let n = self.canaux.len();
        if n == 0 {
            return;
        }
        let trames = entrelaces.len() / n;
        for c in 0..n {
            let buf = &mut self.canaux[c];
            buf.reserve(trames);
            for f in 0..trames {
                let x = entrelaces[f * n + c];
                // Le pic d'échantillon fait partie de la crête vraie.
                self.crete = self.crete.max(x.abs());
                buf.push(x);
            }
            let borne = if self.economie {
                self.borne
            } else {
                f64::INFINITY
            };
            interpoler(&self.phases, self.prises, borne, buf, &mut self.crete);
            let garder = self.prises - 1;
            let longueur = buf.len();
            buf.drain(..longueur - garder);
        }
    }

    /// La crête vraie linéaire vue jusqu'ici (1,0 = pleine échelle).
    pub fn crete(&self) -> f64 {
        self.crete
    }
}

/// Toutes les fenêtres complètes de `buf`, par blocs ; un bloc dont la borne ne
/// peut pas dépasser `crete` est sauté. `borne` infinie : rien n'est sauté
/// (sauf un bloc entièrement nul, dont tous les points valent zéro).
fn interpoler(phases: &[f64], prises: usize, borne: f64, buf: &[f64], crete: &mut f64) {
    let fenetres = (buf.len() + 1).saturating_sub(prises);
    let mut debut = 0;
    while debut < fenetres {
        let fin = (debut + BLOC).min(fenetres);
        let etendue = &buf[debut..fin + prises - 1];
        let max_abs = etendue.iter().fold(0.0f64, |m, x| m.max(x.abs()));
        if max_abs * borne > *crete {
            for k in debut..fin {
                let fenetre = &buf[k..k + prises];
                for phase in phases.chunks_exact(prises) {
                    let mut s = 0.0;
                    for (c, x) in phase.iter().zip(fenetre) {
                        s += c * x;
                    }
                    let a = s.abs();
                    if a > *crete {
                        *crete = a;
                    }
                }
            }
        }
        debut = fin;
    }
}

/// L'ancienne estimation (#1694, avant #2713) : interpolation Catmull-Rom 4×
/// entre les deux derniers échantillons. Gardée pour le banc de coût et la
/// comparaison des tests, plus pour la mesure.
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct CreteCatmullRom {
    hist: Vec<[f64; 3]>,
    crete: f64,
}

impl CreteCatmullRom {
    pub fn new(channels: usize) -> Self {
        Self {
            hist: vec![[0.0; 3]; channels],
            crete: 0.0,
        }
    }

    pub fn nourrir(&mut self, entrelaces: &[f64]) {
        let n = self.hist.len();
        if n == 0 {
            return;
        }
        for (i, &raw) in entrelaces.iter().enumerate() {
            let c = i % n;
            let [p0, p1, p2] = self.hist[c];
            let p3 = raw;
            let a = -p0 + 3.0 * p1 - 3.0 * p2 + p3;
            let b = 2.0 * p0 - 5.0 * p1 + 4.0 * p2 - p3;
            let cc = p2 - p0;
            let d = 2.0 * p1;
            for t in [0.25f64, 0.5, 0.75] {
                let v = 0.5 * (((a * t + b) * t + cc) * t + d);
                self.crete = self.crete.max(v.abs());
            }
            self.crete = self.crete.max(raw.abs());
            self.hist[c] = [p1, p2, p3];
        }
    }

    pub fn crete(&self) -> f64 {
        self.crete
    }
}

#[cfg(test)]
#[path = "crete_vraie_tests.rs"]
mod tests;

#[doc(hidden)]
/// Le calcul complet, sans l'économie des blocs : référence de l'exactitude
/// de [`CreteVraie`] dans les tests, et pire cas du banc de coût.
pub fn crete_vraie_sans_economie(sample_rate: usize, channels: usize, entrelaces: &[f64]) -> f64 {
    let mut m = CreteVraie::new(sample_rate, channels);
    m.economie = false;
    m.nourrir(entrelaces);
    m.crete
}
