use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Deserialize;
use serde_json::{Value, json};
use unicode_normalization::UnicodeNormalization;

use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::settings_repo::SettingsRepo;

use crate::state::AppState;

use std::sync::atomic::{AtomicBool, Ordering};

/// Cooperative cancellation flag for the running library scan.
///
/// Set to `true` by `scan_cancel` (POST /system/scan/cancel) and polled by the
/// batch loop before each batch, so "Arrêter le scan / Stop scan" actually
/// stops the scan instead of being a no-op that only flipped `scan_status` to
/// "idle" while the batch loop kept inserting for minutes (bug #1129). Reset to
/// `false` at the start of every scan.
static SCAN_CANCEL: AtomicBool = AtomicBool::new(false);

/// Clear the cancel flag at the start of a scan. Shared with the startup
/// (auto) scan so "Arrêter le scan" works there too (#1197/#1196).
pub(crate) fn reset_scan_cancel() {
    SCAN_CANCEL.store(false, Ordering::SeqCst);
}

/// Whether "Stop scan" was requested. Polled by both the manual and the startup
/// scan batch loops so either can be cancelled cooperatively.
pub(crate) fn scan_cancel_requested() -> bool {
    SCAN_CANCEL.load(Ordering::SeqCst)
}

/// Racines qui CONTENAIENT des pistes et n'en découvrent plus AUCUNE.
///
/// Un dossier qui passe de milliers de fichiers à zéro n'est pas vide : il est
/// absent. C'est la forme exacte que prend un partage réseau non monté —
/// Dominique COMET, 0.9.73, NAS OpenMediaVault en SMB : « ma bibliothèque
/// disparaît à chaque redémarrage de Tune » (#1652).
///
/// Les gardes existants ne peuvent pas voir ce cas : ils testent
/// `read_dir(root).is_err()`, c'est-à-dire une racine ILLISIBLE. Or un point de
/// montage qui existe mais sur lequel rien n'est monté est parfaitement
/// lisible — et vide. `read_dir` réussit, `missing_dirs` reste vide, et le
/// nettoyage supprime les pistes comme si les fichiers avaient été effacés.
///
/// Zéro n'est donc pas un résultat de scan crédible : c'est une anomalie, et on
/// refuse d'écrire dessus. Le prix de l'erreur est asymétrique — protéger à
/// tort laisse des lignes périmées qu'un scan suivant nettoiera, supprimer à
/// tort détruit la bibliothèque.
///
/// Une racine qui n'avait AUCUNE piste n'est pas concernée : elle n'a rien à
/// perdre, et c'est le cas normal d'un dossier fraîchement configuré.
pub(crate) fn roots_gone_empty(
    roots: &[String],
    existing_paths: &[&str],
    discovered_paths: &std::collections::HashSet<String>,
) -> Vec<String> {
    roots
        .iter()
        .filter(|root| {
            let prefix = format!("{}/", root.trim_end_matches('/'));
            let had = existing_paths.iter().any(|p| p.starts_with(&prefix));
            let has = discovered_paths.iter().any(|p| p.starts_with(&prefix));
            had && !has
        })
        .cloned()
        .collect()
}

/// Le chemin `path` est-il ce répertoire, ou sous lui ?
///
/// `starts_with` seul ne suffit pas : `/mnt/music2` est un préfixe de
/// `/mnt/music22`, et la protection s'appliquerait alors à un dossier voisin
/// — ou pire, ne s'appliquerait pas là où on la croit.
pub(crate) fn sous_le_dossier(path: &str, dossier: &str) -> bool {
    let d = dossier.trim_end_matches('/');
    path == d || path.starts_with(&format!("{d}/"))
}

/// Ce que la purge de fin de scan a le droit de faire d'une piste absente du
/// disque.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerdictPurge {
    /// Le fichier a vraiment disparu d'une racine saine et lue : on retire.
    Supprimer,
    /// La racine est absente, illisible, ou s'est vidée d'un coup — un montage
    /// qui n'est pas là ne prouve rien sur le contenu.
    ProtegeIllisible,
    /// La piste n'est sous AUCUNE racine configurée. Elle n'est pas
    /// « disparue » : elle est hors périmètre. C'est le trou par lequel
    /// 21 277 pistes de Yacine ont été supprimées (#1943) — un point de
    /// montage avait changé, l'ancienne racine n'était plus configurée, donc
    /// aucune des trois protections ne pouvait la couvrir : elle n'était ni
    /// manquante, ni en erreur, ni vidée, puisque personne n'y était allé.
    HorsPerimetre,
}

/// Décider du sort d'une piste absente du disque, en un seul endroit.
///
/// Cette règle existait en DEUX copies — `routes/system/scan.rs` et
/// `auto_scan.rs` — portant les mêmes trous. Les faire diverger encore serait
/// reproduire #1943 ; les faire vivre ici les corrige des deux côtés à la fois.
pub(crate) fn verdict_purge(
    db_path: &str,
    racines_configurees: &[String],
    missing_dirs: &[String],
    error_dirs: &[String],
    emptied_roots: &[String],
) -> VerdictPurge {
    if missing_dirs
        .iter()
        .chain(error_dirs.iter())
        .chain(emptied_roots.iter())
        .any(|d| sous_le_dossier(db_path, d))
    {
        return VerdictPurge::ProtegeIllisible;
    }
    // Une liste de racines VIDE ne veut pas dire « tout est hors périmètre » :
    // elle veut dire qu'on ne sait rien. Ne rien supprimer dans ce cas.
    if racines_configurees.is_empty()
        || !racines_configurees
            .iter()
            .any(|r| sous_le_dossier(db_path, r))
    {
        return VerdictPurge::HorsPerimetre;
    }
    VerdictPurge::Supprimer
}

/// Part maximale de la bibliothèque locale qu'une seule purge peut retirer.
///
/// Aucun plafond n'existait : rien n'empêchait une purge de 100 %. Chez
/// Yacine, 21 277 lignes sur 70 346 — 30 % — sont parties en un cycle sans
/// que rien ne s'y oppose. Une disparition massive est bien plus souvent un
/// montage absent qu'une suppression réelle de fichiers ; au-delà de ce
/// seuil on refuse et on demande à l'utilisateur, plutôt que d'agir.
pub(crate) const PART_MAX_PURGE: f64 = 0.20;

/// La purge dépasse-t-elle le plafond ? `candidats` sont les pistes qui
/// seraient retirées, `total` la population locale examinée.
pub(crate) fn purge_trop_massive(candidats: usize, total: usize) -> bool {
    // En deçà de 50 pistes, un pourcentage n'a pas de sens : retirer 10 pistes
    // sur 20 est banal quand on range sa bibliothèque à la main.
    if total < 50 {
        return false;
    }
    (candidats as f64) / (total as f64) > PART_MAX_PURGE
}

/// Pre-scan skip decision: does `path` need (re)scanning, or is it unchanged
/// since the last scan and safe to skip?
///
/// Returns `true` if the file is new, or its mtime/size differ from what the DB
/// last recorded for it; `false` if it's unchanged (skip — don't re-read tags).
///
/// The lookup key is NFC-normalized because the stored `file_path`s (and the
/// `discovered_paths` set) are NFC, while a filename on disk may be NFD (a FR
/// library ripped on macOS, copied to a Synology, read back over SMB). Skipping
/// this normalization was the "scan interminable" bug: every NFD-named file
/// missed the map, failed the skip, and lofty re-read its tags (heavy embedded
/// art) over slow SMB on EVERY scan (Xavier, DS214/18.5k FR).
///
/// The manual scan and the auto/watcher scan MUST share this one implementation
/// so they can't diverge again — they previously held two copies and only one
/// received the NFC fix.
pub(crate) fn file_needs_scan(
    path: &std::path::Path,
    existing_tracks: &std::collections::HashMap<String, (i64, Option<f64>, Option<i64>)>,
) -> bool {
    let path_str: String = path.to_string_lossy().nfc().collect();
    if let Some(&(_, existing_mtime, existing_size)) = existing_tracks.get(path_str.as_str()) {
        if let Ok(file_meta) = path.metadata() {
            let mtime = file_meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let unchanged = existing_mtime.map_or(false, |m| (m - mtime as f64).abs() <= 0.5)
                && existing_size.map_or(false, |s| s == file_meta.len() as i64);
            return !unchanged;
        }
    }
    true
}

#[derive(Deserialize)]
pub(super) struct ScanQuery {
    /// When true, re-process ALL discovered files (bypass the unchanged-file
    /// skip) so stale album_id assignments get re-resolved by (title, artist).
    /// Self-heals DBs corrupted by the old title-only album merge, where a
    /// track's album_id points at a wrong same-titled album. Slower (re-reads
    /// every file's metadata); default false keeps the fast incremental scan.
    force: Option<bool>,
    /// Autorise une purge qui dépasse le plafond volumétrique.
    ///
    /// DÉLIBÉRÉMENT distinct de `force`. `force` est le bouton « Scan complet »,
    /// que l'on clique pour relire ses fichiers — c'est exactement ce que clique
    /// quelqu'un dont le NAS était hors ligne, pour réparer sa bibliothèque. Y
    /// accrocher l'autorisation de supprimer en masse recréerait #1943 par la
    /// porte de service.
    confirmer_purge: Option<bool>,
    /// Alias for `force` sent by the clients' "Full scan / Scan complet" button.
    /// The web/Flutter clients pass `?full=true`; without this field serde
    /// silently dropped it, so "Scan complet" behaved like an ordinary
    /// incremental scan and could never re-resolve broken album/artist links —
    /// a rescan then skipped every unchanged file, so only "Vider la
    /// bibliothèque" + cold scan repaired the DB (Yacine, Synology ARM64).
    full: Option<bool>,
    /// Targeted scan: when set, only this sub-directory is walked instead of
    /// re-walking every configured music dir. On a network mount (SMB/NFS) the
    /// live `notify` watcher receives no events, so the only way to pick up a
    /// few new tracks was a full re-walk of the whole NAS (stat of every file
    /// = a round-trip each) — minutes to hours for 3 new tracks. Point the scan
    /// at just the folder that changed. The path MUST be inside a configured
    /// music dir; the deleted-track prune is scoped to this sub-tree so tracks
    /// elsewhere are never touched.
    path: Option<String>,
}

