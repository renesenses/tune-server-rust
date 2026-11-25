//! rc14 (sécurité auditive, décision de Bertrand du 25/11, suite) — sous
//! PURE et « Bypass all », la convolution (souvent −6 à −12 dB de réserve),
//! le crossfeed et le repli mono étaient coupés d'un bloc à l'autre, et le
//! ReplayGain passait à 1,0 d'un coup (`refresh_zone_pure_dsp`) : +6 à
//! +10 dB, et un clic. Même règle que pour l'égaliseur : un fondu, puis le
//! niveau d'avant le traitement, jamais au-delà. Volume inchangé : PURE
//! redevient bit-perfect ; volume monté : le surplus reste gardé (voyant,
//! « Rendre le niveau »).
use super::*;
use crate::audio::convolver::ConvolverConfig;

const TAUX: u32 = 48_000;
const BLOC: usize = 480; // 10 ms
const AMPLITUDE: f32 = 0.2;

struct Banc {
    sortie: LocalOutput,
    n: usize,
    freq: f64,
    /// Amplitude relative de la voie droite (1 : les deux voies égales).
    droite: f32,
    gains_db: Vec<f64>,
    gauche: Vec<f32>,
    identiques: Vec<bool>,
}

impl Banc {
    /// Une sortie locale qui joue en 48 kHz stéréo au volume `volume` (‰).
    fn new(volume: u32) -> Self {
        let sortie = LocalOutput::with_options("Banc".into(), false, "wasapi");
        sortie.user_volume.store(volume * 1000, Ordering::SeqCst);
        sortie.recompute_effective_volume();
        sortie
            .current_format
            .store(LocalOutput::pack_format(TAUX, 2), Ordering::Relaxed);
        Self {
            sortie,
            n: 0,
            freq: 1000.0,
            droite: 1.0,
            gains_db: Vec::new(),
            gauche: Vec::new(),
            identiques: Vec::new(),
        }
    }

    /// Une convolution à un seul coefficient `g` par voie : −12 dB pour 0,25.
    fn avec_convolution(self, g: f32) -> Self {
        self.sortie.set_convolution_autorisee(true);
        self.sortie
            .set_convolver_config(ConvolverConfig::new(vec![vec![g], vec![g]], TAUX).unwrap())
            .unwrap();
        // Départ du flux : la boucle de lecture rebâtit le moteur pour le format.
        rebuild_local_convolver(
            &self.sortie.convolver_config,
            &self.sortie.convolver,
            &self.sortie.convolution_autorisee,
            TAUX,
            2,
        )
        .unwrap();
        assert!(self.sortie.has_convolver(), "prémisse : convolution posée");
        self
    }

    fn monter_le_volume(&self, volume: u32) {
        self.sortie
            .user_volume
            .store(volume * 1000, Ordering::SeqCst);
        self.sortie.recompute_effective_volume();
        self.sortie.reborner_le_preampli_garde();
    }

    /// Comme `refresh_zone_pure_dsp` : PURE, puis la chaîne vidée.
    fn entrer_en_pure(&self) {
        let s = &self.sortie;
        s.set_pure_bypass(true);
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
                let s = AMPLITUDE * (2.0 * std::f64::consts::PI * self.freq * t).sin() as f32;
                pcm.push(s);
                pcm.push(s * self.droite);
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
        }
    }

    fn max_db_depuis(&self, depuis: usize) -> f64 {
        self.gains_db[depuis..]
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max)
    }

    fn marche_max_depuis(&self, depuis: usize) -> f64 {
        self.gains_db[depuis - 1..]
            .windows(2)
            .map(|w| w[1] - w[0])
            .fold(0.0_f64, f64::max)
    }

    fn ecart_max_depuis(&self, depuis: usize) -> f32 {
        self.gauche[depuis * BLOC..]
            .windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0, f32::max)
    }

    /// La pente maximale de la sinusoïde au gain `db`.
    fn pente_db(&self, db: f64) -> f64 {
        f64::from(AMPLITUDE) * 10f64.powf(db / 20.0) * 2.0 * std::f64::consts::PI * self.freq
            / f64::from(TAUX)
    }

    fn etat(&self) -> OutputDspState {
        let s = &self.sortie;
        local_dsp_runtime_state(
            &s.eq,
            &s.convolver,
            &s.crossfeed,
            &s.pure_bypass,
            &s.mono_downmix,
            false,
        )
    }

    /// Le gain rendu (volume × ReplayGain × garde), linéaire.
    fn rendu(&self) -> f64 {
        f64::from(self.sortie.volume.load(Ordering::SeqCst)) / f64::from(UNITE_DU_GAIN_DE_RENDU)
    }
}

