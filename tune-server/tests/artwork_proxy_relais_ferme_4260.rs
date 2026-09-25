//! #4260 — `GET /library/artwork/proxy?url=…` n'est plus un relais ouvert.
//!
//! Le gestionnaire relayait n'importe quelle URL que le client lui donnait, et
//! #4061 a publié cette route au LAN dans le `<upnp:albumArtURI>` du dossier
//! Radio de la DIDL — sans jeton, puisque #3933 exempte les ressources de la
//! DIDL de l'authentification. Tout appareil du réseau pouvait faire faire à
//! Tune une requête vers `169.254.169.254`, `localhost:8888/api/…` ou un hôte
//! interne, et lire la réponse.
//!
//! Deux couches, éprouvées ici SUR LA ROUTE (le routeur complet, donc la
//! couche d'authentification réellement montée) :
//! 1. signature HMAC des URL que le serveur publie lui-même (DIDL) ;
//! 2. liste d'hôtes pour les URL non signées, et refus de toute adresse
//!    interne — littérale ou RÉSOLUE — redirections comprises.
//!
//! Rien ne sort sur Internet : l'amont est un serveur local, et le relais du
//! banc lui donne un résolveur factice (`static.qobuz.com` → 127.0.0.1) avec
//! une politique qui admet la boucle locale. Le relais de PRODUCTION, lui,
//! n'est employé que là où l'on prouve un refus — et un compteur sur l'amont
//! atteste qu'aucune requête n'est partie.

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use std::net::SocketAddr;
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::library::artwork_proxy::{
    self, Relais, Resolution, Resolveur, adresse_interdite, url_relais_signee,
};
use tune_core::upnp_server::radio_logo_url;
use tune_server::state::AppState;

/// L'adresse que le serveur média annonce dans sa DIDL.
const BASE: &str = "http://192.168.1.10:8888";
/// Un renderer du salon.
const RENDERER_LAN: &str = "192.168.1.42:41234";
/// Un appelant venu d'Internet.
const DISTANT: &str = "203.0.113.7:44444";

/// Les octets de la pochette que l'amont sert.
const POCHETTE: &[u8] = b"\x89PNG\r\n\x1a\n-pochette-4260-";

// ---------------------------------------------------------------------------
// L'amont : un serveur local qui sert une image et des redirections.
// ---------------------------------------------------------------------------

struct Amont {
    port: u16,
    /// Nombre de requêtes reçues, toutes routes confondues.
    requetes: Arc<AtomicUsize>,
}

async fn amont() -> Amont {
    let requetes = Arc::new(AtomicUsize::new(0));
    let compteur = requetes.clone();
    let compte = move || {
        compteur.fetch_add(1, Ordering::SeqCst);
    };
    let c1 = compte.clone();
    let c2 = compte.clone();
    let c3 = compte.clone();
    let c4 = compte.clone();
    let c5 = compte.clone();
    let c6 = compte.clone();
    let app = Router::new()
        .route(
            "/logo.png",
            get(move || {
                c1();
                async { ([(header::CONTENT_TYPE, "image/png")], POCHETTE) }
            }),
        )
        // Redirection vers une adresse lien-local (le « metadata » des nuages).
        .route(
            "/vers-interne",
            get(move || {
                c2();
                async {
                    (
                        StatusCode::FOUND,
                        [(header::LOCATION, "http://169.254.169.254/latest/meta-data/")],
                    )
                }
            }),
        )
        .route(
            "/vers-hors-liste",
            get(move || {
                c3();
                async {
                    (
                        StatusCode::FOUND,
                        [(header::LOCATION, "http://cdn.inconnu.example/x.png")],
                    )
                }
            }),
        )
        // Redirection relative, même hôte : la forme des CDN.
        .route(
            "/vers-logo",
            get(move || {
                c4();
                async {
                    (
                        StatusCode::MOVED_PERMANENTLY,
                        [(header::LOCATION, "/logo.png")],
                    )
                }
            }),
        )
        .route(
            "/boucle",
            get(move || {
                c5();
                async { (StatusCode::FOUND, [(header::LOCATION, "/boucle")]) }
            }),
        )
        // Ce qu'un attaquant voudrait lire : ne doit JAMAIS être atteint
        // par un chemin refusé.
        .fallback(move || {
            c6();
            async { "secret interne".into_response() }
        });
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let port = ecoute.local_addr().expect("adresse locale").port();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });
    Amont { port, requetes }
}

