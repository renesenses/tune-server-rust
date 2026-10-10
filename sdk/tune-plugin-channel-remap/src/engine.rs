//! Le moteur : un réglage en dB devient une [`Matrice`] linéaire, puis
//! s'applique à des trames entrelacées.
//!
//! # Trois garanties
//!
//! 1. **Au bit près pour l'identité et les permutations.** Quand chaque sortie
//!    recopie au plus UNE entrée, à 0 dB exactement (identité, échange
//!    gauche/droite, 4.0 → 5.1 avec C et LFE muets), la sortie est une
//!    RECOPIE des échantillons — aucune multiplication, aucun arrondi, quel que
//!    soit le format. C'est [`Matrice::est_recopie`].
//! 2. **Pas d'écrêtage par construction**, quand la normalisation est armée
//!    (défaut) : chaque ligne dont la somme des |gains| dépasse 1 est divisée
//!    par cette somme. Deux entrées corrélées à pleine échelle ne peuvent plus
//!    dépasser la pleine échelle. Les rapports entre gains sont conservés ;
//!    l'atténuation est publiée par [`Matrice::attenuation_db`].
//! 3. **Rien d'inventé.** Une case vide (`null`) est un silence, pas un 0 dB ;
//!    un réglage mal formé est refusé, jamais complété.
//!
//! # Ordre des canaux
//!
//! Celui que le décodeur de Tune rend, c'est-à-dire l'ordre par défaut de FLAC
//! et de WAVE_FORMAT_EXTENSIBLE ([`NOMS_PAR_DEFAUT`]). En 4 canaux : FL FR BL
//! BR. En 5.1 : FL FR FC LFE BL BR. En 7.1 : FL FR FC LFE BL BR SL SR.
use serde::{Deserialize, Serialize};

/// Le plus grand nombre de canaux que Tune décode (`audio/channels.rs`).
pub const CANAUX_MAX: u16 = 32;
/// Borne basse d'un gain en dB. En dessous, on écrit `null` (muet).
pub const GAIN_MIN_DB: f32 = -60.0;
/// Borne haute d'un gain en dB.
pub const GAIN_MAX_DB: f32 = 12.0;

/// −3,0103 dB : 1/√2, le coefficient ITU-R BS.775 du centre et des surrounds.
pub const MOINS_3_DB: f32 = -3.010_3;

/// Le réglage tel qu'il est enregistré et échangé avec l'écran.
#[cfg_attr(feature = "schemas", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelRemapSettings {
    pub enabled: bool,
    // Nombre de canaux de la SOURCE auxquels la matrice s'applique.
    #[cfg_attr(feature = "schemas", schemars(range(min = 1, max = 32)))]
    pub inputs: u16,
    // Nombre de canaux de SORTIE que la matrice produit.
    #[cfg_attr(feature = "schemas", schemars(range(min = 1, max = 32)))]
    pub outputs: u16,
    // Gains en dB : une ligne par sortie, une colonne par entrée ; null = muet.
    pub gains_db: Vec<Vec<Option<f32>>>,
    // Garde-fou d'écrêtage : chaque ligne est ramenée à une somme de |gains| <= 1.
    pub normalize: bool,
    // Le préréglage d'origine, s'il y en a un (informatif).
    pub preset: Option<String>,
}

impl Default for ChannelRemapSettings {
    fn default() -> Self {
        let mut s = identite(2);
        s.enabled = false;
        s
    }
}

/// Pourquoi un réglage est refusé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErreurDeMatrice {
    /// `inputs` ou `outputs` hors de 1..=32.
    Canaux,
    /// `gains_db` n'a pas `outputs` lignes de `inputs` colonnes.
    Forme,
    /// Un gain n'est pas fini ou sort de [−60, +12] dB.
    Gain,
}

