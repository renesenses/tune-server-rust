//! L'anneau entre le rappel de capture (fil temps réel du pilote) et le
//! lecteur que la pompe de l'hôte appelle.
//!
//! Le rappel POUSSE ce qu'il reçoit ; s'il n'y a plus de place, l'excédent est
//! perdu et compté comme DÉBORDEMENT — jamais d'attente dans le fil du pilote.
//! Le lecteur ATTEND des données, sans jamais en inventer.
//!
//! L'anneau porte aussi ce que `/etat` rapporte du signal : crête sur la
//! dernière seconde, durée du silence numérique en cours (le symptôme d'une
//! autorisation macOS refusée), et les points (instant de capture, trames
//! captées) d'où la dérive se mesure.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::format::Mesure;

/// Comment l'anneau a été fermé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fin {
    /// Arrêt demandé : le lecteur rend `Ok(0)`.
    Normale,
    /// La capture repart sur un autre format (changement de fréquence) : le
    /// lecteur tient la session en silence jusqu'à son remplacement.
    Relance,
    /// Le périphérique a failli (débranché) : le lecteur rend l'erreur.
    Erreur(String),
}

struct Etat {
    donnees: VecDeque<u8>,
    fin: Option<Fin>,
    /// Génération du lecteur en droit de lire : un lecteur remplacé s'efface.
    generation: u64,
    /// Trames captées depuis le démarrage (perdues comprises : c'est le débit
    /// du PÉRIPHÉRIQUE que la dérive compare).
    trames_captees: u64,
    /// Dernier point de capture : (instant du premier échantillon du bloc,
    /// trames captées AVANT ce bloc).
    dernier_point: Option<(Duration, u64)>,
    crete_courante: f32,
    crete_precedente: f32,
    debut_fenetre_crete: Instant,
    /// Trames nulles consécutives, en cours.
    trames_nulles: u64,
    /// Depuis quand plus aucun lecteur n'écoute (`None` : un lecteur lit).
    sans_lecteur_depuis: Option<Instant>,
}

pub struct Anneau {
    etat: Mutex<Etat>,
    signal: Condvar,
    pub octets_par_trame: usize,
    pub frequence: u32,
    capacite: usize,
    /// Événements de débordement (un bloc au moins partiellement perdu).
    pub debordements: AtomicU64,
    pub trames_perdues: AtomicU64,
    /// Blocs où un préambule IEC 61937 a été vu (flux compressé non décodé).
    pub blocs_iec61937: AtomicU64,
    demarrage: Instant,
}

impl Anneau {
    /// `capacite_ms` d'audio au format servi.
    pub fn new(frequence: u32, octets_par_trame: usize, capacite_ms: u64) -> Self {
        let trames = (frequence as u64 * capacite_ms / 1000).max(1) as usize;
        Self {
            etat: Mutex::new(Etat {
                donnees: VecDeque::with_capacity(trames * octets_par_trame),
                fin: None,
                generation: 0,
                trames_captees: 0,
                dernier_point: None,
                crete_courante: 0.0,
                crete_precedente: 0.0,
                debut_fenetre_crete: Instant::now(),
                trames_nulles: 0,
                sans_lecteur_depuis: Some(Instant::now()),
            }),
            signal: Condvar::new(),
            octets_par_trame,
            frequence,
            capacite: trames * octets_par_trame,
            debordements: AtomicU64::new(0),
            trames_perdues: AtomicU64::new(0),
            blocs_iec61937: AtomicU64::new(0),
            demarrage: Instant::now(),
        }
    }

