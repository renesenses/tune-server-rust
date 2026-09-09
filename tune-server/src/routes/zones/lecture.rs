use super::*;

/// #2369 — ce que la zone OBTIENDRA d'une source DSD, à côté de ce qu'elle a
/// DEMANDÉ (`dsd_mode`).
///
/// Le sélecteur propose « natif » et « dop ». Sur une sortie locale, les deux
/// emballent le DSD en DoP : aucun chemin natif n'existe dans
/// `tune-core/src/outputs/local.rs`. L'écran affichait donc « natif » pendant
/// que du DoP partait, et le testeur qui bascule d'un mode à l'autre refait
/// deux fois le même essai. `dsd_mode` reste ce qui est réglé ; ce champ dit
/// ce qui part.
///
/// Une SEULE fonction pour les deux sites jumeaux (`list_zones` et
/// `get_zone`) : c'est une copie qui dérive qui avait fait répondre deux
/// choses différentes à la même question sur la même zone (#2189).
///
/// Les deux entrées sont celles-là mêmes dont se sert `resolve_local_track` —
/// le préfixe `local:` de `output_device_id`, et `is_network_output_type` sur
/// `output_type` — pour que le panneau ne puisse pas contredire le chemin
/// audio.
fn dsd_transport_value(zone: &Zone, dsd_mode: &str) -> Value {
    let is_local = zone
        .output_device_id
        .as_deref()
        .is_some_and(|id| id.starts_with("local:"));
    let is_network = tune_core::orchestrator::is_network_output_type(zone.output_type.as_deref());
    json!(tune_core::orchestrator::transport_dsd(is_local, is_network, dsd_mode).as_str())
}

pub(super) async fn sync_status(State(state): State<AppState>) -> Json<Value> {
    let zone_repo = ZoneRepo::with_backend(state.backend.clone());
    let zones = zone_repo.list().unwrap_or_default();
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let metrics = state.poller_metrics.lock().await;

    let mut zone_data = Vec::new();
    for z in &zones {
        let zone_id = z.id.unwrap_or(0);
        let ps = state.playback.get_state(zone_id).await;
        let poller = metrics.get(&zone_id).cloned().unwrap_or_default();
        let group_id = z.group_id.as_deref();
        zone_data.push(json!({
            "zone_id": zone_id,
            "name": z.name,
            "output_type": z.output_type,
            "state": match ps.state {
                tune_core::playback::PlayState::Playing => "playing",
                tune_core::playback::PlayState::Paused => "paused",
                tune_core::playback::PlayState::Stopped => "stopped",
            },
            "position_ms": ps.position_ms,
            "duration_ms": ps.now_playing.as_ref().map(|np| np.duration_ms).unwrap_or(0),
            "now_playing": ps.now_playing.as_ref().map(|np| json!({
                "title": np.title,
                "artist": np.artist_name,
                "album": np.album_title,
            })),
            "group_id": group_id,
            "poller": poller,
        }));
    }

    Json(json!({
        "zones": zone_data,
        "groups": groups,
        "total_zones": zones.len(),
        "playing_count": zone_data.iter().filter(|z| z["state"] == "playing").count(),
    }))
}

/// Durée d'observation minimale avant d'oser annoncer un débit.
///
/// Les premiers blocs d'une session partent en rafale — remplissage du tampon,
/// en-tête WAV, réponse au `Range` initial. Rapportés aux quelques dizaines de
/// millisecondes qui viennent de s'écouler, ils donnent un débit à cinq
/// chiffres qui ne décrit rien. Une seconde suffit à lisser l'amorçage.
pub(super) const FENETRE_MINIMALE: std::time::Duration = std::time::Duration::from_secs(1);

