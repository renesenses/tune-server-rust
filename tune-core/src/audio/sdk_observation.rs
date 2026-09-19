//! Adapter from Tune's real bounded PCM tap to the SDK observation service.
//! Available without any premium plugin. Current taps describe decoded source
//! audio; post-DSP/pre-output requests are refused, never fabricated.
use crate::playback::PlaybackManager;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Instant,
};
use tune_plugin_sdk::{
    Error,
    audio::{AudioFormat, ChannelLayout, SampleEncoding},
    observation::*,
};
struct Subscription {
    zone: i64,
    receiver: tokio::sync::broadcast::Receiver<super::tap::PcmTapFrame>,
    dropped: u64,
}
pub struct SdkObservations {
    playback: Arc<PlaybackManager>,
    authorized_zones: BTreeSet<i64>,
    subscriptions: BTreeMap<SubscriptionId, Subscription>,
    next_id: u64,
    started: Instant,
}
impl SdkObservations {
    /// Zone authorization is decided by the host, never by the plugin.
    pub fn new(
        playback: Arc<PlaybackManager>,
        authorized_zones: impl IntoIterator<Item = i64>,
    ) -> Self {
        Self {
            playback,
            authorized_zones: authorized_zones.into_iter().collect(),
            subscriptions: BTreeMap::new(),
            next_id: 0,
            started: Instant::now(),
        }
    }
}
impl ObservationHost for SdkObservations {
    fn points(&self) -> &[ObservationPoint] {
        &[ObservationPoint::DecodedSource]
    }
    fn subscribe(
        &mut self,
        zone_id: i64,
        point: ObservationPoint,
    ) -> Result<SubscriptionId, Error> {
        if point != ObservationPoint::DecodedSource {
            return Err(Error::CapabilityMissing);
        }
        if !self.authorized_zones.contains(&zone_id) || self.subscriptions.len() >= 16 {
            return Err(Error::InvalidState);
        }
        self.next_id = self.next_id.checked_add(1).ok_or(Error::InvalidState)?;
        let id = SubscriptionId(self.next_id);
        self.subscriptions.insert(
            id,
            Subscription {
                zone: zone_id,
                receiver: self.playback.zone_tap(zone_id).subscribe(),
                dropped: 0,
            },
        );
        Ok(id)
    }
    fn try_next(&mut self, id: SubscriptionId) -> Result<Option<SpectrumFrame>, Error> {
        let subscription = self.subscriptions.get_mut(&id).ok_or(Error::InvalidState)?;
        let frame = loop {
            match subscription.receiver.try_recv() {
                Ok(frame) => {
                    let current = self
                        .playback
                        .levels_gen(subscription.zone)
                        .load(std::sync::atomic::Ordering::Acquire);
                    if frame.generation != current {
                        subscription.dropped = subscription.dropped.saturating_add(1);
                        continue;
                    }
                    break frame;
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => return Ok(None),
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                    subscription.dropped = subscription.dropped.saturating_add(n)
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                    return Err(Error::InvalidState);
                }
            }
        };
        if frame.zone_id != subscription.zone {
            return Err(Error::InvalidObservation);
        }
        let f = frame.format;
        if f.sample_format != super::tap::SampleFormat::SignedInt {
            return Err(Error::UnsupportedFormat);
        }
        let encoding = match f.bit_depth {
            16 => SampleEncoding::S16,
            24 => SampleEncoding::S24Le,
            32 => SampleEncoding::S32,
            _ => return Err(Error::UnsupportedFormat),
        };
        let format = AudioFormat::new(
            f.sample_rate,
            match f.channels {
                1 => ChannelLayout::Mono,
                2 => ChannelLayout::Stereo,
                n => ChannelLayout::Discrete(n),
            },
            encoding,
        )?;
        let levels =
            super::levels::compute_levels(&frame.pcm, f.bit_depth, f.channels, f.sample_rate);
        let result = SpectrumFrame {
            stamp: ObservationStamp {
                zone_id: frame.zone_id,
                generation: frame.generation,
                position_frames: (frame.track_position.as_secs_f64() * f64::from(f.sample_rate))
                    .round() as u64,
                monotonic_ns: self.started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
                format,
                point: ObservationPoint::DecodedSource,
                provenance: Provenance::SourceProbe,
                dropped_frames: subscription.dropped,
            },
            relative: levels.spectrum,
            dbfs: levels.spectrum_db,
            frequencies_hz: levels.spectrum_hz.to_vec(),
            resolved: levels.spectrum_resolved.to_vec(),
            fft_size: levels.spectrum_fft_size,
            frames_analyzed: levels.spectrum_frames,
            resolution_hz: levels.spectrum_resolution_hz,
        };
        result.validate()?;
        Ok(Some(result))
    }
    fn unsubscribe(&mut self, id: SubscriptionId) {
        self.subscriptions.remove(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn premium_sdk_real_spectrum_without_plugins_refuses_fake_points_and_stale_epochs() {
        let playback = Arc::new(PlaybackManager::new());
        let tap = playback.zone_tap(7);
        let mut host = SdkObservations::new(playback.clone(), [7]);
        assert_eq!(
            host.subscribe(8, ObservationPoint::DecodedSource),
            Err(Error::InvalidState)
        );
        assert_eq!(
            host.subscribe(7, ObservationPoint::PostDsp),
            Err(Error::CapabilityMissing)
        );
        let id = host.subscribe(7, ObservationPoint::DecodedSource).unwrap();
        let frame = |generation| super::super::tap::PcmTapFrame {
            zone_id: 7,
            pcm: (0..1920)
                .flat_map(|i| {
                    let v = ((i as f64 * std::f64::consts::TAU * 1000.0 / 48000.0).sin() * 16000.0)
                        as i16;
                    [v.to_le_bytes(), v.to_le_bytes()].concat()
                })
                .collect::<Vec<_>>()
                .into(),
            format: super::super::tap::PcmFormat {
                sample_rate: 48000,
                channels: 2,
                bit_depth: 16,
                sample_format: super::super::tap::SampleFormat::SignedInt,
            },
            track_position: std::time::Duration::ZERO,
            window: std::time::Duration::from_millis(40),
            play_seq: 1,
            generation,
        };
        tap.publish(frame(1));
        assert!(
            host.try_next(id).unwrap().is_none(),
            "obsolete epoch was published"
        );
        tap.publish(frame(0));
        let spectrum = host.try_next(id).unwrap().unwrap();
        assert!(
            !spectrum.dbfs.is_empty(),
            "spectrum absent with no plugins installed"
        );
        assert_eq!(spectrum.stamp.dropped_frames, 1);
        assert_eq!(spectrum.stamp.point, ObservationPoint::DecodedSource);
        assert_eq!(spectrum.frequencies_hz.len(), spectrum.dbfs.len());
        spectrum.validate().unwrap();
        for _ in 0..100 {
            tap.publish(frame(0));
        }
        assert!(
            host.try_next(id).unwrap().unwrap().stamp.dropped_frames > 1,
            "slow subscriber did not drop old windows"
        );
        host.unsubscribe(id);
        assert!(host.try_next(id).is_err());
    }
}
