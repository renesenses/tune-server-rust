//! Crossfeed Pro (#5039, phase 1) : le crossfeed par DIFFÉRENCE du greffon
//! `crossfeed` (v1), avec une voie croisée filtrée et une garde de phase.
//!
//! # Principe conservé du v1
//!
//! Pour chaque trame stéréo `n`, `Ld`/`Rd` étant les échantillons retardés de
//! `delay_samples` :
//!
//! ```text
//! y     = f(Rd − Ld)            // voie croisée, filtrée (linéaire)
//! L_out = L + k·y               // = L + k·f(Rd − Ld)
//! R_out = R − k·y               // = R + k·f(Ld − Rd), f étant linéaire
//! ```
//!
//! Les deux termes sont exactement opposés : `L_out + R_out == L + R` à
//! l'arrondi `f32` près. Le Mid est conservé, la balance tonale du mono ne
//! bouge pas, et une source mono (`L == R`) donne `y == 0` : elle traverse
//! intacte, au bit près.
//!
//! # La voie croisée `f`
//!
//! - **ombre de la tête** : passe-bas du 1er ordre (6 dB/octave), de 100 Hz à
//!   10 kHz, DÉSACTIVÉ par défaut (« zéro coloration », décision de Bertrand) ;
//! - **coupe-bas** : passe-haut du 1er ordre à 150 Hz, désactivé par défaut.
//!
//! Les deux sont des filtres du 1er ordre par transformée bilinéaire avec
//! pré-distorsion : la fréquence de coupure (−3 dB) tombe là où on la
//! demande, quel que soit le débit.
//!
//! # Garde de phase
//!
//! Corrélation L/R lissée (constante de temps de 20 à 50 ms). Quand elle
//! devient négative, le dosage effectif est multiplié par `1 + ρ` (donc nul à
//! ρ = −1), puis lissé à son tour : aucune marche d'un échantillon à l'autre.
//! Sur un signal ordinaire (ρ ≥ 0), elle ne fait RIEN.
//!
//! # Changer un réglage en cours de lecture
//!
//! Un nouveau réglage ne remplace pas l'ancien d'un coup : les deux voies
//! croisées tournent ensemble pendant [`FONDU_MS`] et on passe de l'une à
//! l'autre par un fondu linéaire. Retard, fréquence, activation d'un filtre
//! ou dosage : tout passe par ce même fondu. Un réglage qui arrive pendant un
//! fondu attend la fin de celui-ci (seul le dernier est gardé).

use std::f64::consts::PI;

/// Dosage minimal et maximal de la voie croisée (k).
pub const MIN_AMOUNT: f32 = 0.20;
pub const MAX_AMOUNT: f32 = 0.60;
/// Retard maximal de la voie croisée, en ms. Le retard interaural
/// physiologique plafonne vers 0,6–0,7 ms. C'est aussi la longueur de
/// l'historique partagé par les voies.
pub const MAX_DELAY_MS: f32 = 1.0;
/// Bornes de la fréquence de coupure de l'ombre de la tête, en Hz.
pub const MIN_HEAD_SHADOW_HZ: f32 = 100.0;
pub const MAX_HEAD_SHADOW_HZ: f32 = 10_000.0;
/// Fréquence du coupe-bas de la voie croisée, en Hz (fixe).
pub const LOW_CUT_HZ: f32 = 150.0;
/// Bornes de la constante de temps de la garde de phase, en ms.
pub const MIN_PHASE_GUARD_MS: f32 = 20.0;
pub const MAX_PHASE_GUARD_MS: f32 = 50.0;
/// Durée du fondu entre deux réglages, en ms.
pub const FONDU_MS: f64 = 20.0;
/// Constante de temps du lissage du facteur de garde, en ms.
const LISSAGE_GARDE_MS: f64 = 10.0;

/// Les réglages qui font le son, déjà validés.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Params {
    /// Dosage k de la voie croisée ; 0 = identité (greffon éteint).
    pub amount: f32,
    /// Retard de la voie croisée, en ms.
    pub delay_ms: f32,
    /// Passe-bas « ombre de la tête » : `Some(fc)` s'il est actif.
    pub head_shadow_hz: Option<f32>,
    /// Coupe-bas à [`LOW_CUT_HZ`] sur la voie croisée.
    pub low_cut: bool,
    /// Garde de phase : `Some(constante de temps en ms)` si elle est active.
    pub phase_guard_ms: Option<f32>,
}

