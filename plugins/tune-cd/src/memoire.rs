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
//!
//! ## Disque entier en mémoire : l'éjection ne coupe plus la lecture (#6410)
//!
//! Comme sur Daphile : une fois TOUT le disque chargé (`Fin::Termine`), le
//! lecteur n'a plus rien à fournir. L'éjection garde alors le disque en
//! mémoire ([`MemoireCd::garder_apres_ejection`]) et la file continue d'y
//! ouvrir ses pistes ([`MemoireCd::disque_garde`]). La mémoire est rendue à
//! la fin de la lecture du disque ou à l'arrêt (le surveillant, `ejection.rs`),
//! ou au chargement d'un autre disque ([`MemoireCd::chargement`]). Un disque
//! chargé en partie garde le comportement de #6043 : l'éjection libère et
//! arrête.
//!
//! ## Charger sans jouer (#6596)
//!
//! Le chargement se lance aussi SANS lecture : [`MemoireCd::charger_le_disque`]
//! (route `POST /memoire/charger`), que le client propose à l'insertion. Il ne
//! touche ni à la file ni à la lecture d'aucune zone ; la lecture lancée
//! ensuite (`FournisseurCd::ouvrir`) retrouve ce chargement par
//! [`MemoireCd::chargement`] et se sert dans la mémoire. Chaque pas de
//! pourcentage, puis la fin, passent par l'observateur
//! ([`MemoireCd::observer`]) : le greffon en fait les évènements
//! [`EVT_PROGRESSION`] et [`EVT_TERMINE`].
//!
//! ## Vérification AccurateRip réelle (#8522)
//!
//! Le disque ENTIER en mémoire est vérifié contre la base AccurateRip
//! (`accuraterip_base`) par une tâche de fond : identifiant du disque, un
//! appel à la base, CRC de chaque piste au décalage réglé ; si toutes les
//! pistes connues ne sont pas vérifiées, le décalage du lecteur est cherché
//! sur `±DECALAGE_MAX` avec une piste de référence. Le verdict va dans la
//! [`Progression`] (`verification_accuraterip`) et part en évènement
//! [`EVT_ACCURATERIP`]. Base injoignable : « non vérifié ».
//!
//! ## AccurateRip au chargement, et l'enregistrement sans relecture (#8514)
//!
//! Dès qu'une piste est ENTIÈRE en mémoire, le fil de chargement en calcule
//! les CRC AccurateRip v1 et v2 avec le calcul de l'extraction
//! (`extraction::accuraterip`, mêmes bords de disque) : la
//! [`Progression`] les porte (`accuraterip`), le client les montre sans
//! attendre une extraction. Le disque entier chargé se verse ensuite dans la
//! bibliothèque sans relire le lecteur : [`LecteurMemoire`] présente la
//! mémoire comme un lecteur, et l'extraction (encodeur, balises, pochette,
//! nommage, scan ciblé) la lit comme elle lirait le disque.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};

use serde::Serialize;

use crate::accuraterip_base::{
    self as ar, Base, BaseAr, DECALAGE_MAX, IdentifiantAr, StatutAr, VerificationAr,
};
use crate::extraction::accuraterip::{self, CalculAccurateRip};
use crate::extraction::lecture_sure::{Bilan, Verification, lire_bloc};
use crate::lecteur::{ErreurCd, LecteurDisque, Presence};
use crate::toc::{OCTETS_PAR_SECTEUR, TRAMES_PAR_SECTEUR, Toc};

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

/// #6596 — les évènements du chargement, sur le bus puis sur le WebSocket :
/// la [`Progression`] à chaque point de pourcentage gagné, puis une fois à la
/// fin (`fin` renseignée : `termine` = prêt, `echec` ou `ejecte` = échec,
/// `libere` = rendu à la demande).
pub const EVT_PROGRESSION: &str = "cd.memoire.progression";
pub const EVT_TERMINE: &str = "cd.memoire.termine";
/// #8522 — la vérification AccurateRip du disque chargé (en cours, puis
/// finie) : la [`Progression`] entière.
pub const EVT_ACCURATERIP: &str = "cd.memoire.accuraterip";

/// #8522 — ce qu'il faut à la vérification de fond : la base, le décalage
/// réglé, le bus et le moteur asynchrone.
pub struct VerifieurAr {
    pub base: Arc<BaseAr>,
    pub decalage: Arc<dyn Fn() -> i32 + Send + Sync>,
    pub bus: Option<tune_core::event_bus::EventBus>,
    pub moteur: tokio::runtime::Handle,
}

/// Ce que le greffon fait d'une progression (les évènements ci-dessus).
pub type Observateur = Arc<dyn Fn(&Progression) + Send + Sync>;

/// #6596 — l'observateur de production : chaque progression devient
/// [`EVT_PROGRESSION`], la fin [`EVT_TERMINE`], avec la [`Progression`]
/// entière (celle que rendent `GET /etat` et `GET /memoire`).
pub fn observateur_du_bus(bus: tune_core::event_bus::EventBus) -> Observateur {
    Arc::new(move |p: &Progression| {
        let evenement = if p.fin.is_some() {
            EVT_TERMINE
        } else {
            EVT_PROGRESSION
        };
        bus.emit(
            evenement,
            serde_json::to_value(p).unwrap_or(serde_json::Value::Null),
        );
    })
}

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
    /// #6596 — le chargement n'a pas pu aller au bout (lecture, écriture ou
    /// fichier temporaire impossible) : la mémoire est rendue.
    Echec,
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
    /// #8514 — les CRC AccurateRip des pistes entières, par numéro.
    accuraterip: Vec<AccurateRipPiste>,
    /// #8522 — la vérification contre la base.
    verification_ar: Option<VerificationAr>,
}

