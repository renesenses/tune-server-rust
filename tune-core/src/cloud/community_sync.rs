use std::collections::HashMap;
use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::cloud::rate_limit::{self, CloudScope};
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::settings_repo::SettingsRepo;

const COMMUNITY_API: &str = "https://mozaiklabs.fr/api/v1/community/library";

/// Vademecum "extra" keys shared with the cloud (composer, conductor, …).
const EXTRA_KEYS: &[&str] = &[
    "composer",
    "lyricist",
    "writer",
    "arranger",
    "conductor",
    "performer",
    "ensemble",
    "remixer",
    "producer",
];

/// Re-pull a track's extra metadata if its last sync is older than this, so
/// values the cloud derived *after* we first synced a track get picked up.
const EXTRA_RESWEEP_SECS: u64 = 14 * 24 * 60 * 60;

/// Re-attempt MBID resolution for a track the community pool couldn't resolve if
/// its last attempt is older than this. Every attempted track is stamped so the
/// sweep advances across the whole library instead of retrying the same first 100
/// rows each cycle; unresolved tracks are re-tried later, by which time the cloud
/// backfill has usually widened coverage.
const RESOLVE_RETRY_SECS: u64 = 7 * 24 * 60 * 60;

/// Current Unix time in seconds, as a fixed-width string that sorts chronologically.
fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn request_deferred(backend: &Arc<dyn DbBackend>, scope: CloudScope) -> bool {
    let settings = SettingsRepo::with_backend(backend.clone());
    let Some(backoff) = rate_limit::active(&settings, scope) else {
        return false;
    };
    warn!(
        scope = backoff.scope,
        until_epoch = backoff.until_epoch,
        retry_after_seconds = backoff.retry_after_seconds,
        "community_sync_deferred_rate_limit"
    );
    true
}

fn persist_rate_limit(
    backend: &Arc<dyn DbBackend>,
    scope: CloudScope,
    response: &reqwest::Response,
) {
    if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
        return;
    }
    let settings = SettingsRepo::with_backend(backend.clone());
    if let Some(backoff) = rate_limit::defer_from_headers(&settings, scope, response.headers()) {
        warn!(
            scope = backoff.scope,
            until_epoch = backoff.until_epoch,
            retry_after_seconds = backoff.retry_after_seconds,
            "community_sync_rate_limit_persisted"
        );
    }
}

/// Push enriched tracks (those with a MusicBrainz recording ID) to
/// mozaiklabs.fr so other Tune instances can benefit from the metadata.
/// Returns the number of tracks stored server-side.
pub async fn sync_enriched_tracks(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
    instance_id: &str,
) -> Result<usize, String> {
    if request_deferred(backend, CloudScope::CommunityTracks) {
        return Ok(0);
    }
    // Query tracks with musicbrainz_recording_id set
    let rows = backend
        .query_many(
            "SELECT t.musicbrainz_recording_id, t.title, ar.name, al.title, t.genre, t.year, \
             t.composer, t.label, t.isrc, t.format, t.sample_rate, t.bit_depth \
             FROM tracks t \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             LEFT JOIN albums al ON t.album_id = al.id \
             WHERE t.musicbrainz_recording_id IS NOT NULL \
               AND t.musicbrainz_recording_id != '' \
             LIMIT 100",
            &[],
        )
        .map_err(|e| format!("query: {e}"))?;

    if rows.is_empty() {
        debug!("community_sync_no_tracks");
        return Ok(0);
    }

    let tracks: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "musicbrainz_recording_id": r.get(0).and_then(|v| v.as_string()),
                "title": r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "artist_name": r.get(2).and_then(|v| v.as_string()),
                "album_title": r.get(3).and_then(|v| v.as_string()),
                "genre": r.get(4).and_then(|v| v.as_string()),
                "year": r.get(5).and_then(|v| v.as_i64()),
                "composer": r.get(6).and_then(|v| v.as_string()),
                "label": r.get(7).and_then(|v| v.as_string()),
                "isrc": r.get(8).and_then(|v| v.as_string()),
                "format": r.get(9).and_then(|v| v.as_string()),
                "sample_rate": r.get(10).and_then(|v| v.as_i64()),
                "bit_depth": r.get(11).and_then(|v| v.as_i64()),
            })
        })
        .collect();

    let body = serde_json::json!({
        "instance_id": instance_id,
        "tracks": tracks,
    });

    let resp = http_client
        .post(format!("{COMMUNITY_API}/tracks"))
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("community sync: {e}"))?;

    persist_rate_limit(backend, CloudScope::CommunityTracks, &resp);
    if !resp.status().is_success() {
        return Err(format!("community sync: HTTP {}", resp.status()));
    }

    let result: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
    let stored = result["stored"].as_i64().unwrap_or(0) as usize;
    info!(stored, "community_tracks_synced");
    Ok(stored)
}