impl Params {
    /// Aucun traitement : le greffon éteint.
    pub const IDENTITE: Params = Params {
        amount: 0.0,
        delay_ms: 0.0,
        head_shadow_hz: None,
        low_cut: false,
        phase_guard_ms: None,
    };
}

/// Filtre du 1er ordre, transformée bilinéaire pré-distordue.
/// `y[n] = b0·x[n] + b1·x[n−1] − a1·y[n−1]`.
#[derive(Debug, Clone, Copy)]
struct PremierOrdre {
    b0: f64,
    b1: f64,
    a1: f64,
    x1: f64,
    y1: f64,
}

impl PremierOrdre {
    fn coefficient(sample_rate: u32, fc: f32) -> f64 {
        let fs = f64::from(sample_rate);
        // Sous Nyquist, sinon tan() diverge.
        let fc = f64::from(fc).clamp(1.0, 0.45 * fs);
        (PI * fc / fs).tan()
    }
    fn passe_bas(sample_rate: u32, fc: f32) -> Self {
        let k = Self::coefficient(sample_rate, fc);
        Self {
            b0: k / (1.0 + k),
            b1: k / (1.0 + k),
            a1: (k - 1.0) / (k + 1.0),
            x1: 0.0,
            y1: 0.0,
        }
    }
    fn passe_haut(sample_rate: u32, fc: f32) -> Self {
        let k = Self::coefficient(sample_rate, fc);
        Self {
            b0: 1.0 / (1.0 + k),
            b1: -1.0 / (1.0 + k),
            a1: (k - 1.0) / (k + 1.0),
            x1: 0.0,
            y1: 0.0,
        }
    }
    #[inline]
    fn filtrer(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.b1 * self.x1 - self.a1 * self.y1;
        self.x1 = x;
        self.y1 = y;
        y
    }
    fn reprendre(&mut self, autre: &PremierOrdre) {
        self.x1 = autre.x1;
        self.y1 = autre.y1;
    }
    fn effacer(&mut self) {
        self.x1 = 0.0;
        self.y1 = 0.0;
    }
}

/// Une voie croisée pour UN jeu de réglages : son retard et ses filtres.
/// Deux voies tournent ensemble pendant un fondu. L'historique du signal,
/// lui, appartient au moteur ([`Historique`]) : une voie neuve qui allonge le
/// retard y lit le vrai passé, pas des zéros.
#[derive(Debug, Clone)]
struct Voie {
    params: Params,
    delay_samples: usize,
    passe_bas: Option<PremierOrdre>,
    passe_haut: Option<PremierOrdre>,
}

impl Voie {
    fn new(sample_rate: u32, params: Params) -> Self {
        Self {
            params,
            delay_samples: retard_en_echantillons(sample_rate, params.delay_ms),
            passe_bas: params
                .head_shadow_hz
                .map(|fc| PremierOrdre::passe_bas(sample_rate, fc)),
            passe_haut: params
                .low_cut
                .then(|| PremierOrdre::passe_haut(sample_rate, LOW_CUT_HZ)),
        }
    }

    /// `f(Rd − Ld)`, en avançant les filtres.
    #[inline]
    fn croisee(&mut self, h: &Historique) -> f64 {
        let (ld, rd) = h.retarde(self.delay_samples);
        let mut y = f64::from(rd) - f64::from(ld);
        if let Some(f) = &mut self.passe_bas {
            y = f.filtrer(y);
        }
        if let Some(f) = &mut self.passe_haut {
            y = f.filtrer(y);
        }
        y
    }

    /// Reprendre l'état des filtres présents des deux côtés.
    fn reprendre(&mut self, prec: &Voie) {
        if let (Some(a), Some(b)) = (&mut self.passe_bas, &prec.passe_bas) {
            a.reprendre(b);
        }
        if let (Some(a), Some(b)) = (&mut self.passe_haut, &prec.passe_haut) {
            a.reprendre(b);
        }
    }

