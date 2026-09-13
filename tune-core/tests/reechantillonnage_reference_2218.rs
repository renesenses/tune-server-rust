//! T10 de #2218 — le rééchantillonneur mesuré contre une **référence
//! indépendante**, témoins seulement.
//!
//! Le bilan du 12/09 : « le rééchantillonnage n'a aucune référence externe ;
//! il reste gardé par les empreintes de R1, contre la version d'avant, pas
//! contre une vérité ». `flac -d` sert de référence au décodage (T1),
//! `wvunpack` au DSD (T3) ; rien ne servait de référence à
//! `rubato_resample_chunk` / `rubato_resample_track`
//! (`tune-core/src/audio/resample.rs`), et le banc AES17 (#3313) mesure des
//! résidus contre lui-même. Shrek n'a ni sox ni ffmpeg : la référence est
//! écrite ICI, lente et exacte.
//!
//! # La référence
//!
//! Interpolation sinc à fenêtre de Kaiser (β = 14, soit ≈ 136 dB de réjection
//! théorique), 2 × 512 + 1 = 1025 coefficients, tout en `f64`. Pour un rapport
//! rationnel L/M (`vers/de` réduit), l'instant du n-ième échantillon de sortie
//! est exactement `n·M/L` échantillons d'entrée : la table est **polyphase
//! exacte** (L phases, une par reste de `n·M mod L`), sans aucune
//! interpolation entre phases — c'est ce qui la rend exacte, pas seulement
//! précise. Un décalage fractionnaire optionnel `δ` (en échantillons de
//! sortie) est intégré à la table pour comparer Tune **après** retrait de son
//! délai résiduel. La référence est à phase linéaire et à délai nul.
//!
//! Elle est prouvée contre elle-même AVANT de servir (`reference_*`) : un
//! sinus 1 kHz 44,1 → 48 kHz ressort avec THD+N < −120 dB ; un ton au-dessus
//! de Nyquist de sortie 96 → 48 kHz ressort sous −120 dB ; le gain continu
//! vaut 1 à 1e−7 près (5e−9 mesuré, le noyau est tronqué).
//!
//! # Les mesures, par rapport
//!
//! 44,1 → 48, 48 → 44,1, 44,1 → 96, 96 → 48, 44,1 → 192, 176,4 → 48 (la
//! sortie PCM de DSD64, `dsd_to_pcm::choose_output_rate`, vers une zone à
//! 48 kHz) et 192 → 44,1 (le seul rapport qui demande 512 coefficients à
//! `parametres_sinc`). Sur chacun : erreur RMS
//! contre la référence (sinus 1 kHz, balayage 20 Hz → 20 kHz), réponse en
//! fréquence par impulsion (bande à −0,1 dB, réjection des images), réjection
//! des repliements par un ton au-dessus de Nyquist de sortie, délai résiduel
//! (par la phase à 1 kHz et à 10 kHz : phase linéaire ?), bords (64 premières
//! et dernières trames), vidage du flux (`flush`) et identité blocs / piste.
//!
//! Chaque témoin AFFIRME la valeur mesurée aujourd'hui ; ceux marqués
//! `#[ignore = "défaut connu : …"]` affirment ce qu'un rééchantillonneur
//! audiophile doit rendre (bande 20 kHz à −0,1 dB, réjection > 100 dB, erreur
//! RMS < −100 dB — le plancher d'un mot de 24 bits est −144 dBFS).
//!
//! Portes PUBLIQUES : `audio::resample::{new_streaming_resampler,
//! rubato_resample_chunk, rubato_resample_track}` et `rubato::Resampler` pour
//! `output_delay()`. `tune-core` porte `autotests = false` : ce fichier est
//! une cible `[[test]]` du manifeste, sinon il ne serait jamais compilé.

use std::f64::consts::PI;
use std::sync::OnceLock;

use rubato::Resampler;
use tune_core::audio::resample::{
    new_streaming_resampler, parametres_sinc, rubato_resample_chunk, rubato_resample_track,
};

// ───────────────────────────── la référence ─────────────────────────────

/// Demi-longueur du noyau : 512 coefficients de chaque côté.
const DEMI: usize = 512;
/// β de Kaiser : réjection théorique A ≈ β/0,1102 + 8,7 ≈ 136 dB.
const BETA: f64 = 14.0;
/// Largeur de transition d'une fenêtre de Kaiser, en cycles par échantillon
/// d'entrée : Δf = (A − 8) / (2,285 · 2π · N), N = 1024.
const TRANSITION: f64 = (BETA / 0.1102 + 8.7 - 8.0) / (2.285 * 2.0 * PI * (2 * DEMI) as f64);

fn pgcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { pgcd(b, a % b) }
}

/// Bessel modifiée de première espèce, ordre 0, par sa série (converge en
/// ~40 termes pour x ≤ 14, tout en f64).
fn bessel_i0(x: f64) -> f64 {
    let demi_x = x / 2.0;
    let mut somme = 1.0;
    let mut terme = 1.0;
    for k in 1..80 {
        terme *= (demi_x / k as f64) * (demi_x / k as f64);
        somme += terme;
        if terme < 1e-18 * somme {
            break;
        }
    }
    somme
}

fn sinc(x: f64) -> f64 {
    if x.abs() < 1e-12 {
        1.0
    } else {
        (PI * x).sin() / (PI * x)
    }
}

struct Reference {
    l: usize,
    m: usize,
    /// L phases × (2·DEMI + 1) coefficients.
    table: Vec<f64>,
}

impl Reference {
    /// `decalage` : retard en échantillons de SORTIE que la référence
    /// reproduit (0 = délai nul).
    fn new(de: u32, vers: u32, decalage: f64) -> Self {
        let g = pgcd(de as usize, vers as usize);
        let l = vers as usize / g;
        let m = de as usize / g;
        let rapport = vers as f64 / de as f64;
        let coupure = 0.5 * rapport.min(1.0) - TRANSITION / 2.0;
        let i0_beta = bessel_i0(BETA);
        let d_entree = decalage / rapport;
        let taps = 2 * DEMI + 1;
        let mut table = vec![0.0; l * taps];
        for p in 0..l {
            let frac = p as f64 / l as f64 - d_entree;
            for (j, coef) in table[p * taps..(p + 1) * taps].iter_mut().enumerate() {
                let u = frac - (j as f64 - DEMI as f64);
                let r = u / DEMI as f64;
                if r.abs() >= 1.0 {
                    continue;
                }
                let fenetre = bessel_i0(BETA * (1.0 - r * r).sqrt()) / i0_beta;
                *coef = 2.0 * coupure * sinc(2.0 * coupure * u) * fenetre;
            }
        }
        Self { l, m, table }
    }

