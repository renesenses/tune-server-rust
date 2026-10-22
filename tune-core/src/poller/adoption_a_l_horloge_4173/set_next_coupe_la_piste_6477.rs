//! #6477 (fil 2323, rc6, Windows 11) — FiiO K17 en DLNA : chaque piste FLAC
//! est coupée ~30 s avant sa fin.
//!
//! ## Ce que montre le journal du testeur
//!
//! ```text
//! 19:04:03.736 gapless_arm_trace … reason="in_arming_window" reported_duration_ms=404000 … position_ms=374000
//! 19:04:03.866 dlna_set_next device=Zone 1 url=…/stream/155ea0a6-….flac
//! 19:04:03.999 service_fichier_termine stream_id=e418dc88-… complet=false fin="consommateur_parti"
//! 19:04:04.252 stream_request stream_id="155ea0a6-…" range="bytes=0-" agent="Lavf/58.29.100" format=flac
//! 19:04:05.749 gapless_position_reset_detected zone_id=3 prev_pos=374000 new_pos=0 arm_to_advance_ms=1870
//! ```
//!
//! Même motif sur la piste suivante. Le renderer lâche la connexion de la
//! piste en cours ~130 ms après le `SetNextAVTransportURI`, tire la suivante
//! et repart à 0 : il traite le `SetNext` comme une bascule immédiate. Tune
//! entérinait la coupure (`gapless_advance_on_position_reset`) et recommençait
//! à la piste d'après.
//!
//! ## Ce que fait le correctif
//!
//! Une retombée de position moins de 10 s après l'armement, alors qu'il
//! restait au moins 10 s de piste et que le flux de la piste n'avait été tiré
//! qu'en partie, apprend à l'appareil la règle
//! `set_next_coupe_la_piste` (persistante, oubliée quand la version
//! logicielle change ou par « Réinitialiser la compatibilité »). Ensuite, on
//! ne lui envoie plus de `SetNext` : la fin de piste joue la suivante par
//! `SetAVTransportURI` + `Play`. La première coupure n'est pas évitée, c'est
//! elle qui apprend.
//!
//! ## Ce que le banc ne prouve pas
//!
//! Que le K17 se comporte ainsi sur toutes ses versions, ni pourquoi il le
//! fait. Le faux renderer reproduit le motif du journal, pas l'appareil.
//!
//! Chaque test a SON appareil : la règle est rangée par appareil dans une
//! mémoire process partagée, elle ne doit pas fuir vers les autres bancs.

use super::*;
use crate::outputs::dlna_repli_set_uri as compat;

const K17: &str = "uuid:FiiORender-banc-6477";
const K17_TEMOIN: &str = "uuid:FiiORender-banc-6477-temoin";
const SANS_MESURE: &str = "uuid:FiiORender-banc-6477-sans-mesure";
/// Le fichier de banc fait 35 324 octets : le renderer en a tiré une partie
/// seulement, comme les 79,9 Mo sur 84,3 Mo du journal.
const OCTETS_EN_PARTIE: u64 = 30_000;

impl Banc {
    /// Le renderer repart à `position_ms` sur la piste suivante, SANS toucher
    /// à l'état du sondeur : c'est au sondeur de voir la retombée.
    async fn le_renderer_repart_a(&self, position_ms: u64) {
        let reg = self.outputs.lock().await;
        let arc = reg.get(&self.appareil).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.set_state(crate::outputs::traits::TransportState::Playing)
            .await;
        mock.set_position(position_ms);
    }

    /// La piste `ARMEE` (212 s) entre dans sa fenêtre d'armement : quelques
    /// sondages, pour laisser passer la retenue qui suit une avance.
    async fn la_piste_armee_approche_de_sa_fin(&mut self) {
        for horloge in 205..212 {
            self.renderer_a(205_000 + (horloge - 205) * 1_000, horloge)
                .await;
            self.tic().await;
        }
    }
}

/// ⭐ Le cas du fil 2323. Armement à 207 s sur 237,6 s ; le renderer repart à
/// 0 sur la suivante aussitôt. La règle s'apprend, l'écran suit le renderer,
/// et la piste d'après ne reçoit plus de `SetNext`.
#[tokio::test]
async fn un_renderer_qui_lache_la_piste_au_set_next_n_en_recoit_plus() {
    compat::reinitialiser_compatibilite(K17).unwrap();
    let mut banc = Banc::monter_sur(K17).await;
    banc.armer().await;
    let finie = banc.flux_finie.clone();
    banc.le_renderer_tire(&finie, OCTETS_EN_PARTIE).await;

    banc.le_renderer_repart_a(1_000).await;
    banc.tic().await;

    assert!(
        compat::set_next_coupe_la_piste(K17),
        "#6477 : la piste a été lâchée 30 s avant sa fin, juste après le SetNext — \
         la règle doit être apprise pour cet appareil"
    );
    assert!(
        compat::compatibilite_de(K17)
            .set_next_evite_depuis
            .is_some(),
        "la règle doit se lire dans la compatibilité de l'appareil"
    );
    let (position, titre, _) = banc.ecran().await;
    assert_eq!(
        (position, titre.as_str()),
        (1, ARMEE),
        "l'écran suit ce que joue le renderer"
    );

    banc.la_piste_armee_approche_de_sa_fin().await;
    assert_eq!(
        banc.armees().await,
        vec![ARMEE.to_string()],
        "#6477 : plus aucun SetNext vers un renderer qui lâche la piste en cours à sa réception"
    );

    compat::reinitialiser_compatibilite(K17).unwrap();
}

