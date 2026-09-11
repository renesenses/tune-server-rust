use std::sync::Arc;

use tracing::{debug, error, info, warn};

use tune_core::db::zone_repo::CreationDeZone;
use tune_core::outputs::OutputRegistry;
use tune_core::poller::{JournalSondage, TraceEchecSondage};

use crate::config::TuneConfig;
use crate::state::AppState;

/// Spawn all periodic background tasks: squeezebox polling, session GC, position poller,
/// token refresh, UPnP advertiser, alarm scheduler, Deezer proxy config, desktop notifications,
/// and RSS memory diagnostics.
pub async fn spawn_background_tasks(state: &AppState, config: &TuneConfig) {
    spawn_squeezebox_poller(state);
    spawn_hqplayer_poller(state);
    spawn_session_gc(state);
    spawn_dash_temp_gc();
    spawn_position_poller(state);
    spawn_token_refresher(state);
    spawn_tune_tested_refresher(state);
    spawn_upnp_advertiser(state, config).await;
    // Renderers UPnP par zone (#1750) : annonceur propre, relu à chaque
    // cycle — l'opt-in d'une zone prend effet sans redémarrage.
    crate::routes::upnp_media_renderer::spawn_renderer_advertiser(state.clone());
    configure_deezer_proxy(state, config).await;
    spawn_alarm_scheduler(state);
    spawn_desktop_notifications(state, config);
    spawn_memory_diagnostics(state.outputs.clone(), state.streamer.clone());
    spawn_telemetry_reporter(state);
    spawn_heartbeat(state);
    spawn_bio_sync(state);
    spawn_reprise_favoris_streaming(state);
    // CRD-5 : passe automatique des crédits, bornée par tour et reprenable par
    // curseur, derrière le même droit premium que les biographies. Une garde
    // dans `credits.rs` tient cette ligne : l'ordonnanceur de scan a été du
    // code mort pendant des mois pour une ligne comme celle-ci.
    crate::routes::library::credits::spawn_passe_automatique_credits(state);
    spawn_community_sync(state);
    spawn_replaygain_analysis(state);
    spawn_lyrics_catchup(state);
    #[cfg(feature = "audio-embedding")]
    spawn_audio_embedding(state);
    spawn_radio_logo_refresh(state);
    spawn_cloud_library_sync(state);
    spawn_local_audio_rescan(state);
    // Scan programmé (#2469). Cet appel manquait depuis la PR #1230 :
    // `spawn_scan_scheduler` était du code mort, la bascule des clients écrivait
    // un réglage que plus personne ne relisait. Un test de câblage garde la
    // ligne.
    crate::routes::system::scan::spawn_scan_scheduler(state.clone(), config.auto_scan);
    // Vérificateur périodique de mises à jour (#3217). Même défaut que la ligne
    // ci-dessus, et même remède : `UpdateChecker::spawn_periodic` n'avait qu'UNE
    // occurrence dans tout le dépôt — sa définition — et `TUNE_AUTO_UPDATE`
    // était accepté, réglable, et sans effet. Il NOTIFIE et n'installe rien :
    // la garde anti-coupure de #2954 se justifie par « toute installation est
    // un geste délibéré », et une installation automatique la ferait tomber.
    // Un test de câblage garde la ligne.
    crate::routes::system::update::spawn_verificateur_de_mise_a_jour(
        state.clone(),
        config.auto_update,
    );
    spawn_mp3_duration_repair(state);
    spawn_ssdp_startup_scan(state);
    spawn_slimproto_server(state, config.port);
    spawn_social_sharing_listener(state);
    crate::routes::developer_api::spawn_webhook_dispatcher(state);
    #[cfg(feature = "oaat")]
    spawn_oaat_stall_supervisor(state);
    #[cfg(feature = "cloud-relay")]
    spawn_relay_client(state).await;
}

/// Supervise OAAT zones and recover a stalled one with a stop+play restart.
///
/// The OAAT streaming loop can stall when the *source* transcode stream hangs
/// (the transcode downloads the whole track to a temp file before emitting any
/// PCM, so a slow/stalled hi-res download starves `stream.next()`). The output's
/// own 10s watchdog only reconnects the endpoint TCP — the wrong layer — and
/// cannot re-request a transcoded WAV session (they are not seekable and a re-GET
/// re-attaches to the same hung channel). The only reliable recovery is a fresh
/// play, exactly what the `/zones/{id}/stop` + `/zones/{id}/play` sequence does.
///
/// This supervisor polls every 10s, and when an OAAT zone reports a packet stall
/// (`playing && !paused && last_packet_age_ms > 30s`) it re-issues stop+play for
/// the current track. A per-device back-off (≥60s between restarts, give up after
/// 3 consecutive that don't recover → clean stop) prevents a restart loop on a
/// permanently-dead source. History is cleared once a device plays cleanly again.
///
/// NOTE : la doc de ce superviseur precede la fonction de reparation MP3 dans
/// ce fichier ; son `#[cfg(feature = "oaat")]` est pose sur la definition, plus
/// bas. Ne pas le remonter ici : il se reporterait sur l'element suivant.
/// Réparer les durées MP3 rognées par la borne inversée (#2027, #2034).
///
/// `mp3_duration_sanity_check` divisait la taille du fichier par le débit
/// MAXIMUM (320 kbps) pour en déduire une durée « plausible maximale ». C'est
/// la borne MINIMALE : le garde se déclenchait donc dès que le débit réel
/// passait sous 160 kbps, et **réécrivait la durée en base**. Un morceau de
/// 4 min 02 en 130 kbps était inscrit à 1 min 38.
///
/// Corriger la lecture ne suffit pas : les valeurs fausses sont persistées, et
/// un scan ordinaire saute les fichiers dont le mtime et la taille n'ont pas
/// bougé — elles ne seraient donc jamais relues. D'où cette passe.
///
/// Ce n'est pas un détail d'affichage. `duration_ms` note les candidats
/// MusicBrainz : ±10 points selon l'écart, sur une échelle où un candidat
/// sous 30 est REJETÉ. Une durée fausse de deux minutes coûte 20 points et
/// peut faire rejeter un appariement correct — l'enrichissement d'une
/// bibliothèque en 128 kbps est silencieusement dégradé.
///
/// **La détection est une requête, pas un balayage.** La valeur écrite lors du
/// rognage vaut exactement `file_size * 8 * 1000 / 320_000`, soit
/// `file_size / 40` en division entière. On ne relit donc que les fichiers
/// dont la durée porte cette signature. La tolérance de ±1 ms absorbe un
/// arrondi ultérieur — une égalité stricte raterait une piste pour un
/// millième.
///
/// Un MP3 réellement encodé à 320 kbps constant porte cette signature SANS
/// avoir été rogné. Le relire est sans effet : on récrit la durée qu'il a
/// déjà. Le faux positif est donc inoffensif par construction.
/// ⚠ TÉMOIN VERSIONNÉ — ne pas revenir à une clef fixe.
///
/// Les deux moitiés du correctif sont sorties DANS LE MAUVAIS ORDRE :
///
///   v0.9.93 — cette passe de réparation (#2034)
///   v0.9.94 — la correction du lecteur qui faussait les durées (#2027)
///
/// Quiconque a tourné en v0.9.93 a donc vu la passe réparer ses durées, poser
/// son témoin… puis un rescan avec le lecteur ENCORE fautif les re-fausser. Le
/// témoin étant posé, la passe ne serait jamais repassée : durées corrompues à
/// demeure, et un scan ordinaire saute les fichiers dont le mtime et la taille
/// n'ont pas bougé — rien ne les aurait relues.
///
/// Suffixer la clef fait repasser la passe UNE fois chez ces installations.
/// C'est bon marché pour les autres : la requête ne rend que les pistes
/// portant la signature de rognage, soit aucune sur une bibliothèque saine.
///
/// Toute correction future du lecteur devra incrémenter ce suffixe.
///
/// `_v2` → `_v3` (#1865) : la passe ouvrait le chemin de la base TEL QUEL,
/// c'est-à-dire en NFC. Sur un fichier venu de macOS ou d'un partage SMB, dont
/// le nom est en NFD, `probe_duration_ms` rendait `None` — la piste comptait
/// pour « illisible », puis le témoin GLOBAL était posé et la passe ne
/// repassait jamais. Le suffixe la fait repasser une fois, avec le repli de
/// graphie ; la requête ne rend que les pistes portant la signature de
/// rognage, donc aucune sur une bibliothèque saine.
fn spawn_mp3_duration_repair(state: &AppState) {
    let backend = state.backend.clone();
    tokio::spawn(async move {
        let reglages = tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());
        if reglages
            .get("mp3_duration_repair_done_v3")
            .ok()
            .flatten()
            .is_some()
        {
            return;
        }

        // Laisser le démarrage se terminer : cette passe lit des fichiers, et
        // rien ne presse au moment où l'utilisateur attend son interface.
        tokio::time::sleep(std::time::Duration::from_secs(90)).await;

        let candidats = match backend.query_many(
            "SELECT id, file_path, duration_ms, file_size FROM tracks              WHERE file_path LIKE '%.mp3'                AND file_size IS NOT NULL AND file_size > 0                AND duration_ms IS NOT NULL                AND ABS(duration_ms - file_size / 40) <= 1",
            &[],
        ) {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "mp3_duration_repair_query_failed");
                return;
            }
        };

        // Combien de MP3 échappent à la détection faute de taille connue :
        // sans ce chiffre, « 0 piste réparée » se lirait « rien à réparer ».
        let sans_taille = backend
            .query_one(
                "SELECT COUNT(*) FROM tracks WHERE file_path LIKE '%.mp3'                  AND (file_size IS NULL OR file_size = 0)",
                &[],
            )
            .ok()
            .flatten()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap_or(0);

        if candidats.is_empty() {
            info!(sans_taille, "mp3_duration_repair_rien_a_faire");
            let _ = reglages.set("mp3_duration_repair_done_v3", "1");
            return;
        }

        let total = candidats.len();
        info!(total, sans_taille, "mp3_duration_repair_demarre");

        let mut reparees = 0usize;
        let mut inchangees = 0usize;
        let mut illisibles = 0usize;
        // Distinct d'`illisibles` : un fichier ABSENT n'est pas un fichier
        // qu'on n'arrive pas à lire, et les confondre est ce qui a fait passer
        // 135 pistes NFD pour de la casse (#1865).
        let mut introuvables = 0usize;

        for ligne in &candidats {
            let (Some(id), Some(chemin), Some(ancienne)) = (
                ligne.first().and_then(|v| v.as_i64()),
                ligne.get(1).and_then(|v| v.as_string()),
                ligne.get(2).and_then(|v| v.as_i64()),
            ) else {
                continue;
            };

            // Le chemin de la base est en NFC ; le fichier peut être en NFD
            // sur le disque. On résout la graphie réelle avant d'ouvrir, et on
            // n'écrit RIEN dans `tracks.file_path` : la base reste NFC (#1865).
            let Some(sur_disque) =
                tune_core::library::local_path::resolve_existing_local_path(&chemin)
            else {
                introuvables += 1;
                debug!(id, chemin = %chemin, "mp3_duration_repair_chemin_introuvable");
                continue;
            };

            let chemin_clone = sur_disque;
            let reelle = tokio::task::spawn_blocking(move || {
                tune_core::metadata::probe_duration_ms(std::path::Path::new(&chemin_clone))
            })
            .await
            .ok()
            .flatten();

            let Some(reelle) = reelle else {
                illisibles += 1;
                continue;
            };

            let reelle = reelle as i64;
            // Une seconde d'écart : en-deçà, la valeur en base est déjà juste
            // et la réécrire ne ferait que du bruit d'écriture.
            if (reelle - ancienne).abs() <= 1000 {
                inchangees += 1;
                continue;
            }

            match backend.execute(
                "UPDATE tracks SET duration_ms = ? WHERE id = ?",
                &[&reelle as &dyn tune_core::db::backend::ToSqlValue, &id],
            ) {
                Ok(_) => {
                    reparees += 1;
                    debug!(id, ancienne, reelle, chemin = %chemin, "mp3_duration_reparee");
                }
                Err(e) => warn!(id, error = %e, "mp3_duration_repair_ecriture_echouee"),
            }
        }

        info!(
            total,
            reparees,
            inchangees,
            illisibles,
            introuvables,
            sans_taille,
            "mp3_duration_repair_termine"
        );
        let _ = reglages.set("mp3_duration_repair_done_v3", "1");
    });
}

#[cfg(feature = "oaat")]
fn spawn_oaat_stall_supervisor(state: &AppState) {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    let orchestrator = state.orchestrator.clone();
    let outputs = state.outputs.clone();
    let playback = state.playback.clone();
    let backend = state.backend.clone();

    // Restart only after the stall has persisted well beyond the output's own
    // 10s watchdog window, so a transient hiccup that self-recovers is left alone.
    const STALL_MS: u64 = 30_000;
    const MIN_INTERVAL: Duration = Duration::from_secs(60);
    const MAX_CONSECUTIVE: u32 = 3;

    tokio::spawn(async move {
        // device_id → (last restart, consecutive restart count)
        let mut history: HashMap<String, (Instant, u32)> = HashMap::new();
        let mut ticker = tokio::time::interval(Duration::from_secs(10));

        loop {
            ticker.tick().await;

            // Collect the device_id + current stall state of every OAAT output,
            // then release the registry lock before doing any stop/play I/O.
            let mut states: Vec<(String, bool)> = Vec::new();
            {
                let reg = outputs.lock().await;
                for device_id in reg.list() {
                    if !device_id.starts_with("oaat:") && !device_id.starts_with("oaat-group:") {
                        continue;
                    }
                    if let Some(arc) = reg.get(&device_id) {
                        let out = arc.lock().await;
                        if let Some(oaat) = out
                            .as_any()
                            .downcast_ref::<tune_core::outputs::oaat::OaatOutput>()
                        {
                            let snap = oaat.diagnostics_snapshot();
                            let playing = snap["playing"].as_bool().unwrap_or(false);
                            let paused = snap["paused"].as_bool().unwrap_or(false);
                            let age = snap["last_packet_age_ms"].as_u64().unwrap_or(0);
                            let stalled = playing && !paused && age > STALL_MS;
                            states.push((device_id.clone(), stalled));
                        }
                    }
                }
            }

            for (device_id, stalled) in states {
                if !stalled {
                    // Healthy again → forget any prior restart history so the next
                    // isolated stall starts from a clean consecutive count.
                    history.remove(&device_id);
                    continue;
                }

                let now = Instant::now();
                let count = match history.get(&device_id) {
                    // Backing off: restarted this device too recently, wait.
                    Some((last, _)) if now.duration_since(*last) < MIN_INTERVAL => continue,
                    Some((_, c)) => *c,
                    None => 0,
                };

                let zone = match tune_core::db::zone_repo::ZoneRepo::with_backend(backend.clone())
                    .get_by_device_id(&device_id)
                {
                    Ok(Some(z)) => z,
                    _ => continue,
                };
                let zone_id = match zone.id {
                    Some(id) => id,
                    None => continue,
                };

                if count >= MAX_CONSECUTIVE {
                    // Restarts aren't helping — stop cleanly so we stop hammering
                    // the endpoint and leave a well-defined idle state. Once stopped
                    // the zone is no longer "playing" so this won't fire again until
                    // the user replays.
                    error!(
                        zone_id,
                        device_id = %device_id,
                        "oaat_stall_supervisor_giving_up_stopping_zone"
                    );
                    orchestrator.stop(zone_id, Some(&device_id)).await;
                    history.remove(&device_id);
                    continue;
                }

                info!(
                    zone_id,
                    device_id = %device_id,
                    attempt = count + 1,
                    "oaat_stall_supervisor_restarting_zone"
                );

                orchestrator.stop(zone_id, Some(&device_id)).await;
                tokio::time::sleep(Duration::from_secs(2)).await;

                let st = playback.get_state(zone_id).await;
                if let Some(np) = st.now_playing {
                    let req = tune_core::orchestrator::PlayRequest {
                        zone_id,
                        output_device_id: Some(device_id.clone()),
                        track_id: np.track_id,
                        source: if np.source == "local" {
                            None
                        } else {
                            Some(np.source.clone())
                        },
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
                    match orchestrator.play(req).await {
                        Ok(_) => {
                            // Mark the restart so the poller suppresses a phantom
                            // gapless auto-advance: this replay restarts the
                            // CURRENT track from 0, and that position drop would
                            // otherwise be read as a real transition, running
                            // now-playing one track ahead of the audio.
                            playback.mark_restart(zone_id).await;
                            // Restore the queue length from the DB so the poller
                            // keeps auto-advancing after the restart (mirrors the
                            // /zones/{id}/play handler; without it a mid-album stall
                            // recovery would play the current track then stop).
                            let qr = tune_core::db::play_queue_repo::PlayQueueRepo::with_backend(
                                backend.clone(),
                            );
                            let q_len = qr.count(zone_id).unwrap_or(0)
                                + qr.count_streaming(zone_id).unwrap_or(0);
                            if q_len > 0 {
                                let cur_pos = playback.get_state(zone_id).await.queue_position;
                                playback.update_queue_info(zone_id, cur_pos, q_len).await;
                            }
                        }
                        Err(e) => {
                            tracing::warn!(zone_id, error = %e, "oaat_stall_supervisor_replay_failed");
                        }
                    }
                }

                history.insert(device_id.clone(), (now, count + 1));
            }
        }
    });
}

#[cfg(feature = "cloud-relay")]
async fn spawn_relay_client(state: &AppState) {
    // Premium gate: Cloud Relay requires Premium
    if !state
        .license
        .check_feature(tune_core::license::Feature::CloudRelay)
        .await
    {
        info!("cloud_relay_requires_premium");
        return;
    }

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    if let Some(_client) = tune_core::cloud::relay::spawn_relay_client(&settings, state.port) {
        info!("cloud relay client spawned");
    }
}

fn spawn_ssdp_startup_scan(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        // Multiple scan passes to catch slow DLNA renderers (DMP-A8, etc.)
        // that don't respond to the first SSDP multicast.
        for (pass, delay_secs) in [(1, 3), (2, 8), (3, 15)] {
            tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
            if pass == 1 {
                info!("ssdp_startup_scan_starting");
            }

            let scanner = &state.scanner;
            let devices = scanner.rescan().await;

            let mut registered = 0u32;
            let mut outputs = state.outputs.lock().await;
            for d in &devices {
                let location = d.location.as_deref().unwrap_or("");
                if location.is_empty() || outputs.contains(&d.id) {
                    continue;
                }
                // #1280 — appareil que l'utilisateur a fait taire : ce lot de
                // démarrage enregistrait la sortie sans rien demander, donc
                // l'appareil revenait proposé à chaque redémarrage.
                if crate::discovery_setup::appareil_ignore(&state.backend, d) {
                    info!(name = %d.name, device_id = %d.id, "ssdp_startup_appareil_ignore");
                    continue;
                }
                if let Ok(desc) =
                    tune_core::discovery::xml_parser::fetch_device_description(location).await
                {
                    if desc.is_media_renderer() {
                        let service_urls = desc.service_urls();
                        if let (Some(av), Some(rc)) = (
                            service_urls.get("avtransport"),
                            service_urls.get("renderingcontrol"),
                        ) {
                            let base = format!("http://{}:{}", d.host, d.port);
                            let cm_url = service_urls
                                .get("connectionmanager")
                                .or_else(|| service_urls.get("ConnectionManager"))
                                .map(|p| format!("{base}{p}"));
                            let dlna = tune_core::outputs::dlna::DlnaOutput::new(
                                d.name.clone(),
                                d.id.clone(),
                                d.host.clone(),
                                format!("{base}{av}"),
                                format!("{base}{rc}"),
                                cm_url,
                            )
                            .with_upnp_events(
                                crate::startup::create_oh_listener().await,
                                crate::discovery_setup::urls_evenements_dlna(
                                    &d.host,
                                    d.port,
                                    &desc.event_sub_urls(),
                                ),
                            )
                            .with_upnp_silence(
                                crate::config::resolve_upnp_silence(&state.backend, &d.id),
                            );
                            outputs.register(Box::new(dlna));
                            registered += 1;
                        }
                    }
                }
            }
            drop(outputs);

            let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
            for d in &devices {
                // Respect user deletions: a device the user removed is marked
                // hidden. The mDNS/SSDP live handlers already skip zone creation
                // for hidden devices — this startup batch path must do the same,
                // otherwise a deleted zone silently reappears on every restart
                // (Fabien: "Salon: AIRPLAY" zone came back after update).
                if zone_repo.is_device_hidden(&d.id) {
                    info!(name = %d.name, device_id = %d.id, "ssdp_startup_zone_hidden_skipping");
                    continue;
                }
                // #1280 — appareil ignoré : aucune zone, sous aucune de ses
                // identités.
                if crate::discovery_setup::appareil_ignore(&state.backend, d) {
                    info!(name = %d.name, device_id = %d.id, "ssdp_startup_zone_appareil_ignore");
                    continue;
                }

                // Cross-protocol duplicate guard (Phase B, #1239): the startup
                // batch had NO dedup at all — a device already owning a zone
                // under another protocol (BluOS via mDNS) gained a second
                // "dlna" zone here on every fresh boot. Match on the persisted
                // host/MAC identity of visible zones.
                if let Some((zid, zname, ztype)) =
                    zone_repo.find_visible_zone_by_identity(&d.host, d.mac_address.as_deref())
                {
                    if !ztype.is_empty() && !ztype.eq_ignore_ascii_case("dlna") {
                        info!(
                            name = %d.name,
                            device_id = %d.id,
                            host = %d.host,
                            conflicting_zone = %zname,
                            conflicting_zone_id = zid,
                            conflicting_type = %ztype,
                            "ssdp_startup_zone_skipped_conflicting_protocol"
                        );
                        continue;
                    }
                }

                // #3529 — ce lot tourne à CHAQUE démarrage et ne consultait pas
                // « Créer automatiquement les zones ». Le commentaire qui
                // tenait ici lieu de justification (« auto-created zones start
                // dormant … discovery may always register a device ») parle du
                // plafond du palier gratuit, PAS du réglage : une zone naît en
                // ligne (`online` vaut `DEFAULT 1` au schéma). D'où « elles
                // apparaissent ET s'activent toutes seules », chez Fabien, sur
                // une installation où la case est décochée.
                match zone_repo.get_or_create_si_autorise(
                    &d.name,
                    Some("dlna"),
                    &d.id,
                    "ssdp_startup",
                ) {
                    Ok(CreationDeZone::Creee(zid)) => {
                        let _ = zone_repo.set_identity(zid, &d.host, d.mac_address.as_deref());
                        info!(name = %d.name, zone_id = zid, device_id = %d.id, "ssdp_startup_zone_created");
                    }
                    Ok(CreationDeZone::Existante(zid)) => {
                        let _ = zone_repo.set_identity(zid, &d.host, d.mac_address.as_deref());
                        let _ = zone_repo.set_online_by_device(&d.id, true);
                    }
                    Ok(CreationDeZone::Refusee) => {
                        info!(name = %d.name, device_id = %d.id, "ssdp_startup_zone_auto_create_disabled_skipping");
                    }
                    Err(e) => {
                        tracing::warn!(name = %d.name, device_id = %d.id, error = %e, "ssdp_startup_zone_create_failed");
                    }
                }
            }

            info!(
                registered,
                total = devices.len(),
                pass,
                "ssdp_startup_scan_complete"
            );

            if pass > 1 && registered == 0 {
                break;
            }
        }
    });
}

