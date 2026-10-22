//! Sourdine par zéro, Eversolo DMP-A8 (.18, zone 10, 21/10).
//!
//! Le geste « sourdine » du client web pose `volume 0` ; l'appareil
//! l'acquitte et se tait, puis remonte une à quatre minutes plus tard son
//! ANCIEN volume (80) sans rendre le son. Le sondeur l'adoptait : la zone
//! affichait 80 %, non muette, et ni la pause ni la reprise (un `Play` seul)
//! ne rouvraient le son — seul un coup de curseur, qui renvoie un
//! `SetVolume`, le faisait.
//!
//! Banc partagé avec #5695 et #6008 : le VRAI sondeur (`tick`), le VRAI
//! orchestrateur pour la pause et la reprise, une sortie factice DLNA qui
//! enregistre chaque commande de volume reçue.
use super::volume_pure_5695_tests::Banc;
use crate::db::zone_repo::ZoneRepo;

/// La sourdine de Tune, telle que l'appareil la vit : la zone à 0 (base et
/// mémoire) et l'appareil qui l'a appliquée, observé par un tour.
async fn sourdine_par_zero(banc: &mut Banc) {
    banc.tic().await; // première observation (appareil à 50 %).
    banc.zone_a(0.0).await;
    banc.appareil_a(0.0).await;
    banc.tic().await;
}

/// LE défaut : après la sourdine, l'appareil remonte 80 % de lui-même. La
/// consigne de silence doit tenir — en base comme en mémoire.
#[tokio::test]
async fn le_volume_remonte_par_l_appareil_ne_rouvre_pas_la_sourdine() {
    let mut banc = Banc::monter(false).await;
    sourdine_par_zero(&mut banc).await;
    banc.appareil_a(0.8).await;
    for _ in 0..3 {
        banc.tic().await;
    }
    assert_eq!(
        banc.volume_en_base(),
        0.0,
        "sourdine par zéro : le 80 % remonté par l'appareil a été adopté en base — \
         la zone se dirait audible sur un appareil muet"
    );
    assert_eq!(
        banc.volume_en_memoire().await,
        0.0,
        "sourdine par zéro : le 80 % remonté par l'appareil a été adopté en mémoire"
    );
}

/// Le scénario du .18 de bout en bout : sourdine, 80 % remonté, pause,
/// reprise. Aucun `SetVolume` ne part, et la consigne reste le silence : rien
/// n'est commandé au-dessus de ce que Tune tenait avant la pause.
#[tokio::test]
async fn pause_puis_reprise_apres_la_sourdine_ne_commande_rien_au_dessus_de_la_consigne() {
    let mut banc = Banc::monter(false).await;
    sourdine_par_zero(&mut banc).await;
    let deja = banc.appareil_a(0.8).await;
    banc.tic().await;
    let consigne_avant_pause = banc.volume_en_memoire().await;
    banc.pause_puis_reprise().await;
    banc.tic().await;
    let commandes = banc.commandes_depuis(deja).await;
    assert!(
        commandes.iter().all(|v| *v <= consigne_avant_pause + 1e-9),
        "reprise : un SetVolume au-dessus de la consigne d'avant la pause \
         ({consigne_avant_pause}) : {commandes:?}"
    );
    assert!(
        commandes.is_empty(),
        "reprise : aucune commande de volume attendue, reçu {commandes:?}"
    );
    assert_eq!(
        consigne_avant_pause, 0.0,
        "la consigne d'avant la pause doit être la sourdine, pas le 80 % remonté"
    );
    assert_eq!(banc.volume_en_base(), 0.0);
}

/// Hors sourdine, rien ne change : un vrai mouvement de l'appareil depuis une
/// consigne audible est toujours adopté (#6008), et la pause puis la reprise
/// ne commandent aucun volume (#6693).
#[tokio::test]
async fn hors_sourdine_l_adoption_et_la_reprise_restent_inchangees() {
    let mut banc = Banc::monter(false).await;
    banc.tic().await;
    banc.zone_a(80.0).await;
    let deja = banc.appareil_a(0.6).await;
    banc.tic().await;
    assert!(
        (banc.volume_en_base() - 60.0).abs() < 1e-6,
        "témoin : un vrai mouvement de l'appareil reste adopté"
    );
    banc.pause_puis_reprise().await;
    banc.tic().await;
    assert!(banc.commandes_depuis(deja).await.is_empty());
}

/// Zone à volume fixe : ni l'adoption ni la reprise ne commandent un volume.
#[tokio::test]
async fn zone_a_volume_fixe_aucun_set_volume_a_la_reprise() {
    let mut banc = Banc::monter(false).await;
    ZoneRepo::with_backend(banc.base())
        .update_fixed_volume(banc.zone_id, true)
        .unwrap();
    banc.tic().await;
    let deja = banc.appareil_a(0.8).await;
    banc.tic().await;
    banc.pause_puis_reprise().await;
    banc.tic().await;
    assert!(
        banc.commandes_depuis(deja).await.is_empty(),
        "volume fixe : aucune commande de volume ne doit partir"
    );
}

/// La règle elle-même, sans banc.
#[test]
fn la_regle_ne_garde_que_le_silence() {
    use super::decisions::{should_adopt_device_volume, silence_rouvert_par_l_appareil};
    // Le cas du .18 : consigne 0, l'appareil remonte 80 après avoir dit 0.
    assert!(silence_rouvert_par_l_appareil(0.8, 0.0));
    assert!(!should_adopt_device_volume(Some(0.0), 0.8, 0.0));
    // Une consigne audible n'est pas concernée.
    assert!(!silence_rouvert_par_l_appareil(0.8, 0.3));
    assert!(should_adopt_device_volume(Some(0.5), 0.62, 0.3));
    // Une BAISSE vers le silence reste adoptable (rien ne monte).
    assert!(!silence_rouvert_par_l_appareil(0.0, 0.0));
}
