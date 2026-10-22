//! L'émetteur d'un flux AIR : cadence, pré-remplissage, régulation sur les
//! retours `Df`, retransmission des paquets réclamés.
//!
//! Le cycle de vie reproduit celui que les captures montrent chez Roon :
//!
//! 1. un flux neuf part de la position 0 par **0,5 s de silence numérique** ;
//! 2. les paquets partent **au rythme réel**, par rafales ; tant que moins de
//!    **2 s** sont parties, le bit « lecture » vaut 0 (pré-remplissage) ;
//! 3. l'ampli répond par un `Df` toutes les 50 ms : position jouée, plages
//!    manquantes. L'émetteur tient l'avance `envoyé − joué ≈ 2,02 s` en
//!    extrapolant la position jouée avec l'horloge locale : c'est ce qui
//!    absorbe la dérive entre les deux horloges ;
//! 4. un paquet réclamé est renvoyé **à l'identique**, au plus une fois
//!    toutes les 50 ms ; les paquets encodés sont donc gardés en anneau ;
//! 5. en fin de source, 0,5 s de silence, puis on laisse l'ampli vider son
//!    tampon avant le paquet d'arrêt (envoyé par l'appelant).
//!
//! Sans `Df` dans la seconde qui suit le premier paquet, l'ampli n'accepte
//! pas le flux (mauvaise session, mauvaise source, veille…) : on arrête et on
//! le dit, sans insister.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::protocole::{EnteteAudio, TRAMES_PAR_PAQUET, analyser_retour, paquet_audio};

/// Les durées du cycle de vie. [`Cadence::AIR`] porte les valeurs relevées
/// sur les captures ; les tests en utilisent une version courte, pour que le
/// simulateur tourne en une seconde au lieu de cinq.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cadence {
    pub silence_de_tete: Duration,
    pub pre_remplissage: Duration,
    pub avance_cible: Duration,
    pub periode: Duration,
    pub silence_de_queue: Duration,
    pub delai_premier_retour: Duration,
    pub delai_retour_perdu: Duration,
    pub intervalle_de_renvoi: Duration,
    /// Au-delà, on cesse d'attendre que l'ampli ait tout joué.
    pub vidage_max: Duration,
    pub attente_allumage: Duration,
    pub attente_source: Duration,
}

impl Cadence {
    /// Les valeurs observées chez Roon (captures de Rémi, fil 2336).
    pub const AIR: Self = Self {
        silence_de_tete: Duration::from_millis(500),
        pre_remplissage: Duration::from_secs(2),
        avance_cible: Duration::from_millis(2_020),
        periode: Duration::from_millis(40),
        silence_de_queue: Duration::from_millis(500),
        delai_premier_retour: Duration::from_secs(1),
        delai_retour_perdu: Duration::from_secs(2),
        intervalle_de_renvoi: Duration::from_millis(50),
        vidage_max: Duration::from_secs(5),
        attente_allumage: Duration::from_secs(20),
        attente_source: Duration::from_secs(3),
    };
}

/// `duree` en trames à `frequence`, arrondie par défaut à un multiple de 200
/// (taille d'un paquet) : 0,5 s à 44,1 kHz donne 22 000 trames, comme Roon.
#[must_use]
pub fn trames_arrondies(frequence: u32, duree: Duration) -> u64 {
    let trames = (f64::from(frequence) * duree.as_secs_f64()) as u64;
    trames / TRAMES_PAR_PAQUET as u64 * TRAMES_PAR_PAQUET as u64
}

fn trames_ecoulees(frequence: u32, duree: Duration) -> u64 {
    (f64::from(frequence) * duree.as_secs_f64()) as u64
}

/// Ce qu'un flux a besoin de savoir pour partir.
#[derive(Debug, Clone, Copy)]
pub struct ParametresFlux {
    pub session: [u8; 4],
    pub flux: u32,
    pub frequence: u32,
    pub cadence: Cadence,
}

/// Ce que l'on observe d'un flux en cours, lu par la sortie pour son état.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EtatFlux {
    pub silence_de_tete: u64,
    pub position_envoyee: u64,
    pub position_jouee: u64,
    pub retours_recus: u64,
    pub paquets_envoyes: u64,
    pub paquets_renvoyes: u64,
    /// La source est épuisée et tout est parti, silence de queue compris.
    pub tout_envoye: bool,
}

