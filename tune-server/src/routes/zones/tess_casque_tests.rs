//! rc14 — le greffon TESS « Casque » tiré d'OPRA, par le routeur public.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Method, Request};
use tower::ServiceExt;
use tune_core::audio::autoeq_base::TelechargeurAutoEq;
use tune_core::audio::opra_base::CatalogueOpra;
use tune_core::tess::migration::{ID_EQ_GRAPHIQUE, Partie};

use super::*;

const HD650: &str = "sennheiser:hd650::oratory1990_harman_target";

/// Un extrait de la base OPRA (lignes de la base publiée, bandes abrégées).
const BASE: &str = r#"{"type":"vendor","id":"sennheiser","data":{"name":"Sennheiser"}}
{"type":"product","id":"sennheiser::hd650","data":{"name":"HD650","type":"headphones","subtype":"over_the_ear","vendor_id":"sennheiser"}}
{"type":"eq","id":"sennheiser:hd650::oratory1990_harman_target","data":{"author":"oratory1990","details":"Harman Target","type":"parametric_eq","product_id":"sennheiser::hd650","parameters":{"gain_db":-9.3,"bands":[{"type":"peak_dip","frequency":20,"gain_db":4,"q":1.1},{"type":"low_shelf","frequency":105,"gain_db":5.5,"q":0.71},{"type":"high_shelf","frequency":10000,"gain_db":-3,"q":0.71}]}}}
{"type":"eq","id":"sennheiser:hd650::bande_passante","data":{"author":"x","details":"y","type":"parametric_eq","product_id":"sennheiser::hd650","parameters":{"gain_db":0,"bands":[{"type":"band_pass","frequency":1000,"gain_db":3,"q":1}]}}}
"#;

struct FauxReseau(Result<String, String>);

#[async_trait]
impl TelechargeurAutoEq for FauxReseau {
    async fn telecharger(&self, _url: &str, _taille_max: usize) -> Result<String, String> {
        self.0.clone()
    }
}

struct Banc {
    state: AppState,
    app: axum::Router,
    zone: i64,
}

impl Banc {
    async fn nouveau(premium: bool) -> Self {
        Self::avec_reseau(premium, Ok(BASE.to_owned())).await
    }

    async fn avec_reseau(premium: bool, reseau: Result<String, String>) -> Self {
        let mut state = AppState::new(":memory:", 0, Default::default()).unwrap();
        state.opra = Arc::new(CatalogueOpra::new(
            "http://opra.invalid/database_v1.jsonl",
            Arc::new(FauxReseau(reseau)),
            None,
        ));
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
        let zone = ZoneRepo::with_backend(state.backend.clone())
            .create("Zone", Some("local"), Some("local:Realtek HD"))
            .unwrap();
        let app = crate::routes::router(state.clone());
        Self { state, app, zone }
    }

    async fn requete(
        &self,
        methode: Method,
        uri: &str,
        corps: Option<Value>,
    ) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(methode)
            .uri(uri)
            .header("Content-Type", "application/json")
            .body(corps.map_or(Body::empty(), |c| Body::from(c.to_string())))
            .unwrap();
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let statut = resp.status();
        let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            statut,
            serde_json::from_slice(&octets).unwrap_or(Value::Null),
        )
    }

    async fn put_casque(&self, corps: Value) -> (StatusCode, Value) {
        let uri = format!("/api/v1/zones/{}/tess/headphone", self.zone);
        self.requete(Method::PUT, &uri, Some(corps)).await
    }

    async fn put_chaine(&self, mut corps: Value) -> (StatusCode, Value) {
        corps["version"] = json!(tune_core::tess::VERSION_CHAINE);
        let uri = format!("/api/v1/zones/{}/tess", self.zone);
        self.requete(Method::PUT, &uri, Some(corps)).await
    }

    async fn get_casque(&self) -> (StatusCode, Value) {
        let uri = format!("/api/v1/zones/{}/tess/headphone", self.zone);
        self.requete(Method::GET, &uri, None).await
    }

    fn chaine(&self) -> ChaineTess {
        lire_zone(
            &SettingsRepo::with_backend(self.state.backend.clone()),
            self.zone,
            true,
        )
        .unwrap()
        .chaine
    }

    fn maillon_casque(&self) -> MaillonGreffon {
        self.chaine()
            .plugins
            .into_iter()
            .find(|m| m.id == ID_EQ_CASQUE)
            .expect("maillon casque")
    }
}

