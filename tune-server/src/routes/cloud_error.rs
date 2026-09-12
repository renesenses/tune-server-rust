//! Rendre au client un refus du nuage sans en perdre le motif (#2178).
//!
//! ## Ce qui se perdait
//!
//! Un refus du nuage Tune arrivait dans les routes sous forme de chaîne
//! (`"playlist hub list: HTTP 429 Too Many Requests"`), qu'elles emballaient
//! dans `{"error": …}` avec un statut de leur cru — 500, 502, parfois **200**.
//! Le 429 et son `Retry-After` disparaissaient là : l'écran ne recevait ni le
//! motif ni le délai, et ne pouvait que dire « Une erreur est survenue ».
//!
//! Le support a reçu ce traitement seul (#2650, #2835). Ce module le rend
//! disponible à toute la famille, avec le même contrat :
//!
//! * le **statut 429 est préservé** — il est juste, et un refus annoncé en 200
//!   ou en 502 est précisément ce qui empêchait de le reconnaître ;
//! * le corps porte `error: "rate_limited"`, code machine stable ;
//! * il porte `retry_after` en secondes **quand le distant l'annonce**, jamais
//!   fabriqué ;
//! * il porte un `message` dans la langue de l'interface (`Accept-Language`) ;
//! * l'en-tête `Retry-After` est réémis, forme standard pour qui programme ;
//! * le texte amont est conservé sous `upstream_message`.
//!
//! Hors 429, **rien ne change** : le statut par défaut de l'appelant et le
//! texte exact d'avant.

use axum::Json;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use tune_core::cloud::refusal::CloudError;

/// Clé du message générique « limite atteinte », sans délai connu.
pub const CLE_LIMITE: &str = "cloud.tropDeRequetes";
/// Clé du message « limite atteinte », avec `{minutes}` à interpoler.
pub const CLE_LIMITE_DELAI: &str = "cloud.tropDeRequetesDelai";

/// Minutes à attendre, déduites des secondes annoncées par le distant.
///
/// Arrondi vers le HAUT et jamais zéro : renvoyer l'utilisateur « dans 0 min »
/// le ferait revenir trop tôt et reprendre un 429. Le délai exact en secondes
/// n'est pas perdu — il reste dans le corps (`retry_after`) et dans l'en-tête
/// `Retry-After`.
pub fn minutes_a_attendre(secondes: u64) -> u64 {
    secondes.div_ceil(60).max(1)
}

/// Le message de limite atteinte, dans la langue de l'interface.
///
/// Les deux clés sont paramétrées : le support garde sa formulation propre
/// (`support.*`), le reste du nuage emploie [`CLE_LIMITE`] / [`CLE_LIMITE_DELAI`].
pub fn message_limite(
    headers: &HeaderMap,
    retry_after: Option<u64>,
    cle: &str,
    cle_delai: &str,
) -> String {
    let lang = crate::i18n::lang_from_header(headers);
    match retry_after {
        Some(secondes) => crate::i18n::t(&lang, cle_delai)
            .replace("{minutes}", &minutes_a_attendre(secondes).to_string()),
        None => crate::i18n::t(&lang, cle),
    }
}