/// Plein rythme d'un sondeur d'intégration : l'hôte répond, on le rappelle
/// chaque minute.
pub const SONDAGE_INTERVALLE_BASE_SECS: u64 = 60;
/// Plancher de fréquence quand il ne répond plus. Dix minutes : un hôte éteint
/// ne coûte plus que six connexions perdues par heure.
pub const SONDAGE_INTERVALLE_MAX_SECS: u64 = 600;

/// Cadence du prochain tour d'un sondeur d'intégration.
///
/// Écrite en clair dans `spawn_squeezebox_poller`, elle en est extraite parce
/// qu'il fallait la donner **aussi** à `spawn_hqplayer_poller` (#2566) : ce
/// dernier était le seul sondeur sans aucun recul. Un hôte HQPlayer saisi puis
/// débranché était rappelé toutes les soixante secondes, sans fin — **1 440
/// connexions perdues et 1 440 lignes de journal par jour**, pour un appareil
/// dont on savait depuis la première seconde qu'il ne répondait pas.
///
/// Le retour au plein rythme est immédiat, et il ne dépend pas d'un succès :
/// une intégration coupée ou un hôte vidé y ramènent aussi, pour qu'un hôte
/// fraîchement saisi soit pris tout de suite.
///
/// Fonction pure, et testée comme telle (`journal_sondage_hqplayer.rs`) : une
/// cadence éprouvée par de vraies attentes est un test qui dure dix minutes et
/// qui clignote.
pub fn prochain_intervalle_sondage(actuel_secs: u64, echec: bool) -> u64 {
    if echec {
        actuel_secs
            .saturating_mul(2)
            .min(SONDAGE_INTERVALLE_MAX_SECS)
    } else {
        SONDAGE_INTERVALLE_BASE_SECS
    }
}

/// Point d'émission du journal d'un sondage HQPlayer en échec (#2566).
///
/// Fonction à part, et publique, pour une seule raison : le garde
/// `tests/journal_sondage_hqplayer.rs` compte les lignes que `tracing` reçoit
/// de **ce point-ci**, pas d'une copie. Elle reste dans `tune_server::background`
/// pour que la cible du module — celle que l'export de diagnostic comptabilise
/// (`QUOTA_PAR_MODULE`, #1974) — désigne bien le sondeur d'intégration et non
/// le poller.
///
/// Ce que le journal dit désormais, pour un HQPlayer débranché :
/// les **cinq premiers** échecs en entier, avec l'erreur, l'hôte et la cadence
/// suivante — de quoi établir la cause et vérifier que le recul monte —, puis
/// un récapitulatif portant le TOTAL aux paliers 8, 16, 32… Un échec isolé
/// reste dit en entier : c'est le n° 1.
pub fn journaliser_echec_hqplayer(
    journal: &mut JournalSondage,
    host: &str,
    error: &dyn std::fmt::Display,
    next_retry_secs: u64,
) {
    match journal.compter_echec() {
        TraceEchecSondage::Detaille => tracing::debug!(
            error = %error,
            host = %host,
            next_retry_secs,
            "hqplayer_poll_failed"
        ),
        TraceEchecSondage::Recapitulatif => tracing::debug!(
            error = %error,
            host = %host,
            echecs = journal.echecs(),
            detaillees = tune_core::poller::ECHECS_SONDAGE_DETAILLES,
            next_retry_secs,
            "hqplayer_poll_still_failing"
        ),
        TraceEchecSondage::Muet => {}
    }
}

/// Point d'émission du journal d'un sondage HQPlayer qui répond (#2566).
///
/// `hqplayer_poll_registered` partait à **chaque tour**, au niveau `INFO` : un
/// HQPlayer qui marche écrivait 1 440 lignes par jour pour dire qu'il marchait
/// toujours. C'est le même défaut que les 79 lignes du ticket, à l'envers — un
/// journal qui crie pour rien noie celui qui dirait quelque chose — et il est
/// ici plus grave, parce qu'`INFO` traverse les filtres par défaut.
///
/// Reste dit : la **première** découverte, et chaque **retour** après une panne
/// qu'on avait cessé de détailler, avec le total d'échecs.
pub fn journaliser_succes_hqplayer(
    journal: &mut JournalSondage,
    host: &str,
    deja_annonce: &mut bool,
) {
    let retour = journal.cloturer();
    if retour.is_some() || !*deja_annonce {
        info!(
            host = %host,
            echecs = retour.unwrap_or(0),
            "hqplayer_poll_registered"
        );
        *deja_annonce = true;
    }
}

fn spawn_squeezebox_poller(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        // Base cadence when the LMS answers; exponential backoff up to the cap
        // when it doesn't. Without this, an unreachable/refused LMS (e.g. a
        // Daphile box that's off, or a user who never ran one) is hammered with
        // a failed TCP connect every 60s forever — that was Yacine's log filled
        // with `lms_cli_command: TCP connect failed` every minute. Backing off
        // stops the spam and the wasted connects; a reachable LMS is polled
        // normally and recovery resets the interval immediately.
        // Cadence partagée avec le sondeur HQPlayer : voir
        // `prochain_intervalle_sondage` (60 s de base, 600 s de plancher).
        let mut interval_secs = SONDAGE_INTERVALLE_BASE_SECS;
        loop {
            let settings =
                tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
            let enabled = settings
                .get("squeezebox_enabled")
                .ok()
                .flatten()
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false);
            let host = settings
                .get("lms_host")
                .ok()
                .flatten()
                .or_else(|| settings.get("squeezebox_host").ok().flatten())
                .unwrap_or_default();

            if enabled && !host.is_empty() {
                match crate::routes::squeezebox::discover_and_register(&state).await {
                    Ok(players) => {
                        if !players.is_empty() {
                            info!(count = players.len(), lms = %host, "squeezebox_poll_discovered");
                        }
                        // Reachable → back to normal cadence.
                        interval_secs = prochain_intervalle_sondage(interval_secs, false);
                    }
                    Err(e) => {
                        interval_secs = prochain_intervalle_sondage(interval_secs, true);
                        tracing::debug!(
                            error = %e,
                            lms = %host,
                            next_retry_secs = interval_secs,
                            "squeezebox_poll_failed_backing_off"
                        );
                    }
                }
            } else {
                // Integration off / no host configured — idle at base cadence so
                // a freshly configured host is picked up promptly.
                interval_secs = SONDAGE_INTERVALLE_BASE_SECS;
            }

            tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
        }
    });
}

fn spawn_hqplayer_poller(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
        // Même cadence et même comptabilité de journal que le sondeur
        // Squeezebox ci-dessus (#2566). Ce sondeur-ci était le seul à n'avoir
        // ni l'une ni l'autre : 1 440 tours et 1 440 lignes par jour, que
        // l'hôte réponde ou non.
        let mut interval_secs = SONDAGE_INTERVALLE_BASE_SECS;
        let mut journal = JournalSondage::default();
        let mut deja_annonce = false;
        loop {
            let settings =
                tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
            let enabled = settings
                .get("hqplayer_enabled")
                .ok()
                .flatten()
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false);
            let host = settings
                .get("hqplayer_host")
                .ok()
                .flatten()
                .unwrap_or_default();

            if enabled && !host.is_empty() {
                match crate::routes::hqplayer::discover_and_register(&state).await {
                    Ok(_) => {
                        journaliser_succes_hqplayer(&mut journal, &host, &mut deja_annonce);
                        interval_secs = prochain_intervalle_sondage(interval_secs, false);
                    }
                    Err(e) => {
                        interval_secs = prochain_intervalle_sondage(interval_secs, true);
                        journaliser_echec_hqplayer(&mut journal, &host, &e, interval_secs);
                    }
                }
            } else {
                // Intégration coupée ou hôte non saisi : plein rythme, pour
                // qu'un hôte fraîchement configuré soit pris tout de suite.
                interval_secs = SONDAGE_INTERVALLE_BASE_SECS;
            }

            tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
        }
    });
}

/// Sweep leaked Tidal/Qobuz DASH temp files (`tune-dash-*.mp4`). The local
/// transcode no longer deletes the tidal-cache-owned source right after decoding
/// (that caused the ASIO repeat re-download runaway), so files older than well
/// past the cache TTL — no longer served from cache nor being decoded — are
/// removed here to avoid accumulating ~54MB files across a listening session.
fn spawn_dash_temp_gc() {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            ticker.tick().await;
            let dir = std::env::temp_dir();
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            let now = std::time::SystemTime::now();
            let mut removed = 0u32;
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !(name.starts_with("tune-dash-") && name.ends_with(".mp4")) {
                    continue;
                }
                let too_old = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|m| now.duration_since(m).ok())
                    .map(|age| age.as_secs() > 600) // 10 min ≫ 240s cache TTL
                    .unwrap_or(false);
                if too_old && std::fs::remove_file(entry.path()).is_ok() {
                    removed += 1;
                }
            }
            if removed > 0 {
                info!(removed, "dash_temp_gc_sweep");
            }
        }
    });
}

fn spawn_session_gc(state: &AppState) {
    let streamer = state.streamer.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            ticker.tick().await;
            let removed = streamer.cleanup_stale_sessions().await;
            if removed > 0 {
                info!(removed, "session_gc_sweep");
            }
        }
    });
}

fn spawn_position_poller(state: &AppState) {
    let poller = tune_core::poller::PositionPoller::new(
        state.orchestrator.clone(),
        state.playback.clone(),
        state.outputs.clone(),
        state.backend.clone(),
        state.poller_metrics.clone(),
    );
    poller.spawn();
}

/// #3589, volet A — le catalogue « Tune tested », au démarrage puis toutes les
/// six heures.
///
/// Six heures, et non l'heure du `Cache-Control: public, max-age=3600` mesuré
/// sur la réponse du site : une validation d'appareil n'est pas une urgence, et
/// `version` étant un entier qui ne recule jamais, un tour qui ne trouve rien
/// de neuf ne coûte qu'une comparaison.
///
/// 🔴 Ne rend jamais d'erreur : hors ligne, `rafraichir` rend `Repli` et
/// l'instance garde ce qu'elle a — le dernier catalogue rangé, ou le catalogue
/// embarqué si elle n'en a jamais obtenu.
fn spawn_tune_tested_refresher(state: &AppState) {
    let db = state.backend.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(6 * 3600));
        loop {
            // `interval` déclenche IMMÉDIATEMENT son premier tour : c'est le
            // « au démarrage » de l'issue, sans second appel à écrire.
            ticker.tick().await;
            match tune_core::cloud::tune_tested::rafraichir(&db).await {
                tune_core::cloud::tune_tested::Issue::Range { avant, apres } => {
                    tracing::info!(avant, apres, "tune_tested_catalogue_mis_a_jour");
                }
                tune_core::cloud::tune_tested::Issue::Inchange(v) => {
                    tracing::debug!(version = v, "tune_tested_catalogue_inchange");
                }
                tune_core::cloud::tune_tested::Issue::Repli(raison) => {
                    tracing::debug!(%raison, "tune_tested_catalogue_repli");
                }
            }
        }
    });
}

fn spawn_token_refresher(state: &AppState) {
    let services = state.services.clone();
    let db = state.backend.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            ticker.tick().await;
            let registry = services.lock().await;
            for name in registry.list() {
                if let Some(svc) = registry.get(&name) {
                    let mut svc = svc.write().await;
                    match svc.refresh_if_needed().await {
                        Ok(true) => {
                            let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(
                                db.clone(),
                            );
                            let key = format!("auth_tokens_{name}");
                            if svc.session_expired() {
                                // The token was rejected and could not be
                                // renewed. Delete the row instead of leaving a
                                // credential the provider has refused, which a
                                // restart would dutifully reload.
                                settings.delete(&key).ok();
                                tracing::warn!(service = %name, "expired_session_row_deleted");
                            } else if let Some(tokens) = svc.save_tokens() {
                                settings.set(&key, &tokens.to_string()).ok();
                            }
                        }
                        Ok(false) => {}
                        Err(e) => {
                            tracing::warn!(service = %name, error = %e, "token_refresh_failed");
                        }
                    }
                }
            }
        }
    });
}

async fn spawn_upnp_advertiser(state: &AppState, config: &TuneConfig) {
    if let Some(ref upnp) = state.upnp {
        // `upnp_enabled` (POST /api/v1/upnp/config) était écrit mais jamais
        // lu : désactiver le media server depuis l'interface n'avait aucun
        // effet. Les routes /upnp restent montées (inertes sans annonce) ;
        // la bascule prend effet au redémarrage, comme le renommage.
        let enabled =
            tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
                .get("upnp_enabled")
                .ok()
                .flatten()
                .map(|v| v != "false" && v != "0")
                .unwrap_or(true);
        if !enabled {
            info!("upnp_mediaserver_disabled_by_setting");
            return;
        }
        // La LOCATION n'est plus figée ici : l'annonceur recalcule l'IP à
        // chaque cycle et n'annonce rien tant qu'aucune IP réseau n'est
        // disponible — l'ancien repli « 127.0.0.1 » calculé une fois au
        // démarrage publiait du loopback à vie (#1614).
        tune_core::upnp_server::spawn_ssdp_advertiser(
            upnp.uuid.clone(),
            config.port,
            config.advertised_ip.clone(),
        )
        .await;
        info!("upnp_mediaserver_advertiser_started");
    }
}

