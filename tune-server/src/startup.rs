use std::sync::Arc;

use tracing::{info, warn};

use tune_core::outputs::oh_events::OpenHomeEventListener;

use crate::config::TuneConfig;
use crate::state::AppState;

/// Restore zone volumes and playback positions from DB, persist config settings.
pub async fn init_state(state: &AppState, config: &TuneConfig) {
    // Turn any update markers left by a just-applied update into a persisted
    // last_update_result the UI can show. Catches a silent Windows bat-swap
    // failure (came back on the old binary) instead of it looking like a no-op.
    crate::routes::system::update::record_post_update_result(state);

    // Warm the ASIO device cache once at boot, while the audio devices are still
    // idle. An ASIO driver — notably SOtM Diretta — can't be re-enumerated once a
    // zone owns it for playback; `list_asio_devices` then serves this cache
    // instead of re-opening the driver. Without a warm pass, the cache stays
    // empty until someone opens the device list, so if auto-resume starts a zone
    // at boot first, the on-demand listing runs while the driver is busy and the
    // DAC never appears — the zone is stuck on the wrong output with no sound
    // (JP Borderies: SOtM DAC absent from the list). Enumerating here, before any
    // playback, captures it. Fire-and-forget; no-op off Windows / without `asio`.
    // `outputs::local` only exists under `local-audio` (the oaat-only CI build
    // compiles without it).
    #[cfg(feature = "local-audio")]
    tokio::task::spawn_blocking(|| {
        let _ = tune_core::outputs::local::list_asio_devices();
    });

    reset_zones_offline(state);
    deduplicate_zones(state);
    ensure_zones_is_hidden(state);
    cleanup_orphan_queues(state);
    reconcile_favorites(state);
    deduplicate_radios(state);
    restore_zone_volumes(state).await;
    restore_playback_positions(state).await;
    restore_queues(state, config);
    restore_queue_metadata(state, config).await;
    restore_oaat_groups(state).await;
    persist_initial_settings(state, config);
    resolve_ytdlp(state).await;
    restore_convolvers(state).await;
    warm_sqlite_cache(state);

    // Re-register manually-added devices (BluOS, legacy DLNA renderers that
    // don't answer SSDP M-SEARCH). Done off the startup path so an offline
    // device's probe timeout doesn't delay boot.
    let state_clone = state.clone();
    tokio::spawn(async move {
        crate::routes::devices::reregister_manual_devices(&state_clone).await;
        // Re-probe auto-discovered renderers whose lazy SSDP responder won't
        // resurface them after a restart (Cyrus Stream X2, #1126).
        crate::discovery_setup::reregister_known_renderers(&state_clone).await;
    });

    // Re-probe auto-discovered DLNA renderers from their persisted LOCATION,
    // so one with a lazy SSDP responder (Cyrus Stream X2) comes back online
    // after a restart without waiting for multicast (#1126). Runs concurrently
    // with SSDP; the registry is keyed by UUID so the first to win re-attaches
    // the zone and the other is a no-op.
    let state_clone = state.clone();
    tokio::spawn(async move {
        crate::routes::devices::reprobe_persisted_dlna_devices(&state_clone).await;
    });

    // Re-probe auto-discovered DLNA renderers from their persisted LOCATION,
    // so one with a lazy SSDP responder (Cyrus Stream X2) comes back online
    // after a restart without waiting for multicast (#1126). Runs concurrently
    // with SSDP; the registry is keyed by UUID so the first to win re-attaches
    // the zone and the other is a no-op.
    let state_clone = state.clone();
    tokio::spawn(async move {
        crate::routes::devices::reprobe_persisted_dlna_devices(&state_clone).await;
    });
}

/// Reset all zones to offline at startup.  Discovery will set actually-present
/// devices back online.  This prevents stale "online" zones from accumulating
/// across restarts and hitting the free-tier zone limit.
fn reset_zones_offline(state: &AppState) {
    match state.backend.execute("UPDATE zones SET online = 0", &[]) {
        Ok(n) => {
            info!(count = n, "zones_reset_offline_at_startup");
        }
        Err(e) => {
            tracing::warn!(error = %e, "zones_reset_offline_failed");
        }
    }
}

/// Remove duplicate zones (same output_device_id) and add a unique index to
/// prevent future duplicates.  Must run before any discovery task starts.
fn deduplicate_zones(state: &AppState) {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    match zone_repo.deduplicate() {
        Ok(removed) if removed > 0 => {
            info!(removed, "zone_duplicates_removed");
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, "zone_dedup_failed");
        }
    }
    // Add a unique index on output_device_id (idempotent) so duplicate zones
    // can never be created again at the SQL level.
    if let Err(e) = state.backend.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_zones_output_device_id ON zones(output_device_id) WHERE output_device_id IS NOT NULL;"
    ) {
        tracing::warn!(error = %e, "zone_unique_index_failed");
    }
}

