use super::*;

/// Issue du deuxième temps de `play_inner` : un flux résolu à envoyer, ou
/// une lecture déjà close (reprise par une plus récente, renvoi coalescé)
/// dont le résultat remonte tel quel.
enum ResoluOuFini {
    Resolu {
        resolved: ResolvedStream,
        resolve_ms: u128,
    },
    Fini(PlayResult),
}

/// Ce que la demande impose par-dessus le flux résolu : pochette et album
/// demandés, sinon ceux du flux. Relevés une fois, lus par trois temps.
struct Habillage {
    album: Option<String>,
    cover_path: Option<String>,
}

impl PlaybackOrchestrator {
    pub async fn play(&self, req: PlayRequest) -> Result<PlayResult, String> {
        // Free-tier zone cap, enforced at *activation* (a zone's first play).
        // This is the single choke point for every play entry point (direct
        // play, resume, streaming, transfer, alarms, AI) — gating here, rather
        // than at zone creation, both fixes the false "premium required" when
        // dormant auto-discovered zones filled the quota AND closes the "play a
        // few zones at a time to stay free" loophole.
        self.enforce_zone_cap(req.zone_id).await?;

        // Re-tap dedup (#1271): a controller (Flutter/web) can emit `play` TWICE
        // for the SAME track a few seconds apart. The first play already sent
        // SetAVTransportURI + Play to the renderer; the second, arriving AFTER the
        // first fully established playback (so the `superseded` play_seq guard in
        // play_inner sees no overlap and lets it through), would send a SECOND
        // SetAVTransportURI and restart a network renderer from byte 0 — the Revox
        // S100 "plays ~10s then jumps to 0" (forum, Philippe Vella). If this
        // request targets the track already playing on the zone and that track was
        // (re)started within RETAP_DEDUP_WINDOW, coalesce it: return the current
        // state WITHOUT re-resolving the source or re-sending to the renderer.
        //
        // This is complementary to the `superseded` (overlapping) and the
        // last_net_play (post-resolve) guards in play_inner, both untouched.
        // Safety of the exclusions:
        //   - a play of a DIFFERENT track never matches (is_same_track_retap);
        //   - a genuine replay of the same track after the window has an older
        //     start timestamp, so it is NOT coalesced (plays from 0);
        //   - an explicit seek is exempt (seek_ms > 0);
        //   - stall recovery stops the zone first, so state != Playing here, and
        //     auto-resume runs from a Stopped state — neither trips this guard.
        if req.seek_ms.unwrap_or(0) == 0 {
            let state = self.playback.get_state(req.zone_id).await;
            if state.state == PlayState::Playing {
                if let Some(np) = state.now_playing.as_ref() {
                    let recent = state
                        .last_play_started_at
                        .map(|t| t.elapsed() < RETAP_DEDUP_WINDOW)
                        .unwrap_or(false);
                    if recent && Self::is_same_track_retap(np, &req) {
                        info!(
                            zone_id = req.zone_id,
                            title = %np.title,
                            source = %np.source,
                            "orchestrator_play_retap_deduped_same_inflight_track"
                        );
                        return Ok(PlayResult {
                            stream_url: None,
                            output_sent: false,
                            source: np.source.clone(),
                            error: None,
                        });
                    }
                }
            }
        }

        // Public entry point: this is a *new* logical play, so it is recorded
        // in the listen history.
        self.play_inner(req, true).await
    }