    fn trames_de_sortie(&self, trames_entree: usize) -> usize {
        (trames_entree as f64 * self.l as f64 / self.m as f64).round() as usize
    }

    fn gain_continu(&self) -> f64 {
        let taps = 2 * DEMI + 1;
        self.table[..taps].iter().sum()
    }

    fn appliquer(&self, x: &[f64]) -> Vec<f64> {
        let taps = 2 * DEMI + 1;
        let n_sortie = self.trames_de_sortie(x.len());
        let mut y = vec![0.0; n_sortie];
        for (n, out) in y.iter_mut().enumerate() {
            let q0 = (n * self.m) / self.l;
            let p = (n * self.m) % self.l;
            let coefs = &self.table[p * taps..(p + 1) * taps];
            // x[q0 + j − DEMI] pour j ∈ [0, taps) ; hors du signal = 0.
            let debut = q0.saturating_sub(DEMI);
            let fin = (q0 + DEMI + 1).min(x.len());
            let j0 = debut + DEMI - q0;
            let mut acc = 0.0;
            for (xi, c) in x[debut..fin].iter().zip(&coefs[j0..]) {
                acc += xi * c;
            }
            *out = acc;
        }
        y
    }
}

// ───────────────────────── signaux et mètres ─────────────────────────

const AMPLITUDE: f64 = 0.5;
const DUREE_S: f64 = 1.0;

fn sinus(sr: u32, f: f64, duree_s: f64) -> Vec<f32> {
    let n = (sr as f64 * duree_s).round() as usize;
    (0..n)
        .map(|i| (AMPLITUDE * (2.0 * PI * f * i as f64 / sr as f64).sin()) as f32)
        .collect()
}

/// Balayage linéaire 20 Hz → 20 kHz sur `duree_s`.
fn balayage(sr: u32, duree_s: f64) -> Vec<f32> {
    let n = (sr as f64 * duree_s).round() as usize;
    let (f0, f1) = (20.0, 20_000.0);
    (0..n)
        .map(|i| {
            let t = i as f64 / sr as f64;
            let phase = 2.0 * PI * (f0 * t + (f1 - f0) * t * t / (2.0 * duree_s));
            (AMPLITUDE * phase.sin()) as f32
        })
        .collect()
}

fn impulsion(sr: u32, duree_s: f64, position: usize) -> Vec<f32> {
    let n = (sr as f64 * duree_s).round() as usize;
    let mut v = vec![0.0f32; n];
    v[position] = 1.0;
    v
}

fn en_f64(v: &[f32]) -> Vec<f64> {
    v.iter().map(|&s| s as f64).collect()
}

fn rms(v: &[f64]) -> f64 {
    (v.iter().map(|s| s * s).sum::<f64>() / v.len().max(1) as f64).sqrt()
}

fn db(x: f64) -> f64 {
    20.0 * x.max(1e-300).log10()
}

/// Erreur RMS de `mesure` contre `reference` sur `[debut, fin)`, en dB
/// relatifs au RMS de la référence sur le même intervalle.
fn erreur_rms_db(mesure: &[f64], reference: &[f64], debut: usize, fin: usize) -> f64 {
    let err: Vec<f64> = (debut..fin).map(|i| mesure[i] - reference[i]).collect();
    db(rms(&err) / rms(&reference[debut..fin]))
}

/// Ajustement exact `a·sin(ωt) + b·cos(ωt)` par moindres carrés (équations
/// normales complètes : valable pour un nombre non entier de périodes).
/// Rend (amplitude, phase, RMS du résidu).
fn ajuster_sinus(v: &[f64], sr: u32, f: f64, debut: usize, fin: usize) -> (f64, f64, f64) {
    let w = 2.0 * PI * f / sr as f64;
    let (mut ss, mut sc, mut cc, mut xs, mut xc) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (i, &vi) in v.iter().enumerate().take(fin).skip(debut) {
        let (s, c) = (w * i as f64).sin_cos();
        ss += s * s;
        sc += s * c;
        cc += c * c;
        xs += vi * s;
        xc += vi * c;
    }
    let det = ss * cc - sc * sc;
    let a = (xs * cc - xc * sc) / det;
    let b = (ss * xc - sc * xs) / det;
    let residu: Vec<f64> = (debut..fin)
        .map(|i| {
            let (s, c) = (w * i as f64).sin_cos();
            v[i] - a * s - b * c
        })
        .collect();
    ((a * a + b * b).sqrt(), b.atan2(a), rms(&residu))
}

/// Délai de `mesure` par rapport à `reference` (en échantillons de sortie,
/// positif = Tune en retard), par la différence de phase à `f`.
fn delai_par_phase(
    mesure: &[f64],
    reference: &[f64],
    sr: u32,
    f: f64,
    debut: usize,
    fin: usize,
) -> f64 {
    let (_, phi_m, _) = ajuster_sinus(mesure, sr, f, debut, fin);
    let (_, phi_r, _) = ajuster_sinus(reference, sr, f, debut, fin);
    let mut d = phi_r - phi_m;
    while d > PI {
        d -= 2.0 * PI;
    }
    while d < -PI {
        d += 2.0 * PI;
    }
    d / (2.0 * PI * f) * sr as f64
}

/// Module de la réponse en fréquence de `h` (une réponse impulsionnelle) à
/// `f`, par évaluation directe de la TFD.
fn module_a(h: &[f64], sr: u32, f: f64) -> f64 {
    let w = 2.0 * PI * f / sr as f64;
    let (mut re, mut im) = (0.0, 0.0);
    for (i, x) in h.iter().enumerate() {
        let (s, c) = (w * i as f64).sin_cos();
        re += x * c;
        im -= x * s;
    }
    (re * re + im * im).sqrt()
}

// ───────────────────────── Tune : piste et flux ─────────────────────────

fn tune_piste(x: &[f32], de: u32, vers: u32) -> Vec<f64> {
    en_f64(&rubato_resample_track(x, de, vers, 1))
}

