//! #8696 — le fil de rendu WASAPI exclusif réveillé ~39 ms en retard.
//!
//! ## Le fait (fil 2805, rc11)
//!
//! Au lancement d'un album, une session sur deux : MMCSS « Pro Audio » obtenu,
//! anneau plein (`0 événement` de famine), et pourtant le fil de rendu se
//! réveille avec 4 périodes d'écart (`ecart_us≈39000`, `periode_us=10000`,
//! `reveils_en_retard` 43 → 64 en une minute). Arrêter puis relancer — donc
//! recréer le fil — guérit.
//!
//! ## L'hypothèse retenue
//!
//! Processeur hybride (cœurs P/E) et bridage d'énergie de Windows (EcoQoS,
//! « Power Throttling ») : un fil jugé peu important peut être placé sur un
//! cœur lent ou à fréquence réduite, et MMCSS ne l'en exempte pas toujours.
//! Ce module porte ce qui se décide hors FFI, pour être jugé par `cargo test`
//! sur toutes les plateformes :
//!
//! 1. l'état passé à `SetThreadInformation(ThreadPowerThrottling, …)` qui
//!    REFUSE le bridage de vitesse d'exécution (`ControlMask=EXECUTION_SPEED`,
//!    `StateMask=0`) ;
//! 2. la détection d'un retard PERSISTANT (des réveils en retard sans répit
//!    pendant plusieurs secondes), qui mérite une ligne de journal claire ;
//! 3. le relevé des compteurs de la session EN COURS, pour le rapport de bogue
//!    (jusqu'ici `reveils_en_retard` ne sortait qu'à l'arrêt de la session).
//!
//! ⚠️ Ce module ne prouve pas que le bridage soit la cause : seul un essai sur
//! la machine du testeur le dira. Il rend la cause supposée inopérante et la
//! mesure lisible pendant la panne.

// Hors Windows, seuls le registre (vide) et les témoins servent.
#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// `THREAD_INFORMATION_CLASS::ThreadPowerThrottling` (processthreadsapi.h :
/// `ThreadMemoryPriority=0`, `ThreadAbsoluteCpuPriority=1`,
/// `ThreadDynamicCodePolicy=2`, `ThreadPowerThrottling=3`).
pub(crate) const THREAD_POWER_THROTTLING: i32 = 3;

/// `THREAD_POWER_THROTTLING_CURRENT_VERSION`.
pub(crate) const THREAD_POWER_THROTTLING_CURRENT_VERSION: u32 = 1;

/// `THREAD_POWER_THROTTLING_EXECUTION_SPEED`.
pub(crate) const THREAD_POWER_THROTTLING_EXECUTION_SPEED: u32 = 0x1;

/// `THREAD_POWER_THROTTLING_STATE`, tel que l'attend `SetThreadInformation`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EtatDuBridage {
    pub version: u32,
    pub control_mask: u32,
    pub state_mask: u32,
}

/// L'état qui REFUSE le bridage : le bit `EXECUTION_SPEED` est pris en main
/// (`control_mask`) et laissé à zéro (`state_mask`). Un `state_mask` à 1
/// DEMANDERAIT au contraire l'EcoQoS ; un `control_mask` à 0 rendrait la
/// décision au système — exactement la situation du fil 2805.
pub(crate) const fn refus_du_bridage() -> EtatDuBridage {
    EtatDuBridage {
        version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
        control_mask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        state_mask: 0,
    }
}

/// Résolution du minuteur demandée pendant la lecture (`timeBeginPeriod`),
/// en millisecondes. Rendue (`timeEndPeriod`) à la sortie du fil.
pub(crate) const RESOLUTION_MINUTEUR_MS: u32 = 1;

/// Au-delà de cet intervalle sans réveil en retard, l'épisode est clos.
pub(crate) const REPIT_MS: u64 = 5_000;

/// Durée d'un épisode de retards au-delà de laquelle il est dit PERSISTANT.
/// Dans le fil 2805 : ~4 réveils en retard toutes les 6,5 s pendant plus
/// d'une minute — bien au-delà.
pub(crate) const PERSISTANCE_MS: u64 = 10_000;

