//! Lectures de fichiers BORNÉES, pour les lots de scan (#5202).
//!
//! Un lot de scan ne doit jamais attendre sans fin un fichier qui ne rend pas
//! la main : partage SMB muet, verrou Windows, NAS endormi. Chaque lecture
//! tourne sur un fil à part et n'est attendue que [`DELAI_LECTURE_DISQUE`] ;
//! à l'échéance le fichier est sauté et journalisé. Après
//! [`EXPIRATIONS_AVANT_ABANDON`] expirations de suite, le stockage est tenu
//! pour muet et le reste du lot n'est plus lu. « Arrêter » est regardé pendant
//! l'attente, toutes les [`TRANCHE_ATTENTE`].
//!
//! Un appel système bloqué ne s'interrompt pas : le fil qui le porte est
//! abandonné et rendra la main quand le noyau la lui rendra. Sa réponse
//! tardive tombe dans un canal fermé, sans effet.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tune_core::event_bus::EventBus;

/// Délai accordé à UNE lecture d'un fichier par un lot de scan.
///
/// Le même budget que la première lecture des balises par le parcours
/// (`FILE_TIMEOUT`, 30 s, `walker.rs`).
pub(crate) const DELAI_LECTURE_DISQUE: Duration = Duration::from_secs(30);

/// Au bout de ce nombre d'expirations CONSÉCUTIVES, le stockage est tenu pour
/// muet. Sans ce plafond, un partage tombé au milieu d'un lot de 444 fichiers
/// coûterait 444 × 30 s, soit 3 h 42, et laisserait autant de fils bloqués.
pub(crate) const EXPIRATIONS_AVANT_ABANDON: usize = 3;

/// Cadence à laquelle une lecture en attente regarde si l'on a demandé l'arrêt.
const TRANCHE_ATTENTE: Duration = Duration::from_millis(200);

pub(crate) enum Lecture<T> {
    Lue(T),
    Expiree,
    Echouee,
    Annulee,
}

/// Exécute `lire` sur un fil à part et n'attend pas plus de `delai`.
pub(crate) fn lire_avec_delai<T: Send + 'static>(
    quoi: &'static str,
    lire: impl FnOnce() -> T + Send + 'static,
    delai: Duration,
    arret: &dyn Fn() -> bool,
) -> Lecture<T> {
    let (tx, rx) = mpsc::sync_channel(1);
    let lance = std::thread::Builder::new()
        .name(format!("scan-{quoi}"))
        .spawn(move || {
            let _ = tx.send(lire());
        });
    if let Err(e) = lance {
        tracing::warn!(quoi, error = %e, "scan_lecture_bornee_thread_failed");
        return Lecture::Echouee;
    }
    let echeance = Instant::now() + delai;
    loop {
        let reste = echeance.saturating_duration_since(Instant::now());
        if reste.is_zero() {
            return Lecture::Expiree;
        }
        match rx.recv_timeout(reste.min(TRANCHE_ATTENTE)) {
            Ok(v) => return Lecture::Lue(v),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Arrêter agit aussi PENDANT une lecture bloquée.
                if arret() {
                    return Lecture::Annulee;
                }
            }
            // Le lecteur a paniqué : rien à ranger pour ce fichier.
            Err(mpsc::RecvTimeoutError::Disconnected) => return Lecture::Echouee,
        }
    }
}

/// Les lectures d'UN lot : délai, arrêt, et compte des expirations de suite.
pub(crate) struct LecturesBornees<'a> {
    delai: Duration,
    arret: &'a dyn Fn() -> bool,
    consecutives: usize,
    pub(crate) expirees: usize,
    pub(crate) annule: bool,
}

impl<'a> LecturesBornees<'a> {
    pub(crate) fn new(delai: Duration, arret: &'a dyn Fn() -> bool) -> Self {
        Self {
            delai,
            arret,
            consecutives: 0,
            expirees: 0,
            annule: false,
        }
    }

    /// Le stockage ne répond plus, ou l'arrêt est demandé : plus aucune
    /// lecture pour ce lot.
    pub(crate) fn epuise(&self) -> bool {
        self.annule || self.consecutives >= EXPIRATIONS_AVANT_ABANDON
    }

    pub(crate) fn arret_demande(&mut self) -> bool {
        if (self.arret)() {
            self.annule = true;
        }
        self.annule
    }