    /// Free-tier gate: block *activating* a brand-new zone once the free active
    /// limit is reached. A zone that has already played (`last_track_id` set) is
    /// unaffected, so replays / auto-advance / resume never trip the gate; only
    /// the first play of an as-yet-unused zone counts. No license set (tests) or
    /// Premium tier → always allowed.
    pub(super) async fn enforce_zone_cap(&self, zone_id: i64) -> Result<(), String> {
        let Some(ref lic) = self.license else {
            return Ok(());
        };
        let zrepo = ZoneRepo::with_backend(self.db.clone());
        let already_active = zrepo
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.last_track_id)
            .is_some();
        if already_active || lic.is_premium().await {
            return Ok(());
        }
        let active = zrepo.count_active().unwrap_or(0);
        if active >= lic.free_zone_limit() {
            return Err(format!(
                "premium_required:Free tier is limited to {} active zones. Upgrade to Tune Premium for unlimited zones.",
                lic.free_zone_limit()
            ));
        }
        Ok(())
    }

    /// Gate playback on a zone whose stored output device may be gone, trying an
    /// auto-rebind before refusing.
    ///
    /// `Ok(None)` — nothing to do, carry on (this is the nominal path).
    /// `Ok(Some(id))` — the zone was re-bound to `id`, which the caller must use
    /// as the request's `output_device_id`.
    /// `Err(msg)` — playback must be refused; `msg` carries the
    /// `zone_output_unavailable:` sentinel the API maps to a 409.
    pub(super) async fn gate_or_rebind_offline_zone(
        &self,
        zone_id: i64,
        zone: &crate::db::zone_repo::Zone,
    ) -> Result<Option<String>, String> {
        if zone.online {
            return Ok(None);
        }
        let dev_id = zone.output_device_id.as_deref().unwrap_or("");
        // Skip zones with no device yet (being configured) and `local:` zones,
        // which are reputed always available. Then allow a grace window for SSDP
        // polling gaps: if the device is still in the live registry it is
        // reachable, whatever the DB says.
        if dev_id.is_empty()
            || dev_id.starts_with("local:")
            || self.outputs.lock().await.contains(dev_id)
        {
            return Ok(None);
        }

        // The stored device really is gone. Before rejecting, look for a live
        // output carrying the same name (#1287).
        if let Some((new_id, new_type)) = self.find_rebind_target(&zone.name).await {
            let repo = ZoneRepo::with_backend(self.db.clone());
            // Persist so the rebind is sticky — the point is that the user never
            // has to think about this again. `output_type` must follow the id:
            // leaving a zone typed `dlna` while pointing at a `local:` output
            // would take the wrong branch everywhere downstream.
            repo.update_output_device(zone_id, &new_id)?;
            repo.update_output_type(zone_id, &new_type)?;
            repo.update_online(zone_id, true)?;
            info!(
                zone_id,
                zone_name = %zone.name,
                stale_device_id = dev_id,
                new_device_id = %new_id,
                new_output_type = %new_type,
                "zone_rebound_to_live_output_with_same_name"
            );
            if let Some(ref bus) = self.event_bus {
                bus.emit(
                    "zone.rebound",
                    serde_json::json!({
                        "zone_id": zone_id,
                        "device_id": new_id,
                        "output_type": new_type,
                    }),
                );
            }
            return Ok(Some(new_id));
        }

        let msg = format!(
            "zone_output_unavailable:La sortie de cette zone n'est plus disponible. Choisissez une sortie dans les réglages de la zone « {} ».",
            zone.name
        );
        warn!(zone_id, zone_name = %zone.name, "play_rejected_zone_offline");
        if let Some(ref bus) = self.event_bus {
            bus.emit(
                "zone.playback_error",
                serde_json::json!({
                    "zone_id": zone_id,
                    "error": msg,
                }),
            );
        }
        Err(msg)
    }

    pub(super) async fn play_inner(
        &self,
        mut req: PlayRequest,
        record_history: bool,
    ) -> Result<PlayResult, String> {
        let play_start = std::time::Instant::now();
        // Zone navigateur : la sortie est l'onglet, pas un appareil. Relevé ici
        // parce que la ligne de zone est déjà lue juste en dessous, et qu'il
        // faudra le savoir bien plus bas, au moment d'annoncer l'écoute
        // (#1998). Une zone navigateur n'a JAMAIS d'`output_device_id` : si le
        // demandeur en fournit un, ce n'en est pas une.
        let zone_navigateur = self.resoudre_la_sortie_de_la_zone(&mut req).await?;

        // Clean up any gapless-prepared session for this zone before
        // creating a new stream.
        self.cleanup_gapless_session(req.zone_id).await;

        // Remember old session for cleanup AFTER output has been stopped
        let prev_state = self.playback.get_state(req.zone_id).await;
        let old_stream_id = prev_state
            .now_playing
            .as_ref()
            .and_then(|np| np.stream_id.clone());

        // Bump track_generation NOW so the poller resets its wall-clock
        // timer immediately. Without this, a long DASH transcode (20-30s)
        // can run into the 300s timeout from the previous track.
        let play_gen = self.playback.bump_generation(req.zone_id).await;

        // Signaler la recherche AVANT de la lancer : sur YouTube elle peut durer
        // une trentaine de secondes, et un écran muet pendant ce temps se lit
        // comme une panne (forum #1359). Le drapeau retombe dans `play()`, dès
        // qu'une URL jouable existe — y compris sur les chemins d'erreur, qui
        // passent tous par un `play()` ou un `stop()` ultérieur.
        self.playback.set_resolving(req.zone_id, true).await;

        let (resolved, resolve_ms) =
            match self.resoudre_la_demande(&req, play_start, play_gen).await? {
                ResoluOuFini::Resolu {
                    resolved,
                    resolve_ms,
                } => (resolved, resolve_ms),
                ResoluOuFini::Fini(resultat) => return Ok(resultat),
            };

        let habillage = Habillage {
            cover_path: req.cover_url.clone().or(resolved.cover_url.clone()),
            album: req.album_title.clone().or(resolved.album.clone()),
        };
        let np = self.composer_le_now_playing(&req, &resolved, &habillage);

        self.playback.play(req.zone_id, np).await;

        // Persist play state for auto-resume after server restart
        crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .save_play_state(req.zone_id, "playing")
            .ok();

        // L'annonce « en écoute » N'EST PLUS ici : elle attend de savoir si la
        // sortie a accepté le titre. Voir plus bas, après `output_sent`.
        //
        // Elle partait à cet endroit, c'est-à-dire AVANT toute tentative
        // d'envoi. Chez Bilou (#1998), quatre échecs de sortie BluOS d'affilée
        // ont produit quatre annonces à Last.fm pour un titre jamais entendu :
        // `output_sent=false`, zone arrêtée sur-le-champ, session de flux
        // fermée — et 233 ms plus tard « en écoute ».
        //
        // Le profil d'écoute est publié hors de chez l'utilisateur, sur un
        // service tiers, sans correction commode. Une écoute inventée y reste.

        // For local outputs, keep the old stream alive until after play_url()
        // calls stop() — otherwise the audio thread gets a read error when the
        // HTTP session is removed while it's still reading. For network outputs
        // (DLNA), close the old stream first to avoid stale bytes.
        let is_local = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("local:"));
        if !is_local {
            if let Some(ref old_sid) = old_stream_id {
                self.streamer.remove_session(old_sid).await;
            }
        }

        let (output_sent, output_error) = self
            .envoyer_a_la_sortie(
                &req, &resolved, &habillage, is_local, play_start, resolve_ms,
            )
            .await;

        // For local outputs, clean up the old stream now that play_url() has
        // called stop() and the old audio thread is no longer reading.
        if is_local {
            if let Some(ref old_sid) = old_stream_id {
                self.streamer.remove_session(old_sid).await;
            }
        }

        self.annoncer_apres_la_sortie(
            &req,
            &resolved,
            &habillage,
            output_sent,
            zone_navigateur,
            record_history,
        )
        .await;

        info!(
            zone_id = req.zone_id,
            title = %resolved.title,
            source = %resolved.source,
            output_sent,
            "orchestrator_play"
        );

        // Fail fast when the initial output send itself errored.
        //
        // play() already flipped the zone to Playing and bumped
        // track_generation (so the poller armed its 45s track-load grace).
        // That grace exists for a real renderer that accepts the stream but
        // takes a few seconds to start pulling bytes — NOT for a renderer that
        // outright rejected the stream (e.g. AirPlay ANNOUNCE → 403). Without
        // this short-circuit the zone sits "loading" for ~45s of grace + ~30
        // stopped ticks (~100s total) before the poller finally gives up with
        // bytes_sent=0 (Bilou, forum #1135).
        //
        // We only trip here on an explicit send error (output_error.is_some()
        // with a requested output device). The success path (play_media → Ok,
        // output_sent=true) is untouched, so a slow-but-valid renderer keeps
        // its full grace period. The superseded and "no output device" cases
        // returned earlier / set output_error=None and are likewise unaffected.
        if !output_sent && output_error.is_some() && req.output_device_id.is_some() {
            self.arreter_sur_refus_de_sortie(&req, &resolved, &output_error)
                .await;
            return Ok(PlayResult {
                stream_url: Some(resolved.url),
                output_sent: false,
                source: resolved.source,
                error: output_error,
            });
        }

        // Trigger prefetch of the next track in the background.
        // This runs concurrently with the current playback so the next
        // streaming track is already decoded in memory when needed.
        {
            let prefetch = self.prefetch.clone();
            let db = self.db.clone();
            let services = self.services.clone();
            let playback = self.playback.clone();
            let zone_id = req.zone_id;
            tokio::spawn(async move {
                prefetch
                    .prefetch_next(db, services, playback, zone_id)
                    .await;
            });
        }

        Ok(PlayResult {
            stream_url: Some(resolved.url),
            output_sent,
            source: resolved.source,
            error: output_error,
        })
    }

    /// Premier temps de `play_inner` : la sortie de la zone. Complète
    /// `output_device_id` depuis la ligne de zone quand la demande ne le
    /// porte pas, fait passer la garde des appareils disparus (#1287) et
    /// refuse la zone orpheline. Rend vrai pour une zone navigateur, dont la
    /// sortie est l'onglet (#1998).
    async fn resoudre_la_sortie_de_la_zone(&self, req: &mut PlayRequest) -> Result<bool, String> {
        let mut zone_navigateur = false;
        // Ensure output_device_id is populated: if the caller didn't provide
        // it (e.g. web client sends only zone_id + track_id), look it up from
        // the zone's DB record.  This is the primary gate for send_to_output —
        // without it, the stream is created but never sent to the output device.
        if req.output_device_id.is_none() {
            let zone_db = ZoneRepo::with_backend(self.db.clone())
                .get(req.zone_id)
                .ok()
                .flatten();

            // Refuse to start playback on a zone whose device is confirmed gone
            // — unless a live output of the same name can take over (#1287).
            let rebound = match zone_db {
                Some(ref zone) => self.gate_or_rebind_offline_zone(req.zone_id, zone).await?,
                None => None,
            };

            let looked_up =
                rebound.or_else(|| zone_db.as_ref().and_then(|z| z.output_device_id.clone()));
            if looked_up.is_some() {
                debug!(
                    zone_id = req.zone_id,
                    device_id = ?looked_up,
                    "output_device_id_resolved_from_zone_db"
                );
            } else {
                warn!(
                    zone_id = req.zone_id,
                    "output_device_id_missing_not_in_request_nor_zone_db"
                );
                // Orphan-zone guard (Yacine, 24/07): a zone row with NO
                // output_device_id can never produce sound — send_to_output is
                // skipped and play() "succeeds" with output_sent=false, so the
                // client shows the track while nothing plays. Fail loudly with
                // a sentinel the API maps to a clean 4xx instead of a silent
                // success. Browser zones are exempt: they legitimately have no
                // output device (the web client pulls stream_url itself). Zones
                // absent from the DB keep the old behaviour (in-memory tests /
                // transient states).
                if let Some(ref zone) = zone_db {
                    zone_navigateur = zone.output_type.as_deref() == Some("browser");
                    if !zone_navigateur {
                        let msg = format!(
                            "zone_no_output_device:Zone '{}' has no output device assigned — assign an output device to this zone or delete it and re-create it from a device.",
                            zone.name
                        );
                        warn!(zone_id = req.zone_id, zone_name = %zone.name, "play_rejected_zone_without_output_device");
                        if let Some(ref bus) = self.event_bus {
                            bus.emit(
                                "zone.playback_error",
                                serde_json::json!({
                                    "zone_id": req.zone_id,
                                    "error": msg,
                                }),
                            );
                        }
                        return Err(msg);
                    }
                }
            }
            req.output_device_id = looked_up;
        } else {
            // output_device_id was provided by the caller — run the same gate.
            // The client's id comes from the same stale zone row, so a rebind
            // must override it, otherwise we would keep aiming at the dead
            // device the caller just told us about.
            let zone_db = ZoneRepo::with_backend(self.db.clone())
                .get(req.zone_id)
                .ok()
                .flatten();
            let rebound = match zone_db {
                Some(ref zone) => self.gate_or_rebind_offline_zone(req.zone_id, zone).await?,
                None => None,
            };
            if let Some(new_id) = rebound {
                req.output_device_id = Some(new_id);
            }
        }
        Ok(zone_navigateur)
    }

    /// Deuxième temps : résoudre la demande en flux, puis écarter la lecture
    /// qu'une plus récente a doublée ou qu'un renvoi identique au même
    /// renderer rendrait redondant. Toute sortie sans flux abaisse le drapeau
    /// « recherche en cours », sauf la reprise par une lecture gagnante, qui
    /// a posé le sien.
    async fn resoudre_la_demande(
        &self,
        req: &PlayRequest,
        play_start: std::time::Instant,
        play_gen: u64,
    ) -> Result<ResoluOuFini, String> {
        // ⚠ TOUTE sortie de ce bloc doit abaisser le drapeau, sinon la zone reste
        // affichée « recherche en cours » indéfiniment. Trois chemins quittent
        // ici sans passer par `play()` : l'échec du fichier uploadé, la reprise
        // par une lecture plus récente, et l'échec de résolution.
        let resolved = if let Some(ref temp_path) = req.temp_file_path {
            match self.resolve_uploaded_file(temp_path, req).await {
                Ok(r) => r,
                Err(e) => {
                    self.playback.set_resolving(req.zone_id, false).await;
                    return Err(e);
                }
            }
        } else {
            match self.resolve_stream(req).await {
                Ok(r) => r,
                // A newer tap superseded this play before its transcode ran:
                // yield quietly (the winning play drives the output) instead of
                // surfacing an error for the redundant tap.
                Err(e) if e == SUPERSEDED_BEFORE_TRANSCODE => {
                    info!(
                        zone_id = req.zone_id,
                        "orchestrator_play_superseded_before_transcode"
                    );
                    // La lecture gagnante a pose son propre drapeau : ne pas
                    // l'abaisser ici, on effacerait SON etat de recherche.
                    return Ok(ResoluOuFini::Fini(PlayResult {
                        stream_url: None,
                        output_sent: false,
                        source: "local".into(),
                        error: Some("superseded by a newer play".into()),
                    }));
                }
                Err(e) => {
                    self.playback.set_resolving(req.zone_id, false).await;
                    return Err(e);
                }
            }
        };
        let resolve_ms = play_start.elapsed().as_millis();

        // If a newer play for this zone started while we were resolving, abort
        // before sending output. Resolving can take tens of seconds (a slow
        // network-volume ALAC→FLAC transcode for a DLNA renderer), during which
        // a user tapping play again — or the poller — stacks several plays; if
        // each one pushed its stream to the renderer, the overlapping audio came
        // out as noise (Yves, DMP-A10 over DLNA). Only the latest play should
        // reach the device.
        if self.playback.current_play_seq(req.zone_id).await != play_gen {
            info!(
                zone_id = req.zone_id,
                title = %resolved.title,
                resolve_ms,
                "orchestrator_play_superseded_skipping_output"
            );
            if let Some(ref sid) = resolved.stream_id {
                self.streamer.remove_session(sid).await;
            }
            return Ok(ResoluOuFini::Fini(PlayResult {
                stream_url: None,
                output_sent: false,
                source: resolved.source,
                error: Some("superseded by a newer play".into()),
            }));
        }

        // Coalesce a redundant re-play of the SAME track to a NETWORK renderer.
        // The generation guard above only aborts an OVERLAPPING play; it cannot
        // stop one that starts just AFTER the first already sent its URI. A slow
        // pre-transcode-to-file resolve (Tidal AAC / hi-res DASH, 4-10s) races a
        // second play/advance trigger for the same track, and BOTH reach the
        // renderer — the second SetAVTransportURI restarts it from 0 (Revox S100:
        // plays a few seconds, jumps to 0, then plays through — forum, Philippe
        // Vella). If we pushed this exact (source, source_id) to this zone within
        // DUPLICATE_NET_PLAY_WINDOW and this is not an explicit seek, skip the
        // redundant send. Push-URI outputs only (the ones that can even exhibit a
        // SetAVTransportURI restart-from-0) — checked against the registered
        // output's real `output_type()` via `is_push_uri_output_type`, rather
        // than guessed from the device_id string. Local/USB outputs prefill
        // their own ring buffer and don't restart-glitch, and neither do
        // pull-based out-of-tree outputs (oaat, diretta) that fetch audio
        // themselves instead of being pushed a URI — a `!starts_with("local:")`
        // guess would have wrongly caught both of those (they don't use a
        // "local:" device_id prefix either), coalescing a legitimate same-track
        // replay within the window. The record is cleared on stop(), so a
        // stop→replay of the same track still plays.
        let is_net_output = match req.output_device_id.as_deref() {
            Some(id) => is_push_uri_output_type(self.output_type_of(id).await.as_deref()),
            None => false,
        };
        let is_seek = req.seek_ms.unwrap_or(0) > 0;
        if is_net_output && !is_seek {
            let is_dup = {
                let mut last = self.last_net_play.lock().await;
                Self::record_or_detect_duplicate_net_play(
                    &mut last,
                    req.zone_id,
                    &resolved.source,
                    &req.source_id,
                    req.track_id,
                    std::time::Instant::now(),
                )
            };
            if is_dup {
                info!(
                    zone_id = req.zone_id,
                    title = %resolved.title,
                    source = %resolved.source,
                    "orchestrator_play_coalesced_duplicate_net_send"
                );
                if let Some(ref sid) = resolved.stream_id {
                    self.streamer.remove_session(sid).await;
                }
                return Ok(ResoluOuFini::Fini(PlayResult {
                    stream_url: None,
                    output_sent: false,
                    source: resolved.source,
                    error: None,
                }));
            }
        }
        Ok(ResoluOuFini::Resolu {
            resolved,
            resolve_ms,
        })
    }

    /// Troisième temps : le `NowPlaying` annoncé aux clients, la ligne de
    /// bibliothèque prenant le pas sur le flux pour le format et la
    /// résolution (`resolution_annoncee`).
    fn composer_le_now_playing(
        &self,
        req: &PlayRequest,
        resolved: &ResolvedStream,
        habillage: &Habillage,
    ) -> NowPlaying {
        let track_meta = req.track_id.and_then(|tid| {
            crate::db::track_repo::TrackRepo::with_backend(self.db.clone())
                .get(tid)
                .ok()
                .flatten()
        });
        let np = NowPlaying {
            track_id: req.track_id,
            title: resolved.title.clone(),
            artist_name: resolved.artist.clone(),
            album_title: habillage.album.clone(),
            cover_path: habillage.cover_path.clone(),
            duration_ms: resolved.duration_ms.unwrap_or(0),
            source: resolved.source.clone(),
            source_id: req.source_id.clone(),
            stream_id: resolved.stream_id.clone(),
            format: track_meta
                .as_ref()
                .and_then(|t| t.format.clone())
                // Qobuz only ever streams FLAC; surface the source format even
                // when the stream is transcoded to WAV for a local output, so
                // the format chip shows FLAC and not the output container
                // (Progman/Cyrille: Qobuz shown compressed / wrong format).
                .or_else(|| match resolved.source.as_str() {
                    "qobuz" => Some("flac".to_string()),
                    // Bandcamp : le dire ici — et non depuis le client — pour
                    // deux raisons : le repli sur le type MIME afficherait
                    // « MPEG » au lieu de « MP3 », et surtout l'avance de file
                    // re-résout la piste SANS qu'aucun client repasse un
                    // format. Sans cette ligne, la piste 1 s'affichait
                    // autrement que la piste 2 du même habillage.album.
                    //
                    // Le codec vient de l'URL, pas du nom du service : sans
                    // achat Bandcamp ne sert que du `mp3-128`, mais un fichier
                    // acheté descend en `flac`/`alac` par la même porte, et
                    // l'annoncer « MP3 » serait faux (#2074). Repli sur `mp3`
                    // quand l'URL ne nomme rien : c'est l'écoute libre.
                    "bandcamp" => Some(
                        req.source_id
                            .as_deref()
                            .and_then(bandcamp_encoding)
                            .and_then(|enc| bandcamp_quality(&enc))
                            .map(|q| q.codec.to_string())
                            .unwrap_or_else(|| "mp3".to_string()),
                    ),
                    _ => None,
                })
                // A media-server (UPnP/NAS) item has no local track row, so the
                // codec the client read from the DIDL res@protocolInfo is the only
                // authoritative source: audio/mp4 is ambiguous ALAC-vs-AAC, so
                // surface "alac" here instead of falling back to the "mp4" MIME
                // and mislabeling a lossless ALAC as lossy AAC (Yves, NAS).
                .or_else(|| req.media_format.clone())
                .or_else(|| {
                    let mime = &resolved.mime_type;
                    Some(
                        mime.strip_prefix("audio/")
                            .unwrap_or(mime)
                            .replace("x-", "")
                            .to_string(),
                    )
                }),
            // Prefer the SOURCE resolution (library metadata) over the resolved
            // OUTPUT format. Local playback forces a 32-bit WAV to the DAC, but
            // the "now playing" label must show the file's real depth (16/24) —
            // matching the gapless path (advance_queue_metadata), which avoids the
            // "first tracks show 32-bit then correct to 16" glitch. Streaming has
            // no local row so it falls back to the resolved stream format. DSD
            // display is handled separately in zones.rs.
            //
            // Le repli tenait sur `track_meta = None`, et c'etait trop large :
            // une piste LOCALE dont la ligne ignore la frequence ou la
            // profondeur y retombait aussi, et heritait alors du chiffre que
            // `resolve_local_track` venait de fabriquer (`unwrap_or(44100)` /
            // `unwrap_or(16)`). La meme ligne atteinte par avance gapless,
            // elle, n'annoncait rien (`NowPlaying::from_track`). La lecture
            // aleatoire rendait l'ecart visible : elle demarre sans cesse une
            // PREMIERE piste tiree au hasard (#2250, fil 1036). Le predicat
            // porte desormais sur la SOURCE, pas sur la presence d'une ligne.
            sample_rate: resolution_annoncee(
                track_meta
                    .as_ref()
                    .and_then(|t| t.sample_rate.map(|v| v as u32)),
                resolved.sample_rate,
                resolved.source == "local",
            ),
            bit_depth: resolution_annoncee(
                track_meta
                    .as_ref()
                    .and_then(|t| t.bit_depth.map(|v| v as u32)),
                resolved.bit_depth,
                resolved.source == "local",
            ),
            genre: track_meta.as_ref().and_then(|t| t.genre.clone()),
            year: track_meta.as_ref().and_then(|t| t.year),
            // L'album et l'artiste par IDENTIFIANT. Sans eux, le client devait
            // deviner l'album depuis son TITRE : cliquer sur « Entreat (2010) »
            // ouvrait la page de The Cure (FabienM, v0.9.102). `track_meta` les
            // a deja sous la main — c'est la ligne de la bibliotheque.
            //
            // `None` pour une radio ou un flux : ils n'ont pas d'entree en
            // bibliotheque, et inventer un identifiant serait pire que de n'en
            // donner aucun.
            album_id: track_meta.as_ref().and_then(|t| t.album_id),
            artist_id: track_meta.as_ref().and_then(|t| t.artist_id),
            // Le débit annoncé par la SOURCE, quand elle le nomme. C'est ce
            // qui manquait au 128 kbit/s de Bandcamp : annoncé sur l'écran de
            // découverte, il disparaissait dès que la piste partait en zone —
            // le chemin du signal ne montrait plus qu'un « MP3 » qu'aucun
            // auditeur ne peut distinguer d'un 320 (#2074). Une piste locale
            // n'en porte pas : sa résolution réelle est lue au scan.
            bitrate_kbps: resolved.bitrate_kbps,
        };
        np
    }

    /// Quatrième temps : l'envoi à la sortie. Rend `(output_sent,
    /// output_error)`, le couple dont dépendent l'annonce, l'historique et
    /// l'arrêt immédiat en cas de refus.
    async fn envoyer_a_la_sortie(
        &self,
        req: &PlayRequest,
        resolved: &ResolvedStream,
        habillage: &Habillage,
        is_local: bool,
        play_start: std::time::Instant,
        resolve_ms: u128,
    ) -> (bool, Option<String>) {
        if let Some(ref device_id) = req.output_device_id {
            let resolved_cover_url = self.resolve_cover_url(habillage.cover_path.as_deref());
            // One DB read for the local row: the output needs its path, and its
            // album numbering when the request did not carry any (a play by
            // track id, not from a queue row).
            let local_track = if resolved.source == "local" {
                req.track_id.and_then(|tid| {
                    TrackRepo::with_backend(self.db.clone())
                        .get(tid)
                        .ok()
                        .flatten()
                })
            } else {
                None
            };
            let local_file_path = local_track.as_ref().and_then(|t| t.file_path.clone());
            let media_source_id = req
                .source_id
                .clone()
                .or_else(|| req.track_id.map(|t| t.to_string()));
            // The library row stores 0 for "unknown", so filter it out rather
            // than telling the output this is track 0.
            let media_track_number = req.track_number.or_else(|| {
                local_track
                    .as_ref()
                    .map(|t| t.track_number)
                    .filter(|n| *n > 0)
                    .map(|n| n as u32)
            });
            let media_disc_number = req.disc_number.or_else(|| {
                local_track
                    .as_ref()
                    .map(|t| t.disc_number)
                    .filter(|n| *n > 0)
                    .map(|n| n as u32)
            });
            // Une session-canal (conversion à la volée) ne sait pas rejouer un
            // octet passé : la DIDL doit annoncer DLNA.ORG_OP=00, sans quoi le
            // renderer seeke par tranches et gèle à 0:00 (DMP-A8, DSD, 24/08).
            let byte_seekable = match resolved.stream_id.as_deref() {
                Some(sid) => {
                    let session = {
                        let sessions = self.streamer.sessions_state();
                        let guard = sessions.lock().await;
                        guard.get(sid).cloned()
                    };
                    match session {
                        Some(s) => !s.is_channel().await,
                        None => true,
                    }
                }
                None => true,
            };
            let media = crate::outputs::traits::PlayMedia {
                url: &resolved.url,
                mime_type: &resolved.mime_type,
                title: Some(&resolved.title),
                artist: resolved.artist.as_deref(),
                album: habillage.album.as_deref(),
                cover_url: resolved_cover_url.as_deref(),
                duration_ms: resolved.duration_ms.map(|d| d as u64),
                file_size: resolved.file_size,
                file_path: local_file_path.as_deref(),
                sample_rate: resolved.sample_rate,
                bit_depth: resolved.bit_depth,
                channels: resolved.channels,
                // Internet radio is an infinite live stream — mark it so the
                // DLNA DIDL advertises live/senderPaced semantics instead of a
                // seekable file (Yamaha R-N2000A stays silent otherwise).
                live_stream: resolved.source == "radio",
                byte_seekable,
                origin_url: resolved.origin_url.as_deref(),
                source: Some(&resolved.source),
                source_id: media_source_id.as_deref(),
                track_number: media_track_number,
                disc_number: media_disc_number,
            };
            let zone_audiophile = self.zone_audiophile(req.zone_id);

            // #1259: prebuffer ~2s before `Play` on network (DLNA) playback.
            //
            // A DLNA renderer starts its clock on the first byte it pulls. On
            // the initial play the decode pipeline is cold, so the first ~5s
            // trickle out and the renderer under-runs → micro-dropouts
            // (biblio/Qobuz/radio, macOS). Local/USB output has no such glitch:
            // LocalOutput prefills its ring buffer before it starts the DAC
            // (outputs/local.rs). We reproduce that server-side for DLNA here.
            //
            // Gating (kept tight for zero regression on the read path):
            //   - ONLY DLNA outputs (network push renderers with the cold-start
            //     clock). Local/USB and OAAT pull outputs already prefill.
            //   - ONLY channel sessions (WAV transcode / radio). Proxy passthrough
            //     and on-disk serve_file sessions are excluded INSIDE
            //     wait_prefill_ready (they have no channel to fill; blocking on
            //     them would hang).
            //   - Capped by a timeout so a slow or very short source never
            //     freezes the start of playback; `Play` is sent regardless.
            // The poller FSM is untouched: its 45s track-load grace already
            // tolerates the few extra seconds before Play.
            //
            // La RADIO a besoin de la même barrière quelle que soit la sortie
            // (#1628). Un flux live n'a pas de réserve par construction : on ne
            // peut envoyer que ce que la station a déjà émis. En partant sans
            // retard délibéré, la lecture colle au bord du direct et chaque
            // frontière de segment arrivant un peu tard passe sous le lead —
            // mesuré sur FIP → endpoint OAAT le 13/08 : des underruns
            // rythmiques toutes les ~8 s (la cadence des segments), de 0 à
            // 96 ms, indéfiniment. Les 2 s de tampon de l'endpoint n'y peuvent
            // rien : elles ne sont JAMAIS remplies. Accumuler ce retard avant
            // de lancer la lecture donne au flux la réserve qui lui manque ;
            // le coût est quelques secondes au zapping d'une station.
            // Temps réellement passé dans cette barrière. Il était jusqu'ici
            // noyé dans `output_ms` de `playback_timing`, ce qui rendait la
            // ligne trompeuse sur DLNA : jusqu'à 4 s d'attente DÉLIBÉRÉE s'y
            // lisaient comme « la sortie traîne » (#2488). Reste 0 quand la
            // barrière ne s'applique pas.
            let mut prebuffer_ms: u128 = 0;
            if let Some(ref sid) = resolved.stream_id {
                let is_dlna = self.output_type_of(device_id).await.as_deref() == Some("dlna");
                let is_radio = resolved.source == "radio";
                if is_dlna || is_radio {
                    let prebuffer_start = std::time::Instant::now();
                    let sr = resolved.sample_rate.unwrap_or(44100) as u64;
                    let ch = (resolved.channels.unwrap_or(2) as u64).max(1);
                    let bytes_per_sample = ((resolved.bit_depth.unwrap_or(16) as u64) / 8).max(1);
                    // La radio vise plus large : il faut couvrir une frontière
                    // de segment entière (~8 s observées) sans jamais toucher
                    // le fond, là où un fichier n'a qu'un démarrage à froid à
                    // absorber.
                    let secs = if is_radio { RADIO_PREBUFFER_SECS } else { 2 };
                    let target_bytes = sr * ch * bytes_per_sample * secs;
                    // Plafond : une station lente ou muette ne doit jamais
                    // geler le départ — `Play` part de toute façon.
                    let timeout = std::time::Duration::from_secs(if is_radio { 8 } else { 4 });
                    let reached = self
                        .streamer
                        .wait_prefill_ready(sid, target_bytes, timeout)
                        .await;
                    prebuffer_ms = prebuffer_start.elapsed().as_millis();
                    info!(
                        zone_id = req.zone_id,
                        stream_id = %sid,
                        target_bytes,
                        reached,
                        is_radio,
                        prebuffer_ms,
                        "initial_prebuffer_done"
                    );
                }
            }

            // A local output opens this URL in its own reader thread just
            // after `play_media`. If that reader never starts, the session is
            // alive but silent and used to leave only `stale_session_removed`
            // thirty minutes later (#2270). Arming here excludes sessions that
            // are merely prepared in advance for gapless playback.
            if is_local && let Some(ref sid) = resolved.stream_id {
                let _ = arm_local_stream_consumer_watch(
                    self.streamer.clone(),
                    sid.clone(),
                    req.zone_id,
                    device_id.clone(),
                    std::time::Duration::from_secs(15),
                )
                .await;
            }

            let result = self
                .send_to_output(
                    device_id,
                    &media,
                    req.seek_ms,
                    zone_audiophile,
                    req.zone_id,
                    req.track_id,
                )
                .await;
            let total_ms = play_start.elapsed().as_millis();
            // `output_ms` ne compte plus l'attente de pré-tampon : les trois
            // termes s'additionnent maintenant pour donner `total_ms`, et un
            // blanc s'impute à la bonne étape sans relire la source.
            info!(
                zone_id = req.zone_id,
                resolve_ms,
                prebuffer_ms,
                output_ms = total_ms
                    .saturating_sub(resolve_ms)
                    .saturating_sub(prebuffer_ms),
                total_ms,
                title = %resolved.title,
                "playback_timing"
            );

            // #2395 — AUCUNE commande de volume n'est envoyée ici, pour aucune
            // zone.
            //
            // Deux comportements sont morts à cet endroit, et pour deux
            // raisons distinctes :
            //
            // 1. Zone ordinaire : Tune poussait le volume stocké à CHAQUE
            //    lecture, écrasant le niveau réglé directement sur l'appareil
            //    — la valeur stockée dérive (rien ne resynchronise depuis le
            //    device), et un appareil laissé bas remontait aux 50 % stockés
            //    (Fabien, « Salon »). Retiré avant #2395 ; la zone garde le
            //    niveau où elle est physiquement.
            //
            // 2. Zone `fixed_volume` : le plein volume était RÉASSERTÉ 500 ms
            //    après chaque piste (`play_initial_volume_sent`). C'est la
            //    part du mode que l'utilisateur n'a jamais consentie : il a
            //    accepté UN saut à l'armement, pas une commande à 100 %
            //    renvoyée à chaque morceau. Sur un renderer qui porte son
            //    propre ampli — Denon RC12, Marco Polo, fil 1546 — chacune de
            //    ces commandes est une puissance acoustique réelle.
            //
            // Le mode bit-perfect reste entier : le 100 % est commandé UNE
            // fois, à l'armement, derrière la confirmation explicite
            // (`fixed_volume_confirmation_required`, routes/zones.rs), et il
            // est rendu au désarmement (`audio::fixed_volume`).
            //
            // Contrepartie assumée : une commande extérieure qui rebaisse le
            // renderer EN COURS de session casse le bit-perfect sans que rien
            // ne le dise. C'est le prix du choix — un saut annoncé et
            // réversible — et non un défaut à rattraper par une réassertion.

            result
        } else {
            warn!(
                zone_id = req.zone_id,
                "no_output_device_id_skipping_send_to_output"
            );
            (false, None)
        }
    }

    /// Cinquième temps : ce qui ne se dit qu'une fois la sortie entendue.
    /// Annonce « en écoute », historique local, ou annonce différée pour la
    /// zone navigateur. `annonce_apres_sortie_guard` relit ce texte.
    async fn annoncer_apres_la_sortie(
        &self,
        req: &PlayRequest,
        resolved: &ResolvedStream,
        habillage: &Habillage,
        output_sent: bool,
        zone_navigateur: bool,
        record_history: bool,
    ) {
        // Only record a listen-history row for genuinely new plays.  Seek and
        // radio auto-retry re-create the stream for a track that is already
        // playing (via play_without_history) and must not add duplicate rows.
        //
        // Skip live radio entirely: the title/artist supplied at play time is a
        // frozen snapshot (station name, or a stale song copied from a history
        // line when replaying) and never describes what the station is actually
        // streaming now, so recording it produces listen_history rows with no
        // relation to what was heard, plus a fresh bogus row on every replay
        // click (Bilou). Station plays are already tracked in the radio_stations
        // table (record_play), so nothing is lost.
        // Ce que la sortie a refusé n'a été entendu nulle part.
        //
        // `output_sent` est établi juste au-dessus, et l'arrêt immédiat de la
        // zone s'appuie déjà dessus quelques lignes plus bas : le signal
        // existait, dans la même fonction, et ces deux écritures ne le
        // consultaient pas.
        //
        // Le cas « aucune sortie configurée » rend lui aussi `false` (avec
        // `no_output_device_id_skipping_send_to_output`) : ne rien annoncer y
        // est également juste — le titre n'est parti vers aucun appareil.
        //
        // SAUF la zone navigateur, dont la sortie EST l'onglet : elle n'a pas
        // de périphérique, `output_sent` y vaut toujours faux, et une lecture
        // peut parfaitement avoir lieu. La garde ci-dessus lui a supprimé toute
        // annonce, y compris quand elle joue — c'est ce que ce ticket a rouvert
        // (#1998). Elle n'est pas remise dans le cas général : son annonce est
        // DIFFÉRÉE jusqu'à la preuve que l'onglet tire le flux.
        if !output_sent && !zone_navigateur {
            debug!(
                zone_id = req.zone_id,
                title = %resolved.title,
                "play_not_announced_output_not_sent"
            );
        }

        if !output_sent && zone_navigateur {
            let attente = AnnonceNavigateurDifferee {
                stream_id: resolved.stream_id.clone().unwrap_or_default(),
                title: resolved.title.clone(),
                artist: resolved.artist.clone(),
                album: habillage.album.clone(),
                source: resolved.source.clone(),
                source_id: req.source_id.clone(),
                track_id: req.track_id,
                duration_ms: resolved.duration_ms.unwrap_or(0),
                cover_path: habillage.cover_path.clone(),
                record_history,
            };
            if attente.stream_id.is_empty() {
                // Sans flux, rien à observer : on ne saurait jamais dire que
                // l'onglet a lu. Ne rien mettre en attente vaut mieux qu'une
                // entrée qu'aucune preuve ne pourra jamais libérer.
                debug!(
                    zone_id = req.zone_id,
                    title = %resolved.title,
                    "browser_now_playing_not_deferred_no_stream"
                );
            } else {
                debug!(
                    zone_id = req.zone_id,
                    title = %resolved.title,
                    stream_id = %attente.stream_id,
                    "browser_now_playing_deferred_until_stream_pulled"
                );
                if let Ok(mut en_attente) = self.annonces_navigateur.lock() {
                    en_attente.insert(req.zone_id, attente);
                }
            }
        }

        // Annonce « en écoute » multi-service, avec palier de licence.
        if output_sent {
            self.dispatch_now_playing(
                &resolved.title,
                resolved.artist.as_deref(),
                habillage.album.as_deref(),
            );
        }

        // `record_listen` alimente `listen_history`, la statistique locale. Il
        // souffrait du même défaut, et l'issue posait la question sans pouvoir
        // la trancher sur les seuls journaux : oui, l'historique local était
        // falsifié lui aussi.
        //
        // Le scrobble DÉFINITIF, lui, n'a jamais été concerné : il est
        // déclenché par le poller une fois le seuil des 50 % / 4 min franchi
        // (`dispatch_scrobble`, #1113), et une lecture qui n'a jamais démarré
        // ne l'atteint pas. C'est la seconde question ouverte du ticket.
        if output_sent && record_history && resolved.source != "radio" {
            // Owning profile = the zone's current session, set by the play
            // handler from X-Profile-Id and inherited by autoplay / gapless
            // advances (which reuse the zone without touching it). Resolved here
            // in async context so record_listen itself stays sync.
            // Meme lecture, meme raison que le profil : l'etat de zone porte
            // aussi CE QUE l'auditeur a demande (piste, album, playlist,
            // artiste, label), pose par le gestionnaire de `play` a partir du
            // corps de la requete. Une avance automatique herite du contexte
            // sans y toucher.
            let etat = self.playback.get_state(req.zone_id).await;
            let session_profile_id = etat.session_profile_id;
            // Le rang vient de la file de la zone, PAS du contexte : c'est la
            // position reellement atteinte, avance automatique comprise. En
            // aleatoire il reste vide — on re-tirera (#2441).
            let rang = rang_a_retenir(etat.shuffle, etat.queue_position);
            let context = (etat.session_context_type, etat.session_context_id);
            self.record_listen(
                &resolved.title,
                resolved.artist.as_deref(),
                habillage.album.as_deref(),
                &resolved.source,
                req.source_id.as_deref(),
                req.track_id.and_then(|tid| {
                    TrackRepo::with_backend(self.db.clone())
                        .get(tid)
                        .ok()
                        .flatten()
                        .and_then(|t| t.album_id)
                }),
                resolved.duration_ms.unwrap_or(0),
                req.zone_id,
                habillage.cover_path.as_deref(),
                session_profile_id,
                ContexteEcoute {
                    nature: context.0.as_deref(),
                    id: context.1.as_deref(),
                    rang,
                },
            );
        }
    }

    /// Sixième temps, sur refus explicite de la sortie : le dire aux clients
    /// (`zone.playback_error`, fatal), détruire le flux si la commande n'a
    /// certainement pas atteint le renderer, et arrêter la zone sur-le-champ
    /// (#1135).
    async fn arreter_sur_refus_de_sortie(
        &self,
        req: &PlayRequest,
        resolved: &ResolvedStream,
        output_error: &Option<String>,
    ) {
        warn!(
            zone_id = req.zone_id,
            device_id = req.output_device_id.as_deref().unwrap_or(""),
            error = output_error.as_deref().unwrap_or(""),
            "output_send_failed_stopping_zone_immediately"
        );
        // …et le DIRE, pas seulement l'écrire dans le journal du serveur.
        //
        // Ce bloc CONNAÎT la cause — le renderer vient de la donner — puis il
        // la range dans `PlayResult.error` et rend `Ok`. Or `Ok` est ce que
        // presque tous les appelants lisent comme un succès : le corps de la
        // réponse n'est relu que par les branches HTTP qui ATTENDENT `play()`
        // (`POST /zones/:id/play` et ses voisines, via
        // `build_zone_json_with_result`).
        //
        // Partout ailleurs la cause mourait ici : `next` et `previous`
        // répondent `{"status":"playing"}` depuis un `tokio::spawn` avant même
        // que l'envoi soit tenté (routes/playback.rs) ; `resume` et `seek`
        // rendent `Ok(())` ; l'avance automatique, l'autoplay et la relance
        // de démarrage mort du sondeur écrivent `Ok(_) =>` et poursuivent
        // comme si la piste avait démarré ; idem pour les alarmes, la reprise
        // au démarrage, le transfert de zone et l'émulation de renderer UPnP.
        // Vu de l'auditeur : la zone dit « en lecture », rien ne part, et rien
        // ne dit pourquoi.
        //
        // `zone.playback_error` existe déjà pour six autres échecs de lecture
        // (périphérique disparu, décodage, volume, radio…) et le serveur le
        // pousse verbatim à tous les clients (`routes/ws.rs`). Le REFUS D'UNE
        // SORTIE était le seul échec de lecture à ne pas s'en servir.
        //
        // `fatal` pour la même raison qu'en #2630 : la zone s'arrête quinze
        // lignes plus bas, et sans ce drapeau la fenêtre de grâce
        // d'après-lecture du client avalerait le message — l'utilisateur
        // n'aurait, une fois de plus, que le silence.
        //
        // Coût nul sur une lecture qui démarre : on n'entre dans ce bloc que
        // lorsqu'une sortie a EXPLICITEMENT refusé le flux.
        if let Some(ref bus) = self.event_bus {
            bus.emit(
                "zone.playback_error",
                serde_json::json!({
                    "zone_id": req.zone_id,
                    "error": output_error.as_deref().unwrap_or_default(),
                    "fatal": true,
                }),
            );
        }
        // La session de flux ne se détruit que si la commande n'a
        // certainement PAS été exécutée. Sur un timeout, elle a pu atteindre
        // un renderer lent : détruire le flux garantit alors qu'il tombe sur
        // un 404 en allant le chercher, et affiche « chanson non trouvée ».
        // On la laisse vivre — la GC des sessions périmées la ramassera si
        // personne ne la consomme.
        let may_have_landed = output_error.as_deref().is_some_and(command_may_have_landed);
        if let Some(ref sid) = resolved.stream_id {
            if may_have_landed {
                info!(
                    zone_id = req.zone_id,
                    stream_id = %sid,
                    "output_send_timed_out_keeping_stream_session"
                );
            } else {
                self.streamer.remove_session(sid).await;
            }
        }
        // Surface the failure now: flip the zone to Stopped so the poller's
        // load-grace path never runs and the UI reflects the error within a
        // poll tick instead of ~100s later.
        self.playback.stop(req.zone_id).await;
        crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .save_play_state(req.zone_id, "stopped")
            .ok();
    }

    /// Recreate a local (cpal) output on demand and play to it. Only the
    /// `local-audio` build has `outputs::local`; without that feature there is
    /// no local backend, so this is a no-op that reports the device as missing.
    #[cfg(feature = "local-audio")]
    pub(super) async fn recreate_local_and_play(
        &self,
        device_id: &str,
        media: &crate::outputs::traits::PlayMedia<'_>,
        start_position_ms: Option<u64>,
    ) -> (bool, Option<String>) {
        let device_name = device_id.strip_prefix("local:").unwrap_or(device_id);
        // Les réglages, pas des littéraux : voir `reglages_sortie_locale`
        // (#1770). `endpoint_id` et `origin_host` restent absents — les deux
        // ne viennent QUE d'une énumération de périphériques
        // (`AudioDevice::endpoint_id` / `.backend`), rien ne les persiste, et
        // ce chemin n'en a pas : il existe précisément parce que le
        // périphérique n'est PAS énumérable à cet instant. Les inventer serait
        // pire que de les laisser vides.
        let (exclusive_mode, audio_backend) = self.reglages_sortie_locale();
        info!(
            device_id,
            exclusive_mode,
            audio_backend = %audio_backend,
            "output_not_found_recreating_local_output"
        );
        let local_out = crate::outputs::local::LocalOutput::with_options(
            device_name.to_string(),
            exclusive_mode,
            &audio_backend,
        );
        if let Some(position_ms) = start_position_ms {
            local_out.set_pending_start_position_ms(position_ms);
            // Producer always pre-seeked — see the comment in send_to_output.
            local_out.set_producer_seeked(true);
        }
        {
            let mut outputs = self.outputs.lock().await;
            outputs.register(Box::new(local_out));
        }
        let arc = { self.outputs.lock().await.get(device_id) };
        if let Some(arc) = arc {
            let output = arc.lock().await;
            match output.play_media(media).await {
                Ok(()) => {
                    drop(output);
                    info!(device_id, "output_play_sent_after_recreate");
                    (true, None)
                }
                Err(e) => {
                    drop(output);
                    warn!(device_id, error = %e, "output_play_failed_after_recreate");
                    (false, Some(format!("Output device error: {e}")))
                }
            }
        } else {
            (false, Some(format!("Device not found: {device_id}")))
        }
    }

    #[cfg(not(feature = "local-audio"))]
    pub(super) async fn recreate_local_and_play(
        &self,
        device_id: &str,
        _media: &crate::outputs::traits::PlayMedia<'_>,
        _start_position_ms: Option<u64>,
    ) -> (bool, Option<String>) {
        (false, Some(format!("Device not found: {device_id}")))
    }

    /// `output_type()` of the registered output for `device_id` (e.g. "dlna",
    /// "local", "openhome"), or `None` when the device is not registered. Used
    /// to gate the initial DLNA prebuffer barrier (#1259) to DLNA outputs only,
    /// and the duplicate-net-play coalescing (#1129) to push-URI outputs only.
    pub(super) async fn output_type_of(&self, device_id: &str) -> Option<String> {
        let arc = { self.outputs.lock().await.get(device_id) };
        match arc {
            Some(arc) => Some(arc.lock().await.output_type().to_string()),
            None => None,
        }
    }

    pub(super) async fn send_to_output(
        &self,
        device_id: &str,
        media: &crate::outputs::traits::PlayMedia<'_>,
        start_position_ms: Option<u64>,
        zone_audiophile: bool,
        zone_id: i64,
        track_id: Option<i64>,
    ) -> (bool, Option<String>) {
        let lock_start = std::time::Instant::now();
        let (output_arc, used_device_id) = {
            let outputs = self.outputs.lock().await;
            let elapsed = lock_start.elapsed();
            if elapsed.as_millis() > 200 {
                warn!(
                    device_id,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "send_to_output_lock_contention"
                );
            }
            // Bug 2 fix: never fall back to another zone/device.
            // If the exact requested device is not found, return an error so
            // audio never comes out of an unexpected speaker.
            match outputs.get(device_id) {
                Some(arc) => (Some(arc), device_id.to_string()),
                None => (None, device_id.to_string()),
            }
        };
        if let Some(output_arc) = output_arc {
            // For OAAT outputs, arm the start position before play: the
            // native DSD direct path reads the local file itself and must
            // seek to this offset (the seek-positioned HTTP transcode URL
            // is bypassed on that path).
            //
            // Gated on `oaat`: `crate::outputs::oaat` only exists with that
            // feature (outputs/mod.rs), so an ungated reference broke the
            // postgres-without-oaat build (test-postgres "Engine module tests"),
            // mirroring the `local-audio` gate on the analogous block below.
            #[cfg(feature = "oaat")]
            if let Some(position_ms) = start_position_ms {
                if device_id.starts_with("oaat:") {
                    let output = output_arc.lock().await;
                    if let Some(oaat_output) = output
                        .as_any()
                        .downcast_ref::<crate::outputs::oaat::OaatOutput>()
                    {
                        oaat_output.set_pending_start_position_ms(position_ms);
                    }
                    drop(output);
                }
            }
            // For local outputs, set the pending start position before play
            #[cfg(feature = "local-audio")]
            if let Some(position_ms) = start_position_ms {
                if device_id.starts_with("local:") {
                    let output = output_arc.lock().await;
                    if let Some(local_output) = output
                        .as_any()
                        .downcast_ref::<crate::outputs::local::LocalOutput>()
                    {
                        local_output.set_pending_start_position_ms(position_ms);
                        // The WAV a local output receives is ALWAYS pre-seeked:
                        // since b3a4a79f the streaming (Qobuz/Tidal) transcode
                        // arm feeds seek_s to decode_to_pcm_streaming_seeked,
                        // exactly like the local-file arm. Deriving this flag
                        // from media.file_path (streaming → false → consumer
                        // byte-skip) made the output skip the seek offset a
                        // SECOND time on an already-seeked stream: a seek at
                        // 4:30 discarded the entire remainder of the track —
                        // silence, then a ~3s restart loop as the poller kept
                        // recovering the "ended" track (Vincent, #1518).
                        local_output.set_producer_seeked(true);
                    }
                    drop(output);
                }
            }
            // PURE (audiophile) mode: bypass the room-correction convolver on
            // this local output for the zone about to play, so the signal path
            // stays bit-perfect. Applied every play (not just on seek) so a zone
            // toggled in/out of PURE takes effect on the next track; other zones
            // on the same output keep their convolution.
            #[cfg(feature = "local-audio")]
            if device_id.starts_with("local:") {
                let output = output_arc.lock().await;
                if let Some(local_output) = output
                    .as_any()
                    .downcast_ref::<crate::outputs::local::LocalOutput>()
                {
                    local_output.set_pure_bypass(zone_audiophile);
                    // ReplayGain, applied by the output itself for a local DAC.
                    // A PURE zone is left strictly alone: applying a gain would
                    // multiply every sample and the path would no longer be
                    // bit-perfect, which is the one thing PURE promises.
                    let rg = match (zone_audiophile, track_id) {
                        (false, Some(tid)) => {
                            crate::audio::replaygain::playback_factor(&self.db, tid)
                        }
                        _ => 1.0,
                    };
                    local_output.set_replaygain_factor(rg);
                    // Headphone crossfeed (local DAC only). Returns None when the
                    // zone has crossfeed disabled OR is in PURE mode, so a PURE
                    // zone stays bit-perfect. Rebuilt per-play at the resolved
                    // stream sample rate so the delay line matches the DAC clock.
                    let cf_sr = media.sample_rate.unwrap_or(44100);
                    local_output.set_crossfeed(self.load_crossfeed_processor(zone_id, cf_sr));
                    // Zone equalizer, applied by the output itself for a local
                    // DAC — exactly like the crossfeed above, and for the same
                    // reason: a local zone never goes through
                    // `transcode_source_to_file`, the ONLY place the
                    // `EqProcessor` was ever run. `use_file_transcode_for`
                    // requires a network output, so a local zone always took the
                    // streaming pipe, which touches no DSP. Result: the EQ was
                    // applied nowhere at all on a local output — profil
                    // enregistré, courbe affichée, zéro effet au DAC (Jean
                    // Marie, forum #1416 : ses journaux ne montrent QUE des
                    // sorties `local:`, sur les deux zones qu'il a essayées).
                    //
                    // `load_eq_processor` returns None in PURE mode, so the
                    // bit-perfect promise is kept without a second guard.
                    // Built at the resolved stream's rate/channel count so the
                    // biquads match the DAC clock, mirroring the crossfeed.
                    let eq_ch = media.channels.unwrap_or(2).clamp(1, 8) as u16;
                    local_output.set_eq(self.load_eq_processor(zone_id, cf_sr, eq_ch));
                    // Repli mono (#2362) — sortie LOCALE uniquement, comme le
                    // crossfeed juste au-dessus. `zone_mono_downmix` rend
                    // `false` en mode PURE, donc la promesse bit-perfect tient
                    // sans garde supplémentaire, exactement comme pour l'EQ.
                    local_output.set_mono_downmix(self.zone_mono_downmix(zone_id));
                    // Rampe anti-« ploc » à la pause / reprise / arrêt (#1590),
                    // sortie LOCALE uniquement — voir `zone_soft_mute_ms` pour
                    // les sorties qui restent nues et pourquoi.
                    local_output.set_soft_mute_ms(self.zone_soft_mute_ms(zone_id));
                }
                drop(output);
            }
            let output = output_arc.lock().await;
            match output.play_media(media).await {
                Ok(()) => {
                    drop(output);
                    info!(device_id = %used_device_id, "output_play_sent");
                    (true, None)
                }
                Err(e) => {
                    drop(output);
                    warn!(device_id = %used_device_id, error = %e, "output_play_failed");
                    // Le message rendu à l'utilisateur nomme le RÉGLAGE quand
                    // le serveur savait, avant d'envoyer, que la zone forçait
                    // du DSD brut vers un lecteur qui annonce ne pas le lire
                    // (#2396). Relecture sur le chemin d'erreur uniquement :
                    // aucun coût sur une lecture qui démarre.
                    let (dsd_mode, annonce) = self.contexte_dsd(zone_id, &used_device_id).await;
                    (
                        false,
                        Some(Self::message_echec_sortie(
                            &e,
                            &dsd_mode,
                            annonce,
                            media.mime_type,
                        )),
                    )
                }
            }
        } else if device_id.starts_with("local:") {
            self.recreate_local_and_play(device_id, media, start_position_ms)
                .await
        } else {
            warn!(device_id, "output_not_found");
            (
                false,
                Some(format!(
                    "Device not yet discovered: {device_id}. Please retry in a few seconds."
                )),
            )
        }
    }

    /// True when the zone is in PURE (audiophile) mode: bypass ALL per-zone
    /// signal processing for a bit-perfect path — the equalizer, its
    /// room-correction gains (in `load_eq_processor`) and the room-correction
    /// convolver (in the local output). Bertrand: "PURE doit désactiver toutes
    /// les modifs".
    pub(super) fn zone_audiophile(&self, zone_id: i64) -> bool {
        crate::audio::audiophile::zone_enabled(&self.db, zone_id)
    }

    /// Rejouer la piste en cours d'une zone **à sa position courante**, en
    /// recréant le flux.
    ///
    /// Extrait tel quel de la branche `seek` (#1710, lot 1). Aucun changement
    /// de comportement : le seek passe désormais par ici, et les mêmes
    /// événements sont journalisés par l'appelant.
    ///
    /// Pourquoi cette manœuvre existe : une sortie locale ou OAAT consomme un
    /// flux HTTP **séquentiel** (mpsc / chunké). On ne peut pas y chercher une
    /// position par `Range` — sur un DSD servi en WAV chunké, une requête
    /// `Range` brute atterrit au milieu d'un bloc DSD et joue du BRUIT BLANC
    /// (Xavier). Il faut arrêter et rejouer depuis l'offset.
    ///
    /// Elle est extraite parce qu'elle ne sert pas qu'au seek : c'est la seule
    /// voie connue pour faire prendre effet un changement d'égaliseur sur un
    /// chemin transcodé ou navigateur, où le fichier est déjà écrit et déjà
    /// téléchargé (#1710). `raison` sert à distinguer les appelants dans les
    /// journaux — un redémarrage inattendu doit pouvoir être imputé.
    ///
    /// ⚠️ Coûteux et AUDIBLE : environ une seconde de silence. Tout appelant
    /// déclenché par un geste répétable — un curseur qu'on fait glisser — doit
    /// l'anti-rebondir lui-même, sinon il produit une coupure par cran.
    ///
    /// Rend `Err` si la zone n'a rien en lecture ou si la relecture échoue ;
    /// la position est repositionnée dans les deux cas, pour que le poller ne
    /// prenne pas un état `Stopped` pour une panne de lecture.
    pub async fn replay_zone_at_position(
        &self,
        zone_id: i64,
        position_ms: u64,
        raison: &str,
    ) -> Result<(), String> {
        let state = self.playback.get_state(zone_id).await;
        let Some(np) = state.now_playing.clone() else {
            return Err("rien en lecture sur cette zone".into());
        };
        self.playback.seek(zone_id, position_ms as i64).await;

        let zone_row = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten();
        let output_type = zone_row.as_ref().and_then(|z| z.output_type.clone());
        let output_device_id = zone_row.and_then(|z| z.output_device_id);

        // Arrêter la sortie AVANT que play() n'en crée une autre : sans ça,
        // l'ancien fil ASIO/WASAPI peut encore tenir la connexion HTTP quand la
        // nouvelle session démarre — d'où un « request or response body error »
        // intermittent. Les 300 ms lui laissent relâcher le périphérique.
        if let Some(ref did) = output_device_id {
            if did.starts_with("local:") || did.starts_with("oaat:") {
                let arc = { self.outputs.lock().await.get(did.as_str()) };
                if let Some(output) = arc {
                    let _ = output.lock().await.stop().await;
                }
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        }

        let req = PlayRequest {
            zone_id,
            output_device_id: output_device_id.clone(),
            track_id: np.track_id,
            source: Some(np.source.clone()),
            source_id: np.source_id.clone(),
            title: Some(np.title.clone()),
            artist_name: np.artist_name.clone(),
            album_title: np.album_title.clone(),
            cover_url: np.cover_path.clone(),
            duration_ms: Some(np.duration_ms),
            seek_ms: Some(position_ms),
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: None,
            disc_number: None,
        };

        let resultat = self.play_without_history(req).await;
        // Repositionner DANS LES DEUX CAS : à la réussite pour que la grâce du
        // poller parte d'après la relecture, à l'échec pour qu'il ne prenne pas
        // l'état `Stopped` pour une panne.
        self.playback.seek(zone_id, position_ms as i64).await;
        match resultat {
            Ok(_) => {
                // Le flux est neuf. Sur une sortie réseau servie par une
                // session fichier/proxy, il repart de l'OCTET 0 : sans ce
                // Seek, le renderer joue le morceau depuis le début (#2893).
                self.seek_output_after_replay(
                    zone_id,
                    output_device_id.as_deref(),
                    output_type.as_deref(),
                    position_ms,
                )
                .await;
                info!(zone_id, position_ms, raison, "zone_replayed_at_position");
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// Envoyer à la sortie le `Seek` qui manque après une recréation de flux
    /// (#2893). Voir [`replay_needs_output_seek`] pour la règle de décision et
    /// la raison pour laquelle elle n'est ni « toujours », ni « jamais ».
    ///
    /// **Au mieux, jamais fatal** : la relecture, elle, a réussi — le
    /// changement de traitement est bien passé dans le signal. Un renderer qui
    /// refuse le `Seek` laisse un morceau qui repart du début, ce qui est
    /// exactement l'ancien comportement ; le faire remonter en `Err`
    /// transformerait cette dégradation en `eq_replay_failed`, et empêcherait
    /// au passage d'armer le plancher anti-rafale.
    pub(super) async fn seek_output_after_replay(
        &self,
        zone_id: i64,
        device_id: Option<&str>,
        output_type: Option<&str>,
        position_ms: u64,
    ) {
        let Some(did) = device_id else {
            return;
        };
        // La session est celle que `play_without_history` vient d'installer :
        // `now_playing.stream_id` porte le NOUVEAU flux, pas l'ancien.
        let stream_id = self
            .playback
            .get_state(zone_id)
            .await
            .now_playing
            .and_then(|np| np.stream_id);
        let session_is_range_seekable = match stream_id {
            Some(ref sid) => self.streamer.is_seekable_session(sid).await,
            None => false,
        };
        if !replay_needs_output_seek(
            is_network_output_type(output_type),
            session_is_range_seekable,
            position_ms,
        ) {
            return;
        }

        tokio::time::sleep(std::time::Duration::from_millis(
            REPLAY_OUTPUT_SEEK_SETTLE_MS,
        ))
        .await;
        let Some(output) = ({ self.outputs.lock().await.get(did) }) else {
            warn!(
                zone_id,
                device_id = did,
                "replay_output_seek_output_disparue"
            );
            return;
        };
        match output.lock().await.checked_seek(position_ms).await {
            Ok(()) => info!(zone_id, position_ms, "replay_output_seek_sent"),
            Err(e) => warn!(zone_id, position_ms, error = %e, "replay_output_seek_failed"),
        }
        // Repositionner APRÈS le Seek : la grâce du poller doit partir de la
        // commande, pas de la recréation qui la précède de 500 ms.
        self.playback.seek(zone_id, position_ms as i64).await;
    }

    /// True when the zone has an uploaded room-correction IR and is not in PURE
    /// mode. Cheap settings read (like `zone_has_active_eq`) used to force the
    /// transcode path so the FIR reaches network renderers, not just local.
    pub(super) fn zone_has_active_ir(&self, zone_id: i64) -> bool {
        if self.zone_audiophile(zone_id) {
            return false;
        }
        crate::db::settings_repo::SettingsRepo::with_backend(self.db.clone())
            .get(&format!("ir_path_{zone_id}"))
            .ok()
            .flatten()
            .map(|p| !p.is_empty() && std::path::Path::new(&p).exists())
            .unwrap_or(false)
    }

    /// Durée de la rampe anti-« ploc » à la pause, à la reprise et à l'arrêt
    /// pour cette zone, en millisecondes (#1590).
    ///
    /// Demandé par Levente : « the app just stops immediately when pushing
    /// Pause/Stop ». Ce n'est pas un fondu artistique — c'est le retrait du clic
    /// que produit une marche de pleine amplitude à zéro en un échantillon.
    ///
    /// **Ne concerne que la sortie LOCALE.** Sur un renderer réseau — DLNA,
    /// AirPlay, BluOS, Chromecast, OpenHome, Squeezebox, HQPlayer — c'est
    /// l'appareil qui décode et qui rend : nous n'avons pas la main sur ses
    /// échantillons. Le seul levier serait son propre volume, avec sa latence
    /// et sa granularité à lui ; le résultat serait très inégal d'un appareil à
    /// l'autre, parfois pire que la coupure. Ces sorties restent donc
    /// délibérément nues, et la sortie locale exclusive aussi (voir plus bas).
    ///
    /// `0` en mode PURE : la rampe est une modification du signal, elle tombe
    /// avec l'égaliseur et le repli mono. Ce n'est pas la seule garde — la
    /// sortie redésarme d'elle-même sur un flux DoP et en mode exclusif, que
    /// l'orchestrateur ait dit ce qu'il voulait ou non
    /// ([`crate::audio::soft_mute::armed_ms`]).
    ///
    /// Réglable par zone via la clé `zone_<id>_soft_mute_ms` du magasin de
    /// réglages — pas de migration, comme `zone_<id>_mono_downmix`. La valeur
    /// est bornée par [`SOFT_MUTE_MAX_MS`](crate::audio::soft_mute::SOFT_MUTE_MAX_MS) :
    /// au-delà, l'appui sur Pause cesse d'être ressenti comme une pause.
    pub fn zone_soft_mute_ms(&self, zone_id: i64) -> u32 {
        Self::zone_soft_mute_ms_with(&self.db, zone_id)
    }

    /// Même règle, lisible sans orchestrateur — jumelle de
    /// [`Self::zone_mono_downmix_with`].
    pub fn zone_soft_mute_ms_with(
        db: &std::sync::Arc<dyn crate::db::backend::DbBackend>,
        zone_id: i64,
    ) -> u32 {
        use crate::audio::soft_mute::{SOFT_MUTE_DEFAULT_MS, SOFT_MUTE_MAX_MS};
        // PURE : le PCM atteint la sortie intact, aucune rampe n'est appliquée.
        if crate::audio::audiophile::zone_enabled(db, zone_id) {
            return 0;
        }
        crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
            .get(&format!("zone_{zone_id}_soft_mute_ms"))
            .ok()
            .flatten()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(SOFT_MUTE_DEFAULT_MS)
            .min(SOFT_MUTE_MAX_MS)
    }

    pub async fn pause(&self, zone_id: i64, device_id: Option<&str>) -> OutputCommandResult<()> {
        if let Some(did) = device_id {
            // Le backend confirme la commande AVANT que la copie mémoire et
            // la base annoncent Paused.
            let output = { self.outputs.lock().await.get(did) }.ok_or_else(|| {
                OutputCommandError::failed(
                    OutputCommand::Pause,
                    format!("output {did} is not registered"),
                )
            })?;
            output.lock().await.checked_pause().await?;
        }
        self.persist_position(zone_id).await;
        crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .save_play_state(zone_id, "paused")
            .ok();
        self.playback.pause(zone_id).await;
        Ok(())
    }

    /// Dire à l'auditeur que la session n'a pas survécu à la pause — sur les
    /// DEUX canaux, parce qu'aucun des deux ne suffit seul.
    ///
    /// L'`OutputCommandError` remonte à l'appelant HTTP : `POST
    /// /zones/{id}/resume` la rend en 502 avec son `message`
    /// (`output_command_error_response`). Mais seul le client qui a appuyé sur
    /// lecture la voit, et seulement s'il attend la réponse.
    /// `zone.playback_error` — le canal que six autres échecs de lecture
    /// utilisent déjà, poussé verbatim à TOUS les clients par `routes/ws.rs` —
    /// porte la même phrase aux autres télécommandes. La deuxième moitié de
    /// #1998 nommait déjà `resume` parmi les chemins où la cause « mourait ici ».
    ///
    /// `fatal: true` pour la raison écrite en #1960 et #2630 : sans lui, la
    /// fenêtre de grâce d'après-lecture du client web avale le message, et
    /// l'auditeur n'a — une fois de plus — que le silence.
    pub(super) fn dire_session_perdue(
        &self,
        zone_id: i64,
        titre: &str,
        position_ms: Option<u64>,
        cause: Option<&str>,
    ) -> OutputCommandError {
        let message = message_session_perdue(titre, position_ms, cause);
        // `?position_ms` et non `position_ms` : le journal doit distinguer
        // `Some(137000)` de `None`, pas écrire un `0` de plus (#3244).
        warn!(zone_id, ?position_ms, %message, "resume_stream_session_lost");
        if let Some(ref bus) = self.event_bus {
            bus.emit(
                "zone.playback_error",
                serde_json::json!({
                    "zone_id": zone_id,
                    "error": message.clone(),
                    "fatal": true,
                }),
            );
        }
        OutputCommandError::failed(OutputCommand::Resume, message)
    }

    pub async fn resume(&self, zone_id: i64, device_id: Option<&str>) -> OutputCommandResult<()> {
        // Position is preserved across pause (playback state isn't reset), so we
        // know where to resume from.
        let state = self.playback.get_state(zone_id).await;
        let position_ms = state.position_ms.max(0) as u64;
        // #3244 — cette valeur est-elle une MESURE ?
        //
        // Jumeau de #2595 : `position_ms` n'est entretenue que par l'unique
        // `update_position` de production du sondeur, et la boucle de transport
        // de `poller.rs` `continue` avant lui quand la zone n'a pas de
        // périphérique. Sur une zone navigateur la valeur reste donc figée à 0
        // depuis `play()` pendant que le morceau avance dans l'onglet : zéro n'y
        // est pas une position, c'est une absence de mesure.
        //
        // Ce que `resume` en fait, site par site :
        //
        // - `requete_de_retablissement` (le seul geste qui RELANCE à la
        //   position) et le rattrapage `checked_seek` des renderers DLNA sont
        //   déjà gardés par la présence d'une SORTIE — `did` / `device_id`. Or
        //   cette présence est exactement le prédicat de
        //   `position_entretenue_par_le_sondeur` : sans périphérique, aucune des
        //   deux branches n'est atteinte. `resume` ne peut donc PAS relancer une
        //   zone navigateur au début, et le défaut annoncé par #3244 n'est pas
        //   atteignable par là ;
        // - reste `dire_session_perdue`, qui ANNONCE la position à l'auditeur —
        //   et celle-là n'est gardée par rien. Une zone navigateur dont la
        //   session a été ramassée s'entendait dire « ne peut pas reprendre à
        //   0:00 », un horodatage inventé qui désigne le début du morceau. C'est
        //   ce que ce correctif ferme.
        //
        // On ne comble surtout pas la source : `streamer_bytes_sent` mesure le
        // TÉLÉCHARGEMENT, pas l'écoute. Seul le client connaît sa position, et
        // aucune route ne la remonte aujourd'hui (`SeekRequest` est une
        // COMMANDE, pas un rapport).
        let position_mesuree = self
            .position_entretenue_par_le_sondeur(zone_id)
            .then_some(position_ms);

        // Une reprise « sur place » suppose que la SESSION DE FLUX qui alimentait
        // la sortie a survécu à la pause. Deux façons d'y échouer, deux remèdes.
        //
        // WEBRADIO (#1629) : le flux est un DIRECT. Pendant la pause le pipeline
        // continue de se périmer — la connexion icecast peut mourir par un chemin
        // qui ne logge qu'en debug!, la sortie accumule un retard sans borne, les
        // horodatages OAAT prennent toute la durée de la pause de retard — et la
        // reprise « sur place » rend du silence sans la moindre erreur (.42 :
        // pause 15:48 → reprise 16:07, aucun son, volume dans le vide). Le remède
        // est un RE-PLAY de la station : on reprend le direct, pas un différé de
        // dix-neuf minutes. Rien de tout cela ne change ici.
        //
        // PISTE (#2512) : le MÊME silence, par une autre porte, et il n'était pas
        // gardé. Le ramasse-miettes retire toute session NON RADIO restée
        // `SESSION_IDLE_TIMEOUT` (trente minutes) sans servir un octet — la radio
        // y est explicitement exemptée, la piste non — et une lecture en pause ne
        // tire plus rien, ce que le commentaire de la borne dit mot pour mot :
        // « elle survit une demi-heure aujourd'hui ». Passé ce délai la session
        // n'existe plus, `/stream/{id}` répond 404, et toutes les familles de
        // sorties reprennent sur du vide — la locale comprise, qui va chercher
        // son PCM en HTTP.
        //
        // Le remède n'est PAS celui de la radio : on rétablit la même écoute à la
        // MÊME position. Et quand plus rien ne le permet, on le dit.
        if let Some(np) = state.now_playing.as_ref() {
            let est_radio = np.source == "radio";
            let has_url = np.source_id.as_deref().is_some_and(|s| !s.is_empty());
            // De quoi re-demander la MÊME écoute : une URL de station pour une
            // radio, une piste identifiable pour tout le reste.
            let rejouable = if est_radio {
                has_url
            } else {
                np.track_id.is_some() || has_url
            };
            let pause_longue = state
                .paused_at
                .is_some_and(|t| t.elapsed() >= RADIO_RESUME_REPLAY_AFTER);
            // Ce qui tue une session n'est pas le même selon la famille. Radio :
            // le producteur de décodage s'est terminé — plus rien n'alimente la
            // session WAV, reprendre sur place serait silencieux aussi. Piste :
            // `producer_done` n'est jamais armé (seul `decode_radio_stream_to_pcm`
            // le fait), la seule question qui vaille est « la session existe-t-elle
            // encore ».
            let session_morte = match np.stream_id.as_deref() {
                Some(sid) if est_radio => self.streamer.radio_producer_done(sid).await,
                Some(sid) => !self.streamer.session_alive(sid).await,
                None => false,
            };
            let decision = reprise_de_session(est_radio, rejouable, pause_longue, session_morte);
            if decision != RepriseDeSession::SurPlace {
                let did = device_id.map(str::to_string).or_else(|| {
                    ZoneRepo::with_backend(self.db.clone())
                        .get(zone_id)
                        .ok()
                        .flatten()
                        .and_then(|z| z.output_device_id)
                });
                match (decision, did) {
                    (RepriseDeSession::RejouerLeDirect, Some(did)) => {
                        info!(
                            zone_id,
                            paused_long = pause_longue,
                            producer_dead = session_morte,
                            "radio_resume_replay"
                        );
                        let req = PlayRequest {
                            zone_id,
                            output_device_id: Some(did),
                            track_id: None,
                            source: Some("radio".into()),
                            source_id: np.source_id.clone(),
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
                        // Même station, même écoute logique : pas de nouvelle
                        // ligne d'historique (même règle que radio_auto_retry).
                        match self.play_without_history(req).await {
                            Ok(_) => return Ok(()),
                            Err(e) => warn!(
                                zone_id,
                                error = %e,
                                "radio_resume_replay_failed_falling_back"
                            ),
                        }
                    }
                    (RepriseDeSession::RetablirALaPosition, Some(did)) => {
                        info!(zone_id, position_ms, "resume_stream_session_restore");
                        let req = requete_de_retablissement(zone_id, did, np, position_ms);
                        // Même piste, même écoute : pas de seconde ligne
                        // d'historique, exactement comme le re-play radio.
                        match self.play_without_history(req).await {
                            Ok(_) => return Ok(()),
                            // Pas de repli silencieux : la sortie n'a plus rien à
                            // jouer, la reprendre ne ferait que rejouer le défaut.
                            Err(e) => {
                                return Err(self.dire_session_perdue(
                                    zone_id,
                                    &np.title,
                                    position_mesuree,
                                    Some(&e),
                                ));
                            }
                        }
                    }
                    (RepriseDeSession::Expliquer, _)
                    | (RepriseDeSession::RetablirALaPosition, None) => {
                        return Err(self.dire_session_perdue(
                            zone_id,
                            &np.title,
                            position_mesuree,
                            None,
                        ));
                    }
                    // Radio dont on ne connaît aucune sortie : repli inchangé sur
                    // la reprise ordinaire, exactement comme avant #2512.
                    (RepriseDeSession::RejouerLeDirect, None) | (RepriseDeSession::SurPlace, _) => {
                    }
                }
            }
        }
        let output_type = if let Some(did) = device_id {
            let output = { self.outputs.lock().await.get(did) }.ok_or_else(|| {
                OutputCommandError::failed(
                    OutputCommand::Resume,
                    format!("output {did} is not registered"),
                )
            })?;
            let out = output.lock().await;
            let t = out.output_type().to_string();
            out.checked_resume().await?;
            Some(t)
        } else {
            None
        };

        self.playback.resume(zone_id).await;
        crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .save_play_state(zone_id, "playing")
            .ok();

        // Legacy DLNA/OpenHome renderers (e.g. Cyrus Stream X) restart the stream
        // on Play-after-Pause instead of resuming. Seek back to the paused
        // position once the renderer has had a moment to (re)start, so playback
        // continues instead of replaying from the top. Locks are released during
        // the wait so other zones aren't blocked.
        if matches!(output_type.as_deref(), Some("dlna" | "openhome")) && position_ms > 3000 {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            let did = device_id.expect("output type only exists with a device id");
            let output = { self.outputs.lock().await.get(did) };
            if let Some(output) = output {
                match output.lock().await.checked_seek(position_ms).await {
                    Ok(()) => info!(zone_id, position_ms, "dlna_resume_seek"),
                    Err(e) => warn!(zone_id, position_ms, error = %e, "dlna_resume_seek_failed"),
                }
            }
        }
        Ok(())
    }

    /// Les appareils qu'une AUTRE zone que `zone_id` revendique comme sienne.
    ///
    /// Sert à borner le repli de `stop` : ces sorties-là ne lui appartiennent
    /// pas, les arrêter revient à couper la musique de quelqu'un d'autre.
    /// Prend une projection `(id de zone, appareil)` plutôt que des `Zone`
    /// entières : la règle ne dépend que de ces deux champs, et une fonction
    /// qui n'exige pas de construire une zone complète est une fonction qu'on
    /// peut réellement tester.
    pub(super) fn sorties_revendiquees_par_les_autres_zones<'a>(
        zones: impl IntoIterator<Item = (Option<i64>, Option<&'a str>)>,
        zone_id: i64,
    ) -> std::collections::HashSet<String> {
        zones
            .into_iter()
            .filter(|(id, _)| *id != Some(zone_id))
            .filter_map(|(_, appareil)| appareil.map(str::to_string))
            .collect()
    }

    pub async fn stop(&self, zone_id: i64, device_id: Option<&str>) {
        self.persist_position(zone_id).await;
        crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .save_play_state(zone_id, "stopped")
            .ok();
        self.cleanup_gapless_session(zone_id).await;
        self.prefetch.clear().await;
        // Forget the last network-play record so a stop→replay of the SAME track
        // is not mistaken for the duplicate-send it guards against (play_inner).
        self.last_net_play.lock().await.remove(&zone_id);
        // Une zone navigateur arrêtée avant que l'onglet ait tiré quoi que ce
        // soit n'a rien fait entendre : son annonce en attente meurt ici
        // (#1998).
        self.oublier_annonce_navigateur(zone_id);
        let state = self.playback.get_state(zone_id).await;
        let old_stream_id = state
            .now_playing
            .as_ref()
            .and_then(|np| np.stream_id.clone());
        self.playback.stop(zone_id).await;

        // Resolve device_id: prefer explicit, fall back to zone DB
        let resolved_did = match device_id {
            Some(d) => Some(d.to_string()),
            None => crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id),
        };
        if let Some(ref did) = resolved_did {
            let arc = { self.outputs.lock().await.get(did) };
            if let Some(output) = arc {
                if let Err(e) = output.lock().await.stop().await {
                    warn!(zone_id, error = %e, "device_stop_failed");
                }
            }
        } else {
            // Aucun appareil résolu. L'ancien repli arrêtait TOUTES les sorties
            // enregistrées — et c'est ainsi qu'un `next` sur une zone
            // « navigateur » (son dans le navigateur, donc AUCUN appareil côté
            // serveur, par construction : `reject_if_zone_has_no_output_device`
            // laisse justement passer ce cas) coupait la musique de tout le
            // monde.
            //
            // Mesuré sur .18 le 28/08/2026 : dix arrêts en une heure, cadence
            // ~100 s, chacun envoyant `dlna_stop` à l'Eversolo qui jouait la
            // zone 10 et aux deux Sonos. La zone 10 n'étant elle-même jamais
            // passée en « stopped », rien ne reprenait avant que le poller ne
            // déclenche `radio_auto_retry` — jusqu'à près de trois minutes de
            // silence à chaque fois.
            //
            // Le repli garde son utilité — rattraper une sortie orpheline que
            // CETTE zone aurait laissée ouverte — mais il ne doit jamais
            // toucher une sortie qu'une AUTRE zone revendique. Même famille
            // que #2571 : un ordre qui sort du périmètre de la zone active.
            let toutes_les_zones = crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
                .list()
                .unwrap_or_default();
            let revendiquees_ailleurs = Self::sorties_revendiquees_par_les_autres_zones(
                toutes_les_zones
                    .iter()
                    .map(|z| (z.id, z.output_device_id.as_deref())),
                zone_id,
            );
            // Snapshot the Arcs first and release the registry lock, so a slow
            // or offline renderer's stop() SOAP timeout can't hold the lock and
            // starve concurrent playback for ~100s (send_to_output_lock_contention).
            let arcs: Vec<_> = {
                let outputs = self.outputs.lock().await;
                Self::sorties_a_arreter_en_repli(&outputs.list(), &revendiquees_ailleurs)
                    .iter()
                    .filter_map(|did| outputs.get(did))
                    .collect()
            };
            let arretees = arcs.len();
            for output in arcs {
                let _ = output.lock().await.stop().await;
            }
            warn!(
                zone_id,
                arretees,
                epargnees = revendiquees_ailleurs.len(),
                "stop_fallback_no_device_id — repli borné aux sorties qu'aucune autre zone ne revendique"
            );
        }
        // Remove session AFTER the output has been stopped
        if let Some(ref sid) = old_stream_id {
            self.streamer.remove_session(sid).await;
        }
    }

    pub async fn seek(
        &self,
        zone_id: i64,
        mut position_ms: u64,
        device_id: Option<&str>,
    ) -> OutputCommandResult<()> {
        let seek_start = std::time::Instant::now();
        if let Some(did) = device_id {
            let output = { self.outputs.lock().await.get(did) }.ok_or_else(|| {
                OutputCommandError::failed(
                    OutputCommand::Seek,
                    format!("output {did} is not registered"),
                )
            })?;
            output
                .lock()
                .await
                .capabilities()
                .require(OutputCommand::Seek)?;
        }
        // Clamp seek to track duration to prevent out-of-bounds seek on files
        // with incorrect metadata duration (e.g. VBR MP3 with wrong header).
        let state = self.playback.get_state(zone_id).await;
        if let Some(ref np) = state.now_playing {
            if np.duration_ms > 0 && position_ms > np.duration_ms as u64 {
                info!(
                    zone_id,
                    requested = position_ms,
                    duration = np.duration_ms,
                    "seek_clamped_to_duration"
                );
                position_ms = (np.duration_ms as u64).saturating_sub(1000);
            }
        }
        if let Some(did) = device_id {
            self.deplacer_la_sortie(zone_id, did, position_ms, &state, seek_start)
                .await?;
        }

        // La position publique et persistée ne change qu'après confirmation du
        // backend. Les `seek()` temporaires ci-dessus servent uniquement de
        // garde au poller pendant une recréation de flux.
        self.playback.seek(zone_id, position_ms as i64).await;
        let confirmed_state = self.playback.get_state(zone_id).await;
        if let Some(ref np) = confirmed_state.now_playing {
            if let Err(e) = ZoneRepo::with_backend(self.db.clone()).save_playback_position(
                zone_id,
                position_ms as i64,
                np.track_id,
                Some(np.source.as_str()),
                np.source_id.as_deref(),
            ) {
                warn!(zone_id, error = %e, "persist_seek_position_failed");
            }
        }
        Ok(())
    }

    /// Le déplacement de la sortie elle-même : selon la source (flux ou
    /// fichier) et ce que la sortie sait faire, recherche native, recréation
    /// du flux à la position demandée, ou relecture depuis le début avec
    /// recherche différée. L'état de zone n'est mis à jour qu'après, par
    /// `seek`.
    async fn deplacer_la_sortie(
        &self,
        zone_id: i64,
        did: &str,
        position_ms: u64,
        state: &crate::playback::ZoneState,
        seek_start: std::time::Instant,
    ) -> OutputCommandResult<()> {
        let original_position_ms = state.position_ms;
        // For streaming tracks on network outputs (DLNA, OpenHome, etc.),
        // the seek strategy depends on whether the stream session supports
        // HTTP Range-based seeking:
        //
        // - Proxy sessions (FLAC from Tidal/Qobuz CDN) and file sessions
        //   support Range requests.  The renderer can seek by closing the
        //   current HTTP connection and re-requesting with a byte offset.
        //   For these, a direct SOAP Seek command is sufficient — the
        //   renderer handles the rest.
        //
        // - Decoded/transcoded sessions (WAV via mpsc channel) do NOT
        //   support Range seeking.  For these, we must recreate the stream
        //   session as a fallback.
        let is_streaming_source = state
            .now_playing
            .as_ref()
            .map(|np| {
                np.source != "local"
                    && np.source != "radio"
                    && np.source != "podcast"
                    && np.stream_id.is_some()
            })
            .unwrap_or(false);

        // Determine output type from zone DB (avoids locking the output)
        let zone_output_type = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_type);
        let is_network = is_network_output_type(zone_output_type.as_deref());

        if is_streaming_source && is_network {
            // Check if the current stream session supports Range seeking
            let stream_id = state
                .now_playing
                .as_ref()
                .and_then(|np| np.stream_id.clone());
            let is_seekable = if let Some(ref sid) = stream_id {
                self.streamer.is_seekable_session(sid).await
            } else {
                false
            };

            if is_seekable {
                // Proxy/file session: the stream handler already supports
                // Range-based seeking.  Send a direct SOAP Seek — the
                // renderer will close the current connection and re-request
                // with the appropriate byte offset.  Same stream URL, no
                // interruption, no "new track" artifact.
                info!(
                    zone_id,
                    position_ms,
                    source = ?state.now_playing.as_ref().map(|np| &np.source),
                    stream_id = ?stream_id,
                    "seek_streaming_direct_on_seekable_session"
                );

                let output = { self.outputs.lock().await.get(did) }.ok_or_else(|| {
                    OutputCommandError::failed(
                        OutputCommand::Seek,
                        format!("output {did} disappeared during seek"),
                    )
                })?;
                output.lock().await.checked_seek(position_ms).await?;
                info!(
                    zone_id,
                    position_ms,
                    seek_ms = seek_start.elapsed().as_millis() as u64,
                    "seek_streaming_direct_complete"
                );
            } else {
                // Decoded/transcoded session (WAV via mpsc): no Range
                // support.  Recreate the stream so the renderer gets a
                // fresh URL to buffer from.
                info!(
                    zone_id,
                    position_ms,
                    source = ?state.now_playing.as_ref().map(|np| &np.source),
                    "seek_streaming_on_network_output_recreating_stream"
                );

                // Pre-set the seek timestamp BEFORE play() so the poller's
                // seek grace period covers the entire stream-recreation
                // window.  play() calls playback.play() which increments
                // track_generation and clears last_seek_at — we re-set it
                // again after play() returns (and once more after the Seek
                // command) to maintain continuous coverage.
                self.playback.seek(zone_id, position_ms as i64).await;

                // Re-create the stream: build a PlayRequest from the current NowPlaying
                let np = state.now_playing.as_ref().unwrap();
                let output_device_id = ZoneRepo::with_backend(self.db.clone())
                    .get(zone_id)
                    .ok()
                    .flatten()
                    .and_then(|z| z.output_device_id);
                let req = PlayRequest {
                    zone_id,
                    output_device_id,
                    track_id: np.track_id,
                    source: Some(np.source.clone()),
                    source_id: np.source_id.clone(),
                    title: Some(np.title.clone()),
                    artist_name: np.artist_name.clone(),
                    album_title: np.album_title.clone(),
                    cover_url: np.cover_path.clone(),
                    duration_ms: Some(np.duration_ms),
                    seek_ms: None,
                    temp_file_path: None,
                    sample_rate: None,
                    bit_depth: None,
                    media_format: None,
                    track_number: None,
                    disc_number: None,
                };

                match self.play_without_history(req).await {
                    Ok(_) => {
                        // play() cleared last_seek_at — re-set it immediately
                        // so the poller's seek grace covers the buffering window.
                        self.playback.seek(zone_id, position_ms as i64).await;

                        // Stream is now fresh — issue the seek on the output.
                        // Small delay to let the renderer start buffering.
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        let Some(output) = ({ self.outputs.lock().await.get(did) }) else {
                            self.playback.seek(zone_id, original_position_ms).await;
                            return Err(OutputCommandError::failed(
                                OutputCommand::Seek,
                                format!("output {did} disappeared during seek"),
                            ));
                        };
                        if let Err(error) = output.lock().await.checked_seek(position_ms).await {
                            self.playback.seek(zone_id, original_position_ms).await;
                            return Err(error);
                        }
                        // Re-set the seek timestamp so the poller grace period
                        // starts from after the Seek SOAP command, not from
                        // the play() call.
                        self.playback.seek(zone_id, position_ms as i64).await;
                        info!(
                            zone_id,
                            position_ms,
                            seek_ms = seek_start.elapsed().as_millis() as u64,
                            "seek_streaming_complete"
                        );
                    }
                    Err(e) => {
                        warn!(zone_id, error = %e, "seek_streaming_play_recreate_failed");
                        // Restore seek timestamp so the poller doesn't
                        // misinterpret the Stopped state as a playback failure.
                        self.playback.seek(zone_id, position_ms as i64).await;
                        // Fall back to direct seek (best effort)
                        let Some(output) = ({ self.outputs.lock().await.get(did) }) else {
                            self.playback.seek(zone_id, original_position_ms).await;
                            return Err(OutputCommandError::failed(
                                OutputCommand::Seek,
                                format!("output {did} disappeared during seek"),
                            ));
                        };
                        if let Err(error) = output.lock().await.checked_seek(position_ms).await {
                            self.playback.seek(zone_id, original_position_ms).await;
                            return Err(error);
                        }
                    }
                }
            }
        } else {
            // Local + OAAT outputs consume a sequential HTTP transcode
            // stream (mpsc / chunked), so we must stop+replay from the seek
            // position rather than range-seek in place. OAAT DSD is served
            // as a chunked WAV transcode that cannot be range-seeked — a raw
            // Range request lands mid-DSD-block and plays WHITE NOISE
            // (Xavier). Recreating with seek_ms restarts the transcode at
            // the correct offset (paired with the DSD-decode seek fix).
            let is_local_output =
                zone_output_type.as_deref() == Some("local") || zone_output_type.is_none();
            let is_oaat_output = zone_output_type.as_deref() == Some("oaat");
            let has_track = state.now_playing.is_some();

            if (is_local_output || is_oaat_output) && has_track {
                info!(zone_id, position_ms, "seek_local_output_recreating_stream");
                match self
                    .replay_zone_at_position(zone_id, position_ms, "seek")
                    .await
                {
                    Ok(()) => info!(
                        zone_id,
                        position_ms,
                        seek_ms = seek_start.elapsed().as_millis() as u64,
                        "seek_local_output_complete"
                    ),
                    Err(e) => {
                        warn!(zone_id, error = %e, "seek_local_output_play_failed");
                        self.playback.seek(zone_id, original_position_ms).await;
                        return Err(OutputCommandError::failed(OutputCommand::Seek, e));
                    }
                }
            } else {
                let output = { self.outputs.lock().await.get(did) }.ok_or_else(|| {
                    OutputCommandError::failed(
                        OutputCommand::Seek,
                        format!("output {did} disappeared during seek"),
                    )
                })?;
                output.lock().await.checked_seek(position_ms).await?;
            }
        }
        Ok(())
    }

    /// La session de flux `stream_id` est-elle encore inscrite ?
    ///
    /// A lire APRES [`Self::wait_stream_data_ready`], et seulement quand il a
    /// rendu `false` : ce dernier ne dit que « pas encore », jamais « plus
    /// jamais ». Une session VIVANTE et muette est un telechargement lent
    /// (DASH Hi-Res Tidal) ; une session DISPARUE est un producteur mort.
    /// C'est l'idiome que `resume` emploie deja (#2512) pour ne pas reprendre
    /// en silence sur une session ramassee.
    pub async fn stream_session_alive(&self, stream_id: &str) -> bool {
        self.streamer.session_alive(stream_id).await
    }
}
