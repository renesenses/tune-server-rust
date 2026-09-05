use super::*;

/// Ce qu'une URL directe rend au demandeur : URL à jouer, session éventuelle,
/// type MIME, fréquence, profondeur, canaux.
type FluxDirect = (
    String,
    Option<String>,
    String,
    Option<u32>,
    Option<u32>,
    Option<u32>,
);

/// Ce que la demande dit d'une URL directe, relevé une fois avant d'aiguiller
/// entre les sorties : la source résolue, ce qu'on en sait, et la nature de
/// la sortie. Copiable : chaque temps en relit ce qu'il lui faut.
#[derive(Clone, Copy)]
struct Directe<'a> {
    audio_url: &'a str,
    title: &'a String,
    mime_type: &'a str,
    duration_ms: Option<i64>,
    bc_quality: &'a Option<super::bandcamp::BandcampQuality>,
    is_local_output: bool,
    is_browser_output: bool,
    radio_eq_profile: &'a Option<crate::audio::eq::EqProfile>,
}

impl PlaybackOrchestrator {
    /// Check whether a DLNA renderer supports a given MIME type by querying
    /// its ConnectionManager GetProtocolInfo Sink.  Results are cached per
    /// device_id so the SOAP call only happens once per renderer per session.
    pub(super) async fn dlna_supports_mime(&self, device_id: &str, mime: &str) -> bool {
        // Check negative cache first
        {
            let cache = self.dlna_unsupported_mimes.lock().await;
            if let Some(unsupported) = cache.get(device_id) {
                if unsupported.iter().any(|m| m == mime) {
                    return false;
                }
                // We already probed this device — if the MIME is not in the
                // unsupported list, it means it was supported.
                if !unsupported.is_empty() {
                    // Device was probed at least once (it returned some
                    // unsupported entries or we stored an empty vec for it).
                    // But we can't distinguish "probed and supported" from
                    // "never checked this mime".  So we only use the cache
                    // for known negatives and re-probe below if needed.
                }
            }
        }

        // Probe the renderer. None = inconclusive probe (SOAP failed / empty
        // Sink) — fall back conservatively but do NOT cache, so one transient
        // failure doesn't force WAV for the whole session (Marco's Denon).
        let probe = {
            let arc = { self.outputs.lock().await.get(device_id) };
            if let Some(output) = arc {
                let locked = output.lock().await;
                if let Some(dlna) = locked
                    .as_any()
                    .downcast_ref::<crate::outputs::dlna::DlnaOutput>()
                {
                    dlna.supports_mime(mime).await
                } else {
                    // Not a DLNA output — format negotiation doesn't apply
                    Some(true)
                }
            } else {
                Some(true)
            }
        };

        match probe {
            Some(true) => true,
            Some(false) => {
                // Renderer's Sink was read and genuinely lacks this MIME — cache.
                let mut cache = self.dlna_unsupported_mimes.lock().await;
                let entry = cache.entry(device_id.to_string()).or_default();
                if !entry.iter().any(|m| m == mime) {
                    entry.push(mime.to_string());
                }
                false
            }
            None => {
                // Inconclusive — universal formats assumed OK, others not, but
                // not cached so the next play re-probes.
                matches!(
                    mime.to_lowercase().as_str(),
                    "audio/wav" | "audio/x-wav" | "audio/l16" | "audio/mpeg"
                )
            }
        }
    }

