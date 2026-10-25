//! #5575 — un renderer dont le `SetVolume` est DÉCORATIF : il l'acquitte, le
//! relit fidèlement par `GetVolume`, et n'en fait rien au son.
//!
//! Cause établie dans le code de l'appareil, pas supposée : le « Diretta UPnP
//! Renderer » (projet libre `cometdom/DirettaRendererUPnP`, `src/UPnPDevice.cpp`,
//! `actionSetVolume`) range `DesiredVolume` dans `m_volume`, émet l'évènement
//! RenderingControl et répond OK. Aucune autre ligne du projet ne lit
//! `m_volume` : ni `AudioEngine`, ni `DirettaSync`. Le volume y est un état
//! affiché, jamais un gain — le flux part bit-perfect vers la cible Diretta.
//!
//! Le journal de Tades (fil 2071, 05/10) le montrait sans pouvoir le dire :
//! 31 `device_set_volume_sending`, aucun refus, et la relecture rc3
//! (`dlna_volume_relu … applique=oui`) ne peut que confirmer, puisque
//! l'appareil relit ce qu'il a rangé. Le curseur bougeait, la valeur tenait en
//! base, rien ne changeait aux enceintes : le « curseur muet » de l'issue.
//!
//! Règle : sur un tel appareil, une consigne SOUS 100 % est REFUSÉE avec un
//! message qui dit pourquoi et où régler le volume. Le refus suit le chemin
//! déjà établi par #1393 (`Err` → curseur ramené, `zone.playback_error` à
//! l'écran). Une consigne à 100 % reste acceptée : c'est exactement ce que
//! l'appareil restitue, quoi qu'on lui envoie — et c'est ce qui laisse passer
//! l'armement du volume fixe (#2395).
//!
//! Reconnaissance : le `modelName` du descriptif (« Diretta UPnP Renderer »,
//! écrit en dur par `DirettaRenderer.cpp`), ou à défaut l'UDN par défaut du
//! projet (`generateUUID()` : `diretta-renderer-<hash du nom d'hôte>`), celui
//! du journal de Tades (`uuid:diretta-renderer-a7366917f9ddf66e`).

/// `modelName` que pose `DirettaRenderer.cpp` (`upnpConfig.modelName`).
const MODELE_DIRETTA: &str = "diretta upnp renderer";
/// Préfixe de l'UDN que `generateUUID()` produit quand `--uuid` n'est pas donné.
const UDN_DIRETTA: &str = "uuid:diretta-renderer-";

/// L'appareil acquitte-t-il `SetVolume` sans jamais l'appliquer ?
pub(crate) fn volume_decoratif(device_id: &str, modele: Option<&str>) -> bool {
    let par_modele = modele.is_some_and(|m| m.trim().eq_ignore_ascii_case(MODELE_DIRETTA));
    let par_udn = device_id
        .get(..UDN_DIRETTA.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(UDN_DIRETTA));
    par_modele || par_udn
}

/// La consigne doit-elle être refusée ? Seul le plein volume, que l'appareil
/// restitue de toute façon, passe.
pub(crate) fn refuser_la_consigne(volume: f64) -> bool {
    !(volume.is_finite() && volume >= 1.0 - 1e-6)
}

/// Le message lu par l'auditeur : il nomme l'appareil, dit pourquoi rien ne
/// change, et où régler le volume.
pub(crate) fn message_de_refus(nom: &str) -> String {
    format!(
        "« {nom} » (Diretta UPnP Renderer) accepte la commande de volume sans \
         l'appliquer : le son reste au maximum. Réglez le volume sur l'appareil \
         lui-même (DAC ou amplificateur), ou passez la zone en volume fixe."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_renderer_de_tades_est_reconnu_par_son_udn_et_par_son_modele() {
        assert!(volume_decoratif(
            "uuid:diretta-renderer-a7366917f9ddf66e",
            None
        ));
        assert!(volume_decoratif(
            "uuid:autre",
            Some("Diretta UPnP Renderer")
        ));
        assert!(!volume_decoratif(
            "uuid:3E151150-D9C0-11F0-A7C6-800A805C2689",
            Some("DMP-A6")
        ));
        assert!(!volume_decoratif("mock-dlna-001", None));
    }

    #[test]
    fn seul_le_plein_volume_passe() {
        assert!(!refuser_la_consigne(1.0));
        assert!(refuser_la_consigne(0.03));
        assert!(refuser_la_consigne(0.999));
        assert!(refuser_la_consigne(f64::NAN));
    }
}
