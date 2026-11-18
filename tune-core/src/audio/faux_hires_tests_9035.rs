//! #9035 — la raison d'un « douteux » dit ce qui a été MESURÉ. Une coupure
//! sous 20 kHz ne suffit pas à écrire « codec avec perte » : il faut aussi
//! les trous intermittents d'un encodeur ([`trous_de_codec`]). Témoins sur des
//! signaux SYNTHÉTIQUES produits en mémoire (aucun fichier, aucun ffmpeg,
//! aucun son joué) :
//!
//! * un master SANS PERTE filtré à 18 kHz par un filtre raide et fixe :
//!   douteux, coupure à 18 kHz, jamais « codec » ;
//! * un CD suréchantillonné au spectre SOMBRE (l'aigu décroît dès 2 kHz) :
//!   toujours `BandeDeCd` vers 21 kHz, pas de coupure inventée plus bas ;
//! * un « MP3 à 128 kbit/s converti en FLAC 24/96 », simulé : passe-bas à
//!   16 kHz et bandes de l'aigu vidées par intermittence d'une trame courte à
//!   l'autre, comme un encodeur à court de bits — toujours douteux, et cette
//!   fois avec la signature du codec.

use super::tests::{quantifier, rose};
use super::*;
use realfft::RealFftPlanner;
use realfft::num_complex::Complex;

/// Longueur des signaux synthétisés d'un bloc : 2^18 échantillons (2,7 s à
/// 96 kHz).
const N: usize = 1 << 18;

struct Lcg(u64);
impl Lcg {
    fn suivant(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 11) as f64) / ((1u64 << 53) as f64)
    }
}

/// Un signal mono d'amplitude spectrale `gain(f)` (phases aléatoires),
/// normalisé à -6 dBFS de crête.
fn par_spectre(sample_rate: u32, graine: u64, gain: impl Fn(f64) -> f64) -> Vec<f32> {
    let mut plan = RealFftPlanner::<f32>::new();
    let inverse = plan.plan_fft_inverse(N);
    let mut spectre = inverse.make_input_vec();
    let mut alea = Lcg(graine);
    let hz_par_raie = sample_rate as f64 / N as f64;
    for (k, c) in spectre.iter_mut().enumerate() {
        let f = k as f64 * hz_par_raie;
        let phase = alea.suivant() * std::f64::consts::TAU;
        let a = if f >= 20.0 { gain(f) } else { 0.0 };
        *c = Complex::new((a * phase.cos()) as f32, (a * phase.sin()) as f32);
    }
    let n = spectre.len();
    spectre[n - 1].im = 0.0;
    let mut sortie = inverse.make_output_vec();
    inverse.process(&mut spectre, &mut sortie).unwrap();
    let crete = sortie.iter().fold(0f32, |m, s| m.max(s.abs()));
    sortie.iter().map(|s| s / crete * 0.5).collect()
}

/// Un CODEC AVEC PERTE simulé, trame par trame (1024 points, pas de 512,
/// fenêtres racine de Hann à l'analyse et à la synthèse : reconstruction
/// exacte sans retouche) : tout ce qui dépasse `coupure_hz` est retiré
/// (passe-bas de l'encodeur) et, dans les 5 kHz sous la coupure, des bandes
/// de 1,4 kHz s'éteignent par intermittence, pendant 3 à 8 trames (16 à
/// 43 ms à 96 kHz) — les bandes qu'un encodeur à court de bits vide d'un
/// granule à l'autre (la « sfb21 » du MP3 à 128 kbit/s).
fn codec_simule(signal: &[f32], sample_rate: u32, coupure_hz: f64, graine: u64) -> Vec<f32> {
    const T: usize = 1024;
    const PAS: usize = T / 2;
    let mut plan = RealFftPlanner::<f32>::new();
    let avant = plan.plan_fft_forward(T);
    let arriere = plan.plan_fft_inverse(T);
    let fenetre: Vec<f32> = (0..T)
        .map(|i| (0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / T as f32).cos()).sqrt())
        .collect();
    let hz_par_raie = sample_rate as f64 / T as f64;
    let largeur = 1_400.0;
    let bas = coupure_hz - 5_000.0;
    let nb = ((coupure_hz - bas) / largeur).ceil() as usize;
    let mut restant = vec![0usize; nb];
    let mut alea = Lcg(graine);
    let mut sortie = vec![0f32; signal.len()];
    let mut entree = avant.make_input_vec();
    let mut spectre = avant.make_output_vec();
    let mut temps = arriere.make_output_vec();
    let mut debut = 0;
    while debut + T <= signal.len() {
        for b in restant.iter_mut() {
            if *b > 0 {
                *b -= 1;
            } else if alea.suivant() < 0.06 {
                *b = 3 + (alea.suivant() * 6.0) as usize;
            }
        }
        for ((e, &s), &w) in entree
            .iter_mut()
            .zip(&signal[debut..debut + T])
            .zip(&fenetre)
        {
            *e = s * w;
        }
        avant.process(&mut entree, &mut spectre).unwrap();
        for (k, c) in spectre.iter_mut().enumerate() {
            let f = k as f64 * hz_par_raie;
            let eteinte = f >= bas && f < coupure_hz && {
                let b = (((f - bas) / largeur) as usize).min(nb - 1);
                restant[b] > 0
            };
            if f >= coupure_hz || eteinte {
                *c = Complex::new(0.0, 0.0);
            }
        }
        spectre[0].im = 0.0;
        spectre[T / 2].im = 0.0;
        arriere.process(&mut spectre, &mut temps).unwrap();
        for (i, (&x, &w)) in temps.iter().zip(&fenetre).enumerate() {
            sortie[debut + i] += x * w / T as f32;
        }
        debut += PAS;
    }
    sortie
}

