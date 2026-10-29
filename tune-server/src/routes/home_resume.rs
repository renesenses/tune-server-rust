use super::*;
use crate::routes::active_profile::ActiveProfile;
use axum::extract::Path;
use axum::response::{IntoResponse, Response};
use tune_core::db::listening_resume as resumes;

#[derive(Deserialize)]
pub(super) struct ResumeRequest {
    resume_key: String,
}

pub(super) async fn resume_listening(
    State(state): State<AppState>,
    profile: ActiveProfile,
    Path(zone_id): Path<i64>,
    Json(body): Json<ResumeRequest>,
) -> Result<Response, AppError> {
    let _zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::not_found("zone"))?;
    let mut r = resumes::load(&state.backend, &body.resume_key)
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::not_found("reprise"))?;
    // C'est un nouveau geste : un seek récent du même titre ne doit pas
    // conserver son ancienne position lors du rejeu depuis le début.
    state.playback.oublier_le_deplacement(zone_id).await;
    if r.context_type == "podcast" {
        let Some(tune_core::db::play_queue_repo::QueueInput::Streaming {
            source_id,
            title,
            artist,
            cover_url,
            duration_ms,
            ..
        }) = r.items.first()
        else {
            return Err(AppError::bad_request("épisode de reprise absent"));
        };
        let seconds = SettingsRepo::with_backend(state.backend.clone())
            .get(resumes::REWIND_KEY)
            .ok()
            .flatten()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(resumes::DEFAULT_REWIND);
        let request = super::super::podcasts::PlayEpisodeRequest {
            audio_url: source_id.clone(),
            title: Some(title.clone()),
            podcast_name: Some(artist.clone()),
            podcast_id: Some(r.context_id.clone()),
            cover_url: cover_url.clone(),
            duration_ms: Some((*duration_ms).max(0) as u64),
            seek_ms: Some(resumes::rewind(r.position_ms, seconds)),
        };
        let response = super::super::podcasts::play_episode(
            State(state.clone()),
            Path(zone_id),
            Json(request),
        )
        .await
        .into_response();
        return if response.status().is_success() {
            Ok(
                Json(super::super::playback::build_zone_json(&state, zone_id).await)
                    .into_response(),
            )
        } else {
            Ok(response)
        };
    }
    if r.context_type == "playlist" && r.source == "local" {
        let id = r
            .context_id
            .parse::<i64>()
            .map_err(|_| AppError::bad_request("playlist"))?;
        let repo = tune_core::db::playlist_repo::PlaylistRepo::with_backend(state.backend.clone());
        if let Err(response) =
            crate::routes::playlists::owned_or_404_response(&repo, id, profile.id())
        {
            return Ok(response);
        }
    }
    complete_single_container(&state, &mut r).await?;
    let seek_ms = if r.context_type == "track" {
        r.position_ms.max(0) as u64
    } else {
        0
    };
    let shuffled = r.shuffled;
    resumes::restore(&state.backend, zone_id, &r).map_err(AppError::bad_request)?;
    state
        .playback
        .set_session_profile(zone_id, Some(profile.id()))
        .await;
    state
        .playback
        .set_session_context(
            zone_id,
            Some(r.context_type),
            Some(r.context_id),
            Some(r.source),
            Some(r.title),
            r.cover_path,
        )
        .await;
    // L'ordre est déjà matérialisé dans la file : aucun nouveau tirage.
    state.playback.set_shuffle(zone_id, false).await;
    state
        .playback
        .update_queue_info(zone_id, r.position as i64, r.items.len() as i64)
        .await;
    let result = state
        .orchestrator
        .replay_context_from_queue(zone_id, r.position as i64, seek_ms)
        .await
        .map_err(AppError::internal)?;
    if let Some(error) = result.sortie_refusee() {
        return Err(AppError::internal(error));
    }
    if shuffled {
        resumes::mark_current_shuffled(&state.backend, zone_id).map_err(AppError::internal)?;
    }
    super::super::playback::persist_queue_async(&state, zone_id);
    Ok(Json(super::super::playback::build_zone_json(&state, zone_id).await).into_response())
}

#[cfg(test)]
struct LectureFactice(i64);

