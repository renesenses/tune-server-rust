//! Témoins de #6709 : bornes (jamais de gain positif), refus des entrées
//! absurdes, valeurs de modes calculées à la main, et application par le
//! chemin de l'égaliseur sans hausse ni marche.

use super::*;
use crate::audio::eq::{EqProcessor, EqProfile};

fn piece(l: f64, w: f64, h: f64, arriere: f64, avant: f64) -> Piece {
    Piece {
        length_m: l,
        width_m: w,
        height_m: h,
        listener_from_back_wall_m: arriere,
        speaker_from_front_wall_m: avant,
        listener_from_side_wall_m: None,
        listener_height_m: None,
        speaker_height_m: None,
        rt60_s: None,
        count: None,
    }
}

/// La pièce de référence : 5 × 4 × 2,5 m, auditeur à 1 m du mur arrière,
/// enceintes à 0,8 m du mur avant.
fn reference() -> Piece {
    piece(5.0, 4.0, 2.5, 1.0, 0.8)
}

fn verifier_les_bornes(p: &Proposition, contexte: &str) {
    assert!(
        p.filtres.len() <= FILTRES_MAX,
        "{contexte} : {} filtres",
        p.filtres.len()
    );
    for f in &p.filtres {
        let b = f.bande();
        assert!(
            b.gain <= -ATTENUATION_MIN_DB && b.gain >= -ATTENUATION_MAX_DB,
            "{contexte} : filtre à {:.1} Hz de gain {} dB, hors de [−20, −3] — \
             un filtre de mode n'a JAMAIS de gain positif (#6709)",
            f.freq_hz,
            b.gain
        );
        assert!(b.gain < 0.0, "{contexte} : gain {} ≥ 0", b.gain);
        assert!(
            (Q_MIN..=Q_MAX).contains(&b.q),
            "{contexte} : Q {} hors de [4, 20]",
            b.q
        );
        assert!(
            (FREQUENCE_MIN_HZ..=FREQUENCE_MAX_HZ).contains(&b.freq),
            "{contexte} : fréquence {} hors de [20, 250]",
            b.freq
        );
        assert_eq!(b.band_type, "peak");
        assert_eq!(b.channel, None);
    }
}

// ── Témoin (3) : valeurs calculées à la main ────────────────────────────

/// f = c/2 · sqrt((nx/L)² + (ny/W)² + (nz/H)²), c = 343 m/s, sur 5 × 4 × 2,5 m :
/// - (1,0,0) = 171,5 / 5 = 34,3 Hz ; (2,0,0) = 68,6 Hz ; (3,0,0) = 102,9 Hz ;
/// - (0,1,0) = 171,5 / 4 = 42,875 Hz ; (0,2,0) = 85,75 Hz ;
/// - (0,0,1) = 171,5 / 2,5 = 68,6 Hz ;
/// - (1,1,0) = 171,5 · sqrt(0,04 + 0,0625) = 171,5 · 0,320156 = 54,907 Hz ;
/// - (1,1,1) = 171,5 · sqrt(0,04 + 0,0625 + 0,16) = 171,5 · 0,512348 = 87,868 Hz.
#[test]
fn les_modes_de_la_piece_de_reference_valent_le_calcul_a_la_main_6709() {
    let p = reference().valider().unwrap();
    let modes = modes_propres(&p);
    let attendus: [([u32; 3], f64, NatureDuMode); 8] = [
        ([1, 0, 0], 34.3, NatureDuMode::Axial),
        ([2, 0, 0], 68.6, NatureDuMode::Axial),
        ([3, 0, 0], 102.9, NatureDuMode::Axial),
        ([0, 1, 0], 42.875, NatureDuMode::Axial),
        ([0, 2, 0], 85.75, NatureDuMode::Axial),
        ([0, 0, 1], 68.6, NatureDuMode::Axial),
        ([1, 1, 0], 54.907, NatureDuMode::Tangential),
        ([1, 1, 1], 87.868, NatureDuMode::Oblique),
    ];
    for (n, f, nature) in attendus {
        let m = modes
            .iter()
            .find(|m| m.n == n)
            .unwrap_or_else(|| panic!("mode {n:?} absent"));
        assert!(
            (m.freq_hz - f).abs() < 0.001,
            "mode {n:?} : {} Hz, calcul à la main {f} Hz",
            m.freq_hz
        );
        assert_eq!(m.nature, nature, "mode {n:?}");
    }
    // Axiaux entre 20 et 250 Hz : 7 en longueur (34,3·k ≤ 240,1), 5 en
    // largeur (42,875·k ≤ 214,4), 3 en hauteur (68,6·k ≤ 205,8).
    let axiaux = modes
        .iter()
        .filter(|m| m.nature == NatureDuMode::Axial)
        .count();
    assert_eq!(axiaux, 15);
    assert!(
        modes.windows(2).all(|w| w[0].freq_hz <= w[1].freq_hz),
        "les modes sont rendus par fréquence croissante"
    );
    assert!(
        modes
            .iter()
            .all(|m| (FREQUENCE_MIN_HZ..=FREQUENCE_MAX_HZ).contains(&m.freq_hz))
    );
}

