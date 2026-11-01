//! Les routes de « Mon thème » (#2527) : rangement PAR PROFIL, refus d'un
//! thème illisible ou mal formé, bascule `actif`, effacement.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode, request::Parts};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::sqlite::SqliteDb;
use tune_core::plugin_sdk::TunePlugin;
use tune_mon_theme::{HostServices, Hote, MonThemePlugin, cle, router};

/// L'hôte de l'essai : le profil est celui de `X-Profile-Id`, 1 sinon. Le
/// vrai juge (l'extracteur `ActiveProfile`) est essayé côté serveur.
struct HoteEssai;

#[async_trait]
impl Hote for HoteEssai {
    async fn profil_actif(&self, parts: &mut Parts) -> i64 {
        parts
            .headers
            .get("X-Profile-Id")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
    }
}

fn base() -> Arc<dyn DbBackend> {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

fn services(backend: &Arc<dyn DbBackend>) -> HostServices {
    HostServices {
        backend: backend.clone(),
        hote: Arc::new(HoteEssai),
    }
}

fn theme() -> Value {
    json!({
        "format": "tune-theme", "version": 1, "name": "Nuit",
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

async fn appel(
    backend: &Arc<dyn DbBackend>,
    methode: &str,
    chemin: &str,
    profil: i64,
    corps: Option<Value>,
) -> (StatusCode, Value, Option<String>) {
    let req = Request::builder()
        .method(methode)
        .uri(chemin)
        .header("X-Profile-Id", profil.to_string())
        .header("content-type", "application/json")
        .body(match corps {
            Some(c) => Body::from(c.to_string()),
            None => Body::empty(),
        })
        .unwrap();
    let r = router(services(backend)).oneshot(req).await.unwrap();
    let statut = r.status();
    let cache = r
        .headers()
        .get("cache-control")
        .map(|v| v.to_str().unwrap().to_string());
    let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    (statut, serde_json::from_slice(&octets).unwrap(), cache)
}

#[tokio::test]
async fn sans_theme_le_profil_recoit_null_et_inactif() {
    let b = base();
    let (s, v, cache) = appel(&b, "GET", "/theme", 3, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v, json!({"profile_id": 3, "actif": false, "theme": null}));
    assert_eq!(cache.as_deref(), Some("no-store"));
}

#[tokio::test]
async fn le_theme_est_range_par_profil() {
    let b = base();
    let (s, v, _) = appel(&b, "PUT", "/theme", 2, Some(json!({"theme": theme()}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["actif"], true);
    assert_eq!(v["theme"]["name"], "Nuit");

    // Le profil 2 le retrouve (un autre appareil, même profil)…
    let (_, v2, _) = appel(&b, "GET", "/theme", 2, None).await;
    assert_eq!(v2["theme"], v["theme"]);
    // … le profil 5 ne le voit pas.
    let (_, v5, _) = appel(&b, "GET", "/theme", 5, None).await;
    assert_eq!(v5["theme"], Value::Null);

    // Rangé sous la clé du profil 2, et seulement elle.
    let s = SettingsRepo::with_backend(b.clone());
    assert!(s.get(&cle(2)).unwrap().is_some());
    assert!(s.get(&cle(5)).unwrap().is_none());
}

#[tokio::test]
async fn un_theme_illisible_est_refuse_et_rien_n_est_range() {
    let b = base();
    let mut t = theme();
    t["colors"]["--v2-txt2"] = json!("#202020");
    let (s, v, _) = appel(&b, "PUT", "/theme", 1, Some(json!({"theme": t}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["error"], "mon_theme.illisible");
    assert!(
        v["paires"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["texte"] == "--v2-txt2" && p["minimum"] == 4.5)
    );
    let (_, lu, _) = appel(&b, "GET", "/theme", 1, None).await;
    assert_eq!(lu["theme"], Value::Null);
}

#[tokio::test]
async fn une_forme_fausse_est_refusee_par_son_champ() {
    let b = base();
    let mut t = theme();
    t["colors"]["--v2-bg"] = json!("url(javascript:x)");
    let (s, v, _) = appel(&b, "PUT", "/theme", 1, Some(json!({"theme": t}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        v,
        json!({"error": "mon_theme.invalide", "champ": "--v2-bg"})
    );

    let (s, v, _) = appel(&b, "PUT", "/theme", 1, Some(json!("texte"))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["champ"], "body");

    let (s, v, _) = appel(
        &b,
        "PUT",
        "/theme",
        1,
        Some(json!({"theme": theme(), "actif": "oui"})),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["champ"], "actif");
}

#[tokio::test]
async fn actif_bascule_sans_perdre_le_theme_et_l_effacement_l_oublie() {
    let b = base();
    let (s, v, _) = appel(&b, "PUT", "/theme/actif", 4, Some(json!({"actif": true}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(v["error"], "mon_theme.aucun");

    appel(&b, "PUT", "/theme", 4, Some(json!({"theme": theme()}))).await;
    let (s, v, _) = appel(&b, "PUT", "/theme/actif", 4, Some(json!({"actif": false}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["actif"], false);
    assert_eq!(v["theme"]["name"], "Nuit");
    let (_, v, _) = appel(&b, "GET", "/theme", 4, None).await;
    assert_eq!(v["actif"], false);

    let (s, v, _) = appel(&b, "DELETE", "/theme", 4, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["theme"], Value::Null);
    assert!(
        SettingsRepo::with_backend(b.clone())
            .get(&cle(4))
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn un_enregistrement_abime_vaut_pas_de_theme() {
    let b = base();
    SettingsRepo::with_backend(b.clone())
        .set(&cle(1), "{pas du json")
        .unwrap();
    let (s, v, _) = appel(&b, "GET", "/theme", 1, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["theme"], Value::Null);
}

#[test]
fn le_greffon_est_au_catalogue_a_installer_et_fournit_un_theme() {
    let p = MonThemePlugin::new(services(&base()));
    assert_eq!(p.name(), "mon-theme");
    assert!(
        !p.default_enabled(),
        "opt-in : on ne repeint pas sans demande"
    );
    assert!(p.catalogued());
    assert_eq!(p.theme_web(), Some("/theme"));
    assert!(p.required_feature().is_none(), "gratuit");
}
