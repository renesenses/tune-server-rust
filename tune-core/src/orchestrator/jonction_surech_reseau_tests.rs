//! rc15 — bancs mesurés de la continuité gapless (M1), du niveau constant
//! (M2) et du garde-fou du fichier refait sur un profil « Raspberry Pi » (M3).
//!
//! Chaque banc passe par la VRAIE chaîne réseau (`StreamingDsp::process_piste`,
//! étage `SurechReseau`, quantification 24 bits avec dither TPDF) et porte sa
//! contre-épreuve : le même signal sans le correctif dépasse la borne.

use super::*;
use crate::audio::surechantillonnage as su;
use crate::orchestrator::StreamingDsp;
use crate::orchestrator::chaine_reseau_tess::SurechReseau;

const DE: u32 = 44_100;
const VERS: u32 = 176_400;
const CANAUX: u16 = 2;
/// Un pas de quantification 24 bits.
const LSB: f64 = 1.0 / 8_388_608.0;

fn reglages(filtre: &str) -> su::UpsamplerSettings {
    su::UpsamplerSettings {
        enabled: true,
        filter: filtre.into(),
        ..Default::default()
    }
}

fn sinus(n: usize, f: f64, a: f64, phase: f64) -> Vec<f64> {
    (0..n)
        .flat_map(|i| {
            let x = a * (std::f64::consts::TAU * f * i as f64 / f64::from(DE) + phase).sin();
            [x, x]
        })
        .collect()
}

fn en_pcm24(x: &[f64]) -> Vec<u8> {
    x.iter()
        .flat_map(|v| {
            let m = (v * 8_388_608.0).round().clamp(-8_388_608.0, 8_388_607.0) as i32;
            let b = m.to_le_bytes();
            [b[0], b[1], b[2]]
        })
        .collect()
}

fn depuis_pcm24(b: &[u8]) -> Vec<f64> {
    b.chunks_exact(3)
        .map(|s| f64::from(i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8) / 8_388_608.0)
        .collect()
}

fn etage(filtre: &str) -> su::SurechantillonneurF64 {
    su::SurechantillonneurF64::nouveau(&reglages(filtre), DE, VERS, CANAUX).unwrap()
}

/// Une piste suréchantillonnée par la vraie chaîne réseau.
fn surechantillonner(
    filtre: &str,
    x: &[f64],
    jonction: ContexteDeJonction,
    niveau: Option<RampeDeNiveau>,
) -> Vec<f64> {
    let mut dsp = StreamingDsp {
        channels: CANAUX,
        sample_rate: DE,
        surechantillonneur: Some(SurechReseau::nouveau(etage(filtre)).avec_jonction(jonction)),
        niveau,
        ..Default::default()
    };
    let mut pcm = en_pcm24(x);
    dsp.process_piste(&mut pcm, 24);
    depuis_pcm24(&pcm)
}

/// Une piste NON suréchantillonnée, par la même chaîne (étage de niveau seul).
fn sans_surech(x: &[f64], niveau: Option<RampeDeNiveau>) -> Vec<f64> {
    let mut dsp = StreamingDsp {
        channels: CANAUX,
        sample_rate: DE,
        niveau,
        ..Default::default()
    };
    let mut pcm = en_pcm24(x);
    dsp.process_piste(&mut pcm, 24);
    depuis_pcm24(&pcm)
}

/// La plus grande dérivée seconde de la voie gauche sur `[debut, fin)`.
fn derivee_seconde_max(y: &[f64], debut: usize, fin: usize) -> f64 {
    let g: Vec<f64> = y.iter().step_by(2).copied().collect();
    (debut.max(1)..fin.min(g.len() - 1))
        .map(|k| (g[k + 1] - 2.0 * g[k] + g[k - 1]).abs())
        .fold(0.0, f64::max)
}

