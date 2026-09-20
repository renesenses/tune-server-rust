//! Parametric equalizer for the Tune Master Profiler.
//!
//! 3-band EQ using biquad filters (Robert Bristow-Johnson Audio EQ Cookbook):
//! - Low shelf (60-80 Hz) — bass weight
//! - Mid peak (1-3 kHz) — voice presence/clarity
//! - High shelf (10-12 kHz) — treble air/brightness
//!
//! Coefficients and filter state are computed in f64. That is a claim about
//! ARITHMETIC precision and nothing else: the biquad accumulators stay far
//! below the noise floor of 16- and 24-bit material, so the filter adds no
//! audible rounding of its own. It is NOT a claim of bit-perfection — an
//! active equalizer modifies every sample, by design, whatever the width of
//! its accumulators. The two properties are independent and Tune must not
//! trade the wording of one for the other (#2213): the signal-path panel
//! already flips `bit_perfect` to false as soon as the EQ alters the signal
//! (`tune-server/src/routes/zones.rs`, `zone_eq_alters_signal`). Disabled — or
//! in PURE mode, where the processor is never built — this stage is a strict
//! identity and the samples pass through untouched.
//!
//! The EQ profile is stored per-zone and applied in the PCM pipeline before
//! output.

use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

/// User-facing EQ profile combining room macro settings + perceptual sliders.
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EqProfile {
    pub enabled: bool,
    /// Macro environment
    pub listening: ListeningMode,
    pub room_size: RoomSize,
    pub speaker_placement: SpeakerPlacement,
    /// Perceptual sliders: -12.0 to +12.0 dB
    pub bass_gain_db: f64,
    pub mid_gain_db: f64,
    pub treble_gain_db: f64,
    /// Expert-mode bands (graphic 10/15/31 or parametric). When non-empty they
    /// REPLACE the 3-tilt cascade above — the two modes are alternative UIs
    /// over the same per-zone profile. `default` keeps every profile persisted
    /// before this field deserializing unchanged.
    #[serde(default)]
    pub bands: Vec<EqBandSpec>,
}

/// One expert-mode filter band (RBJ biquad).
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EqBandSpec {
    /// Center/corner frequency in Hz
    pub freq: f64,
    /// Gain in dB (shelves/peak; ignored by pass/notch)
    #[serde(default)]
    pub gain: f64,
    #[serde(default = "default_band_q")]
    pub q: f64,
    /// "peak" | "low_shelf" | "high_shelf" | "low_pass" | "high_pass" | "notch"
    #[serde(rename = "type", default = "default_band_type")]
    pub band_type: String,
    /// Le canal auquel cette bande s'applique. `None` = TOUS les canaux.
    ///
    /// C'est le defaut, et c'est ce qui rend les preregla­ges existants
    /// inchanges : un profil enregistre avant cette version n'a pas ce champ,
    /// `serde` le laisse a `None`, et la bande s'applique partout — exactement
    /// comme avant.
    ///
    /// `Some(0)` = gauche, `Some(1)` = droite. Une piece dissymetrique — un mur
    /// d'un cote, une ouverture de l'autre — ne se corrige pas avec la meme
    /// courbe des deux cotes : c'est la demande d'Alexander Jam, abonne
    /// Premium, qui venait chercher l'equivalent de ce que fait Roon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<u16>,
}

impl Default for EqBandSpec {
    /// Une bande neutre, sur tous les canaux. Permet aux appelants d'ecrire
    /// `..Default::default()` et de ne plus casser quand une option s'ajoute —
    /// c'est exactement ce qui vient d'arriver avec `channel`.
    fn default() -> Self {
        Self {
            freq: 1000.0,
            gain: 0.0,
            q: default_band_q(),
            band_type: default_band_type(),
            channel: None,
        }
    }
}

impl EqBandSpec {
    /// Cette bande agit-elle sur ce canal ?
    fn vise_le_canal(&self, ch: u16) -> bool {
        match self.channel {
            None => true,
            Some(c) => c == ch,
        }
    }

    /// Cette bande est-elle une bande **à gain** — celle dont [`Self::coeffs`]
    /// fait un plateau ou une cloche, et dont le champ `gain` pousse vraiment
    /// le signal ?
    ///
    /// La réponse est une LISTE NOIRE, pas une liste blanche, et c'est le
    /// fond du sujet (#4594). `coeffs` range **tout type inconnu** en
    /// `peaking_eq` — c'est son bras `_ =>`. Un `"peaking"`, un `"Peak"`, un
    /// `"bell"` — la casse et les synonymes que `POST /zones/{id}/eq` accepte
    /// sans les valider, `band_type` n'étant qu'un `String` — est donc monté
    /// par [`EqProcessor::new`] en cloche qui POUSSE de son gain.
    ///
    /// La somme des gains positifs, elle, demandait
    /// `matches!(.., "peak" | "low_shelf" | "high_shelf")` : elle répondait
    /// « non » pour ces types-là et les comptait pour zéro. Et comme le terme
    /// L1 de [`EqProfile::automatic_headroom_db_at`] n'entre en jeu que
    /// lorsqu'au moins une bande pousse, la borne vraie s'éteignait avec elle :
    /// la réserve ENTIÈRE tombait à 0 dB. Mesuré sur dix cloches à +6 dB, un
    /// sinus 1 kHz à −6 dBFS ressortait à +4,77 dBFS avec **60,8 %**
    /// d'échantillons écrêtés dur, là où les mêmes bandes en `"peak"`
    /// réservaient 60 dB et n'écrêtaient rien.
    ///
    /// Poser la question ICI, une seule fois, est ce qui empêche la somme et
    /// la cascade de diverger à nouveau. Témoin :
    /// `un_type_de_bande_non_canonique_reserve_comme_une_cloche_4594`.
    fn est_une_bande_a_gain(&self) -> bool {
        !matches!(self.band_type.as_str(), "low_pass" | "high_pass" | "notch")
    }

    /// Ce que cette bande demande de réserver pour sa RÉSONANCE, en dB (≥ 0).
    ///
    /// Un `low_pass` / `high_pass` RBJ vaut |H(fc)| = Q : au-dessus de
    /// Q = 1/√2 il POUSSE, sans qu'aucun champ `gain` ne le dise. La réserve
    /// est `20·log10(Q/0,707)` — zéro à Butterworth et au-dessous, 15,05 dB à
    /// Q = 4. Les autres types rendent zéro : leur boost, quand il existe, est
    /// dans leur gain.
    fn reserve_de_resonance_db(&self) -> f64 {
        if !matches!(self.band_type.as_str(), "low_pass" | "high_pass") || !self.q.is_finite() {
            return 0.0;
        }
        // Même bornage que `coeffs` : la réserve parle du filtre RÉELLEMENT
        // construit, pas de la valeur brute du profil.
        let q = self.q.clamp(0.1, 30.0);
        if q <= Q_SANS_RESONANCE {
            return 0.0;
        }
        20.0 * (q / Q_SANS_RESONANCE).log10()
    }
}

/// Le débit auquel répond [`EqProfile::automatic_headroom_db`] quand
/// l'appelant n'en a pas.
///
/// 44 100 Hz est déjà la sonde du panneau signal-path
/// (`zone_eq_step_description`) et de l'import AutoEq : la valeur affichée et
/// la valeur appliquée parlent donc du même filtre sur toute source CD.
pub const DEBIT_DE_REFERENCE_HZ: f64 = 44_100.0;

/// Le Q d'un `pass` de Butterworth : au-dessous, la réponse ne dépasse jamais
/// l'unité et rien n'est à réserver.
const Q_SANS_RESONANCE: f64 = std::f64::consts::FRAC_1_SQRT_2;

/// Ce qu'on ajoute à la norme L1 pour ne pas réserver la borne au ras.
///
/// Depuis #4594 la L1 n'est plus doublée par la somme des gains : elle EST la
/// réserve, et elle est **atteinte** — le signal `x[n] = signe(h[−n])` sort
/// exactement à 1,000000. Le compteur d'écrêtage de Tune teste
/// `!(-1.0..1.0).contains(&s)` : l'égalité compte comme un over, et un
/// échantillon pile au rail n'a de toute façon plus rien devant lui.
///
/// 0,01 dB — un facteur 0,998849 — écarte le rail sans s'entendre (le pas de
/// volume le plus fin de l'interface vaut 0,5 dB), et couvre du même coup la
/// queue que [`norme_l1`] tronque : celle-ci pèse moins de 10⁻⁴ dB, mille fois
/// moins. Mesuré : crête 0,998849 et zéro over sur le signal adverse, contre
/// 1,000000 et un over sans cette marge.
const MARGE_DE_TRONCATURE_DB: f64 = 0.01;

/// Bornes de la réponse impulsionnelle sommée par [`norme_l1`].
const LONGUEUR_L1_MIN: usize = 4_096;
const LONGUEUR_L1_MAX: usize = 1 << 19;

fn default_band_q() -> f64 {
    1.0
}

fn default_band_type() -> String {
    "peak".into()
}

impl EqBandSpec {
    /// A band that would leave the signal untouched (flat peak/shelf).
    fn is_neutral(&self) -> bool {
        matches!(self.band_type.as_str(), "peak" | "low_shelf" | "high_shelf")
            && self.gain.abs() < 0.01
    }

    fn coeffs(&self, sample_rate: f64) -> BiquadCoeffs {
        // Clamp to sane, stable ranges (mirrors the /eq router validation).
        let freq = self.freq.clamp(10.0, sample_rate * 0.45);
        let q = self.q.clamp(0.1, 30.0);
        let gain = self.gain.clamp(-24.0, 24.0);
        match self.band_type.as_str() {
            "low_shelf" => low_shelf(freq, gain, sample_rate),
            "high_shelf" => high_shelf(freq, gain, sample_rate),
            "low_pass" => low_pass(freq, q, sample_rate),
            "high_pass" => high_pass(freq, q, sample_rate),
            "notch" => notch(freq, q, sample_rate),
            _ => peaking_eq(freq, gain, q, sample_rate),
        }
    }
}

impl Default for EqProfile {
    fn default() -> Self {
        Self {
            enabled: false,
            listening: ListeningMode::Speakers,
            room_size: RoomSize::Medium,
            speaker_placement: SpeakerPlacement::FreeStanding,
            bass_gain_db: 0.0,
            mid_gain_db: 0.0,
            treble_gain_db: 0.0,
            bands: Vec::new(),
        }
    }
}