/// Les deux faits d'une mesure de débit, lus ENSEMBLE sur la MÊME session.
///
/// C'est le point du correctif : le compteur d'octets et la durée pendant
/// laquelle ils sont partis doivent décrire le même objet. Les lire à deux
/// endroits différents est exactement ce qui avait permis de diviser les
/// octets d'une session par l'ancienneté du SERVEUR.
///
/// Ce que compte `bytes_sent` : TOUT ce que le serveur a émis pour cette
/// session, tous chemins de sortie confondus (fichier, radio, mandataire —
/// voir `corps_compte` dans `tune-core/src/http/streamer.rs`). Sortie locale,
/// renderer DLNA et — depuis #2738 — le relais du pont y sont additionnés. Ce
/// n'est donc pas « ce que reçoit un navigateur », c'est ce que la zone a fait
/// sortir.
pub(super) fn mesure_de_session(
    session: &tune_core::http::streamer::StreamSession,
) -> (u64, std::time::Duration) {
    (
        session
            .bytes_sent
            .load(std::sync::atomic::Ordering::Relaxed),
        session.created_at.elapsed(),
    )
}

/// Le débit MOYEN observé sur la vie du flux, en kbit/s — ou `None`.
///
/// `None` n'est pas `0.0`. Les deux se lisent pareil à l'écran et ne disent
/// pas la même chose : `0.0` affirme que rien ne circule, `None` dit qu'on n'a
/// pas de quoi mesurer. Le champ rendait `0.0` dans les deux cas, si bien
/// qu'un flux qui démarre était annoncé muet.
///
/// Le calcul reste en flottant de bout en bout. `octets * 8 / 1000` était une
/// division ENTIÈRE, faite avant celle par le temps : les décimales étaient
/// jetées là, et l'arrondi final à la décimale près ne rattrapait qu'un
/// chiffre déjà faux.
///
/// C'est une MOYENNE sur la session, pas un débit instantané : une pause en
/// cours de piste continue de creuser la fenêtre et tire la valeur vers le
/// bas. Ce qui est garanti, c'est que la fenêtre appartient au flux mesuré.
pub(super) fn debit_observe_kbps(octets_envoyes: u64, fenetre: std::time::Duration) -> Option<f64> {
    if octets_envoyes == 0 || fenetre < FENETRE_MINIMALE {
        return None;
    }
    let kbps = octets_envoyes as f64 * 8.0 / 1000.0 / fenetre.as_secs_f64();
    Some((kbps * 10.0).round() / 10.0)
}

