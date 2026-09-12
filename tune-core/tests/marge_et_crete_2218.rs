//! T9 de #2218 — marge, écrêtage, crête vraie et dither : **témoins d'abord**.
//!
//! La case « Headroom, clipping, true peak et dithering testés » de l'épopée
//! n'avait ni témoin ni protocole. Ce fichier répond à quatre questions par
//! des tests qui rougissent, chacun AFFIRMANT le comportement **mesuré tel
//! qu'il est aujourd'hui** — pas tel qu'on le voudrait. Quand ce comportement
//! est un défaut, un second témoin `#[ignore = "défaut connu : …"]` affirme
//! le comportement attendu, pour que le correctif le dé-ignore.
//!
//! 1. Le gain logiciel peut-il porter un échantillon au-delà de 0 dBFS, et
//!    où est-il écrêté, par quoi, et est-ce nommé ? (`q1_*`)
//! 2. Une chaîne ReplayGain + égaliseur à gain positif produit-elle des
//!    inter-échantillons au-dessus de 0 dBFS ? Crête vraie mesurée par
//!    suréchantillonnage ×4 écrit ICI, dans le test. (`q2_*`)
//! 3. Le passage flottant → entier applique-t-il un dither, une troncature ou
//!    un arrondi, et lequel, étage par étage ? (`q3_*`)
//! 4. Étages désarmés : identité octet pour octet ? (`q4_*`)
//!
//! # Portes utilisées — toutes PUBLIQUES
//!
//! `audio::replaygain::{gain_factor, apply_gain_pcm}`,
//! `audio::eq::{EqProfile, EqBandSpec, EqProcessor}`,
//! `audio::mixer::PcmMixer::apply_gain`, `audio::convolver::Convolver`,
//! `audio::crossfeed::CrossfeedProcessor`, `audio::decode::convert_pcm_bytes`.
//! L'ordre de la chaîne du bras progressif est celui de
//! `orchestrator.rs` (`PorteurDsp::process`) : ReplayGain, puis égaliseur,
//! puis convolveur, puis crossfeed.
//!
//! # Ce qui n'est PAS témoignable d'ici — « sans porte de test »
//!
//! * `outputs::local::f32_to_native_i32` (arrondi + saturation, sans dither),
//!   `pcm_bytes_to_native_i32` et la garde `local_dsp_is_identity` du mode
//!   PURE sont privés à `outputs/local.rs`, derrière `local-audio`. La preuve
//!   d'identité décodeur → puits existe déjà : T8,
//!   `outputs/local/capture_bout_en_bout_2218.rs`.
//! * `decode::StreamingPcmByteAdapter::resample` (troncature vers zéro après
//!   le SRC) est `pub(crate)`.
//! * Le chemin cpal en flottant (macOS/Linux) ne sature RIEN après l'égaliseur
//!   flottant : ce qui dépasse 1,0 part tel quel au pilote.
//!
//! # Témoins existants, cités et non doublés
//!
//! * `audio::eq::tests::processed_integer_silence_receives_zero_mean_tpdf_dither`
//!   (TPDF sur le silence), `float_path_reports_overs_without_hiding_them`
//!   (overs flottants, par un champ privé), `boosted_full_scale_signal_needs_no_hidden_saturator`.
//! * `audio::volume_scale::tests::{le_plafond_est_l_unite, un_db_positif_est_refuse_pas_rabote,
//!   demande_lineaire_hors_bornes_ramenee_sans_paniquer}` : le volume
//!   utilisateur ne dépasse jamais l'unité — pas de témoin ici.
//! * `audio::crossfeed::tests::zero_amount_is_identity`.
//! * `audio::analyzer::tests::true_peak_sees_the_inter_sample_over_that_sample_peak_misses`
//!   (le mètre Catmull-Rom de l'ANALYSE, pas de la lecture).
//!
//! `tune-core` porte `autotests = false` : ce fichier est une cible `[[test]]`
//! du manifeste, sinon il ne serait jamais compilé.

use std::f64::consts::PI;

use tune_core::audio::convolver::Convolver;
use tune_core::audio::crossfeed::CrossfeedProcessor;
use tune_core::audio::decode::convert_pcm_bytes;
use tune_core::audio::eq::{EqBandSpec, EqProcessor, EqProfile};
use tune_core::audio::mixer::PcmMixer;
use tune_core::audio::replaygain::{
    ReplayGainMode, ReplayGainSettings, TrackGain, apply_gain_pcm, gain_factor,
};

const FS: u32 = 44_100;
/// Une seconde : assez pour que les biquads résonnants soient établis.
const N: usize = 44_100;

// ───────────────────────── signaux synthétiques déterministes ─────────────────────────

fn amplitude(dbfs: f64) -> f64 {
    10f64.powf(dbfs / 20.0)
}

fn dbfs(x: f64) -> f64 {
    20.0 * x.abs().log10()
}

fn sinus(freq: f64, dbfs_crete: f64, n: usize, phase: f64) -> Vec<f64> {
    let a = amplitude(dbfs_crete);
    (0..n)
        .map(|i| a * (2.0 * PI * freq * i as f64 / FS as f64 + phase).sin())
        .collect()
}

/// Carré numérique (±A, sans bande limitée) — le pire cas de crête vraie.
fn carre(freq: f64, dbfs_crete: f64, n: usize) -> Vec<f64> {
    let a = amplitude(dbfs_crete);
    (0..n)
        .map(|i| {
            if (2.0 * PI * freq * i as f64 / FS as f64).sin() >= 0.0 {
                a
            } else {
                -a
            }
        })
        .collect()
}

fn impulsion(dbfs_crete: f64, n: usize, position: usize) -> Vec<f64> {
    let mut x = vec![0.0; n];
    x[position] = amplitude(dbfs_crete);
    x
}

// ───────────────────────── PCM entier petit-boutien ─────────────────────────

fn pleine_echelle(bits: u16) -> f64 {
    (1i64 << (bits - 1)) as f64
}

fn rail(bits: u16) -> i64 {
    (1i64 << (bits - 1)) - 1
}

