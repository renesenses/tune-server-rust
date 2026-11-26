//! rc14 — un micro SIMULÉ qui capte à sa propre fréquence (44,1, 96 kHz…)
//! et à sa propre horloge, face à une zone qui joue à 48 kHz.
//!
//! La pièce reste simulée à la cadence de la zone ([`PieceEnDirect`], 48 kHz) ;
//! ce que le micro capte en est un rééchantillonnage CONTINU (sinc fenêtré),
//! au rapport `frequence / (48 000 · (1 + derive_ppm·10⁻⁶))` : un micro dont
//! le quartz va plus vite que celui de la sortie tire plus de son par
//! seconde, comme un UMIK-1 sur son propre port USB.
//!
//! [`PieceEnDirect`]: super::PieceEnDirect

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tune_core::source_pcm::FormatPcm;
use tune_entree_audio::anneau::Anneau;
use tune_entree_audio::format::Mesure;
use tune_entree_audio::peripheriques::{CaptureDemarree, DescriptionEntree, Peripheriques};

use super::{ArretSimule, Boucle, format};

/// Demi-largeur du noyau sinc, en trames jouées.
const DEMI_NOYAU: usize = 24;

/// Le micro à cadence propre.
pub struct MicroACadence {
    pub nom: String,
    /// Sa fréquence nominale de capture.
    pub frequence: u32,
    /// L'écart de son horloge sur celle de la zone (ppm, + : plus rapide).
    pub derive_ppm: f64,
    /// La fréquence à laquelle la zone joue (celle de la pièce simulée).
    pub frequence_zone: u32,
    pub boucle: Arc<Boucle>,
}

/// Le rééchantillonneur continu : un historique de trames jouées et la
/// position (fractionnaire) de la prochaine trame captée.
struct Interpolateur {
    pas: f64,
    coupure: f64,
    histoire: Vec<f64>,
    position: f64,
}

impl Interpolateur {
    fn new(frequence_zone: u32, frequence: u32, derive_ppm: f64) -> Self {
        let pas = frequence_zone as f64 * (1.0 + derive_ppm * 1e-6) / frequence as f64;
        Self {
            pas,
            // Coupure relative à la cadence jouée : la plus basse des deux
            // Nyquist, avec une marge pour la transition.
            coupure: 0.95 * (1.0 / pas).min(1.0),
            // Le noyau est centré : DEMI_NOYAU trames de silence d'avance.
            histoire: vec![0.0; DEMI_NOYAU],
            position: DEMI_NOYAU as f64,
        }
    }

    /// Les trames jouées qu'il faut encore pour sortir `n` trames captées.
    fn besoin(&self, n: usize) -> usize {
        let dernier = self.position + self.pas * (n.saturating_sub(1)) as f64;
        (dernier.floor() as usize + DEMI_NOYAU + 1).saturating_sub(self.histoire.len())
    }

    fn noyau(&self, x: f64) -> f64 {
        let a = DEMI_NOYAU as f64;
        if x.abs() >= a {
            return 0.0;
        }
        let px = std::f64::consts::PI * self.coupure * x;
        let sinc = if px.abs() < 1e-12 { 1.0 } else { px.sin() / px };
        // Blackman sur [−a, a].
        let u = std::f64::consts::PI * (x / a + 1.0);
        let w = 0.42 - 0.5 * u.cos() + 0.08 * (2.0 * u).cos();
        self.coupure * sinc * w
    }

    fn sortir(&mut self, joue: &[f64], n: usize) -> Vec<f64> {
        self.histoire.extend_from_slice(joue);
        let mut sortie = Vec::with_capacity(n);
        for _ in 0..n {
            let centre = self.position.floor() as usize;
            let mut y = 0.0;
            for k in centre + 1 - DEMI_NOYAU..=centre + DEMI_NOYAU {
                y += self.histoire[k] * self.noyau(self.position - k as f64);
            }
            sortie.push(y);
            self.position += self.pas;
        }
        // On oublie ce que le noyau ne touchera plus.
        let oubli = (self.position.floor() as usize).saturating_sub(DEMI_NOYAU);
        if oubli > 0 {
            self.histoire.drain(..oubli);
            self.position -= oubli as f64;
        }
        sortie
    }
}

