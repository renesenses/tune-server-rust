//! Ce qu'une requête venue de loin peut atteindre.
//!
//! Le relais (`tune_core::cloud::relay`) et l'écoute directe
//! (`tune_core::cloud::ecoute_directe::tunnel`) rejouent en local, depuis
//! 127.0.0.1, des requêtes émises ailleurs. Ils les marquent de
//! [`ENTETE_RELAIS`], qu'un appelant distant peut ajouter mais jamais retirer.
//!
//! Une requête marquée :
//!
//! - n'est jamais traitée comme locale : voir [`requete_relayee`], lu par
//!   `auth.rs` (limite de tentatives de connexion, profil `default` sans mot
//!   de passe, exemption DIDL) ;
//! - n'atteint que les routes de [`chemin_relayable`] : la bibliothèque en
//!   lecture, la lecture et sa file, les listes de lecture, radios, podcasts,
//!   favoris, la recherche et les flux. L'administration (`/system/*` hors
//!   `health` et `version`, `/cloud/*`, `/support/*`, `/export/*`,
//!   `/appliance/*`, réglages, greffons, réseau…) n'y est jamais ; `/auth/*`
//!   ne s'y ouvre que pour se connecter, et seulement si l'authentification
//!   est active.
//!
//! La liste est écrite ICI, côté serveur, et pas seulement dans le relais :
//! le serveur ne dépend pas de la version du relais qui le joint.

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tune_core::cloud::ENTETE_RELAIS;
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

/// La requête a-t-elle été rejouée par le relais ou l'écoute directe ?
pub fn requete_relayee(entetes: &HeaderMap) -> bool {
    entetes.contains_key(ENTETE_RELAIS)
}

/// L'adresse sous laquelle une requête relayée est comptée par la limite de
/// tentatives de connexion. Le relais masque l'adresse réelle : toutes les
/// requêtes relayées partagent ce compteur, et il n'est jamais en boucle
/// locale (préfixe IPv6 « à jeter », RFC 6666).
pub const ADRESSE_DES_REQUETES_RELAYEES: std::net::IpAddr =
    std::net::IpAddr::V6(std::net::Ipv6Addr::new(0x0100, 0, 0, 0, 0, 0, 0x7475, 0x6e65));

/// L'adresse à prendre en compte pour l'appelant : celle de la socket, sauf
/// pour une requête relayée.
pub fn adresse_de_l_appelant(pair: std::net::IpAddr, entetes: &HeaderMap) -> std::net::IpAddr {
    if requete_relayee(entetes) {
        ADRESSE_DES_REQUETES_RELAYEES
    } else {
        pair
    }
}

fn lecture(m: &Method) -> bool {
    m == Method::GET || m == Method::HEAD
}

/// Sous `prefixe` exactement, ou dans son sous-arbre.
fn sous(chemin: &str, prefixe: &str) -> bool {
    chemin == prefixe
        || chemin
            .strip_prefix(prefixe)
            .is_some_and(|reste| reste.starts_with('/'))
}

/// Segments d'un chemin de service de streaming qui touchent au compte ou aux
/// réglages du service : jamais en écriture par le relais.
const SEGMENTS_DE_COMPTE: &[&str] = &[
    "auth",
    "login",
    "logout",
    "connect",
    "disconnect",
    "credentials",
    "config",
    "settings",
    "token",
    "tokens",
    "oauth",
    "cookie",
    "cookies",
    "debug",
];