/// Quantification de SYNTHÈSE : arrondi au plus proche, saturé. C'est
/// l'entrée des étages, pas l'objet de la mesure.
fn vers_pcm(x: &[f64], bits: u16) -> Vec<u8> {
    let fe = pleine_echelle(bits);
    let mut out = Vec::with_capacity(x.len() * (bits as usize / 8));
    for &v in x {
        let raw = (v * fe).round().clamp(-fe, fe - 1.0) as i64;
        ecrire(&mut out, raw, bits);
    }
    out
}

fn ecrire(out: &mut Vec<u8>, raw: i64, bits: u16) {
    match bits {
        16 => out.extend_from_slice(&(raw as i16).to_le_bytes()),
        24 => out.extend_from_slice(&(raw as i32).to_le_bytes()[..3]),
        32 => out.extend_from_slice(&(raw as i32).to_le_bytes()),
        _ => unreachable!("profondeur {bits}"),
    }
}

fn depuis_pcm(pcm: &[u8], bits: u16) -> Vec<i64> {
    match bits {
        16 => pcm
            .chunks_exact(2)
            .map(|b| i64::from(i16::from_le_bytes([b[0], b[1]])))
            .collect(),
        24 => pcm
            .chunks_exact(3)
            .map(|b| i64::from((i32::from_le_bytes([0, b[0], b[1], b[2]])) >> 8))
            .collect(),
        32 => pcm
            .chunks_exact(4)
            .map(|b| i64::from(i32::from_le_bytes([b[0], b[1], b[2], b[3]])))
            .collect(),
        _ => unreachable!("profondeur {bits}"),
    }
}

fn normalise(raw: &[i64], bits: u16) -> Vec<f64> {
    let fe = pleine_echelle(bits);
    raw.iter().map(|&v| v as f64 / fe).collect()
}

fn crete_echantillon(x: &[f64]) -> f64 {
    x.iter().fold(0.0f64, |m, v| m.max(v.abs()))
}

/// Échantillons posés SUR le rail (plafond positif ou plancher négatif).
fn au_rail(raw: &[i64], bits: u16) -> usize {
    let r = rail(bits);
    raw.iter().filter(|&&v| v == r || v == -r - 1).count()
}

/// Échantillons au rail À 1 LSB PRÈS : ce que laisse un écrêtage dur SUIVI
/// d'un dither ±1 LSB (`EqProcessor::write_sample_f64` sature à 1,0 − 1 LSB,
/// puis ajoute le dither, puis arrondit).
fn au_rail_a_1_lsb_pres(raw: &[i64], bits: u16) -> usize {
    let r = rail(bits);
    raw.iter().filter(|&&v| v >= r - 1 || v <= -r).count()
}

/// Écrêtage mesuré contre la valeur IDÉALE (avant saturation) : nombre
/// d'échantillons dont l'idéal aurait arrondi AU-DELÀ du rail, et excès
/// maximal en LSB.
fn ecretes(ideal_raw: &[f64], bits: u16) -> (usize, f64) {
    let r = rail(bits) as f64;
    let mut n = 0;
    let mut exces = 0.0f64;
    for &v in ideal_raw {
        let e = if v > r + 0.5 {
            v - r
        } else if v < -r - 1.5 {
            -r - 1.0 - v
        } else {
            continue;
        };
        n += 1;
        exces = exces.max(e);
    }
    (n, exces)
}

// ───────────────────────── crête vraie : suréchantillonnage ×4 ─────────────────────────

/// Crête vraie par suréchantillonnage ×4 — interpolation sinc à fenêtre de
/// Blackman-Harris (4 termes, lobes secondaires à −92 dB), 2·L + 1 = 65
/// coefficients par point interpolé, normalisée en continu. ITU-R BS.1770-5
/// (annexe 2) prescrit un facteur ≥ 4 à 44,1/48 kHz. Écrit ici pour que la
/// mesure ne dépende d'aucun code de production ni d'une caisse nouvelle ; sa
/// propre contre-épreuve est `q2_le_metre_de_crete_vraie_*`. Les L premiers
/// et derniers points ne sont pas interpolés : le signal y est tronqué net
/// (zéro au-delà), et la reconstruction de cet échelon-là dépasserait de
/// 13,7 % du saut — un artefact du bord du tampon, pas une crête du signal.
fn crete_vraie_x4(x: &[f64]) -> f64 {
    const L: isize = 32;
    let mut max = crete_echantillon(x);
    for n in L..(x.len() as isize - L) {
        for p in 1..4 {
            let frac = p as f64 / 4.0;
            let (mut acc, mut poids) = (0.0, 0.0);
            for k in -L..=L {
                let t = k as f64 - frac;
                let sinc = if t == 0.0 {
                    1.0
                } else {
                    (PI * t).sin() / (PI * t)
                };
                let u = PI * t / (L as f64 + 1.0);
                let fenetre = 0.35875
                    + 0.48829 * u.cos()
                    + 0.14128 * (2.0 * u).cos()
                    + 0.01168 * (3.0 * u).cos();
                let idx = n + k;
                let v = if idx >= 0 && (idx as usize) < x.len() {
                    x[idx as usize]
                } else {
                    0.0
                };
                acc += v * sinc * fenetre;
                poids += sinc * fenetre;
            }
            max = max.max((acc / poids).abs());
        }
    }
    max
}

fn dbtp(x: &[f64]) -> f64 {
    dbfs(crete_vraie_x4(x))
}

// ───────────────────────── classification flottant → entier ─────────────────────────

/// Nomme la quantification d'un étage à partir de l'erreur `sortie − idéal`
/// (en LSB), échantillon par échantillon, contre le signe de l'idéal.
fn classer_quantification(ideal_raw: &[f64], sortie: &[i64]) -> &'static str {
    let mut max_abs = 0.0f64;
    let mut vers_zero_seulement = true;
    let mut vers_moins_inf_seulement = true;
    for (&i, &s) in ideal_raw.iter().zip(sortie) {
        let e = s as f64 - i;
        max_abs = max_abs.max(e.abs());
        if (i > 0.0 && e > 1e-9) || (i < 0.0 && e < -1e-9) {
            vers_zero_seulement = false;
        }
        if e > 1e-9 {
            vers_moins_inf_seulement = false;
        }
    }
    if max_abs <= 0.5 + 0.01 {
        "arrondi au plus proche"
    } else if max_abs < 1.0 + 1e-9 && vers_zero_seulement {
        "troncature vers zéro"
    } else if max_abs < 1.0 + 1e-9 && vers_moins_inf_seulement {
        "troncature vers −∞ (décalage)"
    } else {
        "bruit ajouté avant arrondi (dither)"
    }
}

