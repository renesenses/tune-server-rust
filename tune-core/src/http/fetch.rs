//! Shared HTTP fetch utilities for the enrichment paths: a per-key
//! [`RateLimiter`] and a typed [`FetchOutcome`].
//!
//! Enrichment previously spaced its external calls with scattered
//! `tokio::time::sleep(Duration::from_millis(1100))` before each MusicBrainz /
//! Cover Art Archive request. That is fragile (magic numbers duplicated at
//! every call site) and, worse, only spaces requests *within one loop* — two
//! concurrent enrichment tasks (album covers + artist images) could still hit
//! MusicBrainz twice in the same second and earn a 503 block. A shared
//! limiter serialises across all callers.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

/// A minimum-interval limiter keyed by an arbitrary string (typically a host or
/// service name). Each `acquire(key)` reserves the next free time slot for that
/// key and sleeps until it, so N concurrent callers are serialised to one
/// request per `min_interval` — with no busy-waiting and no per-call-site magic
/// numbers.
pub struct RateLimiter {
    min_interval: Duration,
    /// Next instant a request for a given key is allowed to proceed.
    next_slot: Mutex<HashMap<String, Instant>>,
}

impl RateLimiter {
    pub fn with_interval(min_interval: Duration) -> Self {
        Self {
            min_interval,
            next_slot: Mutex::new(HashMap::new()),
        }
    }

    /// Build a limiter allowing at most `rps` requests per second.
    pub fn per_second(rps: f64) -> Self {
        let secs = if rps > 0.0 { 1.0 / rps } else { 0.0 };
        Self::with_interval(Duration::from_secs_f64(secs))
    }

    /// Block until a request for `key` may proceed, reserving that slot so
    /// concurrent callers queue behind it one `min_interval` apart.
    pub async fn acquire(&self, key: &str) {
        // Reserve a slot while holding the lock, then release the lock BEFORE
        // sleeping so other keys aren't blocked and same-key callers each get a
        // distinct, monotonically spaced slot.
        let slot = {
            let mut slots = self.next_slot.lock().await;
            let now = Instant::now();
            let slot = match slots.get(key) {
                Some(&next) if next > now => next,
                _ => now,
            };
            slots.insert(key.to_string(), slot + self.min_interval);
            slot
        };
        let now = Instant::now();
        if slot > now {
            tokio::time::sleep(slot - now).await;
        }
    }
}

/// Shared limiter for MusicBrainz-operated endpoints (musicbrainz.org and the
/// Cover Art Archive), whose published policy is ~1 request/second. Every MB /
/// CAA call in [`crate::library::artwork`] acquires this before requesting.
pub static MUSICBRAINZ: LazyLock<RateLimiter> = LazyLock::new(|| RateLimiter::per_second(1.0));

/// Typed result of fetching a binary resource (e.g. an image), so callers can
/// tell a genuine "not found" from a transient rate-limit or network error.
#[derive(Debug)]
pub enum FetchOutcome {
    /// Body received and at least `min_len` bytes.
    Success(Vec<u8>),
    /// HTTP 404 — the resource does not exist.
    NotFound,
    /// HTTP 429/503 — throttled; the caller should back off, not treat as final.
    RateLimited,
    /// 2xx but the body was smaller than `min_len` (usually an error page).
    TooSmall(usize),
    /// Transport error or any other non-success status.
    Error(String),
}

impl FetchOutcome {
    /// Consume the outcome, yielding the bytes only on success.
    pub fn into_bytes(self) -> Option<Vec<u8>> {
        match self {
            FetchOutcome::Success(b) => Some(b),
            _ => None,
        }
    }

    /// A short static reason for logging (never includes the body).
    pub fn reason(&self) -> &'static str {
        match self {
            FetchOutcome::Success(_) => "success",
            FetchOutcome::NotFound => "not_found",
            FetchOutcome::RateLimited => "rate_limited",
            FetchOutcome::TooSmall(_) => "too_small",
            FetchOutcome::Error(_) => "error",
        }
    }
}

/// Fetch a binary resource, classifying the result. `min_len` rejects tiny
/// bodies (error pages served with a 200) as [`FetchOutcome::TooSmall`].
pub async fn fetch_bytes(client: &reqwest::Client, url: &str, min_len: usize) -> FetchOutcome {
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => return FetchOutcome::Error(e.to_string()),
    };
    let status = resp.status();
    if status.as_u16() == 429 || status.as_u16() == 503 {
        return FetchOutcome::RateLimited;
    }
    if status.as_u16() == 404 {
        return FetchOutcome::NotFound;
    }
    if !status.is_success() {
        return FetchOutcome::Error(format!("http {}", status.as_u16()));
    }
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return FetchOutcome::Error(e.to_string()),
    };
    if bytes.len() < min_len {
        return FetchOutcome::TooSmall(bytes.len());
    }
    FetchOutcome::Success(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_outcome_into_bytes_only_on_success() {
        assert_eq!(
            FetchOutcome::Success(vec![1, 2, 3]).into_bytes(),
            Some(vec![1, 2, 3])
        );
        assert_eq!(FetchOutcome::NotFound.into_bytes(), None);
        assert_eq!(FetchOutcome::RateLimited.into_bytes(), None);
        assert_eq!(FetchOutcome::TooSmall(4).into_bytes(), None);
        assert_eq!(FetchOutcome::Error("x".into()).into_bytes(), None);
    }

    #[test]
    fn fetch_outcome_reason_is_stable() {
        assert_eq!(FetchOutcome::RateLimited.reason(), "rate_limited");
        assert_eq!(FetchOutcome::NotFound.reason(), "not_found");
    }

    #[tokio::test]
    async fn rate_limiter_spaces_same_key_by_interval() {
        // Three sequential acquires on the same key must span at least
        // 2 * interval (the first proceeds immediately). Short interval keeps
        // the test fast while asserting a reliable lower bound.
        let rl = RateLimiter::with_interval(Duration::from_millis(50));
        let start = Instant::now();
        rl.acquire("mb").await;
        rl.acquire("mb").await;
        rl.acquire("mb").await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(100),
            "3 acquires spaced by 50ms must take >= 100ms, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn rate_limiter_independent_keys_do_not_block_each_other() {
        // Distinct keys each get their own immediate first slot, so even a huge
        // interval must not make them wait on one another.
        let rl = RateLimiter::with_interval(Duration::from_secs(10));
        let start = Instant::now();
        rl.acquire("a").await;
        rl.acquire("b").await;
        rl.acquire("c").await;
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "distinct keys must not wait on each other"
        );
    }
}
