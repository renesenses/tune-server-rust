//! Le disque chargé en mémoire (#6043), comme Daphile : au premier `ouvrir`
//! d'une piste, un fil de fond lit TOUT le disque vers un tampon, et la
//! lecture se sert dans ce tampon au lieu de relire le lecteur pendant
//! l'écoute.
//!
//! Ce qu'on y gagne : le lecteur se tait une fois le disque chargé, une
//! rayure se relit sans contrainte de temps réel (la règle de l'extraction,
//! `lecture_sure`, mode `doute`), et l'avance ou le retour tombent dans la
//! mémoire, sans attendre le lecteur.
//!
//! ## Ordre de chargement
//!
//! Le disque est découpé en blocs de [`SECTEURS_PAR_BLOC`] secteurs, piste
//! par piste. Le fil charge d'abord le bloc qu'un lecteur ATTEND (le plus
//! bas sur le disque s'il y en a plusieurs : la piste en cours passe avant
//! la suivante pré-armée), puis continue en avant depuis là — la fin de la
//! piste, les pistes suivantes — et revient enfin au début du disque. La
//! lecture démarre dès que ses blocs sont là.
//!
//! ## Mémoire bornée
//!
//! Le tampon d'une piste est alloué quand son premier bloc arrive. Si la
//! RAM déjà prise plus cette piste dépasse le plafond réglé, la piste va
//! dans un fichier temporaire ANONYME (supprimé par le système à sa
//! fermeture) : un Raspberry Pi à 1 Gio lit quand même tout le disque une
//! fois, sans saturer sa mémoire.
//!
//! ## Fin du chargement
//!
//! Éjection, changement de disque ou de lecteur, réglage désactivé : le
//! chargement s'arrête au prochain bloc, la mémoire et le fichier sont
//! rendus, et un flux qui attendait un bloc reçoit l'erreur d'éjection.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};

use serde::Serialize;

use crate::extraction::lecture_sure::{Bilan, Verification, lire_bloc};
use crate::lecteur::{ErreurCd, LecteurDisque};
use crate::toc::{OCTETS_PAR_SECTEUR, Toc};

/// Secteurs par bloc chargé : la taille de bloc de la lecture vers une zone.
pub const SECTEURS_PAR_BLOC: u32 = crate::flux::SECTEURS_PAR_BLOC;

pub const MIO: u64 = 1024 * 1024;
/// Ce que pèse un CD complet en 16/44 (80 min ≈ 807 Mo, 74 min ≈ 747 Mo) :
/// le repère de « la RAM disponible le permet ».
pub const OCTETS_D_UN_CD: u64 = 700 * MIO;
/// Plafond par défaut quand la RAM disponible n'est pas connue.
pub const PLAFOND_PAR_DEFAUT: u64 = 800 * MIO;
/// Plancher du plafond réglable : de quoi tenir une longue piste.
pub const PLAFOND_MINIMUM: u64 = 64 * MIO;

/// Clés de réglage (table `settings`).
pub const CLE_ACTIF: &str = "cd_charger_en_memoire";
pub const CLE_PLAFOND_MIO: &str = "cd_plafond_memoire_mio";

/// Les réglages du chargement en mémoire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Reglages {
    /// « Charger le CD en mémoire ».
    pub actif: bool,
    /// Au-delà, les pistes vont dans un fichier temporaire.
    pub plafond_octets: u64,
}

impl Reglages {
    /// Le défaut selon la RAM disponible (`None` : inconnue).
    ///
    /// Activé si un CD complet tient dans la MOITIÉ de la RAM disponible
    /// (ou si elle est inconnue) ; le plafond est cette moitié, bornée par
    /// [`PLAFOND_PAR_DEFAUT`] et [`PLAFOND_MINIMUM`].
    pub fn par_defaut(ram_disponible: Option<u64>) -> Self {
        match ram_disponible {
            None => Self {
                actif: true,
                plafond_octets: PLAFOND_PAR_DEFAUT,
            },
            Some(d) => Self {
                actif: d / 2 >= OCTETS_D_UN_CD,
                plafond_octets: (d / 2).clamp(PLAFOND_MINIMUM, PLAFOND_PAR_DEFAUT),
            },
        }
    }
}

/// La RAM disponible, si le système la dit (Linux : `MemAvailable`).
pub fn ram_disponible() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let t = std::fs::read_to_string("/proc/meminfo").ok()?;
        ram_disponible_de_meminfo(&t)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// `MemAvailable:  1234 kB` → octets.
pub fn ram_disponible_de_meminfo(meminfo: &str) -> Option<u64> {
    meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemAvailable:"))
        .and_then(|r| r.split_whitespace().next())
        .and_then(|k| k.parse::<u64>().ok())
        .map(|k| k * 1024)
}

