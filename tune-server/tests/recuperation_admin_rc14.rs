//! rc14 — retrouver l'accès administrateur depuis la machine du serveur.
//!
//! `tune-server --reinitialiser-admin` écrit un code à usage unique ; la base
//! n'en garde que l'empreinte. `POST /auth/admin/reinitialiser` ne l'accepte
//! que d'un appel de CETTE machine, remplace le mot de passe du compte
//! administrateur et révoque les sessions.

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_server::recuperation_admin as ra;
use tune_server::state::AppState;

const CETTE_MACHINE: &str = "127.0.0.1:50000";
const AUTRE_HOTE_DU_LAN: &str = "192.168.1.50:50000";
const ROUTE: &str = "/api/v1/auth/admin/reinitialiser";
const ANCIEN_SECRET: &str = "secret-avant-la-reinitialisation";

/// Les épreuves qui posent `TUNE_APPLIANCE` / `TUNE_WEB80_RELAIS` changent
/// ce que vaut le bouclage pour tout le processus : une à la fois.
static ENVIRONNEMENT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Serveur sous authentification, compte administrateur (profil 1) muni d'un
/// mot de passe connu.
fn etat(chemin: &str) -> AppState {
    let state = AppState::new(chemin, 0, Default::default()).unwrap();
    let reglages = settings(&state);
    reglages.set("auth_enabled", "true").unwrap();
    reglages.set("jwt_secret", ANCIEN_SECRET).unwrap();
    let (id, _) = ra::compte_administrateur(&state.backend).expect("profil administrateur");
    assert_eq!(id, 1);
    let h = tune_server::auth::hash_password("ancien-mot-de-passe").unwrap();
    use tune_core::db::backend::ToSqlValue;
    state
        .backend
        .execute(
            "UPDATE profiles SET password_hash_v2 = ? WHERE id = 1",
            &[&h as &dyn ToSqlValue],
        )
        .unwrap();
    state
}

fn settings(state: &AppState) -> SettingsRepo {
    SettingsRepo::with_backend(state.backend.clone())
}

fn hachage_admin(state: &AppState) -> String {
    state
        .backend
        .query_one("SELECT password_hash_v2 FROM profiles WHERE id = 1", &[])
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_string()))
        .unwrap_or_default()
}

fn mot_de_passe_est(state: &AppState, mot_de_passe: &str) -> bool {
    tune_server::auth::verify_password(mot_de_passe, &hachage_admin(state))
}

async fn appel(
    state: &AppState,
    adresse: &str,
    methode: Method,
    chemin: &str,
    entetes: &[(&str, &str)],
    corps: Option<String>,
) -> (StatusCode, Value) {
    let pair: SocketAddr = adresse.parse().unwrap();
    let app: Router = tune_server::routes::router(state.clone()).layer(MockConnectInfo(pair));
    let mut req = Request::builder().method(methode).uri(chemin);
    // Un navigateur envoie toujours `Host` ; `("host", "")` l'omet.
    if !entetes.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        req = req.header(header::HOST, "localhost:8888");
    }
    for (k, v) in entetes {
        if !v.is_empty() {
            req = req.header(*k, *v);
        }
    }
    let body = match corps {
        Some(c) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(c)
        }
        None => Body::empty(),
    };
    let resp = app.oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let v = serde_json::from_slice(&octets)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&octets).to_string()));
    (status, v)
}

fn corps(code: &str, mot_de_passe: &str) -> String {
    serde_json::json!({ "code": code, "nouveau_mot_de_passe": mot_de_passe }).to_string()
}

async fn reinitialiser(
    state: &AppState,
    adresse: &str,
    entetes: &[(&str, &str)],
    code: &str,
    mot_de_passe: &str,
) -> (StatusCode, Value) {
    appel(
        state,
        adresse,
        Method::POST,
        ROUTE,
        entetes,
        Some(corps(code, mot_de_passe)),
    )
    .await
}

/// Adresse de la connexion, en-têtes ajoutés, et ce que le cas éprouve.
type CasHorsMachine = (
    &'static str,
    &'static [(&'static str, &'static str)],
    &'static str,
);

