use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tracing::{info, warn};

use crate::db::backend::DbBackend;
use crate::scanner::hasher::{
    compute_audio_hash_str, files_are_byte_identical, is_current_audio_hash,
};

#[derive(Debug, Clone)]
pub struct DuplicateEntry {
    pub id: i64,
    pub title: String,
    pub artist_name: Option<String>,
    pub file_path: String,
}

#[derive(Debug, Clone)]
pub struct DuplicateGroup {
    pub hash: String,
    pub tracks: Vec<DuplicateEntry>,
}

#[derive(Debug, Clone)]
pub struct DuplicateScanResult {
    pub total_scanned: usize,
    pub duplicates_found: usize,
    pub groups: Vec<DuplicateGroup>,
    pub errors: usize,
}

fn exact_duplicate_groups(hash: String, tracks: Vec<DuplicateEntry>) -> Vec<DuplicateGroup> {
    let mut exact_partitions: Vec<Vec<DuplicateEntry>> = Vec::new();

    for track in tracks {
        let mut pending = Some(track);
        for partition in &mut exact_partitions {
            let Some(candidate) = pending.as_ref() else {
                break;
            };
            let representative = &partition[0];
            if files_are_byte_identical(
                Path::new(&candidate.file_path),
                Path::new(&representative.file_path),
            )
            .unwrap_or(false)
            {
                partition.push(pending.take().expect("track still pending"));
            }
        }
        if let Some(track) = pending {
            exact_partitions.push(vec![track]);
        }
    }

    exact_partitions
        .into_iter()
        .filter(|partition| partition.len() > 1)
        .map(|tracks| DuplicateGroup {
            hash: hash.clone(),
            tracks,
        })
        .collect()
}

pub fn scan_duplicates(db: &Arc<dyn DbBackend>, limit: usize) -> DuplicateScanResult {
    let query = if limit > 0 {
        format!(
            "SELECT id, file_path, title, audio_hash FROM tracks \
             WHERE source = 'local' AND file_path IS NOT NULL \
             LIMIT {limit}"
        )
    } else {
        "SELECT id, file_path, title, audio_hash FROM tracks \
         WHERE source = 'local' AND file_path IS NOT NULL"
            .to_string()
    };

    let raw_rows = match db.query_many(&query, &[]) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "duplicate_scan_query_error");
            return DuplicateScanResult {
                total_scanned: 0,
                duplicates_found: 0,
                groups: Vec::new(),
                errors: 1,
            };
        }
    };

    let rows: Vec<(i64, String, String, Option<String>)> = raw_rows
        .iter()
        .map(|r| {
            (
                r[0].as_i64().unwrap_or(0),
                r[1].as_string().unwrap_or_default(),
                r[2].as_string().unwrap_or_default(),
                r[3].as_string(),
            )
        })
        .collect();

    let mut hash_map: HashMap<String, Vec<DuplicateEntry>> = HashMap::new();
    let mut scanned = 0;
    let mut errors = 0;

    for (id, file_path, title, existing_hash) in &rows {
        let current_hash = existing_hash
            .as_deref()
            .filter(|hash| is_current_audio_hash(hash))
            .map(str::to_owned);
        let h = if current_hash.is_some() {
            current_hash
        } else {
            let computed = compute_audio_hash_str(file_path);
            if let Some(ref h) = computed {
                if let Err(error) = db.execute(
                    "UPDATE tracks SET audio_hash = ? WHERE id = ?",
                    &[&h.as_str(), id],
                ) {
                    warn!(%error, track_id = id, path = file_path, "audio_hash_update_failed");
                }
            }
            computed
        };

        if let Some(h) = h {
            hash_map.entry(h).or_default().push(DuplicateEntry {
                id: *id,
                title: title.clone(),
                artist_name: None,
                file_path: file_path.clone(),
            });
            scanned += 1;
        } else {
            errors += 1;
        }
    }

    // The sampled hash only selects inexpensive candidates. It can never by
    // itself make two tracks duplicates: every reported group is partitioned
    // by a complete byte-for-byte comparison.
    let groups: Vec<DuplicateGroup> = hash_map
        .into_iter()
        .flat_map(|(hash, tracks)| exact_duplicate_groups(hash, tracks))
        .collect();

    let duplicates_found: usize = groups.iter().map(|g| g.tracks.len() - 1).sum();

    info!(
        scanned,
        groups = groups.len(),
        duplicates = duplicates_found,
        errors,
        "duplicate_scan_complete"
    );

    DuplicateScanResult {
        total_scanned: scanned,
        duplicates_found,
        groups,
        errors,
    }
}

