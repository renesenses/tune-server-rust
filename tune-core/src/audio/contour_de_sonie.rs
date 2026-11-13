//! #8554 — contour de loudness qui SUIT LE VOLUME (ISO 226:2003).
//!
//! Le préréglage « Loudness » de l'égaliseur est une courbe FIXE (environ
//! +6 dB aux extrémités) : il ne sait rien du niveau d'écoute. Le contour de
//! ce module compense les courbes d'égale sonie au niveau réellement joué :
//! nul au niveau de référence, il relève le grave (et un peu l'aigu) à mesure
//! que le volume baisse, pour que l'équilibre tonal entendu à bas niveau
//! reste celui du niveau de référence (demande d'Audio Science Review,
//! discussion #8554).
//!
//! ## Le calcul
//!
//! À la référence, la musique est entendue à `reference_phon` phones (le
//! niveau calibré : 80 phones par défaut, le niveau d'un mixage). Baisser le
//! volume de `A` dB la fait entendre, à 1 kHz, à `P = reference_phon − A`
//! phones. Pour qu'une fréquence `f` soit perçue au même niveau de sonie
//! relatif, il lui faut `Lp(f, P)` dB SPL ; elle n'en reçoit que
//! `Lp(f, reference_phon) − A`. Le contour vaut donc
//!
//! `G(f) = Lp(f, P) − Lp(f, reference_phon) + A`, ramené à 0 à 1 kHz,
//!
//! multiplié par l'intensité (`intensity_pct`), puis réduit d'un même
//! facteur si son maximum dépasse [`GAIN_MAXIMAL_DB`] (la forme est gardée). `P` est borné à la plage de validité de la norme
//! (20 à 90 phones) : sous 20 phones, le contour cesse de croître.
//!
//! Les courbes d'égale sonie se resserrent dans le grave (`Lp` y croît de
//! moins de 1 dB par phone) : `0 ≤ G(f) ≤ A`. Sur une sortie dont le volume
//! est numérique et appliqué APRÈS le contour, la crête rendue ne dépasse
//! donc jamais celle du niveau de référence ; un préampli négatif
//! ([`ContourDeSonie::preampli_db`]) reprend le reste quand ce n'est pas le
//! cas (niveau manuel sur une zone en volume fixe).
//!
//! ## Les filtres
//!
//! Quatre biquads (RBJ) : deux étagères basses douces (50 et 250 Hz, Q 0,4),
//! un « peaking » à 3 kHz (Q 0,7) et une étagère haute à 10 kHz. Leurs gains
//! sont AJUSTÉS au contour par moindres carrés sur les 29 fréquences de la
//! norme (écart maximal < 1 dB de 20 Hz à 10 kHz, témoins). « Grave
//! seulement » ne garde que les deux étagères basses.
//!
//! ## Sans clic
//!
//! Un changement de volume ne pose qu'une CIBLE. Les gains appliqués s'en
//! rapprochent à [`VITESSE_DB_PAR_SECONDE`], par pas recalculés tous les
//! [`TRAMES_PAR_PAS`] trames ; les biquads (forme transposée II, état en
//! `f64`) gardent leur historique d'un pas à l'autre.
//!
//! ## Ce que le contour ne fait pas
//!
//! Il ne connaît que le volume que Tune applique. Sur une zone en volume
//! fixe (niveau réglé sur un préampli analogique en aval), Tune ne voit pas le
//! niveau : le contour y reste éteint, sauf si l'auditeur règle un niveau
//! d'écoute à la main (`level_source: "manual"`). Sous PURE ou en bit-perfect
//! strict, il n'est jamais installé (voir l'orchestrateur).

use serde::{Deserialize, Serialize};

/// Les 29 fréquences de la norme ISO 226:2003, en Hz.
pub const FREQUENCES_ISO: [f64; 29] = [
    20.0, 25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0,
    500.0, 630.0, 800.0, 1000.0, 1250.0, 1600.0, 2000.0, 2500.0, 3150.0, 4000.0, 5000.0, 6300.0,
    8000.0, 10000.0, 12500.0,
];

/// Exposant de la perception de sonie `αf` (ISO 226:2003, tableau 1).
const ALPHA_F: [f64; 29] = [
    0.532, 0.506, 0.480, 0.455, 0.432, 0.409, 0.387, 0.367, 0.349, 0.330, 0.315, 0.301, 0.288,
    0.276, 0.267, 0.259, 0.253, 0.250, 0.246, 0.244, 0.243, 0.243, 0.243, 0.242, 0.242, 0.245,
    0.254, 0.271, 0.301,
];

/// Amplitude de la fonction de transfert linéaire `LU`, en dB.
const L_U: [f64; 29] = [
    -31.6, -27.2, -23.0, -19.1, -15.9, -13.0, -10.3, -8.1, -6.2, -4.5, -3.1, -2.0, -1.1, -0.4, 0.0,
    0.3, 0.5, 0.0, -2.7, -4.1, -1.0, 1.7, 2.5, 1.2, -2.1, -7.1, -11.2, -10.7, -3.1,
];

/// Seuil d'audition `Tf`, en dB.
const T_F: [f64; 29] = [
    78.5, 68.7, 59.5, 51.1, 44.0, 37.5, 31.5, 26.5, 22.1, 17.9, 14.4, 11.4, 8.6, 6.2, 4.4, 3.0,
    2.2, 2.4, 3.5, 1.7, -1.3, -4.2, -6.0, -5.4, -1.5, 6.0, 12.6, 13.9, 12.3,
];

