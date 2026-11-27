use crate::sync::mpsc::Attente;
use crate::systeme::{
    collections::{BTreeMap, VecDeque},
    sync::{Mutex, mpsc},
};
use crate::{
    ConfigPtp, SuiveurPtp, time,
    transport::{self, Canal, Recu},
};
use statime::Clock;
use tune_aes67::ptp::banc::ConfigMaitre;

// Deux trajets symétriques de 50 µs. Le maître émet en deux temps (IEEE 1588-2008).
const TRAJET: u64 = 50_000;
const PAS_OBSERVATION: u64 = 5_000_000;
static BANC: Mutex<()> = Mutex::new(());

pub fn temps_du_maitre(cfg: ConfigMaitre, t: u64) -> u64 {
    (i128::from(t) + i128::from(cfg.decalage_ns) + (t as f64 * cfg.derive_ppm * 1e-6) as i128)
        as u64
}

fn horodater(trame: &mut [u8], ns: u64) {
    trame[34..40].copy_from_slice(&(ns / 1_000_000_000).to_be_bytes()[2..]);
    trame[40..44].copy_from_slice(&((ns % 1_000_000_000) as u32).to_be_bytes());
}

struct Maitre {
    cfg: ConfigMaitre,
    vol: BTreeMap<u64, VecDeque<(Canal, Vec<u8>)>>,
    demandes: u64,
}

impl Maitre {
    fn trame(
        &self,
        kind: u8,
        sequence: u16,
        taille: usize,
        controle: u8,
        intervalle: i8,
    ) -> Vec<u8> {
        let mut m = vec![0; taille];
        m[0] = kind;
        m[1] = 2;
        m[2..4].copy_from_slice(&(taille as u16).to_be_bytes());
        m[4] = self.cfg.domaine;
        m[7] = 0x0c;
        m[20..28].copy_from_slice(&self.cfg.identite);
        m[29] = 1;
        m[30..32].copy_from_slice(&sequence.to_be_bytes());
        m[32] = controle;
        m[33] = intervalle as u8;
        m
    }

    fn livrer_a(&mut self, t: u64, canal: Canal, trame: Vec<u8>) {
        self.vol.entry(t).or_default().push_back((canal, trame));
    }

    fn annoncer(&mut self, t: u64, sequence: u16) {
        let mut m = self.trame(11, sequence, 64, 5, self.cfg.log_annonce);
        horodater(&mut m, temps_du_maitre(self.cfg, t));
        m[44..46].copy_from_slice(&37i16.to_be_bytes());
        m[47] = self.cfg.priorite1;
        m[48..53].copy_from_slice(&[248, 0xfe, 0xff, 0xff, 128]);
        m[53..61].copy_from_slice(&self.cfg.identite);
        m[63] = 0xa0;
        self.livrer_a(t + TRAJET, Canal::General, m);
    }

    fn synchroniser(&mut self, t: u64, sequence: u16) {
        let mut sync = self.trame(0, sequence, 44, 0, self.cfg.log_sync);
        sync[6] = 2;
        self.livrer_a(t + TRAJET, Canal::Evenement, sync);
        let mut suivi = self.trame(8, sequence, 44, 2, self.cfg.log_sync);
        horodater(&mut suivi, temps_du_maitre(self.cfg, t));
        self.livrer_a(t + TRAJET + 1, Canal::General, suivi);
    }

    fn repondre(&mut self, t: u64, req: &[u8]) {
        assert_eq!(req[0] & 0xf, 1, "Delay_Req attendu");
        assert_eq!(req[4], self.cfg.domaine);
        let sequence = u16::from_be_bytes([req[30], req[31]]);
        let mut reponse = self.trame(9, sequence, 54, 3, 0);
        horodater(&mut reponse, temps_du_maitre(self.cfg, t + TRAJET));
        reponse[44..54].copy_from_slice(&req[20..30]);
        self.livrer_a(t + 2 * TRAJET, Canal::General, reponse);
        self.demandes += 1;
    }

    fn conduire(
        mut self,
        s: &SuiveurPtp,
        attentes: mpsc::Receiver<Attente<Recu>>,
        sorties: mpsc::Receiver<Vec<u8>>,
        mut observer: impl FnMut(u64, &SuiveurPtp) -> bool,
    ) -> u64 {
        let (mut t, mut annonce, mut sync, mut mesure) = (0u64, 0, 0, 0);
        let periode = |log| (1e9 * 2f64.powi(i32::from(log))) as u64;
        let pas_annonce = periode(self.cfg.log_annonce);
        let pas_sync = periode(self.cfg.log_sync);
        loop {
            let (delai, rendre) = attentes
                .recv()
                .expect("le fil PTP attend sa prochaine échéance");
            for req in sorties.try_iter() {
                self.repondre(t, &req);
            }
            t = (t + delai.as_nanos() as u64)
                .min(annonce)
                .min(sync)
                .min(mesure);
            if let Some((&arrivee, _)) = self.vol.first_key_value() {
                t = t.min(arrivee);
            }
            time::avancer_a(t);
            if t == annonce {
                self.annoncer(t, (t / pas_annonce) as u16);
                annonce += pas_annonce;
            }
            if t == sync {
                self.synchroniser(t, (t / pas_sync) as u16);
                sync += pas_sync;
            }
            if t == mesure {
                if !observer(t, s) {
                    return self.demandes;
                }
                mesure += PAS_OBSERVATION;
            }
            let mut issue = Err(mpsc::RecvTimeoutError::Timeout);
            if let Some(mut e) = self.vol.first_entry()
                && *e.key() == t
            {
                let (canal, donnees) = e.get_mut().pop_front().unwrap();
                if e.get().is_empty() {
                    e.remove();
                }
                issue = Ok(Recu {
                    canal,
                    donnees,
                    horodatage: s.horloge().now(),
                });
            }
            rendre.send(issue).expect("réception par le suiveur");
        }
    }
}

pub fn jouer(
    cfg: ConfigPtp,
    maitre: ConfigMaitre,
    observer: impl FnMut(u64, &SuiveurPtp) -> bool,
) -> u64 {
    let _garde = BANC.lock().unwrap_or_else(|p| p.into_inner());
    time::avancer_a(0);
    let (vers_maitre, attentes) = mpsc::channel();
    let (emission, sorties) = mpsc::channel();
    transport::RECEPTION.with(|r| r.replace(Some(vers_maitre)));
    transport::EMISSION.with(|e| e.replace(Some(emission)));
    let s = SuiveurPtp::demarrer(cfg);
    // Les deux récepteurs appartiennent à conduire : même si une assertion
    // panique, leur destruction libère le fil avant que le suiveur le joigne.
    Maitre {
        cfg: maitre,
        vol: BTreeMap::new(),
        demandes: 0,
    }
    .conduire(&s, attentes, sorties, observer)
}
