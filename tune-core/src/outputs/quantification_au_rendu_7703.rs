//! #7703 — quantification des callbacks ASIO entiers de repli.
//!
//! L'anneau flottant contient le résultat du DSP. Le callback applique ensuite
//! le volume, puis le TPDF à la profondeur du pilote, juste avant l'arrondi.
//! État conservé entre callbacks : aucune allocation et aucun verrou.
use crate::audio::dither::{Dither, quantifier_avec};

pub(crate) struct QuantificationAuRendu {
    bits: u16,
    echelle: f64,
    dither: Dither,
}

impl QuantificationAuRendu {
    pub(crate) fn new(bits: u16) -> Self {
        assert!(matches!(bits, 16 | 24 | 32));
        Self {
            bits,
            echelle: (1u64 << (bits - 1)) as f64,
            dither: Dither::depuis_graine(0x7703_a510_d17e_0001 ^ u64::from(bits)),
        }
    }

    pub(crate) fn rendre(&mut self, sample: f32, gain: f64) -> i32 {
        let ideal = f64::from(sample) * gain * self.echelle;
        // Pas de dither à 32 bits ni lorsqu'aucun arrondi n'est nécessaire.
        // Cela conserve aussi les mots DoP exacts au gain unité et les zéros.
        let bruit = if self.bits < 32 && ideal.fract() != 0.0 {
            self.dither.tirer()
        } else {
            0.0
        };
        quantifier_avec(ideal, bruit, -self.echelle, self.echelle - 1.0) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "local-audio")]
    fn verifier_le_puits(bits: u16) {
        use crate::outputs::local::RingBuf;
        let mut q = QuantificationAuRendu::new(bits);
        let anneau = RingBuf::new(100_000);
        // Résultat flottant du DSP, puis volume à 3 % au rendu : 0,3 LSB.
        let flottants = vec![(10.0 / q.echelle) as f32; 96_000];
        assert_eq!(anneau.push(&flottants), flottants.len());
        let mut mots = vec![0i32; flottants.len()];
        assert_eq!(
            anneau.pop_mapped(&mut mots, |s| q.rendre(s, 0.03)),
            mots.len()
        );
        let moyenne = mots.iter().map(|&m| f64::from(m)).sum::<f64>() / mots.len() as f64;
        assert!(
            (moyenne - 0.3).abs() < 0.02,
            "repli ASIO {bits} bits : moyenne {moyenne} LSB au lieu de 0,3 ; arrondi sans dither après volume"
        );
        assert!(mots.iter().all(|&m| (-1..=1).contains(&m)));
    }

    #[test]
    #[cfg(feature = "local-audio")]
    fn quantification_7703_repli_asio_16_bits_apres_dsp_et_volume() {
        verifier_le_puits(16);
    }

    #[test]
    #[cfg(feature = "local-audio")]
    fn quantification_7703_repli_asio_24_bits_apres_dsp_et_volume() {
        verifier_le_puits(24);
    }

    #[test]
    fn quantification_7703_mots_exacts_dop_silence_et_32_bits_intacts() {
        let mut q = QuantificationAuRendu::new(24);
        for mot in [0, 1, -1, 0x05_6969, -0x05_9697, 8_388_607, -8_388_608] {
            assert_eq!(q.rendre(mot as f32 / 8_388_608.0, 1.0), mot);
            assert_eq!(q.rendre(mot as f32 / 8_388_608.0, 0.0), 0);
        }
        let mut q = QuantificationAuRendu::new(32);
        for s in [0.0f32, 0.25, -0.5, 1.0, -1.0, 1.0e-6] {
            let attendu = (f64::from(s) * 0.03 * 2_147_483_648.0)
                .round()
                .clamp(-2_147_483_648.0, 2_147_483_647.0) as i32;
            assert_eq!(q.rendre(s, 0.03), attendu);
        }
    }

    #[test]
    fn quantification_7703_les_callbacks_entiers_utilisent_le_puits_dithere() {
        let source = include_str!("asio_exclusive.rs");
        assert_eq!(
            source
                .matches("quantification.rendre(sample, volume)")
                .count(),
            3,
            "les trois callbacks ASIO entiers de repli doivent passer par la quantification finale"
        );
    }
}
