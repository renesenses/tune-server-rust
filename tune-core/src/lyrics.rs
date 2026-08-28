//! Synchronized lyrics via LRCLIB.
//!
//! - Parses LRC-format timestamped lyrics into `Vec<LyricLine>` (delegates
//!   to the canonical parser in [`crate::metadata::lyrics`]).
//! - Fetches from <https://lrclib.net/api/get> (no API key required).
//! - Caches results in the `lyrics_cache` DB table (SQLite and Postgres),
//!   including negative results which are retried after 14 days.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};

/// Negative cache entries (no lyrics found) are retried after this delay.
pub const NEGATIVE_CACHE_TTL_DAYS: i64 = 14;

/// Process-wide count of LRCLIB replies that told us to slow down (HTTP 429
/// « Too Many Requests » or 503). Incremented by [`fetch_lrclib_raw`], never
/// reset — a caller snapshots it before a run and compares afterwards.
///
/// LRCLIB is a free community service with no API key: a batch pass that kept
/// hammering it after a 429 would get the whole instance banned, and that would
/// also kill the on-demand lookup done while listening. The background pass
/// (`crate::library::lyrics_pass`) watches this counter and stops on the first
/// hit. Same idiom as `ARTWORK_RATE_LIMIT_HITS` in `library::artwork`.
pub static LRCLIB_RATE_LIMIT_HITS: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A single time-stamped lyric line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LyricLine {
    /// Milliseconds from track start.
    pub time_ms: i64,
    /// The lyric text for this line.
    pub text: String,
}

/// Full lyrics payload returned by the API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lyrics {
    /// Whether time-synced lyrics are available.
    pub synced: bool,
    /// Parsed time-stamped lines (empty when `synced` is false).
    pub lines: Vec<LyricLine>,
    /// Plain (unsynced) lyrics text.
    pub plain_text: Option<String>,
    /// Attribution source.
    pub source: String,
}

// ---------------------------------------------------------------------------
// LRCLIB API response
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LrclibResponse {
    synced_lyrics: Option<String>,
    plain_lyrics: Option<String>,
}

/// Raw LRCLIB result: original `syncedLyrics` (LRC text) and `plainLyrics`.
#[derive(Debug, Clone, Default)]
pub struct LrclibRaw {
    pub synced_lyrics: Option<String>,
    pub plain_lyrics: Option<String>,
}

impl LrclibRaw {
    pub fn is_empty(&self) -> bool {
        self.synced_lyrics
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
            && self
                .plain_lyrics
                .as_deref()
                .is_none_or(|s| s.trim().is_empty())
    }
}

// ---------------------------------------------------------------------------
// LRC parser (canonical implementation lives in metadata::lyrics)
// ---------------------------------------------------------------------------

