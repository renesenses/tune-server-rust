use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tune_core::scanner::hasher::compute_audio_hash_str;
use tune_core::scanner::quality;
use tune_core::scanner::walker;
use tune_core::scanner::watcher::{ChangeType, FileWatcher};

// ─── Scan functions ─────────────────────────────────────────────

#[pyfunction]
fn list_audio_files(py: Python<'_>, dirs: Vec<String>) -> PyResult<pyo3::Bound<'_, PyList>> {
    let files = py.allow_threads(|| walker::list_audio_files(&dirs));
    let list = PyList::empty(py);
    for f in files {
        list.append(f.to_string_lossy().to_string())?;
    }
    Ok(list)
}

#[pyfunction]
#[pyo3(signature = (dirs, with_hash=false))]
fn scan_directories<'py>(
    py: Python<'py>,
    dirs: Vec<String>,
    with_hash: bool,
) -> PyResult<pyo3::Bound<'py, PyDict>> {
    let (results, stats) = py.allow_threads(|| {
        walker::scan_directories(&dirs, with_hash, None)
    });

    let dict = PyDict::new(py);
    let files_list = PyList::empty(py);

    for f in &results {
        let file_dict = PyDict::new(py);
        file_dict.set_item("path", &f.path)?;
        file_dict.set_item("file_size", f.file_size)?;
        file_dict.set_item("mtime", f.mtime)?;

        if let Some(ref meta) = f.metadata {
            let meta_dict = PyDict::new(py);
            macro_rules! set_opt {
                ($key:expr, $val:expr) => {
                    if let Some(ref v) = $val { meta_dict.set_item($key, v)?; }
                };
            }
            set_opt!("title", meta.title);
            set_opt!("artist", meta.artist);
            set_opt!("album", meta.album);
            set_opt!("album_artist", meta.album_artist);
            set_opt!("album_artist_sort", meta.album_artist_sort);
            set_opt!("track_number", meta.track_number);
            set_opt!("disc_number", meta.disc_number);
            set_opt!("disc_subtitle", meta.disc_subtitle);
            set_opt!("year", meta.year);
            set_opt!("original_year", meta.original_year);
            set_opt!("genre", meta.genre);
            if !meta.genres.is_empty() {
                let genres_list = PyList::empty(py);
                for g in &meta.genres {
                    genres_list.append(g)?;
                }
                meta_dict.set_item("genres", genres_list)?;
            }
            set_opt!("duration_ms", meta.duration_ms);
            set_opt!("sample_rate", meta.sample_rate);
            set_opt!("bit_depth", meta.bit_depth);
            set_opt!("channels", meta.channels);
            set_opt!("format", meta.format);
            set_opt!("file_size", meta.file_size);
            set_opt!("bpm", meta.bpm);
            set_opt!("label", meta.label);
            set_opt!("catalog_number", meta.catalog_number);
            set_opt!("release_date", meta.release_date);
            set_opt!("original_date", meta.original_date);
            set_opt!("musicbrainz_recording_id", meta.musicbrainz_recording_id);
            set_opt!("musicbrainz_release_id", meta.musicbrainz_release_id);
            set_opt!("musicbrainz_artist_id", meta.musicbrainz_artist_id);
            set_opt!("musicbrainz_album_artist_id", meta.musicbrainz_album_artist_id);
            set_opt!("musicbrainz_release_group_id", meta.musicbrainz_release_group_id);
            set_opt!("isrc", meta.isrc);
            meta_dict.set_item("has_cover", meta.has_cover)?;
            meta_dict.set_item("compilation", meta.compilation)?;
            file_dict.set_item("metadata", meta_dict)?;
        }

        if let Some(ref hash) = f.audio_hash {
            file_dict.set_item("audio_hash", hash)?;
        }

        files_list.append(file_dict)?;
    }

    dict.set_item("files", files_list)?;

    let stats_dict = PyDict::new(py);
    stats_dict.set_item("total_files", stats.total_files)?;
    stats_dict.set_item("metadata_ok", stats.metadata_ok)?;
    stats_dict.set_item("metadata_failed", stats.metadata_failed)?;
    stats_dict.set_item("hash_ok", stats.hash_ok)?;
    dict.set_item("stats", stats_dict)?;

    Ok(dict)
}

// ─── Hash ───────────────────────────────────────────────────────

#[pyfunction]
fn audio_hash(py: Python<'_>, path: &str) -> Option<String> {
    py.allow_threads(|| compute_audio_hash_str(path))
}

// ─── Quality ────────────────────────────────────────────────────

#[pyfunction]
#[pyo3(signature = (sr1=None, sr2=None))]
fn same_quality_tier(sr1: Option<u32>, sr2: Option<u32>) -> bool {
    quality::same_quality_tier(sr1, sr2)
}

#[pyfunction]
#[pyo3(signature = (sample_rate=None, bit_depth=None))]
fn quality_suffix_fn(sample_rate: Option<u32>, bit_depth: Option<u16>) -> String {
    quality::quality_suffix(sample_rate, bit_depth)
}

// ─── File Watcher ───────────────────────────────────────────────

struct WatcherInner {
    watcher: FileWatcher,
}

#[pyclass]
pub struct RustFileWatcher {
    inner: Arc<Mutex<Option<WatcherInner>>>,
}

#[pymethods]
impl RustFileWatcher {
    #[new]
    fn new(dirs: Vec<String>) -> PyResult<Self> {
        let watcher = FileWatcher::new(dirs)
            .map_err(pyo3::exceptions::PyRuntimeError::new_err)?;

        Ok(Self {
            inner: Arc::new(Mutex::new(Some(WatcherInner { watcher }))),
        })
    }

    #[pyo3(signature = (timeout_ms=1000, debounce_ms=2000))]
    fn poll_changes<'py>(
        &self,
        py: Python<'py>,
        timeout_ms: u64,
        debounce_ms: u64,
    ) -> PyResult<pyo3::Bound<'py, PyList>> {
        let mut guard = self.inner.lock().unwrap();
        let inner = guard.as_mut().ok_or_else(|| {
            pyo3::exceptions::PyRuntimeError::new_err("watcher stopped")
        })?;

        let changes = py.allow_threads(|| {
            inner.watcher.poll_debounced(
                Duration::from_millis(timeout_ms),
                Duration::from_millis(debounce_ms),
            )
        });

        let list = PyList::empty(py);
        for change in &changes {
            let dict = PyDict::new(py);
            dict.set_item("path", &change.path)?;
            dict.set_item("type", match change.change_type {
                ChangeType::Added => "added",
                ChangeType::Modified => "modified",
                ChangeType::Deleted => "deleted",
            })?;
            list.append(dict)?;
        }
        Ok(list)
    }

    fn stop(&self) -> PyResult<()> {
        let mut guard = self.inner.lock().unwrap();
        if let Some(mut inner) = guard.take() {
            inner.watcher.stop();
        }
        Ok(())
    }
}

pub fn register(m: &pyo3::Bound<'_, pyo3::types::PyModule>) -> PyResult<()> {
    m.add_function(pyo3::wrap_pyfunction!(list_audio_files, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(scan_directories, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(audio_hash, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(same_quality_tier, m)?)?;
    m.add_function(pyo3::wrap_pyfunction!(quality_suffix_fn, m)?)?;
    m.add_class::<RustFileWatcher>()?;
    Ok(())
}
