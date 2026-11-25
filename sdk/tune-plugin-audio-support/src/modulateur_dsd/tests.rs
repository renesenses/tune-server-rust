//! Témoins du modulateur DSD (TESS T11b). Chaque seuil est tiré d'une mesure
//! (banc `examples/banc_modulateur_dsd.rs`), avec une marge ; les valeurs
//! mesurées sont dans le corps de la PR.

use super::mesure::{analyser, bits_du_canal, reference_ordre_2, sinus};
use super::*;

const FS: u32 = 44_100;

/// Module `pcm` (stéréo) et rend les octets.
fn moduler(m: &mut ModulateurDsd, pcm: &[f64]) -> Vec<u8> {
    let mut sortie = vec![0u8; m.octets_pour(pcm.len() / m.canaux())];
    let n = m.traiter(pcm, &mut sortie).expect("modulation");
    assert_eq!(n, sortie.len());
    sortie
}

/// Trames PCM pour obtenir au moins `echantillons_dsd` bits par canal, plus
/// le retard du suréchantillonneur.
fn trames_pour(m: &ModulateurDsd, echantillons_dsd: usize) -> usize {
    (echantillons_dsd + m.retard_dsd() as usize + 4096) / m.rapport() + 1
}

/// Les `n` derniers bits du canal 0 (régime établi, transitoire écarté).
fn fin_du_canal(octets: &[u8], canaux: usize, n: usize) -> Vec<f64> {
    let bits = bits_du_canal(octets, canaux, 0);
    bits[bits.len() - n..].to_vec()
}

const N_FFT: usize = 1 << 20;

#[test]
fn le_ntf_concu_tient_son_gain_hors_bande_et_ses_poles() {
    for (multiple, plafond_db) in [(64.0, -75.0), (128.0, -115.0), (256.0, -155.0)] {
        let ntf = Ntf::concevoir(ORDRE, multiple, HINF).expect("conception");
        let g = ntf.gain_maximal();
        assert!((g - HINF).abs() < 0.01, "gain hors bande {g} ≠ {HINF}");
        assert!(
            ntf.rayon_des_poles() < 0.99,
            "pôles {}",
            ntf.rayon_des_poles()
        );
        assert_eq!(ntf.numerateur[0], 1.0);
        assert_eq!(ntf.denominateur[0], 1.0);
        // Dans la bande 0 – 22,05 kHz, le bruit est atténué d'au moins…
        let pire = (1..=200)
            .map(|i| ntf.module_en(std::f64::consts::PI / multiple * i as f64 / 200.0))
            .fold(0.0, f64::max);
        let pire_db = 20.0 * pire.log10();
        assert!(
            pire_db < plafond_db,
            "DSD{multiple} : NTF en bande {pire_db:.1} dB"
        );
    }
    assert!(Ntf::concevoir(6, 64.0, HINF).is_none(), "ordre non pris");
    assert!(
        Ntf::concevoir(ORDRE, 64.0, 1.0).is_none(),
        "hinf ≤ 1 refusé"
    );
}

#[test]
fn cadences_prises_et_refusees() {
    use CadenceDsd::*;
    assert_eq!(Dsd64.cadence_pour(44_100), Some(2_822_400));
    assert_eq!(Dsd128.cadence_pour(88_200), Some(5_644_800));
    assert_eq!(Dsd256.cadence_pour(44_100), Some(11_289_600));
    assert_eq!(Dsd64.cadence_pour(352_800), Some(2_822_400), "rapport 8");
    assert_eq!(
        Dsd64.cadence_pour(48_000),
        Some(3_072_000),
        "famille 48 kHz"
    );
    assert_eq!(Dsd64.cadence_pour(705_600), None, "rapport 4 refusé");
    assert_eq!(Dsd64.cadence_pour(32_000), None);
    assert_eq!(Dsd64.cadence_pour(0), None);
    assert!(ModulateurDsd::new(44_100, 0, Dsd64).is_err());
    assert!(ModulateurDsd::new(32_000, 2, Dsd64).is_err());
}

