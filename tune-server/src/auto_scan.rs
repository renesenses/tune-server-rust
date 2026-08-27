use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tracing::info;
use unicode_normalization::UnicodeNormalization;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::DbBackend;
use tune_core::db::models::Track;
use tune_core::db::track_repo::TrackRepo;
use tune_core::event_bus::EventBus;
use tune_core::scanner::walker::ScannedFile;

/// Resets `scan_status` to "idle" on every exit path of the startup scan —
/// normal completion, early return, or a panic unwind — so the desktop app's
/// scan banner + "Arrêter le scan" button never stick on forever (#1197/#1196).
struct ScanStatusGuard(Arc<dyn DbBackend>);
impl Drop for ScanStatusGuard {
    fn drop(&mut self) {
        let _ = tune_core::db::settings_repo::SettingsRepo::with_backend(self.0.clone())
            .set("scan_status", "idle");
    }
}

/// Build a `Track` from scanned file metadata, resolving artist/album in the DB.
///
/// Returns `(track, album_id, is_compilation)` or `None` if metadata is missing.
pub fn build_track_from_metadata(
    sf: &ScannedFile,
    artist_repo: &ArtistRepo,
    album_repo: &AlbumRepo,
) -> Option<(Track, Option<i64>)> {
    build_track_from_metadata_opts(sf, artist_repo, album_repo, true, None)
}

pub fn build_track_from_metadata_opts(
    sf: &ScannedFile,
    artist_repo: &ArtistRepo,
    album_repo: &AlbumRepo,
    quality_split: bool,
    // Folder-level compilation decision from the caller (the batch/watcher sees
    // an album's other tracks; a lone file can't). `None` = decide from this
    // file's own tags, the previous behaviour. Passing `Some(true)` keeps a
    // various-artists compilation whose tracks each carry their own artist as
    // album_artist from splitting into one album per artist (JP Borderies).
    compilation_override: Option<bool>,
) -> Option<(Track, Option<i64>)> {
    let meta = sf.metadata.as_ref()?;

    let is_compilation = compilation_override.unwrap_or_else(|| {
        meta.compilation
            || meta
                .album_artist
                .as_deref()
                .map(|s| s.to_lowercase())
                .map(|s| {
                    s == "various artists" || s == "various" || s == "va" || s == "compilations"
                })
                .unwrap_or(false)
    });

    let album_artist_name = if is_compilation {
        "Various Artists"
    } else {
        meta.album_artist.as_deref().unwrap_or_else(|| {
            meta.artist
                .as_deref()
                .unwrap_or(tune_core::db::artist_repo::UNKNOWN_ARTIST_NAME)
        })
    };

    let track_artist_name = meta
        .artist
        .as_deref()
        .unwrap_or(tune_core::db::artist_repo::UNKNOWN_ARTIST_NAME);

    let album_artist_mbid = if is_compilation {
        None
    } else {
        meta.musicbrainz_album_artist_id
            .as_deref()
            .or(meta.musicbrainz_artist_id.as_deref())
    };
    let album_artist_entry = match artist_repo.get_or_create(
        album_artist_name,
        album_artist_mbid,
        meta.album_artist_sort.as_deref(),
    ) {
        Ok(a) => {
            if let Some(ref mbid) = a.musicbrainz_id {
                if a.name.to_lowercase() != album_artist_name.to_lowercase() {
                    tracing::warn!(
                        expected = album_artist_name,
                        resolved = %a.name,
                        mbid = %mbid,
                        file = %sf.path,
                        "album_artist_mbid_name_mismatch"
                    );
                }
            }
            Some(a)
        }
        Err(e) => {
            tracing::warn!(
                artist = album_artist_name,
                error = %e,
                file = %sf.path,
                "album_artist_create_failed_skipping_track"
            );
            return None;
        }
    };
    let album_artist_id = album_artist_entry.as_ref().and_then(|a| a.id);

    let track_artist = if is_compilation && track_artist_name != album_artist_name {
        match artist_repo.get_or_create(
            track_artist_name,
            meta.musicbrainz_artist_id.as_deref(),
            None,
        ) {
            Ok(a) => Some(a),
            Err(e) => {
                tracing::warn!(artist = track_artist_name, error = %e, "track_artist_create_failed");
                album_artist_entry.clone()
            }
        }
    } else {
        album_artist_entry.clone()
    };
    let artist_id = track_artist.as_ref().and_then(|a| a.id);

    let album = meta.album.as_ref().and_then(|title| {
        let Some(aid) = album_artist_id else {
            tracing::warn!(album = title, file = %sf.path, "album_skipped_no_artist_id");
            return None;
        };
        tracing::debug!(
            album = %title,
            album_artist_tag = ?meta.album_artist,
            album_artist_resolved = album_artist_name,
            album_artist_id = aid,
            album_artist_mbid = ?album_artist_mbid,
            track_artist = track_artist_name,
            mb_artist_id = ?meta.musicbrainz_artist_id,
            mb_album_artist_id = ?meta.musicbrainz_album_artist_id,
            file = %sf.path,
            "DIAG_album_resolution"
        );
        // The album's folder identifies the release — see
        // `scanner::album_folder` and `AlbumRepo::get_or_create_for_folder`.
        //
        // The quality tier used to be appended to the TITLE ("Album
        // (96kHz/24bit)") to keep a hi-res copy from merging with a CD rip. It
        // separated far more than intended: an edition whose discs differ in
        // sample rate — a box set at 24/192, 16/44.1 and 24/48 — showed up as
        // three albums under three near-identical titles. The folder separates
        // exactly what should be separate, and the client already renders the
        // real quality as a badge from `sample_rate`/`bit_depth`, so the title
        // never needed to carry it.
        //
        // Disambiguation by MusicBrainz release id and by (title, artist_id,
        // year) is unchanged, inside `get_or_create_for_folder`.
        // `quality_split` keeps its meaning — "if the same album exists in CD and
        // Hi-Res, create two separate entries" — and the folder is what now
        // delivers it. Off ⇒ empty folder ⇒ `get_or_create_for_folder` falls
        // straight through to the title+artist identity, merging both copies.
        let folder = if quality_split {
            tune_core::scanner::album_folder::album_folder(&sf.path).unwrap_or_default()
        } else {
            String::new()
        };
        album_repo
            .get_or_create_for_folder(
                &folder,
                title,
                aid,
                meta.year.map(|y| y as i32),
                meta.musicbrainz_release_id.as_deref(),
            )
            .ok()
    });
    let album_id = album.as_ref().and_then(|a| a.id);

    // Garder la décision qui vient d'être prise (#1957) : c'est elle qui a
    // envoyé l'album sous « Various Artists » plus haut. Cette voie est celle
    // du surveillant de fichiers, où `compilation_override` reconstruit la vue
    // du dossier depuis la base — donc le drapeau enregistré ici est bien le
    // même que celui du scan par lots. `mark_compilation` ne fait que lever le
    // drapeau, jamais le baisser (voir sa documentation).
    if let Some(aid) = album_id
        && is_compilation
    {
        album_repo.mark_compilation(aid).ok();
    }

    // Propagate date metadata from track tags to the album (COALESCE — only
    // fills in values not already set, so the first track with dates wins).
    if let Some(aid) = album_id {
        album_repo
            .update_dates(
                aid,
                meta.year.map(|y| y as i32),
                meta.original_year.map(|y| y as i32),
                meta.release_date.as_deref(),
                meta.original_date.as_deref(),
            )
            .ok();
    }

    // Field mapping is shared with the manual scan via `scan_import` — this
    // path now also populates `genres` and `composer`, which the old inline
    // mapping here dropped.
    let track =
        crate::scan_import::build_track_row(meta, sf, album_id, artist_id, track_artist_name);
    Some((track, album_id))
}

