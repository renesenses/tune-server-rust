//! #4957 — ignorer un appareil ne masque QUE ses propres zones.
//!
//! Terrain (Stéphane Villerio, 0.9.163, fil 1926) : l'Eversolo DMP-A6 est
//! vu sous deux protocoles à la même adresse, `192.168.1.85` : une zone
//! DLNA « DMP-A6 » (celle qui joue) et une entrée AirPlay « eversolo,1 ».
//! Ignorer l'entrée AirPlay masquait AUSSI la zone DLNA, parce que la route
//! balayait toute zone visible du même HÔTE (`device_ignored … zones=2`).
//!
//! La règle : une zone appartient à l'appareil ignoré par son identifiant
//! exact, par une identité jumelle reconnue (`identity_matches`), ou par le
//! couple (protocole, hôte) — jamais par l'adresse IP seule.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::outputs::mock::MockOutput;

const HOTE: &str = "192.168.1.85";
const MAC: &str = "80:0A:80:5C:26:89";
const AIRPLAY: &str = "airplay-192.168.1.85-5500";
const DLNA: &str = "dlna:uuid:3E151150-D9C0-11F0-A7C6-800A805C2689";

struct Banc {
    app: axum::Router,
    state: tune_server::state::AppState,
    zone_airplay: i64,
    zone_dlna: i64,
}

async fn monter() -> Banc {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    {
        let mut reg = state.outputs.lock().await;
        reg.register(Box::new(
            MockOutput::new(AIRPLAY, "eversolo,1")
                .with_type("airplay")
                .with_host(HOTE),
        ));
        reg.register(Box::new(
            MockOutput::new(DLNA, "DMP-A6")
                .with_type("dlna")
                .with_host(HOTE),
        ));
    }
    let zones = ZoneRepo::with_backend(state.backend.clone());
    let zone_airplay = zones
        .create("eversolo,1", Some("airplay"), Some(AIRPLAY))
        .unwrap();
    let zone_dlna = zones.create("DMP-A6", Some("dlna"), Some(DLNA)).unwrap();
    // Même appareil physique : même hôte ET même MAC, comme la découverte
    // les persiste (`set_identity`).
    zones.set_identity(zone_airplay, HOTE, Some(MAC)).unwrap();
    zones.set_identity(zone_dlna, HOTE, Some(MAC)).unwrap();
    Banc {
        app: tune_server::routes::router(state.clone()),
        state,
        zone_airplay,
        zone_dlna,
    }
}

async fn ignorer(banc: &Banc, device_id: &str) -> Value {
    let res = banc
        .app
        .clone()
        .oneshot(
            Request::post(format!("/api/v1/devices/{device_id}/ignore"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = res.status();
    let octets = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&octets).unwrap_or(Value::Null);
    assert_eq!(statut, StatusCode::OK, "{corps}");
    corps
}

fn visibles(banc: &Banc) -> Vec<i64> {
    ZoneRepo::with_backend(banc.state.backend.clone())
        .list()
        .unwrap()
        .iter()
        .filter_map(|z| z.id)
        .collect()
}

/// 🔴 Le cas du terrain : ignorer l'entrée AirPlay laisse la zone DLNA du
/// même hôte visible, et sa sortie dans le registre.
#[tokio::test]
async fn ignorer_l_airplay_laisse_la_zone_dlna_de_la_meme_adresse() {
    let banc = monter().await;
    let corps = ignorer(&banc, AIRPLAY).await;

    assert_eq!(
        corps["hidden_zone_ids"],
        serde_json::json!([banc.zone_airplay]),
        "seule la zone AirPlay doit être masquée : {corps}"
    );
    let visibles = visibles(&banc);
    assert!(
        visibles.contains(&banc.zone_dlna),
        "la zone DLNA de la même adresse doit rester visible : {visibles:?}"
    );
    assert!(!visibles.contains(&banc.zone_airplay));
    assert!(
        banc.state.outputs.lock().await.get(DLNA).is_some(),
        "la sortie DLNA doit rester dans le registre"
    );
    assert!(banc.state.outputs.lock().await.get(AIRPLAY).is_none());
}

/// Symétrique : ignorer le DLNA laisse l'AirPlay.
#[tokio::test]
async fn ignorer_le_dlna_laisse_la_zone_airplay_de_la_meme_adresse() {
    let banc = monter().await;
    let corps = ignorer(&banc, DLNA).await;

    assert_eq!(
        corps["hidden_zone_ids"],
        serde_json::json!([banc.zone_dlna]),
        "seule la zone DLNA doit être masquée : {corps}"
    );
    assert!(visibles(&banc).contains(&banc.zone_airplay));
}

/// Ce qui doit continuer de marcher : une JUMELLE du même protocole au même
/// hôte (un second identifiant DLNA du même appareil, sans sortie
/// enregistrée) est masquée avec lui — c'est le couple (protocole, hôte).
#[tokio::test]
async fn une_jumelle_du_meme_protocole_au_meme_hote_est_masquee_avec_lui() {
    let banc = monter().await;
    let zones = ZoneRepo::with_backend(banc.state.backend.clone());
    let jumelle = zones
        .create(
            "DMP-A6 (2)",
            Some("dlna"),
            Some("dlna:uuid:3E151150-D9C0-11F0-A7C6-000000000002"),
        )
        .unwrap();
    zones.set_identity(jumelle, HOTE, None).unwrap();

    let corps = ignorer(&banc, DLNA).await;

    let mut masquees: Vec<i64> = corps["hidden_zone_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_i64)
        .collect();
    masquees.sort_unstable();
    let mut attendues = vec![banc.zone_dlna, jumelle];
    attendues.sort_unstable();
    assert_eq!(masquees, attendues, "{corps}");
    assert!(visibles(&banc).contains(&banc.zone_airplay));
}
