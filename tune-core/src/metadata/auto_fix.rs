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

        let defective = find_defective_tracks(&repo);
        let total = defective.len();

        {
            let mut p = self.progress.lock().await;
            p.status = "scanning".into();
            p.total = total;
            p.current = 0;
            p.fixed = 0;
            p.suggestions = 0;
        }

        info!(total, "auto_fix_scan_start");

        for (i, track_id) in defective.iter().enumerate() {
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

fn find_defective_tracks(repo: &TrackRepo) -> Vec<i64> {
    let db = repo.backend();
    db.query_many(
        // Un champ d'enrichissement absent n'est pas un defaut : la plupart
        // des fichiers locaux n'ont legitimement ni ISRC, ni genre, ni annee.
        // Ne lancer les appels MusicBrainz couteux que lorsqu'un indice de
        // metadata reellement incoherente existe deja dans la bibliotheque.
        //
        // La numerotation manquante n'est suspecte que si le meme album est
        // par ailleurs numerote. Cela protege notamment les singles et albums
        // autoproduits entierement non numerotes. Deux numeros positifs
        // identiques sur le meme disque sont en revanche contradictoires.
        "SELECT DISTINCT t.id FROM tracks t \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         LEFT JOIN albums al ON t.album_id = al.id \
         WHERE (t.source IS NULL OR t.source = '' OR t.source = 'local') AND (\
             TRIM(t.title) = '' \
             OR LOWER(TRIM(COALESCE(ar.name, ''))) IN ('', 'unknown artist') \
             OR COALESCE(t.disc_number, 0) <= 0 \
             OR (COALESCE(t.track_number, 0) <= 0 AND EXISTS (\
                 SELECT 1 FROM tracks numbered \
                 WHERE numbered.album_id = t.album_id \
                   AND COALESCE(numbered.disc_number, 1) = COALESCE(t.disc_number, 1) \
                   AND numbered.track_number > 0\
             )) \
             OR (t.track_number > 0 AND EXISTS (\
                 SELECT 1 FROM tracks duplicate_number \
                 WHERE duplicate_number.album_id = t.album_id \
                   AND COALESCE(duplicate_number.disc_number, 1) = COALESCE(t.disc_number, 1) \
                   AND duplicate_number.track_number = t.track_number \
                   AND duplicate_number.id <> t.id\
             )) \
             OR (al.folder_path IS NOT NULL AND TRIM(al.folder_path) <> '' \
                 AND t.file_path IS NOT NULL AND TRIM(t.file_path) <> '' \
                 AND SUBSTR(t.file_path, 1, LENGTH(RTRIM(al.folder_path, '/\\')) + 1) \
                     NOT IN (RTRIM(al.folder_path, '/\\') || '/', RTRIM(al.folder_path, '/\\') || '\\')\
             ) \
             OR (al.folder_path IS NOT NULL AND TRIM(al.folder_path) <> '' AND EXISTS (\
                 SELECT 1 FROM albums sibling_album \
                 WHERE sibling_album.folder_path = al.folder_path \
                   AND sibling_album.id <> al.id \
                   AND LOWER(TRIM(sibling_album.title)) <> LOWER(TRIM(al.title))\
             ))\
         ) ORDER BY t.id LIMIT 5000",
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

    fn seed_artist_album(
        db: &SqliteDb,
        artist_id: i64,
        artist: &str,
        album_id: i64,
        album: &str,
        folder: &str,
    ) {
        db.execute(
            "INSERT INTO artists (id, name) VALUES (?, ?)",
            &[&artist_id, &artist],
        )
        .unwrap();
        db.execute(
            "INSERT INTO albums (id, title, artist_id, folder_path) VALUES (?, ?, ?, ?)",
            &[&album_id, &album, &artist_id, &folder],
        )
        .unwrap();
    }

    fn seed_track(
        db: &SqliteDb,
        id: i64,
        title: &str,
        album_id: i64,
        artist_id: i64,
        disc: i64,
        number: i64,
        path: &str,
    ) {
        db.execute(
            "INSERT INTO tracks \
             (id, title, album_id, artist_id, disc_number, track_number, file_path, source) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 'local')",
            &[&id, &title, &album_id, &artist_id, &disc, &number, &path],
        )
        .unwrap();
    }

    #[test]
    fn healthy_self_released_tracks_without_isrc_are_not_selected() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        seed_artist_album(
            &db,
            1,
            "The Bedroom Tapes",
            1,
            "Home Recordings",
            "/music/home-recordings",
        );
        seed_track(
            &db,
            1,
            "First Light",
            1,
            1,
            1,
            1,
            "/music/home-recordings/01-first-light.flac",
        );
        // Cas frequent et sain : ni ISRC, ni genre, ni annee.
        let track = TrackRepo::new(db.clone()).get(1).unwrap().unwrap();
        assert_eq!(track.isrc, None);
        assert_eq!(track.genre, None);
        assert_eq!(track.year, None);

        assert!(find_defective_tracks(&TrackRepo::new(db)).is_empty());
    }

    #[test]
    fn entirely_unnumbered_album_is_not_assumed_broken() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        seed_artist_album(&db, 1, "DIY Artist", 1, "Loose Sessions", "/music/loose");
        seed_track(&db, 1, "Day One", 1, 1, 1, 0, "/music/loose/day-one.flac");
        seed_track(&db, 2, "Day Two", 1, 1, 1, 0, "/music/loose/day-two.flac");

        assert!(find_defective_tracks(&TrackRepo::new(db)).is_empty());
    }

    #[test]
    fn real_metadata_defects_are_selected() {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        seed_artist_album(&db, 1, "Known Artist", 1, "Numbered", "/music/numbered");
        seed_artist_album(&db, 2, "Unknown Artist", 2, "Unknown", "/music/unknown");
        seed_artist_album(&db, 3, "Known Artist 2", 3, "Tagged A", "/music/mixed-tags");
        db.execute(
            "INSERT INTO albums (id, title, artist_id, folder_path) \
             VALUES (4, 'Tagged B', 3, '/music/mixed-tags')",
            &[],
        )
        .unwrap();

        seed_track(&db, 1, "", 1, 1, 1, 1, "/music/numbered/01.flac");
        seed_track(&db, 2, "Mystery", 2, 2, 1, 1, "/music/unknown/01.flac");
        // Un trou au milieu d'un album numerote est un signal, contrairement
        // a un album entierement sans numeros.
        seed_track(&db, 3, "Numbered", 1, 1, 1, 2, "/music/numbered/02.flac");
        seed_track(
            &db,
            4,
            "Missing number",
            1,
            1,
            1,
            0,
            "/music/numbered/x.flac",
        );
        // Deux numeros identiques sur le meme disque sont contradictoires.
        seed_track(
            &db,
            5,
            "Duplicate A",
            1,
            1,
            1,
            3,
            "/music/numbered/03-a.flac",
        );
        seed_track(
            &db,
            6,
            "Duplicate B",
            1,
            1,
            1,
            3,
            "/music/numbered/03-b.flac",
        );
        // Le chemin de la piste contredit le dossier rattache a l'album.
        seed_track(
            &db,
            7,
            "Wrong folder",
            1,
            1,
            1,
            4,
            "/music/elsewhere/04.flac",
        );
        // Deux titres d'album distincts revendiquent le meme dossier.
        seed_track(&db, 8, "Mixed A", 3, 3, 1, 1, "/music/mixed-tags/a.flac");
        seed_track(&db, 9, "Mixed B", 4, 3, 1, 2, "/music/mixed-tags/b.flac");

        assert_eq!(
            find_defective_tracks(&TrackRepo::new(db)),
            vec![1, 2, 4, 5, 6, 7, 8, 9]
        );
    }
}
