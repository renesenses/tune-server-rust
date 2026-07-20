//! Shared track-import helpers for the library scanners.
//!
//! Both the manual scan ([`crate::routes::system::scan`]) and the auto/startup +
//! watcher scans ([`crate::auto_scan`]) turn a [`ScannedFile`]'s
//! [`TrackMetadata`] into a DB [`Track`] row. This module holds the single
//! field-mapping they share so the three former copies cannot drift again — they
//! had already diverged: the manual *insert* path omitted `disc_subtitle`, and
//! the auto/watcher helper omitted `genres` and `composer`.
//!
//! Artist/album *resolution* still lives with each caller for now (it needs
//! batch-wide compilation context); this module owns only the per-file field
//! mapping, which every scan path shares verbatim.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::DbBackend;
use tune_core::db::models::{Album, Artist, Track};
use tune_core::metadata::TrackMetadata;
use tune_core::scanner::walker::ScannedFile;

/// True when an `album_artist` value denotes a various-artists compilation.
pub(crate) fn is_various_artists(s: &str) -> bool {
    let l = s.trim().to_lowercase();
    l == "various artists" || l == "various" || l == "va" || l == "compilations"
}

/// Decide, per `(folder, album title)`, whether that album is a various-artists
/// compilation, from the metadata of a set of scanned tracks.
///
/// A genuine single-artist album has one consistent `album_artist`. An album is
/// treated as a compilation when any of its tracks carries the compilation flag
/// or a "Various Artists" album_artist, OR when the `album_artist` value varies
/// across the tracks of the same `(folder, album)` — the tell-tale of a
/// compilation whose tracks were each tagged with their own artist as the
/// album_artist, which otherwise splits into one album (and cover) per artist.
///
/// Keys are `(folder, album_title.to_lowercase())`.
pub(crate) fn decide_compilation_albums<'a>(
    items: impl Iterator<Item = (String, &'a str, Option<&'a str>, bool)>,
) -> HashMap<(String, String), bool> {
    let mut acc: HashMap<(String, String), (bool, HashSet<String>)> = HashMap::new();
    for (dir, album, album_artist, comp_flag) in items {
        let entry = acc.entry((dir, album.to_lowercase())).or_default();
        let aa = album_artist.map(|s| s.trim()).filter(|s| !s.is_empty());
        if comp_flag || aa.map(is_various_artists).unwrap_or(false) {
            entry.0 = true;
        }
        if let Some(aa) = aa {
            entry.1.insert(aa.to_lowercase());
        }
    }
    acc.into_iter()
        .map(|(k, (flag, artists))| (k, flag || artists.len() >= 2))
        .collect()
}

/// Serialize the parsed multi-genre list to a JSON array string for
/// `tracks.genres`. Falls back to splitting the single `genre` tag for legacy
/// rows that predate multi-genre parsing.
pub fn build_genres_json(genres: &[String], genre: Option<&str>) -> Option<String> {
    if !genres.is_empty() {
        Some(serde_json::to_string(genres).unwrap_or_default())
    } else if let Some(g) = genre.filter(|g| !g.is_empty()) {
        // Split in case the single tag carries separators (legacy data).
        let split = tune_core::metadata::split_genre_tag(g);
        if split.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&split).unwrap_or_default())
        }
    } else {
        None
    }
}