// ---------------------------------------------------------------------------
// Le relais du banc : résolveur factice, boucle locale admise.
// ---------------------------------------------------------------------------

/// Tout nom de la table résout en 127.0.0.1 ; le reste est inconnu.
struct TableLocale(Vec<&'static str>);

impl Resolveur for TableLocale {
    fn resoudre(&self, nom: &str) -> Resolution {
        let connu = self.0.iter().any(|n| n.eq_ignore_ascii_case(nom));
        Box::pin(async move {
            if connu {
                Ok(vec![IpAddr::from([127, 0, 0, 1])])
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "nom inconnu du banc",
                ))
            }
        })
    }
}

fn relais_du_banc() -> Relais {
    Relais::avec(
        Arc::new(TableLocale(vec![
            "static.qobuz.com",
            "mozaiklabs.fr",
            "interne.exemple",
        ])),
        // La boucle locale est l'amont du banc ; tout le reste suit la
        // politique de production (lien-local, privé, etc. restent refusés).
        |ip| ip == IpAddr::from([127, 0, 0, 1]) || !adresse_interdite(ip),
    )
}

fn etat_du_banc() -> AppState {
    let mut state = AppState::new(":memory:", 0, Default::default()).unwrap();
    state.relais_pochettes = Arc::new(relais_du_banc());
    state
}

fn etat_de_production() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn enable_auth(state: &AppState) {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings.set("auth_enabled", "true").unwrap();
    settings.set("jwt_secret", "test-jwt-secret").unwrap();
}

/// Le routeur complet, précédé de l'adresse de l'appelant exactement comme la
/// production la pose (voir `serveur_media_didl_hors_auth_3933`).
fn app(state: &AppState, peer: &str) -> Router {
    let peer: SocketAddr = peer.parse().unwrap();
    tune_server::routes::router(state.clone()).layer(axum::middleware::from_fn(
        move |mut req: Request<Body>, next: axum::middleware::Next| async move {
            req.extensions_mut().insert(ConnectInfo(peer));
            next.run(req).await
        },
    ))
}

fn chemin_relais(url_distante: &str) -> String {
    format!(
        "/api/v1/library/artwork/proxy?url={}",
        urlencoding::encode(url_distante)
    )
}

/// Le chemin (avec sa requête) d'une URL publiée par le serveur.
fn chemin(url: &str) -> String {
    url.strip_prefix(BASE)
        .unwrap_or_else(|| panic!("URL hors du serveur : {url}"))
        .to_string()
}