/// Indice de 1 kHz dans [`FREQUENCES_ISO`] : le contour y vaut 0 par
/// définition.
const INDICE_1_KHZ: usize = 17;

/// Plage de validité de la norme, en phones.
pub const PHONES_MIN: f64 = 20.0;
pub const PHONES_MAX: f64 = 90.0;

/// Gain maximal du contour, en dB, quels que soient le volume et l'intensité.
pub const GAIN_MAXIMAL_DB: f64 = 18.0;

/// Vitesse à laquelle les gains appliqués rejoignent leur cible.
pub const VITESSE_DB_PAR_SECONDE: f64 = 20.0;

/// Les coefficients sont recalculés au plus tous les `TRAMES_PAR_PAS`.
pub const TRAMES_PAR_PAS: usize = 32;

/// Le seul code de courbe connu aujourd'hui. Le champ `curve` existe pour
/// qu'une autre courbe (Fletcher-Munson historique) s'ajoute sans changer
/// le format du réglage.
pub const COURBE_ISO_226_2003: &str = "iso226_2003";

/// La clé de réglage d'une zone.
pub fn cle(zone_id: i64) -> String {
    format!("zone_{zone_id}_loudness")
}

/// D'où vient le niveau d'écoute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SourceDuNiveau {
    /// Le volume de la zone (défaut).
    #[default]
    Volume,
    /// Un niveau réglé à la main (`manual_level_db`), pour une zone dont
    /// Tune ne connaît pas le niveau (volume fixe, préampli en aval).
    Manual,
}

/// Le réglage du contour d'une zone, rangé en JSON sous [`cle`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReglageContour {
    pub enabled: bool,
    pub curve: String,
    /// Ne corriger que le grave (comme JRiver).
    pub bass_only: bool,
    /// Intensité de la compensation, 0 à 100 %.
    pub intensity_pct: f64,
    /// Volume de la zone (dB, ≤ 0) auquel le contour est nul.
    pub reference_volume_db: f64,
    /// Niveau d'écoute à la référence, en phones (40 à 90).
    pub reference_phon: f64,
    pub level_source: SourceDuNiveau,
    /// Niveau d'écoute par rapport à la référence (dB, ≤ 0) quand
    /// `level_source` vaut `manual`.
    pub manual_level_db: f64,
}

impl Default for ReglageContour {
    fn default() -> Self {
        Self {
            enabled: false,
            curve: COURBE_ISO_226_2003.to_string(),
            bass_only: false,
            intensity_pct: 100.0,
            reference_volume_db: 0.0,
            reference_phon: 80.0,
            level_source: SourceDuNiveau::Volume,
            manual_level_db: -20.0,
        }
    }
}

impl ReglageContour {
    /// Refuse une valeur hors bornes, avec le nom du champ fautif.
    pub fn valider(&self) -> Result<(), String> {
        if self.curve != COURBE_ISO_226_2003 {
            return Err(format!(
                "curve inconnue : {} (connue : {COURBE_ISO_226_2003})",
                self.curve
            ));
        }
        let borne = |nom: &str, v: f64, min: f64, max: f64| {
            if v.is_finite() && (min..=max).contains(&v) {
                Ok(())
            } else {
                Err(format!("{nom} hors bornes : {v} (de {min} à {max})"))
            }
        };
        borne("intensity_pct", self.intensity_pct, 0.0, 100.0)?;
        borne("reference_volume_db", self.reference_volume_db, -60.0, 0.0)?;
        borne("reference_phon", self.reference_phon, 40.0, PHONES_MAX)?;
        borne("manual_level_db", self.manual_level_db, -70.0, 0.0)?;
        Ok(())
    }

    /// Lit le réglage rangé ; défaut (éteint) si absent ou illisible.
    pub fn depuis_json(texte: Option<&str>) -> Self {
        texte
            .and_then(|t| serde_json::from_str::<Self>(t).ok())
            .filter(|r| r.valider().is_ok())
            .unwrap_or_default()
    }

    /// L'atténuation `A` (dB, ≥ 0) du niveau d'écoute sous la référence.
    /// `volume_db` est le volume de la zone ; `None` : niveau inconnu
    /// (sourdine, volume nul) en mode volume.
    pub fn attenuation_db(&self, volume_db: Option<f64>) -> Option<f64> {
        match self.level_source {
            SourceDuNiveau::Manual => Some((-self.manual_level_db).max(0.0)),
            SourceDuNiveau::Volume => {
                let v = volume_db.filter(|v| v.is_finite())?;
                Some((self.reference_volume_db - v).max(0.0))
            }
        }
    }
}

/// Niveau de pression (dB SPL) d'un son pur de la fréquence d'indice `i` de
/// [`FREQUENCES_ISO`], perçu à `phon` phones (ISO 226:2003, formule 1).
pub fn spl_iso226(i: usize, phon: f64) -> f64 {
    let (af, lu, tf) = (ALPHA_F[i], L_U[i], T_F[i]);
    let a = 4.47e-3 * (10f64.powf(0.025 * phon) - 1.15)
        + (0.4 * 10f64.powf((tf + lu) / 10.0 - 9.0)).powf(af);
    10.0 / af * a.log10() - lu + 94.0
}

