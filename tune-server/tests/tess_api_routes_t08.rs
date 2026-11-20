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
        "chaîne vide, volume plein"
    );
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
