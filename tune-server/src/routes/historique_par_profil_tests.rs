//! L'historique d'écoute par profil, rejoué par le VRAI routeur
//! (`crate::routes::router`, préfixe `/api/v1`), l'adresse de l'appelant posée
//! en extension `ConnectInfo` comme le fait le service de production.
//!
//! - liste et export : les écoutes du profil appelant et celles sans profil,
//!   jamais celles d'un autre profil (les deux montages, `/history` et
//!   `/library/history`) ;
//! - tableau de bord, carte de chaleur, historique d'un profil nommé : un
//!   autre profil est refusé au compte standard, servi à l'administrateur ;
//!   sans profil demandé, le compte standard reçoit le sien ;
//! - effacement : le profil appelant seulement ; tout le foyer
//!   (`?scope=household`) pour l'administrateur ou cette machine, jamais par
//!   le relais.

use std::net::SocketAddr;

use axum::body::{Body, to_bytes};
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::backend::ToSqlValue;
use tune_core::db::profile_repo::ProfileRepo;
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

const LAN: &str = "192.168.1.50:51000";
const ICI: &str = "127.0.0.1:51000";

struct Banc {
    state: AppState,
    /// Le profil de l'appelant (compte standard quand l'authentification est
    /// active) et l'autre profil du foyer.
    moi: i64,
    autre: i64,
    jeton_admin: Option<String>,
    jeton_moi: Option<String>,
    _dossier: tempfile::TempDir,
}

/// Un foyer de deux profils : « moi » a écouté `Mien`, l'autre `Secret`, et
/// une écoute sans profil (`Commun`).
fn banc(authentification: bool) -> Banc {
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");
    let state = AppState::new(base.to_str().unwrap(), 0, Default::default()).unwrap();
    let profils = ProfileRepo::with_backend(state.backend.clone());
    let admin = profils.create("admin", Some("Admin"), None).unwrap();
    let moi = profils.create("moi", Some("Moi"), None).unwrap();
    let autre = profils.create("autre", Some("Autre"), None).unwrap();
    let (jeton_admin, jeton_moi) = if authentification {
        let reglages = SettingsRepo::with_backend(state.backend.clone());
        reglages.set("jwt_secret", "secret-de-banc").unwrap();
        reglages.set("auth_enabled", "true").unwrap();
        (
            Some(crate::auth::sign_jwt(admin, "admin", "secret-de-banc").unwrap()),
            Some(crate::auth::sign_jwt(moi, "user", "secret-de-banc").unwrap()),
        )
    } else {
        (None, None)
    };
    let ecouter = |titre: &str, profil: Option<i64>| {
        state
            .backend
            .execute(
                "INSERT INTO listen_history (title, artist_name, album_title, source, duration_ms, profile_id) \
                 VALUES (?, 'Interprète', 'Album', 'local', 180000, ?)",
                &[&titre.to_string() as &dyn ToSqlValue, &profil],
            )
            .unwrap();
    };
    ecouter("Mien", Some(moi));
    ecouter("Secret", Some(autre));
    ecouter("Secret bis", Some(autre));
    ecouter("Commun", None);
    Banc {
        state,
        moi,
        autre,
        jeton_admin,
        jeton_moi,
        _dossier: dossier,
    }
}

struct Appel<'a> {
    depuis: &'a str,
    jeton: Option<&'a str>,
    profil: Option<i64>,
    relayee: bool,
}