async fn appel(app: &Router, chemin: &str, jeton: Option<&str>) -> (StatusCode, Vec<u8>) {
    let mut req = Request::get(chemin);
    if let Some(j) = jeton {
        req = req.header(header::AUTHORIZATION, format!("Bearer {j}"));
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = resp.status();
    let corps = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap()
        .to_vec();
    (statut, corps)
}

fn texte(corps: &[u8]) -> String {
    String::from_utf8_lossy(corps).into_owned()
}

// ---------------------------------------------------------------------------
// Couche 2 — adresses internes, avec le relais de PRODUCTION.
// ---------------------------------------------------------------------------

/// LE témoin de #4260 : une adresse interne, littérale, est refusée en 403
/// et l'amont ne voit AUCUNE requête.
///
/// Les URL sont SIGNÉES, à dessein : non signées, la liste d'hôtes les
/// refuserait déjà (« 127.0.0.1 » n'y est pas) et ce témoin resterait vert
/// sans la garde d'adresse — mesuré en contre-épreuve. Signée, seule la garde
/// d'adresse peut refuser.
#[tokio::test]
async fn une_adresse_interne_litterale_est_refusee_sans_requete() {
    let amont = amont().await;
    let state = etat_de_production();
    let secret = artwork_proxy::secret(&state.backend);
    let app = app(&state, RENDERER_LAN);

    for url in [
        format!("http://127.0.0.1:{}/logo.png", amont.port),
        format!("http://127.0.0.1:{}/api/v1/settings", amont.port),
        "http://169.254.169.254/latest/meta-data/".to_string(),
        "http://10.0.0.1/".to_string(),
        "http://192.168.1.1/".to_string(),
        "http://172.16.0.1/".to_string(),
        "http://0.0.0.0:8888/".to_string(),
        "http://[::1]:8888/".to_string(),
        "http://[::ffff:127.0.0.1]/".to_string(),
        "http://2130706433/".to_string(),
    ] {
        let signee = chemin(&url_relais_signee(BASE, &secret, &url));
        let (statut, corps) = appel(&app, &signee, None).await;
        assert_eq!(
            statut,
            StatusCode::FORBIDDEN,
            "{url} : adresse interne, attendu 403 — obtenu {statut} : {}",
            texte(&corps)
        );
        assert!(
            texte(&corps).contains("artwork_proxy_hote_refuse"),
            "{url} : le motif doit être journalisable : {}",
            texte(&corps)
        );
    }
    assert_eq!(
        amont.requetes.load(Ordering::SeqCst),
        0,
        "le serveur a contacté une adresse interne"
    );
}

/// `localhost` n'est pas une adresse : c'est un NOM, qui résout en 127.0.0.1.
/// La garde doit tomber sur l'adresse rendue par le résolveur — le résolveur
/// du système, ici, avec le relais de production. URL signée, pour la même
/// raison qu'au témoin précédent : que seule la garde d'adresse puisse
/// refuser.
#[tokio::test]
async fn localhost_est_refuse_apres_resolution() {
    let amont = amont().await;
    let state = etat_de_production();
    let secret = artwork_proxy::secret(&state.backend);
    let app = app(&state, RENDERER_LAN);

    let url = format!("http://localhost:{}/logo.png", amont.port);
    let signee = chemin(&url_relais_signee(BASE, &secret, &url));
    let (statut, corps) = appel(&app, &signee, None).await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "localhost résout en boucle locale, attendu 403 — obtenu {statut} : {}",
        texte(&corps)
    );
    assert!(
        texte(&corps).contains("localhost"),
        "le refus doit nommer l'hôte : {}",
        texte(&corps)
    );
    assert_eq!(amont.requetes.load(Ordering::SeqCst), 0);
}

/// Un nom PUBLIC de la liste, mais qui résout en adresse interne (rebinding,
/// split DNS, hosts empoisonné) : refusé sur l'adresse, pas sur la chaîne.
/// Résolveur factice : `static.qobuz.com` → 10.0.0.7.
#[tokio::test]
async fn un_hote_de_la_liste_qui_resout_en_prive_est_refuse() {
    struct VersPrive;
    impl Resolveur for VersPrive {
        fn resoudre(&self, _nom: &str) -> Resolution {
            Box::pin(async { Ok(vec![IpAddr::from([10, 0, 0, 7])]) })
        }
    }
    let mut state = etat_de_production();
    state.relais_pochettes = Arc::new(Relais::avec(Arc::new(VersPrive), |ip| {
        !adresse_interdite(ip)
    }));
    let app = app(&state, RENDERER_LAN);

    let (statut, corps) = appel(
        &app,
        &chemin_relais("https://static.qobuz.com/images/cover.jpg"),
        None,
    )
    .await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "static.qobuz.com → 10.0.0.7 : attendu 403 — obtenu {statut} : {}",
        texte(&corps)
    );
    assert!(
        texte(&corps).contains("10.0.0.7"),
        "le refus doit nommer l'adresse rendue : {}",
        texte(&corps)
    );
}