/// Détecte un épisode de réveils en retard qui dure : des retards dont aucun
/// n'est séparé du précédent de plus de [`REPIT_MS`], pendant au moins
/// [`PERSISTANCE_MS`]. Un seul signal par épisode.
#[derive(Debug, Clone)]
pub(crate) struct PersistanceDesRetards {
    repit_ticks: u64,
    persistance_ticks: u64,
    debut: Option<u64>,
    dernier: Option<u64>,
    signale: bool,
}

/// Ce que dit un réveil en retard sur l'épisode en cours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Episode {
    /// Rien à signaler (épisode trop court, ou déjà signalé).
    EnCours,
    /// L'épisode vient de franchir la durée de persistance : à journaliser,
    /// une seule fois. `duree_ticks` : depuis le premier retard de l'épisode.
    DevientPersistant { duree_ticks: u64 },
}

impl PersistanceDesRetards {
    /// `frequence` : `QueryPerformanceFrequency`. `None` si elle est nulle.
    pub(crate) fn nouveau(frequence: i64) -> Option<Self> {
        if frequence <= 0 {
            return None;
        }
        let ms = |n: u64| (n as u128 * frequence as u128 / 1_000) as u64;
        Some(Self {
            repit_ticks: ms(REPIT_MS),
            persistance_ticks: ms(PERSISTANCE_MS),
            debut: None,
            dernier: None,
            signale: false,
        })
    }

    /// À appeler à chaque réveil COMPTÉ en retard, avec l'instant QPC.
    pub(crate) fn retard(&mut self, maintenant: u64) -> Episode {
        let nouvel_episode = match self.dernier {
            None => true,
            Some(d) => match maintenant.checked_sub(d) {
                Some(ecart) => ecart > self.repit_ticks,
                None => true, // horloge qui recule : on repart
            },
        };
        if nouvel_episode {
            self.debut = Some(maintenant);
            self.signale = false;
        }
        self.dernier = Some(maintenant);
        let debut = self.debut.unwrap_or(maintenant);
        let duree = maintenant.saturating_sub(debut);
        if !self.signale && duree >= self.persistance_ticks {
            self.signale = true;
            return Episode::DevientPersistant { duree_ticks: duree };
        }
        Episode::EnCours
    }

    /// Vrai si l'épisode en cours a été déclaré persistant.
    #[cfg(test)]
    pub(crate) fn est_persistant(&self) -> bool {
        self.signale
    }
}

/// Les compteurs vivants d'une session WASAPI exclusive, partagés avec le
/// fil de rendu.
#[derive(Debug)]
struct SessionVivante {
    jeton: u64,
    peripherique: String,
    periode_us: u64,
    reveils_en_retard: Arc<AtomicU64>,
    ecart_max_us: Arc<AtomicU64>,
    underruns: Arc<AtomicU64>,
    deadline_misses: Arc<AtomicU64>,
    persistant: Arc<AtomicBool>,
    bridage_refuse: Option<bool>,
}

static SESSIONS: Mutex<Vec<SessionVivante>> = Mutex::new(Vec::new());
static JETON: AtomicU64 = AtomicU64::new(1);

/// Le relevé d'une session en cours, tel que le lit le rapport de bogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleveSessionWasapi {
    pub peripherique: String,
    pub periode_us: u64,
    pub reveils_en_retard: u64,
    pub ecart_max_us: u64,
    pub underruns: u64,
    pub deadline_misses: u64,
    /// Un épisode de retards a dépassé [`PERSISTANCE_MS`] dans cette session.
    pub retards_persistants: bool,
    /// `Some(true)` : le refus du bridage d'énergie a été accepté par Windows ;
    /// `Some(false)` : refusé (Windows antérieur à 10 1709, etc.) ; `None` :
    /// sans objet (hors Windows).
    pub bridage_refuse: Option<bool>,
}

/// Les compteurs qu'une session inscrit au registre.
pub(crate) struct CompteursSession {
    pub peripherique: String,
    pub periode_us: u64,
    pub reveils_en_retard: Arc<AtomicU64>,
    pub ecart_max_us: Arc<AtomicU64>,
    pub underruns: Arc<AtomicU64>,
    pub deadline_misses: Arc<AtomicU64>,
    pub persistant: Arc<AtomicBool>,
    pub bridage_refuse: Option<bool>,
}

