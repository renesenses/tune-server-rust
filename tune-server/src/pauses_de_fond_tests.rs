//! #6307 — les bases des tests sont distinctes, le miroir des pauses ne l'est pas.
//!
//! Tous les témoins qui modifient ce miroir (pause, remise à zéro, hydratation)
//! et ceux qui en dépendent prennent la même garde, jusqu'à la fin des tâches
//! qu'ils lancent. Le mutex est asynchrone pour ne pas bloquer un exécuteur.

static PAUSES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(crate) struct Garde {
    _verrou: tokio::sync::MutexGuard<'static, ()>,
}

pub(crate) async fn isoler() -> Garde {
    let verrou = PAUSES.lock().await;
    tune_core::taches_de_fond::oublier_pour_les_essais();
    Garde { _verrou: verrou }
}

impl Drop for Garde {
    fn drop(&mut self) {
        // Nettoyer avant de libérer le mutex, y compris après une assertion
        // en échec : le témoin suivant repart avec les passes au repos.
        tune_core::taches_de_fond::oublier_pour_les_essais();
    }
}
