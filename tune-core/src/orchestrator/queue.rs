use super::*;

impl PlaybackOrchestrator {
    /// Remove any gapless-prepared stream session for a zone.
    /// Called when a zone starts a new track or stops, so the
    /// previously prepared session doesn't leak.
    pub(super) async fn cleanup_gapless_session(&self, zone_id: i64) {
        let old_sid = self.gapless_sessions.lock().await.remove(&zone_id);
        if let Some(ref sid) = old_sid {
            self.streamer.remove_session(sid).await;
            debug!(zone_id, stream_id = %sid, "gapless_session_cleaned_up");
        }
    }

    /// Pre-transcode the NEXT local queue track into the transcode cache while
    /// the current one plays, so its play is a cache hit — this masks the ~30s
    /// file-transcode latency across an album (the per-track transition gap
    /// Yves hears on DLNA). Best-effort, background, and a no-op unless the next
    /// track decodes to the SAME PCM params (sample rate / bit depth / channels,
    /// non-DSD): only then is the negotiated output — and thus the cache key —
    /// guaranteed identical to what the next track's real play would produce.
    /// Callers fire this only when the current track was itself cached (no EQ).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn spawn_warm_next_local(
        &self,
        zone_id: i64,
        cur_sr: u32,
        cur_bd: u16,
        cur_ch: u16,
        out_ext: String,
        out_sr: u32,
        out_bd: u16,
        target_fmt: String,
    ) {
        let db = self.db.clone();
        tokio::spawn(async move {
            // Locate the current queue position and the item right after it.
            let qrepo = PlayQueueRepo::with_backend(db.clone());
            let queue = match qrepo.get_queue(zone_id) {
                Ok(q) => q,
                Err(_) => return,
            };
            let Some(cur_pos) = queue.iter().find(|q| q.is_current).map(|q| q.position) else {
                return;
            };
            let Some(next) = queue.iter().find(|q| q.position == cur_pos + 1) else {
                return;
            };
            let Some(next_file) = next.file_path.clone() else {
                return;
            };
            // Same decoded params as the current track? A NULL prop or a DSD
            // source means the negotiated output could differ — skip to avoid
            // warming a cache key the real play won't hit.
            let trepo = TrackRepo::with_backend(db.clone());
            let Some(t) = trepo.get(next.track_id).ok().flatten() else {
                return;
            };
            let is_dsd = t
                .format
                .as_deref()
                .map(|f| matches!(f.to_ascii_lowercase().as_str(), "dsd" | "dsf" | "dff"))
                .unwrap_or(false);
            let (Some(n_sr), Some(n_bd)) = (t.sample_rate, t.bit_depth) else {
                return;
            };
            if is_dsd
                || n_sr as u32 != cur_sr
                || n_bd as u16 != cur_bd
                || t.channels as u16 != cur_ch
            {
                return;
            }
            // Already warmed / cached?
            let Some(cp) =
                crate::transcode_cache::cache_path(&next_file, &out_ext, out_sr, out_bd, cur_ch)
            else {
                return;
            };
            if crate::transcode_cache::is_hit(&cp) {
                return;
            }
            // Transcode into a fresh temp file, then atomically rename it into the
            // cache (crash-safe: a partial write never lands under a cache name).
            let tmp = std::env::temp_dir()
                .join(format!(
                    "tune-transcode-{}.{}",
                    uuid::Uuid::new_v4(),
                    out_ext
                ))
                .to_string_lossy()
                .to_string();
            match transcode_source_to_file(
                next_file,
                out_sr,
                cur_ch,
                out_bd,
                target_fmt,
                None,
                None,
                None,
                tmp.clone(),
                // Pré-chauffage en fond, sans budget ni auditeur qui attend :
                // rien à mesurer, rien à étendre (#3140).
                None,
            )
            .await
            {
                Ok((size, _, _)) if size >= 1024 && std::fs::rename(&tmp, &cp).is_ok() => {
                    tokio::task::spawn_blocking(crate::transcode_cache::evict);
                    info!(zone_id, cache = %cp, "transcode_cache_warmed_next");
                }
                _ => {
                    let _ = std::fs::remove_file(&tmp);
                }
            }
        });
    }

    /// Pre-transcode the NEXT streaming track (Tidal/Qobuz HI-RES DASH) into the
    /// warm cache while the current one plays, so an album/playlist advance is an
    /// instant cache hit instead of another 6-23s blocking download+transcode
    /// (#1146). Opt-in via TUNE_DASH_WARM_CACHE (same flag as the check/store).
    ///
    /// The next track goes to the SAME zone/device as the current one, so the
    /// FLAC-vs-WAV decision is identical — we inherit `out_fmt` from the current
    /// play instead of re-probing the renderer from this detached task. The
    /// current track is located by `cur_source_id` in the streaming queue (robust
    /// against in-flight playback-state timing); we warm the item right after it.
    pub(super) fn spawn_warm_next_streaming(
        &self,
        zone_id: i64,
        cur_source_id: String,
        out_fmt: &'static str,
    ) {
        if !dash_warm_cache_enabled() {
            return;
        }
        let db = self.db.clone();
        let services = self.services.clone();
        tokio::spawn(async move {
            // Find the current track in the streaming queue, then the next item.
            let sq = PlayQueueRepo::with_backend(db.clone())
                .get_streaming_queue(zone_id)
                .unwrap_or_default();
            let Some(cur_idx) = sq
                .iter()
                .position(|it| it["source_id"].as_str() == Some(cur_source_id.as_str()))
            else {
                return;
            };
            let Some(item) = sq.get(cur_idx + 1) else {
                return;
            };
            let source = item["source"].as_str().unwrap_or("").to_string();
            let source_id = item["source_id"].as_str().unwrap_or("").to_string();
            if source.is_empty() || source_id.is_empty() {
                return;
            }

            // Resolve the next track's stream. Only a DASH (file://) result is
            // worth caching — a direct proxy stream isn't transcoded.
            let stream_data = {
                let registry = services.lock().await;
                let Some(svc) = registry.get(&source) else {
                    return;
                };
                let svc = svc.read().await;
                match svc.get_track_url(&source_id, None).await {
                    Ok(d) => d,
                    Err(_) => return,
                }
            };
            let Some(dash_file) = stream_data.url.strip_prefix("file://").map(String::from) else {
                return;
            };
            if !std::path::Path::new(&dash_file).exists() {
                return;
            }

            let sr = stream_data.quality.sample_rate;
            let bd = stream_data.quality.bit_depth.max(16).min(24);
            let key_bd = if out_fmt == "wav" { 16 } else { bd };
            let cp = crate::transcode_cache::cache_path_streaming(
                &source, &source_id, out_fmt, sr, key_bd, 2,
            );
            if crate::transcode_cache::is_hit(&cp) {
                return; // already warmed
            }

            // Decode the fMP4 → encode (FLAC/WAV, WAV capped at 16-bit) → temp,
            // then atomically rename into the cache. Mirrors the play path.
            let is_wav = out_fmt == "wav";
            let tmp = std::env::temp_dir()
                .join(format!(
                    "tune-dash-warm-{}.{}",
                    uuid::Uuid::new_v4(),
                    out_fmt
                ))
                .to_string_lossy()
                .to_string();
            let dash_file_c = dash_file.clone();
            let tmp_c = tmp.clone();
            let result = tokio::task::spawn_blocking(move || {
                let decoded =
                    crate::audio::decode::decode_to_pcm(&dash_file_c, Some(sr), Some(2), 0.0, 0.0)?;
                let mut pcm_bytes = decoded.pcm_bytes();
                let mut actual_bd = decoded.bit_depth;
                if is_wav && actual_bd > 16 {
                    pcm_bytes = crate::audio::decode::convert_pcm_bytes(&pcm_bytes, actual_bd, 16);
                    actual_bd = 16;
                }
                let rt = tokio::runtime::Handle::try_current()
                    .map_err(|e| format!("no tokio runtime: {e}"))?;
                let encoded = rt.block_on(async {
                    let mut encoder = crate::audio::encoder::AudioEncoder::new(
                        out_fmt,
                        decoded.sample_rate,
                        actual_bd as u32,
                        decoded.channels,
                    );
                    encoder.start().await?;
                    encoder.write(&pcm_bytes).await?;
                    encoder.finish().await
                })?;
                std::fs::write(&tmp_c, &encoded).map_err(|e| format!("write temp: {e}"))?;
                Ok::<(u64, u16, u32), String>((
                    encoded.len() as u64,
                    actual_bd,
                    decoded.sample_rate,
                ))
            })
            .await;

            // The fMP4 has served its purpose (a warm cache-hit serves the FLAC),
            // so consume it like the play path does.
            let _ = std::fs::remove_file(&dash_file);

            // Same guard as the play-path store: only cache when the decoded
            // reality matches the key (`quality.*` from the service API can lie);
            // a mismatched entry would mis-advertise depth/rate in DIDL on every
            // later hit (Ruark-silence class, #1137).
            match result {
                Ok(Ok((size, actual_bd, actual_sr)))
                    if size >= 1024
                        && actual_bd == key_bd
                        && actual_sr == sr
                        && std::fs::rename(&tmp, &cp).is_ok() =>
                {
                    tokio::task::spawn_blocking(crate::transcode_cache::evict);
                    info!(zone_id, cache = %cp, next_source_id = %source_id, "streaming_dash_warm_next_stored");
                }
                _ => {
                    let _ = std::fs::remove_file(&tmp);
                }
            }
        });
    }

    /// Clear the prefetch buffer. Should be called when the queue changes
    /// (add/remove/reorder) so stale prefetched data is discarded.
    pub async fn clear_prefetch(&self) {
        self.prefetch.clear().await;
    }

    /// Persist the play_queue table for a zone with the given local track IDs.
    /// Called after queue mutations to keep the DB in sync with in-memory state.
    pub fn persist_local_queue(&self, zone_id: i64, track_ids: &[i64], current_position: i64) {
        let repo = PlayQueueRepo::with_backend(self.db.clone());
        if let Err(e) = repo.set_queue(zone_id, track_ids) {
            warn!(zone_id, error = %e, "persist_local_queue_failed");
            return;
        }
        if current_position > 0 {
            repo.set_current(zone_id, current_position).ok();
        }
    }

    /// Persist the streaming_queue table for a zone.
    pub fn persist_streaming_queue(
        &self,
        zone_id: i64,
        tracks: &[crate::db::play_queue_repo::StreamingQueueItem],
    ) {
        let repo = PlayQueueRepo::with_backend(self.db.clone());
        if let Err(e) = repo.set_streaming_queue(zone_id, tracks) {
            warn!(zone_id, error = %e, "persist_streaming_queue_failed");
        }
    }

    pub async fn play_from_queue(&self, zone_id: i64, position: i64) -> Result<PlayResult, String> {
        let queue_repo = PlayQueueRepo::with_backend(self.db.clone());

        let output_device_id = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id);

        // Unified single-position-space resolution: `position` indexes ONE
        // ordered queue (local + streaming). Look the row up directly — no more
        // "try local, then offset into streaming by position - local_count",
        // which broke manual Next across a source boundary (Sandro S2: the local
        // "next" was never found after a Qobuz track, so the zone froze).
        queue_repo.set_current_pos(zone_id, position).ok();
        let total = queue_repo.count_all(zone_id)?;
        let entry = queue_repo
            .get_at(zone_id, position)?
            .ok_or("no queue item at position")?;

        let req = if let Some(track_id) = entry.track_id {
            // Local track.
            PlayRequest {
                zone_id,
                output_device_id,
                track_id: Some(track_id),
                source: None,
                source_id: None,
                title: entry.title.clone(),
                artist_name: entry.artist_name.clone(),
                album_title: entry.album_title.clone(),
                cover_url: entry.cover_path.clone(),
                duration_ms: entry.duration_ms,
                seek_ms: None,
                temp_file_path: None,
                sample_rate: None,
                bit_depth: None,
                media_format: None,
                track_number: entry.track_number.map(|n| n as u32),
                disc_number: entry.disc_number.map(|n| n as u32),
            }
        } else {
            // Streaming track.
            let source_id = entry.source_id.clone().unwrap_or_default();
            let mut title = entry.title.clone();
            let mut artist = entry.artist_name.clone();
            let mut album = entry.album_title.clone();
            let mut cover = entry.cover_path.clone();
            let mut duration_ms = entry.duration_ms;

            let current_state = self.playback.get_state(zone_id).await;

            // Repeat on a single-track queue re-plays the SAME position, but the
            // streaming row can carry an empty title (persisted without
            // metadata). play() would then hand an empty title down the
            // prefetched path and blank Now Playing (DEvir). When the row title
            // is empty AND now_playing is still the very same track (same
            // source_id), reuse its metadata synchronously — no network
            // round-trip, and it can't mislabel a different track since the
            // source_id must match.
            let title_empty = title.as_deref().unwrap_or("").is_empty();
            if title_empty
                && let Some(np) = current_state.now_playing.as_ref()
                && np.source_id.as_deref() == Some(source_id.as_str())
                && !np.title.is_empty()
            {
                title = Some(np.title.clone());
                artist = artist.or_else(|| np.artist_name.clone());
                album = album.or_else(|| np.album_title.clone());
                cover = cover.or_else(|| np.cover_path.clone());
                // Also reuse the duration: filling ONLY the title from a row
                // whose duration_ms is 0 armed the worst combo downstream —
                // has_title=true disables resolve_streaming_url's get_track
                // duration backfill (reserved for empty titles), duration 0
                // reaches the exclusive local output, and the poller's
                // position-past-end advance (#483) requires duration > 0: on
                // a Repeat All loop transition the ring starved at exactly
                // one track length and playback froze forever, zone stuck
                // "Playing" with a frozen position (DEvir, v0.9.14, ASIO).
                if duration_ms.unwrap_or(0) == 0 && np.duration_ms > 0 {
                    duration_ms = Some(np.duration_ms);
                }
            }

            // Use the stored source, falling back to the current now_playing
            // source (handles old DB rows without a source value).
            let source = entry
                .source
                .clone()
                .filter(|s| !s.is_empty() && s != "local")
                .unwrap_or_else(|| {
                    current_state
                        .now_playing
                        .as_ref()
                        .map(|np| np.source.clone())
                        .unwrap_or_else(|| "tidal".into())
                });

            PlayRequest {
                zone_id,
                output_device_id,
                track_id: None,
                source: Some(source),
                source_id: Some(source_id),
                title,
                artist_name: artist,
                album_title: album,
                cover_url: cover,
                duration_ms,
                seek_ms: None,
                temp_file_path: None,
                sample_rate: None,
                bit_depth: None,
                media_format: None,
                track_number: entry.track_number.map(|n| n as u32),
                disc_number: entry.disc_number.map(|n| n as u32),
            }
        };

        // Set the queue index BEFORE play() emits "started" so the event
        // carries the correct queue_position and the client updates its
        // highlight without refetching the whole queue (#1096).
        self.playback
            .update_queue_info(zone_id, position, total)
            .await;
        let result = self.play(req).await?;
        Ok(result)
    }

    pub async fn advance_queue_metadata(&self, zone_id: i64, position: i64) -> Result<(), String> {
        let queue_repo = PlayQueueRepo::with_backend(self.db.clone());
        queue_repo.set_current_pos(zone_id, position).ok();

        let total = queue_repo.count_all(zone_id)?;
        let entry = queue_repo
            .get_at(zone_id, position)?
            .ok_or("no queue item at position")?;

        let np = if let Some(track_id) = entry.track_id {
            let track_repo = crate::db::track_repo::TrackRepo::with_backend(self.db.clone());
            let track = track_repo.get(track_id).ok().flatten();
            let cover_path = track.as_ref().and_then(|t| t.cover_path.clone());
            // Audio-format fields (format/sample_rate/bit_depth/genre/year) come
            // from the library row via `from_track` (single source of the
            // source-over-output bit-depth rule); display fields come from the
            // queue-entry cache and source is pinned local.
            crate::playback::NowPlaying {
                track_id: Some(track_id),
                title: entry.title.clone().unwrap_or_default(),
                artist_name: entry.artist_name.clone(),
                album_title: entry.album_title.clone(),
                cover_path: self.resolve_cover_url(cover_path.as_deref()),
                duration_ms: entry.duration_ms.unwrap_or(0),
                source: "local".into(),
                source_id: None,
                ..track
                    .as_ref()
                    .map(crate::playback::NowPlaying::from_track)
                    .unwrap_or_default()
            }
        } else {
            let source = entry
                .source
                .clone()
                .filter(|s| !s.is_empty() && s != "local")
                .unwrap_or_else(|| "streaming".into());
            let source = if source == "streaming" {
                let cs = self.playback.get_state(zone_id).await;
                cs.now_playing
                    .as_ref()
                    .map(|np| np.source.clone())
                    .unwrap_or_else(|| "streaming".into())
            } else {
                source
            };
            crate::playback::NowPlaying {
                track_id: None,
                title: entry.title.clone().unwrap_or_default(),
                artist_name: entry.artist_name.clone(),
                album_title: entry.album_title.clone(),
                cover_path: self.resolve_cover_url(entry.cover_path.as_deref()),
                duration_ms: entry.duration_ms.unwrap_or(0),
                source,
                source_id: entry.source_id.clone(),
                stream_id: None,
                ..Default::default()
            }
        };

        // Set the queue index BEFORE update_now_playing emits "track_changed"
        // so the event carries the new queue_position — the client updates its
        // highlight/badge without refetching the whole queue (#1096).
        self.playback
            .update_queue_info(zone_id, position, total)
            .await;
        // Last.fm/ListenBrainz "now playing": a gapless advance is a real track
        // change, but it bypasses play_inner — the only other dispatch site —
        // so the now-playing of every gapless-reached track (tracks 2, 4, 6… of
        // an album) was never sent (#1113). This method is the single funnel
        // for all gapless advance paths (position reset, duration change,
        // confirmed pending advance), so dispatch here exactly once per track.
        self.dispatch_now_playing(
            &np.title,
            np.artist_name.as_deref(),
            np.album_title.as_deref(),
        );
        // Use update_now_playing (not play) to avoid bumping track_generation —
        // the poller must keep its gapless_cooldown intact so it doesn't falsely
        // detect track-end on renderers that briefly report Stopped during
        // gapless transitions. Position MUST reset to 0 (new track from start).
        let advance_track_id = np.track_id;
        let advance_source = np.source.clone();
        let advance_source_id = np.source_id.clone();
        self.playback.update_now_playing(zone_id, np).await;
        // `reset_position` et non `update_position` : ce chemin est le SEUL
        // changement de piste qui n'emprunte pas `play()` — c'est tout l'objet
        // du commentaire ci-dessus. La garde de monotonie de `update_position`
        // n'accepte un recul que d'une COMMANDE, et cette remise à 0 en est
        // une ; passée par la porte des observations, elle aurait été prise
        // pour un renderer qui se contredit et le curseur serait resté collé à
        // la fin de la piste précédente pendant tout un album enchaîné (#3229).
        self.playback.reset_position(zone_id, 0).await;
        self.playback.emit_position(zone_id, 0);

        // Niveaux de la piste devenue courante. Le pré-chargement gapless
        // n'attache pas de forwarder (ses fenêtres seraient datées de
        // l'horloge de la piste précédente — stamps ~4 min devant le
        // renderer, VU morts, observé sur Stream X) : on invalide ici les
        // forwarders de l'ancienne piste puis on démarre un décodage dédié,
        // position 0, comme pour une lecture explicite en passthrough.
        self.playback.bump_levels_gen(zone_id);
        if let (Some(bus), Some(track_id)) = (self.event_bus.clone(), advance_track_id) {
            let track = crate::db::track_repo::TrackRepo::with_backend(self.db.clone())
                .get(track_id)
                .ok()
                .flatten();
            let format = track.as_ref().and_then(|t| t.format.clone());
            // La sortie ne se consulte QUE pour du DSD : c'est le seul format
            // dont le décodage-pour-niveaux coûte assez cher pour valoir une
            // lecture de zone, et le seul chemin qui ne mesure pas (OAAT en
            // DSD natif) n'y arrive qu'en jouant du DSD. Interroger la sortie
            // pour tous les formats aurait éteint les VU d'un FLAC enchaîné
            // juste après un DSD, tant que le drapeau natif n'est pas retombé.
            let la_sortie_mesure = if est_source_dsd(format.as_deref()) {
                let device_id = ZoneRepo::with_backend(self.db.clone())
                    .get(zone_id)
                    .ok()
                    .flatten()
                    .and_then(|z| z.output_device_id);
                self.output_produces_levels(device_id.as_deref()).await
            } else {
                true
            };
            if let Some(path) = fichier_a_mesurer_apres_avance(
                format.as_deref(),
                track.and_then(|t| t.file_path),
                la_sortie_mesure,
            ) {
                // Génération épinglée ici : l'avance vient d'avoir lieu, c'est
                // bien la piste devenue courante (#1110).
                let play_seq = self.playback.current_play_seq(zone_id).await;
                spawn_local_file_levels_decode(bus, self.playback.clone(), zone_id, play_seq, path);
            }
        } else if let (Some(bus), Some(source_id)) =
            (self.event_bus.clone(), advance_source_id.clone())
        {
            // Piste STREAMING devenue courante par avance gapless : pas de
            // fichier local à décoder — sa session prewarm n'a jamais attaché
            // de forwarder (levels_prewarm), donc les pistes 2..n d'un album
            // Qobuz/Tidal gardaient les aiguilles figées même une fois le
            // proxy corrigé pour la lecture explicite. On re-résout l'URL du
            // service (cache DASH compris) et on lance la même sonde de
            // niveaux que la lecture explicite en proxy ; un `file://` (fMP4
            // DASH déjà sur disque) se décode localement, comme une piste
            // passthrough.
            if advance_source != "local" && advance_source != "radio" {
                let services = self.services.clone();
                let playback = self.playback.clone();
                let source = advance_source.clone();
                // Épinglé AVANT la tâche : la résolution d'URL du service peut
                // prendre plusieurs secondes, et lire la génération à son issue
                // rattachait la sonde à ce que la zone jouait ALORS. Si
                // l'auditeur avait enchaîné entre-temps, le forwarder héritait
                // de la nouvelle génération et survivait, en publiant le PCM de
                // la piste précédente sur l'horloge de la nouvelle (#1110).
                let play_seq = self.playback.current_play_seq(zone_id).await;
                tokio::spawn(async move {
                    let resolved = {
                        let registry = services.lock().await;
                        let Some(svc) = registry.get(&source) else {
                            return;
                        };
                        let svc = svc.clone();
                        drop(registry);
                        let svc = svc.read().await;
                        svc.get_track_url(&source_id, None).await.ok()
                    };
                    let Some(data) = resolved else {
                        debug!(zone_id, source = %source, "gapless_streaming_levels_url_unresolved");
                        return;
                    };
                    let codec = data.quality.codec.to_lowercase();
                    if let Some(path) = data.url.strip_prefix("file://") {
                        // fMP4 DASH assemblé sur disque : décodage local
                        // direct, même motif — et même helper — que la branche
                        // fichier ci-dessus.
                        let play_seq = playback.current_play_seq(zone_id).await;
                        spawn_local_file_levels_decode(
                            bus,
                            playback,
                            zone_id,
                            play_seq,
                            path.to_string(),
                        );
                    } else {
                        spawn_proxy_levels_probe_task(
                            playback, bus, zone_id, data.url, codec, play_seq,
                        );
                    }
                });
            }
        }
        Ok(())
    }

    pub async fn resolve_queue_item_url(
        &self,
        zone_id: i64,
        position: i64,
    ) -> Result<ResolvedQueueItem, String> {
        // Pré-chargement gapless : pas de forwarder de niveaux sur les
        // sessions créées ici (voir `levels_prewarm`).
        let _prewarm = self.begin_levels_prewarm(zone_id);
        // Clean up any previously prepared gapless session for this zone
        // before creating a new one.
        self.cleanup_gapless_session(zone_id).await;

        let queue_repo = PlayQueueRepo::with_backend(self.db.clone());

        // Unified single-position-space lookup (local or streaming).
        let entry = queue_repo
            .get_at(zone_id, position)?
            .ok_or("no queue item at position (local or streaming)")?;

        // Local track.
        if let Some(track_id) = entry.track_id {
            let album = entry.album_title.clone();
            let cover = entry.cover_path.clone();
            // Resolve the gapless/prefetch stream FOR THE ACTUAL OUTPUT. Without
            // the device id, resolve_stream doesn't apply the output's format
            // rules, so a local output (which needs WAV/PCM) was pre-armed with
            // the raw FLAC stream — the local gapless chain then hit a non-WAV
            // header and fell back (local_audio_gapless_next_not_wav_falling_back),
            // breaking seamless FLAC gapless (Jean Valjean).
            let output_device_id = ZoneRepo::with_backend(self.db.clone())
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id);
            let req = PlayRequest {
                zone_id,
                output_device_id,
                track_id: Some(track_id),
                source: None,
                source_id: None,
                title: entry.title.clone(),
                artist_name: entry.artist_name.clone(),
                album_title: album.clone(),
                cover_url: cover.clone(),
                duration_ms: entry.duration_ms,
                seek_ms: None,
                temp_file_path: None,
                sample_rate: None,
                bit_depth: None,
                media_format: None,
                track_number: None,
                disc_number: None,
            };
            let resolved = self.resolve_stream(&req).await?;
            if let Some(ref sid) = resolved.stream_id {
                self.gapless_sessions
                    .lock()
                    .await
                    .insert(zone_id, sid.clone());
            }
            let raw_cover = cover.or(resolved.cover_url);
            return Ok(ResolvedQueueItem {
                url: resolved.url,
                mime_type: resolved.mime_type,
                title: resolved.title,
                artist: resolved.artist,
                album,
                cover_url: self.resolve_cover_url(raw_cover.as_deref()),
                duration_ms: resolved.duration_ms.map(|d| d as u64),
                stream_id: resolved.stream_id,
                sample_rate: resolved.sample_rate,
                bit_depth: resolved.bit_depth,
                channels: resolved.channels,
                file_size: resolved.file_size,
                file_path: None,
                source: Some("local".into()),
                source_id: Some(track_id.to_string()),
                track_number: entry.track_number.map(|n| n as u32),
                disc_number: entry.disc_number.map(|n| n as u32),
            });
        }

        // Streaming track (Tidal, Qobuz, Deezer, etc.).
        let source_id = entry.source_id.clone().unwrap_or_default();
        let title = entry.title.clone();
        let artist = entry.artist_name.clone();
        let album = entry.album_title.clone();
        let cover = entry.cover_path.clone();
        let duration = entry.duration_ms;
        let source = match entry
            .source
            .clone()
            .filter(|s| !s.is_empty() && s != "local")
        {
            Some(s) => s,
            None => {
                let cs = self.playback.get_state(zone_id).await;
                cs.now_playing
                    .as_ref()
                    .map(|np| np.source.clone())
                    .unwrap_or_else(|| "tidal".into())
            }
        };
        let output_device_id = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id);
        let req = PlayRequest {
            zone_id,
            output_device_id,
            track_id: None,
            source: Some(source),
            source_id: Some(source_id.clone()),
            title: title.clone(),
            artist_name: artist.clone(),
            album_title: album.clone(),
            cover_url: cover.clone(),
            duration_ms: duration,
            seek_ms: None,
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: None,
            disc_number: None,
        };
        let resolved = self.resolve_stream(&req).await?;
        if let Some(ref sid) = resolved.stream_id {
            self.gapless_sessions
                .lock()
                .await
                .insert(zone_id, sid.clone());
        }
        let raw_cover = cover.or(resolved.cover_url);
        Ok(ResolvedQueueItem {
            url: resolved.url,
            mime_type: resolved.mime_type,
            // Prefer the queue item's metadata (the streaming resolve returns an
            // empty title for Tidal/Qobuz) so the gapless-next SetNext carries
            // the real title instead of blanking it (DEvir).
            title: title.filter(|s| !s.is_empty()).unwrap_or(resolved.title),
            artist: artist.filter(|s| !s.is_empty()).or(resolved.artist),
            album,
            cover_url: self.resolve_cover_url(raw_cover.as_deref()),
            duration_ms: resolved.duration_ms.map(|d| d as u64),
            stream_id: resolved.stream_id,
            sample_rate: resolved.sample_rate,
            bit_depth: resolved.bit_depth,
            channels: resolved.channels,
            file_size: resolved.file_size,
            file_path: None,
            source: entry.source.clone(),
            source_id: Some(source_id.clone()),
            track_number: entry.track_number.map(|n| n as u32),
            disc_number: entry.disc_number.map(|n| n as u32),
        })
    }

    /// Resolve the next queue item as a LOCAL FILE — file path + metadata + native
    /// format, read straight from the DB WITHOUT creating a transcode/stream
    /// session. Used for OAAT native-DSD gapless: the output opens the `.dsf`
    /// directly, so spinning up the usual DSD->PCM transcode (as the URL path
    /// does) would only orphan an unconsumed decode (`dsd_streaming_send_timeout`)
    /// and stall the transition. Returns Ok with `file_path: None` when the next
    /// item is a streaming track or has no local file — the caller then declines
    /// to arm and lets the natural-end fallback advance the queue.
    pub async fn resolve_gapless_next_local_file(
        &self,
        zone_id: i64,
        position: i64,
    ) -> Result<ResolvedQueueItem, String> {
        // Pré-chargement gapless : pas de forwarder de niveaux (voir
        // `levels_prewarm`).
        let _prewarm = self.begin_levels_prewarm(zone_id);
        // Drop any previously prepared gapless (URL) session for this zone so we
        // don't leak a transcode session when switching to the local-file path.
        self.cleanup_gapless_session(zone_id).await;

        let entry = PlayQueueRepo::with_backend(self.db.clone())
            .get_at(zone_id, position)?
            .ok_or("no queue item at position (local or streaming)")?;

        // A local file is present only for local library tracks; streaming
        // items (track_id None / file_path None) return file_path: None so the
        // caller declines to arm gapless and lets the natural end advance.
        let file_path = entry.track_id.and(entry.file_path.clone());
        let mime_type = entry
            .format
            .as_ref()
            .map(|f| format!("audio/{}", f.to_lowercase()))
            .unwrap_or_default();

        Ok(ResolvedQueueItem {
            url: String::new(),
            mime_type,
            title: entry.title.unwrap_or_default(),
            artist: entry.artist_name,
            album: entry.album_title,
            cover_url: self.resolve_cover_url(entry.cover_path.as_deref()),
            duration_ms: entry.duration_ms.map(|d| d as u64),
            stream_id: None,
            sample_rate: entry.sample_rate.map(|r| r as u32),
            bit_depth: entry.bit_depth.map(|b| b as u32),
            channels: None,
            file_size: None,
            file_path,
            source: entry
                .source
                .clone()
                .or_else(|| entry.track_id.map(|_| "local".to_string())),
            source_id: entry
                .source_id
                .clone()
                .or_else(|| entry.track_id.map(|t| t.to_string())),
            track_number: entry.track_number.map(|n| n as u32),
            disc_number: entry.disc_number.map(|n| n as u32),
        })
    }
}
