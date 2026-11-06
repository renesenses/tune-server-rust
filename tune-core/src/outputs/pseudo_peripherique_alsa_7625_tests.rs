//! #7625 — le segfault `tokio-rt-worker` (rc9, puis rc10) est celui de #7263 :
//! le relevé du 05/11 s'arrête juste après
//! `local_audio_device_found … endpoint_id=alsa:speexrate`, le PCM suivant
//! est `alsa:jack`, et le core dump a chargé le greffon jack, libjack,
//! speexrate et samplerate. Les inventaires s'y suivent toutes les minutes
//! (08:43:07, 08:44:07).
//!
//! Ce témoin rejoue la séquence de PCM du relevé du 05/11 : un inventaire
//! ALSA dans l'ordre du journal (`speexrate` puis `jack`, avec `pipewire` et
//! `pulse` dans le même passage), à travers la porte unique par laquelle
//! `outputs::local::parc` obtient les capacités. Il compte les PCM réellement
//! OUVERTS : aucun greffon logiciel ne doit l'être, et les sorties
//! matérielles le restent.
//!
//! Il ne dit rien de la cadence des inventaires : la garde qui empêche la
//! fiche de diagnostic de refaire l'inventaire chaque minute est le témoin
//! serveur `la_fiche_de_diagnostic_ne_rebalaye_pas_un_parc_recent_7263`
//! (https://github.com/renesenses/tune-server-rust/pull/7471).

use super::capacites_a_l_inventaire;

/// L'inventaire, dans l'ordre où `aplay -L` et cpal le rendent sur une
/// machine où le greffon jack d'alsa-plugins est installé. Le couple
/// `speexrate` → `jack` est celui du journal.
const INVENTAIRE_DU_RELEVE: &[(&str, bool)] = &[
    // (endpoint_id, sortie à sonder ?)
    ("alsa:default", true),
    ("alsa:pipewire", true),
    ("alsa:lavrate", false),
    ("alsa:samplerate", false),
    ("alsa:speexrate", false),
    ("alsa:jack", false),
    ("alsa:oss", false),
    ("alsa:pulse", true),
    ("alsa:speex", false),
    ("alsa:upmix", false),
    ("alsa:vdownmix", false),
    ("alsa:samplerate_best", false),
    ("alsa:speexrate_medium", false),
    ("alsa:hw:CARD=PCH,DEV=0", true),
    ("alsa:plughw:CARD=PCH,DEV=0", true),
    ("alsa:sysdefault:CARD=PCH", true),
    ("alsa:front:CARD=PCH,DEV=0", true),
    ("alsa:dmix:CARD=PCH,DEV=0", true),
    ("alsa:hw:CARD=DAC,DEV=0", true),
];

/// Une passe d'inventaire : les PCM que la sonde a ouverts, dans l'ordre.
fn pcm_ouverts_par_une_passe() -> Vec<&'static str> {
    let mut ouverts = Vec::new();
    for (endpoint_id, _) in INVENTAIRE_DU_RELEVE {
        capacites_a_l_inventaire(endpoint_id, || ouverts.push(*endpoint_id), || ());
    }
    ouverts
}

#[test]
fn l_inventaire_du_releve_n_ouvre_ni_jack_ni_les_convertisseurs_7625() {
    let ouverts = pcm_ouverts_par_une_passe();
    for greffon in INVENTAIRE_DU_RELEVE
        .iter()
        .filter(|(_, sonder)| !sonder)
        .map(|(endpoint_id, _)| *endpoint_id)
    {
        assert!(
            !ouverts.contains(&greffon),
            "l'inventaire a ouvert {greffon} ; l'ouvrir charge une bibliothèque native \
             dans le processus (greffon jack, libjack, speexrate, samplerate), c'est le \
             segfault de #7625 / #7263"
        );
    }
}

#[test]
fn les_sorties_materielles_du_releve_restent_sondees_7625() {
    // Le piège : une garde qui n'ouvrirait plus rien passerait le témoin
    // précédent, mais ôterait aux cartes et aux DAC leurs capacités mesurées.
    let attendus: Vec<&str> = INVENTAIRE_DU_RELEVE
        .iter()
        .filter(|(_, sonder)| *sonder)
        .map(|(endpoint_id, _)| *endpoint_id)
        .collect();
    assert_eq!(
        pcm_ouverts_par_une_passe(),
        attendus,
        "seules les sorties qui mènent à du matériel doivent être sondées, toutes"
    );
}
