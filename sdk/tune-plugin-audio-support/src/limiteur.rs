//! #5171 — le limiteur de sécurité de la réserve « Réaliste » de l'égaliseur.
//!
//! La réserve « Sûre » (norme L1 de la cascade) rend l'écrêtage impossible,
//! au prix de beaucoup de niveau : 8,4 dB pour un graphique 31 bandes dont le
//! plus fort curseur est à +3,5 dB. La réserve « Réaliste » ne retire que le
//! maximum RÉEL de la réponse en fréquence : un signal stationnaire, quel qu'il
//! soit, ne peut plus dépasser la pleine échelle, mais un TRANSITOIRE le peut
//! encore — la sonnerie d'une cloche sur un front, que le maximum fréquentiel
//! ne voit pas et que la norme L1 couvrait. Ce limiteur ne sert qu'à ces
//! crêtes-là.
//!
//! # Ce qu'il fait, et ce qu'il ne fait pas
//!
//! * **Inactif sous le seuil, au bit près.** Tant que l'enveloppe reste sous
//!   [`SEUIL`] (−0,2 dBFS), le gain vaut 1 et l'échantillon n'est même pas
//!   multiplié : la sortie est celle de la cascade, identique.
//! * **Au-delà : un GAIN, jamais un écrêtage.** La forme d'onde est multipliée
//!   par un gain `g ≤ 1`, le même sur tous les canaux de la trame (l'image
//!   stéréo ne bouge pas). Le gain suit une courbe à genou doux
//!   ([`courbe`]) : il quitte l'unité en douceur au seuil et tend vers
//!   [`PLAFOND`] (−0,01 dBFS) sans jamais le dépasser, donc aucun échantillon
//!   ne touche le rail et le compteur d'écrêtage de #2218 reste à zéro.
//! * **Attaque instantanée, maintien, relâchement lent.** L'enveloppe saute
//!   sur toute crête nouvelle (attaque en un échantillon : c'est ce qui
//!   garantit le plafond sans regard en avant), se maintient
//!   [`MAINTIEN_S`] (20 ms, plus d'une période à 50 Hz : le gain ne module pas
//!   la forme d'onde d'une basse), puis redescend avec une constante de temps
//!   de [`RELACHEMENT_S`] (150 ms).
//! * **Aucune allocation dans le fil audio.** L'état tient en trois nombres ;
//!   la trame de travail est allouée par l'appelant à la construction.
//!
//! # Pas de regard en avant : latence NULLE
//!
//! Un regard en avant d'une milliseconde aurait permis d'étaler l'attaque sur
//! 44 échantillons au lieu d'un. Il n'a pas été retenu, pour une raison de
//! plomberie et non d'acoustique : un regard en avant RETARDE le signal, et le
//! processeur est reconstruit à chaque piste par les bras de flux réseau
//! (`load_streaming_dsp`), à chaque cran de curseur sur la sortie locale, et
//! aucun de ces chemins ne vide (`drain`) l'étage en fin de piste. Chaque piste
//! aurait perdu sa dernière milliseconde et commencé par une milliseconde de
//! silence — un trou à chaque enchaînement gapless, pour protéger des crêtes
//! rares. La latence du limiteur est donc **zéro trame**
//! (`latency_frames() == 0`, inchangé), et l'attaque se fait en un
//! échantillon, au seul moment où une crête dépasse le seuil.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use tracing::info;

/// Le seuil, en dBFS : au-dessous, le limiteur ne touche à rien.
pub const SEUIL_DBFS: f64 = -0.2;
/// Le plafond, en dBFS : la sortie tend vers lui sans l'atteindre. Même écart
/// au rail que `MARGE_DE_TRONCATURE_DB` de l'égaliseur (0,01 dB) : un
/// échantillon pile au rail compterait comme écrêté.
pub const PLAFOND_DBFS: f64 = -0.01;
/// Le seuil, linéaire (0,977 237).
pub const SEUIL: f64 = 0.977_237_220_955_810_7;
/// Le plafond, linéaire (0,998 849).
pub const PLAFOND: f64 = 0.998_849_369_936_505_2;
/// Maintien de l'enveloppe après une crête, en secondes.
pub const MAINTIEN_S: f64 = 0.020;
/// Constante de temps du relâchement, en secondes.
pub const RELACHEMENT_S: f64 = 0.150;