/// Une route de l'API (`/api/v1` retiré) relayable ?
fn route_d_api_relayable(m: &Method, api: &str, auth_active: bool) -> bool {
    // Lecture, file, volume, transfert : tout ce qui pilote une zone.
    if sous(api, "/zones") || sous(api, "/playback") {
        return true;
    }
    // Ce que l'auditeur range lui-même.
    if sous(api, "/playlists")
        || sous(api, "/radios")
        || sous(api, "/radio-favorites")
        || sous(api, "/podcasts")
        || sous(api, "/history")
        || sous(api, "/library/history")
        || sous(api, "/search")
        || sous(api, "/library/search")
        || sous(api, "/ext/circle")
    {
        return true;
    }
    // Profils : lire, choisir le sien, ses favoris et ses préférences. Créer,
    // modifier ou supprimer un profil n'est pas relayable.
    if sous(api, "/profiles") {
        if lecture(m) {
            return true;
        }
        let segments: Vec<&str> = api.split('/').filter(|s| !s.is_empty()).collect();
        return match segments.as_slice() {
            ["profiles", "switch" | "deactivate"] => true,
            ["profiles", _, "activate" | "settings"] => true,
            ["profiles", _, "favorites", ..] => true,
            _ => false,
        };
    }
    // Bibliothèque : en lecture ; en écriture, seulement les gestes d'écoute.
    if sous(api, "/library") {
        if lecture(m) {
            return true;
        }
        let segments: Vec<&str> = api.split('/').filter(|s| !s.is_empty()).collect();
        return matches!(
            segments.as_slice(),
            ["library", "tracks" | "albums", _, "quick-fav"] | ["library", "albums", _, "rate"]
        );
    }
    // Services de streaming : parcourir et jouer, jamais le compte du service.
    if sous(api, "/streaming") {
        let touche_au_compte = api
            .split('/')
            .any(|s| SEGMENTS_DE_COMPTE.contains(&s.to_ascii_lowercase().as_str()));
        if !touche_au_compte {
            return true;
        }
        // Lire si l'on est connecté au service (`…/auth/status`), rien d'autre.
        return lecture(m) && api.ends_with("/status");
    }
    // Se connecter, quand il y a de quoi se connecter.
    if auth_active
        && matches!(
            (m.as_str(), api),
            ("POST", "/auth/login") | ("POST", "/auth/logout") | ("GET", "/auth/me")
        )
    {
        return true;
    }
    if !lecture(m) {
        return false;
    }
    // Lecture seule.
    sous(api, "/home")
        || sous(api, "/metadata")
        || sous(api, "/lyrics")
        || sous(api, "/i18n")
        || sous(api, "/tags")
        || sous(api, "/stats/listening")
        || sous(api, "/ext/bandcamp")
        || api == "/radio/auto"
        || api == "/devices"
        || api == "/devices/"
        || api == "/plugins"
        || api == "/plugins/"
        || api == "/dashboard/stats"
        || api == "/hqplayer/status"
        || api == "/system/health"
        || api == "/system/version"
        || api == "/ws"
}

/// Un chemin que le serveur accepte d'une requête relayée.
///
/// Le chemin est jugé tel que reçu : tout ce qui pourrait être normalisé
/// autrement par une couche suivante (`..`, `%2e`, `%2f`, `\`, `//`) est
/// refusé d'emblée.
pub fn chemin_relayable(m: &Method, chemin: &str, auth_active: bool) -> bool {
    let bas = chemin.to_ascii_lowercase();
    if !chemin.starts_with('/')
        || chemin.contains("..")
        || chemin.contains('\\')
        || chemin.contains("//")
        || bas.contains("%2e")
        || bas.contains("%2f")
        || bas.contains("%5c")
    {
        return false;
    }
    // Les flux audio, montés à la racine.
    if sous(chemin, "/stream") || sous(chemin, "/deezer-proxy") {
        return lecture(m);
    }
    if chemin == "/ws" {
        return m == Method::GET;
    }
    match chemin.strip_prefix("/api/v1") {
        Some(api) if api.starts_with('/') => route_d_api_relayable(m, api, auth_active),
        _ => false,
    }
}

fn auth_active(state: &AppState) -> bool {
    SettingsRepo::with_backend(state.backend.clone())
        .get("auth_enabled")
        .ok()
        .flatten()
        .is_some_and(|v| v == "true")
}

