//! La vitesse des passes de fond qui décodent (#5519).
//!
//! Tades mesurait ~1 300 pistes/h sur 400 000 : la passe ne décodait qu'UN
//! fichier à la fois, puis dormait 400 ms. Mesuré sur Shrek (40 FLAC réels) :
//! la passe est bornée par le processeur, et K fichiers à la fois vont
//! ×1,9 (K = 2) et ×3,2 (K = 4).
//!
//! Décision de Bertrand (30/09/2026) : trois vitesses, réglables.
//!
//! | Réglage   | Fichiers à la fois                                   |
//! |-----------|------------------------------------------------------|
//! | `discreet`| 1 — le comportement d'avant, sans la pause fixe       |
//! | `normal`  | 2 (**défaut**), jamais plus que le nombre de cœurs    |
//! | `fast`    | jusqu'à 4, jamais plus que le nombre de cœurs moins 1 |
//!
//! Ce qui NE change PAS avec la vitesse :
//! * la priorité à la lecture (#1310, #2495) : chaque fichier en vol court
//!   contre l'arrivée de la lecture, et aucun nouveau ne part tant qu'une zone
//!   joue ;
//! * la garde thermique (#1576) : 80 °C arrête la cascade, 70 °C la relance,
//!   dans les trois modes ;
//! * une seule PASSE décode à la fois (`ANALYSIS_SLOT`) : la vitesse règle le
//!   nombre de fichiers DANS le lot de la passe qui tient le créneau, jamais
//!   l'empilement de deux passes (ReplayGain et CLAP).

use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// La clé du réglage dans `settings`, écrite par `PATCH /system/config`.
pub const CLE_REGLAGE: &str = "background_analysis_speed";

/// Les trois vitesses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Vitesse {
    /// Un fichier à la fois.
    Discrete,
    /// Deux fichiers à la fois (défaut).
    #[default]
    Normale,
    /// Jusqu'à quatre, sans dépasser le nombre de cœurs moins un.
    Rapide,
}

impl Vitesse {
    pub const TOUTES: [Vitesse; 3] = [Vitesse::Discrete, Vitesse::Normale, Vitesse::Rapide];

    /// Le mot du contrat HTTP.
    pub fn id(self) -> &'static str {
        match self {
            Vitesse::Discrete => "discreet",
            Vitesse::Normale => "normal",
            Vitesse::Rapide => "fast",
        }
    }

    pub fn depuis_id(id: &str) -> Option<Self> {
        Self::TOUTES.into_iter().find(|v| v.id() == id)
    }

    /// Combien de fichiers à la fois, sur une machine de `coeurs` cœurs.
    /// Toujours au moins 1.
    pub fn largeur(self, coeurs: usize) -> usize {
        let coeurs = coeurs.max(1);
        match self {
            Vitesse::Discrete => 1,
            Vitesse::Normale => 2.min(coeurs),
            Vitesse::Rapide => 4.min(coeurs.saturating_sub(1)).max(1),
        }
    }
}

/// Le réglage en base. Absent ou illisible : le défaut (`normal`).
pub fn vitesse(backend: &Arc<dyn DbBackend>) -> Vitesse {
    SettingsRepo::with_backend(backend.clone())
        .get(CLE_REGLAGE)
        .ok()
        .flatten()
        .and_then(|v| Vitesse::depuis_id(v.trim().trim_matches('"')))
        .unwrap_or_default()
}

/// Le nombre de cœurs vu par le processus.
pub fn coeurs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Combien de fichiers un lot décode à la fois, MAINTENANT. Relu à chaque lot :
/// un changement de réglage agit au lot suivant, sans redémarrage.
pub fn largeur_courante(backend: &Arc<dyn DbBackend>) -> usize {
    vitesse(backend).largeur(coeurs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_trois_vitesses_et_leurs_bornes() {
        assert_eq!(Vitesse::Discrete.largeur(16), 1);
        assert_eq!(Vitesse::Normale.largeur(16), 2);
        assert_eq!(Vitesse::Rapide.largeur(16), 4);
        // Jamais plus que les cœurs moins un en rapide, jamais plus que les
        // cœurs en normal, jamais zéro.
        assert_eq!(Vitesse::Rapide.largeur(4), 3);
        assert_eq!(Vitesse::Rapide.largeur(2), 1);
        assert_eq!(Vitesse::Rapide.largeur(1), 1);
        assert_eq!(Vitesse::Normale.largeur(1), 1);
        assert_eq!(Vitesse::Discrete.largeur(0), 1);
    }

    #[test]
    fn le_defaut_est_normal_et_les_mots_font_l_aller_retour() {
        assert_eq!(Vitesse::default(), Vitesse::Normale);
        for v in Vitesse::TOUTES {
            assert_eq!(Vitesse::depuis_id(v.id()), Some(v));
        }
        assert_eq!(Vitesse::depuis_id("turbo"), None);
    }

    #[test]
    fn le_reglage_se_lit_en_base_et_retombe_sur_normal() {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL,
                                    updated_at TEXT NOT NULL DEFAULT '');",
        )
        .unwrap();
        let b: Arc<dyn DbBackend> = Arc::new(db);
        assert_eq!(vitesse(&b), Vitesse::Normale);
        SettingsRepo::with_backend(b.clone())
            .set(CLE_REGLAGE, "fast")
            .unwrap();
        assert_eq!(vitesse(&b), Vitesse::Rapide);
        SettingsRepo::with_backend(b.clone())
            .set(CLE_REGLAGE, "n'importe quoi")
            .unwrap();
        assert_eq!(vitesse(&b), Vitesse::Normale);
    }
}