/// Une matrice prête à appliquer.
#[derive(Debug, Clone, PartialEq)]
pub struct Matrice {
    entrees: usize,
    sorties: usize,
    /// `sorties × entrees`, ligne par sortie, APRÈS normalisation.
    coefs: Vec<f64>,
    /// `Some` quand chaque sortie recopie au plus une entrée à gain unité :
    /// l'index de l'entrée recopiée, ou `None` pour un silence.
    routes: Option<Vec<Option<usize>>>,
    /// Par sortie, ce que la normalisation a retiré, en dB (0 ou négatif).
    attenuation_db: Vec<f64>,
}

fn lineaire(db: f32) -> f64 {
    // `powf(0)` vaut exactement 1 : un 0 dB reste un gain unité, ce qui garde
    // la recopie au bit près.
    10f64.powf(f64::from(db) / 20.0)
}

impl Matrice {
    /// Valider et compiler un réglage. Ne regarde pas `enabled` : c'est à
    /// l'appelant de ne pas appliquer un réglage éteint.
    pub fn depuis_reglage(s: &ChannelRemapSettings) -> Result<Self, ErreurDeMatrice> {
        if !(1..=CANAUX_MAX).contains(&s.inputs) || !(1..=CANAUX_MAX).contains(&s.outputs) {
            return Err(ErreurDeMatrice::Canaux);
        }
        let (n, m) = (usize::from(s.inputs), usize::from(s.outputs));
        if s.gains_db.len() != m || s.gains_db.iter().any(|ligne| ligne.len() != n) {
            return Err(ErreurDeMatrice::Forme);
        }
        let mut coefs = Vec::with_capacity(n * m);
        for gain in s.gains_db.iter().flatten() {
            coefs.push(match gain {
                None => 0.0,
                Some(db) if db.is_finite() && (GAIN_MIN_DB..=GAIN_MAX_DB).contains(db) => {
                    lineaire(*db)
                }
                Some(_) => return Err(ErreurDeMatrice::Gain),
            });
        }
        let mut attenuation_db = vec![0.0; m];
        if s.normalize {
            for (ligne, att) in coefs.chunks_exact_mut(n).zip(attenuation_db.iter_mut()) {
                let somme: f64 = ligne.iter().map(|c| c.abs()).sum();
                if somme > 1.0 {
                    for c in ligne.iter_mut() {
                        *c /= somme;
                    }
                    *att = -20.0 * somme.log10();
                }
            }
        }
        let routes = coefs
            .chunks_exact(n)
            .map(|ligne| {
                let actifs: Vec<usize> = (0..n).filter(|&i| ligne[i] != 0.0).collect();
                match actifs.as_slice() {
                    [] => Some(None),
                    [i] if ligne[*i] == 1.0 => Some(Some(*i)),
                    _ => None,
                }
            })
            .collect::<Option<Vec<_>>>();
        Ok(Self {
            entrees: n,
            sorties: m,
            coefs,
            routes,
            attenuation_db,
        })
    }

    /// Le réglage s'il est armé et valide ; `None` sinon.
    pub fn du_reglage_arme(s: &ChannelRemapSettings) -> Option<Self> {
        s.enabled.then(|| Self::depuis_reglage(s).ok()).flatten()
    }

    pub fn entrees(&self) -> u16 {
        self.entrees as u16
    }
    pub fn sorties(&self) -> u16 {
        self.sorties as u16
    }
    /// Le gain linéaire effectif (après normalisation) de `entree` vers `sortie`.
    pub fn coefficient(&self, sortie: usize, entree: usize) -> f64 {
        self.coefs[sortie * self.entrees + entree]
    }
    /// Par sortie, l'atténuation de la normalisation, en dB (0 ou négatif).
    pub fn attenuation_db(&self) -> &[f64] {
        &self.attenuation_db
    }
    /// Chaque sortie recopie-t-elle au plus une entrée, à gain unité ?
    pub fn est_recopie(&self) -> bool {
        self.routes.is_some()
    }
    /// La matrice ne change-t-elle RIEN (N = M, chaque canal à sa place) ?
    pub fn est_identite(&self) -> bool {
        self.entrees == self.sorties
            && self
                .routes
                .as_ref()
                .is_some_and(|r| r.iter().enumerate().all(|(o, src)| *src == Some(o)))
    }

