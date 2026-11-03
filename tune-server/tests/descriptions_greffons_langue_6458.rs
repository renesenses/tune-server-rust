//! #6458 — l'écran Studio › Extensions affichait les descriptions des greffons
//! livrés en FRANÇAIS dans une interface réglée en anglais (Levente, fil 2306).
//! Le serveur rendait `TunePlugin::description()` tel quel, sans aucune notion
//! de langue ; même chose pour le refus 409 `plugin_unavailable`
//! (« Ce plugin doit être installé et activé. »).
//!
//! Ces témoins passent par les ROUTES montées, avec l'en-tête
//! `Accept-Language` que le client web pose sur chaque requête.

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;

use crate::use_scratch_plugin_data_dir;
use tune_core::db::settings_repo::SettingsRepo;
use tune_server::state::AppState;

/// Le texte français affiché AVANT le correctif : il ne doit pas bouger.
const EGALISEUR_FR: &str = "Égaliseur : profil, graphique, paramétrique, presets et AutoEq";
const EGALISEUR_EN: &str = "Equalizer: profile, graphic, parametric, presets and AutoEq";

async fn etat_avec_egaliseur(installe: bool) -> (AppState, axum::Router) {
    use_scratch_plugin_data_dir();
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    if installe {
        SettingsRepo::with_backend(state.backend.clone())
            .set("plugin_equalizer_installed", "true")
            .unwrap();
    }
    tune_server::plugins::init(&state, "http://127.0.0.1:0", vec![]).await;
    let app = tune_server::routes::router(state.clone());
    (state, app)
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    chemin: &str,
    langue: Option<&str>,
    corps: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(methode).uri(chemin);
    if let Some(l) = langue {
        req = req.header(header::ACCEPT_LANGUAGE, l);
    }
    if corps.is_some() {
        req = req.header(header::CONTENT_TYPE, "application/json");
    }
    let resp = app
        .clone()
        .oneshot(
            req.body(Body::from(corps.unwrap_or("").to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn fiche<'a>(liste: &'a Value, nom: &str) -> &'a Value {
    liste
        .as_array()
        .expect("GET /plugins rend un tableau")
        .iter()
        .find(|f| f["name"] == nom)
        .unwrap_or_else(|| panic!("pas de fiche «{nom}» dans {liste}"))
}

/// La description de l'égaliseur suit la langue de la requête : anglais en
/// anglais, et le français d'avant reste mot pour mot.
///
/// Contre-épreuve : retirer l'appel à `localiser_la_description` dans
/// `list_plugins` fait rougir ce test (« en » reçoit le texte français).
#[tokio::test]
async fn la_description_d_un_greffon_suit_la_langue_6458() {
    let (_state, app) = etat_avec_egaliseur(true).await;
    for (entete, attendu) in [
        (Some("en-GB,en;q=0.9"), EGALISEUR_EN),
        (Some("fr-FR,fr;q=0.9"), EGALISEUR_FR),
        (
            Some("de-DE"),
            "Equalizer: Profil, grafisch, parametrisch, Presets und AutoEq",
        ),
        (
            Some("ja"),
            "イコライザー：プロファイル、グラフィック、パラメトリック、プリセット、AutoEq",
        ),
        // Une langue que l'interface ne parle pas, ou pas d'en-tête : anglais.
        (Some("pt-BR,pt;q=0.9"), EGALISEUR_EN),
        (None, EGALISEUR_EN),
    ] {
        let (status, liste) = appel(&app, "GET", "/api/v1/plugins", entete, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            fiche(&liste, "equalizer")["description"],
            attendu,
            "GET /plugins, Accept-Language {entete:?}"
        );
        let (_, unitaire) = appel(&app, "GET", "/api/v1/plugins/equalizer", entete, None).await;
        assert_eq!(
            unitaire["description"], attendu,
            "GET /plugins/equalizer, Accept-Language {entete:?}"
        );
    }
}

/// Chaque greffon livré que la liste montre a SA description traduite : une
/// clé `plugin.description.<nom>` dans la table, différente en anglais et en
/// français. Un greffon ajouté sans clé retomberait en silence sur sa
/// description déclarée — le défaut de #6458, une ligne plus bas.
///
/// Contre-épreuve : retirer la clé `plugin.description.cd` de
/// `i18n_server.json` fait rougir ce test sur «cd».
#[tokio::test]
async fn chaque_greffon_livre_a_sa_description_traduite_6458() {
    let (_state, app) = etat_avec_egaliseur(true).await;
    let (_, en) = appel(&app, "GET", "/api/v1/plugins", Some("en"), None).await;
    let (_, fr) = appel(&app, "GET", "/api/v1/plugins", Some("fr"), None).await;
    let mut vus = 0;
    for f in en.as_array().unwrap() {
        if f["type"] == "wasm" {
            continue;
        }
        let nom = f["name"].as_str().unwrap();
        let cle = format!("plugin.description.{nom}");
        assert!(
            tune_server::i18n::t_opt("en", &cle).is_some(),
            "le greffon livré «{nom}» n'a pas de clé «{cle}»"
        );
        assert_ne!(
            f["description"],
            fiche(&fr, nom)["description"],
            "«{nom}» a la même description en anglais et en français"
        );
        vus += 1;
    }
    assert!(vus >= 2, "la liste doit montrer des greffons livrés : {en}");
}

/// Le refus 409 `plugin_unavailable` parle la langue de la requête, et le
/// français d'avant reste mot pour mot.
///
/// Contre-épreuve : remettre le texte français en dur dans
/// `require_installed` fait rougir ce test sur «en».
#[tokio::test]
async fn le_refus_plugin_unavailable_suit_la_langue_6458() {
    let (_state, app) = etat_avec_egaliseur(false).await;
    let corps = r#"{"eq_profile":{"enabled":true,"bands":[]}}"#;
    for (entete, attendu) in [
        ("fr", "Ce plugin doit être installé et activé."),
        ("en-US", "This plugin must be installed and enabled."),
        ("ko", "이 플러그인을 설치하고 활성화해야 합니다."),
        ("pt-BR", "This plugin must be installed and enabled."),
    ] {
        let (status, refus) = appel(
            &app,
            "PUT",
            "/api/v1/zones/1/dsp",
            Some(entete),
            Some(corps),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "Accept-Language {entete} : {refus}"
        );
        assert_eq!(refus["error"], "plugin_unavailable");
        assert_eq!(refus["message"], attendu, "Accept-Language {entete}");
    }
}
