//! Smart playlists, smart collections and rule-based recommendation routes.

use std::sync::Arc;

use tune_core::db::backend::DbBackend;

pub mod smart_ai;
pub mod smart_collections;
pub mod smart_playlists;
pub mod smart_refs;

/// Sous-ensemble de l'état serveur nécessaire aux routes intelligentes.
///
/// Garder cette frontière réduite permet à Cargo de compiler ces routes sans
/// invalider le reste de `tune-server`.
#[derive(Clone)]
pub struct SmartHttpState {
    pub(crate) backend: Arc<dyn DbBackend>,
}

impl SmartHttpState {
    pub fn new(backend: Arc<dyn DbBackend>) -> Self {
        Self { backend }
    }
}
