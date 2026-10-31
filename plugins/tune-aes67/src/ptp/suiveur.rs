//! Le suiveur PTPv2 : une instance `statime` ESCLAVE SEULEMENT (décision de
//! Bertrand, 30/10/2026 : Tune ne prend jamais l'horloge d'un réseau), un
//! port, mécanisme de délai de bout en bout (Delay_Req / Delay_Resp), filtre
//! de Kalman de `statime`, sur l'horloge virtuelle de
//! [`super::horloge_virtuelle`].
//!
//! Profil par défaut d'AES67 : domaine 0, Announce toutes les 2 s (log 1),
//! délai de réception de 3 annonces, Delay_Req toutes les 1 s (log 0).
//! Accepte un maître IEEE 1588-2008 (PTPv2.0) comme 1588-2019 (v2.1) ; ses
//! propres messages sortent en v2.0.
//!
//! Calage et repli, sans ambiguïté :
//! - CALÉE quand le port est esclave d'un maître, que l'horloge a été réglée
//!   au moins une fois depuis le choix de ce maître, que le filtre a une
//!   estimation (délai moyen mesuré), qu'aucun saut n'a eu lieu depuis
//!   `stabilisation`, qu'une Sync est arrivée depuis moins de
//!   `delai_sans_sync`, et que le décalage estimé est sous `seuil_calage`
//!   (hystérésis : on ne décroche qu'à dix fois le seuil) ;
//! - sinon REPLI SUR L'HORLOGE LOCALE : le SDP annonce `ts-refclk:local`,
//!   le journal le dit (`aes67_ptp_repli_horloge_locale`, avec la raison) et
//!   `GET /api/v1/ext/aes67` l'expose. Le temps continue SANS SAUT sur la
//!   dernière fréquence apprise (le quartz local corrigé), pour que le flux
//!   ne s'interrompe pas.
//!
//! Chaque passage calée ↔ repli, chaque changement de maître et chaque saut
//! d'horloge avancent la génération : le cadenceur repart d'une origine
//! marquée et le SDP est réannoncé.

use std::collections::VecDeque;
use std::net::SocketAddrV4;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rand::SeedableRng;
use rand::rngs::StdRng;
use serde::Serialize;
use statime::PtpInstance;
use statime::config::{
    AcceptAnyMaster, ClockIdentity, DelayMechanism, InstanceConfig, PortConfig, PtpMinorVersion,
    SdoId, TimePropertiesDS, TimeSource,
};
use statime::filters::{KalmanConfiguration, KalmanFilter};
use statime::observability::port::PortState;
use statime::port::{NoForwardedTLVs, PortAction, PortActionIterator, TimestampContext};
use statime::time::{Duration as DureePtp, Interval};
use tracing::{debug, info, warn};

use super::horloge_virtuelle::HorlogeVirtuelle;
use super::transport::{Canal, ConfigReseauPtp, Recu, TransportUdp};
use crate::horloge::{ReferenceHorloge, SourceHorloge};

/// Les réglages du suiveur.
#[derive(Debug, Clone, Copy)]
pub struct ConfigPtp {
    pub domaine: u8,
    pub reseau: ConfigReseauPtp,
    /// log2 de l'intervalle d'annonce attendu (fenêtre des maîtres étrangers
    /// et cadence du BMCA).
    pub intervalle_annonce_log2: i8,
    /// Annonces manquées avant de déclarer le maître perdu.
    pub delai_annonce: u8,
    /// log2 de l'intervalle moyen entre deux Delay_Req.
    pub intervalle_delay_req_log2: i8,
    /// Décalage estimé sous lequel l'horloge est dite calée.
    pub seuil_calage: Duration,
    /// Sans Sync depuis ce délai, on se replie sur l'horloge locale.
    pub delai_sans_sync: Duration,
    /// Délai sans saut d'horloge avant de se dire calé.
    pub stabilisation: Duration,
    /// Au-delà de ce décalage estimé, le filtre SAUTE au lieu de ralentir
    /// ou d'accélérer l'horloge. 10 ms (au lieu de 1 ms par défaut dans
    /// `statime`) : avec l'horodatage logiciel, un pic de latence de
    /// l'ordonnanceur ne doit pas casser le flux par un saut.
    pub seuil_de_saut: Duration,
    /// Identité d'horloge (EUI-64) ; tirée au hasard si absente.
    pub identite: Option<[u8; 8]>,
}

