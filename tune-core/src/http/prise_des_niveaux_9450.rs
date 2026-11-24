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
//! le renderer y recopie les octets qu'il relaie, et la sonde les décode de
//! là. Une seule connexion amont par piste.
//!
//! Règles :
//! - la prise ne freine JAMAIS le renderer : l'envoi ne bloque pas ; une
//!   sonde trop en retard est débranchée (les aiguilles s'arrêtent, le son
//!   continue). Le plafond suit la DURÉE : ~45 s au débit de la piste, borné
//!   entre 8 et 32 Mio, et un budget commun à toutes les prises (pistes
//!   garées comprises) borne la mémoire d'une petite machine à plusieurs
//!   zones ;
//! - la prise garde l'EN-TÊTE du fichier (jusqu'à [`ENTETE_MAX`]) : une sonde
//!   qui se recale (saut du renderer, branchement tardif) redécode depuis
//!   « en-tête + nouvelle position » ;
//! - une connexion NOUVELLE qui commence loin de la position suivie est un
//!   saut (recherche par octets du renderer) : la sonde se recale. Une lecture
//!   de la fin du fichier (étiquettes) est ignorée ; une reconnexion proche
//!   reprend la suite ;
//! - la prise se ferme à la fin connue du fichier, au retrait de la session,
//!   ou quand la sonde s'en va.

use std::io::{self, Read};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Octets d'en-tête gardés pour se recaler (métadonnées FLAC, pochette
/// incluse le plus souvent).
pub const ENTETE_MAX: usize = 1024 * 1024;
/// Durée de piste qu'une sonde peut laisser en attente.
pub const ATTENTE_MAX_S: u64 = 45;
/// Bornes du plafond par prise.
pub const PLAFOND_MIN: u64 = 8 * 1024 * 1024;
pub const PLAFOND_MAX: u64 = 32 * 1024 * 1024;
/// Budget commun à toutes les prises (attente + en-têtes), pistes garées
/// comprises : quatre zones à plein plafond.
pub const BUDGET_COMMUN: u64 = 4 * PLAFOND_MAX;
/// Une connexion nouvelle qui repart en ARRIÈRE de moins de cet écart est une
/// reconnexion (le renderer n'a pas reçu ce que les tampons du noyau
/// tenaient encore) : elle reprend la suite. Au-delà, c'est un saut arrière.
const ECART_ARRIERE_DE_REPRISE: u64 = 8 * 1024 * 1024;
/// Une connexion qui commence dans cette fin de fichier lit des étiquettes.
const QUEUE_DE_FICHIER: u64 = 128 * 1024;

static UTILISE: AtomicU64 = AtomicU64::new(0);

/// Octets tenus par toutes les prises (attente + en-têtes).
pub fn budget_utilise() -> u64 {
    UTILISE.load(Ordering::Relaxed)
}

/// Le plafond d'attente d'une piste qui coule à `octets_par_s`.
pub fn plafond_pour(octets_par_s: u64) -> u64 {
    octets_par_s
        .saturating_mul(ATTENTE_MAX_S)
        .clamp(PLAFOND_MIN, PLAFOND_MAX)
}

enum Message {
    Octets(Vec<u8>),
    /// Le flux reprend à cet octet absolu du fichier.
    Saut(u64),
}

struct Branchement {
    tx: mpsc::Sender<Message>,
    /// Octet absolu que la sonde attend ensuite.
    prochain: u64,
    /// La connexion du renderer que la sonde suit.
    connexion: Option<u64>,
    en_attente: Arc<AtomicU64>,
}

#[derive(Default)]
struct Etat {
    entete: Arc<Mutex<Vec<u8>>>,
    /// Fin du dernier bloc relayé (toutes connexions).
    derniere_fin: u64,
    total: Option<u64>,
    /// La même longueur, lisible par le lecteur (0 = inconnue).
    total_partage: Arc<AtomicU64>,
    branchement: Option<Branchement>,
}

