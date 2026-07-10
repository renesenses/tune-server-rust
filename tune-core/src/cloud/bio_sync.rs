use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::db::album_repo::AlbumRepo;
use crate::db::artist_repo::ArtistRepo;
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

const UPLOAD_URL: &str = "https://mozaiklabs.fr/api/v1/community/bios";
const DOWNLOAD_ARTIST_URL: &str = "https://mozaiklabs.fr/api/v1/community/bios/artists";
const DOWNLOAD_ALBUM_URL: &str = "https://mozaiklabs.fr/api/v1/community/bios/albums";
const BATCH_SIZE: usize = 100;
const REQUEST_TIMEOUT_SECS: u64 = 10;

// ── Upload payload ────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ArtistBioEntry {
    name: String,
    musicbrainz_id: String,
    bio: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lang: Option<String>,
}

#[derive(Debug, Serialize)]
struct AlbumBioEntry {
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    artist_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    musicbrainz_id: Option<String>,
    bio: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    license: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lang: Option<String>,
}

#[derive(Debug, Serialize)]
struct BioUploadPayload {
    server_id: String,
    artists: Vec<ArtistBioEntry>,
    albums: Vec<AlbumBioEntry>,
}

// ── Download response ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ArtistBioResponse {
    musicbrainz_id: String,
    bio: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    lang: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AlbumBioResponse {
    musicbrainz_id: String,
    bio: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    lang: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ArtistBiosWrapper {
    #[serde(default)]
    artists: Vec<ArtistBioResponse>,
}

#[derive(Debug, Deserialize)]
struct AlbumBiosWrapper {
    #[serde(default)]
    albums: Vec<AlbumBioResponse>,
}

#[derive(Debug, Deserialize)]
struct AlbumByTitleBioResponse {
    title: String,
    artist_name: Option<String>,
    bio: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    lang: Option<String>,
}

/// Source label for a downloaded community bio — defaults to "community"
/// when the payload carries no explicit source.
fn bio_source(source: &Option<String>) -> &str {
    match source.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => "community",
    }
}

#[derive(Debug, Deserialize)]
struct AlbumByTitleBiosWrapper {
    #[serde(default)]
    albums: Vec<AlbumByTitleBioResponse>,
}

// ── Public entry points ───────────────────────────────────────────────────────

/// Upload local bios to mozaiklabs.fr in batches of 100.
/// Controlled by `TUNE_TELEMETRY` — returns early if telemetry is off.
/// Fails silently: errors are logged as warnings.
pub async fn upload_bios(db: &Arc<dyn DbBackend>) {
    if !crate::cloud::telemetry::TelemetryReporter::is_enabled() {
        return;
    }

    let settings = SettingsRepo::with_backend(db.clone());
    let server_id = crate::cloud::telemetry::TelemetryReporter::get_or_create_server_id(&settings);

    let artist_repo = ArtistRepo::with_backend(db.clone());
    let album_repo = AlbumRepo::with_backend(db.clone());

    let artists = match artist_repo.artists_with_bio_and_mbid() {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "bio_sync_artist_query_failed");
            return;
        }
    };

    let albums = match album_repo.albums_with_bio() {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "bio_sync_album_query_failed");
            return;
        }
    };

    if artists.is_empty() && albums.is_empty() {
        return;
    }

    let client = match crate::http::client::builder()
        .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "bio_sync_client_build_failed");
            return;
        }
    };

    // Send in batches so we don't build a giant single request.
    let artist_chunks: Vec<_> = artists.chunks(BATCH_SIZE).collect();
    let album_chunks: Vec<_> = albums.chunks(BATCH_SIZE).collect();
    let batch_count = artist_chunks.len().max(album_chunks.len()).max(1);

    for i in 0..batch_count {
        let artist_batch: Vec<ArtistBioEntry> = artist_chunks
            .get(i)
            .map(|chunk| {
                chunk
                    .iter()
                    .map(
                        |(name, mbid, bio, source, source_url, license, lang)| ArtistBioEntry {
                            name: name.clone(),
                            musicbrainz_id: mbid.clone(),
                            bio: bio.clone(),
                            source: source.clone(),
                            source_url: source_url.clone(),
                            license: license.clone(),
                            lang: lang.clone(),
                        },
                    )
                    .collect()
            })
            .unwrap_or_default();

        let album_batch: Vec<AlbumBioEntry> = album_chunks
            .get(i)
            .map(|chunk| {
                chunk
                    .iter()
                    .map(
                        |(title, artist_name, mbid, bio, source, source_url, license, lang)| {
                            AlbumBioEntry {
                                title: title.clone(),
                                artist_name: artist_name.clone(),
                                musicbrainz_id: mbid.clone(),
                                bio: bio.clone(),
                                source: source.clone(),
                                source_url: source_url.clone(),
                                license: license.clone(),
                                lang: lang.clone(),
                            }
                        },
                    )
                    .collect()
            })
            .unwrap_or_default();

        if artist_batch.is_empty() && album_batch.is_empty() {
            continue;
        }

        let payload = BioUploadPayload {
            server_id: server_id.clone(),
            artists: artist_batch,
            albums: album_batch,
        };

        match client.post(UPLOAD_URL).json(&payload).send().await {
            Ok(resp) if resp.status().is_success() => {
                info!(
                    batch = i,
                    artists = payload.artists.len(),
                    albums = payload.albums.len(),
                    "bio_sync_uploaded"
                );
            }
            Ok(resp) => {
                let status = resp.status();
                warn!(batch = i, status = %status, "bio_sync_upload_rejected");
            }
            Err(e) => {
                warn!(batch = i, error = %e, "bio_sync_upload_failed");
            }
        }
    }
}