/// Signaux extrêmes du plan (0 dBFS, sinus de 20 kHz) et pires cas : aucune
/// remise à zéro, l'entrée du quantificateur reste bornée, à chaque cadence.
#[test]
fn stabilite_sur_signaux_extremes_a_chaque_cadence() {
    let trames = 16_384;
    let signaux: Vec<(&str, Vec<f64>)> = vec![
        ("sinus 1 kHz 0 dBFS", sinus(FS, 1_000.0, 1.0, trames, 2)),
        ("sinus 20 kHz 0 dBFS", sinus(FS, 20_000.0, 1.0, trames, 2)),
        // Fs/4 déphasé de 45° : échantillons à ±1,0, crête reconstruite à
        // +3 dB (intersample) : c'est le limiteur qui tient.
        (
            "intersample +3 dB",
            (0..trames)
                .flat_map(|i| {
                    let v = if (i / 2) % 2 == 0 { 1.0 } else { -1.0 };
                    [v, v]
                })
                .collect(),
        ),
        ("continu pleine échelle", vec![1.0; trames * 2]),
        (
            "carré 1 kHz 0 dBFS",
            (0..trames)
                .flat_map(|i| {
                    let v = if (i * 1_000 / FS as usize).is_multiple_of(2) {
                        1.0
                    } else {
                        -1.0
                    };
                    [v, v]
                })
                .collect(),
        ),
        ("bruit blanc pleine échelle", {
            let mut a = Alea(12345);
            (0..trames * 2).map(|_| 2.0 * a.uniforme() - 1.0).collect()
        }),
    ];
    for cadence in [CadenceDsd::Dsd64, CadenceDsd::Dsd128, CadenceDsd::Dsd256] {
        for (nom, pcm) in &signaux {
            let mut m = ModulateurDsd::new(FS, 2, cadence).unwrap();
            moduler(&mut m, pcm);
            assert_eq!(
                m.reinitialisations(),
                0,
                "{cadence:?}, {nom} : boucle instable"
            );
            assert!(
                m.entree_max() < 1.5,
                "{cadence:?}, {nom} : entrée du quantificateur {:.2}",
                m.entree_max()
            );
        }
    }
}

/// Contre-épreuve de la stabilité : le témoin ci-dessus discrimine. Avec un
/// NTF plus agressif (HINF 1,5, même ordre), le même pire cas (+3 dB
/// intersample, tenu à 60 % par le limiteur) fait partir la boucle.
#[test]
fn contre_epreuve_un_ntf_agressif_decroche_sur_le_meme_pire_cas() {
    let trames = 16_384;
    let pcm: Vec<f64> = (0..trames)
        .flat_map(|i| {
            let v = if (i / 2) % 2 == 0 { 1.0 } else { -1.0 };
            [v, v]
        })
        .collect();
    let mut m = ModulateurDsd::avec_reglages(FS, 2, CadenceDsd::Dsd64, 1.5, DITHER).unwrap();
    moduler(&mut m, &pcm);
    assert!(
        m.reinitialisations() > 0,
        "HINF 1,5 devait décrocher à 60 % (entrée max {:.2})",
        m.entree_max()
    );
}

