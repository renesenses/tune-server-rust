use pyo3::prelude::*;
use pyo3::types::PyDict;

use tune_core::db::models::{Artist, Track};
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::track_repo::TrackRepo;

fn artist_to_dict<'py>(py: Python<'py>, a: &Artist) -> PyResult<pyo3::Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    if let Some(id) = a.id { d.set_item("id", id)?; }
    d.set_item("name", &a.name)?;
    if let Some(ref v) = a.sort_name { d.set_item("sort_name", v)?; }
    if let Some(ref v) = a.musicbrainz_id { d.set_item("musicbrainz_id", v)?; }
    if let Some(ref v) = a.bio { d.set_item("bio", v)?; }
    if let Some(ref v) = a.image_path { d.set_item("image_path", v)?; }
    Ok(d)
}

fn track_to_dict<'py>(py: Python<'py>, t: &Track) -> PyResult<pyo3::Bound<'py, PyDict>> {
    let d = PyDict::new(py);
    if let Some(id) = t.id { d.set_item("id", id)?; }
    d.set_item("title", &t.title)?;
    if let Some(v) = t.album_id { d.set_item("album_id", v)?; }
    if let Some(ref v) = t.album_title { d.set_item("album_title", v)?; }
    if let Some(v) = t.artist_id { d.set_item("artist_id", v)?; }
    if let Some(ref v) = t.artist_name { d.set_item("artist_name", v)?; }
    d.set_item("disc_number", t.disc_number)?;
    d.set_item("track_number", t.track_number)?;
    d.set_item("duration_ms", t.duration_ms)?;
    if let Some(ref v) = t.file_path { d.set_item("file_path", v)?; }
    if let Some(ref v) = t.format { d.set_item("format", v)?; }
    if let Some(v) = t.sample_rate { d.set_item("sample_rate", v)?; }
    if let Some(v) = t.bit_depth { d.set_item("bit_depth", v)?; }
    d.set_item("channels", t.channels)?;
    if let Some(ref v) = t.audio_hash { d.set_item("audio_hash", v)?; }
    d.set_item("source", &t.source)?;
    if let Some(ref v) = t.genre { d.set_item("genre", v)?; }
    if let Some(ref v) = t.composer { d.set_item("composer", v)?; }
    if let Some(ref v) = t.musicbrainz_recording_id { d.set_item("musicbrainz_recording_id", v)?; }
    Ok(d)
}

#[pyclass]
pub struct RustDatabase {
    db: SqliteDb,
}

#[pymethods]
impl RustDatabase {
    #[new]
    fn new(path: &str) -> PyResult<Self> {
        let db = if path == ":memory:" {
            SqliteDb::open_in_memory()
        } else {
            SqliteDb::open(path)
        }.map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))?;
        db.init_schema().map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))?;
        Ok(Self { db })
    }

    // ── Artist ──────────────────────────────────────────────

    fn artist_get<'py>(&self, py: Python<'py>, id: i64) -> PyResult<Option<pyo3::Bound<'py, PyDict>>> {
        let repo = ArtistRepo::new(self.db.clone());
        match repo.get(id).map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))? {
            Some(a) => Ok(Some(artist_to_dict(py, &a)?)),
            None => Ok(None),
        }
    }

    fn artist_get_by_name<'py>(&self, py: Python<'py>, name: &str) -> PyResult<Option<pyo3::Bound<'py, PyDict>>> {
        let repo = ArtistRepo::new(self.db.clone());
        match repo.get_by_name(name).map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))? {
            Some(a) => Ok(Some(artist_to_dict(py, &a)?)),
            None => Ok(None),
        }
    }

    fn artist_get_or_create<'py>(&self, py: Python<'py>, name: &str, musicbrainz_id: Option<&str>, sort_name: Option<&str>) -> PyResult<pyo3::Bound<'py, PyDict>> {
        let repo = ArtistRepo::new(self.db.clone());
        let a = repo.get_or_create(name, musicbrainz_id, sort_name)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))?;
        artist_to_dict(py, &a)
    }

    fn artist_count(&self) -> PyResult<i64> {
        let repo = ArtistRepo::new(self.db.clone());
        repo.count().map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))
    }

    fn artist_search<'py>(&self, py: Python<'py>, query: &str, limit: i64) -> PyResult<Vec<pyo3::PyObject>> {
        let repo = ArtistRepo::new(self.db.clone());
        let artists = repo.search(query, limit)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))?;
        artists.iter().map(|a| Ok(artist_to_dict(py, a)?.into())).collect()
    }

    // ── Track ───────────────────────────────────────────────

    fn track_get<'py>(&self, py: Python<'py>, id: i64) -> PyResult<Option<pyo3::Bound<'py, PyDict>>> {
        let repo = TrackRepo::new(self.db.clone());
        match repo.get(id).map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))? {
            Some(t) => Ok(Some(track_to_dict(py, &t)?)),
            None => Ok(None),
        }
    }

    fn track_get_by_path<'py>(&self, py: Python<'py>, path: &str) -> PyResult<Option<pyo3::Bound<'py, PyDict>>> {
        let repo = TrackRepo::new(self.db.clone());
        match repo.get_by_path(path).map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))? {
            Some(t) => Ok(Some(track_to_dict(py, &t)?)),
            None => Ok(None),
        }
    }

    fn track_count(&self) -> PyResult<i64> {
        let repo = TrackRepo::new(self.db.clone());
        repo.count().map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))
    }

    fn track_search<'py>(&self, py: Python<'py>, query: &str, limit: i64) -> PyResult<Vec<pyo3::PyObject>> {
        let repo = TrackRepo::new(self.db.clone());
        let tracks = repo.search(query, limit)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))?;
        tracks.iter().map(|t| Ok(track_to_dict(py, t)?.into())).collect()
    }

    fn track_get_all_paths(&self) -> PyResult<Vec<String>> {
        let repo = TrackRepo::new(self.db.clone());
        let paths = repo.get_all_paths()
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))?;
        Ok(paths.into_iter().collect())
    }

    // ── Album ───────────────────────────────────────────────

    fn album_count(&self) -> PyResult<i64> {
        let repo = AlbumRepo::new(self.db.clone());
        repo.count().map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))
    }

    fn album_delete_orphans(&self) -> PyResult<i64> {
        let repo = AlbumRepo::new(self.db.clone());
        repo.delete_orphans().map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))
    }
}

pub fn register(m: &pyo3::Bound<'_, pyo3::types::PyModule>) -> PyResult<()> {
    m.add_class::<RustDatabase>()?;
    Ok(())
}
