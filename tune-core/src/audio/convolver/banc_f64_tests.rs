//! TESS T09b — banc numérique du moteur de convolution en `f64`.
//!
//! - **64 bits réels** : la sortie du moteur est comparée à une convolution
//!   DIRECTE calculée en double précision (la définition, sans FFT ni blocs),
//!   sur une impulsion, un sinus et un bruit, découpés en appels de tailles
//!   irrégulières. Écart maximal exigé : 1e-12 (≈ −240 dBFS).
//! - **Contre-épreuve** : le même banc, sur le même moteur instancié en `f32`
//!   (l'ancien calcul), échoue : son écart est de l'ordre de 1e-7.
//! - **Latence annoncée exacte** : une impulsion sort `latency_frames()`
//!   trames plus tard, à l'échantillon près.
//! - **Changement de filtre sans discontinuité** : bascule flux en cours
//!   (réchauffe muette puis fondu) ; contre-épreuve : le remplacement brutal
//!   d'avant saute.
//! - **Rééchantillonnage du filtre à phase préservée** : 44,1 → 96 kHz, même
//!   réponse en phase à ±1° sur 20 Hz – 20 kHz.
//! - **Coût processeur** (banc ignoré, à lancer en `--release`) : RI de
//!   65 536 prises, stéréo, 96 et 192 kHz, `f64` contre `f32`.

use super::*;

/// Générateur pseudo-aléatoire déterministe (LCG 64 bits), valeurs dans
/// [−0,5 ; 0,5[.
struct Lcg(u64);

impl Lcg {
    fn suivant(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 11) as f64 / (1u64 << 53) as f64) - 0.5
    }
}

/// Une réponse de pièce plausible : impulsion directe, puis bruit à
/// décroissance exponentielle. Valeurs représentables en `f32` (les deux
/// moteurs du banc reçoivent EXACTEMENT les mêmes prises).
fn ri_de_piece(prises: usize, graine: u64) -> Vec<f64> {
    let mut g = Lcg(graine);
    (0..prises)
        .map(|n| {
            let v = if n == 0 {
                0.7
            } else {
                0.3 * g.suivant() * (-(n as f64) / (prises as f64 / 6.0)).exp()
            };
            f64::from(v as f32)
        })
        .collect()
}

/// Convolution directe de référence, en `f64` : y[n] = Σ h[k]·x[n−k].
fn reference(x: &[f64], h: &[f64]) -> Vec<f64> {
    let mut y = vec![0.0f64; x.len()];
    for (n, yn) in y.iter_mut().enumerate() {
        let k_max = n.min(h.len() - 1);
        let mut acc = 0.0f64;
        for k in 0..=k_max {
            acc += h[k] * x[n - k];
        }
        *yn = acc;
    }
    y
}

/// Les signaux du banc (mono), de `trames` trames.
fn signaux(trames: usize) -> Vec<(&'static str, Vec<f64>)> {
    let mut impulsion = vec![0.0; trames];
    impulsion[0] = 1.0;
    impulsion[trames / 3] = -0.5;
    let sinus = (0..trames)
        .map(|n| 0.9 * (2.0 * std::f64::consts::PI * 997.0 * n as f64 / 48_000.0).sin())
        .collect();
    let mut g = Lcg(0x7E55);
    let bruit = (0..trames).map(|_| 1.8 * g.suivant()).collect();
    vec![("impulsion", impulsion), ("sinus", sinus), ("bruit", bruit)]
}

/// Fait passer `x` (stéréo entrelacé) par un moteur neuf de type `T`, en
/// appels de tailles irrégulières, et rend la sortie ALIGNÉE (latence
/// retirée), de même longueur.
fn passer<T: Flottant>(ri: &[Vec<f64>], bloc: usize, x: &[f64]) -> Vec<f64> {
    let mut moteur = Convolver::<T>::depuis_f64(ri, bloc);
    let ch = ri.len();
    let latence = moteur.latency_frames();
    let mut entree: Vec<T> = x.iter().map(|&v| T::de_f64(v)).collect();
    entree.extend(std::iter::repeat_n(T::default(), latence * ch));
    let lots = [1usize, 255, bloc, 700, 37, 3 * bloc + 1];
    let (mut i, mut t) = (0, 0);
    while i < entree.len() / ch {
        let n = lots[t % lots.len()].min(entree.len() / ch - i);
        t += 1;
        moteur.traiter(&mut entree[i * ch..(i + n) * ch]);
        i += n;
    }
    entree[latence * ch..].iter().map(|v| v.en_f64()).collect()
}

