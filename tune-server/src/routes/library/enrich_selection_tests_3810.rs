//! Route réelle, SQLite mémoire et événements observables ; aucun réseau amont.
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

async fn requete(app: &axum::Router, method: &str, path: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("Content-Type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn une_selection_en_echec_refuse_le_lancement_sans_fausse_reussite() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let precedent = json!({
        "status": "done", "task_id": "passe-precedente",
        "enriched": 3, "errors": 0, "total": 3,
    });
    settings
        .set("enrich_all_status", &precedent.to_string())
        .unwrap();
    // Base isolée : les réglages restent fonctionnels, seule la requête de
    // candidats perd une table requise. Aucun faux résultat de mock.
    state
        .backend
        .execute("ALTER TABLE tracks RENAME TO tracks_indisponibles", &[])
        .unwrap();
    let app = crate::routes::router(state.clone());
    let mut events = state.event_bus.subscribe();

    let (status, body) = requete(&app, "POST", "/api/v1/library/enrich-all").await;
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "une panne de sélection ne doit pas être acceptée comme une passe vide réussie : {body}"
    );
    assert_eq!(body, json!({"error": "enrichment_candidates_unavailable"}));
    assert!(body.get("task_id").is_none());
    let (status, current) = requete(&app, "GET", "/api/v1/library/enrich-all/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        current, precedent,
        "une passe refusée ne doit pas écraser l'état précédent"
    );
    assert!(state.background_tasks.snapshot().is_empty());
    assert!(
        matches!(
            events.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ),
        "le refus ne doit annoncer ni tâche ni progression ni fin réussie"
    );
    // Le gate est conservé avant la sélection, avec sa consommation existante.
    assert_eq!(
        settings.get("enrichment_daily_count").unwrap().as_deref(),
        Some("1")
    );
    settings.set("enrichment_daily_count", "999").unwrap();
    let (status, body) = requete(&app, "POST", "/api/v1/library/enrich-all").await;
    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "le quota doit encore précéder la sélection SQL"
    );
    assert_eq!(body["error"], "free_tier_daily_enrichment_limit_reached");
}

#[tokio::test]
async fn une_bibliotheque_vide_valide_reste_acceptee_et_terminee_sans_erreur() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let app = crate::routes::router(state.clone());
    let mut events = state.event_bus.subscribe();
    let (status, accepted) = requete(&app, "POST", "/api/v1/library/enrich-all").await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let task_id = accepted["task_id"]
        .as_str()
        .expect("identifiant de la passe");
    assert_eq!(accepted["status"], "accepted");

    let completed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.event_type == "library.enrich.completed" {
                break event;
            }
        }
    })
    .await
    .expect("une vraie sélection vide doit terminer sans appel MusicBrainz");
    assert_eq!(completed.data["task_id"], task_id);
    assert_eq!(completed.data["enriched"], 0);
    assert_eq!(completed.data["errors"], 0);
    assert_eq!(completed.data["total"], 0);
    let (status, current) = requete(&app, "GET", "/api/v1/library/enrich-all/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(current["status"], "done");
    assert_eq!(current["task_id"], task_id);
    assert_eq!(current["enriched"], 0);
    assert_eq!(current["errors"], 0);
    assert_eq!(current["total"], 0);
}
