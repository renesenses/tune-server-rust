//! #6336 (reliquat de #6173) — les routes de l'appairage AirPlay 2, du montage
//! SMB et de la connexion à Qobuz, TIDAL et Deezer, éprouvées par le VRAI
//! routeur contre des faux locaux, sans appel réseau réel ni secret réel.
//!
//! Comment ces services deviennent remplaçables sans changer la production :
//!
//! - **programmes externes** (démon `airplay-daemon`, `mount.cifs` ou
//!   l'assistant par `sudo`) : `tune_core::services_factices` les substitue
//!   par un faux script. La substitution n'existe que sous la feature
//!   `services-factices`, allumée par les seuls `[dev-dependencies]` ;
//! - **services de streaming** : `avec_bases_de_test` construit le service
//!   avec des adresses de base locales, connexion comprise (même feature) ;
//! - **licence et SSO** : redirigeables par le réglage `mozaik_base_url`
//!   (`licence_activation_immediate.rs`, `licence_verdict_absent.rs`,
//!   `sso_modules_du_compte.rs`, `sso_reconnexion_apres_attente.rs`), sauf le
//!   battement de la licence et la liaison du serveur au compte, qui passent
//!   par `services_factices::substituer_base` : banc
//!   `licence_sso_remplacables_6336.rs`.
//!
//! Binaire à lui seul : le registre des substitutions est un état de
//! PROCESSUS. Les tests qui s'en servent se suivent sous [`SUBSTITUTIONS`] ;
//! chacun écrit ses faux programmes dans son propre dossier jetable.
//!
//! Aucun appel ne quitte la machine : les faux sont sur `127.0.0.1`, et la
//! télémétrie est coupée par son réglage.
#![cfg(target_os = "linux")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::test_scratch::ScratchDir;
use tune_server::state::AppState;

// ── Outils communs ────────────────────────────────────────────────────────

fn etat(dossier: &ScratchDir) -> AppState {
    let db = dossier.join("tune.db");
    let etat = AppState::new(db.to_str().unwrap(), 0, Default::default()).expect("état isolé");
    SettingsRepo::with_backend(etat.backend.clone())
        .set(tune_core::cloud::telemetry::TELEMETRY_SETTING_KEY, "false")
        .expect("télémétrie coupée");
    etat
}

async fn appeler(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(methode).uri(chemin);
    let body = match corps {
        Some(c) => {
            req = req.header("content-type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let statut = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let valeur = serde_json::from_slice(&octets)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&octets).into_owned()));
    (statut, valeur)
}

/// Un faux serveur HTTP local ; rend `http://127.0.0.1:<port>`.
async fn servir(app: axum::Router) -> String {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.ok();
    });
    format!("http://{adresse}")
}

fn script(chemin: &Path, contenu: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(chemin, contenu).unwrap();
    std::fs::set_permissions(chemin, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Le registre des substitutions pointe des scripts du dossier d'UN test :
/// les tests qui substituent un programme se suivent, jamais ensemble.
static SUBSTITUTIONS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

// ── AirPlay 2 : appairage par code ────────────────────────────────────────

/// Faux `airplay-daemon` : le protocole JSON ligne à ligne du vrai démon.
/// Annonce `ready`, répond `pin_requested` à `pair_pin_start`, puis
/// `connected` au code 1234 et une erreur à tout autre code.
fn faux_demon_airplay(dossier: &Path) {
    let chemin = dossier.join("faux-airplay-daemon");
    script(
        &chemin,
        r#"#!/bin/sh
echo '{"event":"ready"}'
while IFS= read -r ligne; do
  case "$ligne" in
    *'"pair_pin_start"'*) echo '{"event":"pin_requested"}' ;;
    *'"pin":"1234"'*) echo '{"event":"connected"}' ;;
    *'"connect"'*) echo '{"event":"error","message":"code refuse par le recepteur temoin"}' ;;
  esac
done
"#,
    );
    tune_core::services_factices::substituer_programme("airplay-daemon", chemin.to_str().unwrap());
}