/// Re-rattache les favoris orphelins aux items vivants retrouvés par identité
/// (instantané titre/artiste/chemin, historique d'écoute en secours). Un
/// rescan qui recrée albums/pistes sous de nouveaux rowids (racines music
/// déplacées, library clear) laissait des favoris fantômes : cœurs éteints et
/// filtre « Favoris » vide (bug .18, v0.9.50). Au démarrage on ne supprime
/// JAMAIS un favori introuvable — un volume pas encore monté ou un scan à
/// venir peut encore le ramener ; seule la passe post-scan complet supprime.
fn reconcile_favorites(state: &AppState) {
    let reconciler = tune_core::db::favorites_reconcile::FavoritesReconciler::with_backend(
        state.backend.clone(),
    );
    match reconciler.run(false) {
        Ok(stats) if stats.changed() > 0 || stats.unresolved > 0 => {
            info!(
                scanned = stats.scanned,
                snapshots = stats.snapshots_backfilled,
                relinked = stats.relinked,
                deduplicated = stats.deduplicated,
                unresolved = stats.unresolved,
                "favorites_reconciled_at_startup"
            );
        }
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "favorites_reconcile_failed"),
    }
}

fn cleanup_orphan_queues(state: &AppState) {
    let sqls = ["DELETE FROM queue_items WHERE zone_id NOT IN (SELECT id FROM zones)"];
    for sql in &sqls {
        match state.backend.execute(sql, &[]) {
            Ok(removed) if removed > 0 => {
                info!(removed, sql = *sql, "orphan_queue_records_cleaned");
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = %e, "orphan_queue_cleanup_failed");
            }
        }
    }
}

fn ensure_zones_is_hidden(state: &AppState) {
    match state.backend.engine() {
        tune_core::db::engine::Engine::Postgres => {
            // Try ALTER TABLE; ignore "duplicate column" error.
            let result = state.backend.execute(
                "ALTER TABLE zones ADD COLUMN is_hidden INTEGER DEFAULT 0",
                &[],
            );
            match result {
                Ok(_) => info!("zones_is_hidden_column_added"),
                Err(e) if e.contains("duplicate") || e.contains("already exists") => {}
                Err(e) => tracing::warn!(error = %e, "zones_is_hidden_column_add_failed"),
            }
        }
        tune_core::db::engine::Engine::Sqlite => {
            // Migration v38 handles this.
        }
    }

    // Ensure last_play_state column exists (migration v39 for SQLite,
    // idempotent ALTER for Postgres).
    match state.backend.engine() {
        tune_core::db::engine::Engine::Postgres => {
            let result = state.backend.execute(
                "ALTER TABLE zones ADD COLUMN last_play_state TEXT DEFAULT 'stopped'",
                &[],
            );
            match result {
                Ok(_) => info!("zones_last_play_state_column_added"),
                Err(e) if e.contains("duplicate") || e.contains("already exists") => {}
                Err(e) => tracing::warn!(error = %e, "zones_last_play_state_add_failed"),
            }
        }
        _ => {}
    }
}

fn deduplicate_radios(state: &AppState) {
    let dedup_sql = "DELETE FROM radio_stations WHERE id NOT IN (SELECT MIN(id) FROM radio_stations GROUP BY name, url)";
    match state.backend.execute(dedup_sql, &[]) {
        Ok(removed) if removed > 0 => {
            info!(removed, "radio_duplicates_removed");
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, "radio_dedup_failed");
        }
    }
    if let Err(e) = state.backend.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_radio_stations_name_url ON radio_stations(name, url);"
    ) {
        tracing::warn!(error = %e, "radio_unique_index_failed");
    }
}

/// Restore persisted queue snapshots from JSON files on disk.
fn restore_queues(state: &AppState, config: &TuneConfig) {
    tune_core::queue_persistence::restore_all_queues(&state.backend, &config.db_path);
}

/// After queues are restored into the DB, load snapshot metadata (repeat_mode,
/// shuffle, queue_length, current_position) into the PlaybackManager so the
/// poller's `next_position()` sees the correct values after a server restart.
async fn restore_queue_metadata(state: &AppState, config: &TuneConfig) {
    let snapshots = tune_core::queue_persistence::load_all_snapshots(&config.db_path);
    let queue_repo =
        tune_core::db::play_queue_repo::PlayQueueRepo::with_backend(state.backend.clone());

    for snap in &snapshots {
        let zone_id = snap.zone_id;

        // Determine queue length from DB (authoritative after restore_all_queues).
        let local_count = queue_repo.count(zone_id).unwrap_or(0);
        let streaming_count = queue_repo.count_streaming(zone_id).unwrap_or(0);
        let queue_len = if local_count > 0 {
            local_count
        } else {
            streaming_count
        };

        if queue_len > 0 {
            state
                .playback
                .update_queue_info(zone_id, snap.current_position, queue_len)
                .await;
        }

        // Restore repeat mode
        let repeat = match snap.repeat_mode.as_str() {
            "one" => tune_core::playback::RepeatMode::One,
            "all" => tune_core::playback::RepeatMode::All,
            _ => tune_core::playback::RepeatMode::Off,
        };
        state.playback.set_repeat(zone_id, repeat).await;

        // Restore shuffle
        state.playback.set_shuffle(zone_id, snap.shuffle).await;

        info!(
            zone_id,
            queue_len,
            position = snap.current_position,
            repeat_mode = %snap.repeat_mode,
            shuffle = snap.shuffle,
            "queue_metadata_restored"
        );
    }
}