/// La prise d'une session.
pub struct PriseDesNiveaux {
    etat: Mutex<Etat>,
    plafond: AtomicU64,
}

impl Default for PriseDesNiveaux {
    fn default() -> Self {
        Self {
            etat: Mutex::new(Etat::default()),
            plafond: AtomicU64::new(PLAFOND_MAX),
        }
    }
}

impl Drop for PriseDesNiveaux {
    fn drop(&mut self) {
        let e = self.etat.get_mut().unwrap_or_else(|e| e.into_inner());
        let n = e.entete.lock().map(|v| v.len() as u64).unwrap_or(0);
        // L'attente, elle, est rendue par le lecteur (lecture ou `Drop`).
        UTILISE.fetch_sub(n.min(UTILISE.load(Ordering::Relaxed)), Ordering::Relaxed);
    }
}

impl PriseDesNiveaux {
    /// Le débit de la piste, pour le plafond d'attente (`octets_par_s`).
    pub fn poser_le_debit(&self, octets_par_s: u64) {
        self.plafond
            .store(plafond_pour(octets_par_s), Ordering::Relaxed);
    }

    /// Branche une sonde, en remplaçant celle qui y était. Si des octets ont
    /// déjà été relayés, le lecteur commence par un saut à la position
    /// courante (en-tête + suite) au lieu de repartir de 0.
    pub fn brancher(&self, arret: Box<dyn Fn() -> bool + Send + Sync>) -> LecteurDePrise {
        let (tx, rx) = mpsc::channel();
        let en_attente = Arc::new(AtomicU64::new(0));
        let mut e = self.etat.lock().unwrap_or_else(|e| e.into_inner());
        let depart = e.derniere_fin;
        e.branchement = Some(Branchement {
            tx,
            prochain: depart,
            connexion: None,
            en_attente: en_attente.clone(),
        });
        LecteurDePrise {
            rx: Mutex::new(rx),
            en_attente,
            entete: e.entete.clone(),
            total: e.total_partage.clone(),
            reste: Vec::new(),
            lu: 0,
            saut: (depart > 0).then_some(depart),
            a_ignorer: 0,
            fini: false,
            arret,
        }
    }

    /// Une sonde est-elle branchée ?
    pub fn branchee(&self) -> bool {
        self.etat
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .branchement
            .is_some()
    }

    /// Ferme la prise : la sonde lit la fin du flux.
    pub fn couper(&self) {
        self.etat
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .branchement
            .take();
    }

