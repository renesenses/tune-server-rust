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
use crate::outputs::traits::TransportState;

/// Ce que la garde de hausse laisse passer vers `cible` depuis la consigne
/// d'avant le relevé `avant` (échelle linéaire d'un DLNA) : le retour au
/// plafond part SOUS la garde (suggestion 3 de la revue de #7702).
pub(super) fn retenu_depuis(avant: f64, cible: f64) -> f64 {
    crate::audio::garde_de_hausse::plafond(
        avant,
        crate::audio::volume_scale::EchelleDeVolume::Lineaire,
    )
    .min(cible)
}

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
/// défaut (85 %). Tune ramène l'appareil vers le plafond, une fois, SOUS la
/// garde de hausse dont le repère reste la consigne d'avant (30 %) :
/// l'appareil finit au plus à 30 % + 10 dB de garde, jamais au-dessus du
/// plafond. La consigne et le curseur s'alignent sur ce qui est parti.
#[tokio::test]
async fn sans_limite_au_dessus_du_plafond_par_defaut_l_appareil_est_ramene_7379() {
    let mut banc = banc_a_30(None).await;
    let mut recu = banc.evenements_de_lecture();
    let deja = banc.appareil_a(0.95).await;
    for _ in 0..3 {
        banc.tic().await;
    }
    let attendu = retenu_depuis(0.3, 0.85);
    assert!(attendu < 0.85, "la garde doit mordre ici : {attendu}");
    let recus = banc.commandes_depuis(deja).await;
    assert_eq!(
        recus,
        vec![attendu],
        "#7379 : l'appareil à 95 % doit être ramené une fois, sous la garde"
    );
    assert!((banc.volume_en_base() - attendu * 100.0).abs() < 1e-6);
    let publies = volumes_publies(&mut recu, banc.zone_id);
    assert_eq!(
        publies.last().copied(),
        Some(attendu),
        "le curseur doit afficher ce qui est parti : {publies:?}"
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

// ── Revue de #7702 (07/11) ────────────────────────────────────────────────

/// B1 — limite 40 %, consigne 10 %, appareil réellement à 10 %. La réponse
/// de volume devient illisible : le statut rend le repli 0,5 de #5662. Ce
/// n'est pas un geste : rien n'est ramené à la limite, rien n'est suivi, et
/// aucune commande ne part au-dessus de 10 %, zone en lecture.
#[tokio::test]
async fn un_releve_illisible_n_est_jamais_un_geste_zone_en_lecture_7379() {
    let mut banc = Banc::monter(false).await;
    banc.poser_reglage(&cle_securite(banc.zone_id), "0.4");
    banc.zone_a(10.0).await;
    banc.appareil_a(0.1).await;
    banc.tic().await;
    let mut recu = banc.evenements_de_lecture();
    let deja = banc.appareil_a(0.1).await;
    banc.volume_illisible(true).await;
    for _ in 0..4 {
        banc.tic().await;
    }
    let recus = banc.commandes_depuis(deja).await;
    assert!(
        recus.iter().all(|v| *v <= 0.1 + 1e-9),
        "B1 : un relevé illisible a commandé {recus:?} (repli 0,5 pris pour un geste)"
    );
    assert!(
        (banc.volume_en_base() - 10.0).abs() < 1e-6,
        "{}",
        banc.volume_en_base()
    );
    assert!((banc.volume_en_memoire().await - 0.1).abs() < 1e-9);
    assert!(volumes_publies(&mut recu, banc.zone_id).is_empty());
    // Redevenu lisible, toujours à 10 % : rien ne bouge non plus (le repli
    // n'est pas devenu le repère du front #358).
    banc.volume_illisible(false).await;
    banc.tic().await;
    assert!((banc.volume_en_base() - 10.0).abs() < 1e-6);
    assert!(
        banc.commandes_depuis(deja)
            .await
            .iter()
            .all(|v| *v <= 0.1 + 1e-9)
    );
}

/// B1 — même chose sans limite : consigne 20 %, repli 0,5 sous le plafond
/// par défaut. Rien n'est suivi ; le curseur reste à 20, et le « volume − »
/// suivant ne part jamais au-dessus de 20.
#[tokio::test]
async fn un_releve_illisible_sans_limite_n_est_pas_suivi_7379() {
    let mut banc = Banc::monter(false).await;
    banc.zone_a(20.0).await;
    banc.appareil_a(0.2).await;
    banc.tic().await;
    let deja = banc.appareil_a(0.2).await;
    banc.volume_illisible(true).await;
    for _ in 0..3 {
        banc.tic().await;
    }
    assert!(
        (banc.volume_en_base() - 20.0).abs() < 1e-6,
        "{}",
        banc.volume_en_base()
    );
    // Seconde revue : un pas RELATIF depuis ce que la zone affiche. Si le
    // repli avait été suivi, l'affichage serait à 50 % et ce pas enverrait
    // 48 % à un appareil réellement à 20 %.
    banc.pas_de_volume(-0.02).await;
    let recus = banc.commandes_depuis(deja).await;
    assert_eq!(recus.len(), 1, "{recus:?}");
    assert!(
        (recus[0] - 0.18).abs() < 1e-9,
        "B1 : « volume − » depuis l'affichage doit envoyer 18 %, pas {recus:?}"
    );
}

/// B1 — zone ARRÊTÉE dans Tune (site « au repos »), appareil arrêté ou en
/// lecture d'une autre source, limite 40 %, consigne 10 % : le repli 0,5 ne
/// déclenche aucun retour à la limite.
#[tokio::test]
async fn un_releve_illisible_n_est_jamais_un_geste_zone_arretee_7379() {
    for etat in [TransportState::Stopped, TransportState::Playing] {
        let mut banc = Banc::monter(false).await;
        banc.poser_reglage(&cle_securite(banc.zone_id), "0.4");
        banc.zone_a(10.0).await;
        banc.appareil_a(0.1).await;
        banc.arreter_dans_tune().await;
        banc.tic_au_repos(etat).await;
        let deja = banc.appareil_a(0.1).await;
        banc.volume_illisible(true).await;
        for _ in 0..3 {
            banc.tic_au_repos(etat).await;
        }
        let recus = banc.commandes_depuis(deja).await;
        assert!(
            recus.iter().all(|v| *v <= 0.1 + 1e-9),
            "B1 ({etat:?}) : zone arrêtée, le repli a commandé {recus:?}"
        );
        assert!((banc.volume_en_base() - 10.0).abs() < 1e-6);
    }
}

/// Contrôle du banc « au repos » : un VRAI dépassement de la limite, zone
/// arrêtée dans Tune, est bien ramené (le témoin précédent n'est pas vert
/// par un site muet).
#[tokio::test]
async fn zone_arretee_un_vrai_depassement_est_ramene_7379() {
    for etat in [TransportState::Stopped, TransportState::Playing] {
        let mut banc = Banc::monter(false).await;
        banc.poser_reglage(&cle_securite(banc.zone_id), "0.4");
        banc.zone_a(10.0).await;
        banc.appareil_a(0.1).await;
        banc.arreter_dans_tune().await;
        banc.tic_au_repos(etat).await;
        let deja = banc.appareil_a(0.9).await;
        banc.tic_au_repos(etat).await;
        let recus = banc.commandes_depuis(deja).await;
        assert_eq!(recus, vec![retenu_depuis(0.1, 0.4)], "{etat:?}");
    }
}

/// B1, site radio : la webradio joue, la réponse devient illisible, limite
/// 40 %, consigne 10 % : rien ne part au-dessus de 10 %.
#[tokio::test]
async fn un_releve_illisible_en_radio_n_est_jamais_un_geste_7379() {
    let mut banc = Banc::monter(false).await;
    banc.poser_reglage(&cle_securite(banc.zone_id), "0.4");
    banc.en_radio().await;
    banc.zone_a(10.0).await;
    banc.appareil_a(0.1).await;
    // Deux tours : le premier crée l'état de sondage (étranglé), le second
    // relit l'appareil et pose le repère.
    banc.tic_radio().await;
    banc.tic_radio().await;
    let deja = banc.appareil_a(0.1).await;
    banc.volume_illisible(true).await;
    for _ in 0..3 {
        banc.tic_radio().await;
    }
    let recus = banc.commandes_depuis(deja).await;
    assert!(recus.iter().all(|v| *v <= 0.1 + 1e-9), "{recus:?}");
    assert!((banc.volume_en_base() - 10.0).abs() < 1e-6);
}

/// Site radio : un geste au bouton sous le plafond est suivi, au-dessus il
/// est ramené sous la garde, comme au site de lecture.
#[tokio::test]
async fn en_radio_suivi_sous_le_plafond_et_retour_au_dessus_7379() {
    let mut banc = Banc::monter(false).await;
    banc.en_radio().await;
    banc.zone_a(30.0).await;
    banc.appareil_a(0.3).await;
    banc.tic_radio().await;
    banc.tic_radio().await;
    let deja = banc.appareil_a(0.55).await;
    banc.tic_radio().await;
    assert!(
        (banc.volume_en_base() - 55.0).abs() < 1e-6,
        "{}",
        banc.volume_en_base()
    );
    assert!(banc.commandes_depuis(deja).await.is_empty());
    let deja = banc.appareil_a(0.95).await;
    banc.tic_radio().await;
    let attendu = retenu_depuis(0.55, 0.85);
    assert_eq!(banc.commandes_depuis(deja).await, vec![attendu]);
    assert!(attendu <= 0.85);
}

/// B2, de bout en bout — trim −6 dB, consigne 50 % (l'appareil reçoit 25 %).
/// Geste au bouton vers 60 % : la consigne suit le niveau RÉEL (60), jamais
/// 100. Puis l'auditeur remet le trim à 0 (`PATCH /zones`, « Effet
/// immédiat ») : l'appareil ne reçoit jamais plus que 60 %, ni que 85 %.
#[tokio::test]
async fn trim_negatif_suivi_puis_trim_remis_a_zero_7379() {
    let mut banc = banc_a_30(None).await;
    banc.changer_le_trim(-6.0).await;
    banc.consigne(0.5).await;
    let compose = banc.commandes_depuis(0).await.last().copied().unwrap();
    assert!(
        (compose - 0.5 * 10f64.powf(-6.0 / 20.0)).abs() < 1e-9,
        "{compose}"
    );
    banc.appareil_a(compose).await;
    banc.tic().await;
    banc.appareil_a(0.6).await;
    banc.tic().await;
    assert!(
        (banc.volume_en_base() - 60.0).abs() < 1e-6,
        "B2 : trim −6 dB, geste à 60 % : consigne {} % (jamais 100)",
        banc.volume_en_base()
    );
    let deja = banc.appareil_a(0.6).await;
    banc.changer_le_trim(0.0).await;
    let recus = banc.commandes_depuis(deja).await;
    assert!(
        !recus.is_empty() && recus.iter().all(|v| *v <= 0.6 + 1e-9),
        "B2 : trim remis à 0, l'appareil à 60 % a reçu {recus:?}"
    );
}

/// B2 — trim −6 dB au-dessus du plafond : la consigne passe au plafond sans
/// trim (ou plus bas, sous la garde), jamais 100 %.
#[tokio::test]
async fn trim_negatif_au_dessus_du_plafond_jamais_100_7379() {
    let mut banc = banc_a_30(None).await;
    banc.changer_le_trim(-6.0).await;
    banc.consigne(0.5).await;
    let compose = banc.commandes_depuis(0).await.last().copied().unwrap();
    banc.appareil_a(compose).await;
    banc.tic().await;
    banc.appareil_a(0.95).await;
    for _ in 0..3 {
        banc.tic().await;
    }
    assert!(
        banc.volume_en_base() <= 85.0 + 1e-6,
        "B2 : consigne {} % au-dessus du plafond",
        banc.volume_en_base()
    );
    let deja = banc.appareil_a(0.5).await;
    banc.changer_le_trim(0.0).await;
    assert!(
        banc.commandes_depuis(deja)
            .await
            .iter()
            .all(|v| *v <= 0.85 + 1e-9)
    );
}

/// Non bloquant 2 — un relevé jugé AVANT une consigne de l'auditeur n'est
/// pas appliqué après : ni retour au plafond, ni suivi.
#[tokio::test]
async fn un_releve_perime_par_un_set_volume_concurrent_n_est_pas_applique_7379() {
    use super::releve_ne_monte_pas_6739::SuiteDuReleve;
    let banc = banc_a_30(None).await;
    let deja = banc.appareil_a(0.95).await;
    banc.consigne_a_l_instant(0.2).await;
    let apres_baisse = banc.commandes_depuis(deja).await;
    banc.appliquer_un_releve(0.3, SuiteDuReleve::RamenerAuPlafond(0.85))
        .await;
    banc.appliquer_un_releve(0.3, SuiteDuReleve::Adopter(0.6))
        .await;
    assert_eq!(
        banc.commandes_depuis(deja).await,
        apres_baisse,
        "rien ne doit partir après la baisse de l'auditeur"
    );
    assert!((banc.volume_en_memoire().await - 0.2).abs() < 1e-9);
    assert!((banc.volume_en_base() - 20.0).abs() < 1e-6);
}

/// Zones groupées : un geste au bouton sur l'appareil d'une zone du groupe
/// n'aligne que CETTE zone, et rien ne part vers l'autre appareil.
#[tokio::test]
async fn zones_groupees_le_suivi_reste_a_la_zone_du_geste_7379() {
    let mut banc = banc_a_30(None).await;
    let b = banc.grouper_avec("dlna-salon-b", "groupe-7379").await;
    banc.tic().await;
    let deja_a = banc.appareil_a(0.3).await;
    let deja_b = banc.appareil_de("dlna-salon-b", 0.6).await;
    banc.tic().await;
    assert!(
        (banc.volume_en_base_de(b) - 60.0).abs() < 1e-6,
        "{}",
        banc.volume_en_base_de(b)
    );
    assert!(
        (banc.volume_en_base() - 30.0).abs() < 1e-6,
        "{}",
        banc.volume_en_base()
    );
    assert!(banc.commandes_depuis(deja_a).await.is_empty());
    assert!(banc.commandes_de("dlna-salon-b", deja_b).await.is_empty());
}

#[test]
fn un_type_de_sortie_inconnu_n_est_pas_suivi_vers_le_haut_7379() {
    use super::releve_ne_monte_pas_6739::suivi_a_la_hausse_pour;
    assert!(!suivi_a_la_hausse_pour(None));
    // Troisième revue : la sortie locale d'un agent Tune est logicielle.
    assert!(!suivi_a_la_hausse_pour(Some("tune_agent")));
}