    fn effacer(&mut self) {
        if let Some(f) = &mut self.passe_bas {
            f.effacer();
        }
        if let Some(f) = &mut self.passe_haut {
            f.effacer();
        }
    }

    /// Le dosage effectif de cette voie, compte tenu de la garde.
    #[inline]
    fn dosage(&self, garde: f64) -> f64 {
        let k = f64::from(self.params.amount);
        if self.params.phase_guard_ms.is_some() {
            k * garde
        } else {
            k
        }
    }
}

/// Les dernières trames du signal SEC, sur le retard maximal : partagées
/// par toutes les voies, qui y lisent chacune à leur retard.
#[derive(Debug, Clone)]
struct Historique {
    l: Vec<f32>,
    r: Vec<f32>,
    /// Indice de la trame la plus récente.
    pos: usize,
}

impl Historique {
    fn new(sample_rate: u32) -> Self {
        let n = retard_en_echantillons(sample_rate, MAX_DELAY_MS) + 1;
        Self {
            l: vec![0.0; n],
            r: vec![0.0; n],
            pos: 0,
        }
    }
    #[inline]
    fn pousser(&mut self, l: f32, r: f32) {
        self.pos += 1;
        if self.pos >= self.l.len() {
            self.pos = 0;
        }
        self.l[self.pos] = l;
        self.r[self.pos] = r;
    }
    /// La trame d'il y a `retard` trames (0 = la plus récente).
    #[inline]
    fn retarde(&self, retard: usize) -> (f32, f32) {
        let n = self.l.len();
        let i = (self.pos + n - retard.min(n - 1)) % n;
        (self.l[i], self.r[i])
    }
    fn effacer(&mut self) {
        self.l.fill(0.0);
        self.r.fill(0.0);
        self.pos = 0;
    }
}

/// Corrélation L/R lissée et facteur de garde qui en découle.
#[derive(Debug, Clone, Copy)]
struct Garde {
    sll: f64,
    srr: f64,
    slr: f64,
    /// Facteur appliqué au dosage, lissé, dans [0, 1].
    facteur: f64,
    alpha: f64,
    beta: f64,
}

impl Garde {
    fn new(sample_rate: u32, constante_ms: f32) -> Self {
        let mut g = Self {
            sll: 0.0,
            srr: 0.0,
            slr: 0.0,
            facteur: 1.0,
            alpha: 0.0,
            beta: 0.0,
        };
        g.regler(sample_rate, constante_ms);
        g
    }
    fn regler(&mut self, sample_rate: u32, constante_ms: f32) {
        let fs = f64::from(sample_rate);
        self.alpha = 1.0 - (-1000.0 / (f64::from(constante_ms) * fs)).exp();
        self.beta = 1.0 - (-1000.0 / (LISSAGE_GARDE_MS * fs)).exp();
    }
    /// Corrélation courante, dans [−1, 1] ; 0 sur le silence.
    fn correlation(&self) -> f64 {
        let energie = (self.sll * self.srr).sqrt();
        if energie <= 1e-12 {
            0.0
        } else {
            (self.slr / energie).clamp(-1.0, 1.0)
        }
    }
    #[inline]
    fn observer(&mut self, l: f32, r: f32) {
        let (l, r) = (f64::from(l), f64::from(r));
        self.sll += self.alpha * (l * l - self.sll);
        self.srr += self.alpha * (r * r - self.srr);
        self.slr += self.alpha * (l * r - self.slr);
        let cible = (1.0 + self.correlation().min(0.0)).clamp(0.0, 1.0);
        self.facteur += self.beta * (cible - self.facteur);
    }
    fn effacer(&mut self) {
        self.sll = 0.0;
        self.srr = 0.0;
        self.slr = 0.0;
        self.facteur = 1.0;
    }
}

/// Le moteur du greffon : voie courante, voie sortante pendant un fondu,
/// réglage en attente et garde de phase.
#[derive(Debug, Clone)]
pub struct CrossfeedProEngine {
    sample_rate: u32,
    courante: Voie,
    sortante: Option<Voie>,
    en_attente: Option<Params>,
    fondu_pos: u32,
    fondu_len: u32,
    garde: Garde,
    historique: Historique,
}