    /// Recopie un bloc relayé par la connexion `connexion`, qui commence à
    /// l'octet absolu `debut`. `total` : la longueur du fichier si connue.
    pub fn nourrir(&self, connexion: u64, debut: u64, octets: &[u8], total: Option<u64>) {
        let mut e = self.etat.lock().unwrap_or_else(|e| e.into_inner());
        let fin = debut + octets.len() as u64;
        if let Some(t) = total {
            e.total = Some(t);
            e.total_partage.store(t, Ordering::Relaxed);
        }
        // L'en-tête, contigu depuis 0.
        {
            let mut entete = e.entete.lock().unwrap_or_else(|e| e.into_inner());
            let tenu = entete.len() as u64;
            if debut <= tenu && fin > tenu && entete.len() < ENTETE_MAX {
                let neuf = &octets[(tenu - debut) as usize..];
                let n = neuf.len().min(ENTETE_MAX - entete.len());
                entete.extend_from_slice(&neuf[..n]);
                UTILISE.fetch_add(n as u64, Ordering::Relaxed);
            }
        }
        e.derniere_fin = fin;
        let total = e.total;
        let plafond = self.plafond.load(Ordering::Relaxed);
        let Some(b) = e.branchement.as_mut() else {
            return;
        };
        let nouvelle = b.connexion != Some(connexion);
        if nouvelle {
            let en_queue = total.is_some_and(|t| debut + QUEUE_DE_FICHIER >= t && debut > 0);
            if en_queue && debut > b.prochain {
                // Lecture des étiquettes de fin : rien à décoder.
                return;
            }
            // En avant, le renderer n'a jamais reçu ces octets : c'est un saut.
            // En arrière, seulement au-delà de l'écart d'une reconnexion.
            let saut = debut > b.prochain || debut + ECART_ARRIERE_DE_REPRISE < b.prochain;
            if saut {
                // Un saut : la sonde se recale à `debut`.
                if b.tx.send(Message::Saut(debut)).is_err() {
                    e.branchement.take();
                    return;
                }
                b.prochain = debut;
            }
            if debut <= b.prochain {
                b.connexion = Some(connexion);
            }
        }
        if b.connexion != Some(connexion) || debut > b.prochain || fin <= b.prochain {
            return;
        }
        let neuf = &octets[(b.prochain - debut) as usize..];
        let n = neuf.len() as u64;
        let attente = b.en_attente.fetch_add(n, Ordering::Relaxed) + n;
        let commun = UTILISE.fetch_add(n, Ordering::Relaxed) + n;
        if attente > plafond || commun > BUDGET_COMMUN {
            tracing::info!(
                en_attente = attente,
                plafond,
                budget_commun = commun,
                "prise_des_niveaux_debranchee — sonde trop en retard, le renderer n'est pas freiné"
            );
            UTILISE.fetch_sub(n, Ordering::Relaxed);
            b.en_attente.fetch_sub(n, Ordering::Relaxed);
            e.branchement.take();
            return;
        }
        if b.tx.send(Message::Octets(neuf.to_vec())).is_err() {
            UTILISE.fetch_sub(n, Ordering::Relaxed);
            b.en_attente.fetch_sub(n, Ordering::Relaxed);
            e.branchement.take();
            return;
        }
        b.prochain = fin;
        if total.is_some_and(|t| fin >= t) {
            e.branchement.take();
        }
    }
}

/// Ce que la sonde lit : les octets relayés, dans l'ordre du fichier. Un
/// saut termine la lecture en cours (fin de flux pour le décodeur) ; la sonde
/// le prend ([`Self::prendre_le_saut`]) et redécode depuis l'en-tête.
pub struct LecteurDePrise {
    /// Sous verrou : la source d'un décodeur symphonia doit être `Sync`.
    rx: Mutex<mpsc::Receiver<Message>>,
    en_attente: Arc<AtomicU64>,
    entete: Arc<Mutex<Vec<u8>>>,
    total: Arc<AtomicU64>,
    reste: Vec<u8>,
    lu: usize,
    saut: Option<u64>,
    /// Octets à écarter en tête des blocs qui suivent un recalage
    /// (alignement sur une trame WAV).
    a_ignorer: usize,
    fini: bool,
    /// Vrai quand la sonde n'a plus de raison de lire (piste remplacée).
    arret: Box<dyn Fn() -> bool + Send + Sync>,
}

impl LecteurDePrise {
    /// Pose ce qui arrête la lecture.
    pub fn poser_l_arret(&mut self, arret: Box<dyn Fn() -> bool + Send + Sync>) {
        self.arret = arret;
    }

    /// Un saut est-il en attente ? Le rend (octet absolu) et prépare la
    /// lecture suivante : en-tête gardé, puis la suite.
    pub fn prendre_le_saut(&mut self) -> Option<u64> {
        let octet = self.saut.take()?;
        let entete = self
            .entete
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // L'en-tête SEUL (métadonnées), jamais l'audio qui le suit : rejouer
        // la première seconde daterait faux tout ce qui vient après.
        let forme = forme_de_l_en_tete(&entete);
        let n = forme.map_or(entete.len(), |f| f.longueur).min(entete.len());
        self.reste = entete[..n].to_vec();
        // Un WAV reprend sur une trame entière.
        self.a_ignorer = forme.and_then(|f| f.trame).map_or(0, |t| {
            let dans_les_donnees = octet.saturating_sub(n as u64);
            ((t - dans_les_donnees % t) % t) as usize
        });
        self.lu = 0;
        self.fini = false;
        Some(octet)
    }

