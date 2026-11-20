//! #8788 — les formats annoncés par UPnP passent les véritables routes de file.
use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use tower::ServiceExt;

#[tokio::test]
async fn les_ajouts_upnp_seuls_et_en_lot_gardent_le_format_8788() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let zid = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .create("Banc muet", Some("browser"), None)
        .unwrap();
    for lot in [false, true] {
        let piste = serde_json::json!({
            "source": "upnp", "source_id": format!("http://127.0.0.1:9/{lot}.wav"),
            "title": "WAV synthétique", "artist_name": "Banc", "duration_ms": 1000,
            "media_format": "wav", "sample_rate": 705600, "bit_depth": 24
        });
        let corps = if lot {
            serde_json::json!({"tracks": [piste]})
        } else {
            piste
        };
        let rep = router()
            .with_state(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/{zid}/queue/add"))
                    .header("Content-Type", "application/json")
                    .body(Body::from(corps.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let statut = rep.status();
        let corps = to_bytes(rep.into_body(), 1024 * 1024).await.unwrap();
        assert!(
            statut.is_success(),
            "{statut}: {}",
            String::from_utf8_lossy(&corps)
        );
    }
    let repo = PlayQueueRepo::with_backend(state.backend.clone());
    let file = repo.get_ordered(zid).unwrap();
    assert_eq!(file.len(), 2);
    for ligne in &file {
        assert_eq!(
            ligne.format.as_deref(),
            Some("wav"),
            "#8788 : la route a perdu le conteneur UPnP"
        );
        assert_eq!(
            ligne.sample_rate,
            Some(705600),
            "#8788 : la route a perdu la cadence UPnP"
        );
        assert_eq!(
            ligne.bit_depth,
            Some(24),
            "#8788 : la route a perdu la profondeur UPnP"
        );
    }
    for ligne in repo.get_streaming_queue(zid).unwrap() {
        assert_eq!(
            ligne["media_format"], "wav",
            "#8788 : la reprise après redémarrage perd le conteneur"
        );
        assert_eq!(ligne["sample_rate"], 705600);
        assert_eq!(ligne["bit_depth"], 24);
    }
}