/// Pourquoi un chargement s'est arrêté.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Fin {
    /// Tout le disque est en mémoire.
    Termine,
    /// Le disque a disparu (ou le lecteur a changé).
    Ejecte,
    /// Libéré (éjection commandée, réglage, autre disque, extraction).
    Libere,
}

enum Stockage {
    Ram(Vec<u8>),
    Fichier(std::fs::File),
}

struct Piste {
    numero: u8,
    /// Premier secteur (LBA).
    debut: u32,
    secteurs: u32,
    blocs: Vec<bool>,
    stockage: Option<Stockage>,
}

impl Piste {
    fn octets(&self) -> u64 {
        self.secteurs as u64 * OCTETS_PAR_SECTEUR as u64
    }

    /// Le bloc `b` : `(premier secteur LBA, nombre de secteurs)`.
    fn bloc(&self, b: usize) -> (u32, u32) {
        let lba = self.debut + b as u32 * SECTEURS_PAR_BLOC;
        (
            lba,
            (self.debut + self.secteurs - lba).min(SECTEURS_PAR_BLOC),
        )
    }
}

struct Etat {
    pistes: Vec<Piste>,
    /// Lecteur en attente → secteur attendu.
    attentes: HashMap<u64, u32>,
    /// Où reprendre faute d'attente : `(piste, bloc)`.
    curseur: (usize, usize),
    fin: Option<Fin>,
    octets_ram: u64,
    octets_fichier: u64,
    secteurs_charges: u64,
    secteurs_perdus: u32,
    /// La piste dont un bloc a été chargé en dernier.
    piste_en_cours: Option<u8>,
}

impl Etat {
    /// Index de piste et bloc du secteur `lba`.
    fn situer(&self, lba: u32) -> Option<(usize, usize)> {
        self.pistes.iter().enumerate().find_map(|(i, p)| {
            (lba >= p.debut && lba < p.debut + p.secteurs)
                .then(|| (i, ((lba - p.debut) / SECTEURS_PAR_BLOC) as usize))
        })
    }

    /// Le prochain bloc à charger : depuis le plus bas des secteurs
    /// attendus, sinon depuis le curseur ; en avant, puis en revenant au
    /// début du disque.
    fn prochain_bloc(&mut self) -> Option<(usize, usize)> {
        if let Some(depart) = self
            .attentes
            .values()
            .min()
            .copied()
            .and_then(|lba| self.situer(lba))
        {
            self.curseur = depart;
        }
        let (p0, b0) = self.curseur;
        let n = self.pistes.len();
        let manquant = |p: &Piste, depuis: usize| {
            p.blocs
                .iter()
                .skip(depuis)
                .position(|c| !c)
                .map(|b| b + depuis)
        };
        if n == 0 {
            return None;
        }
        // La piste du curseur d'abord, en entier : depuis le curseur, puis
        // son début.
        if let Some(b) = manquant(&self.pistes[p0], b0).or_else(|| manquant(&self.pistes[p0], 0)) {
            return Some((p0, b));
        }
        (1..n)
            .map(|k| (p0 + k) % n)
            .find_map(|i| manquant(&self.pistes[i], 0).map(|b| (i, b)))
    }

    fn rendre_la_memoire(&mut self) {
        for p in &mut self.pistes {
            p.stockage = None;
        }
        self.octets_ram = 0;
        self.octets_fichier = 0;
    }
}

/// Le chargement d'UN disque, sur un lecteur.
pub struct Chargement {
    pub disc_id: String,
    pub generation: u64,
    secteurs_total: u64,
    etat: Mutex<Etat>,
    signal: Condvar,
    arret: AtomicBool,
    fil: Mutex<Option<std::thread::JoinHandle<()>>>,
    prochain_lecteur: AtomicU64,
}

impl Chargement {
    fn new(toc: &Toc, disc_id: String, generation: u64) -> Self {
        let pistes: Vec<Piste> = toc
            .pistes_audio()
            .map(|p| {
                let secteurs = toc.secteurs(p.numero).unwrap_or(0);
                Piste {
                    numero: p.numero,
                    debut: p.debut,
                    secteurs,
                    blocs: vec![false; secteurs.div_ceil(SECTEURS_PAR_BLOC) as usize],
                    stockage: None,
                }
            })
            .collect();
        let secteurs_total = pistes.iter().map(|p| p.secteurs as u64).sum();
        Self {
            disc_id,
            generation,
            secteurs_total,
            etat: Mutex::new(Etat {
                pistes,
                attentes: HashMap::new(),
                curseur: (0, 0),
                fin: None,
                octets_ram: 0,
                octets_fichier: 0,
                secteurs_charges: 0,
                secteurs_perdus: 0,
                piste_en_cours: None,
            }),
            signal: Condvar::new(),
            arret: AtomicBool::new(false),
            fil: Mutex::new(None),
            prochain_lecteur: AtomicU64::new(1),
        }
    }

