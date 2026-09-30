//! #4357 — les réveils EN RETARD de la boucle de rendu WASAPI exclusive.
//!
//! ## Le fait
//!
//! En exclusif événementiel, le pilote signale son événement une fois par
//! période (10 ms chez Didier, `buffer_frames=442` à 44,1 kHz) et la boucle a
//! jusqu'à la période suivante pour remettre le tampon. Un fil réveillé trop
//! tard laisse le pilote rejouer ou taire une période : c'est une coupure
//! audible, et **aucun compteur de Tune ne la voyait**. `underruns` ne compte
//! que l'anneau vide, `deadline_misses` que l'attente de 2 000 ms épuisée. Le
//! 29/09, 905 s de lecture DTS aux « très nombreux » décrochages se sont
//! refermées sur `underruns=0 deadline_misses=0 callback_errors=0`.
//!
//! ## Ce que ce module mesure
//!
//! L'écart, en ticks `QueryPerformanceCounter`, entre deux réveils successifs
//! par l'événement du pilote. Au-delà d'**une période et demie**, le réveil est
//! compté en retard ; au-delà de **deux périodes**, il est grave et mérite une
//! ligne de journal — limitée en débit, parce qu'elle s'écrit depuis le fil de
//! rendu. La décision est pure, hors FFI, pour être jugée par `cargo test` sur
//! toutes les plateformes de CI ; seule la lecture de l'horloge est Windows.
//!
//! ⚠️ Ce que ce compteur ne prouve pas : qu'un réveil en retard soit entendu,
//! ni qu'aucune coupure ne vienne d'ailleurs. Il rend mesurable une hypothèse,
//! il ne la tranche pas à lui seul.

// Seule la boucle de `wasapi_exclusive` (feature `local-audio`) s'en sert.
#![cfg_attr(not(feature = "local-audio"), allow(dead_code))]

/// Le nom de tâche MMCSS demandé pour le fil de rendu, terminé par un nul,
/// en UTF-16 comme l'attend `AvSetMmThreadCharacteristicsW`.
///
/// « Pro Audio » est la tâche que Microsoft prévoit pour le rendu audio à
/// faible latence (clé `...\Multimedia\SystemProfile\Tasks\Pro Audio`).
pub(crate) const TACHE_MMCSS: &str = "Pro Audio";

/// `TACHE_MMCSS` en UTF-16 terminé par un nul.
pub(crate) fn tache_mmcss_utf16() -> Vec<u16> {
    TACHE_MMCSS
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

/// Intervalle minimal entre deux lignes de journal « réveil très en retard »,
/// en millisecondes. Le compteur, lui, compte TOUT.
pub(crate) const INTERVALLE_JOURNAL_MS: u64 = 5_000;

/// Ce que vaut un réveil, comparé au précédent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reveil {
    /// Premier réveil mesuré, ou premier après une remise à zéro : rien à
    /// comparer.
    Premier,
    /// Dans la période et demie.
    AHeure,
    /// Au-delà d'une période et demie, dans les deux périodes.
    EnRetard { ecart_ticks: u64 },
    /// Au-delà de deux périodes. `journaliser` dit si la ligne de journal est
    /// due (limitation de débit).
    TresEnRetard { ecart_ticks: u64, journaliser: bool },
}

/// Suit les réveils successifs de la boucle de rendu.
#[derive(Debug, Clone)]
pub(crate) struct SuiviDesReveils {
    periode_ticks: u64,
    intervalle_journal_ticks: u64,
    dernier: Option<u64>,
    derniere_ligne: Option<u64>,
    en_retard: u64,
    ecart_max_ticks: u64,
}

/// Convertit une durée en 100 ns (`REFERENCE_TIME`) en ticks d'une horloge
/// de fréquence `frequence` Hz. `None` si l'un des deux est nul ou négatif.
pub(crate) fn ticks_depuis_100ns(duree_100ns: i64, frequence: i64) -> Option<u64> {
    if duree_100ns <= 0 || frequence <= 0 {
        return None;
    }
    let ticks = (duree_100ns as u128 * frequence as u128) / 10_000_000u128;
    u64::try_from(ticks).ok().filter(|t| *t > 0)
}

/// Convertit des ticks en microsecondes, pour le journal.
pub(crate) fn microsecondes(ticks: u64, frequence: i64) -> u64 {
    if frequence <= 0 {
        return 0;
    }
    u64::try_from(ticks as u128 * 1_000_000u128 / frequence as u128).unwrap_or(u64::MAX)
}

