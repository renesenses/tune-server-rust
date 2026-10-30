//! Le fil qui émet un paquet RTP par durée de paquet, à heure fixe.
//!
//! Le cadenceur ne connaît le temps que par la [`SourceHorloge`] : l'instant
//! d'envoi du paquet `k` est `origine + k × durée`, l'horodatage RTP est
//! `horodatage(origine + avance) + k × trames`. Brancher une horloge PTP à
//! l'étape E2 ne change donc rien ici.
//!
//! Ce que le fil fait quand le PCM manque (décodeur en retard, pause) : il
//! n'émet RIEN — le récepteur se tait — et repart sur une nouvelle origine,
//! bit M posé, dès que le PCM revient. Un retard de plus de
//! [`RETARD_DE_RESYNCHRONISATION`] (fil suspendu, machine chargée) fait de
//! même plutôt que de rattraper en rafale, ce qu'un récepteur au tampon de
//! quelques millisecondes ne supporterait pas.

use std::collections::VecDeque;
use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::Serialize;
use tracing::{debug, info, warn};

use crate::horloge::{SourceHorloge, horodatage_rtp, instant_de_la_trame};
use crate::rtp::EnteteRtp;

/// Au-delà de ce retard sur l'échéance, on repart d'une nouvelle origine.
pub const RETARD_DE_RESYNCHRONISATION: Duration = Duration::from_millis(20);
/// Attente maximale d'un paquet avant de compter une sous-alimentation.
const ATTENTE_DU_PCM: Duration = Duration::from_millis(2);

/// Ce que le producteur passe au cadenceur.
pub enum Message {
    /// Une charge L24 d'un paquet.
    Charge(Vec<u8>),
    /// Fin du flux : plus rien ne viendra.
    Fin,
}

/// Les ordres que la sortie donne au fil, sans verrou.
#[derive(Default)]
pub struct Commandes {
    pub arret: AtomicBool,
    pub pause: AtomicBool,
    /// Posé par le fil quand il a émis le dernier paquet du flux.
    pub fini: AtomicBool,
}