    fn verrou(&self) -> std::sync::MutexGuard<'_, Etat> {
        self.etat.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Lance le fil de chargement.
    fn demarrer(self: &Arc<Self>, lecteur: Arc<dyn LecteurDisque>, plafond: u64) {
        let c = self.clone();
        let fil = std::thread::Builder::new()
            .name("tune-cd-memoire".into())
            .spawn(move || c.charger(lecteur.as_ref(), plafond))
            .expect("fil de chargement du CD");
        *self.fil.lock().unwrap_or_else(|p| p.into_inner()) = Some(fil);
    }

    fn terminer(&self, fin: Fin) {
        let mut e = self.verrou();
        if e.fin.is_none() || fin != Fin::Termine {
            if fin != Fin::Termine {
                e.rendre_la_memoire();
            }
            e.fin = Some(fin);
        }
        drop(e);
        self.signal.notify_all();
    }

    fn charger(&self, lecteur: &dyn LecteurDisque, plafond: u64) {
        tracing::info!(disc_id = %self.disc_id, plafond, "cd_memoire_chargement_demarre");
        let mut bilan = Bilan::default();
        loop {
            if self.arret.load(Ordering::SeqCst) {
                return self.terminer(Fin::Libere);
            }
            let (i, b, lba, n) = {
                let mut e = self.verrou();
                let Some((i, b)) = e.prochain_bloc() else {
                    drop(e);
                    tracing::info!(disc_id = %self.disc_id, "cd_memoire_chargement_termine");
                    return self.terminer(Fin::Termine);
                };
                if e.pistes[i].stockage.is_none() {
                    let octets = e.pistes[i].octets();
                    let stockage = if e.octets_ram + octets <= plafond {
                        e.octets_ram += octets;
                        Stockage::Ram(vec![0u8; octets as usize])
                    } else {
                        match fichier_temporaire(octets) {
                            Ok(f) => {
                                e.octets_fichier += octets;
                                tracing::info!(
                                    piste = e.pistes[i].numero,
                                    octets,
                                    "cd_memoire_piste_sur_disque"
                                );
                                Stockage::Fichier(f)
                            }
                            Err(err) => {
                                tracing::warn!(%err, "cd_memoire_fichier_temporaire_impossible");
                                drop(e);
                                return self.terminer(Fin::Libere);
                            }
                        }
                    };
                    e.pistes[i].stockage = Some(stockage);
                }
                let (lba, n) = e.pistes[i].bloc(b);
                (i, b, lba, n)
            };
            let perdus_avant = bilan.secteurs_illisibles;
            let lu = lire_bloc(lecteur, lba, n, Verification::Doute, &mut bilan);
            if lecteur.generation_lecteur() != self.generation {
                return self.terminer(Fin::Ejecte);
            }
            let octets = match lu {
                Ok(v) => v,
                Err(ErreurCd::AucunDisque) => {
                    tracing::info!(disc_id = %self.disc_id, "cd_memoire_ejecte_pendant_le_chargement");
                    return self.terminer(Fin::Ejecte);
                }
                Err(err) => {
                    tracing::warn!(%err, "cd_memoire_lecture_impossible");
                    return self.terminer(Fin::Libere);
                }
            };
            let mut e = self.verrou();
            if e.fin.is_some() || self.arret.load(Ordering::SeqCst) {
                drop(e);
                return self.terminer(Fin::Libere);
            }
            let decalage = b as u64 * SECTEURS_PAR_BLOC as u64 * OCTETS_PAR_SECTEUR as u64;
            if let Err(err) = ecrire(e.pistes[i].stockage.as_mut(), decalage, &octets) {
                tracing::warn!(%err, "cd_memoire_ecriture_impossible");
                drop(e);
                return self.terminer(Fin::Libere);
            }
            e.pistes[i].blocs[b] = true;
            e.secteurs_charges += n as u64;
            e.secteurs_perdus += bilan.secteurs_illisibles - perdus_avant;
            e.piste_en_cours = Some(e.pistes[i].numero);
            e.curseur = (i, b + 1);
            drop(e);
            self.signal.notify_all();
        }
    }

    /// Arrête le chargement et rend la mémoire. Bloquant : attend le fil.
    pub fn liberer(&self) {
        self.arret.store(true, Ordering::SeqCst);
        self.terminer(Fin::Libere);
        let fil = self.fil.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(f) = fil
            && f.thread().id() != std::thread::current().id()
        {
            let _ = f.join();
        }
    }

