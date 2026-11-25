//! rc14 — sans authentification, les actions d'administration
//! (`AdminOuCetteMachine`) sont acceptées depuis le RÉSEAU LOCAL.
//!
//! Ce que ces épreuves tiennent, sur les ROUTES MONTÉES :
//!
//! 1. adresse privée (RFC 1918, ULA, lien local) : acceptée ;
//! 2. adresse publique, CGNAT/Tailscale : refusée ;
//! 3. relais distant ou écoute directe (en-tête du relais) : refusé ;
//! 4. conteneur en réseau en pont : refusé par défaut ; avec
//!    `TUNE_ADMIN_LAN_IN_CONTAINER=1` (réseau de l'hôte), le réseau local
//!    passe, jamais la passerelle ;
//! 5. Tune OS avec relais du port 80 : inchangé (refusé, phrase du relais) ;
//! 6. réglage « Administration depuis cette machine seulement » : seule la
//!    machine passe, et seule elle peut l'éteindre ;
//! 7. `X-Forwarded-For` / `Forwarded` : jamais crus ;
//! 8. authentification active : inchangé (l'administrateur, de partout) ;
//! 9. requête d'un autre site ou `Host` étranger : refusée ;
//! 10. la déliaison du compte reste à cette machine.
//!
//! Sondes sans effet : `POST /auth/admin-scope` (le réglage lui-même) ;
//! `POST /appliance/reboot` hors appliance rend 404 APRÈS la garde — un 403
//! prouve donc le refus de la garde, un 404 son passage, et rien ne redémarre.
//!
//! Les variables d'environnement sont globales au processus : chaque épreuve
//! tient `verrou()` pendant toute sa durée.

#![allow(clippy::await_holding_lock)]

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

static VERROU: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn verrou() -> std::sync::MutexGuard<'static, ()> {
    VERROU
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

const PORTEE: &str = "/api/v1/auth/admin-scope";
const REDEMARRER: &str = "/api/v1/appliance/reboot";
const ACTIVATION: &str = "/api/v1/auth/activation";
const SECRET_JWT: &str = "test-jwt-secret";
const HOTE_LAN: &str = "192.168.1.10:8888";

const LAN: &str = "192.168.1.50:50000";
const CETTE_MACHINE: &str = "127.0.0.1:50000";
const PASSERELLE_DOCKER: &str = "172.17.0.1:50000";

/// Les adresses du réseau local acceptées.
const RESEAU_LOCAL: &[&str] = &[
    LAN,
    "10.0.0.7:50000",
    "172.20.3.4:50000",
    "169.254.10.20:50000",
    "[fd12:3456:789a::5]:50000",
    "[fe80::1234]:50000",
    "[::ffff:192.168.1.60]:50000",
];

/// Les adresses hors réseau local refusées.
const HORS_RESEAU_LOCAL: &[&str] = &[
    "203.0.113.5:50000",
    "8.8.8.8:50000",
    "100.101.102.103:50000",
    "[2001:db8::5]:50000",
    "[2a01:e0a::1]:50000",
    "[::ffff:203.0.113.9]:50000",
];

/// Machine ordinaire (ni appliance, ni conteneur, ni mandataire déclaré).
struct Environnement;

impl Environnement {
    fn machine_ordinaire() -> Self {
        Self::poser(false, false, None, false)
    }