impl SuiviDesReveils {
    /// `periode_100ns` : la période retenue à l'`Initialize` ; `frequence` :
    /// `QueryPerformanceFrequency`. `None` si la conversion est impossible —
    /// la boucle rend alors sans mesurer, jamais elle ne refuse de jouer.
    pub(crate) fn nouveau(periode_100ns: i64, frequence: i64) -> Option<Self> {
        let periode_ticks = ticks_depuis_100ns(periode_100ns, frequence)?;
        let intervalle_journal_ticks =
            ticks_depuis_100ns(INTERVALLE_JOURNAL_MS as i64 * 10_000, frequence)?;
        Some(Self {
            periode_ticks,
            intervalle_journal_ticks,
            dernier: None,
            derniere_ligne: None,
            en_retard: 0,
            ecart_max_ticks: 0,
        })
    }

    /// À appeler à chaque réveil par l'événement du pilote, avec l'instant
    /// `QueryPerformanceCounter` lu juste après l'attente.
    pub(crate) fn observer(&mut self, maintenant: u64) -> Reveil {
        let Some(precedent) = self.dernier.replace(maintenant) else {
            return Reveil::Premier;
        };
        // Une horloge qui recule ne vaut rien : on repart de cet instant.
        let Some(ecart) = maintenant.checked_sub(precedent) else {
            return Reveil::Premier;
        };
        self.ecart_max_ticks = self.ecart_max_ticks.max(ecart);
        // Comparaisons entières : 2·écart > 3·période ⇔ écart > 1,5 période.
        if ecart.saturating_mul(2) <= self.periode_ticks.saturating_mul(3) {
            return Reveil::AHeure;
        }
        self.en_retard += 1;
        if ecart <= self.periode_ticks.saturating_mul(2) {
            return Reveil::EnRetard { ecart_ticks: ecart };
        }
        let journaliser = match self.derniere_ligne {
            None => true,
            Some(ligne) => maintenant.saturating_sub(ligne) >= self.intervalle_journal_ticks,
        };
        if journaliser {
            self.derniere_ligne = Some(maintenant);
        }
        Reveil::TresEnRetard {
            ecart_ticks: ecart,
            journaliser,
        }
    }

    /// Oublie le dernier réveil : à appeler quand l'attente a expiré
    /// (`deadline_misses`, déjà compté à part) pour ne pas compter en plus
    /// l'écart de 2 000 ms qui la suit.
    pub(crate) fn oublier_le_dernier(&mut self) {
        self.dernier = None;
    }

    pub(crate) fn reveils_en_retard(&self) -> u64 {
        self.en_retard
    }

    pub(crate) fn ecart_max_ticks(&self) -> u64 {
        self.ecart_max_ticks
    }