pub(super) async fn network_health(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Json<Value> {
    let metrics = state.poller_metrics.lock().await;
    let poller = metrics.get(&id).cloned().unwrap_or_default();
    let ps = state.playback.get_state(id).await;

    let mesure: Option<(u64, std::time::Duration)> = if let Some(ref np) = ps.now_playing
        && let Some(ref sid) = np.stream_id
    {
        let sessions = state.streamer.sessions_state();
        let sessions = sessions.lock().await;
        sessions.get(sid.as_str()).map(|s| mesure_de_session(s))
    } else {
        None
    };

    let stream_bytes = mesure.map_or(0, |(octets, _)| octets);
    let bitrate_kbps = mesure.and_then(|(octets, fenetre)| debit_observe_kbps(octets, fenetre));

    Json(corps_network_health(
        id,
        &poller,
        stream_bytes,
        bitrate_kbps,
    ))
}

/// Le corps JSON de `GET /api/v1/zones/{id}/network-health`, sorti du
/// gestionnaire pour qu'il soit ÉPROUVABLE — comme [`debit_observe_kbps`]
/// juste au-dessus.
///
/// ## #3318 — pourquoi cette route mentait sur la famine d'anneau
///
/// `ZonePollerMetrics` porte depuis `22436699` deux mesures de ce que le DAC
/// n'a PAS reçu : `famine_anneau_evenements` et `famine_anneau_silence_ms`.
/// Le sondeur les y recopie à chaque tick (`tune-core/src/poller/tick.rs`,
/// aux DEUX sites qui insèrent dans `shared_metrics` — le bras radio et le
/// bras « zone en lecture »).
///
/// `GET /zones/sync-status` les rend sans rien faire : il sérialise la
/// structure entière (`"poller": poller`). **Cette route-ci ne le pouvait
/// pas** : elle bâtit son objet champ par champ, et les deux mesures n'y
/// avaient jamais été ajoutées. Une zone dont l'anneau se vidait se lisait
/// donc « saine » ici, avec les sept mêmes champs qu'une zone normale — ce
/// qui est pire que de ne rien annoncer, puisqu'une route qui répond sans
/// le champ se lit comme une absence de défaut.
///
/// Le piège est structurel : un champ ajouté à `ZonePollerMetrics` apparaît
/// tout seul dans `sync-status` et JAMAIS ici. C'est ce que garde
/// `sante_reseau_de_zone_tests.rs`.
pub(super) fn corps_network_health(
    zone_id: i64,
    poller: &tune_core::poller::ZonePollerMetrics,
    stream_bytes: u64,
    bitrate_kbps: Option<f64>,
) -> Value {
    json!({
        "zone_id": zone_id,
        "bytes_sent": stream_bytes,
        "bitrate_kbps": bitrate_kbps,
        "poll_latency_ms": poller.last_latency_ms,
        "max_latency_ms": poller.max_latency_ms,
        "poll_errors": poller.total_errors,
        "total_polls": poller.total_polls,
        // #3318 — ce que le DAC n'a pas reçu, par zone. Le cumul repart de
        // zéro à chaque piste, comme les compteurs de la sortie qu'il
        // recopie : c'est un état du flux EN COURS, pas un historique.
        "famine_anneau_evenements": poller.famine_anneau_evenements,
        "famine_anneau_silence_ms": poller.famine_anneau_silence_ms,
    })
}

pub(super) async fn list_zones(State(state): State<AppState>) -> Json<Value> {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zones = repo.list().unwrap_or_default();
    // DUP-1 (phase 2) : l'age de la derniere reponse, une requete pour toutes.
    let ages = repo.ages_depuis_derniere_vue().unwrap_or_default();
    let devices = state.scanner.devices().await;
    // Manually-added devices (e.g. legacy DLNA renderers that never appear in
    // SSDP discovery) are registered as outputs but absent from `devices`.
    // Treat a registered output as online too, otherwise its zone is shown
    // offline even though playback works.
    let registered_output_ids: std::collections::HashSet<String> =
        state.outputs.lock().await.list().into_iter().collect();
    let default_zone_id: Option<i64> =
        tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
            .get("default_zone_id")
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok());
    let audio_backend_pref = state.display_audio_backend();
    #[cfg(feature = "local-audio")]
    let audio_backend = tune_core::outputs::local::active_backend_name(&audio_backend_pref);
    #[cfg(not(feature = "local-audio"))]
    let audio_backend = "none";
    let mut result = Vec::new();
    for z in &zones {
        let zone_id = z.id.unwrap_or(0);
        let ps = state.playback.get_state(zone_id).await;
        let mut v = serde_json::to_value(z).unwrap_or_default();
        if let Some(obj) = v.as_object_mut() {
            obj.insert(
                "state".into(),
                json!(match ps.state {
                    tune_core::playback::PlayState::Playing => "playing",
                    tune_core::playback::PlayState::Paused => "paused",
                    tune_core::playback::PlayState::Stopped => "stopped",
                }),
            );
            obj.insert("current_track".into(), json!(ps.now_playing));
            inject_metadata_anchor(obj, &ps);
            inject_session_context(obj, &ps);
            obj.insert("position_ms".into(), json!(ps.position_ms));
            obj.insert("queue_length".into(), json!(ps.queue_length));
            // #3514 — le refus « radio hors file » (#3342) fait partie de la
            // décision : sans lui, le bouton reste actif et sans effet.
            let peut_avancer =
                crate::routes::playback::can_skip_next_publie(&state, zone_id, &ps).await;
            obj.insert("can_skip_next".into(), json!(peut_avancer));
            // L'aleatoire et la repetition appartiennent a la ZONE, et ils
            // survivent aux redemarrages : `queue_persistence` les enregistre
            // avec la file, `startup.rs` les restaure.
            //
            // Cette charge utile ne les portait pas. Le client naissait donc a
            // `shuffleEnabled = false` et n'avait aucun moyen d'apprendre le
            // contraire : ses deux sites de recalage lisent `zone.shuffle` /
            // `zone.repeat` (`App.svelte`, `syncTransportFromZone`), c'est-a-dire
            // des champs que personne n'envoyait. Seul un CLIC sur le bouton
            // remettait l'ecran d'accord avec le serveur — le geste qu'on
            // cherche justement a eviter.
            //
            // Resultat vecu par Tades (#2092) : un aleatoire actif cote serveur
            // et eteint a l'ecran, sans limite de duree. L'album part dans le
            // desordre, « suivant » saute au hasard, et le bouton qui
            // expliquerait tout parait inactif. Il a ouvert deux fils, en
            // ecrivant « je ne pense pas avoir parametre cela » : il avait
            // raison de ne pas s'en souvenir, rien ne le lui montrait.
            //
            // Le WebSocket, lui, les envoyait deja (`ws.rs`) : c'est REST qui
            // etait en retard, et c'est REST que le client lit au changement de
            // zone et apres chaque evenement de lecture.
            obj.insert("shuffle".into(), json!(ps.shuffle));
            obj.insert("repeat".into(), json!(ps.repeat));
            // #1274 — `volume` (linéaire, 0..1) et `volume_db` (atténuation en
            // dB, `null` = silence) sortent ensemble du même nombre. Le champ
            // dB est ADDITIF : aucun client déployé ne perd `volume`.
            tune_core::audio::volume_scale::inserer_volume(
                obj,
                if ps.volume > 0.0 {
                    ps.volume
                } else {
                    z.volume / 100.0
                },
            );
            let renderer_label = z
                .output_device_id
                .as_deref()
                .and_then(|id| devices.iter().find(|d| d.id == id).map(|d| d.name.as_str()));
            let wire = match ps
                .now_playing
                .as_ref()
                .and_then(|np| np.stream_id.as_deref())
            {
                Some(sid) => state.streamer.stream_output_wire(sid).await,
                None => None,
            };
            let signal_path = build_signal_path(
                &ps,
                z,
                &state.backend,
                renderer_label,
                audio_backend,
                wire.as_ref(),
            );
            obj.insert("signal_path".into(), json!(signal_path));
            // #1395 — quel backend local tourne VRAIMENT sur cette zone, face à
            // celui qui est réglé. Absent des zones non locales.
            if let Some(status) =
                local_backend_status_value(z.output_type.as_deref(), &audio_backend_pref)
            {
                obj.insert("audio_backend_status".into(), status);
            }
            // Recherche en cours (extraction YouTube longue) : l'interface peut le dire.
            obj.insert("resolving".into(), json!(ps.resolving));
            obj.insert("is_default".into(), json!(default_zone_id == Some(zone_id)));
            // Flux DoP en cours : le curseur de volume ne fait rien, et
            // l'interface doit le dire (#1735). Détecté sur les octets par la
            // sortie, pas déduit de `dsd_mode` — celui-ci dit ce qui a été
            // demandé, pas ce qui part sur le fil.
            obj.insert("dop_active".into(), json!(ps.dop_active));
            let zone_repo = ZoneRepo::with_backend(state.backend.clone());
            // #2369 — le mode DEMANDÉ n'est pas le transport OBTENU. Il reste
            // ICI : `dsd_transport` est un état DÉDUIT de la sortie, pas un
            // réglage de la zone, et il se lit comme `dop_active` juste
            // au-dessus.
            let dsd_mode = zone_repo.get_dsd_mode(zone_id);
            obj.insert("dsd_transport".into(), dsd_transport_value(z, &dsd_mode));
            // #2672 — les neuf réglages du panneau « Avancé · renderer »,
            // `dsd_mode` compris, par l'injecteur partagé avec `get_zone` et
            // `build_zone_json`.
            crate::routes::zones::injecter_reglages_renderer(obj, &zone_repo, zone_id);
            // `autoplay_enabled` est VOLONTAIREMENT absent de la requete SQL
            // de `ZoneRepo` (migration v36 pouvant echouer en silence sous
            // Windows), donc `row_to_zone` le met a `false` sans exception —
            // et la serialisation de la zone propageait ce faux jusqu'au
            // client. Le bouton AutoPlay retombait donc a chaque
            // resynchronisation, alors que le reglage etait bien en base et
            // correctement lu par le poller (Sandro, 0.9.70). On lit la vraie
            // valeur par l'accesseur prevu pour ca, comme les autres reglages
            // de zone ci-dessus.
            // #2271 — les deux champs sortent ensemble et decrivent la meme
            // colonne. `autoplay_enabled` reste emis tel quel : le client web
            // actuel ne lit que lui, et le retirer casserait le bouton.
            let autoplay_mode = zone_repo.get_autoplay_mode(zone_id);
            obj.insert(
                "autoplay_enabled".into(),
                json!(autoplay_mode != AutoplayMode::Off),
            );
            obj.insert("autoplay_mode".into(), json!(autoplay_mode.as_str()));
            let detected_dev = z
                .output_device_id
                .as_deref()
                .and_then(|did| devices.iter().find(|d| d.id == did));
            inject_device_identity(
                obj,
                &state.backend,
                zone_id,
                z.output_device_id.as_deref(),
                detected_dev,
            );
            let online = match z.output_type.as_deref() {
                // Browser zones have no output device by design (the web
                // client pulls stream_url itself) — always online.
                Some("browser") => true,
                // A local zone is online as long as it still has a device
                // assigned; an orphan row without output_device_id can never
                // play (Yacine, 24/07) and must be reported offline so
                // clients grey it out. Other types already fall through to
                // unwrap_or(false) when output_device_id is NULL.
                Some("local") => z.output_device_id.is_some(),
                _ => z
                    .output_device_id
                    .as_deref()
                    .map(|id| {
                        devices.iter().any(|d| d.id == id) || registered_output_ids.contains(id)
                    })
                    .unwrap_or(false),
            };
            obj.insert("online".into(), json!(online));
            for (cle, valeur) in
                super::presence::Presence::qualifier(online, ages.get(&zone_id).copied()).champs()
            {
                obj.insert(cle.into(), valeur);
            }
            obj.insert(
                "output_reach".into(),
                json!(output_reach(&state, z, &ps).await),
            );
            obj.insert(
                "levels_available".into(),
                json!(levels_available(&state, z).await),
            );
            obj.insert(
                "output_capabilities".into(),
                json!(output_capabilities(&state, z.output_device_id.as_deref()).await),
            );
            // #3164 — l'adresse du flux ne se publie QUE pour une zone
            // navigateur. Ce site-ci la rendait à toutes : un onglet ouvert sur
            // la liste des zones tenait de quoi couper la lecture d'un renderer.
            inject_stream_url(
                obj,
                &state,
                z.output_type.as_deref(),
                ps.now_playing
                    .as_ref()
                    .and_then(|np| np.stream_id.as_deref()),
            );
        }
        result.push(v);
    }
    Json(json!(result))
}