/// Le chemin du producteur : blocs de `bloc` trames, puis `flush`. Rend
/// (sortie complète avec délai, trames rendues par le vidage, délai annoncé).
fn tune_flux(x: &[f32], de: u32, vers: u32, canaux: u16, bloc: usize) -> (Vec<f32>, usize, usize) {
    let ch = canaux as usize;
    let mut r = Some(new_streaming_resampler(de, vers, canaux).expect("format valide"));
    let delai = r.as_ref().map(|r| r.output_delay()).unwrap_or(0);
    let mut reste = Vec::new();
    let mut sortie = Vec::new();
    for morceau in x.chunks(bloc * ch) {
        sortie.extend(rubato_resample_chunk(
            &mut r, morceau, canaux, false, &mut reste,
        ));
    }
    let vidage = rubato_resample_chunk(&mut r, &[], canaux, true, &mut reste);
    let n_vidage = vidage.len() / ch;
    sortie.extend(vidage);
    (sortie, n_vidage, delai)
}

// ───────────────────────────── les rapports ─────────────────────────────

#[derive(Clone, Copy)]
struct Rapport {
    de: u32,
    vers: u32,
    nom: &'static str,
}

impl Rapport {
    /// Le noyau que la production choisit RÉELLEMENT pour ce rapport.
    ///
    /// C'était une constante recopiée ici, avec le barème de `resample.rs` en
    /// commentaire. Le correctif D1 a changé ce barème — et la copie, elle,
    /// n'aurait pas bougé : les témoins de délai auraient continué d'affirmer
    /// un noyau qui n'existait plus, en vert. `parametres_sinc` est publique
    /// pour cette raison.
    fn sinc_len(self) -> usize {
        parametres_sinc(self.de, self.vers).sinc_len
    }
}

const RAPPORTS: [Rapport; 7] = [
    Rapport {
        de: 44_100,
        vers: 48_000,
        nom: "44,1 → 48 kHz",
    },
    Rapport {
        de: 48_000,
        vers: 44_100,
        nom: "48 → 44,1 kHz",
    },
    Rapport {
        de: 44_100,
        vers: 96_000,
        nom: "44,1 → 96 kHz",
    },
    Rapport {
        de: 96_000,
        vers: 48_000,
        nom: "96 → 48 kHz",
    },
    Rapport {
        de: 44_100,
        vers: 192_000,
        nom: "44,1 → 192 kHz",
    },
    Rapport {
        de: 176_400,
        vers: 48_000,
        nom: "176,4 → 48 kHz (PCM de DSD64)",
    },
    Rapport {
        de: 192_000,
        vers: 44_100,
        nom: "192 → 44,1 kHz",
    },
];

/// Ce que chaque rapport rend AUJOURD'HUI (relevé sur Shrek, 13/09/2026,
/// APRÈS le correctif D1 : fenêtre Blackman², noyau choisi sur la cadence la
/// plus basse). Les témoins affirment ces chiffres ; les tolérances sont
/// justifiées dans chaque message d'assertion.
///
/// La valeur d'avant est rappelée en commentaire sur chaque entrée : ces
/// relevés-ci ne sont pas des seuils qu'on desserre, ce sont les mesures d'un
/// filtre qui a délibérément changé.
struct Attendu {
    /// Délai résiduel de la piste (trames de sortie, négatif = Tune en avance).
    delai: f64,
    /// Erreur RMS contre la référence alignée, sinus 1 kHz (dB).
    err_sinus_db: f64,
    /// THD+N de Tune à 1 kHz (dB).
    thd_n_db: f64,
    /// Erreur RMS contre la référence alignée, balayage (dB, 2 → 18 kHz).
    err_balayage_db: f64,
    /// Gain à 20 kHz (dB).
    gain_20k_db: f64,
    /// Bande passante à −0,1 dB (Hz).
    bande_hz: f64,
    /// Réjection (dB) : images (montée) ou repliement (descente).
    rejection_db: f64,
    /// Erreur RMS des 64 premières / dernières trames (dB).
    bord_debut_db: f64,
    bord_fin_db: f64,
    /// `output_delay()` annoncé, trames rendues par le vidage, trames de
    /// flux au-delà de `délai + attendu`.
    delai_annonce: usize,
    vidage_trames: usize,
    marge_queue: i64,
}

