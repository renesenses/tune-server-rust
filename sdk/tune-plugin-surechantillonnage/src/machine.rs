//! Test de la machine : le débit réel de chaque filtre, pour chaque rapport,
//! sur CE processeur, avec le chemin SIMD effectivement choisi. Un filtre
//! est « admis » s'il traite l'audio au moins `marge` fois plus vite que le
//! temps réel ; les autres seront masqués dans l'interface.

use std::time::Instant;

use crate::conception::{Conception, catalogue};
use crate::dither::Prng;
use crate::polyphase::{Surechantillonneur, chemin_simd};

/// Les couples mesurés : rapports entiers puis rationnels.
pub const RAPPORTS: [(u32, u32); 6] = [
    (44_100, 352_800),
    (48_000, 384_000),
    (44_100, 705_600),
    (48_000, 768_000),
    (44_100, 384_000),
    (48_000, 352_800),
];

#[derive(Clone, Debug, PartialEq)]
pub struct Resultat {
    pub filtre: &'static str,
    pub de: u32,
    pub vers: u32,
    /// Durée d'audio traitée par seconde de calcul (× temps réel).
    pub facteur_temps_reel: f64,
    /// Milliards de multiplications-accumulations par seconde obtenues.
    pub gmac_s: f64,
    pub memoire_noyaux_octets: usize,
    pub admis: bool,
}

#[derive(Clone, Debug)]
pub struct RapportMachine {
    pub chemin_simd: &'static str,
    pub canaux: usize,
    pub marge: f64,
    pub duree_test_s: f64,
    pub resultats: Vec<Resultat>,
}

impl RapportMachine {
    /// Les filtres admis pour un couple donné.
    pub fn admis_pour(&self, de: u32, vers: u32) -> Vec<&'static str> {
        self.resultats
            .iter()
            .filter(|r| r.de == de && r.vers == vers && r.admis)
            .map(|r| r.filtre)
            .collect()
    }
}

/// Mesure tout le catalogue sur tous les rapports de référence.
pub fn tester_la_machine(canaux: usize, duree_s: f64, marge: f64) -> RapportMachine {
    tester_la_machine_sur(&catalogue(), &RAPPORTS, canaux, duree_s, marge)
}

/// Mesure un sous-ensemble. Deux passes par couple, la meilleure est gardée
/// (la première paie les défauts de cache).
pub fn tester_la_machine_sur(
    conceptions: &[Conception],
    rapports: &[(u32, u32)],
    canaux: usize,
    duree_s: f64,
    marge: f64,
) -> RapportMachine {
    let mut resultats = Vec::new();
    let trames = (duree_s * 48_000.0) as usize;
    let mut prng = Prng::nouveau(0x7614);
    let bloc: Vec<f64> = (0..1024 * canaux).map(|_| prng.tpdf() * 0.5).collect();
    for c in conceptions {
        for &(de, vers) in rapports {
            let Ok(mut sur) = Surechantillonneur::nouveau(c, de, vers, canaux) else {
                continue;
            };
            let trames_in = trames * de as usize / 48_000;
            let mut sortie = Vec::with_capacity(1024 * canaux * (vers / de + 1) as usize);
            let mut meilleur = f64::INFINITY;
            for _ in 0..2 {
                let debut = Instant::now();
                let mut faites = 0;
                while faites < trames_in {
                    sortie.clear();
                    sur.traiter(&bloc, &mut sortie);
                    faites += 1024;
                }
                meilleur = meilleur.min(debut.elapsed().as_secs_f64());
            }
            let audio_s = trames_in as f64 / de as f64;
            let facteur = audio_s / meilleur;
            let mac = sur.mac_par_trame_de_sortie() * vers as f64 * canaux as f64 * audio_s;
            resultats.push(Resultat {
                filtre: c.id,
                de,
                vers,
                facteur_temps_reel: facteur,
                gmac_s: mac / meilleur / 1e9,
                memoire_noyaux_octets: sur.memoire_noyaux_octets(),
                admis: facteur >= marge,
            });
        }
    }
    RapportMachine {
        chemin_simd: chemin_simd().nom(),
        canaux,
        marge,
        duree_test_s: duree_s,
        resultats,
    }
}