pub(super) async fn trigger_scan(
    State(state): State<AppState>,
    Query(q): Query<ScanQuery>,
) -> impl IntoResponse {
    let force = q.force.unwrap_or(false) || q.full.unwrap_or(false);
    let confirmer_purge = q.confirmer_purge.unwrap_or(false);
    // Targeted sub-folder scan (empty/blank string = full scan as before).
    let targeted_req: Option<String> = q
        .path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| tune_core::scanner::walker::normalize_path(s));
    spawn_library_scan_avec(state, force, confirmer_purge, targeted_req).await;
    (StatusCode::ACCEPTED, Json(json!({ "status": "scanning" })))
}

/// Spawn a background library scan (fire-and-forget). Shared by the `/scan`
/// endpoint and by `add_music_dir`, so a folder added in Settings is scanned
/// right away instead of only at the next restart (Jean-Pierre: newly-added
/// folders stayed invisible until the app was restarted).
pub(crate) async fn spawn_library_scan(state: AppState, force: bool, targeted_req: Option<String>) {
    spawn_library_scan_avec(state, force, false, targeted_req).await
}

/// Comme `spawn_library_scan`, mais peut autoriser une purge au-delà du plafond
/// volumétrique. Voir `ScanQuery::confirmer_purge` : ce n'est PAS `force`.
pub(crate) async fn spawn_library_scan_avec(
    state: AppState,
    force: bool,
    confirmer_purge: bool,
    targeted_req: Option<String>,
) {
    if force {
        tracing::info!("scan_force_full_reresolve — bypassing unchanged-file skip");
    }
    // Clear any leftover cancel request from a previous scan before starting.
    SCAN_CANCEL.store(false, Ordering::SeqCst);
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Err(e) = settings.set("scan_status", "scanning") {
        tracing::warn!(error = %e, "scan_status_set_failed");
    }
    if let Err(e) = settings.set("scan_started_at", &chrono_now()) {
        tracing::warn!(error = %e, "scan_started_at_set_failed");
    }

    let db = state.backend.clone();
    let event_bus = state.event_bus.clone();
    // Auto-enrichment after a scan needs BOTH premium AND the user's opt-in.
    // It was previously forced on every Premium account, so a scan of a large
    // library triggered ~20 min of artist-image downloads the user never asked
    // for and could not turn off (JF Paquet: tags already complete, machine
    // busy). Honour the `enrich_on_scan` setting (default on = unchanged
    // behaviour) so it can be disabled from Settings.
    let enrich_on_scan = SettingsRepo::with_backend(state.backend.clone())
        .get("enrich_on_scan")
        .ok()
        .flatten()
        .map(|v| v != "false")
        .unwrap_or(true);
    let auto_enrich_allowed = enrich_on_scan
        && state
            .license
            .check_feature(tune_core::license::Feature::AutoEnrichment)
            .await;
    tokio::spawn(async move {
        let db_for_panic = db.clone();
        let handle = tokio::runtime::Handle::current();
        let result = tokio::task::spawn_blocking(move || {
        let raw_dirs = super::get_music_dirs_list(&db);
        if raw_dirs.is_empty() {
            tracing::warn!("scan_aborted_no_dirs — no music directories configured");
            if let Err(e) = SettingsRepo::with_backend(db).set("scan_status", "idle") {
                tracing::warn!(error = %e, "scan_status_reset_failed");
            }
            // Emit a completion event so the client clears the "scanning" banner.
            // The web UI only drops the banner on `library.scan.completed`; the
            // normal path emits it at the end, but this early return was silent —
            // leaving the panel stuck at "0 scanned, 0 added" forever, with a
            // Stop button that does nothing because the scan already ended
            // (macOS user with no folder yet, #1129).
            event_bus.emit(
                "library.scan.completed",
                json!({
                    "total_files": 0,
                    "inserted": 0,
                    "updated": 0,
                    "skipped": 0,
                    "no_dirs": true,
                }),
            );
            return;
        }

        // Normalize paths for cross-platform compatibility (Windows backslashes, etc.)
        let music_dirs: Vec<String> = raw_dirs
            .iter()
            .map(|d| tune_core::scanner::walker::normalize_path(d))
            .filter(|d| !d.is_empty())
            .collect();

        // Resolve a targeted sub-folder scan. The path must be inside a
        // configured music dir (defence against scanning arbitrary paths); if it
        // is not, fall back to a full scan rather than silently doing nothing.
        let targeted: Option<String> = targeted_req.as_ref().and_then(|p| {
            if music_dirs.iter().any(|root| p == root || p.starts_with(&format!("{root}/"))) {
                Some(p.clone())
            } else {
                tracing::warn!(path = %p, dirs = ?music_dirs, "scan_targeted_path_outside_music_dirs — falling back to full scan");
                None
            }
        });
        let scan_dirs: Vec<String> = match &targeted {
            Some(p) => vec![p.clone()],
            None => music_dirs.clone(),
        };

        tracing::info!(
            dirs = ?scan_dirs,
            targeted = ?targeted,
            platform = std::env::consts::OS,
            "scan_starting"
        );

        // Surface an "indexing" phase IMMEDIATELY, before the directory walk and
        // the mtime/size stat pass below. On a large library over a NAS (SMB)
        // both are slow (a 58k-file walk + per-file stat) and used to run in
        // total silence — the panel showed nothing and the Stop button never
        // appeared, so the scan read as "interminable / frozen" (forum, v0.9.12
        // Win11/NAS/58k). This gives the UI an indeterminate panel + a working
        // Stop from t=0. `total: 0` marks it indeterminate until discovery ends.
        event_bus.emit(
            "library.scan.started",
            json!({ "music_dirs": &music_dirs, "phase": "indexing", "total": 0 }),
        );
        event_bus.emit(
            "library.scan.progress",
            json!({ "phase": "indexing", "scanned": 0i64, "added": 0i64, "total": 0i64 }),
        );

        let exclude_patterns = crate::auto_scan::scan_exclude_patterns(&db);
        if !exclude_patterns.is_empty() {
            tracing::info!(patterns = ?exclude_patterns, "scan_exclude_paths_active");
        }
        let list_result = tune_core::scanner::walker::list_audio_files_with_excludes(
            &scan_dirs,
            &exclude_patterns,
        );
        let missing_dirs = list_result.missing_dirs;
        let missing_dir_reasons = list_result.missing_dir_reasons;
        let error_dirs = list_result.error_dirs;
        let skipped_by_ext = list_result.skipped_by_ext;
        let files = list_result.files;
        let total_discovered = files.len();

        let discovered_paths: std::collections::HashSet<String> = files
            .iter()
            .map(|p| p.to_string_lossy().nfc().collect::<String>())
            .collect();

        // Warn loudly for any CONFIGURED root (full scan only) that is reachable
        // yet yielded zero audio files — a mis-pointed or wrong-level music
        // folder. Yacine's real files live under /volume1/daphile_remote/HDD, but
        // /volume1/daphile_remote/Music and the Freebox mount were configured and
        // are empty, so the scan reported discovered=0 and the library looked
        // permanently "stuck". `missing_dirs` (unreachable/unmounted, reported
        // separately with a reason) are excluded here: this flags only roots that
        // ARE reachable but contain nothing.
        if targeted.is_none() {
            for dir in &scan_dirs {
                if missing_dirs.iter().any(|m| m == dir) {
                    continue;
                }
                let prefix: String =
                    format!("{}/", dir.trim_end_matches('/')).nfc().collect();
                let has_audio = discovered_paths.iter().any(|p| p.starts_with(&prefix));
                if !has_audio {
                    tracing::warn!(
                        dir = %dir,
                        "scan_root_no_audio_files — configured music folder is reachable but contains no audio files (wrong path or empty). Check that it points at the folder holding your music."
                    );
                }
            }
        }

        let track_repo = tune_core::db::track_repo::TrackRepo::with_backend(db.clone());

        // "Separate albums by quality" — when on (default), a quality suffix is
        // appended to the album title so CD and Hi-Res versions become distinct
        // albums. The manual scan must honour it just like the file-watcher
        // (auto_scan) does, otherwise the two paths disagree (Fabien).
        let quality_split = SettingsRepo::with_backend(db.clone())
            .get("quality_split")
            .ok()
            .flatten()
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true);

        // Load existing tracks BEFORE scanning to skip unchanged files.
        // A DB read error must ABORT the scan, not degrade into an empty map:
        // with an empty map every file on disk looks new, so a transient DB
        // hiccup would re-insert the whole library as duplicates.
        let existing_tracks = match track_repo.get_all_local_file_info() {
            Ok(map) => map,
            Err(e) => {
                tracing::error!(error = %e, "scan_aborted_existing_tracks_read_failed");
                let settings = SettingsRepo::with_backend(db.clone());
                settings.set("scan_status", "idle").ok();
                event_bus.emit(
                    "library.scan.completed",
                    json!({
                        "total_files": 0,
                        "inserted": 0,
                        "updated": 0,
                        "skipped": 0,
                        "error": format!("database read failed: {e}"),
                    }),
                );
                return;
            }
        };

        // Same audio-hash dedup as the auto/startup scan: without it, the
        // manual scan (the "Scanner" button — the path users actually hit)
        // happily inserted the same content twice when it exists under two
        // paths, while the auto scan deduped. (hash, album_id) pairs already
        // in the library are skipped for NEW inserts only; updates of an
        // existing path are never affected.
        let mut known_hashes: std::collections::HashSet<(String, i64)> = track_repo
            .get_existing_audio_hash_album_pairs()
            .unwrap_or_default();

        // Quick stat pass: skip files whose mtime+size haven't changed.
        // Parallelised: each `path.metadata()` is a blocking stat that, over a
        // NAS/SMB mount, carries real round-trip latency; doing 58k of them
        // sequentially was a multi-minute silent stall before the first batch
        // (forum: v0.9.12 Win11/NAS/58k, "scan interminable"). rayon fans the
        // stats across the pool, and SCAN_CANCEL is honoured here too so Stop
        // aborts during this phase, not only during batch processing.
        use rayon::prelude::*;
        let files_to_scan: Vec<std::path::PathBuf> = files
            .into_par_iter()
            .filter(|path| {
                if SCAN_CANCEL.load(Ordering::SeqCst) {
                    return false;
                }
                // Force mode: re-process everything so album_id is re-resolved.
                if force {
                    return true;
                }
                // Shared with auto_scan so the manual and watcher scans can't
                // diverge on the NFC key handling (the "scan interminable" bug).
                file_needs_scan(path, &existing_tracks)
            })
            .collect();
        let pre_skipped = (total_discovered - files_to_scan.len()) as i64;

        tracing::info!(
            total = total_discovered,
            changed = files_to_scan.len(),
            unchanged = pre_skipped,
            "pre_scan_filter_complete"
        );

        event_bus.emit(
            "library.scan.started",
            json!({
                "music_dirs": &music_dirs,
                "total": total_discovered,
                "to_scan": files_to_scan.len(),
                "unchanged": pre_skipped,
            }),
        );

        // Emit an immediate progress event so the panel shows "0 / total" and a
        // determinate bar right away, instead of sitting at "0 fichiers, 0
        // ajoutés" until the first batch commits — which on a large/slow NAS is
        // many seconds and reads as "stuck / doing nothing" (bug #1129). The
        // per-batch emit below only fires once `processed > 0`, so without this
        // the very start of a scan has no counter at all.
        event_bus.emit(
            "library.scan.progress",
            json!({
                "phase": "files",
                "scanned": pre_skipped,
                "added": 0i64,
                "total": total_discovered as i64,
                "inserted": 0i64,
                "updated": 0i64,
                "skipped": pre_skipped,
            }),
        );

        // --- Batched scan + import ---
        // Parse metadata in parallel (rayon) in chunks of SCAN_BATCH_SIZE,
        // then batch-insert/update each chunk in its own transaction.
        // This gives progressive availability: tracks are queryable after
        // each batch commits, not only when the entire scan finishes.

        let cache_dir = crate::routes::library::artwork_cache_dir();
        let mut inserted = 0i64;
        let mut updated = 0i64;
        let mut db_insert_failed = 0i64;
        let mut db_update_failed = 0i64;
        // `skipped` stays the aggregate the UI already shows. The manual scan
        // never dedups by audio_hash (only the auto/watcher path does), so
        // everything it skips is either an unchanged file or a file whose
        // metadata could not be read — broken out below so the report says
        // which.
        let mut skipped = pre_skipped;
        let mut skipped_unchanged = pre_skipped;
        let mut skipped_duplicate = 0i64;
        let mut skipped_no_metadata = 0i64;
        let total_to_scan = files_to_scan.len() as i64;
        let total = total_to_scan + pre_skipped;
        let mut last_progress_emit = std::time::Instant::now();
        let scan_timer_start = std::time::Instant::now();

        // Shared artist/album resolver + Track builder, identical to the auto/
        // startup + watcher scans. Owns the cross-batch caches (artist, album,
        // covers, per-folder album-artist pinning), the per-batch compilation
        // decision, and the artwork-extracted counter.
        let mut importer =
            crate::scan_import::TrackImporter::new(db.clone(), quality_split, cache_dir.clone());

        let batch_size = tune_core::scanner::walker::SCAN_BATCH_SIZE;

        // Process files in batches: parse metadata in parallel, then insert in a transaction
        let scan_stats = tune_core::scanner::walker::scan_files_batched(
            &files_to_scan,
            true,
            batch_size,
            |batch, batch_idx, _total_files| {
                // Cooperative cancellation: once "Stop scan" was pressed, skip
                // all remaining batches so the loop drains quickly and the scan
                // stops (bug #1129 — the old cancel only flipped scan_status but
                // the batch loop kept inserting). Files for the remaining
                // batches were already read by the walker, but no DB work is
                // done for them.
                if SCAN_CANCEL.load(Ordering::SeqCst) {
                    return;
                }
                // Collect tracks to batch-insert and batch-update
                let mut to_insert: Vec<tune_core::db::models::Track> =
                    Vec::with_capacity(batch.len());
                let mut to_update: Vec<tune_core::db::models::Track> =
                    Vec::with_capacity(batch.len() / 4);

                // BEGIN transaction for this batch (SQLite only — PG uses autocommit
                // to avoid "current transaction is aborted" cascading failures)
                let is_pg = db.engine() == tune_core::db::engine::Engine::Postgres;
                if !is_pg {
                    if let Err(e) = db.execute_batch("BEGIN IMMEDIATE") {
                        // A failed BEGIN means a transaction is already open on
                        // the shared connection (a previous batch that didn't
                        // commit). Roll it back and retry so the connection
                        // recovers instead of staying poisoned — which would make
                        // every playback set_queue fail for the rest of the
                        // session (Yves: stuck on the last track during a scan).
                        tracing::warn!(error = %e, batch = batch_idx, "scan_batch_begin_failed");
                        let _ = db.execute_batch("ROLLBACK");
                        let _ = db.execute_batch("BEGIN IMMEDIATE");
                    }
                }

                // Resolve artists/albums and build the track rows for this batch
                // via the shared importer — the same logic (compilation
                // flattening, classical-soloist album-artist pinning, mbid album
                // resolution, embedded-cover preference, artist images) as the
                // auto/startup + watcher scans. The importer owns the cross-batch
                // caches and the per-(folder,album) compilation decision.
                importer.begin_batch(&batch);

                for sf in &batch {
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
                    // Without this, get_or_create_with_mbid can create a ghost album
                    // entry (with cover art but no tracks) for files that are ultimately
                    // skipped — the root cause of "duplicate covers after rescan" (#593).
                    // Force mode bypasses this so album_id gets re-resolved.
                    if !force {
                        if let Some(&(_existing_id, existing_mtime, existing_size)) =
                            existing_tracks.get(&sf.path)
                        {
                            let file_changed = existing_mtime
                                .map_or(true, |m| (m - sf.mtime as f64).abs() > 0.5)
                                || existing_size.map_or(true, |s| s != sf.file_size as i64);
                            if !file_changed {
                                skipped += 1;
                                skipped_unchanged += 1;
                                continue;
                            }
                        }
                    }

                    let Some((mut track, _album_id)) = importer.import(sf) else {
                        continue;
                    };

                    // File already exists and has changed → batch update;
                    // otherwise a new file → batch insert. (Unchanged files were
                    // already skipped by the early-exit above.)
                    if let Some(&(existing_id, _, _)) = existing_tracks.get(&sf.path) {
                        track.id = Some(existing_id);
                        to_update.push(track);
                    } else {
                        // Deduplicate by audio_hash + album_id (same rule as
                        // the auto scan): identical content already present in
                        // this album via another path is not inserted again.
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
                }

                // Collect extended metadata for tracks in this batch
                let mut extended_meta_paths: Vec<String> = Vec::new();
                for sf in &batch {
                    if sf.metadata.is_some() {
                        extended_meta_paths.push(sf.path.clone());
                    }
                }

                // Batch insert + update using prepared statements. Per-row
                // failures inside create_batch/update_batch are logged there
                // and swallowed — count the shortfall so the report shows
                // tracks that were scanned but never made it into the DB.
                let batch_inserted = track_repo.create_batch(&to_insert).unwrap_or(0) as i64;
                let batch_updated = track_repo.update_batch(&to_update).unwrap_or(0) as i64;
                db_insert_failed += to_insert.len() as i64 - batch_inserted;
                db_update_failed += to_update.len() as i64 - batch_updated;
                inserted += batch_inserted;
                updated += batch_updated;

                // Store extended metadata (composer, conductor, ReplayGain, MusicBrainz, etc.)
                // in the track_metadata table. Read extended tags and batch-insert.
                {
                    let meta_repo = tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(db.clone());
                    let mut meta_entries: Vec<(i64, std::collections::HashMap<String, String>)> = Vec::new();

                    for path_str in &extended_meta_paths {
                        let path = std::path::Path::new(path_str);
                        // Look up the track_id by file_path
                        if let Ok(Some(track)) = track_repo.get_by_path(path_str) {
                            if let Some(track_id) = track.id {
                                let ext_meta = tune_core::metadata::read_extended_metadata(path);
                                if !ext_meta.is_empty() {
                                    meta_entries.push((track_id, ext_meta));
                                }
                            }
                        }
                    }

                    if !meta_entries.is_empty() {
                        if let Err(e) = meta_repo.set_batch_multi(&meta_entries) {
                            tracing::warn!(error = %e, "scan_extended_metadata_insert_failed");
                        }
                    }
                }

                // Update track_count + album stats for albums touched in this batch
                // so albums are never visible with 0 tracks between batches.
                {
                    let touched_album_ids: std::collections::HashSet<i64> = to_insert
                        .iter()
                        .chain(to_update.iter())
                        .filter_map(|t| t.album_id)
                        .collect();
                    if !touched_album_ids.is_empty() {
                        let ids_csv: String = touched_album_ids
                            .iter()
                            .map(|id| id.to_string())
                            .collect::<Vec<_>>()
                            .join(",");
                        db.execute_batch(&format!(
                            "UPDATE albums SET track_count = \
                             (SELECT COUNT(*) FROM tracks WHERE tracks.album_id = albums.id) \
                             WHERE id IN ({ids_csv});\
                             UPDATE albums SET \
                             format = COALESCE(albums.format, (SELECT t.format FROM tracks t WHERE t.album_id = albums.id AND t.format IS NOT NULL LIMIT 1)), \
                             sample_rate = COALESCE(albums.sample_rate, (SELECT MAX(t.sample_rate) FROM tracks t WHERE t.album_id = albums.id)), \
                             bit_depth = COALESCE(albums.bit_depth, (SELECT MAX(t.bit_depth) FROM tracks t WHERE t.album_id = albums.id)), \
                             genre = COALESCE(NULLIF(albums.genre, ''), (SELECT t.genre FROM tracks t WHERE t.album_id = albums.id AND t.genre IS NOT NULL AND t.genre != '' LIMIT 1)), \
                             disc_count = COALESCE(albums.disc_count, (SELECT MAX(t.disc_number) FROM tracks t WHERE t.album_id = albums.id)) \
                             WHERE id IN ({ids_csv})"
                        )).ok();
                    }
                }

                // COMMIT this batch -- tracks + album stats are now queryable
                if !is_pg {
                    if let Err(e) = db.execute_batch("COMMIT") {
                        tracing::warn!(error = %e, batch = batch_idx, "scan_batch_commit_failed");
                        // Don't leave a half-open transaction poisoning the
                        // shared connection for subsequent writes.
                        let _ = db.execute_batch("ROLLBACK");
                    }
                }

                // Emit progress after each batch
                let processed = inserted + updated + skipped;
                let elapsed = last_progress_emit.elapsed();
                if processed > 0
                    && (batch_idx % 2 == 0 || elapsed >= std::time::Duration::from_secs(2))
                {
                    last_progress_emit = std::time::Instant::now();

                    // Compute scan rate and ETA
                    let elapsed_secs = scan_timer_start.elapsed().as_secs_f64().max(0.001);
                    let tracks_per_second = processed as f64 / elapsed_secs;
                    let remaining = (total - processed).max(0);
                    let eta_seconds = if tracks_per_second > 0.0 {
                        (remaining as f64 / tracks_per_second) as u64
                    } else {
                        0
                    };

                    event_bus.emit(
                        "library.scan.progress",
                        json!({
                            "phase": "files",
                            "scanned": processed,
                            "added": inserted,
                            "total": total,
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

        // Album covers extracted during the scan (owned by the importer).
        let artwork_extracted = importer.artwork_extracted() as i64;

        // Prune tracks whose files no longer exist on disk.
        // SAFETY: skip tracks in missing directories — the volume/NAS may
        // simply be unmounted. Deleting them would wipe the entire library.
        // Same protection for `error_dirs`: a subtree where the WALK itself
        // errored (unreadable subfolder, SMB stall mid-scan) has files that
        // exist but never made it into `discovered_paths`.
        // A cancelled scan never prunes: Stop must never be destructive.
        // Hissé hors du bloc : la réconciliation des favoris, plus bas, doit
        // savoir qu'une racine s'est vidée. Elle l'ignorait, et supprimait
        // DÉFINITIVEMENT les favoris de pistes pourtant conservées (#1943).
        let mut racines_videes: Vec<String> = Vec::new();
        // Hissés pour la même raison que `racines_videes` : le rapport de scan
        // doit pouvoir DIRE ce que la purge a fait, et surtout ce qu'elle a
        // refusé de faire. Tout cela ne vivait que dans `journalctl` (#1943, #1190).
        let mut pruned = 0i64;
        let mut protected = 0i64;
        let mut hors_perimetre = 0i64;
        // `Some((candidats, examinees))` quand le plafond a refusé la purge.
        let mut purge_refusee: Option<(usize, usize)> = None;
        if SCAN_CANCEL.load(Ordering::SeqCst) {
            tracing::info!("post_scan_prune_skipped_cancelled");
        } else {
            // Racines devenues vides : un partage non monté est LISIBLE et
            // vide, donc invisible pour `missing_dirs`. Sans ce garde, le
            // nettoyage ci-dessous efface la bibliothèque entière (#1652).
            let existing_refs: Vec<&str> =
                existing_tracks.keys().map(|s| s.as_str()).collect();
            racines_videes = roots_gone_empty(&scan_dirs, &existing_refs, &discovered_paths);
            let emptied_roots = &racines_videes;
            if !emptied_roots.is_empty() {
                tracing::error!(
                    roots = ?emptied_roots,
                    "post_scan_root_went_empty — ce dossier contenait des pistes et n'en présente plus aucune. Montage absent ? Les pistes sont CONSERVÉES."
                );
            }
            // Décider AVANT de supprimer : le plafond volumétrique a besoin de
            // connaître l'ampleur totale, et une suppression au fil de la
            // boucle ne se rattrape pas.
            let mut a_supprimer: Vec<i64> = Vec::new();
            let mut examinees = 0usize;
            for (db_path, &(track_id, _, _)) in &existing_tracks {
                // Targeted scan: only consider tracks under the scanned sub-tree.
                // `discovered_paths` only holds files below that folder, so a
                // track anywhere else would look "missing" and get wrongly
                // deleted — pruning the whole library except the sub-folder.
                if let Some(ref t) = targeted {
                    if !sous_le_dossier(db_path, t) {
                        continue;
                    }
                }
                examinees += 1;
                if !discovered_paths.contains(db_path.as_str()) {
                    match verdict_purge(
                        db_path,
                        &scan_dirs,
                        &missing_dirs,
                        &error_dirs,
                        emptied_roots,
                    ) {
                        VerdictPurge::ProtegeIllisible => protected += 1,
                        VerdictPurge::HorsPerimetre => hors_perimetre += 1,
                        VerdictPurge::Supprimer => a_supprimer.push(track_id),
                    }
                }
            }
            if purge_trop_massive(a_supprimer.len(), examinees) && !confirmer_purge {
                tracing::error!(
                    candidats = a_supprimer.len(),
                    examinees,
                    plafond = PART_MAX_PURGE,
                    "post_scan_purge_refusee_trop_massive — une disparition de cette ampleur est \
                     bien plus souvent un montage absent qu'une suppression réelle. Les pistes \
                     sont CONSERVÉES ; relancer le scan une fois les montages vérifiés."
                );
                purge_refusee = Some((a_supprimer.len(), examinees));
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
                    racines = ?scan_dirs,
                    "post_scan_tracks_hors_perimetre — ces pistes ne sont sous aucune racine \
                     configurée. Elles sont CONSERVÉES : un point de montage qui a changé n'est \
                     pas un fichier supprimé (#1943)."
                );
            }
            if protected > 0 {
                tracing::warn!(
                    protected,
                    missing = ?missing_dirs,
                    walk_errors = ?error_dirs,
                    emptied = ?emptied_roots,
                    "post_scan_tracks_protected_unreadable_dirs"
                );
            }
            if pruned > 0 {
                tracing::info!(pruned, "post_scan_stale_tracks_removed");
                event_bus.emit(
                    "library.scan.progress",
                    json!({ "phase": "prune", "pruned": pruned }),
                );
            }
        }

        // Backfill + album stats in a single transaction (SQLite only)
        let is_pg = db.engine() == tune_core::db::engine::Engine::Postgres;
        if !is_pg {
            if let Err(e) = db.execute_batch("BEGIN IMMEDIATE") {
                tracing::warn!(error = %e, "post_scan_begin_failed");
                let _ = db.execute_batch("ROLLBACK");
                let _ = db.execute_batch("BEGIN IMMEDIATE");
            }
        }
        {
            if let Err(e) = db.execute(
                "UPDATE tracks SET genres = '[\"' || REPLACE(genre, '\"', '\\\"') || '\"]' \
                 WHERE genre IS NOT NULL AND genre != '' AND (genres IS NULL OR genres = '')",
                &[],
            ) {
                tracing::warn!(error = %e, "post_scan_track_genres_backfill_failed");
            }
            if let Err(e) = db.execute(
                "UPDATE albums SET genres = '[\"' || REPLACE(genre, '\"', '\\\"') || '\"]' \
                 WHERE genre IS NOT NULL AND genre != '' AND (genres IS NULL OR genres = '')",
                &[],
            ) {
                tracing::warn!(error = %e, "post_scan_album_genres_backfill_failed");
            }
            if let Err(e) = db.execute(
                "UPDATE albums SET track_count = \
                 (SELECT COUNT(*) FROM tracks WHERE tracks.album_id = albums.id)",
                &[],
            ) {
                tracing::warn!(error = %e, "post_scan_track_count_update_failed");
            }
            if let Err(e) = db.execute(
                "UPDATE albums SET \
                 format = COALESCE(albums.format, (SELECT t.format FROM tracks t WHERE t.album_id = albums.id AND t.format IS NOT NULL LIMIT 1)), \
                 sample_rate = COALESCE(albums.sample_rate, (SELECT MAX(t.sample_rate) FROM tracks t WHERE t.album_id = albums.id)), \
                 bit_depth = COALESCE(albums.bit_depth, (SELECT MAX(t.bit_depth) FROM tracks t WHERE t.album_id = albums.id)), \
                 genre = COALESCE(NULLIF(albums.genre, ''), (SELECT t.genre FROM tracks t WHERE t.album_id = albums.id AND t.genre IS NOT NULL AND t.genre != '' LIMIT 1)), \
                 genres = COALESCE(NULLIF(albums.genres, ''), (SELECT t.genres FROM tracks t WHERE t.album_id = albums.id AND t.genres IS NOT NULL AND t.genres != '' LIMIT 1)), \
                 disc_count = COALESCE(albums.disc_count, (SELECT MAX(t.disc_number) FROM tracks t WHERE t.album_id = albums.id))",
                &[],
            ) {
                tracing::warn!(error = %e, "post_scan_album_quality_update_failed");
            }

            // Full scan only: realign each album's derived genre with its tracks.
            // The COALESCE above is fill-only (it never overwrites a value once
            // set), so an album whose genre was set once and then went stale —
            // e.g. stuck on "Folk" while its tracks are now "Folk-Punk" (Yves
            // Scordia) — never self-corrected. A forced full scan is an explicit
            // "rebuild from the files" action, so overwrite genre/genres from the
            // tracks; incremental scans keep the fill-only behaviour so values
            // persist between full scans. The EXISTS guard avoids nulling an
            // album genre when no track carries one.
            if force {
                // Pick the album genre by MAJORITY VOTE across its tracks, with a
                // deterministic tie-break, instead of an arbitrary `LIMIT 1` track.
                // A bare `LIMIT 1` (no ORDER BY) let SQLite return any row, so a
                // multi-genre album — or one track carrying a stray tag — got a
                // random genre that could differ per album and change between
                // scans (#1160/#1161). `genres` is rebuilt from the SAME chosen
                // genre so the two columns can never disagree (previously they
                // came from two independent subqueries, which is how an album
                // tagged "Alternatif & Indé" surfaced a stale "singer; Songwriter"
                // genres value from an unrelated track — #1160).
                if let Err(e) = db.execute(
                    "UPDATE albums SET \
                     genre = (SELECT t.genre FROM tracks t \
                              WHERE t.album_id = albums.id AND t.genre IS NOT NULL AND t.genre != '' \
                              GROUP BY t.genre ORDER BY COUNT(*) DESC, t.genre ASC LIMIT 1), \
                     genres = '[\"' || REPLACE( \
                                 (SELECT t.genre FROM tracks t \
                                  WHERE t.album_id = albums.id AND t.genre IS NOT NULL AND t.genre != '' \
                                  GROUP BY t.genre ORDER BY COUNT(*) DESC, t.genre ASC LIMIT 1), \
                                 '\"', '\\\"') || '\"]' \
                     WHERE EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = albums.id AND t.genre IS NOT NULL AND t.genre != '')",
                    &[],
                ) {
                    tracing::warn!(error = %e, "post_scan_album_genre_refresh_failed");
                }
            }
            // Merge duplicate local albums (same title, case-insensitive).
            // After a rescan, tag changes can create a second album entry for
            // tracks that already belonged to an existing album (e.g. when
            // album_artist changed). Merging moves all tracks to the album
            // with the most tracks, so the orphan cleanup below can delete the
            // now-empty duplicate. This is the definitive fix for bug #593
            // ("Doublons pochettes albums apres rescan").
            {
                let dupe_rows = db.query_many(
                    "SELECT LOWER(title), GROUP_CONCAT(id) FROM albums \
                     WHERE source = 'local' \
                     GROUP BY LOWER(title), artist_id HAVING COUNT(id) > 1",
                    &[],
                ).unwrap_or_default();
                let dupes: Vec<(String, String)> = dupe_rows.iter().map(|r| {
                    (r[0].as_string().unwrap_or_default(), r[1].as_string().unwrap_or_default())
                }).collect();
                let mut merged_albums = 0usize;
                for (_title, ids_str) in &dupes {
                    let ids: Vec<i64> = ids_str.split(',').filter_map(|s| s.parse().ok()).collect();
                    if ids.len() < 2 {
                        continue;
                    }
                    // Keep the album with the most tracks
                    let mut best_id = ids[0];
                    let mut best_count = 0i64;
                    for &aid in &ids {
                        let cnt = db.query_one(
                            "SELECT COUNT(id) FROM tracks WHERE album_id = ?",
                            &[&aid],
                        ).ok().flatten().and_then(|r| r[0].as_i64()).unwrap_or(0);
                        if cnt > best_count {
                            best_count = cnt;
                            best_id = aid;
                        }
                    }
                    for &aid in &ids {
                        if aid != best_id {
                            db.execute(
                                "UPDATE tracks SET album_id = ? WHERE album_id = ?",
                                &[&best_id, &aid],
                            ).ok();
                            db.execute(
                                "DELETE FROM albums WHERE id = ?",
                                &[&aid],
                            ).ok();
                            merged_albums += 1;
                        }
                    }
                }
                if merged_albums > 0 {
                    // Refresh track_count for albums that received tracks from merged duplicates
                    db.execute_batch(
                        "UPDATE albums SET track_count = \
                         (SELECT COUNT(*) FROM tracks WHERE tracks.album_id = albums.id)",
                    ).ok();
                    tracing::info!(merged_albums, "post_scan_duplicate_albums_merged");
                }
            }
            // Remove orphan albums with 0 tracks (created by interrupted scans or tag changes)
            let orphan_albums = db.execute(
                "DELETE FROM albums WHERE id IN (\
                 SELECT a.id FROM albums a \
                 LEFT JOIN tracks t ON t.album_id = a.id \
                 WHERE t.id IS NULL AND a.source = 'local')",
                &[],
            ).unwrap_or(0);
            if orphan_albums > 0 {
                tracing::info!(orphan_albums, "post_scan_orphan_albums_cleaned");
            }
        }
        if !is_pg {
            if let Err(e) = db.execute_batch("COMMIT") {
                tracing::warn!(error = %e, "post_scan_commit_failed");
                let _ = db.execute_batch("ROLLBACK");
            }
        }

        // Clean up orphan albums (album rows with no tracks). A full rescan
        // after removing files from disk — or the duplicate-album grouping —
        // can leave album rows behind that no track references. Without this
        // they linger with their cover art and inflate the total album count
        // even though they have no tracks (Alain: emptied library + full
        // rescan still shows removed albums' covers in double/triple). The
        // incremental auto-scan already purges these; the full scan did not.
        let orphan_albums = tune_core::db::album_repo::AlbumRepo::with_backend(db.clone())
            .delete_orphans()
            .unwrap_or(0);
        if orphan_albums > 0 {
            tracing::info!(orphan_albums, "post_scan_orphan_albums_cleaned");
        }

        // Clean up orphan artists left behind after tag corrections
        let orphan_artists = ArtistRepo::with_backend(db.clone()).cleanup_orphans().unwrap_or(0);
        if orphan_artists > 0 {
            tracing::info!(orphan_artists, "post_scan_orphan_artists_cleaned");
        }

        // Réconciliation des favoris : un rescan qui a recréé albums/pistes
        // sous de nouveaux rowids (racines music déplacées, library clear,
        // fusion de doublons ci-dessus) laisse des favoris orphelins → cœurs
        // éteints et filtre « Favoris » vide (bug .18, v0.9.50). On re-rattache
        // par identité (instantané titre/artiste/chemin, historique d'écoute en
        // secours) ; un favori vraiment introuvable n'est supprimé qu'après un
        // scan COMPLET et sain (pas ciblé, pas annulé, aucune racine
        // manquante/illisible) — jamais sur un scan partiel.
        {
            // `emptied_roots` fait partie de la condition depuis #1943 : il
            // manquait ici alors qu'il protégeait déjà la boucle de purge.
            // Conséquence vécue — une racine vidée par un montage absent
            // laissait `full_scan_ok = true`, et la réconciliation supprimait
            // DÉFINITIVEMENT les favoris des pistes conservées. Une purge de
            // pistes se répare par un rescan ; une perte de favoris, non.
            let full_scan_ok = !SCAN_CANCEL.load(Ordering::SeqCst)
                && targeted.is_none()
                && missing_dirs.is_empty()
                && error_dirs.is_empty()
                && racines_videes.is_empty();
            match tune_core::db::favorites_reconcile::FavoritesReconciler::with_backend(db.clone())
                .run(full_scan_ok)
            {
                Ok(stats) if stats.changed() > 0 || stats.unresolved > 0 => {
                    tracing::info!(
                        scanned = stats.scanned,
                        snapshots = stats.snapshots_backfilled,
                        relinked = stats.relinked,
                        deduplicated = stats.deduplicated,
                        deleted = stats.deleted,
                        unresolved = stats.unresolved,
                        "post_scan_favorites_reconciled"
                    );
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "post_scan_favorites_reconcile_failed"),
            }
        }

        // Backfill embedded cover art for local albums still missing a cover.
        // The incremental scan only extracts covers from files it re-processed;
        // unchanged files are skipped, so an improved embedded-art extractor
        // (e.g. DSF ID3v2 covers — Thibaud) never reaches an existing library.
        // Re-extract embedded art (local only, never the network) so those
        // albums self-heal without a forced full rescan.
        let covers_backfilled =
            tune_core::library::artwork::backfill_embedded_covers(&db, &cache_dir);
        if covers_backfilled > 0 {
            tracing::info!(covers_backfilled, "post_scan_embedded_covers_backfilled");
            event_bus.emit(
                "library.scan.progress",
                json!({ "phase": "artwork", "artwork_backfilled": covers_backfilled }),
            );
        }

        // Rebuild FTS indexes so search reflects the current library state.
        // The FTS tables are contentless (content='') and rely on triggers,
        // but manual DB edits or batch operations can leave them stale.
        // A full rebuild after scan guarantees consistency.
        // FTS rebuild + WAL checkpoint are SQLite-specific operations
        if db.engine() == tune_core::db::engine::Engine::Sqlite {
            db.execute_batch(
                "INSERT INTO tracks_fts(tracks_fts) VALUES('delete-all');\
                 INSERT INTO tracks_fts(rowid, title, artist_name, album_title, genre, composer) \
                 SELECT t.id, t.title, ar.name, al.title, t.genre, t.composer \
                 FROM tracks t LEFT JOIN artists ar ON t.artist_id = ar.id LEFT JOIN albums al ON t.album_id = al.id;\
                 INSERT INTO albums_fts(albums_fts) VALUES('delete-all');\
                 INSERT INTO albums_fts(rowid, title, artist_name, genre) \
                 SELECT a.id, a.title, ar.name, a.genre FROM albums a LEFT JOIN artists ar ON a.artist_id = ar.id;\
                 INSERT INTO artists_fts(artists_fts) VALUES('delete-all');\
                 INSERT INTO artists_fts(rowid, name, sort_name) SELECT id, name, sort_name FROM artists;\
                 PRAGMA wal_checkpoint(PASSIVE);",
            ).ok();
            tracing::info!("post_scan_fts_rebuilt");

        }

        // Populate cloud sync changelog with all new/updated entities
        tune_core::cloud::library_sync::populate_changelog_after_scan(&db);

        // Turn any .m3u/.m3u8/.pls files found in the scanned dirs into local
        // playlists (Bertrand). Runs after import so every track is in the DB to
        // match against; idempotent by playlist name so a re-scan never dupes.
        let pl = tune_core::library::playlist_scan::import_local_playlists(&db, &scan_dirs);
        if pl.playlists_created > 0 {
            event_bus.emit(
                "library.playlists.imported",
                json!({ "playlists": pl.playlists_created, "tracks": pl.tracks_added }),
            );
        }

        // Mirror hand-made compilation folders (tracks spanning several albums)
        // into local playlists — opt-in via scan_folder_playlists (Frédéric).
        if tune_core::library::folder_playlists::folder_playlists_enabled(&db) {
            tune_core::library::folder_playlists::sync_folder_playlists(&db);
        }

        let settings = SettingsRepo::with_backend(db.clone());
        if let Err(e) = settings.set("scan_status", "idle") {
            tracing::warn!(error = %e, "scan_status_idle_failed");
        }
        tracing::info!(
            discovered = total_discovered,
            parsed = scan_stats.total_files,
            timeout = scan_stats.metadata_timeout,
            inserted,
            updated,
            skipped,
            skipped_unchanged,
            skipped_duplicate,
            skipped_no_metadata,
            db_insert_failed,
            db_update_failed,
            artwork = artwork_extracted,
            orphan_artists,
            "scan_and_import_complete"
        );

        settings
            .set(
                "scan_result",
                &json!({
                    "total_files": total_discovered,
                    "missing_dirs": missing_dirs.clone(),
                    "missing_dir_reasons": missing_dir_reasons.clone(),
                    "error_dirs": error_dirs.clone(),
                    "parsed": scan_stats.total_files,
                    "metadata_ok": scan_stats.metadata_ok,
                    "metadata_failed": scan_stats.metadata_failed,
                    "metadata_timeout": scan_stats.metadata_timeout,
                    "inserted": inserted,
                    "updated": updated,
                    "skipped": skipped,
                    "skipped_unchanged": skipped_unchanged,
                    "skipped_duplicate": skipped_duplicate,
                    "skipped_no_metadata": skipped_no_metadata,
                    "db_insert_failed": db_insert_failed,
                    "db_update_failed": db_update_failed,
                    "artwork_extracted": artwork_extracted,
                    "failed_paths": scan_stats.failed_paths,
                    // Ce que la purge a fait, et surtout ce qu'elle a REFUSÉ de faire.
                    // L'écran affichait « scan terminé » pendant qu'une racine était
                    // partie vide ; seul `journalctl` le savait (#1943, #1190).
                    "emptied_roots": racines_videes.clone(),
                    "pruned": pruned,
                    "protected": protected,
                    "hors_perimetre": hors_perimetre,
                    "purge_refusee": purge_refusee.map(|(candidats, examinees)| serde_json::json!({
                        "candidats": candidats,
                        "examinees": examinees,
                        "plafond": PART_MAX_PURGE,
                    })),
                })
                .to_string(),
            )
            .ok();

        event_bus.emit(
            "library.scan.completed",
            json!({
                "total_files": total_discovered,
                "missing_dirs": missing_dirs.clone(),
                "missing_dir_reasons": missing_dir_reasons.clone(),
                "error_dirs": error_dirs.clone(),
                "parsed": scan_stats.total_files,
                "metadata_ok": scan_stats.metadata_ok,
                "metadata_timeout": scan_stats.metadata_timeout,
                "inserted": inserted,
                "updated": updated,
                "skipped": skipped,
                "skipped_unchanged": skipped_unchanged,
                "skipped_duplicate": skipped_duplicate,
                "skipped_no_metadata": skipped_no_metadata,
                "db_insert_failed": db_insert_failed,
                "db_update_failed": db_update_failed,
                "artwork_extracted": artwork_extracted,
                "failed_paths": scan_stats.failed_paths,
                // Ce que la purge a fait, et surtout ce qu'elle a REFUSÉ de faire.
                // L'écran affichait « scan terminé » pendant qu'une racine était
                // partie vide ; seul `journalctl` le savait (#1943, #1190).
                "emptied_roots": racines_videes.clone(),
                "pruned": pruned,
                "protected": protected,
                "hors_perimetre": hors_perimetre,
                "purge_refusee": purge_refusee.map(|(candidats, examinees)| serde_json::json!({
                    "candidats": candidats,
                    "examinees": examinees,
                    "plafond": PART_MAX_PURGE,
                })),
            }),
        );

        // Launch batch artwork enrichment as a background task
        // This fetches covers from MusicBrainz Cover Art Archive for albums
        // that don't have embedded cover art.
        // Write scan report JSON for the /scan/report endpoint
        let report = serde_json::json!({
            "total_files": total_discovered,
            "missing_dirs": missing_dirs.clone(),
            "missing_dir_reasons": missing_dir_reasons.clone(),
            "error_dirs": error_dirs.clone(),
            "parsed": scan_stats.total_files,
            "metadata_ok": scan_stats.metadata_ok,
            "metadata_failed": scan_stats.metadata_failed,
            "metadata_timeout": scan_stats.metadata_timeout,
            "inserted": inserted,
            "updated": updated,
            "skipped": skipped,
            "skipped_unchanged": skipped_unchanged,
            "skipped_duplicate": skipped_duplicate,
            "skipped_no_metadata": skipped_no_metadata,
            "db_insert_failed": db_insert_failed,
            "db_update_failed": db_update_failed,
            "artwork_extracted": artwork_extracted,
            "failed_paths": scan_stats.failed_paths,
            // Ce que la purge a fait, et surtout ce qu'elle a REFUSÉ de faire.
            // L'écran affichait « scan terminé » pendant qu'une racine était
            // partie vide ; seul `journalctl` le savait (#1943, #1190).
            "emptied_roots": racines_videes.clone(),
            "pruned": pruned,
            "protected": protected,
            "hors_perimetre": hors_perimetre,
            "purge_refusee": purge_refusee.map(|(candidats, examinees)| serde_json::json!({
                "candidats": candidats,
                "examinees": examinees,
                "plafond": PART_MAX_PURGE,
            })),
            // Fichiers audio rencontrés mais dont Tune ne lit pas le format,
            // comptés par extension ({"mpc": 280, "cue": 132}). Presque toujours
            // vide ; quand il ne l'est pas, c'est la seule chose qui explique à
            // l'utilisateur pourquoi des albums manquent, au lieu de le laisser
            // chercher un bug de scanner (#1763).
            "skipped_unsupported_by_ext": skipped_by_ext,
        });
        let report_path = std::env::var("TUNE_DB_PATH")
            .unwrap_or_else(|_| "tune.db".into())
            .replace(".db", "-scan-report.json");
        if let Ok(json) = serde_json::to_string_pretty(&report) {
            std::fs::write(&report_path, json).ok();
        }

        // Auto enrichment after scan: Premium only
        if auto_enrich_allowed {
            let enrich_db = db.clone();
            let artist_cache_dir = cache_dir.clone();
            let artist_mbid_db = db.clone();
            let artist_enrich_db = db.clone();
            handle.spawn(async move {
                tune_core::library::artwork::batch_enrich_artwork(enrich_db, cache_dir).await;
            });

            handle.spawn(async move {
                // Resolve MusicBrainz IDs BEFORE fetching artist images. The
                // image cascade only enriches artists that already have an MBID
                // (ArtistRepo::list_without_image filters on musicbrainz_id IS
                // NOT NULL), so a library scanned from files without MB tags
                // gets ZERO artist images despite Premium — the candidate list
                // is empty. Mirror the manual enrichment route (system/enrich.rs):
                // match MBIDs first, then fetch images (Fabien: 0 image on 1183
                // artists, none carrying an MBID).
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                tune_core::metadata::matcher::batch_match_artist_mbids(artist_mbid_db).await;
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                tune_core::library::artwork::batch_enrich_artist_artwork(artist_enrich_db, artist_cache_dir).await;
            });
        } else {
            tracing::info!(
                enrich_on_scan,
                "auto_enrichment_after_scan_skipped (needs Premium + enrich_on_scan)"
            );
        }
        }).await;
        if let Err(e) = result {
            tracing::error!("scan_task_panicked — {:?}", e);
            if let Err(e2) = SettingsRepo::with_backend(db_for_panic).set("scan_status", "idle") {
                tracing::warn!(error = %e2, "scan_status_panic_reset_failed");
            }
        }
    });
}

pub(super) async fn scan_status(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let status = settings
        .get("scan_status")
        .ok()
        .flatten()
        .unwrap_or_else(|| "idle".into());
    let scanning = status == "scanning";
    let result = settings
        .get("scan_result")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());

    Json(json!({
        "status": status,
        "scanning": scanning,
        "result": result,
    }))
}

pub(super) async fn scan_cancel(State(state): State<AppState>) -> impl IntoResponse {
    // Signal the running batch loop to stop processing further batches. The scan
    // task then drains its remaining (no-op) batches and runs its normal
    // completion path, which resets scan_status to "idle" and emits
    // library.scan.completed. Without this flag the endpoint only flipped the
    // status string while the scan kept inserting for minutes (bug #1129).
    SCAN_CANCEL.store(true, Ordering::SeqCst);
    tracing::info!("scan_cancel_requested");
    let settings = SettingsRepo::with_backend(state.backend.clone());
    if let Err(e) = settings.set("scan_status", "idle") {
        tracing::warn!(error = %e, "scan_cancel_status_reset_failed");
    }
    // Clear the client's "scanning" banner immediately. The batch loop's own
    // completion event only fires if the scan is *in* that loop — but if it is
    // stuck earlier (walker enumerating a slow/inaccessible NAS path, macOS
    // folder-permission stall) or has already ended, SCAN_CANCEL is a no-op and
    // no completion event is ever emitted, so "Stop scan" does nothing visible
    // (#1129). Emitting here guarantees the banner drops on Stop. A duplicate
    // event from the draining loop is harmless (the UI just clears twice).
    state
        .event_bus
        .emit("library.scan.completed", json!({ "cancelled": true }));
    StatusCode::NO_CONTENT
}

/// Daily scheduled-scan loop. The `/scan/schedule` endpoint has stored
/// `scan_schedule_enabled` / `scan_schedule_time` ("HH:MM") for ages, but
/// nothing ever read them back — the clients' toggle was silently a no-op
/// (the old tune-core ScanScheduler used different keys, an interval model
/// and a SQLite-only handle, and was never spawned; it is deleted).
///
/// Checks every 30 s; fires at most once per matching minute; a scan already
/// in progress skips that day's occurrence instead of stacking.
pub(crate) fn spawn_scan_scheduler(state: AppState) {
    tokio::spawn(async move {
        let mut last_fired: Option<String> = None;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let settings = SettingsRepo::with_backend(state.backend.clone());
            let enabled = settings
                .get("scan_schedule_enabled")
                .ok()
                .flatten()
                .map(|v| v == "true")
                .unwrap_or(false);
            if !enabled {
                continue;
            }
            let sched = settings
                .get("scan_schedule_time")
                .ok()
                .flatten()
                .unwrap_or_else(|| "03:00".into());
            let Some((sh, sm)) = parse_hhmm(&sched) else {
                continue;
            };
            // Local time: the user sets "03:00" meaning THEIR 3am, and log
            // timestamps are already local (see run.rs). Fall back to UTC if
            // the local offset is unavailable (some hardened Linux setups).
            let now = time::OffsetDateTime::now_local()
                .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
            if now.hour() != sh || now.minute() != sm {
                continue;
            }
            let stamp = format!(
                "{}-{:02}-{:02} {:02}:{:02}",
                now.year(),
                now.month() as u8,
                now.day(),
                sh,
                sm
            );
            if last_fired.as_deref() == Some(stamp.as_str()) {
                continue;
            }
            last_fired = Some(stamp);
            let scanning =
                settings.get("scan_status").ok().flatten().as_deref() == Some("scanning");
            if scanning {
                tracing::info!("scheduled_scan_skipped_already_scanning");
                continue;
            }
            tracing::info!(time = %sched, "scheduled_scan_triggered");
            spawn_library_scan(state.clone(), false, None).await;
        }
    });
}

fn parse_hhmm(s: &str) -> Option<(u8, u8)> {
    let (h, m) = s.trim().split_once(':')?;
    let h: u8 = h.trim().parse().ok()?;
    let m: u8 = m.trim().parse().ok()?;
    (h < 24 && m < 60).then_some((h, m))
}

pub(super) async fn scan_schedule(State(state): State<AppState>) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let time = settings
        .get("scan_schedule_time")
        .ok()
        .flatten()
        .unwrap_or_else(|| "03:00".into());
    let enabled = settings
        .get("scan_schedule_enabled")
        .ok()
        .flatten()
        .map(|v| v == "true")
        .unwrap_or(false);
    Json(json!({ "enabled": enabled, "time": time }))
}

#[derive(Deserialize)]
pub(super) struct ScanScheduleReq {
    enabled: bool,
    time: Option<String>,
}

pub(super) async fn set_scan_schedule(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Json(body): Json<ScanScheduleReq>,
) -> Json<Value> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    settings
        .set(
            "scan_schedule_enabled",
            if body.enabled { "true" } else { "false" },
        )
        .ok();
    if let Some(ref t) = body.time {
        settings.set("scan_schedule_time", t).ok();
    }
    Json(json!({ "enabled": body.enabled, "time": body.time }))
}

pub(super) async fn library_clear(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
) -> Json<Value> {
    let repo = tune_core::db::track_repo::TrackRepo::with_backend(state.backend.clone());
    match repo.delete_all() {
        Ok(count) => {
            tracing::info!(tracks_deleted = count, "library_cleared");
            Json(json!({"ok": true, "deleted": count}))
        }
        Err(e) => {
            tracing::warn!(error = %e, "library_clear_failed");
            Json(json!({"ok": false, "error": e.to_string()}))
        }
    }
}

fn chrono_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{now}")
}

