use std::sync::Arc;

use serde_json::{Value, json};
use tracing::{info, warn};

use crate::db::album_repo::AlbumRepo;
use crate::db::artist_repo::ArtistRepo;
use crate::db::backend::DbBackend;
use crate::db::play_queue_repo::PlayQueueRepo;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::orchestrator::PlaybackOrchestrator;
use crate::playback::PlaybackManager;

/// Executes tool calls from the AI assistant against the real server state.
pub struct ToolExecutor {
    db: Arc<dyn DbBackend>,
    orchestrator: Arc<PlaybackOrchestrator>,
    playback: Arc<PlaybackManager>,
    /// The zone_id targeted by the current conversation turn.
    zone_id: i64,
}

impl ToolExecutor {
    pub fn with_backend(
        db: Arc<dyn DbBackend>,
        orchestrator: Arc<PlaybackOrchestrator>,
        playback: Arc<PlaybackManager>,
        zone_id: i64,
    ) -> Self {
        Self {
            db,
            orchestrator,
            playback,
            zone_id,
        }
    }

    pub fn zone_id(&self) -> i64 {
        self.zone_id
    }

    pub fn set_zone_id(&mut self, zone_id: i64) {
        self.zone_id = zone_id;
    }

    pub async fn execute(&mut self, tool_name: &str, input: Value) -> Value {
        info!(tool = %tool_name, zone = self.zone_id, "ai_tool_execute");
        match tool_name {
            "play_album" => self.play_album(input).await,
            "play_track" => self.play_track(input).await,
            "search_library" => self.search_library(input),
            "add_to_queue" => self.add_to_queue(input),
            "pause" => self.pause().await,
            "resume" => self.resume().await,
            "set_volume" => self.set_volume(input).await,
            "next_track" => self.next_track().await,
            "now_playing" => self.now_playing().await,
            "list_zones" => self.list_zones(),
            "set_zone" => self.set_zone(input),
            _ => {
                warn!(tool = %tool_name, "ai_unknown_tool");
                json!({ "error": format!("unknown tool: {tool_name}") })
            }
        }
    }