/// Écart maximal du moteur `T` à la référence directe, sur les trois signaux,
/// en stéréo (une réponse différente par canal).
fn ecart_du_banc<T: Flottant>() -> Vec<(&'static str, f64)> {
    let ri = vec![ri_de_piece(2048, 1), ri_de_piece(1500, 2)];
    let bloc = 256;
    let trames = 6000;
    signaux(trames)
        .into_iter()
        .map(|(nom, s)| {
            // Canal droit : le signal retourné, pour que les canaux diffèrent.
            let droite: Vec<f64> = s.iter().rev().map(|v| -0.8 * v).collect();
            let mut x = Vec::with_capacity(trames * 2);
            for n in 0..trames {
                x.push(s[n]);
                x.push(droite[n]);
            }
            let y = passer::<T>(&ri, bloc, &x);
            let refs = [reference(&s, &ri[0]), reference(&droite, &ri[1])];
            let mut ecart = 0.0f64;
            for n in 0..trames {
                for c in 0..2 {
                    ecart = ecart.max((y[n * 2 + c] - refs[c][n]).abs());
                }
            }
            (nom, ecart)
        })
        .collect()
}

/// Seuil du critère « 64 bits réels » (plan TESS, § 1.1).
const SEUIL_64_BITS: f64 = 1e-12;

#[test]
fn banc_le_moteur_f64_suit_la_convolution_directe_a_1e_12() {
    for (nom, ecart) in ecart_du_banc::<f64>() {
        eprintln!("banc f64 — {nom} : écart max {ecart:.3e}");
        assert!(
            ecart < SEUIL_64_BITS,
            "{nom} : écart {ecart:e} ≥ {SEUIL_64_BITS:e} à la convolution directe f64"
        );
    }
}

#[test]
fn contre_epreuve_le_meme_banc_en_f32_echoue() {
    // Le banc discrimine : l'ancien calcul (le même moteur en `f32`) s'écarte
    // de la référence de l'ordre de 1e-7, bien au-dessus du seuil.
    let ecarts = ecart_du_banc::<f32>();
    for (nom, ecart) in &ecarts {
        eprintln!("banc f32 — {nom} : écart max {ecart:.3e}");
    }
    let pire = ecarts.iter().map(|(_, e)| *e).fold(0.0, f64::max);
    assert!(
        pire > 1e3 * SEUIL_64_BITS,
        "le moteur f32 devrait échouer au banc (écart {pire:e})"
    );
    for (nom, ecart) in &ecarts {
        assert!(
            *ecart >= SEUIL_64_BITS,
            "{nom} : le f32 passerait le banc ({ecart:e}) — le banc ne discrimine pas"
        );
    }
}

#[test]
fn le_chemin_f32_herite_passe_par_le_moteur_f64() {
    // `process_interleaved(&mut [f32])` (chemins hérités) calcule en f64 :
    // son seul écart est l'arrondi f32 de la SORTIE (≤ 2^-24 relatif).
    let ri = vec![ri_de_piece(512, 3)];
    let s = &signaux(3000)[2].1;
    let x32: Vec<f32> = s.iter().map(|&v| v as f32).collect();
    let x64: Vec<f64> = x32.iter().map(|&v| f64::from(v)).collect();
    let mut m = Convolver::new(&[ri[0].iter().map(|&v| v as f32).collect()], 128);
    let mut y = x32.clone();
    y.extend(std::iter::repeat_n(0.0f32, m.latency_frames()));
    m.process_interleaved(&mut y);
    let r = reference(&x64, &ri[0]);
    for n in 0..x32.len() {
        // Un seul arrondi f32, celui de la sortie : au plus un demi-ULP de
        // la valeur exacte (plus l'écart f64 du moteur, < 1e-12).
        let demi_ulp = f64::from((r[n] as f32).abs()) * f64::from(f32::EPSILON) / 2.0;
        let ecart = (f64::from(y[n + 128]) - r[n]).abs();
        assert!(
            ecart <= demi_ulp + 1e-12,
            "trame {n} : {ecart:e} > {demi_ulp:e}"
        );
    }
}

