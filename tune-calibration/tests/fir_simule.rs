//! C3 (#7632, lot TESS T10b) : FIR de correction calculé depuis la MESURE
//! (moyenne de 5 positions, T10a), éprouvé sur pièces simulées. Aucun son
//! n'est joué ni capté : la « remesure » joue le balayage PASSÉ PAR LE FIR
//! dans la pièce simulée, à chaque position, et refait la moyenne — comme le
//! testeur le fera, correction active.
//!
//! Critère (plan TESS, T10b) : la réponse corrigée est dans ±1,5 dB de la
//! cible sur 30 Hz–15 kHz, lue avec le lissage VARIABLE de la conception
//! (`fir::lissage_variable` : 1/12 d'octave sous 300 Hz, 1/3 d'octave
//! au-dessus de 1 kHz, comme le « Var smoothing » de REW). Le pire écart à
//! 1/12 d'octave partout est imprimé aussi : au-dessus de Schroeder, ce qu'il
//! en reste est le peigne des réflexions, qui change avec la place. Ce critère est HORS LIGNE : il ne vaut aucune
//! revendication (Q10) tant que deux pièces réelles n'ont pas été mesurées.

use tune_calibration::analysis::LogResponse;
use tune_calibration::fft;
use tune_calibration::fir::{self, concevoir_fir, ConfigFir, FirMesure, FormatRi};
use tune_calibration::positions::{moyenne_positions, ConfigPositions};
use tune_calibration::sim::{RoomMode, RoomSim, POSITIONS_SIMULEES};
use tune_calibration::sweep;
use tune_calibration::target::TargetCurve;
use tune_calibration::{analyse_capture, MeasurementSpec};

const FS: u32 = 48_000;
const F_LO: f64 = 30.0;
const F_HI: f64 = 15_000.0;
const TOLERANCE_DB: f64 = 1.5;

fn spec() -> MeasurementSpec {
    let mut s = MeasurementSpec::room_default(FS);
    s.sweep.duration_s = 3.0;
    s
}

/// Mesure d'une position ; `fir` = le filtre inséré avant la pièce.
fn mesurer(room: &RoomSim, spec: &MeasurementSpec, fir: Option<&[f64]>) -> LogResponse {
    let sw = sweep::generate(&spec.sweep);
    let joue = match fir {
        Some(h) => fft::convolve(&sw.signal, h),
        None => sw.signal.clone(),
    };
    analyse_capture(&room.capture(&joue, 1.0), &sw, spec, None).response
}

fn moyenne_5(base: &RoomSim, spec: &MeasurementSpec, fir: Option<&[f64]>) -> LogResponse {
    let mesures: Vec<LogResponse> = POSITIONS_SIMULEES[..5]
        .iter()
        .map(|u| mesurer(&base.a_la_position(*u), spec, fir))
        .collect();
    moyenne_positions(&mesures, 0, &ConfigPositions::default())
        .unwrap()
        .reponse
}

/// Pire écart à la cible : lissage variable (le verdict) ou 1/12 d'octave
/// partout (imprimé, pour mémoire).
fn pire(r: &LogResponse, cible: &TargetCurve, variable: bool) -> (f64, f64) {
    let mut cfg = ConfigFir::pour(FS);
    if !variable {
        // Transition repoussée au-delà de la bande : aucun lissage ajouté.
        cfg.transition_lo_hz = 30_000.0;
        cfg.transition_hi_hz = 40_000.0;
    }
    fir::pire_ecart_a_la_cible(r, cible, &cfg, F_LO, F_HI)
}