/// Le contour cible (dB) sur les 29 fréquences de la norme, pour une
/// atténuation `a_db` sous la référence.
pub fn contour_cible(reglage: &ReglageContour, a_db: f64) -> [f64; 29] {
    let reference = reglage.reference_phon.clamp(PHONES_MIN, PHONES_MAX);
    let ecoute = (reference - a_db.max(0.0)).clamp(PHONES_MIN, reference);
    let decalage = reference - ecoute;
    let brut: [f64; 29] =
        std::array::from_fn(|i| spl_iso226(i, ecoute) - spl_iso226(i, reference) + decalage);
    let intensite = (reglage.intensity_pct / 100.0).clamp(0.0, 1.0);
    let releve: [f64; 29] = std::array::from_fn(|i| {
        if reglage.bass_only && i >= INDICE_1_KHZ {
            return 0.0;
        }
        (brut[i] - brut[INDICE_1_KHZ]) * intensite
    });
    // Au-delà de GAIN_MAXIMAL_DB, le contour entier est réduit d'un même
    // facteur : sa FORME est gardée (un écrêtage point par point plierait le
    // grave en un coude que les étagères ne savent pas suivre).
    let crete = releve.iter().fold(0.0_f64, |m, g| m.max(g.abs()));
    let echelle = if crete > GAIN_MAXIMAL_DB {
        GAIN_MAXIMAL_DB / crete
    } else {
        1.0
    };
    releve.map(|g| g * echelle)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Forme {
    EtagereBasse,
    Cloche,
    EtagereHaute,
}

/// Les quatre filtres : forme, fréquence, Q.
const FILTRES: [(Forme, f64, f64); 4] = [
    (Forme::EtagereBasse, 50.0, 0.4),
    (Forme::EtagereBasse, 250.0, 0.4),
    (Forme::Cloche, 3000.0, 0.7),
    (
        Forme::EtagereHaute,
        10000.0,
        std::f64::consts::FRAC_1_SQRT_2,
    ),
];

/// Les filtres « grave seulement » : les deux premiers.
const FILTRES_GRAVES: usize = 2;

/// Coefficients normalisés `[b0, b1, b2, a1, a2]` (RBJ).
fn coefficients(forme: Forme, fc: f64, q: f64, gain_db: f64, sample_rate: f64) -> [f64; 5] {
    let fc = fc.min(sample_rate * 0.45);
    let a = 10f64.powf(gain_db / 40.0);
    let w = std::f64::consts::TAU * fc / sample_rate;
    let (s, c) = w.sin_cos();
    let alpha = s / (2.0 * q);
    let (b0, b1, b2, a0, a1, a2) = match forme {
        Forme::Cloche => (
            1.0 + alpha * a,
            -2.0 * c,
            1.0 - alpha * a,
            1.0 + alpha / a,
            -2.0 * c,
            1.0 - alpha / a,
        ),
        Forme::EtagereBasse => {
            let sa = 2.0 * a.sqrt() * alpha;
            (
                a * ((a + 1.0) - (a - 1.0) * c + sa),
                2.0 * a * ((a - 1.0) - (a + 1.0) * c),
                a * ((a + 1.0) - (a - 1.0) * c - sa),
                (a + 1.0) + (a - 1.0) * c + sa,
                -2.0 * ((a - 1.0) + (a + 1.0) * c),
                (a + 1.0) + (a - 1.0) * c - sa,
            )
        }
        Forme::EtagereHaute => {
            let sa = 2.0 * a.sqrt() * alpha;
            (
                a * ((a + 1.0) + (a - 1.0) * c + sa),
                -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
                a * ((a + 1.0) + (a - 1.0) * c - sa),
                (a + 1.0) - (a - 1.0) * c + sa,
                2.0 * ((a - 1.0) - (a + 1.0) * c),
                (a + 1.0) - (a - 1.0) * c - sa,
            )
        }
    };
    [b0 / a0, b1 / a0, b2 / a0, a1 / a0, a2 / a0]
}

fn module_db(k: &[f64; 5], f: f64, sample_rate: f64) -> f64 {
    let w = std::f64::consts::TAU * f / sample_rate;
    let (s1, c1) = w.sin_cos();
    let (s2, c2) = (2.0 * w).sin_cos();
    // H(e^{jw}) avec z^-1 = cos w − j sin w.
    let num_re = k[0] + k[1] * c1 + k[2] * c2;
    let num_im = -(k[1] * s1 + k[2] * s2);
    let den_re = 1.0 + k[3] * c1 + k[4] * c2;
    let den_im = -(k[3] * s1 + k[4] * s2);
    10.0 * ((num_re * num_re + num_im * num_im) / (den_re * den_re + den_im * den_im)).log10()
}

/// Réponse (dB) à `f` des quatre filtres réglés à `gains`.
pub fn reponse_db(gains: &[f64; 4], f: f64, sample_rate: u32) -> f64 {
    let fs = f64::from(sample_rate);
    FILTRES
        .iter()
        .zip(gains)
        .filter(|(_, g)| **g != 0.0)
        .map(|((forme, fc, q), g)| module_db(&coefficients(*forme, *fc, *q, *g, fs), f, fs))
        .sum()
}

/// Les gains des quatre filtres qui approchent `cible` au mieux (moindres
/// carrés sur les fréquences de la norme sous Nyquist).
pub fn ajuster(cible: &[f64; 29], sample_rate: u32, bass_only: bool) -> [f64; 4] {
    let n = if bass_only { FILTRES_GRAVES } else { 4 };
    if cible.iter().all(|g| g.abs() < 1e-9) {
        return [0.0; 4];
    }
    let fs = f64::from(sample_rate);
    let points: Vec<usize> = (0..29).filter(|&i| FREQUENCES_ISO[i] < fs * 0.45).collect();
    // Jacobienne approchée : la forme de chaque filtre à +6 dB, ramenée au dB.
    let formes: Vec<Vec<f64>> = FILTRES[..n]
        .iter()
        .map(|(forme, fc, q)| {
            let k = coefficients(*forme, *fc, *q, 6.0, fs);
            points
                .iter()
                .map(|&i| module_db(&k, FREQUENCES_ISO[i], fs) / 6.0)
                .collect()
        })
        .collect();
    let mut gains = [0.0; 4];
    for _ in 0..8 {
        let residu: Vec<f64> = points
            .iter()
            .map(|&i| cible[i] - reponse_db(&gains, FREQUENCES_ISO[i], sample_rate))
            .collect();
        // Équations normales (SᵀS + λI) Δ = Sᵀ r.
        let mut m = [[0.0f64; 5]; 4];
        for a in 0..n {
            for b in 0..n {
                m[a][b] = formes[a].iter().zip(&formes[b]).map(|(x, y)| x * y).sum();
            }
            m[a][a] += 1e-3;
            m[a][4] = formes[a].iter().zip(&residu).map(|(x, r)| x * r).sum();
        }
        let Some(delta) = resoudre(&mut m, n) else {
            break;
        };
        for a in 0..n {
            gains[a] = (gains[a] + delta[a]).clamp(-GAIN_MAXIMAL_DB, GAIN_MAXIMAL_DB + 6.0);
        }
    }
    gains
}

/// Élimination de Gauss avec pivot partiel sur la matrice augmentée `n × n+1`
/// (colonne 4 = second membre).
fn resoudre(m: &mut [[f64; 5]; 4], n: usize) -> Option<[f64; 4]> {
    for col in 0..n {
        let pivot = (col..n).max_by(|&a, &b| m[a][col].abs().total_cmp(&m[b][col].abs()))?;
        if m[pivot][col].abs() < 1e-12 {
            return None;
        }
        m.swap(col, pivot);
        for ligne in 0..n {
            if ligne != col {
                let f = m[ligne][col] / m[col][col];
                for k in col..n {
                    m[ligne][k] -= f * m[col][k];
                }
                m[ligne][4] -= f * m[col][4];
            }
        }
    }
    let mut x = [0.0; 4];
    for i in 0..n {
        x[i] = m[i][4] / m[i][i];
    }
    Some(x)
}

/// Les gains de filtres et le préampli d'un réglage à un volume donné.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Consigne {
    pub gains_db: [f64; 4],
    /// Préampli (dB, ≤ 0) qui garde la crête sous la pleine échelle une fois
    /// le volume numérique appliqué.
    pub preampli_db: f64,
}

