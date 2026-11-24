//! TESS T17 — préréglages de chaîne globaux et mode d'application, par le
//! routeur public : `GET/POST /tess/presets`, `PATCH/DELETE
//! /tess/presets/{id}`, `PUT /tess/apply-mode` et
//! `POST /zones/{id}/tess/presets/{preset_id}/apply`.
//!
//! Chaque refus a sa contre-épreuve : la même requête, corrigée du seul
//! défaut, passe.
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
use tune_core::tess::migration::lire_zone;
use tune_core::tess::{ChaineTess, cle_chaine_zone};
use tune_server::routes::zones::tess::verrou_des_put;
use tune_server::routes::zones::tess_prereglages::{
    appliquer_a_la_lecture, cles_en_attente, rafraichissements_demandes, sortie_locale,
    verrou_de_la_liste,
};

/// La file d'attente du mode `at_next_playback` est commune au processus,
/// et passer en `immediately` la sert pour TOUTES les zones ; un `PUT` ou un
/// chargement en mode immédiat vide celle de sa zone. Chaque banc tient donc
/// la série : les tests de ce fichier passent l'un après l'autre.
fn file_partagee() -> std::sync::Arc<tokio::sync::Mutex<()>> {
    static F: std::sync::OnceLock<std::sync::Arc<tokio::sync::Mutex<()>>> =
        std::sync::OnceLock::new();
    F.get_or_init(Default::default).clone()
}

struct Banc {
    state: tune_server::state::AppState,
    app: axum::Router,
    zone: i64,
    _serie: tokio::sync::OwnedMutexGuard<()>,
}

impl Banc {
    async fn nouveau(premium: bool) -> Self {
        Self::avec_sortie(premium, "local", "local:Realtek HD", 0).await
    }

    /// `avant` zones créées d'abord : la zone du banc a un numéro propre
    /// (l'attente du mode `at_next_playback` est tenue par numéro de zone).
    async fn avec_sortie(premium: bool, output_type: &str, device: &str, avant: usize) -> Self {
        let _serie = file_partagee().lock_owned().await;
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
        let repo = ZoneRepo::with_backend(state.backend.clone());
        for i in 0..avant {
            repo.create(
                &format!("Avant {i}"),
                Some("local"),
                Some(&format!("local:avant-{i}")),
            )
            .unwrap();
        }
        let zone = repo
            .create("Zone", Some(output_type), Some(device))
            .unwrap();
        repo.update_volume(zone, 100.0).unwrap();
        let app = tune_server::routes::router(state.clone());
        Self {
            state,
            app,
            zone,
            _serie,
        }
    }

    fn settings(&self) -> SettingsRepo {
        SettingsRepo::with_backend(self.state.backend.clone())
    }

    async fn premium(&self, oui: bool) {
        self.state.license.set_account_premium(oui, None).await;
    }

    async fn brut(&self, methode: Method, uri: &str, corps: Option<Value>) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(methode)
            .uri(uri)
            .header("Content-Type", "application/json");
        let resp = self
            .app
            .clone()
            .oneshot(
                req.body(corps.map_or(Body::empty(), |c| Body::from(c.to_string())))
                    .unwrap(),
            )
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

    async fn put_chaine(&self, corps: Value) -> (StatusCode, Value) {
        let uri = format!("/api/v1/zones/{}/tess", self.zone);
        self.brut(Method::PUT, &uri, Some(corps)).await
    }

