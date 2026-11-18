//! #8696 — la mesure qui tranche : pilote ou ordonnancement ?
//!
//! Les rapports de Didier (fils 2805, 2821, 2911) montrent des réveils du fil
//! de rendu WASAPI exclusif espacés de 4 périodes pile (~40 ms pour 10 ms),
//! anneau plein. Deux histoires restent possibles :
//!
//! - **l'événement du pilote arrive tard** (pilote USB, DPC) : le fil attend,
//!   prêt, un événement qui n'est pas signalé ;
//! - **le fil n'est pas servi à temps**, soit pendant l'attente, soit PENDANT
//!   son propre tour (`GetBuffer` / `ReleaseBuffer` qui bloquent dans le
//!   pilote, ou préemption au milieu du rendu).
//!
//! Ce module décide, hors FFI, ce que chaque réveil en retard dit :
//!
//! 1. l'écart entre deux réveils, en nombre de **périodes** (histogramme
//!    2 / 3 / 4 / 5 / 6 et plus) — la quantification exacte à 4 périodes est
//!    le fait central du dossier ;
//! 2. **où** le temps est passé : dans le tour de rendu précédent (`travail`)
//!    ou dans l'attente de l'événement (`attente`) ;
//! 3. de combien de périodes **l'appareil** a avancé pendant l'écart
//!    (`IAudioClock::GetPosition`) et ce qu'il restait en file
//!    (`GetCurrentPadding`) : un appareil qui a avancé de 4 périodes a joué
//!    sans nous (périodes rejouées : les sauts entendus) ; un appareil figé
//!    s'est arrêté avec nous (le pilote ou le DAC a calé, rien n'est perdu).
//!
//! Le journal est écrit **une ligne par rafale**, pas par réveil : une rafale
//! est une suite de retards dont aucun n'est séparé du précédent de plus de
//! [`REPIT_RAFALE_MS`], close au plus tard après [`DUREE_MAX_RAFALE_MS`]
//! pour qu'une panne continue se lise quand même.

#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

use std::sync::atomic::{AtomicU64, Ordering};

/// Cases de l'histogramme : 2, 3, 4, 5, 6 périodes et plus. Un retard de
/// moins de deux périodes (compté en retard dès 1,5) tombe dans la case 2.
pub const CASES: usize = 5;

/// Libellés des cases, pour le rapport.
pub const LIBELLES_DES_CASES: [&str; CASES] = ["2", "3", "4", "5", "6+"];

/// Un retard séparé du précédent de plus de cette durée ouvre une nouvelle
/// rafale.
pub(crate) const REPIT_RAFALE_MS: u64 = 1_000;

/// Au-delà, la rafale est close et journalisée même si les retards
/// continuent : une ligne par 5 s au plus.
pub(crate) const DUREE_MAX_RAFALE_MS: u64 = 5_000;

/// La case d'un écart : arrondi au nombre de périodes le plus proche, borné
/// à 2..=6.
pub fn case_de_l_ecart(ecart_ticks: u64, periode_ticks: u64) -> usize {
    if periode_ticks == 0 {
        return 0;
    }
    let periodes = (ecart_ticks + periode_ticks / 2) / periode_ticks;
    (periodes.clamp(2, 6) - 2) as usize
}

/// L'histogramme partagé entre le fil de rendu et le relevé du rapport.
#[derive(Debug, Default)]
pub struct Histogramme {
    cases: [AtomicU64; CASES],
}

