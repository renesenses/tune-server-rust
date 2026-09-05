use super::*;

/// Ce que `resolve_local_track` a décidé avant de servir : formats, forçages,
/// transcodage requis, et les données de la piste et de la zone que les deux
/// armes relisent (REF-2 phase 2, #2219). Les champs portent les noms des
/// `let` d'origine ; les armes les destructurent et ne changent pas.
struct DecisionLocale {
    bit_depth: u16,
    bit_depth_wire: u16,
    browser_needs_wav: bool,
    channels: u16,
    dlna_cap_16bit: bool,
    dlna_needs_wav: bool,
    dlna_wav24: bool,
    eq_forces_transcode: bool,
    is_browser_output: bool,
    is_chromecast: bool,
    is_local_output: bool,
    is_network_output: bool,
    local_needs_wav: bool,
    needs_downsample: bool,
    needs_transcode_for_output: bool,
    oaat_needs_wav: bool,
    sample_rate: u32,
    source_format: Option<AudioFormat>,
    track_id: i64,
    zone_max_sample_rate: Option<u32>,
    track_duration_ms: i64,
    track_file_size: Option<i64>,
    file_path: String,
    fmt: String,
    zone: Option<crate::db::zone_repo::Zone>,
    needs_transcode: bool,
}

/// Ce que les décisions rendent : la décision à exécuter, ou une résolution
/// déjà complète (DoP servi directement) (REF-2 phase 2, #2219).
enum DecisionOuResolu {
    Decision(DecisionLocale),
    Resolu(ResolvedStream),
}

/// Le grand tuple rendu par les bras de `resolve_local_track` : identifiant
/// de session, type MIME, extension, taille, fréquence, profondeur, canaux.
type FluxLocal = (
    String,
    String,
    String,
    Option<u64>,
    Option<u32>,
    Option<u32>,
    Option<u32>,
);

/// Ce que le premier temps du transcodage a décidé du format de sortie,
/// avant de choisir entre le fichier pré-transcodé et la session à la volée.
struct FormatDeSortie {
    out_sr: u32,
    out_bd: u16,
    out_mime: String,
    out_ext: String,
    target_format_str: String,
    use_file_transcode: bool,
    info: StreamInfo,
}

impl PlaybackOrchestrator {
    /// Faut-il envoyer le DSD tel quel au renderer ?
    ///
    /// Fonction PURE : le mode reglé, et ce que le sondage a repondu —
    /// `Some(true)` / `Some(false)` sur une reponse concluante, `None` sinon. La
    /// sonde reseau vit dans `sonder_dsd` ; ici il n'y a que la decision, donc
    /// elle se teste.
    ///
    /// La subtilite est le troisieme cas. `None` ne veut PAS dire « non » : des
    /// renderers lisent le DSD sans l'annoncer dans leur GetProtocolInfo.
    /// Ecraser un reglage explicite sur une absence de reponse priverait de DSD
    /// natif des gens qui l'avaient — la faute symetrique de celle qu'on
    /// corrige (#2122).
    pub(crate) fn decider_passthrough_dsd(mode: &str, annonce: Option<bool>) -> bool {
        match mode {
            "pcm" => false,
            // `dop` n'est pas du passthrough : le renderer doit recevoir le DSD
            // emballe en trames PCM, pas le .dsf brut.
            "dop" => false,
            // Choix explicite : la parole de l'utilisateur, TOUJOURS.
            //
            // La version precedente cedait devant un « non » du Sink
            // (`annonce != Some(false)`, #2122). Le terrain l'a dementie :
            // l'Eversolo DMP-A8 annonce 392 formats dans son GetProtocolInfo,
            // AUCUN DSD — et joue le .dsf brut qu'on lui envoie. Un Sink qui
            // omet un format n'est pas un refus, meme quand il a l'air
            // exhaustif : une absence n'est pas une preuve.
            //
            // `native` n'est pas un reglage d'usine : quelqu'un l'a choisi,
            // pour CE renderer. Si la zone reste muette, c'est ce reglage
            // qu'il faut changer — et le journal le dit — pas le serveur qui
            // decide en silence de convertir.
            "native" => true,
            // `auto` : sans reponse claire, on prend le chemin sur.
            _ => annonce.unwrap_or(false),
        }
    }

    /// Le réglage DSD de la zone et ce que le lecteur en avait dit.
    ///
    /// Relu sur le chemin d'ERREUR seulement, et sans jamais resonder le
    /// réseau : le sondage a déjà eu lieu avant l'envoi ([`Self::sonder_dsd`])
    /// et seul un sondage CONCLUANT entre en cache. Un cache vide rend donc
    /// `None` — « je ne sais pas », jamais « non ». C'est précisément ce qui
    /// empêche d'imputer un échec à un réglage sur une absence de réponse.
    pub(super) async fn contexte_dsd(
        &self,
        zone_id: i64,
        device_id: &str,
    ) -> (String, Option<bool>) {
        let mode = ZoneRepo::with_backend(self.db.clone()).get_dsd_mode(zone_id);
        let annonce = self
            .dsd_capabilities
            .lock()
            .await
            .get(device_id)
            .map(|cap| cap.supports_dsf || cap.supports_dff);
        (mode, annonce)
    }

    pub(super) async fn should_dsd_passthrough(&self, zone_id: i64, device_id: &str) -> bool {
        let dsd_mode = ZoneRepo::with_backend(self.db.clone()).get_dsd_mode(zone_id);
        // Le sondage n'a de sens que si la decision peut en dependre : `pcm` et
        // `dop` tranchent sans lui, inutile d'aller sur le reseau.
        let annonce = match dsd_mode.as_str() {
            "pcm" | "dop" => None,
            _ => self.sonder_dsd(device_id).await,
        };
        let passthrough = Self::decider_passthrough_dsd(&dsd_mode, annonce);
        // La ligne qui manquait : ce qui part VRAIMENT sur le fil. Sans elle, le
        // seul événement DSD du journal était celui du DoP, qui ne décide de
        // rien sur une sortie réseau (#2122).
        info!(
            zone_id,
            device_id,
            dsd_mode = %dsd_mode,
            annonce_du_renderer = ?annonce,
            passthrough,
            "dsd_passthrough_decide"
        );
        if dsd_mode == "native" && annonce == Some(false) {
            tracing::info!(
                zone_id,
                device_id,
                "dsd_natif_sans_annonce_du_renderer — le Sink n'annonce pas de \
                 DSD, on envoie le flux brut QUAND MEME : « natif » est un \
                 reglage explicite, et des renderers lisent le DSD sans \
                 l'annoncer (Eversolo DMP-A8). Si la zone reste muette, passer \
                 le mode DSD de la zone en « auto » ou « pcm »."
            );
        }
        passthrough
    }

    /// Le renderer annonce-t-il savoir lire du DSD ?
    ///
    /// `Some(true)` / `Some(false)` sur un sondage CONCLUANT, `None` sinon —
    /// et la distinction compte : `None` ne veut pas dire « non ».
    ///
    /// Extrait pour etre partage par `native` et `auto`. Les deux posaient la
    /// meme question ; un seul la posait.
    pub(super) async fn sonder_dsd(&self, device_id: &str) -> Option<bool> {
        // Seul un sondage CONCLUANT entre en cache. `probe_dsd_support` rend
        // `None` quand GetProtocolInfo a echoue ou que le Sink etait vide, et un
        // appareil qui n'est pas une sortie DLNA (ou qui a quitte la table)
        // n'est pas plus concluant. Mettre ces cas en cache epinglerait
        // l'appareil sur « pas de DSD » pour toute la vie du processus — un
        // echec passager juste apres la decouverte forcerait silencieusement le
        // transcodage PCM sur un renderer qui lit le DSD nativement, sans
        // recours autre qu'un redemarrage. Meme regle que `DlnaOutput::supports_mime`.
        let mut cache = self.dsd_capabilities.lock().await;
        if let Some(cap) = cache.get(device_id) {
            return Some(cap.supports_dsf || cap.supports_dff);
        }
        let cap = {
            let arc = { self.outputs.lock().await.get(device_id) };
            match arc {
                Some(output) => {
                    let locked = output.lock().await;
                    match locked
                        .as_any()
                        .downcast_ref::<crate::outputs::dlna::DlnaOutput>()
                    {
                        Some(dlna) => dlna.probe_dsd_support().await,
                        None => None,
                    }
                }
                None => None,
            }
        };
        cap.map(|cap| {
            let resultat = cap.supports_dsf || cap.supports_dff;
            cache.insert(device_id.to_string(), cap);
            resultat
        })
    }

