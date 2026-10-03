//! #5669 / fil forum 2111 — les JOURS d'un réveil étaient ignorés.
//!
//! L'écran Réveils (v2) n'envoie que `days`. `POST /alarms` écrivait alors
//! `days_of_week = "1111111"`, et le planificateur donne la priorité à ce
//! masque : un réveil « en semaine » sonnait aussi le samedi et le dimanche.
//!
//! Convention unique, celle du serveur partout : 0 = lundi … 6 = dimanche.
//! On lit le réveil comme le planificateur le lit (`get_alarm` +
//! `resolve_alarm_days`), pas seulement la colonne.
use axum::{
    body::Body,
    http::{Method, Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::alarms::{AlarmScheduler, resolve_alarm_days};
use tune_server::state::AppState;

const SAMEDI: u32 = 5;
const DIMANCHE: u32 = 6;

async fn request(s: &AppState, method: Method, path: &str, body: Value) -> (StatusCode, Value) {
    let r = tune_server::routes::router(s.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let bytes = axum::body::to_bytes(r.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes))),
    )
}

async fn etat(premium: bool) -> AppState {
    let s = AppState::new(":memory:", 0, Default::default()).unwrap();
    s.license.set_account_premium(premium, None).await;
    s
}

/// Les jours tels que le planificateur les lit pour ce réveil.
fn jours_du_planificateur(s: &AppState, id: i64) -> Vec<u32> {
    let sched = AlarmScheduler::with_backend(s.backend.clone(), s.orchestrator.clone());
    resolve_alarm_days(&sched.get_alarm(id).unwrap().unwrap())
}

fn reveil(days: Value, days_of_week: Option<&str>) -> Value {
    let mut b = json!({
        "name": "Réveil", "time": "10:42", "source_type": "radio",
        "source_id": "https://icecast.radiofrance.fr/franceinter-hifi.aac",
        "volume": 0.5, "fade_in_seconds": 0, "days": days,
    });
    if let Some(m) = days_of_week {
        b["days_of_week"] = json!(m);
    }
    b
}

async fn creer(s: &AppState, body: Value) -> i64 {
    let (st, v) = request(s, Method::POST, "/api/v1/alarms", body).await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    v["id"].as_i64().unwrap()
}

#[tokio::test]
async fn i5669_reveil_en_semaine_par_days_ne_sonne_pas_le_samedi() {
    let s = etat(true).await;
    // Ce qu'envoie un client qui ne connaît que `days` (0 = lundi).
    let id = creer(&s, reveil(json!("0,1,2,3,4"), None)).await;
    let jours = jours_du_planificateur(&s, id);
    assert!(
        !jours.contains(&SAMEDI) && !jours.contains(&DIMANCHE),
        "réveil « en semaine » programmé {jours:?} (samedi = 5, dimanche = 6)"
    );
    assert_eq!(jours, vec![0, 1, 2, 3, 4]);
}

#[tokio::test]
async fn i5669_dimanche_est_six_pas_zero() {
    let s = etat(true).await;
    let id = creer(&s, reveil(json!("6"), None)).await;
    assert_eq!(jours_du_planificateur(&s, id), vec![DIMANCHE]);
    let id = creer(&s, reveil(json!("0"), None)).await;
    assert_eq!(jours_du_planificateur(&s, id), vec![0], "0 = lundi");
}

#[tokio::test]
async fn i5669_put_days_seul_met_le_masque_a_jour() {
    let s = etat(true).await;
    let id = creer(&s, reveil(json!("daily"), None)).await;
    assert_eq!(jours_du_planificateur(&s, id), (0..7).collect::<Vec<_>>());
    let (st, v) = request(
        &s,
        Method::PUT,
        &format!("/api/v1/alarms/{id}"),
        json!({"days": "5,6"}),
    )
    .await;
    assert!(st.is_success(), "{st}: {v}");
    assert_eq!(jours_du_planificateur(&s, id), vec![SAMEDI, DIMANCHE]);
}

#[tokio::test]
async fn i5669_le_masque_explicite_prime_et_rien_ne_change_sans_jours() {
    let s = etat(true).await;
    // Masque explicite : c'est lui que lit le planificateur.
    let id = creer(&s, reveil(json!("0,1,2,3,4"), Some("1111100"))).await;
    assert_eq!(jours_du_planificateur(&s, id), vec![0, 1, 2, 3, 4]);
    // Aucun jour envoyé du tout : comportement d'avant, inchangé.
    let mut b = reveil(Value::Null, None);
    b.as_object_mut().unwrap().remove("days");
    let id = creer(&s, b).await;
    assert_eq!(jours_du_planificateur(&s, id), (0..7).collect::<Vec<_>>());
}

#[tokio::test]
async fn i5669_jour_invalide_refuse_en_422() {
    let s = etat(true).await;
    for body in [
        reveil(json!("1,9"), None),
        reveil(json!("7"), None),
        reveil(json!("lundi"), None),
        reveil(json!(""), None),
        reveil(json!("0,1"), Some("11111x0")),
        reveil(json!("0,1"), Some("111")),
    ] {
        let (st, v) = request(&s, Method::POST, "/api/v1/alarms", body.clone()).await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body} → {v}");
    }
    let id = creer(&s, reveil(json!("0"), None)).await;
    for body in [json!({"days": "8"}), json!({"days_of_week": "abcdefg"})] {
        let (st, v) = request(
            &s,
            Method::PUT,
            &format!("/api/v1/alarms/{id}"),
            body.clone(),
        )
        .await;
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body} → {v}");
    }
    assert_eq!(
        jours_du_planificateur(&s, id),
        vec![0],
        "rien n'a été écrit"
    );
}

#[tokio::test]
async fn i5669_gratuit_le_choix_des_jours_passe_par_la_meme_porte() {
    // Le palier gratuit refusait déjà `days_of_week != 1111111` (402). Passer
    // par `days` contournait la porte… et le réveil sonnait tous les jours.
    let s = etat(false).await;
    let (st, v) = request(
        &s,
        Method::POST,
        "/api/v1/alarms",
        reveil(json!("0,1,2,3,4"), None),
    )
    .await;
    assert_eq!(st, StatusCode::PAYMENT_REQUIRED, "{v}");
    let id = creer(&s, reveil(json!("daily"), None)).await;
    assert_eq!(jours_du_planificateur(&s, id), (0..7).collect::<Vec<_>>());
    let (st, v) = request(
        &s,
        Method::PUT,
        &format!("/api/v1/alarms/{id}"),
        json!({"days": "0,1,2,3,4"}),
    )
    .await;
    assert_eq!(st, StatusCode::PAYMENT_REQUIRED, "{v}");
}