async fn configure_deezer_proxy(state: &AppState, config: &TuneConfig) {
    let registry = state.services.lock().await;
    if let Some(svc) = registry.get("deezer") {
        let mut svc = svc.write().await;
        if let Some(deezer) = svc
            .as_any_mut()
            .downcast_mut::<tune_core::streaming::deezer::DeezerService>()
        {
            let server_ip = tune_core::discovery::ssdp::get_local_ip()
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "127.0.0.1".into());
            deezer.set_proxy_base_url(Some(format!(
                "http://{}:{}/deezer-proxy",
                server_ip, config.port
            )));
            info!("deezer_proxy_configured");
        }
    }
}

fn spawn_alarm_scheduler(state: &AppState) {
    let alarm_sched = Arc::new(tune_core::alarms::AlarmScheduler::with_backend(
        state.backend.clone(),
        state.orchestrator.clone(),
    ));
    alarm_sched.spawn();
}

fn spawn_desktop_notifications(state: &AppState, config: &TuneConfig) {
    if tune_core::notifications::is_enabled() {
        let server_ip = tune_core::discovery::ssdp::get_local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "127.0.0.1".into());
        let server_base = Arc::new(format!("http://{}:{}", server_ip, config.port));
        tune_core::notifications::spawn_notification_listener(
            state.event_bus.subscribe(),
            server_base,
        );
    }
}

fn spawn_telemetry_reporter(state: &AppState) {
    // #3383 : le ping de demarrage recoit la base, parce qu'il doit lire le
    // consentement avant d'envoyer version, OS, arch et liste des services.
    // #3380 : les deux envois recoivent le repertoire web REELLEMENT servi
    // (`resolve_web_dir`, le meme que le routeur), pour y relire la version de
    // l'interface a chaque battement. Sans ce chemin, mozaiklabs ne connait que
    // la version du binaire — et `web/` est deploye separement.
    let web_dir = crate::config::resolve_web_dir();
    tune_core::cloud::telemetry::spawn_startup_ping(
        state.backend.clone(),
        state.services.clone(),
        web_dir.clone(),
    );
    tune_core::cloud::telemetry::TelemetryReporter::spawn(
        state.backend.clone(),
        state.services.clone(),
        web_dir,
    );
}

/// Cadence du battement de coeur vers mozaiklabs.fr.
///
/// Une heure, et non les 300 s d'origine (#2416). Le testeur a mesure l'ancienne
/// cadence sur son propre journal : un `POST /api/v1/heartbeat` — et, compte
/// mozaiklabs connecte, un `GET /api/v1/user` — toutes les cinq minutes, soit
/// pres de 300 allers-retours par jour sur une machine qui ne fait qu'ecouter
/// de la musique.
///
/// Rien dans le produit n'exige cette frequence. Les droits (licence a clef
/// comme compte SSO) sont couverts par une grace hors-ligne de 14 jours
/// (`tune-core/src/license.rs`, `GRACE_PERIOD_DAYS`) : un rafraichissement
/// horaire laisse ~336 occasions de revalider avant la moindre degradation.
/// Les 5 minutes repondaient a un besoin d'outil d'administration temps reel
/// — voir toutes les instances vivantes dans la console — pas a un besoin de
/// l'utilisateur, qui en paie le trafic et les journaux.
///
/// Les autres boucles de ce fichier partagent le meme 300 s et ne sont PAS
/// concernees : GC des temporaires DASH, rafraichisseur de jetons, diagnostics
/// memoire. Elles sont locales et n'appellent personne.
const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3600);

/// Reprise rapprochée après un battement — ou une revalidation de clé — en
/// échec : 1 min, puis 5, puis 15, puis la cadence horaire. Sans elle, un
/// hôte injoignable au démarrage (réseau pas encore monté, coupure) laissait
/// la licence non confirmée pendant une heure pleine (LIC-3). Un 429 n'est
/// pas un échec de ce type : son `Retry-After` est respecté tel quel.
const REPRISES_BATTEMENT_SECS: [u64; 3] = [60, 300, 900];

fn prochain_intervalle_battement(echecs_consecutifs: u32) -> std::time::Duration {
    if echecs_consecutifs == 0 {
        return HEARTBEAT_INTERVAL;
    }
    REPRISES_BATTEMENT_SECS
        .get(echecs_consecutifs as usize - 1)
        .map(|s| std::time::Duration::from_secs(*s))
        .unwrap_or(HEARTBEAT_INTERVAL)
}

/// Ce qu'un tour de battement a le droit de faire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatPlan {
    /// Envoyer la charge utile a `POST /api/v1/heartbeat`.
    pub send_heartbeat: bool,
    /// Rafraichir les droits premium du compte SSO (`GET /api/v1/user`).
    pub refresh_account: bool,
    /// La clé de licence se revalide par `POST /api/v1/license/validate`,
    /// une charge à trois champs sans rien de descriptif — donc même quand
    /// la télémétrie est refusée. Sans cela, un Premium à CLÉ en opt-out
    /// retombait en Free au bout de la grâce de 14 jours (LIC-1).
    pub revalidate_key: bool,
}

/// Decide ce que fait le tour de battement en fonction de l'opt-out telemetrie.
///
/// `TUNE_TELEMETRY=false` coupe l'ENVOI du battement — c'est bien lui qui porte
/// la charge utile descriptive (version, plateforme, nombre de pistes, nom
/// d'hote, services authentifies, liste des appareils).
///
/// Il ne coupe PAS le rafraichissement des droits premium du compte SSO, et
/// c'est delibere : ce n'est pas de la telemetrie mais l'entretien du service
/// que l'utilisateur a achete. Le couper ferait retomber un abonne payant en
/// gratuit au bout de la grace de 14 jours, pour avoir refuse d'etre compte
/// dans une statistique. Un opt-out ne doit jamais se payer en fonctionnalites
/// perdues. L'appel ne part de toute facon que si un `mozaik_access_token`
/// existe, c'est-a-dire si l'utilisateur a lui-meme lie son compte.
fn heartbeat_plan(telemetry_enabled: bool) -> HeartbeatPlan {
    HeartbeatPlan {
        send_heartbeat: telemetry_enabled,
        refresh_account: true,
        revalidate_key: true,
    }
}

/// Le plan d'UN tour, decide a partir des reglages de CETTE instance.
///
/// #3383 — c'est le seul site d'appel de `heartbeat_plan` en production, et
/// c'est ici que se lit le consentement. `is_enabled_for` et non `is_enabled` :
/// le refus pose dans l'interface (`POST /cloud/telemetry/disable`) compte
/// autant que celui pose dans l'environnement (`TUNE_TELEMETRY=false`).
///
/// Fonction et non expression en ligne, pour qu'un temoin puisse APPELER la
/// decision reelle plutot que d'en recopier les termes.
pub fn plan_du_tour(settings: &tune_core::db::settings_repo::SettingsRepo) -> HeartbeatPlan {
    heartbeat_plan(tune_core::cloud::telemetry::TelemetryReporter::is_enabled_for(settings))
}

/// Lightweight heartbeat — honours `TUNE_TELEMETRY` (#2416).
///
/// Sends a ping every `HEARTBEAT_INTERVAL` (1 h) to mozaiklabs.fr so the admin
/// can see running instances.  Also carries license_key and
/// hardware_fingerprint so the server can validate the license and return
/// tier / expiry information.
///
/// Opting out of telemetry (`TUNE_TELEMETRY=false`) stops the ping — the
/// comment here used to claim the loop ran "ALWAYS regardless of
/// TUNE_TELEMETRY", and the code agreed with it: the opt-out cut nothing.
/// The account premium refresh keeps running either way, see [`heartbeat_plan`].
fn spawn_heartbeat(state: &AppState) {
    let backend = state.backend.clone();
    let services = state.services.clone();
    let outputs = state.outputs.clone();
    let started_at = state.started_at;
    let license = state.license.clone();
    let event_bus = state.event_bus.clone();
    tokio::spawn(async move {
        // Let startup finish before the first heartbeat
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;

        let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());
        let instance_id = match settings.get("instance_id").ok().flatten() {
            Some(id) if !id.is_empty() => id,
            _ => {
                let id = uuid::Uuid::new_v4().to_string();
                settings.set("instance_id", &id).ok();
                id
            }
        };

        // Identifiant de SERVEUR — distinct d'`instance_id`, et c'est le point.
        //
        // Le cloud lie une licence a un serveur via `License::claimSession`,
        // qui recoit `server_id` depuis cette charge utile. Le champ n'y etait
        // pas : la route le lisait, obtenait null, et n'ecrivait rien. Sur 72
        // licences premium, 53 n'avaient donc AUCUN serveur associe — alors
        // meme que ces machines battent toutes les cinq minutes et valident
        // leur licence correctement (`is_premium` est a jour).
        //
        // Consequence directe : le relais Tune Bridge, qui ne connait que le
        // `server_id`, ne peut pas verifier l'eligibilite premium de ces
        // serveurs. Sans ce champ, activer le controle en couperait 57 sur 72.
        //
        // `get_or_create_server_id` est le MEME accesseur que la telemetrie et
        // que le pont : les trois doivent parler du meme identifiant, sans
        // quoi le lien se ferait sur une valeur que personne d'autre ne
        // connait.
        let server_id =
            tune_core::cloud::telemetry::TelemetryReporter::get_or_create_server_id(&settings);

        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| gethostname().unwrap_or_else(|| "unknown".into()));

        let client = match tune_core::http::client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                debug!(error = %e, "heartbeat_client_build_failed");
                return;
            }
        };

        let registre = tune_core::db::task_run_repo::TaskRunRepo::with_backend(backend.clone());

        let mut echecs_consecutifs: u32 = 0;
        loop {
            let plan = plan_du_tour(&settings);

            // Registre des executions automatisees (#2080) : un cycle = une
            // ligne. Le battement est la seule passe dont l'echec est INVISIBLE
            // — rien ne change cote utilisateur quand mozaiklabs.fr ne repond
            // plus, et le `debug!` qui le signale n'est meme pas actif par
            // defaut. Un serveur premium dont la licence cesse d'etre validee
            // se decouvre aujourd'hui trente jours plus tard, a l'expiration de
            // la grace hors-ligne. Ici, ca se lit.
            //
            // Cadence horaire et retention a 50 : le registre garde environ
            // deux jours de battements. C'est le bon ordre de grandeur pour
            // « depuis quand ca ne passe plus », pas pour un historique long.
            let suivi = registre.ouvrir(tune_core::db::task_run_repo::TACHE_BATTEMENT_COEUR);

            // Marqueur de vivacite LOCAL, utilise pour la detection de crash au
            // demarrage suivant. Il ne quitte jamais la machine : il reste donc
            // ecrit meme quand la telemetrie est eteinte, sinon l'opt-out
            // ferait passer chaque arret propre pour un plantage.
            {
                let settings =
                    tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());
                let now_ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                settings
                    .set("server_last_alive_at", &now_ts.to_string())
                    .ok();
            }

            if !plan.send_heartbeat {
                // Opt-out : rien ne part sur le reseau, et la charge utile n'est
                // meme pas collectee. Les droits premium, eux, continuent d'etre
                // rafraichis — voir `heartbeat_plan`.
                if plan.refresh_account {
                    refresh_account_premium(&backend, &license, &services).await;
                }
                // La clé, elle, se revalide quand même : trois champs, rien de
                // descriptif (LIC-1). `None` quand aucune clé n'est enregistrée.
                let revalidation = if plan.revalidate_key {
                    revalider_la_cle(&client, &settings, &license, &event_bus, &server_id).await
                } else {
                    None
                };
                match revalidation {
                    Some((verdict, detail)) => {
                        echecs_consecutifs =
                            if verdict == tune_core::db::task_run_repo::Verdict::Echec {
                                echecs_consecutifs + 1
                            } else {
                                0
                            };
                        suivi.terminer(verdict, None, Some(&detail));
                    }
                    None => suivi.rien_a_faire(Some(
                        "telemetrie desactivee — marqueur local seulement, rien n'est parti",
                    )),
                }
                tokio::time::sleep(prochain_intervalle_battement(echecs_consecutifs)).await;
                continue;
            }

            let tracks = tune_core::db::track_repo::TrackRepo::with_backend(backend.clone())
                .count()
                .unwrap_or(0);
            let uptime_s = started_at.elapsed().as_secs();

            // Collect authenticated streaming services
            // Use try_lock to avoid blocking the heartbeat if another
            // task holds the services or outputs lock.
            let authenticated_services: Vec<String> = match services.try_lock() {
                Ok(registry) => {
                    let names = registry.list();
                    let svc_handles: Vec<_> = names
                        .iter()
                        .filter_map(|n| registry.get(n).map(|h| (n.clone(), h)))
                        .collect();
                    drop(registry);

                    let mut authed = Vec::new();
                    for (name, handle) in svc_handles {
                        // `try_read` et non `try_write` : on ne fait que
                        // LIRE l'etat d'authentification. Avec le RwLock, ce
                        // sondage cesse d'echouer parce qu'une autre lecture
                        // est en cours — il ne renonce plus que si une ecriture
                        // (rafraichissement de jeton, deconnexion) tient le
                        // verrou, ce qui est exactement l'intention (#1969).
                        if let Ok(svc) = handle.try_read() {
                            let status = svc.auth_status().await;
                            if status.authenticated {
                                authed.push(name);
                            }
                        }
                    }
                    authed
                }
                Err(_) => Vec::new(),
            };

            // Look up friendly names from zones DB
            let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(backend.clone());
            let zones_for_heartbeat = zone_repo.list().unwrap_or_default();
            let zone_names: std::collections::HashMap<String, String> = zones_for_heartbeat
                .iter()
                .filter_map(|z| z.output_device_id.clone().map(|did| (did, z.name.clone())))
                .collect();
            // Marque et modèle CORRIGÉS par l'utilisateur (éditeur « Appareil »).
            // Ils vivent dans les réglages sous `zone_{id}_brand` / `_model`, pas
            // dans une colonne de `zones` — d'où le fait qu'ils ne remontaient pas
            // ici : le heartbeat ne connaissait que la marque déduite du MAC.
            // Corriger une marque mal détectée n'avait donc aucun effet côté cloud.
            let zone_overrides: std::collections::HashMap<
                String,
                (Option<String>, Option<String>),
            > = {
                let settings =
                    tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());
                zones_for_heartbeat
                    .iter()
                    .filter_map(|z| {
                        let did = z.output_device_id.clone()?;
                        let id = z.id?;
                        let brand = settings.get(&format!("zone_{id}_brand")).ok().flatten();
                        let model = settings.get(&format!("zone_{id}_model")).ok().flatten();
                        if brand.is_none() && model.is_none() {
                            return None;
                        }
                        Some((did, (brand, model)))
                    })
                    .collect()
            };
            // Physical identity persisted on zones (Phase B): lets the
            // mozaiklabs admin identify renderer brands by MAC OUI instead
            // of guessing from device names.
            let zone_identities: std::collections::HashMap<String, (String, String)> = zone_repo
                .device_identities()
                .into_iter()
                .map(|(did, host, mac)| (did, (host, mac)))
                .collect();

            let devices: Vec<serde_json::Value> = match outputs.try_lock() {
                Ok(registry) => registry
                    .list()
                    .into_iter()
                    .map(|id| {
                        let dev_type = if id.starts_with("local:") {
                            "local"
                        } else if id.starts_with("airplay-") {
                            "airplay"
                        } else if id.starts_with("chromecast-") {
                            "chromecast"
                        } else if id.starts_with("oaat:") {
                            "oaat"
                        } else if id.starts_with("uuid:") {
                            "dlna"
                        } else {
                            "other"
                        };
                        let name = zone_names.get(&id).map(|n| n.as_str()).unwrap_or_else(|| {
                            id.strip_prefix("local:")
                                .or_else(|| id.strip_prefix("uuid:"))
                                .unwrap_or(&id)
                        });
                        let (mac, manufacturer) = zone_identities
                            .get(&id)
                            .map(|(_, mac)| {
                                (
                                    mac.clone(),
                                    tune_core::discovery::mac::vendor_for_mac(mac)
                                        .unwrap_or_default()
                                        .to_string(),
                                )
                            })
                            .unwrap_or_default();
                        // La correction de l'utilisateur prime sur la déduction OUI :
                        // c'est tout l'objet de l'éditeur « Appareil ». `model` n'était
                        // pas envoyé du tout.
                        let (user_brand, user_model) =
                            zone_overrides.get(&id).cloned().unwrap_or((None, None));
                        let manufacturer = user_brand
                            .filter(|b| !b.trim().is_empty())
                            .unwrap_or(manufacturer);
                        serde_json::json!({
                            "name": name,
                            "type": dev_type,
                            "mac": mac,
                            "manufacturer": manufacturer,
                            "model": user_model.filter(|m| !m.trim().is_empty()),
                        })
                    })
                    .collect(),
                Err(_) => Vec::new(),
            };

            // Include license info so the server can validate and respond
            // with the authoritative tier / expiry.
            let ls = license.license_state().await;

            // Grâce hors ligne (#1999) : tracer AVANT la coupure, pas après.
            // La dégradation ne se signalait que par un `warn!` au quatorzième
            // jour ; ici on écrit une ligne par heure dès que la revalidation
            // date de plus de deux jours, avec le compte à rebours. Jamais la
            // clé ni un identifiant d'achat — que des dates et des compteurs.
            if let Some(g) = tune_core::license::offline_grace(&ls) {
                match g.phase {
                    tune_core::license::GracePhase::Grace => info!(
                        source = ?g.source,
                        days_since_validation = g.days_since_validation,
                        days_remaining = g.days_remaining,
                        total_days = g.total_days,
                        "license_offline_grace_active (premium intact)"
                    ),
                    tune_core::license::GracePhase::Expired => info!(
                        source = ?g.source,
                        total_days = g.total_days,
                        "license_offline_grace_lapsed (premium suspended until next successful validation)"
                    ),
                    tune_core::license::GracePhase::Ok => {}
                }
            }

            let payload = serde_json::json!({
                "instance_id": instance_id,
                "version": tune_core::version(),
                "platform": std::env::consts::OS,
                "arch": std::env::consts::ARCH,
                "tracks": tracks,
                "uptime_s": uptime_s,
                "hostname": hostname,
                "services": authenticated_services,
                "devices": devices,
                "license_key": ls.license_key,
                "hardware_fingerprint": ls.hardware_fingerprint,
                // Sans lui, `claimSession` lie la licence a rien.
                "server_id": server_id,
            });

            // Verdict du cycle pour le registre. Le motif ne porte QUE le code
            // HTTP ou un mot : ni URL, ni cle de licence, ni empreinte
            // materielle — la charge utile en contient trois, le registre
            // aucune.
            let mut verdict = tune_core::db::task_run_repo::Verdict::Echec;
            let mut motif = String::from("hote injoignable");

            // CLD-2 : un seul chemin d'appel borné. La portée retenue ne part
            // pas ; la branche « retenu » est celle du pré-contrôle d'avant, au
            // même endroit dans le tour : rafraîchir le compte SSO si prévu
            // (les deux routes ont des compteurs distincts), dormir l'intervalle
            // plein, passer au tour suivant sans clore le suivi ni compter un
            // échec. La charge utile a été relevée pour rien : une fois par
            // heure, quelques lectures de base.
            match tune_core::cloud::rate_limit::appeler(
                &settings,
                tune_core::cloud::rate_limit::CloudScope::InstanceHeartbeat,
                client
                    .post("https://mozaiklabs.fr/api/v1/heartbeat")
                    .header("Accept", "application/json")
                    .json(&payload),
            )
            .await
            {
                tune_core::cloud::rate_limit::AppelCloud::Retenu(backoff) => {
                    debug!(
                        scope = backoff.scope,
                        until_epoch = backoff.until_epoch,
                        retry_after_seconds = backoff.retry_after_seconds,
                        "heartbeat_deferred_rate_limit"
                    );
                    if plan.refresh_account {
                        refresh_account_premium(&backend, &license, &services).await;
                    }
                    tokio::time::sleep(HEARTBEAT_INTERVAL).await;
                    continue;
                }
                tune_core::cloud::rate_limit::AppelCloud::Reponse(resp)
                    if resp.status().is_success() =>
                {
                    verdict = tune_core::db::task_run_repo::Verdict::Succes;
                    motif = format!("accepte ({})", resp.status().as_u16());
                    debug!(instance_id = %instance_id, tracks, uptime_s, "heartbeat_sent");

                    // Parse license validation data from the response body.
                    // The server may or may not include license fields — if
                    // absent (old server, 204, empty body, etc.) we keep the
                    // cached state unchanged.
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        // Floating-license single-session model: the cloud tells
                        // us when this key is currently held by ANOTHER server.
                        // Unlike a bare `license_valid:false` (which can be a
                        // transient re-binding and is softened by the offline
                        // grace), a session conflict is authoritative "not now":
                        // suppress premium here immediately, but keep the key and
                        // `last_validated` intact so premium snaps back once the
                        // other server stops pinging and the conflict clears.
                        let session_conflict = body
                            .get("session_conflict")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);

                        if session_conflict {
                            let active_server = body
                                .get("active_server")
                                .and_then(|v| v.as_str())
                                .map(String::from);
                            let active_since = body
                                .get("active_since")
                                .and_then(|v| v.as_str())
                                .map(String::from);
                            license
                                .set_session_conflict(active_server.clone(), active_since.clone())
                                .await;
                            warn!(
                                active_server = ?active_server,
                                "license_session_conflict_held_elsewhere"
                            );
                            event_bus.emit(
                                "license.session_conflict",
                                serde_json::json!({
                                    "active_server": active_server,
                                    "active_since": active_since,
                                }),
                            );
                        } else if body.get("license_tier").and_then(|v| v.as_str()).is_some() {
                            // No conflict reported → make sure any prior conflict
                            // is cleared before applying the normal verdict.
                            license.clear_session_conflict().await;

                            // Lecture unique du verdict, partagee avec les
                            // trois autres appelants (#3673, meme motif que le
                            // plafond de zones) : `tune_core::license::
                            // verdict_licence`. Une revocation autoritaire est
                            // une expiration *passee*, jamais un
                            // `license_valid:false` nu — celui-la peut etre
                            // transitoire (re-liaison d'empreinte, hoquet du
                            // serveur, cle tenue par une autre machine alors
                            // que le compte est toujours premium).
                            let verdict = tune_core::license::verdict_licence(&body);
                            let valid = matches!(
                                verdict,
                                tune_core::license::VerdictLicence::Confirmee { .. }
                            );
                            let expired_authoritatively =
                                matches!(verdict, tune_core::license::VerdictLicence::Expiree);
                            let sans_verdict =
                                matches!(verdict, tune_core::license::VerdictLicence::Absent);
                            let has_key = ls.license_key.is_some();

                            if sans_verdict {
                                // Le serveur ne s'est pas prononce : ne rien
                                // persister, ni accorder ni revoquer.
                                debug!("heartbeat_licence_sans_verdict_palier_conserve");
                            } else if !valid && has_key && !expired_authoritatively {
                                // Do NOT immediately strip a key-based Premium on
                                // a transient rejection: persisting Free here used
                                // to destroy the premium marker permanently. Keep
                                // the cached tier and, crucially, do NOT refresh
                                // `last_validated` — the 30-day offline grace then
                                // lapses on its own if the rejection persists, so a
                                // valid key survives a bad verdict while a genuinely
                                // revoked one still degrades. (JP #v0.9.9)
                                warn!(
                                    "license_key_rejected_by_server (keeping cached tier within grace)"
                                );
                            } else if !valid {
                                // No key present, or an authoritative expiry:
                                // honor the downgrade to Free.
                                info!(expired_authoritatively, "license_invalidated_by_server");
                                license
                                    .update_from_server(tune_core::license::Tier::Free, None)
                                    .await;
                                event_bus.emit(
                                    "license.updated",
                                    serde_json::json!({
                                        "tier": "free",
                                        "expires_at": null,
                                    }),
                                );
                            } else {
                                let tune_core::license::VerdictLicence::Confirmee {
                                    tier,
                                    expires_at,
                                } = verdict
                                else {
                                    unreachable!("les autres verdicts sont traites plus haut")
                                };

                                license.update_from_server(tier, expires_at.clone()).await;
                                info!(tier = %tier, "license_validated_from_heartbeat");
                                event_bus.emit(
                                    "license.updated",
                                    serde_json::json!({
                                        "tier": tier,
                                        "expires_at": expires_at,
                                    }),
                                );
                            }
                        }
                        // else: no license fields in response — keep cached state.
                    }
                }
                tune_core::cloud::rate_limit::AppelCloud::Reponse(resp) => {
                    debug!(status = %resp.status(), "heartbeat_rejected");
                    motif = format!("refuse ({})", resp.status().as_u16());
                }
                tune_core::cloud::rate_limit::AppelCloud::Erreur(e) => {
                    debug!(error = %e, "heartbeat_failed");
                }
            }

            echecs_consecutifs = if verdict == tune_core::db::task_run_repo::Verdict::Echec {
                echecs_consecutifs + 1
            } else {
                0
            };
            suivi.terminer(verdict, None, Some(&motif));

            // Refresh the account premium (SSO) from /api/v1/user so a lapsed
            // subscription is picked up without waiting for the offline grace or
            // a re-login. No-op when not connected. Never blocks the heartbeat.
            if plan.refresh_account {
                refresh_account_premium(&backend, &license, &services).await;
            }

            tokio::time::sleep(prochain_intervalle_battement(echecs_consecutifs)).await;
        }
    });
}

