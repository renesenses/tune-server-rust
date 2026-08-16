//! Où en est la mise à niveau de la base, pendant qu'elle tourne.
//!
//! Une migration longue était jusqu'ici **entièrement muette** : le serveur
//! ouvrait la base au démarrage, la migrait, et ne répondait à rien tant que ce
//! n'était pas fini. Le testeur « eric » a signalé « l'installation de la 9.70
//! plante » (fil forum 1386) — c'était une migration qui travaillait, sans un
//! mot (#1701).
//!
//! Ce module est le point de lecture partagé : le moteur de migrations écrit
//! son avancement ici, et le répondeur de démarrage du serveur
//! (`tune-server/src/boot_status.rs`) le lit pour l'annoncer au navigateur qui
//! frappe à la porte. Il est volontairement global : pendant les migrations,
//! `AppState` n'existe pas encore — il n'y a rien d'autre où le ranger.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Avancement instantané d'une mise à niveau de base.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MigrationProgress {
    /// `"sqlite"` ou `"postgres"`.
    pub engine: &'static str,
    /// Étapes terminées.
    pub done: usize,
    /// Étapes à faire au total (migrations en attente + contrôles finaux).
    pub total: usize,
    /// Nom de l'étape en cours.
    pub step: String,
    /// Temps écoulé depuis le début de la mise à niveau.
    pub elapsed: Duration,
}

impl MigrationProgress {
    /// La phrase montrée à l'utilisateur. Volontairement en français : elle
    /// finit telle quelle dans la page d'attente et dans le journal.
    pub fn describe(&self) -> String {
        format!(
            "Mise à niveau de la base : étape {} sur {} ({}) — {} s",
            (self.done + 1).min(self.total.max(1)),
            self.total.max(1),
            self.step,
            self.elapsed.as_secs()
        )
    }
}

struct Inner {
    engine: &'static str,
    done: usize,
    total: usize,
    step: String,
    started: Instant,
}

static STATE: Mutex<Option<Inner>> = Mutex::new(None);

fn with_state<R>(f: impl FnOnce(&mut Option<Inner>) -> R) -> R {
    // Un verrou empoisonné ne doit jamais faire tomber un démarrage : on
    // récupère l'intérieur et on continue.
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

/// Déclare le début d'une mise à niveau de `total` étapes.
pub fn begin(engine: &'static str, total: usize) {
    with_state(|s| {
        *s = Some(Inner {
            engine,
            done: 0,
            total,
            step: "préparation".to_string(),
            started: Instant::now(),
        });
    });
}

/// Annonce l'étape qui commence, `done` étant le nombre d'étapes déjà finies.
pub fn advance(step: &str, done: usize) {
    with_state(|s| {
        if let Some(inner) = s.as_mut() {
            inner.step = step.to_string();
            inner.done = done;
        }
    });
}

/// La mise à niveau est terminée : plus rien à annoncer.
pub fn finish() {
    with_state(|s| *s = None);
}

/// L'avancement courant, ou `None` si aucune mise à niveau n'est en cours.
pub fn snapshot() -> Option<MigrationProgress> {
    with_state(|s| {
        s.as_ref().map(|inner| MigrationProgress {
            engine: inner.engine,
            done: inner.done,
            total: inner.total,
            step: inner.step.clone(),
            elapsed: inner.started.elapsed(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// La phrase montrée au testeur qui croit son serveur planté : elle doit
    /// dire une étape, un total, et un temps. C'est tout ce qui manquait.
    #[test]
    fn describe_names_the_step_and_the_total() {
        let p = MigrationProgress {
            engine: "sqlite",
            done: 2,
            total: 21,
            step: "merge_scattered_compilations".to_string(),
            elapsed: Duration::from_secs(94),
        };
        assert_eq!(
            p.describe(),
            "Mise à niveau de la base : étape 3 sur 21 \
             (merge_scattered_compilations) — 94 s"
        );
    }

    /// Dernière étape : le compteur ne doit pas afficher « 22 sur 21 ».
    #[test]
    fn describe_never_overshoots_the_total() {
        let p = MigrationProgress {
            engine: "sqlite",
            done: 21,
            total: 21,
            step: "contrôles finaux".to_string(),
            elapsed: Duration::from_secs(3),
        };
        assert!(
            p.describe().contains("étape 21 sur 21"),
            "compteur hors bornes : {}",
            p.describe()
        );
    }
}