fn reglages(prevent_clipping: bool, plafond_dbtp: f64) -> ReplayGainSettings {
    ReplayGainSettings {
        mode: ReplayGainMode::Track,
        preamp_db: 0.0,
        prevent_clipping,
        true_peak_ceiling_db: plafond_dbtp,
    }
}

fn bande(band_type: &str, freq: f64, gain: f64, q: f64) -> EqBandSpec {
    EqBandSpec {
        freq,
        gain,
        q,
        band_type: band_type.into(),
        ..Default::default()
    }
}

fn profil(bands: Vec<EqBandSpec>) -> EqProfile {
    EqProfile {
        enabled: true,
        bands,
        ..Default::default()
    }
}

// ═════════════════════════ Q1 — gain, écrêtage, nommage ═════════════════════════

/// Sinus 997 Hz à −0,1 dBFS, 16 bits ; ReplayGain +6 dB SANS pic tagué,
/// `prevent_clipping` armé. Mesuré : le facteur reste ×1,995 (+6 dB), 66 % des
/// échantillons sont écrêtés DUR (saturation, pas d'enroulement), et
/// `apply_gain_pcm` ne rend rien : ni compteur, ni journal.
#[test]
fn q1_replaygain_sans_pic_tague_porte_le_sinus_au_dela_de_0_dbfs_et_l_ecrete_dur_sans_le_dire() {
    let x = sinus(997.0, -0.1, N, 0.0);
    let mut pcm = vers_pcm(&x, 16);
    let entree = depuis_pcm(&pcm, 16);

    let facteur = gain_factor(
        TrackGain {
            gain_db: 6.0,
            peak: None,
        },
        reglages(true, 0.0),
    );
    assert!(
        (facteur - 1.9953).abs() < 1e-3,
        "sans pic tagué, prevent_clipping ne retient rien : facteur ×{facteur:.4} (+6 dB)"
    );
    assert_eq!(
        gain_factor(
            TrackGain {
                gain_db: 30.0,
                peak: None
            },
            reglages(true, 0.0)
        ),
        4.0,
        "le seul plafond du facteur est le clamp ×4 (+12 dB) de gain_factor"
    );

    let ideal: Vec<f64> = entree.iter().map(|&v| v as f64 * facteur).collect();
    apply_gain_pcm(&mut pcm, 16, facteur);
    let sortie = depuis_pcm(&pcm, 16);

    let (n_ecretes, exces_lsb) = ecretes(&ideal, 16);
    let n_rail = au_rail(&sortie, 16);
    let crete = crete_echantillon(&normalise(&sortie, 16));
    eprintln!(
        "q1 replaygain sans pic : facteur ×{facteur:.4}, idéal crête {:+.2} dBFS, écrêtés {n_ecretes}/{N} ({:.1} %), excès max {exces_lsb:.0} LSB, au rail {n_rail}",
        dbfs(0.98855 * facteur),
        100.0 * n_ecretes as f64 / N as f64
    );
    assert!(
        n_ecretes > N * 6 / 10 && n_ecretes < N * 7 / 10,
        "écrêtage massif attendu (~66 %) : {n_ecretes}/{N}"
    );
    assert!(
        n_rail >= n_ecretes && n_rail - n_ecretes <= 4,
        "chaque échantillon écrêté est posé SUR le rail (saturation dure) : rail {n_rail}, écrêtés {n_ecretes}"
    );
    assert_eq!(
        sortie.iter().max(),
        Some(&32767),
        "plafond = +rail exactement"
    );
    assert_eq!(
        sortie.iter().min(),
        Some(&-32768),
        "plancher = −rail exactement"
    );
    assert!(
        entree
            .iter()
            .zip(&sortie)
            .all(|(&e, &s)| e == 0 || e.signum() == s.signum()),
        "aucun enroulement : le signe est conservé partout"
    );
    assert!(
        (crete - 1.0).abs() < 1e-4,
        "la crête d'échantillon sort à 0 dBFS pile : {crete}"
    );
}

/// Le comportement ATTENDU : `prevent_clipping` armé ⇒ aucun échantillon
/// écrêté, pic tagué ou non.
#[test]
#[ignore = "défaut connu : sans pic tagué, prevent_clipping n'empêche rien et apply_gain_pcm écrête dur sans compter (docs/mesures/2218-marge-ecretage-crete-vraie.md, issue A)"]
fn q1_defaut_connu_prevent_clipping_arme_ne_devrait_jamais_ecreter_meme_sans_pic_tague() {
    let x = sinus(997.0, -0.1, N, 0.0);
    let mut pcm = vers_pcm(&x, 16);
    let entree = depuis_pcm(&pcm, 16);
    let facteur = gain_factor(
        TrackGain {
            gain_db: 6.0,
            peak: None,
        },
        reglages(true, 0.0),
    );
    let ideal: Vec<f64> = entree.iter().map(|&v| v as f64 * facteur).collect();
    apply_gain_pcm(&mut pcm, 16, facteur);
    let (n_ecretes, _) = ecretes(&ideal, 16);
    assert_eq!(
        n_ecretes, 0,
        "prevent_clipping est armé : aucun échantillon ne devrait dépasser le rail"
    );
}

