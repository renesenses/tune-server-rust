//! #7614 — the rate-changing stage contract, exercised through the test kit.
use tune_plugin_sdk::rate::*;
use tune_plugin_sdk::{Error, Settings, audio::*};
use tune_plugin_testkit::render_rate_f32;

/// Zero-order hold at any ratio: output frame `k` repeats input frame
/// `floor(k · in / out)`. Exact frame accounting, no latency.
struct HoldFactory {
    /// Drops one output frame per block: the miscounting stage the kit must
    /// refuse.
    drops_a_frame: bool,
}
struct Hold {
    request: RateRequest,
    produced: u64,
    consumed: u64,
    drops_a_frame: bool,
}

fn context(pure: bool) -> PlaybackContext {
    PlaybackContext {
        zone_id: 7,
        source: SourceKind::Library,
        delivery: Delivery::Local,
        pure,
        protected_bitstream: false,
    }
}

impl RateChangerFactory for HoldFactory {
    fn assess(&self, ctx: &PlaybackContext, _: &Settings) -> Result<Applicability, Error> {
        Ok(match policy_bypass(ctx, true) {
            Some(reason) => Applicability::Bypass(reason),
            None => Applicability::Process { requires_pcm: true },
        })
    }
    fn negotiate(
        &self,
        input: AudioFormat,
        accepted: &[u32],
        _: &Settings,
    ) -> Result<Option<u32>, Error> {
        Ok(accepted
            .iter()
            .copied()
            .filter(|r| *r > input.sample_rate())
            .max())
    }
    fn prepare(&self, request: RateRequest, _: &Settings) -> Result<Box<dyn RateChanger>, Error> {
        Ok(Box::new(Hold {
            request,
            produced: 0,
            consumed: 0,
            drops_a_frame: self.drops_a_frame,
        }))
    }
}

impl RateChanger for Hold {
    fn request(&self) -> RateRequest {
        self.request
    }
    fn process(
        &mut self,
        input: &AudioBlock<'_>,
        output: &mut AudioBlock<'_>,
        _: BlockContext,
    ) -> Result<RateReport, Error> {
        if input.format() != self.request.input || output.format() != self.output_format() {
            return Err(Error::InvalidFormat);
        }
        if output.frames() < self.max_output_frames() {
            return Err(Error::BlockTooLarge);
        }
        let ch = usize::from(input.format().channels());
        let (i, o) = (
            u64::from(self.request.input.sample_rate()),
            u64::from(self.request.output_rate),
        );
        let Samples::F32(src) = input.samples() else {
            return Err(Error::UnsupportedFormat);
        };
        let SamplesMut::F32(dst) = output.samples_mut() else {
            return Err(Error::UnsupportedFormat);
        };
        let end = self.consumed + input.frames() as u64;
        let mut written = 0usize;
        // Output frame k belongs to input frame floor(k·i/o).
        while (self.produced * i) / o < end {
            let source = ((self.produced * i) / o - self.consumed) as usize;
            dst[written * ch..(written + 1) * ch]
                .copy_from_slice(&src[source * ch..(source + 1) * ch]);
            written += 1;
            self.produced += 1;
        }
        self.consumed = end;
        if self.drops_a_frame && written > 0 {
            written -= 1;
        }
        Ok(RateReport {
            frames_consumed: input.frames(),
            frames_written: written,
            ..Default::default()
        })
    }
    fn reset(&mut self, _: ResetReason) {
        self.produced = 0;
        self.consumed = 0;
    }
    fn latency_frames(&self) -> u32 {
        0
    }
    fn drain(&mut self, _: &mut AudioBlock<'_>) -> Result<DrainReport, Error> {
        Ok(DrainReport {
            frames_written: 0,
            complete: true,
        })
    }
}

fn stereo(rate: u32) -> AudioFormat {
    AudioFormat::new(rate, ChannelLayout::Stereo, SampleEncoding::F32).unwrap()
}