#[cfg(test)]
impl Drop for LectureFactice {
    fn drop(&mut self) {
        tune_core::taches_de_fond::priorite::oublier_la_zone(self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
    use tune_core::orchestrator::ContexteEcoute;

    #[test]
    fn reprise_fabien_7280_contexte_enregistre_puis_continue_listening() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
            .create("Muette", None, None)
            .unwrap();
        let q = PlayQueueRepo::with_backend(state.backend.clone());
        q.append(
            zone,
            &[QueueInput::Streaming {
                source: "qobuz".into(),
                source_id: "9140031".into(),
                title: "Titre démarré".into(),
                artist: "Artiste".into(),
                album: Some("Album".into()),
                cover_url: Some("album.jpg".into()),
                duration_ms: 100000,
                track_number: None,
                disc_number: None,
                album_ref: None,
                artist_ref: None,
            }],
        )
        .unwrap();
        q.set_current_pos(zone, 0).unwrap();
        let c = ContexteEcoute {
            nature: Some("album"),
            id: Some("album-service"),
            rang: Some(0),
            service: Some("qobuz"),
            titre: Some("Album"),
            pochette: Some("album.jpg"),
        };
        resumes::started(
            &state.backend,
            zone,
            &c,
            "qobuz",
            Some("9140031"),
            "Titre démarré",
            Some("Album"),
            Some("piste.jpg"),
        )
        .unwrap();
        let items = fetch_continue_listening(&state, 10, None)
            .unwrap_or_else(|_| panic!("continue-listening doit répondre"));
        assert_eq!(
            items.len(),
            1,
            "le contexte démarré doit sortir de continue-listening"
        );
        assert_eq!(items[0]["last_track_title"], "Titre démarré");
        assert_eq!(items[0]["cover_path"], "album.jpg");
        assert_eq!(items[0]["source"], "qobuz");
        assert!(items[0]["resume_key"].as_str().is_some());
    }

    #[test]
    fn reprise_fabien_7280_services_memes_regles_et_meme_tirage() {
        for source in ["qobuz", "tidal"] {
            for nature in ["album", "playlist"] {
                let state = AppState::new(":memory:", 0, Default::default()).unwrap();
                let zones = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
                let zone = zones.create("Origine", None, None).unwrap();
                let target = zones.create("Reprise", None, None).unwrap();
                let q = PlayQueueRepo::with_backend(state.backend.clone());
                let inputs: Vec<_> = ["trois", "un", "deux"]
                    .into_iter()
                    .map(|id| QueueInput::Streaming {
                        source: source.into(),
                        source_id: id.into(),
                        title: id.into(),
                        artist: "Artiste".into(),
                        album: None,
                        cover_url: Some("piste.jpg".into()),
                        duration_ms: 100000,
                        track_number: None,
                        disc_number: None,
                        album_ref: None,
                        artist_ref: None,
                    })
                    .collect();
                q.append(zone, &inputs).unwrap();
                q.set_current_pos(zone, 1).unwrap();
                resumes::started(
                    &state.backend,
                    zone,
                    &ContexteEcoute {
                        nature: Some(nature),
                        id: Some("conteneur-non-numerique"),
                        rang: None,
                        service: Some(source),
                        titre: Some("Conteneur"),
                        pochette: Some("service.jpg"),
                    },
                    source,
                    Some("un"),
                    "un",
                    None,
                    Some("piste.jpg"),
                )
                .unwrap();
                let row = resumes::recent(&state.backend, Some(zone), 10)
                    .unwrap()
                    .remove(0);
                assert!(row.shuffled, "un tirage de service doit rester mélangé");
                assert_eq!(row.position, 1);
                assert_eq!(row.cover_path.as_deref(), Some("service.jpg"));
                assert_eq!(row.source, source);
                assert_eq!(row.context_type, nature);
                resumes::restore(&state.backend, target, &row).unwrap();
                let restored = q.get_ordered(target).unwrap();
                assert_eq!(
                    restored
                        .iter()
                        .map(|e| e.source_id.as_deref().unwrap())
                        .collect::<Vec<_>>(),
                    ["trois", "un", "deux"],
                    "le même tirage doit survivre sans remplacement par un ordre canonique"
                );
                assert!(restored[1].is_current);
                assert_eq!(
                    restored
                        .iter()
                        .skip(row.position)
                        .map(|e| e.source_id.as_deref().unwrap())
                        .collect::<Vec<_>>(),
                    ["un", "deux"],
                    "rejouer le dernier titre puis la suite, sans réentendre trois"
                );
                assert!(
                    restored
                        .iter()
                        .all(|e| e.track_id.is_none() && e.source.as_deref() == Some(source)),
                    "aucun identifiant de service ne devient local"
                );
                let response = fetch_continue_listening(&state, 10, Some(zone))
                    .unwrap_or_else(|_| panic!("widget"));
                assert_eq!(response[0]["cover_path"], "service.jpg");
                assert_eq!(response[0]["last_track_title"], "un");
            }
        }
    }

    #[test]
    fn reprise_fabien_7280_podcast_position_et_recul() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
            .create("Muette", None, None)
            .unwrap();
        let np = tune_core::playback::NowPlaying {
            title: "Épisode".into(),
            album_title: Some("Podcast".into()),
            source: "podcast".into(),
            source_id: Some("https://podcast.invalid/episode.mp3".into()),
            cover_path: Some("podcast.jpg".into()),
            ..Default::default()
        };
        resumes::podcast_started(&state.backend, zone, &np, 43210).unwrap();
        resumes::podcast_position(
            &state.backend,
            zone,
            np.source_id.as_deref().unwrap(),
            56789,
        )
        .unwrap();
        let items = fetch_continue_listening(&state, 10, Some(zone))
            .unwrap_or_else(|_| panic!("continue-listening doit répondre"));
        assert_eq!(
            items[0]["title"], "Podcast",
            "la vignette doit nommer le podcast"
        );
        assert_eq!(
            items[0]["position_ms"], 56789,
            "la dernière position doit être persistée"
        );
        assert_eq!(
            resumes::rewind(56789, 10),
            46789,
            "la reprise doit reculer de X secondes"
        );
        assert_eq!(
            resumes::rewind(5000, 10),
            0,
            "le recul doit être borné au début de l'épisode"
        );
        assert_eq!(resumes::rewind(56789, 0), 56789);
    }
}