/// Plancher de bruit (silence), bruit hors bande et SINAD à 0 dBFS, comparés
/// à l'étalon d'ordre 2 et au signal d'entrée (STF = 1, niveau de référence).
#[test]
fn plancher_de_bruit_et_comparaison_a_la_reference_dsd64() {
    let fs_dsd = 2_822_400.0;
    // Silence.
    let mut m = ModulateurDsd::new(FS, 2, CadenceDsd::Dsd64).unwrap();
    let n = trames_pour(&m, N_FFT);
    let silence = moduler(&mut m, &vec![0.0; n * 2]);
    let a = analyser(&fin_du_canal(&silence, 2, N_FFT), fs_dsd, None);
    assert!(
        a.bruit_bande_db < -115.0,
        "plancher {:.1} dB",
        a.bruit_bande_db
    );
    assert!(
        a.raie_max_db < -140.0,
        "ton de repos {:.1} dB",
        a.raie_max_db
    );

    // 1 kHz à 0 dBFS : raie à −6,02 dB (0 dB SACD), SINAD.
    let f0 = 1_000.0;
    let mut m = ModulateurDsd::new(FS, 2, CadenceDsd::Dsd64).unwrap();
    let octets = moduler(&mut m, &sinus(FS, f0, 1.0, n, 2));
    let bits = fin_du_canal(&octets, 2, N_FFT);
    let a = analyser(&bits, fs_dsd, Some(f0));
    assert!(
        (a.signal_db + 6.02).abs() < 0.05,
        "niveau {:.3} dB",
        a.signal_db
    );
    assert!(a.sinad_db > 100.0, "SINAD {:.1} dB", a.sinad_db);
    assert!(
        a.bruit_hors_bande_db < -10.0,
        "hors bande {:.1} dB",
        a.bruit_hors_bande_db
    );

    // Étalon d'ordre 2 sur la même entrée à la cadence DSD : il doit être
    // largement battu, sinon le banc ne distingue pas un bon modulateur.
    let entree: Vec<f64> = (0..N_FFT)
        .map(|i| 0.5 * (2.0 * std::f64::consts::PI * f0 * i as f64 / fs_dsd).sin())
        .collect();
    let r = analyser(&reference_ordre_2(&entree), fs_dsd, Some(f0));
    assert!(
        a.bruit_bande_db < r.bruit_bande_db - 30.0,
        "ordre 7 {:.1} dB contre ordre 2 {:.1} dB",
        a.bruit_bande_db,
        r.bruit_bande_db
    );
}

#[test]
fn plancher_de_bruit_dsd128_et_dsd256() {
    for (cadence, plafond) in [(CadenceDsd::Dsd128, -140.0), (CadenceDsd::Dsd256, -150.0)] {
        let mut m = ModulateurDsd::new(FS, 2, cadence).unwrap();
        let n = trames_pour(&m, N_FFT);
        let octets = moduler(&mut m, &sinus(FS, 1_000.0, 1.0, n, 2));
        let a = analyser(
            &fin_du_canal(&octets, 2, N_FFT),
            f64::from(m.cadence_dsd()),
            Some(1_000.0),
        );
        assert!(
            (a.signal_db + 6.02).abs() < 0.05,
            "{cadence:?} niveau {:.3}",
            a.signal_db
        );
        assert!(
            a.bruit_bande_db < plafond,
            "{cadence:?} bruit {:.1} dB",
            a.bruit_bande_db
        );
        assert_eq!(m.reinitialisations(), 0);
    }
}

/// Q9, exception DSD : le volume agit juste avant le modulateur, sans marche
/// (rampe de 10 ms), et la raie baisse exactement du gain demandé.
#[test]
fn volume_juste_avant_le_modulateur() {
    let fs_dsd = 2_822_400.0;
    let mut m = ModulateurDsd::new(FS, 2, CadenceDsd::Dsd64).unwrap();
    m.definir_volume(0.1); // −20 dB
    let n = trames_pour(&m, N_FFT);
    let octets = moduler(&mut m, &sinus(FS, 1_000.0, 1.0, n, 2));
    assert_eq!(m.volume(), 0.1, "rampe terminée");
    let a = analyser(&fin_du_canal(&octets, 2, N_FFT), fs_dsd, Some(1_000.0));
    assert!(
        (a.signal_db + 26.02).abs() < 0.05,
        "−20 dB : raie à {:.3} dB",
        a.signal_db
    );

    // Durée de la rampe : 10 ms à la cadence DSD, au bout de 441 trames PCM.
    let mut m = ModulateurDsd::new(FS, 2, CadenceDsd::Dsd64).unwrap();
    m.definir_volume(0.0);
    let mut sortie = vec![0u8; m.octets_pour(1)];
    let mut trames = 0;
    while m.volume() > 0.0 {
        m.traiter(&[0.0, 0.0], &mut sortie).unwrap();
        trames += 1;
        assert!(trames <= 442, "rampe trop longue");
    }
    assert!(
        trames >= 440,
        "rampe trop courte ({trames} trames) : marche audible"
    );
    // Volume invalide ignoré.
    m.definir_volume(f64::NAN);
    m.definir_volume(-1.0);
    m.traiter(&[0.0, 0.0], &mut sortie).unwrap();
    assert_eq!(m.volume(), 0.0);
}

