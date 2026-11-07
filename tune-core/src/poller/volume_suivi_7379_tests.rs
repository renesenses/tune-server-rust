//! #7379 — décision de Bertrand du 02/11 : quand l'appareil joue PLUS FORT
//! que la consigne de Tune (bouton de l'ampli, autre application), Tune le
//! SUIT tant qu'il reste sous le plafond de suivi (la limite de sécurité de la
//! zone, sinon 85 % par défaut) : la consigne et le curseur s'alignent. Au-
//! dessus, Tune le ramène AU plafond. Rien au-dessus du plafond ne part
//! jamais vers l'appareil.
//!
//! Banc de #5695 : le VRAI sondeur (`tick`), le VRAI orchestrateur, une
//! sortie factice de type `dlna`. Aucun son, aucun appareil réel.
use super::volume_pure_5695_tests::Banc;
use crate::audio::volume_limites::cle_securite;

/// La zone à 30 %, l'appareil conforme, première observation faite.
async fn banc_a_30(limite: Option<&str>) -> Banc {
    let mut banc = Banc::monter(false).await;
    if let Some(l) = limite {
        banc.poser_reglage(&cle_securite(banc.zone_id), l);
    }
    banc.zone_a(30.0).await;
    banc.appareil_a(0.3).await;
    banc.tic().await;
    banc
}

/// Les valeurs `volume` publiées aux clients depuis l'abonnement.
fn volumes_publies(
    recu: &mut tokio::sync::broadcast::Receiver<crate::playback::PlaybackEvent>,
    zone_id: i64,
) -> Vec<f64> {
    let mut v = Vec::new();
    while let Ok(e) = recu.try_recv() {
        if e.event == "volume" && e.zone_id == zone_id {
            v.extend(e.data["volume"].as_f64());
        }
    }
    v
}

/// Geste au bouton de l'ampli de 30 à 55 %, sans limite réglée : Tune suit.
/// La consigne (mémoire et base) passe à 55, le client reçoit `volume 0,55`
/// (le curseur suit), et aucune commande ne part vers l'appareil.
#[tokio::test]
async fn un_geste_au_bouton_sous_le_plafond_est_suivi_7379() {
    let mut banc = banc_a_30(None).await;
    let mut recu = banc.evenements_de_lecture();
    let deja = banc.appareil_a(0.55).await;
    banc.tic().await;
    assert!(
        (banc.volume_en_base() - 55.0).abs() < 1e-6,
        "#7379 : l'ampli monté à 55 % au bouton, la consigne de la zone doit le suivre ; \
         base à {} %",
        banc.volume_en_base()
    );
    assert!((banc.volume_en_memoire().await - 0.55).abs() < 1e-9);
    assert_eq!(
        volumes_publies(&mut recu, banc.zone_id),
        vec![0.55],
        "#7379 : le client doit être notifié pour que le curseur suive le son (#7119)"
    );
    assert!(
        banc.commandes_depuis(deja).await.is_empty(),
        "suivre ne commande rien à l'appareil"
    );
}

/// Geste au bouton vers 95 % sans limite réglée : au-dessus du plafond par
/// défaut (85 %). Tune ramène l'appareil AU plafond, une fois, et la consigne
/// et le curseur s'y alignent. Jamais « illimité ».
#[tokio::test]
async fn sans_limite_au_dessus_du_plafond_par_defaut_l_appareil_est_ramene_7379() {
    let mut banc = banc_a_30(None).await;
    let mut recu = banc.evenements_de_lecture();
    let deja = banc.appareil_a(0.95).await;
    for _ in 0..3 {
        banc.tic().await;
    }
    let recus = banc.commandes_depuis(deja).await;
    assert_eq!(
        recus,
        vec![0.85],
        "#7379 : l'appareil à 95 % doit être ramené au plafond par défaut (85 %), une fois"
    );
    assert!((banc.volume_en_base() - 85.0).abs() < 1e-6);
    let publies = volumes_publies(&mut recu, banc.zone_id);
    assert_eq!(
        publies.last().copied(),
        Some(0.85),
        "le curseur doit afficher le plafond : {publies:?}"
    );
    assert!(publies.iter().all(|v| *v <= 0.85 + 1e-12), "{publies:?}");
}

/// Sous une limite de sécurité réglée à 50 % : suivi jusqu'à 45 %, puis le
/// geste vers 80 % est ramené à 50, jamais plus.
#[tokio::test]
async fn sous_une_limite_reglee_suivi_puis_retour_a_la_limite_7379() {
    let mut banc = banc_a_30(Some("0.5")).await;
    let deja = banc.appareil_a(0.45).await;
    banc.tic().await;
    assert!((banc.volume_en_base() - 45.0).abs() < 1e-6);
    assert!(banc.commandes_depuis(deja).await.is_empty());
    let deja = banc.appareil_a(0.8).await;
    for _ in 0..3 {
        banc.tic().await;
    }
    assert_eq!(banc.commandes_depuis(deja).await, vec![0.5]);
    assert!((banc.volume_en_base() - 50.0).abs() < 1e-6);
}

