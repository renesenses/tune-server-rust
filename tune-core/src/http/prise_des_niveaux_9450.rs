//! #9450 — la prise des VU-mètres sur une session mandataire.
//!
//! Une session mandataire (Qobuz/TIDAL relayés tels quels) ne décode rien :
//! pour animer les VU-mètres, la sonde de niveaux (#1106) ouvrait sa PROPRE
//! connexion sur `/stream/<id>`, et le mandataire ouvrait pour elle une
//! seconde connexion vers le CDN. Chaque piste était donc tirée deux fois en
//! entier depuis l'amont, la connexion de la sonde partant avant même le
//! `SetAVTransportURI` du renderer (journal rc13.1, fil 3033 : un `GET` sans
//! `Range` ni `User-Agent` ~1 ms après `initial_prebuffer_done`).
//!
//! La prise remplace cette seconde connexion : le corps mandataire qui sert
//! le renderer y recopie les octets qu'il relaie, dans l'ordre du fichier, et
//! la sonde les décode de là. Une seule connexion amont par piste.
//!
//! Règles :
//! - la prise ne freine JAMAIS le renderer : l'envoi ne bloque pas, et une
//!   sonde trop en retard ([`PLAFOND_EN_ATTENTE`]) est simplement débranchée
//!   (les aiguilles s'arrêtent, le son continue) ;
//! - seuls des octets CONTIGUS passent : un bloc déjà vu est écarté, un bloc
//!   qui commence au-delà du prochain octet attendu est ignoré (sauts de
//!   plage d'un renderer qui lit l'en-tête de fin) ; une connexion qui
//!   reprend la suite le nourrit de nouveau ;
//! - la prise se ferme à la fin connue du fichier, au retrait de la session,
//!   ou quand la sonde s'en va.

use std::io::{self, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// Octets que la sonde peut laisser en attente avant d'être débranchée.
///
/// La sonde se bride à 30 s d'avance sur la position (#1106) ; un renderer qui
/// tire très loin devant (piste entière en mémoire) accumulerait ici ce
/// qu'elle ne lit pas encore. Au-delà, on la débranche plutôt que de garder
/// la piste en mémoire ou de freiner le renderer.
pub const PLAFOND_EN_ATTENTE: u64 = 64 * 1024 * 1024;

struct Branchement {
    tx: mpsc::Sender<Vec<u8>>,
    /// Octet absolu du fichier que la sonde attend ensuite.
    prochain: u64,
    en_attente: Arc<AtomicU64>,
}

/// La prise d'une session ; vide tant qu'aucune sonde n'y est branchée.
#[derive(Default)]
pub struct PriseDesNiveaux {
    branchement: std::sync::Mutex<Option<Branchement>>,
}

impl PriseDesNiveaux {
    /// Branche une sonde, en remplaçant celle qui y était. Le lecteur rendu
    /// lit le fichier depuis l'octet 0.
    pub fn brancher(&self, arret: Box<dyn Fn() -> bool + Send + Sync>) -> LecteurDePrise {
        let (tx, rx) = mpsc::channel();
        let en_attente = Arc::new(AtomicU64::new(0));
        *self.branchement.lock().unwrap_or_else(|e| e.into_inner()) = Some(Branchement {
            tx,
            prochain: 0,
            en_attente: en_attente.clone(),
        });
        LecteurDePrise {
            rx: std::sync::Mutex::new(rx),
            en_attente,
            reste: Vec::new(),
            lu: 0,
            arret,
        }
    }

    /// Une sonde est-elle branchée ?
    pub fn branchee(&self) -> bool {
        self.branchement
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Ferme la prise : la sonde lit la fin du flux.
    pub fn couper(&self) {
        self.branchement
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }

    /// Recopie un bloc relayé, qui commence à l'octet absolu `debut`. `total` :
    /// la longueur du fichier quand elle est connue (la prise se ferme une
    /// fois le dernier octet passé).
    pub fn nourrir(&self, debut: u64, octets: &[u8], total: Option<u64>) {
        let mut garde = self.branchement.lock().unwrap_or_else(|e| e.into_inner());
        let Some(b) = garde.as_mut() else {
            return;
        };
        let fin = debut + octets.len() as u64;
        if debut > b.prochain || fin <= b.prochain {
            return;
        }
        let neuf = &octets[(b.prochain - debut) as usize..];
        let attente =
            b.en_attente.fetch_add(neuf.len() as u64, Ordering::Relaxed) + neuf.len() as u64;
        if attente > PLAFOND_EN_ATTENTE {
            tracing::info!(
                en_attente = attente,
                "prise_des_niveaux_debranchee — sonde trop en retard, le renderer n'est pas freiné"
            );
            garde.take();
            return;
        }
        if b.tx.send(neuf.to_vec()).is_err() {
            garde.take();
            return;
        }
        b.prochain = fin;
        if total.is_some_and(|t| fin >= t) {
            garde.take();
        }
    }
}

/// Ce que la sonde lit : les octets relayés, dans l'ordre du fichier.
pub struct LecteurDePrise {
    /// Sous verrou : la source d'un décodeur symphonia doit être `Sync`.
    rx: std::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
    en_attente: Arc<AtomicU64>,
    reste: Vec<u8>,
    lu: usize,
    /// Vrai quand la sonde n'a plus de raison de lire (forwarder parti).
    arret: Box<dyn Fn() -> bool + Send + Sync>,
}

impl LecteurDePrise {
    /// Pose ce qui arrête la lecture : pour une prise garée (avance sans
    /// blanc), le forwarder n'existe qu'à l'adoption de la piste.
    pub fn poser_l_arret(&mut self, arret: Box<dyn Fn() -> bool + Send + Sync>) {
        self.arret = arret;
    }
}

/// Prises branchées sur une session PRÉ-ARMÉE (piste suivante d'une avance
/// sans blanc) : le renderer en tire le début avant l'avance, alors que la
/// sonde ne démarre qu'à l'avance. Le lecteur attend ici, par `stream_id`.
static GAREES: std::sync::LazyLock<std::sync::Mutex<Vec<(String, LecteurDePrise)>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(Vec::new()));

/// Au plus autant de prises garées : une piste préparée puis abandonnée ne
/// garde pas ses octets indéfiniment.
const GAREES_AU_PLUS: usize = 4;

/// Gare le lecteur d'une session pré-armée.
pub fn garer(stream_id: &str, lecteur: LecteurDePrise) {
    let mut g = GAREES.lock().unwrap_or_else(|e| e.into_inner());
    g.retain(|(s, _)| s != stream_id);
    g.push((stream_id.to_string(), lecteur));
    let trop = g.len().saturating_sub(GAREES_AU_PLUS);
    g.drain(..trop);
}

/// Reprend le lecteur garé de cette session, s'il y en a un.
pub fn reprendre(stream_id: &str) -> Option<LecteurDePrise> {
    let mut g = GAREES.lock().unwrap_or_else(|e| e.into_inner());
    let i = g.iter().position(|(s, _)| s == stream_id)?;
    Some(g.remove(i).1)
}

/// Oublie le lecteur garé d'une session retirée.
pub fn oublier(stream_id: &str) {
    GAREES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(s, _)| s != stream_id);
}

