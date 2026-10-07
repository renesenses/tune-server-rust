//! Fil forum 2110 (ticket 221) — l'export GRATUIT de configuration porte les
//! zones, l'import les rapproche par appareil, et un aperçu dit ce qui
//! changera avant d'appliquer.
//!
//! Tout passe par les ROUTES montées (`GET /system/config/export`,
//! `POST /system/config/import[?dry_run=true]`), entre deux serveurs en
//! mémoire : « la machine de départ » et « la machine d'arrivée ». Les valeurs
//! secrètes sont FAUSSES et le restent.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_server::state::AppState;

const FAUX_JWT: &str = "FAUX-jwt-export-zones-1a2b";
const FAUX_QOBUZ: &str = "FAUX-jeton-qobuz-3c4d";
const APPAREIL_SALON: &str = "dlna:uuid:FAUX-salon-0001";

fn new_state() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

async fn appel(state: &AppState, req: Request<Body>) -> (StatusCode, Value) {
    let app: Router = tune_server::routes::router(state.clone());
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&bytes).to_string();
    (
        status,
        serde_json::from_str(&texte).unwrap_or(Value::String(texte)),
    )
}

async fn exporter(state: &AppState) -> Value {
    let (st, v) = appel(
        state,
        Request::get("/api/v1/system/config/export")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v
}

async fn importer(state: &AppState, corps: &Value, dry_run: bool) -> (StatusCode, Value) {
    let chemin = if dry_run {
        "/api/v1/system/config/import?dry_run=true"
    } else {
        "/api/v1/system/config/import"
    };
    appel(
        state,
        Request::post(chemin)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await
}

fn zones(state: &AppState) -> ZoneRepo {
    ZoneRepo::with_backend(state.backend.clone())
}

fn reglages(state: &AppState) -> SettingsRepo {
    SettingsRepo::with_backend(state.backend.clone())
}

/// La machine de départ : des secrets, un réglage anodin, et une zone réglée
/// (DSD, crossfeed, EQ, trim) sur un appareil DLNA.
fn machine_de_depart() -> (AppState, i64) {
    let state = new_state();
    let s = reglages(&state);
    s.set("jwt_secret", FAUX_JWT).unwrap();
    s.set(
        "auth_tokens_qobuz",
        &format!(r#"{{"user_auth_token":"{FAUX_QOBUZ}"}}"#),
    )
    .unwrap();
    s.set("theme", "nuit-profonde").unwrap();

    let repo = zones(&state);
    let id = repo
        .create("Salon", Some("dlna"), Some(APPAREIL_SALON))
        .unwrap();
    repo.update_dsd_mode(id, "dop").unwrap();
    repo.update_volume(id, 37.5).unwrap();
    s.set(
        &format!("zone_{id}_crossfeed"),
        r#"{"enabled":true,"level":"medium"}"#,
    )
    .unwrap();
    s.set(&format!("zone_{id}_eq_profile"), "loudness").unwrap();
    s.set(&format!("zone_{id}_gain_trim_db"), "-3.5").unwrap();
    s.set("default_zone_id", &id.to_string()).unwrap();
    (state, id)
}

/// Une machine d'arrivée qui a déjà UNE zone, sur un autre appareil : la zone
/// importée y prendra un AUTRE numéro qu'au départ.
fn machine_d_arrivee() -> (AppState, i64) {
    let state = new_state();
    let cuisine = zones(&state)
        .create("Cuisine", Some("airplay"), Some("airplay:FAUX-cuisine"))
        .unwrap();
    reglages(&state)
        .set(&format!("zone_{cuisine}_eq_profile"), "flat")
        .unwrap();
    (state, cuisine)
}

#[tokio::test]
async fn l_export_contient_les_zones_et_aucun_secret() {
    let (depart, id) = machine_de_depart();
    let v = exporter(&depart).await;
    let texte = v.to_string();

    assert_eq!(v["format"], json!("tune-config"));
    assert_eq!(v["format_version"], json!(2));
    for faux in [FAUX_JWT, FAUX_QOBUZ] {
        assert!(!texte.contains(faux), "un secret sort dans l'export");
    }
    assert_eq!(v["settings"]["theme"], json!("nuit-profonde"));

    let liste = v["zones"]
        .as_array()
        .expect("l'export doit porter les zones");
    let salon = liste
        .iter()
        .find(|z| z["output_device_id"] == json!(APPAREIL_SALON))
        .expect("la zone Salon doit etre exportee");
    assert_eq!(salon["name"], json!("Salon"));
    assert_eq!(salon["dsd_mode"], json!("dop"));
    assert_eq!(salon["volume"].as_f64(), Some(37.5));
    assert_eq!(
        salon["settings"]["zone_{id}_crossfeed"]["level"],
        json!("medium")
    );
    assert_eq!(salon["settings"]["zone_{id}_eq_profile"], json!("loudness"));
    // Un réglage de zone voyage DANS sa zone, plus sous son numéro.
    assert!(
        v["settings"].get(format!("zone_{id}_crossfeed")).is_none(),
        "zone_{id}_crossfeed ne doit plus etre dans la carte settings"
    );
}

#[tokio::test]
async fn un_fichier_ancien_sans_zones_se_restaure_comme_avant() {
    let (arrivee, _) = machine_d_arrivee();
    let avant = zones(&arrivee).list().unwrap().len();
    let ancien = json!({ "theme": "clair", "language": "fr" });

    let (st, apercu) = importer(&arrivee, &ancien, true).await;
    assert_eq!(st, StatusCode::OK, "{apercu}");
    assert_eq!(apercu["format_version"], json!(1));
    assert!(apercu["zones"].as_array().unwrap().is_empty());

    let (st, v) = importer(&arrivee, &ancien, false).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(
        reglages(&arrivee).get("theme").unwrap().as_deref(),
        Some("clair")
    );
    assert_eq!(zones(&arrivee).list().unwrap().len(), avant);
}

#[tokio::test]
async fn l_apercu_ne_modifie_rien_et_dit_ce_qui_changera() {
    let (depart, _) = machine_de_depart();
    let fichier = exporter(&depart).await;
    let (arrivee, _) = machine_d_arrivee();
    reglages(&arrivee).set("theme", "clair").unwrap();

    let reglages_avant = reglages(&arrivee).all().unwrap();
    let zones_avant = zones(&arrivee).list().unwrap().len();

    let (st, apercu) = importer(&arrivee, &fichier, true).await;
    assert_eq!(st, StatusCode::OK, "{apercu}");
    assert_eq!(apercu["dry_run"], json!(true));

    // Rien n'a bougé.
    assert_eq!(reglages(&arrivee).all().unwrap(), reglages_avant);
    assert_eq!(zones(&arrivee).list().unwrap().len(), zones_avant);
    assert!(
        zones(&arrivee)
            .get_by_device_id(APPAREIL_SALON)
            .unwrap()
            .is_none(),
        "l'apercu a cree la zone"
    );

    // Et l'aperçu le dit.
    let modifies: Vec<&str> = apercu["settings"]["modified"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(modifies.contains(&"theme"), "{apercu}");
    let salon = apercu["zones"]
        .as_array()
        .unwrap()
        .iter()
        .find(|z| z["output_device_id"] == json!(APPAREIL_SALON))
        .expect("la zone Salon doit figurer dans l'apercu");
    assert_eq!(salon["status"], json!("added"));
    assert_eq!(salon["offline"], json!(true));
}

#[tokio::test]
async fn une_zone_sans_appareil_ici_est_importee_hors_ligne_avec_ses_reglages() {
    let (depart, id_depart) = machine_de_depart();
    let fichier = exporter(&depart).await;
    let (arrivee, cuisine) = machine_d_arrivee();

    let (st, v) = importer(&arrivee, &fichier, false).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["zones_added"], json!(1), "{v}");

    let repo = zones(&arrivee);
    let salon = repo
        .get_by_device_id(APPAREIL_SALON)
        .unwrap()
        .expect("la zone Salon doit avoir ete creee");
    let id = salon.id.unwrap();
    assert_ne!(id, id_depart, "le banc doit changer de numero");
    assert!(
        !salon.online,
        "une zone sans appareil ici arrive hors ligne"
    );
    assert_eq!(salon.name, "Salon");
    assert!((salon.volume - 37.5).abs() < 1e-9);
    assert_eq!(repo.get_dsd_mode(id), "dop");

    // Les réglages de zone suivent la zone sous son NOUVEAU numéro.
    let s = reglages(&arrivee);
    assert_eq!(
        s.get(&format!("zone_{id}_eq_profile")).unwrap().as_deref(),
        Some("loudness")
    );
    assert_eq!(
        s.get(&format!("zone_{id}_gain_trim_db"))
            .unwrap()
            .as_deref(),
        Some("-3.5")
    );
    assert!(
        s.get(&format!("zone_{id}_crossfeed"))
            .unwrap()
            .is_some_and(|c| c.contains("medium"))
    );
    assert_eq!(
        s.get("default_zone_id").unwrap().as_deref(),
        Some(id.to_string().as_str()),
        "la zone par defaut suit la zone, pas son ancien numero"
    );

    // La zone de la machine absente du fichier n'est ni supprimée ni touchée.
    let c = repo.get(cuisine).unwrap().expect("Cuisine doit survivre");
    assert_eq!(c.name, "Cuisine");
    assert_eq!(
        s.get(&format!("zone_{cuisine}_eq_profile"))
            .unwrap()
            .as_deref(),
        Some("flat")
    );
}

#[tokio::test]
async fn une_zone_est_rapprochee_par_son_appareil_et_jamais_dupliquee() {
    let (depart, _) = machine_de_depart();
    let fichier = exporter(&depart).await;
    let (arrivee, _) = machine_d_arrivee();
    // Le même appareil existe déjà ici, sous un autre nom.
    let ici = zones(&arrivee)
        .create("Ampli du salon", Some("dlna"), Some(APPAREIL_SALON))
        .unwrap();
    let avant = zones(&arrivee).list().unwrap().len();

    let (st, apercu) = importer(&arrivee, &fichier, true).await;
    assert_eq!(st, StatusCode::OK);
    let salon = &apercu["zones"][0];
    assert_eq!(salon["status"], json!("modified"), "{apercu}");
    assert_eq!(salon["offline"], json!(false));
    let changes: Vec<&str> = salon["changes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(changes.contains(&"name") && changes.contains(&"dsd_mode"));

    let (st, v) = importer(&arrivee, &fichier, false).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(zones(&arrivee).list().unwrap().len(), avant);
    assert_eq!(zones(&arrivee).get(ici).unwrap().unwrap().name, "Salon");
    assert_eq!(zones(&arrivee).get_dsd_mode(ici), "dop");

    // Un second import du même fichier ne change plus rien.
    let (_, apercu) = importer(&arrivee, &fichier, true).await;
    assert_eq!(apercu["zones"][0]["status"], json!("unchanged"), "{apercu}");
}

#[tokio::test]
async fn un_fichier_d_une_version_future_est_refuse_sans_rien_ecrire() {
    let (arrivee, _) = machine_d_arrivee();
    let avant = reglages(&arrivee).all().unwrap();
    let (st, _) = importer(
        &arrivee,
        &json!({"format_version": 99, "settings": {"theme": "x"}}),
        false,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(reglages(&arrivee).all().unwrap(), avant);
}

/// Le chemin que l'écran emploie : un serveur antérieur l'ignore (404) au
/// lieu d'appliquer l'import, ce que ferait `?dry_run=true` chez lui.
#[tokio::test]
async fn le_chemin_d_apercu_dedie_ne_modifie_rien() {
    let (depart, _) = machine_de_depart();
    let fichier = exporter(&depart).await;
    let (arrivee, _) = machine_d_arrivee();
    let avant = reglages(&arrivee).all().unwrap();

    let (st, apercu) = appel(
        &arrivee,
        Request::post("/api/v1/system/config/import/preview")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(fichier.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{apercu}");
    assert_eq!(apercu["dry_run"], json!(true));
    assert_eq!(apercu["zones"][0]["status"], json!("added"));
    assert_eq!(reglages(&arrivee).all().unwrap(), avant);
    assert!(
        zones(&arrivee)
            .get_by_device_id(APPAREIL_SALON)
            .unwrap()
            .is_none()
    );
}
