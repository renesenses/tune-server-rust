use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::traits::{OutputStatus, OutputTarget, TransportState};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioDevice {
    pub name: String,
    pub is_default: bool,
    pub max_channels: u16,
    pub sample_rates: Vec<u32>,
}

pub fn list_audio_devices() -> Vec<AudioDevice> {
    let host = cpal::default_host();
    let default_name = host
        .default_output_device()
        .and_then(|d| d.name().ok())
        .unwrap_or_default();

    let mut devices = Vec::new();
    if let Ok(output_devices) = host.output_devices() {
        for device in output_devices {
            let name = device.name().unwrap_or_else(|_| "Unknown".into());
            let is_default = name == default_name;

            let (max_channels, sample_rates) =
                if let Ok(configs) = device.supported_output_configs() {
                    let mut max_ch = 0u16;
                    let mut rates = Vec::new();
                    for config in configs {
                        max_ch = max_ch.max(config.channels());
                        let min = config.min_sample_rate().0;
                        let max = config.max_sample_rate().0;
                        for &rate in &[44100, 48000, 88200, 96000, 176400, 192000] {
                            if rate >= min && rate <= max && !rates.contains(&rate) {
                                rates.push(rate);
                            }
                        }
                    }
                    rates.sort();
                    (max_ch, rates)
                } else {
                    (2, vec![44100, 48000])
                };

            devices.push(AudioDevice {
                name,
                is_default,
                max_channels,
                sample_rates,
            });
        }
    }
    devices
}

pub struct LocalOutput {
    device_name: String,
    device_id: String,
    playing: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    stop_tx: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
}

impl LocalOutput {
    pub fn new(device_name: String) -> Self {
        let device_id = format!("local:{device_name}");
        Self {
            device_name,
            device_id,
            playing: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            volume: Arc::new(AtomicU32::new(1000)),
            stop_tx: std::sync::Mutex::new(None),
        }
    }

    fn find_device_name(&self) -> String {
        self.device_name.clone()
    }
}

#[async_trait::async_trait]
impl OutputTarget for LocalOutput {
    fn name(&self) -> &str {
        &self.device_name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "local"
    }

    async fn play_url(
        &self,
        _url: &str,
        _mime_type: &str,
        _title: Option<&str>,
        _artist: Option<&str>,
    ) -> Result<(), String> {
        self.stop().await.ok();

        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let device_name = self.find_device_name();
        let playing = self.playing.clone();
        let paused = self.paused.clone();
        let volume = self.volume.clone();

        playing.store(true, Ordering::SeqCst);
        paused.store(false, Ordering::SeqCst);

        std::thread::spawn(move || {
            let host = cpal::default_host();
            let device = if device_name == "default" {
                host.default_output_device()
            } else {
                host.output_devices().ok().and_then(|mut devs| {
                    devs.find(|d| {
                        d.name().map(|n| n == device_name || n.contains(&device_name)).unwrap_or(false)
                    })
                })
            };

            let Some(device) = device else {
                warn!(name = %device_name, "audio_device_not_found");
                playing.store(false, Ordering::SeqCst);
                return;
            };

            let config = match device.default_output_config() {
                Ok(c) => c,
                Err(e) => {
                    warn!(error = %e, "audio_config_error");
                    playing.store(false, Ordering::SeqCst);
                    return;
                }
            };

            let stream_config: cpal::StreamConfig = config.clone().into();
            let vol = volume.clone();
            let paused_flag = paused.clone();

            let stream = device.build_output_stream(
                &stream_config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    if paused_flag.load(Ordering::Relaxed) {
                        data.fill(0.0);
                        return;
                    }
                    let v = vol.load(Ordering::Relaxed) as f32 / 1000.0;
                    for sample in data.iter_mut() {
                        *sample = 0.0 * v;
                    }
                },
                |e| warn!(error = %e, "audio_stream_error"),
                None,
            );

            let Ok(stream) = stream else {
                warn!("audio_stream_build_failed");
                playing.store(false, Ordering::SeqCst);
                return;
            };

            if let Err(e) = stream.play() {
                warn!(error = %e, "audio_stream_play_failed");
                playing.store(false, Ordering::SeqCst);
                return;
            }

            info!(device = %device_name, "local_audio_playing");
            let _ = stop_rx.recv();

            drop(stream);
            playing.store(false, Ordering::SeqCst);
            info!(device = %device_name, "local_audio_stopped");
        });

        *self.stop_tx.lock().unwrap() = Some(stop_tx);
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.paused.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.paused.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        if let Some(tx) = self.stop_tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
        self.playing.store(false, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        self.volume.store((volume.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::SeqCst);
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        if muted {
            self.volume.store(0, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let state = if self.playing.load(Ordering::Relaxed) {
            if self.paused.load(Ordering::Relaxed) {
                TransportState::Paused
            } else {
                TransportState::Playing
            }
        } else {
            TransportState::Stopped
        };

        Ok(OutputStatus {
            state,
            position_ms: 0,
            duration_ms: 0,
            volume: self.volume.load(Ordering::Relaxed) as f64 / 1000.0,
            muted: false,
            current_uri: None,
            track_title: None,
            track_artist: None,
        })
    }

    async fn is_available(&self) -> bool {
        let host = cpal::default_host();
        if self.device_name == "default" {
            return host.default_output_device().is_some();
        }
        host.output_devices()
            .map(|devs| {
                devs.into_iter().any(|d| {
                    d.name()
                        .map(|n| n == self.device_name || n.contains(&self.device_name))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    }
}
