//! Mesurer la dérive d'horloge entre l'entrée et la sortie, et décider des
//! reprises du tampon — pur, sans périphérique ni horloge réelle.
//!
//! ## Le choix : un tampon avec reprise, pas un rééchantillonnage adaptatif
//!
//! L'entrée et la sortie ont chacune leur quartz. L'écart se compte en ppm :
//! quelques dizaines au plus entre deux interfaces USB. À 20 ppm, un tampon
//! s'écarte de 1 ms toutes les 50 s, de 72 ms par heure.
//!
//! * Un rééchantillonnage adaptatif (`rubato`) absorbe l'écart en continu,
//!   sans raccord audible, mais il TRANSFORME chaque échantillon : plus rien
//!   n'est bit-perfect, à aucun moment, pour corriger un écart qui, lui, ne se
//!   manifeste qu'au bout de plusieurs heures.
//! * Un tampon avec reprise laisse passer le signal TEL QUEL, octet pour octet,
//!   et ne corrige qu'au moment où le tampon sort de sa fenêtre : il retire
//!   (ou comble de silence) d'un coup l'écart accumulé. Avec une fenêtre de
//!   ±500 ms autour du niveau de régime, une reprise tombe toutes les
//!   `0,5 s / dérive` : à 20 ppm, une toutes les 7 heures.
//!
//! La mesure sur le Mac Studio (25-26/09/2026, PR #5051) tranche :
//!
//! * Yeti X (USB, 48 kHz) contre l'horloge de l'hôte : −6,1 / −6,2 / −6,2 ppm
//!   sur trois captures de 10 à 40 min — stable ;
//! * contre ce qu'une zone locale a JOUÉ (sortie Loopback Audio) : entre −38
//!   et +18 ppm sur une fenêtre de 10 min — la position de zone n'est relevée
//!   qu'à la milliseconde, chaque seconde : c'est une borne, pas un chiffre ;
//! * 26 min en direct vers la zone : 0 reprise, 0 débordement, 0
//!   sous-remplissage.
//!
//! Au pire mesuré (40 ppm), une reprise tombe toutes les 3,5 heures ; à
//! 6 ppm, toutes les 23 heures. Le tampon avec reprise est retenu ; le chemin
//! du signal dit « bit-perfect » tant qu'aucune reprise n'a eu lieu, et le
//! retire à la première. Une reprise a été observée pour une autre cause :
//! une machine saturée (charge > 600) qui affame la sortie ; elle est
//! comptée et journalisée comme les autres.
//!
//! ## La mesure
//!
//! Deux séries de points sur l'horloge de l'hôte : trames CAPTÉES (instants du
//! pilote) et octets CONSOMMÉS (ce que la sortie a tiré de la session). Une
//! droite des moindres carrés sur une fenêtre glissante donne chaque débit ;
//! leur rapport est la dérive. Elle n'a de sens que quand c'est le
//! consommateur qui donne le rythme — le tampon de régime le dit.

use std::collections::VecDeque;

/// Une série (instant en secondes, quantité cumulée) sur une fenêtre glissante.
#[derive(Debug, Clone)]
pub struct Serie {
    points: VecDeque<(f64, f64)>,
    duree_s: f64,
}

impl Serie {
    pub fn new(duree_s: f64) -> Self {
        Self {
            points: VecDeque::new(),
            duree_s,
        }
    }

    pub fn ajouter(&mut self, t: f64, x: f64) {
        if self.points.back().is_some_and(|&(t0, _)| t <= t0) {
            return;
        }
        self.points.push_back((t, x));
        while self
            .points
            .front()
            .is_some_and(|&(t0, _)| t - t0 > self.duree_s)
        {
            self.points.pop_front();
        }
    }

    pub fn etendue_s(&self) -> f64 {
        match (self.points.front(), self.points.back()) {
            (Some(a), Some(b)) => b.0 - a.0,
            _ => 0.0,
        }
    }

    /// Pente des moindres carrés (quantité par seconde), sur au moins
    /// `etendue_min_s` secondes.
    pub fn pente(&self, etendue_min_s: f64) -> Option<f64> {
        if self.points.len() < 3 || self.etendue_s() < etendue_min_s {
            return None;
        }
        let n = self.points.len() as f64;
        // Centrer évite la perte de précision sur des cumuls de 10^9.
        let (t0, x0) = self.points[0];
        let (mut st, mut sx) = (0.0, 0.0);
        for &(t, x) in &self.points {
            st += t - t0;
            sx += x - x0;
        }
        let (mt, mx) = (st / n, sx / n);
        let (mut num, mut den) = (0.0, 0.0);
        for &(t, x) in &self.points {
            let dt = t - t0 - mt;
            num += dt * (x - x0 - mx);
            den += dt * dt;
        }
        (den > 0.0).then(|| num / den)
    }
}

