//! « Analyser la bibliothèque au démarrage » réglable depuis les Réglages.
//!
//! Ordre de précédence tenu ici :
//!
//! 1. le réglage `library_scan_on_startup`, s'il a été posé ;
//! 2. sinon la configuration de déploiement (`TUNE_AUTO_SCAN`, `tune.toml`) ;
//! 3. sinon `false`.
//!
//! Les épreuves jouent la VRAIE décision du démarrage (`scan_au_demarrage`,
//! celle que `bootstrap` appelle) et la VRAIE route `/system/config`.

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

use crate::auto_scan::{
    CLE_SCAN_AU_DEMARRAGE, choix_utilisateur_scan_au_demarrage, scan_au_demarrage,
    scan_au_demarrage_voulu,
};
use crate::state::AppState;

fn etat(auto_scan: bool) -> AppState {
    let config = crate::config::TuneConfig {
        auto_scan,
        ..Default::default()
    };
    AppState::new(":memory:", 0, config).unwrap()
}

fn poser(state: &AppState, valeur: &str) {
    SettingsRepo::with_backend(state.backend.clone())
        .set(CLE_SCAN_AU_DEMARRAGE, valeur)
        .unwrap();
}

async fn requete(state: &AppState, methode: &str, corps: Option<Value>) -> (StatusCode, Value) {
    let corps = corps.map(|c| c.to_string()).unwrap_or_default();
    let response = crate::routes::router(state.clone())
        .oneshot(
            Request::builder()
                .method(methode)
                .uri("/api/v1/system/config")
                .header("content-type", "application/json")
                .body(Body::from(corps))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

#[test]
fn seule_une_valeur_lisible_est_un_choix() {
    assert_eq!(
        choix_utilisateur_scan_au_demarrage(Some("true")),
        Some(true)
    );
    assert_eq!(
        choix_utilisateur_scan_au_demarrage(Some("\"false\"")),
        Some(false)
    );
    assert_eq!(
        choix_utilisateur_scan_au_demarrage(Some(" TRUE ")),
        Some(true)
    );
    assert_eq!(choix_utilisateur_scan_au_demarrage(Some("0")), Some(false));
    assert_eq!(choix_utilisateur_scan_au_demarrage(None), None);
    assert_eq!(choix_utilisateur_scan_au_demarrage(Some("")), None);
    assert_eq!(choix_utilisateur_scan_au_demarrage(Some("peut-être")), None);
}

/// Sans le réglage, rien ne change : la configuration de déploiement décide,
/// comme avant.
#[test]
fn sans_reglage_la_configuration_de_deploiement_decide() {
    for deploiement in [true, false] {
        let state = etat(deploiement);
        assert_eq!(
            scan_au_demarrage_voulu(deploiement, &state.backend),
            deploiement,
            "une installation où personne n'a touché à rien doit garder auto_scan = {deploiement}"
        );
        assert_eq!(scan_au_demarrage(deploiement, &state.backend), deploiement);
    }
}

/// Le choix de l'utilisateur prime sur la configuration de déploiement, dans
/// les deux sens — et c'est la décision du démarrage qui le suit.
#[test]
fn le_reglage_utilisateur_prime_sur_le_deploiement() {
    let state = etat(true);
    poser(&state, "false");
    assert!(
        !scan_au_demarrage(true, &state.backend),
        "réglage « false » : le démarrage ne doit pas scanner, même avec TUNE_AUTO_SCAN=true"
    );

    let state = etat(false);
    poser(&state, "true");
    assert!(
        scan_au_demarrage(false, &state.backend),
        "réglage « true » : le démarrage doit scanner, même sans TUNE_AUTO_SCAN"
    );

    // Une valeur illisible n'est pas un choix : le déploiement reprend la main.
    let state = etat(true);
    poser(&state, "peut-être");
    assert!(scan_au_demarrage(true, &state.backend));
}

/// La route publie la valeur du prochain démarrage et qui la décide ; `PATCH`
/// l'écrit, refuse l'illisible, et `null` rend la main au déploiement.
#[tokio::test]
async fn la_route_publie_ecrit_et_efface_le_reglage() {
    let state = etat(true);
    let (status, config) = requete(&state, "GET", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(config[CLE_SCAN_AU_DEMARRAGE], json!(true), "{config}");
    assert_eq!(
        config[format!("{CLE_SCAN_AU_DEMARRAGE}_source")],
        json!("deployment")
    );

    let (status, _) = requete(
        &state,
        "PATCH",
        Some(json!({ CLE_SCAN_AU_DEMARRAGE: false })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, config) = requete(&state, "GET", None).await;
    assert_eq!(config[CLE_SCAN_AU_DEMARRAGE], json!(false), "{config}");
    assert_eq!(
        config[format!("{CLE_SCAN_AU_DEMARRAGE}_source")],
        json!("user")
    );
    assert!(
        !scan_au_demarrage(state.config.auto_scan, &state.backend),
        "le réglage écrit par la route doit gouverner le prochain démarrage"
    );

    for illisible in [json!("souvent"), json!(1), json!([true])] {
        let (status, _) = requete(
            &state,
            "PATCH",
            Some(json!({ CLE_SCAN_AU_DEMARRAGE: illisible })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{illisible}");
    }
    let (_, config) = requete(&state, "GET", None).await;
    assert_eq!(config[CLE_SCAN_AU_DEMARRAGE], json!(false));

    let (status, _) = requete(
        &state,
        "PATCH",
        Some(json!({ CLE_SCAN_AU_DEMARRAGE: null })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        SettingsRepo::with_backend(state.backend.clone())
            .get(CLE_SCAN_AU_DEMARRAGE)
            .unwrap(),
        None,
        "null doit effacer la ligne, pas écrire « null »"
    );
    let (_, config) = requete(&state, "GET", None).await;
    assert_eq!(config[CLE_SCAN_AU_DEMARRAGE], json!(true));
    assert_eq!(
        config[format!("{CLE_SCAN_AU_DEMARRAGE}_source")],
        json!("deployment")
    );
}

/// #7169 (repère de reprise #5531 posé par une mise à jour forcée) — un choix
/// explicite « false » prime sur la reprise : pas de scan, et le repère est
/// levé, sans quoi il relancerait le scan à chaque démarrage.
#[test]
fn reglage_false_prime_sur_la_reprise_et_leve_le_repere_7169() {
    for deploiement in [true, false] {
        let state = etat(deploiement);
        poser(&state, "false");
        crate::routes::system::scan::poser_reprise_du_scan(&state.backend);
        assert!(
            !scan_au_demarrage(deploiement, &state.backend),
            "#7169 : réglage « false » avec repère de reprise posé : le démarrage ne doit pas scanner (déploiement = {deploiement})"
        );
        assert!(
            !crate::routes::system::scan::reprise_du_scan_demandee(&state.backend),
            "#7169 : réglage « false » : le repère de reprise doit être levé"
        );
        assert!(
            !scan_au_demarrage(deploiement, &state.backend),
            "#7169 : le démarrage suivant ne doit pas scanner non plus"
        );
    }
}

/// Sans choix de l'utilisateur, la reprise #5531 reste inchangée : le repère
/// relance le scan, même sans `auto_scan`, et reste posé jusqu'à un scan
/// allé au bout.
#[test]
fn sans_reglage_le_repere_reprend_le_scan_7169() {
    let state = etat(false);
    crate::routes::system::scan::poser_reprise_du_scan(&state.backend);
    assert!(
        scan_au_demarrage(false, &state.backend),
        "#5531 : sans réglage, le repère doit relancer le scan au démarrage"
    );
    assert!(
        crate::routes::system::scan::reprise_du_scan_demandee(&state.backend),
        "#5531 : seul un scan allé au bout lève le repère"
    );
}

/// Choix « true » : le scan part, repère ou non.
#[test]
fn reglage_true_scanne_au_demarrage_7169() {
    let state = etat(false);
    poser(&state, "true");
    assert!(
        scan_au_demarrage(false, &state.backend),
        "#7169 : réglage « true » : le démarrage doit scanner"
    );
    crate::routes::system::scan::poser_reprise_du_scan(&state.backend);
    assert!(
        scan_au_demarrage(false, &state.backend),
        "#7169 : réglage « true » avec repère : le démarrage doit scanner"
    );
}