fn rms_db(y: &[f64], debut: usize, fin: usize) -> f64 {
    let g: Vec<f64> = y.iter().step_by(2).copied().collect();
    let s: f64 = g[debut..fin].iter().map(|v| v * v).sum();
    10.0 * (s / (fin - debut) as f64).log10()
}

fn crete(y: &[f64]) -> f64 {
    y.iter().fold(0.0f64, |m, v| m.max(v.abs()))
}

/// Deux pistes enchaînées sans blanc : un sinus de 1 kHz coupé au milieu
/// d'une arche. Avec les bords (contexte de la voisine), la dérivée seconde
/// à la frontière reste sous celle du sinus suréchantillonné, au dither
/// près ; le résultat est celui de la piste entière. Contre-épreuve : les
/// mêmes pistes suréchantillonnées SEULES (état du filtre à zéro) la
/// dépassent largement.
#[test]
fn deux_pistes_enchainees_sans_blanc_ne_cliquent_pas_rc15() {
    let a = 0.5;
    let n = 44_100usize;
    let coupe = n / 2 + 7;
    let x = sinus(n, 1_000.0, a, 0.3);
    for filtre in su::greffon::OFFERED_FILTERS {
        let k = trames_de_contexte(etage(filtre).portee_entree());
        let (pa, pb) = x.split_at(coupe * 2);
        let ya = surechantillonner(
            filtre,
            pa,
            ContexteDeJonction {
                avant: Bord::Inconnu,
                apres: Bord::Continu(pb[..k * 2].to_vec()),
            },
            None,
        );
        let yb = surechantillonner(
            filtre,
            pb,
            ContexteDeJonction {
                avant: Bord::Continu(pa[pa.len() - k * 2..].to_vec()),
                apres: Bord::Inconnu,
            },
            None,
        );
        let l = (VERS / DE) as usize;
        assert_eq!(
            ya.len(),
            coupe * l * 2,
            "{filtre} : n·L trames pour la piste A"
        );
        assert_eq!(
            yb.len(),
            (n - coupe) * l * 2,
            "{filtre} : n·L trames pour la piste B"
        );
        let mut enchaine = ya.clone();
        enchaine.extend_from_slice(&yb);
        let entiere = surechantillonner(filtre, &x, ContexteDeJonction::default(), None);
        assert_eq!(enchaine.len(), entiere.len());

        let k0 = coupe * l;
        let (d, f) = (k0 - 4_000, k0 + 4_000);
        // Borne : la dérivée seconde d'un sinus d'amplitude A·g échantillonné
        // à 176,4 kHz, plus le dither (TPDF ±1 LSB et l'arrondi : 6 LSB).
        let amplitude = a * su::gain_de_zone();
        let w = std::f64::consts::TAU * 1_000.0 / f64::from(VERS);
        let borne = amplitude * (2.0 * (w / 2.0).sin()).powi(2) * 1.02 + 6.0 * LSB;
        let mesure = derivee_seconde_max(&enchaine, d, f);
        let ecart = enchaine[2 * d..2 * f]
            .iter()
            .zip(&entiere[2 * d..2 * f])
            .fold(0.0f64, |m, (u, v)| m.max((u - v).abs()));
        eprintln!(
            "M1 filtre={filtre} contexte={k} d2_max={mesure:.3e} borne={borne:.3e} \
             ecart_a_la_piste_entiere={:.1} dB",
            20.0 * (ecart / amplitude).log10()
        );
        assert!(
            mesure <= borne,
            "{filtre} : clic à la frontière ({mesure:.3e} > {borne:.3e})"
        );
        assert!(
            ecart <= 8.0 * LSB,
            "{filtre} : la frontière n'est pas celle d'un seul flux (écart {ecart:.3e})"
        );

        // Contre-épreuve : chaque piste seule, comme avant rc15.
        let mut seules = surechantillonner(filtre, pa, ContexteDeJonction::default(), None);
        seules.extend(surechantillonner(
            filtre,
            pb,
            ContexteDeJonction::default(),
            None,
        ));
        let sabote = derivee_seconde_max(&seules, d, f);
        eprintln!("M1 contre-épreuve filtre={filtre} d2_max={sabote:.3e}");
        assert!(
            sabote > 3.0 * borne,
            "{filtre} : la contre-épreuve devait cliquer ({sabote:.3e})"
        );
    }
}

