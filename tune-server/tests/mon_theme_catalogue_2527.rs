//! tune-web-client#2527 — le greffon « Mon thème » de bout en bout, sur deux
//! démarrages de la MÊME base : proposé au catalogue (sans rien fournir),
//! installé par la route existante, puis actif — sa fiche déclare
//! `provides.web_theme`, et son thème se range PAR PROFIL, jugé par
//! l'extracteur `ActiveProfile` du serveur (l'en-tête `X-Profile-Id` d'un
//! autre profil est ignoré quand l'authentification est active).

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::profile_repo::ProfileRepo;
use tune_core::db::settings_repo::SettingsRepo;

use crate::use_scratch_plugin_data_dir;
use tune_server::state::AppState;

const SECRET: &str = "test-jwt-secret-2527";

fn demarrer(base: &std::path::Path) -> AppState {
    AppState::new(base.to_str().unwrap(), 0, Default::default()).unwrap()
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    profil: Option<i64>,
    jeton: Option<&str>,
    corps: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("content-type", "application/json");
    if let Some(p) = profil {
        req = req.header("X-Profile-Id", p.to_string());
    }
    if let Some(j) = jeton {
        req = req.header(header::AUTHORIZATION, format!("Bearer {j}"));
    }
    let corps = match (methode, corps) {
        (_, Some(c)) => Body::from(c.to_string()),
        ("POST", None) => Body::from("{}"),
        _ => Body::empty(),
    };
    let rep = app.clone().oneshot(req.body(corps).unwrap()).await.unwrap();
    let code = rep.status();
    let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
        .await
        .unwrap();
    (code, serde_json::from_slice(&octets).unwrap_or(Value::Null))
}

async fn fiche(app: &axum::Router) -> Option<Value> {
    let (code, liste) = appel(app, "GET", "/api/v1/plugins", None, None, None).await;
    assert_eq!(code, StatusCode::OK);
    liste
        .as_array()
        .expect("la liste des greffons est un tableau")
        .iter()
        .find(|p| p["name"] == "mon-theme")
        .cloned()
}

fn theme(nom: &str) -> Value {
    json!({
        "format": "tune-theme", "version": 1, "name": nom,
        "colors": {
            "--v2-bg": "#000000", "--v2-bg2": "#080A09", "--v2-surface": "#1F2321",
            "--v2-surface2": "#151817", "--v2-hover": "#101312",
            "--v2-txt": "#FFFFFF", "--v2-txt2": "#D2D8D5", "--v2-txt3": "#A9B1AD",
            "--v2-acc1": "#2A9F5B", "--v2-acc2": "#28BA9B", "--v2-acc-tint": "#E4F9D8",
            "--v2-on-acc": "#000000", "--v2-line": "#2B302D", "--v2-line2": "#3A413D",
            "--v2-active1": "#0F2A1B", "--v2-active2": "#0A1A12", "--v2-player1": "#050606",
            "--v2-player2": "#000000", "--v2-danger": "#FF8F87", "--v2-danger-bd": "#FF6B6B"
        }
    })
}

#[tokio::test]
async fn mon_theme_est_propose_s_installe_et_range_le_theme_par_profil() {
    use_scratch_plugin_data_dir();
    let dossier = tempfile::tempdir().unwrap();
    let base = dossier.path().join("tune.db");

    // ── 1er démarrage : proposé, dormant, et il ne fournit RIEN au client.
    let etat = demarrer(&base);
    let routeurs = tune_server::plugins::init(&etat, "http://127.0.0.1:0", vec![]).await;
    assert!(!routeurs.iter().any(|(n, _)| n == "mon-theme"), "opt-in");
    let app = tune_server::routes::router_with_plugins(etat.clone(), routeurs);
    let f = fiche(&app)
        .await
        .expect("« mon-theme » doit être PROPOSÉ par le gestionnaire (#2527)");
    assert_eq!(f["installed"], false, "{f}");
    assert_eq!(f["premium"], false, "gratuit — {f}");
    assert_eq!(f["compatible"], true, "{f}");
    assert!(f.get("provides").is_none(), "dormant : aucun thème — {f}");
    let (code, rep) = appel(
        &app,
        "POST",
        "/api/v1/plugins/mon-theme/install",
        None,
        None,
        None,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{rep}");
    drop(app);
    drop(etat);

    // ── 2e démarrage : actif, il déclare son thème.
    let etat = demarrer(&base);
    let profils = ProfileRepo::with_backend(etat.backend.clone());
    let alice = profils.create("alice-2527", None, None).unwrap();
    let bruno = profils.create("bruno-2527", None, None).unwrap();
    let routeurs = tune_server::plugins::init(&etat, "http://127.0.0.1:0", vec![]).await;
    let app = tune_server::routes::router_with_plugins(etat.clone(), routeurs);
    let f = fiche(&app).await.expect("un greffon qui tourne est listé");
    assert_eq!(f["enabled"], true, "{f}");
    assert_eq!(
        f["provides"],
        json!({"web_theme": "/api/v1/ext/mon-theme/theme"}),
        "le client découvre le thème par la fiche — {f}"
    );

    let route = "/api/v1/ext/mon-theme/theme";
    let (code, rep) = appel(
        &app,
        "PUT",
        route,
        Some(alice),
        None,
        Some(json!({"theme": theme("Alice")})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{rep}");
    assert_eq!(rep["profile_id"], alice);
    let (_, rep) = appel(&app, "GET", route, Some(alice), None, None).await;
    assert_eq!(rep["theme"]["name"], "Alice");
    let (_, rep) = appel(&app, "GET", route, Some(bruno), None, None).await;
    assert_eq!(rep["theme"], Value::Null, "chacun son thème — {rep}");

    // Un thème illisible ne passe pas, même par l'API.
    let mut gris = theme("Gris");
    gris["colors"]["--v2-txt"] = json!("#111111");
    let (code, rep) = appel(
        &app,
        "PUT",
        route,
        Some(bruno),
        None,
        Some(json!({"theme": gris})),
    )
    .await;
    assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{rep}");
    assert_eq!(rep["error"], "mon_theme.illisible");

    // ── Authentification active : Bruno ne peut pas agir sur le profil
    // d'Alice en posant son en-tête — il écrit et lit SON thème.
    let s = SettingsRepo::with_backend(etat.backend.clone());
    s.set("auth_enabled", "true").unwrap();
    s.set("jwt_secret", SECRET).unwrap();
    let jeton_bruno = tune_server::auth::sign_jwt(bruno, "user", SECRET).unwrap();
    let (code, rep) = appel(
        &app,
        "PUT",
        route,
        Some(alice),
        Some(&jeton_bruno),
        Some(json!({"theme": theme("Bruno")})),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{rep}");
    assert_eq!(rep["profile_id"], bruno, "en-tête d'autrui ignoré — {rep}");
    let jeton_alice = tune_server::auth::sign_jwt(alice, "user", SECRET).unwrap();
    let (_, rep) = appel(&app, "GET", route, None, Some(&jeton_alice), None).await;
    assert_eq!(rep["theme"]["name"], "Alice", "intact — {rep}");
}
