mod db_wrapper;
mod discovery_wrapper;
mod pipeline_wrapper;
mod scanner_wrapper;

use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};
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
            macro_rules! set_opt {
                ($key:expr, $val:expr) => {
                    if let Some(ref v) = $val {
                        dict.set_item($key, v)?;
                    }
                };
            }
            set_opt!("title", m.title);
            set_opt!("artist", m.artist);
            set_opt!("album", m.album);
            set_opt!("album_artist", m.album_artist);
            set_opt!("album_artist_sort", m.album_artist_sort);
            set_opt!("track_number", m.track_number);
            set_opt!("disc_number", m.disc_number);
            set_opt!("disc_subtitle", m.disc_subtitle);
            set_opt!("year", m.year);
            set_opt!("original_year", m.original_year);
            set_opt!("release_date", m.release_date);
            set_opt!("original_date", m.original_date);
            set_opt!("genre", m.genre);
            set_opt!("duration_ms", m.duration_ms);
            set_opt!("sample_rate", m.sample_rate);
            set_opt!("bit_depth", m.bit_depth);
            set_opt!("channels", m.channels);
            set_opt!("format", m.format);
            set_opt!("file_size", m.file_size);
            set_opt!("bpm", m.bpm);
            set_opt!("label", m.label);
            set_opt!("catalog_number", m.catalog_number);
            set_opt!("musicbrainz_recording_id", m.musicbrainz_recording_id);
            set_opt!("musicbrainz_release_id", m.musicbrainz_release_id);
            set_opt!("musicbrainz_artist_id", m.musicbrainz_artist_id);
            set_opt!("musicbrainz_album_artist_id", m.musicbrainz_album_artist_id);
            set_opt!(
                "musicbrainz_release_group_id",
                m.musicbrainz_release_group_id
            );
            set_opt!("isrc", m.isrc);
            dict.set_item("has_cover", m.has_cover)?;
            dict.set_item("compilation", m.compilation)?;

            if !m.credits.is_empty() {
                let credits = PyList::empty(py);
                for c in &m.credits {
                    let cd = PyDict::new(py);
                    cd.set_item("name", &c.name)?;
                    cd.set_item("role", &c.role)?;
                    if let Some(ref inst) = c.instrument {
                        cd.set_item("instrument", inst)?;
                    }
                    credits.append(cd)?;
                }
                dict.set_item("credits", credits)?;
            }

            Ok(Some(dict.into()))
        }
        None => Ok(None),
    }
}

#[pyfunction]
fn build_wav_header(py: Python<'_>, channels: u16, sample_rate: u32, bit_depth: u16) -> PyObject {
    let header = tune_core::audio::wav::build_wav_header(channels, sample_rate, bit_depth);
    PyBytes::new(py, &header).into()
}

#[pyfunction]
fn find_ffmpeg() -> Option<String> {
    tune_core::audio::pipeline::find_ffmpeg()
}

#[pyfunction]
fn format_from_extension(ext: &str) -> Option<String> {
    tune_core::audio::formats::AudioFormat::from_extension(ext)
        .map(|f| format!("{:?}", f).to_lowercase())
}

#[pyfunction]
fn mime_type_for_format(format_name: &str) -> String {
    let fmt = match format_name {
        "flac" => tune_core::audio::formats::AudioFormat::Flac,
        "wav" => tune_core::audio::formats::AudioFormat::Wav,
        "mp3" => tune_core::audio::formats::AudioFormat::Mp3,
        "aac" => tune_core::audio::formats::AudioFormat::Aac,
        "alac" => tune_core::audio::formats::AudioFormat::Alac,
        "ogg" => tune_core::audio::formats::AudioFormat::Ogg,
        "opus" => tune_core::audio::formats::AudioFormat::Opus,
        "aiff" => tune_core::audio::formats::AudioFormat::Aiff,
        "dsd" => tune_core::audio::formats::AudioFormat::Dsd,
        "wavpack" => tune_core::audio::formats::AudioFormat::WavPack,
        "ape" => tune_core::audio::formats::AudioFormat::Ape,
        _ => return "application/octet-stream".to_string(),
    };
    fmt.mime_type().to_string()
}

#[pymodule]
fn tune_native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(version, m)?)?;
    m.add_function(wrap_pyfunction!(read_metadata, m)?)?;
    m.add_function(wrap_pyfunction!(build_wav_header, m)?)?;
    m.add_function(wrap_pyfunction!(find_ffmpeg, m)?)?;
    m.add_function(wrap_pyfunction!(format_from_extension, m)?)?;
    m.add_function(wrap_pyfunction!(mime_type_for_format, m)?)?;
    m.add_class::<pipeline_wrapper::RustPipeline>()?;
    m.add_class::<discovery_wrapper::RustSsdpScanner>()?;
    m.add_class::<discovery_wrapper::RustMdnsScanner>()?;
    scanner_wrapper::register(m)?;
    db_wrapper::register(m)?;
    Ok(())
}