    /// Octets d'en-tête (métadonnées) que le recalage rejoue, s'ils sont
    /// connus — pour dater la reprise.
    pub fn longueur_d_en_tete(&self) -> Option<u64> {
        let entete = self.entete.lock().unwrap_or_else(|e| e.into_inner());
        forme_de_l_en_tete(&entete).map(|f| f.longueur as u64)
    }

    /// La longueur du fichier, si l'amont l'a dite.
    pub fn total(&self) -> Option<u64> {
        Some(self.total.load(Ordering::Relaxed)).filter(|&t| t > 0)
    }

    /// La lecture commence-t-elle par un saut (branchement tardif) ?
    pub fn commence_par_un_saut(&self) -> Option<u64> {
        self.saut
    }

    fn rendre(&self, n: u64) {
        self.en_attente.fetch_sub(n, Ordering::Relaxed);
        UTILISE.fetch_sub(n.min(UTILISE.load(Ordering::Relaxed)), Ordering::Relaxed);
    }
}

impl Drop for LecteurDePrise {
    fn drop(&mut self) {
        let reste = self.en_attente.swap(0, Ordering::Relaxed);
        UTILISE.fetch_sub(
            reste.min(UTILISE.load(Ordering::Relaxed)),
            Ordering::Relaxed,
        );
    }
}

impl Read for LecteurDePrise {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.fini || (self.saut.is_some() && self.lu >= self.reste.len()) {
            return Ok(0);
        }
        while self.lu >= self.reste.len() {
            let recu = self
                .rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .recv_timeout(Duration::from_millis(250));
            match recu {
                Ok(Message::Octets(bloc)) => {
                    self.rendre(bloc.len() as u64);
                    let ecarte = self.a_ignorer.min(bloc.len());
                    self.a_ignorer -= ecarte;
                    self.reste = bloc;
                    self.lu = ecarte;
                }
                Ok(Message::Saut(octet)) => {
                    self.saut = Some(octet);
                    return Ok(0);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if (self.arret)() {
                        self.fini = true;
                        return Ok(0);
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.fini = true;
                    return Ok(0);
                }
            }
        }
        let n = (self.reste.len() - self.lu).min(buf.len());
        buf[..n].copy_from_slice(&self.reste[self.lu..self.lu + n]);
        self.lu += n;
        Ok(n)
    }
}

#[derive(Clone, Copy)]
struct FormeDEnTete {
    /// Octets de métadonnées avant le premier octet audio.
    longueur: usize,
    /// Taille d'une trame quand l'audio en a une fixe (WAV).
    trame: Option<u64>,
}

/// Où l'audio commence : fin des blocs de métadonnées d'un FLAC, début du
/// bloc `data` d'un WAV. `None` : forme inconnue ou en-tête incomplet.
fn forme_de_l_en_tete(o: &[u8]) -> Option<FormeDEnTete> {
    if o.starts_with(b"fLaC") {
        let mut i = 4usize;
        loop {
            let h = o.get(i..i + 4)?;
            let dernier = h[0] & 0x80 != 0;
            let n = (usize::from(h[1]) << 16) | (usize::from(h[2]) << 8) | usize::from(h[3]);
            i += 4 + n;
            if dernier {
                return (i <= o.len()).then_some(FormeDEnTete {
                    longueur: i,
                    trame: None,
                });
            }
        }
    }
    if o.starts_with(b"RIFF") && o.get(8..12) == Some(b"WAVE") {
        let mut i = 12usize;
        let mut trame = None;
        loop {
            let id = o.get(i..i + 4)?;
            let n = u32::from_le_bytes(o.get(i + 4..i + 8)?.try_into().ok()?) as usize;
            if id == b"fmt " {
                trame = o
                    .get(i + 20..i + 22)
                    .map(|b| u64::from(u16::from_le_bytes([b[0], b[1]])))
                    .filter(|&t| t > 0);
            }
            if id == b"data" {
                return Some(FormeDEnTete {
                    longueur: i + 8,
                    trame,
                });
            }
            i += 8 + n + (n & 1);
        }
    }
    None
}