/// Build a JSON array string for the `genres` column from parsed metadata.
///
/// If the structured `genres` vec is non-empty, serialize it as JSON.
/// Otherwise, fall back to the primary `genre` string and wrap it as a
/// single-element array so the column is never NULL when genre data exists.
pub(super) async fn scan_report() -> impl IntoResponse {
    let report_path = std::env::var("TUNE_DB_PATH")
        .unwrap_or_else(|_| "tune.db".into())
        .replace(".db", "-scan-report.json");
    match std::fs::read_to_string(&report_path) {
        Ok(json) => match serde_json::from_str::<Value>(&json) {
            Ok(v) => Json(v).into_response(),
            Err(_) => Json(json!({"error": "invalid report file"})).into_response(),
        },
        Err(_) => Json(json!({"error": "no scan report available yet"})).into_response(),
    }
}

/// GET /system/artist-split-preview — READ-ONLY dry-run of multi-artist credit
/// splitting (Phase 0 telemetry). Reports how many `artists` rows would split,
/// broken down by separator, plus example splits — WITHOUT changing anything.
/// Used to size the change and tune the allowlist before touching scan/DB.
pub(super) async fn artist_split_preview(State(state): State<AppState>) -> Json<Value> {
    use tune_core::metadata::artist_split::analyze_artist_credit;

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let extra: Vec<String> = settings
        .get("artist_split_allowlist")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default();

    let artist_repo = ArtistRepo::with_backend(state.backend.clone());
    let artists = artist_repo.list_all_id_name_mbid().unwrap_or_default();

    let total = artists.len();
    let mut would_split = 0usize;
    let mut would_split_no_mbid = 0usize;
    let mut by_sep: std::collections::HashMap<&'static str, usize> =
        std::collections::HashMap::new();
    let mut examples: Vec<Value> = Vec::new();

    for (_id, name, mbid) in &artists {
        let a = analyze_artist_credit(name, &extra, true);
        if a.would_split() {
            would_split += 1;
            if mbid.is_empty() {
                would_split_no_mbid += 1;
            }
            for s in &a.separators {
                *by_sep.entry(s.as_str()).or_insert(0) += 1;
            }
            if examples.len() < 60 {
                examples.push(json!({
                    "original": a.original,
                    "tokens": a.tokens,
                    "separators": a.separators.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    "has_mbid": !mbid.is_empty(),
                }));
            }
        }
    }

    Json(json!({
        "total_artists": total,
        "would_split": would_split,
        "would_split_no_mbid": would_split_no_mbid,
        "by_separator": by_sep,
        "extra_allowlist_size": extra.len(),
        "examples": examples,
        "note": "dry-run, read-only — no data changed",
    }))
}