async fn appel(b: &Banc, a: Appel<'_>, methode: &str, uri: &str) -> (StatusCode, String) {
    let adresse: SocketAddr = a.depuis.parse().unwrap();
    let mut requete = Request::builder()
        .method(methode)
        .uri(uri)
        .extension(ConnectInfo(adresse));
    if let Some(j) = a.jeton {
        requete = requete.header(header::AUTHORIZATION, format!("Bearer {j}"));
    }
    if let Some(p) = a.profil {
        requete = requete.header("X-Profile-Id", p.to_string());
    }
    if a.relayee {
        requete = requete.header(tune_core::cloud::ENTETE_RELAIS, "1");
    }
    let reponse = crate::routes::router(b.state.clone())
        .oneshot(requete.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = to_bytes(reponse.into_body(), usize::MAX).await.unwrap();
    (statut, String::from_utf8_lossy(&octets).into_owned())
}

/// Le compte standard « moi » (authentification active), ou un appareil du
/// réseau qui a choisi « moi » (authentification coupée).
fn moi(b: &Banc) -> Appel<'_> {
    Appel {
        depuis: LAN,
        jeton: b.jeton_moi.as_deref(),
        profil: Some(b.moi),
        relayee: false,
    }
}

fn admin(b: &Banc) -> Appel<'_> {
    Appel {
        depuis: LAN,
        jeton: b.jeton_admin.as_deref(),
        profil: None,
        relayee: false,
    }
}

fn titres_restants(b: &Banc) -> Vec<String> {
    b.state
        .backend
        .query_many("SELECT title FROM listen_history ORDER BY title", &[])
        .unwrap()
        .iter()
        .filter_map(|c| c.first().and_then(|v| v.as_string()))
        .collect()
}

fn plays(corps: &str) -> i64 {
    let v: Value = serde_json::from_str(corps).unwrap_or(Value::Null);
    v["totals"]["plays"].as_i64().unwrap_or(-1)
}

#[tokio::test]
async fn la_liste_ne_montre_pas_l_historique_d_un_autre_profil() {
    for authentification in [false, true] {
        let b = banc(authentification);
        for uri in [
            "/api/v1/library/history?limit=50",
            "/api/v1/history?limit=50",
        ] {
            let (statut, corps) = appel(&b, moi(&b), "GET", uri).await;
            assert_eq!(statut, StatusCode::OK, "{uri} : {corps}");
            assert!(
                !corps.contains("Secret"),
                "{uri} (auth {authentification}) : l'historique d'un AUTRE profil est servi : {corps}"
            );
            assert!(
                corps.contains("Mien"),
                "{uri} : ma propre écoute manque : {corps}"
            );
            assert!(
                corps.contains("Commun"),
                "{uri} : l'écoute sans profil manque : {corps}"
            );
            let v: Value = serde_json::from_str(&corps).unwrap();
            assert_eq!(
                v["total"], 2,
                "{uri} : le total compte un autre profil : {corps}"
            );
        }
    }
}

#[tokio::test]
async fn l_export_ne_contient_pas_l_historique_d_un_autre_profil() {
    for authentification in [false, true] {
        let b = banc(authentification);
        let (statut, corps) = appel(&b, moi(&b), "GET", "/api/v1/library/history/export").await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
        assert!(
            !corps.contains("Secret"),
            "export (auth {authentification}) : l'historique d'un AUTRE profil est exporté : {corps}"
        );
        assert!(
            corps.contains("Mien") && corps.contains("Commun"),
            "{corps}"
        );
    }
}

#[tokio::test]
async fn le_tableau_de_bord_d_un_autre_profil_est_refuse_au_compte_standard() {
    let b = banc(true);
    let autre = format!(
        "/api/v1/library/history/dashboard?period=all&profile_id={}",
        b.autre
    );
    let (statut, corps) = appel(&b, moi(&b), "GET", &autre).await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "un compte standard lit les statistiques d'un autre profil : {corps}"
    );
    // Sans profil demandé : le sien, pas le foyer.
    let (statut, corps) = appel(
        &b,
        moi(&b),
        "GET",
        "/api/v1/library/history/dashboard?period=all",
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(
        plays(&corps),
        2,
        "sans profil demandé, un compte standard doit recevoir ses écoutes et les communes, pas le foyer : {corps}"
    );
    // Le sien, nommé : servi.
    let mien = format!(
        "/api/v1/library/history/dashboard?period=all&profile_id={}",
        b.moi
    );
    let (statut, corps) = appel(&b, moi(&b), "GET", &mien).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(plays(&corps), 1, "{corps}");
    // La case de la carte de chaleur : même règle.
    let case = format!(
        "/api/v1/library/history/at?period=all&weekday=1&hour=0&profile_id={}",
        b.autre
    );
    let (statut, corps) = appel(&b, moi(&b), "GET", &case).await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "carte de chaleur d'un autre profil : {corps}"
    );
    // L'historique d'un profil nommé.
    let nomme = format!("/api/v1/profiles/{}/history", b.autre);
    let (statut, corps) = appel(&b, moi(&b), "GET", &nomme).await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "un compte standard lit /profiles/{{autre}}/history : {corps}"
    );
    let nomme = format!("/api/v1/profiles/{}/history", b.moi);
    let (statut, corps) = appel(&b, moi(&b), "GET", &nomme).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert!(
        corps.contains("Mien") && !corps.contains("Secret"),
        "{corps}"
    );
}

