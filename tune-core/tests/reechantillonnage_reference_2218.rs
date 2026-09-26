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
//! Deux couples de plus, 352,8 et 384 → 44,1 kHz (#4080), sont mesurés sans
//! la référence — bande à −0,1 dB et gain à 20 kHz seulement — parce qu'ils
//! sont les seuls à demander 1 024 coefficients et que rien ne le vérifiait.
//!
//! Chaque témoin AFFIRME la valeur mesurée aujourd'hui ; ceux marqués
//! `#[ignore = "défaut connu : …"]` affirment ce qu'un rééchantillonneur
//! audiophile doit rendre (bande 20 kHz à −0,1 dB, réjection > 100 dB, erreur
//! RMS < −100 dB — le plancher d'un mot de 24 bits est −144 dBFS).
//!
//! Portes PUBLIQUES : `audio::resample::{alignement_de_piste,
//! new_streaming_resampler, parametres_sinc, rubato_resample_chunk,
//! rubato_resample_track}` et `rubato::Resampler` pour `output_delay()`.
//! `alignement_de_piste` et `parametres_sinc` sont publiques pour la même
//! raison : ce banc doit affirmer ce que la PRODUCTION décide, pas une copie
//! du calcul qui pourrait diverger en silence.
//! `tune-core` porte `autotests = false` : ce fichier est
//! une cible `[[test]]` du manifeste, sinon il ne serait jamais compilé.

use std::f64::consts::PI;
use std::sync::OnceLock;

