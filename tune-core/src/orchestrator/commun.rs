use super::*;

impl PlaybackOrchestrator {
    /// Duplicate-network-play detector. Returns `true` when `(source,
    /// source_id)` was recorded as this zone's last network play within
    /// `DUPLICATE_NET_PLAY_WINDOW` of `now` (⇒ a redundant re-send to coalesce);
    /// otherwise records it as the new last play and returns `false`. Pure map
    /// logic split out of `play_inner` for unit testing.
    /// La cle doit identifier la PISTE, pas seulement sa source.
    ///
    /// `source_id` ne suffit pas : une piste de la bibliotheque locale se joue
    /// par `track_id`, et `play_from_queue` laisse alors `source` et
    /// `source_id` a `None`. La cle valait donc `("local", None)` pour TOUTES
    /// les pistes locales d'une zone, si bien que deux morceaux DIFFERENTS
    /// envoyes au meme renderer reseau a moins de douze secondes d'intervalle
    /// se ressemblaient parfaitement.
    ///
    /// Consequence pour l'utilisateur : sur Chromecast, DLNA ou AirPlay, appuyer
    /// sur « piste suivante » pendant les douze premieres secondes faisait
    /// avancer le serveur SANS rien envoyer au renderer, qui continuait le
    /// morceau precedent. Le bouton paraissait mort (FabienM, v0.9.102, zone
    /// Enfants en Chromecast : quinze `api_next_requested` d'affilee, tous
    /// suivis d'un `orchestrator_play_coalesced_duplicate_net_send` sur des
    /// titres pourtant differents).
    ///
    /// Le test d'origine n'exercait que `tidal` et `qobuz` — des sources qui
    /// portent TOUJOURS un `source_id`. Il ne pouvait pas voir le cas local.
    pub(super) fn record_or_detect_duplicate_net_play(
        map: &mut HashMap<i64, (String, Option<String>, Option<i64>, std::time::Instant)>,
        zone_id: i64,
        source: &str,
        source_id: &Option<String>,
        track_id: Option<i64>,
        now: std::time::Instant,
    ) -> bool {
        let dup = map.get(&zone_id).is_some_and(|(src, sid, tid, when)| {
            src == source
                && sid == source_id
                && *tid == track_id
                && now.duration_since(*when) < DUPLICATE_NET_PLAY_WINDOW
        });
        if !dup {
            map.insert(
                zone_id,
                (source.to_string(), source_id.clone(), track_id, now),
            );
        }
        dup
    }

