//! rc14 (Rémi) — « Enregistrer » écrase le préréglage d'origine, « Recharger »
//! y revient, et un préréglage livré (« Pure ») ne s'écrase pas.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! par sa cible `[[test]]` dans `tune-server/Cargo.toml`.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::tess::api::conforme_au_schema;

/// Le mode d'application et sa file sont communs au processus : un banc à la fois.
fn serie() -> std::sync::Arc<tokio::sync::Mutex<()>> {
    static F: std::sync::OnceLock<std::sync::Arc<tokio::sync::Mutex<()>>> =
        std::sync::OnceLock::new();
    F.get_or_init(Default::default).clone()
}

struct Banc {
    app: axum::Router,
    zone: i64,
    autre: i64,
    _serie: tokio::sync::OwnedMutexGuard<()>,
}

impl Banc {
    async fn nouveau() -> Self {
        let _serie = serie().lock_owned().await;
        let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        let settings = SettingsRepo::with_backend(state.backend.clone());
        settings
            .set(tune_core::audio::premium_plugins::MIGRATION, "complete")
            .unwrap();
        for id in ["equalizer", "crossfeed"] {
            settings
                .set(&format!("plugin_{id}_installed"), "true")
                .unwrap();
            settings
                .set(&format!("plugin_{id}_enabled"), "true")
                .unwrap();
        }
        state.license.set_account_premium(true, None).await;
        let repo = ZoneRepo::with_backend(state.backend.clone());
        let zone = repo
            .create("Salon", Some("local"), Some("local:dac-ecraser"))
            .unwrap();
        let autre = repo
            .create("Bureau", Some("local"), Some("local:dac-ecraser-2"))
            .unwrap();
        let app = tune_server::routes::router(state.clone());
        Self {
            app,
            zone,
            autre,
            _serie,
        }
    }

