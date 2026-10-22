//! Chantier « Assistant IA » — `POST /ai/query` rend le coût EXACT de la
//! question (`cout_eur`) : la somme de ce qui a été compté à chacun de ses
//! tours, au tarif et au taux de l'assistant. Ni la dépense du mois, ni une
//! différence avant/après (Refs #6268).
//!
//! Par la ROUTE MONTÉE, contre un faux fournisseur compatible OpenAI sur
//! 127.0.0.1 : aucun appel réseau réel.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! par sa cible `[[test]]` de `tune-server/Cargo.toml`. Il pose
//! `TUNE_DB_PATH` (le dossier de la clé maîtresse) : un seul essai, dans sa
//! propre cible.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_assistant_ia::reglages::{self, Modification};
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

const CLE: &str = "sk-proj-COUT-EXACT-0123-wxyz";

/// Au tour impair, un appel d'outil (100 000 jetons d'entrée, 50 000 de
/// sortie) ; au tour pair, la réponse en texte (30 000 / 10 000).
async fn faux_fournisseur() -> (String, Arc<Mutex<usize>>) {
    let n = Arc::new(Mutex::new(0usize));
    let n2 = n.clone();
    let app = axum::Router::new().route(
        "/chat/completions",
        axum::routing::post(move |axum::Json(_corps): axum::Json<Value>| {
            let n2 = n2.clone();
            async move {
                let k = {
                    let mut g = n2.lock().unwrap();
                    *g += 1;
                    *g
                };
                axum::Json(if k % 2 == 1 {
                    json!({"choices": [{"message": {"role": "assistant", "content": null,
                        "tool_calls": [{"id": format!("c{k}"), "type": "function",
                            "function": {"name": "search_library",
                                "arguments": "{\"query\":\"Kind of Blue\"}"}}]}}],
                        "usage": {"prompt_tokens": 100_000, "completion_tokens": 50_000}})
                } else {
                    json!({"choices": [{"message": {"role": "assistant",
                        "content": "Je lance Kind of Blue."}}],
                        "usage": {"prompt_tokens": 30_000, "completion_tokens": 10_000}})
                })
            }
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    (adresse, n)
}

async fn question(state: &AppState) -> Value {
    let reponse = tune_server::routes::router(state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/ai/query")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"message": "joue Kind of Blue"}).to_string(),
                ))
                .expect("requête"),
        )
        .await
        .expect("réponse");
    assert_eq!(reponse.status(), StatusCode::OK);
    let octets = axum::body::to_bytes(reponse.into_body(), 1 << 20)
        .await
        .expect("corps");
    serde_json::from_slice(&octets).expect("JSON")
}

fn proche(a: &Value, attendu: f64) -> bool {
    a.as_f64().is_some_and(|x| (x - attendu).abs() < 1e-9)
}

#[tokio::test]
async fn ai_query_rend_le_cout_exact_de_chaque_question() {
    let dossier = tempfile::tempdir().expect("dossier");
    // SAFETY : seul essai de cette cible, posé avant toute lecture.
    unsafe { std::env::set_var("TUNE_DB_PATH", dossier.path().join("tune.db")) };
    let state = AppState::new(":memory:", 0, Default::default()).expect("AppState sur SQLite");
    let (adresse, recues) = faux_fournisseur().await;
    reglages::appliquer(
        &state.backend,
        &tune_assistant_ia::coffre::du_greffon(dossier.path()),
        &Modification {
            fournisseur: Some("compatible_openai".into()),
            modele: Some("mon-modele".into()),
            url_base: Some(adresse),
            cle: Some(CLE.into()),
            prix_entree_usd_mtok: Some(json!("1")),
            prix_sortie_usd_mtok: Some(json!("2")),
            // Le taux compte : 0,5 € le dollar.
            taux_usd_eur: Some(json!("0,5")),
            plafond_eur: Some(json!("10")),
            ..Default::default()
        },
    )
    .expect("réglages");

    // Tour 1 : 0,10 $ + 0,10 $ ; tour 2 : 0,03 $ + 0,02 $. 0,25 $ × 0,5.
    let q1 = question(&state).await;
    assert_eq!(q1["reply"], "Je lance Kind of Blue.", "{q1}");
    assert!(
        proche(&q1["cout_eur"], 0.125),
        "coût de la question 1 : {q1}"
    );
    assert!(proche(&q1["plafond"]["depense_eur"], 0.125), "{q1}");

    // La question 2 rend SON coût, pas la dépense du mois.
    let q2 = question(&state).await;
    assert!(
        proche(&q2["cout_eur"], 0.125),
        "coût de la question 2 : {q2}"
    );
    assert!(proche(&q2["plafond"]["depense_eur"], 0.25), "{q2}");
    assert_eq!(*recues.lock().unwrap(), 4);

    // Plafond à 0,30 € : le tour 1 passe (0,25 < 0,30, +0,10 €), le tour 2
    // est refusé sans partir. Le tour payé reste dans le coût.
    SettingsRepo::with_backend(state.backend.clone())
        .set("assistant_ia_plafond_eur", "0,30")
        .expect("plafond");
    let q3 = question(&state).await;
    assert_eq!(q3["error"], "plafond_atteint", "{q3}");
    assert!(
        proche(&q3["cout_eur"], 0.1),
        "coût de la question refusée : {q3}"
    );
    assert_eq!(q3["plafond"]["atteint"], true, "{q3}");
    assert_eq!(*recues.lock().unwrap(), 5);
    assert!(!q3.to_string().contains(CLE));
}