#[test]
fn la_latence_annoncee_est_exacte_a_l_echantillon() {
    for bloc in [64usize, 256, 1024] {
        for pic in [0usize, 5, 1000] {
            let mut h = vec![0.0f64; pic + 1];
            h[pic] = 1.0;
            let mut m = Convolver::<f64>::depuis_f64(&[h], bloc);
            let latence = m.latency_frames();
            assert_eq!(latence, bloc, "latence annoncée = un bloc");
            let mut x = vec![0.0f64; pic + latence + 3 * bloc];
            x[0] = 1.0;
            m.traiter(&mut x);
            let (i_max, v_max) = x.iter().enumerate().fold((0, 0.0f64), |a, (i, v)| {
                if v.abs() > a.1 { (i, v.abs()) } else { a }
            });
            assert_eq!(i_max, latence + pic, "bloc {bloc}, pic {pic}");
            assert!((v_max - 1.0).abs() < 1e-14);
            // Rien avant la latence annoncée (pas d'avance cachée).
            assert!(x[..latence + pic].iter().all(|v| v.abs() < 1e-14));
        }
    }
}

/// Sinus de test des bascules (48 kHz, 997 Hz, −6 dBFS).
fn sinus_de_bascule(trames: usize) -> Vec<f64> {
    (0..trames)
        .map(|n| 0.5 * (2.0 * std::f64::consts::PI * 997.0 * n as f64 / 48_000.0).sin())
        .collect()
}

/// Plus grand saut entre deux trames consécutives.
fn plus_grand_saut(y: &[f64]) -> f64 {
    y.windows(2)
        .map(|w| (w[1] - w[0]).abs())
        .fold(0.0, f64::max)
}

/// Pente maximale d'un sinus d'amplitude 0,5 à 997 Hz / 48 kHz.
fn pente_du_sinus() -> f64 {
    0.5 * 2.0 * std::f64::consts::PI * 997.0 / 48_000.0
}

#[test]
fn le_changement_de_filtre_en_lecture_est_continu() {
    // Filtre A : gain 1. Filtre B : gain 0,25 retardé de 37 trames (phase
    // changée) suivi d'un écho. La bascule arrive au milieu d'un appel.
    let bloc = 256;
    let a = vec![1.0f64];
    let mut b = vec![0.0f64; 300];
    b[37] = 0.25;
    b[299] = 0.1;
    let x = sinus_de_bascule(48_000);
    let mut m = Convolver::<f64>::depuis_f64(&[a], bloc);
    let mut y = x.clone();
    let (debut, fin) = y.split_at_mut(10_000 + 13);
    m.traiter(debut);
    let fondu = 960; // 20 ms
    assert!(
        m.remplacer_en_fondu(Convolver::depuis_f64(&[b.clone()], bloc), fondu)
            .is_ok()
    );
    assert!(m.bascule_en_cours());
    m.traiter(fin);
    assert!(!m.bascule_en_cours(), "la bascule est terminée");

    // 1. Aucun saut : la sortie ne bouge jamais plus vite qu'un sinus de
    //    même amplitude (marge de 5 % pour la somme des deux contributions).
    let saut = plus_grand_saut(&y[bloc..]);
    eprintln!(
        "bascule — plus grand saut {saut:.4} (pente du sinus {:.4})",
        pente_du_sinus()
    );
    assert!(saut <= 1.05 * pente_du_sinus(), "saut {saut}");

    // 2. Après réchauffe et fondu, la sortie est EXACTEMENT celle du filtre B
    //    tournant depuis le début du flux (rien de la bascule ne reste).
    let ref_b = reference(&x, &b);
    let apres = 10_013 + (300 + bloc) + fondu + 1;
    let mut ecart = 0.0f64;
    for n in apres..x.len() {
        ecart = ecart.max((y[n] - ref_b[n - bloc]).abs());
    }
    assert!(ecart < SEUIL_64_BITS, "après bascule : écart {ecart:e}");
}

