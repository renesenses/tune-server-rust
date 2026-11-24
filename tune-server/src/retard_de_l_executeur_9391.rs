//! #9391 — l'exécuteur tokio dit quand il prend du retard.
//!
//! Sous Tune OS, un testeur voit le démarrage d'une piste convertie (FLAC
//! 192/24 vers une autre cadence) prendre des dizaines de secondes, l'interface
//! figée, puis tout repartir au premier son. Le journal ne permettait pas de
//! départager un travail lourd posé sur l'exécuteur (qui gèle toutes les
//! routes) d'un fil de conversion qui sature le seul cœur disponible (#9381 :
//! tout tournait sur le cœur 0).
//!
//! Une tâche se réveille toutes les [`PERIODE`] et mesure son retard sur
//! l'heure prévue. Un retard d'au moins [`SEUIL`] veut dire qu'aucun fil de
//! l'exécuteur n'était libre — ou que la machine entière n'a pas donné la main.
//! Une ligne au plus par [`FENETRE`] : le premier retard tout de suite, les
//! suivants regroupés (nombre et pire retard), pour qu'un gel de trente
//! secondes ne remplisse pas le journal.

use std::time::Duration;

use tokio::time::Instant;
use tracing::warn;

/// L'intervalle entre deux réveils.
pub const PERIODE: Duration = Duration::from_millis(250);
/// Le retard à partir duquel l'exécuteur est réputé occupé.
pub const SEUIL: Duration = Duration::from_millis(500);
/// Au plus une ligne par fenêtre.
pub const FENETRE: Duration = Duration::from_secs(10);

/// Ce qu'une ligne du journal dit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Annonce {
    /// Retards d'au moins [`SEUIL`] regroupés dans cette ligne.
    pub retards: u32,
    /// Le pire d'entre eux.
    pub pire: Duration,
}

/// Le suivi des retards, sans horloge à lui : testable à la main.
#[derive(Debug, Default)]
pub struct Suivi {
    derniere_annonce: Option<Instant>,
    retards: u32,
    pire: Duration,
}

impl Suivi {
    /// Un réveil en retard de `retard`, constaté à `maintenant`. Rend la ligne
    /// à écrire, s'il en faut une.
    pub fn observer(&mut self, retard: Duration, maintenant: Instant) -> Option<Annonce> {
        if retard >= SEUIL {
            self.retards += 1;
            self.pire = self.pire.max(retard);
        }
        if self.retards == 0 {
            return None;
        }
        let libre = self
            .derniere_annonce
            .is_none_or(|t| maintenant.duration_since(t) >= FENETRE);
        if !libre {
            return None;
        }
        let annonce = Annonce {
            retards: self.retards,
            pire: self.pire,
        };
        self.derniere_annonce = Some(maintenant);
        self.retards = 0;
        self.pire = Duration::ZERO;
        Some(annonce)
    }
}

/// Lance la surveillance, pour toute la vie du processus.
pub fn surveiller() {
    tokio::spawn(async {
        let mut suivi = Suivi::default();
        let mut prevu = Instant::now() + PERIODE;
        loop {
            tokio::time::sleep_until(prevu).await;
            let maintenant = Instant::now();
            let retard = maintenant.saturating_duration_since(prevu);
            if let Some(a) = suivi.observer(retard, maintenant) {
                warn!(
                    retards = a.retards,
                    pire_retard_ms = a.pire.as_millis() as u64,
                    seuil_ms = SEUIL.as_millis() as u64,
                    fenetre_s = FENETRE.as_secs(),
                    "executeur_en_retard_9391: no runtime worker was free on time — \
                     blocking work on the async runtime, or a machine starved of CPU"
                );
            }
            prevu = maintenant + PERIODE;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_reveil_a_l_heure_ne_dit_rien() {
        let mut s = Suivi::default();
        let t = Instant::now();
        assert_eq!(s.observer(Duration::from_millis(3), t), None);
        assert_eq!(s.observer(SEUIL - Duration::from_millis(1), t), None);
    }

    #[test]
    fn le_premier_retard_est_dit_tout_de_suite_puis_les_suivants_sont_regroupes() {
        let mut s = Suivi::default();
        let t0 = Instant::now();
        assert_eq!(
            s.observer(Duration::from_secs(2), t0),
            Some(Annonce {
                retards: 1,
                pire: Duration::from_secs(2)
            })
        );
        assert_eq!(
            s.observer(Duration::from_millis(700), t0 + Duration::from_secs(3)),
            None
        );
        assert_eq!(
            s.observer(Duration::from_secs(4), t0 + Duration::from_secs(8)),
            None
        );
        assert_eq!(
            s.observer(Duration::ZERO, t0 + FENETRE),
            Some(Annonce {
                retards: 2,
                pire: Duration::from_secs(4)
            })
        );
        assert_eq!(s.observer(Duration::ZERO, t0 + FENETRE * 3), None);
    }

    /// Le témoin de bout en bout : un travail bloquant posé sur le seul fil
    /// d'un exécuteur se voit dans le retard du réveil suivant.
    #[test]
    fn un_travail_bloquant_sur_l_executeur_se_mesure_9391() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let retard = rt.block_on(async {
            let prevu = Instant::now() + Duration::from_millis(20);
            let bloquant = tokio::spawn(async {
                // Ce que #9391 soupçonne : un calcul lourd hors `spawn_blocking`.
                std::thread::sleep(Duration::from_millis(800));
            });
            tokio::time::sleep_until(prevu).await;
            let retard = Instant::now().saturating_duration_since(prevu);
            bloquant.await.unwrap();
            retard
        });
        assert!(retard >= SEUIL, "retard mesuré : {retard:?}");
        let mut s = Suivi::default();
        assert!(s.observer(retard, Instant::now()).is_some());
    }
}