/// Même entrée, mêmes octets, quel que soit le découpage en blocs.
#[test]
fn deterministe_et_indifferent_au_decoupage() {
    let pcm = sinus(FS, 3_000.0, 0.8, 4_000, 2);
    let mut a = ModulateurDsd::new(FS, 2, CadenceDsd::Dsd64).unwrap();
    let un_bloc = moduler(&mut a, &pcm);
    let mut b = ModulateurDsd::new(FS, 2, CadenceDsd::Dsd64).unwrap();
    let mut par_morceaux = Vec::new();
    for morceau in pcm.chunks(2 * 37) {
        par_morceaux.extend(moduler(&mut b, morceau));
    }
    assert_eq!(un_bloc, par_morceaux);
    // Deux canaux, deux graines : pas le même bruit sur les deux canaux.
    let g = bits_du_canal(&un_bloc, 2, 0);
    let d = bits_du_canal(&un_bloc, 2, 1);
    assert_ne!(g, d);
}

/// Un NaN en entrée ne laisse aucun NaN dans la boucle : il est compté comme
/// une remise à zéro et la modulation continue, stable.
#[test]
fn un_nan_ne_contamine_pas_la_boucle() {
    let mut m = ModulateurDsd::new(FS, 2, CadenceDsd::Dsd64).unwrap();
    let mut pcm = sinus(FS, 1_000.0, 0.5, 2_000, 2);
    pcm[1_000] = f64::NAN;
    moduler(&mut m, &pcm);
    assert!(m.reinitialisations() > 0);
    let avant = m.reinitialisations();
    moduler(&mut m, &sinus(FS, 1_000.0, 0.5, 20_000, 2));
    // L'historique du suréchantillonneur porte le NaN pendant
    // COEFFICIENTS_PAR_PHASE trames ; ensuite plus rien.
    let apres_purge = m.reinitialisations();
    moduler(&mut m, &sinus(FS, 1_000.0, 0.5, 20_000, 2));
    assert_eq!(
        m.reinitialisations(),
        apres_purge,
        "instabilité durable après {avant}"
    );
}

/// Emballage : MSB = plus ancien. Un continu positif à fond donne une
/// majorité de bits à 1, un négatif une majorité de 0.
#[test]
fn emballage_et_signe() {
    let mut m = ModulateurDsd::new(FS, 1, CadenceDsd::Dsd64).unwrap();
    let plus = moduler(&mut m, &vec![1.0; 4_000]);
    let uns: u32 = plus[plus.len() / 2..].iter().map(|o| o.count_ones()).sum();
    let total = (plus.len() - plus.len() / 2) as u32 * 8;
    // 50 % de modulation : 75 % de bits à 1.
    let part = f64::from(uns) / f64::from(total);
    assert!((part - 0.75).abs() < 0.01, "part de 1 : {part}");
}

/// Le dither casse les tons de repos d'un modulateur à 1 bit. Entrée
/// continue faible (3e-3, soit −50 dBFS) : sans dither, la boucle tombe dans
/// un cycle limite qui sort une raie à −122 dB dans la bande audio ; avec le
/// dither, plus de raie au-dessus de −130 dB (mesuré : −136,6 dB).
/// CONTRE-ÉPREUVE dans le même test : dither à 0, même entrée, la raie
/// dépasse le seuil.
#[test]
fn le_dither_casse_les_tons_de_repos() {
    const SEUIL_DB: f64 = -130.0;
    let fs_dsd = 2_822_400.0;
    let raie = |dither: f64| {
        let mut m = ModulateurDsd::avec_reglages(FS, 2, CadenceDsd::Dsd64, HINF, dither).unwrap();
        let n = trames_pour(&m, N_FFT);
        let octets = moduler(&mut m, &vec![3e-3; n * 2]);
        analyser(&fin_du_canal(&octets, 2, N_FFT), fs_dsd, None).raie_max_db
    };
    let avec = raie(DITHER);
    let sans = raie(0.0);
    assert!(
        avec < SEUIL_DB,
        "avec dither : raie de repos à {avec:.1} dB (sans : {sans:.1} dB)"
    );
    assert!(
        sans > SEUIL_DB,
        "sans dither, aucune raie de repos au-dessus de {SEUIL_DB} dB ({sans:.1} dB) : \
         le témoin ne discrimine pas (avec : {avec:.1} dB)"
    );
}