// ---------------------------------------------------------------------------
// Couche 2 — liste d'hôtes.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn un_hote_hors_liste_non_signe_est_refuse() {
    let amont = amont().await;
    let state = etat_du_banc();
    let app = app(&state, RENDERER_LAN);

    // `interne.exemple` est connu du résolveur du banc : seule la liste
    // d'hôtes peut le refuser.
    let url = format!("http://interne.exemple:{}/logo.png", amont.port);
    let (statut, corps) = appel(&app, &chemin_relais(&url), None).await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "hôte hors liste, attendu 403 — obtenu {statut} : {}",
        texte(&corps)
    );
    assert!(
        texte(&corps).contains("artwork_proxy_hote_refuse")
            && texte(&corps).contains("interne.exemple"),
        "le refus doit nommer l'hôte : {}",
        texte(&corps)
    );
    assert_eq!(amont.requetes.load(Ordering::SeqCst), 0);
}

/// Le client web bâtit ses URL de relais lui-même, sans signature
/// (`api.ts::artworkUrl`) : un hôte de la liste reste accepté, avec ou sans
/// authentification.
#[tokio::test]
async fn un_hote_de_la_liste_non_signe_est_accepte() {
    let amont = amont().await;
    let state = etat_du_banc();
    let app = app(&state, DISTANT);

    let url = format!("http://static.qobuz.com:{}/logo.png", amont.port);
    let (statut, corps) = appel(&app, &chemin_relais(&url), None).await;
    assert_eq!(statut, StatusCode::OK, "{}", texte(&corps));
    assert_eq!(
        corps, POCHETTE,
        "le relais doit rendre l'image telle quelle"
    );
    assert_eq!(amont.requetes.load(Ordering::SeqCst), 1);
}

/// Le réglage `artwork_proxy_hosts` étend la liste — pour une radio dont le
/// logo vit chez un diffuseur que la liste bâtie ne connaît pas.
#[tokio::test]
async fn le_reglage_artwork_proxy_hosts_etend_la_liste() {
    let amont = amont().await;
    let state = etat_du_banc();
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            artwork_proxy::CLE_HOTES_SUPPLEMENTAIRES,
            "autre.test, interne.exemple",
        )
        .unwrap();
    let app = app(&state, DISTANT);

    let url = format!("http://interne.exemple:{}/logo.png", amont.port);
    let (statut, corps) = appel(&app, &chemin_relais(&url), None).await;
    assert_eq!(statut, StatusCode::OK, "{}", texte(&corps));
    assert_eq!(corps, POCHETTE);
}

// ---------------------------------------------------------------------------
// Couche 1 — signature.
// ---------------------------------------------------------------------------

/// L'URL que la DIDL publie (bâtie par `radio_logo_url`, le constructeur
/// même du serveur média) est acceptée telle quelle — y compris vers un hôte
/// hors liste : la signature atteste que c'est le serveur qui l'a produite.
#[tokio::test]
async fn l_url_signee_publiee_par_la_didl_est_acceptee() {
    let amont = amont().await;
    let state = etat_du_banc();
    let secret = artwork_proxy::secret(&state.backend);
    let app = app(&state, RENDERER_LAN);

    let logo = format!("http://interne.exemple:{}/logo.png", amont.port);
    let publiee = radio_logo_url(BASE, &logo, &secret);
    assert!(
        publiee.contains("&sig="),
        "la DIDL doit publier une URL signée : {publiee}"
    );
    let (statut, corps) = appel(&app, &chemin(&publiee), None).await;
    assert_eq!(statut, StatusCode::OK, "{}", texte(&corps));
    assert_eq!(corps, POCHETTE);
}