impl Peripheriques for MicroACadence {
    fn pile(&self) -> &'static str {
        "simule"
    }

    fn lister(&self) -> Result<Vec<DescriptionEntree>, String> {
        Ok(vec![DescriptionEntree {
            nom: self.nom.clone(),
            id: format!("simule:{}", self.nom),
            canaux: 2,
            frequences: vec![self.frequence],
            frequence_courante: Some(self.frequence),
            formats: vec!["i24".into()],
            bits_physiques: Some(24),
            bits_servis: 24,
            par_defaut: false,
            virtuelle: Some(false),
        }])
    }

    fn format_natif(&self, entree: &str) -> Result<(String, FormatPcm), String> {
        if entree == self.nom || entree == format!("simule:{}", self.nom) {
            Ok((self.nom.clone(), format(self.frequence)))
        } else {
            Err(format!("aucune entrée audio « {entree} »"))
        }
    }

    fn demarrer(&self, entree: &str, anneau: Arc<Anneau>) -> Result<CaptureDemarree, String> {
        let (nom, f) = self.format_natif(entree)?;
        let vivant = Arc::new(AtomicBool::new(true));
        let v = vivant.clone();
        let boucle = self.boucle.clone();
        let mut interp = Interpolateur::new(self.frequence_zone, self.frequence, self.derive_ppm);
        let par_bloc = (self.frequence / 100) as usize; // 10 ms
        std::thread::spawn(move || {
            let pause = Duration::from_secs_f64(0.010 / boucle.vitesse);
            while v.load(Ordering::SeqCst) {
                let Some(joue) = boucle.capter_bloc(interp.besoin(par_bloc)) else {
                    std::thread::sleep(Duration::from_micros(50));
                    continue;
                };
                let captes = interp.sortir(&joue, par_bloc);
                let mut o = Vec::with_capacity(par_bloc * 6);
                let mut crete = 0.0f32;
                for x in &captes {
                    let x = x.clamp(-1.0, 1.0);
                    crete = crete.max(x.abs() as f32);
                    let n = (x * 8_388_607.0).round() as i32;
                    o.extend_from_slice(&n.to_le_bytes()[..3]);
                    o.extend_from_slice(&[0, 0, 0]);
                }
                anneau.pousser(
                    &o,
                    Mesure {
                        crete,
                        nul: crete == 0.0,
                        iec61937: false,
                    },
                    None,
                );
                std::thread::sleep(pause);
            }
        });
        Ok(CaptureDemarree {
            id: format!("simule:{nom}"),
            nom,
            format: f,
            arret: Box::new(ArretSimule(vivant)),
        })
    }

    fn frequence_courante(&self, _: &str) -> Option<u32> {
        Some(self.frequence)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un sinus de 1 kHz joué à 48 kHz ressort à la fréquence du micro, même
    /// amplitude : l'interpolateur ne colore pas la bande mesurée.
    #[test]
    fn l_interpolateur_rend_le_sinus_a_la_cadence_du_micro() {
        for fs in [44_100u32, 96_000] {
            let mut i = Interpolateur::new(48_000, fs, 0.0);
            let joue: Vec<f64> = (0..48_000)
                .map(|n| (2.0 * std::f64::consts::PI * 1_000.0 * n as f64 / 48_000.0).sin())
                .collect();
            let n = (fs as usize) / 2;
            let besoin = i.besoin(n);
            let y = i.sortir(&joue[..besoin], n);
            // Le noyau est centré : la trame captée k est la trame jouée k·pas.
            let pire = (1_000..n)
                .map(|k| {
                    let t = k as f64 / fs as f64;
                    (y[k] - (2.0 * std::f64::consts::PI * 1_000.0 * t).sin()).abs()
                })
                .fold(0.0, f64::max);
            assert!(pire < 2e-3, "{fs} Hz : écart {pire}");
        }
    }
}
