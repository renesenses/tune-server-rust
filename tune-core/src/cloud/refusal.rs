//! Un refus du nuage Tune, sous une forme que l'écran peut dire (#2178).
//!
//! ## Le défaut de famille
//!
//! Chaque module de `cloud::` bâtissait son refus en `String` :
//! `format!("playlist hub list: HTTP {}", resp.status())`. La route en faisait
//! `{"error": "<cette chaîne>"}` avec un statut de son cru — 500 ou 502 — et le
//! **429 disparaissait en chemin**, avec l'en-tête `Retry-After` qui l'accompagne.
//! L'écran ne pouvait alors rien dire d'autre que « Une erreur est survenue ».
//!
//! Le chemin du support a été traité seul (#2650, #2835) : il préserve le 429,
//! nomme le motif (`rate_limited`), porte le délai et le traduit. Les autres
//! appelants sont restés nus. [`CloudError`] porte le même contrat pour eux.
//!
//! ## Pourquoi une conversion depuis `String`
//!
//! Les fonctions du nuage mêlent, dans un seul `Result<_, String>`, des erreurs
//! de base, d'analyse et de réseau. `impl From<String>` laisse tous ces
//! `map_err(|e| format!(…))?` inchangés : seul le site du refus HTTP change, et
//! le texte rendu par [`Display`] reste **mot pour mot** celui d'avant — les
//! journaux et les corps hors 429 ne bougent pas.
//!
//! [`Display`]: std::fmt::Display

use std::fmt;

use reqwest::header::HeaderMap;

/// Longueur retenue d'un corps amont non JSON. Une page d'erreur HTML de
/// plusieurs kilo-octets n'a rien à faire dans une réponse d'API ; les premiers
/// caractères suffisent au diagnostic.
const MAX_AMONT: usize = 300;

/// Refus rendu par le nuage Tune.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudError {
    /// Tout le reste : réseau, base, analyse, statuts autres que 429. Le texte
    /// est celui que l'appelant produisait déjà.
    Message(String),
    /// Le service distant a refusé pour cause de limite atteinte (429).
    RateLimited {
        /// Le texte technique d'origine, conservé pour les journaux.
        message: String,
        /// Secondes avant nouvelle tentative, quand le distant les annonce.
        /// **Jamais fabriqué** : `None` veut dire « il ne l'a pas dit ».
        retry_after: Option<u64>,
        /// Ce que le distant a écrit (`message` JSON, sinon le corps tronqué).
        upstream: String,
    },
}

impl CloudError {
    /// Vrai quand le refus est une limite d'usage atteinte.
    pub fn is_rate_limited(&self) -> bool {
        matches!(self, Self::RateLimited { .. })
    }

    /// Le délai annoncé par le distant, en secondes, s'il l'a annoncé.
    pub fn retry_after(&self) -> Option<u64> {
        match self {
            Self::RateLimited { retry_after, .. } => *retry_after,
            Self::Message(_) => None,
        }
    }

    /// Le texte du distant, s'il en a produit un d'exploitable.
    pub fn upstream(&self) -> Option<&str> {
        match self {
            Self::RateLimited { upstream, .. } if !upstream.is_empty() => Some(upstream),
            _ => None,
        }
    }

    /// Construit le refus à partir des éléments d'une réponse.
    ///
    /// `message` est le texte que l'appelant produisait déjà ; il est conservé
    /// tel quel pour ne rien changer aux journaux ni aux corps hors 429.
    pub fn from_parts(message: String, status: u16, headers: &HeaderMap, body: &str) -> Self {
        if status != 429 {
            return Self::Message(message);
        }
        Self::RateLimited {
            message,
            retry_after: crate::cloud::rate_limit::retry_after_secs(headers),
            upstream: texte_amont(body),
        }
    }

    /// Construit le refus depuis une réponse refusée.
    ///
    /// Le corps n'est lu que sur un 429 : les autres chemins gardent
    /// exactement le nombre d'échanges réseau qu'ils avaient.
    pub async fn from_response(message: String, resp: reqwest::Response) -> Self {
        if resp.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Self::Message(message);
        }
        let retry_after = crate::cloud::rate_limit::retry_after_secs(resp.headers());
        let body = resp.text().await.unwrap_or_default();
        Self::RateLimited {
            message,
            retry_after,
            upstream: texte_amont(&body),
        }
    }
}

/// Extrait le texte que le distant a voulu dire.
///
/// Laravel répond `{"message":"Too Many Attempts."}` ; on retient ce champ.
/// À défaut d'un JSON exploitable, on garde le début du corps brut — mieux
/// vaut trois lignes de HTML dans un journal qu'un refus muet.
fn texte_amont(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(m) = v.get("message").and_then(|m| m.as_str()) {
            let m = m.trim();
            if !m.is_empty() {
                return m.chars().take(MAX_AMONT).collect();
            }
        }
    }
    body.trim().chars().take(MAX_AMONT).collect()
}

impl fmt::Display for CloudError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(m) | Self::RateLimited { message: m, .. } => f.write_str(m),
        }
    }
}

impl std::error::Error for CloudError {}

impl From<String> for CloudError {
    fn from(value: String) -> Self {
        Self::Message(value)
    }
}

impl From<&str> for CloudError {
    fn from(value: &str) -> Self {
        Self::Message(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    fn entetes(retry_after: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            reqwest::header::RETRY_AFTER,
            HeaderValue::from_str(retry_after).unwrap(),
        );
        h
    }

    #[test]
    fn un_429_nomme_la_limite_et_porte_le_delai() {
        let err = CloudError::from_parts(
            "playlist hub list: HTTP 429 Too Many Requests".into(),
            429,
            &entetes("30"),
            r#"{"message":"Too Many Attempts."}"#,
        );
        assert!(err.is_rate_limited());
        assert_eq!(err.retry_after(), Some(30));
        assert_eq!(err.upstream(), Some("Too Many Attempts."));
    }

    #[test]
    fn un_429_sans_entete_ne_fabrique_aucun_delai() {
        let err = CloudError::from_parts("x".into(), 429, &HeaderMap::new(), "{}");
        assert!(err.is_rate_limited());
        assert_eq!(err.retry_after(), None);
    }

    #[test]
    fn les_autres_statuts_restent_des_messages() {
        for status in [400u16, 401, 403, 404, 500, 502, 503] {
            let err = CloudError::from_parts("boum".into(), status, &entetes("30"), "{}");
            assert!(!err.is_rate_limited(), "status = {status}");
            assert_eq!(err.retry_after(), None, "status = {status}");
        }
    }

    #[test]
    fn le_texte_rendu_ne_change_pas() {
        let avant = "playlist hub list: HTTP 429 Too Many Requests";
        let err = CloudError::from_parts(avant.into(), 429, &entetes("30"), "");
        assert_eq!(err.to_string(), avant);
        assert_eq!(CloudError::from(avant.to_string()).to_string(), avant);
    }

    #[test]
    fn un_corps_non_json_est_conserve_tronque() {
        let corps = "<html>".to_string() + &"a".repeat(1000);
        let err = CloudError::from_parts("x".into(), 429, &HeaderMap::new(), &corps);
        assert_eq!(err.upstream().map(str::len), Some(MAX_AMONT));
    }

    #[test]
    fn un_corps_vide_ne_donne_pas_de_texte_amont() {
        let err = CloudError::from_parts("x".into(), 429, &HeaderMap::new(), "");
        assert_eq!(err.upstream(), None);
    }
}