fn ramp(frames: usize) -> Vec<f32> {
    (0..frames).flat_map(|n| [n as f32, -(n as f32)]).collect()
}

#[test]
fn integer_ratio_writes_exactly_eight_frames_per_input_frame() {
    let input = ramp(1000);
    let capture = render_rate_f32(
        &HoldFactory {
            drops_a_frame: false,
        },
        context(false),
        true,
        &Settings::Null,
        stereo(44_100),
        &[44_100, 88_200, 352_800],
        &input,
        64,
    )
    .unwrap()
    .expect("a higher accepted rate exists: the stage must run");
    assert_eq!(capture.output_format.sample_rate(), 352_800);
    assert_eq!(capture.output_format.layout(), ChannelLayout::Stereo);
    assert_eq!(capture.processed_frames, 8000);
    for (k, frame) in capture.samples.chunks(2).enumerate() {
        assert_eq!(frame, [(k / 8) as f32, -((k / 8) as f32)], "frame {k}");
    }
}

#[test]
fn rational_ratio_counts_hold_for_every_block_size() {
    let input = ramp(4410);
    for chunk in [1, 7, 147, 1024] {
        let capture = render_rate_f32(
            &HoldFactory {
                drops_a_frame: false,
            },
            context(false),
            true,
            &Settings::Null,
            stereo(44_100),
            &[48_000],
            &input,
            chunk,
        )
        .unwrap()
        .unwrap();
        let expected = expected_output_frames(44_100, 48_000, 4410);
        assert_eq!(expected, 4800);
        assert!(
            capture.processed_frames.abs_diff(expected) <= 1,
            "block size {chunk}: {} frames for {expected}",
            capture.processed_frames
        );
    }
}

#[test]
fn a_stage_that_miscounts_frames_is_refused_before_any_device() {
    let input = ramp(1000);
    let result = render_rate_f32(
        &HoldFactory {
            drops_a_frame: true,
        },
        context(false),
        true,
        &Settings::Null,
        stereo(44_100),
        &[352_800],
        &input,
        64,
    );
    assert!(
        matches!(result, Err(Error::InvalidState)),
        "a stage that writes fewer frames than the ratio must be refused, got {:?}",
        result
            .as_ref()
            .map(|c| c.as_ref().map(|c| c.processed_frames))
    );
}

#[test]
fn pure_and_no_higher_rate_bypass_the_stage() {
    let input = ramp(64);
    let factory = HoldFactory {
        drops_a_frame: false,
    };
    let pure = render_rate_f32(
        &factory,
        context(true),
        true,
        &Settings::Null,
        stereo(44_100),
        &[352_800],
        &input,
        64,
    )
    .unwrap();
    assert!(pure.is_none(), "PURE must never instantiate a rate changer");
    let nothing_higher = render_rate_f32(
        &factory,
        context(false),
        true,
        &Settings::Null,
        stereo(96_000),
        &[44_100, 96_000],
        &input,
        64,
    )
    .unwrap();
    assert!(nothing_higher.is_none());
}

struct Liar;
impl RateChangerFactory for Liar {
    fn assess(&self, _: &PlaybackContext, _: &Settings) -> Result<Applicability, Error> {
        Ok(Applicability::Process { requires_pcm: true })
    }
    fn negotiate(&self, _: AudioFormat, _: &[u32], _: &Settings) -> Result<Option<u32>, Error> {
        Ok(Some(768_000))
    }
    fn prepare(&self, _: RateRequest, _: &Settings) -> Result<Box<dyn RateChanger>, Error> {
        unreachable!("a rate outside the accepted set must stop before prepare")
    }
}

#[test]
fn a_negotiated_rate_the_sink_does_not_accept_is_refused() {
    let result = render_rate_f32(
        &Liar,
        context(false),
        true,
        &Settings::Null,
        stereo(44_100),
        &[352_800],
        &ramp(8),
        8,
    );
    assert!(matches!(result, Err(Error::InvalidState)));
}