#[tokio::test]
async fn une_signature_alteree_est_refusee() {
    let amont = amont().await;
    let state = etat_du_banc();
    let secret = artwork_proxy::secret(&state.backend);
    let app = app(&state, RENDERER_LAN);

    let logo = format!("http://static.qobuz.com:{}/logo.png", amont.port);
    let publiee = chemin(&url_relais_signee(BASE, &secret, &logo));
    let (sans_sig, sig) = publiee.split_once("&sig=").unwrap();
    let alteree = format!(
        "{sans_sig}&sig={}{}",
        if sig.starts_with('0') { "1" } else { "0" },
        &sig[1..]
    );
    let (statut, corps) = appel(&app, &alteree, None).await;
    assert_eq!(
        statut,
        StatusCode::BAD_REQUEST,
        "signature altérée, attendu 400 — obtenu {statut} : {}",
        texte(&corps)
    );
    assert!(texte(&corps).contains("artwork_proxy_signature_invalide"));

    // Signée pour une AUTRE URL : même refus.
    let autre = chemin(&url_relais_signee(
        BASE,
        &secret,
        "https://mozaiklabs.fr/x.png",
    ));
    let (_, sig_autre) = autre.split_once("&sig=").unwrap();
    let (statut, _) = appel(&app, &format!("{sans_sig}&sig={sig_autre}"), None).await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);

    // Même signée, une adresse interne reste interdite : la signature
    // n'ouvre pas la couche 2.
    let interne = chemin(&url_relais_signee(
        BASE,
        &secret,
        "http://169.254.169.254/latest/meta-data/",
    ));
    let (statut, corps) = appel(&app, &interne, None).await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "signée ou non, une adresse interne est refusée : {}",
        texte(&corps)
    );
    assert_eq!(amont.requetes.load(Ordering::SeqCst), 0);
}