/// Un code faux de la même forme que les vrais.
fn code_faux(vrai: &str) -> String {
    let mut faux: Vec<char> = vrai.chars().collect();
    faux[0] = if faux[0] == 'A' { 'B' } else { 'A' };
    faux.into_iter().collect()
}

#[tokio::test]
async fn code_valide_change_le_mot_de_passe_et_revoque_les_sessions() {
    let _env = ENVIRONNEMENT.lock().await;
    let state = etat(":memory:");
    let ancien_jeton = tune_server::auth::sign_jwt(1, "admin", ANCIEN_SECRET).unwrap();
    let bearer = format!("Bearer {ancien_jeton}");
    let (st, _) = appel(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::GET,
        "/api/v1/auth/me",
        &[("authorization", &bearer)],
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "la session d'avant est valide");

    let code = ra::emettre_un_code(&settings(&state)).unwrap();
    // Saisie libre : minuscules et tirets sont admis.
    let saisie = format!("{}-{}", &code[..5], &code[5..]).to_lowercase();
    let (st, v) = reinitialiser(&state, CETTE_MACHINE, &[], &saisie, "nouveau-mot-de-passe").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["reset"], true);
    assert_eq!(v["username"], "default");
    assert!(v["token"].as_str().is_some_and(|t| !t.is_empty()));

    assert!(mot_de_passe_est(&state, "nouveau-mot-de-passe"));
    assert!(!mot_de_passe_est(&state, "ancien-mot-de-passe"));
    // Sessions révoquées : nouveau secret, l'ancien jeton ne passe plus.
    assert_ne!(
        settings(&state).get("jwt_secret").unwrap().as_deref(),
        Some(ANCIEN_SECRET)
    );
    let (st, _) = appel(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::GET,
        "/api/v1/auth/me",
        &[("authorization", &bearer)],
        None,
    )
    .await;
    assert_eq!(
        st,
        StatusCode::UNAUTHORIZED,
        "l'ancienne session est révoquée"
    );
    // Le jeton rendu, lui, ouvre une session.
    let nouveau = format!("Bearer {}", v["token"].as_str().unwrap());
    let (st, _) = appel(
        &state,
        AUTRE_HOTE_DU_LAN,
        Method::GET,
        "/api/v1/auth/me",
        &[("authorization", &nouveau)],
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    // Le code est consommé : il ne sert pas deux fois.
    let (st, v) = reinitialiser(&state, CETTE_MACHINE, &[], &code, "encore-un-autre").await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"], "reset_code_invalid");
    assert!(mot_de_passe_est(&state, "nouveau-mot-de-passe"));
}

#[tokio::test]
async fn compte_renomme_admin_accepte() {
    let _env = ENVIRONNEMENT.lock().await;
    let state = etat(":memory:");
    state
        .backend
        .execute("UPDATE profiles SET username = 'admin' WHERE id = 1", &[])
        .unwrap();
    let code = ra::emettre_un_code(&settings(&state)).unwrap();
    let (st, v) = reinitialiser(&state, CETTE_MACHINE, &[], &code, "nouveau-mot-de-passe").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["username"], "admin");
}

#[tokio::test]
async fn code_faux_puis_cinq_essais_bloque() {
    let _env = ENVIRONNEMENT.lock().await;
    let state = etat(":memory:");
    let code = ra::emettre_un_code(&settings(&state)).unwrap();
    let faux = code_faux(&code);
    for restants in (1..ra::ESSAIS_CODE).rev() {
        let (st, v) =
            reinitialiser(&state, CETTE_MACHINE, &[], &faux, "nouveau-mot-de-passe").await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
        assert_eq!(v["error"], "reset_code_invalid");
        // Rien ne distingue un code faux d'une absence de code.
        assert!(v.get("remaining_attempts").is_none(), "{v}");
        assert_eq!(
            settings(&state).get(ra::CLE_ESSAIS).unwrap().as_deref(),
            Some((ra::ESSAIS_CODE - restants).to_string().as_str())
        );
    }
    let (st, v) = reinitialiser(&state, CETTE_MACHINE, &[], &faux, "nouveau-mot-de-passe").await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "5e essai : {v}");
    assert_eq!(v["error"], "reset_attempts_exceeded");
    // Le code est détruit : même juste, il ne passe plus.
    let (st, v) = reinitialiser(&state, CETTE_MACHINE, &[], &code, "nouveau-mot-de-passe").await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"], "reset_code_invalid");
    assert!(mot_de_passe_est(&state, "ancien-mot-de-passe"));
    assert_eq!(
        settings(&state).get(ra::CLE_EMPREINTE).unwrap(),
        None,
        "empreinte effacée"
    );
}

