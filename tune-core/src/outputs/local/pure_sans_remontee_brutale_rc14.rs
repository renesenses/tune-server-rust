//! rc14 (sécurité auditive, décision de Bertrand du 25/11) — entrer en PURE
//! ou en « Bypass all » ne fait plus sauter le niveau d'un bloc au signal sec
//! (toute la marge d'une série d'égaliseurs, ~+24 dB). L'égaliseur s'efface
//! par un fondu et le niveau rejoint celui d'avant l'égaliseur en ~0,5 s,
//! jamais au-delà ; à volume inchangé, PURE redevient bit-perfect ; volume
//! monté, le surplus reste gardé (voyant, « Rendre le niveau », #6524) et le
//! chemin ne dit pas « bit-perfect ». Le DoP n'est jamais touché.
use super::*;
use crate::audio::eq::{EqBandSpec, EqProcessor, EqProfile};
use crate::tess::egaliseurs_en_serie::combiner_en_serie;

const TAUX: u32 = 48_000;
const BLOC: usize = 480; // 10 ms
const AMPLITUDE: f32 = 0.05;

fn serie() -> EqProcessor {
    let bande = |freq: f64, gain: f64, q: f64, t: &str| EqBandSpec {
        freq,
        gain,
        q,
        band_type: t.into(),
        ..Default::default()
    };
    let a = EqProfile {
        enabled: true,
        bands: vec![bande(40.0, 12.0, 1.0, "peak")],
        ..Default::default()
    };
    let b = EqProfile {
        enabled: true,
        bands: vec![bande(60.0, 12.0, 0.707, "low_shelf")],
        ..Default::default()
    };
    EqProcessor::new(&combiner_en_serie(&[a, b]).unwrap(), TAUX, 2)
}

struct Banc {
    sortie: LocalOutput,
    n: usize,
    freq: f64,
    gains_db: Vec<f64>,
    gauche: Vec<f32>,
    /// Blocs sortis identiques à l'entrée, au bit près.
    identiques: Vec<bool>,
}

impl Banc {
    /// Une sortie locale qui joue la série au volume 500 ‰.
    fn new() -> Self {
        let sortie = LocalOutput::with_options("Banc".into(), false, "wasapi");
        sortie.user_volume.store(500, Ordering::SeqCst);
        sortie.set_eq(Some(serie()));
        Self {
            sortie,
            n: 0,
            freq: 5000.0,
            gains_db: Vec::new(),
            gauche: Vec::new(),
            identiques: Vec::new(),
        }
    }

    fn jouer(&mut self, blocs: usize, dop: bool) {
        for _ in 0..blocs {
            let mut pcm = Vec::with_capacity(BLOC * 2);
            for i in 0..BLOC {
                let t = (self.n + i) as f64 / f64::from(TAUX);
                let s = AMPLITUDE * (2.0 * std::f64::consts::PI * self.freq * t).sin() as f32;
                pcm.push(s);
                pcm.push(s);
            }
            self.n += BLOC;
            let entree = pcm.clone();
            let s = &self.sortie;
            apply_local_dsp(
                &mut pcm,
                &s.eq,
                &s.convolver,
                &s.crossfeed,
                &s.pure_bypass,
                &s.mono_downmix,
                2,
                dop,
            );
            let e: f64 = entree.iter().map(|x| f64::from(*x).powi(2)).sum();
            let o: f64 = pcm.iter().map(|x| f64::from(*x).powi(2)).sum();
            self.gains_db.push(10.0 * (o / e).log10());
            self.identiques.push(
                pcm.iter()
                    .zip(&entree)
                    .all(|(a, b)| a.to_bits() == b.to_bits()),
            );
            self.gauche.extend(pcm.iter().step_by(2));
        }
    }

    fn max_db_depuis(&self, depuis: usize) -> f64 {
        self.gains_db[depuis..]
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max)
    }

    /// Plus grand écart entre échantillons voisins depuis le bloc `depuis`.
    fn ecart_max_depuis(&self, depuis: usize) -> f32 {
        self.gauche[depuis * BLOC..]
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max)
    }

    /// La pente maximale d'une sinusoïde de 5 kHz au gain `db`.
    fn pente_db(&self, db: f64) -> f64 {
        f64::from(AMPLITUDE) * 10f64.powf(db / 20.0) * 2.0 * std::f64::consts::PI * self.freq
            / f64::from(TAUX)
    }

    fn etat(&self, dop: bool) -> OutputDspState {
        let s = &self.sortie;
        local_dsp_runtime_state(
            &s.eq,
            &s.convolver,
            &s.crossfeed,
            &s.pure_bypass,
            &s.mono_downmix,
            dop,
        )
    }
}

/// Entrée en PURE, volume inchangé : aucun saut, puis bit-perfect au plus
/// tard à 0,6 s, et le chemin le dit alors.
#[test]
fn entree_en_pure_volume_inchange_bit_perfect_a_0_6_s_sans_saut() {
    let mut banc = Banc::new();
    banc.jouer(50, false);
    let avant = *banc.gains_db.last().unwrap();
    assert!(avant < -8.0, "la série joue à {avant:.2} dB");
    banc.sortie.set_pure_bypass(true);
    // Comme `refresh_zone_pure_dsp` : la chaîne sous PURE reçoit `None`.
    banc.sortie.replace_eq_live(None);
    let entree = banc.gains_db.len();
    banc.jouer(60, false);
    // Pas de saut : 10 ms ne montent jamais de plus de 1 dB.
    let pas = banc.gains_db[entree - 1..]
        .windows(2)
        .map(|w| w[1] - w[0])
        .fold(0.0_f64, f64::max);
    assert!(
        pas < 1.0,
        "marche de {pas:.2} dB en 10 ms à l'entrée en PURE"
    );
    assert!(
        banc.identiques[entree + 59],
        "à 0,6 s, PURE doit rendre l'entrée au bit près"
    );
    assert!(banc.sortie.eq.lock().unwrap().is_none());
    assert!(banc.sortie.preampli_garde().is_none());
    assert!(!matches!(banc.etat(false), OutputDspState::Applied));
    // Jamais au-dessus du signal sec (le niveau d'avant l'égaliseur).
    assert!(banc.max_db_depuis(entree) <= 0.01);
}

