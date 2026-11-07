//! Un ampli AIR simulé, sur la boucle locale, pour les tests.
//!
//! Il fait ce que les captures montrent d'un Expert : il annonce son état
//! toutes les 200 ms (ici toutes les 20 ms, vers un port de test), applique
//! les commandes `Dr`, décode et vérifie chaque paquet `Dm`, commence à
//! « jouer » au premier paquet marqué lecture, au rythme réel, et renvoie un
//! `Df` toutes les 50 ms (ici 10 ms) avec sa position jouée et ses plages
//! manquantes. Il peut perdre des paquets à la première réception, ou se
//! taire, pour exercer les retransmissions et les délais.
//!
//! Il n'imite que ce que la spécification décrit : c'est un témoin de
//! conformité à la spécification, pas une preuve de ce que fait un ampli.

use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use super::protocole::{
    self, Annonce, CommandeLue, EnteteAudio, PaquetLu, Retour, SourceAnnoncee, TRAMES_PAR_PAQUET,
    analyser_commande, analyser_paquet, opcode,
};

pub const MAC_DE_TEST: [u8; 6] = [0x02, 0x00, 0x5e, 0x00, 0x00, 0x01];
pub const SESSION_DE_TEST: [u8; 4] = [0x5a, 0x17, 0xc0, 0xde];

/// Une annonce plausible : allumé, source 9 « AIR », −40 dB.
pub fn annonce_de_test() -> Annonce {
    let mut sources: Vec<SourceAnnoncee> = (0..15)
        .map(|i| SourceAnnoncee {
            indice: i,
            etat: b'0',
            nom: String::new(),
        })
        .collect();
    sources[6] = SourceAnnoncee {
        indice: 6,
        etat: b'1',
        nom: "TV".into(),
    };
    sources[9] = SourceAnnoncee {
        indice: 9,
        etat: b'4',
        nom: "AIR".into(),
    };
    Annonce {
        compteur: 0,
        flux_en_cours: 0,
        mac: MAC_DE_TEST,
        nom: "EXPERT-TEST".into(),
        sources,
        allume: true,
        tonalite_active: true,
        aucun_flux_air: true,
        source_choisie: 9,
        sourdine: false,
        volume_demi_db: -80,
        aigus_demi_db: 0,
        graves_demi_db: 6,
        balance_pour_cent: 0,
        pre_out: 0,
        frequence_entree: None,
        horloge_ms: 0,
        session: SESSION_DE_TEST,
    }
}

/// Un paquet audio reçu : son en-tête, ses trames, ses deux octets de fin.
pub type PaquetRecu = (EnteteAudio, Vec<[i32; 2]>, [u8; 2]);

#[derive(Default)]
pub struct EtatSimule {
    pub annonce: Option<Annonce>,
    /// Paquets audio reçus et valides, par flux puis par position.
    pub paquets: BTreeMap<u32, BTreeMap<u64, PaquetRecu>>,
    pub arrets: Vec<u32>,
    pub commandes: Vec<CommandeLue>,
    /// Datagrammes refusés (CRC faux, format incohérent).
    pub refus: Vec<String>,
    /// Positions à perdre à leur PREMIÈRE réception.
    pub perdre_une_fois: HashSet<u64>,
    pub perdus: HashSet<u64>,
    /// Ne renvoie aucun `Df`.
    pub muet: bool,
    /// Délai d'allumage simulé.
    pub delai_allumage: Duration,
    allumage_demande: Option<Instant>,
    flux_courant: Option<u32>,
    client: Option<SocketAddr>,
    lecture_depuis: Option<Instant>,
    frequence: u32,
}

impl EtatSimule {
    /// Le PCM reçu d'un flux, dans l'ordre des positions ; `None` si un
    /// paquet manque ou chevauche.
    pub fn pcm(&self, flux: u32) -> Option<Vec<[i32; 2]>> {
        let paquets = self.paquets.get(&flux)?;
        let mut attendu = 0u64;
        let mut v = Vec::new();
        for (&pos, (_, trames, _)) in paquets {
            if pos != attendu {
                return None;
            }
            v.extend_from_slice(trames);
            attendu += trames.len() as u64;
        }
        Some(v)
    }