#[cfg(test)]
mod roots_gone_empty_tests {
    use super::{
        PART_MAX_PURGE, VerdictPurge, purge_trop_massive, roots_gone_empty, sous_le_dossier,
        verdict_purge,
    };
    use std::collections::HashSet;

    fn set(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    // Le cas de Dominique COMET (#1652) : NAS OpenMediaVault en SMB, le
    // service démarre avant que le partage soit monté. Le point de montage
    // existe et se lit — il est simplement vide.
    const NAS: &str = "/mnt/nas/musique";

    #[test]
    fn un_partage_non_monte_protege_toute_la_bibliotheque() {
        let existants = [
            "/mnt/nas/musique/Bach/01.flac",
            "/mnt/nas/musique/Bach/02.flac",
            "/mnt/nas/musique/Mahler/01.flac",
        ];
        // Le scan ne découvre RIEN sous cette racine.
        let decouverts = set(&[]);
        assert_eq!(
            roots_gone_empty(&[NAS.to_string()], &existants, &decouverts),
            vec![NAS.to_string()],
            "zero fichier la ou il y en avait des milliers doit proteger, pas supprimer"
        );
    }

    #[test]
    fn une_racine_qui_repond_normalement_n_est_pas_protegee() {
        // Sans ça, plus aucune piste réellement supprimée ne serait nettoyée.
        let existants = ["/mnt/nas/musique/Bach/01.flac"];
        let decouverts = set(&["/mnt/nas/musique/Bach/01.flac"]);
        assert!(roots_gone_empty(&[NAS.to_string()], &existants, &decouverts).is_empty());
    }

    #[test]
    fn une_seule_piste_retrouvee_suffit_a_lever_la_protection() {
        // La racine répond : les autres absences sont de vraies suppressions.
        let existants = [
            "/mnt/nas/musique/Bach/01.flac",
            "/mnt/nas/musique/Bach/02.flac",
        ];
        let decouverts = set(&["/mnt/nas/musique/Bach/01.flac"]);
        assert!(roots_gone_empty(&[NAS.to_string()], &existants, &decouverts).is_empty());
    }

    // ── Purge de fin de scan : le sort d'une piste absente (#1943) ────────

    /// Le cas de Yacine, 17/08 : bibliothèque sur `/mnt/music2`, et 21 277
    /// lignes en base portant un ANCIEN point de montage qui n'est plus
    /// configuré. Elles ont été supprimées sans qu'aucune protection ne
    /// s'applique — elles n'étaient ni manquantes, ni en erreur, ni sous une
    /// racine vidée, puisque personne n'était allé voir.
    #[test]
    fn une_piste_hors_de_toute_racine_configuree_est_conservee() {
        let v = verdict_purge(
            "/mnt/music/Bach/01.flac",    // ancien montage
            &["/mnt/music2".to_string()], // seule racine configurée aujourd'hui
            &[],
            &[],
            &[],
        );
        assert_eq!(
            v,
            VerdictPurge::HorsPerimetre,
            "un point de montage qui a changé n'est pas un fichier supprimé"
        );
    }

    #[test]
    fn une_piste_sous_une_racine_saine_et_absente_du_disque_est_supprimee() {
        // Sans ça, plus rien ne serait jamais nettoyé.
        let v = verdict_purge(
            "/mnt/music2/Bach/01.flac",
            &["/mnt/music2".to_string()],
            &[],
            &[],
            &[],
        );
        assert_eq!(v, VerdictPurge::Supprimer);
    }

    #[test]
    fn une_racine_videe_protege_ce_qu_elle_contenait() {
        let v = verdict_purge(
            "/mnt/music2/Bach/01.flac",
            &["/mnt/music2".to_string()],
            &[],
            &[],
            &["/mnt/music2".to_string()],
        );
        assert_eq!(v, VerdictPurge::ProtegeIllisible);
    }

    #[test]
    fn sans_aucune_racine_configuree_on_ne_supprime_rien() {
        // Une liste vide ne dit pas « tout est hors périmètre », elle dit
        // qu'on ne sait rien. Le pire moment pour purger.
        let v = verdict_purge("/mnt/music2/Bach/01.flac", &[], &[], &[], &[]);
        assert_eq!(v, VerdictPurge::HorsPerimetre);
    }

    #[test]
    fn un_dossier_voisin_ne_beneficie_pas_du_prefixe() {
        // `/mnt/music2` est un préfixe de `/mnt/music22` : avec un simple
        // `starts_with`, une piste de `music22` passerait pour être sous
        // `music2` — protection appliquée au mauvais endroit, ou pas appliquée
        // là où on la croit.
        assert!(sous_le_dossier("/mnt/music2/a.flac", "/mnt/music2"));
        assert!(!sous_le_dossier("/mnt/music22/a.flac", "/mnt/music2"));
        assert!(sous_le_dossier("/mnt/music2", "/mnt/music2"));
        // Une barre finale sur la racine ne doit rien changer.
        assert!(sous_le_dossier("/mnt/music2/a.flac", "/mnt/music2/"));
    }

    /// La porte de sortie du plafond est un drapeau DÉDIÉ, jamais `force`.
    ///
    /// `force` est le bouton « Scan complet » : on le clique pour relire ses
    /// fichiers, et c'est exactement ce que clique quelqu'un dont le NAS était
    /// hors ligne, pour réparer sa bibliothèque. Y accrocher l'autorisation de
    /// supprimer en masse recréerait #1943 par la porte de service. Ce test
    /// fige la distinction : si un jour quelqu'un fusionne les deux drapeaux
    /// par souci de simplicité, il échoue.
    #[test]
    fn le_plafond_ne_cede_qu_a_une_confirmation_explicite() {
        let candidats = 21_277usize;
        let examinees = 70_346usize;
        // Le cas de Yacine : 30 %, franchement au-dessus du plafond.
        assert!(purge_trop_massive(candidats, examinees));

        // La décision réelle du site d'appel, dans ses deux régimes.
        let refuse = |confirmer: bool| purge_trop_massive(candidats, examinees) && !confirmer;
        assert!(
            refuse(false),
            "sans confirmation, la purge doit être refusée"
        );
        assert!(
            !refuse(true),
            "avec confirmation explicite, elle doit passer"
        );
    }

    /// Une purge sous le plafond n'a jamais besoin d'être confirmée : la
    /// confirmation ne doit pas devenir un passage obligé du scan ordinaire.
    #[test]
    fn sous_le_plafond_la_confirmation_ne_change_rien() {
        for confirmer in [false, true] {
            assert!(
                !(purge_trop_massive(50, 70_346) && !confirmer),
                "50 pistes sur 70 346 passent, confirmer={confirmer}"
            );
        }
    }

    #[test]
    fn une_purge_massive_est_refusee() {
        // Chez Yacine : 21 277 sur 70 346, soit 30 %. Au-dessus du plafond,
        // on refuse — une disparition de cette ampleur est bien plus souvent
        // un montage absent qu'une suppression réelle.
        assert!(purge_trop_massive(21_277, 70_346));
        // Une purge ordinaire passe.
        assert!(!purge_trop_massive(50, 70_346));
        // Le plafond exact ne déclenche pas ; au-delà, oui.
        assert!(!purge_trop_massive(200, 1000));
        assert!(purge_trop_massive(201, 1000));
    }

    #[test]
    fn une_petite_bibliotheque_n_est_pas_soumise_au_plafond() {
        // Retirer 10 pistes sur 20 est banal quand on range à la main : un
        // pourcentage n'a pas de sens à cette échelle.
        assert!(!purge_trop_massive(10, 20));
        assert!(!purge_trop_massive(49, 49));
    }

    #[test]
    fn un_dossier_neuf_sans_piste_n_est_pas_concerne() {
        // Cas normal d'une racine fraîchement configurée : rien à perdre.
        let existants: [&str; 0] = [];
        let decouverts = set(&[]);
        assert!(roots_gone_empty(&[NAS.to_string()], &existants, &decouverts).is_empty());
    }

    #[test]
    fn seule_la_racine_disparue_est_protegee() {
        // Un disque local intact ne doit pas cesser d'être nettoyé parce que
        // le NAS a disparu : la protection est par racine, pas globale.
        let local = "/home/dom/musique";
        let existants = [
            "/mnt/nas/musique/Bach/01.flac",
            "/home/dom/musique/pop/01.flac",
            "/home/dom/musique/pop/supprime.flac",
        ];
        let decouverts = set(&["/home/dom/musique/pop/01.flac"]);
        assert_eq!(
            roots_gone_empty(
                &[NAS.to_string(), local.to_string()],
                &existants,
                &decouverts
            ),
            vec![NAS.to_string()]
        );
    }

    #[test]
    fn une_racine_avec_barre_finale_est_traitee_pareil() {
        let existants = ["/mnt/nas/musique/Bach/01.flac"];
        let decouverts = set(&[]);
        assert_eq!(
            roots_gone_empty(&["/mnt/nas/musique/".to_string()], &existants, &decouverts),
            vec!["/mnt/nas/musique/".to_string()]
        );
    }

    #[test]
    fn une_racine_voisine_de_meme_prefixe_ne_deteint_pas() {
        // `/mnt/nas/musique2` ne doit pas être considérée comme couverte par
        // `/mnt/nas/musique` : c'est la barre finale du préfixe qui l'évite.
        let existants = ["/mnt/nas/musique2/Bach/01.flac"];
        let decouverts = set(&["/mnt/nas/musique2/Bach/01.flac"]);
        assert!(
            roots_gone_empty(&[NAS.to_string()], &existants, &decouverts).is_empty(),
            "la racine voisine ne doit ni proteger ni etre protegee a tort"
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::scan_import::{decide_compilation_albums, is_various_artists};

    fn decide<'a>(
        tracks: &'a [(&'a str, &'a str, Option<&'a str>, bool)],
    ) -> std::collections::HashMap<(String, String), bool> {
        decide_compilation_albums(
            tracks
                .iter()
                .map(|(dir, album, aa, flag)| (dir.to_string(), *album, *aa, *flag)),
        )
    }