/// Re-fetch the mozaiklabs.fr account profile and update the account premium
/// (SSO). No-op if not connected (no access token). On an expired access token,
/// tries the refresh_token grant once and retries. On any network failure the
/// cached state is kept (the offline grace in `LicenseManager` covers it).
/// La charge d'une revalidation de clé, et rien d'autre : la clé, l'empreinte
/// matérielle et l'identifiant de serveur que la route attend. Aucune version,
/// aucun nom d'hôte, aucun compte de pistes — c'est ce qui la distingue du
/// battement, et ce qui autorise à l'envoyer quand la télémétrie est refusée.
fn charge_de_revalidation(
    license_key: &str,
    hardware_fingerprint: &str,
    server_id: &str,
) -> serde_json::Value {
    serde_json::json!({
        "license_key": license_key,
        "hardware_fingerprint": hardware_fingerprint,
        "server_id": server_id,
    })
}

/// Revalide la clé de licence sans battement, par
/// `POST /api/v1/license/validate` (la route que le bouton « Valider » du
/// panneau appelle déjà). Même lecture de la réponse que le battement : une
/// clé refusée hors expiration garde son palier dans la grâce, une expiration
/// autoritaire dégrade, une confirmation restampe `license_last_validated`.
///
/// `None` quand aucune clé n'est enregistrée (rien à revalider) ; sinon le
/// verdict et son motif pour le registre des tâches.
async fn revalider_la_cle(
    client: &reqwest::Client,
    settings: &tune_core::db::settings_repo::SettingsRepo,
    license: &Arc<tune_core::license::LicenseManager>,
    event_bus: &Arc<tune_core::event_bus::EventBus>,
    server_id: &str,
) -> Option<(tune_core::db::task_run_repo::Verdict, String)> {
    use tune_core::db::task_run_repo::Verdict;

    let ls = license.license_state().await;
    let key = ls.license_key.clone()?;

    let payload = charge_de_revalidation(&key, &ls.hardware_fingerprint, server_id);
    // CLD-2 : le même chemin borné que le battement, même portée.
    let resp = tune_core::cloud::rate_limit::appeler(
        settings,
        tune_core::cloud::rate_limit::CloudScope::InstanceHeartbeat,
        client
            .post(crate::routes::cloud::license_validate_url(settings))
            .header("Accept", "application/json")
            .json(&payload),
    )
    .await;

    match resp {
        tune_core::cloud::rate_limit::AppelCloud::Retenu(backoff) => {
            debug!(
                scope = backoff.scope,
                until_epoch = backoff.until_epoch,
                "license_revalidation_deferred_rate_limit"
            );
            Some((
                Verdict::Echec,
                "revalidation differee (429 en cours)".into(),
            ))
        }
        tune_core::cloud::rate_limit::AppelCloud::Reponse(resp) if resp.status().is_success() => {
            let Ok(body) = resp.json::<serde_json::Value>().await else {
                return Some((Verdict::Echec, "reponse illisible".into()));
            };
            // Meme lecture du verdict que le battement et que les deux routes.
            let (tier, expires_at) = match tune_core::license::verdict_licence(&body) {
                tune_core::license::VerdictLicence::Absent => {
                    warn!("license_revalidation_sans_verdict_palier_conserve");
                    return Some((Verdict::Echec, "aucun verdict rendu".into()));
                }
                tune_core::license::VerdictLicence::RefusTransitoire => {
                    warn!("license_key_rejected_by_server (keeping cached tier within grace)");
                    return Some((
                        Verdict::Echec,
                        "cle refusee, palier conserve dans la grace".into(),
                    ));
                }
                tune_core::license::VerdictLicence::Expiree => {
                    info!("license_invalidated_by_server (expiration autoritaire)");
                    license
                        .update_from_server(tune_core::license::Tier::Free, None)
                        .await;
                    event_bus.emit(
                        "license.updated",
                        serde_json::json!({ "tier": "free", "expires_at": null }),
                    );
                    return Some((Verdict::Succes, "licence expiree, palier gratuit".into()));
                }
                tune_core::license::VerdictLicence::Confirmee { tier, expires_at } => {
                    (tier, expires_at)
                }
            };
            license.update_from_server(tier, expires_at.clone()).await;
            info!(tier = %tier, "license_revalidated_without_heartbeat");
            event_bus.emit(
                "license.updated",
                serde_json::json!({ "tier": tier, "expires_at": expires_at }),
            );
            Some((Verdict::Succes, format!("cle revalidee, palier {tier}")))
        }
        tune_core::cloud::rate_limit::AppelCloud::Reponse(resp) => {
            debug!(status = %resp.status(), "license_revalidation_rejected");
            Some((
                Verdict::Echec,
                format!("refuse ({})", resp.status().as_u16()),
            ))
        }
        tune_core::cloud::rate_limit::AppelCloud::Erreur(e) => {
            debug!(error = %e, "license_revalidation_failed");
            Some((Verdict::Echec, "hote injoignable".into()))
        }
    }
}

async fn refresh_account_premium(
    backend: &Arc<dyn tune_core::db::backend::DbBackend>,
    license: &Arc<tune_core::license::LicenseManager>,
    services: &Arc<tokio::sync::Mutex<tune_core::streaming::ServiceRegistry>>,
) {
    use tune_core::cloud::sso::{DEFAULT_CLIENT_ID, MozaikAuth};

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());

    let token = match settings.get("mozaik_access_token").ok().flatten() {
        Some(t) if !t.is_empty() => t,
        _ => return, // not connected — nothing to refresh
    };

    let client_id = settings
        .get("mozaik_client_id")
        .ok()
        .flatten()
        .or_else(|| std::env::var("TUNE_MOZAIK_CLIENT_ID").ok())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_CLIENT_ID.to_string());
    if client_id.is_empty() {
        return;
    }
    let base_url = settings.get("mozaik_base_url").ok().flatten();
    let auth = MozaikAuth::new(client_id, base_url.as_deref());

    // Try the current token; if it fails (likely expired), refresh once & retry.
    let user = match auth.get_user(&token).await {
        Ok(u) => Some(u),
        Err(_) => {
            let refresh = settings
                .get("mozaik_refresh_token")
                .ok()
                .flatten()
                .filter(|s| !s.is_empty());
            match refresh {
                Some(rt) => match auth.refresh_token(&rt).await {
                    Ok(tok) => {
                        settings.set("mozaik_access_token", &tok.access_token).ok();
                        if let Some(ref new_rt) = tok.refresh_token {
                            settings.set("mozaik_refresh_token", new_rt).ok();
                        }
                        auth.get_user(&tok.access_token).await.ok()
                    }
                    Err(e) => {
                        debug!(error = %e, "mozaik_token_refresh_failed");
                        None
                    }
                },
                None => None,
            }
        }
    };

    if let Some(user) = user {
        settings
            .set(
                "mozaik_user",
                &serde_json::to_string(&user).unwrap_or_default(),
            )
            .ok();
        license
            .set_account_premium(user.premium, user.license_expires_at.clone())
            .await;
        // Propagate the Qobuz endpoint order (founder flag) so a change picked
        // up by a re-validation reaches the live QobuzService immediately.
        license.set_qobuz_proxy_first(user.qobuz_proxy_first).await;
        apply_qobuz_proxy_first(services, user.qobuz_proxy_first).await;
        // Paid-module entitlements (separate SKUs, e.g. the Diretta output)
        // travel with the account validation, like the premium flag above.
        license.set_modules(user.modules.clone()).await;
        debug!(premium = user.premium, "mozaik_account_premium_refreshed");
    }
}

/// Push the license-signalled Qobuz endpoint order into the live QobuzService
/// (same downcast pattern as `configure_deezer_proxy`). No-op if the service
/// isn't registered.
pub async fn apply_qobuz_proxy_first(
    services: &Arc<tokio::sync::Mutex<tune_core::streaming::ServiceRegistry>>,
    proxy_first: bool,
) {
    let registry = services.lock().await;
    if let Some(svc) = registry.get("qobuz") {
        let mut svc = svc.write().await;
        if let Some(qobuz) = svc
            .as_any_mut()
            .downcast_mut::<tune_core::streaming::qobuz::QobuzService>()
        {
            qobuz.set_proxy_first(proxy_first);
        }
    }
}

/// Resolve the machine hostname via the `hostname` command.
fn gethostname() -> Option<String> {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
}

/// La clé du réglage qui décide si Tune s'ANNONCE comme serveur Squeezebox.
///
/// Volontairement distincte de `squeezebox_enabled`, qui gouverne l'autre sens
/// du protocole — voir [`annonce_slimproto_activee`].
pub(crate) const CLE_ANNONCE_SLIMPROTO: &str = "slimproto_discovery_enabled";

/// Tune doit-il répondre aux recherches de serveurs Squeezebox du réseau ?
///
/// # #3809 — le réglage que le testeur cochait ne gouvernait pas ce chemin
///
/// Un testeur (fil `bug-bonjour-4s0m58`, v0.9.145) voit Home Assistant se
/// remplir de découvertes Squeezebox pointant sur son PC dès que Tune démarre,
/// et s'arrêter dès qu'il l'arrête. Il écrit : *« Que je coche ou pas la
/// découverte Squeezebox dans Tune ne change rien. »*
///
/// Il a raison, et le code le dit. L'unique interrupteur portant ce nom à
/// l'écran — `settings.squeezeboxEnabled`, « Activer la découverte
/// Squeezebox » — écrit `squeezebox_enabled`, dont les SEULS consommateurs sont
/// `spawn_squeezebox_poller` (ce fichier) et la page d'état
/// `routes/squeezebox.rs`. Tous deux gouvernent Tune **client** d'un Lyrion
/// Music Server : ils vont chercher les platines qu'un LMS déclare. Le répondeur
/// UDP du port 3483, lui — Tune **serveur**, celui que Home Assistant trouve —
/// n'était gouverné par rien du tout : `spawn_slimproto_server` armait
/// `discovery::spawn` à chaque démarrage, sans condition.
///
/// Les deux directions portent le même mot « découverte » et sont opposées.
/// C'est ce malentendu que ce réglage sépare.
///
/// # Pourquoi vrai par défaut, et pourquoi seulement l'annonce
///
/// Le port 3483 a DEUX volets : la connexion de contrôle en TCP, par laquelle
/// une platine Squeezebox pilote Tune, et le répondeur UDP qui permet à cette
/// platine de trouver Tune toute seule. #2938 et #2349 ont coûté cher sur ce
/// point précis — cinq testeurs dont le bind 3483 échouait, plus aucune platine
/// ne voyant Tune. Ce réglage ne touche donc QUE l'annonce : le TCP reste armé
/// quoi qu'il arrive, et la valeur par défaut laisse le comportement
/// exactement tel qu'il est aujourd'hui. Seul un `false` explicite fait taire
/// l'annonce.
pub(crate) fn annonce_slimproto_activee(valeur: Option<&str>) -> bool {
    !matches!(valeur.map(str::trim), Some("false") | Some("0"))
}

