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
//! la seule lecture, par la page des radios du site ([`ORIGINE_DU_SITE`]), des
//! deux chemins exacts qu'elle interroge ([`LECTURES_DU_SITE`]).
//!
//! `TUNE_ALLOWED_ORIGINS` (liste séparée par des virgules, réglée par
//! l'exploitant, jamais par HTTP) ajoute des origines de confiance, par
//! exemple un mandataire inverse qui réécrit l'en-tête `Host`.
//!
//! # Hôtes acceptés
//!
//! Toute requête, quelle que soit sa méthode, est d'abord jugée sur son en-tête
//! `Host` ([`hote_accepte`]) ; hors liste, elle reçoit `421` avant tout
//! traitement. Sont acceptés :
//!
//! - une adresse IP littérale (IPv4, IPv6 entre crochets), quelle qu'elle soit :
//!   accès par IP depuis le réseau local, conteneur Docker joint par l'IP de
//!   son hôte, relais et écoute directe (rejoués sur `127.0.0.1`) ;
//! - `localhost` et `*.localhost` ;
//! - un nom sans point (`nuc`, `tune`), que seul le réseau local résout ;
//! - les suffixes réservés au réseau local : `.local` (mDNS, `tune.local` de
//!   Tune OS), `.lan`, `.home`, `.home.arpa`, `.internal`, `.localdomain` ;
//! - le nom de la machine ([`tune_core::discovery::system_hostname`]) ;
//! - les hôtes de `TUNE_ALLOWED_HOSTS` (séparés par des virgules ; `*.domaine`
//!   accepte aussi les sous-domaines) et ceux des origines de
//!   `TUNE_ALLOWED_ORIGINS`.
//!
//! Une requête sans `Host` (client HTTP/1.0, appel interne) passe : un
//! navigateur en pose toujours un. `X-Forwarded-Host` n'entre pas dans ce
//! jugement : une page de même origine peut le poser elle-même.

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

/// La page publique des radios du site (#370, #7044) interroge, depuis le
/// navigateur, le serveur local : la sonde publique, pour afficher « Connecté
/// à <nom> », et la liste « Mes radios », pour marquer les stations déjà
/// présentes ou favorites. Seule cette origine, seuls ces chemins exacts, en
/// lecture seule : la couche n'ouvre que la LECTURE de leur réponse à ce
/// site-là.
pub const ORIGINE_DU_SITE: &str = "https://mozaiklabs.fr";

/// La sonde publique que lit [`ORIGINE_DU_SITE`] (chemin exact).
pub const SONDE_PUBLIQUE: &str = "/api/v1/system/health";

/// La liste « Mes radios » que lit [`ORIGINE_DU_SITE`] (chemin exact).
pub const MES_RADIOS: &str = "/api/v1/radios";

/// Les chemins exacts que [`ORIGINE_DU_SITE`] peut lire
/// (`site-mozaiklabs/resources/js/lib/tuneServer.ts`). L'ajout d'une station
/// depuis le site est une navigation (`/api/v1/radios/add`), pas une lecture :
/// il n'a pas besoin de CORS et n'est pas ici.
pub const LECTURES_DU_SITE: &[&str] = &[SONDE_PUBLIQUE, MES_RADIOS];

/// Variable d'environnement des hôtes supplémentaires acceptés.
pub const VARIABLE_HOTES: &str = "TUNE_ALLOWED_HOSTS";

/// Suffixes que seul le réseau local résout (aucun n'est délégué dans le DNS
/// public).
const SUFFIXES_LOCAUX: &[&str] = &[
    ".local",
    ".localhost",
    ".lan",
    ".home",
    ".home.arpa",
    ".internal",
    ".localdomain",
];

/// #8198 — le retour de connexion Spotify, renvoyé par le relais du site
/// ([`ORIGINE_DU_SITE`], ou celui que règle `TUNE_SPOTIFY_RELAIS_URL`) en
/// formulaire `POST` : une requête inter-sites par nature. Seul ce chemin
/// exact, en `POST`, échappe au refus : ce qui fait foi y est le `state`,
/// qui doit désigner une autorisation ouverte par CE serveur, et le
/// vérificateur PKCE, qui ne le quitte jamais. Aucune lecture inter-sites
/// n'est ouverte (la couche CORS ne change pas).
pub const RETOUR_SPOTIFY: &str = "/api/v1/streaming/spotify/retour";

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

