//! #8173 — une zone locale apparaît indisponible EXACTEMENT quand l'ouverture
//! ne retrouverait pas son périphérique, et disponible sinon.
//!
//! `is_available` appliquait sa propre règle de nom (`n == name ||
//! n.contains(&name)`), différente de celle de l'ouverture
//! ([`resolve_device`]). Ces scènes rejouent les deux verdicts sur des listes
//! de périphériques FICTIVES (aucune carte son ouverte, aucun son) et exigent
//! qu'ils coïncident. Sans le correctif, chacune des scènes « divergence »
//! rougit : c'est la contre-épreuve notée dans la PR.

use super::resolution::{
    DeviceIdentity, DeviceMatch, DeviceResolution, resolve_device, zone_locale_disponible,
};

const HOTE_WASAPI: &str = "Wasapi";
const HOTE_ASIO: &str = "Asio";

fn vu(host: &str, endpoint_id: &str, raw_name: &str) -> DeviceIdentity {
    DeviceIdentity {
        endpoint_id: endpoint_id.into(),
        raw_name: raw_name.into(),
        host: host.into(),
    }
}

fn wasapi(endpoint_id: &str, raw_name: &str) -> DeviceIdentity {
    vu(HOTE_WASAPI, endpoint_id, raw_name)
}

/// Ce que l'ouverture ferait de la même zone sur la même liste : la zone
/// JOUE si et seulement si un rang est rendu. C'est la référence que la
/// disponibilité doit suivre.
fn l_ouverture_retrouve(
    requested: &str,
    endpoint_id: Option<&str>,
    origin_host: Option<&str>,
    open_host: &str,
    vus: &[DeviceIdentity],
) -> bool {
    matches!(
        resolve_device(requested, endpoint_id, origin_host, open_host, vus),
        DeviceResolution::Matched(_)
    )
}

// ── Divergences corrigées : « indisponible » alors que la lecture aboutit ──

/// Le pilote a renommé le DAC (Windows le fait au changement de taux, #2269),
/// mais la zone connaît son identifiant d'endpoint : la lecture se RE-LIE au
/// DAC. La zone doit donc rester disponible.
#[test]
fn dac_renomme_mais_retrouve_par_son_identifiant_reste_disponible() {
    let vus = [
        wasapi("{realtek}", "Haut-parleurs"),
        wasapi("{denafrips}", "Speakers (USB Audio 2.0)"),
    ];
    assert_eq!(
        resolve_device("Denafrips", Some("{denafrips}"), None, HOTE_WASAPI, &vus),
        DeviceResolution::Matched(DeviceMatch::ByEndpointId(1)),
        "prémisse : l'ouverture retrouve le DAC par son identifiant"
    );
    assert!(
        zone_locale_disponible("Denafrips", Some("{denafrips}"), None, HOTE_WASAPI, &vus),
        "#8173 : un appareil renommé par son pilote mais retrouvé par son identifiant \
         est annoncé indisponible, alors que la lecture y aboutit"
    );
}

/// Deux DAC homonymes : la découverte a nommé le second « Haut-Parleurs (2) »
/// et c'est ce nom que la zone porte. Le pilote, lui, ne connaît que
/// « Haut-Parleurs » : la règle par sous-chaîne ne le trouvait jamais.
#[test]
fn second_homonyme_de_rang_2_reste_disponible() {
    let vus = [
        wasapi("{realtek}", "Haut-Parleurs"),
        wasapi("{dac-usb}", "Haut-Parleurs"),
    ];
    assert_eq!(
        resolve_device("Haut-Parleurs (2)", None, None, HOTE_WASAPI, &vus),
        DeviceResolution::Matched(DeviceMatch::ByDisplayName(1)),
        "prémisse : l'ouverture reconstruit le rang (2) de la découverte"
    );
    assert!(
        zone_locale_disponible("Haut-Parleurs (2)", None, None, HOTE_WASAPI, &vus),
        "#8173 : la zone du second homonyme est annoncée indisponible alors que \
         l'ouverture la retrouve au rang (2)"
    );
}