/// En ppm : de combien `a` va plus vite que `b`.
pub fn ppm(a: f64, b: f64) -> Option<f64> {
    (b > 0.0 && a > 0.0).then(|| (a / b - 1.0) * 1e6)
}

/// Ce que la politique décide pour le tampon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Rien,
    /// Retirer ce nombre d'octets du tampon (l'entrée va plus vite).
    Retirer(usize),
    /// Servir ce nombre d'octets de silence (la sortie va plus vite).
    Combler(usize),
}

/// La politique du tampon avec reprise, en octets.
#[derive(Debug, Clone)]
pub struct Reprise {
    octets_par_trame: usize,
    /// Niveau de régime, fixé après la période d'observation.
    pub reference: Option<usize>,
    observations: Vec<usize>,
    /// Moyenne glissante du niveau (lisse le grain des blocs).
    lisse: Option<f64>,
    pub marge_haute: usize,
    /// Sous ce niveau de régime, la sortie n'est pas régulée par nous : elle
    /// tire tout ce qui arrive, rien à combler de ce côté.
    pub regime_min: usize,
    /// Secondes (depuis l'amorce) pendant lesquelles on n'observe RIEN : la
    /// sortie en aval se remplit, le niveau n'est pas encore celui du régime.
    pub stabilisation_s: f64,
    /// Durée de l'observation qui fixe le niveau de régime (médiane).
    pub observation_s: f64,
}

impl Reprise {
    /// `octets_par_seconde` du format servi.
    pub fn new(octets_par_trame: usize, octets_par_seconde: usize) -> Self {
        Self {
            octets_par_trame,
            reference: None,
            observations: Vec::new(),
            lisse: None,
            marge_haute: octets_par_seconde / 2,
            regime_min: octets_par_seconde * 3 / 20,
            stabilisation_s: 10.0,
            observation_s: 10.0,
        }
    }

    fn trames(&self, octets: usize) -> usize {
        octets / self.octets_par_trame * self.octets_par_trame
    }

    /// Le consommateur donne-t-il le rythme (un tampon de régime existe) ?
    pub fn regule(&self) -> bool {
        self.reference.is_some_and(|r| r >= self.regime_min)
    }

