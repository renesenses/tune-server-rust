//! Decode an APE file and print basic info.
//!
//! Usage: cargo run --example decode -- <file.ape>

use std::fs::File;
use std::io::BufReader;

fn main() {
    let path = std::env::args().nth(1).expect("Usage: decode <file.ape>");
    let file = File::open(&path).expect("failed to open file");
    let mut decoder =
        ape_decoder::ApeDecoder::new(BufReader::new(file)).expect("failed to parse APE");

    let info = decoder.info();
    println!(
        "{}Hz, {} ch, {}-bit, {} samples ({} ms)",
        info.sample_rate, info.channels, info.bits_per_sample, info.total_samples, info.duration_ms,
    );

    let pcm = decoder.decode_all().expect("failed to decode");
    println!("Decoded {} bytes of PCM", pcm.len());
}
