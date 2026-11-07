//! L'avancement des analyses de la cascade qui ne sont pas le ReplayGain :
//! les empreintes et la plage dynamique (#7915).
//!
//! Seule la passe ReplayGain publiait un compte ([`crate::audio::replaygain::progression`]).
//! Or le ReplayGain est coupé par défaut, et depuis #5246 la cascade lance
//! quand même les empreintes puis la plage dynamique sur toute la
//! bibliothèque. Sur un NAS lent, ces deux passes ont tenu le processeur toute
//! une nuit sans qu'aucun écran ne dise ce qui tournait, combien il en restait
//! ni quand ce serait fini.
//!
//! Ce module tient, pour chacune, une CAMPAGNE : traitées, total figé à
//! l'ouverture, heure de début et estimation de fin. Il l'annonce sur le bus à
//! la même cadence que le scan et le ReplayGain (2 s), et sans attendre la
//! cadence aux deux bords. À la clôture, il écrit dans le journal la durée de
//! la campagne et le nombre de pistes traitées.
//!
//! Comme `progression`, c'est un état de processus : un serveur redémarré
//! repart au repos, ce qui est la vérité.

use crate::event_bus::EventBus;
use crate::event_types::EventType;
use serde::Serialize;
use serde_json::json;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Tache;

/// La cadence des annonces, la même que le scan et le ReplayGain.
pub const CADENCE: Duration = crate::audio::replaygain::progression::CADENCE_PROGRESSION_REPLAYGAIN;

/// Ce qu'une passe sait dire d'elle-même, tel que la route le rend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Avancement {
    /// L'identifiant de la tâche ([`Tache::id`]).
    pub task: &'static str,
    /// Une campagne est ouverte : la passe a du travail devant elle.
    pub active: bool,
    pub processed: i64,
    pub total: i64,
    pub remaining: i64,
    /// Estimation du temps restant, en secondes ; `None` tant qu'aucune piste
    /// n'est traitée ou quand la campagne est close.
    pub eta_seconds: Option<u64>,
    /// Début de la campagne, secondes Unix.
    pub started_at: u64,
    /// Dernière mise à jour, secondes Unix.
    pub updated_at: u64,
    /// Durée de la campagne jusqu'ici (ou jusqu'à sa clôture), en ms.
    pub elapsed_ms: u64,
}

struct Etat {
    tache: Tache,
    actif: bool,
    traitees: i64,
    total: i64,
    debut: Instant,
    debut_epoch: u64,
    maj_epoch: u64,
    duree_close: Option<Duration>,
    derniere_emission: Option<Instant>,
}

impl Etat {
    fn releve(&self) -> Avancement {
        let ecoule = self.duree_close.unwrap_or_else(|| self.debut.elapsed());
        let restantes = (self.total - self.traitees).max(0);
        Avancement {
            task: self.tache.id(),
            active: self.actif,
            processed: self.traitees,
            total: self.total,
            remaining: restantes,
            eta_seconds: if self.actif {
                estimer_le_reste(self.traitees, restantes, ecoule)
            } else {
                None
            },
            started_at: self.debut_epoch,
            updated_at: self.maj_epoch,
            elapsed_ms: ecoule.as_millis() as u64,
        }
    }
}

static ETATS: Mutex<Vec<Etat>> = Mutex::new(Vec::new());
static BUS: OnceLock<Arc<EventBus>> = OnceLock::new();

/// Branche le bus d'évènements. Appelé une fois, au démarrage du serveur.
/// Non branché, les compteurs tiennent et la route les rend ; seul le fil
/// d'évènements se tait.
pub fn brancher_le_bus(bus: Arc<EventBus>) {
    let _ = BUS.set(bus);
}

fn maintenant_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Le temps qui reste, au débit observé depuis l'ouverture. `None` tant que
/// rien n'est traité : pas de débit, pas d'estimation.
pub fn estimer_le_reste(traitees: i64, restantes: i64, ecoule: Duration) -> Option<u64> {
    if traitees <= 0 {
        return None;
    }
    if restantes <= 0 {
        return Some(0);
    }
    let par_piste = ecoule.as_secs_f64() / traitees as f64;
    Some((par_piste * restantes as f64).round() as u64)
}

/// Ouvre une campagne si aucune n'est en cours pour cette tâche.
///
/// `compter_les_restants` n'est appelé qu'à l'ouverture : c'est un `COUNT(*)`
/// qui se paie sur une grosse bibliothèque.
pub fn ouvrir_si_besoin(tache: Tache, compter_les_restants: impl FnOnce() -> i64) {
    if ETATS
        .lock()
        .map(|e| e.iter().any(|s| s.tache == tache && s.actif))
        .unwrap_or(true)
    {
        return;
    }
    let restants = compter_les_restants().max(0);
    let instantane = {
        let Ok(mut etats) = ETATS.lock() else {
            return;
        };
        let neuf = Etat {
            tache,
            actif: true,
            traitees: 0,
            total: restants,
            debut: Instant::now(),
            debut_epoch: maintenant_epoch(),
            maj_epoch: maintenant_epoch(),
            duree_close: None,
            derniere_emission: Some(Instant::now()),
        };
        match etats.iter_mut().find(|s| s.tache == tache) {
            Some(s) if s.actif => return,
            Some(s) => *s = neuf,
            None => etats.push(neuf),
        }
        etats.iter().find(|s| s.tache == tache).map(Etat::releve)
    };
    if let Some(a) = instantane {
        tracing::info!(
            tache = tache.id(),
            total = a.total,
            "analyse_de_fond_campagne_ouverte"
        );
        emettre(a, "started");
    }
}

