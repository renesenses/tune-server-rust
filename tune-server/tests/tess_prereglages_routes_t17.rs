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
    appliquer_a_la_lecture, cles_en_attente, sortie_locale, verrou_de_la_liste,
};

struct Banc {
    state: tune_server::state::AppState,
    app: axum::Router,
    zone: i64,
}

impl Banc {
    async fn nouveau(premium: bool) -> Self {
        Self::avec_sortie(premium, "local", "local:Realtek HD", 0).await
    }

    /// `avant` zones créées d'abord : la zone du banc a un numéro propre
    /// (l'attente du mode `at_next_playback` est tenue par numéro de zone).
    async fn avec_sortie(premium: bool, output_type: &str, device: &str, avant: usize) -> Self {
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
        Self { state, app, zone }
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
    b.put_chaine(json!({"version": 1, "plugins": []})).await;
    b.premium(false).await;
    let avant = b.settings().get(&cle_chaine_zone(b.zone)).unwrap();
    let (s, v) = b.appliquer("studio").await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{v}");
    conforme_au_schema("refusal", &v).unwrap_or_else(|e| panic!("{e} : {v}"));
    assert_eq!(v["code"], json!("premium_required"));
    assert_eq!(v["plugin_id"], json!("equalizer-parametric"));
    assert!(v["upgrade_url"].is_string());
    assert!(!v["message"].as_str().unwrap().is_empty());
    // Rien n'a bougé : ni la chaîne, ni l'actif.
    assert_eq!(b.settings().get(&cle_chaine_zone(b.zone)).unwrap(), avant);
    assert_eq!(b.get_zone().await["active_preset"], Value::Null);
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