    pub(super) async fn resolve_uploaded_file(
        &self,
        file_path: &str,
        req: &PlayRequest,
    ) -> Result<ResolvedStream, String> {
        let path = std::path::Path::new(file_path);
        if !path.exists() {
            return Err(format!("uploaded file not found: {file_path}"));
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("wav")
            .to_lowercase();
        let format = crate::audio::formats::AudioFormat::from_extension(&ext);
        let meta = crate::metadata::try_read_metadata(path);
        let title = req
            .title
            .clone()
            .or_else(|| meta.as_ref().ok().and_then(|m| m.title.clone()))
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or("Unknown")
                    .to_string()
            });
        let artist = req
            .artist_name
            .clone()
            .or_else(|| meta.as_ref().ok().and_then(|m| m.artist.clone()));
        let album = req
            .album_title
            .clone()
            .or_else(|| meta.as_ref().ok().and_then(|m| m.album.clone()));
        let duration_ms = req
            .duration_ms
            .map(|d| d as u64)
            .or_else(|| meta.as_ref().ok().and_then(|m| m.duration_ms))
            .unwrap_or(0);
        let sample_rate = meta.as_ref().ok().and_then(|m| m.sample_rate);
        let bit_depth = meta.as_ref().ok().and_then(|m| m.bit_depth);
        let channels = meta.as_ref().ok().and_then(|m| m.channels).unwrap_or(2);

        let mime = format
            .as_ref()
            .map(|f| f.mime_type())
            .unwrap_or("audio/wav")
            .to_string();
        let file_size = std::fs::metadata(path).ok().map(|m| m.len());

        let info = StreamInfo {
            format: ext.clone(),
            mime_type: mime.clone(),
            sample_rate: sample_rate.unwrap_or(44100) as u32,
            bit_depth: bit_depth.unwrap_or(16),
            channels: channels as u16,
            file_size,
            duration_ms: Some(duration_ms as u64),
            ..Default::default()
        };

