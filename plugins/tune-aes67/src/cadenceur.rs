//! Le fil qui émet un paquet RTP par durée de paquet, à heure fixe.
//!
//! Le cadenceur ne connaît le temps que par la [`SourceHorloge`] : l'instant
//! d'envoi du paquet `k` est `origine + k × durée`, l'horodatage RTP est
//! `horodatage(origine + avance) + décalage + k × trames`. Avec le suiveur PTP
//! (étape E2), l'instant d'envoi ET l'horodatage sont en temps PTP : le flux
//! suit la cadence du maître, pas celle du quartz local.
//!
//! Quand la [`SourceHorloge::generation`] change (calage sur un maître,
//! saut d'horloge, changement de maître, repli sur l'horloge locale), le fil
//! repart d'une nouvelle origine, bit M posé : un horodatage ne mélange
//! jamais deux échelles de temps.
//!
//! Ce que le fil fait quand le PCM manque (décodeur en retard, pause) : il
//! n'émet RIEN — le récepteur se tait — et repart sur une nouvelle origine,
//! bit M posé, dès que le PCM revient. Un retard de plus de
//! [`RETARD_DE_RESYNCHRONISATION`] (fil suspendu, machine chargée) fait de
//! même plutôt que de rattraper en rafale, ce qu'un récepteur au tampon de
//! quelques millisecondes ne supporterait pas.
//!
//! Étape E3 : le fil vit autant que la sortie, pas autant qu'une piste. Un
//! saut, une nouvelle lecture ou un arrêt POSENT un nouveau flux
//! ([`Commandes::installer`]) : le tampon est vidé, la séquence RTP continue
//! (aucun recul vu du récepteur), et la nouvelle origine ne part jamais
//! avant la fin du dernier paquet émis sur la même échelle de temps
//! ([`nouvelle_origine`]) : deux paquets ne couvrent jamais le même instant
//! média. Un trou de PCM ne compte comme sous-alimentation qu'une fois
//! l'échéance du paquet suivant passée : un décodeur un peu en retard mais
//! à l'heure n'ouvre pas de nouvelle origine.

use std::collections::VecDeque;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::Serialize;
use tracing::{debug, info, warn};

use crate::horloge::{SourceHorloge, horodatage_rtp_decale, instant_de_la_trame};
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

/// Un flux à émettre : son identifiant et son canal. `rx` absent : plus
/// rien à émettre (arrêt), le fil attend le flux suivant.
pub struct NouveauFlux {
    pub id: u64,
    pub rx: Option<Receiver<Message>>,
}

/// Les ordres que la sortie donne au fil.
#[derive(Default)]
pub struct Commandes {
    pub arret: AtomicBool,
    pub pause: AtomicBool,
    /// Posé par le fil quand il a émis le dernier paquet du flux COURANT
    /// ([`Statistiques::flux_courant`]) ; remis à faux à la pose d'un flux.
    pub fini: AtomicBool,
    a_poser: Mutex<Option<NouveauFlux>>,
}

impl Commandes {
    /// Remplace le flux émis : le fil le prend à son prochain tour, jette
    /// ce qu'il tenait de l'ancien et repart d'une origine marquée.
    pub fn installer(&self, id: u64, rx: Option<Receiver<Message>>) {
        *self.a_poser.lock().unwrap_or_else(|p| p.into_inner()) = Some(NouveauFlux { id, rx });
    }

    fn prendre(&self) -> Option<NouveauFlux> {
        self.a_poser
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }
}

/// Ce que le fil a fait, pour l'état de la sortie et le diagnostic.
#[derive(Default)]
pub struct Statistiques {
    pub paquets: AtomicU64,
    pub trames: AtomicU64,
    pub retard_max_ns: AtomicU64,
    pub sous_alimentations: AtomicU64,
    pub resynchronisations: AtomicU64,
    /// Pauses effectivement prises par le fil, distinctes des ordres reçus.
    pub pauses_observees: AtomicU64,
    pub erreurs_d_envoi: AtomicU64,
    /// Le flux en cours d'émission, et `trames` au moment où il a été posé.
    pub flux_courant: AtomicU64,
    pub trames_au_debut_du_flux: AtomicU64,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ReleveStatistiques {
    pub paquets: u64,
    pub trames: u64,
    pub retard_max_us: u64,
    pub sous_alimentations: u64,
    pub resynchronisations: u64,
    pub pauses_observees: u64,
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
            pauses_observees: self.pauses_observees.load(Ordering::Acquire),
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
    /// `a=mediaclk:direct=<décalage>` : ajouté à chaque horodatage.
    pub decalage_media: u32,
    /// Paquets à tenir prêts avant de poser une origine (départ, reprise).
    pub pretampon_paquets: usize,
    /// Demander l'ordonnancement temps réel pour le fil (SCHED_FIFO sous
    /// Linux, repli sur `nice` si refusé).
    pub temps_reel: bool,
}

/// La fin du dernier paquet émis : instant, horodatage de la trame qui le
/// suit, génération de l'horloge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FinDuDernierPaquet {
    pub fin_ns: u64,
    pub horodatage_suivant: u32,
    pub generation: u64,
}