impl CrossfeedProEngine {
    pub fn new(sample_rate: u32, params: Params) -> Self {
        let fondu_len = ((FONDU_MS / 1000.0) * f64::from(sample_rate))
            .round()
            .max(1.0) as u32;
        Self {
            sample_rate,
            courante: Voie::new(sample_rate, params),
            sortante: None,
            en_attente: None,
            fondu_pos: 0,
            fondu_len,
            garde: Garde::new(
                sample_rate,
                params.phase_guard_ms.unwrap_or(MIN_PHASE_GUARD_MS),
            ),
            historique: Historique::new(sample_rate),
        }
    }

    /// Réglage en cours de lecture : fondu de [`FONDU_MS`] depuis l'état
    /// courant. Pendant un fondu, le réglage attend (seul le dernier compte).
    pub fn set_params(&mut self, params: Params) {
        if self.sortante.is_some() {
            self.en_attente = Some(params);
        } else {
            self.demarrer_fondu(params);
        }
    }

    fn demarrer_fondu(&mut self, params: Params) {
        if params == self.courante.params {
            return;
        }
        if let Some(ms) = params.phase_guard_ms {
            self.garde.regler(self.sample_rate, ms);
        }
        let mut neuve = Voie::new(self.sample_rate, params);
        neuve.reprendre(&self.courante);
        self.sortante = Some(std::mem::replace(&mut self.courante, neuve));
        self.fondu_pos = 0;
    }

    /// Les réglages de la voie courante (ceux vers lesquels on va).
    pub fn params(&self) -> Params {
        self.courante.params
    }

    /// Rien à faire : dosage nul et aucun fondu, ni réglage, en cours.
    pub fn is_identity(&self) -> bool {
        self.courante.params.amount == 0.0 && self.sortante.is_none() && self.en_attente.is_none()
    }

    /// Le dosage réellement appliqué à la dernière trame (fondu et garde
    /// compris).
    pub fn effective_amount(&self) -> f32 {
        let courant = self.courante.dosage(self.garde.facteur);
        match &self.sortante {
            None => courant as f32,
            Some(s) => {
                let w = f64::from(self.fondu_pos) / f64::from(self.fondu_len);
                (w * courant + (1.0 - w) * s.dosage(self.garde.facteur)) as f32
            }
        }
    }

    /// La corrélation L/R lissée que voit la garde de phase.
    pub fn correlation(&self) -> f32 {
        self.garde.correlation() as f32
    }

    /// Retard effectif de la voie courante, en échantillons.
    pub fn delay_samples(&self) -> usize {
        self.courante.delay_samples
    }

    /// Faire avancer l'historique sans toucher au signal (greffon éteint) :
    /// au réallumage, ligne à retard et garde sont déjà à jour.
    pub fn observe_interleaved(&mut self, samples: &[f32]) {
        for trame in samples.as_chunks::<2>().0 {
            self.garde.observer(trame[0], trame[1]);
            self.historique.pousser(trame[0], trame[1]);
        }
    }

    /// Traiter en place un tampon stéréo entrelacé `[L0, R0, L1, R1, …]`.
    /// Un nombre impair d'échantillons est laissé intact.
    pub fn process_interleaved(&mut self, samples: &mut [f32]) {
        if !samples.len().is_multiple_of(2) {
            return;
        }
        for trame in samples.as_chunks_mut::<2>().0 {
            let (l, r) = (trame[0], trame[1]);
            self.garde.observer(l, r);
            self.historique.pousser(l, r);
            let g = self.garde.facteur;
            let mut c = self.courante.dosage(g) * self.courante.croisee(&self.historique);
            if let Some(s) = &mut self.sortante {
                let sortant = s.dosage(g) * s.croisee(&self.historique);
                self.fondu_pos += 1;
                let w = f64::from(self.fondu_pos) / f64::from(self.fondu_len);
                c = w * c + (1.0 - w) * sortant;
                if self.fondu_pos >= self.fondu_len {
                    self.sortante = None;
                    if let Some(p) = self.en_attente.take() {
                        self.demarrer_fondu(p);
                    }
                }
            }
            let c = c as f32;
            trame[0] = (l + c).clamp(-1.0, 1.0);
            trame[1] = (r - c).clamp(-1.0, 1.0);
        }
    }

