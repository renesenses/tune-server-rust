//! TESS T08 — `GET/PUT /zones/{id}/tess` et `GET /tess/catalog`, par le
//! routeur public (authentification comprise).
//!
//! Chaque refus a sa contre-épreuve : la même requête, corrigée du seul
//! défaut, passe.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! par sa cible `[[test]]` dans `tune-server/Cargo.toml`.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::tess::api::conforme_au_schema;
use tune_core::tess::migration::lire_zone;
use tune_core::tess::{ChaineTess, cle_chaine_zone};

const SECRET: &str = "test-jwt-secret";

struct Banc {
    state: tune_server::state::AppState,
    app: axum::Router,
    zone: i64,
}

impl Banc {
    async fn nouveau(premium: bool) -> Self {
        Self::avec_sortie(premium, "local", "local:Realtek HD").await
    }

    async fn avec_sortie(premium: bool, output_type: &str, device: &str) -> Self {
        let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        let settings = SettingsRepo::with_backend(state.backend.clone());
        settings
            .set(tune_core::audio::premium_plugins::MIGRATION, "complete")
            .unwrap();
        for id in ["equalizer", "crossfeed", "channel-remap", "upsampler"] {
            settings
                .set(&format!("plugin_{id}_installed"), "true")
                .unwrap();
            settings
                .set(&format!("plugin_{id}_enabled"), "true")
                .unwrap();
        }
        state.license.set_account_premium(premium, None).await;
        let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
            .create("Zone", Some(output_type), Some(device))
            .unwrap();
        // Volume plein : une chaîne vide est alors bit-perfect.
        tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
            .update_volume(zone, 100.0)
            .unwrap();
        let app = tune_server::routes::router(state.clone());
        Self { state, app, zone }
    }

    fn settings(&self) -> SettingsRepo {
        SettingsRepo::with_backend(self.state.backend.clone())
    }

    async fn premium(&self, oui: bool) {
        self.state.license.set_account_premium(oui, None).await;
    }

    async fn brut(
        &self,
        methode: Method,
        uri: &str,
        corps: Option<String>,
        jeton: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder()
            .method(methode)
            .uri(uri)
            .header("Content-Type", "application/json");
        if let Some(j) = jeton {
            req = req.header(header::AUTHORIZATION, format!("Bearer {j}"));
        }
        let resp = self
            .app
            .clone()
            .oneshot(req.body(corps.map_or(Body::empty(), Body::from)).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn get(&self) -> (StatusCode, Value) {
        let uri = format!("/api/v1/zones/{}/tess", self.zone);
        self.brut(Method::GET, &uri, None, None).await
    }

    async fn put_texte(&self, corps: &str) -> (StatusCode, Value) {
        let uri = format!("/api/v1/zones/{}/tess", self.zone);
        self.brut(Method::PUT, &uri, Some(corps.to_owned()), None)
            .await
    }

    async fn put(&self, corps: Value) -> (StatusCode, Value) {
        self.put_texte(&corps.to_string()).await
    }

    fn enregistree(&self) -> Option<String> {
        self.settings().get(&cle_chaine_zone(self.zone)).unwrap()
    }

    fn chaine(&self) -> ChaineTess {
        lire_zone(&self.settings(), self.zone, true).unwrap().chaine
    }
}

/// Un profil d'égaliseur complet (les champs que le schéma de la caisse exige).
fn profil(bandes: Value) -> Value {
    json!({
        "listening": "speakers", "room_size": "medium",
        "speaker_placement": "free_standing", "bass_gain_db": 0.0,
        "mid_gain_db": 0.0, "treble_gain_db": 0.0, "bands": bandes
    })
}

fn bandes_parametriques() -> Value {
    json!([
        {"freq": 80.0, "gain": 4.0, "q": 0.7, "type": "low_shelf"},
        {"freq": 1200.0, "gain": -3.5, "q": 1.4, "type": "peak"}
    ])
}

fn refus_conforme(v: &Value, statut: u16, code: &str) {
    conforme_au_schema("refusal", v).unwrap_or_else(|e| panic!("{v} : {e}"));
    assert_eq!(v["status"], json!(statut), "{v}");
    assert_eq!(v["code"], json!(code), "{v}");
    // Même forme que les autres refus de l'API (`premium_guard`).
    assert_eq!(v["error"], json!(code), "{v}");
    assert_eq!(v["message"], v["detail"], "{v}");
    assert!(!v["message"].as_str().unwrap_or_default().is_empty(), "{v}");
    assert_eq!(v["upgrade_url"].is_string(), statut == 402, "{v}");
}

fn etape<'a>(reponse: &'a Value, plugin: &str) -> &'a Value {
    reponse["signal_path"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["plugin_id"] == json!(plugin))
        .unwrap_or_else(|| panic!("pas d'étape {plugin} : {reponse}"))
}

// ── GET ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn get_rend_la_zone_conforme_au_schema_et_le_groupe_crossfeed_en_refus_t08() {
    let b = Banc::nouveau(true).await;
    let (s, v) = b.get().await;
    assert_eq!(s, StatusCode::OK, "{v}");
    conforme_au_schema("zone_response", &v).unwrap_or_else(|e| panic!("{e} : {v}"));
    assert_eq!(v["zone_id"], json!(b.zone));
    assert_eq!(v["transport"], json!("local"));
    assert_eq!(v["applicability"]["status"], json!("applied"));
    assert_eq!(
        v["badge"],
        json!("bit_perfect"),
        "chaîne vide, volume plein : {v}"
    );
    // Contre-épreuve : volume à 50 %, le volume logiciel traite.
    tune_core::db::zone_repo::ZoneRepo::with_backend(b.state.backend.clone())
        .update_volume(b.zone, 50.0)
        .unwrap();
    let (_, v) = b.get().await;
    assert_eq!(v["badge"], json!("processed_by_tess"), "{v}");
    assert_eq!(v["volume"]["control"], json!("software"), "{v}");
    assert!(v["volume"]["db"].as_f64().unwrap() < -1.0, "{v}");
    assert_eq!(
        v["catalog"]["groups"][0],
        json!({"id": "crossfeed_crosstalk", "policy": "refuse"})
    );
    // Un GET n'écrit rien (la migration est l'affaire de T03).
    assert!(b.enregistree().is_none());

    // Zone inconnue : 404 qui la nomme (distinct d'un serveur sans TESS).
    let (s, v) = b
        .brut(Method::GET, "/api/v1/zones/9999/tess", None, None)
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(v["error"], json!("zone_not_found"));
    let (s, _) = b
        .brut(
            Method::PUT,
            "/api/v1/zones/9999/tess",
            Some("{\"version\":1}".into()),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_montre_l_etat_effectif_demande_et_motif_t08() {
    let b = Banc::nouveau(true).await;
    // Posée par une ANCIENNE route : la chaîne TESS la voit.
    let (s, v) = b
        .brut(
            Method::PUT,
            &format!("/api/v1/zones/{}/dsp", b.zone),
            Some(
                json!({"crossfeed": {"enabled": true, "amount": 0.35, "delay_ms": 0.3}})
                    .to_string(),
            ),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, v) = b.get().await;
    let cf = etape(&v, "crossfeed");
    assert_eq!(
        (
            cf["requested"].clone(),
            cf["effective"].clone(),
            cf["reason"].clone()
        ),
        (json!(true), json!(true), Value::Null),
        "{cf}"
    );
    assert_eq!(v["badge"], json!("processed_by_tess"));

    // Greffon désinstallé : demandé, pas effectif, motif.
    b.settings()
        .set("plugin_crossfeed_enabled", "false")
        .unwrap();
    let (_, v) = b.get().await;
    let cf = etape(&v, "crossfeed");
    assert_eq!(cf["requested"], json!(true));
    assert_eq!(cf["effective"], json!(false));
    assert_eq!(cf["reason"], json!("plugin_unavailable"));
    conforme_au_schema("zone_response", &v).unwrap();
}

#[tokio::test]
async fn hqplayer_et_diretta_rendent_leur_motif_t08() {
    for (sortie, motif) in [
        ("hqplayer", "hqplayer_own_processing"),
        ("diretta", "diretta_outside_tess"),
    ] {
        let b = Banc::avec_sortie(true, sortie, &format!("{sortie}:x")).await;
        let (s, v) = b
            .put(json!({"version": 1, "plugins": [{"id": "crossfeed", "enabled": true}]}))
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["applicability"]["status"], json!("outside_tess"), "{v}");
        assert_eq!(v["applicability"]["reason"], json!(motif));
        assert!(!v["applicability"]["detail"].as_str().unwrap().is_empty());
        assert_eq!(v["badge"], json!("outside_tess"));
        let cf = etape(&v, "crossfeed");
        assert_eq!(cf["effective"], json!(false));
        assert_eq!(cf["reason"], json!("outside_tess"));
        conforme_au_schema("zone_response", &v).unwrap();
    }
    // Contre-épreuve : la même chaîne sur une zone OAAT est appliquée.
    let b = Banc::avec_sortie(true, "oaat", "oaat:x").await;
    let (_, v) = b
        .put(json!({"version": 1, "plugins": [{"id": "crossfeed", "enabled": true}]}))
        .await;
    assert_eq!(v["applicability"]["status"], json!("applied"), "{v}");
    assert_ne!(etape(&v, "crossfeed")["reason"], json!("outside_tess"));
}

