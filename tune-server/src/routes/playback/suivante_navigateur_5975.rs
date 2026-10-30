//! #5975 — contrat serveur du préchargement navigateur (voir docs).
use super::*;

#[derive(Deserialize)]
pub(super) struct PreparationRequest {
    current_stream_id: String,
}
#[derive(Deserialize)]
pub(super) struct AdoptionRequest {
    current_stream_id: String,
    next_stream_id: String,
    position_ms: i64,
}

fn erreur(e: String) -> axum::response::Response {
    let status = match e.as_str() {
        "zone_not_found" => StatusCode::NOT_FOUND,
        "not_a_browser_zone"
        | "browser_current_changed"
        | "browser_queue_changed"
        | "browser_next_without_stream"
        | "browser_next_not_playing"
        | "browser_next_not_prepared" => StatusCode::CONFLICT,
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    };
    (status, Json(json!({"error": e}))).into_response()
}

pub(super) async fn preparer(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<PreparationRequest>,
) -> axum::response::Response {
    match state
        .orchestrator
        .preparer_suivante_navigateur(zone_id, &body.current_stream_id)
        .await
    {
        Ok(Some(flux)) => Json(json!({
            "next": {"stream_url": flux.url, "stream_id": flux.stream_id,
                "mime_type": flux.mime_type, "duration_ms": flux.duration_ms,
                "title": flux.title}
        }))
        .into_response(),
        Ok(None) => Json(json!({"next": null})).into_response(),
        Err(e) => erreur(e),
    }
}