impl Consigne {
    /// La consigne du réglage pour un volume de zone `volume_db` (le volume
    /// numérique appliqué après le contour sur une sortie locale). `None` :
    /// niveau inconnu, la consigne courante est gardée.
    pub fn calculer(
        reglage: &ReglageContour,
        volume_db: Option<f64>,
        sample_rate: u32,
    ) -> Option<Self> {
        let a = reglage.attenuation_db(volume_db)?;
        let cible = contour_cible(reglage, a);
        let gains_db = ajuster(&cible, sample_rate, reglage.bass_only);
        let crete = FREQUENCES_ISO
            .iter()
            .filter(|f| **f < f64::from(sample_rate) * 0.45)
            .map(|f| reponse_db(&gains_db, *f, sample_rate))
            .fold(0.0_f64, f64::max);
        let volume = volume_db.filter(|v| v.is_finite()).unwrap_or(0.0).min(0.0);
        Some(Self {
            gains_db,
            preampli_db: (-(crete + volume)).min(0.0),
        })
    }

    pub fn est_nulle(&self) -> bool {
        self.gains_db.iter().all(|g| g.abs() < 1e-9) && self.preampli_db.abs() < 1e-9
    }
}

/// L'étage de contour en vol : cible, gains courants lissés, état des filtres.
pub struct ContourDeSonie {
    reglage: ReglageContour,
    sample_rate: u32,
    channels: u16,
    cible: Consigne,
    courant: Consigne,
    coefs: [[f64; 5]; 4],
    /// État TDF-II `[s1, s2]` par canal et par filtre.
    etats: Vec<[[f64; 2]; 4]>,
    /// Coupé : la cible est nulle, l'étage se retire une fois revenu à zéro.
    a_retirer: bool,
    dernier_volume_db: Option<f64>,
}

