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

use super::mesure_du_retard_8696::{CASES, CausesCumulees, CausesLues, Histogramme};

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
    histogramme: Arc<Histogramme>,
    causes: Arc<CausesCumulees>,
    reouverture_apres_ms: Option<u64>,
}

impl SessionVivante {
    fn relever(&self, en_cours: bool) -> ReleveSessionWasapi {
        ReleveSessionWasapi {
            peripherique: self.peripherique.clone(),
            en_cours,
            periode_us: self.periode_us,
            reveils_en_retard: self.reveils_en_retard.load(Ordering::Relaxed),
            ecart_max_us: self.ecart_max_us.load(Ordering::Relaxed),
            underruns: self.underruns.load(Ordering::Relaxed),
            deadline_misses: self.deadline_misses.load(Ordering::Relaxed),
            retards_persistants: self.persistant.load(Ordering::Relaxed),
            bridage_refuse: self.bridage_refuse,
            histogramme_periodes: self.histogramme.lire(),
            causes: self.causes.lire(),
            reouverture_apres_ms: self.reouverture_apres_ms,
        }
    }
}

static SESSIONS: Mutex<Vec<SessionVivante>> = Mutex::new(Vec::new());
/// #8696 — les dernières sessions TERMINÉES, la plus récente en dernier. Le
/// testeur envoie son rapport juste APRÈS « Arrêter » : sans elles, la
/// session malade n'y figurait plus.
static TERMINEES: Mutex<Vec<ReleveSessionWasapi>> = Mutex::new(Vec::new());
/// Combien de sessions terminées le rapport garde.
pub const SESSIONS_TERMINEES_GARDEES: usize = 3;
static JETON: AtomicU64 = AtomicU64::new(1);

/// Le relevé d'une session en cours, tel que le lit le rapport de bogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleveSessionWasapi {
    pub peripherique: String,
    /// Vrai pour une session qui joue encore ; faux pour l'une des
    /// [`SESSIONS_TERMINEES_GARDEES`] dernières sessions closes.
    pub en_cours: bool,
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
    /// #8696 — les réveils en retard par écart, en périodes : 2, 3, 4, 5, 6+.
    pub histogramme_periodes: [u64; CASES],
    /// #8696 — où le temps des retards est passé, et ce qu'a fait l'appareil.
    pub causes: CausesLues,
    /// #8696 — délai entre la fermeture précédente du MÊME appareil par Tune
    /// et cette ouverture, en ms ; `None` : première ouverture du processus.
    /// Les trois sessions malades des fils 2805/2821/2911 s'ouvraient juste
    /// après un arrêt ; ce champ met la corrélation à l'épreuve.
    pub reouverture_apres_ms: Option<u64>,
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
    pub histogramme: Arc<Histogramme>,
    pub causes: Arc<CausesCumulees>,
    pub reouverture_apres_ms: Option<u64>,
}

/// Tant qu'elle vit, la session figure au relevé ; à sa chute, elle en sort.
#[must_use]
pub(crate) struct SessionInscrite {
    jeton: u64,
}

impl Drop for SessionInscrite {
    fn drop(&mut self) {
        let mut sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(i) = sessions.iter().position(|s| s.jeton == self.jeton) else {
            return;
        };
        let close = sessions.remove(i).relever(false);
        drop(sessions);
        let mut terminees = TERMINEES.lock().unwrap_or_else(|e| e.into_inner());
        terminees.push(close);
        let surplus = terminees.len().saturating_sub(SESSIONS_TERMINEES_GARDEES);
        terminees.drain(..surplus);
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
        histogramme: c.histogramme,
        causes: c.causes,
        reouverture_apres_ms: c.reouverture_apres_ms,
    });
    SessionInscrite { jeton }
}

/// #8696 — la dernière fermeture de chaque appareil par Tune (identifiant
/// d'endpoint → instant). Quelques entrées au plus : un par DAC utilisé.
static FERMETURES: Mutex<Vec<(String, std::time::Instant)>> = Mutex::new(Vec::new());

/// Note la fermeture d'un appareil (fin de `stop()`).
pub(crate) fn noter_la_fermeture(endpoint: &str, quand: std::time::Instant) {
    let mut f = FERMETURES.lock().unwrap_or_else(|e| e.into_inner());
    f.retain(|(e, _)| e != endpoint);
    f.push((endpoint.to_string(), quand));
    let surplus = f.len().saturating_sub(16);
    f.drain(..surplus);
}

/// Le délai depuis la dernière fermeture de cet appareil, en ms.
pub(crate) fn ms_depuis_la_fermeture(
    endpoint: &str,
    maintenant: std::time::Instant,
) -> Option<u64> {
    let f = FERMETURES.lock().unwrap_or_else(|e| e.into_inner());
    f.iter().find(|(e, _)| e == endpoint).map(|(_, quand)| {
        u64::try_from(maintenant.saturating_duration_since(*quand).as_millis()).unwrap_or(u64::MAX)
    })
}

