//! Greffon WASM « Playlists converter » — tranche 2 de l'épique #4715 (#4717).
//!
//! Transférer une playlist d'un service vers un autre, **à l'identique** et
//! **par lot**, avec un aperçu obligatoire avant toute écriture, un rapport par
//! playlist, et une reprise qui ne recrée pas ce qui existe déjà.
//!
//! # Les trois règles du ticket, et où elles vivent
//!
//! | Règle | Où |
//! |---|---|
//! | Aperçu obligatoire, rien d'écrit sans accord explicite | [`moteur::Convertisseur::apercu`] / [`moteur::Convertisseur::transferer`] |
//! | Rapport : transférés, introuvables, raison | [`modele::PlaylistDuLot`], [`appariement::Raison`] |
//! | Reprise sans recréer | [`modele::PlaylistDuLot::restant_a_verser`] |
//! | Snapshot daté avant tout transfert, retour en arrière SANS suppression (#4718) | [`snapshots`] |
//! | Liens auto-sync : ajouts seulement, disparitions signalées, journal, aperçu avant la première synchro (#4719) | [`liens`] |
//!
//! # L'appariement (Bertrand, 22/09/2026)
//!
//! **Titre + artiste + durée à ±3 s, les trois.** Le verdict titre+artiste
//! reste celui du matcher partagé du projet, relayé par la fonction hôte
//! `host_streaming_match_track` ; le greffon n'en écrit pas un second. Il
//! ajoute la durée, et déclare **introuvable**, avec sa raison, tout ce qui ne
//! tient pas les trois. Voir [`appariement`].
//!
//! # Ce que le greffon ne peut pas faire
//!
//! Il n'importe **aucune** capacité de suppression, parce que la tranche 1
//! n'en expose aucune : ni playlist, ni piste, ni chez un service. Une capacité
//! absente est la seule garde qu'on ne contourne pas.
//!
//! Il ne parle pas HTTP (pas de permission `net`) : l'ETag de TIDAL et la
//! pagination des pistes sont l'affaire du connecteur, derrière les capacités
//! `streaming` de l'hôte.

pub mod appariement;
pub mod dispatch;
pub mod hote;
pub mod liens;
pub mod modele;
pub mod moteur;
pub mod snapshots;

#[cfg(target_arch = "wasm32")]
mod abi;

#[cfg(test)]
mod banc;
#[cfg(test)]
mod essais;
#[cfg(test)]
mod essais_liens;
#[cfg(test)]
mod essais_snapshots;
