//! Le relais vers l'API du cercle de mozaiklabs (`/api/v1/circle/…`).
//!
//! Chaque appel relit la session SSO dans les réglages — jamais une copie
//! tenue en mémoire : une déconnexion (`POST /cloud/sso/logout` efface le
//! jeton) ou un jeton renouvelé par le battement de compte valent dès l'appel
//! suivant. Et rien de ce que le cloud répond n'est gardé : le cercle n'a pas
//! d'existence locale.

use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::header::{ACCEPT, HeaderValue, RETRY_AFTER};
use reqwest::{Method, StatusCode, Url};
use serde_json::Value;
use tracing::{debug, info, warn};
use tune_core::cloud::sso::{DEFAULT_CLIENT_ID, MozaikAuth};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;

/// Racine du cloud quand `mozaik_base_url` ne la redirige pas — la même que
/// celle du SSO (`tune_core::cloud::sso`).
pub const BASE_PAR_DEFAUT: &str = "https://mozaiklabs.fr";

/// Borne d'un appel. Un relais que l'utilisateur attend ne doit pas tenir les
/// 30 s du client partagé avant de dire « cloud indisponible ».
const DELAI: Duration = Duration::from_secs(15);

/// Ce qu'un appel au cloud a donné, avant sa mise en forme HTTP.
#[derive(Debug)]
pub enum Issue {
    /// Aucune session SSO (aucun appel n'est parti), ou une session que le
    /// cloud refuse encore (401) après le rafraîchissement unique.
    NonConnecte,
    /// Le cloud a répondu (2xx, 3xx ou 4xx hors 401) : relayé tel quel.
    Reponse {
        statut: u16,
        corps: Vec<u8>,
        retry_after: Option<HeaderValue>,
    },
    /// Le cloud n'a pas pu répondre : injoignable, délai dépassé, ou 5xx.
    Indisponible { statut_amont: Option<u16> },
}

pub struct Relais {
    backend: Arc<dyn DbBackend>,
}

impl Relais {
    pub fn new(backend: Arc<dyn DbBackend>) -> Self {
        Self { backend }
    }

    /// Appelle `/api/v1/circle/{segments…}` avec le jeton SSO du serveur.
    ///
    /// `route` est le gabarit journalisé (`"DELETE /members/{user_id}"`) :
    /// ni identifiant, ni corps, ni jeton ne passent au journal.
    pub async fn appeler(
        &self,
        route: &'static str,
        methode: Method,
        segments: &[&str],
        corps: Option<&Value>,
    ) -> Issue {
        let debut = Instant::now();
        let settings = SettingsRepo::with_backend(self.backend.clone());
        let Some(jeton) = lire(&settings, "mozaik_access_token") else {
            debug!(route, "circle_non_connecte");
            return Issue::NonConnecte;
        };
        let base = lire(&settings, "mozaik_base_url").unwrap_or_else(|| BASE_PAR_DEFAUT.into());
        let Some(url) = url_du_cercle(&base, segments) else {
            warn!(route, "circle_adresse_cloud_invalide");
            return Issue::Indisponible { statut_amont: None };
        };

        let mut envoi = envoyer(&url, &methode, &jeton, corps).await;
        // Un 401 peut n'être qu'un jeton d'accès expiré : un seul
        // rafraîchissement, comme le battement de compte, puis le verdict du
        // cloud est relayé tel quel.
        if matches!(&envoi, Ok(r) if r.status() == StatusCode::UNAUTHORIZED)
            && let Some(nouveau) = rafraichir(&settings, &base).await
        {
            envoi = envoyer(&url, &methode, &nouveau, corps).await;
        }

        let duree_ms = debut.elapsed().as_millis() as u64;
        let reponse = match envoi {
            Ok(r) => r,
            Err(e) => {
                // `without_url` : l'adresse ne porte ni jeton ni courriel, mais
                // le journal n'a besoin que de la nature de la panne.
                warn!(route, duree_ms, error = %e.without_url(), "circle_cloud_injoignable");
                return Issue::Indisponible { statut_amont: None };
            }
        };
        let statut = reponse.status();
        // Un 401 qui survit au rafraîchissement unique : la session mozaiklabs
        // est finie. Il ne doit JAMAIS sortir tel quel : côté client, un 401
        // est la fin de la session TUNE (`fetchJSON` efface le jeton Tune).
        // C'est l'état « non connecté au cloud », comme sans session SSO.
        if statut == StatusCode::UNAUTHORIZED {
            info!(route, duree_ms, "circle_session_cloud_refusee");
            return Issue::NonConnecte;
        }
        if statut.is_server_error() {
            warn!(
                route,
                statut = statut.as_u16(),
                duree_ms,
                "circle_cloud_en_panne"
            );
            return Issue::Indisponible {
                statut_amont: Some(statut.as_u16()),
            };
        }
        let retry_after = reponse.headers().get(RETRY_AFTER).cloned();
        match reponse.bytes().await {
            Ok(corps) => {
                info!(route, statut = statut.as_u16(), duree_ms, "circle_relai");
                Issue::Reponse {
                    statut: statut.as_u16(),
                    corps: corps.to_vec(),
                    retry_after,
                }
            }
            Err(e) => {
                warn!(route, error = %e.without_url(), "circle_reponse_interrompue");
                Issue::Indisponible {
                    statut_amont: Some(statut.as_u16()),
                }
            }
        }
    }
}

