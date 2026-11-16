//! Recette du .18, rc12 (13/11/2026) — fiche Genesis › Titres phares ›
//! « Dance on a Volcano » : toast « Erreur de lecture — HTTP 500 —
//! playback_error — aucun disque dans le lecteur ». Le titre désignait le CD
//! (corrigé dans `titres_phares_ecoutes`) ; le refus, lui, sortait brut.
//!
//! Contre-épreuve : retirer la branche `MOTIF_CD_DISQUE_ABSENT` de
//! `play_error_response` fait tomber
//! `une_piste_du_cd_sans_disque_rend_409_nomme_et_non_500`.

use super::{CODE_CD_DISQUE_ABSENT, MOTIF_CD_DISQUE_ABSENT, play_error_response};
use axum::http::StatusCode;
use serde_json::Value;

async fn corps(reponse: axum::response::Response) -> Value {
    let octets = axum::body::to_bytes(reponse.into_body(), 64 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&octets).unwrap()
}

#[tokio::test]
async fn une_piste_du_cd_sans_disque_rend_409_nomme_et_non_500() {
    let reponse = play_error_response("aucun disque dans le lecteur".into(), "fr");
    assert_eq!(
        reponse.status(),
        StatusCode::CONFLICT,
        "un disque sorti du lecteur n'est pas une panne du serveur (500)"
    );
    let corps = corps(reponse).await;
    assert_eq!(corps["error"], CODE_CD_DISQUE_ABSENT, "{corps}");
    assert_eq!(corps["code"], "cd_disc_absent", "{corps}");
    let message = corps["message"].as_str().unwrap();
    assert!(
        message.starts_with("Le disque n'est plus dans le lecteur"),
        "phrase humaine, pas le motif brut : {message}"
    );
}

#[tokio::test]
async fn le_message_du_disque_absent_suit_la_langue() {
    let corps = corps(play_error_response(MOTIF_CD_DISQUE_ABSENT.into(), "en")).await;
    assert!(
        corps["message"]
            .as_str()
            .unwrap()
            .contains("no longer in the drive"),
        "{corps}"
    );
}

/// Le motif reconnu ici est celui que le greffon CD écrit : si sa phrase
/// change, cette garde tombe au lieu que le refus redevienne un 500 muet.
#[cfg(feature = "cd")]
#[test]
fn motif_cd_disque_absent_suit_le_greffon() {
    assert_eq!(
        tune_cd::lecteur::ErreurCd::AucunDisque.to_string(),
        MOTIF_CD_DISQUE_ABSENT
    );
}
