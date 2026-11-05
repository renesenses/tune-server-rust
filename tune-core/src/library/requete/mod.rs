//! La grammaire de requêtes de la bibliothèque (#7848) : un texte comme
//! `composer:bach (conductor:karajan OR conductor:"Herbert") year:1960..1969
//! -genre:live` est analysé en arbre ([`analyse`]), puis compilé en condition
//! SQL paramétrée pour SQLite ou PostgreSQL ([`compile`]).
//!
//! Elle coexiste avec la requête textuelle des règles intelligentes (#6287,
//! `tune-smart-http/src/requete_texte.rs`), qui compile vers les règles
//! existantes ; celle-ci compile directement, sans aucune valeur dans le SQL.

pub mod analyse;
pub mod compile;

pub use analyse::{ErreurDeRequete, Expr, analyser};
pub use compile::{
    Condition, Contexte, FROM_DES_PISTES, LIMITE_MAX, compiler, nombre_de_pistes,
    pistes_correspondantes,
};

#[cfg(test)]
mod requete_7848_tests;