/// Sinus 997 Hz à −0,1 dBFS, 24 bits ; une bande passe-bas à 997 Hz, Q = 4.
/// Un passe-bas RBJ vaut |H(fc)| = Q, soit +12 dB à la résonance — et
/// `automatic_headroom_db` ne réserve RIEN pour un filtre « pass ». Mesuré :
/// préampli 0 dB, ~84 % d'overs comptés dans `EqProcessStats` (exposés par
/// `eq_overs`), écrêtés DUR par `write_sample_f64` à 1,0 − 1 LSB PUIS dithérés
/// (ils sortent au rail ou 1 LSB en dessous), sans journal.
#[test]
fn q1_l_egaliseur_entier_ne_reserve_rien_pour_un_passe_bas_resonnant_et_ecrete_dur_en_comptant() {
    let p = profil(vec![bande("low_pass", 997.0, 0.0, 4.0)]);
    assert_eq!(
        p.automatic_headroom_db(0),
        0.0,
        "réserve automatique : rien pour un passe-bas, quelle que soit sa résonance"
    );
    let mut eq = EqProcessor::new(&p, FS, 1);
    assert_eq!(eq.preamp_db(0), Some(0.0));

    let x = sinus(997.0, -0.1, N, 0.0);
    let mut pcm = vers_pcm(&x, 24);
    let stats = eq.process_pcm(&mut pcm, 24);
    let sortie = depuis_pcm(&pcm, 24);
    let n_rail = au_rail(&sortie, 24);
    let n_rail_1 = au_rail_a_1_lsb_pres(&sortie, 24);
    eprintln!(
        "q1 égaliseur passe-bas Q=4 : préampli {:?} dB, overs {}/{N} ({:.1} %), au rail {n_rail}, au rail à 1 LSB près {n_rail_1}, non finis {}",
        eq.preamp_db(0),
        stats.overs,
        100.0 * stats.overs as f64 / N as f64,
        stats.non_finite_samples
    );
    assert!(
        stats.overs > N as u64 * 3 / 4 && stats.overs < N as u64 * 9 / 10,
        "résonance +12 dB sur un signal à −0,1 dBFS : la majorité des échantillons dépasse (~84 %) : {}",
        stats.overs
    );
    assert!(
        n_rail_1 >= stats.overs.saturating_sub(4) as usize,
        "chaque over est écrêté DUR (rail ou rail − 1 LSB, le dither venant APRÈS la saturation) : {n_rail_1}, overs {}",
        stats.overs
    );
    assert!(
        n_rail < n_rail_1 && n_rail > n_rail_1 / 2,
        "le dither ±1 LSB répartit le plateau écrêté entre le rail et 1 LSB en dessous : {n_rail} / {n_rail_1}"
    );
    assert_eq!(sortie.iter().max(), Some(&8_388_607));
    assert_eq!(sortie.iter().min(), Some(&-8_388_608));
    assert_eq!(
        eq.process_stats().overs,
        stats.overs,
        "compté, cumulé — mais jamais journalisé"
    );
}

/// Le comportement ATTENDU : la réserve automatique couvre la résonance
/// (+20·log10(Q) dB) des passe-bas / passe-haut, et rien ne dépasse.
#[test]
#[ignore = "défaut connu : automatic_headroom_db ignore la résonance +20·log10(Q) dB des filtres low_pass/high_pass (issue B)"]
fn q1_defaut_connu_la_reserve_automatique_devrait_couvrir_la_resonance_d_un_passe_bas() {
    let p = profil(vec![bande("low_pass", 997.0, 0.0, 4.0)]);
    assert!(
        p.automatic_headroom_db(0) <= -12.0,
        "Q = 4 ⇒ +12,04 dB à fc : la réserve devrait être ≤ −12 dB, elle vaut {}",
        p.automatic_headroom_db(0)
    );
    let mut eq = EqProcessor::new(&p, FS, 1);
    let mut pcm = vers_pcm(&sinus(997.0, -0.1, N, 0.0), 24);
    let stats = eq.process_pcm(&mut pcm, 24);
    assert_eq!(stats.overs, 0, "aucun over avec une réserve correcte");
}

/// Même profil, chemin FLOTTANT (sortie locale) : les overs sont comptés et
/// laissés tels quels, jusqu'à ×3,95. C'est documenté comme voulu (le
/// saturateur est plus loin — `f32_to_native_i32`, privé, ou personne sur le
/// chemin cpal flottant).
#[test]
fn q1_l_egaliseur_flottant_laisse_passer_les_overs_sans_les_ecreter() {
    let p = profil(vec![bande("low_pass", 997.0, 0.0, 4.0)]);
    let mut eq = EqProcessor::new(&p, FS, 1);
    let mut s: Vec<f32> = sinus(997.0, -0.1, N, 0.0)
        .iter()
        .map(|&v| v as f32)
        .collect();
    let stats = eq.process_interleaved(&mut s);
    let crete = s.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    eprintln!(
        "q1 égaliseur flottant : overs {}, crête {crete:.3} ({:+.2} dBFS)",
        stats.overs,
        dbfs(f64::from(crete))
    );
    assert!(stats.overs > N as u64 * 3 / 4);
    assert!(
        crete > 3.5 && crete < 4.2,
        "aucune saturation sur le chemin flottant : crête ×{crete:.3} (Q = 4 ⇒ ×4 attendu)"
    );
}

/// Carré 50 Hz à −0,05 dBFS, 24 bits, plateau grave 80 Hz +6 dB. La réserve
/// automatique retire 6 dB — la somme des gains positifs, c'est-à-dire le
/// maximum de la réponse en FRÉQUENCE. Mesuré : la réponse en TEMPS d'un
/// plateau d'ordre 2 dépasse ce maximum (sa norme L1 est plus grande que son
/// gain crête), et ~40 % des échantillons sortent du rail, écrêtés dur.
#[test]
fn q1_un_carre_a_moins_0_05_dbfs_sous_un_plateau_grave_reserve_depasse_quand_meme() {
    let p = profil(vec![bande("low_shelf", 80.0, 6.0, 0.707)]);
    let mut eq = EqProcessor::new(&p, FS, 1);
    assert_eq!(
        eq.preamp_db(0),
        Some(-6.0),
        "réserve = somme des gains positifs"
    );
    let x = carre(50.0, -0.05, N);
    let mut pcm = vers_pcm(&x, 24);
    let stats = eq.process_pcm(&mut pcm, 24);
    let sortie = normalise(&depuis_pcm(&pcm, 24), 24);

    // Le même signal sur le chemin flottant, non saturé : de combien la
    // réserve est-elle courte ?
    let mut flottant: Vec<f32> = x.iter().map(|&v| v as f32).collect();
    EqProcessor::new(&p, FS, 1).process_interleaved(&mut flottant);
    let crete_flottante = flottant.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    eprintln!(
        "q1 carré 50 Hz −0,05 dBFS + plateau grave 80 Hz +6 dB : overs {} ({:.1} %), crête entière {:+.2} dBFS, crête flottante {:+.2} dBFS (réserve courte de {:.2} dB)",
        stats.overs,
        100.0 * stats.overs as f64 / N as f64,
        dbfs(crete_echantillon(&sortie)),
        dbfs(f64::from(crete_flottante)),
        dbfs(f64::from(crete_flottante)) + 0.05
    );
    assert!(
        stats.overs > N as u64 * 35 / 100 && stats.overs < N as u64 * 45 / 100,
        "un plateau réservé en fréquence dépasse en temps : {} overs (~40 % attendus)",
        stats.overs
    );
    assert!(
        crete_flottante > 1.03 && crete_flottante < 1.15,
        "la réserve est courte d'environ 0,5 dB : crête flottante ×{crete_flottante:.3}"
    );
    assert!(dbfs(crete_echantillon(&sortie)) > -0.001, "écrêté au rail");
}

