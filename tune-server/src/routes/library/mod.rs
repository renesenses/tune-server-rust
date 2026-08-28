mod albums;
mod albums_detailed;
mod ambiances;
mod artists;
mod artwork;
mod better_quality;
mod browse;
mod collections;
mod credits;
mod duplicates;
mod enrich;
mod facets;
mod folder_facet;
mod genres;
mod ingest;
mod proposals;
mod query_multi;
mod ratings;
mod reports;
mod search;
mod stats;
mod tracks;
pub(crate) mod write_tags;

use axum::Router;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::Value;

use crate::state::AppState;

#[derive(Deserialize)]
pub(super) struct Pagination {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct SearchQuery {
    pub q: String,
    pub limit: Option<i64>,
}

pub(super) const API_CACHE_TTL_SECS: i64 = 86400; // 24 hours

/// Body limit for a drag-and-dropped album on `/ingest/upload`. Sized for a
/// hi-res or DSD release, which a 50 MB default would refuse outright. The
/// handler streams each part to disk, so this bounds the request, not memory.
///
/// `saturating_mul` keeps this const-evaluable on 32-bit targets (Android
/// armv7): 8 GiB overflows a 32-bit `usize`, so there it caps at `usize::MAX`
/// (~4 GiB) — ample for any upload a 32-bit device could handle — while 64-bit
/// targets get the full 8 GiB unchanged.
const INGEST_UPLOAD_LIMIT: usize = 8usize
    .saturating_mul(1024)
    .saturating_mul(1024)
    .saturating_mul(1024);

pub(super) fn api_cache_get(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    key: &str,
) -> Option<Value> {
    use tune_core::db::backend::ToSqlValue;
    let row = backend
        .query_one(
            "SELECT value FROM settings WHERE key = ? AND \
             CAST(strftime('%s','now') AS INTEGER) - CAST(strftime('%s', updated_at) AS INTEGER) < ?",
            &[&key as &dyn ToSqlValue, &API_CACHE_TTL_SECS as &dyn ToSqlValue],
        )
        .ok()?
        .and_then(|r| r.first().and_then(|v| v.as_string()))?;
    serde_json::from_str(&row).ok()
}

pub(super) fn api_cache_set(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    key: &str,
    data: &Value,
) {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone());
    settings.set(key, &data.to_string()).ok();
}

pub(super) fn now_iso_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    // Approximate date from days since epoch
    let mut y = 1970i64;
    let mut d = days as i64;
    loop {
        let ylen = if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
            366
        } else {
            365
        };
        if d < ylen {
            break;
        }
        d -= ylen;
        y += 1;
    }
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let mdays = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut mo = 0usize;
    for (i, &ml) in mdays.iter().enumerate() {
        if d < ml as i64 {
            mo = i;
            break;
        }
        d -= ml as i64;
    }
    format!("{y:04}-{:02}-{:02}T{h:02}:{m:02}:{s:02}Z", mo + 1, d + 1)
}

pub(super) fn artwork_is_hex_hash(s: &str) -> bool {
    (s.len() == 32 || s.len() == 64) && s.chars().all(|c| c.is_ascii_hexdigit())
}

