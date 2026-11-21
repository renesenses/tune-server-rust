//! C4 (#7632, lot TESS T10a) : la mesure à plusieurs positions, sur pièces
//! simulées. Chaque position est mesurée par la chaîne complète (balayage,
//! capture simulée, déconvolution, analyse) ; aucun son n'est joué ni capté.
//!
//! Le « vrai » comportement de la zone d'écoute est la moyenne en puissance
//! des réponses vraies (IR synthétiques) sur 41 places réparties de −1 à +1.

use tune_calibration::analysis::{self, LogResponse};
use tune_calibration::deconv;
use tune_calibration::positions::{
    self, ecart_rms_db, ecart_type_creux_db, ecart_type_db, moyenne_positions, ConfigPositions,
    Moyenne,
};
use tune_calibration::sim::{RoomSim, POSITIONS_SIMULEES};
use tune_calibration::sweep;
use tune_calibration::{analyse_capture, MeasurementSpec};

const FS: u32 = 48_000;
/// La bande où la pièce fait la loi (sous Schroeder) et où la correction agit.
const F_LO: f64 = 30.0;
const F_HI: f64 = 300.0;

fn spec() -> MeasurementSpec {
    let mut s = MeasurementSpec::room_default(FS);
    s.sweep.duration_s = 3.0;
    s
}

fn mesurer(room: &RoomSim, spec: &MeasurementSpec) -> LogResponse {
    let sw = sweep::generate(&spec.sweep);
    analyse_capture(&room.capture(&sw.signal, 1.0), &sw, spec, None).response
}

fn reponse_vraie(room: &RoomSim, spec: &MeasurementSpec) -> LogResponse {
    let ir = deconv::ImpulseResponse {
        sample_rate: FS,
        samples: room.impulse_response(),
        peak_index: 0,
        delay_samples: 0,
    };
    analysis::measured_response(&ir, spec.window_post_ms, spec.smoothing_n, &spec.grid())
}

fn zone_d_ecoute(base: &RoomSim, spec: &MeasurementSpec) -> LogResponse {
    let toutes: Vec<LogResponse> = (0..41)
        .map(|i| reponse_vraie(&base.a_la_position(-1.0 + i as f64 * 0.05), spec))
        .collect();
    positions::moyenne_ponderee(
        &toutes.iter().collect::<Vec<_>>(),
        &[1.0; 41],
        20,
        Moyenne::Puissance,
    )
}

/// La preuve demandée : 5 positions moyennées RÉDUISENT l'écart-type des
/// creux par rapport à la seule place d'écoute, et collent mieux à la zone
/// d'écoute réelle. Les chiffres sont imprimés (`--nocapture`).
#[test]
fn cinq_positions_reduisent_l_ecart_type_des_creux() {
    let spec = spec();
    let base = RoomSim::dry_living_room(FS);
    let mesures: Vec<LogResponse> = POSITIONS_SIMULEES[..5]
        .iter()
        .map(|u| mesurer(&base.a_la_position(*u), &spec))
        .collect();
    let une = &mesures[0];
    let m = moyenne_positions(&mesures, 0, &ConfigPositions::default()).unwrap();
    assert_eq!(
        m.retenues(),
        5,
        "aucune position saine rejetée : {:?}",
        m.verdicts
    );
    let cinq = &m.reponse;
    let vraie = zone_d_ecoute(&base, &spec);

    let creux_1 = ecart_type_creux_db(une, F_LO, F_HI);
    let creux_5 = ecart_type_creux_db(cinq, F_LO, F_HI);
    let et_1 = ecart_type_db(une, F_LO, F_HI);
    let et_5 = ecart_type_db(cinq, F_LO, F_HI);
    let max_1 = positions::creux_max_db(une, F_LO, F_HI);
    let max_5 = positions::creux_max_db(cinq, F_LO, F_HI);
    let err_1 = ecart_rms_db(une, &vraie, F_LO, F_HI);
    let err_5 = ecart_rms_db(cinq, &vraie, F_LO, F_HI);
    let creux_par_position: Vec<String> = mesures
        .iter()
        .map(|r| format!("{:.2}", ecart_type_creux_db(r, F_LO, F_HI)))
        .collect();
    println!(
        "T10a {F_LO}-{F_HI} Hz | écart-type des creux : 1 position {creux_1:.2} dB, 5 positions {creux_5:.2} dB (par position : {creux_par_position:?}) | \
         écart-type : {et_1:.2} → {et_5:.2} dB | creux max : {max_1:.2} → {max_5:.2} dB | \
         écart à la zone d'écoute (41 places) : {err_1:.2} → {err_5:.2} dB"
    );
    assert!(
        creux_5 < 0.7 * creux_1,
        "écart-type des creux : {creux_1:.2} dB en un point, {creux_5:.2} dB sur 5 positions"
    );
    assert!(et_5 < et_1, "écart-type {et_1:.2} → {et_5:.2} dB");
    assert!(max_5 < max_1, "creux max {max_1:.2} → {max_5:.2} dB");
    assert!(
        err_5 < 0.7 * err_1,
        "écart à la zone d'écoute : {err_1:.2} dB en un point, {err_5:.2} dB sur 5 positions"
    );
}

/// Contre-épreuve : 5 mesures à la MÊME place donnent exactement la courbe
/// d'une seule — la moyenne ne fabrique rien.
#[test]
fn cinq_positions_identiques_donnent_la_meme_courbe_qu_une_seule() {
    let spec = spec();
    let une = mesurer(&RoomSim::dry_living_room(FS).a_la_position(0.0), &spec);
    for moyenne in [Moyenne::Puissance, Moyenne::Db] {
        let cfg = ConfigPositions {
            moyenne,
            ..Default::default()
        };
        let m = moyenne_positions(&vec![une.clone(); 5], 0, &cfg).unwrap();
        assert_eq!(m.reponse, une, "{moyenne:?}");
    }
}

/// Une position aberrante (micro couvert d'un coussin : −14 dB et aigus
/// étouffés, ou capture d'une autre pièce) est rejetée ; les autres sont
/// gardées et la moyenne reste celle des quatre saines.
#[test]
fn une_mesure_aberrante_est_rejetee() {
    let spec = spec();
    let base = RoomSim::dry_living_room(FS);
    let mut mesures: Vec<LogResponse> = POSITIONS_SIMULEES[..4]
        .iter()
        .map(|u| mesurer(&base.a_la_position(*u), &spec))
        .collect();
    let mut couvert = base.a_la_position(0.3);
    couvert.direct_gain *= 0.2;
    mesures.insert(2, mesurer(&couvert, &spec));
    let m = moyenne_positions(&mesures, 0, &ConfigPositions::default()).unwrap();
    let rejetees: Vec<usize> = m
        .verdicts
        .iter()
        .filter(|v| v.rejetee)
        .map(|v| v.index)
        .collect();
    println!("T10a rejet : {:?}", m.verdicts);
    assert_eq!(rejetees, vec![2], "{:?}", m.verdicts);
    let saines: Vec<LogResponse> = mesures
        .iter()
        .enumerate()
        .filter(|(k, _)| *k != 2)
        .map(|(_, r)| r.clone())
        .collect();
    let attendue = moyenne_positions(&saines, 0, &ConfigPositions::default()).unwrap();
    assert_eq!(m.reponse, attendue.reponse);
}
