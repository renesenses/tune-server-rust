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
    /// LAT-F1 (phase 1) : le renderer a-t-il ANNONCÉ le LPCM à la profondeur
    /// qu'on lui servirait ? `hi_res` = plus de 16 bits, auquel cas
    /// `audio/L16` seul ne suffit pas — un WAV 24 bits servi à un renderer
    /// qui n'a annoncé que du 16 bits lit des échantillons désalignés et
    /// joue du SILENCE (#1137, Ruark R3).
    ///
    /// Mêmes conventions que `dlna_supports_mime` : une sortie absente du
    /// registre est présumée capable (elle n'est pas là pour dire le
    /// contraire), une sortie qui n'est pas un `DlnaOutput` n'a pas de Sink à
    /// lire donc répond NON, et une sonde inconcluante répond NON sans être
    /// mise en cache — la lecture suivante re-sonde. Les réponses concluantes
    /// sont mémorisées par renderer : une sonde SOAP par session, pas par
    /// morceau.
    /// `pub` et non `pub(super)` depuis #2742 : la route `/zones/{id}/dsp`
    /// doit pouvoir dire si le crossfeed a un chemin sur une zone réseau, et
    /// la réponse tient à cette sonde. Le résultat est mis en cache par
    /// renderer et profondeur, l'appel depuis la route est donc gratuit après
    /// le premier.
    pub async fn dlna_accepte_lpcm(&self, device_id: &str, hi_res: bool) -> bool {
        let cle = format!("{device_id}|{}", if hi_res { "24" } else { "16" });
        if let Some(connu) = self.dlna_lpcm_accepte.lock().await.get(&cle) {
            return *connu;
        }
        let arc = { self.outputs.lock().await.get(device_id) };
        let Some(output) = arc else {
            return true;
        };
        let caps = {
            let locked = output.lock().await;
            let Some(dlna) = locked
                .as_any()
                .downcast_ref::<crate::outputs::dlna::DlnaOutput>()
            else {
                return false;
            };
            dlna.probe_capabilities().await
        };
        if !caps.probed {
            return false;
        }
        let accepte = caps.wav || caps.lpcm24 || (!hi_res && caps.lpcm16);
        tracing::info!(
            device_id,
            hi_res,
            accepte,
            wav = caps.wav,
            lpcm16 = caps.lpcm16,
            lpcm24 = caps.lpcm24,
            "dlna_lpcm_capability_probed"
        );
        self.dlna_lpcm_accepte.lock().await.insert(cle, accepte);
        accepte
    }
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
        // #3270 (point 4) — un REFUS NOMMÉ avant toute promesse de lecture.
        //
        // C'est le point unique que traversent les DEUX entrées d'un fichier
        // téléversé : `resolve_stream` (`commun.rs`, branche `source ==
        // "upload"`) et `resoudre_la_demande` (`transport.rs`, branche
        // `req.temp_file_path`). Sans lui, `AudioFormat::from_extension` rendait
        // `None` pour un `.wma` ou un `.iso` et ce `None` était absorbé douze
        // lignes plus bas par `unwrap_or("audio/wav")` : le fichier obtenait une
        // session de flux annoncée `audio/wav`, la sortie ne décodait rien, et
        // la zone se taisait sans un mot.
        //
        // La sentinelle `format_not_playable:` est celle de #3234 :
        // `play_error_response` (`tune-server/src/routes/playback.rs`) la
        // transforme déjà en `422 {"error":"format_not_playable","message":…}`.
        // On n'ouvre pas un second canal pour dire la même chose.
        if let Some(motif) = crate::audio::support::refus_de_televersement(path) {
            warn!(
                zone_id = req.zone_id,
                file = %file_path,
                %motif,
                "uploaded_file_format_not_playable"
            );
            return Err(format!("format_not_playable:{motif}"));
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

    /// L'URL de lecture rangée par l'indexation dans l'instantané de la piste.
    ///
    /// Une seule clé, une seule ligne : `track_metadata(track_id, key)` est la
    /// clé primaire de la table, donc un seul saut d'index. Rend `None` si la
    /// piste n'a pas été indexée, si l'instantané est incomplet, ou si la base
    /// est illisible — trois cas que l'appelant traite de la même façon : il le
    /// DIT plutôt que de lancer quelque chose au hasard.
    pub(super) fn url_de_lecture_indexee(&self, track_id: i64) -> Option<String> {
        use crate::db::backend::ToSqlValue;
        self.db
            .query_one(
                "SELECT value FROM track_metadata WHERE track_id = ? AND key = ?",
                &[
                    &track_id as &dyn ToSqlValue,
                    &CLE_URL_DE_LECTURE_UPNP as &dyn ToSqlValue,
                ],
            )
            .ok()
            .flatten()
            .and_then(|ligne| ligne.first().and_then(|v| v.as_string()))
            .filter(|u| est_une_url_http(u))
    }

    /// **#4323 — une URI qui désigne une piste de NOTRE bibliothèque doit
    /// retrouver son `track_id`, et donc son vrai titre.**
    ///
    /// Le MediaRenderer reçoit un `SetAVTransportURI` et construit un
    /// `PlayRequest` avec `track_id: None`, `source_id: Some(<URI>)` et un
    /// `title` qui vient UNIQUEMENT du DIDL envoyé par le point de contrôle.
    /// Quand ce DIDL est vide — ou illisible —, `resolve_direct_url_de_source`
    /// retombe sur « Episode », le repli du chemin podcast/radio. Chez Tades
    /// (fil 1819), neuf cartes « Episode » sans pochette dans « Récemment
    /// joué », pour des lectures dont l'URI était
    /// `http://192.168.0.167:8888/api/v1/library/tracks/187500/audio` :
    /// c'est-à-dire des pistes que Tune connaît par cœur.
    ///
    /// Cette URI n'est pas quelconque : c'est EXACTEMENT ce que le serveur
    /// média de Tune publie dans le `<res>` de chaque piste
    /// ([`crate::upnp_server::track_audio_url`]). Le point de contrôle l'a
    /// recopiée telle quelle. On la relit donc à l'envers, et on repose sur la
    /// demande ce que la ligne de bibliothèque dit déjà.
    ///
    /// Trois choix, tous restrictifs :
    ///
    /// 1. **On ne contredit jamais le demandeur.** Un `track_id` déjà posé, un
    ///    titre déjà nommé par le DIDL, un artiste déjà là : on ne comble que
    ///    des silences. Un point de contrôle qui envoie de bonnes métadonnées
    ///    garde les siennes.
    /// 2. **`source` et `source_id` ne bougent PAS.** Le renderer reconnaît sa
    ///    session en comparant `now_playing.source_id` à l'URI de la session
    ///    (`doit_reprendre`), et la reprise après pause en dépend. Le chemin
    ///    des octets reste celui d'aujourd'hui : `url_nommee` l'emporte dans
    ///    `resolve_direct_url_de_source`, l'URI est jouée telle quelle.
    /// 3. **L'hôte doit être le nôtre** ([`hote_de_cette_machine`]). Un second
    ///    Tune sur le même réseau publie SES `<res>` avec SON adresse, et
    ///    l'identifiant 187500 n'y désigne pas la même piste. Afficher un
    ///    titre FAUX serait pire que « Episode ». Quand l'hôte ne se
    ///    reconnaît pas, on ne fait rien et le comportement d'avant demeure.
    pub(super) fn resoudre_l_uri_en_piste_de_bibliotheque(&self, req: &mut PlayRequest) {
        if req.track_id.is_some() {
            return;
        }
        let Some(track_id) = req
            .source_id
            .as_deref()
            .and_then(track_id_dans_une_url_audio_de_tune)
        else {
            return;
        };
        let Some(piste) = crate::db::track_repo::TrackRepo::with_backend(self.db.clone())
            .get(track_id)
            .ok()
            .flatten()
        else {
            // L'URL a la bonne forme mais la ligne n'existe pas (piste
            // supprimée, base d'un autre serveur derrière la même adresse) :
            // rien n'est affirmé.
            return;
        };
        let titre_etait_vide = req.title.is_none();
        req.track_id = Some(track_id);
        if req.title.is_none() {
            req.title = Some(piste.title.clone());
        }
        if req.artist_name.is_none() {
            req.artist_name = piste.artist_name.clone();
        }
        if req.album_title.is_none() {
            req.album_title = piste.album_title.clone();
        }
        if req.cover_url.is_none() {
            req.cover_url = piste.cover_path.clone();
        }
        if req.duration_ms.is_none() && piste.duration_ms > 0 {
            req.duration_ms = Some(piste.duration_ms);
        }
        info!(
            zone_id = req.zone_id,
            track_id,
            titre_etait_vide,
            titre = %piste.title,
            "uri_du_renderer_resolue_en_piste_de_bibliotheque"
        );
    }

    /// #4362 — le serveur multimédia d'où vient cette piste est-il ABSENT, au
    /// sens exact où la bibliothèque affiche « Serveur absent » ?
    ///
    /// Deux lectures étroites et sans effet : le `source_id` de la ligne (qui
    /// porte l'UDN en préfixe depuis la phase 2 de #2219), puis le registre
    /// durable des serveurs — quelques unités, jamais des dizaines de milliers
    /// de lignes, et seulement sur le chemin d'une piste indexée.
    ///
    /// Le verdict lui-même n'est pas rendu ici : il est délégué à la
    /// qualification que la route `/network/media-servers` appelle déjà
    /// (`discovery/presence_serveur.rs`), plafond de bascule en masse compris.
    /// Deux implémentations auraient fini par dire deux choses différentes du
    /// même serveur, et l'auditeur aurait vu un badge qui contredit un refus.
    ///
    /// `None` à la moindre incertitude — piste sans `source_id` exploitable,
    /// serveur inconnu du registre, base illisible. Un défaut de base ne doit
    /// pas se muer en refus de lecture.
    pub(super) fn serveur_de_la_piste_absent(
        &self,
        track_id: i64,
    ) -> Option<super::serveur_source_absent_4362::ServeurAbsent> {
        use crate::db::backend::ToSqlValue;
        let source_id: String = self
            .db
            .query_one(
                "SELECT source_id FROM tracks WHERE id = ?",
                &[&track_id as &dyn ToSqlValue],
            )
            .ok()
            .flatten()
            .and_then(|ligne| ligne.first().and_then(|v| v.as_string()))?;
        let udn = super::serveur_source_absent_4362::udn_de_la_piste(&source_id)?;
        let registre = crate::db::media_server_repo::MediaServerRepo::with_backend(self.db.clone())
            .lister()
            .ok()?;
        super::serveur_source_absent_4362::serveur_absent(&registre, udn)
    }

    pub(super) async fn resolve_direct_url(
        &self,
        req: &PlayRequest,
    ) -> Result<ResolvedStream, String> {
        self.resolve_direct_url_de_source(req, None).await
    }

    /// Comme [`Self::resolve_direct_url`], mais la source peut être IMPOSÉE par
    /// l'appelant.
    ///
    /// `PlayRequest.source` vient du corps de la demande, et le bouton Lecture
    /// n'en envoie pas : une piste indexée depuis un serveur UPnP arrive avec
    /// `source = None`. `resolve_stream` lit alors la source sur la LIGNE et la
    /// passe ici, faute de quoi le repli `"podcast"` ci-dessous s'appliquerait
    /// et aucune des branches `upnp` — ni le refus OAAT — ne verrait le jour.
    pub(super) async fn resolve_direct_url_de_source(
        &self,
        req: &PlayRequest,
        source_de_la_ligne: Option<&str>,
    ) -> Result<ResolvedStream, String> {
        // **Où la lecture trouve l'URL d'une piste indexée.**
        //
        // `tracks.source_id` porte l'IDENTITÉ d'une piste distante depuis la
        // phase 2 — un condensat `<udn>|<hex>` —, parce que ni l'`ObjectID` ni
        // l'URL de `res` ne sont stables (les deux portent l'identifiant du
        // conteneur parent ; mesuré sur Asset le 14/09). L'URL de lecture vit
        // donc dans l'instantané d'affichage, `track_metadata.upnp_res_url`,
        // exactement comme `streaming_item_tags` range ce qu'il faut pour
        // afficher sans interroger le service.
        //
        // L'ordre ci-dessous n'est pas indifférent :
        //
        // 1. une URL http(s) NOMMÉE dans la demande gagne toujours. C'est le
        //    chemin du renderer (`upnp_media_renderer.rs`), qui reçoit un
        //    `SetAVTransportURI` et n'a pas de ligne en base : le contredire
        //    casserait un chemin éprouvé ;
        // 2. sinon, l'instantané de la ligne ;
        // 3. sinon, `source_id` tel quel — le contrat inchangé de `radio`,
        //    `podcast` et `bandcamp`, dont le `source_id` EST l'URL.
        let source_apparente = source_de_la_ligne
            .map(str::to_string)
            .or_else(|| req.source.clone())
            .unwrap_or_else(|| "podcast".into());
        let url_nommee = req
            .source_id
            .as_deref()
            .filter(|u| est_une_url_http(u))
            .map(str::to_string);
        let url_indexee = if url_nommee.is_none() && source_apparente == "upnp" {
            req.track_id.and_then(|id| self.url_de_lecture_indexee(id))
        } else {
            None
        };
        let depuis_l_instantane = url_indexee.is_some();
        let url_retenue = url_nommee.or(url_indexee);
        let raw_url = url_retenue
            .as_deref()
            .or(req.source_id.as_deref())
            .ok_or("source_id (audio URL) required for podcast/radio playback")?;
        // Une piste indexée dont l'instantané ne porte aucune URL jouable ne
        // doit pas partir « au cas où » : `source_id` est alors le condensat
        // d'identité, et le pousser à une sortie produirait une erreur de
        // décodage illisible, ou pire, un silence. On le dit.
        if source_apparente == "upnp" && !est_une_url_http(raw_url) {
            return Err(format!(
                "Lecture impossible : « {} » est indexée depuis un serveur \
                 multimédia, mais aucune URL de lecture n'est enregistrée pour \
                 elle. Relancer l'indexation de ce serveur \
                 (POST /network/media-servers/<id>/indexer) la rétablira.",
                req.title.as_deref().unwrap_or("cette piste")
            ));
        }
        // Au niveau INFO, donc visible avec le `log_level` ordinaire : c'est la
        // ligne qui dit, pour une piste indexée, D'OÙ vient l'adresse jouée.
        // Posée en `debug!` elle n'aurait fait que changer de silence, et le
        // témoin de la phase 3 n'aurait rien à lire.
        if depuis_l_instantane {
            info!(
                track_id = ?req.track_id,
                zone_id = req.zone_id,
                url = %raw_url,
                "upnp_url_de_lecture_lue_dans_l_instantane"
            );
        }
        // #4362 — l'URL existe, mais le SERVEUR qui la sert répond-il encore ?
        //
        // La bibliothèque le savait déjà : le badge « Serveur absent » vient du
        // registre durable `media_servers` et de la qualification qui en tire
        // `presence` / `proposable` (`GET /network/media-servers`). Ce chemin-ci
        // ne l'avait jamais consulté : il lisait l'adresse et la poussait. Avec
        // Asset arrêté, l'Eversolo recevait une URL morte et jouait du silence
        // sans qu'une seule ligne d'erreur soit écrite.
        //
        // La garde ne s'arme que sur `depuis_l_instantane`, c'est-à-dire sur une
        // piste de BIBLIOTHÈQUE dont on a retrouvé l'adresse. Une URL NOMMÉE
        // dans la demande — le chemin du renderer, qui reçoit un
        // `SetAVTransportURI` et n'a pas de ligne en base — reste hors sujet et
        // n'est pas contredite, exactement comme le dit la règle 1 plus haut.
        if depuis_l_instantane
            && let Some(track_id) = req.track_id
            && let Some(absent) = self.serveur_de_la_piste_absent(track_id)
        {
            warn!(
                track_id,
                zone_id = req.zone_id,
                url = %raw_url,
                raison = absent.raison.code(),
                depuis_secs = absent.depuis_secs,
                "upnp_refus_serveur_source_absent"
            );
            return Err(super::serveur_source_absent_4362::motif_du_refus(&absent));
        }
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
        let source = source_apparente;
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
        // ------------------------------------------------------------------
        // D4 — « jouable partout, défauts assumés et DITS » (Bertrand, 14/09).
        //
        // OAAT est la SEULE sortie qui ne peut pas jouer une piste de serveur
        // UPnP, et elle ne peut pas le dire par elle-même : un point de sortie
        // OAAT « ne consomme que du PCM en conteneur WAV » (voir
        // `decoder_bandcamp_en_wav`, plus bas). Lui pousser le FLAC ou le MP3
        // d'un serveur média tel quel produit un SILENCE — pas une erreur, pas
        // un voyant rouge : une zone qui dit « en lecture » et ne joue rien.
        //
        // Les deux autres sources qui passent par ici ont chacune leur bras de
        // décodage vers OAAT (`is_radio`, `is_bandcamp`) ; ce chemin-ci n'en a
        // jamais eu. Plutôt que de faire semblant, on refuse AVANT de lancer
        // quoi que ce soit : `resoudre_la_demande` abaisse le drapeau
        // « recherche en cours » et remonte ce motif sans qu'un seul octet ne
        // parte vers le point de sortie.
        //
        // Le refus est fermé sur trois conditions, pour ne rien casser de ce
        // qui marche : la source EST `upnp`, la sortie EST OAAT, et le flux
        // amont n'est PAS déjà du WAV — un serveur qui publie du
        // `audio/wav` (Asset le propose en `.forced.wav`) reste jouable et
        // continue de passer.
        //
        // Le motif n'est plus rédigé ici. Il vient de `verdict_upnp`, la table
        // unique de D4, que `routes/playback.rs` lit AUSSI pour annoncer les
        // dégradations des trois sorties jouantes : deux textes écrits
        // séparément auraient divergé au premier correctif.
        //
        // 🔴 Ce motif était écrit à la main, et il était ABÎMÉ : les
        // continuations de chaîne avaient été perdues à l'écriture, si bien que
        // le message livré portait des suites de dix-huit espaces en plein
        // milieu de ses phrases. Personne ne l'a vu — les témoins cherchaient
        // « OAAT », « WAV », « silence », des mots isolés qu'un texte crevé
        // contient tout aussi bien. Le témoin exige désormais une PHRASE
        // entière (`temoins_du_refus_oaat`, et le banc de route), ce qui est le
        // seul contrôle qui aurait rougi.
        if source == "upnp"
            && !crate::orchestrator::verdict_upnp::SortieD4::Oaat.joue_un_flux_compresse()
            && is_oaat_output
            && !est_du_wav(mime_type)
        {
            let titre = req.title.as_deref().unwrap_or("cette piste");
            return Err(crate::orchestrator::verdict_upnp::motif_du_refus_oaat(
                titre, mime_type,
            ));
        }
        // ------------------------------------------------------------------

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
                // 🔴 #4311 — le relais passe les octets VERBATIM : rien n'est
                // décodé côté serveur, donc aucun niveau n'était mesuré.
                let codec = d.bc_quality.as_ref().map(|q| q.codec).unwrap_or("mp3");
                self.sonder_les_niveaux_bandcamp(req.zone_id, d.audio_url, codec)
                    .await;
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
                //
                // 🔴 #4311 — « rien à interposer » valait pour le FLUX, pas
                // pour les NIVEAUX : `LocalOutput` décode mais ne mesure rien
                // (`outputs/local.rs` n'a aucune occurrence de `levels`), et
                // ce bras ne lançait aucune sonde. Spectre et bargraphe
                // restaient au plancher sur toute lecture Bandcamp en sortie
                // locale (GgB, 0.9.153, « HDA Intel PCH »).
                let codec = bc_quality.as_ref().map(|q| q.codec).unwrap_or("mp3");
                self.sonder_les_niveaux_bandcamp(req.zone_id, audio_url, codec)
                    .await;
                (
                    audio_url.to_string(),
                    None,
                    mime_type.to_string(),
                    Some(44100u32),
                    Some(16u32),
                    Some(2u32),
                )
            } else if is_browser_output {
                // 🔴 #2076 / #2158 generalises — le DERNIER bras direct
                // (serveur multimedia, podcast) rendait encore l'URL amont
                // telle quelle, sans regarder si la sortie etait un onglet.
                //
                // Le client web reecrit une URL absolue en chemin relatif pour
                // joindre l'hote qu'il a su atteindre. Sur une URL TIERCE, cela
                // jette le domaine : l'onglet demande `cdn.exemple.org/…` A
                // TUNE, qui ne connait pas ce chemin et repond par son repli
                // SPA — `200 text/html`, « Failed to init decoder ». C'est mot
                // pour mot la panne de Bilou (#2076, fil 1509), et les deux
                // autres bras l'ont deja corrigee chacun de leur cote :
                // Bandcamp par `relayer_bandcamp_au_reseau`, la radio par
                // #2670, dont le commentaire nomme explicitement la meme cause.
                //
                // On relaie donc les octets VERBATIM, comme Bandcamp : aucun
                // transcodage, la resolution annoncee par l'appelant est
                // conservee (un ALAC 24 bits d'un NAS ne doit pas se retrouver
                // etiquete 44,1/16 — Yves), et l'URL rendue est une adresse de
                // Tune, que le client peut reecrire sans rien casser.
                self.relayer_direct_au_navigateur(req, d).await
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
        // #3756 — la zone redemande la station : on oublie le verdict définitif
        // qu'une tentative précédente avait pu porter sur elle. Un geste
        // explicite de l'auditeur a toujours le droit de réessayer ; c'est la
        // RELANCE AUTOMATIQUE du sondeur, et elle seule, que la mémoire borne.
        self.oublier_radio_refusee(req.zone_id);
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
        // #3973 — lu avant la tâche détachée, comme la zone elle-même.
        let radio_strict = crate::audio::bitperfect_strict::zone_enabled(&self.db, req.zone_id);
        let err_station = title.clone();
        // #3756 — de quoi RETENIR l'échec, pas seulement le dire. Le sondeur
        // ne voit que le `Ok` de `play()` ; sans cette mémoire il relance une
        // station que le décodeur vient de déclarer irrécupérable.
        let refusees = self.radios_refusees.clone();
        let refus_source_id = req.source_id.clone();
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
                    radio_strict,
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
                    // Verdict DÉFINITIF ou simple panne ? Les deux préfixes
                    // ci-dessous portent déjà, chacun dans son commentaire, la
                    // phrase « ne guérira pas en réessayant » — mais personne
                    // ne la lisait hors de la boucle de reconnexion interne.
                    // Une coupure réseau, elle, n'entre pas ici : la reprise
                    // légitime d'un flux qui tombe continue de marcher.
                    let definitif = e.starts_with(super::radio::RADIO_NOT_AUDIO)
                        || e.starts_with(super::radio::RADIO_HLS_UNSUPPORTED);
                    if let (true, Some(sid)) = (definitif, refus_source_id.as_deref()) {
                        warn!(
                            zone_id = err_zone,
                            source_id = sid,
                            error = %e,
                            "radio_echec_definitif_relance_desarmee_3756"
                        );
                        PlaybackOrchestrator::noter_radio_refusee(&refusees, err_zone, sid);
                    }
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
    /// #4311 — niveaux d'une lecture Bandcamp que le serveur NE DÉCODE PAS :
    /// sortie locale (`LocalOutput` décode lui-même et ne mesure rien) et
    /// relais réseau/navigateur (octets verbatim). Sans sonde, aucun
    /// `playback.audio_levels` ne partait pour la zone — spectre et bargraphe
    /// inertes, les deux à la fois, exactement ce que GgB décrit. Même geste
    /// que le proxy Qobuz/Tidal (`resolve_stream.rs`, #1106) : une seconde
    /// connexion décodée pour les seuls niveaux, le flux joué n'est pas
    /// touché. L'indice de codec vient de l'URL (`mp3-128`, `flac`), repli
    /// `mp3` de l'écoute libre ; le sondeur reconnaît de toute façon le
    /// conteneur.
    async fn sonder_les_niveaux_bandcamp(&self, zone_id: i64, audio_url: &str, codec: &str) {
        self.spawn_proxy_levels_probe(zone_id, audio_url.to_string(), codec.to_string())
            .await;
    }

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
        let bc_strict = crate::audio::bitperfect_strict::zone_enabled(&self.db, req.zone_id);
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
                decode_radio_stream_to_pcm(
                    bc_url,
                    tx,
                    data_ready,
                    session,
                    None,
                    bc_levels_tx,
                    bc_strict,
                )
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
    /// Relayer une URL TIERCE vers l'onglet, octet pour octet (#2076).
    ///
    /// Meme geste que [`Self::relayer_bandcamp_au_reseau`] — une session proxy
    /// locale, `create_proxy_session(..., false)` — mais pour le bras generique
    /// : serveur multimedia (UPnP/DLNA) et podcast.
    ///
    /// Rien n'est transcode. Le conteneur annonce est deduit de l'URL, et la
    /// resolution que l'appelant a portee depuis les attributs `res@` du DIDL
    /// est conservee telle quelle : c'est ce qui empeche un ALAC 24 bits d'un
    /// NAS d'etre affiche en 44,1 kHz / 16 bits.
    async fn relayer_direct_au_navigateur(&self, req: &PlayRequest, d: Directe<'_>) -> FluxDirect {
        let Directe {
            audio_url,
            mime_type,
            duration_ms,
            ..
        } = d;
        let conteneur = conteneur_depuis_url(audio_url, mime_type);
        let info = StreamInfo {
            format: conteneur.to_string(),
            mime_type: mime_type.to_string(),
            sample_rate: req.sample_rate.unwrap_or(44_100),
            bit_depth: req.bit_depth.unwrap_or(16),
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
            .get_stream_url(&session_id, &server_ip, conteneur);
        info!(
            url = %audio_url,
            conteneur,
            "direct_proxy_for_browser_output"
        );
        (
            stream_url,
            Some(session_id),
            mime_type.to_string(),
            req.sample_rate,
            req.bit_depth.map(|b| b as u32),
            None,
        )
    }
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
        // #3756 — la zone redemande la station : on oublie le verdict définitif
        // porté par une tentative précédente (voir `decoder_la_radio_en_wav`).
        self.oublier_radio_refusee(req.zone_id);
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
            // #3973 — lu avant la tâche détachée, comme la zone elle-même.
            let radio_strict = crate::audio::bitperfect_strict::zone_enabled(&self.db, req.zone_id);
            let err_station = title.clone();
            // #3756 — même mémoire que le chemin local/OAAT. Le journal du
            // ticket vient d'une sortie ALSA, mais rien dans la boucle de
            // relance du sondeur ne distingue les deux : armer un seul des
            // deux chemins laisserait la relance sans fin sur l'autre.
            let refusees = self.radios_refusees.clone();
            let refus_source_id = req.source_id.clone();
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    decode_radio_stream_to_pcm(
                        radio_url,
                        tx,
                        data_ready,
                        session,
                        radio_eq_profile.clone(),
                        radio_levels_tx,
                        radio_strict,
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
                        let definitif = e.starts_with(super::radio::RADIO_NOT_AUDIO)
                            || e.starts_with(super::radio::RADIO_HLS_UNSUPPORTED);
                        if let (true, Some(sid)) = (definitif, refus_source_id.as_deref()) {
                            warn!(
                                zone_id = err_zone,
                                source_id = sid,
                                error = %e,
                                "radio_echec_definitif_relance_desarmee_3756"
                            );
                            PlaybackOrchestrator::noter_radio_refusee(&refusees, err_zone, sid);
                        }
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

/// Le conteneur a annoncer pour une URL relayee verbatim.
///
/// L'extension de l'URL fait foi — c'est elle que le serveur amont a choisie.
/// Faute d'extension reconnue, on retombe sur ce que dit le type MIME, et en
/// dernier ressort sur `mp3`, l'encodage le plus repandu sur ces deux chemins
/// (podcast, serveur multimedia). Le conteneur ne sert qu'a nommer l'extension
/// de l'adresse rendue : les octets, eux, passent tels quels.
fn conteneur_depuis_url(url: &str, mime: &str) -> &'static str {
    let chemin = url.split(['?', '#']).next().unwrap_or(url).to_lowercase();
    for (suffixe, conteneur) in [
        (".flac", "flac"),
        (".wav", "wav"),
        (".m4a", "m4a"),
        (".mp4", "m4a"),
        (".aac", "aac"),
        (".ogg", "ogg"),
        (".opus", "opus"),
        (".mp3", "mp3"),
    ] {
        if chemin.ends_with(suffixe) {
            return conteneur;
        }
    }
    match mime {
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/wav" | "audio/x-wav" | "audio/wave" => "wav",
        "audio/mp4" | "audio/m4a" | "audio/x-m4a" => "m4a",
        "audio/aac" => "aac",
        "audio/ogg" | "application/ogg" => "ogg",
        "audio/opus" => "opus",
        _ => "mp3",
    }
}

/// Ce flux est-il déjà du PCM en conteneur WAV ?
///
/// Le seul contenu qu'un point de sortie OAAT sait ouvrir. La liste est celle
/// des types que le dépôt écrit ou reconnaît déjà pour du WAV
/// (`conteneur_depuis_url` ci-dessus, `decoder_bandcamp_en_wav`), plus
/// `audio/vnd.wave`, la forme enregistrée à l'IANA que certains serveurs
/// publient. La comparaison ignore la casse et les paramètres qui suivent le
/// point-virgule (`audio/wav; charset=…`), parce qu'un `protocolInfo` DLNA en
/// porte.
pub(crate) fn est_du_wav(mime: &str) -> bool {
    let base = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    matches!(
        base.as_str(),
        "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave"
    )
}

#[cfg(test)]
mod temoins_du_refus_oaat {
    use super::est_du_wav;

    /// Ce que le refus laisse passer — sans cette liste, un serveur qui publie
    /// du WAV (Asset le fait, en `.forced.wav`) serait refusé pour rien.
    #[test]
    fn le_wav_reste_jouable_sur_oaat() {
        for mime in [
            "audio/wav",
            "audio/x-wav",
            "audio/wave",
            "audio/vnd.wave",
            "AUDIO/WAV",
            "audio/wav; charset=binary",
        ] {
            assert!(est_du_wav(mime), "{mime} devrait être reconnu comme du WAV");
        }
    }

    /// **La contre-épreuve du refus** : ce sont EXACTEMENT ces types que les
    /// serveurs médias publient en premier `res`, et ceux qu'OAAT ne sait pas
    /// ouvrir. Si `est_du_wav` devenait laxiste, le silence reviendrait sans
    /// qu'un seul test ne rougisse ailleurs.
    #[test]
    fn tout_le_reste_ne_l_est_pas() {
        for mime in [
            "audio/x-flac",
            "audio/flac",
            "audio/mpeg",
            "audio/mp4",
            "audio/aac",
            "application/x-dsd",
            // `audio/L16` est du PCM, mais SANS conteneur : c'est justement le
            // flux « headerless » sur lequel les renderers s'étranglent
            // (`res_format_rank`, routes/network.rs). Il ne passe pas.
            "audio/L16",
            "",
        ] {
            assert!(
                !est_du_wav(mime),
                "{mime} ne doit PAS être pris pour du WAV : OAAT n'en tirerait qu'un silence"
            );
        }
    }
}

/// La clé sous laquelle l'indexation range l'URL de lecture d'une piste
/// distante (`routes/indexation_upnp.rs`, phase 2).
///
/// Elle est répétée ici et pas importée : `tune-core` ne dépend pas de
/// `tune-server`, et c'est dans ce sens que va la dépendance. Le témoin
/// `la_lecture_et_l_indexation_parlent_de_la_meme_cle` exige que les deux
/// littéraux restent égaux — une divergence rendrait toute piste indexée
/// injouable, en silence.
pub(crate) const CLE_URL_DE_LECTURE_UPNP: &str = "upnp_res_url";

/// Cette chaîne est-elle une URL qu'on peut aller chercher en HTTP ?
///
/// Le seul test qui sépare une URL de lecture du CONDENSAT D'IDENTITÉ que
/// `tracks.source_id` porte depuis la phase 2 (`uuid:…|9f3c…`). Sans lui, le
/// condensat partirait à la sortie comme s'il était une adresse.
pub(crate) fn est_une_url_http(valeur: &str) -> bool {
    let v = valeur.trim();
    (v.starts_with("http://") || v.starts_with("https://")) && v.len() > "https://".len()
}

#[cfg(test)]
mod temoins_de_l_url_indexee {
    use super::{CLE_URL_DE_LECTURE_UPNP, est_une_url_http};

    /// Le condensat d'identité ne doit JAMAIS être pris pour une adresse.
    #[test]
    fn un_condensat_d_identite_n_est_pas_une_url() {
        for valeur in [
            "uuid:258FC2D5-E2C3-B734-0-123456789abc|85944171f73967e8",
            "track/21825",
            "",
            "   ",
            "https://",
            "d6120941636376083059-co4E8D6A18CD1AC698",
        ] {
            assert!(
                !est_une_url_http(valeur),
                "« {valeur} » ne doit pas passer pour une URL de lecture"
            );
        }
    }

    /// …et une vraie URL de `res` doit passer, http comme https.
    #[test]
    fn une_url_de_res_passe() {
        for valeur in [
            "http://192.168.1.41:26125/content/c2/b16/f44100/d61-coX.flac",
            "https://192.168.1.42:8888/api/v1/library/tracks/21825/audio",
            "  http://192.168.1.18:8888/x.wav  ",
        ] {
            assert!(est_une_url_http(valeur), "« {valeur} » devrait passer");
        }
    }

    /// La clé de l'instantané est un contrat entre DEUX caisses : l'indexation
    /// l'écrit dans `tune-server`, la lecture la relit dans `tune-core`. Si
    /// l'une des deux change de nom, plus aucune piste indexée ne joue — et
    /// rien d'autre ne rougirait.
    #[test]
    fn la_lecture_et_l_indexation_parlent_de_la_meme_cle() {
        assert_eq!(
            CLE_URL_DE_LECTURE_UPNP, "upnp_res_url",
            "la clé lue par la lecture a changé : verifier \
             `routes/indexation_upnp.rs::CLE_URL_DE_LECTURE`"
        );
    }
}

/// **L'inverse de [`crate::upnp_server::track_audio_url`].**
///
/// Rend l'identifiant de piste quand `url` est l'adresse SOUS LAQUELLE CETTE
/// INSTANCE publie l'audio d'une de ses pistes, et `None` dans tous les autres
/// cas — autre hôte, autre chemin, identifiant illisible.
///
/// Le chemin n'est pas réécrit à la main : il est construit à partir de
/// [`crate::upnp_server::API_PATH`], la même constante que le constructeur.
/// Le témoin `l_aller_et_le_retour_parlent_de_la_meme_url` fait l'aller-retour
/// sur le constructeur lui-même, pour qu'un changement de route rougisse ici
/// plutôt que de rendre silencieusement « Episode ».
fn track_id_dans_une_url_audio_de_tune(url: &str) -> Option<i64> {
    let url = url.trim();
    let sans_schema = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))?;
    let (hote, chemin) = sans_schema.split_at(sans_schema.find('/')?);
    if !hote_de_cette_machine(hote) {
        return None;
    }
    let chemin = chemin.split(['?', '#']).next().unwrap_or(chemin);
    chemin
        .strip_prefix(crate::upnp_server::API_PATH)?
        .strip_prefix("/library/tracks/")?
        .strip_suffix("/audio")?
        .parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
}

/// Cet hôte d'URL est-il une adresse de CETTE machine ?
///
/// La question tient en un mot : l'identifiant de piste porté par l'URL
/// n'a de sens que dans la base d'un serveur donné. Un autre Tune du même
/// réseau publie ses `<res>` avec son adresse à lui, et son `187500` est une
/// autre piste. Répondre « non » ne coûte que le repli d'aujourd'hui ;
/// répondre « oui » à tort afficherait un titre faux.
///
/// La boucle locale et `localhost` passent (c'est l'accès depuis la machine
/// même), puis les adresses IPv4 réellement portées par les interfaces
/// (`local_ipv4_addresses`, qui exclut la boucle — d'où le test séparé).
/// Un NOM d'hôte ne passe pas : le résoudre demanderait un appel DNS sur le
/// chemin de lecture, et l'échec est sans dommage.
fn hote_de_cette_machine(hote: &str) -> bool {
    let sans_port = match hote.strip_prefix('[') {
        // IPv6 littéral : `[::1]:8888`
        Some(reste) => reste.split(']').next().unwrap_or(reste),
        None => hote.split(':').next().unwrap_or(hote),
    };
    if sans_port.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let Ok(ip) = sans_port.parse::<std::net::IpAddr>() else {
        return false;
    };
    if ip.is_loopback() {
        return true;
    }
    match ip {
        std::net::IpAddr::V4(v4) => crate::discovery::ssdp::local_ipv4_addresses().contains(&v4),
        std::net::IpAddr::V6(_) => false,
    }
}

#[cfg(test)]
mod temoins_de_l_uri_du_renderer {
    use super::{hote_de_cette_machine, track_id_dans_une_url_audio_de_tune};

    /// L'URI que Tades a vue passer (fil 1819), à l'adresse près : une piste
    /// de la bibliothèque, servie par Tune lui-même.
    #[test]
    fn une_url_de_piste_de_tune_rend_son_identifiant() {
        for url in [
            "http://127.0.0.1:8888/api/v1/library/tracks/187500/audio",
            "https://127.0.0.1:8888/api/v1/library/tracks/187500/audio",
            "http://localhost:8888/api/v1/library/tracks/187500/audio",
            "  http://127.0.0.1/api/v1/library/tracks/187500/audio  ",
            "http://[::1]:8888/api/v1/library/tracks/187500/audio",
            "http://127.0.0.1:8888/api/v1/library/tracks/187500/audio?x=1",
        ] {
            assert_eq!(
                track_id_dans_une_url_audio_de_tune(url),
                Some(187_500),
                "« {url} » désigne la piste 187500 de cette bibliothèque"
            );
        }
    }

    /// **La contre-épreuve.** Rien d'autre ne doit être pris pour une piste
    /// d'ici : un titre faux serait pire que le repli « Episode ».
    #[test]
    fn rien_d_autre_ne_passe() {
        for url in [
            // Un AUTRE serveur — Tune ou non. `198.51.100.7` est réservé à la
            // documentation (RFC 5737) : aucune interface ne le porte.
            "http://198.51.100.7:8888/api/v1/library/tracks/187500/audio",
            // Un nom d'hôte : non résolu, donc non reconnu.
            "http://tune.local:8888/api/v1/library/tracks/187500/audio",
            // Le serveur média d'un tiers.
            "http://127.0.0.1:26125/content/c2/b16/f44100/d61-coX.flac",
            // Nos autres routes : une radio n'est pas une piste.
            "http://127.0.0.1:8888/api/v1/radios/12/audio.wav",
            "http://127.0.0.1:8888/api/v1/library/tracks/187500/metadata",
            "http://127.0.0.1:8888/api/v1/library/tracks/187500",
            // Identifiants illisibles ou absurdes.
            "http://127.0.0.1:8888/api/v1/library/tracks/abc/audio",
            "http://127.0.0.1:8888/api/v1/library/tracks/0/audio",
            "http://127.0.0.1:8888/api/v1/library/tracks/-3/audio",
            "http://127.0.0.1:8888/api/v1/library/tracks//audio",
            // Pas une URL du tout — le condensat d'identité de la phase 2.
            "uuid:258FC2D5-E2C3-B734-0-123456789abc|85944171f73967e8",
            "",
        ] {
            assert_eq!(
                track_id_dans_une_url_audio_de_tune(url),
                None,
                "« {url} » ne désigne PAS une piste de cette bibliothèque"
            );
        }
    }

    /// Le chemin lu ici et le chemin publié dans la DIDL sont le MÊME contrat.
    /// L'aller-retour passe par le constructeur : si la route
    /// `/tracks/{id}/audio` ou `API_PATH` changeait, ce témoin rougirait — et
    /// non l'affichage, huit mois plus tard, chez un testeur.
    #[test]
    fn l_aller_et_le_retour_parlent_de_la_meme_url() {
        let url = crate::upnp_server::track_audio_url("http://127.0.0.1:8888", 187_500);
        assert_eq!(
            track_id_dans_une_url_audio_de_tune(&url),
            Some(187_500),
            "l'URL publiée dans le <res> doit se relire : {url}"
        );
    }

    /// La boucle locale est nous ; l'adresse de documentation ne l'est pas.
    #[test]
    fn l_hote_se_reconnait_ou_se_tait() {
        assert!(hote_de_cette_machine("127.0.0.1:8888"));
        assert!(hote_de_cette_machine("127.0.0.1"));
        assert!(hote_de_cette_machine("LocalHost:8888"));
        assert!(hote_de_cette_machine("[::1]:8888"));
        assert!(!hote_de_cette_machine("198.51.100.7:8888"));
        assert!(!hote_de_cette_machine("tune.local:8888"));
        assert!(!hote_de_cette_machine(""));
    }
}