fn profil_graphique() -> Value {
    json!({
        "listening": "speakers", "room_size": "medium",
        "speaker_placement": "free_standing", "bass_gain_db": 2.0,
        "mid_gain_db": 0.0, "treble_gain_db": 0.0, "bands": []
    })
}

#[tokio::test]
async fn la_recherche_rend_marque_modele_variantes_et_attribution() {
    let b = Banc::nouveau(false).await;
    let (s, v) = b
        .requete(
            Method::GET,
            "/api/v1/tess/headphone/search?q=sennheiser%20hd%20650",
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["total"], 1);
    let p = &v["results"][0];
    assert_eq!(p["id"], "sennheiser::hd650");
    assert_eq!(p["vendor"], "Sennheiser");
    assert_eq!(p["name"], "HD650");
    assert_eq!(p["variants"][0]["id"], HD650);
    assert_eq!(p["variants"][0]["author"], "oratory1990");
    assert_eq!(v["source"]["license"], "CC BY-SA 4.0");
    // Requête vide : liste vide, sans toucher au réseau.
    let (s, v) = b
        .requete(Method::GET, "/api/v1/tess/headphone/search?q=", None)
        .await;
    assert_eq!((s, v["total"].clone()), (StatusCode::OK, json!(0)));
}

#[tokio::test]
async fn hors_ligne_la_recherche_le_dit() {
    let b = Banc::avec_reseau(false, Err("coupé".into())).await;
    let (s, v) = b
        .requete(Method::GET, "/api/v1/tess/headphone/search?q=hd650", None)
        .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    assert_eq!(v["error"], "opra_unreachable");
}

#[tokio::test]
async fn l_apercu_rend_bandes_preampli_et_courbe() {
    let b = Banc::nouveau(false).await;
    let uri = format!("/api/v1/tess/headphone/profile?id={HD650}");
    let (s, v) = b.requete(Method::GET, &uri, None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["bands"].as_array().unwrap().len(), 3);
    assert_eq!(v["level"], 100);
    assert!(v["preamp_db"].as_f64().unwrap() < -3.0, "{v}");
    assert_eq!(v["curve"]["frequency_hz"].as_array().unwrap().len(), 96);
    assert_eq!(v["curve"]["gain_db"].as_array().unwrap().len(), 96);
    assert_eq!(v["product"]["vendor"], "Sennheiser");
    assert_eq!(v["variant"]["details"], "Harman Target");
    assert_eq!(v["settings"]["opra"]["id"], HD650);
    // Une correction que Tune ne sait pas jouer : refusée, nommée.
    let uri = "/api/v1/tess/headphone/profile?id=sennheiser:hd650::bande_passante";
    let (s, v) = b.requete(Method::GET, uri, None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["error"], "opra_profile_unsupported");
    assert_eq!(v["reason"], "unknown_filter_type");
    let (s, v) = b
        .requete(
            Method::GET,
            "/api/v1/tess/headphone/profile?id=nulle::part",
            None,
        )
        .await;
    assert_eq!(
        (s, v["error"].clone()),
        (StatusCode::NOT_FOUND, json!("opra_profile_unknown"))
    );
}