const ATTENDU: [Attendu; 7] = [
    // 44,1 → 48 kHz — AVANT le correctif D1 : −10,31 dB à 20 kHz, bande
    // 18 550 Hz, erreur RMS −108,4 dB, délai annoncé 69 (noyau 128).
    Attendu {
        delai: -0.685,
        err_sinus_db: -121.4,
        thd_n_db: -136.8,
        err_balayage_db: -108.4,
        gain_20k_db: 0.0,
        bande_hz: 20_750.0,
        rejection_db: -116.4,
        bord_debut_db: -64.0,
        bord_fin_db: -63.4,
        delai_annonce: 139,
        vidage_trames: 2_229,
        marge_queue: 2_013,
    },
    // 48 → 44,1 kHz — AVANT : −9,90 dB à 20 kHz, bande 18 450 Hz, erreur RMS
    // −105,0 dB, délai annoncé 58 (noyau 128).
    Attendu {
        delai: -0.404,
        err_sinus_db: -118.1,
        thd_n_db: -138.2,
        err_balayage_db: -110.8,
        gain_20k_db: 0.0,
        bande_hz: 20_700.0,
        rejection_db: -122.6,
        bord_debut_db: -64.9,
        bord_fin_db: -66.2,
        delai_annonce: 117,
        vidage_trames: 1_882,
        marge_queue: 939,
    },
    // 44,1 → 96 kHz — AVANT : −10,31 dB à 20 kHz, bande 18 550 Hz, délai
    // annoncé 139 (noyau 128).
    Attendu {
        delai: -0.369,
        err_sinus_db: -121.4,
        thd_n_db: -136.1,
        err_balayage_db: -108.4,
        gain_20k_db: 0.0,
        bande_hz: 20_750.0,
        rejection_db: -111.1,
        bord_debut_db: -60.7,
        bord_fin_db: -60.8,
        delai_annonce: 278,
        vidage_trames: 4_458,
        marge_queue: 4_027,
    },
    // 96 → 48 kHz — AVANT : −0,92 dB à 20 kHz, bande 18 950 Hz, délai annoncé
    // 32 (noyau 128). L'erreur RMS PASSE de −99,1 à −88,4 dB : voir le témoin
    // `audiophile_erreur_1k_96_vers_48`, c'est un écart de GAIN en bande
    // (+0,00033 dB), pas de la distorsion — le THD+N reste à −146,3 dB.
    Attendu {
        delai: -1.002,
        err_sinus_db: -88.4,
        thd_n_db: -146.3,
        err_balayage_db: -88.4,
        gain_20k_db: 0.0,
        bande_hz: 22_050.0,
        rejection_db: -134.1,
        bord_debut_db: -74.0,
        bord_fin_db: -73.5,
        delai_annonce: 64,
        vidage_trames: 1_024,
        marge_queue: 575,
    },
    // 44,1 → 192 kHz — AVANT : −10,31 dB à 20 kHz, bande 18 550 Hz, délai
    // annoncé 278 (noyau 128).
    Attendu {
        delai: -0.738,
        err_sinus_db: -121.4,
        thd_n_db: -135.9,
        err_balayage_db: -108.4,
        gain_20k_db: 0.0,
        bande_hz: 20_750.0,
        rejection_db: -111.1,
        bord_debut_db: -61.1,
        bord_fin_db: -61.0,
        delai_annonce: 557,
        vidage_trames: 8_916,
        marge_queue: 8_054,
    },
    // 176,4 → 48 kHz — AVANT : −0,03 dB à 20 kHz, bande 20 400 Hz, erreur RMS
    // −99,4 dB. Le noyau ne change PAS (256 avant comme après) : seule la
    // fenêtre passe de Blackman-Harris² à Blackman², d'où 750 Hz de bande en
    // plus et une erreur RMS qui passe enfin sous les −100 dB.
    Attendu {
        delai: -0.171,
        err_sinus_db: -102.0,
        thd_n_db: -144.5,
        err_balayage_db: -102.3,
        gain_20k_db: 0.0,
        bande_hz: 21_150.0,
        rejection_db: -144.3,
        bord_debut_db: -77.5,
        bord_fin_db: -77.1,
        delai_annonce: 34,
        vidage_trames: 557,
        marge_queue: 448,
    },
    // 192 → 44,1 kHz — AVANT : −0,04 dB à 20 kHz, bande 20 200 Hz. Noyau 512
    // avant comme après ; seule la fenêtre change.
    Attendu {
        delai: -0.201,
        err_sinus_db: -85.4,
        thd_n_db: -144.6,
        err_balayage_db: -85.4,
        gain_20k_db: 0.0,
        bande_hz: 20_600.0,
        rejection_db: -145.8,
        bord_debut_db: -73.6,
        bord_fin_db: -77.4,
        delai_annonce: 58,
        vidage_trames: 471,
        marge_queue: 294,
    },
];

/// Tout ce qui est mesuré sur un rapport, calculé une fois par binaire.
#[derive(Debug, Clone)]
struct Mesures {
    /// Délai résiduel de Tune (piste) en trames de sortie, par la phase à 1 kHz.
    delai_1k: f64,
    /// Idem à 10 kHz : égal au précédent si la phase est linéaire.
    delai_10k: f64,
    /// Erreur RMS (dB) sinus 1 kHz, référence à délai nul (ce que voit le gapless).
    err_sinus_brute_db: f64,
    /// Erreur RMS (dB) sinus 1 kHz, référence décalée du délai résiduel mesuré.
    err_sinus_db: f64,
    /// Erreur RMS (dB) balayage 20 Hz → 20 kHz, référence décalée.
    err_balayage_db: f64,
    /// THD+N (dB) de la sortie de Tune à 1 kHz : résidu après ajustement exact.
    thd_n_1k_db: f64,
    /// Gain à 1 kHz (dB) par ajustement du sinus (amplitude rendue / amplitude émise).
    gain_1k_db: f64,
    /// Gain à 20 kHz (dB) par ajustement d'un sinus à 20 kHz.
    gain_20k_sinus_db: f64,
    /// Gain à 20 kHz (dB) par la TFD de l'impulsion (normalisée par le rapport).
    gain_20k_impulsion_db: f64,
    /// Fréquence (Hz) où la réponse passe sous −0,1 dB.
    bande_01db_hz: f64,
    /// Ondulation crête (dB) entre 20 Hz et 20 kHz.
    ondulation_db: f64,
    /// Module (dB) à la moitié de la cadence la plus basse (Nyquist utile).
    gain_nyquist_db: f64,
    /// Réjection (dB) : images (montée) ou repliement d'un ton (descente).
    rejection_db: f64,
    /// Ce que mesure `rejection_db`.
    rejection_methode: &'static str,
    /// Erreur RMS (dB) sur les 64 premières trames de la piste.
    bord_debut_db: f64,
    /// Erreur RMS (dB) sur les 64 dernières trames de la piste.
    bord_fin_db: f64,
    /// Longueur de la piste rendue − longueur attendue (trames).
    longueur_ecart: i64,
    /// Délai annoncé par rubato (`output_delay`), trames de sortie.
    delai_annonce: usize,
    /// Trames rendues par le `flush` seul (blocs de 1 024).
    vidage_trames: usize,
    /// Trames de flux (blocs de 1 024) au-delà de `délai + attendu` : marge de queue.
    flux_marge_queue: i64,
    /// Erreur RMS (dB) des 64 dernières trames UTILES du flux (blocs de 1 024)
    /// contre la référence décalée : la queue vidée est-elle juste ?
    vidage_err_db: f64,
    /// Écart absolu maximal blocs de 1 024 vs piste (stéréo), trames utiles.
    blocs_1024_vs_piste: f64,
    /// Écart absolu maximal blocs de 4 096 vs blocs de 1 024 (stéréo).
    blocs_4096_vs_1024: f64,
}