#[tokio::test]
async fn code_expire_refuse() {
    let _env = ENVIRONNEMENT.lock().await;
    let state = etat(":memory:");
    let code = ra::emettre_un_code(&settings(&state)).unwrap();
    let il_y_a_16_min = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - 16 * 60;
    settings(&state)
        .set(ra::CLE_EMIS, &il_y_a_16_min.to_string())
        .unwrap();
    let (st, v) = reinitialiser(&state, CETTE_MACHINE, &[], &code, "nouveau-mot-de-passe").await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"], "reset_code_invalid");
    assert!(mot_de_passe_est(&state, "ancien-mot-de-passe"));
}

#[tokio::test]
async fn sans_code_emis_refuse() {
    let _env = ENVIRONNEMENT.lock().await;
    let state = etat(":memory:");
    let (st, v) = reinitialiser(
        &state,
        CETTE_MACHINE,
        &[],
        "ABCDEFGHJK",
        "nouveau-mot-de-passe",
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert!(mot_de_passe_est(&state, "ancien-mot-de-passe"));
    // La même réponse, mot pour mot, que pour un code faux.
    let code = ra::emettre_un_code(&settings(&state)).unwrap();
    let (st_faux, v_faux) = reinitialiser(
        &state,
        CETTE_MACHINE,
        &[],
        &code_faux(&code),
        "nouveau-mot-de-passe",
    )
    .await;
    assert_eq!((st, &v), (st_faux, &v_faux));
}

#[tokio::test]
async fn mot_de_passe_trop_court_ne_brule_pas_le_code() {
    let _env = ENVIRONNEMENT.lock().await;
    let state = etat(":memory:");
    let code = ra::emettre_un_code(&settings(&state)).unwrap();
    let (st, v) = reinitialiser(&state, CETTE_MACHINE, &[], &code, "court").await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(
        settings(&state).get(ra::CLE_ESSAIS).unwrap().as_deref(),
        Some("0")
    );
    let (st, _) = reinitialiser(&state, CETTE_MACHINE, &[], &code, "assez-long-cette-fois").await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn hors_machine_proxy_et_relais_refuses() {
    let _env = ENVIRONNEMENT.lock().await;
    let state = etat(":memory:");
    let code = ra::emettre_un_code(&settings(&state)).unwrap();
    let cas: [CasHorsMachine; 4] = [
        (AUTRE_HOTE_DU_LAN, &[], "autre hôte du réseau local"),
        (
            CETTE_MACHINE,
            &[("x-forwarded-for", "192.168.1.50")],
            "X-Forwarded-For",
        ),
        (
            CETTE_MACHINE,
            &[("forwarded", "for=192.168.1.50")],
            "Forwarded",
        ),
        (CETTE_MACHINE, &[("x-tune-relais", "1")], "relais distant"),
    ];
    for (adresse, entetes, quoi) in cas {
        let (st, v) = reinitialiser(&state, adresse, entetes, &code, "nouveau-mot-de-passe").await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{quoi} : {v}");
        // Le relais distant est arrêté plus tôt encore, par la garde du
        // relais (`not_available_remotely`) : la route n'est jamais atteinte.
        assert!(
            v["error"] == "reset_not_local" || v["error"] == "not_available_remotely",
            "{quoi} : {v}"
        );
    }
    assert!(mot_de_passe_est(&state, "ancien-mot-de-passe"));
    // Un refus hors machine ne compte pas d'essai et ne consomme rien.
    assert_eq!(
        settings(&state).get(ra::CLE_ESSAIS).unwrap().as_deref(),
        Some("0")
    );
    let (st, v) = reinitialiser(&state, CETTE_MACHINE, &[], &code, "nouveau-mot-de-passe").await;
    assert_eq!(st, StatusCode::OK, "{v}");
}

#[tokio::test]
async fn relais_du_port_80_de_tune_os_refuse() {
    let _env = ENVIRONNEMENT.lock().await;
    let state = etat(":memory:");
    let code = ra::emettre_un_code(&settings(&state)).unwrap();
    // Appliance + relais du port 80 : une connexion du réseau local arrive
    // de 127.0.0.1, le bouclage ne prouve plus rien.
    // SAFETY : les épreuves de ce fichier qui lisent l'environnement sont
    // sérialisées par `ENVIRONNEMENT`.
    unsafe {
        std::env::set_var("TUNE_APPLIANCE", "1");
        std::env::set_var("TUNE_WEB80_RELAIS", "1");
    }
    let (st, v) = reinitialiser(&state, CETTE_MACHINE, &[], &code, "nouveau-mot-de-passe").await;
    unsafe {
        std::env::remove_var("TUNE_APPLIANCE");
        std::env::remove_var("TUNE_WEB80_RELAIS");
    }
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"], "reset_not_local");
    assert!(mot_de_passe_est(&state, "ancien-mot-de-passe"));
}

#[tokio::test]
async fn le_code_n_est_jamais_ecrit_en_clair_dans_la_base() {
    let _env = ENVIRONNEMENT.lock().await;
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");
    let state = etat(base.to_str().unwrap());
    let code = ra::emettre_un_code(&settings(&state)).unwrap();
    assert_eq!(code.len(), 10);
    // Ni dans les réglages...
    for (cle, valeur) in settings(&state).all().unwrap() {
        assert!(!valeur.contains(&code), "réglage {cle} contient le code");
    }
    let empreinte = settings(&state).get(ra::CLE_EMPREINTE).unwrap().unwrap();
    assert!(empreinte.starts_with("$argon2"), "{empreinte}");
    // ... ni nulle part dans les fichiers de la base (WAL compris).
    for entree in std::fs::read_dir(dossier.path()).unwrap() {
        let chemin = entree.unwrap().path();
        if chemin.is_file() {
            let octets = std::fs::read(&chemin).unwrap();
            assert!(
                !octets.windows(code.len()).any(|w| w == code.as_bytes()),
                "{} contient le code en clair",
                chemin.display()
            );
        }
    }
}

#[tokio::test]
async fn la_route_est_publique_mais_seulement_en_post() {
    let _env = ENVIRONNEMENT.lock().await;
    let state = etat(":memory:");
    // Sans jeton, authentification active : la couche laisse passer, le
    // gestionnaire décide (ici : pas de code, 403 du gestionnaire).
    let (st, _) = reinitialiser(
        &state,
        CETTE_MACHINE,
        &[],
        "ABCDEFGHJK",
        "nouveau-mot-de-passe",
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _) = appel(&state, CETTE_MACHINE, Method::GET, ROUTE, &[], None).await;
    assert_ne!(st, StatusCode::OK);
}

#[tokio::test]
async fn sous_commande_sans_base_message_clair_et_rien_de_cree() {
    let dossier = tempfile::tempdir().unwrap();
    let absente = dossier.path().join("absente").join("tune.db");
    let config = tune_server::config::TuneConfig {
        db_path: absente.to_str().unwrap().to_string(),
        database_url: None,
        ..Default::default()
    };
    let Err(ra::BaseIntrouvable(message)) = ra::ouvrir_la_base(&config).await else {
        panic!("une base absente doit être refusée");
    };
    assert!(message.contains("Aucune base Tune"), "{message}");
    assert!(message.contains("No Tune database"), "{message}");
    assert!(message.contains("TUNE_DB_PATH"), "{message}");
    assert!(!absente.exists(), "la commande ne crée jamais de base");
}

#[tokio::test]
async fn sous_commande_emet_un_code_que_la_route_accepte() {
    let _env = ENVIRONNEMENT.lock().await;
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");
    let state = etat(base.to_str().unwrap());
    // Un second processus ouvre la même base, comme la sous-commande.
    let config = tune_server::config::TuneConfig {
        db_path: base.to_str().unwrap().to_string(),
        database_url: None,
        ..Default::default()
    };
    let backend = ra::ouvrir_la_base(&config).await.expect("base existante");
    let (id, nom) = ra::compte_administrateur(&backend).unwrap();
    assert_eq!((id, nom.as_str()), (1, "default"));
    let code = ra::emettre_un_code(&SettingsRepo::with_backend(backend)).unwrap();
    let (st, v) = reinitialiser(&state, CETTE_MACHINE, &[], &code, "nouveau-mot-de-passe").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(mot_de_passe_est(&state, "nouveau-mot-de-passe"));
}

#[tokio::test]
async fn mode_terminal_pose_le_mot_de_passe_et_revoque() {
    let state = etat(":memory:");
    ra::emettre_un_code(&settings(&state)).unwrap();
    settings(&state)
        .set("api_key", "cle-d-api-d-avant")
        .unwrap();
    settings(&state)
        .set("bridge_token", "jeton-du-pont")
        .unwrap();
    ra::poser_le_mot_de_passe(&state.backend, 1, "pose-dans-le-terminal").unwrap();
    assert!(mot_de_passe_est(&state, "pose-dans-le-terminal"));
    assert_ne!(
        settings(&state).get("jwt_secret").unwrap().as_deref(),
        Some(ANCIEN_SECRET)
    );
    assert_eq!(settings(&state).get(ra::CLE_EMPREINTE).unwrap(), None);
    // Comme annoncé dans le guide : clé d'API effacée, jeton du pont gardé.
    assert_eq!(settings(&state).get("api_key").unwrap(), None);
    assert_eq!(
        settings(&state).get("bridge_token").unwrap().as_deref(),
        Some("jeton-du-pont")
    );
}

/// Base occupée (ici : un déclencheur refuse l'écriture du secret) : la
/// transaction est annulée en entier. Ni mot de passe changé, ni succès
/// annoncé, et le code reste valable.
#[tokio::test]
async fn secret_non_ecrit_aucun_succes_et_rien_ne_change() {
    let _env = ENVIRONNEMENT.lock().await;
    let state = etat(":memory:");
    let code = ra::emettre_un_code(&settings(&state)).unwrap();
    state
        .backend
        .execute_batch(
            "CREATE TRIGGER secret_refuse_i BEFORE INSERT ON settings \
             WHEN NEW.key = 'jwt_secret' BEGIN SELECT RAISE(ABORT, 'base occupee'); END; \
             CREATE TRIGGER secret_refuse_u BEFORE UPDATE ON settings \
             WHEN NEW.key = 'jwt_secret' BEGIN SELECT RAISE(ABORT, 'base occupee'); END;",
        )
        .unwrap();
    let (st, v) = reinitialiser(&state, CETTE_MACHINE, &[], &code, "nouveau-mot-de-passe").await;
    assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{v}");
    assert_eq!(v["error"], "reset_failed");
    assert!(v.get("token").is_none(), "{v}");
    assert!(
        mot_de_passe_est(&state, "ancien-mot-de-passe"),
        "annulé en entier"
    );
    assert_eq!(
        settings(&state).get("jwt_secret").unwrap().as_deref(),
        Some(ANCIEN_SECRET)
    );
    assert!(
        ra::poser_le_mot_de_passe(&state.backend, 1, "autre-mot-de-passe").is_err(),
        "le mode terminal ne s'annonce pas réussi non plus"
    );
    assert!(mot_de_passe_est(&state, "ancien-mot-de-passe"));
    // La base libérée, le même code passe.
    state
        .backend
        .execute_batch("DROP TRIGGER secret_refuse_i; DROP TRIGGER secret_refuse_u;")
        .unwrap();
    let (st, v) = reinitialiser(&state, CETTE_MACHINE, &[], &code, "nouveau-mot-de-passe").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(mot_de_passe_est(&state, "nouveau-mot-de-passe"));
}

#[tokio::test]
async fn requete_sans_host_refusee() {
    let _env = ENVIRONNEMENT.lock().await;
    let state = etat(":memory:");
    let code = ra::emettre_un_code(&settings(&state)).unwrap();
    let (st, v) = reinitialiser(
        &state,
        CETTE_MACHINE,
        &[("host", "")],
        &code,
        "nouveau-mot-de-passe",
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"], "reset_not_local");
    assert!(mot_de_passe_est(&state, "ancien-mot-de-passe"));
}

/// M1 — le mode terminal change le secret depuis un AUTRE processus : la
/// veille du service le voit et ferme les WebSocket ouvertes.
#[tokio::test]
async fn veille_du_secret_ferme_les_websockets_apres_le_mode_terminal() {
    let _env = ENVIRONNEMENT.lock().await;
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");
    let state = etat(base.to_str().unwrap());
    let mut abonne = state.fermeture_websockets.subscribe();
    abonne.borrow_and_update();
    let veille = tune_server::auth::surveiller_le_secret(
        state.clone(),
        std::time::Duration::from_millis(50),
    );
    // Rien ne change : aucune fermeture.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(300), abonne.changed())
            .await
            .is_err(),
        "aucune fermeture sans changement de secret"
    );
    // La commande, comme un second processus : sa propre connexion.
    let config = tune_server::config::TuneConfig {
        db_path: base.to_str().unwrap().to_string(),
        database_url: None,
        ..Default::default()
    };
    let autre = ra::ouvrir_la_base(&config).await.unwrap();
    ra::poser_le_mot_de_passe(&autre, 1, "pose-dans-le-terminal").unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), abonne.changed())
        .await
        .expect("les WebSocket doivent être fermées")
        .unwrap();
    veille.abort();
}