impl ContourDeSonie {
    pub fn new(
        reglage: ReglageContour,
        sample_rate: u32,
        channels: u16,
        volume_db: Option<f64>,
    ) -> Self {
        let mut c = Self {
            reglage,
            sample_rate: sample_rate.max(1),
            channels: channels.max(1),
            cible: Consigne::default(),
            courant: Consigne::default(),
            coefs: [[1.0, 0.0, 0.0, 0.0, 0.0]; 4],
            etats: vec![[[0.0; 2]; 4]; usize::from(channels.max(1))],
            a_retirer: false,
            dernier_volume_db: None,
        };
        c.suivre_le_volume(volume_db);
        // Au début d'une piste, rien à lisser : la consigne s'applique d'emblée.
        c.courant = c.cible;
        c.recalculer_les_coefficients();
        c
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    pub fn reglage(&self) -> &ReglageContour {
        &self.reglage
    }

    /// Le volume de la zone a changé : nouvelle cible, rejointe en douceur.
    pub fn suivre_le_volume(&mut self, volume_db: Option<f64>) {
        if volume_db.is_some() {
            self.dernier_volume_db = volume_db;
        }
        if self.a_retirer {
            return;
        }
        if let Some(c) = Consigne::calculer(&self.reglage, self.dernier_volume_db, self.sample_rate)
        {
            self.cible = c;
        }
    }

    /// Un nouveau réglage, appliqué en douceur (l'état des filtres reste).
    pub fn regler(&mut self, reglage: ReglageContour) {
        self.reglage = reglage;
        self.a_retirer = false;
        self.suivre_le_volume(self.dernier_volume_db);
    }

    /// Couper : la cible devient nulle ; l'étage se retire revenu à zéro.
    pub fn eteindre(&mut self) {
        self.a_retirer = true;
        self.cible = Consigne::default();
    }

    pub fn est_coupe(&self) -> bool {
        self.a_retirer
    }

    /// Coupé ET revenu à zéro : rien ne reste appliqué.
    pub fn est_au_repos(&self) -> bool {
        self.a_retirer && self.courant.est_nulle()
    }

    /// Le contour ne touche aucun échantillon (gains courants et cible nuls).
    pub fn est_identite(&self) -> bool {
        self.courant.est_nulle() && self.cible.est_nulle()
    }

    pub fn consigne_courante(&self) -> Consigne {
        self.courant
    }

    pub fn consigne_cible(&self) -> Consigne {
        self.cible
    }

    pub fn decrire(&self) -> String {
        format!(
            "contour_de_sonie:{:+.1}/{:+.1}/{:+.1}/{:+.1}dB pre{:+.1}@{}",
            self.courant.gains_db[0],
            self.courant.gains_db[1],
            self.courant.gains_db[2],
            self.courant.gains_db[3],
            self.courant.preampli_db,
            self.sample_rate
        )
    }

    fn recalculer_les_coefficients(&mut self) {
        let fs = f64::from(self.sample_rate);
        for (i, (forme, fc, q)) in FILTRES.iter().enumerate() {
            self.coefs[i] = coefficients(*forme, *fc, *q, self.courant.gains_db[i], fs);
        }
    }

    /// Avance les gains courants d'un pas vers la cible ; vrai s'ils ont bougé.
    fn avancer(&mut self, trames: usize) -> bool {
        let pas = VITESSE_DB_PAR_SECONDE * trames as f64 / f64::from(self.sample_rate);
        let mut bouge = false;
        let rapprocher = |courant: &mut f64, cible: f64, bouge: &mut bool| {
            let ecart = cible - *courant;
            if ecart != 0.0 {
                *courant = if ecart.abs() <= pas {
                    cible
                } else {
                    *courant + pas * ecart.signum()
                };
                *bouge = true;
            }
        };
        for i in 0..4 {
            rapprocher(
                &mut self.courant.gains_db[i],
                self.cible.gains_db[i],
                &mut bouge,
            );
        }
        rapprocher(
            &mut self.courant.preampli_db,
            self.cible.preampli_db,
            &mut bouge,
        );
        if bouge {
            self.recalculer_les_coefficients();
        }
        bouge
    }

    /// Applique le contour EN PLACE à des échantillons entrelacés.
    ///
    /// Gains courants et cible nuls : aucun échantillon n'est touché.
    pub fn process_interleaved(&mut self, samples: &mut [f32]) {
        if self.est_identite() {
            // Identité exacte : l'historique ne sert plus, il repartira de zéro.
            for e in &mut self.etats {
                *e = [[0.0; 2]; 4];
            }
            return;
        }
        let canaux = usize::from(self.channels);
        if self.etats.len() != canaux {
            self.etats = vec![[[0.0; 2]; 4]; canaux];
        }
        for bloc in samples.chunks_mut(TRAMES_PAR_PAS * canaux) {
            self.avancer(bloc.len() / canaux);
            let pre = 10f64.powf(self.courant.preampli_db / 20.0);
            let actifs: Vec<usize> = (0..4)
                .filter(|&i| self.courant.gains_db[i] != 0.0)
                .collect();
            for trame in bloc.chunks_mut(canaux) {
                for (c, s) in trame.iter_mut().enumerate() {
                    let mut x = f64::from(*s);
                    let etat = &mut self.etats[c];
                    for &i in &actifs {
                        let k = &self.coefs[i];
                        let st = &mut etat[i];
                        let y = k[0] * x + st[0];
                        st[0] = k[1] * x - k[3] * y + st[1];
                        st[1] = k[2] * x - k[4] * y;
                        x = y;
                    }
                    *s = (x * pre) as f32;
                }
            }
        }
    }
}

/// Fréquences auxquelles l'API et le chemin du signal montrent le contour.
pub const FREQUENCES_D_APERCU: [f64; 6] = [31.5, 63.0, 125.0, 1000.0, 4000.0, 10000.0];

/// Le volume en dB d'un gain linéaire de zone (0..1) ; `None` au silence.
pub fn volume_db_du_gain(gain: f64) -> Option<f64> {
    (gain.is_finite() && gain > 0.0).then(|| 20.0 * gain.min(1.0).log10())
}

/// L'atténuation sous la référence et le contour aux
/// [`FREQUENCES_D_APERCU`], pour un volume de zone. `None` : niveau inconnu.
pub fn apercu(reglage: &ReglageContour, volume_db: Option<f64>) -> Option<(f64, Vec<(f64, f64)>)> {
    let a = reglage.attenuation_db(volume_db)?;
    let cible = contour_cible(reglage, a);
    let points = FREQUENCES_D_APERCU
        .iter()
        .map(|f| {
            let i = FREQUENCES_ISO
                .iter()
                .position(|x| x == f)
                .unwrap_or(INDICE_1_KHZ);
            (*f, (cible[i] * 10.0).round() / 10.0)
        })
        .collect();
    Some((a, points))
}

/// Ce que le contour fait sur une zone, et pourquoi il ne fait rien.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EtatDuContour {
    /// Réglage éteint.
    Desactive,
    /// Installé sur la chaîne de la zone.
    Actif,
    /// PURE (ou une mesure de pièce) tient la zone : chemin intouché.
    ContournePure,
    /// Bit-perfect strict demandé : aucun traitement.
    ContourneBitPerfectStrict,
    /// Volume fixe sans niveau manuel : Tune ne connaît pas le niveau
    /// d'écoute (il se règle en aval).
    NiveauInconnu,
    /// La sortie de la zone n'applique pas encore le contour (sortie réseau).
    SortieNonPriseEnCharge,
}