fn mesurer(r: Rapport) -> Mesures {
    let Rapport { de, vers, .. } = r;
    let rapport = vers as f64 / de as f64;
    let nyq_utile = 0.5 * de.min(vers) as f64;

    // ── sinus 1 kHz : délai, erreur brute, erreur alignée ──
    let x = sinus(de, 1_000.0, DUREE_S);
    let piste = tune_piste(&x, de, vers);
    let ref0 = Reference::new(de, vers, 0.0);
    let attendu = ref0.trames_de_sortie(x.len());
    let longueur_ecart = piste.len() as i64 - attendu as i64;
    let y0 = ref0.appliquer(&en_f64(&x));
    let n = piste.len().min(y0.len());
    let (c0, c1) = (n / 10, n * 9 / 10);
    let delai_1k = delai_par_phase(&piste, &y0, vers, 1_000.0, c0, c1);
    let err_sinus_brute_db = erreur_rms_db(&piste, &y0, c0, c1);
    let refd = Reference::new(de, vers, delai_1k);
    let yd = refd.appliquer(&en_f64(&x));
    let err_sinus_db = erreur_rms_db(&piste, &yd, c0, c1);
    let (amp_1k, _, residu_1k) = ajuster_sinus(&piste, vers, 1_000.0, c0, c1);
    let thd_n_1k_db = db(residu_1k / (amp_1k / 2f64.sqrt()));
    let gain_1k_db = db(amp_1k / AMPLITUDE);
    let bord_debut_db = erreur_rms_db(&piste, &yd, 0, 64.min(n));
    let bord_fin_db = erreur_rms_db(&piste, &yd, n - 64.min(n), n);

    // ── 10 kHz : la phase est-elle linéaire ? ──
    let x10 = sinus(de, 10_000.0, DUREE_S);
    let piste10 = tune_piste(&x10, de, vers);
    let y10 = ref0.appliquer(&en_f64(&x10));
    let n10 = piste10.len().min(y10.len());
    let delai_10k = delai_par_phase(&piste10, &y10, vers, 10_000.0, n10 / 10, n10 * 9 / 10);

    // ── 20 kHz par un sinus : le second avis sur la bande passante ──
    let x20 = sinus(de, 20_000.0, DUREE_S);
    let piste20 = tune_piste(&x20, de, vers);
    let n20 = piste20.len();
    let (amp_20k, _, _) = ajuster_sinus(&piste20, vers, 20_000.0, n20 / 10, n20 * 9 / 10);
    let gain_20k_sinus_db = db(amp_20k / AMPLITUDE);

    // ── balayage ──
    let xb = balayage(de, DUREE_S);
    let pisteb = tune_piste(&xb, de, vers);
    let yb = refd.appliquer(&en_f64(&xb));
    let nb = pisteb.len().min(yb.len());
    let err_balayage_db = erreur_rms_db(&pisteb, &yb, nb / 10, nb * 9 / 10);

    // ── impulsion : réponse en fréquence de Tune ──
    let pos = de as usize / 10;
    let xi = impulsion(de, 0.25, pos);
    let h_tune = tune_piste(&xi, de, vers);
    let centre = ((pos as f64 * rapport).round() as usize).min(h_tune.len());
    let fen = 4_096.min(centre);
    let h = &h_tune[centre - fen..(centre + fen).min(h_tune.len())];
    // Un interpolateur à gain unité rend une impulsion dont la somme vaut le
    // rapport de cadences : la TFD de sortie est normalisée par ce rapport.
    let module = |f: f64| module_a(h, vers, f) / rapport;
    let gain_20k_impulsion_db = db(module(20_000.0));
    let mut bande_01db_hz = 0.0;
    let mut ondulation_db: f64 = 0.0;
    let mut f = 20.0;
    while f < nyq_utile {
        let g = db(module(f));
        if f <= 20_000.0 {
            ondulation_db = ondulation_db.max(g.abs());
        }
        if g < -0.1 && bande_01db_hz == 0.0 {
            bande_01db_hz = f;
        }
        f += if f < 1_000.0 { 20.0 } else { 50.0 };
    }
    if bande_01db_hz == 0.0 {
        bande_01db_hz = nyq_utile;
    }
    let gain_nyquist_db = db(module(nyq_utile));

    // ── réjection ──
    let (rejection_db, rejection_methode) = if vers > de {
        // Montée : images entre Nyquist d'entrée et Nyquist de sortie, à
        // partir du quart de la bande d'arrêt (la transition est exclue).
        let nyq_in = 0.5 * de as f64;
        let nyq_out = 0.5 * vers as f64;
        let debut = nyq_in + 0.25 * (nyq_out - nyq_in);
        let mut pire: f64 = -400.0;
        let mut f = debut;
        while f <= nyq_out {
            pire = pire.max(db(module(f)));
            f += 50.0;
        }
        (
            pire,
            "images par l'impulsion, du quart de la bande d'arrêt à Nyquist de sortie",
        )
    } else {
        // Descente : ton au quart de la bande d'arrêt ; tout ce qui ressort
        // est un repliement (la référence, elle, rend < −120 dB).
        let nyq_out = 0.5 * vers as f64;
        let nyq_in = 0.5 * de as f64;
        let f_ton = nyq_out + 0.25 * (nyq_in - nyq_out);
        let xt = sinus(de, f_ton, DUREE_S);
        let pt = tune_piste(&xt, de, vers);
        let nt = pt.len();
        let repli = rms(&pt[nt / 10..nt * 9 / 10]) / (AMPLITUDE / 2f64.sqrt());
        (
            db(repli),
            "repliement d'un ton au quart de la bande d'arrêt, RMS de la sortie",
        )
    };

    // ── flux par blocs : vidage et identité avec la piste ──
    let (flux, vidage_trames, delai_annonce) = tune_flux(&x, de, vers, 1, 1_024);
    let flux_marge_queue = flux.len() as i64 - (delai_annonce + attendu) as i64;
    let flux64 = en_f64(&flux);
    let utile: Vec<f64> = flux64
        .iter()
        .skip(delai_annonce)
        .take(attendu)
        .copied()
        .collect();
    let nu = utile.len().min(yd.len());
    let vidage_err_db = erreur_rms_db(&utile, &yd, nu - 64.min(nu), nu);

    // stéréo, comme le producteur : blocs de 1 024 puis 4 096 contre la piste
    let xs: Vec<f32> = x.iter().flat_map(|&s| [s, -s * 0.5]).collect();
    let piste_s = rubato_resample_track(&xs, de, vers, 2);
    let (f1, _, d1) = tune_flux(&xs, de, vers, 2, 1_024);
    let (f4, _, d4) = tune_flux(&xs, de, vers, 2, 4_096);
    let u1 = &f1[d1 * 2..(d1 * 2 + piste_s.len()).min(f1.len())];
    let u4 = &f4[d4 * 2..(d4 * 2 + piste_s.len()).min(f4.len())];
    let ecart_max = |a: &[f32], b: &[f32]| {
        a.iter()
            .zip(b)
            .map(|(p, q)| (p - q).abs() as f64)
            .fold(0.0, f64::max)
    };
    let blocs_1024_vs_piste = ecart_max(u1, &piste_s);
    let blocs_4096_vs_1024 = ecart_max(u4, u1);

    Mesures {
        delai_1k,
        delai_10k,
        err_sinus_brute_db,
        err_sinus_db,
        err_balayage_db,
        thd_n_1k_db,
        gain_1k_db,
        gain_20k_sinus_db,
        gain_20k_impulsion_db,
        bande_01db_hz,
        ondulation_db,
        gain_nyquist_db,
        rejection_db,
        rejection_methode,
        bord_debut_db,
        bord_fin_db,
        longueur_ecart,
        delai_annonce,
        vidage_trames,
        flux_marge_queue,
        vidage_err_db,
        blocs_1024_vs_piste,
        blocs_4096_vs_1024,
    }
}