/// Pull enriched metadata from the community cloud and apply it to
/// local tracks that are missing genre/year/etc. Only fills in NULL
/// fields — never overwrites existing local metadata.
pub async fn pull_community_enrichments(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
) -> Result<usize, String> {
    if request_deferred(backend, CloudScope::CommunityEnriched) {
        return Ok(0);
    }
    let resp = http_client
        .get(format!("{COMMUNITY_API}/enriched"))
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("community pull: {e}"))?;

    persist_rate_limit(backend, CloudScope::CommunityEnriched, &resp);
    if !resp.status().is_success() {
        return Err(format!("community pull: HTTP {}", resp.status()));
    }

    let data: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
    let total = data["tracks"].as_array().map(|a| a.len()).unwrap_or(0);

    let mut applied = 0usize;
    if let Some(arr) = data["tracks"].as_array() {
        for t in arr {
            let mb_id = match t["musicbrainz_recording_id"].as_str() {
                Some(id) => id.to_string(),
                None => continue,
            };
            let genre = t["genre"].as_str().map(|s| s.to_string());
            let year = t["year"].as_i64().map(|v| v as i32);
            let composer = t["composer"].as_str().map(|s| s.to_string());
            let label = t["label"].as_str().map(|s| s.to_string());
            let isrc = t["isrc"].as_str().map(|s| s.to_string());

            let result = backend.execute(
                "UPDATE tracks SET \
                 genre = COALESCE(genre, ?), \
                 year = COALESCE(year, ?), \
                 composer = COALESCE(composer, ?), \
                 label = COALESCE(label, ?), \
                 isrc = COALESCE(isrc, ?) \
                 WHERE musicbrainz_recording_id = ? \
                 AND (genre IS NULL OR year IS NULL OR composer IS NULL)",
                &[
                    &genre as &dyn ToSqlValue,
                    &year as &dyn ToSqlValue,
                    &composer as &dyn ToSqlValue,
                    &label as &dyn ToSqlValue,
                    &isrc as &dyn ToSqlValue,
                    &mb_id as &dyn ToSqlValue,
                ],
            );
            if result.is_ok() {
                applied += 1;
            }
        }
    }

    info!(pulled = total, applied, "community_enrichments_pulled");
    Ok(applied)
}

