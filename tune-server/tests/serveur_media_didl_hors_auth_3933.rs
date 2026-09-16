//! #3933 — ce que le serveur média publie dans sa DIDL doit rester atteignable
//! quand `auth_enabled = true`.
//!
//! Mesure d'origine : `auth_middleware` enveloppe le routeur `api`
//! (`routes/mod.rs`, `app.nest("/api/v1", api)`), tandis que `/upnp` et
//! `/stream` sont montés à la RACINE, hors de cette couche. Or les quatre URL
//! de ressource que `upnp_server.rs` place dans sa DIDL vivent TOUTES sous
//! `/api/v1`. Un renderer DLNA découvrait le serveur, le parcourait, et se
//! faisait refuser en 401 chaque ressource qu'il y trouvait — l'audio comme la
//! pochette — sans aucun moyen de s'authentifier.
//!
//! Les chemins éprouvés ici ne sont JAMAIS écrits à la main : ils sont
//! fabriqués par les constructeurs d'URL de `tune_core::upnp_server`, ceux-là
//! mêmes qui remplissent la DIDL. Une cinquième ressource ajoutée à la DIDL
//! sans entrée dans `est_ressource_didl` ferait donc rougir ce témoin.
//!
//! Et c'est la ROUTE qui est éprouvée, pas la fonction de décision : les
//! requêtes traversent `tune_server::routes::router(...)` complet, donc la
//! couche d'authentification réellement montée.

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use std::net::SocketAddr;
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::upnp_server::{artwork_url, radio_audio_url, radio_logo_url, track_audio_url};
use tune_server::state::AppState;

/// L'adresse que le serveur média annonce dans sa DIDL.
const BASE: &str = "http://192.168.1.10:8888";

/// Un renderer du salon : plage privée RFC 1918.
const RENDERER_LAN: &str = "192.168.1.42:41234";
/// Le même appelant vu par une socket à double pile.
const RENDERER_LAN_V6_MAPPE: &str = "[::ffff:192.168.1.42]:41234";
/// Un appelant venu d'Internet — TEST-NET-3, jamais routable chez personne.
const DISTANT: &str = "203.0.113.7:44444";

/// Un logo de station tel que `refresh_radio_logos` en écrit : une adresse
/// absolue de l'annuaire, que la DIDL relaie par `/library/artwork/proxy`.
/// Pointe sur un port fermé de la machine pour que le relais échoue tout de
/// suite : ce témoin juge le 401, pas ce que rend le relais.
const LOGO_DISTANT: &str = "http://127.0.0.1:1/radio-logos/exemple.png";

/// Un condensat du cache de pochettes (32 hexadécimaux, forme MD5 héritée).
const CONDENSAT: &str = "0123456789abcdef0123456789abcdef";

/// Identifiants volontairement absents de la base.
///
/// Une base neuve n'est PAS vide : la migration livre une cinquantaine de
/// stations. Avec `radio_id = 12`, le GET franchissait bien la couche
/// d'authentification — c'est ce qu'on veut prouver — mais ouvrait ensuite une
/// vraie session de diffusion vers le diffuseur amont, sur Internet, et le
/// témoin ne rendait plus jamais la main. Ce banc juge le verdict de la couche
/// d'authentification (401 ou pas), jamais ce que rend le gestionnaire : des
/// identifiants inexistants suffisent et restent hors du réseau.
const PISTE_ABSENTE: i64 = 999_000_001;
const RADIO_ABSENTE: i64 = 999_000_002;

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn enable_auth(state: &AppState) {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("auth_enabled", "true").unwrap();
    settings.set("jwt_secret", "test-jwt-secret").unwrap();
}

/// Le routeur complet, précédé de l'adresse de l'appelant **exactement comme
/// la production la pose**.
///
/// Pas `MockConnectInfo` : ce raccourci d'axum insère un `MockConnectInfo<T>`
/// dans les extensions, pas un `ConnectInfo<T>` — seul l'extracteur
/// `ConnectInfo` sait retomber dessus. La couche d'authentification, elle, lit
/// l'extension directement (elle reçoit une `Request`, pas des extracteurs), et
/// ne verrait donc rien. Ce qu'`into_make_service_with_connect_info` pose en
/// production (`bootstrap.rs`), c'est `ConnectInfo(peer)` : c'est ce qu'on pose
/// ici. Un témoin bâti sur `MockConnectInfo` serait resté ROUGE avec le
/// correctif en place — mesuré.
fn app(state: &AppState, peer: &str) -> Router {
    let peer: SocketAddr = peer.parse().unwrap();
    tune_server::routes::router(state.clone()).layer(axum::middleware::from_fn(
        move |mut req: Request<Body>, next: axum::middleware::Next| async move {
            req.extensions_mut().insert(ConnectInfo(peer));
            next.run(req).await
        },
    ))
}

/// Le chemin (avec sa requête) d'une URL publiée dans la DIDL.
fn chemin(url: &str) -> String {
    url.strip_prefix(BASE)
        .unwrap_or_else(|| panic!("la DIDL a publié une URL hors du serveur : {url}"))
        .to_string()
}