    fn fin_contigue(&self, flux: u32) -> u64 {
        let mut attendu = 0u64;
        if let Some(p) = self.paquets.get(&flux) {
            for (&pos, (_, t, _)) in p {
                if pos != attendu {
                    break;
                }
                attendu += t.len() as u64;
            }
        }
        attendu
    }
}

pub struct Simulateur {
    pub adresse: SocketAddr,
    pub etat: Arc<Mutex<EtatSimule>>,
    taches: Vec<JoinHandle<()>>,
}

impl Drop for Simulateur {
    fn drop(&mut self) {
        for t in &self.taches {
            t.abort();
        }
    }
}

fn verrou(e: &Mutex<EtatSimule>) -> std::sync::MutexGuard<'_, EtatSimule> {
    e.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Simulateur {
    /// Démarre un ampli simulé. `annonces_vers` : où diffuser ses annonces
    /// (le port d'écoute du test), ou `None`.
    pub async fn demarrer(annonce: Annonce, annonces_vers: Option<SocketAddr>) -> Self {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let adresse = socket.local_addr().unwrap();
        let etat = Arc::new(Mutex::new(EtatSimule {
            annonce: Some(annonce),
            ..EtatSimule::default()
        }));
        let mut taches = Vec::new();
        // Réception : commandes et audio.
        {
            let socket = socket.clone();
            let etat = etat.clone();
            taches.push(tokio::spawn(async move {
                let mut b = vec![0u8; 4096];
                loop {
                    let Ok((n, de)) = socket.recv_from(&mut b).await else {
                        continue;
                    };
                    recevoir(&etat, &b[..n], de);
                }
            }));
        }
        // Retours Df toutes les 10 ms, annonces toutes les 20 ms.
        {
            let socket = socket.clone();
            let etat = etat.clone();
            taches.push(tokio::spawn(async move {
                let mut n: u64 = 0;
                loop {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    n += 1;
                    let (df, da) = {
                        let mut e = verrou(&etat);
                        avancer(&mut e);
                        let df = retour_a_envoyer(&e);
                        let da = n
                            .is_multiple_of(2)
                            .then(|| e.annonce.as_ref().map(protocole::annonce))
                            .flatten();
                        (df, da)
                    };
                    if let Some((octets, vers)) = df {
                        let _ = socket.send_to(&octets, vers).await;
                    }
                    if let (Some(da), Some(vers)) = (da, annonces_vers) {
                        let _ = socket.send_to(&da, vers).await;
                    }
                }
            }));
        }
        Self {
            adresse,
            etat,
            taches,
        }
    }

    pub fn etat(&self) -> std::sync::MutexGuard<'_, EtatSimule> {
        verrou(&self.etat)
    }
}

fn avancer(e: &mut EtatSimule) {
    if let Some(t) = e.allumage_demande
        && t.elapsed() >= e.delai_allumage
    {
        e.allumage_demande = None;
        if let Some(a) = e.annonce.as_mut() {
            a.allume = true;
        }
    }
    if let Some(a) = e.annonce.as_mut() {
        a.compteur = a.compteur.wrapping_add(1);
    }
}

fn position_jouee(e: &EtatSimule, flux: u32) -> u64 {
    let Some(depuis) = e.lecture_depuis else {
        return 0;
    };
    let ecoule = (depuis.elapsed().as_secs_f64() * f64::from(e.frequence)) as u64;
    ecoule.min(e.fin_contigue(flux))
}