impl ConfigPtp {
    /// Le profil par défaut d'AES67, sur 224.0.1.129, ports 319 et 320.
    pub fn aes67(interface: Option<std::net::Ipv4Addr>) -> Self {
        Self {
            domaine: 0,
            reseau: ConfigReseauPtp::aes67(interface),
            intervalle_annonce_log2: 1,
            delai_annonce: 3,
            intervalle_delay_req_log2: 0,
            seuil_calage: Duration::from_millis(1),
            delai_sans_sync: Duration::from_secs(3),
            stabilisation: Duration::from_secs(2),
            seuil_de_saut: Duration::from_millis(10),
            identite: None,
        }
    }
}

/// Ce que `GET /api/v1/ext/aes67` montre du PTP.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct EtatPtp {
    /// Le fil PTP tourne (faux si les ports n'ont pas pu être ouverts).
    pub actif: bool,
    /// `ecoute`, `esclave`, `passif`, `maitre`, `defaillant`, `arrete`,
    /// `indisponible`.
    pub etat_port: String,
    pub calee: bool,
    /// Vrai dès que l'horloge n'est pas calée : le flux part sur l'horloge
    /// locale, annoncé `ts-refclk:local`.
    pub repli_horloge_locale: bool,
    pub raison_du_repli: Option<String>,
    /// Le grand maître suivi, au format du SDP (`00-1D-C1-FF-FE-12-34-56`).
    pub maitre: Option<String>,
    pub domaine: u8,
    pub decalage_ns: Option<i64>,
    pub delai_moyen_ns: Option<i64>,
    pub frequence_ppm: f64,
    pub sauts: u64,
    pub generation: u64,
    pub derniere_sync_il_y_a_ms: Option<u64>,
    /// Toujours `logiciel` : voir [`super::transport`].
    pub horodatage: &'static str,
    pub identite: String,
    pub erreur: Option<String>,
}

/// L'EUI-64 au format de `ts-refclk` (RFC 7273) : octets en hexadécimal
/// majuscule séparés par des tirets.
pub fn eui64(id: &[u8; 8]) -> String {
    id.iter()
        .map(|o| format!("{o:02X}"))
        .collect::<Vec<_>>()
        .join("-")
}

#[derive(Debug)]
struct Partage {
    actif: bool,
    etat_port: &'static str,
    calee: bool,
    raison: Option<&'static str>,
    maitre: Option<[u8; 8]>,
    decalage_ns: Option<i64>,
    delai_ns: Option<i64>,
    derniere_sync: Option<Instant>,
    erreur: Option<String>,
}

impl Partage {
    /// Calée, ET une Sync récente : si le fil PTP s'arrêtait net, la
    /// lecture se replierait d'elle-même sur l'horloge locale.
    fn calee(&self, delai_sans_sync: Duration) -> bool {
        self.calee
            && self
                .derniere_sync
                .is_some_and(|t| t.elapsed() < delai_sans_sync)
    }
}

pub struct SuiveurPtp {
    horloge: HorlogeVirtuelle,
    generation: Arc<AtomicU64>,
    partage: Arc<Mutex<Partage>>,
    arret: Arc<AtomicBool>,
    fils: Mutex<Vec<JoinHandle<()>>>,
    domaine: u8,
    delai_sans_sync: Duration,
    identite: [u8; 8],
    adresses: Option<(SocketAddrV4, SocketAddrV4)>,
}

fn graine() -> u64 {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    t ^ (u64::from(std::process::id()) << 32)
}

/// Une identité tirée au hasard, bit « administrée localement » posé : elle
/// ne sert qu'à adresser nos Delay_Req, jamais à se porter maître.
fn identite_aleatoire() -> [u8; 8] {
    use rand::RngCore;
    let mut id = [0u8; 8];
    StdRng::seed_from_u64(graine()).fill_bytes(&mut id);
    id[0] = (id[0] & 0xfc) | 0x02;
    id
}

