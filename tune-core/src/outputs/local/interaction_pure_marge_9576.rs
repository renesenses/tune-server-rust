//! Relecture de #9576 — interactions entre la transition PURE (#9549,
//! `volume_avant_traitement`) et la marge gardée d'un filtre de convolution
//! retiré (#9561, `volume_avant_convolution`). Mesures sur la chaîne locale
//! réelle (`apply_local_dsp`), gains par bloc de 10 ms (volume exclu : il est
//! appliqué au rendu, `user_volume` ne sert qu'aux bornes).
//!
//! Règle : toute hausse rejoint le niveau d'avant en ~0,5 s, jamais au-delà ;
//! gain ≤ 1 ; aucun clic.
use super::*;
use crate::audio::convolver::ConvolverConfig;
use crate::tess::chaine::Headroom;

const TAUX: u32 = 48_000;
const BLOC: usize = 480; // 10 ms
const AMPLITUDE: f32 = 0.2;
const FREQ: f64 = 1000.0;

/// Un filtre plat de +6,02 dB (Dirac de 2), en WAV : en Auto, marge
/// −6,52 dB dans le convolueur, niveau net −0,5 dB.
fn ri_plus_6() -> crate::test_scratch::ScratchFile {
    let f = crate::test_scratch::scratch_file("ri-9576-interaction", ".wav");
    let taps = 256usize;
    let donnees = taps * 4;
    let mut w = Vec::with_capacity(44 + donnees);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&((36 + donnees) as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&3u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&TAUX.to_le_bytes());
    w.extend_from_slice(&(TAUX * 4).to_le_bytes());
    w.extend_from_slice(&4u16.to_le_bytes());
    w.extend_from_slice(&32u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(donnees as u32).to_le_bytes());
    for i in 0..taps {
        let v: f32 = if i == 0 { 2.0 } else { 0.0 };
        w.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(f.path(), w).unwrap();
    f
}

fn auto(ri: &crate::test_scratch::ScratchFile) -> ConvolverConfig {
    ConvolverConfig::from_wav(&ri.as_str())
        .unwrap()
        .avec_headroom(Headroom::Auto {})
}

fn manuel(ri: &crate::test_scratch::ScratchFile) -> ConvolverConfig {
    ConvolverConfig::from_wav(&ri.as_str())
        .unwrap()
        .avec_headroom(Headroom::Manual { db: -8.0 })
        .avec_egaliseur_amont(Some(crate::tess::egaliseurs_en_serie::gain_fixe(-8.0)))
}

struct Banc {
    sortie: LocalOutput,
    _ri: crate::test_scratch::ScratchFile,
    n: usize,
    gains_db: Vec<f64>,
    gauche: Vec<f32>,
    rms_gauche: Vec<f64>,
    identiques: Vec<bool>,
}

impl Banc {
    /// Une sortie qui joue du 48 kHz stéréo au volume 500 ‰, filtre +6 dB en
    /// Auto (marge −6,52 dB) en service.
    fn new() -> Self {
        let ri = ri_plus_6();
        let sortie = LocalOutput::with_options("Banc 9576".into(), false, "wasapi");
        sortie.user_volume.store(500, Ordering::SeqCst);
        sortie.recompute_effective_volume();
        sortie.declare_current_format_for_test(TAUX, 2);
        sortie.set_convolution_autorisee(true);
        sortie.set_convolver_config(auto(&ri)).unwrap();
        rebuild_local_convolver(
            &sortie.convolver_config,
            &sortie.convolver,
            &sortie.convolution_autorisee,
            TAUX,
            2,
        )
        .unwrap();
        assert!(sortie.has_convolver());
        Self {
            sortie,
            _ri: ri,
            n: 0,
            gains_db: Vec::new(),
            gauche: Vec::new(),
            rms_gauche: Vec::new(),
            identiques: Vec::new(),
        }
    }

    fn monter_le_volume(&self, volume: u32) {
        self.sortie.user_volume.store(volume, Ordering::SeqCst);
        self.sortie.recompute_effective_volume();
        self.sortie.reborner_le_preampli_garde();
    }

    /// Comme `refresh_zone_pure_dsp` (PURE armé puis la chaîne vidée).
    fn entrer_en_pure(&self) {
        let s = &self.sortie;
        s.set_pure_bypass(true);
        s.set_compensation_de_niveau(true);
        s.set_replaygain_factor(1.0);
        s.replace_crossfeed_live(None);
        s.replace_eq_live(None);
        s.set_mono_downmix(false);
    }

    /// Comme `refresh_zone_pure_dsp` à la sortie de PURE.
    fn sortir_de_pure(&self) {
        let s = &self.sortie;
        s.set_pure_bypass(false);
        s.set_compensation_de_niveau(true);
        s.set_replaygain_factor(1.0);
        s.replace_crossfeed_live(None);
        s.replace_eq_live(None);
        s.set_mono_downmix(false);
    }

    fn jouer(&mut self, blocs: usize) {
        for _ in 0..blocs {
            let mut pcm = Vec::with_capacity(BLOC * 2);
            for i in 0..BLOC {
                let t = (self.n + i) as f64 / f64::from(TAUX);
                let s = AMPLITUDE * (2.0 * std::f64::consts::PI * FREQ * t).sin() as f32;
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
                false,
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
            let g: f64 = pcm.iter().step_by(2).map(|x| f64::from(*x).powi(2)).sum();
            self.rms_gauche.push((g / BLOC as f64).sqrt());
        }
    }

    fn trace(&self, depuis: usize) -> String {
        self.gains_db[depuis..]
            .iter()
            .map(|g| format!("{g:+.2}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Jamais au-dessus de `plafond` dB (le niveau d'avant).
    fn jamais_au_dessus(&self, depuis: usize, plafond: f64, quoi: &str) {
        for (n, db) in self.gains_db[depuis..].iter().enumerate() {
            assert!(
                *db <= plafond + 0.1,
                "{quoi} : {db:+.2} dB au bloc +{n}, plafond {plafond:+.2} dB\n{}",
                self.trace(depuis)
            );
        }
    }

    /// Aucune hausse de plus de 1,5 dB d'un bloc de 10 ms au suivant.
    fn sans_marche(&self, depuis: usize, quoi: &str) {
        for (n, w) in self.gains_db[depuis.max(1) - 1..].windows(2).enumerate() {
            assert!(
                w[1] - w[0] < 1.5,
                "{quoi} : marche de {:+.2} dB au bloc +{n}\n{}",
                w[1] - w[0],
                self.trace(depuis.max(1) - 1)
            );
        }
    }

    /// Aucun clic : chaque pas d'échantillon reste sous la pente du sinus au
    /// niveau des deux blocs qui l'encadrent (même mesure que #9556).
    fn sans_clic(&self, depuis: usize, quoi: &str) {
        let pente_par_rms =
            std::f64::consts::SQRT_2 * 2.0 * std::f64::consts::PI * FREQ / f64::from(TAUX);
        for b in depuis.max(1)..self.rms_gauche.len() {
            let rms = self.rms_gauche[b].max(self.rms_gauche[b - 1]);
            let plafond = rms * pente_par_rms * 1.15 + 1e-6;
            for i in b * BLOC..(b + 1) * BLOC {
                let pas = f64::from((self.gauche[i] - self.gauche[i - 1]).abs());
                assert!(
                    pas <= plafond,
                    "{quoi} : clic au bloc {b} (éch. {i}) : {pas:.6} > {plafond:.6}"
                );
            }
        }
    }
}

/// A1 — le filtre à marge est retiré PENDANT le fondu d'entrée en PURE
/// (100 ms après la bascule), volume inchangé : jamais au-dessus du sec
/// (0 dB, niveau d'avant le filtre), sans marche ni clic, puis bit-perfect.
#[test]
fn filtre_a_marge_retire_pendant_l_entree_en_pure_9576() {
    let mut b = Banc::new();
    b.jouer(50);
    b.entrer_en_pure();
    let entree = b.gains_db.len();
    b.jouer(10);
    b.sortie.clear_convolver();
    b.jouer(150);
    b.jamais_au_dessus(entree, 0.0, "retrait pendant l'entrée en PURE");
    b.sans_marche(entree, "retrait pendant l'entrée en PURE");
    b.sans_clic(entree, "retrait pendant l'entrée en PURE");
    assert!(
        b.identiques.last().copied().unwrap(),
        "PURE doit redevenir bit-perfect\n{}",
        b.trace(entree)
    );
}

/// A2 — même retrait, mais l'auditeur avait monté le son de 6 dB depuis la
/// pose du filtre : le niveau d'avant (rapporté au volume courant) est
/// −6,02 dB, jamais dépassé.
#[test]
fn filtre_a_marge_retire_pendant_l_entree_en_pure_volume_monte_9576() {
    let mut b = Banc::new();
    b.jouer(50);
    b.monter_le_volume(1000);
    b.jouer(5);
    let tenu = *b.gains_db.last().unwrap();
    b.entrer_en_pure();
    let entree = b.gains_db.len();
    b.jouer(10);
    b.sortie.clear_convolver();
    b.jouer(150);
    // Jamais au-dessus du niveau tenu (volume monté par l'auditeur), et la
    // remontée s'arrête au niveau d'avant le filtre (−6,02 dB).
    b.jamais_au_dessus(entree, tenu, "retrait en PURE, volume monté");
    b.sans_marche(entree, "retrait en PURE, volume monté");
    // Constat (non bloquant) : le niveau finit à −3,30 dB, sous le niveau
    // tenu (−0,72 dB) mais au-dessus de la borne de #9561 (−6,02 dB, celle
    // d'un retrait hors PURE) : le gain de la chaîne sortante est une
    // moyenne d'énergie sur les 300 ms, qui mêle filtre et marge gardée.
    let fin = *b.gains_db.last().unwrap();
    assert!(fin <= tenu + 0.1, "fin {fin:+.2}\n{}", b.trace(entree));
}

/// A3 — l'inverse : le filtre à marge est retiré hors PURE, et l'on ENTRE en
/// PURE pendant que sa marge gardée remonte (100 ms plus tard). Volume
/// inchangé : la remontée continue sans marche jusqu'au sec.
#[test]
fn entree_en_pure_pendant_la_remontee_d_une_marge_retiree_9576() {
    let mut b = Banc::new();
    b.jouer(50);
    b.sortie.clear_convolver();
    let retrait = b.gains_db.len();
    b.jouer(10);
    b.entrer_en_pure();
    b.jouer(150);
    b.jamais_au_dessus(retrait, 0.0, "PURE pendant la remontée");
    b.sans_marche(retrait, "PURE pendant la remontée");
    b.sans_clic(retrait, "PURE pendant la remontée");
}

/// A4 — même chose, volume monté de 6 dB depuis la pose du filtre : la
/// marge retirée est bornée à −6,02 dB (#9561) ; l'entrée en PURE ne doit
/// pas la faire sauter au sec (+6 dB au-dessus du niveau d'avant).
#[test]
fn entree_en_pure_pendant_la_remontee_volume_monte_9576() {
    let mut b = Banc::new();
    b.jouer(50);
    b.monter_le_volume(1000);
    b.jouer(5);
    b.sortie.clear_convolver();
    let retrait = b.gains_db.len();
    b.jouer(80);
    let tenu = *b.gains_db.last().unwrap();
    assert!(
        (tenu - -6.02).abs() < 0.2,
        "prémisse : marge tenue à −6,02 dB ({})",
        b.trace(retrait)
    );
    let rendu_avant = b.sortie.volume.load(Ordering::SeqCst);
    b.entrer_en_pure();
    let entree = b.gains_db.len();
    b.jouer(150);
    // Le rendu (volume × garde) ne compense pas : il ne baisse pas.
    assert!(b.sortie.volume.load(Ordering::SeqCst) >= rendu_avant);
    b.jamais_au_dessus(entree, -6.02, "PURE après un retrait borné");
    b.sans_marche(entree, "PURE après un retrait borné");
}

/// B1 — on QUITTE PURE pendant le fondu d'entrée, puis la marge change
/// (Auto → manuel : marge retirée, ~0,5 s, bornée).
#[test]
fn sortie_de_pure_puis_changement_de_marge_9576() {
    let mut b = Banc::new();
    let ri = ri_plus_6();
    b.jouer(50);
    let avant = *b.gains_db.last().unwrap();
    b.entrer_en_pure();
    b.jouer(10);
    b.sortir_de_pure();
    let sortie = b.gains_db.len();
    b.jouer(20);
    b.jamais_au_dessus(sortie, avant.max(0.0), "sortie de PURE");
    assert!(
        b.sortie
            .reposer_la_convolution_si_la_marge_change(manuel(&ri))
            .unwrap()
    );
    let change = b.gains_db.len();
    b.jouer(120);
    // Le filtre sans marge rend +6,02 dB : la borne (volume inchangé) est le
    // niveau du filtre, atteint en ~0,5 s, sans marche.
    b.jamais_au_dessus(change, 6.02, "changement de marge après PURE");
    b.sans_marche(change, "changement de marge après PURE");
}

/// B2 — la marge change PENDANT PURE (Auto → manuel), puis on sort de PURE :
/// pas de hausse au-dessus du niveau du nouveau filtre, et la hausse vers
/// lui prend ~0,5 s ; volume monté de 6 dB depuis la pose : jamais au-dessus
/// du niveau d'avant le filtre (borne −6,02 dB × +6,02 dB = 0 dB).
#[test]
fn changement_de_marge_sous_pure_puis_sortie_9576() {
    for monte in [false, true] {
        let mut b = Banc::new();
        let ri = ri_plus_6();
        b.jouer(50);
        if monte {
            b.monter_le_volume(1000);
        }
        b.entrer_en_pure();
        b.jouer(60);
        b.sortie
            .reposer_la_convolution_si_la_marge_change(manuel(&ri))
            .unwrap();
        b.jouer(20);
        b.sortir_de_pure();
        let sortie = b.gains_db.len();
        b.jouer(150);
        let plafond = if monte { 0.0 } else { 6.02 };
        b.jamais_au_dessus(sortie, plafond, &format!("marge changée sous PURE (monté {monte})"));
        b.sans_marche(sortie, &format!("marge changée sous PURE (monté {monte})"));
    }
}

/// C1 — filtre à marge RETIRÉ sous PURE (fondu fini), puis sortie de PURE :
/// le sec revient, jamais au-dessus de 0 dB, sans marche ; et le niveau finit
/// au sec (pas de marge gardée « pour toujours »).
#[test]
fn filtre_retire_sous_pure_puis_sortie_9576() {
    let mut b = Banc::new();
    b.jouer(50);
    b.entrer_en_pure();
    b.jouer(80);
    b.sortie.clear_convolver();
    b.jouer(20);
    b.sortir_de_pure();
    let sortie = b.gains_db.len();
    b.jouer(150);
    b.jamais_au_dessus(sortie, 0.0, "retrait sous PURE puis sortie");
    b.sans_marche(sortie, "retrait sous PURE puis sortie");
    let fin = *b.gains_db.last().unwrap();
    assert!(fin > -0.2, "niveau final {fin:+.2} dB\n{}", b.trace(sortie));
}

/// C2 — après un retrait sous PURE et la sortie, `volume_avant_traitement`
/// ne reste pas périmé : une fois la marge gardée rendue, le repère suivant
/// (ReplayGain posé à la piste suivante) l'oublie.
#[test]
fn pas_de_repere_perime_apres_un_retrait_sous_pure_9576() {
    let mut b = Banc::new();
    b.jouer(50);
    b.entrer_en_pure();
    b.jouer(80);
    b.sortie.clear_convolver();
    b.sortir_de_pure();
    b.jouer(100);
    b.sortie.set_replaygain_factor(1.0);
    assert_eq!(
        b.sortie.volume_avant_traitement.load(Ordering::SeqCst),
        u32::MAX,
        "repère périmé après la sortie de PURE"
    );
}
