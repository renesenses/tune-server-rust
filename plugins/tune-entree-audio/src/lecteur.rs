//! Le lecteur que la pompe de l'hôte appelle : il sort de l'anneau ce que la
//! capture y a mis, mesure la dérive, et applique la politique de reprise.
//!
//! ## L'amorce
//!
//! Le premier `read` attend que l'anneau tienne `amorce` d'audio, puis le
//! lecteur rend tout d'un coup. La sortie en aval se remplit alors jusqu'à
//! refuser d'en prendre davantage : dès cet instant c'est ELLE qui donne le
//! rythme (celui de son horloge), l'excédent reste dans l'anneau, et c'est ce
//! niveau de régime que la politique de reprise surveille. Sans amorce, la
//! sortie tirerait tout ce qui arrive, et la dérive s'accumulerait dans son
//! tampon, hors de vue. Prix : `amorce` de latence.

use std::io::{self, Read};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use tracing::{info, warn};
use tune_core::source_pcm::{Compensation, Consommation, EtatDirect};

use crate::anneau::{Anneau, Fin};
use crate::derive::{Decision, Reprise, Serie, ppm};

/// Ce que la mesure de la dérive a établi.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct Derives {
    /// Entrée contre consommateur, en ppm (positif : l'entrée va plus vite).
    pub derive_ppm: Option<f64>,
    /// Entrée contre l'horloge de l'hôte.
    pub capture_contre_hote_ppm: Option<f64>,
    /// Consommateur contre l'horloge de l'hôte.
    pub consommation_contre_hote_ppm: Option<f64>,
    /// Le consommateur donne-t-il le rythme (tampon de régime établi) ?
    pub consommateur_regule: bool,
    /// Tampon de régime, en ms (fixé après l'observation).
    pub tampon_de_regime_ms: Option<f64>,
    /// Niveau courant de l'anneau, en ms.
    pub tampon_ms: f64,
    /// Capté mais pas encore tiré par le consommateur, en ms : la latence
    /// jusqu'à l'ENTRÉE de la sortie (son propre tampon en sus).
    pub latence_ms: f64,
    /// Durée couverte par la fenêtre de mesure, en s.
    pub fenetre_s: f64,
    /// Position JOUÉE par la zone contre l'horloge de l'hôte (sortie locale :
    /// trames rendues par le pilote, à la milliseconde).
    pub position_contre_hote_ppm: Option<f64>,
    /// D'où vient `derive_ppm` : `position_de_la_zone` (ce que la sortie a
    /// réellement joué) ou `octets_consommes` (quand la sortie régule).
    pub derive_source: Option<&'static str>,
}

/// Origine commune des instants hôte (positions de zone).
static ORIGINE: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

/// Dérive de l'entrée contre la zone : trames captées par seconde d'hôte
/// contre millisecondes JOUÉES par seconde d'hôte.
pub fn derive_contre_la_zone(
    capte_trames_par_s: f64,
    position_ms_par_s: f64,
    frequence: u32,
) -> Option<f64> {
    ppm(
        capte_trames_par_s,
        position_ms_par_s * frequence as f64 / 1000.0,
    )
}

/// Compteurs d'une session en direct, partagés avec `/etat` et publiés au
/// chemin du signal.
#[derive(Default)]
pub struct Mesures {
    pub reprises: AtomicU64,
    pub trames_retirees: AtomicU64,
    pub trames_comblees: AtomicU64,
    /// La capture n'a pas fourni à temps (attente > [`ATTENTE_SOUS_REMPLISSAGE`]).
    pub sous_remplissages: AtomicU64,
    pub derives: Mutex<Derives>,
    /// (instant hôte, position jouée par la zone en ms), une par seconde.
    positions: Mutex<Option<Serie>>,
}

impl Mesures {
    /// Une position JOUÉE par la zone, relevée maintenant.
    pub fn noter_position(&self, position_ms: i64) {
        let t = ORIGINE.elapsed().as_secs_f64();
        let mut p = self.positions.lock().unwrap_or_else(|e| e.into_inner());
        p.get_or_insert_with(|| Serie::new(600.0))
            .ajouter(t, position_ms as f64);
    }

