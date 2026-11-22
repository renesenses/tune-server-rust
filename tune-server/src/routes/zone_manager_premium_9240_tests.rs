//! #9240 — le multiroom est gratuit jusqu'à 3 zones ou récepteurs dans un
//! groupe, OAAT compris ; au-delà, Premium (décision de Bertrand du
//! 20/11/2026). Les routes du gestionnaire de zones appliquent la règle comme
//! `POST /zones/group` ; une réduction n'est jamais refusée.
//!
//! Contre-épreuve : neutraliser `refus_multiroom_sans_premium` fait tomber les
//! témoins « 4 → 402 ».

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

/// `n` récepteurs OAAT joignables : des ports TCP qui acceptent.
async fn recepteurs(n: usize) -> (Vec<tokio::net::TcpListener>, Value) {
    let mut ecoutes = Vec::new();
    let mut points = Vec::new();
    for _ in 0..n {
        let e = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        points.push(json!({"host": "127.0.0.1", "port": e.local_addr().unwrap().port()}));
        ecoutes.push(e);
    }
    (ecoutes, json!(points))
}

fn groupes_oaat(state: &crate::state::AppState) -> usize {
    SettingsRepo::with_backend(state.backend.clone())
        .get("oaat_groups")
        .unwrap()
        .and_then(|s| serde_json::from_str::<Vec<Value>>(&s).ok())
        .map_or(0, |g| g.len())
}

async fn creer_oaat(state: &crate::state::AppState, n: usize) -> (StatusCode, Value) {
    let (_ecoutes, points) = recepteurs(n).await;
    envoyer(
        state,
        "POST",
        "/oaat-groups",
        json!({ "name": "Maison", "endpoints": points }),
    )
    .await
}

#[tokio::test]
async fn compte_gratuit_oaat_jusqu_a_3_recepteurs_est_permis_9240() {
    for n in [1, 2, 3] {
        let state = etat(false).await;
        let (statut, corps) = creer_oaat(&state, n).await;
        assert_eq!(statut, StatusCode::OK, "{n} récepteur(s) : {corps}");
        assert_eq!(groupes_oaat(&state), 1);
    }
}

#[tokio::test]
async fn compte_gratuit_oaat_4_recepteurs_est_refuse_402_9240() {
    let state = etat(false).await;
    let (statut, corps) = creer_oaat(&state, 4).await;
    assert!(est_le_refus_premium(statut, &corps), "{statut} {corps}");
    assert_eq!(groupes_oaat(&state), 0, "rien ne doit être enregistré");
}

#[tokio::test]
async fn premium_oaat_4_recepteurs_est_cree_9240() {
    let state = etat(true).await;
    let (statut, corps) = creer_oaat(&state, 4).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(groupes_oaat(&state), 1);
}

/// Ajouter un 4e récepteur à un groupe OAAT de 3 : Premium.
#[cfg(feature = "oaat")]
#[tokio::test]
async fn compte_gratuit_un_4e_recepteur_oaat_est_refuse_402_9240() {
    let state = etat(false).await;
    let points: Vec<(String, u16)> = (0..3)
        .map(|i| ("127.0.0.1".to_string(), 9_741 + i))
        .collect();
    let sortie = tune_core::outputs::oaat::OaatMultiroomOutput::new(
        "Maison".into(),
        "oaat-mr-9240".into(),
        points,
    );
    state.outputs.lock().await.register(Box::new(sortie));
    let (statut, corps) = envoyer(
        &state,
        "POST",
        "/oaat-groups/oaat-mr-9240/endpoints",
        json!({ "host": "127.0.0.1", "port": 9_750 }),
    )
    .await;
    assert!(est_le_refus_premium(statut, &corps), "{statut} {corps}");
}

/// Le gestionnaire de zones crée et recompose aussi des groupes de zones.
#[tokio::test]
async fn compte_gratuit_le_gestionnaire_groupe_jusqu_a_3_zones_9240() {
    let state = etat(false).await;
    for zones in [json!([1, 2]), json!([1, 2, 3])] {
        let (statut, corps) = envoyer(
            &state,
            "POST",
            "/groups",
            json!({ "name": "G", "zone_ids": zones }),
        )
        .await;
        assert_eq!(statut, StatusCode::CREATED, "{zones} : {corps}");
    }
    let (statut, corps) = envoyer(
        &state,
        "POST",
        "/groups",
        json!({ "name": "G", "zone_ids": [1, 2, 3, 4] }),
    )
    .await;
    assert!(est_le_refus_premium(statut, &corps), "{statut} {corps}");
}

#[tokio::test]
async fn premium_le_gestionnaire_groupe_4_zones_9240() {
    let state = etat(true).await;
    let (statut, corps) = envoyer(
        &state,
        "POST",
        "/groups",
        json!({ "name": "G", "zone_ids": [1, 2, 3, 4] }),
    )
    .await;
    assert_eq!(statut, StatusCode::CREATED, "{corps}");
}

fn poser_un_groupe(state: &crate::state::AppState, zones: Value) {
    SettingsRepo::with_backend(state.backend.clone())
        .set(
            "zone_groups",
            &json!([{ "id": 1, "name": "G1", "zone_ids": zones }]).to_string(),
        )
        .unwrap();
}

/// Agrandir au-delà de 3 : Premium. Réduire un groupe de 4 (formé quand le
/// compte était Premium) : toujours permis.
#[tokio::test]
async fn compte_gratuit_agrandir_refuse_reduire_permis_9240() {
    let state = etat(false).await;
    poser_un_groupe(&state, json!([1, 2]));
    let (statut, corps) = envoyer(
        &state,
        "PATCH",
        "/groups/1",
        json!({ "zone_ids": [1, 2, 3, 4] }),
    )
    .await;
    assert!(est_le_refus_premium(statut, &corps), "{statut} {corps}");
    let (statut, _) = envoyer(
        &state,
        "PATCH",
        "/groups/1",
        json!({ "zone_ids": [1, 2, 3] }),
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "passer à 3 zones est gratuit");

    poser_un_groupe(&state, json!([1, 2, 3, 4, 5]));
    let (statut, corps) = envoyer(
        &state,
        "PATCH",
        "/groups/1",
        json!({ "zone_ids": [1, 2, 3, 4] }),
    )
    .await;
    assert_eq!(
        statut,
        StatusCode::OK,
        "réduire n'est jamais refusé : {corps}"
    );
    let (statut, _) = envoyer(&state, "PATCH", "/groups/1", json!({ "name": "Rez" })).await;
    assert_eq!(statut, StatusCode::OK);
}