/// 🔴 TÉMOIN de l'issue — un master SANS PERTE filtré à 18 kHz par un filtre
/// raide et fixe, en 24/96 : douteux (rien au-dessus de 18 kHz), la coupure
/// est mesurée vers 18 kHz, et la raison n'est PAS « codec avec perte ».
#[test]
fn un_master_sans_perte_filtre_a_18_khz_n_est_pas_un_codec_9035() {
    for (sample_rate, bits) in [(96_000, 24), (48_000, 24)] {
        let pcm = quantifier(&rose(sample_rate, 18_000.0, 901), bits);
        let m = mesurer(&pcm, 1, sample_rate, bits);
        let v = juger(&m);
        assert!(
            matches!(v, Verdict::Douteux(Raison::CoupureSpectrale { .. })),
            "{sample_rate} Hz : filtré à 18 kHz sans trou de codec — {v:?} {m:?}"
        );
        assert_eq!(v.code_raison(), Some("coupure_spectrale"));
        assert!(!m.signature_codec, "{m:?}");
        let f = m.coupure_hz.expect("la coupure");
        assert!((f - 18_000.0).abs() < 400.0, "coupure {f:.0} Hz : {m:?}");
        let trous = m
            .part_trouee
            .expect("l'aigu sous 18 kHz est assez fourni pour juger");
        assert!(trous < 0.001, "aucun trou dans un filtre fixe : {trous}");
    }
}

/// Un master SOMBRE : spectre rose jusqu'à `coude_hz`, puis 24 dB par octave
/// de plus, coupé net à `bande_hz` (le filtre d'un CD).
fn sombre_coupe(sample_rate: u32, coude_hz: f64, bande_hz: f64, graine: u64) -> Vec<f32> {
    par_spectre(sample_rate, graine, |f| {
        if f < bande_hz {
            (1.0 / f.sqrt()) / (1.0 + (f / coude_hz).powi(2)).powi(2)
        } else {
            0.0
        }
    })
}

/// 🔴 TÉMOIN sur le bord — un CD suréchantillonné en 24/96 dont l'aigu
/// décroît dès 2 kHz (24 dB par octave de plus que le rose) : le niveau près
/// du filtre est à plus de 20 dB sous la médiane de la bande passante. La
/// coupure retenue doit rester celle du filtre (21 kHz) et la raison
/// `BandeDeCd`, pas une coupure inventée sous 20 kHz.
#[test]
fn un_cd_surechantillonne_sombre_reste_bande_de_cd_9035() {
    let pcm = quantifier(&sombre_coupe(96_000, 2_000.0, 21_000.0, 902), 24);
    let m = mesurer(&pcm, 1, 96_000, 24);
    let v = juger(&m);
    assert!(
        matches!(v, Verdict::Douteux(Raison::BandeDeCd { .. })),
        "CD suréchantillonné sombre : {v:?} {m:?}"
    );
    let f = m.coupure_hz.expect("la coupure");
    assert!((f - 21_000.0).abs() < 400.0, "coupure {f:.0} Hz : {m:?}");
}

/// 🔴 TÉMOIN — un « MP3 à 128 kbit/s converti en FLAC 24/96 », simulé :
/// passe-bas à 16 kHz et bandes vidées par intermittence sous la coupure.
/// Toujours douteux, coupure vers 16 kHz, et la signature du codec est
/// mesurée : la raison peut dire « codec avec perte ».
#[test]
fn un_mp3_128_converti_en_flac_96_khz_reste_douteux_avec_sa_signature_9035() {
    let source = par_spectre(96_000, 903, |f| 1.0 / f.sqrt());
    let pcm = quantifier(&codec_simule(&source, 96_000, 16_000.0, 904), 24);
    let m = mesurer(&pcm, 1, 96_000, 24);
    let v = juger(&m);
    assert!(
        matches!(v, Verdict::Douteux(Raison::SignatureDeCodec { .. })),
        "MP3 simulé : {v:?} part trouée {:?} — {m:?}",
        m.part_trouee
    );
    assert_eq!(v.code_raison(), Some("signature_de_codec"));
    let f = m.coupure_hz.expect("la coupure");
    assert!((f - 16_000.0).abs() < 400.0, "coupure {f:.0} Hz : {m:?}");
}