/// Resolve MusicBrainz recording IDs for local tracks that don't have one, by
/// asking the community pool (which aggregates MBIDs from many libraries). This
/// breaks the chicken-and-egg where a client needs an MBID to benefit from
/// community metadata but only has a handful. Fills `musicbrainz_recording_id`
/// (and `genre` when empty); never overwrites an MBID that already exists.
///
/// Each attempted track is stamped with a `mb_resolve_tried` = <unix-seconds>
/// sentinel so the sweep advances across the whole library instead of retrying
/// the same first 100 rows every cycle; an unresolved track is re-tried once its
/// stamp is older than `RESOLVE_RETRY_SECS`, catching MBIDs the cloud backfill
/// added after our first pass.
pub async fn resolve_missing_mbids(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
) -> Result<usize, String> {
    if request_deferred(backend, CloudScope::CommunityResolve) {
        return Ok(0);
    }
    // Local tracks lacking an MBID but with an artist + title to match on, that
    // we haven't attempted recently. The `mb_resolve_tried` sentinel (stamped
    // below for every attempted track) slides the window forward each cycle, so a
    // batch that the cloud can't yet resolve doesn't wedge the sweep on the same
    // first 100 rows. The stamp is a fixed-width epoch string, so a lexical `>=`
    // compare is chronological.
    let cutoff = now_epoch_secs()
        .saturating_sub(RESOLVE_RETRY_SECS)
        .to_string();
    let rows = backend
        .query_many(
            "SELECT t.id, ar.name, al.title, t.title \
             FROM tracks t \
             LEFT JOIN artists ar ON t.artist_id = ar.id \
             LEFT JOIN albums al ON t.album_id = al.id \
             WHERE (t.musicbrainz_recording_id IS NULL OR t.musicbrainz_recording_id = '') \
               AND ar.name IS NOT NULL AND ar.name != '' \
               AND t.title IS NOT NULL AND t.title != '' \
               AND NOT EXISTS ( \
                 SELECT 1 FROM track_metadata m \
                 WHERE m.track_id = t.id AND m.key = 'mb_resolve_tried' AND m.value >= ? \
               ) \
             LIMIT 100",
            &[&cutoff as &dyn ToSqlValue],
        )
        .map_err(|e| format!("query: {e}"))?;

    if rows.is_empty() {
        debug!("community_resolve_no_candidates");
        return Ok(0);
    }

    // Keep row order: the server echoes back an `index` into the items array,
    // which we map to the corresponding track id.
    let ids: Vec<i64> = rows
        .iter()
        .filter_map(|r| r.get(0).and_then(|v| v.as_i64()))
        .collect();
    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "artist_name": r.get(1).and_then(|v| v.as_string()).unwrap_or_default(),
                "album_title": r.get(2).and_then(|v| v.as_string()),
                "title": r.get(3).and_then(|v| v.as_string()).unwrap_or_default(),
            })
        })
        .collect();

    // ids and items are both built from `rows`, so they must line up.
    if ids.len() != items.len() {
        return Err("row/id length mismatch".into());
    }

    let body = serde_json::json!({ "items": items });
    let resp = http_client
        .post(format!("{COMMUNITY_API}/resolve"))
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("community resolve: {e}"))?;

    persist_rate_limit(backend, CloudScope::CommunityResolve, &resp);
    if !resp.status().is_success() {
        return Err(format!("community resolve: HTTP {}", resp.status()));
    }

    let data: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;

    let mut applied = 0usize;
    if let Some(arr) = data["resolved"].as_array() {
        for r in arr {
            let idx = match r["index"].as_i64() {
                Some(i) if i >= 0 && (i as usize) < ids.len() => i as usize,
                _ => continue,
            };
            let mbid = match r["musicbrainz_recording_id"].as_str() {
                Some(id) if !id.is_empty() => id.to_string(),
                _ => continue,
            };
            let genre = r["genre"].as_str().map(|s| s.to_string());
            let track_id = ids[idx];

            let result = backend.execute(
                "UPDATE tracks SET \
                 musicbrainz_recording_id = ?, \
                 genre = COALESCE(genre, ?) \
                 WHERE id = ? AND (musicbrainz_recording_id IS NULL OR musicbrainz_recording_id = '')",
                &[
                    &mbid as &dyn ToSqlValue,
                    &genre as &dyn ToSqlValue,
                    &track_id as &dyn ToSqlValue,
                ],
            );
            if result.is_ok() {
                applied += 1;
            }
        }
    }

    // Stamp every attempted track (resolved or not) so the next cycle moves on to
    // the following batch. Resolved tracks drop out of the candidate query anyway
    // (they now have an MBID); the stamp is what lets the sweep skip past tracks
    // the cloud couldn't resolve — until RESOLVE_RETRY_SECS elapses and they're
    // re-tried against a by-then larger pool.
    let repo = crate::db::track_metadata_repo::TrackMetadataRepo::with_backend(backend.clone());
    let stamp = now_epoch_secs().to_string();
    for track_id in &ids {
        let _ = repo.set(*track_id, "mb_resolve_tried", &stamp);
    }

    info!(candidates = ids.len(), applied, "community_mbids_resolved");
    Ok(applied)
}

