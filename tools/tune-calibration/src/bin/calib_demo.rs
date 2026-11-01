//! Démonstration hors serveur : mesure de pièces simulées, égaliseur
//! automatique, vérification, RT60, coût du FIR. Imprime un rapport
//! Markdown (repris dans docs/etudes/7632-calibration.md).
//!
//! `calib-demo [taux]` — taux d'échantillonnage, 48000 par défaut.

use std::time::Instant;
use tune_calibration::eq::{self, AutoEqConfig};
use tune_calibration::fir;
use tune_calibration::sim::{self, RoomSim};
use tune_calibration::sweep;
use tune_calibration::target::TargetCurve;
use tune_calibration::{analyse_capture, design_eq, MeasurementSpec};

fn main() {
    let fs: u32 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(48_000);
    let mut spec = MeasurementSpec::room_default(fs);
    spec.sweep.duration_s = 6.0;
    spec.extract.post_s = 1.4;
    let sw = sweep::generate(&spec.sweep);
    println!("# Prototype C1/C2 — pièces simulées à {fs} Hz\n");
    println!(
        "Balayage {:.0}–{:.0} Hz, {} s, {:.0} dBFS ; fenêtre {} ms ; lissage 1/{} oct ; grille {} pts/oct.\n",
        spec.sweep.f_start,
        spec.sweep.f_end,
        spec.sweep.duration_s,
        20.0 * spec.sweep.amplitude.log10(),
        spec.window_post_ms,
        spec.smoothing_n,
        spec.points_per_octave
    );

    for (nom, room) in [("sèche", RoomSim::dry_living_room(fs)), ("réverbérante", RoomSim::reverberant_living_room(fs))] {
        println!("## Pièce {nom}\n");
        let t = Instant::now();
        let captured = room.capture(&sw.signal, 2.0);
        let m = analyse_capture(&captured, &sw, &spec, None);
        let dt = t.elapsed();
        println!(
            "- Retard retrouvé : {} éch. = {:.3} ms (vrai {:.3} ms) ; RSB {:.1} dB ; analyse en {:.0} ms.",
            m.ir.delay_samples,
            m.ir.delay_ms(),
            room.direct_delay_s * 1000.0,
            m.ir.snr_db(),
            dt.as_secs_f64() * 1000.0
        );
        println!("- Modes simulés : {}.", room.modes.iter().map(|m| format!("{:.0} Hz +{:.0} dB Q{:.0}", m.freq, m.gain_db, m.q)).collect::<Vec<_>>().join(", "));

        let cfg = AutoEqConfig::single_position(fs as f64);
        let t = Instant::now();
        let r = design_eq(&m.response, &TargetCurve::flat(), &cfg);
        let dt = t.elapsed();
        println!("- Égaliseur ({} bandes, {:.0} ms) :", r.bands.len(), dt.as_secs_f64() * 1000.0);
        println!("\n| Bande | f (Hz) | gain (dB) | Q |\n|---|---|---|---|");
        for (i, b) in r.bands.iter().enumerate() {
            println!("| {} | {:.1} | {:+.2} | {:.2} |", i + 1, b.freq, b.gain, b.q);
        }
        println!(
            "\n- Écart à la cible dans {:.0}–{:.0} Hz (mesure lissée 1/{}) : pic {:+.2} → {:+.2} dB ; dépassement efficace {:.2} → {:.2} dB ; coût pondéré {:.2} → {:.2} dB ; gain cumulé max {:+.2} dB ; préampli {:+.1} dB.",
            cfg.f_lo, cfg.f_hi, cfg.pre_smoothing_n, r.max_peak_before_db, r.max_peak_after_db, r.overshoot_rms_before_db, r.overshoot_rms_after_db, r.rms_before_db, r.rms_after_db, r.max_total_gain_db, r.preamp_db
        );
        if !r.warnings.is_empty() {
            println!("- Avertissements : {}", r.warnings.join(" ; "));
        }

        // Vérification : rejouer à travers l'égaliseur.
        let corrige = sim::apply_bands(fs, &r.bands, &sw.signal);
        let m2 = analyse_capture(&room.capture(&corrige, 2.0), &sw, &spec, None);
        let off1 = TargetCurve::flat().align_offset_percentile(&m.response, cfg.align_lo, cfg.align_hi, cfg.align_percentile);
        let off2 = TargetCurve::flat().align_offset_percentile(&m2.response, cfg.align_lo, cfg.align_hi, cfg.align_percentile);
        println!("\n| f (Hz) | avant (dB) | après (dB) |\n|---|---|---|");
        for f in [30.0, 42.0, 50.0, 68.0, 80.0, 100.0, 115.0, 150.0, 200.0, 300.0] {
            println!("| {f:.0} | {:+.2} | {:+.2} |", m.response.at(f) - off1, m2.response.at(f) - off2);
        }

        println!("\n| Octave (Hz) | EDT (s) | T20 (s) | T30 (s) | dynamique (dB) | RT60 simulé (s) |\n|---|---|---|---|---|---|");
        for b in &m.rt60 {
            let simule = room.tail.iter().find(|t| (t.center_hz - b.center_hz).abs() < 1.0).map(|t| format!("{:.2}", t.rt60_s)).unwrap_or("—".into());
            println!("| {:.0} | {:.2} | {:.2} | {:.2} | {:.0} | {} |", b.center_hz, b.edt_s, b.t20_s, b.t30_s, b.dynamic_range_db, simule);
        }
        println!();
    }

    println!("## Coût du FIR dans le convolueur de Tune (bloc 1024, par canal)\n");
    println!("| fs (Hz) | taps | partitions | latence bloc (ms) | MFLOP/s | FDL (Kio) |\n|---|---|---|---|---|---|");
    for fs in [44_100u32, 48_000, 96_000, 192_000] {
        for taps in [4096usize, 16_384, 65_536] {
            let c = fir::convolution_cost(fs, taps, 1024);
            println!("| {fs} | {taps} | {} | {:.1} | {:.0} | {} |", c.partitions, c.block_latency_ms, c.flops_per_s / 1e6, c.fdl_bytes / 1024);
        }
    }

    println!("\n## FIR phase mixte (esquisse C3) sur la pièce sèche\n");
    let room = RoomSim::dry_living_room(fs);
    let m = analyse_capture(&room.capture(&sw.signal, 2.0), &sw, &spec, None);
    let r = design_eq(&m.response, &TargetCurve::flat(), &AutoEqConfig::single_position(fs as f64));
    let bands = r.bands.clone();
    let corr = move |f: f64| eq::bands_response_db(fs as f64, &bands, &[f])[0];
    let t = Instant::now();
    let h = fir::mixed_phase_design(fs, 16_384, 300.0, &corr);
    let dt = t.elapsed();
    println!("16 384 taps conçus en {:.0} ms. Écart FIR − égaliseur :\n", dt.as_secs_f64() * 1000.0);
    println!("| f (Hz) | EQ (dB) | FIR (dB) |\n|---|---|---|");
    for f in [30.0, 42.0, 68.0, 115.0, 200.0, 300.0, 1000.0, 5000.0] {
        println!("| {f:.0} | {:+.2} | {:+.2} |", corr(f), fir::fir_magnitude_db(&h, fs, f));
    }
}