/// Map a [`ScannedFile`]'s metadata onto a DB [`Track`] row.
///
/// `album_id` / `artist_id` / `track_artist_name` come from the caller's
/// artist/album resolution. The title falls back to the file stem when the tag
/// has none. `id` is left `None`; the update path sets it afterwards.
pub fn build_track_row(
    meta: &TrackMetadata,
    sf: &ScannedFile,
    album_id: Option<i64>,
    artist_id: Option<i64>,
    track_artist_name: &str,
) -> Track {
    let title = meta.title.clone().unwrap_or_else(|| {
        std::path::Path::new(&sf.path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    });

    let mut track = Track::new(title);
    track.album_id = album_id;
    track.artist_id = artist_id;
    track.artist_name = Some(track_artist_name.to_string());
    track.album_artist = meta.album_artist.clone();
    track.album_title = meta.album.clone();
    track.disc_number = meta.disc_number.unwrap_or(1) as i32;
    track.disc_subtitle = meta.disc_subtitle.clone();
    track.track_number = meta.track_number.unwrap_or(0) as i32;
    track.duration_ms = meta.duration_ms.unwrap_or(0) as i64;
    track.file_path = Some(sf.path.clone());
    track.format = meta.format.clone();
    track.sample_rate = meta.sample_rate.map(|s| s as i32);
    track.bit_depth = meta.bit_depth.map(|b| b as i32);
    track.channels = meta.channels.unwrap_or(2) as i32;
    track.file_size = Some(sf.file_size as i64);
    track.file_mtime = Some(sf.mtime as f64);
    track.audio_hash = sf.audio_hash.clone();
    track.genre = meta.genre.clone();
    track.genres = build_genres_json(&meta.genres, meta.genre.as_deref());
    track.composer = meta
        .credits
        .iter()
        .find(|c| c.role == "composer")
        .map(|c| c.name.clone());
    track.year = meta.year.map(|y| y as i32);
    track.bpm = meta.bpm;
    track.label = meta.label.clone();
    track.isrc = meta.isrc.clone();
    track.musicbrainz_recording_id = meta.musicbrainz_recording_id.clone();
    track.comments = meta.comment.clone();
    track
}

/// Batch-stateful importer that resolves a scanned file's artist and album in
/// the DB and builds its [`Track`] row, sharing one implementation between the
/// manual scan and the auto/startup + watcher scans.
///
/// It carries the caches and the per-batch compilation decision the resolution
/// needs, so both scan paths get the *same* album grouping — the classical-
/// soloist album-artist pinning and the compilation-flattening that previously
/// lived only in the manual scan (the auto/watcher path used a simpler resolver
/// and could split a compilation, or an album with per-track soloists, into one
/// album+cover per artist).
///
/// Usage per batch: call [`begin_batch`](Self::begin_batch) once with the whole
/// batch, then [`import`](Self::import) for each file the caller has decided to
/// (re)index. The caller keeps ownership of the unchanged-file skip, the
/// insert-vs-update decision, dedup, and the transaction.
pub struct TrackImporter {
    artist_repo: ArtistRepo,
    album_repo: AlbumRepo,
    quality_split: bool,
    cache_dir: std::path::PathBuf,
    /// Caches persist across batches for the lifetime of a scan.
    artist_cache: HashMap<String, Arc<Artist>>,
    album_cache: HashMap<(String, i64, Option<i32>), Arc<Album>>,
    albums_with_cover: HashSet<i64>,
    /// First track-artist seen per folder, used to pin the album artist when a
    /// track has no `album_artist` tag (classical soloists / features).
    dir_album_artist: HashMap<String, String>,
    /// Per-batch `(folder, album)` → is-compilation decision.
    comp_decision: HashMap<(String, String), bool>,
    artwork_extracted: u64,
}

impl TrackImporter {
    pub fn new(db: Arc<dyn DbBackend>, quality_split: bool, cache_dir: std::path::PathBuf) -> Self {
        Self {
            artist_repo: ArtistRepo::with_backend(db.clone()),
            album_repo: AlbumRepo::with_backend(db),
            quality_split,
            cache_dir,
            artist_cache: HashMap::new(),
            album_cache: HashMap::new(),
            albums_with_cover: HashSet::new(),
            dir_album_artist: HashMap::new(),
            comp_decision: HashMap::new(),
            artwork_extracted: 0,
        }
    }

    /// Number of album covers extracted so far (for the scan report).
    pub fn artwork_extracted(&self) -> u64 {
        self.artwork_extracted
    }

    /// Compute the per-`(folder, album)` compilation decision for this batch so
    /// every track of an album agrees on its album artist regardless of
    /// inconsistent per-track `album_artist` tags. Files are walked in directory
    /// order, so an album's tracks are contiguous and land in the same batch.
    pub fn begin_batch(&mut self, batch: &[ScannedFile]) {
        self.comp_decision = decide_compilation_albums(batch.iter().filter_map(|sf| {
            let meta = sf.metadata.as_ref()?;
            let album = meta.album.as_deref()?;
            let dir = std::path::Path::new(&sf.path)
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            Some((dir, album, meta.album_artist.as_deref(), meta.compilation))
        }));
    }

    /// Resolve artist + album, extract album cover / artist image as a side
    /// effect, and build the `Track` row. Returns `None` when the file has no
    /// metadata. `id` is left `None`; the caller sets it for the update path.
    pub fn import(&mut self, sf: &ScannedFile) -> Option<(Track, Option<i64>)> {
        let meta = sf.metadata.as_ref()?;

        // Compilation status: prefer the per-(folder,album) batch decision so
        // every track of the album agrees; fall back to this track's own signal
        // if the album was not seen whole in this batch (album straddles a batch
        // boundary, or an incremental scan touches a single track).
        let album_dir = std::path::Path::new(&sf.path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let is_compilation = meta
            .album
            .as_ref()
            .and_then(|a| {
                self.comp_decision
                    .get(&(album_dir.clone(), a.to_lowercase()))
                    .copied()
            })
            .unwrap_or_else(|| {
                meta.compilation
                    || meta
                        .album_artist
                        .as_deref()
                        .map(is_various_artists)
                        .unwrap_or(false)
            });

        let album_artist_name = if is_compilation {
            "Various Artists".to_string()
        } else if let Some(aa) = meta.album_artist.as_deref() {
            aa.to_string()
        } else {
            // No album_artist tag: pin the album artist to the first track
            // artist seen in this folder so all of the album's tracks resolve to
            // a single album row instead of splitting per differing track artist.
            let track_a = meta.artist.as_deref().unwrap_or("Unknown Artist");
            self.dir_album_artist
                .entry(album_dir.clone())
                .or_insert_with(|| track_a.to_string())
                .clone()
        };

        let track_artist_name = meta
            .artist
            .as_deref()
            .unwrap_or("Unknown Artist")
            .to_string();

        let album_artist_mbid = if is_compilation {
            None
        } else {
            meta.musicbrainz_album_artist_id
                .as_deref()
                .or(meta.musicbrainz_artist_id.as_deref())
        };
        let album_artist_entry = if let Some(cached) = self.artist_cache.get(&album_artist_name) {
            Some(Arc::clone(cached))
        } else {
            let result = self
                .artist_repo
                .get_or_create(
                    &album_artist_name,
                    album_artist_mbid,
                    meta.album_artist_sort.as_deref(),
                )
                .ok()
                .map(Arc::new);
            if let Some(ref a) = result {
                self.artist_cache
                    .insert(album_artist_name.clone(), Arc::clone(a));
            }
            result
        };
        let album_artist_id = album_artist_entry.as_ref().and_then(|a| a.id);

        let track_artist = if is_compilation && track_artist_name != album_artist_name {
            if let Some(cached) = self.artist_cache.get(&track_artist_name) {
                Some(Arc::clone(cached))
            } else {
                let result = self
                    .artist_repo
                    .get_or_create(&track_artist_name, meta.musicbrainz_artist_id.as_deref(), None)
                    .ok()
                    .map(Arc::new);
                if let Some(ref a) = result {
                    self.artist_cache
                        .insert(track_artist_name.clone(), Arc::clone(a));
                }
                result
            }
        } else {
            album_artist_entry.clone()
        };
        let artist_id = track_artist.as_ref().and_then(|a| a.id);

        if let Some(ref album_title) = meta.album {
            let t = album_title.to_lowercase();
            if t.contains("best") || t.contains("greatest") || t.contains("hits") {
                tracing::info!(
                    album = %album_title,
                    album_artist_tag = ?meta.album_artist,
                    artist_tag = ?meta.artist,
                    resolved_album_artist = album_artist_name.as_str(),
                    resolved_artist_id = ?album_artist_id,
                    resolved_artist_name = ?album_artist_entry.as_ref().map(|a| &a.name),
                    year = ?meta.year,
                    file = %sf.path,
                    "DIAG_generic_album_scan"
                );
            }
        }

        let album_key = meta.album.as_ref().map(|t| {
            let title = if self.quality_split {
                let suffix =
                    tune_core::scanner::quality::quality_suffix(meta.sample_rate, meta.bit_depth);
                if suffix.is_empty() {
                    t.clone()
                } else {
                    format!("{t} ({suffix})")
                }
            } else {
                t.clone()
            };
            (title, album_artist_id.unwrap_or(0), meta.year.map(|y| y as i32))
        });

        let album = if let Some(ref key) = album_key {
            if let Some(cached) = self.album_cache.get(key) {
                let c = Arc::clone(cached);
                if c.artist_id != Some(key.1) {
                    tracing::warn!(
                        album = %key.0,
                        cache_key_artist_id = key.1,
                        cached_album_id = ?c.id,
                        cached_album_artist_id = ?c.artist_id,
                        file = %sf.path,
                        "BUG_album_cache_artist_mismatch"
                    );
                }
                Some(c)
            } else {
                let result = self.album_repo.get_or_create_with_mbid(
                    &key.0,
                    key.1,
                    key.2,
                    meta.musicbrainz_release_id.as_deref(),
                );
                if let Err(ref e) = result {
                    tracing::warn!(
                        album = %key.0,
                        artist_id = key.1,
                        year = ?key.2,
                        error = %e,
                        file = %sf.path,
                        "BUG_album_create_failed"
                    );
                }
                let result = result.ok().map(Arc::new);
                if let Some(ref a) = result {
                    if a.artist_id != Some(key.1) {
                        tracing::warn!(
                            album = %key.0,
                            requested_artist_id = key.1,
                            returned_album_id = ?a.id,
                            returned_artist_id = ?a.artist_id,
                            mb_release_id = ?meta.musicbrainz_release_id,
                            file = %sf.path,
                            "BUG_album_artist_mismatch"
                        );
                    }
                    self.album_cache.insert(key.clone(), Arc::clone(a));
                }
                result
            }
        } else {
            None
        };

        let album_id = album.as_ref().and_then(|a| a.id);

        // Propagate date metadata from track tags to the album.
        if let Some(aid) = album_id {
            self.album_repo
                .update_dates(
                    aid,
                    meta.year.map(|y| y as i32),
                    meta.original_year.map(|y| y as i32),
                    meta.release_date.as_deref(),
                    meta.original_date.as_deref(),
                )
                .ok();
        }

        if let Some(aid) = album_id
            && !self.albums_with_cover.contains(&aid)
        {
            // Prefer the embedded cover already read while parsing the tags —
            // re-opening the file to extract it failed (os error 3) for some
            // accented Windows paths even though the first read had succeeded.
            let cover_hash = match meta.cover_art.as_ref() {
                Some(cover) => tune_core::library::artwork::save_embedded_cover(
                    std::path::Path::new(&sf.path),
                    &self.cache_dir,
                    cover,
                ),
                None => tune_core::library::artwork::get_or_extract(
                    std::path::Path::new(&sf.path),
                    &self.cache_dir,
                ),
            };
            if let Some(hash) = cover_hash {
                if let Err(e) = self.album_repo.update_cover_path(aid, &hash) {
                    tracing::warn!(album_id = aid, error = %e, "cover_path_update_failed");
                }
                self.albums_with_cover.insert(aid);
                self.artwork_extracted += 1;
            }
        }

        // Check for a local artist image (artist.jpg/png next to the tracks).
        if let Some(ref art) = track_artist {
            if art.image_path.is_none() {
                if let Some(parent) = std::path::Path::new(&sf.path).parent() {
                    for name in &["artist.jpg", "artist.png", "Artist.jpg", "Artist.png"] {
                        let candidate = parent.join(name);
                        if candidate.exists() {
                            let hash = tune_core::library::artwork::artwork_hash(
                                &candidate.to_string_lossy(),
                            );
                            let ext = candidate
                                .extension()
                                .and_then(|e| e.to_str())
                                .unwrap_or("jpg");
                            // Only record the image if the cache write succeeded —
                            // otherwise the DB claims "has image" with nothing on
                            // disk (grey square + permanent skip).
                            let saved = std::fs::read(&candidate).ok().and_then(|data| {
                                tune_core::library::artwork::save_to_cache(
                                    &data,
                                    &self.cache_dir,
                                    &hash,
                                    ext,
                                )
                            });
                            if saved.is_none() {
                                tracing::warn!(
                                    artist = %art.name,
                                    candidate = %candidate.display(),
                                    "artist_image_cache_write_failed_not_recording"
                                );
                                continue;
                            }
                            let mut updated_artist = tune_core::db::models::Artist::clone(art);
                            updated_artist.image_path = Some(hash);
                            updated_artist.image_source = Some("local".to_string());
                            if let Err(e) = self.artist_repo.update(&updated_artist) {
                                tracing::warn!(error = %e, "artist_image_update_failed");
                            }
                            break;
                        }
                    }
                }
            }
        }

        let track = build_track_row(meta, sf, album_id, artist_id, &track_artist_name);
        Some((track, album_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::metadata::{TrackCredit, TrackMetadata};
    use tune_core::scanner::walker::ScannedFile;

    fn sf(path: &str) -> ScannedFile {
        ScannedFile {
            path: path.to_string(),
            metadata: None,
            audio_hash: Some("hash-1".into()),
            file_size: 4096,
            mtime: 1_700_000_000,
        }
    }

    #[test]
    fn build_genres_json_prefers_parsed_list() {
        let g = build_genres_json(&["Jazz".into(), "Fusion".into()], Some("ignored"));
        assert_eq!(g.as_deref(), Some(r#"["Jazz","Fusion"]"#));
    }

    #[test]
    fn build_genres_json_falls_back_to_single_tag_split() {
        // Empty parsed list → split the legacy single tag.
        let g = build_genres_json(&[], Some("Jazz; Fusion"));
        assert_eq!(g.as_deref(), Some(r#"["Jazz","Fusion"]"#));
        // Nothing at all → None (not an empty-array string).
        assert_eq!(build_genres_json(&[], None), None);
        assert_eq!(build_genres_json(&[], Some("")), None);
    }

    #[test]
    fn build_track_row_maps_every_field_incl_previously_dropped_ones() {
        let meta = TrackMetadata {
            title: Some("So What".into()),
            album: Some("Kind of Blue".into()),
            album_artist: Some("Miles Davis".into()),
            disc_number: Some(1),
            disc_subtitle: Some("Side A".into()),
            track_number: Some(1),
            duration_ms: Some(544_000),
            sample_rate: Some(44_100),
            bit_depth: Some(24),
            channels: Some(2),
            format: Some("flac".into()),
            year: Some(1959),
            bpm: Some(136.0),
            label: Some("Columbia".into()),
            isrc: Some("USSM15900001".into()),
            musicbrainz_recording_id: Some("rec-1".into()),
            comment: Some("remaster".into()),
            genres: vec!["Jazz".into(), "Modal".into()],
            genre: Some("Jazz".into()),
            credits: vec![TrackCredit {
                name: "Miles Davis".into(),
                role: "composer".into(),
                instrument: None,
            }],
            ..Default::default()
        };
        let track = build_track_row(&meta, &sf("/m/kob/01.flac"), Some(7), Some(3), "Miles Davis");

        assert_eq!(track.id, None);
        assert_eq!(track.title, "So What");
        assert_eq!(track.album_id, Some(7));
        assert_eq!(track.artist_id, Some(3));
        assert_eq!(track.artist_name.as_deref(), Some("Miles Davis"));
        assert_eq!(track.album_title.as_deref(), Some("Kind of Blue"));
        // disc_subtitle was dropped by the old manual *insert* path.
        assert_eq!(track.disc_subtitle.as_deref(), Some("Side A"));
        assert_eq!(track.duration_ms, 544_000);
        assert_eq!(track.sample_rate, Some(44_100));
        assert_eq!(track.bit_depth, Some(24));
        assert_eq!(track.channels, 2);
        assert_eq!(track.file_path.as_deref(), Some("/m/kob/01.flac"));
        assert_eq!(track.file_size, Some(4096));
        assert_eq!(track.audio_hash.as_deref(), Some("hash-1"));
        // genres + composer were dropped by the old auto/watcher helper.
        assert_eq!(track.genres.as_deref(), Some(r#"["Jazz","Modal"]"#));
        assert_eq!(track.composer.as_deref(), Some("Miles Davis"));
        assert_eq!(track.year, Some(1959));
        assert_eq!(track.bpm, Some(136.0));
        assert_eq!(track.isrc.as_deref(), Some("USSM15900001"));
        assert_eq!(track.comments.as_deref(), Some("remaster"));
    }

    #[test]
    fn build_track_row_title_falls_back_to_file_stem_and_defaults() {
        let meta = TrackMetadata::default();
        let track = build_track_row(&meta, &sf("/m/x/Untitled Take.flac"), None, None, "Unknown Artist");
        assert_eq!(track.title, "Untitled Take");
        // Sensible defaults when tags are absent.
        assert_eq!(track.disc_number, 1);
        assert_eq!(track.track_number, 0);
        assert_eq!(track.channels, 2);
        assert_eq!(track.duration_ms, 0);
        assert_eq!(track.genres, None);
        assert_eq!(track.composer, None);
    }
}