/// Sans continuité, un bord est un fondu court et propre : la piste part de
/// zéro et y revient, sans marche, en n·L trames exactement.
#[test]
fn une_rupture_est_un_fondu_court_rc15() {
    let n = 4_410usize;
    let a = 0.5;
    let x = sinus(n, 1_000.0, a, 1.0);
    let y = surechantillonner(
        "polyvalent",
        &x,
        ContexteDeJonction {
            avant: Bord::Rupture,
            apres: Bord::Rupture,
        },
        None,
    );
    assert_eq!(y.len(), n * 4 * 2);
    let f = trames_de_fondu(VERS);
    assert!(y[0].abs() < 0.01 * a && y[y.len() - 1].abs() < 0.01 * a);
    let amplitude = a * su::gain_de_zone();
    let w = std::f64::consts::TAU * 1_000.0 / f64::from(VERS);
    // Le fondu ajoute au plus (π/F)² × A à la dérivée seconde du sinus.
    let borne = amplitude
        * ((2.0 * (w / 2.0).sin()).powi(2)
            + 2.0 * w * std::f64::consts::PI / f as f64
            + (std::f64::consts::PI / f as f64).powi(2))
        + 6.0 * LSB;
    let total = y.len() / 2;
    assert!(derivee_seconde_max(&y, 1, 2 * f) <= borne);
    assert!(derivee_seconde_max(&y, total - 2 * f, total) <= borne);
}

/// M2 — le niveau RMS d'une piste suréchantillonnée (quel que soit le
/// filtre) et celui d'une piste qui ne l'est pas, sur la même zone, sont
/// égaux à ±0,1 dB. Contre-épreuve : sans l'étage de niveau, la piste non
/// suréchantillonnée est à +3 dB.
#[test]
fn le_niveau_rms_ne_depend_pas_du_surechantillonnage_rc15() {
    let n = 44_100usize;
    let x = sinus(n, 997.0, 0.5, 0.0);
    let zone = NiveauPlanifie {
        depart: su::gain_de_zone(),
        cible: su::gain_de_zone(),
    };
    let non_surech = sans_surech(&x, zone.rampe(DE, 2, false));
    let reference = rms_db(&non_surech, n / 4, 3 * n / 4);
    for filtre in su::greffon::OFFERED_FILTERS {
        let y = surechantillonner(
            filtre,
            &x,
            ContexteDeJonction::default(),
            zone.rampe(DE, 2, true),
        );
        let r = rms_db(&y, n, 3 * n);
        eprintln!(
            "M2 filtre={filtre} rms_surech={r:.3} dB rms_non_surech={reference:.3} dB \
             reserve_du_filtre={:.2} dB",
            su::greffon::headroom_db(filtre, 4).unwrap()
        );
        assert!(
            (r - reference).abs() <= 0.1,
            "{filtre} : {r:.3} contre {reference:.3} dB"
        );
    }
    // Contre-épreuve : la piste non suréchantillonnée sans le niveau de zone.
    let brut = rms_db(&sans_surech(&x, None), n / 4, 3 * n / 4);
    eprintln!("M2 contre-épreuve rms_sans_niveau={brut:.3} dB");
    assert!(brut - reference > 2.9);
}