/// Le comportement ATTENDU : la réserve couvre aussi la réponse en TEMPS
/// (norme L1) d'un plateau, et aucun échantillon ne dépasse.
#[test]
#[ignore = "défaut connu : la réserve automatique est un maximum FRÉQUENTIEL ; un carré sous un plateau +6 dB dépasse de ~0,5 dB en temps (issue B)"]
fn q1_defaut_connu_la_reserve_automatique_devrait_couvrir_la_reponse_en_temps_d_un_plateau() {
    let p = profil(vec![bande("low_shelf", 80.0, 6.0, 0.707)]);
    let mut eq = EqProcessor::new(&p, FS, 1);
    let mut pcm = vers_pcm(&carre(50.0, -0.05, N), 24);
    let stats = eq.process_pcm(&mut pcm, 24);
    assert_eq!(stats.overs, 0, "aucun over sous un plateau réservé");
}

// ═════════════════════════ Q2 — crête vraie ═════════════════════════

/// Contre-épreuve du mètre : un sinus à fs/4 déphasé de π/4 n'a AUCUN
/// échantillon à sa crête (0,707·A) ; le mètre ×4 doit retrouver A. Et sur un
/// sinus dense (997 Hz) il ne doit rien inventer.
#[test]
fn q2_le_metre_de_crete_vraie_retrouve_la_crete_cachee_entre_deux_echantillons() {
    let cache = sinus(FS as f64 / 4.0, dbfs(0.9), 4096, PI / 4.0);
    let crete_ech = crete_echantillon(&cache);
    let crete_vraie = crete_vraie_x4(&cache);
    eprintln!(
        "q2 mètre : fs/4 déphasé — échantillon {crete_ech:.4} ({:+.2} dBFS), vraie {crete_vraie:.4} ({:+.2} dBTP)",
        dbfs(crete_ech),
        dbfs(crete_vraie)
    );
    assert!((crete_ech - 0.9 * 0.7071).abs() < 1e-3);
    assert!(
        (crete_vraie - 0.9).abs() < 0.9 * 0.003,
        "le mètre retrouve la crête cachée (+3 dB) à 0,3 % près : {crete_vraie}"
    );

    let dense = sinus(997.0, -0.1, N, 0.0);
    let tp = dbtp(&dense);
    assert!(
        (tp + 0.1).abs() < 0.02,
        "sur un sinus dense, crête vraie ≈ crête d'échantillon : {tp:+.3} dBTP"
    );
    let imp = impulsion(-0.1, 4096, 2048);
    assert!(
        (dbtp(&imp) + 0.1).abs() < 1e-9,
        "une impulsion isolée n'a pas de crête cachée : {:+.4} dBTP",
        dbtp(&imp)
    );
}

/// Un carré numérique à −0,05 dBFS porte DÉJÀ une crête vraie de ~+2,1 dBTP
/// avant tout traitement : la reconstruction à bande limitée d'un échelon
/// échantillonné dépasse de 13,7 % du saut (Σ sinc(½ − n) = 1,137), et le
/// saut d'un carré ±A vaut 2A — soit 1,274·A.
#[test]
fn q2_un_carre_a_moins_0_05_dbfs_depasse_deja_0_dbtp_avant_tout_traitement() {
    let x = carre(997.0, -0.05, N);
    let tp = dbtp(&x);
    eprintln!("q2 carré brut : crête échantillon −0,05 dBFS, crête vraie {tp:+.2} dBTP");
    assert!(
        tp > 1.9 && tp < 2.3,
        "échelon échantillonné : +2,1 dBTP attendu (1,274·A), mesuré {tp:+.2} dBTP"
    );
}