fn retour_a_envoyer(e: &EtatSimule) -> Option<(Vec<u8>, SocketAddr)> {
    if e.muet {
        return None;
    }
    let flux = e.flux_courant?;
    let client = e.client?;
    let paquets = e.paquets.get(&flux)?;
    let jouee = position_jouee(e, flux);
    let base = jouee / TRAMES_PAR_PAQUET as u64;
    let plus_haut = paquets.keys().next_back().copied().unwrap_or(0) / TRAMES_PAR_PAQUET as u64;
    let mut manquants = Vec::new();
    let mut k = base;
    while k < plus_haut {
        if !paquets.contains_key(&(k * TRAMES_PAR_PAQUET as u64)) {
            let debut = k;
            while k < plus_haut && !paquets.contains_key(&(k * TRAMES_PAR_PAQUET as u64)) {
                k += 1;
            }
            manquants.push(((debut - base) as u16, (k - 1 - base) as u16));
        }
        k += 1;
    }
    manquants.truncate(32);
    let derniere = e.fin_contigue(flux).saturating_sub(1);
    let r = Retour {
        session: e.annonce.as_ref()?.session,
        flux,
        derniere_trame_recue: derniere,
        dernier_paquet_recu: paquets.keys().next_back().copied().unwrap_or(0),
        position_jouee: jouee,
        manquants,
    };
    Some((protocole::retour(&r), client))
}

fn recevoir(etat: &Mutex<EtatSimule>, o: &[u8], de: SocketAddr) {
    let mut e = verrou(etat);
    if o.starts_with(b"Dr") {
        match analyser_commande(o) {
            Ok(c) => {
                appliquer(&mut e, &c);
                e.commandes.push(c);
            }
            Err(r) => e.refus.push(format!("Dr : {r}")),
        }
        return;
    }
    if !o.starts_with(b"Dm") {
        e.refus.push("datagramme inconnu".into());
        return;
    }
    match analyser_paquet(o) {
        Ok(PaquetLu::Arret { flux, .. }) => {
            e.arrets.push(flux);
            e.flux_courant = None;
            e.lecture_depuis = None;
            if let Some(a) = e.annonce.as_mut() {
                a.flux_en_cours = 0;
                a.aucun_flux_air = true;
            }
        }
        Ok(PaquetLu::Audio {
            entete,
            trames,
            decalages,
        }) => {
            // Mauvaise session : l'ampli ignore le flux (hypothèse Q1).
            if e.annonce.as_ref().map(|a| a.session) != Some(entete.session) {
                e.refus.push("session étrangère".into());
                return;
            }
            if e.perdre_une_fois.remove(&entete.position) {
                e.perdus.insert(entete.position);
                return;
            }
            if e.flux_courant != Some(entete.flux) {
                e.flux_courant = Some(entete.flux);
                e.lecture_depuis = None;
            }
            e.client = Some(de);
            e.frequence = entete.frequence;
            if entete.lecture && e.lecture_depuis.is_none() {
                e.lecture_depuis = Some(Instant::now());
                if let Some(a) = e.annonce.as_mut() {
                    a.flux_en_cours = (entete.flux & 0xFF) as u8;
                    a.aucun_flux_air = false;
                }
            }
            e.paquets
                .entry(entete.flux)
                .or_default()
                .insert(entete.position, (entete, trames, decalages));
        }
        Err(r) => e.refus.push(format!("Dm : {r}")),
    }
}

fn appliquer(e: &mut EtatSimule, c: &CommandeLue) {
    // Roon envoie chaque commande deux fois : la seconde est sans effet.
    if e.commandes
        .last()
        .is_some_and(|d| d.compteur_commande == c.compteur_commande && d.opcode == c.opcode)
    {
        return;
    }
    if c.opcode == opcode::ALLUMER {
        e.allumage_demande = Some(Instant::now());
        return;
    }
    let Some(a) = e.annonce.as_mut() else { return };
    match c.opcode {
        opcode::VOLUME => a.volume_demi_db = (c.valeur * 2.0).round() as i16,
        opcode::SOURDINE => a.sourdine = true,
        opcode::FIN_DE_SOURDINE => a.sourdine = false,
        opcode::VEILLE => a.allume = false,
        opcode::SOURCE => a.source_choisie = c.valeur as u8,
        _ => {}
    }
}
