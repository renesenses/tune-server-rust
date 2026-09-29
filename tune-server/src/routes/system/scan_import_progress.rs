//! Prélecture des crédits hors transaction et arrêt entre deux fichiers (#5202).
//!
//! Chaque lecture est BORNÉE : un fichier dont la lecture ne rend pas la main
//! (partage SMB muet, verrou Windows, NAS endormi) est sauté et journalisé au
//! bout de [`DELAI_LECTURE_CREDITS`], et ne retient plus le lot entier.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use tune_core::event_bus::EventBus;

type Metadonnees = HashMap<String, String>;
pub(super) type LecteurMetadonnees = Arc<dyn Fn(&Path) -> Metadonnees + Send + Sync>;

/// Délai accordé à la relecture des crédits d'UN fichier (#5202).
///
/// Ces balises ont déjà été lues une première fois par le parcours, sous ses
/// propres délais (`FILE_TIMEOUT`, 30 s, dans `walker.rs`) : le même budget
/// suffit ici. Les crédits sont un complément ; un fichier qui ne les rend pas
/// garde sa piste, ses balises de base et sa pochette, et n'y perd que cela.
pub(super) const DELAI_LECTURE_CREDITS: Duration = Duration::from_secs(30);

/// Au bout de ce nombre d'expirations CONSÉCUTIVES, le partage est tenu pour
/// muet et le reste du lot n'est plus relu. Sans ce plafond, un partage tombé
/// au milieu d'un lot de 444 fichiers coûterait 444 × 30 s, soit 3 h 42 à
/// 97 %, et laisserait autant de fils bloqués dans le noyau.
pub(super) const EXPIRATIONS_AVANT_ABANDON: usize = 3;

/// Cadence à laquelle une lecture en attente regarde si l'on a demandé l'arrêt.
const TRANCHE_ATTENTE: Duration = Duration::from_millis(200);

enum Lecture {
    Lue(Metadonnees),
    Expiree,
    Echouee,
    Annulee,
}

/// Lit un fichier sur un fil à part et n'attend pas plus de `delai`.
///
/// Un appel système bloqué ne s'interrompt pas : le fil qui le porte est
/// abandonné et rendra la main quand le noyau la lui rendra. Sa réponse
/// tardive tombe dans un canal fermé, sans effet.
fn lire_avec_delai(
    chemin: &str,
    lire: &LecteurMetadonnees,
    delai: Duration,
    arret: &impl Fn() -> bool,
) -> Lecture {
    let (tx, rx) = mpsc::sync_channel(1);
    let lecteur = Arc::clone(lire);
    let path = PathBuf::from(chemin);
    let lance = std::thread::Builder::new()
        .name("scan-credits".into())
        .spawn(move || {
            let _ = tx.send(lecteur(&path));
        });
    if let Err(e) = lance {
        tracing::warn!(path = %chemin, error = %e, "scan_extended_metadata_thread_failed");
        return Lecture::Echouee;
    }
    let echeance = Instant::now() + delai;
    loop {
        let reste = echeance.saturating_duration_since(Instant::now());
        if reste.is_zero() {
            return Lecture::Expiree;
        }
        match rx.recv_timeout(reste.min(TRANCHE_ATTENTE)) {
            Ok(meta) => return Lecture::Lue(meta),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Arrêter agit aussi PENDANT une lecture bloquée, pas seulement
                // entre deux fichiers.
                if arret() {
                    return Lecture::Annulee;
                }
            }
            // Le lecteur a paniqué : rien à ranger pour ce fichier.
            Err(mpsc::RecvTimeoutError::Disconnected) => return Lecture::Echouee,
        }
    }
}

/// `None` signifie annulé : le lot ne doit pas ouvrir sa transaction.
#[allow(clippy::too_many_arguments)]
pub(super) fn lire_metadonnees_du_lot(
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
    let mut expirees = 0usize;
    let mut consecutives = 0usize;
    let mut abandonnes = 0usize;
    for (i, chemin) in chemins.iter().enumerate() {
        if arret() {
            tracing::info!(
                lot,
                lus = i,
                total = chemins.len(),
                "scan_extended_metadata_cancelled"
            );
            return None;
        }
        if consecutives >= EXPIRATIONS_AVANT_ABANDON {
            abandonnes = chemins.len() - i;
            tracing::warn!(
                lot,
                lus = i,
                total = chemins.len(),
                non_relus = abandonnes,
                expirees,
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
        match lire_avec_delai(chemin, lire, delai, &arret) {
            Lecture::Lue(meta) => {
                consecutives = 0;
                if !meta.is_empty() {
                    resultat.insert(chemin.clone(), meta);
                }
            }
            Lecture::Expiree => {
                expirees += 1;
                consecutives += 1;
                tracing::warn!(
                    lot,
                    path = %chemin,
                    delai_ms = delai.as_millis() as u64,
                    "scan_extended_metadata_timeout — la lecture de ce fichier ne rend pas \
                     la main : il est importé sans ses crédits (#5202)"
                );
            }
            Lecture::Echouee => {
                consecutives = 0;
                tracing::warn!(lot, path = %chemin, "scan_extended_metadata_read_failed");
            }
            Lecture::Annulee => {
                tracing::info!(
                    lot,
                    lus = i,
                    total = chemins.len(),
                    "scan_extended_metadata_cancelled"
                );
                return None;
            }
        }
    }
    if arret() {
        return None;
    }
    tracing::info!(
        lot,
        lus = chemins.len() - abandonnes,
        expirees,
        non_relus = abandonnes,
        elapsed_ms = debut.elapsed().as_millis() as u64,
        "scan_extended_metadata_complete"
    );
    Some(resultat)
}