#[cfg(test)]
mod route_tests {
    use super::*;
    #[tokio::test]
    async fn reprise_fabien_7280_la_route_recale_le_vrai_controle_de_sortie_factice() {
        let _pauses = crate::pauses_de_fond_tests::isoler().await;
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let mock =
            tune_core::outputs::mock::MockOutput::new("mock-reprise", "Muette").with_type("dlna");
        state.outputs.lock().await.register(Box::new(mock));
        let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
            .create("Muette", Some("dlna"), Some("mock-reprise"))
            .unwrap();
        let _lecture = LectureFactice(zone);
        SettingsRepo::with_backend(state.backend.clone())
            .set(resumes::REWIND_KEY, "17")
            .unwrap();
        let np = tune_core::playback::NowPlaying {
            title: "Épisode".into(),
            album_title: Some("Podcast".into()),
            source: "podcast".into(),
            source_id: Some("https://podcast.invalid/episode.mp3".into()),
            duration_ms: 100000,
            ..Default::default()
        };
        resumes::podcast_started(&state.backend, zone, &np, 56000).unwrap();
        let key = resumes::recent(&state.backend, None, 10).unwrap()[0]
            .resume_key
            .clone();
        let response = resume_listening(
            State(state.clone()),
            ActiveProfile(1),
            Path(zone),
            Json(ResumeRequest { resume_key: key }),
        )
        .await
        .unwrap_or_else(|_| panic!("la reprise doit répondre"));
        assert!(
            response.status().is_success(),
            "la reprise podcast doit réussir sans son"
        );
        let output = state.outputs.lock().await.get("mock-reprise").unwrap();
        let output = output.lock().await;
        let mock = output
            .as_any()
            .downcast_ref::<tune_core::outputs::mock::MockOutput>()
            .unwrap();
        assert_eq!(
            mock.seek_calls(),
            vec![39000],
            "la route doit reprendre 17 secondes avant la position mémorisée"
        );
    }
}

