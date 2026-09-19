use tune_plugin_sdk::{Error, Settings, audio::*, observation::*};
use tune_plugin_testkit::{ObservationQueue, render_f32};

struct DelayFactory;
struct Delay {
    previous: f32,
}
impl DspFactory for DelayFactory {
    fn assess(&self, _: &PlaybackContext, _: &Settings) -> Result<Applicability, Error> {
        Ok(Applicability::Process { requires_pcm: true })
    }
    fn prepare(&self, _: AudioFormat, _: usize, _: &Settings) -> Result<Box<dyn Processor>, Error> {
        Ok(Box::new(Delay { previous: 0.0 }))
    }
}
impl Processor for Delay {
    fn process(
        &mut self,
        block: &mut AudioBlock<'_>,
        _: BlockContext,
    ) -> Result<ProcessReport, Error> {
        let SamplesMut::F32(samples) = block.samples_mut() else {
            return Err(Error::UnsupportedFormat);
        };
        for s in samples.iter_mut() {
            std::mem::swap(s, &mut self.previous);
        }
        Ok(ProcessReport {
            changed: true,
            ..Default::default()
        })
    }
    fn reset(&mut self, _: ResetReason) {
        self.previous = 0.0;
    }
    fn latency_frames(&self) -> u32 {
        1
    }
    fn drain(&mut self, block: &mut AudioBlock<'_>) -> Result<DrainReport, Error> {
        let SamplesMut::F32(samples) = block.samples_mut() else {
            return Err(Error::UnsupportedFormat);
        };
        samples[0] = self.previous;
        Ok(DrainReport {
            frames_written: 1,
            complete: true,
        })
    }
}

#[test]
fn state_survives_block_boundaries_and_tail_is_captured() {
    let format = AudioFormat::new(48_000, ChannelLayout::Mono, SampleEncoding::F32).unwrap();
    let ctx = PlaybackContext {
        zone_id: 7,
        source: SourceKind::Radio,
        delivery: Delivery::Local,
        pure: false,
        protected_bitstream: false,
    };
    for block in [1, 2, 16] {
        let output = render_f32(
            &DelayFactory,
            ctx,
            true,
            &serde_json::json!({}),
            format,
            &[0.5, 0.3, 0.2],
            block,
        )
        .unwrap();
        assert_eq!(
            output.samples,
            [0.0, 0.5, 0.3, 0.2],
            "processor state and drain must survive arbitrary chunking"
        );
    }
}

#[test]
fn spectrum_queue_works_without_a_plugin_and_drops_oldest() {
    let format = AudioFormat::new(48_000, ChannelLayout::Mono, SampleEncoding::S16).unwrap();
    let mut queue = ObservationQueue::new(2).unwrap();
    for position in 0..4 {
        queue
            .publish(SpectrumFrame {
                stamp: ObservationStamp {
                    zone_id: 1,
                    generation: 1,
                    position_frames: position,
                    monotonic_ns: position,
                    format,
                    point: ObservationPoint::DecodedSource,
                    provenance: Provenance::Pipeline,
                    dropped_frames: 0,
                },
                relative: vec![1.0],
                dbfs: vec![-20.0],
                frequencies_hz: vec![1000.0],
                resolved: vec![true],
                fft_size: 2048,
                frames_analyzed: 1920,
                resolution_hz: 25.0,
            })
            .unwrap();
    }
    assert_eq!(
        queue.pop().unwrap().stamp.position_frames,
        2,
        "slow observer must retain recent audio, not block the producer"
    );
    let last = queue.pop().unwrap();
    assert_eq!(last.stamp.position_frames, 3);
    assert_eq!(last.stamp.dropped_frames, 2);
    assert!(queue.pop().is_none());
}
