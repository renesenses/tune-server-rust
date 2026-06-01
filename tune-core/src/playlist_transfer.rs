use serde::Serialize;
use tokio::sync::Mutex;
use tracing::info;

use crate::track_matcher::{MatchCandidate, MatchResult, find_best_match};

#[derive(Debug, Clone, Serialize)]
pub struct TransferProgress {
    pub status: String,
    pub current: usize,
    pub total: usize,
    pub matched: usize,
    pub approximate: usize,
    pub not_found: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct TransferResult {
    pub source_service: String,
    pub target_service: String,
    pub playlist_name: String,
    pub total: usize,
    pub matched: usize,
    pub approximate: usize,
    pub not_found: usize,
    pub results: Vec<MatchResult>,
    pub target_playlist_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TransferTrack {
    pub title: String,
    pub artist_name: String,
    pub album_title: String,
    pub duration_ms: i64,
    pub source_id: String,
    pub isrc: String,
}

pub struct PlaylistTransfer {
    progress: Mutex<TransferProgress>,
}

impl Default for PlaylistTransfer {
    fn default() -> Self {
        Self::new()
    }
}

impl PlaylistTransfer {
    pub fn new() -> Self {
        Self {
            progress: Mutex::new(TransferProgress {
                status: "idle".into(),
                current: 0,
                total: 0,
                matched: 0,
                approximate: 0,
                not_found: 0,
            }),
        }
    }

    pub async fn progress(&self) -> TransferProgress {
        self.progress.lock().await.clone()
    }

    pub async fn execute<F, Fut>(
        &self,
        source_tracks: &[TransferTrack],
        source_service: &str,
        target_service: &str,
        playlist_name: &str,
        search_fn: F,
        _match_threshold: f64,
    ) -> TransferResult
    where
        F: Fn(String) -> Fut,
        Fut: std::future::Future<Output = Vec<MatchCandidate>>,
    {
        let total = source_tracks.len();

        {
            let mut p = self.progress.lock().await;
            p.status = "transferring".into();
            p.total = total;
            p.current = 0;
            p.matched = 0;
            p.approximate = 0;
            p.not_found = 0;
        }

        info!(
            source = source_service,
            target = target_service,
            playlist = playlist_name,
            tracks = total,
            "transfer_start"
        );

        let mut results = Vec::with_capacity(total);
        let mut matched = 0usize;
        let mut approximate = 0usize;
        let mut not_found = 0usize;

        for (i, track) in source_tracks.iter().enumerate() {
            let query = format!("{} {}", track.title, track.artist_name);
            let candidates = search_fn(query).await;

            let result = find_best_match(
                &track.title,
                &track.artist_name,
                &track.isrc,
                track.duration_ms,
                &candidates,
            );

            match result.status.as_str() {
                "matched" => matched += 1,
                "approximate" => approximate += 1,
                _ => not_found += 1,
            }

            results.push(result);

            {
                let mut p = self.progress.lock().await;
                p.current = i + 1;
                p.matched = matched;
                p.approximate = approximate;
                p.not_found = not_found;
            }
        }

        {
            let mut p = self.progress.lock().await;
            p.status = "complete".into();
        }

        info!(matched, approximate, not_found, "transfer_complete");

        TransferResult {
            source_service: source_service.into(),
            target_service: target_service.into(),
            playlist_name: playlist_name.into(),
            total,
            matched,
            approximate,
            not_found,
            results,
            target_playlist_id: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transfer_empty() {
        let transfer = PlaylistTransfer::new();
        let result = transfer
            .execute(&[], "spotify", "tidal", "Test", |_| async { vec![] }, 0.6)
            .await;

        assert_eq!(result.total, 0);
        assert_eq!(result.matched, 0);
    }

    #[tokio::test]
    async fn transfer_with_match() {
        let transfer = PlaylistTransfer::new();

        let tracks = vec![TransferTrack {
            title: "Imagine".into(),
            artist_name: "John Lennon".into(),
            album_title: "Imagine".into(),
            duration_ms: 187000,
            source_id: "sp123".into(),
            isrc: String::new(),
        }];

        let result = transfer
            .execute(
                &tracks,
                "spotify",
                "tidal",
                "My Playlist",
                |_query| async {
                    vec![MatchCandidate {
                        title: "Imagine".into(),
                        artist_name: "John Lennon".into(),
                        album_title: "Imagine".into(),
                        source_id: "tid456".into(),
                        duration_ms: 187000,
                        isrc: String::new(),
                        score: 0.0,
                        match_method: String::new(),
                        confidence: String::new(),
                    }]
                },
                0.6,
            )
            .await;

        assert_eq!(result.total, 1);
        assert_eq!(result.matched, 1);
        assert_eq!(result.not_found, 0);
    }

    #[tokio::test]
    async fn transfer_no_candidates() {
        let transfer = PlaylistTransfer::new();

        let tracks = vec![TransferTrack {
            title: "Unknown Song".into(),
            artist_name: "Nobody".into(),
            album_title: String::new(),
            duration_ms: 0,
            source_id: "x".into(),
            isrc: String::new(),
        }];

        let result = transfer
            .execute(&tracks, "a", "b", "Test", |_| async { vec![] }, 0.6)
            .await;

        assert_eq!(result.not_found, 1);
    }

    #[tokio::test]
    async fn progress_tracking() {
        let transfer = PlaylistTransfer::new();
        let p = transfer.progress().await;
        assert_eq!(p.status, "idle");
        assert_eq!(p.total, 0);
    }
}
