//! Consommateur « vers tampon » (#7632, calibration C1) : une capture dont
//! les échantillons sont ACCUMULÉS en mémoire, en flottants, pour un
//! analyseur — à côté de `LecteurCapture`, qui sert une entrée en direct à
//! une zone et ne garde rien.
//!
//! Même `Peripheriques`, même `Anneau`, même rappel du pilote : seul le
//! consommateur change. Le lecteur en direct tire l'anneau au rythme de la
//! sortie et applique une politique de reprise ; ce consommateur-ci tire tout
//! ce qui arrive, aussitôt, et le range par voie. Une mesure de pièce dure
//! quelques dizaines de secondes : à 48 kHz stéréo en f64, c'est 23 Mo par
//! minute — borné par `capacite_max_s`.
//!
//! Il ne connaît ni le balayage ni la déconvolution : il rend des trames, un
//! compteur, et sait attendre qu'il y en ait assez.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tune_core::source_pcm::FormatPcm;

use crate::anneau::{Anneau, Fin};
use crate::peripheriques::{Arret, Peripheriques};

/// Anneau de 10 s : le fil de rangement tire sans attendre, l'anneau n'est
/// qu'un sas entre le rappel du pilote et lui.
const CAPACITE_ANNEAU_MS: u64 = 10_000;

struct Etat {
    /// Une suite d'échantillons par voie, pleine échelle (±1).
    voies: Vec<Vec<f64>>,
    /// Trames rangées (toutes voies).
    trames: u64,
    /// Trames refusées parce que `capacite_max` était atteinte.
    trames_ecartees: u64,
    fin: Option<Fin>,
}

/// Les échantillons accumulés, partagés entre le fil de rangement et
/// l'analyseur.
pub struct Tampon {
    etat: Mutex<Etat>,
    signal: Condvar,
    format: FormatPcm,
    capacite_max: u64,
}

impl Tampon {
    fn verrou(&self) -> std::sync::MutexGuard<'_, Etat> {
        self.etat.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn format(&self) -> FormatPcm {
        self.format
    }

    /// Trames rangées jusqu'ici.
    pub fn trames(&self) -> u64 {
        self.verrou().trames
    }

    pub fn trames_ecartees(&self) -> u64 {
        self.verrou().trames_ecartees
    }

    /// La fin de la capture, si elle est survenue.
    pub fn fin(&self) -> Option<Fin> {
        self.verrou().fin.clone()
    }

