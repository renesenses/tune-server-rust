//! rc14 — l'égaliseur paramétrique est Premium sur TOUTES les routes qui
//! écrivent `zone_{id}_eq_profile`, comme par `PUT /zones/{id}/tess` :
//! `PUT /zones/{id}/dsp`, `POST /zones/{id}/eq` et
//! `POST /eq/presets/{id}/activate`. Le graphique et le casque (AutoEQ)
//! restent libres ; une zone qui porte déjà un paramétrique n'est bloquée
//! dans aucun autre geste.
//!
//! Chaque refus a sa contre-épreuve : la même requête passe avec le droit.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! par sa cible `[[test]]` dans `tune-server/Cargo.toml`.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

struct Banc {
    state: tune_server::state::AppState,
    app: axum::Router,
    zone: i64,
}

impl Banc {
    async fn nouveau(premium: bool) -> Self {
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
            .create("Zone", Some("local"), Some("local:Realtek HD"))
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

    async fn appel(&self, methode: Method, uri: &str, corps: Option<Value>) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(methode)
            .uri(uri)
            .header("Content-Type", "application/json")
            .header("Accept-Language", "fr")
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

    async fn dsp(&self, corps: Value) -> (StatusCode, Value) {
        let uri = format!("/api/v1/zones/{}/dsp", self.zone);
        self.appel(Method::PUT, &uri, Some(corps)).await
    }

    async fn eq(&self, corps: Value) -> (StatusCode, Value) {
        let uri = format!("/api/v1/zones/{}/eq", self.zone);
        self.appel(Method::POST, &uri, Some(corps)).await
    }

    /// Crée un préréglage (route gratuite) et rend son id.
    async fn prereglage(&self, eq_type: &str, bandes: Value) -> String {
        let (s, v) = self
            .appel(
                Method::POST,
                "/api/v1/eq/presets",
                Some(json!({"name": "Essai", "eq_type": eq_type, "bands": bandes})),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        v["id"].as_str().unwrap().to_owned()
    }

    async fn activer(&self, id: &str) -> (StatusCode, Value) {
        let uri = format!("/api/v1/eq/presets/{id}/activate?zone_id={}", self.zone);
        self.appel(Method::POST, &uri, None).await
    }

    fn profil_enregistre(&self) -> Option<String> {
        self.settings()
            .get(&format!("zone_{}_eq_profile", self.zone))
            .unwrap()
    }
}

fn bandes_parametriques() -> Value {
    json!([
        {"freq": 80.0, "gain": 4.0, "q": 0.7, "type": "low_shelf"},
        {"freq": 1200.0, "gain": -3.5, "q": 1.4, "type": "peak"}
    ])
}

/// Une courbe de l'égaliseur graphique : la grille ISO à 10 bandes, Q 1.
fn bandes_graphiques() -> Value {
    let gains = [3.0, 2.0, 0.0, -1.0, 0.0, 1.0, 2.0, 0.0, -2.0, 1.0];
    Value::Array(
        tune_core::audio::eq_presets::GRILLE_10
            .iter()
            .zip(gains)
            .map(|(f, g)| json!({"freq": f, "gain": g, "q": 1.0, "type": "peak"}))
            .collect(),
    )
}

fn profil(bandes: Value, enabled: bool) -> Value {
    json!({
        "enabled": enabled,
        "listening": "speakers", "room_size": "medium",
        "speaker_placement": "free_standing", "bass_gain_db": 0.0,
        "mid_gain_db": 0.0, "treble_gain_db": 0.0, "bands": bandes
    })
}

/// Le refus des autres routes Premium (`premium_guard`), droit nommé.
fn refus_parametrique(s: StatusCode, v: &Value) {
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{v}");
    assert_eq!(v["error"], json!("premium_required"), "{v}");
    assert_eq!(v["code"], json!("parametric_eq"), "{v}");
    assert_eq!(v["feature"], json!("Parametric EQ"), "{v}");
    assert!(v["upgrade_url"].is_string(), "{v}");
    assert!(!v["message"].as_str().unwrap_or_default().is_empty(), "{v}");
}

#[tokio::test]
async fn put_dsp_parametrique_402_sans_droit_200_avec() {
    let b = Banc::nouveau(false).await;
    let corps = json!({"eq_profile": profil(bandes_parametriques(), true)});
    let (s, v) = b.dsp(corps.clone()).await;
    refus_parametrique(s, &v);
    assert!(b.profil_enregistre().is_none(), "rien n'est écrit");
    // Un corps mixte n'écrit rien non plus : ni crossfeed ni égaliseur.
    let mut mixte = corps.clone();
    mixte["crossfeed"] = json!({"enabled": true, "amount": 0.3, "delay_ms": 0.3});
    let (s, v) = b.dsp(mixte).await;
    refus_parametrique(s, &v);
    assert!(
        b.settings()
            .get(&format!("zone_{}_crossfeed", b.zone))
            .unwrap()
            .is_none()
    );
    b.premium(true).await;
    let (s, v) = b.dsp(corps).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(b.profil_enregistre().is_some());
}

#[tokio::test]
async fn post_eq_parametrique_402_sans_droit_200_avec() {
    let b = Banc::nouveau(false).await;
    let corps = json!({"bands": bandes_parametriques(), "enabled": true});
    let (s, v) = b.eq(corps.clone()).await;
    refus_parametrique(s, &v);
    assert!(b.profil_enregistre().is_none(), "rien n'est écrit");
    b.premium(true).await;
    let (s, v) = b.eq(corps).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(b.profil_enregistre().is_some());
}

#[tokio::test]
async fn activer_un_prereglage_parametrique_402_sans_droit_200_avec() {
    let b = Banc::nouveau(false).await;
    let id = b.prereglage("parametric", bandes_parametriques()).await;
    let (s, v) = b.activer(&id).await;
    refus_parametrique(s, &v);
    assert!(b.profil_enregistre().is_none(), "rien n'est écrit");
    assert!(b.settings().get("eq_active_preset").unwrap().is_none());
    b.premium(true).await;
    let (s, v) = b.activer(&id).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["activated"], json!(true));
}

#[tokio::test]
async fn le_graphique_reste_libre_sur_les_trois_routes() {
    let b = Banc::nouveau(false).await;
    let (s, v) = b
        .dsp(json!({"eq_profile": profil(bandes_graphiques(), true)}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = b
        .eq(json!({"bands": bandes_graphiques(), "enabled": true}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // Préréglage nommé (écran « En cours de lecture ») et curseurs seuls.
    let (s, v) = b.eq(json!({"preset": "rock"})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = b.dsp(json!({"eq_profile": profil(json!([]), true)})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let id = b.prereglage("graphic", bandes_graphiques()).await;
    let (s, v) = b.activer(&id).await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

#[tokio::test]
async fn le_casque_autoeq_reste_libre() {
    let b = Banc::nouveau(false).await;
    let texte = "Preamp: -6.4 dB\n\
                 Filter 1: ON LSC Fc 105 Hz Gain 6.4 dB Q 0.70\n\
                 Filter 2: ON PK Fc 8800 Hz Gain 5.1 dB Q 1.42\n";
    let (s, v) = b
        .appel(
            Method::POST,
            "/api/v1/eq/import/autoeq",
            Some(json!({"text": texte, "name": "HD 650"})),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let preset = v["preset"].clone();
    let id = preset["id"].as_str().unwrap();
    let (s, v) = b.activer(id).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // Le client web applique le même préréglage par `POST /zones/{id}/eq`.
    let (s, v) = b
        .eq(json!({"bands": preset["bands"], "enabled": true}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // Contre-épreuve : une bande retouchée n'est plus le profil du casque.
    let mut retouchees = preset["bands"].clone();
    retouchees[0]["gain"] = json!(2.0);
    let (s, v) = b.eq(json!({"bands": retouchees, "enabled": true})).await;
    refus_parametrique(s, &v);
}

/// Licence expirée, migration : le paramétrique déjà enregistré n'est ni
/// réécrit ni bloquant. La zone se lit, se règle (réserve, crossfeed),
/// s'éteint ; seul un nouveau réglage paramétrique, ou le rallumer, demande
/// le droit.
#[tokio::test]
async fn une_zone_qui_a_deja_un_parametrique_n_est_pas_bloquee() {
    let b = Banc::nouveau(true).await;
    let (s, v) = b
        .eq(json!({"bands": bandes_parametriques(), "enabled": true}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    b.premium(false).await;

    let (s, v) = b
        .appel(Method::GET, &format!("/api/v1/zones/{}/tess", b.zone), None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = b
        .appel(Method::GET, &format!("/api/v1/zones/{}/eq", b.zone), None)
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["bands"].as_array().unwrap().len(), 2);

    let (s, v) = b.eq(json!({"headroom_mode": "realistic"})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = b
        .dsp(json!({"crossfeed": {"enabled": true, "amount": 0.3, "delay_ms": 0.3}}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = b
        .dsp(json!({"eq_profile": profil(bandes_parametriques(), true)}))
        .await;
    assert_eq!(s, StatusCode::OK, "mêmes bandes renvoyées : {v}");
    let (s, v) = b.eq(json!({"enabled": false})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let enregistre = b.profil_enregistre().unwrap();
    assert!(enregistre.contains("1200"), "bandes gardées : {enregistre}");

    // Le rallumer, ou en changer les bandes, demande le droit.
    let (s, v) = b.eq(json!({"enabled": true})).await;
    refus_parametrique(s, &v);
    let mut autres = bandes_parametriques();
    autres[1]["gain"] = json!(-6.0);
    let (s, v) = b.eq(json!({"bands": autres, "enabled": false})).await;
    refus_parametrique(s, &v);
    // Passer au graphique reste libre.
    let (s, v) = b
        .eq(json!({"bands": bandes_graphiques(), "enabled": true}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

/// TESS : un greffon Premium décoché ne reçoit pas de nouveaux réglages en
/// Free ; ajouté sans réglages, il reste libre.
#[tokio::test]
async fn tess_reglages_d_un_greffon_premium_decoche() {
    let b = Banc::nouveau(false).await;
    let uri = format!("/api/v1/zones/{}/tess", b.zone);
    let (s, v) = b
        .appel(
            Method::PUT,
            &uri,
            Some(json!({"version": 1, "plugins": [
                {"id": "equalizer-parametric", "enabled": false, "settings": null}
            ]})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let mut reglages = profil(bandes_parametriques(), true);
    reglages.as_object_mut().unwrap().remove("enabled");
    let corps = json!({"version": 1, "plugins": [
        {"id": "equalizer-parametric", "enabled": false, "settings": reglages}
    ]});
    let (s, v) = b.appel(Method::PUT, &uri, Some(corps.clone())).await;
    assert_eq!(s, StatusCode::PAYMENT_REQUIRED, "{v}");
    assert_eq!(v["plugin_id"], json!("equalizer-parametric"), "{v}");
    b.premium(true).await;
    let (s, v) = b.appel(Method::PUT, &uri, Some(corps.clone())).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // Renvoyés tels quels sans le droit : libres.
    b.premium(false).await;
    let (s, v) = b.appel(Method::GET, &uri, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let chaine = v["chain"].clone();
    let (s, v) = b.appel(Method::PUT, &uri, Some(chaine)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

/// Un préréglage AutoEQ dont les bandes sont remplacées par
/// `PUT /eq/presets/{id}` n'est plus un égaliseur casque : l'activer en Free
/// demande le droit. Le renommer seul ne change rien.
#[tokio::test]
async fn un_prereglage_autoeq_modifie_n_est_plus_un_casque() {
    let b = Banc::nouveau(false).await;
    let texte = "Preamp: -6.4 dB\n\
                 Filter 1: ON LSC Fc 105 Hz Gain 6.4 dB Q 0.70\n\
                 Filter 2: ON PK Fc 8800 Hz Gain 5.1 dB Q 1.42\n";
    let (s, v) = b
        .appel(
            Method::POST,
            "/api/v1/eq/import/autoeq",
            Some(json!({"text": texte, "name": "HD 650"})),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let preset = v["preset"].clone();
    let id = preset["id"].as_str().unwrap().to_owned();
    let uri = format!("/api/v1/eq/presets/{id}");

    // Renommé, mêmes bandes : toujours un casque.
    let (s, v) = b
        .appel(
            Method::PUT,
            &uri,
            Some(json!({"name": "HD 650 bis", "bands": preset["bands"]})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["source"], json!("autoeq"), "{v}");
    let (s, v) = b.activer(&id).await;
    assert_eq!(s, StatusCode::OK, "{v}");

    // Bandes remplacées : la source part, le droit est demandé.
    let (s, v) = b
        .appel(
            Method::PUT,
            &uri,
            Some(json!({"name": "HD 650", "bands": bandes_parametriques()})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(v.get("source").is_none(), "{v}");
    let (s, v) = b.activer(&id).await;
    refus_parametrique(s, &v);
}

/// Une zone dont l'égaliseur est déjà la façade « casque » garde des
/// bandes paramétriques libres (même règle que la chaîne TESS).
#[tokio::test]
async fn une_zone_casque_reste_libre() {
    let b = Banc::nouveau(false).await;
    let mut reglages = profil(bandes_parametriques(), true);
    reglages.as_object_mut().unwrap().remove("enabled");
    let uri = format!("/api/v1/zones/{}/tess", b.zone);
    let (s, v) = b
        .appel(
            Method::PUT,
            &uri,
            Some(json!({"version": 1, "plugins": [
                {"id": "equalizer-headphone", "enabled": true, "settings": reglages}
            ]})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let mut autres = bandes_parametriques();
    autres[0]["gain"] = json!(2.5);
    let (s, v) = b.eq(json!({"bands": autres, "enabled": true})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = b
        .dsp(json!({"eq_profile": profil(bandes_parametriques(), true)}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

/// Les bandes du profil de pièce de la zone sont la correction de pièce, pas
/// un égaliseur paramétrique : elles relèvent de leur propre route.
#[tokio::test]
async fn les_bandes_du_profil_de_piece_ne_sont_pas_du_parametrique() {
    use tune_core::room_correction::{CorrectionFilter, FilterType, RoomProfile};
    let b = Banc::nouveau(false).await;
    let filtres = vec![
        CorrectionFilter {
            frequency_hz: 63.0,
            gain_db: -5.0,
            q_factor: 4.0,
            filter_type: FilterType::Peaking,
        },
        CorrectionFilter {
            frequency_hz: 140.0,
            gain_db: -3.0,
            q_factor: 2.5,
            filter_type: FilterType::Peaking,
        },
    ];
    let piece = RoomProfile {
        name: "Salon".into(),
        zone_id: b.zone.to_string(),
        filters: filtres.clone(),
        created_at: "2026-11-25T00:00:00Z".into(),
        measurement_data: None,
        origin: Some("estimation".into()),
    };
    b.settings()
        .set(
            &format!("room_profile_{}", b.zone),
            &serde_json::to_string(&piece).unwrap(),
        )
        .unwrap();
    let bandes =
        serde_json::to_value(tune_core::room_correction::profil_eq_de_correction(&filtres).bands)
            .unwrap();
    let (s, v) = b.dsp(json!({"eq_profile": profil(bandes, true)})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    // Contre-épreuve : les mêmes bandes retouchées sont du paramétrique.
    let mut retouchees =
        serde_json::to_value(tune_core::room_correction::profil_eq_de_correction(&filtres).bands)
            .unwrap();
    retouchees[0]["gain"] = json!(-4.0);
    let (s, v) = b.dsp(json!({"eq_profile": profil(retouchees, true)})).await;
    refus_parametrique(s, &v);
}

/// Une chaîne enregistrée avant l'exclusivité des égaliseurs (graphique ET
/// paramétrique cochés) est normalisée au `PUT` : la marque
/// `replaced_in_group` posée par le serveur ne compte pas comme un nouveau
/// réglage du paramétrique, et le `PUT` Free passe.
#[tokio::test]
async fn tess_marque_replaced_in_group_n_est_pas_un_reglage() {
    let b = Banc::nouveau(false).await;
    let mut graphique = profil(bandes_graphiques(), true);
    graphique.as_object_mut().unwrap().remove("enabled");
    let mut parametrique = profil(bandes_parametriques(), true);
    parametrique.as_object_mut().unwrap().remove("enabled");
    let ancienne: tune_core::tess::ChaineTess = serde_json::from_value(json!({
        "version": 1,
        "plugins": [
            {"id": "equalizer-graphic", "enabled": true, "settings": graphique},
            {"id": "equalizer-parametric", "enabled": true, "settings": parametrique}
        ]
    }))
    .unwrap();
    tune_core::tess::migration::ecrire_chaine(&b.settings(), b.zone, &ancienne).unwrap();
    let uri = format!("/api/v1/zones/{}/tess", b.zone);
    let (s, v) = b.appel(Method::GET, &uri, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let chaine = v["chain"].clone();
    let coches = chaine["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["enabled"] == json!(true))
        .count();
    assert_eq!(
        coches, 2,
        "l'ancienne chaîne coche deux égaliseurs : {chaine}"
    );
    let (s, v) = b.appel(Method::PUT, &uri, Some(chaine)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let p = v["chain"]["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == json!("equalizer-parametric"))
        .cloned()
        .unwrap();
    assert_eq!(p["enabled"], json!(false), "{p}");
}