impl SuiveurPtp {
    /// Ouvre les sockets et lance le fil PTP. Ne rend jamais d'erreur : un
    /// port refusé (319/320 sans droit) laisse le suiveur INACTIF, en repli
    /// sur l'horloge locale, avec l'erreur dans [`SuiveurPtp::etat`].
    pub fn demarrer(cfg: ConfigPtp) -> Arc<Self> {
        let generation = Arc::new(AtomicU64::new(0));
        let horloge = HorlogeVirtuelle::new(generation.clone());
        let identite = cfg.identite.unwrap_or_else(identite_aleatoire);
        let partage = Arc::new(Mutex::new(Partage {
            actif: false,
            etat_port: "indisponible",
            calee: false,
            raison: Some("aucun maître vu"),
            maitre: None,
            decalage_ns: None,
            delai_ns: None,
            derniere_sync: None,
            erreur: None,
        }));
        let arret = Arc::new(AtomicBool::new(false));
        let mut fils = Vec::new();
        let mut adresses = None;
        let lancement =
            TransportUdp::ouvrir(cfg.reseau).and_then(|transport| {
                adresses = transport.adresses_locales().ok();
                let (tx, rx) = channel();
                fils.extend(transport.lancer_la_reception(horloge.clone(), tx, arret.clone())?);
                let (h, p, g, a) = (
                    horloge.clone(),
                    partage.clone(),
                    generation.clone(),
                    arret.clone(),
                );
                fils.push(std::thread::Builder::new().name("aes67-ptp".into()).spawn(
                    move || {
                        let partage = p.clone();
                        let generation = g.clone();
                        let issue = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            boucle(cfg, identite, transport, rx, h, p, g, a)
                        }));
                        if issue.is_err() {
                            // Le fil PTP est mort : repli explicite et durable.
                            let mut p = partage.lock().unwrap_or_else(|e| e.into_inner());
                            if p.calee {
                                generation.fetch_add(1, Ordering::AcqRel);
                            }
                            p.calee = false;
                            p.actif = false;
                            p.etat_port = "defaillant";
                            p.raison = Some("fil PTP arrêté sur erreur");
                            p.erreur = Some("fil PTP arrêté sur erreur (panique)".into());
                            warn!("aes67_ptp_fil_mort_repli_horloge_locale");
                        }
                    },
                )?);
                Ok(())
            });
        {
            let mut p = partage.lock().unwrap_or_else(|e| e.into_inner());
            match lancement {
                Ok(()) => {
                    p.actif = true;
                    p.etat_port = "ecoute";
                    info!(
                        identite = %eui64(&identite),
                        domaine = cfg.domaine,
                        ?adresses,
                        "aes67_ptp_suiveur_demarre"
                    );
                }
                Err(e) => {
                    arret.store(true, Ordering::Release);
                    warn!(
                        error = %e,
                        "aes67_ptp_indisponible_repli_horloge_locale"
                    );
                    p.erreur = Some(e.to_string());
                    p.raison = Some("PTP indisponible");
                }
            }
        }
        Arc::new(Self {
            horloge,
            generation,
            partage,
            arret,
            fils: Mutex::new(fils),
            domaine: cfg.domaine,
            delai_sans_sync: cfg.delai_sans_sync,
            identite,
            adresses,
        })
    }

    fn partage(&self) -> MutexGuard<'_, Partage> {
        self.partage.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Les adresses (événement, général) où le suiveur écoute.
    pub fn adresses_locales(&self) -> Option<(SocketAddrV4, SocketAddrV4)> {
        self.adresses
    }

    /// L'horloge virtuelle (pour le banc : la comparer au maître).
    pub fn horloge(&self) -> &HorlogeVirtuelle {
        &self.horloge
    }

    pub fn etat(&self) -> EtatPtp {
        let p = self.partage();
        let calee = p.calee(self.delai_sans_sync);
        let raison = if p.calee && !calee {
            "aucune Sync récente du maître"
        } else {
            p.raison.unwrap_or("non calée")
        };
        EtatPtp {
            actif: p.actif,
            etat_port: p.etat_port.to_string(),
            calee,
            repli_horloge_locale: !calee,
            raison_du_repli: (!calee).then(|| raison.to_string()),
            maitre: p.maitre.as_ref().map(eui64),
            domaine: self.domaine,
            decalage_ns: p.decalage_ns,
            delai_moyen_ns: p.delai_ns,
            frequence_ppm: self.horloge.ppm(),
            sauts: self.horloge.sauts(),
            generation: self.generation.load(Ordering::Acquire),
            derniere_sync_il_y_a_ms: p.derniere_sync.map(|t| t.elapsed().as_millis() as u64),
            horodatage: "logiciel",
            identite: eui64(&self.identite),
            erreur: p.erreur.clone(),
        }
    }

    /// Arrête les fils. Idempotent.
    pub fn arreter(&self) {
        self.arret.store(true, Ordering::Release);
        let fils: Vec<_> = self
            .fils
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect();
        for f in fils {
            let _ = f.join();
        }
        let mut p = self.partage();
        if p.actif {
            p.actif = false;
            p.etat_port = "arrete";
            if p.calee {
                p.calee = false;
                self.generation.fetch_add(1, Ordering::AcqRel);
            }
            p.raison = Some("suiveur arrêté");
        }
    }
}

