//! TESS — *Tune Engine for Sound Shaping* : le **contrat** (lot T01).
//!
//! Ce module ne contient QUE des types et des fonctions pures : aucune ligne
//! n'est branchée dans la lecture. Il fixe ce que les lots suivants consomment :
//!
//! - [`etage`] : le trait temps réel [`EtageTess`] (`f64` entrelacé, tampons
//!   préalloués par l'hôte, aucune allocation dans le fil audio) et
//!   l'[`EtapeFinale`] (quantification PCM ou modulateur DSD) ;
//! - [`greffon`] : le [`DescripteurGreffon`] (id, catégorie, droit, `Feature`,
//!   schéma de réglages, contraintes de place, groupe exclusif) et le
//!   catalogue de référence ;
//! - [`chaine`] : la [`ChaineTess`] versionnée d'une zone, ses étages fixes, la
//!   lecture stricte d'un `PUT` et sa validation (400/402/409) ;
//! - [`resolution`] : l'état réel à l'exécution, étape par étape, dans l'ordre
//!   de Q9, sans jamais rejeter la chaîne ;
//! - [`prereglages`] : préréglages de chaîne et mode d'application (A2) ;
//! - [`zones`] : où TESS s'applique (Q8) ;
//! - [`api`] : les corps JSON de `GET/PUT /zones/{id}/tess` et leurs schémas ;
//! - [`migration`] (T02) : la chaîne enregistrée d'une zone, tirée des
//!   anciennes clés, et l'écriture croisée vers elles.
//!
//! Compatibilité : les types de RÉPONSE tolèrent champs et valeurs inconnus
//! (repli `unknown`) ; seul le corps d'un `PUT` est lu strictement. Le document
//! de référence est `docs/tess/contrat.md`.

pub mod api;
pub mod chaine;
/// Chemin du signal TESS, rapproché du réel (lot T16).
pub mod chemin;
pub mod etage;
pub mod greffon;
pub mod migration;
pub mod prereglages;
pub mod resolution;
pub mod zones;

pub use api::{RefusTess, ReponseTessZone, schema_json, schema_json_lecture};
pub use chaine::{
    ChaineTess, ContexteValidation, Dither, ErreurChaine, EtagesFixes, Headroom, MaillonGreffon,
    Quantification, TauxDsd, Upsampling, VERSION_CHAINE,
};
pub use etage::{
    Disposition, ErreurEtage, EtageTess, EtapeFinale, FormatFlux, SortieFinale, preparer_chaine,
};
pub use greffon::{
    Canaux, Catalogue, Categorie, ContraintesPlace, DescripteurGreffon, Droit, ErreurCatalogue,
    GroupeExclusif, Origine, PolitiqueGroupe, Position, catalogue_de_reference,
};
pub use prereglages::{ID_PREREGLAGE_PURE, ModeApplication, PreReglage, PreReglages};
pub use resolution::{
    Badge, CodeEtape, ContexteExecution, ControleVolume, EtapeResolue, EtatVolume, LectureDsd,
    MotifEtape, PlacementVolume, Resolution, SourceFlux, VolumeZone,
};
pub use zones::{Applicabilite, MotifHorsTess, TransportZone};

/// Étages fixes (lot T07) : Headroom, Volume au rendu, bilan de niveau,
/// étape finale avec mise en forme du bruit.
pub mod etages_fixes;

/// Version du contrat TESS publiée par l'API (`contract_version`). Elle monte
/// quand un champ change de SENS ; un ajout facultatif (champ, valeur
/// d'énumération) ne la fait pas monter, puisque les clients les ignorent.
pub const VERSION_CONTRAT: u32 = 1;

/// Clé de réglage qui porte la chaîne d'une zone (lot T02, [`migration`]).
pub fn cle_chaine_zone(zone_id: i64) -> String {
    format!("zone_{zone_id}_tess_chain")
}

#[cfg(test)]
mod tests {
    #[test]
    fn la_cle_de_la_chaine_suit_le_modele_des_cles_de_zone() {
        assert_eq!(super::cle_chaine_zone(7), "zone_7_tess_chain");
        assert_eq!(super::cle_chaine_zone(-1), "zone_-1_tess_chain");
    }
}