/// Contre-épreuve : une fin ORDINAIRE (le renderer va jusqu'au bout, puis
/// enchaîne) n'apprend rien, et la piste d'après est armée comme avant.
#[tokio::test]
async fn une_fin_ordinaire_n_apprend_rien_et_la_suivante_est_armee() {
    compat::reinitialiser_compatibilite(K17_TEMOIN).unwrap();
    let mut banc = Banc::monter_sur(K17_TEMOIN).await;
    banc.armer().await;
    let finie = banc.flux_finie.clone();
    banc.le_renderer_tire(&finie, OCTETS_EN_PARTIE).await;
    banc.renderer_a(236_000, 236).await;
    banc.tic().await;

    banc.le_renderer_repart_a(1_000).await;
    banc.tic().await;

    assert!(!compat::set_next_coupe_la_piste(K17_TEMOIN));
    let (position, titre, _) = banc.ecran().await;
    assert_eq!((position, titre.as_str()), (1, ARMEE));

    banc.la_piste_armee_approche_de_sa_fin().await;
    assert_eq!(
        banc.armees().await,
        vec![ARMEE.to_string(), SUIVANTE.to_string()],
        "sans la règle, la piste d'après part en SetNext comme avant"
    );

    compat::reinitialiser_compatibilite(K17_TEMOIN).unwrap();
}

/// Contre-épreuve : une retombée aussitôt après l'armement, mais sur un flux
/// dont rien ne dit qu'il a été lâché en route (aucun octet mesuré), n'apprend
/// rien.
#[tokio::test]
async fn une_retombee_sans_flux_lache_n_apprend_rien() {
    compat::reinitialiser_compatibilite(SANS_MESURE).unwrap();
    let mut banc = Banc::monter_sur(SANS_MESURE).await;
    banc.armer().await;
    banc.le_renderer_repart_a(1_000).await;
    banc.tic().await;
    assert!(!compat::set_next_coupe_la_piste(SANS_MESURE));
    compat::reinitialiser_compatibilite(SANS_MESURE).unwrap();
}

/// Les trois mesures doivent concorder : retombée peu après l'armement,
/// reste de piste important, flux tiré en partie seulement.
#[test]
fn la_coupure_au_set_next_se_reconnait_a_trois_mesures() {
    use decisions::set_next_a_coupe_la_piste as coupe;
    let (servis, total) = (Some(79_888_384), Some(84_253_482));
    // Le journal du K17 : armement à 374 s sur 404,7 s, retombée 1,87 s après.
    assert!(coupe(Some(1_870), 374_000, 404_700, servis, total));
    // Fin ordinaire : retombée à la fin de la fenêtre, ~30 s après l'armement.
    assert!(!coupe(Some(30_500), 403_000, 404_700, servis, total));
    // Armement tardif (#5970) suivi d'une fin ordinaire : peu de reste.
    assert!(!coupe(Some(4_000), 402_000, 404_700, servis, total));
    // Le flux a été tiré jusqu'au bout : le renderer ne l'a pas lâché.
    assert!(!coupe(Some(1_870), 374_000, 404_700, total, total));
    // Aucun octet mesuré, ou flux inconnu : rien à conclure.
    assert!(!coupe(Some(1_870), 374_000, 404_700, Some(0), total));
    assert!(!coupe(Some(1_870), 374_000, 404_700, None, total));
    assert!(!coupe(Some(1_870), 374_000, 404_700, servis, None));
    // Sans armement, ou durée inconnue : rien à conclure.
    assert!(!coupe(None, 374_000, 404_700, servis, total));
    assert!(!coupe(Some(1_870), 374_000, 0, servis, total));
}

/// La règle survit à un redémarrage de Tune, se lit en base, et « Réinitialiser
/// la compatibilité » l'efface.
#[test]
fn la_regle_survit_au_redemarrage_et_au_reset() {
    let _ = compat::base_de_test();
    let dev = "uuid:FiiORender-banc-6477-persistance";
    let _ = compat::reinitialiser_compatibilite(dev);
    assert!(compat::memoriser_set_next_coupe_la_piste(dev));
    assert!(
        !compat::memoriser_set_next_coupe_la_piste(dev),
        "idempotent"
    );
    // L'écriture part sur un fil bloquant hors exécuteur : synchrone ici.
    compat::oublier_en_memoire_seulement(dev);
    assert!(
        compat::set_next_coupe_la_piste(dev),
        "relu en base après redémarrage"
    );
    assert!(
        compat::valeur_en_base(dev).is_some_and(|v| v.contains("set_next_coupe_la_piste")),
        "rangé en base"
    );
    assert_eq!(compat::reinitialiser_compatibilite(dev), Ok(1));
    assert!(!compat::set_next_coupe_la_piste(dev));
}