/// Convolution sous PURE, volume inchangé : aucune marche (fondu du convolué
/// au sec, puis remontée de la réserve), jamais au-dessus du sec, aucun clic,
/// puis bit-perfect strict à 1 s. Avant : +12 dB en un bloc.
#[test]
fn convolution_sous_pure_volume_inchange_fondu_puis_bit_perfect_rc14() {
    let mut banc = Banc::new(500).avec_convolution(0.25);
    banc.jouer(50);
    let avant = *banc.gains_db.last().unwrap();
    assert!(
        (avant + 12.04).abs() < 0.2,
        "la convolution joue à {avant:.2} dB"
    );
    banc.entrer_en_pure();
    let entree = banc.gains_db.len();
    banc.jouer(100);
    let marche = banc.marche_max_depuis(entree);
    assert!(
        marche < 1.0,
        "marche de {marche:.2} dB en 10 ms à l'entrée en PURE"
    );
    assert!(
        banc.max_db_depuis(entree) <= 0.01,
        "au-dessus du sec : {:.2} dB",
        banc.max_db_depuis(entree)
    );
    let haut = banc.max_db_depuis(entree - 1);
    let ecart = banc.ecart_max_depuis(entree - 1);
    assert!(
        f64::from(ecart) <= banc.pente_db(haut) * 1.05,
        "clic : écart {ecart:.6} > pente {:.6}",
        banc.pente_db(haut)
    );
    assert!(
        banc.identiques[entree + 99],
        "à 1 s, PURE doit rendre l'entrée au bit près"
    );
    assert!(banc.sortie.eq.lock().unwrap().is_none());
    assert!(banc.sortie.preampli_garde().is_none());
    assert!(!matches!(banc.etat(), OutputDspState::Applied));
}

/// Convolution sous PURE, volume MONTÉ (+6 dB) depuis la pose de la
/// convolution : jamais au-dessus du niveau d'avant le traitement (−6,02 dB
/// du sec à ce volume) ; le surplus reste gardé, se voit, le chemin ne dit
/// pas « bit-perfect » ; « Rendre le niveau » le rend. Avant : 0 dB d'un coup.
#[test]
fn convolution_sous_pure_volume_monte_jamais_au_dessus_de_la_borne_rc14() {
    let mut banc = Banc::new(500).avec_convolution(0.25);
    banc.jouer(50);
    banc.monter_le_volume(1000);
    banc.jouer(5);
    let borne_db = 20.0 * 0.5_f64.log10();
    banc.entrer_en_pure();
    let entree = banc.gains_db.len();
    banc.jouer(200);
    let pire = banc.max_db_depuis(entree);
    assert!(
        pire <= borne_db + 0.1,
        "PURE, volume monté : {pire:.2} dB, au-dessus de la borne {borne_db:.2} dB"
    );
    let fin = *banc.gains_db.last().unwrap();
    assert!((fin - borne_db).abs() < 0.2, "gardé à {fin:.2} dB");
    assert!(banc.marche_max_depuis(entree) < 1.0);
    let garde = banc
        .sortie
        .preampli_garde()
        .expect("le surplus gardé se voit sous PURE");
    assert!(garde.db < -5.0, "{garde:?}");
    assert!(matches!(banc.etat(), OutputDspState::Applied));
    banc.sortie.liberer_le_preampli_garde();
    let geste = banc.gains_db.len();
    banc.jouer(500);
    assert!(banc.marche_max_depuis(geste) < 1.0);
    assert!(
        *banc.identiques.last().unwrap(),
        "bit-perfect après le geste"
    );
    assert!(banc.sortie.eq.lock().unwrap().is_none());
}

