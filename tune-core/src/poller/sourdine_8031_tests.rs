//! #8031 — témoins sur le VRAI sondeur (`tick`) et le VRAI orchestrateur
//! (`set_mute`), sortie factice DLNA dont on fixe ce qu'elle relit.
//!
//! Le scénario du .18 (Eversolo DMP-A8, 06/11) : zone à 50 %, sourdine,
//! l'appareil relit un volume faux pendant la coupure, levée, l'appareil
//! retrouve 50 %. Le curseur (volume de la zone en mémoire, ce que les
//! clients lisent) doit revenir à 50 % à chaque fois.
use super::volume_pure_5695_tests::Banc;

const CONSIGNE: f64 = 0.5;

/// Zone à 50 % en base et en mémoire, appareil à 50 % déjà observé.
async fn zone_a_50(banc: &mut Banc) {
    banc.zone_a(CONSIGNE * 100.0).await;
    banc.appareil_a(CONSIGNE).await;
    banc.tic().await;
}

fn assert_consigne_tenue(banc_base: f64, memoire: f64, quand: &str) {
    assert!(
        (memoire - CONSIGNE).abs() < 1e-9,
        "{quand} : la zone est à {memoire} en mémoire au lieu de {CONSIGNE} — \
         le curseur ne revient pas à sa place"
    );
    assert!(
        (banc_base - CONSIGNE * 100.0).abs() < 1e-6,
        "{quand} : la zone est à {banc_base} % en base au lieu de {} %",
        CONSIGNE * 100.0
    );
}

/// Site principal (piste en lecture) : pendant la sourdine, l'appareil relit
/// une valeur transitoire (20 %), plus basse que la consigne. Avant #8031 elle
/// était adoptée, et le retour à 50 % à la levée, une hausse, refusé (#6739).
#[tokio::test]
async fn une_valeur_transitoire_relue_pendant_la_sourdine_n_est_pas_adoptee() {
    let mut banc = Banc::monter(false).await;
    zone_a_50(&mut banc).await;
    banc.sourdine(true).await;
    banc.appareil_a(0.2).await;
    banc.ecouler_la_grace_de_sourdine().await;
    for _ in 0..3 {
        banc.tic().await;
    }
    assert_consigne_tenue(
        banc.volume_en_base(),
        banc.volume_en_memoire().await,
        "pendant la sourdine",
    );
    banc.sourdine(false).await;
    banc.appareil_a(CONSIGNE).await;
    banc.ecouler_la_grace_de_sourdine().await;
    for _ in 0..3 {
        banc.tic().await;
    }
    assert_consigne_tenue(
        banc.volume_en_base(),
        banc.volume_en_memoire().await,
        "après la levée",
    );
}

/// Site radio : il n'écarte pas le zéro. Une sourdine relue à 0 sur
/// l'appareil devenait la consigne de la zone.
#[tokio::test]
async fn radio_un_zero_relu_pendant_la_sourdine_n_est_pas_adopte() {
    let mut banc = Banc::monter(false).await;
    banc.en_radio().await;
    banc.zone_a(CONSIGNE * 100.0).await;
    banc.appareil_a(CONSIGNE).await;
    // Deux tours : le premier crée l'état de sondage (étranglé), le second
    // relit l'appareil et pose le repère.
    banc.tic_radio().await;
    banc.tic_radio().await;
    banc.sourdine(true).await;
    banc.appareil_a(0.0).await;
    banc.ecouler_la_grace_de_sourdine().await;
    banc.tic_radio().await;
    banc.tic_radio().await;
    assert_consigne_tenue(
        banc.volume_en_base(),
        banc.volume_en_memoire().await,
        "radio, pendant la sourdine",
    );
    banc.sourdine(false).await;
    banc.appareil_a(CONSIGNE).await;
    banc.ecouler_la_grace_de_sourdine().await;
    banc.tic_radio().await;
    banc.tic_radio().await;
    assert_consigne_tenue(
        banc.volume_en_base(),
        banc.volume_en_memoire().await,
        "radio, après la levée",
    );
}

/// Ordre inversé : la levée est faite, mais l'appareil relit ENCORE la valeur
/// de la coupure (un `LastChange` en retard, une remontée progressive) avant
/// de retrouver 50 %. La grâce qui suit la levée couvre ce relevé.
#[tokio::test]
async fn un_releve_en_retard_juste_apres_la_levee_n_est_pas_adopte() {
    let mut banc = Banc::monter(false).await;
    zone_a_50(&mut banc).await;
    banc.sourdine(true).await;
    banc.appareil_a(0.2).await;
    banc.tic().await;
    banc.sourdine(false).await;
    // Le relevé en retard, la zone n'est plus muette, l'appareil non plus.
    banc.appareil_a(0.1).await;
    banc.tic().await;
    banc.tic().await;
    banc.appareil_a(CONSIGNE).await;
    banc.ecouler_la_grace_de_sourdine().await;
    for _ in 0..3 {
        banc.tic().await;
    }
    assert_consigne_tenue(
        banc.volume_en_base(),
        banc.volume_en_memoire().await,
        "relevé en retard après la levée",
    );
}