/// La route ferme elle-même les WebSocket : la veille n'en ajoute pas une
/// seconde.
#[tokio::test]
async fn veille_du_secret_ne_double_pas_la_fermeture_de_la_route() {
    let _env = ENVIRONNEMENT.lock().await;
    let state = etat(":memory:");
    let mut veille = tune_server::auth::VeilleDuSecret::new(&state);
    let code = ra::emettre_un_code(&settings(&state)).unwrap();
    let avant = *state.fermeture_websockets.borrow();
    let (st, _) = reinitialiser(&state, CETTE_MACHINE, &[], &code, "nouveau-mot-de-passe").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(*state.fermeture_websockets.borrow(), avant + 1);
    assert!(!veille.verifier(&state));
    assert_eq!(*state.fermeture_websockets.borrow(), avant + 1);
    // Un changement venu d'ailleurs, lui, ferme.
    ra::poser_le_mot_de_passe(&state.backend, 1, "pose-dans-le-terminal").unwrap();
    assert!(veille.verifier(&state));
    assert_eq!(*state.fermeture_websockets.borrow(), avant + 2);
}

#[test]
fn installeur_windows_a_son_lanceur_et_son_entree() {
    let racine = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let yml = std::fs::read_to_string(racine.join(".github/workflows/release.yml")).unwrap();
    assert!(yml.contains("reinitialiser-admin.bat\" w"), "lanceur écrit");
    assert!(yml.contains("'tune-server.exe --reinitialiser-admin$\\r$\\n'"));
    assert!(yml.contains("'pause$\\r$\\n'"), "la fenêtre reste ouverte");
    assert!(
        yml.lines().any(|l| l.trim().starts_with("CreateShortcut")
            && l.contains("Mot de passe administrateur oublié.lnk")
            && l.contains("reinitialiser-admin.bat")),
        "entrée du menu Démarrer"
    );
}