/// Download community bios for artists that have no local bio.
/// Only downloads for artists/albums that already have a MusicBrainz ID.
/// Fails silently. Respects `TUNE_TELEMETRY` flag.
pub async fn download_bios(db: &Arc<dyn DbBackend>) {
    if !crate::cloud::telemetry::TelemetryReporter::is_enabled() {
        return;
    }

    let client = match crate::http::client::builder()
        .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "bio_download_client_build_failed");
            return;
        }
    };

    download_artist_bios(db, &client).await;
    download_album_bios(db, &client).await;
}

async fn download_artist_bios(db: &Arc<dyn DbBackend>, client: &reqwest::Client) {
    let artist_repo = ArtistRepo::with_backend(db.clone());

    let candidates = match artist_repo.artists_without_bio_with_mbid() {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "bio_download_artist_candidates_failed");
            return;
        }
    };

    if candidates.is_empty() {
        return;
    }

    for chunk in candidates.chunks(BATCH_SIZE) {
        let ids: Vec<&str> = chunk.iter().map(|(_, mbid)| mbid.as_str()).collect();
        let query = ids.join(",");

        let url = format!(
            "{DOWNLOAD_ARTIST_URL}?musicbrainz_ids={}",
            urlencoding::encode(&query)
        );

        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "bio_download_artists_request_failed");
                continue;
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            warn!(status = %status, "bio_download_artists_rejected");
            continue;
        }

        let wrapper: ArtistBiosWrapper = match resp.json().await {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "bio_download_artists_parse_failed");
                continue;
            }
        };

        // Build a lookup map from mbid -> artist_id
        let id_map: std::collections::HashMap<&str, i64> = chunk
            .iter()
            .map(|(id, mbid)| (mbid.as_str(), *id))
            .collect();

        let mut applied = 0usize;
        for entry in &wrapper.artists {
            if let Some(&artist_id) = id_map.get(entry.musicbrainz_id.as_str()) {
                if let Err(e) = artist_repo.update_bio_full(
                    artist_id,
                    &entry.bio,
                    bio_source(&entry.source),
                    entry.source_url.clone(),
                    entry.license.as_deref().unwrap_or(""),
                    entry.lang.as_deref().unwrap_or(""),
                ) {
                    warn!(artist_id, error = %e, "bio_download_artist_update_failed");
                } else {
                    applied += 1;
                }
            }
        }

        if applied > 0 {
            info!(applied, "bio_download_artists_applied");
        }
    }
}