/// Une consigne que l'auditeur a posée lui-même à 90 % dans Tune, sans
/// limite : le plafond par défaut ne la rabote pas. Un geste au bouton vers
/// 98 % est ramené à 90 (la consigne), pas à 85.
#[tokio::test]
async fn le_plafond_par_defaut_ne_rabote_pas_une_consigne_de_tune_7379() {
    let mut banc = banc_a_30(None).await;
    banc.consigne(0.9).await;
    banc.tic().await;
    let deja = banc.appareil_a(0.98).await;
    for _ in 0..3 {
        banc.tic().await;
    }
    assert_eq!(banc.commandes_depuis(deja).await, vec![0.9]);
    assert!((banc.volume_en_base() - 90.0).abs() < 1e-6);
}

/// La garde contre les montées brusques (#6919, #6721) tient toujours pour
/// les gestes venus de Tune : après avoir suivi l'ampli à 55 %, un curseur
/// posé à 100 % sans confirmation est plafonné.
#[tokio::test]
async fn la_garde_de_hausse_tient_apres_un_suivi_7379() {
    let mut banc = banc_a_30(None).await;
    banc.appareil_a(0.55).await;
    banc.tic().await;
    assert!((banc.volume_en_memoire().await - 0.55).abs() < 1e-9);
    let deja = banc.appareil_a(0.55).await;
    banc.consigne_gardee(1.0).await;
    let recus = banc.commandes_depuis(deja).await;
    assert!(
        recus.len() == 1 && recus[0] > 0.55 && recus[0] < 0.99,
        "#6919 : un saut de 55 à 100 % depuis Tune doit être plafonné : {recus:?}"
    );
}

/// #6008 sous la décision du 02/11 : l'Abyss qui remonte d'elle-même de 22 à
/// 80 %, sans limite réglée, est SUIVIE (80 % est sous le plafond par défaut
/// de 85 %) : consigne et curseur à 80, aucune commande. Une limite de
/// sécurité réglée sur la zone reste la protection.
#[tokio::test]
async fn l_abyss_a_80_est_suivie_sous_le_plafond_par_defaut_7379() {
    let mut banc = Banc::monter(false).await;
    banc.zone_a(22.0).await;
    banc.appareil_a(0.22).await;
    banc.tic().await;
    let mut recu = banc.evenements_de_lecture();
    let deja = banc.appareil_a(0.80).await;
    banc.tic().await;
    assert!(
        (banc.volume_en_base() - 80.0).abs() < 1e-6,
        "#7379 : 80 % est sous le plafond par défaut, la zone suit ({} %)",
        banc.volume_en_base()
    );
    assert_eq!(volumes_publies(&mut recu, banc.zone_id), vec![0.8]);
    assert!(banc.commandes_depuis(deja).await.is_empty());
}

/// #7379, #8074 — une sortie LOCALE (volume logiciel, pas de bouton d'ampli)
/// relue plus haut que la consigne : son gain est celui que Tune lui a posé,
/// jamais un geste à suivre. La consigne reste à 30 %, rien n'est commandé,
/// et le curseur ne bouge pas.
#[tokio::test]
async fn une_sortie_locale_relue_plus_haut_n_est_pas_suivie_7379() {
    let mut banc = Banc::monter_sur(false, "local:test-dac", "local").await;
    banc.zone_a(30.0).await;
    banc.appareil_a(0.3).await;
    banc.tic().await;
    let mut recu = banc.evenements_de_lecture();
    let deja = banc.appareil_a(0.6).await;
    for _ in 0..3 {
        banc.tic().await;
    }
    assert!(
        (banc.volume_en_base() - 30.0).abs() < 1e-6,
        "#7379 : une sortie locale ne fait jamais monter la consigne ({} %)",
        banc.volume_en_base()
    );
    assert!((banc.volume_en_memoire().await - 0.3).abs() < 1e-9);
    assert!(volumes_publies(&mut recu, banc.zone_id).is_empty());
    assert!(banc.commandes_depuis(deja).await.is_empty());
    // Une baisse relue reste adoptée, comme avant (#6008).
    let deja = banc.appareil_a(0.2).await;
    banc.tic().await;
    assert!((banc.volume_en_base() - 20.0).abs() < 1e-6);
    assert!(banc.commandes_depuis(deja).await.is_empty());
}

/// #7379, #8074 — une sortie locale relue au-dessus de la limite de sécurité
/// est ramenée à SA consigne (bornée), jamais montée vers la limite.
#[tokio::test]
async fn une_sortie_locale_au_dessus_de_la_limite_revient_a_sa_consigne_7379() {
    let mut banc = Banc::monter_sur(false, "local:test-dac", "local").await;
    banc.poser_reglage(&cle_securite(banc.zone_id), "0.5");
    banc.zone_a(30.0).await;
    banc.appareil_a(0.3).await;
    banc.tic().await;
    let deja = banc.appareil_a(0.9).await;
    for _ in 0..3 {
        banc.tic().await;
    }
    let recus = banc.commandes_depuis(deja).await;
    assert_eq!(recus.len(), 1, "{recus:?}");
    assert!(recus[0] <= 0.3 + 1e-9, "{recus:?}");
    assert!((banc.volume_en_base() - 30.0).abs() < 1e-6);
}

#[test]
fn le_suivi_a_la_hausse_ne_vaut_que_pour_un_volume_materiel_7379() {
    use super::releve_ne_monte_pas_6739::suivi_a_la_hausse_pour;
    for t in ["dlna", "openhome", "airplay", "bluos", "chromecast"] {
        assert!(suivi_a_la_hausse_pour(Some(t)), "{t}");
    }
    assert!(!suivi_a_la_hausse_pour(Some("local")));
    assert!(!suivi_a_la_hausse_pour(Some("browser")));
}
