//! TESS — *Tune Engine for Sound Shaping* : le **contrat** (lot T01).
//!
//! Ce module ne contient QUE des types : aucune ligne n'est branchée dans la
//! lecture. Il fixe ce que les lots suivants consomment :
//!
//! - [`etage`] : le trait [`EtageTess`] d'un étage de traitement, qui travaille
//!   en `f64` entrelacé (décision Q5 : 64 bits réels, une seule quantification
//!   à la fin) ;
//! - [`greffon`] : le [`DescripteurGreffon`] (id, droit Free/Premium, schéma de
//!   réglages, contraintes de place) et le catalogue de référence ;
//! - [`chaine`] : la [`ChaineTess`] versionnée d'une zone, ses étages fixes et
//!   l'ordre réel des étapes (décision Q9) ;
//! - [`prereglages`] : préréglages de chaîne et mode d'application
//!   (maquette A2) ;
//! - [`zones`] : où TESS s'applique (décision Q8 : HQPlayer et Diretta hors
//!   TESS, avec un motif) ;
//! - [`api`] : les corps JSON de `GET/PUT /zones/{id}/tess` et leur schéma.
//!
//! Le document de référence est `docs/tess/contrat.md`.

pub mod api;
pub mod chaine;
pub mod etage;
pub mod greffon;
pub mod prereglages;
pub mod zones;

pub use api::{RefusTess, ReponseTessZone, schema_json};
pub use chaine::{
    ChaineTess, CodeEtape, ContexteValidation, ErreurChaine, EtagesFixes, Etape, Headroom,
    MaillonGreffon, PlacementVolume, Quantification, TauxDsd, Upsampling, VERSION_CHAINE,
};
pub use etage::{ErreurEtage, EtageTess, FormatFlux, TamponTess};
pub use greffon::{
    Canaux, Catalogue, Categorie, ContraintesPlace, DescripteurGreffon, Droit, ErreurCatalogue,
    Origine, Position, catalogue_de_reference,
};
pub use prereglages::{ID_PREREGLAGE_PURE, ModeApplication, PreReglage, PreReglages};
pub use zones::{Applicabilite, MotifHorsTess, TransportZone};

/// Version du contrat TESS publiée par l'API (`contract_version`). Elle monte
/// quand un champ change de sens ; un ajout facultatif ne la fait pas monter.
pub const VERSION_CONTRAT: u32 = 1;

/// Clé de réglage qui portera la chaîne d'une zone (lot T02).
pub fn cle_chaine_zone(zone_id: i64) -> String {
    format!("zone_{zone_id}_tess_chain")
}

#[cfg(test)]
mod tests {
    #[test]
    fn la_cle_de_la_chaine_suit_le_modele_des_cles_de_zone() {
        assert_eq!(super::cle_chaine_zone(7), "zone_7_tess_chain");
    }
}