/// Rend la réponse HTTP d'un refus du nuage.
///
/// `defaut` est le statut que la route employait déjà pour un refus ordinaire :
/// il est conservé tel quel hors 429, pour ne rien casser.
///
/// `enveloppe` sont les champs que le corps portait en plus de `error` — par
/// exemple `{"playlists": []}`. Un écran qui rend la liste avant de regarder
/// l'erreur continue donc de fonctionner.
pub fn reponse(
    err: &CloudError,
    headers: &HeaderMap,
    defaut: StatusCode,
    enveloppe: Value,
) -> Response {
    let mut corps = match enveloppe {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    };

    let CloudError::RateLimited {
        retry_after,
        upstream,
        ..
    } = err
    else {
        corps.insert("error".into(), json!(err.to_string()));
        return (defaut, Json(Value::Object(corps))).into_response();
    };

    corps.insert("error".into(), json!("rate_limited"));
    corps.insert(
        "message".into(),
        json!(message_limite(
            headers,
            *retry_after,
            CLE_LIMITE,
            CLE_LIMITE_DELAI
        )),
    );
    if let Some(secs) = retry_after {
        corps.insert("retry_after".into(), json!(secs));
    }
    if !upstream.is_empty() {
        corps.insert("upstream_message".into(), json!(upstream));
    }

    let mut resp = (StatusCode::TOO_MANY_REQUESTS, Json(Value::Object(corps))).into_response();
    if let Some(secs) = retry_after {
        if let Ok(v) = header::HeaderValue::from_str(&secs.to_string()) {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accept_language(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::ACCEPT_LANGUAGE, v.parse().unwrap());
        h
    }

    async fn lire(resp: Response) -> (StatusCode, Option<String>, Value) {
        let status = resp.status();
        let retry = resp
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, retry, serde_json::from_slice(&octets).unwrap())
    }

    fn limite(retry_after: Option<u64>) -> CloudError {
        CloudError::RateLimited {
            message: "playlist hub list: HTTP 429 Too Many Requests".into(),
            retry_after,
            upstream: "Too Many Attempts.".into(),
        }
    }

    #[tokio::test]
    async fn une_limite_nomme_le_motif_le_delai_et_l_entete() {
        let resp = reponse(
            &limite(Some(30)),
            &accept_language("fr-FR,fr;q=0.9"),
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "playlists": [] }),
        );
        let (status, retry, corps) = lire(resp).await;

        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "corps = {corps}");
        assert_eq!(retry.as_deref(), Some("30"));
        assert_eq!(corps["error"], json!("rate_limited"));
        assert_eq!(corps["retry_after"], json!(30));
        assert_eq!(corps["upstream_message"], json!("Too Many Attempts."));
        // L'enveloppe survit : l'écran rend sa liste vide sans casser.
        assert_eq!(corps["playlists"], json!([]));

        let message = corps["message"].as_str().expect("message absent");
        assert!(message.contains("trop de requêtes"), "message = {message}");
        assert!(message.contains('1'), "1 minute attendue : {message}");
    }

    #[tokio::test]
    async fn une_limite_sans_delai_ne_l_invente_pas() {
        let resp = reponse(
            &limite(None),
            &HeaderMap::new(),
            StatusCode::BAD_GATEWAY,
            json!({}),
        );
        let (status, retry, corps) = lire(resp).await;

        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(retry, None, "pas d'en-tête sans délai connu");
        assert_eq!(corps["error"], json!("rate_limited"));
        assert!(corps.get("retry_after").is_none(), "corps = {corps}");
    }

    #[tokio::test]
    async fn le_message_suit_la_langue_de_l_interface() {
        let mut vus = std::collections::HashSet::new();
        for langue in ["fr", "en", "de", "es", "it", "zh", "ja", "ko", "ro", "sv"] {
            let resp = reponse(
                &limite(Some(120)),
                &accept_language(langue),
                StatusCode::BAD_GATEWAY,
                json!({}),
            );
            let (_, _, corps) = lire(resp).await;
            let message = corps["message"].as_str().unwrap().to_string();
            assert!(
                message.contains('2'),
                "langue {langue} : {message} devrait porter 2 minutes"
            );
            assert!(
                !message.starts_with("cloud."),
                "clé non traduite pour {langue} : {message}"
            );
            assert!(
                vus.insert(message),
                "langue {langue} : traduction dupliquée"
            );
        }
    }

    /// Témoin : hors 429, ni le statut ni le texte ne bougent.
    #[tokio::test]
    async fn un_refus_ordinaire_garde_son_statut_et_son_texte() {
        let resp = reponse(
            &CloudError::Message("playlist hub list: HTTP 503 Service Unavailable".into()),
            &accept_language("fr"),
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "playlists": [] }),
        );
        let (status, retry, corps) = lire(resp).await;

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(retry, None);
        assert_eq!(
            corps["error"],
            json!("playlist hub list: HTTP 503 Service Unavailable")
        );
        assert_eq!(corps["playlists"], json!([]));
        assert!(corps.get("retry_after").is_none());
    }

    #[test]
    fn les_minutes_arrondissent_au_superieur_sans_jamais_zero() {
        assert_eq!(minutes_a_attendre(0), 1);
        assert_eq!(minutes_a_attendre(1), 1);
        assert_eq!(minutes_a_attendre(60), 1);
        assert_eq!(minutes_a_attendre(61), 2);
        assert_eq!(minutes_a_attendre(3540), 59);
    }
}
