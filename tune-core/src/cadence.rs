//! Cadencement des evenements d'avancement.
//!
//! Une passe qui traite des dizaines de milliers de fichiers ne doit PAS
//! emettre un evenement par fichier : le bus est un `broadcast` borne, et un
//! client lent finirait par prendre du retard (`Lagged`) puis a perdre des
//! evenements qui, eux, comptent — la fin de scan, un changement de zone.
//!
//! C'est la discipline que `library.scan.progress` s'impose deja depuis
//! longtemps (`routes/system/scan.rs` n'emet qu'apres un lot ET au plus une
//! fois toutes les deux secondes ; `routes/ws.rs` re-filtre par client). Ce
//! type la rend reutilisable au lieu de la recopier a chaque nouvelle passe
//! (#2870).

use std::time::{Duration, Instant};

/// Intervalle minimal entre deux annonces d'avancement du MEME sujet.
///
/// Deux secondes, comme `library.scan.progress` : au-dela l'affichage parait
/// fige, en-deca on inonde le bus sans que l'oeil y gagne quoi que ce soit.
pub const INTERVALLE_AVANCEMENT: Duration = Duration::from_secs(2);

/// Laisse passer la premiere annonce, puis une au plus par intervalle.
#[derive(Debug)]
pub struct Cadence {
    intervalle: Duration,
    derniere: Option<Instant>,
}

impl Cadence {
    pub fn nouvelle(intervalle: Duration) -> Self {
        Self {
            intervalle,
            derniere: None,
        }
    }

    /// Cadence d'avancement standard (voir `INTERVALLE_AVANCEMENT`).
    pub fn avancement() -> Self {
        Self::nouvelle(INTERVALLE_AVANCEMENT)
    }

    /// Rend `true` si l'annonce doit partir MAINTENANT, et note l'instant.
    ///
    /// La PREMIERE annonce passe toujours : sans elle, une barre de progression
    /// resterait vide pendant les deux premieres secondes d'une passe qui peut
    /// durer des heures — et c'est justement l'instant ou l'utilisateur regarde.
    pub fn autorise(&mut self) -> bool {
        self.autorise_a(Instant::now())
    }

    /// Meme decision, a un instant donne — c'est ce qui rend la regle testable
    /// sans faire dormir le test (un test qui attend une horloge reelle est un
    /// test qui clignotera un jour en CI).
    pub fn autorise_a(&mut self, maintenant: Instant) -> bool {
        match self.derniere {
            Some(precedente) if maintenant.duration_since(precedente) < self.intervalle => false,
            _ => {
                self.derniere = Some(maintenant);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_premiere_annonce_passe_toujours() {
        let mut c = Cadence::avancement();
        assert!(c.autorise(), "une barre vide pendant 2 s est un defaut");
    }

    #[test]
    fn les_suivantes_sont_espacees_de_l_intervalle() {
        let t0 = Instant::now();
        let mut c = Cadence::nouvelle(Duration::from_secs(2));
        assert!(c.autorise_a(t0));
        assert!(!c.autorise_a(t0 + Duration::from_millis(1)));
        assert!(!c.autorise_a(t0 + Duration::from_millis(1999)));
        assert!(c.autorise_a(t0 + Duration::from_millis(2000)));
        // Et la fenetre repart de l'annonce REELLEMENT emise, pas de celles
        // qu'on a refusees.
        assert!(!c.autorise_a(t0 + Duration::from_millis(2001)));
        assert!(c.autorise_a(t0 + Duration::from_millis(4000)));
    }

    /// Contre-epreuve : une cadence a zero ne filtre rien. Si ce test passait
    /// avec un `false`, c'est que le cadenceur bloque au lieu de cadencer.
    #[test]
    fn une_cadence_nulle_laisse_tout_passer() {
        let t0 = Instant::now();
        let mut c = Cadence::nouvelle(Duration::ZERO);
        assert!(c.autorise_a(t0));
        assert!(c.autorise_a(t0));
    }
}