/// Avec l'authentification active, un appel entré par l'exemption DIDL (LAN,
/// sans jeton) doit porter la signature : une URL non signée — même vers un
/// hôte de la liste — est refusée en 400. Le client web AUTHENTIFIÉ, lui,
/// garde la compatibilité : la même URL non signée passe.
#[tokio::test]
async fn sans_jeton_par_l_exemption_didl_la_signature_est_exigee() {
    let amont = amont().await;
    let state = etat_du_banc();
    enable_auth(&state);
    let secret = artwork_proxy::secret(&state.backend);

    let logo = format!("http://static.qobuz.com:{}/logo.png", amont.port);
    let non_signee = chemin_relais(&logo);

    // Renderer du LAN, sans jeton : exemption DIDL → signature exigée.
    let app_lan = app(&state, RENDERER_LAN);
    let (statut, corps) = appel(&app_lan, &non_signee, None).await;
    assert_eq!(
        statut,
        StatusCode::BAD_REQUEST,
        "exemption DIDL sans signature, attendu 400 — obtenu {statut} : {}",
        texte(&corps)
    );
    assert!(texte(&corps).contains("artwork_proxy_signature_requise"));

    // Le même renderer, avec l'URL que la DIDL lui a donnée : accepté.
    let (statut, corps) = appel(
        &app_lan,
        &chemin(&radio_logo_url(BASE, &logo, &secret)),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{}", texte(&corps));
    assert_eq!(corps, POCHETTE);

    // Client web authentifié, hors LAN : l'URL non signée reste acceptée.
    let jeton = tune_server::auth::sign_jwt(1, "admin", "test-jwt-secret").unwrap();
    let app_web = app(&state, DISTANT);
    let (statut, corps) = appel(&app_web, &non_signee, Some(&jeton)).await;
    assert_eq!(
        statut,
        StatusCode::OK,
        "client web authentifié, hôte de la liste : compatibilité — {}",
        texte(&corps)
    );
    assert_eq!(corps, POCHETTE);
}

// ---------------------------------------------------------------------------
// Redirections.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn une_redirection_vers_l_interne_est_refusee() {
    let amont = amont().await;
    let state = etat_du_banc();
    let app = app(&state, DISTANT);

    let url = format!("http://static.qobuz.com:{}/vers-interne", amont.port);
    let (statut, corps) = appel(&app, &chemin_relais(&url), None).await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "redirection vers 169.254.169.254, attendu 403 — obtenu {statut} : {}",
        texte(&corps)
    );
    assert!(
        texte(&corps).contains("169.254.169.254"),
        "le refus doit nommer l'adresse : {}",
        texte(&corps)
    );
    // Une seule requête : la première. La cible n'a pas été touchée.
    assert_eq!(amont.requetes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn une_redirection_vers_un_hote_hors_liste_est_refusee() {
    let amont = amont().await;
    let state = etat_du_banc();
    let app = app(&state, DISTANT);

    let url = format!("http://static.qobuz.com:{}/vers-hors-liste", amont.port);
    let (statut, corps) = appel(&app, &chemin_relais(&url), None).await;
    assert_eq!(statut, StatusCode::FORBIDDEN, "{}", texte(&corps));
    assert!(
        texte(&corps).contains("cdn.inconnu.example"),
        "le refus doit nommer l'hôte : {}",
        texte(&corps)
    );
}

/// La forme des CDN : une redirection relative, même hôte — suivie.
#[tokio::test]
async fn une_redirection_vers_le_meme_hote_est_suivie() {
    let amont = amont().await;
    let state = etat_du_banc();
    let app = app(&state, DISTANT);

    let url = format!("http://static.qobuz.com:{}/vers-logo", amont.port);
    let (statut, corps) = appel(&app, &chemin_relais(&url), None).await;
    assert_eq!(statut, StatusCode::OK, "{}", texte(&corps));
    assert_eq!(corps, POCHETTE);
    assert_eq!(amont.requetes.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn une_boucle_de_redirections_est_coupee() {
    let amont = amont().await;
    let state = etat_du_banc();
    let app = app(&state, DISTANT);

    let url = format!("http://static.qobuz.com:{}/boucle", amont.port);
    let (statut, corps) = appel(&app, &chemin_relais(&url), None).await;
    assert_eq!(statut, StatusCode::FORBIDDEN, "{}", texte(&corps));
    assert!(texte(&corps).contains("trop de redirections"));
    assert!(
        amont.requetes.load(Ordering::SeqCst) <= 6,
        "la boucle doit être bornée"
    );
}

/// Ce qui n'est pas une URL http(s) ne part nulle part.
#[tokio::test]
async fn un_schema_autre_que_http_est_refuse() {
    let state = etat_de_production();
    let app = app(&state, DISTANT);
    for url in [
        "file:///etc/passwd",
        "ftp://mozaiklabs.fr/a.png",
        "pas une url",
    ] {
        let (statut, _) = appel(&app, &chemin_relais(url), None).await;
        assert_eq!(statut, StatusCode::BAD_REQUEST, "{url}");
    }
}

// ---------------------------------------------------------------------------
// Pochette ENREGISTRÉE d'un album de serveur UPnP (réseau local) — .18,
// 17/09/2026 : les 39 pochettes UPnP étaient grises, `artwork_proxy_hote_refuse`
// à chaque vignette. Admise si et seulement si l'URL est le `cover_path` d'un
// album de la bibliothèque.
// ---------------------------------------------------------------------------

/// Le relais « réseau local » du banc : `mac-studio.lan` → 127.0.0.1 (l'amont
/// du banc), boucle locale admise pour lui seul ; le relais général reste
/// celui du banc, qui ne connaît pas ce nom et le refuse sur la liste d'hôtes.
fn etat_avec_relais_lan() -> AppState {
    let mut state = etat_du_banc();
    state.relais_pochettes_lan = Arc::new(Relais::avec(
        Arc::new(TableLocale(vec!["mac-studio.lan"])),
        |ip| {
            ip == IpAddr::from([127, 0, 0, 1])
                || !adresse_interdite(ip)
                || artwork_proxy::adresse_reseau_local(ip)
        },
    ));
    state
}

fn inscrire_album(state: &AppState, cover_path: &str) {
    let cp = cover_path.to_string();
    state
        .backend
        .execute(
            "INSERT INTO albums (title, cover_path, source) VALUES ('Kino Music', $1, 'upnp')",
            &[&cp as &dyn tune_core::db::backend::ToSqlValue],
        )
        .expect("album inscrit");
}

#[tokio::test]
async fn la_pochette_enregistree_d_un_album_upnp_est_relayee() {
    let amont = amont().await;
    let state = etat_avec_relais_lan();
    let url = format!("http://mac-studio.lan:{}/logo.png", amont.port);
    inscrire_album(&state, &url);
    let app = app(&state, DISTANT);

    let (statut, corps) = appel(&app, &chemin_relais(&url), None).await;
    assert_eq!(statut, StatusCode::OK, "{}", texte(&corps));
    assert_eq!(corps, POCHETTE);
    assert_eq!(amont.requetes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn la_meme_adresse_inconnue_de_la_bibliotheque_reste_refusee() {
    let amont = amont().await;
    let state = etat_avec_relais_lan();
    // Un album existe, mais avec une AUTRE pochette : l'exception tient à
    // l'égalité exacte, pas à l'hôte.
    inscrire_album(
        &state,
        &format!("http://mac-studio.lan:{}/autre.png", amont.port),
    );
    let app = app(&state, DISTANT);

    let url = format!("http://mac-studio.lan:{}/logo.png", amont.port);
    let (statut, corps) = appel(&app, &chemin_relais(&url), None).await;
    assert_eq!(statut, StatusCode::FORBIDDEN, "{}", texte(&corps));
    assert_eq!(
        amont.requetes.load(Ordering::SeqCst),
        0,
        "aucune requête ne doit partir"
    );
}

#[tokio::test]
async fn l_exemption_didl_ne_profite_pas_de_l_exception() {
    // Par la DIDL, une URL non signée est refusée AVANT tout — même si c'est
    // la pochette d'un album.
    let amont = amont().await;
    let state = etat_avec_relais_lan();
    let url = format!("http://mac-studio.lan:{}/logo.png", amont.port);
    inscrire_album(&state, &url);
    enable_auth(&state);
    let app = app(&state, RENDERER_LAN);

    let (statut, _corps) = appel(&app, &chemin_relais(&url), None).await;
    assert_ne!(statut, StatusCode::OK);
    assert_eq!(amont.requetes.load(Ordering::SeqCst), 0);
}

// ---------------------------------------------------------------------------
// #4954 — la pochette d'un SERVEUR TUNE découvert (onglet « Serveurs
// multimédia »). Le client web lit le catalogue d'un serveur Tune par son API
// REST et bâtit, pour chaque album, `http://<ip-lan>:8888/api/v1/library/
// artwork/<condensat>` (`tuneRemote.ts::pochetteDistante`). En 0.9.163 il
// l'envoie à CE relais, qui refusait l'adresse privée : 2 809 tuiles grises
// chez Jean Valjean, le serveur Tune lui-même compris. L'exception tient à
// deux conditions à la fois : l'hôte:port est un serveur multimédia DÉCOUVERT
// (registre SSDP, le nôtre y compris, #3786), et le chemin est EXACTEMENT la
// route de pochette de Tune suivie d'un condensat.
// ---------------------------------------------------------------------------

/// Un condensat SHA-256 (64 hexadécimaux), la forme de `albums.cover_path`.
const CONDENSAT_4954: &str = "ce0a963bb7eb63c3b33b4e00b6ab3427ce0a963bb7eb63c3b33b4e00b6ab3427";

/// Un serveur Tune du banc : sert sa pochette sur la route de Tune, et compte
/// TOUT ce qu'il reçoit (le repli dit « secret » : il ne doit jamais sortir).
async fn amont_tune() -> Amont {
    let requetes = Arc::new(AtomicUsize::new(0));
    let (c1, c2) = (requetes.clone(), requetes.clone());
    let app = Router::new()
        .route(
            "/api/v1/library/artwork/{condensat}",
            get(move || {
                c1.fetch_add(1, Ordering::SeqCst);
                async { ([(header::CONTENT_TYPE, "image/jpeg")], POCHETTE) }
            }),
        )
        .fallback(move || {
            c2.fetch_add(1, Ordering::SeqCst);
            async { "secret interne".into_response() }
        });
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("port libre");
    let port = ecoute.local_addr().expect("adresse locale").port();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });
    Amont { port, requetes }
}

/// Inscrit un serveur multimédia au registre SSDP, comme la découverte.
async fn decouvrir_serveur(state: &AppState, hote: &str, port: u16) {
    let info = tune_core::discovery::ssdp::MediaServerInfo {
        id: format!("uuid:banc-4954-{port}"),
        name: "Tune Server".into(),
        manufacturer: "Mozaiklabs".into(),
        model: "Tune".into(),
        location: format!("http://{hote}:{port}/upnp/description.xml"),
        content_directory_url: format!("http://{hote}:{port}/upnp/control/content_directory"),
        host: hote.into(),
        port,
        last_seen: std::time::Instant::now(),
        max_age: std::time::Duration::from_secs(1800),
    };
    state
        .media_servers
        .lock()
        .await
        .insert(info.id.clone(), info);
}

/// LE témoin de #4954 : l'URL que le client bâtit pour un album d'un serveur
/// Tune découvert, adresse littérale, est relayée.
#[tokio::test]
async fn la_pochette_d_un_serveur_tune_decouvert_est_relayee_4954() {
    let amont = amont_tune().await;
    let state = etat_avec_relais_lan();
    decouvrir_serveur(&state, "127.0.0.1", amont.port).await;
    let app = app(&state, DISTANT);

    let url = format!(
        "http://127.0.0.1:{}/api/v1/library/artwork/{CONDENSAT_4954}",
        amont.port
    );
    let (statut, corps) = appel(&app, &chemin_relais(&url), None).await;
    assert_eq!(statut, StatusCode::OK, "{}", texte(&corps));
    assert_eq!(corps, POCHETTE);
    assert_eq!(amont.requetes.load(Ordering::SeqCst), 1);
}

/// Contre-garde : même URL, serveur ABSENT du registre ⇒ refus, rien ne part.
#[tokio::test]
async fn la_pochette_d_un_serveur_non_decouvert_reste_refusee_4954() {
    let amont = amont_tune().await;
    let state = etat_avec_relais_lan();
    // Un serveur découvert sur le même hôte, mais un AUTRE port.
    decouvrir_serveur(&state, "127.0.0.1", amont.port.wrapping_add(1)).await;
    let app = app(&state, DISTANT);

    let url = format!(
        "http://127.0.0.1:{}/api/v1/library/artwork/{CONDENSAT_4954}",
        amont.port
    );
    let (statut, corps) = appel(&app, &chemin_relais(&url), None).await;
    assert_eq!(statut, StatusCode::FORBIDDEN, "{}", texte(&corps));
    assert_eq!(amont.requetes.load(Ordering::SeqCst), 0);
}

/// Contre-garde : serveur découvert, mais un AUTRE chemin que la pochette —
/// l'exception ne fait pas du relais une porte vers son API.
#[tokio::test]
async fn un_autre_chemin_d_un_serveur_decouvert_reste_refuse_4954() {
    let amont = amont_tune().await;
    let state = etat_avec_relais_lan();
    decouvrir_serveur(&state, "127.0.0.1", amont.port).await;
    let app = app(&state, DISTANT);

    for chemin_amont in [
        "/api/v1/system/profile".to_string(),
        "/api/v1/library/artwork/proxy?url=http://169.254.169.254/".to_string(),
        "/api/v1/library/artwork/pas-un-condensat".to_string(),
        format!("/api/v1/library/artwork/{CONDENSAT_4954}?x=1"),
        format!("/api/v1/library/artwork/{CONDENSAT_4954}/../../system/profile"),
    ] {
        let url = format!("http://127.0.0.1:{}{chemin_amont}", amont.port);
        let (statut, corps) = appel(&app, &chemin_relais(&url), None).await;
        assert_eq!(
            statut,
            StatusCode::FORBIDDEN,
            "{chemin_amont} : {}",
            texte(&corps)
        );
    }
    assert_eq!(amont.requetes.load(Ordering::SeqCst), 0);
}

/// Contre-garde : par l'exemption DIDL (sans jeton), pas d'exception.
#[tokio::test]
async fn l_exemption_didl_ne_profite_pas_du_serveur_decouvert_4954() {
    let amont = amont_tune().await;
    let state = etat_avec_relais_lan();
    decouvrir_serveur(&state, "127.0.0.1", amont.port).await;
    enable_auth(&state);
    let app = app(&state, RENDERER_LAN);

    let url = format!(
        "http://127.0.0.1:{}/api/v1/library/artwork/{CONDENSAT_4954}",
        amont.port
    );
    let (statut, _corps) = appel(&app, &chemin_relais(&url), None).await;
    assert_ne!(statut, StatusCode::OK);
    assert_eq!(amont.requetes.load(Ordering::SeqCst), 0);
}