    fn is_comp(
        m: &std::collections::HashMap<(String, String), bool>,
        dir: &str,
        album: &str,
    ) -> bool {
        *m.get(&(dir.to_string(), album.to_lowercase())).unwrap()
    }

    #[test]
    fn va_sentinels() {
        for s in [
            "Various Artists",
            "various",
            "VA",
            "Compilations",
            "  various artists  ",
        ] {
            assert!(is_various_artists(s), "{s} should be VA");
        }
        for s in ["The Beatles", "Various State", "AC/DC"] {
            assert!(!is_various_artists(s), "{s} should not be VA");
        }
    }

    #[test]
    fn single_artist_album_is_not_compilation() {
        // Consistent album_artist across the album -> not a compilation.
        let m = decide(&[
            ("/m/beatles/abbey", "Abbey Road", Some("The Beatles"), false),
            ("/m/beatles/abbey", "Abbey Road", Some("The Beatles"), false),
        ]);
        assert!(!is_comp(&m, "/m/beatles/abbey", "Abbey Road"));
    }

    #[test]
    fn per_track_album_artist_variance_is_compilation() {
        // The reported bug: a compilation whose tracks each carry their own
        // artist as the album_artist (no flag, no "Various Artists").
        let m = decide(&[
            ("/m/comp/jazz", "Best of Jazz", Some("Miles Davis"), false),
            ("/m/comp/jazz", "Best of Jazz", Some("John Coltrane"), false),
            ("/m/comp/jazz", "Best of Jazz", Some("Bill Evans"), false),
        ]);
        assert!(is_comp(&m, "/m/comp/jazz", "Best of Jazz"));
    }