    /// Reprendre l'état d'un moteur précédent (remplacement à chaud).
    pub fn inherit_state_from(&mut self, prec: &CrossfeedProEngine) {
        self.courante.reprendre(&prec.courante);
        self.historique.clone_from(&prec.historique);
        let (alpha, beta) = (self.garde.alpha, self.garde.beta);
        self.garde = prec.garde;
        self.garde.alpha = alpha;
        self.garde.beta = beta;
    }

    /// Oublier l'historique (nouvelle piste, saut) ; les réglages restent.
    pub fn reset_history(&mut self) {
        self.courante.effacer();
        self.sortante = None;
        if let Some(p) = self.en_attente.take() {
            self.courante = Voie::new(self.sample_rate, p);
        }
        self.garde.effacer();
        self.historique.effacer();
    }
}

fn retard_en_echantillons(sample_rate: u32, delay_ms: f32) -> usize {
    let ms = delay_ms.clamp(0.0, MAX_DELAY_MS);
    ((ms / 1000.0) * sample_rate as f32).round() as usize
}

// ---------------------------------------------------------------------------
// Préréglages : valeurs de libbs2b
// ---------------------------------------------------------------------------
//
// Source : libbs2b 3.1.0 (Boris Mikhaylov), archive
// `libbs2b-3.1.0.tar.gz` de https://sourceforge.net/projects/bs2b/files/libbs2b/3.1.0/
// (sha256 6aaafd81aae3898ee40148dd1349aab348db9bfae9767d0e66e0b07ddd4b2528),
// fichier `src/bs2b.h`, lignes 54 à 58 :
//
// ```c
// /* Default crossfeed levels */
// /* bs2b_set_level() */
// #define BS2B_DEFAULT_CLEVEL  ( ( uint32_t )700 | ( ( uint32_t )45 << 16 ) )
// #define BS2B_CMOY_CLEVEL     ( ( uint32_t )700 | ( ( uint32_t )60 << 16 ) )
// #define BS2B_JMEIER_CLEVEL   ( ( uint32_t )650 | ( ( uint32_t )95 << 16 ) )
// ```
//
// Les 16 bits bas sont la fréquence de coupure en Hz, les 16 bits hauts le
// niveau d'injection en dixièmes de dB (« feed level (dB * 10 @ low
// frequencies) », même fichier, ligne 39). Le mode d'emploi livré dans
// l'archive (`win32/bs2bconvert/bs2bconvert-readme.txt`) dit la même chose :
// « default preset - 700Hz/260us, 4.5 dB ; Chu Moy's preset - 700Hz/260us,
// 6.0 dB ; Jan Meier's preset - 650Hz/280us, 9.5 dB ».
//
// « Meier étendu » : libbs2b n'en donne AUCUNE valeur ; il n'est pas proposé.
//
// Traduction dans ce modèle :
// - le niveau d'injection est l'écart, dans le grave, entre la voie directe
//   et la voie croisée (`level = GB_hi − GB_lo` dans `bs2b.c`, `init()`). Ici,
//   dans le grave (filtre à gain 1), la voie directe vaut `1 − k` et la voie
//   croisée `k` : `k = 1 / (1 + 10^(niveau/20))` ;
// - libbs2b n'a PAS de ligne à retard : les 260/280 µs du mode d'emploi sont
//   le retard que produit son propre passe-bas (macro `bs2b_level_delay`).
//   Ajouter ce retard en ligne à retard le doublerait : le préréglage met le
//   retard à 0 et laisse le passe-bas le produire, comme dans libbs2b ;
// - la voie directe de libbs2b est une étagère « highboost » normalisée ; ici
//   elle reste `1 − k·f`, contrainte de la conservation du Mid. Coupure et
//   niveau dans le grave sont repris, pas la courbe entière.

/// Les préréglages imitant les crossfeeds connus, valeurs de libbs2b.
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    /// `BS2B_DEFAULT_CLEVEL` : 700 Hz, 4,5 dB.
    #[default]
    Bs2b,
    /// `BS2B_CMOY_CLEVEL` : 700 Hz, 6,0 dB.
    ChuMoy,
    /// `BS2B_JMEIER_CLEVEL` : 650 Hz, 9,5 dB.
    JanMeier,
}

