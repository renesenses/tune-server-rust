//! rc14 — la mesure à la fréquence NATIVE du micro (44,1 à 192 kHz).
//!
//! ## Le choix : capter à la fréquence du micro, analyser à 48 kHz
//!
//! Une interface calée sur une horloge externe (word clock, S/PDIF), ou
//! fixée à 44,1 ou 96 kHz, ne peut pas passer à 48 kHz pour la mesure. On ne
//! lui demande plus : la capture se fait à sa fréquence, et le segment capté
//! est rééchantillonné vers [`FREQUENCE_MESURE`] avant l'analyse, par le
//! rééchantillonneur sinc du serveur
//! (`tune_core::audio::resample::rubato_resample_batch_exact` : bande
//! passante à −0,1 dB au-delà de 20,6 kHz, réjection sous −110 dB, retard de
//! groupe retiré — la RI garde son instant zéro).
//!
//! Tout le reste ne bouge pas : le balayage est toujours joué à 48 kHz (la
//! zone l'adapte à sa sortie comme n'importe quelle piste), et l'analyse —
//! déconvolution, grille de fréquences, filtres d'octave, RT60, étalonnage du
//! micro (indexé en Hz), correction C2/C3 — tourne à 48 kHz comme avant. Jouer
//! ET capter à la fréquence native aurait changé la cadence du générateur, de
//! la RI exportée et des coefficients de correction, et dépendu de ce que la
//! sortie de la zone accepte : beaucoup plus de surface pour le même résultat.
//!
//! ## L'horloge
//!
//! Le rapport appliqué est le rapport NOMINAL (44 100 → 48 000…). L'écart
//! réel entre l'horloge du micro et celle de la sortie — quelques dizaines de
//! ppm entre deux appareils USB (`tune_entree_audio::derive` : −6 ppm mesurés
//! sur un Yeti X, 40 au pire) — n'est pas corrigé, exactement comme
//! aujourd'hui avec un UMIK-1 séparé à 48 kHz : sur un balayage de 20 s,
//! 40 ppm font 0,8 ms d'étirement, que la déconvolution d'un balayage
//! exponentiel absorbe (léger étalement de la RI dans l'aigu, aucun effet sur
//! la réponse dans la bande de correction). Les essais le vérifient
//! (`tests/mesure_frequences.rs`).

use tune_core::audio::resample::rubato_resample_batch_exact;

use crate::mesure::FREQUENCE_MESURE;

/// Les fréquences de capture acceptées par la mesure.
pub const FREQUENCES_ACCEPTEES: [u32; 6] = [44_100, 48_000, 88_200, 96_000, 176_400, 192_000];

/// La mesure peut-elle capter à `frequence` ?
pub fn acceptee(frequence: u32) -> bool {
    FREQUENCES_ACCEPTEES.contains(&frequence)
}

/// Le refus d'une entrée à une fréquence non prise en charge, ou rien.
pub fn verifier(nom_entree: &str, frequence: u32) -> Result<(), String> {
    if acceptee(frequence) {
        return Ok(());
    }
    let liste: Vec<String> = FREQUENCES_ACCEPTEES.iter().map(u32::to_string).collect();
    Err(format!(
        "l'entrée « {nom_entree} » capte à {frequence} Hz : la mesure accepte {} Hz (régler le périphérique)",
        liste.join(", ")
    ))
}

/// Le passage entre les trames du micro et celles de l'analyse (48 kHz).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cadence {
    /// La fréquence de capture du micro.
    pub captee: u32,
}

impl Cadence {
    pub fn de(captee: u32) -> Self {
        Self {
            captee: captee.max(1),
        }
    }

    /// Des trames du micro, comptées en trames d'analyse (par défaut).
    pub fn vers_analyse(&self, trames_captees: u64) -> u64 {
        (trames_captees as u128 * FREQUENCE_MESURE as u128 / self.captee as u128) as u64
    }

    /// Des trames d'analyse, comptées en trames du micro (par excès : la
    /// fenêtre captée couvre toujours la fenêtre analysée).
    pub fn vers_capture(&self, trames_analyse: u64) -> u64 {
        (trames_analyse as u128 * self.captee as u128).div_ceil(FREQUENCE_MESURE as u128) as u64
    }

    /// Un segment capté (une voie), ramené à la cadence d'analyse et à
    /// exactement `trames_analyse` trames. Identité à 48 kHz.
    pub fn a_l_analyse(&self, capte: Vec<f64>, trames_analyse: usize) -> Vec<f64> {
        let mut x = if self.captee == FREQUENCE_MESURE {
            capte
        } else {
            let f: Vec<f32> = capte.iter().map(|v| *v as f32).collect();
            rubato_resample_batch_exact(&f, self.captee, FREQUENCE_MESURE, 1)
                .into_iter()
                .map(f64::from)
                .collect()
        };
        x.resize(trames_analyse, 0.0);
        x
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_six_frequences_et_pas_les_autres() {
        for f in FREQUENCES_ACCEPTEES {
            assert!(verifier("X", f).is_ok(), "{f}");
        }
        let e = verifier("TASCAM US-366", 32_000).unwrap_err();
        assert!(e.contains("32000 Hz"), "{e}");
        assert!(
            e.contains("44100, 48000, 88200, 96000, 176400, 192000"),
            "{e}"
        );
        assert!(!acceptee(22_050) && !acceptee(384_000));
    }

    #[test]
    fn les_trames_passent_d_une_cadence_a_l_autre() {
        let c = Cadence::de(44_100);
        assert_eq!(c.vers_capture(48_000), 44_100);
        assert_eq!(c.vers_analyse(44_100), 48_000);
        // Par excès vers la capture, par défaut vers l'analyse.
        assert_eq!(c.vers_capture(1), 1);
        assert_eq!(c.vers_analyse(1), 1);
        let c = Cadence::de(192_000);
        assert_eq!(c.vers_capture(3 * 48_000), 3 * 192_000);
        assert_eq!(Cadence::de(48_000).vers_capture(12_345), 12_345);
    }

    /// Un sinus de 1 kHz capté à 96 kHz ressort à 48 kHz, même amplitude,
    /// même phase (retard de groupe retiré), à la longueur demandée.
    #[test]
    fn un_segment_capte_ressort_a_48_khz_sans_retard() {
        let s = |fs: f64, n: usize| -> Vec<f64> {
            (0..n)
                .map(|i| 0.5 * (2.0 * std::f64::consts::PI * 1_000.0 * i as f64 / fs).sin())
                .collect()
        };
        for fs in [44_100u32, 96_000, 192_000] {
            let c = Cadence::de(fs);
            let n = c.vers_capture(48_000) as usize;
            let y = c.a_l_analyse(s(fs as f64, n), 48_000);
            assert_eq!(y.len(), 48_000);
            let attendu = s(48_000.0, 48_000);
            let pire = y[4_800..43_200]
                .iter()
                .zip(&attendu[4_800..43_200])
                .map(|(a, b)| (a - b).abs())
                .fold(0.0, f64::max);
            assert!(pire < 1e-3, "{fs} Hz : écart {pire}");
        }
        let x = vec![0.25; 100];
        assert_eq!(Cadence::de(48_000).a_l_analyse(x.clone(), 100), x);
    }
}
