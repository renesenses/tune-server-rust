use super::*;

impl PositionPoller {
    pub(super) async fn handle_track_end(
        &self,
        zone_id: i64,
        zone_state: &crate::playback::ZoneState,
    ) {
        // Diagnostic: capture now-playing info to help diagnose premature advance issues.
        let np_title = zone_state
            .now_playing
            .as_ref()
            .map(|np| np.title.as_str())
            .unwrap_or("unknown");
        let np_duration = zone_state
            .now_playing
            .as_ref()
            .map(|np| np.duration_ms)
            .unwrap_or(0);

        let device_id = self.get_zone_device_id(zone_id);

        let Some(next_pos) = Self::next_position(zone_state) else {
            self.terminer_la_file(zone_id, zone_state, device_id.as_deref())
                .await;
            return;
        };

        let is_repeat = matches!(zone_state.repeat, RepeatMode::One | RepeatMode::All);
        info!(
            zone_id,
            next_pos,
            repeat = ?zone_state.repeat,
            shuffle = zone_state.shuffle,
            is_repeat,
            title = %np_title,
            duration_ms = np_duration,
            queue_len = zone_state.queue_length,
            queue_pos = zone_state.queue_position,
            "auto_next"
        );
        // Skip tracks that cannot be played instead of ending the session. A
        // single unplayable streaming track — rights withdrawn, region block,
        // the service returning no URL at any format — used to stop the whole
        // queue: playing 11 albums to a zone died on one blocked track with
        // 108 items still queued, leaving nothing but a WARN behind. Walk
        // forward over the dead items, announce each one, and stop only when
        // the queue really is exhausted (or the failures look systemic).
        if self
            .avancer_avec_reprises(zone_id, zone_state, next_pos)
            .await
        {
            return;
        }
        self.orchestrator.stop(zone_id, device_id.as_deref()).await;
    }