/// #8514 — les CRC AccurateRip v1 et v2 d'une piste chargée, en hexadécimal
/// (8 chiffres) : le calcul de l'extraction, sur les mêmes secteurs, SANS
/// correction du décalage du lecteur ni comparaison à la base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AccurateRipPiste {
    pub piste: u8,
    pub v1: String,
    pub v2: String,
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
    /// #6410 — la TOC du disque chargé : une fois le disque éjecté, c'est
    /// elle qui situe les pistes, le lecteur n'en a plus.
    toc: Toc,
    secteurs_total: u64,
    etat: Mutex<Etat>,
    signal: Condvar,
    arret: AtomicBool,
    fil: Mutex<Option<std::thread::JoinHandle<()>>>,
    prochain_lecteur: AtomicU64,
    /// #6596 — reçoit la progression (évènements du greffon).
    observateur: Option<Observateur>,
    /// #8522 — vérifie le disque contre la base une fois entier.
    verifieur: Mutex<Option<Arc<VerifieurAr>>>,
}

impl Chargement {
    fn new(toc: &Toc, disc_id: String, generation: u64, observateur: Option<Observateur>) -> Self {
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
            toc: toc.clone(),
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
                accuraterip: Vec::new(),
                verification_ar: None,
            }),
            signal: Condvar::new(),
            arret: AtomicBool::new(false),
            fil: Mutex::new(None),
            prochain_lecteur: AtomicU64::new(1),
            observateur,
            verifieur: Mutex::new(None),
        }
    }

    /// #6596 — passe la progression à l'observateur, s'il y en a un.
    fn annoncer(&self) {
        if let Some(o) = &self.observateur {
            o(&self.progression());
        }
    }

    fn verrou(&self) -> std::sync::MutexGuard<'_, Etat> {
        self.etat.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// La TOC du disque chargé.
    pub fn toc(&self) -> &Toc {
        &self.toc
    }

    /// Tout le disque est en mémoire : le lecteur n'a plus rien à fournir.
    pub fn complet(&self) -> bool {
        self.verrou().fin == Some(Fin::Termine)
    }

    /// Lance le fil de chargement.
    fn demarrer(self: &Arc<Self>, lecteur: Arc<dyn LecteurDisque>, plafond: u64) {
        let c = self.clone();
        let fil = std::thread::Builder::new()
            .name("tune-cd-memoire".into())
            .spawn(move || {
                c.charger(lecteur.as_ref(), plafond);
                // #8522 — entier : la vérification part en fond.
                if c.complet() {
                    c.lancer_la_verification();
                }
            })
            .expect("fil de chargement du CD");
        *self.fil.lock().unwrap_or_else(|p| p.into_inner()) = Some(fil);
    }

    fn terminer(&self, fin: Fin) {
        let mut e = self.verrou();
        let mut change = false;
        if e.fin.is_none() || fin != Fin::Termine {
            if fin != Fin::Termine {
                e.rendre_la_memoire();
            }
            change = e.fin.as_ref() != Some(&fin);
            e.fin = Some(fin);
        }
        drop(e);
        // L'annonce AVANT le réveil : qui attend la fin la trouve annoncée.
        if change {
            self.annoncer();
        }
        self.signal.notify_all();
    }

    fn charger(&self, lecteur: &dyn LecteurDisque, plafond: u64) {
        tracing::info!(disc_id = %self.disc_id, plafond, "cd_memoire_chargement_demarre");
        self.annoncer();
        let mut pourcentage_annonce = 0u8;
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
                                return self.terminer(Fin::Echec);
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
                    return self.terminer(Fin::Echec);
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
                return self.terminer(Fin::Echec);
            }
            e.pistes[i].blocs[b] = true;
            e.secteurs_charges += n as u64;
            e.secteurs_perdus += bilan.secteurs_illisibles - perdus_avant;
            e.piste_en_cours = Some(e.pistes[i].numero);
            e.curseur = (i, b + 1);
            let piste_entiere = e.pistes[i].blocs.iter().all(|c| *c);
            let pourcentage = pourcentage_de(e.secteurs_charges, self.secteurs_total);
            drop(e);
            self.signal.notify_all();
            // #8514 — la piste est entière : ses CRC AccurateRip, tout de
            // suite (le 100 % les porte donc tous).
            if piste_entiere {
                self.calculer_accuraterip(i);
            }
            // #6596 — un évènement par point gagné, pas un par bloc (un CD
            // en compte des milliers). Le 100 % part avec la fin.
            if pourcentage > pourcentage_annonce && pourcentage < 100 {
                pourcentage_annonce = pourcentage;
                self.annoncer();
            }
        }
    }

    /// #8514 — les CRC AccurateRip de la piste d'index `i`, entière en
    /// mémoire, lus bloc par bloc (le verrou n'est tenu que le temps d'une
    /// copie : la lecture vers une zone n'attend pas). Mémoire rendue en
    /// route : rien n'est rangé.
    fn calculer_accuraterip(&self, i: usize) {
        let (numero, debut, secteurs, n_pistes) = {
            let e = self.verrou();
            let p = &e.pistes[i];
            (p.numero, p.debut, p.secteurs, e.pistes.len())
        };
        let mut calcul = CalculAccurateRip::new(
            secteurs * TRAMES_PAR_SECTEUR as u32,
            i == 0,
            i + 1 == n_pistes,
        );
        let mut tampon = vec![0u8; SECTEURS_PAR_BLOC as usize * OCTETS_PAR_SECTEUR];
        let mut lba = debut;
        while lba < debut + secteurs {
            let n = (debut + secteurs - lba).min(SECTEURS_PAR_BLOC);
            let octets = &mut tampon[..n as usize * OCTETS_PAR_SECTEUR];
            if self.copier_secteurs(lba, n, octets).is_err() {
                return;
            }
            calcul.ajouter(octets);
            lba += n;
        }
        let (v1, v2) = calcul.resultat();
        let mut e = self.verrou();
        if !e.accuraterip.iter().any(|a| a.piste == numero) {
            e.accuraterip.push(AccurateRipPiste {
                piste: numero,
                v1: accuraterip::hex(v1),
                v2: accuraterip::hex(v2),
            });
            e.accuraterip.sort_by_key(|a| a.piste);
        }
    }

    /// #8514 — copie dans `sortie` (`n × 2 352` octets) les secteurs
    /// `[lba, lba + n)`, tous déjà chargés, sans attendre ni relire le
    /// lecteur. Un secteur pas encore chargé, ou la mémoire rendue
    /// (éjection, libération) : [`ErreurCd::AucunDisque`], que l'extraction
    /// traduit en « disque retiré ».
    pub fn copier_secteurs(&self, lba: u32, n: u32, sortie: &mut [u8]) -> Result<(), ErreurCd> {
        if sortie.len() != n as usize * OCTETS_PAR_SECTEUR {
            return Err(ErreurCd::Autre("tampon de taille incorrecte".into()));
        }
        let mut s = lba;
        let mut fait = 0usize;
        while s < lba + n {
            let mut e = self.verrou();
            if e.fin.as_ref().is_some_and(|f| *f != Fin::Termine) {
                return Err(ErreurCd::AucunDisque);
            }
            let Some((i, b)) = e.situer(s) else {
                return Err(ErreurCd::Lecture {
                    lba: s,
                    raison: "secteur hors des pistes chargées".into(),
                });
            };
            let p = &mut e.pistes[i];
            if !p.blocs[b] {
                return Err(ErreurCd::AucunDisque);
            }
            let (_, k) = p.bloc(b);
            let fin_bloc = p.debut + b as u32 * SECTEURS_PAR_BLOC + k;
            let m = (fin_bloc.min(lba + n) - s) as usize * OCTETS_PAR_SECTEUR;
            let decalage = (s - p.debut) as u64 * OCTETS_PAR_SECTEUR as u64;
            lire(p.stockage.as_mut(), decalage, &mut sortie[fait..fait + m])
                .map_err(|_| ErreurCd::AucunDisque)?;
            fait += m;
            s += (m / OCTETS_PAR_SECTEUR) as u32;
        }
        Ok(())
    }

    /// #8522 — lance la vérification AccurateRip en fond, s'il y a un
    /// vérificateur : « en cours » tout de suite, le verdict ensuite.
    pub fn lancer_la_verification(self: &Arc<Self>) {
        let Some(v) = self
            .verifieur
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        else {
            return;
        };
        let id = IdentifiantAr::depuis_toc(&self.toc);
        self.poser_verification(VerificationAr {
            identifiant: id.nom(),
            base: "en_cours",
            decalage: (v.decalage)(),
            decalage_detecte: false,
            pistes: Vec::new(),
        });
        let c = self.clone();
        v.moteur.clone().spawn(async move {
            let base = v.base.consulter(&id).await;
            let decalage = (v.decalage)();
            let c2 = c.clone();
            let r =
                tokio::task::spawn_blocking(move || c2.verifier_accuraterip(&base, &id, decalage))
                    .await
                    .ok()
                    .flatten();
            let Some(r) = r else {
                return;
            };
            tracing::info!(
                id = %r.identifiant,
                base = r.base,
                decalage = r.decalage,
                detecte = r.decalage_detecte,
                verifiees = r.pistes.iter().filter(|p| p.statut == StatutAr::Verifie).count(),
                pistes = r.pistes.len(),
                "cd_memoire_accuraterip_verifie"
            );
            c.poser_verification(r);
        });
    }

    fn poser_verification(&self, v: VerificationAr) {
        self.verrou().verification_ar = Some(v);
        let verifieur = self
            .verifieur
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(bus) = verifieur.and_then(|v| v.bus.clone()) {
            bus.emit(
                EVT_ACCURATERIP,
                serde_json::to_value(self.progression()).unwrap_or(serde_json::Value::Null),
            );
        }
    }

    /// #8522 — la vérification AccurateRip du disque entier en mémoire
    /// (bloquant : relit la mémoire, jamais le lecteur). `None` si la
    /// mémoire a été rendue entre-temps.
    pub fn verifier_accuraterip(
        &self,
        base: &Base,
        id: &IdentifiantAr,
        decalage_regle: i32,
    ) -> Option<VerificationAr> {
        let plages: Vec<(u8, u32, u32)> = {
            let e = self.verrou();
            e.pistes
                .iter()
                .map(|p| (p.numero, p.debut, p.debut + p.secteurs))
                .collect()
        };
        let n = plages.len();
        let audio = (plages.first()?.1, plages.last()?.2);
        let mut lire = |lba: u32, k: u32, s: &mut [u8]| self.copier_secteurs(lba, k, s);
        let mut calculer = |decalage: i32| -> Option<Vec<(u32, u32)>> {
            plages
                .iter()
                .enumerate()
                .map(|(i, (_, d, f))| {
                    ar::crc_decale(&mut lire, (*d, *f), audio, decalage, (i == 0, i + 1 == n)).ok()
                })
                .collect()
        };
        let verifiees = |crcs: &[(u32, u32)], decalage: i32| {
            crcs.iter()
                .enumerate()
                .filter(|(i, c)| ar::juger(base, *i, 0, **c, decalage).statut == StatutAr::Verifie)
                .count()
        };
        let mut decalage = decalage_regle;
        let mut crcs = calculer(decalage)?;
        let connues = (0..n).filter(|i| !base.crcs(*i).is_empty()).count();
        if matches!(base, Base::Trouvee(_)) && verifiees(&crcs, decalage) < connues {
            for o in self.decalages_candidats(base, &plages, audio) {
                if o == decalage {
                    continue;
                }
                let c = calculer(o)?;
                if verifiees(&c, o) > verifiees(&crcs, decalage) {
                    crcs = c;
                    decalage = o;
                }
            }
        }
        Some(VerificationAr {
            identifiant: id.nom(),
            base: base.etat(),
            decalage,
            decalage_detecte: decalage != decalage_regle,
            pistes: plages
                .iter()
                .zip(&crcs)
                .enumerate()
                .map(|(i, ((numero, _, _), c))| ar::juger(base, i, *numero, *c, decalage))
                .collect(),
        })
    }

    /// #8522 — les décalages où le CRC v1 d'une piste de référence (la plus
    /// courte des pistes que la base connaît, hors bords du disque si
    /// possible) figure dans la base, du plus petit au plus grand en valeur
    /// absolue ; cinq au plus.
    fn decalages_candidats(
        &self,
        base: &Base,
        plages: &[(u8, u32, u32)],
        audio: (u32, u32),
    ) -> Vec<i32> {
        let n = plages.len();
        let connues: Vec<usize> = (0..n).filter(|i| !base.crcs(*i).is_empty()).collect();
        let reference = connues
            .iter()
            .filter(|i| n <= 2 || (**i != 0 && **i + 1 != n))
            .chain(connues.iter())
            .min_by_key(|i| plages[**i].2 - plages[**i].1)
            .copied();
        let Some(i) = reference else {
            return Vec::new();
        };
        let (_, d, f) = plages[i];
        let r = DECALAGE_MAX as usize;
        let trames = (f - d) as usize * TRAMES_PAR_SECTEUR as usize;
        let mut lire = |lba: u32, k: u32, s: &mut [u8]| self.copier_secteurs(lba, k, s);
        let Ok(octets) = ar::lire_trames(
            &mut lire,
            d as i64 * TRAMES_PAR_SECTEUR as i64 - r as i64,
            (trames + 2 * r) as u64,
            audio,
        ) else {
            return Vec::new();
        };
        let x: Vec<u32> = octets
            .as_chunks::<4>()
            .0
            .iter()
            .map(|t| u32::from_le_bytes(*t))
            .collect();
        let connus = base.crcs(i);
        let mut candidats: Vec<i32> = ar::crc_v1_glissants(&x, r, trames, (i == 0, i + 1 == n))
            .iter()
            .enumerate()
            .filter(|(_, c)| connus.contains(c))
            .map(|(k, _)| k as i32 - r as i32)
            .collect();
        candidats.sort_by_key(|o| o.abs());
        candidats.truncate(5);
        candidats
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
            pourcentage: pourcentage_de(e.secteurs_charges, self.secteurs_total),
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
            accuraterip: e.accuraterip.clone(),
            verification_accuraterip: e.verification_ar.clone(),
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

fn pourcentage_de(charges: u64, total: u64) -> u8 {
    (charges * 100).checked_div(total).unwrap_or(100) as u8
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
    /// #8514 — les CRC AccurateRip des pistes entières en mémoire.
    pub accuraterip: Vec<AccurateRipPiste>,
    /// #8522 — la vérification contre la base AccurateRip : `base` vaut
    /// `en_cours`, puis `trouvee`, `absente` ou `injoignable`.
    pub verification_accuraterip: Option<VerificationAr>,
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

/// #8514 — le disque ENTIER en mémoire, vu comme un lecteur : l'extraction
/// le lit comme le disque (mêmes secteurs, mêmes CRC), sans toucher au
/// lecteur. Sa génération ne change jamais : c'est la mémoire rendue
/// (éjection d'un disque partiel, autre disque, libération) qui arrête
/// l'extraction, par [`ErreurCd::AucunDisque`].
pub struct LecteurMemoire {
    pub chargement: Arc<Chargement>,
}

impl LecteurDisque for LecteurMemoire {
    fn chemin(&self) -> String {
        "memoire".into()
    }
    fn presence(&self) -> Presence {
        if self.chargement.complet() {
            Presence::Disque
        } else {
            Presence::Vide
        }
    }
    fn lire_toc(&self) -> Result<Toc, ErreurCd> {
        Ok(self.chargement.toc().clone())
    }
    fn lire_secteurs(&self, lba: u32, nombre: u32, sortie: &mut [u8]) -> Result<(), ErreurCd> {
        self.chargement.copier_secteurs(lba, nombre, sortie)
    }
}

/// #8514 — voir [`MemoireCd::retenir_pour_enregistrement`].
pub struct RetenueEnregistrement(Arc<MemoireCd>);

impl Drop for RetenueEnregistrement {
    fn drop(&mut self) {
        self.0.enregistrements.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Le chargement en mémoire du greffon : ses réglages et le disque chargé.
pub struct MemoireCd {
    reglages: RwLock<Reglages>,
    courant: Mutex<Option<Arc<Chargement>>>,
    ram_disponible: Option<u64>,
    /// #6410 — le disque courant, entier en mémoire, a été éjecté : la
    /// lecture continue depuis la mémoire.
    garde: AtomicBool,
    /// #6596 — les évènements du greffon, passés à chaque chargement.
    observateur: RwLock<Option<Observateur>>,
    /// #8522 — la vérification AccurateRip, passée à chaque chargement.
    verifieur: RwLock<Option<Arc<VerifieurAr>>>,
    /// #8514 — les enregistrements dans la bibliothèque qui lisent la
    /// mémoire ([`RetenueEnregistrement`]) : tant qu'il y en a, le disque
    /// gardé après son éjection n'est pas rendu en fin de lecture.
    enregistrements: AtomicUsize,
}

impl MemoireCd {
    pub fn new(reglages: Reglages) -> Self {
        Self {
            reglages: RwLock::new(reglages),
            courant: Mutex::new(None),
            ram_disponible: ram_disponible(),
            garde: AtomicBool::new(false),
            observateur: RwLock::new(None),
            verifieur: RwLock::new(None),
            enregistrements: AtomicUsize::new(0),
        }
    }

    /// #8522 — chaque disque chargé en entier est vérifié par `v`.
    pub fn verifier_avec(&self, v: VerifieurAr) {
        *self.verifieur.write().unwrap_or_else(|p| p.into_inner()) = Some(Arc::new(v));
    }

    /// #6596 — la progression de chaque chargement à venir passe par `o`.
    pub fn observer(&self, o: Observateur) {
        *self.observateur.write().unwrap_or_else(|p| p.into_inner()) = Some(o);
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
            // #6410 — un autre disque se charge : le disque gardé après son
            // éjection rend sa mémoire.
            if self.garde.swap(false, Ordering::SeqCst) {
                tracing::info!(disc_id = %ancien.disc_id, "cd_memoire_gardee_liberee_autre_disque");
            }
            ancien.liberer();
        }
        let observateur = self
            .observateur
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let c = Arc::new(Chargement::new(
            toc,
            disc_id.to_string(),
            generation,
            observateur,
        ));
        {
            let mut e = c.verrou();
            if let Some(depart) = e.situer(depart) {
                e.curseur = depart;
            }
        }
        *c.verifieur.lock().unwrap_or_else(|p| p.into_inner()) = self
            .verifieur
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        c.demarrer(lecteur.clone(), r.plafond_octets);
        *courant = Some(c.clone());
        Some(c)
    }

    /// #6596 (relecture rc7) — le disque d'un AUTRE `disc_id` qui tient
    /// encore la mémoire (chargement en cours, ou entier : gardé après son
    /// éjection, #6410), et s'il est encore en cours de chargement. Charger
    /// un autre disque le libérerait.
    pub fn autre_disque_en_memoire(&self, disc_id: &str) -> Option<(String, bool)> {
        let c = self.courant()?;
        if c.disc_id == disc_id {
            return None;
        }
        match c.verrou().fin {
            None => Some((c.disc_id.clone(), true)),
            Some(Fin::Termine) => Some((c.disc_id.clone(), false)),
            _ => None,
        }
    }

    /// #6596 — « Charger en mémoire » sans rien jouer : le disque entier, du
    /// début, ou le chargement déjà en cours (ou fini) de ce disque. Aucune
    /// zone n'est touchée. `None` si le réglage est désactivé.
    pub fn charger_le_disque(
        &self,
        lecteur: &Arc<dyn LecteurDisque>,
        toc: &Toc,
        disc_id: &str,
        generation: u64,
    ) -> Option<Arc<Chargement>> {
        let depart = toc.pistes_audio().next().map(|p| p.debut).unwrap_or(0);
        self.chargement(lecteur, toc, disc_id, generation, depart)
    }

    /// Arrête le chargement en cours et rend la mémoire. Bloquant.
    pub fn liberer(&self) {
        let c = self
            .courant
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        self.garde.store(false, Ordering::SeqCst);
        if let Some(c) = c {
            tracing::info!(disc_id = %c.disc_id, "cd_memoire_liberee");
            c.liberer();
        }
    }

    /// #6410 — le disque vient de quitter le lecteur. S'il est ENTIER en
    /// mémoire, on le garde (la lecture continue) et on rend `true` ; sinon
    /// on libère, comme #6043, et on rend `false`. Bloquant.
    pub fn garder_apres_ejection(&self) -> bool {
        let complet = self.courant().filter(|c| c.complet());
        match complet {
            Some(c) => {
                if !self.garde.swap(true, Ordering::SeqCst) {
                    tracing::info!(disc_id = %c.disc_id, "cd_memoire_gardee_apres_ejection");
                }
                true
            }
            None => {
                self.liberer();
                false
            }
        }
    }

    /// Le disque entier en mémoire, quel que soit l'état du lecteur.
    pub fn disque_complet(&self) -> Option<Arc<Chargement>> {
        self.courant().filter(|c| c.complet())
    }

    /// #8514 — retient la mémoire pour un enregistrement : prise AVANT de
    /// vérifier que le disque est entier (le surveillant ne peut plus le
    /// rendre entre la vérification et la lecture), relâchée quand la
    /// retenue tombe — fin, refus, erreur ou panique.
    pub fn retenir_pour_enregistrement(self: &Arc<Self>) -> RetenueEnregistrement {
        self.enregistrements.fetch_add(1, Ordering::SeqCst);
        RetenueEnregistrement(self.clone())
    }

    /// #8514 — un enregistrement lit-il la mémoire ?
    pub fn enregistrement_en_cours(&self) -> bool {
        self.enregistrements.load(Ordering::SeqCst) > 0
    }

    /// #6410 — le disque éjecté mais gardé en mémoire, s'il y en a un.
    pub fn disque_garde(&self) -> Option<Arc<Chargement>> {
        if !self.garde.load(Ordering::SeqCst) {
            return None;
        }
        self.disque_complet()
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

    /// #8514 — référence AccurateRip écrite à part, d'un seul tenant, en
    /// 64 bits (seuls les 32 bits bas comptent : l'addition déborde sans
    /// dommage) : Σ mot × position (v1) et Σ (bas + haut) du produit (v2),
    /// en sautant 5 × 588 trames en tête du disque et en fin.
    fn accuraterip_de_reference(pcm: &[u8], premiere: bool, derniere: bool) -> (String, String) {
        let n = (pcm.len() / 4) as u64;
        let (mut v1, mut v2) = (0u64, 0u64);
        for (k, t) in pcm.as_chunks::<4>().0.iter().enumerate() {
            let position = k as u64 + 1;
            if (premiere && position < 2_940) || (derniere && position > n - 2_940) {
                continue;
            }
            let mot = u32::from_le_bytes(*t) as u64;
            let produit = mot * position;
            v1 = v1.wrapping_add(produit);
            v2 = v2.wrapping_add((produit & 0xFFFF_FFFF) + (produit >> 32));
        }
        (format!("{:08x}", v1 as u32), format!("{:08x}", v2 as u32))
    }

    /// #8514 — les CRC AccurateRip sont calculés AU CHARGEMENT, piste par
    /// piste, et valent ceux du disque — même quand l'ordre de chargement
    /// part du milieu (avance dans la piste 3 d'abord).
    #[test]
    fn accuraterip_est_calcule_au_chargement() {
        let j = Journal::new(Duration::ZERO);
        let (f, m) = fournisseur(j.clone(), u64::MAX);
        // Le chargement part de la piste 3, à 2 s : ses blocs, puis le
        // début du disque.
        lire(&f, 3, 2_000).unwrap();
        assert_eq!(m.courant().unwrap().attendre_la_fin(), Some(Fin::Termine));
        let t = toc();
        let attendus: Vec<AccurateRipPiste> = (1..=3u8)
            .map(|n| {
                let p = t.piste(n).unwrap();
                let (v1, v2) = accuraterip_de_reference(
                    &contenu_des_secteurs(p.debut, t.secteurs(n).unwrap()),
                    n == 1,
                    n == 3,
                );
                AccurateRipPiste { piste: n, v1, v2 }
            })
            .collect();
        let p = m.progression().unwrap();
        assert_eq!(p.accuraterip, attendus);
        // Les bords du disque comptent : la piste 1 sans le saut de tête ne
        // donnerait pas la même somme.
        let sans_saut = accuraterip_de_reference(&contenu_des_secteurs(0, 300), false, false);
        assert_ne!(
            (p.accuraterip[0].v1.clone(), p.accuraterip[0].v2.clone()),
            sans_saut
        );
        // Et l'évènement de fin les porte (la progression sérialisée).
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["accuraterip"][2]["piste"], 3);
        assert_eq!(json["accuraterip"][2]["v1"], attendus[2].v1);
    }

    /// #8514 — la mémoire se lit comme un lecteur : secteurs identiques, à
    /// cheval sur deux pistes, sans une lecture de plus au lecteur ; rendue,
    /// elle répond « plus de disque ».
    #[test]
    fn le_lecteur_memoire_rend_les_secteurs_sans_relire_le_disque() {
        let j = Journal::new(Duration::ZERO);
        let (_, m) = fournisseur(j.clone(), octets(900));
        let l: Arc<dyn LecteurDisque> = j.clone();
        let c = m.charger_le_disque(&l, &toc(), "d", 0).unwrap();
        assert_eq!(c.attendre_la_fin(), Some(Fin::Termine));
        let avant = j.lectures().len();
        let lm = LecteurMemoire {
            chargement: c.clone(),
        };
        assert_eq!(lm.presence(), Presence::Disque);
        assert_eq!(lm.lire_toc().unwrap(), toc());
        // 290 → 310 : la fin de la piste 1 (RAM) et le début de la piste 2
        // (fichier temporaire, au-delà du plafond).
        let mut v = vec![0u8; 20 * OCTETS_PAR_SECTEUR];
        lm.lire_secteurs(290, 20, &mut v).unwrap();
        assert_eq!(v, contenu_des_secteurs(290, 20));
        let mut tout = vec![0u8; 1_500 * OCTETS_PAR_SECTEUR];
        lm.lire_secteurs(0, 1_500, &mut tout).unwrap();
        assert_eq!(tout, contenu_des_secteurs(0, 1_500));
        assert_eq!(j.lectures().len(), avant, "le lecteur a été relu");
        m.liberer();
        assert_eq!(
            lm.lire_secteurs(0, 1, &mut v[..OCTETS_PAR_SECTEUR]),
            Err(ErreurCd::AucunDisque)
        );
    }

    /// #8522 — la base « réelle » d'un disque simulé, comme la vraie : un
    /// pressage de CRC v1 (soumissions anciennes, confiance 10 + n), un de
    /// CRC v2 (20 + n), au décalage `decalage`, et un pressage étranger.
    fn base_simulee(id: &IdentifiantAr, decalage: i32) -> Base {
        let t = toc();
        let disque = contenu_des_secteurs(0, 1_500);
        let mut lire = |lba: u32, n: u32, s: &mut [u8]| {
            let a = lba as usize * OCTETS_PAR_SECTEUR;
            s.copy_from_slice(&disque[a..a + n as usize * OCTETS_PAR_SECTEUR]);
            Ok(())
        };
        let pistes: Vec<(u8, u32, u32)> = (1..=3u8)
            .map(|n| {
                let p = t.piste(n).unwrap();
                ar::crc_decale(
                    &mut lire,
                    (p.debut, p.debut + t.secteurs(n).unwrap()),
                    (0, 1_500),
                    decalage,
                    (n == 1, n == 3),
                )
                .unwrap()
            })
            .zip(1u8..)
            .map(|((v1, v2), n)| (n, v1, v2))
            .collect();
        let en_v1: Vec<(u8, u32, u32)> = pistes.iter().map(|(n, v1, _)| (10 + n, *v1, 0)).collect();
        let en_v2: Vec<(u8, u32, u32)> = pistes.iter().map(|(n, _, v2)| (20 + n, *v2, 0)).collect();
        let autre: Vec<(u8, u32, u32)> = vec![(4, 0xDEAD, 0); 3];
        let o = crate::accuraterip_base::tests_communs::fixture(
            id,
            &[&en_v1[..], &en_v2[..], &autre[..]],
        );
        Base::Trouvee(ar::analyser(&o, id).unwrap())
    }

    fn charge_en_entier() -> (Arc<MemoireCd>, Arc<Chargement>) {
        let j = Journal::new(Duration::ZERO);
        let (_, m) = fournisseur(j.clone(), octets(900));
        let l: Arc<dyn LecteurDisque> = j.clone();
        let c = m.charger_le_disque(&l, &toc(), "d", 0).unwrap();
        assert_eq!(c.attendre_la_fin(), Some(Fin::Termine));
        (m, c)
    }

    /// #8522 — le disque en mémoire est vérifié contre la base : réglé au
    /// bon décalage, toutes les pistes « vérifiées » avec leur confiance ;
    /// réglé à 0 alors que le lecteur lit à +6, le décalage est DÉTECTÉ.
    #[test]
    fn la_verification_accuraterip_trouve_le_decalage_du_lecteur() {
        let (m, c) = charge_en_entier();
        let id = IdentifiantAr::depuis_toc(&toc());
        let base = base_simulee(&id, 6);
        let v = c.verifier_accuraterip(&base, &id, 6).unwrap();
        assert_eq!(
            (v.base, v.decalage, v.decalage_detecte),
            ("trouvee", 6, false)
        );
        assert_eq!(
            v.pistes
                .iter()
                .map(|p| (p.piste, p.statut, p.version, p.confiance))
                .collect::<Vec<_>>(),
            vec![
                (1, StatutAr::Verifie, Some("v2"), 21),
                (2, StatutAr::Verifie, Some("v2"), 22),
                (3, StatutAr::Verifie, Some("v2"), 23)
            ]
        );
        let v = c.verifier_accuraterip(&base, &id, 0).unwrap();
        assert_eq!(
            (v.decalage, v.decalage_detecte),
            (6, true),
            "décalage non détecté"
        );
        assert!(
            v.pistes
                .iter()
                .all(|p| p.statut == StatutAr::Verifie && p.decalage == 6)
        );
        m.liberer();
    }

    /// #8522 — contre-épreuves : une base qui ne connaît que d'autres CRC
    /// dit « différent » (et ne s'invente pas de décalage) ; absente,
    /// « absent » ; injoignable, « non vérifié » — jamais une erreur.
    #[test]
    fn la_verification_dit_different_absent_ou_non_verifie() {
        let (m, c) = charge_en_entier();
        let id = IdentifiantAr::depuis_toc(&toc());
        let autre = crate::accuraterip_base::tests_communs::fixture(
            &id,
            &[&[(9u8, 0xBEEFu32, 0u32); 3][..]],
        );
        let base = Base::Trouvee(ar::analyser(&autre, &id).unwrap());
        let v = c.verifier_accuraterip(&base, &id, 0).unwrap();
        assert_eq!((v.decalage, v.decalage_detecte), (0, false));
        assert!(
            v.pistes
                .iter()
                .all(|p| p.statut == StatutAr::Different && p.confiance == 9)
        );
        let v = c.verifier_accuraterip(&Base::Absente, &id, 0).unwrap();
        assert!(v.pistes.iter().all(|p| p.statut == StatutAr::Absent));
        assert_eq!(v.base, "absente");
        let v = c
            .verifier_accuraterip(&Base::Injoignable("hors ligne".into()), &id, 0)
            .unwrap();
        assert!(v.pistes.iter().all(|p| p.statut == StatutAr::NonVerifie));
        // Les CRC au décalage 0 sont ceux calculés au chargement (#8514).
        let p = c.progression();
        for (a, b) in v.pistes.iter().zip(&p.accuraterip) {
            assert_eq!((&a.v1, &a.v2), (&b.v1, &b.v2));
        }
        m.liberer();
    }

    struct SourceFixe(Vec<u8>);
    #[async_trait::async_trait]
    impl ar::SourceAr for SourceFixe {
        async fn telecharger(&self, _: &str) -> Result<Option<Vec<u8>>, String> {
            Ok(Some(self.0.clone()))
        }
    }

    /// #8522 — de bout en bout : un disque chargé en entier est vérifié EN
    /// FOND, une seule consultation de la base ; la progression le porte.
    #[tokio::test(flavor = "multi_thread")]
    async fn le_disque_charge_est_verifie_en_fond() {
        let id = IdentifiantAr::depuis_toc(&toc());
        // La base : les CRC v2 au décalage 0, en octets.
        let t = toc();
        let disque = contenu_des_secteurs(0, 1_500);
        let mut lire = |lba: u32, n: u32, s: &mut [u8]| {
            let a = lba as usize * OCTETS_PAR_SECTEUR;
            s.copy_from_slice(&disque[a..a + n as usize * OCTETS_PAR_SECTEUR]);
            Ok(())
        };
        let pistes: Vec<(u8, u32, u32)> = (1..=3u8)
            .map(|n| {
                let p = t.piste(n).unwrap();
                let (_, v2) = ar::crc_decale(
                    &mut lire,
                    (p.debut, p.debut + t.secteurs(n).unwrap()),
                    (0, 1_500),
                    0,
                    (n == 1, n == 3),
                )
                .unwrap();
                (5, v2, 0)
            })
            .collect();
        let octets_base = crate::accuraterip_base::tests_communs::fixture(&id, &[&pistes[..]]);
        let base = Arc::new(BaseAr::new(Arc::new(SourceFixe(octets_base))));
        let j = Journal::new(Duration::ZERO);
        let (_, m) = fournisseur(j.clone(), u64::MAX);
        m.verifier_avec(VerifieurAr {
            base: base.clone(),
            decalage: Arc::new(|| 0),
            bus: None,
            moteur: tokio::runtime::Handle::current(),
        });
        let l: Arc<dyn LecteurDisque> = j.clone();
        let c = m.charger_le_disque(&l, &toc(), "d", 0).unwrap();
        let c2 = c.clone();
        tokio::task::spawn_blocking(move || c2.attendre_la_fin())
            .await
            .unwrap();
        let mut v = None;
        for _ in 0..500 {
            v = c
                .progression()
                .verification_accuraterip
                .filter(|v| v.base != "en_cours");
            if v.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let v = v.expect("aucun verdict");
        assert_eq!(v.identifiant, id.nom());
        assert!(
            v.pistes
                .iter()
                .all(|p| p.statut == StatutAr::Verifie && p.confiance == 5),
            "{v:?}"
        );
        assert_eq!(base.appels(), 1);
        let json = serde_json::to_value(c.progression()).unwrap();
        assert_eq!(
            json["verification_accuraterip"]["pistes"][0]["statut"],
            "verifie"
        );
        m.liberer();
    }

    /// #8514 — la retenue se compte et se relâche en tombant, même si deux
    /// demandes se croisent.
    #[test]
    fn la_retenue_d_enregistrement_tombe_avec_sa_garde() {
        let m = Arc::new(MemoireCd::new(Reglages {
            actif: true,
            plafond_octets: u64::MAX,
        }));
        assert!(!m.enregistrement_en_cours());
        let a = m.retenir_pour_enregistrement();
        let b = m.retenir_pour_enregistrement();
        drop(a);
        assert!(m.enregistrement_en_cours(), "l'autre retenue vit encore");
        drop(b);
        assert!(!m.enregistrement_en_cours());
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