/// Prises branchées sur une session PRÉ-ARMÉE (piste suivante d'une avance
/// sans blanc) : le renderer en tire le début avant l'avance, alors que la
/// sonde ne démarre qu'à l'avance. Le lecteur attend ici, par `stream_id`.
static GAREES: std::sync::LazyLock<Mutex<Vec<(String, LecteurDePrise)>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::new()));

/// Au plus autant de prises garées.
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

/// La session a-t-elle une prise garée ?
pub fn est_garee(stream_id: &str) -> bool {
    GAREES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|(s, _)| s == stream_id)
}

/// Oublie le lecteur garé d'une session retirée.
pub fn oublier(stream_id: &str) {
    GAREES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|(s, _)| s != stream_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un arrêt qui tombe au bout de 10 s : un test qui se bloque échoue au
    /// lieu de pendre (relecture de #9458).
    fn borne() -> Box<dyn Fn() -> bool + Send + Sync> {
        let fin = std::time::Instant::now() + std::time::Duration::from_secs(10);
        Box::new(move || std::time::Instant::now() >= fin)
    }

    fn tout_lire(l: &mut LecteurDePrise) -> Vec<u8> {
        let mut v = Vec::new();
        l.read_to_end(&mut v).unwrap();
        v
    }

    #[test]
    fn les_octets_contigus_passent_une_fois_dans_l_ordre() {
        let prise = PriseDesNiveaux::default();
        let mut l = prise.brancher(borne());
        let fichier: Vec<u8> = (0..100u8).collect();
        let total = Some(100);
        prise.nourrir(1, 0, &fichier[..40], total);
        // Une reconnexion proche rejoue le début puis prend la suite.
        prise.nourrir(2, 0, &fichier[..60], total);
        prise.nourrir(2, 60, &fichier[60..], total);
        assert!(!prise.branchee(), "fermée au dernier octet connu");
        assert_eq!(tout_lire(&mut l), fichier);
        assert!(l.prendre_le_saut().is_none());
    }

    #[test]
    fn la_lecture_des_etiquettes_de_fin_est_ignoree() {
        let prise = PriseDesNiveaux::default();
        let mut l = prise.brancher(borne());
        let total = 4 * 1024 * 1024u64;
        let bloc = vec![7u8; 1000];
        prise.nourrir(1, 0, &bloc, Some(total));
        prise.nourrir(2, total - 187, &bloc[..187], Some(total));
        prise.nourrir(3, 28, &bloc, Some(total));
        prise.couper();
        let lu = tout_lire(&mut l);
        assert_eq!(lu.len(), 1028);
        assert!(l.prendre_le_saut().is_none());
    }

    /// #9450, revue — un saut du renderer (recherche par octets) recale la
    /// sonde : en-tête gardé, puis la suite depuis la nouvelle position.
    #[test]
    fn un_saut_recale_la_sonde_depuis_l_en_tete() {
        let prise = PriseDesNiveaux::default();
        let mut l = prise.brancher(borne());
        let total = Some(10 * 1024 * 1024u64);
        prise.nourrir(1, 0, &[1, 2, 3, 4], total);
        let loin = 5 * 1024 * 1024u64;
        prise.nourrir(2, loin, &[9, 9], total);
        assert_eq!(tout_lire(&mut l), vec![1, 2, 3, 4], "fin de flux au saut");
        assert_eq!(l.prendre_le_saut(), Some(loin));
        prise.couper();
        assert_eq!(tout_lire(&mut l), vec![1, 2, 3, 4, 9, 9]);
    }

    #[test]
    fn une_sonde_trop_en_retard_est_debranchee_sans_rien_bloquer() {
        let prise = PriseDesNiveaux::default();
        prise.poser_le_debit(100_000); // 45 s = 4,5 Mo → plancher 8 Mio
        let _l = prise.brancher(borne());
        let bloc = vec![0u8; 1024 * 1024];
        let mut pos = 0u64;
        for _ in 0..(PLAFOND_MIN / bloc.len() as u64 + 2) {
            prise.nourrir(1, pos, &bloc, None);
            pos += bloc.len() as u64;
        }
        assert!(!prise.branchee());
    }

    #[test]
    fn le_plafond_suit_la_duree_borne_entre_8_et_32_mio() {
        assert_eq!(plafond_pour(10_000), PLAFOND_MIN);
        assert_eq!(plafond_pour(300_000), 300_000 * 45);
        assert_eq!(plafond_pour(1_152_000), PLAFOND_MAX);
    }

    /// Avance sans blanc : la prise garée garde le début tiré avant l'avance.
    #[test]
    fn une_prise_garee_rend_le_debut_tire_avant_l_avance() {
        let prise = PriseDesNiveaux::default();
        garer("s-garee-9450", prise.brancher(borne()));
        prise.nourrir(1, 0, &[1, 2, 3], Some(6));
        assert!(est_garee("s-garee-9450"));
        let mut l = reprendre("s-garee-9450").expect("garée");
        assert!(
            reprendre("s-garee-9450").is_none(),
            "reprise une seule fois"
        );
        prise.nourrir(1, 3, &[4, 5, 6], Some(6));
        assert_eq!(tout_lire(&mut l), vec![1, 2, 3, 4, 5, 6]);
    }

    /// Un branchement tardif ne repart pas de 0 : il se recale à la position
    /// courante, en-tête d'abord.
    #[test]
    fn un_branchement_tardif_se_recale_sans_repartir_de_zero() {
        let prise = PriseDesNiveaux::default();
        prise.nourrir(1, 0, &[1, 2, 3], None);
        let mut l = prise.brancher(borne());
        assert_eq!(l.commence_par_un_saut(), Some(3));
        assert_eq!(l.prendre_le_saut(), Some(3));
        prise.nourrir(1, 3, &[4, 5], None);
        prise.couper();
        assert_eq!(tout_lire(&mut l), vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn l_en_tete_rejoue_s_arrete_au_premier_octet_audio() {
        // FLAC : STREAMINFO (34) puis un bloc PADDING de 10, le dernier.
        let mut flac = b"fLaC".to_vec();
        flac.extend_from_slice(&[0x00, 0, 0, 34]);
        flac.extend_from_slice(&[0u8; 34]);
        flac.extend_from_slice(&[0x81, 0, 0, 10]);
        flac.extend_from_slice(&[0u8; 10]);
        flac.extend_from_slice(&[0xFF, 0xF8, 1, 2]);
        let f = forme_de_l_en_tete(&flac).unwrap();
        assert_eq!(f.longueur, 4 + 4 + 34 + 4 + 10);
        // WAV canonique : 44 octets, trame de 4.
        let mut wav = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&[1, 0, 2, 0]);
        wav.extend_from_slice(&44_100u32.to_le_bytes());
        wav.extend_from_slice(&176_400u32.to_le_bytes());
        wav.extend_from_slice(&[4, 0, 16, 0]);
        wav.extend_from_slice(b"data\0\0\0\0");
        let f = forme_de_l_en_tete(&wav).unwrap();
        assert_eq!((f.longueur, f.trame), (44, Some(4)));
        assert!(forme_de_l_en_tete(b"ID3").is_none());
    }

    #[test]
    fn la_sonde_qui_s_en_va_rend_la_fin_du_flux() {
        let prise = PriseDesNiveaux::default();
        let mut l = prise.brancher(Box::new(|| true));
        assert!(tout_lire(&mut l).is_empty());
    }
}