/// Pull the community "extra metadata" (Vademecum k/v — composer, lyricist,
/// conductor, performer, …) for local tracks that already have an MBID, and
/// store it in `track_metadata`. Only fills keys the track doesn't already have
/// (never overwrites the user's own file tags). The `/extra` endpoint is served
/// from Tune's own cloud, so there is no MusicBrainz rate limit to respect here.
///
/// Each processed track is stamped with a `mb_extra_synced` = <unix-seconds>
/// sentinel so the sweep advances across the library; a track is re-pulled once
/// its stamp is older than `EXTRA_RESWEEP_SECS`, catching extra the cloud derived
/// after our first pass.
pub async fn pull_community_extra(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
) -> Result<usize, String> {
    if request_deferred(backend, CloudScope::CommunityExtraRead) {
        return Ok(0);
    }
    // Tracks with an MBID whose extra hasn't been synced recently. The sentinel
    // stores a fixed-width epoch string, so a lexical `>=` compare is chronological.
    let cutoff = now_epoch_secs()
        .saturating_sub(EXTRA_RESWEEP_SECS)
        .to_string();
    let rows = backend
        .query_many(
            "SELECT t.id, t.musicbrainz_recording_id \
             FROM tracks t \
             WHERE t.musicbrainz_recording_id IS NOT NULL AND t.musicbrainz_recording_id != '' \
               AND NOT EXISTS ( \
                 SELECT 1 FROM track_metadata m \
                 WHERE m.track_id = t.id AND m.key = 'mb_extra_synced' AND m.value >= ? \
               ) \
             LIMIT 100",
            &[&cutoff as &dyn ToSqlValue],
        )
        .map_err(|e| format!("query: {e}"))?;

    if rows.is_empty() {
        debug!("community_extra_no_candidates");
        return Ok(0);
    }

    let candidates: Vec<(i64, String)> = rows
        .iter()
        .filter_map(|r| {
            let id = r.get(0).and_then(|v| v.as_i64())?;
            let mbid = r.get(1).and_then(|v| v.as_string())?;
            Some((id, mbid))
        })
        .collect();

    // De-duplicate MBIDs for the request.
    let mut mbids: Vec<String> = candidates.iter().map(|(_, m)| m.clone()).collect();
    mbids.sort();
    mbids.dedup();
    let csv = mbids.join(",");

    let resp = http_client
        .get(format!("{COMMUNITY_API}/extra"))
        .query(&[("mbids", csv.as_str())])
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("community extra: {e}"))?;

    persist_rate_limit(backend, CloudScope::CommunityExtraRead, &resp);
    if !resp.status().is_success() {
        return Err(format!("community extra: HTTP {}", resp.status()));
    }

    let data: serde_json::Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;

    // Build mbid -> key -> [values]. `extra` is a JSON object when there are
    // results, or an empty array `[]` when the pool has nothing for these MBIDs.
    let mut by_mbid: HashMap<String, HashMap<String, Vec<String>>> = HashMap::new();
    if let Some(obj) = data["extra"].as_object() {
        for (mbid, pairs) in obj {
            if let Some(arr) = pairs.as_array() {
                let map = by_mbid.entry(mbid.clone()).or_default();
                for p in arr {
                    let key = match p["key"].as_str() {
                        Some(k) if !k.is_empty() => k.to_string(),
                        _ => continue,
                    };
                    let value = match p["value"].as_str() {
                        Some(v) if !v.is_empty() => v.to_string(),
                        _ => continue,
                    };
                    map.entry(key).or_default().push(value);
                }
            }
        }
    }

    let repo = crate::db::track_metadata_repo::TrackMetadataRepo::with_backend(backend.clone());
    let mut enriched = 0usize;
    for (track_id, mbid) in &candidates {
        if let Some(keys) = by_mbid.get(mbid) {
            // Don't overwrite the user's own tags: only set keys not present locally.
            let existing = repo.get_all(*track_id).unwrap_or_default();
            let mut wrote_any = false;
            for (key, values) in keys {
                if existing.contains_key(key) {
                    continue;
                }
                let joined = values.join("; ");
                if repo.set(*track_id, key, &joined).is_ok() {
                    wrote_any = true;
                }
            }
            if wrote_any {
                enriched += 1;
            }
        }
        // Stamp with the current epoch so the sweep advances and re-pulls later.
        let _ = repo.set(*track_id, "mb_extra_synced", &now_epoch_secs().to_string());
    }

    info!(
        candidates = candidates.len(),
        enriched, "community_extra_pulled"
    );
    Ok(enriched)
}

