//! La quantification PCM finale (lot T03) : l'[`EtapeFinale`] qui fait sortir
//! la chaîne du `f64`, et la SEULE quantification d'une chaîne TESS.
//!
//! - Entrée : `f64` entrelacé, pleine échelle ±1,0 (la sortie de
//!   l'[`Executeur`](super::Executeur), volume compris quand T07 l'y place).
//! - Sortie : mots cadrés à gauche sur 32 bits, `bits` significatifs (16, 24
//!   ou 32), comme l'anneau natif.
//! - **Pas de requantification, pas de dither** (règle de `audio::dither`) :
//!   un bloc dont chaque échantillon tombe déjà sur un mot exact sort à
//!   l'identité, sans bruit. C'est ce qui garde le bit-perfect quand la chaîne
//!   est vide ou contournée : le mot source revient tel quel.
//! - Sinon, dither TPDF ±1 LSB AVANT l'arrondi (#7703), à TOUTES les
//!   profondeurs : en `f64`, un mot 32 bits n'est plus « plus fin que le
//!   signal ». Le générateur est continu d'un bloc à l'autre (graine fixée à
//!   la préparation) : aucune raie à la période du bloc, et une sortie
//!   reproductible.
//! - `dither: false` (réglage `quantization.dither = off`) : arrondi nu.
//! - La mise en forme du bruit (`noise_shaped`) et le modulateur DSD sont
//!   l'affaire de T07 et T11b.

use super::etage::{ErreurEtage, EtapeFinale, FormatFlux, SortieFinale, verifier_tampon};
use crate::audio::dither::{Dither, quantifier_avec};

const GRAINE: u64 = 0x5445_5353_5f51_0003;

/// Quantification PCM finale. Voir le module.
pub struct QuantificationPcm {
    dither: bool,
    generateur: Dither,
    canaux: u16,
    max_trames: usize,
    prepare: bool,
}

impl QuantificationPcm {
    pub fn new(dither: bool) -> Self {
        Self {
            dither,
            generateur: Dither::depuis_graine(GRAINE),
            canaux: 0,
            max_trames: 0,
            prepare: false,
        }
    }
}

/// Échelle, bornes et décalage d'une profondeur PCM.
fn grille(bits: u8) -> Option<(f64, f64, f64, u32)> {
    match bits {
        16 | 24 | 32 => {
            let echelle = (1u64 << (bits - 1)) as f64;
            Some((echelle, -echelle, echelle - 1.0, u32::from(32 - bits)))
        }
        _ => None,
    }
}

impl EtapeFinale for QuantificationPcm {
    fn preparer(&mut self, entree: FormatFlux, max_trames: usize) -> Result<usize, ErreurEtage> {
        self.canaux = entree.channels;
        self.max_trames = max_trames;
        self.generateur = Dither::depuis_graine(GRAINE);
        self.prepare = true;
        Ok(max_trames * usize::from(entree.channels))
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
        verifier_tampon(entree.len(), trames, self.canaux)?;
        verifier_tampon(echantillons.len(), trames, self.canaux)?;
        let n = trames * usize::from(self.canaux);
        let entree = &entree[..n];
        let exact = entree.iter().all(|x| {
            let v = x * echelle;
            v.fract() == 0.0 && (min..=max).contains(&v)
        });
        let dither = self.dither && !exact;
        for (mot, x) in echantillons[..n].iter_mut().zip(entree) {
            let bruit = if dither { self.generateur.tirer() } else { 0.0 };
            let q = quantifier_avec(x * echelle, bruit, min, max);
            // `q` tient dans `bits` bits signés : le décalage le cadre à gauche
            // sans débordement.
            *mot = ((q as i64) << decalage) as i32;
        }
        Ok(n)
    }

