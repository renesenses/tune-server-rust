//! Contrats appliance sérialisés autour de leurs variables d’environnement.

// Le verrou d'environnement est TENU pendant toute l'épreuve, `.await` compris :
// c'est lui qui sérialise les tests qui posent des variables d'environnement.
// `await_holding_lock` décrit ici l'intention même (clippy 1.98).
#![allow(clippy::await_holding_lock)]

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_environment() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[path = "appliance.rs"]
mod appliance;
#[path = "appliance_storage.rs"]
mod appliance_storage;