    pub(crate) fn periode_ticks(&self) -> u64 {
        self.periode_ticks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Une horloge à 10 MHz : un tick = 100 ns, la période de Didier
    /// (`period_100ns=100227`) vaut donc 100 227 ticks.
    const F: i64 = 10_000_000;
    const P: u64 = 100_227;

    fn suivi() -> SuiviDesReveils {
        SuiviDesReveils::nouveau(P as i64, F).expect("période valide")
    }

    #[test]
    fn la_periode_se_convertit_en_ticks_de_l_horloge() {
        assert_eq!(ticks_depuis_100ns(100_227, 10_000_000), Some(100_227));
        // Horloge QPC courante à 3 MHz environ : 10 ms = 30 000 ticks.
        assert_eq!(ticks_depuis_100ns(100_000, 3_000_000), Some(30_000));
        assert_eq!(ticks_depuis_100ns(0, F), None);
        assert_eq!(ticks_depuis_100ns(100_000, 0), None);
        assert_eq!(ticks_depuis_100ns(-5, F), None);
        assert!(SuiviDesReveils::nouveau(0, F).is_none());
    }

    #[test]
    fn le_premier_reveil_n_a_rien_a_comparer() {
        let mut s = suivi();
        assert_eq!(s.observer(1_000), Reveil::Premier);
        assert_eq!(s.reveils_en_retard(), 0);
    }

    /// Le cœur du seuil : jusqu'à 1,5 période inclus, le réveil est à l'heure.
    #[test]
    fn jusqu_a_une_periode_et_demie_le_reveil_est_a_l_heure() {
        let mut s = suivi();
        s.observer(0);
        assert_eq!(s.observer(P), Reveil::AHeure);
        let limite = P * 3 / 2; // 150 340 : 2·150 340 = 300 680 ≤ 300 681
        assert_eq!(s.observer(P + limite), Reveil::AHeure);
        assert_eq!(s.reveils_en_retard(), 0);
    }

    #[test]
    fn au_dela_d_une_periode_et_demie_le_reveil_est_en_retard() {
        let mut s = suivi();
        s.observer(0);
        let ecart = P * 3 / 2 + 1;
        assert_eq!(s.observer(ecart), Reveil::EnRetard { ecart_ticks: ecart });
        assert_eq!(s.reveils_en_retard(), 1);
        // Deux périodes pile : encore « en retard », pas « très en retard ».
        let t = ecart + 2 * P;
        assert_eq!(s.observer(t), Reveil::EnRetard { ecart_ticks: 2 * P });
        assert_eq!(s.reveils_en_retard(), 2);
    }

    #[test]
    fn au_dela_de_deux_periodes_le_retard_est_journalise_une_fois_par_intervalle() {
        let mut s = suivi();
        s.observer(0);
        let grave = 2 * P + 1;
        assert_eq!(
            s.observer(grave),
            Reveil::TresEnRetard {
                ecart_ticks: grave,
                journaliser: true
            }
        );
        // Un second retard grave juste après : compté, pas journalisé.
        assert_eq!(
            s.observer(2 * grave),
            Reveil::TresEnRetard {
                ecart_ticks: grave,
                journaliser: false
            }
        );
        assert_eq!(s.reveils_en_retard(), 2);
        // Cinq secondes après la première ligne (50 000 000 ticks à 10 MHz) :
        // de nouveau due.
        let loin = grave + 50_000_000;
        assert!(matches!(
            s.observer(loin),
            Reveil::TresEnRetard {
                journaliser: true,
                ..
            }
        ));
        assert_eq!(s.reveils_en_retard(), 3);
        assert_eq!(s.ecart_max_ticks(), loin - 2 * grave);
    }

    /// L'expiration de l'attente est déjà `deadline_misses` : l'écart qui la
    /// suit ne doit pas être compté une seconde fois.
    #[test]
    fn une_echeance_manquee_ne_compte_pas_un_retard_de_plus() {
        let mut s = suivi();
        s.observer(0);
        s.oublier_le_dernier();
        assert_eq!(s.observer(20_000_000), Reveil::Premier);
        assert_eq!(s.reveils_en_retard(), 0);
    }

    #[test]
    fn une_horloge_qui_recule_ne_fabrique_pas_de_retard() {
        let mut s = suivi();
        s.observer(1_000_000);
        assert_eq!(s.observer(10), Reveil::Premier);
        assert_eq!(s.reveils_en_retard(), 0);
    }

    #[test]
    fn les_microsecondes_du_journal() {
        assert_eq!(microsecondes(100_227, F), 10_022);
        assert_eq!(microsecondes(30_000, 3_000_000), 10_000);
        assert_eq!(microsecondes(5, 0), 0);
    }

    #[test]
    fn la_tache_mmcss_est_pro_audio_terminee_par_un_nul() {
        let t = tache_mmcss_utf16();
        assert_eq!(t.last(), Some(&0));
        assert_eq!(String::from_utf16(&t[..t.len() - 1]).unwrap(), "Pro Audio");
    }

    /// Le branchement — sans lui, le suivi ci-dessus ne garde rien. Comparé
    /// sans blancs : rustfmt coupe librement un appel long.
    #[test]
    fn la_boucle_de_rendu_est_promue_mmcss_mesure_ses_reveils_et_rend_la_promotion() {
        let source: String = include_str!("wasapi_exclusive.rs")
            .split_whitespace()
            .collect();
        let promotion =
            source.find("AvSetMmThreadCharacteristicsW(tache.as_ptr(),&mutindice_tache)");
        let boucle = source.find("whilerunning.load(Ordering::SeqCst){");
        let mesure = source.find("suivi.observer(");
        let rendu = source.find("AvRevertMmThreadCharacteristics(mmcss)");
        let fin = source.find("info!(\"wasapi_exclusive_render_thread_stopped\");");
        assert!(
            promotion.is_some(),
            "le fil de rendu n'est pas promu en MMCSS"
        );
        assert!(boucle.is_some() && mesure.is_some() && rendu.is_some() && fin.is_some());
        assert!(promotion < boucle, "la promotion doit précéder la boucle");
        assert!(boucle < mesure, "la mesure vit dans la boucle");
        assert!(
            mesure < rendu && rendu < fin,
            "la promotion est rendue à la sortie du fil"
        );
        assert!(
            source.contains("reveils_en_retard=self.late_wakeup_count(),"),
            "wasapi_exclusive_stopped doit porter le compteur de réveils en retard"
        );
    }
}
