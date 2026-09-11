//! Une zone dont l'endpoint est inconnu doit ouvrir le PCM MATÉRIEL (#1655).
//!
//! ## Ce que ces gardes tiennent
//!
//! #3240 a appris à la DÉCOUVERTE d'écarter les greffons ALSA (`dmix:`,
//! `sysdefault:`, `plughw:`…) au profit du `hw:` : c'est ce qui a levé le
//! plafond à 48 kHz de GgB sur l'Eversolo DAC-Z8. Mais la RÉSOLUTION
//! (`resolve_device`) ne voit pas cette liste fusionnée — elle reçoit la liste
//! BRUTE de `host.output_devices()`, où les dix PCM de la carte figurent tous
//! et portent tous le MÊME nom. Quand l'identifiant d'endpoint manque, l'étape
//! « nom d'affichage » rendait le PREMIER énuméré : un greffon. Le plafond
//! rentrait par la porte de derrière, sur le seul chemin qui n'a pas
//! d'endpoint à présenter — `recreate_local_and_play`.
//!
//! Les cinq témoins ci-dessous fixent la règle ET ses bornes : l'appariement
//! par endpoint reste souverain, Windows et macOS ne bougent pas d'un pouce, et
//! une machine sans aucun `hw:` (PipeWire seul) garde mot pour mot le
//! comportement d'avant.
use super::{DeviceIdentity, DeviceMatch, DeviceResolution, resolve_device};

const NOM: &str = "Eversolo DAC-Z8, USB Audio";

fn alsa(pcm: &str) -> DeviceIdentity {
    DeviceIdentity {
        endpoint_id: format!("alsa:{pcm}"),
        raw_name: NOM.to_string(),
        host: "Alsa".to_string(),
    }
}

/// L'ordre d'énumération d'alsa-lib, tel que `snd_device_name_hint` le rend :
/// les greffons d'abord, le matériel enfoui au milieu.
fn carte_alsa_complete() -> Vec<DeviceIdentity> {
    vec![
        alsa("sysdefault:CARD=DACZ8"),
        alsa("front:CARD=DACZ8,DEV=0"),
        alsa("dmix:CARD=DACZ8,DEV=0"),
        alsa("hw:CARD=DACZ8,DEV=0"),
        alsa("plughw:CARD=DACZ8,DEV=0"),
    ]
}

fn rang_du_pcm(candidates: &[DeviceIdentity], pcm: &str) -> usize {
    candidates
        .iter()
        .position(|c| c.endpoint_id == format!("alsa:{pcm}"))
        .expect("le PCM cherché doit être dans la liste")
}

/// Le cas de GgB, sur le chemin SANS endpoint : la zone ne connaît que son nom
/// d'affichage, et c'est le `hw:` qui doit être ouvert — pas le `sysdefault:`
/// que l'ordre d'alsa-lib place en tête.
#[test]
fn une_zone_sans_endpoint_ouvre_le_pcm_materiel_et_non_le_greffon() {
    let candidates = carte_alsa_complete();
    let resolution = resolve_device(NOM, None, Some("Alsa"), "Alsa", &candidates);

    let DeviceResolution::Matched(appariement) = resolution else {
        panic!("le nom devait s'apparier, et a rendu {resolution:?}");
    };
    let retenu = &candidates[appariement.index()];
    assert_eq!(
        retenu.endpoint_id, "alsa:hw:CARD=DACZ8,DEV=0",
        "le PCM ouvert est un greffon : c'est le plafond 48 kHz de #1655 qui \
         revient par le chemin de résolution — appariement {appariement:?}"
    );
    assert_eq!(
        appariement,
        DeviceMatch::ByAlsaHardwarePcm {
            retenu: rang_du_pcm(&candidates, "hw:CARD=DACZ8,DEV=0"),
            greffon: rang_du_pcm(&candidates, "sysdefault:CARD=DACZ8"),
        },
        "la bascule doit être NOMMÉE — c'est elle que le journal publie"
    );
}