/// Volume MONTÉ pendant que la série jouait : l'entrée en PURE ne dépasse
/// jamais le niveau d'avant l'égaliseur ; le surplus reste gardé, se voit,
/// et le chemin ne dit pas « bit-perfect » avant « Rendre le niveau ».
#[test]
fn entree_en_pure_volume_monte_jamais_au_dessus_de_la_borne() {
    let mut banc = Banc::new();
    banc.jouer(50, false);
    // +6 dB de volume : la borne vaut 500 / 1000, soit −6,02 dB.
    banc.sortie.user_volume.store(1000, Ordering::SeqCst);
    banc.sortie.reborner_le_preampli_garde();
    let borne_db = 20.0 * 0.5_f64.log10();
    banc.sortie.set_pure_bypass(true);
    banc.sortie.replace_eq_live(None);
    let entree = banc.gains_db.len();
    banc.jouer(200, false);
    let pire = banc.max_db_depuis(entree);
    assert!(
        pire <= borne_db + 0.1,
        "PURE, volume monté : {pire:.2} dB, au-dessus de la borne {borne_db:.2} dB"
    );
    let fin = *banc.gains_db.last().unwrap();
    assert!((fin - borne_db).abs() < 0.1, "gardé à {fin:.2} dB");
    let garde = banc
        .sortie
        .preampli_garde()
        .expect("le surplus gardé se voit sous PURE");
    assert!(garde.db < -5.0);
    assert!(
        matches!(banc.etat(false), OutputDspState::Applied),
        "niveau réduit : le chemin ne doit pas dire bit-perfect"
    );
    assert!(!banc.identiques.last().unwrap());
    // « Rendre le niveau » : retour progressif, puis bit-perfect strict.
    banc.sortie.liberer_le_preampli_garde();
    let geste = banc.gains_db.len();
    banc.jouer(500, false);
    let pas = banc.gains_db[geste - 1..]
        .windows(2)
        .map(|w| w[1] - w[0])
        .fold(0.0_f64, f64::max);
    assert!(
        pas < 1.0,
        "marche de {pas:.2} dB après « Rendre le niveau »"
    );
    assert!(
        *banc.identiques.last().unwrap(),
        "bit-perfect après le geste"
    );
    assert!(banc.sortie.eq.lock().unwrap().is_none());
}

/// Pas de clic à l'entrée en PURE : l'écart entre échantillons voisins ne
/// dépasse jamais la pente de la sinusoïde au niveau le plus haut atteint.
#[test]
fn entree_en_pure_sans_clic() {
    // 200 Hz : la pente de la sinusoïde est faible, une marche se verrait.
    let mut banc = Banc::new();
    banc.freq = 200.0;
    banc.jouer(50, false);
    banc.sortie.set_pure_bypass(true);
    banc.sortie.replace_eq_live(None);
    let entree = banc.gains_db.len();
    banc.jouer(80, false);
    let haut = banc.max_db_depuis(entree - 1);
    let ecart = banc.ecart_max_depuis(entree - 1);
    assert!(
        f64::from(ecart) <= banc.pente_db(haut) * 1.05,
        "écart {ecart:.6} > pente {:.6}",
        banc.pente_db(haut)
    );
}

/// Sortie de PURE : la série revient par un fondu, sans clic ni marche, à
/// sa marge (le niveau ne monte pas).
#[test]
fn sortie_de_pure_la_serie_revient_par_un_fondu() {
    let mut banc = Banc::new();
    banc.jouer(50, false);
    let avec_serie = *banc.gains_db.last().unwrap();
    banc.sortie.set_pure_bypass(true);
    banc.sortie.replace_eq_live(None);
    banc.jouer(100, false);
    assert!(*banc.identiques.last().unwrap());
    banc.sortie.set_pure_bypass(false);
    banc.sortie.replace_eq_live(Some(serie()));
    let sortie = banc.gains_db.len();
    banc.jouer(100, false);
    let descente = banc.gains_db[sortie - 1..]
        .windows(2)
        .map(|w| w[0] - w[1])
        .fold(0.0_f64, f64::max);
    assert!(descente < 6.0, "marche de {descente:.2} dB en 10 ms");
    assert!(banc.max_db_depuis(sortie) <= 0.01, "le niveau ne monte pas");
    let fin = *banc.gains_db.last().unwrap();
    assert!(
        (fin - avec_serie).abs() < 0.1,
        "{fin:.2} contre {avec_serie:.2}"
    );
    let ecart = banc.ecart_max_depuis(sortie - 1);
    assert!(
        f64::from(ecart) <= banc.pente_db(0.0) * 1.05,
        "clic : {ecart:.6}"
    );
}

/// DoP : jamais touché, transition ou non.
#[test]
fn le_dop_reste_intact_pendant_la_transition() {
    let mut banc = Banc::new();
    banc.jouer(10, true);
    banc.sortie.set_pure_bypass(true);
    banc.sortie.replace_eq_live(None);
    banc.jouer(30, true);
    assert!(
        banc.identiques.iter().all(|x| *x),
        "un bloc DoP a été modifié"
    );
    assert!(matches!(banc.etat(true), OutputDspState::BypassedDop));
}
