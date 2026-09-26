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
    build_track_from_metadata_opts(sf, artist_repo, album_repo, true, None, None)
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
    // L'unique artiste d'album ÉTIQUETÉ du dossier, quand le dossier n'en a
    // qu'un. Il ne sert QU'aux fichiers dont les balises n'ont pas pu être
    // lues (`TrackMetadata::artist_from_path`) : sans lui, un fichier en délai
    // dépassé se rangeait sous le nom de son dossier, donc dans un album à
    // part de ses voisines (#3232). `None` = comportement d'avant.
    folder_tagged_artist: Option<&str>,
) -> Option<(Track, Option<i64>)> {
    let meta = sf.metadata.as_ref()?;

    // C1 — le tag fait foi, la forme sert de repli. Même règle que
    // `scan_import::TrackImporter::import` ; cette voie-ci est celle du
    // surveillant de fichiers.
    let is_compilation = compilation_override.unwrap_or_else(|| {
        meta.compilation.unwrap_or_else(|| {
            meta.album_artist
                .as_deref()
                .map(crate::scan_import::is_various_artists)
                .unwrap_or(false)
        })
    });

    let album_artist_name = if is_compilation {
        "Various Artists"
    } else {
        meta.album_artist.as_deref().unwrap_or_else(|| {
            // Balises illisibles : `meta.artist` n'est que le nom d'un dossier.
            // Le vrai artiste du dossier, s'il n'y en a qu'un, vaut mieux.
            if meta.artist_from_path
                && let Some(a) = folder_tagged_artist
            {
                return a;
            }
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

        // Registre des executions automatisees (#2080). Ouvert AVANT toute
        // sortie anticipee : « aucun dossier configure » et « un scan tenait
        // deja le verrou » sont deux reponses valables a « le scan n'a rien
        // fait », et un registre qui ne les consignait pas laisserait ces deux
        // cas indistinguables d'un scan jamais lance.
        //
        // Le temoin ferme la ligne sur TOUS les chemins de sortie : `terminer`
        // en fin de scan, et son `Drop` en `interrompu` partout ailleurs — y
        // compris le deroulement d'une panique. Il ne couvre pas l'extinction
        // du processus ; c'est la cloture des orphelines au demarrage suivant
        // qui s'en charge (`startup::ouvrir_le_registre_des_executions`).
        let suivi = tune_core::db::task_run_repo::TaskRunRepo::with_backend(db.clone())
            .ouvrir(tune_core::db::task_run_repo::TACHE_SCAN_DEMARRAGE);

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
            suivi.rien_a_faire(Some("aucun dossier de musique configure"));
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
            suivi.rien_a_faire(Some("un autre scan tenait deja le verrou"));
            scan_done_clone.store(true, Ordering::Release);
            return;
        };
        let _scan_lease = scan_lease;

        // Make the startup scan first-class, exactly like the manual one:
        // advertise it via `scan_status` and honour cooperative cancellation.
        // The guards reset the persisted status before releasing the unique
        // owner on every exit path, including panic unwind.
        // L'annonce passe par le chemin PARTAGÉ avec le scan manuel : elle
        // pose l'horodatage ET le statut. Écrire le statut seul, comme on le
        // faisait, rendait ce scan indatable : le garde-fou de mise à jour
        // traitait « scanning sans horodatage » comme frais indéfiniment, si
        // bien qu'un processus tué ici — coupure de courant, `kill -9`, ou un
        // plantage au démarrage (#2302) — différait les mises à jour POUR
        // TOUJOURS. `ScanStatusGuard` ne rattrape pas ce cas : il ne s'exécute
        // que sur les sorties de la tâche, jamais sur une mort du processus
        // (#2976).
        crate::routes::system::scan::marquer_scan_en_cours(&db);
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
        let mut skipped_unsupported_paths = list_result.skipped_paths;
        // Les feuilles CUE, relues après le parcours (#1763) — mêmes clés que
        // le scan manuel. Les deux scans écrivent le MÊME fichier
        // `<db>-scan-report.json` : une clé posée d'un seul côté ferait
        // dépendre la réponse de `/scan/report` de QUEL scan a tourné en
        // dernier, ce qui est précisément le défaut de #2012 / #2050.
        //
        // #3631 (lot 2b) : le MÊME parcours ÉCRIT maintenant les pistes
        // virtuelles. `inventorier` construisait les `PisteCue` puis les
        // jetait ; `inventorier_et_ecrire` les range, sans relire une seule
        // feuille de plus, et rend l'inventaire à l'identique.
        let (inventaire_cue, bilan_cue, images_cue) =
            tune_core::scanner::cue_bibliotheque::inventorier_ecrire_et_confronter(
                db.clone(),
                &list_result.dossiers_avec_feuille_cue,
                // 🔴 `music_dirs`, PAS `scan_dirs` : un scan ciblé ne porte que
                // le sous-arbre demandé, et l'élagage CUE prendrait tout le
                // reste de la bibliothèque pour « hors périmètre ». Ce sont les
                // racines DÉCLARÉES qui bornent la décision, jamais l'étendue
                // du scan en cours.
                &music_dirs,
                // #5108 : la base est confrontée aux feuilles relues (retouchées ou
                // supprimées), sous le plafond de la purge.
                &tune_core::scanner::cue_bibliotheque::ConfrontationDuScan {
                    fichiers_vus: &list_result.files,
                    trop_massive: &crate::routes::system::scan::purge_trop_massive,
                },
            );
        if inventaire_cue.dossiers > 0 {
            info!(
                dossiers = inventaire_cue.dossiers,
                albums = inventaire_cue.albums,
                albums_multi_feuilles = inventaire_cue.albums_multi_feuilles,
                pistes = inventaire_cue.pistes,
                feuilles_ecartees = inventaire_cue.feuilles_ecartees,
                pistes_creees = bilan_cue.pistes_creees,
                pistes_mises_a_jour = bilan_cue.pistes_mises_a_jour,
                pistes_elaguees = bilan_cue.pistes_elaguees,
                "scan_cue_sheets_inventoried — feuilles CUE : ce qu'elles décrivent"
            );
        }
        // Un fichier image découpé par une feuille n'est PLUS une piste à lui
        // seul : ses tranches le représentent. Sans ce retrait, l'album
        // existerait deux fois — une piste de 74 minutes à côté de ses quinze.
        // Le retrait vaut aussi pour `discovered_paths` juste en dessous : une
        // bibliothèque déjà scannée voit ainsi sa piste « image entière »
        // élaguée au scan suivant, au lieu de rester en doublon à vie.
        let files: Vec<std::path::PathBuf> = if images_cue.is_empty() {
            list_result.files
        } else {
            list_result
                .files
                .into_iter()
                .filter(|p| !images_cue.contains(p))
                .collect()
        };
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
        let existing_tracks = match track_repo.get_all_file_info_by_path() {
            Ok(map) => map,
            Err(e) => {
                tracing::error!(error = %e, "auto_scan_aborted_existing_tracks_read_failed");
                // Le message d'erreur du moteur peut porter un chemin de base :
                // on n'inscrit que le motif, pas `e`.
                suivi.echec("lecture des pistes existantes impossible");
                scan_done_clone.store(true, Ordering::Release);
                return;
            }
        };
        let mut known_hashes = track_repo
            .get_existing_audio_hash_album_paths()
            .unwrap_or_default();
        // #4907 — les exemplaires déjà rattachés, sœur exacte du scan manuel.
        tune_core::library::exemplaires::nettoyer_les_orphelins(&*db);
        let existing_copies: crate::routes::system::scan::CarteDesChemins =
            tune_core::library::exemplaires::carte_des_exemplaires(&*db).unwrap_or_default();

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
                && crate::routes::system::scan::file_needs_scan(path, &existing_copies)
        };
        // `scan_io_concurrency()` et non 32 en dur : ce pool ignorait
        // `TUNE_SCAN_IO_CONCURRENCY`, donc régler la variable ne calmait que la
        // moitié de la charge — et personne ne comprenait pourquoi (#1948).
        let stat_pool = rayon::ThreadPoolBuilder::new()
            .num_threads(tune_core::scanner::walker::scan_io_concurrency())
            .build()
            .ok();
        // `partition` et non `filter` : les fichiers ÉCARTÉS sont gardés, parce
        // qu'ils PÈSENT sur le verdict « compilation » de leur dossier, dont la
        // base est le seul témoin quand ce scan ne les relit pas (#3528). Même
        // geste que le scan manuel, pour les mêmes raisons.
        let (files_to_scan, files_ecartes): (Vec<std::path::PathBuf>, Vec<std::path::PathBuf>) =
            match &stat_pool {
                Some(pool) => pool.install(|| files.into_par_iter().partition(|p| is_changed(p))),
                None => files.into_iter().partition(|p| is_changed(p)),
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
        let mut importer = crate::scan_import::TrackImporter::new(
            db.clone(),
            quality_split,
            cache_dir.clone(),
            crate::scan_import::PorteeDuScan {
                a_scanner: &files_to_scan,
                ecartes: &files_ecartes,
            },
        );
        let mut inserted = 0u64;
        let mut updated = 0u64;
        // `db_insert_failed` / `db_update_failed` sont désormais agrégés par le
        // parcours, à partir de ce que chaque lot rend (#2939) : voir plus bas,
        // après `scan_files_batched`.
        // `skipped` stays the aggregate the UI already shows; the per-cause
        // counters make the report actionable ("skipped 1200" alone doesn't
        // say whether the library is healthy or half the NAS failed to read).
        let mut skipped = pre_skipped as u64;
        let mut skipped_unchanged = pre_skipped as u64;
        let mut skipped_duplicate = 0u64;
        let mut skipped_no_metadata = 0u64;
        let mut skipped_unsupported = 0u64;
        // Les CHEMINS, en regard des compteurs ci-dessus (#2050). Sœurs de
        // celles du scan manuel : les deux boucles écartent pour les mêmes
        // trois motifs, et doivent le dire de la même façon.
        let mut skipped_no_metadata_paths: Vec<String> = Vec::new();
        let mut skipped_duplicate_paths: Vec<String> = Vec::new();

        // Progress telemetry for the auto/startup scan (parity with the manual
        // scan) so the UI shows a live bar during it too.
        let scan_total = files_to_scan.len() as i64;
        let scan_timer_start = std::time::Instant::now();
        let mut last_progress_emit = scan_timer_start;

        // #4896 — les balises lues, par album : voir `BalisesVuesParAlbum`.
        let mut balises_vues = BalisesVuesParAlbum::default();
        let stats = tune_core::scanner::walker::scan_files_batched(
            &files_to_scan,
            true,
            tune_core::scanner::walker::SCAN_BATCH_SIZE,
            |batch, batch_idx, _total_files| {
                // Cooperative cancellation: once "Arrêter le scan" was pressed,
                // skip all remaining batches so the startup scan drains quickly
                // (same pattern as the manual scan, #1129/#1197).
                if crate::routes::system::scan::scan_cancel_requested() {
                    // Rien présenté à la base, donc rien de refusé (#2939).
                    return tune_core::scanner::walker::EcrituresDuLot::SANS_PERTE;
                }
                let mut to_insert: Vec<Track> = Vec::with_capacity(batch.len());
                let mut to_update: Vec<Track> = Vec::with_capacity(batch.len() / 4);
                // Lignes posées par un importateur que ce lot reprend (#2939).
                let mut a_adopter: Vec<i64> = Vec::new();
                let mut exemplaires_du_lot =
                    crate::routes::system::scan::ExemplairesDuLot::default();

                // Manual transaction for batch performance (SQLite only;
                // PG handles transactions at the pool level).
                let is_sqlite = db.engine() == tune_core::db::engine::Engine::Sqlite;
                let sqlite_write_guard = is_sqlite.then(crate::sqlite_write_gate::scan_batch);
                if is_sqlite && db.execute("BEGIN IMMEDIATE", &[]).is_ok() {
                    // Se nommer : tout `write_tx` concurrent echouera tant
                    // que ce lot tient la connexion, et sans cette
                    // etiquette son message n'apprend rien (#1997).
                    tune_core::db::tx_holder::declarer("scan:auto");
                }

                importer.begin_batch(&batch);

                for sf in &batch {
                    // Un écrivain (favori, édition, enrichissement…) attend que
                    // ce lot ferme sa transaction : lui céder la place entre deux
                    // fichiers, plutôt qu'à la fin du lot (transaction_du_lot.rs).
                    db.ceder_aux_ecrivains();
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
                        // Le chemin ne vivait que dans ce `warn!` (#2050).
                        tune_core::scanner::walker::pousser_chemin_ecarte(
                            &mut skipped_no_metadata_paths,
                            sf.path.clone(),
                        );
                        continue;
                    }

                    // Insertion, mise à jour ou rien : la MÊME règle que le scan
                    // manuel, appelée et non recopiée (#2939). Prise avant
                    // `importer.import`, sans quoi un album fantôme (pochette,
                    // zéro piste) naît pour un fichier qu'on va écarter.
                    // `force` est faux ici : le scan automatique ne re-résout
                    // jamais les album_id d'un fichier inchangé.
                    // Un exemplaire déjà rattaché et inchangé ne se relit pas (#4907).
                    if crate::routes::system::scan::verdict_ecriture(
                        &sf.path,
                        sf.mtime,
                        sf.file_size,
                        false,
                        &existing_copies,
                    ) == crate::routes::system::scan::VerdictEcriture::Inchange
                    {
                        skipped += 1;
                        skipped_unchanged += 1;
                        continue;
                    }
                    let verdict = crate::routes::system::scan::verdict_ecriture(
                        &sf.path,
                        sf.mtime,
                        sf.file_size,
                        false,
                        &existing_tracks,
                    );
                    if verdict == crate::routes::system::scan::VerdictEcriture::Inchange {
                        skipped += 1;
                        skipped_unchanged += 1;
                        continue;
                    }

                    let Some((mut track, _album_id)) = importer.import(sf) else {
                        continue;
                    };

                    if let crate::routes::system::scan::VerdictEcriture::MettreAJour {
                        id,
                        adopter,
                    } = verdict
                    {
                        track.id = Some(id);
                        if adopter {
                            a_adopter.push(id);
                        }
                        balises_vues.noter(track.album_id, sf.metadata.as_ref());
                        to_update.push(track);
                        continue;
                    }

                    // #4907 — une copie octet pour octet d'une piste du même album
                    // devient un EXEMPLAIRE de cette piste (règle partagée).
                    if let Some(existing_path) =
                        exemplaires_du_lot.exemplaire_identique(&track, &known_hashes)
                    {
                        tracing::debug!(
                            path = %sf.path,
                            existing_path = %existing_path,
                            "auto_scan_exemplaire_identique"
                        );
                        skipped += 1;
                        skipped_duplicate += 1;
                        tune_core::scanner::walker::pousser_chemin_ecarte(
                            &mut skipped_duplicate_paths,
                            format!("{} (exemplaire de {})", sf.path, existing_path),
                        );
                        continue;
                    }

                    balises_vues.noter(track.album_id, sf.metadata.as_ref());
                    to_insert.push(track);
                }

                // Per-row failures inside create_batch/update_batch are logged
                // there and swallowed — count the shortfall so the report shows
                // tracks that were scanned but never made it into the DB.
                let batch_inserted = track_repo.create_batch(&to_insert).unwrap_or(0) as u64;
                let batch_updated = track_repo.update_batch(&to_update).unwrap_or(0) as u64;
                exemplaires_du_lot.ecrire(&*db, &existing_copies, &to_insert);
                // La pochette PROPRE d'une piste se pose à part : `update_batch`
                // n'écrit pas `cover_path`, faute de quoi une piste relue
                // recopierait dans sa ligne la pochette de son ALBUM (la lecture
                // est un `COALESCE`). Sans cet appel, l'image du single « Angry »
                // n'atteint jamais la base d'une bibliothèque déjà scannée
                // (#4650). Rien à écrire pour une piste sans pochette propre.
                if let Err(e) = track_repo.appliquer_pochettes_de_piste(&to_update) {
                    tracing::warn!(error = %e, "auto_scan_pochettes_de_piste_echec");
                }
                // Reprise des lignes d'importation relues sur le disque — sœur
                // exacte du scan manuel (#2939).
                match track_repo.adopter_en_local(&a_adopter) {
                    Ok(0) => {}
                    Ok(adoptees) => tracing::info!(
                        adoptees,
                        "auto_scan_lignes_importees_adoptees — ces pistes existaient sous une \
                         source d'importation au même chemin : le scan les a mises à jour et \
                         reprises."
                    ),
                    Err(e) => tracing::warn!(error = %e, "auto_scan_adoption_locale_failed"),
                }
                if batch_inserted == to_insert.len() as u64 {
                    for track in &to_insert {
                        if let (Some(hash), Some(album_id), Some(path)) =
                            (&track.audio_hash, track.album_id, &track.file_path)
                        {
                            known_hashes
                                .entry((hash.clone(), album_id))
                                .or_default()
                                .push(path.clone());
                        }
                    }
                }
                // Le manque à écrire de ce lot, rendu au parcours à la fin de
                // la fermeture — sœur exacte du scan manuel (#2939).
                let mut ecritures = tune_core::scanner::walker::EcrituresDuLot::manque(
                    to_insert.len(),
                    batch_inserted as usize,
                )
                .avec_manque_a_la_mise_a_jour(to_update.len(), batch_updated as usize);
                inserted += batch_inserted;
                updated += batch_updated;

                // Extract extended metadata (ISRC, ReplayGain, MusicBrainz, lyrics, etc.)
                //
                // #5043 — ce bloc parcourt le LOT DE TRAVAIL du scan, et le
                // scan de démarrage est toujours INCRÉMENTAL : `files_to_scan`
                // ne retient que les fichiers neufs ou modifiés
                // (`file_needs_scan`), et `verdict_ecriture` y est appelé avec
                // `force = false`. Un fichier inchangé n'entre donc jamais
                // ici : ce scan ne rattrape rien, et c'est voulu — rouvrir
                // toute la bibliothèque à chaque démarrage serait une
                // régression de performance à chaque allumage.
                //
                // Le rattrapage d'une bibliothèque constituée avant l'ajout de
                // ce bloc se fait au SCAN COMPLET (`?force=true` / `?full=true`,
                // le bouton « Scan complet »), dans
                // `routes::system::scan::spawn_library_scan_confirmee` : c'est
                // le seul scan dont le lot de travail contient les fichiers
                // inchangés. Voir la borne posée là-bas
                // (`rattrapage_metadonnees_5043`) : il n'y rouvre que les
                // pistes qui n'ont AUCUNE métadonnée étendue.
                {
                    let meta_repo =
                        tune_core::db::track_metadata_repo::TrackMetadataRepo::with_backend(
                            db.clone(),
                        );
                    let mut meta_entries: Vec<(i64, std::collections::HashMap<String, String>)> =
                        Vec::new();
                    // #5043 — les `tracks.id` du lot, par une lecture FORTE.
                    //
                    // Ce bloc tourne DANS la transaction du lot. `get_by_path`
                    // passait par le pool de lecture — des connexions SÉPARÉES
                    // sous SQLite, qui ne voient pas ce que cette transaction
                    // vient d'écrire. Elle rendait `None`, le `if let
                    // Ok(Some(..))` l'avalait, et aucune métadonnée étendue
                    // n'entrait en base. Le défaut est invisible sur une base
                    // `:memory:`, où les connexions de lecture sont des clones
                    // de celle d'écriture.
                    let chemins: Vec<String> = batch
                        .iter()
                        .filter(|sf| sf.metadata.is_some())
                        .map(|sf| sf.path.clone())
                        .collect();
                    let ids = tune_core::db::rattrapage_metadonnees_5043::ids_par_chemin(
                        &db, &chemins,
                    )
                    .unwrap_or_else(|e| {
                        tracing::warn!(error = %e, "auto_scan_ids_des_metadonnees_etendues_echec");
                        std::collections::HashMap::new()
                    });
                    for chemin in &chemins {
                        // Relire les balises coûte une E/S par fichier : céder ici aussi.
                        db.ceder_aux_ecrivains();
                        let Some(track_id) = ids.get(chemin).copied() else {
                            continue;
                        };
                        let ext = tune_core::metadata::read_extended_metadata(
                            std::path::Path::new(chemin),
                        );
                        if !ext.is_empty() {
                            meta_entries.push((track_id, ext));
                        }
                    }
                    if !meta_entries.is_empty() {
                        // Le DR lu dans un `foo_dr.txt` voisin (#4186) ne se
                        // compte que s'il est ENTRÉ en base — sœur exacte du
                        // scan manuel.
                        match meta_repo.set_batch_multi(&meta_entries) {
                            Ok(()) => {
                                ecritures = ecritures.avec_dr_des_rapports_voisins(
                                    meta_entries.iter().map(|(_, m)| m),
                                );
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "auto_scan_extended_metadata_insert_failed");
                            }
                        }
                    }
                }

                if is_sqlite {
                    db.execute("COMMIT", &[]).ok();
                    // Liberer meme si le COMMIT a echoue : une etiquette
                    // perimee accuserait un innocent au prochain incident.
                    tune_core::db::tx_holder::liberer();
                }
                drop(sqlite_write_guard);

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

                ecritures
            },
        );

        // Une seule source pour le journal et pour le rapport (#2939).
        let db_insert_failed = stats.db_insert_failed as u64;
        let db_update_failed = stats.db_update_failed as u64;

        for (format, count) in &stats.unsupported_by_ext {
            *skipped_by_ext.entry(format.clone()).or_insert(0) += count;
        }
        skipped_reasons.extend(stats.unsupported_reasons.clone());
        // Même fusion que pour les décomptes : les deux sources d'« écarté
        // faute de décodeur » aboutissent à une seule liste (#2050).
        for chemin in &stats.unsupported_paths {
            tune_core::scanner::walker::pousser_chemin_ecarte(
                &mut skipped_unsupported_paths,
                chemin.clone(),
            );
        }

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
        // Le scan automatique purge lui aussi (voir `pruned` plus bas), et il
        // émet lui aussi `library.scan.completed`. Son rapport ne portait
        // AUCUN compteur de purge : le bandeau annonçait donc « 0 supprimés »
        // sur ce chemin-là également. Hissé pour que le rapport puisse le
        // publier (#2146).
        let mut pistes_supprimees = 0i64;
        let mut db_delete_failed = 0i64;
        if crate::routes::system::scan::scan_cancel_requested() {
            info!("auto_scan_prune_skipped_cancelled");
        } else {
            // C'est CE scan-ci qui frappait Dominique : il tourne au démarrage
            // du service, précisément au moment où un montage SMB peut ne pas
            // encore être là. Le point de montage existe, il est lisible, il
            // est vide — et la bibliothèque partait avec (#1652).
            // Comme dans le scan manuel : la carte couvre toute la table (la
            // portée de `file_path TEXT UNIQUE`), mais la purge ne retire que
            // ce que le scan a posé. Le filtre est explicite, et la purge se
            // comporte exactement comme avant #2939.
            let pistes_locales: std::collections::HashMap<&str, i64> = existing_tracks
                .iter()
                .filter(|(_, info)| info.est_locale())
                .map(|(chemin, info)| (chemin.as_str(), info.id))
                .collect();
            // #4907 — les exemplaires tels qu'ils sont MAINTENANT (ce scan
            // vient peut-être d'en rattacher) ; ils comptent pour les racines
            // vidées comme pour sauver une piste.
            let copies_du_scan: crate::routes::system::scan::CarteDesChemins =
                tune_core::library::exemplaires::carte_des_exemplaires(&*db).unwrap_or_default();
            let existing_refs: Vec<&str> = pistes_locales
                .keys()
                .copied()
                .chain(copies_du_scan.keys().map(String::as_str))
                .collect();
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
            let mut protected = 0i64;
            let mut hors_perimetre = 0i64;
            let mut a_supprimer: Vec<i64> = Vec::new();
            let examinees = pistes_locales.len();
            for (&db_path, &track_id) in &pistes_locales {
                if !discovered_paths.contains(db_path) {
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
            let a_promouvoir = crate::routes::system::scan::separer_les_promotions(
                &mut a_supprimer,
                &copies_du_scan,
                &discovered_paths,
            );
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
            let bilan = crate::routes::system::scan::supprimer_pistes_du_scan(
                &track_repo,
                a_supprimer,
                "auto",
            );
            let bilan_promotions = crate::routes::system::scan::promouvoir_les_exemplaires(
                &*db,
                &track_repo,
                a_promouvoir,
                "auto",
            );
            crate::routes::system::scan::purger_les_exemplaires_disparus(
                &*db,
                &copies_du_scan,
                &discovered_paths,
                None,
                |chemin| {
                    verdict_purge(
                        chemin,
                        &music_dirs,
                        &missing_dirs,
                        &error_dirs,
                        emptied_roots,
                        &sous_arbres,
                    )
                },
            );
            let pruned = bilan.removed + bilan_promotions.removed;
            db_delete_failed = bilan.db_delete_failed + bilan_promotions.db_delete_failed;
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
            pistes_supprimees = pruned;
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

        // #4896 — APRÈS la purge : la ligne album d'un dossier retouché suit
        // ses balises, par la même règle que le surveillant.
        balises_vues.realigner(&db);

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
            // Les albums masqués (#1391) suivent la même mécanique : un
            // rowid renouvelé est re-rattaché par identité, et un marqueur
            // vraiment introuvable n'est purgé que sur un scan COMPLET sain
            // (même garde `full_scan_ok`, #1943).
            match tune_core::db::hidden_repo::HiddenRepo::with_backend(db.clone())
                .reconcile(full_scan_ok)
            {
                Ok(h) if h.changed() > 0 || h.unresolved > 0 => {
                    info!(
                        scanned = h.scanned,
                        relinked = h.relinked,
                        deduplicated = h.deduplicated,
                        deleted = h.deleted,
                        unresolved = h.unresolved,
                        "auto_scan_hidden_albums_reconciled"
                    );
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "auto_scan_hidden_albums_reconcile_failed"),
            }
            // Les paires « pas des doublons » (#1276) : même mécanique, même
            // garde `full_scan_ok`. Un arbitrage perdu ne se voit pas — il se
            // paie à la fusion suivante, qui supprime la ligne perdante.
            match tune_core::db::album_distinct_repo::AlbumDistinctRepo::with_backend(db.clone())
                .reconcile(full_scan_ok)
            {
                Ok(d) if d.changed() > 0 || d.unresolved > 0 => {
                    info!(
                        scanned = d.scanned,
                        relinked = d.relinked,
                        deduplicated = d.deduplicated,
                        deleted = d.deleted,
                        unresolved = d.unresolved,
                        "auto_scan_album_distinct_pairs_reconciled"
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "auto_scan_album_distinct_pairs_reconcile_failed")
                }
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
            db_delete_failed,
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
            // Ce que la purge a effectivement retiré. Le client lit cette clé
            // pour le bandeau de fin de scan (#2146).
            "removed": pistes_supprimees,
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
            "db_delete_failed": db_delete_failed,
            "artwork_extracted": artwork_extracted,
            "failed_paths": stats.failed_paths,
            // Les fichiers de 0 octet (#2060) — même clé que le scan manuel.
            // Un compteur : il part chez les trois consommateurs.
            "skipped_empty_files": stats.empty_files,
            // Le DR lu dans un `foo_dr.txt` voisin (#4186) — même clé que le
            // scan manuel (`ChiffresDeFinDeScan::rapport`).
            "dr_from_sidecar_file": stats.dr_from_sidecar,
            // Les Matroska admis / écartés (#3633) — mêmes clés que le scan
            // manuel (`ChiffresDeFinDeScan::rapport`).
            "matroska_admitted": stats.matroska_admis,
            "matroska_rejected": stats.matroska_ecartes,
            "skipped_unsupported_by_ext": skipped_by_ext,
            "skipped_unsupported_reasons": skipped_reasons,
            // Ce que les feuilles CUE décrivent (#1763) — mêmes clés que
            // `ChiffresDeFinDeScan::cue_sheets`.
            "cue_sheets": {
                "folders": inventaire_cue.dossiers,
                "folders_not_inventoried": inventaire_cue.dossiers_non_inventories,
                "albums": inventaire_cue.albums,
                "albums_multi_sheet": inventaire_cue.albums_multi_feuilles,
                "sheets_used": inventaire_cue.feuilles_retenues,
                "tracks": inventaire_cue.pistes,
                "sheets_skipped": inventaire_cue.feuilles_ecartees,
                "sheets_skipped_by_reason": inventaire_cue.ecarts_par_cle,
            },
        });

        // La liste demandée (#2050) — mêmes clés que le scan manuel, sans quoi
        // le rapport dépendrait de QUEL scan l'a produit. Comme dans
        // `ChiffresDeFinDeScan::rapport_du_fichier`, elle ne sort QUE par le
        // fichier : ce sont des chemins de l'utilisateur, et
        // `library.scan.completed` est diffusé à tous les clients connectés.
        let mut report_fichier = report.clone();
        report_fichier["skipped_unsupported_paths"] = serde_json::json!(skipped_unsupported_paths);
        report_fichier["skipped_no_metadata_paths"] = serde_json::json!(skipped_no_metadata_paths);
        report_fichier["skipped_duplicate_paths"] = serde_json::json!(skipped_duplicate_paths);
        report_fichier["cue_sheets_skipped_paths"] =
            serde_json::json!(inventaire_cue.chemins_ecartes);
        // LESQUELS sont vides (#2060) — même clé que le scan manuel.
        report_fichier["skipped_empty_file_paths"] = serde_json::json!(stats.empty_file_paths);
        report_fichier["skipped_paths_truncated"] = serde_json::json!(
            [
                skipped_unsupported_paths.len(),
                skipped_no_metadata_paths.len(),
                skipped_duplicate_paths.len(),
                inventaire_cue.chemins_ecartes.len(),
                stats.empty_file_paths.len(),
            ]
            .iter()
            .any(|n| *n >= tune_core::scanner::walker::PLAFOND_CHEMINS_ECARTES)
        );

        let report_path = std::env::var("TUNE_DB_PATH")
            .unwrap_or_else(|_| "tune.db".into())
            .replace(".db", "-scan-report.json");
        if let Ok(json) = serde_json::to_string_pretty(&report_fichier) {
            std::fs::write(&report_path, json).ok();
        }

        event_bus.emit("library.scan.completed", report);

        // Le compteur du registre est ce qui a CHANGE, pas ce qui a ete vu :
        // un scan qui relit 40 000 fichiers inchanges n'a rien fait, et
        // inscrire 40 000 le ferait passer pour un gros travail.
        let modifies = inserted as i64 + updated as i64 + pistes_supprimees;
        let verdict = if modifies == 0 {
            tune_core::db::task_run_repo::Verdict::RienAFaire
        } else {
            tune_core::db::task_run_repo::Verdict::Succes
        };
        // Que des compteurs : ni chemin, ni nom de dossier. `missing_dirs` et
        // `failed_paths` sont des chemins de l'utilisateur — ils restent dans
        // le rapport de scan, jamais dans le registre.
        let detail = format!(
            "{total_discovered} vus, {inserted} ajoutees, {updated} mises a jour, \
             {pistes_supprimees} retirees, {} dossiers absents",
            missing_dirs.len()
        );
        suivi.terminer(verdict, Some(modifies), Some(&detail));

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
        // Une suppression n'a rien à attendre ; un événement de DOSSIER non
        // plus (#4896) — et la taille d'un dossier ne se stabilise pas, elle
        // vaut 0 sous Windows : il resterait en attente pour toujours.
        if matches!(
            change.change_type,
            ChangeType::Deleted | ChangeType::DossierApparu | ChangeType::DossierDisparu
        ) {
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
mod registre_du_scan_tests {
    /// Le scan de demarrage inscrit son execution au registre (#2080) sur
    /// TOUTES ses sorties. Les deux sorties anticipees comptent autant que la
    /// normale : « aucun dossier configure » et « un scan tenait deja le
    /// verrou » sont precisement les deux reponses a « le scan n'a rien fait »,
    /// et sans elles ce cas se lirait comme un scan jamais lance.
    #[test]
    fn le_scan_de_demarrage_ferme_sa_ligne_sur_toutes_ses_sorties() {
        let source = include_str!("auto_scan.rs");
        let corps = source
            .split("pub fn spawn_auto_scan")
            .nth(1)
            .expect("spawn_auto_scan introuvable")
            .split("\n    scan_done\n}")
            .next()
            .expect("fin de spawn_auto_scan introuvable");

        assert_eq!(
            corps.matches("TACHE_SCAN_DEMARRAGE").count(),
            1,
            "une seule ouverture de ligne pour un scan"
        );
        assert_eq!(
            corps.matches("suivi.rien_a_faire").count(),
            2,
            "les deux sorties anticipees (aucun dossier, verrou deja tenu) \
             doivent chacune fermer la ligne"
        );
        assert_eq!(corps.matches("suivi.echec").count(), 1);
        assert_eq!(corps.matches("suivi.terminer").count(), 1);
    }

    /// #2976 : le scan de demarrage doit annoncer son etat par le chemin
    /// PARTAGE (`scan::marquer_scan_en_cours`), qui pose l'horodatage EN MEME
    /// TEMPS que le statut. Une annonce ecrite a la main ici reposerait
    /// `scan_status = "scanning"` sans date, et un scan sans date ne peut etre
    /// declare perime par personne : un processus tue pendant ce scan
    /// differerait les mises a jour a vie.
    ///
    /// Ce garde regarde l'APPELANT, pas la fonction appelee : c'est
    /// exactement le defaut « ecrit mais pas branche » qu'il existe pour
    /// empecher de revenir.
    #[test]
    fn le_scan_de_demarrage_annonce_son_etat_par_le_chemin_partage() {
        let source = include_str!("auto_scan.rs");
        let corps = source
            .split("pub fn spawn_auto_scan")
            .nth(1)
            .expect("spawn_auto_scan introuvable")
            .split("\n    scan_done\n}")
            .next()
            .expect("fin de spawn_auto_scan introuvable");
        assert_eq!(
            corps.matches("marquer_scan_en_cours(").count(),
            1,
            "le scan de demarrage annonce son etat par le chemin partage, une seule fois"
        );
        let annonce_a_la_main = format!("set({:?}, {:?})", "scan_status", "scanning");
        assert!(
            !corps.contains(&annonce_a_la_main),
            "aucune annonce ecrite a la main dans le scan de demarrage : \
             elle poserait le statut sans horodatage (#2976)"
        );
    }

    /// Le rapport de scan contient les chemins de l'utilisateur
    /// (`missing_dirs`, `failed_paths`). Le registre, lui, ne doit porter que
    /// des compteurs — il est fait pour etre colle dans un ticket.
    #[test]
    fn le_registre_du_scan_ne_recopie_aucun_chemin() {
        let source = include_str!("auto_scan.rs");
        let detail = source
            .split("let detail = format!(")
            .nth(1)
            .expect("le detail du registre a change de forme")
            .split(");")
            .next()
            .unwrap();

        for interdit in [
            "missing_dirs.clone",
            "failed_paths",
            "error_dirs",
            "music_dirs",
            "file_path",
            "report_path",
        ] {
            assert!(
                !detail.contains(interdit),
                "le detail inscrit au registre ne doit pas porter `{interdit}`"
            );
        }
        assert!(
            detail.contains("missing_dirs.len()"),
            "seul le NOMBRE de dossiers absents est inscrit, jamais leur nom"
        );
    }
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
        let dir = std::path::PathBuf::from(format!(
            ".{}",
            tune_core::test_scratch::scratch_name("settle_test")
        ));
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

/// La quatrième porte de suppression — celle du surveillant de fichiers.
///
/// Aucun système de fichiers, aucune horloge, aucun minuteur : la liste des
/// racines illisibles est un paramètre. Ces tests rendent le même verdict sur
/// n'importe quel hôte, y compris la CI Linux, alors que le défaut qu'ils
/// couvrent frappe surtout Windows et les partages réseau.
#[cfg(test)]
mod surveillant_suppression_tests {
    use super::verdict_suppression_surveillant as verdict;
    use crate::routes::system::scan::VerdictPurge;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    /// Le cas normal : le fichier a vraiment disparu d'une racine qui répond.
    #[test]
    fn un_fichier_efface_sous_une_racine_saine_part() {
        assert_eq!(
            verdict("/mnt/music/Jazz/a.flac", &v(&["/mnt/music"]), &[]),
            VerdictPurge::Supprimer
        );
    }

    /// La racine qui porte le fichier est tombée : on garde.
    #[test]
    fn une_racine_illisible_protege_ses_pistes() {
        assert_eq!(
            verdict(
                "/mnt/music/Jazz/a.flac",
                &v(&["/mnt/music"]),
                &v(&["/mnt/music"])
            ),
            VerdictPurge::ProtegeIllisible
        );
    }

    /// PREMIÈRE moitié perdue : `starts_with` nu, et la PREMIÈRE racine qui
    /// préfixe l'emporte. `/mnt/music2` répond, `/mnt/music22` est tombé —
    /// l'ancien garde interrogeait `/mnt/music2`, la trouvait lisible, et
    /// supprimait toute la bibliothèque du partage absent.
    #[test]
    fn la_racine_voisine_ne_repond_plus_pour_le_partage_tombe() {
        let racines = v(&["/mnt/music2", "/mnt/music22"]);
        let illisibles = v(&["/mnt/music22"]);
        assert_eq!(
            verdict("/mnt/music22/Jazz/a.flac", &racines, &illisibles),
            VerdictPurge::ProtegeIllisible
        );
        // Et la voisine, elle, purge normalement : la protection ne déborde pas.
        assert_eq!(
            verdict("/mnt/music2/Jazz/a.flac", &racines, &illisibles),
            VerdictPurge::Supprimer
        );
    }

    /// Même moitié, écriture Windows — `G:\Musique` et `G:\Musique 2`.
    /// `starts_with` ne voyait pas non plus la frontière d'antislash.
    #[test]
    fn la_frontiere_vaut_aussi_en_antislash() {
        let racines = v(&[r"G:\Musique", r"G:\Musique 2"]);
        assert_eq!(
            verdict(
                r"G:\Musique 2\Jazz\a.flac",
                &racines,
                &v(&[r"G:\Musique 2"])
            ),
            VerdictPurge::ProtegeIllisible
        );
        assert_eq!(
            verdict(r"G:\Musique\Jazz\a.flac", &racines, &[]),
            VerdictPurge::Supprimer
        );
    }

    /// SECONDE moitié perdue : hors périmètre (#1943). L'ancien garde ne
    /// trouvait aucune racine préfixe, sautait le `if let`, et supprimait
    /// sans condition. C'est le trou des 21 277 pistes de Yacine.
    #[test]
    fn une_piste_sous_aucune_racine_est_conservee() {
        assert_eq!(
            verdict("/ancien/montage/Jazz/a.flac", &v(&["/mnt/music"]), &[]),
            VerdictPurge::HorsPerimetre
        );
    }

    /// Aucune racine connue : on ne sait rien, on ne supprime rien.
    #[test]
    fn sans_racine_configuree_rien_ne_part() {
        assert_eq!(
            verdict("/mnt/music/Jazz/a.flac", &[], &[]),
            VerdictPurge::HorsPerimetre
        );
    }

    /// Contre-épreuve figée : l'ANCIENNE formule du garde, telle qu'elle
    /// était écrite, sur les deux cas ci-dessus. Elle laissait passer les
    /// deux suppressions. Ce test échouerait si quelqu'un la réintroduisait
    /// en croyant qu'elle était équivalente.
    #[test]
    fn l_ancien_garde_laissait_passer_les_deux() {
        let racines = v(&["/mnt/music2", "/mnt/music22"]);
        let illisible = "/mnt/music22";
        let ancien_supprimait = |chemin: &str| {
            // `find` : la PREMIÈRE racine qui préfixe, pas la bonne.
            match racines.iter().find(|r| chemin.starts_with(r.as_str())) {
                // La racine trouvée est lisible ⇒ l'ancien code supprimait.
                Some(root) => root != illisible,
                // Aucune racine ⇒ l'ancien code supprimait aussi.
                None => true,
            }
        };
        assert!(
            ancien_supprimait("/mnt/music22/Jazz/a.flac"),
            "l'ancien garde retenait /mnt/music2 pour un chemin de /mnt/music22"
        );
        assert!(
            ancien_supprimait("/ancien/montage/Jazz/a.flac"),
            "l'ancien garde n'avait aucune protection hors périmètre"
        );
    }
}

/// Le surveillant a-t-il le droit de retirer cette piste de la base ?
///
/// Le surveillant de fichiers supprime des lignes de `tracks`, comme la purge
/// de fin de scan — et c'était la seule des quatre portes de suppression
/// (scan manuel, scan automatique, retrait de racine, surveillant) à ne pas
/// passer par [`verdict_purge`]. Elle y perdait ses DEUX moitiés :
///
/// 1. **La frontière de séparateur.** Le garde testait
///    `change.path.starts_with(racine)`, un préfixe de NOM : `/mnt/music2`
///    « contient » alors `/mnt/music22/album/a.flac`. Pire, il retenait la
///    PREMIÈRE racine qui préfixe — donc si `/mnt/music2` répond et que
///    `/mnt/music22` est le partage tombé, le garde interroge la mauvaise
///    racine, la trouve lisible, et supprime. C'est la quatrième occurrence
///    du défaut de #2016.
/// 2. **La protection « hors périmètre » (#1943).** Une piste sous AUCUNE
///    racine configurée ne trouvait aucun garde du tout : `find` rendait
///    `None`, le `if let` était sauté, et `delete_by_path` partait sans
///    condition. C'est très exactement le trou par lequel 21 277 pistes de
///    Yacine ont disparu — un point de montage renommé, l'ancienne racine
///    plus configurée, personne pour dire « je ne sais pas ».
///
/// `racines_illisibles` est la liste des racines dont `read_dir` échoue à cet
/// instant. Elle est passée en paramètre, et non relue ici, pour que la règle
/// se teste sans système de fichiers — et pour qu'un lot de mille
/// suppressions ne paie pas mille `read_dir` sur un partage tombé.
pub(crate) fn verdict_suppression_surveillant(
    chemin: &str,
    racines: &[String],
    racines_illisibles: &[String],
) -> crate::routes::system::scan::VerdictPurge {
    crate::routes::system::scan::verdict_purge(chemin, racines, racines_illisibles, &[], &[], &[])
}
/// Relit UN fichier que le surveillant signale ajouté ou modifié, et remplace
/// sa ligne. Extrait tel quel de la boucle de `spawn_file_watcher` pour être
/// joué par les tests (#4896) — la boucle n'appelle plus que ceci.
///
/// Rend l'album du fichier quand ses balises ne s'accordent plus avec la ligne
/// album de son dossier (titre ou artiste d'album) : c'est la liste que
/// [`realigner_albums_sur_les_balises`] reprend en fin de lot.
pub(crate) fn reimporter_fichier_surveillant(
    db: &Arc<dyn DbBackend>,
    change: &tune_core::scanner::watcher::FileChange,
    watcher_quality_split: bool,
) -> Option<i64> {
    let mut album_a_realigner = None;
    // #4896 — un « ajout » sur un chemin DÉJÀ indexé est un REMPLACEMENT.
    // Sous Windows, un fichier remplacé par déplacement depuis un autre
    // dossier arrive en `FILE_ACTION_REMOVED` puis `FILE_ACTION_ADDED`, sans
    // `FILE_ACTION_MODIFIED` : `notify` rend `Remove` puis `Create`, et la
    // fusion du lot garde le dernier, `Added`. Cette branche ne supprimait
    // l'ancienne ligne que pour `Modified` : l'empreinte audio retrouvait
    // alors le fichier lui-même (« doublon ») et les balises neuves étaient
    // perdues sans un mot.
    let existante = TrackRepo::with_backend(db.clone())
        .get_by_path(&change.path)
        .ok()
        .flatten();
    let remplace = change.change_type == tune_core::scanner::watcher::ChangeType::Modified
        || existante.is_some();
    // Unchanged-file guard (Jean Marie: "le scan tourne
    // en boucle", macOS Ventura). A Modified event whose
    // on-disk mtime+size still match the stored row is a
    // self-induced event: reading a file to import it
    // makes macOS write an extended attribute, which
    // fires another Modify event → re-read → infinite
    // loop. Detect it with a cheap stat and skip —
    // crucially WITHOUT reading the content (scan_files_
    // parallel), since the read is what re-triggers it.
    // Même garde pour un « ajout » sur un chemin connu (#4896).
    if let Some(existing) = existante
        && let Ok(fs_meta) = std::fs::metadata(&change.path)
    {
        let fs_size = fs_meta.len() as i64;
        let fs_mtime = fs_meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as f64);
        let unchanged = existing.file_size == Some(fs_size)
            && match (existing.file_mtime, fs_mtime) {
                (Some(a), Some(b)) => (a - b).abs() <= 0.5,
                _ => false,
            };
        if unchanged {
            tracing::debug!(path = %change.path, "watcher_skip_unchanged");
            return None;
        }
    }
    let files: Vec<std::path::PathBuf> = vec![std::path::PathBuf::from(&change.path)];
    let (scanned, _) = tune_core::scanner::walker::scan_files_parallel(&files, true, None);
    let track_repo = TrackRepo::with_backend(db.clone());
    let artist_repo = ArtistRepo::with_backend(db.clone());
    let album_repo = AlbumRepo::with_backend(db.clone());

    for sf in &scanned {
        if sf.metadata.is_none() {
            continue;
        }

        if remplace {
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
        // Rendu : `(compilation ?, artiste d'album unique
        // du dossier)`. Le second sert au seul fichier
        // dont les balises n'ont pas pu être lues (#3232).
        let (comp_override, folder_tagged_artist): (Option<bool>, Option<String>) = sf
            .metadata
            .as_ref()
            .and_then(|meta| {
                let dir = std::path::Path::new(&sf.path).parent()?;
                // Le TAG, en trois etats (C1).
                let tag = meta.compilation;
                let mut va_tague = false;
                let mut artists: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                // La casse d'origine du premier artiste
                // vu : `artists` est en minuscules, or
                // ce nom peut devenir celui de l'album.
                let mut premier: Option<String> = None;
                let mut note = |aa: Option<&str>| {
                    if let Some(a) = aa.map(str::trim).filter(|s| !s.is_empty()) {
                        if crate::scan_import::is_various_artists(a) {
                            va_tague = true;
                        }
                        if artists.insert(a.to_lowercase()) && premier.is_none() {
                            premier = Some(a.to_string());
                        }
                    }
                };
                // Un artiste déduit du CHEMIN n'est pas
                // une balise : il ne compte pas.
                if !meta.artist_from_path {
                    note(meta.album_artist.as_deref());
                }
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
                let unique = if artists.len() == 1 { premier } else { None };
                // C1 : le tag tranche s'il existe ; sinon la forme.
                Some((Some(tag.unwrap_or(va_tague || artists.len() >= 2)), unique))
            })
            .unwrap_or((None, None));
        let Some((track, album_id)) = build_track_from_metadata_opts(
            sf,
            &artist_repo,
            &album_repo,
            watcher_quality_split,
            comp_override,
            folder_tagged_artist.as_deref(),
        ) else {
            tracing::warn!(path = %sf.path, "watcher_track_skipped_no_metadata");
            continue;
        };

        // The hash is only a candidate selector. The
        // watcher is allowed to skip solely after a
        // full byte-for-byte comparison.
        if let (Some(hash), Some(aid)) = (&track.audio_hash, album_id) {
            let candidates = track_repo
                .paths_by_audio_hash_and_album(hash, aid)
                .unwrap_or_default();
            if let Some(existing_path) = tune_core::scanner::hasher::find_byte_identical_path(
                std::path::Path::new(&sf.path),
                &candidates,
            ) {
                // #4907 — la copie devient un EXEMPLAIRE de la piste
                // identique, pas un fichier ignoré.
                if let Some(n) = tune_core::library::exemplaires::NouvelExemplaire::depuis_la_piste(
                    &existing_path,
                    &track,
                ) {
                    tune_core::library::exemplaires::rattacher(&**db, &[n]);
                }
                tracing::debug!(
                    audio_hash = %hash,
                    album_id = aid,
                    path = %sf.path,
                    existing_path = %existing_path,
                    "watcher_exemplaire_identique"
                );
                continue;
            }
            if !candidates.is_empty() {
                tracing::warn!(
                    audio_hash = %hash,
                    album_id = aid,
                    path = %sf.path,
                    candidates = candidates.len(),
                    "watcher_audio_hash_candidate_not_byte_identical"
                );
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
        }

        if ranger_la_piste_du_surveillant(&track_repo, &album_repo, &track, album_id) {
            info!(path = %sf.path, "watcher_track_added");
            // #4896 — les balises relues désavouent-elles la ligne album du
            // dossier ? Un simple lookup ; la relecture du dossier entier
            // n'a lieu qu'en fin de lot, et seulement dans ce cas.
            if let (Some(aid), Some(meta)) = (album_id, sf.metadata.as_ref())
                && let Ok(Some(album)) = album_repo.get(aid)
            {
                let (titre, artiste) = balises_d_album(meta);
                if balises_desavouent_l_album(&album, titre, artiste) {
                    album_a_realigner = Some(aid);
                }
            }
        }
    }
    album_a_realigner
}

/// #4896 (Didier, fil 1904) — la ligne album d'un dossier suit les balises
/// que l'on vient d'y retoucher.
///
/// L'album est identifié par son DOSSIER (`get_or_create_for_folder`) : relire
/// une piste retouchée la rattache à la ligne existante du dossier, titre et
/// artiste d'album compris. Chez Didier, le coffret indexé sans balise ALBUM
/// gardait le nom tiré du chemin après que Mp3tag eut posé ALBUM et
/// ALBUMARTIST sur chaque piste : les pistes étaient relues, l'album non.
///
/// Ne réécrit une ligne que si TOUTES ses pistes, relues sur le disque,
/// portent le même ALBUM (et, pour l'artiste, le même artiste d'album) : un
/// dossier retouché à moitié ne décide rien, la dernière piste enregistrée
/// par l'éditeur déclenchera la reprise. Portée : les seuls albums que le lot
/// a touchés, jamais la bibliothèque. Une compilation, une ligne non locale
/// et un champ édité à la main dans Tune ne sont pas repris.
pub(crate) fn realigner_albums_sur_les_balises(
    db: &Arc<dyn DbBackend>,
    albums: &std::collections::HashSet<i64>,
) {
    let album_repo = AlbumRepo::with_backend(db.clone());
    let track_repo = TrackRepo::with_backend(db.clone());
    let artist_repo = ArtistRepo::with_backend(db.clone());
    for &aid in albums {
        let Ok(Some(album)) = album_repo.get(aid) else {
            continue;
        };
        if album.is_compilation || album.source != "local" {
            continue;
        }
        let Ok(pistes) = track_repo.list_by_album(aid) else {
            continue;
        };
        let Some((titre, artiste)) = balises_unanimes_du_dossier(&pistes) else {
            continue;
        };
        let titre = (titre != album.title).then_some(titre);
        let artist_id = artiste
            .filter(|nom| {
                !album
                    .artist_name
                    .as_deref()
                    .is_some_and(|actuel| actuel.eq_ignore_ascii_case(nom))
            })
            .and_then(|nom| artist_repo.get_or_create(&nom, None, None).ok())
            .and_then(|a| a.id);
        if titre.is_none() && artist_id.is_none() {
            continue;
        }
        match album_repo.realigner_sur_les_balises(aid, titre.as_deref(), artist_id) {
            Ok(true) => info!(
                album_id = aid,
                ancien_titre = %album.title,
                nouveau_titre = ?titre,
                nouvel_artiste_id = ?artist_id,
                "watcher_album_realigne_sur_les_balises"
            ),
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(album_id = aid, error = %e, "watcher_album_realignement_echoue")
            }
        }
    }
}

/// (ALBUM, artiste d'album) communs à toutes les pistes, relus sur le disque.
/// `None` au moindre désaccord ou fichier illisible. L'artiste vaut `None`
/// quand les pistes ne s'accordent pas sur lui, ou qu'il n'est tiré que du
/// chemin : le titre peut alors être repris seul.
fn balises_unanimes_du_dossier(
    pistes: &[tune_core::db::models::Track],
) -> Option<(String, Option<String>)> {
    let mut titre: Option<String> = None;
    let mut artiste: Option<String> = None;
    let mut artiste_unanime = true;
    for piste in pistes {
        let chemin = piste.file_path.as_deref()?;
        let meta = tune_core::metadata::read_metadata(std::path::Path::new(chemin))?;
        let (t, a) = balises_d_album(&meta);
        let t = t?;
        match &titre {
            None => titre = Some(t.to_string()),
            Some(deja) if deja == t => {}
            Some(_) => return None,
        }
        match (a, &artiste) {
            (None, _) => artiste_unanime = false,
            (Some(a), None) => artiste = Some(a.to_string()),
            (Some(a), Some(deja)) if deja.eq_ignore_ascii_case(a) => {}
            (Some(_), Some(_)) => artiste_unanime = false,
        }
    }
    Some((titre?, artiste.filter(|_| artiste_unanime)))
}

/// (ALBUM, artiste d'album) que les balises d'UNE piste proposent à la ligne
/// album de son dossier : la lecture de [`balises_unanimes_du_dossier`], piste
/// par piste. Un artiste tiré du chemin n'est pas une balise ; « Various
/// Artists » ne nomme personne.
fn balises_d_album(meta: &tune_core::metadata::TrackMetadata) -> (Option<&str>, Option<&str>) {
    let titre = meta
        .album
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let artiste = if meta.artist_from_path {
        None
    } else {
        meta.album_artist
            .as_deref()
            .or(meta.artist.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty() && !crate::scan_import::is_various_artists(s))
    };
    (titre, artiste)
}

/// Les balises d'une piste désavouent-elles la ligne album où elle est rangée ?
/// Un simple regard, sans lecture de disque : c'est ce qui décide si
/// [`realigner_albums_sur_les_balises`] relira le dossier. Une compilation
/// n'est jamais désavouée (son titre peut être celui du dossier).
fn balises_desavouent_l_album(
    album: &tune_core::db::models::Album,
    titre: Option<&str>,
    artiste: Option<&str>,
) -> bool {
    if album.is_compilation {
        return false;
    }
    let titre_differe = titre.is_some_and(|t| t != album.title);
    let artiste_differe = artiste.is_some_and(|a| {
        !album
            .artist_name
            .as_deref()
            .is_some_and(|actuel| actuel.eq_ignore_ascii_case(a))
    });
    titre_differe || artiste_differe
}

/// #4896 — les balises qu'un scan (manuel ou de démarrage) a lues, rangées par
/// l'album où l'import a mis chaque piste. Le scan reconnaît lui aussi l'album
/// à son DOSSIER (`get_or_create_for_folder`) : relire les pistes d'un dossier
/// retouché ne renommait pas sa ligne album, pas plus qu'au surveillant.
///
/// Une paire (titre, artiste) distincte par album et non une par piste : un
/// scan complet de cent mille pistes n'en garde que quelques milliers. Le
/// verdict attend la fin du scan ([`Self::realigner`]), APRÈS la purge des
/// fichiers disparus : un éditeur qui renomme les fichiers en retouchant les
/// balises laisse sinon, dans l'album, des lignes dont le fichier n'existe
/// plus, et le dossier ne serait jamais « unanime ».
#[derive(Default)]
pub(crate) struct BalisesVuesParAlbum(
    std::collections::HashMap<i64, std::collections::HashSet<(Option<String>, Option<String>)>>,
);

impl BalisesVuesParAlbum {
    /// Une piste que le scan vient d'écrire (insérée ou mise à jour).
    pub(crate) fn noter(
        &mut self,
        album_id: Option<i64>,
        meta: Option<&tune_core::metadata::TrackMetadata>,
    ) {
        let (Some(aid), Some(meta)) = (album_id, meta) else {
            return;
        };
        let (titre, artiste) = balises_d_album(meta);
        self.0
            .entry(aid)
            .or_default()
            .insert((titre.map(str::to_string), artiste.map(str::to_string)));
    }

    /// Fin de scan : les albums que des balises lues désavouent sont repris
    /// par la MÊME règle que le surveillant — toutes les pistes du dossier
    /// doivent s'accorder, jamais une compilation, une ligne non locale ou un
    /// champ édité à la main. Rend le nombre d'albums soumis à cette règle.
    pub(crate) fn realigner(self, db: &Arc<dyn DbBackend>) -> usize {
        let album_repo = AlbumRepo::with_backend(db.clone());
        let desavoues: std::collections::HashSet<i64> = self
            .0
            .into_iter()
            .filter(|(aid, vues)| {
                album_repo.get(*aid).ok().flatten().is_some_and(|album| {
                    vues.iter().any(|(titre, artiste)| {
                        balises_desavouent_l_album(&album, titre.as_deref(), artiste.as_deref())
                    })
                })
            })
            .map(|(aid, _)| aid)
            .collect();
        if !desavoues.is_empty() {
            info!(
                albums = desavoues.len(),
                "scan_albums_a_realigner_sur_les_balises"
            );
            realigner_albums_sur_les_balises(db, &desavoues);
        }
        desavoues.len()
    }
}

/// Ce que la boucle du surveillant lit une fois, à son démarrage.
pub(crate) struct ReglagesDuSurveillant<'a> {
    /// Motifs d'exclusion des scans, en minuscules.
    pub(crate) exclusions: &'a [String],
    /// Racines de musique normalisées : l'arbitrage des suppressions (#1943).
    pub(crate) racines: &'a [String],
    pub(crate) quality_split: bool,
}

/// UN lot du surveillant, une fois écartés les fichiers encore en cours
/// d'écriture (`settle_partition`). Extrait tel quel de la boucle de
/// `spawn_file_watcher` (#4896) pour que les épreuves jouent la voie de
/// production ; la boucle n'appelle plus que ceci, puis nettoie les albums
/// vides et annonce le changement.
///
/// `dossiers_en_attente` porte d'un lot au suivant les dossiers disparus que
/// rien n'a encore expliqués (voir [`traiter_les_dossiers_du_lot`]). Rend les
/// fichiers à relire au cycle suivant : le contenu des dossiers apparus, qui
/// passe par la même attente d'écriture stable que tout fichier signalé.
pub(crate) fn traiter_le_lot_du_surveillant(
    db: &Arc<dyn DbBackend>,
    changes: Vec<tune_core::scanner::watcher::FileChange>,
    reglages: &ReglagesDuSurveillant<'_>,
    dossiers_en_attente: &mut Vec<String>,
) -> Vec<tune_core::scanner::watcher::FileChange> {
    use crate::routes::system::scan::VerdictPurge;
    use tune_core::scanner::watcher::ChangeType;
    let (dossiers, fichiers): (Vec<_>, Vec<_>) = changes.into_iter().partition(|c| {
        matches!(
            c.change_type,
            ChangeType::DossierApparu | ChangeType::DossierDisparu
        )
    });
    // Racines illisibles À CET INSTANT. Calculée une fois par
    // lot, et seulement s'il porte une suppression : `read_dir`
    // sur un partage réseau tombé peut bloquer plusieurs
    // secondes, et un lot en porte des centaines.
    let porte_une_suppression = !dossiers_en_attente.is_empty()
        || dossiers
            .iter()
            .any(|c| c.change_type == ChangeType::DossierDisparu)
        || fichiers
            .iter()
            .any(|c| c.change_type == ChangeType::Deleted);
    let racines_illisibles: Vec<String> = if porte_une_suppression {
        reglages
            .racines
            .iter()
            .filter(|r| std::fs::read_dir(r).is_err())
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    // #4896 — les DOSSIERS d'abord : une piste emportée par son dossier doit
    // avoir son nouveau chemin avant que les événements de fichier du même lot
    // (ceux du PollWatcher, qui voit chaque fichier bouger) ne la cherchent.
    // La porte n'est prise que s'il y a un dossier à traiter : un lot de
    // fichiers seuls ne doit pas attendre la fin d'un lot de scan pour rien.
    let a_relire = if dossiers.is_empty() && dossiers_en_attente.is_empty() {
        Vec::new()
    } else {
        let _porte = porte_du_scan(db);
        traiter_les_dossiers_du_lot(
            db,
            &dossiers,
            reglages,
            &racines_illisibles,
            dossiers_en_attente,
        )
    };
    let mut fichiers: Vec<_> = fichiers
        .into_iter()
        .filter(|c| !ecarte_du_surveillant(&c.path, reglages.exclusions))
        .collect();
    // #5073 — les dossiers dont une feuille CUE ou un fichier audio a changé
    // sont relus par le découpage du scan, AVANT d'importer quoi que ce soit :
    // un FLAC que sa feuille découpe n'est pas une piste à lui seul.
    let images_decoupees = relire_les_feuilles_cue_du_lot(db, &mut fichiers);
    let mut albums_a_realigner = std::collections::HashSet::new();
    for change in fichiers {
        // La feuille elle-même n'est pas une piste : son dossier vient d'être
        // relu. Un fichier qu'elle découpe est représenté par ses tranches.
        if tune_core::scanner::watcher::est_une_feuille_cue(std::path::Path::new(&change.path)) {
            continue;
        }
        if change.change_type != ChangeType::Deleted
            && images_decoupees.contains(std::path::Path::new(&change.path))
        {
            tracing::debug!(path = %change.path, "watcher_skip_image_decoupee_par_sa_feuille");
            continue;
        }
        // Un fichier à la fois : un lot de scan en attente passe entre deux.
        let _porte = porte_du_scan(db);
        match change.change_type {
            ChangeType::Added | ChangeType::Modified => {
                if let Some(aid) =
                    reimporter_fichier_surveillant(db, &change, reglages.quality_split)
                {
                    albums_a_realigner.insert(aid);
                }
            }
            ChangeType::Deleted => {
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
                // Même arbitrage que la purge de fin de scan, et
                // par la MÊME fonction : cette règle ne doit
                // exister qu'à un endroit (#1943). Ce chemin-ci
                // la contournait, et y perdait ses deux moitiés.
                match verdict_suppression_surveillant(
                    &change.path,
                    reglages.racines,
                    &racines_illisibles,
                ) {
                    VerdictPurge::ProtegeIllisible => {
                        tracing::warn!(
                            path = %change.path,
                            "watcher_delete_skipped_root_unreachable — mount dropped, keeping tracks"
                        );
                        continue;
                    }
                    VerdictPurge::HorsPerimetre => {
                        tracing::warn!(
                            path = %change.path,
                            "watcher_delete_skipped_out_of_scope — cette piste n'est sous AUCUNE racine configurée, elle n'est pas « disparue » (#1943)"
                        );
                        continue;
                    }
                    VerdictPurge::Supprimer => {}
                }
                // #4907 — un exemplaire disparu ne retire que lui-même ; le
                // fichier d'une piste qui a une copie joignable cède sa place
                // à la copie, et la piste garde son identifiant.
                match tune_core::library::exemplaires::retirer_le_fichier(&**db, &change.path) {
                    tune_core::library::exemplaires::RetraitDuFichier::Aucun => {}
                    autre => {
                        info!(path = %change.path, retrait = ?autre, "watcher_exemplaire_retire");
                        continue;
                    }
                }
                let track_repo = TrackRepo::with_backend(db.clone());
                if track_repo.delete_by_path(&change.path).is_ok() {
                    info!(path = %change.path, "watcher_track_removed");
                }
            }
            // Traités plus haut, par `traiter_les_dossiers_du_lot`.
            ChangeType::DossierApparu | ChangeType::DossierDisparu => {}
        }
    }
    // #4896 — la ligne album d'un dossier retouché suit ses balises.
    if !albums_a_realigner.is_empty() {
        let _porte = porte_du_scan(db);
        realigner_albums_sur_les_balises(db, &albums_a_realigner);
    }
    a_relire
}

/// Un chemin que le surveillant ignore : exclu des scans, ou fichier
/// temporaire de Tune.
fn ecarte_du_surveillant(chemin: &str, exclusions: &[String]) -> bool {
    // Same exclusions as the scans (re-read per event batch
    // so setting edits apply without a restart is overkill;
    // the list was read once at watcher start).
    if !exclusions.is_empty() {
        let path_l = chemin.to_lowercase();
        if exclusions.iter().any(|x| path_l.contains(x.as_str())) {
            return true;
        }
    }
    // Tune's own streaming temp files (tune-stream-*/
    // tune-prefetch-* in %TEMP%) fire watcher events on every
    // transcode when the library root is a parent of the temp
    // dir — 119 ghost scans in 2 minutes on Frédéric's setup,
    // degrading the first seconds of each streaming play.
    tune_core::scanner::is_tune_temp_file(std::path::Path::new(chemin))
}

/// #5073 (Gros Bidon, fil 1904) — un album « FLAC unique + feuille CUE »
/// déposé Tune lancé était importé en UNE piste : le surveillant ne relayait
/// pas le `.cue`, et importait le FLAC seul. Le découpage n'avait lieu qu'à
/// une analyse complète.
///
/// Relit, par le découpage du scan (`cue_bibliotheque::relire_le_dossier`),
/// chaque dossier du lot dont une feuille a changé (ajoutée, retouchée,
/// supprimée) ou dont un fichier audio a changé à côté d'une feuille — un FLAC
/// arrivé avant ou avec sa feuille, un dossier apparu. Rend les fichiers image
/// désormais découpés, que la boucle ne doit pas importer en piste entière, et
/// ajoute au lot, en `Added`, ceux qu'aucune feuille ne découpe plus.
fn relire_les_feuilles_cue_du_lot(
    db: &Arc<dyn DbBackend>,
    fichiers: &mut Vec<tune_core::scanner::watcher::FileChange>,
) -> std::collections::HashSet<std::path::PathBuf> {
    use tune_core::scanner::watcher::{ChangeType, FileChange, est_une_feuille_cue};
    let mut porte_une_feuille: std::collections::HashMap<std::path::PathBuf, bool> =
        std::collections::HashMap::new();
    let mut dossiers: Vec<std::path::PathBuf> = Vec::new();
    for change in fichiers.iter() {
        let chemin = std::path::Path::new(&change.path);
        let Some(dossier) = chemin.parent() else {
            continue;
        };
        let concerne = est_une_feuille_cue(chemin)
            || *porte_une_feuille
                .entry(dossier.to_path_buf())
                .or_insert_with(|| {
                    std::fs::read_dir(dossier).is_ok_and(|entrees| {
                        entrees.flatten().any(|e| est_une_feuille_cue(&e.path()))
                    })
                });
        if concerne && !dossiers.iter().any(|d| d == dossier) {
            dossiers.push(dossier.to_path_buf());
        }
    }
    let mut images_decoupees = std::collections::HashSet::new();
    if dossiers.is_empty() {
        return images_decoupees;
    }
    let _porte = porte_du_scan(db);
    for dossier in dossiers {
        let relecture = tune_core::scanner::cue_bibliotheque::relire_le_dossier(db, &dossier);
        info!(
            dossier = %dossier.display(),
            images_decoupees = relecture.images_decoupees.len(),
            pistes_creees = relecture.bilan.pistes_creees,
            pistes_mises_a_jour = relecture.bilan.pistes_mises_a_jour,
            tranches_retirees = relecture.tranches_retirees,
            pistes_entieres_retirees = relecture.pistes_entieres_retirees,
            images_liberees = relecture.images_liberees.len(),
            "watcher_dossier_cue_relu (#5073)"
        );
        images_decoupees.extend(relecture.images_decoupees);
        for image in relecture.images_liberees {
            let chemin = image.to_string_lossy().into_owned();
            // Déjà dans le lot : il sera importé par la boucle, une fois.
            if let Some(present) = fichiers.iter_mut().find(|c| c.path == chemin) {
                if present.change_type == ChangeType::Deleted {
                    continue;
                }
                present.change_type = ChangeType::Added;
            } else {
                fichiers.push(FileChange {
                    change_type: ChangeType::Added,
                    path: chemin,
                });
            }
        }
    }
    images_decoupees
}

/// La porte des lots de scan (`sqlite_write_gate`), prise par le surveillant
/// autour de chacune de ses écritures — SQLite seulement.
///
/// Un lot de scan tient `BEGIN IMMEDIATE` sur l'unique connexion d'écriture à
/// travers des centaines d'appels. Sans la porte, les écritures du surveillant
/// entraient dans CETTE transaction, que le pool de lecture ne voit pas :
/// voir `surveillant_pendant_un_lot_de_scan_tests.rs`. Sous PostgreSQL, le
/// scan travaille sans transaction de lot : rien à attendre.
fn porte_du_scan(db: &Arc<dyn DbBackend>) -> Option<tokio::sync::MutexGuard<'static, ()>> {
    (db.engine() == tune_core::db::engine::Engine::Sqlite)
        .then(crate::sqlite_write_gate::surveillant)
}

/// Un dossier disparu que ce lot examine : son chemin, les fichiers indexés
/// sous lui (chemin, taille), et s'il a déjà attendu un lot.
struct DossierDisparu {
    chemin: String,
    fichiers: Vec<(String, Option<i64>)>,
    deja_attendu: bool,
}

/// `<dossier><séparateur>` en NFC : le préfixe sous lequel la base range les
/// fichiers d'un dossier (même forme que `DossierExact` côté dépôt).
fn prefixe_en_base(dossier: &str) -> String {
    let base: String = dossier.trim_end_matches(['/', '\\']).nfc().collect();
    format!("{base}{}", std::path::MAIN_SEPARATOR)
}

/// #4896 (Didier, fil 1904) — un dossier d'album renommé, déplacé, mis à la
/// corbeille ou supprimé pendant que Tune tourne.
///
/// Le surveillant ne voyait que des fichiers audio : les trois moteurs natifs
/// ne signalent que le DOSSIER, si bien que l'album gardait ses pistes sous un
/// chemin mort jusqu'au scan suivant (voir `evenement_de_dossier` dans
/// `tune-core/src/scanner/watcher.rs` pour les séquences de chaque moteur).
///
/// - **Renommé ou déplacé** : un dossier apparu dont on retrouve les fichiers
///   d'un dossier disparu — même chemin relatif, même taille, pour plus de la
///   moitié d'entre eux — est le MÊME dossier. Ses pistes changent de chemin
///   en gardant leur ligne : identifiant, favoris, écoutes, étiquettes, date
///   d'ajout. Rien n'est supprimé puis réimporté comme un album neuf. La paire
///   se reconnaît sur le disque et non sur l'ordre des événements : Windows
///   dit `From`/`To` ou `Remove`/`Create`, FSEvents deux `Name(Any)` sans lien,
///   inotify une paire explicite, le PollWatcher une disparition et une
///   création.
/// - **Disparu sans successeur** : il attend UN lot (quelques secondes), au
///   cas où son nouveau nom arriverait dans le suivant, puis ses pistes sont
///   retirées sous le même arbitrage que toute suppression du surveillant —
///   racine illisible ou hors périmètre, rien ne part (#1943) — et sous le
///   même plafond que la purge du scan (`purge_trop_massive`) : au-delà, c'est
///   au scan, qui sait demander confirmation, de trancher.
/// - **Apparu** : ses fichiers audio que la base ne connaît pas à ce chemin
///   sont rendus pour être relus au cycle suivant, après l'attente d'écriture
///   stable (un dossier copié se remplit encore).
///
/// Portée : les seules pistes rangées sous le dossier concerné, découpage
/// exact (`…/Album` n'emporte pas `…/Album 2`).
fn traiter_les_dossiers_du_lot(
    db: &Arc<dyn DbBackend>,
    dossiers: &[tune_core::scanner::watcher::FileChange],
    reglages: &ReglagesDuSurveillant<'_>,
    racines_illisibles: &[String],
    en_attente: &mut Vec<String>,
) -> Vec<tune_core::scanner::watcher::FileChange> {
    use tune_core::scanner::watcher::{ChangeType, FileChange};
    let track_repo = TrackRepo::with_backend(db.clone());
    let mut disparus: Vec<DossierDisparu> = Vec::new();
    let du_lot = dossiers
        .iter()
        .filter(|c| c.change_type == ChangeType::DossierDisparu)
        .map(|c| (c.path.clone(), false));
    let du_lot_precedent = std::mem::take(en_attente).into_iter().map(|p| (p, true));
    for (chemin, deja_attendu) in du_lot.chain(du_lot_precedent) {
        if let Some(connu) = disparus.iter_mut().find(|d| d.chemin == chemin) {
            connu.deja_attendu |= deja_attendu;
            continue;
        }
        // Revenu entre-temps (renommé puis rendu à son nom) : rien n'a bougé.
        if std::fs::symlink_metadata(&chemin).is_ok() {
            continue;
        }
        let mut fichiers = track_repo
            .fichiers_sous_dossier(&chemin)
            .unwrap_or_default();
        // #4907 — les exemplaires rangés sous ce dossier le suivent aussi : un
        // dossier qui ne porte QUE des copies est un dossier à apparier.
        fichiers.extend(tune_core::library::exemplaires::exemplaires_sous_dossier(
            &**db, &chemin,
        ));
        // Un fichier qui n'était pas audio (pochette, temporaire d'un éditeur
        // de balises), ou un dossier sans piste indexée : rien à faire.
        if fichiers.is_empty() {
            continue;
        }
        disparus.push(DossierDisparu {
            chemin,
            fichiers,
            deja_attendu,
        });
    }

    let mut a_relire = Vec::new();
    for apparu in dossiers
        .iter()
        .filter(|c| c.change_type == ChangeType::DossierApparu)
    {
        let nouveau = std::path::Path::new(&apparu.path);
        if !nouveau.is_dir() {
            continue;
        }
        if let Some((i, deplacements)) = ancien_nom_du_dossier(nouveau, &disparus) {
            let ancien = disparus.remove(i);
            deplacer_le_dossier(db, &ancien.chemin, &apparu.path, &deplacements);
            // Ce qui n'a pas suivi (fichier retiré pendant le déplacement) est
            // bien parti : même traitement qu'un dossier disparu qui a attendu.
            let mut restants = track_repo
                .fichiers_sous_dossier(&ancien.chemin)
                .unwrap_or_default();
            restants.extend(tune_core::library::exemplaires::exemplaires_sous_dossier(
                &**db,
                &ancien.chemin,
            ));
            if !restants.is_empty() {
                disparus.push(DossierDisparu {
                    chemin: ancien.chemin,
                    fichiers: restants,
                    deja_attendu: true,
                });
            }
        }
        for fichier in tune_core::scanner::watcher::fichiers_audio_sous(nouveau) {
            if matches!(track_repo.get_by_path(&fichier), Ok(None))
                && !tune_core::library::exemplaires::est_un_exemplaire(&**db, &fichier)
            {
                a_relire.push(FileChange {
                    change_type: ChangeType::Added,
                    path: fichier,
                });
            }
        }
    }

    for disparu in disparus {
        if disparu.deja_attendu {
            retirer_les_pistes_du_dossier(db, &disparu, reglages.racines, racines_illisibles);
        } else {
            tracing::debug!(dossier = %disparu.chemin, pistes = disparu.fichiers.len(), "watcher_dossier_disparu_en_attente");
            en_attente.push(disparu.chemin);
        }
    }
    a_relire
}

/// Le dossier disparu dont `nouveau` porte les fichiers, et les déplacements
/// `(ancien chemin en base, nouveau chemin en base)` de ceux qu'on y retrouve
/// — même chemin relatif, même taille quand elle est connue. Il en faut PLUS
/// DE LA MOITIÉ : deux albums distincts partagent volontiers un « 01.flac »,
/// jamais la majorité de leurs fichiers à l'octet près. Le meilleur candidat
/// l'emporte.
fn ancien_nom_du_dossier(
    nouveau: &std::path::Path,
    disparus: &[DossierDisparu],
) -> Option<(usize, Vec<(String, String)>)> {
    let prefixe_nouveau = prefixe_en_base(&nouveau.to_string_lossy());
    let mut meilleur: Option<(usize, Vec<(String, String)>)> = None;
    for (i, disparu) in disparus.iter().enumerate() {
        let prefixe_ancien = prefixe_en_base(&disparu.chemin);
        let deplacements: Vec<(String, String)> = disparu
            .fichiers
            .iter()
            .filter_map(|(chemin, taille)| {
                let reste = chemin.strip_prefix(&prefixe_ancien)?;
                let sur_disque = std::fs::metadata(nouveau.join(reste)).ok()?;
                let meme_taille = taille.is_none_or(|t| t == sur_disque.len() as i64);
                (sur_disque.is_file() && meme_taille)
                    .then(|| (chemin.clone(), format!("{prefixe_nouveau}{reste}")))
            })
            .collect();
        if deplacements.len() * 2 <= disparu.fichiers.len() {
            continue;
        }
        if meilleur
            .as_ref()
            .is_none_or(|(_, deja)| deplacements.len() > deja.len())
        {
            meilleur = Some((i, deplacements));
        }
    }
    meilleur
}

/// Déplace en base les pistes d'un dossier renommé, et les albums que ce
/// dossier identifie.
fn deplacer_le_dossier(
    db: &Arc<dyn DbBackend>,
    ancien: &str,
    nouveau: &str,
    deplacements: &[(String, String)],
) {
    let pistes = TrackRepo::with_backend(db.clone()).deplacer_fichiers(deplacements);
    // #4907 — les exemplaires du dossier suivent, rattachés à leur piste. Un
    // couple qui n'est pas une copie ne touche rien, et inversement.
    let exemplaires =
        tune_core::library::exemplaires::deplacer_les_exemplaires(&**db, deplacements);
    let albums = AlbumRepo::with_backend(db.clone()).deplacer_dossier(ancien, nouveau);
    match (pistes, albums) {
        (Ok(pistes), Ok(albums)) => info!(
            ancien = %ancien,
            nouveau = %nouveau,
            pistes,
            albums,
            exemplaires,
            "watcher_dossier_deplace — pistes et albums gardent leur ligne (#4896)"
        ),
        (pistes, albums) => tracing::warn!(
            ancien = %ancien,
            nouveau = %nouveau,
            pistes = ?pistes.err(),
            albums = ?albums.err(),
            "watcher_dossier_deplacement_echoue"
        ),
    }
}

/// Retire les pistes d'un dossier disparu pour de bon, sous l'arbitrage de
/// toute suppression du surveillant (#1943) et le plafond de la purge du scan.
fn retirer_les_pistes_du_dossier(
    db: &Arc<dyn DbBackend>,
    disparu: &DossierDisparu,
    racines: &[String],
    racines_illisibles: &[String],
) {
    use crate::routes::system::scan::VerdictPurge;
    let track_repo = TrackRepo::with_backend(db.clone());
    let a_retirer: Vec<&str> = disparu
        .fichiers
        .iter()
        .map(|(chemin, _)| chemin.as_str())
        .filter(|chemin| {
            matches!(
                verdict_suppression_surveillant(chemin, racines, racines_illisibles),
                VerdictPurge::Supprimer
            )
        })
        .collect();
    if a_retirer.len() < disparu.fichiers.len() {
        tracing::warn!(
            dossier = %disparu.chemin,
            gardees = disparu.fichiers.len() - a_retirer.len(),
            "watcher_dossier_disparu_pistes_gardees — racine illisible ou hors périmètre (#1943)"
        );
    }
    let total = track_repo.count().unwrap_or(0).max(0) as usize;
    if crate::routes::system::scan::purge_trop_massive(a_retirer.len(), total) {
        tracing::warn!(
            dossier = %disparu.chemin,
            pistes = a_retirer.len(),
            total,
            "watcher_dossier_disparu_trop_massif — rien n'est retiré, le prochain scan en décidera"
        );
        return;
    }
    let mut retirees = 0usize;
    for chemin in a_retirer {
        // #4907 — même règle que pour un fichier disparu seul : une piste qui
        // a un exemplaire joignable ailleurs y bascule et garde son identifiant.
        if !matches!(
            tune_core::library::exemplaires::retirer_le_fichier(&**db, chemin),
            tune_core::library::exemplaires::RetraitDuFichier::Aucun
        ) {
            continue;
        }
        if track_repo.delete_by_path(chemin).is_ok() {
            retirees += 1;
        }
        // Une image découpée par une feuille CUE porte ses tranches ici.
        let _ = track_repo.delete_by_cue_media(chemin);
    }
    info!(dossier = %disparu.chemin, pistes = retirees, "watcher_dossier_retire");
}

/// Le surveillant range UNE piste lue sur le disque, PUIS remonte sur son
/// album ce qui s'en déduit (nombre de pistes, qualité, genre, label).
///
/// L'ordre est la correction (#4836, suite) : la remontée précédait
/// l'insertion, si bien qu'elle ne voyait pas la piste qu'on venait de lire.
/// Le premier fichier d'un album neuf n'y portait jamais son label, et un
/// fichier modifié (supprimé puis réinséré) le retirait du vote. Le scan de
/// démarrage et le scan manuel, eux, remontent APRÈS avoir écrit leurs
/// pistes : les trois chemins suivent désormais le même ordre.
pub(crate) fn ranger_la_piste_du_surveillant(
    track_repo: &TrackRepo,
    album_repo: &AlbumRepo,
    track: &Track,
    album_id: Option<i64>,
) -> bool {
    let rangee = track_repo.create(track).is_ok();
    if let Some(aid) = album_id {
        album_repo.update_track_count(aid).ok();
        album_repo.update_quality_from_tracks(aid).ok();
    }
    rangee
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

    // Le surveillant supprime des lignes de `tracks` : il passe par le MÊME
    // arbitrage que la purge de fin de scan (#1943), dans
    // `traiter_le_lot_du_surveillant`.

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
            // #4896 — dossiers disparus que le lot précédent n'a pas expliqués.
            let mut dossiers_en_attente: Vec<String> = Vec::new();
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
                // Un dossier disparu en attente se tranche au lot suivant, même
                // sans nouvel événement : ce lot-là compte comme un changement.
                let had_changes = !changes.is_empty() || !dossiers_en_attente.is_empty();
                let a_relire = traiter_le_lot_du_surveillant(
                    &db,
                    changes,
                    &ReglagesDuSurveillant {
                        exclusions: &watcher_excludes,
                        racines: &guard_roots,
                        quality_split: watcher_quality_split,
                    },
                    &mut dossiers_en_attente,
                );
                // Le contenu d'un dossier apparu attend, comme tout fichier
                // signalé, que son écriture soit stable (#4896).
                pending_settle.extend(a_relire);
                // After a batch, remove any album left with 0 tracks. An
                // incremental re-import can re-point a track to a new album
                // row (album_artist tag drift) and leave the old row as a
                // cover-only ghost — eric: "une fois avec les pistes, une
                // autre fois juste la pochette". The manual scan cleans
                // these; the watcher never did.
                if had_changes {
                    let album_repo = AlbumRepo::with_backend(db.clone());
                    let cleaned = {
                        let _porte = porte_du_scan(&db);
                        album_repo.delete_orphans().unwrap_or(0)
                    };
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

#[cfg(test)]
#[path = "surveillant_retouche_tests_4896.rs"]
mod surveillant_retouche_tests_4896;

#[cfg(test)]
#[path = "surveillant_dossiers_tests_4896.rs"]
mod surveillant_dossiers_tests_4896;

#[cfg(test)]
#[path = "scan_realigne_tests_4896.rs"]
mod scan_realigne_tests_4896;

#[cfg(test)]
#[path = "scan_metadonnees_etendues_tests_5043.rs"]
mod scan_metadonnees_etendues_tests_5043;

#[cfg(test)]
#[path = "surveillant_pendant_un_lot_de_scan_tests.rs"]
mod surveillant_pendant_un_lot_de_scan_tests;

#[cfg(test)]
#[path = "surveillant_feuille_cue_tests_5073.rs"]
mod surveillant_feuille_cue_tests_5073;

#[cfg(test)]
#[path = "scan_feuille_cue_tests_5108.rs"]
mod scan_feuille_cue_tests_5108;
