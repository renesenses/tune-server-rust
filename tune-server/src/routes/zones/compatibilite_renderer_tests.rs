//! `GET` / `DELETE /zones/{id}/compatibilite-renderer` : la commande
//! `SetAVTransportURI` apprise pour le renderer d'une zone, exposée au
//! diagnostic et oubliable à la main.
//!
//! Le profil est posé EN BASE, sous la clé que la mémoire de `tune-core`
//! relit : la route doit le trouver sans qu'aucune lecture n'ait eu lieu
//! dans ce processus — c'est la situation d'un redémarrage.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::dlna_repli_set_uri as compat;

async fn appeler(app: &axum::Router, methode: &str, chemin: &str) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method(methode)
        .uri(chemin)
        .body(Body::empty())
        .unwrap();
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_compatibilite_apprise_se_lit_puis_se_reinitialise_par_la_route() {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = crate::routes::router(state.clone());
    compat::installer_persistance(state.backend.clone());

    let udn = "uuid:banc-route-compatibilite-renderer";
    let cle = format!("{}{udn}", compat::PREFIXE_CLE_REGLAGE);
    let reglages = SettingsRepo::with_backend(state.backend.clone());
    reglages
        .set(
            &cle,
            r#"{"firmware":"1.2.3","profils":{"audio/flac":{"mime_annonce":"audio/x-flac","niveau_didl_min":1,"servi_sous_ce_mime":true,"appris_le":"2026-10-07T10:00:00Z"}}}"#,
        )
        .unwrap();
    let zones = ZoneRepo::with_backend(state.backend.clone());
    let zone = zones.create("Salon", Some("dlna"), Some(udn)).unwrap();
    let locale = zones
        .create("Bureau", Some("local"), Some("local:defaut"))
        .unwrap();

    let (statut, corps) = appeler(
        &app,
        "GET",
        &format!("/api/v1/zones/{zone}/compatibilite-renderer"),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["device_id"], udn);
    assert_eq!(corps["firmware"], "1.2.3");
    assert_eq!(corps["mode_conservateur"], true);
    let profils = corps["profils"].as_array().expect("liste de profils");
    assert_eq!(profils.len(), 1, "{corps}");
    assert_eq!(profils[0]["mime_source"], "audio/flac");
    assert_eq!(profils[0]["mime_annonce"], "audio/x-flac");
    assert_eq!(profils[0]["niveau_didl_min"], 1);
    assert_eq!(profils[0]["appris_le"], "2026-10-07T10:00:00Z");

    let (statut, corps) = appeler(
        &app,
        "DELETE",
        &format!("/api/v1/zones/{zone}/compatibilite-renderer"),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(corps["profils_oublies"], 1);
    assert_eq!(
        reglages.get(&cle).unwrap(),
        None,
        "la réinitialisation doit effacer la base AVANT de répondre"
    );

    let (_, corps) = appeler(
        &app,
        "GET",
        &format!("/api/v1/zones/{zone}/compatibilite-renderer"),
    )
    .await;
    assert_eq!(corps["mode_conservateur"], false, "{corps}");
    assert_eq!(corps["profils"].as_array().map(Vec::len), Some(0));

    // Une zone qui n'est pas DLNA n'a pas de commande SetAVTransportURI.
    let (statut, _) = appeler(
        &app,
        "GET",
        &format!("/api/v1/zones/{locale}/compatibilite-renderer"),
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST);
    let (statut, _) = appeler(&app, "GET", "/api/v1/zones/999999/compatibilite-renderer").await;
    assert_eq!(statut, StatusCode::NOT_FOUND);
}
