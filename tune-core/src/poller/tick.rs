use super::*;

impl PositionPoller {
    pub(super) async fn tick(
        &self,
        poll_states: &mut HashMap<i64, ZonePollState>,
        idle_backoff: &mut HashMap<i64, IdlePollBackoff>,
        startup_at: &Instant,
    ) {
        let states = self.playback.all_states().await;

        poll_states.retain(|zone_id, _| {
            states
                .iter()
                .any(|s| s.zone_id == *zone_id && s.state == PlayState::Playing)
        });

        // Also poll stopped zones to detect externally-started playback and sync volume
        let all_zones = crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .list()
            .unwrap_or_default();

        // Ne pas laisser l'état de recul survivre à une zone supprimée.
        idle_backoff.retain(|zone_id, _| all_zones.iter().any(|z| z.id == Some(*zone_id)));

        for zone in &all_zones {
            let zone_id = zone.id.unwrap_or(0);
            if zone_id == 0 {
                continue;
            }
            let device_id = match zone.output_device_id.as_deref() {
                Some(id) if !id.is_empty() => id.to_string(),
                _ => continue,
            };

            let in_states = states
                .iter()
                .any(|s| s.zone_id == zone_id && s.state == PlayState::Playing);
            if in_states {
                continue;
            } // already handled below

            // Recul après échec : sans cela un appareil injoignable était sondé
            // chaque seconde sans fin (voir IdlePollBackoff).
            if idle_backoff.entry(zone_id).or_default().should_skip() {
                continue;
            }

            let status = {
                let output_arc = {
                    let outputs = self.outputs.lock().await;
                    match outputs.get(&device_id) {
                        Some(o) => o,
                        None => continue,
                    }
                };
                match get_status_with_signal_path_bounded(&output_arc, *STATUS_POLL_TIMEOUT).await {
                    Ok((s, signal_path, dsp_metrics)) => {
                        let b = idle_backoff.entry(zone_id).or_default();
                        b.record_success(s.state);
                        // Clôture de panne (#2566) : muette si le sondage
                        // n'avait jamais échoué.
                        b.journal.succes(zone_id, &device_id);
                        // Le curseur de volume est inerte tant que dure le DoP :
                        // l'état de zone doit le dire au client (#1735).
                        self.playback.set_dop_active(zone_id, s.dop_active).await;
                        self.playback
                            .set_output_signal_path(zone_id, signal_path)
                            .await;
                        self.playback
                            .set_output_dsp_metrics(zone_id, dsp_metrics)
                            .await;
                        s
                    }
                    Err(e) => {
                        let b = idle_backoff.entry(zone_id).or_default();
                        b.record_failure();
                        // Plafonné, sur le modèle de #2890 (#2566) : une panne
                        // durable dit sa cause quelques fois, puis se
                        // récapitule aux paliers de doublement. Le recul
                        // lui-même, lui, ne change pas d'un tick.
                        let skip_ticks = b.remaining;
                        b.journal.echec(zone_id, &device_id, &e, skip_ticks);
                        continue;
                    }
                }
            };

            // Sync volume from device only when playing AND the device
            // reports a significantly different volume from what we have in
            // memory.  Many DLNA renderers report a stale default (e.g. 50%)
            // right after playback starts, which would overwrite the user's
            // saved volume. Skip during the first 30s after startup to let
            // restore_zone_volumes take precedence over device defaults.
            let in_startup_grace = startup_at.elapsed().as_secs() < 30;
            let in_volume_grace = self
                .playback
                .get_state(zone_id)
                .await
                .last_volume_set_at
                .is_some_and(|t| t.elapsed().as_secs() < VOLUME_GRACE_SECS);
            if !zone.fixed_volume
                && !in_startup_grace
                && !in_volume_grace
                && status.volume > 0.001
                && status.volume < 0.999
                && status.state == TransportState::Playing
            {
                let db_vol = zone.volume / 100.0;
                let prev_device_vol = poll_states.get(&zone_id).and_then(|p| p.last_device_volume);
                // Edge-triggered: adopt the renderer's volume only when it
                // actually moved since the last poll (see decisions::
                // should_adopt_device_volume), so a stale default (Fabien's
                // Devialet stuck at 50%) can't overwrite the saved volume.
                if decisions::should_adopt_device_volume(prev_device_vol, status.volume, db_vol) {
                    self.playback.set_volume(zone_id, status.volume).await;
                    // #2886 — `as i32` TRONQUAIT : le volume adopte du renderer
                    // tombait a 0 sous 0,01 lineaire (-40 dB).
                    let vol_pct = status.volume * 100.0;
                    crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
                        .update_volume(zone_id, vol_pct)
                        .ok();
                }
                // Remember what the renderer reported so the next tick can
                // detect a genuine change.
                if let Some(ps) = poll_states.get_mut(&zone_id) {
                    ps.last_device_volume = Some(status.volume);
                }
            }

            // Recover playing state from device — only if Tune was actually
            // playing on this zone before (last_play_state == "playing" in DB).
            // Without this check, playback from other apps (Roon, Spotify
            // Connect, etc.) on a shared renderer (Sonos) would be captured
            // by Tune and trigger phantom queue playback when the other app stops.
            // Skip recovery during startup grace (30s) — the orchestrator may
            // still be sending play commands and the renderer reports Playing
            // before PlaybackManager is updated.
            // Re-read PlaybackManager state AFTER the device poll to avoid
            // the race where orchestrator.play() sets Playing between the
            // initial states read and the device poll response.
            let fresh_states = self.playback.all_states().await;
            let already_playing = fresh_states
                .iter()
                .any(|s| s.zone_id == zone_id && s.state == PlayState::Playing);

            // ── Le renderer nous appartient-il encore ? ──
            //
            // Deux serveurs Tune sur le même appareil, ou son lecteur interne
            // qui reprend la main après un redémarrage : chaque perdant
            // échouait EN SILENCE, l'interface relançait toutes les quinze
            // secondes, et un conflit d'appareil s'est déguisé en « bug DSD »
            // (DMP-A8, 24/08). On regarde l'URI que le renderer rapporte, et
            // on DIT à l'utilisateur qui tient l'appareil — une fois par
            // piste, après trois ticks concordants (une transition peut
            // montrer un instant l'URI précédente).
            if already_playing {
                let notre_stream_id = fresh_states
                    .iter()
                    .find(|s| s.zone_id == zone_id)
                    .and_then(|s| s.now_playing.as_ref())
                    .and_then(|np| np.stream_id.clone());
                if let Some(ps) = poll_states.get_mut(&zone_id) {
                    use decisions::TenueDuRenderer;
                    let verdict = decisions::qui_tient_le_renderer(
                        status.current_uri.as_deref(),
                        notre_stream_id.as_deref(),
                    );
                    // L'URI vide ne dit « lecteur interne » que si le
                    // transport est actif : un renderer arrêté a le droit de
                    // n'avoir rien chargé.
                    let etrangere = match &verdict {
                        TenueDuRenderer::LeNotre => false,
                        TenueDuRenderer::LecteurInterne => status.state == TransportState::Playing,
                        _ => true,
                    };
                    if etrangere {
                        ps.tenue_etrangere_ticks = ps.tenue_etrangere_ticks.saturating_add(1);
                        if ps.tenue_etrangere_ticks >= 3 && !ps.tenue_signalee {
                            ps.tenue_signalee = true;
                            let message = match &verdict {
                                TenueDuRenderer::AutreServeurTune(hote) => format!(
                                    "Cet appareil est tenu par un autre serveur Tune ({hote}).                                      Arrêtez la lecture sur ce serveur-là, ou choisissez un autre appareil."
                                ),
                                TenueDuRenderer::LecteurInterne => "L'appareil joue depuis sa propre                                      interface. Arrêtez la lecture sur l'appareil lui-même, puis relancez."
                                    .to_string(),
                                _ => "Cet appareil est tenu par une autre application.                                      Arrêtez-y la lecture, puis relancez."
                                    .to_string(),
                            };
                            warn!(
                                zone_id,
                                device = %device_id,
                                verdict = ?verdict,
                                uri = ?status.current_uri,
                                "renderer_tenu_par_un_tiers — la lecture demandée ne sortira pas"
                            );
                            if let Some(ref bus) = self.event_bus {
                                bus.emit(
                                    "zone.playback_error",
                                    serde_json::json!({
                                        "zone_id": zone_id,
                                        "error": message,
                                        // `fatal` : rien ne se rétablira tout
                                        // seul, l'utilisateur doit agir — le
                                        // message le lui dit.
                                        "fatal": true,
                                    }),
                                );
                            }
                        }
                    } else {
                        ps.tenue_etrangere_ticks = 0;
                    }
                }
            }
            if status.state == TransportState::Playing && !already_playing && !in_startup_grace {
                let last_state =
                    ZoneRepo::with_backend(self.db.clone()).get_last_play_state(zone_id);
                if last_state.as_deref() == Some("playing") {
                    // Re-resolve the zone's REAL last-played track from the
                    // persisted state instead of a bogus "Recovering..."
                    // placeholder with track_id = None. That placeholder (a) showed
                    // as a phantom track "that corresponds to nothing" and (b) made
                    // resume() replay a track it couldn't play, so pressing play
                    // after a zone switch did nothing (#729).
                    let zone = ZoneRepo::with_backend(self.db.clone())
                        .get(zone_id)
                        .ok()
                        .flatten();
                    let last_track_id = zone.as_ref().and_then(|z| z.last_track_id);
                    let last_source = zone
                        .as_ref()
                        .and_then(|z| z.last_track_source.clone())
                        .unwrap_or_else(|| "local".into());
                    let last_source_id = zone.as_ref().and_then(|z| z.last_track_source_id.clone());
                    // Prefer the persisted local track's real metadata; fall back
                    // to what the device reports. Never invent a placeholder title.
                    let db_track = last_track_id.and_then(|tid| {
                        crate::db::track_repo::TrackRepo::with_backend(self.db.clone())
                            .get(tid)
                            .ok()
                            .flatten()
                    });
                    let title = db_track
                        .as_ref()
                        .map(|t| t.title.clone())
                        .or_else(|| status.track_title.clone());

                    // #2991 — ce chemin posait `stream_id: None` en dur. Sur une
                    // RADIO, ce `None` se recopie ensuite dans chaque
                    // now-playing produit par `refresh_radio_metadata`, et le
                    // titre cesse définitivement d'être publié vers le flux :
                    // l'écran du lecteur réseau reste figé sur le premier
                    // morceau pendant que l'interface Tune, elle, suit.
                    //
                    // Le renderer annonce l'URI qu'il tire ; on y RELIT
                    // l'identifiant, puis on ne l'adopte que si le gestionnaire
                    // de flux connaît cette session — `streamer_bytes_sent`
                    // rend `None` pour un flux inconnu, ce qui écarte l'URI
                    // d'un AUTRE serveur Tune du réseau.
                    let stream_id_repris =
                        match decisions::stream_id_de_l_uri(status.current_uri.as_deref()) {
                            Some(sid)
                                if self.orchestrator.streamer_bytes_sent(&sid).await.is_some() =>
                            {
                                Some(sid)
                            }
                            _ => None,
                        };
                    // Only recover if we actually know what is playing — otherwise
                    // skip so a titleless device blip never surfaces as a phantom.
                    if let Some(title) = title {
                        let np = crate::playback::NowPlaying {
                            track_id: last_track_id,
                            title,
                            artist_name: db_track
                                .as_ref()
                                .and_then(|t| t.artist_name.clone())
                                .or_else(|| status.track_artist.clone()),
                            album_title: db_track.as_ref().and_then(|t| t.album_title.clone()),
                            cover_path: db_track.as_ref().and_then(|t| t.cover_path.clone()),
                            duration_ms: db_track
                                .as_ref()
                                .map(|t| t.duration_ms)
                                .unwrap_or(status.duration_ms as i64),
                            source: last_source,
                            source_id: last_source_id,
                            stream_id: stream_id_repris,
                            ..Default::default()
                        };
                        let stream_id_journal =
                            np.stream_id.as_deref().unwrap_or("absent").to_string();
                        self.playback.play(zone_id, np).await;
                        info!(
                            zone_id,
                            device = %device_id,
                            stream_id = %stream_id_journal,
                            "playback_recovered_from_device"
                        );
                    } else {
                        debug!(
                            zone_id,
                            device = %device_id,
                            "playback_recovery_skipped_unknown_track"
                        );
                    }
                } else {
                    debug!(
                        zone_id,
                        device = %device_id,
                        last_state = ?last_state,
                        "playback_recovery_skipped_not_tune_playback"
                    );
                }
            }
        }

        for zone_state in &states {
            if zone_state.state != PlayState::Playing {
                continue;
            }

            let zone_id = zone_state.zone_id;
            let device_id = match self.get_zone_device_id(zone_id) {
                Some(id) => id,
                // Pas de peripherique de sortie : une zone navigateur, par
                // conception (le client web tire `stream_url` lui-meme).
                //
                // Cette porte renvoyait tout le monde, et le rafraichissement
                // des metadonnees radio vit 400 lignes plus bas, dans le bloc
                // qui suit l'interrogation du peripherique. Consequence : sur
                // « Cet ordinateur », `fetch_radio_metadata` n'etait JAMAIS
                // appele. Pas d'echec, pas de trace — l'appel n'existait pas.
                //
                // Ce n'etait pas une regression : ca n'a jamais marche. D'ou
                // deux testeurs sur la meme station et la meme version avec
                // des resultats opposes (Jean Valjean sur une vraie sortie :
                // titre, interprete et paroles ; Bilou sur « Cet ordinateur » :
                // rien). Fil forum « Metadonnees radio disparues ? ».
                //
                // Le reste de la boucle (transport, gapless, fin de piste) n'a
                // effectivement rien a faire ici : on garde le `continue`.
                None => {
                    let ps = poll_states
                        .entry(zone_id)
                        .or_insert_with(|| ZonePollState::new(zone_state.track_generation));
                    // Meme etranglement que la zone avec peripherique : le tick
                    // est a la seconde, l'API de la station non.
                    if decisions::deviceless_radio_refresh_due(
                        zone_state.state == PlayState::Playing,
                        zone_state.now_playing.as_ref().map(|np| np.source.as_str()),
                        ps.last_radio_poll.elapsed(),
                        RADIO_POLL_INTERVAL_SECS,
                    ) {
                        ps.last_radio_poll = Instant::now();
                        self.refresh_radio_metadata(zone_id, zone_state).await;
                    }

                    // Zone navigateur : l'annonce « en écoute » que le
                    // démarrage a mise en attente part d'ICI, une fois
                    // constaté que l'onglet tire réellement le flux (#1998).
                    //
                    // Le démarrage ne peut pas trancher : sans périphérique de
                    // sortie, `output_sent` y vaut toujours faux, qu'on écoute
                    // ou non. Le seul fait observable est la consommation du
                    // flux, et elle n'apparaît qu'après coup — c'est
                    // exactement ce qu'une boucle de scrutation est là pour
                    // voir. L'orchestrateur ne fait rien tant qu'il n'a rien
                    // en attente pour ce flux, donc ce tick ne coûte qu'une
                    // comparaison sur toutes les autres zones.
                    if let Some(stream_id) = zone_state
                        .now_playing
                        .as_ref()
                        .and_then(|np| np.stream_id.as_deref())
                    {
                        self.orchestrator
                            .confirmer_lecture_navigateur(zone_id, stream_id)
                            .await;
                    }

                    // … et le versant symétrique : l'ABSENCE de preuve.
                    //
                    // #2657 a appris à cette branche à LIBÉRER l'annonce quand
                    // l'onglet tire le flux. Rien ne lui apprenait à RENONCER
                    // quand personne ne le tire : la zone restait « en
                    // lecture » pour toujours, barre de progression comprise,
                    // alors que le démarrage avait déjà renoncé à envoyer quoi
                    // que ce soit (`output_sent=false`, #2630).
                    if self.abandonner_lecture_sans_destination(zone_state).await {
                        poll_states.remove(&zone_id);
                    }
                    continue;
                }
            };

            let ps = poll_states
                .entry(zone_id)
                .or_insert_with(|| ZonePollState::new(zone_state.track_generation));

            // Detect track change: if the generation changed, the orchestrator
            // started a new track (via play() / play_from_queue / next / previous).
            // Reset all per-track poller state so stale values from the previous
            // track (peak_position, gapless flags, etc.) cannot cause false
            // gapless advances or premature track-end detection.
            //
            // Exception: if last_seek_at is recent (< 10s), this generation
            // change is from a seek (which recreates the stream), not a real
            // track change. In that case, preserve position state to avoid
            // the seek bar jumping back to 0.
            if ps.track_generation != zone_state.track_generation {
                let is_seek = zone_state
                    .last_seek_at
                    .map(|t| t.elapsed().as_secs() < 10)
                    .unwrap_or(false);

                if is_seek {
                    info!(
                        zone_id,
                        old_gen = ps.track_generation,
                        new_gen = zone_state.track_generation,
                        position_ms = zone_state.position_ms,
                        "poller_generation_changed_during_seek_preserving_position"
                    );
                } else {
                    info!(
                        zone_id,
                        old_gen = ps.track_generation,
                        new_gen = zone_state.track_generation,
                        "poller_track_generation_changed_resetting_state"
                    );
                    ps.last_position_ms = 0;
                    ps.peak_position_ms = 0;
                    ps.scrobbled_key = None;
                    ps.last_bytes_sent = 0;
                    ps.playing_stall_ticks = 0;
                    ps.stall_declines = 0;
                    ps.past_end_ticks = 0;
                    ps.track_started_at = Some(Instant::now());
                }
                ps.gapless_sent = false;
                ps.gapless_sent_at = None;
                ps.gapless_cooldown = 0;
                ps.stopped_ticks = 0;
                ps.track_generation = zone_state.track_generation;
                ps.tenue_etrangere_ticks = 0;
                ps.tenue_signalee = false;
                ps.track_loaded_at = Instant::now();
                ps.past_end_ticks = 0;
                ps.gapless_advance_pending = false;
                ps.gapless_stuck_ticks = 0;
                // Re-arm the DLNA poll-fail wall-clock fallback for the new track.
                ps.wall_clock_end_fired = false;
                // Le constat de depassement vaut pour UNE piste (#2493).
                ps.depassement_duree_ticks = 0;
                ps.depassement_duree_signale = false;
                // Force one gapless_arm_trace line at the start of the new track.
                ps.gapless_arm_logged = None;
                ps.gapless_dsd_skip_pos = None;
                ps.gapless_armed = None;
            }

            // Scrobble the current track once it has genuinely been listened past
            // the Last.fm threshold (50% or 4 min). Driven from a single place
            // that sees every track regardless of how it was reached (direct
            // play, gapless, prefetch) and uses the live position — unlike the old
            // play-start dispatch that scrobbled instantly on a skip and dropped
            // every prefetched track (Bilou, #1113). Radio and sub-30s / unknown
            // tracks are excluded by `should_dispatch_scrobble`. The latch is
            // keyed on the track's identity, so a gapless metadata advance —
            // which swaps now-playing WITHOUT bumping track_generation — re-arms
            // it for the new track (tracks 2, 4, 6… of an album, #1113).
            if zone_state.state == PlayState::Playing
                && let Some(np) = zone_state.now_playing.as_ref()
            {
                let key = decisions::scrobble_track_key(
                    zone_state.track_generation,
                    zone_state.queue_position,
                    np.track_id,
                    &np.title,
                    np.artist_name.as_deref(),
                );
                if decisions::should_dispatch_scrobble(
                    ps.scrobbled_key.as_deref(),
                    &key,
                    &np.source,
                    np.duration_ms,
                    zone_state.position_ms,
                ) {
                    self.orchestrator.dispatch_scrobble(
                        &np.title,
                        np.artist_name.as_deref(),
                        np.album_title.as_deref(),
                    );
                    ps.scrobbled_key = Some(key);
                }
            }

            if ps.backoff_remaining > 0 {
                ps.backoff_remaining -= 1;
                continue;
            }

            // Radio zones: throttle polling to every RADIO_POLL_INTERVAL_SECS.
            // Polling a DLNA renderer (especially DMP-A8) every second with 4
            // SOAP calls while it plays an infinite radio stream causes buffer
            // underruns, noise, and playback cuts.  Radio has no meaningful
            // position/duration — only transport state and metadata matter,
            // and those change slowly.
            let is_radio = zone_state
                .now_playing
                .as_ref()
                .map(|np| np.source == "radio")
                .unwrap_or(false);
            if is_radio
                && !decisions::radio_poll_due(
                    ps.last_radio_poll.elapsed(),
                    RADIO_POLL_INTERVAL_SECS,
                )
            {
                continue;
            }

            ps.total_polls += 1;
            let poll_start = Instant::now();

            // A push-based output that failed on its own thread reports it
            // here. Handle it before any status-based reasoning (Yacine,
            // 8 Aug 2026).
            //
            // Ce commentaire disait « les heuristiques de blocage plus bas
            // finiraient par arrêter la zone, en ~73 s ». MESURÉ FAUX sur
            // #3108 : `dlna_playing_stall_eligible` exige
            // `output_type == "dlna"`. Pour une sortie LOCALE, ce bloc n'est
            // pas le raccourci d'un filet plus lent — c'est le seul filet.
            // Une zone locale dont la position se fige n'est reprise par rien.
            {
                let output_arc = {
                    let outputs = self.outputs.lock().await;
                    match outputs.get(&device_id) {
                        Some(o) => o,
                        None => continue,
                    }
                };
                let failure = {
                    let output = output_arc.lock().await;
                    output.take_output_failure()
                };
                if let Some(msg) = failure {
                    warn!(
                        zone_id,
                        device = %device_id,
                        error = %msg,
                        "output_reported_failure_stopping_zone"
                    );
                    if let Some(ref bus) = self.event_bus {
                        // `fatal` tells the client this is not worth waiting
                        // out. It opens a 30 s grace window on every play so a
                        // slow HI-RES pre-transcode reads as "chargement…"
                        // rather than a failure (#1146) — but a device that
                        // refuses to open will never recover, and we now report
                        // it within a second, i.e. squarely inside that window.
                        // Without this flag the message would be swallowed and
                        // the user would be left with a spinner and nothing
                        // else — worse than the silence this whole change fixes.
                        bus.emit(
                            "zone.playback_error",
                            serde_json::json!({
                                "zone_id": zone_id,
                                "error": msg,
                                "fatal": true,
                            }),
                        );
                    }
                    poll_states.remove(&zone_id);
                    let device_id_ref = self.get_zone_device_id(zone_id);
                    self.orchestrator
                        .stop(zone_id, device_id_ref.as_deref())
                        .await;
                    continue;
                }
            }

            let status = {
                let output_arc = {
                    let outputs = self.outputs.lock().await;
                    match outputs.get(&device_id) {
                        Some(o) => o,
                        None => continue,
                    }
                };
                match get_status_with_signal_path_bounded(&output_arc, *STATUS_POLL_TIMEOUT).await {
                    Ok((s, signal_path, dsp_metrics)) => {
                        ps.consecutive_errors = 0;
                        // Clôture de panne (#2566) : muette si le sondage
                        // n'avait jamais cessé de répondre.
                        ps.journal.succes_lecture(zone_id, &device_id);
                        let latency = poll_start.elapsed().as_millis() as u32;
                        ps.last_latency_ms = latency;
                        if latency > ps.max_latency_ms {
                            ps.max_latency_ms = latency;
                        }
                        // Même report que sur le chemin « zone au repos » : un
                        // flux peut entrer ou sortir du DoP d'une piste à
                        // l'autre sans changement d'état de zone (#1735).
                        self.playback.set_dop_active(zone_id, s.dop_active).await;
                        self.playback
                            .set_output_signal_path(zone_id, signal_path)
                            .await;
                        self.playback
                            .set_output_dsp_metrics(zone_id, dsp_metrics)
                            .await;
                        s
                    }
                    Err(e) => {
                        ps.consecutive_errors = ps.consecutive_errors.saturating_add(1);
                        ps.total_errors += 1;
                        ps.backoff_remaining = 1u8 << ps.consecutive_errors.min(4);
                        // Les trois compteurs ci-dessus sont tenus AVANT, et le
                        // journal n'en touche aucun : une panne qui cesse
                        // d'être dite continue d'être comptée, donc le repli de
                        // fin de piste (`poll_failed_past_end`) et l'arrêt de
                        // zone décident sur exactement les mêmes chiffres
                        // qu'avant (#2566). Seul le volume du journal change :
                        // 1 ligne toutes les 17 s pour un appareil muet, sans
                        // fin, devient 5 lignes détaillées puis un récapitulatif
                        // aux paliers de doublement.
                        let backoff = ps.backoff_remaining;
                        ps.journal.echec_lecture(zone_id, &device_id, &e, backoff);

                        // Poll-fail end-of-track fallback for a DLNA renderer
                        // whose status poll errors outright (LMS UPnP bridge:
                        // GetPositionInfo SOAP fails). We get no state/position/
                        // duration, so end-of-track is decided purely on Tune's
                        // wall clock vs the queue-known duration. Guarded so a
                        // paused/stopped track (Tune not Playing), a mid-track
                        // blip (< a couple failures), a seek, or a re-fire can't
                        // false-advance. We can't remove the poll state here (it's
                        // borrowed), so a per-track latch prevents re-firing.
                        let is_dlna = all_zones
                            .iter()
                            .find(|z| z.id == Some(zone_id))
                            .and_then(|z| z.output_type.as_deref())
                            == Some("dlna");
                        let tune_playing = zone_state.state == PlayState::Playing;
                        let track_duration_ms = zone_state
                            .now_playing
                            .as_ref()
                            .map(|np| np.duration_ms as u64)
                            .unwrap_or(0);
                        let wall_elapsed = ps
                            .track_started_at
                            .map(|t| t.elapsed().as_secs())
                            .unwrap_or(0);
                        let in_seek_grace = zone_state
                            .last_seek_at
                            .map(|t| t.elapsed().as_secs() < SEEK_STREAMING_GRACE_SECS)
                            .unwrap_or(false);
                        if !in_seek_grace
                            && decisions::poll_failed_past_end(
                                is_dlna,
                                tune_playing,
                                track_duration_ms,
                                wall_elapsed,
                                ps.consecutive_errors,
                                ps.wall_clock_end_fired,
                            )
                        {
                            info!(
                                zone_id,
                                device = %device_id,
                                track_dur = track_duration_ms,
                                wall_secs = wall_elapsed,
                                consec_err = ps.consecutive_errors,
                                "dlna_poll_failed_wall_clock_advancing"
                            );
                            ps.wall_clock_end_fired = true;
                            self.handle_track_end(zone_id, zone_state).await;
                        }
                        continue;
                    }
                }
            };

            // Update last_radio_poll so the throttle gate works on next tick.
            if is_radio {
                ps.last_radio_poll = Instant::now();
            }

            // Radio zones: after the throttled poll, only check transport
            // state (is it still playing?) and do metadata polling.
            // Skip position tracking, gapless logic, and track-end detection
            // — none of that applies to infinite streams.
            if is_radio {
                // A renderer that is truly streaming reports an advancing
                // position even when it (mis)reports Stopped for a live source
                // — the Yamaha R-N2000A does this on MP3 ICEcast streams (AAC
                // plays fine). Only treat the radio as stopped if it's Stopped
                // AND the position is NOT advancing; otherwise the auto-retry
                // below keeps restarting a stream the renderer is happily
                // playing (Cyrille: TSF Jazz / Radio Classique cut every ~45s).
                let radio_position_advancing = status.position_ms > ps.last_radio_position_ms;
                ps.last_radio_position_ms = status.position_ms;
                let radio_stopped =
                    status.state == TransportState::Stopped && !radio_position_advancing;

                if !radio_stopped {
                    ps.radio_stopped_ticks = 0;
                    // Still playing — sync volume only.
                    let zone_fixed_volume = all_zones
                        .iter()
                        .find(|z| z.id == Some(zone_id))
                        .map(|z| z.fixed_volume)
                        .unwrap_or(false);
                    let in_vol_grace = zone_state
                        .last_volume_set_at
                        .is_some_and(|t| t.elapsed().as_secs() < VOLUME_GRACE_SECS);
                    // Edge-triggered like the main volume-sync path, so a radio
                    // renderer reporting a stale default can't keep resetting the
                    // saved volume (Fabien's Devialet Salon reverting to 50).
                    if !zone_fixed_volume
                        && !in_vol_grace
                        && status.volume < 0.999
                        && decisions::should_adopt_device_volume(
                            ps.last_device_volume,
                            status.volume,
                            zone_state.volume,
                        )
                    {
                        self.playback.set_volume(zone_id, status.volume).await;
                        // #2886 — `as i32` TRONQUAIT : le volume adopte du renderer
                        // tombait a 0 sous 0,01 lineaire (-40 dB).
                        let vol_pct = status.volume * 100.0;
                        let db = self.db.clone();
                        crate::db::zone_repo::ZoneRepo::with_backend(db)
                            .update_volume(zone_id, vol_pct)
                            .ok();
                    }
                    ps.last_device_volume = Some(status.volume);
                }

                // Le titre diffuse par une webradio NE DEPEND PAS de l'etat du
                // renderer : il se lit sur une API externe (Radio Paradise,
                // Radio France) ou dans le flux ICY, tous deux independants de
                // ce que fait l'appareil. Rien ne justifiait de conditionner
                // cette lecture a la bonne sante du transport.
                //
                // C'etait pourtant le cas : l'appel vivait dans le
                // `if !radio_stopped` ci-dessus, aux cotes de la synchro de
                // volume — qui, elle, a bien besoin d'un renderer en lecture.
                // Consequence : un renderer qui ne demarre pas figeait
                // l'affichage sur le nom de la station, et un bug de LECTURE se
                // deguisait en bug de METADONNEES. Bilou a ouvert deux fils
                // distincts pour un seul probleme (#1522, #1492).
                //
                // La garde sur le peripherique de sortie, elle, est tombee
                // avec #1536.
                self.refresh_radio_metadata(zone_id, zone_state).await;

                // Sync metrics and skip the rest of the loop (no gapless/track-end).
                self.shared_metrics.lock().await.insert(
                    zone_id,
                    ZonePollerMetrics {
                        total_polls: ps.total_polls,
                        total_errors: ps.total_errors,
                        consecutive_errors: ps.consecutive_errors,
                        last_latency_ms: ps.last_latency_ms,
                        max_latency_ms: ps.max_latency_ms,
                        // Chemin RADIO : un flux sans fin ne depasse aucune
                        // duree, et ce bras n'evalue meme pas le predicat
                        // (#2493). Constat toujours faux, par construction.
                        lecture_au_dela_de_la_duree: false,
                    },
                );

                if radio_stopped {
                    ps.radio_stopped_ticks = ps.radio_stopped_ticks.saturating_add(1);
                    if ps.radio_stopped_ticks >= 3 && ps.radio_stopped_ticks < 6 {
                        if zone_state.track_generation != ps.track_generation {
                            debug!(zone_id, "radio_auto_retry_skipped_generation_changed");
                            ps.radio_stopped_ticks = 0;
                        } else {
                            info!(zone_id, ticks = ps.radio_stopped_ticks, "radio_auto_retry");
                            let device_id_ref = self.get_zone_device_id(zone_id);
                            if let Some(ref did) = device_id_ref {
                                if let Some(ref np) = zone_state.now_playing {
                                    if let Some(ref sid) = np.source_id {
                                        let req = crate::orchestrator::PlayRequest {
                                            zone_id,
                                            output_device_id: Some(did.clone()),
                                            track_id: None,
                                            source: Some("radio".into()),
                                            source_id: Some(sid.clone()),
                                            title: Some(np.title.clone()),
                                            artist_name: np.artist_name.clone(),
                                            album_title: np.album_title.clone(),
                                            cover_url: np.cover_path.clone(),
                                            duration_ms: None,
                                            seek_ms: None,
                                            temp_file_path: None,
                                            sample_rate: None,
                                            bit_depth: None,
                                            media_format: None,
                                            track_number: None,
                                            disc_number: None,
                                        };
                                        // Reconnecting the *same* station — do
                                        // not add a duplicate listen-history row.
                                        match self.orchestrator.play_without_history(req).await {
                                            Ok(_) => {
                                                info!(zone_id, "radio_auto_retry_success");
                                                ps.radio_stopped_ticks = 0;
                                            }
                                            Err(e) => {
                                                warn!(zone_id, error = %e, "radio_auto_retry_failed")
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else if ps.radio_stopped_ticks >= 6 {
                        info!(
                            zone_id,
                            ticks = ps.radio_stopped_ticks,
                            "radio_renderer_stopped_giving_up"
                        );
                        poll_states.remove(&zone_id);
                        let device_id_ref = self.get_zone_device_id(zone_id);
                        self.orchestrator
                            .stop(zone_id, device_id_ref.as_deref())
                            .await;
                    } else {
                        debug!(
                            zone_id,
                            ticks = ps.radio_stopped_ticks,
                            "radio_transient_stopped_tolerating"
                        );
                    }
                }
                continue;
            }

            // Check whether we're in the seek grace period: after a seek the
            // in-memory position is authoritative and the output may still
            // report the old (pre-seek) position until the stream restarts.
            // During this window we skip overwriting position to prevent the
            // progress bar from snapping back.
            //
            // For streaming sources (Qobuz/Tidal) on network outputs (DLNA),
            // seeking recreates the entire stream session — the renderer may
            // report Stopped for several seconds while it buffers the new
            // stream.  Use a longer grace period to prevent the poller from
            // accumulating stopped_ticks and false-skipping to the next track.
            let is_streaming_seek = zone_state.now_playing.as_ref().is_some_and(|np| {
                np.source != "local"
                    && np.source != "radio"
                    && np.source != "podcast"
                    && np.stream_id.is_some()
            }) && all_zones
                .iter()
                .find(|z| z.id == Some(zone_id))
                .and_then(|z| z.output_type.as_deref())
                .is_some_and(|t| {
                    matches!(
                        t,
                        "dlna" | "openhome" | "chromecast" | "bluos" | "squeezebox"
                    )
                });
            let seek_grace_secs = if is_streaming_seek {
                SEEK_STREAMING_GRACE_SECS
            } else {
                SEEK_GRACE_SECS
            };
            let in_seek_grace = zone_state
                .last_seek_at
                .map(|t| t.elapsed().as_secs() < seek_grace_secs)
                .unwrap_or(false);

            // Fold a NEW seek into the wall-clock baseline: rewind
            // track_started_at by the seek target so wall_elapsed reads as if
            // the track had played at 1x from position 0. Without this, a
            // seek near the end leaves wall_elapsed at a few seconds, and the
            // anti-spurious guards (played_enough, ended_naturally_wall_ok)
            // veto the REAL track end — playback stops instead of advancing
            // to the next track (DEvir, v0.9.0-rc4). Instant identity makes
            // this once-per-seek; play() clears last_seek_at on track change.
            if let Some(seek_at) = zone_state.last_seek_at {
                if ps.last_seek_seen != Some(seek_at) {
                    ps.last_seek_seen = Some(seek_at);
                    let target = Duration::from_millis(zone_state.position_ms.max(0) as u64);
                    ps.track_started_at =
                        Instant::now().checked_sub(target).or(ps.track_started_at);
                }
            }

            if !in_seek_grace {
                // Clamp the reported position to the track duration so the UI
                // progress bar doesn't briefly overshoot past the end. The
                // output can report a position a few seconds past the duration
                // during the past-end / gapless window before the track
                // advances (reported by DEvir). Internal poller logic keeps
                // using the raw status.position_ms (peak, past-end detection).
                let dur = zone_state
                    .now_playing
                    .as_ref()
                    .map(|np| np.duration_ms as i64)
                    .unwrap_or(0);
                let reported = if dur > 0 {
                    (status.position_ms as i64).min(dur)
                } else {
                    status.position_ms as i64
                };
                // La garde de monotonie vit dans `update_position` : elle sait,
                // elle, quels chemins ont le droit d'abaisser le plancher (une
                // COMMANDE — déplacement, changement de piste, avance gapless —
                // et jamais une observation). Elle rend la position RETENUE, et
                // c'est celle-là qu'il faut émettre : émettre `reported` ferait
                // diverger l'évènement `position` de l'état servi par
                // `GET /zones`, et l'écran reculerait quand même (#3229).
                let publiee = self.playback.update_position(zone_id, reported).await;
                self.playback.emit_position(zone_id, publiee);
            }

            // Sync volume from device (skip if fixed_volume)
            let zone_fixed_volume = all_zones
                .iter()
                .find(|z| z.id == Some(zone_id))
                .map(|z| z.fixed_volume)
                .unwrap_or(false);
            let in_vol_grace2 = zone_state
                .last_volume_set_at
                .is_some_and(|t| t.elapsed().as_secs() < VOLUME_GRACE_SECS);
            if !zone_fixed_volume
                && !in_vol_grace2
                && status.volume > 0.001
                && status.volume < 0.999
                && decisions::should_adopt_device_volume(
                    ps.last_device_volume,
                    status.volume,
                    zone_state.volume,
                )
            {
                self.playback.set_volume(zone_id, status.volume).await;
                // #2886 — `as i32` TRONQUAIT : le volume adopte du renderer
                // tombait a 0 sous 0,01 lineaire (-40 dB).
                let vol_pct = status.volume * 100.0;
                let db = self.db.clone();
                crate::db::zone_repo::ZoneRepo::with_backend(db)
                    .update_volume(zone_id, vol_pct)
                    .ok();
            }
            // Edge-triggered like the stopped/radio paths: record the reported
            // volume so a renderer stuck at a persistent default (HiFi Rose
            // RS130 reporting 25) can't repeatedly clobber the saved volume.
            // This normal-playing path was never migrated to the #358 predicate
            // and still used level-triggered adoption → auto-reset to 25%
            // (Philippe).
            ps.last_device_volume = Some(status.volume);

            // --- Persist position to DB periodically ---
            ps.ticks_since_db_save += 1;
            if ps.ticks_since_db_save >= POSITION_SAVE_INTERVAL_TICKS {
                ps.ticks_since_db_save = 0;
                let np = zone_state.now_playing.as_ref();
                let track_id = np.and_then(|n| n.track_id);
                let source = np.map(|n| n.source.as_str());
                let source_id = np.and_then(|n| n.source_id.as_deref());
                // Don't persist a position within END_MARGIN of the end — a
                // resume there would seek into the end zone and (on exclusive
                // outputs) bounce via the past-end detector. See
                // decisions::position_to_persist.
                let dur_ms = np.map(|n| n.duration_ms as i64).unwrap_or(0).max(0) as u64;
                let save_position_ms =
                    decisions::position_to_persist(status.position_ms, dur_ms) as i64;
                ZoneRepo::with_backend(self.db.clone())
                    .save_playback_position(zone_id, save_position_ms, track_id, source, source_id)
                    .ok();
            }

            // Track the high-water mark for position — used to verify that
            // Discard provably-stale early samples BEFORE they poison anything:
            // some renderers report the previous session's position for the
            // first seconds after a fresh Play (DMP-A6 → near-end staging at
            // +6s then phantom position_reset advance). Skip the whole
            // position-driven logic for this tick; the next honest sample
            // resumes it, and past 30s of wall time the guard stands down.
            let wall_elapsed_now = ps
                .track_started_at
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            // A non-realtime output is exempt: "position far ahead of the wall
            // clock" is a ghost only if playback runs at 1x. A recorder that
            // finished the capture reports position = duration straight away, so
            // this `continue` skipped the whole end-of-track path on every tick
            // until the wall clock caught up with the track's length.
            if status.realtime
                && decisions::stale_start_position(wall_elapsed_now, status.position_ms)
            {
                debug!(
                    zone_id,
                    pos_ms = status.position_ms,
                    wall_s = wall_elapsed_now,
                    "stale_start_position_ignored"
                );
                continue;
            }

            // enough of the track was actually played before accepting a
            // gapless transition.  We update this BEFORE checking for resets
            // so the peak reflects the last known good position.
            if status.position_ms > ps.peak_position_ms {
                ps.peak_position_ms = status.position_ms;
            }

            let track_duration_ms = zone_state
                .now_playing
                .as_ref()
                .map(|np| np.duration_ms as u64)
                .unwrap_or(0);

            // Helper: has enough of the track been played?
            // When track_duration is known: peak_position_ms >= 80% of duration.
            // When track_duration is unknown (0): require peak_position_ms >= 60s
            // to avoid false skips on slow renderers (Shanling SCD1.3 etc.)
            // that report duration=0 and briefly show Stopped while buffering.
            let wall_elapsed = ps
                .track_started_at
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            let played_enough =
                decisions::played_enough(track_duration_ms, ps.peak_position_ms, wall_elapsed);

            // Detect position reset: position drops from >30s to <5s.
            // This is a strong signal that the renderer performed a gapless
            // transition (the new track starts from 0).
            //
            // Arm on `gapless_sent` (the boolean), NOT `gapless_sent_at`
            // (the timestamp). SetNextAVTransportURI is sent GAPLESS_WINDOW_MS
            // (30s) before the end, but `gapless_sent_at` expires after
            // GAPLESS_GUARD_SECS (15s) — i.e. ~15s BEFORE the track actually
            // ends. Using the timestamp therefore disarmed this detection for
            // the entire final stretch of every track, so a renderer that
            // transitions seamlessly (continuous Playing, no Stopped blip) and
            // plays consecutive tracks of similar duration was caught by
            // NEITHER position_reset (disarmed) NOR the duration_changed path
            // (needs >2s duration difference). Tune then stayed one track
            // behind: its UI showed track N restarting while the renderer
            // played track N+1, and when the phantom track N passed its
            // duration handle_track_end re-issued play → track N+1 restarted on
            // the renderer (forum #1019, Marantz ND8006). `gapless_sent` stays
            // true from SetNext until the transition is detected or the track
            // generation changes, so it covers the whole window.
            // Snapshot the real previous position BEFORE it is overwritten
            // below — the diagnostic `info!` further down must log the genuine
            // prior sample, not the just-stored current one (the old code read
            // `ps.last_position_ms` after the overwrite, so `prev_pos` was
            // always mis-logged equal to `new_pos`).
            let prev_position_ms = ps.last_position_ms;
            let mut position_reset =
                decisions::position_reset(ps.last_position_ms, status.position_ms, ps.gapless_sent);
            // Suppress this metadata-only advance fallback for outputs that don't
            // do internal gapless (Chromecast, slimproto, exclusive local): for
            // them a position drop to 0 means the track ENDED (device IDLE /
            // FINISHED), not that it auto-advanced. Firing here sends no `play`
            // and steals the event from the natural-end path (Stopped branch →
            // play_from_queue = real load), causing Rhorn's 1-2s-then-zero loop
            // (#1072). Compute can_internal_gapless only when a raw reset fires
            // (rare: end of track). Env-guarded for rollback.
            if position_reset && std::env::var("TUNE_DISABLE_CAST_ADVANCE_FIX").is_err() {
                let can_internal_gapless = {
                    let outputs = self.outputs.lock().await;
                    match outputs.get(&device_id) {
                        Some(arc) => arc.lock().await.supports_internal_gapless(),
                        None => false,
                    }
                };
                position_reset = decisions::position_reset_fires(
                    position_reset,
                    can_internal_gapless,
                    in_seek_grace,
                );
                if !position_reset {
                    if in_seek_grace {
                        info!(zone_id, "gapless_advance_suppressed_after_seek");
                    } else {
                        info!(zone_id, "position_reset_deferred_to_natural_end");
                    }
                }
            }
            ps.last_position_ms = status.position_ms;

            if position_reset {
                if !played_enough {
                    warn!(
                        zone_id,
                        peak_pos = ps.peak_position_ms,
                        track_dur = track_duration_ms,
                        "gapless_position_reset_ignored_not_enough_played"
                    );
                } else {
                    // Real time elapsed between arming SetNext (next-track URL
                    // resolved) and the renderer actually transitioning. Key
                    // metric for streaming URL/token expiry diagnosis.
                    let arm_to_advance_ms = ps
                        .gapless_sent_at
                        .map(|t| t.elapsed().as_millis() as u64)
                        .unwrap_or(0);
                    info!(
                        zone_id,
                        prev_pos = prev_position_ms,
                        new_pos = status.position_ms,
                        arm_to_advance_ms,
                        "gapless_position_reset_detected"
                    );
                    ps.gapless_sent = false;
                    ps.gapless_sent_at = None;
                    // Retire de l'etat, gardee sous la main : c'est elle qui
                    // dit ou avancer (#3026).
                    let arme_avant = ps.gapless_armed.take();
                    ps.stopped_ticks = 0;
                    ps.past_end_ticks = 0;
                    ps.peak_position_ms = 0;
                    ps.last_position_ms = 0;
                    ps.last_bytes_sent = 0;
                    ps.playing_stall_ticks = 0;
                    ps.stall_declines = 0;
                    ps.track_started_at = Some(Instant::now());
                    ps.gapless_advance_pending = false;
                    ps.gapless_stuck_ticks = 0;
                    // A stall-recovery restart (OAAT stall supervisor) replays
                    // the CURRENT track from 0. That from-zero position drop
                    // trips `position_reset` exactly like a real gapless
                    // transition — but the renderer is still on the SAME track,
                    // so advancing would run now-playing one track ahead of the
                    // audio ("ça avance mais joue le morceau précédent", Xavier,
                    // OAAT Tune Endpoint). Suppress the advance for a brief
                    // window after a restart; the state resets above still run,
                    // so the next genuine transition re-arms and advances
                    // normally.
                    let recently_restarted = zone_state
                        .last_restart_at
                        .map(|t| {
                            t.elapsed()
                                < std::time::Duration::from_secs(RESTART_ADVANCE_SUPPRESS_SECS)
                        })
                        .unwrap_or(false);
                    if recently_restarted {
                        info!(zone_id, "gapless_advance_suppressed_after_restart");
                    } else if let Some(next_pos) = self
                        .position_a_avancer(zone_id, zone_state, arme_avant)
                        .await
                    {
                        info!(zone_id, next_pos, "gapless_advance_on_position_reset");
                        if let Err(e) = self
                            .orchestrator
                            .advance_queue_metadata(zone_id, next_pos)
                            .await
                        {
                            warn!(zone_id, error = %e, "gapless_advance_failed");
                        }
                        ps.gapless_cooldown = 4;
                        // The identity-keyed latch re-arms by itself on the new
                        // track; clearing it here additionally covers gapless
                        // repeat-one, where the advanced track has the same
                        // identity as the latched one (#1113).
                        ps.scrobbled_key = None;
                    }
                }
            }

            // Clear expired guard
            if let Some(sent_at) = ps.gapless_sent_at {
                if sent_at.elapsed() > std::time::Duration::from_secs(GAPLESS_GUARD_SECS) {
                    debug!(zone_id, "gapless_guard_expired");
                    ps.gapless_sent_at = None;
                }
            }

            let in_gapless_guard = ps.gapless_sent_at.is_some();

            let mut track_ended = false;
            // Quelle branche a conclu « la piste est finie ». Journalisé tel
            // quel par `track_end_gap` au moment d'enchaîner (#2488) : sans
            // lui, le journal ne dit pas laquelle des cinq portes de sortie a
            // servi, et donc pas quel plancher de silence a été payé.
            let mut motif_fin_de_piste: &'static str = "";
            let mut force_stop = false;
            let mut force_stop_demarrage_mort = false;

            // Guard: if Tune's own playback state for this zone is Stopped
            // (or has no now_playing), ignore device state changes entirely.
            // This prevents phantom playback when another app (e.g. Roon)
            // plays on a shared renderer (e.g. Sonos) and then stops —
            // Tune would otherwise interpret the Stopped→Playing cycle as
            // its own track ending and auto-advance to the next queue item.
            let tune_is_playing =
                zone_state.state == PlayState::Playing || zone_state.state == PlayState::Paused;
            let tune_has_track = zone_state.now_playing.is_some();

            match status.state {
                TransportState::Stopped if !tune_is_playing || !tune_has_track => {
                    // Tune is not playing on this zone — ignore device Stopped.
                    ps.stopped_ticks = 0;
                    ps.playing_stall_ticks = 0;
                }
                TransportState::Stopped => {
                    ps.playing_stall_ticks = 0;
                    // During the seek grace period, the renderer may report
                    // Stopped while it buffers the new stream (especially for
                    // streaming seeks that recreate the session).  Suppress
                    // stopped_ticks to prevent false track-end detection.
                    // Not for a non-realtime output. The grace exists to let a
                    // renderer buffer, and its `peak < 5s` condition reads "no
                    // audio has come out yet" — but a recorder that finished the
                    // whole capture in two seconds legitimately never reports a
                    // position past 5s, so the grace held every track back for
                    // its full 45s and a rip crawled at ~46s per track.
                    let in_track_load_grace = status.realtime
                        && ps.track_loaded_at.elapsed().as_secs() < TRACK_LOAD_GRACE_SECS
                        && ps.peak_position_ms < 5_000;
                    // A DSD track on a DLNA renderer whose peak reached the end.
                    // Gapless is intentionally not armed for a DSD next on DLNA
                    // (prepare_gapless / #402) and DLNA never reports
                    // ended_naturally, so without this fast path the track only
                    // ends after STOPPED_TICKS_THRESHOLD polls — a fixed ~5s gap
                    // between DSD tracks (Benjithom, RS130).
                    let dlna_dsd_reached_end = decisions::dlna_dsd_reached_end(
                        all_zones
                            .iter()
                            .find(|z| z.id == Some(zone_id))
                            .and_then(|z| z.output_type.as_deref())
                            .unwrap_or(""),
                        zone_state
                            .now_playing
                            .as_ref()
                            .and_then(|np| np.format.as_deref()),
                        track_duration_ms,
                        ps.peak_position_ms,
                    );
                    // v0.9 rc.2 FSM shadow: snapshot the Stopped-arm inputs
                    // (pre-mutation) so classify_stopped can be compared to the
                    // arm's real outcome under TUNE_POLLER_FSM_SHADOW. Cheap
                    // (no I/O); the compare/log at the arm tail is flag-gated.
                    let mut fsm_in = fsm::StoppedInput {
                        tune_is_playing,
                        tune_has_track,
                        in_seek_grace,
                        in_track_load_grace,
                        gapless_cooldown: ps.gapless_cooldown,
                        in_gapless_guard,
                        played_enough,
                        gapless_advance_pending: ps.gapless_advance_pending,
                        gapless_stuck_ticks: ps.gapless_stuck_ticks,
                        ended_naturally: status.ended_naturally,
                        wall_elapsed,
                        track_duration_ms,
                        stopped_ticks: ps.stopped_ticks,
                        natural_end: decisions::natural_end(
                            played_enough,
                            matches!(zone_state.repeat, RepeatMode::One | RepeatMode::All),
                            ps.peak_position_ms,
                            status.ended_naturally,
                            wall_elapsed,
                            track_duration_ms,
                            status.realtime,
                        ),
                        gapless_sent: ps.gapless_sent,
                        realtime: status.realtime,
                        // Refined by the natural-end branch below (live probe),
                        // same late-update pattern as `consommation`.
                        can_internal_gapless: true,
                        // Rien n'a encore été mesuré sur ce tour : « inconnue »
                        // est le seul départ honnête (#2394). La branche du
                        // seuil d'échec, seule à interroger le gestionnaire de
                        // flux, la remplace par un verdict mesuré.
                        consommation: fsm::ConsommationFlux::Inconnue,
                        dlna_dsd_reached_end,
                    };
                    let mut fsm_actual: Option<fsm::StoppedOutcome>;
                    if in_seek_grace {
                        fsm_actual = Some(fsm::StoppedOutcome::SuppressSeekGrace);
                        ps.stopped_ticks = 0;
                        debug!(
                            zone_id,
                            seek_grace_secs = seek_grace_secs,
                            "seek_grace_suppressing_stopped_ticks"
                        );
                    } else if in_track_load_grace {
                        fsm_actual = Some(fsm::StoppedOutcome::SuppressLoadGrace);
                        ps.stopped_ticks = 0;
                        debug!(
                            zone_id,
                            elapsed = ps.track_loaded_at.elapsed().as_secs(),
                            grace = TRACK_LOAD_GRACE_SECS,
                            "track_load_grace_suppressing_stopped_ticks"
                        );
                    } else if ps.gapless_cooldown > 0 {
                        fsm_actual = Some(fsm::StoppedOutcome::SuppressCooldown);
                        ps.gapless_cooldown -= 1;
                        ps.stopped_ticks = 0;
                    } else if in_gapless_guard {
                        if !played_enough {
                            fsm_actual = Some(fsm::StoppedOutcome::GuardStoppedIgnored);
                            // Renderer reported Stopped during guard but not
                            // enough of the track was played — ignore to avoid
                            // false skip (DMP-A8 quirk).
                            debug!(
                                zone_id,
                                peak_pos = ps.peak_position_ms,
                                track_dur = track_duration_ms,
                                "gapless_guard_stopped_ignored_not_enough_played"
                            );
                        } else {
                            fsm_actual = Some(fsm::StoppedOutcome::GuardStoppedPending);
                            // During the gapless guard period, a Stopped state
                            // MAY mean the renderer transitioned via gapless.
                            // Don't advance metadata yet — wait for the renderer
                            // to report Playing (position reset) to confirm.
                            // If it stays Stopped, the stuck handler will force
                            // play_from_queue which handles metadata correctly.
                            info!(zone_id, "gapless_guard_stopped_pending_confirmation");
                            ps.gapless_sent = false;
                            ps.gapless_armed = None;
                            ps.gapless_sent_at = None;
                            ps.stopped_ticks = 0;
                            ps.peak_position_ms = 0;
                            ps.last_position_ms = 0;
                            ps.track_started_at = None;
                            ps.gapless_advance_pending = true;
                            ps.gapless_stuck_ticks = 0;
                            ps.gapless_cooldown = 4;
                        }
                    } else if ps.gapless_advance_pending {
                        // The poller advanced metadata expecting the renderer
                        // to auto-transition via gapless, but the renderer is
                        // still Stopped after the cooldown expired.  Count
                        // stuck ticks and force play_from_queue if the renderer
                        // doesn't pick up within GAPLESS_STUCK_THRESHOLD.
                        ps.gapless_stuck_ticks += 1;
                        if ps.gapless_stuck_ticks >= GAPLESS_STUCK_THRESHOLD {
                            fsm_actual = Some(fsm::StoppedOutcome::StuckForceEnd);
                            warn!(
                                zone_id,
                                stuck_ticks = ps.gapless_stuck_ticks,
                                "gapless_advance_stuck_forcing_play"
                            );
                            ps.gapless_advance_pending = false;
                            ps.gapless_stuck_ticks = 0;
                            ps.stopped_ticks = 0;
                            track_ended = true;
                            motif_fin_de_piste = decisions::motif_fin::AVANCE_GAPLESS_BLOQUEE;
                        } else {
                            fsm_actual = Some(fsm::StoppedOutcome::StuckWaiting);
                            debug!(
                                zone_id,
                                stuck_ticks = ps.gapless_stuck_ticks,
                                threshold = GAPLESS_STUCK_THRESHOLD,
                                "gapless_advance_waiting_for_renderer"
                            );
                        }
                    } else if status.ended_naturally
                        && (played_enough
                            || decisions::peak_reached_end(track_duration_ms, ps.peak_position_ms)
                            || decisions::ended_naturally_wall_ok(wall_elapsed, track_duration_ms))
                    {
                        fsm_actual = Some(fsm::StoppedOutcome::LocalEndedNaturally);
                        // Local outputs (WASAPI/ALSA/CoreAudio) signal
                        // ended_naturally when the audio stream reaches EOF.
                        // Skip the STOPPED_TICKS_THRESHOLD wait — we know
                        // the track is done, no need to accumulate 5s of
                        // stopped ticks.
                        info!(
                            zone_id,
                            wall_elapsed,
                            peak_pos = ps.peak_position_ms,
                            "local_output_ended_naturally_advancing"
                        );
                        track_ended = true;
                        motif_fin_de_piste = decisions::motif_fin::FIN_NATURELLE_LOCALE;
                    } else if dlna_dsd_reached_end {
                        fsm_actual = Some(fsm::StoppedOutcome::DsdDlnaReachedEnd);
                        // A DSD track on a DLNA renderer: gapless is intentionally
                        // not armed (prepare_gapless / #402) and DLNA never sets
                        // ended_naturally, so the only remaining end-of-track
                        // signal is STOPPED_TICKS_THRESHOLD polls = a fixed ~5s
                        // gap between DSD tracks (Benjithom, RS130). The peak
                        // reaching the end proves the track finished — advance
                        // now, ~4s sooner.
                        info!(
                            zone_id,
                            peak_pos = ps.peak_position_ms,
                            track_dur = track_duration_ms,
                            "dlna_dsd_reached_end_advancing"
                        );
                        ps.stopped_ticks = 0;
                        track_ended = true;
                        motif_fin_de_piste = decisions::motif_fin::DSD_DLNA_PIC_ATTEINT;
                    } else {
                        // Default for this block; overridden by the natural-end
                        // and failure sub-branches below.
                        fsm_actual = Some(fsm::StoppedOutcome::Waiting);
                        ps.stopped_ticks += 1;
                        if ps.stopped_ticks >= STOPPED_TICKS_THRESHOLD {
                            // When repeat mode is active (One or All) on DLNA,
                            // be more lenient about accepting track-end: if the
                            // renderer has reported Stopped and we've seen any
                            // meaningful playback (peak > 5s), treat it as a
                            // natural end so the poller re-triggers play instead
                            // of accumulating stopped_ticks until force_stop.
                            // (DEvir QA B-05: repeat mode doesn't work on DLNA)
                            let repeat_active =
                                matches!(zone_state.repeat, RepeatMode::One | RepeatMode::All);
                            let natural_end = decisions::natural_end(
                                played_enough,
                                repeat_active,
                                ps.peak_position_ms,
                                status.ended_naturally,
                                wall_elapsed,
                                track_duration_ms,
                                status.realtime,
                            );
                            // Not a warning for a non-realtime output: finishing
                            // a track in under 5s is its normal mode, not a
                            // renderer misreporting the end.
                            if status.ended_naturally
                                && status.realtime
                                && wall_elapsed < 5
                                && !played_enough
                            {
                                warn!(
                                    zone_id,
                                    wall_elapsed,
                                    peak_pos = ps.peak_position_ms,
                                    track_dur = track_duration_ms,
                                    "ended_naturally_rejected_too_early"
                                );
                            }
                            if natural_end {
                                // Only DLNA renderers auto-transition after
                                // SetNextAVTransportURI. For exclusive local
                                // outputs (ASIO / WASAPI exclusive) the near-end
                                // branch sets gapless_sent=true only to suppress
                                // re-arming — no SetNext is ever sent — so the
                                // "wait for transition" path below would hang
                                // forever and repeat/advance would never fire
                                // (DEvir: repeat fails on clean ASIO playback).
                                // Only wait when the output can actually
                                // transition internally; otherwise end normally.
                                let can_internal_gapless = {
                                    let outputs = self.outputs.lock().await;
                                    match outputs.get(&device_id) {
                                        Some(arc) => arc.lock().await.supports_internal_gapless(),
                                        None => false,
                                    }
                                };
                                fsm_in.can_internal_gapless = can_internal_gapless;
                                let awaiting_dlna_transition =
                                    ps.gapless_sent && can_internal_gapless;
                                if awaiting_dlna_transition {
                                    fsm_actual =
                                        Some(fsm::StoppedOutcome::NaturalEndGaplessWaiting);
                                    // Gapless was prepared via SetNextAVTransportURI.
                                    // Don't advance metadata yet — wait for the
                                    // renderer to confirm the transition by starting
                                    // to play (position reset detected in the Playing
                                    // handler).  If it stays Stopped after the
                                    // cooldown + stuck threshold, fall through to
                                    // play_from_queue which handles metadata itself.
                                    info!(zone_id, "gapless_natural_end_waiting_for_transition");
                                    ps.gapless_sent = false;
                                    ps.gapless_armed = None;
                                    ps.gapless_sent_at = None;
                                    ps.stopped_ticks = 0;
                                    ps.peak_position_ms = 0;
                                    ps.last_position_ms = 0;
                                    ps.track_started_at = None;
                                    ps.gapless_advance_pending = true;
                                    ps.gapless_stuck_ticks = 0;
                                    ps.gapless_cooldown = 4;
                                } else {
                                    // Avant d'accepter cette fin : le renderer
                                    // a-t-il vraiment reçu le morceau ? Sur un
                                    // réseau qui hoquette il cale, annonce
                                    // Stopped, et on tronquait la fin en
                                    // silence. Les octets servis tranchent.
                                    let sid = zone_state
                                        .now_playing
                                        .as_ref()
                                        .and_then(|np| np.stream_id.clone());
                                    let (sent, total) = match sid.as_deref() {
                                        Some(sid) => (
                                            self.orchestrator
                                                .streamer_bytes_sent(sid)
                                                .await
                                                .unwrap_or(0),
                                            self.orchestrator.streamer_total_bytes(sid).await,
                                        ),
                                        None => (0, None),
                                    };
                                    let seeked = zone_state.last_seek_at.is_some();
                                    if decisions::renderer_could_have_finished(sent, total, seeked)
                                    {
                                        fsm_actual = Some(fsm::StoppedOutcome::NaturalEndAdvance);
                                        ps.gapless_sent = false;
                                        ps.gapless_armed = None;
                                        track_ended = true;
                                        motif_fin_de_piste =
                                            decisions::motif_fin::FIN_NATURELLE_APRES_STOPPED;
                                    } else if ps.stall_declines < STALL_DECLINE_MAX_TICKS {
                                        // On laisse au renderer le temps de
                                        // reprendre : s'il repart, il repassera
                                        // Playing et cette branche disparaît.
                                        ps.stall_declines = ps.stall_declines.saturating_add(1);
                                        if ps.stall_declines == 1 {
                                            warn!(
                                                zone_id,
                                                peak_pos = ps.peak_position_ms,
                                                track_dur = track_duration_ms,
                                                bytes_sent = sent,
                                                bytes_total = total.unwrap_or(0),
                                                "renderer_stopped_on_incomplete_stream_waiting"
                                            );
                                        }
                                    } else {
                                        // La lecture a échoué : on arrête la
                                        // zone bruyamment plutôt que d'avancer
                                        // en faisant croire à une fin normale.
                                        warn!(
                                            zone_id,
                                            peak_pos = ps.peak_position_ms,
                                            track_dur = track_duration_ms,
                                            bytes_sent = sent,
                                            bytes_total = total.unwrap_or(0),
                                            "renderer_stalled_not_advancing_stopping_zone"
                                        );
                                        track_ended = false;
                                        force_stop = true;
                                    }
                                }
                            } else if ps.stopped_ticks >= STOPPED_FAILURE_THRESHOLD {
                                // Check if the stream is still being consumed
                                // (renderer actively fetching audio data). If so,
                                // don't kill — the renderer is playing but not
                                // reporting state (DMP-A10, LHC, Shanling, etc.).
                                // #2394 — le compteur a le droit de ne pas
                                // savoir. `stream_id` absent et session
                                // inconnue du gestionnaire de flux rendaient
                                // tous deux `0`, indiscernable d'« aucun octet
                                // servi » ; et c'est ce chiffre qui arme
                                // `force_stop`. Voir `fsm::consommation_flux`.
                                let stream_id = zone_state
                                    .now_playing
                                    .as_ref()
                                    .and_then(|np| np.stream_id.clone());
                                let octets_servis: Option<u64> = match stream_id.as_deref() {
                                    Some(sid) => self.orchestrator.streamer_bytes_sent(sid).await,
                                    None => None,
                                };
                                let consommation =
                                    fsm::consommation_flux(octets_servis, ps.last_bytes_sent);
                                // Un compteur inconnu n'écrase pas le dernier
                                // compte MESURÉ : sinon la reprise du
                                // `stream_id` ferait repartir la comparaison
                                // depuis un faux zéro.
                                if let Some(octets) = octets_servis {
                                    ps.last_bytes_sent = octets;
                                }
                                fsm_in.consommation = consommation;

                                if consommation == fsm::ConsommationFlux::Consomme {
                                    fsm_actual = Some(fsm::StoppedOutcome::FailureWaitingConsuming);
                                    if ps.stopped_ticks % 30 == 0 {
                                        debug!(
                                            zone_id,
                                            peak_pos = ps.peak_position_ms,
                                            wall_secs = wall_elapsed,
                                            bytes_sent = octets_servis.unwrap_or(0),
                                            consommation = consommation.etiquette(),
                                            "dlna_renderer_not_reporting_state_waiting"
                                        );
                                    }
                                } else if consommation == fsm::ConsommationFlux::Inconnue {
                                    // On ne mesure RIEN — ce n'est pas « rien
                                    // servi ». Couper ici couperait une zone
                                    // qui joue (une avance gapless pose
                                    // `stream_id: None`, cf. #2991). On attend,
                                    // et on le DIT : un état invisible se
                                    // reconfondrait avec zéro.
                                    fsm_actual = Some(fsm::StoppedOutcome::FailureWaitingUnknown);
                                    if ps.stopped_ticks % 30 == 0 {
                                        warn!(
                                            zone_id,
                                            peak_pos = ps.peak_position_ms,
                                            track_dur = track_duration_ms,
                                            wall_secs = wall_elapsed,
                                            consommation = consommation.etiquette(),
                                            has_stream_id = stream_id.is_some(),
                                            "octets_servis_inconnus_zone_non_coupee"
                                        );
                                    }
                                } else {
                                    let current_bytes = octets_servis.unwrap_or(0);
                                    fsm_actual = Some(fsm::StoppedOutcome::FailureStop);
                                    warn!(
                                        zone_id,
                                        peak_pos = ps.peak_position_ms,
                                        track_dur = track_duration_ms,
                                        wall_secs = wall_elapsed,
                                        bytes_sent = current_bytes,
                                        consommation = consommation.etiquette(),
                                        "playback_failure_stopping_zone"
                                    );
                                    track_ended = false;
                                    force_stop = true;
                                    // « Démarrage mort » (#2394) : la piste n'a
                                    // JAMAIS été tirée (0 octet servi) sur un
                                    // renderer DLNA — le profil du pipeline
                                    // Eversolo coincé, qui acquitte le Play et
                                    // ouvre ses connexions sans rien lire. Une
                                    // relance (précédée de Pause→Stop, son
                                    // libérateur connu) passe alors presque
                                    // toujours à la main. Distinct d'un
                                    // décrochage EN COURS de lecture
                                    // (bytes_sent > 0), qu'on ne rejoue pas.
                                    //
                                    // On n'arrive ici qu'avec un compteur
                                    // MESURÉ (`ASec`) : le `0` d'ignorance ne
                                    // déclenche plus la relance automatique
                                    // Pause→Stop→Play sur une zone qui joue.
                                    force_stop_demarrage_mort = decisions::demarrage_mort(
                                        all_zones
                                            .iter()
                                            .find(|z| z.id == Some(zone_id))
                                            .and_then(|z| z.output_type.as_deref())
                                            .unwrap_or(""),
                                        current_bytes,
                                    );
                                }
                            } else {
                                debug!(
                                    zone_id,
                                    peak_pos = ps.peak_position_ms,
                                    track_dur = track_duration_ms,
                                    wall_secs = wall_elapsed,
                                    stopped_ticks = ps.stopped_ticks,
                                    unknown_dur_min_peak = if track_duration_ms == 0 {
                                        MIN_PEAK_UNKNOWN_DURATION_MS
                                    } else {
                                        0
                                    },
                                    "stopped_early_waiting"
                                );
                            }
                        }
                    }
                    // v0.9 rc.2 — FSM shadow-compare (flag-gated, log only).
                    if *POLLER_FSM_SHADOW {
                        if let Some(actual) = fsm_actual {
                            let predicted = fsm::classify_stopped(&fsm_in);
                            if predicted != actual {
                                warn!(zone_id, ?predicted, ?actual, "poller_fsm_shadow_divergence");
                            }
                        }
                    }
                }
                TransportState::Playing | TransportState::Transitioning => {
                    ps.stopped_ticks = 0;
                    ps.gapless_cooldown = 0;
                    // v0.9 rc.2 FSM shadow: snapshot the Playing-arm inputs
                    // (pre-mutation). gapless_enabled is filled in the arm branch
                    // when it is actually read; default true matches the arm.
                    let fsm_has_next = Self::next_position(zone_state).is_some();
                    let output_type_str = all_zones
                        .iter()
                        .find(|z| z.id == Some(zone_id))
                        .and_then(|z| z.output_type.as_deref())
                        .unwrap_or("");
                    let is_dlna = output_type_str == "dlna";
                    // Un flux de radio n'a pas de fin : sa position ne peut rien
                    // depasser (#2493).
                    let source_est_radio = zone_state
                        .now_playing
                        .as_ref()
                        .is_some_and(|np| np.source == "radio");
                    let mut fsm_pin = fsm::PlayingInput {
                        gapless_advance_pending: ps.gapless_advance_pending,
                        has_next: fsm_has_next,
                        gapless_sent: ps.gapless_sent,
                        track_duration_ms,
                        reported_duration_ms: status.duration_ms,
                        played_enough,
                        position_ms: status.position_ms,
                        past_end_ticks: ps.past_end_ticks,
                        gapless_enabled: true,
                        is_dlna,
                        wall_elapsed_secs: wall_elapsed,
                    };
                    let mut fsm_pact = fsm::PlayingDecision {
                        confirm_gapless_advance: ps.gapless_advance_pending && fsm_has_next,
                        ..Default::default()
                    };
                    // Renderer started playing — gapless transition confirmed.
                    // NOW advance metadata (deferred from the Stopped handler
                    // to avoid showing the wrong track on renderers that don't
                    // actually auto-transition via SetNextAVTransportURI).
                    if ps.gapless_advance_pending {
                        ps.gapless_advance_pending = false;
                        ps.gapless_stuck_ticks = 0;
                        if let Some(next_pos) = Self::next_position(zone_state) {
                            info!(zone_id, next_pos, "gapless_confirmed_advancing_metadata");
                            if let Err(e) = self
                                .orchestrator
                                .advance_queue_metadata(zone_id, next_pos)
                                .await
                            {
                                warn!(zone_id, error = %e, "gapless_confirmed_advance_failed");
                            }
                            ps.gapless_cooldown = 4;
                            // Identity-keyed latch re-arms on the new track;
                            // clearing also covers gapless repeat-one (#1113).
                            ps.scrobbled_key = None;
                        }
                    }
                    if ps.track_started_at.is_none() {
                        ps.track_started_at = Some(Instant::now());
                    }

                    // Instrumentation (#1239): trace the gapless arming window for
                    // a realtime renderer that has a next track (BluOS reports
                    // honest secs/totlen). Recomputes should_arm_gapless read-only
                    // — it drives no decision — and logs ONLY when the armed state
                    // flips (window opens/closes) or once per track (the latch is
                    // reset to None on track change), so it never spams the ~1 s
                    // tick. Goal: later confirm why the arming window fails to open
                    // after a /Add. `reason` explains the current arm gate.
                    if status.realtime && fsm_has_next {
                        let armed = decisions::should_arm_gapless(
                            ps.gapless_sent,
                            status.duration_ms,
                            track_duration_ms,
                            status.position_ms,
                        );
                        if ps.gapless_arm_logged != Some(armed) {
                            let effective_duration_ms = decisions::sane_current_duration(
                                status.duration_ms,
                                track_duration_ms,
                            );
                            let reason = if ps.gapless_sent {
                                "already_armed"
                            } else if effective_duration_ms <= GAPLESS_WINDOW_MS {
                                "duration_le_window"
                            } else if status.position_ms
                                < effective_duration_ms.saturating_sub(GAPLESS_WINDOW_MS)
                            {
                                "before_arming_window"
                            } else {
                                "in_arming_window"
                            };
                            info!(
                                zone_id,
                                output = output_type_str,
                                armed,
                                reason,
                                reported_duration_ms = status.duration_ms,
                                queue_duration_ms = track_duration_ms,
                                effective_duration_ms,
                                position_ms = status.position_ms,
                                gapless_sent = ps.gapless_sent,
                                "gapless_arm_trace"
                            );
                            ps.gapless_arm_logged = Some(armed);
                        }
                    }

                    // Detect gapless transition: renderer reports a different
                    // duration than the current track AND the position confirms
                    // the track actually ended (near end or reset to start).
                    // Some DLNA renderers (DMP-A6/A8) report inaccurate durations
                    // from the start, so duration mismatch alone is insufficient.
                    let duration_changed = decisions::duration_changed(
                        ps.gapless_sent,
                        track_duration_ms,
                        status.duration_ms,
                    );
                    // Position must confirm we are actually at the end of the
                    // current track OR that the position has reset to the
                    // start of the next track.  The played_enough guard
                    // prevents false transitions when a renderer (DMP-A8)
                    // reports position < 5s immediately after SetNext.
                    let position_confirms_transition = decisions::position_confirms_transition(
                        played_enough,
                        status.position_ms,
                        track_duration_ms,
                    );
                    fsm_pact.transition_detected = duration_changed && position_confirms_transition;
                    if duration_changed && position_confirms_transition {
                        let arm_to_advance_ms = ps
                            .gapless_sent_at
                            .map(|t| t.elapsed().as_millis() as u64)
                            .unwrap_or(0);
                        info!(
                            zone_id,
                            renderer_dur = status.duration_ms,
                            track_dur = track_duration_ms,
                            peak_pos = ps.peak_position_ms,
                            arm_to_advance_ms,
                            "gapless_transition_detected"
                        );
                        ps.gapless_sent = false;
                        ps.gapless_sent_at = None;
                        // Voir `position_a_avancer` (#3026).
                        let arme_avant = ps.gapless_armed.take();
                        ps.peak_position_ms = 0;
                        ps.last_position_ms = 0;
                        ps.last_bytes_sent = 0;
                        ps.playing_stall_ticks = 0;
                        ps.stall_declines = 0;
                        ps.track_started_at = Some(Instant::now());
                        ps.stopped_ticks = 0;
                        ps.past_end_ticks = 0;
                        ps.gapless_advance_pending = false;
                        ps.gapless_stuck_ticks = 0;
                        // New track after a gapless advance (no generation bump):
                        // re-arm the once-per-track gapless_arm_trace line.
                        ps.gapless_arm_logged = None;
                        ps.gapless_dsd_skip_pos = None;
                        if let Some(next_pos) = self
                            .position_a_avancer(zone_id, zone_state, arme_avant)
                            .await
                        {
                            info!(zone_id, next_pos, "gapless_advance_metadata");
                            if let Err(e) = self
                                .orchestrator
                                .advance_queue_metadata(zone_id, next_pos)
                                .await
                            {
                                warn!(zone_id, error = %e, "gapless_advance_failed");
                            }
                            // Suppress handle_track_end for a few ticks — the
                            // renderer may briefly report Stopped during the
                            // gapless transition, which would otherwise send a
                            // redundant Stop+Play and cause an audible restart.
                            ps.gapless_cooldown = 4;
                            // Identity-keyed latch re-arms on the new track;
                            // clearing also covers gapless repeat-one (#1113).
                            ps.scrobbled_key = None;
                        } else {
                            self.handle_track_end(zone_id, zone_state).await;
                        }
                    } else if {
                        // Une preparation trop vieille ne vaut plus rien : son
                        // flux a expire cote serveur. On la jette pour que la
                        // condition ci-dessous rearme proprement, plutot que de
                        // laisser le renderer chercher une adresse morte.
                        if decisions::gapless_stage_expired(
                            ps.gapless_sent,
                            ps.gapless_sent_at.map(|t| t.elapsed().as_secs()),
                        ) {
                            info!(
                                zone_id,
                                age_secs = ps.gapless_sent_at.map(|t| t.elapsed().as_secs()),
                                "gapless_stage_expired_rearming"
                            );
                            ps.gapless_sent = false;
                            ps.gapless_sent_at = None;
                            ps.gapless_armed = None;
                        }
                        // « Lire ensuite » PENDANT la fenetre d'armement : la
                        // piste que le renderer a acceptee n'est plus celle que
                        // la file annonce comme suivante (#3026).
                        //
                        // Le geste explicite de l'utilisateur gagne. On desarme,
                        // et la condition ci-dessous re-arme DANS LE MEME TICK
                        // avec la piste inseree : un nouveau
                        // `SetNextAVTransportURI` part vers le renderer.
                        //
                        // Le blanc eventuel est celui que l'utilisateur a
                        // lui-meme provoque, et lui seul : tant que personne ne
                        // touche a la file, les deux identifiants sont egaux,
                        // rien n'est desarme, et l'enchainement sans blanc est
                        // intact. C'est tout l'ecart avec le faux correctif —
                        // desarmer des qu'on touche a la file — qui supprimerait
                        // le defaut en supprimant la fonctionnalite.
                        if ps.gapless_sent {
                            let suivant = Self::next_position(zone_state);
                            let ligne_au_suivant = suivant.and_then(|p| {
                                crate::db::play_queue_repo::PlayQueueRepo::with_backend(
                                    self.db.clone(),
                                )
                                .get_at(zone_id, p)
                                .ok()
                                .flatten()
                                .map(|e| e.id)
                            });
                            if decisions::gapless_arm_outdated(
                                ps.gapless_armed.map(|a| a.row_id),
                                ligne_au_suivant,
                            ) {
                                info!(
                                    zone_id,
                                    armed_row = ?ps.gapless_armed.map(|a| a.row_id),
                                    armed_pos = ?ps.gapless_armed.map(|a| a.position),
                                    next_pos = ?suivant,
                                    row_at_next = ?ligne_au_suivant,
                                    "gapless_rearm_queue_changed"
                                );
                                ps.gapless_sent = false;
                                ps.gapless_sent_at = None;
                                ps.gapless_armed = None;
                                // Une ligne de trace neuve pour le nouvel
                                // armement : sans cela le journal garderait
                                // « already_armed » et ne dirait pas ce qui
                                // vient d'etre renvoye.
                                ps.gapless_arm_logged = None;
                            }
                        }
                        decisions::should_arm_gapless(
                            ps.gapless_sent,
                            status.duration_ms,
                            track_duration_ms,
                            status.position_ms,
                        )
                    } {
                        // Only send SetNextAVTransportURI if gapless is enabled for this zone
                        let gapless_enabled = ZoneRepo::with_backend(self.db.clone())
                            .get(zone_id)
                            .ok()
                            .flatten()
                            .map(|z| z.gapless_enabled)
                            .unwrap_or(true);
                        fsm_pin.gapless_enabled = gapless_enabled;
                        fsm_pact.arm_gapless = gapless_enabled;
                        if gapless_enabled {
                            // Exclusive-mode local outputs (ASIO / WASAPI
                            // exclusive) can't chain internally. Detect that
                            // BEFORE prepare_gapless resolves the next URL —
                            // otherwise it downloads + transcodes the next track
                            // then discards it, and because prepare_gapless
                            // returns false, gapless_sent stays false and this
                            // branch re-fires every tick, re-downloading the same
                            // track in a tight loop (DEvir: repeat=one on ASIO
                            // Fireface, 55 wasted Qobuz downloads/min). Mark
                            // gapless_sent so we stop retrying; the natural-end
                            // fallback advances/repeats the queue.
                            let can_internal_gapless = {
                                let outputs = self.outputs.lock().await;
                                match outputs.get(&device_id) {
                                    Some(arc) => arc.lock().await.supports_internal_gapless(),
                                    None => true,
                                }
                            };
                            if !can_internal_gapless {
                                info!(zone_id, "gapless_skipped_exclusive_output");
                                ps.gapless_sent = true;
                                // Le drapeau est pose pour cesser de re-tenter,
                                // pas parce qu'une piste est partie : ne rien
                                // laisser croire le contraire (#3026).
                                ps.gapless_armed = None;
                            } else if decisions::dsd_skip_latched(
                                ps.gapless_dsd_skip_pos,
                                Self::next_position(zone_state),
                            ) {
                                // Suivant DSD sur DLNA, déjà constaté pour cette
                                // position : ne pas re-résoudre (donc re-créer puis
                                // détruire une session fichier) à chaque tick
                                // (spin 1 Hz, #2394). handle_track_end jouera la
                                // piste explicitement en fin de morceau.
                            } else {
                                match self.prepare_gapless(zone_id, zone_state, &device_id).await {
                                    GaplessPrep::Armed(arme) => {
                                        ps.gapless_sent_at = Some(Instant::now());
                                        ps.gapless_sent = true;
                                        // Ce que le renderer a ACCEPTE. Pose
                                        // apres `set_next_media` seulement :
                                        // un envoi refuse n'arme rien (#3026).
                                        ps.gapless_armed = arme;
                                    }
                                    GaplessPrep::DsdNextSkipped => {
                                        ps.gapless_dsd_skip_pos = Self::next_position(zone_state);
                                    }
                                    GaplessPrep::NotArmed => {}
                                }
                            }
                        } else {
                            debug!(zone_id, "gapless_disabled_for_zone");
                        }
                    }

                    // Position-based end-of-track detection: when the output
                    // still reports Playing but position has reached or exceeded
                    // the known track duration, the audio has effectively ended
                    // (e.g. local/cpal output draining its ring buffer).
                    // Wait POSITION_PAST_END_TICKS consecutive ticks to avoid
                    // cutting off the last fraction of a second of audio.
                    // Add a 3-second margin to avoid cutting off the end of
                    // tracks on DLNA renderers that report position slightly
                    // ahead of actual playback.
                    // Margin path (pure predicate, v0.9 extraction): position ran
                    // past duration + END_MARGIN_MS.
                    //
                    // Fix B (#1239): guard against an UNDER-scanned DB duration. A
                    // BluOS Node reports honest secs/totlen; when Tune's scanned
                    // (queue) duration is shorter than the real audio, the DB-only
                    // threshold fires early — the track is cut mid-play, which on
                    // BluOS triggers a /Clear + /Play and desyncs the now-playing
                    // metadata by one track. For a realtime renderer we widen the
                    // end-of-track threshold to max(queue, reported) BEFORE the
                    // margin, reusing `sane_current_duration` for reliability: it
                    // returns the DB duration when reported is 0 or egregiously off
                    // (RS130), so an absurd/absent report keeps the DB-only
                    // behavior and max() only ever widens for a trustworthy report
                    // that exceeds the DB scan. Non-realtime outputs are unchanged.
                    // This can only DELAY the position-based past-end, never block a
                    // track: the Stopped/natural-end and wall-clock paths remain the
                    // ultimate end-of-track guarantees for a renderer that stalls.
                    let effective_end_duration_ms = if status.realtime {
                        track_duration_ms.max(decisions::sane_current_duration(
                            status.duration_ms,
                            track_duration_ms,
                        ))
                    } else {
                        track_duration_ms
                    };
                    let past_end = decisions::past_end_reached(
                        effective_end_duration_ms,
                        played_enough,
                        status.position_ms,
                    );
                    // Exclusive local outputs (ASIO / WASAPI exclusive) cap the
                    // reported position at exactly the track duration and keep
                    // reporting Playing — their blocking HTTP read never sees a
                    // clean EOF at the loop point, so the +3s margin above never
                    // triggers. Treat "reached the very end (within 250ms) and
                    // held there for POSITION_PAST_END_TICKS ticks" as ended, so
                    // repeat/advance fires (DEvir: ASIO Fireface repeat never
                    // looped). Gated to exclusive outputs so DLNA — which can sit
                    // near the end legitimately — keeps the +3s margin above.
                    let reached_end_exclusive = !past_end
                        && !in_seek_grace
                        && track_duration_ms > decisions::END_MARGIN_MS
                        && played_enough
                        && status.position_ms + 250 >= track_duration_ms
                        && {
                            let outputs = self.outputs.lock().await;
                            match outputs.get(&device_id) {
                                Some(arc) => !arc.lock().await.supports_internal_gapless(),
                                None => false,
                            }
                        };
                    // Wall-clock fallback for a DLNA renderer (LMS UPnP bridge)
                    // that reports no duration of its own and never advances its
                    // position: treat the track as ended once the queue-known
                    // duration (plus margin) has elapsed on the wall clock.
                    // Guarded to `!in_seek_grace` on top of the helper's DLNA +
                    // reported-duration==0 gate (see wall_clock_past_end).
                    let wall_clock_past_end = !in_seek_grace
                        && decisions::wall_clock_past_end(
                            is_dlna,
                            status.duration_ms,
                            track_duration_ms,
                            wall_elapsed,
                        );
                    // Chromecast has no reliable end-of-track signal on a 1 Hz
                    // fresh-connect poll (FINISHED is a one-shot broadcast; a
                    // frozen near-end position dodges the position paths), and —
                    // unlike DLNA — no wall-clock fallback, so an album stalls
                    // after track 1 (Rhorn, forum #1226). Advance on Tune's own
                    // clock once the track's full duration has elapsed, still
                    // gated by played_enough (peak ≥ 80 %, honest on Cast) so a
                    // genuine mid-track buffering stall can't false-advance.
                    let chromecast_wall_clock_past_end = !in_seek_grace
                        && decisions::chromecast_wall_clock_past_end(
                            output_type_str,
                            played_enough,
                            track_duration_ms,
                            wall_elapsed,
                        );
                    // DMP-A6/A8 : PLAYING éternel, position gelée À la durée,
                    // poll sain — seul filet possible : l'horloge de Tune
                    // (même !in_seek_grace que ses deux voisins).
                    let dlna_frozen_end = !in_seek_grace
                        && decisions::dlna_frozen_at_end_wall_clock(
                            is_dlna,
                            played_enough,
                            track_duration_ms,
                            status.position_ms,
                            wall_elapsed,
                        );
                    if past_end
                        || reached_end_exclusive
                        || wall_clock_past_end
                        || chromecast_wall_clock_past_end
                        || dlna_frozen_end
                    {
                        ps.past_end_ticks += 1;
                        if ps.past_end_ticks >= POSITION_PAST_END_TICKS {
                            info!(
                                zone_id,
                                position_ms = status.position_ms,
                                track_dur = track_duration_ms,
                                wall_secs = wall_elapsed,
                                past_end_ticks = ps.past_end_ticks,
                                exclusive_end = reached_end_exclusive,
                                wall_clock_end = wall_clock_past_end,
                                cast_wall_clock_end = chromecast_wall_clock_past_end,
                                dlna_frozen_end,
                                "position_past_end_advancing"
                            );
                            track_ended = true;
                            fsm_pact.past_end_track_ended = true;
                            motif_fin_de_piste = decisions::motif_fin::POSITION_AU_DELA_DE_LA_FIN;
                        }
                    } else {
                        ps.past_end_ticks = 0;
                    }

                    // #2493 — Tades : « un morceau de 1'46 tourne depuis dix
                    // minutes » (Serenade/upmpdcli). Aucun des cinq detecteurs
                    // ci-dessus n'a agi, et la position montree a l'ecran est
                    // plafonnee a la duree : le testeur voit « 1:46 / 1:46, en
                    // lecture » indefiniment. Tune n'a alors plus le droit de
                    // presenter cet etat comme une lecture ordinaire.
                    //
                    // Ce bloc ne touche NI `track_ended` NI `force_stop`. La
                    // meme forme est produite par une lecture bloquee et par une
                    // duree fausse (etiquette erronee, piste reellement plus
                    // longue) : couper reviendrait a amputer une ecoute valide
                    // une fois sur deux. On DIT, on n'agit pas — voir
                    // `decisions::position_au_dela_de_la_duree`.
                    //
                    // Complementaire de #2116 juste en dessous, qui exclut
                    // explicitement la zone de fin (`near_known_end`) : celui-la
                    // couvre la position gelee AVANT la fin, celui-ci la
                    // position collee A la fin.
                    if decisions::position_au_dela_de_la_duree(
                        source_est_radio,
                        effective_end_duration_ms,
                        status.position_ms,
                    ) {
                        ps.depassement_duree_ticks = ps.depassement_duree_ticks.saturating_add(1);
                    } else {
                        ps.depassement_duree_ticks = 0;
                        ps.depassement_duree_signale = false;
                    }
                    if !track_ended
                        && ps.depassement_duree_ticks >= DEPASSEMENT_DUREE_TICKS
                        && !ps.depassement_duree_signale
                    {
                        ps.depassement_duree_signale = true;
                        // Les trois inconnues que le ticket reclamait faute de
                        // journal : la position est-elle figee, reboucle-t-elle,
                        // et des octets sont-ils encore servis ?
                        let octets_servis = match zone_state
                            .now_playing
                            .as_ref()
                            .and_then(|np| np.stream_id.as_deref())
                        {
                            Some(sid) => self.orchestrator.streamer_bytes_sent(sid).await,
                            None => None,
                        };
                        warn!(
                            zone_id,
                            output_type = output_type_str,
                            position_ms = status.position_ms,
                            peak_position_ms = ps.peak_position_ms,
                            duree_file_ms = track_duration_ms,
                            duree_rapportee_ms = status.duration_ms,
                            duree_effective_ms = effective_end_duration_ms,
                            wall_secs = wall_elapsed,
                            ticks = ps.depassement_duree_ticks,
                            ?octets_servis,
                            "lecture_annoncee_au_dela_de_la_duree"
                        );
                    }

                    // #2116: a renderer can acknowledge Play forever while
                    // producing no more sound. Only stop after two independent
                    // progress signals (renderer position and bytes served by
                    // Tune) have both remained frozen for the full observation
                    // window. The pure eligibility predicate deliberately
                    // excludes startup, seeks, unknown-position devices and a
                    // normal frozen-at-end transition.
                    let stream_id = zone_state
                        .now_playing
                        .as_ref()
                        .and_then(|np| np.stream_id.as_deref());
                    let playing_stall_eligible = !track_ended
                        && decisions::dlna_playing_stall_eligible(
                            output_type_str,
                            zone_state.state == PlayState::Playing,
                            status.state == TransportState::Playing,
                            status.realtime,
                            stream_id.is_some(),
                            in_seek_grace,
                            ps.track_loaded_at.elapsed().as_secs(),
                            ps.peak_position_ms,
                            status.position_ms,
                            track_duration_ms,
                        );
                    if playing_stall_eligible {
                        if let Some(current_bytes) = match stream_id {
                            Some(sid) => self.orchestrator.streamer_bytes_sent(sid).await,
                            None => None,
                        } {
                            let previous_bytes = ps.last_bytes_sent;
                            ps.playing_stall_ticks = decisions::next_dlna_playing_stall_ticks(
                                ps.playing_stall_ticks,
                                true,
                                prev_position_ms,
                                status.position_ms,
                                previous_bytes,
                                current_bytes,
                            );
                            ps.last_bytes_sent = current_bytes;
                            if ps.playing_stall_ticks >= PLAYING_STALL_THRESHOLD {
                                warn!(
                                    zone_id,
                                    position_ms = status.position_ms,
                                    peak_position_ms = ps.peak_position_ms,
                                    bytes_sent = current_bytes,
                                    stall_ticks = ps.playing_stall_ticks,
                                    wall_secs = wall_elapsed,
                                    "dlna_playing_without_progress_stopping_zone"
                                );
                                force_stop = true;
                            }
                        } else {
                            // No byte evidence means no conviction: a transient
                            // metrics lookup failure restarts the whole window.
                            ps.playing_stall_ticks = 0;
                        }
                    } else {
                        ps.playing_stall_ticks = 0;
                    }
                    // v0.9 rc.2 — FSM shadow-compare for the Playing arm.
                    if *POLLER_FSM_SHADOW {
                        let predicted = fsm::classify_playing(&fsm_pin);
                        if predicted != fsm_pact {
                            warn!(
                                zone_id,
                                ?predicted,
                                actual = ?fsm_pact,
                                "poller_fsm_shadow_divergence_playing"
                            );
                        }
                    }
                }
                TransportState::Paused => {
                    ps.stopped_ticks = 0;
                    ps.playing_stall_ticks = 0;
                }
            }

            // Sync metrics to shared map for external visibility
            self.shared_metrics.lock().await.insert(
                zone_id,
                ZonePollerMetrics {
                    total_polls: ps.total_polls,
                    total_errors: ps.total_errors,
                    consecutive_errors: ps.consecutive_errors,
                    last_latency_ms: ps.last_latency_ms,
                    max_latency_ms: ps.max_latency_ms,
                    lecture_au_dela_de_la_duree: ps.depassement_duree_signale,
                },
            );

            if force_stop {
                poll_states.remove(&zone_id);
                let device_id_ref = self.get_zone_device_id(zone_id);
                let relance = force_stop_demarrage_mort && {
                    let mut relances = self.relances_demarrage_mort.lock().await;
                    let autorisee = decisions::relance_demarrage_mort_autorisee(
                        relances.get(&zone_id).map(|t| t.elapsed().as_secs()),
                    );
                    if autorisee {
                        relances.insert(zone_id, Instant::now());
                    }
                    autorisee
                };
                if relance {
                    // Pause→Stop d'abord : le pipeline Eversolo coincé ACQUITTE
                    // les Stop sans les exécuter, seul Pause→Stop le libère
                    // (constaté par SOAP direct sur le DMP-A8). Best-effort :
                    // un appareil sain n'en souffre pas.
                    if let Err(error) = self
                        .orchestrator
                        .pause(zone_id, device_id_ref.as_deref())
                        .await
                    {
                        warn!(zone_id, error = %error, "demarrage_mort_pause_echouee");
                    }
                    self.orchestrator
                        .stop(zone_id, device_id_ref.as_deref())
                        .await;
                    let position = zone_state.queue_position;
                    match self.orchestrator.play_from_queue(zone_id, position).await {
                        Ok(_) => {
                            warn!(zone_id, position, "demarrage_mort_relance_automatique");
                        }
                        Err(e) => {
                            warn!(zone_id, position, error = %e, "demarrage_mort_relance_echouee");
                            self.orchestrator
                                .stop(zone_id, device_id_ref.as_deref())
                                .await;
                        }
                    }
                } else {
                    self.orchestrator
                        .stop(zone_id, device_id_ref.as_deref())
                        .await;
                }
            } else if track_ended {
                // #2488 — la moitié invisible du blanc entre deux pistes.
                //
                // `playback_timing` (orchestrator) démarre à `play_inner`,
                // donc APRÈS cette décision : tout ce que le sondeur a attendu
                // pour conclure « c'est fini » n'apparaît nulle part. Sur un
                // renderer réseau ce terme domine — de 0 ms (la sortie locale
                // réveille le sondeur) à plusieurs secondes selon la branche.
                // Une seule ligne, ici, au seul entonnoir d'avance côté
                // serveur, avec le nom de la branche et son plancher.
                let etat = poll_states.get(&zone_id);
                info!(
                    zone_id,
                    motif = motif_fin_de_piste,
                    plancher_ms = decisions::plancher_de_detection_ms(motif_fin_de_piste),
                    stopped_ticks = etat.map(|p| p.stopped_ticks).unwrap_or(0),
                    past_end_ticks = etat.map(|p| p.past_end_ticks).unwrap_or(0),
                    gapless_sent = etat.map(|p| p.gapless_sent).unwrap_or(false),
                    peak_pos = etat.map(|p| p.peak_position_ms).unwrap_or(0),
                    track_dur = track_duration_ms,
                    wall_secs = wall_elapsed,
                    output = all_zones
                        .iter()
                        .find(|z| z.id == Some(zone_id))
                        .and_then(|z| z.output_type.as_deref())
                        .unwrap_or(""),
                    "track_end_gap"
                );
                poll_states.remove(&zone_id);
                self.handle_track_end(zone_id, zone_state).await;
            }
        }
    }
}