#[tokio::test]
async fn l_administrateur_garde_le_foyer_et_les_autres_profils() {
    let b = banc(true);
    let (statut, corps) = appel(
        &b,
        admin(&b),
        "GET",
        "/api/v1/library/history/dashboard?period=all",
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(
        plays(&corps),
        4,
        "l'administrateur perd le total du foyer : {corps}"
    );
    let autre = format!(
        "/api/v1/library/history/dashboard?period=all&profile_id={}",
        b.autre
    );
    let (statut, corps) = appel(&b, admin(&b), "GET", &autre).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(plays(&corps), 2, "{corps}");
    let nomme = format!("/api/v1/profiles/{}/history", b.autre);
    let (statut, corps) = appel(&b, admin(&b), "GET", &nomme).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert!(corps.contains("Secret"), "{corps}");
}

#[tokio::test]
async fn sans_authentification_le_tableau_de_bord_reste_celui_du_foyer() {
    // Authentification coupée : rien ne change pour les foyers sans compte.
    let b = banc(false);
    let (statut, corps) = appel(
        &b,
        moi(&b),
        "GET",
        "/api/v1/library/history/dashboard?period=all",
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(plays(&corps), 4, "{corps}");
}

#[tokio::test]
async fn vider_n_efface_que_l_historique_du_profil_appelant() {
    for authentification in [false, true] {
        let b = banc(authentification);
        let (statut, corps) = appel(&b, moi(&b), "DELETE", "/api/v1/library/history").await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
        // Authentification coupée : ce que montre la liste (le mien et les
        // communes). Active : un compte standard n'efface pas les communes.
        let attendu: Vec<String> = if authentification {
            vec!["Commun".into(), "Secret".into(), "Secret bis".into()]
        } else {
            vec!["Secret".into(), "Secret bis".into()]
        };
        assert_eq!(
            titres_restants(&b),
            attendu,
            "auth {authentification} : vider a touché l'historique d'un AUTRE profil (ou le commun), ou laissé le mien"
        );
    }
}

#[tokio::test]
async fn vider_tout_le_foyer_est_reserve_a_l_administration() {
    // Authentification coupée : un appareil du réseau est refusé, cette
    // machine passe, une requête relayée (vue de 127.0.0.1) est refusée.
    let b = banc(false);
    let foyer = "/api/v1/library/history?scope=household";
    let (statut, corps) = appel(&b, moi(&b), "DELETE", foyer).await;
    assert_eq!(statut, StatusCode::FORBIDDEN, "réseau local : {corps}");
    let relayee = Appel {
        depuis: ICI,
        jeton: None,
        profil: None,
        relayee: true,
    };
    let (statut, corps) = appel(&b, relayee, "DELETE", foyer).await;
    assert_eq!(statut, StatusCode::FORBIDDEN, "relais : {corps}");
    assert_eq!(
        titres_restants(&b).len(),
        4,
        "un refus a effacé quelque chose"
    );
    let ici = Appel {
        depuis: ICI,
        jeton: None,
        profil: None,
        relayee: false,
    };
    let (statut, corps) = appel(&b, ici, "DELETE", foyer).await;
    assert_eq!(statut, StatusCode::OK, "cette machine : {corps}");
    assert!(titres_restants(&b).is_empty());

    // Authentification active : le compte standard est refusé,
    // l'administrateur passe, sauf par le relais.
    let b = banc(true);
    let (statut, corps) = appel(&b, moi(&b), "DELETE", foyer).await;
    assert_eq!(statut, StatusCode::FORBIDDEN, "compte standard : {corps}");
    let admin_relaye = Appel {
        depuis: ICI,
        jeton: b.jeton_admin.as_deref(),
        profil: None,
        relayee: true,
    };
    let (statut, corps) = appel(&b, admin_relaye, "DELETE", foyer).await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "administrateur relayé : {corps}"
    );
    assert_eq!(
        titres_restants(&b).len(),
        4,
        "un refus a effacé quelque chose"
    );
    let (statut, corps) = appel(&b, admin(&b), "DELETE", foyer).await;
    assert_eq!(statut, StatusCode::OK, "administrateur : {corps}");
    assert!(titres_restants(&b).is_empty());
}

#[tokio::test]
async fn rien_ne_s_efface_par_le_relais() {
    // Par le relais, `X-Profile-Id` désignerait n'importe quel profil : en
    // N requêtes, tout le foyer serait vidé. L'effacement n'y passe jamais,
    // ni pour un appareil sans compte, ni pour un compte standard, ni pour
    // l'administrateur.
    for authentification in [false, true] {
        let b = banc(authentification);
        let jetons = [b.jeton_moi.clone(), b.jeton_admin.clone()];
        for jeton in jetons.iter() {
            for profil in [b.moi, b.autre] {
                let relayee = Appel {
                    depuis: ICI,
                    jeton: jeton.as_deref(),
                    profil: Some(profil),
                    relayee: true,
                };
                let (statut, corps) = appel(&b, relayee, "DELETE", "/api/v1/library/history").await;
                assert_eq!(
                    statut,
                    StatusCode::FORBIDDEN,
                    "auth {authentification}, profil {profil} : effacement relayé accepté : {corps}"
                );
            }
        }
        assert_eq!(
            titres_restants(&b).len(),
            4,
            "un effacement relayé est passé"
        );
    }
}

#[tokio::test]
async fn les_classements_et_les_statistiques_d_un_profil_restent_les_siens() {
    let b = banc(true);
    for uri in [
        "/api/v1/library/history/top-tracks?limit=50",
        "/api/v1/library/history/top-artists?limit=50",
        "/api/v1/library/history/top-albums?limit=50",
    ] {
        let (statut, corps) = appel(&b, moi(&b), "GET", uri).await;
        assert_eq!(statut, StatusCode::OK, "{uri} : {corps}");
        assert!(
            !corps.contains("Secret"),
            "{uri} : le classement d'un compte standard compte un AUTRE profil : {corps}"
        );
        let v: Value = serde_json::from_str(&corps).unwrap();
        let total: i64 = v
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["plays"].as_i64().unwrap_or(0))
            .sum();
        assert_eq!(
            total, 2,
            "{uri} : un AUTRE profil compte dans le classement : {corps}"
        );
        // L'administrateur garde le foyer.
        let (_, corps) = appel(&b, admin(&b), "GET", uri).await;
        let v: Value = serde_json::from_str(&corps).unwrap();
        let total: i64 = v
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["plays"].as_i64().unwrap_or(0))
            .sum();
        assert_eq!(total, 4, "{uri} (admin) : {corps}");
    }
    let stats = format!("/api/v1/profiles/{}/stats", b.autre);
    let (statut, corps) = appel(&b, moi(&b), "GET", &stats).await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "un compte standard lit /profiles/{{autre}}/stats : {corps}"
    );
    let (statut, corps) = appel(&b, admin(&b), "GET", &stats).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let stats = format!("/api/v1/profiles/{}/stats", b.moi);
    let (statut, corps) = appel(&b, moi(&b), "GET", &stats).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
}

