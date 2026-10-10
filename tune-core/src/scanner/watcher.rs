use std::collections::HashMap;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use std::time::{Instant, SystemTime};

use notify::event::ModifyKind;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tracing::{debug, info, warn};

/// Fil 2148 (#5792) — le délai entre deux passages de la sonde d'un partage
/// réseau. Les moteurs natifs (FSEvents/inotify/ReadDirectoryChangesW) ne
/// reçoivent RIEN des changements faits par une autre machine sur un partage
/// SMB/NFS : la sonde va voir elle-même.
///
/// Il valait 900 s, figés, avec un parcours COMPLET de l'arbre à chaque
/// passage (Pierre M : 6 min 43 pour `K:\`) : un album copié sur le NAS
/// attendait jusqu'à un quart d'heure. Le passage ordinaire est désormais
/// incrémental (voir [`Releve::tour`]), ce qui permet un défaut de 5 min et un
/// réglage serveur, `network_poll_interval_secs`, borné de 60 s à 1 h.
pub const NETWORK_POLL_INTERVAL_KEY: &str = "network_poll_interval_secs";
/// Défaut du délai entre deux passages, en secondes.
pub const NETWORK_POLL_INTERVAL_DEFAULT: u64 = 300;
/// Plancher : en dessous d'une minute, même le passage incrémental (un `stat`
/// par dossier de premier et de second niveau) tiendrait le NAS éveillé.
pub const NETWORK_POLL_INTERVAL_FLOOR: u64 = 60;
/// Plafond : au-delà d'une heure, un album ajouté se fait attendre plus
/// longtemps qu'un rescan manuel.
pub const NETWORK_POLL_INTERVAL_CEILING: u64 = 3_600;

/// Un passage sur `RELEVE_COMPLET_PERIODE` au plus relit TOUT l'arbre : c'est
/// le filet de ce que la comparaison des dates de dossiers ne voit pas (un
/// fichier retouché sur place, un changement à plus de deux niveaux sous la
/// racine, un serveur qui ne met pas à jour la date de ses dossiers).
const RELEVE_COMPLET_PERIODE: Duration = Duration::from_secs(3_600);

/// Le délai en vigueur, partagé par toutes les sondes. Réglé au démarrage du
/// surveillant et à chaque `PATCH /system/config` qui le change : une sonde le
/// relit pendant son attente, sans redémarrage.
static INTERVALLE_RESEAU_SECS: AtomicU64 = AtomicU64::new(NETWORK_POLL_INTERVAL_DEFAULT);

/// Résout le délai depuis sa forme PERSISTÉE : illisible ⇒ le défaut, hors
/// bornes ⇒ ramené dans les bornes. Même discipline que
/// `resolve_shuffle_max_tracks`.
pub fn resolve_network_poll_interval(brut: Option<&str>) -> u64 {
    brut.map(str::trim)
        .map(|v| v.trim_matches('"').trim())
        .filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(NETWORK_POLL_INTERVAL_DEFAULT)
        .clamp(NETWORK_POLL_INTERVAL_FLOOR, NETWORK_POLL_INTERVAL_CEILING)
}

/// Valide une valeur ARRIVANTE (`PATCH /system/config`) : hors bornes ou
/// illisible, elle est refusée en nommant les bornes, jamais devinée.
pub fn valider_network_poll_interval(brut: &str) -> Result<u64, String> {
    let nettoye = brut.trim().trim_matches('"').trim();
    let Ok(n) = nettoye.parse::<u64>() else {
        return Err(format!(
            "{NETWORK_POLL_INTERVAL_KEY} : un nombre de secondes entre \
             {NETWORK_POLL_INTERVAL_FLOOR} et {NETWORK_POLL_INTERVAL_CEILING} — reçu « {brut} »"
        ));
    };
    if !(NETWORK_POLL_INTERVAL_FLOOR..=NETWORK_POLL_INTERVAL_CEILING).contains(&n) {
        return Err(format!(
            "{NETWORK_POLL_INTERVAL_KEY} = {n} : hors bornes ({NETWORK_POLL_INTERVAL_FLOOR} à \
             {NETWORK_POLL_INTERVAL_CEILING} secondes)"
        ));
    }
    Ok(n)
}

/// Applique le délai à toutes les sondes, en vigueur dès leur attente en
/// cours. Rend la valeur appliquée, ramenée dans les bornes.
pub fn regler_intervalle_reseau(secs: u64) -> u64 {
    let borne = secs.clamp(NETWORK_POLL_INTERVAL_FLOOR, NETWORK_POLL_INTERVAL_CEILING);
    INTERVALLE_RESEAU_SECS.store(borne, Ordering::Release);
    borne
}

/// Le délai en vigueur, en secondes.
pub fn intervalle_reseau() -> u64 {
    INTERVALLE_RESEAU_SECS.load(Ordering::Acquire)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeType {
    Added,
    Modified,
    Deleted,
    /// #4896 — un DOSSIER est apparu sous une racine : renommé (nouveau nom),
    /// déplacé depuis ailleurs, ou créé. Les trois moteurs natifs ne signalent
    /// que le dossier, jamais les fichiers qu'il emporte.
    DossierApparu,
    /// #4896 — un chemin qui n'est pas un fichier audio a quitté le disque :
    /// peut-être un dossier renommé (ancien nom), déplacé hors de la racine,
    /// mis à la corbeille ou supprimé. Rien ne dit ici que c'était un dossier
    /// — il n'existe plus — : `auto_scan` le décide d'après les pistes qu'il
    /// contenait, et un chemin sans piste n'y touche à rien.
    DossierDisparu,
    /// #5034 — une IMAGE DE POCHETTE de dossier (`cover.jpg`, `folder.png`…)
    /// a été créée, modifiée, renommée ou supprimée. Le surveillant ne
    /// relayait que l'audio : remplacer ou retirer le `cover.jpg` d'un album
    /// n'était vu par personne jusqu'au scan suivant — et même lui ne le
    /// voyait pas, les pistes n'ayant pas changé. Le geste exact importe peu :
    /// `auto_scan` relit l'album du dossier et laisse la règle trancher.
    ImageDePochette,
}

#[derive(Debug, Clone)]
pub struct FileChange {
    pub change_type: ChangeType,
    pub path: String,
}

pub struct FileWatcher {
    watcher: Option<RecommendedWatcher>,
    /// Les racines réseau sondées, une sonde ([`Releve`]) par racine — le
    /// moteur natif ne reçoit rien des changements faits par une autre machine
    /// sur un partage SMB/NFS. Voir [`Sondage`] : une sonde n'y entre qu'une
    /// fois son relevé initial terminé, en tâche de fond.
    sondage: Arc<Sondage>,
    event_tx: mpsc::Sender<FileChange>,
    event_rx: std::sync::Mutex<mpsc::Receiver<FileChange>>,
    /// Dirs currently watched by the native watcher.
    dirs: Vec<PathBuf>,
    /// Requested dirs not currently watched (missing/unmounted at the time).
    /// `ensure_watches` retries them so a NAS mounted after boot — or
    /// remounted after a drop — gets picked up without a restart.
    pending: Vec<PathBuf>,
}

/// Les sondes des racines réseau, partagées avec les fils qui les amorcent.
///
/// Le relevé initial d'une sonde PARCOURT TOUT L'ARBRE : il relève la date et
/// la taille de chaque fichier pour comparer les passages suivants. Sur un
/// grand partage SMB, il dure des minutes (Pierre M : 6 min 43 pour `K:\`). Il
/// se faisait dans `FileWatcher::new`, donc AVANT la boucle du surveillant :
/// tant qu'il durait, aucun changement n'était traité, même sur les racines
/// locales déjà suivies par le moteur natif, et `auto_scan` jetait ensuite
/// comme « rejoués » les événements accumulés entre-temps.
///
/// Le relevé d'une racine réseau part donc sur son propre fil : la sonde
/// n'entre ici qu'une fois prête, et les racines locales sont suivies dès la
/// création du surveillant. (Défaut relevé en instruisant le fil forum 2148 ;
/// rien n'établit que c'est lui qui a retardé le surveillant de ce testeur.)
#[derive(Default)]
struct Sondage {
    /// Sondes prêtes : leur relevé initial est fait, elles comparent.
    pretes: Mutex<Vec<(PathBuf, SondeReseau)>>,
    /// Racines dont le relevé initial est en cours sur un fil à part.
    en_amorce: Mutex<Vec<PathBuf>>,
    /// Racines dont la sonde n'a pas pu être posée : elles reviennent au
    /// moteur natif, comme avant (`poll_watch_failed`).
    repli_natif: Mutex<Vec<PathBuf>>,
    /// Posé par `stop` : une amorce qui se termine après ne pose rien.
    arrete: AtomicBool,
}

/// Une sonde de racine réseau en service. Son fil passe toutes les
/// [`intervalle_reseau`] secondes ; la lâcher arrête ce fil.
struct SondeReseau {
    arret: Arc<AtomicBool>,
}

impl Drop for SondeReseau {
    fn drop(&mut self) {
        self.arret.store(true, Ordering::Release);
    }
}

fn verrou<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Ce qu'une sonde retient d'un chemin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entree {
    Fichier {
        mtime: Option<SystemTime>,
        taille: u64,
    },
    Dossier {
        mtime: Option<SystemTime>,
    },
}

impl Entree {
    fn est_un_dossier(&self) -> bool {
        matches!(self, Entree::Dossier { .. })
    }
}

/// Ce qu'un passage de sonde a coûté et trouvé.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BilanDuTour {
    /// `read_dir` lancés.
    pub dossiers_lus: usize,
    /// `stat` lancés, fichiers et dossiers.
    pub entrees_examinees: usize,
    /// Événements émis vers le surveillant.
    pub evenements: usize,
    /// Passage complet (tout l'arbre relu) ou incrémental.
    pub complet: bool,
}

