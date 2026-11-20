//! #8913 — recette du .18, rc12 (17/11/2026) : une piste YouTube, yt-dlp non
//! activé, et à l'écran « HTTP 502 — upstream_error — Streaming service
//! error: YouTube playback failed for track 8g25GeJgxgs: native API and
//! yt-dlp both failed… ». Le refus porte désormais le code stable
//! `youtube_playback_disabled` : 409, phrase traduite (`message_key`, #7077)
//! et l'action qui active la lecture YouTube.
//!
//! Contre-épreuve : retirer la branche `CODE_LECTURE_YOUTUBE_INACTIVE` de
//! `play_error_response` rend le 502 `upstream_error` et fait tomber
//! `youtube_sans_yt_dlp_rend_409_nomme_et_non_502`.

use super::play_error_response;
use axum::http::StatusCode;
use serde_json::Value;
use tune_core::streaming::youtube::CODE_LECTURE_YOUTUBE_INACTIVE;

async fn corps(reponse: axum::response::Response) -> Value {
    let octets = axum::body::to_bytes(reponse.into_body(), 64 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&octets).unwrap()
}

/// Le message tel qu'il remonte du cœur : `TuneError::Streaming` affiché.
fn erreur_du_coeur() -> String {
    format!(
        "Streaming service error: {CODE_LECTURE_YOUTUBE_INACTIVE}: YouTube playback is not \
         enabled — turn it on in Settings (downloads the yt-dlp helper)"
    )
}

#[tokio::test]
async fn youtube_sans_yt_dlp_rend_409_nomme_et_non_502() {
    let reponse = play_error_response(erreur_du_coeur(), "fr");
    assert_eq!(
        reponse.status(),
        StatusCode::CONFLICT,
        "un réglage à activer, pas une panne de Google (502)"
    );
    let corps = corps(reponse).await;
    assert_eq!(corps["error"], "youtube_playback_disabled", "{corps}");
    assert_eq!(corps["code"], "youtube_playback_disabled", "{corps}");
    assert_eq!(
        corps["message_key"], "playback.youtube_playback_disabled",
        "{corps}"
    );
    assert_eq!(corps["action"], "enable_youtube_playback", "{corps}");
    assert_eq!(corps["enable_endpoint"], "/api/v1/system/youtube/enable");
    let message = corps["message"].as_str().unwrap();
    assert!(
        message.starts_with("La lecture YouTube n'est pas activée"),
        "phrase humaine, pas le motif brut : {message}"
    );
    assert!(
        message.contains("› Lecture YouTube"),
        "le chemin : {message}"
    );
    assert!(
        corps["cause"].as_str().unwrap().contains("yt-dlp"),
        "la cause technique reste dans `cause` : {corps}"
    );
}

#[tokio::test]
async fn la_phrase_suit_la_langue_dans_les_seize() {
    let mut vues = std::collections::HashSet::new();
    for lang in crate::i18n::SUPPORTED {
        let corps = corps(play_error_response(erreur_du_coeur(), lang)).await;
        let m = corps["message"].as_str().unwrap().to_string();
        assert!(m.contains("yt-dlp"), "{lang} : {m}");
        assert!(m.contains("YouTube"), "{lang} : {m}");
        assert!(vues.insert(m.clone()), "{lang} : phrase en double {m}");
    }
    assert_eq!(vues.len(), 16);
}

/// Témoin : une autre panne YouTube (yt-dlp présent mais en échec) garde
/// son 502 `upstream_error` — la branche ne vise que le réglage absent.
#[tokio::test]
async fn une_vraie_panne_youtube_reste_un_502() {
    let reponse = play_error_response(
        "Streaming service error: YouTube playback failed for track x: native API and \
         yt-dlp both failed. Last error: yt-dlp failed for x: HTTP Error 403"
            .into(),
        "fr",
    );
    assert_eq!(reponse.status(), StatusCode::BAD_GATEWAY);
    assert_eq!(corps(reponse).await["error"], "upstream_error");
}