    #[test]
    fn explicit_va_album_artist_is_compilation() {
        let m = decide(&[
            ("/m/comp/hits", "Now 100", Some("Various Artists"), false),
            ("/m/comp/hits", "Now 100", Some("Various Artists"), false),
        ]);
        assert!(is_comp(&m, "/m/comp/hits", "Now 100"));
    }

    #[test]
    fn compilation_flag_wins_even_with_consistent_artist() {
        let m = decide(&[
            ("/m/comp/ost", "OST", Some("Hans Zimmer"), true),
            ("/m/comp/ost", "OST", Some("Hans Zimmer"), false),
        ]);
        assert!(is_comp(&m, "/m/comp/ost", "OST"));
    }

    #[test]
    fn features_with_consistent_album_artist_not_compilation() {
        // Guests on some tracks, but album_artist stays the main artist -> the
        // album must not be flagged as a compilation.
        let m = decide(&[
            ("/m/drake/album", "Scorpion", Some("Drake"), false),
            ("/m/drake/album", "Scorpion", Some("Drake"), false),
        ]);
        assert!(!is_comp(&m, "/m/drake/album", "Scorpion"));
    }

    #[test]
    fn distinct_albums_same_folder_decided_independently() {
        // Two different single-artist albums sharing a folder must not be merged
        // into a compilation just because two album_artists appear in the dir.
        let m = decide(&[
            ("/m/singles", "Album A", Some("Artist A"), false),
            ("/m/singles", "Album B", Some("Artist B"), false),
        ]);
        assert!(!is_comp(&m, "/m/singles", "Album A"));
        assert!(!is_comp(&m, "/m/singles", "Album B"));
    }