fn spawn_slimproto_server(state: &AppState, port_http: u16) {
    let local_ip = tune_core::discovery::ssdp::get_local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string());

    // Wire the server to app state so connected players register as zones and
    // can be driven for playback.
    let db = state.backend.clone();
    let event_bus = state.event_bus.clone();
    let outputs = state.outputs.clone();
    let server_ip = local_ip.clone();
    tokio::spawn(async move {
        let server = Arc::new(tune_core::slimproto::SlimProtoServer::new_with_state(
            db, event_bus, outputs, server_ip,
        ));
        if let Err(e) = server.spawn().await {
            error!(error = %e, "slimproto_server_failed");
        }
    });
    let cli_state = Arc::new(tune_core::slimproto::cli_server::CliState {
        players: tune_core::slimproto::new_player_registry(),
        server_name: "Tune".to_string(),
        server_version: tune_core::version().to_string(),
        local_ip,
    });
    tokio::spawn(tune_core::slimproto::cli_server::start_cli_server(
        cli_state,
    ));

    // Le volet UDP du port 3483 : sans lui, une Squeezebox ou un squeezelite
    // en decouverte automatique ne trouve jamais Tune — il fallait donner
    // l'adresse a la main. Le TCP seul est une porte sans sonnette.
    //
    // #3809 — mais une sonnette qui sonne chez le voisin. Sur un réseau qui
    // porte déjà un LMS, Home Assistant redécouvre Tune en boucle comme second
    // serveur Squeezebox. Ce chemin n'avait AUCUN interrupteur : celui que le
    // testeur cochait gouverne l'autre sens du protocole. Voir
    // `annonce_slimproto_activee` — l'annonce seule est gouvernée, jamais
    // l'écoute TCP armée plus haut (#2938, #2349).
    let annonce = annonce_slimproto_activee(
        tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
            .get(CLE_ANNONCE_SLIMPROTO)
            .ok()
            .flatten()
            .as_deref(),
    );
    if !annonce {
        info!(
            reglage = CLE_ANNONCE_SLIMPROTO,
            "slimproto_annonce_desactivee — Tune ne repond plus aux recherches              de serveurs Squeezebox ; l'ecoute TCP 3483 reste armee"
        );
        return;
    }
    tune_core::slimproto::discovery::spawn(tune_core::slimproto::discovery::IdentiteServeur {
        nom: "Tune".to_string(),
        port_http,
        port_cli: 9090,
        version: tune_core::version().to_string(),
    });
}

/// Le délai avant la reprise des favoris de service : le démarrage a mieux à
/// faire, et les jetons des services sont restaurés puis rafraîchis dans les
/// premières secondes. 90 s, comme la veille Bandcamp, pour la même raison.
const REPRISE_FAVORIS_DELAI_SECS: u64 = 90;

/// Reprend UNE fois, au démarrage, les favoris posés chez Qobuz/Tidal/… dans
/// la table de Tune (#3419).
///
/// # Pourquoi un appelant, et pas seulement une route
///
/// Sans passage automatique, la reprise ne serait qu'une route que personne
/// n'appelle : les clients déjà publiés ne la connaissent pas, et le défaut —
/// une règle « Favori · est · Piste » qui rend 0 — resterait entier jusqu'à ce
/// qu'un client soit mis à jour. L'issue le propose explicitement :
/// « une route de synchronisation explicite, appelable au démarrage ou depuis
/// les Réglages ».
///
/// # Sur quel profil
///
/// Sur `active_profile_id` — le modèle mono-actif que `/profiles/switch` et
/// l'orchestrateur (marquage de l'historique) utilisent déjà —, à défaut le
/// profil 1. Au démarrage il n'y a aucune requête, donc aucun `X-Profile-Id` :
/// c'est la seule identité que la machine possède à cet instant. Les autres
/// profils d'un foyer se reprennent par la route, qui agit sur l'appelant.
///
/// # Une passe, pas une veille
///
/// Aucune répétition : la reprise n'ajoute jamais qu'au premier passage, et la
/// question « faut-il resynchroniser périodiquement, et que faire d'un favori
/// retiré chez le service » demande un arbitrage que ce correctif ne tranche
/// pas (voir `tune_core::streaming::favorites_import`).
fn spawn_reprise_favoris_streaming(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(REPRISE_FAVORIS_DELAI_SECS)).await;
        let profil =
            tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
                .get("active_profile_id")
                .ok()
                .flatten()
                .and_then(|v| v.trim().parse::<i64>().ok())
                .filter(|&id| id > 0)
                .unwrap_or(1);
        let comptes = crate::routes::profiles::reprendre_les_favoris(&state, profil, None).await;
        let services = comptes.as_object().map(|o| o.len()).unwrap_or(0);
        if services > 0 {
            info!(
                profile_id = profil,
                services,
                comptes = %comptes,
                "reprise_favoris_streaming_au_demarrage"
            );
        }
    });
}

fn spawn_bio_sync(state: &AppState) {
    let license = state.license.clone();
    let db = state.backend.clone();
    let rx = state.event_bus.subscribe();
    tokio::spawn(async move {
        // Wait for startup to settle before checking license
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        if !license
            .check_feature(tune_core::license::Feature::AutoEnrichment)
            .await
        {
            info!("bio_sync_auto_download_requires_premium — upload-only mode");
            // Still upload local bios (community contribution) but skip auto download
            let db_upload = db.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_secs(55)).await;
                loop {
                    tune_core::cloud::bio_sync::upload_bios(&db_upload).await;
                    tokio::time::sleep(std::time::Duration::from_secs(86400)).await;
                }
            });
            return;
        }
        tune_core::cloud::bio_sync::spawn(db, rx);
    });
}

/// Delais avant de retenter le rattrapage des logos, quand l'annuaire est
/// injoignable. Un serveur d'appliance demarre AVANT que le reseau soit la —
/// la box negocie, le Wi-Fi s'associe, le VPN monte. La passe unique de
/// demarrage tombait alors dans le vide et il fallait redemarrer pour
/// retenter : aucune vignette de station jusque-la (#2421).
///
/// On s'arrete des que l'annuaire repond, meme s'il ne connait pas toutes les
/// stations : une station absente de l'annuaire ne s'y trouvera pas davantage
/// au dixieme essai.
const RATTRAPAGE_LOGOS_DELAIS_SECS: [u64; 3] = [30, 120, 600];

/// Best-effort, at boot: fill in missing station logos from the mozaiklabs.fr
/// radio directory so the seeded default stations show a vignette instead of
/// the placeholder mic (Pascal). Cloud-graceful — a no-op offline.
fn spawn_radio_logo_refresh(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        let mut delais = RATTRAPAGE_LOGOS_DELAIS_SECS.iter();
        loop {
            let bilan = crate::routes::radios::refresh_radio_logos(&state).await;
            // Trace INCONDITIONNELLE. Elle ne s'ecrivait que si des logos
            // avaient ete poses — c'est-a-dire jamais dans les deux cas ou
            // elle aurait servi : annuaire injoignable, et stations absentes
            // de l'annuaire. Le journal ne permettait donc pas de separer
            // « je n'ai pas trouve » de « je n'ai pas pu chercher ».
            tracing::info!(
                updated = bilan.updated,
                sans_logo = bilan.sans_logo,
                annuaire_injoignable = bilan.annuaire_injoignable,
                "radio_logos_backfilled_at_startup"
            );
            if !bilan.annuaire_injoignable {
                return;
            }
            let Some(secs) = delais.next() else {
                tracing::warn!("radio_logos_directory_unreachable_giving_up");
                return;
            };
            tokio::time::sleep(std::time::Duration::from_secs(*secs)).await;
        }
    });
}

fn spawn_community_sync(state: &AppState) {
    tune_core::cloud::community_sync::spawn(state.backend.clone());
}

/// Background ReplayGain analysis: fills `rg_track_gain`/`rg_track_peak` (+ album)
/// for local tracks whose files carry no ReplayGain tags, by measuring EBU R128
/// loudness. Throttled and fully separate from the scan (which stays tag-only) so
/// it never slows indexing. Gated by the `replaygain_analysis_enabled` setting.
fn spawn_replaygain_analysis(state: &AppState) {
    tune_core::audio::replaygain::spawn(state.backend.clone());
}

/// #2172 — le rattrapage des paroles.
///
/// Le titre de l'issue disait « aucun passage de fond ne récupère les
/// paroles » : les deux passes de `library::lyrics_pass` existaient depuis la
/// 0.9.118, mais leur SEUL appelant était `POST /library/lyrics/fetch`, un
/// bouton. Cette ligne est ce qui manquait — sans elle, le cœur reste du code
/// que rien n'atteint, exactement comme `spawn_scan_scheduler` avant #2469.
///
/// Ne fait rien tant que `lyrics_lrclib_enabled` n'est pas activé, s'efface
/// devant toute zone qui joue, et ne demande jamais plus de `LOT_DE_FOND`
/// paroles d'affilée.
fn spawn_lyrics_catchup(state: &AppState) {
    tune_core::library::lyrics_pass::spawn(state.backend.clone(), state.http_client.clone());
}

/// Background CLAP audio-embedding sweep for the acoustic Smart Radio. Opt-in
/// build (feature-gated) AND opt-in at runtime (`audio_embedding_enabled`); the
/// loop no-ops cheaply until enabled and a model is present.
#[cfg(feature = "audio-embedding")]
fn spawn_audio_embedding(state: &AppState) {
    tune_core::audio::embedding::spawn(state.backend.clone(), state.license.clone());
}

fn spawn_cloud_library_sync(state: &AppState) {
    tune_core::cloud::library_sync::spawn(state.backend.clone(), state.license.clone());
    // L'arbitrage part apres la synchronisation : demander des propositions
    // sur un catalogue pas encore pousse au cloud ne donnerait rien.
    tune_core::cloud::metadata_proposals::spawn(state.backend.clone(), state.license.clone());
}

/// Relevé mémoire périodique — et ce qu'il faut pour NOMMER ce qui grossit.
///
/// Cette trace existait déjà et tournait toutes les cinq minutes sous Linux :
/// elle disait `rss_mb` et le nombre de sorties. C'est-à-dire qu'elle
/// constatait la croissance sans jamais donner de quoi l'imputer.
///
/// JeromeQ (#2077) est passé de 117 Mo à 1,8 Go en trente-sept minutes de
/// lecture sur Ubuntu — donc cette trace tournait chez lui, et elle n'aurait
/// rien appris de plus que ses captures de `smem`.
///
/// On y ajoute les deux compteurs qui séparent les hypothèses que le ticket
/// n'a pas pu départager :
///
/// - `stream_sessions` : une session est créée par piste et n'est ramassée
///   qu'après **trente minutes SANS un octet servi** (`cleanup_stale_sessions`).
///   Le critère était l'âge absolu depuis la création ; il est désormais
///   l'inactivité, doublée d'un plafond absolu de vingt-quatre heures en
///   filet (#2536). Chacune tient
///   un canal de 128 morceaux. Un compteur qui monte avec la lecture et
///   redescend au repos désigne ce cache ; un compteur plat innocente le
///   chemin de lecture, et c'est aussi une réponse.
/// - `rss_delta_mb` : la croissance depuis le démarrage du serveur. Un seul
///   relevé ne dit rien ; c'est l'écart qui parle, et le lire dans la ligne
///   évite d'avoir à retrouver la première.
///
/// Ce n'est PAS un correctif. Le ticket demande explicitement trois mesures du
/// testeur avant de coder, parce qu'une fuite de lecture et une fuite de tâche
/// de fond n'ont pas le même correctif. Ceci rend le prochain relevé
/// exploitable, rien de plus.
fn spawn_memory_diagnostics(
    outputs: Arc<tokio::sync::Mutex<OutputRegistry>>,
    streamer: Arc<tune_core::http::streamer::AudioStreamer>,
) {
    tokio::spawn(async move {
        // Le relevé lui-même n'existe que sur Linux (`/proc/self/statm`) : la
        // base de comparaison suit la même portée, sinon elle est déclarée et
        // jamais lue ailleurs. `rss_delta_mb` reste donc mesuré partout où la
        // trace tourne — c'est-à-dire sur Linux, et nulle part ailleurs.
        #[cfg(target_os = "linux")]
        let mut rss_initial_mb: Option<u64> = None;
        loop {
            #[cfg(target_os = "linux")]
            if let Ok(statm) = tokio::fs::read_to_string("/proc/self/statm").await {
                let rss_pages: u64 = statm
                    .split_whitespace()
                    .nth(1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let rss_mb = rss_pages * 4 / 1024;
                let count = outputs.lock().await.list().len();
                let stream_sessions = streamer.sessions_state().lock().await.len();
                let base = *rss_initial_mb.get_or_insert(rss_mb);
                // Signé : un relevé sous la valeur de départ est une information
                // (mémoire rendue), pas un débordement à cacher.
                let rss_delta_mb = rss_mb as i64 - base as i64;
                info!(
                    rss_mb,
                    rss_delta_mb,
                    outputs_count = count,
                    stream_sessions,
                    "memory_diagnostics"
                );
            }
            let _ = (&outputs, &streamer); // keep alive on non-linux
            tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        }
    });
}

/// Periodically re-enumerate local audio devices to detect USB DACs that were
/// plugged in after startup or took time to initialize.
///
/// Cadence is platform-dependent. On Windows/macOS we poll every 120s so a
/// freshly-plugged USB DAC appears quickly. On Linux, PipeWire already handles
/// device hotplug dynamically and each cpal re-enumeration re-probes *every*
/// PipeWire/ALSA node; doing that every 2 minutes for hours has been linked to
/// runaway pipewire/wireplumber memory growth → OOM (JeromeQ, #1257, Ubuntu
/// 24.04, 8GB). So poll far less often there — a USB DAC is merely detected a
/// little later.
#[cfg(target_os = "linux")]
const LOCAL_AUDIO_RESCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);
#[cfg(not(target_os = "linux"))]
const LOCAL_AUDIO_RESCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(120);

#[cfg(feature = "local-audio")]
fn spawn_local_audio_rescan(state: &AppState) {
    let state = state.clone();
    tokio::spawn(async move {
        // Initial delay to avoid conflicting with startup registration
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        loop {
            rescan_local_audio_devices(&state).await;
            tokio::time::sleep(LOCAL_AUDIO_RESCAN_INTERVAL).await;
        }
    });
}

#[cfg(not(feature = "local-audio"))]
fn spawn_local_audio_rescan(_state: &AppState) {}

/// Whether any registered local (`local:`) output is currently playing. Used to
/// suppress device enumeration (which probes formats and can crash the active
/// WASAPI stream on Windows — DEvir) while local playback is in progress.
#[cfg(feature = "local-audio")]
pub async fn any_local_output_playing(state: &AppState) -> bool {
    let outputs = state.outputs.lock().await;
    for id in outputs.list() {
        if !id.starts_with("local:") {
            continue;
        }
        if let Some(output) = outputs.get(&id) {
            let output = output.lock().await;
            if let Ok(status) = output.get_status().await {
                if status.state == tune_core::outputs::traits::TransportState::Playing {
                    return true;
                }
            }
        }
    }
    false
}

/// Whether ANY zone (local or network) is currently playing, read from the
/// in-memory playback state — no device or network probing. Used on Linux to
/// suppress the periodic audio re-enumeration during a listening session:
/// re-probing PipeWire while music plays for hours drives runaway pipewire
/// memory growth (JeromeQ, #1257). Network renderers don't use PipeWire, but a
/// long radio session to one is exactly when the useless re-probing piles up.
#[cfg(all(feature = "local-audio", target_os = "linux"))]
async fn any_zone_playing(state: &AppState) -> bool {
    state
        .playback
        .all_states()
        .await
        .iter()
        .any(|z| z.state == tune_core::playback::PlayState::Playing)
}