use rubato::Resampler;
use tune_core::audio::resample::{
    alignement_de_piste, new_streaming_resampler, parametres_sinc, rubato_resample_chunk,
    rubato_resample_track,
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

/// La réponse en fréquence RÉELLE de Tune pour ce couple : une impulsion
/// passée par `rubato_resample_track`, fenêtrée à ±4 096 trames autour de son
/// centre, puis le module de sa TFD à `f`, normalisé.
///
/// Ne dépend pas de la référence sinc : c'est ce qui permet de la mesurer aussi
/// sur des couples que la référence ne couvre pas (#4080).
fn reponse_de_tune(de: u32, vers: u32) -> impl Fn(f64) -> f64 {
    let rapport = vers as f64 / de as f64;
    let pos = de as usize / 10;
    let xi = impulsion(de, 0.25, pos);
    let h_tune = tune_piste(&xi, de, vers);
    let centre = ((pos as f64 * rapport).round() as usize).min(h_tune.len());
    let fen = 4_096.min(centre);
    let h = h_tune[centre - fen..(centre + fen).min(h_tune.len())].to_vec();
    // Un interpolateur à gain unité rend une impulsion dont la somme vaut le
    // rapport de cadences : la TFD de sortie est normalisée par ce rapport.
    move |f: f64| module_a(&h, vers, f) / rapport
}

/// Balaie `module` de 20 Hz à `nyq_utile` (pas de 20 Hz sous 1 kHz, 50 Hz
/// au-dessus) : rend (bande à −0,1 dB en Hz, ondulation maximale sous 20 kHz
/// en dB). Sans point sous −0,1 dB, la bande vaut `nyq_utile`.
fn bande_et_ondulation(module: &dyn Fn(f64) -> f64, nyq_utile: f64) -> (f64, f64) {
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
    (bande_01db_hz, ondulation_db)
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
/// APRÈS le correctif D1 — fenêtre Blackman², noyau choisi sur la cadence la
/// plus basse — ET le correctif D2, #4078 : la piste retire son délai VRAI,
/// pré-roll compris). Les témoins affirment ces chiffres ; les tolérances sont
/// justifiées dans chaque message d'assertion.
///
/// La valeur d'avant est rappelée en commentaire sur chaque entrée : ces
/// relevés-ci ne sont pas des seuils qu'on desserre, ce sont les mesures d'un
/// filtre qui a délibérément changé.
///
/// D2 (#4078) ne touche ni le filtre ni la longueur : `err_sinus_db`,
/// `err_balayage_db`, `gain_20k_db`, `bande_hz`, `thd_n_db`,
/// `delai_annonce`, `vidage_trames` et `marge_queue` sont **inchangés**. Ce
/// qui bouge est le CADRAGE : `delai` (−0,17…−1,00 → ±0,0034 trame),
/// `err_sinus_brute_db` (−17,7…−38,2 → −68,4…−79,3 dB) et les deux bords.
///
/// ⚠️ `rejection_db` bouge sur les TROIS montées depuis 44,1 kHz, et c'est un
/// effet de MESURE, pas de filtre. Elle s'y mesure sur une impulsion, et son
/// plancher est l'interpolation linéaire de rubato entre phases — une erreur
/// qui n'est pas à bande limitée, donc qui dépend de l'endroit où l'impulsion
/// tombe par rapport à la grille de sortie. Aligner la piste l'y ramène. Le
/// balayage l'a mesuré sur 44,1 → 48 : résidu −0,0043 → −107,4 dB ; +0,0026
/// (retenu) → −109,9 dB ; +0,0094 → −116,6 dB. Choisir ce dernier flatterait
/// le chiffre au prix d'un décalage 3,7 fois plus grand : refusé. La réjection
/// reste au-delà de 107 dB partout, et les mesures sur SIGNAL (erreur RMS,
/// THD+N, bande, balayage) ne bougent pas d'un dixième de dB.
struct Attendu {
    /// Délai résiduel de la piste (trames de sortie, négatif = Tune en avance).
    delai: f64,
    /// Ce que `alignement_de_piste` decide (#4078) : pre-roll d'entree et
    /// trames de sortie retirees.
    pre_roll: usize,
    a_retirer: usize,
    /// Erreur RMS contre la référence alignée, sinus 1 kHz (dB).
    err_sinus_db: f64,
    /// Erreur RMS contre la référence à délai NUL, sinus 1 kHz (dB) : ce que
    /// verrait un banc d'identité sample-exact, sans recalage (#4078).
    err_sinus_brute_db: f64,
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

// D3 (#4079) : RMS remesures apres correction de la somme de normalisation.
// Reference, tolerances et seuil audiophile restent inchanges.
const ATTENDU: [Attendu; 7] = [
    // 44,1 → 48 kHz — AVANT le correctif D1 : −10,31 dB à 20 kHz, bande
    // 18 550 Hz, erreur RMS −108,4 dB, délai annoncé 69 (noyau 128).
    Attendu {
        delai: 0.0026,
        pre_roll: 53,
        a_retirer: 196,
        err_sinus_db: -135.7,
        err_sinus_brute_db: -69.5,
        thd_n_db: -136.8,
        err_balayage_db: -107.2,
        gain_20k_db: 0.0,
        bande_hz: 20_750.0,
        rejection_db: -109.9,
        bord_debut_db: -63.5,
        bord_fin_db: -63.9,
        delai_annonce: 139,
        vidage_trames: 2_229,
        marge_queue: 2_013,
    },
    // 48 → 44,1 kHz — AVANT : −9,90 dB à 20 kHz, bande 18 450 Hz, erreur RMS
    // −105,0 dB, délai annoncé 58 (noyau 128).
    Attendu {
        delai: 0.0027,
        pre_roll: 155,
        a_retirer: 259,
        err_sinus_db: -137.7,
        err_sinus_brute_db: -68.4,
        thd_n_db: -138.2,
        err_balayage_db: -108.7,
        gain_20k_db: 0.0,
        bande_hz: 20_700.0,
        rejection_db: -122.6,
        bord_debut_db: -65.3,
        bord_fin_db: -65.6,
        delai_annonce: 117,
        vidage_trames: 1_882,
        marge_queue: 939,
    },
    // 44,1 → 96 kHz — AVANT : −10,31 dB à 20 kHz, bande 18 550 Hz, délai
    // annoncé 139 (noyau 128).
    Attendu {
        delai: -0.0017,
        pre_roll: 36,
        a_retirer: 356,
        err_sinus_db: -135.4,
        err_sinus_brute_db: -79.1,
        thd_n_db: -136.1,
        err_balayage_db: -107.1,
        gain_20k_db: 0.0,
        bande_hz: 20_750.0,
        rejection_db: -107.4,
        bord_debut_db: -60.6,
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
        delai: -0.002,
        pre_roll: 0,
        a_retirer: 63,
        err_sinus_db: -137.2,
        err_sinus_brute_db: -71.8,
        thd_n_db: -146.3,
        err_balayage_db: -118.4,
        gain_20k_db: 0.0,
        bande_hz: 22_050.0,
        rejection_db: -134.1,
        bord_debut_db: -73.5,
        bord_fin_db: -74.0,
        delai_annonce: 64,
        vidage_trames: 1_024,
        marge_queue: 575,
    },
    // 44,1 → 192 kHz — AVANT : −10,31 dB à 20 kHz, bande 18 550 Hz, délai
    // annoncé 278 (noyau 128).
    Attendu {
        delai: -0.0034,
        pre_roll: 36,
        a_retirer: 713,
        err_sinus_db: -135.3,
        err_sinus_brute_db: -79.1,
        thd_n_db: -135.9,
        err_balayage_db: -107.2,
        gain_20k_db: 0.0,
        bande_hz: 20_750.0,
        rejection_db: -107.5,
        bord_debut_db: -60.9,
        bord_fin_db: -61.1,
        delai_annonce: 557,
        vidage_trames: 8_916,
        marge_queue: 8_054,
    },
    // 176,4 → 48 kHz — AVANT : −0,03 dB à 20 kHz, bande 20 400 Hz, erreur RMS
    // −99,4 dB. Le noyau ne change PAS (256 avant comme après) : seule la
    // fenêtre passe de Blackman-Harris² à Blackman², d'où 750 Hz de bande en
    // plus et une erreur RMS qui passe enfin sous les −100 dB.
    Attendu {
        delai: -0.0011,
        pre_roll: 19,
        a_retirer: 39,
        err_sinus_db: -143.4,
        err_sinus_brute_db: -77.1,
        thd_n_db: -144.5,
        err_balayage_db: -130.0,
        gain_20k_db: 0.0,
        bande_hz: 21_150.0,
        rejection_db: -144.3,
        bord_debut_db: -76.9,
        bord_fin_db: -77.1,
        delai_annonce: 34,
        vidage_trames: 557,
        marge_queue: 448,
    },
    // 192 → 44,1 kHz — AVANT : −0,04 dB à 20 kHz, bande 20 200 Hz. Noyau 512
    // avant comme après ; seule la fenêtre change.
    Attendu {
        delai: 0.0007,
        pre_roll: 27,
        a_retirer: 64,
        err_sinus_db: -144.5,
        err_sinus_brute_db: -80.5,
        thd_n_db: -144.6,
        err_balayage_db: -132.0,
        gain_20k_db: 0.0,
        bande_hz: 20_600.0,
        rejection_db: -145.8,
        bord_debut_db: -74.4,
        bord_fin_db: -74.7,
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
    /// Ce que la PRODUCTION a décidé pour aligner la piste (#4078) : pré-roll
    /// d'entrée, trames de sortie retirées, résidu prévu (trames de sortie).
    pre_roll: usize,
    a_retirer: usize,
    residu_prevu: f64,
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
    let module = reponse_de_tune(de, vers);
    let gain_20k_impulsion_db = db(module(20_000.0));
    let (bande_01db_hz, ondulation_db) = bande_et_ondulation(&module, nyq_utile);
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
    //
    // La piste ne se compare plus au flux « moins `output_delay()` » : depuis
    // #4078 elle porte un pré-roll de `pre_roll` trames d'entrée et retire
    // `a_retirer` trames de sortie. Ce que ces témoins doivent prouver est
    // inchangé — la piste n'est QUE le flux, recadré — mais il faut recadrer
    // pareil pour le voir.
    let (pre_roll, a_retirer, residu_prevu) = alignement_de_piste(de, vers);
    let pre_rouler = |v: &[f32], canaux: usize| -> Vec<f32> {
        let mut w = vec![0.0f32; pre_roll * canaux];
        w.extend_from_slice(v);
        w
    };

    let (flux, vidage_trames, delai_annonce) = tune_flux(&x, de, vers, 1, 1_024);
    let flux_marge_queue = flux.len() as i64 - (delai_annonce + attendu) as i64;
    let (flux_cadre, _, _) = tune_flux(&pre_rouler(&x, 1), de, vers, 1, 1_024);
    let utile: Vec<f64> = en_f64(&flux_cadre)
        .iter()
        .skip(a_retirer)
        .take(attendu)
        .copied()
        .collect();
    let nu = utile.len().min(yd.len());
    let vidage_err_db = erreur_rms_db(&utile, &yd, nu - 64.min(nu), nu);

    // stéréo, comme le producteur : blocs de 1 024 puis 4 096 contre la piste
    let xs: Vec<f32> = x.iter().flat_map(|&s| [s, -s * 0.5]).collect();
    let piste_s = rubato_resample_track(&xs, de, vers, 2);
    let xs_cadre = pre_rouler(&xs, 2);
    let (f1, _, _) = tune_flux(&xs_cadre, de, vers, 2, 1_024);
    let (f4, _, _) = tune_flux(&xs_cadre, de, vers, 2, 4_096);
    let (d1, d4) = (a_retirer, a_retirer);
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
        pre_roll,
        a_retirer,
        residu_prevu,
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
                // Le délai vrai du sinc de rubato vaut (sinc_len/2 + pré-roll − 1/256)·ratio − 1
                // trame de sortie — le 1/256 est le pas de la table suréchantillonnée
                // (`oversampling_factor = 256`, interpolation linéaire). C'est
                // `alignement_de_piste` qui choisit le pré-roll et le retrait ; on
                // affirme ici que la MESURE rend bien ce que la production a prévu,
                // et non une copie du calcul.
                let ratio = r.vers as f64 / r.de as f64;
                let explique = ((r.sinc_len() as f64 / 2.0 - 1.0 / 256.0 + m.pre_roll as f64)
                    * ratio
                    - 1.0)
                    - m.a_retirer as f64;
                assert!(
                    (m.delai_1k - explique).abs() < 0.005,
                    "{} : délai résiduel {:.4} ≠ ((sinc_len/2 + pré-roll − 1/256)·ratio − 1) − retrait = {:.4} \
                     (± 0,005 : les sept rapports s'y tiennent à 0,001 près)",
                    r.nom, m.delai_1k, explique
                );
                assert!(
                    (m.delai_1k - m.residu_prevu).abs() < 0.005,
                    "{} : la production annonce un résidu de {:.4} trame (pré-roll {}, \
                     retrait {}), la phase en mesure {:.4}",
                    r.nom, m.residu_prevu, m.pre_roll, m.a_retirer, m.delai_1k
                );
                assert_eq!(
                    (m.pre_roll, m.a_retirer), (a.pre_roll, a.a_retirer),
                    "{} : `alignement_de_piste` rend (pré-roll {}, retrait {}), attendu ({}, {}) \
                     — un changement de noyau déplace les deux",
                    r.nom, m.pre_roll, m.a_retirer, a.pre_roll, a.a_retirer
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
                // Ce que le correctif D2 achète, vu par un banc d'identité : la
                // référence à délai NUL, sans recalage. Elle valait −17,7 à
                // −38,2 dB tant que la piste retirait `output_delay()`.
                assert!(
                    (m.err_sinus_brute_db - a.err_sinus_brute_db).abs() < 1.0,
                    "{} : erreur RMS à 1 kHz contre la référence NON recalée = {:.1} dB, \
                     attendu {:.1} ± 1 (c'est le résidu de délai, irréductible : {:.4} trame)",
                    r.nom, m.err_sinus_brute_db, a.err_sinus_brute_db, m.delai_1k
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

            // D2 est CORRIGÉ (#4078) : ce témoin était `#[ignore]`, il est
            // désormais exécuté. Il ne demande rien de nouveau — c'est le
            // contrat « exact » que la fonction annonce depuis #1525.
            #[test]
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
fn audiophile_erreur_1k_192_vers_44_1() {
    affirme_erreur(6);
}

// ──────── #4080 : les deux couples que le barreau 1 024 sert, MESURÉS ────────
//
// `le_choix_du_noyau_suit_la_cadence_la_plus_basse` (`audio/resample.rs`)
// exerce les 8 × 7 couples du produit, mais contre le MODÈLE
// `bande_a_moins_0_1_db` (coupure × Nyquist bas − 2,82 · from / N), pas contre
// le filtre. Un retoucheur qui changerait le barème ET la constante du modèle
// le laisserait vert. Les deux témoins ci-dessous mesurent la réponse RÉELLE de
// Tune — impulsion, puis sinus à 20 kHz, la même instrumentation que les sept
// rapports — sur les deux seuls couples qui demandent 1 024 coefficients : le
// PCM de DSD256 (352,8 kHz) ou de 384 kHz servi à une zone à la cadence du CD.
// À 512 coefficients ils rendaient 19 698 et 19 526 Hz.
//
// Ils ne passent pas par la référence sinc : ses 1 025 coefficients sont posés
// à la cadence d'ENTRÉE, et à 352,8 kHz sa transition (≈ 3 kHz) dépasserait
// les 2 050 Hz qui séparent 20 kHz de Nyquist bas. Ce que ces témoins
// affirment ne demande pas de référence : une bande et un gain, mesurés sur
// Tune seul.

fn affirme_bande_mesuree(de: u32, vers: u32, nom: &str) {
    let module = reponse_de_tune(de, vers);
    let nyq_utile = 0.5 * de.min(vers) as f64;
    let (bande_hz, ondulation_db) = bande_et_ondulation(&module, nyq_utile);
    let x20 = sinus(de, 20_000.0, DUREE_S);
    let piste20 = tune_piste(&x20, de, vers);
    let n20 = piste20.len();
    let (amp_20k, _, _) = ajuster_sinus(&piste20, vers, 20_000.0, n20 / 10, n20 * 9 / 10);
    let gain_20k_db = db(amp_20k / AMPLITUDE);
    let noyau = parametres_sinc(de, vers).sinc_len;
    eprintln!(
        "[T10 #4080] {nom} : noyau {noyau}, bande à −0,1 dB = {bande_hz:.0} Hz, \
         gain à 20 kHz = {gain_20k_db:.3} dB, ondulation {ondulation_db:.4} dB"
    );
    assert!(
        bande_hz >= AUDIOPHILE_BANDE_HZ && gain_20k_db > -0.1,
        "{nom} : noyau {noyau} coefficients, bande à −0,1 dB MESURÉE = {bande_hz:.0} Hz, \
         gain à 20 kHz = {gain_20k_db:.2} dB ; Tune promet 20 kHz à −0,1 dB. Ce couple est \
         l'un des deux qui demandent 1 024 coefficients (#4080) : 512 n'y rendent que \
         19 698 / 19 526 Hz"
    );
}

#[test]
fn audiophile_bande_20k_352_8_vers_44_1() {
    affirme_bande_mesuree(352_800, 44_100, "352,8 → 44,1 kHz (PCM de DSD256)");
}

#[test]
fn audiophile_bande_20k_384_vers_44_1() {
    affirme_bande_mesuree(384_000, 44_100, "384 → 44,1 kHz");
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

/// Le signal continu verifie directement le gain du filtre, sans recalage de
/// phase ni ajustement de sinus : les queues du sinc ne doivent pas biaiser
/// le niveau constant. On exclut seulement les transitoires des deux bords.
#[test]
fn i4079_le_gain_continu_de_tune_reste_unitaire_sur_les_sept_rapports() {
    for r in RAPPORTS {
        let entree = vec![0.5f32; r.de as usize / 4];
        let sortie = tune_piste(&entree, r.de, r.vers);
        let milieu = &sortie[sortie.len() / 4..sortie.len() * 3 / 4];
        let pire = milieu
            .iter()
            .map(|&x| (x / 0.5 - 1.0).abs())
            .fold(0.0f64, f64::max);
        eprintln!("[4079 DC] {} : erreur relative maximale {pire:.9e}", r.nom);
        assert!(
            pire < 1e-6,
            "{} : gain continu relatif hors de 1 ± 1e-6 : {pire:.9e}",
            r.nom
        );
    }
}

// ══════════════════════════════════════════════════════════════════════════
// #4754 — interpolation Linéaire contre Cubique : qualité ET coût processeur
// ══════════════════════════════════════════════════════════════════════════
//
// `parametres_sinc` fixe `interpolation: SincInterpolationType::Linear`. Ce
// banc ne change RIEN d'autre : même `sinc_len`, même `f_cutoff`, même
// fenêtre, même sur-échantillonnage (256 phases), même taille de bloc, même
// `FixedAsync::Input`, même fonction de production `rubato_resample_chunk`,
// même recadrage par `alignement_de_piste`. Seul le mode d'interpolation
// ENTRE les phases de la table diffère.
//
// La comparaison est faite contre la MÊME référence indépendante que le reste
// du fichier (sinc polyphase exacte, Kaiser β = 14, f64) : les deux chiffres
// se lisent donc sur la même échelle, et aucun des deux n'est comparé à
// lui-même.
//
// Témoins `#[ignore]` : ce sont des mesures, pas des gardes. Elles changent
// avec la machine.
//
// ```text
// cargo test --release -p tune-core --test reechantillonnage_reference_2218 \
//     -- --ignored --nocapture interpolation_4754
// ```
mod interpolation_4754 {
    use super::*;
    use rubato::{Async, FixedAsync, SincInterpolationType};
    use std::time::Instant;

    /// La taille de bloc de `new_streaming_resampler`.
    const BLOC: usize = 1_024;

    fn resampleur(interp: SincInterpolationType, de: u32, vers: u32, canaux: u16) -> Async<f32> {
        let mut params = parametres_sinc(de, vers);
        params.interpolation = interp;
        Async::<f32>::new_sinc(
            vers as f64 / de as f64,
            1.1,
            &params,
            BLOC,
            canaux as usize,
            FixedAsync::Input,
        )
        .expect("paramètres valides")
    }

    /// Une piste passée par le chemin en FLUX de la production, recadrée comme
    /// `rubato_resample_track` : la sortie est directement comparable à la
    /// référence et à `tune_piste`.
    fn piste_interp(interp: SincInterpolationType, x: &[f32], de: u32, vers: u32) -> Vec<f64> {
        let (pre_roll, a_retirer, _) = alignement_de_piste(de, vers);
        let attendu = Reference::new(de, vers, 0.0).trames_de_sortie(x.len());
        let mut entree = vec![0.0f32; pre_roll];
        entree.extend_from_slice(x);

        let mut r = Some(resampleur(interp, de, vers, 1));
        let mut reste = Vec::new();
        let mut sortie: Vec<f32> = Vec::new();
        for morceau in entree.chunks(BLOC) {
            sortie.extend(rubato_resample_chunk(&mut r, morceau, 1, false, &mut reste));
        }
        sortie.extend(rubato_resample_chunk(&mut r, &[], 1, true, &mut reste));
        en_f64(&sortie)
            .into_iter()
            .skip(a_retirer)
            .take(attendu)
            .collect()
    }

    /// La réponse en fréquence réelle de ce mode d'interpolation, par
    /// impulsion — même recette que `reponse_de_tune`, mode choisi.
    fn reponse(interp: SincInterpolationType, de: u32, vers: u32) -> impl Fn(f64) -> f64 {
        let rapport = vers as f64 / de as f64;
        let pos = de as usize / 10;
        let xi = impulsion(de, 0.25, pos);
        let h = piste_interp(interp, &xi, de, vers);
        let centre = ((pos as f64 * rapport).round() as usize).min(h.len());
        let fen = 4_096.min(centre);
        let h = h[centre - fen..(centre + fen).min(h.len())].to_vec();
        move |f: f64| module_a(&h, vers, f) / rapport
    }

    struct Releve {
        err_sinus_db: f64,
        thd_n_1k_db: f64,
        err_balayage_db: f64,
        rejection_db: f64,
        bande_01db_hz: f64,
        ondulation_db: f64,
    }

    fn relever(interp: SincInterpolationType, r: Rapport) -> Releve {
        let Rapport { de, vers, .. } = r;
        let nyq_utile = 0.5 * de.min(vers) as f64;

        // sinus 1 kHz, aligné sur son propre délai de phase
        let x = sinus(de, 1_000.0, DUREE_S);
        let piste = piste_interp(interp, &x, de, vers);
        let ref0 = Reference::new(de, vers, 0.0);
        let y0 = ref0.appliquer(&en_f64(&x));
        let n = piste.len().min(y0.len());
        let (c0, c1) = (n / 10, n * 9 / 10);
        let delai = delai_par_phase(&piste, &y0, vers, 1_000.0, c0, c1);
        let refd = Reference::new(de, vers, delai);
        let yd = refd.appliquer(&en_f64(&x));
        let err_sinus_db = erreur_rms_db(&piste, &yd, c0, c1);
        let (amp, _, residu) = ajuster_sinus(&piste, vers, 1_000.0, c0, c1);
        let thd_n_1k_db = db(residu / (amp / 2f64.sqrt()));

        // balayage 20 Hz → 20 kHz
        let xb = balayage(de, DUREE_S);
        let pisteb = piste_interp(interp, &xb, de, vers);
        let yb = refd.appliquer(&en_f64(&xb));
        let nb = pisteb.len().min(yb.len());
        let err_balayage_db = erreur_rms_db(&pisteb, &yb, nb / 10, nb * 9 / 10);

        // réponse en fréquence et réjection — même méthode que `mesurer`
        let module = reponse(interp, de, vers);
        let (bande_01db_hz, ondulation_db) = bande_et_ondulation(&module, nyq_utile);
        let rejection_db = if vers > de {
            let (nyq_in, nyq_out) = (0.5 * de as f64, 0.5 * vers as f64);
            let mut pire: f64 = -400.0;
            let mut f = nyq_in + 0.25 * (nyq_out - nyq_in);
            while f <= nyq_out {
                pire = pire.max(db(module(f)));
                f += 50.0;
            }
            pire
        } else {
            let (nyq_in, nyq_out) = (0.5 * de as f64, 0.5 * vers as f64);
            let f_ton = nyq_out + 0.25 * (nyq_in - nyq_out);
            let pt = piste_interp(interp, &sinus(de, f_ton, DUREE_S), de, vers);
            let nt = pt.len();
            db(rms(&pt[nt / 10..nt * 9 / 10]) / (AMPLITUDE / 2f64.sqrt()))
        };

        Releve {
            err_sinus_db,
            thd_n_1k_db,
            err_balayage_db,
            rejection_db,
            bande_01db_hz,
            ondulation_db,
        }
    }

    /// Contrôle de fidélité du banc : l'arme « Linéaire » de CE banc doit
    /// rendre le même signal que la production. Sans ce témoin, les deux
    /// colonnes pourraient parler d'un chemin que Tune n'emprunte pas.
    #[test]
    fn le_banc_4754_reproduit_bien_le_chemin_de_production() {
        for r in RAPPORTS {
            let x = sinus(r.de, 1_000.0, 0.2);
            let mien = piste_interp(SincInterpolationType::Linear, &x, r.de, r.vers);
            let prod = tune_piste(&x, r.de, r.vers);
            let n = mien.len().min(prod.len());
            assert!(n > 0, "{} : piste vide", r.nom);
            let pire = (0..n)
                .map(|i| (mien[i] - prod[i]).abs())
                .fold(0.0, f64::max);
            assert!(
                pire < 1e-6,
                "{} : le banc #4754 diverge de la production ({pire:.3e})",
                r.nom
            );
        }
    }

    /// QUALITÉ — Linéaire contre Cubique, sur les sept rapports.
    #[test]
    #[ignore = "mesure #4754, pas une garde"]
    fn banc_4754_qualite_lineaire_contre_cubique() {
        println!("\n=== #4754 — QUALITÉ : Linéaire (production) contre Cubique ===");
        println!(
            "{:<18} {:<9} {:>9} {:>9} {:>9} {:>9} {:>10} {:>8}",
            "rapport", "interp", "err.sin", "THD+N", "err.bal", "réject.", "bande-.1dB", "ondul."
        );
        for r in RAPPORTS {
            let ligne = |nom: &str, interp: SincInterpolationType| {
                let m = relever(interp, r);
                println!(
                    "{:<18} {nom:<9} {:>8.1}dB {:>8.1}dB {:>8.1}dB {:>8.1}dB {:>8.0}Hz {:>7.3}dB",
                    r.nom,
                    m.err_sinus_db,
                    m.thd_n_1k_db,
                    m.err_balayage_db,
                    m.rejection_db,
                    m.bande_01db_hz,
                    m.ondulation_db
                );
                m
            };
            let l = ligne("Linéaire", SincInterpolationType::Linear);
            let c = ligne("Cubique", SincInterpolationType::Cubic);
            println!(
                "{:<18} {:<9} {:>+8.1}dB {:>+8.1}dB {:>+8.1}dB {:>+8.1}dB",
                "",
                "Δ (C−L)",
                c.err_sinus_db - l.err_sinus_db,
                c.thd_n_1k_db - l.thd_n_1k_db,
                c.err_balayage_db - l.err_balayage_db,
                c.rejection_db - l.rejection_db
            );
        }
        println!("\nPlus NÉGATIF = meilleur pour err.sin / THD+N / err.bal / réjection.");
        println!(
            "Repère : le plancher d'un mot de 24 bits est −144 dBFS ; d'un 16 bits, −96 dBFS."
        );
    }

    /// COÛT — le temps processeur des deux modes, sur le chemin en flux.
    #[test]
    #[ignore = "mesure #4754, pas une garde"]
    fn banc_4754_cout_processeur_lineaire_contre_cubique() {
        const SECONDES: f64 = 10.0;
        const PASSES: usize = 5;
        const CANAUX: u16 = 2;

        println!(
            "\n=== #4754 — COÛT : {SECONDES} s de stéréo, blocs de {BLOC}, meilleur de {PASSES} ==="
        );
        println!(
            "{:<18} {:>12} {:>12} {:>9} {:>12} {:>12}",
            "rapport", "Linéaire ms", "Cubique ms", "rapport", "Lin. % cœur", "Cub. % cœur"
        );

        for r in RAPPORTS {
            let trames = (r.de as f64 * SECONDES) as usize;
            let x: Vec<f32> = (0..trames * CANAUX as usize)
                .map(|i| (0.5 * (i as f64 * 0.017).sin()) as f32)
                .collect();

            let mesurer = |interp: SincInterpolationType| -> f64 {
                let mut meilleur = f64::INFINITY;
                for _ in 0..PASSES {
                    let mut res = Some(resampleur(interp, r.de, r.vers, CANAUX));
                    let mut reste = Vec::new();
                    let t0 = Instant::now();
                    for morceau in x.chunks(BLOC * CANAUX as usize) {
                        std::hint::black_box(rubato_resample_chunk(
                            &mut res, morceau, CANAUX, false, &mut reste,
                        ));
                    }
                    std::hint::black_box(rubato_resample_chunk(
                        &mut res,
                        &[],
                        CANAUX,
                        true,
                        &mut reste,
                    ));
                    meilleur = meilleur.min(t0.elapsed().as_secs_f64() * 1e3);
                }
                meilleur
            };

            let lin = mesurer(SincInterpolationType::Linear);
            let cub = mesurer(SincInterpolationType::Cubic);
            println!(
                "{:<18} {lin:>12.2} {cub:>12.2} {:>8.2}× {:>11.2}% {:>11.2}%",
                r.nom,
                cub / lin.max(1e-9),
                lin / (SECONDES * 1e3) * 100.0,
                cub / (SECONDES * 1e3) * 100.0
            );
        }
        println!("\n« % cœur » = part d'UN cœur consommée pour tenir le temps réel stéréo.");
    }
}