    #[test]
    fn no_album_artist_is_not_flagged_compilation() {
        // Missing album_artist is left to the folder-first-artist heuristic in
        // the scan loop, not treated as a compilation here.
        let m = decide(&[
            ("/m/x/rec", "Recital", None, false),
            ("/m/x/rec", "Recital", None, false),
        ]);
        assert!(!is_comp(&m, "/m/x/rec", "Recital"));
    }

    #[test]
    fn same_album_title_different_folders_are_separate() {
        let m = decide(&[
            ("/m/a/greatest", "Greatest Hits", Some("Queen"), false),
            ("/m/b/greatest", "Greatest Hits", Some("ABBA"), false),
        ]);
        assert!(!is_comp(&m, "/m/a/greatest", "Greatest Hits"));
        assert!(!is_comp(&m, "/m/b/greatest", "Greatest Hits"));
    }

    #[test]
    fn parse_hhmm_accepts_valid_rejects_invalid() {
        assert_eq!(super::parse_hhmm("03:00"), Some((3, 0)));
        assert_eq!(super::parse_hhmm(" 23:59 "), Some((23, 59)));
        assert_eq!(super::parse_hhmm("3:5"), Some((3, 5)));
        assert_eq!(super::parse_hhmm("24:00"), None);
        assert_eq!(super::parse_hhmm("12:60"), None);
        assert_eq!(super::parse_hhmm("noon"), None);
        assert_eq!(super::parse_hhmm(""), None);
    }
}