#[tokio::test]
async fn un_compte_gratuit_applique_un_profil_opra_reconnu() {
    let b = Banc::nouveau(false).await;
    let (s, v) = b.put_casque(json!({ "opra_id": HD650 })).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["headphone"]["profile"]["id"], HD650);
    assert_eq!(v["headphone"]["enabled"], true);

    // Le maillon garde la référence OPRA, pas seulement des bandes.
    let m = b.maillon_casque();
    assert!(m.enabled);
    let r = profil_reconnu(&m.settings).expect("profil reconnu");
    assert_eq!((r.id.as_str(), r.level), (HD650, 100));
    assert_eq!(m.settings["headroom_mode"], "realistic");

    // Les bandes atteignent la clé que le moteur lit.
    let brut = SettingsRepo::with_backend(b.state.backend.clone())
        .get(&Partie::Egaliseur.cle(b.zone))
        .unwrap()
        .expect("zone_eq_profile");
    let profil: Value = serde_json::from_str(&brut).unwrap();
    assert_eq!(profil["enabled"], true);
    assert_eq!(profil["bands"].as_array().unwrap().len(), 3);

    let (s, v) = b.get_casque().await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["profile"]["vendor"], "Sennheiser");
    assert_eq!(v["free_bands"], false);
    assert!(v["preamp_db"].as_f64().unwrap() < 0.0);
}

#[tokio::test]
async fn niveau_et_interrupteur_sans_rechoisir_le_profil() {
    let b = Banc::nouveau(false).await;
    assert_eq!(
        b.put_casque(json!({ "opra_id": HD650 })).await.0,
        StatusCode::OK
    );
    let (s, v) = b.put_casque(json!({ "level": 50 })).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let m = b.maillon_casque();
    let r = profil_reconnu(&m.settings).unwrap();
    assert_eq!((r.id.as_str(), r.level), (HD650, 50));
    // Le plateau grave (+5,5 dB) est à moitié.
    assert_eq!(m.settings["bands"][1]["gain"], 2.75);

    let (s, _) = b.put_casque(json!({ "enabled": false })).await;
    assert_eq!(s, StatusCode::OK);
    let m = b.maillon_casque();
    assert!(!m.enabled);
    // Éteint, il garde son profil et son niveau.
    assert_eq!(profil_reconnu(&m.settings).unwrap().level, 50);
}

