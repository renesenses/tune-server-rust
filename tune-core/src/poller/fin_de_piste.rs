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
        use crate::db::zone_repo::{AutoplayMode, ZoneRepo};
        let mode = ZoneRepo::with_backend(self.db.clone()).get_autoplay_mode(zone_id);
        match mode {
            AutoplayMode::RandomAlbum
            | AutoplayMode::RandomArtist
            | AutoplayMode::RandomYear
            | AutoplayMode::RandomTracks => {
                self.continuer_aleatoirement(zone_id, mode, device_id).await;
                return;
            }
            AutoplayMode::Off | AutoplayMode::Similar => {}
        }

        if mode == AutoplayMode::Similar {
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
                            self.orchestrator.stop(zone_id, device_id).await;
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
                        self.orchestrator.stop(zone_id, device_id).await;
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
                        self.orchestrator.stop(zone_id, device_id).await;
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
                    self.orchestrator.stop(zone_id, device_id).await;
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
                    self.orchestrator.stop(zone_id, device_id).await;
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
        self.orchestrator.stop(zone_id, device_id).await;
    }

    async fn continuer_aleatoirement(
        &self,
        zone_id: i64,
        mode: crate::db::zone_repo::AutoplayMode,
        device_id: Option<&str>,
    ) {
        let result = (|| {
            let generated = crate::playback::auto_dj::generate_random_queue(&self.db, mode)?;
            let ids: Vec<i64> = generated
                .iter()
                .filter_map(|t| t["track_id"].as_i64())
                .collect();
            if ids.is_empty() {
                return Ok(None);
            }
            use crate::db::play_queue_repo::{PlayQueueRepo, QueueInput};
            let queue = PlayQueueRepo::with_backend(self.db.clone());
            let items: Vec<_> = ids
                .iter()
                .map(|id| QueueInput::Local { track_id: *id })
                .collect();
            // Position et pistes retenues viennent de la transaction d'ajout :
            // file vide, fin streaming, ajout concurrent ou rescan ne doivent
            // pas nous faire jouer une ancienne position ni annoncer une piste perdue.
            let outcome = queue.insert_at_bilan(zone_id, &items, None)?;
            let Some(position) = outcome.start else {
                return Ok(None);
            };
            let ids: Vec<_> = outcome.retenus.iter().map(|&i| ids[i]).collect();
            let generated: Vec<_> = outcome
                .retenus
                .iter()
                .map(|&i| generated[i].clone())
                .collect();
            Ok::<_, String>(Some((position, ids, generated)))
        })();
        match result {
            Ok(Some((position, ids, generated))) => {
                if let Some(ref bus) = self.event_bus {
                    bus.emit(
                        "playback.autoplay_tracks_added",
                        serde_json::json!({
                            "zone_id": zone_id, "autoplay_mode": mode.as_str(),
                            "track_ids": ids, "tracks": generated,
                            "seed_track_id": null, "seed_artist": null,
                        }),
                    );
                }
                match self.orchestrator.play_from_queue(zone_id, position).await {
                    Ok(_) => return,
                    Err(e) => {
                        warn!(zone_id, mode = mode.as_str(), error = %e, "autoplay_play_failed")
                    }
                }
            }
            Ok(None) => info!(
                zone_id,
                mode = mode.as_str(),
                "autoplay_no_local_candidates"
            ),
            Err(e) => {
                warn!(zone_id, mode = mode.as_str(), error = %e, "autoplay_random_queue_failed")
            }
        }
        self.orchestrator.stop(zone_id, device_id).await;
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
        // #4362 (point 2) — les pistes d'un serveur multimédia ABSENT sont
        // enjambées d'un coup, et dites en nommant le serveur, plutôt qu'une
        // à une par la boucle d'échecs ci-dessous (qui ne dit que le motif
        // brut, et s'arrête au 25e).
        let attempt_pos = self
            .orchestrator
            .enjamber_les_serveurs_absents(zone_id, next_pos)
            .await
            .map_or(next_pos, |(position, _)| position);
        // #4806 — les titres BANNIS sont sautés AVANT toute tentative : ce ne
        // sont pas des échecs, ils ne consomment aucun des deux budgets
        // ci-dessous. Si tout ce qui restait était banni, la file est finie.
        let mut attempt_pos = match self
            .orchestrator
            .enjamber_les_pistes_bannies(zone_id, attempt_pos)
            .await
        {
            crate::orchestrator::Enjambee::Rien => attempt_pos,
            crate::orchestrator::Enjambee::Reprise(p) => p,
            crate::orchestrator::Enjambee::FileEpuisee => return false,
        };
        // 🔴 DEUX compteurs, et c'est tout le correctif (Bertrand, 21/09/2026 :
        // « la lecture d'une playlist s'arrête sur un morceau non trouvé »).
        //
        // `skipped` garde son rôle : les pannes SYSTÉMIQUES — jeton expiré,
        // réseau mort — qu'il ne faut pas marteler une fois par piste de la
        // file. C'est ce que dit le commentaire de `MAX_CONSECUTIVE_SKIPS`.
        //
        // `injouables` compte les refus que le service prononce PISTE PAR
        // PISTE. Les mêler était le défaut : sur une playlist dont
        // l'indisponibilité est groupée, vingt-six titres morts d'affilée
        // épuisaient un budget prévu pour une panne, et la lecture s'arrêtait.
        let mut skipped = 0u32;
        let mut injouables = 0u32;
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
                    if super::refus_de_piste::refus_propre_a_la_piste(&e.to_string()) {
                        injouables += 1;
                        if injouables >= PLAFOND_PISTES_INJOUABLES {
                            warn!(
                                zone_id,
                                injouables,
                                "auto_next_unplayable_limit_reached — le service a refusé \
                                 autant de pistes d'affilée : on s'arrête pour ne pas boucler"
                            );
                            break;
                        }
                    } else {
                        skipped += 1;
                        // A run this long is not "one bad track" any more — an
                        // expired token or a dead network would otherwise have us
                        // hammer the service once per queued item.
                        if skipped >= MAX_CONSECUTIVE_SKIPS {
                            warn!(zone_id, skipped, "auto_next_skip_limit_reached");
                            break;
                        }
                    }
                    match Self::next_position_after(zone_state, attempt_pos) {
                        // Same slot again means repeat-one on a dead track:
                        // skipping would spin forever.
                        Some(p) if p != attempt_pos => {
                            // #4806 — après un échec aussi, les titres bannis
                            // qui suivent sont enjambés : sans cela, un titre
                            // banni juste derrière une piste injouable serait
                            // joué.
                            attempt_pos = match self
                                .orchestrator
                                .enjamber_les_pistes_bannies(zone_id, p)
                                .await
                            {
                                crate::orchestrator::Enjambee::Rien => p,
                                crate::orchestrator::Enjambee::Reprise(q) => q,
                                crate::orchestrator::Enjambee::FileEpuisee => break,
                            };
                        }
                        _ => break,
                    }
                }
            }
        }
        false
    }

    /// #4173 — à la fin prononcée à l'HORLOGE, ce que l'on sait de
    /// l'enchaînement : le verdict, le flux armé (s'il en est un), les octets
    /// que le renderer en a tirés.
    ///
    /// Trois lectures, aucune écriture : le flux rangé sous la zone à
    /// l'armement (`flux_pre_arme`), ses octets servis, et l'URI que le
    /// renderer rapporte jouer. Voir `decisions::enchainement_sur_le_flux_arme`.
    pub(super) async fn enchainement_a_l_horloge(
        &self,
        zone_id: i64,
        is_dlna: bool,
        gapless_sent: bool,
        status: &OutputStatus,
    ) -> (decisions::EnchainementArme, Option<String>, Option<u64>) {
        if !is_dlna || !gapless_sent {
            return (decisions::EnchainementArme::Aucun, None, None);
        }
        let flux_arme = self.orchestrator.flux_pre_arme(zone_id).await;
        let octets_tires = match flux_arme.as_deref() {
            Some(sid) => self.orchestrator.streamer_bytes_sent(sid).await,
            None => None,
        };
        let verdict = decisions::enchainement_sur_le_flux_arme(
            is_dlna,
            gapless_sent,
            flux_arme.as_deref(),
            status.current_uri.as_deref(),
            octets_tires,
        );
        (verdict, flux_arme, octets_tires)
    }

    /// #3967 — demander au renderer de basculer LUI-MÊME sur la suivante
    /// qu'il tient déjà, au lieu de détruire son flux armé et de tout
    /// relancer.
    ///
    /// Rend `true` seulement si l'appareil a ACQUITTÉ la consigne. Un refus,
    /// un SOAP muet, une sortie absente du registre : `false`, et l'appelant
    /// reprend le repli d'aujourd'hui sans rien avoir changé.
    ///
    /// La consigne n'est pas une preuve : l'adoption qui la suit est
    /// surveillée sur [`BASCULE_DELAI_SECS`] et relance la piste ADOPTÉE si
    /// le renderer n'a pas bougé.
    pub(super) async fn demander_la_bascule(&self, zone_id: i64, device_id: &str) -> bool {
        let output_arc = {
            let outputs = self.outputs.lock().await;
            outputs.get(device_id)
        };
        let Some(output_arc) = output_arc else {
            return false;
        };
        let t0 = Instant::now();
        let issue = {
            let output = output_arc.lock().await;
            output.basculer_sur_la_suivante_preparee().await
        };
        match issue {
            Ok(()) => {
                info!(
                    zone_id,
                    device = %device_id,
                    next_ms = t0.elapsed().as_millis() as u64,
                    "gapless_bascule_demandee"
                );
                true
            }
            Err(e) => {
                warn!(
                    zone_id,
                    device = %device_id,
                    error = %e,
                    "gapless_bascule_refusee"
                );
                false
            }
        }
    }

    /// #4173 — la fin à l'horloge ADOPTE l'enchaînement du renderer.
    ///
    /// Même avance que `gapless_transition_detected` (`advance_queue_metadata`,
    /// qui fait adopter le flux pré-armé à la zone, #3442) : ni
    /// `SetAVTransportURI`, ni `Play`, ni `stream_session_removed` sur le flux
    /// que le renderer tire. L'adoption est PROVISOIRE tant que le renderer
    /// n'a pas donné signe de vie sur la piste adoptée : `adoption_horloge`
    /// la surveille (`decisions::suite_de_l_adoption`), et le repli
    /// `SetAVTransportURI` + `Play` reprend — sur la piste adoptée — si rien
    /// ne vient dans `ADOPTION_HORLOGE_DELAI_SECS`.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn adopter_l_enchainement_a_l_horloge(
        &self,
        zone_id: i64,
        zone_state: &crate::playback::ZoneState,
        status: &OutputStatus,
        ps: &mut ZonePollState,
        flux: String,
        preuve: decisions::EnchainementArme,
        octets_tires: Option<u64>,
        track_duration_ms: u64,
        wall_elapsed: u64,
    ) {
        info!(
            zone_id,
            position_ms = status.position_ms,
            track_dur = track_duration_ms,
            wall_secs = wall_elapsed,
            past_end_ticks = ps.past_end_ticks,
            stream_id = %flux,
            uri = ?status.current_uri,
            octets_tires = ?octets_tires,
            preuve = ?preuve,
            arm_to_advance_ms = ps
                .gapless_sent_at
                .map(|t| t.elapsed().as_millis() as u64)
                .unwrap_or(0),
            "gapless_adoption_a_l_horloge"
        );
        ps.gapless_sent = false;
        ps.gapless_sent_at = None;
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
        ps.gapless_arm_logged = None;
        ps.gapless_dsd_skip_pos = None;
        ps.transition(fsm::Transition::TransitionDetectee);
        match self
            .position_a_avancer(zone_id, zone_state, arme_avant)
            .await
        {
            Some(next_pos) => {
                info!(zone_id, next_pos, "gapless_advance_metadata");
                if let Err(e) = self
                    .orchestrator
                    .advance_queue_metadata(zone_id, next_pos)
                    .await
                {
                    warn!(zone_id, error = %e, "gapless_advance_failed");
                }
                ps.gapless_cooldown = 4;
                ps.scrobbled_key = None;
                ps.adoption_horloge = Some(AdoptionHorloge {
                    depuis: Instant::now(),
                    position_figee_ms: status.position_ms,
                    flux,
                    preuve,
                    // #3967 — une bascule COMMANDÉE se juge en trois sondages,
                    // pas en huit : le renderer n'a rien à charger.
                    delai_secs: if preuve == decisions::EnchainementArme::Bascule {
                        BASCULE_DELAI_SECS
                    } else {
                        ADOPTION_HORLOGE_DELAI_SECS
                    },
                });
            }
            None => {
                self.handle_track_end(zone_id, zone_state).await;
            }
        }
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
        // #4806 / #5143 — une suivante BANNIE (locale ou de service) n'est
        // jamais armée : armée, elle serait jouée par le renderer sans que la
        // file ait son mot à dire. On arme la prochaine JOUABLE, celle que
        // l'avance de la file jouerait : l'enchaînement sans blanc est gardé
        // par-dessus la bannie.
        let Some(next_pos) = Self::prochaine_position_jouable(&self.db, zone_id, zone_state) else {
            return GaplessPrep::NotArmed;
        };

        // L'identite de ce qu'on s'apprete a armer, lue AVANT de le resoudre :
        // une ligne de file, pas une position (#3026). C'est la seule trace de
        // ce que le renderer aura reellement accepte.
        let ligne = crate::db::play_queue_repo::PlayQueueRepo::with_backend(self.db.clone())
            .get_at(zone_id, next_pos)
            .ok()
            .flatten();

        let arme = ligne.map(|e| ArmedNext {
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
            return self
                .armer_le_fichier_local(zone_id, next_pos, device_id, arme)
                .await;
        }

        // v0.9 gapless characterization: time the next-track resolution and
        // surface failures at warn. These paths were debug-only, so streaming
        // gapless instability (Tidal DASH download slowness, URL/token issues)
        // was invisible in production journald. Logging only — no behaviour change.
        self.armer_le_flux_suivant(zone_id, next_pos, device_id, arme)
            .await
    }

    /// Sortie qui préfère un FICHIER pour l'enchaînement : la piste suivante
    /// est résolue en chemin local et remise à la sortie par `set_next_media`.
    /// Chaque issue rend son verdict ; rien ne retombe sur le flux.
    async fn armer_le_fichier_local(
        &self,
        zone_id: i64,
        next_pos: i64,
        device_id: &str,
        arme: Option<ArmedNext>,
    ) -> GaplessPrep {
        let t0 = Instant::now();
        match self
            .orchestrator
            .resolve_gapless_next_local_file(zone_id, next_pos)
            .await
        {
            Ok(resolved) if resolved.file_path.is_some() => {
                // #1894 — mesurée, jamais supposée (`media_byte_seekable`).
                let byte_seekable = self
                    .orchestrator
                    .media_byte_seekable(resolved.stream_id.as_deref())
                    .await;
                let output_arc = {
                    let outputs = self.outputs.lock().await;
                    outputs.get(device_id)
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
                    byte_seekable,
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
                        // Chemin FICHIER LOCAL (OAAT en DSD natif) : aucune
                        // suivante à vérifier auprès d'un renderer réseau.
                        GaplessPrep::Armed(arme, SuivantePreparee::Inconnue)
                    }
                    Err(e) => {
                        warn!(zone_id, error = %e, "gapless_set_next_local_file_failed");
                        GaplessPrep::NotArmed
                    }
                };
            }
            Ok(_) => {
                info!(zone_id, "gapless_local_file_skipped_no_local_next");
                GaplessPrep::NotArmed
            }
            Err(e) => {
                warn!(zone_id, error = %e, "gapless_local_file_resolve_failed");
                GaplessPrep::NotArmed
            }
        }
    }

    /// Enchaînement par FLUX : la piste suivante est résolue (transcode
    /// compris), on attend ses premiers octets dans le budget, puis la sortie
    /// la reçoit en `set_next_media`. Un DSD à suivre est signalé, pas armé.
    async fn armer_le_flux_suivant(
        &self,
        zone_id: i64,
        next_pos: i64,
        device_id: &str,
        arme: Option<ArmedNext>,
    ) -> GaplessPrep {
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
                // #1894 — mesurée, jamais supposée (`media_byte_seekable`).
                let byte_seekable = self
                    .orchestrator
                    .media_byte_seekable(resolved.stream_id.as_deref())
                    .await;
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
                        byte_seekable,
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
                        // #3967 — l'acquittement ne prouve rien. On demande
                        // maintenant à l'appareil ce qu'il RETIENT et ce qu'il
                        // DÉCLARE pouvoir faire ; c'est la seule chose qui
                        // autorisera, en fin de piste, une bascule par `Next`
                        // au lieu de tout relancer. Une sortie qui ne sait pas
                        // répondre rend `Inconnue` et rien ne change.
                        let t_verif = Instant::now();
                        let tenue = output.suivante_preparee(&resolved.url).await;
                        info!(
                            zone_id,
                            title = %resolved.title,
                            resolve_ms,
                            streaming = is_streaming,
                            suivante = ?tenue,
                            verif_ms = t_verif.elapsed().as_millis() as u64,
                            "gapless_next_set"
                        );
                        GaplessPrep::Armed(arme, tenue)
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

#[cfg(test)]
mod autoplay_2271_tests {
    use super::*;
    use crate::db::{
        backend::DbBackend,
        play_queue_repo::PlayQueueRepo,
        sqlite::SqliteDb,
        zone_repo::{AutoplayMode, ZoneRepo},
    };
    use crate::outputs::mock::MockOutput;

    async fn exercise(mode: AutoplayMode, with_previous: bool, with_library: bool) {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(db);
        let repo = ZoneRepo::with_backend(db.clone());
        let zone_id = repo
            .create("Autoplay", Some("mock"), Some("mock-autoplay"))
            .unwrap();
        repo.update_autoplay_mode(zone_id, mode).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        if with_library {
            let path = tmp.path().join("track.wav");
            let mut wav =
                crate::audio::wav::build_wav_header_with_duration(2, 44100, 16, Some(1000))
                    .to_vec();
            wav.resize(wav.len() + 44100 * 4, 0);
            std::fs::write(&path, wav).unwrap();
            db.execute_batch(
                "INSERT INTO artists (id, name) VALUES (1, 'Artist');
                INSERT INTO albums (id, title, artist_id, year) VALUES (1, 'Album', 1, 2001);",
            )
            .unwrap();
            db.execute("INSERT INTO tracks (id, title, artist_id, album_id, file_path, format, sample_rate, bit_depth, duration_ms) \
                        VALUES (1, 'Random local', 1, 1, ?, 'wav', 44100, 16, 1000)", &[&path.to_str().unwrap()]).unwrap();
        }
        if with_previous {
            db.execute(
                "INSERT INTO queue_items (zone_id, position, source, source_id, title) \
                        VALUES (?, 0, 'qobuz', 'previous', 'Previous streaming')",
                &[&zone_id],
            )
            .unwrap();
        }
        let playback = Arc::new(crate::playback::PlaybackManager::new());
        let outputs = Arc::new(Mutex::new(crate::outputs::registry::OutputRegistry::new()));
        outputs
            .lock()
            .await
            .register(Box::new(MockOutput::new("mock-autoplay", "Autoplay")));
        let orchestrator = Arc::new(crate::orchestrator::PlaybackOrchestrator::new(
            db.clone(),
            playback.clone(),
            Arc::new(crate::http::streamer::AudioStreamer::new(0)),
            Arc::new(Mutex::new(crate::streaming::ServiceRegistry::new())),
            outputs.clone(),
            None,
        ));
        let bus = Arc::new(crate::event_bus::EventBus::new());
        let mut events = bus.subscribe();
        let poller = PositionPoller::new(
            orchestrator,
            playback.clone(),
            outputs,
            db.clone(),
            Arc::new(Mutex::new(HashMap::new())),
        )
        .with_event_bus(bus);
        let state = crate::playback::ZoneState {
            zone_id,
            queue_length: i64::from(with_previous),
            queue_position: 0,
            now_playing: with_previous.then(|| crate::playback::NowPlaying {
                title: "Previous streaming".into(),
                source: "qobuz".into(),
                source_id: Some("previous".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        // Vraie entree de fin de piste : pas le generateur appele seul.
        poller.handle_track_end(zone_id, &state).await;
        let queue = PlayQueueRepo::with_backend(db);
        let actual = playback.get_state(zone_id).await;
        let expected = with_library && mode != AutoplayMode::Off;
        assert_eq!(
            queue.count_all(zone_id).unwrap(),
            i64::from(with_previous) + i64::from(expected),
            "{mode:?}"
        );
        if expected {
            assert_eq!(
                actual.state,
                crate::playback::PlayState::Playing,
                "{mode:?}: {actual:?}"
            );
            assert_eq!(actual.queue_position, i64::from(with_previous));
            let target = poller.outputs.lock().await.get("mock-autoplay").unwrap();
            let output_status = target.lock().await.get_status().await.unwrap();
            assert_eq!(output_status.state, TransportState::Playing);
            assert!(
                output_status.current_uri.is_some(),
                "la sortie a recu une URL a jouer"
            );

            assert_eq!(
                actual.now_playing.as_ref().and_then(|np| np.track_id),
                Some(1)
            );
            let mut added = None;
            while let Ok(event) = events.try_recv() {
                if event.event_type == "playback.autoplay_tracks_added" {
                    added = Some(event.data);
                }
            }
            let added = added.expect("evenement apres ajout effectif");
            assert_eq!(added["autoplay_mode"], mode.as_str());
            assert_eq!(added["track_ids"], serde_json::json!([1]));
            // La continuation reste active au bout du lot ajoute.
            poller.handle_track_end(zone_id, &actual).await;
            let next = playback.get_state(zone_id).await;
            assert_eq!(next.state, crate::playback::PlayState::Playing);
            assert_eq!(next.queue_position, i64::from(with_previous) + 1);
            assert_eq!(
                queue.count_all(zone_id).unwrap(),
                i64::from(with_previous) + 2
            );
        } else {
            assert_eq!(actual.state, crate::playback::PlayState::Stopped);
            assert!(events.try_recv().is_err(), "aucun faux ajout");
        }
    }

    #[tokio::test]
    async fn autoplay_2271_random_modes_start_without_seed_and_after_streaming() {
        for mode in [
            AutoplayMode::RandomAlbum,
            AutoplayMode::RandomArtist,
            AutoplayMode::RandomYear,
            AutoplayMode::RandomTracks,
        ] {
            exercise(mode, false, true).await;
            exercise(mode, true, true).await;
        }
    }

    #[tokio::test]
    async fn autoplay_2271_empty_library_stops_and_off_never_fills_the_queue() {
        for mode in [
            AutoplayMode::RandomAlbum,
            AutoplayMode::RandomArtist,
            AutoplayMode::RandomYear,
            AutoplayMode::RandomTracks,
        ] {
            exercise(mode, false, false).await;
        }
        exercise(AutoplayMode::Off, true, true).await;
    }
}