impl Histogramme {
    pub fn ajouter(&self, case: usize) {
        if let Some(c) = self.cases.get(case) {
            c.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn lire(&self) -> [u64; CASES] {
        std::array::from_fn(|i| self.cases[i].load(Ordering::Relaxed))
    }
}

/// Ce qu'un réveil en retard dit de sa cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OuEstLeTemps {
    /// Le tour de rendu précédent (réveil → attente suivante) a duré plus
    /// d'une période : le temps est perdu DANS Tune ou dans un appel au
    /// pilote (`GetBuffer` / `ReleaseBuffer`), pas dans l'attente.
    DansLeRendu,
    /// L'attente de l'événement a duré : événement signalé tard par le
    /// pilote, ou fil prêt mais pas servi.
    DansLAttente,
}

/// Classe un réveil en retard. `travail_ticks` : du réveil précédent à
/// l'entrée dans l'attente ; `attente_ticks` : de l'entrée dans l'attente
/// à ce réveil.
pub fn ou_est_le_temps(travail_ticks: u64, attente_ticks: u64, periode_ticks: u64) -> OuEstLeTemps {
    if travail_ticks > periode_ticks && travail_ticks >= attente_ticks {
        OuEstLeTemps::DansLeRendu
    } else {
        OuEstLeTemps::DansLAttente
    }
}

/// L'avance de l'appareil pendant l'écart, en centièmes de période.
/// `delta_position` : écart de `IAudioClock::GetPosition`, dans l'unité de
/// `frequence_horloge` (`IAudioClock::GetFrequency`).
pub fn avance_en_centiemes_de_periode(
    delta_position: u64,
    frequence_horloge: u64,
    periode_100ns: i64,
) -> Option<u64> {
    if frequence_horloge == 0 || periode_100ns <= 0 {
        return None;
    }
    // position / fréquence = secondes ; / (période / 1e7) = périodes.
    let num = delta_position as u128 * 10_000_000u128 * 100;
    let den = frequence_horloge as u128 * periode_100ns as u128;
    u64::try_from(num / den).ok()
}

/// Ce qu'on sait d'UN réveil en retard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetardObserve {
    pub ecart_ticks: u64,
    pub ou: OuEstLeTemps,
    /// Avance de l'appareil pendant l'écart, en centièmes de période ;
    /// `None` si l'horloge de l'appareil est illisible.
    pub avance_appareil_centiemes: Option<u64>,
    /// `GetCurrentPadding` au réveil, en trames ; `None` si illisible.
    pub rembourrage_trames: Option<u32>,
}

/// Le bilan d'une rafale, journalisé en UNE ligne.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BilanDeRafale {
    pub reveils: u64,
    pub duree_ticks: u64,
    pub histogramme: [u64; CASES],
    pub dans_le_rendu: u64,
    pub dans_l_attente: u64,
    /// Retards où l'appareil a avancé d'au moins 1,5 période : il a joué
    /// sans nous.
    pub appareil_a_avance: u64,
    /// Retards où l'appareil n'a presque pas avancé (< 1,5 période) : il
    /// s'est arrêté avec nous.
    pub appareil_fige: u64,
    pub avance_max_centiemes: Option<u64>,
    pub rembourrage_min: Option<u32>,
    pub rembourrage_max: Option<u32>,
    pub ecart_max_ticks: u64,
}

impl BilanDeRafale {
    fn ouvrir() -> Self {
        Self {
            reveils: 0,
            duree_ticks: 0,
            histogramme: [0; CASES],
            dans_le_rendu: 0,
            dans_l_attente: 0,
            appareil_a_avance: 0,
            appareil_fige: 0,
            avance_max_centiemes: None,
            rembourrage_min: None,
            rembourrage_max: None,
            ecart_max_ticks: 0,
        }
    }

    /// Le verdict lisible de la rafale — ce que le journal et le rapport
    /// disent en clair.
    pub fn verdict(&self) -> &'static str {
        if self.dans_le_rendu * 2 > self.reveils {
            "temps perdu dans le tour de rendu (appel au pilote bloquant ou fil préempté pendant le rendu)"
        } else if self.appareil_fige * 2 > self.reveils {
            "l'appareil s'est arrêté avec le fil (pilote ou DAC qui cale) : aucune période perdue"
        } else if self.appareil_a_avance * 2 > self.reveils {
            "l'appareil a joué sans le fil : événement du pilote signalé tard ou fil prêt non servi ; périodes rejouées"
        } else {
            "indéterminé (horloge de l'appareil illisible ou causes mêlées)"
        }
    }
}

/// Regroupe les réveils en retard en rafales.
#[derive(Debug, Clone)]
pub struct Rafales {
    periode_ticks: u64,
    repit_ticks: u64,
    duree_max_ticks: u64,
    debut: u64,
    dernier: u64,
    courante: Option<BilanDeRafale>,
}

impl Rafales {
    /// `frequence` : `QueryPerformanceFrequency`. `None` si nulle.
    pub fn nouveau(periode_ticks: u64, frequence: i64) -> Option<Self> {
        if frequence <= 0 || periode_ticks == 0 {
            return None;
        }
        let ms = |n: u64| (n as u128 * frequence as u128 / 1_000) as u64;
        Some(Self {
            periode_ticks,
            repit_ticks: ms(REPIT_RAFALE_MS),
            duree_max_ticks: ms(DUREE_MAX_RAFALE_MS),
            debut: 0,
            dernier: 0,
            courante: None,
        })
    }