/// CONTRE-ÉPREUVE du témoin précédent — la même source, le même passe-bas à
/// 16 kHz, mais SANS les trous : la signature disparaît, la raison redevient
/// la coupure seule. C'est bien le trou intermittent qui fait « codec », pas
/// la fréquence.
#[test]
fn le_meme_passe_bas_sans_trous_n_est_qu_une_coupure_9035() {
    let source = par_spectre(
        96_000,
        903,
        |f| if f < 16_000.0 { 1.0 / f.sqrt() } else { 0.0 },
    );
    let pcm = quantifier(&source, 24);
    let m = mesurer(&pcm, 1, 96_000, 24);
    assert!(
        matches!(juger(&m), Verdict::Douteux(Raison::CoupureSpectrale { .. })),
        "{m:?}"
    );
    assert!(!m.signature_codec, "{m:?}");
}

/// Les codes de raison publiés : l'ancien `coupure_de_codec`, rangé sans
/// signature mesurée, se publie `coupure_spectrale`.
#[test]
fn l_ancien_code_codec_se_publie_coupure_spectrale_9035() {
    assert_eq!(raison_publiee("coupure_de_codec"), "coupure_spectrale");
    for code in [
        "coupure_spectrale",
        "signature_de_codec",
        "bande_de_cd",
        "bits_de_remplissage",
    ] {
        assert_eq!(raison_publiee(code), code);
    }
}

/// Passe-bas idéal à `coupure_hz` sur tout le signal (FFT de sa longueur,
/// une puissance de deux) : le filtre fixe d'un mastering.
fn passe_bas(signal: &[f32], sample_rate: u32, coupure_hz: f64) -> Vec<f32> {
    // En f64 : l'arrondi d'une FFT f32 de 2^18 points laisse au-dessus de la
    // coupure un plancher d'erreur qui n'est pas plat jusqu'à Nyquist, et y
    // dessinerait une falaise d'artefact.
    let n = signal.len();
    let mut plan = RealFftPlanner::<f64>::new();
    let avant = plan.plan_fft_forward(n);
    let arriere = plan.plan_fft_inverse(n);
    let mut entree: Vec<f64> = signal.iter().map(|&s| s as f64).collect();
    let mut spectre = avant.make_output_vec();
    avant.process(&mut entree, &mut spectre).unwrap();
    let hz_par_raie = sample_rate as f64 / n as f64;
    for (k, c) in spectre.iter_mut().enumerate() {
        if k as f64 * hz_par_raie >= coupure_hz {
            *c = Complex::new(0.0, 0.0);
        }
    }
    spectre[0].im = 0.0;
    let m = spectre.len();
    spectre[m - 1].im = 0.0;
    let mut sortie = arriere.make_output_vec();
    arriere.process(&mut spectre, &mut sortie).unwrap();
    sortie.iter().map(|s| (s / n as f64) as f32).collect()
}

/// Normalise à -6 dBFS de crête, ajoute un bruit de fond rose à -60 dB (le
/// souffle d'une prise réelle) et filtre le tout à 18 kHz : un master SANS
/// PERTE filtré au mastering, dont il faut juger les trous.
fn masterise(musique: Vec<f32>, graine: u64) -> Vec<i32> {
    let crete = musique.iter().fold(0f32, |m, s| m.max(s.abs())).max(1e-9);
    let fond = par_spectre(96_000, graine, |f| 1.0 / f.sqrt());
    let mix: Vec<f32> = musique
        .iter()
        .zip(&fond)
        .map(|(m, b)| m / crete * 0.5 + b * 1e-3)
        .collect();
    let filtre = passe_bas(&mix, 96_000, 18_000.0);
    let crete = filtre.iter().fold(0f32, |m, s| m.max(s.abs())).max(1e-9);
    // Un dither TPDF d'un LSB, comme en pose tout convertisseur : sans lui,
    // l'arrondi d'un signal synthétique très lisse n'est pas un bruit blanc et
    // creuse le spectre près de Nyquist (une falaise d'artefact).
    let lsb = 1.0 / (1u32 << 23) as f32;
    let mut alea = Lcg(graine ^ 0x9035);
    let dither: Vec<f32> = filtre
        .iter()
        .map(|s| s / crete * 0.5 + (alea.suivant() as f32 - alea.suivant() as f32) * lsb)
        .collect();
    quantifier(&dither, 24)
}

