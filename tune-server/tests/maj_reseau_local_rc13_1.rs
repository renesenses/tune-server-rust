//! rc13.1 — la mise à jour depuis le réseau local, sans authentification.
//!
//! En rc13, `POST /system/update/install` était sous `AdminOuCetteMachine` :
//! sans authentification, seul un appel de la machine du serveur passait, et
//! sous Tune OS (relais du port 80 depuis 127.0.0.1) même la machine était
//! refusée. Presque personne ne pouvait se mettre à jour depuis l'interface.
//!
//! Ce que ces tests tiennent, sur les ROUTES MONTÉES :
//!
//! 1. réseau local, sans authentification : l'installation passe la garde ;
//! 2. relais distant, mandataire inverse, CGNAT/Tailscale : refusée ;
//! 3. authentification active, non administrateur : refusée ;
//! 4. redémarrage depuis le réseau local, sans authentification : refusé,
//!    avec une phrase traduite qui nomme l'action ;
//! 5. Tune OS avec relais du port 80 : l'installation passe la garde.
//!
//! « Passe la garde » se lit sans réseau ni installation : une mise à jour
//! est déjà posée en cours (`update_phase`), et le gestionnaire répond alors
//! `409 already_in_progress` — réponse qu'il ne peut rendre qu'APRÈS la garde.
//! Un refus de garde rend `403` (ou `401` sans jeton) avant le gestionnaire.
//!
//! Les variables d'environnement (TUNE_APPLIANCE, TUNE_WEB80_RELAIS) sont
//! globales au processus : chaque test tient `crate::lock_environment()`.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const INSTALLER: &str = "/api/v1/system/update/install";
const REDEMARRER_LA_MACHINE: &str = "/api/v1/appliance/reboot";
const SECRET_JWT: &str = "test-jwt-secret";

const LAN: &str = "192.168.1.50:50000";
const LAN_IPV6: &str = "[fd12:3456:789a::5]:50000";
const CETTE_MACHINE: &str = "127.0.0.1:50000";
const CGNAT_TAILSCALE: &str = "100.101.102.103:50000";

struct Environnement;

impl Environnement {
    fn poser(appliance: bool, relais_80: bool) -> Self {
        unsafe {
            if appliance {
                std::env::set_var("TUNE_APPLIANCE", "1");
            } else {
                std::env::remove_var("TUNE_APPLIANCE");
            }
            std::env::set_var("TUNE_WEB80_RELAIS", if relais_80 { "1" } else { "0" });
            std::env::remove_var("TUNE_ALLOWED_ORIGINS");
            std::env::remove_var("TUNE_ALLOWED_HOSTS");
            // Machine ordinaire, même quand l'épreuve tourne en conteneur.
            std::env::set_var("TUNE_TEST_HORS_CONTENEUR", "1");
        }
        Environnement
    }
}

impl Drop for Environnement {
    fn drop(&mut self) {
        unsafe {
            std::env::remove_var("TUNE_APPLIANCE");
            std::env::remove_var("TUNE_WEB80_RELAIS");
            std::env::remove_var("TUNE_TEST_HORS_CONTENEUR");
        }
    }
}

/// Un serveur sur lequel une mise à jour est déjà « en cours » : le
/// gestionnaire d'installation rend 409 sans rien télécharger.
fn serveur_avec_maj_en_cours() -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    *state.update_phase.lock().unwrap() = Some("downloading".into());
    state
}

fn activer_l_authentification(state: &AppState) {
    let s = SettingsRepo::with_backend(state.backend.clone());
    s.set("auth_enabled", "true").unwrap();
    s.set("jwt_secret", SECRET_JWT).unwrap();
}

async fn post(
    state: &AppState,
    pair: &str,
    chemin: &str,
    entetes: &[(&str, &str)],
) -> (StatusCode, Value) {
    let pair: std::net::SocketAddr = pair.parse().unwrap();
    let app = tune_server::routes::router(state.clone())
        .layer(axum::extract::connect_info::MockConnectInfo(pair));
    let mut req = Request::post(chemin);
    for (nom, valeur) in entetes {
        req = req.header(*nom, *valeur);
    }
    let resp = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&octets).unwrap_or(json!(null)),
    )
}

