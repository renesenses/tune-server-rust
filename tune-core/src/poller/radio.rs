use super::*;

impl PositionPoller {
    /// Rafraichit le titre/interprete d'une zone qui joue une radio.
    ///
    /// Extrait de la boucle de sondage pour pouvoir servir DEUX appelants : la
    /// zone qui a un peripherique de sortie, et celle qui n'en a pas. La
    /// seconde n'etait servie par personne — voir l'appel dans `tick`.
    ///
    /// Le choix de la vignette et la recherche de la station vivent dans
    /// [`vignette_du_pas_radio`] et [`station_du_now_playing`] : les deux se
    /// prouvent hors reseau, ce que cette fonction-ci ne permet pas.
    pub(super) async fn refresh_radio_metadata(
        &self,
        zone_id: i64,
        zone_state: &crate::playback::ZoneState,
    ) {
        // Radio metadata polling (title/artist from ICY or external)
        if let Some(ref np) = zone_state.now_playing {
            if np.source == "radio" {
                if let Some(ref source_id) = np.source_id {
                    // source_id is either a numeric radio DB id or the stream URL itself
                    // Le logo de la station sert de REPLI quand le titre en
                    // cours n'a pas de pochette. Il faut le relire ici et non
                    // reprendre `np.cover_path` : dès qu'un titre a posé sa
                    // pochette, `cover_path` la porte, et le titre suivant —
                    // une chronique, un jingle — hériterait de la pochette du
                    // précédent au lieu de revenir au logo.
                    let radio_repo =
                        crate::db::radio_repo::RadioRepo::with_backend(self.db.clone());
                    let mut logo_station: Option<String> = None;
                    let (station_name, stream_url) =
                        if let Some(station) = station_du_now_playing(&radio_repo, source_id) {
                            logo_station = station.logo_url.clone();
                            (station.name.clone(), station.url.clone())
                        } else {
                            // Station introuvable en base : on retombe sur
                            // `album_title`, qui porte le nom de la station et
                            // survit aux mises a jour (`np.title`, lui, prend
                            // le titre du morceau des le premier
                            // rafraichissement).
                            let name = np.album_title.clone().unwrap_or_else(|| np.title.clone());
                            (name, source_id.clone())
                        };

                    if let Some(meta) =
                        crate::radio_metadata::fetch_radio_metadata(&station_name, &stream_url)
                            .await
                    {
                        // La pochette du titre quand la station la donne, le
                        // logo sinon. Bertrand : « mettre la pochette de
                        // l'album et non le logo de la radio ».
                        let pochette = vignette_du_pas_radio(
                            meta.cover_url.as_deref(),
                            logo_station.as_deref(),
                        );
                        let title_changed = np.title != meta.title
                            || np.artist_name != meta.artist
                            || np.cover_path != pochette;
                        if title_changed {
                            let new_np = crate::playback::NowPlaying {
                                track_id: None,
                                title: meta.title,
                                artist_name: meta.artist,
                                album_title: Some(station_name.clone()),
                                cover_path: pochette,
                                duration_ms: 0,
                                source: "radio".into(),
                                source_id: np.source_id.clone(),
                                stream_id: np.stream_id.clone(),
                                ..Default::default()
                            };
                            // Le renderer, lui, ne lit pas le now-playing : il
                            // reçoit des blocs ICY dans le flux. On publie donc
                            // titre ET pochette là où le gestionnaire de flux
                            // saura les relire, sinon l'appareil reste figé sur
                            // le morceau qui passait à sa connexion (#2161).
                            //
                            // On lit ces trois valeurs SUR `new_np`, et non sur
                            // des copies prises plus haut : ce sont exactement
                            // celles que l'interface Tune va recevoir. Trois
                            // variables `*_for_icy` parallèles pouvaient diverger
                            // du now-playing sans qu'aucune épreuve ne le voie —
                            // et c'est cette classe d'écart silencieux entre le
                            // producteur et le consommateur qui a produit ce
                            // ticket. Ici, l'écart n'est plus représentable.
                            //
                            // ── Et ce que cette garde TAISAIT (#2991) ──
                            //
                            // Sans `stream_id`, le `if let` ci-dessous ne
                            // publiait rien — en silence. L'interface Tune, elle,
                            // était mise à jour deux lignes plus bas par
                            // `update_now_playing`, qui ne dépend pas du
                            // `stream_id`. « Dans Tune ça fonctionne, sur le
                            // RS250A non » est le symptôme EXACT de cet écart, et
                            // rien au journal ne permettait de le distinguer d'un
                            // renderer qui n'aurait pas demandé l'ICY. Deux causes
                            // opposées, une seule absence de trace.
                            //
                            // On lit donc le canal AVANT de publier, et on
                            // journalise dans TOUS les cas — y compris celui qui
                            // marche, sans quoi « pas de ligne » resterait
                            // ambigu.
                            let canal = crate::http::streamer::canal_radio(np.stream_id.as_deref());
                            if let Some(sid) = np.stream_id.as_deref() {
                                crate::http::streamer::publish_radio_now(
                                    sid,
                                    new_np.artist_name.clone(),
                                    new_np.title.clone(),
                                    new_np.cover_path.clone(),
                                );
                            }
                            // UNE ligne doit suffire à savoir laquelle des
                            // branches mord la prochaine fois qu'un testeur
                            // signale un écran figé. Même nom d'évènement dans
                            // les deux cas — seul le niveau change — pour qu'un
                            // `grep radio_refresh_channel` les ramène ensemble.
                            let sid_journal = np.stream_id.as_deref().unwrap_or("absent");
                            if canal.atteint_le_renderer() {
                                debug!(
                                    zone_id,
                                    station = %station_name,
                                    stream_id = sid_journal,
                                    canal = canal.libelle(),
                                    "radio_refresh_channel"
                                );
                            } else {
                                warn!(
                                    zone_id,
                                    station = %station_name,
                                    stream_id = sid_journal,
                                    canal = canal.libelle(),
                                    "radio_refresh_channel — le morceau a changé mais l'écran du \
                                     lecteur réseau ne l'apprendra pas"
                                );
                            }
                            self.playback.update_now_playing(zone_id, new_np).await;
                            debug!(zone_id, station = %station_name, "radio_metadata_updated");
                        }
                    }
                }
            }
        }
    }

