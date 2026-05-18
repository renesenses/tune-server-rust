use pyo3::prelude::*;
use pyo3::types::PyDict;
use std::path::Path;

#[pyfunction]
fn version() -> &'static str {
    tune_core::version()
}

#[pyfunction]
fn read_metadata(py: Python<'_>, path: &str) -> PyResult<Option<PyObject>> {
    let meta = tune_core::metadata::read_metadata(Path::new(path));
    match meta {
        Some(m) => {
            let dict = PyDict::new(py);
            if let Some(v) = &m.title { dict.set_item("title", v)?; }
            if let Some(v) = &m.artist { dict.set_item("artist", v)?; }
            if let Some(v) = &m.album { dict.set_item("album", v)?; }
            if let Some(v) = &m.album_artist { dict.set_item("album_artist", v)?; }
            if let Some(v) = m.track_number { dict.set_item("track_number", v)?; }
            if let Some(v) = m.disc_number { dict.set_item("disc_number", v)?; }
            if let Some(v) = m.year { dict.set_item("year", v)?; }
            if let Some(v) = &m.genre { dict.set_item("genre", v)?; }
            if let Some(v) = m.duration_ms { dict.set_item("duration_ms", v)?; }
            if let Some(v) = m.sample_rate { dict.set_item("sample_rate", v)?; }
            if let Some(v) = m.bit_depth { dict.set_item("bit_depth", v)?; }
            if let Some(v) = m.channels { dict.set_item("channels", v)?; }
            if let Some(v) = &m.format { dict.set_item("format", v)?; }
            if let Some(v) = m.file_size { dict.set_item("file_size", v)?; }
            if let Some(v) = &m.musicbrainz_recording_id { dict.set_item("musicbrainz_recording_id", v)?; }
            if let Some(v) = &m.musicbrainz_release_id { dict.set_item("musicbrainz_release_id", v)?; }
            if let Some(v) = &m.musicbrainz_artist_id { dict.set_item("musicbrainz_artist_id", v)?; }
            if let Some(v) = &m.isrc { dict.set_item("isrc", v)?; }
            dict.set_item("has_cover", m.has_cover)?;
            Ok(Some(dict.into()))
        }
        None => Ok(None),
    }
}

#[pymodule]
fn tune_native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add_function(wrap_pyfunction!(read_metadata, m)?)?;
    Ok(())
}
