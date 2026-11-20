//! #8726 — après un redémarrage de l'endpoint en pleine lecture, `play`
//! n'envoyait plus que `resume`, vers une session terminée.
//!
//! L'appareil perdu (`DeviceLost`) est retiré du registre et arrêté ; à son
//! retour, mDNS enregistre une sortie OAAT NEUVE, sans tâche de lecture. La
//! zone, restée « en lecture », passait par la reprise sur place :
//! `OaatOutput::resume` ne trouvait aucun canal de commandes, n'envoyait rien
//! et rendait `Ok`. Aucune connexion vers le port de contrôle, aucun son.
//!
//! La sortie dit maintenant qu'elle n'a plus de session
//! (`device_released_on_pause`) et refuse la reprise : l'orchestrateur
//! rétablit la piste à la position conservée par `play_media` (contrat #6062).
//!
//! Aucun endpoint, aucune socket, aucune sortie sonore : seul le canal de
//! commandes, tel que `play_media` l'installe, est simulé.

use super::*;

fn sortie() -> OaatOutput {
    OaatOutput::new(
        "cm4 [Verox]".into(),
        "127.0.0.1".into(),
        9,
        "verox-8726".into(),
    )
}

/// Épreuve : la sortie réenregistrée après le retour de l'appareil n'a
/// jamais lancé de lecture. Avant le correctif, `resume` rendait `Ok`.
#[tokio::test]
async fn reprise_8726_sortie_neuve_sans_session_refusee_et_rendue() {
    let out = sortie();
    assert!(
        out.device_released_on_pause(),
        "sans session, l'orchestrateur doit rétablir la piste"
    );
    let refus = out.resume().await.expect_err("aucune session à reprendre");
    assert!(refus.contains("cm4 [Verox]"), "{refus}");
    assert!(refus.contains("reopened"), "{refus}");
}

/// Épreuve : la tâche de lecture s'est terminée d'elle-même (récepteur du
/// canal détruit, « playback complete ») sans `stop`.
#[tokio::test]
async fn reprise_8726_tache_terminee_refusee_et_rendue() {
    let out = sortie();
    let (tx, rx) = tokio::sync::mpsc::channel::<OaatCommand>(4);
    *out.command_tx.lock().await = Some(tx);
    drop(rx);
    assert!(out.device_released_on_pause());
    assert!(out.resume().await.is_err());
}

/// Contre-épreuve : une session vivante (la tâche lit le canal) se reprend
/// sur place, sans rétablissement, et reçoit bien `Resume`.
#[tokio::test]
async fn reprise_8726_session_vivante_reprise_sur_place() {
    let out = sortie();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<OaatCommand>(4);
    *out.command_tx.lock().await = Some(tx);
    out.pause().await.unwrap();
    assert!(matches!(rx.recv().await, Some(OaatCommand::Pause)));
    assert!(
        !out.device_released_on_pause(),
        "une session vivante ne doit pas être rétablie"
    );
    out.resume().await.expect("reprise sur place");
    assert!(matches!(rx.recv().await, Some(OaatCommand::Resume)));
}