/// Le relevé des sessions WASAPI exclusives EN COURS (vide hors Windows ou
/// quand rien ne joue). Lu par le rapport de bogue.
pub fn releve_des_sessions_en_cours() -> Vec<ReleveSessionWasapi> {
    let sessions = SESSIONS.lock().unwrap_or_else(|e| e.into_inner());
    sessions.iter().map(|s| s.relever(true)).collect()
}

/// #8696 — les sessions en cours, puis les dernières sessions terminées (la
/// plus récente d'abord). C'est ce que lit le rapport de bogue.
pub fn releve_des_sessions() -> Vec<ReleveSessionWasapi> {
    let mut v = releve_des_sessions_en_cours();
    let terminees = TERMINEES.lock().unwrap_or_else(|e| e.into_inner());
    v.extend(terminees.iter().rev().cloned());
    v
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
            histogramme: Arc::new(Histogramme::default()),
            causes: Arc::new(CausesCumulees::default()),
            reouverture_apres_ms: None,
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

    /// #8696 — le rapport part juste APRÈS « Arrêter » : la session close y
    /// reste, avec son histogramme, marquée terminée.
    /// Les deux témoins des sessions closes partagent la liste bornée : l'un
    /// évincerait la session de l'autre s'ils couraient ensemble.
    static SERIE: Mutex<()> = Mutex::new(());

    #[test]
    fn la_session_close_reste_au_releve_avec_son_histogramme() {
        let _serie = SERIE.lock().unwrap_or_else(|e| e.into_inner());
        let nom = "témoin-8696-terminee";
        let histogramme = Arc::new(Histogramme::default());
        let inscription = inscrire_session(CompteursSession {
            peripherique: nom.to_string(),
            periode_us: 10_000,
            reveils_en_retard: Arc::new(AtomicU64::new(125)),
            ecart_max_us: Arc::new(AtomicU64::new(40_592)),
            underruns: Arc::new(AtomicU64::new(0)),
            deadline_misses: Arc::new(AtomicU64::new(0)),
            persistant: Arc::new(AtomicBool::new(true)),
            bridage_refuse: Some(true),
            histogramme: histogramme.clone(),
            causes: Arc::new(CausesCumulees::default()),
            reouverture_apres_ms: None,
        });
        histogramme.ajouter(2);
        let en_cours: Vec<_> = releve_des_sessions()
            .into_iter()
            .filter(|s| s.peripherique == nom)
            .collect();
        assert_eq!(en_cours.len(), 1);
        assert!(en_cours[0].en_cours);
        drop(inscription);
        let apres: Vec<_> = releve_des_sessions()
            .into_iter()
            .filter(|s| s.peripherique == nom)
            .collect();
        assert_eq!(apres.len(), 1, "la session close figure au relevé");
        assert!(!apres[0].en_cours);
        assert_eq!(apres[0].reveils_en_retard, 125);
        assert_eq!(apres[0].histogramme_periodes, [0, 0, 1, 0, 0]);
        assert!(
            releve_des_sessions_en_cours()
                .iter()
                .all(|s| s.peripherique != nom)
        );
    }

    #[test]
    fn le_releve_ne_garde_que_les_dernieres_sessions_closes() {
        let _serie = SERIE.lock().unwrap_or_else(|e| e.into_inner());
        for i in 0..(SESSIONS_TERMINEES_GARDEES + 4) {
            let inscription = inscrire_session(CompteursSession {
                peripherique: format!("témoin-8696-borne-{i}"),
                periode_us: 10_000,
                reveils_en_retard: Arc::new(AtomicU64::new(0)),
                ecart_max_us: Arc::new(AtomicU64::new(0)),
                underruns: Arc::new(AtomicU64::new(0)),
                deadline_misses: Arc::new(AtomicU64::new(0)),
                persistant: Arc::new(AtomicBool::new(false)),
                bridage_refuse: None,
                histogramme: Arc::new(Histogramme::default()),
                causes: Arc::new(CausesCumulees::default()),
                reouverture_apres_ms: Some(230),
            });
            drop(inscription);
        }
        let closes = TERMINEES.lock().unwrap_or_else(|e| e.into_inner()).len();
        assert!(closes <= SESSIONS_TERMINEES_GARDEES);
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

    #[test]
    fn le_delai_depuis_la_fermeture_se_mesure_par_appareil() {
        let t0 = std::time::Instant::now();
        let a = "témoin-8696-endpoint-a";
        assert_eq!(ms_depuis_la_fermeture(a, t0), None, "jamais fermé");
        noter_la_fermeture(a, t0);
        let plus_tard = t0 + std::time::Duration::from_millis(230);
        assert_eq!(ms_depuis_la_fermeture(a, plus_tard), Some(230));
        assert_eq!(
            ms_depuis_la_fermeture("témoin-8696-endpoint-b", plus_tard),
            None
        );
        // Une nouvelle fermeture remplace l'ancienne.
        noter_la_fermeture(a, plus_tard);
        assert_eq!(ms_depuis_la_fermeture(a, plus_tard), Some(0));
    }
}