pub fn scan_fingerprint_duplicates(db: &Arc<dyn DbBackend>) -> Vec<DuplicateGroup> {
    let raw_rows = match db.query_many(
        "SELECT t.id, t.title, ar.name, t.file_path, t.acoustid_fingerprint
         FROM tracks t
         LEFT JOIN artists ar ON t.artist_id = ar.id
         WHERE t.acoustid_fingerprint IS NOT NULL AND t.acoustid_fingerprint != ''
         ORDER BY t.acoustid_fingerprint, t.id",
        &[],
    ) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "fingerprint_duplicate_query_error");
            return Vec::new();
        }
    };

    let mut fp_map: HashMap<String, Vec<DuplicateEntry>> = HashMap::new();
    for r in &raw_rows {
        let id = r[0].as_i64().unwrap_or(0);
        let title = r[1].as_string().unwrap_or_default();
        let artist = r[2].as_string();
        let path = r[3].as_string().unwrap_or_default();
        let fp = r[4].as_string().unwrap_or_default();
        fp_map.entry(fp).or_default().push(DuplicateEntry {
            id,
            title,
            artist_name: artist,
            file_path: path,
        });
    }

    fp_map
        .into_iter()
        .filter(|(_, g)| g.len() > 1)
        .map(|(fp, tracks)| DuplicateGroup { hash: fp, tracks })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        let mut data = vec![0u8; 16 * 1024];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i % 256) as u8;
        }
        std::fs::write(&path, &data).unwrap();

        let h1 = compute_audio_hash_str(path.to_str().unwrap());
        let h2 = compute_audio_hash_str(path.to_str().unwrap());
        assert!(h1.is_some());
        assert_eq!(h1, h2);
    }

    #[test]
    fn hash_nonexistent_file() {
        let result = compute_audio_hash_str("/nonexistent/file.flac");
        assert!(result.is_none());
    }

    #[test]
    fn hash_tiny_nonempty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.bin");
        std::fs::write(&path, &[0u8; 100]).unwrap();
        let result = compute_audio_hash_str(path.to_str().unwrap());
        assert!(result.is_some());
    }

    #[test]
    fn different_files_different_hashes() {
        let dir = tempfile::tempdir().unwrap();

        let mut data_a = vec![0u8; 16 * 1024];
        for (i, b) in data_a.iter_mut().enumerate() {
            *b = (i % 256) as u8;
        }
        let path_a = dir.path().join("a.bin");
        std::fs::write(&path_a, &data_a).unwrap();

        let mut data_b = vec![0xFFu8; 16 * 1024];
        for (i, b) in data_b.iter_mut().enumerate() {
            *b = ((i + 1) % 256) as u8;
        }
        let path_b = dir.path().join("b.bin");
        std::fs::write(&path_b, &data_b).unwrap();

        let ha = compute_audio_hash_str(path_a.to_str().unwrap()).unwrap();
        let hb = compute_audio_hash_str(path_b.to_str().unwrap()).unwrap();
        assert_ne!(ha, hb);
    }

    #[test]
    fn hash_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        let data = vec![42u8; 16 * 1024];
        std::fs::write(&path, &data).unwrap();

        let h = compute_audio_hash_str(path.to_str().unwrap()).unwrap();
        assert!(is_current_audio_hash(&h));
    }
}