    /// Fin de file : plus rien à jouer après cette piste. Autoplay (piste
    /// semée depuis l'écoute en cours) quand la zone l'a demandé, sinon la
    /// zone s'arrête. Chaque chemin sort après avoir décidé.
    async fn terminer_la_file(
        &self,
        zone_id: i64,
        zone_state: &crate::playback::ZoneState,
        device_id: Option<&str>,
    ) {
        // Queue ended — check if autoplay is enabled for this zone
        let autoplay_enabled = crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .get_autoplay_enabled(zone_id);

        if autoplay_enabled {
            let mut seed_track_id = zone_state.now_playing.as_ref().and_then(|np| np.track_id);
            let mut seed_artist = zone_state
                .now_playing
                .as_ref()
                .and_then(|np| np.artist_name.clone());

            // File vide DÈS LE DÉPART : rien n'a joué, donc rien à
            // prolonger. C'était le cas d'un serveur qu'on rallume ou
            // d'une file qu'on vient d'effacer — le réglage « lecture
            // automatique » était activé et il ne se passait rien, la
            // seule trace étant un `autoplay_skipped_no_seed` en DEBUG.
            // On repart de la dernière écoute de LA ZONE, à défaut de la
            // maison : c'est la graine la plus proche de ce que
            // l'auditeur attend d'entendre.
            if seed_artist.is_none() && seed_track_id.is_none() {
                // La radio par défaut se construit sur les DERNIERS TITRES
                // écoutés, et non sur le seul dernier artiste : c'est la
                // différence entre prolonger un morceau et proposer une
                // radio. On demande leurs semblables à plusieurs artistes
                // récents, et on choisit dans tout ce pool.
                let radio =
                    crate::playback::auto_dj::radio_depuis_l_historique(&self.db, zone_id, 10)
                        .await;
                let ids: Vec<i64> = radio
                    .iter()
                    .filter_map(|t| t["track_id"].as_i64())
                    .collect();
                if !ids.is_empty() {
                    info!(
                        zone_id,
                        count = ids.len(),
                        "autoplay_radio_depuis_l_historique"
                    );
                    let queue_repo =
                        crate::db::play_queue_repo::PlayQueueRepo::with_backend(self.db.clone());
                    if queue_repo.append_tracks(zone_id, &ids).is_ok() {
                        let new_pos = zone_state.queue_position + 1;
                        if let Err(e) = self.orchestrator.play_from_queue(zone_id, new_pos).await {
                            warn!(zone_id, error = %e, "autoplay_play_failed");
                            self.orchestrator.stop(zone_id, device_id.as_deref()).await;
                        }
                        return;
                    }
                }

                // La bibliothèque n'a rien rendu : on garde une graine pour
                // les autres cartes de la chaîne — radio du service,
                // genre/BPM — plutôt que de s'arrêter là.
                if let Some(g) = crate::playback::auto_dj::graine_recente(&self.db, zone_id) {
                    info!(
                        zone_id,
                        artist = %g.artist_name.as_deref().unwrap_or(""),
                        "autoplay_graine_depuis_l_historique"
                    );
                    seed_track_id = g.track_id;
                    seed_artist = g.artist_name;
                }
            }

            // « Radio artistes similaires » : la graine est le NOM d'artiste,
            // donc une écoute streaming (pas de track_id local) alimente
            // aussi l'autoplay. Repli sur le générateur genre/BPM local si
            // l'API d'enrichissement est injoignable ou ne matche rien dans
            // la bibliothèque (Tune doit marcher sans mozaiklabs.fr).
            // La source de l'ecoute en cours passe AVANT le generateur
            // local. Le repli streaming plus bas ne se declenchait que si
            // le local n'avait rien rendu — donc jamais, chez qui a une
            // bibliotheque locale garnie. L'autoplay enchainait alors des
            // titres locaux au milieu d'une ecoute Qobuz.
            // Le repli streaming plus bas est le MEME appel : sans ce
            // temoin il refaisait a l'identique le travail que la branche
            // preferee venait d'echouer — deux fois les memes appels
            // reseau, deux fois les memes lignes de log.
            let mut streaming_already_tried = false;
            let seed_source = zone_state.now_playing.as_ref().map(|np| np.source.clone());
            let seed_source_id = zone_state
                .now_playing
                .as_ref()
                .and_then(|np| np.source_id.clone());
            if decisions::autoplay_prefers_streaming(seed_source.as_deref())
                && let Some(ref artist) = seed_artist
                && let Some(ref source) = seed_source
            {
                let added = self
                    .autoplay_streaming_radio(zone_id, artist, source, seed_source_id.as_deref())
                    .await;
                if added > 0 {
                    let new_pos = zone_state.queue_position + 1;
                    info!(
                        zone_id,
                        added,
                        source = %source,
                        "autoplay_streaming_radio_started_preferred"
                    );
                    if let Err(e) = self.orchestrator.play_from_queue(zone_id, new_pos).await {
                        warn!(zone_id, error = %e, "autoplay_play_failed");
                        self.orchestrator.stop(zone_id, device_id.as_deref()).await;
                    }
                    return;
                }
                // Le service n'a rien rendu (hors catalogue, API muette) :
                // on retombe sur le generateur local plutot que de laisser
                // la file s'arreter en silence.
                info!(zone_id, "autoplay_streaming_empty_falling_back_local");
                streaming_already_tried = true;
            }

            let mut generated = Vec::new();
            if let Some(ref artist) = seed_artist {
                info!(zone_id, artist = %artist, "autoplay_similar_artists_radio");
                generated =
                    crate::playback::auto_dj::generate_similar_artists_queue(&self.db, artist, 10)
                        .await;
            }
            if generated.is_empty() {
                if let Some(seed_id) = seed_track_id {
                    info!(
                        zone_id,
                        seed_track_id = seed_id,
                        "autoplay_generating_tracks"
                    );
                    generated = crate::playback::auto_dj::generate_queue(&self.db, seed_id, 10);
                } else if seed_artist.is_none() {
                    debug!(zone_id, "autoplay_skipped_no_seed");
                }
            }

            let track_ids: Vec<i64> = generated
                .iter()
                .filter_map(|t| t["track_id"].as_i64())
                .collect();

            // Rien en local : la radio s'arrêtait là, en silence. Pour
            // quelqu'un qui écoute Qobuz sans bibliothèque locale, c'était
            // TOUJOURS le cas — la graine streaming était gérée, les
            // résultats ne pouvaient être que locaux. On va donc chercher
            // les artistes similaires dans le service de la piste en cours.
            if track_ids.is_empty()
                && !streaming_already_tried
                && let Some(ref artist) = seed_artist
                && let Some(source) = zone_state
                    .now_playing
                    .as_ref()
                    .map(|np| np.source.clone())
                    .filter(|s| s != "local" && !s.is_empty())
            {
                let added = self
                    .autoplay_streaming_radio(zone_id, artist, &source, seed_source_id.as_deref())
                    .await;
                if added > 0 {
                    let new_pos = zone_state.queue_position + 1;
                    info!(
                        zone_id,
                        added,
                        source = %source,
                        "autoplay_streaming_radio_started"
                    );
                    if let Err(e) = self.orchestrator.play_from_queue(zone_id, new_pos).await {
                        warn!(zone_id, error = %e, "autoplay_play_failed");
                        self.orchestrator.stop(zone_id, device_id.as_deref()).await;
                    }
                    return;
                }
            }

            if !track_ids.is_empty() {
                info!(
                    zone_id,
                    count = track_ids.len(),
                    "autoplay_tracks_generated"
                );

                // Append generated tracks to the play queue
                let queue_repo =
                    crate::db::play_queue_repo::PlayQueueRepo::with_backend(self.db.clone());
                if let Err(e) = queue_repo.append_tracks(zone_id, &track_ids) {
                    warn!(zone_id, error = %e, "autoplay_append_queue_failed");
                    self.orchestrator.stop(zone_id, device_id.as_deref()).await;
                    return;
                }

                // Emit autoplay_tracks_added event for UI updates
                if let Some(ref bus) = self.event_bus {
                    bus.emit(
                        "playback.autoplay_tracks_added",
                        serde_json::json!({
                            "zone_id": zone_id,
                            "track_ids": track_ids,
                            "tracks": generated,
                            "seed_track_id": seed_track_id,
                            "seed_artist": seed_artist,
                        }),
                    );
                }

                // Play the first generated track (next position after current)
                let new_pos = zone_state.queue_position + 1;
                info!(zone_id, new_pos, "autoplay_starting_generated_track");
                if let Err(e) = self.orchestrator.play_from_queue(zone_id, new_pos).await {
                    warn!(zone_id, error = %e, "autoplay_play_failed");
                    self.orchestrator.stop(zone_id, device_id.as_deref()).await;
                }
                return;
            }
            info!(zone_id, "autoplay_no_similar_tracks_found");
        }

        // Log the queue geometry so a "doesn't advance to next track" report
        // (Jean-Pierre) can be told apart at a glance: queue_len=1 means the
        // play truncated the queue to a single track (single-track play path),
        // whereas queue_len>1 with pos+1<len would be a genuine advance bug.
        info!(
            zone_id,
            queue_pos = zone_state.queue_position,
            queue_len = zone_state.queue_length,
            repeat = ?zone_state.repeat,
            "queue_ended"
        );
        self.orchestrator.stop(zone_id, device_id.as_deref()).await;
        return;
    }