/// Spawn the auto-scan task that indexes all music directories at startup.
///
/// Returns an `Arc<AtomicBool>` that is set to `true` once the scan finishes.
/// The file watcher should wait for this flag before monitoring directories,
/// otherwise it may pick up filesystem events triggered by the scan itself
/// (macOS FSEvents can replay recent events on watcher startup) and race
/// with the scanner — deleting freshly inserted tracks.
pub fn spawn_auto_scan(db: Arc<dyn DbBackend>, event_bus: Arc<EventBus>) -> Arc<AtomicBool> {
    let scan_done = Arc::new(AtomicBool::new(false));
    let scan_done_clone = scan_done.clone();
    tokio::task::spawn_blocking(move || {
        info!("auto_scan_starting");
        let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone());
        let raw_dirs: Vec<String> = settings
            .get("music_dirs")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        let music_dirs: Vec<String> = raw_dirs
            .iter()
            .map(|d| tune_core::scanner::walker::normalize_path(d))
            .filter(|d| !d.is_empty())
            .collect();

        if music_dirs.is_empty() {
            info!("auto_scan_skipped_no_dirs");
            // Mark the scan "done" even on this early exit: the file watcher
            // waits on this flag before it starts watching.
            scan_done_clone.store(true, Ordering::Release);
            return;
        }

        // Le scan de démarrage partage exactement la même porte que les scans
        // manuels et planifiés. L'acquisition précède même l'énumération : deux
        // walkers ne peuvent donc jamais converger ensuite vers des écritures
        // et purges concurrentes.
        let Some(scan_lease) = crate::routes::system::scan::try_begin_scan() else {
            info!("auto_scan_skipped_already_scanning");
            scan_done_clone.store(true, Ordering::Release);
            return;
        };
        let _scan_lease = scan_lease;

        // Make the startup scan first-class, exactly like the manual one:
        // advertise it via `scan_status` and honour cooperative cancellation.
        // The guards reset the persisted status before releasing the unique
        // owner on every exit path, including panic unwind.
        let _ = settings.set("scan_status", "scanning");
        let _scan_status_guard = ScanStatusGuard(db.clone());

        let exclude_patterns = scan_exclude_patterns(&db);
        if !exclude_patterns.is_empty() {
            info!(patterns = ?exclude_patterns, "scan_exclude_paths_active");
        }
        let list_result = tune_core::scanner::walker::list_audio_files_with_excludes(
            &music_dirs,
            &exclude_patterns,
        );
        let missing_dirs = list_result.missing_dirs;
        let missing_dir_reasons = list_result.missing_dir_reasons;
        let error_dirs = list_result.error_dirs;
        let mut skipped_by_ext = list_result.skipped_by_ext;
        let mut skipped_reasons = list_result.skipped_reasons;
        let files = list_result.files;
        let total_discovered = files.len();
        info!(files = total_discovered, "auto_scan_files_found");

        // NFC-normalized set of every path found on disk this scan. Used after
        // the scan to prune tracks whose files were deleted while the server was
        // stopped (Symptom 2: deleted albums persist). Normalization matches how
        // existing_tracks keys are compared in the pre-filter below.
        let discovered_paths: std::collections::HashSet<String> = files
            .iter()
            .map(|p| p.to_string_lossy().nfc().collect::<String>())
            .collect();

        let track_repo = TrackRepo::with_backend(db.clone());
        // Artist/album resolution during the batch loop is owned by the shared
        // `TrackImporter` below; `album_repo` is still used post-scan for album
        // stats and orphan cleanup.
        let album_repo = AlbumRepo::with_backend(db.clone());

        // A DB read error must ABORT the scan, not degrade into an empty map:
        // with an empty map every file on disk looks new, so a transient DB
        // hiccup would re-insert the whole library as duplicates. (The
        // ScanStatusGuard resets scan_status on this early return.)
        let existing_tracks = match track_repo.get_all_local_file_info() {
            Ok(map) => map,
            Err(e) => {
                tracing::error!(error = %e, "auto_scan_aborted_existing_tracks_read_failed");
                scan_done_clone.store(true, Ordering::Release);
                return;
            }
        };
        let mut known_hashes: std::collections::HashSet<(String, i64)> = track_repo
            .get_existing_audio_hash_album_pairs()
            .unwrap_or_default();

        // Keep only files that are new or whose mtime/size changed since the
        // last scan. This stat()s every discovered file; on a network mount
        // (SMB/NFS) each stat is a round-trip, so 100k files took minutes at
        // startup (Yves: "très long à démarrer"). Run the checks on a dedicated
        // thread pool oversubscribed well past the core count so the network
        // latency of many stats overlaps instead of running one at a time.
        use rayon::prelude::*;
        // Shared with the manual scan (routes::system::scan) so the two pre-scan
        // skip filters can't diverge on the NFC key again (the "scan
        // interminable" bug: NFD-named files missing the map and re-read over SMB).
        let is_changed = |path: &std::path::Path| {
            crate::routes::system::scan::file_needs_scan(path, &existing_tracks)
        };
        // `scan_io_concurrency()` et non 32 en dur : ce pool ignorait
        // `TUNE_SCAN_IO_CONCURRENCY`, donc régler la variable ne calmait que la
        // moitié de la charge — et personne ne comprenait pourquoi (#1948).
        let stat_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(tune_core::scanner::walker::scan_io_concurrency())
            .build()
            .ok();
        let files_to_scan: Vec<std::path::PathBuf> = match &stat_pool {
            Some(pool) => {
                pool.install(|| files.into_par_iter().filter(|p| is_changed(p)).collect())
            }
            None => files.into_iter().filter(|p| is_changed(p)).collect(),
        };
        let pre_skipped = total_discovered - files_to_scan.len();

        info!(
            total = total_discovered,
            changed = files_to_scan.len(),
            unchanged = pre_skipped,
            "auto_scan_pre_filter_complete"
        );

        event_bus.emit(
            "library.scan.started",
            serde_json::json!({
                "music_dirs": &music_dirs,
                "total": total_discovered,
                "to_scan": files_to_scan.len(),
                "unchanged": pre_skipped,
                "auto": true,
            }),
        );

        let cache_dir = crate::routes::library::artwork_cache_dir();
        info!(cache_dir = %cache_dir.display(), "artwork_cache_dir_resolved");
        let quality_split = tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone())
            .get("quality_split")
            .ok()
            .flatten()
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true);
        // Shared artist/album resolver + Track builder, identical to the manual
        // scan. Using it here fixes the drift where the auto/startup scan used a
        // simpler resolver and could split a compilation (or an album with
        // per-track soloists) into one album+cover per artist.
        let mut importer =
            crate::scan_import::TrackImporter::new(db.clone(), quality_split, cache_dir.clone());
        let mut inserted = 0u64;
        let mut updated = 0u64;
        let mut db_insert_failed = 0u64;
        let mut db_update_failed = 0u64;
        // `skipped` stays the aggregate the UI already shows; the per-cause
        // counters make the report actionable ("skipped 1200" alone doesn't
        // say whether the library is healthy or half the NAS failed to read).
        let mut skipped = pre_skipped as u64;
        let mut skipped_unchanged = pre_skipped as u64;
        let mut skipped_duplicate = 0u64;
        let mut skipped_no_metadata = 0u64;
        let mut skipped_unsupported = 0u64;

        // Progress telemetry for the auto/startup scan (parity with the manual
        // scan) so the UI shows a live bar during it too.
        let scan_total = files_to_scan.len() as i64;
        let scan_timer_start = std::time::Instant::now();
        let mut last_progress_emit = scan_timer_start;

        let stats = tune_core::scanner::walker::scan_files_batched(
            &files_to_scan,
            true,
            tune_core::scanner::walker::SCAN_BATCH_SIZE,
            |batch, batch_idx, _total_files| {
                // Cooperative cancellation: once "Arrêter le scan" was pressed,
                // skip all remaining batches so the startup scan drains quickly
                // (same pattern as the manual scan, #1129/#1197).
                if crate::routes::system::scan::scan_cancel_requested() {
                    return;
                }
                let mut to_insert: Vec<Track> = Vec::with_capacity(batch.len());
                let mut to_update: Vec<Track> = Vec::with_capacity(batch.len() / 4);

                // Manual transaction for batch performance (SQLite only;
                // PG handles transactions at the pool level).
                if db.engine() == tune_core::db::engine::Engine::Sqlite {
                    if db.execute("BEGIN IMMEDIATE", &[]).is_ok() {
                        // Se nommer : tout `write_tx` concurrent echouera tant
                        // que ce lot tient la connexion, et sans cette
                        // etiquette son message n'apprend rien (#1997).
                        tune_core::db::tx_holder::declarer("scan:auto");
                    }
                }

                importer.begin_batch(&batch);

                for sf in &batch {
                    if let Some(unsupported) = &sf.unsupported {
                        tracing::info!(
                            path = %sf.path,
                            format = %unsupported.report_key,
                            reason = unsupported.reason,
                            "scan_track_skipped_unsupported"
                        );
                        skipped += 1;
                        skipped_unsupported += 1;
                        continue;
                    }
                    if sf.metadata.is_none() {
                        tracing::warn!(path = %sf.path, "scan_track_skipped_no_metadata");
                        // Counted in the aggregate too, so `processed` can
                        // actually reach `total` — before this, every failed
                        // file made the progress bar stop short of 100%.
                        skipped += 1;
                        skipped_no_metadata += 1;
                        continue;
                    }

                    // Early-exit: skip unchanged files BEFORE resolving artist/album.
                    // Without this, build_track_from_metadata can create a ghost album
                    // (with cover but no tracks) for files that are ultimately skipped.
                    if let Some(&(_existing_id, existing_mtime, existing_size)) =
                        existing_tracks.get(&sf.path)
                    {
                        let file_changed = existing_mtime
                            .is_none_or(|m| (m - sf.mtime as f64).abs() > 0.5)
                            || (existing_size != Some(sf.file_size as i64));
                        if !file_changed {
                            skipped += 1;
                            skipped_unchanged += 1;
                            continue;
                        }
                    }

                    let Some((mut track, _album_id)) = importer.import(sf) else {
                        continue;
                    };

                    // File already exists and has changed — collect for batch update
                    if let Some(&(existing_id, _, _)) = existing_tracks.get(&sf.path) {
                        track.id = Some(existing_id);
                        to_update.push(track);
                        continue;
                    }

                    // Deduplicate by audio_hash + album_id: if the same content
                    // already exists in this album (via a different path), skip it.
                    if let (Some(hash), Some(aid)) = (&track.audio_hash, track.album_id) {
                        let key = (hash.clone(), aid);
                        if known_hashes.contains(&key) {
                            tracing::debug!(
                                audio_hash = %hash,
                                album_id = aid,
                                path = %sf.path,
                                "skip_duplicate_audio_hash"
                            );
                            skipped += 1;
                            skipped_duplicate += 1;
                            continue;
                        }
                        known_hashes.insert(key);
                    }

                    to_insert.push(track);
                }

                // Per-row failures inside create_batch/update_batch are logged
                // there and swallowed — count the shortfall so the report shows
                // tracks that were scanned but never made it into the DB.
                let batch_inserted = track_repo.create_batch(&to_insert).unwrap_or(0) as u64;
                let batch_updated = track_repo.update_batch(&to_update).unwrap_or(0) as u64;
                db_insert_failed += to_insert.len() as u64 - batch_inserted;
                db_update_failed += to_update.len() as u64 - batch_updated;
                inserted += batch_inserted;
                updated += batch_updated;

                // Extract extended metadata (ISRC, ReplayGain, MusicBrainz, lyrics, etc.)
                {
                    let meta_repo =
                        tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(
                            db.clone(),
                        );
                    let mut meta_entries: Vec<(i64, std::collections::HashMap<String, String>)> =
                        Vec::new();
                    for sf in &batch {
                        if sf.metadata.is_some() {
                            let path = std::path::Path::new(&sf.path);
                            if let Ok(Some(track)) = track_repo.get_by_path(&sf.path) {
                                if let Some(track_id) = track.id {
                                    let ext = tune_core::metadata::read_extended_metadata(path);
                                    if !ext.is_empty() {
                                        meta_entries.push((track_id, ext));
                                    }
                                }
                            }
                        }
                    }
                    if !meta_entries.is_empty() {
                        meta_repo.set_batch_multi(&meta_entries).ok();
                    }
                }

                if db.engine() == tune_core::db::engine::Engine::Sqlite {
                    db.execute("COMMIT", &[]).ok();
                    // Liberer meme si le COMMIT a echoue : une etiquette
                    // perimee accuserait un innocent au prochain incident.
                    tune_core::db::tx_holder::liberer();
                }

                // Emit scan progress after each batch (throttled every other
                // batch or 2s), mirroring the manual scan's payload/phase.
                let processed = (inserted + updated + skipped) as i64;
                if processed > 0
                    && (batch_idx % 2 == 0
                        || last_progress_emit.elapsed() >= std::time::Duration::from_secs(2))
                {
                    last_progress_emit = std::time::Instant::now();
                    let elapsed_secs = scan_timer_start.elapsed().as_secs_f64().max(0.001);
                    let tracks_per_second = processed as f64 / elapsed_secs;
                    let remaining = (scan_total - processed).max(0);
                    let eta_seconds = if tracks_per_second > 0.0 {
                        (remaining as f64 / tracks_per_second) as u64
                    } else {
                        0
                    };
                    event_bus.emit(
                        "library.scan.progress",
                        serde_json::json!({
                            "phase": "files",
                            "scanned": processed,
                            "added": inserted,
                            "total": scan_total,
                            "batch": batch_idx,
                            "inserted": inserted,
                            "updated": updated,
                            "skipped": skipped,
                            "tracks_per_second": (tracks_per_second * 10.0).round() / 10.0,
                            "eta_seconds": eta_seconds,
                        }),
                    );
                }
            },
        );

        for (format, count) in &stats.unsupported_by_ext {
            *skipped_by_ext.entry(format.clone()).or_insert(0) += count;
        }
        skipped_reasons.extend(stats.unsupported_reasons.clone());

        // Album covers extracted during the scan (owned by the importer).
        let artwork_extracted = importer.artwork_extracted();

        // Prune tracks whose files no longer exist on disk. The startup
        // auto-scan never removed stale rows, so files/folders deleted while
        // the server was stopped kept track_count>0 and their album was never
        // orphaned → "les albums supprimés continuent d'apparaître" (eric).
        // SAFETY: skip tracks under a missing directory (unmounted NAS / a
        // Docker mount that isn't present) — deleting them would wipe the
        // library. Mirrors the manual-scan prune (routes/system/scan.rs).
        // A cancelled scan never prunes: `discovered_paths` may be partial and
        // Stop must never be destructive. Same subtree protection as the manual
        // scan for `error_dirs` (walk errors mid-scan: files exist but never
        // made it into the discovered set).
        // Hissé hors du bloc pour la réconciliation des favoris (#1943).
        let mut racines_videes: Vec<String> = Vec::new();
        if crate::routes::system::scan::scan_cancel_requested() {
            info!("auto_scan_prune_skipped_cancelled");
        } else {
            // C'est CE scan-ci qui frappait Dominique : il tourne au démarrage
            // du service, précisément au moment où un montage SMB peut ne pas
            // encore être là. Le point de montage existe, il est lisible, il
            // est vide — et la bibliothèque partait avec (#1652).
            let existing_refs: Vec<&str> = existing_tracks.keys().map(|s| s.as_str()).collect();
            racines_videes = crate::routes::system::scan::roots_gone_empty(
                &music_dirs,
                &existing_refs,
                &discovered_paths,
            );
            let emptied_roots = &racines_videes;
            // Un montage IMBRIQUÉ qui tombe laisse la racine répondre : ni
            // `missing_dirs`, ni `error_dirs`, ni `emptied_roots` ne le voient,
            // et tout le sous-arbre partait sans un mot (#1943).
            let sous_arbres =
                crate::routes::system::scan::sous_arbres_vides(&existing_refs, &discovered_paths);
            if !sous_arbres.is_empty() {
                tracing::error!(
                    dossiers = ?sous_arbres,
                    seuil = SEUIL_SOUS_ARBRE_VIDE,
                    "auto_scan_sous_arbre_vide — ces dossiers ont perdu leurs pistes d'un coup \
                     alors que leur racine répond. Montage imbriqué absent ? CONSERVÉES."
                );
            }
            if !emptied_roots.is_empty() {
                tracing::error!(
                    roots = ?emptied_roots,
                    "auto_scan_root_went_empty — ce dossier contenait des pistes et n'en présente plus aucune. Montage absent ? Les pistes sont CONSERVÉES."
                );
            }
            // Même règle que le scan manuel, et au même endroit : ces deux
            // boucles étaient des copies portant les mêmes trous (#1943).
            // Celle-ci est la plus dangereuse des deux — elle tourne au
            // démarrage, donc AVANT qu'un montage USB ou SMB soit prêt.
            use crate::routes::system::scan::{
                PART_MAX_PURGE, SEUIL_SOUS_ARBRE_VIDE, VerdictPurge, purge_trop_massive,
                verdict_purge,
            };
            let mut pruned = 0i64;
            let mut protected = 0i64;
            let mut hors_perimetre = 0i64;
            let mut a_supprimer: Vec<i64> = Vec::new();
            let examinees = existing_tracks.len();
            for (db_path, &(track_id, _, _)) in &existing_tracks {
                if !discovered_paths.contains(db_path.as_str()) {
                    match verdict_purge(
                        db_path,
                        &music_dirs,
                        &missing_dirs,
                        &error_dirs,
                        emptied_roots,
                        &sous_arbres,
                    ) {
                        VerdictPurge::ProtegeIllisible => protected += 1,
                        VerdictPurge::HorsPerimetre => hors_perimetre += 1,
                        VerdictPurge::Supprimer => a_supprimer.push(track_id),
                    }
                }
            }
            if purge_trop_massive(a_supprimer.len(), examinees) {
                tracing::error!(
                    candidats = a_supprimer.len(),
                    examinees,
                    plafond = PART_MAX_PURGE,
                    // Pas de `confirm_purge` ici, et c'est VOLONTAIRE : un
                    // scan automatique n'a aucune intention d'utilisateur
                    // derrière lui. Il ne doit jamais pouvoir supprimer en
                    // masse, quel que soit le réglage. La sortie passe par un
                    // scan explicite — on le dit, plutôt que de laisser le
                    // refus se rejouer sans issue.
                    "auto_scan_purge_refusee_trop_massive — disparition massive au démarrage : \
                     bien plus souvent un montage pas encore prêt qu'une suppression réelle. \
                     Les pistes sont CONSERVÉES. Un scan automatique ne peut JAMAIS purger \
                     au-delà du plafond : si ces pistes ont vraiment été supprimées, lancer un \
                     scan explicite avec `?confirm_purge={}`.",
                    a_supprimer.len()
                );
                protected += a_supprimer.len() as i64;
                a_supprimer.clear();
            }
            for track_id in a_supprimer {
                if track_repo.delete(track_id).is_ok() {
                    pruned += 1;
                }
            }
            if hors_perimetre > 0 {
                tracing::warn!(
                    hors_perimetre,
                    racines = ?music_dirs,
                    "auto_scan_tracks_hors_perimetre — hors de toute racine configurée, donc \
                     CONSERVÉES (#1943)."
                );
            }
            if protected > 0 {
                tracing::warn!(
                    protected,
                    missing = ?missing_dirs,
                    walk_errors = ?error_dirs,
                    emptied = ?emptied_roots,
                    "auto_scan_tracks_protected_unreadable_dirs"
                );
            }
            if pruned > 0 {
                info!(pruned, "auto_scan_stale_tracks_removed");
            }
        }

        // Comme la réconciliation des favoris, une réattribution d'album exige
        // un scan complet et sain. Le démarrage avec montage absent, erreur de
        // parcours ou annulation reste strictement en lecture seule ici.
        let full_scan_ok = !crate::routes::system::scan::scan_cancel_requested()
            && missing_dirs.is_empty()
            && error_dirs.is_empty()
            && racines_videes.is_empty();
        if full_scan_ok {
            match album_repo.repair_empty_mbid_artist_collapses() {
                Ok(repaired) if repaired > 0 => {
                    tracing::warn!(repaired, "auto_scan_album_artists_repaired")
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "auto_scan_album_artist_repair_failed"),
            }
        }

        for album in album_repo.list(99999, 0).unwrap_or_default() {
            if let Some(id) = album.id {
                album_repo.update_track_count(id).ok();
                album_repo.update_quality_from_tracks(id).ok();
            }
        }

        // Clean up orphan albums with 0 tracks (ghost entries from
        // artist_id changes or interrupted scans) — bug #593.
        let orphan_albums = album_repo.delete_orphans().unwrap_or(0);
        if orphan_albums > 0 {
            info!(orphan_albums, "auto_scan_orphan_albums_cleaned");
        }

        // Réconciliation des favoris : le prune + orphan cleanup ci-dessus
        // peuvent avoir renouvelé les rowids d'albums/pistes favoris (racines
        // music déplacées — bug .18) ; on re-rattache par identité (instantané
        // titre/artiste/chemin, historique d'écoute en secours) et on ne
        // supprime un favori vraiment introuvable qu'après un scan complet
        // sain (aucune racine manquante/illisible, non annulé).
        {
            // `emptied_roots` inclus depuis #1943 : sans lui, une racine vidée
            // par un montage absent laissait passer la réconciliation, qui
            // supprimait définitivement les favoris. Irréversible.
            match tune_core::db::favorites_reconcile::FavoritesReconciler::with_backend(db.clone())
                .run(full_scan_ok)
            {
                Ok(fav_stats) if fav_stats.changed() > 0 || fav_stats.unresolved > 0 => {
                    info!(
                        scanned = fav_stats.scanned,
                        snapshots = fav_stats.snapshots_backfilled,
                        relinked = fav_stats.relinked,
                        deduplicated = fav_stats.deduplicated,
                        deleted = fav_stats.deleted,
                        unresolved = fav_stats.unresolved,
                        "auto_scan_favorites_reconciled"
                    );
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "auto_scan_favorites_reconcile_failed"),
            }
        }

        info!(
            total = stats.total_files,
            ok = stats.metadata_ok,
            failed = stats.metadata_failed,
            timeout = stats.metadata_timeout,
            inserted,
            updated,
            skipped,
            skipped_unchanged,
            skipped_duplicate,
            skipped_no_metadata,
            skipped_unsupported,
            db_insert_failed,
            db_update_failed,
            artwork = artwork_extracted,
            orphan_albums,
            "auto_scan_complete"
        );

        // Import any playlist files (.m3u/.m3u8/.pls) found in the library as
        // local playlists — same as the manual scan (Bertrand). Idempotent by
        // playlist name, so the startup scan re-running never duplicates them.
        let pl = tune_core::library::playlist_scan::import_local_playlists(&db, &music_dirs);
        if pl.playlists_created > 0 {
            event_bus.emit(
                "library.playlists.imported",
                serde_json::json!({ "playlists": pl.playlists_created, "tracks": pl.tracks_added }),
            );
        }

        // Mirror hand-made compilation folders (tracks spanning several albums)
        // into local playlists — opt-in via scan_folder_playlists (Frédéric).
        if tune_core::library::folder_playlists::folder_playlists_enabled(&db) {
            tune_core::library::folder_playlists::sync_folder_playlists(&db);
        }

        let report = serde_json::json!({
            "total_files": stats.total_files,
            "missing_dirs": missing_dirs.clone(),
            "missing_dir_reasons": missing_dir_reasons.clone(),
            "error_dirs": error_dirs.clone(),
            "metadata_ok": stats.metadata_ok,
            "metadata_failed": stats.metadata_failed,
            "metadata_timeout": stats.metadata_timeout,
            "inserted": inserted,
            "updated": updated,
            "skipped": skipped,
            "skipped_unchanged": skipped_unchanged,
            "skipped_duplicate": skipped_duplicate,
            "skipped_no_metadata": skipped_no_metadata,
            "skipped_unsupported": skipped_unsupported,
            "db_insert_failed": db_insert_failed,
            "db_update_failed": db_update_failed,
            "artwork_extracted": artwork_extracted,
            "failed_paths": stats.failed_paths,
            "skipped_unsupported_by_ext": skipped_by_ext,
            "skipped_unsupported_reasons": skipped_reasons,
        });

        let report_path = std::env::var("TUNE_DB_PATH")
            .unwrap_or_else(|_| "tune.db".into())
            .replace(".db", "-scan-report.json");
        if let Ok(json) = serde_json::to_string_pretty(&report) {
            std::fs::write(&report_path, json).ok();
        }

        event_bus.emit("library.scan.completed", report);
        scan_done_clone.store(true, Ordering::Release);
    });
    scan_done
}