    /// Appliquer à une trame : `entree` a `entrees()` échantillons, `sortie`
    /// en reçoit `sorties()`. `vers`/`depuis` convertissent le type natif vers
    /// et depuis le domaine de calcul ; une recopie ne les appelle pas.
    pub fn appliquer_trame<T: Copy>(
        &self,
        entree: &[T],
        sortie: &mut [T],
        silence: T,
        vers: impl Fn(T) -> f64,
        depuis: impl Fn(f64) -> T,
    ) {
        debug_assert_eq!(entree.len(), self.entrees);
        debug_assert_eq!(sortie.len(), self.sorties);
        if let Some(routes) = &self.routes {
            for (o, src) in sortie.iter_mut().zip(routes) {
                *o = src.map_or(silence, |i| entree[i]);
            }
            return;
        }
        for (o, ligne) in sortie.iter_mut().zip(self.coefs.chunks_exact(self.entrees)) {
            let somme: f64 = ligne
                .iter()
                .zip(entree)
                .filter(|(c, _)| **c != 0.0)
                .map(|(c, x)| c * vers(*x))
                .sum();
            *o = depuis(somme);
        }
    }

    /// Appliquer à un tampon `f32` entrelacé de `entrees()` canaux ; rend un
    /// tampon de `sorties()` canaux, au même nombre de trames. Une trame
    /// incomplète en fin de tampon est ignorée (comme `adapt_channels_f32`).
    pub fn appliquer_f32(&self, entree: &[f32]) -> Vec<f32> {
        let trames = entree.len() / self.entrees;
        let mut sortie = vec![0.0f32; trames * self.sorties];
        for (e, s) in entree
            .chunks_exact(self.entrees)
            .zip(sortie.chunks_exact_mut(self.sorties))
        {
            self.appliquer_trame(e, s, 0.0, f64::from, |v| v as f32);
        }
        sortie
    }

    /// Appliquer à un tampon d'entiers justifiés à droite de `profondeur` bits ;
    /// un mélange est arrondi au plus proche puis borné à la pleine échelle.
    pub fn appliquer_i32(&self, entree: &[i32], profondeur: u16) -> Vec<i32> {
        let p = profondeur.clamp(8, 32);
        let min = -(1i64 << (p - 1)) as f64;
        let max = ((1i64 << (p - 1)) - 1) as f64;
        let trames = entree.len() / self.entrees;
        let mut sortie = vec![0i32; trames * self.sorties];
        for (e, s) in entree
            .chunks_exact(self.entrees)
            .zip(sortie.chunks_exact_mut(self.sorties))
        {
            self.appliquer_trame(e, s, 0, f64::from, |v| v.round().clamp(min, max) as i32);
        }
        sortie
    }
}

/// Les noms des canaux dans l'ordre que rend le décodeur de Tune (ordre par
/// défaut FLAC / WAVE_FORMAT_EXTENSIBLE), de 1 à 8 canaux. Au-delà, l'écran
/// numérote.
pub const NOMS_PAR_DEFAUT: [&[&str]; 8] = [
    &["M"],
    &["FL", "FR"],
    &["FL", "FR", "FC"],
    &["FL", "FR", "BL", "BR"],
    &["FL", "FR", "FC", "BL", "BR"],
    &["FL", "FR", "FC", "LFE", "BL", "BR"],
    &["FL", "FR", "FC", "LFE", "BC", "SL", "SR"],
    &["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"],
];