/// Re-enumerate local audio devices and register any new ones.
/// Removes devices that have disappeared (unless actively playing).
#[cfg(feature = "local-audio")]
pub async fn rescan_local_audio_devices(state: &AppState) {
    // On Windows, use WASAPI for the periodic rescan instead of re-probing
    // ASIO every cycle.  Re-probing ASIO can crash the process when the ASIO
    // driver is in a bad state (e.g. SOtM Diretta via RDP — the ASIO SDK
    // calls abort() internally, killing the process with no panic/error).
    // ASIO devices are detected at startup; the hotplug rescan only needs to
    // track WASAPI device changes (USB DACs plugged/unplugged).
    let configured_backend = state.effective_audio_backend();
    // When ASIO is configured, force WASAPI for the periodic rescan
    // (re-probing ASIO during playback can crash the driver).
    // ASIO devices were registered at startup and won't change.
    let scan_backend = if configured_backend.eq_ignore_ascii_case("asio") {
        "wasapi".to_string()
    } else {
        configured_backend.clone()
    };
    let is_asio_configured = configured_backend.eq_ignore_ascii_case("asio");

    // Do NOT re-enumerate audio devices while a local output is actively
    // playing. On Windows the WASAPI enumeration probes each device's supported
    // formats, which can invalidate the active render stream and kill playback:
    // refreshing the UI during local playback triggered a hotplug rescan whose
    // enumeration crashed the active fallback stream (audio_stream_error: "The
    // requested device is no longer available") → 10s decoder timeout → total
    // stop (DEvir, Win11 WASAPI fallback). Hotplug detection resumes on the next
    // cycle once playback stops. This also protects any active ASIO output.
    if any_local_output_playing(state).await {
        debug!("local_audio_rescan_skipped_active_playback");
        return;
    }

    // On Linux, also skip while ANY zone is playing (even a network renderer).
    // Re-probing PipeWire every cycle during a long listening session is the
    // exact pattern that grows pipewire/wireplumber memory unbounded → OOM
    // (JeromeQ, #1257, radio to a Volumio zone). A USB DAC plugged in mid-
    // session is simply picked up on the next idle cycle.
    #[cfg(target_os = "linux")]
    if any_zone_playing(state).await {
        debug!("local_audio_rescan_skipped_zone_playing");
        return;
    }

    let backend_clone = scan_backend.clone();
    let devices = match tokio::task::spawn_blocking(move || {
        tune_core::outputs::local::list_audio_devices_with_backend(&backend_clone)
    })
    .await
    {
        Ok(d) => d,
        Err(_) => return,
    };

    {
        let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
        // #2269 — l'identité des sorties locales, AVANT d'enregistrer ou de
        // créer quoi que ce soit.
        //
        // Une zone locale est identifiée par `local:{nom}`. Quand le pilote
        // renomme l'endpoint — Windows le fait au changement de taux
        // d'échantillonnage — la boucle ci-dessous ne reconnaît plus
        // `local:{nouveau nom}` et offre à l'appareil une zone NEUVE, à côté
        // de l'ancienne restée orpheline avec tous ses réglages. Cette passe
        // fait suivre la zone à son appareil, par l'identifiant d'endpoint
        // stable qu'elle a enregistré.
        //
        // La RÈGLE est ailleurs — `outputs::identite_de_sortie`, une fonction
        // pure : liste BLANCHE de backends (WASAPI et CoreAudio seulement, cf.
        // sa table), quatre refus nommés, aucune fusion de zones. Ici on ne
        // fait que lui donner le parc et journaliser ce qu'elle a décidé.
        //
        // ⚠️ Elle ne fait pas revenir un appareil DÉBRANCHÉ : un périphérique
        // absent de `devices` reste introuvable, identifiant ou pas.
        let parc_pour_identite: Vec<tune_core::outputs::identite_de_sortie::SortieEnumeree> =
            devices
                .iter()
                .map(
                    |dev| tune_core::outputs::identite_de_sortie::SortieEnumeree {
                        nom: dev.name.clone(),
                        endpoint_id: dev.endpoint_id.clone(),
                    },
                )
                .collect();
        match zone_repo.appliquer_identite_de_sortie(&parc_pour_identite) {
            Ok(rapport) => {
                for r in &rapport.reassociees {
                    info!(
                        zone_id = r.zone_id,
                        ancien = %r.ancien_device_id,
                        nouveau = %r.nouveau_device_id,
                        endpoint_id = %r.endpoint_id,
                        "zone_locale_reassociee_par_identifiant_stable"
                    );
                }
                // Les refus sont DITS. Une zone qui ne retrouve pas son
                // appareil alors qu'elle en connaît l'identifiant est
                // exactement ce qu'un rapport de bogue doit pouvoir nommer.
                for (zone_id, motif) in &rapport.refus {
                    warn!(
                        zone_id,
                        motif = %motif,
                        "reassociation_de_zone_locale_refusee"
                    );
                }
                if !rapport.apprises.is_empty() {
                    info!(
                        zones = rapport.apprises.len(),
                        "identifiant_de_sortie_locale_appris"
                    );
                }
                // Les télécommandes sont connectées, elles : une zone qui
                // change d'appareil doit se rafraîchir à l'écran, sans quoi
                // elles gardent l'ancien identifiant jusqu'au prochain
                // rechargement complet.
                for r in &rapport.reassociees {
                    state.event_bus.emit_typed(
                        tune_core::event_types::EventType::ZoneUpdated,
                        serde_json::json!({
                            "zone_id": r.zone_id,
                            "device_id": r.nouveau_device_id,
                        }),
                    );
                }
            }
            Err(e) => warn!(error = %e, "identite_de_sortie_locale_non_appliquee"),
        }
    }
    // Collect new device IDs first (no lock needed)
    let new_device_ids: std::collections::HashSet<String> = devices
        .iter()
        .map(|dev| format!("local:{}", dev.name))
        .collect();

    // Phase 1: Register new devices and remove stale ones (hold lock briefly)
    let mut new_devices_to_zone: Vec<(String, String, bool)> = Vec::new();
    let mut removed_device_ids: Vec<String> = Vec::new();
    {
        let mut outputs = state.outputs.lock().await;
        let existing_ids: std::collections::HashSet<String> = outputs
            .list()
            .into_iter()
            .filter(|id| id.starts_with("local:"))
            .collect();

        let mut registered_count = 0;

        for dev in &devices {
            let device_id = format!("local:{}", dev.name);

            // Already registered — still ensure a zone exists (may have been deleted)
            if existing_ids.contains(&device_id) || outputs.contains(&device_id) {
                new_devices_to_zone.push((device_id, dev.name.clone(), dev.is_default));
                continue;
            }

            // New device found — register it
            // L'hôte qui a énuméré ce nom voyage avec lui (#3230).
            //
            // #3245 — et le mode exclusif se décide AVEC lui. Ce chemin-ci est
            // celui qui rendait le débordement certain : quand ASIO est
            // configuré, ce rescan FORCE l'énumération WASAPI (voir
            // `scan_backend` plus haut), donc tous les noms qu'il enregistre
            // sont des noms WASAPI. `state.effective_exclusive_mode()` valait
            // pourtant `true`, imposé par ASIO, et `LocalOutput` ouvrait ces
            // sorties en WASAPI EXCLUSIF — le son des autres applications de la
            // machine disparaissait sur un périphérique que personne n'avait
            // demandé en exclusif (jfpaquet, Asus Essence STX II).
            let statut_exclusif = tune_core::config::local_exclusive_mode_du_peripherique(
                &configured_backend,
                Some(dev.backend.as_str()),
                state.requested_exclusive_mode(),
            );
            let local_out = tune_core::outputs::local::LocalOutput::with_options_and_endpoint(
                dev.name.clone(),
                (!dev.endpoint_id.is_empty()).then(|| dev.endpoint_id.clone()),
                statut_exclusif.effective,
                &configured_backend,
            )
            .with_origin_host(&dev.backend);
            outputs.register(Box::new(local_out));
            registered_count += 1;

            info!(
                name = %dev.name,
                device_id = %device_id,
                default = dev.is_default,
                channels = dev.max_channels,
                "local_audio_hotplug_detected"
            );

            new_devices_to_zone.push((device_id, dev.name.clone(), dev.is_default));
        }

        // Remove WASAPI devices that have disappeared (USB DAC unplugged),
        // but only if not actively playing.
        // On Windows, only remove devices that were found in the current
        // scan backend (WASAPI). Devices registered by ASIO at startup
        // won't appear in WASAPI scans — don't remove them, as dropping
        // ASIO outputs can crash the process via the driver FFI.
        for old_id in &existing_ids {
            if new_device_ids.contains(old_id) {
                continue;
            }

            // Only remove if the device name matches one we could have
            // discovered with the current scan backend.  If the scan used
            // WASAPI but this device was registered by ASIO at startup,
            // it won't be in new_device_ids but we must NOT remove it.
            // If the scan returned nothing, skip all removals — an empty
            // result means the backend couldn't enumerate (e.g. WASAPI held
            // exclusively by foobar2000), not that everything disappeared.
            if devices.is_empty() {
                debug!("local_audio_rescan_empty_skipping_all_removals");
                break;
            }
            let old_name = old_id.strip_prefix("local:").unwrap_or(old_id);
            let was_in_scan_scope = devices.iter().any(|d| d.name == old_name);
            if !was_in_scan_scope {
                debug!(device_id = %old_id, "local_audio_skipping_removal_different_backend");
                continue;
            }

            let is_playing = if let Some(output) = outputs.get(old_id) {
                let output = output.lock().await;
                match output.get_status().await {
                    Ok(status) => {
                        status.state == tune_core::outputs::traits::TransportState::Playing
                    }
                    Err(_) => false,
                }
            } else {
                false
            };

            if !is_playing {
                outputs.remove(old_id);
                info!(device_id = %old_id, "local_audio_device_removed");
                removed_device_ids.push(old_id.clone());
            }
        }

        if registered_count > 0 {
            info!(
                new_devices = registered_count,
                total = devices.len(),
                "local_audio_rescan_complete"
            );
        }
    } // outputs lock released here

    // Phase 2a: Mark zones of unplugged devices offline and tell the clients
    // (no lock held). Without this the zone stayed listed as playable after a
    // USB DAC unplug even though its output was gone (#1626). The zone itself
    // is kept: when the DAC comes back, the re-registration path below flips
    // it online again — automatic recovery.
    if !removed_device_ids.is_empty() {
        let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
        for device_id in &removed_device_ids {
            let _ = zone_repo.set_online_by_device(device_id, false);
            state.event_bus.emit_typed(
                tune_core::event_types::EventType::ZoneUpdated,
                serde_json::json!({ "device_id": device_id, "online": false }),
            );
            info!(device_id = %device_id, "local_audio_zone_set_offline");
        }
    }

    // Phase 2: Create zones and emit events (no lock held)
    if !new_devices_to_zone.is_empty() {
        let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
        // #3529 — même lecture du réglage que partout ailleurs, mais elle
        // n'est plus recopiée : `ZoneRepo` la porte une fois pour toutes.
        let auto_create = zone_repo.zone_auto_create_autorise();
        let system_default_device_id = crate::startup::first_system_default_name(
            new_devices_to_zone
                .iter()
                .map(|(device_id, _, is_default)| (device_id.as_str(), *is_default)),
        )
        .map(str::to_owned);

        for (device_id, dev_name, is_default) in &new_devices_to_zone {
            // When ASIO is configured, don't create new zones for WASAPI
            // devices discovered by the fallback rescan. Users should only
            // see ASIO zones (e.g. "HoloAudio ASIO Driver"), not confusing
            // generic WASAPI names like "Haut-parleurs" for the same DAC.
            // Only update online status for WASAPI zones that already exist.
            if is_asio_configured {
                continue;
            }
            // #1770 : même règle qu'au démarrage — l'étiquette générique ne se
            // minte qu'une fois. Un DAC branché à chaud qui devient la sortie
            // système ne doit pas produire un second « This Computer » à côté
            // de celui que porte déjà une autre sortie locale.
            let generique_deja_pris = zone_repo
                .etiquette_generique_locale_prise(device_id)
                .unwrap_or(false);
            let zone_name =
                tune_core::config::nom_de_zone_locale(dev_name, *is_default, generique_deja_pris);

            // #1770 : le rescan peut créer au plus UNE zone, celle de la sortie
            // système. Les autres sorties restent enregistrées pour que
            // l'interface puisse les proposer à la création manuelle. Une zone
            // déjà connue est reconnectée normalement.
            let zone_exists = zone_repo
                .get_by_device_id(device_id)
                .ok()
                .flatten()
                .is_some();
            let is_system_default = system_default_device_id.as_deref() == Some(device_id.as_str());
            let action =
                crate::startup::local_zone_action(zone_exists, auto_create, is_system_default);
            if action == crate::startup::LocalZoneAction::Skip {
                info!(
                    name = %zone_name,
                    device_id = %device_id,
                    "local_audio_hotplug_zone_manual_creation_required"
                );
                state.event_bus.emit(
                    "device.discovered",
                    serde_json::json!({
                        "id": device_id,
                        "name": dev_name,
                        "type": "local",
                        "hotplug": true,
                        "zone_creation": "manual",
                    }),
                );
                continue;
            }

            match zone_repo.get_or_create(&zone_name, Some("local"), device_id) {
                Ok((zid, true)) => {
                    info!(
                        name = %zone_name,
                        zone_id = zid,
                        device_id = %device_id,
                        "local_audio_hotplug_zone_created"
                    );
                }
                Ok((zid, false)) => {
                    let _ = zone_repo.set_online_by_device(device_id, true);
                    debug!(zone_id = zid, device_id = %device_id, "local_audio_zone_set_online");
                    // Même soin d'étiquette générique qu'au démarrage (#1233).
                    if !is_default
                        && let Ok(n) = zone_repo.rename_generic_local_label(zid, dev_name)
                        && n > 0
                    {
                        info!(zone_id = zid, name = %dev_name, "local_zone_generic_label_healed");
                    }
                }
                Err(e) => {
                    tracing::warn!(name = %zone_name, device_id = %device_id, error = %e, "local_audio_hotplug_zone_create_failed");
                }
            }

            // Emit event for UI refresh
            state.event_bus.emit(
                "device.discovered",
                serde_json::json!({
                    "id": device_id,
                    "name": dev_name,
                    "type": "local",
                    "hotplug": true,
                }),
            );
        }
    }
}

fn spawn_social_sharing_listener(state: &AppState) {
    let license = state.license.clone();
    let backend = state.backend.clone();
    let mut rx = state.playback.subscribe();
    let http_client = state.http_client.clone();

    tokio::spawn(async move {
        loop {
            let event = match rx.recv().await {
                Ok(e) => e,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    debug!(skipped = n, "social_sharing_listener_lagged");
                    continue;
                }
                Err(_) => break,
            };

            // Only react to track-start events
            if event.event != "started" {
                continue;
            }

            // Premium gate
            if !license
                .check_feature(tune_core::license::Feature::SocialSharing)
                .await
            {
                continue;
            }

            // Check sharing profile
            let profile = tune_core::social::load_profile(&backend);
            if !profile.enabled || !profile.share_now_playing {
                continue;
            }

            // Build the card from event data
            let title = event
                .data
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let artist = event
                .data
                .get("artist_name")
                .and_then(|v| v.as_str())
                .map(String::from);
            let album = event
                .data
                .get("album_title")
                .and_then(|v| v.as_str())
                .map(String::from);
            let cover = event
                .data
                .get("cover_path")
                .and_then(|v| v.as_str())
                .map(String::from);
            let source = event
                .data
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or("local")
                .to_string();

            if title.is_empty() {
                continue;
            }

            let card = tune_core::social::NowListeningCard {
                title,
                artist,
                album,
                cover_url: cover,
                format: None,
                sample_rate: None,
                bit_depth: None,
                source,
                shared_at: time::OffsetDateTime::now_utc()
                    .format(&time::format_description::well_known::Rfc3339)
                    .unwrap_or_default(),
            };

            let payload = serde_json::json!({
                "display_name": profile.display_name,
                "now_listening": card,
            });

            let client = http_client.clone();
            tokio::spawn(async move {
                match client
                    .post("https://mozaiklabs.fr/api/v1/community/now-listening")
                    .json(&payload)
                    .send()
                    .await
                {
                    Ok(resp) if resp.status().is_success() => {
                        debug!("social_auto_share_ok");
                    }
                    Ok(resp) => {
                        debug!(
                            status = resp.status().as_u16(),
                            "social_auto_share_upstream_error"
                        );
                    }
                    Err(e) => {
                        debug!(error = %e, "social_auto_share_failed");
                    }
                }
            });
        }
    });
}

#[cfg(test)]
mod heartbeat_cadence_et_optout_tests {
    use super::{HEARTBEAT_INTERVAL, heartbeat_plan};

    /// Fait constate sur le journal d'un testeur (#2416) : le battement partait
    /// toutes les 300 s, et il partait meme quand la telemetrie etait eteinte.
    /// La grace hors-ligne est de 14 jours (`tune-core/src/license.rs`,
    /// `GRACE_PERIOD_DAYS`) : une cadence horaire est largement suffisante pour
    /// tenir les droits a jour. 5 minutes etait une cadence d'outil
    /// d'administration temps reel, pas un besoin du produit.
    #[test]
    fn la_cadence_du_battement_est_horaire() {
        assert_eq!(
            HEARTBEAT_INTERVAL.as_secs(),
            3600,
            "le battement doit battre une fois par heure, pas toutes les 5 minutes"
        );
    }

    /// `TUNE_TELEMETRY=false` doit couper l'ENVOI du battement. Le commentaire
    /// du code affirmait l'inverse (« runs ALWAYS regardless of TUNE_TELEMETRY »)
    /// et le code lui donnait raison : l'opt-out ne coupait rien.
    #[test]
    fn le_battement_ne_part_pas_quand_la_telemetrie_est_eteinte() {
        assert!(
            !heartbeat_plan(false).send_heartbeat,
            "TUNE_TELEMETRY=false doit couper l'envoi du battement"
        );
    }

    /// L'opt-out ne doit pas devenir un interrupteur general : telemetrie
    /// active, le battement part comme avant.
    #[test]
    fn le_battement_part_quand_la_telemetrie_est_active() {
        assert!(
            heartbeat_plan(true).send_heartbeat,
            "telemetrie active, le battement doit partir"
        );
    }

    /// Decision de perimetre : le rafraichissement des droits premium (SSO,
    /// `GET /api/v1/user`) SURVIT a l'extinction de la telemetrie. Le couper
    /// ferait retomber un abonne payant en gratuit au bout de la grace de 14
    /// jours, pour avoir refuse une statistique — ce n'est pas ce qu'il a
    /// demande.
    #[test]
    fn les_droits_premium_se_rafraichissent_meme_telemetrie_eteinte() {
        assert!(
            heartbeat_plan(false).refresh_account,
            "couper la telemetrie ne doit jamais degrader un compte premium"
        );
    }

    /// LIC-1 : l'opt-out coupe le battement DESCRIPTIF, pas la revalidation de
    /// la clé. Sans elle, `license_last_validated` n'est plus jamais restampé
    /// et `LicenseManager::load` dégrade un Premium à clé en Free à J+14.
    #[test]
    fn la_cle_se_revalide_meme_telemetrie_eteinte() {
        assert!(
            heartbeat_plan(false).revalidate_key,
            "refuser la telemetrie ne doit pas faire perdre une licence a cle"
        );
    }

    /// Ce qui autorise cet envoi en opt-out : la charge ne porte que la clé,
    /// l'empreinte et l'identifiant de serveur — rien de descriptif.
    #[test]
    fn la_revalidation_ne_porte_que_trois_champs() {
        let charge = super::charge_de_revalidation("TUNE-TEST", "empreinte", "srv");
        let objet = charge.as_object().expect("un objet JSON");
        let mut clefs: Vec<&str> = objet.keys().map(String::as_str).collect();
        clefs.sort_unstable();
        assert_eq!(
            clefs,
            ["hardware_fingerprint", "license_key", "server_id"],
            "la revalidation ne doit rien porter de descriptif (version, hote, pistes…)"
        );
    }

    /// La branche opt-out du battement appelle bien la revalidation, entre le
    /// rafraîchissement du compte et le `continue`. Lu dans le code de
    /// production seul : ce module cite le nom dans ses commentaires.
    #[test]
    fn la_branche_optout_revalide_la_cle() {
        // (voir aussi `le_battement_emprunte_le_chemin_d_appel_unique` plus bas)
        let source = include_str!("background.rs");
        let production = source
            .split(&format!("#[cfg({})]", "test"))
            .next()
            .expect("source vide");
        let debut = production
            .find("if !plan.send_heartbeat {")
            .expect("branche opt-out introuvable");
        let branche = &production[debut..];
        let fin = branche
            .find("continue;")
            .expect("fin de la branche opt-out");
        assert!(
            branche[..fin].contains(&format!("revalider_la_cle{}", "(")),
            "la branche opt-out du battement doit revalider la cle (LIC-1)"
        );
    }

