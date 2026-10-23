//! #6739 — témoins de la séquence du rapport (fil 2366/2375, Marantz NR1200
//! en DLNA, tweeter détruit) : trim de +8,5 dB, consigne 0 puis 0,36.
//!
//! Banc partagé avec #5695, #6008, #6541 et la sourdine : le VRAI sondeur
//! (`tick`), le VRAI orchestrateur (`set_volume`), une sortie factice de type
//! `dlna` qui enregistre chaque niveau reçu. Le renderer DLNA lit ce niveau en
//! pour-cent entier (`round(v × 100)`, `SetVolume 0..100`) : « l'appareil ne
//! reçoit jamais plus de 36 » se lit donc `v ≤ 0,36`.
use super::decisions::{JugementDuReleve, juger_le_releve_de_volume};
use super::volume_pure_5695_tests::Banc;

/// Le trim du rapport : 0,9579 / 0,36 = 2,66, soit +8,5 dB.
const TRIM_DU_RAPPORT: &str = "8.5";

fn en_pour_cent(v: f64) -> u32 {
    (v * 100.0).round() as u32
}

/// (a) La séquence exacte du journal : `device_set_volume_sending volume=0.0`
/// puis `volume=0.36 device_volume=0.9578…`, `dlna_set_volume_ok volume_pct=96`.
#[tokio::test]
async fn un_trim_positif_ne_monte_jamais_le_pourcentage_envoye_au_renderer_6739() {
    let mut banc = Banc::monter(false).await;
    banc.poser_reglage(
        &format!("zone_{}_gain_trim_db", banc.zone_id),
        TRIM_DU_RAPPORT,
    );
    banc.tic().await;
    let deja = banc.appareil_a(0.5).await;
    banc.consigne(0.0).await;
    banc.tic().await;
    banc.consigne(0.36).await;
    banc.tic().await;
    let recus = banc.commandes_depuis(deja).await;
    assert_eq!(recus.len(), 2, "deux consignes, deux ordres : {recus:?}");
    for v in &recus {
        assert!(
            en_pour_cent(*v) <= 36,
            "#6739 : trim positif de +8,5 dB multiplié sur le pour-cent d'un renderer \
             DLNA — consigne 0,36, l'appareil a reçu SetVolume {} ({recus:?})",
            en_pour_cent(*v)
        );
    }
    assert_eq!(
        en_pour_cent(recus[1]),
        36,
        "la consigne part telle quelle : {recus:?}"
    );
    assert!(
        (banc.volume_en_base() - 36.0).abs() < 1e-6,
        "la consigne de l'utilisateur reste 36 %"
    );
}

/// Un trim NÉGATIF reste appliqué sur une sortie à volume matériel : il ne
/// fait que baisser.
#[tokio::test]
async fn un_trim_negatif_reste_applique_sur_le_renderer_6739() {
    let mut banc = Banc::monter(false).await;
    banc.poser_reglage(&format!("zone_{}_gain_trim_db", banc.zone_id), "-6");
    banc.tic().await;
    let deja = banc.appareil_a(0.5).await;
    banc.consigne(0.5).await;
    let recus = banc.commandes_depuis(deja).await;
    let attendu = 0.5 * 10f64.powf(-6.0 / 20.0);
    assert!(
        recus.len() == 1 && (recus[0] - attendu).abs() < 1e-9,
        "trim −6 dB : 0,5 doit partir à {attendu}, reçu {recus:?}"
    );
}

/// (b) Le cliquet : l'appareil relu à 96 alors que la consigne est 36. Le
/// relevé ne doit pas devenir la consigne, et la consigne suivante (un pas de
/// « volume − » depuis la consigne affichée) ne monte pas.
#[tokio::test]
async fn un_releve_a_96_ne_remonte_pas_la_consigne_suivante_6739() {
    let mut banc = Banc::monter(false).await;
    banc.poser_reglage(
        &format!("zone_{}_gain_trim_db", banc.zone_id),
        TRIM_DU_RAPPORT,
    );
    banc.tic().await;
    banc.consigne(0.36).await;
    banc.tic().await;
    // Le relevé `GetVolume` rend 96.
    let deja = banc.appareil_a(0.96).await;
    for _ in 0..3 {
        banc.tic().await;
    }
    assert!(
        (banc.volume_en_base() - 36.0).abs() < 1e-6,
        "#6739 : le relevé de l'appareil (96 %) a REMONTÉ le volume utilisateur en base \
         ({} %) — cliquet vers 100 %",
        banc.volume_en_base()
    );
    let affiche = banc.volume_en_memoire().await;
    assert!(
        affiche <= 0.36 + 1e-9,
        "#6739 : le relevé de l'appareil (96 %) a remonté la consigne en mémoire ({affiche})"
    );
    // Le geste suivant : « volume − » d'un pas depuis ce que la zone affiche.
    banc.consigne(affiche - 0.05).await;
    let recus = banc.commandes_depuis(deja).await;
    assert!(
        !recus.is_empty() && recus.iter().all(|v| en_pour_cent(*v) <= 36),
        "#6739 : après un relevé à 96, un pas de « volume − » a envoyé {recus:?} — la \
         consigne suivante a MONTÉ"
    );
}