    /// Attend la fin du fil (témoins).
    pub fn attendre_la_fin(&self) -> Option<Fin> {
        let mut e = self.verrou();
        while e.fin.is_none() {
            e = self.signal.wait(e).unwrap_or_else(|p| p.into_inner());
        }
        e.fin.clone()
    }

    pub fn progression(&self) -> Progression {
        let e = self.verrou();
        Progression {
            disc_id: self.disc_id.clone(),
            secteurs_charges: e.secteurs_charges,
            secteurs_total: self.secteurs_total,
            pourcentage: (e.secteurs_charges * 100)
                .checked_div(self.secteurs_total)
                .unwrap_or(100) as u8,
            pistes_chargees: e
                .pistes
                .iter()
                .filter(|p| p.blocs.iter().all(|c| *c))
                .map(|p| p.numero)
                .collect(),
            piste_en_cours: e.piste_en_cours,
            octets_ram: e.octets_ram,
            octets_fichier: e.octets_fichier,
            secteurs_perdus: e.secteurs_perdus,
            fin: e.fin.clone(),
        }
    }

    /// Un flux sur `[debut, fin)` de la piste `numero`, servi par la mémoire.
    pub fn flux(self: &Arc<Self>, debut: u32, fin: u32) -> FluxMemoire {
        FluxMemoire {
            chargement: self.clone(),
            id: self.prochain_lecteur.fetch_add(1, Ordering::SeqCst),
            prochain: debut,
            dans_secteur: 0,
            fin: fin.max(debut),
        }
    }

    /// Copie dans `buf` à partir du secteur `lba` (dans un bloc chargé), en
    /// attendant ce bloc s'il le faut. Rend le nombre d'octets copiés.
    fn lire_a(
        &self,
        id: u64,
        lba: u32,
        decalage_secteur: usize,
        buf: &mut [u8],
        fin: u32,
    ) -> std::io::Result<usize> {
        let mut e = self.verrou();
        let Some((i, b)) = e.situer(lba) else {
            return Ok(0);
        };
        loop {
            if e.pistes[i].blocs[b] {
                break;
            }
            if e.fin.is_some() {
                e.attentes.remove(&id);
                return Err(ejection());
            }
            e.attentes.insert(id, lba);
            e = self.signal.wait(e).unwrap_or_else(|p| p.into_inner());
        }
        e.attentes.remove(&id);
        if e.fin.as_ref().is_some_and(|f| *f != Fin::Termine) {
            return Err(ejection());
        }
        let p = &mut e.pistes[i];
        let (_, n) = p.bloc(b);
        let fin_bloc = (p.debut + b as u32 * SECTEURS_PAR_BLOC + n).min(fin);
        let dispo = (fin_bloc - lba) as usize * OCTETS_PAR_SECTEUR - decalage_secteur;
        let k = dispo.min(buf.len());
        let decalage = (lba - p.debut) as u64 * OCTETS_PAR_SECTEUR as u64 + decalage_secteur as u64;
        lire(p.stockage.as_mut(), decalage, &mut buf[..k])?;
        Ok(k)
    }

    fn oublier_lecteur(&self, id: u64) {
        let mut e = self.verrou();
        if e.attentes.remove(&id).is_some() {
            drop(e);
            self.signal.notify_all();
        }
    }
}

fn ejection() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotConnected, "le disque a été éjecté")
}

fn fichier_temporaire(octets: u64) -> std::io::Result<std::fs::File> {
    let f = tempfile::tempfile()?;
    f.set_len(octets)?;
    Ok(f)
}

fn ecrire(s: Option<&mut Stockage>, decalage: u64, octets: &[u8]) -> std::io::Result<()> {
    match s {
        Some(Stockage::Ram(v)) => {
            let d = decalage as usize;
            v[d..d + octets.len()].copy_from_slice(octets);
            Ok(())
        }
        Some(Stockage::Fichier(f)) => {
            f.seek(SeekFrom::Start(decalage))?;
            f.write_all(octets)
        }
        None => Err(ejection()),
    }
}

fn lire(s: Option<&mut Stockage>, decalage: u64, buf: &mut [u8]) -> std::io::Result<()> {
    match s {
        Some(Stockage::Ram(v)) => {
            let d = decalage as usize;
            buf.copy_from_slice(&v[d..d + buf.len()]);
            Ok(())
        }
        Some(Stockage::Fichier(f)) => {
            f.seek(SeekFrom::Start(decalage))?;
            f.read_exact(buf)
        }
        None => Err(ejection()),
    }
}

/// La progression du chargement, telle que `GET /etat` la rend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Progression {
    pub disc_id: String,
    pub secteurs_charges: u64,
    pub secteurs_total: u64,
    pub pourcentage: u8,
    pub pistes_chargees: Vec<u8>,
    pub piste_en_cours: Option<u8>,
    pub octets_ram: u64,
    pub octets_fichier: u64,
    pub secteurs_perdus: u32,
    pub fin: Option<Fin>,
}