    /// CLD-2 : le battement et la revalidation de clé passent par
    /// `rate_limit::appeler`, et la partie de production de ce fichier ne
    /// vérifie plus la portée ni ne mémorise le 429 elle-même. La branche
    /// « retenu » du battement garde ses trois gestes : rafraîchir le compte
    /// SSO si prévu, dormir l'intervalle plein, passer au tour suivant.
    #[test]
    fn le_battement_emprunte_le_chemin_d_appel_unique() {
        let source = include_str!("background.rs");
        let production = source
            .split(&format!("#[cfg({})]", "test"))
            .next()
            .expect("source vide");
        assert!(!production.contains(&format!("rate_limit::active{}", "(")));
        assert!(!production.contains(&format!("defer_from_headers{}", "(")));
        assert_eq!(
            production
                .matches(&format!("rate_limit::appeler{}", "("))
                .count(),
            2,
            "le battement et la revalidation, et rien d'autre dans ce fichier"
        );
        let retenu = production
            .find("AppelCloud::Retenu(backoff) => {\n                    debug!(")
            .expect("branche retenu du battement");
        let branche = &production[retenu..retenu + 900];
        for geste in [
            "refresh_account_premium(",
            "sleep(HEARTBEAT_INTERVAL)",
            "continue;",
        ] {
            assert!(
                branche.contains(geste),
                "{geste} doit rester dans la branche retenue"
            );
        }
    }

    /// LIC-3 : après un échec, le battement revient vite (1, 5, 15 min), puis
    /// retrouve sa cadence horaire ; sans échec, il ne bat pas plus souvent.
    #[test]
    fn la_reprise_apres_echec_est_rapprochee_puis_horaire() {
        use super::prochain_intervalle_battement as prochain;
        assert_eq!(prochain(0), HEARTBEAT_INTERVAL);
        assert_eq!(prochain(1).as_secs(), 60);
        assert_eq!(prochain(2).as_secs(), 300);
        assert_eq!(prochain(3).as_secs(), 900);
        assert_eq!(prochain(4), HEARTBEAT_INTERVAL);
        assert_eq!(prochain(u32::MAX), HEARTBEAT_INTERVAL);
    }

    /// Les deux sommeils qui suivent un verdict (opt-out, fin de cycle) passent
    /// par la reprise ; seul le sommeil du 429 garde l'heure pleine, parce que
    /// le serveur a lui-même dit quand revenir.
    #[test]
    fn les_sommeils_apres_verdict_passent_par_la_reprise() {
        let source = include_str!("background.rs");
        let production = source
            .split(&format!("#[cfg({})]", "test"))
            .next()
            .expect("source vide");
        let corps = production
            .split("fn spawn_heartbeat")
            .nth(1)
            .expect("spawn_heartbeat introuvable");
        assert_eq!(
            corps
                .matches("sleep(prochain_intervalle_battement(")
                .count(),
            2,
            "les sommeils apres verdict doivent passer par prochain_intervalle_battement"
        );
        assert_eq!(
            corps.matches("sleep(HEARTBEAT_INTERVAL)").count(),
            1,
            "seul le sommeil du 429 garde HEARTBEAT_INTERVAL"
        );
    }

    /// Seule la boucle du battement change de cadence. Les autres boucles de ce
    /// fichier partagent le meme 300 s et ne sont pas concernees : elles doivent
    /// rester exactement au nombre ou elles etaient.
    #[test]
    fn les_autres_boucles_gardent_leur_cadence_de_300_s() {
        let source = include_str!("background.rs");
        // Aiguille construite a l'execution : ecrite en clair, elle se
        // compterait elle-meme.
        let aiguille = format!("from_secs({})", 300);
        let restants = source.matches(&aiguille).count();
        assert_eq!(
            restants, 3,
            "il doit rester exactement 3 boucles a 300 s (GC des temporaires \
             DASH, rafraichisseur de jetons, diagnostics memoire) — le \
             battement, lui, doit passer par HEARTBEAT_INTERVAL"
        );
    }

    /// Le battement doit lire l'opt-out par le MEME mecanisme que le reste des
    /// envois (`TelemetryReporter::is_enabled_for`), pas par une seconde
    /// lecture maison de la variable d'environnement.
    ///
    /// #3383 : `is_enabled_for` et non `is_enabled` — le second ne connait que
    /// l'environnement, et laissait la bascule de l'interface sans effet.
    #[test]
    fn l_optout_reutilise_le_mecanisme_de_la_telemetrie() {
        // On ne regarde QUE le code de production : les modules de test citent
        // le nom dans leurs commentaires et leurs messages d'assertion, et le
        // test se prouverait tout seul.
        let source = include_str!("background.rs");
        let production = source
            .split(&format!("#[cfg({})]", "test"))
            .next()
            .expect("source vide");
        assert!(
            production.contains(&format!("TelemetryReporter::{}(", "is_enabled_for")),
            "l'opt-out du battement doit reutiliser TelemetryReporter::is_enabled_for, \
             pas relire TUNE_TELEMETRY pour son compte"
        );
    }

    /// #3383 — le battement lit le reglage ECRIT par la bascule de
    /// l'interface, et pas seulement la variable d'environnement.
    ///
    /// Ce temoin APPELLE `plan_du_tour`, c'est-a-dire l'unique expression que
    /// la boucle de production evalue a chaque tour : debrancher la conduite
    /// le fait rougir, ce qu'une recherche de chaine dans le fichier ne ferait
    /// pas.
    #[test]
    fn le_refus_pose_dans_l_interface_coupe_le_battement() {
        let db = base_de_test();
        let reglages = tune_core::db::settings_repo::SettingsRepo::with_backend(db);

        // Rien de pose : comportement d'avant, le battement part.
        assert!(
            super::plan_du_tour(&reglages).send_heartbeat,
            "une installation qui n'a rien decoche doit continuer d'emettre"
        );

        // `POST /cloud/telemetry/disable` ecrit ceci, et rien d'autre.
        reglages
            .set(tune_core::cloud::telemetry::TELEMETRY_SETTING_KEY, "false")
            .expect("ecriture du reglage");
        let plan = super::plan_du_tour(&reglages);
        assert!(
            !plan.send_heartbeat,
            "le refus pose dans l'interface doit couper la charge utile descriptive"
        );
        // Et la licence, elle, continue de vivre — LIC-1 : un opt-out ne se
        // paie jamais en fonctionnalites perdues, y compris a J+15.
        assert!(
            plan.revalidate_key,
            "refuser la telemetrie ne doit pas faire perdre une licence a cle"
        );
        assert!(
            plan.refresh_account,
            "refuser la telemetrie ne doit pas degrader un compte premium"
        );

        // Et il se rallume.
        reglages
            .set(tune_core::cloud::telemetry::TELEMETRY_SETTING_KEY, "true")
            .expect("ecriture du reglage");
        assert!(
            super::plan_du_tour(&reglages).send_heartbeat,
            "re-cocher doit reellement rallumer"
        );
    }

    fn base_de_test() -> std::sync::Arc<dyn tune_core::db::backend::DbBackend> {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().expect("base en memoire");
        db.init_schema().expect("schema");
        tune_core::db::migrations::run_migrations(&db).expect("migrations");
        std::sync::Arc::new(db)
    }
}

#[cfg(test)]
mod heartbeat_server_id_tests {
    /// Le heartbeat doit porter `server_id`, sans quoi le cloud ne peut lier
    /// aucune licence a aucun serveur.
    ///
    /// La route `/api/v1/heartbeat` lit `server_id` et le passe a
    /// `License::claimSession`. Le champ manquait : elle obtenait null et
    /// n'ecrivait rien. Mesure en production le 2026-08-18 : sur 72 licences
    /// premium, 53 sans serveur associe, et seulement 15 auraient ete jugees
    /// eligibles par le relais Tune Bridge.
    ///
    /// Ce test lit le CONTENU du fichier. C'est grossier, mais il attrape la
    /// seule regression qui compte — quelqu'un qui retire le champ de la
    /// charge utile, ou qui le remplace par `instance_id`, lequel est un
    /// identifiant DIFFERENT que le relais ne connait pas.
    #[test]
    fn la_charge_utile_porte_le_server_id() {
        let source = include_str!("background.rs");
        let charge = source
            .split("\"hardware_fingerprint\": ls.hardware_fingerprint,")
            .nth(1)
            .expect("charge utile du heartbeat introuvable");
        let fin = charge
            .find("});")
            .expect("fin de la charge utile introuvable");

        assert!(
            charge[..fin].contains("\"server_id\": server_id"),
            "le heartbeat n'envoie plus `server_id` : le cloud ne pourra plus \
             lier les licences aux serveurs, et le relais refusera tout le monde"
        );
    }

    /// `instance_id` et `server_id` sont deux UUID distincts, ranges sous deux
    /// cles de reglages differentes. Les confondre lierait la licence a un
    /// identifiant que ni la telemetrie ni le relais ne connaissent.
    #[test]
    fn le_server_id_vient_du_meme_accesseur_que_la_telemetrie() {
        let source = include_str!("background.rs");
        assert!(
            source.contains("TelemetryReporter::get_or_create_server_id"),
            "le server_id du heartbeat doit venir du meme accesseur que la \
             telemetrie et que le pont"
        );
    }

    /// Le battement inscrit son cycle au registre des executions (#2080), et
    /// sur les DEUX sorties : l'envoi reel, et l'opt-out ou rien ne part.
    ///
    /// Sans la seconde, un serveur telemetrie eteinte n'aurait aucune ligne, et
    /// « le battement ne tourne plus » serait indistinguable de « le battement
    /// tourne mais n'envoie rien, comme demande ».
    #[test]
    fn le_battement_inscrit_chaque_cycle_au_registre() {
        let source = include_str!("background.rs");
        let corps = source
            .split("fn spawn_heartbeat")
            .nth(1)
            .expect("spawn_heartbeat introuvable")
            .split("\n}\n")
            .next()
            .expect("fin de spawn_heartbeat introuvable");

        assert_eq!(
            corps.matches("TACHE_BATTEMENT_COEUR").count(),
            1,
            "un cycle = une ligne : le registre doit etre ouvert une fois par \
             tour de boucle, pas plusieurs"
        );
        assert!(
            corps.contains("suivi.rien_a_faire"),
            "l'opt-out telemetrie doit fermer sa ligne en `rien a faire`, pas \
             la laisser ouverte jusqu'au prochain redemarrage"
        );
        assert!(
            corps.contains("suivi.terminer(verdict"),
            "le cycle qui a envoye doit fermer sa ligne avec son verdict"
        );
    }

    /// La charge utile du battement porte la cle de licence et l'empreinte
    /// materielle. Le registre, lui, ne doit porter qu'un code HTTP.
    #[test]
    fn le_registre_du_battement_ne_recopie_ni_cle_ni_empreinte() {
        let source = include_str!("background.rs");
        let corps = source
            .split("fn spawn_heartbeat")
            .nth(1)
            .expect("spawn_heartbeat introuvable")
            .split("\n}\n")
            .next()
            .expect("fin de spawn_heartbeat introuvable");

        for ligne in corps.lines().filter(|l| l.contains("motif =")) {
            for interdit in [
                "license_key",
                "hardware_fingerprint",
                "instance_id",
                "server_id",
                "hostname",
            ] {
                assert!(
                    !ligne.contains(interdit),
                    "le motif inscrit au registre ne doit pas porter `{interdit}` : {ligne}"
                );
            }
        }
    }
}

/// GARDE DE SITE (#1770) — les deux chemins qui NOMMENT une zone locale
/// doivent passer par `config::nom_de_zone_locale`, jamais écrire l'étiquette
/// générique en dur.
///
/// C'est la précondition du correctif. La règle « on ne minte pas deux fois
/// l'étiquette générique » vit dans `tune-core::config` ; un site qui écrit
/// `"This Computer"` lui-même ne la consulte pas et refabrique le doublon —
/// deux zones du même nom après une bascule ASIO → WASAPI, mesuré chez
/// jfpaquet en 0.9.130.
///
/// Pourquoi une garde textuelle : les deux fonctions visées sont derrière
/// `#[cfg(feature = "local-audio")]` et demandent un `AppState` complet plus de
/// vrais périphériques. La garde lit le code de PRODUCTION seul — le module de
/// test est retranché avant l'examen, sans quoi sa propre contre-épreuve le
/// ferait rougir. Sa contre-épreuve est
/// `la_garde_refuse_une_etiquette_ecrite_en_dur`.
#[cfg(test)]
mod etiquette_generique_i1770 {
    /// Le code de production d'un fichier : tout ce qui précède le premier
    /// module de test. L'aiguille est construite à l'exécution, sinon ce
    /// module se retrancherait au mauvais endroit dès qu'on le relit par
    /// `include_str!`.
    fn production(source: &str) -> &str {
        let marqueur_test = format!("#[cfg({})]", "test");
        source.split(&marqueur_test).next().unwrap_or("")
    }

    /// Les étiquettes génériques écrites en dur, en littéral Rust, dans le
    /// code de production. Rendues par numéro de ligne (1-indexé).
    fn etiquettes_en_dur(source: &str) -> Vec<usize> {
        let production = production(source);
        let mut lignes = Vec::new();
        for etiquette in tune_core::config::ETIQUETTES_LOCALES_GENERIQUES {
            let aiguille = format!("\"{etiquette}\"");
            let mut curseur = 0usize;
            while let Some(pos) = production[curseur..].find(&aiguille) {
                let debut = curseur + pos;
                lignes.push(production[..debut].lines().count());
                curseur = debut + aiguille.len();
            }
        }
        lignes.sort_unstable();
        lignes
    }

    fn appels_a_la_regle(source: &str) -> usize {
        production(source)
            .matches("config::nom_de_zone_locale(")
            .count()
    }

    #[test]
    fn les_deux_sites_de_nommage_local_passent_par_la_regle() {
        let fichiers = [
            (
                "tune-server/src/background.rs",
                include_str!("background.rs"),
            ),
            ("tune-server/src/startup.rs", include_str!("startup.rs")),
        ];
        let mut total = 0usize;
        for (chemin, source) in fichiers {
            total += appels_a_la_regle(source);
            let en_dur = etiquettes_en_dur(source);
            assert!(
                en_dur.is_empty(),
                "{chemin} écrit une étiquette de zone locale générique en dur, \
                 ligne(s) {en_dur:?}. Ce site ne consulte donc pas \
                 `config::nom_de_zone_locale` et peut minter un second \
                 « This Computer » à côté de celui d'un autre moteur audio \
                 (#1770, jfpaquet, 0.9.130)"
            );
        }
        assert_eq!(
            total, 2,
            "les deux seuls sites qui nomment une zone locale issue d'une \
             énumération sont `register_local_outputs` (startup.rs) et \
             `rescan_local_audio_devices` (background.rs). Un site de plus ou \
             de moins : reprendre le recensement de #1770 avant de toucher à \
             ce compte"
        );
    }

    /// CONTRE-ÉPREUVE du détecteur lui-même. Sans elle, `etiquettes_en_dur`
    /// pourrait ne rien détecter du tout et la garde serait verte contre rien.
    #[test]
    fn la_garde_refuse_une_etiquette_ecrite_en_dur() {
        let fabrique = "fn f() {\n    let zone_name = \"This Computer\".to_string();\n}\n";
        assert_eq!(
            etiquettes_en_dur(fabrique),
            vec![2],
            "le détecteur doit rougir sur un site qui écrit l'étiquette en dur"
        );
        assert_eq!(
            appels_a_la_regle(fabrique),
            0,
            "et ne doit compter aucun appel à la règle là où il n'y en a pas"
        );
    }

    /// TÉMOIN — le détecteur ne rougit pas sur un site conforme.
    #[test]
    fn temoin_un_site_conforme_passe() {
        let fabrique = "fn f() {\n    let zone_name = \
                        tune_core::config::nom_de_zone_locale(&n, d, p);\n}\n";
        assert!(etiquettes_en_dur(fabrique).is_empty());
        assert_eq!(appels_a_la_regle(fabrique), 1);
    }
}

/// GARDE DE SITE (#1770) — les deux chemins qui enregistrent une sortie locale
/// à partir d'une énumération doivent ÉTIQUETER l'hôte qui a énuméré le nom.
///
/// C'est la précondition du correctif. La rectification du backend vit dans
/// `LocalOutput::with_origin_host` (tune-core), donc une sortie qui n'étiquette
/// pas son origine n'est pas rectifiée : elle repart avec `asio` sur un nom
/// WASAPI, et `AsioExclusiveOutput` la refuse — c'est exactement la panne de
/// jfpaquet.
///
/// Pourquoi une garde textuelle et pas une épreuve de bout en bout : les deux
/// fonctions visées sont derrière `#[cfg(feature = "local-audio")]` et
/// demandent un `AppState` complet plus de vrais périphériques. La garde lit le
/// code de PRODUCTION seul — le module de test est retranché avant l'examen,
/// sans quoi il se prouverait lui-même. Sa propre contre-épreuve est
/// `la_garde_refuse_un_site_sans_etiquette` : le détecteur doit rougir sur un
/// extrait fabriqué qui n'étiquette rien.
#[cfg(test)]
mod etiquette_hote_origine_i1770 {
    /// Les sites d'enregistrement de ce fichier qui n'enchaînent PAS
    /// `.with_origin_host(...)` sur le constructeur, rendus par numéro de
    /// ligne (1-indexé) dans le fichier d'origine.
    fn sites_sans_etiquette(source: &str) -> Vec<usize> {
        // Aiguille construite à l'exécution : écrite en clair, ce module se
        // compterait lui-même dès qu'on le relit par `include_str!`.
        let marqueur_test = format!("#[cfg({})]", "test");
        let production = source.split(&marqueur_test).next().unwrap_or("");

        const CONSTRUCTEUR: &str = "LocalOutput::with_options_and_endpoint(";
        const ETIQUETTE: &str = ".with_origin_host(";

        let mut manquants = Vec::new();
        let mut curseur = 0usize;
        while let Some(pos) = production[curseur..].find(CONSTRUCTEUR) {
            let debut = curseur + pos;
            // La chaîne d'appel tient largement dans 800 octets : le plus long
            // des deux sites en fait moins de 300. La borne est reculée sur une
            // frontière de caractère — ces fichiers sont pleins d'accents et de
            // tirets cadratins, et trancher au milieu d'un « — » panique.
            let mut fin = (debut + 800).min(production.len());
            while fin > debut && !production.is_char_boundary(fin) {
                fin -= 1;
            }
            if !production[debut..fin].contains(ETIQUETTE) {
                manquants.push(production[..debut].lines().count());
            }
            curseur = debut + CONSTRUCTEUR.len();
        }
        manquants
    }