    pub(super) async fn resolve_local_track(
        &self,
        req: &PlayRequest,
    ) -> Result<ResolvedStream, String> {
        let track_id = req.track_id.ok_or("no track_id for local playback")?;
        let repo = TrackRepo::with_backend(self.db.clone());
        let mut track = repo
            .get(track_id)
            .map_err(|e| e.to_string())?
            .ok_or("track not found")?;

        let file_path = track.file_path.clone().ok_or("track has no file_path")?;

        // The DB row can outlive the file (moved/deleted external drive, stale
        // scan, duplicate compilation entry pointing at an old path). Without
        // this check the missing file is only discovered later, inside the
        // spawned streaming transcode task (transcode_streaming_decode_failed),
        // AFTER output_play_sent — so the track "plays" silently with no error
        // surfaced and the queue can stall (JP: two "Studio 105" entries, the
        // one pointing at a moved X:\…\.flac played no sound). Fail fast here so
        // play() returns a clean error the client shows, instead of streaming
        // silence.
        //
        // The DB stores paths NFC-normalized (scanner), but a file is opened by
        // its raw on-disk bytes: on a Samba/CIFS or macOS-origin share whose
        // filenames are NFD (decomposed), the stored NFC path misses the real
        // file and a present, listable track reads as "missing" (Dominique
        // Comet, 0.9.48 after a rescan rewrote paths to NFC). Resolve the true
        // on-disk spelling (stored form, then NFD) before giving up.
        let file_path = match resolve_existing_local_path(&file_path) {
            Some(resolved) => resolved,
            None => {
                warn!(track_id, file = %file_path, "local_track_file_missing");
                return Err(format!("file_not_found:{file_path}"));
            }
        };

        // Le format que Tune ne sait pas rendre doit se DIRE, pas se taire.
        //
        // Même raison que `file_not_found:` juste au-dessus, et même endroit :
        // sans garde ici, le défaut n'apparaît qu'après `output_play_sent`, dans
        // la tâche de transcodage détachée — trop tard pour que la route en dise
        // quoi que ce soit, et le morceau « joue » en silence.
        //
        // Pour un ISO SACD c'est pire qu'un silence de plus : `from_extension`
        // rend `None` pour `iso`, `needs_transcode` retombe sur
        // `unwrap_or(AudioFormat::Flac)`, et une image disque part sur le fil
        // comme si c'était du FLAC. Le geste attendu n'est pas une relance ni un
        // nouveau parcours : il faut un outil que Tune ne fournit pas — 22 albums
        // chez JeromeQ pour la seule trace d'un `warn!` (#3234, fil 1206).
        //
        // Ce refus ne fabrique PAS un quatrième canal d'état : il emprunte la
        // sentinelle que `play_error_response` sait déjà nommer, comme
        // `file_not_found:`, et il répète le motif que le rapport de parcours
        // affiche depuis #2992.
        if let Some(motif) =
            crate::audio::iso_sacd::refus_de_lecture(std::path::Path::new(&file_path))
        {
            warn!(track_id, file = %file_path, motif, "local_track_format_not_playable");
            return Err(format!("format_not_playable:{motif}"));
        }
        let fmt = track.format.clone().unwrap_or_else(|| "flac".into());
        let source_format = AudioFormat::from_extension(&fmt);
        // DSD is 1-bit at MHz rates. When the DB row is missing audio props
        // (lofty returns None for many .dsf/.dff files), fall back to DSD64
        // defaults, NOT the PCM 44100/16 defaults — otherwise a native-DSD track
        // played to a DSD-capable renderer shows "44.1 kHz / 16 bit" in the
        // signal path / now-playing chip (Benjithom, HiFi Rose RS130), and the
        // DSD→PCM transcode-fallback rate math is fed the wrong input rate.
        let is_dsd_source = source_format == Some(AudioFormat::Dsd);

        // Play-time duration backfill. A scan that timed out on slow storage
        // (NAS: Pierre M, Yacine) falls back to filename-only metadata with
        // duration_ms = 0, and DSD/other files lofty can't read a duration for
        // also land at 0 in the DB. A 0 is quietly corrosive: the poller reads
        // now_playing.duration_ms (= the DB value) verbatim, and at 0 it loses
        // gapless arming, the position-past-end fast advance, BOTH wall-clock
        // advance nets, prefetch AND crossfade — so the queue stalls or cuts on
        // that track. Recover the real duration now and persist it so the track
        // self-heals for every later read. DSD is read from the header because
        // lofty (which get_duration uses) is exactly what returned 0 for it.
        if track.duration_ms <= 0 {
            if let Some(ms) = probe_local_duration_ms(&file_path, source_format).await {
                track.duration_ms = ms;
                let repo2 = TrackRepo::with_backend(self.db.clone());
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = repo2.update_duration(track_id, ms) {
                        warn!(track_id, error = %e, "play_time_duration_persist_failed");
                    }
                });
                info!(track_id, duration_ms = ms, "play_time_duration_backfilled");
            }
        }

        let decision = match self
            .decider_la_lecture_locale(
                req,
                &track,
                track_id,
                file_path.clone(),
                fmt.clone(),
                source_format,
                is_dsd_source,
            )
            .await?
        {
            DecisionOuResolu::Resolu(resolu) => return Ok(resolu),
            DecisionOuResolu::Decision(decision) => decision,
        };
        let (
            session_id,
            out_mime,
            out_ext,
            resolved_file_size,
            resolved_sr,
            resolved_bd,
            resolved_ch,
        ) = if decision.needs_transcode {
            self.transcoder_la_piste(req, &decision).await?
        } else {
            self.servir_en_passthrough(req, &decision).await?
        };
        let DecisionLocale {
            is_network_output,
            needs_transcode,
            ..
        } = decision;

        let server_ip = self.server_ip();
        let stream_url = self
            .streamer
            .get_stream_url(&session_id, &server_ip, &out_ext);

        // For a transcoded WAV/LPCM stream served with an exact byte length
        // (the file-transcode path pre-encodes the whole WAV, so file_size is
        // the real body size), advertise a DIDL `res@duration` derived from
        // that byte length instead of the scanned `track.duration_ms`. The two
        // can disagree by a few seconds (the FLAC STREAMINFO/scan duration vs.
        // the actual decoded sample count), and when the DIDL duration is
        // LONGER than the bytes the renderer receives, some renderers (Marantz
        // ND 8006) reach EOF, see position < advertised duration, and
        // restart/loop the track near the end instead of advancing (#1132).
        // Computing duration from size/byte_rate keeps duration and size
        // mathematically consistent, so the progress bar tracks correctly and
        // the track advances cleanly. Only applies when we know the exact size
        // AND the audio params; otherwise fall back to the scanned duration.
        let didl_duration_ms = if out_mime == "audio/wav" || out_mime == "audio/x-wav" {
            match (resolved_file_size, resolved_sr, resolved_bd, resolved_ch) {
                (Some(size), Some(sr), Some(bd), Some(ch))
                    if size > 44 && sr > 0 && bd > 0 && ch > 0 =>
                {
                    let byte_rate = sr as u64 * ch as u64 * (bd as u64 / 8);
                    if byte_rate > 0 {
                        Some(((size - 44) * 1000 / byte_rate) as i64)
                    } else {
                        Some(track.duration_ms)
                    }
                }
                _ => Some(track.duration_ms),
            }
        } else if !needs_transcode && is_network_output {
            // Native passthrough (FLAC/ALAC/… served raw) to a network renderer.
            //
            // The gapless-queued (SetNextAVTransportURI) track is the one that
            // regresses on the Marantz ND 8006 (Jean Valjean, #1132): odd tracks
            // start via a fresh SetAVTransportURI + Play — the renderer fetches
            // the URL itself, learns the true byte length, and ends the track at
            // the real EOF, so an over-long DIDL duration is harmless. But on the
            // gapless auto-transition to the *next* track the renderer does NOT
            // re-probe the stream — it models playback purely from the DIDL
            // `res@duration` we supplied via SetNext. When the scanned
            // `track.duration_ms` (possibly recovered by a slow/fallback scan on
            // a NAS, or drifted vs. the real sample count) is a few seconds LONGER
            // than the file's true duration, the renderer holds at the real EOF
            // with its estimate still reading position < duration, loses the
            // format/duration/progress display and cuts near the end of the
            // queued track. 1626ec21 only made `res@size` consistent; the
            // duration was still the scanned value on this passthrough path.
            //
            // Prefer the file container's authoritative duration (FLAC STREAMINFO
            // total_samples / sample_rate via lofty — metadata only, no decode)
            // so the SetNext DIDL `res@duration` matches the bytes actually
            // served. This corrects the current-track DIDL identically (it can
            // only get MORE accurate, never worse — the initial Play already
            // ends at real EOF). Fall back to the scanned duration if the probe
            // fails, so a NAS timeout never blanks the duration entirely.
            let probed_secs = crate::audio::analyzer::get_duration(&file_path).await.ok();
            Some(passthrough_didl_duration_ms(probed_secs, track.duration_ms))
        } else {
            Some(track.duration_ms)
        };

        Ok(ResolvedStream {
            url: stream_url,
            mime_type: out_mime,
            title: track.title,
            artist: track.artist_name,
            album: track.album_title,
            duration_ms: didl_duration_ms,
            source: "local".into(),
            cover_url: track.cover_path,
            stream_id: Some(session_id),
            file_size: resolved_file_size,
            sample_rate: resolved_sr,
            bit_depth: resolved_bd,
            channels: resolved_ch,
            origin_url: None,
            bitrate_kbps: None,
        })
    }

    /// Les décisions de `resolve_local_track` (formats, sortie, forçages DLNA,
    /// plafonds, égaliseur, transcodage requis), sorties telles quelles (REF-2
    /// phase 2, #2219). Le DoP servi directement rend une résolution complète
    /// sans passer par les armes : c'est le retour anticipé d'origine.
    #[allow(clippy::too_many_arguments)]
    async fn decider_la_lecture_locale(
        &self,
        req: &PlayRequest,
        track: &crate::db::models::Track,
        track_id: i64,
        file_path: String,
        fmt: String,
        source_format: Option<AudioFormat>,
        is_dsd_source: bool,
    ) -> Result<DecisionOuResolu, String> {
        let sample_rate = track
            .sample_rate
            .unwrap_or(if is_dsd_source { 2_822_400 } else { 44100 })
            as u32;
        let bit_depth = track
            .bit_depth
            .unwrap_or(if is_dsd_source { 1 } else { 16 }) as u16;
        let channels = track.channels as u16;

        // Determine the output type and max_sample_rate for this zone.
        let zone = ZoneRepo::with_backend(self.db.clone())
            .get(req.zone_id)
            .ok()
            .flatten();
        let zone_output_type = zone.as_ref().and_then(|z| z.output_type.clone());
        // Quirks catalogue (marque+modèle choisis par l'utilisateur pour la zone).
        // Additif : n'a d'effet que si l'utilisateur a explicitement sélectionné
        // un modèle catalogué. Sinon profil neutre (aucun changement).
        let device_quirks = crate::device_catalog::resolve_zone_quirks(&self.db, req.zone_id);
        // Le plafond catalogue se combine en `min` avec l'override de zone : il
        // ne peut que rendre la contrainte plus stricte, jamais l'assouplir.
        let zone_max_sample_rate = crate::device_catalog::combine_max_sample_rate(
            zone.as_ref().and_then(|z| z.max_sample_rate),
            device_quirks.max_sample_rate,
        );

        let is_oaat_output = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("oaat:") || id.starts_with("oaat-group:"));
        // OAAT endpoints: transcode to WAV for reliable bit-perfect playback.
        // Always transcode, even WAV sources, to normalise EXTENSIBLE/FLOAT
        // variants into simple PCM that the endpoint can reliably parse.
        let oaat_needs_wav = is_oaat_output && source_format.is_some();

        // Local output (cpal) has a simple WAV parser that only understands
        // standard PCM (format tag 1).  Real-world WAV files can use
        // WAVE_FORMAT_EXTENSIBLE (0xFFFE), IEEE_FLOAT (3), or have extra
        // metadata chunks that shift the data offset beyond the parser's
        // 4096-byte header buffer.  Feeding such files as passthrough causes
        // white noise because the byte layout doesn't match what the parser
        // expects (wrong bit depth, wrong data offset, or float-as-integer).
        //
        // Fix: ALWAYS transcode through symphonia for local output, even when
        // the source is already WAV.  Symphonia handles all WAV variants and
        // produces normalised integer PCM.  The HTTP stream handler then
        // prepends a simple 44-byte PCM header that the local parser handles
        // correctly.  The overhead is negligible (memcpy, no re-encoding).
        let is_local_output = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("local:"));
        let local_needs_wav = is_local_output && source_format.is_some();

        // Calculé ici, et non plus après la branche DoP : celle-ci en a besoin
        // pour servir du DoP à un renderer réseau (#1772). Ne dépend que de
        // `zone_output_type`, connu bien plus haut.
        let is_network_output = is_network_output_type(zone_output_type.as_deref());

        // DSD en DoP (DSD over PCM), c'est-à-dire du DSD transporté dans des
        // trames PCM 24 bits au seizième du débit.
        //
        // Deux cas, et le second manquait (#1772, Marco Polo, Wiim Pro → DAC
        // Denafrips) :
        //
        //  - sortie locale : « natif » comme « dop » passent par ici, la carte
        //    son ne sachant pas recevoir de DSD autrement ;
        //  - renderer réseau : uniquement sur choix EXPLICITE « dop ». Le
        //    lecteur réseau qui ne sait pas lire un .dsf sait souvent lire le
        //    DoP — c'est ce que fait MinimServer, que le testeur a comparé.
        //
        // Avant ce correctif, `"dop"` n'était comparé qu'ici, sous le garde
        // `is_local_output` : sur un renderer, le réglage tombait dans le
        // fourre-tout de `should_dsd_passthrough`, était traité comme « auto »,
        // et le Wiim n'annonçant pas le DSF, le serveur transcodait en PCM. Le
        // DAC recevait donc du WAV 176,4 kHz — le débit DoP du DSD64, ce qui
        // rendait le symptôme parfaitement trompeur.
        //
        // Le DoP réseau est plus sûr que le local : les octets partent par HTTP
        // sans passer par le rappel cpal, donc ni le volume ni le ReplayGain ne
        // peuvent détruire les marqueurs (cf. le grésillement de Cyrille).
        let dsd_mode = if source_format == Some(AudioFormat::Dsd) {
            ZoneRepo::with_backend(self.db.clone()).get_dsd_mode(req.zone_id)
        } else {
            String::new()
        };
        let dop_requested = dop_requested(is_local_output, is_network_output, &dsd_mode);

        // Un mode « auto » qui ne fait rien d'automatique, et qui se taisait.
        //
        // `dop_requested` ne reconnaît que `"native"` et `"dop"` — et en réseau,
        // `"dop"` seul. Le mode par DÉFAUT, `"auto"`, ne produit donc JAMAIS de
        // DoP, nulle part. Une piste DSD y part en PCM, ce qui est un choix
        // défendable, mais rien ne le disait : ni à l'écran, ni au journal.
        //
        // Conséquence vécue : un testeur dont un autre serveur lit le même
        // fichier sur le même DAC conclut que Tune « bloque sur le DSD », et
        // nous cherchons un défaut de lecture là où il n'y a qu'un réglage par
        // défaut trompeur (Tades, Hifiman Serenade, #1657). Une ligne de journal
        // aurait suffi à le voir sans lui poser la question.
        //
        // Tracé ici plutôt qu'en amont : c'est le seul endroit qui connaisse à
        // la fois le format de la source, le type de sortie et le mode réglé.
        //
        // Le nom de l'événement disait « sera converti en PCM ». C'était faux sur
        // une sortie réseau : ici, seul le DoP est écarté. La conversion, elle,
        // dépend de `should_dsd_passthrough`, plus bas — et en mode « native »
        // elle n'avait PAS lieu. Le journal annonçait donc du PCM pendant qu'une
        // URL `.dsd` partait sur le fil, ce qui a coûté trois diagnostics faux
        // (#2122). L'événement dit maintenant ce qu'il sait vraiment ; la
        // décision de conversion est tracée là où elle est prise.
        if source_format == Some(AudioFormat::Dsd) && !dop_requested {
            info!(
                zone_id = req.zone_id,
                dsd_mode = %dsd_mode,
                is_local_output,
                is_network_output,
                "dsd_dop_not_requested"
            );
        }

        if source_format == Some(AudioFormat::Dsd) {
            if dop_requested {
                // La cadence et le nombre de canaux se lisent DANS LE FICHIER,
                // pas dans la base.
                //
                // L'en-tête WAV décrivait la ligne `tracks` pendant que la
                // charge utile sortait de `parse_dsf`/`parse_dff` : deux
                // sources qui n'ont aucune raison de coïncider. Un écart d'un
                // canal désaligne chaque mot de 24 bits, le marqueur DoP ne
                // tombe plus sur l'octet de poids fort, le DAC ne verrouille
                // pas en DSD et joue le train DSD comme du PCM — c'est-à-dire
                // du bruit blanc (Marco Polo, Wiim Pro, #1894). Un écart de
                // cadence annonce un débit que le renderer n'appliquera pas.
                //
                // Le fichier est la seule source qui décrit ce qui part
                // réellement sur le fil, et c'est la même que celle dont
                // l'encodeur se sert (`decode_dsd_to_dop_streaming`). La base
                // ne sert plus que de repli si l'en-tête est illisible.
                let dsd_probe = {
                    let ext = std::path::Path::new(&file_path)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("dsf")
                        .to_lowercase();
                    if ext == "dff" {
                        crate::audio::dff::parse_dff(&file_path)
                            .ok()
                            .map(|i| (i.sample_rate, i.channels))
                    } else {
                        crate::audio::dsf::parse_dsf(&file_path)
                            .ok()
                            .map(|i| (i.sample_rate, i.channels))
                    }
                };
                if dsd_probe.is_none() {
                    warn!(
                        path = %file_path,
                        "dsd_header_unreadable_falling_back_to_db_metadata"
                    );
                }
                let (dsd_rate, dop_channels) = dop_wire_params(
                    dsd_probe,
                    track.sample_rate.map(|v| v as u32),
                    track.channels as u32,
                );
                let dop_rate = crate::audio::dsd_to_dop::DsdToDoP::dop_rate(dsd_rate);
                // Réutilise le plafond déjà combiné avec le quirk catalogue.
                let zone_max_sr = zone_max_sample_rate;
                if let Some(max_sr) = zone_max_sr {
                    if dop_rate > max_sr {
                        info!(
                            dsd_rate,
                            dop_rate, max_sr, "dsd_dop_rate_exceeds_zone_max_falling_back_to_pcm"
                        );
                        // Fall through to normal DSD→PCM transcode path
                    }
                }
                if zone_max_sr.is_none_or(|max_sr| dop_rate <= max_sr) {
                    // `dop_channels` vient de `dop_wire_params`, calculé plus
                    // haut avec la cadence : même source, même raison.

                    let wav_info = StreamInfo {
                        format: "wav".into(),
                        mime_type: "audio/wav".into(),
                        sample_rate: dop_rate,
                        bit_depth: 24,
                        channels: dop_channels,
                        file_size: None,
                        duration_ms: Some(track.duration_ms as u64),
                        ..Default::default()
                    };

                    let (session_id, tx, data_ready) =
                        self.streamer.create_session(wav_info, true, 128).await;

                    info!(
                        file = %file_path,
                        dsd_rate,
                        dop_rate,
                        channels = dop_channels,
                        sortie = if is_local_output { "locale" } else { "réseau" },
                        "dsd_dop_streaming"
                    );

                    let fp = file_path.clone();
                    let ext = std::path::Path::new(&fp)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("dsf")
                        .to_lowercase();
                    tokio::task::spawn_blocking(move || {
                        // Send WAV header first
                        let wav_hdr =
                            crate::audio::wav::build_wav_header(dop_channels, dop_rate, 24);
                        let rt = tokio::runtime::Handle::current();
                        let _ = rt.block_on(tx.send(wav_hdr.to_vec()));
                        data_ready.notify_one();

                        let mut first = false;
                        match crate::audio::decode::decode_dsd_to_dop_streaming(
                            &fp, &ext, tx, 65536, &mut first, &None, &rt,
                        ) {
                            Ok(_) => tracing::debug!("dsd_dop_stream_complete"),
                            Err(e) => tracing::warn!(error = %e, "dsd_dop_stream_failed"),
                        }
                    });

                    let server_ip = self.server_ip();
                    let stream_url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");

                    return Ok(DecisionOuResolu::Resolu(ResolvedStream {
                        url: stream_url,
                        stream_id: Some(session_id),
                        title: track.title.clone(),
                        artist: track.artist_name.clone(),
                        album: track.album_title.clone(),
                        duration_ms: Some(track.duration_ms),
                        source: "local".into(),
                        mime_type: "audio/wav".into(),
                        sample_rate: Some(dop_rate),
                        bit_depth: Some(24),
                        channels: Some(dop_channels as u32),
                        origin_url: None,
                        bitrate_kbps: None,
                        cover_url: self.resolve_cover_url(track.cover_path.as_deref()),
                        file_size: None,
                    }));
                } // end dop_rate <= max check
            }
        }

        // Transcode exotic formats (AIFF, DSD, WavPack, APE, ALAC, WMA) for network outputs
        // that receive a URL and play it directly. FLAC, WAV, MP3, AAC pass through as-is.
        // (`is_network_output` est calculé plus haut, la branche DoP en a besoin.)

        // Browser (Web Audio) zones pull the file themselves via <audio> and can
        // only decode the mainstream web codecs (FLAC/MP3/AAC/WAV/Ogg/Opus). An
        // exotic source — above all DSD — is otherwise served RAW (no network/
        // local output claims it, so nothing forces a transcode) and the <audio>
        // element is handed bytes it can't play, staying SILENT (Reivax66, local
        // DSD album on the "Cet ordinateur" zone, 0.9.44). Decode those to PCM/WAV
        // here, mirroring the streaming arm which already serves WAV to browser
        // zones. Codecs a browser plays natively stay direct (no regression).
        let is_browser_output = zone_output_type.as_deref() == Some("browser");
        let browser_needs_wav = is_browser_output
            && matches!(
                source_format,
                Some(AudioFormat::Dsd)
                    | Some(AudioFormat::WavPack)
                    | Some(AudioFormat::Ape)
                    | Some(AudioFormat::Wma)
                    | Some(AudioFormat::Aiff)
                    | Some(AudioFormat::Alac)
            );

        // DSD native passthrough: skip transcode when the renderer supports DSD natively.
        let dsd_passthrough = if source_format == Some(AudioFormat::Dsd) && is_network_output {
            let did = req
                .output_device_id
                .as_deref()
                .or(zone.as_ref().and_then(|z| z.output_device_id.as_deref()))
                .unwrap_or("");
            self.should_dsd_passthrough(req.zone_id, did).await
        } else {
            false
        };

        // ALAC native passthrough (opt-in per zone): serve the ALAC file
        // straight to a renderer that decodes it, instead of transcoding to
        // FLAC — bit-perfect and zero CPU. Off by default because ALAC and AAC
        // share the audio/mp4 MIME, so it can't be auto-detected safely.
        // LPCM override: a zone set to serve WAV/LPCM must transcode (to strip
        // the renderer's ALAC decoder quirks — e.g. LHC-56 pops at start), so it
        // takes precedence over ALAC passthrough.
        let dlna_lpcm =
            is_network_output && ZoneRepo::with_backend(self.db.clone()).get_dlna_lpcm(req.zone_id);
        // Opt-in per zone: serve genuine 24-bit WAV instead of the 16-bit LPCM
        // fallback. Only offered in the UI for renderers that advertise
        // `audio/L24` (capability probe). Forces the WAV path exactly like
        // `dlna_lpcm`, but the DIDL drops the 16-bit-only `DLNA.ORG_PN=LPCM`
        // profile (see didl::dlna_flags_for_mime_bd) so a strict renderer no
        // longer maps the stream back to 16-bit and reads misaligned samples
        // (the #1137 silence class). Only meaningful when the source is deeper
        // than 16-bit; a 16-bit source keeps the plain LPCM path.
        // `bit_depth` vient de la ligne `tracks`, et pour un ALAC elle peut y
        // être absente : la profondeur n'est lisible que par une sonde
        // Symphonia sur le cookie magique (`probe_m4a_props`), arrivée après
        // coup. Une piste scannée avant — ou dont la sonde a échoué — porte
        // alors le défaut `16`, et l'opt-in 24 bits ne s'arme JAMAIS, quel que
        // soit le réglage : coché, vérifié, sans effet (Yves, ALAC 24/96, #1654).
        //
        // Sonder le fichier lève l'ambiguïté, et le coût est nul en pratique :
        // la sonde ne tourne que si la zone a EXPLICITEMENT demandé le 24 bits
        // (opt-in rare) et que la base annonce 16 ou moins sur un conteneur qui
        // sait porter davantage. Un vrai ALAC 16 bits répond 16 et rien ne
        // change.
        let wav24_opt_in = is_network_output
            && ZoneRepo::with_backend(self.db.clone()).get_dlna_wav24(req.zone_id);
        let bit_depth_wire = if wav24_opt_in && bit_depth <= 16 {
            profondeur_sondee_si_la_base_ignore(&file_path, source_format).unwrap_or(bit_depth)
        } else {
            bit_depth
        };
        let dlna_wav24 = wav24_opt_in && bit_depth_wire > 16;
        // Both WAV overrides force a transcode away from FLAC/ALAC passthrough.
        //
        // …SAUF sur une source FLAC dont la zone demande explicitement le FLAC
        // natif. Le forçage WAV existe pour contourner le décodeur ALAC du
        // renderer (LHC-56 qui claque au démarrage, cf. le commentaire de
        // `dlna_lpcm` ci-dessus) : l'appliquer aussi au FLAC est un dommage
        // collatéral, jamais l'objectif.
        //
        // Sans cette exception, les deux réglages se contredisent et c'est le
        // WAV qui gagne en silence : Yves a lu un FLAC transcodé en WAV alors
        // que « FLAC natif » était coché, et en a conclu que Tune gardait en
        // mémoire les réglages du morceau précédent (forum #1437). Les deux
        // cases décrivent en fait deux sources différentes, et peuvent donc
        // coexister — l'ALAC part en WAV, le FLAC reste du FLAC.
        //
        // L'exception exige l'opt-in `dlna_native_flac` : sans lui, une source
        // FLAC continue de suivre le forçage, ce dont ont besoin les renderers
        // qui ne savent pas lire le FLAC.
        let dlna_force_wav = wav_override_applies(
            dlna_lpcm || dlna_wav24,
            source_format == Some(AudioFormat::Flac),
            is_network_output
                && ZoneRepo::with_backend(self.db.clone()).get_dlna_native_flac(req.zone_id),
        );
        // Opt-in per zone: cap output to 16-bit. Some renderers advertise
        // `audio/flac` (so Tune sends hi-res FLAC/ALAC direct) but only decode
        // 16-bit internally — 24-bit direct plays SILENCE (Ruark R3, Yves #1137).
        // Forces a 16-bit downconvert (kept as FLAC) instead of direct
        // passthrough, without regressing renderers that genuinely play 24-bit.
        // Only meaningful when the source is deeper than 16-bit.
        // Flag zone `dlna_cap_16bit` OR quirk catalogue `force_16bit` (additif :
        // le quirk ne peut que l'activer, jamais le désactiver — Ruark R3 #1137).
        let dlna_cap_16bit = is_network_output
            && bit_depth > 16
            && (ZoneRepo::with_backend(self.db.clone()).get_dlna_cap_16bit(req.zone_id)
                || device_quirks.force_16bit);
        let alac_passthrough = source_format == Some(AudioFormat::Alac)
            && is_network_output
            && !dlna_force_wav
            && !dlna_cap_16bit
            && ZoneRepo::with_backend(self.db.clone()).get_alac_passthrough(req.zone_id);
        // Même mécanique pour l'AAC (Marco Polo, #1424) : un Marantz SR7009 ou
        // un Denon RC12 le décodent nativement, et le transcoder en FLAC ne fait
        // que retarder le premier son et consommer du processeur — l'AAC étant
        // déjà compressé avec perte, le transcodage n'apporte aucune qualité.
        //
        // Pas de garde `dlna_cap_16bit` ici, contrairement à l'ALAC : ce plafond
        // vise les sources plus profondes que 16 bits, ce qu'un AAC n'est jamais.
        // `dlna_force_wav` reste respecté — un renderer qui exige du LPCM le
        // dit, et son exigence prime sur une préférence.
        let aac_passthrough = source_format == Some(AudioFormat::Aac)
            && is_network_output
            && !dlna_force_wav
            && ZoneRepo::with_backend(self.db.clone()).get_aac_passthrough(req.zone_id);

        // Chromecast's Default Media Receiver decodes a narrower set than most
        // DLNA renderers — notably it cannot play AIFF (which DLNA plays
        // direct). Serving AIFF direct to a Cast device fails the LOAD, so the
        // track never leaves position 0; auto-advance then skips to the next
        // track every few seconds and the shuffle-all queue "resets" endlessly,
        // never becoming audible (forum #1210, Mika, BeoPlay A9 via CAST).
        let is_chromecast = zone_output_type.as_deref() == Some("chromecast");
        let needs_transcode_for_output = is_network_output
            && !dsd_passthrough
            && !alac_passthrough
            && !aac_passthrough
            && source_format.as_ref().is_some_and(|f| {
                if is_chromecast {
                    f.needs_transcode_for_chromecast()
                } else {
                    f.needs_transcode_for_dlna()
                }
            });

        // DLNA format negotiation: if the output will be FLAC (either source
        // is FLAC, or source needs transcode and target is FLAC), check that
        // the renderer supports audio/flac. Otherwise force WAV (LPCM).
        let is_dlna = zone_output_type.as_deref() == Some("dlna");
        let will_be_flac = source_format == Some(AudioFormat::Flac)
            || (needs_transcode_for_output
                && source_format
                    .map(|f| f.dlna_transcode_target() == AudioFormat::Flac)
                    .unwrap_or(false));
        let dlna_needs_wav = if is_dlna && will_be_flac {
            let did = req
                .output_device_id
                .as_deref()
                .or(zone.as_ref().and_then(|z| z.output_device_id.as_deref()))
                .unwrap_or("");
            if dlna_force_wav {
                // User forces WAV for this zone (16-bit LPCM via `dlna_lpcm`, or
                // genuine 24-bit via `dlna_wav24`): skips the slow native FLAC
                // encoder for hi-res AND avoids a renderer whose ALAC decoder
                // pops at start (Yves, LHC-56). Takes precedence over the FLAC
                // override below.
                true
            } else if did.is_empty() {
                false
            } else if ZoneRepo::with_backend(self.db.clone()).get_dlna_native_flac(req.zone_id) {
                // User forces native FLAC for this zone: some renderers decode
                // FLAC but never advertise it (Marco's Denon Ceol N12 returns an
                // empty GetProtocolInfo Sink), so protocol negotiation wrongly
                // falls back to WAV. Honour the override and send FLAC.
                false
            } else {
                !self.dlna_supports_mime(did, "audio/flac").await
            }
        } else {
            false
        };

        // Downsample if the zone has a max_sample_rate cap and the source
        // exceeds it. For DSD, `sample_rate` is the raw DSD bit rate (MHz), so
        // this uses the PCM *output* rate for the comparison and never
        // downsamples a native DSD passthrough — otherwise a capped zone would
        // silently turn passthrough into a full DSD→PCM transcode (100s decode,
        // transcode_timeout_120s, album cutoff on the HiFi Rose RS130).
        let needs_downsample = crate::audio::formats::needs_downsample_for_cap(
            source_format,
            sample_rate,
            zone_max_sample_rate,
            dsd_passthrough,
        );
        // Un égaliseur ACTIVÉ sur la zone doit s'entendre : en passthrough
        // réseau (FLAC servi brut à la Beoplay A9), l'EqProcessor n'était
        // jamais appliqué — profil « appliqué » côté UI, zéro effet audible
        // (Mika, forum #1216). Activer l'EQ est un choix explicite de
        // traitement (les puristes ont le mode PURE, qui désactive ceci via
        // load_eq_processor→None) : on force alors le chemin transcodé, où
        // l'EQ est déjà branché. Jamais sur un passthrough DSD/ALAC voulu.
        // Les zones NAVIGATEUR tirent aussi le fichier brut via <audio> (FLAC
        // local servi direct) : même trou que #1216, mesuré sur .18 — deux
        // captures du flux EQ ±12 dB strictement identiques (md5). L'EQ y
        // force donc aussi le transcodage.
        // Une sortie PULL hors dépôt — `diretta`, `oaat` — va chercher le flux
        // elle-même et n'est ni « réseau » ni « navigateur » au sens ci-dessus.
        // Elle recevait donc le fichier brut : même trou que #1216, une
        // troisième fois (Eric, forum : égaliseur sans effet vers un renderer
        // Diretta). La sortie LOCALE est exclue — elle passe déjà par le
        // transcodage dès que le format source est connu (`local_needs_wav`).
        let is_pull_dsp_output = pull_output_needs_dsp_transcode(
            zone_output_type.as_deref(),
            is_local_output,
            is_oaat_output,
            source_format,
        );
        let eq_forces_transcode = (is_network_output || is_browser_output || is_pull_dsp_output)
            && !dsd_passthrough
            && !alac_passthrough
            && (self.zone_has_active_eq(req.zone_id)
                || self.zone_has_active_ir(req.zone_id)
                // ReplayGain scales the samples, so it lives in the same place
                // as the EQ — and would be discarded in the same way on a
                // passthrough. Enabling it is an explicit choice of processing;
                // PURE zones are excluded upstream.
                || self.zone_replaygain_changes_audio(req.zone_id, req.track_id));
        // En navigateur, la sortie transcodée doit être du WAV : un FLAC
        // ré-encodé à la volée n'a pas de seektable et cale le <audio> sur les
        // Range (#1168) — même règle que le bras streaming.
        let browser_needs_wav = browser_needs_wav || (is_browser_output && eq_forces_transcode);

        let needs_transcode = needs_transcode_for_output
            || oaat_needs_wav
            || local_needs_wav
            || browser_needs_wav
            || needs_downsample
            || dlna_needs_wav
            || eq_forces_transcode
            // 16-bit cap on a FLAC-direct renderer: force a transcode so the
            // hi-res FLAC is re-encoded at 16-bit instead of served direct
            // (silent on the Ruark R3, #1137). ALAC already transcodes because
            // the cap disables alac_passthrough above.
            || (dlna_cap_16bit && will_be_flac);
        if eq_forces_transcode && !needs_transcode_for_output && !dlna_needs_wav {
            info!(zone_id = req.zone_id, "eq_active_forcing_network_transcode");
        }

        let track_duration_ms = track.duration_ms;
        let track_file_size = track.file_size;
        let decision = DecisionLocale {
            bit_depth,
            bit_depth_wire,
            browser_needs_wav,
            channels,
            dlna_cap_16bit,
            dlna_needs_wav,
            dlna_wav24,
            eq_forces_transcode,
            is_browser_output,
            is_chromecast,
            is_local_output,
            is_network_output,
            local_needs_wav,
            needs_downsample,
            needs_transcode_for_output,
            oaat_needs_wav,
            sample_rate,
            source_format,
            track_id,
            zone_max_sample_rate,
            track_duration_ms,
            track_file_size,
            file_path,
            fmt,
            zone,
            needs_transcode,
        };
        Ok(DecisionOuResolu::Decision(decision))
    }

    /// Arme « transcodage » du grand tuple de `resolve_local_track`, sortie telle
    /// quelle (REF-2 phase 2, #2219) : transcodage en fichier (cache, budget) ou
    /// en flux (WAV pour local/OAAT), selon la décision. Rend (session, mime,
    /// extension, taille, fréquence, profondeur, canaux) servis.
    async fn transcoder_la_piste(
        &self,
        req: &PlayRequest,
        decision: &DecisionLocale,
    ) -> Result<
        (
            String,
            String,
            String,
            Option<u64>,
            Option<u32>,
            Option<u32>,
            Option<u32>,
        ),
        String,
    > {
        let format = self.decider_le_format_de_sortie(req, decision);
        if format.use_file_transcode {
            self.transcoder_vers_fichier(req, decision, format).await
        } else {
            self.transcoder_en_session(req, decision, format).await
        }
    }

    /// Premier temps du transcodage : le format de sortie. Fréquence plafonnée
    /// par la zone, profondeur selon la sortie, conteneur et type MIME, et le
    /// choix entre fichier pré-transcodé et session à la volée
    /// (`use_file_transcode_for`).
    fn decider_le_format_de_sortie(
        &self,
        req: &PlayRequest,
        decision: &DecisionLocale,
    ) -> FormatDeSortie {
        let DecisionLocale {
            bit_depth,
            bit_depth_wire,
            browser_needs_wav,
            channels,
            dlna_cap_16bit,
            dlna_needs_wav,
            dlna_wav24,
            eq_forces_transcode,
            is_browser_output,
            is_chromecast,
            is_network_output,
            local_needs_wav,
            needs_downsample,
            needs_transcode_for_output,
            oaat_needs_wav,
            sample_rate,
            source_format,
            zone_max_sample_rate,
            track_duration_ms,
            ref file_path,
            ..
        } = *decision;
        let src_fmt = source_format.unwrap_or(AudioFormat::Flac);
        let target_fmt = if oaat_needs_wav || local_needs_wav || browser_needs_wav {
            AudioFormat::Wav
        } else if dlna_needs_wav {
            // Renderer doesn't support FLAC — transcode to WAV (LPCM)
            // which has a proper DLNA.ORG_PN=LPCM profile.
            AudioFormat::Wav
        } else if needs_downsample && !needs_transcode_for_output {
            // Only downsampling — keep the same lossless format
            AudioFormat::Flac
        } else if is_chromecast && src_fmt == AudioFormat::Aiff {
            // AIFF → FLAC for Chromecast (Cast decodes FLAC up to
            // 24-bit/96k, but not AIFF). dlna_transcode_target(Aiff) is a
            // no-op (Aiff→Aiff) meant for DLNA, so it must be overridden
            // here or the Cast device would be fed AIFF again (#1210).
            AudioFormat::Flac
        } else if src_fmt == AudioFormat::Dsd && is_network_output {
            // DSD → network renderer: stream as progressive WAV/LPCM instead
            // of a blocking pre-transcode to a FLAC file.
            //
            // DSD→FLAC is the slowest transcode (74–86s for a track). The
            // FLAC path takes `use_file_transcode` below, which decodes AND
            // encodes the WHOLE file to /tmp BEFORE serving a single byte —
            // so a renderer that can't wait ~80s for its transport URI to
            // become playable times out and plays SILENCE. Linn Klimax /
            // OpenHome (Pierre Mack) never decodes DSD itself, so it always
            // hit this ~80s stall.
            //
            // A WAV target routes through the streaming session instead: the
            // decoder feeds PCM as it runs (first bytes in ~1s), and the HTTP
            // layer still advertises an exact Content-Length
            // (StreamInfo::wav_content_length, from the known duration) +
            // Accept-Ranges + 206-on-`bytes=0-` — exactly what DLNA/OpenHome
            // renderers require. This is the same streaming-WAV path the
            // Eversolo DMP-A6/A8 already use. Renderers that need a 16-bit
            // LPCM cap keep it via `dlna_needs_wav` above; this branch only
            // catches FLAC-capable renderers (Linn) that were paying the full
            // ~80s stall for nothing.
            AudioFormat::Wav
        } else {
            src_fmt.dlna_transcode_target()
        };
        let mut out_sr = src_fmt.dsd_output_sample_rate(sample_rate);
        // Apply zone max_sample_rate cap
        if let Some(max_sr) = zone_max_sample_rate {
            if out_sr > max_sr {
                info!(
                    zone_id = req.zone_id,
                    source_rate = out_sr,
                    max_rate = max_sr,
                    "zone_max_sample_rate_cap_applied"
                );
                out_sr = max_sr;
            }
        }
        let out_bd: u16 = if local_needs_wav {
            // Local output (cpal/WASAPI): always use 32-bit WAV.
            //
            // Symphonia decodes all audio into AudioBuffer<i32> (left-justified
            // 32-bit integers) regardless of source bit depth.  When packing
            // these into 24-bit (3 bytes/sample), any mismatch between the
            // reported source_bd and the actual sample range causes byte
            // misalignment in the PCM stream — the local parser then reads
            // from wrong offsets, producing white noise.
            //
            // Using 32-bit eliminates this class of bugs entirely: each i32
            // sample is written as 4 bytes, matching the WAV header's declared
            // byte width.  The local output converts to f32 for cpal anyway,
            // so there is zero quality loss.
            32
        } else if browser_needs_wav {
            // Browser <audio> plays 16-bit PCM WAV everywhere; 24/32-bit are
            // spotty across engines. Match the streaming arm (browser = 16-bit
            // WAV) so playback is guaranteed audible.
            16
        } else if src_fmt == AudioFormat::Dsd {
            24
        } else if oaat_needs_wav {
            // OAAT endpoints (Tune's own RPi renderers) parse the WAV fmt
            // chunk and handle true 24-bit PCM: cap at 24-bit.
            cap_output_bit_depth(bit_depth)
        } else if dlna_wav24 {
            // Zone opt-in: serve genuine 24-bit WAV to a renderer that
            // advertises `audio/L24`. The DIDL drops the 16-bit-only
            // `DLNA.ORG_PN=LPCM` profile (didl::dlna_flags_for_mime_bd keyed
            // on this bit_depth), so the renderer parses the real 24-bit WAV
            // header instead of mapping a false profile back to 16-bit and
            // reading misaligned samples (#1137). `dlna_wav24` is already
            // gated on `bit_depth_wire > 16` above; cap at 24 (FLAC/WAV
            // ceiling).
            //
            // `bit_depth_wire`, pas `bit_depth` : sur un ALAC dont la base
            // ignore la profondeur, c'est la sonde du fichier qui fait foi.
            // Prendre la valeur de la base ici servirait un en-tête 16 bits
            // pour un flux 24 — exactement le défaut que ce chemin corrige
            // (#1654).
            bit_depth_wire.min(24)
        } else if dlna_needs_wav {
            // Generic DLNA renderers that need a WAV/LPCM fallback: cap at
            // 16-bit.
            //
            // The WAV we serve is advertised in DIDL with
            // `DLNA.ORG_PN=LPCM` and Content-Type `audio/wav`.  The DLNA
            // LPCM profile is standardised for 16-bit only (`audio/L16`);
            // there is no standard PN for 24-bit LPCM.  Many hi-fi
            // renderers (Ruark R3, LHC-62 — Yves, forum #1137) map that
            // advertised profile to 16-bit and, fed genuine 24-bit PCM
            // (3 bytes/sample), read misaligned samples and play SILENCE.
            // 16-bit tracks worked because 16-bit WAV *is* valid LPCM.
            //
            // Renderers that can preserve hi-res advertise `audio/flac`
            // and take the FLAC branch above (dlna_needs_wav = false), so
            // this cap only ever applies to the LPCM fallback where
            // guaranteed-audible 16-bit is the correct trade-off.
            16
        } else if dlna_cap_16bit {
            // Zone opt-in cap: renderer advertises `audio/flac` but only
            // decodes 16-bit (Ruark R3, #1137). Downconvert to 16-bit FLAC
            // instead of sending silent hi-res direct.
            16
        } else if src_fmt == AudioFormat::Alac {
            // ALAC: transcode to FLAC for DLNA (universally supported).
            // FLAC max is 24-bit; cap at min(source_bd, 24) but at least 16.
            cap_output_bit_depth(bit_depth)
        } else {
            cap_output_bit_depth(bit_depth)
        };
        // La profondeur ANNONCÉE doit être une profondeur QU'ON SAIT ÉCRIRE.
        //
        // `out_bd` part dans `StreamInfo`, donc dans le `<res bitsPerSample>`
        // du DIDL et dans le choix du profil `DLNA.ORG_PN` : c'est le contrat
        // passé au renderer. Or deux branches le laissent sortir de
        // {16, 24, 32} — `cap_output_bit_depth` ne borne qu'à 16..24, et
        // `dlna_wav24` prend `bit_depth_wire.min(24)`. Une source de 20 bits,
        // légale en ALAC comme en FLAC, annonçait donc 20 bits, que rien en
        // aval ne sait ni convertir ni encoder. Arrondi vers le HAUT, comme
        // au décodage : aucun bit perdu (#1437).
        let out_bd = crate::audio::decode::container_bit_depth(out_bd);
        let out_mime = if oaat_needs_wav || local_needs_wav {
            "audio/wav".to_string()
        } else {
            target_fmt.mime_type().to_string()
        };
        let out_ext = if oaat_needs_wav || local_needs_wav {
            "wav".to_string()
        } else {
            target_fmt.container_format().to_string()
        };

        info!(
            file = %file_path,
            source = ?src_fmt,
            target = ?target_fmt,
            sample_rate = out_sr,
            bit_depth = out_bd,
            "transcode_required"
        );

        // For network outputs (DLNA, OpenHome, etc.) with non-WAV targets
        // (e.g. FLAC), pre-transcode to a temp file on disk so the HTTP
        // handler can serve it with Content-Length and Accept-Ranges.
        // Renderers like the darTZeel LHC-208 reject chunked transfer
        // (no Content-Length) and require a known file size.
        //
        // For local/OAAT outputs (WAV target), keep using streaming
        // sessions — those outputs don't need Content-Length.
        let target_format_str = if target_fmt == AudioFormat::Wav {
            "wav".to_string()
        } else {
            target_fmt.container_format().to_string()
        };
        // Network outputs need file transcode for Content-Length + Range.
        // Local outputs use streaming sessions — the _keep_alive_tx in
        // StreamSession prevents the channel from closing when the decoder
        // finishes, so ASIO/WASAPI can consume all buffered data at their
        // own pace. This avoids the 28s download delay of file transcode.
        // A DSD source served as WAV/LPCM can stream (exact Content-Length
        // from wav_content_length) instead of blocking on a temp file that
        // times out at 120s for DSD256/512 → silence (Villerio). Gated by
        // the `dsd_lpcm_stream` setting (toggle in Settings → Lecture),
        // off by default pending field validation; read live so the toggle
        // takes effect without a restart.
        let dsd_lpcm_streams = src_fmt == AudioFormat::Dsd
            && target_fmt == AudioFormat::Wav
            && SettingsRepo::with_backend(self.db.clone())
                .get("dsd_lpcm_stream")
                .ok()
                .flatten()
                .as_deref()
                == Some("true");
        let use_file_transcode = use_file_transcode_for(
            is_network_output,
            target_format_str == "wav",
            dlna_needs_wav,
            dsd_lpcm_streams,
            // Une zone dont un TRAITEMENT est actif doit l'entendre : le
            // bras progressif appelle `decode_to_pcm_streaming_seeked`, qui
            // ne reçoit ni EqProcessor, ni convolveur, ni facteur
            // ReplayGain — seul `transcode_source_to_file` les applique
            // (voir les points 1a/1b de cette fonction). Le « transcodage
            // forcé » servait donc un flux sans aucun des trois.
            //
            // Mesuré sur .18 pour le NAVIGATEUR : capture WAV avec EQ
            // strictement identique à la source décodée (#1168). La même
            // fuite existe sur une sortie RÉSEAU depuis que le DSD y part en
            // WAV progressif (0cf27ade, 27/07) : une zone DLNA/OpenHome avec
            // égaliseur, correction FIR ou ReplayGain actif lisant du DSD
            // prend ce bras et perd les trois, en silence. C'est la famille
            // #1216 — déjà corrigée pour le passthrough réseau, le
            // navigateur et les sorties PULL, jamais ici.
            //
            // `eq_forces_transcode` est déjà borné aux sorties réseau /
            // navigateur / PULL et aux zones où un traitement est
            // RÉELLEMENT actif (PURE rend `None`) : une zone sans
            // traitement — l'immense majorité, et le cas qu'arbitre #1363 —
            // garde le bras progressif. Les sorties PULL (`oaat`,
            // `diretta`) restent hors du champ : elles ne passent jamais par
            // le transcodage fichier et le changement n'est pas mesuré.
            (is_browser_output || is_network_output) && eq_forces_transcode,
        );

        let info = StreamInfo {
            format: out_ext.clone(),
            mime_type: out_mime.clone(),
            sample_rate: out_sr,
            bit_depth: out_bd,
            channels,
            file_size: None,
            duration_ms: Some(track_duration_ms as u64),
            ..Default::default()
        };
        FormatDeSortie {
            out_sr,
            out_bd,
            out_mime,
            out_ext,
            target_format_str,
            use_file_transcode,
            info,
        }
    }

    /// Deuxième temps, sorties réseau : décodage → encodage → fichier
    /// temporaire, puis session de fichier servie avec Content-Length et
    /// Range. Cache par empreinte quand aucun traitement n'altère les octets.
    /// `annonce_apres_sortie_guard` relit ce texte (niveaux sur cache hit).
    async fn transcoder_vers_fichier(
        &self,
        req: &PlayRequest,
        decision: &DecisionLocale,
        format: FormatDeSortie,
    ) -> Result<FluxLocal, String> {
        let DecisionLocale {
            bit_depth,
            channels,
            is_local_output,
            sample_rate,
            track_duration_ms,
            ref file_path,
            ..
        } = *decision;
        let FormatDeSortie {
            out_sr,
            out_bd,
            out_mime,
            out_ext,
            target_format_str,
            ..
        } = format;
        let flux = {
            // ── Pre-transcode to temp file (FLAC) ──────────────────
            // Decode → encode → write to /tmp, then create a file session.
            // The HTTP handler serves file sessions with Content-Length
            // and Range support, which DLNA renderers require.
            let fp = file_path.clone();
            let ev_bus = self.event_bus.clone();
            let playback = self.playback.clone();
            let zone_id = req.zone_id;
            // EQ alters the encoded bytes and is not part of the cache key,
            // so a zone with an active EQ never uses the cache (always fresh).
            let eq_profile = self.load_eq_processor(req.zone_id, out_sr, channels);
            // The FIR convolver, like the EQ, alters the encoded bytes and
            // is not part of the cache key → a zone with an active IR never
            // uses the cache (always fresh).
            let convolver = self.load_convolver(req.zone_id, out_sr, channels);
            // ReplayGain scales the samples, so like the EQ and the FIR it
            // changes the encoded bytes without being part of the cache key.
            // A cached transcode made at a different gain would be served
            // silently at the wrong level — so a gained transcode is never
            // cached, and never reads the cache.
            // NOT for a local zone: the local output applies the gain on
            // its own render path, and a local zone with a known source
            // format always comes through here (`local_needs_wav`) — so
            // baking it in as well multiplied the gain twice. A -6 dB track
            // played at -12 dB, quietly.
            let replaygain_factor = match (
                is_local_output || self.zone_audiophile(req.zone_id),
                req.track_id,
            ) {
                (false, Some(tid)) => {
                    let f = crate::audio::replaygain::playback_factor(&self.db, tid);
                    if (f - 1.0).abs() > 1e-6 {
                        Some(f)
                    } else {
                        None
                    }
                }
                _ => None,
            };
            let cache_path_opt = if eq_profile.is_some()
                || convolver.is_some()
                || replaygain_factor.is_some()
            {
                None
            } else {
                crate::transcode_cache::cache_path(&file_path, &out_ext, out_sr, out_bd, channels)
            };
            // The transcode always writes to a fresh `tune-transcode-*` file
            // (subject to the normal cleanup); on success it is atomically
            // renamed into the cache. A crash mid-transcode therefore can
            // never leave a partial file under a cache name that a later hit
            // would serve.
            let tmp_path = std::env::temp_dir()
                .join(format!(
                    "tune-transcode-{}.{}",
                    uuid::Uuid::new_v4(),
                    &out_ext
                ))
                .to_string_lossy()
                .to_string();

            // Serialize transcodes of this same source file and drop any
            // play a newer tap has already superseded, so a burst of taps
            // can't spawn overlapping ALAC→FLAC transcodes of one file
            // (Yves, DMP-A10 over DLNA). Capture our own play seq, then
            // wait our turn on the per-file gate; if a newer play bumped the
            // generation while we waited, skip the transcode entirely.
            let my_seq = self.playback.current_play_seq(req.zone_id).await;
            let file_gate = {
                let mut gates = TRANSCODE_GATE.lock().await;
                gates
                    .entry(file_path.clone())
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone()
            };
            let _file_hold = file_gate.lock().await;
            if self.playback.current_play_seq(req.zone_id).await != my_seq {
                info!(
                    zone_id = req.zone_id,
                    file = %file_path,
                    "transcode_skipped_superseded_burst"
                );
                return Err(SUPERSEDED_BEFORE_TRANSCODE.into());
            }

            // Cache hit: an identical rendition already exists on disk —
            // serve it and skip the entire decode/encode (Yves: ~30s → instant
            // on replay / superseded burst).
            if let Some(cp) = cache_path_opt
                .as_ref()
                .filter(|cp| crate::transcode_cache::is_hit(cp))
            {
                crate::transcode_cache::touch(cp);
                let file_size = std::fs::metadata(cp).map(|m| m.len()).unwrap_or(0);
                info!(file = %file_path, cache = %cp, file_size, "transcode_cache_hit");
                let file_info = StreamInfo {
                    format: out_ext.clone(),
                    mime_type: out_mime.clone(),
                    sample_rate: out_sr,
                    bit_depth: out_bd,
                    channels,
                    file_size: Some(file_size),
                    duration_ms: Some(track_duration_ms as u64),
                    ..Default::default()
                };
                let session_id = self
                    .streamer
                    .create_file_session(file_info, cp.clone(), false)
                    .await;
                // The current track was a cache hit → warm the next one too,
                // so an album keeps hitting the cache track after track.
                self.spawn_warm_next_local(
                    req.zone_id,
                    sample_rate,
                    bit_depth,
                    channels,
                    out_ext.clone(),
                    out_sr,
                    out_bd,
                    target_format_str.clone(),
                );

                // …et les VU-mètres avec, sinon ils s'éteignent DÈS la
                // deuxième écoute.
                //
                // Le chemin du transcodage frais, juste en dessous, émet ses
                // niveaux depuis le `pcm_bytes` que lui rend
                // `transcode_source_to_file`. Un cache hit saute tout le
                // décodage — c'est son intérêt — donc plus une seule fenêtre
                // de PCM ne passe par ici, et rien n'attachait de forwarder :
                // aiguilles à zéro, spectrogramme plat, pour une lecture
                // pourtant parfaitement normale.
                //
                // Le symptôme suit exactement la mise en cache, ce qui le
                // rendait incompréhensible côté testeur : la PREMIÈRE écoute
                // d'une piste anime tout, chaque REPRISE est morte. Et il
                // frappe l'ALAC en premier parce que l'ALAC transcode
                // toujours pour un renderer réseau — il peuple donc ce cache
                // à chaque album, là où un FLAC part souvent en natif sans
                // jamais traverser ce bloc. Journaux d'Yves Corbat du
                // 01/09/2026 : 7 des 8 lectures de « Topography of Mind »
                // sont des cache hits, toutes sans niveaux.
                //
                // On décode la RENDITION mise en cache, pas la source : c'est
                // elle qui part au renderer, donc c'est elle que les aiguilles
                // doivent décrire. Aucune divergence à craindre au passage —
                // `cache_path_opt` est `None` dès qu'un EQ, une convolution ou
                // un ReplayGain est en jeu, donc une rendition en cache est
                // toujours du signal non traité.
                //
                // Décodage EN FLUX, le PCM part dans un puits, seules les
                // fenêtres ressortent : matérialiser la piste coûterait
                // ~1,9 Go sur un 24/192 de dix minutes, uniquement pour
                // animer des aiguilles.
                //
                // Par `spawn_local_file_levels_decode`, et pas en recopiant
                // la forme à la main. La première version de ce bloc s'était
                // modelée sur le décodage-pour-niveaux du PASSTHROUGH, qui
                // n'a jamais eu de frein (#1423) : elle en a hérité la forme
                // (flux, PCM au puits) mais pas le bridage que porte la
                // fonction ci-dessus — son puits drainait sans condition. Le
                // décodage courait alors à la vitesse du DISQUE pendant que
                // le forwarder ne publie qu'au temps réel, et la file du
                // forwarder — non bornée, chaque fenêtre portant son PCM —
                // retenait la piste ENTIÈRE. Le comble : le commentaire
                // ci-dessus invoquait les ~1,9 Go qu'il laissait revenir par
                // la file. Et le cache hit est le cas COURANT, pas le rare.
                //
                // On décode la rendition à son débit NATIF : la clef du
                // cache (`transcode_cache::cache_path`) couvre `out_sr`,
                // `out_bd` et `channels`, donc le fichier en cache est déjà
                // dans ce format — rééchantillonner vers lui ne changeait
                // rien.
                if let Some(bus) = ev_bus
                    .clone()
                    .filter(|_| self.levels_attach_allowed(zone_id))
                {
                    // Génération épinglée au moment de la décision (#1110) :
                    // ce décodage dure toute la piste, il ne doit pas pouvoir
                    // se raccrocher à la suivante.
                    let play_seq = playback.current_play_seq(zone_id).await;
                    // Cache hit : la rendition est servie depuis son début
                    // (un seek passe par Range HTTP).
                    spawn_local_file_levels_decode(
                        bus,
                        playback.clone(),
                        zone_id,
                        play_seq,
                        cp.clone(),
                    );
                }
                (
                    session_id,
                    out_mime,
                    out_ext,
                    Some(file_size),
                    Some(out_sr),
                    Some(out_bd as u32),
                    Some(channels as u32),
                )
            } else {
                info!(
                    file = %fp,
                    tmp = %tmp_path,
                    target = %target_format_str,
                    sample_rate = out_sr,
                    bit_depth = out_bd,
                    "transcode_to_temp_file_start"
                );

                // Target bit depth chosen above (out_bd). For the generic DLNA
                // WAV/LPCM fallback this is 16 (LPCM is a 16-bit-only profile);
                // the decoded PCM must actually be reduced to 16-bit here, not
                // merely relabelled — otherwise 24-bit samples are served under
                // a 16-bit WAV header and the renderer plays silence (#1137).
                let target_bd = out_bd;
                // Le budget doit suivre la TAILLE, pas une constante.
                //
                // 120 s fixes suffisaient tant qu'on transcodait du FLAC ;
                // ils ne suffisent plus pour du DSD. Journaux de Cyrille
                // (#1330, ampli Yamaha en zone PCM, source sur NAS) : un
                // FLAC DXD est prêt en ~6 s, un DSD128 en ~20 s, et un
                // mouvement de symphonie en DSD256 courait encore au-delà.
                // Passé le délai, la lecture ne démarre JAMAIS — d'où « le
                // DSD128 passe, le DSD256 non », qui n'a rien à voir avec
                // la fréquence (les deux visent 352,8 kHz) et tout à voir
                // avec le volume de données à décoder.
                let transcode_budget = transcode_budget_for(&fp);
                info!(
                    file = %file_path,
                    budget_s = transcode_budget.as_secs(),
                    "transcode_budget_selected"
                );
                // …mais la taille seule ne suffit pas : elle ignore la
                // VITESSE de la machine (#3140). `120 + 0,3154·D` en DSD256
                // n'est tenable qu'à partir de `× 3,17` temps réel ; Shrek
                // décode à `× 2,2`, et Tune livre de l'ARM64. La balise
                // publie l'audio déjà décodé, le chien de garde en tire le
                // facteur réel de CET hôte et n'ÉTEND le budget que si
                // celui de la taille ne suffit pas.
                let progres = crate::audio::decode_progress::DecodeProgress::new();
                let politique =
                    BudgetAdaptatif::new(track_duration_ms as f64 / 1000.0, transcode_budget);
                // Même raison que le pré-transcode DASH plus bas : la ligne
                // de fin doit porter sa propre durée, pour rester lisible
                // seule dans un export de journal tronqué par la rotation.
                let file_transcode_start = std::time::Instant::now();
                let transcode_result = transcoder_sous_budget(
                    transcode_source_to_file(
                        fp.clone(),
                        out_sr,
                        channels,
                        target_bd,
                        target_format_str.clone(),
                        eq_profile,
                        convolver,
                        replaygain_factor,
                        tmp_path.clone(),
                        Some(progres.clone()),
                    ),
                    progres,
                    politique,
                    PAS_SONDAGE_BUDGET,
                    Some(file_path.as_str()),
                )
                .await;

                match transcode_result {
                    Ok(Ok((file_size, pcm_bytes, actual_bd))) => {
                        if file_size < 1024 {
                            warn!(
                                file = %file_path,
                                file_size,
                                "transcode_produced_empty_file — source may be corrupted or encrypted"
                            );
                            let _ = std::fs::remove_file(&tmp_path);
                            return Err("transcode produced empty file (corrupted source?)".into());
                        }
                        // Promote the completed file into the cache (atomic rename
                        // within the temp dir) so the next identical request is a
                        // hit. If we're not caching, or the rename fails, serve the
                        // freshly-written file as before.
                        let serve_path = match cache_path_opt.as_ref() {
                            Some(cp) if std::fs::rename(&tmp_path, cp).is_ok() => {
                                tokio::task::spawn_blocking(crate::transcode_cache::evict);
                                cp.clone()
                            }
                            _ => tmp_path.clone(),
                        };
                        info!(
                            file = %file_path,
                            tmp = %serve_path,
                            file_size,
                            elapsed_ms = file_transcode_start.elapsed().as_millis() as u64,
                            "transcode_to_temp_file_complete"
                        );

                        // Emit audio levels in the background, paced to
                        // the playback clock by the forwarder. Pas pendant
                        // un pré-chargement gapless : la session décrit la
                        // piste SUIVANTE, ses niveaux partiraient datés de
                        // l'horloge de la piste courante.
                        if let Some(bus) = ev_bus
                            .clone()
                            .filter(|_| self.levels_attach_allowed(zone_id))
                        {
                            let playback = playback.clone();
                            let actual_ch = channels;
                            let sr = out_sr;
                            // Génération épinglée au moment de la décision,
                            // pas au démarrage de la tâche (#1110).
                            let play_seq = playback.current_play_seq(zone_id).await;
                            tokio::spawn(async move {
                                // Temp-file : le PCM décodé part du début
                                // du fichier (un seek passe par Range HTTP).
                                let levels_tx = spawn_paced_levels_forwarder(
                                    bus, playback, zone_id, play_seq, 0,
                                );
                                tokio::task::spawn_blocking(move || {
                                    crate::audio::tap::send_windowed_pcm(
                                        &levels_tx, &pcm_bytes, actual_bd, actual_ch, sr,
                                    );
                                })
                                .await
                                .ok();
                            });
                        }

                        // Create a file session — HTTP handler serves with
                        // Content-Length and Range support.
                        let file_info = StreamInfo {
                            format: out_ext.clone(),
                            mime_type: out_mime.clone(),
                            sample_rate: out_sr,
                            bit_depth: out_bd,
                            channels,
                            file_size: Some(file_size),
                            duration_ms: Some(track_duration_ms as u64),
                            ..Default::default()
                        };
                        let session_id = self
                            .streamer
                            .create_file_session(file_info, serve_path, false)
                            .await;

                        // Current track just transcoded into the cache → warm
                        // the next one in the background while this one plays,
                        // so the album transition is a cache hit (no 30s gap).
                        // Only when the current was actually cached (Some means
                        // no EQ) — warming an EQ zone would populate an entry
                        // the real (EQ) play never hits.
                        if cache_path_opt.is_some() {
                            self.spawn_warm_next_local(
                                req.zone_id,
                                sample_rate,
                                bit_depth,
                                channels,
                                out_ext.clone(),
                                out_sr,
                                out_bd,
                                target_format_str.clone(),
                            );
                        }
                        (
                            session_id,
                            out_mime,
                            out_ext,
                            Some(file_size),
                            Some(out_sr),
                            Some(out_bd as u32),
                            Some(channels as u32),
                        )
                    }
                    Ok(Err(e)) => {
                        warn!(error = %e, file = %file_path, "transcode_to_temp_file_failed");
                        let _ = std::fs::remove_file(&tmp_path);
                        return Err(format!("transcode failed: {e}"));
                    }
                    Err(depassement) => {
                        let budget_s = depassement.budget.as_secs();
                        let size_mb = std::fs::metadata(&fp)
                            .map(|m| m.len() / (1024 * 1024))
                            .unwrap_or(0);
                        let _ = std::fs::remove_file(&tmp_path);
                        // La moitié utile de #3140 : DIRE que c'est l'hôte.
                        //
                        // L'ancienne ligne n'annonçait qu'un délai dépassé
                        // et une taille, et envoyait chercher du côté du
                        // disque ou du réseau — alors que la cause est la
                        // vitesse du processeur, et qu'elle est désormais
                        // MESURÉE. On la nomme, avec son facteur et celui
                        // qu'il aurait fallu.
                        match (depassement.facteur, depassement.facteur_requis()) {
                            (Some(mesure), Some(requis)) => {
                                warn!(
                                    file = %file_path,
                                    budget_s,
                                    size_mb,
                                    track_s = depassement.piste_s,
                                    decoded_s = depassement.decode.as_secs_f64(),
                                    elapsed_s = depassement.ecoule.as_secs_f64(),
                                    host_realtime_factor = mesure,
                                    required_realtime_factor = requis,
                                    "transcode_timeout_host_too_slow"
                                );
                                return Err(format!(
                                    "transcode timeout after {budget_s}s: this HOST decodes \
                                     this file at \u{d7}{mesure:.2} real time and would need \
                                     \u{d7}{requis:.2} to finish a {track_min:.1} min track \
                                     \u{2014} the machine is too slow for this format, not the \
                                     disk and not the file ({size_mb} MB, \
                                     {decoded:.0}s of audio decoded)",
                                    track_min = depassement.piste_s / 60.0,
                                    decoded = depassement.decode.as_secs_f64(),
                                ));
                            }
                            // Rien n'a été mesuré (décodeur qui ne publie
                            // pas, durée de piste inconnue, blocage avant
                            // la première fenêtre) : le message d'avant,
                            // mot pour mot.
                            _ => {
                                warn!(
                                    file = %file_path,
                                    budget_s,
                                    size_mb,
                                    "transcode_timeout"
                                );
                                return Err(format!(
                                    "transcode timeout after {budget_s}s for a {size_mb} MB source \u{2014} \
                                     disk or network too slow, or the file is unusually large"
                                ));
                            }
                        }
                    }
                }
            }
        };
        Ok(flux)
    }

    /// Deuxième temps, sorties locales et PULL : session de flux alimentée par
    /// le décodeur, longueur WAV calculée pour la DIDL, canal gardé ouvert
    /// jusqu'à la fin du décodage.
    async fn transcoder_en_session(
        &self,
        req: &PlayRequest,
        decision: &DecisionLocale,
        format: FormatDeSortie,
    ) -> Result<FluxLocal, String> {
        let DecisionLocale {
            channels,
            ref file_path,
            ..
        } = *decision;
        let FormatDeSortie {
            out_sr,
            out_bd,
            out_mime,
            out_ext,
            info,
            ..
        } = format;
        let flux = {
            // ── Streaming transcode (WAV for local/OAAT) ──────────
            // Use the computed WAV content length for the DIDL size
            // attribute so DLNA renderers know the correct stream size.
            let transcode_file_size = info.wav_content_length();

            let (session_id, tx, data_ready) = self.streamer.create_session(info, false, 256).await;

            // Mark session: the streaming decoder sends the WAV header
            // with the real source sample rate, so the stream handler
            // must NOT prepend its own.
            {
                let sessions = self.streamer.sessions_state();
                let sessions = sessions.lock().await;
                if let Some(session) = sessions.get(&session_id) {
                    session
                        .wav_header_included
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }

            let fp = file_path.clone();
            let ev_bus = self.event_bus.clone();
            let playback = self.playback.clone();
            let zone_id = req.zone_id;
            let seek_s = req.seek_ms.map(|ms| ms as f64 / 1000.0).unwrap_or(0.0);
            let streamer_sessions = self.streamer.sessions_state();
            let close_session_id = session_id.clone();
            // Pré-chargement gapless : session de la piste suivante, pas
            // de forwarder (voir `levels_prewarm`).
            let attach_levels = self.levels_attach_allowed(zone_id);
            tokio::spawn(async move {
                debug!(file = %fp, sample_rate = out_sr, channels, "transcode_decoding");

                // Bus conservé pour signaler un échec de décodage au client :
                // un décodage transcodé qui échoue (codec non supporté, fichier
                // corrompu…) ne doit PLUS produire un flux silencieux qui boucle
                // toutes les ~2 s — on remonte une erreur visible.
                let err_bus = ev_bus.clone();

                // Forwarder cadencé si le bus existe ; sinon un canal dont
                // le récepteur est aussitôt abandonné (le décodeur ignore
                // les erreurs d'envoi).
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

                let fp_clone = fp.clone();
                let tx_clone = tx.clone();
                drop(tx);

                let result = tokio::task::spawn_blocking(move || {
                    crate::audio::decode::decode_to_pcm_streaming_seeked(
                        &fp_clone,
                        Some(out_sr),
                        Some(channels as u32),
                        Some(out_bd),
                        tx_clone,
                        32768,
                        data_ready,
                        levels_tx,
                        seek_s,
                    )
                })
                .await;

                match result {
                    Ok(Ok(_bit_depth)) => {
                        debug!(file = %fp, "transcode_complete_streaming");
                    }
                    Ok(Err(e)) => {
                        warn!(error = %e, file = %fp, "transcode_streaming_decode_failed");
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
                        warn!(error = %e, file = %fp, "transcode_streaming_task_panic");
                        if let Some(ref bus) = err_bus {
                            bus.emit(
                                "zone.playback_error",
                                serde_json::json!({
                                    "zone_id": zone_id,
                                    "error": "Le décodage de la piste a échoué (erreur interne).",
                                }),
                            );
                        }
                    }
                }

                // Signal EOF by dropping the keep-alive sender. The
                // decoder's tx is already dropped at this point, but the
                // _keep_alive_tx in the session keeps the channel open
                // until we explicitly close it here.
                let sessions = streamer_sessions.lock().await;
                if let Some(session) = sessions.get(&close_session_id) {
                    session.close_sender().await;
                }
            });

            (
                session_id,
                out_mime,
                out_ext,
                transcode_file_size,
                Some(out_sr),
                Some(out_bd as u32),
                Some(channels as u32),
            )
        };
        Ok(flux)
    }

    /// Arme « passthrough » du grand tuple de `resolve_local_track`, sortie telle
    /// quelle : le fichier est servi brut par une session de fichier, avec le
    /// décodage parallèle pour les niveaux quand personne d'autre ne décode.
    async fn servir_en_passthrough(
        &self,
        req: &PlayRequest,
        decision: &DecisionLocale,
    ) -> Result<
        (
            String,
            String,
            String,
            Option<u64>,
            Option<u32>,
            Option<u32>,
            Option<u32>,
        ),
        String,
    > {
        let DecisionLocale {
            bit_depth,
            channels,
            is_browser_output,
            is_network_output,
            sample_rate,
            source_format,
            track_duration_ms,
            track_file_size,
            ref file_path,
            ref fmt,
            ref zone,
            ..
        } = *decision;
        let flux = {
            // Standard passthrough: serve the raw file.
            // For DSD, use the MIME type declared by the renderer (from GetProtocolInfo)
            // instead of the generic application/x-dsd — some renderers (Yamaha R-N2000A)
            // only accept the specific MIME they advertise (e.g. audio/dsf).
            let mime = if source_format == Some(AudioFormat::Dsd) && is_network_output {
                let did = req
                    .output_device_id
                    .as_deref()
                    .or(zone.as_ref().and_then(|z| z.output_device_id.as_deref()))
                    .unwrap_or("");
                let cap = self.dsd_capabilities.lock().await;
                cap.get(did)
                    .and_then(|c| c.dsf_mime.clone())
                    .unwrap_or_else(|| "application/x-dsd".into())
            } else {
                source_format
                    .map(|f| f.mime_type().to_string())
                    .unwrap_or_else(|| "audio/flac".into())
            };

            // For a native passthrough served to a *network* renderer (DLNA
            // native FLAC, ALAC, DSD…), advertise the ACTUAL on-disk byte
            // length as `res@size` / HEAD Content-Length instead of the
            // scanned `track_file_size`.
            //
            // The GET handler (`serve_file`) always streams `disk_size` bytes,
            // but the DIDL `res@size` and the HEAD Content-Length are taken from
            // the DB `track_file_size`. When those disagree — the file was
            // re-tagged / had cover art (re)embedded after the scan, or was
            // scanned by an older/fallback code path — a renderer that models
            // playback position from `bytes_received / (size/duration)` (Marantz
            // ND 8006, native FLAC) reaches true EOF while its estimate still
            // reads position < duration, so it restarts/loops the track near the
            // end instead of advancing to the next queued item, and loses the
            // format/duration/progress display on that queued track (#1132).
            //
            // For a *compressed* stream (FLAC) we cannot derive duration from
            // size, but making `res@size` equal the exact bytes the renderer
            // will actually receive keeps its position model consistent — the
            // FLAC analogue of the WAV size/duration fix in 1046ae8e. Only the
            // network passthrough path is touched; local/OAAT/WAV-transcode
            // paths keep their existing sizing (they never reach this branch).
            let passthrough_disk_size = if is_network_output {
                tokio::fs::metadata(&file_path).await.ok().map(|m| m.len())
            } else {
                None
            };
            let passthrough_file_size =
                passthrough_disk_size.or_else(|| track_file_size.map(|s| s as u64));

            let info = StreamInfo {
                format: fmt.clone(),
                mime_type: mime.clone(),
                sample_rate,
                bit_depth,
                channels,
                file_size: passthrough_file_size,
                duration_ms: Some(track_duration_ms as u64),
                ..Default::default()
            };

            let session_id = self
                .streamer
                .create_file_session(info, file_path.clone(), false)
                .await;

            // For M4A/ALAC passthrough, attach an on-the-fly faststart map so the
            // file is served as `ftyp + patched-moov + mdat` (moov relocated to
            // the front). The renderer then reads its metadata up front and starts
            // immediately instead of seeking to the END of the file first — a slow
            // start + Range storm, esp. over a NAS mount (Yves, LHC-56, 192/24
            // ALAC on SMB). This reads only ftyp+moov (never mdat), so it adds no
            // copy latency, and falls back to the original file if not applicable.
            if source_format == Some(AudioFormat::Alac) {
                let fp = file_path.clone();
                // Two shapes to fix: (1) moov-after-mdat → relocate moov to the
                // front (and strip the cover on the way); (2) ALREADY faststart
                // (ftyp|moov|mdat) → moov stays put but its `covr` cover art still
                // makes the LHC-56 "ploc" at track start, so strip it in place.
                // prepare_faststart handles (1) and returns None for (2), which was
                // the gap: already-faststart files with artwork kept clicking
                // (Yves: "Do What U Will" / "ABOVE AND BEYOND"). Fall back to the
                // in-place cover strip. Both read only ftyp+moov (no mdat copy).
                let mapped = tokio::task::spawn_blocking(move || {
                    crate::audio::faststart::prepare_faststart(std::path::Path::new(&fp))
                        .map(|m| ("relocate", m))
                        .or_else(|| {
                            crate::audio::faststart::prepare_cover_strip_faststart(
                                std::path::Path::new(&fp),
                            )
                            .map(|m| ("cover_strip", m))
                        })
                })
                .await;
                if let Ok(Some((how, map))) = mapped {
                    info!(file = %file_path, how, "m4a_faststart_applied");
                    self.streamer.set_faststart(&session_id, map).await;
                }
            }

            // Parallel decode-for-levels: decode the audio in the background
            // purely to emit VU-meter events for the web client. This does not
            // affect the actual audio stream served to the output device.
            // Skip DSD (1-bit at MHz rates, can't decode inline for levels)
            // and exotic formats that need heavy conversion.
            let skip_passthrough_levels = source_format
                .as_ref()
                .is_some_and(|f| f.needs_transcode_for_dlna());
            // Ce decodage parallele n'a de sens que si PERSONNE d'autre ne
            // decode le fichier cote serveur : sortie reseau ou navigateur, qui
            // recoivent une URL et lisent eux-memes. Une sortie locale (comme
            // OAAT, AirPlay, HQPlayer ou le pont) decode deja pour alimenter le
            // peripherique, et son chemin de lecture emet ses propres niveaux :
            // on decodait donc la piste ENTIERE une seconde fois pour rien,
            // ~65 evenements/s au lieu de ~32, avec des horodatages qui
            // divergent apres un seek (l'un part du seek, l'autre de 0) et,
            // depuis #1106, des fenetres dupliquees sur le tap PCM (#1110).
            let output_decodes_server_side = !(is_network_output || is_browser_output);
            if !skip_passthrough_levels
                && !output_decodes_server_side
                && self.levels_attach_allowed(req.zone_id)
            {
                if let Some(ref bus) = self.event_bus {
                    let bus = bus.clone();
                    let playback = self.playback.clone();
                    let fp = file_path.clone();
                    let zone_id = req.zone_id;
                    let sr = sample_rate;
                    let ch = channels as u32;
                    // Génération épinglée au moment de la décision (#1110) :
                    // ce décodage complet dure toute la piste, il ne doit pas
                    // pouvoir se raccrocher à la suivante.
                    let play_seq = self.playback.current_play_seq(req.zone_id).await;
                    tokio::spawn(async move {
                        // Passthrough : le décodage pour niveaux part de 0.
                        let cadence = playback.clone();
                        let levels_tx =
                            spawn_paced_levels_forwarder(bus, playback, zone_id, play_seq, 0);
                        // Décodage EN FLUX, pas en une fois. `decode_to_pcm`
                        // matérialisait la piste entière en mémoire avant
                        // d'émettre la moindre fenêtre : ~1,9 Go pour un
                        // 24/192 de dix minutes, alloué à chaque début de
                        // piste et uniquement pour animer des aiguilles.
                        // C'est la même faute que #1109 (ReplayGain), un cran
                        // plus loin dans la chaîne. Le décodeur en flux émet
                        // les niveaux au fil de l'eau ; le PCM produit part
                        // dans un puits, seul l'ordre de grandeur du tampon
                        // reste en mémoire.
                        //
                        // ... mais un puits qui draine SANS CONDITION ne borne
                        // rien : il rend simplement au décodeur la vitesse du
                        // DISQUE, pendant que le forwarder ne publie qu'au
                        // TEMPS RÉEL. Sa file est non bornée et chaque fenêtre
                        // porte son PCM, si bien que les ~1,9 Go chassés de la
                        // porte d'entrée revenaient par la file — la rétention
                        // SUIVAIT la durée de la piste (mesuré sur un WAV
                        // 44,1/16 : 10 551 296 octets à 60 s, 21 102 592 à
                        // 120 s, extrapolé à ~380 Mo en 24/96). C'est le défaut
                        // d'origine de ce bloc (#1423), celui sur lequel #3104
                        // s'était modelé ; #3145 le corrige ici.
                        //
                        // Le frein passe par `spawn_braked_levels_sink`, et NON
                        // par `spawn_local_file_levels_decode` : cette
                        // dernière décode au débit NATIF du fichier, alors que
                        // le passthrough décode aux valeurs TAGUÉES de la piste
                        // (`Some(sr)` / `Some(ch)`, lues sur `tracks`). Sur un
                        // fichier bien tagué c'est identique ; sur un fichier
                        // mal tagué c'est un écart réel — et le passthrough est
                        // exactement le chemin de cette population-là. On
                        // freine donc SANS toucher à ce qui est décodé.
                        let (sink_tx, relais_tx) =
                            spawn_braked_levels_sink(cadence, zone_id, levels_tx);
                        let ready = std::sync::Arc::new(tokio::sync::Notify::new());
                        let result = tokio::task::spawn_blocking(move || {
                            crate::audio::decode::decode_to_pcm_streaming_with_levels(
                                &fp,
                                Some(sr),
                                Some(ch),
                                None,
                                sink_tx,
                                LEVELS_DECODE_CHUNK,
                                ready,
                                relais_tx,
                            )
                        })
                        .await;
                        match result {
                            Err(e) => debug!(error = %e, "passthrough_levels_task_panic"),
                            Ok(Err(e)) => debug!(error = %e, "passthrough_levels_decode_failed"),
                            Ok(Ok(_)) => {}
                        }
                    });
                }
            }

            (
                session_id,
                mime,
                fmt.clone(),
                passthrough_file_size,
                Some(sample_rate),
                Some(bit_depth as u32),
                Some(channels as u32),
            )
        };
        Ok(flux)
    }
}