fn eprouver(nom: &str, base: &RoomSim, cible: &TargetCurve) -> FirMesure {
    let spec = spec();
    let avant = moyenne_5(base, &spec, None);
    let fir = concevoir_fir(&avant, cible, &ConfigFir::pour(FS)).unwrap();
    let apres = moyenne_5(base, &spec, Some(&fir.taps));
    let (f_av, p_av) = pire(&avant, cible, true);
    let (f_ap, p_ap) = pire(&apres, cible, true);
    let (f_br, p_br) = pire(&apres, cible, false);
    // La prévision (mesure + module du FIR) colle à la remesure.
    let (_, p_prevu) = pire(&fir::reponse_prevue(&avant, &fir), cible, true);
    assert!(
        (p_prevu - p_ap).abs() < 0.5,
        "{nom} : prévu {p_prevu:+.2} dB, remesuré {p_ap:+.2} dB"
    );
    println!(
        "T10b {nom} : {} coefficients, latence {:.1} ms, normalisation −{:.2} dB, \
         {} points bornés ({} en creux) | pire écart à la cible {F_LO}–{F_HI} Hz : \
         avant {p_av:+.2} dB à {f_av:.0} Hz, après {p_ap:+.2} dB à {f_ap:.0} Hz \
         (à 1/12 d'octave partout : {p_br:+.2} dB à {f_br:.0} Hz)",
        fir.taps.len(),
        fir.latence_ms(),
        fir.normalisation_db,
        fir.points_bornes,
        fir.points_creux_non_combles
    );
    assert!(
        p_av.abs() > 2.0 * TOLERANCE_DB,
        "{nom} : la pièce simulée doit être hors tolérance avant correction ({p_av:+.2} dB)"
    );
    assert!(
        p_ap.abs() <= TOLERANCE_DB,
        "{nom} : après correction, {p_ap:+.2} dB à {f_ap:.0} Hz (tolérance ±{TOLERANCE_DB} dB)"
    );
    fir
}

/// LA preuve : salon sec (3 modes, réflexions qui bougent avec la place),
/// cible plate, 5 positions → FIR → remesure à 5 positions.
#[test]
fn la_reponse_corrigee_tient_la_cible_plate_a_1_5_db() {
    eprouver(
        "salon sec, cible plate",
        &RoomSim::dry_living_room(FS),
        &TargetCurve::flat(),
    );
}

/// Même preuve, cible configurable : pente Harman « in-room ».
#[test]
fn la_reponse_corrigee_tient_la_cible_harman_a_1_5_db() {
    eprouver(
        "salon sec, cible Harman",
        &RoomSim::dry_living_room(FS),
        &TargetCurve::harman_in_room(),
    );
}

/// Pièce réverbérante (queue diffuse, RT60 0,3–0,7 s).
#[test]
fn la_reponse_corrigee_tient_la_cible_en_piece_reverberante() {
    eprouver(
        "salon réverbérant, cible plate",
        &RoomSim::reverberant_living_room(FS),
        &TargetCurve::flat(),
    );
}

/// Contre-épreuve 1 : une mesure déjà conforme à une cible plate donne un
/// FIR ≈ identité : une impulsion unique au rang de la latence, module plat.
#[test]
fn cible_plate_et_mesure_plate_donnent_l_identite() {
    let plate = LogResponse {
        freqs: LogResponse::grid(20.0, 20_000.0, 48),
        magnitude_db: vec![75.0; LogResponse::grid(20.0, 20_000.0, 48).len()],
    };
    let fir = concevoir_fir(&plate, &TargetCurve::flat(), &ConfigFir::pour(FS)).unwrap();
    let l = fir.latence_echantillons;
    let e_tot: f64 = fir.taps.iter().map(|v| v * v).sum();
    println!(
        "T10b identité : pic {:.6} au rang {l}, énergie hors pic {:.2e}",
        fir.taps[l],
        e_tot - fir.taps[l] * fir.taps[l]
    );
    assert!((fir.taps[l] - 1.0).abs() < 1e-6, "pic {}", fir.taps[l]);
    assert!(e_tot - fir.taps[l] * fir.taps[l] < 1e-9);
    assert_eq!(fir.points_bornes, 0);
    let freqs = LogResponse::grid(20.0, 20_000.0, 12);
    for (f, g) in freqs.iter().zip(fir::module_fir_db(&fir.taps, FS, &freqs)) {
        assert!(g.abs() < 0.01, "{f} Hz : {g} dB");
    }
}

/// Contre-épreuve 1 bis, sur une VRAIE mesure : pièce anéchoïque simulée
/// (son direct seul) et cible plate → FIR quasi neutre.
#[test]
fn piece_neutre_mesuree_donne_un_fir_quasi_neutre() {
    let mut neutre = RoomSim::dry_living_room(FS);
    neutre.modes.clear();
    neutre.reflections.clear();
    let m = mesurer(&neutre, &spec(), None);
    let fir = concevoir_fir(&m, &TargetCurve::flat(), &ConfigFir::pour(FS)).unwrap();
    let freqs = LogResponse::grid(F_LO, F_HI, 12);
    let g = fir::module_fir_db(&fir.taps, FS, &freqs);
    let max = g.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let min = g.iter().cloned().fold(f64::INFINITY, f64::min);
    println!("T10b pièce neutre : module du FIR entre {min:+.3} et {max:+.3} dB");
    assert!(max - min < 0.3, "FIR non neutre : {min:+.3}..{max:+.3} dB");
}

