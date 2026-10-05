//! Smart playlists, smart collections and rule-based recommendation routes.

use std::sync::Arc;

use tune_core::db::backend::DbBackend;

pub mod catalogue;
pub mod collections_par_defaut;
pub mod comptes;
pub(crate) mod criteres;
mod etiquettes_streaming;
pub(crate) mod regles_sql;
pub mod smart_ai;
pub mod smart_collections;
pub mod smart_playlists;
pub mod smart_refs;
mod source_streaming;

/// Sous-ensemble de l'état serveur nécessaire aux routes intelligentes.
///
/// Garder cette frontière réduite permet à Cargo de compiler ces routes sans
/// invalider le reste de `tune-server`.
#[derive(Clone)]
pub struct SmartHttpState {
    pub(crate) backend: Arc<dyn DbBackend>,
    /// De quoi interroger le CATALOGUE d'un service (#4473).
    ///
    /// `None` partout où le registre des services n'existe pas — en épreuve,
    /// et dans tout appelant qui n'en a pas. Une règle `catalogue:<service>`
    /// est alors REFUSÉE, jamais silencieusement vide : c'est la leçon de
    /// #4469, où une règle non traduite valait « vrai pour tout ».
    pub(crate) catalogue: Option<Arc<dyn catalogue::CatalogueDistant>>,
    /// Les comptes et pochettes de la liste des collections, en cache jusqu'au
    /// prochain changement de la bibliothèque (#5438). `None` : tout est
    /// recalculé à chaque liste, comme avant.
    pub(crate) comptes: Option<Arc<comptes::CacheDesComptes>>,
}

impl SmartHttpState {
    pub fn new(backend: Arc<dyn DbBackend>) -> Self {
        Self {
            backend,
            catalogue: None,
            comptes: None,
        }
    }

    /// Le même état, muni du cache des comptes de la liste (#5438).
    pub fn avec_comptes(mut self, c: Arc<comptes::CacheDesComptes>) -> Self {
        self.comptes = Some(c);
        self
    }

    /// Le même état, muni de quoi interroger les catalogues.
    pub fn avec_catalogue(mut self, c: Arc<dyn catalogue::CatalogueDistant>) -> Self {
        self.catalogue = Some(c);
        self
    }
}