/// Une lecture du secret en ERREUR (base occupée, coupure PostgreSQL) n'est
/// pas un secret changé : aucune WebSocket n'est fermée, celles du pont
/// comprises, et le passage suivant réessaie.
#[tokio::test]
async fn veille_du_secret_erreur_de_lecture_aucune_fermeture() {
    let _env = ENVIRONNEMENT.lock().await;
    let state = etat(":memory:");
    let mut veille = tune_server::auth::VeilleDuSecret::new(&state);
    let avant = *state.fermeture_websockets.borrow();
    // La table des réglages devient illisible : `get` rend `Err`.
    state
        .backend
        .execute_batch("ALTER TABLE settings RENAME TO settings_hors_ligne")
        .unwrap();
    assert!(
        SettingsRepo::with_backend(state.backend.clone())
            .get("jwt_secret")
            .is_err(),
        "la lecture doit échouer pour que l'épreuve porte"
    );
    for _ in 0..3 {
        assert!(!veille.verifier(&state));
    }
    assert_eq!(
        *state.fermeture_websockets.borrow(),
        avant,
        "aucune fermeture"
    );
    // La base revient, secret inchangé : toujours rien.
    state
        .backend
        .execute_batch("ALTER TABLE settings_hors_ligne RENAME TO settings")
        .unwrap();
    assert!(!veille.verifier(&state));
    assert_eq!(*state.fermeture_websockets.borrow(), avant);
    // Contre-épreuve : un vrai changement ferme toujours.
    ra::poser_le_mot_de_passe(&state.backend, 1, "pose-dans-le-terminal").unwrap();
    assert!(veille.verifier(&state));
    assert_eq!(*state.fermeture_websockets.borrow(), avant + 1);
}