/// Dix sourdines de suite, avec à chaque fois un relevé faux pendant la
/// coupure : le curseur revient à chaque fois.
#[tokio::test]
async fn dix_sourdines_de_suite_le_curseur_revient_a_chaque_fois() {
    let mut banc = Banc::monter(false).await;
    zone_a_50(&mut banc).await;
    for i in 0..10 {
        banc.sourdine(true).await;
        banc.appareil_a(if i % 2 == 0 { 0.0 } else { 0.15 }).await;
        banc.ecouler_la_grace_de_sourdine().await;
        banc.tic().await;
        banc.sourdine(false).await;
        banc.appareil_a(CONSIGNE).await;
        banc.ecouler_la_grace_de_sourdine().await;
        banc.tic().await;
        assert_consigne_tenue(
            banc.volume_en_base(),
            banc.volume_en_memoire().await,
            &format!("sourdine n° {}", i + 1),
        );
    }
}

/// À la levée, le volume de la zone est repoussé aux clients
/// (`playback.volume`), après `playback.muted`.
#[tokio::test]
async fn la_levee_repousse_le_volume_de_la_zone_aux_clients() {
    let mut banc = Banc::monter(false).await;
    zone_a_50(&mut banc).await;
    banc.sourdine(true).await;
    let mut recu = banc.evenements();
    banc.sourdine(false).await;
    let mut vus = Vec::new();
    while let Ok(e) = recu.try_recv() {
        vus.push((e.event.clone(), e.data.clone()));
    }
    let muet = vus.iter().position(|(n, _)| n == "muted");
    let volume = vus.iter().position(|(n, _)| n == "volume");
    assert!(muet.is_some(), "aucun évènement muted à la levée : {vus:?}");
    let Some(volume) = volume else {
        panic!("la levée n'a pas repoussé le volume de la zone : {vus:?}");
    };
    assert!(volume > muet.unwrap(), "volume avant muted : {vus:?}");
    assert_eq!(
        vus[volume].1["volume"].as_f64(),
        Some(CONSIGNE),
        "volume repoussé faux : {vus:?}"
    );
}

/// #6700 tenu : la sourdine ne commande aucun volume, ni à la pose ni à la
/// levée. Et aucune hausse : aucun `SetVolume` ne part pendant ces tours.
#[tokio::test]
async fn la_sourdine_ne_commande_aucun_volume() {
    let mut banc = Banc::monter(false).await;
    zone_a_50(&mut banc).await;
    let deja = banc.appareil_a(CONSIGNE).await;
    banc.sourdine(true).await;
    banc.tic().await;
    banc.sourdine(false).await;
    banc.tic().await;
    let commandes = banc.commandes_depuis(deja).await;
    assert!(
        commandes.is_empty(),
        "sourdine : des SetVolume sont partis : {commandes:?}"
    );
}

/// Hors sourdine, rien ne change : une vraie baisse sur l'appareil est
/// adoptée (#6008, #6739). #7379 (décision du 02/11) : une hausse sous le
/// plafond de suivi (85 % sans limite réglée) est suivie ; au-dessus, Tune
/// ramène l'appareil au plafond.
#[tokio::test]
async fn hors_sourdine_une_baisse_reste_adoptee_et_une_hausse_suivie_sous_le_plafond() {
    let mut banc = Banc::monter(false).await;
    zone_a_50(&mut banc).await;
    banc.appareil_a(0.3).await;
    banc.tic().await;
    assert!((banc.volume_en_memoire().await - 0.3).abs() < 1e-9);
    banc.appareil_a(0.8).await;
    banc.tic().await;
    assert!((banc.volume_en_memoire().await - 0.8).abs() < 1e-9);
    let deja = banc.appareil_a(0.95).await;
    banc.tic().await;
    assert!((banc.volume_en_memoire().await - 0.85).abs() < 1e-9);
    assert_eq!(banc.commandes_depuis(deja).await, vec![0.85]);
}

/// Une baisse faite sur l'appareil PENDANT la sourdine (télécommande) est
/// adoptée à la levée : le repère est resté le volume d'avant la coupure.
#[tokio::test]
async fn une_baisse_faite_pendant_la_sourdine_est_adoptee_apres_la_levee() {
    let mut banc = Banc::monter(false).await;
    zone_a_50(&mut banc).await;
    banc.sourdine(true).await;
    banc.appareil_a(0.0).await;
    banc.ecouler_la_grace_de_sourdine().await;
    banc.tic().await;
    banc.sourdine(false).await;
    banc.appareil_a(0.3).await;
    banc.ecouler_la_grace_de_sourdine().await;
    banc.tic().await;
    assert!(
        (banc.volume_en_memoire().await - 0.3).abs() < 1e-9,
        "la baisse faite à la télécommande pendant la sourdine est perdue"
    );
}