static CACHE: [OnceLock<Mesures>; 7] = [const { OnceLock::new() }; 7];

fn mesures(i: usize) -> &'static Mesures {
    CACHE[i].get_or_init(|| {
        let m = mesurer(RAPPORTS[i]);
        eprintln!("[T10] {} : {m:#?}", RAPPORTS[i].nom);
        m
    })
}

// ───────────────────── la référence, prouvée contre elle-même ─────────────────────

#[test]
fn reference_gain_continu_unite() {
    for r in RAPPORTS {
        let g = Reference::new(r.de, r.vers, 0.0).gain_continu();
        assert!(
            (g - 1.0).abs() < 1e-7,
            "{} : gain continu de la référence = {g:.12}, attendu 1 ± 1e-7 (noyau tronqué à 1025 coefficients : 5e-9 mesuré)",
            r.nom
        );
    }
}

#[test]
fn reference_sinus_1k_44_vers_48_thd_n_sous_moins_120_db() {
    let x = sinus(44_100, 1_000.0, DUREE_S);
    let y = Reference::new(44_100, 48_000, 0.0).appliquer(&en_f64(&x));
    let n = y.len();
    let (amp, phase, residu) = ajuster_sinus(&y, 48_000, 1_000.0, n / 10, n * 9 / 10);
    let thd_n = db(residu / (amp / 2f64.sqrt()));
    eprintln!(
        "[T10] référence 44,1 → 48 : amplitude {amp:.9}, phase {phase:.3e} rad, THD+N {thd_n:.1} dB"
    );
    assert!(
        (amp - AMPLITUDE).abs() < 1e-6,
        "amplitude rendue {amp:.9} pour {AMPLITUDE} : la référence n'est pas transparente"
    );
    assert!(
        phase.abs() < 1e-6,
        "phase {phase:.3e} rad : la référence n'est pas à délai nul"
    );
    assert!(
        thd_n < -120.0,
        "THD+N de la référence = {thd_n:.1} dB, attendu < −120 dB"
    );
}

#[test]
fn reference_ton_au_dessus_de_nyquist_96_vers_48_rejete_sous_moins_120_db() {
    // 30 kHz à 96 kHz : au-dessus de Nyquist de sortie (24 kHz), doit disparaître.
    let x = sinus(96_000, 30_000.0, DUREE_S);
    let y = Reference::new(96_000, 48_000, 0.0).appliquer(&en_f64(&x));
    let n = y.len();
    let rejet = db(rms(&y[n / 10..n * 9 / 10]) / (AMPLITUDE / 2f64.sqrt()));
    eprintln!("[T10] référence 96 → 48, ton 30 kHz : {rejet:.1} dB");
    assert!(
        rejet < -120.0,
        "repliement de la référence = {rejet:.1} dB, attendu < −120 dB"
    );
}

#[test]
fn reference_decalage_fractionnaire_est_exact() {
    // Décalée de 0,37 trame de sortie, la référence doit rendre exactement ce
    // délai par la phase (c'est l'outil qui mesure le délai de Tune).
    let x = sinus(44_100, 1_000.0, DUREE_S);
    let y0 = Reference::new(44_100, 48_000, 0.0).appliquer(&en_f64(&x));
    let yd = Reference::new(44_100, 48_000, 0.37).appliquer(&en_f64(&x));
    let n = y0.len();
    let d = delai_par_phase(&yd, &y0, 48_000, 1_000.0, n / 10, n * 9 / 10);
    assert!((d - 0.37).abs() < 1e-6, "délai mesuré {d:.9}, attendu 0,37");
}

// ───────────────────── les témoins, un par rapport et par mesure ─────────────────────

/// Seuils d'un rééchantillonneur audiophile, affirmés par les témoins
/// `#[ignore]` là où la mesure est en dessous.
const AUDIOPHILE_BANDE_HZ: f64 = 20_000.0;
const AUDIOPHILE_ERREUR_DB: f64 = -100.0;
const AUDIOPHILE_REJECTION_DB: f64 = -100.0;