/// Un titre seul ajouté depuis une fiche garde son origine. Sa reprise
/// reconstruit aussi la suite du conteneur, absente de la file d'origine.
async fn complete_single_container(
    state: &AppState,
    r: &mut resumes::Resume,
) -> Result<(), AppError> {
    use tune_core::db::play_queue_repo::QueueInput;
    if r.items.is_empty() || r.shuffled || !matches!(r.context_type.as_str(), "album" | "playlist")
    {
        return Ok(());
    }
    let current = r
        .items
        .get(r.position)
        .ok_or_else(|| AppError::bad_request("position de reprise absente"))?
        .clone();
    let full = if r.source == "local" {
        let id = r
            .context_id
            .parse::<i64>()
            .map_err(|_| AppError::bad_request("conteneur"))?;
        if r.context_type == "album" {
            tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone())
                .list_by_album(id)
                .map_err(|e| AppError::internal(e.to_string()))?
                .into_iter()
                .filter_map(|t| t.id.map(|track_id| QueueInput::Local { track_id }))
                .collect()
        } else {
            tune_core::db::playlist_repo::PlaylistRepo::with_backend(state.backend.clone())
                .get_entries(id)
                .map_err(AppError::internal)?
                .into_iter()
                .filter_map(|e| match e.content {
                    tune_core::db::playlist_repo::EntryContent::Local(track_id) => {
                        Some(QueueInput::Local { track_id })
                    }
                    tune_core::db::playlist_repo::EntryContent::Service(s) => {
                        Some(QueueInput::Streaming {
                            source: s.source,
                            source_id: s.source_id,
                            title: s.title,
                            artist: s.artist.unwrap_or_default(),
                            album: s.album,
                            cover_url: s.cover_url,
                            duration_ms: s.duration_ms.unwrap_or(0),
                            track_number: None,
                            disc_number: None,
                            album_ref: s.album_source_id,
                            artist_ref: None,
                        })
                    }
                    _ => None,
                })
                .collect()
        }
    } else {
        let services = state.services.lock().await;
        let svc = services
            .get(&r.source)
            .ok_or_else(|| AppError::bad_request("service de reprise absent"))?;
        let svc = svc.read().await;
        let tracks = if r.context_type == "album" {
            svc.get_album_tracks(&r.context_id).await
        } else {
            svc.get_playlist_tracks(&r.context_id).await
        }
        .map_err(|e| AppError::internal(e.to_string()))?;
        tracks
            .into_iter()
            .map(|t| QueueInput::Streaming {
                source: r.source.clone(),
                source_id: t.id,
                title: t.title,
                artist: t.artist,
                album: t.album,
                cover_url: t.cover_path,
                duration_ms: t.duration_ms as i64,
                track_number: t.track_number.map(i64::from),
                disc_number: t.disc_number.map(i64::from),
                album_ref: t.album_id,
                artist_ref: t.artist_id,
            })
            .collect()
    };
    let full: Vec<QueueInput> = full;
    let matches_current = |item: &QueueInput| match (item, &current) {
        (QueueInput::Local { track_id: a }, QueueInput::Local { track_id: b }) => a == b,
        (
            QueueInput::Streaming {
                source: a,
                source_id: ai,
                ..
            },
            QueueInput::Streaming {
                source: b,
                source_id: bi,
                ..
            },
        ) => a == b && ai == bi,
        _ => false,
    };
    // Une playlist peut contenir deux fois le même titre : conserver le
    // rang mémorisé quand le conteneur le confirme, avant de chercher ailleurs.
    let position = if full.get(r.position).is_some_and(&matches_current) {
        Some(r.position)
    } else {
        full.iter().position(matches_current)
    }
    .ok_or_else(|| AppError::not_found("dernier titre absent du conteneur"))?;
    r.items = full;
    r.position = position;
    Ok(())
}