    /// `None` : le fichier n'a rien rendu dans le délai (ou le lecteur a
    /// paniqué, ou l'arrêt est demandé, ou le stockage est déjà tenu pour
    /// muet). L'appelant SAUTE alors ce qu'il voulait faire de ce fichier : un
    /// délai dépassé ne dit rien du contenu, il ne vaut jamais « absent ».
    pub(crate) fn lire<T: Send + 'static>(
        &mut self,
        quoi: &'static str,
        chemin: &str,
        lire: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        if self.epuise() {
            return None;
        }
        match lire_avec_delai(quoi, lire, self.delai, self.arret) {
            Lecture::Lue(v) => {
                self.consecutives = 0;
                Some(v)
            }
            Lecture::Expiree => {
                self.expirees += 1;
                self.consecutives += 1;
                tracing::warn!(
                    quoi,
                    path = %chemin,
                    delai_ms = self.delai.as_millis() as u64,
                    "scan_lecture_disque_timeout — la lecture de ce fichier ne rend pas la \
                     main : il est sauté (#5202)"
                );
                if self.consecutives == EXPIRATIONS_AVANT_ABANDON {
                    tracing::warn!(
                        quoi,
                        expirees = self.expirees,
                        "scan_lecture_disque_abandonnee — plusieurs fichiers de suite ne \
                         répondent pas : le stockage ne répond plus, le reste du lot n'est \
                         plus lu (#5202)"
                    );
                }
                None
            }
            Lecture::Echouee => {
                self.consecutives = 0;
                tracing::warn!(quoi, path = %chemin, "scan_lecture_disque_failed");
                None
            }
            Lecture::Annulee => {
                self.annule = true;
                None
            }
        }
    }
}

type Metadonnees = HashMap<String, String>;
pub(crate) type LecteurMetadonnees = Arc<dyn Fn(&Path) -> Metadonnees + Send + Sync>;

/// Relit les métadonnées étendues des fichiers d'un lot, AVANT sa transaction.
///
/// `None` signifie annulé : le lot ne doit pas ouvrir sa transaction.
#[allow(clippy::too_many_arguments)]
pub(crate) fn lire_metadonnees_du_lot(
    chemins: &[String],
    lire: &LecteurMetadonnees,
    delai: Duration,
    arret: impl Fn() -> bool,
    bus: &EventBus,
    lot: usize,
    scanned: i64,
    total: i64,
) -> Option<HashMap<String, Metadonnees>> {
    let mut resultat = HashMap::new();
    let debut = Instant::now();
    let mut dernier = None;
    let mut lectures = LecturesBornees::new(delai, &arret);
    let mut abandonnes = 0usize;
    for (i, chemin) in chemins.iter().enumerate() {
        if lectures.arret_demande() {
            tracing::info!(
                lot,
                lus = i,
                total = chemins.len(),
                "scan_extended_metadata_cancelled"
            );
            return None;
        }
        if lectures.epuise() {
            abandonnes = chemins.len() - i;
            tracing::warn!(
                lot,
                lus = i,
                total = chemins.len(),
                non_relus = abandonnes,
                expirees = lectures.expirees,
                delai_ms = delai.as_millis() as u64,
                "scan_extended_metadata_abandoned — plusieurs fichiers de suite ne rendent \
                 pas leurs crédits : le stockage ne répond plus, le reste du lot est importé \
                 sans les relire (#5202)"
            );
            break;
        }
        let path = Path::new(chemin);
        if dernier.is_none_or(|instant: Instant| instant.elapsed() >= Duration::from_secs(2)) {
            // Les compteurs d'import restent ceux des lots validés. Le chemin
            // et le compteur de crédits décrivent le travail réellement en cours.
            bus.emit(
                "library.scan.progress",
                serde_json::json!({
                    "phase": "files", "stage": "extended_metadata",
                    "scanned": scanned, "total": total, "batch": lot,
                    "current_dir": path.parent().map(|p| p.to_string_lossy()),
                    "current_file": chemin,
                    "metadata_read": i, "metadata_total": chemins.len(),
                }),
            );
            tracing::info!(lot, lus = i, total = chemins.len(), path = %chemin,
                elapsed_ms = debut.elapsed().as_millis() as u64,
                "scan_extended_metadata_progress");
            dernier = Some(Instant::now());
        }
        let lecteur = Arc::clone(lire);
        let p = PathBuf::from(chemin);
        if let Some(meta) = lectures.lire("credits", chemin, move || lecteur(&p))
            && !meta.is_empty()
        {
            resultat.insert(chemin.clone(), meta);
        }
        if lectures.annule {
            tracing::info!(
                lot,
                lus = i,
                total = chemins.len(),
                "scan_extended_metadata_cancelled"
            );
            return None;
        }
    }
    if arret() {
        return None;
    }
    tracing::info!(
        lot,
        lus = chemins.len() - abandonnes,
        expirees = lectures.expirees,
        non_relus = abandonnes,
        elapsed_ms = debut.elapsed().as_millis() as u64,
        "scan_extended_metadata_complete"
    );
    Some(resultat)
}