/// L'étape 1 garde le dernier mot : un endpoint explicite est une identité, pas
/// une préférence. Sans cette borne, la correction ci-dessus détournerait une
/// zone que l'utilisateur a délibérément posée sur un greffon (partage du DAC
/// entre applications).
#[test]
fn l_appariement_par_endpoint_reste_souverain() {
    let candidates = carte_alsa_complete();
    let resolution = resolve_device(
        NOM,
        Some("alsa:dmix:CARD=DACZ8,DEV=0"),
        Some("Alsa"),
        "Alsa",
        &candidates,
    );
    assert_eq!(
        resolution,
        DeviceResolution::Matched(DeviceMatch::ByEndpointId(rang_du_pcm(
            &candidates,
            "dmix:CARD=DACZ8,DEV=0"
        ))),
        "un endpoint demandé nommément doit être rendu tel quel"
    );
}

/// Windows : deux DAC physiques distincts s'annoncent tous deux
/// « Haut-Parleurs », et seul le rang `(n)` les distingue (#2272). Aucun de
/// leurs identifiants n'est un PCM ALSA : la règle ne doit pas s'armer, et le
/// second homonyme doit rester joignable.
#[test]
fn sur_wasapi_le_departage_par_rang_ne_bouge_pas() {
    let candidates = vec![
        DeviceIdentity {
            endpoint_id: "{0.0.0.00000000}.{70600b1a-0000-0000-0000-000000000001}".to_string(),
            raw_name: "Haut-Parleurs".to_string(),
            host: "Wasapi".to_string(),
        },
        DeviceIdentity {
            endpoint_id: "{0.0.0.00000000}.{70600b1a-0000-0000-0000-000000000002}".to_string(),
            raw_name: "Haut-Parleurs".to_string(),
            host: "Wasapi".to_string(),
        },
    ];
    assert_eq!(
        resolve_device("Haut-Parleurs", None, Some("Wasapi"), "Wasapi", &candidates),
        DeviceResolution::Matched(DeviceMatch::ByDisplayName(0)),
    );
    assert_eq!(
        resolve_device(
            "Haut-Parleurs (2)",
            None,
            Some("Wasapi"),
            "Wasapi",
            &candidates
        ),
        DeviceResolution::Matched(DeviceMatch::ByDisplayName(1)),
    );
}

/// Une machine où PipeWire est le seul chemin praticable : aucune variante
/// n'est un `hw:`. Le critère ne départage rien et le comportement d'avant
/// s'applique mot pour mot.
#[test]
fn sans_aucun_pcm_materiel_le_comportement_d_avant_tient() {
    let candidates = vec![alsa("pipewire"), alsa("default"), alsa("sysdefault:CARD=X")];
    assert_eq!(
        resolve_device(NOM, None, Some("Alsa"), "Alsa", &candidates),
        DeviceResolution::Matched(DeviceMatch::ByDisplayName(0)),
    );
}

/// Deux `hw:` homonymes (deux sous-périphériques de la même carte) : le
/// vainqueur ne doit pas dépendre de l'ordre d'énumération d'alsa-lib. Même
/// dernier cran que `variante_alsa_candidate_l_emporte` — le plus petit
/// identifiant.
#[test]
fn le_departage_entre_deux_materiels_ne_depend_pas_de_l_ordre() {
    let dans_un_sens = vec![
        alsa("dmix:CARD=DACZ8,DEV=0"),
        alsa("hw:CARD=DACZ8,DEV=1"),
        alsa("hw:CARD=DACZ8,DEV=0"),
    ];
    let dans_l_autre = vec![
        alsa("dmix:CARD=DACZ8,DEV=0"),
        alsa("hw:CARD=DACZ8,DEV=0"),
        alsa("hw:CARD=DACZ8,DEV=1"),
    ];
    for candidates in [dans_un_sens, dans_l_autre] {
        let resolution = resolve_device(NOM, None, Some("Alsa"), "Alsa", &candidates);
        let DeviceResolution::Matched(appariement) = resolution else {
            panic!("le nom devait s'apparier, et a rendu {resolution:?}");
        };
        assert_eq!(
            candidates[appariement.index()].endpoint_id,
            "alsa:hw:CARD=DACZ8,DEV=0",
            "le vainqueur dépend de l'ordre d'énumération"
        );
    }
}
