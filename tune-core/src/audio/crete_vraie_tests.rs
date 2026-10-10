//! Conformité de la crête vraie (#2713) : signaux de l'EBU Tech 3341 (cas 15
//! à 19), sinus pleine échelle aux pires phases, exactitude de l'économie par
//! blocs, invariance au découpage, multicanal.
//!
//! La valeur attendue est celle du signal CONTINU, connue analytiquement :
//! l'amplitude de la sinusoïde. C'est la référence indépendante — elle ne
//! dépend d'aucun filtre. Le filtre du tableau de l'annexe 2 de BS.1770 et
//! l'ancien Catmull-Rom sont mesurés sur les mêmes signaux, pour comparaison.

use super::*;
use std::f64::consts::PI;

fn db(x: f64) -> f64 {
    20.0 * x.log10()
}

/// Sinusoïde stéréo (les deux canaux en phase) d'amplitude `a`, de fréquence
/// `f` à `fs`, de phase `phase` (radians), `secondes` de long, avec un fondu
/// d'entrée et de sortie de 10 ms (Tech 3341, cas 15).
fn sinus(fs: usize, f: f64, a: f64, phase: f64, secondes: f64) -> Vec<f64> {
    let n = (fs as f64 * secondes) as usize;
    let fondu = fs / 100;
    let mut v = Vec::with_capacity(2 * n);
    for i in 0..n {
        let enveloppe = if i < fondu {
            0.5 - 0.5 * (PI * i as f64 / fondu as f64).cos()
        } else if i >= n - fondu {
            0.5 - 0.5 * (PI * (n - 1 - i) as f64 / fondu as f64).cos()
        } else {
            1.0
        };
        let x = a * enveloppe * (2.0 * PI * f * i as f64 / fs as f64 + phase).sin();
        v.push(x);
        v.push(x);
    }
    v
}

fn tune(fs: usize, x: &[f64]) -> f64 {
    let mut m = CreteVraie::new(fs, 2);
    m.nourrir(x);
    m.crete()
}

fn annexe(x: &[f64]) -> f64 {
    let mut m = CreteVraie::annexe_2_bs1770(2);
    m.nourrir(x);
    m.crete()
}

fn catmull_rom(x: &[f64]) -> f64 {
    let mut m = CreteCatmullRom::new(2);
    m.nourrir(x);
    m.crete()
}

/// La phase qui place la crête du sinus à mi-chemin entre deux points de la
/// grille suréchantillonnée `facteur`× : le pire cas de l'appendice 1 de
/// l'annexe 2. Crête à `t = 1/(2·facteur)` d'intervalle après un échantillon.
fn phase_pire(fs: usize, f: f64, facteur: usize) -> f64 {
    PI / 2.0 - 2.0 * PI * (f / fs as f64) * (0.5 / facteur as f64)
}

// ─── EBU Tech 3341, tableau 1, cas 15 à 19 ─────────────────────────────────

/// Les cas 15 à 19 du tableau 1 de l'EBU Tech 3341 (2023) : fréquence en
/// fraction de fs, amplitude (FFS), phase (degrés), crête vraie attendue
/// (dBTP). Tolérance de la norme : +0,2 / −0,4 dB. Tune tient ±0,1 dB.
const TECH_3341: [(u32, f64, f64, f64, f64); 5] = [
    (15, 4.0, 0.50, 0.0, -6.0),
    (16, 4.0, 0.50, 45.0, -6.0),
    (17, 6.0, 0.50, 60.0, -6.0),
    (18, 8.0, 0.50, 67.5, -6.0),
    (19, 4.0, 1.41, 45.0, 3.0),
];

#[test]
fn tech_3341_cas_15_a_19_a_0_1_db_pres() {
    for fs in [48_000usize, 44_100] {
        for (cas, diviseur, a, phase_deg, attendu) in TECH_3341 {
            let x = sinus(fs, fs as f64 / diviseur, a, phase_deg.to_radians(), 1.0);
            let mesure = db(tune(fs, &x));
            let norme = db(annexe(&x));
            eprintln!(
                "Tech 3341 cas {cas} à {fs} Hz : attendu {attendu:+.1} dBTP, \
                 Tune {mesure:+.3}, annexe 2 {norme:+.3}, Catmull-Rom {:+.3}",
                db(catmull_rom(&x))
            );
            // L'attendu de la norme est arrondi au dixième : on compare à
            // l'amplitude exacte (−6,02 dB pour 0,5 ; +2,98 dB pour 1,41).
            let exact = db(a);
            assert!(
                (mesure - exact).abs() <= 0.1,
                "cas {cas} à {fs} Hz : {mesure:.3} dBTP, attendu {exact:.3} ±0,1"
            );
            assert!(
                (mesure - attendu) <= 0.2 && (attendu - mesure) <= 0.4,
                "cas {cas} : hors de la tolérance de la Tech 3341"
            );
            // Le filtre de l'annexe, recopié du tableau, tient lui aussi la
            // tolérance de la Tech 3341 : c'est ce qui le valide comme
            // référence.
            assert!(
                (norme - attendu) <= 0.2 && (attendu - norme) <= 0.4,
                "cas {cas} à {fs} Hz : filtre de l'annexe {norme:.3}, hors tolérance"
            );
        }
    }
}