/// Couche posée sur tout le routeur : une requête relayée hors de
/// [`chemin_relayable`] reçoit 403, sans atteindre aucun gestionnaire.
pub async fn garde_du_relais(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if !requete_relayee(request.headers()) {
        return next.run(request).await;
    }
    let chemin = request.uri().path().to_string();
    if chemin_relayable(request.method(), &chemin, auth_active(&state)) {
        return next.run(request).await;
    }
    tracing::info!(methode = %request.method(), "requete_relayee_refusee");
    (
        StatusCode::FORBIDDEN,
        axum::Json(json!({
            "error": "not_available_remotely",
            "message": "Cette fonction n'est disponible que sur le réseau local.",
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(m: Method, c: &str) -> bool {
        chemin_relayable(&m, c, false)
    }

    #[test]
    fn ecoute_relayable() {
        for (m, c) in [
            (Method::GET, "/api/v1/library/albums"),
            (Method::GET, "/api/v1/library/tracks/42/audio"),
            (Method::GET, "/api/v1/library/artwork/abc"),
            (Method::POST, "/api/v1/zones/3/queue/add"),
            (Method::PUT, "/api/v1/zones/3/volume"),
            (Method::POST, "/api/v1/playlists/7/tracks"),
            (Method::POST, "/api/v1/profiles/1/favorites/add"),
            (Method::POST, "/api/v1/profiles/1/activate"),
            (Method::POST, "/api/v1/library/tracks/9/quick-fav"),
            (Method::POST, "/api/v1/library/search/acoustic"),
            (Method::POST, "/api/v1/streaming/qobuz/favorites/add"),
            (Method::GET, "/api/v1/streaming/qobuz/album/1"),
            (Method::GET, "/api/v1/system/health"),
            (Method::GET, "/stream/2338763c.flac"),
            (Method::HEAD, "/stream/2338763c.flac"),
            (Method::GET, "/ws"),
            (Method::GET, "/api/v1/ws"),
        ] {
            assert!(ok(m.clone(), c), "{m} {c} doit rester relayable");
        }
    }

    #[test]
    fn administration_refusee() {
        for (m, c) in [
            (Method::GET, "/api/v1/system/config"),
            (Method::PATCH, "/api/v1/system/config"),
            (Method::GET, "/api/v1/system/config/export"),
            (Method::POST, "/api/v1/system/database/migrate"),
            (Method::GET, "/api/v1/system/database/export"),
            (Method::GET, "/api/v1/system/logs"),
            (Method::GET, "/api/v1/cloud/bridge/access-link"),
            (Method::GET, "/api/v1/cloud/sso/status"),
            (Method::POST, "/api/v1/auth/login"),
            (Method::POST, "/api/v1/auth/token"),
            (Method::POST, "/api/v1/auth/config"),
            (Method::GET, "/api/v1/export/library.csv"),
            (Method::POST, "/api/v1/appliance/wifi"),
            (Method::POST, "/api/v1/network/smb/mount"),
            (Method::POST, "/api/v1/onboarding/complete"),
            (Method::POST, "/api/v1/support/tickets"),
            (Method::POST, "/api/v1/profiles"),
            (Method::DELETE, "/api/v1/profiles/2"),
            (Method::POST, "/api/v1/library/ingest/apply"),
            (Method::POST, "/api/v1/library/albums/3/edition/write-tags"),
            (Method::POST, "/api/v1/streaming/qobuz/login"),
            (Method::POST, "/api/v1/streaming/youtube/auth/poll"),
            (Method::POST, "/api/v1/ext/dj/start"),
            (Method::POST, "/api/v1/plugins/install"),
            (Method::GET, "/api/v1/plugins/x/config"),
            (Method::POST, "/stream/abc"),
            (Method::GET, "/agent-tune/pair"),
            (Method::GET, "/ws/bridge"),
            (Method::GET, "/"),
        ] {
            assert!(!ok(m.clone(), c), "{m} {c} ne doit pas être relayable");
        }
    }

    #[test]
    fn chemins_detournes_refuses() {
        for c in [
            "/api/v1/library/../system/config",
            "/api/v1/library/%2e%2e/system/config",
            "/api/v1/library/%2E%2E%2Fsystem%2Fconfig",
            "/stream/..%2fapi/v1/system/config",
            "/api/v1/library\\..\\system",
            "//api/v1/library/albums",
            "/api/v1/zonesx/1",
            "/api/v1/libraryx",
        ] {
            assert!(!ok(Method::GET, c), "{c} ne doit pas être relayable");
        }
    }

    #[test]
    fn connexion_seulement_si_l_auth_est_active() {
        assert!(!chemin_relayable(&Method::POST, "/api/v1/auth/login", false));
        assert!(chemin_relayable(&Method::POST, "/api/v1/auth/login", true));
        assert!(chemin_relayable(&Method::GET, "/api/v1/auth/me", true));
        // Même authentifié, l'administration reste locale.
        assert!(!chemin_relayable(&Method::POST, "/api/v1/auth/token", true));
        assert!(!chemin_relayable(&Method::GET, "/api/v1/system/config", true));
        assert!(!chemin_relayable(&Method::POST, "/api/v1/auth/register", true));
    }

    #[test]
    fn adresse_des_requetes_relayees() {
        let mut h = HeaderMap::new();
        let local: std::net::IpAddr = [127, 0, 0, 1].into();
        assert_eq!(adresse_de_l_appelant(local, &h), local);
        h.insert(ENTETE_RELAIS, "1".parse().unwrap());
        let vue = adresse_de_l_appelant(local, &h);
        assert!(!vue.is_loopback());
        assert_eq!(vue, ADRESSE_DES_REQUETES_RELAYEES);
    }
}