/// Le PCM d'une plage de secteurs, servi par la mémoire.
pub struct FluxMemoire {
    chargement: Arc<Chargement>,
    id: u64,
    /// Secteur courant (LBA) et octets déjà rendus de ce secteur.
    prochain: u32,
    dans_secteur: usize,
    fin: u32,
}

impl FluxMemoire {
    pub fn octets_restants(&self) -> u64 {
        (self.fin - self.prochain) as u64 * OCTETS_PAR_SECTEUR as u64 - self.dans_secteur as u64
    }
}

impl Read for FluxMemoire {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.prochain >= self.fin || buf.is_empty() {
            return Ok(0);
        }
        let k = self
            .chargement
            .lire_a(self.id, self.prochain, self.dans_secteur, buf, self.fin)?;
        let total = self.dans_secteur + k;
        self.prochain += (total / OCTETS_PAR_SECTEUR) as u32;
        self.dans_secteur = total % OCTETS_PAR_SECTEUR;
        Ok(k)
    }
}

impl Drop for FluxMemoire {
    fn drop(&mut self) {
        self.chargement.oublier_lecteur(self.id);
    }
}

/// Le chargement en mémoire du greffon : ses réglages et le disque chargé.
pub struct MemoireCd {
    reglages: RwLock<Reglages>,
    courant: Mutex<Option<Arc<Chargement>>>,
    ram_disponible: Option<u64>,
}

impl MemoireCd {
    pub fn new(reglages: Reglages) -> Self {
        Self {
            reglages: RwLock::new(reglages),
            courant: Mutex::new(None),
            ram_disponible: ram_disponible(),
        }
    }

    /// Les réglages rangés, à défaut ceux que la RAM disponible permet.
    pub fn depuis_reglages_ranges(actif: Option<&str>, plafond_mio: Option<&str>) -> Self {
        let defaut = Reglages::par_defaut(ram_disponible());
        Self::new(Reglages {
            actif: actif.map(|v| v == "true").unwrap_or(defaut.actif),
            plafond_octets: plafond_mio
                .and_then(|v| v.parse::<u64>().ok())
                .map(|m| (m * MIO).max(PLAFOND_MINIMUM))
                .unwrap_or(defaut.plafond_octets),
        })
    }

    pub fn reglages(&self) -> Reglages {
        *self.reglages.read().unwrap_or_else(|p| p.into_inner())
    }

    pub fn ram_disponible(&self) -> Option<u64> {
        self.ram_disponible
    }

    /// Change les réglages ; désactiver libère le disque chargé. Bloquant.
    pub fn regler(&self, actif: Option<bool>, plafond_octets: Option<u64>) -> Reglages {
        let r = {
            let mut r = self.reglages.write().unwrap_or_else(|p| p.into_inner());
            if let Some(a) = actif {
                r.actif = a;
            }
            if let Some(p) = plafond_octets {
                r.plafond_octets = p.max(PLAFOND_MINIMUM);
            }
            *r
        };
        if !r.actif {
            self.liberer();
        }
        r
    }

    /// Le chargement du disque `disc_id` sur ce lecteur, lancé s'il ne
    /// l'est pas. `None` si le réglage est désactivé.
    ///
    /// `depart` : le secteur que la lecture demande, d'où part un NOUVEAU
    /// chargement.
    pub fn chargement(
        &self,
        lecteur: &Arc<dyn LecteurDisque>,
        toc: &Toc,
        disc_id: &str,
        generation: u64,
        depart: u32,
    ) -> Option<Arc<Chargement>> {
        let r = self.reglages();
        if !r.actif {
            return None;
        }
        let mut courant = self.courant.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(c) = courant.as_ref()
            && c.disc_id == disc_id
            && c.generation == generation
            && matches!(c.verrou().fin, None | Some(Fin::Termine))
        {
            return Some(c.clone());
        }
        if let Some(ancien) = courant.take() {
            ancien.liberer();
        }
        let c = Arc::new(Chargement::new(toc, disc_id.to_string(), generation));
        {
            let mut e = c.verrou();
            if let Some(depart) = e.situer(depart) {
                e.curseur = depart;
            }
        }
        c.demarrer(lecteur.clone(), r.plafond_octets);
        *courant = Some(c.clone());
        Some(c)
    }

    /// Arrête le chargement en cours et rend la mémoire. Bloquant.
    pub fn liberer(&self) {
        let c = self
            .courant
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(c) = c {
            tracing::info!(disc_id = %c.disc_id, "cd_memoire_liberee");
            c.liberer();
        }
    }