/// Spawn the file watcher that monitors music directories for live changes.
///
/// If `wait_for_scan` is provided, the watcher will wait until the initial scan
/// completes before starting to monitor directories. This prevents the watcher
/// from picking up stale FSEvents replayed on subscription and racing with the
/// scanner (deleting tracks that the scanner just inserted).
/// Parse the `scan_exclude_paths` setting: a JSON array of case-insensitive
/// path substrings excluded from scanning and watching (staging folders,
/// backup trees, a sibling's library on a shared NAS).
pub(crate) fn scan_exclude_patterns(
    db: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
) -> Vec<String> {
    tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone())
        .get("scan_exclude_paths")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
}

/// How long to wait before re-checking a freshly-changed file's size. A file
/// still being written — a large copy in progress, a download, or a file
/// produced in real time — fires Create/Modify events while incomplete; scanning
/// it then
/// reads 0 bytes or a truncated FLAC (`scan_file_empty_skipped`) and churns a
/// burst of retry inserts. We defer until the size is non-zero and stable across
/// this window.
const WATCHER_SETTLE_RECHECK_MS: u64 = 400;

/// Split a batch of watcher changes into files ready to scan now vs. files still
/// being written (carried to the next cycle, ~2 s later). Deletes pass straight
/// through as ready. Excluded paths and Tune's own temp files are dropped (never
/// scanned, never deferred). An Added/Modified file is "settled" when its size
/// is non-zero and unchanged across a single `WATCHER_SETTLE_RECHECK_MS` recheck
/// — ONE sleep per batch regardless of how many files changed, so a burst never
/// blocks the loop per-file. A file that vanished between events is dropped.
fn settle_partition(
    changes: Vec<tune_core::scanner::watcher::FileChange>,
    excludes: &[String],
) -> (
    Vec<tune_core::scanner::watcher::FileChange>,
    Vec<tune_core::scanner::watcher::FileChange>,
) {
    use tune_core::scanner::watcher::ChangeType;
    let mut ready = Vec::new();
    let mut to_recheck: Vec<(tune_core::scanner::watcher::FileChange, u64)> = Vec::new();
    for change in changes {
        let path_l = change.path.to_lowercase();
        if !excludes.is_empty() && excludes.iter().any(|x| path_l.contains(x.as_str())) {
            continue;
        }
        if tune_core::scanner::is_tune_temp_file(std::path::Path::new(&change.path)) {
            continue;
        }
        if change.change_type == ChangeType::Deleted {
            ready.push(change);
            continue;
        }
        match std::fs::metadata(&change.path) {
            Ok(m) => to_recheck.push((change, m.len())),
            // Gone/unreadable between the event and now — a transient. Drop it;
            // if it reappears a fresh event will re-surface it.
            Err(_) => {}
        }
    }
    if to_recheck.is_empty() {
        return (ready, Vec::new());
    }
    std::thread::sleep(std::time::Duration::from_millis(WATCHER_SETTLE_RECHECK_MS));
    let mut pending = Vec::new();
    for (change, size1) in to_recheck {
        match std::fs::metadata(&change.path) {
            // Non-zero AND unchanged over the recheck window → writing has stopped.
            Ok(m) if m.len() > 0 && m.len() == size1 => ready.push(change),
            // Still zero, or grew during the window → keep writing; re-check next cycle.
            Ok(_) => pending.push(change),
            // Vanished during the recheck → drop.
            Err(_) => {}
        }
    }
    (ready, pending)
}

