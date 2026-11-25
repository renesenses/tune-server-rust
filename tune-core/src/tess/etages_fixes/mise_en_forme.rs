//! L'étape finale **`noise_shaped`** : quantification PCM avec dither TPDF
//! ±1 LSB et mise en forme du bruit par rétroaction d'erreur.
//!
//! ```text
//!   v[n] = x[n] − Σ c_k · e[n−k]
//!   y[n] = Q( v[n] + d[n] )          d : TPDF ±1 LSB
//!   e[n] = y[n] − v[n]               ⇒  Y = X + (1 − C(z))·E
//! ```
//!
//! Les coefficients `c_k` sont ceux, publiés, du suréchantillonneur du SDK
//! (`tune_plugin_surechantillonnage::dither`) : une seule source.
//! - 44,1 et 48 kHz : pondéré E d'ordre 5 (Lipshitz, Wannamaker &
//!   Vanderkooy 1991), −16,6 dB de bruit au continu ;
//! - au-delà : `(1 − z⁻¹)²`, qui pousse le bruit au-dessus de l'audible
//!   (les courbes psychoacoustiques publiées sont calées sur 44,1 kHz).
//!
//! Règles partagées avec la quantification TPDF de T03
//! (`tess::quantification`) :
//! - **pas de requantification, pas de dither** : un bloc dont chaque
//!   échantillon tombe sur un mot exact sort à l'identité (bit-perfect), et
//!   la mémoire d'erreur repart de zéro ;
//! - générateur continu d'un bloc à l'autre, graine fixée à la préparation :
//!   aucune raie à la période du bloc, sortie reproductible ;
//! - **stabilité** : l'erreur réinjectée est bornée à ±2 LSB, comme dans le
//!   SDK ; un signal à 0 dBFS sature au rail sans emballer la boucle.
//!
//! Aucune allocation après [`EtapeFinale::preparer`].

use tune_plugin_surechantillonnage::dither::formeur_par_id;

use crate::audio::dither::{Dither, quantifier_avec};
use crate::tess::etage::{ErreurEtage, EtapeFinale, FormatFlux, SortieFinale, verifier_tampon};

const GRAINE: u64 = 0x5445_5353_5f4e_5307;
/// Ordre maximal des formeurs retenus (pour la mémoire préallouée).
const ORDRE_MAX: usize = 9;
/// Borne de l'erreur réinjectée, en LSB.
const ERREUR_MAX_LSB: f64 = 2.0;

/// L'id du formeur retenu pour une cadence.
pub fn formeur_pour(cadence: u32) -> &'static str {
    match cadence {
        44_100 | 48_000 => "lipshitz5e",
        _ => "ordre2",
    }
}

/// Quantification avec mise en forme du bruit. Voir le module.
pub struct QuantificationMiseEnForme {
    coefficients: [f64; ORDRE_MAX],
    ordre: usize,
    /// Anneaux d'erreur : `canaux × ordre` cases, préalloués.
    erreurs: Vec<f64>,
    positions: Vec<usize>,
    generateur: Dither,
    canaux: usize,
    max_trames: usize,
    formeur: &'static str,
    prepare: bool,
}

impl Default for QuantificationMiseEnForme {
    fn default() -> Self {
        Self::new()
    }
}

impl QuantificationMiseEnForme {
    pub fn new() -> Self {
        Self {
            coefficients: [0.0; ORDRE_MAX],
            ordre: 0,
            erreurs: Vec::new(),
            positions: Vec::new(),
            generateur: Dither::depuis_graine(GRAINE),
            canaux: 0,
            max_trames: 0,
            formeur: "",
            prepare: false,
        }
    }

    /// Le formeur retenu à la préparation.
    pub fn formeur(&self) -> &'static str {
        self.formeur
    }

    fn remettre_a_zero(&mut self) {
        self.erreurs.iter_mut().for_each(|e| *e = 0.0);
        self.positions.iter_mut().for_each(|p| *p = 0);
    }
}