    fn clore(&mut self) -> Option<BilanDeRafale> {
        let mut b = self.courante.take()?;
        b.duree_ticks = self.dernier.saturating_sub(self.debut);
        Some(b)
    }

    /// À chaque réveil À L'HEURE : clôt la rafale si le répit est atteint.
    /// Une comparaison d'entiers, rien d'autre.
    pub fn a_l_heure(&mut self, maintenant: u64) -> Option<BilanDeRafale> {
        if self.courante.is_some() && maintenant.saturating_sub(self.dernier) > self.repit_ticks {
            return self.clore();
        }
        None
    }

    /// À chaque réveil EN RETARD. Rend le bilan d'une rafale close (la
    /// précédente, après un répit, ou la courante, après sa durée maximale).
    pub fn retard(&mut self, maintenant: u64, r: RetardObserve) -> Option<BilanDeRafale> {
        let mut close = None;
        if self.courante.is_some()
            && (maintenant.saturating_sub(self.dernier) > self.repit_ticks
                || maintenant < self.dernier)
        {
            close = self.clore();
        }
        if self.courante.is_none() {
            self.courante = Some(BilanDeRafale::ouvrir());
            self.debut = maintenant;
        }
        self.dernier = maintenant;
        let periode_ticks = self.periode_ticks;
        if let Some(b) = self.courante.as_mut() {
            b.reveils += 1;
            b.histogramme[case_de_l_ecart(r.ecart_ticks, periode_ticks)] += 1;
            b.ecart_max_ticks = b.ecart_max_ticks.max(r.ecart_ticks);
            match r.ou {
                OuEstLeTemps::DansLeRendu => b.dans_le_rendu += 1,
                OuEstLeTemps::DansLAttente => b.dans_l_attente += 1,
            }
            if let Some(a) = r.avance_appareil_centiemes {
                if a >= 150 {
                    b.appareil_a_avance += 1;
                } else {
                    b.appareil_fige += 1;
                }
                b.avance_max_centiemes = Some(b.avance_max_centiemes.map_or(a, |m| m.max(a)));
            }
            if let Some(p) = r.rembourrage_trames {
                b.rembourrage_min = Some(b.rembourrage_min.map_or(p, |m| m.min(p)));
                b.rembourrage_max = Some(b.rembourrage_max.map_or(p, |m| m.max(p)));
            }
        }
        if close.is_none() && maintenant.saturating_sub(self.debut) >= self.duree_max_ticks {
            close = self.clore();
        }
        close
    }

    /// À l'arrêt du fil : la rafale en cours, s'il y en a une.
    pub fn terminer(&mut self) -> Option<BilanDeRafale> {
        self.clore()
    }
}

/// Les compteurs de cause, cumulés sur la session, pour le rapport.
#[derive(Debug, Default)]
pub struct CausesCumulees {
    pub dans_le_rendu: AtomicU64,
    pub dans_l_attente: AtomicU64,
    pub appareil_a_avance: AtomicU64,
    pub appareil_fige: AtomicU64,
}

/// Leur lecture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CausesLues {
    pub dans_le_rendu: u64,
    pub dans_l_attente: u64,
    pub appareil_a_avance: u64,
    pub appareil_fige: u64,
}

impl CausesCumulees {
    pub fn noter(&self, r: &RetardObserve) {
        match r.ou {
            OuEstLeTemps::DansLeRendu => self.dans_le_rendu.fetch_add(1, Ordering::Relaxed),
            OuEstLeTemps::DansLAttente => self.dans_l_attente.fetch_add(1, Ordering::Relaxed),
        };
        match r.avance_appareil_centiemes {
            Some(a) if a >= 150 => {
                self.appareil_a_avance.fetch_add(1, Ordering::Relaxed);
            }
            Some(_) => {
                self.appareil_fige.fetch_add(1, Ordering::Relaxed);
            }
            None => {}
        }
    }