/// Une baisse faite sur l'appareil (télécommande) reste reflétée : le relevé
/// plus bas est adopté, trim retiré.
#[tokio::test]
async fn une_baisse_sur_l_appareil_reste_adoptee_6739() {
    let mut banc = Banc::monter(false).await;
    banc.zone_a(60.0).await;
    banc.tic().await; // première observation, appareil à 50 %.
    banc.appareil_a(0.2).await;
    banc.tic().await;
    assert!(
        (banc.volume_en_base() - 20.0).abs() < 1e-6,
        "une baisse sur l'appareil doit rester adoptée, base à {}",
        banc.volume_en_base()
    );
}

/// La règle elle-même, sans banc.
#[test]
fn la_regle_du_releve_ne_monte_jamais_6739() {
    // Plus haut que la consigne : jamais adopté, avec ou sans trim.
    assert!(matches!(
        juger_le_releve_de_volume(Some(0.36), 0.96, 0.36, 8.5),
        JugementDuReleve::PlusHautNonAdopte(_)
    ));
    assert!(matches!(
        juger_le_releve_de_volume(Some(0.5), 0.62, 0.3, 0.0),
        JugementDuReleve::PlusHautNonAdopte(_)
    ));
    // Trim −6 dB : le relevé du niveau composé n'est PAS une baisse.
    let compose = 0.5 * 10f64.powf(-6.0 / 20.0);
    assert_eq!(
        juger_le_releve_de_volume(Some(0.1), compose, 0.5, -6.0),
        JugementDuReleve::Ignorer
    );
    // Trim −6 dB, appareil baissé à 0,1 : consigne 0,1 / 0,501 ≈ 0,2.
    match juger_le_releve_de_volume(Some(compose), 0.1, 0.5, -6.0) {
        JugementDuReleve::Adopter(v) => assert!((v - 0.1 / 10f64.powf(-6.0 / 20.0)).abs() < 1e-9),
        autre => panic!("baisse sur l'appareil non adoptée : {autre:?}"),
    }
    // Premier relevé, NaN : rien.
    assert_eq!(
        juger_le_releve_de_volume(None, 0.1, 0.5, 0.0),
        JugementDuReleve::Ignorer
    );
    assert_eq!(
        juger_le_releve_de_volume(Some(0.5), f64::NAN, 0.5, 0.0),
        JugementDuReleve::Ignorer
    );
}

/// La composition du trim, selon la nature du volume de la sortie.
#[test]
fn la_composition_du_trim_selon_la_sortie_6739() {
    use crate::orchestrator::{sortie_a_volume_logiciel, volume_appareil_avec_trim};
    for t in [
        "dlna",
        "openhome",
        "airplay",
        "airplay2",
        "chromecast",
        "bluos",
        "squeezebox",
        "slimproto",
        "hqplayer",
        "oaat",
        "mock",
        "diretta",
    ] {
        assert!(
            !sortie_a_volume_logiciel(t),
            "{t} doit être tenue pour matérielle"
        );
        assert_eq!(volume_appareil_avec_trim(0.36, 8.5, true), 0.36);
    }
    assert!(sortie_a_volume_logiciel("local"));
    // Sortie locale : le trim est un gain numérique, jamais au-dessus de 1.
    let v = volume_appareil_avec_trim(0.3, 6.0, false);
    assert!((v - 0.3 * 10f64.powf(6.0 / 20.0)).abs() < 1e-9);
    assert_eq!(volume_appareil_avec_trim(0.9, 12.0, false), 1.0);
    assert_eq!(volume_appareil_avec_trim(1.0, 40.0, false), 1.0);
    // Valeurs illisibles : jamais NaN sur le fil.
    assert_eq!(volume_appareil_avec_trim(f64::NAN, 0.0, true), 0.0);
    assert_eq!(volume_appareil_avec_trim(0.4, f64::NAN, true), 0.4);
    assert_eq!(volume_appareil_avec_trim(0.4, f64::NAN, false), 0.4);
}
