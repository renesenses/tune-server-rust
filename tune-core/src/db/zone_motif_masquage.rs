//! POURQUOI une zone est masquée, et la seule réparation sûre (#5077).
//!
//! Stéphane Villerio (fil 1926) a perdu la zone DLNA de son DMP-A6 : ignorer
//! l'entrée AirPlay du même boîtier masquait, par l'ancienne cascade de
//! `ignore_device` (#4957), toutes les zones de la même adresse. Le défaut est
//! corrigé (#4957, #4970, #4975, #4982), mais les zones déjà emportées restent
//! masquées — et rien ne permettait de les distinguer d'une zone que
//! l'utilisateur a VOULU retirer : le masquage était un seul bit,
//! `zones.is_hidden`, sans motif ni date.
//!
//! La migration 112 (PG 075) pose `zones.motif_masquage` et `zones.masquee_le`.
//! Chaque chemin qui masque écrit SON motif ; chaque démasquage les efface.
//!
//! # Ce que la réparation a le droit de faire
//!
//! Une seule chose : démasquer une zone de motif
//! [`MotifMasquage::AppareilIgnore`] dont l'appareil n'est PLUS ignoré — la
//! zone n'a été masquée que parce que l'appareil l'était, et l'utilisateur a
//! levé ce geste.
//!
//! Elle ne touche JAMAIS :
//!
//! * un motif NUL — masquage d'avant la migration : on ne devine rien, et une
//!   zone de Villerio emportée par l'ancien défaut n'est pas discernable
//!   d'une suppression voulue ;
//! * une [`MotifMasquage::SuppressionUtilisateur`], ni aucun autre motif ;
//! * une zone encore visée par une entrée de la liste d'ignorés — par son
//!   identité (identifiant, MAC du même protocole, hôte + nom), ou par le seul
//!   couple (hôte, protocole compatible), qui est la règle la plus large de la
//!   cascade (#4957) : dans le doute, la zone reste masquée.
//!
//! Et si la liste d'ignorés ne peut pas être LUE, rien n'est démasqué : une
//! erreur de lecture n'est pas une liste vide.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::backend::DbBackend;
use super::ignored_device_repo::{
    DeviceIdentity, IgnoredDevice, IgnoredDeviceRepo, identity_matches,
};
use super::zone_repo::{ZoneMasquee, ZoneRepo};

/// La raison d'un masquage de zone, telle qu'elle est écrite dans
/// `zones.motif_masquage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MotifMasquage {
    /// `DELETE /zones/{id}` : l'utilisateur a supprimé cette zone.
    SuppressionUtilisateur,
    /// `DELETE /zones` : l'utilisateur a supprimé toutes les zones.
    SuppressionTotale,
    /// Jumelle « Cet ordinateur » / « This Computer » d'une zone locale par
    /// défaut (`hide_duplicate_generic_local`).
    DoublonLocalGenerique,
    /// Cascade d'« Ignorer cet appareil » (`POST /devices/{id}/ignore`).
    AppareilIgnore,
    /// Notre propre façade MediaRenderer revenue par SSDP (#3688).
    ZoneReflet,
    /// Doublon versé dans une autre zone (`POST /zones/{d}/fusionner-dans/{c}`).
    Fusion,
    /// Valeur écrite par une version plus récente, que celle-ci ne connaît pas.
    Autre,
}

impl MotifMasquage {
    pub const TOUS: [MotifMasquage; 7] = [
        MotifMasquage::SuppressionUtilisateur,
        MotifMasquage::SuppressionTotale,
        MotifMasquage::DoublonLocalGenerique,
        MotifMasquage::AppareilIgnore,
        MotifMasquage::ZoneReflet,
        MotifMasquage::Fusion,
        MotifMasquage::Autre,
    ];

    /// La valeur stockée — la même que la sérialisation serde.
    pub fn as_str(self) -> &'static str {
        match self {
            MotifMasquage::SuppressionUtilisateur => "suppression_utilisateur",
            MotifMasquage::SuppressionTotale => "suppression_totale",
            MotifMasquage::DoublonLocalGenerique => "doublon_local_generique",
            MotifMasquage::AppareilIgnore => "appareil_ignore",
            MotifMasquage::ZoneReflet => "zone_reflet",
            MotifMasquage::Fusion => "fusion",
            MotifMasquage::Autre => "autre",
        }
    }

    /// Relit la colonne. NUL (ou vide) → `None` : motif INCONNU. Un texte que
    /// cette version ne connaît pas → [`MotifMasquage::Autre`], qui n'est
    /// jamais réparable : on ne répare pas ce qu'on ne comprend pas.
    pub fn depuis_stocke(valeur: Option<&str>) -> Option<Self> {
        let v = valeur?.trim();
        if v.is_empty() {
            return None;
        }
        Some(
            Self::TOUS
                .into_iter()
                .find(|m| m.as_str() == v)
                .unwrap_or(MotifMasquage::Autre),
        )
    }

    /// Le motif peut-il un jour être démasqué automatiquement ? Seul
    /// `appareil_ignore` — et seulement quand l'appareil n'est plus ignoré,
    /// ce que décide [`reparer_les_masquages_surs`].
    pub fn reparable(self) -> bool {
        matches!(self, MotifMasquage::AppareilIgnore)
    }

    /// Un geste EXPLICITE de l'utilisateur (supprimer, tout supprimer,
    /// fusionner) remplace le motif d'une zone déjà masquée. Un masquage
    /// automatique, lui, ne touche qu'une zone visible : il ne doit jamais
    /// recouvrir une suppression par un motif réparable.
    pub fn ecrase_un_masquage_existant(self) -> bool {
        matches!(
            self,
            MotifMasquage::SuppressionUtilisateur
                | MotifMasquage::SuppressionTotale
                | MotifMasquage::Fusion
        )
    }
}