macro_rules! temoins_du_rapport {
    ($module:ident, $i:expr) => {
        mod $module {
            use super::*;
            const I: usize = $i;

            #[test]
            fn delai_residuel_affirme_la_mesure() {
                let (m, a, r) = (mesures(I), &ATTENDU[I], RAPPORTS[I]);
                assert!(
                    (m.delai_1k - a.delai).abs() < 0.01,
                    "{} : délai résiduel {:.3} trame(s), attendu {:.3} ± 0,01 (la phase à 1 kHz \
                     le résout à 1e-6 près ; 0,01 couvre l'arrondi flottant)",
                    r.nom, m.delai_1k, a.delai
                );
                assert!(
                    (m.delai_1k - m.delai_10k).abs() < 1e-3,
                    "{} : délai {:.4} à 1 kHz mais {:.4} à 10 kHz : la phase n'est pas linéaire",
                    r.nom, m.delai_1k, m.delai_10k
                );
                // Le délai vrai du sinc de rubato vaut (sinc_len/2 − 1/256)·ratio − 1
                // trame de sortie — le 1/256 est le pas de la table suréchantillonnée
                // (`oversampling_factor = 256`, interpolation linéaire) ; `output_delay()`
                // rend ⌊sinc_len/2 · ratio⌋ ; la piste retire ce dernier : reste
                // (fraction − 1) ∈ (−1, 0], jamais nul.
                let ratio = r.vers as f64 / r.de as f64;
                let explique = ((r.sinc_len() as f64 / 2.0 - 1.0 / 256.0) * ratio - 1.0)
                    - m.delai_annonce as f64;
                assert!(
                    (m.delai_1k - explique).abs() < 0.005,
                    "{} : délai résiduel {:.4} ≠ ((sinc_len/2 − 1/256)·ratio − 1) − output_delay() = {:.4} \
                     (± 0,005 : les sept rapports s'y tiennent à 0,001 près)",
                    r.nom, m.delai_1k, explique
                );
            }

            #[test]
            fn erreur_rms_sinus_1k_affirme_la_mesure() {
                let (m, a, r) = (mesures(I), &ATTENDU[I], RAPPORTS[I]);
                assert!(
                    (m.err_sinus_db - a.err_sinus_db).abs() < 1.0,
                    "{} : erreur RMS à 1 kHz contre la référence alignée = {:.1} dB, attendu {:.1} ± 1 \
                     (mesure déterministe ; 1 dB couvre un réordonnancement flottant, pas un \
                     changement de noyau)",
                    r.nom, m.err_sinus_db, a.err_sinus_db
                );
                assert!(
                    m.thd_n_1k_db < a.thd_n_db + 3.0,
                    "{} : THD+N de Tune à 1 kHz = {:.1} dB, attendu ≤ {:.1} + 3 (plancher f32 ≈ −140 dB)",
                    r.nom, m.thd_n_1k_db, a.thd_n_db
                );
                assert!(
                    m.err_sinus_brute_db > -50.0,
                    "{} : l'erreur SANS alignement vaut {:.1} dB : le délai résiduel a disparu, \
                     le témoin de délai doit être relu",
                    r.nom, m.err_sinus_brute_db
                );
            }

            #[test]
            fn erreur_rms_balayage_affirme_la_mesure() {
                let (m, a, r) = (mesures(I), &ATTENDU[I], RAPPORTS[I]);
                assert!(
                    (m.err_balayage_db - a.err_balayage_db).abs() < 1.0,
                    "{} : erreur RMS du balayage 20 Hz → 20 kHz (2 → 18 kHz mesurés) = {:.1} dB, \
                     attendu {:.1} ± 1",
                    r.nom, m.err_balayage_db, a.err_balayage_db
                );
            }

            #[test]
            fn bande_passante_affirme_la_mesure() {
                let (m, a, r) = (mesures(I), &ATTENDU[I], RAPPORTS[I]);
                assert!(
                    (m.gain_20k_sinus_db - m.gain_20k_impulsion_db).abs() < 0.01,
                    "{} : gain à 20 kHz {:.3} dB par le sinus mais {:.3} dB par l'impulsion : \
                     les deux méthodes divergent, la mesure n'est pas fiable",
                    r.nom, m.gain_20k_sinus_db, m.gain_20k_impulsion_db
                );
                assert!(
                    (m.gain_20k_sinus_db - a.gain_20k_db).abs() < 0.05,
                    "{} : gain à 20 kHz = {:.2} dB, attendu {:.2} ± 0,05",
                    r.nom, m.gain_20k_sinus_db, a.gain_20k_db
                );
                assert!(
                    (m.bande_01db_hz - a.bande_hz).abs() <= 100.0,
                    "{} : bande passante à −0,1 dB = {:.0} Hz, attendu {:.0} ± 100 (pas de balayage \
                     50 Hz) ; ondulation crête 20 Hz → 20 kHz {:.3} dB, module à Nyquist utile {:.1} dB",
                    r.nom, m.bande_01db_hz, a.bande_hz, m.ondulation_db, m.gain_nyquist_db
                );
                assert!(
                    m.gain_1k_db.abs() < 0.001,
                    "{} : gain à 1 kHz = {:.5} dB, attendu 0 ± 0,001",
                    r.nom, m.gain_1k_db
                );
            }

            #[test]
            fn rejection_affirme_la_mesure() {
                let (m, a, r) = (mesures(I), &ATTENDU[I], RAPPORTS[I]);
                assert!(
                    m.rejection_db < a.rejection_db + 3.0,
                    "{} : réjection ({}) = {:.1} dB, attendu ≤ {:.1} + 3",
                    r.nom, m.rejection_methode, m.rejection_db, a.rejection_db
                );
                assert!(
                    m.rejection_db < AUDIOPHILE_REJECTION_DB,
                    "{} : réjection {:.1} dB, un rééchantillonneur audiophile rend < {AUDIOPHILE_REJECTION_DB} dB",
                    r.nom, m.rejection_db
                );
            }

            #[test]
            fn bords_affirment_la_mesure() {
                let (m, a, r) = (mesures(I), &ATTENDU[I], RAPPORTS[I]);
                assert!(
                    (m.bord_debut_db - a.bord_debut_db).abs() < 3.0,
                    "{} : erreur RMS des 64 premières trames = {:.1} dB, attendu {:.1} ± 3 (la \
                     référence sonne 512 trames avant un départ dur, Tune {} : l'écart est \
                     attendu, sa VALEUR est surveillée)",
                    r.nom, m.bord_debut_db, a.bord_debut_db, r.sinc_len() / 2
                );
                assert!(
                    (m.bord_fin_db - a.bord_fin_db).abs() < 3.0,
                    "{} : erreur RMS des 64 dernières trames = {:.1} dB, attendu {:.1} ± 3",
                    r.nom, m.bord_fin_db, a.bord_fin_db
                );
            }

            #[test]
            fn longueur_et_vidage_affirment_la_mesure() {
                let (m, a, r) = (mesures(I), &ATTENDU[I], RAPPORTS[I]);
                assert_eq!(
                    m.longueur_ecart, 0,
                    "{} : la piste rend {} trame(s) de plus que round(n·vers/de)",
                    r.nom, m.longueur_ecart
                );
                assert_eq!(
                    m.delai_annonce, a.delai_annonce,
                    "{} : output_delay() = {}, attendu {} (⌊sinc_len/2 · ratio⌋)",
                    r.nom, m.delai_annonce, a.delai_annonce
                );
                assert_eq!(
                    m.vidage_trames, a.vidage_trames,
                    "{} : le vidage rend {} trames, attendu {} (déterministe : reste du bloc \
                     de 1 024 puis un bloc de silence)",
                    r.nom, m.vidage_trames, a.vidage_trames
                );
                assert_eq!(
                    m.flux_marge_queue, a.marge_queue,
                    "{} : le flux rend {} trame(s) au-delà de délai + attendu, attendu {}",
                    r.nom, m.flux_marge_queue, a.marge_queue
                );
                assert!(
                    m.flux_marge_queue >= 0,
                    "{} : le vidage tronque la queue de {} trame(s)",
                    r.nom, -m.flux_marge_queue
                );
                assert!(
                    (m.vidage_err_db - a.bord_fin_db).abs() < 3.0,
                    "{} : les 64 dernières trames UTILES du flux vidé s'écartent de la référence \
                     de {:.1} dB, attendu {:.1} ± 3 (= le bord de fin de la piste : le vidage \
                     rend les mêmes échantillons)",
                    r.nom, m.vidage_err_db, a.bord_fin_db
                );
            }

            #[test]
            fn blocs_de_1024_et_4096_rendent_la_piste_a_l_identique() {
                let (m, r) = (mesures(I), RAPPORTS[I]);
                assert_eq!(
                    m.blocs_1024_vs_piste, 0.0,
                    "{} : blocs de 1 024 (stéréo, délai retiré) vs piste d'un bloc : écart max \
                     {:e}, attendu 0 exactement (le gapless en dépend)",
                    r.nom, m.blocs_1024_vs_piste
                );
                assert_eq!(
                    m.blocs_4096_vs_1024, 0.0,
                    "{} : blocs de 4 096 vs blocs de 1 024 : écart max {:e}, attendu 0 exactement",
                    r.nom, m.blocs_4096_vs_1024
                );
            }

            #[test]
            #[ignore = "défaut connu : la piste retire ⌊sinc_len/2·ratio⌋ trames alors que le délai vrai vaut (sinc_len/2 − 1/256)·ratio − 1 ; reste −0,17 à −1,00 trame (96 → 48 : une trame entière perdue en tête)"]
            fn audiophile_delai_residuel_nul() {
                let (m, r) = (mesures(I), RAPPORTS[I]);
                assert!(
                    m.delai_1k.abs() < 0.01,
                    "{} : délai résiduel {:.3} trame(s), un contrat « exact » exige 0 ± 0,01",
                    r.nom, m.delai_1k
                );
            }
        }
    };
}