/// L'ouverture compare les noms sans la casse ; la disponibilité la comparait
/// avec. Une zone ancienne écrite « haut-parleurs » jouait et s'affichait
/// pourtant hors ligne.
#[test]
fn la_casse_du_nom_ne_rend_pas_la_zone_indisponible() {
    let vus = [wasapi("{realtek}", "Haut-parleurs")];
    assert!(l_ouverture_retrouve(
        "haut-parleurs",
        None,
        None,
        HOTE_WASAPI,
        &vus
    ));
    assert!(
        zone_locale_disponible("haut-parleurs", None, None, HOTE_WASAPI, &vus),
        "#8173 : la disponibilité est sensible à la casse, l'ouverture ne l'est pas"
    );
}

/// CoreAudio et PipeWire rendent des noms verbeux : la zone, créée hier, porte
/// « Denafrips USB Audio » et le pilote dit aujourd'hui « Denafrips ».
/// L'ouverture accepte la sous-chaîne dans les deux sens, dès lors qu'elle
/// désigne UNE seule candidate ; la disponibilité ne regardait qu'un sens.
#[test]
fn sous_chaine_dans_l_autre_sens_reste_disponible() {
    let vus = [
        wasapi("{realtek}", "Haut-parleurs"),
        wasapi("{denafrips}", "Denafrips"),
    ];
    assert_eq!(
        resolve_device("Denafrips USB Audio", None, None, HOTE_WASAPI, &vus),
        DeviceResolution::Matched(DeviceMatch::BySubstring(1)),
        "prémisse : l'ouverture retrouve la seule candidate contenue dans le nom"
    );
    assert!(
        zone_locale_disponible("Denafrips USB Audio", None, None, HOTE_WASAPI, &vus),
        "#8173 : la disponibilité ne lit la sous-chaîne que dans un sens, \
         l'ouverture dans les deux"
    );
}

// ── Divergences corrigées : « disponible » alors que la lecture est refusée ──

/// Deux candidates contiennent le nom demandé : l'ouverture préfère l'aveu
/// d'ignorance au mauvais DAC (#2272) et REFUSE. La zone ne doit pas être
/// annoncée disponible pour échouer au clic.
#[test]
fn nom_contenu_par_deux_candidates_est_indisponible_comme_a_l_ouverture() {
    let vus = [
        wasapi("{a}", "DAC USB (gauche)"),
        wasapi("{b}", "DAC USB (droite)"),
    ];
    assert_eq!(
        resolve_device("DAC USB", None, None, HOTE_WASAPI, &vus),
        DeviceResolution::NotFound,
        "prémisse : l'ouverture refuse une sous-chaîne ambiguë"
    );
    assert!(
        !zone_locale_disponible("DAC USB", None, None, HOTE_WASAPI, &vus),
        "#8173 : la zone est annoncée disponible, et la lecture sera refusée \
         (sous-chaîne ambiguë)"
    );
}

/// Le nom de la zone vient de WASAPI, l'hôte ouvert est ASIO : l'ouverture
/// refuse sans repli (#3230), même si un périphérique ASIO porte ce nom.
#[test]
fn nom_venu_d_un_autre_hote_est_indisponible() {
    let vus = [vu(HOTE_ASIO, "asio:0", "Haut-parleurs")];
    assert!(matches!(
        resolve_device("Haut-parleurs", None, Some(HOTE_WASAPI), HOTE_ASIO, &vus),
        DeviceResolution::ForeignHost { .. }
    ));
    assert!(
        !zone_locale_disponible("Haut-parleurs", None, Some(HOTE_WASAPI), HOTE_ASIO, &vus),
        "#8173 : la zone est annoncée disponible, et la lecture sera refusée \
         (nom venu d'un autre hôte)"
    );
}

