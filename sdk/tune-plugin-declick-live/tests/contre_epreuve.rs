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

/// HARD generators, OUTSIDE the counter-proof: measured, not asserted. The
/// white-noise harpsichord's attacks are short broadband bursts; the bare
/// piano's attacks are a linear ramp of every partial up to Nyquist plus a
/// broadband thump. Prints how many samples the plug-in would touch.
#[test]
fn hard_generators_robustness_is_measured() {
    use tune_plugin_declick_live::corpus::{harpsichord_hard, piano_hard};
    for (name, generator) in [
        (
            "clavecin dur",
            harpsichord_hard as fn(u32, f64, u64) -> Vec<f64>,
        ),
        ("piano dur", piano_hard),
    ] {
        for rate in [44_100, 96_000] {
            let x = stereo(generator, rate, SECONDS, 7);
            let input = Pcm::quantize(SampleEncoding::S16, &x);
            let r = run(rate, 2, &input, &enabled());
            println!(
                "robustesse {name:<13} {rate:>6} Hz S16: {} réparation(s), {} échantillon(s) modifié(s)",
                r.stats.clicks_repaired, r.stats.samples_modified
            );
        }
    }
}
