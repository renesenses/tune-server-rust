use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

use tune_core::discovery::device::DiscoveredDevice;
use tune_core::discovery::mdns::{MdnsEvent, MdnsScanner};
use tune_core::discovery::ssdp::{SsdpEvent, SsdpScanner};

fn device_to_pydict<'py>(
    py: Python<'py>,
    dev: &DiscoveredDevice,
) -> PyResult<pyo3::Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("id", &dev.id)?;
    dict.set_item("name", &dev.name)?;
    dict.set_item("type", dev.device_type.to_string())?;
    dict.set_item("host", &dev.host)?;
    dict.set_item("port", dev.port)?;
    dict.set_item("available", dev.available)?;
    if let Some(ref m) = dev.manufacturer {
        dict.set_item("manufacturer", m)?;
    }
    if let Some(ref m) = dev.model {
        dict.set_item("model", m)?;
    }
    if let Some(ref l) = dev.location {
        dict.set_item("location", l)?;
    }
    if let Some(ref v) = dev.airplay_version {
        dict.set_item("airplay_version", v)?;
    }
    if let Some(ref m) = dev.mac_address {
        dict.set_item("mac_address", m)?;
    }
    if !dev.capabilities.is_empty() {
        let caps = PyDict::new(py);
        for (k, v) in &dev.capabilities {
            match v {
                serde_json::Value::Bool(b) => {
                    caps.set_item(k, *b)?;
                }
                serde_json::Value::String(s) => {
                    caps.set_item(k, s)?;
                }
                serde_json::Value::Number(n) => {
                    if let Some(i) = n.as_i64() {
                        caps.set_item(k, i)?;
                    } else if let Some(f) = n.as_f64() {
                        caps.set_item(k, f)?;
                    }
                }
                _ => {
                    let s = serde_json::to_string(v).unwrap_or_default();
                    caps.set_item(k, s)?;
                }
            }
        }
        dict.set_item("capabilities", caps)?;
    }
    Ok(dict)
}

// ─── SSDP ───────────────────────────────────────────────────────

struct SsdpInner {
    scanner: SsdpScanner,
    event_rx: mpsc::Receiver<SsdpEvent>,
    runtime: tokio::runtime::Runtime,
}

#[pyclass]
pub struct RustSsdpScanner {
    inner: Arc<Mutex<Option<SsdpInner>>>,
}

#[pymethods]
impl RustSsdpScanner {
    #[new]
    fn new() -> PyResult<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("tokio: {e}")))?;

        let (event_tx, event_rx) = mpsc::channel(256);
        let scanner = SsdpScanner::new(event_tx);

        Ok(Self {
            inner: Arc::new(Mutex::new(Some(SsdpInner {
                scanner,
                event_rx,
                runtime,
            }))),
        })
    }

    fn start(&self) -> PyResult<()> {
        let mut guard = self.inner.lock().unwrap();
        let inner = guard
            .as_mut()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("scanner already stopped"))?;
        inner.runtime.block_on(inner.scanner.start());
        Ok(())
    }

    fn stop(&self) -> PyResult<()> {
        let mut guard = self.inner.lock().unwrap();
        if let Some(mut inner) = guard.take() {
            inner.runtime.block_on(inner.scanner.stop());
        }
        Ok(())
    }

    fn rescan<'py>(&self, py: Python<'py>) -> PyResult<pyo3::Bound<'py, PyList>> {
        let mut guard = self.inner.lock().unwrap();
        let inner = guard
            .as_mut()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("scanner not running"))?;
        let devices = py.allow_threads(|| inner.runtime.block_on(inner.scanner.rescan()));
        let list = PyList::empty(py);
        for dev in &devices {
            list.append(device_to_pydict(py, dev)?)?;
        }
        Ok(list)
    }

    fn devices<'py>(&self, py: Python<'py>) -> PyResult<pyo3::Bound<'py, PyList>> {
        let guard = self.inner.lock().unwrap();
        let inner = guard
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("scanner not running"))?;
        let devices = inner.runtime.block_on(inner.scanner.devices());
        let list = PyList::empty(py);
        for dev in &devices {
            list.append(device_to_pydict(py, dev)?)?;
        }
        Ok(list)
    }

    fn device_count(&self) -> PyResult<usize> {
        let guard = self.inner.lock().unwrap();
        let inner = guard
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("scanner not running"))?;
        Ok(inner.runtime.block_on(inner.scanner.device_count()))
    }

    fn poll_event<'py>(
        &self,
        py: Python<'py>,
        timeout_ms: u64,
    ) -> PyResult<Option<pyo3::Bound<'py, PyDict>>> {
        let mut guard = self.inner.lock().unwrap();
        let inner = guard
            .as_mut()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("scanner not running"))?;
        let timeout = std::time::Duration::from_millis(timeout_ms);
        let event = py.allow_threads(|| {
            inner.runtime.block_on(async {
                tokio::time::timeout(timeout, inner.event_rx.recv())
                    .await
                    .ok()
                    .flatten()
            })
        });
        match event {
            Some(SsdpEvent::DeviceDiscovered(dev)) => {
                let dict = PyDict::new(py);
                dict.set_item("event", "discovered")?;
                dict.set_item("device", device_to_pydict(py, &dev)?)?;
                Ok(Some(dict))
            }
            Some(SsdpEvent::DeviceLost(id)) => {
                let dict = PyDict::new(py);
                dict.set_item("event", "lost")?;
                dict.set_item("device_id", id)?;
                Ok(Some(dict))
            }
            None => Ok(None),
        }
    }
}