/// Comment un flux s'est terminé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinDeFlux {
    /// La source est épuisée et l'ampli a tout joué : fin de piste.
    Naturelle,
    /// Le flux s'est interrompu. `envoyer_l_arret` est faux quand un autre
    /// hôte a pris l'ampli : on ne se bat pas pour lui.
    Interrompu {
        motif: String,
        envoyer_l_arret: bool,
    },
}

impl FinDeFlux {
    #[must_use]
    pub fn interrompu(motif: impl Into<String>) -> Self {
        Self::Interrompu {
            motif: motif.into(),
            envoyer_l_arret: true,
        }
    }
}

/// Un lot de trames stéréo 24 bits venu du décodeur, ou son échec.
pub type LotDeTrames = Result<Vec<[i32; 2]>, String>;

/// Vérifiée à chaque période : `Err` arrête le flux (ampli parti, source
/// changée à la télécommande, autre hôte).
pub type Garde = Arc<dyn Fn(&EtatFlux) -> Result<(), FinDeFlux> + Send + Sync>;

struct PaquetGarde {
    position: u64,
    octets: Vec<u8>,
    renvoye_a: Option<Instant>,
}

/// Diffuse un flux jusqu'à sa fin. Ne s'arrête jamais en silence : la
/// valeur rendue dit pourquoi.
pub async fn diffuser(
    socket: Arc<UdpSocket>,
    ampli: SocketAddr,
    p: ParametresFlux,
    mut source: mpsc::Receiver<LotDeTrames>,
    etat: Arc<Mutex<EtatFlux>>,
    garde: Garde,
) -> FinDeFlux {
    let c = p.cadence;
    let f = p.frequence;
    let silence_de_tete = trames_arrondies(f, c.silence_de_tete);
    let seuil_de_lecture = trames_arrondies(f, c.pre_remplissage);
    let avance = trames_ecoulees(f, c.avance_cible);
    let silence_de_queue = trames_arrondies(f, c.silence_de_queue).max(TRAMES_PAR_PAQUET as u64);
    {
        let mut e = verrou(&etat);
        *e = EtatFlux {
            silence_de_tete,
            ..EtatFlux::default()
        };
    }

    let t0 = Instant::now();
    let mut horloge = tokio::time::interval(c.periode);
    horloge.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut tampon: VecDeque<[i32; 2]> = VecDeque::new();
    let mut source_finie = false;
    let mut queue_ajoutee = false;
    let mut position: u64 = 0;
    let mut position_finale: Option<u64> = None;
    let mut fin_envoyee_a: Option<Instant> = None;
    let mut anneau: VecDeque<PaquetGarde> = VecDeque::new();
    let mut premier_envoi: Option<Instant> = None;
    let mut dernier_retour: Option<Instant> = None;
    // La position jouée du dernier `Df`, et l'instant où on l'a lue.
    let mut jouee: u64 = 0;
    let mut jouee_lue_a = t0;
    let mut tampon_reception = vec![0u8; 2048];

    loop {
        tokio::select! {
            _ = horloge.tick() => {
                let instantane = verrou(&etat).clone();
                if let Err(fin) = garde(&instantane) {
                    return fin;
                }
                // Ce que le décodeur a produit depuis la dernière période.
                loop {
                    match source.try_recv() {
                        Ok(Ok(lot)) => tampon.extend(lot),
                        Ok(Err(e)) => return FinDeFlux::interrompu(format!("décodage : {e}")),
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => {
                            source_finie = true;
                            break;
                        }
                    }
                }
                let cible = if jouee > 0 {
                    jouee + trames_ecoulees(f, jouee_lue_a.elapsed()) + avance
                } else {
                    // Pré-remplissage : au rythme réel, une période d'avance.
                    trames_ecoulees(f, t0.elapsed() + c.periode)
                };
                while position_finale.is_none() && position < cible {
                    let trames: Vec<[i32; 2]> = if position < silence_de_tete {
                        vec![[0, 0]; TRAMES_PAR_PAQUET]
                    } else if tampon.len() >= TRAMES_PAR_PAQUET {
                        tampon.drain(..TRAMES_PAR_PAQUET).collect()
                    } else if source_finie {
                        if !queue_ajoutee {
                            // Le silence de queue, plus de quoi compléter le
                            // dernier paquet entamé.
                            let reste = (TRAMES_PAR_PAQUET - tampon.len() % TRAMES_PAR_PAQUET)
                                % TRAMES_PAR_PAQUET;
                            let ajout = silence_de_queue as usize + reste;
                            tampon.extend(std::iter::repeat_n([0, 0], ajout));
                            queue_ajoutee = true;
                            continue;
                        }
                        position_finale = Some(position);
                        fin_envoyee_a = Some(Instant::now());
                        verrou(&etat).tout_envoye = true;
                        debug!(flux = p.flux, position, "devialet_air_tout_envoye");
                        break;
                    } else {
                        // Le décodeur est en retard : l'ampli a 2 s de
                        // tampon, on attend la période suivante plutôt que
                        // d'inventer du silence au milieu de la musique.
                        break;
                    };
                    let entete = EnteteAudio {
                        session: p.session,
                        flux: p.flux,
                        frequence: f,
                        lecture: position >= seuil_de_lecture,
                        position,
                    };
                    let octets = paquet_audio(&entete, &trames);
                    if let Err(e) = socket.send_to(&octets, ampli).await {
                        return FinDeFlux::interrompu(format!("envoi vers {ampli} : {e}"));
                    }
                    premier_envoi.get_or_insert_with(Instant::now);
                    anneau.push_back(PaquetGarde { position, octets, renvoye_a: None });
                    position += TRAMES_PAR_PAQUET as u64;
                    let mut e = verrou(&etat);
                    e.position_envoyee = position;
                    e.paquets_envoyes += 1;
                }
                // Les paquets déjà joués ne seront plus réclamés.
                while anneau
                    .front()
                    .is_some_and(|g| g.position + (TRAMES_PAR_PAQUET as u64) <= jouee)
                {
                    anneau.pop_front();
                }
                if dernier_retour.is_none()
                    && premier_envoi.is_some_and(|t| t.elapsed() > c.delai_premier_retour)
                {
                    warn!(flux = p.flux, %ampli, "devialet_air_aucun_retour");
                    return FinDeFlux::interrompu(
                        "l'ampli ne répond pas au flux (aucun retour Df) : \
                         est-il allumé, sur la source AIR, et la session est-elle la bonne ?",
                    );
                }
                if dernier_retour.is_some_and(|t| t.elapsed() > c.delai_retour_perdu) {
                    return FinDeFlux::interrompu("l'ampli ne répond plus (retours Df interrompus)");
                }
                if let (Some(fin), Some(depuis)) = (position_finale, fin_envoyee_a) {
                    if jouee + TRAMES_PAR_PAQUET as u64 >= fin {
                        info!(flux = p.flux, "devialet_air_fin_naturelle");
                        return FinDeFlux::Naturelle;
                    }
                    if depuis.elapsed() > c.avance_cible + c.vidage_max {
                        warn!(flux = p.flux, jouee, fin, "devialet_air_vidage_incomplet");
                        return FinDeFlux::Naturelle;
                    }
                }
            }
            recu = socket.recv_from(&mut tampon_reception) => {
                let Ok((n, de)) = recu else { continue };
                if de.ip() != ampli.ip() {
                    continue;
                }
                let Ok(df) = analyser_retour(&tampon_reception[..n]) else { continue };
                if df.session != p.session || df.flux != p.flux {
                    continue;
                }
                dernier_retour = Some(Instant::now());
                if df.position_jouee != jouee {
                    jouee = df.position_jouee;
                    jouee_lue_a = Instant::now();
                }
                {
                    let mut e = verrou(&etat);
                    e.position_jouee = jouee;
                    e.retours_recus += 1;
                }
                for reclamee in df.positions_reclamees() {
                    let Some(g) = anneau.iter_mut().find(|g| g.position == reclamee) else {
                        continue;
                    };
                    if g.renvoye_a.is_some_and(|t| t.elapsed() < c.intervalle_de_renvoi) {
                        continue;
                    }
                    if socket.send_to(&g.octets, ampli).await.is_ok() {
                        g.renvoye_a = Some(Instant::now());
                        verrou(&etat).paquets_renvoyes += 1;
                    }
                }
            }
        }
    }
}

fn verrou<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