    /// Avance : joue la position suivante, et saute les pistes qui échouent
    /// jusqu'à `MAX_CONSECUTIVE_SKIPS`. Rend vrai quand une piste est partie ;
    /// faux quand la file est épuisée ou la série d'échecs trop longue.
    async fn avancer_avec_reprises(
        &self,
        zone_id: i64,
        zone_state: &crate::playback::ZoneState,
        next_pos: i64,
    ) -> bool {
        let mut attempt_pos = next_pos;
        let mut skipped = 0u32;
        loop {
            match self
                .orchestrator
                .play_from_queue(zone_id, attempt_pos)
                .await
            {
                Ok(_) => {
                    if skipped > 0 {
                        info!(
                            zone_id,
                            skipped,
                            next_pos = attempt_pos,
                            "auto_next_resumed_after_skips"
                        );
                    }
                    return true;
                }
                Err(e) => {
                    warn!(zone_id, error = %e, pos = attempt_pos, "auto_next_failed");
                    if let Some(ref bus) = self.event_bus {
                        bus.emit(
                            "playback.track_skipped",
                            serde_json::json!({
                                "zone_id": zone_id,
                                "position": attempt_pos,
                                "reason": e.to_string(),
                            }),
                        );
                    }
                    skipped += 1;
                    // A run this long is not "one bad track" any more — an
                    // expired token or a dead network would otherwise have us
                    // hammer the service once per queued item.
                    if skipped >= MAX_CONSECUTIVE_SKIPS {
                        warn!(zone_id, skipped, "auto_next_skip_limit_reached");
                        break;
                    }
                    match Self::next_position_after(zone_state, attempt_pos) {
                        // Same slot again means repeat-one on a dead track:
                        // skipping would spin forever.
                        Some(p) if p != attempt_pos => attempt_pos = p,
                        _ => break,
                    }
                }
            }
        }
        false
    }

    pub(super) async fn resolve_gapless_next(
        &self,
        zone_id: i64,
        next_pos: i64,
    ) -> Result<crate::orchestrator::ResolvedQueueItem, String> {
        match self
            .orchestrator
            .resolve_queue_item_url(zone_id, next_pos)
            .await
        {
            Ok(r) => Ok(r),
            Err(e) => {
                warn!(zone_id, error = %e, attempt = 1, "gapless_resolve_retry");
                self.orchestrator
                    .resolve_queue_item_url(zone_id, next_pos)
                    .await
            }
        }
    }