async fn restore_convolvers(state: &AppState) {
    #[cfg(not(feature = "local-audio"))]
    let _ = state;
    #[cfg(feature = "local-audio")]
    {
        let settings =
            tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
        let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
        if let Ok(zones) = zone_repo.list() {
            for zone in &zones {
                let Some(zone_id) = zone.id else { continue };
                let key = format!("ir_path_{zone_id}");
                if let Ok(Some(ir_path)) = settings.get(&key) {
                    if !std::path::Path::new(&ir_path).exists() {
                        continue;
                    }
                    let device_id = zone.output_device_id.as_deref().unwrap_or("");
                    if !device_id.starts_with("local:") {
                        continue;
                    }
                    let outputs = state.outputs.lock().await;
                    if let Some(output) = outputs.get(device_id) {
                        let output = output.lock().await;
                        if let Some(local) = output
                            .as_any()
                            .downcast_ref::<tune_core::outputs::local::LocalOutput>()
                        {
                            match local.set_convolver_ir(&ir_path) {
                                Ok(()) => {
                                    info!(zone_id, ir_path = %ir_path, "convolver_restored")
                                }
                                Err(e) => {
                                    warn!(zone_id, error = %e, "convolver_restore_failed")
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Touch key tables so SQLite page cache is warm for the first UI load.
fn warm_sqlite_cache(state: &AppState) {
    use tune_core::db::{album_repo::AlbumRepo, artist_repo::ArtistRepo, track_repo::TrackRepo};
    let _ = TrackRepo::with_backend(state.backend.clone()).count();
    let _ = AlbumRepo::with_backend(state.backend.clone()).count();
    let _ = ArtistRepo::with_backend(state.backend.clone()).count();
    info!("sqlite_cache_warmed");
}

/// Initialize PlaybackManager volume from DB-stored zone volumes and mark devices offline.
///
/// Une zone stockée à 100 % était ramenée à 20 % ici, « garde-fou contre un
/// réveil à plein volume » (2fdc2b5e, collatéral d'un défaut DLNA où le poller
/// écrivait 100 en base pour un renderer à sortie fixe). Ce garde-fou ne
/// protégeait de rien : `PlaybackManager::set_volume` n'écrit ni la base ni la
/// sortie. Il laissait trois valeurs pour une seule zone — base 100, mémoire
/// 0.2, `LocalOutput::user_volume` 1.0 — et envoyait un événement `volume: 0.2`
/// que personne n'avait demandé (les 20 % de #1504 et #1480, attribués à tort
/// au défaut 50 % de `ZoneState::default()` dans #1548).
///
/// La cause d'origine est traitée à la source : le poller ignore désormais un
/// renderer qui annonce 100 % (`status.volume < 0.999`), donc un 100 % en base
/// est aujourd'hui un choix de l'utilisateur. Et la vraie protection contre le
/// réveil à plein volume est dans `register_local_outputs`, qui ensemence la
/// sortie avec la valeur stockée — celle-là agit sur le son. Refs #1596.
async fn restore_zone_volumes(state: &AppState) {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    if let Ok(zones) = zone_repo.list() {
        for zone in &zones {
            if let Some(id) = zone.id {
                let vol = (zone.volume as f64) / 100.0;
                if zone.fixed_volume {
                    // Contrat « Volume fixe (bit-perfect) » : 100 % est un
                    // ENGAGEMENT, pas un oubli — le DoP meurt au moindre gain
                    // logiciel (les marqueurs 0x05/0xFA ne survivent pas à une
                    // multiplication). Le garde-fou ci-dessous rabaissait ces
                    // zones à 20 % à chaque redémarrage, en mémoire seulement :
                    // la base disait 100, l'effectif était 0.2, et le DSD de
                    // Cyrille ressortait en grésillement alors que tous ses
                    // réglages étaient bons (forum 1320, #1504 pour le
                    // désaccord d'affichage).
                    state.playback.set_volume(id, 1.0).await;
                    info!(zone_id = id, zone_name = %zone.name, "zone_volume_fixed_restored_full");
                } else {
                    state.playback.set_volume(id, vol).await;
                    info!(zone_id = id, zone_name = %zone.name, volume = vol, "zone_volume_restored");
                }
            }
        }
    }
}

/// Restore last playback positions from DB so the UI shows where playback left off.
async fn restore_playback_positions(state: &AppState) {
    let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());
    let track_repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    if let Ok(zones) = zone_repo.list() {
        for zone in &zones {
            let Some(zone_id) = zone.id else { continue };
            if zone.last_position_ms == 0
                && zone.last_track_id.is_none()
                && zone.last_track_source.as_deref() != Some("radio")
            {
                continue;
            }
            let np = if let Some(track_id) = zone.last_track_id {
                if let Ok(Some(track)) = track_repo.get(track_id) {
                    // Restore the source/source_id from the *zone* row (the
                    // saved playback origin), not the library row — a track may
                    // have been played from a streaming source.
                    tune_core::playback::NowPlaying {
                        source: zone
                            .last_track_source
                            .clone()
                            .unwrap_or_else(|| "local".into()),
                        source_id: zone.last_track_source_id.clone(),
                        ..tune_core::playback::NowPlaying::from_track(&track)
                    }
                } else {
                    continue;
                }
            } else if zone.last_track_source.as_deref() == Some("radio") {
                continue;
            } else {
                continue;
            };
            let clamped_pos = if np.duration_ms > 0 {
                zone.last_position_ms
                    .min(np.duration_ms.saturating_sub(1000))
            } else {
                zone.last_position_ms
            };
            let dur = np.duration_ms;
            state
                .playback
                .restore_position(zone_id, clamped_pos, np)
                .await;
            info!(
                zone_id,
                zone_name = %zone.name,
                position_ms = clamped_pos,
                original_ms = zone.last_position_ms,
                duration_ms = dur,
                track_id = ?zone.last_track_id,
                "playback_position_restored"
            );
        }
    }
}

/// Restore persisted OAAT multiroom groups from the settings DB.
#[cfg(feature = "oaat")]
async fn restore_oaat_groups(state: &AppState) {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let groups_json = settings
        .get("oaat_groups")
        .ok()
        .flatten()
        .unwrap_or_else(|| "[]".into());
    let groups: Vec<serde_json::Value> = serde_json::from_str(&groups_json).unwrap_or_default();

    let mut restored = 0usize;
    for group in &groups {
        let id = match group["id"].as_str() {
            Some(id) => id.to_string(),
            None => continue,
        };
        let name = group["name"].as_str().unwrap_or("OAAT Group").to_string();
        let endpoints: Vec<(String, u16)> = group["endpoints"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|ep| {
                let host = ep["host"].as_str()?.to_string();
                let port = ep["port"].as_u64()? as u16;
                Some((host, port))
            })
            .collect();

        if endpoints.is_empty() {
            continue;
        }

        let output = tune_core::outputs::oaat::OaatMultiroomOutput::new(
            name.clone(),
            id.clone(),
            endpoints.clone(),
        );
        let mut outputs = state.outputs.lock().await;
        outputs.register(Box::new(output));
        drop(outputs);

        info!(group_id = %id, name = %name, endpoints = endpoints.len(), "oaat_group_restored");
        restored += 1;
    }

    if restored > 0 {
        info!(count = restored, "oaat_groups_restore_complete");
    }
}

#[cfg(not(feature = "oaat"))]
async fn restore_oaat_groups(_state: &AppState) {}

/// Create the OpenHome event listener (shared between SSDP handler and outputs).
pub async fn create_oh_listener() -> Option<Arc<OpenHomeEventListener>> {
    let server_ip = tune_core::discovery::ssdp::get_local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "127.0.0.1".into());
    match OpenHomeEventListener::new(server_ip).await {
        Ok(l) => Some(Arc::new(l)),
        Err(e) => {
            tracing::warn!(error = %e, "oh_event_listener_init_failed");
            None
        }
    }
}

/// Persist music_dirs and discogs_token from config/env into the settings DB.
fn persist_initial_settings(state: &AppState, config: &TuneConfig) {
    if !config.music_dirs.is_empty() {
        let settings =
            tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
        // Seed music_dirs from config ONLY on first run — never clobber a list
        // the user has since edited in Settings. Overwriting on every boot meant
        // a too-broad folder removed via the UI (e.g. C:\ = the whole drive)
        // reappeared on the next restart, so it could never be removed and the
        // temp dir kept being re-scanned (Frédéric). Mirrors the discogs_token
        // first-run guard below. An explicit empty list ("[]") counts as set, so
        // "remove everything" is respected.
        let already_set = settings.get("music_dirs").ok().flatten().is_some();
        if !already_set {
            let normalized_dirs: Vec<String> = config
                .music_dirs
                .iter()
                .map(|d| tune_core::scanner::walker::normalize_path(d))
                .filter(|d| !d.is_empty())
                .collect();
            settings
                .set(
                    "music_dirs",
                    &serde_json::to_string(&normalized_dirs).unwrap(),
                )
                .ok();
        }
    }

    if let Some(ref token) = config.discogs_token {
        let settings =
            tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
        let already_set = settings
            .get("discogs_token")
            .ok()
            .flatten()
            .filter(|v| !v.is_empty())
            .is_some();
        if !already_set {
            settings.set("discogs_token", token).ok();
            info!("discogs_token_persisted_from_env");
        }
    }

    // Mirror the Last.fm API key/secret from env into the settings DB. The whole
    // scrobbling flow (auth.getSession exchange in service_tokens.rs, and the
    // scrobbler in orchestrator.rs) reads these from the settings table, not from
    // config — so a user who only set TUNE_LASTFM_API_KEY/SECRET in .env got
    // "lastfm_api_key not configured" and no scrobbling, even though the keys were
    // loaded (forum #1113). Read straight from env (the server TuneConfig does not
    // carry Last.fm) and persist once when absent, exactly like discogs_token.
    for (env_var, key) in [
        ("TUNE_LASTFM_API_KEY", "lastfm_api_key"),
        ("TUNE_LASTFM_API_SECRET", "lastfm_api_secret"),
    ] {
        let env_val = match std::env::var(env_var) {
            Ok(v) if !v.is_empty() => v,
            _ => continue,
        };
        let settings =
            tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
        let already_set = settings
            .get(key)
            .ok()
            .flatten()
            .filter(|v| !v.is_empty())
            .is_some();
        if !already_set {
            settings.set(key, &env_val).ok();
            info!("{key}_persisted_from_env");
        }
    }

    // Seed the quality_split default so the DB is the single source of truth.
    // get_config injects a `true` default in memory but never persists it, so an
    // untouched DB has no row — and both the manual and auto scanners fall back
    // to `unwrap_or(true)`, silently splitting albums by quality while the UI
    // shows the toggle "enabled". Seeding once (only when the row is absent)
    // makes the toggle authoritative and inspectable via SQL. Reported by Fabien:
    // `SELECT value FROM settings WHERE key='quality_split'` returned empty, and
    // disabling the option in the UI had no visible effect on the next scan.
    {
        let settings =
            tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
        let has_row = settings
            .get("quality_split")
            .ok()
            .flatten()
            .filter(|v| !v.is_empty())
            .is_some();
        if !has_row {
            settings.set("quality_split", "true").ok();
            info!("quality_split_default_seeded value=true");
        }
    }
}

/// Resolve the managed `yt-dlp` binary at boot (from the `yt_dlp_path` setting,
/// the auto-download location, or PATH) so YouTube playback works if it was
/// previously enabled. Does not download anything — that's the opt-in button.
async fn resolve_ytdlp(state: &AppState) {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let configured = settings.get("yt_dlp_path").ok().flatten();
    match tune_core::ytdlp::resolve(configured.as_deref()).await {
        Some(path) => info!(path = %path.display(), "youtube_ytdlp_ready"),
        None => info!("youtube_ytdlp_absent — YouTube playback not enabled"),
    }
}

/// Niveau à donner à une sortie locale qui vient de naître, d'après ce que la
/// base dit de sa zone. Une zone « Volume fixe (bit-perfect) » reste à pleine
/// échelle — c'est son contrat, le DoP ne survit pas à une multiplication.
///
/// Volontairement HORS du gate `local-audio` : c'est de l'arithmétique, sans
/// dépendance à `outputs::local`, et les tests tournent dans les deux jeux de
/// fonctionnalités.
#[cfg_attr(not(feature = "local-audio"), allow(dead_code))]
fn seed_volume_for(zone_volume: i32, fixed_volume: bool) -> f64 {
    if fixed_volume {
        1.0
    } else {
        (zone_volume as f64 / 100.0).clamp(0.0, 1.0)
    }
}

/// Register local audio output devices (USB DAC, headphones, speakers) and auto-create zones.
#[cfg(feature = "local-audio")]
pub async fn register_local_outputs(state: &AppState) {
    // Prefer DB-persisted backend (set via UI) over config/env default
    let audio_backend_owned = state.effective_audio_backend();
    let audio_backend = &audio_backend_owned;
    let exclusive_mode = state.effective_exclusive_mode();
    // Publish it: this is the value the outputs below are built with, and the
    // only honest answer for the signal path until the next restart.
    if let Ok(mut slot) = state.active_audio_backend.write() {
        *slot = Some(audio_backend_owned.clone());
    }

    // Enumerate output devices OFF the async runtime and under a hard timeout.
    //
    // Enumerating ASIO opens each driver to read its formats, and an ASIO driver
    // can only be opened by ONE process at a time: if another app (JRiver, foobar,
    // a DSD ASIO proxy…) already holds it, the open BLOCKS — potentially forever.
    // This call sits on the critical boot path *before* the HTTP listener starts
    // serving, so a blocked ASIO probe used to wedge the whole server: the port was
    // bound but nothing accepted connections → completely blank web UI (JP
    // Borderies, Denafrips USB DAC in ASIO with JRiver open). Running it in
    // `spawn_blocking` under a timeout guarantees the web UI always comes up; if the
    // scan does not respond we start WITHOUT local zones for this boot rather than
    // hang. The device becomes usable again once its driver is free (close the other
    // app) and Tune is relaunched.
    async fn scan_devices(backend: String) -> Option<Vec<tune_core::outputs::local::AudioDevice>> {
        match tokio::time::timeout(
            std::time::Duration::from_secs(8),
            tokio::task::spawn_blocking(move || {
                tune_core::outputs::local::list_audio_devices_with_backend(&backend)
            }),
        )
        .await
        {
            Ok(Ok(devices)) => Some(devices),
            Ok(Err(_)) => {
                warn!("local_audio_enumeration_panicked — starting without local zones this boot");
                None
            }
            Err(_) => {
                warn!(
                    "local_audio_enumeration_timeout — an audio driver (most likely an ASIO device \
                     held by another application such as JRiver) did not respond within 8s. Starting \
                     the server WITHOUT local zones so the web UI stays available; close the other app \
                     and relaunch Tune to use the device."
                );
                None
            }
        }
    }

    // `None` means the scan timed out or panicked. When that happens we do NOT
    // attempt the WASAPI fallback: a hung ASIO probe still holds the internal scan
    // lock, so a second enumeration would only block (and time out) again — better
    // to bring the UI up now and let the next relaunch (with the driver free) pick
    // the device up.
    let scan = scan_devices(audio_backend_owned.clone()).await;
    let mut devices = scan.clone().unwrap_or_default();
    // When ASIO is selected AND the host actually responded but exposed no devices,
    // also enumerate WASAPI so the user still has fallback outputs available.
    if devices.is_empty() && scan.is_some() && audio_backend.eq_ignore_ascii_case("asio") {
        warn!("asio_returned_no_devices — also enumerating WASAPI as fallback");
        devices = scan_devices("wasapi".to_string()).await.unwrap_or_default();
    }
    if !devices.is_empty() {
        let mut outputs = state.outputs.lock().await;
        let zone_repo = tune_core::db::zone_repo::ZoneRepo::with_backend(state.backend.clone());

        for dev in &devices {
            let device_id = format!("local:{}", dev.name);
            let local_out = tune_core::outputs::local::LocalOutput::with_options(
                dev.name.clone(),
                exclusive_mode,
                audio_backend,
            );
            // Ensemencer la sortie avec le volume stocké.
            //
            // `LocalOutput` naît à `user_volume = 1.0` et rien ne le rectifiait :
            // `restore_zone_volumes` ne touche que la copie mémoire du
            // PlaybackManager, et depuis le compromis « Fabien » l'orchestrateur
            // ne réimpose plus le volume enregistré à la lecture. Une zone locale
            // réglée à 30 % repartait donc à PLEIN VOLUME au premier morceau
            // après un redémarrage — c'est précisément le réveil brutal que
            // l'écrêtage à 20 % prétendait empêcher sans jamais y toucher (#1596).
            //
            // Ce compromis-là ne s'applique pas ici : il protège le niveau
            // *physique* d'un appareil externe, que Tune ne connaît pas. Le gain
            // logiciel local, lui, n'appartient qu'à Tune, et sa valeur de départ
            // n'a aucune raison d'être 100 % plutôt que ce que l'utilisateur a
            // réglé. Une zone « Volume fixe » reste à 1.0 : c'est son contrat.
            if let Ok(Some(zone)) = zone_repo.get_by_device_id(&device_id) {
                let stored = seed_volume_for(zone.volume, zone.fixed_volume);
                if let Err(e) =
                    tune_core::outputs::OutputTarget::set_volume(&local_out, stored).await
                {
                    warn!(device_id = %device_id, error = %e, "local_output_volume_seed_failed");
                } else {
                    info!(device_id = %device_id, volume = stored, "local_output_volume_seeded");
                }
            }
            outputs.register(Box::new(local_out));
            info!(
                name = %dev.name,
                device_id = %device_id,
                default = dev.is_default,
                channels = dev.max_channels,
                rates = ?dev.sample_rates,
                "local_audio_output_registered"
            );

            let zone_name = if dev.is_default {
                "This Computer".to_string()
            } else {
                dev.name.clone()
            };

            // « Creer les zones automatiquement » vaut ICI aussi.
            //
            // Les trois autres chemins de decouverte — SSDP, mDNS et le chemin
            // fournisseur — consultent tous `zone_auto_create` avant de creer.
            // Celui-ci, le seul qui s'execute au DEMARRAGE du serveur, ne le
            // consultait pas : une sortie audio locale se voyait donc attribuer
            // une zone a chaque lancement, donc a chaque mise a jour, meme
            // reglage decoche. Une zone deja existante n'est pas concernee
            // (`get_or_create` la renvoie telle quelle) : on ne bloque que la
            // creation, jamais la reconnexion.
            let auto_create =
                tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
                    .get("zone_auto_create")
                    .ok()
                    .flatten()
                    .map(|v| v != "false")
                    .unwrap_or(true);
            if !auto_create
                && zone_repo
                    .get_by_device_id(&device_id)
                    .ok()
                    .flatten()
                    .is_none()
            {
                info!(
                    name = %zone_name,
                    device_id = %device_id,
                    "local_audio_zone_auto_create_disabled_skipping"
                );
                continue;
            }

            match zone_repo.get_or_create(&zone_name, Some("local"), &device_id) {
                Ok((zid, true)) => {
                    info!(
                        name = %zone_name,
                        zone_id = zid,
                        device_id = %device_id,
                        "local_audio_zone_auto_created"
                    );
                }
                Ok((zid, false)) => {
                    let _ = zone_repo.set_online_by_device(&device_id, true);
                    // Zones héritées : les anciennes versions nommaient TOUTES
                    // les zones locales « This Computer » — deux DAC devenaient
                    // des jumelles indiscernables (forum #1233, Alain). Un DAC
                    // non-défaut coincé sur l'étiquette générique prend le nom
                    // du périphérique ; un nom personnalisé n'est jamais touché.
                    if !dev.is_default
                        && let Ok(n) = zone_repo.rename_generic_local_label(zid, &dev.name)
                        && n > 0
                    {
                        info!(zone_id = zid, name = %dev.name, "local_zone_generic_label_healed");
                    }
                    // Device par défaut : le device_id étant dérivé du NOM du
                    // périphérique (`local:<name>`), un renommage du Mac ou un
                    // changement de locale macOS crée une SECONDE zone par
                    // défaut portant l'étiquette générique de l'autre langue
                    // (« This Computer » ⇄ « Cet ordinateur »). get_or_create /
                    // deduplicate matchent sur device_id et ne fusionnent jamais
                    // ces jumelles → les deux restent dans le sélecteur (Philippe
                    // Vella). On masque les jumelles génériques, en gardant celle
                    // liée au device vivant. Étiquettes génériques uniquement —
                    // une zone renommée par l'utilisateur n'est jamais touchée.
                    if dev.is_default
                        && let Ok(n) = zone_repo.hide_duplicate_generic_local(zid)
                        && n > 0
                    {
                        info!(
                            zone_id = zid,
                            hidden = n,
                            "local_default_zone_duplicates_hidden"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        name = %zone_name,
                        device_id = %device_id,
                        error = %e,
                        "local_audio_zone_create_failed"
                    );
                }
            }
        }

        info!(count = devices.len(), "local_audio_devices_registered");
    } else {
        info!("no_local_audio_devices_found");
    }
}

/// Remonte les partages reseau enregistres, avant que quoi que ce soit ne lise
/// la bibliotheque.
///
/// Rien ne les remontait au demarrage. Consequence chez Dominique Comet
/// (#1692) : apres chaque redemarrage son partage SMB n'etait plus monte, le
/// repertoire configure existait mais vide, le scan annoncait « 0 fichier », et
/// il devait re-saisir son partage ET ses identifiants pour retrouver sa
/// musique.
///
/// ⚠️ On lit la table que les ROUTES ecrivent (`mount_type/server/share/…/
/// active`), pas celle de `mount_manager.rs` (`host/share_name/…/auto_mount`),
/// qui porte le meme nom, des colonnes differentes, et n'est construite nulle
/// part hors tests. Batir le remontage sur `auto_mount` interrogerait une table
/// que le serveur ne remplit jamais.
///
/// Chaque montage est independant : un partage injoignable est journalise et
/// n'empeche ni les autres ni le demarrage. Un NAS eteint ne doit pas empecher
/// Tune de servir ce qui est local.
pub async fn remount_network_shares(state: &AppState) {
    let rows = match state.backend.query_many(
        "SELECT server, share, mount_path, username, password \
         FROM network_mounts WHERE mount_type = 'smb' AND COALESCE(active, 1) = 1",
        &[],
    ) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "remount_network_shares_query_failed");
            return;
        }
    };
    if rows.is_empty() {
        return;
    }
    info!(count = rows.len(), "remounting_network_shares");
    for r in rows {
        let host = r.first().and_then(|v| v.as_string()).unwrap_or_default();
        let share = r.get(1).and_then(|v| v.as_string()).unwrap_or_default();
        let path = r.get(2).and_then(|v| v.as_string()).unwrap_or_default();
        if host.is_empty() || share.is_empty() || path.is_empty() {
            continue;
        }
        // Deja monte (redemarrage du seul service, systeme reste debout) :
        // ne pas empiler un second montage sur le meme point.
        if std::path::Path::new(&path)
            .read_dir()
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
        {
            tracing::debug!(host = %host, share = %share, path = %path, "network_share_already_populated_skipping");
            continue;
        }
        let user = r.get(3).and_then(|v| v.as_string()).unwrap_or_default();
        let pass = r.get(4).and_then(|v| v.as_string()).unwrap_or_default();

        // Meme commande que la route de montage — volontairement recopiee
        // plutot que factorisee : la route rend des erreurs HTTP detaillees a
        // un humain qui attend, celle-ci journalise et passe au suivant. Les
        // fusionner obligerait a inventer une abstraction pour deux appelants
        // aux contrats opposes.
        let result = if cfg!(target_os = "macos") {
            let creds = if user.is_empty() {
                "guest@".to_string()
            } else if pass.is_empty() {
                format!("{user}@")
            } else {
                format!("{user}:{pass}@")
            };
            let unc = format!("//{creds}{host}/{share}");
            tokio::time::timeout(
                std::time::Duration::from_secs(15),
                tokio::process::Command::new("mount_smbfs")
                    .args([&unc, &path])
                    .output(),
            )
            .await
        } else {
            let u = if user.is_empty() { "guest" } else { &user };
            let unc = format!("//{host}/{share}");
            let opts = format!("username={u},password={pass},vers=3.0");
            tokio::time::timeout(
                std::time::Duration::from_secs(15),
                tokio::process::Command::new("mount.cifs")
                    .args([&unc, &path, "-o", &opts])
                    .output(),
            )
            .await
        };

        match result {
            Ok(Ok(out)) if out.status.success() => {
                info!(host = %host, share = %share, path = %path, "network_share_remounted")
            }
            Ok(Ok(out)) => warn!(
                host = %host, share = %share,
                error = %String::from_utf8_lossy(&out.stderr).trim(),
                "network_share_remount_failed"
            ),
            Ok(Err(e)) => {
                warn!(host = %host, share = %share, error = %e, "network_share_remount_failed")
            }
            Err(_) => warn!(host = %host, share = %share, "network_share_remount_timeout"),
        }
    }
}

#[cfg(test)]
mod restore_zone_volumes_tests {
    use super::*;
    use tune_core::db::zone_repo::ZoneRepo;

    fn state_with_zone(volume: i32, fixed: bool) -> (AppState, i64) {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        let repo = ZoneRepo::with_backend(state.backend.clone());
        let id = repo
            .create("Zone test", Some("local"), Some("local:Test"))
            .unwrap();
        repo.update_volume(id, volume).unwrap();
        repo.update_fixed_volume(id, fixed).unwrap();
        (state, id)
    }

    /// Forum 1320 (Cyrille) / #1504 — le garde-fou anti-réveil rabaissait
    /// AUSSI les zones « Volume fixe (bit-perfect) » à 20 % au redémarrage :
    /// la base disait 100, l'effectif était 0.2, et le DoP mourait (le
    /// moindre gain logiciel détruit les marqueurs). Une zone fixed_volume
    /// doit redémarrer à exactement 1.0. Ce test ÉCHOUE contre le code
    /// d'avant (0.2 au lieu de 1.0).
    #[tokio::test]
    async fn fixed_volume_zone_restarts_at_full_scale() {
        let (state, id) = state_with_zone(100, true);
        restore_zone_volumes(&state).await;
        let vol = state.playback.get_state(id).await.volume;
        assert!(
            (vol - 1.0).abs() < 1e-9,
            "zone bit-perfect restaurée à {vol} au lieu de 1.0"
        );
    }

    /// #1596 — une zone ordinaire stockée à 100 % revient à 100 %.
    ///
    /// L'écrêtage à 20 % qui vivait ici ne descendait le son de personne : il
    /// ne touchait ni la base ni la sortie. Il ne produisait qu'un désaccord à
    /// trois voix et un événement `volume: 0.2` — les 20 % que Jean Valjean
    /// (#1504) et Bebelalu55 (#1480) ont vus s'afficher. Ce test ÉCHOUE contre
    /// le code d'avant (0.2 au lieu de 1.0).
    #[tokio::test]
    async fn non_fixed_zone_at_full_scale_is_restored_verbatim() {
        let (state, id) = state_with_zone(100, false);
        restore_zone_volumes(&state).await;
        let vol = state.playback.get_state(id).await.volume;
        assert!(
            (vol - 1.0).abs() < 1e-9,
            "un 100 % choisi par l'utilisateur doit revenir à 1.0, obtenu: {vol}"
        );
    }

    /// La mémoire ne doit jamais contredire la base après restauration : c'est
    /// le désaccord que #1548 a soigné côté affichage sans le supprimer.
    #[tokio::test]
    async fn memory_agrees_with_db_for_every_stored_level() {
        for stocke in [0, 20, 55, 99, 100] {
            let (state, id) = state_with_zone(stocke, false);
            restore_zone_volumes(&state).await;
            let vol = state.playback.get_state(id).await.volume;
            let attendu = stocke as f64 / 100.0;
            assert!(
                (vol - attendu).abs() < 1e-9,
                "base {stocke} % / mémoire {vol} — les deux doivent dire la même chose"
            );
        }
    }

    /// #1596 — la protection réelle contre le réveil à plein volume.
    ///
    /// `LocalOutput` naît à 1.0 et personne ne le corrigeait : une zone locale
    /// à 30 % repartait à fond au premier morceau après un redémarrage. C'est
    /// le seul endroit où le volume stocké atteint vraiment le son.
    #[test]
    fn local_output_is_seeded_with_the_stored_level() {
        assert!((seed_volume_for(30, false) - 0.30).abs() < 1e-9);
        assert!((seed_volume_for(0, false) - 0.0).abs() < 1e-9);
        assert!((seed_volume_for(100, false) - 1.0).abs() < 1e-9);
    }

    /// Une zone bit-perfect ne s'ensemence jamais autrement qu'à pleine échelle,
    /// quelle que soit la valeur qui traîne en base (forum 1320, Cyrille).
    #[test]
    fn fixed_volume_output_is_seeded_at_full_scale() {
        assert!((seed_volume_for(20, true) - 1.0).abs() < 1e-9);
        assert!((seed_volume_for(100, true) - 1.0).abs() < 1e-9);
    }

    /// Une valeur aberrante en base ne doit pas amplifier — le gain est un
    /// multiplicateur appliqué à chaque échantillon.
    #[test]
    fn out_of_range_stored_level_never_amplifies() {
        assert!((seed_volume_for(150, false) - 1.0).abs() < 1e-9);
        assert!((seed_volume_for(-5, false) - 0.0).abs() < 1e-9);
    }

    /// Un volume ordinaire est restauré tel quel, fixed ou pas.
    #[tokio::test]
    async fn ordinary_volume_is_restored_verbatim() {
        let (state, id) = state_with_zone(55, false);
        restore_zone_volumes(&state).await;
        let vol = state.playback.get_state(id).await.volume;
        assert!((vol - 0.55).abs() < 1e-9, "volume restauré: {vol}");
    }
}