    fn reinitialiser(&mut self) {
        self.generateur = Dither::depuis_graine(GRAINE);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STEREO: FormatFlux = FormatFlux {
        channels: 2,
        sample_rate_hz: 44_100,
    };

    fn quantifier(q: &mut QuantificationPcm, x: &[f64], bits: u8) -> Vec<i32> {
        let mut mots = vec![0; x.len()];
        let n = q
            .finaliser(
                x,
                x.len() / 2,
                SortieFinale::Pcm {
                    echantillons: &mut mots,
                    bits,
                },
            )
            .unwrap();
        assert_eq!(n, x.len());
        mots
    }

    /// Bit-perfect : des mots source exacts (16, 24 et 32 bits) reviennent à
    /// l'identité, sans bruit, même dither armé. Contre-épreuve : un gain de
    /// −1 dB requantifie, et le dither est présent.
    #[test]
    fn des_mots_exacts_reviennent_a_l_identite_et_un_gain_requantifie() {
        for bits in [16u8, 24, 32] {
            let echelle = (1u64 << (bits - 1)) as f64;
            let max = (echelle - 1.0) as i64;
            let mots: Vec<i64> = vec![0, 1, -1, max, -max - 1, 12_345, -(max / 3)];
            let mut source: Vec<f64> = mots.iter().map(|&m| m as f64 / echelle).collect();
            source.push(0.0);
            let mut q = QuantificationPcm::new(true);
            q.preparer(STEREO, 4).unwrap();
            let sortie = quantifier(&mut q, &source, bits);
            let attendu: Vec<i32> = source
                .iter()
                .map(|x| (((x * echelle) as i64) << (32 - bits)) as i32)
                .collect();
            assert_eq!(sortie, attendu, "{bits} bits : identité attendue");

            let gain = 10f64.powf(-1.0 / 20.0);
            let attenue: Vec<f64> = source.iter().map(|x| x * gain).collect();
            let avec = quantifier(&mut q, &attenue, bits);
            let mut sans_dither = QuantificationPcm::new(false);
            sans_dither.preparer(STEREO, 4).unwrap();
            let nu = quantifier(&mut sans_dither, &attenue, bits);
            assert_ne!(
                avec, nu,
                "{bits} bits : le dither doit être présent à −1 dB"
            );
        }
    }

    /// Le dither TPDF est borné à ±1 LSB et de moyenne nulle.
    #[test]
    fn le_dither_reste_dans_un_lsb() {
        let mut q = QuantificationPcm::new(true);
        q.preparer(STEREO, 4_096).unwrap();
        let x: Vec<f64> = (0..8_192).map(|i| 0.25 + (i as f64) * 1e-9).collect();
        let mots = quantifier(&mut q, &x, 16);
        let ideal: Vec<f64> = x.iter().map(|v| v * 32_768.0).collect();
        let mut somme = 0.0;
        for (m, i) in mots.iter().zip(&ideal) {
            let e = f64::from(*m >> 16) - i;
            assert!(e.abs() <= 1.5, "erreur {e}");
            somme += e;
        }
        assert!((somme / mots.len() as f64).abs() < 0.05);
    }

    #[test]
    fn refus_sans_panique() {
        let mut q = QuantificationPcm::new(true);
        let mut mots = [0; 4];
        assert_eq!(
            q.finaliser(
                &[0.0; 4],
                2,
                SortieFinale::Pcm {
                    echantillons: &mut mots,
                    bits: 24
                }
            ),
            Err(ErreurEtage::NonPrepare)
        );
        q.preparer(STEREO, 2).unwrap();
        assert_eq!(
            q.finaliser(
                &[0.0; 4],
                2,
                SortieFinale::Pcm {
                    echantillons: &mut mots,
                    bits: 20
                }
            ),
            Err(ErreurEtage::ModeNonPrisEnCharge)
        );
        assert_eq!(
            q.finaliser(
                &[0.0; 4],
                3,
                SortieFinale::Pcm {
                    echantillons: &mut mots,
                    bits: 24
                }
            ),
            Err(ErreurEtage::BlocTropGrand { trames: 3, max: 2 })
        );
        let mut octets = [0u8; 4];
        assert_eq!(
            q.finaliser(
                &[0.0; 4],
                2,
                SortieFinale::Dsd {
                    octets: &mut octets,
                    taux: crate::tess::TauxDsd::Dsd64
                }
            ),
            Err(ErreurEtage::ModeNonPrisEnCharge)
        );
    }
}
