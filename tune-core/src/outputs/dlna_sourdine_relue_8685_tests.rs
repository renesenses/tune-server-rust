//! 🔴 #8685 — sourdine inversée sur l'Eversolo DMP-A8 (recette rc12, .18).
//!
//! Le faux appareil (`FauxEversolo`, celui de #8151) BASCULE sa sourdine à
//! chaque `SetMute`, quelle que soit la valeur : la forme la plus simple d'un
//! firmware qui ne tient pas un ordre redondant. On rejoue le journal du
//! 13/11 : sourdine, réaffirmation à l'enchaînement (#8151), levée, pose,
//! levée. À la fin, ce que Tune annonce doit être ce que l'appareil fait.
//!
//! Contre-épreuves : un appareil conforme garde exactement son comportement,
//! un appareil sans `GetMute` garde celui d'avant (l'acquittement fait foi),
//! et un appareil qui acquitte sans appliquer fait échouer l'ordre au lieu
//! d'annoncer un état faux. Aucun son : SOAP sur 127.0.0.1.

use super::sourdine_relue_8685::sourdine_lue;
use crate::outputs::dlna_repli_set_uri as compat;
use crate::outputs::dlna_sourdine_8151_tests::{FauxEversolo, demarrer, piste};
use crate::outputs::traits::OutputTarget;

/// Ce que Tune annonce et ce qui sort des haut-parleurs vont ensemble.
async fn annonce_et_appareil_d_accord(sortie: &super::DlnaOutput, etat: &FauxEversolo) {
    let annonce = sortie.get_status().await.unwrap().muted;
    let muet = *etat.muet.lock().await;
    assert_eq!(
        annonce,
        muet,
        "#8685 : Tune annonce muted={annonce}, l'appareil est muet={muet} ; reçu : {:?}",
        etat.commandes.lock().await
    );
}

#[test]
fn la_sourdine_lue_dans_get_mute() {
    let r =
        |v: &str| format!("<u:GetMuteResponse><CurrentMute>{v}</CurrentMute></u:GetMuteResponse>");
    assert_eq!(sourdine_lue(&r("1")), Some(true));
    assert_eq!(sourdine_lue(&r(" 0 ")), Some(false));
    assert_eq!(sourdine_lue(&r("TRUE")), Some(true));
    assert_eq!(sourdine_lue(&r("false")), Some(false));
    assert_eq!(sourdine_lue(&r("peut-être")), None);
    assert_eq!(sourdine_lue("<u:GetMuteResponse/>"), None);
    assert_eq!(
        sourdine_lue(
            "<s:Fault><detail><UPnPError><errorCode>401</errorCode></UPnPError></detail></s:Fault>"
        ),
        None
    );
}

/// ⭐ Le journal du 13/11, sur un appareil qui bascule.
#[tokio::test]
async fn la_sourdine_ne_s_inverse_plus_sur_un_appareil_qui_bascule_8685() {
    let udn = "uuid:banc-8685-bascule";
    compat::reinitialiser_compatibilite(udn).unwrap();
    compat::memoriser_volume_remonte_a_l_uri(udn);
    let mut etat = FauxEversolo::new(false);
    etat.set_mute_bascule = true;
    let (sortie, vie) = demarrer(etat.clone(), udn).await;
    sortie.set_volume(0.63).await.unwrap();
    sortie
        .play_media(&piste("http://127.0.0.1:9/stream/8685-a.flac"))
        .await
        .expect("la piste part");

    // L'auditeur coupe le son.
    sortie.set_mute(true).await.unwrap();
    assert_eq!(etat.audible().await, 0, "sourdine posée");
    // 14:10:16 — la zone muette est réaffirmée à l'enchaînement (#8151).
    sortie.set_mute(true).await.unwrap();
    assert_eq!(
        etat.audible().await,
        0,
        "#8685 : la réaffirmation a rendu le son ; reçu : {:?}",
        etat.commandes.lock().await
    );
    annonce_et_appareil_d_accord(&sortie, &etat).await;
    // Une nouvelle piste en sourdine (avant `Play`, après l'URI).
    sortie
        .play_media(&piste("http://127.0.0.1:9/stream/8685-b.flac"))
        .await
        .expect("la piste part");
    assert_eq!(etat.audible().await, 0, "nouvelle piste : toujours muette");

    // 14:10:28 — levée, 14:10:29 — pose, 14:10:30 — levée.
    sortie.set_mute(false).await.unwrap();
    assert_eq!(
        etat.audible().await,
        63,
        "#8685 : bouton « non muet », pas de son ; reçu : {:?}",
        etat.commandes.lock().await
    );
    sortie.set_mute(true).await.unwrap();
    assert_eq!(etat.audible().await, 0, "bouton « muet », du son");
    sortie.set_mute(false).await.unwrap();
    assert_eq!(etat.audible().await, 63, "levée finale : le son revient");
    annonce_et_appareil_d_accord(&sortie, &etat).await;

    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();
}