/// Fil 2148 (#5792) — le relevé d'une racine réseau, et ses passages.
///
/// `notify::PollWatcher` relisait TOUT l'arbre à chaque passage : un `stat` par
/// fichier, des centaines de milliers sur un NAS. Un passage ordinaire ne
/// relit plus que ce qui a bougé, d'après la date des dossiers : un dossier
/// change de date quand une entrée y est créée, renommée ou supprimée.
///
/// - la racine, puis chaque dossier de PREMIER niveau (l'artiste, d'ordinaire)
///   qui a changé de date : relecture de ses entrées directes, et de tout le
///   sous-arbre d'un dossier apparu ;
/// - chaque dossier de SECOND niveau (l'album) qui a changé de date : son
///   sous-arbre entier est relu.
///
/// Un dossier inchangé coûte un `stat`, et ses fichiers aucun. Ce que les
/// dates de dossiers ne disent pas — un fichier retouché sur place, un
/// changement à trois niveaux ou plus sous la racine, un serveur qui ne date
/// pas ses dossiers — attend le passage complet, une fois par
/// [`RELEVE_COMPLET_PERIODE`] au plus.
///
/// Les événements émis sont ceux du gestionnaire commun
/// ([`make_event_handler`]) : `Create`/`Remove` de fichier ou de dossier, et
/// `Modify(Data)` pour un fichier dont la date ou la taille a changé.
/// (`PollWatcher` émettait `Modify(Metadata(WriteTime))` pour ce dernier cas,
/// que le gestionnaire écarte : une retouche de balises sur le NAS n'arrivait
/// jamais.)
pub(crate) struct Releve {
    racine: PathBuf,
    mtime_racine: Option<SystemTime>,
    /// Tout ce qui est sous la racine, la racine exclue.
    entrees: BTreeMap<PathBuf, Entree>,
    /// Dossiers vus changés au passage précédent : relus une fois de plus.
    /// Certains serveurs datent à la seconde (FAT, à deux secondes) : un
    /// second changement dans la même seconde que la relecture ne changerait
    /// pas la date.
    suspects: HashSet<PathBuf>,
}

/// Lit `dossier` : ses entrées directes, ou tout son sous-arbre. `None` si un
/// `read_dir` échoue — rien n'est conclu d'une lecture partielle, qui ferait
/// passer pour supprimé ce qui n'a pas été lu. Les liens symboliques de
/// dossier ne sont pas suivis (comme [`fichiers_audio_sous`]).
fn lister(
    dossier: &Path,
    recursif: bool,
    bilan: &mut BilanDuTour,
) -> Option<BTreeMap<PathBuf, Entree>> {
    let mut trouves = BTreeMap::new();
    let mut a_lire = vec![dossier.to_path_buf()];
    while let Some(courant) = a_lire.pop() {
        bilan.dossiers_lus += 1;
        for entree in std::fs::read_dir(&courant).ok()? {
            let entree = entree.ok()?;
            let Ok(genre) = entree.file_type() else {
                continue;
            };
            let chemin = entree.path();
            bilan.entrees_examinees += 1;
            // `DirEntry::metadata` ne suit pas les liens ; `fs::metadata` si.
            let meta = if genre.is_symlink() {
                std::fs::metadata(&chemin)
            } else {
                entree.metadata()
            };
            // Disparu entre la liste et le `stat` : le passage suivant le dira.
            let Ok(meta) = meta else {
                continue;
            };
            if meta.is_dir() {
                if genre.is_symlink() {
                    continue;
                }
                trouves.insert(
                    chemin.clone(),
                    Entree::Dossier {
                        mtime: meta.modified().ok(),
                    },
                );
                if recursif {
                    a_lire.push(chemin);
                }
            } else {
                trouves.insert(
                    chemin,
                    Entree::Fichier {
                        mtime: meta.modified().ok(),
                        taille: meta.len(),
                    },
                );
            }
        }
    }
    Some(trouves)
}

fn evenement(kind: EventKind, chemin: &Path) -> Event {
    Event::new(kind).add_path(chemin.to_path_buf())
}

impl Releve {
    /// Le relevé initial : tout l'arbre. `None` si la racine ne se lit pas.
    pub(crate) fn initial(racine: &Path) -> Option<(Self, BilanDuTour)> {
        let mut bilan = BilanDuTour {
            complet: true,
            ..Default::default()
        };
        bilan.entrees_examinees += 1;
        let mtime_racine = std::fs::metadata(racine).ok()?.modified().ok();
        let entrees = lister(racine, true, &mut bilan)?;
        Some((
            Self {
                racine: racine.to_path_buf(),
                mtime_racine,
                entrees,
                suspects: HashSet::new(),
            },
            bilan,
        ))
    }

    /// Combien de chemins le relevé connaît sous la racine.
    pub(crate) fn taille(&self) -> usize {
        self.entrees.len()
    }

    /// Ce que le relevé sait sous `dossier`, `dossier` exclu.
    fn sous(&self, dossier: &Path) -> impl Iterator<Item = (&PathBuf, &Entree)> {
        use std::ops::Bound::{Excluded, Unbounded};
        self.entrees
            .range::<PathBuf, _>((Excluded(dossier.to_path_buf()), Unbounded))
            .take_while(move |(p, _)| p.starts_with(dossier))
    }

    fn mtime_connue(&self, dossier: &Path) -> Option<Option<SystemTime>> {
        if dossier == self.racine {
            return Some(self.mtime_racine);
        }
        match self.entrees.get(dossier) {
            Some(Entree::Dossier { mtime }) => Some(*mtime),
            _ => None,
        }
    }

    fn poser_mtime(&mut self, dossier: &Path, mtime: Option<SystemTime>) {
        if dossier == self.racine {
            self.mtime_racine = mtime;
        } else if let Some(Entree::Dossier { mtime: m }) = self.entrees.get_mut(dossier) {
            *m = mtime;
        }
    }

    /// Un passage : incrémental, ou complet si `complet`.
    pub(crate) fn tour(&mut self, complet: bool, emettre: &mut dyn FnMut(Event)) -> BilanDuTour {
        let mut bilan = BilanDuTour {
            complet,
            ..Default::default()
        };
        let suspects = std::mem::take(&mut self.suspects);
        let racine = self.racine.clone();
        if complet {
            bilan.entrees_examinees += 1;
            let Some(mtime) = std::fs::metadata(&racine).ok().map(|m| m.modified().ok()) else {
                return bilan;
            };
            if let Some(neuf) = lister(&racine, true, &mut bilan) {
                let ancien: BTreeMap<PathBuf, Entree> =
                    self.sous(&racine).map(|(p, e)| (p.clone(), *e)).collect();
                self.remplacer(ancien, neuf, emettre, &mut bilan);
                self.mtime_racine = mtime;
            } else {
                self.suspects = suspects;
            }
            return bilan;
        }

        // Niveau 0 puis 1 : relecture des entrées directes.
        self.verifier(&racine, &suspects, false, emettre, &mut bilan);
        let niveau = |releve: &Self, n: usize| -> Vec<PathBuf> {
            releve
                .entrees
                .iter()
                .filter(|(p, e)| {
                    e.est_un_dossier()
                        && p.strip_prefix(&releve.racine)
                            .is_ok_and(|r| r.components().count() == n)
                })
                .map(|(p, _)| p.clone())
                .collect()
        };
        for d in niveau(self, 1) {
            self.verifier(&d, &suspects, false, emettre, &mut bilan);
        }
        // Niveau 2 : le sous-arbre entier.
        for d in niveau(self, 2) {
            self.verifier(&d, &suspects, true, emettre, &mut bilan);
        }
        bilan
    }

    /// Relit `dossier` si sa date a changé (ou s'il est suspect). Rend vrai
    /// s'il a été relu.
    fn verifier(
        &mut self,
        dossier: &Path,
        suspects: &HashSet<PathBuf>,
        sous_arbre: bool,
        emettre: &mut dyn FnMut(Event),
        bilan: &mut BilanDuTour,
    ) -> bool {
        let Some(connue) = self.mtime_connue(dossier) else {
            return false;
        };
        bilan.entrees_examinees += 1;
        // Disparu ou illisible : c'est la relecture de son parent qui le dira.
        let Ok(meta) = std::fs::metadata(dossier) else {
            return false;
        };
        let mtime = meta.modified().ok();
        let a_change = mtime != connue;
        if !a_change && !suspects.contains(dossier) {
            return false;
        }
        let relu = if sous_arbre {
            self.relire_le_sous_arbre(dossier, emettre, bilan)
        } else {
            self.relire_les_entrees(dossier, emettre, bilan)
        };
        if relu {
            // La date lue AVANT la relecture : un changement pendant celle-ci
            // laisse une date plus récente, vue au passage suivant.
            self.poser_mtime(dossier, mtime);
        }
        if a_change || !relu {
            self.suspects.insert(dossier.to_path_buf());
        }
        relu
    }

    fn relire_le_sous_arbre(
        &mut self,
        dossier: &Path,
        emettre: &mut dyn FnMut(Event),
        bilan: &mut BilanDuTour,
    ) -> bool {
        let Some(neuf) = lister(dossier, true, bilan) else {
            return false;
        };
        let ancien: BTreeMap<PathBuf, Entree> =
            self.sous(dossier).map(|(p, e)| (p.clone(), *e)).collect();
        self.remplacer(ancien, neuf, emettre, bilan);
        true
    }