fn garde_passee(status: StatusCode, corps: &Value) -> bool {
    status == StatusCode::CONFLICT && corps["status"] == json!("already_in_progress")
}

/// 1. Le réseau local, sans authentification, installe la mise à jour
/// officielle (IPv4 privée, IPv6 unique locale, lien local).
#[tokio::test]
async fn reseau_local_sans_authentification_la_mise_a_jour_passe() {
    let _verrou = crate::lock_environment();
    let _env = Environnement::poser(false, false);
    let state = serveur_avec_maj_en_cours();
    for pair in [LAN, LAN_IPV6, "169.254.10.20:50000", "10.0.0.7:50000"] {
        let (status, corps) = post(&state, pair, INSTALLER, &[("host", "192.168.1.10:8888")]).await;
        assert!(
            garde_passee(status, &corps),
            "{pair} : la mise à jour depuis le réseau local doit passer la garde — {status} {corps}"
        );
    }
    // Le client web servi par le serveur, joint par son nom mDNS.
    let (status, corps) = post(
        &state,
        LAN,
        INSTALLER,
        &[
            ("host", "tune.local:8888"),
            ("origin", "http://tune.local:8888"),
            ("sec-fetch-site", "same-origin"),
        ],
    )
    .await;
    assert!(
        garde_passee(status, &corps),
        "client web en mDNS — {status} {corps}"
    );
}

