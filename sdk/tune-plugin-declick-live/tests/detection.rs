//! TESS T13 — synthetic clicks injected into a generated corpus: detection
//! rate, false repairs and local SNR gain. `--nocapture` prints the table.
mod common;
use common::*;
use tune_plugin_declick_live::Event;
use tune_plugin_declick_live::corpus::{
    Click, cymbals, harpsichord, inject_clicks, piano, programme, stereo,
};
use tune_plugin_declick_live::engine::ORDER;
use tune_plugin_sdk::audio::SampleEncoding;

struct Score {
    detected: [usize; 3],
    total: [usize; 3],
    impulses: (usize, usize),
    damped: (usize, usize),
    false_repairs: usize,
    snr_gain_db: f64,
}

fn bucket(amplitude: f64) -> usize {
    let db = 20.0 * amplitude.log10();
    if db < -30.0 {
        0
    } else if db < -20.0 {
        1
    } else {
        2
    }
}

/// An event belongs to a click when it overlaps the click's matched-filter
/// support, the click widened by the AR order on each side.
fn hits(e: &Event, c: &Click) -> bool {
    usize::from(e.channel) == c.channel
        && (e.frame as usize) < c.frame + c.width + ORDER
        && c.frame < e.frame as usize + e.len as usize + ORDER
}

fn score(rate: u32, clean: &[f64], clicks: usize, seed: u64) -> Score {
    let mut dirty = clean.to_vec();
    let injected = inject_clicks(&mut dirty, 2, rate, clicks, seed);
    let r = run(
        rate,
        2,
        &Pcm::quantize(SampleEncoding::F32, &dirty),
        &enabled(),
    );
    let out = r.output.to_f64();
    let mut s = Score {
        detected: [0; 3],
        total: [0; 3],
        impulses: (0, 0),
        damped: (0, 0),
        false_repairs: 0,
        snr_gain_db: 0.0,
    };
    let (mut before, mut after) = (0.0, 0.0);
    let margin = (rate / 500) as usize; // 2 ms around each click
    for c in &injected {
        let found = r.events.iter().any(|e| hits(e, c));
        let b = bucket(c.amplitude);
        s.total[b] += 1;
        s.detected[b] += usize::from(found);
        let kind = if c.width == 1 {
            &mut s.impulses
        } else {
            &mut s.damped
        };
        kind.0 += usize::from(found);
        kind.1 += 1;
        for f in c.frame.saturating_sub(margin)..c.frame + c.width + margin {
            let i = f * 2 + c.channel;
            // F32 carrier: compare against the clean signal at f32 precision.
            let reference = f64::from(clean[i] as f32);
            before += (f64::from(dirty[i] as f32) - reference).powi(2);
            after += (out[i] - reference).powi(2);
        }
    }
    s.false_repairs = r
        .events
        .iter()
        .filter(|e| !injected.iter().any(|c| hits(e, c)))
        .count();
    s.snr_gain_db = 10.0 * (before / after.max(1e-30)).log10();
    s
}

fn print(name: &str, rate: u32, s: &Score) {
    let pct = |d: usize, t: usize| 100.0 * d as f64 / t.max(1) as f64;
    println!(
        "détection {name:<10} {rate:>6} Hz : −40…−30 dBFS {:5.1} % ({}/{}), −30…−20 {:5.1} % ({}/{}), −20…−6 {:5.1} % ({}/{}) ; impulsions {:5.1} %, amorties {:5.1} % ; fausses réparations {} ; gain local {:.1} dB",
        pct(s.detected[0], s.total[0]),
        s.detected[0],
        s.total[0],
        pct(s.detected[1], s.total[1]),
        s.detected[1],
        s.total[1],
        pct(s.detected[2], s.total[2]),
        s.detected[2],
        s.total[2],
        pct(s.impulses.0, s.impulses.1),
        pct(s.damped.0, s.damped.1),
        s.false_repairs,
        s.snr_gain_db,
    );
}

#[test]
fn clicks_in_the_programme_corpus_are_found_and_repaired() {
    for rate in [44_100, 96_000] {
        let clean = stereo(programme, rate, 30.0, 3);
        let s = score(rate, &clean, 200, 17);
        print("programme", rate, &s);
        // Default threshold (18): conservative, chosen so that the
        // counter-proof leaves harpsichord, cymbals and piano untouched.
        // Clicks above −20 dBFS are all found; quieter ones, buried under a
        // programme around −20 dBFS RMS, are the price of that choice.
        assert_eq!(s.detected[2], s.total[2], "clicks above −20 dBFS");
        assert!(
            s.detected[1] * 2 >= s.total[1],
            "−30…−20 dBFS: {}/{}",
            s.detected[1],
            s.total[1]
        );
        assert!(s.false_repairs <= 2, "false repairs: {}", s.false_repairs);
        assert!(
            s.snr_gain_db >= 4.0,
            "local SNR gain {:.1} dB",
            s.snr_gain_db
        );
    }
}

#[test]
fn clicks_in_hard_material_are_reported() {
    // Measurement only (no threshold): clicks hidden under cymbals or
    // harpsichord attacks are the method's known limit; the counter-proof
    // guards the music.
    for (name, generator) in [
        ("piano", piano as fn(u32, f64, u64) -> Vec<f64>),
        ("clavecin", harpsichord),
        ("cymbales", cymbals),
    ] {
        let clean = stereo(generator, 48_000, 20.0, 9);
        let s = score(48_000, &clean, 120, 23);
        print(name, 48_000, &s);
    }
}