impl Drop for SuiveurPtp {
    fn drop(&mut self) {
        self.arreter();
    }
}

impl SourceHorloge for SuiveurPtp {
    fn maintenant_ns(&self) -> u64 {
        self.horloge.maintenant_ns()
    }
    fn reference(&self) -> ReferenceHorloge {
        let p = self.partage();
        match (p.calee(self.delai_sans_sync), p.maitre) {
            (true, Some(identite_maitre)) => ReferenceHorloge::Ptp {
                identite_maitre,
                domaine: self.domaine,
            },
            _ => ReferenceHorloge::Locale,
        }
    }
    fn calee(&self) -> bool {
        self.partage().calee(self.delai_sans_sync)
    }
    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
}

// --- Le fil PTP ---------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Minuteur {
    Annonce = 0,
    Sync = 1,
    DelayReq = 2,
    ReceptionAnnonce = 3,
    Filtre = 4,
}

const MINUTEURS: [Minuteur; 5] = [
    Minuteur::Annonce,
    Minuteur::Sync,
    Minuteur::DelayReq,
    Minuteur::ReceptionAnnonce,
    Minuteur::Filtre,
];

/// Une action de `statime`, détachée du port qui l'a produite.
enum Action {
    Evenement {
        context: TimestampContext,
        donnees: Vec<u8>,
    },
    General(Vec<u8>),
    Armer(Minuteur, Duration),
}

fn posseder(actions: PortActionIterator<'_>) -> Vec<Action> {
    actions
        .filter_map(|a| match a {
            PortAction::SendEvent { context, data, .. } => Some(Action::Evenement {
                context,
                donnees: data.to_vec(),
            }),
            PortAction::SendGeneral { data, .. } => Some(Action::General(data.to_vec())),
            PortAction::ResetAnnounceTimer { duration } => {
                Some(Action::Armer(Minuteur::Annonce, duration))
            }
            PortAction::ResetSyncTimer { duration } => {
                Some(Action::Armer(Minuteur::Sync, duration))
            }
            PortAction::ResetDelayRequestTimer { duration } => {
                Some(Action::Armer(Minuteur::DelayReq, duration))
            }
            PortAction::ResetAnnounceReceiptTimer { duration } => {
                Some(Action::Armer(Minuteur::ReceptionAnnonce, duration))
            }
            PortAction::ResetFilterUpdateTimer { duration } => {
                Some(Action::Armer(Minuteur::Filtre, duration))
            }
            PortAction::ForwardTLV { .. } => None,
        })
        .collect()
}

/// Un message d'événement Sync de notre domaine ?
fn est_une_sync(donnees: &[u8], domaine: u8) -> bool {
    donnees.len() >= 44 && donnees[0] & 0x0f == 0x0 && donnees[4] == domaine
}

fn nom_de_l_etat(e: PortState) -> &'static str {
    match e {
        PortState::Faulty => "defaillant",
        PortState::Listening => "ecoute",
        PortState::Master => "maitre",
        PortState::Passive => "passif",
        PortState::Slave => "esclave",
        // Initialisation, désactivé, pré-maître, non calibré : états
        // transitoires que l'esclave seul ne tient pas longtemps.
        _ => "transitoire",
    }
}