/// La courbe à genou doux : l'enveloppe `e` (> [`SEUIL`]) devient la crête de
/// sortie `F(e)`. Continue et de pente 1 au seuil, strictement croissante,
/// au plus [`PLAFOND`] (atteint seulement quand `tanh` sature en `f64`, loin
/// au-delà du seuil).
#[inline]
pub fn courbe(e: f64) -> f64 {
    let largeur = PLAFOND - SEUIL;
    SEUIL + largeur * ((e - SEUIL) / largeur).tanh()
}

/// L'état d'un limiteur (lié : un gain pour tous les canaux).
#[derive(Debug, Clone, Copy)]
pub struct Limiteur {
    enveloppe: f64,
    maintien_restant: u32,
    maintien: u32,
    relachement: f64,
    compteur: CompteurDuLimiteur,
}

impl Limiteur {
    /// Un limiteur pour ce débit (en trames par seconde).
    pub fn new(sample_rate: u32) -> Self {
        let sr = f64::from(sample_rate.max(1));
        Self {
            enveloppe: 0.0,
            maintien_restant: 0,
            maintien: (MAINTIEN_S * sr).round() as u32,
            relachement: (-1.0 / (RELACHEMENT_S * sr)).exp(),
            compteur: CompteurDuLimiteur::default(),
        }
    }

    /// Le gain de la trame dont la plus forte valeur absolue est `crete`.
    /// Rend EXACTEMENT 1,0 tant que l'enveloppe reste sous le seuil.
    #[inline]
    pub fn gain(&mut self, crete: f64) -> f64 {
        if crete >= self.enveloppe {
            self.enveloppe = crete;
            self.maintien_restant = self.maintien;
        } else if self.maintien_restant > 0 {
            self.maintien_restant -= 1;
        } else {
            self.enveloppe = (self.enveloppe * self.relachement).max(crete);
        }
        self.compteur.trames_vues += 1;
        if self.enveloppe <= SEUIL || !self.enveloppe.is_finite() {
            return 1.0;
        }
        let g = courbe(self.enveloppe) / self.enveloppe;
        self.compteur.noter(g);
        g
    }

    /// Oublie l'enveloppe (nouveau flux, saut) — garde les compteurs.
    pub fn oublier(&mut self) {
        self.enveloppe = 0.0;
        self.maintien_restant = 0;
    }

    /// Reprend l'état d'un limiteur remplacé en cours de lecture.
    pub fn heriter(&mut self, precedent: &Limiteur) {
        self.enveloppe = precedent.enveloppe;
        self.maintien_restant = precedent.maintien_restant.min(self.maintien);
        self.compteur = precedent.compteur;
    }

    /// Les compteurs de la piste.
    pub fn compteur(&self) -> CompteurDuLimiteur {
        self.compteur
    }

    /// Remet les compteurs à zéro (nouvelle piste).
    pub fn remettre_les_compteurs(&mut self) {
        self.compteur = CompteurDuLimiteur::default();
    }
}

/// Ce que le limiteur a fait sur une piste. Champs simples, zéro allocation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct CompteurDuLimiteur {
    /// Trames passées par le limiteur.
    pub trames_vues: u64,
    /// Trames dont le gain a été inférieur à 1.
    pub trames_limitees: u64,
    /// Plus faible gain appliqué, en dB (≤ 0 ; 0 tant que rien n'a agi).
    pub reduction_max_db: f64,
}

impl CompteurDuLimiteur {
    #[inline]
    fn noter(&mut self, g: f64) {
        self.trames_limitees += 1;
        let db = 20.0 * g.log10();
        if db < self.reduction_max_db {
            self.reduction_max_db = db;
        }
    }

    /// Part des trames limitées, en pourcentage.
    pub fn pourcentage(&self) -> f64 {
        if self.trames_vues == 0 {
            0.0
        } else {
            100.0 * self.trames_limitees as f64 / self.trames_vues as f64
        }
    }
}

/// Totaux du processus, en atomiques : lus par le chemin du signal et le
/// rapport de diagnostic, comme les totaux d'écrêtage de #2218.
pub struct TotauxLimiteur {
    trames_vues: AtomicU64,
    trames_limitees: AtomicU64,
    /// Réduction maximale, en centièmes de dB positifs.
    reduction_max_cdb: AtomicU64,
    pistes_limitees: AtomicU64,
}