impl EtatDuContour {
    pub fn code(self) -> &'static str {
        match self {
            Self::Desactive => "disabled",
            Self::Actif => "active",
            Self::ContournePure => "bypassed_pure",
            Self::ContourneBitPerfectStrict => "bypassed_strict_bitperfect",
            Self::NiveauInconnu => "level_unknown_fixed_volume",
            Self::SortieNonPriseEnCharge => "output_not_supported",
        }
    }

    /// La décision, sans base : PURE et le bit-perfect strict priment sur tout.
    pub fn decider(
        reglage: &ReglageContour,
        pure: bool,
        strict: bool,
        volume_fixe: bool,
        sortie_locale: bool,
    ) -> Self {
        if !reglage.enabled {
            Self::Desactive
        } else if pure {
            Self::ContournePure
        } else if strict {
            Self::ContourneBitPerfectStrict
        } else if volume_fixe && reglage.level_source == SourceDuNiveau::Volume {
            Self::NiveauInconnu
        } else if !sortie_locale {
            Self::SortieNonPriseEnCharge
        } else {
            Self::Actif
        }
    }
}

/// Le réglage rangé d'une zone (défaut éteint).
pub fn lire(
    db: &std::sync::Arc<dyn crate::db::backend::DbBackend>,
    zone_id: i64,
) -> ReglageContour {
    let texte = crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .get(&cle(zone_id))
        .ok()
        .flatten();
    ReglageContour::depuis_json(texte.as_deref())
}