#[tokio::test]
async fn l_activite_et_les_tableaux_du_tableau_de_bord_restent_ceux_du_profil() {
    let b = banc(true);
    let (statut, corps) = appel(&b, moi(&b), "GET", "/api/v1/library/activity?limit=50").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert!(
        !corps.contains("Secret"),
        "/library/activity : l'historique d'un AUTRE profil est servi : {corps}"
    );
    assert!(
        corps.contains("Mien") && corps.contains("Commun"),
        "{corps}"
    );
    for uri in [
        "/api/v1/dashboard/top-tracks?limit=50",
        "/api/v1/dashboard/top-artists?limit=50",
        "/api/v1/dashboard/top-albums?limit=50",
        "/api/v1/dashboard/listening-history?days=30",
    ] {
        let (statut, corps) = appel(&b, moi(&b), "GET", uri).await;
        assert_eq!(statut, StatusCode::OK, "{uri} : {corps}");
        assert!(!corps.contains("Secret"), "{uri} : {corps}");
        let v: Value = serde_json::from_str(&corps).unwrap();
        let total: i64 = v
            .as_array()
            .unwrap()
            .iter()
            .map(|l| {
                l["plays"]
                    .as_i64()
                    .or_else(|| l["play_count"].as_i64())
                    .unwrap_or(0)
            })
            .sum();
        assert_eq!(total, 2, "{uri} : un AUTRE profil compte : {corps}");
        let (_, corps) = appel(&b, admin(&b), "GET", uri).await;
        let v: Value = serde_json::from_str(&corps).unwrap();
        let total: i64 = v
            .as_array()
            .unwrap()
            .iter()
            .map(|l| {
                l["plays"]
                    .as_i64()
                    .or_else(|| l["play_count"].as_i64())
                    .unwrap_or(0)
            })
            .sum();
        assert_eq!(total, 4, "{uri} (admin) : {corps}");
    }
    let (statut, corps) = appel(&b, moi(&b), "GET", "/api/v1/dashboard/stats").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let v: Value = serde_json::from_str(&corps).unwrap();
    assert_eq!(
        v["total_listens"], 2,
        "/dashboard/stats compte le foyer : {corps}"
    );
    let (_, corps) = appel(&b, admin(&b), "GET", "/api/v1/dashboard/stats").await;
    let v: Value = serde_json::from_str(&corps).unwrap();
    assert_eq!(v["total_listens"], 4, "{corps}");
}