/// Contre-épreuve : un appareil conforme ne reçoit plus d'ordre redondant,
/// et le son suit le bouton.
#[tokio::test]
async fn un_appareil_conforme_ne_recoit_plus_d_ordre_redondant_8685() {
    let udn = "uuid:banc-8685-conforme";
    compat::reinitialiser_compatibilite(udn).unwrap();
    let etat = FauxEversolo::new(false);
    let (sortie, vie) = demarrer(etat.clone(), udn).await;
    sortie.set_volume(0.40).await.unwrap();
    etat.commandes.lock().await.clear();

    sortie.set_mute(true).await.unwrap();
    sortie.set_mute(true).await.unwrap();
    sortie.set_mute(false).await.unwrap();
    sortie.set_mute(false).await.unwrap();
    let commandes = etat.commandes.lock().await.clone();
    assert_eq!(
        commandes
            .iter()
            .filter(|c| c.starts_with("SetMute"))
            .collect::<Vec<_>>(),
        ["SetMute(1)", "SetMute(0)"],
        "un seul ordre par changement d'état : {commandes:?}"
    );
    assert_eq!(etat.audible().await, 40);
    annonce_et_appareil_d_accord(&sortie, &etat).await;

    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();
}

/// Contre-épreuve : sans `GetMute` lisible, rien ne change — chaque ordre
/// part, l'acquittement fait foi.
#[tokio::test]
async fn sans_get_mute_l_acquittement_fait_foi_comme_avant_8685() {
    let udn = "uuid:banc-8685-sans-get-mute";
    compat::reinitialiser_compatibilite(udn).unwrap();
    let mut etat = FauxEversolo::new(false);
    etat.sans_get_mute = true;
    let (sortie, vie) = demarrer(etat.clone(), udn).await;
    sortie.set_volume(0.40).await.unwrap();
    etat.commandes.lock().await.clear();

    sortie.set_mute(true).await.unwrap();
    sortie.set_mute(true).await.unwrap();
    assert_eq!(etat.audible().await, 0);
    assert!(sortie.get_status().await.unwrap().muted);
    sortie.set_mute(false).await.unwrap();
    assert_eq!(etat.audible().await, 40);
    assert_eq!(
        *etat.commandes.lock().await,
        ["SetMute(1)", "SetMute(1)", "SetMute(0)"],
        "sans relecture possible, le comportement d'avant"
    );

    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();
}

/// Un appareil qui acquitte `SetMute` sans l'appliquer : l'ordre échoue, et
/// Tune n'annonce pas une sourdine que l'appareil n'a pas.
#[tokio::test]
async fn une_sourdine_acquittee_mais_non_appliquee_echoue_8685() {
    let udn = "uuid:banc-8685-ignore";
    compat::reinitialiser_compatibilite(udn).unwrap();
    let mut etat = FauxEversolo::new(false);
    etat.set_mute_ignore = true;
    let (sortie, vie) = demarrer(etat.clone(), udn).await;
    sortie.set_volume(0.40).await.unwrap();

    let refus = sortie.set_mute(true).await;
    assert!(refus.is_err(), "la sourdine non appliquée doit échouer");
    annonce_et_appareil_d_accord(&sortie, &etat).await;
    assert!(!sortie.get_status().await.unwrap().muted);

    compat::reinitialiser_compatibilite(udn).unwrap();
    vie.abort();
}