#[cfg(test)]
mod config_tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn reprise_fabien_7280_reglage_podcast_defaut_patch_et_refus() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let get = crate::routes::router(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/system/config")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body: Value =
            serde_json::from_slice(&to_bytes(get.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(
            body[resumes::REWIND_KEY],
            10,
            "le serveur doit publier les 10 secondes proposées par défaut"
        );
        for (value, ok) in [
            (json!(17), true),
            (json!(0), true),
            (json!(300), true),
            (json!(-1), false),
            (json!(301), false),
            (json!(1.5), false),
            (json!("10"), false),
        ] {
            let response = crate::routes::router(state.clone())
                .oneshot(
                    Request::builder()
                        .method("PATCH")
                        .uri("/api/v1/system/config")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({(resumes::REWIND_KEY): value}).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status().is_success(),
                ok,
                "validation du recul podcast pour {value}"
            );
        }
        assert_eq!(
            SettingsRepo::with_backend(state.backend.clone())
                .get(resumes::REWIND_KEY)
                .unwrap()
                .as_deref(),
            Some("300"),
            "un réglage refusé ne doit pas être écrit"
        );
    }
}

#[cfg(test)]
mod single_origin_tests {
    use super::*;
    use tune_core::db::backend::ToSqlValue;
    use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
    use tune_core::orchestrator::ContexteEcoute;

    #[tokio::test]
    async fn reprise_fabien_7280_un_titre_ajoute_depuis_un_album_retrouve_toute_sa_suite() {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let db = &state.backend;
        db.execute("INSERT INTO albums (title) VALUES ('Album demandé')", &[])
            .unwrap();
        let album = db.last_insert_rowid();
        let mut ids = Vec::new();
        for i in 1..=3_i64 {
            let path = format!("fixture-reprise-{i}.flac");
            db.execute("INSERT INTO tracks (title, file_path, album_id, track_number) VALUES ('Titre', ?, ?, ?)", &[&path as &dyn ToSqlValue, &album, &i]).unwrap();
            ids.push(db.last_insert_rowid());
        }
        let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(db.clone())
            .create("Muette", None, None)
            .unwrap();
        let q = PlayQueueRepo::with_backend(db.clone());
        q.append(zone, &[QueueInput::Local { track_id: ids[1] }])
            .unwrap();
        q.set_current_pos(zone, 0).unwrap();
        let album_id = album.to_string();
        resumes::started(
            db,
            zone,
            &ContexteEcoute {
                nature: Some("album"),
                id: Some(&album_id),
                rang: Some(0),
                service: Some("local"),
                titre: None,
                pochette: None,
            },
            "local",
            None,
            "Titre",
            Some("Album demandé"),
            None,
        )
        .unwrap();
        let mut r = resumes::recent(db, None, 10).unwrap().remove(0);
        complete_single_container(&state, &mut r)
            .await
            .unwrap_or_else(|_| panic!("l'album doit se reconstruire"));
        assert_eq!(
            r.position, 1,
            "la reprise doit rejouer le titre ajouté, pas le premier titre de l'album"
        );
        assert_eq!(
            r.items.len(),
            3,
            "la reprise doit reconstruire la suite de l'album même si un seul titre avait été ajouté"
        );
        resumes::restore(db, zone, &r).unwrap();
        assert_eq!(
            q.get_ordered(zone)
                .unwrap()
                .iter()
                .map(|e| e.track_id.unwrap())
                .collect::<Vec<_>>(),
            ids
        );
        r.items = vec![
            QueueInput::Local { track_id: ids[1] },
            QueueInput::Local { track_id: ids[0] },
        ];
        r.position = 0;
        r.shuffled = true;
        complete_single_container(&state, &mut r)
            .await
            .unwrap_or_else(|_| panic!("ordre mémorisé"));
        assert_eq!(
            r.items.len(),
            2,
            "une file mélangée doit garder son ordre mémorisé"
        );
        r.shuffled = false;
        complete_single_container(&state, &mut r)
            .await
            .unwrap_or_else(|_| panic!("sélection partielle"));
        assert_eq!(
            r.items.len(),
            3,
            "plusieurs titres sélectionnés doivent aussi retrouver toute la suite de l'album"
        );
        assert_eq!(r.position, 1);
        let playlists = tune_core::db::playlist_repo::PlaylistRepo::with_backend(db.clone());
        let playlist = playlists.create("Doublons", None, 1).unwrap();
        playlists
            .add_tracks(playlist, &[ids[1], ids[0], ids[1]], None)
            .unwrap();
        r.context_type = "playlist".into();
        r.context_id = playlist.to_string();
        r.items = vec![
            QueueInput::Local { track_id: ids[1] },
            QueueInput::Local { track_id: ids[0] },
            QueueInput::Local { track_id: ids[1] },
        ];
        r.position = 2;
        complete_single_container(&state, &mut r)
            .await
            .unwrap_or_else(|_| panic!("playlist avec doublon"));
        assert_eq!(
            r.position, 2,
            "le dernier titre doit rester la seconde occurrence de la playlist"
        );
        resumes::restore(db, zone, &r).unwrap();
        assert!(q.get_ordered(zone).unwrap()[2].is_current);
    }
}

#[cfg(test)]
mod replay_tests {
    use super::*;
    use std::sync::Arc;
    use tune_core::db::play_queue_repo::{PlayQueueRepo, QueueInput};
    use tune_core::orchestrator::ContexteEcoute;
    use tune_core::source_url::{FournisseurDUrl, RefusDUrl, UrlFournie};
    struct SourceMuette;
    #[async_trait::async_trait]
    impl FournisseurDUrl for SourceMuette {
        async fn url(&self, _: i64, _: &str) -> Result<UrlFournie, RefusDUrl> {
            Ok(UrlFournie {
                url: "http://127.0.0.1:9/reprise".into(),
                media_format: Some("flac".into()),
            })
        }
    }

    #[tokio::test]
    async fn reprise_fabien_7280_la_route_rejoue_le_dernier_titre_depuis_zero_meme_apres_un_seek() {
        let _pauses = crate::pauses_de_fond_tests::isoler().await;
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        state
            .orchestrator
            .sources_url()
            .inscrire("circle", Arc::new(SourceMuette));
        state.outputs.lock().await.register(Box::new(
            tune_core::outputs::mock::MockOutput::new("mock-replay", "Muette").with_type("dlna"),
        ));
        let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
            .create("Muette", Some("dlna"), Some("mock-replay"))
            .unwrap();
        let _lecture = LectureFactice(zone);
        let q = PlayQueueRepo::with_backend(state.backend.clone());
        let input = |id: &str| QueueInput::Streaming {
            source: "circle".into(),
            source_id: id.into(),
            title: id.into(),
            artist: "Artiste".into(),
            album: None,
            cover_url: None,
            duration_ms: 100000,
            track_number: None,
            disc_number: None,
            album_ref: None,
            artist_ref: None,
        };
        q.append(zone, &[input("un"), input("deux")]).unwrap();
        q.set_current_pos(zone, 1).unwrap();
        resumes::started(
            &state.backend,
            zone,
            &ContexteEcoute {
                nature: Some("playlist"),
                id: Some("p42"),
                rang: None,
                service: Some("circle"),
                titre: Some("Playlist"),
                pochette: None,
            },
            "circle",
            Some("deux"),
            "deux",
            None,
            None,
        )
        .unwrap();
        let key = resumes::recent(&state.backend, None, 10).unwrap()[0]
            .resume_key
            .clone();
        state
            .playback
            .play(
                zone,
                tune_core::playback::NowPlaying {
                    title: "deux".into(),
                    source: "circle".into(),
                    source_id: Some("deux".into()),
                    duration_ms: 100000,
                    ..Default::default()
                },
            )
            .await;
        state.playback.seek(zone, 42000).await;
        let response = resume_listening(
            State(state.clone()),
            ActiveProfile(1),
            Path(zone),
            Json(ResumeRequest { resume_key: key }),
        )
        .await
        .unwrap_or_else(|_| panic!("la reprise doit répondre"));
        assert!(response.status().is_success());
        let s = state.playback.get_state(zone).await;
        assert_eq!(
            s.position_ms, 0,
            "la reprise doit rejouer le dernier titre depuis zéro, pas conserver son seek récent"
        );
        assert_eq!(
            s.now_playing.unwrap().source_id.as_deref(),
            Some("deux"),
            "la reprise doit rejouer le dernier titre démarré, pas la tête de la file"
        );
        assert_eq!(s.queue_position, 1);
        assert_eq!(
            q.get_ordered(zone).unwrap().len(),
            2,
            "la reprise doit reconstruire la file du contexte"
        );
    }
    #[tokio::test]
    async fn reprise_fabien_7280_un_titre_seul_reprend_sa_position_memorisee() {
        let _pauses = crate::pauses_de_fond_tests::isoler().await;
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        state
            .orchestrator
            .sources_url()
            .inscrire("circle", Arc::new(SourceMuette));
        state.outputs.lock().await.register(Box::new(
            tune_core::outputs::mock::MockOutput::new("mock-replay", "Muette").with_type("dlna"),
        ));
        let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
            .create("Muette", Some("dlna"), Some("mock-replay"))
            .unwrap();
        let _lecture = LectureFactice(zone);
        let q = PlayQueueRepo::with_backend(state.backend.clone());
        let input = |id: &str| QueueInput::Streaming {
            source: "circle".into(),
            source_id: id.into(),
            title: id.into(),
            artist: "Artiste".into(),
            album: None,
            cover_url: None,
            duration_ms: 100000,
            track_number: None,
            disc_number: None,
            album_ref: None,
            artist_ref: None,
        };
        q.append(zone, &[input("un"), input("deux")]).unwrap();
        q.set_current_pos(zone, 1).unwrap();
        resumes::started(
            &state.backend,
            zone,
            &ContexteEcoute {
                nature: Some("track"),
                id: Some("deux"),
                rang: None,
                service: Some("circle"),
                titre: Some("Playlist"),
                pochette: None,
            },
            "circle",
            Some("deux"),
            "deux",
            None,
            None,
        )
        .unwrap();
        let key = resumes::recent(&state.backend, None, 10).unwrap()[0]
            .resume_key
            .clone();
        state
            .playback
            .play(
                zone,
                tune_core::playback::NowPlaying {
                    title: "deux".into(),
                    source: "circle".into(),
                    source_id: Some("deux".into()),
                    duration_ms: 100000,
                    ..Default::default()
                },
            )
            .await;
        state.playback.seek(zone, 42000).await;
        let played = state.playback.get_state(zone).await;
        resumes::track_position(&state.backend, &played).unwrap();
        let saved = resumes::load(&state.backend, &key).unwrap().unwrap();
        assert_eq!(
            saved.position_ms, 42000,
            "la position du titre seul doit être persistée"
        );
        assert_eq!(
            saved.items.len(),
            1,
            "le titre seul ne reprend pas les voisins"
        );
        state.playback.seek(zone, 17000).await;
        let response = resume_listening(
            State(state.clone()),
            ActiveProfile(1),
            Path(zone),
            Json(ResumeRequest { resume_key: key }),
        )
        .await
        .unwrap_or_else(|_| panic!("la reprise doit répondre"));
        assert!(response.status().is_success());
        let s = state.playback.get_state(zone).await;
        assert_eq!(
            s.position_ms, 42000,
            "le titre seul reprend la position mémorisée, sans recul et sans seek récent"
        );
        assert_eq!(
            s.now_playing.unwrap().source_id.as_deref(),
            Some("deux"),
            "la reprise doit rejouer le dernier titre démarré, pas la tête de la file"
        );
        assert_eq!(s.queue_position, 0);
        assert_eq!(
            q.get_ordered(zone).unwrap().len(),
            1,
            "la reprise du titre seul exclut les titres voisins"
        );
    }
}

#[cfg(test)]
mod server_shuffle_tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;
    use tune_core::db::backend::ToSqlValue;

    #[tokio::test]
    async fn reprise_fabien_7280_le_menu_album_aleatoire_memorise_son_ordre() {
        let _pauses = crate::pauses_de_fond_tests::isoler().await;
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        state.outputs.lock().await.register(Box::new(
            tune_core::outputs::mock::MockOutput::new("mock-album-shuffle", "Muette")
                .with_type("dlna"),
        ));
        let zone = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone())
            .create("Muette", Some("dlna"), Some("mock-album-shuffle"))
            .unwrap();
        let _lecture = LectureFactice(zone);
        state
            .backend
            .execute("INSERT INTO albums (title) VALUES ('Album mélangé')", &[])
            .unwrap();
        let album = state.backend.last_insert_rowid();
        let dir = tempfile::tempdir().unwrap();
        let mut wav =
            tune_core::audio::wav::build_wav_header_with_data_size(2, 44100, 16, 16).to_vec();
        wav.extend_from_slice(&[0; 16]);
        for i in 1..=2_i64 {
            let path = dir.path().join(format!("silence-{i}.wav"));
            std::fs::write(&path, &wav).unwrap();
            let path = path.to_string_lossy().into_owned();
            let title = format!("Titre {i}");
            state.backend.execute("INSERT INTO tracks (title, file_path, album_id, track_number, duration_ms, format, sample_rate, bit_depth, channels) VALUES (?, ?, ?, ?, 1000, 'wav', 44100, 16, 2)", &[&title as &dyn ToSqlValue, &path, &album, &i]).unwrap();
        }
        let response = crate::routes::router(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!(
                        "/api/v1/playback/shuffle-all?zone_id={zone}&album_id={album}"
                    ))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(response.status().is_success());
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(
            body["output_sent"], true,
            "la sortie factice doit accepter le titre"
        );
        let rows = resumes::recent(&state.backend, Some(zone), 10).unwrap();
        assert_eq!(rows.len(), 1, "l'album réellement démarré doit être suivi");
        assert_eq!(rows[0].context_type, "album");
        assert_eq!(rows[0].context_id, album.to_string());
        assert!(
            rows[0].shuffled,
            "le menu album aléatoire doit conserver le tirage mémorisé sans shuffle de zone"
        );
        assert_eq!(rows[0].items.len(), 2);
        assert!(!state.playback.get_state(zone).await.shuffle);
    }
}