#[tokio::test]
async fn les_corps_fautifs_sont_refuses() {
    let b = Banc::nouveau(false).await;
    let (s, v) = b.put_casque(json!({ "level": 80 })).await;
    assert_eq!(
        (s, v["error"].clone()),
        (StatusCode::BAD_REQUEST, json!("opra_profile_required"))
    );
    let (s, v) = b
        .put_casque(json!({ "opra_id": HD650, "level": 101 }))
        .await;
    assert_eq!(
        (s, v["error"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_level"))
    );
    // Le client ne fournit pas de bandes : il désigne une correction.
    let (s, v) = b.put_casque(json!({ "opra_id": HD650, "bands": [] })).await;
    assert_eq!(
        (s, v["error"].clone()),
        (StatusCode::BAD_REQUEST, json!("invalid_body"))
    );
    let (s, v) = b.put_casque(json!({ "opra_id": "nulle::part" })).await;
    assert_eq!(
        (s, v["error"].clone()),
        (StatusCode::NOT_FOUND, json!("opra_profile_unknown"))
    );
    // Rien n'a été écrit.
    assert!(b.chaine().plugins.iter().all(|m| m.id != ID_EQ_CASQUE));
}

/// Le casque suit la règle de groupe des égaliseurs EN VIGUEUR : exactement
/// ce que répond `PUT /zones/{id}/tess` pour la même chaîne. Exclusifs
/// aujourd'hui (409), combinables quand la règle sera levée : ce témoin ne
/// change pas.
#[tokio::test]
async fn le_casque_suit_la_regle_des_egaliseurs_de_la_chaine() {
    let temoin = Banc::nouveau(false).await;
    let casque = Banc::nouveau(false).await;
    for b in [&temoin, &casque] {
        let (s, v) = b
            .put_chaine(json!({ "plugins": [
                { "id": ID_EQ_GRAPHIQUE, "enabled": true, "settings": profil_graphique() }
            ]}))
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
    // Le témoin coche un casque à bandes libres à côté du graphique…
    let mut reglages = profil_graphique();
    reglages["bands"] = json!([{ "freq": 105.0, "gain": 5.5, "q": 0.71, "type": "low_shelf" }]);
    let (attendu, _) = temoin
        .put_chaine(json!({ "plugins": [
            { "id": ID_EQ_CASQUE, "enabled": true, "settings": reglages },
            { "id": ID_EQ_GRAPHIQUE, "enabled": true, "settings": profil_graphique() }
        ]}))
        .await;
    // … la route du casque répond pareil.
    let (s, v) = casque.put_casque(json!({ "opra_id": HD650 })).await;
    assert_eq!(s, attendu, "{v}");
    if s == StatusCode::OK {
        let ids: Vec<(String, bool)> = casque
            .chaine()
            .plugins
            .into_iter()
            .map(|m| (m.id, m.enabled))
            .collect();
        assert!(ids.contains(&(ID_EQ_CASQUE.to_owned(), true)), "{ids:?}");
    }
}

/// La voie de l'écran TESS : les `settings` de l'aperçu, posés tels quels
/// par `PUT /zones/{id}/tess`, gardent un profil reconnu.
#[tokio::test]
async fn les_reglages_de_l_apercu_passent_par_le_put_de_la_chaine() {
    let b = Banc::nouveau(false).await;
    let uri = format!("/api/v1/tess/headphone/profile?id={HD650}&level=80");
    let (_, apercu) = b.requete(Method::GET, &uri, None).await;
    let (s, v) = b
        .put_chaine(json!({ "plugins": [
            { "id": ID_EQ_CASQUE, "enabled": true, "settings": apercu["settings"] }
        ]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, v) = b.get_casque().await;
    assert_eq!(v["profile"]["id"], HD650);
    assert_eq!(v["profile"]["level"], 80);
    assert_eq!(v["free_bands"], false);
}

#[tokio::test]
async fn des_bandes_libres_restent_acceptees_mais_non_reconnues() {
    let b = Banc::nouveau(false).await;
    let mut reglages = profil_graphique();
    reglages["bands"] = json!([{ "freq": 105.0, "gain": 5.5, "q": 0.71, "type": "low_shelf" }]);
    let (s, v) = b
        .put_chaine(json!({ "plugins": [
            { "id": ID_EQ_CASQUE, "enabled": true, "settings": reglages }
        ]}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (_, v) = b.get_casque().await;
    assert_eq!(v["present"], true);
    assert_eq!(v["profile"], Value::Null);
    assert_eq!(v["free_bands"], true);
}

#[test]
fn le_casque_se_pose_avant_le_premier_egaliseur() {
    let mut c = ChaineTess::default();
    c.plugins.push(MaillonGreffon {
        id: "crossfeed".into(),
        enabled: true,
        settings: Value::Null,
    });
    c.plugins.push(MaillonGreffon {
        id: ID_EQ_GRAPHIQUE.into(),
        enabled: false,
        settings: Value::Null,
    });
    let casque = MaillonGreffon {
        id: ID_EQ_CASQUE.into(),
        enabled: true,
        settings: json!({}),
    };
    let ids = |c: &ChaineTess| c.plugins.iter().map(|m| m.id.clone()).collect::<Vec<_>>();
    let posee = chaine_avec_le_casque(&c, casque.clone());
    assert_eq!(ids(&posee), ["crossfeed", ID_EQ_CASQUE, ID_EQ_GRAPHIQUE]);
    // Déjà là : remplacé à sa place, jamais doublé.
    let reposee = chaine_avec_le_casque(&posee, casque);
    assert_eq!(ids(&reposee), ids(&posee));
}