async fn statut(app: &Router, methode: &str, chemin: &str) -> StatusCode {
    let req = Request::builder()
        .method(methode)
        .uri(chemin)
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

/// Les quatre ressources de la DIDL, telles que la DIDL les épelle.
fn ressources_publiees_par_la_didl() -> Vec<(&'static str, String)> {
    vec![
        (
            "audio de piste",
            chemin(&track_audio_url(BASE, PISTE_ABSENTE)),
        ),
        ("flux radio", chemin(&radio_audio_url(BASE, RADIO_ABSENTE))),
        ("pochette", chemin(&artwork_url(BASE, CONDENSAT))),
        (
            "logo de station relayé",
            // #4260 : l'URL de relais est signée ; le secret n'importe pas
            // ici, seule la couche d'authentification est jugée.
            chemin(&radio_logo_url(BASE, LOGO_DISTANT, "secret-de-banc")),
        ),
    ]
}

/// LE témoin de #3933 : avec l'authentification active, un renderer du réseau
/// local ne doit plus se faire refuser ce que la DIDL lui a donné.
#[tokio::test]
async fn le_renderer_du_lan_atteint_tout_ce_que_la_didl_publie() {
    let state = new_state();
    enable_auth(&state);
    let app = app(&state, RENDERER_LAN);

    for (quoi, chemin) in ressources_publiees_par_la_didl() {
        let st = statut(&app, "GET", &chemin).await;
        assert_ne!(
            st,
            StatusCode::UNAUTHORIZED,
            "{quoi} ({chemin}) : la DIDL le publie, un renderer ne peut pas s'authentifier"
        );
    }
}

/// Un renderer sonde en HEAD avant de lire — la route radio a d'ailleurs son
/// propre gestionnaire `.head(…)`. Une exemption réservée au GET laisserait
/// l'ampli échouer avant même de demander les octets.
#[tokio::test]
async fn la_sonde_head_du_renderer_passe_aussi() {
    let state = new_state();
    enable_auth(&state);
    let app = app(&state, RENDERER_LAN);

    for (quoi, chemin) in ressources_publiees_par_la_didl() {
        let st = statut(&app, "HEAD", &chemin).await;
        assert_ne!(
            st,
            StatusCode::UNAUTHORIZED,
            "{quoi} ({chemin}) : la sonde HEAD précède la lecture"
        );
    }
}

/// Une socket à double pile rend l'appelant IPv4 sous la forme
/// `::ffff:192.168.1.42`. Sans déballage, tout le LAN serait vu comme distant.
#[tokio::test]
async fn le_lan_vu_en_ipv4_mappe_reste_du_lan() {
    let state = new_state();
    enable_auth(&state);
    let app = app(&state, RENDERER_LAN_V6_MAPPE);

    let chemin = chemin(&track_audio_url(BASE, PISTE_ABSENTE));
    assert_ne!(
        statut(&app, "GET", &chemin).await,
        StatusCode::UNAUTHORIZED,
        "::ffff:192.168.1.42 est le même renderer que 192.168.1.42"
    );
}

/// L'exemption ne franchit pas le LAN : un serveur publié sur Internet ne
/// s'ouvre pas. La DIDL n'est de toute façon annoncée qu'en SSDP.
#[tokio::test]
async fn un_appelant_distant_reste_refuse() {
    let state = new_state();
    enable_auth(&state);
    let app = app(&state, DISTANT);

    for (quoi, chemin) in ressources_publiees_par_la_didl() {
        assert_eq!(
            statut(&app, "GET", &chemin).await,
            StatusCode::UNAUTHORIZED,
            "{quoi} ({chemin}) : hors du LAN, le jeton reste exigé"
        );
    }
}

/// La brèche que cette exemption ne doit PAS ouvrir : le reste de l'API.
/// Le serveur média ne publie ni la liste des albums, ni les réglages, ni la
/// moindre route d'écriture — rien de tout cela ne doit passer.
#[tokio::test]
async fn le_reste_de_l_api_reste_ferme_au_lan() {
    let state = new_state();
    enable_auth(&state);
    let app = app(&state, RENDERER_LAN);

    for chemin in [
        "/api/v1/library/albums",
        "/api/v1/library/tracks",
        "/api/v1/radios",
        "/api/v1/settings",
        "/api/v1/library/artwork/enrich/status",
        // Un condensat, ça n'est pas n'importe quoi : `est_ressource_didl`
        // n'ouvre `/library/artwork/{x}` que sur 32 ou 64 hexadécimaux.
        "/api/v1/library/artwork/pas-un-condensat",
    ] {
        assert_eq!(
            statut(&app, "GET", chemin).await,
            StatusCode::UNAUTHORIZED,
            "{chemin} n'est pas publié par le serveur média"
        );
    }
}

/// GET et HEAD, rien d'autre. Le contrôle de méthode est fait par la couche
/// d'authentification, donc AVANT le routage : un POST sur le chemin de
/// l'audio d'une piste doit rendre 401, pas 405.
#[tokio::test]
async fn une_ecriture_sur_un_chemin_de_la_didl_reste_refusee() {
    let state = new_state();
    enable_auth(&state);
    let app = app(&state, RENDERER_LAN);

    let chemin = chemin(&track_audio_url(BASE, PISTE_ABSENTE));
    for methode in ["POST", "PUT", "DELETE"] {
        assert_eq!(
            statut(&app, methode, &chemin).await,
            StatusCode::UNAUTHORIZED,
            "{methode} {chemin} : seules la lecture et la sonde sont exemptées"
        );
    }
}

/// Un jeton valide continue de faire ce qu'il faisait : l'exemption s'ajoute,
/// elle ne remplace rien.
#[tokio::test]
async fn un_jeton_valide_ouvre_toujours_le_reste_de_l_api() {
    let state = new_state();
    enable_auth(&state);
    let app = app(&state, DISTANT);

    let jeton = tune_server::auth::sign_jwt(1, "admin", "test-jwt-secret").unwrap();
    let req = Request::get("/api/v1/library/albums")
        .header(header::AUTHORIZATION, format!("Bearer {jeton}"))
        .body(Body::empty())
        .unwrap();
    let st = app.clone().oneshot(req).await.unwrap().status();
    assert_ne!(
        st,
        StatusCode::UNAUTHORIZED,
        "un jeton admin doit toujours ouvrir l'API"
    );
}
