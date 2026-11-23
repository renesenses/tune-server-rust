//! rc14 — relier le compte mozaiklabs.fr depuis le réseau local, sans
//! authentification, même règle que la mise à jour en rc13.1
//! (`auth::AdminOuReseauLocalPourLaMiseAJour`).
//!
//! Ce que ces tests tiennent, sur le VRAI routeur (`crate::routes::router`) :
//!
//! 1. réseau local, sans authentification : `GET /cloud/sso/authorize` lance la
//!    liaison, le retour passe la garde (le contrôle du `state` tranche ensuite),
//!    et `GET /cloud/sso/status` montre le compte relié (le menu du compte et
//!    sa photo en dépendent) ;
//! 2. relais distant, mandataire inverse, CGNAT/Tailscale, adresse publique :
//!    refusés, avec une phrase traduite qui nomme l'action, et aucune liaison
//!    n'est préparée ; le statut n'y montre toujours que « relié » ;
//! 3. authentification active, sans jeton : refusé, depuis le réseau local
//!    comme depuis la machine ;
//! 4. décisions du 23/11 :
//!    - depuis le réseau local, la liaison n'est acceptée que si AUCUN compte
//!      n'est relié ; cette machine remplace toujours le compte. Le contrôle
//!      est refait au retour, avant et après l'échange avec mozaiklabs.fr ;
//!    - la déliaison (`POST /cloud/sso/disconnect`) passe sous la même garde :
//!      réseau local accepté, relais et mandataire refusés, authentification
//!      active sans jeton refusée.

use std::net::SocketAddr;

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

const AUTORISER: &str = "/api/v1/cloud/sso/authorize";
const STATUT: &str = "/api/v1/cloud/sso/status";
const ICI: &str = "127.0.0.1:52000";
const COMPTE: &str = r#"{"id":1,"email":"proprio@exemple.test","display_name":"P","is_admin":false,"avatar_url":null}"#;

struct Banc {
    state: AppState,
    _dossier: tempfile::TempDir,
}

fn banc() -> Banc {
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");
    let state = AppState::new(base.to_str().unwrap(), 0, Default::default()).unwrap();
    // Aucun réseau : tout appel sortant vers mozaiklabs échoue tout de suite.
    SettingsRepo::with_backend(state.backend.clone())
        .set("mozaik_base_url", "http://127.0.0.1:9")
        .unwrap();
    Banc {
        state,
        _dossier: dossier,
    }
}

fn reglages(state: &AppState) -> SettingsRepo {
    SettingsRepo::with_backend(state.backend.clone())
}

fn attente_du_serveur(state: &AppState) -> String {
    reglages(state)
        .get("mozaik_pkce_pending")
        .unwrap()
        .unwrap_or_default()
}