/// M2 — la crête servie ne dépasse JAMAIS celle de la source : ni sur une
/// piste suréchantillonnée (crête vraie, fs/4 à 45° compris), ni pendant une
/// rampe, montée comprise ; la montée rejoint le niveau visé en 0,5 s sans le
/// dépasser.
#[test]
fn la_crete_ne_depasse_jamais_la_source_rc15() {
    let n = 44_100usize;
    // fs/4 à 45° : crête des échantillons 0,707·A, crête vraie A.
    let a = 0.9;
    let x = sinus(n, f64::from(DE) / 4.0, a, std::f64::consts::FRAC_PI_4);
    let g = su::gain_de_zone();
    for (depart, cible) in [(g, g), (1.0, g), (g, 1.0)] {
        let plan = NiveauPlanifie { depart, cible };
        let y = surechantillonner(
            "polyvalent",
            &x,
            ContexteDeJonction::default(),
            plan.rampe(DE, 2, true),
        );
        let c = crete(&y);
        eprintln!("M2 crête suréchantillonnée {depart:.3}→{cible:.3} : {c:.4} (source vraie {a})");
        // Tolérance : l'ondulation de la bande passante (< 0,001 dB).
        assert!(c <= a * 1.000_1 + LSB, "suréchantillonnée : {c} > {a}");
        let y = sans_surech(&x, plan.rampe(DE, 2, false));
        let c = crete(&y);
        let source = crete(&x);
        assert!(c <= source + LSB, "non suréchantillonnée : {c} > {source}");
    }
    // La montée : monotone, jamais au-dessus de sa cible, atteinte à 0,5 s.
    let r = RampeDeNiveau::nouvelle(g, 1.0, DE, 2);
    let mut avant = 0.0;
    for i in 0..(DE as usize) {
        let v = r.gain(i);
        assert!(v >= avant && v <= 1.0, "trame {i} : {v}");
        avant = v;
    }
    assert!(r.gain(DE as usize / 4) < 0.99);
    assert_eq!(r.gain(DE as usize / 2), 1.0);
}

fn verdict_pi(facteur: f64) -> su::VerdictMachine {
    su::VerdictMachine {
        simd_path: "neon".into(),
        margin: su::greffon::MACHINE_MARGIN,
        results: vec![su::MesureMachine {
            filter: "polyvalent".into(),
            from_hz: 44_100,
            to_hz: 352_800,
            realtime_factor: facteur,
            admitted: facteur >= su::greffon::MACHINE_MARGIN,
        }],
        measured_at: 0,
    }
}

fn plan_pi(facteur: f64) -> super::super::surechantillonnage_reseau::SurechPlanifie {
    super::super::surechantillonnage_reseau::SurechPlanifie {
        reglages: reglages("polyvalent"),
        de: 44_100,
        vers: 176_400,
        proposees: vec![44_100, 48_000, 88_200, 96_000, 176_400, 192_000],
        flac: true,
        verdict: verdict_pi(facteur),
        cadences_flac: vec![44_100, 48_000, 88_200, 96_000, 176_400, 192_000],
        cadences_lpcm: Vec::new(),
        plafond_zone: None,
        jonction: Default::default(),
        niveau: None,
    }
}