/// Ce que le fil retient d'un tour à l'autre pour décider du calage.
struct Suivi {
    maitre: Option<[u8; 8]>,
    corrections_au_choix: u64,
    calee: bool,
}

#[allow(clippy::too_many_arguments)]
fn boucle(
    cfg: ConfigPtp,
    identite: [u8; 8],
    transport: TransportUdp,
    rx: Receiver<Recu>,
    horloge: HorlogeVirtuelle,
    partage: Arc<Mutex<Partage>>,
    generation: Arc<AtomicU64>,
    arret: Arc<AtomicBool>,
) {
    let instance = PtpInstance::<KalmanFilter>::new(
        InstanceConfig {
            clock_identity: ClockIdentity(identite),
            // Les pires priorités, et surtout `slave_only` : jamais maître.
            priority_1: 255,
            priority_2: 255,
            domain_number: cfg.domaine,
            sdo_id: SdoId::default(),
            slave_only: true,
            path_trace: false,
        },
        TimePropertiesDS::new_arbitrary_time(false, false, TimeSource::InternalOscillator),
    );
    let port = instance.add_port(
        PortConfig {
            acceptable_master_list: AcceptAnyMaster,
            delay_mechanism: DelayMechanism::E2E {
                interval: Interval::from_log_2(cfg.intervalle_delay_req_log2),
            },
            announce_interval: Interval::from_log_2(cfg.intervalle_annonce_log2),
            announce_receipt_timeout: cfg.delai_annonce,
            sync_interval: Interval::from_log_2(-3),
            master_only: false,
            delay_asymmetry: DureePtp::default(),
            minor_ptp_version: PtpMinorVersion::Zero,
        },
        KalmanConfiguration {
            step_threshold: DureePtp::from_seconds(cfg.seuil_de_saut.as_secs_f64()),
            ..KalmanConfiguration::default()
        },
        horloge.clone(),
        StdRng::seed_from_u64(graine()),
    );
    let (mut port, premieres) = port.end_bmca();
    let mut file: VecDeque<Action> = posseder(premieres).into();
    let mut minuteurs: [Option<Instant>; 5] = [None; 5];
    let mut prochain_bmca = Instant::now() + instance.bmca_interval();
    let mut suivi = Suivi {
        maitre: None,
        corrections_au_choix: horloge.corrections(),
        calee: false,
    };
    let seuil_ns = i64::try_from(cfg.seuil_calage.as_nanos()).unwrap_or(i64::MAX);

    'fil: loop {
        // 1. Ce que `statime` a demandé.
        while let Some(action) = file.pop_front() {
            match action {
                Action::Evenement { context, donnees } => {
                    match transport.envoyer_evenement(&donnees, &horloge) {
                        Ok(t) => file.extend(posseder(port.handle_send_timestamp(context, t))),
                        Err(e) => debug!(error = %e, "aes67_ptp_envoi_evenement_refuse"),
                    }
                }
                Action::General(donnees) => {
                    if let Err(e) = transport.envoyer_general(&donnees) {
                        debug!(error = %e, "aes67_ptp_envoi_general_refuse");
                    }
                }
                Action::Armer(m, d) => minuteurs[m as usize] = Some(Instant::now() + d),
            }
        }
        if arret.load(Ordering::Acquire) {
            break 'fil;
        }

        // 2. Le BMCA, à sa cadence.
        if Instant::now() >= prochain_bmca {
            let mut en_bmca = port.start_bmca();
            instance.bmca(&mut [&mut en_bmca]);
            let (p, actions) = en_bmca.end_bmca();
            port = p;
            file.extend(posseder(actions));
            prochain_bmca = Instant::now() + instance.bmca_interval();
        }

        // 3. Ce que le monde en voit.
        publier(
            &cfg,
            seuil_ns,
            port.is_steering(),
            nom_de_l_etat(port.port_ds().port_state),
            instance.parent_ds().grandmaster_identity.0,
            port.port_current_ds_contribution().map(|e| {
                (
                    e.offset_from_master.nanos_rounded() as i64,
                    e.mean_delay.nanos_rounded() as i64,
                )
            }),
            &horloge,
            &partage,
            &generation,
            &mut suivi,
        );
        if !file.is_empty() {
            continue;
        }

        // 4. Attendre un message ou le prochain minuteur.
        let maintenant = Instant::now();
        let echeance = minuteurs
            .iter()
            .flatten()
            .copied()
            .chain([prochain_bmca, maintenant + Duration::from_millis(100)])
            .min()
            .unwrap_or(maintenant);
        match rx.recv_timeout(echeance.saturating_duration_since(maintenant)) {
            Ok(recu) => {
                let actions = match recu.canal {
                    Canal::Evenement => {
                        if port.is_steering() && est_une_sync(&recu.donnees, cfg.domaine) {
                            partage
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .derniere_sync = Some(Instant::now());
                        }
                        port.handle_event_receive(&recu.donnees, recu.horodatage)
                    }
                    Canal::General => port.handle_general_receive(&recu.donnees),
                };
                file.extend(posseder(actions));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break 'fil,
        }

        // 5. Les minuteurs échus.
        let maintenant = Instant::now();
        for m in MINUTEURS {
            if minuteurs[m as usize].is_some_and(|t| t <= maintenant) {
                minuteurs[m as usize] = None;
                let actions = match m {
                    Minuteur::Annonce => port.handle_announce_timer(&mut NoForwardedTLVs),
                    Minuteur::Sync => port.handle_sync_timer(),
                    Minuteur::DelayReq => port.handle_delay_request_timer(),
                    Minuteur::ReceptionAnnonce => port.handle_announce_receipt_timer(),
                    Minuteur::Filtre => port.handle_filter_update_timer(),
                };
                file.extend(posseder(actions));
            }
        }
    }
    debug!("aes67_ptp_fil_termine");
}

