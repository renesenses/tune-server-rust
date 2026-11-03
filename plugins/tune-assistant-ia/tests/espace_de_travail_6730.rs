//! #6730 point 3 — l'identifiant d'espace de travail Anthropic, par les
//! ROUTES et le VRAI client HTTP, contre un faux « Anthropic » sur
//! 127.0.0.1 qui retient les en-têtes reçus. Aucun appel réseau réel.
//!
//! Ce qui est gardé : l'en-tête `anthropic-workspace-id` part quand
//! l'identifiant est renseigné, et seulement alors ; la clé reste scellée au
//! coffre et ne ressort ni dans les réglages, ni dans les réponses.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_assistant_ia::fournisseur::{ENTETE_ESPACE_DE_TRAVAIL, fabrique_reelle};
use tune_assistant_ia::reglages::{CLE_API_CHIFFREE, CLE_ESPACE_DE_TRAVAIL};
use tune_assistant_ia::{Etat, Passerelle, router_vers};
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::sqlite::SqliteDb;

const CLE: &str = "sk-ant-admin01-ORGANISATION-0123456789-wxyz";
const ESPACE: &str = "wrkspc_01EssaiEspaceDeTravail";

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Les en-têtes de chaque requête reçue : (clé, espace de travail).
type Recues = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// Un faux Anthropic : il rend la réponse de l'API quand l'en-tête d'espace
/// de travail est là, et le 400 « not scoped to a workspace » sinon — la
/// conduite d'une clé d'organisation (constat du 22/10).
async fn faux_anthropic() -> (String, Recues) {
    let recues: Recues = Arc::default();
    let r2 = recues.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |entetes: HeaderMap| {
            let r2 = r2.clone();
            async move {
                let lire = |n: &str| {
                    entetes
                        .get(n)
                        .and_then(|v| v.to_str().ok())
                        .map(String::from)
                };
                let espace = lire(ENTETE_ESPACE_DE_TRAVAIL);
                r2.lock()
                    .unwrap()
                    .push((lire("x-api-key").unwrap_or_default(), espace.clone()));
                if espace.is_none() {
                    return (
                        StatusCode::BAD_REQUEST,
                        axum::Json(json!({"type": "error", "error": {
                            "type": "invalid_request_error",
                            "message": "This API key is not scoped to a workspace, so this request must include the anthropic-workspace-id header with the ID of the workspace to use."
                        }})),
                    );
                }
                (
                    StatusCode::OK,
                    axum::Json(json!({
                        "content": [{"type": "text", "text": "{\"ok\": true}"}],
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 10, "output_tokens": 5}
                    })),
                )
            }
        }),
    );
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = format!("http://{}", ecoute.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(ecoute, app).await.unwrap();
    });
    (adresse, recues)
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

struct Banc {
    app: axum::Router,
    db: Arc<dyn DbBackend>,
    recues: Recues,
    dossier: tempfile::TempDir,
}

async fn banc() -> Banc {
    let db = base();
    let dossier = tempfile::tempdir().unwrap();
    let (adresse, recues) = faux_anthropic().await;
    let etat = Etat::new(
        db.clone(),
        dossier.path(),
        dossier.path().join("cache"),
        Vec::new(),
        fabrique_reelle(),
    );
    let app = router_vers(etat);
    let (s, v) = appel(
        &app,
        "PUT",
        "/reglages",
        Some(json!({"fournisseur": "anthropic", "cle": CLE, "plafond_eur": "5", "url_base": adresse})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    Banc {
        app,
        db,
        recues,
        dossier,
    }
}

/// Sans identifiant : rien n'est ajouté, le 400 est nommé et dit quoi faire.
#[tokio::test]
async fn sans_identifiant_le_400_est_nomme_et_dit_quoi_faire_6730() {
    let b = banc().await;
    let (s, v) = appel(&b.app, "POST", "/tester", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"], "cle_sans_espace_de_travail", "{v}");
    assert!(
        v.to_string().contains("identifiant d'espace de travail"),
        "{v}"
    );
    assert_eq!(*b.recues.lock().unwrap(), vec![(CLE.to_string(), None)]);
}

/// Avec l'identifiant : l'en-tête part, « Tester la clé » réussit, et la
/// vue des réglages rend l'identifiant, jamais la clé.
#[tokio::test]
async fn avec_l_identifiant_l_en_tete_part_et_la_cle_ne_ressort_pas_6730() {
    let b = banc().await;
    let (s, v) = appel(
        &b.app,
        "PUT",
        "/reglages",
        Some(json!({"espace_de_travail_anthropic": format!("  {ESPACE} ")})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["espace_de_travail_anthropic"], ESPACE);
    assert!(!v.to_string().contains(CLE), "la clé ressort : {v}");

    let (s, v) = appel(&b.app, "POST", "/tester", None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(!v.to_string().contains(CLE), "la clé ressort : {v}");
    assert_eq!(
        b.recues.lock().unwrap().last().unwrap(),
        &(CLE.to_string(), Some(ESPACE.to_string())),
        "#6730 : l'en-tête anthropic-workspace-id doit partir"
    );

    // La clé reste au coffre, scellée ; l'identifiant n'est pas un secret.
    let s = SettingsRepo::with_backend(b.db.clone());
    let scelle = s.get(CLE_API_CHIFFREE).unwrap().unwrap();
    assert!(!scelle.contains(CLE));
    assert_eq!(
        s.get(CLE_ESPACE_DE_TRAVAIL).unwrap().as_deref(),
        Some(ESPACE)
    );
}

/// L'ancien `/ai/query` (passerelle de l'hôte) envoie le même en-tête.
#[tokio::test]
async fn la_passerelle_de_l_ancien_assistant_envoie_l_en_tete_6730() {
    let b = banc().await;
    appel(
        &b.app,
        "PUT",
        "/reglages",
        Some(json!({"espace_de_travail_anthropic": ESPACE})),
    )
    .await;
    let p = Passerelle::new(b.db.clone(), b.dossier.path());
    let v = p
        .discuter_anthropic(
            "système",
            &json!([{"role": "user", "content": "bonjour"}]),
            &json!([]),
            64,
        )
        .await
        .expect("le tour passe avec l'espace de travail");
    assert_eq!(v["stop_reason"], "end_turn");
    assert_eq!(
        b.recues.lock().unwrap().last().unwrap().1.as_deref(),
        Some(ESPACE)
    );
}

/// Un identifiant illisible (de quoi casser un en-tête) est refusé, rien
/// n'est écrit ; `""` l'efface.
#[tokio::test]
async fn un_identifiant_illisible_est_refuse_et_vide_l_efface_6730() {
    let b = banc().await;
    let (s, _) = appel(
        &b.app,
        "PUT",
        "/reglages",
        Some(json!({"espace_de_travail_anthropic": "wrkspc_01\r\nX-Injecte: 1"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let repo = SettingsRepo::with_backend(b.db.clone());
    assert_eq!(repo.get(CLE_ESPACE_DE_TRAVAIL).unwrap(), None);

    appel(
        &b.app,
        "PUT",
        "/reglages",
        Some(json!({"espace_de_travail_anthropic": ESPACE})),
    )
    .await;
    let (s, v) = appel(
        &b.app,
        "PUT",
        "/reglages",
        Some(json!({"espace_de_travail_anthropic": ""})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["espace_de_travail_anthropic"], "");
    appel(&b.app, "POST", "/tester", None).await;
    assert_eq!(b.recues.lock().unwrap().last().unwrap().1, None);
}
