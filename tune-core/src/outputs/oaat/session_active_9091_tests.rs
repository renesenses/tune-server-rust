//! #9091 — `session_active` dit à la découverte si une annonce mDNS expirée
//! peut retirer la sortie. Seul le canal de commandes, tel que `play_media`
//! l'installe, est simulé : aucun endpoint, aucune socket.
use super::*;

fn sortie() -> OaatOutput {
    OaatOutput::new(
        "HifiBerry DAC+ / DAC+ Pro".into(),
        "127.0.0.1".into(),
        9740,
        "9c3766c5-9091".into(),
    )
}

/// Contre-épreuve : une sortie neuve, jamais lancée, n'a pas de session.
#[tokio::test]
async fn session_9091_sortie_neuve_sans_session() {
    let out = sortie();
    assert!(!out.session_active());
    assert_eq!(out.adresse_de_controle(), ("127.0.0.1", 9740));
}

/// Épreuve : la tâche de lecture lit son canal (lecture ou pause) : la
/// session est vivante, la découverte ne doit pas la couper.
#[tokio::test]
async fn session_9091_tache_vivante_session_active() {
    let out = sortie();
    let (tx, rx) = tokio::sync::mpsc::channel::<OaatCommand>(4);
    *out.command_tx.lock().await = Some(tx);
    assert!(out.session_active());
    drop(rx);
    assert!(
        !out.session_active(),
        "tâche terminée (récepteur détruit) : plus de session"
    );
}

/// Épreuve : drapeau de lecture levé (fenêtre où `play_media` démarre).
#[tokio::test]
async fn session_9091_lecture_en_cours_session_active() {
    let out = sortie();
    out.playing.store(true, Ordering::SeqCst);
    assert!(out.session_active());
}
