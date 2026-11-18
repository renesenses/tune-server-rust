#![allow(dead_code)]
use serde_json::{Value, json};
use tune_plugin_declick_live::sdk::Instance;
use tune_plugin_declick_live::{DeclickLiveFactory, Event, Stats};
use tune_plugin_sdk::audio::*;

pub const BLOCK: usize = 512;

pub fn enabled() -> Value {
    json!({"enabled": true})
}

pub fn ctx() -> BlockContext {
    BlockContext {
        zone_id: 1,
        generation: 1,
        position_frames: 0,
    }
}

/// Integer or float carrier of one run, quantized from `f64`.
#[derive(Clone, PartialEq, Debug)]
pub enum Pcm {
    S16(Vec<i16>),
    S24(Vec<u8>),
    S32(Vec<i32>),
    F32(Vec<f32>),
    F64(Vec<f64>),
}

impl Pcm {
    pub fn quantize(encoding: SampleEncoding, x: &[f64]) -> Self {
        match encoding {
            SampleEncoding::S16 => Pcm::S16(
                x.iter()
                    .map(|v| (v * 32768.0).round().clamp(-32768.0, 32767.0) as i16)
                    .collect(),
            ),
            SampleEncoding::S24Le => Pcm::S24(
                x.iter()
                    .flat_map(|v| {
                        let q = (v * 8_388_608.0).round().clamp(-8_388_608.0, 8_388_607.0) as i32;
                        let b = q.to_le_bytes();
                        [b[0], b[1], b[2]]
                    })
                    .collect(),
            ),
            SampleEncoding::S32 => Pcm::S32(
                x.iter()
                    .map(|v| {
                        (v * 2_147_483_648.0)
                            .round()
                            .clamp(-2_147_483_648.0, 2_147_483_647.0) as i32
                    })
                    .collect(),
            ),
            SampleEncoding::F32 => Pcm::F32(x.iter().map(|v| *v as f32).collect()),
            SampleEncoding::F64 => Pcm::F64(x.to_vec()),
        }
    }
    pub fn encoding(&self) -> SampleEncoding {
        match self {
            Pcm::S16(_) => SampleEncoding::S16,
            Pcm::S24(_) => SampleEncoding::S24Le,
            Pcm::S32(_) => SampleEncoding::S32,
            Pcm::F32(_) => SampleEncoding::F32,
            Pcm::F64(_) => SampleEncoding::F64,
        }
    }
    pub fn samples(&self) -> usize {
        match self {
            Pcm::S16(v) => v.len(),
            Pcm::S24(v) => v.len() / 3,
            Pcm::S32(v) => v.len(),
            Pcm::F32(v) => v.len(),
            Pcm::F64(v) => v.len(),
        }
    }
    pub fn to_f64(&self) -> Vec<f64> {
        match self {
            Pcm::S16(v) => v.iter().map(|x| f64::from(*x) / 32768.0).collect(),
            Pcm::S24(v) => v
                .as_chunks::<3>()
                .0
                .iter()
                .map(|b| f64::from(i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) / 8_388_608.0)
                .collect(),
            Pcm::S32(v) => v.iter().map(|x| f64::from(*x) / 2_147_483_648.0).collect(),
            Pcm::F32(v) => v.iter().map(|x| f64::from(*x)).collect(),
            Pcm::F64(v) => v.clone(),
        }
    }
    fn block<'a>(&'a mut self, from: usize, to: usize) -> SamplesMut<'a> {
        match self {
            Pcm::S16(v) => SamplesMut::S16(&mut v[from..to]),
            Pcm::S24(v) => SamplesMut::S24Le(&mut v[from * 3..to * 3]),
            Pcm::S32(v) => SamplesMut::S32(&mut v[from..to]),
            Pcm::F32(v) => SamplesMut::F32(&mut v[from..to]),
            Pcm::F64(v) => SamplesMut::F64(&mut v[from..to]),
        }
    }
}

pub struct Run {
    /// Output with the announced latency removed (same length as input).
    pub output: Pcm,
    pub latency: usize,
    pub events: Vec<Event>,
    pub stats: Stats,
}

/// Streams `input` through the SDK stage in blocks, then drains. The
/// output is realigned by the announced latency.
pub fn run(rate: u32, channels: u16, input: &Pcm, settings: &Value) -> Run {
    let layout = if channels == 2 {
        ChannelLayout::Stereo
    } else {
        ChannelLayout::Discrete(channels)
    };
    let format = AudioFormat::new(rate, layout, input.encoding()).unwrap();
    let mut p = DeclickLiveFactory.prepare(format, BLOCK, settings).unwrap();
    let latency = p.latency_frames() as usize;
    let ch = usize::from(channels);
    let total = input.samples();
    let mut stream = input.clone();
    let mut pos = 0;
    while pos < total {
        let end = (pos + BLOCK * ch).min(total);
        let mut b = AudioBlock::new(format, stream.block(pos, end), BLOCK).unwrap();
        p.process(&mut b, ctx()).unwrap();
        pos = end;
    }
    // Drain into a tail of `latency` frames.
    let mut tail = Pcm::quantize(input.encoding(), &vec![0.0; latency * ch]);
    let mut written = 0;
    loop {
        let to = (written + BLOCK).min(latency);
        if to == written {
            break;
        }
        let mut b = AudioBlock::new(format, tail.block(written * ch, to * ch), BLOCK).unwrap();
        let r = p.drain(&mut b).unwrap();
        written += r.frames_written;
        if r.complete {
            break;
        }
    }
    assert_eq!(
        written,
        latency.min(total / ch),
        "drain must return the whole delay line"
    );
    let mut out = stream.to_f64();
    out.extend(tail.to_f64());
    let aligned = out[latency * ch..latency * ch + total].to_vec();
    let instance = (&*p as &dyn std::any::Any)
        .downcast_ref::<Instance>()
        .unwrap();
    Run {
        output: Pcm::quantize(input.encoding(), &aligned),
        latency,
        events: instance.engine().events().to_vec(),
        stats: instance.engine().stats(),
    }
}
