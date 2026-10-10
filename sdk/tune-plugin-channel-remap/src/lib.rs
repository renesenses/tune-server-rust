//! Réaffectation des canaux (#6044) : une matrice N entrées × M sorties, gains
//! en dB, pour envoyer chaque canal d'un fichier multicanal là où il doit
//! aller — d'abord les fichiers 4.0 (quadriphonie) vers une installation
//! stéréo, 5.1 ou 7.1.
//!
//! Le moteur ([`Matrice`]) ne dépend que de la bibliothèque standard : l'hôte
//! Tune l'embarque dans `tune-core` (comme le crossfeed) et l'applique là où le
//! nombre de canaux change, à l'adaptation source → périphérique. Le
//! [`ChannelRemap`] du SDK, lui, travaille SUR PLACE comme tout processeur
//! ABI 1 : il ne sait faire que N → N (échange gauche/droite, surrounds
//! réaffectés, mono) et refuse N → M, que le contrat `AudioBlock` ne permet
//! pas.
#![cfg_attr(not(feature = "native"), forbid(unsafe_code))]
#![deny(unsafe_op_in_unsafe_fn)]
mod engine;
pub use engine::*;
mod sdk;
pub use sdk::ChannelRemap;

#[cfg(feature = "native")]
tune_plugin_abi::export_dsp!(crate::ChannelRemap, include_str!("../manifest.json"));