    fn poser(
        appliance: bool,
        relais_80: bool,
        passerelle: Option<&str>,
        admin_conteneur: bool,
    ) -> Self {
        unsafe {
            if appliance {
                std::env::set_var("TUNE_APPLIANCE", "1");
            } else {
                std::env::remove_var("TUNE_APPLIANCE");
            }
            std::env::set_var("TUNE_WEB80_RELAIS", if relais_80 { "1" } else { "0" });
            std::env::remove_var("TUNE_ALLOWED_ORIGINS");
            std::env::remove_var("TUNE_ALLOWED_HOSTS");
            std::env::remove_var("TUNE_FIRST_SETUP_IN_CONTAINER");
            match passerelle {
                Some(p) => {
                    std::env::set_var("TUNE_TEST_PASSERELLE_CONTENEUR", p);
                    std::env::remove_var("TUNE_TEST_HORS_CONTENEUR");
                }
                None => {
                    std::env::remove_var("TUNE_TEST_PASSERELLE_CONTENEUR");
                    std::env::set_var("TUNE_TEST_HORS_CONTENEUR", "1");
                }
            }
            if admin_conteneur {
                std::env::set_var("TUNE_ADMIN_LAN_IN_CONTAINER", "1");
            } else {
                std::env::remove_var("TUNE_ADMIN_LAN_IN_CONTAINER");
            }
        }
        Environnement
    }
}

impl Drop for Environnement {
    fn drop(&mut self) {
        unsafe {
            for v in [
                "TUNE_APPLIANCE",
                "TUNE_WEB80_RELAIS",
                "TUNE_ALLOWED_ORIGINS",
                "TUNE_ALLOWED_HOSTS",
                "TUNE_FIRST_SETUP_IN_CONTAINER",
                "TUNE_TEST_PASSERELLE_CONTENEUR",
                "TUNE_TEST_HORS_CONTENEUR",
                "TUNE_ADMIN_LAN_IN_CONTAINER",
            ] {
                std::env::remove_var(v);
            }
        }
    }
}

fn serveur() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn reglages(state: &AppState) -> SettingsRepo {
    SettingsRepo::with_backend(state.backend.clone())
}

fn activer_l_authentification(state: &AppState) {
    let s = reglages(state);
    s.set("auth_enabled", "true").unwrap();
    s.set("jwt_secret", SECRET_JWT).unwrap();
}

async fn appel(
    state: &AppState,
    methode: &str,
    pair: &str,
    chemin: &str,
    entetes: &[(&str, &str)],
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let pair: std::net::SocketAddr = pair.parse().unwrap();
    let app = tune_server::routes::router(state.clone())
        .layer(axum::extract::connect_info::MockConnectInfo(pair));
    let mut req = Request::builder().method(methode).uri(chemin);
    for (nom, valeur) in entetes {
        req = req.header(*nom, *valeur);
    }
    let corps = match corps {
        Some(c) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.oneshot(req.body(corps).unwrap()).await.unwrap();
    let status = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&octets).unwrap_or(json!(null)),
    )
}

/// `POST /auth/admin-scope` (éteindre le réglage strict : sans effet ici).
async fn sonde(state: &AppState, pair: &str, entetes: &[(&str, &str)]) -> (StatusCode, Value) {
    appel(
        state,
        "POST",
        pair,
        PORTEE,
        entetes,
        Some(json!({ "this_machine_only": false })),
    )
    .await
}

/// `POST /appliance/reboot` hors appliance : 404 après la garde.
async fn redemarrer(state: &AppState, pair: &str, entetes: &[(&str, &str)]) -> StatusCode {
    appel(state, "POST", pair, REDEMARRER, entetes, None)
        .await
        .0
}

fn refus(status: StatusCode, corps: &Value, motif: &str, contexte: &str) {
    assert_eq!(status, StatusCode::FORBIDDEN, "{contexte} — {corps}");
    assert_eq!(
        corps["error"],
        json!("auth_required"),
        "{contexte} — {corps}"
    );
    assert_eq!(corps["reason"], json!(motif), "{contexte} — {corps}");
}

