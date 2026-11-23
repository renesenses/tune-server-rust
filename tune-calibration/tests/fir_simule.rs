//! C3 (#7632, TESS T10b) : le FIR de correction calculé depuis la MESURE,
//! éprouvé sur pièces simulées. Aucun son n'est joué ni capté : la
//! « capture » est la convolution du balayage par la pièce synthétique.
//!
//! Preuves :
//! * la réponse corrigée (mesure ⊛ FIR, et pièce VRAIE ⊛ FIR) est dans
//!   ±1,5 dB de la cible sur 30 Hz–15 kHz ; contre-épreuve : sans FIR
//!   (identité), le même contrôle échoue ;
//! * la phase excédentaire d'un passe-tout (raccordement de caisson) est
//!   corrigée sous la fréquence de bascule, dans le pré-tintement annoncé ;
//! * la latence annoncée est la position du pic et le retard de groupe ;
//! * le fichier `.wav` se relit, par les pas du lecteur du convolueur, sans
//!   écart (64 bits) ou à l'arrondi `f32` près (32 bits).
//!
//! Lissage du contrôle : réponse analysée comme la mesure (fenêtre de
//! 500 ms, 1/12 d'octave), puis 1/6 d'octave sous 300 Hz et 1/3 au-dessus
//! (`fir::lissage_variable`), le lissage de la conception.

use tune_calibration::analysis::{self, LogResponse};
use tune_calibration::deconv::ImpulseResponse;
use tune_calibration::eq::Biquad;
use tune_calibration::fft;
use tune_calibration::fir::{
    concevoir_fir, ecart_max_a_la_cible_db, lissage_variable, reponse_corrigee, FirCorrection,
    OptionsFir, OptionsPhase,
};
use tune_calibration::sim::{self, RoomSim};
use tune_calibration::sweep;
use tune_calibration::target::TargetCurve;
use tune_calibration::wav::{ecrire_wav, FormatEchantillon};
use tune_calibration::{analyse_capture, MeasurementSpec};

const FS: u32 = 48_000;
const BANDE: (f64, f64) = (30.0, 15_000.0);
const TOLERANCE_DB: f64 = 1.5;

fn spec() -> MeasurementSpec {
    let mut s = MeasurementSpec::room_default(FS);
    s.sweep.duration_s = 4.0;
    s
}

/// Balayage, capture simulée, déconvolution, analyse : la mesure telle que
/// le greffon la fait.
fn mesurer(room: &RoomSim) -> (ImpulseResponse, LogResponse) {
    let s = spec();
    let sw = sweep::generate(&s.sweep);
    let cap = room.capture(&sw.signal, 1.0);
    let m = analyse_capture(&cap, &sw, &s, None);
    (m.ir, m.response)
}

fn vraie(room: &RoomSim) -> ImpulseResponse {
    ImpulseResponse {
        sample_rate: FS,
        samples: room.impulse_response(),
        peak_index: 0,
        delay_samples: 0,
    }
}

/// Écart maximal à la cible de `ri` ⊛ `h`, au lissage de la conception.
/// `decalage` : `None` = réaligné par la médiane (pièce vraie, dont le
/// niveau absolu n'est pas celui de la mesure).
fn ecart(
    ri: &ImpulseResponse,
    h: &[f64],
    latence: usize,
    cible: &TargetCurve,
    decalage: Option<f64>,
) -> f64 {
    let grille = LogResponse::grid(20.0, 20_000.0, 48);
    let r = lissage_variable(&reponse_corrigee(ri, h, latence, &grille), 6, 3, 300.0);
    let d = decalage.unwrap_or_else(|| cible.align_offset(&r, BANDE.0, BANDE.1));
    ecart_max_a_la_cible_db(&r, cible, d, BANDE.0, BANDE.1)
}

fn concevoir(
    ir: &ImpulseResponse,
    m: &LogResponse,
    cible: &TargetCurve,
    phase: bool,
) -> FirCorrection {
    let o = OptionsFir {
        phase: phase.then(OptionsPhase::default),
        ..OptionsFir::default()
    };
    concevoir_fir(FS, m, Some(ir), cible, &o).expect("FIR")
}

