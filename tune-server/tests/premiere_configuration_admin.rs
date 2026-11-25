//! Première configuration : le mot de passe administrateur se crée dans
//! l'assistant, sans le code du journal, à quatre conditions — aucun
//! administrateur avec mot de passe, fenêtre de première configuration
//! ouverte (installation neuve), appel de cette machine ou du réseau local,
//! une seule fois. Hors de ces conditions, le code du journal reste la voie.

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;

use tune_server::auth::{
    DUREE_PREMIERE_CONFIGURATION, base_neuve_avant_ouverture,
    oublier_l_horloge_de_la_premiere_configuration, ouvrir_la_premiere_configuration,
    premiere_configuration_restante, vieillir_la_premiere_configuration,
};
use tune_server::state::AppState;

const CETTE_MACHINE: &str = "127.0.0.1:50000";
const HOTE_DU_LAN: &str = "192.168.1.50:50000";
const AUTRE_HOTE_DU_LAN: &str = "192.168.1.51:50000";
const INTERNET: &str = "203.0.113.5:50000";
const CGNAT: &str = "100.64.1.2:50000";

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

/// Une installation neuve dont la fenêtre vient de s'ouvrir.
fn installation_neuve() -> AppState {
    let state = new_state();
    assert!(ouvrir_la_premiere_configuration(&state));
    state
}

fn active(state: &AppState) -> bool {
    tune_server::auth::authentification_active(state)
}

fn mot_de_passe_admin(state: &AppState) -> Option<String> {
    state
        .backend
        .query_one("SELECT password_hash_v2 FROM profiles WHERE id = 1", &[])
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_string()))
        .filter(|h| !h.is_empty())
}

