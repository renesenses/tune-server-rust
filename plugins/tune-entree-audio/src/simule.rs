//! Des entrées simulées, pour prouver le contrôleur, le lecteur et les routes
//! sans matériel. Chaque capture pousse, au rythme réel, un signal connu : le
//! compteur des trames produites, sur 16 bits stéréo (les deux voies égales).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tune_core::source_pcm::FormatPcm;

use crate::anneau::Anneau;
use crate::format::Mesure;
use crate::peripheriques::{Arret, CaptureDemarree, DescriptionEntree, Peripheriques};

pub struct EntreeSimulee {
    pub nom: String,
    pub canaux: u16,
    pub frequence: Arc<AtomicU32>,
    /// `true` : la capture rend des zéros (autorisation macOS refusée).
    pub zeros: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct Simulees {
    pub entrees: Mutex<Vec<EntreeSimulee>>,
    /// Captures démarrées, dans l'ordre : (nom, fréquence).
    pub demarrages: Mutex<Vec<(String, u32)>>,
    /// Tant que vrai, `format_natif` ne rend pas la main (CoreAudio muet).
    pub muet: AtomicBool,
}

impl Simulees {
    pub fn avec(nom: &str, frequence: u32) -> Arc<Self> {
        Self::avec_canaux(nom, frequence, 2)
    }

    /// Une entrée de `canaux` voies : la voie `c` de la trame `n` vaut
    /// `n · 8 + c` (16 bits).
    pub fn avec_canaux(nom: &str, frequence: u32, canaux: u16) -> Arc<Self> {
        let s = Arc::new(Self::default());
        s.entrees.lock().unwrap().push(EntreeSimulee {
            canaux,
            nom: nom.into(),
            frequence: Arc::new(AtomicU32::new(frequence)),
            zeros: Arc::default(),
        });
        s
    }

    pub fn regler_frequence(&self, nom: &str, f: u32) {
        for e in self.entrees.lock().unwrap().iter() {
            if e.nom == nom {
                e.frequence.store(f, Ordering::SeqCst);
            }
        }
    }

    pub fn rendre_des_zeros(&self, nom: &str) {
        for e in self.entrees.lock().unwrap().iter() {
            if e.nom == nom {
                e.zeros.store(true, Ordering::SeqCst);
            }
        }
    }

    fn trouver(&self, nom: &str) -> Result<(u32, Arc<AtomicBool>, u16), String> {
        self.entrees
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.nom == nom)
            .map(|e| {
                (
                    e.frequence.load(Ordering::SeqCst),
                    e.zeros.clone(),
                    e.canaux,
                )
            })
            .ok_or_else(|| format!("aucune entrée audio « {nom} »"))
    }
}

pub fn format(frequence: u32, canaux: u16) -> FormatPcm {
    FormatPcm {
        frequence,
        canaux,
        bits: 16,
    }
}

struct ArretSimule(Arc<AtomicBool>);
impl Arret for ArretSimule {
    fn arreter(self: Box<Self>) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl Peripheriques for Simulees {
    fn pile(&self) -> &'static str {
        "simule"
    }

    fn lister(&self) -> Result<Vec<DescriptionEntree>, String> {
        Ok(self
            .entrees
            .lock()
            .unwrap()
            .iter()
            .map(|e| DescriptionEntree {
                nom: e.nom.clone(),
                id: format!("simule:{}", e.nom),
                canaux: e.canaux,
                frequences: vec![44_100, 48_000],
                frequence_courante: Some(e.frequence.load(Ordering::SeqCst)),
                formats: vec!["i16".into()],
                bits_physiques: Some(16),
                bits_servis: 16,
                par_defaut: false,
                virtuelle: Some(e.nom.contains("Loopback")),
            })
            .collect())
    }

    fn format_natif(&self, entree: &str) -> Result<(String, FormatPcm), String> {
        while self.muet.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let (f, _, c) = self.trouver(entree)?;
        Ok((entree.to_string(), format(f, c)))
    }

    fn demarrer(&self, entree: &str, anneau: Arc<Anneau>) -> Result<CaptureDemarree, String> {
        let (f, zeros, canaux) = self.trouver(entree)?;
        self.demarrages
            .lock()
            .unwrap()
            .push((entree.to_string(), f));
        let vivant = Arc::new(AtomicBool::new(true));
        let v = vivant.clone();
        std::thread::spawn(move || {
            let par_bloc = (f / 100) as usize; // 10 ms
            let mut n: u32 = 0;
            while v.load(Ordering::SeqCst) {
                let mut o = Vec::with_capacity(par_bloc * 2 * canaux as usize);
                let nul = zeros.load(Ordering::SeqCst);
                for _ in 0..par_bloc {
                    for c in 0..canaux as u32 {
                        let s = if nul {
                            0u16
                        } else if canaux == 2 {
                            n as u16
                        } else {
                            (n.wrapping_mul(8) + c) as u16
                        };
                        o.extend_from_slice(&s.to_le_bytes());
                    }
                    n = n.wrapping_add(1);
                }
                anneau.pousser(
                    &o,
                    Mesure {
                        crete: if nul { 0.0 } else { 0.5 },
                        nul,
                        iec61937: false,
                    },
                    None,
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        Ok(CaptureDemarree {
            nom: entree.to_string(),
            format: format(f, canaux),
            arret: Box::new(ArretSimule(vivant)),
        })
    }

    fn frequence_courante(&self, entree: &str) -> Option<u32> {
        self.trouver(entree).ok().map(|(f, _, _)| f)
    }
}