#[tokio::test]
async fn un_compte_standard_ne_touche_pas_aux_autres_profils() {
    let b = banc(true);
    let autre = format!("/api/v1/profiles/{}", b.autre);
    let mien = format!("/api/v1/profiles/{}", b.moi);
    let (statut, corps) =
        appel_corps(&b, moi(&b), "PUT", &autre, json!({ "name": "pirate" })).await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "renommer un autre profil : {corps}"
    );
    let (statut, corps) = appel(&b, moi(&b), "DELETE", &autre).await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "supprimer un autre profil : {corps}"
    );
    let (statut, corps) = appel(&b, moi(&b), "DELETE", &mien).await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "supprimer un profil (standard) : {corps}"
    );
    let (statut, corps) = appel_corps(
        &b,
        moi(&b),
        "POST",
        "/api/v1/profiles/switch",
        json!({ "profile_id": b.autre }),
    )
    .await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "basculer sur un autre profil : {corps}"
    );
    let (statut, corps) = appel(&b, moi(&b), "POST", &format!("{autre}/activate")).await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "activer un autre profil : {corps}"
    );
    // Rien n'a bougé.
    let autre_profil = ProfileRepo::with_backend(b.state.backend.clone())
        .get(b.autre)
        .unwrap()
        .expect("le profil existe encore");
    assert_eq!(autre_profil.name, "autre", "un autre profil a été renommé");
    // Les siens : permis.
    let (statut, corps) = appel_corps(&b, moi(&b), "PUT", &mien, json!({ "name": "moi2" })).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    let (statut, corps) = appel_corps(
        &b,
        moi(&b),
        "POST",
        "/api/v1/profiles/switch",
        json!({ "profile_id": b.moi }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    // L'administrateur supprime un profil : ses écoutes partent avec lui.
    let (statut, corps) = appel(&b, admin(&b), "DELETE", &autre).await;
    assert_eq!(statut, StatusCode::NO_CONTENT, "{corps}");
    assert_eq!(
        titres_restants(&b),
        vec!["Commun".to_string(), "Mien".to_string()],
        "les écoutes du profil supprimé restent orphelines"
    );
}

async fn appel_corps(
    b: &Banc,
    a: Appel<'_>,
    methode: &str,
    uri: &str,
    corps: Value,
) -> (StatusCode, String) {
    let adresse: SocketAddr = a.depuis.parse().unwrap();
    let mut requete = Request::builder()
        .method(methode)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .extension(ConnectInfo(adresse));
    if let Some(j) = a.jeton {
        requete = requete.header(header::AUTHORIZATION, format!("Bearer {j}"));
    }
    if let Some(p) = a.profil {
        requete = requete.header("X-Profile-Id", p.to_string());
    }
    let reponse = crate::routes::router(b.state.clone())
        .oneshot(requete.body(Body::from(corps.to_string())).unwrap())
        .await
        .unwrap();
    let statut = reponse.status();
    let octets = to_bytes(reponse.into_body(), usize::MAX).await.unwrap();
    (statut, String::from_utf8_lossy(&octets).into_owned())
}

#[tokio::test]
async fn preferences_retour_au_defaut_et_creation_restent_a_l_administrateur() {
    let b = banc(true);
    let autre = format!("/api/v1/profiles/{}/settings", b.autre);
    let mien = format!("/api/v1/profiles/{}/settings", b.moi);
    let (statut, corps) = appel(&b, moi(&b), "GET", &autre).await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "lire les préférences d'autrui : {corps}"
    );
    let (statut, corps) =
        appel_corps(&b, moi(&b), "POST", &autre, json!({ "theme": "pirate" })).await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "écrire les préférences d'autrui : {corps}"
    );
    let reglages =
        tune_core::db::settings_repo::SettingsRepo::with_backend(b.state.backend.clone());
    assert!(
        reglages
            .get(&format!("profile_{}_settings", b.autre))
            .unwrap()
            .is_none(),
        "les préférences d'autrui ont été écrites"
    );
    let (statut, corps) =
        appel_corps(&b, moi(&b), "POST", &mien, json!({ "theme": "sombre" })).await;
    assert_eq!(statut, StatusCode::NO_CONTENT, "{corps}");
    let (statut, corps) = appel(&b, moi(&b), "GET", &mien).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert!(corps.contains("sombre"), "{corps}");
    let (statut, corps) = appel(&b, admin(&b), "GET", &autre).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");

    // Revenir au profil par défaut change le profil actif de tous.
    let (statut, corps) = appel(&b, moi(&b), "POST", "/api/v1/profiles/deactivate").await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "deactivate par un compte standard : {corps}"
    );
    let (statut, corps) = appel(&b, admin(&b), "POST", "/api/v1/profiles/deactivate").await;
    assert_eq!(statut, StatusCode::OK, "{corps}");

    // Créer un profil = créer un compte.
    let (statut, corps) = appel_corps(
        &b,
        moi(&b),
        "POST",
        "/api/v1/profiles",
        json!({ "name": "intrus" }),
    )
    .await;
    assert_eq!(
        statut,
        StatusCode::FORBIDDEN,
        "création par un compte standard : {corps}"
    );
    assert!(
        ProfileRepo::with_backend(b.state.backend.clone())
            .list()
            .unwrap()
            .iter()
            .all(|p| p.name != "intrus"),
        "un compte standard a créé un profil"
    );
    let (statut, corps) = appel_corps(
        &b,
        admin(&b),
        "POST",
        "/api/v1/profiles",
        json!({ "name": "invite2" }),
    )
    .await;
    assert_ne!(statut, StatusCode::FORBIDDEN, "{corps}");
}