/// Contre-épreuve 2 : un creux profond (−20 dB, étroit) n'est PAS comblé
/// au-delà de la borne. Le FIR n'y remonte pas plus que `remontee_max_db`
/// au-dessus de son niveau de référence (médiane 200 Hz–2 kHz).
#[test]
fn un_creux_profond_n_est_pas_comble_au_dela_de_la_borne() {
    let mut piece = RoomSim::dry_living_room(FS);
    piece.modes.push(RoomMode {
        freq: 90.0,
        gain_db: -20.0,
        q: 8.0,
    });
    let m = moyenne_5(&piece, &spec(), None);
    let cfg = ConfigFir::pour(FS);
    let fir = concevoir_fir(&m, &TargetCurve::flat(), &cfg).unwrap();
    let freqs = LogResponse::grid(20.0, 20_000.0, 48);
    let g = fir::module_fir_db(&fir.taps, FS, &freqs);
    let mut refs: Vec<f64> = freqs
        .iter()
        .zip(&g)
        .filter(|(f, _)| **f >= 200.0 && **f <= 2000.0)
        .map(|(_, g)| *g)
        .collect();
    refs.sort_by(|a, b| a.total_cmp(b));
    let reference = refs[refs.len() / 2];
    let au_creux = fir::module_fir_db(&fir.taps, FS, &[90.0])[0] - reference;
    let remontee_max = freqs
        .iter()
        .zip(&g)
        .map(|(_, g)| g - reference)
        .fold(f64::NEG_INFINITY, f64::max);
    let profondeur = m.at(200.0) - m.at(90.0);
    println!(
        "T10b creux 90 Hz : profondeur mesurée {profondeur:.1} dB, FIR au creux {au_creux:+.2} dB, \
         remontée max du FIR {remontee_max:+.2} dB (borne {} dB), {} points en creux non comblés",
        cfg.remontee_max_db, fir.points_creux_non_combles
    );
    assert!(
        profondeur > 8.0,
        "le creux doit survivre à la moyenne : {profondeur:.1} dB"
    );
    assert!(au_creux <= 0.5, "le creux est comblé : {au_creux:+.2} dB");
    assert!(
        remontee_max <= cfg.remontee_max_db + 0.5,
        "remontée {remontee_max:+.2} dB > borne {} dB",
        cfg.remontee_max_db
    );
    assert!(fir.points_creux_non_combles > 0);
    assert!(!fir.avertissements.is_empty());
}

/// Le filtre ne dépasse jamais 0 dB (normalisation) : aucun écrêtage.
#[test]
fn le_filtre_normalise_ne_depasse_pas_0_db() {
    let m = moyenne_5(&RoomSim::dry_living_room(FS), &spec(), None);
    let fir = concevoir_fir(&m, &TargetCurve::harman_in_room(), &ConfigFir::pour(FS)).unwrap();
    let freqs = LogResponse::grid(10.0, 23_000.0, 96);
    let max = fir::module_fir_db(&fir.taps, FS, &freqs)
        .into_iter()
        .fold(f64::NEG_INFINITY, f64::max);
    assert!(max <= 0.01, "gain max {max:+.3} dB");
}

/// La RI produite est un WAV flottant relu à l'identique (fmt + data).
#[test]
fn la_ri_wav_se_relit_a_l_identique() {
    let m = moyenne_5(&RoomSim::dry_living_room(FS), &spec(), None);
    let fir = concevoir_fir(&m, &TargetCurve::flat(), &ConfigFir::pour(FS)).unwrap();
    for (format, octets) in [(FormatRi::Float32, 4usize), (FormatRi::Float64, 8)] {
        let wav = fir::wav_ri(&[&fir.taps, &fir.taps], FS, format).unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 3, "IEEE float");
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 2, "canaux");
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), FS);
        assert_eq!(&wav[36..40], b"data");
        let n = u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize;
        assert_eq!(n, fir.taps.len() * 2 * octets);
        assert_eq!(wav.len(), 44 + n);
        // Deuxième coefficient du canal droit.
        let o = 44 + 3 * octets;
        let v = if octets == 4 {
            f32::from_le_bytes(wav[o..o + 4].try_into().unwrap()) as f64
        } else {
            f64::from_le_bytes(wav[o..o + 8].try_into().unwrap())
        };
        assert!((v - fir.taps[1]).abs() < 1e-6);
    }
}