async fn download_album_bios(db: &Arc<dyn DbBackend>, client: &reqwest::Client) {
    let album_repo = AlbumRepo::with_backend(db.clone());

    // Phase 1: download by MBID (existing path, for albums that have one)
    let mbid_candidates = match album_repo.albums_without_bio_with_mbid() {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "bio_download_album_candidates_failed");
            Vec::new()
        }
    };

    for chunk in mbid_candidates.chunks(BATCH_SIZE) {
        let ids: Vec<&str> = chunk.iter().map(|(_, mbid)| mbid.as_str()).collect();
        let query = ids.join(",");

        let url = format!(
            "{DOWNLOAD_ALBUM_URL}?musicbrainz_ids={}",
            urlencoding::encode(&query)
        );

        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "bio_download_albums_request_failed");
                continue;
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            warn!(status = %status, "bio_download_albums_rejected");
            continue;
        }

        let wrapper: AlbumBiosWrapper = match resp.json().await {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "bio_download_albums_parse_failed");
                continue;
            }
        };

        let id_map: std::collections::HashMap<&str, i64> = chunk
            .iter()
            .map(|(id, mbid)| (mbid.as_str(), *id))
            .collect();

        let mut applied = 0usize;
        for entry in &wrapper.albums {
            if let Some(&album_id) = id_map.get(entry.musicbrainz_id.as_str()) {
                if let Err(e) = album_repo.update_bio_full(
                    album_id,
                    &entry.bio,
                    bio_source(&entry.source),
                    entry.source_url.clone(),
                    entry.license.as_deref().unwrap_or(""),
                    entry.lang.as_deref().unwrap_or(""),
                ) {
                    warn!(album_id, error = %e, "bio_download_album_update_failed");
                } else {
                    applied += 1;
                }
            }
        }

        if applied > 0 {
            info!(applied, "bio_download_albums_by_mbid_applied");
        }
    }

    // Phase 2: download by title+artist (for albums without MBID)
    let title_candidates = match album_repo.albums_without_bio_without_mbid() {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "bio_download_album_title_candidates_failed");
            return;
        }
    };

    if title_candidates.is_empty() {
        return;
    }

    for chunk in title_candidates.chunks(BATCH_SIZE) {
        // Build JSON array of {title, artist_name} for the VPS endpoint
        let titles_json: Vec<serde_json::Value> = chunk
            .iter()
            .map(|(_, title, artist)| {
                serde_json::json!({
                    "title": title,
                    "artist_name": artist,
                })
            })
            .collect();

        let titles_param = serde_json::to_string(&titles_json).unwrap_or_default();
        let url = format!(
            "{DOWNLOAD_ALBUM_URL}?titles={}",
            urlencoding::encode(&titles_param)
        );

        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "bio_download_albums_by_title_request_failed");
                continue;
            }
        };

        if !resp.status().is_success() {
            let status = resp.status();
            warn!(status = %status, "bio_download_albums_by_title_rejected");
            continue;
        }

        let wrapper: AlbumByTitleBiosWrapper = match resp.json().await {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "bio_download_albums_by_title_parse_failed");
                continue;
            }
        };

        // Build a lookup map from (title, artist_name) -> album_id
        let id_map: std::collections::HashMap<(&str, Option<&str>), i64> = chunk
            .iter()
            .map(|(id, title, artist)| ((title.as_str(), artist.as_deref()), *id))
            .collect();

        let mut applied = 0usize;
        for entry in &wrapper.albums {
            let key = (entry.title.as_str(), entry.artist_name.as_deref());
            if let Some(&album_id) = id_map.get(&key) {
                if let Err(e) = album_repo.update_bio_full(
                    album_id,
                    &entry.bio,
                    bio_source(&entry.source),
                    entry.source_url.clone(),
                    entry.license.as_deref().unwrap_or(""),
                    entry.lang.as_deref().unwrap_or(""),
                ) {
                    warn!(album_id, error = %e, "bio_download_album_by_title_update_failed");
                } else {
                    applied += 1;
                }
            }
        }

        if applied > 0 {
            info!(applied, "bio_download_albums_by_title_applied");
        }
    }
}

