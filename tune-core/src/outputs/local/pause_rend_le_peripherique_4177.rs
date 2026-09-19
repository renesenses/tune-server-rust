//! #4177 — en exclusif Windows (WASAPI exclusif, ASIO), la pause REND le
//! périphérique au lieu de le garder en poussant du silence.
//!
//! Le fil de rendu parti, `get_status` doit continuer de dire `Paused` : sans
//! cela le sondeur lirait `Stopped` et passerait à la piste suivante au milieu
//! d'une pause. Et `device_released_on_pause()` dit à l'orchestrateur de
//! rétablir par `play_url` à la position conservée.

use super::*;

/// La règle pure : Windows, exclusif, un flux qui joue — et rien d'autre.
#[test]
fn la_regle_ne_rend_le_peripherique_qu_en_exclusif_windows_avec_un_flux_4177() {
    assert!(la_pause_rend_le_peripherique(true, true, true));
    assert!(
        !la_pause_rend_le_peripherique(false, true, true),
        "macOS/Linux : la pause reste un booléen"
    );
    assert!(
        !la_pause_rend_le_peripherique(true, false, true),
        "mode partagé : le mixeur Windows n'est pris par personne"
    );
    assert!(
        !la_pause_rend_le_peripherique(true, true, false),
        "sans flux, rien à rendre"
    );
}

/// Le témoin : fil parti + drapeau ⇒ `Paused`, position gardée, et la sortie
/// dit qu'elle a rendu le périphérique. Sans le drapeau (contre-épreuve
/// ci-dessous), le même état est `Stopped`.
#[tokio::test]
async fn une_pause_qui_a_rendu_le_peripherique_reste_une_pause_4177() {
    let sortie = LocalOutput::with_options("Haut-parleurs".into(), true, "wasapi");
    sortie.paused.store(true, Ordering::SeqCst);
    sortie
        .peripherique_rendu_en_pause
        .store(true, Ordering::SeqCst);
    sortie.position_ms.store(42_000, Ordering::SeqCst);
    let statut = sortie.get_status().await.unwrap();
    assert_eq!(
        statut.state,
        TransportState::Paused,
        "un fil parti pour rendre le périphérique est une PAUSE, pas un arrêt"
    );
    assert_eq!(statut.position_ms, 42_000, "la position survit à la pause");
    assert!(sortie.device_released_on_pause());
}

/// Le contraste : sans périphérique rendu, un fil parti reste un arrêt — le
/// chemin du sondeur pour les fins de piste n'a pas bougé.
#[tokio::test]
async fn sans_peripherique_rendu_un_fil_parti_reste_un_arret_4177() {
    let sortie = LocalOutput::with_options("Haut-parleurs".into(), true, "wasapi");
    sortie.paused.store(true, Ordering::SeqCst);
    assert_eq!(
        sortie.get_status().await.unwrap().state,
        TransportState::Stopped
    );
    assert!(!sortie.device_released_on_pause());
}

/// La garde du BRANCHEMENT : `pause()` applique la règle, ferme le flux par
/// `stop()`, garde la position et lève le drapeau ; `play_url()` l'efface.
#[test]
fn pause_rend_le_peripherique_sous_la_regle_et_garde_la_position_4177() {
    // `local.rs` porte des `#[cfg(test)]` bien avant `pause()` : on lit le
    // fichier ENTIER (ce module-ci est un fichier à part, il ne s'y trouve
    // pas), et aucun motif ne dépend des fins de ligne — le clone Windows du
    // banc est en CRLF.
    let prod = include_str!("../local.rs");
    let debut = prod
        .find("async fn pause(&self) -> Result<(), String> {")
        .expect("pause() de LocalOutput");
    let fin = debut + prod[debut..].find("async fn resume(&self)").unwrap();
    let corps = &prod[debut..fin];
    assert!(corps.contains("la_pause_rend_le_peripherique("), "la règle");
    assert!(corps.contains("self.stop().await?"), "le flux est fermé");
    assert!(
        corps.contains("self.position_ms.store(position_ms"),
        "la position est rendue après stop()"
    );
    assert!(
        corps.contains(".store(true, Ordering::SeqCst)")
            && corps.contains("peripherique_rendu_en_pause"),
        "le drapeau est levé"
    );
    let play_url = prod.find("async fn play_url(").expect("play_url");
    let apres = &prod[play_url..play_url + 12_000];
    assert!(
        apres.contains("peripherique_rendu_en_pause"),
        "play_url() doit effacer le drapeau"
    );
}
