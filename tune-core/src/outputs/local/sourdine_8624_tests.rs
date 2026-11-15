//! #8624 — la sortie locale levait sa sourdine à chaque `set_volume` non nul :
//! une limite de sécurité, une alarme, PURE ou le sondeur rendaient le son à
//! une zone toujours affichée en sourdine. Règle de #8165 : seul un geste de
//! volume lève la sourdine (`set_mute(false)` appelé par la route), au niveau
//! du geste. Aucun son : `LocalOutput` sans flux, on lit le gain de rendu.

use std::sync::atomic::Ordering;

use super::LocalOutput;
use crate::audio::volume_scale::UNITE_DU_GAIN_DE_RENDU;
use crate::outputs::traits::OutputTarget;

fn gain(sortie: &LocalOutput) -> u32 {
    sortie.gain_de_rendu().load(Ordering::SeqCst)
}

/// Un `set_volume` interne (réimposition, limite, PURE, reprise) sur une
/// sortie en sourdine : le gain reste nul et la sortie reste muette.
#[tokio::test]
async fn set_volume_garde_la_sourdine_de_la_sortie_locale_8624() {
    let sortie = LocalOutput::new("DAC 8624".to_string());
    sortie.set_volume(0.40).await.unwrap();
    sortie.set_mute(true).await.unwrap();
    assert_eq!(gain(&sortie), 0);

    sortie.set_volume(0.25).await.unwrap();
    assert_eq!(gain(&sortie), 0, "#8624 : set_volume a rendu le son");
    let statut = sortie.get_status().await.unwrap();
    assert!(statut.muted, "#8624 : set_volume a levé la sourdine");

    sortie.set_volume(1.0).await.unwrap();
    assert_eq!(
        gain(&sortie),
        0,
        "#8624 : la réimposition à 100 % (PURE) a rendu le son"
    );
}

/// Le geste (consigne puis `set_mute(false)`, comme
/// `lever_la_sourdine_au_geste_de_volume`) rend le son au NIVEAU DU GESTE,
/// pas à celui d'avant la sourdine ni à pleine échelle.
#[tokio::test]
async fn un_geste_leve_la_sourdine_au_niveau_du_geste_8624() {
    let temoin = LocalOutput::new("Témoin".to_string());
    temoin.set_volume(0.20).await.unwrap();
    let attendu = gain(&temoin);
    assert!(attendu > 0);

    let sortie = LocalOutput::new("DAC 8624".to_string());
    sortie.set_volume(0.40).await.unwrap();
    sortie.set_mute(true).await.unwrap();
    sortie.set_volume(0.20).await.unwrap();
    sortie.set_mute(false).await.unwrap();
    assert_eq!(gain(&sortie), attendu, "le son revient au niveau du geste");
    assert!(!sortie.get_status().await.unwrap().muted);
}

/// Un geste à 0 % pendant la sourdine, puis la levée : la sortie reste à 0,
/// elle ne repart pas à pleine échelle (aucun saut de niveau).
#[tokio::test]
async fn un_geste_a_zero_ne_repart_pas_a_pleine_echelle_8624() {
    let sortie = LocalOutput::new("DAC 8624".to_string());
    sortie.set_volume(0.40).await.unwrap();
    sortie.set_mute(true).await.unwrap();
    sortie.set_volume(0.0).await.unwrap();
    sortie.set_mute(false).await.unwrap();
    assert_eq!(gain(&sortie), 0);
}

/// Hors sourdine, rien ne change : la consigne s'applique tout de suite, et
/// une sourdine levée sans consigne rend le niveau d'avant.
#[tokio::test]
async fn hors_sourdine_la_consigne_s_applique_8624() {
    let sortie = LocalOutput::new("DAC 8624".to_string());
    sortie.set_volume(1.0).await.unwrap();
    assert_eq!(gain(&sortie), UNITE_DU_GAIN_DE_RENDU);
    sortie.set_volume(0.30).await.unwrap();
    let a_30 = gain(&sortie);
    assert!(a_30 > 0 && a_30 < UNITE_DU_GAIN_DE_RENDU);
    sortie.set_mute(true).await.unwrap();
    sortie.set_mute(false).await.unwrap();
    assert_eq!(
        gain(&sortie),
        a_30,
        "aller-retour de sourdine : même niveau"
    );
}

/// Une sourdine levée sans aucun niveau connu part de l'unité, comme avant.
#[tokio::test]
async fn levee_sans_niveau_connu_8624() {
    let sortie = LocalOutput::new("DAC 8624".to_string());
    sortie.set_mute(true).await.unwrap();
    sortie.set_mute(false).await.unwrap();
    assert_eq!(gain(&sortie), UNITE_DU_GAIN_DE_RENDU);
}