    async fn play_album(&self, input: Value) -> Value {
        let album_name = input
            .get("album_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let album_repo = AlbumRepo::with_backend(self.db.clone());
        let albums = album_repo.search(album_name, 5).unwrap_or_default();

        if albums.is_empty() {
            return json!({ "error": "no album found", "query": album_name });
        }

        let album = &albums[0];
        let album_id = match album.id {
            Some(id) => id,
            None => return json!({ "error": "album has no ID" }),
        };

        // Get first track of the album to start playback
        let track_repo = TrackRepo::with_backend(self.db.clone());
        let tracks = track_repo.list_by_album(album_id).unwrap_or_default();
        if tracks.is_empty() {
            return json!({
                "error": "album has no tracks",
                "album": album.title,
            });
        }

        let first_track = &tracks[0];
        let device_id = self.get_zone_device_id();

        let req = crate::orchestrator::PlayRequest {
            zone_id: self.zone_id,
            output_device_id: device_id,
            track_id: first_track.id,
            source: None,
            source_id: None,
            title: Some(first_track.title.clone()),
            artist_name: first_track.artist_name.clone(),
            album_title: first_track.album_title.clone(),
            cover_url: first_track.cover_path.clone(),
            duration_ms: Some(first_track.duration_ms),
            seek_ms: None,
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: None,
            disc_number: None,
            album_ref: None,
        };

        // Queue remaining tracks
        if tracks.len() > 1 {
            let queue_repo = PlayQueueRepo::with_backend(self.db.clone());
            let track_ids: Vec<i64> = tracks.iter().filter_map(|t| t.id).collect();
            queue_repo.add_tracks(self.zone_id, &track_ids, None).ok();
        }

        match self.orchestrator.play(req).await {
            Ok(_result) => json!({
                "status": "playing",
                "album": album.title,
                "artist": album.artist_name,
                "track_count": tracks.len(),
                "first_track": first_track.title,
            }),
            Err(e) => json!({ "error": format!("playback failed: {e}") }),
        }
    }

    async fn play_track(&self, input: Value) -> Value {
        let track_name = input
            .get("track_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let track_repo = TrackRepo::with_backend(self.db.clone());
        let tracks = track_repo.search(track_name, 5).unwrap_or_default();

        if tracks.is_empty() {
            return json!({ "error": "no track found", "query": track_name });
        }

        let track = &tracks[0];
        let device_id = self.get_zone_device_id();

        let req = crate::orchestrator::PlayRequest {
            zone_id: self.zone_id,
            output_device_id: device_id,
            track_id: track.id,
            source: None,
            source_id: None,
            title: Some(track.title.clone()),
            artist_name: track.artist_name.clone(),
            album_title: track.album_title.clone(),
            cover_url: track.cover_path.clone(),
            duration_ms: Some(track.duration_ms),
            seek_ms: None,
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: None,
            disc_number: None,
            album_ref: None,
        };

        match self.orchestrator.play(req).await {
            Ok(_result) => json!({
                "status": "playing",
                "track": track.title,
                "artist": track.artist_name,
                "album": track.album_title,
                "track_id": track.id,
            }),
            Err(e) => json!({ "error": format!("playback failed: {e}") }),
        }
    }

    fn search_library(&self, input: Value) -> Value {
        let query = input.get("query").and_then(|v| v.as_str()).unwrap_or("");

        let limit = 10;

        let artist_repo = ArtistRepo::with_backend(self.db.clone());
        let album_repo = AlbumRepo::with_backend(self.db.clone());
        let track_repo = TrackRepo::with_backend(self.db.clone());

        let artists = artist_repo.search(query, limit).unwrap_or_default();
        let albums = album_repo.search(query, limit).unwrap_or_default();
        let tracks = track_repo.search(query, limit).unwrap_or_default();

        let artist_results: Vec<Value> = artists
            .iter()
            .map(|a| {
                json!({
                    "id": a.id,
                    "name": a.name,
                })
            })
            .collect();

        let album_results: Vec<Value> = albums
            .iter()
            .map(|a| {
                json!({
                    "id": a.id,
                    "title": a.title,
                    "artist": a.artist_name,
                    "year": a.year,
                    "genre": a.genre,
                })
            })
            .collect();

        let track_results: Vec<Value> = tracks
            .iter()
            .map(|t| {
                json!({
                    "id": t.id,
                    "title": t.title,
                    "artist": t.artist_name,
                    "album": t.album_title,
                    "duration_ms": t.duration_ms,
                })
            })
            .collect();

        json!({
            "artists": artist_results,
            "albums": album_results,
            "tracks": track_results,
        })
    }

    fn add_to_queue(&self, input: Value) -> Value {
        let track_id = match input.get("track_id").and_then(|v| v.as_i64()) {
            Some(id) => id,
            None => return json!({ "error": "track_id is required" }),
        };

        let track_repo = TrackRepo::with_backend(self.db.clone());
        let track = match track_repo.get(track_id) {
            Ok(Some(t)) => t,
            Ok(None) => return json!({ "error": "track not found", "track_id": track_id }),
            Err(e) => return json!({ "error": format!("db error: {e}") }),
        };

        let queue_repo = PlayQueueRepo::with_backend(self.db.clone());
        match queue_repo.add_tracks(self.zone_id, &[track_id], None) {
            Ok(_) => json!({
                "status": "added",
                "track": track.title,
                "artist": track.artist_name,
                "track_id": track_id,
            }),
            Err(e) => json!({ "error": format!("queue error: {e}") }),
        }
    }

    async fn pause(&self) -> Value {
        info!(
            zone_id = self.zone_id,
            origine = "assistant",
            "pause_requested"
        );
        let device_id = self.get_zone_device_id();
        match self
            .orchestrator
            .pause(self.zone_id, device_id.as_deref())
            .await
        {
            Ok(()) => json!({ "status": "paused" }),
            Err(error) => json!({ "error": "output_command_failed", "detail": error }),
        }
    }

    async fn resume(&self) -> Value {
        info!(
            zone_id = self.zone_id,
            origine = "assistant",
            "resume_requested"
        );
        let device_id = self.get_zone_device_id();
        match self
            .orchestrator
            .resume(self.zone_id, device_id.as_deref())
            .await
        {
            Ok(()) => json!({ "status": "resumed" }),
            Err(error) => json!({ "error": "output_command_failed", "detail": error }),
        }
    }

    /// #6836 — le volume demandé par le modèle. Trois règles, avant même
    /// l'orchestrateur :
    ///
    /// 1. **Aucun défaut** : un `volume` absent, non numérique, non fini ou
    ///    hors de 0..1 est REFUSÉ et l'erreur rendue au modèle. Le défaut de
    ///    0,5 d'avant faisait monter une zone à 20 % sur une question mal
    ///    comprise.
    /// 2. **Une baisse passe toujours.**
    /// 3. **Une hausse passe par petits pas** : au plus
    ///    [`hausse_maximale_par_appel`] (3 points par défaut) au-dessus du
    ///    volume courant ; au-delà, refus, et la réponse dit au modèle
    ///    jusqu'où il peut aller. Jamais un saut de 20 % à 100 %.
    ///
    /// La consigne retenue part ensuite par `PlaybackOrchestrator::set_volume`,
    /// le point de passage unique de toutes les sorties : verrou PURE (refus
    /// d'une baisse sous « Forcer à 100 % »), volume fixe (tenu à 100 %, rien
    /// n'est envoyé), limites de sécurité et de confort #6541, trim.
    async fn set_volume(&self, input: Value) -> Value {
        let volume = match input.get("volume").and_then(Value::as_f64) {
            Some(v) if v.is_finite() && (0.0..=1.0).contains(&v) => v,
            _ => {
                warn!(zone_id = self.zone_id, entree = %input, "ai_set_volume_refuse_argument");
                return json!({
                    "error": "invalid_volume",
                    "message": "volume is required: a number between 0.0 and 1.0. \
                                Nothing was changed.",
                });
            }
        };
        let actuel = self.playback.get_state(self.zone_id).await.volume;
        let hausse_max = hausse_maximale_par_appel(&self.db);
        if volume > actuel + hausse_max + 1e-9 {
            let plafond = (actuel + hausse_max).min(1.0);
            warn!(
                zone_id = self.zone_id,
                actuel,
                demande = volume,
                hausse_max,
                "ai_set_volume_refuse_hausse"
            );
            return json!({
                "error": "volume_raise_too_large",
                "message": format!(
                    "The assistant may raise the volume by at most {:.0} points per request. \
                     Current volume is {:.0}%; the highest allowed now is {:.0}%. Nothing was changed.",
                    hausse_max * 100.0,
                    actuel * 100.0,
                    plafond * 100.0
                ),
                "current_volume": actuel,
                "max_volume_allowed": plafond,
            });
        }

        // DLNA et OpenHome arrondissent au pourcentage entier : 0,995
        // devient déjà 100 %. Même un petit pas ne doit pas y conduire.
        if (volume * 100.0).round() >= 100.0 {
            return json!({
                "error": "full_volume_refused",
                "message": "The assistant cannot set full volume. Nothing was changed.",
            });
        }

        let device_id = self.get_zone_device_id();
        match self
            .orchestrator
            .set_volume(self.zone_id, volume, device_id.as_deref())
            .await
        {
            // La valeur EFFECTIVE (limites, volume fixe), pas la demande.
            Ok(()) => json!({
                "status": "volume_set",
                "volume": self.playback.get_state(self.zone_id).await.volume,
                "requested": volume,
            }),
            Err(error) => json!({ "error": "output_command_failed", "detail": error }),
        }
    }

    async fn next_track(&self) -> Value {
        let current = self.playback.get_state(self.zone_id).await;

        let Some(next_pos) = crate::poller::PositionPoller::next_position(&current) else {
            return json!({ "status": "end_of_queue", "message": "no more tracks in queue" });
        };
        // #4806 — comme la route « Suivant » : les titres bannis (locaux et
        // de service) sont enjambés.
        let next_pos = match self
            .orchestrator
            .enjamber_les_pistes_bannies(self.zone_id, next_pos)
            .await
        {
            crate::orchestrator::Enjambee::Rien => next_pos,
            crate::orchestrator::Enjambee::Reprise(p) => p,
            crate::orchestrator::Enjambee::FileEpuisee => {
                return json!({ "status": "end_of_queue", "message": "no more tracks in queue" });
            }
        };

        match self
            .orchestrator
            .play_from_queue(self.zone_id, next_pos)
            .await
        {
            Ok(_result) => {
                let state = self.playback.get_state(self.zone_id).await;
                let np = state.now_playing.as_ref();
                json!({
                    "status": "playing",
                    "track": np.map(|n| n.title.as_str()),
                    "artist": np.and_then(|n| n.artist_name.as_deref()),
                })
            }
            Err(e) => json!({ "error": format!("next failed: {e}") }),
        }
    }

    async fn now_playing(&self) -> Value {
        let state = self.playback.get_state(self.zone_id).await;
        match &state.now_playing {
            Some(np) => json!({
                "state": format!("{:?}", state.state).to_lowercase(),
                "track": np.title,
                "artist": np.artist_name,
                "album": np.album_title,
                "duration_ms": np.duration_ms,
                "position_ms": state.position_ms,
                "volume": state.volume,
                "zone_id": self.zone_id,
            }),
            None => json!({
                "state": "stopped",
                "message": "nothing is playing",
                "zone_id": self.zone_id,
            }),
        }
    }

    fn list_zones(&self) -> Value {
        let zone_repo = ZoneRepo::with_backend(self.db.clone());
        let zones = zone_repo.list().unwrap_or_default();
        let zone_list: Vec<Value> = zones
            .iter()
            .map(|z| {
                json!({
                    "id": z.id,
                    "name": z.name,
                    "output_type": z.output_type,
                    "online": z.online,
                })
            })
            .collect();
        json!({ "zones": zone_list })
    }

    fn set_zone(&mut self, input: Value) -> Value {
        let zone_id = match input.get("zone_id").and_then(|v| v.as_i64()) {
            Some(id) => id,
            None => return json!({ "error": "zone_id is required" }),
        };

        // Verify zone exists
        let zone_repo = ZoneRepo::with_backend(self.db.clone());
        match zone_repo.get(zone_id) {
            Ok(Some(z)) => {
                self.zone_id = zone_id;
                json!({
                    "status": "zone_set",
                    "zone_id": zone_id,
                    "zone_name": z.name,
                })
            }
            Ok(None) => json!({ "error": "zone not found", "zone_id": zone_id }),
            Err(e) => json!({ "error": format!("db error: {e}") }),
        }
    }

    fn get_zone_device_id(&self) -> Option<String> {
        ZoneRepo::with_backend(self.db.clone())
            .get(self.zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id)
    }
}

/// #6836 — hausse maximale (fraction 0..1) qu'un appel de l'assistant peut
/// demander au-dessus du volume courant. VALEUR PROVISOIRE : 3 points, en
/// attendant la décision de Bertrand. Réglable sans recompiler par le réglage
/// [`REGLAGE_HAUSSE_MAXIMALE`] (fraction 0..1) ; une valeur absente, illisible
/// ou hors bornes rend le défaut.
pub const HAUSSE_MAXIMALE_PAR_APPEL: f64 = 0.03;

/// Clé du réglage qui remplace [`HAUSSE_MAXIMALE_PAR_APPEL`].
pub const REGLAGE_HAUSSE_MAXIMALE: &str = "ai_volume_max_raise_per_call";

pub fn hausse_maximale_par_appel(db: &Arc<dyn DbBackend>) -> f64 {
    crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .get(REGLAGE_HAUSSE_MAXIMALE)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
        .unwrap_or(HAUSSE_MAXIMALE_PAR_APPEL)
}

#[cfg(test)]
#[path = "executor_volume_6836_tests.rs"]
mod volume_6836_tests;