pub(super) async fn get_zone(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let audio_backend_pref = state.display_audio_backend();
    #[cfg(feature = "local-audio")]
    let audio_backend = tune_core::outputs::local::active_backend_name(&audio_backend_pref);
    #[cfg(not(feature = "local-audio"))]
    let audio_backend = "none";
    match repo.get(id) {
        Ok(Some(zone)) => {
            let ps = state.playback.get_state(id).await;
            let mut v = serde_json::to_value(&zone).unwrap_or_default();
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "state".into(),
                    json!(match ps.state {
                        tune_core::playback::PlayState::Playing => "playing",
                        tune_core::playback::PlayState::Paused => "paused",
                        tune_core::playback::PlayState::Stopped => "stopped",
                    }),
                );
                obj.insert("current_track".into(), json!(ps.now_playing));
                inject_metadata_anchor(obj, &ps);
                inject_session_context(obj, &ps);
                obj.insert("position_ms".into(), json!(ps.position_ms));
                obj.insert("queue_length".into(), json!(ps.queue_length));
                // Expose the queue index too so the client can refresh the
                // "now playing" highlight on track change without refetching the
                // whole queue (expensive under a large shuffle queue, #1096).
                obj.insert("queue_position".into(), json!(ps.queue_position));
                // #3514 — même décision qu'au-dessus, même refus.
                let peut_avancer =
                    crate::routes::playback::can_skip_next_publie(&state, id, &ps).await;
                obj.insert("can_skip_next".into(), json!(peut_avancer));
                // Meme raison qu'au-dessus (#2092) : c'est cette charge utile
                // que le client relit apres chaque evenement de lecture, et
                // c'est elle qui doit lui apprendre un aleatoire deja actif.
                obj.insert("shuffle".into(), json!(ps.shuffle));
                obj.insert("repeat".into(), json!(ps.repeat));
                // #1274 — même paire qu'au-dessus, depuis la même source :
                // ici la colonne `zones.volume`, arrondie au pour-cent.
                tune_core::audio::volume_scale::inserer_volume(obj, zone.volume / 100.0);
                let devices = state.scanner.devices().await;
                let registered_output_ids: std::collections::HashSet<String> =
                    state.outputs.lock().await.list().into_iter().collect();
                let renderer_label = zone
                    .output_device_id
                    .as_deref()
                    .and_then(|id| devices.iter().find(|d| d.id == id).map(|d| d.name.as_str()));
                let wire = match ps
                    .now_playing
                    .as_ref()
                    .and_then(|np| np.stream_id.as_deref())
                {
                    Some(sid) => state.streamer.stream_output_wire(sid).await,
                    None => None,
                };
                let signal_path = build_signal_path(
                    &ps,
                    &zone,
                    &state.backend,
                    renderer_label,
                    audio_backend,
                    wire.as_ref(),
                );
                obj.insert("signal_path".into(), json!(signal_path));
                // #1395 — voir la note au site jumeau (`list_zones`).
                if let Some(status) =
                    local_backend_status_value(zone.output_type.as_deref(), &audio_backend_pref)
                {
                    obj.insert("audio_backend_status".into(), status);
                }
                // Recherche en cours (extraction YouTube longue) : l'interface peut le dire.
                obj.insert("resolving".into(), json!(ps.resolving));
                // Voir la note au site jumeau : DoP en cours ⇒ volume inerte.
                obj.insert("dop_active".into(), json!(ps.dop_active));
                // Voir la note au site jumeau : le mode demandé n'est pas le
                // transport obtenu (#2369), et cet état déduit reste ici.
                let dsd_mode = repo.get_dsd_mode(id);
                obj.insert(
                    "dsd_transport".into(),
                    dsd_transport_value(&zone, &dsd_mode),
                );
                // #2672 — même injecteur que la liste et `build_zone_json`.
                crate::routes::zones::injecter_reglages_renderer(obj, &repo, id);
                // Meme correction que dans la liste : la valeur serialisee
                // depuis la struct vaut toujours `false`.
                // #2271 — meme paire que dans la liste.
                let autoplay_mode = repo.get_autoplay_mode(id);
                obj.insert(
                    "autoplay_enabled".into(),
                    json!(autoplay_mode != AutoplayMode::Off),
                );
                obj.insert("autoplay_mode".into(), json!(autoplay_mode.as_str()));
                let detected_dev = zone
                    .output_device_id
                    .as_deref()
                    .and_then(|did| devices.iter().find(|d| d.id == did));
                inject_device_identity(
                    obj,
                    &state.backend,
                    id,
                    zone.output_device_id.as_deref(),
                    detected_dev,
                );
                let online = match zone.output_type.as_deref() {
                    // Same rules as list_zones: browser zones need no device;
                    // a local zone without output_device_id is an orphan that
                    // can never play → offline.
                    Some("browser") => true,
                    Some("local") => zone.output_device_id.is_some(),
                    _ => zone
                        .output_device_id
                        .as_deref()
                        .map(|did| {
                            devices.iter().any(|d| d.id == did)
                                || registered_output_ids.contains(did)
                        })
                        .unwrap_or(false),
                };
                obj.insert("online".into(), json!(online));
                let age = ZoneRepo::with_backend(state.backend.clone())
                    .ages_depuis_derniere_vue()
                    .unwrap_or_default()
                    .get(&id)
                    .copied();
                for (cle, valeur) in super::presence::Presence::qualifier(online, age).champs() {
                    obj.insert(cle.into(), valeur);
                }
                obj.insert(
                    "output_reach".into(),
                    json!(output_reach(&state, &zone, &ps).await),
                );
                obj.insert(
                    "levels_available".into(),
                    json!(levels_available(&state, &zone).await),
                );
                obj.insert(
                    "output_capabilities".into(),
                    json!(output_capabilities(&state, zone.output_device_id.as_deref()).await),
                );
                // #3164 — même règle que la liste, et le même trou : la fiche
                // d'une zone DLNA rendait l'adresse de son flux au client web.
                inject_stream_url(
                    obj,
                    &state,
                    zone.output_type.as_deref(),
                    ps.now_playing
                        .as_ref()
                        .and_then(|np| np.stream_id.as_deref()),
                );
            }
            Json(v).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}
