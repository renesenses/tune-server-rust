//! #6541 — le banc du sondeur (celui de #5695) avec une limite de sécurité :
//! un appareil monté hors de Tune au-dessus de la limite n'est pas adopté,
//! il est ramené.
use super::volume_pure_5695_tests::Banc;
use crate::audio::volume_limites::cle_securite;

/// Une zone ordinaire à 40 %, limite de sécurité à 70 %.
async fn banc_limite() -> Banc {
    let mut banc = Banc::monter(false).await;
    banc.poser_reglage(&cle_securite(banc.zone_id), "0.7");
    banc.zone_a(40.0).await;
    banc.appareil_a(0.4).await;
    banc.tic().await; // première observation, appareil conforme.
    banc
}

/// LE besoin du fil 2335 côté renderer : la molette de l'appareil le pousse à
/// 95 %. Le sondeur adoptait 95 en base ; il recommande désormais la
/// consigne de la zone (40 %) et n'écrit rien.
#[tokio::test]
async fn au_dessus_de_la_limite_le_sondeur_ramene_au_lieu_d_adopter_6541() {
    let mut banc = banc_limite().await;
    let deja = banc.appareil_a(0.95).await;
    banc.tic().await;
    assert_eq!(
        banc.volume_en_base(),
        40.0,
        "limite de sécurité 70 % : le 95 % de l'appareil ne doit pas devenir le volume de la zone"
    );
    assert_eq!(
        banc.commandes_depuis(deja).await,
        vec![0.4],
        "le sondeur doit recommander la consigne de la zone à l'appareil"
    );
}

/// Un renderer qui se réveille à 100 % : #5662 l'ignore sans rien imposer ;
/// sous une limite posée, il est ramené.
#[tokio::test]
async fn un_renderer_a_100_est_ramene_sous_la_limite_6541() {
    let mut banc = banc_limite().await;
    let deja = banc.appareil_a(1.0).await;
    banc.tic().await;
    assert_eq!(banc.commandes_depuis(deja).await, vec![0.4]);
}

/// Sous la limite, l'adoption historique (#6008) reste : 60 % < 70 %.
#[tokio::test]
async fn sous_la_limite_le_volume_de_l_appareil_est_adopte_6541() {
    let mut banc = banc_limite().await;
    let deja = banc.appareil_a(0.6).await;
    banc.tic().await;
    assert!((banc.volume_en_base() - 60.0).abs() < 1e-6);
    assert!(banc.commandes_depuis(deja).await.is_empty());
}

/// Décision du 21/10 : une limite posée avant le passage en PURE verrouillé
/// est ignorée. L'appareil à 100 % (conforme à PURE) n'est ni ramené à la
/// limite ni recommandé : AUCUNE commande de volume.
#[tokio::test]
async fn sous_pure_verrouille_la_limite_n_envoie_rien_6541() {
    let mut banc = Banc::monter(true).await;
    banc.poser_reglage(&cle_securite(banc.zone_id), "0.7");
    let deja = banc.appareil_a(1.0).await;
    banc.tic().await;
    banc.tic().await;
    assert_eq!(
        banc.commandes_depuis(deja).await,
        Vec::<f64>::new(),
        "PURE verrouillé avec une limite de 70 % enregistrée : aucune commande de volume"
    );
    assert_eq!(banc.volume_en_base(), 100.0);
}
