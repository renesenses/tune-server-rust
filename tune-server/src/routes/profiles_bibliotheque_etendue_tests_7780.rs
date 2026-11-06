//! #7780 — témoins du réglage « Bibliothèque élargie aux favoris des services ».
//!
//! Par le routeur réel (`crate::routes::router`, préfixe `/api/v1`), sans
//! authentification ; le profil appelant est désigné par `X-Profile-Id`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::profile_repo::ProfileRepo;
use tune_core::db::streaming_favorites_repo::StreamingFavoritesRepo;

use crate::state::AppState;

fn etat() -> (AppState, i64, i64) {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let repo = ProfileRepo::with_backend(state.backend.clone());
    let gerard = repo.create("gerard", Some("Gérard"), None).unwrap();
    let anne = repo.create("anne", Some("Anne"), None).unwrap();
    (state, gerard, anne)
}

async fn appel(state: &AppState, req: Request<Body>) -> (StatusCode, Value) {
    let resp = crate::routes::router(state.clone())
        .oneshot(req)
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

async fn lire_reglage(state: &AppState, profil: i64) -> (StatusCode, Value) {
    let req = Request::get(format!(
        "/api/v1/profiles/{profil}/settings/bibliotheque-etendue"
    ))
    .header("X-Profile-Id", profil.to_string())
    .body(Body::empty())
    .unwrap();
    appel(state, req).await
}

async fn ecrire_reglage(state: &AppState, profil: i64, actif: bool) -> StatusCode {
    let req = Request::put(format!(
        "/api/v1/profiles/{profil}/settings/bibliotheque-etendue"
    ))
    .header("X-Profile-Id", profil.to_string())
    .header("Content-Type", "application/json")
    .body(Body::from(json!({ "enabled": actif }).to_string()))
    .unwrap();
    appel(state, req).await.0
}

async fn favoris(state: &AppState, profil: i64, requete: &str) -> Vec<Value> {
    let req = Request::get(format!(
        "/api/v1/profiles/{profil}/favorites/streaming?{requete}"
    ))
    .header("X-Profile-Id", profil.to_string())
    .body(Body::empty())
    .unwrap();
    let (statut, corps) = appel(state, req).await;
    assert_eq!(statut, StatusCode::OK, "favoris de service : {corps}");
    corps.as_array().cloned().unwrap_or_default()
}

fn poser_un_favori_qobuz(state: &AppState, profil: i64) {
    StreamingFavoritesRepo::with_backend(state.backend.clone())
        .add(
            profil,
            "album",
            "qobuz",
            "0060253780",
            Some("Kind of Blue"),
            Some("Miles Davis"),
            None,
            None,
        )
        .unwrap();
}

#[tokio::test]
async fn la_bibliotheque_elargie_est_activee_par_defaut_7780() {
    let (state, gerard, _) = etat();
    let (statut, corps) = lire_reglage(&state, gerard).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        corps,
        json!({ "enabled": true }),
        "sans réglage écrit, la bibliothèque élargie doit être ACTIVÉE (défaut de #7780)"
    );
}

#[tokio::test]
async fn desactiver_la_bibliotheque_elargie_est_retenu_pour_ce_profil_seul_7780() {
    let (state, gerard, anne) = etat();
    assert_eq!(ecrire_reglage(&state, gerard, false).await, StatusCode::OK);
    assert_eq!(
        lire_reglage(&state, gerard).await.1,
        json!({ "enabled": false }),
        "le réglage désactivé doit être relu désactivé"
    );
    assert_eq!(
        lire_reglage(&state, anne).await.1,
        json!({ "enabled": true }),
        "désactiver pour un profil ne doit pas toucher les autres"
    );
    assert_eq!(ecrire_reglage(&state, gerard, true).await, StatusCode::OK);
    assert_eq!(
        lire_reglage(&state, gerard).await.1,
        json!({ "enabled": true })
    );
}

#[tokio::test]
async fn le_reglage_d_un_autre_profil_n_est_ni_lu_ni_ecrit_7780() {
    let (state, gerard, anne) = etat();
    let req = Request::put(format!(
        "/api/v1/profiles/{anne}/settings/bibliotheque-etendue"
    ))
    .header("X-Profile-Id", gerard.to_string())
    .header("Content-Type", "application/json")
    .body(Body::from(json!({ "enabled": false }).to_string()))
    .unwrap();
    assert_eq!(appel(&state, req).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        lire_reglage(&state, anne).await.1,
        json!({ "enabled": true })
    );
}

#[tokio::test]
async fn la_bibliotheque_recoit_les_favoris_de_service_seulement_reglage_active_7780() {
    let (state, gerard, _) = etat();
    poser_un_favori_qobuz(&state, gerard);

    let actives = favoris(&state, gerard, "item_type=album&pour=bibliotheque").await;
    assert_eq!(
        actives.len(),
        1,
        "réglage activé, la Bibliothèque doit recevoir le favori Qobuz : {actives:?}"
    );
    assert_eq!(actives[0]["service"], "qobuz");

    ecrire_reglage(&state, gerard, false).await;
    let desactives = favoris(&state, gerard, "item_type=album&pour=bibliotheque").await;
    assert!(
        desactives.is_empty(),
        "réglage désactivé, la Bibliothèque ne doit recevoir AUCUN favori de service : {desactives:?}"
    );

    let ecran_favoris = favoris(&state, gerard, "item_type=album").await;
    assert_eq!(
        ecran_favoris.len(),
        1,
        "l'écran Favoris (sans `pour`) garde ses favoris de service, réglage désactivé ou non"
    );
}