    /// Les entrées directes de `dossier` ; le sous-arbre d'un dossier apparu
    /// ou disparu en entier ; un dossier resté en place n'est pas relu (sa
    /// date, à lui, le dira).
    fn relire_les_entrees(
        &mut self,
        dossier: &Path,
        emettre: &mut dyn FnMut(Event),
        bilan: &mut BilanDuTour,
    ) -> bool {
        let Some(directes) = lister(dossier, false, bilan) else {
            return false;
        };
        let mut ancien: BTreeMap<PathBuf, Entree> = self
            .sous(dossier)
            .filter(|(p, _)| p.parent() == Some(dossier))
            .map(|(p, e)| (p.clone(), *e))
            .collect();
        let mut neuf = BTreeMap::new();
        for (chemin, entree) in directes {
            match (entree, ancien.get(&chemin)) {
                // Resté en place : sa date reste la sienne, pas celle que la
                // liste du parent vient de lire — sinon son propre changement
                // passerait inaperçu.
                (Entree::Dossier { .. }, Some(vieux @ Entree::Dossier { .. })) => {
                    neuf.insert(chemin, *vieux);
                }
                (Entree::Dossier { .. }, _) => {
                    let Some(dessous) = lister(&chemin, true, bilan) else {
                        return false;
                    };
                    neuf.insert(chemin, entree);
                    neuf.extend(dessous);
                }
                (Entree::Fichier { .. }, _) => {
                    neuf.insert(chemin, entree);
                }
            }
        }
        let disparus: Vec<PathBuf> = ancien
            .iter()
            .filter(|(p, e)| {
                e.est_un_dossier() && !matches!(neuf.get(*p), Some(Entree::Dossier { .. }))
            })
            .map(|(p, _)| p.clone())
            .collect();
        for d in disparus {
            let dessous: Vec<(PathBuf, Entree)> =
                self.sous(&d).map(|(p, e)| (p.clone(), *e)).collect();
            ancien.extend(dessous);
        }
        self.remplacer(ancien, neuf, emettre, bilan);
        true
    }

    /// Remplace `ancien` par `neuf` dans le relevé et émet la différence.
    fn remplacer(
        &mut self,
        ancien: BTreeMap<PathBuf, Entree>,
        neuf: BTreeMap<PathBuf, Entree>,
        emettre: &mut dyn FnMut(Event),
        bilan: &mut BilanDuTour,
    ) {
        use notify::event::{CreateKind, DataChange, RemoveKind};
        let mut emettre = |e: Event| {
            bilan.evenements += 1;
            emettre(e);
        };
        let retrait = |e: &Entree| {
            EventKind::Remove(if e.est_un_dossier() {
                RemoveKind::Folder
            } else {
                RemoveKind::File
            })
        };
        let creation = |e: &Entree| {
            EventKind::Create(if e.est_un_dossier() {
                CreateKind::Folder
            } else {
                CreateKind::File
            })
        };
        for (chemin, vieux) in &ancien {
            match neuf.get(chemin) {
                None => emettre(evenement(retrait(vieux), chemin)),
                Some(e) if e.est_un_dossier() != vieux.est_un_dossier() => {
                    emettre(evenement(retrait(vieux), chemin));
                }
                _ => {}
            }
        }
        for (chemin, entree) in &neuf {
            match ancien.get(chemin) {
                None => emettre(evenement(creation(entree), chemin)),
                Some(vieux) if vieux.est_un_dossier() != entree.est_un_dossier() => {
                    emettre(evenement(creation(entree), chemin));
                }
                Some(vieux @ Entree::Fichier { .. }) if vieux != entree => emettre(evenement(
                    EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                    chemin,
                )),
                _ => {}
            }
        }
        for chemin in ancien.keys() {
            if !neuf.contains_key(chemin) {
                self.entrees.remove(chemin);
            }
        }
        self.entrees.extend(neuf);
    }
}

/// Épreuves : simuler une racine réseau sur un système de fichiers local, et
/// retenir son relevé initial aussi longtemps qu'on veut.
#[cfg(test)]
mod simulation_reseau {
    use std::path::{Path, PathBuf};
    use std::sync::{Condvar, Mutex};

    pub(super) static RACINES: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    pub(super) static RETENUES: (Mutex<Vec<PathBuf>>, Condvar) =
        (Mutex::new(Vec::new()), Condvar::new());

    pub(super) fn simulee(dir: &Path) -> bool {
        RACINES.lock().unwrap().iter().any(|r| r == dir)
    }

    pub(super) fn attendre_la_liberation(dir: &Path) {
        let (verrou, signal) = &RETENUES;
        let mut retenues = verrou.lock().unwrap();
        while retenues.iter().any(|r| r == dir) {
            retenues = signal.wait(retenues).unwrap();
        }
    }

    pub(super) fn liberer(dir: &Path) {
        let (verrou, signal) = &RETENUES;
        verrou.lock().unwrap().retain(|r| r != dir);
        signal.notify_all();
    }
}

fn est_une_racine_reseau(dir: &Path) -> bool {
    #[cfg(test)]
    if simulation_reseau::simulee(dir) {
        return true;
    }
    is_network_path(dir)
}

/// Le relevé initial d'une racine réseau, sur son propre fil (voir
/// [`Sondage`]), puis ses passages sur ce même fil. Le relevé peut durer des
/// heures sur un grand partage SMB.
fn amorcer_la_sonde(sondage: Arc<Sondage>, event_tx: mpsc::Sender<FileChange>, dir: PathBuf) {
    let fil = std::thread::Builder::new()
        .name("tune-sonde-reseau".into())
        .spawn({
            let sondage = sondage.clone();
            let dir = dir.clone();
            move || {
                #[cfg(test)]
                simulation_reseau::attendre_la_liberation(&dir);
                let debut = Instant::now();
                let releve = Releve::initial(&dir);
                verrou(&sondage.en_amorce).retain(|d| d != &dir);
                match releve {
                    Some((releve, bilan)) if !sondage.arrete.load(Ordering::Acquire) => {
                        info!(
                            dir = %dir.display(),
                            interval_secs = intervalle_reseau(),
                            releve_ms = debut.elapsed().as_millis() as u64,
                            entrees = releve.taille(),
                            dossiers_lus = bilan.dossiers_lus,
                            "watching_directory_poll — network mount, incremental polling"
                        );
                        let arret = Arc::new(AtomicBool::new(false));
                        verrou(&sondage.pretes).push((
                            dir.clone(),
                            SondeReseau {
                                arret: arret.clone(),
                            },
                        ));
                        sonder(releve, &arret, &sondage, make_event_handler(event_tx));
                    }
                    // Surveillant arrêté pendant le relevé : rien n'est posé.
                    Some(_) => {}
                    None => {
                        warn!(dir = %dir.display(), "poll_watch_failed — root unreadable, falling back to native watch");
                        verrou(&sondage.repli_natif).push(dir);
                    }
                }
            }
        });
    if let Err(e) = fil {
        warn!(dir = %dir.display(), error = %e, "poll_watch_failed — falling back to native watch");
        verrou(&sondage.en_amorce).retain(|d| d != &dir);
        verrou(&sondage.repli_natif).push(dir);
    }
}