    pub fn progression(&self) -> Option<Progression> {
        self.courant
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|c| c.progression())
    }

    /// Le chargement courant (témoins).
    pub fn courant(&self) -> Option<Arc<Chargement>> {
        self.courant
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

#[cfg(test)]
mod tests {
    //! #6043 — témoins sur un faux lecteur dont chaque secteur dit son
    //! numéro (`simule.rs`) : un octet mal placé ne peut pas passer.

    use std::time::Duration;

    use super::*;
    use crate::discid::disc_id;
    use crate::fournisseur::{FournisseurCd, source_id};
    use crate::lecteur::{ErreurEjection, Presence};
    use crate::simule::{LecteurSimule, contenu_des_secteurs};
    use crate::toc::PisteToc;
    use tune_core::source_pcm::FournisseurPcm;

    /// Trois pistes : 300, 800 et 400 secteurs (piste 2 : 10,7 s).
    fn toc() -> Toc {
        let p = |numero, debut| PisteToc {
            numero,
            debut,
            audio: true,
        };
        Toc::nouvelle(vec![p(1, 0), p(2, 300), p(3, 1_100)], 1_500).unwrap()
    }

    fn octets(secteurs: u32) -> u64 {
        secteurs as u64 * OCTETS_PAR_SECTEUR as u64
    }

    /// Le faux lecteur, qui retient l'ORDRE des lectures et peut ralentir.
    struct Journal {
        l: Arc<LecteurSimule>,
        lectures: Mutex<Vec<u32>>,
        pause: Duration,
    }

    impl Journal {
        fn new(pause: Duration) -> Arc<Self> {
            Arc::new(Self {
                l: Arc::new(LecteurSimule::new(toc())),
                lectures: Mutex::new(Vec::new()),
                pause,
            })
        }
        fn lectures(&self) -> Vec<u32> {
            self.lectures.lock().unwrap().clone()
        }
    }

    impl LecteurDisque for Journal {
        fn chemin(&self) -> String {
            "journal".into()
        }
        fn presence(&self) -> Presence {
            self.l.presence()
        }
        fn lire_toc(&self) -> Result<Toc, ErreurCd> {
            self.l.lire_toc()
        }
        fn lire_secteurs(&self, lba: u32, nombre: u32, sortie: &mut [u8]) -> Result<(), ErreurCd> {
            self.lectures.lock().unwrap().push(lba);
            std::thread::sleep(self.pause);
            self.l.lire_secteurs(lba, nombre, sortie)
        }
        fn ejecter_disque(&self) -> Result<(), ErreurEjection> {
            self.l.ejecter_disque()
        }
    }

    fn fournisseur(lecteur: Arc<Journal>, plafond_octets: u64) -> (FournisseurCd, Arc<MemoireCd>) {
        let m = Arc::new(MemoireCd::new(Reglages {
            actif: true,
            plafond_octets,
        }));
        (
            FournisseurCd {
                lecteur,
                memoire: Some(m.clone()),
            },
            m,
        )
    }

    fn lire(f: &FournisseurCd, piste: u8, depuis_ms: u64) -> std::io::Result<Vec<u8>> {
        let mut flux = f
            .ouvrir(&source_id(&disc_id(&toc()), piste), depuis_ms)
            .unwrap();
        let mut v = Vec::new();
        flux.lecteur.read_to_end(&mut v)?;
        assert_eq!(flux.octets, v.len() as u64, "longueur annoncée");
        Ok(v)
    }

    /// Le contenu servi par la mémoire est celui du disque, au bit près,
    /// piste par piste ; une fois chargé, le lecteur ne lit plus rien.
    #[test]
    fn le_contenu_servi_par_la_memoire_est_identique_au_bit_pres() {
        let j = Journal::new(Duration::ZERO);
        let (f, m) = fournisseur(j.clone(), u64::MAX);
        let t = toc();
        for n in 1..=3u8 {
            let p = t.piste(n).unwrap();
            assert_eq!(
                lire(&f, n, 0).unwrap(),
                contenu_des_secteurs(p.debut, t.secteurs(n).unwrap()),
                "piste {n}"
            );
        }
        let c = m.courant().unwrap();
        assert_eq!(c.attendre_la_fin(), Some(Fin::Termine));
        let p = m.progression().unwrap();
        assert_eq!(
            (p.secteurs_charges, p.secteurs_total, p.pourcentage),
            (1_500, 1_500, 100)
        );
        assert_eq!(p.pistes_chargees, vec![1, 2, 3]);
        assert_eq!(p.octets_ram, octets(1_500));
        // Chaque bloc n'a été lu qu'une fois.
        let mut l = j.lectures();
        let n = l.len();
        l.sort_unstable();
        l.dedup();
        assert_eq!(l.len(), n, "un bloc relu");

        // Le disque entier, rejoué : plus une lecture au lecteur.
        let avant = j.lectures().len();
        let mut tout = Vec::new();
        for n in 1..=3u8 {
            tout.extend(lire(&f, n, 0).unwrap());
        }
        assert_eq!(tout, contenu_des_secteurs(0, 1_500));
        assert_eq!(j.lectures().len(), avant, "le lecteur a été relu");
    }

    /// La piste demandée passe en premier, à partir de la position demandée ;
    /// puis la suite du disque, puis le début.
    #[test]
    fn la_piste_demandee_est_chargee_en_premier() {
        let j = Journal::new(Duration::ZERO);
        let (f, m) = fournisseur(j.clone(), u64::MAX);
        // Piste 3, à 2 s : secteur 1 100 + 150.
        let v = lire(&f, 3, 2_000).unwrap();
        assert_eq!(v, contenu_des_secteurs(1_250, 250));
        m.courant().unwrap().attendre_la_fin();
        let l = j.lectures();
        // Les blocs de la piste 3 partent de 1 100 : 1 250 est dans celui
        // de 1 244.
        assert_eq!(
            l[0], 1_244,
            "premier bloc lu : celui de la position demandée"
        );
        // Le premier bloc lu HORS de la piste 3 : le reste vient après.
        let i = l.iter().position(|&s| s < 1_100).unwrap();
        assert!(
            l[..i].iter().all(|&s| s >= 1_100),
            "la piste 3 entière passe avant les autres : {:?}",
            &l[..i + 1]
        );
    }

    /// L'avance tombe dans la mémoire : le bon secteur, sans relire le
    /// lecteur.
    #[test]
    fn l_avance_se_fait_dans_la_memoire() {
        let j = Journal::new(Duration::ZERO);
        let (f, m) = fournisseur(j.clone(), u64::MAX);
        lire(&f, 1, 0).unwrap();
        m.courant().unwrap().attendre_la_fin();
        let avant = j.lectures().len();
        // 6,5 s dans la piste 2 = 487,5 secteurs : on reprend au 487ᵉ.
        let v = lire(&f, 2, 6_500).unwrap();
        assert_eq!(v, contenu_des_secteurs(300 + 487, 800 - 487));
        // Retour au début de la piste 1.
        assert_eq!(lire(&f, 1, 0).unwrap(), contenu_des_secteurs(0, 300));
        assert_eq!(j.lectures().len(), avant, "l'avance a relu le lecteur");
    }

    /// L'avance AVANT la fin du chargement : le bloc visé devient
    /// prioritaire, sans attendre que le chargement l'atteigne.
    #[test]
    fn l_avance_pendant_le_chargement_rend_le_bloc_vise_prioritaire() {
        let j = Journal::new(Duration::from_millis(5));
        let (f, m) = fournisseur(j.clone(), u64::MAX);
        let mut flux = f.ouvrir(&source_id(&disc_id(&toc()), 1), 0).unwrap();
        let mut bloc = vec![0u8; OCTETS_PAR_SECTEUR];
        flux.lecteur.read_exact(&mut bloc).unwrap();
        // Avance dans la piste 3, à 4 s = 300 secteurs : le secteur 1 400,
        // que le chargement dans l'ordre n'atteindrait qu'après les pistes
        // 1 et 2 entières (1 100 secteurs).
        let mut flux3 = f.ouvrir(&source_id(&disc_id(&toc()), 3), 4_000).unwrap();
        let mut v = Vec::new();
        flux3.lecteur.read_to_end(&mut v).unwrap();
        assert_eq!(v, contenu_des_secteurs(1_400, 100));
        let charges = m.progression().unwrap().secteurs_charges;
        assert!(
            charges < 1_100,
            "le bloc visé a attendu les pistes précédentes ({charges} secteurs chargés)"
        );
        drop(flux);
        m.liberer();
    }

    /// L'éjection pendant le chargement l'arrête, rend la mémoire, et un
    /// flux en attente reçoit l'erreur d'éjection — pas du silence.
    #[test]
    fn l_ejection_pendant_le_chargement_l_arrete_et_rend_la_memoire() {
        let j = Journal::new(Duration::ZERO);
        j.l.ejecter_apres(5);
        let (f, m) = fournisseur(j.clone(), u64::MAX);
        let e = lire(&f, 1, 0).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::NotConnected);
        let c = m.courant().unwrap();
        assert_eq!(c.attendre_la_fin(), Some(Fin::Ejecte));
        let p = c.progression();
        assert_eq!((p.octets_ram, p.octets_fichier), (0, 0), "mémoire rendue");
        assert_eq!(
            j.lectures().len(),
            6,
            "le chargement a continué après l'éjection"
        );
    }

    /// `liberer` (éjection commandée, autre disque) pendant le chargement :
    /// le fil s'arrête au bloc suivant et la mémoire est rendue.
    #[test]
    fn liberer_pendant_le_chargement_arrete_le_fil() {
        let j = Journal::new(Duration::from_millis(5));
        let (f, m) = fournisseur(j.clone(), u64::MAX);
        let mut flux = f.ouvrir(&source_id(&disc_id(&toc()), 1), 0).unwrap();
        let mut bloc = vec![0u8; OCTETS_PAR_SECTEUR];
        flux.lecteur.read_exact(&mut bloc).unwrap();
        let c = m.courant().unwrap();
        m.liberer();
        assert!(m.progression().is_none());
        assert_eq!(c.attendre_la_fin(), Some(Fin::Libere));
        let lu = j.lectures().len();
        assert!(lu < 1_500usize.div_ceil(24), "chargement allé au bout");
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(j.lectures().len(), lu, "le fil lit encore après liberer");
        assert_eq!(c.progression().octets_ram, 0);
        let mut reste = Vec::new();
        assert_eq!(
            flux.lecteur.read_to_end(&mut reste).unwrap_err().kind(),
            std::io::ErrorKind::NotConnected
        );
    }

    /// Le plafond : la RAM n'en dépasse jamais la valeur, les pistes de trop
    /// vont dans un fichier temporaire, et le contenu reste exact.
    #[test]
    fn le_plafond_memoire_est_respecte_avec_repli_sur_fichier() {
        let j = Journal::new(Duration::ZERO);
        let plafond = octets(900);
        let (f, m) = fournisseur(j.clone(), plafond);
        lire(&f, 1, 0).unwrap();
        m.courant().unwrap().attendre_la_fin();
        let p = m.progression().unwrap();
        assert!(
            p.octets_ram <= plafond,
            "RAM {} > plafond {plafond}",
            p.octets_ram
        );
        // Piste 1 (300) en RAM, piste 2 (800) dépasse : fichier ; piste 3
        // (400) tient encore.
        assert_eq!(p.octets_ram, octets(700));
        assert_eq!(p.octets_fichier, octets(800));
        let mut tout = Vec::new();
        for n in 1..=3u8 {
            tout.extend(lire(&f, n, 0).unwrap());
        }
        assert_eq!(tout, contenu_des_secteurs(0, 1_500));
    }

    /// Réglage désactivé : la lecture relit le lecteur en temps réel.
    #[test]
    fn desactive_la_lecture_relit_le_lecteur() {
        let j = Journal::new(Duration::ZERO);
        let (f, m) = fournisseur(j.clone(), u64::MAX);
        m.regler(Some(false), None);
        assert_eq!(lire(&f, 2, 0).unwrap(), contenu_des_secteurs(300, 800));
        assert!(m.courant().is_none());
        assert_eq!(j.lectures().len(), 800usize.div_ceil(24));
    }

    /// Un autre disque libère le précédent.
    #[test]
    fn un_autre_disque_libere_le_precedent() {
        let j = Journal::new(Duration::ZERO);
        let (_, m) = fournisseur(j.clone(), u64::MAX);
        let l: Arc<dyn LecteurDisque> = j.clone();
        let a = m.chargement(&l, &toc(), "a", 0, 0).unwrap();
        a.attendre_la_fin();
        let b = m.chargement(&l, &toc(), "b", 0, 0).unwrap();
        assert_eq!(a.progression().fin, Some(Fin::Libere));
        assert_eq!(a.progression().octets_ram, 0);
        assert!(!Arc::ptr_eq(&a, &b));
        // Le même disque reprend le même chargement.
        assert!(Arc::ptr_eq(
            &b,
            &m.chargement(&l, &toc(), "b", 0, 0).unwrap()
        ));
        m.liberer();
    }

    #[test]
    fn le_defaut_suit_la_ram_disponible() {
        assert_eq!(
            Reglages::par_defaut(None),
            Reglages {
                actif: true,
                plafond_octets: PLAFOND_PAR_DEFAUT
            }
        );
        // Raspberry Pi à 1 Gio : ~600 Mio disponibles — désactivé, plafond
        // à la moitié.
        let pi = Reglages::par_defaut(Some(600 * MIO));
        assert!(!pi.actif);
        assert_eq!(pi.plafond_octets, 300 * MIO);
        let gros = Reglages::par_defaut(Some(16 * 1024 * MIO));
        assert!(gros.actif);
        assert_eq!(gros.plafond_octets, PLAFOND_PAR_DEFAUT);
        assert_eq!(
            ram_disponible_de_meminfo("MemTotal: 8 kB\nMemAvailable:   2048 kB\n"),
            Some(2 * MIO)
        );
    }
}
