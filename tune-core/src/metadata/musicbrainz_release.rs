//! MusicBrainz release lookup.
//!
//! Two shapes of question, because they need opposite queries:
//!
//! * "which release is this, exactly?" — [`lookup_release`] narrows the search
//!   with the track count and year and returns only a high-confidence hit.
//! * "which releases could this be?" — [`lookup_release_candidates`] deliberately
//!   does *not* constrain on track count, because that is precisely what tells a
//!   deluxe edition apart from the standard one, and returns the list with the
//!   details a human needs to choose (year, country, label, track count, format).
//!
//! Response parsing is split into pure functions so the field extraction — the
//! part that actually breaks when MusicBrainz reshapes its JSON — is testable
//! without a network call.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::debug;

const MB_API: &str = "https://musicbrainz.org/ws/2";
const MB_UA: &str = "TuneServer/1.0 (contact@mozaiklabs.fr)";
const MB_RATE_LIMIT_MS: u64 = 1100;

/// Below this score a search hit is noise rather than a match.
const MIN_CONFIDENT_SCORE: i32 = 80;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MBReleaseMatch {
    pub release_id: String,
    pub release_group_id: Option<String>,
    pub title: String,
    pub artist: String,
    pub score: i32,
    // -- Edition details --
    //
    // What lets someone pick between the seven releases MusicBrainz holds for a
    // popular album. All optional: MusicBrainz is a wiki and any of them can be
    // absent. `#[serde(default)]` keeps older stored payloads deserializable.
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub year: Option<u32>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub catalog_number: Option<String>,
    #[serde(default)]
    pub track_count: Option<u32>,
    #[serde(default)]
    pub disc_count: Option<u32>,
    /// `CD`, `Digital Media`, `12" Vinyl`, …
    #[serde(default)]
    pub media_format: Option<String>,
    /// MusicBrainz's own edition note, e.g. "deluxe edition", "reissue".
    #[serde(default)]
    pub disambiguation: Option<String>,
    /// `Official`, `Promotion`, `Bootleg`, …
    #[serde(default)]
    pub status: Option<String>,
}

/// One track of a chosen release.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MBTrack {
    /// Position within its disc, 1-based.
    pub position: u32,
    /// Disc (medium) number, 1-based.
    pub disc: u32,
    /// Printed number when it differs from the position (vinyl sides: `A1`).
    pub number: Option<String>,
    pub title: String,
    pub length_ms: Option<u64>,
    pub recording_id: Option<String>,
    /// Set only when the track credits someone other than the release artist —
    /// the useful case being a compilation.
    pub artist: Option<String>,
}

/// A release with its track listing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MBReleaseDetail {
    pub release_id: String,
    pub title: String,
    pub artist: String,
    pub date: Option<String>,
    pub year: Option<u32>,
    pub country: Option<String>,
    pub label: Option<String>,
    pub catalog_number: Option<String>,
    pub disc_count: u32,
    pub tracks: Vec<MBTrack>,
}