/// Les noms des `n` canaux, s'ils ont un ordre par défaut.
pub fn noms_des_canaux(n: u16) -> Option<&'static [&'static str]> {
    (1..=8)
        .contains(&n)
        .then(|| NOMS_PAR_DEFAUT[usize::from(n) - 1])
}

/// La matrice identité de `n` canaux (armée).
pub fn identite(n: u16) -> ChannelRemapSettings {
    let n = n.clamp(1, CANAUX_MAX);
    let gains_db = (0..n)
        .map(|o| (0..n).map(|i| (i == o).then_some(0.0)).collect())
        .collect();
    ChannelRemapSettings {
        enabled: true,
        inputs: n,
        outputs: n,
        gains_db,
        normalize: true,
        preset: Some("identity".into()),
    }
}

/// Les identifiants des préréglages, dans l'ordre de l'écran.
pub const PREREGLAGES: [&str; 7] = [
    "quad_to_stereo",
    "quad_to_5_1",
    "quad_to_7_1",
    "5_1_to_stereo_itu",
    "swap_lr",
    "mono",
    "identity",
];

fn matrice(
    preset: &str,
    inputs: u16,
    outputs: u16,
    routes: &[(usize, usize, f32)],
) -> ChannelRemapSettings {
    let mut gains_db = vec![vec![None; usize::from(inputs)]; usize::from(outputs)];
    for &(sortie, entree, db) in routes {
        gains_db[sortie][entree] = Some(db);
    }
    ChannelRemapSettings {
        enabled: true,
        inputs,
        outputs,
        gains_db,
        normalize: true,
        preset: Some(preset.into()),
    }
}

/// Un préréglage, armé et normalisé. `None` pour un identifiant inconnu.
///
/// - `quad_to_stereo` : 4.0 → 2.0, G = FL + BL −3 dB, D = FR + BR −3 dB
///   (normalisé : rien n'est perdu, rien n'écrête) ;
/// - `quad_to_5_1` : 4.0 → 5.1, avant sur l'avant, arrière sur les surrounds,
///   centre et LFE muets (recopie au bit près) ;
/// - `quad_to_7_1` : 4.0 → 7.1, arrière sur BL/BR, C, LFE, SL et SR muets ;
/// - `5_1_to_stereo_itu` : ITU-R BS.775, C et surrounds à −3 dB, LFE écarté ;
/// - `swap_lr` : échange gauche/droite (recopie au bit près) ;
/// - `mono` : (G + D) / 2 sur les deux voies ;
/// - `identity` : stéréo inchangée (point de départ d'une matrice libre).
pub fn prereglage(id: &str) -> Option<ChannelRemapSettings> {
    let m3 = MOINS_3_DB;
    Some(match id {
        "quad_to_stereo" => matrice(
            id,
            4,
            2,
            &[(0, 0, 0.0), (0, 2, m3), (1, 1, 0.0), (1, 3, m3)],
        ),
        "quad_to_5_1" => matrice(
            id,
            4,
            6,
            &[(0, 0, 0.0), (1, 1, 0.0), (4, 2, 0.0), (5, 3, 0.0)],
        ),
        "quad_to_7_1" => matrice(
            id,
            4,
            8,
            &[(0, 0, 0.0), (1, 1, 0.0), (4, 2, 0.0), (5, 3, 0.0)],
        ),
        "5_1_to_stereo_itu" => matrice(
            id,
            6,
            2,
            &[
                (0, 0, 0.0),
                (0, 2, m3),
                (0, 4, m3),
                (1, 1, 0.0),
                (1, 2, m3),
                (1, 5, m3),
            ],
        ),
        "swap_lr" => matrice(id, 2, 2, &[(0, 1, 0.0), (1, 0, 0.0)]),
        "mono" => matrice(
            id,
            2,
            2,
            &[(0, 0, 0.0), (0, 1, 0.0), (1, 0, 0.0), (1, 1, 0.0)],
        ),
        "identity" => identite(2),
        _ => return None,
    })
}