    async fn get_zone(&self) -> Value {
        let uri = format!("/api/v1/zones/{}/tess", self.zone);
        let (s, v) = self.brut(Method::GET, &uri, None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v
    }

    async fn liste(&self) -> Value {
        let (s, v) = self.brut(Method::GET, "/api/v1/tess/presets", None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        conforme_au_schema("presets", &v).unwrap_or_else(|e| panic!("{e} : {v}"));
        v
    }

    async fn enregistrer(&self, nom: &str) -> (StatusCode, Value) {
        self.brut(
            Method::POST,
            "/api/v1/tess/presets",
            Some(json!({"name": nom, "zone_id": self.zone})),
        )
        .await
    }

    async fn appliquer(&self, preset: &str) -> (StatusCode, Value) {
        let uri = format!("/api/v1/zones/{}/tess/presets/{preset}/apply", self.zone);
        self.brut(Method::POST, &uri, None).await
    }

    fn chaine(&self) -> ChaineTess {
        lire_zone(&self.settings(), self.zone, true).unwrap().chaine
    }
}

fn profil(bandes: Value) -> Value {
    json!({
        "listening": "speakers", "room_size": "medium",
        "speaker_placement": "free_standing", "bass_gain_db": 0.0,
        "mid_gain_db": 0.0, "treble_gain_db": 0.0, "bands": bandes
    })
}

fn parametrique() -> Value {
    json!({"id": "equalizer-parametric", "enabled": true, "settings": profil(json!([
        {"freq": 80.0, "gain": 4.0, "q": 0.7, "type": "low_shelf"},
        {"freq": 1200.0, "gain": -3.5, "q": 1.4, "type": "peak"}
    ]))})
}

fn crossfeed() -> Value {
    json!({"id": "crossfeed", "enabled": true, "settings": {"amount": 0.4, "delay_ms": 0.3}})
}

fn ids(liste: &Value) -> Vec<String> {
    liste["presets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_owned())
        .collect()
}

// ── Liste, enregistrement, renommage, suppression ───────────────────

#[tokio::test]
async fn la_liste_part_de_pure_et_du_mode_immediat_t17() {
    let b = Banc::nouveau(true).await;
    let v = b.liste().await;
    assert_eq!(v["apply_mode"], json!("immediately"));
    assert_eq!(ids(&v), vec!["pure"]);
    assert_eq!(v["presets"][0]["builtin"], json!(true));
    assert_eq!(v["presets"][0]["chain"]["bypass_all"], json!(true));
    // Lire n'écrit rien.
    assert!(b.settings().get("tess_presets").unwrap().is_none());
}

#[tokio::test]
async fn enregistrer_la_chaine_courante_la_garde_avec_son_ordre_et_ses_reglages_t17() {
    let b = Banc::nouveau(true).await;
    let (s, v) = b
        .put_chaine(
            json!({"version": 1, "plugins": [crossfeed(), parametrique()],
                           "fixed": {"headroom": {"mode": "manual", "db": -6.0}}}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, p) = b.enregistrer("  Evening listening ").await;
    assert_eq!(s, StatusCode::CREATED, "{p}");
    assert_eq!(p["id"], json!("evening-listening"));
    assert_eq!(p["name"], json!("Evening listening"));
    assert_eq!(p["builtin"], json!(false));
    let ordre: Vec<&str> = p["chain"]["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ordre, vec!["crossfeed", "equalizer-parametric"]);
    assert_eq!(p["chain"]["plugins"][0]["settings"]["amount"], json!(0.4));
    assert_eq!(
        p["chain"]["fixed"]["headroom"],
        json!({"mode": "manual", "db": -6.0})
    );
    // La liste le garde, la zone le marque « ACTIVE ».
    let v = b.liste().await;
    assert_eq!(ids(&v), vec!["pure", "evening-listening"]);
    assert_eq!(
        b.get_zone().await["active_preset"],
        json!("evening-listening")
    );
    // Même nom : un autre identifiant, rien n'est écrasé.
    let (s, p2) = b.enregistrer("Evening listening").await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(p2["id"], json!("evening-listening-2"));
    assert_eq!(b.liste().await["presets"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn un_enregistrement_mal_forme_est_refuse_et_sa_contre_epreuve_passe_t17() {
    let b = Banc::nouveau(true).await;
    for (corps, statut, code) in [
        (
            json!({"name": "   ", "zone_id": b.zone}),
            400,
            "invalid_name",
        ),
        (
            json!({"name": "x".repeat(81), "zone_id": b.zone}),
            400,
            "invalid_name",
        ),
        (json!({"name": "A"}), 400, "invalid_body"),
        (
            json!({"name": "A", "zone_id": b.zone, "chain": {"version": 1}}),
            400,
            "invalid_body",
        ),
        (json!({"name": "A", "zone_id": 9999}), 404, "zone_not_found"),
        (json!({"name": "A", "zone": b.zone}), 400, "invalid_body"),
        // Chaîne fournie : lue strictement, comme le corps d'un PUT.
        (
            json!({"name": "A", "chain": {"version": 1, "colour": "blue"}}),
            400,
            "invalid_body",
        ),
        (
            json!({"name": "A", "chain": {"version": 1, "plugins": [{"id": "nope", "enabled": true}]}}),
            409,
            "unknown_plugin",
        ),
    ] {
        let (s, v) = b
            .brut(Method::POST, "/api/v1/tess/presets", Some(corps.clone()))
            .await;
        assert_eq!(s.as_u16(), statut, "{corps} → {v}");
        assert_eq!(v["error"], json!(code), "{corps} → {v}");
    }
    assert_eq!(ids(&b.liste().await), vec!["pure"]);
    // Contre-épreuves : nom de 80 caractères, chaîne fournie valide.
    let (s, v) = b
        .brut(
            Method::POST,
            "/api/v1/tess/presets",
            Some(json!({"name": "é".repeat(80), "zone_id": b.zone})),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, v) = b
        .brut(
            Method::POST,
            "/api/v1/tess/presets",
            Some(json!({"name": "Casque", "chain": {"version": 1, "plugins": [crossfeed()]}})),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["id"], json!("casque"));
}

#[tokio::test]
async fn renommer_garde_l_identifiant_et_supprimer_oublie_l_actif_t17() {
    let b = Banc::nouveau(true).await;
    b.put_chaine(json!({"version": 1, "plugins": [crossfeed()]}))
        .await;
    let (_, p) = b.enregistrer("Headphones").await;
    let (s, v) = b
        .brut(
            Method::PATCH,
            "/api/v1/tess/presets/headphones",
            Some(json!({"name": "Casque du soir"})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["id"], p["id"]);
    assert_eq!(v["name"], json!("Casque du soir"));
    assert_eq!(b.get_zone().await["active_preset"], json!("headphones"));
    // Refus : nom vide, préréglage inconnu.
    let (s, v) = b
        .brut(
            Method::PATCH,
            "/api/v1/tess/presets/headphones",
            Some(json!({"name": ""})),
        )
        .await;
    assert_eq!(
        (s, v["error"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_name"))
    );
    let (s, v) = b
        .brut(
            Method::PATCH,
            "/api/v1/tess/presets/nope",
            Some(json!({"name": "X"})),
        )
        .await;
    assert_eq!(
        (s, v["error"].clone()),
        (StatusCode::NOT_FOUND, json!("preset_not_found"))
    );

    let (s, _) = b
        .brut(Method::DELETE, "/api/v1/tess/presets/headphones", None)
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(ids(&b.liste().await), vec!["pure"]);
    // La zone garde sa chaîne, sans préréglage actif.
    let z = b.get_zone().await;
    assert_eq!(z["active_preset"], Value::Null);
    assert_eq!(z["chain"]["plugins"][0]["id"], json!("crossfeed"));
    let (s, v) = b
        .brut(Method::DELETE, "/api/v1/tess/presets/headphones", None)
        .await;
    assert_eq!(
        (s, v["error"].clone()),
        (StatusCode::NOT_FOUND, json!("preset_not_found"))
    );
    // Pure aussi se supprime (la croix de la maquette A2).
    let (s, _) = b
        .brut(Method::DELETE, "/api/v1/tess/presets/pure", None)
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(ids(&b.liste().await).is_empty());
}

// ── Chargement ──────────────────────────────────────────────────────

#[tokio::test]
async fn charger_remplace_la_chaine_et_marque_le_prereglage_actif_t17() {
    let b = Banc::nouveau(true).await;
    b.put_chaine(json!({"version": 1, "plugins": [crossfeed()]}))
        .await;
    b.enregistrer("Headphones").await;
    // Une autre chaîne, puis le rappel du préréglage.
    let (s, _) = b
        .put_chaine(json!({"version": 1, "plugins": [parametrique()]}))
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(
        b.get_zone().await["active_preset"],
        Value::Null,
        "PUT à la main"
    );
    let (s, v) = b.appliquer("headphones").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    conforme_au_schema("zone_response", &v["zone"]).unwrap_or_else(|e| panic!("{e} : {v}"));
    assert_eq!(v["preset_id"], json!("headphones"));
    assert_eq!(v["apply_mode"], json!("immediately"));
    assert_eq!(v["not_applied"], json!([]));
    assert_eq!(v["zone"]["active_preset"], json!("headphones"));
    let c = b.chaine();
    assert_eq!(c.plugins.len(), 1, "{:?}", c.plugins);
    assert_eq!(c.plugins[0].id, "crossfeed");
    // L'ancienne route voit le préréglage chargé (écriture croisée).
    let (_, dsp) = b
        .brut(Method::GET, &format!("/api/v1/zones/{}/dsp", b.zone), None)
        .await;
    assert_eq!(dsp["crossfeed"]["enabled"], json!(true), "{dsp}");
    assert_ne!(dsp["eq_profile"]["enabled"], json!(true), "{dsp}");

    // Le renvoyer tel quel garde l'actif ; le modifier l'oublie.
    let (s, _) = b.put_chaine(v["zone"]["chain"].clone()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b.get_zone().await["active_preset"], json!("headphones"));
    let mut autre = v["zone"]["chain"].clone();
    autre["plugins"][0]["enabled"] = json!(false);
    b.put_chaine(autre).await;
    assert_eq!(b.get_zone().await["active_preset"], Value::Null);

    // Pure : bit-perfect (volume plein, chaîne vide sous bypass).
    let (s, v) = b.appliquer("pure").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["zone"]["badge"], json!("bit_perfect"), "{v}");
    assert_eq!(v["zone"]["active_preset"], json!("pure"));
    // Préréglage inconnu : 404 qui le nomme ; la chaîne ne bouge pas.
    let avant = b.settings().get(&cle_chaine_zone(b.zone)).unwrap();
    let (s, v) = b.appliquer("nope").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(v["error"], json!("preset_not_found"));
    assert_eq!(v["preset_id"], json!("nope"));
    assert_eq!(b.settings().get(&cle_chaine_zone(b.zone)).unwrap(), avant);
}

#[tokio::test]
async fn un_prereglage_premium_ne_donne_jamais_le_droit_a_un_compte_gratuit_t17() {
    let b = Banc::nouveau(true).await;
    b.put_chaine(json!({"version": 1, "plugins": [parametrique(), crossfeed()]}))
        .await;
    b.enregistrer("Studio").await;
    // Un marqueur VALIDE sur la zone : « Casque » décrit sa chaîne.
    b.put_chaine(json!({"version": 1, "plugins": [crossfeed()]}))
        .await;
    b.enregistrer("Casque").await;
    assert_eq!(b.get_zone().await["active_preset"], json!("casque"));
    b.premium(false).await;
    let avant = b.settings().get(&cle_chaine_zone(b.zone)).unwrap();
    let (s, v) = b.appliquer("studio").await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{v}");
    conforme_au_schema("refusal", &v).unwrap_or_else(|e| panic!("{e} : {v}"));
    assert_eq!(v["code"], json!("premium_required"));
    assert_eq!(v["plugin_id"], json!("equalizer-parametric"));
    assert!(v["upgrade_url"].is_string());
    assert!(!v["message"].as_str().unwrap().is_empty());
    // Rien n'a bougé : ni la chaîne, ni le marqueur d'origine (toujours valide).
    assert_eq!(b.settings().get(&cle_chaine_zone(b.zone)).unwrap(), avant);
    assert_eq!(b.get_zone().await["active_preset"], json!("casque"));
    // Idem sur un refus 409 (version de chaîne trop récente dans le préréglage).
    let mut l = tune_core::tess::prereglages::lire(&b.settings()).unwrap();
    let mut futur = l.get("casque").unwrap().clone();
    futur.id = "futur".into();
    futur.chain.version = 99;
    l.presets.push(futur);
    tune_core::tess::prereglages::ecrire(&b.settings(), &l).unwrap();
    let (s, v) = b.appliquer("futur").await;
    assert!(s.is_client_error(), "{s} {v}");
    assert_eq!(b.settings().get(&cle_chaine_zone(b.zone)).unwrap(), avant);
    assert_eq!(b.get_zone().await["active_preset"], json!("casque"));
    // Le préréglage reste enregistré (le droit se vérifie au chargement).
    assert!(ids(&b.liste().await).contains(&"studio".to_owned()));
    // Contre-épreuve : avec le Premium, le même chargement passe.
    b.premium(true).await;
    let (s, v) = b.appliquer("studio").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(b.chaine().plugins[0].id, "equalizer-parametric");
}

#[tokio::test]
async fn un_etage_sans_effet_sur_cette_sortie_est_dit_avec_son_motif_t17() {
    let b = Banc::avec_sortie(true, "hqplayer", "hqplayer:x", 0).await;
    let (s, p) = b
        .brut(
            Method::POST,
            "/api/v1/tess/presets",
            Some(json!({"name": "Casque", "chain": {"version": 1, "plugins": [crossfeed()]}})),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{p}");
    let (s, v) = b.appliquer("casque").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["zone"]["applicability"]["status"], json!("outside_tess"));
    assert_eq!(
        v["not_applied"],
        json!([{"plugin_id": "crossfeed", "reason": "outside_tess",
                "detail": v["not_applied"][0]["detail"].clone()}]),
        "{v}"
    );
    // Greffon désinstallé sur une zone locale : motif propre.
    drop(b);
    let b = Banc::nouveau(true).await;
    b.brut(
        Method::POST,
        "/api/v1/tess/presets",
        Some(json!({"name": "Casque", "chain": {"version": 1, "plugins": [crossfeed()]}})),
    )
    .await;
    let (_, v) = b.appliquer("casque").await;
    assert_eq!(v["not_applied"], json!([]), "contre-épreuve : {v}");
    b.settings()
        .set("plugin_crossfeed_enabled", "false")
        .unwrap();
    let (s, v) = b.appliquer("casque").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        v["not_applied"][0]["reason"],
        json!("plugin_unavailable"),
        "{v}"
    );
}

// ── Mode d'application ──────────────────────────────────────────────

#[tokio::test]
async fn le_mode_se_regle_et_une_valeur_inconnue_est_refusee_t17() {
    let b = Banc::nouveau(true).await;
    for faux in [
        json!({"apply_mode": "crossfade"}),
        json!({}),
        json!({"apply_mode": "immediately", "x": 1}),
    ] {
        let (s, v) = b
            .brut(Method::PUT, "/api/v1/tess/apply-mode", Some(faux.clone()))
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{faux} → {v}");
        assert_eq!(v["error"], json!("invalid_apply_mode"));
    }
    assert_eq!(b.liste().await["apply_mode"], json!("immediately"));
    let (s, v) = b
        .brut(
            Method::PUT,
            "/api/v1/tess/apply-mode",
            Some(json!({"apply_mode": "at_next_playback"})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    conforme_au_schema("presets", &v).unwrap();
    assert_eq!(b.liste().await["apply_mode"], json!("at_next_playback"));
}

#[tokio::test]
async fn au_prochain_demarrage_rien_n_interrompt_la_lecture_t17() {
    // Numéro de zone propre au test : l'attente est tenue par zone.
    let b = Banc::avec_sortie(true, "local", "local:Realtek HD", 40).await;
    b.brut(
        Method::PUT,
        "/api/v1/tess/apply-mode",
        Some(json!({"apply_mode": "at_next_playback"})),
    )
    .await;
    let (s, v) = b
        .put_chaine(json!({"version": 1, "plugins": [crossfeed()]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // Les anciennes clés sont écrites (la prochaine lecture part avec)…
    let (_, dsp) = b
        .brut(Method::GET, &format!("/api/v1/zones/{}/dsp", b.zone), None)
        .await;
    assert_eq!(dsp["crossfeed"]["enabled"], json!(true), "{dsp}");
    // … mais la sortie vivante n'est pas touchée : le rafraîchissement attend.
    let attente = cles_en_attente(b.zone);
    assert!(
        attente
            .iter()
            .any(|c| c == &format!("zone_{}_crossfeed", b.zone)),
        "{attente:?}"
    );
    // Un préréglage chargé dans ce mode attend aussi, et le dit.
    b.enregistrer("Casque").await;
    let (s, v) = b.appliquer("casque").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["apply_mode"], json!("at_next_playback"));
    // Le démarrage de la lecture l'applique, une fois.
    appliquer_a_la_lecture(&b.state, b.zone).await;
    assert!(cles_en_attente(b.zone).is_empty());

    // Contre-épreuve : en mode immédiat, rien n'attend (et ce qui attendait
    // est servi avec).
    b.put_chaine(json!({"version": 1, "plugins": []})).await;
    assert!(!cles_en_attente(b.zone).is_empty());
    b.brut(
        Method::PUT,
        "/api/v1/tess/apply-mode",
        Some(json!({"apply_mode": "immediately"})),
    )
    .await;
    b.put_chaine(json!({"version": 1, "plugins": [crossfeed()]}))
        .await;
    assert!(cles_en_attente(b.zone).is_empty());
}

// ── Suites de relecture (T17) ───────────────────────────────────────

async fn mode(b: &Banc, m: &str) {
    let (s, v) = b
        .brut(
            Method::PUT,
            "/api/v1/tess/apply-mode",
            Some(json!({"apply_mode": m})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

#[tokio::test]
async fn la_lecture_attend_le_put_en_cours_de_la_zone_suite_t17() {
    let b = Banc::avec_sortie(true, "local", "local:Realtek HD", 50).await;
    mode(&b, "at_next_playback").await;
    b.put_chaine(json!({"version": 1, "plugins": [crossfeed()]}))
        .await;
    assert!(!cles_en_attente(b.zone).is_empty());
    // Un PUT tient la zone : le démarrage de lecture attend, la file reste.
    let verrou = verrou_des_put(b.zone);
    let garde = verrou.lock().await;
    let state = b.state.clone();
    let zone = b.zone;
    let tache = tokio::spawn(async move { appliquer_a_la_lecture(&state, zone).await });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !cles_en_attente(b.zone).is_empty(),
        "la file a été lue pendant le PUT"
    );
    drop(garde);
    tache.await.unwrap();
    assert!(cles_en_attente(b.zone).is_empty());
}

#[tokio::test]
async fn une_zone_reseau_sans_appareil_n_est_pas_locale_suite_t17() {
    let b = Banc::nouveau(true).await;
    let repo = ZoneRepo::with_backend(b.state.backend.clone());
    let reseau = repo.create("Salon DLNA", Some("dlna"), None).unwrap();
    assert!(!sortie_locale(&repo.get(reseau).unwrap().unwrap()));
    // Contre-épreuves : locale déclarée, ou type absent (locale par défaut).
    assert!(sortie_locale(&repo.get(b.zone).unwrap().unwrap()));
    let sans_type = repo.create("Sans type", None, None).unwrap();
    assert!(sortie_locale(&repo.get(sans_type).unwrap().unwrap()));
}

#[tokio::test]
async fn repasser_en_immediat_vide_la_file_suite_t17() {
    let b = Banc::avec_sortie(true, "local", "local:Realtek HD", 60).await;
    mode(&b, "at_next_playback").await;
    b.put_chaine(json!({"version": 1, "plugins": [crossfeed()]}))
        .await;
    assert!(!cles_en_attente(b.zone).is_empty());
    // Contre-épreuve : reposer le même mode ne sert rien.
    mode(&b, "at_next_playback").await;
    assert!(!cles_en_attente(b.zone).is_empty());
    mode(&b, "immediately").await;
    assert!(cles_en_attente(b.zone).is_empty());
}

#[tokio::test]
async fn charger_lit_la_liste_sous_son_verrou_suite_t17() {
    let b = Banc::nouveau(true).await;
    b.put_chaine(json!({"version": 1, "plugins": [crossfeed()]}))
        .await;
    b.enregistrer("Headphones").await;
    let app = b.app.clone();
    let uri = format!("/api/v1/zones/{}/tess/presets/headphones/apply", b.zone);
    let garde = verrou_de_la_liste().lock().await;
    let tache = tokio::spawn(async move {
        app.oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !tache.is_finished(),
        "chargé pendant qu'on modifiait la liste"
    );
    drop(garde);
    assert_eq!(tache.await.unwrap(), StatusCode::OK);
}

#[tokio::test]
async fn un_put_identique_sur_un_compte_gratuit_garde_l_actif_suite_t17() {
    let b = Banc::nouveau(true).await;
    b.put_chaine(json!({"version": 1, "plugins": [parametrique()]}))
        .await;
    b.enregistrer("Studio").await;
    b.premium(false).await;
    // Le GET gratuit montre le paramétrique converti en graphique (P2).
    let vue = b.get_zone().await;
    assert_eq!(
        vue["chain"]["plugins"][0]["id"],
        json!("equalizer-graphic"),
        "{vue}"
    );
    assert_eq!(vue["active_preset"], json!("studio"));
    // Renvoyée telle quelle, la chaîne ne change pas : l'actif reste.
    let (s, v) = b.put_chaine(vue["chain"].clone()).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(b.chaine().plugins[0].id, "equalizer-parametric");
    assert_eq!(b.get_zone().await["active_preset"], json!("studio"));
    // Contre-épreuve : décocher l'oublie.
    let mut autre = vue["chain"].clone();
    autre["plugins"][0]["enabled"] = json!(false);
    b.put_chaine(autre).await;
    assert_eq!(b.get_zone().await["active_preset"], Value::Null);
}

// ── Retouches de relecture (T17) ────────────────────────────────────

#[tokio::test]
async fn une_suppression_pendant_un_chargement_ne_laisse_pas_de_marqueur_orphelin_suite_t17() {
    let b = Banc::nouveau(true).await;
    b.put_chaine(json!({"version": 1, "plugins": [crossfeed()]}))
        .await;
    b.enregistrer("Headphones").await;
    b.put_chaine(json!({"version": 1, "plugins": []})).await;
    assert_eq!(b.get_zone().await["active_preset"], Value::Null);
    // Un PUT tient la zone : le chargement attend après avoir lu la liste.
    let verrou = verrou_des_put(b.zone);
    let garde = verrou.lock().await;
    let (app1, app2) = (b.app.clone(), b.app.clone());
    let uri = format!("/api/v1/zones/{}/tess/presets/headphones/apply", b.zone);
    let charger = tokio::spawn(async move {
        app1.oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
    });
    // Le chargement tient la liste (et attend la zone).
    attendre_que_la_liste_soit_tenue().await;
    let supprimer = tokio::spawn(async move {
        app2.oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/api/v1/tess/presets/headphones")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
    });
    // La suppression attend la liste : rien d'observable, on lui laisse le
    // temps de se mettre en file.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    drop(garde);
    assert_eq!(charger.await.unwrap(), StatusCode::OK);
    assert_eq!(supprimer.await.unwrap(), StatusCode::NO_CONTENT);
    // Le préréglage n'existe plus : aucune zone ne doit le marquer.
    assert!(!ids(&b.liste().await).contains(&"headphones".to_owned()));
    assert_eq!(b.get_zone().await["active_preset"], Value::Null);
}

#[tokio::test]
async fn passer_en_immediat_rafraichit_aussi_une_zone_reseau_suite_t17() {
    let b = Banc::avec_sortie(true, "dlna", "dlna:salon", 70).await;
    mode(&b, "at_next_playback").await;
    b.put_chaine(json!({"version": 1, "plugins": [crossfeed()]}))
        .await;
    assert!(!cles_en_attente(b.zone).is_empty());
    // Contre-épreuve : au démarrage d'une lecture, une zone réseau n'est pas
    // rafraîchie (le flux vient d'être fabriqué avec les clés).
    let avant = rafraichissements_demandes(b.zone);
    appliquer_a_la_lecture(&b.state, b.zone).await;
    assert_eq!(rafraichissements_demandes(b.zone), avant);
    // Le passage en immédiat, lui, DEMANDE le rafraîchissement de la sortie
    // réseau (ce témoin compte les demandes ; ce qu'elles produisent sur un
    // flux réseau, relance par l'égaliseur, relève des tests de l'orchestrateur).
    b.put_chaine(json!({"version": 1, "plugins": []})).await;
    assert!(!cles_en_attente(b.zone).is_empty());
    mode(&b, "immediately").await;
    assert!(cles_en_attente(b.zone).is_empty());
    assert_eq!(rafraichissements_demandes(b.zone), avant + 1);
}

#[tokio::test]
async fn au_dela_de_deux_cents_l_enregistrement_est_refuse_suite_t17() {
    let b = Banc::nouveau(true).await;
    let mut l = tune_core::tess::PreReglages::default();
    for i in 1..tune_core::tess::prereglages::PREREGLAGES_MAX {
        l.presets.push(tune_core::tess::PreReglage {
            id: format!("p{i}"),
            name: format!("P{i}"),
            builtin: false,
            chain: ChaineTess::default(),
        });
    }
    tune_core::tess::prereglages::ecrire(&b.settings(), &l).unwrap();
    let (s, v) = b.enregistrer("Un de trop").await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], json!("too_many_presets"));
    // Contre-épreuve : une place libérée, l'enregistrement passe.
    b.brut(Method::DELETE, "/api/v1/tess/presets/p1", None)
        .await;
    let (s, v) = b.enregistrer("Un de trop").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
}

#[tokio::test]
async fn une_entree_stockee_trop_longue_ne_fait_pas_tomber_les_routes_suite_t17() {
    let b = Banc::nouveau(true).await;
    let corps = json!({"apply_mode": "immediately", "presets": [
        {"id": "pure", "name": "Pure — no processing", "builtin": true,
         "chain": {"version": 1, "bypass_all": true}},
        {"id": "long", "name": "x".repeat(81), "chain": {"version": 1}},
        {"id": "casque", "name": "Casque", "chain": {"version": 1}}
    ]});
    b.settings()
        .set("tess_presets", &corps.to_string())
        .unwrap();
    // Rendue telle quelle (rien n'est perdu) ; les routes servent le reste.
    assert_eq!(ids(&b.liste().await), vec!["pure", "long", "casque"]);
    let (s, _) = b.appliquer("casque").await;
    assert_eq!(s, StatusCode::OK);
}

/// L'entrée « long » de la base, brute.
fn entree_brute(b: &Banc, id: &str) -> Option<Value> {
    let brut: Value =
        serde_json::from_str(&b.settings().get("tess_presets").unwrap().unwrap()).unwrap();
    brut["presets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == json!(id))
        .cloned()
}

#[tokio::test]
async fn une_entree_invalide_en_base_survit_aux_ecritures_des_routes_suite_t17() {
    let b = Banc::nouveau(true).await;
    let long = "x".repeat(81);
    let corps = json!({"apply_mode": "immediately", "presets": [
        {"id": "long", "name": long, "chain": {"version": 1}},
        {"id": "casque", "name": "Casque", "chain": {"version": 1}}
    ]});
    b.settings()
        .set("tess_presets", &corps.to_string())
        .unwrap();
    // Enregistrer un AUTRE préréglage, en renommer un, changer de mode…
    let (s, v) = b.enregistrer("Soirée").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, _) = b
        .brut(
            Method::PATCH,
            "/api/v1/tess/presets/casque",
            Some(json!({"name": "Casque du soir"})),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    mode(&b, "at_next_playback").await;
    let (s, _) = b
        .brut(Method::DELETE, "/api/v1/tess/presets/soiree", None)
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // … et l'entrée invalide est toujours en base, intacte.
    let e = entree_brute(&b, "long").expect("entrée invalide perdue");
    assert_eq!(e["name"], json!(long));
    // Contre-épreuve : on ne peut pas en CRÉER une pareille.
    let (s, v) = b.enregistrer(&"y".repeat(81)).await;
    assert_eq!(
        (s, v["error"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_name"))
    );
}

/// Fait échouer, en base, l'écriture (`INSERT`/`UPDATE`) et/ou l'effacement
/// (`DELETE`) des marqueurs « ACTIVE » dont la clé correspond à `motif`.
fn bloquer_le_marqueur(b: &Banc, motif: &str, ecriture: bool, effacement: bool) {
    use tune_core::db::backend::DbBackend;
    let mut sql = String::new();
    if ecriture {
        sql += &format!(
            "CREATE TRIGGER t17_ins BEFORE INSERT ON settings WHEN NEW.key LIKE '{motif}' \
             BEGIN SELECT RAISE(ABORT, 'refus de test'); END; \
             CREATE TRIGGER t17_upd BEFORE UPDATE ON settings WHEN NEW.key LIKE '{motif}' \
             BEGIN SELECT RAISE(ABORT, 'refus de test'); END;"
        );
    }
    if effacement {
        sql += &format!(
            "CREATE TRIGGER t17_del BEFORE DELETE ON settings WHEN OLD.key LIKE '{motif}' \
             BEGIN SELECT RAISE(ABORT, 'refus de test'); END;"
        );
    }
    b.state.backend.execute_batch(&sql).unwrap();
}

const MARQUEURS: &str = "zone_%_tess_active_preset";

#[tokio::test]
async fn un_marqueur_non_ecrit_au_chargement_n_empeche_pas_la_chaine_suite_t17() {
    let b = Banc::nouveau(true).await;
    b.put_chaine(json!({"version": 1, "plugins": [crossfeed()]}))
        .await;
    b.enregistrer("Headphones").await;
    b.put_chaine(json!({"version": 1, "plugins": [parametrique()]}))
        .await;
    b.enregistrer("Studio").await;
    assert_eq!(b.get_zone().await["active_preset"], json!("studio"));
    bloquer_le_marqueur(&b, MARQUEURS, true, false);
    let (s, v) = b.appliquer("headphones").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["warnings"], json!(["active_preset_not_stored"]), "{v}");
    // La chaîne est appliquée et servie…
    assert_eq!(
        b.chaine()
            .plugins
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        vec!["crossfeed"]
    );
    assert_eq!(
        b.get_zone().await["chain"]["plugins"][0]["id"],
        json!("crossfeed")
    );
    // … et aucun marqueur périmé (« studio » ne décrit plus la zone).
    assert_eq!(b.get_zone().await["active_preset"], Value::Null);
}

#[tokio::test]
async fn un_marqueur_non_efface_au_put_n_empeche_pas_la_chaine_suite_t17() {
    let b = Banc::nouveau(true).await;
    b.put_chaine(json!({"version": 1, "plugins": [crossfeed()]}))
        .await;
    b.enregistrer("Headphones").await;
    bloquer_le_marqueur(&b, MARQUEURS, false, true);
    let (s, v) = b
        .put_chaine(json!({"version": 1, "plugins": [parametrique()]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    conforme_au_schema("zone_response", &v).unwrap_or_else(|e| panic!("{e} : {v}"));
    assert_eq!(
        v["chain"]["plugins"][0]["id"],
        json!("equalizer-parametric")
    );
    // Effacé au mieux (valeur vide) : aucun marqueur périmé.
    assert_eq!(v["active_preset"], Value::Null, "{v}");
    assert_eq!(b.get_zone().await["active_preset"], Value::Null);
}

#[tokio::test]
async fn un_marqueur_non_ecrit_a_l_enregistrement_ne_cree_pas_de_doublon_suite_t17() {
    let b = Banc::nouveau(true).await;
    b.put_chaine(json!({"version": 1, "plugins": [crossfeed()]}))
        .await;
    bloquer_le_marqueur(&b, MARQUEURS, true, false);
    let (s, v) = b.enregistrer("Headphones").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["id"], json!("headphones"));
    assert_eq!(v["warnings"], json!(["active_preset_not_stored"]));
    assert_eq!(ids(&b.liste().await), vec!["pure", "headphones"]);
    assert_eq!(b.get_zone().await["active_preset"], Value::Null);
}

#[tokio::test]
async fn une_suppression_continue_si_un_marqueur_resiste_suite_t17() {
    let b = Banc::nouveau(true).await;
    let autre = ZoneRepo::with_backend(b.state.backend.clone())
        .create("Autre", Some("local"), Some("local:autre"))
        .unwrap();
    b.put_chaine(json!({"version": 1, "plugins": [crossfeed()]}))
        .await;
    b.enregistrer("Headphones").await;
    b.settings()
        .set(&format!("zone_{autre}_tess_active_preset"), "headphones")
        .unwrap();
    // Le marqueur de la PREMIÈRE zone ne se laisse ni effacer ni vider.
    bloquer_le_marqueur(
        &b,
        &format!("zone_{}_tess_active_preset", b.zone),
        true,
        true,
    );
    let (s, v) = b
        .brut(Method::DELETE, "/api/v1/tess/presets/headphones", None)
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    assert!(!ids(&b.liste().await).contains(&"headphones".to_owned()));
    // L'autre zone est quand même nettoyée.
    assert_eq!(
        b.settings()
            .get(&format!("zone_{autre}_tess_active_preset"))
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn enregistrer_lit_la_chaine_sous_le_verrou_de_la_zone_suite_t17() {
    let b = Banc::nouveau(true).await;
    b.put_chaine(json!({"version": 1, "plugins": []})).await;
    // Un PUT tient la zone…
    let verrou = verrou_des_put(b.zone);
    let garde = verrou.lock().await;
    let app = b.app.clone();
    let zone = b.zone;
    let enregistrer = tokio::spawn(async move {
        let r = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/tess/presets")
                    .header("Content-Type", "application/json")
                    .body(Body::from(
                        json!({"name": "Casque", "zone_id": zone}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(r.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice::<Value>(&bytes).unwrap()
    });
    attendre_que_la_liste_soit_tenue().await;
    // … et écrit sa chaîne pendant que l'enregistrement attend.
    let nouvelle: ChaineTess =
        serde_json::from_value(json!({"version": 1, "plugins": [crossfeed()]})).unwrap();
    tune_core::tess::migration::ecrire_chaine(&b.settings(), b.zone, &nouvelle).unwrap();
    drop(garde);
    let p = enregistrer.await.unwrap();
    // Le préréglage garde la chaîne ÉCRITE par le PUT, et la marque active.
    assert_eq!(p["chain"]["plugins"][0]["id"], json!("crossfeed"), "{p}");
    assert_eq!(b.get_zone().await["active_preset"], json!("casque"));
}

/// Attend qu'une requête tienne le verrou de la liste.
async fn attendre_que_la_liste_soit_tenue() {
    for _ in 0..500 {
        if verrou_de_la_liste().try_lock().is_err() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    panic!("la liste n'a jamais été tenue");
}

#[tokio::test]
async fn un_marqueur_ni_ecrit_ni_efface_refuse_le_chargement_sans_rien_changer_suite_t17() {
    let b = Banc::nouveau(true).await;
    b.put_chaine(json!({"version": 1, "plugins": [crossfeed()]}))
        .await;
    b.enregistrer("Headphones").await;
    b.put_chaine(json!({"version": 1, "plugins": [parametrique()]}))
        .await;
    b.enregistrer("Studio").await;
    let avant = b.settings().get(&cle_chaine_zone(b.zone)).unwrap();
    bloquer_le_marqueur(&b, MARQUEURS, true, true);
    let (s, v) = b.appliquer("headphones").await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    assert_eq!(v["error"], json!("storage_error"));
    // Rien n'a bougé : la chaîne, et le marqueur qui la décrit toujours.
    assert_eq!(b.settings().get(&cle_chaine_zone(b.zone)).unwrap(), avant);
    assert_eq!(b.chaine().plugins[0].id, "equalizer-parametric");
    assert_eq!(b.get_zone().await["active_preset"], json!("studio"));
}