// ---------------------------------------------------------------------------
// Condition de la revue de sécurité (#9564 × #9445/#9569) : la récupération
// de l'accès administrateur révoque aussi les sessions longues et les jetons
// de renouvellement, dans la même transaction que le mot de passe.
// ---------------------------------------------------------------------------

/// Un hôte distant : la session longue (`remember`) et son jeton de
/// renouvellement y sont rendus, et rien n'y est dispensé de jeton.
const DISTANT: &str = "203.0.113.7:44444";

fn compter(state: &AppState, sql: &str) -> i64 {
    state
        .backend
        .query_one(sql, &[])
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_i64()))
        .unwrap_or(-1)
}

const FAMILLES_OUVERTES: &str =
    "SELECT COUNT(*) FROM jetons_de_renouvellement WHERE revoque_le IS NULL";
const SESSIONS_INSCRITES: &str =
    "SELECT COUNT(*) FROM settings WHERE key LIKE 'auth_session_token:%'";

#[tokio::test]
async fn reinitialisation_refuse_l_ancien_jeton_d_acces_et_de_renouvellement() {
    let _env = ENVIRONNEMENT.lock().await;
    for par_la_route in [true, false] {
        let state = etat(":memory:");
        let (_, nom) = ra::compte_administrateur(&state.backend).unwrap();
        let connexion = serde_json::json!({
            "username": nom,
            "password": "ancien-mot-de-passe",
            "remember": true,
        })
        .to_string();
        let (st, v) = appel(
            &state,
            DISTANT,
            Method::POST,
            "/api/v1/auth/login",
            &[("host", "")],
            Some(connexion),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "connexion : {v}");
        let acces = v["token"].as_str().expect("jeton d'accès").to_string();
        let renouvellement = v["refresh_token"]
            .as_str()
            .expect("jeton de renouvellement")
            .to_string();
        let bearer = format!("Bearer {acces}");
        let moi = |state: AppState, bearer: String| async move {
            appel(
                &state,
                DISTANT,
                Method::GET,
                "/api/v1/auth/me",
                &[("host", ""), ("authorization", bearer.as_str())],
                None,
            )
            .await
            .0
        };
        // Contre-épreuve : avant la réinitialisation, la session vit.
        assert_eq!(moi(state.clone(), bearer.clone()).await, StatusCode::OK);
        assert!(compter(&state, FAMILLES_OUVERTES) >= 1);
        assert!(compter(&state, SESSIONS_INSCRITES) >= 1);

        if par_la_route {
            let code = ra::emettre_un_code(&settings(&state)).unwrap();
            let (st, v) =
                reinitialiser(&state, CETTE_MACHINE, &[], &code, "nouveau-mot-de-passe").await;
            assert_eq!(st, StatusCode::OK, "{v}");
        } else {
            ra::poser_le_mot_de_passe(&state.backend, 1, "pose-dans-le-terminal").unwrap();
        }

        // La révocation est écrite, pas seulement déduite du mot de passe.
        assert_eq!(compter(&state, FAMILLES_OUVERTES), 0, "familles révoquées");
        assert_eq!(compter(&state, SESSIONS_INSCRITES), 0, "sessions désinscrites");
        assert_eq!(
            moi(state.clone(), bearer.clone()).await,
            StatusCode::UNAUTHORIZED,
            "ancien jeton d'accès refusé (route : {par_la_route})"
        );
        let (st, v) = appel(
            &state,
            DISTANT,
            Method::POST,
            "/api/v1/auth/refresh",
            &[("host", "")],
            Some(serde_json::json!({ "refresh_token": renouvellement }).to_string()),
        )
        .await;
        assert_eq!(
            st,
            StatusCode::UNAUTHORIZED,
            "ancien jeton de renouvellement refusé (route : {par_la_route}) : {v}"
        );
    }
}