/// Ce que le fil a fait, pour l'état de la sortie et le diagnostic.
#[derive(Default)]
pub struct Statistiques {
    pub paquets: AtomicU64,
    pub trames: AtomicU64,
    pub retard_max_ns: AtomicU64,
    pub sous_alimentations: AtomicU64,
    pub resynchronisations: AtomicU64,
    pub erreurs_d_envoi: AtomicU64,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ReleveStatistiques {
    pub paquets: u64,
    pub trames: u64,
    pub retard_max_us: u64,
    pub sous_alimentations: u64,
    pub resynchronisations: u64,
    pub erreurs_d_envoi: u64,
}

impl Statistiques {
    pub fn releve(&self) -> ReleveStatistiques {
        ReleveStatistiques {
            paquets: self.paquets.load(Ordering::Relaxed),
            trames: self.trames.load(Ordering::Relaxed),
            retard_max_us: self.retard_max_ns.load(Ordering::Relaxed) / 1000,
            sous_alimentations: self.sous_alimentations.load(Ordering::Relaxed),
            resynchronisations: self.resynchronisations.load(Ordering::Relaxed),
            erreurs_d_envoi: self.erreurs_d_envoi.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ParametresCadence {
    pub cadence: u32,
    pub trames_par_paquet: u32,
    pub type_charge: u8,
    pub ssrc: u32,
    /// Avance de l'horodatage sur l'instant d'envoi. Zéro à l'étape E1 ;
    /// réglable pour l'étape E2, où il faudra la mesurer face à un récepteur.
    pub avance: Duration,
    /// Paquets à tenir prêts avant de poser une origine (départ, reprise).
    pub pretampon_paquets: usize,
    /// Demander l'ordonnancement temps réel pour le fil (SCHED_FIFO sous
    /// Linux, repli sur `nice` si refusé).
    pub temps_reel: bool,
}

/// Lance le fil. `socket` est connecté à la destination.
pub fn lancer(
    socket: UdpSocket,
    rx: Receiver<Message>,
    horloge: Arc<dyn SourceHorloge>,
    p: ParametresCadence,
    commandes: Arc<Commandes>,
    stats: Arc<Statistiques>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("aes67-rtp".into())
        .spawn(move || {
            if p.temps_reel {
                let issue = tune_core::audio::ordonnancement_rt::demander_pour_le_fil_courant();
                debug!(?issue, "aes67_fil_rtp_ordonnancement");
            }
            boucle(&socket, &rx, horloge.as_ref(), p, &commandes, &stats);
        })
}

fn attendre_jusqu_a(horloge: &dyn SourceHorloge, echeance_ns: u64, commandes: &Commandes) {
    loop {
        let maintenant = horloge.maintenant_ns();
        if maintenant >= echeance_ns || commandes.arret.load(Ordering::Acquire) {
            return;
        }
        let reste = Duration::from_nanos(echeance_ns - maintenant);
        std::thread::sleep(reste.min(Duration::from_millis(10)));
    }
}

fn boucle(
    socket: &UdpSocket,
    rx: &Receiver<Message>,
    horloge: &dyn SourceHorloge,
    p: ParametresCadence,
    commandes: &Commandes,
    stats: &Statistiques,
) {
    let mut sequence: u16 = (p.ssrc >> 16) as u16;
    // (origine en ns, horodatage RTP de l'origine), et le rang du paquet.
    let mut origine: Option<(u64, u32)> = None;
    let mut rang: u64 = 0;
    // Les charges déjà tirées du canal. Au départ (et à chaque reprise), on
    // attend d'en avoir `pretampon_paquets` — ou la fin du flux — avant de
    // poser l'origine : un décodeur qui démarre lentement (rééchantillonnage
    // des premiers blocs) ne doit pas affamer le flux dès la deuxième
    // milliseconde.
    let mut tampon: VecDeque<Vec<u8>> = VecDeque::new();
    let mut fin_vue = false;
    let pretampon = p.pretampon_paquets.max(1);
    let mut premiere_origine = true;
    let avance_ns = u64::try_from(p.avance.as_nanos()).unwrap_or(0);
    let resync_ns = u64::try_from(RETARD_DE_RESYNCHRONISATION.as_nanos()).unwrap_or(u64::MAX);

    loop {
        if commandes.arret.load(Ordering::Acquire) {
            break;
        }
        if commandes.pause.load(Ordering::Acquire) {
            origine = None;
            std::thread::sleep(ATTENTE_DU_PCM);
            continue;
        }
        // Tirer, sans attendre, tout ce qui est prêt.
        while !fin_vue {
            match rx.try_recv() {
                Ok(Message::Charge(c)) => tampon.push_back(c),
                Ok(Message::Fin) | Err(TryRecvError::Disconnected) => fin_vue = true,
                Err(TryRecvError::Empty) => break,
            }
        }
        let a_remplir = if origine.is_none() {
            tampon.len() < pretampon
        } else {
            tampon.is_empty()
        };
        if a_remplir && !fin_vue {
            if tampon.is_empty() && origine.take().is_some() {
                stats.sous_alimentations.fetch_add(1, Ordering::Relaxed);
                debug!("aes67_sous_alimentation");
            }
            match rx.recv_timeout(ATTENTE_DU_PCM) {
                Ok(Message::Charge(c)) => tampon.push_back(c),
                Ok(Message::Fin) | Err(RecvTimeoutError::Disconnected) => fin_vue = true,
                Err(RecvTimeoutError::Timeout) => {}
            }
            continue;
        }
        let Some(charge) = tampon.pop_front() else {
            // Fin du flux et tampon vide : tout est parti.
            commandes.fini.store(true, Ordering::Release);
            break;
        };

        let maintenant = horloge.maintenant_ns();
        let marqueur = origine.is_none();
        let (origine_ns, origine_ts) = *origine.get_or_insert_with(|| {
            rang = 0;
            if premiere_origine {
                premiere_origine = false;
            } else {
                stats.resynchronisations.fetch_add(1, Ordering::Relaxed);
            }
            (
                maintenant,
                horodatage_rtp(maintenant.saturating_add(avance_ns), p.cadence),
            )
        });
        let trames_avant = rang * u64::from(p.trames_par_paquet);
        let echeance = instant_de_la_trame(origine_ns, trames_avant, p.cadence);
        if maintenant > echeance.saturating_add(resync_ns) {
            warn!(
                retard_us = (maintenant - echeance) / 1000,
                "aes67_retard_resynchronisation"
            );
            origine = None;
            tampon.push_front(charge);
            continue;
        }
        attendre_jusqu_a(horloge, echeance, commandes);
        if commandes.arret.load(Ordering::Acquire) {
            break;
        }
        let entete = EnteteRtp {
            marqueur,
            type_charge: p.type_charge,
            sequence,
            horodatage: origine_ts.wrapping_add(trames_avant as u32),
            ssrc: p.ssrc,
        };
        let retard = horloge.maintenant_ns().saturating_sub(echeance);
        stats.retard_max_ns.fetch_max(retard, Ordering::Relaxed);
        if let Err(e) = socket.send(&entete.paquet(&charge)) {
            // Un envoi refusé (réseau absent) ne doit pas arrêter la lecture :
            // le compteur le dit, le journal une fois sur mille.
            let n = stats.erreurs_d_envoi.fetch_add(1, Ordering::Relaxed);
            if n % 1000 == 0 {
                warn!(error = %e, erreurs = n + 1, "aes67_envoi_refuse");
            }
        }
        sequence = sequence.wrapping_add(1);
        rang += 1;
        stats.paquets.fetch_add(1, Ordering::Relaxed);
        stats
            .trames
            .fetch_add(u64::from(p.trames_par_paquet), Ordering::Relaxed);
    }
    info!(stats = ?stats.releve(), "aes67_fil_rtp_termine");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::horloge::HorlogeSysteme;
    use std::sync::mpsc::sync_channel;

    fn parametres() -> ParametresCadence {
        ParametresCadence {
            cadence: 48_000,
            trames_par_paquet: 48,
            type_charge: 98,
            ssrc: 0x1234_5678,
            avance: Duration::ZERO,
            pretampon_paquets: 1,
            temps_reel: false,
        }
    }

    #[test]
    fn cent_paquets_partent_en_cent_millisecondes_horodates_de_48_en_48() {
        let ecoute = UdpSocket::bind("127.0.0.1:0").unwrap();
        ecoute
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.connect(ecoute.local_addr().unwrap()).unwrap();
        let (tx, rx) = sync_channel(200);
        for i in 0..100u8 {
            tx.send(Message::Charge(vec![i; 288])).unwrap();
        }
        tx.send(Message::Fin).unwrap();
        let commandes = Arc::new(Commandes::default());
        let stats = Arc::new(Statistiques::default());
        let debut = std::time::Instant::now();
        let fil = lancer(
            s,
            rx,
            Arc::new(HorlogeSysteme::new()),
            parametres(),
            commandes.clone(),
            stats.clone(),
        )
        .unwrap();
        let mut recus = Vec::new();
        let mut t = [0u8; 2048];
        while recus.len() < 100 {
            let n = ecoute.recv(&mut t).unwrap();
            recus.push(t[..n].to_vec());
        }
        fil.join().unwrap();
        let duree = debut.elapsed();
        assert!(commandes.fini.load(Ordering::Acquire));
        // 100 paquets de 1 ms : au moins 99 ms (le premier part à t = 0).
        assert!(duree >= Duration::from_millis(99), "{duree:?}");
        let entetes: Vec<EnteteRtp> = recus
            .iter()
            .map(|p| EnteteRtp::lire(p).unwrap().0)
            .collect();
        assert!(entetes[0].marqueur);
        assert!(entetes[1..].iter().all(|e| !e.marqueur));
        for (i, paire) in entetes.windows(2).enumerate() {
            assert_eq!(paire[1].sequence, paire[0].sequence.wrapping_add(1), "{i}");
            assert_eq!(
                paire[1].horodatage,
                paire[0].horodatage.wrapping_add(48),
                "{i}"
            );
            assert_eq!(paire[1].ssrc, 0x1234_5678);
        }
        for (i, p) in recus.iter().enumerate() {
            assert_eq!(&p[12..], &[i as u8; 288][..]);
        }
        let r = stats.releve();
        assert_eq!(r.paquets, 100);
        assert_eq!(r.trames, 4800);
        assert_eq!(r.sous_alimentations, 0);
        assert_eq!(r.resynchronisations, 0);
    }

    #[test]
    fn un_trou_de_pcm_tait_le_flux_puis_repart_marque() {
        let ecoute = UdpSocket::bind("127.0.0.1:0").unwrap();
        ecoute
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.connect(ecoute.local_addr().unwrap()).unwrap();
        let (tx, rx) = sync_channel(200);
        let commandes = Arc::new(Commandes::default());
        let stats = Arc::new(Statistiques::default());
        let fil = lancer(
            s,
            rx,
            Arc::new(HorlogeSysteme::new()),
            parametres(),
            commandes.clone(),
            stats.clone(),
        )
        .unwrap();
        for _ in 0..5 {
            tx.send(Message::Charge(vec![0; 288])).unwrap();
        }
        std::thread::sleep(Duration::from_millis(50));
        for _ in 0..5 {
            tx.send(Message::Charge(vec![0; 288])).unwrap();
        }
        tx.send(Message::Fin).unwrap();
        fil.join().unwrap();
        let mut t = [0u8; 2048];
        let mut entetes = Vec::new();
        for _ in 0..10 {
            let n = ecoute.recv(&mut t).unwrap();
            entetes.push(EnteteRtp::lire(&t[..n]).unwrap().0);
        }
        assert!(entetes[0].marqueur && entetes[5].marqueur);
        assert_eq!(entetes[5].sequence, entetes[4].sequence.wrapping_add(1));
        // ~50 ms de silence : l'horodatage a sauté d'environ 2400 trames.
        let saut = entetes[5].horodatage.wrapping_sub(entetes[4].horodatage);
        assert!(saut > 48 * 20, "saut {saut}");
        let r = stats.releve();
        assert_eq!(r.sous_alimentations, 1);
        assert_eq!(r.resynchronisations, 1);
    }

    #[test]
    fn l_arret_interrompt_le_fil_sans_attendre_la_fin() {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.connect("127.0.0.1:9").unwrap();
        let (_tx, rx) = sync_channel(1);
        let commandes = Arc::new(Commandes::default());
        let fil = lancer(
            s,
            rx,
            Arc::new(HorlogeSysteme::new()),
            parametres(),
            commandes.clone(),
            Arc::new(Statistiques::default()),
        )
        .unwrap();
        commandes.arret.store(true, Ordering::Release);
        fil.join().unwrap();
        assert!(!commandes.fini.load(Ordering::Acquire));
    }
}