    /// Radio « artistes similaires » servie par le service de streaming.
    ///
    /// Le pendant streaming de `auto_dj::generate_similar_artists_queue`, qui
    /// ne sait produire que des pistes de la bibliothèque locale. Renvoie le
    /// nombre de pistes ajoutées — 0 si le service est absent, non authentifié,
    /// ou ne trouve rien : la radio se tait alors comme avant, sans casser la
    /// lecture.
    pub(super) async fn autoplay_streaming_radio(
        &self,
        zone_id: i64,
        seed_artist: &str,
        source: &str,
        seed_source_id: Option<&str>,
    ) -> usize {
        let Some(service) = self.orchestrator.services.lock().await.get(source) else {
            warn!(zone_id, source, "autoplay_streaming_service_absent");
            return 0;
        };

        // Source 1 : l'API d'enrichissement. Elle ne repond que par MBID, et
        // une piste de streaming n'en transporte aucun — en pratique elle rend
        // toujours zero candidat sur une ecoute Qobuz (#1553).
        let names = crate::playback::auto_dj::similar_artist_names(&self.db, seed_artist, 20).await;
        let from_enrichment = !names.is_empty();

        // Source 2 : le service lui-meme. Deux appels reseau, pas un de plus.
        // On garde les IDENTIFIANTS de catalogue, pas seulement les noms : ils
        // permettent ensuite de demander « des titres DE cet artiste » plutot
        // que « des titres qui contiennent son nom ».
        let mut service_artists: Vec<crate::streaming::traits::StreamArtist> = Vec::new();
        if names.is_empty() {
            info!(
                zone_id,
                seed_artist, source, "autoplay_streaming_enrichment_empty_trying_service"
            );
            service_artists = crate::playback::auto_dj::service_similar_artists(
                seed_artist,
                20,
                |query| {
                    let service = service.clone();
                    async move {
                        let svc = service.read().await;
                        match svc.search(&query, 10).await {
                            Ok(res) => res.artists,
                            Err(e) => {
                                warn!(artist = %query, error = %e, "autoplay_streaming_artist_search_failed");
                                Vec::new()
                            }
                        }
                    }
                },
                |artist_id| {
                    let service = service.clone();
                    async move {
                        let svc = service.read().await;
                        match svc.get_similar_artists(&artist_id, 20).await {
                            Ok(artists) => artists,
                            Err(e) => {
                                warn!(artist_id = %artist_id, error = %e, "autoplay_streaming_similar_failed");
                                Vec::new()
                            }
                        }
                    }
                },
            )
            .await;
        }

        if names.is_empty() && service_artists.is_empty() {
            // Les DEUX sources sont muettes : c'est ici que la file s'arrete,
            // et c'est la ligne que doit trouver quiconque diagnostique un
            // « autoplay qui ne fait rien ».
            warn!(
                zone_id,
                seed_artist, source, "autoplay_streaming_no_similar_names_from_any_source"
            );
            return 0;
        }
        let candidates = if from_enrichment {
            names.len()
        } else {
            service_artists.len()
        };
        info!(
            zone_id,
            source, seed_artist, candidates, from_enrichment, "autoplay_streaming_candidates"
        );

        // Ne jamais reproposer ce qu'on vient d'entendre, ni ce qui est deja
        // dans la file : une radio qui rejoue la piste qui se termine n'est pas
        // une radio.
        let mut exclude: std::collections::HashSet<String> = std::collections::HashSet::new();
        if let Some(id) = seed_source_id {
            exclude.insert(id.to_string());
        }
        if let Ok(rows) = crate::db::play_queue_repo::PlayQueueRepo::with_backend(self.db.clone())
            .get_ordered(zone_id)
        {
            exclude.extend(rows.into_iter().filter_map(|r| r.source_id));
        }

        // Deux facons de transformer un voisin en piste jouable :
        //  - via l'API d'enrichissement on n'a qu'un NOM, donc une recherche ;
        //  - via le service on a son identifiant de catalogue, donc ses titres
        //    a lui. La recherche par nom reste le repli quand l'artiste n'a pas
        //    de titres exposes.
        let names_by_id: std::collections::HashMap<String, String> = service_artists
            .iter()
            .map(|a| (a.id.clone(), a.name.clone()))
            .collect();
        let keys: Vec<String> = if from_enrichment {
            names.clone()
        } else {
            service_artists.iter().map(|a| a.id.clone()).collect()
        };

        let found =
            crate::playback::auto_dj::streaming_tracks_for_artist_names(&keys, 10, &exclude, |key| {
                let service = service.clone();
                let artist_name = names_by_id.get(&key).cloned();
                async move {
                    let svc = service.read().await;
                    // Chemin identifiant : les titres DE l'artiste, sans
                    // ambiguite de titre homonyme.
                    if let Some(ref name) = artist_name {
                        match svc.get_artist_top_tracks(&key).await {
                            Ok(tracks) if !tracks.is_empty() => return tracks,
                            Ok(_) => {}
                            Err(e) => {
                                warn!(artist_id = %key, error = %e, "autoplay_streaming_top_tracks_failed");
                            }
                        }
                        return match svc.search(name, 5).await {
                            Ok(res) => res.tracks,
                            Err(e) => {
                                warn!(artist = %name, error = %e, "autoplay_streaming_search_failed");
                                Vec::new()
                            }
                        };
                    }
                    match svc.search(&key, 5).await {
                        Ok(res) => res.tracks,
                        Err(e) => {
                            warn!(artist = %key, error = %e, "autoplay_streaming_search_failed");
                            Vec::new()
                        }
                    }
                }
            })
            .await;
        if found.is_empty() {
            warn!(
                zone_id,
                source, candidates, "autoplay_streaming_no_playable_track"
            );
            return 0;
        }
        let items: Vec<crate::db::play_queue_repo::StreamingQueueItem> = found
            .iter()
            .map(|t| {
                (
                    t.id.clone(),
                    t.title.clone(),
                    t.artist.clone(),
                    t.album.clone(),
                    t.cover_path.clone(),
                    t.duration_ms as i64,
                    Some(source.to_string()),
                    t.track_number.map(|n| n as i64),
                    t.disc_number.map(|n| n as i64),
                )
            })
            .collect();
        let queue_repo = crate::db::play_queue_repo::PlayQueueRepo::with_backend(self.db.clone());
        if let Err(e) = queue_repo.append_streaming_queue(zone_id, &items) {
            warn!(zone_id, error = %e, "autoplay_streaming_append_failed");
            return 0;
        }
        if let Some(ref bus) = self.event_bus {
            bus.emit(
                "playback.autoplay_tracks_added",
                serde_json::json!({
                    "zone_id": zone_id,
                    "source": source,
                    "seed_artist": seed_artist,
                    "count": items.len(),
                }),
            );
        }
        items.len()
    }
}