fn liste_d_environnement(variable: &str) -> Vec<String> {
    std::env::var(variable)
        .ok()
        .map(|v| {
            v.split(',')
                .map(|o| o.trim().to_ascii_lowercase())
                .filter(|o| !o.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// `hote[:port]` → nom ou adresse, en minuscules, sans port, sans crochets ni
/// point final. `None` si la valeur est malformée.
fn nom_de_l_hote(hote: &str) -> Option<String> {
    let h = hote.trim().to_ascii_lowercase();
    if h.is_empty() || h.contains(['@', '/', '\\', ' ']) {
        return None;
    }
    let nom = if let Some(reste) = h.strip_prefix('[') {
        // IPv6 : `[::1]` ou `[::1]:8888`.
        let (adresse, apres) = reste.split_once(']')?;
        if !(apres.is_empty() || apres.starts_with(':')) {
            return None;
        }
        adresse.to_string()
    } else {
        match h.rsplit_once(':') {
            Some((nom, port)) if !nom.contains(':') => {
                if !port.chars().all(|c| c.is_ascii_digit()) {
                    return None;
                }
                nom.to_string()
            }
            // Plusieurs `:` sans crochets : IPv6 nue.
            Some(_) => h.clone(),
            None => h.clone(),
        }
    };
    let nom = nom.trim_end_matches('.').to_string();
    (!nom.is_empty()).then_some(nom)
}

fn est_ip_litterale(nom: &str) -> bool {
    // Zone d'une adresse de lien local (`fe80::1%en0`) : ignorée.
    let sans_zone = nom.split('%').next().unwrap_or(nom);
    sans_zone.parse::<std::net::IpAddr>().is_ok()
}

fn correspond(nom: &str, motif: &str) -> bool {
    let motif = motif.trim().trim_end_matches('.');
    if let Some(domaine) = motif.strip_prefix("*.") {
        nom == domaine || nom.ends_with(&format!(".{domaine}"))
    } else {
        nom_de_l_hote(motif).is_some_and(|m| m == nom)
    }
}

/// Jugement pur de l'en-tête `Host`, sans lecture de l'environnement.
///
/// `declares` : hôtes de l'exploitant (motifs `nom`, `nom:port`, `*.domaine`) ;
/// `machine` : nom de la machine.
pub fn hote_accepte_parmi(hote: &str, declares: &[String], machine: Option<&str>) -> bool {
    let Some(nom) = nom_de_l_hote(hote) else {
        return false;
    };
    if est_ip_litterale(&nom) || nom == "localhost" || !nom.contains('.') {
        return true;
    }
    if SUFFIXES_LOCAUX.iter().any(|s| nom.ends_with(s)) {
        return true;
    }
    if let Some(m) = machine {
        let m = m.trim().trim_end_matches('.').to_ascii_lowercase();
        if !m.is_empty() && nom == m {
            return true;
        }
    }
    declares.iter().any(|d| correspond(&nom, d))
}

fn nom_de_la_machine() -> &'static str {
    static NOM: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    NOM.get_or_init(tune_core::discovery::system_hostname)
}

/// L'en-tête `Host` désigne-t-il ce serveur (voir « Hôtes acceptés ») ?
pub fn hote_accepte(hote: &str) -> bool {
    let mut declares = liste_d_environnement(VARIABLE_HOTES);
    declares.extend(
        origines_de_l_exploitant()
            .iter()
            .filter_map(|o| decouper_origine(o).map(|(_, autorite)| autorite)),
    );
    hote_accepte_parmi(hote, &declares, Some(nom_de_la_machine()))
}

/// L'en-tête `Host` de la requête est-il absent ou accepté ?
pub fn hote_de_la_requete_accepte(entetes: &HeaderMap) -> bool {
    match entetes.get(axum::http::header::HOST) {
        None => true,
        Some(v) => v.to_str().ok().is_some_and(hote_accepte),
    }
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
    // Un `Host` hors liste ne désigne pas ce serveur : son origine non plus.
    if !hote_de_la_requete_accepte(entetes) {
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

/// Le chemin échappe-t-il au refus des requêtes inter-sites ? Seul
/// `POST` [`RETOUR_SPOTIFY`].
pub fn exempte(methode: &Method, chemin: &str) -> bool {
    *methode == Method::POST && chemin == RETOUR_SPOTIFY
}

/// Couche de refus des requêtes inter-sites qui changent l'état.
pub async fn garde(requete: Request, suivant: Next) -> Response {
    if !hote_de_la_requete_accepte(requete.headers()) {
        tracing::warn!(
            methode = %requete.method(),
            chemin = %requete.uri().path(),
            hote = ?requete.headers().get(axum::http::header::HOST),
            "hote_refuse"
        );
        return (
            StatusCode::MISDIRECTED_REQUEST,
            axum::Json(json!({
                "error": "host_not_allowed",
                "message": "Host not allowed. Add it to TUNE_ALLOWED_HOSTS.",
            })),
        )
            .into_response();
    }
    // #8198 — après la garde des hôtes : le retour du relais Spotify.
    if exempte(requete.method(), requete.uri().path()) {
        return suivant.run(requete).await;
    }
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
    // Même liste d'hôtes que la garde : un `Host` étranger ne lit rien.
    if !hote_de_la_requete_accepte(&parts.headers) {
        return false;
    }
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
    // La sonde et « Mes radios », pour la page des radios du site, et pour
    // lui seul.
    LECTURES_DU_SITE.contains(&parts.uri.path())
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
    fn le_site_ne_lit_que_la_sonde_et_mes_radios() {
        let site = HeaderValue::from_static(ORIGINE_DU_SITE);
        let autre = HeaderValue::from_static("https://exemple.invalid");
        let hote = [("host", "192.168.1.18:8888")];
        for chemin in [SONDE_PUBLIQUE, MES_RADIOS, "/api/v1/system/health?x=1"] {
            for m in [Method::GET, Method::HEAD] {
                assert!(
                    cors_autorise(&site, &parts(m.clone(), chemin, &hote)),
                    "{m} {chemin}"
                );
                // Une autre origine ne les lit pas.
                assert!(
                    !cors_autorise(&autre, &parts(m.clone(), chemin, &hote)),
                    "{m} {chemin}"
                );
            }
        }
        // Le site ne lit rien d'autre.
        for chemin in [
            "/api/v1/system/config",
            "/api/v1/system/health/monitor",
            "/api/v1/system/logs",
            "/api/v1/radios/favorites",
            "/api/v1/radios/1",
            "/api/v1/radios/add",
        ] {
            assert!(
                !cors_autorise(&site, &parts(Method::GET, chemin, &hote)),
                "{chemin}"
            );
        }
        // Et n'écrit nulle part, ni directement ni par préflight.
        for chemin in [SONDE_PUBLIQUE, MES_RADIOS] {
            for m in [Method::POST, Method::PUT, Method::DELETE, Method::PATCH] {
                assert!(
                    !cors_autorise(&site, &parts(m.clone(), chemin, &hote)),
                    "{m} {chemin}"
                );
                let preflight = parts(
                    Method::OPTIONS,
                    chemin,
                    &[
                        ("host", "192.168.1.18:8888"),
                        ("access-control-request-method", m.as_str()),
                    ],
                );
                assert!(!cors_autorise(&site, &preflight), "préflight {m} {chemin}");
            }
        }
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

    // ── Hôtes acceptés ────────────────────────────────────────────────

    fn accepte(hote: &str) -> bool {
        hote_accepte_parmi(hote, &[], Some("nuc-salon"))
    }

    #[test]
    fn les_adresses_ip_passent() {
        for h in [
            "192.168.1.18:8888",
            "192.168.1.18",
            "10.0.0.5:8888",
            "127.0.0.1:8888",
            "[::1]:8888",
            "[::1]",
            "[fe80::1]:8888",
            "[2a01:e0a::1]:8888",
            "100.64.0.7:8888",
        ] {
            assert!(accepte(h), "{h}");
        }
    }

    #[test]
    fn les_noms_locaux_passent() {
        for h in [
            "localhost:8888",
            "localhost",
            "app.localhost:8888",
            "tune.local",
            "tune.local:8888",
            "Tune.Local.",
            "Studio-de-Bertrand.local:8888",
            "nas.lan:8888",
            "tune.home.arpa",
            "tune:8888",
            "nuc-salon:8888",
        ] {
            assert!(accepte(h), "{h}");
        }
    }

    #[test]
    fn le_nom_de_la_machine_passe_meme_qualifie() {
        assert!(hote_accepte_parmi(
            "nuc.bureau.example.com:8888",
            &[],
            Some("nuc.bureau.example.com")
        ));
        assert!(!hote_accepte_parmi(
            "nuc.bureau.example.com:8888",
            &[],
            Some("autre")
        ));
    }

    #[test]
    fn les_hotes_declares_par_l_exploitant_passent() {
        let declares = vec![
            "musique.example.com".to_string(),
            "*.maison.example.net".to_string(),
            "tune.example.org:8443".to_string(),
        ];
        let ok = |h| hote_accepte_parmi(h, &declares, None);
        assert!(ok("musique.example.com"));
        assert!(ok("MUSIQUE.example.com:443"));
        assert!(ok("salon.maison.example.net:8888"));
        assert!(ok("maison.example.net"));
        assert!(ok("tune.example.org:8443"));
        assert!(!ok("example.com"));
        assert!(!ok("musique.example.com.attaquant.example"));
        assert!(!ok("attaquantmaison.example.net"));
    }

    #[test]
    fn un_hote_etranger_est_refuse() {
        for h in [
            "attaquant.example:8888",
            "attaquant.example",
            "192.168.1.18.attaquant.example:8888",
            "tune.local.attaquant.example",
            "localhost.attaquant.example",
            "",
            "user@192.168.1.18",
            "tune.local:abc",
        ] {
            assert!(!accepte(h), "{h}");
        }
    }

    /// N1 : la page d'un site étranger dont le nom pointe désormais sur le
    /// serveur se présente « de même origine » ; l'hôte la trahit.
    #[test]
    fn une_page_de_meme_origine_sous_un_hote_etranger_est_refusee() {
        let h = entetes(&[
            ("host", "attaquant.example:8888"),
            ("origin", "http://attaquant.example:8888"),
            ("sec-fetch-site", "cross-site"),
        ]);
        assert!(!hote_de_la_requete_accepte(&h));
        assert!(!est_meme_origine("http://attaquant.example:8888", &h));
        assert_eq!(juger(&Method::POST, &h), Verdict::InterSites);
        // Lecture CORS refusée, même du catalogue public.
        let p = parts(
            Method::GET,
            "/api/v1/library/albums",
            &[("host", "attaquant.example:8888")],
        );
        let o = HeaderValue::from_static("http://attaquant.example:8888");
        assert!(!cors_autorise(&o, &p));
        // L'hôte légitime, lui, lit toujours le catalogue.
        let p = parts(
            Method::GET,
            "/api/v1/library/albums",
            &[("host", "192.168.1.18:8888")],
        );
        assert!(cors_autorise(&o, &p));
    }

    #[test]
    fn une_requete_sans_hote_passe() {
        assert!(hote_de_la_requete_accepte(&HeaderMap::new()));
    }

    /// La garde complète, montée comme en production.
    #[tokio::test]
    async fn la_garde_repond_421_hors_liste() {
        use tower::ServiceExt;
        let app = axum::Router::new()
            .route(
                "/api/v1/system/health",
                axum::routing::get(|| async { "ok" }),
            )
            .layer(axum::middleware::from_fn(garde))
            .layer(couche_cors());
        let requete = |hote: &str| {
            axum::http::Request::builder()
                .uri("/api/v1/system/health")
                .header("host", hote)
                .header("sec-fetch-site", "same-origin")
                .body(axum::body::Body::empty())
                .unwrap()
        };
        for (hote, attendu) in [
            ("192.168.1.18:8888", StatusCode::OK),
            ("tune.local", StatusCode::OK),
            ("127.0.0.1:8888", StatusCode::OK),
            ("[::1]:8888", StatusCode::OK),
            ("attaquant.example:8888", StatusCode::MISDIRECTED_REQUEST),
        ] {
            let r = app.clone().oneshot(requete(hote)).await.unwrap();
            assert_eq!(r.status(), attendu, "{hote}");
        }
    }

    /// `TUNE_ALLOWED_HOSTS` lue par la garde réelle. Aucun autre témoin ne
    /// lit cette variable : la poser ici ne trouble pas la suite parallèle.
    #[test]
    fn la_variable_d_environnement_ajoute_des_hotes() {
        let hote = "musique-n1.example.com:8888";
        assert!(!hote_accepte(hote));
        // SAFETY: variable propre à ce témoin, lue seulement par lui.
        unsafe { std::env::set_var(VARIABLE_HOTES, " autre.example , Musique-N1.example.com ") };
        assert!(hote_accepte(hote));
        assert!(hote_accepte("autre.example"));
        assert!(!hote_accepte("tiers-n1.example.com"));
        unsafe { std::env::remove_var(VARIABLE_HOTES) };
        assert!(!hote_accepte(hote));
    }
}