/// Carré −0,05 dBFS, 24 bits ; ReplayGain +6 dB avec le pic d'ÉCHANTILLON
/// tagué (0,9943 — ce qu'écrit tout tagueur externe), `prevent_clipping`.
/// Mesuré : le facteur est ramené à 1/pic, le plateau positif est posé 1 LSB
/// AU-DELÀ du rail (le plafond vise 1,0 = 2^23, non représentable), et la
/// crête vraie sort à ~+2,1 dBTP. Le plafond −1 dBTP (#1694) retire 1 dB, il
/// ne mesure rien (+1,1 dBTP). Avec le pic VRAI tagué (ce que l'analyse de
/// Tune écrit dans `rg_track_true_peak`), la crête vraie tient sous 0 dBTP :
/// le mécanisme est juste, c'est le tag qui manque.
#[test]
fn q2_replaygain_avec_pic_d_echantillon_tague_pose_le_pic_au_rail_et_laisse_la_crete_vraie_au_dessus_de_0_dbtp()
 {
    let x = carre(997.0, -0.05, N);
    let pic_echantillon = crete_echantillon(&x);
    let gain = TrackGain {
        gain_db: 6.0,
        peak: Some(pic_echantillon),
    };

    let facteur = gain_factor(gain, reglages(true, 0.0));
    assert!(
        (facteur * pic_echantillon - 1.0).abs() < 1e-9,
        "prevent_clipping ramène le facteur à 1/pic : ×{facteur:.5}"
    );
    let mut pcm = vers_pcm(&x, 24);
    let entree = depuis_pcm(&pcm, 24);
    let ideal: Vec<f64> = entree.iter().map(|&v| v as f64 * facteur).collect();
    apply_gain_pcm(&mut pcm, 24, facteur);
    let sortie = depuis_pcm(&pcm, 24);
    let (n_ecretes, exces) = ecretes(&ideal, 24);
    let tp0 = dbtp(&normalise(&sortie, 24));
    eprintln!(
        "q2 replaygain pic d'échantillon, plafond 0 dB : facteur ×{facteur:.5}, écrêtés {n_ecretes}/{N} (excès max {exces:.2} LSB), crête vraie {tp0:+.2} dBTP"
    );
    assert!(
        n_ecretes > N * 45 / 100 && n_ecretes < N * 55 / 100 && exces < 1.5,
        "tout le plateau positif dépasse le rail d'exactement 1 LSB : {n_ecretes}, excès {exces}"
    );
    assert!(
        tp0 > 1.9 && tp0 < 2.3,
        "inter-échantillons au-dessus de 0 dBTP malgré prevent_clipping : {tp0:+.2} dBTP"
    );

    let facteur_m1 = gain_factor(gain, reglages(true, -1.0));
    let mut pcm = vers_pcm(&x, 24);
    apply_gain_pcm(&mut pcm, 24, facteur_m1);
    let tp1 = dbtp(&normalise(&depuis_pcm(&pcm, 24), 24));
    eprintln!(
        "q2 replaygain pic d'échantillon, plafond −1 dBTP : facteur ×{facteur_m1:.5}, crête vraie {tp1:+.2} dBTP"
    );
    assert!(
        (tp1 - (tp0 - 1.0)).abs() < 0.05,
        "le plafond −1 dBTP de #1694 retire 1 dB et ne mesure rien : {tp1:+.2} dBTP"
    );

    let pic_vrai = crete_vraie_x4(&x);
    let facteur_vrai = gain_factor(
        TrackGain {
            gain_db: 6.0,
            peak: Some(pic_vrai),
        },
        reglages(true, 0.0),
    );
    let mut pcm = vers_pcm(&x, 24);
    apply_gain_pcm(&mut pcm, 24, facteur_vrai);
    let tp_vrai = dbtp(&normalise(&depuis_pcm(&pcm, 24), 24));
    eprintln!(
        "q2 replaygain pic VRAI tagué ({pic_vrai:.4}), plafond 0 dBTP : facteur ×{facteur_vrai:.5}, crête vraie {tp_vrai:+.3} dBTP"
    );
    assert!(
        tp_vrai <= 0.005 && tp_vrai > -0.1,
        "avec le pic vrai, prevent_clipping tient la crête vraie à 0 dBTP : {tp_vrai:+.3}"
    );
}

/// Le comportement ATTENDU : `prevent_clipping` avec plafond 0 dBTP tient la
/// crête VRAIE sous 0 dBTP, même quand seul un pic d'échantillon est tagué.
#[test]
#[ignore = "défaut connu : avec un pic d'ÉCHANTILLON tagué (sans rg_track_true_peak), prevent_clipping laisse passer jusqu'à +2,1 dBTP (issue C)"]
fn q2_defaut_connu_prevent_clipping_devrait_tenir_la_crete_vraie_sous_0_dbtp_avec_un_pic_d_echantillon()
 {
    let x = carre(997.0, -0.05, N);
    let gain = TrackGain {
        gain_db: 6.0,
        peak: Some(crete_echantillon(&x)),
    };
    let mut pcm = vers_pcm(&x, 24);
    apply_gain_pcm(&mut pcm, 24, gain_factor(gain, reglages(true, 0.0)));
    let tp = dbtp(&normalise(&depuis_pcm(&pcm, 24), 24));
    assert!(tp <= 0.0, "crête vraie {tp:+.2} dBTP");
}

/// La chaîne du bras progressif, dans son ordre : ReplayGain (+6 dB, pic
/// d'échantillon tagué) PUIS égaliseur (+6 dB de crête à 3 kHz, réserve
/// −6 dB), sur un sinus 997 Hz à −0,1 dBFS, 16 bits. Mesuré : le ReplayGain
/// pose le sinus au rail (0 dBFS, ≈ 0 dBTP), l'égaliseur le redescend
/// (~−4,5 dBFS) sans over.
#[test]
fn q2_la_chaine_replaygain_puis_egaliseur_sur_un_sinus_a_moins_0_1_dbfs() {
    let x = sinus(997.0, -0.1, N, 0.0);
    let pic = crete_echantillon(&x);
    let mut pcm = vers_pcm(&x, 16);

    let facteur = gain_factor(
        TrackGain {
            gain_db: 6.0,
            peak: Some(pic),
        },
        reglages(true, 0.0),
    );
    apply_gain_pcm(&mut pcm, 16, facteur);
    let apres_rg = normalise(&depuis_pcm(&pcm, 16), 16);
    let tp_rg = dbtp(&apres_rg);

    let mut eq = EqProcessor::new(&profil(vec![bande("peak", 3000.0, 6.0, 1.0)]), FS, 1);
    assert_eq!(eq.preamp_db(0), Some(-6.0));
    let stats = eq.process_pcm(&mut pcm, 16);
    let apres_eq = normalise(&depuis_pcm(&pcm, 16), 16);
    let tp_eq = dbtp(&apres_eq);
    eprintln!(
        "q2 chaîne RG→EQ : après RG crête {:+.3} dBFS / {tp_rg:+.3} dBTP ; après EQ crête {:+.2} dBFS / {tp_eq:+.2} dBTP, overs {}",
        dbfs(crete_echantillon(&apres_rg)),
        dbfs(crete_echantillon(&apres_eq)),
        stats.overs
    );
    let mots = depuis_pcm(&vers_pcm(&apres_rg, 16), 16);
    assert_eq!(
        mots.iter().max(),
        Some(&32767),
        "ReplayGain pose la crête au rail"
    );
    assert_eq!(mots.iter().min(), Some(&-32768), "… des deux côtés");
    assert!(
        tp_rg.abs() < 0.05,
        "au rail, crête vraie ≈ 0 dBTP : {tp_rg:+.3}"
    );
    assert_eq!(stats.overs, 0);
    assert!(tp_eq < -3.0 && tp_eq > -6.0, "{tp_eq:+.2} dBTP");
}

// ═════════════════════════ Q3 — flottant → entier ═════════════════════════