impl Preset {
    pub const ALL: [Preset; 3] = [Preset::Bs2b, Preset::ChuMoy, Preset::JanMeier];

    /// Le mot `level` de libbs2b, recopié tel quel (voir la source plus haut).
    const fn clevel(self) -> u32 {
        match self {
            Preset::Bs2b => 700 | (45 << 16),
            Preset::ChuMoy => 700 | (60 << 16),
            Preset::JanMeier => 650 | (95 << 16),
        }
    }
    /// Fréquence de coupure du passe-bas, en Hz.
    pub fn cut_hz(self) -> f32 {
        (self.clevel() & 0xffff) as f32
    }
    /// Niveau d'injection dans le grave, en dB.
    pub fn feed_db(self) -> f32 {
        (self.clevel() >> 16) as f32 / 10.0
    }
    /// Le dosage k qui donne ce niveau d'injection dans ce modèle.
    pub fn amount(self) -> f32 {
        (1.0 / (1.0 + 10f64.powf(f64::from(self.feed_db()) / 20.0))) as f32
    }
    pub fn id(self) -> &'static str {
        match self {
            Preset::Bs2b => "bs2b",
            Preset::ChuMoy => "chu_moy",
            Preset::JanMeier => "jan_meier",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Preset::Bs2b => "BS2B",
            Preset::ChuMoy => "Chu Moy",
            Preset::JanMeier => "Jan Meier",
        }
    }
    /// La constante de libbs2b d'où viennent les valeurs.
    pub fn source(self) -> &'static str {
        match self {
            Preset::Bs2b => "libbs2b 3.1.0, src/bs2b.h, BS2B_DEFAULT_CLEVEL",
            Preset::ChuMoy => "libbs2b 3.1.0, src/bs2b.h, BS2B_CMOY_CLEVEL",
            Preset::JanMeier => "libbs2b 3.1.0, src/bs2b.h, BS2B_JMEIER_CLEVEL",
        }
    }
}

// ---------------------------------------------------------------------------
// Retour à l'entier, à la même échelle que le décodage (#4973, repris du v1)
// ---------------------------------------------------------------------------

pub(crate) fn quantifier_i16(s: f32) -> i16 {
    tune_plugin_audio_support::dither::quantifier_avec(
        f64::from(s) * 32_768.0,
        0.0,
        -32_768.0,
        32_767.0,
    ) as i16
}

pub(crate) fn quantifier_i24(s: f32) -> [u8; 3] {
    let v = tune_plugin_audio_support::dither::quantifier_avec(
        f64::from(s) * 8_388_608.0,
        0.0,
        -8_388_608.0,
        8_388_607.0,
    ) as i32;
    let b = v.to_le_bytes();
    [b[0], b[1], b[2]]
}