/// Le réglage d'une zone et ce qu'il y fait, lus en base. `sortie_locale` :
/// la zone joue sur une sortie `local:`.
pub fn etat_de_la_zone(
    db: &std::sync::Arc<dyn crate::db::backend::DbBackend>,
    zone_id: i64,
    sortie_locale: bool,
) -> (ReglageContour, EtatDuContour) {
    let reglage = lire(db, zone_id);
    if !reglage.enabled {
        return (reglage, EtatDuContour::Desactive);
    }
    let volume_fixe = crate::db::zone_repo::ZoneRepo::with_backend(db.clone())
        .get(zone_id)
        .ok()
        .flatten()
        .is_some_and(|z| z.fixed_volume);
    let etat = EtatDuContour::decider(
        &reglage,
        super::audiophile::traitements_contournes(db, zone_id),
        super::bitperfect_strict::zone_enabled(db, zone_id),
        volume_fixe,
        sortie_locale,
    );
    (reglage, etat)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reglage() -> ReglageContour {
        ReglageContour {
            enabled: true,
            ..Default::default()
        }
    }

    fn indice(f: f64) -> usize {
        FREQUENCES_ISO.iter().position(|x| *x == f).unwrap()
    }

    #[test]
    fn iso226_a_1_khz_le_niveau_vaut_les_phones() {
        for phon in [20.0, 40.0, 60.0, 80.0, 90.0] {
            let spl = spl_iso226(INDICE_1_KHZ, phon);
            assert!((spl - phon).abs() < 0.1, "{phon} phones : {spl} dB SPL");
        }
    }

    /// Valeurs publiées de la norme (tableau de la courbe à 40 phones).
    #[test]
    fn iso226_valeurs_de_reference_a_40_phones() {
        for (f, attendu) in [(20.0, 99.85), (100.0, 64.4), (4000.0, 36.65)] {
            let spl = spl_iso226(indice(f), 40.0);
            assert!(
                (spl - attendu).abs() < 0.3,
                "{f} Hz : {spl}, attendu {attendu}"
            );
        }
    }

    #[test]
    fn le_contour_est_nul_a_la_reference() {
        let r = reglage();
        let c = contour_cible(&r, r.attenuation_db(Some(0.0)).unwrap());
        assert!(c.iter().all(|g| g.abs() < 1e-9), "{c:?}");
        let k = Consigne::calculer(&r, Some(0.0), 48_000).unwrap();
        assert!(k.est_nulle(), "{k:?}");
    }

    /// Valeurs du contour à plusieurs volumes (référence 80 phones à 0 dB).
    #[test]
    fn valeurs_du_contour_a_plusieurs_volumes() {
        let r = reglage();
        let attendu = [
            // (volume dB, gain à 31,5 Hz, à 100 Hz, à 10 kHz)
            (-10.0, 4.7, 3.1, 0.7),
            (-20.0, 9.4, 6.2, 1.4),
            (-30.0, 14.0, 9.1, 2.0),
            (-40.0, 15.98, 10.26, 2.2),
        ];
        for (v, g31, g100, g10k) in attendu {
            let c = contour_cible(&r, r.attenuation_db(Some(v)).unwrap());
            assert!(
                (c[indice(31.5)] - g31).abs() < 0.15,
                "{v} dB : 31,5 Hz {}",
                c[indice(31.5)]
            );
            assert!(
                (c[indice(100.0)] - g100).abs() < 0.15,
                "{v} dB : 100 Hz {}",
                c[indice(100.0)]
            );
            assert!(
                (c[indice(10000.0)] - g10k).abs() < 0.15,
                "{v} dB : 10 kHz {}",
                c[indice(10000.0)]
            );
            assert_eq!(c[INDICE_1_KHZ], 0.0);
            // 0 ≤ G ≤ A dans le grave.
            assert!(
                c[..INDICE_1_KHZ]
                    .iter()
                    .all(|g| *g >= 0.0 && *g <= -v + 1e-9)
            );
        }
    }

    #[test]
    fn le_contour_croit_quand_le_volume_baisse() {
        let r = reglage();
        let mut avant = 0.0;
        for v in [-5.0, -15.0, -25.0, -35.0] {
            let g = contour_cible(&r, r.attenuation_db(Some(v)).unwrap())[indice(50.0)];
            assert!(g > avant, "{v} dB : {g} ≤ {avant}");
            avant = g;
        }
    }

    #[test]
    fn la_reference_deplace_le_point_nul() {
        let r = ReglageContour {
            reference_volume_db: -20.0,
            ..reglage()
        };
        assert_eq!(r.attenuation_db(Some(-20.0)), Some(0.0));
        assert_eq!(r.attenuation_db(Some(-10.0)), Some(0.0), "au-dessus : rien");
        assert_eq!(r.attenuation_db(Some(-35.0)), Some(15.0));
        assert_eq!(r.attenuation_db(None), None, "niveau inconnu");
    }

    #[test]
    fn grave_seulement_et_intensite() {
        let plein = contour_cible(&reglage(), 20.0);
        let grave = contour_cible(
            &ReglageContour {
                bass_only: true,
                ..reglage()
            },
            20.0,
        );
        assert!(grave[INDICE_1_KHZ..].iter().all(|g| *g == 0.0));
        assert_eq!(grave[..INDICE_1_KHZ], plein[..INDICE_1_KHZ]);
        let gains = ajuster(&grave, 48_000, true);
        assert_eq!((gains[2], gains[3]), (0.0, 0.0), "{gains:?}");
        let moitie = contour_cible(
            &ReglageContour {
                intensity_pct: 50.0,
                ..reglage()
            },
            20.0,
        );
        for (m, p) in moitie.iter().zip(&plein) {
            assert!((m - p / 2.0).abs() < 1e-9);
        }
    }

    #[test]
    fn le_niveau_manuel_ne_lit_pas_le_volume() {
        let r = ReglageContour {
            level_source: SourceDuNiveau::Manual,
            manual_level_db: -25.0,
            ..reglage()
        };
        assert_eq!(r.attenuation_db(None), Some(25.0));
        assert_eq!(r.attenuation_db(Some(0.0)), Some(25.0));
        // Volume numérique à 0 dB : le préampli reprend la crête.
        let k = Consigne::calculer(&r, Some(0.0), 48_000).unwrap();
        assert!(k.preampli_db < -5.0, "{k:?}");
    }

    #[test]
    fn les_filtres_suivent_le_contour_a_1_2_db_pres() {
        let r = reglage();
        for sr in [44_100, 48_000, 96_000, 192_000] {
            for a in [5.0, 10.0, 20.0, 30.0, 40.0, 60.0] {
                let cible = contour_cible(&r, a);
                let gains = ajuster(&cible, sr, false);
                let tolerance = 1.2;
                for i in 0..28 {
                    let e = reponse_db(&gains, FREQUENCES_ISO[i], sr) - cible[i];
                    assert!(
                        e.abs() < tolerance,
                        "{sr} Hz, A={a} : {} Hz écart {e:.2}",
                        FREQUENCES_ISO[i]
                    );
                }
            }
        }
    }

    #[test]
    fn la_crete_ne_depasse_pas_la_reference_en_mode_volume() {
        let r = reglage();
        for v in [-10.0, -20.0, -40.0] {
            let k = Consigne::calculer(&r, Some(v), 48_000).unwrap();
            let crete = FREQUENCES_ISO
                .iter()
                .map(|f| reponse_db(&k.gains_db, *f, 48_000))
                .fold(f64::MIN, f64::max);
            assert!(
                crete + k.preampli_db + v <= 1e-6,
                "{v} dB : crête {crete} pre {}",
                k.preampli_db
            );
        }
    }

    #[test]
    fn identite_exacte_a_la_reference() {
        let mut c = ContourDeSonie::new(reglage(), 48_000, 2, Some(0.0));
        let entree: Vec<f32> = (0..4_800)
            .map(|i| ((i as f32) * 0.013).sin() * 0.7)
            .collect();
        let mut sortie = entree.clone();
        c.process_interleaved(&mut sortie);
        assert!(c.est_identite());
        assert_eq!(sortie, entree, "aucun échantillon ne doit changer");
    }

    fn sinus(f: f64, sr: u32, trames: usize, phase0: usize) -> Vec<f32> {
        (0..trames)
            .flat_map(|n| {
                let s =
                    (std::f64::consts::TAU * f * (n + phase0) as f64 / f64::from(sr)).sin() * 0.1;
                [s as f32, s as f32]
            })
            .collect()
    }

    fn niveau_db(x: &[f32]) -> f64 {
        let rms = (x.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / x.len() as f64).sqrt();
        20.0 * (rms / (0.1 / 2f64.sqrt())).log10()
    }

    #[test]
    fn le_grave_monte_quand_le_volume_baisse_et_1_khz_ne_bouge_pas() {
        let sr = 48_000;
        let mut c = ContourDeSonie::new(reglage(), sr, 2, Some(-30.0));
        let mut grave = sinus(50.0, sr, sr as usize, 0);
        c.process_interleaved(&mut grave);
        let g = niveau_db(&grave[grave.len() / 2..]);
        let attendu = c.consigne_courante();
        let prevu = reponse_db(&attendu.gains_db, 50.0, sr) + attendu.preampli_db;
        assert!(
            (g - prevu).abs() < 0.3,
            "50 Hz : {g:.2} dB, prévu {prevu:.2}"
        );
        assert!(g > 8.0, "50 Hz relevé de {g:.2} dB seulement");
        let mut c = ContourDeSonie::new(reglage(), sr, 2, Some(-30.0));
        let mut milieu = sinus(1000.0, sr, sr as usize / 2, 0);
        c.process_interleaved(&mut milieu);
        let m = niveau_db(&milieu[milieu.len() / 2..]) - c.consigne_courante().preampli_db;
        assert!(m.abs() < 1.0, "1 kHz bouge de {m:.2} dB");
    }

    /// Un saut de volume ne fait pas sauter le signal : la consigne est
    /// rejointe en douceur, sans marche d'un échantillon à l'autre.
    #[test]
    fn un_saut_de_volume_est_lisse_sans_clic() {
        let sr = 48_000;
        let mut c = ContourDeSonie::new(reglage(), sr, 2, Some(0.0));
        let mut tout = Vec::new();
        let mut a = sinus(60.0, sr, 4_800, 0);
        c.process_interleaved(&mut a);
        tout.extend(a);
        c.suivre_le_volume(Some(-40.0));
        assert!(c.consigne_courante().est_nulle(), "rien ne saute à l'appel");
        let mut b = sinus(60.0, sr, sr as usize, 4_800);
        c.process_interleaved(&mut b);
        tout.extend(b);
        // Pente maximale d'un sinus de 60 Hz relevé à ~+15 dB : 2π·60/48000 ·
        // 0,1 · 10^(15/20) ≈ 0,0044 par échantillon. Une marche la dépasserait.
        let pente = tout
            .chunks(2)
            .zip(tout.chunks(2).skip(1))
            .map(|(x, y)| (y[0] - x[0]).abs())
            .fold(0.0f32, f32::max);
        assert!(pente < 0.006, "marche de {pente}");
        assert_eq!(
            c.consigne_courante(),
            c.consigne_cible(),
            "cible rejointe en 1 s"
        );
    }

    #[test]
    fn couper_ramene_a_zero_puis_au_repos() {
        let sr = 48_000;
        let mut c = ContourDeSonie::new(reglage(), sr, 2, Some(-30.0));
        c.eteindre();
        assert!(!c.est_au_repos(), "le grave redescend en douceur");
        let mut x = sinus(60.0, sr, sr as usize, 0);
        c.process_interleaved(&mut x);
        assert!(c.est_au_repos());
    }

    #[test]
    fn pure_et_bit_perfect_strict_priment() {
        let r = reglage();
        let d = |pure, strict, fixe, local| EtatDuContour::decider(&r, pure, strict, fixe, local);
        assert_eq!(d(false, false, false, true), EtatDuContour::Actif);
        assert_eq!(d(true, false, false, true), EtatDuContour::ContournePure);
        assert_eq!(d(true, true, true, false), EtatDuContour::ContournePure);
        assert_eq!(
            d(false, true, false, true),
            EtatDuContour::ContourneBitPerfectStrict
        );
        assert_eq!(d(false, false, true, true), EtatDuContour::NiveauInconnu);
        assert_eq!(
            d(false, false, false, false),
            EtatDuContour::SortieNonPriseEnCharge
        );
        let manuel = ReglageContour {
            level_source: SourceDuNiveau::Manual,
            ..reglage()
        };
        assert_eq!(
            EtatDuContour::decider(&manuel, false, false, true, true),
            EtatDuContour::Actif,
            "volume fixe + niveau manuel : le contour tourne"
        );
        assert_eq!(
            EtatDuContour::decider(&ReglageContour::default(), false, false, false, true),
            EtatDuContour::Desactive
        );
    }

    #[test]
    fn le_reglage_refuse_les_valeurs_hors_bornes() {
        assert!(reglage().valider().is_ok());
        for r in [
            ReglageContour {
                intensity_pct: 120.0,
                ..reglage()
            },
            ReglageContour {
                reference_volume_db: 3.0,
                ..reglage()
            },
            ReglageContour {
                reference_phon: 100.0,
                ..reglage()
            },
            ReglageContour {
                curve: "fletcher".into(),
                ..reglage()
            },
            ReglageContour {
                manual_level_db: f64::NAN,
                ..reglage()
            },
        ] {
            assert!(r.valider().is_err(), "{r:?}");
        }
        assert_eq!(ReglageContour::depuis_json(None), ReglageContour::default());
        assert!(
            !ReglageContour::depuis_json(Some("{\"enabled\":true,\"intensity_pct\":500}")).enabled
        );
    }
}