// ─── Sinus pleine échelle aux pires phases ─────────────────────────────────

/// Le pire écart sur un balayage de phases, en dB, et la phase où il tombe.
fn pire_sur_les_phases(fs: usize, f: f64, mesure: impl Fn(&[f64]) -> f64) -> (f64, f64) {
    pire_sur_n_phases(fs, f, 96, mesure)
}

fn pire_sur_n_phases(fs: usize, f: f64, n: usize, mesure: impl Fn(&[f64]) -> f64) -> (f64, f64) {
    let mut pire = (f64::INFINITY, 0.0);
    let mut phases: Vec<f64> = (0..n).map(|k| 2.0 * PI * k as f64 / n as f64).collect();
    phases.push(phase_pire(fs, f, 8));
    phases.push(phase_pire(fs, f, 4));
    phases.push(phase_pire(fs, f, 1));
    for phase in phases {
        let x = sinus(fs, f, 1.0, phase, 0.25);
        let e = db(mesure(&x));
        if e.abs() > pire.0.abs() || pire.0.is_infinite() {
            pire = (e, phase);
        }
    }
    pire
}

#[test]
fn sinus_pleine_echelle_997_hz_12_khz_16_khz_aux_pires_phases() {
    for fs in [48_000usize, 44_100] {
        for f in [997.0, 12_000.0, 16_000.0] {
            let (e, phase) = pire_sur_les_phases(fs, f, |x| tune(fs, x));
            let (e_norme, _) = pire_sur_les_phases(fs, f, annexe);
            let (e_cr, _) = pire_sur_les_phases(fs, f, catmull_rom);
            // La borne de l'appendice 1 : 20·log10(cos(π·f/fs / 8)).
            let borne = db((PI * f / fs as f64 / 8.0).cos());
            eprintln!(
                "{f} Hz à {fs} Hz, 0 dBTP : Tune {e:+.3} dB (phase {:.1}°, borne de grille \
                 {borne:+.3}), annexe 2 {e_norme:+.3}, Catmull-Rom {e_cr:+.3}",
                phase.to_degrees()
            );
            assert!(
                e.abs() <= 0.1,
                "{f} Hz à {fs} Hz : écart {e:.3} dB à la phase {:.1}°",
                phase.to_degrees()
            );
            // Jamais plus bas que ce que la grille 8× permet (à l'erreur du
            // filtre près, 0,02 dB), jamais au-dessus du signal continu de
            // plus de 0,02 dB.
            assert!(
                e >= borne - 0.02,
                "{f} Hz : {e:.3} sous la borne {borne:.3}"
            );
            assert!(e <= 0.02, "{f} Hz : {e:.3} au-dessus du signal continu");
        }
    }
}

/// La contre-épreuve de la raison d'être du chantier : l'ancien calcul
/// sous-estimait la crête de plus d'un décibel à 12 et 16 kHz.
#[test]
fn catmull_rom_sous_estimait_de_plus_d_un_decibel() {
    let (e12, _) = pire_sur_les_phases(48_000, 12_000.0, catmull_rom);
    let (e16, _) = pire_sur_les_phases(48_000, 16_000.0, catmull_rom);
    assert!(e12 < -1.0, "12 kHz : {e12:.3}");
    assert!(e16 < -1.0, "16 kHz : {e16:.3}");
}

