//! #8135 (fil 2703) — une RADIO comme élément d'une playlist.
//!
//! Une playlist de titres locaux sert de réveil ; à sa fin, une radio prend le
//! relais pour la matinée. Ce banc garde le contrat, route par route :
//!
//! - l'AJOUT range une station de « Mes radios » (`radio_ids`) et refuse en le
//!   disant une station inconnue ;
//! - la LISTE la montre comme une radio (`radio`, `radio_id`, `unavailable`),
//!   y compris retirée de « Mes radios » ;
//! - la LECTURE enfile les pistes PUIS le flux de la radio ; une station
//!   retirée est sautée ;
//! - l'EXPORT M3U écrit son flux, que l'IMPORT range de nouveau en radio.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::radio_repo::{RadioRepo, RadioStation};

const FIP: &str = "https://flux.invalid/fip-8135.aac";

fn etat() -> tune_server::state::AppState {
    tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn appli(state: &tune_server::state::AppState) -> axum::Router {
    tune_server::routes::router(state.clone())
}

fn piste(state: &tune_server::state::AppState, titre: &str, chemin: &str) -> i64 {
    let repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    let mut t = tune_core::db::models::Track::new(titre.into());
    t.file_path = Some(chemin.into());
    repo.create(&t).expect("insert track")
}

fn station(state: &tune_server::state::AppState, nom: &str, url: &str) -> i64 {
    RadioRepo::with_backend(state.backend.clone())
        .create(&RadioStation {
            id: None,
            name: nom.into(),
            url: url.into(),
            homepage: None,
            logo_url: None,
            country: None,
            language: None,
            genre: None,
            codec: None,
            bitrate: None,
            is_favorite: false,
            last_played: None,
            play_count: 0,
        })
        .expect("station")
}

async fn reponse(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!(String::from_utf8_lossy(&bytes).to_string()));
    (status, json)
}