// ─── mDNS ───────────────────────────────────────────────────────

struct MdnsInner {
    scanner: MdnsScanner,
    event_rx: mpsc::Receiver<MdnsEvent>,
    runtime: tokio::runtime::Runtime,
}

#[pyclass]
pub struct RustMdnsScanner {
    inner: Arc<Mutex<Option<MdnsInner>>>,
}

#[pymethods]
impl RustMdnsScanner {
    #[new]
    #[pyo3(signature = (airplay=true, bluos=true, chromecast=true, squeezebox=true, tune_peers=true))]
    fn new(
        airplay: bool,
        bluos: bool,
        chromecast: bool,
        squeezebox: bool,
        tune_peers: bool,
    ) -> PyResult<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(format!("tokio: {e}")))?;

        let (event_tx, event_rx) = mpsc::channel(256);
        let mut scanner =
            MdnsScanner::new(event_tx).map_err(pyo3::exceptions::PyRuntimeError::new_err)?;

        if airplay {
            scanner = scanner.with_airplay();
        }
        if bluos {
            scanner = scanner.with_bluos();
        }
        if chromecast {
            scanner = scanner.with_chromecast();
        }
        if squeezebox {
            scanner = scanner.with_squeezebox();
        }
        if tune_peers {
            scanner = scanner.with_tune_peers();
        }

        Ok(Self {
            inner: Arc::new(Mutex::new(Some(MdnsInner {
                scanner,
                event_rx,
                runtime,
            }))),
        })
    }

    fn start(&self) -> PyResult<()> {
        let mut guard = self.inner.lock().unwrap();
        let inner = guard
            .as_mut()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("scanner already stopped"))?;
        inner
            .scanner
            .start()
            .map_err(pyo3::exceptions::PyRuntimeError::new_err)
    }

    fn stop(&self) -> PyResult<()> {
        let mut guard = self.inner.lock().unwrap();
        if let Some(mut inner) = guard.take() {
            inner.scanner.stop();
        }
        Ok(())
    }

    fn devices<'py>(&self, py: Python<'py>) -> PyResult<pyo3::Bound<'py, PyList>> {
        let guard = self.inner.lock().unwrap();
        let inner = guard
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("scanner not running"))?;
        let devices = inner.runtime.block_on(inner.scanner.devices());
        let list = PyList::empty(py);
        for dev in &devices {
            list.append(device_to_pydict(py, dev)?)?;
        }
        Ok(list)
    }

    fn device_count(&self) -> PyResult<usize> {
        let guard = self.inner.lock().unwrap();
        let inner = guard
            .as_ref()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("scanner not running"))?;
        Ok(inner.runtime.block_on(inner.scanner.device_count()))
    }

    fn poll_event<'py>(
        &self,
        py: Python<'py>,
        timeout_ms: u64,
    ) -> PyResult<Option<pyo3::Bound<'py, PyDict>>> {
        let mut guard = self.inner.lock().unwrap();
        let inner = guard
            .as_mut()
            .ok_or_else(|| pyo3::exceptions::PyRuntimeError::new_err("scanner not running"))?;
        let timeout = std::time::Duration::from_millis(timeout_ms);
        let event = py.allow_threads(|| {
            inner.runtime.block_on(async {
                tokio::time::timeout(timeout, inner.event_rx.recv())
                    .await
                    .ok()
                    .flatten()
            })
        });
        match event {
            Some(MdnsEvent::DeviceDiscovered(dev)) => {
                let dict = PyDict::new(py);
                dict.set_item("event", "discovered")?;
                dict.set_item("device", device_to_pydict(py, &dev)?)?;
                Ok(Some(dict))
            }
            Some(MdnsEvent::DeviceLost(id)) => {
                let dict = PyDict::new(py);
                dict.set_item("event", "lost")?;
                dict.set_item("device_id", id)?;
                Ok(Some(dict))
            }
            Some(MdnsEvent::DeviceUpdated(dev)) => {
                let dict = PyDict::new(py);
                dict.set_item("event", "updated")?;
                dict.set_item("device", device_to_pydict(py, &dev)?)?;
                Ok(Some(dict))
            }
            None => Ok(None),
        }
    }
}