fn grille(bits: u8) -> Option<(f64, f64, f64, u32)> {
    match bits {
        16 | 24 | 32 => {
            let echelle = (1u64 << (bits - 1)) as f64;
            Some((echelle, -echelle, echelle - 1.0, u32::from(32 - bits)))
        }
        _ => None,
    }
}

impl EtapeFinale for QuantificationMiseEnForme {
    fn preparer(&mut self, entree: FormatFlux, max_trames: usize) -> Result<usize, ErreurEtage> {
        if entree.channels == 0 {
            return Err(ErreurEtage::FormatNonPrisEnCharge {
                channels: entree.channels,
                sample_rate_hz: entree.sample_rate_hz,
            });
        }
        let id = formeur_pour(entree.sample_rate_hz);
        let formeur = formeur_par_id(id).ok_or(ErreurEtage::Reglages("formeur inconnu"))?;
        if formeur.coefficients.len() > ORDRE_MAX {
            return Err(ErreurEtage::Reglages("formeur d'ordre trop élevé"));
        }
        self.ordre = formeur.coefficients.len();
        self.coefficients = [0.0; ORDRE_MAX];
        self.coefficients[..self.ordre].copy_from_slice(&formeur.coefficients);
        self.formeur = id;
        self.canaux = usize::from(entree.channels);
        self.max_trames = max_trames;
        self.erreurs = vec![0.0; self.canaux * self.ordre];
        self.positions = vec![0; self.canaux];
        self.generateur = Dither::depuis_graine(GRAINE);
        self.prepare = true;
        Ok(max_trames * self.canaux)
    }

    fn finaliser(
        &mut self,
        entree: &[f64],
        trames: usize,
        sortie: SortieFinale<'_>,
    ) -> Result<usize, ErreurEtage> {
        if !self.prepare {
            return Err(ErreurEtage::NonPrepare);
        }
        if trames > self.max_trames {
            return Err(ErreurEtage::BlocTropGrand {
                trames,
                max: self.max_trames,
            });
        }
        let SortieFinale::Pcm { echantillons, bits } = sortie else {
            return Err(ErreurEtage::ModeNonPrisEnCharge);
        };
        let Some((echelle, min, max, decalage)) = grille(bits) else {
            return Err(ErreurEtage::ModeNonPrisEnCharge);
        };
        let canaux = self.canaux;
        verifier_tampon(entree.len(), trames, canaux as u16)?;
        verifier_tampon(echantillons.len(), trames, canaux as u16)?;
        let n = trames * canaux;
        let entree = &entree[..n];
        let exact = entree.iter().all(|x| {
            let v = x * echelle;
            v.fract() == 0.0 && (min..=max).contains(&v)
        });
        if exact {
            // Pas de requantification, pas de dither : le mot source revient.
            self.remettre_a_zero();
            for (mot, x) in echantillons[..n].iter_mut().zip(entree) {
                *mot = (((x * echelle) as i64) << decalage) as i32;
            }
            return Ok(n);
        }
        let ordre = self.ordre;
        for (i, (mot, x)) in echantillons[..n].iter_mut().zip(entree).enumerate() {
            let canal = i % canaux;
            let base = canal * ordre;
            let mut v = x * echelle;
            if ordre > 0 {
                let p = self.positions[canal];
                let mut retro = 0.0;
                for k in 0..ordre {
                    retro += self.coefficients[k] * self.erreurs[base + (p + ordre - k) % ordre];
                }
                v -= retro;
            }
            let q = quantifier_avec(v, self.generateur.tirer(), min, max);
            if ordre > 0 {
                let p = (self.positions[canal] + 1) % ordre;
                self.erreurs[base + p] = (q as f64 - v).clamp(-ERREUR_MAX_LSB, ERREUR_MAX_LSB);
                self.positions[canal] = p;
            }
            *mot = (q << decalage) as i32;
        }
        Ok(n)
    }

    fn reinitialiser(&mut self) {
        self.remettre_a_zero();
        self.generateur = Dither::depuis_graine(GRAINE);
    }
}
