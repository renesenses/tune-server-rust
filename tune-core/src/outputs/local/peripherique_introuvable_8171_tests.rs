//! #8171 — Windows 10 Pro, zone « Denafrips » : le son sortait des
//! haut-parleurs du PC. Ces scènes rejouent la résolution sur des listes de
//! périphériques FICTIVES (aucune carte son ouverte, aucun son) et y ajoutent
//! la conduite que le chemin partagé applique au verdict « introuvable ».

use super::resolution::{DeviceIdentity, DeviceMatch, DeviceResolution, resolve_device};
use crate::outputs::peripherique_introuvable::{ConduiteSiIntrouvable, conduite_si_introuvable};

const HOTE_WASAPI: &str = "Wasapi";

fn wasapi(endpoint_id: &str, raw_name: &str) -> DeviceIdentity {
    DeviceIdentity {
        endpoint_id: endpoint_id.into(),
        raw_name: raw_name.into(),
        host: HOTE_WASAPI.into(),
    }
}

/// Ce que la zone ouvrirait sous Windows : un rang de la liste, ou rien.
/// La sortie par défaut n'est JAMAIS une réponse possible.
fn ouvert_sous_windows(resolution: &DeviceResolution) -> Option<usize> {
    match resolution {
        DeviceResolution::Matched(m) => Some(m.index()),
        DeviceResolution::ForeignHost { .. } => None,
        DeviceResolution::NotFound => match conduite_si_introuvable("windows") {
            ConduiteSiIntrouvable::Refuser => None,
            // Rang fictif hors liste : la sortie par défaut du système.
            ConduiteSiIntrouvable::OuvrirLaSortieSysteme => Some(usize::MAX),
        },
    }
}

/// Le pilote a renommé le DAC, mais la zone connaît son identifiant
/// d'endpoint : elle se RE-LIE au DAC, pas aux haut-parleurs.
#[test]
fn nom_change_avec_identifiant_connu_la_zone_se_relie_au_dac() {
    let vus = [
        wasapi("{realtek}", "Haut-parleurs"),
        wasapi("{denafrips}", "Haut-parleurs (Denafrips USB Audio)"),
    ];
    let r = resolve_device("Denafrips", Some("{denafrips}"), None, HOTE_WASAPI, &vus);
    assert_eq!(r, DeviceResolution::Matched(DeviceMatch::ByEndpointId(1)));
    assert_eq!(ouvert_sous_windows(&r), Some(1));
}

/// Identifiant absent (zone ancienne) et nom introuvable : la lecture est
/// REFUSÉE, jamais envoyée vers la sortie par défaut.
#[test]
fn identifiant_absent_et_nom_introuvable_la_lecture_est_refusee() {
    let vus = [
        wasapi("{realtek}", "Haut-parleurs"),
        wasapi("{hdmi}", "Téléviseur"),
    ];
    let r = resolve_device("Denafrips", None, None, HOTE_WASAPI, &vus);
    assert_eq!(r, DeviceResolution::NotFound);
    assert_eq!(
        ouvert_sous_windows(&r),
        None,
        "#8171 : le DAC est introuvable et la zone joue sur la sortie par défaut \
         (les haut-parleurs du PC) au lieu de refuser"
    );
}

/// L'identifiant mémorisé ne correspond plus à rien (DAC réinstallé : Windows
/// lui a donné un nouvel endpoint) et le nom a changé : refus.
#[test]
fn identifiant_perime_et_nom_change_la_lecture_est_refusee() {
    let vus = [
        wasapi("{realtek}", "Haut-parleurs"),
        wasapi("{nouveau}", "Speakers (USB Audio 2.0)"),
    ];
    let r = resolve_device("Denafrips", Some("{ancien}"), None, HOTE_WASAPI, &vus);
    assert_eq!(r, DeviceResolution::NotFound);
    assert_eq!(ouvert_sous_windows(&r), None);
}

/// Rang `(n)` renuméroté : la zone visait « Haut-Parleurs (2) », le DAC, et
/// l'un des deux homonymes a disparu. Le survivant peut être l'autre carte :
/// la deviner, c'est risquer le mauvais haut-parleur (#2272). Refus.
#[test]
fn rang_renumerote_la_lecture_est_refusee_et_non_devinee() {
    let vus = [wasapi("{realtek}", "Haut-Parleurs")];
    let r = resolve_device("Haut-Parleurs (2)", None, None, HOTE_WASAPI, &vus);
    assert_eq!(r, DeviceResolution::NotFound);
    assert_eq!(ouvert_sous_windows(&r), None);
}

/// Le cas nominal ne change pas : le DAC présent sous son nom est ouvert.
#[test]
fn dac_present_sous_son_nom_est_ouvert() {
    let vus = [
        wasapi("{realtek}", "Haut-parleurs"),
        wasapi("{denafrips}", "Denafrips"),
    ];
    let r = resolve_device("Denafrips", None, None, HOTE_WASAPI, &vus);
    assert_eq!(r, DeviceResolution::Matched(DeviceMatch::ByDisplayName(1)));
    assert_eq!(ouvert_sous_windows(&r), Some(1));
}