/// L'origine d'une nouvelle suite de paquets (départ, reprise, saut,
/// sous-alimentation) : `(instant du premier paquet, son horodatage)`.
///
/// Sur la même échelle de temps que le dernier paquet émis, l'origine ne
/// part jamais avant la fin de ce paquet, et son horodatage jamais avant la
/// trame qui le suit : un récepteur ne voit ni recul ni chevauchement. Sur
/// une autre échelle (nouvelle génération de l'horloge), seule la nouvelle
/// échelle compte.
pub fn nouvelle_origine(
    maintenant_ns: u64,
    generation: u64,
    dernier: Option<FinDuDernierPaquet>,
    avance_ns: u64,
    cadence: u32,
    decalage_media: u32,
) -> (u64, u32) {
    let meme_echelle = dernier.filter(|d| d.generation == generation);
    let origine_ns = match meme_echelle {
        Some(d) => maintenant_ns.max(d.fin_ns),
        None => maintenant_ns,
    };
    let mut horodatage = horodatage_rtp_decale(
        origine_ns.saturating_add(avance_ns),
        cadence,
        decalage_media,
    );
    if let Some(d) = meme_echelle
        && (horodatage.wrapping_sub(d.horodatage_suivant) as i32) < 0
    {
        horodatage = d.horodatage_suivant;
    }
    (origine_ns, horodatage)
}

/// Lance le fil. `socket` est connecté à la destination ; `rx` est le
/// premier flux (identifiant 0), ou rien.
pub fn lancer(
    socket: UdpSocket,
    rx: Option<Receiver<Message>>,
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
            boucle(&socket, rx, horloge.as_ref(), p, &commandes, &stats);
        })
}

fn attendre_jusqu_a(
    horloge: &dyn SourceHorloge,
    echeance_ns: u64,
    generation: u64,
    commandes: &Commandes,
) {
    loop {
        let maintenant = horloge.maintenant_ns();
        if maintenant >= echeance_ns
            || commandes.arret.load(Ordering::Acquire)
            || horloge.generation() != generation
        {
            return;
        }
        let reste = Duration::from_nanos(echeance_ns - maintenant);
        std::thread::sleep(reste.min(Duration::from_millis(10)));
    }
}