async fn poste(app: &axum::Router, path: &str, body: Value) -> (StatusCode, Value) {
    reponse(
        app,
        Request::builder()
            .method("POST")
            .uri(path)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

async fn lis(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    reponse(app, Request::get(path).body(Body::empty()).unwrap()).await
}

async fn lignes(app: &axum::Router, playlist: i64) -> Vec<Value> {
    let (st, v) = lis(app, &format!("/api/v1/playlists/{playlist}/tracks")).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    v.as_array().cloned().unwrap_or_default()
}

/// Une playlist « deux pistes puis FIP », rangée par les routes du client.
async fn reveil(st: &tune_server::state::AppState, app: &axum::Router) -> (i64, [i64; 2], i64) {
    let t1 = piste(st, "Un", "/musique/un.flac");
    let t2 = piste(st, "Deux", "/musique/deux.flac");
    let fip = station(st, "FIP", FIP);
    let (s, v) = poste(app, "/api/v1/playlists", json!({ "name": "Réveil" })).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let pl = v["id"].as_i64().unwrap();
    let (s, v) = poste(
        app,
        &format!("/api/v1/playlists/{pl}/tracks"),
        json!({ "track_ids": [t1, t2] }),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (s, v) = poste(
        app,
        &format!("/api/v1/playlists/{pl}/tracks"),
        json!({ "radio_ids": [fip] }),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    (pl, [t1, t2], fip)
}

#[tokio::test]
async fn une_radio_se_range_et_se_montre_comme_une_radio_8135() {
    let st = etat();
    let app = appli(&st);
    let (pl, [t1, t2], fip) = reveil(&st, &app).await;

    let vues = lignes(&app, pl).await;
    assert_eq!(vues.len(), 3, "{vues:?}");
    assert_eq!(vues[0]["id"], t1);
    assert_eq!(vues[1]["id"], t2);
    let radio = &vues[2];
    assert_eq!(radio["source"], "radio");
    assert_eq!(radio["radio"], true);
    assert_eq!(radio["radio_id"], fip);
    assert_eq!(radio["unavailable"], false);
    assert_eq!(radio["title"], "FIP");
    assert!(
        radio["entry_id"].as_i64().is_some(),
        "elle se retire comme une ligne"
    );

    // Retirée de « Mes radios » : elle reste, marquée indisponible.
    RadioRepo::with_backend(st.backend.clone())
        .delete(fip)
        .unwrap();
    let vues = lignes(&app, pl).await;
    assert_eq!(vues.len(), 3);
    assert_eq!(vues[2]["unavailable"], true);
    assert_eq!(vues[2]["title"], "FIP");
}

#[tokio::test]
async fn une_station_inconnue_est_refusee_en_le_disant_8135() {
    let st = etat();
    let app = appli(&st);
    let (s, v) = poste(&app, "/api/v1/playlists", json!({ "name": "Vide" })).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let pl = v["id"].as_i64().unwrap();
    let (s, v) = poste(
        &app,
        &format!("/api/v1/playlists/{pl}/tracks"),
        json!({ "radio_ids": [987654] }),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{v}");
    assert_eq!(v["code"], "playlist_radio_not_found", "{v}");
    assert!(lignes(&app, pl).await.is_empty());
}

/// La lecture enfile les pistes PUIS le flux de la radio — l'adresse que
/// `POST /radios/{id}/play` aurait jouée. Avant, la ligne radio n'existait pas ;
/// posée en titre de service, elle serait entrée avec `source_id` = l'id de
/// la station, qui ne joue rien.
#[tokio::test]
async fn lire_la_playlist_enchaine_les_pistes_puis_le_flux_8135() {
    let st = etat();
    let app = appli(&st);
    let (pl, [t1, t2], _) = reveil(&st, &app).await;
    let zid = tune_core::db::zone_repo::ZoneRepo::with_backend(st.backend.clone())
        .create("Chambre", Some("local"), None)
        .unwrap();

    let (status, corps) = poste(
        &app,
        &format!("/api/v1/zones/{zid}/play"),
        json!({ "playlist_id": pl }),
    )
    .await;
    assert_ne!(status, StatusCode::BAD_REQUEST, "{corps}");

    let file = tune_core::db::play_queue_repo::PlayQueueRepo::with_backend(st.backend.clone())
        .get_ordered(zid)
        .unwrap();
    let vues: Vec<String> = file
        .iter()
        .map(|e| match e.track_id {
            Some(id) => format!("L{id}"),
            None => format!(
                "{}:{}",
                e.source.as_deref().unwrap_or("?"),
                e.source_id.as_deref().unwrap_or("?")
            ),
        })
        .collect();
    assert_eq!(
        vues,
        vec![format!("L{t1}"), format!("L{t2}"), format!("radio:{FIP}")]
    );
}

#[tokio::test]
async fn une_station_retiree_est_sautee_a_la_lecture_8135() {
    let st = etat();
    let app = appli(&st);
    let (pl, [t1, t2], fip) = reveil(&st, &app).await;
    RadioRepo::with_backend(st.backend.clone())
        .delete(fip)
        .unwrap();
    let zid = tune_core::db::zone_repo::ZoneRepo::with_backend(st.backend.clone())
        .create("Chambre", Some("local"), None)
        .unwrap();
    let (status, corps) = poste(
        &app,
        &format!("/api/v1/zones/{zid}/play"),
        json!({ "playlist_id": pl }),
    )
    .await;
    assert_ne!(status, StatusCode::BAD_REQUEST, "{corps}");
    let file = tune_core::db::play_queue_repo::PlayQueueRepo::with_backend(st.backend.clone())
        .get_ordered(zid)
        .unwrap();
    assert_eq!(
        file.iter().map(|e| e.track_id).collect::<Vec<_>>(),
        vec![Some(t1), Some(t2)]
    );
}

#[tokio::test]
async fn l_export_m3u_ecrit_le_flux_et_l_import_range_une_radio_8135() {
    let st = etat();
    let app = appli(&st);
    let (pl, _, fip) = reveil(&st, &app).await;

    let (status, m3u) = lis(&app, &format!("/api/v1/playlists/{pl}/export")).await;
    assert_eq!(status, StatusCode::OK);
    let m3u = m3u.as_str().unwrap().to_string();
    assert!(m3u.contains(&format!("#EXTINF:-1,FIP\n{FIP}\n")), "{m3u}");

    let corps = format!(
        "--borne\r\nContent-Disposition: form-data; name=\"file\"; filename=\"reveil.m3u\"\r\nContent-Type: audio/x-mpegurl\r\n\r\n{m3u}\r\n--borne--\r\n"
    );
    let (status, cree) = reponse(
        &app,
        Request::post("/api/v1/playlists/import/m3u")
            .header("content-type", "multipart/form-data; boundary=borne")
            .body(Body::from(corps))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{cree}");
    let vues = lignes(&app, cree["id"].as_i64().unwrap()).await;
    assert_eq!(vues.len(), 3, "{vues:?}");
    assert_eq!(vues[2]["radio"], true);
    assert_eq!(vues[2]["radio_id"], fip);

    let (status, json) = lis(&app, &format!("/api/v1/playlists/{pl}/export?format=json")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["tracks"][2]["source"], "radio");
    assert_eq!(json["tracks"][2]["stream_url"], FIP);
}