    fn pente_des_positions(&self) -> Option<f64> {
        self.positions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .and_then(|s| s.pente(60.0))
    }

    pub fn derives(&self) -> Derives {
        self.derives
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl EtatDirect for Mesures {
    fn compensation(&self) -> Compensation {
        Compensation {
            methode: "tampon_avec_reprise",
            reechantillonne: false,
            reprises: self.reprises.load(Ordering::Relaxed),
            derive_ppm: self.derives().derive_ppm,
        }
    }
}

/// Au-delà, une attente de données est un sous-remplissage de la capture.
pub const ATTENTE_SOUS_REMPLISSAGE: Duration = Duration::from_millis(100);
/// Au-delà, l'entrée est tenue pour muette : le flux s'arrête en erreur.
pub const ENTREE_MUETTE: Duration = Duration::from_secs(5);
/// Tenue de la session en silence pendant une relance de fréquence.
pub const TENUE_DE_RELANCE: Duration = Duration::from_secs(10);

pub struct Reglages {
    pub amorce: Duration,
    /// Durée d'un bloc rendu à la pompe.
    pub quantum: Duration,
    /// Fenêtre glissante de la mesure de dérive.
    pub fenetre: Duration,
}

impl Default for Reglages {
    fn default() -> Self {
        Self {
            amorce: Duration::from_millis(2_500),
            quantum: Duration::from_millis(20),
            fenetre: Duration::from_secs(600),
        }
    }
}

pub struct LecteurCapture {
    anneau: Arc<Anneau>,
    generation: u64,
    mesures: Arc<Mesures>,
    consommation: Consommation,
    octets_par_seconde: usize,
    amorce: usize,
    amorce_faite: bool,
    quantum: usize,
    reprise: Reprise,
    capte: Serie,
    consomme: Serie,
    debut: Instant,
    dernier_echantillon: Option<Instant>,
    emis: u64,
    silence_a_servir: usize,
    relance_depuis: Option<Instant>,
    muette_depuis: Option<Instant>,
    /// Quand l'amorce a été servie : l'origine des temps de la reprise.
    amorce_servie: Option<Instant>,
}

impl LecteurCapture {
    pub fn new(
        anneau: Arc<Anneau>,
        mesures: Arc<Mesures>,
        consommation: Consommation,
        reglages: &Reglages,
    ) -> Self {
        let generation = anneau.nouvelle_lecture();
        let opt = anneau.octets_par_trame;
        let ops = anneau.frequence as usize * opt;
        let en_octets =
            |d: Duration| ((ops as f64 * d.as_secs_f64()) as usize / opt * opt).max(opt);
        Self {
            generation,
            mesures,
            consommation,
            octets_par_seconde: ops,
            amorce: en_octets(reglages.amorce),
            amorce_faite: false,
            quantum: en_octets(reglages.quantum),
            reprise: Reprise::new(opt, ops),
            capte: Serie::new(reglages.fenetre.as_secs_f64()),
            consomme: Serie::new(reglages.fenetre.as_secs_f64()),
            debut: Instant::now(),
            dernier_echantillon: None,
            emis: 0,
            silence_a_servir: 0,
            relance_depuis: None,
            muette_depuis: None,
            amorce_servie: None,
            anneau,
        }
    }

    fn ms(&self, octets: f64) -> f64 {
        octets * 1000.0 / self.octets_par_seconde.max(1) as f64
    }

    /// Un point par seconde dans chaque série, et la dérive qui s'en déduit.
    fn mesurer(&mut self, niveau: usize) {
        let maintenant = Instant::now();
        let consomme = self.consommation.octets();
        let mut d = self.mesures.derives();
        d.tampon_ms = self.ms(niveau as f64);
        d.latence_ms = self.ms(niveau as f64 + self.emis.saturating_sub(consomme) as f64);
        if self
            .dernier_echantillon
            .is_none_or(|t| maintenant - t >= Duration::from_secs(1))
        {
            self.dernier_echantillon = Some(maintenant);
            if let Some((instant, trames)) = self.anneau.dernier_point() {
                self.capte.ajouter(instant.as_secs_f64(), trames as f64);
            }
            self.consomme
                .ajouter((maintenant - self.debut).as_secs_f64(), consomme as f64);
            let nominal_trames = self.anneau.frequence as f64;
            let pente_capte = self.capte.pente(60.0);
            let pente_consomme = self.consomme.pente(60.0);
            d.capture_contre_hote_ppm = pente_capte.and_then(|p| ppm(p, nominal_trames));
            d.consommation_contre_hote_ppm =
                pente_consomme.and_then(|p| ppm(p, self.octets_par_seconde as f64));
            let regule = self.reprise.regule();
            d.consommateur_regule = regule;
            d.tampon_de_regime_ms = self.reprise.reference.map(|r| self.ms(r as f64));
            let pente_position = self.mesures.pente_des_positions();
            d.position_contre_hote_ppm = pente_position.and_then(|p| ppm(p, 1000.0));
            // Le débit CONSOMMÉ, dans l'ordre de fiabilité : ce que la zone a
            // réellement JOUÉ ; à défaut, les octets tirés — mais seulement
            // quand la sortie régule, sans quoi leur débit est le nôtre et
            // l'on publierait un zéro trompeur.
            (d.derive_ppm, d.derive_source) = match (pente_capte, pente_position, pente_consomme) {
                (Some(c), Some(p), _) => (
                    derive_contre_la_zone(c, p, self.anneau.frequence),
                    Some("position_de_la_zone"),
                ),
                (Some(c), None, Some(s)) if regule => (
                    ppm(c * self.anneau.octets_par_trame as f64, s),
                    Some("octets_consommes"),
                ),
                _ => (None, None),
            };
            d.fenetre_s = self.consomme.etendue_s();
        }
        *self
            .mesures
            .derives
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = d;
    }

    fn appliquer(&mut self, decision: Decision) {
        let opt = self.anneau.octets_par_trame as u64;
        match decision {
            Decision::Rien => {}
            Decision::Retirer(n) => {
                let retire = self.anneau.retirer(n) as u64;
                self.mesures.reprises.fetch_add(1, Ordering::Relaxed);
                self.mesures
                    .trames_retirees
                    .fetch_add(retire / opt, Ordering::Relaxed);
                warn!(
                    retire_ms = self.ms(retire as f64),
                    "entree_audio_reprise_retrait — l'entrée va plus vite que la sortie"
                );
            }
            Decision::Combler(n) => {
                self.silence_a_servir = n;
                self.mesures.reprises.fetch_add(1, Ordering::Relaxed);
                self.mesures
                    .trames_comblees
                    .fetch_add(n as u64 / opt, Ordering::Relaxed);
                warn!(
                    comble_ms = self.ms(n as f64),
                    "entree_audio_reprise_silence — la sortie va plus vite que l'entrée"
                );
            }
        }
    }

    fn silence(&mut self, buf: &mut [u8], n: usize) -> usize {
        let opt = self.anneau.octets_par_trame;
        let n = n.min(buf.len()) / opt * opt;
        buf[..n].fill(0);
        self.emis += n as u64;
        n
    }
}

impl Read for LecteurCapture {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let opt = self.anneau.octets_par_trame;
        if buf.len() < opt {
            return Ok(0);
        }
        // Relance en cours : la session est tenue en silence au rythme réel
        // jusqu'à ce que la zone passe sur le nouveau flux.
        if let Some(depuis) = self.relance_depuis {
            if depuis.elapsed() >= TENUE_DE_RELANCE {
                return Ok(0);
            }
            std::thread::sleep(Duration::from_millis(20));
            let n = self.quantum;
            return Ok(self.silence(buf, n));
        }
        if self.silence_a_servir > 0 {
            let n = self.silence_a_servir.min(self.quantum);
            let n = self.silence(buf, n);
            self.silence_a_servir -= n;
            return Ok(n);
        }
        loop {
            let (min, max, delai) = if self.amorce_faite {
                (self.quantum / 2, self.quantum, ATTENTE_SOUS_REMPLISSAGE)
            } else {
                (self.amorce, self.quantum, Duration::from_secs(1))
            };
            let niveau = self.anneau.niveau();
            if self.amorce_faite {
                self.mesurer(niveau);
                let t = self
                    .amorce_servie
                    .map(|t| t.elapsed().as_secs_f64())
                    .unwrap_or(0.0);
                let d = self.reprise.observer(niveau, t);
                self.appliquer(d);
                if self.silence_a_servir > 0 {
                    let n = self.silence_a_servir.min(self.quantum);
                    let n = self.silence(buf, n);
                    self.silence_a_servir -= n;
                    return Ok(n);
                }
            }
            match self
                .anneau
                .lire(self.generation, min, max.min(buf.len()), delai)
            {
                Ok(Some(v)) => {
                    if !self.amorce_faite {
                        self.amorce_faite = true;
                        self.amorce_servie = Some(Instant::now());
                        info!(
                            amorce_ms = self.ms(self.amorce as f64),
                            "entree_audio_amorce_servie"
                        );
                    }
                    self.muette_depuis = None;
                    buf[..v.len()].copy_from_slice(&v);
                    self.emis += v.len() as u64;
                    return Ok(v.len());
                }
                Ok(None) => {
                    let depuis = *self.muette_depuis.get_or_insert_with(Instant::now);
                    if self.amorce_faite {
                        self.mesures
                            .sous_remplissages
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    let limite = if self.amorce_faite {
                        ENTREE_MUETTE
                    } else {
                        ENTREE_MUETTE + self.duree_amorce()
                    };
                    if depuis.elapsed() >= limite {
                        if !self.amorce_faite && self.anneau.niveau() > 0 {
                            // Amorce incomplète mais l'entrée vit : on part.
                            self.amorce_faite = true;
                            self.amorce_servie = Some(Instant::now());
                            continue;
                        }
                        return Err(io::Error::other(
                            "l'entrée audio ne rend plus aucun échantillon",
                        ));
                    }
                }
                Err(Fin::Normale) => return Ok(0),
                Err(Fin::Relance) => {
                    self.relance_depuis = Some(Instant::now());
                    let n = self.quantum;
                    return Ok(self.silence(buf, n));
                }
                Err(Fin::Erreur(e)) => return Err(io::Error::other(e)),
            }
        }
    }
}

impl Drop for LecteurCapture {
    fn drop(&mut self) {
        self.anneau.lecteur_parti(self.generation);
    }
}

impl LecteurCapture {
    fn duree_amorce(&self) -> Duration {
        Duration::from_secs_f64(self.amorce as f64 / self.octets_par_seconde.max(1) as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::Mesure;

    fn mesure() -> Mesure {
        Mesure {
            crete: 0.1,
            ..Default::default()
        }
    }

    fn reglages(amorce_ms: u64) -> Reglages {
        Reglages {
            amorce: Duration::from_millis(amorce_ms),
            quantum: Duration::from_millis(20),
            fenetre: Duration::from_secs(600),
        }
    }

    /// 1 kHz, 4 octets par trame : 4 000 octets par seconde.
    fn anneau() -> Arc<Anneau> {
        Arc::new(Anneau::new(1_000, 4, 10_000))
    }

    #[test]
    fn le_lecteur_rend_exactement_ce_qui_a_ete_capte_dans_l_ordre() {
        let a = anneau();
        let mut l = LecteurCapture::new(
            a.clone(),
            Arc::default(),
            Consommation::new(|| 0),
            &reglages(100),
        );
        let signal: Vec<u8> = (0..4_000u32).map(|i| (i % 251) as u8).collect();
        for bloc in signal.chunks(40) {
            a.pousser(bloc, mesure(), None);
        }
        let mut lu = Vec::new();
        let mut buf = vec![0u8; 32 * 1024];
        while lu.len() < signal.len() {
            let n = l.read(&mut buf).unwrap();
            lu.extend_from_slice(&buf[..n]);
        }
        assert_eq!(lu, signal);
    }

    #[test]
    fn l_amorce_retient_le_premier_bloc_tant_qu_elle_n_est_pas_atteinte() {
        let a = anneau();
        let a2 = a.clone();
        let mut l = LecteurCapture::new(
            a.clone(),
            Arc::default(),
            Consommation::new(|| 0),
            &reglages(500),
        );
        let t = std::thread::spawn(move || {
            for _ in 0..10 {
                std::thread::sleep(Duration::from_millis(30));
                a2.pousser(&[1u8; 400], mesure(), None); // 100 ms
            }
        });
        let debut = Instant::now();
        let mut buf = vec![0u8; 32 * 1024];
        let n = l.read(&mut buf).unwrap();
        assert!(n > 0);
        assert!(
            debut.elapsed() >= Duration::from_millis(140),
            "{:?}",
            debut.elapsed()
        );
        assert!(a.niveau() + n >= 2_000, "au moins 500 ms étaient là");
        t.join().unwrap();
    }

    #[test]
    fn une_relance_tient_la_session_en_silence_puis_rend_la_fin() {
        let a = anneau();
        let mut l = LecteurCapture::new(
            a.clone(),
            Arc::default(),
            Consommation::new(|| 0),
            &reglages(0),
        );
        a.pousser(&[9u8; 400], mesure(), None);
        let mut buf = vec![0u8; 1024];
        assert!(l.read(&mut buf).unwrap() > 0);
        a.fermer(Fin::Relance);
        let n = l.read(&mut buf).unwrap();
        assert!(n > 0);
        assert!(buf[..n].iter().all(|&b| b == 0), "du silence, pas une fin");
        l.relance_depuis = Some(Instant::now() - TENUE_DE_RELANCE);
        assert_eq!(l.read(&mut buf).unwrap(), 0);
    }

    #[test]
    fn un_peripherique_qui_faillit_est_une_erreur_et_un_arret_une_fin() {
        let a = anneau();
        let mut l = LecteurCapture::new(
            a.clone(),
            Arc::default(),
            Consommation::new(|| 0),
            &reglages(0),
        );
        a.fermer(Fin::Erreur("débranché".into()));
        let mut buf = vec![0u8; 64];
        assert!(l.read(&mut buf).is_err());

        let a = anneau();
        let mut l = LecteurCapture::new(
            a.clone(),
            Arc::default(),
            Consommation::new(|| 0),
            &reglages(0),
        );
        a.fermer(Fin::Normale);
        assert_eq!(l.read(&mut buf).unwrap(), 0);
    }

    /// La dérive contre la zone : 48 000 trames captées par seconde d'hôte
    /// contre une zone qui joue 1 000,02 ms par seconde → l'entrée est plus
    /// LENTE de 20 ppm.
    #[test]
    fn la_derive_contre_la_zone_compare_capte_et_joue() {
        let d = derive_contre_la_zone(48_000.0, 1_000.02, 48_000).unwrap();
        assert!((d + 20.0).abs() < 0.01, "{d}");
        let d = derive_contre_la_zone(48_000.0 * (1.0 + 7e-6), 1_000.0, 48_000).unwrap();
        assert!((d - 7.0).abs() < 0.01, "{d}");
    }

    #[test]
    fn une_reprise_retire_du_bit_perfect_au_chemin_du_signal() {
        let m = Arc::new(Mesures::default());
        assert!(m.compensation().bit_perfect());
        m.reprises.fetch_add(1, Ordering::Relaxed);
        let c = m.compensation();
        assert!(!c.bit_perfect());
        assert!(!c.reechantillonne);
        assert_eq!(c.methode, "tampon_avec_reprise");
    }
}