/// `reinitialiser()` : pas de marche (le volume et sa rampe sont gardés), et
/// reprise propre — la même entrée rend les MÊMES bits qu'un modulateur neuf
/// remis à zéro au même volume (boucles, historiques ET générateurs du dither
/// revenus à leur état de départ).
#[test]
fn reinitialiser_reprend_proprement_sans_marche() {
    let amener_au_volume = |m: &mut ModulateurDsd| {
        m.definir_volume(0.5);
        let silence = vec![0.0; 2 * 1_000];
        moduler(m, &silence);
        assert_eq!(m.volume(), 0.5);
    };
    let b = sinus(FS, 1_000.0, 0.7, 3_000, 2);

    // Un modulateur qui a déjà travaillé (autre signal), puis remis à zéro.
    let mut use_ = ModulateurDsd::new(FS, 2, CadenceDsd::Dsd64).unwrap();
    amener_au_volume(&mut use_);
    moduler(&mut use_, &sinus(FS, 7_000.0, 0.9, 5_000, 2));
    use_.reinitialiser();
    assert_eq!(use_.volume(), 0.5, "la remise à zéro a changé le volume");

    // Le même, neuf, remis à zéro au même volume.
    let mut neuf = ModulateurDsd::new(FS, 2, CadenceDsd::Dsd64).unwrap();
    amener_au_volume(&mut neuf);
    neuf.reinitialiser();

    assert_eq!(moduler(&mut use_, &b), moduler(&mut neuf, &b));
    assert_eq!(use_.reinitialisations(), 0);
}

/// Le volume ne dépasse jamais 0 dB.
#[test]
fn le_volume_est_plafonne_a_zero_db() {
    let mut m = ModulateurDsd::new(FS, 2, CadenceDsd::Dsd64).unwrap();
    m.definir_volume(2.0);
    moduler(&mut m, &vec![0.0; 2 * 1_000]);
    assert_eq!(m.volume(), 1.0);
}

/// Les deux refus de `traiter` disent ce qui ne va pas.
#[test]
fn les_refus_de_traiter_sont_explicites() {
    let mut m = ModulateurDsd::new(FS, 2, CadenceDsd::Dsd64).unwrap();
    let mut sortie = vec![0u8; m.octets_pour(10)];
    assert_eq!(
        m.traiter(&[0.0; 3], &mut sortie),
        Err(ErreurModulateur::TramesIncompletes {
            echantillons: 3,
            canaux: 2
        })
    );
    let besoin = m.octets_pour(10);
    assert_eq!(
        m.traiter(&[0.0; 20], &mut sortie[..besoin - 1]),
        Err(ErreurModulateur::TamponTropCourt {
            besoin,
            recu: besoin - 1
        })
    );
}

/// `fixer_volume` : le PREMIER échantillon est déjà au niveau demandé. Avec
/// `definir_volume` seul, le modulateur partirait de 0 dB et descendrait en
/// 10 ms (contre-épreuve : la même entrée, rampe depuis 1,0).
#[test]
fn fixer_volume_pose_le_niveau_des_le_premier_echantillon() {
    let densite = |octets: &[u8]| {
        let uns: u32 = octets.iter().step_by(2).map(|o| o.count_ones()).sum();
        let n = (octets.len() / 2) as f64 * 8.0;
        (2.0 * f64::from(uns) - n) / n
    };
    // 5 ms de continu pleine échelle.
    let pcm = vec![1.0; 2 * 220];
    let mut m = ModulateurDsd::new(FS, 2, CadenceDsd::Dsd64).unwrap();
    m.fixer_volume(0.0);
    let d = densite(&moduler(&mut m, &pcm));
    assert!(d.abs() < 0.02, "sourdine dès le départ : {d}");
    let mut rampe = ModulateurDsd::new(FS, 2, CadenceDsd::Dsd64).unwrap();
    rampe.definir_volume(0.0);
    let d = densite(&moduler(&mut rampe, &pcm));
    assert!(d > 0.1, "contre-épreuve : la rampe part de 0 dB ({d})");
}