/// L'auditeur au milieu de la largeur (le défaut) est sur le nœud de (0,1,0) :
/// ce mode ne s'y entend pas, il n'est pas proposé. (0,2,0) y a un ventre.
#[test]
fn un_auditeur_sur_un_noeud_n_entend_pas_le_mode_6709() {
    let prop = proposer(&reference()).unwrap();
    let m010 = prop.modes.iter().find(|m| m.n == [0, 1, 0]).unwrap();
    assert!(
        m010.couplage < 1e-9,
        "couplage de (0,1,0) : {}",
        m010.couplage
    );
    assert!(
        prop.filtres.iter().all(|f| !f.modes.contains(&[0, 1, 0])),
        "un mode sur un nœud de l'auditeur ne reçoit pas de filtre"
    );
    let m020 = prop.modes.iter().find(|m| m.n == [0, 2, 0]).unwrap();
    assert!(
        m020.couplage > 0.3,
        "couplage de (0,2,0) : {}",
        m020.couplage
    );
}

/// L'exemple de l'issue : un mode axial pleinement couplé vers 37,5 Hz reçoit
/// environ −12 dB et Q ≈ 7 (RT60 0,4 s : Δf = 5,5 Hz).
#[test]
fn un_mode_axial_plein_recoit_moins_douze_et_q_sept_6709() {
    // L = 343 / 75 = 4,5733 m → (1,0,0) à 37,5 Hz. Auditeur et enceinte
    // contre les murs (ventres de pression du mode).
    let mut p = piece(343.0 / 75.0, 3.3, 2.4, 0.05, 0.05);
    p.count = Some(10);
    let prop = proposer(&p).unwrap();
    let f = prop
        .filtres
        .iter()
        .find(|f| f.modes.contains(&[1, 0, 0]))
        .expect("le mode (1,0,0) est proposé");
    assert!((f.freq_hz - 37.5).abs() < 0.5, "{} Hz", f.freq_hz);
    assert!(
        f.gain_db() <= -11.5 && f.gain_db() >= -ATTENUATION_PROPOSEE_MAX_DB,
        "{} dB",
        f.gain_db()
    );
    assert!((f.q - 37.5 / 5.5).abs() < 0.3, "Q {}", f.q);
}

// ── Témoin (1) : aucun gain positif, quelles que soient les entrées ─────

/// Construire un filtre ne peut pas rendre un gain positif, même si on le
/// lui demande.
#[test]
fn un_filtre_de_mode_ne_porte_jamais_de_gain_positif_6709() {
    for att in [
        -50.0,
        -12.0,
        -3.0,
        0.0,
        1.0,
        3.0,
        12.0,
        20.0,
        100.0,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ] {
        let f = FiltreDeMode::new(60.0, att, 7.0, vec![[1, 0, 0]]);
        assert!(
            (-ATTENUATION_MAX_DB..=-ATTENUATION_MIN_DB).contains(&f.gain_db()),
            "atténuation demandée {att} : gain {} dB (attendu entre −20 et −3)",
            f.gain_db()
        );
        assert!(f.bande().gain < 0.0);
    }
}