temoins_du_rapport!(r44_1_vers_48, 0);
temoins_du_rapport!(r48_vers_44_1, 1);
temoins_du_rapport!(r44_1_vers_96, 2);
temoins_du_rapport!(r96_vers_48, 3);
temoins_du_rapport!(r44_1_vers_192, 4);
temoins_du_rapport!(r176_4_vers_48_dsd64, 5);
temoins_du_rapport!(r192_vers_44_1, 6);

// ─────────── seuils audiophiles : passés là où Tune les tient, ignorés ailleurs ───────────

fn affirme_bande(i: usize) {
    let (m, r) = (mesures(i), RAPPORTS[i]);
    assert!(
        m.bande_01db_hz >= AUDIOPHILE_BANDE_HZ && m.gain_20k_sinus_db > -0.1,
        "{} : bande à −0,1 dB = {:.0} Hz, gain à 20 kHz = {:.2} dB ; un rééchantillonneur \
         audiophile tient 20 kHz à −0,1 dB",
        r.nom,
        m.bande_01db_hz,
        m.gain_20k_sinus_db
    );
}

fn affirme_erreur(i: usize) {
    let (m, r) = (mesures(i), RAPPORTS[i]);
    assert!(
        m.err_sinus_db < AUDIOPHILE_ERREUR_DB,
        "{} : erreur RMS à 1 kHz contre la référence = {:.1} dB ; à 24 bits on attend < \
         {AUDIOPHILE_ERREUR_DB} dB (c'est le gain en bande passante qui s'écarte : {:.5} dB, \
         le THD+N est à {:.1} dB)",
        r.nom,
        m.err_sinus_db,
        m.gain_1k_db,
        m.thd_n_1k_db
    );
}

// D1 est CORRIGÉ (13/09) : les cinq témoins ci-dessous étaient `#[ignore]`,
// ils sont désormais exécutés. Ils ne demandent rien de nouveau — c'est le
// même `affirme_bande` qu'avant, avec le même seuil de 20 kHz à −0,1 dB.
#[test]
fn audiophile_bande_20k_44_1_vers_48() {
    affirme_bande(0);
}
#[test]
fn audiophile_bande_20k_48_vers_44_1() {
    affirme_bande(1);
}
#[test]
fn audiophile_bande_20k_44_1_vers_96() {
    affirme_bande(2);
}
#[test]
fn audiophile_bande_20k_96_vers_48() {
    affirme_bande(3);
}
#[test]
fn audiophile_bande_20k_44_1_vers_192() {
    affirme_bande(4);
}
#[test]
fn audiophile_bande_20k_176_4_vers_48() {
    affirme_bande(5);
}
#[test]
fn audiophile_bande_20k_192_vers_44_1() {
    affirme_bande(6);
}

#[test]
fn audiophile_erreur_1k_44_1_vers_48() {
    affirme_erreur(0);
}
#[test]
fn audiophile_erreur_1k_48_vers_44_1() {
    affirme_erreur(1);
}
#[test]
fn audiophile_erreur_1k_44_1_vers_96() {
    affirme_erreur(2);
}
#[test]
#[ignore = "défaut connu, AGGRAVÉ par le correctif D1 : −88,4 dB en 96 → 48 kHz (était −99,1). L'écart est entièrement un gain en bande de +0,00033 dB (0,0038 %) — le THD+N reste à −146,3 dB, donc aucune distorsion ajoutée. C'est la normalisation de gain de la fenêtre, le même défaut que 192 → 44,1 ; il se traite à part"]
fn audiophile_erreur_1k_96_vers_48() {
    affirme_erreur(3);
}
#[test]
fn audiophile_erreur_1k_44_1_vers_192() {
    affirme_erreur(4);
}
// Était `#[ignore]` à −99,4 dB, 0,6 dB sous le seuil. La fenêtre Blackman²
// l'amène à −102,0 dB : le témoin s'exécute.
#[test]
fn audiophile_erreur_1k_176_4_vers_48() {
    affirme_erreur(5);
}
#[test]
#[ignore = "défaut connu : −85,4 dB en 192 → 44,1 kHz (était −87,8 ; gain en bande −0,00047 dB, noyau 512). Comme en 96 → 48, l'écart est un GAIN, pas une distorsion : THD+N à −144,6 dB"]
fn audiophile_erreur_1k_192_vers_44_1() {
    affirme_erreur(6);
}

// ───────────────────────────── le relevé ─────────────────────────────

/// Imprime toutes les mesures (`--nocapture`) : c'est le relevé du document
/// `docs/mesures/2218-reechantillonnage-reference.md`.
#[test]
fn releve_de_tous_les_rapports() {
    for (i, r) in RAPPORTS.iter().enumerate() {
        let m = mesures(i);
        assert!(m.err_sinus_db.is_finite(), "{} : mesure non finie", r.nom);
    }
}