/// La preuve de T10b : le salon sec du prototype (modes de +9, +6 et +5 dB
/// à 42, 68 et 115 Hz, réflexion de sol), mesuré par balayage, corrigé vers
/// une cible plate et vers la cible Harman, en phase minimale et en phase
/// mixte : dans ±1,5 dB sur 30 Hz–15 kHz. Contre-épreuve : l'identité ne
/// passe pas le même contrôle.
#[test]
fn le_fir_mesure_ramene_la_piece_a_1_5_db_de_la_cible() {
    let room = RoomSim::dry_living_room(FS);
    let (ir, m) = mesurer(&room);
    let vraie = vraie(&room);
    for (nom, cible) in [
        ("plate", TargetCurve::flat()),
        ("harman", TargetCurve::harman_in_room()),
    ] {
        for phase in [false, true] {
            let fir = concevoir(&ir, &m, &cible, phase);
            let e_mesure = ecart(
                &ir,
                &fir.coefficients,
                fir.latence_trames,
                &cible,
                Some(fir.decalage_cible_db),
            );
            let e_vraie = ecart(&vraie, &fir.coefficients, fir.latence_trames, &cible, None);
            eprintln!(
                "T10b {nom} phase_mixte={phase} : prises {} latence {} trames ({:.3} ms) pic {} ; écart max 30 Hz–15 kHz : mesure⊛FIR {e_mesure:.3} dB, pièce vraie⊛FIR {e_vraie:.3} dB ; gain {:.2}…{:.2} dB, normalisation {:.2} dB",
                fir.coefficients.len(),
                fir.latence_trames,
                fir.latence_ms(),
                fir.pic_trame,
                fir.gain_min_db,
                fir.gain_max_db,
                fir.normalisation_db
            );
            assert!(
                e_mesure < TOLERANCE_DB,
                "{nom}, phase mixte {phase} : mesure⊛FIR à {e_mesure:.2} dB"
            );
            assert!(
                e_vraie < TOLERANCE_DB,
                "{nom}, phase mixte {phase} : pièce vraie⊛FIR à {e_vraie:.2} dB"
            );
            assert_eq!(fir.pic_trame, fir.latence_trames);
            assert!(
                fir.gain_max_db <= 0.05,
                "crête du filtre {} dB",
                fir.gain_max_db
            );
        }
        // Contre-épreuve : sans FIR, le même contrôle échoue (les modes).
        let identite = [1.0];
        let e_sans = ecart(&ir, &identite, 0, &cible, None);
        eprintln!("T10b {nom} contre-épreuve sans FIR : écart max {e_sans:.3} dB");
        assert!(
            e_sans > TOLERANCE_DB + 3.0,
            "sans FIR : {e_sans:.2} dB, le banc ne discrimine pas"
        );
    }
}

/// Dans le salon réverbérant (queue diffuse, RT60 de 0,3 à 0,7 s), la même
/// promesse tient en phase minimale.
#[test]
fn le_fir_mesure_tient_aussi_dans_la_piece_reverberante() {
    let room = RoomSim::reverberant_living_room(FS);
    let (ir, m) = mesurer(&room);
    let cible = TargetCurve::flat();
    let fir = concevoir(&ir, &m, &cible, false);
    let e = ecart(
        &ir,
        &fir.coefficients,
        0,
        &cible,
        Some(fir.decalage_cible_db),
    );
    let e_sans = ecart(&ir, &[1.0], 0, &cible, None);
    eprintln!("T10b réverbérante : écart max {e:.3} dB avec FIR, {e_sans:.3} dB sans");
    let grille = LogResponse::grid(20.0, 20_000.0, 48);
    let r = lissage_variable(
        &reponse_corrigee(&ir, &fir.coefficients, 0, &grille),
        6,
        3,
        300.0,
    );
    for (f, m) in r.freqs.iter().zip(&r.magnitude_db) {
        let d = m - cible.level_db(*f) - fir.decalage_cible_db;
        if (BANDE.0..=BANDE.1).contains(f) && d.abs() > 1.0 {
            eprintln!("T10b réverbérante : {f:.1} Hz écart {d:+.2} dB");
        }
    }
    assert!(e < TOLERANCE_DB, "écart {e:.2} dB");
    assert!(e_sans > TOLERANCE_DB);
}