    pub(super) fn server_ip(&self) -> String {
        self.advertised_ip.clone().unwrap_or_else(|| {
            crate::discovery::ssdp::get_local_ip()
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "127.0.0.1".into())
        })
    }

    /// Identity match for the re-tap dedup: is `req` targeting the SAME track the
    /// zone's current `now_playing` (`np`) represents? Prefers the library
    /// `track_id` when both sides carry one; otherwise matches a non-empty
    /// streaming `(source, source_id)` — and if `req` names a `source` it must
    /// agree with the now-playing source. Returns `false` when neither side
    /// yields a positive identifier, so two unidentifiable plays never collide
    /// (a false negative merely lets the normal play path run). Pure so it can be
    /// unit-tested without a live orchestrator.
    pub(super) fn is_same_track_retap(np: &NowPlaying, req: &PlayRequest) -> bool {
        if let (Some(a), Some(b)) = (np.track_id, req.track_id) {
            return a == b;
        }
        match (&np.source_id, &req.source_id) {
            (Some(a), Some(b)) if !a.is_empty() && a == b => {
                req.source.as_deref().is_none_or(|s| s == np.source)
            }
            _ => false,
        }
    }

    /// Pick a live output to re-bind a zone to, when its stored
    /// `output_device_id` has vanished from the registry.
    ///
    /// Matches on the zone's display name (case-insensitive) and **prefers a
    /// `local:` output**: the case this exists for is a zone created long ago
    /// against a *network* view of a device that is now only reachable locally
    /// (Alex Campbell's "Mac Studio Speakers", once seen over the network by a
    /// second server on a Raspberry Pi, today a plain CoreAudio output).
    ///
    /// Returns `None` when there is no match **or** when the match is ambiguous
    /// — several same-name outputs with no single local one. Binding "at
    /// random" would send audio to the wrong device, which is worse than the
    /// clear error the caller falls back to.
    pub(super) async fn find_rebind_target(&self, zone_name: &str) -> Option<(String, String)> {
        let candidates = { self.outputs.lock().await.find_by_name(zone_name) };
        if candidates.is_empty() {
            return None;
        }
        let mut locals: Vec<&(String, String)> = candidates
            .iter()
            .filter(|(id, _)| id.starts_with("local:"))
            .collect();
        if locals.len() == 1 {
            return Some(locals.remove(0).clone());
        }
        if locals.is_empty() && candidates.len() == 1 {
            return Some(candidates[0].clone());
        }
        warn!(
            zone_name,
            candidates = candidates.len(),
            locals = locals.len(),
            "zone_rebind_ambiguous_not_rebinding"
        );
        None
    }

    /// Le message d'échec de lecture, tel que l'utilisateur le lit.
    ///
    /// Un message d'échec doit permettre d'AGIR. Celui du DLNA — « Le renderer
    /// a acquitté Play mais joue toujours une autre source » — décrit
    /// fidèlement ce que l'appareil a fait, et c'est justement le problème :
    /// il désigne le matériel. L'utilisateur cherche du côté du renderer, du
    /// réseau, de son installation ; l'un d'eux a réinstallé son système
    /// entier (#2396).
    ///
    /// Or dans un cas précis le serveur SAIT, avant même d'envoyer, pourquoi
    /// cela ne marchera probablement pas : la zone est en DSD « natif », le
    /// Sink du lecteur a répondu qu'il ne lit pas le DSD, et on lui envoie le
    /// flux brut quand même. Ce choix-là n'est pas remis en cause — « natif »
    /// est un réglage explicite et des renderers lisent le DSD sans l'annoncer
    /// (Eversolo DMP-A8), cf. [`Self::decider_passthrough_dsd`]. Ce qui était
    /// faux, c'est le message : il accusait l'appareil au lieu de nommer le
    /// réglage et l'action qui le corrige.
    ///
    /// Hors de ce cas — le lecteur a dit oui, le sondage est resté muet, la
    /// zone est en `auto`, la source n'est pas du DSD brut — le message ne
    /// change pas d'un caractère. Accuser un réglage à tort serait la faute
    /// symétrique de celle qu'on corrige.
    ///
    /// Le préfixe « Output device error » est conservé dans tous les cas : la
    /// route de lecture s'en sert pour rendre un 503 « appareil indisponible »
    /// plutôt qu'un 500 (`tune-server/src/routes/playback.rs`), et
    /// [`command_may_have_landed`] cherche le marqueur de timeout SOAP à
    /// l'intérieur.
    pub(crate) fn message_echec_sortie(
        erreur: &str,
        dsd_mode: &str,
        annonce: Option<bool>,
        mime_type: &str,
    ) -> String {
        if dsd_mode == "native" && annonce == Some(false) && est_dsd_brut(mime_type) {
            return format!(
                "Output device error: le mode DSD de cette zone est réglé sur « natif » \
                 et ce lecteur annonce ne pas lire le DSD — le flux DSD brut lui a été \
                 envoyé quand même, et il ne l'a pas appliqué. Passer le mode DSD de la \
                 zone en « DoP » ou « PCM » pour lire ce fichier. \
                 (réponse du lecteur : {erreur})"
            );
        }
        format!("Output device error: {erreur}")
    }

    pub(super) async fn resolve_stream(&self, req: &PlayRequest) -> Result<ResolvedStream, String> {
        if let Some(ref source) = req.source
            && source != "local"
        {
            // An out-of-library file dragged into the queue is stored as
            // source="upload" with source_id = the uploaded temp file path (see
            // queue_add). Every advance/jump/repeat funnels through resolve_stream,
            // so resolve it here — not only via the one-shot temp_file_path field —
            // otherwise it plays once but fails on queue advance (Sergio:
            // glisser-lire un fichier hors bibliothèque).
            if source == "upload" {
                let path = req
                    .source_id
                    .as_deref()
                    .ok_or("upload source requires source_id (file path)")?;
                return self.resolve_uploaded_file(path, req).await;
            }
            // `bandcamp` entre par la MÊME porte qu'un podcast ou une radio :
            // une URL distante déjà jouable, sans service enregistré derrière.
            // Sans cette ligne il tombait dans `resolve_streaming_url`, qui
            // cherche un service nommé « bandcamp » dans le registre et
            // échoue — c'est pour ça que la vue jouait dans l'onglet plutôt
            // que dans la zone (#1768).
            if source == "podcast" || source == "radio" || source == "upnp" || source == "bandcamp"
            {
                return self.resolve_direct_url(req).await;
            }
            return self.resolve_streaming_url(source, req).await;
        }

        self.resolve_local_track(req).await
    }

    /// Dereference an M3U/PLS *playlist* URL to its first real http(s) stream.
    ///
    /// Many stations are published as a small `.m3u`/`.pls` file whose body is
    /// just the actual stream URL(s) — e.g. `radioswissjazz.ch/live/mp3.m3u`
    /// contains a single line pointing at the Icecast stream. Playing the
    /// playlist URL directly feeds the playlist *text* to the audio decoder, so
    /// the level meter twitches on garbage but no sound comes out (Pascal,
    /// v0.9.21).
    ///
    /// Returns `Some(stream_url)` only when `url` is a playlist that
    /// dereferenced to a different http(s) URL; `None` for a direct media URL
    /// (no network hit — cheap extension gate first), for an HLS `.m3u8`
    /// manifest, or on any fetch/parse failure — so the caller keeps the
    /// original URL.
    ///
    /// `.m3u8` est écarté du déréférencement, et ce n'est PAS parce que « le
    /// manifeste EST le flux, consommé directement par le lecteur » — ce que
    /// cette phrase affirmait jusqu'à #2307, en annonçant une capacité qui
    /// n'a jamais existé. Tune n'a aucun chargeur de segments HLS : le seul
    /// lecteur de flux radio est `decode_radio_stream_to_pcm`, un GET unique
    /// poussé dans symphonia. Et déréférencer ne servirait à rien non plus :
    /// une playlist HLS ne porte que des chemins de segments RELATIFS, donc
    /// le filtre `http(s)` ci-dessous n'y trouverait rien à rendre (constat
    /// déjà écrit noir sur blanc dans la migration 86, qui a retiré BBC
    /// Radio 3 pour cette raison). La garde reste donc telle quelle ; c'est
    /// la LECTURE qui refuse HLS, en le NOMMANT — voir
    /// [`RADIO_HLS_UNSUPPORTED`].
    pub(super) async fn resolve_playlist_url(&self, url: &str) -> Option<String> {
        let path = url
            .split(['?', '#'])
            .next()
            .unwrap_or(url)
            .to_ascii_lowercase();
        if !(path.ends_with(".m3u") || path.ends_with(".pls")) {
            return None;
        }
        let body = crate::http::client::shared()
            .get(url)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .ok()?
            .bytes()
            .await
            .ok()?;
        let inner = crate::library::m3u_parser::parse_m3u_content(&body, true)
            .into_iter()
            .map(|e| e.path)
            .find(|p| {
                let p = p.trim();
                p.starts_with("http://") || p.starts_with("https://")
            })?;
        let inner = inner.trim().to_string();
        if inner == url.trim() {
            return None; // playlist pointed back at itself — nothing gained
        }
        info!(playlist = %url, stream = %inner, "radio_playlist_dereferenced");
        Some(inner)
    }

    /// Convert a cover_path (which may be a short hash or a full URL) into an
    /// absolute HTTP URL accessible by network renderers (DLNA/OpenHome).
    /// Hash-only values like `"abc123def"` become `http://IP:PORT/api/v1/artwork/abc123def`.
    /// Full URLs (starting with `http://` or `https://`) are passed through unchanged.
    pub(super) fn resolve_cover_url(&self, cover: Option<&str>) -> Option<String> {
        let c = cover?;
        if c.starts_with("http://") || c.starts_with("https://") {
            return Some(c.to_string());
        }
        // It's a local artwork hash — build an absolute URL
        let server_ip = self.server_ip();
        // Use the streamer port (same as API server port)
        let port = std::env::var("TUNE_PORT")
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(8888);
        Some(format!(
            "http://{server_ip}:{port}/api/v1/library/artwork/{c}"
        ))
    }

    /// Les deux réglages qu'une sortie locale DOIT porter, lus là où la page
    /// de réglages les écrit : la base.
    ///
    /// #1770 (point 3) — la sortie reconstruite à la volée par
    /// [`Self::recreate_local_and_play`] les codait en dur (`false`,
    /// `"auto"`). Conséquence, sur Windows et macOS — les seules plateformes
    /// qui ont un chemin exclusif ([`crate::outputs::local`],
    /// `exclusive_mode_support`) : un DAC éteint au démarrage, ou retiré par
    /// le balayage à chaud, repartait au PREMIER appui sur Lecture en mode
    /// partagé et jamais en ASIO — `select_host("auto")` ne sonde même pas
    /// ASIO — alors que les réglages disaient le contraire, et sans que
    /// l'écran le trahisse (`display_audio_backend` rend la valeur VOULUE).
    ///
    /// La chaîne est celle de `AppState::effective_audio_backend` /
    /// `effective_exclusive_mode`, MOINS le fichier de configuration, qui
    /// n'existe que dans `tune-server` et n'est pas atteignable d'ici : la
    /// base d'abord, l'environnement en repli, puis les valeurs par défaut de
    /// [`crate::config::TuneConfig`] — c'est-à-dire exactement ce que le
    /// codage en dur rendait, et jamais moins.
    pub fn reglages_sortie_locale(&self) -> (bool, String) {
        self.reglages_sortie_locale_avec(|cle| std::env::var(cle).ok())
    }

    /// Même résolution, avec l'environnement en PARAMÈTRE.
    ///
    /// Même intention que [`crate::config::resolve_local_audio_backend`] : la
    /// règle doit être éprouvable sans dépendre des variables de la machine
    /// qui l'éprouve — sinon l'essai serait vert ou rouge selon le `.env` du
    /// runner, et `std::env::set_var` dans un test casse la suite entière.
    pub fn reglages_sortie_locale_avec<F>(&self, environnement: F) -> (bool, String)
    where
        F: Fn(&str) -> Option<String>,
    {
        let reglages = crate::db::settings_repo::SettingsRepo::with_backend(self.db.clone());
        let backend = reglages
            .get("local_audio_backend")
            .ok()
            .flatten()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| crate::config::resolve_local_audio_backend(&environnement))
            .unwrap_or_else(|| "auto".to_string());
        let demande = reglages
            .get("local_exclusive_mode")
            .ok()
            .flatten()
            .map(|v| matches!(v.trim().to_lowercase().as_str(), "true" | "1" | "yes"))
            .unwrap_or_else(|| {
                environnement("TUNE_LOCAL_EXCLUSIVE_MODE")
                    .map(|v| matches!(v.trim().to_lowercase().as_str(), "true" | "1" | "yes"))
                    .unwrap_or(false)
            });
        // ASIO est exclusif par nature, et c'est une notion Windows (#1268,
        // #3192). La règle vit dans `tune-core::config` et n'est pas recopiée
        // ici : une seule écriture pour les deux chemins.
        let effectif = crate::config::local_exclusive_mode_status(&backend, demande).effective;
        (effectif, backend)
    }

    /// La position de lecture de cette zone est-elle ENTRETENUE ?
    ///
    /// #2595 — Pierre M, zone 987 : basculer en mode Audiophile pendant
    /// l'écoute fait repartir le morceau **du début**.
    ///
    /// [`Self::schedule_eq_replay`] relit `position_ms` pour rejouer « là où on
    /// en était ». Or cette valeur n'est entretenue que par l'unique
    /// `update_position` de production du sondeur — et la boucle de transport
    /// de `poller.rs` s'ouvre sur `get_zone_device_id`, dont la branche `None`
    /// fait `continue` **avant** cet appel. Une zone sans périphérique de
    /// sortie — une zone navigateur, « Cet ordinateur », par conception : le
    /// client web tire `stream_url` lui-même — n'est donc jamais observée. Son
    /// `position_ms` reste figé sur ce que la dernière COMMANDE y a écrit,
    /// c'est-à-dire 0 depuis `play()`, pendant que le morceau avance dans
    /// l'onglet.
    ///
    /// Zéro n'est alors pas une position : c'est une absence de mesure.
    /// Rejouer dessus n'est pas « reprendre », c'est recommencer.
    ///
    /// Le prédicat est **celui du sondeur, à l'identique** et à dessein, et il
    /// est STRUCTUREL — pas un seuil, pas une heuristique sur la valeur : la
    /// question posée est « quelqu'un observe-t-il cette zone ? ».
    /// Périphérique présent ⇒ le sondeur passe ⇒ la position est mesurée à la
    /// seconde. Pas de périphérique ⇒ personne ne la mesure, et le serveur ne
    /// sait pas où en est la lecture. Rien ici ne confond « zéro » et « je ne
    /// sais pas » : les deux cas ne se lisent pas dans la même valeur.
    ///
    /// Le seul autre gisement possible serait le client lui-même — c'est
    /// l'onglet qui joue, donc lui seul connaît sa position. Aucune route ne la
    /// remonte aujourd'hui (`SeekRequest` est une COMMANDE, pas un rapport), et
    /// l'inventer côté serveur à partir de `streamer_bytes_sent` mesurerait le
    /// TÉLÉCHARGEMENT, pas l'écoute — un lecteur qui tamponne en avance donnerait
    /// une position en avance. Tant que le client ne la déclare pas, la réponse
    /// honnête est « je ne sais pas ».
    pub(super) fn position_entretenue_par_le_sondeur(&self, zone_id: i64) -> bool {
        ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id)
            .is_some()
    }

    /// Faire prendre effet une bascule du mode PURE, PAR TOUS LES CHEMINS.
    ///
    /// Jumeau de [`Self::apply_eq_change`], et pour la même raison : la règle
    /// « local d'abord, redémarrage sinon » ne doit vivre qu'à un endroit.
    ///
    /// - **sortie locale** : [`Self::refresh_zone_pure_dsp`] repousse l'état
    ///   derrière les mutex de la sortie — immédiat, sans coupure ;
    /// - **tout le reste** (DLNA, navigateur) : les traitements ont été gravés
    ///   dans le fichier transcodé, déjà écrit et déjà téléchargé. Rien à
    ///   remplacer ; seul un redémarrage du flux le re-rend, et c'est
    ///   exactement ce que [`Self::schedule_eq_replay`] sait faire — même
    ///   anti-rebond, même plancher, parce que c'est le même coût (environ une
    ///   seconde de silence) et le même geste répétable.
    ///
    /// Rend `true` quand la bascule a atteint le son **immédiatement**. Un
    /// redémarrage programmé rend `false` : il n'a pas encore eu lieu.
    pub async fn apply_audiophile_change(self: &std::sync::Arc<Self>, zone_id: i64) -> bool {
        if self.refresh_zone_pure_dsp(zone_id).await {
            return true;
        }
        // Pas de chemin local vivant. Le redémarrage n'a de sens que si quelque
        // chose joue : sinon la prochaine lecture appliquera l'état toute seule.
        let joue = self.playback.get_state(zone_id).await.now_playing.is_some();
        if joue {
            self.schedule_eq_replay(zone_id);
        }
        false
    }

    pub(super) fn record_listen(
        &self,
        title: &str,
        artist: Option<&str>,
        album: Option<&str>,
        source: &str,
        source_id: Option<&str>,
        album_id: Option<i64>,
        duration_ms: i64,
        zone_id: i64,
        cover_url: Option<&str>,
        session_profile_id: Option<i64>,
        contexte: ContexteEcoute<'_>,
    ) {
        // The owning profile is resolved by the caller from the zone's session
        // (set by the play handler from X-Profile-Id, inherited by autoplay /
        // gapless advances). `None` → tag NULL rather than guess an owner: a
        // wrong attribution pollutes a person's taste profile once per-profile
        // recommendations land, an absence doesn't.
        let repo = HistoryRepo::with_backend(self.db.clone());
        repo.record(&ListenRecord {
            id: None,
            track_id: None,
            title: title.into(),
            artist_name: artist.map(Into::into),
            album_title: album.map(Into::into),
            source: source.into(),
            source_id: source_id.map(Into::into),
            album_id,
            duration_ms,
            listened_at: None,
            zone_id: Some(zone_id),
            cover_url: cover_url.map(Into::into),
            profile_id: session_profile_id,
            // Ce que l'auditeur a demande. Ecrit tel qu'il l'a dit : rien
            // n'est deduit ici de ce qui a fini par jouer. Le ticket #2441
            // etablit que cette information n'etait ecrite NULLE PART — la
            // section « Continuer l'ecoute » ne pouvait donc que repartir de
            // la table `albums`.
            context_type: contexte.nature.map(Into::into),
            context_id: contexte.id.map(Into::into),
            context_position: contexte.rang,
        })
        .ok();

        // NOTE: scrobbling is intentionally NOT dispatched here. It used to fire
        // at play-start, which (a) scrobbled a track the instant it began — so
        // skipping after a few seconds still scrobbled it, ignoring Last.fm's
        // 50%/4-min rule — and (b) was gated by `record_history`, which the
        // gapless/prefetch advance paths bypass (`play_without_history`), so
        // every other track on an album was silently dropped (Bilou, #1113). The
        // poller now dispatches the scrobble once the track has actually been
        // listened past the threshold (see `dispatch_scrobble`).
    }

    /// L'onglet a-t-il commencé à tirer le flux de cette zone ? Si oui, et
    /// seulement alors, l'annonce « en écoute » mise en attente au démarrage
    /// part — une fois (#1998).
    ///
    /// Appelée à chaque tick par le poller pour une zone SANS périphérique de
    /// sortie. C'est le poller qui a l'horloge ; la décision, elle, reste ici,
    /// avec les données du démarrage — `record_history` en particulier, qu'un
    /// observateur extérieur ne peut pas reconstituer.
    ///
    /// La preuve est celle dont `output_reach` se sert déjà pour dire
    /// « browser_unattended » (`tune-server/src/routes/zones.rs`) : des octets
    /// réellement partis sur la session de flux. Aucune détection nouvelle.
    ///
    /// Le délai vaut au plus un tick de poller (~1 s) après le premier octet
    /// tiré : la règle de durée minimale de Last.fm porte sur le scrobble
    /// définitif (50 % / 4 min, côté poller), pas sur « en écoute », et une
    /// seconde ne coûte aucune écoute légitime.
    ///
    /// Rend `true` quand l'annonce vient de partir.
    pub async fn confirmer_lecture_navigateur(&self, zone_id: i64, stream_id: &str) -> bool {
        // Rien en attente pour CE flux → rien à faire, et surtout pas
        // d'interrogation du streamer à chaque tick de chaque zone.
        {
            let Ok(en_attente) = self.annonces_navigateur.lock() else {
                return false;
            };
            if en_attente.get(&zone_id).map(|a| a.stream_id.as_str()) != Some(stream_id) {
                return false;
            }
        }

        let tire = self
            .streamer
            .stream_bytes_sent(stream_id)
            .await
            .is_some_and(|n| n > 0);
        if !tire {
            return false;
        }

        // Retirer AVANT d'annoncer : le verrou « une seule fois » est le retrait
        // lui-même. Re-vérifier le flux protège d'une lecture qui aurait
        // remplacé l'entrée pendant l'attente ci-dessus.
        let attente = {
            let Ok(mut en_attente) = self.annonces_navigateur.lock() else {
                return false;
            };
            match en_attente.get(&zone_id) {
                Some(a) if a.stream_id == stream_id => en_attente.remove(&zone_id),
                _ => None,
            }
        };
        let Some(attente) = attente else {
            return false;
        };

        info!(
            zone_id,
            title = %attente.title,
            source = %attente.source,
            stream_id = %stream_id,
            "browser_playback_confirmed_announcing"
        );

        self.dispatch_now_playing(
            &attente.title,
            attente.artist.as_deref(),
            attente.album.as_deref(),
        );

        // Même exclusion que le chemin nominal : la radio n'entre pas dans
        // l'historique local (son titre au démarrage est un instantané figé),
        // et une re-création de flux pour une piste déjà en cours
        // (`play_without_history`) ne doit pas doublonner la ligne.
        if attente.record_history && attente.source != "radio" {
            let etat = self.playback.get_state(zone_id).await;
            let album_id = attente.track_id.and_then(|tid| {
                TrackRepo::with_backend(self.db.clone())
                    .get(tid)
                    .ok()
                    .flatten()
                    .and_then(|t| t.album_id)
            });
            self.record_listen(
                &attente.title,
                attente.artist.as_deref(),
                attente.album.as_deref(),
                &attente.source,
                attente.source_id.as_deref(),
                album_id,
                attente.duration_ms,
                zone_id,
                attente.cover_path.as_deref(),
                etat.session_profile_id,
                ContexteEcoute {
                    nature: etat.session_context_type.as_deref(),
                    id: etat.session_context_id.as_deref(),
                    rang: rang_a_retenir(etat.shuffle, etat.queue_position),
                },
            );
        }

        true
    }

    /// Quelles sorties le repli de `stop` a le droit de toucher.
    ///
    /// Tout ce qui est revendiqué par une autre zone est épargné, sans
    /// exception : c'est l'invariant que ce repli avait perdu.
    pub(super) fn sorties_a_arreter_en_repli(
        toutes: &[String],
        revendiquees_ailleurs: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        toutes
            .iter()
            .filter(|did| !revendiquees_ailleurs.contains(did.as_str()))
            .cloned()
            .collect()
    }

    pub async fn set_mute(
        &self,
        zone_id: i64,
        muted: bool,
        device_id: Option<&str>,
    ) -> OutputCommandResult<()> {
        if let Some(did) = device_id {
            let output = { self.outputs.lock().await.get(did) }.ok_or_else(|| {
                OutputCommandError::failed(
                    OutputCommand::SetMute,
                    format!("output {did} is not registered"),
                )
            })?;
            output.lock().await.checked_set_mute(muted).await?;
        }
        self.playback.set_mute(zone_id, muted).await;
        ZoneRepo::with_backend(self.db.clone())
            .update_muted(zone_id, muted)
            .map_err(|message| OutputCommandError::failed(OutputCommand::SetMute, message))?;
        Ok(())
    }

    pub async fn wait_stream_data_ready(&self, stream_id: &str, timeout_ms: u64) -> bool {
        self.streamer.wait_data_ready(stream_id, timeout_ms).await
    }

    pub async fn streamer_bytes_sent(&self, stream_id: &str) -> Option<u64> {
        self.streamer.stream_bytes_sent(stream_id).await
    }

    /// Consigne le constat « aucun onglet ne reçoit le son de cette zone »
    /// (voir [`crate::playback::PlaybackManager::note_browser_unattended`]).
    pub async fn note_browser_unattended(&self, zone_id: i64, unattended: bool) {
        self.playback
            .note_browser_unattended(zone_id, unattended)
            .await;
    }

    /// Taille totale du flux (voir [`AudioStreamer::stream_total_bytes`]).
    pub async fn streamer_total_bytes(&self, stream_id: &str) -> Option<u64> {
        self.streamer.stream_total_bytes(stream_id).await
    }

    pub(super) async fn persist_position(&self, zone_id: i64) {
        let state = self.playback.get_state(zone_id).await;
        if let Some(ref np) = state.now_playing {
            ZoneRepo::with_backend(self.db.clone())
                .save_playback_position(
                    zone_id,
                    state.position_ms,
                    np.track_id,
                    Some(np.source.as_str()),
                    np.source_id.as_deref(),
                )
                .ok();
        }
    }
}
