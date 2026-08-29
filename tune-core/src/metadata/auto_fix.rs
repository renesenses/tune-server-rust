use std::sync::Arc;

use serde::Serialize;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::db::sqlite::SqliteDb;
use crate::db::track_repo::TrackRepo;
use crate::metadata::enrichment::MetadataEnricher;

#[derive(Debug, Clone, Serialize)]
pub struct AutoFixProgress {
    pub status: String,
    pub current: usize,
    pub total: usize,
    pub fixed: usize,
    pub suggestions: usize,
}

impl Default for AutoFixProgress {
    fn default() -> Self {
        Self {
            status: "idle".into(),
            current: 0,
            total: 0,
            fixed: 0,
            suggestions: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FixSuggestion {
    pub track_id: i64,
    pub field: String,
    pub current_value: String,
    pub suggested_value: String,
    pub confidence: f64,
    pub source: String,
}

pub struct AutoFixEngine {
    db: SqliteDb,
    progress: Mutex<AutoFixProgress>,
    running: Mutex<bool>,
    cancel: Mutex<bool>,
    suggestions: Mutex<Vec<FixSuggestion>>,
}

impl AutoFixEngine {
    pub fn new(db: SqliteDb) -> Self {
        Self {
            db,
            progress: Mutex::new(AutoFixProgress::default()),
            running: Mutex::new(false),
            cancel: Mutex::new(false),
            suggestions: Mutex::new(Vec::new()),
        }
    }

    pub async fn status(&self) -> AutoFixProgress {
        self.progress.lock().await.clone()
    }

    pub async fn is_running(&self) -> bool {
        *self.running.lock().await
    }

    pub async fn get_suggestions(&self) -> Vec<FixSuggestion> {
        self.suggestions.lock().await.clone()
    }

    pub async fn start_scan(
        self: Arc<Self>,
        auto_apply_threshold: f64,
        batch_size: usize,
    ) -> Result<(), String> {
        if *self.running.lock().await {
            return Err("scan already running".into());
        }

        *self.running.lock().await = true;
        *self.cancel.lock().await = false;
        *self.suggestions.lock().await = Vec::new();

        let engine = self.clone();
        tokio::spawn(async move {
            engine.scan_loop(auto_apply_threshold, batch_size).await;
            *engine.running.lock().await = false;
        });

        Ok(())
    }

    pub async fn stop(&self) {
        *self.cancel.lock().await = true;
    }

    async fn scan_loop(&self, auto_apply_threshold: f64, batch_size: usize) {
        let repo = TrackRepo::new(self.db.clone());
        let enricher = MetadataEnricher::new(self.db.clone());

        let incomplete = find_incomplete_tracks(&repo);
        let total = incomplete.len();

        {
            let mut p = self.progress.lock().await;
            p.status = "scanning".into();
            p.total = total;
            p.current = 0;
            p.fixed = 0;
            p.suggestions = 0;
        }

        info!(total, "auto_fix_scan_start");

        for (i, track_id) in incomplete.iter().enumerate() {
            if *self.cancel.lock().await {
                info!("auto_fix_cancelled");
                break;
            }

            self.progress.lock().await.current = i + 1;

            match enricher.enrich_track(*track_id).await {
                Ok(Some(result)) => {
                    let track = match repo.get(*track_id) {
                        Ok(Some(t)) => t,
                        _ => continue,
                    };

                    if let Some(ref genre) = result.genre
                        && (track.genre.is_none() || track.genre.as_deref() == Some(""))
                    {
                        self.handle_candidate(
                            *track_id,
                            "genre",
                            "",
                            genre,
                            0.85,
                            auto_apply_threshold,
                            "musicbrainz",
                        )
                        .await;
                    }

                    if let Some(year) = result.year
                        && (track.year.is_none() || track.year == Some(0))
                    {
                        self.handle_candidate(
                            *track_id,
                            "year",
                            "",
                            &year.to_string(),
                            0.9,
                            auto_apply_threshold,
                            "musicbrainz",
                        )
                        .await;
                    }

                    if let Some(ref isrc) = result.isrc
                        && track.isrc.is_none()
                    {
                        self.handle_candidate(
                            *track_id,
                            "isrc",
                            "",
                            isrc,
                            0.95,
                            auto_apply_threshold,
                            "musicbrainz",
                        )
                        .await;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    debug!(track_id, error = %e, "auto_fix_enrich_failed");
                }
            }

            // Rate limit: 1 req/sec for MusicBrainz
            if (i + 1) % batch_size == 0 {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }

        let mut p = self.progress.lock().await;
        p.status = "complete".into();
        info!(
            fixed = p.fixed,
            suggestions = p.suggestions,
            "auto_fix_scan_complete"
        );
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_candidate(
        &self,
        track_id: i64,
        field: &str,
        current: &str,
        suggested: &str,
        confidence: f64,
        auto_apply_threshold: f64,
        source: &str,
    ) {
        if confidence >= auto_apply_threshold {
            match self.apply_suggestion(track_id, field, suggested).await {
                Ok(()) => return,
                Err(error) => {
                    // Ne jamais perdre une proposition parce que son application
                    // automatique a echoue : elle reste disponible pour validation
                    // manuelle, avec la cause dans le journal.
                    warn!(
                        track_id,
                        field,
                        confidence,
                        auto_apply_threshold,
                        %error,
                        "auto_fix_apply_failed_kept_as_suggestion"
                    );
                }
            }
        }

        self.add_suggestion(track_id, field, current, suggested, confidence, source)
            .await;
    }

    async fn add_suggestion(
        &self,
        track_id: i64,
        field: &str,
        current: &str,
        suggested: &str,
        confidence: f64,
        source: &str,
    ) {
        let suggestion = FixSuggestion {
            track_id,
            field: field.into(),
            current_value: current.into(),
            suggested_value: suggested.into(),
            confidence,
            source: source.into(),
        };

        self.suggestions.lock().await.push(suggestion);
        self.progress.lock().await.suggestions += 1;
    }

    pub async fn apply_suggestion(
        &self,
        track_id: i64,
        field: &str,
        value: &str,
    ) -> Result<(), String> {
        let repo = TrackRepo::new(self.db.clone());
        let mut track = repo
            .get(track_id)
            .map_err(|e| e.to_string())?
            .ok_or("track not found")?;

        match field {
            "genre" => track.genre = Some(value.into()),
            "year" => track.year = value.parse().ok(),
            "isrc" => track.isrc = Some(value.into()),
            "composer" => track.composer = Some(value.into()),
            "label" => track.label = Some(value.into()),
            _ => return Err(format!("unknown field: {field}")),
        }

        repo.update(&track)?;
        self.progress.lock().await.fixed += 1;
        info!(track_id, field, value, "auto_fix_applied");
        Ok(())
    }
}

fn find_incomplete_tracks(repo: &TrackRepo) -> Vec<i64> {
    let db = repo.backend();
    db.query_many(
        "SELECT id FROM tracks WHERE \
         (genre IS NULL OR genre = '') OR \
         (year IS NULL OR year = 0) OR \
         isrc IS NULL \
         ORDER BY id LIMIT 5000",
        &[],
    )
    .unwrap_or_default()
    .into_iter()
    .filter_map(|cols| cols.first().and_then(|v| v.as_i64()))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_progress() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let engine = AutoFixEngine::new(db);

        let p = engine.status().await;
        assert_eq!(p.status, "idle");
        assert_eq!(p.total, 0);
        assert!(!engine.is_running().await);
    }

    #[tokio::test]
    async fn suggestions_empty() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let engine = AutoFixEngine::new(db);
        assert!(engine.get_suggestions().await.is_empty());
    }

    #[test]
    fn fix_suggestion_serialize() {
        let s = FixSuggestion {
            track_id: 42,
            field: "genre".into(),
            current_value: "".into(),
            suggested_value: "Rock".into(),
            confidence: 0.9,
            source: "musicbrainz".into(),
        };
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["track_id"], 42);
        assert_eq!(json["suggested_value"], "Rock");
    }

    #[tokio::test]
    async fn threshold_applies_at_or_above_and_keeps_lower_confidence_as_suggestion() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        db.execute(
            "INSERT INTO artists (id, name) VALUES (1, 'Miles Davis')",
            &[],
        )
        .unwrap();
        db.execute(
            "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Kind of Blue', 1)",
            &[],
        )
        .unwrap();
        db.execute(
            "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms) \
             VALUES (1, 'So What', 1, 1, 545000)",
            &[],
        )
        .unwrap();

        let engine = AutoFixEngine::new(db);

        // 0,85 reste sous le seuil : la base ne doit pas etre modifiee.
        engine
            .handle_candidate(1, "genre", "", "Jazz", 0.85, 0.9, "musicbrainz")
            .await;
        let track = TrackRepo::new(engine.db.clone()).get(1).unwrap().unwrap();
        assert_eq!(track.genre, None);
        assert_eq!(engine.get_suggestions().await.len(), 1);

        // La borne est inclusive : une confiance exactement egale au seuil
        // applique la correction et n'ajoute pas une seconde suggestion.
        engine
            .handle_candidate(1, "year", "", "1959", 0.9, 0.9, "musicbrainz")
            .await;
        let track = TrackRepo::new(engine.db.clone()).get(1).unwrap().unwrap();
        assert_eq!(track.year, Some(1959));
        assert_eq!(engine.get_suggestions().await.len(), 1);

        let progress = engine.status().await;
        assert_eq!(progress.fixed, 1);
        assert_eq!(progress.suggestions, 1);
    }
}