/// Le verdict d'un témoin NON codec : la recherche des trous, menée sous la
/// coupure de 18 kHz du master, juge (assez de trames et de bandes) et ne
/// conclut PAS au codec ; le verdict complet ne dit jamais « codec » non plus.
/// La recherche est appelée directement : sur un signal synthétique très
/// dynamique, l'étape de la falaise (antérieure à #9035) peut retenir une
/// falaise d'artefact près de Nyquist et ne jamais lancer la recherche — ce
/// qui serait un faux NÉGATIF, pas l'objet de ces témoins.
fn pas_de_signature(nom: &str, pcm: &[i32]) {
    let trous = trous_de_codec(&[(0.0, pcm)], 1, 96_000, 24, 18_000.0)
        .unwrap_or_else(|| panic!("{nom} : la recherche des trous doit juger"));
    assert!(
        !trous.signature(),
        "{nom} : pas de signature de codec attendue — {trous:?}"
    );
    let m = mesurer(pcm, 1, 96_000, 24);
    assert!(
        !matches!(juger(&m), Verdict::Douteux(Raison::SignatureDeCodec { .. })),
        "{nom} : {m:?}"
    );
}

/// 🔴 TÉMOIN tonal — des notes aiguës (fondamentales de 1,2 à 3 kHz) dont les
/// partiels montent jusqu'à 17,5 kHz, chacun avec son déclin (les plus hauts
/// s'éteignent les premiers) et une attaque bruitée : l'aigu s'allume et
/// s'éteint d'une note à l'autre, comme un clavecin ou des cordes. Pas de
/// signature de codec.
#[test]
fn des_partiels_aigus_qui_s_eteignent_ne_sont_pas_un_codec_9035() {
    let sr = 96_000f32;
    let mut alea = Lcg(905);
    let note = (0.15 * sr) as usize;
    let mut musique = vec![0f32; N];
    let mut debut = 0;
    while debut < N {
        let f0 = 1_200.0 + alea.suivant() as f32 * 1_800.0;
        let fin = (debut + note).min(N);
        for (i, s) in musique[debut..fin].iter_mut().enumerate() {
            let t = i as f32 / sr;
            let mut x = 0f32;
            let mut k = 1;
            while f0 * k as f32 <= 17_500.0 {
                let tau = 0.3 / k as f32;
                x +=
                    (std::f32::consts::TAU * f0 * k as f32 * t).sin() * (-t / tau).exp() / k as f32;
                k += 1;
            }
            *s = x;
        }
        // L'attaque : 5 ms de bruit large bande.
        for s in musique[debut..(debut + (0.005 * sr) as usize).min(N)].iter_mut() {
            *s += (alea.suivant() as f32 - 0.5) * 0.5;
        }
        debut = fin;
    }
    pas_de_signature("partiels aigus", &masterise(musique, 906));
}

/// 🔴 TÉMOIN non stationnaire — une musique sombre (rien au-dessus de 8 kHz)
/// et un charleston toutes les 250 ms, qui décroît en 40 ms : l'aigu entier
/// apparaît et disparaît. Pas de signature de codec.
#[test]
fn un_charleston_intermittent_n_est_pas_un_codec_9035() {
    let sr = 96_000f32;
    let fond = par_spectre(
        96_000,
        907,
        |f| if f < 8_000.0 { 1.0 / f.sqrt() } else { 0.0 },
    );
    let mut alea = Lcg(908);
    let periode = (0.25 * sr) as usize;
    let musique: Vec<f32> = fond
        .iter()
        .enumerate()
        .map(|(i, &x)| {
            let t = (i % periode) as f32 / sr;
            let coup = (alea.suivant() as f32 - 0.5) * 1.5 * (-t / 0.04).exp();
            x + coup
        })
        .collect();
    pas_de_signature("charleston", &masterise(musique, 909));
}

/// 🔴 TÉMOIN — un roll-off DOUX (12 dB par octave de plus dès 6 kHz, une
/// vieille bande), dont le niveau monte et descend de 20 dB par passages.
/// Pas de signature de codec.
#[test]
fn un_roll_off_doux_qui_varie_n_est_pas_un_codec_9035() {
    let base = par_spectre(96_000, 910, |f| {
        (1.0 / f.sqrt()) / (1.0 + (f / 6_000.0).powi(2))
    });
    let passage = 96_000 / 2;
    let musique: Vec<f32> = base
        .iter()
        .enumerate()
        .map(|(i, &x)| {
            let g = if (i / passage) % 2 == 0 { 1.0 } else { 0.1 };
            x * g
        })
        .collect();
    pas_de_signature("roll-off doux", &masterise(musique, 911));
}