#[allow(clippy::too_many_arguments)]
fn publier(
    cfg: &ConfigPtp,
    seuil_ns: i64,
    esclave: bool,
    etat_port: &'static str,
    grand_maitre: [u8; 8],
    estimation: Option<(i64, i64)>,
    horloge: &HorlogeVirtuelle,
    partage: &Mutex<Partage>,
    generation: &AtomicU64,
    suivi: &mut Suivi,
) {
    let maitre = esclave.then_some(grand_maitre);
    let mut p = partage.lock().unwrap_or_else(|e| e.into_inner());
    if maitre != suivi.maitre {
        suivi.corrections_au_choix = horloge.corrections();
        match maitre {
            Some(m) => info!(maitre = %eui64(&m), "aes67_ptp_maitre_choisi"),
            None => {
                p.derniere_sync = None;
                if let Some(ancien) = suivi.maitre {
                    warn!(maitre = %eui64(&ancien), "aes67_ptp_maitre_perdu");
                }
            }
        }
        if suivi.calee {
            // Le maître suivi a changé : la référence annoncée aussi.
            generation.fetch_add(1, Ordering::AcqRel);
        }
        suivi.calee = false;
        suivi.maitre = maitre;
    }
    let sync_recente = p
        .derniere_sync
        .is_some_and(|t| t.elapsed() < cfg.delai_sans_sync);
    let regle = horloge.corrections() > suivi.corrections_au_choix;
    // Le filtre rend (0, 0) tant qu'il n'a rien mesuré : un délai moyen
    // nul n'est pas une estimation.
    let estime = estimation.is_some_and(|(_, delai)| delai != 0);
    let stable = horloge
        .dernier_saut()
        .is_none_or(|t| t.elapsed() >= cfg.stabilisation);
    let decalage = estimation.map(|(d, _)| d.unsigned_abs());
    let seuil = if suivi.calee {
        seuil_ns.saturating_mul(10)
    } else {
        seuil_ns
    } as u64;
    let dans_le_seuil = decalage.is_some_and(|d| d < seuil);
    let calee = esclave && regle && estime && stable && sync_recente && dans_le_seuil;
    let raison = if !esclave {
        "aucun maître vu"
    } else if !sync_recente {
        "aucune Sync récente du maître"
    } else if !regle || !estime {
        "horloge pas encore réglée sur le maître"
    } else if !stable {
        "horloge sautée, en stabilisation"
    } else if !dans_le_seuil {
        "décalage au-dessus du seuil"
    } else {
        ""
    };
    if calee != suivi.calee {
        generation.fetch_add(1, Ordering::AcqRel);
        if calee {
            info!(
                maitre = %maitre.as_ref().map(eui64).unwrap_or_default(),
                decalage_ns = estimation.map(|(d, _)| d),
                "aes67_ptp_cale"
            );
        } else {
            warn!(raison, "aes67_ptp_repli_horloge_locale");
        }
        suivi.calee = calee;
    }
    p.etat_port = etat_port;
    p.calee = calee;
    p.raison = (!calee).then_some(raison);
    p.maitre = maitre;
    p.decalage_ns = estimation.map(|(d, _)| d);
    p.delai_ns = estimation.map(|(_, d)| d);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn l_identite_s_ecrit_comme_dans_ts_refclk() {
        assert_eq!(
            eui64(&[0x00, 0x1d, 0xc1, 0xff, 0xfe, 0x12, 0x34, 0x56]),
            "00-1D-C1-FF-FE-12-34-56"
        );
        let id = identite_aleatoire();
        assert_eq!(id[0] & 0x03, 0x02, "administrée localement, unicast");
    }

    #[test]
    fn le_profil_par_defaut_est_celui_d_aes67() {
        let c = ConfigPtp::aes67(None);
        assert_eq!(c.domaine, 0);
        assert_eq!(c.intervalle_annonce_log2, 1);
        assert_eq!(c.delai_annonce, 3);
        assert_eq!(c.reseau.liaison_evenement.port(), 319);
    }

    #[test]
    fn la_sync_se_reconnait_a_son_type_et_a_son_domaine() {
        let mut m = [0u8; 44];
        assert!(est_une_sync(&m, 0));
        assert!(!est_une_sync(&m, 1));
        m[0] = 0x08; // Follow_Up
        assert!(!est_une_sync(&m, 0));
        assert!(!est_une_sync(&m[..30], 0));
    }

    #[test]
    fn sans_maitre_le_suiveur_se_dit_en_repli_sur_l_horloge_locale() {
        let rien = "127.0.0.1:9".parse().unwrap();
        let mut cfg = ConfigPtp::aes67(None);
        cfg.reseau = ConfigReseauPtp::unicast(Ipv4Addr::LOCALHOST, rien, rien);
        cfg.intervalle_annonce_log2 = -3;
        let s = SuiveurPtp::demarrer(cfg);
        std::thread::sleep(Duration::from_millis(400));
        let e = s.etat();
        assert!(e.actif);
        assert!(!e.calee && e.repli_horloge_locale);
        assert_eq!(e.raison_du_repli.as_deref(), Some("aucun maître vu"));
        assert_eq!(e.horodatage, "logiciel");
        assert_eq!(s.reference(), ReferenceHorloge::Locale);
        assert!(!s.calee());
        assert_eq!(s.generation(), 0);
        s.arreter();
        assert_eq!(s.etat().etat_port, "arrete");
    }

    #[test]
    fn des_ports_refuses_laissent_le_suiveur_inactif_en_repli() {
        // Un port déjà pris en exclusivité (pas de SO_REUSEADDR) : la
        // liaison échoue comme 319 sans droit, sans avoir besoin de root.
        let pris = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let std::net::SocketAddr::V4(adresse) = pris.local_addr().unwrap() else {
            unreachable!()
        };
        let mut cfg = ConfigPtp::aes67(None);
        cfg.reseau = ConfigReseauPtp::unicast(Ipv4Addr::LOCALHOST, adresse, adresse);
        cfg.reseau.liaison_evenement = adresse;
        let s = SuiveurPtp::demarrer(cfg);
        let e = s.etat();
        assert!(!e.actif);
        assert_eq!(e.etat_port, "indisponible");
        assert!(e.repli_horloge_locale);
        assert!(e.erreur.is_some(), "{e:?}");
        assert_eq!(s.reference(), ReferenceHorloge::Locale);
        // L'horloge tourne quand même : l'émetteur peut jouer.
        let a = s.maintenant_ns();
        std::thread::sleep(Duration::from_millis(2));
        assert!(s.maintenant_ns() > a);
    }
}
