//! #6385 — importer un fichier d'égaliseur venu d'ailleurs, par les ROUTES
//! montées : `POST /api/v1/eq/import/preview` (aperçu, rien d'écrit) et
//! `POST /api/v1/eq/import/autoeq` (enregistrement), sur les fichiers réels de
//! `tune-core/tests/fixtures/` (AutoEq, Equalizer APO, OPRA).
//!
//! ⚠️ `tune-server` porte `autotests = false` — ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

const HD_650: &str = include_str!("../../tune-core/tests/fixtures/autoeq/sennheiser_hd_650.txt");
const APO: &str =
    include_str!("../../tune-core/tests/fixtures/egaliseur_6385/apo_rew_stereo_lg_tv.txt");
const OPRA: &str = include_str!(
    "../../tune-core/tests/fixtures/egaliseur_6385/opra_sennheiser_hd650_oratory1990.json"
);

const APERCU: &str = "/api/v1/eq/import/preview";
const IMPORT: &str = "/api/v1/eq/import/autoeq";

async fn app_premium() -> axum::Router {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .set("plugin_equalizer_installed", "true")
        .unwrap();
    state.license.set_account_premium(true, None).await;
    tune_server::routes::router(state)
}

async fn appel(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn post(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::post(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    appel(app, req).await
}

async fn nombre_de_prereglages(app: &axum::Router) -> usize {
    let (status, liste) = appel(
        app,
        Request::get("/api/v1/eq/presets")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    liste["presets"].as_array().map(Vec::len).unwrap_or(0)
}

fn proche(v: &Value, attendu: f64) -> bool {
    v.as_f64().is_some_and(|x| (x - attendu).abs() < 1e-9)
}

/// L'aperçu rend les bandes, le gain, le Q et le préampli du fichier — et
/// n'écrit AUCUN préréglage. C'est ce que l'écran montre avant « Appliquer ».
#[tokio::test]
async fn lapercu_rend_les_bandes_dautoeq_sans_rien_enregistrer() {
    let app = app_premium().await;
    let avant = nombre_de_prereglages(&app).await;

    let (status, corps) = post(&app, APERCU, json!({ "text": HD_650, "name": "HD 650" })).await;
    assert_eq!(status, StatusCode::OK, "corps : {corps}");
    assert_eq!(corps["format"], "parametric_eq");
    assert_eq!(corps["band_count"], 10);
    assert_eq!(corps["max_bands"], 31);
    assert!(proche(&corps["preamp_db"], -6.1), "corps : {corps}");
    let premiere = &corps["bands"][0];
    assert_eq!(premiere["type"], "low_shelf");
    assert!(
        proche(&premiere["freq"], 105.0)
            && proche(&premiere["gain"], 6.4)
            && proche(&premiere["q"], 0.7)
    );
    assert!(
        corps.get("preset").is_none(),
        "un aperçu ne crée pas de préréglage : {corps}"
    );

    assert_eq!(
        nombre_de_prereglages(&app).await,
        avant,
        "l'aperçu a écrit un préréglage"
    );
}

/// Le profil OPRA réel : aperçu fidèle, puis import qui crée le préréglage.
#[tokio::test]
async fn un_profil_opra_reel_se_previsualise_puis_simporte() {
    let app = app_premium().await;
    let (status, apercu) = post(&app, APERCU, json!({ "text": OPRA })).await;
    assert_eq!(status, StatusCode::OK, "corps : {apercu}");
    assert_eq!(apercu["format"], "opra");
    assert_eq!(apercu["band_count"], 10);
    assert!(proche(&apercu["preamp_db"], -9.3));
    assert_eq!(apercu["bands"][0]["type"], "peak");
    assert!(
        proche(&apercu["bands"][0]["freq"], 20.0)
            && proche(&apercu["bands"][0]["gain"], 4.0)
            && proche(&apercu["bands"][0]["q"], 1.1)
    );

    let (status, cree) = post(
        &app,
        IMPORT,
        json!({ "text": OPRA, "name": "HD650 (OPRA)" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "corps : {cree}");
    assert_eq!(cree["preset"]["source"], "opra");
    assert_eq!(
        cree["preset"]["bands"], apercu["bands"],
        "l'aperçu montre ce que l'import enregistre"
    );
}

/// Une ligne invalide d'un vrai profil AutoEq rend une erreur NOMMÉE (`code`,
/// `line`), sur l'aperçu comme sur l'import.
#[tokio::test]
async fn une_ligne_invalide_rend_une_erreur_nommee_sur_les_deux_routes() {
    let app = app_premium().await;
    let abime = HD_650.replace("Filter 3: ON PK Fc 118 Hz", "Filter 3: ON PK Fc cent Hz");
    for chemin in [APERCU, IMPORT] {
        let (status, corps) = post(&app, chemin, json!({ "text": abime })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{chemin} : {corps}");
        assert_eq!(corps["code"], "invalid_number", "{chemin} : {corps}");
        assert_eq!(corps["line"], 4, "{chemin} : {corps}");
        assert!(
            corps["error"].as_str().unwrap().contains("ligne 4"),
            "{chemin} : {corps}"
        );
    }
    assert_eq!(nombre_de_prereglages(&app).await, 0);
}

/// Le fichier Equalizer APO réel tel que publié : sa ligne `Device:` est
/// refusée en nommant la ligne 1. Sans elle, ses 35 bandes dépassent les 31
/// de l'égaliseur : refus chiffré, jamais de troncature.
#[tokio::test]
async fn le_fichier_apo_reel_est_refuse_nommement_et_sans_troncature() {
    let app = app_premium().await;
    let (status, corps) = post(&app, APERCU, json!({ "text": APO })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "corps : {corps}");
    assert_eq!(corps["code"], "unrecognized_line");
    assert_eq!(corps["line"], 1);

    let sans_device = APO.split_once('\n').unwrap().1;
    for chemin in [APERCU, IMPORT] {
        let (status, corps) = post(&app, chemin, json!({ "text": sans_device })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{chemin} : {corps}");
        assert_eq!(corps["code"], "too_many_bands", "{chemin} : {corps}");
        assert_eq!(corps["band_count"], 35);
        assert_eq!(corps["max_bands"], 31);
    }
    assert_eq!(nombre_de_prereglages(&app).await, 0);
}