const SORTIE_AP2: &str = "airplay2:temoin-6336";

async fn etat_avec_recepteur_airplay2(dossier: &ScratchDir) -> AppState {
    faux_demon_airplay(dossier);
    let etat = etat(dossier);
    let sortie = tune_core::outputs::airplay2::Airplay2Output::new(
        "Téléviseur témoin".into(),
        "127.0.0.1".into(),
        7000,
        SORTIE_AP2.into(),
        "00:11:22:33:44:55".into(),
    );
    etat.outputs.lock().await.register(Box::new(sortie));
    etat
}

/// Attend que `pair-status` quitte `idle` (le démon répond de façon
/// asynchrone), au plus deux secondes.
async fn statut_d_appairage(app: &axum::Router, attendu: &str) -> String {
    let chemin = format!("/api/v1/outputs/{SORTIE_AP2}/airplay/pair-status");
    let mut dernier = String::new();
    for _ in 0..100 {
        let (statut, corps) = appeler(app, "GET", &chemin, None).await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
        dernier = corps["status"].as_str().unwrap_or_default().to_string();
        if dernier.starts_with(attendu) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    dernier
}

#[tokio::test]
async fn l_appairage_airplay2_par_code_va_jusqu_a_connecte_6336() {
    let _une_substitution_a_la_fois = SUBSTITUTIONS.lock().await;
    let dossier = tune_core::test_scratch::scratch_dir("tune-6336-ap2");
    let app = tune_server::routes::router(etat_avec_recepteur_airplay2(&dossier).await);

    let (statut, corps) = appeler(
        &app,
        "POST",
        &format!("/api/v1/outputs/{SORTIE_AP2}/airplay/pair-start"),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["status"], "pin_requested", "{corps}");
    assert_eq!(
        statut_d_appairage(&app, "pin_requested").await,
        "pin_requested"
    );

    let (statut, corps) = appeler(
        &app,
        "POST",
        &format!("/api/v1/outputs/{SORTIE_AP2}/airplay/pair-pin"),
        Some(json!({"pin": " 1234 "})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(
        statut_d_appairage(&app, "connected").await,
        "connected",
        "le code saisi doit parvenir au démon, sans ses espaces"
    );
}

#[tokio::test]
async fn un_mauvais_code_airplay2_rend_l_echec_du_recepteur_6336() {
    let _une_substitution_a_la_fois = SUBSTITUTIONS.lock().await;
    let dossier = tune_core::test_scratch::scratch_dir("tune-6336-ap2-refus");
    let app = tune_server::routes::router(etat_avec_recepteur_airplay2(&dossier).await);

    let (statut, _) = appeler(
        &app,
        "POST",
        &format!("/api/v1/outputs/{SORTIE_AP2}/airplay/pair-start"),
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        statut_d_appairage(&app, "pin_requested").await,
        "pin_requested"
    );
    let (statut, _) = appeler(
        &app,
        "POST",
        &format!("/api/v1/outputs/{SORTIE_AP2}/airplay/pair-pin"),
        Some(json!({"pin": "0000"})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        statut_d_appairage(&app, "failed:").await,
        "failed:code refuse par le recepteur temoin"
    );
}

#[tokio::test]
async fn un_code_sans_appairage_en_cours_est_refuse_6336() {
    let _une_substitution_a_la_fois = SUBSTITUTIONS.lock().await;
    let dossier = tune_core::test_scratch::scratch_dir("tune-6336-ap2-sans");
    let app = tune_server::routes::router(etat_avec_recepteur_airplay2(&dossier).await);
    let (statut, corps) = appeler(
        &app,
        "POST",
        &format!("/api/v1/outputs/{SORTIE_AP2}/airplay/pair-pin"),
        Some(json!({"pin": "1234"})),
    )
    .await;
    assert_eq!(statut, StatusCode::INTERNAL_SERVER_ERROR, "{corps}");
    assert!(
        corps.to_string().contains("start_pin_pairing"),
        "le refus doit dire qu'il faut d'abord demander le code : {corps}"
    );
}

// ── SMB : montage d'un partage ────────────────────────────────────────────

/// Faux `mount.cifs` (service en root) ET faux `sudo` (service sous un autre
/// compte, assistant `smb-mount`) : un seul script, qui note ses arguments et
/// son entrée standard dans `journal`, et refuse les identifiants pour le
/// partage `refuse`.
fn faux_montage_smb(dossier: &Path) -> PathBuf {
    let journal = dossier.join("journal-montage");
    let chemin = dossier.join("faux-mount-cifs");
    script(
        &chemin,
        &format!(
            r#"#!/bin/sh
entree=$(cat)
identifiants=""
for a in "$@"; do
  case "$a" in
    *credentials=*) f=${{a#*credentials=}}; f=${{f%%,*}}; identifiants=$(cat "$f" | tr '\n' ' ') ;;
  esac
done
echo "ARGS $* ENTREE $entree IDENTIFIANTS $identifiants" >> '{journal}'
case "$*" in
  *//127.0.0.1/refuse*) echo 'mount error(13): Permission denied' >&2; exit 32 ;;
esac
exit 0
"#,
            journal = journal.display()
        ),
    );
    let faux = chemin.to_str().unwrap();
    tune_core::services_factices::substituer_programme("mount.cifs", faux);
    tune_core::services_factices::substituer_programme(&tune_server::privilege::sudo(), faux);
    // Le point de montage est choisi par le serveur, sous sa racine : celle du
    // banc vit dans le dossier jetable. Sous `SUBSTITUTIONS`, un seul test à
    // la fois la lit.
    // SAFETY : les tests de ce binaire qui lisent la variable se suivent sous
    // le verrou `SUBSTITUTIONS`.
    unsafe {
        std::env::set_var(
            tune_server::routes::network::VARIABLE_RACINE_DES_MONTAGES,
            dossier.join("montages"),
        );
    }
    journal
}

fn lignes_du_journal(journal: &Path, motif: &str) -> Vec<String> {
    std::fs::read_to_string(journal)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.contains(motif))
        .map(String::from)
        .collect()
}

#[tokio::test]
async fn monter_un_partage_smb_lance_le_montage_et_enregistre_le_partage_6336() {
    let _une_substitution_a_la_fois = SUBSTITUTIONS.lock().await;
    let dossier = tune_core::test_scratch::scratch_dir("tune-6336-smb");
    let journal = faux_montage_smb(&dossier);
    let point = dossier.join("montages").join("127.0.0.1_musique");
    let etat = etat(&dossier);
    let app = tune_server::routes::router(etat.clone());

    let (statut, corps) = appeler(
        &app,
        "POST",
        "/api/v1/network/smb/mount",
        Some(json!({
            "host": "127.0.0.1",
            "share_name": "musique",
            "username": "temoin",
            "password": "mot-de-passe-factice-6336",
        })),
    )
    .await;
    assert_eq!(statut, StatusCode::CREATED, "{corps}");
    assert_eq!(corps["mounted"], true, "{corps}");
    assert_eq!(corps["mount_path"], point.to_str().unwrap(), "{corps}");
    assert!(point.is_dir(), "le point de montage doit avoir été créé");

    // Le programme de montage a bien été lancé, sur ce partage et ce point,
    // avec le mot de passe (dans le fichier `credentials=` en root, sur
    // l'entrée de l'assistant sinon) — et une seule fois : la négociation
    // libre a réussi.
    let appels = lignes_du_journal(&journal, &format!("{} ", point.display()));
    assert_eq!(appels.len(), 1, "un seul essai attendu : {appels:?}");
    assert!(appels[0].contains("//127.0.0.1/musique"), "{appels:?}");
    assert!(
        appels[0].contains("mot-de-passe-factice-6336"),
        "{appels:?}"
    );

    // Le partage est enregistré, avec le constat de montage.
    let (statut, liste) = appeler(&app, "GET", "/api/v1/network/smb/mounts", None).await;
    assert_eq!(statut, StatusCode::OK, "{liste}");
    assert!(
        liste.to_string().contains("musique"),
        "le partage monté doit figurer dans la liste : {liste}"
    );
}

#[tokio::test]
async fn un_refus_d_identifiants_smb_arrete_l_echelle_et_le_dit_6336() {
    let _une_substitution_a_la_fois = SUBSTITUTIONS.lock().await;
    let dossier = tune_core::test_scratch::scratch_dir("tune-6336-smb-refus");
    let journal = faux_montage_smb(&dossier);
    let point = dossier.join("montages").join("127.0.0.1_refuse");
    let app = tune_server::routes::router(etat(&dossier));

    let (statut, corps) = appeler(
        &app,
        "POST",
        "/api/v1/network/smb/mount",
        Some(json!({
            "host": "127.0.0.1",
            "share_name": "refuse",
            "username": "temoin",
            "password": "mauvais-mot-de-passe-factice",
        })),
    )
    .await;
    assert_eq!(statut, StatusCode::INTERNAL_SERVER_ERROR, "{corps}");
    assert!(
        corps["error"]
            .as_str()
            .is_some_and(|e| e.contains("Permission denied")),
        "la vraie cause doit être rendue : {corps}"
    );
    // Un refus d'identifiants ne se répare pas en changeant de dialecte.
    let appels = lignes_du_journal(&journal, &format!("{} ", point.display()));
    assert_eq!(
        appels.len(),
        1,
        "l'échelle des dialectes doit s'arrêter : {appels:?}"
    );
}

// ── Qobuz : connexion par identifiants ────────────────────────────────────

/// Faux Qobuz : la configuration distante (identifiants d'application
/// factices), et `/user/login` qui accepte un seul couple.
async fn faux_qobuz() -> String {
    use axum::routing::{get, post};
    let app = axum::Router::new()
        .route(
            "/config.json",
            get(|| async {
                axum::Json(json!({"qobuz": {"app_id": "app-id-factice-6336", "app_secret": "secret-factice-6336"}}))
            }),
        )
        .route(
            "/api/user/login",
            post(
                |q: axum::extract::Query<std::collections::HashMap<String, String>>,
                 f: axum::extract::Form<std::collections::HashMap<String, String>>| async move {
                    let bon = q.get("app_id").map(String::as_str) == Some("app-id-factice-6336")
                        && f.get("username").map(String::as_str) == Some("temoin@example.invalid")
                        && f.get("password").map(String::as_str) == Some("mot-de-passe-factice");
                    if bon {
                        (
                            StatusCode::OK,
                            axum::Json(json!({
                                "user_auth_token": "jeton-qobuz-factice-6336",
                                "user": {"display_name": "Témoin Qobuz", "credential": {"label": "Studio"}},
                            })),
                        )
                    } else {
                        (
                            StatusCode::UNAUTHORIZED,
                            axum::Json(json!({"status": "error", "code": 401, "message": "Invalid username/email and password combination"})),
                        )
                    }
                },
            ),
        );
    servir(app).await
}

async fn etat_avec_qobuz(dossier: &ScratchDir) -> AppState {
    let base = faux_qobuz().await;
    let etat = etat(dossier);
    let qobuz = tune_core::streaming::qobuz::QobuzService::avec_bases_de_test(
        "app-id-initial".into(),
        "secret-initial".into(),
        tune_core::streaming::qobuz::BasesQobuz {
            api: format!("{base}/api"),
            // Le relais aussi est local : un repli ne sortirait pas non plus.
            proxy: format!("{base}/relais-absent"),
            config_distante: format!("{base}/config.json"),
        },
    );
    etat.services.lock().await.register(Box::new(qobuz));
    etat
}

#[tokio::test]
async fn la_connexion_qobuz_par_identifiants_rend_le_compte_sans_son_jeton_6336() {
    let dossier = tune_core::test_scratch::scratch_dir("tune-6336-qobuz");
    let etat = etat_avec_qobuz(&dossier).await;
    let reglages = SettingsRepo::with_backend(etat.backend.clone());
    let app = tune_server::routes::router(etat);

    let (statut, corps) = appeler(
        &app,
        "POST",
        "/api/v1/streaming/qobuz/auth",
        Some(json!({"username": "temoin@example.invalid", "password": "mot-de-passe-factice"})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["authenticated"], true, "{corps}");
    assert_eq!(corps["username"], "Témoin Qobuz", "{corps}");
    let texte = corps.to_string();
    assert!(
        !texte.contains("jeton-qobuz-factice-6336"),
        "jeton rendu : {corps}"
    );
    assert!(
        !texte.contains("mot-de-passe-factice"),
        "mot de passe rendu : {corps}"
    );

    // Le jeton est rangé pour le redémarrage ; le mot de passe, jamais.
    let range = reglages
        .get("auth_tokens_qobuz")
        .unwrap()
        .unwrap_or_default();
    assert!(range.contains("jeton-qobuz-factice-6336"), "{range}");
    assert!(!range.contains("mot-de-passe-factice"), "{range}");
}

#[tokio::test]
async fn un_mot_de_passe_qobuz_refuse_rend_400_et_ne_connecte_pas_6336() {
    let dossier = tune_core::test_scratch::scratch_dir("tune-6336-qobuz-refus");
    let app = tune_server::routes::router(etat_avec_qobuz(&dossier).await);

    let (statut, corps) = appeler(
        &app,
        "POST",
        "/api/v1/streaming/qobuz/auth",
        Some(json!({"username": "temoin@example.invalid", "password": "faux"})),
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
    // #7068 : le 401 du service est désormais un refus structuré ; le
    // contrat #6336 reste HTTP400 et aucune session ouverte, sans imposer
    // que le texte brut du service soit encore recopié dans la réponse.
    assert_eq!(
        corps["code"], "invalid_credentials",
        "#7068/#6336 : le refus d'identifiants doit porter son code structuré : {corps}"
    );
    assert_eq!(corps["service"], "qobuz", "{corps}");
    let message = corps["error"]
        .as_str()
        .expect("une phrase d'erreur lisible");
    assert!(
        !message.trim().is_empty() && !message.contains('{') && !message.contains('}'),
        "#7068 : le refus doit rester une phrase sans JSON du service : {corps}"
    );
    let (_, etat_auth) = appeler(&app, "GET", "/api/v1/streaming/qobuz/auth/status", None).await;
    assert_eq!(etat_auth["authenticated"], false, "{etat_auth}");
}

// ── TIDAL : connexion par code d'appareil ─────────────────────────────────

/// Faux TIDAL : l'autorisation d'appareil, l'échange du code, et le profil.
async fn faux_tidal() -> String {
    use axum::routing::{get, post};
    let app = axum::Router::new()
        .route(
            "/oauth2/device_authorization",
            post(|| async {
                axum::Json(json!({
                    "deviceCode": "code-appareil-factice-6336",
                    "userCode": "TEMOIN",
                    "verificationUri": "link.tidal.invalid",
                    "verificationUriComplete": "link.tidal.invalid/TEMOIN",
                    "expiresIn": 300,
                    "interval": 2,
                }))
            }),
        )
        .route(
            "/oauth2/token",
            post(
                |f: axum::extract::Form<std::collections::HashMap<String, String>>| async move {
                    if f.get("device_code").map(String::as_str)
                        == Some("code-appareil-factice-6336")
                    {
                        (
                            StatusCode::OK,
                            axum::Json(json!({
                                "access_token": "jeton-tidal-factice-6336",
                                "refresh_token": "rafraichissement-tidal-factice-6336",
                                "token_type": "Bearer",
                                "expires_in": 3600,
                                "user_id": 6336,
                            })),
                        )
                    } else {
                        (
                            StatusCode::BAD_REQUEST,
                            axum::Json(json!({"error": "invalid_grant"})),
                        )
                    }
                },
            ),
        )
        .route(
            "/v1/users/me",
            get(|| async { axum::Json(json!({"username": "temoin-tidal", "countryCode": "BE"})) }),
        )
        .route(
            "/v1/users/6336/subscription",
            get(|| async { axum::Json(json!({"subscription": {"type": "HIFI"}})) }),
        );
    servir(app).await
}

#[tokio::test]
async fn la_connexion_tidal_par_code_d_appareil_aboutit_par_le_sondage_6336() {
    let base = faux_tidal().await;
    let dossier = tune_core::test_scratch::scratch_dir("tune-6336-tidal");
    let etat = etat(&dossier);
    etat.services.lock().await.register(Box::new(
        tune_core::streaming::tidal::TidalService::avec_bases_de_test(
            format!("{base}/v1"),
            format!("{base}/oauth2"),
        ),
    ));
    let reglages = SettingsRepo::with_backend(etat.backend.clone());
    let app = tune_server::routes::router(etat);

    let (statut, corps) = appeler(
        &app,
        "POST",
        "/api/v1/streaming/tidal/auth",
        Some(json!({"device_flow": true})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["authenticated"], false, "{corps}");
    assert_eq!(corps["user_code"], "TEMOIN", "{corps}");
    assert_eq!(
        corps["verification_url"], "https://link.tidal.invalid/TEMOIN",
        "{corps}"
    );

    let (statut, corps) = appeler(&app, "GET", "/api/v1/streaming/tidal/auth/status", None).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["authenticated"], true, "{corps}");
    assert_eq!(corps["username"], "temoin-tidal", "{corps}");
    assert!(
        !corps.to_string().contains("jeton-tidal-factice-6336"),
        "jeton rendu : {corps}"
    );
    let range = reglages
        .get("auth_tokens_tidal")
        .unwrap()
        .unwrap_or_default();
    assert!(
        range.contains("rafraichissement-tidal-factice-6336"),
        "{range}"
    );
}

// ── Deezer : connexion par ARL ────────────────────────────────────────────

/// Un ARL FACTICE, de la longueur d'un vrai (le service refuse moins de 100
/// caractères avant tout appel).
fn arl_factice() -> String {
    "arl-factice-6336-".repeat(12)
}

/// Fausse passerelle Deezer : `deezer.getUserData` reconnaît l'ARL factice
/// par le cookie, et rend un compte anonyme (`USER_ID` 0) pour tout autre.
async fn fausse_passerelle_deezer() -> String {
    use axum::routing::post;
    let attendu = format!("arl={}", arl_factice());
    let app = axum::Router::new().route(
        "/gw",
        post(move |entetes: axum::http::HeaderMap| {
            let attendu = attendu.clone();
            async move {
                let cookie = entetes
                    .get("cookie")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default();
                let utilisateur = if cookie.starts_with(&attendu) {
                    json!({"USER_ID": 6336, "BLOG_NAME": "Témoin Deezer",
                           "OPTIONS": {"license_token": "licence-factice-6336"}})
                } else {
                    json!({"USER_ID": 0})
                };
                axum::Json(json!({"error": [], "results": {
                    "USER": utilisateur, "checkForm": "jeton-csrf-factice"
                }}))
            }
        }),
    );
    servir(app).await
}

async fn etat_avec_deezer(dossier: &ScratchDir) -> AppState {
    let base = fausse_passerelle_deezer().await;
    let etat = etat(dossier);
    etat.services.lock().await.register(Box::new(
        tune_core::streaming::deezer::DeezerService::avec_bases_de_test(
            format!("{base}/gw"),
            format!("{base}/api"),
        ),
    ));
    etat
}

#[tokio::test]
async fn la_connexion_deezer_par_arl_rend_le_compte_sans_l_arl_6336() {
    let dossier = tune_core::test_scratch::scratch_dir("tune-6336-deezer");
    let app = tune_server::routes::router(etat_avec_deezer(&dossier).await);

    let (statut, corps) = appeler(
        &app,
        "POST",
        "/api/v1/streaming/deezer/auth",
        Some(json!({"arl": arl_factice()})),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["authenticated"], true, "{corps}");
    assert_eq!(corps["username"], "Témoin Deezer", "{corps}");
    assert!(
        !corps.to_string().contains("arl-factice-6336"),
        "l'ARL ne doit pas revenir dans la réponse : {corps}"
    );
}

#[tokio::test]
async fn un_arl_refuse_par_deezer_rend_400_6336() {
    let dossier = tune_core::test_scratch::scratch_dir("tune-6336-deezer-refus");
    let app = tune_server::routes::router(etat_avec_deezer(&dossier).await);

    let (statut, corps) = appeler(
        &app,
        "POST",
        "/api/v1/streaming/deezer/auth",
        Some(json!({"arl": "inconnu-".repeat(20)})),
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "{corps}");
    let (_, etat_auth) = appeler(&app, "GET", "/api/v1/streaming/deezer/auth/status", None).await;
    assert_eq!(etat_auth["authenticated"], false, "{etat_auth}");
}

// ── La production ne change pas ───────────────────────────────────────────

/// Les substitutions n'existent que sous la feature `services-factices`. Elle
/// ne doit être allumée QUE par les `[dev-dependencies]` de tune-server : un
/// manifeste qui l'allumerait en dépendance normale, ou une ligne de build
/// publiée qui la nommerait, rendrait ces substituts actifs en production.
#[test]
fn la_feature_des_services_factices_n_atteint_aucun_binaire_publie_6336() {
    let racine = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let espace: toml::Value =
        toml::from_str(&std::fs::read_to_string(racine.join("Cargo.toml")).unwrap()).unwrap();
    let membres = espace["workspace"]["members"].as_array().unwrap();
    let mut allumages = Vec::new();
    for membre in membres {
        let chemin = racine.join(membre.as_str().unwrap()).join("Cargo.toml");
        let Ok(texte) = std::fs::read_to_string(&chemin) else {
            continue;
        };
        let manifeste: toml::Value = toml::from_str(&texte).unwrap();
        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(deps) = manifeste.get(section).and_then(|d| d.as_table()) {
                for (nom, dep) in deps {
                    let porte = dep
                        .get("features")
                        .and_then(|f| f.as_array())
                        .is_some_and(|f| f.iter().any(|x| x.as_str() == Some("services-factices")));
                    if porte {
                        allumages.push(format!("{} [{section}] {nom}", membre.as_str().unwrap()));
                    }
                }
            }
        }
        // Une feature du manifeste qui l'entraînerait (`x = ["tune-core/services-factices"]`).
        if let Some(features) = manifeste.get("features").and_then(|f| f.as_table()) {
            for (nom, liste) in features {
                if liste.as_array().is_some_and(|l| {
                    l.iter()
                        .any(|x| x.as_str().is_some_and(|x| x.ends_with("services-factices")))
                }) {
                    allumages.push(format!("{} [features] {nom}", membre.as_str().unwrap()));
                }
            }
        }
    }
    assert_eq!(
        allumages,
        vec!["tune-server [dev-dependencies] tune-core".to_string()],
        "`services-factices` ne doit être allumée que par les dev-dependencies de tune-server"
    );
    for publie in ["release.yml", "docker.yml"] {
        let texte = std::fs::read_to_string(racine.join(".github/workflows").join(publie)).unwrap();
        assert!(
            !texte.contains("services-factices"),
            "{publie} nomme `services-factices` : un binaire publié porterait les substituts"
        );
    }
    for dockerfile in ["Dockerfile", "Dockerfile.dist"] {
        let texte = std::fs::read_to_string(racine.join(dockerfile)).unwrap_or_default();
        assert!(
            !texte.contains("services-factices"),
            "{dockerfile} nomme `services-factices`"
        );
    }
}

/// #7250 — le vrai connecteur, la route et son cache gardent les 357 favoris.
#[tokio::test]
async fn favoris_tidal_plafonnes_route_et_cache_7250() {
    use axum::extract::Query;
    use std::collections::HashMap;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tune_core::streaming::traits::StreamingService;
    let appels = Arc::new(AtomicUsize::new(0));
    let compteur = appels.clone();
    let faux = axum::Router::new().route("/users/7/favorites/tracks", axum::routing::get(
        move |Query(q): Query<HashMap<String, String>>| {
            let compteur = compteur.clone();
            async move {
                compteur.fetch_add(1, Ordering::SeqCst);
                let offset = q["offset"].parse::<usize>().unwrap();
                let items: Vec<_> = (offset..(offset + 50).min(357)).map(|i| json!({
                    "created": "2026-10-01T10:00:00.000+0000",
                    "item": {"id": i + 1, "title": format!("Piste {}", i + 1)}
                })).collect();
                axum::Json(json!({"limit": 50, "offset": offset, "totalNumberOfItems": 357, "items": items}))
            }
        }
    ))
    .route("/users/7/favorites/albums", axum::routing::get(|| async { axum::Json(json!({"totalNumberOfItems": 0, "items": []})) }))
    .route("/users/7/favorites/artists", axum::routing::get(|| async { axum::Json(json!({"totalNumberOfItems": 0, "items": []})) }));
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", ecoute.local_addr().unwrap());
    let serveur = tokio::spawn(async move {
        axum::serve(ecoute, faux).await.unwrap();
    });
    let mut svc = tune_core::streaming::tidal::TidalService::avec_bases_de_test(base.clone(), base);
    assert!(svc.restore_tokens(&json!({"access_token": "jeton-factice-7250", "refresh_token": "renouvellement-factice-7250", "user_id": 7, "country_code": "BE"})));
    let dossier = tune_core::test_scratch::scratch_dir("tune-7250-favoris");
    let etat = etat(&dossier);
    etat.services.lock().await.register(Box::new(svc));
    let backend = etat.backend.clone();
    let service = etat.services.lock().await.get("tidal").unwrap();
    let app = tune_server::routes::router(etat);
    let attendus: Vec<_> = (1..=357).map(|id| id.to_string()).collect();
    for passage in 0..2 {
        let (statut, corps) = appeler(
            &app,
            "GET",
            "/api/v1/streaming/tidal/favorites/tracks",
            None,
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
        let ids: Vec<_> = corps["tracks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["source_id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            ids, attendus,
            "#7250 : la route et son cache doivent conserver chaque favori (passage {passage})"
        );
        assert_eq!(
            appels.load(Ordering::SeqCst),
            8,
            "#7250 : huit pages puis une réponse en cache sans nouvelle lecture"
        );
    }
    let bilan = tune_core::streaming::favorites_mirror::rafraichir(&service, &backend, 1).await;
    assert_eq!(bilan.echecs, 0, "#7250 : miroir complet : {bilan:?}");
    assert_eq!(
        bilan.ajoutes, 357,
        "#7250 : tous les favoris doivent être importés : {bilan:?}"
    );
    let repo = tune_core::db::streaming_favorites_repo::StreamingFavoritesRepo::with_backend(
        backend.clone(),
    );
    let ranges = repo.list(1, Some("track")).unwrap();
    assert_eq!(ranges.len(), 357, "#7250 : 357 lignes réellement rangées");
    for id in &attendus {
        assert!(
            repo.is_favorite(1, "track", "tidal", id).unwrap(),
            "#7250 : favori {id} absent du miroir"
        );
    }
    let bis = tune_core::streaming::favorites_mirror::rafraichir(&service, &backend, 1).await;
    assert_eq!(bis.echecs, 0, "{bis:?}");
    assert_eq!(
        bis.retires, 0,
        "#7250 : la relecture ne doit retirer aucun favori : {bis:?}"
    );
    assert_eq!(repo.list(1, Some("track")).unwrap().len(), 357);
    serveur.abort();
}
