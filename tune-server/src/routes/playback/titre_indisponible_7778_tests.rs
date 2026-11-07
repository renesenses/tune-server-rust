//! #7778, point 3 — fil 2637 (rc10, zone DLNA) : un « titre phare » d'un
//! album Qobuz finit en erreur 500, journal `play_resolution_failed
//! error=no url`.
//!
//! Qobuz ne rend aucune adresse de flux pour ce titre, quel que soit le
//! format (`qobuz.rs`, `ok_or("no url")`). Ce refus vise la piste seule :
//! `play_error_response` le laissait au fourre-tout, qui ne reconnaissait
//! aucun mot de « no url » et répondait 500 avec le motif brut.
//!
//! Contre-épreuve : retirer la branche `refus_propre_a_la_piste` de
//! `play_error_response` fait tomber
//! `un_titre_sans_adresse_de_flux_rend_422_nomme_et_non_500_7778`.

use super::play_error_response;
use axum::http::StatusCode;
use serde_json::Value;

async fn corps(reponse: axum::response::Response) -> Value {
    let octets = axum::body::to_bytes(reponse.into_body(), 64 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&octets).unwrap()
}

#[tokio::test]
async fn un_titre_sans_adresse_de_flux_rend_422_nomme_et_non_500_7778() {
    let reponse = play_error_response("no url".into(), "fr");
    assert_eq!(
        reponse.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "#7778 : un titre que le service refuse n'est pas une panne du serveur (500)"
    );
    let corps = corps(reponse).await;
    assert_eq!(corps["error"], "track_unavailable", "{corps}");
    assert_eq!(corps["code"], "track_unavailable", "{corps}");
    let message = corps["message"].as_str().unwrap();
    assert!(
        !message.contains("no url") && message.contains("pas disponible"),
        "#7778 : le message montré doit nommer l'indisponibilité, pas « no url » : {message}"
    );
}

#[tokio::test]
async fn le_message_du_titre_indisponible_suit_la_langue_7778() {
    let corps = corps(play_error_response("no url".into(), "en")).await;
    assert!(
        corps["message"].as_str().unwrap().contains("not available"),
        "{corps}"
    );
}

/// Contre-épreuve du périmètre : une panne Qobuz qui n'est pas un refus de
/// la piste garde son 502.
#[tokio::test]
async fn une_panne_qobuz_qui_ne_vise_pas_la_piste_reste_en_502_7778() {
    let reponse = play_error_response("Qobuz stream url: 500".into(), "fr");
    assert_eq!(reponse.status(), StatusCode::BAD_GATEWAY);
}