#[test]
fn contre_epreuve_le_remplacement_brutal_saute() {
    // L'ancien geste de la sortie locale : `*convolver = Some(nouveau)`. Le
    // nouveau moteur repart d'une file amorcée de zéros : la sortie tombe à 0
    // d'un coup, puis reprend sans l'historique — le test de continuité
    // ci-dessus le refuserait.
    let bloc = 256;
    let x = sinus_de_bascule(20_000);
    let mut m = Convolver::<f64>::depuis_f64(&[vec![1.0]], bloc);
    let mut y = x.clone();
    let (debut, fin) = y.split_at_mut(10_013);
    m.traiter(debut);
    m = Convolver::depuis_f64(&[vec![0.0, 0.25]], bloc);
    m.traiter(fin);
    let saut = plus_grand_saut(&y[bloc..]);
    assert!(
        saut > 1.05 * pente_du_sinus(),
        "le remplacement brutal devrait sauter ({saut})"
    );
}

#[test]
fn une_bascule_incompatible_est_rendue_a_l_appelant() {
    let mut m = Convolver::<f64>::depuis_f64(&[vec![1.0]], 256);
    let autre_canaux = Convolver::depuis_f64(&[vec![1.0], vec![1.0]], 256);
    assert!(m.remplacer_en_fondu(autre_canaux, 960).is_err());
    let autre_bloc = Convolver::depuis_f64(&[vec![1.0]], 512);
    assert!(m.remplacer_en_fondu(autre_bloc, 960).is_err());
    m.eteindre_en_fondu(0);
    assert!(
        m.remplacer_en_fondu(Convolver::depuis_f64(&[vec![1.0]], 256), 960)
            .is_err(),
        "un moteur éteint ne se rallume pas en cours de flux"
    );
}

#[test]
fn la_bascule_n_alloue_rien_dans_traiter() {
    // Les capacités de toutes les files et de tous les tampons sont posées à
    // la construction : traiter (bascule comprise) ne les fait pas grandir.
    let bloc = 128;
    let mut m = Convolver::<f64>::depuis_f64(&[ri_de_piece(400, 9), ri_de_piece(300, 8)], bloc);
    let capacites = |m: &Convolver<f64>| {
        (
            m.output_buf
                .iter()
                .map(VecDeque::capacity)
                .collect::<Vec<_>>(),
            m.direct_retarde
                .iter()
                .map(VecDeque::capacity)
                .collect::<Vec<_>>(),
            m.input_buf.iter().map(Vec::capacity).collect::<Vec<_>>(),
        )
    };
    let mut x: Vec<f64> = sinus_de_bascule(20_000)
        .into_iter()
        .flat_map(|v| [v, -v])
        .collect();
    let (a, b) = x.split_at_mut(5_000);
    m.traiter(a);
    let nouveau = Convolver::<f64>::depuis_f64(&[ri_de_piece(200, 7), ri_de_piece(250, 6)], bloc);
    let attendu = capacites(&nouveau);
    m.remplacer_en_fondu(nouveau, 480).ok().unwrap();
    m.traiter(b);
    assert!(!m.bascule_en_cours());
    assert_eq!(capacites(&m), attendu, "le successeur n'a pas grandi");
}