/// M3 — le garde-fou sur un profil « Raspberry Pi » (polyvalent à ×6 le
/// temps réel vers 352,8 kHz, admis de justesse) et un lecteur qui ne lit que
/// le FLAC :
/// - une piste de 30 s ne tient pas à 176,4 kHz (5 s estimées) mais tient à
///   88,2 kHz : elle part à 88,2 kHz au lieu d'être refusée ;
/// - le coût MESURÉ des fichiers déjà refaits prime sur l'estimation : à
///   0,05 s de rendu par seconde de musique, une piste d'une minute tient à
///   176,4 kHz (1,5 s) que l'estimation refusait (10 s) ;
/// - contre-épreuve : une piste de 10 min reste refusée (le long silence
///   reste interdit).
#[test]
fn le_garde_fou_sur_un_profil_pi_rc15() {
    let max = su::DELAI_MAX_AVANT_LA_PREMIERE_NOTE_S;
    let p = plan_pi(6.0);
    let delai = |q: &super::super::surechantillonnage_reseau::SurechPlanifie,
                 duree: f64,
                 mesure: Option<su::CoutMesure>| {
        su::delai_du_fichier_s(&q.verdict, "polyvalent", q.vers, duree, mesure)
    };
    // 30 s : 5 s à 176,4 kHz, 2,5 s à 88,2 kHz.
    let d = delai(&p, 30.0, None).unwrap();
    assert!((d - 5.0).abs() < 1e-9, "{d}");
    let q = p
        .cadence_plus_basse_qui_tient(|q| delai(q, 30.0, None).is_some_and(|d| d <= max))
        .expect("une cadence plus basse tient");
    assert_eq!(q.vers, 88_200);
    eprintln!("M3 Pi 30 s : 176,4 kHz {d:.2} s, part à {} Hz", q.vers);

    // Le coût mesuré sur cette machine prime.
    let mesure = Some(su::CoutMesure {
        secondes_par_seconde: 0.05,
        mesures: 3,
    });
    assert!(delai(&p, 60.0, None).unwrap() > max);
    let d = delai(&p, 60.0, mesure).unwrap();
    assert!(d <= max, "{d}");
    eprintln!(
        "M3 Pi 60 s : estimé {:.2} s, mesuré {d:.2} s",
        delai(&p, 60.0, None).unwrap()
    );

    // Contre-épreuve : 10 min ne tient à aucune cadence.
    assert!(
        p.cadence_plus_basse_qui_tient(|q| delai(q, 600.0, None).is_some_and(|d| d <= max))
            .is_none()
    );
    assert!(delai(&p, 600.0, mesure).unwrap() > max);
}

/// M3 — la mesure : une moyenne glissante de ce que coûtent réellement les
/// fichiers refaits, ramenée à la cadence de référence.
#[test]
fn le_cout_du_fichier_est_mesure_rc15() {
    let filtre = "resolution-pm";
    assert!(su::cout_mesure_du_fichier(filtre, 176_400).is_none());
    // 60 s de musique rendues en 3 s vers 176,4 kHz : 0,1 s/s à 352,8 kHz.
    su::noter_le_cout_du_fichier(filtre, 176_400, 60.0, 3.0);
    let c = su::cout_mesure_du_fichier(filtre, 88_200).unwrap();
    assert!((c.secondes_par_seconde - 0.1).abs() < 1e-12, "{c:?}");
    su::noter_le_cout_du_fichier(filtre, 176_400, 60.0, 6.0);
    let c = su::cout_mesure_du_fichier(filtre, 176_400).unwrap();
    assert!((c.secondes_par_seconde - (0.1 * 2.0 / 3.0 + 0.2 / 3.0)).abs() < 1e-12);
    assert_eq!(c.mesures, 2);
}

/// m — un lecteur qui refuse une cadence annoncée ne la reçoit plus.
#[test]
fn une_cadence_refusee_est_retenue_rc15() {
    let app = "uuid:banc-rc15-menteur";
    let annoncees = vec![44_100, 88_200, 176_400, 192_000];
    assert_eq!(
        su::sans_les_cadences_refusees(app, annoncees.clone()),
        annoncees
    );
    su::noter_une_cadence_refusee(app, 176_400);
    assert_eq!(
        su::sans_les_cadences_refusees(app, annoncees),
        vec![44_100, 88_200, 192_000]
    );
}

/// M2 — le plan de niveau suit la zone : une piste part du niveau de la
/// précédente, et la retient pour la suivante.
#[test]
fn le_niveau_part_de_la_piste_precedente_rc15() {
    let zone = 9_015_001;
    let g = su::gain_de_zone();
    let p = planifier_le_niveau(zone, g);
    assert_eq!((p.depart, p.cible), (g, g));
    let p = planifier_le_niveau(zone, 1.0);
    assert_eq!((p.depart, p.cible), (g, 1.0));
    assert!(p.rampe(DE, 2, false).is_some());
    let p = planifier_le_niveau(zone, 1.0);
    assert!(!p.agit());
    assert!(cible_de_la_piste(true, false) < 1.0);
    assert_eq!(cible_de_la_piste(false, false), 1.0);
}
