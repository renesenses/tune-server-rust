use super::dlna_cap_16bit_applies;

/// Le cas de #3183 : aucun drapeau de zone, mais l'appareil choisi pour la
/// zone est au catalogue avec `force_16bit` (Ruark R3). Le plafond s'applique.
#[test]
fn le_quirk_catalogue_suffit_sans_drapeau_de_zone() {
    assert!(dlna_cap_16bit_applies(true, 24, false, true));
}

/// Le drapeau de zone seul, comme avant (#1137).
#[test]
fn le_drapeau_de_zone_suffit_sans_quirk() {
    assert!(dlna_cap_16bit_applies(true, 24, true, false));
}

/// Additif : le quirk ne peut jamais DÉSACTIVER un plafond demandé.
#[test]
fn les_deux_sources_ensemble_plafonnent() {
    assert!(dlna_cap_16bit_applies(true, 24, true, true));
}

/// Sans objet jusqu'à 16 bits : rien à plafonner, quelle que soit la source.
#[test]
fn une_source_16_bits_n_est_jamais_plafonnee() {
    assert!(!dlna_cap_16bit_applies(true, 16, true, true));
}

/// Hors sortie réseau, le plafond DLNA n'existe pas.
#[test]
fn hors_sortie_reseau_le_plafond_ne_s_applique_pas() {
    assert!(!dlna_cap_16bit_applies(false, 24, true, true));
}

/// Aucune source : pas de plafond.
#[test]
fn sans_drapeau_ni_quirk_pas_de_plafond() {
    assert!(!dlna_cap_16bit_applies(true, 24, false, false));
}