/// Passe-tout du second ordre (RBJ) : module plat, phase excédentaire pure.
fn passe_tout(f0: f64, q: f64) -> Biquad {
    let w0 = 2.0 * std::f64::consts::PI * f0 / FS as f64;
    let alpha = w0.sin() / (2.0 * q);
    let a0 = 1.0 + alpha;
    Biquad {
        b0: (1.0 - alpha) / a0,
        b1: -2.0 * w0.cos() / a0,
        b2: (1.0 + alpha) / a0,
        a1: -2.0 * w0.cos() / a0,
        a2: (1.0 - alpha) / a0,
    }
}

/// Retard de groupe moyen (ms) de `h` sur ±1/12 d'octave autour de `f`.
fn retard_de_groupe_ms(h: &[f64], f: f64) -> f64 {
    let nfft = fft::next_pow2(h.len() * 2).max(1 << 18);
    let x = fft::rfft(h, nfft);
    let df = FS as f64 / nfft as f64;
    let demi = 2f64.powf(1.0 / 12.0);
    let (a, b) = (((f / demi) / df) as usize, ((f * demi) / df) as usize);
    let mut s = 0.0;
    for k in a..b {
        s += -(x[k + 1] * x[k].conj()).arg() / (2.0 * std::f64::consts::PI * df);
    }
    s / (b - a) as f64 * 1000.0
}

/// Une pièce sans mode, dont l'enceinte porte le passe-tout d'un
/// raccordement de caisson à 80 Hz (Q 0,707 ; 5,6 ms de retard de groupe à
/// 80 Hz, 0,4 ms à 300 Hz) : la phase minimale n'y peut rien (le module est
/// plat) ; la phase mixte aplatit le retard de groupe sous 300 Hz. Le
/// pré-tintement est borné par construction (rien avant le rang 0, pic à la
/// latence annoncée) et son énergie est dite. Le module reste à ±1,5 dB.
/// Contre-épreuve : la phase minimale seule laisse l'écart de retard.
#[test]
fn la_phase_excedentaire_d_un_passe_tout_est_corrigee_dans_le_delai_annonce() {
    let mut room = RoomSim::dry_living_room(FS);
    room.modes.clear();
    let h = sim::filter_biquad(
        &passe_tout(80.0, std::f64::consts::FRAC_1_SQRT_2),
        &room.impulse_response(),
    );
    let mut samples = vec![0.0; 4_800];
    samples.extend(h);
    let ir = ImpulseResponse {
        sample_rate: FS,
        samples,
        peak_index: 4_800,
        delay_samples: 4_800,
    };
    let m = analysis::measured_response(&ir, 500.0, 12, &LogResponse::grid(20.0, 20_000.0, 48));
    let cible = TargetCurve::flat();
    let freqs = [
        30.0, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 300.0,
    ];
    let etalement = |fir: &FirCorrection| {
        let c = fft::convolve(&ir.samples, &fir.coefficients);
        let t: Vec<f64> = freqs.iter().map(|f| retard_de_groupe_ms(&c, *f)).collect();
        let max = t.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = t.iter().cloned().fold(f64::INFINITY, f64::min);
        (max - min, t)
    };
    let minimale = concevoir(&ir, &m, &cible, false);
    let mixte = concevoir(&ir, &m, &cible, true);
    let (e_min, t_min) = etalement(&minimale);
    let (e_mix, t_mix) = etalement(&mixte);
    eprintln!("T10b passe-tout : étalement du retard de groupe 30–300 Hz : phase minimale {e_min:.3} ms {t_min:.2?}, phase mixte {e_mix:.3} ms {t_mix:.2?}");
    assert!(e_min > 4.0, "contre-épreuve : phase minimale {e_min:.2} ms");
    assert!(e_mix < 1.0, "phase mixte : {e_mix:.2} ms");

    let p = OptionsPhase::default();
    let attendue = (p.pre_tintement_ms * FS as f64 / 1000.0).round() as usize;
    assert_eq!(mixte.latence_trames, attendue);
    assert_eq!(mixte.pic_trame, mixte.latence_trames);
    let pre = mixte.energie_pre_db.expect("énergie du pré-tintement");
    eprintln!(
        "T10b passe-tout : latence {} trames, énergie avant le pic {pre:.1} dB",
        mixte.latence_trames
    );
    // La borne est TEMPORELLE (rien avant le rang 0, soit 20 ms avant le
    // pic) ; l'énergie qui s'y trouve est mesurée (−16,8 dB pour ce
    // passe-tout de 5,6 ms) et seulement bornée grossièrement ici.
    assert!(pre < -12.0, "pré-tintement {pre:.1} dB");
    // Les 10 premières ms (fondu de la fenêtre) ne portent presque rien.
    let h = &mixte.coefficients;
    let total: f64 = h.iter().map(|v| v * v).sum();
    let tete: f64 = h[..mixte.latence_trames / 2].iter().map(|v| v * v).sum();
    let tete_db = 10.0 * (tete / total).log10();
    eprintln!("T10b passe-tout : énergie à plus de 10 ms avant le pic {tete_db:.1} dB");
    assert!(tete_db < -40.0, "{tete_db:.1} dB");
    let e = ecart(
        &ir,
        &mixte.coefficients,
        mixte.latence_trames,
        &cible,
        Some(mixte.decalage_cible_db),
    );
    assert!(e < TOLERANCE_DB, "module en phase mixte : {e:.2} dB");
}