pub(crate) fn quantifier_i32(s: f32) -> i32 {
    tune_plugin_audio_support::dither::quantifier_avec(
        f64::from(s) * 2_147_483_648.0,
        0.0,
        f64::from(i32::MIN),
        f64::from(i32::MAX),
    ) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    const FS: u32 = 96_000;

    fn params(amount: f32) -> Params {
        Params {
            amount,
            delay_ms: 0.3,
            head_shadow_hz: None,
            low_cut: false,
            phase_guard_ms: Some(30.0),
        }
    }

    /// Stéréo entrelacée : L et R deux sinus sans rapport simple.
    fn stereo(frames: usize, fs: u32) -> Vec<f32> {
        (0..frames)
            .flat_map(|n| {
                let t = n as f64 / f64::from(fs);
                // Amplitudes choisies pour que rien n'écrête (|L|, |R| ≤ 0,3 ;
                // |k·y| ≤ 0,6 × 0,6) : l'écrêtage à ±1 est hors de la promesse.
                let l = 0.25 * (2.0 * PI * 440.0 * t).sin() + 0.05 * (2.0 * PI * 3_100.0 * t).sin();
                let r = 0.20 * (2.0 * PI * 523.0 * t + 0.7).sin()
                    - 0.06 * (2.0 * PI * 7_000.0 * t).sin();
                [l as f32, r as f32]
            })
            .collect()
    }

    /// Amplitude efficace de la voie croisée seule, sur un sinus à `freq` :
    /// L = sinus, R = 0, donc R_out = −k·f(−Ld) = k·f(Ld). On mesure
    /// R_out / (k·A) après établissement : c'est |f| à cette fréquence.
    fn gain_croise_db(p: Params, freq: f64) -> f64 {
        let mut e = CrossfeedProEngine::new(FS, p);
        let n = FS as usize / 2; // 0,5 s
        let a = 0.5;
        let mut s: Vec<f32> = (0..n)
            .flat_map(|i| {
                let v = a * (2.0 * PI * freq * i as f64 / f64::from(FS)).sin();
                [v as f32, 0.0]
            })
            .collect();
        e.process_interleaved(&mut s);
        let debut = n / 2; // on laisse s'établir
        let (mut acc_out, mut acc_in) = (0.0f64, 0.0f64);
        for i in debut..n {
            let x = a * (2.0 * PI * freq * i as f64 / f64::from(FS)).sin();
            acc_in += x * x;
            acc_out += f64::from(s[2 * i + 1]).powi(2);
        }
        let k = f64::from(p.amount);
        10.0 * (acc_out / (acc_in * k * k)).log10()
    }

    /// Mid conservé : |(L_out + R_out) − (L + R)| ≤ 1e-6 sur un signal
    /// stéréo quelconque, filtres actifs et inactifs.
    #[test]
    fn le_mid_est_conserve() {
        for p in [
            params(0.3),
            Params {
                head_shadow_hz: Some(700.0),
                low_cut: true,
                ..params(0.6)
            },
        ] {
            let mut e = CrossfeedProEngine::new(FS, p);
            let entree = stereo(FS as usize / 4, FS);
            let mut sortie = entree.clone();
            e.process_interleaved(&mut sortie);
            let erreur = entree
                .as_chunks::<2>()
                .0
                .iter()
                .zip(sortie.as_chunks::<2>().0)
                .map(|(i, o)| ((o[0] + o[1]) - (i[0] + i[1])).abs())
                .fold(0.0f32, f32::max);
            assert!(
                erreur <= 1e-6,
                "Mid : erreur maximale {erreur:e} pour {p:?}"
            );
            assert_ne!(entree, sortie, "le greffon n'a rien fait");
        }
    }

    /// Une source mono (L == R) traverse intacte, au bit près.
    #[test]
    fn le_mono_est_intact() {
        let p = Params {
            head_shadow_hz: Some(700.0),
            low_cut: true,
            ..params(0.6)
        };
        let mut e = CrossfeedProEngine::new(FS, p);
        let entree: Vec<f32> = stereo(20_000, FS)
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|t| [t[0], t[0]])
            .collect();
        let mut sortie = entree.clone();
        e.process_interleaved(&mut sortie);
        assert_eq!(sortie, entree);
    }

    /// Passe-bas actif : −3 dB à la coupure, puis 6 dB/octave. Inactif :
    /// voie croisée plate. Coupe-bas : −3 dB à 150 Hz.
    #[test]
    fn la_voie_croisee_suit_la_pente_attendue() {
        let fc = 700.0;
        let actif = Params {
            head_shadow_hz: Some(fc),
            phase_guard_ms: None,
            ..params(0.3)
        };
        let a_fc = gain_croise_db(actif, f64::from(fc));
        assert!((a_fc + 3.01).abs() < 0.2, "à fc : {a_fc:.2} dB");
        let bas = gain_croise_db(actif, 100.0);
        assert!(bas > -0.25, "sous la coupure : {bas:.2} dB");
        let a_4 = gain_croise_db(actif, 4.0 * f64::from(fc));
        let a_8 = gain_croise_db(actif, 8.0 * f64::from(fc));
        let pente = a_8 - a_4;
        assert!((pente + 6.0).abs() < 0.5, "pente {pente:.2} dB/octave");

        let inactif = Params {
            phase_guard_ms: None,
            ..params(0.3)
        };
        for f in [100.0, 700.0, 2_800.0, 5_600.0, 12_000.0] {
            let g = gain_croise_db(inactif, f);
            assert!(g.abs() < 0.05, "filtre inactif, {f} Hz : {g:.3} dB");
        }

        let coupe_bas = Params {
            low_cut: true,
            phase_guard_ms: None,
            ..params(0.3)
        };
        let a_150 = gain_croise_db(coupe_bas, 150.0);
        assert!(
            (a_150 + 3.01).abs() < 0.2,
            "coupe-bas à 150 Hz : {a_150:.2}"
        );
        let a_2k = gain_croise_db(coupe_bas, 2_000.0);
        assert!(a_2k > -0.1, "coupe-bas à 2 kHz : {a_2k:.2}");
    }

    /// Garde de phase : R = −L fait tomber le dosage effectif ; un signal
    /// ordinaire (ρ ≥ 0) le laisse intact.
    #[test]
    fn la_garde_de_phase_baisse_le_dosage_en_opposition() {
        let n = FS as usize / 2;
        let sinus = |i: usize| (0.4 * (2.0 * PI * 330.0 * i as f64 / f64::from(FS)).sin()) as f32;

        let mut e = CrossfeedProEngine::new(FS, params(0.3));
        let mut oppose: Vec<f32> = (0..n).flat_map(|i| [sinus(i), -sinus(i)]).collect();
        e.process_interleaved(&mut oppose);
        assert!(e.correlation() < -0.9, "corrélation {}", e.correlation());
        let k = e.effective_amount();
        assert!(k < 0.03, "opposition : dosage effectif {k} (réglé 0,3)");

        let mut e = CrossfeedProEngine::new(FS, params(0.3));
        let mut normal: Vec<f32> = (0..n)
            .flat_map(|i| [sinus(i), 0.5 * sinus(i + 3)])
            .collect();
        e.process_interleaved(&mut normal);
        assert!(e.correlation() > 0.5);
        assert!(
            (e.effective_amount() - 0.3).abs() < 1e-6,
            "signal ordinaire : {}",
            e.effective_amount()
        );
    }

    /// La garde descend sans marche : d'un échantillon à l'autre, le dosage
    /// effectif ne bouge que d'une fraction infime quand un signal passe en
    /// opposition de phase d'un coup.
    #[test]
    fn la_garde_ne_fait_pas_de_marche() {
        let mut e = CrossfeedProEngine::new(FS, params(0.6));
        let sinus = |i: usize| (0.4 * (2.0 * PI * 330.0 * i as f64 / f64::from(FS)).sin()) as f32;
        let mut prec = e.effective_amount();
        let mut pire = 0.0f32;
        for i in 0..FS as usize {
            let r = if i < FS as usize / 4 {
                sinus(i)
            } else {
                -sinus(i)
            };
            let mut t = [sinus(i), r];
            e.process_interleaved(&mut t);
            let k = e.effective_amount();
            pire = pire.max((k - prec).abs());
            prec = k;
        }
        assert!(prec < 0.05, "la garde n'a pas agi : {prec}");
        assert!(pire < 1e-3, "marche de dosage : {pire}");
    }

    /// Les valeurs recopiées de libbs2b, et le niveau qu'elles donnent ici.
    #[test]
    fn les_prereglages_reprennent_libbs2b() {
        let attendu = [
            (Preset::Bs2b, 700.0, 4.5),
            (Preset::ChuMoy, 700.0, 6.0),
            (Preset::JanMeier, 650.0, 9.5),
        ];
        for (p, fc, db) in attendu {
            assert_eq!(p.cut_hz(), fc, "{p:?}");
            assert_eq!(p.feed_db(), db, "{p:?}");
            let k = f64::from(p.amount());
            assert!(
                (MIN_AMOUNT..=MAX_AMOUNT).contains(&p.amount()),
                "{p:?} : k = {k}"
            );
            let niveau = 20.0 * ((1.0 - k) / k).log10();
            assert!((niveau - f64::from(db)).abs() < 1e-3, "{p:?} : {niveau} dB");
        }
        assert_eq!(Preset::default(), Preset::Bs2b);
    }
}