/// Crossfeed et repli mono sous PURE (voie droite muette : le repli mono
/// retire 3 dB) : même fondu, sans marche ni clic, jamais au-dessus du sec,
/// puis bit-perfect. Avant : +3 dB et un clic d'un bloc à l'autre.
#[test]
fn crossfeed_et_mono_sous_pure_fondu_puis_bit_perfect_rc14() {
    let mut banc = Banc::new(500);
    banc.droite = 0.0;
    banc.sortie
        .set_crossfeed(Some(crate::audio::crossfeed::CrossfeedProcessor::new(
            TAUX, 0.6, 0.3,
        )));
    banc.sortie.set_mono_downmix(true);
    banc.jouer(50);
    banc.entrer_en_pure();
    let entree = banc.gains_db.len();
    banc.jouer(100);
    assert!(banc.marche_max_depuis(entree) < 1.0);
    assert!(banc.max_db_depuis(entree) <= 0.01);
    let haut = banc.max_db_depuis(entree - 1);
    assert!(f64::from(banc.ecart_max_depuis(entree - 1)) <= banc.pente_db(haut) * 1.05);
    assert!(banc.identiques[entree + 99]);
    assert!(banc.sortie.eq.lock().unwrap().is_none());
}

/// Attend (au plus 3 s) que la garde du rendu ait fini sa rampe, en
/// vérifiant à chaque relevé que le gain rendu ne dépasse pas `plafond`.
fn attendre_la_garde(banc: &Banc, plafond: f64) -> f64 {
    let debut = std::time::Instant::now();
    let mut pire = 0.0_f64;
    loop {
        let r = banc.rendu();
        pire = pire.max(r);
        assert!(
            r <= plafond * 1.001,
            "gain rendu {r:.4} au-dessus du plafond {plafond:.4}"
        );
        if !banc.sortie.reserve_du_surechantillonneur.garde_en_rampe()
            && debut.elapsed() > std::time::Duration::from_millis(50)
        {
            return pire;
        }
        assert!(
            debut.elapsed() < std::time::Duration::from_secs(3),
            "la garde du rendu ne finit pas"
        );
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// ReplayGain sous PURE, volume inchangé : le facteur ne passe plus à 1 d'un
/// coup ; le gain rendu remonte en ~0,5 s jusqu'au volume seul (le niveau
/// d'avant le ReplayGain), puis PURE est à l'unité. Avant : ×2,5 d'un coup.
#[test]
fn replaygain_sous_pure_volume_inchange_fondu_puis_unite_rc14() {
    let banc = Banc::new(1000);
    banc.sortie.set_replaygain_factor(0.4);
    let avant = banc.rendu();
    assert!((avant - 0.4).abs() < 1e-3, "prémisse : {avant}");
    banc.entrer_en_pure();
    let juste_apres = banc.rendu();
    assert!(
        juste_apres <= avant * 1.01,
        "PURE : le gain rendu saute de {avant:.3} à {juste_apres:.3}"
    );
    attendre_la_garde(&banc, 1.0);
    assert!(
        (banc.rendu() - 1.0).abs() < 1e-6,
        "fin de la rampe : {}",
        banc.rendu()
    );
    assert!(banc.sortie.preampli_garde().is_none());
}

/// ReplayGain sous PURE, volume MONTÉ (25 % → 50 %) depuis la pose du
/// ReplayGain : jamais au-dessus du niveau d'avant (25 % sans ReplayGain) ;
/// le surplus reste gardé et se voit ; « Rendre le niveau » le rend. Avant :
/// 50 % d'un coup (+6 dB au-dessus du niveau d'avant).
#[test]
fn replaygain_sous_pure_volume_monte_jamais_au_dessus_rc14() {
    let banc = Banc::new(250);
    banc.sortie.set_replaygain_factor(0.4);
    banc.monter_le_volume(500);
    let avant = banc.rendu();
    assert!((avant - 0.2).abs() < 1e-3, "prémisse : {avant}");
    banc.entrer_en_pure();
    let borne = 0.25;
    attendre_la_garde(&banc, borne);
    assert!(
        (banc.rendu() - borne).abs() < 2e-3,
        "gardé à {}, attendu {borne}",
        banc.rendu()
    );
    let garde = banc
        .sortie
        .preampli_garde()
        .expect("voyant « niveau réduit »");
    assert!((garde.db - 20.0 * 0.5_f64.log10()).abs() < 0.1, "{garde:?}");
    banc.sortie.liberer_le_preampli_garde();
    attendre_la_garde(&banc, 0.5);
    assert!((banc.rendu() - 0.5).abs() < 1e-6);
    assert!(banc.sortie.preampli_garde().is_none());
}
