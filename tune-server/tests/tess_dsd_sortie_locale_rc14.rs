//! rc14 — TESS vers le DSD sur la sortie locale : le contrat de l'API.
//!
//! - `dsd_capabilities` publié dans `GET/PUT /zones/{id}/tess` ;
//! - 402 `premium_required` (`plugin_id: "upsampling"`) en Free, AVANT tout
//!   contrôle de la sortie ;
//! - 409 `dsd_not_supported_by_output` quand la sortie locale ne prend pas la
//!   cible : la chaîne enregistrée ne bouge pas, le suréchantillonnage PCM en
//!   place reste allumé ;
//! - le chemin résolu dit le transport (natif/DoP) et l'ordre du modulateur ;
//! - une sortie réseau n'est pas touchée (`not_applied_on_transport`).
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
use tune_core::tess::chaine::Upsampling;
use tune_core::tess::migration::lire_zone;

struct Banc {
    state: tune_server::state::AppState,
    app: axum::Router,
    zone: i64,
}

impl Banc {
    async fn nouveau(premium: bool, device: &str, output_type: &str, dsd_mode: &str) -> Self {
        let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        let settings = SettingsRepo::with_backend(state.backend.clone());
        settings
            .set(tune_core::audio::premium_plugins::MIGRATION, "complete")
            .unwrap();
        settings.set("plugin_upsampler_installed", "true").unwrap();
        settings.set("plugin_upsampler_enabled", "true").unwrap();
        state.license.set_account_premium(premium, None).await;
        let zones = ZoneRepo::with_backend(state.backend.clone());
        let zone = zones
            .create("DAC", Some(output_type), Some(device))
            .unwrap();
        zones.update_volume(zone, 100.0).unwrap();
        zones.update_dsd_mode(zone, dsd_mode).unwrap();
        // rc14.1 (fils 3123, 3126) — le DoP modulé ne part que sur une sortie
        // EXCLUSIVE : la case (macOS, Windows) et le PCM `hw:` (Linux).
        settings.set("local_exclusive_mode", "true").unwrap();
        zones
            .update_output_endpoint_id(zone, "alsa:hw:CARD=DAC,DEV=0")
            .unwrap();
        // La machine de test (debug) n'est pas celle de la production.
        tune_core::tess::modulation_dsd::retenir_mesure_de_la_machine([50.0, 25.0, 12.0]);
        let app = tune_server::routes::router(state.clone());
        Self { state, app, zone }
    }

    async fn requete(&self, methode: Method, corps: Option<Value>) -> (StatusCode, Value) {
        let uri = format!("/api/v1/zones/{}/tess", self.zone);
        let req = Request::builder()
            .method(methode)
            .uri(uri)
            .header("Content-Type", "application/json")
            .body(corps.map_or(Body::empty(), |c| Body::from(c.to_string())))
            .unwrap();
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    fn upsampling_enregistre(&self) -> Upsampling {
        let s = SettingsRepo::with_backend(self.state.backend.clone());
        lire_zone(&s, self.zone, true)
            .unwrap()
            .chaine
            .fixed
            .upsampling
    }

    fn surechantillonneur_pcm_allume(&self) -> bool {
        let s = SettingsRepo::with_backend(self.state.backend.clone());
        tune_core::audio::surechantillonnage::reglages_de_la_zone(&s, self.zone).enabled
    }
}

fn dsd(rate: &str) -> Value {
    json!({"version": 1, "fixed": {"upsampling": {"mode": "dsd", "rate": rate}}})
}

fn pcm() -> Value {
    json!({"version": 1, "fixed": {"upsampling": {"mode": "pcm", "filter": "polyvalent"}}})
}

fn etape<'a>(v: &'a Value, code: &str) -> Option<&'a Value> {
    v["signal_path"]
        .as_array()?
        .iter()
        .find(|e| e["code"] == json!(code))
}

fn cible<'a>(v: &'a Value, rate: &str) -> &'a Value {
    v["dsd_capabilities"]["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["rate"] == json!(rate))
        .unwrap_or_else(|| panic!("pas de cible {rate} : {v}"))
}

