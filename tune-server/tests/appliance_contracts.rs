//! Contrats appliance sérialisés autour de leurs variables d’environnement.

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