/// Balayage : 4 000 pièces tirées dans les bornes, plus toutes les
/// combinaisons de bornes extrêmes. Chaque proposition reste dans
/// [−20, −3] dB, Q [4, 20], [20, 250] Hz, 10 filtres au plus.
#[test]
fn aucune_piece_ne_fait_proposer_un_gain_positif_6709() {
    // Générateur congruentiel : déterministe, sans dépendance.
    let mut graine: u64 = 0x6709_6709;
    let mut tirer = move |min: f64, max: f64| {
        graine = graine
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        min + (max - min) * ((graine >> 11) as f64 / (1u64 << 53) as f64)
    };
    let mut proposees = 0;
    for i in 0..4_000 {
        let l = tirer(COTE_MIN_M, COTE_MAX_M);
        let w = tirer(COTE_MIN_M, COTE_MAX_M);
        let h = tirer(HAUTEUR_MIN_M, HAUTEUR_MAX_M);
        let arriere = tirer(0.0, l);
        let avant = tirer(0.0, l);
        let mut p = piece(l, w, h, arriere, avant);
        p.rt60_s = Some(tirer(RT60_MIN_S, RT60_MAX_S));
        p.count = Some(tirer(5.0, 10.99) as i64);
        p.listener_height_m = Some(tirer(0.0, h));
        p.speaker_height_m = Some(tirer(0.0, h));
        p.listener_from_side_wall_m = Some(tirer(0.0, w));
        if let Ok(prop) = proposer(&p) {
            verifier_les_bornes(&prop, &format!("tirage {i} ({p:?})"));
            proposees += 1;
        }
    }
    assert!(proposees > 1_000, "{proposees} tirages seulement acceptés");

    for l in [COTE_MIN_M, COTE_MAX_M] {
        for w in [COTE_MIN_M, COTE_MAX_M] {
            for h in [HAUTEUR_MIN_M, HAUTEUR_MAX_M] {
                for rt in [RT60_MIN_S, RT60_MAX_S] {
                    for n in [FILTRES_MIN as i64, FILTRES_MAX as i64] {
                        let mut p = piece(l, w, h, 0.05, 0.05);
                        p.rt60_s = Some(rt);
                        p.count = Some(n);
                        let prop = proposer(&p).expect("bornes extrêmes acceptées");
                        verifier_les_bornes(&prop, &format!("{p:?}"));
                    }
                }
            }
        }
    }
}

/// Une pièce de vie ordinaire reçoit bien de 5 à 10 filtres.
#[test]
fn une_piece_ordinaire_recoit_de_cinq_a_dix_filtres_6709() {
    for (l, w, h, arriere, avant) in [
        (5.0, 4.0, 2.5, 1.0, 0.8),
        (6.2, 3.8, 2.6, 1.4, 0.6),
        (4.2, 3.1, 2.4, 0.9, 0.5),
        (8.0, 5.5, 2.8, 2.0, 1.0),
        (3.0, 3.0, 2.5, 0.8, 0.4),
    ] {
        let prop = proposer(&piece(l, w, h, arriere, avant)).unwrap();
        assert!(
            (FILTRES_MIN..=FILTRES_DEFAUT).contains(&prop.filtres.len()),
            "{l} × {w} × {h} : {} filtres",
            prop.filtres.len()
        );
        verifier_les_bornes(&prop, &format!("{l} × {w} × {h}"));
        assert!(
            prop.filtres
                .windows(2)
                .all(|f| f[0].freq_hz <= f[1].freq_hz),
            "filtres par fréquence croissante"
        );
    }
}

