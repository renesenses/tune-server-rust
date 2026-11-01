//! rc11 (Bertrand, 01/11/2026) — DJ et Karaoké s'activent depuis Réglages ›
//! Extensions, par les routes ordinaires des greffons (`POST
//! /plugins/{id}/install`, `DELETE /plugins/{id}`), exactement comme les autres
//! opt-in. Avant, ils étaient hors catalogue (#2090) : la liste ne les
//! proposait pas, et seul un `plugin_dj_installed=true` écrit à la main les
//! chargeait.
//!
//! Chaque témoin a sa contre-épreuve : sans l'installation le greffon ne monte
//! AUCUN routeur ; avec elle, il en monte un.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

use crate::use_scratch_plugin_data_dir;

const IDS: [&str; 2] = ["dj", "karaoke"];

fn new_state() -> AppState {
    use_scratch_plugin_data_dir();
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

async fn appel(app: &axum::Router, methode: &str, chemin: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

async fn fiche(app: &axum::Router, id: &str) -> Option<Value> {
    let (_, liste) = appel(app, "GET", "/api/v1/plugins").await;
    let mut trouvees = liste
        .as_array()
        .expect("la liste des greffons est un tableau")
        .iter()
        .filter(|p| p["name"] == id)
        .cloned()
        .collect::<Vec<_>>();
    // #7428 — une extension n'apparaît qu'UNE fois, avec son état.
    assert!(
        trouvees.len() <= 1,
        "« {id} » listé {} fois",
        trouvees.len()
    );
    trouvees.pop()
}

#[tokio::test]
async fn dj_et_karaoke_sont_proposes_non_installes() {
    let state = new_state();
    tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![]).await;
    let app = tune_server::routes::router(state.clone());
    for id in IDS {
        let f = fiche(&app, id)
            .await
            .unwrap_or_else(|| panic!("« {id} » doit figurer au catalogue"));
        assert_eq!(f["installed"], false, "{f}");
        assert_eq!(f["enabled"], false, "{f}");
        assert_eq!(f["loaded"], false, "{f}");
        assert!(
            !f["description"].as_str().unwrap_or("").is_empty(),
            "la case porte une description : {f}"
        );
    }
}

#[tokio::test]
async fn cocher_installe_et_annonce_le_redemarrage_decocher_retire() {
    for id in IDS {
        let state = new_state();
        tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![]).await;
        let app = tune_server::routes::router(state.clone());
        let reglages = SettingsRepo::with_backend(state.backend.clone());
        let cle = format!("plugin_{id}_installed");
        assert_eq!(
            reglages.get(&cle).unwrap(),
            None,
            "banc : rien avant le clic"
        );

        let (statut, corps) = appel(&app, "POST", &format!("/api/v1/plugins/{id}/install")).await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
        assert_eq!(corps["restart_required"], true, "{corps}");
        assert_eq!(reglages.get(&cle).unwrap().as_deref(), Some("true"));

        // La fiche relue : case cochée, en attente du redémarrage.
        let f = fiche(&app, id).await.expect("toujours listé");
        assert_eq!(f["installed"], true, "{f}");
        assert_eq!(f["enabled"], true, "{f}");
        assert_eq!(f["loaded"], false, "{f}");
        assert_eq!(f["restart_required"], true, "{f}");

        // Décocher : le réglage disparaît et la fiche redevient proposée.
        let (statut, corps) = appel(&app, "DELETE", &format!("/api/v1/plugins/{id}")).await;
        assert_eq!(statut, StatusCode::OK, "{corps}");
        assert_eq!(reglages.get(&cle).unwrap(), None);
        let f = fiche(&app, id).await.expect("toujours proposé");
        assert_eq!(f["installed"], false, "{f}");
        assert_eq!(f["enabled"], false, "{f}");
    }
}

#[tokio::test]
async fn l_installation_decide_du_chargement_avec_contre_epreuve() {
    for id in IDS {
        // Contre-épreuve : sans la case, aucun routeur.
        let sans = new_state();
        let routeurs = tune_server::plugins::init(&sans, "http://127.0.0.1:0", vec![]).await;
        assert!(
            !routeurs.iter().any(|(n, _)| n == id),
            "« {id} » ne doit rien monter tant qu'il n'est pas coché"
        );

        // Cochée (comme la route le fait), au démarrage suivant il se charge.
        let avec = new_state();
        let reglages = SettingsRepo::with_backend(avec.backend.clone());
        reglages
            .set(&format!("plugin_{id}_installed"), "true")
            .unwrap();
        reglages
            .set(&format!("plugin_{id}_enabled"), "true")
            .unwrap();
        let routeurs = tune_server::plugins::init(&avec, "http://127.0.0.1:0", vec![]).await;
        assert!(
            routeurs.iter().any(|(n, _)| n == id),
            "« {id} » coché doit monter son routeur"
        );
        let app = tune_server::routes::router(avec.clone());
        let f = fiche(&app, id).await.expect("chargé donc listé");
        assert_eq!(f["installed"], true, "{f}");
        assert_eq!(f["enabled"], true, "{f}");

        // Décoché puis redémarré : plus chargé.
        let apres = new_state();
        let reglages = SettingsRepo::with_backend(apres.backend.clone());
        reglages
            .set(&format!("plugin_{id}_installed"), "true")
            .unwrap();
        reglages.delete(&format!("plugin_{id}_installed")).unwrap();
        let routeurs = tune_server::plugins::init(&apres, "http://127.0.0.1:0", vec![]).await;
        assert!(
            !routeurs.iter().any(|(n, _)| n == id),
            "décoché : plus chargé"
        );
    }
}