#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ListeningMode {
    Headphones,
    Speakers,
}

#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RoomSize {
    Small,
    Medium,
    Large,
}

#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SpeakerPlacement {
    NearWall,
    FreeStanding,
}

impl EqProfile {
    /// Compute the effective gain for each band, combining the environment
    /// tone preset with the user's perceptual adjustments.
    pub fn effective_gains(&self) -> (f64, f64, f64) {
        let (base_bass, base_mid, base_treble) = self.environment_tone_preset();
        (
            base_bass + self.bass_gain_db,
            base_mid + self.mid_gain_db,
            base_treble + self.treble_gain_db,
        )
    }

    /// Réserve automatique d'un canal, au débit de référence.
    ///
    /// Le débit entre dans le calcul (voir [`Self::automatic_headroom_db_at`]) :
    /// cette porte-ci répond pour [`DEBIT_DE_REFERENCE_HZ`], qui est déjà la
    /// sonde du panneau signal-path et de l'import AutoEq. Le chemin audio,
    /// lui, réserve au débit RÉEL de la zone.
    pub fn automatic_headroom_db(&self, channel: u16) -> f64 {
        self.automatic_headroom_db_at(channel, DEBIT_DE_REFERENCE_HZ)
    }

    /// Réserve automatique d'un canal, en dB (négative ou nulle), au débit donné.
    ///
    /// **Deux** termes, et seulement deux — chacun répond à un défaut mesuré
    /// (#4073, #4594, `docs/mesures/2218-marge-ecretage-crete-vraie.md`) :
    ///
    /// 1. **La norme L1 de la cascade à gain**, en dB. `max|y| ≤ ‖h‖₁·max|x|`
    ///    est la borne VRAIE pour une entrée bornée quelconque — et c'est une
    ///    réponse en TEMPS, pas en fréquence : elle voit la sonnerie d'un
    ///    plateau sur un front, que le maximum de la réponse en fréquence ne
    ///    voit pas. Un plateau grave de +6 dB a un maximum fréquentiel de
    ///    6,000 dB et une norme L1 de 6,505 dB ; sur un carré, 40 % des
    ///    échantillons sortaient du rail avec la première valeur. Réserver la
    ///    L1 rend l'écrêtage **impossible** : l'entrée PCM est bornée à 1,0,
    ///    donc `|y| ≤ ‖h‖₁ · (1/‖h‖₁) = 1`.
    /// 2. **La résonance des `low_pass` / `high_pass`**, `20·log10(Q/0,707)`
    ///    pour Q > 0,707, zéro sinon. Un passe-bas RBJ vaut |H(fc)| = Q :
    ///    à Q = 4 il pousse de 12,04 dB et la réserve valait… 0 dB, pour 83,7 %
    ///    d'overs écrêtés dur. La formule rend 15,05 dB, soit le maximum
    ///    fréquentiel EXACT (Q/√(1−1/4Q²) = 12,11 dB) plus une marge qui couvre
    ///    aussi la norme L1 du même filtre (14,19 dB).
    ///
    /// ## Ce que la somme des gains positifs faisait ici, et pourquoi elle en est partie
    ///
    /// Jusqu'à #4594, un troisième terme entrait en `max()` avec la L1 : la
    /// **somme des gains positifs** des bandes à gain (d423c16b). Elle majore
    /// bien le maximum de la réponse en fréquence — les biquads se cascadent,
    /// leurs gains s'additionnent en dB — mais très grossièrement dès qu'il y a
    /// plusieurs bandes, parce qu'elle suppose que toutes poussent à la même
    /// fréquence. Sur un égaliseur graphique, où les bandes sont disjointes par
    /// construction, elle domine systématiquement la borne vraie et atténue
    /// pour rien. Mesuré :
    ///
    /// ```text
    /// préréglage      somme (appliquée)   L1 (borne vraie)   max en fréquence
    /// bass_boost           −20,00 dB          −13,40 dB          +10,29 dB
    /// rock                 −28,00 dB          −13,64 dB           +7,64 dB
    /// loudness             −27,00 dB          −13,73 dB           +7,24 dB
    /// dix cloches à +6 dB  −60,00 dB          −16,53 dB          +10,85 dB
    /// ```
    ///
    /// Choisir « Rock » en un geste coûtait 28 dB pour un besoin de 13,64 : le
    /// symptôme que les testeurs décrivent en « pas de son quand j'active
    /// l'égaliseur » (#1640), « Egaliseur » (#3479) et « il baisse le volume au
    /// minimum » (#4407). La L1 seule reste une borne vraie — **strictement**
    /// plus sûre que le maximum fréquentiel, qu'elle majore toujours — et rend
    /// le niveau. Arbitré par Bertrand le 20/09 pour la v0.9.160.
    ///
    /// **Ce qui n'est volontairement PAS réservé** : la norme L1 des filtres
    /// `pass` et `notch` eux-mêmes. Un passe-haut de Butterworth (Q = 0,707,
    /// le filtre anti-rumble ordinaire) a un maximum fréquentiel de 0 dB et une
    /// norme L1 de **7,02 dB** : la couvrir coûterait 7 dB de niveau à tout
    /// utilisateur d'un coupe-bas, pour un dépassement qui ne se produit que
    /// sur un signal adverse. Une réserve trop large abîme le son autant
    /// qu'une réserve trop courte ; cette ligne-là est tracée ici, et elle est
    /// témoignée (`marge_et_crete_2218.rs`).
    ///
    /// Même raison pour la **cascade qui ne pousse nulle part** : une bande qui
    /// ne fait que creuser sonne elle aussi, sa norme L1 dépasse l'unité, et
    /// réserver là-dessus atténuerait un profil purement soustractif — ce que
    /// personne ne demande et que `un_profil_uniquement_attenuateur_ne_reserve_aucune_marge`
    /// interdit. Le terme L1 ne s'applique donc que lorsqu'au moins une bande
    /// pousse — c'est le booléen que rend [`Self::cascade_a_gain`], et c'est
    /// désormais son seul rôle : il ouvre la porte, il ne fixe plus la valeur.
    ///
    /// Cette porte-là a été **remesurée** en passant à la borne vraie (#4594),
    /// parce qu'elle laisse passer un écrêtage réel : le préréglage `classical`
    /// (que des creux) a une norme L1 de 2,171 dB et sort 16 864 échantillons
    /// du rail sur un carré pleine échelle. La tentation était de fermer la
    /// porte. Ce que coûterait sa fermeture a été mesuré avant d'y toucher :
    ///
    /// ```text
    /// profil PUREMENT soustractif           L1 qu'il faudrait réserver
    /// classical (livré)                          2,171 dB
    /// une cloche −3 dB Q=1                       2,544 dB
    /// curseurs −12/−12/−12                       5,294 dB
    /// plateau grave −12 dB                       5,516 dB
    /// une cloche −24 dB Q=1                      6,957 dB
    /// dix cloches −24 dB Q=10                   13,346 dB
    /// ```
    ///
    /// Jusqu'à **13,3 dB retirés à un profil qui ne fait que creuser** : c'est
    /// exactement le défaut qu'on vient de corriger ailleurs, réintroduit par
    /// l'autre bout. La porte reste donc ouverte, pour la même raison, chiffrée,
    /// que la norme L1 des `pass` reste hors réserve. Un profil sans aucun
    /// gain positif peut dépasser le rail sur un signal adverse ; c'était vrai
    /// avant #4594 et ça l'est encore, à l'échantillon près.
    pub fn automatic_headroom_db_at(&self, channel: u16, sample_rate: f64) -> f64 {
        let (au_moins_une_bande_pousse, cascade) = self.cascade_a_gain(channel, sample_rate);
        let l1_db = if au_moins_une_bande_pousse {
            let l1 = norme_l1(&cascade);
            if l1 > 1.0 {
                20.0 * l1.log10() + MARGE_DE_TRONCATURE_DB
            } else {
                0.0
            }
        } else {
            0.0
        };
        let resonance_db: f64 = if self.bands.is_empty() {
            0.0
        } else {
            self.bands
                .iter()
                .filter(|band| band.vise_le_canal(channel))
                .map(EqBandSpec::reserve_de_resonance_db)
                .sum()
        };
        -(l1_db + resonance_db)
    }

    /// Au moins une bande POUSSE-t-elle, et la cascade des bandes à GAIN
    /// (`peak` / `low_shelf` / `high_shelf`, et tout type inconnu — que
    /// [`EqBandSpec::coeffs`] traite en `peaking_eq`), exactement celle que
    /// construit [`EqProcessor::new`] pour ce canal, amputée des `pass` et des
    /// `notch` dont la norme L1 n'est pas réservée (voir
    /// [`Self::automatic_headroom_db_at`]).
    ///
    /// Les deux réponses viennent de la MÊME porte,
    /// [`EqBandSpec::est_une_bande_a_gain`]. Elles ne le faisaient pas : la
    /// somme filtrait par liste blanche et la cascade par liste noire, si bien
    /// qu'un type non canonique poussait sans rien réserver (#4594).
    ///
    /// Le premier membre était la SOMME des gains positifs, et elle servait de
    /// réserve. Depuis #4594 elle n'est plus qu'un **booléen** — « au moins une
    /// bande pousse-t-elle ? » — qui ouvre la porte du terme L1 : la valeur,
    /// elle, vient entièrement de la borne vraie. Une somme et une cascade,
    /// c'étaient deux réponses à la même question, et celle qui gagnait le
    /// `max()` n'était pas celle qui mesure.
    fn cascade_a_gain(&self, channel: u16, sample_rate: f64) -> (bool, Vec<BiquadCoeffs>) {
        if self.bands.is_empty() {
            let (bass, mid, treble) = self.effective_gains();
            let pousse = [bass, mid, treble]
                .into_iter()
                .any(|gain| gain.is_finite() && gain > 0.0);
            let utilisable = [bass, mid, treble].iter().all(|g| g.is_finite());
            let actif = bass.abs() > 0.01 || mid.abs() > 0.01 || treble.abs() > 0.01;
            let cascade = if utilisable && actif && sample_rate.is_finite() && sample_rate > 0.0 {
                vec![
                    low_shelf(80.0, bass, sample_rate),
                    peaking_eq(2000.0, mid, 1.0, sample_rate),
                    high_shelf(10000.0, treble, sample_rate),
                ]
            } else {
                Vec::new()
            };
            return (pousse, cascade);
        }

        let pousse = self.bands.iter().any(|band| {
            band.vise_le_canal(channel)
                && band.est_une_bande_a_gain()
                && band.gain.is_finite()
                && band.gain > 0.0
        });

        let cascade = if sample_rate.is_finite() && sample_rate > 0.0 {
            self.bands
                .iter()
                .filter(|band| {
                    band.vise_le_canal(channel)
                        && !band.is_neutral()
                        && band.est_une_bande_a_gain()
                        && band.freq.is_finite()
                        && band.gain.is_finite()
                        && band.q.is_finite()
                })
                .map(|band| band.coeffs(sample_rate))
                .collect()
        } else {
            Vec::new()
        };
        (pousse, cascade)
    }