/// 1. Le réseau local, sans authentification, passe la garde.
#[tokio::test]
async fn reseau_local_sans_authentification_l_action_d_administration_passe() {
    let _v = verrou();
    let _env = Environnement::machine_ordinaire();
    let state = serveur();
    for pair in RESEAU_LOCAL {
        let (status, corps) = sonde(&state, pair, &[("host", HOTE_LAN)]).await;
        assert_eq!(status, StatusCode::OK, "{pair} — {corps}");
        assert_eq!(
            redemarrer(&state, pair, &[("host", HOTE_LAN)]).await,
            StatusCode::NOT_FOUND,
            "{pair} : la garde doit laisser passer (404 hors appliance)"
        );
    }
    // Le client web servi par le serveur, joint par son nom mDNS.
    let (status, corps) = sonde(
        &state,
        LAN,
        &[
            ("host", "tune.local:8888"),
            ("origin", "http://tune.local:8888"),
            ("sec-fetch-site", "same-origin"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "client web en mDNS — {corps}");
    // L'écran le sait : les boutons ne sont pas grisés.
    let (_, etat) = appel(&state, "GET", LAN, ACTIVATION, &[("host", HOTE_LAN)], None).await;
    assert_eq!(etat["machine_actions_allowed"], json!(true), "{etat}");
    assert_eq!(etat["admin_this_machine_only"], json!(false), "{etat}");
    assert_eq!(etat["machine_actions_refusal"], json!(null), "{etat}");
    assert_eq!(etat["from_this_machine"], json!(false), "{etat}");
}

/// 2. Une adresse publique (IPv4, IPv6, IPv4 dans IPv6) ou CGNAT : refusée.
#[tokio::test]
async fn adresse_publique_refusee() {
    let _v = verrou();
    let _env = Environnement::machine_ordinaire();
    let state = serveur();
    for pair in HORS_RESEAU_LOCAL {
        let (status, corps) = sonde(&state, pair, &[("host", HOTE_LAN)]).await;
        refus(status, &corps, "not_local_network", pair);
        assert_eq!(
            corps["message_key"],
            json!("api.machine.reservedOutsideLanAdmin"),
            "{corps}"
        );
        assert_eq!(
            redemarrer(&state, pair, &[("host", HOTE_LAN)]).await,
            StatusCode::FORBIDDEN,
            "{pair}"
        );
        let (_, etat) = appel(&state, "GET", pair, ACTIVATION, &[("host", HOTE_LAN)], None).await;
        assert_eq!(
            etat["machine_actions_allowed"],
            json!(false),
            "{pair} — {etat}"
        );
        assert_eq!(
            etat["machine_actions_refusal"],
            json!("not_local_network"),
            "{pair} — {etat}"
        );
    }
    // Phrase traduite, qui nomme l'action.
    let (_, corps) = appel(
        &state,
        "POST",
        "203.0.113.5:50000",
        REDEMARRER,
        &[("host", HOTE_LAN), ("accept-language", "fr-FR,fr;q=0.9")],
        None,
    )
    .await;
    let message = corps["message"].as_str().unwrap_or_default();
    assert!(
        message.starts_with("Redémarrer") && message.contains("réseau local"),
        "phrase française attendue : {message}"
    );
}

/// 3. Une requête rejouée par le relais distant ou l'écoute directe n'est
/// jamais locale, d'où qu'elle paraisse venir.
#[tokio::test]
async fn relais_distant_refuse() {
    let _v = verrou();
    let _env = Environnement::machine_ordinaire();
    let state = serveur();
    for pair in [CETTE_MACHINE, LAN, "[::1]:50000"] {
        for hote in ["localhost:8888", HOTE_LAN] {
            let entetes = [("host", hote), (tune_core::cloud::ENTETE_RELAIS, "1")];
            let (status, corps) = sonde(&state, pair, &entetes).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "relais {pair} {hote} — {corps}"
            );
            assert_eq!(
                redemarrer(&state, pair, &entetes).await,
                StatusCode::FORBIDDEN,
                "relais {pair} {hote}"
            );
        }
    }
}

/// 4. Conteneur : en réseau en pont, tout arrive de la passerelle, Internet
/// compris. Refusé par défaut, même depuis une adresse privée ; avec
/// `TUNE_ADMIN_LAN_IN_CONTAINER=1` (réseau de l'hôte), le réseau local passe,
/// la passerelle jamais. L'autorisation de la première configuration n'ouvre
/// pas l'administration.
#[tokio::test]
async fn conteneur_en_pont_refuse_par_defaut() {
    let _v = verrou();
    let state = serveur();
    {
        let _env = Environnement::poser(false, false, Some("172.17.0.1"), false);
        for pair in [PASSERELLE_DOCKER, LAN, "172.17.0.5:50000"] {
            let (status, corps) = sonde(&state, pair, &[("host", HOTE_LAN)]).await;
            refus(status, &corps, "container_network", pair);
            assert_eq!(
                corps["message_key"],
                json!("api.machine.reservedContainerNetwork"),
                "{corps}"
            );
            assert_eq!(
                redemarrer(&state, pair, &[("host", HOTE_LAN)]).await,
                StatusCode::FORBIDDEN
            );
            let (_, etat) =
                appel(&state, "GET", pair, ACTIVATION, &[("host", HOTE_LAN)], None).await;
            assert_eq!(
                etat["machine_actions_refusal"],
                json!("container_network"),
                "{pair} — {etat}"
            );
        }
        unsafe { std::env::set_var("TUNE_FIRST_SETUP_IN_CONTAINER", "1") };
        let (status, corps) = sonde(&state, LAN, &[("host", HOTE_LAN)]).await;
        refus(
            status,
            &corps,
            "container_network",
            "première configuration seule",
        );
    }
    {
        // Réseau de l'hôte déclaré : l'adresse réelle est vue.
        let _env = Environnement::poser(false, false, Some("192.168.1.1,fe80::1"), true);
        let (status, corps) = sonde(&state, LAN, &[("host", HOTE_LAN)]).await;
        assert_eq!(status, StatusCode::OK, "réseau de l'hôte, LAN — {corps}");
        for pair in [
            "192.168.1.1:50000",
            "[fe80::1]:50000",
            "[::ffff:192.168.1.1]:50000",
        ] {
            let (status, corps) = sonde(&state, pair, &[("host", HOTE_LAN)]).await;
            refus(status, &corps, "container_network", pair);
        }
        // Un nom à un seul label donné par un tiers (réseau de conteneurs).
        let (status, corps) = sonde(&state, LAN, &[("host", "tune:8888")]).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "Host tune:8888 — {corps}");
        // Internet reste refusé.
        let (status, corps) = sonde(&state, "203.0.113.5:50000", &[("host", HOTE_LAN)]).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{corps}");
    }
    {
        // Passerelle illisible : dans le doute, tout est passerelle.
        let _env = Environnement::poser(false, false, Some(""), true);
        let (status, corps) = sonde(&state, LAN, &[("host", HOTE_LAN)]).await;
        refus(status, &corps, "container_network", "passerelle illisible");
    }
}