fn boucle(
    socket: &UdpSocket,
    rx_initial: Option<Receiver<Message>>,
    horloge: &dyn SourceHorloge,
    p: ParametresCadence,
    commandes: &Commandes,
    stats: &Statistiques,
) {
    let mut rx = rx_initial;
    let mut sequence: u16 = (p.ssrc >> 16) as u16;
    // (origine en ns, horodatage RTP de l'origine, génération de l'horloge),
    // et le rang du paquet.
    let mut origine: Option<(u64, u32, u64)> = None;
    let mut rang: u64 = 0;
    let mut dernier: Option<FinDuDernierPaquet> = None;
    // Les charges déjà tirées du canal. Au départ (et à chaque reprise), on
    // attend d'en avoir `pretampon_paquets` — ou la fin du flux — avant de
    // poser l'origine : un décodeur qui démarre lentement (rééchantillonnage
    // des premiers blocs) ne doit pas affamer le flux dès la deuxième
    // milliseconde.
    let mut tampon: VecDeque<Vec<u8>> = VecDeque::new();
    let mut fin_vue = rx.is_none();
    let pretampon = p.pretampon_paquets.max(1);
    let mut premiere_origine = true;
    let mut etait_en_pause = false;
    let avance_ns = u64::try_from(p.avance.as_nanos()).unwrap_or(0);
    let resync_ns = u64::try_from(RETARD_DE_RESYNCHRONISATION.as_nanos()).unwrap_or(u64::MAX);
    let tpp = u64::from(p.trames_par_paquet);

    loop {
        if commandes.arret.load(Ordering::Acquire) {
            break;
        }
        if let Some(n) = commandes.prendre() {
            // Saut, nouvelle lecture ou arrêt : ce qui restait de l'ancien
            // flux ne part pas ; la séquence continue, l'origine est neuve.
            tampon.clear();
            rx = n.rx;
            fin_vue = rx.is_none();
            origine = None;
            commandes.fini.store(false, Ordering::Release);
            stats
                .trames_au_debut_du_flux
                .store(stats.trames.load(Ordering::Relaxed), Ordering::Relaxed);
            stats.flux_courant.store(n.id, Ordering::Release);
            debug!(flux = n.id, "aes67_flux_pose");
        }
        if commandes.pause.load(Ordering::Acquire) {
            origine = None;
            if !etait_en_pause {
                stats.pauses_observees.fetch_add(1, Ordering::Release);
                etait_en_pause = true;
            }
            std::thread::sleep(ATTENTE_DU_PCM);
            continue;
        }
        etait_en_pause = false;
        let Some(canal) = rx.as_ref() else {
            std::thread::sleep(ATTENTE_DU_PCM);
            continue;
        };
        // Tirer, sans attendre, ce qui est prêt, jusqu'au prétampon
        // seulement : le canal borné doit rester la contre-pression du
        // producteur. Tirer TOUT faisait du tampon une file sans fond où le
        // producteur déversait chaque piste à la vitesse du décodeur ; une
        // suivante préparée pendant la lecture (`set_next_media`) arrivait
        // alors après la fin du décodage et n'était jamais enchaînée.
        while !fin_vue && tampon.len() < pretampon {
            match canal.try_recv() {
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
            // Sous-alimentation seulement si l'échéance du paquet suivant
            // est passée : avant, le PCM peut encore arriver à l'heure.
            if tampon.is_empty()
                && let Some((origine_ns, _, _)) = origine
                && horloge.maintenant_ns() > instant_de_la_trame(origine_ns, rang * tpp, p.cadence)
            {
                origine = None;
                stats.sous_alimentations.fetch_add(1, Ordering::Relaxed);
                debug!("aes67_sous_alimentation");
            }
            match canal.recv_timeout(ATTENTE_DU_PCM) {
                Ok(Message::Charge(c)) => tampon.push_back(c),
                Ok(Message::Fin) | Err(RecvTimeoutError::Disconnected) => fin_vue = true,
                Err(RecvTimeoutError::Timeout) => {}
            }
            continue;
        }
        let Some(charge) = tampon.pop_front() else {
            // Fin du flux et tampon vide : tout est parti. Le fil reste là
            // pour le flux suivant.
            if !commandes.fini.swap(true, Ordering::AcqRel) {
                info!(stats = ?stats.releve(), "aes67_flux_termine");
            }
            rx = None;
            continue;
        };

        if let Some((_, _, g)) = origine
            && g != horloge.generation()
        {
            info!(
                avant = g,
                apres = horloge.generation(),
                "aes67_horloge_changee_nouvelle_origine"
            );
            origine = None;
        }
        let generation = horloge.generation();
        let maintenant = horloge.maintenant_ns();
        let marqueur = origine.is_none();
        let (origine_ns, origine_ts, _) = *origine.get_or_insert_with(|| {
            rang = 0;
            if premiere_origine {
                premiere_origine = false;
            } else {
                stats.resynchronisations.fetch_add(1, Ordering::Relaxed);
            }
            let (o_ns, o_ts) = nouvelle_origine(
                maintenant,
                generation,
                dernier,
                avance_ns,
                p.cadence,
                p.decalage_media,
            );
            (o_ns, o_ts, generation)
        });
        let trames_avant = rang * tpp;
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
        attendre_jusqu_a(horloge, echeance, generation, commandes);
        if commandes.arret.load(Ordering::Acquire) {
            break;
        }
        if horloge.generation() != generation {
            // L'échelle a changé pendant l'attente : ce paquet repartira sur
            // la nouvelle origine.
            tampon.push_front(charge);
            continue;
        }
        let horodatage = origine_ts.wrapping_add(trames_avant as u32);
        let entete = EnteteRtp {
            marqueur,
            type_charge: p.type_charge,
            sequence,
            horodatage,
            ssrc: p.ssrc,
        };
        let retard = horloge.maintenant_ns().saturating_sub(echeance);
        stats.retard_max_ns.fetch_max(retard, Ordering::Relaxed);
        if let Err(e) = socket.send(&entete.paquet(&charge)) {
            // Un envoi refusé (réseau absent) ne doit pas arrêter la lecture :
            // le compteur le dit, le journal une fois sur mille.
            let n = stats.erreurs_d_envoi.fetch_add(1, Ordering::Relaxed);
            if n.is_multiple_of(1000) {
                warn!(error = %e, erreurs = n + 1, "aes67_envoi_refuse");
            }
        }
        dernier = Some(FinDuDernierPaquet {
            fin_ns: instant_de_la_trame(origine_ns, trames_avant + tpp, p.cadence),
            horodatage_suivant: horodatage.wrapping_add(p.trames_par_paquet),
            generation,
        });
        sequence = sequence.wrapping_add(1);
        rang += 1;
        stats.paquets.fetch_add(1, Ordering::Relaxed);
        stats.trames.fetch_add(tpp, Ordering::Relaxed);
    }
    info!(stats = ?stats.releve(), "aes67_fil_rtp_termine");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::horloge::{HorlogeSysteme, horodatage_rtp};
    use std::sync::mpsc::sync_channel;

    /// Plafond de toute attente en temps réel : large, pour qu'une machine
    /// chargée ne fasse jamais échouer un test juste (#8023). Les durées que
    /// les tests vérifient sont celles de l'horloge simulée, pas celles-ci.
    const PLAFOND: Duration = Duration::from_secs(10);

    /// L'instant de départ de l'horloge simulée : un temps PTP réaliste
    /// (2026), multiple de la milliseconde pour que les horodatages tombent
    /// juste.
    const T0_NS: u64 = 1_793_000_000_000_000_000;
    const MS_NS: u64 = 1_000_000;

    /// Une horloge que seul le test fait avancer (#8023). Avant, les tests
    /// donnaient au cadenceur l'horloge réelle et se coordonnaient avec son
    /// fil par des `sleep` : sur une machine chargée, le fil du test perdait
    /// quelques millisecondes pendant que le fil émetteur continuait d'émettre
    /// un paquet par milliseconde, et les comptes exacts ne tenaient plus.
    /// Ici, tant que le test n'avance pas l'horloge, le cadenceur ne peut ni
    /// émettre, ni prendre de retard, ni compter une sous-alimentation.
    struct HorlogeManuelle {
        /// (temps en ns, génération), changés ensemble par [`Self::sauter`].
        etat: Mutex<(u64, u64)>,
        /// Lectures de `maintenant_ns` : le fil garé sur une échéance relit
        /// l'horloge environ chaque milliseconde réelle.
        lectures: AtomicU64,
    }

    impl HorlogeManuelle {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                etat: Mutex::new((T0_NS, 0)),
                lectures: AtomicU64::new(0),
            })
        }
        fn avancer_ns(&self, ns: u64) {
            self.etat.lock().unwrap().0 += ns;
        }
        /// Saut d'horloge et nouvelle génération d'un seul coup, comme le
        /// suiveur PTP quand il se cale sur un maître.
        fn sauter(&self, ns: u64, generation: u64) {
            let mut e = self.etat.lock().unwrap();
            *e = (e.0 + ns, generation);
        }
    }

    impl SourceHorloge for HorlogeManuelle {
        fn maintenant_ns(&self) -> u64 {
            self.lectures.fetch_add(1, Ordering::AcqRel);
            self.etat.lock().unwrap().0
        }
        fn generation(&self) -> u64 {
            self.etat.lock().unwrap().1
        }
        fn reference(&self) -> crate::horloge::ReferenceHorloge {
            crate::horloge::ReferenceHorloge::Locale
        }
    }

    /// Attend, en temps réel et sous plafond, que `condition` soit vraie.
    fn attendre_que(quoi: &str, condition: impl Fn() -> bool) {
        let debut = std::time::Instant::now();
        while !condition() {
            assert!(debut.elapsed() < PLAFOND, "{quoi} : pas vu en {PLAFOND:?}");
            std::thread::sleep(Duration::from_micros(100));
        }
    }

    fn paquets(stats: &Statistiques) -> u64 {
        stats.paquets.load(Ordering::Relaxed)
    }

    /// Attend que le fil ait émis `n` paquets en tout, sans toucher à
    /// l'horloge (paquet dont l'échéance est déjà là : départ, reprise).
    fn attendre_paquets(stats: &Statistiques, n: u64) {
        attendre_que(&format!("{n} paquets émis"), || paquets(stats) >= n);
        assert_eq!(
            paquets(stats),
            n,
            "paquets émis : un paquet est parti avant son échéance"
        );
    }

    /// Avance l'horloge d'une durée de paquet (1 ms) et attend le paquet qui
    /// arrive alors à échéance : exactement un.
    fn avancer_d_un_paquet(horloge: &HorlogeManuelle, stats: &Statistiques) {
        let n = paquets(stats);
        horloge.avancer_ns(MS_NS);
        attendre_que(&format!("paquet {n} émis"), || paquets(stats) > n);
        assert_eq!(
            paquets(stats),
            n + 1,
            "plus d'un paquet parti pour 1 ms d'horloge"
        );
    }

    /// Attend que le fil soit garé sur l'échéance de son prochain paquet :
    /// il relit l'horloge sans rien émettre. Quatre lectures : un fil qui
    /// n'attendrait pas l'échéance en ferait trois par paquet émis.
    fn attendre_que_le_fil_attende(horloge: &HorlogeManuelle) {
        let l = horloge.lectures.load(Ordering::Acquire);
        attendre_que("fil garé sur l'échéance", || {
            horloge.lectures.load(Ordering::Acquire) >= l + 4
        });
    }

    /// Attend que le flux soit entièrement parti, puis arrête le fil (qui,
    /// depuis l'étape E3, survit à la fin d'un flux).
    fn finir(commandes: &Commandes, fil: JoinHandle<()>) {
        attendre_que("flux fini", || commandes.fini.load(Ordering::Acquire));
        commandes.arret.store(true, Ordering::Release);
        fil.join().unwrap();
    }

    fn parametres() -> ParametresCadence {
        ParametresCadence {
            cadence: 48_000,
            trames_par_paquet: 48,
            type_charge: 98,
            ssrc: 0x1234_5678,
            avance: Duration::ZERO,
            decalage_media: 0,
            pretampon_paquets: 1,
            temps_reel: false,
        }
    }

    #[test]
    fn cent_paquets_partent_en_cent_millisecondes_horodates_de_48_en_48() {
        let (ecoute, s) = ecoute_et_emetteur();
        let (tx, rx) = sync_channel(200);
        for i in 0..100u8 {
            tx.send(Message::Charge(vec![i; 288])).unwrap();
        }
        tx.send(Message::Fin).unwrap();
        let horloge = HorlogeManuelle::new();
        let commandes = Arc::new(Commandes::default());
        let stats = Arc::new(Statistiques::default());
        let fil = lancer(
            s,
            Some(rx),
            horloge.clone(),
            parametres(),
            commandes.clone(),
            stats.clone(),
        )
        .unwrap();
        // Le premier part à t = 0, puis un par milliseconde d'horloge :
        // jamais avant son échéance, ce que vérifiait « au moins 99 ms ».
        attendre_paquets(&stats, 1);
        let mut recus = recevoir(&ecoute, 1);
        for k in 1..100 {
            attendre_que_le_fil_attende(&horloge);
            assert_eq!(paquets(&stats), k, "paquet parti avant son échéance");
            avancer_d_un_paquet(&horloge, &stats);
            recus.extend(recevoir(&ecoute, 1));
        }
        finir(&commandes, fil);
        let entetes: Vec<EnteteRtp> = recus.iter().map(|(e, _)| *e).collect();
        assert!(entetes[0].marqueur);
        assert!(entetes[1..].iter().all(|e| !e.marqueur));
        assert_eq!(entetes[0].horodatage, horodatage_rtp(T0_NS, 48_000));
        for (i, paire) in entetes.windows(2).enumerate() {
            assert_eq!(paire[1].sequence, paire[0].sequence.wrapping_add(1), "{i}");
            assert_eq!(
                paire[1].horodatage,
                paire[0].horodatage.wrapping_add(48),
                "{i}"
            );
            assert_eq!(paire[1].ssrc, 0x1234_5678);
        }
        for (i, (_, c)) in recus.iter().enumerate() {
            assert_eq!(&c[..], &[i as u8; 288][..]);
        }
        let r = stats.releve();
        assert_eq!(r.paquets, 100);
        assert_eq!(r.trames, 4800);
        assert_eq!(r.retard_max_us, 0, "parti à l'échéance, pas après");
        assert_eq!(r.sous_alimentations, 0);
        assert_eq!(r.resynchronisations, 0);
    }

    #[test]
    fn un_trou_de_pcm_tait_le_flux_puis_repart_marque() {
        let (ecoute, s) = ecoute_et_emetteur();
        let (tx, rx) = sync_channel(200);
        let horloge = HorlogeManuelle::new();
        let commandes = Arc::new(Commandes::default());
        let stats = Arc::new(Statistiques::default());
        let fil = lancer(
            s,
            Some(rx),
            horloge.clone(),
            parametres(),
            commandes.clone(),
            stats.clone(),
        )
        .unwrap();
        for _ in 0..5 {
            tx.send(Message::Charge(vec![0; 288])).unwrap();
        }
        attendre_paquets(&stats, 1);
        for _ in 1..5 {
            avancer_d_un_paquet(&horloge, &stats);
        }
        // Tampon vide, mais l'échéance du paquet suivant n'est pas passée :
        // rien à compter.
        assert_eq!(
            stats.releve().sous_alimentations,
            0,
            "sous-alimentation comptée avant l'échéance"
        );
        // 50 ms sans PCM : une sous-alimentation, une seule.
        horloge.avancer_ns(50 * MS_NS);
        attendre_que("sous-alimentation comptée", || {
            stats.sous_alimentations.load(Ordering::Relaxed) >= 1
        });
        for _ in 0..5 {
            tx.send(Message::Charge(vec![0; 288])).unwrap();
        }
        tx.send(Message::Fin).unwrap();
        // La reprise part tout de suite, sur une origine neuve.
        attendre_paquets(&stats, 6);
        for _ in 6..10 {
            avancer_d_un_paquet(&horloge, &stats);
        }
        finir(&commandes, fil);
        let entetes: Vec<EnteteRtp> = recevoir(&ecoute, 10).iter().map(|(e, _)| *e).collect();
        assert!(entetes[0].marqueur && entetes[5].marqueur);
        assert!(
            entetes[1..5]
                .iter()
                .chain(&entetes[6..])
                .all(|e| !e.marqueur)
        );
        assert_eq!(entetes[5].sequence, entetes[4].sequence.wrapping_add(1));
        // Paquet 4 à t = 4 ms, reprise à t = 54 ms : +50 ms, 2400 trames.
        let saut = entetes[5].horodatage.wrapping_sub(entetes[4].horodatage);
        assert_eq!(saut, 48 * 50, "saut d'horodatage");
        let r = stats.releve();
        assert_eq!(r.sous_alimentations, 1);
        assert_eq!(r.resynchronisations, 1);
    }

    #[test]
    fn un_changement_de_generation_repart_d_une_origine_marquee_sur_la_nouvelle_echelle() {
        let (ecoute, s) = ecoute_et_emetteur();
        let (tx, rx) = sync_channel(400);
        for _ in 0..200 {
            tx.send(Message::Charge(vec![0; 288])).unwrap();
        }
        tx.send(Message::Fin).unwrap();
        let horloge = HorlogeManuelle::new();
        let stats = Arc::new(Statistiques::default());
        let mut p = parametres();
        p.decalage_media = 1000;
        let commandes = Arc::new(Commandes::default());
        let fil = lancer(
            s,
            Some(rx),
            horloge.clone(),
            p,
            commandes.clone(),
            stats.clone(),
        )
        .unwrap();
        attendre_paquets(&stats, 1);
        let mut entetes: Vec<EnteteRtp> = recevoir(&ecoute, 1).iter().map(|(e, _)| *e).collect();
        for _ in 1..50 {
            avancer_d_un_paquet(&horloge, &stats);
            entetes.extend(recevoir(&ecoute, 1).iter().map(|(e, _)| *e));
        }
        // Saut d'une heure, comme un calage PTP : nouvelle génération, posée
        // en même temps que le temps. Le paquet 50 part tout de suite, sur
        // la nouvelle échelle.
        const HEURE_NS: u64 = 3_600_000_000_000;
        horloge.sauter(HEURE_NS, 1);
        let apres_saut = T0_NS + 49 * MS_NS + HEURE_NS;
        attendre_paquets(&stats, 51);
        entetes.extend(recevoir(&ecoute, 1).iter().map(|(e, _)| *e));
        for _ in 51..200 {
            avancer_d_un_paquet(&horloge, &stats);
            entetes.extend(recevoir(&ecoute, 1).iter().map(|(e, _)| *e));
        }
        finir(&commandes, fil);
        let marques: Vec<usize> = entetes
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, e)| e.marqueur)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(marques, vec![50], "une seule origine, au paquet 50");
        // Après la rupture, l'horodatage suit la nouvelle échelle (+1 h, soit
        // 172 800 000 trames de plus, modulo 2³²) avec le décalage annoncé.
        let attendu = horodatage_rtp_decale(apres_saut, 48_000, 1000);
        assert_eq!(entetes[50].horodatage, attendu);
        for paire in entetes[50..].windows(2) {
            assert_eq!(paire[1].horodatage, paire[0].horodatage.wrapping_add(48));
        }
        assert_eq!(stats.releve().resynchronisations, 1);
    }

    #[test]
    fn l_arret_interrompt_le_fil_sans_attendre_la_fin() {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.connect("127.0.0.1:9").unwrap();
        let (_tx, rx) = sync_channel::<Message>(1);
        let commandes = Arc::new(Commandes::default());
        let fil = lancer(
            s,
            Some(rx),
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

    fn ecoute_et_emetteur() -> (UdpSocket, UdpSocket) {
        let ecoute = UdpSocket::bind("127.0.0.1:0").unwrap();
        ecoute.set_read_timeout(Some(PLAFOND)).unwrap();
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        s.connect(ecoute.local_addr().unwrap()).unwrap();
        (ecoute, s)
    }

    fn recevoir(ecoute: &UdpSocket, n: usize) -> Vec<(EnteteRtp, Vec<u8>)> {
        let mut t = [0u8; 2048];
        (0..n)
            .map(|_| {
                let n = ecoute.recv(&mut t).unwrap();
                let (e, c) = EnteteRtp::lire(&t[..n]).unwrap();
                (e, c.to_vec())
            })
            .collect()
    }

    /// Ni recul ni chevauchement : chaque paquet commence au plus tôt à la
    /// trame qui suit le précédent, exactement à celle-là s'il n'est pas
    /// marqué ; la séquence avance de un.
    fn verifier_la_continuite(entetes: &[EnteteRtp]) {
        for (i, paire) in entetes.windows(2).enumerate() {
            let pas = paire[1].horodatage.wrapping_sub(paire[0].horodatage) as i32;
            assert!(
                pas >= 48,
                "paquet {} : horodatage +{pas} (< 48 trames)",
                i + 1
            );
            if !paire[1].marqueur {
                assert_eq!(pas, 48, "paquet {} non marqué : +{pas}", i + 1);
            }
            assert_eq!(
                paire[1].sequence,
                paire[0].sequence.wrapping_add(1),
                "séquence au paquet {}",
                i + 1
            );
        }
    }

    #[test]
    fn un_trou_plus_court_qu_un_paquet_ne_fait_ni_reculer_ni_chevaucher_l_horodatage() {
        // Le PCM du paquet 6 arrive juste APRÈS l'envoi du paquet 5, bien
        // avant son échéance (1 ms plus tard). Avant l'étape E3, le fil
        // déclarait une sous-alimentation dès le tampon vide et repartait
        // d'une origine « maintenant » : horodatage +2 à +5 trames au lieu de
        // +48, le paquet 6 recouvrait 90 % du paquet 5.
        let (ecoute, s) = ecoute_et_emetteur();
        let (tx, rx) = sync_channel(200);
        let horloge = HorlogeManuelle::new();
        let commandes = Arc::new(Commandes::default());
        let stats = Arc::new(Statistiques::default());
        let fil = lancer(
            s,
            Some(rx),
            horloge.clone(),
            parametres(),
            commandes.clone(),
            stats.clone(),
        )
        .unwrap();
        let mut recus = Vec::new();
        for tour in 0..4u8 {
            for _ in 0..5 {
                tx.send(Message::Charge(vec![tour; 288])).unwrap();
            }
            for k in 0..5 {
                if tour == 0 && k == 0 {
                    attendre_paquets(&stats, 1);
                } else {
                    avancer_d_un_paquet(&horloge, &stats);
                }
            }
            recus.extend(recevoir(&ecoute, 5));
        }
        tx.send(Message::Fin).unwrap();
        finir(&commandes, fil);
        let entetes: Vec<EnteteRtp> = recus.iter().map(|(e, _)| *e).collect();
        verifier_la_continuite(&entetes);
        // Le PCM est toujours arrivé avant l'échéance : aucune origine neuve.
        assert!(entetes[0].marqueur);
        let marques: Vec<usize> = (1..entetes.len())
            .filter(|&i| entetes[i].marqueur)
            .collect();
        assert!(marques.is_empty(), "paquets marqués : {marques:?}");
        let r = stats.releve();
        assert_eq!(r.sous_alimentations, 0);
        assert_eq!(r.resynchronisations, 0);
    }

    #[test]
    fn la_nouvelle_origine_ne_part_jamais_avant_la_fin_du_dernier_paquet() {
        let dernier = FinDuDernierPaquet {
            fin_ns: 2_000_000_000,
            horodatage_suivant: 96_000,
            generation: 3,
        };
        // 0,3 ms avant la fin du dernier paquet : on attend sa fin.
        let (o, ts) = nouvelle_origine(1_999_700_000, 3, Some(dernier), 0, 48_000, 0);
        assert_eq!((o, ts), (2_000_000_000, 96_000));
        // Plus tard : maintenant, sur l'horloge média.
        let (o, ts) = nouvelle_origine(2_500_000_000, 3, Some(dernier), 0, 48_000, 0);
        assert_eq!((o, ts), (2_500_000_000, 120_000));
        // Un horodatage d'horloge en retard d'une trame (arrondi, décalage)
        // est relevé à la trame suivante, jamais en dessous.
        let d = FinDuDernierPaquet {
            horodatage_suivant: 96_010,
            ..dernier
        };
        let (_, ts) = nouvelle_origine(2_000_000_000, 3, Some(d), 0, 48_000, 0);
        assert_eq!(ts, 96_010);
        // Passage de 2³² : la comparaison est circulaire. L'horloge donne
        // 2³² − 100 (décalage média), la trame suivante est déjà repassée à
        // 5 : 2³² − 100 est AVANT 5, on prend 5.
        let d = FinDuDernierPaquet {
            horodatage_suivant: 5,
            ..dernier
        };
        let decalage = u32::MAX - 96_000 - 99;
        let (_, ts) = nouvelle_origine(2_000_000_000, 3, Some(d), 0, 48_000, decalage);
        assert_eq!(ts, 5);
        // Et 10 après 2³² − 100 n'est pas « avant » : rien à relever.
        let d = FinDuDernierPaquet {
            horodatage_suivant: u32::MAX - 150,
            ..dernier
        };
        let (_, ts) = nouvelle_origine(2_000_000_000, 3, Some(d), 0, 48_000, decalage);
        assert_eq!(ts, u32::MAX - 99);
        // Autre génération : seule la nouvelle échelle compte.
        let (o, ts) = nouvelle_origine(1_000_000_000, 4, Some(dernier), 0, 48_000, 7);
        assert_eq!((o, ts), (1_000_000_000, 48_007));
    }

    #[test]
    fn un_nouveau_flux_jette_l_ancien_continue_la_sequence_et_part_marque() {
        let (ecoute, s) = ecoute_et_emetteur();
        let (tx, rx) = sync_channel(400);
        for _ in 0..300 {
            tx.send(Message::Charge(vec![1; 288])).unwrap();
        }
        let horloge = HorlogeManuelle::new();
        let commandes = Arc::new(Commandes::default());
        let stats = Arc::new(Statistiques::default());
        // Un prétampon de quatre paquets : à la pose du nouveau flux, le fil
        // tient encore trois charges de l'ancien, qui doivent être jetées.
        let mut p = parametres();
        p.pretampon_paquets = 4;
        let fil = lancer(
            s,
            Some(rx),
            horloge.clone(),
            p,
            commandes.clone(),
            stats.clone(),
        )
        .unwrap();
        attendre_paquets(&stats, 1);
        for _ in 1..20 {
            avancer_d_un_paquet(&horloge, &stats);
        }
        let mut recus = recevoir(&ecoute, 20);
        let (tx2, rx2) = sync_channel(400);
        for _ in 0..30 {
            tx2.send(Message::Charge(vec![2; 288])).unwrap();
        }
        tx2.send(Message::Fin).unwrap();
        // Le fil attend l'échéance du paquet 20, déjà tiré de l'ancien flux.
        attendre_que_le_fil_attende(&horloge);
        commandes.installer(7, Some(rx2));
        // Exactement un paquet de l'ancien flux part encore (celui qui
        // attendait son échéance), puis les 30 du nouveau.
        while recus.iter().filter(|(_, c)| c[0] == 2).count() < 30 {
            avancer_d_un_paquet(&horloge, &stats);
            recus.extend(recevoir(&ecoute, 1));
        }
        finir(&commandes, fil);
        drop(tx);
        let premier_nouveau = recus.iter().position(|(_, c)| c[0] == 2).unwrap();
        assert_eq!(
            premier_nouveau, 21,
            "{premier_nouveau} paquets de l'ancien flux (21 attendus)"
        );
        assert!(
            recus[premier_nouveau].0.marqueur,
            "le nouveau flux part marqué"
        );
        assert!(recus[premier_nouveau..].iter().all(|(_, c)| c[0] == 2));
        assert_eq!(recus.len() - premier_nouveau, 30);
        let entetes: Vec<EnteteRtp> = recus.iter().map(|(e, _)| *e).collect();
        verifier_la_continuite(&entetes);
        assert_eq!(stats.flux_courant.load(Ordering::Acquire), 7);
        assert_eq!(
            stats.trames.load(Ordering::Relaxed)
                - stats.trames_au_debut_du_flux.load(Ordering::Relaxed),
            30 * 48
        );
    }

    #[test]
    fn la_pause_tait_le_flux_et_la_reprise_repart_marquee_sans_rien_perdre() {
        let (ecoute, s) = ecoute_et_emetteur();
        let (tx, rx) = sync_channel(400);
        for i in 0..100u8 {
            tx.send(Message::Charge(vec![i; 288])).unwrap();
        }
        tx.send(Message::Fin).unwrap();
        let horloge = HorlogeManuelle::new();
        let commandes = Arc::new(Commandes::default());
        let stats = Arc::new(Statistiques::default());
        let fil = lancer(
            s,
            Some(rx),
            horloge.clone(),
            parametres(),
            commandes.clone(),
            stats.clone(),
        )
        .unwrap();
        attendre_paquets(&stats, 1);
        for _ in 1..30 {
            avancer_d_un_paquet(&horloge, &stats);
        }
        // Le fil attend l'échéance du paquet 30, déjà tiré du canal : c'est
        // le seul qui peut encore partir après l'ordre de pause.
        attendre_que_le_fil_attende(&horloge);
        commandes.pause.store(true, Ordering::Release);
        avancer_d_un_paquet(&horloge, &stats);
        // 60 ms de pause, milliseconde par milliseconde : rien ne part.
        for _ in 0..60 {
            horloge.avancer_ns(MS_NS);
            std::thread::sleep(Duration::from_millis(1));
        }
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(paquets(&stats), 31, "paquets avant la reprise");
        let mut recus = recevoir(&ecoute, 31);
        let avant = recus.len();
        commandes.pause.store(false, Ordering::Release);
        // La reprise part tout de suite, sur une origine neuve.
        attendre_paquets(&stats, 32);
        for _ in 32..100 {
            avancer_d_un_paquet(&horloge, &stats);
        }
        recus.extend(recevoir(&ecoute, 100 - avant));
        finir(&commandes, fil);
        let entetes: Vec<EnteteRtp> = recus.iter().map(|(e, _)| *e).collect();
        verifier_la_continuite(&entetes);
        assert!(entetes[avant].marqueur, "la reprise repart marquée");
        // Paquet 30 à t = 30 ms, reprise à t = 90 ms : l'horloge média a
        // avancé de 60 ms pendant la pause.
        let saut = entetes[avant]
            .horodatage
            .wrapping_sub(entetes[avant - 1].horodatage);
        assert_eq!(saut, 48 * 60, "l'horloge média a avancé pendant la pause");
        // Toutes les charges, dans l'ordre, aucune perdue ni doublée.
        for (i, (_, c)) in recus.iter().enumerate() {
            assert_eq!(c[0], i as u8);
        }
    }
}
