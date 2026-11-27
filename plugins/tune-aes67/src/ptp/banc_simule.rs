//! Deux maîtres successifs sur une liaison à délai constant. La boucle de
//! production pilote elle-même l'horloge en attendant messages et minuteurs.
use super::*;
use crate::ptp::horloge_virtuelle::HorlogeVirtuelle;
use crate::ptp::{
    ConfigPtp, SuiveurPtp,
    suiveur::LiaisonPtp,
    transport::{Canal, Recu},
};
use statime::{Clock, time::Time};
use std::collections::BTreeMap;
use std::sync::mpsc::RecvTimeoutError;

const TRAJET_NS: u64 = 50_000;

/// Le pilote rend le maître actif à chaque émission et inspecte le suiveur
/// toutes les 5 ms. Renvoie le nombre de Delay_Req servis au maître.
pub fn jouer(
    cfg: ConfigPtp,
    maitre: impl Fn(u64) -> Option<ConfigMaitre>,
    observer: impl FnMut(u64, &SuiveurPtp) -> bool,
) -> u64 {
    let demandes = AtomicU64::new(0);
    let temps = Arc::new(AtomicU64::new(0));
    SuiveurPtp::jouer_sur_banc(cfg, temps.clone(), |suiveur| Liaison {
        temps,
        suiveur,
        maitre,
        observer,
        demandes: &demandes,
        file: BTreeMap::new(),
        sync: 0,
        annonce: 0,
        observation: 0,
        seq_sync: 0,
        seq_annonce: 0,
    });
    demandes.into_inner()
}

pub fn temps_du_maitre(cfg: ConfigMaitre, ns: u64) -> u64 {
    (i128::from(ns) + i128::from(cfg.decalage_ns) + (ns as f64 * cfg.derive_ppm * 1e-6) as i128)
        as u64
}

fn port(cfg: ConfigMaitre) -> [u8; 10] {
    let mut port = [0; 10];
    port[..8].copy_from_slice(&cfg.identite);
    port[9] = 1;
    port
}

struct Liaison<'a, M, O> {
    demandes: &'a AtomicU64,
    temps: Arc<AtomicU64>,
    suiveur: Arc<SuiveurPtp>,
    maitre: M,
    observer: O,
    file: BTreeMap<u64, Vec<(Canal, Vec<u8>)>>,
    sync: u64,
    annonce: u64,
    observation: u64,
    seq_sync: u16,
    seq_annonce: u16,
}

impl<M: Fn(u64) -> Option<ConfigMaitre>, O: FnMut(u64, &SuiveurPtp) -> bool> LiaisonPtp
    for Liaison<'_, M, O>
{
    fn graine(&self) -> u64 {
        5438
    }
    fn evenement(&mut self, donnees: &[u8], horloge: &HorlogeVirtuelle) -> io::Result<Time> {
        let t = self.temps.load(Ordering::Relaxed);
        if let Some(cfg) = (self.maitre)(t + TRAJET_NS)
            && donnees.len() >= 44
            && donnees[0] & 0xf == DELAY_REQ
            && donnees[4] == cfg.domaine
        {
            self.demandes.fetch_add(1, Ordering::Relaxed);
            let rep = reponse_delai(cfg, port(cfg), donnees, temps_du_maitre(cfg, t + TRAJET_NS));
            self.file
                .entry(t + 2 * TRAJET_NS)
                .or_default()
                .push((Canal::General, rep));
        }
        Ok(horloge.now())
    }
    fn general(&mut self, _: &[u8]) -> io::Result<()> {
        Ok(())
    }
    fn recevoir(&mut self, attente: Duration) -> Result<Recu, RecvTimeoutError> {
        let fin = self.temps.load(Ordering::Relaxed) + attente.as_nanos() as u64;
        let paquet = self.file.first_key_value().map_or(fin, |(&t, _)| t);
        let t = fin
            .min(paquet)
            .min(self.observation)
            .min(self.sync)
            .min(self.annonce);
        self.temps.store(t, Ordering::Relaxed);
        let maitre = (self.maitre)(t);
        if self.annonce == t {
            if let Some(cfg) = maitre {
                let m = annonce(cfg, port(cfg), self.seq_annonce, temps_du_maitre(cfg, t));
                self.file
                    .entry(t + TRAJET_NS)
                    .or_default()
                    .push((Canal::General, m));
                self.seq_annonce = self.seq_annonce.wrapping_add(1);
            }
            self.annonce += duree_log2(maitre.unwrap_or_else(ConfigMaitre::par_defaut).log_annonce)
                .as_nanos() as u64;
        }
        if self.sync == t {
            if let Some(cfg) = maitre {
                let m = sync(cfg, port(cfg), self.seq_sync);
                self.file
                    .entry(t + TRAJET_NS)
                    .or_default()
                    .push((Canal::Evenement, m));
                let m = suivi(cfg, port(cfg), self.seq_sync, temps_du_maitre(cfg, t));
                self.file
                    .entry(t + TRAJET_NS + 1)
                    .or_default()
                    .push((Canal::General, m));
                self.seq_sync = self.seq_sync.wrapping_add(1);
            }
            self.sync += duree_log2(maitre.unwrap_or_else(ConfigMaitre::par_defaut).log_sync)
                .as_nanos() as u64;
        }
        if self.observation == t {
            if !(self.observer)(t, &self.suiveur) {
                return Err(RecvTimeoutError::Disconnected);
            }
            self.observation += 5_000_000;
        }
        if let Some(mut entree) = self.file.first_entry()
            && *entree.key() == t
        {
            let (canal, donnees) = entree.get_mut().remove(0);
            if entree.get().is_empty() {
                entree.remove();
            }
            return Ok(Recu {
                canal,
                donnees,
                horodatage: self.suiveur.horloge().now(),
            });
        }
        Err(RecvTimeoutError::Timeout)
    }
}
