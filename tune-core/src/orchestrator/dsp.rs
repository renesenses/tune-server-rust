use super::*;

/// Ce que `serve_prefetched_pcm` annonce de la piste : relevé une fois, lu
/// par la sortie réseau comme par la sortie locale.
struct Etiquettes {
    title: String,
    artist: Option<String>,
    album: Option<String>,
    cover_url: Option<String>,
    duration_ms: u64,
}

/// Quand un réglage d'égaliseur atteint le son (#4680).
///
/// Le client en tire sa phrase : seul [`Self::PisteSuivante`] justifie
/// « prendra effet à la piste suivante ». [`Self::Relance`] s'entend dans
/// l'instant — le flux est relancé à la position courante après
/// l'anti-rebond (`EQ_REPLAY_DEBOUNCE_MS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PorteeDuReglage {
    /// Le son est déjà conforme : sortie locale modifiée à chaud, ou flux
    /// réseau qui porte déjà ce traitement.
    Immediate,
    /// Zone réseau : le flux va être relancé à la position courante.
    Relance,
    /// Rien ne peut agir avant la piste suivante (position inconnue, #2595).
    PisteSuivante,
    /// Rien ne joue : la prochaine lecture partira avec ce réglage.
    RienNeJoue,
}

impl PorteeDuReglage {
    /// Le mot publié dans les réponses (`portee`).
    pub fn code(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::Relance => "restart",
            Self::PisteSuivante => "next_track",
            Self::RienNeJoue => "not_playing",
        }
    }

    /// Le `chemin` de `eq_change_journal` dit déjà ce qui s'est passé : la
    /// portée s'en déduit, pour qu'elle ne puisse pas diverger du journal.
    fn du_chemin(chemin: &str) -> Self {
        match chemin {
            "local_a_chaud" | "flux_conserve_signal_identique" => Self::Immediate,
            "replay_programme" => Self::Relance,
            "rien_ne_joue" => Self::RienNeJoue,
            _ => Self::PisteSuivante,
        }
    }
}

/// Pourquoi l'égaliseur n'a pas pu être remplacé À CHAUD sur une sortie locale.
///
/// #3479 — « l'activation de l'égaliseur coupe le son » (Reivax66, 0.9.138,
/// Windows). Les six sorties anticipées de
/// [`PlaybackOrchestrator::refresh_zone_eq`] rendaient toutes le MÊME `false`,
/// sans une ligne de journal. Le serveur ne disait donc nulle part si le
/// réglage avait atteint la sortie locale, s'il était parti en redémarrage de
/// flux réseau, ou s'il n'avait rien atteint du tout — et un rapport de
/// diagnostic joint à un fil du forum ne permettait pas de trancher entre la
/// famille « zone RÉSEAU » (#3357) et la famille « sortie LOCALE » (#1735),
/// c'est-à-dire de savoir quel code regarder.
///
/// Chaque motif porte son nom, et ce nom part dans `eq_change_journal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// `CaisseSansAudioLocal` n'est construite que SANS `local-audio`, les cinq
// autres seulement AVEC : aucune configuration ne les construit toutes.
#[allow(dead_code)]
pub(crate) enum EchecEqLocal {
    /// Caisse compilée sans `local-audio` : il n'existe aucune sortie locale.
    CaisseSansAudioLocal,
    /// La zone n'a pas de périphérique de sortie, ou sa ligne est absente.
    ZoneSansPeripherique,
    /// La zone joue ailleurs que sur la machine — DLNA, navigateur, OAAT…
    SortieNonLocale,
    /// `local:…`, mais ce périphérique n'est pas au registre des sorties.
    SortieAbsenteDuRegistre,
    /// Le registre a bien cette clé, mais ce n'est pas une `LocalOutput`.
    SortiePasUneSortieLocale,
    /// Rien ne joue : aucun couple (taux, canaux) n'a encore été vu, et bâtir
    /// des biquads pour un format inconnu donnerait des coefficients faux.
    FormatInconnu,
}

impl EchecEqLocal {
    /// Le mot qui part dans le journal. Stable : c'est lui qu'on cherchera
    /// dans un export de diagnostic.
    pub(crate) fn nom(self) -> &'static str {
        match self {
            Self::CaisseSansAudioLocal => "caisse_sans_audio_local",
            Self::ZoneSansPeripherique => "zone_sans_peripherique",
            Self::SortieNonLocale => "sortie_non_locale",
            Self::SortieAbsenteDuRegistre => "sortie_absente_du_registre",
            Self::SortiePasUneSortieLocale => "sortie_pas_une_sortie_locale",
            Self::FormatInconnu => "format_inconnu",
        }
    }
}

/// Ce qu'un changement d'égaliseur a RÉELLEMENT fait sur la sortie locale.
#[derive(Debug, Clone)]
pub(crate) struct RapportEqAChaud {
    /// Le périphérique de la zone, vide si elle n'en a pas.
    pub(crate) device_id: String,
    /// `None` quand le contrat a bien été posé sur une sortie locale vivante.
    pub(crate) echec: Option<EchecEqLocal>,
    /// Le couple (taux, canaux) du flux en cours — le format AVANT traitement,
    /// et aussi celui d'APRÈS : un égaliseur ne change ni la cadence ni le
    /// nombre de canaux. Ce qu'il change est le NIVEAU, d'où `preamp_db`.
    pub(crate) format: Option<(u32, u16)>,
    /// Un `EqProcessor` audible a-t-il été monté ? `false` couvre aussi bien le
    /// retrait volontaire (PURE, profil désactivé) qu'un profil dont toutes les
    /// bandes sont neutres.
    pub(crate) eq_actif: bool,
    /// Le pré-gain automatique, en dB, canal gauche puis droit.
    ///
    /// C'est la marge que l'égaliseur RETIRE au signal pour ne pas écrêter
    /// (`EqProfile::automatic_headroom_db`, qui réserve la norme L1 de la
    /// cascade — la borne vraie — plus la résonance des `low_pass` /
    /// `high_pass` ; #4073 puis #4594). Sur un profil qui pousse beaucoup,
    /// c'est de lui que vient une chute de niveau — mesurée jusqu'à −12,4 dB
    /// sur un profil AutoEq réel (`tune-core/tests/autoeq_profils_reels.rs`,
    /// où elle valait −22,2 dB avant #4594) — et il ne se lisait jusqu'ici
    /// dans aucun journal.
    pub(crate) preamp_db: Option<f64>,
    pub(crate) preamp_db_droite: Option<f64>,
}

impl RapportEqAChaud {
    fn echoue(device_id: String, echec: EchecEqLocal) -> Self {
        Self {
            device_id,
            echec: Some(echec),
            format: None,
            eq_actif: false,
            preamp_db: None,
            preamp_db_droite: None,
        }
    }

    /// Le contrat a-t-il atteint une sortie locale vivante ? C'est exactement
    /// ce que [`PlaybackOrchestrator::refresh_zone_eq`] rendait déjà.
    pub(crate) fn applique(&self) -> bool {
        self.echec.is_none()
    }

    /// Le format, écrit pour un humain. `-` quand rien ne joue.
    pub(crate) fn format_ecrit(&self) -> String {
        match self.format {
            Some((taux, canaux)) => format!("{taux} Hz / {canaux} canaux / f32"),
            None => "-".to_string(),
        }
    }
}

/// La famille de sortie d'une zone, telle que le journal doit la NOMMER.
///
/// Fonction PURE : elle ne lit que ce que la ligne de zone porte déjà. Elle
/// existe pour que « locale » et « réseau » soient un MOT dans le journal, et
/// non une déduction à refaire après coup sur un identifiant de périphérique —
/// c'est précisément cette déduction qui manquait pour instruire #3479.
pub(crate) fn famille_de_sortie(device_id: &str, output_type: Option<&str>) -> &'static str {
    if device_id.is_empty() && output_type.is_none() {
        return "absente";
    }
    if device_id.starts_with("local:") || output_type == Some("local") {
        return "locale";
    }
    if device_id.starts_with("oaat:") || device_id.starts_with("oaat-group:") {
        return "oaat";
    }
    if output_type == Some("browser") {
        return "navigateur";
    }
    if is_network_output_type(output_type) {
        return "reseau";
    }
    if is_pull_dsp_output_type(output_type) {
        return "pull";
    }
    "inconnue"
}