/// `n` pistes de moins à traiter. Hors campagne : ignoré.
pub fn avancer(tache: Tache, n: i64) {
    let instantane = {
        let Ok(mut etats) = ETATS.lock() else {
            return;
        };
        let Some(s) = etats.iter_mut().find(|s| s.tache == tache && s.actif) else {
            return;
        };
        s.traitees += n.max(0);
        // Un scan a pu ajouter des pistes : un total qui monte vaut mieux
        // qu'une jauge au-delà de 100 %.
        if s.traitees > s.total {
            s.total = s.traitees;
        }
        s.maj_epoch = maintenant_epoch();
        let du = s
            .derniere_emission
            .map(|t| t.elapsed() >= CADENCE)
            .unwrap_or(true);
        if !du {
            return;
        }
        s.derniere_emission = Some(Instant::now());
        s.releve()
    };
    emettre(instantane, "progress");
}

/// La passe n'a plus rien à traiter : clore la campagne, l'annoncer une fois
/// et écrire sa durée dans le journal. Sans campagne ouverte : rien.
pub fn clore(tache: Tache) {
    let instantane = {
        let Ok(mut etats) = ETATS.lock() else {
            return;
        };
        let Some(s) = etats.iter_mut().find(|s| s.tache == tache && s.actif) else {
            return;
        };
        s.actif = false;
        s.duree_close = Some(s.debut.elapsed());
        s.maj_epoch = maintenant_epoch();
        s.derniere_emission = Some(Instant::now());
        s.releve()
    };
    tracing::info!(
        tache = tache.id(),
        traitees = instantane.processed,
        duree_ms = instantane.elapsed_ms,
        "analyse_de_fond_campagne_terminee"
    );
    emettre(instantane, "idle");
}

/// L'avancement d'une tâche, s'il a déjà parlé depuis le démarrage.
pub fn releve(tache: Tache) -> Option<Avancement> {
    ETATS
        .lock()
        .ok()?
        .iter()
        .find(|s| s.tache == tache)
        .map(Etat::releve)
}

/// Remet l'état à neuf. **Réservé aux témoins.**
#[doc(hidden)]
pub fn reinitialiser_pour_les_essais() {
    if let Ok(mut etats) = ETATS.lock() {
        etats.clear();
    }
}

fn emettre(a: Avancement, phase: &'static str) {
    let Some(bus) = BUS.get() else {
        return;
    };
    bus.emit_typed(
        EventType::BackgroundAnalysisProgress,
        json!({
            "phase": phase,
            "task": a.task,
            "active": a.active,
            "processed": a.processed,
            "total": a.total,
            "remaining": a.remaining,
            "eta_seconds": a.eta_seconds,
            "started_at": a.started_at,
            "updated_at": a.updated_at,
            "elapsed_ms": a.elapsed_ms,
        }),
    );
}

/// Le journal des durées des étapes d'un travail long (#7915) : une ligne
/// `etape_terminee` par étape, avec sa durée, pour qu'un rapport dise où le
/// temps est passé.
pub struct ChronoDesEtapes {
    travail: &'static str,
    depuis: Instant,
}

impl ChronoDesEtapes {
    pub fn demarrer(travail: &'static str) -> Self {
        ChronoDesEtapes {
            travail,
            depuis: Instant::now(),
        }
    }

    /// Clore l'étape en cours, l'écrire au journal et ouvrir la suivante.
    /// Rend sa durée.
    pub fn fin(&mut self, etape: &'static str) -> Duration {
        let duree = self.depuis.elapsed();
        tracing::info!(
            travail = self.travail,
            etape,
            duree_ms = duree.as_millis() as u64,
            "etape_terminee"
        );
        self.depuis = Instant::now();
        duree
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l_estimation_suit_le_debit_observe() {
        assert_eq!(estimer_le_reste(0, 100, Duration::from_secs(10)), None);
        assert_eq!(estimer_le_reste(10, 0, Duration::from_secs(10)), Some(0));
        // 10 pistes en 20 s, 30 restantes : 60 s.
        assert_eq!(estimer_le_reste(10, 30, Duration::from_secs(20)), Some(60));
    }

    /// #7915 — témoin de l'avancement publié : une campagne ouverte, avancée
    /// puis close rend ses chiffres, son estimation, et se ferme.
    #[test]
    fn une_campagne_publie_traitees_total_et_fin_7915() {
        // Des tâches que la cascade ne touche pas : les témoins de la cascade
        // tournent en parallèle sur le même état de processus.
        let (t, autre) = (Tache::Integrite, Tache::FauxHiRes);
        assert_eq!(releve(t), None);
        ouvrir_si_besoin(t, || 40);
        avancer(t, 1);
        avancer(t, 9);
        let a = releve(t).expect("campagne ouverte");
        assert!(a.active);
        assert_eq!((a.processed, a.total, a.remaining), (10, 40, 30));
        assert!(
            a.eta_seconds.is_some(),
            "une estimation dès la première piste"
        );
        // Une seconde ouverture ne recompte ni ne remet à zéro.
        ouvrir_si_besoin(t, || panic!("ne recompte pas"));
        // L'autre passe est indépendante.
        assert_eq!(releve(autre), None);
        clore(t);
        let a = releve(t).unwrap();
        assert!(!a.active);
        assert_eq!(a.processed, 10, "le compte reste lisible après la fin");
        assert_eq!(a.eta_seconds, None);
    }
}
