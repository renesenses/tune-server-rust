//! Réglages communs de l'assistant (décisions de Bertrand du 23/10/2026),
//! éprouvés par le VRAI client HTTP contre de faux fournisseurs locaux
//! (127.0.0.1) — aucun appel réseau réel :
//!
//! - Claude Haiku 5.5 par défaut ;
//! - `output_config.effort: "low"` pour un usage court, et SEULEMENT pour un
//!   modèle qui l'accepte (Haiku 4.5 le refuse par un 400) ;
//! - Ollama reçoit `options.num_ctx` à la taille de la demande.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_assistant_ia::fournisseur::fabrique_reelle;
use tune_assistant_ia::{Etat, router_vers};
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;

const CLE: &str = "sk-ant-api03-ESSAI-0123456789-abcd";

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    uri: &str,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(methode).uri(uri);
    let body = match corps {
        Some(c) => {
            req = req.header("content-type", "application/json");
            Body::from(c.to_string())
        }
        None => Body::empty(),
    };
    let rep = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let statut = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// Un faux serveur qui retient le DERNIER corps reçu et rend `reponse`.
async fn faux(chemin: &'static str, reponse: Value) -> (String, Arc<Mutex<Option<Value>>>) {
    let recu = Arc::new(Mutex::new(None));
    let r2 = recu.clone();
    let app = axum::Router::new().route(
        chemin,
        axum::routing::post(move |axum::Json(corps): axum::Json<Value>| {
            let r2 = r2.clone();
            let reponse = reponse.clone();
            async move {
                *r2.lock().unwrap() = Some(corps);
                axum::Json(reponse)
            }
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    (adresse, recu)
}

fn banc() -> (axum::Router, tempfile::TempDir) {
    let dossier = tempfile::tempdir().unwrap();
    let etat = Etat::new(
        base(),
        dossier.path(),
        dossier.path().join("cache"),
        Vec::new(),
        fabrique_reelle(),
    );
    (router_vers(etat), dossier)
}

fn reponse_anthropic() -> Value {
    json!({
        "content": [{"type": "text", "text": "{\"ok\": true}"}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 5},
    })
}

#[tokio::test]
async fn haiku_5_5_par_defaut_et_effort_bas_pour_un_usage_court() {
    let (app, _d) = banc();
    let (adresse, recu) = faux("/v1/messages", reponse_anthropic()).await;
    let (s, v) = appel(
        &app,
        "PUT",
        "/reglages",
        Some(json!({"fournisseur": "anthropic", "cle": CLE, "plafond_eur": "5", "url_base": adresse})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        v["modele_effectif"], "claude-haiku-5-5",
        "Haiku 5.5 par défaut"
    );

    let (s, t) = appel(&app, "POST", "/tester", None).await;
    assert_eq!(s, StatusCode::OK, "{t}");
    let corps = recu.lock().unwrap().clone().unwrap();
    assert_eq!(corps["model"], "claude-haiku-5-5");
    assert_eq!(
        corps["output_config"]["effort"], "low",
        "« Tester la clé » est un usage court : effort bas attendu : {corps}"
    );
}

#[tokio::test]
async fn haiku_4_5_ne_recoit_jamais_effort() {
    let (app, _d) = banc();
    let (adresse, recu) = faux("/v1/messages", reponse_anthropic()).await;
    let (s, v) = appel(
        &app,
        "PUT",
        "/reglages",
        Some(json!({
            "fournisseur": "anthropic", "cle": CLE, "plafond_eur": "5", "url_base": adresse,
            "modele": "claude-haiku-4-5", "prix_entree_usd_mtok": "1", "prix_sortie_usd_mtok": "5",
        })),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, t) = appel(&app, "POST", "/tester", None).await;
    assert_eq!(s, StatusCode::OK, "{t}");
    let corps = recu.lock().unwrap().clone().unwrap();
    assert_eq!(corps["model"], "claude-haiku-4-5");
    assert!(
        corps.get("output_config").is_none(),
        "Haiku 4.5 refuse `effort` par un 400 : il ne doit pas en recevoir : {corps}"
    );
}

#[tokio::test]
async fn ollama_recoit_num_ctx() {
    let (app, _d) = banc();
    let (adresse, recu) = faux(
        "/api/chat",
        json!({"message": {"role": "assistant", "content": "{\"ok\": true}"}, "prompt_eval_count": 10, "eval_count": 5}),
    )
    .await;
    let (s, v) = appel(
        &app,
        "PUT",
        "/reglages",
        Some(json!({"fournisseur": "ollama", "modele": "llama3.2", "plafond_eur": "5", "url_base": adresse})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, t) = appel(&app, "POST", "/tester", None).await;
    assert_eq!(s, StatusCode::OK, "{t}");
    let corps = recu.lock().unwrap().clone().unwrap();
    let n = corps["options"]["num_ctx"].as_u64().unwrap_or(0);
    assert!(
        n >= u64::from(tune_assistant_ia::couts::NUM_CTX_MIN),
        "Ollama doit recevoir options.num_ctx (sinon il tronque sans erreur) : {corps}"
    );
    assert!(corps.get("output_config").is_none(), "{corps}");
}