/// Un réglage non vide.
fn lire(settings: &SettingsRepo, cle: &str) -> Option<String> {
    settings
        .get(cle)
        .ok()
        .flatten()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `{base}/api/v1/circle/{segments…}`, chaque segment encodé à part : un
/// identifiant qui contiendrait `/` ou `?` reste UN segment et ne peut pas
/// viser une autre route du cloud.
pub fn url_du_cercle(base: &str, segments: &[&str]) -> Option<Url> {
    let mut url = Url::parse(base.trim()).ok()?;
    if url.cannot_be_a_base() || !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.set_query(None);
    url.set_fragment(None);
    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .extend(["api", "v1", "circle"])
        .extend(segments);
    Some(url)
}

async fn envoyer(
    url: &Url,
    methode: &Method,
    jeton: &str,
    corps: Option<&Value>,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut requete = tune_core::http::client::shared()
        .request(methode.clone(), url.clone())
        .bearer_auth(jeton)
        .header(ACCEPT, "application/json")
        .timeout(DELAI);
    if let Some(c) = corps {
        requete = requete.json(c);
    }
    requete.send().await
}

/// Rafraîchit la session SSO du serveur — la même, pas une seconde — et rend
/// le nouveau jeton d'accès. Même résolution du client OAuth que le battement
/// de compte (`mozaik_client_id`, `TUNE_MOZAIK_CLIENT_ID`, puis le client
/// public par défaut).
async fn rafraichir(settings: &SettingsRepo, base: &str) -> Option<String> {
    let jeton_de_rafraichissement = lire(settings, "mozaik_refresh_token")?;
    let client_id = lire(settings, "mozaik_client_id")
        .or_else(|| {
            std::env::var("TUNE_MOZAIK_CLIENT_ID")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string());
    match MozaikAuth::new(client_id, Some(base))
        .refresh_token(&jeton_de_rafraichissement)
        .await
    {
        Ok(t) => {
            settings.set("mozaik_access_token", &t.access_token).ok();
            if let Some(nouveau) = t.refresh_token.as_deref().filter(|s| !s.is_empty()) {
                settings.set("mozaik_refresh_token", nouveau).ok();
            }
            info!("circle_session_rafraichie");
            Some(t.access_token)
        }
        Err(e) => {
            // Le message de `refresh_token` ne porte que le statut.
            debug!(error = %e, "circle_rafraichissement_refuse");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::url_du_cercle;

    #[test]
    fn l_adresse_suit_la_base_avec_ou_sans_barre_finale() {
        for base in ["http://127.0.0.1:9099", "http://127.0.0.1:9099/"] {
            assert_eq!(
                url_du_cercle(base, &["members", "7"]).unwrap().as_str(),
                "http://127.0.0.1:9099/api/v1/circle/members/7"
            );
        }
        assert_eq!(
            url_du_cercle("https://mozaiklabs.fr", &[])
                .unwrap()
                .as_str(),
            "https://mozaiklabs.fr/api/v1/circle"
        );
    }

    #[test]
    fn un_identifiant_ne_peut_pas_sortir_de_son_segment() {
        let url = url_du_cercle(
            "https://mozaiklabs.fr",
            &["invitations", "a/../../user?x=1"],
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://mozaiklabs.fr/api/v1/circle/invitations/a%2F..%2F..%2Fuser%3Fx=1"
        );
    }

    #[test]
    fn une_base_sans_schema_web_est_refusee() {
        assert!(url_du_cercle("file:///etc", &[]).is_none());
        assert!(url_du_cercle("pas une adresse", &[]).is_none());
    }
}
