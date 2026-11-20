//! #9240 — le multiroom, OAAT compris, est Premium (décision du 20/11/2026) :
//! les routes du gestionnaire de zones qui créent un groupe ou y ajoutent un
//! membre rendent le même refus 402 que `POST /zones/group`.
//!
//! Contre-épreuve : retirer l'appel à `refus_multiroom_sans_premium` d'une
//! route fait tomber son témoin « compte gratuit ».

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

async fn etat(premium: bool) -> crate::state::AppState {
    let state = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    if premium {
        state.license.set_account_premium(true, None).await;
    }
    state
}

async fn envoyer(
    state: &crate::state::AppState,
    methode: &str,
    uri: &str,
    corps: Value,
) -> (StatusCode, Value) {
    let app = super::router().with_state(state.clone());
    let req = Request::builder()
        .method(methode)
        .uri(uri)
        .header("Content-Type", "application/json")
        .body(Body::from(corps.to_string()))
        .unwrap();
    let reponse = app.oneshot(req).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(json!(null)),
    )
}

fn est_le_refus_premium(statut: StatusCode, corps: &Value) -> bool {
    statut == StatusCode::PAYMENT_REQUIRED
        && corps["error"] == "premium_required"
        && corps["code"] == "multiroom_sync"
}

/// Un récepteur OAAT joignable : un port TCP qui accepte.
async fn recepteur_joignable() -> (tokio::net::TcpListener, u16) {
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ecoute.local_addr().unwrap().port();
    (ecoute, port)
}

fn groupes_oaat(state: &crate::state::AppState) -> usize {
    SettingsRepo::with_backend(state.backend.clone())
        .get("oaat_groups")
        .unwrap()
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
        .map_or(0, |g| g.len())
}

#[tokio::test]
async fn compte_gratuit_un_groupe_oaat_est_refuse_402_9240() {
    let state = etat(false).await;
    let (_ecoute, port) = recepteur_joignable().await;
    let (statut, corps) = envoyer(
        &state,
        "POST",
        "/oaat-groups",
        json!({ "name": "Salon + Cuisine", "endpoints": [{"host": "127.0.0.1", "port": port}] }),
    )
    .await;
    assert!(est_le_refus_premium(statut, &corps), "{statut} {corps}");
    assert_eq!(groupes_oaat(&state), 0, "rien ne doit être enregistré");
}

/// Témoin : le même groupe, avec Premium, est créé.
#[tokio::test]
async fn premium_un_groupe_oaat_est_cree_9240() {
    let state = etat(true).await;
    let (_ecoute, port) = recepteur_joignable().await;
    let (statut, corps) = envoyer(
        &state,
        "POST",
        "/oaat-groups",
        json!({ "name": "Salon + Cuisine", "endpoints": [{"host": "127.0.0.1", "port": port}] }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(groupes_oaat(&state), 1);
}

#[tokio::test]
async fn compte_gratuit_ajouter_un_recepteur_oaat_est_refuse_402_9240() {
    let state = etat(false).await;
    let (statut, corps) = envoyer(
        &state,
        "POST",
        "/oaat-groups/oaat-mr-1/endpoints",
        json!({ "host": "127.0.0.1", "port": 9740 }),
    )
    .await;
    #[cfg(feature = "oaat")]
    assert!(est_le_refus_premium(statut, &corps), "{statut} {corps}");
    #[cfg(not(feature = "oaat"))]
    let _ = (statut, corps);
}

/// Le gestionnaire de zones crée et recompose aussi des groupes de zones :
/// même règle que `POST /zones/group`.
#[tokio::test]
async fn compte_gratuit_le_gestionnaire_ne_groupe_pas_les_zones_9240() {
    let state = etat(false).await;
    let (statut, corps) = envoyer(
        &state,
        "POST",
        "/groups",
        json!({ "name": "Tout", "zone_ids": [1, 2] }),
    )
    .await;
    assert!(est_le_refus_premium(statut, &corps), "{statut} {corps}");

    SettingsRepo::with_backend(state.backend.clone())
        .set(
            "zone_groups",
            &json!([{ "id": 1, "name": "G1", "zone_ids": [1, 2] }]).to_string(),
        )
        .unwrap();
    let (statut, corps) = envoyer(
        &state,
        "PATCH",
        "/groups/1",
        json!({ "zone_ids": [1, 2, 3] }),
    )
    .await;
    assert!(est_le_refus_premium(statut, &corps), "{statut} {corps}");
    // Renommer n'ajoute personne : permis.
    let (statut, _) = envoyer(&state, "PATCH", "/groups/1", json!({ "name": "Rez" })).await;
    assert_eq!(statut, StatusCode::OK);
}

/// Témoin : avec Premium, le gestionnaire crée le groupe.
#[tokio::test]
async fn premium_le_gestionnaire_groupe_les_zones_9240() {
    let state = etat(true).await;
    let (statut, corps) = envoyer(
        &state,
        "POST",
        "/groups",
        json!({ "name": "Tout", "zone_ids": [1, 2] }),
    )
    .await;
    assert_eq!(statut, StatusCode::CREATED, "{corps}");
}
