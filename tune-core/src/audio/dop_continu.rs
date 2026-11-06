//! #7704 — la continuité d'un porteur DoP **côté rappel de rendu**.
//!
//! Un DAC qui reçoit du DoP (`audio::dsd_to_dop`) bascule en DSD tant que
//! l'octet haut de chaque mot 24 bits alterne 0x05 / 0xFA. Les rappels de la
//! sortie locale comblaient toute période sans données — pause, arrêt, silence
//! forcé, pré-remplissage après une recherche, famine — par des ZÉROS : plus
//! de marqueur, le DAC sort du DSD, puis le redétecte à la reprise et lit
//! entre-temps les trames DoP comme du PCM (le « ploc » DoP ; un DAC lent à
//! basculer peut lire des zéros comme du DSD, c'est-à-dire la pleine échelle
//! négative). Note « Upsampler Pro » (#7614), prérequis 2.
//!
//! Ce module tient l'état que le rappel garde d'une période à l'autre :
//!
//! - il **observe** les trames réellement émises et reconnaît le porteur à la
//!   même règle que `outputs::local::is_dop_pcm` (32 trames alternées, même
//!   marqueur sur tous les canaux) ;
//! - tant que le porteur est reconnu, il **comble** en trames de repos DoP :
//!   charge utile 0x69 (densité 50 %, jamais des zéros), marqueurs qui
//!   continuent l'alternance ;
//! - à la reprise, si la première trame réelle porterait le même marqueur que
//!   la dernière émise, il **raccorde** par une trame de repos (5,7 µs en
//!   DSD64), pour que l'alternance ne se rompe jamais.
//!
//! Rien ici ne touche un flux PCM : tant que le porteur n'est pas reconnu, le
//! comblement reste des zéros, comme avant. Pur, sans allocation, sans verrou :
//! tenable dans le rappel temps réel.

use super::dsd_to_dop::{MARQUEUR_IMPAIR, MARQUEUR_PAIR, marqueur_suivant, mot_de_repos};

/// Trames alternées consécutives au-delà desquelles le flux est tenu pour DoP.
/// Même seuil que la sonde du producteur (`DOP_DETECT_FRAMES`).
pub const SEUIL_DE_RECONNAISSANCE: u32 = 32;

/// L'état de continuité DoP d'un rappel de rendu.
#[derive(Debug, Clone)]
pub struct SuiviDoP {
    canaux: usize,
    consecutives: u32,
    dernier: Option<u8>,
}

impl SuiviDoP {
    #[must_use]
    pub fn new(canaux: u16) -> Self {
        Self {
            canaux: usize::from(canaux.max(1)),
            consecutives: 0,
            dernier: None,
        }
    }

    #[must_use]
    pub fn canaux(&self) -> usize {
        self.canaux
    }

    /// Le flux émis est-il un porteur DoP ?
    #[must_use]
    pub fn actif(&self) -> bool {
        self.consecutives >= SEUIL_DE_RECONNAISSANCE
    }

    /// Le dernier marqueur émis vers le DAC, s'il y en a un.
    #[must_use]
    pub fn dernier_marqueur(&self) -> Option<u8> {
        self.dernier
    }

    /// Note les trames réellement émises. `haut` rend l'octet haut du mot
    /// 24 bits que porte l'échantillon.
    pub fn observer<T: Copy>(&mut self, emis: &[T], haut: impl Fn(T) -> u8) {
        for trame in emis.chunks_exact(self.canaux) {
            let m = haut(trame[0]);
            let porte = (m == MARQUEUR_PAIR || m == MARQUEUR_IMPAIR)
                && trame[1..].iter().all(|&s| haut(s) == m);
            if porte && self.dernier != Some(m) {
                self.consecutives = self.consecutives.saturating_add(1);
                self.dernier = Some(m);
            } else if porte {
                // Marqueur répété : alternance rompue, on recompte depuis ici.
                self.consecutives = 1;
            } else {
                self.consecutives = 0;
                self.dernier = None;
            }
        }
    }

    /// Comble `sortie` : trames de repos DoP qui continuent l'alternance si le
    /// porteur est reconnu, sinon `zero` partout (le PCM garde son silence).
    /// Rend `true` si le comblement est du repos DoP.
    pub fn combler<T: Copy>(&mut self, sortie: &mut [T], zero: T, repos: impl Fn(u8) -> T) -> bool {
        let Some(mut m) = self.dernier.filter(|_| self.actif()) else {
            sortie.fill(zero);
            return false;
        };
        let mut trames = 0u32;
        for trame in sortie.chunks_mut(self.canaux) {
            m = marqueur_suivant(m);
            trame.fill(repos(m));
            trames = trames.saturating_add(1);
        }
        self.dernier = Some(m);
        self.consecutives = self.consecutives.saturating_add(trames);
        true
    }

