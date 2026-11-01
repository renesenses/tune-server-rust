//! Une pièce, une zone et un micro SIMULÉS, pour prouver le déroulé complet
//! sans jouer ni capter un seul son réel.
//!
//! * [`HoteSimule`] tient lieu d'orchestrateur : `jouer` ouvre le
//!   fournisseur « calibration » comme le ferait `resolve_stream`, puis une
//!   pompe tire le `Read` du générateur et dépose les échantillons dans la
//!   [`Boucle`] ; `regler_volume` et `arreter` sont notés.
//! * [`MicroSimule`] est un `Peripheriques` d'entree-audio : sa capture tire
//!   la boucle à `vitesse` fois le temps réel, fait traverser à chaque
//!   échantillon la pièce ([`PieceEnDirect`] : retard, réflexion, modes,
//!   gain du micro, bruit de fond) et pousse du PCM 24 bits dans l'anneau,
//!   exactement comme le rappel cpal.
//!
//! Le temps est celui de la capture : une mesure de 15 s d'audio se déroule
//! en moins d'une seconde à `vitesse = 25`, sans rien changer au code mesuré,
//! qui attend des TRAMES, pas des secondes.

use std::collections::VecDeque;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tune_calibration::eq::Biquad;
use tune_calibration::fft;
use tune_calibration::sim::RoomSim;
use tune_core::source_pcm::{FormatPcm, FournisseurPcm};
use tune_entree_audio::anneau::Anneau;
use tune_entree_audio::format::Mesure;
use tune_entree_audio::peripheriques::{Arret, CaptureDemarree, DescriptionEntree, Peripheriques};

use crate::fournisseur::FournisseurBalayage;
use crate::hote::{ElementBalayage, HoteZone, TraitementsDeZone};

/// La pièce, échantillon par échantillon.
pub struct PieceEnDirect {
    ligne: VecDeque<f64>,
    retard_direct: usize,
    reflexions: Vec<(usize, f64)>,
    gain_direct: f64,
    biquads: Vec<(Biquad, [f64; 4])>,
    /// Gain du micro (1 = nominal ; 40 = micro trop chaud, écrêtage).
    pub gain_micro: f64,
    bruit_amp: f64,
    graine: u64,
}

impl PieceEnDirect {
    pub fn depuis(sim: &RoomSim, gain_micro: f64) -> Self {
        let fs = sim.sample_rate as f64;
        let retard_direct = (sim.direct_delay_s * fs).round() as usize;
        let reflexions: Vec<(usize, f64)> = sim
            .reflections
            .iter()
            .map(|(d, g)| (retard_direct + (d * fs).round() as usize, *g))
            .collect();
        let plus_long = reflexions
            .iter()
            .map(|(d, _)| *d)
            .max()
            .unwrap_or(0)
            .max(retard_direct);
        Self {
            ligne: VecDeque::from(vec![0.0; plus_long + 1]),
            retard_direct,
            reflexions,
            gain_direct: sim.direct_gain,
            biquads: sim
                .modes
                .iter()
                .map(|m| (Biquad::peaking(fs, m.freq, m.gain_db, m.q), [0.0; 4]))
                .collect(),
            gain_micro,
            bruit_amp: if sim.noise_floor_dbfs.is_finite() {
                fft::from_db(sim.noise_floor_dbfs) * 3f64.sqrt()
            } else {
                0.0
            },
            graine: 0x1234_5678_9ABC_DEF0,
        }
    }

    fn bruit(&mut self) -> f64 {
        self.graine ^= self.graine << 13;
        self.graine ^= self.graine >> 7;
        self.graine ^= self.graine << 17;
        ((self.graine >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) * self.bruit_amp
    }

    /// Un échantillon joué entre, un échantillon capté sort.
    pub fn traverser(&mut self, x: f64) -> f64 {
        self.ligne.push_front(x);
        self.ligne.pop_back();
        let mut y = self.ligne[self.retard_direct] * self.gain_direct;
        for (d, g) in &self.reflexions {
            y += self.ligne[*d] * self.gain_direct * g;
        }
        for (b, s) in self.biquads.iter_mut() {
            let y0 = b.b0 * y + b.b1 * s[0] + b.b2 * s[1] - b.a1 * s[2] - b.a2 * s[3];
            s[1] = s[0];
            s[0] = y;
            s[3] = s[2];
            s[2] = y0;
            y = y0;
        }
        (y + self.bruit()) * self.gain_micro
    }
}

/// Ce que la zone a joué et que le micro n'a pas encore capté.
pub struct Boucle {
    joue: Mutex<VecDeque<f64>>,
    piece: Mutex<PieceEnDirect>,
    /// Vitesse du temps simulé (1 = temps réel).
    pub vitesse: f64,
}

const FILE_MAX: usize = 48_000;

impl Boucle {
    pub fn new(piece: PieceEnDirect, vitesse: f64) -> Arc<Self> {
        Arc::new(Self {
            joue: Mutex::new(VecDeque::new()),
            piece: Mutex::new(piece),
            vitesse,
        })
    }

    /// Dépose ce que la zone joue ; bloque si le micro n'a pas suivi.
    pub fn deposer(&self, echantillons: &[f64], vivant: &AtomicBool) {
        for x in echantillons {
            loop {
                {
                    let mut j = self.joue.lock().unwrap();
                    if j.len() < FILE_MAX {
                        j.push_back(*x);
                        break;
                    }
                }
                if !vivant.load(Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_micros(200));
            }
        }
    }

