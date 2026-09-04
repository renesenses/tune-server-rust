use super::*;

impl PlaybackOrchestrator {
    /// Crée le flux WAV éphémère demandé par un renderer qui parcourt les
    /// radios du MediaServer.
    ///
    /// La connexion à la station ne commence qu'ici, au GET audio — jamais au
    /// Browse ni au HEAD. La route HTTP possède la durée de vie de la session
    /// et la retire lorsque son corps est terminé ou abandonné.
    pub async fn create_media_server_radio_session(&self, radio_url: String) -> String {
        let wav_info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            file_size: None,
            duration_ms: None,
            ..Default::default()
        };
        let (stream_id, tx, data_ready, session) =
            self.streamer.create_radio_session(wav_info, 256).await;
        let stream_id_for_task = stream_id.clone();
        let session_for_done = session.clone();

        info!(
            stream_id = %stream_id,
            url = %radio_url,
            "media_server_radio_decode_started"
        );
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                decode_radio_stream_to_pcm(radio_url, tx, data_ready, session, None, None)
            })
            .await;

            session_for_done
                .producer_done
                .store(true, std::sync::atomic::Ordering::Relaxed);
            // Réveille aussi le corps qui attend encore la détection du vrai
            // format. `notify_one` mémorise un permis si l'échec précède son
            // premier poll, contrairement à `notify_waiters`.
            session_for_done.data_ready.notify_one();
            // `create_radio_session` conserve un émetteur de garde pour les
            // flux permanents. Cette session est liée à une requête : quand le
            // producteur finit, le corps HTTP doit recevoir EOF.
            session_for_done.close_sender().await;

            match result {
                Ok(Ok(())) => debug!(
                    stream_id = %stream_id_for_task,
                    "media_server_radio_decode_ended"
                ),
                Ok(Err(error)) => warn!(
                    stream_id = %stream_id_for_task,
                    error = %error,
                    "media_server_radio_decode_failed"
                ),
                Err(error) => warn!(
                    stream_id = %stream_id_for_task,
                    error = %error,
                    "media_server_radio_decode_task_panicked"
                ),
            }
        });

        stream_id
    }

    pub(super) async fn resolve_streaming_url(
        &self,
        service_name: &str,
        req: &PlayRequest,
    ) -> Result<ResolvedStream, String> {
        let source_id = req
            .source_id
            .as_deref()
            .ok_or("source_id required for streaming")?;

        // Check for prefetched PCM data before downloading.
        // If the prefetch engine has already decoded this track, serve
        // the PCM directly via a streaming session — zero download delay.
        // Skip prefetch for network outputs (DLNA) when buffer is truncated
        // (30s mode) — the renderer needs the full file.
        //
        // A seek must resolve a FRESH stream at the requested position. The
        // prefetch buffer always starts at position 0, so serving it on a seek
        // would (a) play from the wrong position and (b) race the recreated
        // local output: the buffered PCM feed completes before ASIO/WASAPI
        // attaches, leaving the stream with 0 frames → playback stops.
        // (DEvir: seek on a TIDAL track → title stays but music stops.)
        // Only consider the prefetch buffer when NOT seeking.
        let prefetched = if req.seek_ms.is_some_and(|ms| ms > 0) {
            None
        } else {
            self.prefetch.take_prefetched(service_name, source_id).await
        };
        if let Some(prefetched) = prefetched {
            let is_network = req
                .output_device_id
                .as_deref()
                .is_some_and(|id| !id.starts_with("local:") && !id.starts_with("oaat:"));
            let bytes_per_sec = (prefetched.sample_rate as usize)
                * (prefetched.bit_depth as usize / 8)
                * (prefetched.channels as usize);
            let buffered_ms = if bytes_per_sec > 0 {
                (prefetched.pcm_data.len() as u64 * 1000) / bytes_per_sec as u64
            } else {
                0
            };
            let is_truncated = prefetch_buffer_truncated(buffered_ms, prefetched.duration_ms);

            // Skip a truncated prefetch buffer for EVERY output, not just DLNA.
            // The prefetch head-start is only ~30s; `serve_prefetched_pcm` feeds
            // exactly that PCM into the session and then drops the sender. On a
            // network output that meant a short file; on a LOCAL EXCLUSIVE output
            // (ASIO) the blocking HTTP read never gets a clean EOF at the loop
            // point, so once the 30s buffer is consumed the audio thread starves
            // and freezes until the 20s watchdog resets the host to WASAPI
            // (DEvir bug-20, repeat-one on a >30s track). Fetching the full
            // stream instead keeps the exclusive read fed for the whole track.
            if is_truncated {
                info!(
                    service = service_name,
                    source_id = %source_id,
                    buffered_ms,
                    duration_ms = prefetched.duration_ms,
                    is_network,
                    "prefetch_skip_truncated_serving_full_stream"
                );
            } else {
                info!(
                    service = service_name,
                    source_id = %source_id,
                    title = %prefetched.title,
                    buffer_bytes = prefetched.pcm_data.len(),
                    "prefetch_hit_serving_buffered_pcm"
                );
                return self.serve_prefetched_pcm(prefetched, req).await;
            }
        }

        let registry = self.services.lock().await;
        let svc = registry
            .get(service_name)
            .ok_or_else(|| format!("unknown service: {service_name}"))?;
        let mut svc = svc.write().await;

        // Try to get the track URL; if it fails with an auth error, attempt
        // a token refresh and retry once. This handles Qobuz tokens expiring
        // mid-session (search still works without auth, but playback doesn't).
        let stream_data = match svc.get_track_url(source_id, None).await {
            Ok(data) => data,
            Err(ref e)
                if {
                    let msg = e.to_string();
                    msg.contains("401") || msg.contains("403")
                } =>
            {
                info!(
                    service = service_name,
                    error = %e,
                    "streaming_auth_error_attempting_refresh"
                );
                if svc.refresh_if_needed().await.unwrap_or(false) {
                    svc.get_track_url(source_id, None)
                        .await
                        .map_err(|e| e.to_string())?
                } else {
                    return Err(e.to_string());
                }
            }
            Err(e) => return Err(e.to_string()),
        };

        let info = StreamInfo {
            format: stream_data.quality.codec.to_lowercase(),
            mime_type: stream_data.mime_type.clone(),
            sample_rate: stream_data.quality.sample_rate,
            bit_depth: stream_data.quality.bit_depth,
            channels: 2,
            file_size: None,
            duration_ms: None,
            ..Default::default()
        };

        let is_https = stream_data.url.starts_with("https://");
        // file:// URLs come from Tidal DASH multi-segment downloads — the fMP4
        // has already been assembled on disk by get_track_url().
        let is_dash_file = stream_data.url.starts_with("file://");
        let is_oaat_stream = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("oaat:") || id.starts_with("oaat-group:"));
        let is_local_stream = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("local:"));

        // Local and OAAT outputs expect raw PCM in a WAV container.
        // Streaming services deliver compressed audio (FLAC, AAC, etc.)
        // which LocalOutput cannot decode — it would interpret compressed
        // bytes as raw PCM samples, producing white noise.
        // Fix: download → decode → WAV transcode, same as local files.
        let (stream_url, sid, out_mime, stream_file_size) = if is_local_stream || is_oaat_stream {
            let upstream_url = stream_data.url.clone();
            let codec = stream_data.quality.codec.to_lowercase();
            // Cap the WAV rate to the zone's max_sample_rate (e.g. an OAAT
            // endpoint whose DAC tops out at 96k). resolve_local_track applies
            // this cap for local files; the streaming path historically did NOT,
            // so a 192k Qobuz/Tidal track was transcoded to a 192k WAV and handed
            // to a 96k OAAT endpoint → the DAC rejected the rate → silence with no
            // server-side error (radio at 44.1/48k on the same zone played fine).
            // decode_to_pcm_streaming_with_levels resamples to `sr`, so capping
            // here downsamples the PCM, not just the WAV header.
            let zone_max_sample_rate = ZoneRepo::with_backend(self.db.clone())
                .get(req.zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.max_sample_rate);
            let mut sr = stream_data.quality.sample_rate;
            if let Some(max_sr) = zone_max_sample_rate {
                if sr > max_sr {
                    info!(
                        zone_id = req.zone_id,
                        source_rate = sr,
                        max_rate = max_sr,
                        "streaming_zone_max_sample_rate_cap_applied"
                    );
                    sr = max_sr;
                }
            }
            // Local output: 32-bit to avoid 24-bit byte misalignment noise
            // (see local_needs_wav comment in resolve_local_track).
            // OAAT: cap at 24-bit (endpoints may not support 32-bit WAV).
            let bd = if is_local_stream {
                32
            } else {
                cap_output_bit_depth(stream_data.quality.bit_depth)
            };

            let wav_info = StreamInfo {
                format: "wav".into(),
                mime_type: "audio/wav".into(),
                sample_rate: sr,
                bit_depth: bd,
                channels: 2,
                file_size: None,
                duration_ms: None,
                ..Default::default()
            };

            // Guard against a stale/cleaned-up DASH temp file (mirrors the
            // `is_dash_file` DLNA path below). The local transcode runs
            // fire-and-forget in a spawned task, so a missing file would decode
            // to nothing while play() still reports output_sent=true. Fail early
            // so the caller sees the real failure instead of silent no-playback.
            // (Reported on ASIO with 24/192 Tidal DASH after the temp file is gone.)
            if upstream_url.starts_with("file://") {
                let fp = upstream_url
                    .strip_prefix("file://")
                    .unwrap_or(&upstream_url);
                let size = std::fs::metadata(fp).map(|m| m.len()).unwrap_or(0);
                if size == 0 {
                    warn!(path = %fp, "streaming_dash_file_missing_or_empty");
                    return Err(format!(
                        "DASH temp file missing or empty (needs re-download): {fp}"
                    ));
                }
            }

            let (session_id, tx, data_ready) =
                self.streamer.create_session(wav_info, false, 256).await;

            {
                let sessions = self.streamer.sessions_state();
                let sessions = sessions.lock().await;
                if let Some(session) = sessions.get(&session_id) {
                    session
                        .wav_header_included
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }

            info!(
                service = service_name,
                codec = %codec,
                sample_rate = sr,
                bit_depth = bd,
                "streaming_transcode_to_wav_for_local_output"
            );

            let ev_bus = self.event_bus.clone();
            let playback = self.playback.clone();
            let zone_id = req.zone_id;
            let streamer_for_eof = self.streamer.clone();
            let session_id_for_eof = session_id.clone();
            // Pré-chargement gapless : pas de forwarder (voir `levels_prewarm`).
            let attach_levels = self.levels_attach_allowed(zone_id);
            // Seek d'une piste streaming (Qobuz/Tidal) sur sortie locale/OAAT :
            // le chemin local passait déjà l'offset au décodeur, celui-ci
            // repartait TOUJOURS de zéro — l'audio recommençait au début alors
            // que l'UI affichait la position demandée (repros Hard To Say
            // Goodbye 405s et Bina 1015s, .18, 28/07).
            let seek_s = req.seek_ms.map(|ms| ms as f64 / 1000.0).unwrap_or(0.0);

            // Detect file:// URLs from DASH multi-segment downloads — the fMP4
            // is already on disk, skip the HTTP download step.
            let is_dash_local = upstream_url.starts_with("file://");
            // Le CDN YouTube accepte les requetes Range. Un M4A peut garder son
            // atome `moov` a la fin : une source HTTP seekable permet a
            // Symphonia de lire cet index puis de revenir aux premiers paquets,
            // sans attendre le telechargement complet (#1885). Les autres
            // services gardent leur chemin eprouve dans cette premiere vague.
            let use_http_range = service_name.eq_ignore_ascii_case("youtube")
                && matches!(codec.as_str(), "m4a" | "mp4" | "aac");

            // Background task: download upstream → temp file → decode → WAV → session
            tokio::spawn(async move {
                // Audio-levels channel so the web client VU-meter works for
                // streaming-service content played through local/OAAT outputs.
                // Paced to the playback clock by the forwarder; without a bus,
                // the receiver is dropped and the decoder's sends are no-ops.
                let levels_tx = match ev_bus.filter(|_| attach_levels) {
                    Some(bus) => {
                        let play_seq = playback.current_play_seq(zone_id).await;
                        spawn_paced_levels_forwarder(
                            bus,
                            playback,
                            zone_id,
                            play_seq,
                            (seek_s * 1000.0) as i64,
                        )
                    }
                    None => {
                        tokio::sync::mpsc::unbounded_channel::<crate::audio::tap::RawWindow>().0
                    }
                };

                // Sonde Range AVANT d'envoyer un en-tete WAV. Si le CDN le
                // refuse, aucun octet n'a encore rejoint la session et le repli
                // historique par fichier temporaire reste parfaitement propre.
                let ranged_source = if use_http_range {
                    let upstream = upstream_url.clone();
                    match tokio::task::spawn_blocking(move || {
                        crate::audio::http_range::HttpRangeSource::open(&upstream)
                    })
                    .await
                    {
                        Ok(Ok(source)) => {
                            info!("streaming_http_range_decode_selected");
                            Some(source)
                        }
                        Ok(Err(e)) => {
                            info!(error = %e, "streaming_http_range_unavailable_falling_back");
                            None
                        }
                        Err(e) => {
                            warn!(error = %e, "streaming_http_range_probe_task_failed");
                            None
                        }
                    }
                } else {
                    None
                };

                // Sans source Range, conserver strictement le chemin existant :
                // fichier DASH deja local ou telechargement complet vers un temp.
                let tmp_file = if ranged_source.is_some() {
                    None
                } else if is_dash_local {
                    let file_path = upstream_url
                        .strip_prefix("file://")
                        .unwrap_or(&upstream_url)
                        .to_string();
                    let file_size = std::fs::metadata(&file_path)
                        .ok()
                        .map(|m| m.len())
                        .unwrap_or(0);
                    info!(
                        path = %file_path,
                        file_size,
                        "streaming_dash_file_already_on_disk"
                    );
                    Some((file_path, false))
                } else {
                    let tmp_path = std::env::temp_dir()
                        .join(format!("tune-stream-{}.{}", uuid::Uuid::new_v4(), codec))
                        .to_string_lossy()
                        .to_string();
                    let tmp_path_clone = tmp_path.clone();
                    let upstream = upstream_url.clone();
                    let download_result = tokio::task::spawn_blocking(move || {
                        let resp = crate::http::client::blocking_builder()
                            .timeout(std::time::Duration::from_secs(120))
                            .build()
                            .and_then(|c| c.get(&upstream).send());
                        match resp {
                            Ok(mut r) if r.status().is_success() => {
                                let mut file = match std::fs::File::create(&tmp_path_clone) {
                                    Ok(f) => f,
                                    Err(e) => return Err(format!("tmp create: {e}")),
                                };
                                match std::io::copy(&mut r, &mut file) {
                                    Ok(bytes) => {
                                        debug!(bytes, path = %tmp_path_clone, "streaming_download_complete");
                                        Ok(tmp_path_clone)
                                    }
                                    Err(e) => Err(format!("download copy: {e}")),
                                }
                            }
                            Ok(r) => Err(format!("upstream HTTP {}", r.status())),
                            Err(e) => Err(format!("upstream fetch: {e}")),
                        }
                    })
                    .await;

                    match download_result {
                        Ok(Ok(path)) => Some((path, true)),
                        Ok(Err(e)) => {
                            warn!(error = %e, "streaming_transcode_download_failed");
                            // #3287 : ces deux sorties quittaient la tache
                            // AVANT le `end_session_input` du bas, en se
                            // contentant d'effacer le fichier temporaire. La
                            // session restait donc inscrite, sans producteur,
                            // canal ouvert — et le gapless s'y enchainait.
                            abandonner_la_session_de_transcodage(
                                &streamer_for_eof,
                                &session_id_for_eof,
                                &tmp_path,
                            )
                            .await;
                            return;
                        }
                        Err(e) => {
                            warn!(error = %e, "streaming_transcode_task_join_failed");
                            abandonner_la_session_de_transcodage(
                                &streamer_for_eof,
                                &session_id_for_eof,
                                &tmp_path,
                            )
                            .await;
                            return;
                        }
                    }
                };

                let tx_for_decode = tx.clone();
                // Drop the original sender so the channel closes when decode finishes.
                drop(tx);
                let decode_result = if let Some(source) = ranged_source {
                    tokio::task::spawn_blocking(move || {
                        crate::audio::decode::decode_http_range_to_pcm_streaming_seeked(
                            source,
                            &codec,
                            Some(sr),
                            Some(2),
                            Some(bd),
                            tx_for_decode,
                            32768,
                            data_ready,
                            levels_tx,
                            seek_s,
                        )
                    })
                    .await
                } else {
                    let tmp_file_clone = tmp_file.as_ref().unwrap().0.clone();
                    tokio::task::spawn_blocking(move || {
                        crate::audio::decode::decode_to_pcm_streaming_seeked(
                            &tmp_file_clone,
                            Some(sr),
                            Some(2),
                            Some(bd),
                            tx_for_decode,
                            32768,
                            data_ready,
                            levels_tx,
                            seek_s,
                        )
                    })
                    .await
                };

                // Clean up the temp file — but ONLY if WE downloaded it. For a
                // file:// DASH source, tmp_file IS the Tidal-cache-owned
                // tune-dash-*.mp4 that is still referenced by the cached stream
                // URL. Deleting it here made every subsequent re-resolution
                // (repeat=one, or a seek that recreates the local stream) see the
                // file gone, mark the cache stale, and re-download the whole
                // ~54MB DASH — while concurrent transcodes raced on the emptied
                // file (file_size=0 → decode failed). That was the ASIO "repeat"
                // runaway (also on Qobuz). Leave cache-owned files alone.
                if let Some((tmp_file, owned)) = tmp_file
                    && owned
                {
                    let _ = std::fs::remove_file(&tmp_file);
                }

                match decode_result {
                    Ok(Ok((_bit_depth, actual_rate))) => {
                        if actual_rate != sr {
                            tracing::info!(
                                api_rate = sr,
                                actual_rate,
                                "streaming_sample_rate_mismatch_wav_header_has_correct_rate"
                            );
                        }
                        debug!("streaming_transcode_complete_progressive");
                    }
                    Ok(Err(e)) => {
                        warn!(error = %e, "streaming_transcode_decode_failed");
                    }
                    Err(e) => {
                        warn!(error = %e, "streaming_transcode_decode_task_panic");
                    }
                }

                // Fin d'entrée : sans ça, le keep-alive de la session garde le
                // canal ouvert après la fin du décodage, le corps HTTP ne se
                // termine jamais, et l'OAAT (gapless interne basé sur l'EOF)
                // reste muet en fin de piste puis se fait relancer par le
                // superviseur — silence + « le dernier morceau est rejoué ».
                streamer_for_eof
                    .end_session_input(&session_id_for_eof)
                    .await;
            });

            let server_ip = self.server_ip();
            let url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");
            (url, Some(session_id), "audio/wav".to_string(), None)
        } else if is_dash_file {
            // DASH multi-segment fMP4 already assembled on disk by get_track_url().
            // DLNA renderers can't decode fMP4+FLAC directly, and chunked WAV
            // causes noise on many renderers (darTZeel, Eversolo, etc.).
            // Pre-transcode to a FLAC temp file so we can serve with Content-Length.
            let dash_file_path = stream_data
                .url
                .strip_prefix("file://")
                .unwrap_or(&stream_data.url)
                .to_string();

            if !std::path::Path::new(&dash_file_path).exists() {
                warn!(path = %dash_file_path, "streaming_dash_file_missing_skipping_decode");
                return Err("DASH file missing (already consumed by prior decode)".into());
            }

            // Chaîne DSP de la zone, chargée UNE fois et réutilisée par la
            // décision de cache chaud ET par le transcodage ci-dessous. Un
            // second chargement pourrait observer un traitement tout juste
            // activé et ranger un transcodage traité sous la clé du flux brut,
            // empoisonnant tous les accès ultérieurs à cette piste.
            //
            // ⚠️ Ce bras ne chargeait que l'ÉGALISEUR (#2863) : le convolveur de
            // correction de pièce et le ReplayGain y étaient perdus, exactement
            // comme sur les bras non-DASH. `StreamingDsp` porte les trois.
            let mut dash_dsp = self.load_streaming_dsp(
                req.zone_id,
                req.track_id,
                stream_data.quality.sample_rate,
                2,
            );
            let dash_dsp_active = dash_dsp.is_active();

            // Browser (Web Audio) zones pull the stream themselves via <audio> and
            // issue arbitrary byte-Range requests to buffer/seek. Our native FLAC
            // encoder writes no SEEKTABLE, so a mid-file offset never lands on a
            // frame boundary; Safari can't resync and playback stalls a few seconds
            // in while the timeline keeps running (Philippe Vella, Tidal HI-RES on
            // the browser "Cet ordinateur" zone, 0.9.42). WAV's linear byte↔sample
            // layout makes every Range resolvable, so serve WAV to browser zones —
            // the same format the local output already plays fine for these tracks.
            let is_browser_output = ZoneRepo::with_backend(self.db.clone())
                .get(req.zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_type)
                .as_deref()
                == Some("browser");

            struct DashWarm {
                cache_path: String,
                enc_format: &'static str,
                key_bit_depth: u16,
                force_flac: bool,
            }

            // Warm-cache (opt-in, TUNE_DASH_WARM_CACHE): a prior play/warm of this
            // exact track+quality+format may have left a finished transcode on
            // disk. All the format-decision work (incl. a dlna_supports_mime await)
            // runs ONLY when the flag is on, so a disabled build is byte-identical.
            // `warm` is None when the flag is off or a zone EQ is active (EQ is
            // out of the key). When Some, its format decision is authoritative for
            // the whole DASH arm (see dash_enc_format below), so the cache key and
            // the encoded bytes can never disagree.
            let warm: Option<DashWarm> = if dash_warm_cache_enabled() {
                let wsr = stream_data.quality.sample_rate;
                let wbd = stream_data.quality.bit_depth.max(16).min(24);
                let wdid = req.output_device_id.as_deref().unwrap_or("");
                let wflac =
                    ZoneRepo::with_backend(self.db.clone()).get_dlna_native_flac(req.zone_id);
                let wfmt = if is_browser_output {
                    "wav"
                } else if wdid.is_empty()
                    || wflac
                    || self.dlna_supports_mime(wdid, "audio/flac").await
                {
                    "flac"
                } else {
                    "wav"
                };
                let wkbd = if wfmt == "wav" { 16 } else { wbd };
                // Le traitement de zone n'entre PAS dans la clé de cache : un
                // flux traité ne peut donc jamais partager la clé d'un flux
                // brut. La garde ne couvrait que l'égaliseur ; le convolveur et
                // le ReplayGain la traversaient (#2863).
                if !dash_dsp_active {
                    Some(DashWarm {
                        cache_path: crate::transcode_cache::cache_path_streaming(
                            service_name,
                            source_id,
                            wfmt,
                            wsr,
                            wkbd,
                            2,
                        ),
                        enc_format: wfmt,
                        key_bit_depth: wkbd,
                        force_flac: wflac,
                    })
                } else {
                    None
                }
            } else {
                None
            };

            // Cache hit → serve the finished transcode, skipping the whole
            // download+decode+encode. The fMP4 on disk is left untouched (not
            // renamed to `.decoding` / consumed), so a concurrent path can still
            // use it. Mirrors the common metadata tail before returning.
            if let Some(w) = warm.as_ref() {
                if crate::transcode_cache::is_hit(&w.cache_path) {
                    crate::transcode_cache::touch(&w.cache_path);
                    if let Ok(md) = std::fs::metadata(&w.cache_path) {
                        let file_size = md.len();
                        let hit_mime = if w.enc_format == "flac" {
                            "audio/flac"
                        } else {
                            "audio/wav"
                        };
                        let file_info = StreamInfo {
                            format: w.enc_format.into(),
                            mime_type: hit_mime.into(),
                            sample_rate: stream_data.quality.sample_rate,
                            bit_depth: w.key_bit_depth,
                            channels: 2,
                            file_size: Some(file_size),
                            duration_ms: None,
                            ..Default::default()
                        };
                        let session_id = self
                            .streamer
                            .create_file_session(file_info, w.cache_path.clone(), false)
                            .await;
                        let server_ip = self.server_ip();
                        let stream_url =
                            self.streamer
                                .get_stream_url(&session_id, &server_ip, w.enc_format);
                        info!(cache = %w.cache_path, file_size, "streaming_dash_warm_cache_hit");
                        // Warm N+1 into the cache while this track plays (same
                        // device → same FLAC/WAV decision, so inherit it).
                        self.spawn_warm_next_streaming(
                            req.zone_id,
                            source_id.to_string(),
                            w.enc_format,
                        );

                        let has_title = req.title.as_deref().is_some_and(|s| !s.is_empty());
                        let (title, artist, album, duration_ms, cover_path) = if has_title {
                            (
                                req.title.clone().unwrap_or_default(),
                                req.artist_name.clone(),
                                req.album_title.clone(),
                                req.duration_ms,
                                req.cover_url.clone(),
                            )
                        } else {
                            match svc.get_track(source_id).await {
                                Ok(track) => (
                                    track.title,
                                    Some(track.artist),
                                    track.album,
                                    Some(track.duration_ms as i64),
                                    track.cover_path,
                                ),
                                Err(_) => (
                                    req.title
                                        .clone()
                                        .filter(|s| !s.is_empty())
                                        .unwrap_or_else(|| "Unknown".into()),
                                    req.artist_name.clone(),
                                    req.album_title.clone(),
                                    req.duration_ms,
                                    req.cover_url.clone(),
                                ),
                            }
                        };
                        return Ok(ResolvedStream {
                            url: stream_url,
                            mime_type: hit_mime.into(),
                            title,
                            artist,
                            album,
                            duration_ms,
                            source: service_name.into(),
                            cover_url: cover_path,
                            stream_id: Some(session_id),
                            file_size: Some(file_size),
                            sample_rate: Some(stream_data.quality.sample_rate),
                            bit_depth: Some(stream_data.quality.bit_depth as u32),
                            channels: Some(2),
                            origin_url: None,
                            bitrate_kbps: None,
                        });
                    }
                }
            }

            let unique_path = format!("{}.decoding", &dash_file_path);
            if std::fs::rename(&dash_file_path, &unique_path).is_err() {
                warn!(path = %dash_file_path, "streaming_dash_file_already_being_decoded");
                return Err("DASH file already being decoded".into());
            }

            let sr = stream_data.quality.sample_rate;
            let bd = stream_data.quality.bit_depth.max(16).min(24);

            let tmp_path = std::env::temp_dir()
                .join(format!("tune-dash-transcode-{}.flac", uuid::Uuid::new_v4()))
                .to_string_lossy()
                .to_string();

            info!(
                path = %unique_path,
                tmp = %tmp_path,
                sample_rate = sr,
                bit_depth = bd,
                "streaming_dash_pre_transcode_to_flac"
            );

            // Strict DLNA renderers (Revox, Denon, Marantz) reject FLAC — their
            // Sink doesn't advertise audio/flac, so they fetch the file but play
            // nothing. Serve them LPCM/WAV instead, like the local-file path.
            // Otherwise keep FLAC (smaller, Content-Length). Previously these
            // streaming paths always emitted audio/flac (Philippe / Revox S100).
            let dash_did = req.output_device_id.as_deref().unwrap_or("");
            // Honour the per-zone "native FLAC" override for streaming DASH too
            // (Tidal/Qobuz Hi-Res), not just local files: some renderers decode
            // FLAC but never advertise it (Marco's Denon Ceol N12 returns an
            // empty GetProtocolInfo Sink), so negotiation wrongly falls back to
            // WAV. When the zone forces native FLAC, keep FLAC here as well.
            //
            // When the warm-cache key was computed above, REUSE its decision
            // instead of re-deriving it: the same logic evaluated twice can
            // diverge (device cache refresh, zone toggle flipped mid-request)
            // and would store a transcode under a key describing other bytes.
            let (dash_enc_format, dash_force_flac) = match warm.as_ref() {
                Some(w) => (w.enc_format, w.force_flac),
                None => {
                    let force =
                        ZoneRepo::with_backend(self.db.clone()).get_dlna_native_flac(req.zone_id);
                    let fmt = if is_browser_output {
                        // Browser pulls with byte-Range requests; a seektable-less
                        // FLAC stalls it (see is_browser_output note above). WAV.
                        "wav"
                    } else if dash_did.is_empty()
                        || force
                        || self.dlna_supports_mime(dash_did, "audio/flac").await
                    {
                        "flac"
                    } else {
                        "wav"
                    };
                    (fmt, force)
                }
            };
            // Make the streaming-DLNA format decision explicit in the log so we
            // can tell why a renderer got WAV vs FLAC (Marco: multiple Denon
            // zones — is the "native FLAC" toggle set on the ZONE being played?).
            info!(
                zone_id = req.zone_id,
                device_id = %dash_did,
                native_flac_override = dash_force_flac,
                chosen_format = dash_enc_format,
                "streaming_dash_dlna_format_decision"
            );

            // Streaming remux (#1146, opt-in TUNE_DASH_STREAM_REMUX): chunked-stream
            // the remuxed FLAC to a Lavf-class renderer (DMP-A8) AS the DASH file
            // downloads, matching Qobuz's instant start — no wait for the whole
            // file + no re-encode. Only FLAC + no-EQ (a WAV renderer or a zone EQ
            // needs decoded PCM → keep the file path). Reads the GROWING fMP4 via
            // the dash_growth registry when TUNE_DASH_STREAM_DECODE armed the
            // background download, so playback begins on the first fragments.
            if dash_enc_format == "flac"
                && !dash_dsp_active
                && std::env::var("TUNE_DASH_STREAM_REMUX")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false)
            {
                let info = StreamInfo {
                    format: "flac".into(),
                    mime_type: "audio/flac".into(),
                    sample_rate: sr,
                    bit_depth: bd,
                    channels: 2,
                    file_size: None, // chunked — no Content-Length
                    duration_ms: None,
                    ..Default::default()
                };
                let (session_id, tx, data_ready, _session) =
                    self.streamer.create_radio_session(info, 256).await;
                let up = unique_path.clone();
                tokio::spawn(async move {
                    let up_stream = up.clone();
                    let r = tokio::task::spawn_blocking(move || {
                        crate::audio::decode::remux_flac_dash_stream(&up_stream, tx)
                    })
                    .await;
                    match r {
                        Ok(Ok(())) => debug!("streaming_dash_remux_stream_ended"),
                        Ok(Err(e)) => warn!(error = %e, "streaming_dash_remux_stream_failed"),
                        Err(e) => warn!(error = %e, "streaming_dash_remux_stream_panic"),
                    }
                    let _ = std::fs::remove_file(&up);
                });
                data_ready.notify_one();
                info!(
                    zone_id = req.zone_id,
                    "streaming_dash_remux_chunked_started"
                );

                let server_ip = self.server_ip();
                let stream_url = self
                    .streamer
                    .get_stream_url(&session_id, &server_ip, "flac");

                let has_title = req.title.as_deref().is_some_and(|s| !s.is_empty());
                let (title, artist, album, duration_ms, cover_path) = if has_title {
                    (
                        req.title.clone().unwrap_or_default(),
                        req.artist_name.clone(),
                        req.album_title.clone(),
                        req.duration_ms,
                        req.cover_url.clone(),
                    )
                } else {
                    match svc.get_track(source_id).await {
                        Ok(track) => (
                            track.title,
                            Some(track.artist),
                            track.album,
                            Some(track.duration_ms as i64),
                            track.cover_path,
                        ),
                        Err(_) => (
                            req.title
                                .clone()
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| "Unknown".into()),
                            req.artist_name.clone(),
                            req.album_title.clone(),
                            req.duration_ms,
                            req.cover_url.clone(),
                        ),
                    }
                };
                return Ok(ResolvedStream {
                    url: stream_url,
                    mime_type: "audio/flac".into(),
                    title,
                    artist,
                    album,
                    duration_ms,
                    source: service_name.into(),
                    cover_url: cover_path,
                    stream_id: Some(session_id),
                    file_size: None,
                    sample_rate: Some(sr),
                    bit_depth: Some(bd as u32),
                    channels: Some(2),
                    origin_url: None,
                    bitrate_kbps: None,
                });
            }

            let tmp_path_clone = tmp_path.clone();
            let unique_path_clone = unique_path.clone();
            // When falling back to WAV/LPCM (renderer has no audio/flac sink),
            // the served WAV is advertised with `DLNA.ORG_PN=LPCM`, a 16-bit-only
            // DLNA profile. A 24-bit Hi-Res stream (Tidal/Qobuz) served under it
            // plays SILENCE on renderers like the Ruark R3 / LHC-62 (Yves,
            // #1137). Cap the LPCM fallback at 16-bit; FLAC keeps full hi-res.
            let dash_is_wav = dash_enc_format == "wav";
            // VU-mètres : le PCM décodé de ce pré-transcode part aussi vers le
            // forwarder de niveaux (cadencé par lui — voir #1105). Sans ça,
            // une piste DASH (Tidal HI-RES) sur DLNA/browser laissait les
            // aiguilles figées. Le chemin remux (opt-in TUNE_DASH_REMUX) ne
            // décode rien : VU légitimement muets dans ce cas.
            let dash_levels_tx = self.levels_forwarder_if_allowed(req.zone_id, 0).await;
            // Durée du pré-transcode, sur la ligne de FIN.
            //
            // C'est l'étape qui domine le démarrage d'une piste DASH (Tidal
            // HI-RES) vers un renderer réseau : décodage intégral en PCM puis
            // ré-encodage, avant que le moindre octet ne parte. Le journal
            // disait qu'elle avait eu lieu, jamais combien elle avait coûté :
            // il fallait soustraire deux horodatages. Or le fichier de journal
            // est plafonné et tourne — la ligne de DÉBUT peut avoir disparu de
            // l'export d'un testeur alors que la ligne de FIN y est encore, et
            // la durée devenait alors impossible à établir. Même convention que
            // `tidal_dash_multi_segment_download_complete`, qui porte déjà son
            // `elapsed_ms` (`streaming/tidal.rs`).
            let pre_transcode_start = std::time::Instant::now();
            let transcode_result = tokio::task::spawn_blocking(move || {
                // Fast path: Tidal HI-RES DASH is ALREADY FLAC (frames inside an
                // fMP4). If the renderer takes FLAC and no zone EQ is active, REMUX
                // (copy the FLAC frames + STREAMINFO into a .flac) instead of
                // decode→PCM→re-encode — a ~59s CPU transcode becomes a sub-second
                // I/O copy, bit-identical (#1146). Opt-in via TUNE_DASH_REMUX;
                // WAV renderers and EQ zones fall through to the decode path.
                let remux = !dash_is_wav
                    && !dash_dsp_active
                    && std::env::var("TUNE_DASH_REMUX")
                        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                        .unwrap_or(false);
                if remux {
                    return crate::audio::decode::remux_flac_dash(
                        &unique_path_clone,
                        &tmp_path_clone,
                    );
                }

                let decoded = crate::audio::decode::decode_to_pcm(
                    &unique_path_clone,
                    Some(sr),
                    Some(2),
                    0.0,
                    0.0,
                )?;

                let mut pcm_bytes = decoded.pcm_bytes();
                let mut actual_bd = decoded.bit_depth;

                if dash_is_wav && actual_bd > 16 {
                    pcm_bytes = crate::audio::decode::convert_pcm_bytes(&pcm_bytes, actual_bd, 16);
                    actual_bd = 16;
                }

                // ReplayGain, égaliseur, puis convolveur FIR — l'ordre de
                // `transcode_source_to_file`. Ce bras n'appliquait que le
                // deuxième (#2863).
                dash_dsp.process(&mut pcm_bytes, actual_bd);

                // Niveaux post-EQ : les VU décrivent ce qui sera entendu.
                if let Some(ref ltx) = dash_levels_tx {
                    crate::audio::tap::send_windowed_pcm(
                        ltx,
                        &pcm_bytes,
                        actual_bd,
                        decoded.channels as u16,
                        decoded.sample_rate,
                    );
                }

                let rt = tokio::runtime::Handle::try_current()
                    .map_err(|e| format!("no tokio runtime: {e}"))?;
                let encoded_data = rt.block_on(async {
                    let mut encoder = crate::audio::encoder::AudioEncoder::new(
                        dash_enc_format,
                        decoded.sample_rate,
                        actual_bd as u32,
                        decoded.channels,
                    );
                    encoder.start().await?;
                    encoder.write(&pcm_bytes).await?;
                    encoder.finish().await
                })?;

                std::fs::write(&tmp_path_clone, &encoded_data)
                    .map_err(|e| format!("write temp file: {e}"))?;

                let file_size = encoded_data.len() as u64;
                Ok::<(u64, u16, u32), String>((file_size, actual_bd, decoded.sample_rate))
            })
            .await;

            let _ = std::fs::remove_file(&unique_path);

            match transcode_result {
                Ok(Ok((file_size, actual_bd, actual_sr))) => {
                    info!(
                        tmp = %tmp_path,
                        file_size,
                        bit_depth = actual_bd,
                        elapsed_ms = pre_transcode_start.elapsed().as_millis() as u64,
                        "streaming_dash_pre_transcode_complete"
                    );

                    let dash_mime = if dash_enc_format == "flac" {
                        "audio/flac"
                    } else {
                        "audio/wav"
                    };
                    let file_info = StreamInfo {
                        format: dash_enc_format.into(),
                        mime_type: dash_mime.into(),
                        sample_rate: sr,
                        // Use the *encoded* depth (`actual_bd`), which the WAV
                        // fallback caps at 16-bit — otherwise DIDL/WAV would
                        // advertise 24-bit LPCM and the renderer plays silence.
                        bit_depth: actual_bd,
                        channels: 2,
                        file_size: Some(file_size),
                        duration_ms: None,
                        ..Default::default()
                    };
                    // Store into the warm cache (atomic rename) when enabled, so
                    // the next play of this exact track is an instant hit. Any
                    // rename failure falls back to serving the temp file — no
                    // regression. `evict` keeps the cache under its size cap.
                    //
                    // Guard: only store when the DECODED reality matches the key.
                    // `quality.bit_depth`/`sample_rate` come from the service API
                    // and can lie about the actual stream; a later hit would then
                    // advertise a depth/rate the file doesn't have in DIDL — the
                    // Ruark-silence class of bug (#1137, 24-bit LPCM). A skipped
                    // store just means the old temp-file behaviour for this track.
                    let key_matches_reality =
                        warm.as_ref().is_some_and(|w| w.key_bit_depth == actual_bd)
                            && sr == actual_sr;
                    let serve_path = match warm.as_ref() {
                        Some(w)
                            if key_matches_reality
                                && std::fs::rename(&tmp_path, &w.cache_path).is_ok() =>
                        {
                            tokio::task::spawn_blocking(crate::transcode_cache::evict);
                            info!(cache = %w.cache_path, file_size, "streaming_dash_warm_cache_store");
                            w.cache_path.clone()
                        }
                        _ => tmp_path,
                    };
                    // Warm the next streaming track into the cache in the
                    // background (same zone/device → inherit dash_enc_format).
                    if warm.is_some() {
                        self.spawn_warm_next_streaming(
                            req.zone_id,
                            source_id.to_string(),
                            dash_enc_format,
                        );
                    }
                    let session_id = self
                        .streamer
                        .create_file_session(file_info, serve_path, false)
                        .await;

                    let server_ip = self.server_ip();
                    let url =
                        self.streamer
                            .get_stream_url(&session_id, &server_ip, dash_enc_format);
                    (
                        url,
                        Some(session_id),
                        dash_mime.to_string(),
                        Some(file_size),
                    )
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "streaming_dash_pre_transcode_failed");
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(format!("DASH transcode failed: {e}"));
                }
                Err(e) => {
                    warn!(error = %e, "streaming_dash_pre_transcode_task_panic");
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(format!("DASH transcode task panic: {e}"));
                }
            }
        } else if is_https {
            let codec_lower = stream_data.quality.codec.to_lowercase();
            // Codecs that legacy DLNA renderers can't decode must be
            // pre-transcoded to FLAC. AAC/MP4 (most renderers reject AAC over
            // DLNA) plus Opus/Ogg-Vorbis: YouTube delivers Opus-in-WebM, which
            // old renderers like the Cyrus Stream X reject outright (no
            // audio/webm or audio/opus sink), leaving the transport in
            // ERROR_OCCURRED.
            let needs_flac_transcode = codec_lower == "aac"
                || codec_lower == "mp4"
                || stream_data.mime_type.contains("mp4")
                || AudioFormat::from_extension(&codec_lower)
                    .is_some_and(|f| f.needs_transcode_for_dlna());

            if needs_flac_transcode {
                // AAC/MP4 streams need transcoding for DLNA — most renderers
                // (DMP-A8, etc.) don't support AAC via DLNA.  Pre-transcode to
                // FLAC temp file so we serve with Content-Length (chunked WAV
                // causes noise on many renderers).
                let sr = stream_data.quality.sample_rate;
                let bd = stream_data.quality.bit_depth.max(16).min(24) as u16;

                info!(
                    service = service_name,
                    codec = %codec_lower,
                    sample_rate = sr,
                    "streaming_aac_transcode_to_wav_channel"
                );

                // ── Téléchargement court, puis CANAL streaming ──
                //
                // L'ancien chemin transcodait la PISTE ENTIÈRE en fichier avant
                // de jouer : télécharger + tout décoder + tout encoder = 34 s
                // mesurées entre la décision et le play (Tidal AAC → DMP-A8,
                // .18, 25/08). Le canal WAV — le chemin des DSD et des radios,
                // au contrat rendu honnête en 0.9.106 — démarre dès les
                // premiers blocs décodés. Seul le téléchargement du fichier
                // AAC reste devant le play : quelques secondes.
                let upstream_url = stream_data.url.clone();
                let codec = codec_lower.clone();
                let tmp_dl = std::env::temp_dir()
                    .join(format!("tune-stream-{}.{}", uuid::Uuid::new_v4(), codec))
                    .to_string_lossy()
                    .to_string();
                let tmp_dl_clone = tmp_dl.clone();
                let dl = tokio::task::spawn_blocking(move || {
                    let resp = crate::http::client::blocking_builder()
                        .timeout(std::time::Duration::from_secs(120))
                        .build()
                        .and_then(|c| c.get(&upstream_url).send())
                        .map_err(|e| format!("upstream fetch: {e}"))?;
                    if !resp.status().is_success() {
                        return Err(format!("upstream HTTP {}", resp.status()));
                    }
                    let bytes = resp.bytes().map_err(|e| format!("download: {e}"))?;
                    std::fs::write(&tmp_dl_clone, &bytes).map_err(|e| format!("write dl: {e}"))?;
                    Ok::<(), String>(())
                })
                .await;
                match dl {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        warn!(error = %e, "streaming_aac_download_failed");
                        let _ = std::fs::remove_file(&tmp_dl);
                        return Err(format!("AAC download failed: {e}"));
                    }
                    Err(e) => {
                        warn!(error = %e, "streaming_aac_download_task_panic");
                        let _ = std::fs::remove_file(&tmp_dl);
                        return Err(format!("AAC download task panic: {e}"));
                    }
                }

                let info = StreamInfo {
                    format: "wav".into(),
                    mime_type: "audio/wav".into(),
                    sample_rate: sr,
                    bit_depth: bd,
                    channels: 2,
                    ..Default::default()
                };
                let (session_id, tx, data_ready) =
                    self.streamer.create_session(info, false, 256).await;
                // Chaîne DSP de la zone (#2863). Ce bras servait le PCM décodé
                // TEL QUEL : égaliseur, convolveur et ReplayGain y étaient
                // calculés côté interface puis jetés. Le relais les applique au
                // fil de l'eau, sans rien bufferiser de plus — le démarrage
                // immédiat conquis en 0.9.106 est préservé. Sans traitement
                // actif, le canal reste celui d'avant, à l'octet près.
                let aac_dsp = self.load_streaming_dsp(req.zone_id, req.track_id, sr, 2);
                let tx = if aac_dsp.is_active() {
                    info!(
                        zone_id = req.zone_id,
                        "streaming_aac_channel_dsp_relay_inserted"
                    );
                    spawn_streaming_dsp_relay(aac_dsp, bd, true, tx)
                } else {
                    tx
                };
                {
                    let sessions = self.streamer.sessions_state();
                    let sessions = sessions.lock().await;
                    if let Some(session) = sessions.get(&session_id) {
                        session
                            .wav_header_included
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }

                let ev_bus = self.event_bus.clone();
                let playback = self.playback.clone();
                let zone_id = req.zone_id;
                let attach_levels = self.levels_attach_allowed(zone_id);
                let fp = tmp_dl.clone();
                tokio::spawn(async move {
                    let err_bus = ev_bus.clone();
                    let levels_tx = match ev_bus.filter(|_| attach_levels) {
                        Some(bus) => {
                            let play_seq = playback.current_play_seq(zone_id).await;
                            spawn_paced_levels_forwarder(bus, playback, zone_id, play_seq, 0)
                        }
                        None => {
                            tokio::sync::mpsc::unbounded_channel::<crate::audio::tap::RawWindow>().0
                        }
                    };
                    let fp_clone = fp.clone();
                    let tx_clone = tx.clone();
                    drop(tx);
                    let result = tokio::task::spawn_blocking(move || {
                        crate::audio::decode::decode_to_pcm_streaming_seeked(
                            &fp_clone,
                            Some(sr),
                            Some(2),
                            Some(bd),
                            tx_clone,
                            32768,
                            data_ready,
                            levels_tx,
                            0.0,
                        )
                    })
                    .await;
                    let _ = std::fs::remove_file(&fp);
                    match result {
                        Ok(Ok(_)) => {
                            debug!("streaming_aac_channel_complete");
                        }
                        Ok(Err(e)) => {
                            warn!(error = %e, "streaming_aac_channel_decode_failed");
                            if let Some(ref bus) = err_bus {
                                bus.emit(
                                    "zone.playback_error",
                                    serde_json::json!({
                                        "zone_id": zone_id,
                                        "error": format!("Impossible de décoder la piste : {e}"),
                                    }),
                                );
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "streaming_aac_channel_task_panic");
                        }
                    }
                });

                let server_ip = self.server_ip();
                let url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");
                (url, Some(session_id), "audio/wav".to_string(), None)
            } else {
                // Non-AAC codecs (FLAC, etc.) — check if the DLNA renderer
                // actually supports this MIME type before proxying directly.
                // Strict renderers (Denon, Marantz, Revox) reject FLAC because
                // their GetProtocolInfo Sink doesn't list audio/flac.  In that
                // case, transcode to WAV (LPCM) which has a proper DLNA.ORG_PN
                // profile and is universally supported.
                let zone = ZoneRepo::with_backend(self.db.clone())
                    .get(req.zone_id)
                    .ok()
                    .flatten();
                let zone_output_type = zone.as_ref().and_then(|z| z.output_type.clone());
                let is_dlna = zone_output_type.as_deref() == Some("dlna");
                let device_id = req
                    .output_device_id
                    .as_deref()
                    .or(zone.as_ref().and_then(|z| z.output_device_id.as_deref()))
                    .unwrap_or("");
                let renderer_supports_mime = if is_dlna
                    && (stream_data.mime_type == "audio/flac"
                        || stream_data.mime_type == "audio/x-flac")
                    && !device_id.is_empty()
                {
                    self.dlna_supports_mime(device_id, &stream_data.mime_type)
                        .await
                } else {
                    true
                };

                // Chaîne DSP de la zone (#2863). Le bras « proxy verbatim »
                // ci-dessous ne décode RIEN : il relaie les octets du CDN. Un
                // égaliseur, une correction de pièce ou un ReplayGain armés y
                // étaient donc calculés puis jetés, sans une ligne de journal —
                // « je règle mon égaliseur, j'écoute du Qobuz sur ma zone
                // réseau, et je n'entends aucune différence ».
                let sr = stream_data.quality.sample_rate;
                let mut https_dsp = self.load_streaming_dsp(req.zone_id, req.track_id, sr, 2);
                let https_dsp_active = https_dsp.is_active();

                if streaming_needs_pretranscode(renderer_supports_mime, https_dsp_active) {
                    // Deux raisons d'arriver ici : le renderer ne sait pas lire
                    // le FLAC (→ WAV/LPCM), ou un traitement doit entrer dans le
                    // signal (→ FLAC pleine profondeur, le renderer sait le
                    // lire). Même schéma que le pré-transcodage AAC :
                    // téléchargement → décodage → traitement → encodage →
                    // session fichier (Content-Length, pas de chunked).
                    let bd = stream_data.quality.bit_depth.max(16).min(24);
                    let enc_format = streaming_pretranscode_format(renderer_supports_mime);
                    let enc_is_wav = enc_format == "wav";

                    info!(
                        service = service_name,
                        codec = %codec_lower,
                        device = %device_id,
                        sample_rate = sr,
                        bit_depth = bd,
                        enc_format,
                        dsp_active = https_dsp_active,
                        renderer_supports_mime,
                        "streaming_pretranscode_for_renderer_or_dsp"
                    );

                    let upstream_url = stream_data.url.clone();
                    let tmp_dl = std::env::temp_dir()
                        .join(format!(
                            "tune-stream-{}.{}",
                            uuid::Uuid::new_v4(),
                            codec_lower
                        ))
                        .to_string_lossy()
                        .to_string();
                    let tmp_wav = std::env::temp_dir()
                        .join(format!(
                            "tune-stream-pretranscode-{}.{}",
                            uuid::Uuid::new_v4(),
                            enc_format
                        ))
                        .to_string_lossy()
                        .to_string();

                    let tmp_dl_clone = tmp_dl.clone();
                    let tmp_wav_clone = tmp_wav.clone();
                    // VU-mètres : même ajout que les autres pré-transcodes —
                    // le PCM décodé alimente le forwarder de niveaux.
                    let wav_levels_tx = self.levels_forwarder_if_allowed(req.zone_id, 0).await;
                    let transcode_result = tokio::task::spawn_blocking(move || {
                        // 1. Download
                        let resp = crate::http::client::blocking_builder()
                            .timeout(std::time::Duration::from_secs(120))
                            .build()
                            .and_then(|c| c.get(&upstream_url).send())
                            .map_err(|e| format!("upstream fetch: {e}"))?;
                        if !resp.status().is_success() {
                            return Err(format!("upstream HTTP {}", resp.status()));
                        }
                        let bytes = resp.bytes().map_err(|e| format!("download: {e}"))?;
                        std::fs::write(&tmp_dl_clone, &bytes)
                            .map_err(|e| format!("write dl: {e}"))?;

                        // 2. Decode to PCM
                        let decoded = crate::audio::decode::decode_to_pcm(
                            &tmp_dl_clone,
                            Some(sr),
                            Some(2),
                            0.0,
                            0.0,
                        )?;
                        let mut pcm_bytes = decoded.pcm_bytes();
                        let mut actual_bd = decoded.bit_depth;
                        let actual_sr = decoded.sample_rate;
                        let actual_ch = decoded.channels;

                        // Plafond 16 bits : UNIQUEMENT quand on retombe sur
                        // WAV/LPCM. Le renderer a rejeté le FLAC, on sert donc
                        // sous `DLNA.ORG_PN=LPCM`, un profil 16 bits seulement —
                        // un Hi-Res 24 bits servi dessous joue du SILENCE sur un
                        // Ruark R3 / LHC-62 (Yves, #1137).
                        //
                        // Quand c'est le TRAITEMENT qui impose le
                        // pré-transcodage (#2863), le renderer sait lire le
                        // FLAC : on ré-encode en FLAC pleine profondeur, et le
                        // plafond n'a pas lieu d'être. Sans cette distinction,
                        // armer un égaliseur ferait tomber tout le Hi-Res Qobuz
                        // à 16 bits — une dégradation jamais demandée.
                        if enc_is_wav && actual_bd > 16 {
                            pcm_bytes =
                                crate::audio::decode::convert_pcm_bytes(&pcm_bytes, actual_bd, 16);
                            actual_bd = 16;
                        }

                        // ReplayGain, égaliseur, puis convolveur FIR — l'ordre
                        // de `transcode_source_to_file`. Sans traitement actif,
                        // `pcm_bytes` n'est pas touché d'un octet.
                        https_dsp.process(&mut pcm_bytes, actual_bd);

                        // Niveaux post-traitement : les VU décrivent ce qui sera
                        // entendu, comme sur le bras DASH.
                        if let Some(ref ltx) = wav_levels_tx {
                            crate::audio::tap::send_windowed_pcm(
                                ltx,
                                &pcm_bytes,
                                actual_bd,
                                actual_ch as u16,
                                actual_sr,
                            );
                        }

                        // 3. Encode
                        let rt = tokio::runtime::Handle::try_current()
                            .map_err(|e| format!("no tokio runtime: {e}"))?;
                        let encoded_data = rt.block_on(async {
                            let mut encoder = crate::audio::encoder::AudioEncoder::new(
                                enc_format,
                                actual_sr,
                                actual_bd as u32,
                                actual_ch,
                            );
                            encoder.start().await?;
                            encoder.write(&pcm_bytes).await?;
                            encoder.finish().await
                        })?;

                        std::fs::write(&tmp_wav_clone, &encoded_data)
                            .map_err(|e| format!("write pre-transcode: {e}"))?;

                        let _ = std::fs::remove_file(&tmp_dl_clone);
                        let file_size = encoded_data.len() as u64;
                        Ok::<(u64, u16, u32, u16), String>((
                            file_size,
                            actual_bd,
                            actual_sr,
                            actual_ch as u16,
                        ))
                    })
                    .await;

                    match transcode_result {
                        Ok(Ok((file_size, actual_bd, actual_sr, actual_ch))) => {
                            info!(
                                tmp = %tmp_wav,
                                file_size,
                                bit_depth = actual_bd,
                                sample_rate = actual_sr,
                                enc_format,
                                "streaming_pretranscode_complete"
                            );

                            let enc_mime = if enc_is_wav {
                                "audio/wav"
                            } else {
                                "audio/flac"
                            };
                            let file_info = StreamInfo {
                                format: enc_format.into(),
                                mime_type: enc_mime.into(),
                                sample_rate: actual_sr,
                                bit_depth: actual_bd,
                                channels: actual_ch,
                                file_size: Some(file_size),
                                duration_ms: None,
                                ..Default::default()
                            };
                            let session_id = self
                                .streamer
                                .create_file_session(file_info, tmp_wav, false)
                                .await;

                            let server_ip = self.server_ip();
                            let url =
                                self.streamer
                                    .get_stream_url(&session_id, &server_ip, enc_format);
                            (url, Some(session_id), enc_mime.to_string(), Some(file_size))
                        }
                        Ok(Err(e)) => {
                            warn!(error = %e, "streaming_pretranscode_failed");
                            let _ = std::fs::remove_file(&tmp_dl);
                            let _ = std::fs::remove_file(&tmp_wav);
                            return Err(format!("streaming pre-transcode failed: {e}"));
                        }
                        Err(e) => {
                            warn!(error = %e, "streaming_pretranscode_task_panic");
                            let _ = std::fs::remove_file(&tmp_dl);
                            let _ = std::fs::remove_file(&tmp_wav);
                            return Err(format!("streaming pre-transcode task panic: {e}"));
                        }
                    }
                } else {
                    // Renderer supports FLAC et AUCUN traitement actif — proxy
                    // direct, octets du CDN verbatim, bit-perfect. C'est le
                    // chemin de l'immense majorité des écoutes, et il ne bouge
                    // pas d'un octet.
                    //
                    // Qobuz/Tidal signed CDN URLs carry a short TTL (Qobuz
                    // `etsp=<unix-expiry>`, ~60 min). On a long Hi-Res track the
                    // URL expires mid-playback and a client Range-resume against
                    // the stored URL fails at the connection/auth level. Attach a
                    // re-resolver so the proxy layer can fetch a FRESH signed URL
                    // for the same track+quality and resume byte-exact (#1136).
                    // Only for real https CDN URLs (not file:// DASH assemblies).
                    let reresolve: Option<crate::http::streamer::ReresolveFn> = if is_https {
                        let services = self.services.clone();
                        let service_name = service_name.to_string();
                        let source_id = source_id.to_string();
                        Some(std::sync::Arc::new(move || {
                            let services = services.clone();
                            let service_name = service_name.clone();
                            let source_id = source_id.clone();
                            Box::pin(async move {
                                let registry = services.lock().await;
                                let svc = registry
                                    .get(&service_name)
                                    .ok_or_else(|| format!("unknown service: {service_name}"))?;
                                let mut svc = svc.write().await;
                                // Best-effort token refresh, then re-resolve with
                                // the same default quality the initial play used.
                                let _ = svc.refresh_if_needed().await;
                                match svc.get_track_url(&source_id, None).await {
                                    Ok(data) => Ok(data.url),
                                    Err(e) => Err(e.to_string()),
                                }
                            })
                                as std::pin::Pin<
                                    Box<
                                        dyn std::future::Future<Output = Result<String, String>>
                                            + Send,
                                    >,
                                >
                        }))
                    } else {
                        None
                    };
                    let session_id = self
                        .streamer
                        .create_proxy_session_with_reresolve(
                            info,
                            stream_data.url.clone(),
                            false,
                            reresolve,
                        )
                        .await;
                    let server_ip = self.server_ip();
                    let url = self
                        .streamer
                        .get_stream_url(&session_id, &server_ip, &codec_lower);

                    // VU-mètres (#1106) : le proxy sert les octets CDN
                    // verbatim, rien n'est décodé côté serveur → aucun
                    // `playback.audio_levels`, aiguilles figées sur Qobuz/
                    // Tidal direct alors qu'une piste locale les anime. On
                    // décode le même flux en parallèle, uniquement pour les
                    // niveaux — le flux servi reste bit-perfect.
                    //
                    // On tape NOTRE session proxy (`url`, localhost) et non
                    // l'URL CDN signée `stream_data.url` : le navigateur, en
                    // consommant le proxy, fait re-résoudre une URL signée
                    // fraîche, et l'ancienne signature tapée directement était
                    // rejetée par le CDN (aucune fenêtre décodée → aiguilles
                    // figées, la 1re version du fix #1247). Passer par le proxy
                    // réutilise sa re-résolution / reprise et sert exactement
                    // les octets joués. Le bridage ≤30 s en avance impose une
                    // contre-pression TCP : le proxy ne pré-télécharge pas
                    // toute la piste.
                    self.spawn_proxy_levels_probe(req.zone_id, url.clone(), codec_lower.clone())
                        .await;

                    // Report the mime of the codec we actually serve, not the
                    // upstream API's mime_type. Qobuz can return a mime that does
                    // not normalise to a lossless format, so Now Playing showed
                    // FLAC tracks as "compressé"/lossy (Progman). codec_lower is
                    // authoritative for what the proxy streams.
                    (url, Some(session_id), format!("audio/{codec_lower}"), None)
                }
            }
        } else {
            (
                stream_data.url.clone(),
                None,
                stream_data.mime_type.clone(),
                None,
            )
        };

        // Only trust the caller-supplied title when it is actually non-empty.
        // Repeat All (and some queue paths) re-play a streaming_queue row whose
        // stored title is "" — `req.title` is then Some("") and the old
        // `is_some()` check served that empty title verbatim, wiping Now Playing
        // (DEvir: `auto_next title=Shine...` followed by `orchestrator_play
        // title=`). Falling through to get_track() refetches the real metadata
        // from the service. The network call only fires when the title is
        // missing, so the happy path is unchanged.
        let has_title = req.title.as_deref().is_some_and(|s| !s.is_empty());
        let (title, artist, album, mut duration_ms, cover_path) = if has_title {
            (
                req.title.clone().unwrap_or_default(),
                req.artist_name.clone(),
                req.album_title.clone(),
                req.duration_ms,
                req.cover_url.clone(),
            )
        } else {
            match svc.get_track(source_id).await {
                Ok(track) => (
                    track.title,
                    Some(track.artist),
                    track.album,
                    Some(track.duration_ms as i64),
                    track.cover_path,
                ),
                Err(_) => (
                    req.title
                        .clone()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "Unknown".into()),
                    req.artist_name.clone(),
                    req.album_title.clone(),
                    req.duration_ms,
                    req.cover_url.clone(),
                ),
            }
        };

        // Duration backfill, mirroring serve_prefetched_pcm (#497): a non-empty
        // title with duration 0 skips the get_track branch above, and duration 0
        // on an EXCLUSIVE local output disarms the poller's position-past-end
        // advance (#483, which requires duration > 0) — on a Repeat All loop
        // transition the ring then starved at exactly one track length and
        // playback froze forever (DEvir, v0.9.14, ASIO, DASH file reused from
        // disk). The network call only fires in the degraded duration-0 case,
        // so the happy path is unchanged.
        if duration_ms.unwrap_or(0) == 0
            && let Ok(track) = svc.get_track(source_id).await
            && track.duration_ms > 0
        {
            duration_ms = Some(track.duration_ms as i64);
        }

        // Same contract as the radio branch: every path above may have replaced
        // the service's signed CDN URL with one of our own proxy or transcode
        // endpoints. Keep the upstream so an output that wants the bytes as the
        // service published them — a recorder keeping the original FLAC instead
        // of the proxy's re-stream or a WAV transcode — can ask for them. `None`
        // when we are handing out the upstream unchanged.
        let origin_url = (stream_url != stream_data.url).then(|| stream_data.url.clone());

        Ok(ResolvedStream {
            url: stream_url,
            mime_type: out_mime,
            title,
            artist,
            album,
            duration_ms,
            source: service_name.into(),
            cover_url: cover_path,
            stream_id: sid,
            file_size: stream_file_size,
            sample_rate: Some(stream_data.quality.sample_rate),
            bit_depth: Some(stream_data.quality.bit_depth as u32),
            channels: Some(2),
            origin_url,
            bitrate_kbps: None,
        })
    }
}