async fn get(
    state: &AppState,
    depuis: &str,
    uri: &str,
    entetes: &[(&str, &str)],
) -> (StatusCode, String) {
    let adresse: SocketAddr = depuis.parse().unwrap();
    let mut requete = Request::get(uri).extension(ConnectInfo(adresse));
    for (nom, valeur) in entetes {
        requete = requete.header(*nom, *valeur);
    }
    let reponse = crate::routes::router(state.clone())
        .oneshot(requete.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = to_bytes(reponse.into_body(), usize::MAX).await.unwrap();
    (statut, String::from_utf8_lossy(&octets).into_owned())
}

async fn post(
    state: &AppState,
    depuis: &str,
    uri: &str,
    entetes: &[(&str, &str)],
) -> (StatusCode, String) {
    let adresse: SocketAddr = depuis.parse().unwrap();
    let mut requete = Request::post(uri).extension(ConnectInfo(adresse));
    for (nom, valeur) in entetes {
        requete = requete.header(*nom, *valeur);
    }
    let reponse = crate::routes::router(state.clone())
        .oneshot(requete.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = to_bytes(reponse.into_body(), usize::MAX).await.unwrap();
    (statut, String::from_utf8_lossy(&octets).into_owned())
}

/// Le compte déjà relié au serveur (jeton et identité).
fn relier_le_proprietaire(state: &AppState) {
    let r = reglages(state);
    r.set("mozaik_access_token", "jeton-du-proprietaire")
        .unwrap();
    r.set("mozaik_user", COMPTE).unwrap();
}

fn courriel_relie(state: &AppState) -> String {
    let compte = reglages(state)
        .get("mozaik_user")
        .unwrap()
        .unwrap_or_default();
    json_de(&compte)["email"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn json_de(texte: &str) -> Value {
    serde_json::from_str(texte).unwrap_or(Value::Null)
}

/// 1. Le réseau local relie le compte : IPv4 privées, IPv6 unique locale,
/// lien local ; et le client web servi par le serveur, joint par son nom mDNS.
#[tokio::test]
async fn reseau_local_sans_authentification_la_liaison_passe() {
    for depuis in [
        "192.168.1.50:52000",
        "10.0.0.7:52000",
        "172.20.1.4:52000",
        "169.254.10.20:52000",
        "[fd12:3456:789a::5]:52000",
        "[fe80::1]:52000",
    ] {
        let b = banc();
        let (statut, page) = get(
            &b.state,
            depuis,
            AUTORISER,
            &[("host", "192.168.1.10:8888")],
        )
        .await;
        assert_eq!(
            statut,
            StatusCode::TEMPORARY_REDIRECT,
            "{depuis} : la liaison depuis le réseau local doit passer la garde — {page}"
        );
        assert!(
            !attente_du_serveur(&b.state).is_empty(),
            "{depuis} : l'autorisation est préparée"
        );
    }

    let b = banc();
    let (statut, page) = get(
        &b.state,
        "192.168.1.50:52000",
        AUTORISER,
        &[
            ("host", "tune.local:8888"),
            ("sec-fetch-site", "same-origin"),
        ],
    )
    .await;
    assert_eq!(statut, StatusCode::TEMPORARY_REDIRECT, "mDNS — {page}");

    // Le retour de mozaiklabs.fr (navigation inter-sites) passe la garde : le
    // contrôle du `state` tranche ensuite (400, pas 403), et rien ne se relie.
    let (statut, page) = get(
        &b.state,
        "192.168.1.50:52000",
        "/api/v1/cloud/sso/callback?code=c&state=faux",
        &[
            ("host", "tune.local:8888"),
            ("sec-fetch-site", "cross-site"),
        ],
    )
    .await;
    assert_eq!(
        statut,
        StatusCode::BAD_REQUEST,
        "retour depuis le LAN — {page}"
    );
    assert!(
        reglages(&b.state)
            .get("mozaik_access_token")
            .unwrap()
            .is_none()
    );
}

/// 1 ter. Un retour au `state` faux n'annule pas la liaison en cours :
/// l'attente PKCE n'est consommée qu'après un `state` exact.
#[tokio::test]
async fn un_state_faux_n_annule_pas_la_liaison_en_cours() {
    let b = banc();
    let depuis = "192.168.1.50:52000";
    let hote = [("host", "192.168.1.10:8888")];
    let (statut, page) = get(&b.state, depuis, AUTORISER, &hote).await;
    assert_eq!(statut, StatusCode::TEMPORARY_REDIRECT, "{page}");
    let attente = attente_du_serveur(&b.state);
    let etat = json_de(&attente)["state"].as_str().unwrap().to_string();

    let (statut, page) = get(
        &b.state,
        depuis,
        "/api/v1/cloud/sso/callback?code=c&state=faux",
        &hote,
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "state faux — {page}");
    assert_eq!(
        attente_du_serveur(&b.state),
        attente,
        "un state faux ne consomme pas l'attente"
    );
    assert!(
        reglages(&b.state)
            .get("mozaik_pkce_pending_at")
            .unwrap()
            .is_some(),
        "la date de l'attente reste"
    );

    // Le bon `state` passe le contrôle (l'échange échoue ensuite, aucun
    // réseau) et consomme l'attente, à usage unique.
    let (statut, page) = get(
        &b.state,
        depuis,
        &format!("/api/v1/cloud/sso/callback?code=c&state={etat}"),
        &hote,
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_GATEWAY, "bon state — {page}");
    assert!(attente_du_serveur(&b.state).is_empty(), "attente consommée");
}

/// 1 quater. Depuis le réseau local, un `Host` que le serveur n'accepte pas
/// (nom étranger) est refusé : rien n'est préparé. La garde d'origine répond
/// 421 avant la route ; `appel_depuis_le_reseau_local` refuse aussi ce `Host`.
#[tokio::test]
async fn reseau_local_avec_un_host_non_accepte_la_liaison_est_refusee() {
    let b = banc();
    let (statut, page) = get(
        &b.state,
        "192.168.1.50:52000",
        AUTORISER,
        &[("host", "evil.example")],
    )
    .await;
    assert_eq!(
        statut,
        StatusCode::MISDIRECTED_REQUEST,
        "host: evil.example — {page}"
    );
    assert!(attente_du_serveur(&b.state).is_empty(), "rien de préparé");
}

/// 1 bis. Le statut montre le compte relié au réseau local : le menu du compte
/// (nom, courriel, photo de profil) n'est plus « relié, sans détails ».
#[tokio::test]
async fn reseau_local_sans_authentification_le_statut_montre_le_compte() {
    let b = banc();
    let r = reglages(&b.state);
    r.set("mozaik_access_token", "jeton-du-proprietaire")
        .unwrap();
    r.set("mozaik_user", COMPTE).unwrap();
    let (_, texte) = get(
        &b.state,
        "192.168.1.50:52000",
        STATUT,
        &[("host", "192.168.1.10:8888")],
    )
    .await;
    let v = json_de(&texte);
    assert_eq!(v["connected"], true, "{texte}");
    assert_eq!(v["details_hidden"], false, "{texte}");
    assert_eq!(v["user"]["email"], "proprio@exemple.test", "{texte}");
}

/// 2. Hors du réseau local : refusé, phrase traduite, aucune liaison préparée.
#[tokio::test]
async fn hors_du_reseau_local_la_liaison_reste_refusee() {
    // CGNAT / Tailscale et adresse publique.
    for depuis in ["100.101.102.103:52000", "203.0.113.9:52000"] {
        let b = banc();
        let (statut, page) = get(
            &b.state,
            depuis,
            AUTORISER,
            &[
                ("host", "192.168.1.10:8888"),
                ("accept-language", "fr-FR,fr;q=0.9"),
            ],
        )
        .await;
        assert_eq!(statut, StatusCode::FORBIDDEN, "{depuis} — {page}");
        assert!(page.contains("<html"), "le refus est une page : {page}");
        assert!(
            page.contains("Relier le compte mozaiklabs.fr") && page.contains("réseau local"),
            "{depuis} : phrase française attendue — {page}"
        );
        assert!(
            attente_du_serveur(&b.state).is_empty(),
            "{depuis} : rien de préparé"
        );
    }

    // Mandataire inverse local, qui exposerait le serveur sur Internet.
    let b = banc();
    let (statut, page) = get(
        &b.state,
        ICI,
        AUTORISER,
        &[
            ("host", "127.0.0.1:8888"),
            ("x-forwarded-for", "203.0.113.9"),
        ],
    )
    .await;
    assert_eq!(statut, StatusCode::FORBIDDEN, "mandataire — {page}");
    assert!(
        attente_du_serveur(&b.state).is_empty(),
        "mandataire : rien de préparé"
    );
    // Même derrière un mandataire du réseau local.
    let (statut, page) = get(
        &b.state,
        "192.168.1.2:52000",
        AUTORISER,
        &[
            ("host", "tune.local:8888"),
            ("x-forwarded-for", "203.0.113.9"),
        ],
    )
    .await;
    assert_eq!(statut, StatusCode::FORBIDDEN, "mandataire du LAN — {page}");

    // Relais distant et écoute directe : rejoués depuis 127.0.0.1, marqués.
    for depuis in [ICI, "192.168.1.50:52000"] {
        let (statut, page) = get(
            &b.state,
            depuis,
            AUTORISER,
            &[
                (tune_core::cloud::ENTETE_RELAIS, "1"),
                ("host", "127.0.0.1:8888"),
            ],
        )
        .await;
        assert_eq!(
            statut,
            StatusCode::FORBIDDEN,
            "relais depuis {depuis} — {page}"
        );
    }
    assert!(
        attente_du_serveur(&b.state).is_empty(),
        "relais : rien de préparé"
    );

    // Le statut, lui, ne montre que « relié » au mandataire.
    let r = reglages(&b.state);
    r.set("mozaik_access_token", "jeton-du-proprietaire")
        .unwrap();
    r.set("mozaik_user", COMPTE).unwrap();
    for depuis in [ICI, "192.168.1.2:52000"] {
        let (_, texte) = get(
            &b.state,
            depuis,
            STATUT,
            &[
                ("host", "tune.local:8888"),
                ("x-forwarded-for", "203.0.113.9"),
            ],
        )
        .await;
        let v = json_de(&texte);
        assert_eq!(v["details_hidden"], true, "mandataire {depuis} — {texte}");
        assert!(v["user"].is_null(), "mandataire {depuis} — {texte}");
    }
    let (_, texte) = get(
        &b.state,
        "100.101.102.103:52000",
        STATUT,
        &[("host", "192.168.1.10:8888")],
    )
    .await;
    assert!(json_de(&texte)["user"].is_null(), "CGNAT — {texte}");
}

/// 3. Authentification active, sans jeton : refusé, réseau local ou machine.
/// (Inchangé : ce test est vert avant comme après, c'est voulu.)
#[tokio::test]
async fn authentification_active_sans_jeton_la_liaison_est_refusee() {
    let b = banc();
    let r = reglages(&b.state);
    r.set("jwt_secret", "secret-de-banc").unwrap();
    r.set("auth_enabled", "true").unwrap();
    for depuis in ["192.168.1.50:52000", ICI] {
        let (statut, page) = get(
            &b.state,
            depuis,
            AUTORISER,
            &[("host", "192.168.1.10:8888")],
        )
        .await;
        assert_eq!(
            statut,
            StatusCode::UNAUTHORIZED,
            "sans jeton depuis {depuis} — {page}"
        );
        assert!(
            attente_du_serveur(&b.state).is_empty(),
            "{depuis} : rien de préparé"
        );
    }
}

// ─── Décision du 23/11 : pas de remplacement depuis le réseau local ─────────

const LAN: &str = "192.168.1.50:52000";
const HOTE: (&str, &str) = ("host", "192.168.1.10:8888");
const DISCONNECT: &str = "/api/v1/cloud/sso/disconnect";

/// 5. Réseau local, aucun compte relié : la liaison passe (307) et elle est
/// préparée.
#[tokio::test]
async fn reseau_local_aucun_compte_relie_la_liaison_passe() {
    let b = banc();
    let (statut, page) = get(&b.state, LAN, AUTORISER, &[HOTE]).await;
    assert_eq!(statut, StatusCode::TEMPORARY_REDIRECT, "{page}");
    assert!(!attente_du_serveur(&b.state).is_empty(), "liaison préparée");
}

/// 6. Réseau local, un compte déjà relié : refusé (403, motif
/// `account_already_linked`, phrase claire), rien de préparé, le compte reste.
#[tokio::test]
async fn reseau_local_compte_deja_relie_la_liaison_est_refusee() {
    let b = banc();
    relier_le_proprietaire(&b.state);
    let (statut, page) = get(
        &b.state,
        LAN,
        AUTORISER,
        &[HOTE, ("accept-language", "fr-FR,fr;q=0.9")],
    )
    .await;
    assert_eq!(statut, StatusCode::FORBIDDEN, "{page}");
    assert!(page.contains("<html"), "le refus est une page : {page}");
    assert!(
        page.contains("déjà relié à ce serveur") && page.contains("déliez-le"),
        "phrase française attendue — {page}"
    );
    assert!(attente_du_serveur(&b.state).is_empty(), "rien de préparé");
    assert_eq!(courriel_relie(&b.state), "proprio@exemple.test");

    // Le motif dédié, lisible par un client (JSON de la garde).
    let refus = crate::auth::refus_compte_deja_relie(&axum::http::HeaderMap::new());
    assert_eq!(refus.0, StatusCode::FORBIDDEN);
    assert_eq!(refus.1.0["reason"], "account_already_linked");

    // Même un compte dont la session est périmée (« reconnexion requise ») :
    // son identité suffit, il reste le compte du serveur.
    let b = banc();
    reglages(&b.state).set("mozaik_user", COMPTE).unwrap();
    let (statut, page) = get(&b.state, LAN, AUTORISER, &[HOTE]).await;
    assert_eq!(statut, StatusCode::FORBIDDEN, "identité seule — {page}");
}

/// 7. Cette machine, un compte déjà relié : la liaison passe (remplacement
/// permis depuis la machine).
#[tokio::test]
async fn cette_machine_compte_deja_relie_la_liaison_passe() {
    let b = banc();
    relier_le_proprietaire(&b.state);
    let (statut, page) = get(&b.state, ICI, AUTORISER, &[("host", "127.0.0.1:8888")]).await;
    assert_eq!(statut, StatusCode::TEMPORARY_REDIRECT, "{page}");
    assert!(!attente_du_serveur(&b.state).is_empty(), "liaison préparée");
}

/// 8. La course : une liaison lancée du réseau local quand aucun compte
/// n'était relié, puis un compte relié avant son retour. Le retour est
/// refusé, même avec le bon `state`, et le compte relié n'est pas remplacé.
#[tokio::test]
async fn reseau_local_le_retour_refait_le_controle() {
    let b = banc();
    let base = super::cloud_garde_compte_tests::faux_mozaiklabs("tiers@exemple.test").await;
    reglages(&b.state).set("mozaik_base_url", &base).unwrap();
    let (statut, page) = get(&b.state, LAN, AUTORISER, &[HOTE]).await;
    assert_eq!(statut, StatusCode::TEMPORARY_REDIRECT, "{page}");
    let etat = json_de(&attente_du_serveur(&b.state))["state"]
        .as_str()
        .unwrap()
        .to_string();

    // L'autre liaison, finie entre-temps.
    relier_le_proprietaire(&b.state);

    let (statut, page) = get(
        &b.state,
        LAN,
        &format!("/api/v1/cloud/sso/callback?code=c&state={etat}"),
        &[HOTE],
    )
    .await;
    assert_eq!(statut, StatusCode::FORBIDDEN, "retour — {page}");
    // Refusé dès l'entrée du retour : l'attente n'est pas consommée et le code
    // n'est pas échangé (aucun jeton émis pour rien).
    assert!(
        !attente_du_serveur(&b.state).is_empty(),
        "le contrôle a lieu avant l'échange du code"
    );
    assert_eq!(courriel_relie(&b.state), "proprio@exemple.test");
    assert_eq!(
        reglages(&b.state)
            .get("mozaik_access_token")
            .unwrap()
            .as_deref(),
        Some("jeton-du-proprietaire")
    );
}

/// Un faux mozaiklabs.fr dont l'échange du code « laisse finir » une autre
/// liaison pendant qu'il répond (le compte du propriétaire est relié dans la
/// base à cet instant).
async fn faux_mozaiklabs_pendant_une_autre_liaison(state: &AppState) -> String {
    use axum::routing::{get, post};
    let backend = state.backend.clone();
    let app = axum::Router::new()
        .route(
            "/oauth/token",
            post(move || {
                let r = SettingsRepo::with_backend(backend.clone());
                async move {
                    r.set("mozaik_access_token", "jeton-du-proprietaire")
                        .unwrap();
                    r.set("mozaik_user", COMPTE).unwrap();
                    axum::Json(serde_json::json!({
                        "access_token": "jeton-tiers",
                        "refresh_token": "renouv-tiers",
                        "expires_in": 3600
                    }))
                }
            }),
        )
        .route(
            "/api/v1/user",
            get(|| async {
                axum::Json(serde_json::json!({
                    "id": 77,
                    "email": "tiers@exemple.test",
                    "display_name": "Tiers",
                    "is_admin": false,
                    "avatar_url": null
                }))
            }),
        )
        .fallback(|| async { StatusCode::NOT_FOUND });
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });
    format!("http://{adresse}")
}

/// 9. La course, au plus près : l'autre liaison finit PENDANT l'échange du
/// code. Le contrôle refait juste avant d'enregistrer la session refuse.
#[tokio::test]
async fn reseau_local_une_liaison_finie_pendant_l_echange_n_est_pas_remplacee() {
    let b = banc();
    let base = faux_mozaiklabs_pendant_une_autre_liaison(&b.state).await;
    reglages(&b.state).set("mozaik_base_url", &base).unwrap();
    let (statut, page) = get(&b.state, LAN, AUTORISER, &[HOTE]).await;
    assert_eq!(statut, StatusCode::TEMPORARY_REDIRECT, "{page}");
    let etat = json_de(&attente_du_serveur(&b.state))["state"]
        .as_str()
        .unwrap()
        .to_string();
    let (statut, page) = get(
        &b.state,
        LAN,
        &format!("/api/v1/cloud/sso/callback?code=c&state={etat}"),
        &[HOTE],
    )
    .await;
    assert_eq!(statut, StatusCode::FORBIDDEN, "retour — {page}");
    assert_eq!(courriel_relie(&b.state), "proprio@exemple.test");
    assert_eq!(
        reglages(&b.state)
            .get("mozaik_access_token")
            .unwrap()
            .as_deref(),
        Some("jeton-du-proprietaire")
    );
}

/// 10. Témoin de la ronde : du réseau local, sans compte relié, la ronde
/// complète relie bien le serveur (le contrôle ne bloque pas le cas permis).
#[tokio::test]
async fn reseau_local_aucun_compte_la_ronde_complete_relie() {
    let b = banc();
    let base = super::cloud_garde_compte_tests::faux_mozaiklabs("tiers@exemple.test").await;
    reglages(&b.state).set("mozaik_base_url", &base).unwrap();
    let (statut, _) =
        super::cloud_garde_compte_tests::ronde_complete(&b.state, LAN, None, "mozaik_pkce_pending")
            .await;
    assert_eq!(statut, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(courriel_relie(&b.state), "tiers@exemple.test");
}

// ─── Décision du 23/11 : délier depuis le réseau local ──────────────────────

/// 11. Réseau local, sans authentification : la déliaison passe et efface le
/// compte du serveur.
#[tokio::test]
async fn reseau_local_sans_authentification_la_deliaison_passe() {
    let b = banc();
    relier_le_proprietaire(&b.state);
    let (statut, texte) = post(&b.state, LAN, DISCONNECT, &[HOTE]).await;
    assert_eq!(statut, StatusCode::OK, "{texte}");
    assert_eq!(json_de(&texte)["connected"], false, "{texte}");
    assert!(
        reglages(&b.state)
            .get("mozaik_access_token")
            .unwrap()
            .is_none()
    );
    assert!(reglages(&b.state).get("mozaik_user").unwrap().is_none());
}

/// 12. Relais distant, mandataire, CGNAT : la déliaison est refusée, le
/// compte reste relié.
#[tokio::test]
async fn hors_du_reseau_local_la_deliaison_est_refusee() {
    let b = banc();
    relier_le_proprietaire(&b.state);
    let cas: [(&str, &[(&str, &str)]); 5] = [
        (
            ICI,
            &[
                (tune_core::cloud::ENTETE_RELAIS, "1"),
                ("host", "127.0.0.1:8888"),
            ],
        ),
        (LAN, &[(tune_core::cloud::ENTETE_RELAIS, "1"), HOTE]),
        (
            ICI,
            &[
                ("host", "127.0.0.1:8888"),
                ("x-forwarded-for", "203.0.113.9"),
            ],
        ),
        (
            "192.168.1.2:52000",
            &[
                ("host", "tune.local:8888"),
                ("x-forwarded-for", "203.0.113.9"),
            ],
        ),
        ("100.101.102.103:52000", &[HOTE]),
    ];
    for (depuis, entetes) in cas {
        let (statut, texte) = post(&b.state, depuis, DISCONNECT, entetes).await;
        assert_eq!(
            statut,
            StatusCode::FORBIDDEN,
            "{depuis} {entetes:?} — {texte}"
        );
        // Le relais distant est arrêté plus tôt par sa propre garde
        // (`not_available_remotely`) ; les autres par la garde du compte.
        let v = json_de(&texte);
        assert!(
            v["error"] == "not_available_remotely" || v["reason"] == "not_local_network",
            "{depuis} {entetes:?} — {texte}"
        );
    }
    assert_eq!(courriel_relie(&b.state), "proprio@exemple.test");
    assert_eq!(
        reglages(&b.state)
            .get("mozaik_access_token")
            .unwrap()
            .as_deref(),
        Some("jeton-du-proprietaire")
    );
}

/// 13. Authentification active, sans jeton : la déliaison est refusée (401),
/// réseau local ou machine.
#[tokio::test]
async fn authentification_active_sans_jeton_la_deliaison_est_refusee() {
    let b = banc();
    relier_le_proprietaire(&b.state);
    let r = reglages(&b.state);
    r.set("jwt_secret", "secret-de-banc").unwrap();
    r.set("auth_enabled", "true").unwrap();
    for depuis in [LAN, ICI] {
        let (statut, texte) = post(&b.state, depuis, DISCONNECT, &[HOTE]).await;
        assert_eq!(
            statut,
            StatusCode::UNAUTHORIZED,
            "sans jeton depuis {depuis} — {texte}"
        );
    }
    assert_eq!(courriel_relie(&b.state), "proprio@exemple.test");
}

/// Les phrases du refus de liaison existent dans les seize langues.
#[test]
fn les_phrases_du_refus_de_liaison_existent_dans_les_seize_langues() {
    let catalogue: Value = serde_json::from_str(include_str!("../i18n_server.json")).unwrap();
    for l in [
        "fr", "en", "de", "es", "it", "zh", "ja", "ko", "ro", "sv", "hu", "vi", "nb", "pl", "el",
        "tr",
    ] {
        let t = catalogue["api.machine.reservedOutsideLanAccount"][l]
            .as_str()
            .unwrap_or_default();
        assert!(
            t.contains("{action}"),
            "reservedOutsideLanAccount/{l} : {t:?}"
        );
        let a = catalogue["api.machine.action.account"][l]
            .as_str()
            .unwrap_or_default();
        assert!(!a.trim().is_empty(), "action.account/{l} absente");
        let d = catalogue["api.machine.accountAlreadyLinked"][l]
            .as_str()
            .unwrap_or_default();
        assert!(
            d.contains("mozaiklabs.fr"),
            "accountAlreadyLinked/{l} : {d:?}"
        );
    }
    assert_eq!(
        crate::auth::action_du_chemin("/api/v1/cloud/sso/authorize"),
        "account"
    );
}