    pub fn lire(&self) -> CausesLues {
        CausesLues {
            dans_le_rendu: self.dans_le_rendu.load(Ordering::Relaxed),
            dans_l_attente: self.dans_l_attente.load(Ordering::Relaxed),
            appareil_a_avance: self.appareil_a_avance.load(Ordering::Relaxed),
            appareil_fige: self.appareil_fige.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10 MHz : 1 ms = 10 000 ticks ; période de 10 ms = 100 000 ticks.
    const F: i64 = 10_000_000;
    const MS: u64 = 10_000;
    const P: u64 = 100_000;

    fn retard(ecart_ms: u64, ou: OuEstLeTemps, avance: Option<u64>) -> RetardObserve {
        RetardObserve {
            ecart_ticks: ecart_ms * MS,
            ou,
            avance_appareil_centiemes: avance,
            rembourrage_trames: Some(441),
        }
    }

    #[test]
    fn les_ecarts_du_fil_2911_tombent_tous_dans_la_case_4() {
        for us in [38_891u64, 39_006, 39_995, 40_592, 40_813] {
            assert_eq!(case_de_l_ecart(us * 10, P), 2, "{us} µs");
        }
        assert_eq!(case_de_l_ecart(16 * MS, P), 0, "1,6 période : case 2");
        assert_eq!(case_de_l_ecart(30 * MS, P), 1);
        assert_eq!(case_de_l_ecart(50 * MS, P), 3);
        assert_eq!(case_de_l_ecart(500 * MS, P), 4, "6 et plus");
        assert_eq!(case_de_l_ecart(1, 0), 0);
    }

    #[test]
    fn l_histogramme_partage_compte_par_case() {
        let h = Histogramme::default();
        h.ajouter(2);
        h.ajouter(2);
        h.ajouter(4);
        h.ajouter(99); // hors bornes : ignoré
        assert_eq!(h.lire(), [0, 0, 2, 0, 1]);
    }

    #[test]
    fn un_tour_de_rendu_long_designe_le_rendu_et_non_l_attente() {
        assert_eq!(
            ou_est_le_temps(31 * MS, 9 * MS, P),
            OuEstLeTemps::DansLeRendu
        );
        assert_eq!(
            ou_est_le_temps(MS / 10, 39 * MS, P),
            OuEstLeTemps::DansLAttente
        );
        // Contre-épreuve : un tour court reste « attente » même si l'attente
        // est courte elle aussi.
        assert_eq!(
            ou_est_le_temps(5 * MS, 5 * MS, P),
            OuEstLeTemps::DansLAttente
        );
    }

    #[test]
    fn l_avance_de_l_appareil_se_compte_en_periodes() {
        // Horloge en octets/s : 44,1 kHz × 8 octets = 352 800 B/s. 40 ms =
        // 14 112 octets = 4 périodes de 10 ms.
        assert_eq!(
            avance_en_centiemes_de_periode(14_112, 352_800, 100_000),
            Some(400)
        );
        // 10 ms : une période.
        assert_eq!(
            avance_en_centiemes_de_periode(3_528, 352_800, 100_000),
            Some(100)
        );
        assert_eq!(avance_en_centiemes_de_periode(1, 0, 100_000), None);
        assert_eq!(avance_en_centiemes_de_periode(1, 1, 0), None);
    }

    #[test]
    fn une_panne_continue_donne_une_ligne_toutes_les_cinq_secondes_au_plus() {
        // Le rythme du fil 2911 : ~2 retards par seconde pendant 59 s.
        let mut r = Rafales::nouveau(P, F).unwrap();
        let mut bilans = Vec::new();
        for i in 0..118u64 {
            let t = 1_000 * MS + i * 500 * MS;
            if let Some(b) = r.retard(t, retard(40, OuEstLeTemps::DansLAttente, Some(400))) {
                bilans.push(b);
            }
        }
        bilans.extend(r.terminer());
        assert!(
            bilans.len() >= 11 && bilans.len() <= 13,
            "{} lignes",
            bilans.len()
        );
        let total: u64 = bilans.iter().map(|b| b.reveils).sum();
        assert_eq!(total, 118, "aucun réveil perdu par le regroupement");
        assert!(bilans.iter().all(|b| b.histogramme[2] == b.reveils));
        assert!(bilans[0].verdict().contains("joué sans le fil"));
    }

    #[test]
    fn des_retards_isoles_font_chacun_leur_rafale() {
        let mut r = Rafales::nouveau(P, F).unwrap();
        assert!(
            r.retard(0, retard(40, OuEstLeTemps::DansLAttente, None))
                .is_none()
        );
        // Un réveil à l'heure 2 s plus tard clôt la rafale.
        let b = r.a_l_heure(2_000 * MS).expect("rafale close au répit");
        assert_eq!(b.reveils, 1);
        assert!(r.a_l_heure(2_010 * MS).is_none(), "une seule clôture");
        assert!(
            r.retard(5_000 * MS, retard(30, OuEstLeTemps::DansLeRendu, Some(100)))
                .is_none()
        );
        let b = r.terminer().expect("la rafale en cours sort à l'arrêt");
        assert_eq!(b.histogramme, [0, 1, 0, 0, 0]);
        assert_eq!(b.dans_le_rendu, 1);
        assert_eq!(b.appareil_fige, 1);
        assert!(b.verdict().contains("tour de rendu"));
        assert!(r.terminer().is_none());
    }

    #[test]
    fn les_retards_rapproches_sont_une_seule_rafale() {
        let mut r = Rafales::nouveau(P, F).unwrap();
        for i in 0..5u64 {
            assert!(
                r.retard(
                    i * 300 * MS,
                    retard(40, OuEstLeTemps::DansLAttente, Some(105))
                )
                .is_none()
            );
        }
        // Un réveil à l'heure dans le répit ne clôt rien.
        assert!(r.a_l_heure(1_500 * MS).is_none());
        let b = r.a_l_heure(2_300 * MS).unwrap();
        assert_eq!(b.reveils, 5);
        assert_eq!(b.duree_ticks, 1_200 * MS);
        assert_eq!(b.appareil_fige, 5);
        assert_eq!(b.rembourrage_min, Some(441));
        assert!(b.verdict().contains("s'est arrêté"));
    }

    #[test]
    fn les_causes_cumulees_comptent_chaque_retard() {
        let c = CausesCumulees::default();
        c.noter(&retard(40, OuEstLeTemps::DansLAttente, Some(400)));
        c.noter(&retard(40, OuEstLeTemps::DansLeRendu, Some(90)));
        c.noter(&retard(40, OuEstLeTemps::DansLAttente, None));
        assert_eq!(
            c.lire(),
            CausesLues {
                dans_le_rendu: 1,
                dans_l_attente: 2,
                appareil_a_avance: 1,
                appareil_fige: 1,
            }
        );
    }

    /// Le branchement dans la boucle de rendu : la mesure est faite APRÈS le
    /// réveil et AVANT le tampon, la ligne de rafale APRÈS le tampon, et la
    /// rafale en cours sort à l'arrêt du fil. Comparé sans blancs.
    #[test]
    fn la_boucle_de_rendu_mesure_et_journalise_par_rafale() {
        let source: String = include_str!("wasapi_exclusive.rs")
            .split_whitespace()
            .collect();
        let boucle = source.find("whilerunning.load(Ordering::SeqCst){");
        let avant_attente =
            source.find("avant_attente=(unsafe{ffi::QueryPerformanceCounter(&mutt)}");
        let attente = source.find("WaitForMultipleObjects(2,poignees.as_ptr()");
        let histogramme =
            source.find("histogramme.ajouter(case_de_l_ecart(ecart_ticks,periode_ticks));");
        let rembourrage = source.find("rembourrage_courant(audio_client_mesure)");
        let tampon = source.find("letget_buf:GetBufferFn");
        let ligne = source.find("journaliser_la_rafale(bilan,frequence");
        let terminer = source.find("rafales.as_mut().and_then(Rafales::terminer)");
        let fin = source.find("info!(\"wasapi_exclusive_render_thread_stopped\");");
        for (nom, p) in [
            ("boucle", boucle),
            ("entrée dans l'attente", avant_attente),
            ("attente", attente),
            ("histogramme", histogramme),
            ("rembourrage", rembourrage),
            ("tampon", tampon),
            ("ligne de rafale", ligne),
            ("rafale à l'arrêt", terminer),
            ("fin", fin),
        ] {
            assert!(p.is_some(), "absent de wasapi_exclusive.rs : {nom}");
        }
        assert!(boucle < avant_attente && avant_attente < attente);
        assert!(attente < histogramme && histogramme < tampon);
        assert!(rembourrage < tampon && tampon < ligne);
        assert!(ligne < terminer && terminer < fin);
    }
}
