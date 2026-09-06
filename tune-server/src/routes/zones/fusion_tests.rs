//! DUP-1 (phase 1) : la fusion de deux zones par la route publique.
//!
//! Les deux cas sont ceux du relevé du 05/09 sur le serveur de test : le
//! Sonos Play:1 en deux UDN (racine et `_MR`), le Mac Studio en identifiant IP
//! puis MAC. Le premier fusionne ; le second est refusé tant qu'aucun appareil
//! découvert ne relie l'adresse au MAC — la règle de `cle_appareil`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;

fn serveur() -> (axum::Router, crate::state::AppState) {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let routeur = crate::routes::router(state.clone());
    (routeur, state)
}

async fn appeler(app: &axum::Router, methode: &str, chemin: &str) -> (StatusCode, Value) {
    let requete = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("Content-Type", "application/json")
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

async fn ids_des_zones(app: &axum::Router) -> Vec<i64> {
    let (_, corps) = appeler(app, "GET", "/api/v1/zones").await;
    let liste = corps
        .as_array()
        .cloned()
        .or_else(|| corps["zones"].as_array().cloned())
        .unwrap_or_default();
    let mut ids: Vec<i64> = liste.iter().filter_map(|z| z["id"].as_i64()).collect();
    ids.sort_unstable();
    ids
}

#[tokio::test]
async fn les_deux_zones_du_sonos_fusionnent_par_la_route() {
    let (app, state) = serveur();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let doublon = repo
        .create(
            "Chambre",
            Some("dlna"),
            Some("uuid:RINCON_B8E937B44D0801400_MR"),
        )
        .unwrap();
    let cible = repo
        .create(
            "Chambre - Sonos Play:1 Media Renderer - RINCON_B8E937B44D0801400",
            Some("dlna"),
            Some("uuid:RINCON_B8E937B44D0801400"),
        )
        .unwrap();
    state
        .backend
        .execute_batch(&format!(
            "INSERT INTO queue_items (zone_id, position, title) VALUES ({doublon}, 0, 'Time');"
        ))
        .unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(&format!("zone_{doublon}_eq_profile"), "chambre")
        .unwrap();

    let (statut, corps) = appeler(
        &app,
        "POST",
        &format!("/api/v1/zones/{doublon}/fusionner-dans/{cible}"),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "corps : {corps}");
    assert_eq!(corps["file_reportee"].as_u64(), Some(1));
    assert_eq!(corps["reglages_reportes"].as_u64(), Some(1));
    assert_eq!(
        ids_des_zones(&app).await,
        vec![cible],
        "seule la cible reste visible"
    );
    assert_eq!(
        settings
            .get(&format!("zone_{cible}_eq_profile"))
            .unwrap()
            .as_deref(),
        Some("chambre")
    );

    // Le doublon ne renaît pas : son identifiant reste occupé et masqué,
    // exactement ce que la découverte consulte avant de créer une zone.
    let (id, cree) = repo
        .get_or_create("Chambre", Some("dlna"), "uuid:RINCON_B8E937B44D0801400_MR")
        .unwrap();
    assert_eq!((id, cree), (doublon, false));
    assert!(repo.is_device_hidden("uuid:RINCON_B8E937B44D0801400_MR"));
    assert_eq!(ids_des_zones(&app).await, vec![cible]);
}

#[tokio::test]
async fn deux_zones_qui_ne_designent_pas_le_meme_appareil_ne_fusionnent_pas() {
    let (app, state) = serveur();
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let par_ip = repo
        .create(
            "Mac13,1",
            Some("airplay"),
            Some("airplay-192.168.1.41-7000"),
        )
        .unwrap();
    let par_mac = repo
        .create(
            "Mac Studio",
            Some("airplay"),
            Some("airplay-76:4D:00:C0:BD:51"),
        )
        .unwrap();

    let (statut, corps) = appeler(
        &app,
        "POST",
        &format!("/api/v1/zones/{par_mac}/fusionner-dans/{par_ip}"),
    )
    .await;
    assert_eq!(statut, StatusCode::CONFLICT, "corps : {corps}");
    assert_eq!(corps["error"].as_str(), Some("zones_distinctes"));
    let mut attendus = vec![par_ip, par_mac];
    attendus.sort_unstable();
    assert_eq!(ids_des_zones(&app).await, attendus, "rien n'a bougé");

    let (statut, corps) = appeler(
        &app,
        "POST",
        &format!("/api/v1/zones/{par_ip}/fusionner-dans/{par_ip}"),
    )
    .await;
    assert_eq!(statut, StatusCode::BAD_REQUEST, "corps : {corps}");
    let (statut, _) = appeler(
        &app,
        "POST",
        &format!("/api/v1/zones/{par_ip}/fusionner-dans/999"),
    )
    .await;
    assert_eq!(statut, StatusCode::NOT_FOUND);
}