    /// Attend que `n` trames au moins soient rangées. `Ok(Some(trames))` ;
    /// `Ok(None)` : délai écoulé ; `Err(fin)` si la capture s'est terminée
    /// avant (le tampon reste lisible).
    pub fn attendre_trames(&self, n: u64, delai: Duration) -> Result<Option<u64>, Fin> {
        let echeance = Instant::now() + delai;
        let mut e = self.verrou();
        loop {
            if e.trames >= n {
                return Ok(Some(e.trames));
            }
            if let Some(fin) = &e.fin {
                return Err(fin.clone());
            }
            let reste = echeance.saturating_duration_since(Instant::now());
            if reste.is_zero() {
                return Ok(None);
            }
            e = self
                .signal
                .wait_timeout(e, reste)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    /// Les échantillons de la voie `voie`, trames `[depuis, depuis + n)`,
    /// tronqués à ce qui existe.
    pub fn extraire(&self, voie: usize, depuis: u64, n: usize) -> Vec<f64> {
        let e = self.verrou();
        let Some(v) = e.voies.get(voie) else {
            return Vec::new();
        };
        let debut = (depuis as usize).min(v.len());
        let fin = debut.saturating_add(n).min(v.len());
        v[debut..fin].to_vec()
    }

    /// Crête (pleine échelle, linéaire) de la voie depuis la trame `depuis`.
    pub fn crete_depuis(&self, voie: usize, depuis: u64) -> f64 {
        let e = self.verrou();
        e.voies
            .get(voie)
            .map(|v| {
                v.iter()
                    .skip(depuis as usize)
                    .fold(0.0f64, |m, x| m.max(x.abs()))
            })
            .unwrap_or(0.0)
    }

    fn ranger(&self, octets: &[u8]) {
        let canaux = self.format.canaux as usize;
        let par_ech = (self.format.bits / 8) as usize;
        let par_trame = canaux * par_ech;
        if par_trame == 0 {
            return;
        }
        let echelle = if self.format.bits == 16 {
            32_768.0
        } else {
            8_388_608.0
        };
        let mut e = self.verrou();
        if e.fin.is_some() {
            return;
        }
        for trame in octets.chunks_exact(par_trame) {
            if e.trames >= self.capacite_max {
                e.trames_ecartees += 1;
                continue;
            }
            for (c, ech) in trame.chunks_exact(par_ech).enumerate() {
                let v = match par_ech {
                    2 => i16::from_le_bytes([ech[0], ech[1]]) as f64,
                    3 => (i32::from_le_bytes([0, ech[0], ech[1], ech[2]]) >> 8) as f64,
                    _ => (i32::from_le_bytes([ech[0], ech[1], ech[2], ech[3]]) >> 8) as f64,
                };
                e.voies[c].push(v / echelle);
            }
            e.trames += 1;
        }
        drop(e);
        self.signal.notify_all();
    }

    fn terminer(&self, fin: Fin) {
        let mut e = self.verrou();
        if e.fin.is_none() {
            e.fin = Some(fin);
        }
        drop(e);
        self.signal.notify_all();
    }
}

/// Une capture démarrée vers un [`Tampon`].
pub struct CaptureVersTampon {
    nom: String,
    tampon: Arc<Tampon>,
    anneau: Arc<Anneau>,
    arret: Mutex<Option<Box<dyn Arret>>>,
    vivant: Arc<AtomicBool>,
    fil: Mutex<Option<JoinHandle<()>>>,
}

impl CaptureVersTampon {
    /// Démarre `entree` à son format natif et range tout ce qu'elle rend,
    /// jusqu'à `capacite_max_s` secondes.
    pub fn demarrer(
        peripheriques: &Arc<dyn Peripheriques>,
        entree: &str,
        capacite_max_s: f64,
    ) -> Result<Self, String> {
        let (nom, format) = peripheriques.format_natif(entree)?;
        if format.bits != 16 && format.bits != 24 {
            return Err(format!(
                "format servi {} bits : le tampon attend 16 ou 24 bits",
                format.bits
            ));
        }
        let opt = format.octets_par_trame() as usize;
        let anneau = Arc::new(Anneau::new(format.frequence, opt, CAPACITE_ANNEAU_MS));
        let capture = peripheriques.demarrer(entree, anneau.clone())?;
        if capture.format != format {
            capture.arret.arreter();
            return Err(format!(
                "le format de « {nom} » a changé pendant l'ouverture ({:?} → {:?})",
                format, capture.format
            ));
        }
        let tampon = Arc::new(Tampon {
            etat: Mutex::new(Etat {
                voies: (0..format.canaux).map(|_| Vec::new()).collect(),
                trames: 0,
                trames_ecartees: 0,
                fin: None,
            }),
            signal: Condvar::new(),
            format,
            capacite_max: (capacite_max_s.max(1.0) * format.frequence as f64) as u64,
        });
        let vivant = Arc::new(AtomicBool::new(true));
        let generation = anneau.nouvelle_lecture();
        let fil = {
            let (anneau, tampon, vivant) = (anneau.clone(), tampon.clone(), vivant.clone());
            std::thread::Builder::new()
                .name("calibration-tampon".into())
                .spawn(move || {
                    while vivant.load(Ordering::SeqCst) {
                        match anneau.lire(generation, 1, 1 << 18, Duration::from_millis(200)) {
                            Ok(Some(octets)) => tampon.ranger(&octets),
                            Ok(None) => {}
                            Err(fin) => {
                                tampon.terminer(fin);
                                return;
                            }
                        }
                    }
                    tampon.terminer(Fin::Normale);
                })
                .map_err(|e| format!("fil de rangement : {e}"))?
        };
        tracing::info!(
            entree = %capture.nom,
            frequence = format.frequence,
            bits = format.bits,
            canaux = format.canaux,
            pile = peripheriques.pile(),
            "calibration_capture_vers_tampon_demarree"
        );
        Ok(Self {
            nom: capture.nom,
            tampon,
            anneau,
            arret: Mutex::new(Some(capture.arret)),
            vivant,
            fil: Mutex::new(Some(fil)),
        })
    }

    pub fn nom(&self) -> &str {
        &self.nom
    }

    pub fn tampon(&self) -> &Arc<Tampon> {
        &self.tampon
    }

    /// Débordements de l'anneau : des trames perdues entre le pilote et le
    /// tampon, donc une mesure à rejeter.
    pub fn debordements(&self) -> u64 {
        self.anneau.debordements.load(Ordering::Relaxed)
    }

    /// Arrête la capture ; le tampon reste lisible.
    pub fn arreter(&self) {
        self.vivant.store(false, Ordering::SeqCst);
        self.anneau.fermer(Fin::Normale);
        if let Some(a) = self.arret.lock().unwrap_or_else(|e| e.into_inner()).take() {
            a.arreter();
        }
        if let Some(f) = self.fil.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = f.join();
        }
    }
}

impl Drop for CaptureVersTampon {
    fn drop(&mut self) {
        self.arreter();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simule::Simulees;

    /// L'entrée simulée compte ses trames (16 bits, les deux voies égales) :
    /// le tampon doit les rendre dans l'ordre, sans trou, en flottants.
    #[test]
    fn le_tampon_range_les_trames_captees_dans_l_ordre() {
        let p: Arc<dyn Peripheriques> = Simulees::avec("Micro USB", 48_000);
        let c = CaptureVersTampon::demarrer(&p, "Micro USB", 5.0).unwrap();
        assert_eq!(c.tampon().format().canaux, 2);
        let n = c
            .tampon()
            .attendre_trames(2_000, Duration::from_secs(5))
            .unwrap()
            .expect("délai");
        assert!(n >= 2_000);
        let v = c.tampon().extraire(0, 0, 2_000);
        assert_eq!(v.len(), 2_000);
        for (i, paire) in v.windows(2).enumerate() {
            let (a, b) = (paire[0] * 32_768.0, paire[1] * 32_768.0);
            let suivant = if a >= 32_767.0 { -32_768.0 } else { a + 1.0 };
            assert!((b - suivant).abs() < 0.5, "trou à la trame {i} : {a} → {b}");
        }
        c.arreter();
        assert!(c.tampon().fin().is_some());
        assert_eq!(c.debordements(), 0);
    }

    /// La capacité borne la mémoire : au-delà, les trames sont écartées et
    /// comptées, la capture ne meurt pas.
    #[test]
    fn la_capacite_borne_le_tampon() {
        let p: Arc<dyn Peripheriques> = Simulees::avec("Micro USB", 48_000);
        let c = CaptureVersTampon::demarrer(&p, "Micro USB", 1.0).unwrap();
        // 1 s de capacité : à 48 kHz, 48 000 trames au plus.
        let r = c
            .tampon()
            .attendre_trames(48_000, Duration::from_secs(5))
            .unwrap()
            .expect("délai");
        assert_eq!(r, 48_000);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(c.tampon().trames(), 48_000);
        assert!(c.tampon().trames_ecartees() > 0);
        assert!(c.tampon().fin().is_none());
        c.arreter();
    }

    #[test]
    fn une_entree_inconnue_est_refusee() {
        let p: Arc<dyn Peripheriques> = Simulees::avec("Micro USB", 48_000);
        let e = CaptureVersTampon::demarrer(&p, "Fantôme", 5.0).unwrap_err();
        assert!(e.contains("Fantôme"), "{e}");
    }
}