#[test]
fn la_reponse_reechantillonnee_de_44_1_a_96_khz_garde_sa_phase_a_1_degre() {
    // Une réponse de pièce à 44,1 kHz : impulsion directe, champ diffus en
    // décroissance exponentielle (passe-bas à un pôle : partie réelle
    // positive, donc aucun creux où la phase ne serait pas définie), écho à
    // 6,8 ms. Rééchantillonnée vers 96 kHz par le repli de #8761 (rubato
    // sinc, noyau symétrique, marges retirées).
    let de = 44_100u32;
    let vers = 96_000u32;
    let n = 8192;
    let ri: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f64 / de as f64;
            let mut v = if i == 0 {
                0.8
            } else {
                0.3 * (-(t / 0.01)).exp()
            };
            if i == 300 {
                v += 0.2;
            }
            v as f32
        })
        .collect();
    let rr = par_cadence::reechantillonner_ri(&[ri.clone()], de, vers);
    // Bande utile : jusqu'à 0,45 × 44,1 kHz (19,8 kHz), comme le témoin de #8761.
    let freqs = log_freq_grid(120, 20.0, 0.45 * de as f64);
    let avant = fir_frequency_response(&ri, de, &freqs);
    let apres = fir_frequency_response(&rr[0], vers, &freqs);
    let mut pire_phase = 0.0f64;
    let mut pire_db = 0.0f64;
    for (a, b) in avant.iter().zip(&apres) {
        let mut d = b.phase_deg - a.phase_deg;
        while d > 180.0 {
            d -= 360.0;
        }
        while d < -180.0 {
            d += 360.0;
        }
        pire_phase = pire_phase.max(d.abs());
        pire_db = pire_db.max((b.magnitude_db - a.magnitude_db).abs());
    }
    eprintln!("rééchantillonnage 44,1 → 96 kHz : phase ±{pire_phase:.4}°, module ±{pire_db:.4} dB");
    assert!(pire_phase <= 1.0, "phase : écart {pire_phase}°");
    assert!(pire_db <= 0.1, "module : écart {pire_db} dB");
}

/// Coût processeur, F64 contre F32, sur la RI de la chaîne type (65 536
/// prises), stéréo, à 96 et 192 kHz, en blocs de 1024 trames — plus l'écart à
/// la référence directe sur cette RI longue (toutes les partitions servent).
///
/// `cargo test -j4 -p tune-core --release --lib banc_cpu_convolution -- --ignored --nocapture`
#[test]
#[ignore = "banc processeur, en --release sur Shrek"]
fn banc_cpu_convolution_65536_prises() {
    let prises = 65_536;
    let ri = vec![ri_de_piece(prises, 11), ri_de_piece(prises, 12)];
    for taux in [96_000usize, 192_000] {
        let secondes = 10;
        let trames = taux * secondes;
        let mut g = Lcg(5);
        let x: Vec<f64> = (0..trames * 2).map(|_| g.suivant()).collect();
        let mut temps = Vec::new();
        for f64_ou_f32 in [true, false] {
            let debut = std::time::Instant::now();
            if f64_ou_f32 {
                let mut m = Convolver::<f64>::depuis_f64(&ri, 1024);
                let mut y = x.clone();
                for bloc in y.chunks_mut(1024 * 2) {
                    m.traiter(bloc);
                }
                std::hint::black_box(&y);
            } else {
                let mut m = Convolver::<f32>::depuis_f64(&ri, 1024);
                let mut y: Vec<f32> = x.iter().map(|&v| v as f32).collect();
                for bloc in y.chunks_mut(1024 * 2) {
                    m.traiter(bloc);
                }
                std::hint::black_box(&y);
            }
            let s = debut.elapsed().as_secs_f64();
            temps.push(s);
            eprintln!(
                "CPU convolution {prises} prises stéréo à {} kHz, {} : {s:.3} s pour {secondes} s d'audio = {:.2} % d'un cœur",
                taux / 1000,
                if f64_ou_f32 { "f64" } else { "f32" },
                100.0 * s / secondes as f64
            );
        }
        eprintln!(
            "rapport F64/F32 à {} kHz : {:.2}",
            taux / 1000,
            temps[0] / temps[1]
        );
    }
    // Précision sur la RI longue : 70 000 trames (toutes les partitions).
    let trames = 70_000;
    let mut g = Lcg(77);
    let s: Vec<f64> = (0..trames).map(|_| 1.8 * g.suivant()).collect();
    let r = reference(&s, &ri[0]);
    for f64_ou_f32 in [true, false] {
        let y = if f64_ou_f32 {
            passer::<f64>(&ri[..1], 1024, &s)
        } else {
            passer::<f32>(&ri[..1], 1024, &s)
        };
        let ecart = y
            .iter()
            .zip(&r)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        eprintln!(
            "précision RI {prises} prises, bruit : {} écart max {ecart:.3e}",
            if f64_ou_f32 { "f64" } else { "f32" }
        );
        if f64_ou_f32 {
            assert!(ecart < SEUIL_64_BITS);
        }
    }
}