pub(crate) fn artwork_cache_dir() -> std::path::PathBuf {
    if let Ok(v) = std::env::var("TUNE_ARTWORK_DIR") {
        return std::path::PathBuf::from(v);
    }

    // On Windows, resolve relative artwork_cache to %LOCALAPPDATA%\TuneServer\
    // to avoid writing into read-only Program Files or an unpredictable CWD.
    #[cfg(target_os = "windows")]
    {
        let data_dir = std::env::var("LOCALAPPDATA")
            .map(|d| format!("{d}\\TuneServer"))
            .unwrap_or_else(|_| "TuneServer".into());
        return std::path::PathBuf::from(format!("{data_dir}\\artwork_cache"));
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            let app_support = std::path::PathBuf::from(home)
                .join("Library/Application Support/Tune/artwork_cache");
            return app_support;
        }
    }

    #[cfg(not(target_os = "windows"))]
    std::path::PathBuf::from("artwork_cache")
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/artists", get(artists::list_artists))
        .route(
            "/artists/{id}",
            get(artists::get_artist).put(artists::update_artist),
        )
        .route("/artists/{id}/albums", get(artists::artist_albums))
        .route("/artists/{id}/tracks", get(artists::artist_tracks))
        .route("/artists/{id}/bio", get(artists::artist_bio))
        .route("/artists/{id}/similar", get(artists::artist_similar))
        .route("/artists/{id}/metadata", get(artists::artist_metadata))
        .route(
            "/albums",
            get(albums::list_albums).post(albums::create_album),
        )
        .route("/albums/batch-update", post(albums::batch_update_albums))
        .route("/albums/count", get(albums::album_count))
        .route("/albums/filters", get(albums::album_filters))
        .route("/facets", get(facets::library_facets))
        .route("/albums-detailed", get(albums_detailed::albums_detailed))
        .route("/folder-facet", get(folder_facet::folder_facet))
        .route("/albums/recent", get(albums::recent_albums))
        .route("/albums/grouped", get(albums::albums_grouped))
        .route("/albums/{id}/completeness", get(albums::album_completeness))
        .route(
            "/albums/{id}",
            get(albums::get_album).put(albums::update_album),
        )
        .route("/albums/{id}/tracks", get(albums::album_tracks))
        .route(
            "/albums/{id}/metadata",
            get(albums::album_metadata_get).put(albums::album_metadata_put),
        )
        .route("/tracks", get(tracks::list_tracks))
        .route("/tracks/count", get(tracks::track_count))
        // PUT mirrors POST /metadata/tracks/{id}/edit so track editing lives on
        // the same REST family as albums/artists (PUT /library/…/{id}).
        .route(
            "/tracks/{id}",
            get(tracks::get_track).put(crate::routes::metadata::edit_track),
        )
        .route("/tracks/{id}/audio", get(tracks::stream_track_audio))
        .route("/tracks/{id}/rescan", post(tracks::rescan_track))
        .route("/tracks/{id}/waveform", get(tracks::track_waveform))
        .route("/tracks/{id}/similar", get(tracks::track_similar))
        .route("/tracks/{id}/versions", get(tracks::track_versions))
        .route(
            "/tracks/{id}/synced-lyrics",
            get(tracks::track_synced_lyrics),
        )
        .route("/identify", post(tracks::identify_track))
        .route("/tracks/{id}/source-links", get(tracks::track_source_links))
        .route("/tracks/{id}/quick-fav", post(tracks::quick_fav_track))
        .route("/albums/{id}/quick-fav", post(albums::quick_fav_album))
        .route(
            "/genre-tree",
            get(genres::genre_tree).put(genres::update_genre_tree),
        )
        .route("/albums/top-rated", get(albums::top_rated_albums))
        .route("/albums/{id}/rate", post(albums::rate_album))
        .route("/albums/{id}/rating", get(albums::get_album_rating))
        .route("/tracks/{id}/credits", get(credits::track_credits))
        .route("/artists/{id}/credits", get(credits::artist_credits))
        .route(
            "/tracks/{id}/credits/enrich",
            post(credits::enrich_track_credits),
        )
        .route(
            "/albums/{id}/credits/enrich",
            post(credits::enrich_album_credits),
        )
        .route("/enrich-credits", post(credits::enrich_all_credits))
        // Generic metadata reports (wrong cover / credit / bio / image).
        .route(
            "/reports",
            get(reports::list_reports).post(reports::create_report),
        )
        // Corrections que la communaute propose sur cette bibliotheque.
        .route("/proposals", get(proposals::list_proposals))
        .route("/proposals/{id}/decision", post(proposals::decide_proposal))
        .route("/proposals/auto-apply", post(proposals::set_auto_apply))
        .route("/tracks/{id}/all-tags", get(tracks::track_all_tags))
        .route(
            "/tracks/{id}/metadata",
            get(tracks::track_metadata_get).put(tracks::track_metadata_put),
        )
        // Ingest — bring an outside folder into the library. `upload` gets its
        // own body limit: the global 50 MB cap is fine for JSON but would
        // reject a dropped album on the first file.
        .route(
            "/ingest/settings",
            get(ingest::get_ingest_settings).put(ingest::put_ingest_settings),
        )
        .route("/ingest/analyze", post(ingest::analyze))
        .route("/ingest/release-tracks", post(ingest::release_tracks))
        .route("/ingest/plan", post(ingest::plan))
        .route("/ingest/apply", post(ingest::apply))
        .route("/ingest/jobs", get(ingest::list_jobs))
        .route("/ingest/jobs/{id}", get(ingest::get_job))
        .route("/ingest/jobs/{id}/undo", post(ingest::undo_job))
        .route(
            "/ingest/upload",
            post(ingest::upload).layer(axum::extract::DefaultBodyLimit::max(INGEST_UPLOAD_LIMIT)),
        )
        .route("/browse", get(browse::browse_roots))
        .route("/browse/dir", get(browse::browse_directory))
        .route("/folders", get(browse::browse_folders))
        .route("/genres", get(genres::list_genres))
        .route("/genres/rename", post(genres::rename_genre))
        .route("/genres/{name}/albums", get(genres::genre_albums))
        .route("/recommendations", get(albums::recommendations))
        .route("/stats/completeness", get(stats::completeness_stats))
        .route("/search", get(search::search))
        .route("/search/acoustic", post(search::acoustic_search))
        .route("/search/acoustic/status", get(search::acoustic_status))
        // Ambiances enregistrées : du stockage, pas de la recherche — donc
        // hors de la feature `audio-embedding`, sinon un serveur sans le
        // modèle perdrait aussi la liste.
        .route(
            "/ambiances",
            get(ambiances::list_ambiances).post(ambiances::create_ambiance),
        )
        .route(
            "/ambiances/{id}",
            axum::routing::patch(ambiances::update_ambiance).delete(ambiances::delete_ambiance),
        )
        .route("/stats", get(stats::library_stats))
        .route("/artwork/{hash}", get(artwork::serve_artwork))
        .route("/artwork/proxy", get(artwork::proxy_artwork))
        .route(
            "/albums/{id}/artwork",
            get(artwork::album_artwork).post(artwork::upload_album_artwork),
        )
        .route(
            "/albums/{id}/artwork/enrich",
            post(artwork::enrich_album_artwork),
        )
        .route("/artwork/enrich", post(artwork::batch_enrich_artwork))
        .route(
            "/artwork/enrich/status",
            get(artwork::batch_enrich_artwork_status),
        )
        .route(
            "/artwork/enrich-artists",
            post(artwork::batch_enrich_artist_artwork),
        )
        .route(
            "/artwork/enrich-artists/force",
            post(artwork::force_refetch_artist_artwork),
        )
        .route(
            "/artwork/enrich-artists/status",
            get(artwork::batch_enrich_artist_artwork_status),
        )
        .route("/duplicates", get(duplicates::list_duplicates))
        .route(
            "/tracks/{id}/better-quality",
            get(better_quality::track_better_quality),
        )
        .route(
            "/albums/{id}/better-quality",
            get(better_quality::album_better_quality),
        )
        .route("/duplicates/scan", post(duplicates::scan_duplicates))
        .route("/duplicates/resolve", post(duplicates::resolve_duplicate))
        .route("/activity", get(stats::library_activity))
        .route("/albums/{id}/bio", get(albums::album_bio))
        .route("/albums/{id}/similar", get(albums::album_similar))
        .route(
            "/albums/{id}/artwork/rescan",
            post(artwork::rescan_album_artwork),
        )
        .route(
            "/albums/merge-duplicates",
            post(albums::merge_duplicate_albums_route),
        )
        .route("/artists/{id}/image", get(artists::artist_image))
        .route("/artists/{id}/timeline", get(artists::artist_timeline))
        .route(
            "/artists/{id}/image/upload",
            post(artists::artist_image_upload),
        )
        .route(
            "/artists/{id}/image/report",
            post(artists::artist_image_report),
        )
        .route("/tracks/{id}/lyrics", get(tracks::track_lyrics))
        .route("/ratings/export", get(ratings::export_ratings))
        .route("/ratings/import", post(ratings::import_ratings))
        .route("/enrich-all", post(enrich::enrich_all_library))
        .route("/enrich-all/status", get(enrich::enrich_all_status))
        .route("/write-tags", post(write_tags::write_tags_to_files))
        .route("/write-tags/status", get(write_tags::write_tags_status))
        .route("/artwork/rescan", post(artwork::rescan_all_artwork))
        .route("/rescan-metadata", post(tracks::rescan_metadata))
        .route(
            "/rescan-metadata/status",
            get(tracks::rescan_metadata_status),
        )
        .route("/duplicates/smart", get(duplicates::smart_duplicates))
        .route(
            "/collections",
            get(collections::list_collections).post(collections::create_collection),
        )
        .route(
            "/collections/{id}",
            get(collections::get_collection).delete(collections::delete_collection),
        )
        .route(
            "/collections/{id}/albums",
            get(collections::collection_albums),
        )
        .route(
            "/collections/{id}/albums/{album_id}",
            post(collections::add_album_to_collection)
                .delete(collections::remove_album_from_collection),
        )
}
