use pyo3::prelude::*;
use pyo3::types::PyDict;
use std::sync::{Arc, Mutex};
use tune_core::audio::formats::AudioFormat;
use tune_core::audio::pipeline::{AudioPipeline, PipelineConfig};
use tokio::sync::mpsc;

struct PipelineInner {
    pipeline: AudioPipeline,
    rx: mpsc::Receiver<Vec<u8>>,
    runtime: tokio::runtime::Runtime,
}

#[pyclass]
pub struct RustPipeline {
    inner: Arc<Mutex<Option<PipelineInner>>>,
}

#[pymethods]
impl RustPipeline {
    #[new]
    #[pyo3(signature = (file_path, output_format="wav", sample_rate=44100, bit_depth=16, channels=2, seek_ms=None, buffer_chunks=512))]
    fn new(
        file_path: &str,
        output_format: &str,
        sample_rate: u32,
        bit_depth: u16,
        channels: u16,
        seek_ms: Option<u64>,
        buffer_chunks: usize,
    ) -> PyResult<Self> {
        let fmt = parse_format(output_format);
        let config = PipelineConfig {
            file_path: file_path.to_string(),
            output_format: fmt,
            sample_rate,
            bit_depth,
            channels,
            seek_ms,
        };

        let (pipeline, rx) = AudioPipeline::new(config, buffer_chunks);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("tokio: {e}")))?;

        Ok(Self {
            inner: Arc::new(Mutex::new(Some(PipelineInner {
                pipeline,
                rx,
                runtime,
            }))),
        })
    }

    fn start(&self) -> PyResult<()> {
        let mut guard = self.inner.lock().unwrap();
        let inner = guard.as_mut().ok_or_else(|| {
            pyo3::exceptions::PyRuntimeError::new_err("pipeline already stopped")
        })?;
        inner.runtime.block_on(inner.pipeline.start())
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e))
    }

    #[pyo3(signature = (timeout_ms=5000))]
    fn read_chunk<'py>(&self, py: Python<'py>, timeout_ms: u64) -> PyResult<Option<Bound<'py, pyo3::types::PyBytes>>> {
        let mut guard = self.inner.lock().unwrap();
        let inner = guard.as_mut().ok_or_else(|| {
            pyo3::exceptions::PyRuntimeError::new_err("pipeline already stopped")
        })?;
        let timeout = std::time::Duration::from_millis(timeout_ms);
        let chunk = py.allow_threads(|| {
            inner.runtime.block_on(async {
                tokio::time::timeout(timeout, inner.rx.recv()).await.ok().flatten()
            })
        });
        match chunk {
            Some(data) => Ok(Some(pyo3::types::PyBytes::new(py, &data))),
            None => Ok(None),
        }
    }

    fn stop(&self) -> PyResult<()> {
        let mut guard = self.inner.lock().unwrap();
        if let Some(mut inner) = guard.take() {
            inner.runtime.block_on(inner.pipeline.stop());
        }
        Ok(())
    }

    fn is_running(&self) -> bool {
        let guard = self.inner.lock().unwrap();
        guard.as_ref().is_some_and(|i| i.pipeline.is_running())
    }

    fn stream_info<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let guard = self.inner.lock().unwrap();
        let inner = guard.as_ref().ok_or_else(|| {
            pyo3::exceptions::PyRuntimeError::new_err("pipeline not initialized")
        })?;
        let info = inner.pipeline.stream_info();
        let dict = PyDict::new(py);
        dict.set_item("format", format!("{:?}", info.format).to_lowercase())?;
        dict.set_item("sample_rate", info.sample_rate)?;
        dict.set_item("bit_depth", info.bit_depth)?;
        dict.set_item("channels", info.channels)?;
        dict.set_item("mime_type", &info.mime_type)?;
        Ok(dict)
    }
}

impl Drop for RustPipeline {
    fn drop(&mut self) {
        let mut guard = self.inner.lock().unwrap();
        if let Some(mut inner) = guard.take() {
            inner.runtime.block_on(inner.pipeline.stop());
        }
    }
}

fn parse_format(s: &str) -> AudioFormat {
    match s.to_lowercase().as_str() {
        "flac" => AudioFormat::Flac,
        "wav" => AudioFormat::Wav,
        "mp3" => AudioFormat::Mp3,
        "aac" => AudioFormat::Aac,
        "alac" => AudioFormat::Alac,
        "ogg" => AudioFormat::Ogg,
        "opus" => AudioFormat::Opus,
        "aiff" => AudioFormat::Aiff,
        _ => AudioFormat::Wav,
    }
}