    /// `n` échantillons captés : ce qui a été joué, ou du silence, à travers
    /// la pièce.
    fn capter(&self, n: usize) -> Vec<f64> {
        let mut j = self.joue.lock().unwrap();
        let mut p = self.piece.lock().unwrap();
        (0..n)
            .map(|_| p.traverser(j.pop_front().unwrap_or(0.0)))
            .collect()
    }
}

/// Le micro : une entrée 48 kHz, 2 voies, 24 bits, dont la capture tire la boucle.
pub struct MicroSimule {
    pub nom: String,
    pub frequence: u32,
    pub boucle: Arc<Boucle>,
    pub demarrages: Mutex<usize>,
}

struct ArretSimule(Arc<AtomicBool>);
impl Arret for ArretSimule {
    fn arreter(self: Box<Self>) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn format(frequence: u32) -> FormatPcm {
    FormatPcm {
        frequence,
        canaux: 2,
        bits: 24,
    }
}

impl Peripheriques for MicroSimule {
    fn pile(&self) -> &'static str {
        "simule"
    }

    fn lister(&self) -> Result<Vec<DescriptionEntree>, String> {
        Ok(vec![DescriptionEntree {
            nom: self.nom.clone(),
            id: format!("simule:{}", self.nom),
            canaux: 2,
            frequences: vec![44_100, 48_000],
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
        *self.demarrages.lock().unwrap() += 1;
        let vivant = Arc::new(AtomicBool::new(true));
        let v = vivant.clone();
        let boucle = self.boucle.clone();
        let fs = self.frequence;
        std::thread::spawn(move || {
            let par_bloc = (fs / 100) as usize; // 10 ms
            let pause = Duration::from_secs_f64(0.010 / boucle.vitesse);
            while v.load(Ordering::SeqCst) {
                let captes = boucle.capter(par_bloc);
                let mut o = Vec::with_capacity(par_bloc * 6);
                let mut crete = 0.0f32;
                for x in &captes {
                    let x = x.clamp(-1.0, 1.0);
                    crete = crete.max(x.abs() as f32);
                    let n = (x * 8_388_607.0).round() as i32;
                    o.extend_from_slice(&n.to_le_bytes()[..3]);
                    // Voie 2 : muette (le micro est mono sur la voie 1).
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
            nom,
            format: f,
            arret: Box::new(ArretSimule(vivant)),
        })
    }

    fn frequence_courante(&self, _: &str) -> Option<u32> {
        Some(self.frequence)
    }
}

/// Ce que l'hôte simulé a reçu.
#[derive(Debug, Default)]
pub struct Journal {
    pub volumes: Vec<f64>,
    pub joues: Vec<ElementBalayage>,
    pub arrets: usize,
    pub erreurs_ouverture: Vec<String>,
}

pub struct HoteSimule {
    pub fournisseur: Arc<FournisseurBalayage>,
    pub boucle: Arc<Boucle>,
    /// Le volume que la base « connaît » ; `None` : zone inconnue.
    pub volume: Mutex<Option<f64>>,
    pub traitements: TraitementsDeZone,
    pub journal: Mutex<Journal>,
    pompe_vivante: Arc<AtomicBool>,
}

impl HoteSimule {
    pub fn new(
        fournisseur: Arc<FournisseurBalayage>,
        boucle: Arc<Boucle>,
        volume: Option<f64>,
    ) -> Arc<Self> {
        Arc::new(Self {
            fournisseur,
            boucle,
            volume: Mutex::new(volume),
            traitements: TraitementsDeZone::default(),
            journal: Mutex::new(Journal::default()),
            pompe_vivante: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn journal(&self) -> std::sync::MutexGuard<'_, Journal> {
        self.journal.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl HoteZone for HoteSimule {
    fn volume(&self, _zone_id: i64) -> Option<f64> {
        *self.volume.lock().unwrap()
    }

    fn regler_volume(&self, _zone_id: i64, volume_pct: f64) -> Result<(), String> {
        self.journal().volumes.push(volume_pct);
        *self.volume.lock().unwrap() = Some(volume_pct);
        Ok(())
    }

    fn jouer(&self, _zone_id: i64, element: ElementBalayage) -> Result<(), String> {
        self.journal().joues.push(element.clone());
        let flux = match self.fournisseur.ouvrir(&element.mesure_id, 0) {
            Ok(f) => f,
            Err(e) => {
                self.journal().erreurs_ouverture.push(e.clone());
                return Err(e);
            }
        };
        let mut lecteur = flux.lecteur;
        let par_trame = flux.format.octets_par_trame() as usize;
        let bits = flux.format.bits;
        let vivant = self.pompe_vivante.clone();
        vivant.store(true, Ordering::SeqCst);
        let boucle = self.boucle.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; par_trame * 480];
            let mut echantillons = Vec::with_capacity(480);
            while vivant.load(Ordering::SeqCst) {
                let n = match lecteur.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                echantillons.clear();
                for t in buf[..n].chunks_exact(par_trame) {
                    // Voie gauche de la zone.
                    let v = if bits == 16 {
                        i16::from_le_bytes([t[0], t[1]]) as f64 / 32_768.0
                    } else {
                        (i32::from_le_bytes([0, t[0], t[1], t[2]]) >> 8) as f64 / 8_388_608.0
                    };
                    echantillons.push(v);
                }
                boucle.deposer(&echantillons, &vivant);
            }
        });
        Ok(())
    }

    fn arreter(&self, _zone_id: i64) {
        self.journal().arrets += 1;
        self.pompe_vivante.store(false, Ordering::SeqCst);
    }

    fn traitements(&self, _zone_id: i64) -> TraitementsDeZone {
        self.traitements.clone()
    }
}
