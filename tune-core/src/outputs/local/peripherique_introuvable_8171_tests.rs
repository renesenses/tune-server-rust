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

// ---------------------------------------------------------------------------
// Retour rc12 (fil 2708, réponse 11420) : DAC éteint, le son part encore sur
// les haut-parleurs du PC.
// ---------------------------------------------------------------------------

const ID_DAC: &str = "wasapi:{0.0.0.00000000}.{dac-denafrips}";
const ID_REALTEK: &str = "wasapi:{0.0.0.00000000}.{realtek}";

/// LA CAUSE, rejouée : la zone du DAC porte le nom « Haut-parleurs » et
/// l'identifiant du DAC. DAC éteint, la résolution par NOM retrouve la carte
/// Realtek — pas un repli sur la sortie par défaut, un HOMONYME : la règle de
/// la rc12 ne pouvait pas le voir.
#[test]
fn dac_eteint_le_nom_haut_parleurs_retombait_sur_la_carte_du_pc_8171() {
    let parc = [wasapi(ID_REALTEK, "Haut-parleurs")];
    let resolution = resolve_device(
        "Haut-parleurs",
        Some(ID_DAC),
        Some(HOTE_WASAPI),
        HOTE_WASAPI,
        &parc,
    );
    assert_eq!(
        resolution,
        DeviceResolution::Matched(DeviceMatch::ByDisplayName(0)),
        "la résolution par nom choisit bien la carte du PC : c'est le constat"
    );
    // … et la règle de l'homonyme l'écarte.
    assert!(
        crate::outputs::peripherique_introuvable::le_nom_designe_un_autre_endpoint(
            Some(ID_DAC),
            &parc[0].endpoint_id,
            parc.iter().map(|p| p.endpoint_id.as_str()),
        )
    );
}

/// CONTRE-ÉPREUVE : DAC allumé, il est retrouvé par SON identifiant, quel que
/// soit l'ordre des homonymes.
#[test]
fn dac_allume_retrouve_par_son_identifiant_parmi_les_homonymes_8171() {
    for parc in [
        [
            wasapi(ID_REALTEK, "Haut-parleurs"),
            wasapi(ID_DAC, "Haut-parleurs"),
        ],
        [
            wasapi(ID_DAC, "Haut-parleurs"),
            wasapi(ID_REALTEK, "Haut-parleurs"),
        ],
    ] {
        let resolution = resolve_device(
            "Haut-parleurs",
            Some(ID_DAC),
            Some(HOTE_WASAPI),
            HOTE_WASAPI,
            &parc,
        );
        let DeviceResolution::Matched(m) = resolution else {
            panic!("{resolution:?}");
        };
        assert_eq!(parc[m.index()].endpoint_id, ID_DAC);
        assert!(matches!(m, DeviceMatch::ByEndpointId(_)));
    }
}

/// L'énumération sans sonde nomme les endpoints comme la découverte : mêmes
/// rangs `(n)`, donc mêmes `local:{nom}` que ceux stockés dans les zones.
#[test]
fn la_liste_sans_sonde_nomme_comme_la_decouverte_8171() {
    let parc = [
        wasapi(ID_REALTEK, "Haut-parleurs"),
        wasapi("wasapi:{x}.{spdif}", "Realtek Digital Output"),
        wasapi(ID_DAC, "Haut-parleurs"),
        wasapi("wasapi:{x}.{dac2}", "DENAFRIPS USB DAC"),
    ];
    let noms: Vec<String> = super::resolution::nommer_comme_la_decouverte(&parc)
        .into_iter()
        .map(|(nom, _, _)| nom)
        .collect();
    assert_eq!(
        noms,
        [
            "Haut-parleurs",
            "Realtek Digital Output",
            "Haut-parleurs (2)",
            "DENAFRIPS USB DAC"
        ]
    );
}
