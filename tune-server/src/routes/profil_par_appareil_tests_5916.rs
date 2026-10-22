//! #5916 — le dernier profil choisi survit à un navigateur qui oublie tout.
//!
//! Les essais passent par le routeur RÉEL (`crate::routes::router`, préfixe
//! `/api/v1`) ; l'adresse de l'appareil est posée en extension `ConnectInfo`
//! sur chaque requête, comme le fait le service de production
//! (`aiguillage_des_flux::servir`). `MockConnectInfo` posé par
//! `Router::layer` n'atteint pas ce handler imbriqué : mesuré, `None`.

use std::net::SocketAddr;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::profile_repo::ProfileRepo;

use crate::state::AppState;

const SALON: &str = "192.168.1.20:51000";
const BUREAU: &str = "192.168.1.30:51000";
const NEUF: &str = "192.168.1.40:51000";

fn etat() -> (AppState, i64, i64) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ProfileRepo::with_backend(state.backend.clone());
    let gerard = repo.create("gerard", Some("Gérard"), None).unwrap();
    let anne = repo.create("anne", Some("Anne"), None).unwrap();
    (state, gerard, anne)
}

/// L'application telle que la voit l'appareil d'adresse `adresse`.
#[derive(Clone)]
struct Appli {
    app: Router,
    adresse: SocketAddr,
}

fn depuis(state: &AppState, adresse: &str) -> Appli {
    Appli {
        app: crate::routes::router(state.clone()),
        adresse: adresse.parse().unwrap(),
    }
}

async fn retenir(app: &Appli, profil: i64, entetes: &[(&str, &str)]) -> StatusCode {
    let mut req = Request::put("/api/v1/profiles/dernier-choisi")
        .header("Content-Type", "application/json")
        .extension(ConnectInfo(app.adresse));
    for (k, v) in entetes {
        req = req.header(*k, *v);
    }
    app.app
        .clone()
        .oneshot(
            req.body(Body::from(json!({ "profile_id": profil }).to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn lire(app: &Appli, entetes: &[(&str, &str)]) -> (StatusCode, Value) {
    let mut req =
        Request::get("/api/v1/profiles/dernier-choisi").extension(ConnectInfo(app.adresse));
    for (k, v) in entetes {
        req = req.header(*k, *v);
    }
    let resp = app
        .app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let statut = resp.status();
    let corps = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&corps).unwrap_or(Value::Null),
    )
}

/// LE DÉFAUT DU FIL 2168 : le navigateur du salon a choisi Gérard, celui du
/// bureau Anne. Chacun, son stockage effacé, doit retrouver SON profil, et
/// pas le dernier choisi dans la maison.
#[tokio::test]
async fn chaque_appareil_retrouve_son_dernier_profil() {
    let (state, gerard, anne) = etat();
    let salon = depuis(&state, SALON);
    let bureau = depuis(&state, BUREAU);

    assert_eq!(retenir(&salon, gerard, &[]).await, StatusCode::NO_CONTENT);
    assert_eq!(retenir(&bureau, anne, &[]).await, StatusCode::NO_CONTENT);

    let (statut, corps) = lire(&salon, &[]).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        corps,
        json!({ "profile_id": gerard, "source": "appareil" }),
        "#5916 : le salon doit retrouver Gérard, son dernier choix, et non \
         le dernier choix de la maison"
    );
    assert_eq!(
        lire(&bureau, &[]).await.1,
        json!({ "profile_id": anne, "source": "appareil" })
    );
}

/// Le repli documenté : un appareil qui n'a jamais rien choisi s'ouvre sur
/// le dernier profil choisi, tous appareils confondus.
#[tokio::test]
async fn un_appareil_neuf_prend_le_dernier_choix_de_la_maison() {
    let (state, gerard, anne) = etat();
    retenir(&depuis(&state, SALON), gerard, &[]).await;
    retenir(&depuis(&state, BUREAU), anne, &[]).await;

    assert_eq!(
        lire(&depuis(&state, NEUF), &[]).await.1,
        json!({ "profile_id": anne, "source": "tous_appareils" })
    );
}

/// Derrière un mandataire, le pair de la socket n'est pas l'appareil : on ne
/// retient rien sous son adresse, on ne lit que le repli.
#[tokio::test]
async fn derriere_un_mandataire_seul_le_repli_joue() {
    let (state, gerard, anne) = etat();
    let local = depuis(&state, "127.0.0.1:40000");
    retenir(&local, gerard, &[]).await;
    retenir(&local, anne, &[("x-forwarded-for", "203.0.113.9")]).await;

    assert_eq!(
        lire(&local, &[]).await.1,
        json!({ "profile_id": gerard, "source": "appareil" }),
        "le choix fait au travers du mandataire ne doit pas s'écrire sous \
         l'adresse du mandataire"
    );
    assert_eq!(
        lire(&local, &[("x-forwarded-for", "203.0.113.9")]).await.1,
        json!({ "profile_id": anne, "source": "tous_appareils" })
    );
}

/// Un profil supprimé depuis ne se ressert pas ; un profil inconnu ne se
/// retient pas.
#[tokio::test]
async fn un_profil_disparu_ne_revient_pas() {
    let (state, gerard, _) = etat();
    let salon = depuis(&state, SALON);
    retenir(&salon, gerard, &[]).await;
    ProfileRepo::with_backend(state.backend.clone())
        .delete(gerard)
        .unwrap();

    assert_eq!(
        lire(&salon, &[]).await.1,
        json!({ "profile_id": null, "source": null })
    );
    assert_eq!(retenir(&salon, 9999, &[]).await, StatusCode::NOT_FOUND);
}

/// Ces lignes ne sont le réglage de personne : `/system/config` ne les
/// publie pas (elles portent l'adresse IP de chaque appareil).
#[tokio::test]
async fn system_config_ne_publie_pas_les_choix_par_appareil() {
    let (state, gerard, _) = etat();
    let salon = depuis(&state, SALON);
    retenir(&salon, gerard, &[]).await;

    let (statut, config) = lire_chemin(&salon, "/api/v1/system/config").await;
    assert_eq!(statut, StatusCode::OK);
    let fuites: Vec<&String> = config
        .as_object()
        .unwrap()
        .keys()
        .filter(|k| k.starts_with(super::PREFIXE))
        .collect();
    assert!(fuites.is_empty(), "{fuites:?}");
}

async fn lire_chemin(app: &Appli, chemin: &str) -> (StatusCode, Value) {
    let resp = app
        .app
        .clone()
        .oneshot(
            Request::get(chemin)
                .extension(ConnectInfo(app.adresse))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = resp.status();
    let corps = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&corps).unwrap_or(Value::Null),
    )
}