/// Contribute the user's hand-curated file-tag credits (composer, conductor,
/// performer, …) to the community pool, so others can benefit. Only tracks whose
/// extra is PRISTINE — an MBID present, but not yet pushed and not yet pulled —
/// are sent, so we never echo back the MusicBrainz data we ourselves fetched.
/// Values the server promotes to canonical require agreement from several users.
pub async fn push_local_extra(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
    instance_id: &str,
) -> Result<usize, String> {
    if request_deferred(backend, CloudScope::CommunityExtraWrite) {
        return Ok(0);
    }
    // Pristine candidates: MBID set, has at least one extra key, never pushed,
    // never pulled (so its extra is 100% the user's own tags).
    let key_list = EXTRA_KEYS
        .iter()
        .map(|k| format!("'{k}'"))
        .collect::<Vec<_>>()
        .join(",");
    let rows = backend
        .query_many(
            &format!(
                "SELECT DISTINCT t.id, t.musicbrainz_recording_id \
                 FROM tracks t \
                 JOIN track_metadata mv ON mv.track_id = t.id AND mv.key IN ({key_list}) \
                 WHERE t.musicbrainz_recording_id IS NOT NULL AND t.musicbrainz_recording_id != '' \
                   AND NOT EXISTS (SELECT 1 FROM track_metadata m \
                     WHERE m.track_id = t.id AND m.key = 'mb_extra_pushed') \
                   AND NOT EXISTS (SELECT 1 FROM track_metadata m \
                     WHERE m.track_id = t.id AND m.key = 'mb_extra_synced') \
                 LIMIT 50"
            ),
            &[],
        )
        .map_err(|e| format!("query: {e}"))?;

    if rows.is_empty() {
        debug!("community_push_extra_no_candidates");
        return Ok(0);
    }

    let candidates: Vec<(i64, String)> = rows
        .iter()
        .filter_map(|r| {
            let id = r.get(0).and_then(|v| v.as_i64())?;
            let mbid = r.get(1).and_then(|v| v.as_string())?;
            Some((id, mbid))
        })
        .collect();

    let repo = crate::db::track_metadata_repo::TrackMetadataRepo::with_backend(backend.clone());
    let mut items: Vec<serde_json::Value> = Vec::new();
    for (track_id, mbid) in &candidates {
        let meta = repo.get_all(*track_id).unwrap_or_default();
        for key in EXTRA_KEYS {
            if let Some(value) = meta.get(*key) {
                // A single stored value may hold several names joined with "; ".
                for v in value.split("; ") {
                    let v = v.trim();
                    if !v.is_empty() && v.len() <= 500 {
                        items.push(serde_json::json!({
                            "musicbrainz_recording_id": mbid,
                            "key": key,
                            "value": v,
                        }));
                    }
                }
            }
        }
    }

    // Mark every candidate as pushed even if it yielded no items, so the sweep
    // advances and we don't re-scan the same tracks each cycle.
    let stamp = now_epoch_secs().to_string();
    let mark_pushed = |cands: &[(i64, String)]| {
        for (track_id, _) in cands {
            let _ = repo.set(*track_id, "mb_extra_pushed", &stamp);
        }
    };

    if items.is_empty() {
        mark_pushed(&candidates);
        return Ok(0);
    }

    // The endpoint accepts up to 500 items; our 50-track window stays well under.
    items.truncate(500);
    let sent = items.len();
    let body = serde_json::json!({ "instance_id": instance_id, "items": items });

    let resp = http_client
        .post(format!("{COMMUNITY_API}/extra"))
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("community push extra: {e}"))?;

    persist_rate_limit(backend, CloudScope::CommunityExtraWrite, &resp);
    if !resp.status().is_success() {
        return Err(format!("community push extra: HTTP {}", resp.status()));
    }

    mark_pushed(&candidates);
    info!(
        tracks = candidates.len(),
        values = sent,
        "community_extra_pushed"
    );
    Ok(sent)
}

/// Spawn the periodic community sync task. Runs every 30 minutes,
/// gated behind the `community_sync_enabled` setting.
pub fn spawn(backend: Arc<dyn DbBackend>) {
    let client = match crate::http::client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "community_sync_client_build_failed");
            return;
        }
    };

    tokio::spawn(async move {
        // Wait 90s after startup before the first sync
        tokio::time::sleep(std::time::Duration::from_secs(90)).await;

        loop {
            let settings = crate::db::settings_repo::SettingsRepo::with_backend(backend.clone());
            let enabled = settings
                .get("community_sync_enabled")
                .ok()
                .flatten()
                .map(|v| v == "true")
                .unwrap_or(false);

            if enabled {
                // Resolve MBIDs first (no instance_id required) so freshly
                // resolved tracks can be pushed/enriched on this same cycle.
                if let Err(e) = resolve_missing_mbids(&backend, &client).await {
                    warn!(error = %e, "community_sync_resolve_failed");
                }

                let instance_id = settings
                    .get("instance_id")
                    .ok()
                    .flatten()
                    .unwrap_or_default();

                if !instance_id.is_empty() {
                    if let Err(e) = sync_enriched_tracks(&backend, &client, &instance_id).await {
                        warn!(error = %e, "community_sync_push_failed");
                    }
                    if let Err(e) = pull_community_enrichments(&backend, &client).await {
                        warn!(error = %e, "community_sync_pull_failed");
                    }
                    // Contribute the user's own file-tag credits (needs instance_id),
                    // before pulling so we only ever push pristine (un-pulled) extra.
                    if let Err(e) = push_local_extra(&backend, &client, &instance_id).await {
                        warn!(error = %e, "community_sync_push_extra_failed");
                    }
                } else {
                    debug!("community_sync_skipped_no_instance_id");
                }

                // Pull Vademecum extra metadata (served from Tune's own cloud,
                // no instance_id needed and no MusicBrainz rate limit involved).
                if let Err(e) = pull_community_extra(&backend, &client).await {
                    warn!(error = %e, "community_sync_extra_failed");
                }
            }

            // Every 30 minutes
            tokio::time::sleep(std::time::Duration::from_secs(1800)).await;
        }
    });
}
