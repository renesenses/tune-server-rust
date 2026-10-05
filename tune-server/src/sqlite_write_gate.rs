use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
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

/// Le surveillant de fichiers, autour de chacune de ses écritures. À appeler
/// uniquement depuis `spawn_blocking` (la boucle de `spawn_file_watcher`).
///
/// Sans elle, ses écritures s'intercalaient entre deux instructions d'un lot de
/// scan et entraient dans SA transaction, encore ouverte — alors que le
/// surveillant relit ensuite par le pool de lecture, qui ne la voit pas. Une
/// piste réenregistrée à l'identique était supprimée dans le lot, retrouvée
/// comme « doublon d'elle-même » par ce pool, jamais recréée, et le `COMMIT`
/// du lot validait la perte ; un dossier renommé voyait son `write_tx` refusé
/// et ses pistes revenir comme neuves.
pub(crate) fn surveillant() -> MutexGuard<'static, ()> {
    gate().blocking_lock()
}

/// Écritures de file qui attendent la porte en ce moment.
///
/// Lu par [`ceder_le_lot`] : un lot de scan ne rend la porte que si quelqu'un
/// l'attend.
static FILE_EN_ATTENTE: AtomicUsize = AtomicUsize::new(0);

/// Inscrit une écriture de file en attente, et la désinscrit quoi qu'il
/// arrive : la requête HTTP peut être abandonnée pendant l'attente.
struct Inscription;

impl Inscription {
    fn new() -> Self {
        FILE_EN_ATTENTE.fetch_add(1, Ordering::AcqRel);
        Self
    }
}

impl Drop for Inscription {
    fn drop(&mut self) {
        FILE_EN_ATTENTE.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Une écriture de file attend-elle la porte ?
pub(crate) fn file_en_attente() -> bool {
    FILE_EN_ATTENTE.load(Ordering::Acquire) > 0
}

/// Point de cession d'un lot de scan, entre deux fichiers (ou deux passes).
///
/// Ticket 190 : une écriture de file a attendu plus d'une minute. La porte
/// était tenue pendant TOUT le lot, et la cession entre deux fichiers
/// (`ceder_aux_ecrivains`, #5202) ne la rendait pas : elle ne libère que la
/// connexion, et une écriture de file attend la porte AVANT de la demander.
/// Pendant ce temps, l'auditeur a rappuyé sur Lire ; ses demandes sont
/// parties ensemble à la fin du lot, en rafale.
///
/// Si une écriture de file attend, le lot valide ce qu'il a fait (`COMMIT`),
/// rend la porte — le mutex de Tokio est équitable : l'écriture en attente
/// passe avant que ce fil ne la reprenne —, la reprend, puis rouvre sa
/// transaction (`BEGIN IMMEDIATE`) sous la même étiquette. C'est la même
/// perte d'atomicité que `ceder_aux_ecrivains`, que le scan accepte déjà.
///
/// Sinon, c'est la cession ordinaire aux écrivains de la connexion.
///
/// À appeler uniquement depuis `spawn_blocking`, par le fil qui tient
/// `porte` et la transaction du lot.
pub(crate) fn ceder_le_lot(
    db: &dyn tune_core::db::backend::DbBackend,
    porte: &mut Option<MutexGuard<'static, ()>>,
    etiquette: &'static str,
) -> bool {
    if porte.is_none() || !file_en_attente() {
        return db.ceder_aux_ecrivains();
    }
    let debut = Instant::now();
    tune_core::db::tx_holder::liberer();
    if let Err(e) = db.execute_batch("COMMIT") {
        tracing::warn!(error = %e, etiquette, "lot_de_scan_cession_commit_refuse");
        let _ = db.execute_batch("ROLLBACK");
    }
    drop(porte.take());
    *porte = Some(gate().blocking_lock());
    if let Err(e) = db.execute_batch("BEGIN IMMEDIATE") {
        tracing::warn!(error = %e, etiquette, "lot_de_scan_cession_begin_refuse");
    }
    tune_core::db::tx_holder::declarer(etiquette);
    tracing::info!(
        etiquette,
        cede_ms = debut.elapsed().as_millis() as u64,
        "lot_de_scan_cede_a_la_file"
    );
    true
}

/// Attente asynchrone : ne bloque pas un worker Tokio pendant un lot de scan.
pub(crate) async fn user_queue() -> MutexGuard<'static, ()> {
    let started = Instant::now();
    let inscription = Inscription::new();
    let guard = gate().lock().await;
    drop(inscription);
    let waited = started.elapsed();
    if waited >= std::time::Duration::from_millis(10) {
        tracing::info!(
            waited_ms = waited.as_millis() as u64,
            "queue_write_waited_for_scan_batch"
        );
    }
    guard
}