/// `apply_gain_pcm` : `as i16` / `as i32` après saturation = troncature VERS
/// ZÉRO, sans dither. Conséquence mesurable : un facteur de 1 − 1e-7 (−0,000001 dB,
/// inaudible) abaisse CHAQUE échantillon non nul d'exactement 1 LSB.
#[test]
fn q3_apply_gain_pcm_tronque_vers_zero_sans_dither() {
    let x = sinus(997.0, -20.0, N, 0.0);
    for bits in [16u16, 24, 32] {
        let mut pcm = vers_pcm(&x, bits);
        let entree = depuis_pcm(&pcm, bits);
        let facteur = amplitude(-1.0);
        let ideal: Vec<f64> = entree.iter().map(|&v| v as f64 * facteur).collect();
        apply_gain_pcm(&mut pcm, bits, facteur);
        let sortie = depuis_pcm(&pcm, bits);
        let classe = classer_quantification(&ideal, &sortie);
        eprintln!("q3 apply_gain_pcm {bits} bits, −1 dB : {classe}");
        assert_eq!(classe, "troncature vers zéro", "{bits} bits");
    }

    let mut pcm = vers_pcm(&sinus(997.0, -0.1, N, 0.0), 16);
    let entree = depuis_pcm(&pcm, 16);
    apply_gain_pcm(&mut pcm, 16, 1.0 - 1e-7);
    let sortie = depuis_pcm(&pcm, 16);
    let non_nuls = entree.iter().filter(|&&v| v != 0).count();
    let decales = entree
        .iter()
        .zip(&sortie)
        .filter(|&(&e, &s)| e != 0 && (s - e) == -e.signum())
        .count();
    eprintln!(
        "q3 apply_gain_pcm ×(1 − 1e-7) : {decales}/{non_nuls} échantillons non nuls décalés d'1 LSB vers zéro"
    );
    assert_eq!(
        decales, non_nuls,
        "un gain de −0,000001 dB déplace TOUT le signal d'1 LSB vers zéro"
    );
}

/// `PcmMixer::apply_gain` : même écriture (`SampleFormat::write`), même
/// troncature vers zéro, sans dither.
#[test]
fn q3_pcm_mixer_apply_gain_tronque_vers_zero_lui_aussi() {
    let x = sinus(997.0, -20.0, N, 0.0);
    for bits in [16u16, 24, 32] {
        let mut pcm = vers_pcm(&x, bits);
        let entree = depuis_pcm(&pcm, bits);
        let facteur = amplitude(-1.0);
        let ideal: Vec<f64> = entree
            .iter()
            .map(|&v| v as f64 * f64::from(facteur as f32))
            .collect();
        PcmMixer::apply_gain(&mut pcm, facteur as f32, bits).expect("profondeur mélangeable");
        let classe = classer_quantification(&ideal, &depuis_pcm(&pcm, bits));
        eprintln!("q3 PcmMixer::apply_gain {bits} bits, −1 dB : {classe}");
        assert_eq!(classe, "troncature vers zéro", "{bits} bits");
    }
}

/// `EqProcessor::process_pcm` : requantification avec dither TPDF ±1 LSB puis
/// arrondi, à TOUTE profondeur (16, 24 et 32 bits). Référence : le même
/// signal passé à 32 bits, où ±1 LSB vaut −186 dBFS.
#[test]
fn q3_l_egaliseur_entier_requantifie_avec_un_dither_tpdf_a_toute_profondeur() {
    let p = profil(vec![bande("high_shelf", 20_000.0, -1.0, 0.707)]);
    let x = sinus(997.0, -20.0, N, 0.0);

    for bits in [16u16, 24] {
        let mut pcm = vers_pcm(&x, bits);
        // Référence : les MÊMES mots, élargis à 32 bits sans arithmétique,
        // passés par la même cascade — l'idéal à `bits` près est ref32 / 2^(32−bits).
        let mut ref32 = Vec::with_capacity(pcm.len() / (bits as usize / 8) * 4);
        for raw in depuis_pcm(&pcm, bits) {
            ecrire(&mut ref32, raw << (32 - bits), 32);
        }
        EqProcessor::new(&p, FS, 1).process_pcm(&mut ref32, 32);
        let ideal: Vec<f64> = depuis_pcm(&ref32, 32)
            .iter()
            .map(|&v| v as f64 / f64::from(1u32 << (32 - bits)))
            .collect();
        EqProcessor::new(&p, FS, 1).process_pcm(&mut pcm, bits);
        let sortie = depuis_pcm(&pcm, bits);
        let classe = classer_quantification(&ideal, &sortie);
        let err: Vec<f64> = ideal
            .iter()
            .zip(&sortie)
            .map(|(i, &s)| s as f64 - i)
            .collect();
        let moyenne = err.iter().sum::<f64>() / err.len() as f64;
        let max = err.iter().fold(0.0f64, |m, e| m.max(e.abs()));
        eprintln!(
            "q3 EqProcessor::process_pcm {bits} bits : {classe}, erreur moyenne {moyenne:+.4} LSB, max {max:.2} LSB"
        );
        assert_eq!(classe, "bruit ajouté avant arrondi (dither)", "{bits} bits");
        assert!(
            max <= 1.5 + 1e-6,
            "TPDF ±1 LSB puis arrondi : |erreur| ≤ 1,5 LSB, {max}"
        );
        assert!(moyenne.abs() < 0.02, "dither centré : {moyenne}");
    }

    // 32 bits : le dither est là aussi (±1 LSB à 2^31), visible sur le silence.
    let mut silence = vec![0u8; 4 * 8192];
    EqProcessor::new(&p, FS, 1).process_pcm(&mut silence, 32);
    let vals = depuis_pcm(&silence, 32);
    assert!(
        vals.iter().any(|&v| v == 1) && vals.iter().any(|&v| v == -1),
        "le silence 32 bits ressort dithéré à ±1 LSB"
    );
}