        let (session_id, tx, data_ready) = self.streamer.create_session(info, true, 128).await;
        let fp = file_path.to_string();
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            let file = std::fs::read(&fp);
            match file {
                Ok(data) => {
                    let _ = rt.block_on(tx.send(data));
                    data_ready.notify_one();
                }
                Err(e) => {
                    tracing::warn!(error = %e, "uploaded_file_read_failed");
                }
            }
        });

        let server_ip = self.server_ip();
        let stream_url = self.streamer.get_stream_url(&session_id, &server_ip, &ext);

        Ok(ResolvedStream {
            url: stream_url,
            stream_id: Some(session_id),
            title,
            artist,
            album,
            duration_ms: Some(duration_ms as i64),
            source: "upload".into(),
            mime_type: mime,
            sample_rate: sample_rate.map(|s| s as u32),
            bit_depth: bit_depth.map(|b| b as u32),
            channels: Some(channels as u32),
            origin_url: None,
            bitrate_kbps: None,
            cover_url: None,
            file_size,
        })
    }

    pub(super) async fn resolve_direct_url(
        &self,
        req: &PlayRequest,
    ) -> Result<ResolvedStream, String> {
        let raw_url = req
            .source_id
            .as_deref()
            .ok_or("source_id (audio URL) required for podcast/radio playback")?;
        // A station is often published as an .m3u/.pls PLAYLIST file rather than a
        // direct stream. Dereference it to the real stream first, otherwise the
        // decoder is fed the playlist text and no sound plays (Pascal). Cheap for
        // a direct URL (extension gate, no network hit); keeps `raw_url` on any
        // failure. Applies to every downstream radio path (local and network).
        let resolved_playlist = self.resolve_playlist_url(raw_url).await;
        let audio_url: &str = resolved_playlist.as_deref().unwrap_or(raw_url);
        let title = req.title.clone().unwrap_or_else(|| "Episode".into());
        let artist = req.artist_name.clone();
        let album = req.album_title.clone();
        let cover_url = req.cover_url.clone();
        let duration_ms = req.duration_ms;
        let source = req.source.clone().unwrap_or_else(|| "podcast".into());
        // La qualité Bandcamp est LUE DANS L'URL, jamais déduite du nom du
        // service. L'écoute libre est du `mp3-128` ; un fichier ACHETÉ entre
        // par la même porte en `flac`, `alac` ou `mp3-320`, et l'étiqueter
        // « MP3 128 » serait un mensonge dans le sens le plus coûteux pour ce
        // logiciel (#2074). `None` quand l'URL ne nomme rien : on retombe
        // alors sur ce que Bandcamp sert sans session, sans rien affirmer de
        // plus.
        let bc_quality = (source == "bandcamp")
            .then(|| bandcamp_encoding(audio_url))
            .flatten()
            .and_then(|enc| bandcamp_quality(&enc));
        // Les URL de flux Bandcamp (`t4.bcbits.com/stream/<hash>/mp3-128/<id>`)
        // n'ont pas d'extension : `guess_mime_from_url` retomberait sur son
        // défaut, qui se trouve être le bon. On l'affirme plutôt que d'en
        // dépendre — si ce défaut changeait, la zone recevrait un MIME faux.
        let mime_type = if source == "bandcamp" {
            bc_quality
                .as_ref()
                .map(|q| q.mime_type)
                .unwrap_or("audio/mpeg")
        } else {
            guess_mime_from_url(audio_url)
        };
        let is_radio = source == "radio";
        let is_bandcamp = source == "bandcamp";

        let is_local_output = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("local:"));
        let is_oaat_output = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("oaat:") || id.starts_with("oaat-group:"));
        // Une zone navigateur n'a volontairement aucun `output_device_id` :
        // l'onglet est la sortie et tire lui-même `stream_url`. On doit donc
        // lire son type en base plutôt que déduire « aucune sortie » de
        // l'absence de périphérique (#2076, #2158). Cette propriété vaut pour
        // Bandcamp comme pour la radio dont l'EQ force désormais le proxy WAV.
        let is_browser_output = req.output_device_id.is_none()
            && ZoneRepo::with_backend(self.db.clone())
                .get(req.zone_id)
                .ok()
                .flatten()
                .and_then(|zone| zone.output_type)
                .as_deref()
                == Some("browser");

        // La sortie locale applique déjà l'EQ dans son callback : le refaire
        // ici colorerait le signal deux fois. OAAT, DLNA et navigateur
        // consomment en revanche le WAV construit par ce décodeur ; le profil
        // doit voyager jusqu'au moment où son format réel sera connu (#2063).
        // Un profil neutre ne force aucun transcodage inutile.
        let radio_eq_profile = if is_radio
            && !is_local_output
            && (is_oaat_output || req.output_device_id.is_some() || is_browser_output)
        {
            self.load_eq_profile(req.zone_id).filter(|profile| {
                crate::audio::eq::EqProcessor::new(profile, 44_100, 2).is_enabled()
            })
        } else {
            None
        };

        let d = Directe {
            audio_url,
            title: &title,
            mime_type,
            duration_ms,
            bc_quality: &bc_quality,
            is_local_output,
            is_browser_output,
            radio_eq_profile: &radio_eq_profile,
        };
        let (url, stream_id, out_mime, out_sr, out_bd, out_ch) =
            if is_radio && (is_local_output || is_oaat_output) {
                self.decoder_la_radio_en_wav(req, d).await
            } else if is_bandcamp && is_oaat_output {
                self.decoder_bandcamp_en_wav(req, d).await
            } else if is_bandcamp
                && !is_local_output
                && (req.output_device_id.is_some() || is_browser_output)
            {
                self.relayer_bandcamp_au_reseau(d).await
            } else if is_radio {
                self.servir_la_radio_au_reseau(req, d).await
            } else if is_bandcamp {
                // Sortie LOCALE (ou aucune sortie encore liée). `LocalOutput`
                // télécharge et décode lui-même un flux HTTP compressé
                // (`local_audio_non_wav_stream_detected_decoding`) : rien à
                // interposer, et un transcodage ne ferait que dégrader deux fois.
                //
                // La résolution est AFFIRMÉE plutôt que laissée au défaut
                // (44,1 kHz / 16 bits est ce que le mp3-128 de Bandcamp décode) :
                // le chemin du signal doit annoncer « MP3 — Avec perte », et non
                // hériter d'une valeur par défaut qu'on n'aurait pas choisie.
                (
                    audio_url.to_string(),
                    None,
                    mime_type.to_string(),
                    Some(44100u32),
                    Some(16u32),
                    Some(2u32),
                )
            } else {
                // Media-server / podcast direct URL. Carry the real resolution the
                // client passed from the DIDL res@ attributes (e.g. 24-bit ALAC)
                // instead of letting the signal path default to 44.1kHz/16bit and
                // mislabel a hi-res ALAC as lossy AAC (Yves, NAS).
                (
                    audio_url.to_string(),
                    None,
                    mime_type.to_string(),
                    req.sample_rate,
                    req.bit_depth.map(|b| b as u32),
                    None,
                )
            };

        // Every branch above may have replaced the station/enclosure URL with one
        // of our proxy endpoints (WAV transcode for renderers that need it, or a
        // local decode session). Keep the original so an output that wants the
        // bytes as published — and the ICY metadata the proxy drops — can ask
        // for them. `None` when we are handing out the upstream URL unchanged.
        let origin_url = (url != audio_url).then(|| audio_url.to_string());

        Ok(ResolvedStream {
            url,
            mime_type: out_mime,
            title,
            artist,
            album,
            duration_ms,
            source,
            cover_url,
            stream_id,
            file_size: None,
            sample_rate: out_sr,
            bit_depth: out_bd,
            channels: out_ch,
            origin_url,
            // Le débit voyage jusqu'à la zone quelle que soit la sortie prise
            // ci-dessus — locale, WAV décodé pour OAAT, ou proxy MP3 pour un
            // renderer réseau : les trois portent le MÊME flux source, et
            // c'est LUI que le chemin du signal doit annoncer (#2074).
            bitrate_kbps: bc_quality.as_ref().and_then(|q| q.bitrate_kbps),
        })
    }

    /// Radio vers une sortie locale ou OAAT : ces sorties ne lisent pas un flux
    /// compressé, on décode la station en WAV dans une session-canal (avec
    /// l'égaliseur de zone s'il est actif).
    async fn decoder_la_radio_en_wav(&self, req: &PlayRequest, d: Directe<'_>) -> FluxDirect {
        let Directe {
            audio_url,
            title,
            is_local_output,
            ..
        } = d;
        let radio_eq_profile = d.radio_eq_profile.clone();
        // Local/OAAT outputs cannot play compressed streams directly —
        // they expect raw PCM in a WAV container.  For radio (infinite
        // stream), we decode the HTTP stream progressively to PCM and
        // serve it as WAV through a streaming session.
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

        let (session_id, tx, data_ready, session) =
            self.streamer.create_radio_session(wav_info, 256).await;

        info!(
            source = "radio",
            url = %audio_url,
            "radio_decode_to_wav_for_local_output"
        );

        let radio_url = audio_url.to_string();
        // VU-mètres sur radio : forwarder de niveaux alimenté par le PCM
        // décodé du flux (le décodage-pour-niveaux fichier ne s'applique
        // pas à un live). Observateur pur, n'affecte pas le flux servi.
        let radio_levels_tx = if let Some(ref bus) = self.event_bus {
            let play_seq = self.playback.current_play_seq(req.zone_id).await;
            Some(spawn_paced_levels_forwarder(
                bus.clone(),
                self.playback.clone(),
                req.zone_id,
                play_seq,
                0,
            ))
        } else {
            None
        };
        // Clone kept OUTSIDE the decode task: several of its exit paths
        // (consumer dropped, reconnect give-up) only log at debug!, so in
        // production the producer can die invisibly. The flag lets
        // resume() detect that state and re-play the station (#1629).
        let session_for_done = session.clone();
        // De quoi DIRE l'échec plutôt que de le laisser au journal.
        let err_bus = self.event_bus.clone();
        let err_zone = req.zone_id;
        let err_station = title.clone();
        tokio::spawn(async move {
            // Download + decode in a blocking thread since symphonia and
            // reqwest::blocking are both synchronous.
            let result = tokio::task::spawn_blocking(move || {
                decode_radio_stream_to_pcm(
                    radio_url,
                    tx,
                    data_ready,
                    session,
                    if is_local_output {
                        None
                    } else {
                        radio_eq_profile.clone()
                    },
                    radio_levels_tx,
                )
            })
            .await;

            // Whatever the exit path — clean end, error or panic — nothing
            // will produce PCM for this session anymore.
            session_for_done
                .producer_done
                .store(true, std::sync::atomic::Ordering::Relaxed);

            match result {
                Ok(Ok(())) => {
                    debug!("radio_local_decode_stream_ended");
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "radio_local_decode_failed");
                    emit_radio_playback_error(&err_bus, err_zone, &err_station, &e);
                }
                Err(e) => {
                    warn!(error = %e, "radio_local_decode_task_panic");
                    emit_radio_playback_error(
                        &err_bus,
                        err_zone,
                        &err_station,
                        "erreur interne du décodeur",
                    );
                }
            }
        });

        let server_ip = self.server_ip();
        let stream_url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");
        (
            stream_url,
            Some(session_id),
            "audio/wav".to_string(),
            Some(44100u32),
            Some(16u32),
            Some(2u32),
        )
    }

    /// Bandcamp vers une sortie OAAT : même décodage en WAV, sans égaliseur.
    async fn decoder_bandcamp_en_wav(&self, req: &PlayRequest, d: Directe<'_>) -> FluxDirect {
        let Directe { audio_url, .. } = d;
        // Un endpoint OAAT ne consomme que du PCM en conteneur WAV : son
        // chemin HTTP le dit noir sur blanc (« Compressed formats fall
        // through to HTTP streaming where the orchestrator already decoded
        // them to WAV »). Lui pousser le mp3-128 de Bandcamp tel quel
        // donnerait un flux qu'il ne sait pas ouvrir — c'est-à-dire le
        // silence, exactement ce qu'on corrige.
        //
        // On réutilise la MÊME session de décodage que la radio sur OAAT,
        // qui tourne en production sur .18 : `decode_radio_stream_to_pcm`
        // décode un flux HTTP au fil de l'eau et se termine proprement à
        // la fin des octets — une piste finie n'est qu'un flux qui
        // s'arrête. Aucun chemin existant n'est modifié : la branche est
        // fermée sur `source == "bandcamp"`.
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
        let (session_id, tx, data_ready, session) =
            self.streamer.create_radio_session(wav_info, 256).await;
        info!(url = %audio_url, "bandcamp_decode_to_wav_for_oaat_output");
        let bc_url = audio_url.to_string();
        let bc_levels_tx = if let Some(ref bus) = self.event_bus {
            let play_seq = self.playback.current_play_seq(req.zone_id).await;
            Some(spawn_paced_levels_forwarder(
                bus.clone(),
                self.playback.clone(),
                req.zone_id,
                play_seq,
                0,
            ))
        } else {
            None
        };
        let session_for_done = session.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                decode_radio_stream_to_pcm(bc_url, tx, data_ready, session, None, bc_levels_tx)
            })
            .await;
            session_for_done
                .producer_done
                .store(true, std::sync::atomic::Ordering::Relaxed);
            match result {
                Ok(Ok(())) => debug!("bandcamp_oaat_decode_stream_ended"),
                Ok(Err(e)) => warn!(error = %e, "bandcamp_oaat_decode_failed"),
                Err(e) => warn!(error = %e, "bandcamp_oaat_decode_task_panic"),
            }
        });
        let server_ip = self.server_ip();
        let stream_url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");
        (
            stream_url,
            Some(session_id),
            "audio/wav".to_string(),
            Some(44100u32),
            Some(16u32),
            Some(2u32),
        )
    }

    /// Bandcamp vers un renderer réseau ou le navigateur : relais HTTP du flux
    /// HTTPS par une session mandataire, le codec annoncé venant de l'URL.
    async fn relayer_bandcamp_au_reseau(&self, d: Directe<'_>) -> FluxDirect {
        let Directe {
            audio_url,
            mime_type,
            duration_ms,
            bc_quality,
            is_browser_output,
            ..
        } = d;
        // Sortie RÉSEAU (DLNA/OpenHome) ou navigateur. Bandcamp ne publie
        // ses flux qu'en HTTPS : un renderer DLNA ne sait pas ouvrir TLS,
        // tandis que le client web réécrit une URL tierce en chemin local
        // et reçoit alors du text/html au lieu du MP3 (#2076, #2158).
        //
        // On la sert donc par une session proxy locale, en clair, comme
        // les pistes Tidal/Qobuz (`create_proxy_session`). Les octets
        // passent verbatim : c'est du MP3 que tout renderer sait lire, il
        // n'y a rien à transcoder.
        //
        // Conteneur et MIME suivent l'encodage LU DANS L'URL, avec repli
        // sur le `mp3` de l'écoute libre : le proxy passe les octets tels
        // quels, donc annoncer `audio/mpeg` sur un FLAC acheté ferait
        // exactement le mislabel dont ce chemin se protège (#2074).
        let bc_codec = bc_quality.as_ref().map(|q| q.codec).unwrap_or("mp3");
        let info = StreamInfo {
            format: bc_codec.into(),
            mime_type: mime_type.to_string(),
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            file_size: None,
            duration_ms: duration_ms.map(|d| d as u64),
            ..Default::default()
        };
        let session_id = self
            .streamer
            .create_proxy_session(info, audio_url.to_string(), false)
            .await;
        let server_ip = self.server_ip();
        let stream_url = self
            .streamer
            .get_stream_url(&session_id, &server_ip, bc_codec);
        info!(
            url = %audio_url,
            browser = is_browser_output,
            codec = bc_codec,
            "bandcamp_proxy_for_network_or_browser_output"
        );
        (
            stream_url,
            Some(session_id),
            mime_type.to_string(),
            Some(44100u32),
            Some(16u32),
            Some(2u32),
        )
    }

    /// Radio vers un renderer réseau : transcodage WAV par mandataire quand le
    /// renderer ou l'égaliseur l'exige, sinon l'URL de la station, en HTTP.
    async fn servir_la_radio_au_reseau(&self, req: &PlayRequest, d: Directe<'_>) -> FluxDirect {
        let Directe {
            audio_url,
            title,
            mime_type,
            is_browser_output,
            ..
        } = d;
        let radio_eq_profile = d.radio_eq_profile.clone();
        // Network outputs (DLNA): check if the renderer supports the
        // radio stream format (typically AAC). If not, proxy + transcode
        // to WAV so the renderer can play it.
        // Passthrough ONLY when the URL carries an unambiguous,
        // renderer-supported extension (.mp3/.flac/.wav). Extension-less
        // Icecast mounts fall through guess_mime_from_url() to the default
        // "audio/mpeg", and .aac (ADTS) maps to "audio/mp4" — both are
        // mislabels. The renderer then opens a stream whose bytes don't
        // match the advertised protocolInfo, reports PLAYING and emits
        // SILENCE (Cyrille, Yamaha R-N2000A). Transcode every ambiguous
        // codec (.aac/.ogg/.opus/HLS/extension-less) to WAV so sound is
        // guaranteed; explicit .mp3/.flac stations still pass through with
        // no CPU/bandwidth cost.
        let url_path = audio_url.split(['?', '#']).next().unwrap_or(audio_url);
        let reliable_ext = {
            let p = url_path.to_lowercase();
            p.ends_with(".mp3") || p.ends_with(".flac") || p.ends_with(".wav")
        };
        // A radio stream bound to a specific DLNA renderer is ALWAYS
        // proxied+transcoded to WAV. Direct passthrough of an infinite
        // Icecast stream is unreliable: it carries no Content-Length and
        // may use ICY framing, so the renderer HEAD-probes, reports
        // PLAYING, then emits silence — even for an explicit .mp3 whose
        // HEAD returns 200 (Cyrille, Yamaha R-N2000A: Radio Classique
        // proxied → sound, TSF Jazz sent direct → silent + retry loop).
        // WAV is universally supported, so proxying guarantees sound at
        // low CPU/LAN cost. Only device-less network resolves (no HEAD to
        // gamble on) keep the extension-based passthrough.
        // Un EQ actif interdit le passthrough, même pour un MP3 explicite :
        // les octets compressés contourneraient entièrement le DSP. C'est
        // notamment le cas d'une zone navigateur, qui n'a aucun device_id
        // mais doit recevoir le WAV déjà égalisé par Tune (#2063).
        //
        // Une zone NAVIGATEUR n'a jamais droit au passthrough, EQ ou pas
        // (#2670). Le client web reecrit toute URL absolue en chemin
        // relatif — `browserPlay`, `u.pathname + u.search`, pour joindre
        // l'hote Tune plutot que l'IP annoncee par le serveur. Lui rendre
        // l'URL de la station fait donc demander `/tsfjazz-high.mp3` a
        // Tune, qui repond par son repli SPA : 200 `text/html`, sa propre
        // page. L'auditeur recoit une page web a la place du flux, et Tune
        // n'a rien a en dire puisqu'il n'a jamais ouvert le flux lui-meme :
        // le controle `non_audio_content_type` vit dans
        // `decode_radio_stream_to_pcm`, que ce chemin court-circuite.
        // C'est la MEME cause que #2076 / #2158, deja corrigee pour
        // Bandcamp quelques branches plus haut par un proxy local.
        //
        // La bascule ne coute rien de nouveau : une zone navigateur recoit
        // deja du WAV pour toute station au codec ambigu (.aac, .ogg, sans
        // extension), soit 44 des 51 entrees de l'annuaire au 28/08/2026.
        // Seules les rares URL en .mp3/.flac/.wav prenaient ce raccourci —
        // TSF Jazz en fait partie, et c'est la station signalee.
        let needs_proxy = req.output_device_id.is_some()
            || is_browser_output
            || !reliable_ext
            || radio_eq_profile.is_some();

        if needs_proxy {
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
            let (session_id, tx, data_ready, session) =
                self.streamer.create_radio_session(wav_info, 256).await;
            info!(url = %audio_url, "radio_proxy_transcode_for_dlna");
            let radio_url = audio_url.to_string();
            // VU-mètres sur radio (DLNA) : forwarder de niveaux alimenté
            // par le PCM décodé. Observateur pur, n'affecte pas le flux.
            let radio_levels_tx = if let Some(ref bus) = self.event_bus {
                let play_seq = self.playback.current_play_seq(req.zone_id).await;
                Some(spawn_paced_levels_forwarder(
                    bus.clone(),
                    self.playback.clone(),
                    req.zone_id,
                    play_seq,
                    0,
                ))
            } else {
                None
            };
            // Même marquage que le chemin local/OAAT : resume() lit ce
            // drapeau pour savoir que plus rien n'alimente la session et
            // rejouer la station (#1629).
            let session_for_done = session.clone();
            // Même dette que le chemin local : l'échec restait au journal.
            let err_bus = self.event_bus.clone();
            let err_zone = req.zone_id;
            let err_station = title.clone();
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    decode_radio_stream_to_pcm(
                        radio_url,
                        tx,
                        data_ready,
                        session,
                        radio_eq_profile.clone(),
                        radio_levels_tx,
                    )
                })
                .await;
                session_for_done
                    .producer_done
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                match result {
                    Ok(Ok(())) => debug!("radio_dlna_decode_stream_ended"),
                    Ok(Err(e)) => {
                        warn!(error = %e, "radio_dlna_decode_failed");
                        emit_radio_playback_error(&err_bus, err_zone, &err_station, &e);
                    }
                    Err(e) => {
                        warn!(error = %e, "radio_dlna_decode_task_panic");
                        emit_radio_playback_error(
                            &err_bus,
                            err_zone,
                            &err_station,
                            "erreur interne du décodeur",
                        );
                    }
                }
            });
            let server_ip = self.server_ip();
            let stream_url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");
            (
                stream_url,
                Some(session_id),
                "audio/wav".to_string(),
                Some(44100u32),
                Some(16u32),
                Some(2u32),
            )
        } else {
            // Renderer supports the format — send direct URL.
            // Downgrade https→http since DLNA renderers can't do TLS.
            let direct_url = if audio_url.starts_with("https://") {
                audio_url.replacen("https://", "http://", 1)
            } else {
                audio_url.to_string()
            };
            (direct_url, None, mime_type.to_string(), None, None, None)
        }
    }
}