/// 2. Relais distant, mandataire inverse, CGNAT/Tailscale : refusés, avec
/// une phrase qui dit quoi faire.
#[tokio::test]
async fn hors_du_reseau_local_la_mise_a_jour_reste_refusee() {
    let _verrou = crate::lock_environment();
    let _env = Environnement::poser(false, false);
    let state = serveur_avec_maj_en_cours();

    // Le relais cloud et l'écoute directe rejouent depuis 127.0.0.1, marqués.
    for pair in [CETTE_MACHINE, LAN] {
        let (status, corps) = post(
            &state,
            pair,
            INSTALLER,
            &[
                (tune_core::cloud::ENTETE_RELAIS, "1"),
                ("host", "127.0.0.1:8888"),
            ],
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "relais depuis {pair} — {corps}"
        );
    }

    // Mandataire inverse local qui expose le serveur sur Internet.
    let (status, corps) = post(
        &state,
        CETTE_MACHINE,
        INSTALLER,
        &[
            ("host", "127.0.0.1:8888"),
            ("x-forwarded-for", "203.0.113.9"),
        ],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "mandataire inverse — {corps}"
    );
    assert_eq!(corps["reason"], json!("not_local_network"), "{corps}");

    // Bouclage hors appliance avec un Host non local : relais TCP inconnu.
    let (status, corps) = post(
        &state,
        CETTE_MACHINE,
        INSTALLER,
        &[("host", "tune.local:8888")],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "bouclage + relais TCP — {corps}"
    );

    // CGNAT / Tailscale : accès à distance, pas le réseau local. Une adresse
    // publique non plus.
    for pair in [CGNAT_TAILSCALE, "203.0.113.9:50000"] {
        let (status, corps) = post(
            &state,
            pair,
            INSTALLER,
            &[
                ("host", "192.168.1.10:8888"),
                ("accept-language", "fr-FR,fr;q=0.9"),
            ],
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{pair} — {corps}");
        assert_eq!(corps["reason"], json!("not_local_network"), "{corps}");
        assert_eq!(corps["action"], json!("update"), "{corps}");
        assert_eq!(
            corps["message_key"],
            json!("api.machine.reservedOutsideLan"),
            "{corps}"
        );
        let message = corps["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("Installer la mise à jour") && message.contains("réseau local"),
            "phrase française attendue : {message}"
        );
    }

    // Une page d'un autre site qui viserait le serveur depuis le navigateur
    // d'un poste du réseau local : refusée par la garde d'origine.
    let (status, corps) = post(
        &state,
        LAN,
        INSTALLER,
        &[
            ("host", "192.168.1.10:8888"),
            ("origin", "https://exemple.invalid"),
            ("sec-fetch-site", "cross-site"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "page inter-sites — {corps}");
}

/// 3. Authentification active : l'administrateur seul, même depuis le
/// réseau local ou la machine.
#[tokio::test]
async fn authentification_active_un_non_administrateur_est_refuse() {
    let _verrou = crate::lock_environment();
    let _env = Environnement::poser(false, false);
    let state = serveur_avec_maj_en_cours();
    activer_l_authentification(&state);
    let utilisateur = tune_server::auth::sign_jwt(2, "user", SECRET_JWT).unwrap();
    let porteur = format!("Bearer {utilisateur}");
    for pair in [LAN, CETTE_MACHINE] {
        let (status, corps) = post(
            &state,
            pair,
            INSTALLER,
            &[
                ("host", "192.168.1.10:8888"),
                (header::AUTHORIZATION.as_str(), &porteur),
            ],
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "utilisateur depuis {pair} — {corps}"
        );
        let (status, corps) = post(&state, pair, INSTALLER, &[("host", "192.168.1.10:8888")]).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "sans jeton depuis {pair} — {corps}"
        );
    }
    // L'administrateur passe, comme avant.
    let admin = format!(
        "Bearer {}",
        tune_server::auth::sign_jwt(1, "admin", SECRET_JWT).unwrap()
    );
    let (status, corps) = post(
        &state,
        LAN,
        INSTALLER,
        &[
            ("host", "192.168.1.10:8888"),
            (header::AUTHORIZATION.as_str(), &admin),
        ],
    )
    .await;
    assert!(
        garde_passee(status, &corps),
        "administrateur — {status} {corps}"
    );
}

/// 4. rc14 : les autres actions de machine s'ouvrent aussi au réseau local
/// (voir `administration_reseau_local_rc14.rs`), sauf quand le réglage
/// « Administration depuis cette machine seulement » est allumé : elles sont
/// alors refusées, avec une phrase traduite qui nomme l'action et dit
/// comment faire. La mise à jour, elle, reste ouverte au réseau local.
///
/// Hors appliance, `/appliance/reboot` rend 404 APRÈS la garde : un 403 prouve
/// donc que la garde a refusé, et rien ne redémarre.
#[tokio::test]
async fn redemarrage_depuis_le_reseau_local_reste_refuse() {
    let _verrou = crate::lock_environment();
    let _env = Environnement::poser(false, false);
    let state = serveur_avec_maj_en_cours();
    // Réglage strict éteint (par défaut) : la garde laisse passer (404).
    let (status, corps) = post(
        &state,
        LAN,
        REDEMARRER_LA_MACHINE,
        &[("host", "192.168.1.10:8888")],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "rc14, réseau local — {corps}"
    );
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            tune_server::auth::CLE_ADMINISTRATION_CETTE_MACHINE_SEULEMENT,
            "true",
        )
        .unwrap();
    let (status, corps) = post(&state, LAN, INSTALLER, &[("host", "192.168.1.10:8888")]).await;
    assert!(
        garde_passee(status, &corps),
        "réglage strict : la mise à jour reste ouverte au réseau local — {status} {corps}"
    );
    let (status, corps) = post(
        &state,
        LAN,
        REDEMARRER_LA_MACHINE,
        &[
            ("host", "192.168.1.10:8888"),
            ("accept-language", "fr-FR,fr;q=0.9"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{corps}");
    assert_eq!(corps["error"], json!("auth_required"), "{corps}");
    assert_eq!(corps["reason"], json!("not_local"), "{corps}");
    assert_eq!(corps["action"], json!("restart"), "{corps}");
    assert_eq!(
        corps["message_key"],
        json!("api.machine.reservedNotLocal"),
        "{corps}"
    );
    let message = corps["message"].as_str().unwrap_or_default();
    assert!(
        message.starts_with("Redémarrer") && message.contains("authentification"),
        "phrase française attendue : {message}"
    );
    // En anglais par défaut.
    let (_, corps) = post(
        &state,
        LAN,
        REDEMARRER_LA_MACHINE,
        &[("host", "192.168.1.10:8888")],
    )
    .await;
    let message = corps["message"].as_str().unwrap_or_default();
    assert!(
        message.starts_with("Restart"),
        "phrase anglaise attendue : {message}"
    );

    // Le canal de mise à jour aussi, sous le réglage strict.
    let pair: std::net::SocketAddr = LAN.parse().unwrap();
    let app = tune_server::routes::router(state.clone())
        .layer(axum::extract::connect_info::MockConnectInfo(pair));
    let resp = app
        .oneshot(
            Request::put("/api/v1/system/update/channel")
                .header("host", "192.168.1.10:8888")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"channel":"beta"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "canal de mise à jour depuis le LAN"
    );
}

/// 5. Tune OS dont le relais du port 80 rouvre les connexions du réseau
/// local depuis 127.0.0.1 : la mise à jour passe ; les actions de machine
/// restent refusées, avec la phrase propre à ce relais.
#[tokio::test]
async fn tune_os_avec_relais_du_port_80_la_mise_a_jour_passe() {
    let _verrou = crate::lock_environment();
    let _env = Environnement::poser(true, true);
    assert!(!tune_server::routes::appliance::bouclage_fiable());
    let state = serveur_avec_maj_en_cours();
    for hote in ["tune.local", "192.168.1.10", "127.0.0.1:8888"] {
        let (status, corps) = post(&state, CETTE_MACHINE, INSTALLER, &[("host", hote)]).await;
        assert!(
            garde_passee(status, &corps),
            "Tune OS, relais du port 80, Host {hote} — {status} {corps}"
        );
    }
    // Image corrigée (nftables) : l'adresse du réseau local est conservée.
    let (status, corps) = post(&state, LAN, INSTALLER, &[("host", "tune.local")]).await;
    assert!(
        garde_passee(status, &corps),
        "Tune OS, LAN direct — {status} {corps}"
    );

    // Le relais cloud reste refusé, même sur l'appliance.
    let (status, corps) = post(
        &state,
        CETTE_MACHINE,
        INSTALLER,
        &[(tune_core::cloud::ENTETE_RELAIS, "1")],
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "relais cloud sur Tune OS — {corps}"
    );

    // Action de machine sous le relais du port 80 : refusée AVANT le
    // gestionnaire (rien ne s'installe), phrase propre au relais.
    let (status, corps) = post(
        &state,
        CETTE_MACHINE,
        "/api/v1/appliance/install-to-disk",
        &[
            ("host", "tune.local"),
            ("accept-language", "de-DE,de;q=0.9"),
        ],
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{corps}");
    assert_eq!(corps["reason"], json!("port80_relay"), "{corps}");
    assert_eq!(
        corps["message_key"],
        json!("api.machine.reservedPort80Relay"),
        "{corps}"
    );
    let message = corps["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("Tune-OS") && message.contains("Administrator"),
        "phrase allemande attendue : {message}"
    );
}

/// Les phrases du refus existent dans les seize langues, et chacune garde
/// le paramètre `{action}`.
#[test]
fn les_phrases_du_refus_existent_dans_les_seize_langues() {
    let catalogue: Value = serde_json::from_str(include_str!("../src/i18n_server.json")).unwrap();
    let langues = [
        "fr", "en", "de", "es", "it", "zh", "ja", "ko", "ro", "sv", "hu", "vi", "nb", "pl", "el",
        "tr",
    ];
    let phrases = [
        "api.machine.reservedNotLocal",
        "api.machine.reservedPort80Relay",
        "api.machine.reservedOutsideLan",
    ];
    let actions = [
        "update",
        "updateChannel",
        "restart",
        "shutdown",
        "backup",
        "support",
        "remote",
        "extensions",
        "other",
    ];
    for cle in phrases {
        for l in langues {
            let t = catalogue[cle][l].as_str().unwrap_or_default();
            assert!(t.contains("{action}"), "{cle}/{l} sans {{action}} : {t:?}");
        }
    }
    for a in actions {
        let cle = format!("api.machine.action.{a}");
        for l in langues {
            let t = catalogue[&cle][l].as_str().unwrap_or_default();
            assert!(!t.trim().is_empty(), "{cle}/{l} absente");
        }
    }
}