/// Spawn a background task that:
/// - Waits 60s after startup, then uploads local bios and repeats every 24h.
/// - Listens for `library.scan.completed` events to trigger a community bio download.
pub fn spawn(
    db: Arc<dyn DbBackend>,
    mut scan_completed_rx: tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
) {
    // Upload task: 60s delay then every 24h
    let db_upload = db.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        loop {
            upload_bios(&db_upload).await;
            tokio::time::sleep(std::time::Duration::from_secs(86400)).await;
        }
    });

    // Download task: triggered after each library scan completes
    let db_download = db.clone();
    tokio::spawn(async move {
        loop {
            match scan_completed_rx.recv().await {
                Ok(ev) if ev.event_type == "library.scan.completed" => {
                    download_bios(&db_download).await;
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;

    fn fresh_db() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    #[test]
    fn artists_with_bio_and_mbid_empty() {
        let db = fresh_db();
        let repo = ArtistRepo::with_backend(db);
        let result = repo.artists_with_bio_and_mbid().unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn albums_with_bio_and_mbid_empty() {
        let db = fresh_db();
        let repo = AlbumRepo::with_backend(db);
        let result = repo.albums_with_bio_and_mbid().unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn artists_with_bio_and_mbid_filters_correctly() {
        use crate::db::models::Artist;

        let db = fresh_db();
        let repo = ArtistRepo::with_backend(db);

        // Artist with bio + mbid → should appear
        let mut a1 = Artist::new("Miles Davis".into());
        a1.musicbrainz_id = Some("mbid-1".into());
        a1.bio = Some("Jazz legend".into());
        let id1 = repo.create(&a1).unwrap();

        // Artist with bio but no mbid → should NOT appear
        let mut a2 = Artist::new("No MBID".into());
        a2.bio = Some("Some bio".into());
        repo.create(&a2).unwrap();

        // Artist with mbid but no bio → should NOT appear
        let mut a3 = Artist::new("No Bio".into());
        a3.musicbrainz_id = Some("mbid-3".into());
        repo.create(&a3).unwrap();

        let result = repo.artists_with_bio_and_mbid().unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "Miles Davis");
        assert_eq!(result[0].1, "mbid-1");
        assert_eq!(result[0].2, "Jazz legend");

        let without = repo.artists_without_bio_with_mbid().unwrap();
        assert_eq!(without.len(), 1);
        let (found_id, found_mbid) = &without[0];
        assert_eq!(*found_id, id1 + 2); // a3 is the third insert
        assert_eq!(found_mbid, "mbid-3");
    }

    #[test]
    fn albums_with_bio_and_mbid_filters_correctly() {
        use crate::db::models::Album;

        let db = fresh_db();
        let repo = AlbumRepo::with_backend(db);

        // Album with bio + release_group_id → should appear
        let mut a1 = Album::new("Kind of Blue".into());
        a1.musicbrainz_release_group_id = Some("rg-1".into());
        a1.bio = Some("Classic album".into());
        repo.create(&a1).unwrap();

        // Album with bio but no release_group_id → should NOT appear
        let mut a2 = Album::new("No MBID".into());
        a2.bio = Some("Some bio".into());
        repo.create(&a2).unwrap();

        // Album with release_group_id but no bio → should NOT appear
        let mut a3 = Album::new("No Bio".into());
        a3.musicbrainz_release_group_id = Some("rg-3".into());
        repo.create(&a3).unwrap();

        let result = repo.albums_with_bio_and_mbid().unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "Kind of Blue");
        assert_eq!(result[0].2, "rg-1");
        assert_eq!(result[0].3, "Classic album");

        let without = repo.albums_without_bio_with_mbid().unwrap();
        assert_eq!(without.len(), 1);
        assert_eq!(without[0].1, "rg-3");
    }

    #[test]
    fn albums_with_bio_returns_all_regardless_of_mbid() {
        use crate::db::models::Album;

        let db = fresh_db();
        let repo = AlbumRepo::with_backend(db);

        // Album with bio + MBID
        let mut a1 = Album::new("Kind of Blue".into());
        a1.musicbrainz_release_group_id = Some("rg-1".into());
        a1.bio = Some("Classic album".into());
        repo.create(&a1).unwrap();

        // Album with bio but NO MBID → should also appear
        let mut a2 = Album::new("No MBID Album".into());
        a2.bio = Some("Bio without MBID".into());
        repo.create(&a2).unwrap();

        // Album without bio → should NOT appear
        let _a3 = Album::new("No Bio".into());
        repo.create(&_a3).unwrap();

        let result = repo.albums_with_bio().unwrap();
        assert_eq!(result.len(), 2);

        // First album has MBID
        assert_eq!(result[0].0, "Kind of Blue");
        assert_eq!(result[0].2, Some("rg-1".to_string()));
        assert_eq!(result[0].3, "Classic album");

        // Second album has no MBID
        assert_eq!(result[1].0, "No MBID Album");
        assert_eq!(result[1].2, None);
        assert_eq!(result[1].3, "Bio without MBID");
    }
}