// ── Catalogue ───────────────────────────────────────────────────────

#[tokio::test]
async fn le_catalogue_porte_les_schemas_du_sdk_et_les_droits_du_compte_t08() {
    let b = Banc::nouveau(false).await;
    let (s, v) = b
        .brut(Method::GET, "/api/v1/tess/catalog", None, None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    conforme_au_schema("catalog_response", &v).unwrap_or_else(|e| panic!("{e} : {v}"));
    assert_eq!(v["premium"], json!(false));
    let desc = |id: &str| {
        v["catalog"]["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == json!(id))
            .unwrap()
            .clone()
    };
    let etat = |id: &str| {
        v["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == json!(id))
            .unwrap()
            .clone()
    };
    // Schémas des caisses du SDK, sans `enabled` (porté par le maillon).
    let eq = desc("equalizer-parametric")["settings_schema"].clone();
    assert!(eq["properties"]["bands"].is_object(), "{eq}");
    assert!(eq["properties"].get("enabled").is_none(), "{eq}");
    assert!(
        !eq["required"]
            .as_array()
            .unwrap()
            .contains(&json!("enabled"))
    );
    assert!(desc("crossfeed")["settings_schema"]["properties"]["amount"].is_object());
    assert!(desc("channel-remap")["settings_schema"]["properties"]["gains_db"].is_object());
    // Droits du compte gratuit.
    assert_eq!(etat("crossfeed")["entitled"], json!(true));
    assert_eq!(etat("crossfeed")["reason"], Value::Null);
    assert_eq!(etat("convolution")["entitled"], json!(false));
    assert_eq!(etat("convolution")["reason"], json!("premium_required"));
    assert_eq!(
        etat("equalizer-parametric")["reason"],
        json!("premium_required")
    );
    assert_eq!(etat("declick-live")["reason"], json!("plugin_unavailable"));
    // Contre-épreuve : Premium, le même catalogue ouvre la convolution.
    b.premium(true).await;
    let (_, v) = b
        .brut(Method::GET, "/api/v1/tess/catalog", None, None)
        .await;
    let conv = v["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == json!("convolution"))
        .unwrap()
        .clone();
    assert_eq!(conv["entitled"], json!(true));
    assert_eq!(conv["reason"], Value::Null);
}

// ── PUT : écriture et écriture croisée ──────────────────────────────

#[tokio::test]
async fn put_ecrit_la_chaine_et_les_anciennes_routes_la_voient_t08() {
    let b = Banc::nouveau(true).await;
    let (s, v) = b
        .put(json!({
            "version": 1,
            "plugins": [
                {"id": "equalizer-parametric", "enabled": true, "settings": profil(bandes_parametriques())},
                {"id": "crossfeed", "enabled": true, "settings": {"amount": 0.4, "delay_ms": 0.3}}
            ]
        }))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    conforme_au_schema("zone_response", &v).unwrap();
    assert_eq!(
        v["chain"]["plugins"][0]["id"],
        json!("equalizer-parametric")
    );
    assert!(b.enregistree().is_some());
    // L'ancienne route lit ce que TESS a écrit (clients mobiles).
    let (_, dsp) = b
        .brut(
            Method::GET,
            &format!("/api/v1/zones/{}/dsp", b.zone),
            None,
            None,
        )
        .await;
    assert_eq!(dsp["crossfeed"]["enabled"], json!(true), "{dsp}");
    assert_eq!(dsp["eq_profile"]["enabled"], json!(true), "{dsp}");
    assert_eq!(dsp["eq_profile"]["bands"].as_array().unwrap().len(), 2);

    // Aller-retour : le GET renvoyé tel quel ne change rien.
    let avant = b.enregistree();
    let (s, _) = b.put(v["chain"].clone()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b.enregistree(), avant);

    // Décocher le crossfeed se voit dans l'ancienne route.
    let mut c = v["chain"].clone();
    c["plugins"][1]["enabled"] = json!(false);
    let (s, _) = b.put(c).await;
    assert_eq!(s, StatusCode::OK);
    let (_, dsp) = b
        .brut(
            Method::GET,
            &format!("/api/v1/zones/{}/dsp", b.zone),
            None,
            None,
        )
        .await;
    assert_eq!(dsp["crossfeed"]["enabled"], json!(false), "{dsp}");
}

#[tokio::test]
async fn un_champ_absent_garde_sa_valeur_enregistree_t08() {
    let b = Banc::nouveau(true).await;
    let (s, _) = b
        .put(json!({
            "version": 1,
            "plugins": [{"id": "crossfeed", "enabled": true}],
            "fixed": {"headroom": {"mode": "manual", "db": -6.0}, "quantization": {"dither": "noise_shaped"}}
        }))
        .await;
    assert_eq!(s, StatusCode::OK);
    // Un client qui ne renvoie que ce qu'il connaît ne perd rien.
    let (s, v) = b
        .put(
            json!({"version": 1, "bypass_all": true, "fixed": {"quantization": {"dither": "off"}}}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let c = b.chaine();
    assert!(c.bypass_all);
    assert_eq!(c.plugins.len(), 1, "{:?}", c.plugins);
    assert_eq!(
        serde_json::to_value(&c.fixed).unwrap()["headroom"],
        json!({"mode": "manual", "db": -6.0})
    );
    assert_eq!(
        serde_json::to_value(&c.fixed).unwrap()["quantization"],
        json!({"dither": "off"})
    );
    // Contre-épreuve : une liste vide, envoyée, vide la chaîne.
    let (s, _) = b.put(json!({"version": 1, "plugins": []})).await;
    assert_eq!(s, StatusCode::OK);
    assert!(b.chaine().plugins.is_empty());
}

// ── PUT : refus ─────────────────────────────────────────────────────

#[tokio::test]
async fn corps_illisible_400_invalid_body_sans_rien_ecrire_t08() {
    let b = Banc::nouveau(true).await;
    let (s, _) = b
        .put(json!({"version": 1, "plugins": [{"id": "crossfeed", "enabled": true}]}))
        .await;
    assert_eq!(s, StatusCode::OK);
    let avant = b.enregistree();
    for corps in [
        "{".to_owned(),
        "[]".to_owned(),
        json!({"plugins": []}).to_string(),
        json!({"version": 1, "volume": -18}).to_string(),
        json!({"version": 1, "plugins": [{"id": "crossfeed", "enabled": true, "x": 1}]})
            .to_string(),
        json!({"version": 1, "fixed": {"quantization": {"dither": "magic"}}}).to_string(),
        json!({"version": 1, "fixed": {"upsampling": {"mode": "oversample_x3"}}}).to_string(),
    ] {
        let (s, v) = b.put_texte(&corps).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{corps} → {v}");
        refus_conforme(&v, 400, "invalid_body");
        assert_eq!(b.enregistree(), avant, "{corps} a écrit");
    }
    // Contre-épreuve : le même corps sans le champ inconnu passe.
    let (s, _) = b
        .put(json!({"version": 1, "plugins": [{"id": "crossfeed", "enabled": false}]}))
        .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn chaine_invalide_409_et_sa_contre_epreuve_t08() {
    let b = Banc::nouveau(true).await;
    let cas = [
        (
            json!({"version": 2}),
            "unsupported_version",
            json!({"version": 1}),
        ),
        (
            json!({"version": 1, "plugins": [{"id": "nope", "enabled": true}]}),
            "unknown_plugin",
            json!({"version": 1, "plugins": [{"id": "mono-downmix", "enabled": true}]}),
        ),
        (
            json!({"version": 1, "plugins": [{"id": "crossfeed", "enabled": true}, {"id": "crossfeed", "enabled": false}]}),
            "duplicate_plugin",
            json!({"version": 1, "plugins": [{"id": "crossfeed", "enabled": true}]}),
        ),
        (
            json!({"version": 1, "plugins": [{"id": "channel-remap", "enabled": false}, {"id": "crossfeed", "enabled": true}]}),
            "must_be_last_plugin",
            json!({"version": 1, "plugins": [{"id": "crossfeed", "enabled": true}, {"id": "channel-remap", "enabled": false}]}),
        ),
        (
            // Le schéma de la caisse exige `listening` & co.
            json!({"version": 1, "plugins": [{"id": "equalizer-graphic", "enabled": true, "settings": {"bands": []}}]}),
            "invalid_settings",
            json!({"version": 1, "plugins": [{"id": "equalizer-graphic", "enabled": true, "settings": profil(json!([]))}]}),
        ),
        (
            json!({"version": 1, "fixed": {"headroom": {"mode": "manual", "db": 3.0}}}),
            "headroom_out_of_range",
            json!({"version": 1, "fixed": {"headroom": {"mode": "manual", "db": -3.0}}}),
        ),
        (
            json!({"version": 1, "fixed": {"upsampling": {"mode": "pcm", "filter": "polyvalent", "max_ratio": 3}}}),
            "invalid_upsampling",
            json!({"version": 1, "fixed": {"upsampling": {"mode": "pcm", "filter": "polyvalent", "max_ratio": 4}}}),
        ),
        (
            // Exclusivité du groupe crossfeed (décision du 18/11).
            json!({"version": 1, "plugins": [{"id": "crossfeed", "enabled": true}, {"id": "crosstalk", "enabled": true}]}),
            "mutually_exclusive",
            json!({"version": 1, "plugins": [{"id": "crossfeed", "enabled": false}, {"id": "crosstalk", "enabled": true}]}),
        ),
    ];
    for (mauvais, code, bon) in cas {
        let avant = b.enregistree();
        let (s, v) = b.put(mauvais.clone()).await;
        assert_eq!(s, StatusCode::CONFLICT, "{mauvais} → {v}");
        refus_conforme(&v, 409, code);
        assert_eq!(b.enregistree(), avant, "{mauvais} a écrit");
        let (s, v) = b.put(bon.clone()).await;
        assert_eq!(s, StatusCode::OK, "contre-épreuve {bon} → {v}");
    }
}

#[tokio::test]
async fn premium_requis_402_et_sa_contre_epreuve_t08() {
    let b = Banc::nouveau(false).await;
    for (corps, greffon) in [
        (
            json!({"version": 1, "plugins": [{"id": "convolution", "enabled": true, "settings": {"ir_path": "/x.wav"}}]}),
            "convolution",
        ),
        (
            json!({"version": 1, "plugins": [{"id": "equalizer-parametric", "enabled": true, "settings": profil(bandes_parametriques())}]}),
            "equalizer-parametric",
        ),
        (
            json!({"version": 1, "fixed": {"upsampling": {"mode": "pcm", "filter": "polyvalent"}}}),
            "upsampling",
        ),
    ] {
        let (s, v) = b.put(corps.clone()).await;
        assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{corps} → {v}");
        refus_conforme(&v, 402, "premium_required");
        assert_eq!(v["plugin_id"], json!(greffon));
        assert!(b.enregistree().is_none());
    }
    // Décoché, ou sous « Bypass all », rien ne demande le droit.
    let (s, v) = b
        .put(json!({"version": 1, "plugins": [{"id": "convolution", "enabled": false}]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // Gratuit : crossfeed simple (Q2 révisée).
    let (s, v) = b
        .put(json!({"version": 1, "plugins": [{"id": "crossfeed", "enabled": true}]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // Contre-épreuve : le même corps Premium passe avec le droit.
    b.premium(true).await;
    let (s, v) = b
        .put(json!({"version": 1, "plugins": [{"id": "convolution", "enabled": true, "settings": {"ir_path": "/x.wav"}}]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

#[tokio::test]
async fn perdre_le_premium_ne_bloque_pas_l_edition_du_reste_t08() {
    let b = Banc::nouveau(true).await;
    let conv = json!({"id": "convolution", "enabled": true, "settings": {"ir_path": "/x.wav"}});
    let (s, _) = b
        .put(
            json!({"version": 1, "plugins": [conv.clone(), {"id": "crossfeed", "enabled": false}]}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    b.premium(false).await;
    // Le maillon Premium inchangé n'est pas revérifié…
    let (s, v) = b
        .put(json!({"version": 1, "plugins": [conv.clone(), {"id": "crossfeed", "enabled": true}]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // … mais il ne lève rien : la résolution l'éteint.
    let c = etape(&v, "convolution");
    assert_eq!(c["effective"], json!(false));
    assert_eq!(c["reason"], json!("premium_required"));
    // Contre-épreuve : le modifier demande le droit.
    let (s, v) = b
        .put(json!({"version": 1, "plugins": [
            {"id": "convolution", "enabled": true, "settings": {"ir_path": "/y.wav"}},
            {"id": "crossfeed", "enabled": true}
        ]}))
        .await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{v}");
    refus_conforme(&v, 402, "premium_required");
}

// ── Droit Premium contrôlé sous `bypass_all` ────────────────────────

/// Sans Premium, « Bypass all » ne lève pas le droit : sinon un premier PUT
/// sous bypass enregistre le maillon, et un second qui décoche le bypass le
/// trouve « inchangé ». Convolution, égaliseur paramétrique, greffon tiers
/// et upsampling.
#[tokio::test]
async fn le_droit_est_controle_sous_bypass_all_en_deux_put_t08() {
    let b = Banc::nouveau(false).await;
    let cas = [
        (
            json!({"plugins": [{"id": "convolution", "enabled": true, "settings": {"ir_path": "/x.wav"}}]}),
            "convolution",
        ),
        (
            json!({"plugins": [{"id": "equalizer-parametric", "enabled": true, "settings": profil(bandes_parametriques())}]}),
            "equalizer-parametric",
        ),
        (
            json!({"fixed": {"upsampling": {"mode": "pcm", "filter": "polyvalent"}}}),
            "upsampling",
        ),
    ];
    for (partie, greffon) in &cas {
        for bypass in [true, false] {
            let mut corps = partie.clone();
            corps["version"] = json!(1);
            corps["bypass_all"] = json!(bypass);
            let (s, v) = b.put(corps.clone()).await;
            assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{corps} → {v}");
            refus_conforme(&v, 402, "premium_required");
            assert_eq!(v["plugin_id"], json!(greffon));
            assert!(b.enregistree().is_none(), "{corps}");
        }
    }
    // Une chaîne enregistrée sous bypass par un compte Premium, Premium
    // perdu : renvoyée inchangée, bypass décoché, elle passe (le droit a été
    // contrôlé à l'écriture) ; la résolution éteint le maillon.
    for (partie, greffon) in &cas {
        b.premium(true).await;
        let mut corps = partie.clone();
        corps["version"] = json!(1);
        corps["bypass_all"] = json!(true);
        let (s, v) = b.put(corps.clone()).await;
        assert_eq!(s, StatusCode::OK, "{corps} → {v}");
        b.premium(false).await;
        corps["bypass_all"] = json!(false);
        let (s, v) = b.put(corps.clone()).await;
        assert_eq!(s, StatusCode::OK, "{corps} → {v}");
        assert!(!b.chaine().bypass_all, "{corps}");
        match *greffon {
            // Sans Premium, la lecture le montre converti en graphique (P2) ;
            // la chaîne enregistrée le garde.
            "equalizer-parametric" => assert!(
                b.chaine()
                    .plugins
                    .iter()
                    .any(|m| m.id == *greffon && m.enabled),
                "{corps}"
            ),
            "upsampling" => {}
            _ => {
                let e = etape(&v, greffon);
                assert_eq!(e["effective"], json!(false), "{v}");
                assert_eq!(e["reason"], json!("premium_required"), "{v}");
            }
        }
        // Remise à zéro pour le cas suivant.
        let (s, _) = b
            .put(json!({"version": 1, "bypass_all": false, "plugins": [],
                "fixed": {"upsampling": {"mode": "off"}}}))
            .await;
        assert_eq!(s, StatusCode::OK);
    }
    // Contre-épreuve : hors bypass, le même maillon inchangé reste exempté
    // (perdre le Premium ne bloque pas l'édition du reste).
    b.premium(true).await;
    let conv = json!({"id": "convolution", "enabled": true, "settings": {"ir_path": "/x.wav"}});
    let (s, _) = b
        .put(json!({"version": 1, "bypass_all": false, "plugins": [conv.clone()]}))
        .await;
    assert_eq!(s, StatusCode::OK);
    b.premium(false).await;
    let (s, v) = b
        .put(json!({"version": 1, "plugins": [conv.clone(), {"id": "crossfeed", "enabled": true}]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // Et le compte Premium passe partout, bypass compris.
    b.premium(true).await;
    let (s, v) = b
        .put(json!({"version": 1, "bypass_all": true, "plugins": [conv]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

/// Ancien compte Premium : chaîne sous bypass avec deux maillons Premium
/// cochés et l'upsampling, puis Premium perdu. Chaque geste de l'écran
/// renvoie la chaîne entière : aucun ne prend de 402.
#[tokio::test]
async fn premium_perdu_sous_bypass_ne_bloque_aucun_geste_t08() {
    let b = Banc::nouveau(true).await;
    let conv = json!({"id": "convolution", "enabled": true, "settings": {"ir_path": "/x.wav"}});
    let eq = json!({"id": "equalizer-parametric", "enabled": true, "settings": profil(bandes_parametriques())});
    let up = json!({"mode": "pcm", "filter": "polyvalent"});
    let (s, v) = b
        .put(
            json!({"version": 1, "bypass_all": true, "plugins": [conv.clone(), eq.clone()],
            "fixed": {"upsampling": up.clone()}}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    b.premium(false).await;
    // Décocher le bypass, la chaîne entière renvoyée.
    let (s, v) = b
        .put(
            json!({"version": 1, "bypass_all": false, "plugins": [conv.clone(), eq.clone()],
            "fixed": {"upsampling": up.clone()}}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(!b.chaine().bypass_all);
    assert_eq!(
        etape(&v, "convolution")["reason"],
        json!("premium_required"),
        "{v}"
    );
    // L'égaliseur paramétrique est lu converti (P2) ; enregistré tel quel.
    assert!(
        b.chaine()
            .plugins
            .iter()
            .any(|m| m.id == "equalizer-parametric" && m.enabled)
    );
    // Déplacer, décocher, retirer : 200.
    for plugins in [
        json!([eq.clone(), conv.clone()]),
        json!([eq.clone(), {"id": "convolution", "enabled": false, "settings": {"ir_path": "/x.wav"}}]),
        json!([eq.clone()]),
    ] {
        let (s, v) = b
            .put(
                json!({"version": 1, "bypass_all": false, "plugins": plugins,
                "fixed": {"upsampling": up.clone()}}),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{plugins} → {v}");
    }
    // Contre-épreuve : recocher un maillon Premium retiré reste un 402.
    let (s, v) = b
        .put(
            json!({"version": 1, "bypass_all": false, "plugins": [eq, conv],
            "fixed": {"upsampling": up}}),
        )
        .await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{v}");
    refus_conforme(&v, 402, "premium_required");
    assert_eq!(v["plugin_id"], json!("convolution"));
}

/// Un paquet tiers installé mais non chargé : absent du catalogue, son
/// maillon s'affiche `unknown_plugin`. Renvoyé inchangé, il est conservé
/// et le reste de la chaîne se modifie ; modifié, il reste refusé (409).
#[tokio::test]
async fn un_maillon_tiers_inconnu_inchange_est_conserve_t08() {
    for premium in [true, false] {
        let b = Banc::nouveau(premium).await;
        b.settings()
            .set(
                &format!("zone_{}_native_plugin_acme-reverb", b.zone),
                r#"{"enabled":true,"mix":0.3}"#,
            )
            .unwrap();
        let (s, v) = b.get().await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(etape(&v, "acme-reverb")["reason"], json!("unknown_plugin"));
        let mut c = v["chain"].clone();
        c["plugins"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "crossfeed", "enabled": true}));
        let (s, v) = b.put(c.clone()).await;
        assert_eq!(s, StatusCode::OK, "premium={premium} : {c} → {v}");
        let chaine = b.chaine();
        let tiers = chaine
            .plugins
            .iter()
            .find(|m| m.id == "acme-reverb")
            .expect("maillon tiers conservé");
        assert!(tiers.enabled);
        assert_eq!(tiers.settings, json!({"mix": 0.3}));
        assert!(
            chaine
                .plugins
                .iter()
                .any(|m| m.id == "crossfeed" && m.enabled)
        );
        // Contre-épreuve : un maillon inconnu modifié reste un 409.
        let mut c2 = c.clone();
        for m in c2["plugins"].as_array_mut().unwrap() {
            if m["id"] == json!("acme-reverb") {
                m["settings"] = json!({"mix": 0.9});
            }
        }
        let (s, v) = b.put(c2).await;
        assert_eq!(s, StatusCode::CONFLICT, "{v}");
        refus_conforme(&v, 409, "unknown_plugin");
    }
}

// ── D1 : l'origine de la correction de pièce vient du serveur ────────

fn filtres_de_piece() -> Vec<tune_core::room_correction::CorrectionFilter> {
    use tune_core::room_correction::{CorrectionFilter, FilterType};
    vec![
        CorrectionFilter {
            frequency_hz: 52.0,
            gain_db: -6.5,
            q_factor: 4.0,
            filter_type: FilterType::Peaking,
        },
        CorrectionFilter {
            frequency_hz: 118.0,
            gain_db: -3.0,
            q_factor: 2.5,
            filter_type: FilterType::Peaking,
        },
    ]
}

impl Banc {
    /// Pose `room_profile_{zone}`, comme la calibration (C2 mesurée, C5
    /// estimée sans micro).
    fn poser_la_piece(&self, origine: &str) {
        let piece = tune_core::room_correction::RoomProfile {
            name: if origine == "estimation" {
                format!("Estimation {}", self.zone)
            } else {
                "Living room".into()
            },
            zone_id: self.zone.to_string(),
            filters: filtres_de_piece(),
            created_at: "2026-11-20T00:00:00Z".into(),
            measurement_data: None,
            origin: Some(origine.into()),
        };
        self.settings()
            .set(
                &format!("room_profile_{}", self.zone),
                &serde_json::to_string(&piece).unwrap(),
            )
            .unwrap();
    }
}

/// Le maillon `room-correction` que l'écran renvoie, avec l'origine que le
/// CLIENT prétend.
fn correction(origine: Option<&str>) -> Value {
    let eq = tune_core::room_correction::profil_eq_de_correction(&filtres_de_piece());
    let mut s = serde_json::to_value(&eq).unwrap();
    s.as_object_mut().unwrap().remove("enabled");
    if let Some(o) = origine {
        s["origin"] = json!(o);
    }
    json!({"id": "room-correction", "enabled": true, "settings": s})
}

#[tokio::test]
async fn une_origine_envoyee_par_le_client_ne_leve_jamais_le_droit_t08() {
    // Gratuit, aucun profil de pièce : « estimation » prétendue par le
    // client → réécrite « measured » → 402, rien d'écrit.
    let b = Banc::nouveau(false).await;
    let (s, v) = b
        .put(json!({"version": 1, "plugins": [correction(Some("estimation"))]}))
        .await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{v}");
    refus_conforme(&v, 402, "premium_required");
    assert_eq!(v["plugin_id"], json!("room-correction"));
    assert!(b.enregistree().is_none());

    // Une pièce MESURÉE au micro : toujours Premium (D1).
    b.poser_la_piece("measured");
    let (s, v) = b
        .put(json!({"version": 1, "plugins": [correction(Some("estimation"))]}))
        .await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{v}");
    assert!(b.enregistree().is_none());

    // Contre-épreuve : la pièce de la zone est CALCULÉE SANS MICRO (C5) :
    // Free, même corps (et même sans origine) → accepté et effectif.
    b.poser_la_piece("estimation");
    let (s, v) = b
        .put(json!({"version": 1, "plugins": [correction(None)]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let e = etape(&v, "room-correction");
    assert_eq!(
        (e["effective"].clone(), e["reason"].clone()),
        (json!(true), Value::Null),
        "{v}"
    );
    assert_eq!(
        b.chaine().plugins[0].settings["origin"],
        json!("estimation")
    );
    conforme_au_schema("zone_response", &v).unwrap();
}

#[tokio::test]
async fn la_correction_mesuree_convertie_redevient_la_mesure_enregistree_t08() {
    let b = Banc::nouveau(true).await;
    b.poser_la_piece("measured");
    let (s, v) = b
        .put(json!({"version": 1, "plugins": [
            correction(None),
            {"id": "crossfeed", "enabled": false}
        ]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(b.chaine().plugins[0].settings["origin"], json!("measured"));
    // Compte redevenu gratuit : le GET montre la mesure convertie en
    // graphique au plus près (D1, comme P2).
    b.premium(false).await;
    let (_, v) = b.get().await;
    assert_eq!(
        v["chain"]["plugins"][0]["id"],
        json!("equalizer-graphic"),
        "{v}"
    );
    // Renvoyée telle quelle, crossfeed coché : la mesure enregistrée reste.
    let mut c = v["chain"].clone();
    c["plugins"][1]["enabled"] = json!(true);
    let (s, v) = b.put(c.clone()).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let enr = b.chaine();
    assert_eq!(enr.plugins[0].id, "room-correction", "{:?}", enr.plugins);
    assert_eq!(enr.plugins[0].settings["origin"], json!("measured"));
    assert!(enr.plugins[1].enabled);
    // Redevenu Premium : la mesure traite de nouveau.
    b.premium(true).await;
    let (_, v) = b.get().await;
    assert_eq!(
        etape(&v, "room-correction")["effective"],
        json!(true),
        "{v}"
    );
}

#[tokio::test]
async fn le_graphique_converti_p2_redevient_le_parametrique_enregistre_t08() {
    let b = Banc::nouveau(true).await;
    let (s, _) = b
        .put(json!({"version": 1, "plugins": [
            {"id": "equalizer-parametric", "enabled": true, "settings": profil(bandes_parametriques())},
            {"id": "crossfeed", "enabled": false}
        ]}))
        .await;
    assert_eq!(s, StatusCode::OK);
    b.premium(false).await;
    let (_, v) = b.get().await;
    assert_eq!(
        v["chain"]["plugins"][0]["id"],
        json!("equalizer-graphic"),
        "{v}"
    );
    // Le compte gratuit renvoie la chaîne vue, crossfeed coché.
    let mut c = v["chain"].clone();
    c["plugins"][1]["enabled"] = json!(true);
    let (s, v) = b.put(c.clone()).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let enr = b.chaine();
    assert_eq!(
        enr.plugins[0].id, "equalizer-parametric",
        "{:?}",
        enr.plugins
    );
    assert!(enr.plugins[1].enabled);
    // Contre-épreuve : un graphique MODIFIÉ remplace le paramétrique.
    c["plugins"][0]["settings"]["bands"][0]["gain"] = json!(1.5);
    let (s, v) = b.put(c).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(b.chaine().plugins[0].id, "equalizer-graphic");
}

// ── Authentification ────────────────────────────────────────────────

#[tokio::test]
async fn authentification_comme_les_autres_routes_de_zone_t08() {
    let b = Banc::nouveau(true).await;
    let s = b.settings();
    s.set("auth_enabled", "true").unwrap();
    s.set("jwt_secret", SECRET).unwrap();
    let z = b.zone;
    let corps = Some(json!({"version": 1}).to_string());
    let jeton = tune_server::auth::sign_jwt(2, "user", SECRET).unwrap();
    for (m, uri, c) in [
        (Method::GET, format!("/api/v1/zones/{z}/tess"), None),
        (
            Method::PUT,
            format!("/api/v1/zones/{z}/tess"),
            corps.clone(),
        ),
        (Method::GET, "/api/v1/tess/catalog".to_owned(), None),
        // Témoin : l'ancienne route de zone se comporte de même.
        (Method::GET, format!("/api/v1/zones/{z}/dsp"), None),
    ] {
        let (st, _) = b.brut(m.clone(), &uri, c.clone(), None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{m} {uri}");
        let (st, v) = b.brut(m.clone(), &uri, c.clone(), Some("faux")).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{m} {uri} {v}");
        let (st, v) = b.brut(m.clone(), &uri, c, Some(&jeton)).await;
        assert_eq!(st, StatusCode::OK, "{m} {uri} {v}");
    }
}

/// T12b — une zone de la famille UPnP publie la voie de sa chaîne
/// (`delivery`), conforme au schéma ; une zone locale ne la publie pas.
/// Rien n'a joué : l'état ne SONDE pas (relecture du 23/11). DLNA et OpenHome
/// restent `unknown` (aucune réponse mémorisée), donc un crossfeed seul n'y
/// est pas promis ; BluOS et Chromecast acceptent le LPCM 16 bits par type,
/// SlimProto le refuse.
#[tokio::test]
async fn la_famille_upnp_publie_la_voie_de_sa_chaine_t12b() {
    for (sortie, appareil, lpcm, voie, crossfeed) in [
        ("dlna", "uuid:absent-t12b", "unknown", "untouched", false),
        (
            "openhome",
            "uuid:oh-absent-t12b",
            "unknown",
            "untouched",
            false,
        ),
        (
            "bluos",
            "bluos:absent-t12b",
            "accepted",
            "progressive_lpcm",
            true,
        ),
        (
            "chromecast",
            "cast:absent-t12b",
            "accepted",
            "progressive_lpcm",
            true,
        ),
        (
            "slimproto",
            "slim:absent-t12b",
            "refused",
            "untouched",
            false,
        ),
    ] {
        let b = Banc::avec_sortie(true, sortie, appareil).await;
        let (s, v) = b.get().await;
        assert_eq!(s, StatusCode::OK, "{v}");
        conforme_au_schema("zone_response", &v).unwrap_or_else(|e| panic!("{sortie} {e} : {v}"));
        assert_eq!(v["delivery"]["route"], json!("untouched"), "{sortie} : {v}");
        assert_eq!(v["delivery"]["reason"], json!("nothing_to_process"), "{v}");
        let (s, v) = b
            .brut(
                Method::PUT,
                &format!("/api/v1/zones/{}/dsp", b.zone),
                Some(
                    json!({"crossfeed": {"enabled": true, "amount": 0.35, "delay_ms": 0.3}})
                        .to_string(),
                ),
                None,
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        let (_, v) = b.get().await;
        conforme_au_schema("zone_response", &v).unwrap();
        assert_eq!(v["delivery"]["route"], json!(voie), "{sortie} : {v}");
        assert_eq!(
            v["delivery"]["renderer_lpcm"],
            json!(lpcm),
            "{sortie} : {v}"
        );
        assert_eq!(
            etape(&v, "crossfeed")["effective"],
            json!(crossfeed),
            "{sortie} : {v}"
        );
        if !crossfeed {
            assert_eq!(
                etape(&v, "crossfeed")["reason"],
                json!("renderer_without_lpcm"),
                "{v}"
            );
        }
    }
    // Contre-épreuve : une zone locale n'a pas de `delivery`.
    let b = Banc::nouveau(true).await;
    let (_, v) = b.get().await;
    assert!(v.get("delivery").is_none(), "{v}");
}

/// T12b (relecture de suivi) — un `GET` d'état pendant qu'une LECTURE sonde
/// un lecteur muet (elle garde le verrou de la sortie pendant la sonde SOAP) :
/// la route n'attend ni la sonde ni le verrou, et répond en moins d'une
/// seconde.
#[tokio::test]
async fn le_get_d_etat_n_attend_pas_la_sonde_d_une_lecture_t12b() {
    let muet = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = muet.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut tenues = Vec::new();
        while let Ok((sock, _)) = muet.accept().await {
            tenues.push(sock);
        }
    });
    let appareil = "uuid:muet-t12b-route";
    let b = Banc::avec_sortie(true, "dlna", appareil).await;
    b.state
        .outputs
        .lock()
        .await
        .register(Box::new(tune_core::outputs::dlna::DlnaOutput::new(
            "Lecteur muet".into(),
            appareil.into(),
            "127.0.0.1".into(),
            format!("http://127.0.0.1:{port}/av"),
            format!("http://127.0.0.1:{port}/rc"),
            Some(format!("http://127.0.0.1:{port}/cm")),
        )));
    // La lecture en parallèle : sa sonde tient le verrou de la sortie.
    let orch = b.state.orchestrator.clone();
    let sonde = tokio::spawn(async move { orch.dlna_accepte_lpcm(appareil, false).await });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !sonde.is_finished(),
        "la sonde de lecture doit être en cours"
    );
    let t0 = std::time::Instant::now();
    let (s, v) = tokio::time::timeout(std::time::Duration::from_secs(1), b.get())
        .await
        .expect("le GET d'état répond en moins d'une seconde");
    assert!(t0.elapsed() < std::time::Duration::from_secs(1));
    assert_eq!(s, StatusCode::OK, "{v}");
    conforme_au_schema("zone_response", &v).unwrap();
    assert_eq!(v["delivery"]["renderer_lpcm"], json!("unknown"), "{v}");
    sonde.abort();
}

/// rc14 (GO du 25/11) — les égaliseurs sont COMBINABLES : graphique et
/// paramétrique cochés ensemble passent (200), restent cochés, et sont tous
/// deux effectifs. `zone_{id}_eq_profile` (anciennes routes) porte le
/// premier ; un ancien client qui le lit ou l'écrit ne casse rien. Le groupe
/// crossfeed, lui, reste exclusif.
#[tokio::test]
async fn deux_egaliseurs_coches_passent_et_s_enchainent_rc14() {
    let b = Banc::nouveau(true).await;
    let (s, v) = b
        .put(json!({"version": 1, "plugins": [
            {"id": "equalizer-graphic", "enabled": true, "settings": profil(json!([]))},
            {"id": "equalizer-parametric", "enabled": true, "settings": profil(bandes_parametriques())}
        ]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let c = b.chaine();
    let coches: Vec<&str> = c
        .plugins
        .iter()
        .filter(|m| m.enabled)
        .map(|m| m.id.as_str())
        .collect();
    assert_eq!(coches, ["equalizer-graphic", "equalizer-parametric"]);
    for id in ["equalizer-graphic", "equalizer-parametric"] {
        let e = etape(&v, id);
        assert_eq!(e["effective"], json!(true), "{id} : {e}");
        assert_eq!(e["reason"], Value::Null, "{id} : {e}");
    }
    // Ancien client : `GET /zones/{id}/eq` lit le principal (le graphique,
    // à plat), et son `POST` le règle sans décocher le paramétrique.
    let uri = format!("/api/v1/zones/{}/eq", b.zone);
    let (s, eq) = b.brut(Method::GET, &uri, None, None).await;
    assert_eq!(s, StatusCode::OK, "{eq}");
    assert_eq!(eq["bands"], json!([]), "{eq}");
    let (s, eq) = b
        .brut(
            Method::POST,
            &uri,
            Some(json!({"enabled": true, "bands": [{"freq": 1000.0, "gain": 2.0, "q": 1.0, "type": "peak"}]}).to_string()),
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{eq}");
    let c = b.chaine();
    assert!(
        c.plugins
            .iter()
            .any(|m| m.id == "equalizer-parametric" && m.enabled),
        "{c:?}"
    );
    // Le catalogue ne publie plus de groupe « equalizer ».
    let (_, cat) = b
        .brut(Method::GET, "/api/v1/tess/catalog", None, None)
        .await;
    let groupes = cat["catalog"]["groups"].as_array().unwrap();
    assert!(
        !groupes.iter().any(|x| x["id"] == json!("equalizer")),
        "{cat}"
    );
    // Groupe crossfeed inchangé : toujours 409.
    let (s, v) = b
        .put(json!({"version": 1, "plugins": [{"id": "crossfeed", "enabled": true}, {"id": "crosstalk", "enabled": true}]}))
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    refus_conforme(&v, 409, "mutually_exclusive");
}

/// rc14 — une zone enregistrée sous l'exclusivité du 24/11 (paramétrique
/// décoché avec `replaced_in_group` vers le graphique) le retrouve coché à
/// la lecture, sans marque.
#[tokio::test]
async fn une_zone_normalisee_par_l_exclusivite_retrouve_ses_egaliseurs_rc14() {
    let b = Banc::nouveau(true).await;
    let (s, v) = b
        .put(json!({"version": 1, "plugins": [
            {"id": "equalizer-graphic", "enabled": true, "settings": profil(json!([]))},
            {"id": "equalizer-parametric", "enabled": true, "settings": profil(bandes_parametriques())}
        ]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // La chaîne telle que #9512 l'aurait enregistrée.
    let mut brute: Value = serde_json::from_str(&b.enregistree().unwrap()).unwrap();
    for m in brute["plugins"].as_array_mut().unwrap() {
        if m["id"] == json!("equalizer-parametric") {
            m["enabled"] = json!(false);
            m["settings"]["replaced_in_group"] = json!("equalizer-graphic");
        }
    }
    b.settings()
        .set(&cle_chaine_zone(b.zone), &brute.to_string())
        .unwrap();
    let (_, v) = b.get().await;
    let p = etape(&v, "equalizer-parametric");
    assert_eq!(p["effective"], json!(true), "{p}");
    let c = b.chaine();
    let m = c
        .plugins
        .iter()
        .find(|m| m.id == "equalizer-parametric")
        .unwrap();
    assert!(m.enabled);
    assert!(m.settings.get("replaced_in_group").is_none(), "{m:?}");
}

/// rc14 — bornes de gain publiées au chemin que lit le web, et tenues au
/// `PUT` : −30 / +15 dB pour le paramétrique et la pièce, −30 / +20 dB pour
/// le casque (OPRA, #9519), ±12 dB pour le graphique.
#[tokio::test]
async fn les_bornes_de_gain_sont_publiees_et_tenues_rc14() {
    let b = Banc::nouveau(true).await;
    let (_, cat) = b
        .brut(Method::GET, "/api/v1/tess/catalog", None, None)
        .await;
    let gain = |id: &str| {
        cat["catalog"]["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == json!(id))
            .unwrap()["settings_schema"]["properties"]["bands"]["items"]["properties"]["gain"]
            .clone()
    };
    for (id, min, max) in [
        ("equalizer-parametric", -30.0, 15.0),
        ("equalizer-headphone", -30.0, 20.0),
        ("room-correction", -30.0, 15.0),
        ("equalizer-graphic", -12.0, 12.0),
    ] {
        let g = gain(id);
        assert_eq!(
            (g["minimum"].as_f64(), g["maximum"].as_f64()),
            (Some(min), Some(max)),
            "{id} : {g}"
        );
    }
    let parametrique = |g: f64| {
        json!({"version": 1, "plugins": [
            {"id": "equalizer-parametric", "enabled": true,
             "settings": profil(json!([{"freq": 3000.0, "gain": g, "q": 4.0, "type": "peak"}]))}
        ]})
    };
    let (s, v) = b.put(parametrique(-30.0)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = b.put(parametrique(15.0)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    for g in [-30.5, 15.5] {
        let avant = b.enregistree();
        let (s, v) = b.put(parametrique(g)).await;
        assert_eq!(s, StatusCode::CONFLICT, "{g} : {v}");
        refus_conforme(&v, 409, "invalid_settings");
        assert_eq!(b.enregistree(), avant);
    }
    let (s, v) = b
        .put(json!({"version": 1, "plugins": [
            {"id": "equalizer-graphic", "enabled": true,
             "settings": profil(json!([{"freq": 1000.0, "gain": -13.0, "q": 1.0, "type": "peak"}]))}
        ]}))
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    refus_conforme(&v, 409, "invalid_settings"); // #9519 — une correction casque OPRA à +20 dB passe.
    let (s, v) = b
        .put(json!({"version": 1, "plugins": [
            {"id": "equalizer-headphone", "enabled": true,
             "settings": profil(json!([{"freq": 9000.0, "gain": 20.0, "q": 2.0, "type": "peak"}]))}
        ]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

/// rc14 — un paquet installé mais désactivé n'est pas « à installer » : il
/// reste `plugin_unavailable`, sans `install_package`. Désinstallé, il
/// redevient `plugin_not_installed` avec le paquet à installer.
#[tokio::test]
async fn catalogue_distingue_desactive_et_non_installe_rc14() {
    let b = Banc::nouveau(true).await;
    let etat = |rep: &Value, id: &str| {
        rep["plugins"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == json!(id))
            .unwrap()
            .clone()
    };
    let catalogue = || async {
        let (s, v) = b
            .brut(Method::GET, "/api/v1/tess/catalog", None, None)
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        conforme_au_schema("catalog_response", &v).unwrap_or_else(|e| panic!("{e} : {v}"));
        v
    };
    // Tout installé et actif : rien à installer.
    let v = catalogue().await;
    let eq = etat(&v, "equalizer-graphic");
    assert_eq!(eq["available"], json!(true), "{eq}");
    assert!(eq.get("install_package").is_none(), "{eq}");

    b.settings()
        .set("plugin_equalizer_enabled", "false")
        .unwrap();
    let v = catalogue().await;
    let eq = etat(&v, "equalizer-graphic");
    assert_eq!(eq["reason"], json!("plugin_unavailable"), "{eq}");
    assert!(eq.get("install_package").is_none(), "{eq}");

    b.settings()
        .set("plugin_equalizer_installed", "false")
        .unwrap();
    b.settings()
        .set("plugin_channel-remap_installed", "false")
        .unwrap();
    let v = catalogue().await;
    for (id, paquet) in [
        ("equalizer-graphic", "equalizer"),
        ("equalizer-parametric", "equalizer"),
        ("room-correction", "equalizer"),
        ("channel-remap", "channel-remap"),
    ] {
        let e = etat(&v, id);
        assert_eq!(e["available"], json!(false), "{e}");
        assert_eq!(e["reason"], json!("plugin_not_installed"), "{e}");
        assert_eq!(e["install_package"], json!(paquet), "{e}");
    }
    // Ni le crossfeed (installé), ni les greffons natifs tiers, ni le
    // déclic en direct ne proposent d'installation.
    for id in ["crossfeed", "crosstalk", "declick-live", "convolution"] {
        let e = etat(&v, id);
        assert!(e.get("install_package").is_none(), "{e}");
        assert_ne!(e["reason"], json!("plugin_not_installed"), "{e}");
    }
}

/// rc14 — le web ajoute la convolution cochée avant qu'un fichier soit
/// choisi (`settings: null`). Ce n'est pas une chaîne invalide : 200, le
/// maillon est inactif (`no_filter_selected`) et aucun `ir_path_{zone}`
/// n'est posé. Contre-épreuve : avec un fichier, elle traite.
#[tokio::test]
async fn convolution_cochee_sans_fichier_acceptee_et_inactive_rc14() {
    let b = Banc::nouveau(true).await;
    let etape_conv = |v: &Value| {
        v["signal_path"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["plugin_id"] == json!("convolution"))
            .unwrap_or_else(|| panic!("convolution absente du chemin : {v}"))
            .clone()
    };
    let cle_ir = format!("ir_path_{}", b.zone);
    for reglages in [Value::Null, json!({}), json!({"ir_path": "  "})] {
        let (s, v) = b
            .put(json!({"version": 1, "plugins": [
                {"id": "convolution", "enabled": true, "settings": reglages}
            ]}))
            .await;
        assert_eq!(s, StatusCode::OK, "{reglages} → {v}");
        conforme_au_schema("zone_response", &v).unwrap_or_else(|e| panic!("{e} : {v}"));
        let e = etape_conv(&v);
        assert_eq!(e["requested"], json!(true), "{e}");
        assert_eq!(e["effective"], json!(false), "{e}");
        assert_eq!(e["reason"], json!("no_filter_selected"), "{e}");
        assert!(b.settings().get(&cle_ir).unwrap().is_none());
        // Relu par GET, même chose.
        let (_, v) = b.get().await;
        assert_eq!(etape_conv(&v)["reason"], json!("no_filter_selected"));
    }
    // Contre-épreuve : un fichier choisi, la convolution traite et le pose.
    let (s, v) = b
        .put(json!({"version": 1, "plugins": [
            {"id": "convolution", "enabled": true, "settings": {"ir_path": "/x.wav"}}
        ]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let e = etape_conv(&v);
    assert_eq!(e["effective"], json!(true), "{e}");
    assert!(e["reason"].is_null(), "{e}");
    assert_eq!(
        b.settings().get(&cle_ir).unwrap().as_deref(),
        Some("/x.wav")
    );
}

/// rc14 (relecture) — une zone a déjà `ir_path_{zone}` ; un client envoie la
/// convolution cochée SANS fichier : la clé est effacée (comme décochée),
/// sinon le DSP convoluerait encore `/a.wav` pendant que la réponse dit
/// `no_filter_selected`.
#[tokio::test]
async fn convolution_cochee_sans_fichier_efface_l_ancien_ir_rc14() {
    let cle_ir = |b: &Banc| format!("ir_path_{}", b.zone);
    for reglages in [Value::Null, json!({}), json!({"ir_path": "  "})] {
        let b = Banc::nouveau(true).await;
        let (s, v) = b
            .put(json!({"version": 1, "plugins": [
                {"id": "convolution", "enabled": true, "settings": {"ir_path": "/a.wav"}}
            ]}))
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(
            b.settings().get(&cle_ir(&b)).unwrap().as_deref(),
            Some("/a.wav")
        );
        let (s, v) = b
            .put(json!({"version": 1, "plugins": [
                {"id": "convolution", "enabled": true, "settings": reglages}
            ]}))
            .await;
        assert_eq!(s, StatusCode::OK, "{reglages} → {v}");
        let e = v["signal_path"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["plugin_id"] == json!("convolution"))
            .unwrap()
            .clone();
        assert_eq!(e["effective"], json!(false), "{e}");
        assert_eq!(e["reason"], json!("no_filter_selected"), "{e}");
        assert!(
            b.settings().get(&cle_ir(&b)).unwrap().is_none(),
            "{reglages} : ir_path_{{zone}} encore posé"
        );
    }
}

/// rc14 (Rémi, base neuve) — le moteur `upsampler`, facultatif, n'est pas
/// installé : l'étage fixe de suréchantillonnage demandé ne traite pas, et
/// le chemin du signal le dit `plugin_not_installed` avec
/// `params.install_package = "upsampler"` (avant : annoncé effectif alors
/// que rien ne suréchantillonnait). Installé, il traite. Désactivé :
/// `plugin_unavailable`, sans paquet.
#[tokio::test]
async fn surechantillonnage_non_installe_propose_l_installation_rc14() {
    let b = Banc::nouveau(true).await;
    b.settings()
        .set("plugin_upsampler_installed", "false")
        .unwrap();
    let etape = |v: &Value| {
        v["signal_path"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["code"] == json!("upsampling"))
            .unwrap()
            .clone()
    };
    let corps =
        json!({"version": 1, "fixed": {"upsampling": {"mode": "pcm", "filter": "polyvalent"}}});
    let (s, v) = b.put(corps.clone()).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    conforme_au_schema("zone_response", &v).unwrap_or_else(|e| panic!("{e} : {v}"));
    let e = etape(&v);
    assert_eq!(e["requested"], json!(true), "{e}");
    assert_eq!(e["effective"], json!(false), "{e}");
    assert_eq!(e["reason"], json!("plugin_not_installed"), "{e}");
    assert_eq!(e["params"]["install_package"], json!("upsampler"), "{e}");
    let (_, v) = b.get().await;
    assert_eq!(etape(&v)["reason"], json!("plugin_not_installed"));

    // Installé (ce que fait `POST /plugins/upsampler/install`) : il traite.
    b.settings()
        .set("plugin_upsampler_installed", "true")
        .unwrap();
    let (_, v) = b.get().await;
    let e = etape(&v);
    assert_eq!(e["effective"], json!(true), "{e}");
    assert!(e["reason"].is_null(), "{e}");

    // Installé mais désactivé : indisponible, rien à installer.
    b.settings()
        .set("plugin_upsampler_enabled", "false")
        .unwrap();
    let (_, v) = b.get().await;
    let e = etape(&v);
    assert_eq!(e["reason"], json!("plugin_unavailable"), "{e}");
    assert!(e["params"].get("install_package").is_none(), "{e}");
}

/// Revue #9520 (2) — un ancien client ne connaît qu'un égaliseur :
/// `POST /zones/{id}/eq` `enabled: false` éteint TOUTE la série (le moteur
/// n'applique plus rien, le `GET` le dit), et `enabled: true` les rallume
/// toutes ensemble, réglages gardés.
#[tokio::test]
async fn revue_9520_ancien_client_eteint_l_egaliseur() {
    let b = Banc::nouveau(true).await;
    // Le graphique à plat (curseurs) : la façade qu'une ancienne route relit
    // comme graphique.
    let mut graphique = profil(json!([]));
    graphique["bass_gain_db"] = json!(3.0);
    let parametrique = profil(bandes_parametriques());
    let (s, v) = b
        .put(json!({"version": 1, "plugins": [
            {"id": "equalizer-graphic", "enabled": true, "settings": graphique},
            {"id": "equalizer-parametric", "enabled": true, "settings": parametrique}
        ]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let reglages = |c: &ChaineTess, id: &str| {
        let m = c.plugins.iter().find(|m| m.id == id).unwrap();
        let mut r = m.settings.clone();
        if let Some(o) = r.as_object_mut() {
            o.remove("off_by_legacy_eq");
        }
        (m.enabled, r)
    };
    let avant = b.chaine();
    let uri = format!("/api/v1/zones/{}/eq", b.zone);
    let (s, eq) = b
        .brut(Method::POST, &uri, Some(json!({"enabled": false}).to_string()), None)
        .await;
    assert_eq!(s, StatusCode::OK, "{eq}");
    let c = b.chaine();
    for id in ["equalizer-graphic", "equalizer-parametric"] {
        assert!(!reglages(&c, id).0, "{id} doit être éteint : {c:?}");
    }
    assert!(
        tune_core::tess::egaliseurs_en_serie::profil_du_moteur(&b.settings(), b.zone, true)
            .is_none(),
        "le moteur ne doit plus rien appliquer"
    );
    let (_, eq) = b.brut(Method::GET, &uri, None, None).await;
    assert_eq!(eq["enabled"], json!(false), "{eq}");
    let (_, v) = b.get().await;
    for id in ["equalizer-graphic", "equalizer-parametric"] {
        assert_eq!(etape(&v, id)["effective"], json!(false), "{id}");
    }

    let (s, eq) = b
        .brut(Method::POST, &uri, Some(json!({"enabled": true}).to_string()), None)
        .await;
    assert_eq!(s, StatusCode::OK, "{eq}");
    let apres = b.chaine();
    for id in ["equalizer-graphic", "equalizer-parametric"] {
        assert_eq!(reglages(&apres, id), reglages(&avant, id), "{id}");
    }
    let p = tune_core::tess::egaliseurs_en_serie::profil_du_moteur(&b.settings(), b.zone, true)
        .expect("série rallumée");
    // Les trois curseurs du graphique puis les deux bandes du paramétrique.
    assert_eq!(p.bands.len(), 5, "{p:?}");
    let (_, eq) = b.brut(Method::GET, &uri, None, None).await;
    assert_eq!(eq["enabled"], json!(true), "{eq}");
}