fn protocoles_compatibles(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    a.is_empty() || b.is_empty() || a.eq_ignore_ascii_case(b)
}

/// La zone masquée est-elle ENCORE visée par une entrée de la liste
/// d'ignorés ? Le prédicat, seul et pur — c'est lui que les tests fixent.
///
/// Conservateur par construction : `true` dès qu'il y a un doute.
///
/// 1. une zone sans identifiant d'appareil ne se confronte à rien : gardée ;
/// 2. [`identity_matches`] — identifiant exact, MAC du même protocole, hôte +
///    nom équivalent : les règles mêmes de la découverte ;
/// 3. même hôte et protocole compatible (l'un des deux inconnu compris) : la
///    règle la plus large de la cascade d'`ignore_device` (#4957). Un autre
///    appareil du même boîtier, encore ignoré, a pu emporter cette zone.
pub fn zone_encore_visee_par_un_ignore(zone: &ZoneMasquee, ignores: &[IgnoredDevice]) -> bool {
    if zone.device_id.trim().is_empty() {
        return true;
    }
    let identite = DeviceIdentity::new(&zone.device_id, &zone.host, &zone.name)
        .with_mac(zone.mac.as_deref())
        .with_protocol(Some(zone.protocole.as_str()));
    ignores.iter().any(|entree| {
        identity_matches(entree, identite)
            || (!entree.host.trim().is_empty()
                && entree.host.trim().eq_ignore_ascii_case(zone.host.trim())
                && protocoles_compatibles(&entree.device_type, &zone.protocole))
    })
}

/// Le bilan d'une passe de réparation.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RapportDeReparation {
    /// Zones démasquées : motif `appareil_ignore`, appareil libéré.
    pub demasquees: Vec<i64>,
    /// Zones `appareil_ignore` laissées masquées : encore visées par un ignoré.
    pub gardees: Vec<i64>,
}

/// La réparation sûre (#5077) : démasque les zones de motif
/// `appareil_ignore` dont l'appareil n'est plus ignoré. Voir l'en-tête du
/// module pour ce qu'elle ne touche jamais.
///
/// Rend `Err` sans rien démasquer si les zones ou la liste d'ignorés ne
/// peuvent pas être lues (base d'avant la migration 112 comprise).
pub fn reparer_les_masquages_surs(db: Arc<dyn DbBackend>) -> Result<RapportDeReparation, String> {
    let zones = ZoneRepo::with_backend(db.clone());
    let motif = MotifMasquage::AppareilIgnore;
    debug_assert!(motif.reparable());
    let candidates = zones.zones_masquees_pour_motif(motif)?;
    let mut rapport = RapportDeReparation::default();
    if candidates.is_empty() {
        return Ok(rapport);
    }
    // Lecture STRICTE : `IgnoredDeviceRepo::matching` rend « personne
    // d'ignoré » sur une erreur, ce qui démasquerait tout ici.
    let ignores = IgnoredDeviceRepo::with_backend(db).list()?;
    for zone in candidates {
        if zone_encore_visee_par_un_ignore(&zone, &ignores) {
            rapport.gardees.push(zone.id);
            continue;
        }
        match zones.demasquer_si_motif(zone.id, motif) {
            Ok(n) if n > 0 => {
                tracing::info!(
                    zone_id = zone.id,
                    name = %zone.name,
                    device_id = %zone.device_id,
                    masquee_le = ?zone.masquee_le,
                    "zone_demasquee_appareil_plus_ignore"
                );
                rapport.demasquees.push(zone.id);
            }
            // Le motif a changé entre la lecture et l'écriture (suppression
            // arrivée entre-temps) : rien à faire, et c'est voulu.
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(zone_id = zone.id, error = %e, "zone_demasquage_echoue");
            }
        }
    }
    Ok(rapport)
}

#[cfg(test)]
#[path = "zone_motif_masquage_tests_5077.rs"]
mod tests;
