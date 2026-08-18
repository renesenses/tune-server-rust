//! Throughput benchmark for the DSD→PCM streaming converter.
//!
//! Background: a Denon renderer that can't play DSD forced a transcode that
//! took **74–86 s per track** (Xavier Joly / Reivax66, v0.8.235). The FIR
//! decimation loop was the bottleneck. Fixes #380/#383/#384 (passthrough logic,
//! FIR fast-path, fewer full-file copies) landed but there was **no benchmark**
//! to measure the result or catch a regression — and no way to quantify the
//! planned v0.9 f32/SIMD step (Étape 3). This bench fills that gap.
//!
//! It measures the hot `feed()` loop only (streamer construction — including
//! FIR design — is excluded via `iter_batched`), over synthetic DSD input fed
//! in 16 KB chunks to mimic streaming. Throughput is reported in bytes of DSD
//! consumed, so the number reads as "DSD MB/s"; higher is better. Run:
//!
//!     cargo bench -p tune-core --bench dsd_to_pcm

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use std::hint::black_box;
use tune_core::audio::dsd_to_pcm::DsdToPcmStreamer;

/// Deterministic pseudo-random DSD bytes (LCG). Real DSD is high-entropy PDM;
/// an all-zero buffer could be special-cased by the branch predictor and is
/// unrepresentative, so fill with varied bytes. No `rand` dependency, and the
/// same seed every run keeps results comparable across builds.
fn synth_dsd(bytes: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(bytes);
    let mut s: u32 = 0x1234_5678;
    for _ in 0..bytes {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        v.push((s >> 24) as u8);
    }
    v
}

/// Bench one DSD rate: transcode `seconds` of interleaved stereo DSD to PCM.
fn bench_rate(c: &mut Criterion, label: &str, dsd_rate: u32, target_rate: u32, seconds: usize) {
    let channels = 2usize;
    // DSD carries 1 bit per sample → dsd_rate/8 bytes per channel per second.
    let total = (dsd_rate as usize / 8) * seconds * channels;
    let input = synth_dsd(total);

    let mut group = c.benchmark_group("dsd_to_pcm");
    group.throughput(Throughput::Bytes(total as u64));
    group.bench_function(BenchmarkId::from_parameter(label), |b| {
        b.iter_batched(
            || DsdToPcmStreamer::new(dsd_rate, target_rate, channels, true),
            |mut streamer| {
                let mut out_len = 0usize;
                for chunk in input.chunks(16 * 1024) {
                    out_len += streamer.feed(black_box(chunk)).len();
                }
                out_len += streamer.flush().len();
                out_len
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

fn benches(c: &mut Criterion) {
    // choose_output_rate(): DSD64 → 176.4k, DSD128/256 → 352.8k.
    bench_rate(c, "dsd64_stereo_1s", 2_822_400, 176_400, 1);
    bench_rate(c, "dsd128_stereo_1s", 5_644_800, 352_800, 1);
    bench_rate(c, "dsd256_stereo_1s", 11_289_600, 352_800, 1);
}

criterion_group!(benches_group, benches);
criterion_main!(benches_group);
