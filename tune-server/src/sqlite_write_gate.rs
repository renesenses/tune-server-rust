use std::sync::OnceLock;
use std::time::Instant;

use tokio::sync::{Mutex, MutexGuard};

/// Porte process-wide des longues transactions SQLite du scan.
///
/// Les lots de scan utilisent volontairement `BEGIN IMMEDIATE` puis plusieurs
/// appels au backend. Le mutex interne de la connexion est donc relâché entre
/// ces appels, alors que la transaction SQLite reste ouverte. Une écriture de
/// file pouvait s'intercaler, tenter son propre `BEGIN`, épuiser ses retries et
/// vider la file (#1997). Cette porte couvre l'intervalle logique complet.
///
/// Le mutex Tokio est FIFO : après le commit d'un lot, une action utilisateur
/// déjà en attente passe avant que le scan ne puisse prendre le lot suivant.
fn gate() -> &'static Mutex<()> {
    static GATE: OnceLock<Mutex<()>> = OnceLock::new();
    GATE.get_or_init(|| Mutex::new(()))
}

/// À appeler uniquement depuis `spawn_blocking`, autour de BEGIN…COMMIT.
pub(crate) fn scan_batch() -> MutexGuard<'static, ()> {
    gate().blocking_lock()
}

/// Attente asynchrone : ne bloque pas un worker Tokio pendant un lot de scan.
pub(crate) async fn user_queue() -> MutexGuard<'static, ()> {
    let started = Instant::now();
    let guard = gate().lock().await;
    let waited = started.elapsed();
    if waited >= std::time::Duration::from_millis(10) {
        tracing::info!(
            waited_ms = waited.as_millis() as u64,
            "queue_write_waited_for_scan_batch"
        );
    }
    guard
}