/// Parse an LRC-format string into a sorted `Vec<LyricLine>`.
///
/// Accepted format per line: `[MM:SS.xx] text` where `xx` can be 1-3
/// digits (centiseconds or milliseconds). Several timestamps per line are
/// supported and metadata tags (`[ar:..]`, `[ti:..]`…) are ignored.
pub fn parse_lrc(lrc: &str) -> Vec<LyricLine> {
    crate::metadata::lyrics::parse_lrc(lrc)
        .into_iter()
        .map(|l| LyricLine {
            time_ms: l.time_ms as i64,
            text: l.text,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// LRCLIB fetch
// ---------------------------------------------------------------------------

/// Fetch raw lyrics from LRCLIB for a given artist/track/album/duration.
///
/// Returns `Ok(None)` when LRCLIB has no entry (HTTP 404), `Err` on
/// network/protocol failures. Short 5 s timeout: this is called from an
/// interactive endpoint and must fail fast.
pub async fn fetch_lrclib_raw(
    client: &reqwest::Client,
    artist: &str,
    track_name: &str,
    album_name: Option<&str>,
    duration_secs: Option<i64>,
) -> Result<Option<LrclibRaw>, String> {
    let mut url = format!(
        "https://lrclib.net/api/get?artist_name={}&track_name={}",
        urlencoding::encode(artist),
        urlencoding::encode(track_name),
    );

    if let Some(album) = album_name.filter(|a| !a.trim().is_empty()) {
        url.push_str(&format!("&album_name={}", urlencoding::encode(album)));
    }
    if let Some(dur) = duration_secs {
        url.push_str(&format!("&duration={dur}"));
    }

    debug!(url = %url, "lrclib_fetch");

    let resp = client
        .get(&url)
        .header("User-Agent", format!("Tune/{}", crate::version()))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("lrclib request failed: {e}"))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }

    if !resp.status().is_success() {
        // 429/503 = « ralentis ». On le compte pour que la passe de fond
        // s'arrête au premier signal au lieu de continuer à cogner.
        if matches!(
            resp.status(),
            reqwest::StatusCode::TOO_MANY_REQUESTS | reqwest::StatusCode::SERVICE_UNAVAILABLE
        ) {
            LRCLIB_RATE_LIMIT_HITS.fetch_add(1, Ordering::Relaxed);
            warn!(status = %resp.status(), "lrclib_rate_limited");
        }
        return Err(format!("lrclib returned {}", resp.status()));
    }

    let body: LrclibResponse = resp
        .json()
        .await
        .map_err(|e| format!("lrclib parse error: {e}"))?;

    Ok(Some(LrclibRaw {
        synced_lyrics: body.synced_lyrics,
        plain_lyrics: body.plain_lyrics,
    }))
}

/// Fetch lyrics from LRCLIB for a given artist/track/duration.
///
/// `duration_secs` is the track length in seconds (integer). LRCLIB
/// uses it for disambiguation when multiple versions exist.
pub async fn fetch_from_lrclib(
    client: &reqwest::Client,
    artist: &str,
    track_name: &str,
    album_name: Option<&str>,
    duration_secs: Option<i64>,
) -> Result<Lyrics, String> {
    let raw = fetch_lrclib_raw(client, artist, track_name, album_name, duration_secs)
        .await?
        .unwrap_or_default();

    let lines = raw
        .synced_lyrics
        .as_deref()
        .map(parse_lrc)
        .unwrap_or_default();

    Ok(Lyrics {
        synced: !lines.is_empty(),
        lines,
        plain_text: raw.plain_lyrics,
        source: "lrclib".into(),
    })
}

// ---------------------------------------------------------------------------
// Cache key for metadata-only lookups (radio: title+artist, no track id)
// ---------------------------------------------------------------------------

/// Normalise un champ de métadonnée (titre ou artiste) pour la clé de cache :
/// minuscules + espaces internes réduits à un seul + trim. Deux variantes ICY
/// du même morceau (« Miles Davis » / « miles  davis ») partagent ainsi la
/// même entrée `lyrics_cache`.
pub fn normalize_meta(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Identifiant synthétique de cache pour une paire titre+artiste normalisée.
///
/// `lyrics_cache` est clé par `track_id` (PK, sans FK) ; les pistes de la
/// bibliothèque ont des ids AUTOINCREMENT strictement positifs. Les paroles
/// « radio » (pas de piste) sont donc rangées sous un id **négatif** dérivé
/// d'un FNV-1a 64 bits de `artist\u{1f}title` normalisés — aucune migration,
/// aucune collision possible avec une vraie piste.
pub fn meta_cache_id(title: &str, artist: &str) -> i64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let key = format!("{}\u{1f}{}", normalize_meta(artist), normalize_meta(title));
    let mut hash = FNV_OFFSET;
    for b in key.as_bytes() {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    // Borne sur 63 bits puis négation ; 0 est réservé (« pas d'id »).
    let positive = (hash & 0x7fff_ffff_ffff_ffff) as i64;
    if positive == 0 { -1 } else { -positive }
}

// ---------------------------------------------------------------------------
// Cache layer (`lyrics_cache` table — exists in both SQLite and Postgres)
// ---------------------------------------------------------------------------

/// One row of the `lyrics_cache` table. `synced_lyrics` holds raw LRC text,
/// `plain_lyrics` the unsynced fallback; both `None` = cached negative.
#[derive(Debug, Clone, Default)]
pub struct LyricsCacheEntry {
    pub synced_lyrics: Option<String>,
    pub plain_lyrics: Option<String>,
    pub source: String,
    /// ISO-8601 UTC (`YYYY-MM-DDTHH:MM:SSZ`).
    pub fetched_at: Option<String>,
}

impl LyricsCacheEntry {
    pub fn is_negative(&self) -> bool {
        self.synced_lyrics
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
            && self
                .plain_lyrics
                .as_deref()
                .is_none_or(|s| s.trim().is_empty())
    }

    /// True when a cached negative is still fresh (no re-fetch needed).
    pub fn negative_still_fresh(&self) -> bool {
        let Some(ref fetched) = self.fetched_at else {
            return false;
        };
        // Both engines store `YYYY-MM-DDTHH:MM:SSZ`: lexicographic order is
        // chronological order for this fixed-width format.
        fetched.as_str() >= negative_retry_cutoff().as_str()
    }

    /// Vrai quand cette entrée **dispense d'interroger LRCLIB** : soit elle
    /// porte des paroles (les positifs n'expirent pas), soit c'est un échec
    /// encore frais.
    ///
    /// Règle unique partagée par la récupération à la demande
    /// ([`get_lyrics`]) et par la passe de fond
    /// (`crate::library::lyrics_pass`) : une recherche déjà payée — y compris
    /// une recherche **infructueuse** — ne doit jamais être repayée.
    pub fn spares_a_fetch(&self) -> bool {
        !self.is_negative() || self.negative_still_fresh()
    }
}

/// Horodatage ISO-8601 UTC en deçà duquel un échec en cache est périmé et
/// mérite une nouvelle tentative. Format à largeur fixe : la comparaison
/// lexicographique vaut comparaison chronologique, sur les deux moteurs.
pub fn negative_retry_cutoff() -> String {
    (chrono::Utc::now() - chrono::Duration::days(NEGATIVE_CACHE_TTL_DAYS))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string()
}

fn dialect_sql(db: &Arc<dyn DbBackend>, f: impl Fn(&dyn SqlDialect) -> String) -> String {
    match db.engine() {
        Engine::Sqlite => f(&SqliteDialect),
        Engine::Postgres => f(&PostgresDialect),
    }
}

/// Load the cached `lyrics_cache` row for a track, if any.
pub fn load_cache_entry(db: &Arc<dyn DbBackend>, track_id: i64) -> Option<LyricsCacheEntry> {
    let sql = dialect_sql(db, |d| {
        format!(
            "SELECT synced_lyrics, plain_lyrics, source, fetched_at \
             FROM lyrics_cache WHERE track_id = {}",
            d.placeholder(1)
        )
    });
    let params: [&dyn ToSqlValue; 1] = [&track_id];

    let row = db.query_one(&sql, &params).ok()??;
    if row.len() < 4 {
        return None;
    }

    Some(LyricsCacheEntry {
        synced_lyrics: row[0].as_str().map(|s| s.to_string()),
        plain_lyrics: row[1].as_str().map(|s| s.to_string()),
        source: row[2].as_str().unwrap_or("lrclib").to_string(),
        fetched_at: row[3].as_str().map(|s| s.to_string()),
    })
}

/// Upsert a `lyrics_cache` row (works on SQLite and Postgres). Storing a
/// row with both bodies `None` records a negative result.
pub fn store_cache_entry(
    db: &Arc<dyn DbBackend>,
    track_id: i64,
    title: &str,
    artist: &str,
    synced_lyrics: Option<&str>,
    plain_lyrics: Option<&str>,
) {
    let sql = dialect_sql(db, |d| {
        format!(
            "INSERT INTO lyrics_cache \
             (track_id, title, artist, synced_lyrics, plain_lyrics, source, fetched_at) \
             VALUES ({}, {}, {}, {}, {}, {}, {}) \
             ON CONFLICT(track_id) DO UPDATE SET \
             title = excluded.title, artist = excluded.artist, \
             synced_lyrics = excluded.synced_lyrics, \
             plain_lyrics = excluded.plain_lyrics, \
             source = excluded.source, fetched_at = excluded.fetched_at",
            d.placeholder(1),
            d.placeholder(2),
            d.placeholder(3),
            d.placeholder(4),
            d.placeholder(5),
            d.placeholder(6),
            d.placeholder(7),
        )
    });

    let fetched_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let source = "lrclib";
    let params: [&dyn ToSqlValue; 7] = [
        &track_id,
        &title,
        &artist,
        &synced_lyrics as &dyn ToSqlValue,
        &plain_lyrics as &dyn ToSqlValue,
        &source,
        &fetched_at,
    ];

    if let Err(e) = db.execute(&sql, &params) {
        warn!(error = %e, track_id, "lyrics_cache_store_failed");
    }
}

// ---------------------------------------------------------------------------
// Public API: cache-first, fallback to LRCLIB
// ---------------------------------------------------------------------------

/// Get lyrics for a track. Checks the DB cache first, then falls back
/// to LRCLIB if not cached (negatives are retried after 14 days).
pub async fn get_lyrics(
    db: &Arc<dyn DbBackend>,
    client: &reqwest::Client,
    track_id: i64,
    title: &str,
    artist: &str,
    album: Option<&str>,
    duration_ms: i64,
) -> Result<Lyrics, String> {
    // 1. Try cache.
    if let Some(cached) = load_cache_entry(db, track_id) {
        if cached.spares_a_fetch() {
            debug!(track_id, "lyrics_cache_hit");
            let lines = cached
                .synced_lyrics
                .as_deref()
                .map(parse_lrc)
                .unwrap_or_default();
            return Ok(Lyrics {
                synced: !lines.is_empty(),
                lines,
                plain_text: cached.plain_lyrics,
                source: cached.source,
            });
        }
    }

    // 2. Fetch from LRCLIB.
    let duration_secs = if duration_ms > 0 {
        Some(duration_ms / 1000)
    } else {
        None
    };

    let raw = fetch_lrclib_raw(client, artist, title, album, duration_secs)
        .await?
        .unwrap_or_default();

    // 3. Cache result (even empty — avoids repeated failed lookups).
    store_cache_entry(
        db,
        track_id,
        title,
        artist,
        raw.synced_lyrics.as_deref(),
        raw.plain_lyrics.as_deref(),
    );

    let lines = raw
        .synced_lyrics
        .as_deref()
        .map(parse_lrc)
        .unwrap_or_default();

    Ok(Lyrics {
        synced: !lines.is_empty(),
        lines,
        plain_text: raw.plain_lyrics,
        source: "lrclib".into(),
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lrc_basic() {
        let lrc = "\
[00:12.34] First line
[00:15.00] Second line
[01:02.50] Third line
";
        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].time_ms, 12_340);
        assert_eq!(lines[0].text, "First line");
        assert_eq!(lines[1].time_ms, 15_000);
        assert_eq!(lines[2].time_ms, 62_500);
        assert_eq!(lines[2].text, "Third line");
    }

    #[test]
    fn parse_lrc_three_digit_frac() {
        let lrc = "[00:05.123] Precise";
        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].time_ms, 5_123);
    }

    #[test]
    fn parse_lrc_empty_and_garbage() {
        let lrc = "\n\nnot a timestamp\n[bad] nope\n";
        let lines = parse_lrc(lrc);
        assert!(lines.is_empty());
    }

    #[test]
    fn parse_lrc_single_digit_frac() {
        let lrc = "[00:03.5] Single";
        let lines = parse_lrc(lrc);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].time_ms, 3_500);
    }

    #[test]
    fn normalize_meta_case_and_whitespace() {
        assert_eq!(normalize_meta("  Miles   DAVIS "), "miles davis");
        assert_eq!(normalize_meta("So\tWhat"), "so what");
        assert_eq!(normalize_meta(""), "");
    }

    #[test]
    fn meta_cache_id_stable_and_negative() {
        let a = meta_cache_id("So What", "Miles Davis");
        // Négatif : ne peut jamais entrer en collision avec un track_id réel.
        assert!(a < 0);
        // Stable et insensible à la casse / aux espaces multiples.
        assert_eq!(a, meta_cache_id("so  what", "MILES  DAVIS"));
        // Titre et artiste ne sont pas interchangeables.
        assert_ne!(a, meta_cache_id("Miles Davis", "So What"));
        // Une autre paire donne une autre clé.
        assert_ne!(a, meta_cache_id("Blue in Green", "Miles Davis"));
    }

    #[test]
    fn negative_cache_freshness() {
        let fresh = LyricsCacheEntry {
            fetched_at: Some(chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()),
            ..Default::default()
        };
        assert!(fresh.is_negative());
        assert!(fresh.negative_still_fresh());

        let stale = LyricsCacheEntry {
            fetched_at: Some("2020-01-01T00:00:00Z".into()),
            ..Default::default()
        };
        assert!(stale.is_negative());
        assert!(!stale.negative_still_fresh());

        let missing = LyricsCacheEntry::default();
        assert!(!missing.negative_still_fresh());
    }
}