impl PlaybackOrchestrator {
    /// Marque la zone en résolution gapless jusqu'au drop du garde.
    pub(super) fn begin_levels_prewarm(&self, zone_id: i64) -> LevelsPrewarmScope<'_> {
        self.levels_prewarm
            .lock()
            .expect("levels_prewarm lock")
            .insert(zone_id);
        LevelsPrewarmScope {
            set: &self.levels_prewarm,
            zone_id,
        }
    }

    /// Faut-il attacher un forwarder de niveaux aux sessions de cette zone ?
    /// Non pendant une résolution gapless (pré-chargement de la piste
    /// suivante).
    pub(super) fn levels_attach_allowed(&self, zone_id: i64) -> bool {
        !self
            .levels_prewarm
            .lock()
            .expect("levels_prewarm lock")
            .contains(&zone_id)
    }

    /// Forwarder de niveaux pour la zone, si elle y a droit (bus présent et
    /// pas de pré-chargement gapless en cours). Capture le `play_seq`
    /// courant : le forwarder meurt de lui-même quand la piste est remplacée.
    /// Factorise le motif répété par tous les chemins qui ont le PCM décodé
    /// en main (transcodes streaming, prefetch) — voir #1105/#1106.
    pub(super) async fn levels_forwarder_if_allowed(
        &self,
        zone_id: i64,
        start_position_ms: i64,
    ) -> Option<tokio::sync::mpsc::UnboundedSender<crate::audio::tap::RawWindow>> {
        let bus = self
            .event_bus
            .clone()
            .filter(|_| self.levels_attach_allowed(zone_id))?;
        let play_seq = self.playback.current_play_seq(zone_id).await;
        Some(spawn_paced_levels_forwarder(
            bus,
            self.playback.clone(),
            zone_id,
            play_seq,
            start_position_ms,
        ))
    }

    /// VU-mètres d'une session proxy (passthrough streaming Qobuz/Tidal) :
    /// lance en tâche de fond une seconde connexion CDN décodée uniquement
    /// pour les niveaux (voir [`decode_http_stream_for_levels`]). Le flux
    /// servi au renderer n'est pas touché — bit-perfect préservé. Une tâche
    /// sœur échantillonne la position rapportée de la zone pour brider la
    /// sonde au rythme de lecture ; les deux s'arrêtent quand le forwarder
    /// disparaît (stop / piste remplacée).
    pub(super) async fn spawn_proxy_levels_probe(
        &self,
        zone_id: i64,
        url: String,
        codec_hint: String,
    ) {
        let Some(bus) = self
            .event_bus
            .clone()
            .filter(|_| self.levels_attach_allowed(zone_id))
        else {
            return;
        };
        // Épinglé ICI, pas dans la tâche : c'est la piste dont on vient de
        // décider les niveaux (#1110).
        let play_seq = self.playback.current_play_seq(zone_id).await;
        spawn_proxy_levels_probe_task(
            self.playback.clone(),
            bus,
            zone_id,
            url,
            codec_hint,
            play_seq,
        );
    }

    /// Serve prefetched PCM data as a WAV stream session.
    ///
    /// Creates a streaming session and feeds the already-decoded PCM into it,
    /// bypassing the download+decode pipeline entirely.
    pub(super) async fn serve_prefetched_pcm(
        &self,
        prefetched: crate::prefetch::PrefetchedTrack,
        req: &PlayRequest,
    ) -> Result<ResolvedStream, String> {
        let bd = prefetched.bit_depth;
        let etiquettes = self.etiqueter_le_prefetch(&prefetched, req).await;

        let is_local_stream = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("local:"));
        let is_network_output = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| !id.starts_with("local:") && !id.starts_with("oaat:"));
        // Même règle que le bras transcodage : ce qu'on ANNONCE doit être une
        // largeur que la chaîne sait ÉCRIRE.
        //
        // `bd.max(16).min(24)` est la troisième écriture à la main de
        // `cap_output_bit_depth` — la fonction créée précisément pour qu'on
        // cesse de la réécrire (#1610) — et elle en a le même angle mort :
        // 17..23 passent intacts, et ni `encode_wav` ni `pcm_to_i32` ne savent
        // les écrire. Un prefetch d'une source de 20 bits partait donc vers un
        // encodeur qui la refuse (#1437).
        let out_bd = if is_local_stream {
            32
        } else {
            crate::audio::decode::container_bit_depth(cap_output_bit_depth(bd))
        };

        // For DLNA/network outputs, encode prefetched PCM to a file.
        // Use FLAC if the renderer supports it, otherwise WAV.
        if is_network_output {
            return self
                .encoder_le_prefetch_en_fichier(prefetched, req, etiquettes, out_bd)
                .await;
        }
        self.servir_le_prefetch_en_wav(prefetched, req, etiquettes, out_bd)
            .await
    }

    /// Premier temps : le titre, l'artiste, l'album, la pochette et la durée
    /// annoncés. La demande (l'écoute en cours) l'emporte sur le tampon, qui
    /// décrit la piste SUIVANTE ; le service comble ce qui manque encore.
    async fn etiqueter_le_prefetch(
        &self,
        prefetched: &crate::prefetch::PrefetchedTrack,
        req: &PlayRequest,
    ) -> Etiquettes {
        // Prefer the request's metadata (from now_playing) over the prefetch
        // buffer's. The buffer is built for the *next* track and can carry an
        // empty title (prefetched before its metadata was resolved); serving it
        // verbatim after a seek wipes the Now Playing title (DEvir: title
        // disappears when seeking shortly after a TIDAL track starts).
        let mut title = req
            .title
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| prefetched.title.clone());
        let mut artist = req
            .artist_name
            .clone()
            .or_else(|| prefetched.artist.clone());
        let mut album = req.album_title.clone().or_else(|| prefetched.album.clone());
        let mut cover_url = req
            .cover_url
            .clone()
            .or_else(|| prefetched.cover_url.clone());

        // Duration can also be missing from the prefetch buffer (metadata not
        // resolved at prefetch time → `prefetched.duration_ms == 0`). Serving a
        // zero duration is worse than a blank title: on an exclusive output the
        // poller's position-based end detection needs duration > 0, so a
        // 0-duration repeat can only advance via the 45 s load-grace timeout —
        // and since the next repeat inherits 0 again, playback falls into an
        // infinite 45 s silent loading loop (DEvir: seek under Repeat One).
        // Prefer the prefetch value, fall back to the request, then recover from
        // the service metadata below alongside the title.
        let mut duration_ms: u64 = if prefetched.duration_ms > 0 {
            prefetched.duration_ms
        } else {
            req.duration_ms
                .filter(|d| *d > 0)
                .map(|d| d as u64)
                .unwrap_or(0)
        };

        // Both the request and the prefetch buffer can carry an empty title when
        // the streaming_queue row was persisted without metadata (DEvir: Repeat
        // All on a single-track queue prefetches itself, then re-plays via this
        // prefetched path with `title=""` — auto_next logs the right title but
        // orchestrator_play/Now Playing go blank). When that (or a missing
        // duration) happens, refetch the real metadata from the service so Now
        // Playing is never blanked and end detection has a duration.
        if title.is_empty() || duration_ms == 0 {
            let registry = self.services.lock().await;
            if let Some(svc) = registry.get(&prefetched.source) {
                let svc = svc.read().await;
                if let Ok(track) = svc.get_track(&prefetched.source_id).await {
                    if title.is_empty() {
                        title = track.title;
                        artist = artist.or(Some(track.artist));
                        album = album.or(track.album);
                        cover_url = cover_url.or(track.cover_path);
                    }
                    if duration_ms == 0 && track.duration_ms > 0 {
                        duration_ms = track.duration_ms;
                    }
                }
            }
        }

        // Determine output bit depth based on output type
        Etiquettes {
            title,
            artist,
            album,
            cover_url,
            duration_ms,
        }
    }

    /// Sortie réseau (DLNA, OpenHome) : le PCM préchargé est encodé en FLAC,
    /// ou en WAV si le renderer ne lit pas le FLAC, dans un fichier servi
    /// avec Content-Length ; les niveaux partent en parallèle.
    async fn encoder_le_prefetch_en_fichier(
        &self,
        prefetched: crate::prefetch::PrefetchedTrack,
        req: &PlayRequest,
        etiquettes: Etiquettes,
        out_bd: u16,
    ) -> Result<ResolvedStream, String> {
        let Etiquettes {
            title,
            artist,
            cover_url,
            duration_ms,
            ..
        } = etiquettes;
        let sr = prefetched.sample_rate;
        let bd = prefetched.bit_depth;
        let ch = prefetched.channels;
        let use_wav = if let Some(device_id) = req.output_device_id.as_deref() {
            !self.dlna_supports_mime(device_id, "audio/flac").await
        } else {
            false
        };
        let ext = if use_wav { "wav" } else { "flac" };
        let tmp_path =
            std::env::temp_dir().join(format!("tune-prefetch-{}.{ext}", uuid::Uuid::new_v4()));
        let tmp_str = tmp_path.to_string_lossy().to_string();
        // Match the encoded header's bit depth (out_bd) to the actual PCM.
        let pcm_data = if bd != out_bd {
            crate::audio::decode::convert_pcm_bytes(&prefetched.pcm_data, bd, out_bd)
        } else {
            prefetched.pcm_data
        };
        let encode_sr = sr;
        let encode_bd = out_bd;
        let encode_ch = ch;
        let encode_path = tmp_str.clone();
        let encode_wav = use_wav;
        // VU-mètres : le tampon prefetch EST le PCM décodé — sans ce
        // renvoi, une piste streaming servie depuis le prefetch (gapless
        // N+1) laissait les aiguilles figées alors que la piste jouée via
        // le pipeline download+decode les animait.
        let prefetch_levels_tx = self.levels_forwarder_if_allowed(req.zone_id, 0).await;
        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            if let Some(ref ltx) = prefetch_levels_tx {
                crate::audio::tap::send_windowed_pcm(
                    ltx,
                    &pcm_data,
                    encode_bd,
                    encode_ch as u16,
                    encode_sr,
                );
            }
            let data_size = pcm_data.len() as u32;
            let byte_rate = encode_sr * encode_ch as u32 * (encode_bd as u32 / 8);
            let block_align = encode_ch as u16 * (encode_bd as u16 / 8);
            if encode_wav {
                let mut f = std::fs::File::create(&encode_path)
                    .map_err(|e| format!("create tmp wav: {e}"))?;
                let mut hdr = Vec::with_capacity(44);
                hdr.extend_from_slice(b"RIFF");
                hdr.extend_from_slice(&(36 + data_size).to_le_bytes());
                hdr.extend_from_slice(b"WAVEfmt ");
                hdr.extend_from_slice(&16u32.to_le_bytes());
                hdr.extend_from_slice(&1u16.to_le_bytes());
                hdr.extend_from_slice(&(encode_ch as u16).to_le_bytes());
                hdr.extend_from_slice(&encode_sr.to_le_bytes());
                hdr.extend_from_slice(&byte_rate.to_le_bytes());
                hdr.extend_from_slice(&block_align.to_le_bytes());
                hdr.extend_from_slice(&(encode_bd as u16).to_le_bytes());
                hdr.extend_from_slice(b"data");
                hdr.extend_from_slice(&data_size.to_le_bytes());
                f.write_all(&hdr)
                    .map_err(|e| format!("write wav header: {e}"))?;
                f.write_all(&pcm_data)
                    .map_err(|e| format!("write wav pcm: {e}"))?;
                Ok::<(), String>(())
            } else {
                // Encodage FLAC NATIF. Le chemin precedent ecrivait un WAV
                // temporaire puis lancait `ffmpeg -c:a flac` — un binaire
                // externe retire du projet en v0.8.46, donc un echec
                // systematique partout ou il n'est pas installe par
                // ailleurs, et un fichier de cache jamais produit.
                //
                // `AudioEncoder` est deja dans l'arbre et fait le meme
                // travail sans processus externe ni fichier intermediaire.
                // On est dans un `spawn_blocking`, donc les variantes
                // `_sync` sont exactement ce qu'il faut (cf. leur
                // documentation : pas d'await, encodage pur CPU).
                let mut enc = crate::audio::encoder::AudioEncoder::new(
                    "flac",
                    encode_sr,
                    encode_bd as u32,
                    encode_ch as u32,
                );
                enc.start_sync()?;
                enc.write_sync(&pcm_data)?;
                let flac = enc.finish_sync()?;
                std::fs::write(&encode_path, &flac).map_err(|e| format!("write flac: {e}"))?;
                Ok(())
            }
        })
        .await
        .map_err(|e| format!("spawn: {e}"))??;

        let file_size = std::fs::metadata(&tmp_str).map(|m| m.len()).unwrap_or(0);
        let (out_format, out_mime) = if use_wav {
            ("wav", "audio/wav")
        } else {
            ("flac", "audio/flac")
        };
        info!(
            title = %prefetched.title,
            file_size,
            format = out_format,
            "prefetch_pcm_encoded_for_dlna"
        );

        let flac_info = StreamInfo {
            format: out_format.into(),
            mime_type: out_mime.into(),
            sample_rate: sr,
            bit_depth: out_bd,
            channels: ch,
            file_size: Some(file_size),
            duration_ms: Some(duration_ms),
            ..Default::default()
        };

        let session_id = self
            .streamer
            .create_file_session(flac_info, tmp_str.clone(), false)
            .await;

        let server_ip = self.server_ip();
        let stream_url = self
            .streamer
            .get_stream_url(&session_id, &server_ip, "flac");

        Ok(ResolvedStream {
            url: stream_url,
            stream_id: Some(session_id),
            title: title.clone(),
            artist: artist.clone(),
            album: None,
            duration_ms: Some(duration_ms as i64),
            source: prefetched.source,
            mime_type: "audio/flac".into(),
            sample_rate: Some(sr),
            bit_depth: Some(out_bd as u32),
            channels: Some(ch as u32),
            // What we serve here is a local session over the decoded buffer;
            // the service's own URL travels on `PrefetchedTrack` so a
            // recorder still gets the published bytes rather than our
            // re-encode. A track shorter than the prefetch window is served
            // from this path, so without it short tracks were the only ones
            // captured through the proxy — and filed under `Stream/`.
            origin_url: prefetched.upstream_url,
            bitrate_kbps: None,
            cover_url: cover_url.clone(),
            file_size: Some(file_size),
        })
    }

    /// Sortie locale ou PULL : le PCM préchargé est poussé tel quel (converti
    /// à la profondeur de sortie) dans une session-canal WAV.
    async fn servir_le_prefetch_en_wav(
        &self,
        prefetched: crate::prefetch::PrefetchedTrack,
        req: &PlayRequest,
        etiquettes: Etiquettes,
        out_bd: u16,
    ) -> Result<ResolvedStream, String> {
        let Etiquettes {
            title,
            artist,
            album,
            cover_url,
            duration_ms,
        } = etiquettes;
        let sr = prefetched.sample_rate;
        let bd = prefetched.bit_depth;
        let ch = prefetched.channels;
        let wav_info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: sr,
            bit_depth: out_bd,
            channels: ch,
            file_size: None,
            duration_ms: Some(duration_ms),
            ..Default::default()
        };

        let (session_id, tx, data_ready) = self.streamer.create_session(wav_info, false, 256).await;

        // Feed the prefetched PCM data into the session in chunks.
        // This happens nearly instantly since the data is already in memory.
        // The buffer is stored at the source bit depth (`bd`); widen it to the
        // WAV header's `out_bd` (32 for local output) or the device reads 32-bit
        // frames out of 16-bit data → white noise (Bilou: bruit blanc next-track).
        let pcm_data = if bd != out_bd {
            info!(
                from_bd = bd,
                to_bd = out_bd,
                "prefetch_pcm_bit_depth_converted"
            );
            crate::audio::decode::convert_pcm_bytes(&prefetched.pcm_data, bd, out_bd)
        } else {
            prefetched.pcm_data
        };
        // VU-mètres : même renvoi que la branche réseau — le tampon prefetch
        // est le PCM décodé, il alimente le forwarder de niveaux. Fenêtrage
        // AVANT le gavage de la session (une passe memcpy, quelques dizaines
        // de ms) : le gavage, lui, dure toute la piste (canal borné, rythmé
        // par le client), les niveaux seraient arrivés trop tard.
        let prefetch_levels_tx = self.levels_forwarder_if_allowed(req.zone_id, 0).await;
        let pcm_data = std::sync::Arc::new(pcm_data);
        tokio::spawn(async move {
            if let Some(ltx) = prefetch_levels_tx {
                let levels_pcm = pcm_data.clone();
                let levels_bd = out_bd;
                let levels_ch = ch;
                let levels_sr = sr;
                tokio::task::spawn_blocking(move || {
                    crate::audio::tap::send_windowed_pcm(
                        &ltx,
                        &levels_pcm,
                        levels_bd,
                        levels_ch as u16,
                        levels_sr,
                    );
                });
            }
            let chunk_size = 32768;
            let mut first = true;
            for chunk in pcm_data.chunks(chunk_size) {
                if tx.send(chunk.to_vec()).await.is_err() {
                    debug!("prefetch_session_consumer_dropped");
                    return;
                }
                if first {
                    first = false;
                    data_ready.notify_one();
                }
            }
            if first {
                // No data was sent (empty buffer)
                data_ready.notify_one();
            }
            debug!("prefetch_pcm_feed_complete");
        });

        let server_ip = self.server_ip();
        let stream_url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");

        Ok(ResolvedStream {
            url: stream_url,
            mime_type: "audio/wav".into(),
            title: title.clone(),
            artist: artist.clone(),
            album: album.clone(),
            duration_ms: Some(duration_ms as i64),
            source: prefetched.source,
            cover_url: cover_url.clone(),
            stream_id: Some(session_id),
            file_size: None,
            sample_rate: Some(sr),
            bit_depth: Some(out_bd as u32),
            channels: Some(ch as u32),
            // Same as the FLAC-session branch above: this is a WAV session over
            // the decoded buffer, so an output that wants the source container
            // needs the service's own URL, not ours.
            origin_url: prefetched.upstream_url,
            bitrate_kbps: None,
        })
    }

    /// True when the zone has an ENABLED equalizer profile with an audible
    /// effect (and is not in PURE mode). Cheap settings read used to decide
    /// routing BEFORE the sample rate is known — the actual EqProcessor is
    /// built later by the transcode path at the real rate.
    /// True when ReplayGain would change the samples for this zone's track.
    ///
    /// Same shape as [`Self::zone_has_active_eq`], and needed for the same
    /// reason: a network renderer served the file raw never runs any of our
    /// DSP, so without forcing the transcode the gain would be computed,
    /// logged, and silently thrown away.
    pub(super) fn zone_replaygain_changes_audio(
        &self,
        zone_id: i64,
        track_id: Option<i64>,
    ) -> bool {
        if self.zone_audiophile(zone_id) {
            return false;
        }
        match track_id {
            Some(tid) => {
                (crate::audio::replaygain::playback_factor(&self.db, tid) - 1.0).abs() > 1e-6
            }
            None => false,
        }
    }

    /// Réappliquer l'égaliseur d'une zone à la sortie locale qui joue, sans
    /// attendre la piste suivante.
    ///
    /// `set_eq` n'était appelé qu'au démarrage d'une piste — « rebuilt at each
    /// play », par construction, puisqu'un `EqProcessor` se bâtit POUR un couple
    /// (taux, canaux). Bouger un curseur en cours de lecture persistait donc le
    /// profil, renvoyait 200, et ne changeait rien avant la piste suivante
    /// (#1725). Or c'est exactement le geste par lequel on règle un égaliseur :
    /// musique en cours, à l'oreille. Trois signalements « l'égaliseur ne
    /// fonctionne pas » (#1372, #1555, #1688) l'ont précédé.
    ///
    /// `LocalOutput::current_format` mémorise désormais le couple réellement vu
    /// par `apply_local_dsp`, ce qui permet de rebâtir aux bons coefficients.
    ///
    /// Renvoie `true` si une sortie locale vivante a reçu le nouveau contrat,
    /// y compris quand ce contrat retire l'égaliseur (`None`) en mode PURE ou
    /// avec un profil désactivé. `false` est réservé à l'absence de chemin
    /// local vivant : zone distante, sortie absente ou format encore inconnu.
    pub async fn refresh_zone_eq(&self, zone_id: i64) -> bool {
        self.refresh_zone_eq_detaille(zone_id).await.applique()
    }

    /// Le corps de [`Self::refresh_zone_eq`], mais qui DIT ce qu'il a fait.
    ///
    /// Même travail, mêmes conditions d'arrêt, à l'octet près — seule la valeur
    /// rendue change : un [`RapportEqAChaud`] au lieu d'un `bool` muet. Chaque
    /// sortie anticipée porte désormais son motif ([`EchecEqLocal`]), et le
    /// chemin nominal rapporte le format vu, le nombre de bandes et le pré-gain
    /// réservé. C'est cette matière que `eq_change_journal` met dans le journal
    /// (#3479).
    /// Le périphérique de sortie de la zone, ou la chaîne vide s'il n'y en a
    /// pas.
    ///
    /// Lu **sans condition de compilation** : c'est une colonne de `zones`, pas
    /// une propriété de la sortie locale. La CI l'a montré sur le jeu
    /// `--no-default-features --features oaat,cloud-relay,bandcamp` — le bras
    /// « caisse sans audio local » rendait un `device_id` VIDE, et
    /// `eq_change_journal` perdait l'identifiant de l'appareil selon le binaire
    /// livré. Une trace qui change de contenu avec les fonctionnalités
    /// compilées ne vaut rien pour diagnostiquer chez un testeur : c'est
    /// justement le binaire allégé qu'on ne peut pas interroger autrement.
    fn peripherique_de_la_zone(&self, zone_id: i64) -> String {
        ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id)
            .unwrap_or_default()
    }

    pub(crate) async fn refresh_zone_eq_detaille(&self, zone_id: i64) -> RapportEqAChaud {
        // Le périphérique est relevé AVANT tout aiguillage, et par le même
        // chemin dans les deux jeux de fonctionnalités : c'est ce qui garantit
        // que le journal le porte toujours.
        let device_id = self.peripherique_de_la_zone(zone_id);
        #[cfg(not(feature = "local-audio"))]
        {
            RapportEqAChaud::echoue(device_id, EchecEqLocal::CaisseSansAudioLocal)
        }
        #[cfg(feature = "local-audio")]
        {
            if device_id.is_empty() {
                return RapportEqAChaud::echoue(device_id, EchecEqLocal::ZoneSansPeripherique);
            }
            if !device_id.starts_with("local:") {
                return RapportEqAChaud::echoue(device_id, EchecEqLocal::SortieNonLocale);
            }
            let Some(output_arc) = ({ self.outputs.lock().await.get(&device_id) }) else {
                return RapportEqAChaud::echoue(device_id, EchecEqLocal::SortieAbsenteDuRegistre);
            };
            let output = output_arc.lock().await;
            let Some(local_output) = output
                .as_any()
                .downcast_ref::<crate::outputs::local::LocalOutput>()
            else {
                return RapportEqAChaud::echoue(device_id, EchecEqLocal::SortiePasUneSortieLocale);
            };
            // Pas de flux en cours : la prochaine lecture rebâtira l'EQ de
            // toute façon, et bâtir pour un format inconnu donnerait des
            // coefficients faux.
            let Some((taux, canaux)) = local_output.current_format() else {
                return RapportEqAChaud::echoue(device_id, EchecEqLocal::FormatInconnu);
            };
            let eq = self.load_eq_processor(zone_id, taux, canaux);
            let actif = eq.is_some();
            let preamp_db = eq.as_ref().and_then(|e| e.preamp_db(0));
            let preamp_db_droite = eq.as_ref().and_then(|e| e.preamp_db(1));
            // `replace_eq_live` et non `set_eq` : la piste est en cours, donc
            // l'historique des biquads doit survivre au remplacement, sinon le
            // geste même qu'on vient de rendre possible — bouger un curseur en
            // écoutant — claque à chaque cran.
            local_output.replace_eq_live(eq);
            info!(
                zone_id,
                device_id = %device_id,
                sample_rate = taux,
                channels = canaux,
                actif,
                "zone_eq_refreshed_live"
            );
            RapportEqAChaud {
                device_id,
                echec: None,
                format: Some((taux, canaux)),
                eq_actif: actif,
                preamp_db,
                preamp_db_droite,
            }
        }
    }

    /// Faire prendre effet un changement d'égaliseur, PAR TOUS LES CHEMINS.
    ///
    /// Point d'entrée unique des routes qui écrivent `zone_{id}_eq_profile`.
    /// Il existe parce que la règle « local d'abord, redémarrage sinon » ne
    /// doit vivre qu'à un seul endroit : c'est sa duplication implicite entre
    /// quatre routes qui avait produit #1725.
    ///
    /// - **sortie locale** : [`Self::refresh_zone_eq`] remplace l'`EqProcessor`
    ///   derrière son mutex — immédiat, inaudible, aucune coupure ;
    /// - **tout le reste** (DLNA, navigateur) : le fichier transcodé est déjà
    ///   écrit et téléchargé, rien à remplacer. [`Self::schedule_eq_replay`]
    ///   programme un redémarrage anti-rebondi (#1710).
    ///
    /// Rend `true` quand le réglage a atteint le son **immédiatement** — donc
    /// uniquement sur le chemin local. Un redémarrage programmé rend `false` :
    /// il n'a pas encore eu lieu, et une demande plus récente peut encore
    /// l'annuler. L'interface continue donc d'annoncer « prendra effet à la
    /// piste suivante », ce qui est vrai jusqu'à ce que le redémarrage advienne
    /// — mieux vaut cela qu'une promesse que l'anti-rebond peut retirer.
    ///
    /// Toute mutation annonce ensuite `zone.updated`. Le contrat de cet
    /// événement est volontairement minimal : les clients rechargent la zone
    /// et reconstruisent ainsi `signal_path` depuis le profil EQ qui vient
    /// d'être persisté, au lieu de conserver l'instantané de la lecture (#1985).
    pub async fn apply_eq_change(self: &std::sync::Arc<Self>, zone_id: i64) -> bool {
        self.apply_eq_change_portee(zone_id).await == PorteeDuReglage::Immediate
    }

    /// [`Self::apply_eq_change`], mais qui dit QUAND le réglage atteindra le
    /// son, au lieu d'un booléen à plusieurs sens (#4680).
    ///
    /// `false` confondait « relance du flux programmée dans une demi-seconde »
    /// (DLNA, navigateur) avec « piste suivante » : l'interface annonçait donc
    /// « prendra effet à la piste suivante » sur une zone réseau où l'effet
    /// s'entend presque aussitôt — recette v0.9.161, Eversolo DMP-A8.
    pub async fn apply_eq_change_portee(
        self: &std::sync::Arc<Self>,
        zone_id: i64,
    ) -> PorteeDuReglage {
        let debut = std::time::Instant::now();
        let rapport = self.refresh_zone_eq_detaille(zone_id).await;
        let applique_a_chaud = rapport.applique();
        let mut chemin = if applique_a_chaud {
            "local_a_chaud"
        } else {
            "sans_chemin"
        };
        if !applique_a_chaud {
            // Pas de chemin local vivant. Reste le redémarrage — mais uniquement si
            // quelque chose joue : sinon la prochaine lecture rebâtira l'EQ toute
            // seule, et redémarrer un flux inexistant n'a aucun sens.
            let np = self.playback.get_state(zone_id).await.now_playing;
            chemin = if np.is_none() {
                "rien_ne_joue"
            } else if np
                .as_ref()
                .is_some_and(|np| self.flux_porte_deja_ce_traitement(zone_id, np))
            {
                // #4407 — le flux qui joue porte déjà exactement ce traitement :
                // le refabriquer rendrait les mêmes octets et couperait le son
                // (2,787 s sur une radio DLNA, Marantz ND8006). Jumeau de la
                // garde PURE de #4004, mais comparé au flux SERVI, pas au
                // réglage en base : une relance abandonnée laisse l'ancien
                // traitement dans le flux, et la comparaison le voit.
                "flux_conserve_signal_identique"
            } else if self.schedule_eq_replay(zone_id) {
                "replay_programme"
            } else {
                // `schedule_eq_replay` a déjà dit POURQUOI (#2595) ; ici on
                // retient seulement que rien ne partira.
                "replay_refuse_position_inconnue"
            };
        }

        // Arrete AVANT la journalisation, et c'est le point : ce qu'on chiffre
        // est le cout de la BASCULE — relire la zone, batir la cascade, la
        // poser derriere le mutex que la boucle audio relit a chaque paquet —,
        // pas le cout de la trace qui la raconte. Passe ce point, plus rien ne
        // touche le son.
        let duree = debut.elapsed();
        self.journaliser_le_changement_d_eq(zone_id, &rapport, chemin, duree);

        if let Some(ref bus) = self.event_bus {
            bus.emit("zone.updated", serde_json::json!({ "zone_id": zone_id }));
        }
        PorteeDuReglage::du_chemin(chemin)
    }

    /// Combien de flux résolus une zone garde en mémoire (#4407) : celui qui
    /// joue, celui qu'on pré-arme pour l'enchaînement, et de la marge.
    pub(crate) const FLUX_TRAITES_PAR_ZONE: usize = 4;

    /// L'empreinte du traitement qu'un flux bâti MAINTENANT pour cette zone
    /// porterait dans ses octets (#4407). Mêmes lectures que
    /// [`Self::traitement_que_pure_gouverne`], mais les VALEURS et non les
    /// noms : deux profils d'égaliseur différents donnent deux empreintes, un
    /// profil sans effet (`EqProcessor::is_enabled() == false`, grave,
    /// médium, aigu et bandes compris) n'en laisse aucune — c'est ce qui
    /// distingue « `bandes=0` » d'« égaliseur inactif », que JP Robbe a
    /// rappelé sur le ticket. PURE fait rendre `None` aux quatre chargeurs.
    ///
    /// Approximation dans le sens sûr : la radio ne cuit que l'égaliseur, le
    /// bras fichier ne cuit pas le crossfeed — une empreinte qui bouge sur ces
    /// étages relance un flux qui n'en avait pas besoin, jamais l'inverse.
    pub(super) fn empreinte_du_traitement(&self, zone_id: i64, track_id: Option<i64>) -> String {
        if self.zone_audiophile(zone_id) {
            return "pure".to_string();
        }
        let eq = self
            .eq_profile_configure(zone_id)
            .filter(|p| crate::audio::eq::EqProcessor::new(p, 44100, 2).is_enabled())
            .map(|p| serde_json::to_string(&p).unwrap_or_else(|_| format!("{p:?}")))
            .unwrap_or_default();
        let ir = self
            .chemin_ir_configure(zone_id)
            .filter(|p| std::path::Path::new(p).exists())
            .unwrap_or_default();
        let crossfeed = self
            .crossfeed_configure(zone_id)
            .map(|(amount, delay_ms)| format!("{amount}:{delay_ms}"))
            .unwrap_or_default();
        let replaygain = track_id
            .map(|tid| crate::audio::replaygain::playback_factor(&self.db, tid))
            .filter(|f| (f - 1.0).abs() > 1e-6)
            .map(|f| f.to_bits().to_string())
            .unwrap_or_default();
        format!("eq={eq}\u{1f}ir={ir}\u{1f}cf={crossfeed}\u{1f}rg={replaygain}")
    }

    /// Retenir le traitement avec lequel le flux `stream_id` a été résolu.
    pub(super) fn noter_traitement_du_flux(
        &self,
        zone_id: i64,
        stream_id: &str,
        empreinte: String,
    ) {
        let mut flux = self.traitement_des_flux.lock().unwrap();
        let notes = flux.entry(zone_id).or_default();
        notes.retain(|(sid, _)| sid != stream_id);
        notes.push((stream_id.to_string(), empreinte));
        let trop = notes.len().saturating_sub(Self::FLUX_TRAITES_PAR_ZONE);
        notes.drain(..trop);
    }

    /// Le flux que la zone JOUE porte-t-il déjà le traitement qu'elle demande ?
    ///
    /// Inconnu — pas de `stream_id` (URL directe, fichier déposé), flux résolu
    /// avant le démarrage du serveur ou sorti de la mémoire — ⇒ `false` : on
    /// relance, comme avant.
    pub(super) fn flux_porte_deja_ce_traitement(
        &self,
        zone_id: i64,
        np: &crate::playback::NowPlaying,
    ) -> bool {
        let Some(ref sid) = np.stream_id else {
            return false;
        };
        let servi = self
            .traitement_des_flux
            .lock()
            .unwrap()
            .get(&zone_id)
            .and_then(|notes| notes.iter().find(|(s, _)| s == sid).map(|(_, e)| e.clone()));
        servi.is_some_and(|e| e == self.empreinte_du_traitement(zone_id, np.track_id))
    }

    /// LA trace qui tranche, à l'activation de l'égaliseur (#3479).
    ///
    /// Une ligne, au niveau INFO — donc présente dans l'export de diagnostic
    /// qu'un testeur joint à son fil — et qui répond aux quatre questions
    /// qu'aucun journal ne savait répondre le 06/09/2026, quand Reivax66 a
    /// signalé « l'activation de l'égaliseur coupe le son » depuis Windows :
    ///
    /// | question | champs |
    /// |---|---|
    /// | quelle zone ? | `zone_id`, `zone` |
    /// | quel type de sortie ? | `famille`, `output_type`, `device_id` |
    /// | quel format avant, quel format après ? | `format_avant`, `format_apres`, `preamp_db_g`, `preamp_db_d` |
    /// | quel est le premier échec ? | `premier_echec`, `chemin` |
    ///
    /// « Avant » et « après » sont DEUX champs même quand la cadence et les
    /// canaux ne bougent pas, et c'est délibéré : le fait qu'ils ne bougent PAS
    /// est ce qui écarte l'hypothèse « format refusé par le périphérique après
    /// traitement ». Sur ce chemin, ce que l'égaliseur change est le NIVEAU, et
    /// c'est `preamp_db_g` / `preamp_db_d` qui le chiffrent.
    ///
    /// Le pendant réseau de cette ligne est `eq_format_apres_traitement`
    /// (`resolve_local.rs`) : là, le format change pour de bon, et l'étiquette
    /// servie au renderer peut diverger de la charge utile (#3357).
    fn journaliser_le_changement_d_eq(
        &self,
        zone_id: i64,
        rapport: &RapportEqAChaud,
        chemin: &str,
        duree: std::time::Duration,
    ) {
        let zone = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten();
        let nom = zone.as_ref().map(|z| z.name.clone()).unwrap_or_default();
        let output_type = zone.as_ref().and_then(|z| z.output_type.clone());
        let famille = famille_de_sortie(&rapport.device_id, output_type.as_deref());
        let format = rapport.format_ecrit();
        // Le profil est une propriété de la ZONE, pas de la sortie : il se lit
        // même quand aucun chemin local n'a pu être servi, et c'est justement
        // là que sa lecture compte.
        let profil = self.load_eq_profile(zone_id);
        info!(
            zone_id,
            zone = %nom,
            famille,
            output_type = output_type.as_deref().unwrap_or("-"),
            device_id = %rapport.device_id,
            pure = self.zone_audiophile(zone_id),
            profil_actif = profil.is_some(),
            bandes = profil.map(|p| p.bands.len()).unwrap_or(0),
            eq_actif = rapport.eq_actif,
            format_avant = %format,
            format_apres = %format,
            preamp_db_g = rapport.preamp_db.unwrap_or(0.0),
            preamp_db_d = rapport.preamp_db_droite.unwrap_or(0.0),
            chemin,
            premier_echec = rapport.echec.map(EchecEqLocal::nom).unwrap_or("-"),
            duree_ms = duree.as_micros() as f64 / 1000.0,
            amortissement = %Self::amortissement_du_chemin(chemin),
            "eq_change_journal"
        );
    }

    /// Quel amortissement gouverne CETTE bascule — et le mot compte (#3479).
    ///
    /// Le 08/09/2026, Reivax66 écrit « maintenant ça fonctionne, il reste à
    /// améliorer la réactivité ». Le tri du soir même a montré que trois
    /// dossiers sans une ligne de code commune produisent cette phrase, et que
    /// rien dans le journal ne permettait de choisir : `eq_change_journal`
    /// nommait le chemin, jamais son coût ni ce qui le retenait.
    ///
    /// Or les deux chemins de [`Self::apply_eq_change`] ne sont pas amortis de
    /// la même façon, et c'est une asymétrie DÉLIBÉRÉE que personne ne pouvait
    /// lire depuis un export :
    ///
    /// | chemin | amortissement | pourquoi |
    /// |---|---|---|
    /// | `local_a_chaud` | **aucun** | l'`EqProcessor` vit derrière un mutex relu à chaque paquet : le remplacer est inaudible, donc chaque cran de curseur le refait, et c'est ce qui rend le geste « bouger un curseur en écoutant » possible (#1725) |
    /// | `replay_programme` | anti-rebond + plancher | là, le flux est redémarré, la manœuvre est AUDIBLE : sans amortissement un curseur de 31 bandes produirait 31 coupures (#1710) |
    /// | le reste | sans objet | rien n'a été programmé |
    ///
    /// Ce que ce champ permet de faire sur un simple export, et qu'il fallait
    /// jusqu'ici le code sous les yeux pour établir : **un `local_a_chaud`
    /// exclut le redémarrage de flux**, donc la famille #2102
    /// (« chaque changement d'égaliseur coupe et relance le flux ») ; et il
    /// exclut aussi #1725/#1710 (« l'égaliseur n'agit qu'à la piste
    /// suivante »), puisque le mutex remplacé est celui que le paquet SUIVANT
    /// relit. Les deux se lisaient déjà dans `chemin` — mais seulement pour qui
    /// savait ce que `chemin` implique. Ici, c'est écrit.
    ///
    /// Les deux durées viennent des constantes réelles, jamais d'un littéral
    /// recopié : une valeur qu'on change se lit aussitôt dans le journal, et un
    /// amortissement qu'on retire disparaît de la ligne.
    pub fn amortissement_du_chemin(chemin: &str) -> String {
        match chemin {
            "local_a_chaud" => "aucun".to_string(),
            "replay_programme" => format!(
                "anti_rebond_{}ms_plancher_{}ms",
                Self::EQ_REPLAY_DEBOUNCE_MS,
                Self::EQ_REPLAY_FLOOR_MS
            ),
            _ => "sans_objet".to_string(),
        }
    }

    /// Faire prendre effet un changement d'égaliseur sur un chemin **non
    /// local**, en redémarrant le flux à la position courante (#1710, lot 2).
    ///
    /// Sur une sortie locale, `refresh_zone_eq` suffit : l'`EqProcessor` vit
    /// derrière un mutex relu à chaque paquet. Ailleurs — DLNA, navigateur — le
    /// fichier transcodé est déjà écrit, déjà en cours de téléchargement,
    /// souvent déjà en cache. Rien à remplacer : il faut re-résoudre.
    ///
    /// **Anti-rebondi et planchéié**, parce que la manœuvre est audible. Sans
    /// ça, un curseur de 31 bandes produirait 31 coupures d'une seconde — un
    /// remède pire que le mal. Voir [`Self::replay_zone_at_position`].
    ///
    /// Rend immédiatement : le redémarrage est différé dans une tâche. La
    /// valeur dit si un redémarrage a été **programmé**, pas s'il a eu lieu —
    /// une demande plus récente peut encore l'annuler.
    ///
    /// **Ne programme rien quand la position de la zone n'est pas mesurée**
    /// (#2595) : voir `Self::position_entretenue_par_le_sondeur`.
    pub fn schedule_eq_replay(self: &std::sync::Arc<Self>, zone_id: i64) -> bool {
        // #2595 — ne pas rejouer à une position INCONNUE.
        //
        // Ce redémarrage ne vaut que par sa promesse : reprendre là où on en
        // était. Sur une zone dont personne ne mesure la position, la promesse
        // est vide et le geste devient destructeur — il ramène l'auditeur au
        // début du morceau, ce qu'a signalé Pierre M.
        //
        // Renoncer est ici le geste le MOINS destructeur. Le réglage prendra
        // effet à la piste suivante, ce que la réponse dit déjà
        // (`applied_live: false`) et ce que le client sait déjà afficher. Un
        // morceau qui continue vaut mieux qu'un morceau qui recommence.
        //
        // On ne comble surtout PAS la source : une zone sans périphérique n'a
        // aucune source de vérité côté serveur sur sa position. Y écrire une
        // valeur reconstituée ferait passer une supposition pour une mesure —
        // exactement le défaut qu'on corrige.
        if !self.position_entretenue_par_le_sondeur(zone_id) {
            info!(zone_id, "eq_replay_skipped_position_inconnue");
            return false;
        }
        let generation = {
            let mut gens = self.eq_replay_gen.lock().unwrap();
            let g = gens.entry(zone_id).or_insert(0);
            *g += 1;
            *g
        };
        let moi = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(
                Self::EQ_REPLAY_DEBOUNCE_MS,
            ))
            .await;
            // Une demande plus récente est arrivée pendant l'attente : c'est
            // elle qui redémarrera, pas nous.
            {
                let gens = moi.eq_replay_gen.lock().unwrap();
                if gens.get(&zone_id).copied() != Some(generation) {
                    return;
                }
            }
            // Plancher : trop tôt après le précédent, on renonce plutôt que de
            // hacher. Le réglage prendra effet à la piste suivante, ce que le
            // client sait déjà dire (`applied_live: false`).
            {
                let derniers = moi.eq_replay_last.lock().unwrap();
                if let Some(t) = derniers.get(&zone_id) {
                    if t.elapsed().as_millis() < Self::EQ_REPLAY_FLOOR_MS as u128 {
                        info!(zone_id, "eq_replay_skipped_floor");
                        return;
                    }
                }
            }
            let position_ms = moi.playback.get_state(zone_id).await.position_ms.max(0) as u64;
            match moi
                .replay_zone_at_position(zone_id, position_ms, "eq_change")
                .await
            {
                Ok(()) => {
                    moi.eq_replay_last
                        .lock()
                        .unwrap()
                        .insert(zone_id, std::time::Instant::now());
                    info!(zone_id, position_ms, "eq_replay_done");
                }
                Err(e) => warn!(zone_id, error = %e, "eq_replay_failed"),
            }
        });
        true
    }

    /// Réappliquer le crossfeed d'une zone à la sortie locale qui joue, sans
    /// attendre la piste suivante.
    ///
    /// Jumeau de [`Self::refresh_zone_eq`], pour le même défaut : `set_crossfeed`
    /// n'était appelé qu'au démarrage d'une piste (`orchestrator.rs`, chemin de
    /// lecture), si bien qu'activer le crossfeed ou déplacer `amount` /
    /// `delay_ms` en écoutant persistait la configuration, renvoyait un succès,
    /// et ne changeait rien avant la piste suivante (#1786).
    ///
    /// Renvoie `true` si un crossfeed a été poussé vers une sortie vivante.
    /// `false` couvre tout le reste — zone sans sortie locale, rien en lecture,
    /// crossfeed désactivé, mode PURE (où `load_crossfeed_processor` rend `None`,
    /// donc la promesse bit-perfect tient sans garde supplémentaire).
    pub async fn refresh_zone_crossfeed(&self, zone_id: i64) -> bool {
        #[cfg(not(feature = "local-audio"))]
        {
            let _ = zone_id;
            false
        }
        #[cfg(feature = "local-audio")]
        {
            let Some(device_id) = ZoneRepo::with_backend(self.db.clone())
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id)
            else {
                return false;
            };
            if !device_id.starts_with("local:") {
                return false;
            }
            let Some(output_arc) = ({ self.outputs.lock().await.get(&device_id) }) else {
                return false;
            };
            let output = output_arc.lock().await;
            let Some(local_output) = output
                .as_any()
                .downcast_ref::<crate::outputs::local::LocalOutput>()
            else {
                return false;
            };
            // Pas de flux en cours : la prochaine lecture rebâtira le crossfeed
            // de toute façon, et le bâtir pour un taux inconnu donnerait une
            // ligne à retard de la mauvaise longueur.
            let Some((taux, _canaux)) = local_output.current_format() else {
                return false;
            };
            let cf = self.load_crossfeed_processor(zone_id, taux);
            let actif = cf.is_some();
            // `replace_crossfeed_live` et non `set_crossfeed` : la piste est en
            // cours, donc les lignes à retard doivent survivre au remplacement.
            local_output.replace_crossfeed_live(cf);
            info!(
                zone_id,
                device_id = %device_id,
                sample_rate = taux,
                actif,
                "zone_crossfeed_refreshed_live"
            );
            actif
        }
    }

    /// Réappliquer TOUT ce que le mode PURE gouverne à la sortie locale qui
    /// joue, sans attendre la piste suivante.
    ///
    /// Le bloc PURE du chemin de lecture — `set_pure_bypass`, ReplayGain,
    /// crossfeed, égaliseur — n'était exécuté qu'au démarrage d'une piste, et
    /// son commentaire l'assumait : « so a zone toggled in/out of PURE takes
    /// effect on the **next track** ». Or basculer PURE est un geste qu'on fait
    /// en écoutant, exactement comme bouger un curseur d'égaliseur (#1725) ou
    /// de crossfeed (#1786). Entre le clic et la piste suivante, l'interrupteur
    /// est vert, le badge PURE est allumé, le panneau annonce un chemin
    /// intouché — et l'`EqProcessor` installé dans la sortie continue de
    /// filtrer chaque échantillon (Jean Valjean, #1986 : « il me semble que
    /// l'égaliseur est toujours actif », +8 dB de Bass Boost en PURE).
    ///
    /// Ici l'écart est plus grave que pour l'EQ ou le crossfeed : ces deux-là
    /// ne promettaient qu'un réglage tardif. PURE promet, lui, que rien ne
    /// touche le signal — et l'affichage le REPETE, puisque le chemin du signal
    /// lit la même clé (`zones.rs`, `zone_eq_alters_signal`). Le panneau disait
    /// donc vrai sur l'intention et faux sur le son.
    ///
    /// La sortie de PURE est tout aussi concernée, et c'est le point 3 du même
    /// signalement (« je devrais revenir au réglage précédent ») : les
    /// processeurs restent alors à `None` et l'égaliseur choisi par
    /// l'utilisateur ne revient qu'à la piste suivante.
    ///
    /// Les quatre réglages sont repoussés ensemble, sous le même verrou, parce
    /// qu'ils décrivent un seul état : en repousser trois laisserait une
    /// combinaison que le chemin de lecture ne produit jamais.
    ///
    /// Rend `true` quand une sortie locale VIVANTE a reçu le nouvel état —
    /// sans rien dire de ce qu'il contient. C'est la différence avec
    /// [`Self::refresh_zone_eq`], qui rend `true` si un égaliseur est actif :
    /// entrer en PURE éteint tout, donc un `false` de ce genre signifierait
    /// « rien reçu » alors que tout vient d'être appliqué.
    pub async fn refresh_zone_pure_dsp(&self, zone_id: i64) -> bool {
        #[cfg(not(feature = "local-audio"))]
        {
            let _ = zone_id;
            false
        }
        #[cfg(feature = "local-audio")]
        {
            let Some(device_id) = ZoneRepo::with_backend(self.db.clone())
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id)
            else {
                return false;
            };
            if !device_id.starts_with("local:") {
                return false;
            }
            let Some(output_arc) = ({ self.outputs.lock().await.get(&device_id) }) else {
                return false;
            };
            // Le track_id est lu AVANT de prendre le verrou de la sortie : le
            // ReplayGain en dépend, et `get_state` prend ses propres verrous.
            let track_id = self
                .playback
                .get_state(zone_id)
                .await
                .now_playing
                .and_then(|np| np.track_id);
            let output = output_arc.lock().await;
            let Some(local_output) = output
                .as_any()
                .downcast_ref::<crate::outputs::local::LocalOutput>()
            else {
                return false;
            };
            // Rien en cours : la prochaine lecture appliquera l'état complet de
            // toute façon, et bâtir des filtres pour un format inconnu donnerait
            // des coefficients faux. Même garde que les deux jumelles.
            let Some((taux, canaux)) = local_output.current_format() else {
                // #4176 — « rien en cours » n'est pas la seule lecture d'un
                // format absent : le fil peut être LANCÉ et attendre encore son
                // premier octet (jusqu'à 10 s sur une radio). Il lira
                // `pure_bypass` à l'ouverture : on le pose, on ne relance rien
                // — la relance ouvrait le périphérique exclusif une seconde
                // fois et perdait la course (`0x8889000A`, zone arrêtée).
                if local_output.flux_en_demarrage() {
                    let pure = self.zone_audiophile(zone_id);
                    local_output.set_pure_bypass(pure);
                    info!(zone_id, pure, "zone_pure_reportee_au_flux_qui_demarre");
                    return true;
                }
                return false;
            };

            let pure = self.zone_audiophile(zone_id);
            local_output.set_pure_bypass(pure);
            // Mêmes expressions que le bloc du chemin de lecture, à dessein :
            // toutes trois rendent `None`/1.0 en PURE, donc l'état repoussé est
            // celui qu'une lecture démarrée maintenant produirait.
            let rg = match (pure, track_id) {
                (false, Some(tid)) => crate::audio::replaygain::playback_factor(&self.db, tid),
                _ => 1.0,
            };
            local_output.set_compensation_de_niveau(self.zone_compensation_de_niveau(zone_id));
            local_output.set_replaygain_factor(rg);
            // #4384 — une bascule PURE en cours d'écoute change le facteur
            // ReplayGain sans passer par `send_to_output` : c'est ici que le
            // crête-mètre d'une zone déjà en lecture apprend le gain de sa
            // sortie. Idempotent — on repousse le même `Arc`.
            self.playback
                .brancher_le_gain_de_sortie(zone_id, local_output.gain_de_rendu());
            self.playback
                .brancher_le_gain_moyen_du_dsp(zone_id, local_output.gain_moyen_du_dsp());
            // `replace_*_live` et non `set_*` : la piste est en cours, donc
            // l'historique des biquads et les lignes à retard doivent survivre
            // au remplacement — sinon la bascule claque.
            local_output.replace_crossfeed_live(self.load_crossfeed_processor(zone_id, taux));
            local_output.replace_eq_live(self.load_eq_processor(zone_id, taux, canaux));
            // Le repli mono est lui aussi gouverné par PURE (#2362) : basculer
            // PURE doit donc le désarmer ou le réarmer dans le même geste, sans
            // quoi une zone qui sort de PURE resterait stéréo jusqu'à la piste
            // suivante alors que le panneau annonce déjà « Mono ».
            local_output.set_mono_downmix(self.zone_mono_downmix(zone_id));
            // Même raison pour la rampe (#1590) : `zone_soft_mute_ms` rend 0 en
            // PURE, donc une zone qui ENTRE en PURE doit la perdre dans le même
            // geste, et une zone qui en sort doit la retrouver — sans attendre
            // la piste suivante.
            local_output.set_soft_mute_ms(self.zone_soft_mute_ms(zone_id));
            info!(
                zone_id,
                device_id = %device_id,
                pure,
                sample_rate = taux,
                channels = canaux,
                replaygain = rg,
                "zone_pure_dsp_refreshed_live"
            );
            true
        }
    }

    pub(super) fn zone_has_active_eq(&self, zone_id: i64) -> bool {
        // 44100/2 is only a probe: EqProcessor::is_enabled() depends on the
        // gains, not the rate.
        self.load_eq_processor(zone_id, 44100, 2).is_some()
    }

    pub(super) fn load_eq_processor(
        &self,
        zone_id: i64,
        sample_rate: u32,
        channels: u16,
    ) -> Option<crate::audio::eq::EqProcessor> {
        let profile = self.load_eq_profile(zone_id)?;
        let eq = crate::audio::eq::EqProcessor::new(&profile, sample_rate, channels);
        if eq.is_enabled() { Some(eq) } else { None }
    }

    /// Chaîne DSP d'une zone pour un bras STREAMING, liée au format PCM que le
    /// bras va réellement produire.
    ///
    /// Les bras de `resolve_streaming_url` décodent tous avec `Some(sr)` et
    /// `Some(2)` : les coefficients construits ici décrivent donc exactement le
    /// PCM qui sera traité, pas une supposition.
    ///
    /// Le mode PURE (audiophile) est déjà refusé par les trois chargeurs, et le
    /// ReplayGain n'est retenu que s'il change réellement les échantillons —
    /// même seuil que `zone_replaygain_changes_audio`. Une zone sans traitement
    /// rend donc un `StreamingDsp` inactif, et le bras garde son comportement
    /// d'avant à l'octet près.
    pub(super) fn load_streaming_dsp(
        &self,
        zone_id: i64,
        track_id: Option<i64>,
        sample_rate: u32,
        channels: u16,
    ) -> StreamingDsp {
        let replaygain = match track_id {
            Some(tid) if !self.zone_audiophile(zone_id) => {
                let f = crate::audio::replaygain::playback_factor(&self.db, tid);
                if (f - 1.0).abs() > 1e-6 {
                    Some(f)
                } else {
                    None
                }
            }
            _ => None,
        };
        StreamingDsp {
            replaygain,
            eq: self.load_eq_processor(zone_id, sample_rate, channels),
            convolver: self.load_convolver(zone_id, sample_rate, channels),
            // #2742 — le crossfeed rejoint les trois autres. `load_crossfeed_processor`
            // n'a jamais porté de garde `local:` : la contrainte vivait chez ses
            // APPELANTS, tous dans la sortie locale. Un bras progressif est donc
            // libre de le charger, et c'est ce qui lui ouvre les zones réseau.
            // La ligne à retard est bâtie au taux réellement servi, comme les
            // biquads de l'égaliseur juste au-dessus.
            crossfeed: self.load_crossfeed_processor(zone_id, sample_rate),
            channels,
        }
    }

    /// La zone a-t-elle un crossfeed réellement actif ?
    ///
    /// Jumeau de [`Self::zone_has_active_eq`], et pour le même usage : décider
    /// du chemin AVANT de connaître le taux réel du flux. 44 100 n'est qu'une
    /// sonde — `load_crossfeed_processor` ne rend `None` que sur le mode PURE,
    /// la case décochée ou un `amount` nul, et aucun des trois ne dépend du
    /// taux ; seule la longueur de la ligne à retard en dépend.
    pub(super) fn zone_has_active_crossfeed(&self, zone_id: i64) -> bool {
        self.load_crossfeed_processor(zone_id, 44100).is_some()
    }

    /// Profil EQ réellement actif pour une zone, sans encore le lier à un
    /// format PCM. Le décodeur radio ne connaît le taux et le nombre de canaux
    /// qu'après sa sonde ; lui transmettre le profil permet de construire les
    /// coefficients exacts à cet instant au lieu de supposer 44,1 kHz (#2063).
    pub(super) fn load_eq_profile(&self, zone_id: i64) -> Option<crate::audio::eq::EqProfile> {
        // PURE mode: never build an EqProcessor so the PCM reaches the output
        // untouched.
        if self.zone_audiophile(zone_id) {
            return None;
        }
        self.eq_profile_configure(zone_id)
    }

    /// Le profil d'égaliseur ACTIVÉ de la zone, **sans regarder le mode
    /// PURE** (#4004).
    ///
    /// C'est la lecture que [`Self::load_eq_profile`] fait derrière sa garde
    /// PURE, sortie d'elle pour qu'une seule fonction lise la clé. Elle sert à
    /// répondre à une autre question que « faut-il filtrer ? » : « la bascule
    /// PURE changerait-elle quelque chose ? » — ce qui suppose de voir le
    /// réglage que PURE cache. `None` si la clé est absente, illisible ou si
    /// le profil est désactivé.
    pub(super) fn eq_profile_configure(&self, zone_id: i64) -> Option<crate::audio::eq::EqProfile> {
        if !crate::audio::premium_plugins::enabled(
            &crate::db::settings_repo::SettingsRepo::with_backend(self.db.clone()),
            "equalizer",
        ) {
            return None;
        }
        let settings = crate::db::settings_repo::SettingsRepo::with_backend(self.db.clone());
        let key = format!("zone_{zone_id}_eq_profile");
        let profile: crate::audio::eq::EqProfile = settings
            .get(&key)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())?;
        if !profile.enabled {
            return None;
        }
        Some(profile)
    }

    /// Quel traitement, parmi ceux que le mode PURE gouverne sur un flux
    /// TRANSCODÉ, est réellement armé sur cette zone — PURE ou pas (#4004).
    ///
    /// Sur une sortie réseau ou navigateur, PURE n'a qu'un seul effet sur les
    /// octets servis : il fait rendre `None` aux quatre chargeurs que la
    /// résolution consulte — égaliseur ([`Self::load_eq_profile`]),
    /// correction de pièce ([`Self::chemin_ir`]), crossfeed
    /// ([`Self::load_crossfeed_processor`]) et ReplayGain
    /// ([`Self::zone_replaygain_changes_audio`], `load_streaming_dsp`, bras
    /// fichier de `resolve_local.rs`). Le repli mono, la rampe et le verrou de
    /// volume ne touchent que la sortie LOCALE ou la commande de volume, pas le
    /// fichier. Comparer la résolution « avant » et « après » une bascule PURE
    /// revient donc exactement à demander si l'un de ces quatre réglages est
    /// configuré : s'il ne l'est pas, les deux résolutions produisent les mêmes
    /// octets, et refabriquer le flux ne fait que couper le son (Jean Valjean,
    /// Marantz ND8006 : 1,8 à 2,4 s de silence par bascule, sans aucun
    /// traitement armé).
    ///
    /// Le crossfeed est compté même quand le bras fichier ne le cuit pas
    /// (il ne l'est que sur le bras progressif, `dsp_progressif_reseau`) :
    /// l'approximation va dans le sens sûr — au pire une relance qui aurait pu
    /// être évitée, jamais une bascule qui n'atteint pas le son.
    ///
    /// Rend le NOM du premier traitement armé, pour le journal, ou `None`
    /// quand la bascule ne change rien au signal effectif.
    pub(super) fn traitement_que_pure_gouverne(
        &self,
        zone_id: i64,
        track_id: Option<i64>,
    ) -> Option<&'static str> {
        if self
            .eq_profile_configure(zone_id)
            .is_some_and(|p| crate::audio::eq::EqProcessor::new(&p, 44100, 2).is_enabled())
        {
            return Some("egaliseur");
        }
        if self
            .chemin_ir_configure(zone_id)
            .is_some_and(|p| std::path::Path::new(&p).exists())
        {
            return Some("correction_de_piece");
        }
        if self.crossfeed_configure(zone_id).is_some() {
            return Some("crossfeed");
        }
        if track_id.is_some_and(|tid| {
            (crate::audio::replaygain::playback_factor(&self.db, tid) - 1.0).abs() > 1e-6
        }) {
            return Some("replaygain");
        }
        None
    }

    /// Build the room-correction FIR convolver for a zone's TRANSCODED stream,
    /// or `None`. Symmetric to `load_eq_processor`: PURE (audiophile) mode →
    /// `None`; otherwise load the uploaded IR (`ir_path_{zone}`) for the
    /// stream's sample rate + channel count. Applied in `transcode_source_to_file`
    /// after the EQ, so it colours the bytes served to a network renderer.
    /// Le fichier de réponse impulsionnelle de la zone (`ir_path_{zone}`),
    /// `None` en mode PURE ou sans réglage. Une seule lecture du réglage pour
    /// le convolveur et pour l'empreinte du cache (LAT-F2).
    pub(super) fn chemin_ir(&self, zone_id: i64) -> Option<String> {
        if self.zone_audiophile(zone_id) {
            return None;
        }
        self.chemin_ir_configure(zone_id)
    }

    /// Le chemin de réponse impulsionnelle CONFIGURÉ, sans regarder le mode
    /// PURE (#4004) — la lecture de [`Self::chemin_ir`] derrière sa garde,
    /// partagée avec [`Self::zone_has_active_ir`] et
    /// [`Self::traitement_que_pure_gouverne`]. `None` si la clé est absente ou
    /// vide ; l'existence du fichier n'est PAS vérifiée ici.
    pub(super) fn chemin_ir_configure(&self, zone_id: i64) -> Option<String> {
        crate::db::settings_repo::SettingsRepo::with_backend(self.db.clone())
            .get(&format!("ir_path_{zone_id}"))
            .ok()
            .flatten()
            .filter(|p| !p.is_empty())
    }

    /// Les octets de la réponse impulsionnelle, pour l'empreinte du cache de
    /// transcodage : c'est le CONTENU qui décide de la rendition, pas le nom
    /// du fichier. `None` si la zone n'en a pas ou s'il est illisible.
    pub(super) fn octets_ir(&self, zone_id: i64) -> Option<Vec<u8>> {
        std::fs::read(self.chemin_ir(zone_id)?).ok()
    }

    pub(super) fn load_convolver(
        &self,
        zone_id: i64,
        sample_rate: u32,
        channels: u16,
    ) -> Option<crate::audio::convolver::Convolver> {
        let path = self.chemin_ir(zone_id)?;
        match crate::audio::convolver::Convolver::from_wav_for(
            &path,
            1024,
            sample_rate,
            channels as usize,
        ) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(zone_id, path, error = %e, "room_correction_ir_load_failed");
                None
            }
        }
    }

    /// Build the headphone crossfeed processor for a zone's LOCAL output, or
    /// `None` when it should not run. Symmetric to `load_eq_processor`:
    ///
    ///   - PURE (audiophile) mode → `None` (bit-perfect path, no coloration).
    ///   - crossfeed `enabled == false` (the default) → `None`.
    ///   - `amount == 0` → `None` (would be a pure identity anyway).
    ///
    /// Config lives in the settings key `zone_{id}_crossfeed` as JSON
    /// `{ "enabled": bool, "amount": f32, "delay_ms": f32 }`. Values are clamped
    /// defensively (amount 0..0.5, delay_ms 0..5) mirroring the route validation.
    pub(super) fn load_crossfeed_processor(
        &self,
        zone_id: i64,
        sample_rate: u32,
    ) -> Option<crate::audio::crossfeed::CrossfeedProcessor> {
        // PURE mode: no crossfeed, keep the signal path bit-perfect.
        if self.zone_audiophile(zone_id) {
            return None;
        }
        let (amount, delay_ms) = self.crossfeed_configure(zone_id)?;
        Some(crate::audio::crossfeed::CrossfeedProcessor::new(
            sample_rate,
            amount,
            delay_ms,
        ))
    }

    /// Le crossfeed CONFIGURÉ de la zone — `(amount, delay_ms)` bornés — sans
    /// regarder le mode PURE (#4004). C'est la lecture de
    /// [`Self::load_crossfeed_processor`] derrière sa garde, sortie d'elle pour
    /// que [`Self::traitement_que_pure_gouverne`] lise la même clé avec les
    /// mêmes bornes. `None` sur la case décochée, une clé absente ou illisible,
    /// ou un `amount` nul (identité).
    pub(super) fn crossfeed_configure(&self, zone_id: i64) -> Option<(f32, f32)> {
        if self
            .license
            .as_ref()
            .is_some_and(|license| !license.premium_snapshot())
        {
            return None;
        }
        if !crate::audio::premium_plugins::enabled(
            &crate::db::settings_repo::SettingsRepo::with_backend(self.db.clone()),
            "crossfeed",
        ) {
            return None;
        }
        let settings = crate::db::settings_repo::SettingsRepo::with_backend(self.db.clone());
        let cfg: serde_json::Value = settings
            .get(&format!("zone_{zone_id}_crossfeed"))
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())?;
        if !cfg
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return None;
        }
        // Mêmes bornes que la route et les préréglages : une seule définition
        // (`audio::crossfeed::borner`, #4683).
        let (amount, delay_ms) = crate::audio::crossfeed::borner(
            cfg.get("amount").and_then(|v| v.as_f64()).unwrap_or(0.30),
            cfg.get("delay_ms").and_then(|v| v.as_f64()).unwrap_or(0.30),
        );
        if amount == 0.0 {
            return None;
        }
        Some((amount as f32, delay_ms as f32))
    }

    /// #4685 — la compensation de niveau est ACTIVE par défaut.
    ///
    /// Pourquoi « oui » :
    ///
    /// 1. c'est l'objet même de #4685 — à volume égal, on juge le réglage,
    ///    pas le fait que plus fort sonne mieux ; une comparaison honnête
    ///    qu'il faudrait d'abord aller chercher dans un réglage ne sert pas ;
    /// 2. elle répond à la plainte la plus fréquente sur l'égaliseur — « pas
    ///    de son quand j'active l'égaliseur » (#1640), « il baisse le volume
    ///    au minimum » (#4407) : la réserve anti-écrêtage, indispensable,
    ///    faisait perdre jusqu'à 10 dB de niveau moyen ;
    /// 3. elle ne peut ni écrêter ni dépasser : elle passe par le volume
    ///    effectif, raboté à l'unité APRÈS le DSP, donc jamais plus fort que
    ///    la même zone sans égaliseur ni crossfeed au même curseur.
    ///
    /// Le prix, assumé : un auditeur qui avait remonté son curseur à la main
    /// pour rattraper la réserve entendra, à la mise à jour, un niveau plus
    /// élevé qu'hier — jamais plus que sans égaliseur. L'interrupteur existe
    /// pour lui.
    pub const COMPENSATION_DE_NIVEAU_PAR_DEFAUT: bool = true;

    /// La clé de réglage de l'interrupteur, une seule écriture pour
    /// l'orchestrateur et la route.
    pub fn cle_compensation_de_niveau(zone_id: i64) -> String {
        format!("zone_{zone_id}_level_compensation")
    }

    /// #4685 — la zone demande-t-elle la compensation de niveau de son
    /// égaliseur et de son crossfeed ?
    ///
    /// Clé [`Self::cle_compensation_de_niveau`], **vraie par défaut** :
    /// absente, vide ou illisible, elle compense. Seul `"false"` l'éteint.
    /// Le choix du défaut est dit dans
    /// [`Self::COMPENSATION_DE_NIVEAU_PAR_DEFAUT`].
    pub fn zone_compensation_de_niveau(&self, zone_id: i64) -> bool {
        Self::zone_compensation_de_niveau_with(&self.db, zone_id)
    }

    /// Même règle, lisible sans orchestrateur — c'est par là que la route
    /// `GET /zones/{id}/dsp` publie l'interrupteur.
    pub fn zone_compensation_de_niveau_with(
        db: &std::sync::Arc<dyn crate::db::backend::DbBackend>,
        zone_id: i64,
    ) -> bool {
        match crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
            .get(&Self::cle_compensation_de_niveau(zone_id))
            .ok()
            .flatten()
            .as_deref()
        {
            Some("false") => false,
            Some("true") => true,
            _ => Self::COMPENSATION_DE_NIVEAU_PAR_DEFAUT,
        }
    }

    /// #4685 — ce que l'égaliseur et le crossfeed de la zone font au niveau
    /// MOYEN, en dB, `(égaliseur, crossfeed)` — tels que la sortie locale les
    /// installerait, gardes comprises (PURE, droits, greffon, case décochée) :
    /// ce sont les MÊMES chargeurs que la lecture. Sondés à 44,1 kHz stéréo,
    /// le débit de référence du reste de l'écran ; le débit réel ne déplace
    /// ces nombres que de quelques centièmes de dB.
    pub fn gain_moyen_du_dsp_de_zone(&self, zone_id: i64) -> (f64, f64) {
        let eq = self
            .load_eq_processor(zone_id, 44_100, 2)
            .map(|p| p.gain_moyen_db())
            .unwrap_or(0.0);
        let cf = self
            .load_crossfeed_processor(zone_id, 44_100)
            .map(|p| p.gain_moyen_db())
            .unwrap_or(0.0);
        (eq, cf)
    }

    /// #4685 — repousser l'interrupteur de compensation à la sortie locale
    /// qui joue, sans attendre la piste suivante. Jumeau de
    /// [`Self::refresh_zone_mono_downmix`] : rend `true` quand une sortie
    /// locale vivante l'a reçu.
    pub async fn refresh_zone_compensation(&self, zone_id: i64) -> bool {
        #[cfg(not(feature = "local-audio"))]
        {
            let _ = zone_id;
            false
        }
        #[cfg(feature = "local-audio")]
        {
            let Some(device_id) = ZoneRepo::with_backend(self.db.clone())
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id)
            else {
                return false;
            };
            if !device_id.starts_with("local:") {
                return false;
            }
            let Some(output_arc) = ({ self.outputs.lock().await.get(&device_id) }) else {
                return false;
            };
            let output = output_arc.lock().await;
            let Some(local_output) = output
                .as_any()
                .downcast_ref::<crate::outputs::local::LocalOutput>()
            else {
                return false;
            };
            let active = self.zone_compensation_de_niveau(zone_id);
            local_output.set_compensation_de_niveau(active);
            info!(zone_id, device_id = %device_id, active, "zone_compensation_refreshed_live");
            true
        }
    }

    /// La zone demande-t-elle le repli mono sur sa sortie LOCALE ? (#2362)
    ///
    /// Symétrique de `Self::load_crossfeed_processor` :
    ///
    ///   - mode PURE (audiophile) → `false` (chemin bit-perfect, intouché) ;
    ///   - réglage absent, vide, ou différent de `"true"` → `false` (défaut).
    ///
    /// Le réglage vit dans la clé `zone_{id}_mono_downmix`, écrite par
    /// `PATCH /zones/{id}` — même forme que `zone_{id}_upnp_renderer` : la clé
    /// est SUPPRIMÉE quand l'utilisateur désactive, jamais mise à `"false"`.
    ///
    /// Public : `tune-server` le relit pour composer le chemin du signal, afin
    /// que le panneau et le son répondent à la MÊME question — c'est la leçon
    /// de #1548/#1559 (EQ oublié du verdict) et de #1627 (ReplayGain).
    pub fn zone_mono_downmix(&self, zone_id: i64) -> bool {
        Self::zone_mono_downmix_with(&self.db, zone_id)
    }

    /// Même règle, lisible sans orchestrateur — c'est par là que le serveur
    /// compose le chemin du signal.
    pub fn zone_mono_downmix_with(
        db: &std::sync::Arc<dyn crate::db::backend::DbBackend>,
        zone_id: i64,
    ) -> bool {
        // PURE : le PCM atteint la sortie intact, aucun repli n'est appliqué.
        if crate::audio::audiophile::zone_enabled(db, zone_id) {
            return false;
        }
        crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
            .get(&format!("zone_{zone_id}_mono_downmix"))
            .ok()
            .flatten()
            .as_deref()
            == Some("true")
    }

    /// Réappliquer le repli mono d'une zone à la sortie locale qui joue, sans
    /// attendre la piste suivante.
    ///
    /// Jumeau de [`Self::refresh_zone_crossfeed`], pour le même défaut : sans
    /// lui, cocher la case en écoutant persisterait le réglage, renverrait un
    /// succès, et ne changerait rien avant la piste suivante (#1725, #1786).
    /// Or c'est exactement ainsi qu'on vérifie ce réglage-ci : une seule
    /// enceinte, on coche, et on doit entendre revenir ce qui était panné à
    /// droite.
    ///
    /// Contrairement au crossfeed, il n'y a **pas** de garde sur
    /// `current_format()` : le repli n'a aucun filtre à bâtir pour un taux
    /// donné, donc rien à faire dépendre d'un flux en cours. Armer le drapeau
    /// sur une sortie silencieuse est correct et évite de perdre le réglage.
    ///
    /// Renvoie `true` si le drapeau a été poussé vers une sortie locale vivante.
    pub async fn refresh_zone_mono_downmix(&self, zone_id: i64) -> bool {
        #[cfg(not(feature = "local-audio"))]
        {
            let _ = zone_id;
            false
        }
        #[cfg(feature = "local-audio")]
        {
            let Some(device_id) = ZoneRepo::with_backend(self.db.clone())
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id)
            else {
                return false;
            };
            if !device_id.starts_with("local:") {
                return false;
            }
            let Some(output_arc) = ({ self.outputs.lock().await.get(&device_id) }) else {
                return false;
            };
            let output = output_arc.lock().await;
            let Some(local_output) = output
                .as_any()
                .downcast_ref::<crate::outputs::local::LocalOutput>()
            else {
                return false;
            };
            let mono = self.zone_mono_downmix(zone_id);
            local_output.set_mono_downmix(mono);
            info!(
                zone_id,
                device_id = %device_id,
                mono,
                "zone_mono_downmix_refreshed_live"
            );
            true
        }
    }

    pub async fn set_volume(
        &self,
        zone_id: i64,
        volume: f64,
        device_id: Option<&str>,
    ) -> OutputCommandResult<()> {
        // When fixed_volume is enabled, pin volume to 1.0 (bit-perfect) and
        // skip sending to the device — the DAC/renderer handles volume.
        let zone = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten();
        if zone.as_ref().is_some_and(|z| z.fixed_volume) {
            self.playback.set_volume(zone_id, 1.0).await;
            ZoneRepo::with_backend(self.db.clone())
                .update_volume(zone_id, 100.0)
                .map_err(|message| OutputCommandError::failed(OutputCommand::SetVolume, message))?;
            return Ok(());
        }

        // Trim de gain par renderer (setting `zone_{id}_gain_trim_db`, ±12 dB) :
        // composé UNIQUEMENT dans la valeur envoyée au device. Le volume
        // affiché/persisté reste celui de l'utilisateur, le cache de
        // transcodage n'est pas affecté (rien n'est cuit dans le PCM), et les
        // zones fixed_volume ne passent jamais ici (early return ci-dessus).
        // Limite assumée : un trim positif est plafonné quand user_volume est
        // déjà haut (clamp 0..1).
        let device_volume = {
            let trim_db = crate::db::settings_repo::SettingsRepo::with_backend(self.db.clone())
                .get(&format!("zone_{zone_id}_gain_trim_db"))
                .ok()
                .flatten()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0);
            (volume * gain_trim_factor(trim_db)).clamp(0.0, 1.0)
        };
        if let Some(did) = device_id {
            let output = { self.outputs.lock().await.get(did) }.ok_or_else(|| {
                OutputCommandError::failed(
                    OutputCommand::SetVolume,
                    format!("output {did} is not registered"),
                )
            })?;
            info!(
                zone_id,
                volume,
                device_volume,
                device_id = did,
                "device_set_volume_sending"
            );
            if let Err(error) = output.lock().await.checked_set_volume(device_volume).await {
                warn!(zone_id, error = %error, "device_set_volume_failed");
                if let Some(ref bus) = self.event_bus {
                    bus.emit(
                        "zone.playback_error",
                        serde_json::json!({
                            "zone_id": zone_id,
                            "error": error.to_string(),
                        }),
                    );
                }
                return Err(error);
            }
        } else {
            info!(zone_id, volume, "set_volume_no_device_id");
        }

        // Le backend a accepté la commande : seulement maintenant les deux
        // copies internes et la base peuvent annoncer la nouvelle valeur.
        self.playback.set_volume(zone_id, volume).await;
        self.playback.mark_volume_changed(zone_id).await;
        ZoneRepo::with_backend(self.db.clone())
            // #2886 — plus d'arrondi a l'entier : il coutait 3 dB vers
            // -37 dB et COUPAIT le son sous 0,005 lineaire (-46,0205999133 dB).
            .update_volume(zone_id, volume.clamp(0.0, 1.0) * 100.0)
            .map_err(|message| OutputCommandError::failed(OutputCommand::SetVolume, message))?;
        Ok(())
    }

    /// Arme le volume fixe : commande le plein volume au périphérique, **une
    /// seule fois** (#2395).
    ///
    /// C'est le seul chemin autorisé à commander une zone `fixed_volume` :
    /// [`Self::set_volume`] sort au plus tôt pour ces zones et ne parle jamais
    /// au device. L'appelant est la route qui écrit `fixed_volume` (PATCH
    /// `/zones/{id}`), APRÈS que la confirmation explicite a été obtenue —
    /// jamais la lecture, qui ne commande plus rien.
    ///
    /// Le trim de gain par renderer n'est délibérément pas composé ici : le
    /// mode promet du bit-perfect, et `1.0` doit rester `1.0`.
    ///
    /// L'ordre suit celui de `set_volume` : le device d'abord, la base et
    /// l'état interne seulement s'il a accepté. Une zone sans sortie
    /// enregistrée n'est pas une erreur — le 100 % est alors seulement
    /// persisté, et la sortie le recevra à son enregistrement.
    pub async fn arm_fixed_volume(
        &self,
        zone_id: i64,
        device_id: Option<&str>,
    ) -> OutputCommandResult<()> {
        if let Some(did) = device_id {
            let output = { self.outputs.lock().await.get(did) };
            match output {
                Some(output) => {
                    info!(zone_id, device_id = did, "fixed_volume_arm_sending");
                    if let Err(error) = output.lock().await.checked_set_volume(1.0).await {
                        warn!(zone_id, error = %error, "fixed_volume_arm_failed");
                        return Err(error);
                    }
                }
                // Sortie déclarée mais pas encore enregistrée : rien à
                // commander, et surtout rien à refuser — la zone reste armée.
                None => info!(zone_id, device_id = did, "fixed_volume_arm_no_output"),
            }
        } else {
            info!(zone_id, "fixed_volume_arm_no_device_id");
        }

        self.playback.set_volume(zone_id, 1.0).await;
        self.playback.mark_volume_changed(zone_id).await;
        ZoneRepo::with_backend(self.db.clone())
            .update_volume(zone_id, 100.0)
            .map_err(|message| OutputCommandError::failed(OutputCommand::SetVolume, message))?;
        Ok(())
    }

    /// Cette sortie produit-elle des niveaux exploitables ?
    ///
    /// `false` sur le seul chemin qui n'en produit aucun : OAAT en DSD natif,
    /// où la sortie ouvre le `.dsf` elle-même et expédie du 1 bit sans que
    /// personne ne décode. Les VU-mètres n'y reçoivent rien — l'aiguille reste
    /// où elle est, ce qui se lit comme une panne alors que c'est une absence
    /// de mesure. Le client ne peut pas deviner la différence entre « pas de
    /// niveaux » et « des niveaux qui tardent » : c'est au serveur de le dire.
    ///
    /// Rendre du DSD en PCM en parallèle rien que pour animer deux aiguilles
    /// coûterait, pendant l'écoute, exactement le décodage qu'on a retiré de
    /// ce chemin (blocage Zicmu, `dsd_streaming_send_timeout`).
    pub async fn output_produces_levels(&self, device_id: Option<&str>) -> bool {
        let Some(device_id) = device_id else {
            return true;
        };
        #[cfg(feature = "oaat")]
        if device_id.starts_with("oaat:") {
            let arc = { self.outputs.lock().await.get(device_id) };
            if let Some(arc) = arc {
                let output = arc.lock().await;
                if let Some(oaat) = output
                    .as_any()
                    .downcast_ref::<crate::outputs::oaat::OaatOutput>()
                {
                    return !oaat.is_native_dsd_active();
                }
            }
        }
        true
    }
}
