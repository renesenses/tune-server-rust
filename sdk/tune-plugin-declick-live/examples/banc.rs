//! TESS T13 — CPU and latency bench. Release build:
//! `cargo run --release -p tune-plugin-declick-live --example banc`
//! Optional arguments: rates in Hz (default 44100 48000 96000 192000).
//! Nothing is played: the corpus is generated in memory.
use serde_json::json;
use std::time::Instant;
use tune_plugin_declick_live::DeclickLiveFactory;
use tune_plugin_declick_live::corpus::{inject_clicks, programme, stereo};
use tune_plugin_sdk::audio::*;

const SECONDS: f64 = 30.0;
const BLOCK: usize = 512;

fn main() {
    let rates: Vec<u32> = std::env::args()
        .skip(1)
        .filter_map(|a| a.parse().ok())
        .collect();
    let rates = if rates.is_empty() {
        vec![44_100, 48_000, 96_000, 192_000]
    } else {
        rates
    };
    println!(
        "| cadence | latence | temps réel | % d'un cœur | pire bloc (% de sa durée) | clics réparés |"
    );
    println!("|---|---|---|---|---|---|");
    for rate in rates {
        let mut x = stereo(programme, rate, SECONDS, 1);
        inject_clicks(&mut x, 2, rate, 300, 2);
        let mut pcm: Vec<f32> = x.iter().map(|v| *v as f32).collect();
        let fmt = AudioFormat::new(rate, ChannelLayout::Stereo, SampleEncoding::F32).unwrap();
        let mut p = DeclickLiveFactory
            .prepare(fmt, BLOCK, &json!({"enabled": true}))
            .unwrap();
        let ctx = BlockContext {
            zone_id: 1,
            generation: 1,
            position_frames: 0,
        };
        let block_s = BLOCK as f64 / f64::from(rate);
        let mut worst = 0.0f64;
        let start = Instant::now();
        for chunk in pcm.chunks_mut(BLOCK * 2) {
            let t = Instant::now();
            let mut b = AudioBlock::new(fmt, SamplesMut::F32(chunk), BLOCK).unwrap();
            p.process(&mut b, ctx).unwrap();
            worst = worst.max(t.elapsed().as_secs_f64());
        }
        let elapsed = start.elapsed().as_secs_f64();
        let repaired = p.diagnostics()["clicks_repaired"].as_u64().unwrap_or(0);
        println!(
            "| {:.1} kHz | {:.1} ms ({} trames) | ×{:.0} | {:.2} % | {:.1} % | {} |",
            f64::from(rate) / 1000.0,
            p.latency_frames() as f64 * 1000.0 / f64::from(rate),
            p.latency_frames(),
            SECONDS / elapsed,
            100.0 * elapsed / SECONDS,
            100.0 * worst / block_s,
            repaired,
        );
        std::hint::black_box(&pcm);
    }
}
