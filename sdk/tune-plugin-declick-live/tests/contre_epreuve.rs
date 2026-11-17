//! TESS T13 — counter-proof: hard CLEAN material (harpsichord attacks,
//! cymbals, piano), generated. Not a single sample may change: the output
//! must be the input delayed by the announced latency, bit for bit, in
//! every encoding. Run with `--nocapture` to see the rejection counters.
mod common;
use common::*;
use tune_plugin_declick_live::corpus::{cymbals, harpsichord, piano, stereo};
use tune_plugin_sdk::audio::SampleEncoding;

const SECONDS: f64 = 20.0;

fn counter_proof(name: &str, generator: fn(u32, f64, u64) -> Vec<f64>) {
    for rate in [44_100, 96_000] {
        let x = stereo(generator, rate, SECONDS, 7);
        for enc in [
            SampleEncoding::S16,
            SampleEncoding::S24Le,
            SampleEncoding::F32,
        ] {
            let input = Pcm::quantize(enc, &x);
            let r = run(rate, 2, &input, &enabled());
            println!(
                "contre-épreuve {name:<10} {rate:>6} Hz {enc:?}: {} réparation(s), rejets transitoires {}, longs {}, non résolus {}",
                r.stats.clicks_repaired,
                r.stats.rejected_transient,
                r.stats.rejected_long,
                r.stats.rejected_unresolved
            );
            assert!(
                r.events.is_empty(),
                "{name} {rate} Hz {enc:?}: modified {:?}",
                r.events
            );
            assert!(
                r.output == input,
                "{name} {rate} Hz {enc:?}: output differs from the delayed input"
            );
        }
    }
}

#[test]
fn harpsichord_attacks_are_left_untouched() {
    counter_proof("clavecin", harpsichord);
}

#[test]
fn cymbals_are_left_untouched() {
    counter_proof("cymbales", cymbals);
}

#[test]
fn piano_is_left_untouched() {
    counter_proof("piano", piano);
}