/// La latence annoncée est exacte : le pic de la réponse corrigée arrive
/// `latence_trames` après celui de la mesure (EXACT, à la trame), et le
/// retard de groupe du filtre au-dessus de la bascule vaut
/// `latence_trames / cadence` à 0,1 ms près : la partie à phase minimale y
/// ajoute le sien, petit et variable avec la fréquence (−0,05 ms mesuré à
/// 2 kHz), qui n'est pas un retard.
#[test]
fn la_latence_annoncee_est_celle_du_pic_et_du_retard_de_groupe() {
    let room = RoomSim::dry_living_room(FS);
    let (ir, m) = mesurer(&room);
    for phase in [false, true] {
        let fir = concevoir(&ir, &m, &TargetCurve::flat(), phase);
        let c = fft::convolve(&ir.samples, &fir.coefficients);
        let decale = fft::argmax_abs(&c) as isize - fft::argmax_abs(&ir.samples) as isize;
        assert_eq!(decale, fir.latence_trames as isize, "phase mixte {phase}");
        let attendu = fir.latence_ms();
        for f in [2_000.0, 4_000.0, 8_000.0] {
            let tau = retard_de_groupe_ms(&fir.coefficients, f);
            eprintln!("T10b latence phase_mixte={phase} : {f} Hz retard de groupe {tau:.4} ms, annoncé {attendu:.4} ms");
            assert!(
                (tau - attendu).abs() < 0.1,
                "{f} Hz : {tau:.3} ms ≠ {attendu:.3} ms"
            );
        }
    }
}