impl TotauxLimiteur {
    const fn new() -> Self {
        Self {
            trames_vues: AtomicU64::new(0),
            trames_limitees: AtomicU64::new(0),
            reduction_max_cdb: AtomicU64::new(0),
            pistes_limitees: AtomicU64::new(0),
        }
    }

    /// Ajoute le DELTA d'un bloc (`apres` − `avant`). Rend `true` quand ce
    /// bloc est le premier de la piste à limiter.
    pub fn absorber(&self, avant: &CompteurDuLimiteur, apres: &CompteurDuLimiteur) -> bool {
        self.trames_vues.fetch_add(
            apres.trames_vues.saturating_sub(avant.trames_vues),
            Ordering::Relaxed,
        );
        let limitees = apres.trames_limitees.saturating_sub(avant.trames_limitees);
        if limitees == 0 {
            return false;
        }
        self.trames_limitees.fetch_add(limitees, Ordering::Relaxed);
        self.reduction_max_cdb.fetch_max(
            (-apres.reduction_max_db * 100.0).round().max(0.0) as u64,
            Ordering::Relaxed,
        );
        avant.trames_limitees == 0
    }

    /// Une piste close après que le limiteur a agi.
    pub fn piste_close(&self) {
        self.pistes_limitees.fetch_add(1, Ordering::Relaxed);
    }

    /// Photographie lisible.
    pub fn releve(&self) -> ReleveLimiteur {
        let vues = self.trames_vues.load(Ordering::Relaxed);
        let limitees = self.trames_limitees.load(Ordering::Relaxed);
        ReleveLimiteur {
            trames_vues: vues,
            trames_limitees: limitees,
            pourcentage: if vues == 0 {
                0.0
            } else {
                (100_000.0 * limitees as f64 / vues as f64).round() / 1000.0
            },
            reduction_max_db: -(self.reduction_max_cdb.load(Ordering::Relaxed) as f64) / 100.0
                + 0.0,
            pistes_limitees: self.pistes_limitees.load(Ordering::Relaxed),
        }
    }
}

/// Le limiteur dans le chemin du signal et le rapport.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ReleveLimiteur {
    pub trames_vues: u64,
    pub trames_limitees: u64,
    /// Au millième de pour cent.
    pub pourcentage: f64,
    /// Au centième de dB (≤ 0).
    pub reduction_max_db: f64,
    pub pistes_limitees: u64,
}

/// Les totaux du processus.
pub static REGISTRE: TotauxLimiteur = TotauxLimiteur::new();

/// UNE ligne quand le limiteur agit pour la première fois sur une piste.
pub fn dire_premier(c: &CompteurDuLimiteur) {
    info!(
        moment = "premier",
        trames_vues = c.trames_vues,
        trames_limitees = c.trames_limitees,
        reduction_max_db = format_args!("{:.2}", c.reduction_max_db),
        "dsp_limiteur"
    );
}

/// UNE ligne en fin de piste, avec le total — silencieuse si rien n'a agi.
pub fn dire_fin(c: &CompteurDuLimiteur) {
    if c.trames_limitees == 0 {
        return;
    }
    info!(
        moment = "fin",
        trames_vues = c.trames_vues,
        trames_limitees = c.trames_limitees,
        pourcentage = format_args!("{:.3}", c.pourcentage()),
        reduction_max_db = format_args!("{:.2}", c.reduction_max_db),
        "dsp_limiteur"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_constantes_lineaires_sont_celles_des_dbfs() {
        assert!((20.0 * SEUIL.log10() - SEUIL_DBFS).abs() < 1e-12);
        assert!((20.0 * PLAFOND.log10() - PLAFOND_DBFS).abs() < 1e-12);
    }

    #[test]
    fn la_courbe_est_continue_au_seuil_et_reste_sous_le_plafond() {
        assert!((courbe(SEUIL) - SEUIL).abs() < 1e-15);
        let mut precedent = SEUIL;
        for i in 1..10_000 {
            let e = SEUIL + i as f64 * 0.01;
            let f = courbe(e);
            assert!(f <= PLAFOND, "courbe({e}) = {f} dépasse le plafond");
            assert!(f >= precedent, "courbe non croissante en {e}");
            precedent = f;
        }
    }
}