    /// Tone preset for the DECLARED listening environment.
    /// Returns (bass_db, mid_db, treble_db) offsets.
    ///
    /// Six hard-coded tilts, chosen from two enums the listener picks in a
    /// three-question wizard — room size and speaker placement — plus a
    /// headphone case. **Nothing is measured here.** No microphone, no sweep,
    /// no impulse response, no frequency response of the room or of the
    /// speakers ever reaches this function; two rooms answering the same three
    /// questions get the same three numbers.
    ///
    /// The name this function carried until #2213 promised something the code
    /// does not do. In audiophile usage, "room correction" means a filter
    /// DERIVED FROM AN ACOUSTIC MEASUREMENT — which Tune does offer,
    /// elsewhere: `crate::room_correction` stores a `RoomProfile` with its
    /// `measurement_data`, and `crate::audio::convolver` convolves a WAV
    /// impulse response exported from REW, Acourate or Audiolense. Those
    /// deserve the term; this table does not.
    ///
    /// What it is worth is stated plainly: a sane starting tilt for a stated
    /// environment, to be finished by ear with the three sliders.
    fn environment_tone_preset(&self) -> (f64, f64, f64) {
        if self.listening == ListeningMode::Headphones {
            // Headphones: slight bass boost for missing physical impact,
            // slight treble rolloff for reduced fatigue
            return (1.5, 0.0, -1.0);
        }

        match (self.room_size, self.speaker_placement) {
            // Small room + near wall: strong bass buildup, reduce bass
            (RoomSize::Small, SpeakerPlacement::NearWall) => (-4.0, 0.5, 0.0),
            // Small room + free standing: moderate bass buildup
            (RoomSize::Small, SpeakerPlacement::FreeStanding) => (-2.0, 0.0, 0.5),
            // Medium room + near wall: some bass buildup
            (RoomSize::Medium, SpeakerPlacement::NearWall) => (-2.5, 0.0, 0.0),
            // Medium room + free standing: neutral (reference)
            (RoomSize::Medium, SpeakerPlacement::FreeStanding) => (0.0, 0.0, 0.0),
            // Large room + near wall: slight bass buildup, treble loss
            (RoomSize::Large, SpeakerPlacement::NearWall) => (-1.5, 0.0, 1.0),
            // Large room + free standing: bass rolls off, compensate
            (RoomSize::Large, SpeakerPlacement::FreeStanding) => (1.5, 0.0, 1.5),
        }
    }
}

