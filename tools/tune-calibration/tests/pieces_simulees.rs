//! Essais sur pièces simulées : la mesure retrouve les modes et le retard,
//! le RT60 par bande est retrouvé, l'étalonnage micro annule la coloration, la synchronisation par
//! repère fonctionne. Aucun son n'est joué ni capté.

use tune_calibration::analysis::{self, LogResponse};
use tune_calibration::deconv;
use tune_calibration::eq::{self, Band, BandType};
use tune_calibration::mic_cal::MicCalibration;
use tune_calibration::sim::RoomSim;
use tune_calibration::sweep;
use tune_calibration::{analyse_capture, MeasurementSpec};

const FS: u32 = 48_000;

fn spec_rapide() -> MeasurementSpec {
    let mut s = MeasurementSpec::room_default(FS);
    s.sweep.duration_s = 4.0;
    s.points_per_octave = 48;
    s
}

/// Réponse « vraie » de la pièce : même chaîne d'analyse appliquée à l'IR
/// synthétique elle-même (sans balayage ni capture).
fn reponse_vraie(room: &RoomSim, spec: &MeasurementSpec) -> LogResponse {
    let h = room.impulse_response();
    let ir = deconv::ImpulseResponse { sample_rate: FS, samples: h, peak_index: 0, delay_samples: 0 };
    analysis::measured_response(&ir, spec.window_post_ms, spec.smoothing_n, &spec.grid())
}

fn maxima_locaux(r: &LogResponse, f_lo: f64, f_hi: f64, prominence_db: f64) -> Vec<f64> {
    let mut out = Vec::new();
    let m = &r.magnitude_db;
    for i in 2..m.len() - 2 {
        let f = r.freqs[i];
        if f < f_lo || f > f_hi {
            continue;
        }
        if m[i] > m[i - 1] && m[i] >= m[i + 1] {
            // Proéminence grossière : par rapport au minimum dans ±1/3 d'octave.
            let lo = r.freqs.iter().position(|x| *x >= f / 1.26).unwrap_or(0);
            let hi = r.freqs.iter().position(|x| *x > f * 1.26).unwrap_or(m.len());
            let floor = m[lo..hi].iter().cloned().fold(f64::INFINITY, f64::min);
            if m[i] - floor >= prominence_db {
                out.push(f);
            }
        }
    }
    out
}

#[test]
fn la_mesure_retrouve_le_retard_la_reponse_et_les_modes() {
    let spec = spec_rapide();
    let room = RoomSim::dry_living_room(FS);
    let sw = sweep::generate(&spec.sweep);
    let captured = room.capture(&sw.signal, 1.0);
    let m = analyse_capture(&captured, &sw, &spec, None);

    // Retard : 12 ms = 576 échantillons, à l'échantillon près.
    assert_eq!(m.ir.delay_samples, 576, "retard {} ms", m.ir.delay_ms());
    assert!(m.ir.snr_db() > 40.0, "RSB crête/bruit {:.1} dB", m.ir.snr_db());

    // Réponse : écart à la vraie réponse < 0,3 dB de 30 Hz à 10 kHz.
    let vraie = reponse_vraie(&room, &spec);
    let mut worst = (0.0f64, 0.0f64);
    for (i, f) in m.response.freqs.iter().enumerate() {
        if *f >= 30.0 && *f <= 10_000.0 {
            let d = (m.response.magnitude_db[i] - vraie.magnitude_db[i]).abs();
            if d > worst.0 {
                worst = (d, *f);
            }
        }
    }
    let detail: Vec<String> = [30.0, 40.0, 60.0, 100.0, 300.0, 1000.0, 3000.0, 8000.0, 10_000.0]
        .iter()
        .map(|f| format!("{f:.0} Hz {:+.2}", m.response.at(*f) - vraie.at(*f)))
        .collect();
    assert!(worst.0 < 0.3, "écart max {:.3} dB à {:.1} Hz ; {}", worst.0, worst.1, detail.join(", "));

    // Les trois modes ressortent comme maxima locaux à ±4 %.
    let pics = maxima_locaux(&m.response, 25.0, 300.0, 2.0);
    for mode in &room.modes {
        assert!(
            pics.iter().any(|p| (p / mode.freq).log2().abs() < 0.06),
            "mode {} Hz absent des pics {:?}",
            mode.freq,
            pics
        );
    }
}

#[test]
fn le_rt60_par_bande_est_retrouve() {
    let mut spec = spec_rapide();
    spec.extract.post_s = 1.4;
    let room = RoomSim::reverberant_living_room(FS);
    let sw = sweep::generate(&spec.sweep);
    let m = analyse_capture(&room.capture(&sw.signal, 2.0), &sw, &spec, None);
    for band in &room.tail {
        let got = m.rt60.iter().find(|b| (b.center_hz - band.center_hz).abs() < 1.0).unwrap();
        let t = if got.t30_s.is_finite() { got.t30_s } else { got.t20_s };
        let tol = if band.center_hz < 100.0 { 0.35 } else { 0.25 };
        assert!(
            (t - band.rt60_s).abs() / band.rt60_s < tol,
            "{} Hz : T {:.2} s attendu {:.2} s (EDT {:.2}, T20 {:.2}, T30 {:.2}, dyn {:.0} dB)",
            band.center_hz,
            t,
            band.rt60_s,
            got.edt_s,
            got.t20_s,
            got.t30_s,
            got.dynamic_range_db
        );
    }
    // La cascade a bien des tranches décroissantes.
    let wf = analysis::waterfall(&m.ir, 300.0, 30.0, 10, &LogResponse::grid(20.0, 500.0, 24));
    assert_eq!(wf.times_ms.len(), 10);
    let first: f64 = wf.magnitudes_db[0].iter().sum::<f64>() / wf.freqs.len() as f64;
    let last: f64 = wf.magnitudes_db[9].iter().sum::<f64>() / wf.freqs.len() as f64;
    assert!(last < first - 6.0, "cascade : {first:.1} → {last:.1} dB");
}