/// Le lecteur du convolueur (`tune-core/src/audio/convolver.rs`,
/// `read_wav_ir`), reproduit pas à pas : parcours des blocs RIFF à leur vrai
/// rang, `fmt ` d'au moins 16 octets, format étendu seulement à partir de 40,
/// `data` borné à ce qui est présent, échantillons entrelacés par canal.
fn lire_comme_le_convolueur(data: &[u8]) -> Result<(Vec<Vec<f64>>, u32), String> {
    if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err("not a RIFF/WAVE file".into());
    }
    let (mut format_tag, mut channels, mut sample_rate, mut bits) = (1u16, 0usize, 0u32, 0usize);
    let mut data_range = None;
    let mut pos = 12usize;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let declared = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = pos + 8;
        let size = declared.min(data.len() - body);
        match id {
            b"fmt " if size >= 16 => {
                format_tag = u16::from_le_bytes([data[body], data[body + 1]]);
                channels = u16::from_le_bytes([data[body + 2], data[body + 3]]) as usize;
                sample_rate = u32::from_le_bytes(data[body + 4..body + 8].try_into().unwrap());
                bits = u16::from_le_bytes([data[body + 14], data[body + 15]]) as usize;
                if format_tag == 0xFFFE && size >= 40 {
                    format_tag = u16::from_le_bytes([data[body + 24], data[body + 25]]);
                }
            }
            b"data" => data_range = Some((body, body + size)),
            _ => {}
        }
        pos = body + size + (size & 1);
    }
    let (dstart, dend) = data_range.ok_or("missing data chunk")?;
    if channels == 0 {
        return Err("missing or invalid fmt chunk".into());
    }
    let bps = bits / 8;
    let is_float = format_tag == 3;
    let spc = (dend - dstart) / bps / channels;
    let mut ir = vec![Vec::with_capacity(spc); channels];
    for i in 0..spc {
        for (ch, v) in ir.iter_mut().enumerate() {
            let o = dstart + (i * channels + ch) * bps;
            v.push(match (bits, is_float) {
                (32, true) => f32::from_le_bytes(data[o..o + 4].try_into().unwrap()) as f64,
                (64, true) => f64::from_le_bytes(data[o..o + 8].try_into().unwrap()),
                _ => return Err(format!("unsupported {bits}-bit float={is_float}")),
            });
        }
    }
    Ok((ir, sample_rate))
}

/// Aller-retour du fichier : le FIR mesuré, mono puis un canal par enceinte,
/// en 64 bits (écart NUL) et en 32 bits (l'arrondi `f32`, rien de plus).
#[test]
fn le_wav_du_fir_se_relit_comme_le_convolueur_le_lit() {
    let room = RoomSim::dry_living_room(FS);
    let (ir, m) = mesurer(&room);
    let fir = concevoir(&ir, &m, &TargetCurve::flat(), true);
    let h = fir.coefficients.clone();
    let droite: Vec<f64> = h.iter().map(|v| v * 0.5).collect();

    let octets = ecrire_wav(FS, &[h.clone()], FormatEchantillon::Flottant64).unwrap();
    let (lu, sr) = lire_comme_le_convolueur(&octets).unwrap();
    assert_eq!(sr, FS);
    assert_eq!(lu.len(), 1);
    assert_eq!(lu[0], h, "64 bits : écart non nul");

    let octets = ecrire_wav(
        FS,
        &[h.clone(), droite.clone()],
        FormatEchantillon::Flottant64,
    )
    .unwrap();
    let (lu, _) = lire_comme_le_convolueur(&octets).unwrap();
    assert_eq!(lu, vec![h.clone(), droite.clone()]);

    let octets = ecrire_wav(
        FS,
        &[h.clone(), droite.clone()],
        FormatEchantillon::Flottant32,
    )
    .unwrap();
    let (lu, _) = lire_comme_le_convolueur(&octets).unwrap();
    let mut pire = 0.0f64;
    for (canal, attendu) in lu.iter().zip([&h, &droite]) {
        assert_eq!(canal.len(), attendu.len());
        for (a, b) in canal.iter().zip(attendu.iter()) {
            assert_eq!(*a, *b as f32 as f64);
            pire = pire.max((a - b).abs());
        }
    }
    eprintln!("T10b WAV : 64 bits écart 0 ; 32 bits écart max {pire:.3e}");
    assert!(pire < 1e-7);
}
