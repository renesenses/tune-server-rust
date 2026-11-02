//! Contrôle d'origine des requêtes HTTP : CORS strict et refus des requêtes
//! inter-sites qui changent l'état.
//!
//! # Qui appelle le serveur, et avec quels en-têtes
//!
//! - **Le client web servi par le serveur** (et les skins, et `tune-desktop`
//!   qui ouvre `http://localhost:8888`) : même origine. Un navigateur récent
//!   envoie `Sec-Fetch-Site: same-origin` ; tous envoient `Origin` égal à
//!   l'hôte joint sur une requête non `GET`.
//! - **Les applis natives** (iOS, Android/Flutter, Tune Remote), le moteur du
//!   widget (`reqwest`), un autre serveur Tune (maître/agent), les appareils
//!   UPnP/DLNA, Home Assistant, `curl` : ni `Origin` ni `Sec-Fetch-*`.
//! - **Le relais cloud et l'écoute directe** : la requête est rejouée par le
//!   serveur lui-même sur `127.0.0.1`, avec la seule liste d'en-têtes du relais
//!   (`tune-bridge/src/api_proxy.rs`, `ENTETES_RELAYES`), qui ne contient ni
//!   `Origin` ni `Sec-Fetch-*` : elle arrive comme celle d'une appli native.
//! - **La fenêtre du widget** (Tauri) : `Origin: tauri://localhost` (macOS) ou
//!   `http(s)://tauri.localhost` (Windows), qui ne désignent aucun site web.
//! - **Le client web d'un AUTRE serveur Tune** qui parcourt ce catalogue
//!   (`tuneRemote.ts`) : de simples `GET` de `/api/v1/library/…`.
//!
//! # Ce qui est refusé
//!
//! Une requête qui change l'état (toute méthode hors `GET`, `HEAD`, `OPTIONS`)
//! est refusée en `403` quand elle vient d'une page web d'un autre site :
//! `Sec-Fetch-Site: cross-site` ou `same-site`, ou un `Origin` qui n'est ni
//! l'hôte joint, ni une origine native de la liste. L'absence des deux
//! en-têtes vaut client natif et passe : un navigateur pose toujours `Origin`
//! sur une requête non `GET`, et une page ne peut forger ni l'un ni l'autre.
//! La règle vaut que l'authentification soit active ou non : avec elle, elle
//! ferme aussi la voie du cookie de session.
//!
//! # CORS
//!
//! `CorsLayer::permissive()` autorisait toute origine à LIRE toute réponse. La
//! couche n'autorise plus que : les origines natives, la même origine, et, pour
//! les autres origines, la seule lecture (`GET`/`HEAD`) du catalogue
//! `/api/v1/library/` dont a besoin le parcours d'un autre serveur Tune, et
//! la seule lecture de la sonde publique `/api/v1/system/health` par la page
//! des radios du site ([`ORIGINE_DU_SITE`]).
//!
//! `TUNE_ALLOWED_ORIGINS` (liste séparée par des virgules, réglée par
//! l'exploitant, jamais par HTTP) ajoute des origines de confiance, par
//! exemple un mandataire inverse qui réécrit l'en-tête `Host`.

use axum::extract::Request;
use axum::http::header::HeaderName;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, request::Parts};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, Any, CorsLayer};

/// Origines des fenêtres natives (Tauri : widget, desktop). Aucun site web ne
/// peut s'en réclamer.
pub const ORIGINES_NATIVES: &[&str] = &[
    "tauri://localhost",
    "http://tauri.localhost",
    "https://tauri.localhost",
];

/// Variable d'environnement des origines supplémentaires de confiance.
pub const VARIABLE_ORIGINES: &str = "TUNE_ALLOWED_ORIGINS";

/// Préfixe du catalogue lisible depuis une autre origine (`GET`/`HEAD`).
pub const CATALOGUE_PUBLIC: &str = "/api/v1/library/";