/// `decode::convert_pcm_bytes` (24 → 16 bits, chemin du transcodage DLNA/WAV
/// 16 bits depuis une source 24 bits, et de la mémoire de préchargement) :
/// décalage arithmétique = troncature vers −∞, sans dither.
#[test]
fn q3_convert_pcm_bytes_reduit_24_vers_16_bits_par_decalage_sans_dither() {
    let mut cas = Vec::new();
    for raw in [384i64, 385, 383, -384, -385, -1, 1, 255] {
        ecrire(&mut cas, raw, 24);
    }
    let sortie = depuis_pcm(&convert_pcm_bytes(&cas, 24, 16), 16);
    assert_eq!(
        sortie,
        vec![1, 1, 1, -2, -2, -1, 0, 0],
        "385/256 = 1,504 → 1 (arrondi donnerait 2) ; −1/256 → −1 (vers −∞) ; 255/256 → 0"
    );

    let x = sinus(997.0, -20.0, N, 0.0);
    let pcm24 = vers_pcm(&x, 24);
    let ideal: Vec<f64> = depuis_pcm(&pcm24, 24)
        .iter()
        .map(|&v| v as f64 / 256.0)
        .collect();
    let classe =
        classer_quantification(&ideal, &depuis_pcm(&convert_pcm_bytes(&pcm24, 24, 16), 16));
    eprintln!("q3 convert_pcm_bytes 24→16 : {classe}");
    assert_eq!(classe, "troncature vers −∞ (décalage)");
}

/// Le comportement ATTENDU d'une réduction de profondeur audiophile : un
/// dither (TPDF) avant l'arrondi, pas un décalage.
#[test]
#[ignore = "défaut connu : convert_pcm_bytes réduit 24→16 bits par décalage, sans dither ni arrondi (issue D)"]
fn q3_defaut_connu_la_reduction_24_vers_16_bits_devrait_dither() {
    let x = sinus(997.0, -20.0, N, 0.0);
    let pcm24 = vers_pcm(&x, 24);
    let ideal: Vec<f64> = depuis_pcm(&pcm24, 24)
        .iter()
        .map(|&v| v as f64 / 256.0)
        .collect();
    let classe =
        classer_quantification(&ideal, &depuis_pcm(&convert_pcm_bytes(&pcm24, 24, 16), 16));
    assert_eq!(classe, "bruit ajouté avant arrondi (dither)");
}

/// `Convolver::process_pcm` : arrondi au plus proche, saturé à ±1,0, sans
/// dither — et une réponse impulsionnelle UNITÉ n'est pas l'identité : le
/// décodage divise par 32768 et l'encodage multiplie par 32767 (−0,00027 dB),
/// donc tout échantillon |x| ≥ 16384 (~66 % d'un sinus à −0,1 dBFS) perd 1 LSB.
#[test]
fn q3_le_convolveur_entier_arrondit_sans_dither_et_une_ir_unite_perd_1_lsb() {
    let x = sinus(997.0, -0.1, N, 0.0);
    let mut pcm = vers_pcm(&x, 16);
    let entree = depuis_pcm(&pcm, 16);
    let mut conv = Convolver::new(&[vec![1.0f32]], 256);
    conv.process_pcm(&mut pcm, 16);
    let sortie = depuis_pcm(&pcm, 16);
    let ideal: Vec<f64> = entree
        .iter()
        .map(|&v| v as f64 * 32767.0 / 32768.0)
        .collect();
    let classe = classer_quantification(&ideal, &sortie);
    let perdus = entree.iter().zip(&sortie).filter(|(e, s)| e != s).count();
    eprintln!(
        "q3 Convolver::process_pcm IR unité 16 bits : {classe}, {perdus}/{N} échantillons ≠ entrée"
    );
    assert_eq!(classe, "arrondi au plus proche");
    assert!(
        perdus > N * 60 / 100 && perdus < N * 72 / 100,
        "IR unité ≠ identité : {perdus} échantillons (|x| ≥ 16384, ~66 %) décalés d'1 LSB (gain 32767/32768)"
    );
}

// ═════════════════════════ Q4 — identité des étages désarmés ═════════════════════════

/// Chaque étage du bras progressif, DÉSARMÉ, rend les octets tels quels, à
/// 16, 24 et 32 bits : égaliseur `enabled: false`, égaliseur armé mais à
/// bandes neutres, ReplayGain `Off` (facteur 1,0 → retour immédiat),
/// `PcmMixer::apply_gain(1.0)` (non court-circuité, mais ×1 exact), crossfeed
/// à 0. Le convolveur n'a PAS d'état désarmé : il n'est pas dans la chaîne
/// quand il n'est pas chargé. La garde PURE de la sortie locale
/// (`local_dsp_is_identity` → `pcm_bytes_to_native_i32`) est privée :
/// non témoignable d'ici, prouvée par T8.
#[test]
fn q4_les_etages_desarmes_sont_l_identite_octet_pour_octet() {
    let x = sinus(997.0, -0.1, N, 0.3);
    let facteur_off = gain_factor(
        TrackGain {
            gain_db: 6.0,
            peak: None,
        },
        ReplayGainSettings {
            mode: ReplayGainMode::Off,
            ..reglages(true, 0.0)
        },
    );
    assert_eq!(facteur_off, 1.0, "mode Off ⇒ facteur exactement 1");

    for bits in [16u16, 24, 32] {
        let original = vers_pcm(&x, bits);

        let mut pcm = original.clone();
        apply_gain_pcm(&mut pcm, bits, facteur_off);
        assert_eq!(pcm, original, "ReplayGain Off, {bits} bits");

        let mut pcm = original.clone();
        let mut eq = EqProcessor::new(&EqProfile::default(), FS, 1);
        assert!(!eq.is_enabled());
        assert_eq!(eq.process_pcm(&mut pcm, bits), Default::default());
        assert_eq!(pcm, original, "égaliseur désactivé, {bits} bits");

        let mut pcm = original.clone();
        let mut eq = EqProcessor::new(&profil(vec![bande("peak", 1000.0, 0.0, 1.0)]), FS, 1);
        assert!(!eq.is_enabled(), "bandes neutres ⇒ non armé");
        eq.process_pcm(&mut pcm, bits);
        assert_eq!(pcm, original, "égaliseur à bandes neutres, {bits} bits");

        let mut pcm = original.clone();
        PcmMixer::apply_gain(&mut pcm, 1.0, bits).unwrap();
        assert_eq!(pcm, original, "PcmMixer ×1,0, {bits} bits");
    }

    let mut flottant: Vec<f32> = x.iter().map(|&v| v as f32).collect();
    let original = flottant.clone();
    EqProcessor::new(&EqProfile::default(), FS, 1).process_interleaved(&mut flottant);
    CrossfeedProcessor::new(FS, 0.0, 0.3).process_interleaved(&mut flottant);
    assert_eq!(flottant, original, "chemin flottant désarmé");
}