    /// Combien de sites ce fichier porte, étiquetés ou non.
    fn nombre_de_sites(source: &str) -> usize {
        let marqueur_test = format!("#[cfg({})]", "test");
        source
            .split(&marqueur_test)
            .next()
            .unwrap_or("")
            .matches("LocalOutput::with_options_and_endpoint(")
            .count()
    }

    #[test]
    fn les_deux_sites_d_enregistrement_local_etiquettent_l_hote_d_origine() {
        let fichiers = [
            (
                "tune-server/src/background.rs",
                include_str!("background.rs"),
            ),
            ("tune-server/src/startup.rs", include_str!("startup.rs")),
        ];

        let mut total = 0usize;
        for (chemin, source) in fichiers {
            total += nombre_de_sites(source);
            let manquants = sites_sans_etiquette(source);
            assert!(
                manquants.is_empty(),
                "{chemin} enregistre une sortie locale sans étiqueter l'hôte \
                 qui a énuméré son nom, ligne(s) {manquants:?}. Sans \
                 `.with_origin_host(&dev.backend)`, \
                 `config::openable_local_backend` n'est jamais consultée : la \
                 sortie repart avec le backend CONFIGURÉ (`asio`) sur un nom \
                 énuméré par WASAPI, et `AsioExclusiveOutput::new` ne peut que \
                 la refuser (#1770, jfpaquet, 0.9.130)"
            );
        }

        assert_eq!(
            total, 2,
            "les deux seuls sites d'enregistrement d'une sortie locale issue \
             d'une énumération sont `register_local_outputs` (startup.rs) et \
             `rescan_local_audio_devices` (background.rs). Un site de plus ou \
             de moins : reprendre le recensement de #1770 avant de toucher à \
             ce compte"
        );
    }

    /// CONTRE-ÉPREUVE du détecteur lui-même. Sans elle, `sites_sans_etiquette`
    /// pourrait ne rien détecter du tout et la garde serait verte contre rien.
    #[test]
    fn la_garde_refuse_un_site_sans_etiquette() {
        let sain = "let o = LocalOutput::with_options_and_endpoint(\n    n, e, x, b,\n)\n.with_origin_host(&dev.backend);\n";
        assert!(
            sites_sans_etiquette(sain).is_empty(),
            "le détecteur doit accepter un site correctement étiqueté"
        );

        let malade = "let o = LocalOutput::with_options_and_endpoint(\n    n, e, x, &configured_backend,\n);\noutputs.register(Box::new(o));\n";
        assert_eq!(
            sites_sans_etiquette(malade),
            vec![1],
            "le détecteur doit nommer la ligne d'un site qui n'étiquette pas \
             son hôte d'origine"
        );

        // Et il ne doit pas se laisser sauver par une étiquette posée
        // très loin, sur un AUTRE appel.
        let loin = format!(
            "let o = LocalOutput::with_options_and_endpoint(n, e, x, b);\n{}\n.with_origin_host(&dev.backend);\n",
            "// remplissage\n".repeat(80)
        );
        assert_eq!(
            sites_sans_etiquette(&loin),
            vec![1],
            "une étiquette hors de la chaîne d'appel ne doit pas compter"
        );
    }
}

/// #3245 — le mode exclusif d'une sortie locale doit se décider PAR
/// PÉRIPHÉRIQUE, sur les DEUX sites qui en enregistrent une.
///
/// La règle est `tune_core::config::local_exclusive_mode_du_peripherique` :
/// elle compose `openable_local_backend` (le backend sous lequel CE nom
/// s'ouvrira, #1770) avec `exclusive_mode_status` (la contrainte ASIO, #3192).
/// Un site qui passe à la place la valeur MACHINE —
/// `effective_exclusive_mode()` — impose l'exclusif d'ASIO à une sortie qui
/// sera ouverte en WASAPI ; Tune ouvre alors WASAPI en exclusif et la machine
/// perd le son de toutes ses autres applications (jfpaquet, Asus Essence
/// STX II).
///
/// Garde TEXTUELLE, et pour la même raison que sa jumelle
/// `etiquette_hote_origine_i1770` : les deux fonctions visées sont derrière
/// `#[cfg(feature = "local-audio")]` et demandent un `AppState` complet plus
/// de vrais périphériques. Elle nomme les SITES D'APPEL et lit le code de
/// PRODUCTION seul — le module de test est retranché avant l'examen, sans quoi
/// il se prouverait lui-même. Sa contre-épreuve est
/// `la_garde_refuse_un_site_qui_impose_la_valeur_machine`.
#[cfg(test)]
mod exclusif_par_peripherique_i3245 {
    const CONSTRUCTEUR: &str = "LocalOutput::with_options_and_endpoint(";
    /// Le nom de la variable que les deux sites remplissent avec la règle.
    /// C'est bien le SITE D'APPEL qui est nommé : la garde n'accepte pas un
    /// booléen calculé ailleurs et recopié.
    const REGLE: &str = "statut_exclusif.effective";

    /// Les ARGUMENTS d'un appel au constructeur, parenthèses comprises.
    ///
    /// Une simple fenêtre d'octets — celle de la garde de #1770 — ne
    /// conviendrait pas ici : le site de `startup.rs` journalise
    /// `statut_exclusif.effective` juste après l'appel, et une fenêtre large se
    /// laisserait sauver par cette TRACE pendant que l'ARGUMENT, lui, serait
    /// redevenu la valeur machine. On lit donc exactement la liste
    /// d'arguments, en comptant les parenthèses.
    fn arguments_du_constructeur(source: &str, debut: usize) -> Option<&str> {
        let ouvre = debut + CONSTRUCTEUR.len() - 1;
        let mut profondeur = 0i32;
        for (i, octet) in source[ouvre..].bytes().enumerate() {
            match octet {
                b'(' => profondeur += 1,
                b')' => {
                    profondeur -= 1;
                    if profondeur == 0 {
                        return Some(&source[ouvre..ouvre + i + 1]);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Le code de production seul : l'aiguille est construite à l'exécution,
    /// sans quoi ce module se compterait lui-même à travers `include_str!`.
    fn production(source: &str) -> &str {
        let marqueur_test = format!("#[cfg({})]", "test");
        let coupe = source.find(&marqueur_test).unwrap_or(source.len());
        &source[..coupe]
    }

    /// Les sites d'enregistrement dont l'ARGUMENT de mode exclusif ne vient PAS
    /// de la règle par périphérique, par numéro de ligne (1-indexé).
    fn sites_sans_regle_par_peripherique(source: &str) -> Vec<usize> {
        let production = production(source);
        let mut manquants = Vec::new();
        let mut curseur = 0usize;
        while let Some(pos) = production[curseur..].find(CONSTRUCTEUR) {
            let debut = curseur + pos;
            let conforme = arguments_du_constructeur(production, debut)
                .is_some_and(|args| args.contains(REGLE));
            if !conforme {
                manquants.push(production[..debut].lines().count());
            }
            curseur = debut + CONSTRUCTEUR.len();
        }
        manquants
    }

    /// Combien de sites ce fichier porte, conformes ou non.
    fn nombre_de_sites_i3245(source: &str) -> usize {
        production(source).matches(CONSTRUCTEUR).count()
    }

    #[test]
    fn les_deux_sites_d_enregistrement_local_decident_l_exclusif_par_peripherique() {
        let fichiers = [
            (
                "tune-server/src/background.rs",
                include_str!("background.rs"),
            ),
            ("tune-server/src/startup.rs", include_str!("startup.rs")),
        ];

        let mut total = 0usize;
        for (chemin, source) in fichiers {
            total += nombre_de_sites_i3245(source);
            let manquants = sites_sans_regle_par_peripherique(source);
            assert!(
                manquants.is_empty(),
                "{chemin} construit une sortie locale sans décider son mode \
                 exclusif PAR PÉRIPHÉRIQUE, ligne(s) {manquants:?}. Sans \
                 `local_exclusive_mode_du_peripherique(...)` en ARGUMENT, la \
                 contrainte « ASIO est exclusif par nature » déborde sur un nom \
                 énuméré par WASAPI : Tune ouvre WASAPI en EXCLUSIF et la \
                 machine perd le son de toutes ses autres applications (#3245, \
                 jfpaquet, Asus Essence STX II)"
            );
        }

        assert_eq!(
            total, 2,
            "les deux seuls sites d'enregistrement d'une sortie locale issue \
             d'une énumération sont `register_local_outputs` (startup.rs) et \
             `rescan_local_audio_devices` (background.rs). Un site de plus ou \
             de moins : reprendre le recensement avant de toucher à ce compte"
        );
    }

    /// CONTRE-ÉPREUVE du détecteur lui-même. Sans elle,
    /// `sites_sans_regle_par_peripherique` pourrait ne rien détecter du tout et
    /// la garde serait verte contre rien.
    #[test]
    fn la_garde_refuse_un_site_qui_impose_la_valeur_machine() {
        let sain = "let x = regle(b, Some(&dev.backend), d);\nlet o = LocalOutput::with_options_and_endpoint(n, e, statut_exclusif.effective, b);\n";
        assert!(
            sites_sans_regle_par_peripherique(sain).is_empty(),
            "le détecteur doit accepter un site dont l'ARGUMENT vient de la règle"
        );
        assert_eq!(nombre_de_sites_i3245(sain), 1);

        let malade = "let s = state.clone();\nlet o = LocalOutput::with_options_and_endpoint(n, e, state.effective_exclusive_mode(), b);\n";
        assert_eq!(
            sites_sans_regle_par_peripherique(malade),
            vec![2],
            "le détecteur doit nommer la ligne d'un site qui impose la valeur \
             MACHINE à chaque périphérique — c'est le défaut de #3245"
        );

        // LE CAS QUI COMPTE, et celui qu'une simple fenêtre d'octets raterait :
        // l'argument est redevenu la valeur machine, mais la TRACE voisine
        // nomme encore la règle. C'est exactement la forme du site de
        // `startup.rs`. Un détecteur qui lirait 800 octets autour de l'appel
        // resterait vert ici, contre un défaut bien vivant.
        let sauve_par_sa_trace = "let s = state.clone();\nlet o = LocalOutput::with_options_and_endpoint(n, e, state.effective_exclusive_mode(), b);\ninfo!(effectif = statut_exclusif.effective);\n";
        assert_eq!(
            sites_sans_regle_par_peripherique(sauve_par_sa_trace),
            vec![2],
            "une trace qui NOMME la règle ne doit pas tenir lieu de l'avoir \
             APPELÉE : c'est l'argument qui atteint le pilote, pas le journal"
        );

        // Et le retranchement du module de test doit vraiment couper.
        let avec_tests = format!(
            "let o = LocalOutput::with_options_and_endpoint(n, e, x, b);\n#[cfg({})]\nmod t {{ LocalOutput::with_options_and_endpoint(n, e, statut_exclusif.effective, b) }}\n",
            "test"
        );
        assert_eq!(
            nombre_de_sites_i3245(&avec_tests),
            1,
            "un site cité dans un module de test ne doit pas être compté"
        );
    }
}

/// #2269 — la passe d'identité doit courir AVANT l'enregistrement, aux DEUX
/// points qui enregistrent une sortie locale.
///
/// « Écrit mais pas branché » est le défaut le plus cher de ce dépôt : une
/// règle correcte, éprouvée, que personne n'appelle. Ici, l'ordre compte
/// autant que l'appel — appliquer l'identité APRÈS la boucle
/// d'enregistrement laisserait la zone neuve déjà créée, et la
/// ré-association n'aurait plus qu'un doublon à contempler.
#[cfg(test)]
mod identite_de_sortie_branchee {
    /// ⚠️ #2082 — ce fichier est inclus en ENTIER, ce module compris. Un motif
    /// écrit d'un bloc se trouverait LUI-MÊME et rendrait le garde vrai quoi
    /// qu'il arrive. Les deux motifs sont donc assemblés à la compilation.
    const APPEL: &str = concat!("appliquer_identite_de_sortie(&", "parc_pour_identite)");
    const CONSTRUCTEUR: &str = concat!("LocalOutput::with_options_and_", "endpoint(");

    #[test]
    fn les_deux_points_denregistrement_appliquent_lidentite_avant_denregistrer() {
        for (fichier, source) in [
            ("startup.rs", include_str!("startup.rs")),
            ("background.rs", include_str!("background.rs")),
        ] {
            let appel = source.find(APPEL).unwrap_or_else(|| {
                panic!(
                    "{fichier} enregistre une sortie locale sans appliquer \
                     l'identité de zone : un DAC revenu sous un autre nom y \
                     recevrait une zone NEUVE à côté de l'ancienne (#2269)."
                )
            });
            let construction = source.find(CONSTRUCTEUR).unwrap_or_else(|| {
                panic!(
                    "{fichier} ne construit plus de sortie locale : ce garde \
                     ne garde plus rien tant qu'il n'a pas suivi."
                )
            });
            assert!(
                appel < construction,
                "{fichier} applique l'identité APRÈS avoir enregistré la \
                 sortie : la zone neuve est déjà créée, et la ré-association \
                 n'a plus qu'un doublon à contempler (#2269)."
            );
        }
    }
}

/// #3809 — l'annonce SlimProto, et l'interrupteur qui ne la gouvernait pas.
#[cfg(test)]
mod annonce_slimproto_tests {
    use super::*;

    #[test]
    fn sans_reglage_l_annonce_reste_armee_comme_avant() {
        assert!(
            annonce_slimproto_activee(None),
            "un parc installé ne doit pas perdre la découverte de ses platines \
             sur une mise à jour (#2938, #2349)"
        );
    }

    #[test]
    fn seul_un_refus_explicite_fait_taire_l_annonce() {
        assert!(!annonce_slimproto_activee(Some("false")));
        assert!(!annonce_slimproto_activee(Some("0")));
        assert!(!annonce_slimproto_activee(Some("  false  ")));
    }

    #[test]
    fn toute_autre_valeur_laisse_l_annonce_armee() {
        for valeur in ["true", "1", "", "oui", "False"] {
            assert!(
                annonce_slimproto_activee(Some(valeur)),
                "« {valeur} » n'est pas un refus : l'annonce reste armée"
            );
        }
    }

    /// La clé du réglage de l'annonce n'est PAS celle de l'intégration LMS.
    ///
    /// Les deux portent le mot « découverte » et vont en sens inverse : c'est
    /// le malentendu exact que #3809 relève. Les confondre rebrancherait le
    /// répondeur UDP sur un réglage dont la valeur par défaut est `false` —
    /// plus aucune platine Squeezebox ne trouverait Tune (#2938, #2349).
    #[test]
    fn l_annonce_et_l_integration_lms_ne_partagent_pas_leur_cle() {
        assert_ne!(CLE_ANNONCE_SLIMPROTO, "squeezebox_enabled");
    }

    /// Garde de SITE : sans appelant, `annonce_slimproto_activee` serait un
    /// réglage de plus qui ne gouverne rien — exactement le défaut que #3809
    /// dénonce. Le dépôt a déjà payé ce motif neuf fois en v0.9.146.
    ///
    /// Le marqueur est épelé en deux morceaux pour que ce test ne se compte pas
    /// lui-même.
    #[test]
    fn le_repondeur_udp_ne_part_qu_apres_la_lecture_du_reglage() {
        let source = include_str!("background.rs");
        let lecture = concat!("annonce_slimproto_", "activee(");
        let armement = concat!("slimproto::discovery::", "spawn(");
        let pos_lecture = source
            .find(&format!("    let annonce = {lecture}"))
            .expect("le réglage doit être lu dans `spawn_slimproto_server` (#3809)");
        let pos_armement = source
            .find(&format!("    tune_core::{armement}"))
            .expect("le répondeur UDP doit rester armé quelque part (#2938)");
        assert!(
            pos_lecture < pos_armement,
            "le répondeur UDP s'armait avant toute lecture du réglage : \
             l'annonce n'était gouvernée par rien (#3809)"
        );
    }

    /// Le réglage ne doit toucher QUE l'annonce.
    ///
    /// `SlimProtoServer::spawn` (TCP 3483) et le serveur CLI 9090 sont armés
    /// plus haut dans la même fonction, avant le `return` du refus : les
    /// déplacer sous la garde couperait la joignabilité des platines, ce que
    /// #2938 et #2349 interdisent.
    #[test]
    fn l_ecoute_tcp_reste_armee_meme_quand_l_annonce_se_tait() {
        // ⚠️ Le corps est borné à la FONCTION, et chaque marqueur épelé en deux
        // morceaux. Écrit autrement, ce témoin lisait le fichier entier — donc
        // ses propres marqueurs — et restait VERT sur son propre sabotage :
        // mesuré le 11/09, la garde retirée de la fonction ne le faisait pas
        // rougir.
        let source = include_str!("background.rs");
        let debut = source
            .find(concat!("fn spawn_slimproto_", "server(state: &AppState"))
            .expect("la fonction doit exister");
        let corps = &source[debut..];
        let corps = &corps[..corps.find("\n}\n").expect("la fonction doit se fermer")];
        let pos_tcp = corps
            .find(concat!("SlimProtoServer::", "new_with_state"))
            .expect("le serveur TCP doit être armé dans cette fonction (#2938)");
        let pos_garde = corps
            .find(concat!("if !", "annonce {"))
            .expect("la garde de l'annonce doit exister dans cette fonction (#3809)");
        assert!(
            pos_tcp < pos_garde,
            "l'écoute TCP 3483 doit être armée AVANT la garde : sans elle, \
             aucune platine Squeezebox ne peut plus piloter Tune (#2938, #2349)"
        );
    }
}
