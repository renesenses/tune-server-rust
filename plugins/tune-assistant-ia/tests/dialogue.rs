//! Étape 3 — l'ancien assistant (`POST /ai/query`) dialogue avec TOUS les
//! fournisseurs de l'assistant, par la passerelle. Faux serveurs sur
//! 127.0.0.1 (compatible OpenAI, Ollama) : aucun appel réseau réel.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tune_assistant_ia::Passerelle;
use tune_assistant_ia::reglages::{self, Modification};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::sqlite::SqliteDb;

const CLE: &str = "sk-proj-DIALOGUE-ETAPE-TROIS-0123-wxyz";

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Les requêtes reçues : (en-tête Authorization, corps).
type Recues = Arc<Mutex<Vec<(String, Value)>>>;

/// Un faux serveur : au premier tour il demande un outil, au second il
/// répond en texte. `chemin` : `/chat/completions` ou `/api/chat`.
async fn faux_serveur(chemin: &'static str, ollama: bool) -> (String, Recues) {
    let recues: Recues = Arc::new(Mutex::new(Vec::new()));
    let r2 = recues.clone();
    let app = axum::Router::new().route(
        chemin,
        axum::routing::post(
            move |entetes: axum::http::HeaderMap, axum::Json(corps): axum::Json<Value>| {
                let r2 = r2.clone();
                async move {
                    let auth = entetes
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    let n = {
                        let mut r = r2.lock().unwrap();
                        r.push((auth, corps));
                        r.len()
                    };
                    let message = if n == 1 {
                        if ollama {
                            json!({"role": "assistant", "content": "",
                                "tool_calls": [{"function": {"name": "search_library",
                                    "arguments": {"query": "Kind of Blue"}}}]})
                        } else {
                            json!({"role": "assistant", "content": null,
                                "tool_calls": [{"id": "c1", "type": "function",
                                    "function": {"name": "search_library",
                                        "arguments": "{\"query\":\"Kind of Blue\"}"}}]})
                        }
                    } else {
                        json!({"role": "assistant", "content": "Je lance Kind of Blue."})
                    };
                    axum::Json(if ollama {
                        json!({"message": message, "prompt_eval_count": 100_000, "eval_count": 0})
                    } else {
                        json!({"choices": [{"message": message}],
                            "usage": {"prompt_tokens": 100_000, "completion_tokens": 0}})
                    })
                }
            },
        ),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    (adresse, recues)
}

fn outils() -> Value {
    json!([{"name": "search_library", "description": "Search the local library",
        "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}}}])
}

/// Les deux tours tels que `tune-server/src/routes/ai.rs` les écrit.
async fn deux_tours(p: &Passerelle) -> (Value, Value) {
    let mut messages = json!([{"role": "user", "content": "joue Kind of Blue"}]);
    let v1 = p
        .discuter("système", &messages, &outils(), 1024)
        .await
        .expect("tour 1");
    let appel = v1["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["type"] == "tool_use")
        .cloned()
        .expect("un appel d'outil");
    messages
        .as_array_mut()
        .unwrap()
        .push(json!({"role": "assistant", "content": v1["content"]}));
    messages
        .as_array_mut()
        .unwrap()
        .push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": appel["id"], "content": "{\"albums\":[]}"}
        ]}));
    let v2 = p
        .discuter("système", &messages, &outils(), 1024)
        .await
        .expect("tour 2");
    (appel, v2)
}

fn conso(db: &Arc<dyn DbBackend>) -> Value {
    serde_json::from_str(
        &SettingsRepo::with_backend(db.clone())
            .get("assistant_ia_consommation")
            .unwrap()
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn le_dialogue_a_outils_part_chez_un_fournisseur_compatible_openai() {
    let db = base();
    let dossier = tempfile::tempdir().unwrap();
    let (adresse, recues) = faux_serveur("/chat/completions", false).await;
    reglages::appliquer(
        &db,
        &tune_assistant_ia::coffre::du_greffon(dossier.path()),
        &Modification {
            fournisseur: Some("compatible_openai".into()),
            modele: Some("mon-modele".into()),
            url_base: Some(adresse),
            cle: Some(CLE.into()),
            // 1 $ le million de jetons : 100 000 jetons = 0,10 € (taux 1).
            prix_entree_usd_mtok: Some(json!("1")),
            prix_sortie_usd_mtok: Some(json!("1")),
            plafond_eur: Some(json!("0,25")),
            ..Default::default()
        },
    )
    .unwrap();
    let p = Passerelle::new(db.clone(), dossier.path());
    assert!(p.dialogue_pret());
    assert!(!p.anthropic_pret());

    let (appel, v2) = deux_tours(&p).await;
    assert_eq!(appel["name"], "search_library");
    assert_eq!(appel["input"]["query"], "Kind of Blue");
    assert_eq!(v2["content"][0]["text"], "Je lance Kind of Blue.");
    assert_eq!(v2["stop_reason"], "end_turn");

    let r = recues.lock().unwrap().clone();
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].0, format!("Bearer {CLE}"));
    // Les outils partent en fonctions, la consigne en tête.
    assert_eq!(r[0].1["tools"][0]["type"], "function");
    assert_eq!(r[0].1["tools"][0]["function"]["name"], "search_library");
    assert_eq!(r[0].1["messages"][0]["role"], "system");
    assert_eq!(r[0].1["model"], "mon-modele");
    // Le résultat de l'outil revient en message `tool`, rattaché à l'appel.
    let m = r[1].1["messages"].as_array().unwrap();
    assert_eq!(m[2]["tool_calls"][0]["id"], "c1");
    assert_eq!(m[3]["role"], "tool");
    assert_eq!(m[3]["tool_call_id"], "c1");

    // Compté pour l'usage `requete`, au tarif saisi, avec l'alerte à 80 %.
    let c = conso(&db);
    assert_eq!(c["par_usage"]["requete"]["appels"], 2, "{c}");
    assert_eq!(c["micro_eur"], 200_000, "{c}");
    assert_eq!(p.etat_plafond()["alerte"], true);
    assert_eq!(p.etat_plafond()["atteint"], false);

    // Un troisième tour passe (0,20 < 0,25), le quatrième est refusé sans
    // partir.
    let messages = json!([{"role": "user", "content": "et après ?"}]);
    p.discuter("système", &messages, &outils(), 1024)
        .await
        .expect("tour 3");
    let e = p
        .discuter("système", &messages, &outils(), 1024)
        .await
        .unwrap_err();
    assert_eq!(e.code(), "plafond_atteint");
    assert_eq!(recues.lock().unwrap().len(), 3);
    // La clé n'est jamais dans la consommation.
    assert!(!c.to_string().contains(CLE));
}

#[tokio::test]
async fn le_dialogue_a_outils_part_chez_ollama_sans_cle_et_sans_cout() {
    let db = base();
    let dossier = tempfile::tempdir().unwrap();
    let (adresse, recues) = faux_serveur("/api/chat", true).await;
    reglages::appliquer(
        &db,
        &tune_assistant_ia::coffre::du_greffon(dossier.path()),
        &Modification {
            fournisseur: Some("ollama".into()),
            modele: Some("llama3.2".into()),
            url_base: Some(adresse),
            ..Default::default()
        },
    )
    .unwrap();
    let p = Passerelle::new(db.clone(), dossier.path());
    assert!(p.dialogue_pret());

    let (appel, v2) = deux_tours(&p).await;
    assert_eq!(appel["id"], "appel_0", "Ollama ne donne pas d'identifiant");
    assert_eq!(appel["input"]["query"], "Kind of Blue");
    assert_eq!(v2["content"][0]["text"], "Je lance Kind of Blue.");

    let r = recues.lock().unwrap().clone();
    assert_eq!(r[0].0, "", "aucune clé vers Ollama");
    assert_eq!(r[0].1["stream"], false);
    assert_eq!(r[0].1["tools"][0]["function"]["name"], "search_library");
    let m = r[1].1["messages"].as_array().unwrap();
    assert_eq!(
        m[2]["tool_calls"][0]["function"]["arguments"]["query"],
        "Kind of Blue"
    );
    assert_eq!(m[3]["role"], "tool");
    assert_eq!(m[3]["tool_name"], "search_library");

    let c = conso(&db);
    assert_eq!(c["par_usage"]["requete"]["appels"], 2, "{c}");
    assert_eq!(c["micro_eur"], 0, "Ollama ne coûte rien : {c}");
}

#[tokio::test]
async fn sans_fournisseur_le_dialogue_n_est_pas_pret() {
    let db = base();
    let dossier = tempfile::tempdir().unwrap();
    let p = Passerelle::new(db, dossier.path());
    assert!(!p.dialogue_pret());
    let e = p
        .discuter(
            "s",
            &json!([{"role": "user", "content": "x"}]),
            &outils(),
            64,
        )
        .await
        .unwrap_err();
    assert_eq!(e.code(), "non_configure");
}
