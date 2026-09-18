use super::*;
use axum::{body::Body, http::Request};
use serde_json::Value;
use tower::ServiceExt;
use tune_core::db::media_server_repo::MediaServerRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::discovery::device::{DiscoveredDevice, OutputType};
use tune_core::discovery::ssdp::{MediaServerInfo, SsdpEvent};

const PORT: u16 = 38886;

fn state() -> AppState {
    let config = TuneConfig {
        port: PORT,
        ..Default::default()
    };
    AppState::new(":memory:", PORT, config).unwrap()
}

fn own_server(state: &AppState) -> MediaServerInfo {
    MediaServerInfo {
        id: state.upnp.as_ref().expect("local UPnP state").uuid.clone(),
        name: "Tune Server (self fixture)".into(),
        manufacturer: "Tune".into(),
        model: "Tune Server".into(),
        location: tune_core::upnp_server::advert_location("127.0.0.1", PORT),
        content_directory_url: format!(
            "http://127.0.0.1:{PORT}{}/ContentDirectory/control",
            tune_core::upnp_server::MOUNT_PATH
        ),
        host: "127.0.0.1".into(),
        port: PORT,
        last_seen: std::time::Instant::now(),
        max_age: std::time::Duration::from_secs(1800),
    }
}

/// Feed the actual production event consumer, then close its input and wait
/// until every event is processed. No scanner or multicast task is started.
async fn deliver(state: &AppState, events: Vec<SsdpEvent>) {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let handler = spawn_ssdp_event_handler(state, &state.config, None, rx);
    for event in events {
        tx.send(event).await.unwrap();
    }
    drop(tx);
    tokio::time::timeout(std::time::Duration::from_secs(10), handler)
        .await
        .expect("SSDP event consumer must finish")
        .unwrap();
}

async fn list_http(state: &AppState) -> Vec<Value> {
    let response = crate::routes::network::router()
        .with_state(state.clone())
        .oneshot(
            Request::builder()
                .uri("/media-servers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let items = body["items"]
        .as_array()
        .expect("media-server response must expose items");
    assert_eq!(body["total"].as_u64(), Some(items.len() as u64));
    items.clone()
}

fn assert_no_zone_or_track_mutation(state: &AppState) {
    assert!(
        ZoneRepo::with_backend(state.backend.clone())
            .list()
            .unwrap()
            .is_empty(),
        "a MediaServer must not create a zone"
    );
    let rows = state
        .backend
        .query_one("SELECT COUNT(*) FROM tracks", &[])
        .unwrap()
        .unwrap();
    assert_eq!(
        rows[0].as_i64(),
        Some(0),
        "listing must not insert tracks in this fixture"
    );
}

#[tokio::test]
async fn own_media_server_is_persisted_and_visible_in_http_list() {
    let state = state();
    let own = own_server(&state);
    let id = own.id.clone();
    assert!(est_notre_propre_serveur_multimedia(
        &own,
        PORT,
        &["127.0.0.1".into()],
        None
    ));
    deliver(&state, vec![SsdpEvent::MediaServerDiscovered(own)]).await;

    let listed = list_http(&state).await;
    assert_eq!(
        listed.len(),
        1,
        "our discovered MediaServer must appear in the HTTP list"
    );
    assert_eq!(listed[0]["id"], id);
    assert_eq!(listed[0]["reachable"], true);
    assert_eq!(listed[0]["proposable"], true);
    assert!(
        state.media_servers.lock().await.contains_key(&id),
        "Browse needs the in-memory entry"
    );
    let persisted = MediaServerRepo::with_backend(state.backend.clone())
        .lister()
        .unwrap();
    assert_eq!(
        persisted.len(),
        1,
        "the durable registry must retain the same server"
    );
    assert_eq!(persisted[0].udn, id);
    // active means offered to the user; it is not an automatic import job.
    assert!(persisted[0].active);
    assert_no_zone_or_track_mutation(&state);
    assert!(
        state.outputs.lock().await.list().is_empty(),
        "a source must not become an output"
    );
}

#[tokio::test]
async fn repeated_self_announcements_keep_one_entry_and_preserve_the_neighbor() {
    let state = state();
    let own = own_server(&state);
    let mut neighbor = own.clone();
    neighbor.id = "uuid:neighbor-tune".into();
    neighbor.name = "Other Tune".into();
    neighbor.host = "192.0.2.22".into();
    neighbor.location = tune_core::upnp_server::advert_location(&neighbor.host, PORT);
    neighbor.content_directory_url =
        format!("http://192.0.2.22:{PORT}/upnp/ContentDirectory/control");
    deliver(
        &state,
        vec![
            SsdpEvent::MediaServerDiscovered(own.clone()),
            SsdpEvent::MediaServerDiscovered(neighbor.clone()),
            SsdpEvent::MediaServerDiscovered(own.clone()),
        ],
    )
    .await;
    let listed = list_http(&state).await;
    assert_eq!(
        listed.len(),
        2,
        "self and neighboring Tune must both remain visible without duplicates"
    );
    for id in [&own.id, &neighbor.id] {
        assert_eq!(
            listed
                .iter()
                .filter(|row| row["id"].as_str() == Some(id.as_str()))
                .count(),
            1
        );
    }
    assert_eq!(
        MediaServerRepo::with_backend(state.backend.clone())
            .lister()
            .unwrap()
            .len(),
        2
    );
    assert_no_zone_or_track_mutation(&state);
}

#[tokio::test]
async fn own_renderer_remains_excluded_while_own_media_server_is_listed() {
    let state = state();
    let own = own_server(&state);
    let mut renderer = DiscoveredDevice::new(
        "uuid:self-renderer".into(),
        "Living room (Tune)".into(),
        OutputType::Dlna,
        "127.0.0.1".into(),
        PORT,
    );
    renderer.location = Some(format!(
        "http://127.0.0.1:{PORT}/upnp/renderer/1/description.xml"
    ));
    deliver(
        &state,
        vec![
            SsdpEvent::DeviceDiscovered(Box::new(renderer)),
            SsdpEvent::MediaServerDiscovered(own.clone()),
        ],
    )
    .await;

    let listed = list_http(&state).await;
    assert_eq!(
        listed.len(),
        1,
        "renderer exclusion must not hide the MediaServer"
    );
    assert_eq!(listed[0]["id"], own.id);
    assert_no_zone_or_track_mutation(&state);
    assert!(
        state.outputs.lock().await.list().is_empty(),
        "own renderer must not re-create a phantom output"
    );
}

#[tokio::test]
async fn no_announcement_does_not_synthesize_a_server() {
    let state = state();
    deliver(&state, Vec::new()).await;
    assert!(
        list_http(&state).await.is_empty(),
        "only discovered servers are listed"
    );
    assert!(state.media_servers.lock().await.is_empty());
    assert_no_zone_or_track_mutation(&state);
}