/// 5. Tune OS dont le relais du port 80 rouvre les connexions depuis
/// 127.0.0.1 : rien ne change, ni depuis la machine, ni depuis le réseau
/// local (l'adresse cliente réelle n'est pas connue de façon fiable).
#[tokio::test]
async fn tune_os_avec_relais_du_port_80_inchange() {
    let _v = verrou();
    let _env = Environnement::poser(true, true, None, false);
    assert!(!tune_server::routes::appliance::bouclage_fiable());
    let state = serveur();
    for pair in [CETTE_MACHINE, LAN, "[fd12:3456:789a::5]:50000"] {
        for hote in ["tune.local", HOTE_LAN, "localhost:8888"] {
            let (status, corps) = sonde(&state, pair, &[("host", hote)]).await;
            refus(
                status,
                &corps,
                "port80_relay",
                &format!("Tune OS {pair} {hote}"),
            );
        }
        let (_, etat) = appel(&state, "GET", pair, ACTIVATION, &[("host", HOTE_LAN)], None).await;
        assert_eq!(
            etat["machine_actions_allowed"],
            json!(false),
            "{pair} — {etat}"
        );
        assert_eq!(
            etat["machine_actions_refusal"],
            json!("port80_relay"),
            "{pair} — {etat}"
        );
    }
}

