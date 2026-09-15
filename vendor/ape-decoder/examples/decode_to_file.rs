//! Decode an APE file to a WAV file or raw PCM.
//!
//! Usage: cargo run --release --example decode_to_file -- <input.ape> <output.wav>
//!        cargo run --release --example decode_to_file -- --raw <input.ape> <output.pcm>

use std::fs::File;
use std::io::{BufReader, Write};

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let (raw_mode, input, output) = if args.len() == 4 && args[1] == "--raw" {
        (true, &args[2], &args[3])
    } else if args.len() == 3 {
        (false, &args[1], &args[2])
    } else {
        eprintln!("Usage: decode_to_file [--raw] <input.ape> <output>");
        std::process::exit(1);
    };

    let file = File::open(input).expect("failed to open input file");
    let mut decoder =
        ape_decoder::ApeDecoder::new(BufReader::new(file)).expect("failed to parse APE");

    {
        let info = decoder.info();
        eprintln!(
            "{}Hz, {} ch, {}-bit, {} samples ({} ms)",
            info.sample_rate,
            info.channels,
            info.bits_per_sample,
            info.total_samples,
            info.duration_ms,
        );
    }

    let pcm = decoder.decode_all().expect("failed to decode");
    eprintln!("Decoded {} bytes of PCM", pcm.len());

    let mut out = File::create(output).expect("failed to create output file");

    if !raw_mode {
        // Write WAV header: prefer stored header from the APE file, fall back to generated
        let header = decoder
            .wav_header_data()
            .map(|h| h.to_vec())
            .unwrap_or_else(|| decoder.info().generate_wav_header());
        out.write_all(&header).expect("failed to write WAV header");
    }

    out.write_all(&pcm).expect("failed to write PCM data");
    eprintln!("Wrote {}", output);
}