#[test]
fn l_etalonnage_du_micro_annule_sa_coloration() {
    let spec = spec_rapide();
    let room = RoomSim::dry_living_room(FS);
    let sw = sweep::generate(&spec.sweep);
    let coloration = [
        Band { freq: 150.0, gain: -3.0, q: 1.5, kind: BandType::Peak, channel: None },
        Band { freq: 6000.0, gain: 4.0, q: 0.7, kind: BandType::HighShelf, channel: None },
    ];
    // Fichier d'étalonnage « constructeur » : l'écart du micro sur 60 points.
    let mut cal = String::from("\"Sens Factor =-0.5dB, SERNO: 7000042\"\n");
    for f in LogResponse::grid(10.0, 20_000.0, 6) {
        cal.push_str(&format!("{:.3}\t{:.3}\n", f, eq::bands_response_db(FS as f64, &coloration, &[f])[0]));
    }
    let mic = MicCalibration::parse(&cal).unwrap();

    let sans_micro = analyse_capture(&room.capture(&sw.signal, 1.0), &sw, &spec, None);
    let brut = analyse_capture(&room.capture_with_mic(&sw.signal, 1.0, &coloration), &sw, &spec, None);
    let corrige = analyse_capture(&room.capture_with_mic(&sw.signal, 1.0, &coloration), &sw, &spec, Some(&mic));

    let ecart = |a: &LogResponse, b: &LogResponse| {
        a.freqs
            .iter()
            .enumerate()
            .filter(|(_, f)| **f >= 30.0 && **f <= 15_000.0)
            .map(|(i, _)| (a.magnitude_db[i] - b.magnitude_db[i]).abs())
            .fold(0.0f64, f64::max)
    };
    assert!(ecart(&brut.response, &sans_micro.response) > 2.5, "la coloration doit se voir sans étalonnage");
    let residu = ecart(&corrige.response, &sans_micro.response);
    assert!(residu < 0.25, "résidu après étalonnage {residu:.3} dB");
    assert!((mic.spl_from_dbfs(-18.0).unwrap() - 94.5).abs() < 1e-9);
}

#[test]
fn le_repere_de_synchronisation_retrouve_l_origine_sans_horloge_commune() {
    // Séquence jouée : repère, 200 ms de silence, balayage. La capture commence
    // à un instant inconnu (téléphone), avec un retard acoustique de 12 ms.
    let spec = spec_rapide();
    let room = RoomSim::dry_living_room(FS);
    let sw = sweep::generate(&spec.sweep);
    let marker = deconv::sync_marker(FS, 0.1, sw.spec.amplitude);
    let gap = (0.2 * FS as f64) as usize;
    let mut played = marker.clone();
    played.extend(std::iter::repeat(0.0).take(gap));
    played.extend(&sw.signal);
    // Le téléphone a commencé à enregistrer `inconnu` échantillons avant la lecture.
    let inconnu = 31_337;
    let mut captured = vec![0.0; inconnu];
    captured.extend(room.capture(&played, 1.0));

    let t0 = deconv::find_marker_offset(&captured, &marker);
    assert_eq!(t0, inconnu + 576, "origine retrouvée");
    // À partir de t0, le balayage commence à t0 + marker + gap : on déconvolue
    // la tranche correspondante et l'on doit retrouver un retard nul (le vol
    // acoustique est déjà dans t0, commun au repère et au balayage).
    let start = t0 + marker.len() + gap;
    let ir = deconv::deconvolve(&captured[start..], &sw, &deconv::ExtractSpec::default());
    assert!(ir.delay_samples <= 1, "retard résiduel {} éch.", ir.delay_samples);
}

#[test]
fn la_moyenne_spatiale_lisse_les_differences_entre_positions() {
    let spec = spec_rapide();
    let sw = sweep::generate(&spec.sweep);
    let mut p1 = RoomSim::dry_living_room(FS);
    let mut p2 = RoomSim::dry_living_room(FS);
    // En position 2, le mode à 68 Hz est un creux (nœud) et celui de 42 Hz plus fort.
    p2.modes[1].gain_db = -8.0;
    p2.modes[0].gain_db = 12.0;
    p1.direct_delay_s = 0.011;
    let m1 = analyse_capture(&p1.capture(&sw.signal, 1.0), &sw, &spec, None);
    let m2 = analyse_capture(&p2.capture(&sw.signal, 1.0), &sw, &spec, None);
    let avg = analysis::spatial_average(&[m1.response.clone(), m2.response.clone()], &[1.0, 1.0], true);
    // Le mode commun (42 Hz) reste un pic de la moyenne ; le mode discordant
    // (68 Hz, creux en position 2) y est atténué d'au moins 2,5 dB par rapport
    // à la position 1.
    let pic = |r: &LogResponse, f: f64| r.at(f) - r.mean_db(150.0, 300.0);
    assert!(pic(&avg, 42.0) > 4.0, "42 Hz dans la moyenne : {:.1} dB", pic(&avg, 42.0));
    // Moyenne de puissance de +6 et −8 dB = +3,2 dB : atténuation attendue ≈ 2,8 dB.
    assert!(pic(&avg, 68.0) < pic(&m1.response, 68.0) - 2.5, "68 Hz : {:.1} → {:.1} dB", pic(&m1.response, 68.0), pic(&avg, 68.0));
    let lr = analysis::difference_db(&m1.response, &m2.response);
    assert!(lr.magnitude_db.iter().any(|d| d.abs() > 6.0));
}