#[tokio::test]
async fn les_capacites_sont_publiees_et_conformes_au_schema() {
    let b = Banc::nouveau(true, "local:SMSL SU-9", "local", "dop").await;
    ZoneRepo::with_backend(b.state.backend.clone())
        .update_max_sample_rate(b.zone, Some(384_000))
        .unwrap();
    let (s, v) = b.requete(Method::GET, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    conforme_au_schema("zone_response", &v).unwrap_or_else(|e| panic!("{e} : {v}"));
    let caps = &v["dsd_capabilities"];
    assert_eq!(caps["dsd_mode"], "dop");
    assert_eq!(caps["output_local"], true);
    assert_eq!(caps["modulator_order"], 7);
    assert_eq!(caps["volume_placement"], "before_dsd_modulator");
    assert_eq!(
        caps["reference_level"],
        "0 dBFS = 50 % modulation (0 dB SACD)"
    );
    let d64 = cible(&v, "dsd64");
    assert_eq!(
        (&d64["accepted"], &d64["transport"], &d64["dop_rate_hz"]),
        (&json!(true), &json!("dop"), &json!(176_400))
    );
    assert_eq!(d64["family_48k"]["dop_rate_hz"], 192_000);
    let d256 = cible(&v, "dsd256");
    assert_eq!(d256["accepted"], false);
    assert_eq!(d256["reason"], "rate_above_zone_max");
}

#[tokio::test]
async fn en_free_le_dsd_rend_402_avant_tout_controle_de_la_sortie() {
    // Zone « auto » : la sortie refuserait aussi ; le droit passe d'abord.
    let b = Banc::nouveau(false, "local:SMSL SU-9", "local", "auto").await;
    let (s, v) = b.requete(Method::PUT, Some(dsd("dsd64"))).await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{v}");
    assert_eq!(v["code"], "premium_required");
    assert_eq!(v["plugin_id"], "upsampling");
    conforme_au_schema("refusal", &v).unwrap_or_else(|e| panic!("{e} : {v}"));
}

#[tokio::test]
async fn cible_impossible_409_et_le_pcm_en_place_n_est_pas_eteint() {
    let b = Banc::nouveau(true, "local:Realtek HD", "local", "auto").await;
    let (s, v) = b.requete(Method::PUT, Some(pcm())).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(b.surechantillonneur_pcm_allume());

    let (s, v) = b.requete(Method::PUT, Some(dsd("dsd128"))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    conforme_au_schema("refusal", &v).unwrap_or_else(|e| panic!("{e} : {v}"));
    assert_eq!(v["code"], "dsd_not_supported_by_output");
    assert_eq!(v["plugin_id"], "upsampling");
    assert_eq!(v["reason"], "dsd_mode_not_enabled");
    assert_eq!(v["dsd_capabilities"]["dsd_mode"], "auto");
    // Rien n'a bougé : chaîne en PCM, suréchantillonneur toujours allumé.
    assert!(matches!(b.upsampling_enregistre(), Upsampling::Pcm { .. }));
    assert!(b.surechantillonneur_pcm_allume());

    // Contre-épreuve : la zone déclare son DAC (DoP), la même requête passe.
    ZoneRepo::with_backend(b.state.backend.clone())
        .update_dsd_mode(b.zone, "dop")
        .unwrap();
    let (s, v) = b.requete(Method::PUT, Some(dsd("dsd128"))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    conforme_au_schema("zone_response", &v).unwrap_or_else(|e| panic!("{e} : {v}"));
    let m = etape(&v, "dsd_modulator").expect("étape du modulateur");
    assert_eq!(m["effective"], true, "{m}");
    assert_eq!(m["params"]["transport"], "dop", "{m}");
    assert_eq!(m["params"]["order"], 7, "{m}");
    assert_eq!(m["params"]["rate_hz"], 5_644_800, "{m}");
    assert_eq!(m["params"]["dop_rate_hz"], 352_800, "{m}");
}

#[tokio::test]
async fn capacite_perdue_apres_coup_le_chemin_le_dit_sans_bloquer_l_edition() {
    let b = Banc::nouveau(true, "local:SMSL SU-9", "local", "dop").await;
    let (s, _) = b.requete(Method::PUT, Some(dsd("dsd256"))).await;
    assert_eq!(s, StatusCode::OK);
    // Le DAC est rebranché sur une entrée qui ne prend plus le DSD : la zone
    // repasse en « pcm ».
    ZoneRepo::with_backend(b.state.backend.clone())
        .update_dsd_mode(b.zone, "pcm")
        .unwrap();
    let (s, v) = b.requete(Method::GET, None).await;
    assert_eq!(s, StatusCode::OK);
    let up = etape(&v, "upsampling").unwrap();
    assert_eq!(up["effective"], false, "{up}");
    assert_eq!(up["reason"], "dsd_not_supported_by_output", "{up}");
    assert_eq!(up["detail"], "dsd_mode_not_enabled", "{up}");
    assert_ne!(v["volume"]["placement"], "before_dsd_modulator");
    conforme_au_schema("zone_response", &v).unwrap_or_else(|e| panic!("{e} : {v}"));
    // La même cible, renvoyée inchangée avec le reste : pas de 409.
    let (s, v) = b
        .requete(
            Method::PUT,
            Some(json!({"version": 1, "bypass_all": true,
                "fixed": {"upsampling": {"mode": "dsd", "rate": "dsd256"}}})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

#[tokio::test]
async fn une_sortie_reseau_garde_not_applied_on_transport() {
    let b = Banc::nouveau(true, "uuid:renderer-dlna", "dlna", "dop").await;
    let (s, v) = b.requete(Method::PUT, Some(dsd("dsd64"))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["dsd_capabilities"]["output_local"], false);
    assert_eq!(cible(&v, "dsd64")["reason"], "output_not_local");
    // Le chemin résolu n'est pas réécrit par les capacités sur le réseau.
    let up = etape(&v, "upsampling").unwrap();
    assert_ne!(up["reason"], "dsd_not_supported_by_output", "{up}");
}