/// Les dimensions absurdes sont REFUSÉES, avec le champ fautif.
#[test]
fn les_entrees_absurdes_sont_refusees_proprement_6709() {
    let cas: Vec<(Piece, &str, &str)> = vec![
        (
            piece(0.0, 4.0, 2.5, 1.0, 0.8),
            "length_m",
            "room_value_out_of_bounds",
        ),
        (
            piece(-5.0, 4.0, 2.5, 1.0, 0.8),
            "length_m",
            "room_value_out_of_bounds",
        ),
        (
            piece(1e9, 4.0, 2.5, 1.0, 0.8),
            "length_m",
            "room_value_out_of_bounds",
        ),
        (
            piece(f64::NAN, 4.0, 2.5, 1.0, 0.8),
            "length_m",
            "room_value_not_finite",
        ),
        (
            piece(f64::INFINITY, 4.0, 2.5, 1.0, 0.8),
            "length_m",
            "room_value_not_finite",
        ),
        (
            piece(5.0, 0.0, 2.5, 1.0, 0.8),
            "width_m",
            "room_value_out_of_bounds",
        ),
        (
            piece(5.0, -4.0, 2.5, 1.0, 0.8),
            "width_m",
            "room_value_out_of_bounds",
        ),
        (
            piece(5.0, f64::MAX, 2.5, 1.0, 0.8),
            "width_m",
            "room_value_out_of_bounds",
        ),
        (
            piece(5.0, 4.0, 0.0, 1.0, 0.8),
            "height_m",
            "room_value_out_of_bounds",
        ),
        (
            piece(5.0, 4.0, 1e6, 1.0, 0.8),
            "height_m",
            "room_value_out_of_bounds",
        ),
        (
            piece(5.0, 4.0, f64::NEG_INFINITY, 1.0, 0.8),
            "height_m",
            "room_value_not_finite",
        ),
        (
            piece(5.0, 4.0, 2.5, -1.0, 0.8),
            "listener_from_back_wall_m",
            "room_value_out_of_bounds",
        ),
        (
            piece(5.0, 4.0, 2.5, 6.0, 0.8),
            "listener_from_back_wall_m",
            "room_value_out_of_bounds",
        ),
        (
            piece(5.0, 4.0, 2.5, 1.0, -0.8),
            "speaker_from_front_wall_m",
            "room_value_out_of_bounds",
        ),
        (
            piece(5.0, 4.0, 2.5, 1.0, 5.0),
            "speaker_from_front_wall_m",
            "room_value_out_of_bounds",
        ),
    ];
    for (p, champ, cle) in cas {
        let e = proposer(&p).expect_err(&format!("{p:?} doit être refusée"));
        assert_eq!(e.champ(), Some(champ), "{p:?} : {e}");
        assert_eq!(e.cle(), cle, "{p:?} : {e}");
    }

    // L'auditeur derrière les enceintes, ou collé à leur plan.
    for (arriere, avant) in [(4.0, 3.0), (2.0, 2.8)] {
        let e = proposer(&piece(5.0, 4.0, 2.5, arriere, avant)).unwrap_err();
        assert_eq!(e.cle(), "room_positions_incompatible", "{e}");
    }

    // Les optionnels sont contrôlés de même.
    let mut p = reference();
    p.count = Some(4);
    assert_eq!(proposer(&p).unwrap_err().champ(), Some("count"));
    p.count = Some(11);
    assert_eq!(proposer(&p).unwrap_err().champ(), Some("count"));
    p.count = Some(-3);
    assert_eq!(proposer(&p).unwrap_err().champ(), Some("count"));
    let mut p = reference();
    p.rt60_s = Some(0.0);
    assert_eq!(proposer(&p).unwrap_err().champ(), Some("rt60_s"));
    let mut p = reference();
    p.listener_height_m = Some(3.0);
    assert_eq!(proposer(&p).unwrap_err().champ(), Some("listener_height_m"));
    let mut p = reference();
    p.speaker_height_m = Some(f64::NAN);
    assert_eq!(proposer(&p).unwrap_err().champ(), Some("speaker_height_m"));
    let mut p = reference();
    p.listener_from_side_wall_m = Some(-1.0);
    assert_eq!(
        proposer(&p).unwrap_err().champ(),
        Some("listener_from_side_wall_m")
    );
}

// ── Témoin (2) : par le chemin de l'égaliseur, sans hausse ni marche ────

const TAUX: u32 = 48_000;

/// Le profil que `POST /zones/{id}/eq` écrit pour ces bandes.
fn profil_de(prop: &Proposition) -> EqProfile {
    EqProfile {
        enabled: true,
        bands: prop.bandes(),
        ..Default::default()
    }
}

fn sinus(freq: f64, secondes: f64, amplitude: f32) -> Vec<f32> {
    let n = (f64::from(TAUX) * secondes) as usize;
    let mut v = Vec::with_capacity(n * 2);
    for i in 0..n {
        let s = (amplitude as f64
            * (2.0 * std::f64::consts::PI * freq * i as f64 / f64::from(TAUX)).sin())
            as f32;
        v.push(s);
        v.push(s);
    }
    v
}

fn rms(v: &[f32]) -> f64 {
    (v.iter().map(|s| f64::from(*s).powi(2)).sum::<f64>() / v.len().max(1) as f64).sqrt()
}

/// Les bandes proposées, jouées par l'EqProcessor de la sortie locale :
/// aucune fréquence ne sort plus fort qu'elle n'est entrée (régime établi),
/// et aucun échantillon ne dépasse la crête d'entrée, transitoire compris.
#[test]
fn les_filtres_proposes_ne_font_jamais_monter_le_niveau_rendu_6709() {
    for p in [
        reference(),
        piece(6.2, 3.8, 2.6, 1.4, 0.6),
        piece(3.0, 3.0, 2.5, 0.8, 0.4),
    ] {
        let prop = proposer(&p).unwrap();
        let profil = profil_de(&prop);
        assert!(
            profil.max_reponse_db_at(0, f64::from(TAUX)) <= 1e-9,
            "la cascade proposée pousse quelque part : {} dB",
            profil.max_reponse_db_at(0, f64::from(TAUX))
        );
        let mut freqs: Vec<f64> = prop.filtres.iter().map(|f| f.freq_hz).collect();
        freqs.extend([
            25.0, 31.5, 45.0, 63.0, 100.0, 160.0, 250.0, 500.0, 1000.0, 4000.0, 12000.0,
        ]);
        for f in freqs {
            let mut eq = EqProcessor::new(&profil, TAUX, 2);
            let entree = sinus(f, 3.0, 0.5);
            let mut sortie = entree.clone();
            eq.process_interleaved(&mut sortie);
            let fin = entree.len() - TAUX as usize * 2; // la dernière seconde
            let gain = rms(&sortie[fin..]) / rms(&entree[fin..]);
            // Régime établi : la crête ne dépasse pas non plus celle d'entrée.
            // (À l'ATTAQUE d'un sinus, un profil purement soustractif peut
            // dépasser la crête d'entrée de quelques dixièmes de dB : il ne
            // réserve aucune marge, décision chiffrée de #4594, inchangée ici.)
            let crete = sortie[fin..].iter().fold(0.0_f32, |m, s| m.max(s.abs()));
            assert!(
                crete <= 0.5 + 1e-3,
                "{p:?} à {f:.1} Hz : crête établie {crete} > crête d'entrée 0,5"
            );
            assert!(
                gain <= 1.0 + 1e-3,
                "{p:?} à {f:.1} Hz : le niveau rendu monte de {:.3} dB",
                20.0 * gain.log10()
            );
        }
    }
}