#[cfg(test)]
mod settle_tests {
    use super::settle_partition;
    use std::io::Write;
    use tune_core::scanner::watcher::{ChangeType, FileChange};

    fn ch(path: &str, t: ChangeType) -> FileChange {
        FileChange {
            change_type: t,
            path: path.to_string(),
        }
    }

    #[test]
    fn settles_stable_nonzero_defers_zero_drops_missing_and_excluded() {
        // NOT under the system temp dir: is_tune_temp_file() drops everything
        // there, which would (correctly) exclude the fixtures and mask the logic
        // under test. A unique dir relative to the test cwd (the crate root) is
        // resolved by fs::metadata but never matches starts_with(temp_dir()).
        let dir = std::path::PathBuf::from(format!(".settle_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let stable = dir.join("stable.flac");
        std::fs::File::create(&stable)
            .unwrap()
            .write_all(b"1234567890")
            .unwrap();
        let stable_p = stable.to_string_lossy().to_string();

        let empty = dir.join("empty.flac"); // zero bytes → still being written
        std::fs::File::create(&empty).unwrap();
        let empty_p = empty.to_string_lossy().to_string();

        let missing_p = dir.join("missing.flac").to_string_lossy().to_string(); // never created

        let changes = vec![
            ch(&stable_p, ChangeType::Added),
            ch(&empty_p, ChangeType::Added),
            ch(&missing_p, ChangeType::Added),
            ch("/lib/A_Sibling_excluded_dir/foo.flac", ChangeType::Added),
            ch(&stable_p, ChangeType::Deleted),
        ];
        let (ready, pending) = settle_partition(changes, &["excluded".to_string()]);

        // Stable non-zero Added is scanned now; Delete passes straight through.
        assert!(
            ready
                .iter()
                .any(|c| c.path == stable_p && c.change_type == ChangeType::Added)
        );
        assert!(ready.iter().any(|c| c.change_type == ChangeType::Deleted));
        // Zero-byte file is deferred, not scanned.
        assert!(pending.iter().any(|c| c.path == empty_p));
        assert!(!ready.iter().any(|c| c.path == empty_p));
        // Missing + excluded are dropped entirely (neither ready nor pending).
        for set in [&ready, &pending] {
            assert!(!set.iter().any(|c| c.path == missing_p));
            assert!(!set.iter().any(|c| c.path.contains("excluded")));
        }

        std::fs::remove_dir_all(&dir).ok();
    }
}

/// `event_bus` est ce qui manquait : le surveillant importait, et ne le disait
/// a personne. Voir l'emission de `library.updated` en fin de lot.
pub fn spawn_file_watcher(
    db: Arc<dyn DbBackend>,
    wait_for_scan: Option<Arc<AtomicBool>>,
    event_bus: Arc<tune_core::event_bus::EventBus>,
) {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone());
    let music_dirs: Vec<String> = settings
        .get("music_dirs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    if music_dirs.is_empty() {
        return;
    }

    // Normalized roots for the delete guard below — same normalization the
    // watcher applies internally.
    let guard_roots: Vec<String> = music_dirs
        .iter()
        .map(|d| tune_core::scanner::walker::normalize_path(d))
        .filter(|d| !d.is_empty())
        .collect();

    tokio::task::spawn_blocking(move || {
        // Wait for the initial auto-scan to complete before creating the
        // watcher. On macOS, FSEvents replays recent events when a new
        // watcher subscribes, which can cause the watcher to delete+reinsert
        // tracks that the scanner just added.
        if let Some(ref flag) = wait_for_scan {
            info!("file_watcher_waiting_for_scan");
            while !flag.load(Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            info!("file_watcher_scan_complete_starting_watch");
        }
        // FileWatcher::new can take MINUTES: for a network mount the poll
        // watcher's initial watch() walks the whole tree synchronously to
        // build its baseline (Pierre M: 6 min 43 for K:\ over SMB, the
        // server looked hung after sqlite_cache_warmed). It must run here,
        // on the blocking thread AFTER the startup scan — never on the
        // startup path.
        let mut watcher = match tune_core::scanner::watcher::FileWatcher::new(music_dirs) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(error = %e, "file_watcher_init_failed");
                return;
            }
        };
        info!("file_watcher_started");
        {
            // Always drain stale events before entering the watch loop.
            // On macOS, FSEvents replays recent events from the persistent
            // journal when a new stream is created, even with
            // kFSEventStreamEventIdSinceNow.  Give it 2 seconds to flush
            // (the default FSEvents coalescing latency) to avoid
            // reprocessing events that already happened before startup.
            std::thread::sleep(std::time::Duration::from_secs(2));
            let stale = watcher.poll_changes(std::time::Duration::from_millis(200));
            if !stale.is_empty() {
                info!(count = stale.len(), "file_watcher_drained_stale_events");
            }
            let watcher_excludes: Vec<String> = scan_exclude_patterns(&db)
                .iter()
                .map(|p| p.trim().to_lowercase())
                .filter(|p| !p.is_empty())
                .collect();
            let watcher_quality_split =
                tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone())
                    .get("quality_split")
                    .ok()
                    .flatten()
                    .map(|v| v != "false" && v != "0")
                    .unwrap_or(true);
            let mut liveness_tick: u32 = 0;
            // Files seen changed but still being written (a large copy in
            // progress, a download, a real-time producer): carried across cycles
            // until their size settles, so the final COMPLETE write is scanned.
            let mut pending_settle: Vec<tune_core::scanner::watcher::FileChange> = Vec::new();
            loop {
                // Every ~2 min (each idle iteration blocks ~2s): re-watch
                // roots that appeared or came back after an unmount, and
                // drop dead watches. A NAS mounted after boot used to stay
                // invisible to live updates until a server restart.
                liveness_tick = liveness_tick.wrapping_add(1);
                if liveness_tick % 60 == 0 {
                    watcher.ensure_watches();
                }
                let mut changes = watcher.poll_debounced(
                    std::time::Duration::from_secs(2),
                    std::time::Duration::from_millis(500),
                );
                // Re-examine files that were still being written last cycle, then
                // split off any that are STILL growing (or zero-byte) so we scan
                // only complete files — no more 0-byte/truncated snapshots of a
                // file captured mid-write. One recheck sleep for the whole batch.
                changes.append(&mut pending_settle);
                let (changes, still_writing) = settle_partition(changes, &watcher_excludes);
                pending_settle = still_writing;
                let had_changes = !changes.is_empty();
                for change in changes {
                    // Same exclusions as the scans (re-read per event batch
                    // so setting edits apply without a restart is overkill;
                    // the list was read once at watcher start).
                    if !watcher_excludes.is_empty() {
                        let path_l = change.path.to_lowercase();
                        if watcher_excludes.iter().any(|x| path_l.contains(x.as_str())) {
                            continue;
                        }
                    }
                    // Tune's own streaming temp files (tune-stream-*/
                    // tune-prefetch-* in %TEMP%) fire watcher events on every
                    // transcode when the library root is a parent of the temp
                    // dir — 119 ghost scans in 2 minutes on Frédéric's setup,
                    // degrading the first seconds of each streaming play.
                    if tune_core::scanner::is_tune_temp_file(std::path::Path::new(&change.path)) {
                        continue;
                    }
                    match change.change_type {
                        tune_core::scanner::watcher::ChangeType::Added
                        | tune_core::scanner::watcher::ChangeType::Modified => {
                            // Unchanged-file guard (Jean Marie: "le scan tourne
                            // en boucle", macOS Ventura). A Modified event whose
                            // on-disk mtime+size still match the stored row is a
                            // self-induced event: reading a file to import it
                            // makes macOS write an extended attribute, which
                            // fires another Modify event → re-read → infinite
                            // loop. Detect it with a cheap stat and skip —
                            // crucially WITHOUT reading the content (scan_files_
                            // parallel), since the read is what re-triggers it.
                            if change.change_type
                                == tune_core::scanner::watcher::ChangeType::Modified
                            {
                                if let Ok(Some(existing)) =
                                    TrackRepo::with_backend(db.clone()).get_by_path(&change.path)
                                {
                                    if let Ok(fs_meta) = std::fs::metadata(&change.path) {
                                        let fs_size = fs_meta.len() as i64;
                                        let fs_mtime = fs_meta
                                            .modified()
                                            .ok()
                                            .and_then(|t| {
                                                t.duration_since(std::time::UNIX_EPOCH).ok()
                                            })
                                            .map(|d| d.as_secs() as f64);
                                        let unchanged =
                                            existing.file_size.map_or(false, |s| s == fs_size)
                                                && match (existing.file_mtime, fs_mtime) {
                                                    (Some(a), Some(b)) => (a - b).abs() <= 0.5,
                                                    _ => false,
                                                };
                                        if unchanged {
                                            tracing::debug!(path = %change.path, "watcher_skip_unchanged");
                                            continue;
                                        }
                                    }
                                }
                            }
                            let files: Vec<std::path::PathBuf> =
                                vec![std::path::PathBuf::from(&change.path)];
                            let (scanned, _) =
                                tune_core::scanner::walker::scan_files_parallel(&files, true, None);
                            let track_repo = TrackRepo::with_backend(db.clone());
                            let artist_repo = ArtistRepo::with_backend(db.clone());
                            let album_repo = AlbumRepo::with_backend(db.clone());

                            for sf in &scanned {
                                if sf.metadata.is_none() {
                                    continue;
                                }

                                if change.change_type
                                    == tune_core::scanner::watcher::ChangeType::Modified
                                {
                                    track_repo.delete_by_path(&sf.path).ok();
                                }

                                // Decide compilation over the whole folder from
                                // the siblings already in the DB, so re-importing
                                // a single file (MP3tag save → Modified event)
                                // doesn't split a various-artists album tagged
                                // with per-track album_artist into one album per
                                // artist (JP Borderies). The manual/batch scan
                                // sees the whole album at once; the watcher sees
                                // one file, so it reconstructs the folder view
                                // from the DB. Any doubt → None → per-file
                                // self-decide (previous behaviour, no regression).
                                let comp_override: Option<bool> =
                                    sf.metadata.as_ref().and_then(|meta| {
                                        let dir = std::path::Path::new(&sf.path).parent()?;
                                        let mut comp = meta.compilation;
                                        let mut artists: std::collections::HashSet<String> =
                                            std::collections::HashSet::new();
                                        let mut note = |aa: Option<&str>| {
                                            if let Some(a) =
                                                aa.map(str::trim).filter(|s| !s.is_empty())
                                            {
                                                if crate::scan_import::is_various_artists(a) {
                                                    comp = true;
                                                }
                                                artists.insert(a.to_lowercase());
                                            }
                                        };
                                        note(meta.album_artist.as_deref());
                                        let siblings = track_repo
                                            .siblings_album_artists(&dir.to_string_lossy())
                                            .ok()?;
                                        for (fp, aa) in &siblings {
                                            // Direct children only (exclude
                                            // sub-folders sharing the prefix).
                                            if std::path::Path::new(fp).parent() != Some(dir) {
                                                continue;
                                            }
                                            note(aa.as_deref());
                                        }
                                        Some(comp || artists.len() >= 2)
                                    });
                                let Some((track, album_id)) = build_track_from_metadata_opts(
                                    sf,
                                    &artist_repo,
                                    &album_repo,
                                    watcher_quality_split,
                                    comp_override,
                                ) else {
                                    tracing::warn!(path = %sf.path, "watcher_track_skipped_no_metadata");
                                    continue;
                                };

                                // Skip duplicate: same audio content already in this album
                                if let (Some(hash), Some(aid)) = (&track.audio_hash, album_id) {
                                    if track_repo
                                        .exists_by_audio_hash_and_album(hash, aid)
                                        .unwrap_or(false)
                                    {
                                        tracing::debug!(
                                            audio_hash = %hash,
                                            album_id = aid,
                                            path = %sf.path,
                                            "watcher_skip_duplicate_audio_hash"
                                        );
                                        continue;
                                    }
                                }

                                if let Some(aid) = album_id {
                                    let cache_dir = crate::routes::library::artwork_cache_dir();
                                    if let Some(hash) = tune_core::library::artwork::get_or_extract(
                                        std::path::Path::new(&sf.path),
                                        &cache_dir,
                                    ) {
                                        album_repo.update_cover_path(aid, &hash).ok();
                                    }
                                    album_repo.update_track_count(aid).ok();
                                    album_repo.update_quality_from_tracks(aid).ok();
                                }

                                if track_repo.create(&track).is_ok() {
                                    info!(path = %sf.path, "watcher_track_added");
                                }
                            }
                        }
                        tune_core::scanner::watcher::ChangeType::Deleted => {
                            // NEVER delete tracks because a mount dropped:
                            // when a NAS goes away, the whole subtree fires
                            // Remove events (and the poll watcher for
                            // network mounts sees every file "vanish").
                            // If the owning music root is unreadable, the
                            // files are unreachable — not deleted.
                            if std::path::Path::new(&change.path).exists() {
                                tracing::debug!(path = %change.path, "watcher_delete_ignored_file_still_present");
                                continue;
                            }
                            if let Some(root) = guard_roots
                                .iter()
                                .find(|r| change.path.starts_with(r.as_str()))
                                && std::fs::read_dir(root).is_err()
                            {
                                tracing::warn!(
                                    path = %change.path,
                                    root = %root,
                                    "watcher_delete_skipped_root_unreachable — mount dropped, keeping tracks"
                                );
                                continue;
                            }
                            let track_repo = TrackRepo::with_backend(db.clone());
                            if track_repo.delete_by_path(&change.path).is_ok() {
                                info!(path = %change.path, "watcher_track_removed");
                            }
                        }
                    }
                }
                // After a batch, remove any album left with 0 tracks. An
                // incremental re-import can re-point a track to a new album
                // row (album_artist tag drift) and leave the old row as a
                // cover-only ghost — eric: "une fois avec les pistes, une
                // autre fois juste la pochette". The manual scan cleans
                // these; the watcher never did.
                if had_changes {
                    let album_repo = AlbumRepo::with_backend(db.clone());
                    let cleaned = album_repo.delete_orphans().unwrap_or(0);
                    if cleaned > 0 {
                        info!(cleaned, "watcher_orphan_albums_cleaned");
                    }

                    // DIRE que la bibliotheque a change.
                    //
                    // Le surveillant importait en silence : il ne recevait meme
                    // pas le bus d'evenements, il ne POUVAIT donc rien annoncer.
                    // Les listes du client restaient telles quelles, et il
                    // fallait changer d'onglet puis revenir pour voir arriver
                    // les albums qu'on venait de deposer — c'est mot pour mot
                    // le contournement que Patatorz decrit (fil forum #1517).
                    //
                    // Un evenement PROPRE, et non `library.scan.completed` :
                    // celui-la fait afficher au client une banniere « prete »,
                    // qui n'aurait aucun sens a chaque fichier depose. Ici on
                    // veut seulement que les listes se rechargent.
                    event_bus.emit(
                        tune_core::event_types::EventType::LibraryUpdated.as_str(),
                        serde_json::json!({ "source": "watcher" }),
                    );
                    info!("watcher_library_updated_emis");
                }
            }
        }
    });
}