impl Read for LecteurDePrise {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.lu >= self.reste.len() {
            let recu = self
                .rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .recv_timeout(Duration::from_millis(250));
            match recu {
                Ok(bloc) => {
                    self.en_attente
                        .fetch_sub(bloc.len() as u64, Ordering::Relaxed);
                    self.reste = bloc;
                    self.lu = 0;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if (self.arret)() {
                        return Ok(0);
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(0),
            }
        }
        let n = (self.reste.len() - self.lu).min(buf.len());
        buf[..n].copy_from_slice(&self.reste[self.lu..self.lu + n]);
        self.lu += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tout_lire(mut l: LecteurDePrise) -> Vec<u8> {
        let mut v = Vec::new();
        l.read_to_end(&mut v).unwrap();
        v
    }

    #[test]
    fn les_octets_contigus_passent_une_fois_dans_l_ordre() {
        let prise = PriseDesNiveaux::default();
        let l = prise.brancher(Box::new(|| false));
        let fichier: Vec<u8> = (0..100u8).collect();
        prise.nourrir(0, &fichier[..40], Some(100));
        // Une seconde connexion rejoue le début, puis prend la suite.
        prise.nourrir(0, &fichier[..60], Some(100));
        // Un saut vers la fin (en-tête de fin lu par un renderer) est ignoré.
        prise.nourrir(90, &fichier[90..], Some(100));
        prise.nourrir(60, &fichier[60..], Some(100));
        assert!(!prise.branchee(), "fermée au dernier octet connu");
        assert_eq!(tout_lire(l), fichier);
    }

    #[test]
    fn une_sonde_trop_en_retard_est_debranchee_sans_rien_bloquer() {
        let prise = PriseDesNiveaux::default();
        let _l = prise.brancher(Box::new(|| false));
        let bloc = vec![0u8; 1024 * 1024];
        let mut pos = 0u64;
        for _ in 0..(PLAFOND_EN_ATTENTE / bloc.len() as u64 + 2) {
            prise.nourrir(pos, &bloc, None);
            pos += bloc.len() as u64;
        }
        assert!(!prise.branchee());
    }

    /// Avance sans blanc : la prise branchée à la préparation garde ce que le
    /// renderer a tiré AVANT l'avance, et la sonde le lit en entier ensuite.
    #[test]
    fn une_prise_garee_rend_le_debut_tire_avant_l_avance() {
        let prise = PriseDesNiveaux::default();
        garer("s-garee-9450", prise.brancher(Box::new(|| false)));
        prise.nourrir(0, &[1, 2, 3], Some(6));
        let mut l = reprendre("s-garee-9450").expect("garée");
        assert!(
            reprendre("s-garee-9450").is_none(),
            "reprise une seule fois"
        );
        l.poser_l_arret(Box::new(|| false));
        prise.nourrir(3, &[4, 5, 6], Some(6));
        assert_eq!(tout_lire(l), vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn la_sonde_qui_s_en_va_rend_la_fin_du_flux() {
        let prise = PriseDesNiveaux::default();
        let l = prise.brancher(Box::new(|| true));
        assert!(tout_lire(l).is_empty());
    }
}