/// Appliquer la proposition PENDANT la lecture passe par la relève de
/// l'égaliseur (#5215) : fondu de 300 ms, aucune fenêtre de 10 ms plus forte
/// que le signal sec, et aucune marche entre deux fenêtres voisines.
#[test]
fn appliquer_la_proposition_en_vol_ne_fait_ni_hausse_ni_marche_6709() {
    let prop = proposer(&reference()).unwrap();
    let freqs: Vec<f64> = [1000.0]
        .into_iter()
        .chain(prop.filtres.iter().map(|f| f.freq_hz))
        .collect();
    for (f, depuis_un_egaliseur) in freqs.iter().flat_map(|f| [(*f, false), (*f, true)]) {
        // Égaliseur coupé (aucun), ou à plat en service (activé, sans
        // bande) ; puis la proposition prend la relève, sur un sinus continu.
        let tout = sinus(f, 5.0, 0.5);
        let coupe = TAUX as usize * 2 * 2;
        let avant = depuis_un_egaliseur.then(|| {
            let plat = EqProfile {
                enabled: true,
                ..Default::default()
            };
            let mut avant = EqProcessor::new(&plat, TAUX, 2);
            let mut chauffe = tout[..coupe].to_vec();
            avant.process_interleaved(&mut chauffe);
            avant
        });
        let mut eq = EqProcessor::prendre_la_releve(
            Some(EqProcessor::new(&profil_de(&prop), TAUX, 2)),
            avant,
        )
        .expect("un égaliseur reste monté");
        if !depuis_un_egaliseur {
            assert!(eq.en_fondu(), "l'activation en vol arme le fondu de #5215");
        }

        let suite = &tout[coupe..];
        let mut sortie = suite.to_vec();
        for bloc in sortie.chunks_mut(960 * 2) {
            eq.process_interleaved(bloc);
        }
        // Rapport sortie / entrée par fenêtre de 50 ms : comparer à la MÊME
        // fenêtre du signal sec neutralise les périodes incomplètes.
        let fenetre = 2_400 * 2;
        let rapports: Vec<f64> = sortie
            .chunks(fenetre)
            .zip(suite.chunks(fenetre))
            .map(|(s, e)| rms(s) / rms(e))
            .collect();
        for (i, r) in rapports.iter().enumerate() {
            assert!(
                *r <= 1.0 + 1e-2,
                "{f:.1} Hz (depuis un égaliseur : {depuis_un_egaliseur}), fenêtre {i} : {:+.2} dB par rapport au sec",
                20.0 * r.log10()
            );
        }
        // Vers le haut : aucune marche (0,5 dB par 50 ms au plus). Vers le
        // bas, à la fréquence même d'un mode, l'atténuation s'installe au
        // rythme du biquad (constante de temps ~Q/(π·f), 50 à 70 ms sous
        // 40 Hz) quand on part d'un égaliseur déjà monté : la règle #1725
        // ne fond pas une bascule qui déplace le niveau moyen de moins de
        // 1 dB. Ce n'est pas une marche : on borne à 6 dB par 50 ms.
        for (i, w) in rapports.windows(2).enumerate() {
            let pas = 20.0 * (w[1] / w[0]).log10();
            assert!(
                pas < 0.5 && pas > -6.0,
                "{f:.1} Hz (depuis un égaliseur : {depuis_un_egaliseur}), fenêtres {i}→{} : marche de {pas:+.2} dB",
                i + 1
            );
        }
    }
}