/// Rang `(n)` renuméroté : un des deux homonymes a disparu. L'ouverture ne
/// devine pas le survivant (#2272) ; la disponibilité non plus.
#[test]
fn rang_renumerote_est_indisponible_comme_a_l_ouverture() {
    let vus = [wasapi("{realtek}", "Haut-Parleurs")];
    assert!(!l_ouverture_retrouve(
        "Haut-Parleurs (2)",
        None,
        None,
        HOTE_WASAPI,
        &vus
    ));
    assert!(!zone_locale_disponible(
        "Haut-Parleurs (2)",
        None,
        None,
        HOTE_WASAPI,
        &vus
    ));
}

// ── Cas nominaux, inchangés ──

/// DAC éteint ou débranché : plus rien de ce nom ni de cet identifiant.
#[test]
fn dac_debranche_est_indisponible() {
    let vus = [
        wasapi("{realtek}", "Haut-parleurs"),
        wasapi("{hdmi}", "Téléviseur"),
    ];
    assert!(!zone_locale_disponible(
        "Denafrips",
        Some("{denafrips}"),
        None,
        HOTE_WASAPI,
        &vus
    ));
    assert!(!zone_locale_disponible(
        "Denafrips",
        None,
        None,
        HOTE_WASAPI,
        &vus
    ));
}

/// Identifiant périmé (DAC réinstallé) ET nom changé : indisponible.
#[test]
fn identifiant_perime_et_nom_change_est_indisponible() {
    let vus = [
        wasapi("{realtek}", "Haut-parleurs"),
        wasapi("{nouveau}", "Speakers (USB Audio 2.0)"),
    ];
    assert!(!zone_locale_disponible(
        "Denafrips",
        Some("{ancien}"),
        None,
        HOTE_WASAPI,
        &vus
    ));
}

/// Le DAC présent sous son nom : disponible, hier comme aujourd'hui.
#[test]
fn dac_present_sous_son_nom_est_disponible() {
    let vus = [
        wasapi("{realtek}", "Haut-parleurs"),
        wasapi("{denafrips}", "Denafrips"),
    ];
    assert!(zone_locale_disponible(
        "Denafrips",
        None,
        None,
        HOTE_WASAPI,
        &vus
    ));
}

/// Énumération vide (pilote tenu par une autre application, #4093) :
/// « introuvable », donc indisponible — comme avant.
#[test]
fn enumeration_vide_est_indisponible() {
    assert!(!zone_locale_disponible(
        "Denafrips",
        Some("{denafrips}"),
        None,
        HOTE_WASAPI,
        &[]
    ));
}

// ── Garde de site : « écrit mais pas branché » ──

/// La décision ne vaut que si `LocalOutput::is_available` la consulte, avec
/// l'identifiant d'endpoint ET l'hôte d'origine de la zone, sur la liste que
/// l'ouverture lit. `is_available` vit dans un `spawn_blocking` sur un hôte
/// cpal réel qu'aucune épreuve n'ouvre ici : on relit la source.
#[test]
fn is_available_consulte_la_regle_d_ouverture_avec_l_identifiant_et_l_hote() {
    let source = include_str!("../local.rs");
    let debut = source
        .find("async fn is_available(&self) -> bool {")
        .expect("LocalOutput::is_available introuvable");
    let fin = source[debut..]
        .find("\n    }\n")
        .map(|i| debut + i)
        .expect("fin du corps de is_available");
    let corps = &source[debut..fin];

    assert!(
        !corps.contains("n.contains(&name)"),
        "#8173 : is_available applique encore sa propre règle de nom au lieu de \
         celle de l'ouverture ; corps lu :\n{corps}"
    );
    assert!(
        corps.contains("enumerer_les_candidates(&host)"),
        "#8173 : is_available doit lire la même liste de périphériques que \
         l'ouverture (puits nuls écartés) ; corps lu :\n{corps}"
    );
    assert!(
        corps.contains("zone_locale_disponible("),
        "#8173 : is_available ne consulte pas zone_locale_disponible ; corps lu :\n{corps}"
    );
    assert!(
        corps.contains("endpoint_id.as_deref()") && corps.contains("origin_host.as_deref()"),
        "#8173 : is_available doit passer l'identifiant d'endpoint ET l'hôte \
         d'origine de la zone, sans quoi un DAC renommé reste « indisponible » ; \
         corps lu :\n{corps}"
    );
}