    /// Avant de tirer des trames réelles : si la première (`premier`, lue
    /// sans la consommer) porterait le marqueur de la dernière trame émise,
    /// pose UNE trame de repos en tête de `sortie` et rend sa longueur, à
    /// sauter par l'appelant. Sinon rend 0 et ne touche rien.
    pub fn raccorder<T: Copy>(
        &mut self,
        premier: Option<T>,
        sortie: &mut [T],
        haut: impl Fn(T) -> u8,
        repos: impl Fn(u8) -> T,
    ) -> usize {
        let ch = self.canaux;
        match premier {
            Some(p) if self.actif() && sortie.len() > ch && self.dernier == Some(haut(p)) => {
                // `zero` n'est pas lu : le porteur est actif.
                let z = repos(MARQUEUR_PAIR);
                self.combler(&mut sortie[..ch], z, repos);
                ch
            }
            _ => 0,
        }
    }
}

/// Octet haut du mot 24 bits porté par un `f32` (anneau flottant : un mot
/// 24 bits divisé par 2²³, exact dans la mantisse).
#[must_use]
pub fn haut_f32(s: f32) -> u8 {
    (((s * 8_388_608.0) as i32) >> 16) as u8
}

/// La trame de repos DoP en `f32`.
#[must_use]
pub fn repos_f32(marqueur: u8) -> f32 {
    mot_de_repos(marqueur) as f32 / 8_388_608.0
}

/// Octet haut du mot 24 bits porté par un `i32` aligné à gauche (anneau natif).
#[must_use]
pub fn haut_i32(w: i32) -> u8 {
    (w >> 24) as u8
}

/// La trame de repos DoP en `i32` aligné à gauche.
#[must_use]
pub fn repos_i32(marqueur: u8) -> i32 {
    mot_de_repos(marqueur) << 8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::dsd_to_dop::DsdToDoP;

    #[test]
    fn la_trame_de_repos_porte_0x69_et_son_marqueur() {
        for m in [MARQUEUR_PAIR, MARQUEUR_IMPAIR] {
            let w = mot_de_repos(m);
            assert_eq!((w & 0xFF) as u8, 0x69);
            assert_eq!(((w >> 8) & 0xFF) as u8, 0x69);
            assert_eq!(haut_i32(repos_i32(m)), m);
            assert_eq!(haut_f32(repos_f32(m)), m);
        }
    }

    /// Un PCM ordinaire n'est jamais pris pour du DoP : son silence reste zéro.
    #[test]
    fn un_flux_pcm_garde_des_zeros() {
        let mut s = SuiviDoP::new(2);
        let pcm: Vec<f32> = (0..4096).map(|i| (i as f32 * 0.01).sin() * 0.5).collect();
        s.observer(&pcm, haut_f32);
        let mut sortie = vec![1.0f32; 64];
        assert!(!s.combler(&mut sortie, 0.0, repos_f32));
        assert!(sortie.iter().all(|&v| v == 0.0));
    }

    /// Une piste au nombre de trames impair se clôt sur 0xFA : la suivante,
    /// qui commence par 0x05, enchaîne sans rupture d'alternance.
    #[test]
    fn la_cloture_rend_le_compte_de_trames_pair() {
        let mut a = DsdToDoP::new(2, false);
        let mut flux = a.feed(&[0xAA; 2 * 2 * 33]); // 33 trames
        assert_eq!(flux.len(), 33 * 6);
        flux.extend_from_slice(&a.terminer());
        assert_eq!(flux.len(), 34 * 6, "une trame de repos ajoutée");
        assert_eq!(
            &flux[flux.len() - 6..],
            &[0x69, 0x69, 0xFA, 0x69, 0x69, 0xFA]
        );
        let mut b = DsdToDoP::new(2, false);
        flux.extend_from_slice(&b.feed(&[0x55; 2 * 2 * 4]));
        flux.extend_from_slice(&b.terminer());
        let marqueurs: Vec<u8> = flux.chunks_exact(6).map(|t| t[2]).collect();
        for paire in marqueurs.windows(2) {
            assert_ne!(paire[0], paire[1], "alternance rompue à la frontière");
        }
        // Compte pair : rien n'est ajouté.
        assert!(DsdToDoP::new(2, false).terminer().is_empty());
    }
}