    fn verrou(&self) -> std::sync::MutexGuard<'_, Etat> {
        self.etat.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Appelé par le rappel du pilote. `instant` : l'instant de capture du
    /// premier échantillon, sur l'horloge du pilote (`None` : horloge hôte).
    pub fn pousser(&self, octets: &[u8], mesure: Mesure, instant: Option<Duration>) {
        let trames = (octets.len() / self.octets_par_trame) as u64;
        let mut e = self.verrou();
        if e.fin.is_some() {
            return;
        }
        let instant = instant.unwrap_or_else(|| self.demarrage.elapsed());
        e.dernier_point = Some((instant, e.trames_captees));
        e.trames_captees += trames;
        if e.sans_lecteur_depuis.is_some() {
            // Personne n'écoute : on garde les plus récentes, sans compter de
            // débordement — rien n'est perdu pour personne.
            let exces = (e.donnees.len() + octets.len()).saturating_sub(self.capacite);
            let exces = exces.div_ceil(self.octets_par_trame) * self.octets_par_trame;
            let n = exces.min(e.donnees.len());
            e.donnees.drain(..n);
        }
        let place = self.capacite.saturating_sub(e.donnees.len());
        let pris = place.min(octets.len()) / self.octets_par_trame * self.octets_par_trame;
        if pris < octets.len() {
            self.debordements.fetch_add(1, Ordering::Relaxed);
            self.trames_perdues.fetch_add(
                ((octets.len() - pris) / self.octets_par_trame) as u64,
                Ordering::Relaxed,
            );
        }
        e.donnees.extend(&octets[..pris]);
        if e.debut_fenetre_crete.elapsed() >= Duration::from_secs(1) {
            e.crete_precedente = e.crete_courante;
            e.crete_courante = 0.0;
            e.debut_fenetre_crete = Instant::now();
        }
        if mesure.crete > e.crete_courante {
            e.crete_courante = mesure.crete;
        }
        if mesure.iec61937 {
            self.blocs_iec61937.fetch_add(1, Ordering::Relaxed);
        }
        if mesure.nul {
            e.trames_nulles += trames;
        } else {
            e.trames_nulles = 0;
        }
        drop(e);
        self.signal.notify_all();
    }

    pub fn fermer(&self, fin: Fin) {
        let mut e = self.verrou();
        if e.fin.is_none() {
            e.fin = Some(fin);
        }
        drop(e);
        self.signal.notify_all();
    }

    pub fn fin(&self) -> Option<Fin> {
        self.verrou().fin.clone()
    }

    /// Un nouveau lecteur prend la main : l'ancien s'efface, le contenu est
    /// vidé (le nouveau flux part de maintenant).
    pub fn nouvelle_lecture(&self) -> u64 {
        let mut e = self.verrou();
        e.generation += 1;
        e.donnees.clear();
        e.sans_lecteur_depuis = None;
        // Les compteurs décrivent la session servie, pas la vie de la capture.
        self.debordements.store(0, Ordering::Relaxed);
        self.trames_perdues.store(0, Ordering::Relaxed);
        self.blocs_iec61937.store(0, Ordering::Relaxed);
        let g = e.generation;
        drop(e);
        self.signal.notify_all();
        g
    }

    /// Le lecteur de `generation` s'en va (sa session est finie).
    pub fn lecteur_parti(&self, generation: u64) {
        let mut e = self.verrou();
        if e.generation == generation && e.sans_lecteur_depuis.is_none() {
            e.sans_lecteur_depuis = Some(Instant::now());
        }
    }

    /// Depuis combien de temps plus aucun lecteur n'écoute.
    pub fn sans_lecteur(&self) -> Option<Duration> {
        self.verrou().sans_lecteur_depuis.map(|t| t.elapsed())
    }

    pub fn generation(&self) -> u64 {
        self.verrou().generation
    }

    /// Octets en attente de lecture.
    pub fn niveau(&self) -> usize {
        self.verrou().donnees.len()
    }

    pub fn trames_captees(&self) -> u64 {
        self.verrou().trames_captees
    }

    pub fn dernier_point(&self) -> Option<(Duration, u64)> {
        self.verrou().dernier_point
    }

    /// Crête sur la dernière seconde écoulée (ou la seconde en cours si elle
    /// est plus forte), pleine échelle.
    pub fn crete(&self) -> f32 {
        let e = self.verrou();
        if e.debut_fenetre_crete.elapsed() >= Duration::from_secs(2) {
            // Plus aucun bloc depuis deux secondes : pas de signal.
            return 0.0;
        }
        e.crete_courante.max(e.crete_precedente)
    }

    /// Durée du silence NUMÉRIQUE en cours (échantillons exactement nuls).
    pub fn silence_numerique(&self) -> Duration {
        let n = self.verrou().trames_nulles;
        Duration::from_secs_f64(n as f64 / self.frequence.max(1) as f64)
    }