    async fn brut(&self, m: Method, uri: &str, corps: Option<Value>) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(m)
            .uri(uri)
            .header("Content-Type", "application/json")
            .body(corps.map_or(Body::empty(), |c| Body::from(c.to_string())))
            .unwrap();
        let r = self.app.clone().oneshot(req).await.unwrap();
        let s = r.status();
        let b = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        (s, serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    async fn put_chaine(&self, zone: i64, corps: Value) {
        let (s, v) = self
            .brut(Method::PUT, &format!("/api/v1/zones/{zone}/tess"), Some(corps))
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }

    async fn zone(&self, zone: i64) -> Value {
        let (s, v) = self
            .brut(Method::GET, &format!("/api/v1/zones/{zone}/tess"), None)
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        conforme_au_schema("zone_response", &v).unwrap_or_else(|e| panic!("{e} : {v}"));
        v
    }

    async fn ecraser(&self, id: &str, corps: Value) -> (StatusCode, Value) {
        self.brut(Method::PUT, &format!("/api/v1/tess/presets/{id}"), Some(corps))
            .await
    }

    async fn preset(&self, id: &str) -> Value {
        let (_, l) = self.brut(Method::GET, "/api/v1/tess/presets", None).await;
        l["presets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == json!(id))
            .cloned()
            .unwrap_or(Value::Null)
    }
}

fn crossfeed(amount: f64) -> Value {
    json!({"version": 1, "plugins": [
        {"id": "crossfeed", "enabled": true, "settings": {"amount": amount, "delay_ms": 0.3}}
    ]})
}

fn amount(chaine: &Value) -> f64 {
    chaine["plugins"][0]["settings"]["amount"].as_f64().unwrap()
}

#[tokio::test]
async fn retoucher_marque_modifie_puis_enregistrer_ecrase_sans_nouvelle_ligne_rc14() {
    let b = Banc::nouveau().await;
    b.put_chaine(b.zone, crossfeed(0.4)).await;
    let (s, p) = b
        .brut(
            Method::POST,
            "/api/v1/tess/presets",
            Some(json!({"name": "Casque", "zone_id": b.zone})),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{p}");
    let z = b.zone(b.zone).await;
    assert_eq!(z["active_preset"], json!("casque"));
    assert_eq!(z["base_preset"], json!("casque"));
    assert!(z.get("preset_modified").is_none(), "non modifié : champ absent");

    // Une retouche : plus « ACTIVE », mais l'origine reste, « modifié ».
    b.put_chaine(b.zone, crossfeed(0.7)).await;
    let z = b.zone(b.zone).await;
    assert_eq!(z["active_preset"], Value::Null);
    assert_eq!(z["base_preset"], json!("casque"));
    assert_eq!(z["preset_modified"], json!(true));

    // « Enregistrer » : la même ligne, la nouvelle chaîne.
    let (s, v) = b.ecraser("casque", json!({"zone_id": b.zone})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    conforme_au_schema("preset", &v).unwrap();
    assert_eq!(v["id"], json!("casque"));
    assert_eq!(v["name"], json!("Casque"));
    assert_eq!(amount(&v["chain"]), 0.7);
    let (_, l) = b.brut(Method::GET, "/api/v1/tess/presets", None).await;
    assert_eq!(l["presets"].as_array().unwrap().len(), 2, "pure + casque, sans doublon");
    let z = b.zone(b.zone).await;
    assert_eq!(z["active_preset"], json!("casque"));
    assert!(z.get("preset_modified").is_none());
}

#[tokio::test]
async fn recharger_revient_au_prereglage_tel_qu_enregistre_rc14() {
    let b = Banc::nouveau().await;
    b.put_chaine(b.zone, crossfeed(0.4)).await;
    b.brut(
        Method::POST,
        "/api/v1/tess/presets",
        Some(json!({"name": "Casque", "zone_id": b.zone})),
    )
    .await;
    b.put_chaine(b.zone, crossfeed(0.9)).await;
    assert_eq!(b.zone(b.zone).await["preset_modified"], json!(true));
    // « Recharger » = charger l'origine, directement (sans passer par un autre).
    let (s, v) = b
        .brut(
            Method::POST,
            &format!("/api/v1/zones/{}/tess/presets/casque/apply", b.zone),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let z = b.zone(b.zone).await;
    assert_eq!(amount(&z["chain"]), 0.4);
    assert_eq!(z["active_preset"], json!("casque"));
    assert!(z.get("preset_modified").is_none());
}

#[tokio::test]
async fn un_prereglage_livre_ne_s_ecrase_pas_rc14() {
    let b = Banc::nouveau().await;
    let avant = b.preset("pure").await;
    // Chargé, puis retouché : l'origine est « Pure », modifiée.
    let (s, v) = b
        .brut(
            Method::POST,
            &format!("/api/v1/zones/{}/tess/presets/pure/apply", b.zone),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    b.put_chaine(b.zone, crossfeed(0.5)).await;
    let z = b.zone(b.zone).await;
    assert_eq!(z["base_preset"], json!("pure"));
    assert_eq!(z["preset_modified"], json!(true));
    for corps in [json!({"zone_id": b.zone}), json!({"chain": crossfeed(0.5)})] {
        let (s, v) = b.ecraser("pure", corps.clone()).await;
        assert_eq!(s, StatusCode::CONFLICT, "{corps} → {v}");
        assert_eq!(v["error"], json!("builtin_preset_read_only"));
        assert_eq!(v["preset_id"], json!("pure"));
    }
    assert_eq!(b.preset("pure").await, avant, "Pure est intact");
    // Contre-épreuve : « Enregistrer sous… » (POST) passe.
    let (s, v) = b
        .brut(
            Method::POST,
            "/api/v1/tess/presets",
            Some(json!({"name": "Pure retouché", "zone_id": b.zone})),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_ne!(v["id"], json!("pure"));
}

#[tokio::test]
async fn les_refus_de_l_ecrasement_et_les_autres_zones_rc14() {
    let b = Banc::nouveau().await;
    b.put_chaine(b.zone, crossfeed(0.4)).await;
    b.brut(
        Method::POST,
        "/api/v1/tess/presets",
        Some(json!({"name": "Casque", "zone_id": b.zone})),
    )
    .await;
    // L'autre zone charge le même préréglage.
    let (s, _) = b
        .brut(
            Method::POST,
            &format!("/api/v1/zones/{}/tess/presets/casque/apply", b.autre),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    for (id, corps, statut, code) in [
        ("inconnu", json!({"zone_id": b.zone}), 404, "preset_not_found"),
        ("casque", json!({}), 400, "invalid_body"),
        (
            "casque",
            json!({"zone_id": b.zone, "chain": crossfeed(0.1)}),
            400,
            "invalid_body",
        ),
        ("casque", json!({"zone_id": 9999}), 404, "zone_not_found"),
    ] {
        let (s, v) = b.ecraser(id, corps.clone()).await;
        assert_eq!(s.as_u16(), statut, "{corps} → {v}");
        assert_eq!(v["error"], json!(code), "{corps} → {v}");
    }
    assert_eq!(amount(&b.preset("casque").await["chain"]), 0.4);
    // Chaîne fournie : écrase ; l'autre zone ne le décrit plus.
    let (s, v) = b.ecraser("casque", json!({"chain": crossfeed(0.2)})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(amount(&b.preset("casque").await["chain"]), 0.2);
    let autre = b.zone(b.autre).await;
    assert_eq!(autre["active_preset"], Value::Null);
    assert_eq!(autre["base_preset"], json!("casque"));
    assert_eq!(autre["preset_modified"], json!(true));
    // Supprimé : plus d'origine.
    let (s, _) = b
        .brut(Method::DELETE, "/api/v1/tess/presets/casque", None)
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let z = b.zone(b.zone).await;
    assert!(z.get("base_preset").is_none());
    assert!(z.get("preset_modified").is_none());
}