/// Tant qu'elle vit, la session figure au relevé ; à sa chute, elle en sort.
#[must_use]
pub(crate) struct SessionInscrite {
    jeton: u64,
}

impl Drop for SessionInscrite {
    fn drop(&mut self) {
        let mut sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        sessions.retain(|s| s.jeton != self.jeton);
    }
}

/// Inscrit une session au registre. Appelé une fois par le fil de rendu à
/// son démarrage — jamais dans la boucle.
pub(crate) fn inscrire_session(c: CompteursSession) -> SessionInscrite {
    let jeton = JETON.fetch_add(1, Ordering::Relaxed);
    let mut sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    sessions.push(SessionVivante {
        jeton,
        peripherique: c.peripherique,
        periode_us: c.periode_us,
        reveils_en_retard: c.reveils_en_retard,
        ecart_max_us: c.ecart_max_us,
        underruns: c.underruns,
        deadline_misses: c.deadline_misses,
        persistant: c.persistant,
        bridage_refuse: c.bridage_refuse,
    });
    SessionInscrite { jeton }
}

/// Le relevé des sessions WASAPI exclusives EN COURS (vide hors Windows ou
/// quand rien ne joue). Lu par le rapport de bogue.
pub fn releve_des_sessions_en_cours() -> Vec<ReleveSessionWasapi> {
    let sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    sessions
        .iter()
        .map(|s| ReleveSessionWasapi {
            peripherique: s.peripherique.clone(),
            periode_us: s.periode_us,
            reveils_en_retard: s.reveils_en_retard.load(Ordering::Relaxed),
            ecart_max_us: s.ecart_max_us.load(Ordering::Relaxed),
            underruns: s.underruns.load(Ordering::Relaxed),
            deadline_misses: s.deadline_misses.load(Ordering::Relaxed),
            retards_persistants: s.persistant.load(Ordering::Relaxed),
            bridage_refuse: s.bridage_refuse,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10 MHz : un tick = 100 ns, 1 ms = 10 000 ticks.
    const F: i64 = 10_000_000;
    const MS: u64 = 10_000;

    #[test]
    fn l_etat_refuse_le_bridage_de_vitesse_d_execution() {
        let e = refus_du_bridage();
        assert_eq!(e.version, 1);
        assert_eq!(
            e.control_mask, 0x1,
            "EXECUTION_SPEED doit être pris en main"
        );
        assert_eq!(e.state_mask, 0, "un 1 ici DEMANDERAIT l'EcoQoS");
        assert_eq!(std::mem::size_of::<EtatDuBridage>(), 12);
        assert_eq!(THREAD_POWER_THROTTLING, 3);
    }

    #[test]
    fn des_retards_isoles_ne_sont_jamais_persistants() {
        let mut p = PersistanceDesRetards::nouveau(F).unwrap();
        // Un retard toutes les 6 s : chaque fois un nouvel épisode.
        for i in 0..20u64 {
            assert_eq!(p.retard(i * 6_000 * MS), Episode::EnCours);
        }
        assert!(!p.est_persistant());
    }

    #[test]
    fn le_rythme_du_fil_2805_devient_persistant_une_seule_fois() {
        // ~4 retards toutes les 6,5 s : un toutes les 1 625 ms.
        let mut p = PersistanceDesRetards::nouveau(F).unwrap();
        let mut signaux = Vec::new();
        for i in 0..60u64 {
            let t = i * 1_625 * MS;
            if let Episode::DevientPersistant { duree_ticks } = p.retard(t) {
                signaux.push((t, duree_ticks));
            }
        }
        assert_eq!(signaux.len(), 1, "un seul signal par épisode");
        let (t, duree) = signaux[0];
        assert!(duree >= PERSISTANCE_MS * MS);
        assert!(t < 12_000 * MS, "signalé dès que les 10 s sont franchies");
    }

    #[test]
    fn un_repit_clot_l_episode_et_en_permet_un_nouveau_signal() {
        let mut p = PersistanceDesRetards::nouveau(F).unwrap();
        let mut n = 0;
        for i in 0..12u64 {
            if matches!(p.retard(i * 1_000 * MS), Episode::DevientPersistant { .. }) {
                n += 1;
            }
        }
        assert_eq!(n, 1);
        // 6 s sans retard : répit.
        let base = 11_000 * MS + 6_000 * MS;
        assert_eq!(p.retard(base), Episode::EnCours);
        assert!(!p.est_persistant());
        for i in 1..=10u64 {
            if matches!(
                p.retard(base + i * 1_000 * MS),
                Episode::DevientPersistant { .. }
            ) {
                n += 1;
            }
        }
        assert_eq!(n, 2);
    }

    #[test]
    fn une_horloge_qui_recule_ouvre_un_nouvel_episode() {
        let mut p = PersistanceDesRetards::nouveau(F).unwrap();
        p.retard(50_000 * MS);
        assert_eq!(p.retard(1_000 * MS), Episode::EnCours);
        assert!(PersistanceDesRetards::nouveau(0).is_none());
    }

    #[test]
    fn la_session_en_cours_figure_au_releve_puis_en_sort() {
        let retards = Arc::new(AtomicU64::new(0));
        let persistant = Arc::new(AtomicBool::new(false));
        let nom = "témoin-8696-registre";
        let inscription = inscrire_session(CompteursSession {
            peripherique: nom.to_string(),
            periode_us: 10_000,
            reveils_en_retard: retards.clone(),
            ecart_max_us: Arc::new(AtomicU64::new(39_000)),
            underruns: Arc::new(AtomicU64::new(0)),
            deadline_misses: Arc::new(AtomicU64::new(0)),
            persistant: persistant.clone(),
            bridage_refuse: Some(true),
        });
        // Le relevé lit les compteurs VIVANTS, pas une copie figée.
        retards.store(64, Ordering::Relaxed);
        persistant.store(true, Ordering::Relaxed);
        let r: Vec<_> = releve_des_sessions_en_cours()
            .into_iter()
            .filter(|s| s.peripherique == nom)
            .collect();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].reveils_en_retard, 64);
        assert_eq!(r[0].ecart_max_us, 39_000);
        assert!(r[0].retards_persistants);
        assert_eq!(r[0].bridage_refuse, Some(true));
        drop(inscription);
        assert!(
            releve_des_sessions_en_cours()
                .iter()
                .all(|s| s.peripherique != nom)
        );
    }

    /// Le branchement dans la boucle de rendu — sans lui, rien ci-dessus ne
    /// joue. Comparé sans blancs : rustfmt coupe librement un appel long.
    #[test]
    fn la_boucle_de_rendu_refuse_le_bridage_et_s_inscrit_avant_de_boucler() {
        let source: String = include_str!("wasapi_exclusive.rs")
            .split_whitespace()
            .collect();
        let bridage = source.find("ffi::SetThreadInformation(");
        let minuteur = source.find("ffi::timeBeginPeriod(RESOLUTION_MINUTEUR_MS)");
        let inscription = source.find("inscrire_session(CompteursSession{");
        let boucle = source.find("whilerunning.load(Ordering::SeqCst){");
        let persistance = source.find("persistance.as_mut()");
        let fin_minuteur = source.find("ffi::timeEndPeriod(RESOLUTION_MINUTEUR_MS)");
        let fin = source.find("info!(\"wasapi_exclusive_render_thread_stopped\");");
        for (nom, p) in [
            ("refus du bridage", bridage),
            ("minuteur", minuteur),
            ("inscription", inscription),
            ("boucle", boucle),
            ("persistance", persistance),
            ("fin du minuteur", fin_minuteur),
            ("fin", fin),
        ] {
            assert!(p.is_some(), "absent de wasapi_exclusive.rs : {nom}");
        }
        assert!(bridage < boucle && minuteur < boucle && inscription < boucle);
        assert!(boucle < persistance);
        assert!(persistance < fin_minuteur && fin_minuteur < fin);
    }
}