    /// Attend qu'au moins `min` octets soient disponibles (ou la fin, ou le
    /// délai), puis en retire au plus `max`, en trames entières.
    ///
    /// `Ok(None)` : délai écoulé sans assez de données. `Err(fin)` : l'anneau
    /// est fermé et vide, ou la génération a changé (`Fin::Normale`).
    pub fn lire(
        &self,
        generation: u64,
        min: usize,
        max: usize,
        delai: Duration,
    ) -> Result<Option<Vec<u8>>, Fin> {
        let echeance = Instant::now() + delai;
        let mut e = self.verrou();
        loop {
            if e.generation != generation {
                return Err(Fin::Normale);
            }
            if let Some(fin) = &e.fin {
                return Err(fin.clone());
            }
            if e.donnees.len() >= min.max(self.octets_par_trame) {
                let n = e.donnees.len().min(max) / self.octets_par_trame * self.octets_par_trame;
                return Ok(Some(e.donnees.drain(..n).collect()));
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

    /// Retire `octets` (trames entières) du début de l'anneau : une reprise.
    pub fn retirer(&self, octets: usize) -> usize {
        let mut e = self.verrou();
        let n = octets.min(e.donnees.len()) / self.octets_par_trame * self.octets_par_trame;
        e.donnees.drain(..n);
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(nul: bool) -> Mesure {
        Mesure {
            crete: 0.5,
            nul,
            iec61937: false,
        }
    }

    #[test]
    fn un_anneau_plein_perd_l_excedent_et_le_compte() {
        // 10 ms à 1 kHz, 4 octets par trame : 10 trames, 40 octets.
        let a = Anneau::new(1_000, 4, 10);
        a.nouvelle_lecture(); // un lecteur écoute : une perte en est une
        a.pousser(&[1u8; 32], m(false), None);
        a.pousser(&[2u8; 16], m(false), None);
        assert_eq!(a.niveau(), 40);
        assert_eq!(a.debordements.load(Ordering::Relaxed), 1);
        assert_eq!(a.trames_perdues.load(Ordering::Relaxed), 2);
        assert_eq!(
            a.trames_captees(),
            12,
            "le débit du périphérique, pertes comprises"
        );
    }

    #[test]
    fn la_lecture_rend_des_trames_entieres_et_attend_sans_inventer() {
        let a = Anneau::new(1_000, 4, 1_000);
        let g = a.nouvelle_lecture();
        assert_eq!(a.lire(g, 4, 100, Duration::from_millis(5)), Ok(None));
        a.pousser(&[7u8; 10], m(false), None); // 2 trames + 2 octets perdus en route
        let v = a
            .lire(g, 4, 100, Duration::from_millis(5))
            .unwrap()
            .unwrap();
        assert_eq!(v.len(), 8);
    }

    #[test]
    fn un_lecteur_remplace_s_efface_et_une_fermeture_se_propage() {
        let a = Anneau::new(1_000, 4, 1_000);
        let g1 = a.nouvelle_lecture();
        let g2 = a.nouvelle_lecture();
        assert_eq!(a.lire(g1, 4, 8, Duration::ZERO), Err(Fin::Normale));
        a.fermer(Fin::Erreur("débranché".into()));
        assert_eq!(
            a.lire(g2, 4, 8, Duration::ZERO),
            Err(Fin::Erreur("débranché".into()))
        );
    }

    #[test]
    fn le_silence_numerique_se_mesure_et_se_remet_a_zero() {
        let a = Anneau::new(1_000, 4, 10_000);
        a.pousser(&[0u8; 4 * 500], m(true), None);
        a.pousser(&[0u8; 4 * 500], m(true), None);
        assert_eq!(a.silence_numerique(), Duration::from_secs(1));
        a.pousser(&[1u8; 4], m(false), None);
        assert_eq!(a.silence_numerique(), Duration::ZERO);
    }

    /// Sans lecteur, l'anneau garde les plus récentes sans compter de
    /// débordement ; un lecteur qui part le dit.
    #[test]
    fn sans_lecteur_rien_n_est_compte_comme_perdu() {
        let a = Anneau::new(1_000, 4, 10);
        assert!(a.sans_lecteur().is_some());
        a.pousser(&[1u8; 40], m(false), None);
        a.pousser(&[2u8; 40], m(false), None);
        assert_eq!(a.niveau(), 40);
        assert_eq!(a.debordements.load(Ordering::Relaxed), 0);
        let g = a.nouvelle_lecture();
        assert!(a.sans_lecteur().is_none());
        a.lecteur_parti(g + 1);
        assert!(
            a.sans_lecteur().is_none(),
            "un ancien lecteur ne compte pas"
        );
        a.lecteur_parti(g);
        assert!(a.sans_lecteur().is_some());
    }
}