    /// Une observation du niveau, avant une lecture, `t_s` secondes après
    /// l'amorce.
    ///
    /// Le niveau de régime n'est fixé qu'APRÈS la stabilisation : pendant
    /// que l'amorce se déverse dans la sortie, l'anneau se vide en quelques
    /// secondes, et une référence prise là déclencherait des reprises à tort
    /// (constaté sur le Mac Studio : 12 reprises en 8 s avant ce garde-fou).
    pub fn observer(&mut self, niveau: usize, t_s: f64) -> Decision {
        let lisse = match self.lisse {
            None => niveau as f64,
            Some(l) => l + (niveau as f64 - l) * 0.02,
        };
        self.lisse = Some(lisse);
        let Some(reference) = self.reference else {
            if t_s >= self.stabilisation_s {
                self.observations.push(niveau);
            }
            if t_s >= self.stabilisation_s + self.observation_s && !self.observations.is_empty() {
                let mut o = std::mem::take(&mut self.observations);
                o.sort_unstable();
                self.reference = Some(o[o.len() / 2]);
                self.lisse = Some(o[o.len() / 2] as f64);
            }
            return Decision::Rien;
        };
        let lisse = lisse as usize;
        if lisse > reference + self.marge_haute {
            self.lisse = Some(reference as f64);
            return Decision::Retirer(self.trames(niveau.saturating_sub(reference)));
        }
        if self.regule() && lisse < reference / 2 {
            self.lisse = Some(reference as f64);
            return Decision::Combler(self.trames(reference - lisse));
        }
        Decision::Rien
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_pente_retrouve_un_debit_a_une_fraction_de_ppm_malgre_le_grain() {
        // 48 kHz décalé de +25 ppm, observé par blocs de 512 trames avec une
        // gigue d'horodatage de ±1 ms.
        let vrai = 48_000.0 * (1.0 + 25e-6);
        let mut s = Serie::new(600.0);
        let mut trames = 0.0;
        let mut i = 0u64;
        while trames / vrai < 600.0 {
            let t = trames / vrai + if i % 2 == 0 { 0.001 } else { -0.001 };
            s.ajouter(t, trames);
            trames += 512.0;
            i += 1;
        }
        let p = s.pente(60.0).unwrap();
        let mesure = ppm(p, 48_000.0).unwrap();
        assert!((mesure - 25.0).abs() < 0.5, "{mesure}");
    }

    #[test]
    fn une_fenetre_trop_courte_ne_mesure_rien() {
        let mut s = Serie::new(600.0);
        s.ajouter(0.0, 0.0);
        s.ajouter(1.0, 10.0);
        s.ajouter(2.0, 20.0);
        assert_eq!(s.pente(60.0), None);
        assert_eq!(s.pente(1.0), Some(10.0));
    }

    /// Octets par trame et par seconde de 48 kHz / 24 bits / stéréo.
    const T: usize = 6;
    const S: usize = 288_000;

    /// Une horloge d'observations toutes les 20 ms.
    struct Horloge(f64);
    impl Horloge {
        fn t(&mut self) -> f64 {
            self.0 += 0.02;
            self.0
        }
    }

    /// Jusqu'à la fin de l'observation, au niveau donné.
    fn regime(r: &mut Reprise, niveau: usize) -> Horloge {
        let mut h = Horloge(0.0);
        while h.0 <= r.stabilisation_s + r.observation_s {
            let t = h.t();
            assert_eq!(r.observer(niveau, t), Decision::Rien);
        }
        assert!(r.reference.is_some());
        h
    }

    #[test]
    fn rien_ne_bouge_tant_que_le_niveau_reste_dans_sa_fenetre() {
        let mut r = Reprise::new(T, S);
        let mut h = regime(&mut r, S / 2);
        assert!(r.regule());
        for k in 0..10_000 {
            // ±100 ms de grain autour du régime.
            let n = S / 2 + (k % 3) * S / 10 - S / 10;
            assert_eq!(r.observer(n, h.t()), Decision::Rien);
        }
    }

    #[test]
    fn une_entree_plus_rapide_se_reprend_en_retirant_l_ecart_d_un_coup() {
        let mut r = Reprise::new(T, S);
        let mut h = regime(&mut r, S / 2);
        let mut decision = Decision::Rien;
        let mut n = S / 2;
        while decision == Decision::Rien {
            n += T;
            decision = r.observer(n, h.t());
        }
        let Decision::Retirer(k) = decision else {
            panic!("{decision:?}")
        };
        assert_eq!(k % T, 0, "trames entières");
        assert!(n - k <= S / 2 + T, "ramené au régime");
        assert!(n > S / 2 + S / 2, "pas avant la marge");
    }

    #[test]
    fn une_sortie_plus_rapide_se_reprend_en_comblant_quand_elle_est_regulee() {
        let mut r = Reprise::new(T, S);
        let mut h = regime(&mut r, S / 2);
        let mut n = S / 2;
        let decision = loop {
            n -= T;
            let d = r.observer(n, h.t());
            if d != Decision::Rien {
                break d;
            }
        };
        assert!(matches!(decision, Decision::Combler(k) if k % T == 0 && k > 0));
    }

    #[test]
    fn une_sortie_non_regulee_ne_recoit_jamais_de_silence_invente() {
        let mut r = Reprise::new(T, S);
        // Le consommateur tire tout : le tampon reste presque vide.
        let mut h = regime(&mut r, T * 10);
        assert!(!r.regule());
        for _ in 0..10_000 {
            assert_eq!(r.observer(0, h.t()), Decision::Rien);
        }
    }

    /// Le cas du Mac Studio : l'amorce (2,5 s) se déverse dans la sortie,
    /// l'anneau passe de 2,5 s à ~0,3 s en quelques secondes. Rien ne doit
    /// être repris pendant ce déversement, et le régime est le niveau STABLE.
    #[test]
    fn le_deversement_de_l_amorce_ne_fixe_pas_le_regime_et_ne_declenche_rien() {
        let mut r = Reprise::new(T, S);
        let mut h = Horloge(0.0);
        let stable = S * 3 / 10;
        while h.0 < 25.0 {
            let t = h.t();
            // 2,5 s au départ, décroissance jusqu'au régime en 5 s.
            let n = if t < 5.0 {
                (S as f64 * (2.5 - (2.5 - 0.3) * t / 5.0)) as usize / T * T
            } else {
                stable
            };
            assert_eq!(r.observer(n, t), Decision::Rien, "à {t:.2} s");
        }
        assert_eq!(r.reference, Some(stable));
    }
}