/// Biquad filter coefficients (Direct Form I).
#[derive(Debug, Clone, Copy)]
struct BiquadCoeffs {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

/// Biquad filter state (per channel).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct BiquadState {
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl BiquadState {
    fn process(&mut self, c: &BiquadCoeffs, x: f64) -> f64 {
        let y = c.b0 * x + c.b1 * self.x1 + c.b2 * self.x2 - c.a1 * self.y1 - c.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Norme L1 de la réponse impulsionnelle d'une cascade de biquads.
///
/// `Σ|h[n]|` : le gain le plus grand que cette cascade puisse donner à une
/// entrée bornée, `max|y| ≤ ‖h‖₁·max|x|`, atteint par `x[n] = signe(h[−n])`.
/// C'est une réponse en TEMPS : elle voit ce que le maximum de la réponse en
/// fréquence ne voit pas — la sonnerie d'un plateau sur un front, qui est
/// exactement le défaut de #4073.
///
/// La somme est tronquée, jamais infinie : la longueur vient du pôle le plus
/// lent de la cascade (|p|² = a₂ pour une paire conjuguée), assez loin pour
/// que l'enveloppe soit tombée à 10⁻⁹, bornée à [`LONGUEUR_L1_MIN`] …
/// [`LONGUEUR_L1_MAX`]. Une troncature ne peut que SOUS-estimer — et depuis
/// #4594 cette valeur n'est plus doublée par la somme des gains : elle EST la
/// réserve. La borne de longueur est donc la garantie elle-même, et c'est
/// pourquoi elle vise 10⁻⁹ et non un compte rond : la queue laissée dehors
/// pèse moins que le LSB d'un 32 bits. Ce que la troncature laisse dehors est
/// témoigné là où il se verrait — en sortie :
/// `aucun_prereglage_livre_n_ecrete_meme_sur_le_signal_adverse_4594` injecte
/// le signal `x[n] = signe(h[−n])` qui ATTEINT la borne, et vérifie qu'aucun
/// échantillon ne sort du rail. Rendue en linéaire ; une cascade vide vaut
/// 1,0 (0 dB).
fn norme_l1(cascade: &[BiquadCoeffs]) -> f64 {
    if cascade.is_empty() {
        return 1.0;
    }
    let rayon = cascade
        .iter()
        .map(|c| c.a2.abs().sqrt())
        .fold(0.0_f64, f64::max);
    if !rayon.is_finite() {
        return 1.0;
    }
    let rayon = rayon.min(0.999_999);
    let longueur = if rayon <= f64::EPSILON {
        LONGUEUR_L1_MIN
    } else {
        let brut = 1e-9_f64.ln() / rayon.ln();
        if brut.is_finite() && brut >= 0.0 {
            (brut.ceil() as usize).clamp(LONGUEUR_L1_MIN, LONGUEUR_L1_MAX)
        } else {
            LONGUEUR_L1_MAX
        }
    };

    let mut etats = vec![BiquadState::default(); cascade.len()];
    let mut somme = 0.0_f64;
    for n in 0..longueur {
        let mut v = if n == 0 { 1.0 } else { 0.0 };
        for (coeffs, etat) in cascade.iter().zip(etats.iter_mut()) {
            v = etat.process(coeffs, v);
        }
        somme += v.abs();
    }
    if somme.is_finite() { somme } else { 1.0 }
}

/// Design a low-shelf biquad filter.
fn low_shelf(freq: f64, gain_db: f64, sample_rate: f64) -> BiquadCoeffs {
    let a = 10.0_f64.powf(gain_db / 40.0);
    let w0 = 2.0 * PI * freq / sample_rate;
    let cos_w0 = w0.cos();
    let sin_w0 = w0.sin();
    let alpha = sin_w0 / 2.0 * (2.0_f64).sqrt();
    let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;

    let a0 = (a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha;
    BiquadCoeffs {
        b0: (a * ((a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha)) / a0,
        b1: (2.0 * a * ((a - 1.0) - (a + 1.0) * cos_w0)) / a0,
        b2: (a * ((a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha)) / a0,
        a1: (-2.0 * ((a - 1.0) + (a + 1.0) * cos_w0)) / a0,
        a2: ((a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha) / a0,
    }
}

/// Design a peaking EQ biquad filter.
fn peaking_eq(freq: f64, gain_db: f64, q: f64, sample_rate: f64) -> BiquadCoeffs {
    let a = 10.0_f64.powf(gain_db / 40.0);
    let w0 = 2.0 * PI * freq / sample_rate;
    let cos_w0 = w0.cos();
    let sin_w0 = w0.sin();
    let alpha = sin_w0 / (2.0 * q);

    let a0 = 1.0 + alpha / a;
    BiquadCoeffs {
        b0: (1.0 + alpha * a) / a0,
        b1: (-2.0 * cos_w0) / a0,
        b2: (1.0 - alpha * a) / a0,
        a1: (-2.0 * cos_w0) / a0,
        a2: (1.0 - alpha / a) / a0,
    }
}

/// Design a high-shelf biquad filter.
fn high_shelf(freq: f64, gain_db: f64, sample_rate: f64) -> BiquadCoeffs {
    let a = 10.0_f64.powf(gain_db / 40.0);
    let w0 = 2.0 * PI * freq / sample_rate;
    let cos_w0 = w0.cos();
    let sin_w0 = w0.sin();
    let alpha = sin_w0 / 2.0 * (2.0_f64).sqrt();
    let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;

    let a0 = (a + 1.0) - (a - 1.0) * cos_w0 + two_sqrt_a_alpha;
    BiquadCoeffs {
        b0: (a * ((a + 1.0) + (a - 1.0) * cos_w0 + two_sqrt_a_alpha)) / a0,
        b1: (-2.0 * a * ((a - 1.0) + (a + 1.0) * cos_w0)) / a0,
        b2: (a * ((a + 1.0) + (a - 1.0) * cos_w0 - two_sqrt_a_alpha)) / a0,
        a1: (2.0 * ((a - 1.0) - (a + 1.0) * cos_w0)) / a0,
        a2: ((a + 1.0) - (a - 1.0) * cos_w0 - two_sqrt_a_alpha) / a0,
    }
}

/// Design a low-pass biquad filter (RBJ cookbook).
fn low_pass(freq: f64, q: f64, sample_rate: f64) -> BiquadCoeffs {
    let w0 = 2.0 * PI * freq / sample_rate;
    let cos_w0 = w0.cos();
    let alpha = w0.sin() / (2.0 * q);
    let a0 = 1.0 + alpha;
    BiquadCoeffs {
        b0: ((1.0 - cos_w0) / 2.0) / a0,
        b1: (1.0 - cos_w0) / a0,
        b2: ((1.0 - cos_w0) / 2.0) / a0,
        a1: (-2.0 * cos_w0) / a0,
        a2: (1.0 - alpha) / a0,
    }
}

/// Design a high-pass biquad filter (RBJ cookbook).
fn high_pass(freq: f64, q: f64, sample_rate: f64) -> BiquadCoeffs {
    let w0 = 2.0 * PI * freq / sample_rate;
    let cos_w0 = w0.cos();
    let alpha = w0.sin() / (2.0 * q);
    let a0 = 1.0 + alpha;
    BiquadCoeffs {
        b0: ((1.0 + cos_w0) / 2.0) / a0,
        b1: (-(1.0 + cos_w0)) / a0,
        b2: ((1.0 + cos_w0) / 2.0) / a0,
        a1: (-2.0 * cos_w0) / a0,
        a2: (1.0 - alpha) / a0,
    }
}

/// Design a notch biquad filter (RBJ cookbook).
fn notch(freq: f64, q: f64, sample_rate: f64) -> BiquadCoeffs {
    let w0 = 2.0 * PI * freq / sample_rate;
    let cos_w0 = w0.cos();
    let alpha = w0.sin() / (2.0 * q);
    let a0 = 1.0 + alpha;
    BiquadCoeffs {
        b0: 1.0 / a0,
        b1: (-2.0 * cos_w0) / a0,
        b2: 1.0 / a0,
        a1: (-2.0 * cos_w0) / a0,
        a2: (1.0 - alpha) / a0,
    }
}

/// 3-band parametric EQ processor.
///
/// Processes interleaved PCM samples in-place. Supports any bit depth
/// (samples are converted to/from f64 internally).
pub struct EqProcessor {
    /// Cascade biquad PAR CANAL : `[canal][etage]`.
    ///
    /// Elle etait partagee — les memes coefficients pour tous les canaux. Une
    /// bande peut desormais viser un canal (#Alexander Jam), et gauche et
    /// droite n'ont donc plus forcement la meme courbe.
    filters: Vec<Vec<BiquadCoeffs>>,
    /// Per-channel state for each cascade stage: [channel][stage]
    states: Vec<Vec<BiquadState>>,
    /// Automatic pre-gain per channel, applied before the cascade.
    preamp_gains: Vec<f64>,
    preamp_db: Vec<f64>,
    /// Une suite de dither TPDF **indépendante par canal**, déterministe.
    /// L'implémentation vient de [`tune_plugin_audio_support::dither`] — partagée avec
    /// ReplayGain, le mélangeur et la réduction de profondeur (#4075, #4076) ;
    /// l'égaliseur n'en garde plus de copie, seulement son état par canal.
    dither_states: Vec<tune_plugin_audio_support::dither::Dither>,
    /// Cumulative runtime diagnostics since this processor was built for the
    /// current stream. Read by the output telemetry path (#2212).
    process_stats: EqProcessStats,
    /// #2218 (T9, défaut B) — les `overs` étaient comptés, jamais dits. Ce
    /// compteur suit `process_stats.overs` là où il est incrémenté, avec
    /// l'excès et l'index du premier, et porte les DEUX lignes `dsp_ecretage`
    /// de la piste : `ecretage_premier_dit` après le premier bloc qui écrête,
    /// la fin dans `Drop`. `ecretage_fin_dite` est atomique parce que
    /// `inherit_state_from` doit faire taire un processeur qu'elle ne tient
    /// que par `&` : relayé, il ne clôt pas une piste qui continue.
    ecretage: tune_plugin_audio_support::ecretage::CompteurDEcretage,
    ecretage_premier_dit: bool,
    ecretage_fin_dite: std::sync::atomic::AtomicBool,
    channels: u16,
    enabled: bool,
}

/// Diagnostics for one processed audio buffer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EqProcessStats {
    /// Samples outside the nominal -1..1 output range before quantization.
    pub overs: u64,
    /// Invalid samples replaced with silence rather than propagated.
    pub non_finite_samples: u64,
}

impl EqProcessor {
    /// Create a new EQ processor from a profile and sample rate.
    pub fn new(profile: &EqProfile, sample_rate: u32, channels: u16) -> Self {
        let sr = sample_rate as f64;

        // Expert-mode bands take over the whole cascade when present; the
        // 3-tilt profiler cascade is the fallback (unchanged behaviour).
        // La cascade du profileur historique (3 filtres de tilt) ne connait pas
        // les canaux : elle s'applique partout, a l'identique. Seules les
        // bandes du mode expert peuvent viser un canal.
        let commune: Vec<BiquadCoeffs> = if profile.bands.is_empty() {
            let (bass_db, mid_db, treble_db) = profile.effective_gains();
            if bass_db.abs() > 0.01 || mid_db.abs() > 0.01 || treble_db.abs() > 0.01 {
                vec![
                    low_shelf(80.0, bass_db, sr),
                    peaking_eq(2000.0, mid_db, 1.0, sr),
                    high_shelf(10000.0, treble_db, sr),
                ]
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        let filters: Vec<Vec<BiquadCoeffs>> = (0..channels.max(1))
            .map(|ch| {
                if profile.bands.is_empty() {
                    commune.clone()
                } else {
                    profile
                        .bands
                        .iter()
                        .filter(|b| !b.is_neutral() && b.vise_le_canal(ch))
                        .map(|b| b.coeffs(sr))
                        .collect()
                }
            })
            .collect();

        let states = filters
            .iter()
            .map(|f| vec![BiquadState::default(); f.len()])
            .collect();
        // Au débit RÉEL : la norme L1 d'un plateau et la place d'un aigu sous
        // Nyquist ne sont pas les mêmes à 44,1 et à 192 kHz.
        let preamp_db: Vec<f64> = (0..channels.max(1))
            .map(|ch| profile.automatic_headroom_db_at(ch, sr))
            .collect();
        let preamp_gains = preamp_db
            .iter()
            .map(|db| 10.0_f64.powf(db / 20.0))
            .collect();
        // Un profil dont TOUTES les bandes sont neutres, ou qui ne vise aucun
        // canal existant, ne doit pas rester « actif » a ne rien faire.
        let enabled = profile.enabled && filters.iter().any(|f| !f.is_empty());

        Self {
            filters,
            states,
            preamp_gains,
            preamp_db,
            dither_states: (0..channels.max(1))
                .map(|channel| {
                    tune_plugin_audio_support::dither::Dither::depuis_graine(
                        0x9e37_79b9_7f4a_7c15_u64 ^ (u64::from(channel) + 1),
                    )
                })
                .collect(),
            process_stats: EqProcessStats::default(),
            ecretage: tune_plugin_audio_support::ecretage::CompteurDEcretage::default(),
            ecretage_premier_dit: false,
            ecretage_fin_dite: std::sync::atomic::AtomicBool::new(false),
            channels,
            enabled,
        }
    }

    /// Reset history for a new stream/seek without reallocating filters.
    /// Called by the producer, not by the device callback (telemetry may log).
    pub fn reset_history(&mut self) {
        self.close_telemetry();
        for states in &mut self.states {
            states.fill(BiquadState::default());
        }
        for (channel, dither) in self.dither_states.iter_mut().enumerate() {
            *dither = tune_plugin_audio_support::dither::Dither::depuis_graine(
                0x9e37_79b9_7f4a_7c15_u64 ^ (channel as u64 + 1),
            );
        }
        self.process_stats = EqProcessStats::default();
        self.ecretage = Default::default();
        self.ecretage_premier_dit = false;
        self.ecretage_fin_dite
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }

    /// #2218 — le compteur d'écrêtage de la piste : `echantillons_ecretes`
    /// vaut exactement `process_stats().overs`, avec en plus l'excès maximal,
    /// la crête et l'index du premier.
    pub fn ecretage(&self) -> tune_plugin_audio_support::ecretage::CompteurDEcretage {
        self.ecretage
    }

    /// Après un bloc : cumule le delta dans le registre du processus et dit
    /// le premier écrêtage de la piste UNE fois. Jamais dans la boucle
    /// d'échantillons.
    fn apres_le_bloc(&mut self, avant: &tune_plugin_audio_support::ecretage::CompteurDEcretage) {
        tune_plugin_audio_support::ecretage::REGISTRE
            .egaliseur
            .absorber(avant, &self.ecretage);
        if !self.ecretage_premier_dit && self.ecretage.echantillons_ecretes > 0 {
            self.ecretage_premier_dit = true;
            tune_plugin_audio_support::ecretage::dire_premier(
                tune_plugin_audio_support::ecretage::EtageEcretant::Egaliseur,
                tune_plugin_audio_support::ecretage::Portee::Piste,
                &self.ecretage,
            );
        }
    }

    /// Process interleaved PCM bytes in-place.
    /// `bit_depth`: 16, 24, or 32.
    pub fn process_pcm(&mut self, pcm: &mut [u8], bit_depth: u16) -> EqProcessStats {
        let mut stats = EqProcessStats::default();
        if !self.enabled || pcm.is_empty() || self.channels == 0 {
            return stats;
        }

        let bytes_per_sample = (bit_depth / 8) as usize;
        let frame_size = bytes_per_sample * self.channels as usize;
        // #2218 — l'excès en LSB de la profondeur traitée ; `base` place le
        // premier écrêtage dans la piste, pas dans le bloc.
        let max_val = (1i64 << (bit_depth - 1)) as f64;
        let avant = self.ecretage;
        let base = avant.echantillons_vus;
        let canaux = self.channels as u64;

        for (fi, frame) in pcm.chunks_exact_mut(frame_size).enumerate() {
            for ch in 0..self.channels as usize {
                let offset = ch * bytes_per_sample;
                let sample = read_sample_f64(&frame[offset..], bytes_per_sample, bit_depth)
                    * self.preamp_gains[ch];

                let state = &mut self.states[ch];
                let cascade = &self.filters[ch];
                let mut s = sample;
                for (stage, coeffs) in state.iter_mut().zip(cascade.iter()) {
                    s = stage.process(coeffs, s);
                }

                if !s.is_finite() {
                    stats.non_finite_samples += 1;
                    s = 0.0;
                } else if !(-1.0..1.0).contains(&s) {
                    stats.overs += 1;
                    self.ecretage.noter_ecrete(
                        base + fi as u64 * canaux + ch as u64,
                        (s.abs() - 1.0) * max_val,
                        s.abs(),
                    );
                }

                let dither = self.dither_states[ch].tirer();
                write_sample_f64(&mut frame[offset..], s, bytes_per_sample, bit_depth, dither);
            }
        }
        self.ecretage
            .noter_vus((pcm.len() / frame_size) as u64 * canaux);

        self.process_stats.overs = self.process_stats.overs.saturating_add(stats.overs);
        self.process_stats.non_finite_samples = self
            .process_stats
            .non_finite_samples
            .saturating_add(stats.non_finite_samples);
        self.apres_le_bloc(&avant);
        stats
    }

    /// Process an **interleaved f32** buffer (`[L0, R0, L1, R1, …]`, normalised
    /// to -1..1) in place.
    ///
    /// Same cascade and same per-channel state as
    /// [`Self::process_pcm`] — only the sample representation differs. The
    /// local output (`outputs/local.rs`) already holds its audio as f32 for
    /// cpal and runs its convolver / crossfeed on that buffer; going through
    /// the byte-oriented `process_pcm` there would mean packing to PCM and back
    /// on every chunk, in the audio hot path.
    ///
    /// A buffer that is not a whole number of frames is left untouched rather
    /// than processed half-way: a partial frame would advance the per-channel
    /// filter states out of step and every later chunk would be filtered with
    /// the wrong channel's history.
    pub fn process_interleaved(&mut self, samples: &mut [f32]) -> EqProcessStats {
        let mut stats = EqProcessStats::default();
        if !self.enabled || samples.is_empty() || self.channels == 0 {
            return stats;
        }
        let ch_count = self.channels as usize;
        if !samples.len().is_multiple_of(ch_count) {
            return stats;
        }

        // #2218 — ce chemin ne sature PAS (T9 : crête ×3,95 laissée passer) ;
        // il compte les overs comme `process_pcm`, avec un LSB de référence à
        // 24 bits faute de profondeur.
        const LSB_REFERENCE: f64 = 8_388_608.0;
        let avant = self.ecretage;
        let base = avant.echantillons_vus;

        for (fi, frame) in samples.chunks_exact_mut(ch_count).enumerate() {
            for (ch, sample) in frame.iter_mut().enumerate() {
                let state = &mut self.states[ch];
                let cascade = &self.filters[ch];
                let mut s = *sample as f64 * self.preamp_gains[ch];
                if !s.is_finite() {
                    stats.non_finite_samples += 1;
                    s = 0.0;
                }
                for (stage, coeffs) in state.iter_mut().zip(cascade.iter()) {
                    s = stage.process(coeffs, s);
                }
                if !s.is_finite() {
                    stats.non_finite_samples += 1;
                    s = 0.0;
                } else if !(-1.0..1.0).contains(&s) {
                    stats.overs += 1;
                    self.ecretage.noter_ecrete(
                        base + (fi * ch_count + ch) as u64,
                        (s.abs() - 1.0) * LSB_REFERENCE,
                        s.abs(),
                    );
                }
                *sample = s as f32;
            }
        }
        self.ecretage.noter_vus(samples.len() as u64);

        self.process_stats.overs = self.process_stats.overs.saturating_add(stats.overs);
        self.process_stats.non_finite_samples = self
            .process_stats
            .non_finite_samples
            .saturating_add(stats.non_finite_samples);
        self.apres_le_bloc(&avant);
        stats
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Diagnostics cumulés depuis la construction du processeur pour la piste.
    pub fn process_stats(&self) -> EqProcessStats {
        self.process_stats
    }

    /// Automatic pre-gain for a channel, in dB.
    /// Linear filter response from the exact prepared coefficients, including
    /// pre-gain. Control-plane only; this is not a live spectrum or clipping model.
    pub fn response(&self, sample_rate: u32) -> serde_json::Value {
        let upper = (f64::from(sample_rate) * 0.499).min(24000.0);
        let hz: Vec<f64> = (0..256)
            .map(|i| 10.0 * (upper / 10.0).powf(i as f64 / 255.0))
            .collect();
        let channels: Vec<Vec<f64>> = self
            .filters
            .iter()
            .enumerate()
            .map(|(channel, cascade)| {
                hz.iter()
                    .map(|frequency| {
                        if !self.enabled {
                            return 0.0;
                        }
                        let w = 2.0 * PI * frequency / f64::from(sample_rate);
                        let mut magnitude = self.preamp_gains[channel];
                        for c in cascade {
                            let re = c.b0 + c.b1 * w.cos() + c.b2 * (2.0 * w).cos();
                            let im = -c.b1 * w.sin() - c.b2 * (2.0 * w).sin();
                            let dre = 1.0 + c.a1 * w.cos() + c.a2 * (2.0 * w).cos();
                            let dim = -c.a1 * w.sin() - c.a2 * (2.0 * w).sin();
                            magnitude *= (re.hypot(im) / dre.hypot(dim)).max(1e-15);
                        }
                        20.0 * magnitude.max(1e-15).log10()
                    })
                    .collect()
            })
            .collect();
        serde_json::json!({"frequency_hz":hz,"channels_db":channels,"sample_rate":sample_rate,"provenance":"prepared_coefficients","includes_preamp":true})
    }

    pub fn preamp_db(&self, channel: u16) -> Option<f64> {
        self.preamp_db.get(channel as usize).copied()
    }

    /// Reprendre l'historique des filtres du processeur que celui-ci remplace,
    /// quand la cascade a la même forme.
    ///
    /// Sert au remplacement **en cours de lecture** (#1725) : un curseur bougé
    /// pendant qu'un morceau joue. Un `EqProcessor` neuf part avec des
    /// `BiquadState` à zéro ; injecter un signal continu dans un filtre dont
    /// l'historique vient d'être remis à zéro est une discontinuité, et une
    /// discontinuité dans le chemin audio, c'est un clic. Un curseur qu'on
    /// fait glisser en produirait un à chaque cran.
    ///
    /// L'état n'est transposable que si la cascade coïncide en nombre de
    /// canaux ET d'étages, parce que `states[canal][étage]` est positionnel :
    /// l'étage *n* de l'ancienne cascade n'est le même filtre que l'étage *n*
    /// de la nouvelle que si rien n'a été inséré ni retiré. Changer le gain ou
    /// la fréquence d'une bande conserve la forme — les coefficients changent,
    /// pas la cascade — et c'est justement le cas courant, celui du glissement.
    ///
    /// Quand la forme change — une bande qui repasse sous le seuil
    /// d'audibilité sort de la cascade via `is_neutral()`, donc le nombre
    /// d'étages bouge — l'historique n'est pas transposable et reste à zéro. Ce
    /// transitoire-là est inévitable, et c'est le même qu'on entend déjà au
    /// début de chaque piste.
    pub fn inherit_state_from(&mut self, previous: &EqProcessor) {
        // `filters` compte desormais les CANAUX, plus les etages : comparer sa
        // longueur ne dit plus rien de la forme de la cascade. Il faut comparer
        // canal par canal, sinon on recopierait l'etat d'une cascade a une
        // autre qui n'a pas les memes etages — c'est-a-dire exactement ce que
        // ce garde-fou existe pour empecher.
        //
        // Et depuis que les bandes peuvent viser un canal, deux canaux du meme
        // profil n'ont pas forcement la meme longueur de cascade : la
        // comparaison DOIT etre par canal.
        if previous.channels != self.channels
            || previous.filters.len() != self.filters.len()
            || previous
                .filters
                .iter()
                .zip(self.filters.iter())
                .any(|(a, b)| a.len() != b.len())
        {
            return;
        }
        self.states.clone_from(&previous.states);
        self.dither_states.clone_from(&previous.dither_states);
        // #3479 — et les COMPTEURS avec l'historique.
        //
        // `process_stats` comptait depuis la construction du processeur. Or sur
        // le chemin `local_a_chaud` un `EqProcessor` neuf est bâti à CHAQUE
        // cran de curseur, délibérément et sans amortissement (#1725) :
        // l'export de Reivax66 en montre sept en 1,5 s. Le compteur repartait
        // donc de zéro exactement au moment que le ticket décrit — « activer
        // l'égaliseur coupe le son » — et le nombre d'échantillons remis à zéro
        // lu dans le rapport de diagnostic pouvait être nul pour la seule
        // raison que l'étage venait d'être remplacé.
        //
        // Un compteur qui se tait au moment qu'on l'a mis là pour observer ne
        // vaut pas mieux que pas de compteur. Il suit donc l'historique des
        // filtres, dans le seul cas où celui-ci est transposable : même forme
        // de cascade, c'est-à-dire précisément le glissement de curseur. Quand
        // la forme change, l'historique ET les compteurs repartent à zéro —
        // c'est un autre étage, et le dire serait faux.
        //
        // Aucun échantillon n'est touché : `process_stats` ne sort que par
        // `dsp_metrics()`, vers `/zones/{id}/signal-path` et le rapport.
        self.process_stats = previous.process_stats;
        // #2218 — et le compteur d'écrêtage, avec ses deux « déjà dit » : le
        // relayé ne clôt pas la piste (elle continue dans `self`), et `self`
        // ne redit pas un premier écrêtage déjà dit.
        self.ecretage = previous.ecretage;
        self.ecretage_premier_dit = previous.ecretage_premier_dit;
        previous
            .ecretage_fin_dite
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// #2218 — la fin de la piste : UNE ligne `dsp_ecretage` avec le total, si
/// quelque chose a été écrêté, et rien sinon. Un processeur relayé par
/// `inherit_state_from` se tait : sa piste continue ailleurs.
///
/// Le processeur est détruit côté producteur (`set_eq` / `replace_eq_live`
/// de la sortie locale, fin du relais du bras progressif), jamais dans le
/// rappel cpal, qui ne fait que vider l'anneau.
impl EqProcessor {
    fn close_telemetry(&mut self) {
        if self
            .ecretage_fin_dite
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        if self.ecretage.echantillons_ecretes > 0 {
            tune_plugin_audio_support::ecretage::REGISTRE
                .egaliseur
                .piste_close();
        }
        tune_plugin_audio_support::ecretage::dire_fin(
            tune_plugin_audio_support::ecretage::EtageEcretant::Egaliseur,
            tune_plugin_audio_support::ecretage::Portee::Piste,
            &self.ecretage,
        );
    }
}

impl Drop for EqProcessor {
    fn drop(&mut self) {
        self.close_telemetry();
    }
}

fn read_sample_f64(buf: &[u8], bytes: usize, bit_depth: u16) -> f64 {
    let max_val = (1i64 << (bit_depth - 1)) as f64;
    let raw = match bytes {
        2 => i16::from_le_bytes([buf[0], buf[1]]) as f64,
        3 => {
            let val = buf[0] as i32 | (buf[1] as i32) << 8 | ((buf[2] as i8) as i32) << 16;
            val as f64
        }
        4 => i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as f64,
        _ => 0.0,
    };
    raw / max_val
}

fn write_sample_f64(buf: &mut [u8], sample: f64, bytes: usize, bit_depth: u16, dither_lsb: f64) {
    let max_val = (1i64 << (bit_depth - 1)) as f64;
    // La saturation à 1,0 − 1 LSB AVANT le dither est propre à l'égaliseur :
    // c'est son écrêtage, compté séparément (`stats.overs`). Le dither,
    // l'arrondi et la saturation finale viennent du module partagé — même
    // arithmétique qu'avant, à l'octet près.
    let clamped = sample.clamp(-1.0, 1.0 - 1.0 / max_val);
    let raw = tune_plugin_audio_support::dither::quantifier_avec(
        clamped * max_val,
        dither_lsb,
        -max_val,
        max_val - 1.0,
    );
    match bytes {
        2 => {
            let b = (raw as i16).to_le_bytes();
            buf[0] = b[0];
            buf[1] = b[1];
        }
        3 => {
            buf[0] = raw as u8;
            buf[1] = (raw >> 8) as u8;
            buf[2] = (raw >> 16) as u8;
        }
        4 => {
            let b = (raw as i32).to_le_bytes();
            buf[0] = b[0];
            buf[1] = b[1];
            buf[2] = b[2];
            buf[3] = b[3];
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response_db(coeffs: BiquadCoeffs, freq: f64, sample_rate: f64) -> f64 {
        let omega = 2.0 * PI * freq / sample_rate;
        let (sin_1, cos_1) = omega.sin_cos();
        let (sin_2, cos_2) = (2.0 * omega).sin_cos();
        let numerator_re = coeffs.b0 + coeffs.b1 * cos_1 + coeffs.b2 * cos_2;
        let numerator_im = -coeffs.b1 * sin_1 - coeffs.b2 * sin_2;
        let denominator_re = 1.0 + coeffs.a1 * cos_1 + coeffs.a2 * cos_2;
        let denominator_im = -coeffs.a1 * sin_1 - coeffs.a2 * sin_2;
        let numerator_power = numerator_re * numerator_re + numerator_im * numerator_im;
        let denominator_power = denominator_re * denominator_re + denominator_im * denominator_im;
        10.0 * (numerator_power / denominator_power).log10()
    }

    #[test]
    fn biquads_match_rbj_reference_points() {
        const SAMPLE_RATE: f64 = 48_000.0;
        const CENTER: f64 = 2_000.0;
        const GAIN_DB: f64 = 9.0;
        const BUTTERWORTH_Q: f64 = std::f64::consts::FRAC_1_SQRT_2;

        let peak = response_db(
            peaking_eq(CENTER, GAIN_DB, 1.3, SAMPLE_RATE),
            CENTER,
            SAMPLE_RATE,
        );
        assert!((peak - GAIN_DB).abs() < 1e-9, "pic central : {peak} dB");

        for (name, coeffs) in [
            ("passe-bas", low_pass(CENTER, BUTTERWORTH_Q, SAMPLE_RATE)),
            ("passe-haut", high_pass(CENTER, BUTTERWORTH_Q, SAMPLE_RATE)),
        ] {
            let cutoff = response_db(coeffs, CENTER, SAMPLE_RATE);
            assert!(
                (cutoff + 3.010_299_956_64).abs() < 1e-9,
                "{name} à la coupure : {cutoff} dB"
            );
        }

        let low = low_shelf(CENTER, GAIN_DB, SAMPLE_RATE);
        assert!((response_db(low, 1.0, SAMPLE_RATE) - GAIN_DB).abs() < 0.001);
        assert!(response_db(low, 23_000.0, SAMPLE_RATE).abs() < 0.001);

        let high = high_shelf(CENTER, GAIN_DB, SAMPLE_RATE);
        assert!(response_db(high, 1.0, SAMPLE_RATE).abs() < 0.001);
        assert!((response_db(high, 23_000.0, SAMPLE_RATE) - GAIN_DB).abs() < 0.001);

        let notch_at_center = response_db(notch(CENTER, 1.0, SAMPLE_RATE), CENTER, SAMPLE_RATE);
        assert!(
            notch_at_center < -250.0,
            "le zéro du notch ne tombe pas au centre : {notch_at_center} dB"
        );
    }

    #[test]
    fn flat_eq_is_transparent() {
        let profile = EqProfile::default();
        let eq = EqProcessor::new(&profile, 44100, 2);
        assert!(!eq.is_enabled());
    }

    #[test]
    fn boosted_eq_modifies_signal() {
        let profile = EqProfile {
            enabled: true,
            bass_gain_db: 6.0,
            mid_gain_db: 0.0,
            treble_gain_db: 0.0,
            ..Default::default()
        };
        let mut eq = EqProcessor::new(&profile, 44100, 2);
        assert!(eq.is_enabled());

        // Generate a 80Hz sine wave (2 channels, 16-bit, 1024 samples)
        let sr = 44100.0;
        let freq = 80.0;
        let mut pcm = Vec::with_capacity(1024 * 4);
        for i in 0..1024 {
            let sample = (2.0 * PI * freq * i as f64 / sr).sin() * 0.5;
            let s16 = (sample * 32767.0) as i16;
            pcm.extend_from_slice(&s16.to_le_bytes()); // L
            pcm.extend_from_slice(&s16.to_le_bytes()); // R
        }

        let original = pcm.clone();
        eq.process_pcm(&mut pcm, 16);

        // Signal should be modified (boosted bass)
        assert_ne!(pcm, original);
    }

    #[test]
    fn neutral_bands_are_transparent() {
        // Expert-mode bands all flat → no cascade, EQ reports disabled.
        let profile = EqProfile {
            enabled: true,
            bands: vec![
                EqBandSpec {
                    freq: 1000.0,
                    gain: 0.0,
                    q: 1.0,
                    band_type: "peak".into(),
                    ..Default::default()
                },
                EqBandSpec {
                    freq: 100.0,
                    gain: 0.0,
                    q: 1.0,
                    band_type: "low_shelf".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let eq = EqProcessor::new(&profile, 44100, 2);
        assert!(!eq.is_enabled());
    }

    #[test]
    fn band_high_shelf_attenuates_treble() {
        // -12 dB high shelf at 2 kHz must attenuate an 8 kHz sine strongly.
        let profile = EqProfile {
            enabled: true,
            bands: vec![EqBandSpec {
                freq: 2000.0,
                gain: -12.0,
                q: 0.71,
                band_type: "high_shelf".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut eq = EqProcessor::new(&profile, 44100, 1);
        assert!(eq.is_enabled());

        let sr = 44100.0;
        let freq = 8000.0;
        let mut pcm = Vec::with_capacity(4096 * 2);
        for i in 0..4096 {
            let sample = (2.0 * PI * freq * i as f64 / sr).sin() * 0.5;
            let s16 = (sample * 32767.0) as i16;
            pcm.extend_from_slice(&s16.to_le_bytes());
        }
        let rms = |buf: &[u8]| {
            let mut acc = 0.0f64;
            let mut n = 0usize;
            for c in buf.as_chunks::<2>().0.iter() {
                let v = i16::from_le_bytes([c[0], c[1]]) as f64 / 32768.0;
                acc += v * v;
                n += 1;
            }
            (acc / n as f64).sqrt()
        };
        let rms_before = rms(&pcm);
        eq.process_pcm(&mut pcm, 16);
        // Skip the first 512 samples (filter settle) for the RMS check.
        let rms_after = rms(&pcm[1024..]);
        let delta_db = 20.0 * (rms_after / rms_before).log10();
        assert!(
            delta_db < -9.0,
            "expected ~-12 dB at 8 kHz, got {delta_db:.2} dB"
        );
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn environment_tone_presets() {
        let mut p = EqProfile::default();

        p.room_size = RoomSize::Small;
        p.speaker_placement = SpeakerPlacement::NearWall;
        let (bass, _, _) = p.environment_tone_preset();
        assert!(bass < 0.0, "small room near wall should cut bass");

        p.room_size = RoomSize::Large;
        p.speaker_placement = SpeakerPlacement::FreeStanding;
        let (bass, _, treble) = p.environment_tone_preset();
        assert!(bass > 0.0, "large room freestanding should boost bass");
        assert!(treble > 0.0, "large room should boost treble");
    }

    /// La réserve est la borne vraie, et elle est calculée PAR CANAL.
    ///
    /// Les trois bandes sont à 1 kHz, Q = 1 : elles se recouvrent
    /// exactement. Le canal gauche voit +6, +3 et −12 (réponse nette −3 dB), le
    /// droit +6 et −12 (nette −6 dB). Jusqu'à #4594 la réserve valait la somme
    /// des gains positifs — 9 dB à gauche, 6 à droite — alors qu'aucune de ces
    /// deux cascades ne pousse de plus de 3 dB nulle part. La norme L1 voit
    /// ce que la somme ne voyait pas : 2,462 dB à gauche, 3,885 à droite, et
    /// c'est le canal le PLUS creusé qui réserve le plus, parce qu'un creux
    /// profond sonne longtemps.
    #[test]
    fn automatic_headroom_is_the_true_bound_per_channel() {
        let profile = EqProfile {
            enabled: true,
            bands: vec![
                EqBandSpec {
                    gain: 6.0,
                    channel: None,
                    ..Default::default()
                },
                EqBandSpec {
                    gain: 3.0,
                    channel: Some(0),
                    ..Default::default()
                },
                EqBandSpec {
                    gain: -12.0,
                    channel: None,
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        assert_eq!(
            profile.automatic_headroom_db(0),
            -2.461_945_232_181_620_6
        );
        assert_eq!(
            profile.automatic_headroom_db(1),
            -3.884_722_078_857_195_4
        );
        let eq = EqProcessor::new(&profile, 44_100, 2);
        assert_eq!(eq.preamp_db(0), Some(-2.461_945_232_181_620_6));
        assert_eq!(eq.preamp_db(1), Some(-3.884_722_078_857_195_4));
    }

    #[test]
    fn processed_integer_silence_receives_zero_mean_tpdf_dither() {
        let profile = EqProfile {
            enabled: true,
            bands: vec![EqBandSpec {
                freq: 20_000.0,
                gain: -1.0,
                band_type: "high_shelf".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut pcm = vec![0_u8; 32_768 * 2];
        let stats = EqProcessor::new(&profile, 44_100, 1).process_pcm(&mut pcm, 16);
        let values: Vec<i16> = pcm
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        let sum: i64 = values.iter().map(|&v| i64::from(v)).sum();

        assert_eq!(stats, EqProcessStats::default());
        assert!(values.contains(&-1));
        assert!(values.contains(&1));
        assert!(sum.abs() < 512, "TPDF mean drifted: sum={sum}");
    }

    #[test]
    fn boosted_full_scale_signal_needs_no_hidden_saturator() {
        let profile = EqProfile {
            enabled: true,
            bass_gain_db: 12.0,
            mid_gain_db: 12.0,
            treble_gain_db: 12.0,
            ..Default::default()
        };
        let mut pcm = Vec::with_capacity(44_100 * 4);
        for i in 0..44_100 {
            let value = (2.0 * PI * 997.0 * i as f64 / 44_100.0).sin() * 0.999;
            let raw = (value * i32::MAX as f64) as i32;
            pcm.extend_from_slice(&raw.to_le_bytes());
        }

        let mut eq = EqProcessor::new(&profile, 44_100, 1);
        // #4594 : 19,970 dB (la borne vraie) et non plus 36 (12 + 12 + 12).
        // Ce qui compte pour ce témoin n'a pas bougé d'un iota : `overs == 0`.
        assert_eq!(eq.preamp_db(0), Some(-19.969_591_046_242_122));
        let stats = eq.process_pcm(&mut pcm, 32);
        assert_eq!(stats.overs, 0);
        assert_eq!(stats.non_finite_samples, 0);
    }

    #[test]
    fn float_path_is_linear_and_has_no_implicit_saturator() {
        let profile = EqProfile {
            enabled: true,
            bands: vec![EqBandSpec {
                freq: 1_000.0,
                gain: 12.0,
                ..Default::default()
            }],
            ..Default::default()
        };
        let input: Vec<f32> = (0..4096)
            .map(|i| (2.0 * PI * 1_000.0 * i as f64 / 44_100.0).sin() as f32 * 0.4)
            .collect();
        let mut quiet = input.clone();
        let mut loud: Vec<f32> = input.iter().map(|sample| sample * 2.0).collect();

        EqProcessor::new(&profile, 44_100, 1).process_interleaved(&mut quiet);
        EqProcessor::new(&profile, 44_100, 1).process_interleaved(&mut loud);

        for (index, (quiet, loud)) in quiet.iter().zip(&loud).enumerate() {
            assert!(
                (*loud - 2.0 * *quiet).abs() < 2e-6,
                "nonlinear output at sample {index}: {quiet} -> {loud}"
            );
        }
    }

    #[test]
    fn float_path_reports_overs_without_hiding_them() {
        let profile = EqProfile {
            enabled: true,
            bands: vec![EqBandSpec {
                freq: 1_000.0,
                gain: 12.0,
                ..Default::default()
            }],
            ..Default::default()
        };
        let mut eq = EqProcessor::new(&profile, 44_100, 1);
        // Disable the automatic allowance only inside this counter-test to
        // prove that an over is reported and remains observable in float.
        eq.preamp_gains[0] = 1.0;
        let mut samples: Vec<f32> = (0..4096)
            .map(|i| (2.0 * PI * 1_000.0 * i as f64 / 44_100.0).sin() as f32 * 0.8)
            .collect();

        let stats = eq.process_interleaved(&mut samples);

        assert!(stats.overs > 0);
        assert!(samples.iter().any(|sample| sample.abs() > 1.0));
        assert_eq!(eq.process_stats(), stats);

        let mut second = vec![0.9_f32; 128];
        let second_stats = eq.process_interleaved(&mut second);
        assert_eq!(
            eq.process_stats().overs,
            stats.overs.saturating_add(second_stats.overs)
        );
    }

    #[test]
    fn invalid_float_sample_is_counted_and_never_propagated() {
        let profile = shelf_profile();
        let mut samples = vec![f32::NAN, 0.0];
        let stats = EqProcessor::new(&profile, 44_100, 1).process_interleaved(&mut samples);

        assert_eq!(stats.non_finite_samples, 1);
        assert_eq!(samples[0], 0.0);
        assert!(samples[1].is_finite());
    }

    /// Profil de test : -12 dB de plateau aigu à 2 kHz, stéréo.
    fn shelf_profile() -> EqProfile {
        EqProfile {
            enabled: true,
            bands: vec![EqBandSpec {
                freq: 2000.0,
                gain: -12.0,
                q: 0.71,
                band_type: "high_shelf".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// Sinus stéréo entrelacé, en f32 et en PCM 32 bits, échantillon pour
    /// échantillon.
    fn stereo_sine(freq: f64, frames: usize) -> (Vec<f32>, Vec<u8>) {
        let sr = 44100.0;
        let mut f32s = Vec::with_capacity(frames * 2);
        let mut pcm = Vec::with_capacity(frames * 8);
        for i in 0..frames {
            let v = (2.0 * PI * freq * i as f64 / sr).sin() * 0.5;
            // Aller-retour par l'entier 32 bits AVANT de remplir les deux
            // tampons : sans ça la comparaison mesurerait l'erreur de
            // quantification du PCM, pas l'égalité des deux chemins.
            let raw = (v * 2147483648.0) as i32;
            let q = raw as f64 / 2147483648.0;
            for _ in 0..2 {
                f32s.push(q as f32);
                pcm.extend_from_slice(&raw.to_le_bytes());
            }
        }
        (f32s, pcm)
    }

    /// Le chemin f32 (sortie locale) et le chemin PCM (transcodage) doivent
    /// donner le MÊME signal : c'est ce qui permet à une zone d'entendre la
    /// même correction sur son DAC et vers un renderer réseau.
    #[test]
    fn interleaved_matches_pcm_path() {
        let profile = shelf_profile();
        let (mut f32s, mut pcm) = stereo_sine(8000.0, 2048);

        EqProcessor::new(&profile, 44100, 2).process_interleaved(&mut f32s);
        EqProcessor::new(&profile, 44100, 2).process_pcm(&mut pcm, 32);

        assert_eq!(f32s.len() * 4, pcm.len());
        for (i, (got, chunk)) in f32s.iter().zip(pcm.as_chunks::<4>().0.iter()).enumerate() {
            let expected =
                i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]) as f32 / 2147483648.0;
            assert!(
                (got - expected).abs() < 1e-6,
                "échantillon {i} : f32={got} pcm={expected}"
            );
        }
    }

    /// Les deux canaux ont leur propre état de biquad : un signal présent à
    /// gauche seulement ne doit pas colorer la droite. Une erreur d'indice
    /// dans `process_interleaved` se verrait ici et nulle part ailleurs.
    #[test]
    fn interleaved_keeps_channels_independent() {
        let profile = shelf_profile();
        let mut eq = EqProcessor::new(&profile, 44100, 2);
        let mut samples = vec![0.0f32; 2048];
        for f in 0..1024 {
            samples[2 * f] = ((2.0 * PI * 8000.0 * f as f64 / 44100.0).sin() * 0.5) as f32;
        }
        eq.process_interleaved(&mut samples);
        for f in 0..1024 {
            assert_eq!(samples[2 * f + 1], 0.0, "canal droit sali à la trame {f}");
        }
        assert!(samples.iter().step_by(2).any(|&s| s != 0.0));
    }

    /// Un profil désactivé (ou en mode PURE, que `load_eq_processor` traduit
    /// par `None`) laisse le signal strictement intact — la promesse
    /// bit-perfect.
    #[test]
    fn interleaved_is_identity_when_disabled() {
        let profile = EqProfile {
            enabled: false,
            ..shelf_profile()
        };
        let mut eq = EqProcessor::new(&profile, 44100, 2);
        assert!(!eq.is_enabled());
        let (mut samples, _) = stereo_sine(1000.0, 256);
        let before = samples.clone();
        eq.process_interleaved(&mut samples);
        assert_eq!(samples, before);
    }

    /// Un tampon qui ne contient pas un nombre entier de trames est laissé
    /// intact : le traiter à moitié décalerait l'état des filtres d'un canal
    /// et toutes les trames suivantes seraient filtrées avec le mauvais
    /// historique.
    #[test]
    fn interleaved_ignores_partial_frame() {
        let profile = shelf_profile();
        let mut eq = EqProcessor::new(&profile, 44100, 2);
        let mut samples = vec![0.5f32; 7]; // 3 trames + 1 échantillon
        let before = samples.clone();
        eq.process_interleaved(&mut samples);
        assert_eq!(samples, before);
    }

    /// L'état des biquads persiste d'un appel à l'autre : la sortie d'un
    /// tampon découpé en morceaux est identique à celle du tampon entier.
    /// C'est la condition pour que la sortie locale, qui reçoit l'audio par
    /// paquets de taille arbitraire, ne craque pas aux jointures.
    #[test]
    fn interleaved_state_persists_across_chunks() {
        let profile = shelf_profile();
        let (whole, _) = stereo_sine(8000.0, 1024);

        let mut one_shot = whole.clone();
        EqProcessor::new(&profile, 44100, 2).process_interleaved(&mut one_shot);

        let mut chunked = whole.clone();
        let mut eq = EqProcessor::new(&profile, 44100, 2);
        for chunk in chunked.chunks_mut(200 * 2) {
            eq.process_interleaved(chunk);
        }

        assert_eq!(one_shot, chunked);
    }

    /// Même profil, même forme de cascade : reprendre l'état revient exactement
    /// au même que n'avoir jamais changé de processeur. C'est LA garantie qui
    /// autorise à remplacer l'égaliseur en pleine lecture sans que ça s'entende.
    #[test]
    fn inherited_state_continues_the_stream_exactly() {
        let profile = shelf_profile();
        let (whole, _) = stereo_sine(8000.0, 1024);
        let split = 400 * 2; // frontière du remplacement, en échantillons

        // Référence : un seul processeur, du début à la fin.
        let mut reference = whole.clone();
        EqProcessor::new(&profile, 44100, 2).process_interleaved(&mut reference);

        // Remplacement à chaud : un processeur neuf reprend l'historique.
        let mut swapped = whole.clone();
        let mut first = EqProcessor::new(&profile, 44100, 2);
        first.process_interleaved(&mut swapped[..split]);
        let mut second = EqProcessor::new(&profile, 44100, 2);
        second.inherit_state_from(&first);
        second.process_interleaved(&mut swapped[split..]);

        assert_eq!(reference, swapped);
    }

    /// Sans reprise d'état, le même remplacement dévie du flux continu — c'est
    /// le clic. Ce test existe pour que la reprise d'état ne puisse pas
    /// disparaître en silence lors d'un remaniement.
    #[test]
    fn dropping_state_breaks_the_stream_where_inheriting_does_not() {
        let profile = shelf_profile();
        let (whole, _) = stereo_sine(8000.0, 1024);
        let split = 400 * 2;

        let mut reference = whole.clone();
        EqProcessor::new(&profile, 44100, 2).process_interleaved(&mut reference);

        let mut naive = whole.clone();
        EqProcessor::new(&profile, 44100, 2).process_interleaved(&mut naive[..split]);
        // Processeur neuf, historique à zéro : le comportement d'un `set_eq`
        // nu en cours de lecture.
        EqProcessor::new(&profile, 44100, 2).process_interleaved(&mut naive[split..]);

        let ecart = (naive[split] - reference[split]).abs();
        assert!(
            ecart > 1e-6,
            "un historique perdu devrait dévier du flux continu (écart {ecart})"
        );
    }

    /// La cascade change de forme (une bande s'ajoute) : l'état n'est plus
    /// transposable et ne doit PAS être recopié sur des étages qui ne se
    /// correspondent plus.
    #[test]
    fn state_is_not_inherited_across_a_different_cascade() {
        let one_band = shelf_profile();
        let mut two_bands = shelf_profile();
        two_bands.bands.push(EqBandSpec {
            freq: 120.0,
            gain: 6.0,
            q: 0.71,
            band_type: "low_shelf".into(),
            ..Default::default()
        });

        let (mut buf, _) = stereo_sine(8000.0, 256);
        let mut warmed = EqProcessor::new(&one_band, 44100, 2);
        warmed.process_interleaved(&mut buf);

        let mut fresh = EqProcessor::new(&two_bands, 44100, 2);
        fresh.inherit_state_from(&warmed);

        assert_eq!(
            fresh.states,
            EqProcessor::new(&two_bands, 44100, 2).states,
            "état repris d'une cascade de taille différente"
        );
    }

    /// #3479 — le compteur d'échantillons remis à ZÉRO survit au remplacement
    /// à chaud, sinon il se tait au moment précis qu'il est là pour observer.
    ///
    /// Sur le chemin `local_a_chaud`, un `EqProcessor` neuf est bâti à CHAQUE
    /// cran de curseur — sept en 1,5 s dans l'export de Reivax66. Un compteur
    /// qui repart de zéro à chaque cran rendrait `0` au rapport de diagnostic
    /// pour la seule raison que l'étage vient d'être remplacé, et ce `0` se
    /// lirait comme « l'égaliseur n'a rien mis à zéro ».
    #[test]
    fn les_compteurs_survivent_au_remplacement_a_chaud() {
        let profile = shelf_profile();
        let mut premier = EqProcessor::new(&profile, 44_100, 2);
        // Une charge utile qui FAIT compter : deux `NaN`, un par canal.
        let mut souille = vec![f32::NAN, f32::NAN, 0.1, 0.1];
        premier.process_interleaved(&mut souille);
        assert_eq!(
            premier.process_stats().non_finite_samples,
            2,
            "le banc doit d'abord faire compter quelque chose"
        );

        let mut second = EqProcessor::new(&profile, 44_100, 2);
        second.inherit_state_from(&premier);

        assert_eq!(
            second.process_stats().non_finite_samples,
            2,
            "#3479 — un cran de curseur ne doit pas effacer ce que l'étage \
             précédent a mesuré : le rapport de diagnostic lirait un `0` qui \
             ne veut rien dire"
        );
    }

    /// La contre-épreuve : quand la forme de la cascade CHANGE, l'historique
    /// n'est pas transposable — et les compteurs non plus. Ce n'est plus le
    /// même étage, et le prétendre serait faux.
    #[test]
    fn les_compteurs_ne_survivent_pas_a_un_changement_de_forme() {
        let profile = shelf_profile();
        let mut stereo = EqProcessor::new(&profile, 44_100, 2);
        let mut souille = vec![f32::NAN, f32::NAN];
        stereo.process_interleaved(&mut souille);
        assert_eq!(stereo.process_stats().non_finite_samples, 2);

        let mut mono = EqProcessor::new(&profile, 44_100, 1);
        mono.inherit_state_from(&stereo);

        assert_eq!(
            mono.process_stats().non_finite_samples,
            0,
            "une cascade d'une autre forme est un autre étage : reprendre ses \
             compteurs attribuerait à l'un ce que l'autre a fait"
        );
    }

    /// Le nombre de canaux est l'autre garde-fou : reprendre l'état d'une
    /// cascade stéréo sur une cascade mono écraserait la structure.
    #[test]
    fn state_is_not_inherited_across_a_channel_count_change() {
        let profile = shelf_profile();
        let mut stereo = EqProcessor::new(&profile, 44100, 2);
        let (mut buf, _) = stereo_sine(8000.0, 256);
        stereo.process_interleaved(&mut buf);

        let mut mono = EqProcessor::new(&profile, 44100, 1);
        mono.inherit_state_from(&stereo);

        assert_eq!(mono.states.len(), 1, "le nombre de canaux a été écrasé");
        assert_eq!(mono.states, EqProcessor::new(&profile, 44100, 1).states);
    }

    // --- Une bande peut ne viser QU'UN canal (Alexander Jam, Premium) ---

    fn bande(freq: f64, gain: f64, canal: Option<u16>) -> EqBandSpec {
        EqBandSpec {
            freq,
            gain,
            q: 0.71,
            band_type: "peak".into(),
            channel: canal,
        }
    }

    fn energie(samples: &[f32], pas: usize, depart: usize) -> f64 {
        samples
            .iter()
            .skip(depart)
            .step_by(pas)
            .map(|v| (*v as f64) * (*v as f64))
            .sum()
    }

    /// Le coeur de la demande : une piece dissymetrique se corrige d'un seul
    /// cote. Avant, la meme courbe partait a gauche ET a droite — l'egaliseur
    /// ne pouvait pas rattraper un desequilibre, ce pour quoi cet abonne avait
    /// paye.
    #[test]
    fn une_bande_ne_touche_que_son_canal() {
        let profil = EqProfile {
            enabled: true,
            bands: vec![bande(1000.0, -12.0, Some(0))],
            ..Default::default()
        };
        let (mut buf, _) = stereo_sine(1000.0, 4096);
        let avant_g = energie(&buf, 2, 0);
        let avant_d = energie(&buf, 2, 1);

        EqProcessor::new(&profil, 44100, 2).process_interleaved(&mut buf);

        let apres_g = energie(&buf, 2, 0);
        let apres_d = energie(&buf, 2, 1);
        assert!(
            apres_g < avant_g * 0.2,
            "la gauche doit etre attenuee : {avant_g} -> {apres_g}"
        );
        assert!(
            (apres_d - avant_d).abs() / avant_d < 0.01,
            "la droite doit rester intacte : {avant_d} -> {apres_d}"
        );
    }

    /// Deux courbes differentes, une par canal — le cas reel d'une piece dont
    /// un seul cote resonne.
    #[test]
    fn chaque_canal_peut_avoir_sa_propre_courbe() {
        let profil = EqProfile {
            enabled: true,
            bands: vec![bande(1000.0, -6.0, Some(0)), bande(1000.0, -12.0, Some(1))],
            ..Default::default()
        };
        let (mut buf, _) = stereo_sine(1000.0, 4096);
        let avant = energie(&buf, 2, 0);
        EqProcessor::new(&profil, 44100, 2).process_interleaved(&mut buf);
        let gauche = energie(&buf, 2, 0);
        let droite = energie(&buf, 2, 1);
        assert!(gauche < avant, "gauche attenuee");
        assert!(droite < gauche, "droite davantage attenuee");
    }

    /// Le defaut ne change RIEN : un prereglage enregistre avant cette version
    /// n'a pas de champ `channel`, et doit se comporter exactement comme
    /// avant — sur les deux canaux.
    #[test]
    fn une_bande_sans_canal_agit_partout_comme_avant() {
        let profil = EqProfile {
            enabled: true,
            bands: vec![bande(1000.0, -12.0, None)],
            ..Default::default()
        };
        let (mut buf, _) = stereo_sine(1000.0, 4096);
        let avant = energie(&buf, 2, 0);
        EqProcessor::new(&profil, 44100, 2).process_interleaved(&mut buf);
        let g = energie(&buf, 2, 0);
        let d = energie(&buf, 2, 1);
        assert!(
            g < avant * 0.2 && d < avant * 0.2,
            "les deux canaux sont attenues"
        );
        assert!((g - d).abs() / g < 1e-6, "et de la meme facon : {g} vs {d}");
    }

    /// Un JSON d'avant cette version se relit sans `channel` — le champ est
    /// facultatif, et son absence vaut « tous les canaux ».
    #[test]
    fn un_prereglage_ancien_se_relit_sans_canal() {
        let json = r#"{"freq":100.0,"gain":3.0,"q":0.7,"type":"low_shelf"}"#;
        let b: EqBandSpec = serde_json::from_str(json).unwrap();
        assert_eq!(b.channel, None);
        assert!(b.vise_le_canal(0) && b.vise_le_canal(1));
        // Et il ne repart PAS avec un champ que le client ne connait pas.
        assert!(!serde_json::to_string(&b).unwrap().contains("channel"));
    }

    #[test]
    fn une_bande_qui_ne_vise_aucun_canal_existant_n_active_rien() {
        // Canal 5 sur une sortie stereo : le profil ne doit pas rester
        // « actif » a ne rien faire.
        let profil = EqProfile {
            enabled: true,
            bands: vec![bande(1000.0, 12.0, Some(5))],
            ..Default::default()
        };
        assert!(!EqProcessor::new(&profil, 44100, 2).enabled);
    }
}