fn normalize(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn year_from_date(date: Option<&str>) -> Option<u32> {
    date?.get(0..4)?.parse().ok()
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Join an `artist-credit` array into a display string, honouring the
/// join phrases so "Queen & David Bowie" does not come out as "Queen David
/// Bowie".
fn artist_credit(v: &Value) -> String {
    let Some(credits) = v.get("artist-credit").and_then(|c| c.as_array()) else {
        return String::new();
    };
    let mut out = String::new();
    for credit in credits {
        let name = credit
            .get("name")
            .and_then(|n| n.as_str())
            .or_else(|| {
                credit
                    .get("artist")
                    .and_then(|a| a.get("name"))
                    .and_then(|n| n.as_str())
            })
            .unwrap_or("");
        out.push_str(name);
        if let Some(join) = credit.get("joinphrase").and_then(|j| j.as_str()) {
            out.push_str(join);
        }
    }
    out.trim().to_string()
}

/// Does this search hit plausibly refer to what we asked for?
///
/// MusicBrainz happily returns loosely-related releases; without this a search
/// for one album offers up the artist's whole discography as "candidates".
fn plausible(rel_title: &str, rel_artist: &str, want_title: &str, want_artist: &str) -> bool {
    let norm_title = normalize(want_title);
    let norm_rel_title = normalize(rel_title);
    if norm_rel_title != norm_title
        && !norm_title.contains(&norm_rel_title)
        && !norm_rel_title.contains(&norm_title)
    {
        return false;
    }

    let norm_artist = normalize(want_artist);
    let norm_rel_artist = normalize(rel_artist);
    if !norm_artist.is_empty()
        && !norm_rel_artist.is_empty()
        && !norm_artist.contains(&norm_rel_artist)
        && !norm_rel_artist.contains(&norm_artist)
    {
        return false;
    }

    true
}

/// Pull every plausible release out of a `/release` search response.
///
/// Pure: this is the part that breaks when the API reshapes, so it is tested
/// against captured payloads instead of the live service.
pub fn parse_search_results(
    data: &Value,
    want_title: &str,
    want_artist: &str,
) -> Vec<MBReleaseMatch> {
    let Some(releases) = data.get("releases").and_then(|r| r.as_array()) else {
        return Vec::new();
    };

    let mut out: Vec<MBReleaseMatch> = Vec::new();
    for rel in releases {
        let Some(release_id) = str_field(rel, "id") else {
            continue;
        };
        let rel_title = str_field(rel, "title").unwrap_or_default();
        let rel_artist = artist_credit(rel);

        if !plausible(&rel_title, &rel_artist, want_title, want_artist) {
            continue;
        }

        // Media: a release can span several discs, possibly of mixed formats.
        let media = rel.get("media").and_then(|m| m.as_array());
        let disc_count = media.map(|m| m.len() as u32).filter(|n| *n > 0);
        let media_format = media.and_then(|m| {
            let mut formats: Vec<String> =
                m.iter().filter_map(|x| str_field(x, "format")).collect();
            formats.dedup();
            if formats.is_empty() {
                None
            } else {
                Some(formats.join(" + "))
            }
        });
        // `track-count` at the top level covers the whole release; fall back to
        // summing the media when the search index omits it.
        let track_count = rel
            .get("track-count")
            .and_then(|t| t.as_u64())
            .map(|n| n as u32)
            .or_else(|| {
                media.map(|m| {
                    m.iter()
                        .filter_map(|x| x.get("track-count").and_then(|t| t.as_u64()))
                        .sum::<u64>() as u32
                })
            })
            .filter(|n| *n > 0);

        let label_info = rel.get("label-info").and_then(|l| l.as_array());
        let label = label_info.and_then(|infos| {
            infos
                .iter()
                .find_map(|i| i.get("label").and_then(|l| str_field(l, "name")))
        });
        let catalog_number =
            label_info.and_then(|infos| infos.iter().find_map(|i| str_field(i, "catalog-number")));

        let date = str_field(rel, "date");
        let country = str_field(rel, "country").or_else(|| {
            rel.get("release-events")
                .and_then(|e| e.as_array())
                .and_then(|events| {
                    events.iter().find_map(|e| {
                        e.get("area")
                            .and_then(|a| str_field(a, "iso-3166-1-codes"))
                            .or_else(|| e.get("area").and_then(|a| str_field(a, "name")))
                    })
                })
        });

        out.push(MBReleaseMatch {
            release_id,
            release_group_id: rel.get("release-group").and_then(|g| str_field(g, "id")),
            title: rel_title,
            artist: rel_artist,
            score: rel.get("score").and_then(|s| s.as_i64()).unwrap_or(0) as i32,
            year: year_from_date(date.as_deref()),
            date,
            country,
            label,
            catalog_number,
            track_count,
            disc_count,
            media_format,
            disambiguation: str_field(rel, "disambiguation"),
            status: str_field(rel, "status"),
        });
    }

    // The same release can surface twice across paginated indexes.
    out.dedup_by(|a, b| a.release_id == b.release_id);
    out
}

/// Order candidates for a human: best score first, and among equal scores the
/// one whose track count matches what is on disk.
fn rank_candidates(
    mut candidates: Vec<MBReleaseMatch>,
    track_hint: Option<u32>,
) -> Vec<MBReleaseMatch> {
    candidates.sort_by_key(|c| {
        let delta = match (track_hint, c.track_count) {
            (Some(hint), Some(tc)) => (tc as i64 - hint as i64).abs(),
            // Unknown track count sorts after a known mismatch of one track:
            // an edition we cannot compare is a weaker suggestion.
            (Some(_), None) => 2,
            _ => 0,
        };
        (std::cmp::Reverse(c.score), delta)
    });
    candidates
}

/// Parse a `/release/{id}?inc=recordings` response into a track listing.
pub fn parse_release_detail(data: &Value) -> Option<MBReleaseDetail> {
    let release_id = str_field(data, "id")?;
    let date = str_field(data, "date");

    let label_info = data.get("label-info").and_then(|l| l.as_array());
    let media = data.get("media").and_then(|m| m.as_array());

    let mut tracks: Vec<MBTrack> = Vec::new();
    if let Some(media) = media {
        for (idx, medium) in media.iter().enumerate() {
            // Trust the declared position; fall back to the array order, since
            // some releases omit it on single-disc media.
            let disc = medium
                .get("position")
                .and_then(|p| p.as_u64())
                .map(|n| n as u32)
                .unwrap_or(idx as u32 + 1);

            let Some(list) = medium.get("tracks").and_then(|t| t.as_array()) else {
                continue;
            };
            for (tidx, track) in list.iter().enumerate() {
                let recording = track.get("recording");
                let title = str_field(track, "title")
                    .or_else(|| recording.and_then(|r| str_field(r, "title")))
                    .unwrap_or_default();
                if title.is_empty() {
                    continue;
                }
                let length_ms = track.get("length").and_then(|l| l.as_u64()).or_else(|| {
                    recording
                        .and_then(|r| r.get("length"))
                        .and_then(|l| l.as_u64())
                });
                let credited = artist_credit(track);

                tracks.push(MBTrack {
                    position: track
                        .get("position")
                        .and_then(|p| p.as_u64())
                        .map(|n| n as u32)
                        .unwrap_or(tidx as u32 + 1),
                    disc,
                    number: str_field(track, "number"),
                    title,
                    length_ms,
                    recording_id: recording.and_then(|r| str_field(r, "id")),
                    artist: if credited.is_empty() {
                        None
                    } else {
                        Some(credited)
                    },
                });
            }
        }
    }

    Some(MBReleaseDetail {
        release_id,
        title: str_field(data, "title").unwrap_or_default(),
        artist: artist_credit(data),
        year: year_from_date(date.as_deref()),
        date,
        country: str_field(data, "country"),
        label: label_info.and_then(|infos| {
            infos
                .iter()
                .find_map(|i| i.get("label").and_then(|l| str_field(l, "name")))
        }),
        catalog_number: label_info
            .and_then(|infos| infos.iter().find_map(|i| str_field(i, "catalog-number"))),
        disc_count: media.map(|m| m.len() as u32).unwrap_or(0),
        tracks,
    })
}

// -- Network --

async fn mb_get(path: &str, params: &[(&str, String)]) -> Option<Value> {
    let client = crate::http::client::shared();
    let resp = client
        .get(format!("{MB_API}/{path}"))
        .query(params)
        .header("User-Agent", MB_UA)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .ok()?;

    if !resp.status().is_success() {
        debug!(status = %resp.status(), path = path, "mb_request_http_error");
        return None;
    }
    resp.json().await.ok()
}

/// Best-guess identification: narrow the query with everything we know and
/// return a hit only if MusicBrainz is confident.
pub async fn lookup_release(
    title: &str,
    artist: &str,
    track_count: Option<i32>,
    year: Option<i32>,
) -> Option<MBReleaseMatch> {
    let mut query_parts = vec![
        format!("release:\"{title}\""),
        format!("artist:\"{artist}\""),
    ];
    if let Some(tc) = track_count {
        query_parts.push(format!("tracks:{tc}"));
    }
    if let Some(y) = year {
        query_parts.push(format!("date:{y}"));
    }

    let data = mb_get(
        "release",
        &[
            ("query", query_parts.join(" AND ")),
            ("limit", "5".to_string()),
            ("fmt", "json".to_string()),
        ],
    )
    .await?;

    parse_search_results(&data, title, artist)
        .into_iter()
        .max_by_key(|m| m.score)
        .filter(|m| m.score >= MIN_CONFIDENT_SCORE)
}

/// Every plausible release for an album, for the user to choose from.
///
/// The query is deliberately loose — only title and artist. Constraining on the
/// track count would hide the very editions someone opens this list for: a
/// 15-track deluxe never comes back from a `tracks:14` search. The count is
/// used to *rank* instead, and is reported per candidate so the difference is
/// visible.
pub async fn lookup_release_candidates(
    title: &str,
    artist: &str,
    track_hint: Option<u32>,
    limit: usize,
) -> Vec<MBReleaseMatch> {
    if title.trim().is_empty() {
        return Vec::new();
    }

    let mut query_parts = vec![format!("release:\"{title}\"")];
    if !artist.trim().is_empty() {
        query_parts.push(format!("artist:\"{artist}\""));
    }

    // Ask for more than we show: the plausibility filter drops some, and
    // MusicBrainz mixes in loosely-related releases.
    let fetch = (limit * 3).clamp(10, 100);
    let Some(data) = mb_get(
        "release",
        &[
            ("query", query_parts.join(" AND ")),
            ("limit", fetch.to_string()),
            ("fmt", "json".to_string()),
        ],
    )
    .await
    else {
        return Vec::new();
    };

    let mut candidates = rank_candidates(parse_search_results(&data, title, artist), track_hint);
    candidates.truncate(limit);
    debug!(
        count = candidates.len(),
        title = title,
        "mb_release_candidates_found"
    );
    candidates
}

/// Fetch a chosen release with its track listing.
pub async fn lookup_release_detail(release_id: &str) -> Option<MBReleaseDetail> {
    if release_id.trim().is_empty() {
        return None;
    }
    let data = mb_get(
        &format!("release/{release_id}"),
        &[
            ("inc", "recordings+artist-credits+labels".to_string()),
            ("fmt", "json".to_string()),
        ],
    )
    .await?;
    parse_release_detail(&data)
}

pub async fn rate_limit_delay() {
    tokio::time::sleep(std::time::Duration::from_millis(MB_RATE_LIMIT_MS)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_text() {
        assert_eq!(normalize("Kind of Blue"), "kind of blue");
        assert_eq!(normalize("Hello, World!"), "hello world");
        assert_eq!(normalize("  spaces  "), "spaces");
    }

    #[test]
    fn mb_release_match_serde() {
        let m = MBReleaseMatch {
            release_id: "abc-123".into(),
            release_group_id: Some("def-456".into()),
            title: "Kind of Blue".into(),
            artist: "Miles Davis".into(),
            score: 95,
            ..Default::default()
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: MBReleaseMatch = serde_json::from_str(&json).unwrap();
        assert_eq!(back.score, 95);
        assert_eq!(back.release_id, "abc-123");
    }

    #[test]
    fn mb_release_match_deserializes_without_edition_fields() {
        // Payload stored before the edition details existed.
        let old =
            r#"{"release_id":"a","release_group_id":null,"title":"T","artist":"A","score":90}"#;
        let back: MBReleaseMatch = serde_json::from_str(old).unwrap();
        assert_eq!(back.score, 90);
        assert!(back.label.is_none());
        assert!(back.track_count.is_none());
    }

    #[test]
    fn normalize_empty() {
        assert_eq!(normalize(""), "");
    }

    #[test]
    fn normalize_unicode() {
        assert_eq!(normalize("Café Résumé"), "café résumé");
    }

    // -- Search parsing --

    /// Shape of a real `/release?query=` hit, trimmed to the fields we read.
    fn search_payload() -> Value {
        json!({
            "releases": [
                {
                    "id": "rel-standard",
                    "score": 100,
                    "title": "Absolution",
                    "status": "Official",
                    "date": "2003-09-29",
                    "country": "GB",
                    "track-count": 14,
                    "disambiguation": "",
                    "artist-credit": [{ "name": "Muse", "joinphrase": "" }],
                    "release-group": { "id": "rg-1" },
                    "label-info": [
                        { "catalog-number": "TAS 0002", "label": { "name": "Taste Media" } }
                    ],
                    "media": [{ "format": "CD", "track-count": 14 }]
                },
                {
                    "id": "rel-deluxe",
                    "score": 100,
                    "title": "Absolution",
                    "status": "Official",
                    "date": "2004-03-23",
                    "country": "US",
                    "track-count": 15,
                    "disambiguation": "US edition",
                    "artist-credit": [{ "name": "Muse", "joinphrase": "" }],
                    "release-group": { "id": "rg-1" },
                    "label-info": [{ "label": { "name": "Warner Bros." } }],
                    "media": [{ "format": "CD", "track-count": 15 }]
                },
                {
                    "id": "rel-other-album",
                    "score": 72,
                    "title": "Origin of Symmetry",
                    "artist-credit": [{ "name": "Muse", "joinphrase": "" }],
                    "media": [{ "format": "CD", "track-count": 12 }]
                }
            ]
        })
    }

    #[test]
    fn parses_edition_details() {
        let out = parse_search_results(&search_payload(), "Absolution", "Muse");
        assert_eq!(out.len(), 2, "the unrelated album must be filtered out");

        let std = &out[0];
        assert_eq!(std.release_id, "rel-standard");
        assert_eq!(std.year, Some(2003));
        assert_eq!(std.date.as_deref(), Some("2003-09-29"));
        assert_eq!(std.country.as_deref(), Some("GB"));
        assert_eq!(std.label.as_deref(), Some("Taste Media"));
        assert_eq!(std.catalog_number.as_deref(), Some("TAS 0002"));
        assert_eq!(std.track_count, Some(14));
        assert_eq!(std.disc_count, Some(1));
        assert_eq!(std.media_format.as_deref(), Some("CD"));
        assert_eq!(std.release_group_id.as_deref(), Some("rg-1"));
        // An empty disambiguation must not become Some("").
        assert!(std.disambiguation.is_none());

        assert_eq!(out[1].disambiguation.as_deref(), Some("US edition"));
    }

    #[test]
    fn filters_out_unrelated_titles() {
        let out = parse_search_results(&search_payload(), "Absolution", "Muse");
        assert!(!out.iter().any(|r| r.release_id == "rel-other-album"));
    }

    #[test]
    fn filters_out_other_artists() {
        let data = json!({
            "releases": [{
                "id": "x", "score": 100, "title": "Absolution",
                "artist-credit": [{ "name": "Some Tribute Band", "joinphrase": "" }]
            }]
        });
        assert!(parse_search_results(&data, "Absolution", "Muse").is_empty());
    }

    #[test]
    fn parse_search_handles_empty_and_malformed() {
        assert!(parse_search_results(&json!({}), "T", "A").is_empty());
        assert!(parse_search_results(&json!({"releases": []}), "T", "A").is_empty());
        // A hit with no id is unusable.
        let no_id = json!({"releases": [{"score": 100, "title": "T"}]});
        assert!(parse_search_results(&no_id, "T", "").is_empty());
    }

    #[test]
    fn sums_track_count_across_discs_when_absent() {
        let data = json!({
            "releases": [{
                "id": "box", "score": 90, "title": "Absolution",
                "artist-credit": [{ "name": "Muse", "joinphrase": "" }],
                "media": [
                    { "format": "CD", "track-count": 14 },
                    { "format": "DVD", "track-count": 5 }
                ]
            }]
        });
        let out = parse_search_results(&data, "Absolution", "Muse");
        assert_eq!(out[0].track_count, Some(19));
        assert_eq!(out[0].disc_count, Some(2));
        assert_eq!(out[0].media_format.as_deref(), Some("CD + DVD"));
    }

    #[test]
    fn joins_collaboration_artists_with_their_join_phrase() {
        let data = json!({
            "releases": [{
                "id": "c", "score": 100, "title": "Under Pressure",
                "artist-credit": [
                    { "name": "Queen", "joinphrase": " & " },
                    { "name": "David Bowie", "joinphrase": "" }
                ]
            }]
        });
        let out = parse_search_results(&data, "Under Pressure", "Queen");
        assert_eq!(out[0].artist, "Queen & David Bowie");
    }

    #[test]
    fn reads_country_from_release_events() {
        let data = json!({
            "releases": [{
                "id": "e", "score": 100, "title": "Absolution",
                "artist-credit": [{ "name": "Muse", "joinphrase": "" }],
                "release-events": [{ "area": { "iso-3166-1-codes": "JP" } }]
            }]
        });
        let out = parse_search_results(&data, "Absolution", "Muse");
        assert_eq!(out[0].country.as_deref(), Some("JP"));
    }

    // -- Ranking --

    fn candidate(id: &str, score: i32, tracks: Option<u32>) -> MBReleaseMatch {
        MBReleaseMatch {
            release_id: id.into(),
            score,
            track_count: tracks,
            ..Default::default()
        }
    }

    #[test]
    fn ranks_by_score_first() {
        let out = rank_candidates(
            vec![
                candidate("low", 70, Some(14)),
                candidate("high", 100, Some(99)),
            ],
            Some(14),
        );
        assert_eq!(out[0].release_id, "high");
    }

    #[test]
    fn breaks_score_ties_on_track_count() {
        // Both scored 100; the one matching the 15 files on disk comes first.
        let out = rank_candidates(
            vec![
                candidate("std", 100, Some(14)),
                candidate("deluxe", 100, Some(15)),
            ],
            Some(15),
        );
        assert_eq!(out[0].release_id, "deluxe");
        assert_eq!(out[1].release_id, "std");
    }

    #[test]
    fn ranks_unknown_track_count_after_a_near_miss() {
        let out = rank_candidates(
            vec![
                candidate("unknown", 100, None),
                candidate("off-by-one", 100, Some(14)),
            ],
            Some(15),
        );
        assert_eq!(out[0].release_id, "off-by-one");
    }

    #[test]
    fn ranking_without_a_hint_keeps_score_order() {
        let out = rank_candidates(
            vec![candidate("a", 80, Some(14)), candidate("b", 95, None)],
            None,
        );
        assert_eq!(out[0].release_id, "b");
    }

    // -- Release detail parsing --

    fn detail_payload() -> Value {
        json!({
            "id": "rel-standard",
            "title": "Absolution",
            "date": "2003-09-29",
            "country": "GB",
            "artist-credit": [{ "name": "Muse", "joinphrase": "" }],
            "label-info": [{ "catalog-number": "TAS 0002", "label": { "name": "Taste Media" } }],
            "media": [
                {
                    "position": 1,
                    "format": "CD",
                    "tracks": [
                        { "id": "t1", "position": 1, "number": "1", "title": "Intro", "length": 22000,
                          "recording": { "id": "rec-1", "title": "Intro" } },
                        { "id": "t2", "position": 2, "number": "2", "title": "Apocalypse Please", "length": 197000,
                          "recording": { "id": "rec-2", "title": "Apocalypse Please" } }
                    ]
                },
                {
                    "position": 2,
                    "format": "DVD",
                    "tracks": [
                        { "id": "t3", "position": 1, "number": "1", "title": "Live at Glastonbury",
                          "recording": { "id": "rec-3" } }
                    ]
                }
            ]
        })
    }

    #[test]
    fn parses_track_listing_across_discs() {
        let d = parse_release_detail(&detail_payload()).unwrap();
        assert_eq!(d.release_id, "rel-standard");
        assert_eq!(d.artist, "Muse");
        assert_eq!(d.year, Some(2003));
        assert_eq!(d.label.as_deref(), Some("Taste Media"));
        assert_eq!(d.disc_count, 2);
        assert_eq!(d.tracks.len(), 3);

        assert_eq!(d.tracks[0].title, "Intro");
        assert_eq!(d.tracks[0].disc, 1);
        assert_eq!(d.tracks[0].position, 1);
        assert_eq!(d.tracks[0].length_ms, Some(22000));
        assert_eq!(d.tracks[0].recording_id.as_deref(), Some("rec-1"));

        assert_eq!(d.tracks[2].disc, 2);
        assert_eq!(d.tracks[2].title, "Live at Glastonbury");
    }

    #[test]
    fn falls_back_to_recording_title_and_array_order() {
        let data = json!({
            "id": "r",
            "title": "T",
            "media": [{
                "tracks": [
                    { "recording": { "title": "From the recording" } },
                    { "title": "Own title" }
                ]
            }]
        });
        let d = parse_release_detail(&data).unwrap();
        assert_eq!(d.tracks[0].title, "From the recording");
        assert_eq!(d.tracks[0].position, 1);
        assert_eq!(
            d.tracks[0].disc, 1,
            "single medium with no position is disc 1"
        );
        assert_eq!(d.tracks[1].position, 2);
    }

    #[test]
    fn keeps_printed_track_numbers() {
        let data = json!({
            "id": "r", "title": "T",
            "media": [{ "position": 1, "format": "12\" Vinyl", "tracks": [
                { "position": 1, "number": "A1", "title": "Side opener" }
            ]}]
        });
        let d = parse_release_detail(&data).unwrap();
        assert_eq!(d.tracks[0].number.as_deref(), Some("A1"));
    }

    #[test]
    fn detail_without_media_is_still_usable() {
        let data = json!({ "id": "r", "title": "T" });
        let d = parse_release_detail(&data).unwrap();
        assert_eq!(d.disc_count, 0);
        assert!(d.tracks.is_empty());
    }

    #[test]
    fn detail_requires_an_id() {
        assert!(parse_release_detail(&json!({ "title": "T" })).is_none());
    }

    #[test]
    fn skips_untitled_tracks() {
        let data = json!({
            "id": "r", "title": "T",
            "media": [{ "tracks": [{ "position": 1 }, { "position": 2, "title": "Real" }] }]
        });
        let d = parse_release_detail(&data).unwrap();
        assert_eq!(d.tracks.len(), 1);
        assert_eq!(d.tracks[0].title, "Real");
    }
}
