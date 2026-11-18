//! #8696 (hors sujet relevé dans le fil 2911) — un fichier multicanal vers
//! une sortie WASAPI exclusive qui a moins de voies.
//!
//! Un DSF 6 canaux envoyé vers un SMSL SU-8 (DAC stéréo) : l'ouverture
//! exclusive échouait sur « aucun format PCM accepté pour 6ch 176400Hz … »,
//! et la zone s'arrêtait sur ce message de pilote. La cause, elle, est
//! simple : la sortie déclare 2 voies, le fichier en a 6. Tune ne mélange
//! pas les voies d'office en exclusif (ce ne serait plus bit-perfect, et le
//! choix du mélange appartient à l'auditeur) : le message le dit, et dit
//! comment l'écouter quand même.
//!
//! Hors FFI, pour être jugé par `cargo test` sur toutes les plateformes.

/// Le message clair, quand la sortie déclare MOINS de voies que le flux.
/// `None` si les voies déclarées sont inconnues ou suffisantes : le refus a
/// alors une autre cause, et le message de négociation reste le bon.
pub(crate) fn message_voies_en_trop(
    peripherique: &str,
    voies_du_flux: u32,
    voies_declarees: Option<u32>,
) -> Option<String> {
    let declarees = voies_declarees.filter(|&d| d > 0 && d < voies_du_flux)?;
    Some(format!(
        "{peripherique} n'a que {declarees} voie(s) et ce fichier en a {voies_du_flux} : \
         le mode exclusif ne mélange pas les voies. Pour l'écouter sur cette sortie, \
         réglez une grille {voies_du_flux} → {declarees} dans le greffon « Réaffectation \
         des canaux » de la zone"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_dsf_six_canaux_vers_un_dac_stereo_se_dit_en_clair() {
        let m = message_voies_en_trop("Sortie SMSL SU-8", 6, Some(2)).unwrap();
        assert!(m.contains("Sortie SMSL SU-8 n'a que 2 voie(s) et ce fichier en a 6"));
        assert!(m.contains("grille 6 → 2 dans le greffon « Réaffectation des canaux »"));
    }

    /// Contre-épreuve : un refus qui n'a rien à voir avec le nombre de voies
    /// garde le message de négociation.
    #[test]
    fn assez_de_voies_ou_voies_inconnues_ne_changent_rien() {
        assert_eq!(message_voies_en_trop("DAC", 2, Some(2)), None);
        assert_eq!(message_voies_en_trop("DAC", 2, Some(8)), None);
        assert_eq!(message_voies_en_trop("DAC", 6, None), None);
        assert_eq!(message_voies_en_trop("DAC", 6, Some(0)), None);
    }
}