/// La page publique des radios du site (#370, #7044) lit, depuis le
/// navigateur, la sonde publique du serveur local pour afficher « Connecté à
/// <nom> ». Seule cette origine, seule cette route, en lecture seule : la
/// route ne demande aucun jeton, la couche n'ouvre que la LECTURE de sa
/// réponse à ce site-là.
pub const ORIGINE_DU_SITE: &str = "https://mozaiklabs.fr";

/// La sonde publique que lit [`ORIGINE_DU_SITE`] (chemin exact).
pub const SONDE_PUBLIQUE: &str = "/api/v1/system/health";

const SEC_FETCH_SITE: HeaderName = HeaderName::from_static("sec-fetch-site");
const X_FORWARDED_HOST: HeaderName = HeaderName::from_static("x-forwarded-host");

/// Le verdict sur une requête.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Acceptee,
    /// Refusée : vient d'une page d'un autre site.
    InterSites,
}

fn origines_de_l_exploitant() -> Vec<String> {
    std::env::var(VARIABLE_ORIGINES)
        .ok()
        .map(|v| {
            v.split(',')
                .map(|o| o.trim().trim_end_matches('/').to_ascii_lowercase())
                .filter(|o| !o.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// `scheme://hote[:port]` → `(scheme, hote:port sans port par défaut)`.
fn decouper_origine(origine: &str) -> Option<(String, String)> {
    let (schema, reste) = origine.split_once("://")?;
    let schema = schema.to_ascii_lowercase();
    let autorite = reste.split(['/', '?', '#']).next()?.to_ascii_lowercase();
    if autorite.is_empty() || autorite.contains('@') {
        return None;
    }
    Some((schema.clone(), sans_port_par_defaut(&schema, &autorite)))
}

fn sans_port_par_defaut(schema: &str, autorite: &str) -> String {
    let defaut = match schema {
        "http" | "ws" => ":80",
        "https" | "wss" => ":443",
        _ => return autorite.to_string(),
    };
    autorite
        .strip_suffix(defaut)
        .unwrap_or(autorite)
        .to_string()
}

/// L'origine désigne-t-elle une fenêtre native ou une origine déclarée par
/// l'exploitant ?
pub fn est_origine_de_confiance(origine: &str) -> bool {
    let o = origine.trim().trim_end_matches('/').to_ascii_lowercase();
    ORIGINES_NATIVES.contains(&o.as_str()) || origines_de_l_exploitant().contains(&o)
}

/// L'origine est-elle celle de l'hôte joint (`Host`, ou `X-Forwarded-Host`
/// posé par un mandataire inverse) ?
///
/// `X-Forwarded-Host` n'est pas un en-tête qu'une page peut poser sur une
/// requête inter-sites sans préflight, et le préflight lui est refusé.
pub fn est_meme_origine(origine: &str, entetes: &HeaderMap) -> bool {
    let Some((schema, autorite)) = decouper_origine(origine) else {
        return false;
    };
    if !matches!(schema.as_str(), "http" | "https") {
        return false;
    }
    let hotes = [
        entetes.get(axum::http::header::HOST),
        entetes.get(X_FORWARDED_HOST),
    ];
    hotes.into_iter().flatten().any(|h| {
        h.to_str().ok().is_some_and(|h| {
            h.split(',').any(|h| {
                let h = h.trim().to_ascii_lowercase();
                !h.is_empty() && sans_port_par_defaut(&schema, &h) == autorite
            })
        })
    })
}

fn methode_sure(m: &Method) -> bool {
    matches!(*m, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Juge une requête d'après sa méthode et ses en-têtes.
pub fn juger(methode: &Method, entetes: &HeaderMap) -> Verdict {
    if methode_sure(methode) {
        return Verdict::Acceptee;
    }
    let site = entetes
        .get(SEC_FETCH_SITE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase());
    let origine = entetes
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::trim);

    // Une origine native ou déclarée passe, quel que soit `Sec-Fetch-Site`
    // (la fenêtre du widget est « cross-site » pour son moteur web).
    if origine.is_some_and(est_origine_de_confiance) {
        return Verdict::Acceptee;
    }
    match site.as_deref() {
        // Même origine pour le navigateur, ou geste de l'utilisateur
        // (barre d'adresse, favori) : rien d'un autre site.
        Some("same-origin") | Some("none") => return Verdict::Acceptee,
        Some(_) => {
            // `cross-site` ou `same-site` : un autre site, sauf origine égale
            // à l'hôte joint (mandataire qui réécrit, navigateur particulier).
            return match origine {
                Some(o) if est_meme_origine(o, entetes) => Verdict::Acceptee,
                _ => Verdict::InterSites,
            };
        }
        None => {}
    }
    match origine {
        // Ni `Origin` ni `Sec-Fetch-Site` : client natif, relais, appareil.
        None => Verdict::Acceptee,
        Some(o) if est_meme_origine(o, entetes) => Verdict::Acceptee,
        // `Origin: null` (iframe isolée, fichier local) ou autre site.
        Some(_) => Verdict::InterSites,
    }
}

/// Couche de refus des requêtes inter-sites qui changent l'état.
pub async fn garde(requete: Request, suivant: Next) -> Response {
    match juger(requete.method(), requete.headers()) {
        Verdict::Acceptee => suivant.run(requete).await,
        Verdict::InterSites => {
            tracing::warn!(
                methode = %requete.method(),
                chemin = %requete.uri().path(),
                origine = ?requete.headers().get(axum::http::header::ORIGIN),
                "requete_inter_sites_refusee"
            );
            (
                StatusCode::FORBIDDEN,
                axum::Json(json!({
                    "error": "cross_site_request",
                    "message": "Cross-site request refused.",
                })),
            )
                .into_response()
        }
    }
}

/// La méthode réellement demandée : celle du préflight pour `OPTIONS`.
fn methode_demandee(parts: &Parts) -> Option<Method> {
    if parts.method == Method::OPTIONS {
        parts
            .headers
            .get(axum::http::header::ACCESS_CONTROL_REQUEST_METHOD)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| Method::from_bytes(v.trim().as_bytes()).ok())
    } else {
        Some(parts.method.clone())
    }
}

/// L'origine peut-elle lire la réponse (CORS) ?
pub fn cors_autorise(origine: &HeaderValue, parts: &Parts) -> bool {
    let Ok(origine) = origine.to_str() else {
        return false;
    };
    if est_origine_de_confiance(origine) || est_meme_origine(origine, &parts.headers) {
        return true;
    }
    let lecture = methode_demandee(parts).is_some_and(|m| m == Method::GET || m == Method::HEAD);
    if !lecture {
        return false;
    }
    // Le catalogue, en lecture seule, pour le parcours d'un autre serveur.
    if parts.uri.path().starts_with(CATALOGUE_PUBLIC) {
        return true;
    }
    // La sonde publique, pour la page des radios du site, et pour lui seul.
    parts.uri.path() == SONDE_PUBLIQUE
        && origine
            .trim()
            .trim_end_matches('/')
            .eq_ignore_ascii_case(ORIGINE_DU_SITE)
}

/// La couche CORS du serveur, à la place de `CorsLayer::permissive()`.
pub fn couche_cors() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(cors_autorise))
        .allow_methods(AllowMethods::mirror_request())
        .allow_headers(AllowHeaders::mirror_request())
        .expose_headers(Any)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entetes(paires: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in paires {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn le_client_web_servi_passe() {
        let h = entetes(&[
            ("host", "192.168.1.18:8888"),
            ("origin", "http://192.168.1.18:8888"),
            ("sec-fetch-site", "same-origin"),
        ]);
        assert_eq!(juger(&Method::PATCH, &h), Verdict::Acceptee);
        // Navigateur sans Sec-Fetch-Site : l'Origin égal à l'hôte suffit.
        let h = entetes(&[("host", "tune.local"), ("origin", "http://tune.local:80")]);
        assert_eq!(juger(&Method::POST, &h), Verdict::Acceptee);
    }

    #[test]
    fn une_page_d_un_autre_site_est_refusee() {
        let h = entetes(&[
            ("host", "192.168.1.18:8888"),
            ("origin", "https://exemple.invalid"),
            ("sec-fetch-site", "cross-site"),
        ]);
        assert_eq!(juger(&Method::PATCH, &h), Verdict::InterSites);
        let h = entetes(&[
            ("host", "192.168.1.18:8888"),
            ("origin", "https://exemple.invalid"),
        ]);
        assert_eq!(juger(&Method::POST, &h), Verdict::InterSites);
        let h = entetes(&[("host", "192.168.1.18:8888"), ("origin", "null")]);
        assert_eq!(juger(&Method::DELETE, &h), Verdict::InterSites);
        let h = entetes(&[
            ("host", "192.168.1.18:8888"),
            ("sec-fetch-site", "cross-site"),
        ]);
        assert_eq!(juger(&Method::POST, &h), Verdict::InterSites);
        // Autre port du même hôte : autre origine.
        let h = entetes(&[
            ("host", "192.168.1.18:8888"),
            ("origin", "http://192.168.1.18:9999"),
            ("sec-fetch-site", "same-site"),
        ]);
        assert_eq!(juger(&Method::POST, &h), Verdict::InterSites);
    }

    fn parts(methode: Method, chemin: &str, paires: &[(&str, &str)]) -> Parts {
        let mut r = axum::http::Request::builder().method(methode).uri(chemin);
        for (k, v) in paires {
            r = r.header(*k, *v);
        }
        r.body(()).unwrap().into_parts().0
    }

    #[test]
    fn le_site_ne_lit_que_la_sonde_publique() {
        let site = HeaderValue::from_static(ORIGINE_DU_SITE);
        let autre = HeaderValue::from_static("https://exemple.invalid");
        let hote = [("host", "192.168.1.18:8888")];
        assert!(cors_autorise(
            &site,
            &parts(Method::GET, SONDE_PUBLIQUE, &hote)
        ));
        assert!(cors_autorise(
            &site,
            &parts(Method::GET, "/api/v1/system/health?x=1", &hote)
        ));
        // Une autre origine ne lit pas la sonde.
        assert!(!cors_autorise(
            &autre,
            &parts(Method::GET, SONDE_PUBLIQUE, &hote)
        ));
        // Le site ne lit rien d'autre, et n'écrit pas.
        for chemin in [
            "/api/v1/system/config",
            "/api/v1/system/health/monitor",
            "/api/v1/system/logs",
        ] {
            assert!(
                !cors_autorise(&site, &parts(Method::GET, chemin, &hote)),
                "{chemin}"
            );
        }
        let preflight = parts(
            Method::OPTIONS,
            SONDE_PUBLIQUE,
            &[
                ("host", "192.168.1.18:8888"),
                ("access-control-request-method", "POST"),
            ],
        );
        assert!(!cors_autorise(&site, &preflight));
    }

    #[test]
    fn natifs_relais_et_lectures_passent() {
        assert_eq!(juger(&Method::POST, &HeaderMap::new()), Verdict::Acceptee);
        let h = entetes(&[
            ("authorization", "Bearer x"),
            ("content-type", "application/json"),
        ]);
        assert_eq!(juger(&Method::PATCH, &h), Verdict::Acceptee);
        let h = entetes(&[
            ("origin", "tauri://localhost"),
            ("sec-fetch-site", "cross-site"),
        ]);
        assert_eq!(juger(&Method::PUT, &h), Verdict::Acceptee);
        let h = entetes(&[("origin", "http://tauri.localhost")]);
        assert_eq!(juger(&Method::POST, &h), Verdict::Acceptee);
        let h = entetes(&[
            ("origin", "https://exemple.invalid"),
            ("sec-fetch-site", "cross-site"),
        ]);
        assert_eq!(juger(&Method::GET, &h), Verdict::Acceptee);
    }
}
