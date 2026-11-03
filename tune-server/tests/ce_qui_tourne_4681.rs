//! #4681 — `GET /system/background-tasks/running`, « ce qui tourne », par le
//! VRAI routeur.
//!
//! Binaire à lui seul : le témoin de lecture, les cessions notées, l'avancement
//! de la cascade et les retenues thermiques sont des états de PROCESSUS. Un
//! seul test, qui les pose dans l'ordre et les remet à neuf.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_server::state::AppState;

const ROUTE: &str = "/api/v1/system/background-tasks/running";

async fn lire(app: &axum::Router) -> Value {
    let resp = app
        .clone()
        .oneshot(Request::get(ROUTE).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let octets = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    serde_json::from_slice(&octets).unwrap()
}

fn passe<'a>(corps: &'a Value, id: &str) -> &'a Value {
    corps["passes"]
        .as_array()
        .expect("passes absent")
        .iter()
        .find(|p| p["id"] == id)
        .unwrap_or_else(|| panic!("aucune ligne pour {id} : {corps}"))
}

fn ids(v: &Value) -> Vec<&str> {
    v.as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect()
}

#[tokio::test]
async fn ce_qui_tourne_dit_les_passes_la_lecture_l_avancement_et_la_chaleur_4681() {
    use tune_core::taches_de_fond::priorite;
    priorite::oublier_la_lecture_pour_les_essais();
    tune_core::audio::replaygain::progression::reinitialiser_pour_les_essais();

    let dossier = tune_core::test_scratch::scratch_dir("tune-4681-ce-qui-tourne");
    let db = dossier.join("tune.db");
    let state = AppState::new(db.to_str().unwrap(), 0, Default::default()).unwrap();
    let app = tune_server::routes::router(state.clone());

    // Au repos : toutes les passes ont leur ligne, rien ne tourne.
    let repos = lire(&app).await;
    let attendues = [
        "replaygain",
        "fingerprints",
        "dynamic_range",
        "acoustic",
        "enrichment",
        "artist_images",
        "identification",
        "hires_check",
        "scan",
    ];
    for id in attendues {
        assert_eq!(passe(&repos, id)["activity"], "idle", "{id} : {repos}");
    }
    assert!(ids(&repos["running"]).is_empty(), "{repos}");
    assert_eq!(repos["playback"]["active"], false, "{repos}");

    // Une zone joue. La cascade a cédé, l'enrichissement est freiné ; la
    // cascade a une campagne ouverte, l'enrichissement publie 3 sur 7.
    priorite::noter_etat_de_lecture(42, "playing");
    priorite::noter_cedee("replaygain");
    priorite::noter_cedee("enrichment");
    tune_core::audio::replaygain::progression::ouvrir_si_besoin(|| 10);
    tune_core::audio::replaygain::progression::avancer();
    let _enrichissement =
        state
            .background_tasks
            .begin("enrich_all", "Enrichissement", "enrichment");
    state
        .background_tasks
        .update_progress("enrich_all", 3, 7, "MusicBrainz");

    let lecture = lire(&app).await;
    assert_eq!(lecture["playback"]["active"], true, "{lecture}");
    assert_eq!(
        lecture["playback"]["playing_zone_ids"],
        serde_json::json!([42])
    );

    let rg = passe(&lecture, "replaygain");
    assert_eq!(rg["activity"], "yielded", "la passe qui décode cède : {rg}");
    assert_eq!(rg["reason"], "playback", "{rg}");
    assert_eq!(rg["progress"]["processed"], 1, "{rg}");
    assert_eq!(rg["progress"]["total"], 10, "{rg}");

    let en = passe(&lecture, "enrichment");
    assert_eq!(
        en["activity"], "throttled",
        "l'enrichissement ralentit : {en}"
    );
    assert_eq!(en["reason"], "playback", "{en}");
    assert_eq!(en["progress"]["processed"], 3, "{en}");
    assert_eq!(en["progress"]["total"], 7, "{en}");
    assert_eq!(en["progress"]["detail"], "MusicBrainz", "{en}");

    let freinees = ids(&lecture["throttled_for_playback"]);
    assert!(freinees.contains(&"replaygain") && freinees.contains(&"enrichment"));
    let tournent = ids(&lecture["running"]);
    assert!(tournent.contains(&"replaygain") && tournent.contains(&"enrichment"));
    assert!(!tournent.contains(&"acoustic"), "{lecture}");

    // La garde thermique, telle que la machine du banc la voit. Quelle que
    // soit cette machine, la règle de #6450 se lit : illisible ⇒ bridé à un
    // fichier ; lisible ⇒ la largeur réglée.
    let t = &lecture["thermal"];
    assert_eq!(t["speed"], "normal", "{t}");
    assert!(t["width_configured"].as_u64().is_some(), "{t}");
    match t["readable"].as_bool() {
        Some(false) => {
            assert_eq!(t["throttled"], true, "{t}");
            assert_eq!(t["width_effective"], 1, "{t}");
            assert!(t["reason"].is_string(), "{t}");
        }
        Some(true) => {
            assert_eq!(t["throttled"], false, "{t}");
            assert_eq!(t["width_effective"], t["width_configured"], "{t}");
        }
        None => assert!(t["width_effective"].is_null(), "{t}"),
    }
    assert_eq!(t["pause_above_c"], 80.0, "{t}");

    // La lecture s'arrête : plus rien n'est freiné, l'enrichissement travaille.
    priorite::noter_etat_de_lecture(42, "stopped");
    let apres = lire(&app).await;
    assert!(ids(&apres["throttled_for_playback"]).is_empty(), "{apres}");
    assert_eq!(
        passe(&apres, "enrichment")["activity"],
        "working",
        "{apres}"
    );

    priorite::oublier_la_lecture_pour_les_essais();
    tune_core::audio::replaygain::progression::reinitialiser_pour_les_essais();
}