    pub(super) async fn prepare_gapless(
        &self,
        zone_id: i64,
        zone_state: &crate::playback::ZoneState,
        device_id: &str,
    ) -> GaplessPrep {
        let Some(next_pos) = Self::next_position(zone_state) else {
            return GaplessPrep::NotArmed;
        };

        // L'identite de ce qu'on s'apprete a armer, lue AVANT de le resoudre :
        // une ligne de file, pas une position (#3026). C'est la seule trace de
        // ce que le renderer aura reellement accepte.
        let arme = crate::db::play_queue_repo::PlayQueueRepo::with_backend(self.db.clone())
            .get_at(zone_id, next_pos)
            .ok()
            .flatten()
            .map(|e| ArmedNext {
                row_id: e.id,
                position: next_pos,
            });

        // Local-file gapless (OAAT native DSD): the output reads the next
        // track's `.dsf` directly, so resolve it as a local file WITHOUT a
        // transcode session (no orphaned DSD->PCM decode / send-timeout stall)
        // and stage it via set_next_media(file_path=..). If the next item has no
        // local file (streaming track), don't arm — the natural-end fallback
        // advances the queue.
        let prefers_local_file = {
            let outputs = self.outputs.lock().await;
            match outputs.get(device_id) {
                Some(arc) => arc.lock().await.prefers_local_file_gapless(),
                None => false,
            }
        };
        if prefers_local_file {
            let t0 = Instant::now();
            match self
                .orchestrator
                .resolve_gapless_next_local_file(zone_id, next_pos)
                .await
            {
                Ok(resolved) if resolved.file_path.is_some() => {
                    let output_arc = {
                        let outputs = self.outputs.lock().await;
                        outputs.get(device_id).map(|a| a.clone())
                    };
                    let Some(output_arc) = output_arc else {
                        return GaplessPrep::NotArmed;
                    };
                    let output = output_arc.lock().await;
                    let media = crate::outputs::PlayMedia {
                        url: &resolved.url,
                        mime_type: &resolved.mime_type,
                        title: Some(&resolved.title),
                        artist: resolved.artist.as_deref(),
                        album: resolved.album.as_deref(),
                        cover_url: resolved.cover_url.as_deref(),
                        duration_ms: resolved.duration_ms,
                        file_size: resolved.file_size,
                        file_path: resolved.file_path.as_deref(),
                        sample_rate: resolved.sample_rate,
                        bit_depth: resolved.bit_depth,
                        channels: resolved.channels,
                        live_stream: false,
                        byte_seekable: true,
                        origin_url: None,
                        source: resolved.source.as_deref(),
                        source_id: resolved.source_id.as_deref(),
                        track_number: resolved.track_number,
                        disc_number: resolved.disc_number,
                    };
                    return match output.set_next_media(&media).await {
                        Ok(()) => {
                            info!(
                                zone_id,
                                title = %resolved.title,
                                resolve_ms = t0.elapsed().as_millis() as u64,
                                "gapless_next_set_local_file"
                            );
                            GaplessPrep::Armed(arme)
                        }
                        Err(e) => {
                            warn!(zone_id, error = %e, "gapless_set_next_local_file_failed");
                            GaplessPrep::NotArmed
                        }
                    };
                }
                Ok(_) => {
                    info!(zone_id, "gapless_local_file_skipped_no_local_next");
                    return GaplessPrep::NotArmed;
                }
                Err(e) => {
                    warn!(zone_id, error = %e, "gapless_local_file_resolve_failed");
                    return GaplessPrep::NotArmed;
                }
            }
        }

        // v0.9 gapless characterization: time the next-track resolution and
        // surface failures at warn. These paths were debug-only, so streaming
        // gapless instability (Tidal DASH download slowness, URL/token issues)
        // was invisible in production journald. Logging only — no behaviour change.
        let t0 = Instant::now();
        match self.resolve_gapless_next(zone_id, next_pos).await {
            Ok(resolved) => {
                let resolve_ms = t0.elapsed().as_millis() as u64;
                let is_streaming = resolved.stream_id.is_some();
                if let Some(ref sid) = resolved.stream_id {
                    let w0 = Instant::now();
                    if !self.orchestrator.wait_stream_data_ready(sid, 5000).await {
                        // The next track's transcode session produced no data
                        // within the 5s budget — common for Tidal Hi-Res DASH
                        // multi-segment downloads. A session that is merely SLOW
                        // is still armed: refusing here would put a gap between
                        // every Hi-Res track.
                        //
                        // Mais « pas encore » et « plus jamais » ne se
                        // distinguent pas dans `data_ready`. La seule question
                        // qui les separe est celle que `resume` pose deja
                        // (#2512) : la session existe-t-elle encore ? Le
                        // producteur d'un transcodage streaming la RETIRE
                        // desormais quand il meurt sans ecrire un octet — echec
                        // de telechargement CDN, voir
                        // `abandonner_la_session_de_transcodage`. S'enchainer
                        // sur une session disparue fige la sortie locale
                        // jusqu'au Stop (#3287, Gros Bidon, Qobuz en USB) : on
                        // n'arme pas, et la fin naturelle avance la file avec un
                        // petit blanc — jamais un gel.
                        let session_vivante = self.orchestrator.stream_session_alive(sid).await;
                        warn!(
                            zone_id,
                            resolve_ms,
                            waited_ms = w0.elapsed().as_millis() as u64,
                            session_vivante,
                            "gapless_data_ready_timeout"
                        );
                        if !session_vivante {
                            warn!(
                                zone_id,
                                stream_id = %sid,
                                "gapless_non_arme_session_disparue"
                            );
                            return GaplessPrep::NotArmed;
                        }
                    }
                }
                let output_arc = {
                    let outputs = self.outputs.lock().await;
                    outputs.get(device_id)
                };
                if let Some(output_arc) = output_arc {
                    let output = output_arc.lock().await;
                    // Exclusive-mode local outputs (ASIO / WASAPI exclusive) take
                    // a dedicated playback loop that returns at EOF without
                    // consuming the staged next_media — they cannot chain
                    // internally. Arming gapless for them orphans the staged
                    // track AND arms the poller guard, which suppresses the
                    // natural-end advance: a single-track Repeat queue never
                    // loops, and multi-track albums stall after each track
                    // (DEvir, ASIO Fireface USB). Skip arming; the natural-end
                    // fallback advances the queue (a small gap, never a stall).
                    if !output.supports_internal_gapless() {
                        info!(zone_id, "gapless_skipped_exclusive_output");
                        return GaplessPrep::NotArmed;
                    }
                    // DSD gapless guard for DLNA renderers (HiFi Rose RS130,
                    // Benjithom). They accept SetNextAVTransportURI for a DSD
                    // stream but never transition to it — the next stream is
                    // never consumed (bytes_sent stays 0) and the poller
                    // force-stops the zone after STOPPED_FAILURE_THRESHOLD ticks,
                    // i.e. "the album cuts after track 1". Don't arm gapless for a
                    // DSD next on DLNA; handle_track_end plays it explicitly at
                    // end-of-track instead (a small gap, never a cut). Local
                    // output keeps its internal DSD gapless chain untouched.
                    if output.output_type() == "dlna" {
                        let url_lc = resolved.url.to_lowercase();
                        let next_is_dsd = crate::playback::gapless::est_dsd(&resolved.mime_type)
                            || url_lc.ends_with(".dsf")
                            || url_lc.ends_with(".dff");
                        if next_is_dsd {
                            info!(
                                zone_id,
                                mime = %resolved.mime_type,
                                "gapless_skipped_dsd_next_dlna"
                            );
                            return GaplessPrep::DsdNextSkipped;
                        }
                    }
                    let media = crate::outputs::PlayMedia {
                        url: &resolved.url,
                        mime_type: &resolved.mime_type,
                        title: Some(&resolved.title),
                        artist: resolved.artist.as_deref(),
                        album: resolved.album.as_deref(),
                        cover_url: resolved.cover_url.as_deref(),
                        duration_ms: resolved.duration_ms,
                        file_size: resolved.file_size,
                        file_path: None,
                        sample_rate: resolved.sample_rate,
                        bit_depth: resolved.bit_depth,
                        channels: resolved.channels,
                        live_stream: false,
                        byte_seekable: true,
                        origin_url: None,
                        source: resolved.source.as_deref(),
                        source_id: resolved.source_id.as_deref(),
                        track_number: resolved.track_number,
                        disc_number: resolved.disc_number,
                    };
                    if let Err(e) = output.set_next_media(&media).await {
                        warn!(zone_id, error = %e, resolve_ms, "gapless_set_next_failed");
                        GaplessPrep::NotArmed
                    } else {
                        info!(
                            zone_id,
                            title = %resolved.title,
                            resolve_ms,
                            streaming = is_streaming,
                            "gapless_next_set"
                        );
                        GaplessPrep::Armed(arme)
                    }
                } else {
                    GaplessPrep::NotArmed
                }
            }
            Err(e) => {
                warn!(
                    zone_id,
                    error = %e,
                    resolve_ms = t0.elapsed().as_millis() as u64,
                    "gapless_resolve_failed"
                );
                GaplessPrep::NotArmed
            }
        }
    }
}
