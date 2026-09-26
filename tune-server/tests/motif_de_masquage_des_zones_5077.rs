//! #5077 — le MOTIF du masquage d'une zone, par les vraies routes.
//!
//! « Ignorer cet appareil » masque ses zones au motif `appareil_ignore` ;
//! « Ne plus ignorer » les rend, par la réparation sûre, si plus aucun
//! ignoré ne les vise. Une zone que l'utilisateur a SUPPRIMÉE garde son motif
//! `suppression_utilisateur` : ni la cascade d'« Ignorer » ni la réparation
//! n'y touchent. Même banc que #4957 (DMP-A6 de Villerio, fil 1926).

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
    zones.set_identity(zone_airplay, HOTE, Some(MAC)).unwrap();
    zones.set_identity(zone_dlna, HOTE, Some(MAC)).unwrap();
    Banc {
        app: tune_server::routes::router(state.clone()),
        state,
        zone_airplay,
        zone_dlna,
    }
}

async fn appeler(banc: &Banc, methode: &str, chemin: &str) -> (StatusCode, Value) {
    let res = banc
        .app
        .clone()
        .oneshot(
            Request::builder()
                .method(methode)
                .uri(format!("/api/v1{chemin}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let statut = res.status();
    let octets = axum::body::to_bytes(res.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

fn repo(banc: &Banc) -> ZoneRepo {
    ZoneRepo::with_backend(banc.state.backend.clone())
}

fn motif(banc: &Banc, id: i64) -> (bool, Option<String>) {
    let e = repo(banc).etat_de_masquage(id).unwrap().unwrap();
    (e.masquee, e.motif)
}

/// Ignorer le DLNA masque SA zone au motif `appareil_ignore` ; ne plus
/// l'ignorer la rend, motif et date effacés, et la réponse le dit.
#[tokio::test]
async fn ne_plus_ignorer_rend_la_zone_masquee_par_la_cascade() {
    let banc = monter().await;
    let (statut, corps) = appeler(&banc, "POST", &format!("/devices/{DLNA}/ignore")).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(
        motif(&banc, banc.zone_dlna),
        (true, Some("appareil_ignore".into()))
    );
    assert_eq!(motif(&banc, banc.zone_airplay), (false, None));

    let (statut, corps) = appeler(&banc, "DELETE", &format!("/devices/{DLNA}/ignore")).await;
    assert_eq!(statut, StatusCode::OK, "{corps}");
    assert_eq!(
        corps["unhidden_zone_ids"],
        serde_json::json!([banc.zone_dlna]),
        "{corps}"
    );
    let e = repo(&banc)
        .etat_de_masquage(banc.zone_dlna)
        .unwrap()
        .unwrap();
    assert_eq!(
        (e.masquee, e.motif, e.masquee_le),
        (false, None, None),
        "le démasquage efface motif et date"
    );
}

/// 🔴 Une zone SUPPRIMÉE par l'utilisateur ne revient jamais : ignorer puis
/// ne plus ignorer son appareil la laisse masquée, motif intact.
#[tokio::test]
async fn une_zone_supprimee_ne_revient_pas_au_deblocage() {
    let banc = monter().await;
    let (statut, _) = appeler(&banc, "DELETE", &format!("/zones/{}", banc.zone_dlna)).await;
    assert_eq!(statut, StatusCode::NO_CONTENT);
    assert_eq!(
        motif(&banc, banc.zone_dlna),
        (true, Some("suppression_utilisateur".into()))
    );

    let (statut, _) = appeler(&banc, "POST", &format!("/devices/{DLNA}/ignore")).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        motif(&banc, banc.zone_dlna),
        (true, Some("suppression_utilisateur".into())),
        "la cascade d'« Ignorer » ne doit pas recouvrir une suppression"
    );

    let (statut, corps) = appeler(&banc, "DELETE", &format!("/devices/{DLNA}/ignore")).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(corps["unhidden_zone_ids"], serde_json::json!([]), "{corps}");
    assert_eq!(
        motif(&banc, banc.zone_dlna),
        (true, Some("suppression_utilisateur".into()))
    );
}

/// « Supprimer toutes les zones » pose `suppression_totale`, et le
/// déblocage d'un appareil n'en rend aucune.
#[tokio::test]
async fn supprimer_toutes_les_zones_pose_suppression_totale() {
    let banc = monter().await;
    let (statut, _) = appeler(&banc, "POST", &format!("/devices/{AIRPLAY}/ignore")).await;
    assert_eq!(statut, StatusCode::OK);
    let (statut, _) = appeler(&banc, "DELETE", "/zones").await;
    assert_eq!(statut, StatusCode::NO_CONTENT);
    for id in [banc.zone_airplay, banc.zone_dlna] {
        assert_eq!(motif(&banc, id), (true, Some("suppression_totale".into())));
    }
    let (_, corps) = appeler(&banc, "DELETE", &format!("/devices/{AIRPLAY}/ignore")).await;
    assert_eq!(corps["unhidden_zone_ids"], serde_json::json!([]), "{corps}");
}