/// Hors de la bande d'origine des tests, aux débits élevés (88,2 à 352,8 kHz,
/// dont celui du DSD converti) : 4×, 16 kHz et 20 kHz au pire.
#[test]
fn hautes_frequences_d_echantillonnage() {
    for fs in [88_200usize, 96_000, 176_400, 192_000, 352_800] {
        assert_eq!(facteur_de_surechantillonnage(fs), 4);
        for f in [997.0, 16_000.0, 20_000.0] {
            let (e, _) = pire_sur_n_phases(fs, f, 24, |x| tune(fs, x));
            eprintln!("{f} Hz à {fs} Hz : {e:+.3} dB");
            assert!(e.abs() <= 0.12, "{f} Hz à {fs} Hz : {e:.3} dB");
        }
    }
    for fs in [8_000usize, 22_050, 32_000, 44_100, 48_000, 88_199] {
        assert_eq!(facteur_de_surechantillonnage(fs), 8, "{fs}");
    }
}

// ─── Exactitude de l'économie, découpage, multicanal ───────────────────────

/// Un signal qui ressemble à de la musique pour l'économie : enveloppe qui
/// varie, partiels, bruit pseudo-aléatoire déterministe, quelques overs.
fn musique(fs: usize, canaux: usize, secondes: f64) -> Vec<f64> {
    let n = (fs as f64 * secondes) as usize;
    let mut graine: u64 = 0x2713;
    let mut alea = move || {
        graine = graine
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((graine >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    };
    let mut v = Vec::with_capacity(n * canaux);
    for i in 0..n {
        let t = i as f64 / fs as f64;
        let env = 0.3 + 0.7 * (0.5 + 0.5 * (2.0 * PI * 0.7 * t).sin()).powi(3);
        for c in 0..canaux {
            let s = 0.5 * (2.0 * PI * (220.0 + 55.0 * c as f64) * t).sin()
                + 0.3 * (2.0 * PI * 3_520.0 * t + c as f64).sin()
                + 0.25 * (2.0 * PI * 11_025.7 * t).sin()
                + 0.1 * alea();
            v.push(env * s);
        }
    }
    v
}

#[test]
fn l_economie_par_blocs_rend_le_calcul_complet_au_bit_pres() {
    for fs in [44_100usize, 96_000] {
        let x = musique(fs, 2, 3.0);
        let avec = tune(fs, &x);
        let sans = crete_vraie_sans_economie(fs, 2, &x);
        assert_eq!(avec.to_bits(), sans.to_bits(), "{fs} Hz : {avec} != {sans}");
        assert!(avec > 1.0, "le signal de test doit déborder : {avec}");
    }
}

#[test]
fn invariante_au_decoupage() {
    let fs = 44_100;
    let x = musique(fs, 2, 2.0);
    let entier = tune(fs, &x);
    for morceau in [1usize, 7, 101, 4_410, 30 * 44_100] {
        let mut m = CreteVraie::new(fs, 2);
        for c in x.chunks(morceau * 2) {
            m.nourrir(c);
        }
        assert_eq!(
            m.crete().to_bits(),
            entier.to_bits(),
            "morceaux de {morceau}"
        );
    }
}

/// Six canaux : la crête est celle du canal le plus chaud, mesuré seul.
#[test]
fn multicanal_la_crete_du_canal_le_plus_chaud() {
    let fs = 48_000;
    let canaux = 6;
    let mut x = musique(fs, canaux, 1.0);
    // Le canal 4 porte un over inter-échantillons franc : fs/4 à 45°.
    for (i, trame) in x.chunks_mut(canaux).enumerate() {
        // Fondu d'entrée de 10 ms : un départ brutal sonnerait (Gibbs).
        let fondu = (i as f64 / 480.0).min(1.0);
        trame[4] = fondu * 1.2 * (PI / 4.0 + PI / 2.0 * i as f64).sin();
    }
    let mut tous = CreteVraie::new(fs, canaux);
    tous.nourrir(&x);
    let mut max_seul = 0.0f64;
    for c in 0..canaux {
        let seul: Vec<f64> = x.iter().skip(c).step_by(canaux).copied().collect();
        let mut m = CreteVraie::new(fs, 1);
        m.nourrir(&seul);
        max_seul = max_seul.max(m.crete());
    }
    assert_eq!(tous.crete().to_bits(), max_seul.to_bits());
    assert!((db(tous.crete()) - db(1.2)).abs() < 0.1, "{}", tous.crete());
}

#[test]
fn le_pic_d_echantillon_est_toujours_compris() {
    let x = [0.0, 0.0, 0.9, -0.95, 0.0, 0.0];
    let mut m = CreteVraie::new(44_100, 1);
    m.nourrir(&x);
    assert!(m.crete() >= 0.95);
    let mut vide = CreteVraie::new(44_100, 0);
    vide.nourrir(&x);
    assert_eq!(vide.crete(), 0.0);
}