pub(super) async fn adopter(
    State(state): State<AppState>,
    Path(zone_id): Path<i64>,
    Json(body): Json<AdoptionRequest>,
) -> axum::response::Response {
    let verrou = verrou_du_suivant(zone_id);
    let _decision = verrou.lock().await;
    match state
        .orchestrator
        .adopter_suivante_navigateur(
            zone_id,
            &body.current_stream_id,
            &body.next_stream_id,
            body.position_ms,
        )
        .await
    {
        Ok(()) => Json(json!({"status": "adopted"})).into_response(),
        Err(e) => erreur(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;
    use tune_core::db::{models::Track, track_repo::TrackRepo, zone_repo::ZoneRepo};

    async fn banc() -> (AppState, i64, tempfile::TempDir, String) {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let zone = ZoneRepo::with_backend(state.backend.clone())
            .create("Safari 5975", Some("browser"), None)
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let mut ids = Vec::new();
        for n in 0..3 {
            let path = dir.path().join(format!("piste-{n}.wav"));
            // PCM stéréo muet, une seconde ; aucun appareil ni lecteur lancé.
            let mut wav = b"RIFF".to_vec();
            wav.extend_from_slice(&(36u32 + 176400).to_le_bytes());
            wav.extend_from_slice(b"WAVEfmt ");
            wav.extend_from_slice(&16u32.to_le_bytes());
            wav.extend_from_slice(&1u16.to_le_bytes());
            wav.extend_from_slice(&2u16.to_le_bytes());
            wav.extend_from_slice(&44100u32.to_le_bytes());
            wav.extend_from_slice(&176400u32.to_le_bytes());
            wav.extend_from_slice(&4u16.to_le_bytes());
            wav.extend_from_slice(&16u16.to_le_bytes());
            wav.extend_from_slice(b"data");
            wav.extend_from_slice(&176400u32.to_le_bytes());
            wav.resize(44 + 176400, 0);
            std::fs::write(&path, wav).unwrap();
            let mut track = Track::new(format!("Dark Side {n}"));
            track.file_path = Some(path.to_string_lossy().into_owned());
            track.format = Some("wav".into());
            track.duration_ms = 1000;
            track.sample_rate = Some(44100);
            track.bit_depth = Some(16);
            track.channels = 2;
            ids.push(
                TrackRepo::with_backend(state.backend.clone())
                    .create(&track)
                    .unwrap(),
            );
        }
        PlayQueueRepo::with_backend(state.backend.clone())
            .set_queue(zone, &ids)
            .unwrap();
        state.orchestrator.play_from_queue(zone, 0).await.unwrap();
        let sid = state
            .playback
            .get_state(zone)
            .await
            .now_playing
            .unwrap()
            .stream_id
            .unwrap();
        (state, zone, dir, sid)
    }

    async fn appel(
        state: &AppState,
        zone: i64,
        suffixe: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let app = crate::routes::router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/v1/zones/{zone}/browser-next{suffixe}"))
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    #[tokio::test]
    async fn url_avant_la_fin_puis_adoption_du_meme_flux_5975() {
        let (state, zone, _dir, sid) = banc().await;
        let (status, prepared) = appel(&state, zone, "", json!({"current_stream_id":sid})).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "URL suivante absente avant ended : {prepared}"
        );
        assert!(
            prepared["next"]["stream_url"]
                .as_str()
                .is_some_and(|s| s.contains("/stream/")),
            "le navigateur ne peut pas precharger avant ended : {prepared}"
        );
        assert!(
            state.orchestrator.streamer.session_alive(&sid).await,
            "precharger a ferme le flux courant"
        );
        let before = state.playback.get_state(zone).await;
        assert_eq!(
            before.queue_position, 0,
            "precharger a avance la file avant la fin"
        );
        assert_eq!(
            before.now_playing.unwrap().stream_id.as_deref(),
            Some(sid.as_str())
        );
        let (_, again) = appel(&state, zone, "", json!({"current_stream_id":sid})).await;
        assert_eq!(
            again, prepared,
            "une preparation repetee detruit le flux deja precharge"
        );
        let next = prepared["next"]["stream_id"].as_str().unwrap();
        let (status, response) = appel(
            &state,
            zone,
            "/playing",
            json!({"current_stream_id":sid,"next_stream_id":next,"position_ms":100}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "adoption du flux precharge : {response}"
        );
        let after = state.playback.get_state(zone).await;
        assert_eq!(after.queue_position, 1);
        assert_eq!(
            after.now_playing.unwrap().stream_id.as_deref(),
            Some(next),
            "adoption : le serveur a rouvert un autre flux"
        );
        assert!(
            state
                .orchestrator
                .streamer
                .stream_output_wire(next)
                .await
                .is_some(),
            "le flux adopte a ete detruit"
        );
        let (status, _) = appel(
            &state,
            zone,
            "/playing",
            json!({"current_stream_id":sid,"next_stream_id":next,"position_ms":100}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "une confirmation repetee avance deux fois"
        );
        assert_eq!(state.playback.get_state(zone).await.queue_position, 1);
        let (status, third) = appel(&state, zone, "", json!({"current_stream_id":next})).await;
        assert_eq!(status, StatusCode::OK, "{third}");
        assert_eq!(third["next"]["title"], "Dark Side 2");
        assert!(
            state.orchestrator.streamer.session_alive(next).await,
            "preparer N+2 a ferme le flux N+1 adopte"
        );
    }

    #[tokio::test]
    async fn sonde_et_file_modifiee_ne_sont_pas_adoptees_5975() {
        let (state, zone, _dir, sid) = banc().await;
        let (_, p) = appel(&state, zone, "", json!({"current_stream_id":sid})).await;
        let next = &p["next"]["stream_id"];
        let (status, response) = appel(
            &state,
            zone,
            "/playing",
            json!({"current_stream_id":sid,"next_stream_id":next,"position_ms":0}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(response["error"], "browser_next_not_playing");
        state.playback.update_queue_info(zone, 1, 3).await;
        let (status, p) = appel(&state, zone, "", json!({"current_stream_id":sid})).await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "un suivant manuel en cours prepare N+2 : {p}"
        );
        state.playback.update_queue_info(zone, 0, 3).await;
        let q = PlayQueueRepo::with_backend(state.backend.clone());
        let ids: Vec<_> = q
            .get_ordered(zone)
            .unwrap()
            .iter()
            .rev()
            .map(|e| e.track_id.unwrap())
            .collect();
        q.set_queue(zone, &ids).unwrap();
        let (status, _) = appel(
            &state,
            zone,
            "/playing",
            json!({"current_stream_id":sid,"next_stream_id":next,"position_ms":100}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::CONFLICT,
            "une ancienne URL adopte une nouvelle ligne de file"
        );
        assert_eq!(state.playback.get_state(zone).await.queue_position, 0);
    }

    #[tokio::test]
    async fn fin_de_file_radio_et_autre_sortie_sans_prechargement_5975() {
        let (state, zone, _dir, sid) = banc().await;
        state.playback.update_queue_info(zone, 0, 1).await;
        let (status, p) = appel(&state, zone, "", json!({"current_stream_id":sid})).await;
        assert_eq!(status, StatusCode::OK);
        assert!(p["next"].is_null(), "fin de file : une suivante inventee");
        let mut np = state.playback.get_state(zone).await.now_playing.unwrap();
        np.source = "radio".into();
        state.playback.update_now_playing(zone, np).await;
        state.playback.update_queue_info(zone, 0, 3).await;
        let (_, p) = appel(&state, zone, "", json!({"current_stream_id":sid})).await;
        assert!(p["next"].is_null(), "le direct precharge une ancienne file");
        let other = ZoneRepo::with_backend(state.backend.clone())
            .create("DLNA 5975", Some("dlna"), None)
            .unwrap();
        let (status, p) = appel(&state, other, "", json!({"current_stream_id":"none"})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(p["error"], "not_a_browser_zone");
        let (status, _) = appel(&state, 999999, "", json!({"current_stream_id":"none"})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
    #[tokio::test]
    async fn repetition_et_bannis_suivent_la_decision_automatique_5975() {
        use tune_core::{db::hidden_repo::HiddenRepo, playback::RepeatMode};
        let (state, zone, _dir, sid) = banc().await;
        state.playback.set_repeat(zone, RepeatMode::One).await;
        let (_, p) = appel(&state, zone, "", json!({"current_stream_id":sid})).await;
        assert_eq!(
            p["next"]["title"], "Dark Side 0",
            "repeat-one doit preparer la meme piste"
        );
        state.playback.set_repeat(zone, RepeatMode::Off).await;
        let q = PlayQueueRepo::with_backend(state.backend.clone());
        let banned = q.get_at(zone, 1).unwrap().unwrap().track_id.unwrap();
        let profile =
            tune_core::db::hidden_repo::profil_des_bannis_de_la_zone(&state.backend, None);
        HiddenRepo::with_backend(state.backend.clone())
            .ban_track(profile, banned)
            .unwrap();
        let (_, p) = appel(&state, zone, "", json!({"current_stream_id":sid})).await;
        assert_eq!(
            p["next"]["title"], "Dark Side 2",
            "une piste bannie est prechargee au lieu de la suivante jouable"
        );
        state.playback.stop(zone).await;
        let (status, p) = appel(&state, zone, "", json!({"current_stream_id":sid})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(p["error"], "browser_current_changed");
    }
}