/// Les passages d'une sonde, jusqu'à ce qu'on la lâche ou que le surveillant
/// s'arrête. Le délai est relu pendant l'attente : un nouveau réglage vaut
/// dès l'attente en cours.
fn sonder(
    mut releve: Releve,
    arret: &AtomicBool,
    sondage: &Sondage,
    gestionnaire: impl Fn(Result<Event, notify::Error>),
) {
    let mut dernier_complet = Instant::now();
    loop {
        let attente = Instant::now();
        loop {
            if arret.load(Ordering::Acquire) || sondage.arrete.load(Ordering::Acquire) {
                return;
            }
            if attente.elapsed() >= Duration::from_secs(intervalle_reseau()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        let complet = dernier_complet.elapsed() >= RELEVE_COMPLET_PERIODE;
        let debut = Instant::now();
        let bilan = releve.tour(complet, &mut |e| gestionnaire(Ok(e)));
        if complet {
            dernier_complet = Instant::now();
        }
        let duree_ms = debut.elapsed().as_millis() as u64;
        if bilan.complet || bilan.evenements > 0 {
            info!(
                dir = %releve.racine.display(),
                complet = bilan.complet,
                duree_ms,
                dossiers_lus = bilan.dossiers_lus,
                entrees_examinees = bilan.entrees_examinees,
                evenements = bilan.evenements,
                "network_poll_pass"
            );
        } else {
            debug!(
                dir = %releve.racine.display(),
                duree_ms,
                entrees_examinees = bilan.entrees_examinees,
                "network_poll_pass_unchanged"
            );
        }
    }
}

/// Shared notify event handler: translate raw events into FileChange messages.
fn make_event_handler(event_tx: mpsc::Sender<FileChange>) -> impl Fn(Result<Event, notify::Error>) {
    move |res: Result<Event, notify::Error>| match res {
        Ok(event) => {
            let change_type = match event.kind {
                EventKind::Create(_) => Some(ChangeType::Added),
                // Only treat data/content changes and renames as
                // modifications.  Ignore metadata-only changes
                // (xattr, Finder info, inode meta) — on macOS,
                // Spotlight indexing writes extended attributes to
                // audio files after they are read, which fires
                // Modify(Metadata(Extended)) events.  Treating
                // those as content changes creates an infinite
                // read→xattr→event→read loop (seen on Ventura).
                EventKind::Modify(ModifyKind::Data(_))
                | EventKind::Modify(ModifyKind::Name(_))
                | EventKind::Modify(ModifyKind::Any) => Some(ChangeType::Modified),
                EventKind::Modify(ModifyKind::Metadata(_))
                | EventKind::Modify(ModifyKind::Other) => None,
                EventKind::Remove(_) => Some(ChangeType::Deleted),
                _ => None,
            };

            if let Some(ct) = change_type {
                for path in &event.paths {
                    // #5073 — la feuille CUE aussi : c'est elle qui découpe
                    // son FLAC, et `auto_scan` relit alors son dossier.
                    // #5299 — l'image `.iso` aussi : `auto_scan` en déplie
                    // les fichiers audio.
                    if (is_audio_file(path) || est_une_feuille_cue(path) || est_une_image_iso(path))
                        && !super::is_tune_temp_file(path)
                    {
                        let _ = event_tx.send(FileChange {
                            change_type: genre_d_un_fichier(&event.kind, path, &ct),
                            path: path.to_string_lossy().to_string(),
                        });
                    }
                }
                // #5168 — un rapport de plage dynamique (`foo_dr.txt` et ses
                // cousins, tout `.txt`) posé ou réécrit : réveiller le
                // rattrapage des rapports pour CE dossier. Les pistes n'ont
                // pas changé, le scan n'a donc rien à relire — c'est la passe
                // `taches_de_fond::rapports_dr` qui lit le rapport.
                if ct != ChangeType::Deleted {
                    for path in &event.paths {
                        if crate::metadata::foo_dr::peut_etre_un_rapport(path)
                            && let Some(dossier) = path.parent()
                        {
                            crate::taches_de_fond::rapports_dr::signaler_le_dossier(
                                dossier.to_path_buf(),
                            );
                        }
                    }
                }
            }
            // #4896 — les événements de DOSSIER. Ils étaient tous écartés par
            // le filtre audio ci-dessus : un dossier d'album renommé ou mis à
            // la corbeille n'était vu qu'au scan suivant. Un montage ou un
            // démontage (FSEvents, info « mount ») n'en est pas un : la
            // reprise d'une racine est l'affaire de `ensure_watches`.
            if event.info() == Some("mount") {
                return;
            }
            for path in &event.paths {
                if is_audio_file(path)
                    || est_une_feuille_cue(path)
                    || est_une_image_iso(path)
                    || super::is_tune_temp_file(path)
                {
                    continue;
                }
                // #5034 — une image de pochette n'est pas un dossier : sous
                // Windows, sa suppression (`Remove(Any)`) passerait sinon pour
                // un dossier disparu.
                if crate::library::pochette_disque::est_une_image_de_pochette(path) {
                    let _ = event_tx.send(FileChange {
                        change_type: ChangeType::ImageDePochette,
                        path: path.to_string_lossy().to_string(),
                    });
                    continue;
                }
                if let Some(genre) = evenement_de_dossier(&event.kind, path) {
                    let _ = event_tx.send(FileChange {
                        change_type: genre,
                        path: path.to_string_lossy().to_string(),
                    });
                }
            }
        }
        Err(e) => {
            warn!(error = %e, "watcher_error");
        }
    }
}

/// #4896 — le genre d'un événement portant sur un fichier audio (ou une
/// feuille CUE).
///
/// Un RENOMMAGE (`Modify(Name)`) arrive pour l'ancien nom comme pour le
/// nouveau — Windows `Name(From)`/`Name(To)`, macOS `Name(Any)` deux fois,
/// Linux `Name(From)`/`Name(To)`/`Name(Both)`. Il était traduit en
/// `Modified` des deux côtés ; or l'ancien nom n'existe plus : l'attente
/// d'écriture stable le jetait comme transitoire, et sa ligne restait en base
/// jusqu'au scan suivant, sous un chemin mort, à côté de la ligne du nouveau
/// nom. Sous macOS, mettre UN fichier à la corbeille est aussi un
/// `Name(Any)` : il ne disparaissait pas davantage. Un nom qui n'existe plus
/// à l'arrivée de l'événement est donc une DISPARITION ; `auto_scan` apparie
/// ensuite ancien et nouveau nom quand c'est le même fichier.
fn genre_d_un_fichier(genre: &EventKind, chemin: &Path, traduit: &ChangeType) -> ChangeType {
    if matches!(genre, EventKind::Modify(ModifyKind::Name(_)))
        && std::fs::symlink_metadata(chemin).is_err()
    {
        ChangeType::Deleted
    } else {
        traduit.clone()
    }
}

/// #4896 — ce qu'un événement dit d'un chemin qui n'est pas un fichier audio.
///
/// Les trois moteurs natifs de `notify` 7.0.0 ne décrivent pas un dossier de
/// la même façon, et deux d'entre eux ne disent même pas que c'en est un :
///
/// | geste | Windows (`windows.rs`) | macOS (`fsevent.rs`) | Linux (`inotify.rs`) |
/// |---|---|---|---|
/// | renommer sur place | `Name(From)` ancien, `Name(To)` nouveau | `Name(Any)` ancien, `Name(Any)` nouveau | `Name(From)`, `Name(To)`, `Name(Both)` [ancien, nouveau] |
/// | déplacer sous la racine | `Remove(Any)` ancien, `Create(Any)` nouveau | comme renommer | comme renommer |
/// | corbeille / sortie de la racine | `Remove(Any)` | `Name(Any)` | `Name(From)` |
/// | supprimer | `Remove(Any)` par fichier puis dossier | `Remove(File)`… puis `Remove(Folder)` | idem |
/// | entrer dans la racine | `Create(Any)` | `Name(Any)` | `Name(To)` |
///
/// Le seul arbitre commun est donc le DISQUE, lu à l'arrivée de l'événement :
/// un chemin qui est un dossier est apparu, un chemin qui n'existe plus a
/// disparu. Un chemin toujours présent qui n'est pas un dossier (pochette,
/// fichier temporaire d'un éditeur de balises) ne dit rien. Un « disparu »
/// n'était peut-être qu'un fichier : `auto_scan` n'y touche que s'il couvrait
/// des pistes indexées.
fn evenement_de_dossier(genre: &EventKind, chemin: &Path) -> Option<ChangeType> {
    use notify::event::{CreateKind, RemoveKind};
    let disparu = || std::fs::symlink_metadata(chemin).is_err();
    match genre {
        // Le moteur a dit « fichier » : ce n'est pas un dossier.
        EventKind::Create(CreateKind::File) | EventKind::Remove(RemoveKind::File) => None,
        EventKind::Create(_) => chemin.is_dir().then_some(ChangeType::DossierApparu),
        EventKind::Remove(_) => disparu().then_some(ChangeType::DossierDisparu),
        EventKind::Modify(ModifyKind::Name(_)) => {
            if chemin.is_dir() {
                Some(ChangeType::DossierApparu)
            } else if disparu() {
                Some(ChangeType::DossierDisparu)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// #4896 — les fichiers audio d'un dossier apparu, à toute profondeur, et
/// ses images `.iso` (#5299) : un
/// dossier renommé ou déplacé n'amène aucun événement pour son contenu. Les
/// liens symboliques de DOSSIER ne sont pas suivis (une boucle ne se parcourt
/// pas) ; un fichier illisible est simplement absent de la liste.
pub fn fichiers_audio_sous(dossier: &Path) -> Vec<String> {
    let mut trouves = Vec::new();
    let mut a_lire = vec![dossier.to_path_buf()];
    while let Some(courant) = a_lire.pop() {
        let Ok(entrees) = std::fs::read_dir(&courant) else {
            continue;
        };
        for entree in entrees.flatten() {
            let Ok(genre) = entree.file_type() else {
                continue;
            };
            let chemin = entree.path();
            if genre.is_dir() {
                a_lire.push(chemin);
            } else if (is_audio_file(&chemin) || est_une_image_iso(&chemin))
                && !super::is_tune_temp_file(&chemin)
            {
                trouves.push(chemin.to_string_lossy().to_string());
            }
        }
    }
    trouves.sort();
    trouves
}

/// Le moteur `notify` que ce module traduit, pour que les épreuves des autres
/// caisses fabriquent ses événements sans en dépendre elles-mêmes.
pub use notify;

/// Rejoue des événements `notify` BRUTS dans le gestionnaire de production,
/// puis les fusionne comme `poll_debounced` : le dernier événement d'un chemin
/// l'emporte. Sert aux épreuves du surveillant (#4896) : le gestionnaire est
/// privé, et une épreuve qui recopierait sa traduction ne garderait qu'une
/// copie. (La fusion, trois lignes, est celle de `poll_debounced`, que ce
/// correctif ne touche pas.)
pub fn rejouer_evenements_notify(evenements: Vec<Event>) -> Vec<FileChange> {
    let (tx, rx) = mpsc::channel();
    let gestionnaire = make_event_handler(tx);
    for e in evenements {
        gestionnaire(Ok(e));
    }
    let mut fusion: HashMap<String, ChangeType> = HashMap::new();
    while let Ok(c) = rx.try_recv() {
        fusion.insert(c.path, c.change_type);
    }
    let mut changes: Vec<FileChange> = fusion
        .into_iter()
        .map(|(path, change_type)| FileChange { change_type, path })
        .collect();
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes
}

impl FileWatcher {
    pub fn new(dirs: Vec<String>) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();

        let watcher = notify::recommended_watcher(make_event_handler(tx.clone()))
            .map_err(|e| format!("watcher init: {e}"))?;

        // Normalize like every other consumer of music_dirs (trailing slashes,
        // Windows separators) — the raw settings values were passed through
        // before, so a dir stored as "D:/Musique/" was watched under a path
        // spelling the rest of the pipeline never uses.
        let requested: Vec<PathBuf> = dirs
            .iter()
            .map(|d| PathBuf::from(super::walker::normalize_path(d)))
            .filter(|p| !p.as_os_str().is_empty())
            .collect();

        let mut this = Self {
            watcher: Some(watcher),
            sondage: Arc::new(Sondage::default()),
            event_tx: tx,
            event_rx: std::sync::Mutex::new(rx),
            dirs: Vec::new(),
            pending: requested.clone(),
        };
        this.ensure_watches();

        // Une racine réseau dont le relevé est en cours compte comme suivie :
        // elle le sera dès que sa sonde sera prête.
        let sondees =
            !verrou(&this.sondage.pretes).is_empty() || !verrou(&this.sondage.en_amorce).is_empty();
        if this.dirs.is_empty() && !sondees && !requested.is_empty() {
            return Err("no music directory could be watched".to_string());
        }
        Ok(this)
    }

    /// Les racines réseau dont la sonde est prête (relevé initial terminé).
    pub fn racines_sondees(&self) -> Vec<PathBuf> {
        verrou(&self.sondage.pretes)
            .iter()
            .map(|(d, _)| d.clone())
            .collect()
    }

    /// Try to watch every pending dir, and detect watched dirs whose mount
    /// vanished. Called at startup and periodically from the watch loop, so a
    /// NAS mounted late — or remounted after a drop — resumes live updates
    /// without a server restart. Watch per-directory, resiliently: one
    /// unreadable or unmounted dir must not kill watching for the others (it
    /// aborted the whole watcher before).
    pub fn ensure_watches(&mut self) {
        // Watched dirs whose mount disappeared go back to pending; their
        // native watch is dead even if the mount comes back under the path.
        let mut still_watched = Vec::new();
        for dir in std::mem::take(&mut self.dirs) {
            if std::fs::read_dir(&dir).is_ok() {
                still_watched.push(dir);
            } else {
                warn!(dir = %dir.display(), "watch_dir_lost — unmounted or unreadable, will re-watch when it returns");
                if let Some(w) = self.watcher.as_mut() {
                    let _ = w.unwatch(&dir);
                }
                self.pending.push(dir);
            }
        }
        self.dirs = still_watched;
        {
            let mut pretes = verrou(&self.sondage.pretes);
            let mut still_polled = Vec::new();
            for (dir, sonde) in std::mem::take(&mut *pretes) {
                if std::fs::read_dir(&dir).is_ok() {
                    still_polled.push((dir, sonde));
                } else {
                    warn!(dir = %dir.display(), "watch_dir_lost — unmounted or unreadable, will re-watch when it returns");
                    drop(sonde);
                    self.pending.push(dir);
                }
            }
            *pretes = still_polled;
        }

        // Une sonde qui n'a pas pu être posée : la racine revient au moteur
        // natif, sans repasser par la sonde.
        let repli: Vec<PathBuf> = std::mem::take(&mut *verrou(&self.sondage.repli_natif));

        // Retry pending dirs.
        let a_reprendre: Vec<(PathBuf, bool)> = std::mem::take(&mut self.pending)
            .into_iter()
            .map(|d| (d, false))
            .chain(repli.into_iter().map(|d| (d, true)))
            .collect();
        for (dir, natif_impose) in a_reprendre {
            if std::fs::read_dir(&dir).is_err() {
                self.pending.push(dir);
                continue;
            }
            if !natif_impose && est_une_racine_reseau(&dir) {
                // Native backends receive no events for changes made by other
                // machines on an SMB/NFS share — poll instead. Le relevé
                // initial de la sonde part sur son propre fil (fil 2148) :
                // il ne retient ni la création du surveillant ni sa boucle.
                let mut en_amorce = verrou(&self.sondage.en_amorce);
                if !en_amorce.contains(&dir) {
                    en_amorce.push(dir.clone());
                    drop(en_amorce);
                    info!(dir = %dir.display(), "watching_directory_poll_amorce — network mount, baseline walk in background");
                    amorcer_la_sonde(self.sondage.clone(), self.event_tx.clone(), dir);
                }
                continue;
            }
            if let Some(w) = self.watcher.as_mut() {
                match w.watch(&dir, RecursiveMode::Recursive) {
                    Ok(()) => {
                        info!(dir = %dir.display(), "watching_directory");
                        self.dirs.push(dir);
                    }
                    Err(e) => {
                        warn!(dir = %dir.display(), error = %e, "watch_dir_failed — skipping, other dirs still watched");
                        self.pending.push(dir);
                    }
                }
            }
        }
    }

    pub fn poll_changes(&self, timeout: Duration) -> Vec<FileChange> {
        let rx = self.event_rx.lock().unwrap();
        let mut changes = Vec::new();
        match rx.recv_timeout(timeout) {
            Ok(change) => {
                changes.push(change);
                while let Ok(c) = rx.try_recv() {
                    changes.push(c);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                debug!("watcher_channel_disconnected");
            }
        }
        changes
    }

    pub fn poll_debounced(&self, timeout: Duration, debounce: Duration) -> Vec<FileChange> {
        let raw = self.poll_changes(timeout);
        if raw.is_empty() {
            return raw;
        }

        std::thread::sleep(debounce);

        let rx = self.event_rx.lock().unwrap();
        let mut more = Vec::new();
        while let Ok(c) = rx.try_recv() {
            more.push(c);
        }

        let mut merged: HashMap<String, ChangeType> = HashMap::new();
        for change in raw.into_iter().chain(more) {
            merged.insert(change.path.clone(), change.change_type);
        }

        merged
            .into_iter()
            .map(|(path, change_type)| FileChange { change_type, path })
            .collect()
    }

    pub fn stop(&mut self) {
        if let Some(mut w) = self.watcher.take() {
            for dir in &self.dirs {
                let _ = w.unwatch(dir);
            }
        }
        self.sondage.arrete.store(true, Ordering::Release);
        // Lâcher une sonde arrête son fil.
        verrou(&self.sondage.pretes).clear();
        info!("file_watcher_stopped");
    }
}

impl Drop for FileWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Whether a path lives on a network filesystem (SMB/CIFS/NFS/WebDAV/AFP,
/// FUSE-backed remotes, Windows UNC or mapped network drives). Native watch
/// backends are deaf to remote changes on those — the caller polls instead.
#[cfg(target_os = "macos")]
fn is_network_path(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(cpath) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(cpath.as_ptr(), &mut buf) } != 0 {
        return false;
    }
    let fstype = unsafe { std::ffi::CStr::from_ptr(buf.f_fstypename.as_ptr()) };
    let fstype = fstype.to_string_lossy().to_lowercase();
    matches!(
        fstype.as_str(),
        "smbfs" | "nfs" | "afpfs" | "webdav" | "cifs"
    ) || fstype.starts_with("fuse")
}

#[cfg(target_os = "linux")]
fn is_network_path(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(cpath) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(cpath.as_ptr(), &mut buf) } != 0 {
        return false;
    }
    // Magic numbers from linux/magic.h.
    const NFS_SUPER_MAGIC: i64 = 0x6969;
    const SMB_SUPER_MAGIC: i64 = 0x517B;
    const SMB2_MAGIC_NUMBER: i64 = 0xFE534D42;
    const CIFS_MAGIC_NUMBER: i64 = 0xFF534D42;
    const FUSE_SUPER_MAGIC: i64 = 0x65735546;
    const NCP_SUPER_MAGIC: i64 = 0x564C;
    const CODA_SUPER_MAGIC: i64 = 0x73757245;
    matches!(
        buf.f_type as i64,
        NFS_SUPER_MAGIC
            | SMB_SUPER_MAGIC
            | SMB2_MAGIC_NUMBER
            | CIFS_MAGIC_NUMBER
            | FUSE_SUPER_MAGIC
            | NCP_SUPER_MAGIC
            | CODA_SUPER_MAGIC
    )
}

#[cfg(windows)]
fn is_network_path(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;
    let s = path.as_os_str().to_string_lossy();
    // UNC share: \\server\share\...
    if s.starts_with("\\\\") {
        return true;
    }
    // Mapped drive letter: ask Windows for the drive type of "X:\".
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetDriveTypeW(lp_root_path_name: *const u16) -> u32;
        }
        const DRIVE_REMOTE: u32 = 4;
        let root: Vec<u16> = std::ffi::OsString::from(format!("{}:\\", s.chars().next().unwrap()))
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        return unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_REMOTE;
    }
    false
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn is_network_path(_path: &Path) -> bool {
    false
}

/// #5073 — une feuille CUE (`.cue`, toute casse). Le surveillant la relaie
/// comme un fichier : sans elle, un album « image + feuille » déposé Tune
/// lancé était importé en UNE piste, le découpage n'ayant lieu qu'au scan.
pub fn est_une_feuille_cue(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("cue"))
}

/// #5299 — une image `.iso` (toute casse). Le surveillant la relaie comme un
/// fichier : `auto_scan` déplie une image de DONNÉES en ses fichiers audio,
/// sous leur chemin virtuel (`image.iso!/dossier/piste.flac`). Une image SACD
/// n'est pas dépliée là : elle attend le scan, comme avant.
pub fn est_une_image_iso(path: &Path) -> bool {
    crate::audio::iso9660::est_extension_iso(path)
}

fn is_audio_file(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        let ext = e.to_lowercase();
        // Single source of truth with the walker. "iso" is excluded here:
        // ISO SACD requires the DSF-extraction step that only the full
        // directory walk performs — a raw .iso fed to the watcher pipeline
        // would just fail tag reading. (The old duplicated list had already
        // drifted and was missing "iso" only by accident.)
        ext != "iso" && super::walker::SUPPORTED_EXTENSIONS.contains(&ext.as_str())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[test]
    fn audio_file_detection() {
        assert!(is_audio_file(Path::new("test.flac")));
        assert!(is_audio_file(Path::new("test.MP3")));
        assert!(is_audio_file(Path::new("/path/to/file.dsf")));
        assert!(!is_audio_file(Path::new("readme.txt")));
        assert!(!is_audio_file(Path::new("cover.jpg")));
    }

    /// Rejoue des événements `notify` bruts dans le gestionnaire de
    /// production, puis les fusionne comme `poll_debounced` : le dernier
    /// événement d'un chemin l'emporte.
    fn rejouer(evenements: Vec<Event>) -> HashMap<String, ChangeType> {
        let (tx, rx) = mpsc::channel();
        let gestionnaire = make_event_handler(tx);
        for e in evenements {
            gestionnaire(Ok(e));
        }
        let mut fusion = HashMap::new();
        while let Ok(c) = rx.try_recv() {
            fusion.insert(c.path, c.change_type);
        }
        fusion
    }

    fn ev(kind: EventKind, chemin: &str) -> Event {
        Event::new(kind).add_path(PathBuf::from(chemin))
    }

    /// #4896 (Didier, fil 1904) — les séquences que le moteur Windows de
    /// `notify` 7.0.0 fabrique (`src/windows.rs`, `handle_event`) :
    /// `FILE_ACTION_MODIFIED` → `Modify(Any)`, `ADDED` → `Create(Any)`,
    /// `REMOVED` → `Remove(Any)`, `RENAMED_OLD_NAME`/`NEW_NAME` →
    /// `Modify(Name(From/To))`. Aucune n'est perdue pour le fichier audio :
    /// la retouche Mp3tag parvient bien jusqu'à `auto_scan`. Le dernier cas
    /// arrive en `Added` sur un chemin DÉJÀ indexé, que `auto_scan` doit
    /// traiter comme un remplacement (`reimporter_fichier_surveillant`).
    #[test]
    fn les_sequences_windows_d_une_retouche_atteignent_le_fichier_audio_4896() {
        use notify::event::{CreateKind, RemoveKind, RenameMode};
        let x = r"D:\Musique\Pink Floyd\Multichannel 7.1\01 - Speak To Me.flac";
        // Écriture en place (FLAC au remplissage suffisant).
        let en_place = rejouer(vec![ev(EventKind::Modify(ModifyKind::Any), x)]);
        assert_eq!(en_place.get(x), Some(&ChangeType::Modified));
        // Fichier temporaire du même dossier, puis renommage par-dessus. Le
        // disque tel que le moteur le laisse : le FLAC réécrit est en place,
        // le temporaire n'existe plus — un nom de renommage absent du disque
        // serait une disparition (`genre_d_un_fichier`).
        let scene = crate::test_scratch::scratch_dir_in(
            std::env::current_dir().unwrap(),
            "watcher-retouche-par-renommage-4896",
        );
        let x_reel = scene.join("01 - Speak To Me.flac");
        let tmp_reel = scene.join("01 - Speak To Me.tmp");
        fs::write(&x_reel, b"x").unwrap();
        let (x_reel, tmp_reel) = (
            x_reel.to_string_lossy().into_owned(),
            tmp_reel.to_string_lossy().into_owned(),
        );
        let par_renommage = rejouer(vec![
            ev(EventKind::Create(CreateKind::Any), &tmp_reel),
            ev(EventKind::Modify(ModifyKind::Any), &tmp_reel),
            ev(EventKind::Remove(RemoveKind::Any), &x_reel),
            ev(
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                &tmp_reel,
            ),
            ev(EventKind::Modify(ModifyKind::Name(RenameMode::To)), &x_reel),
        ]);
        assert_eq!(par_renommage.get(&x_reel), Some(&ChangeType::Modified));
        // Le temporaire n'est pas un changement de FICHIER audio. Disparu du
        // disque, il peut sortir en candidat « dossier disparu » : `auto_scan`
        // l'écarte faute de piste indexée sous ce chemin (#4896,
        // `un_chemin_disparu_sans_piste_ne_touche_a_rien_4896`).
        assert!(
            !matches!(
                par_renommage.get(&tmp_reel),
                Some(ChangeType::Added | ChangeType::Modified | ChangeType::Deleted)
            ),
            "le temporaire est filtré"
        );
        // Remplacement par déplacement depuis un autre dossier : REMOVED puis
        // ADDED, sans MODIFIED.
        let par_deplacement = rejouer(vec![
            ev(EventKind::Remove(RemoveKind::Any), x),
            ev(EventKind::Create(CreateKind::Any), x),
        ]);
        assert_eq!(par_deplacement.get(x), Some(&ChangeType::Added));
    }

    /// Un dossier d'album APRÈS son renommage : l'ancien nom n'existe plus, le
    /// nouveau porte ses fichiers. C'est l'état du disque quand `notify` livre
    /// les événements. Racine sous le dossier courant : `is_tune_temp_file`
    /// écarte tout ce qui vit sous le dossier temporaire du système.
    fn scene_renommee(etiquette: &str) -> (crate::test_scratch::ScratchDir, PathBuf, PathBuf) {
        let racine = crate::test_scratch::scratch_dir_in(
            std::env::current_dir().unwrap(),
            &format!("watcher-dossiers-4896-{etiquette}"),
        );
        let ancien = racine.join("Pink Floyd").join("Multichannel 7.1");
        let nouveau = racine
            .join("Pink Floyd")
            .join("1973 - The Dark Side Of The Moon");
        fs::create_dir_all(&nouveau).unwrap();
        fs::write(nouveau.join("01 - Speak To Me.flac"), b"x").unwrap();
        (racine, ancien, nouveau)
    }

    fn genres(changes: &[FileChange]) -> HashMap<String, ChangeType> {
        changes
            .iter()
            .map(|c| (c.path.clone(), c.change_type.clone()))
            .collect()
    }

    fn evp(kind: EventKind, chemin: &Path) -> Event {
        Event::new(kind).add_path(chemin.to_path_buf())
    }

    /// #4896 — un dossier d'album RENOMMÉ ou DÉPLACÉ sous la racine, tel que
    /// chacun des trois moteurs natifs de `notify` 7.0.0 le livre (voir
    /// `evenement_de_dossier`). Avant le correctif, toutes ces séquences
    /// étaient perdues : aucun des deux chemins n'a d'extension audio.
    #[test]
    fn un_dossier_renomme_sort_en_disparu_puis_apparu_sur_les_trois_moteurs_4896() {
        use notify::event::{CreateKind, RemoveKind, RenameMode};
        let (_racine, ancien, nouveau) = scene_renommee("renomme");
        let nom = |m| EventKind::Modify(ModifyKind::Name(m));
        let sequences: Vec<(&str, Vec<Event>)> = vec![
            (
                "Windows, renommage sur place (RENAMED_OLD_NAME/NEW_NAME)",
                vec![
                    evp(nom(RenameMode::From), &ancien),
                    evp(nom(RenameMode::To), &nouveau),
                ],
            ),
            (
                "Windows, déplacement vers un autre parent (REMOVED/ADDED)",
                vec![
                    evp(EventKind::Remove(RemoveKind::Any), &ancien),
                    evp(EventKind::Create(CreateKind::Any), &nouveau),
                ],
            ),
            (
                "macOS FSEvents (ItemRenamed sur chaque nom)",
                vec![
                    evp(nom(RenameMode::Any), &ancien),
                    evp(nom(RenameMode::Any), &nouveau),
                ],
            ),
            (
                "Linux inotify (MOVED_FROM, MOVED_TO, paire, MOVE_SELF)",
                vec![
                    evp(nom(RenameMode::From), &ancien),
                    evp(nom(RenameMode::To), &nouveau),
                    Event::new(nom(RenameMode::Both))
                        .add_path(ancien.clone())
                        .add_path(nouveau.clone()),
                    evp(nom(RenameMode::From), &ancien),
                ],
            ),
            (
                "PollWatcher (partage réseau) : disparition puis création",
                vec![
                    evp(EventKind::Remove(RemoveKind::Any), &ancien),
                    evp(EventKind::Create(CreateKind::Any), &nouveau),
                    evp(
                        EventKind::Create(CreateKind::Any),
                        &nouveau.join("01 - Speak To Me.flac"),
                    ),
                ],
            ),
        ];
        for (moteur, evenements) in sequences {
            let vus = genres(&rejouer_evenements_notify(evenements));
            assert_eq!(
                vus.get(&*ancien.to_string_lossy()),
                Some(&ChangeType::DossierDisparu),
                "{moteur} : l'ancien nom doit sortir en « dossier disparu »"
            );
            assert_eq!(
                vus.get(&*nouveau.to_string_lossy()),
                Some(&ChangeType::DossierApparu),
                "{moteur} : le nouveau nom doit sortir en « dossier apparu »"
            );
        }
    }

    /// #4896 — le dossier mis à la corbeille ou sorti de la racine : un seul
    /// événement, sur le dossier.
    #[test]
    fn un_dossier_mis_a_la_corbeille_sort_en_disparu_sur_les_trois_moteurs_4896() {
        use notify::event::{RemoveKind, RenameMode};
        let (_racine, ancien, _nouveau) = scene_renommee("corbeille");
        for (moteur, kind) in [
            ("Windows (REMOVED)", EventKind::Remove(RemoveKind::Any)),
            (
                "macOS (ItemRenamed vers ~/.Trash)",
                EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
            ),
            (
                "Linux (MOVED_FROM sans MOVED_TO)",
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            ),
            (
                "macOS/Linux, suppression (IsDir / ISDIR)",
                EventKind::Remove(RemoveKind::Folder),
            ),
        ] {
            let vus = genres(&rejouer_evenements_notify(vec![evp(kind, &ancien)]));
            assert_eq!(
                vus.get(&*ancien.to_string_lossy()),
                Some(&ChangeType::DossierDisparu),
                "{moteur}"
            );
        }
    }

    /// Contre-épreuves : ce qui n'est PAS un dossier qui bouge ne sort pas.
    #[test]
    fn ni_une_pochette_ni_un_montage_ne_passent_pour_un_dossier_4896() {
        use notify::event::{CreateKind, RemoveKind, RenameMode};
        let (_racine, _ancien, nouveau) = scene_renommee("contre");
        let pochette = nouveau.join("cover.jpg");
        fs::write(&pochette, b"jpg").unwrap();
        let vus = genres(&rejouer_evenements_notify(vec![
            // Toujours là, pas un dossier : rien à dire.
            evp(EventKind::Remove(RemoveKind::Any), &pochette),
            evp(
                EventKind::Modify(ModifyKind::Name(RenameMode::Any)),
                &pochette,
            ),
            evp(EventKind::Create(CreateKind::Any), &pochette),
        ]));
        // #5034 — elle est désormais RELAYÉE, mais comme image de pochette :
        // jamais comme un dossier.
        assert_eq!(
            vus.values().collect::<Vec<_>>(),
            vec![&ChangeType::ImageDePochette],
            "une pochette n'est jamais un dossier : {vus:?}"
        );
        // Le moteur a dit « fichier » : pas de dossier, même disparu.
        let vus = genres(&rejouer_evenements_notify(vec![evp(
            EventKind::Remove(RemoveKind::File),
            &nouveau.join("notes.txt"),
        )]));
        assert!(vus.is_empty(), "{vus:?}");
        // Un montage (FSEvents, info « mount ») n'est pas un dossier d'album.
        let vus = genres(&rejouer_evenements_notify(vec![
            evp(EventKind::Create(CreateKind::Other), &nouveau).set_info("mount"),
        ]));
        assert!(vus.is_empty(), "{vus:?}");
        // Les événements de CONTENU d'un dossier ne le font pas « apparaître ».
        let vus = genres(&rejouer_evenements_notify(vec![evp(
            EventKind::Modify(ModifyKind::Any),
            &nouveau,
        )]));
        assert!(vus.is_empty(), "{vus:?}");
    }

    #[test]
    fn les_fichiers_audio_d_un_dossier_apparu_se_listent_a_toute_profondeur_4896() {
        let (_racine, _ancien, nouveau) = scene_renommee("liste");
        fs::create_dir_all(nouveau.join("CD2")).unwrap();
        fs::write(nouveau.join("CD2").join("01 - Us And Them.flac"), b"x").unwrap();
        fs::write(nouveau.join("cover.jpg"), b"x").unwrap();
        let vus = fichiers_audio_sous(&nouveau);
        assert_eq!(
            vus,
            vec![
                nouveau
                    .join("01 - Speak To Me.flac")
                    .to_string_lossy()
                    .to_string(),
                nouveau
                    .join("CD2")
                    .join("01 - Us And Them.flac")
                    .to_string_lossy()
                    .to_string(),
            ]
        );
    }

    #[test]
    fn watcher_lifecycle() {
        let dir = tempfile::TempDir::new().unwrap();

        let mut watcher = FileWatcher::new(vec![dir.path().to_string_lossy().to_string()]).unwrap();

        let test_file = dir.path().join("test.flac");
        {
            let mut f = fs::File::create(&test_file).unwrap();
            f.write_all(b"fake flac data").unwrap();
        }

        let changes = watcher.poll_changes(Duration::from_secs(2));
        // May or may not catch the event depending on timing
        if !changes.is_empty() {
            assert!(changes.iter().any(|c| c.path.contains("test.flac")));
        }

        watcher.stop();
    }

    /// Fil forum 2148 — le relevé initial d'une racine RÉSEAU ne retient plus
    /// le surveillant. Le relevé initial parcourt tout l'arbre avant de
    /// rendre la main ; fait dans `FileWatcher::new`, ce parcours retardait
    /// d'autant la boucle du surveillant, dossiers locaux compris. Ici le
    /// relevé est RETENU indéfiniment : la création doit rendre la main quand
    /// même, la racine locale doit être suivie, et la sonde n'entre en service
    /// qu'une fois son relevé libéré.
    #[test]
    fn le_releve_d_une_racine_reseau_ne_retient_ni_la_creation_ni_les_racines_locales_2148() {
        let racine = crate::test_scratch::scratch_dir("watcher-releve-reseau-2148");
        let locale = racine.join("locale");
        let reseau = racine.join("partage");
        fs::create_dir_all(&locale).unwrap();
        fs::create_dir_all(&reseau).unwrap();
        fs::write(reseau.join("deja-la.flac"), b"x").unwrap();
        simulation_reseau::RACINES
            .lock()
            .unwrap()
            .push(reseau.clone());
        simulation_reseau::RETENUES
            .0
            .lock()
            .unwrap()
            .push(reseau.clone());

        let dirs = vec![
            reseau.to_string_lossy().to_string(),
            locale.to_string_lossy().to_string(),
        ];
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(FileWatcher::new(dirs));
        });
        let cree = rx.recv_timeout(Duration::from_secs(20));
        // Libérer AVANT d'affirmer : un échec ne doit pas laisser de fil
        // suspendu derrière lui.
        simulation_reseau::liberer(&reseau);
        let mut watcher = cree
            .expect(
                "🔴 fil 2148 — FileWatcher::new attend la fin du relevé initial \
                 de la racine réseau : la boucle du surveillant ne démarre pas, \
                 et aucune racine locale n'est suivie pendant ce temps",
            )
            .expect("le surveillant se crée");
        assert!(
            watcher.dirs.contains(&locale),
            "la racine locale est suivie par le moteur natif dès la création : {:?}",
            watcher.dirs
        );
        assert!(
            !watcher.dirs.contains(&reseau),
            "la racine réseau n'est pas confiée au moteur natif : {:?}",
            watcher.dirs
        );

        // Le relevé libéré, la sonde entre en service.
        let limite = std::time::Instant::now() + Duration::from_secs(20);
        while !watcher.racines_sondees().contains(&reseau) {
            assert!(
                std::time::Instant::now() < limite,
                "la sonde de la racine réseau n'est jamais entrée en service"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        // Un second passage de `ensure_watches` ne relance pas d'amorce.
        watcher.ensure_watches();
        assert_eq!(
            watcher
                .racines_sondees()
                .iter()
                .filter(|d| **d == reseau)
                .count(),
            1,
            "une seule sonde par racine réseau"
        );
        assert!(verrou(&watcher.sondage.en_amorce).is_empty());
        watcher.stop();
        assert!(
            watcher.racines_sondees().is_empty(),
            "stop retire les sondes"
        );
    }

    /// Fil forum 2148 — un surveillant arrêté pendant le relevé initial d'une
    /// racine réseau ne voit pas cette sonde se poser après coup.
    #[test]
    fn un_releve_qui_finit_apres_l_arret_ne_pose_aucune_sonde_2148() {
        let racine = crate::test_scratch::scratch_dir("watcher-releve-apres-arret-2148");
        let reseau = racine.join("partage");
        fs::create_dir_all(&reseau).unwrap();
        simulation_reseau::RACINES
            .lock()
            .unwrap()
            .push(reseau.clone());
        simulation_reseau::RETENUES
            .0
            .lock()
            .unwrap()
            .push(reseau.clone());

        let mut watcher = FileWatcher::new(vec![reseau.to_string_lossy().to_string()])
            .expect("une racine réseau en cours de relevé compte comme suivie");
        watcher.stop();
        simulation_reseau::liberer(&reseau);
        let limite = std::time::Instant::now() + Duration::from_secs(20);
        while !verrou(&watcher.sondage.en_amorce).is_empty() {
            assert!(
                std::time::Instant::now() < limite,
                "le relevé ne s'est pas terminé"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            watcher.racines_sondees().is_empty(),
            "une sonde finie après l'arrêt ne doit pas se poser"
        );
    }

    /// #5168 — un `foo_dr.txt` posé ou réécrit dans un dossier d'album doit
    /// réveiller le rattrapage des rapports pour CE dossier. Avant, le
    /// surveillant ne relayait que l'audio : le rapport restait invisible
    /// jusqu'au prochain scan complet, et même lui ne le lisait pas (les
    /// pistes n'avaient pas changé).
    #[test]
    fn un_rapport_dr_pose_ou_reecrit_reveille_le_rattrapage_de_son_dossier_5168() {
        use notify::event::{CreateKind, DataChange, RemoveKind};
        let racine = crate::test_scratch::scratch_dir("watcher-foo-dr-5168");
        let pose = racine.join("Album pose");
        let reecrit = racine.join("Album reecrit");
        let supprime = racine.join("Album supprime");
        for d in [&pose, &reecrit, &supprime] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(pose.join("foo_dr.txt"), "x").unwrap();
        fs::write(reecrit.join("FOO_DR.TXT"), "x").unwrap();
        let _ = rejouer_evenements_notify(vec![
            evp(
                EventKind::Create(CreateKind::File),
                &pose.join("foo_dr.txt"),
            ),
            evp(
                EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                &reecrit.join("FOO_DR.TXT"),
            ),
            evp(
                EventKind::Remove(RemoveKind::File),
                &supprime.join("foo_dr.txt"),
            ),
            // Une pochette n'est pas un rapport.
            evp(
                EventKind::Create(CreateKind::File),
                &supprime.join("cover.jpg"),
            ),
        ]);
        let signales = crate::taches_de_fond::rapports_dr::dossiers_signales();
        for d in [&pose, &reecrit] {
            assert!(
                signales.contains(d),
                "🔴 #5168 — un rapport de DR posé ou réécrit dans {d:?} n'a pas \
                 réveillé le rattrapage de son dossier : le surveillant ne \
                 relaie que l'audio. Signalés : {signales:?}"
            );
        }
        assert!(
            !signales.contains(&supprime),
            "un rapport supprimé ou une pochette ne relancent rien : {signales:?}"
        );
    }

    /// Une bibliothèque `Artiste/Album/piste.flac` sous `racine`.
    fn bibliotheque(racine: &Path, artistes: usize, albums: usize, pistes: usize) {
        for a in 0..artistes {
            for b in 0..albums {
                let album = racine
                    .join(format!("Artiste {a:04}"))
                    .join(format!("Album {b:02}"));
                fs::create_dir_all(&album).unwrap();
                for p in 0..pistes {
                    fs::write(album.join(format!("{p:02} - Piste.flac")), b"x").unwrap();
                }
            }
        }
    }

    /// Un passage de sonde, traduit par le gestionnaire de production.
    fn passage(releve: &mut Releve, complet: bool) -> (BilanDuTour, HashMap<String, ChangeType>) {
        let mut bruts = Vec::new();
        let bilan = releve.tour(complet, &mut |e| bruts.push(e));
        (bilan, genres(&rejouer_evenements_notify(bruts)))
    }

    /// Certains systèmes de fichiers datent à la seconde : laisser la date
    /// d'un dossier changer pour de bon.
    fn laisser_passer_une_seconde() {
        std::thread::sleep(Duration::from_millis(1_100));
    }

    /// Fil 2148 (#5792) — un fichier ajouté dans un sous-dossier d'un partage
    /// réseau est vu au passage suivant SANS relire tout l'arbre : seul le
    /// dossier dont la date a changé est relu, les autres coûtent un `stat`
    /// chacun et leurs fichiers aucun.
    #[test]
    fn un_fichier_ajoute_dans_un_sous_dossier_est_vu_sans_relire_tout_l_arbre_2148() {
        let racine = crate::test_scratch::scratch_dir_in(
            std::env::current_dir().unwrap(),
            "watcher-sonde-incrementale-2148",
        );
        bibliotheque(&racine, 20, 5, 3);
        let (mut releve, initial) = Releve::initial(&racine).expect("relevé initial");
        // 20 artistes, 100 albums, 300 pistes.
        assert_eq!(releve.taille(), 420);
        assert_eq!(initial.dossiers_lus, 121);

        // Rien n'a bougé : aucune liste relue, aucun événement.
        let (bilan, vus) = passage(&mut releve, false);
        assert!(vus.is_empty(), "{vus:?}");
        assert_eq!(
            bilan.dossiers_lus, 0,
            "🔴 #5792 — un passage sans changement relit des dossiers (parcours complet ?) : {bilan:?}"
        );

        laisser_passer_une_seconde();
        let nouveau = racine
            .join("Artiste 0007")
            .join("Album 03")
            .join("99 - Nouvelle.flac");
        fs::write(&nouveau, b"x").unwrap();
        let (bilan, vus) = passage(&mut releve, false);
        assert_eq!(
            vus.get(&*nouveau.to_string_lossy()),
            Some(&ChangeType::Added),
            "🔴 #5792 — le fichier ajouté n'est pas vu au passage suivant : {vus:?}"
        );
        assert_eq!(vus.len(), 1, "{vus:?}");
        assert_eq!(
            bilan.dossiers_lus, 1,
            "🔴 #5792 — seul l'album qui a bougé doit être relu : {bilan:?}"
        );
        // Un `stat` pour la racine, les 20 artistes et les 100 albums, plus
        // les 4 entrées de l'album relu : rien des 296 autres pistes.
        assert_eq!(bilan.entrees_examinees, 1 + 20 + 100 + 4, "{bilan:?}");
        assert!(!bilan.complet);
    }

    /// Fil 2148 — les autres gestes, au premier et au second niveau, dans un
    /// même passage : un album renommé, un album ajouté chez un artiste dont
    /// un AUTRE album reçoit un fichier (la date de cet album-là ne doit pas
    /// être écrasée par la relecture de l'artiste), un artiste supprimé.
    #[test]
    fn renommer_ajouter_et_supprimer_sous_les_deux_premiers_niveaux_2148() {
        let racine = crate::test_scratch::scratch_dir_in(
            std::env::current_dir().unwrap(),
            "watcher-sonde-gestes-2148",
        );
        bibliotheque(&racine, 4, 3, 2);
        let (mut releve, _) = Releve::initial(&racine).unwrap();
        laisser_passer_une_seconde();

        let a1 = racine.join("Artiste 0001");
        fs::rename(a1.join("Album 00"), a1.join("Album renommé")).unwrap();
        let a2 = racine.join("Artiste 0002");
        fs::create_dir_all(a2.join("Album neuf")).unwrap();
        fs::write(a2.join("Album neuf").join("01.flac"), b"x").unwrap();
        fs::write(a2.join("Album 01").join("99.flac"), b"x").unwrap();
        let a3 = racine.join("Artiste 0003");
        fs::remove_dir_all(&a3).unwrap();

        let (_bilan, vus) = passage(&mut releve, false);
        let vu = |p: PathBuf| vus.get(&*p.to_string_lossy()).cloned();
        assert_eq!(
            vu(a1.join("Album 00")),
            Some(ChangeType::DossierDisparu),
            "🔴 album renommé non vu : la relecture de la racine a-t-elle écrasé la \
             date de l'artiste resté en place ?"
        );
        assert_eq!(
            vu(a1.join("Album renommé")),
            Some(ChangeType::DossierApparu)
        );
        assert_eq!(
            vu(a1.join("Album renommé").join("00 - Piste.flac")),
            Some(ChangeType::Added)
        );
        assert_eq!(vu(a2.join("Album neuf")), Some(ChangeType::DossierApparu));
        assert_eq!(
            vu(a2.join("Album neuf").join("01.flac")),
            Some(ChangeType::Added)
        );
        assert_eq!(
            vu(a2.join("Album 01").join("99.flac")),
            Some(ChangeType::Added),
            "la relecture de l'artiste ne doit pas masquer le changement de son album"
        );
        assert_eq!(vu(a3.clone()), Some(ChangeType::DossierDisparu));
        assert_eq!(
            vu(a3.join("Album 00").join("00 - Piste.flac")),
            Some(ChangeType::Deleted)
        );

        // Le passage suivant relit une fois les dossiers suspects, sans rien
        // inventer.
        let (_, vus) = passage(&mut releve, false);
        assert!(vus.is_empty(), "{vus:?}");
        let (bilan, vus) = passage(&mut releve, false);
        assert!(vus.is_empty(), "{vus:?}");
        assert_eq!(bilan.dossiers_lus, 0, "{bilan:?}");
    }

    /// Fil 2148 — ce que la date des dossiers ne dit pas (une retouche sur
    /// place, un changement à trois niveaux sous la racine) attend le passage
    /// complet, qui le voit. La retouche sort en `Modified` : avec
    /// `PollWatcher`, elle sortait en `Modify(Metadata(WriteTime))`, écarté.
    #[test]
    fn le_passage_complet_voit_ce_que_les_dates_de_dossiers_taisent_2148() {
        let racine = crate::test_scratch::scratch_dir_in(
            std::env::current_dir().unwrap(),
            "watcher-sonde-complet-2148",
        );
        bibliotheque(&racine, 2, 2, 2);
        let cd2 = racine.join("Artiste 0000").join("Album 00").join("CD2");
        fs::create_dir_all(&cd2).unwrap();
        let (mut releve, _) = Releve::initial(&racine).unwrap();
        laisser_passer_une_seconde();
        let retouche = racine
            .join("Artiste 0001")
            .join("Album 01")
            .join("00 - Piste.flac");
        fs::write(&retouche, b"balises reecrites").unwrap();
        let profond = cd2.join("01.flac");
        fs::write(&profond, b"x").unwrap();

        let (_, vus) = passage(&mut releve, false);
        assert!(
            vus.is_empty(),
            "le passage incrémental ne les voit pas : {vus:?}"
        );
        let (bilan, vus) = passage(&mut releve, true);
        assert!(bilan.complet);
        assert_eq!(
            vus.get(&*retouche.to_string_lossy()),
            Some(&ChangeType::Modified)
        );
        assert_eq!(
            vus.get(&*profond.to_string_lossy()),
            Some(&ChangeType::Added)
        );
    }

    #[test]
    fn le_delai_des_partages_reseau_se_resout_dans_ses_bornes_2148() {
        assert_eq!(resolve_network_poll_interval(None), 300);
        assert_eq!(resolve_network_poll_interval(Some("beaucoup")), 300);
        assert_eq!(resolve_network_poll_interval(Some("\"120\"")), 120);
        assert_eq!(resolve_network_poll_interval(Some("5")), 60);
        assert_eq!(resolve_network_poll_interval(Some("90000")), 3_600);
        assert_eq!(valider_network_poll_interval("600"), Ok(600));
        assert!(valider_network_poll_interval("59").is_err());
        assert!(valider_network_poll_interval("3601").is_err());
        assert!(valider_network_poll_interval("-1").is_err());
    }

    /// Banc du fil 2148 : le coût d'un passage sur un partage simulé.
    ///
    /// `TUNE_BANC_SONDE_RACINE=/dev/shm/... cargo test -p tune-core --lib \
    ///  banc_sonde_reseau -- --ignored --nocapture`
    /// Sans la variable, l'arbre va dans un dossier jetable du dossier courant.
    #[test]
    #[ignore = "banc : 100 000 fichiers, à lancer à la main"]
    fn banc_sonde_reseau_100000_fichiers_2148() {
        let garde;
        let racine = match std::env::var_os("TUNE_BANC_SONDE_RACINE") {
            Some(r) => {
                let r = PathBuf::from(r);
                let _ = fs::remove_dir_all(&r);
                fs::create_dir_all(&r).unwrap();
                r
            }
            None => {
                garde = crate::test_scratch::scratch_dir_in(
                    std::env::current_dir().unwrap(),
                    "watcher-banc-2148",
                );
                garde.to_path_buf()
            }
        };
        let t = Instant::now();
        bibliotheque(&racine, 1_000, 10, 10);
        eprintln!("arbre : 100 000 fichiers créés en {:?}", t.elapsed());
        let t = Instant::now();
        let (mut releve, initial) = Releve::initial(&racine).unwrap();
        eprintln!("relevé initial : {:?} {initial:?}", t.elapsed());
        for complet in [false, true, false] {
            let t = Instant::now();
            let (bilan, vus) = passage(&mut releve, complet);
            eprintln!(
                "passage sans changement : {:?} {bilan:?} ({} vus)",
                t.elapsed(),
                vus.len()
            );
        }
        laisser_passer_une_seconde();
        fs::write(
            racine
                .join("Artiste 0500")
                .join("Album 05")
                .join("99 - Nouvelle.flac"),
            b"x",
        )
        .unwrap();
        let t = Instant::now();
        let (bilan, vus) = passage(&mut releve, false);
        eprintln!(
            "passage avec un ajout : {:?} {bilan:?} ({} vus)",
            t.elapsed(),
            vus.len()
        );
        assert_eq!(vus.len(), 1);
        if std::env::var_os("TUNE_BANC_SONDE_RACINE").is_some() {
            let _ = fs::remove_dir_all(&racine);
        }
    }
}