/// 6. Réglage strict : seule la machine passe ; seule elle (ou
/// l'administrateur) l'éteint ; l'écriture générique des réglages ne le
/// touche pas.
#[tokio::test]
async fn reglage_strict_seule_la_machine() {
    let _v = verrou();
    let _env = Environnement::machine_ordinaire();
    let state = serveur();
    // L'allumer depuis le réseau local : permis (on se restreint).
    let (status, corps) = appel(
        &state,
        "POST",
        LAN,
        PORTEE,
        &[("host", HOTE_LAN)],
        Some(json!({ "this_machine_only": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_eq!(corps["admin_this_machine_only"], json!(true), "{corps}");
    assert!(tune_server::auth::administration_cette_machine_seulement(
        &state
    ));

    for pair in RESEAU_LOCAL {
        let (status, corps) = sonde(&state, pair, &[("host", HOTE_LAN)]).await;
        refus(status, &corps, "not_local", pair);
        assert_eq!(
            corps["message_key"],
            json!("api.machine.reservedNotLocal"),
            "{corps}"
        );
        assert_eq!(
            redemarrer(&state, pair, &[("host", HOTE_LAN)]).await,
            StatusCode::FORBIDDEN
        );
    }
    let (_, etat) = appel(&state, "GET", LAN, ACTIVATION, &[("host", HOTE_LAN)], None).await;
    assert_eq!(etat["machine_actions_allowed"], json!(false), "{etat}");
    assert_eq!(etat["admin_this_machine_only"], json!(true), "{etat}");
    assert_eq!(
        etat["machine_actions_refusal"],
        json!("not_local"),
        "{etat}"
    );

    // Le réglage brut, par l'écriture générique : refusé (clé protégée).
    let (status, corps) = appel(
        &state,
        "PATCH",
        LAN,
        "/api/v1/system/config",
        &[("host", HOTE_LAN)],
        Some(json!({ "auth_admin_this_machine_only": "false" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{corps}");
    assert!(tune_server::auth::administration_cette_machine_seulement(
        &state
    ));

    // Cette machine passe, et seule elle l'éteint.
    let ici = [("host", "localhost:8888")];
    assert_eq!(
        redemarrer(&state, CETTE_MACHINE, &ici).await,
        StatusCode::NOT_FOUND
    );
    let (status, corps) = sonde(&state, CETTE_MACHINE, &ici).await;
    assert_eq!(status, StatusCode::OK, "{corps}");
    assert!(!tune_server::auth::administration_cette_machine_seulement(
        &state
    ));
    let (status, corps) = sonde(&state, LAN, &[("host", HOTE_LAN)]).await;
    assert_eq!(status, StatusCode::OK, "réglage éteint, LAN — {corps}");
}

/// 7. Les en-têtes de mandataire ne sont jamais crus : une adresse publique
/// qui se dit privée reste refusée, et un appel du réseau local qui porte
/// ces en-têtes (un mandataire du LAN qui relaie Internet) aussi.
#[tokio::test]
async fn en_tetes_de_mandataire_jamais_crus() {
    let _v = verrou();
    let _env = Environnement::machine_ordinaire();
    let state = serveur();
    let mandataires: &[(&str, &str)] = &[
        ("x-forwarded-for", "192.168.1.20"),
        ("forwarded", "for=192.168.1.20"),
        ("x-real-ip", "192.168.1.20"),
        ("x-forwarded-for", "127.0.0.1"),
    ];
    for (nom, valeur) in mandataires {
        for pair in ["203.0.113.5:50000", LAN, CETTE_MACHINE] {
            let hote = if pair == CETTE_MACHINE {
                "localhost:8888"
            } else {
                HOTE_LAN
            };
            let (status, corps) = sonde(&state, pair, &[("host", hote), (nom, valeur)]).await;
            assert_eq!(
                status,
                StatusCode::FORBIDDEN,
                "{pair} {nom}: {valeur} — {corps}"
            );
        }
    }
    // Mandataire déclaré (`TUNE_ALLOWED_HOSTS`) : plus aucune adresse n'est
    // une preuve.
    unsafe { std::env::set_var("TUNE_ALLOWED_HOSTS", "tune.example.org") };
    let (status, corps) = sonde(&state, LAN, &[("host", "tune.example.org")]).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "mandataire déclaré — {corps}"
    );
    // HTTP/1.0, l'empreinte de nginx par défaut.
    unsafe { std::env::remove_var("TUNE_ALLOWED_HOSTS") };
    let pair: std::net::SocketAddr = LAN.parse().unwrap();
    let app = tune_server::routes::router(state.clone())
        .layer(axum::extract::connect_info::MockConnectInfo(pair));
    let resp = app
        .oneshot(
            Request::post(PORTEE)
                .version(axum::http::Version::HTTP_10)
                .header("host", HOTE_LAN)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"this_machine_only":false}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "HTTP/1.0");
}

/// 8. Authentification active : rien ne change. Sans jeton, 401 de partout ;
/// un compte standard, 403 ; l'administrateur, de partout.
#[tokio::test]
async fn authentification_active_inchangee() {
    let _v = verrou();
    let _env = Environnement::machine_ordinaire();
    let state = serveur();
    activer_l_authentification(&state);
    let admin = format!(
        "Bearer {}",
        tune_server::auth::sign_jwt(1, "admin", SECRET_JWT).unwrap()
    );
    let utilisateur = format!(
        "Bearer {}",
        tune_server::auth::sign_jwt(2, "user", SECRET_JWT).unwrap()
    );
    for pair in [LAN, CETTE_MACHINE, "203.0.113.5:50000"] {
        let hote = if pair == CETTE_MACHINE {
            "localhost:8888"
        } else {
            HOTE_LAN
        };
        let (status, corps) = sonde(&state, pair, &[("host", hote)]).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "sans jeton {pair} — {corps}"
        );
        let (status, corps) = sonde(
            &state,
            pair,
            &[
                ("host", hote),
                (header::AUTHORIZATION.as_str(), &utilisateur),
            ],
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "compte standard {pair} — {corps}"
        );
        let (status, corps) = sonde(
            &state,
            pair,
            &[("host", hote), (header::AUTHORIZATION.as_str(), &admin)],
        )
        .await;
        assert_eq!(status, StatusCode::OK, "administrateur {pair} — {corps}");
    }
    let (_, etat) = appel(&state, "GET", LAN, ACTIVATION, &[("host", HOTE_LAN)], None).await;
    assert_eq!(etat["machine_actions_allowed"], json!(null), "{etat}");
}

/// 9. Une page d'un autre site (CSRF) ou un nom d'hôte étranger (DNS
/// rebinding) depuis un navigateur du réseau local : refusé.
#[tokio::test]
async fn autre_site_ou_hote_etranger_refuse() {
    let _v = verrou();
    let _env = Environnement::machine_ordinaire();
    let state = serveur();
    let (status, corps) = sonde(
        &state,
        LAN,
        &[
            ("host", HOTE_LAN),
            ("origin", "https://site-tiers.example"),
            ("sec-fetch-site", "cross-site"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "autre site — {corps}");
    let (status, corps) = sonde(&state, LAN, &[("host", "attaquant.example:8888")]).await;
    // La garde d'hôte répond 421 avant même la garde d'administration.
    assert!(
        matches!(
            status,
            StatusCode::FORBIDDEN | StatusCode::MISDIRECTED_REQUEST
        ),
        "hôte étranger — {status} {corps}"
    );
}

/// 10. Délier le compte mozaiklabs.fr reste à cette machine (décision du
/// 23/11), même quand le réseau local administre.
#[tokio::test]
async fn deliaison_du_compte_reste_a_cette_machine() {
    let _v = verrou();
    let _env = Environnement::machine_ordinaire();
    let state = serveur();
    let (status, corps) = appel(
        &state,
        "POST",
        LAN,
        "/api/v1/cloud/sso/disconnect",
        &[("host", HOTE_LAN)],
        None,
    )
    .await;
    refus(status, &corps, "not_local", "déliaison depuis le LAN");
}