async fn appel(
    state: &AppState,
    adresse: &str,
    methode: Method,
    chemin: &str,
    entetes: &[(&str, &str)],
    corps: Option<&str>,
) -> (StatusCode, Value) {
    let pair: SocketAddr = adresse.parse().unwrap();
    let app: Router = tune_server::routes::router(state.clone()).layer(MockConnectInfo(pair));
    let mut req = Request::builder().method(methode).uri(chemin);
    for (n, v) in entetes {
        req = req.header(*n, *v);
    }
    let body = match corps {
        Some(c) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(c.to_string())
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

async fn etat(state: &AppState, adresse: &str) -> Value {
    let (st, v) = appel(
        state,
        adresse,
        Method::GET,
        "/api/v1/auth/activation",
        &[],
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v
}

async fn creer(
    state: &AppState,
    adresse: &str,
    entetes: &[(&str, &str)],
    mot_de_passe: &str,
) -> (StatusCode, Value) {
    let corps = format!(r#"{{"password":"{mot_de_passe}"}}"#);
    appel(
        state,
        adresse,
        Method::POST,
        "/api/v1/auth/activation",
        entetes,
        Some(&corps),
    )
    .await
}

/// Refus attendu : rien n'est créé, rien n'est activé.
fn rien_n_a_change(state: &AppState) {
    assert!(!active(state));
    assert_eq!(mot_de_passe_admin(state), None);
}

// --- création permise ------------------------------------------------------

#[tokio::test]
async fn creation_depuis_le_reseau_local_dans_la_fenetre() {
    let state = installation_neuve();
    let v = etat(&state, HOTE_DU_LAN).await;
    assert_eq!(v["first_setup"], true, "{v}");
    assert_eq!(v["code_required"], false, "{v}");
    let reste = v["first_setup_expires_in"].as_u64().unwrap();
    assert!(reste > 0 && reste <= DUREE_PREMIERE_CONFIGURATION.as_secs());
    assert_eq!(v["min_password_length"], 8);
    assert_eq!(v["max_password_length"], 256);

    let (st, v) = creer(&state, HOTE_DU_LAN, &[], "proprio-1234").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["auth_enabled"], true);
    assert_eq!(v["role"], "admin");
    assert!(v["token"].as_str().is_some_and(|t| !t.is_empty()));
    assert!(active(&state));
    let hachage = mot_de_passe_admin(&state).expect("mot de passe enregistré");
    assert!(tune_server::auth::verify_password("proprio-1234", &hachage));

    // La fenêtre a servi : fermée pour de bon.
    assert_eq!(premiere_configuration_restante(&state), None);
}

#[tokio::test]
async fn creation_avec_l_origine_du_serveur() {
    // Le navigateur de l'assistant : même origine que l'hôte joint.
    let state = installation_neuve();
    let (st, v) = creer(
        &state,
        HOTE_DU_LAN,
        &[
            ("host", "192.168.1.20:8888"),
            ("origin", "http://192.168.1.20:8888"),
            ("sec-fetch-site", "same-origin"),
        ],
        "proprio-1234",
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(active(&state));
}

#[tokio::test]
async fn la_fenetre_survit_a_un_redemarrage_rapide() {
    // Après un redémarrage, l'horloge monotone est perdue : l'heure
    // enregistrée prend le relais.
    let state = installation_neuve();
    oublier_l_horloge_de_la_premiere_configuration(&state);
    vieillir_la_premiere_configuration(&state, Duration::from_secs(10 * 60));
    assert!(premiere_configuration_restante(&state).is_some());
    let (st, v) = creer(&state, HOTE_DU_LAN, &[], "proprio-1234").await;
    assert_eq!(st, StatusCode::OK, "{v}");
}

// --- refus -----------------------------------------------------------------

#[tokio::test]
async fn refus_hors_fenetre() {
    let state = installation_neuve();
    vieillir_la_premiere_configuration(
        &state,
        DUREE_PREMIERE_CONFIGURATION + Duration::from_secs(1),
    );
    let v = etat(&state, HOTE_DU_LAN).await;
    assert_eq!(v["first_setup"], false, "{v}");
    assert_eq!(v["code_required"], true, "{v}");
    let (st, v) = creer(&state, HOTE_DU_LAN, &[], "intrus-1234").await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(v["error"], "activation_code_required", "{v}");
    rien_n_a_change(&state);
}

#[tokio::test]
async fn refus_hors_fenetre_apres_redemarrage() {
    let state = installation_neuve();
    oublier_l_horloge_de_la_premiere_configuration(&state);
    vieillir_la_premiere_configuration(
        &state,
        DUREE_PREMIERE_CONFIGURATION + Duration::from_secs(60),
    );
    let (st, _) = creer(&state, HOTE_DU_LAN, &[], "intrus-1234").await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    rien_n_a_change(&state);
}

#[tokio::test]
async fn refus_sur_une_installation_existante() {
    // Base déjà présente au démarrage : la fenêtre ne s'ouvre jamais.
    let state = new_state();
    assert_eq!(premiere_configuration_restante(&state), None);
    let v = etat(&state, HOTE_DU_LAN).await;
    assert_eq!(v["first_setup"], false, "{v}");
    assert_eq!(v["code_required"], true, "{v}");
    let (st, v) = creer(&state, HOTE_DU_LAN, &[], "intrus-1234").await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    rien_n_a_change(&state);
}

#[tokio::test]
async fn refus_depuis_une_adresse_hors_du_reseau_local() {
    let state = installation_neuve();
    for adresse in [INTERNET, CGNAT, "[2001:db8::5]:50000"] {
        let v = etat(&state, adresse).await;
        assert_eq!(v["first_setup"], false, "{adresse} {v}");
        assert_eq!(v["code_required"], true, "{adresse} {v}");
        let (st, v) = creer(&state, adresse, &[], "intrus-1234").await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{adresse} {v}");
        rien_n_a_change(&state);
    }
}

#[tokio::test]
async fn refus_par_le_relais_ou_l_ecoute_directe() {
    // Rejouée en local (vue de 127.0.0.1), émise ailleurs.
    let state = installation_neuve();
    for adresse in [CETTE_MACHINE, HOTE_DU_LAN] {
        let (st, v) = creer(&state, adresse, &[("x-tune-relais", "1")], "intrus-1234").await;
        assert!(
            st == StatusCode::FORBIDDEN || st == StatusCode::NOT_FOUND,
            "{adresse} {st} {v}"
        );
        rien_n_a_change(&state);
    }
}

#[tokio::test]
async fn refus_derriere_un_mandataire() {
    let state = installation_neuve();
    for entete in [
        ("x-forwarded-for", "192.168.1.9"),
        ("x-real-ip", "192.168.1.9"),
        ("forwarded", "for=192.168.1.9"),
        ("via", "1.1 proxy"),
    ] {
        for adresse in [CETTE_MACHINE, HOTE_DU_LAN] {
            let v = {
                let (_, v) = appel(
                    &state,
                    adresse,
                    Method::GET,
                    "/api/v1/auth/activation",
                    &[entete],
                    None,
                )
                .await;
                v
            };
            assert_eq!(v["first_setup"], false, "{entete:?} {adresse} {v}");
            let (st, v) = creer(&state, adresse, &[entete], "intrus-1234").await;
            assert_eq!(st, StatusCode::FORBIDDEN, "{entete:?} {adresse} {v}");
            rien_n_a_change(&state);
        }
    }
}

#[tokio::test]
async fn refus_d_une_requete_inter_sites() {
    // Une page d'un autre site ouverte sur le LAN ne crée pas l'administrateur.
    let state = installation_neuve();
    let (st, v) = creer(
        &state,
        HOTE_DU_LAN,
        &[
            ("host", "192.168.1.20:8888"),
            ("origin", "http://evil.example"),
            ("sec-fetch-site", "cross-site"),
        ],
        "intrus-1234",
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    rien_n_a_change(&state);
}

#[tokio::test]
async fn refus_si_un_administrateur_existe_deja() {
    let state = installation_neuve();
    // Un mot de passe administrateur déjà posé (authentification coupée).
    let hachage = tune_server::auth::hash_password("deja-1234").unwrap();
    {
        use tune_core::db::backend::ToSqlValue;
        state
            .backend
            .execute(
                "UPDATE profiles SET password_hash_v2 = ? WHERE id = 1",
                &[&hachage as &dyn ToSqlValue],
            )
            .unwrap();
    }
    assert_eq!(premiere_configuration_restante(&state), None);
    let v = etat(&state, HOTE_DU_LAN).await;
    assert_eq!(v["first_setup"], false, "{v}");
    let (st, v) = creer(&state, HOTE_DU_LAN, &[], "intrus-1234").await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{v}");
    assert!(!active(&state));
    assert_eq!(
        mot_de_passe_admin(&state).as_deref(),
        Some(hachage.as_str())
    );
}

#[tokio::test]
async fn une_seule_fois_meme_apres_desactivation() {
    let state = installation_neuve();
    let (st, v) = creer(&state, HOTE_DU_LAN, &[], "proprio-1234").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let jeton = v["token"].as_str().unwrap().to_string();
    // L'administrateur désactive l'authentification dans la fenêtre.
    let auth = format!("Bearer {jeton}");
    let (st, v) = appel(
        &state,
        HOTE_DU_LAN,
        Method::POST,
        "/api/v1/auth/config",
        &[("authorization", auth.as_str())],
        Some(r#"{"auth_enabled":false}"#),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(!active(&state));
    // La fenêtre ne se rouvre pas : un tiers du LAN n'y prend pas la main.
    assert_eq!(premiere_configuration_restante(&state), None);
    let (st, _) = creer(&state, AUTRE_HOTE_DU_LAN, &[], "intrus-1234").await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(!active(&state));
    let hachage = mot_de_passe_admin(&state).unwrap();
    assert!(tune_server::auth::verify_password("proprio-1234", &hachage));
    // Et une seconde ouverture au démarrage est sans effet.
    assert!(!ouvrir_la_premiere_configuration(&state));
}

#[tokio::test]
async fn longueurs_du_mot_de_passe() {
    let state = installation_neuve();
    let (st, v) = creer(&state, HOTE_DU_LAN, &[], "court").await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"], "password_too_short");
    let long = "x".repeat(257);
    let (st, v) = creer(&state, HOTE_DU_LAN, &[], &long).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"], "password_too_long");
    rien_n_a_change(&state);
    // La fenêtre n'est pas brûlée par un refus de forme.
    assert!(premiere_configuration_restante(&state).is_some());
}

// --- course ----------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deux_navigateurs_en_meme_temps_un_seul_gagne() {
    for _ in 0..5 {
        let state = installation_neuve();
        let mut taches = Vec::new();
        for (i, adresse) in [
            HOTE_DU_LAN,
            AUTRE_HOTE_DU_LAN,
            HOTE_DU_LAN,
            AUTRE_HOTE_DU_LAN,
        ]
        .into_iter()
        .enumerate()
        {
            let state = state.clone();
            taches.push(tokio::spawn(async move {
                let mdp = format!("navigateur-{i}-1234");
                let (st, _) = creer(&state, adresse, &[], &mdp).await;
                (st, mdp)
            }));
        }
        let mut gagnants = Vec::new();
        for t in taches {
            let (st, mdp) = t.await.unwrap();
            if st == StatusCode::OK {
                gagnants.push(mdp);
            } else {
                assert!(
                    st == StatusCode::CONFLICT || st == StatusCode::FORBIDDEN,
                    "perdant : {st}"
                );
            }
        }
        assert_eq!(gagnants.len(), 1, "un seul gagnant : {gagnants:?}");
        let hachage = mot_de_passe_admin(&state).unwrap();
        assert!(tune_server::auth::verify_password(&gagnants[0], &hachage));
    }
}

// --- repli -----------------------------------------------------------------

#[tokio::test]
async fn le_code_du_journal_marche_toujours_hors_fenetre() {
    let state = installation_neuve();
    vieillir_la_premiere_configuration(
        &state,
        DUREE_PREMIERE_CONFIGURATION + Duration::from_secs(1),
    );
    let _ = tune_server::auth::emettre_code_activation(&state);
    let code = tune_server::auth::code_activation_en_cours(&state).unwrap();
    let corps = format!(r#"{{"password":"proprio-1234","code":"{code}"}}"#);
    let (st, v) = appel(
        &state,
        INTERNET,
        Method::POST,
        "/api/v1/auth/activation",
        &[],
        Some(&corps),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(active(&state));
}

#[tokio::test]
async fn cette_machine_reste_sans_code_hors_fenetre() {
    let state = new_state();
    let (st, v) = creer(&state, CETTE_MACHINE, &[], "proprio-1234").await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(active(&state));
}

// --- démarrage -------------------------------------------------------------

#[test]
fn seule_une_base_absente_est_neuve() {
    let dossier = std::env::temp_dir().join(format!(
        "tune-premiere-configuration-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dossier).unwrap();
    let base = dossier.join("tune.db");
    let _ = std::fs::remove_file(&base);
    let chemin = base.to_str().unwrap();
    assert!(base_neuve_avant_ouverture(chemin, None));
    assert!(base_neuve_avant_ouverture(chemin, Some("sqlite://tune.db")));
    // PostgreSQL : jamais neuve, le code reste la voie.
    assert!(!base_neuve_avant_ouverture(
        chemin,
        Some("postgres://tune@localhost/tune")
    ));
    assert!(!base_neuve_avant_ouverture(":memory:", None));
    std::fs::write(&base, b"").unwrap();
    assert!(!base_neuve_avant_ouverture(chemin, None));
    let _ = std::fs::remove_dir_all(&dossier);
}

#[test]
fn les_cles_de_la_fenetre_sont_protegees() {
    // L'écriture générique des réglages ne rouvre pas la fenêtre.
    for cle in [
        tune_server::auth::CLE_DEBUT_PREMIERE_CONFIGURATION,
        tune_server::auth::CLE_FIN_PREMIERE_CONFIGURATION,
    ] {
        assert!(tune_core::reglages_proteges::est_protege(cle), "{cle}");
    }
}

#[tokio::test]
async fn le_patch_generique_ne_rouvre_pas_la_fenetre() {
    let state = new_state();
    let corps = format!(
        r#"{{"{}":"{}"}}"#,
        tune_server::auth::CLE_DEBUT_PREMIERE_CONFIGURATION,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    );
    let _ = appel(
        &state,
        HOTE_DU_LAN,
        Method::PATCH,
        "/api/v1/system/config",
        &[],
        Some(&corps),
    )
    .await;
    assert_eq!(premiere_configuration_restante(&state), None);
    let (st, _) = creer(&state, HOTE_DU_LAN, &[], "intrus-1234").await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    rien_n_a_change(&state);
}
