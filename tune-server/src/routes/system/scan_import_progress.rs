//! Prélecture des crédits hors transaction et arrêt entre deux fichiers (#5202).
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tune_core::event_bus::EventBus;

type Metadonnees = HashMap<String, String>;
pub(super) type LecteurMetadonnees = Arc<dyn Fn(&Path) -> Metadonnees + Send + Sync>;

/// `None` signifie annulé : le lot ne doit pas ouvrir sa transaction.
/// Une lecture système déjà en cours doit revenir avant que l'arrêt soit vu.
#[allow(clippy::too_many_arguments)]
pub(super) fn lire_metadonnees_du_lot(
    chemins: &[String],
    lire: &(dyn Fn(&Path) -> Metadonnees + Send + Sync),
    arret: impl Fn() -> bool,
    bus: &EventBus,
    lot: usize,
    scanned: i64,
    total: i64,
) -> Option<HashMap<String, Metadonnees>> {
    let mut resultat = HashMap::new();
    let debut = Instant::now();
    let mut dernier = None;
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
        let meta = lire(path);
        if !meta.is_empty() {
            resultat.insert(chemin.clone(), meta);
        }
    }
    if arret() {
        return None;
    }
    tracing::info!(
        lot,
        lus = chemins.len(),
        elapsed_ms = debut.elapsed().as_millis() as u64,
        "scan_extended_metadata_complete"
    );
    Some(resultat)
}
