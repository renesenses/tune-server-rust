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
//! `audio::replaygain::{gain_factor, playback_factor, apply_gain_pcm}`,
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

use std::f64::consts::{FRAC_1_SQRT_2, PI};

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
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i64::from(i16::from_le_bytes([b[0], b[1]])))
            .collect(),
        24 => pcm
            .as_chunks::<3>()
            .0
            .iter()
            .map(|b| i64::from((i32::from_le_bytes([0, b[0], b[1], b[2]])) >> 8))
            .collect(),
        32 => pcm
            .as_chunks::<4>()
            .0
            .iter()
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
/// `prevent_clipping` **désarmé**. Mesuré : le facteur reste ×1,995 (+6 dB),
/// 66 % des échantillons sont écrêtés DUR (saturation, pas d'enroulement).
///
/// 🔴 **#4072 a changé l'armement, pas la saturation.** Ce témoin mesurait le
/// même stimulus avec `prevent_clipping` ARMÉ, et c'était le défaut A de T9 :
/// le garde-fou ne retenait rien sans pic tagué. Depuis le correctif, armé, le
/// facteur ne dépasse plus l'unité — c'est le jumeau ci-dessous qui le tient.
/// Ce que ce témoin garde toujours, désarmé, c'est le comportement de
/// `apply_gain_pcm` lui-même : saturation dure au rail, signe conservé, aucun
/// enroulement. L'auditeur qui décoche la case obtient exactement cela, et les
/// chiffres de T9 restent mesurés, à l'échantillon près.
#[test]
fn q1_replaygain_sans_garde_fou_porte_le_sinus_au_dela_de_0_dbfs_et_l_ecrete_dur() {
    let x = sinus(997.0, -0.1, N, 0.0);
    let mut pcm = vers_pcm(&x, 16);
    let entree = depuis_pcm(&pcm, 16);

    let facteur = gain_factor(
        TrackGain {
            gain_db: 6.0,
            peak: None,
        },
        reglages(false, 0.0),
    );
    assert!(
        (facteur - 1.9953).abs() < 1e-3,
        "garde-fou désarmé : rien ne retient le facteur ×{facteur:.4} (+6 dB)"
    );
    assert_eq!(
        gain_factor(
            TrackGain {
                gain_db: 30.0,
                peak: None
            },
            reglages(false, 0.0)
        ),
        4.0,
        "désarmé, le seul plafond du facteur est le clamp ×4 (+12 dB) de gain_factor"
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

/// Le comportement ATTENDU, **tenu depuis #4072** : `prevent_clipping` armé ⇒
/// aucun échantillon écrêté, pic tagué ou non.
///
/// Le témoin était `#[ignore]` — c'était le défaut A de T9. Il est réveillé
/// par le correctif : sans pic tagué, `gain_factor` refuse le gain positif en
/// entier (l'unité, pas le plafond dBTP), et les octets sortent intacts.
///
/// Ce qu'il garde, et qui rougirait si le refus disparaissait : le nombre
/// d'écrêtés contre l'IDÉAL (0), le compteur du registre (0), et l'identité
/// octet pour octet du PCM — un facteur simplement raboté à 0,99 passerait le
/// premier et pas le troisième.
#[test]
fn q1_prevent_clipping_arme_n_ecrete_jamais_meme_sans_pic_tague() {
    let x = sinus(997.0, -0.1, N, 0.0);
    let mut pcm = vers_pcm(&x, 16);
    let original = pcm.clone();
    let entree = depuis_pcm(&pcm, 16);
    let facteur = gain_factor(
        TrackGain {
            gain_db: 6.0,
            peak: None,
        },
        reglages(true, 0.0),
    );
    eprintln!("q1 prevent_clipping armé, +6 dB sans pic : facteur ×{facteur:.6}");
    assert!(
        facteur <= 1.0,
        "sans pic tagué, le facteur ne doit jamais amplifier : ×{facteur:.6}"
    );
    let ideal: Vec<f64> = entree.iter().map(|&v| v as f64 * facteur).collect();
    apply_gain_pcm(&mut pcm, 16, facteur);
    let (n_ecretes, exces_lsb) = ecretes(&ideal, 16);
    assert_eq!(
        n_ecretes, 0,
        "prevent_clipping est armé : aucun échantillon ne devrait dépasser le rail (excès max {exces_lsb:.0} LSB)"
    );
    assert_eq!(
        pcm, original,
        "un gain refusé est l'identité : pas un octet ne bouge"
    );
    let sortie = depuis_pcm(&pcm, 16);
    assert_eq!(
        au_rail(&sortie, 16),
        0,
        "aucun échantillon posé sur le rail"
    );
}

/// Sinus 997 Hz à −0,1 dBFS, 24 bits ; une bande passe-bas à 997 Hz, Q = 4.
/// Un passe-bas RBJ vaut |H(fc)| = Q, soit +12,04 dB à la résonance — et
/// jusqu'à #4073 `automatic_headroom_db` ne réservait RIEN pour un filtre
/// « pass » : 0 dB de préampli, **36 896 / 44 100 overs (83,7 %)** écrêtés DUR
/// par `write_sample_f64`, comptés mais jamais journalisés.
///
/// Depuis #4073 la réserve regarde le Q : `20·log10(Q/0,707)` = **−15,05 dB**,
/// ce qui couvre le maximum fréquentiel exact (Q/√(1−1/4Q²) = +12,11 dB) ET la
/// norme L1 du même filtre (+14,19 dB, mesurée). Résultat : zéro over, rien au
/// rail, et la crête retombe à −3,11 dBFS.
#[test]
fn q1_l_egaliseur_entier_reserve_la_resonance_d_un_passe_bas_et_n_ecrete_plus() {
    let p = profil(vec![bande("low_pass", 997.0, 0.0, 4.0)]);
    let reserve = p.automatic_headroom_db(0);
    assert!(
        (reserve + 15.0515).abs() < 1e-3,
        "réserve = 20·log10(Q/0,707) pour Q = 4, soit −15,0515 dB : {reserve}"
    );
    let mut eq = EqProcessor::new(&p, FS, 1);
    assert_eq!(eq.preamp_db(0), Some(reserve));

    let x = sinus(997.0, -0.1, N, 0.0);
    let mut pcm = vers_pcm(&x, 24);
    let stats = eq.process_pcm(&mut pcm, 24);
    let sortie = depuis_pcm(&pcm, 24);
    let n_rail = au_rail(&sortie, 24);
    let n_rail_1 = au_rail_a_1_lsb_pres(&sortie, 24);
    let crete = crete_echantillon(&normalise(&sortie, 24));
    eprintln!(
        "q1 égaliseur passe-bas Q=4 : préampli {:?} dB, overs {}/{N}, au rail {n_rail}, au rail à 1 LSB près {n_rail_1}, crête {:+.2} dBFS, non finis {}",
        eq.preamp_db(0),
        stats.overs,
        dbfs(crete),
        stats.non_finite_samples
    );
    assert_eq!(
        stats.overs, 0,
        "la résonance est réservée : plus un seul échantillon ne dépasse"
    );
    assert_eq!(n_rail, 0, "plus rien au rail");
    assert_eq!(n_rail_1, 0, "ni à 1 LSB du rail");
    assert_eq!(stats.non_finite_samples, 0);
    assert_eq!(
        eq.process_stats().overs,
        0,
        "le compteur cumulé de la piste reste à zéro"
    );
}

/// Le comportement ATTENDU par #4073 — et désormais OBTENU. Le nom est
/// conservé tel quel : c'est celui que citent l'issue et
/// `docs/mesures/2218-marge-ecretage-crete-vraie.md`, et c'est ce témoin-là
/// qui devait passer de `#[ignore]` à vert.
#[test]
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

/// Le prix de la réserve, et la ligne qu'elle ne franchit PAS.
///
/// Un passe-haut de Butterworth (Q = 0,707) — le coupe-bas ordinaire — a un
/// maximum fréquentiel de 0 dB et une norme L1 de **+7,02 dB** : sur un carré
/// à 50 Hz il dépasse le rail. #4073 ne le réserve pas, et c'est un CHOIX :
/// couvrir cette norme-là coûterait 7 dB de niveau à tout utilisateur d'un
/// coupe-bas, pour un dépassement que seul un signal adverse atteint. Une
/// réserve trop large abîme le son autant qu'une réserve trop courte ; ce
/// témoin fige la ligne, chiffrée, pour que personne ne la déplace sans le
/// dire.
#[test]
fn q1_un_passe_haut_de_butterworth_ne_reserve_rien_et_c_est_assume() {
    let p = profil(vec![bande("high_pass", 997.0, 0.0, FRAC_1_SQRT_2)]);
    assert_eq!(
        p.automatic_headroom_db(0),
        0.0,
        "Q ≤ 0,707 : aucune résonance, aucune réserve"
    );
    let mut eq = EqProcessor::new(&p, FS, 1);
    let mut pcm = vers_pcm(&carre(50.0, -0.05, N), 24);
    let stats = eq.process_pcm(&mut pcm, 24);
    eprintln!(
        "q1 passe-haut Butterworth sur un carré 50 Hz : réserve 0 dB, overs {} / {N} ({:.1} %) — norme L1 du filtre +7,02 dB, non réservée",
        stats.overs,
        100.0 * stats.overs as f64 / N as f64
    );
    assert!(
        stats.overs > 0,
        "la norme L1 d'un passe-haut n'est pas réservée : le carré dépasse"
    );
}

/// Même profil, chemin FLOTTANT (sortie locale). Le chemin flottant ne sature
/// TOUJOURS rien — le saturateur est plus loin (`f32_to_native_i32`, privé, ou
/// personne sur le chemin cpal) — mais il n'a plus rien à laisser passer :
/// la réserve de #4073 ramène la crête de ×3,95 (+11,94 dBFS, mesuré avant) à
/// moins de l'unité, et le compteur d'overs tombe à zéro.
#[test]
fn q1_l_egaliseur_flottant_ne_deborde_plus_grace_a_la_reserve() {
    let p = profil(vec![bande("low_pass", 997.0, 0.0, 4.0)]);
    let mut eq = EqProcessor::new(&p, FS, 1);
    let mut s: Vec<f32> = sinus(997.0, -0.1, N, 0.0)
        .iter()
        .map(|&v| v as f32)
        .collect();
    let stats = eq.process_interleaved(&mut s);
    let crete = s.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    eprintln!(
        "q1 égaliseur flottant : préampli {:?} dB, overs {}, crête {crete:.3} ({:+.2} dBFS)",
        eq.preamp_db(0),
        stats.overs,
        dbfs(f64::from(crete))
    );
    assert_eq!(
        stats.overs, 0,
        "plus un over sur le chemin flottant non plus"
    );
    assert!(
        crete < 1.0,
        "la crête reste sous l'unité : ×{crete:.3} — rien à saturer, donc rien à confier au pilote"
    );
}

/// Carré 50 Hz à −0,05 dBFS, 24 bits, plateau grave 80 Hz +6 dB. La réserve
/// automatique retirait 6 dB — la somme des gains positifs, c'est-à-dire le
/// maximum de la réponse en FRÉQUENCE. Mesuré : la réponse en TEMPS d'un
/// plateau d'ordre 2 dépasse ce maximum (sa **norme L1 vaut 6,505 dB**, contre
/// 6,000 dB de gain crête), et 17 825 / 44 100 échantillons (40,4 %) sortaient
/// du rail, écrêtés dur.
///
/// Depuis #4073 la réserve prend le PLUS GRAND de la somme des gains et de la
/// norme L1 de la cascade : −6,505 dB, soit 0,50 dB de plus. Rien ne dépasse,
/// et la crête flottante s'arrête à ×0,9942 — la borne `max|y| ≤ ‖h‖₁·max|x|`
/// est serrée, ce n'est pas une marge de confort.
#[test]
fn q1_un_carre_a_moins_0_05_dbfs_sous_un_plateau_grave_tient_grace_a_la_norme_l1() {
    let p = profil(vec![bande("low_shelf", 80.0, 6.0, FRAC_1_SQRT_2)]);
    let mut eq = EqProcessor::new(&p, FS, 1);
    let reserve = eq.preamp_db(0).expect("un canal");
    assert!(
        (reserve + 6.5149).abs() < 1e-3,
        "réserve = norme L1 du plateau (6,505 dB) plus la marge de troncature de \
         #4594 (0,01), là où la somme des gains ne disait que 6,0 : {reserve}"
    );
    let x = carre(50.0, -0.05, N);
    let mut pcm = vers_pcm(&x, 24);
    let stats = eq.process_pcm(&mut pcm, 24);
    let sortie = normalise(&depuis_pcm(&pcm, 24), 24);

    // Le même signal sur le chemin flottant, non saturé : de combien la
    // réserve est-elle LARGE, maintenant ?
    let mut flottant: Vec<f32> = x.iter().map(|&v| v as f32).collect();
    EqProcessor::new(&p, FS, 1).process_interleaved(&mut flottant);
    let crete_flottante = flottant.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    eprintln!(
        "q1 carré 50 Hz −0,05 dBFS + plateau grave 80 Hz +6 dB : réserve {reserve:.4} dB, overs {} ({:.1} %), crête entière {:+.3} dBFS, crête flottante ×{crete_flottante:.4} ({:+.3} dBFS)",
        stats.overs,
        100.0 * stats.overs as f64 / N as f64,
        dbfs(crete_echantillon(&sortie)),
        dbfs(f64::from(crete_flottante))
    );
    assert_eq!(
        stats.overs, 0,
        "la norme L1 couvre la réponse en temps : plus aucun over"
    );
    assert!(
        crete_flottante < 1.0 && crete_flottante > 0.99,
        "borne serrée, pas une marge de confort : crête flottante ×{crete_flottante:.4}"
    );
    assert_eq!(au_rail(&depuis_pcm(&pcm, 24), 24), 0, "plus rien au rail");
}

/// Le comportement ATTENDU par #4073 — et désormais OBTENU. Nom conservé :
/// c'est le témoin que l'issue cite et qui devait passer de `#[ignore]` à vert.
#[test]
fn q1_defaut_connu_la_reserve_automatique_devrait_couvrir_la_reponse_en_temps_d_un_plateau() {
    let p = profil(vec![bande("low_shelf", 80.0, 6.0, FRAC_1_SQRT_2)]);
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
    assert!((crete_ech - 0.9 * FRAC_1_SQRT_2).abs() < 1e-3);
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
fn q2_calcul_scalaire_sans_provenance_pose_le_pic_au_rail_et_laisse_la_crete_vraie_au_dessus_de_0_dbtp()
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

/// #4074 : tags réels → porte de lecture → PCM → mètre FIR indépendant.
/// La réserve estimée couvre ce carré ; elle n'est pas une garantie universelle.
#[test]
fn q2_sample_peak_headroom_protects_the_square_through_playback() {
    use std::sync::Arc;
    use tune_core::audio::replaygain::{MODE_KEY, TRUE_PEAK_CEILING_KEY, playback_factor};
    use tune_core::db::{
        backend::DbBackend, settings_repo::SettingsRepo, sqlite::SqliteDb,
        track_metadata_repo::TrackMetadataRepo,
    };
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    tune_core::db::migrations::run_migrations(&db).unwrap();
    db.execute("INSERT INTO artists (id, name) VALUES (1, 'T9')", &[])
        .unwrap();
    db.execute(
        "INSERT INTO albums (id, title, artist_id) VALUES (1, 'T9', 1)",
        &[],
    )
    .unwrap();
    db.execute("INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, sample_rate, channels) VALUES (42, 'T9', 1, 1, '/t9.flac', 1000, 44100, 1)", &[]).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);
    let settings = SettingsRepo::with_backend(backend.clone());
    settings.set(MODE_KEY, "track").unwrap();
    let meta = TrackMetadataRepo::with_backend(backend.clone());
    let x = carre(997.0, -0.05, N);
    meta.set(42, "rg_track_gain", "+6").unwrap();
    meta.set(42, "rg_track_peak", &crete_echantillon(&x).to_string())
        .unwrap();
    for ceiling in [0.0, -1.0] {
        settings
            .set(TRUE_PEAK_CEILING_KEY, &ceiling.to_string())
            .unwrap();
        let factor = playback_factor(&backend, 42);
        let mut pcm = vers_pcm(&x, 24);
        apply_gain_pcm(&mut pcm, 24, factor);
        let tp = dbtp(&normalise(&depuis_pcm(&pcm, 24), 24));
        eprintln!("sample peak: ceiling {ceiling}, factor {factor:.6}, true peak {tp:+.3} dBTP");
        assert!(
            tp <= ceiling && tp > ceiling - 1.0,
            "crête vraie {tp:+.3} dBTP"
        );
    }
    settings.set(TRUE_PEAK_CEILING_KEY, "0").unwrap();
    meta.set(42, "rg_track_true_peak", &amplitude(dbtp(&x)).to_string())
        .unwrap();
    let mut pcm = vers_pcm(&x, 24);
    apply_gain_pcm(&mut pcm, 24, playback_factor(&backend, 42));
    let tp = dbtp(&normalise(&depuis_pcm(&pcm, 24), 24));
    assert!(
        tp <= 0.005 && tp > -0.1,
        "true peak must replace the estimate: {tp}"
    );
}

/// Référence scalaire sans provenance (#4074), dans l’ordre du bras progressif : ReplayGain (+6 dB, pic
/// d'échantillon tagué) PUIS égaliseur (+6 dB de crête à 3 kHz, réserve
/// −7,13 dB depuis #4073 : la norme L1 d'une cloche de +6 dB vaut 7,13 dB,
/// plus que la somme des gains), sur un sinus 997 Hz à −0,1 dBFS, 16 bits.
/// Mesuré : le ReplayGain pose le sinus au rail (0 dBFS, ≈ 0 dBTP),
/// l'égaliseur le redescend (~−5,9 dBFS) sans over.
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
    let reserve = eq.preamp_db(0).expect("un canal");
    assert!(
        (reserve + 7.1408).abs() < 1e-3,
        "réserve = norme L1 de la cloche (7,131 dB) plus la marge de troncature de \
         #4594 (0,01), pas sa somme de gains (6,0) : {reserve}"
    );
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
    assert!(tp_eq < -3.0 && tp_eq > -8.0, "{tp_eq:+.2} dBTP");
}

// ═════════════════════════ Q3 — flottant → entier ═════════════════════════

/// Corrélation de l'erreur avec le SIGNE du signal, en LSB.
///
/// C'est la mesure qui sépare une distorsion d'un bruit. Une troncature vers
/// zéro donne −1 quand elle déplace chaque échantillon d'un LSB vers zéro, et
/// ≈ −0,5 sur une erreur uniforme ; un dither centré donne ≈ 0. Une moyenne
/// signée nue ne verrait rien : sur un sinus symétrique, les erreurs des
/// alternances positive et négative s'annulent exactement.
fn correlation_au_signe(ideal_raw: &[f64], sortie: &[i64]) -> f64 {
    let mut somme = 0.0;
    let mut vus = 0usize;
    for (&i, &s) in ideal_raw.iter().zip(sortie) {
        if i == 0.0 {
            continue;
        }
        somme += (s as f64 - i) * i.signum();
        vus += 1;
    }
    if vus == 0 { 0.0 } else { somme / vus as f64 }
}

/// `apply_gain_pcm` : dither TPDF ±1 LSB puis arrondi au plus proche (#4076).
///
/// Ce qui était mesuré et qui n'est plus : `clamp` puis `as i16` / `as i32`,
/// donc une troncature VERS ZÉRO, dont l'erreur porte le signe du signal —
/// une distorsion, pas un bruit. Deux conséquences tenues ici :
///
/// * l'erreur n'est plus corrélée au signe du signal ;
/// * un facteur de 1 − 1e-7 (−0,000001 dB, inaudible EN TANT QUE GAIN) ne
///   déplace plus 44 098 échantillons non nuls sur 44 098 d'un LSB vers zéro.
#[test]
fn q3_apply_gain_pcm_dithere_au_lieu_de_tronquer_vers_zero() {
    let x = sinus(997.0, -20.0, N, 0.0);
    for bits in [16u16, 24, 32] {
        let mut pcm = vers_pcm(&x, bits);
        let entree = depuis_pcm(&pcm, bits);
        let facteur = amplitude(-1.0);
        let ideal: Vec<f64> = entree.iter().map(|&v| v as f64 * facteur).collect();
        apply_gain_pcm(&mut pcm, bits, facteur);
        let sortie = depuis_pcm(&pcm, bits);
        let classe = classer_quantification(&ideal, &sortie);
        let correlation = correlation_au_signe(&ideal, &sortie);
        eprintln!(
            "q3 apply_gain_pcm {bits} bits, −1 dB : {classe}, erreur·signe(signal) = {correlation:+.4} LSB"
        );
        assert_eq!(classe, "bruit ajouté avant arrondi (dither)", "{bits} bits");
        assert!(
            correlation.abs() < 0.05,
            "{bits} bits : l'erreur reste corrélée au signe du signal ({correlation:+.4} LSB) — c'est une distorsion, pas un bruit"
        );
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
    let intacts = entree
        .iter()
        .zip(&sortie)
        .filter(|&(&e, &s)| e != 0 && s == e)
        .count();
    eprintln!(
        "q3 apply_gain_pcm ×(1 − 1e-7) : {decales}/{non_nuls} déplacés d'1 LSB vers zéro, {intacts}/{non_nuls} intacts"
    );
    assert!(
        decales < non_nuls,
        "−0,000001 dB déplace ENCORE tout le signal d'1 LSB vers zéro : {decales}/{non_nuls}"
    );
    assert!(
        intacts > non_nuls / 4,
        "un gain inaudible devrait laisser une bonne part du signal intacte : {intacts}/{non_nuls}"
    );
}

/// Un facteur ENTIER ne requantifie rien : il ne dithère donc pas. ×2 sur des
/// entiers est exact, et `apply_gain_pcm` doit le rendre exact — la règle
/// « pas de requantification, pas de dither » de `audio::dither`.
#[test]
fn q3_un_facteur_entier_ne_dithere_pas() {
    let x = sinus(997.0, -20.0, N, 0.0);
    for bits in [16u16, 24, 32] {
        let mut pcm = vers_pcm(&x, bits);
        let entree = depuis_pcm(&pcm, bits);
        apply_gain_pcm(&mut pcm, bits, 2.0);
        let sortie = depuis_pcm(&pcm, bits);
        let rail = (1i64 << (bits - 1)) - 1;
        for (&e, &s) in entree.iter().zip(&sortie) {
            assert_eq!(
                s,
                (e * 2).clamp(-(rail + 1), rail),
                "{bits} bits : ×2 devrait être exact, sans un LSB de bruit"
            );
        }
    }
}

/// `PcmMixer::apply_gain` : même correctif, même implémentation de dither.
#[test]
fn q3_pcm_mixer_apply_gain_dithere_lui_aussi() {
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
        let sortie = depuis_pcm(&pcm, bits);
        let classe = classer_quantification(&ideal, &sortie);
        let correlation = correlation_au_signe(&ideal, &sortie);
        eprintln!(
            "q3 PcmMixer::apply_gain {bits} bits, −1 dB : {classe}, erreur·signe(signal) = {correlation:+.4} LSB"
        );
        assert_eq!(classe, "bruit ajouté avant arrondi (dither)", "{bits} bits");
        assert!(
            correlation.abs() < 0.05,
            "{bits} bits : erreur corrélée au signe du signal ({correlation:+.4} LSB)"
        );
    }
}

/// 🔴 Le dither est DÉTERMINISTE : même entrée, même sortie, octet pour octet.
///
/// Le cache de transcodage nomme ses renditions d'après tout ce qui change les
/// octets encodés, pour qu'une requête identique retrouve le fichier fini ; et
/// la sortie OAAT reprend un flux interrompu par `Range`, donc un deuxième
/// passage doit continuer le premier à l'octet. Un bruit tiré au hasard
/// casserait les deux.
#[test]
fn q3_le_dither_est_deterministe_pour_une_meme_entree() {
    let x = sinus(997.0, -20.0, N, 0.0);
    let facteur = amplitude(-1.0);
    for bits in [16u16, 24, 32] {
        let passe = |quoi: &dyn Fn(&mut Vec<u8>)| {
            let mut pcm = vers_pcm(&x, bits);
            quoi(&mut pcm);
            pcm
        };
        let rg = |p: &mut Vec<u8>| apply_gain_pcm(p, bits, facteur);
        assert_eq!(
            passe(&rg),
            passe(&rg),
            "{bits} bits : ReplayGain non reproductible"
        );
        let mx = |p: &mut Vec<u8>| {
            PcmMixer::apply_gain(p, facteur as f32, bits).expect("profondeur mélangeable");
        };
        assert_eq!(
            passe(&mx),
            passe(&mx),
            "{bits} bits : mélangeur non reproductible"
        );
    }
    let pcm24 = vers_pcm(&x, 24);
    assert_eq!(
        convert_pcm_bytes(&pcm24, 24, 16),
        convert_pcm_bytes(&pcm24, 24, 16),
        "réduction 24→16 non reproductible : cache de transcodage cassé"
    );
}

/// `EqProcessor::process_pcm` : requantification avec dither TPDF ±1 LSB puis
/// arrondi, à TOUTE profondeur (16, 24 et 32 bits). Référence : le même
/// signal passé à 32 bits, où ±1 LSB vaut −186 dBFS.
#[test]
fn q3_l_egaliseur_entier_requantifie_avec_un_dither_tpdf_a_toute_profondeur() {
    let p = profil(vec![bande("high_shelf", 20_000.0, -1.0, FRAC_1_SQRT_2)]);
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
        vals.contains(&1) && vals.contains(&-1),
        "le silence 32 bits ressort dithéré à ±1 LSB"
    );
}

/// `decode::convert_pcm_bytes` (24 → 16 bits, chemin du transcodage DLNA/WAV
/// 16 bits depuis une source 24 bits, et de la mémoire de préchargement) :
/// dither TPDF puis arrondi, là où c'était un décalage arithmétique — donc une
/// troncature vers −∞ (#4075).
///
/// 🔖 **C'est le témoin pré-écrit de T9**, `q3_defaut_connu_la_reduction_24_vers_16_bits_devrait_dither`,
/// dont #4075 lève le `#[ignore = "défaut connu …"]`. Son assertion d'origine
/// — `classer_quantification(...) == "bruit ajouté avant arrondi (dither)"` —
/// est reprise mot pour mot ci-dessous ; seul le biais moyen a été ajouté,
/// parce qu'un dither doit être CENTRÉ, là où le décalage biaisait d'un
/// demi-LSB vers −∞. Le témoin qui affirmait le défaut,
/// `q3_convert_pcm_bytes_reduit_24_vers_16_bits_par_decalage_sans_dither`, est
/// remplacé par celui-ci : il affirmait un comportement qui n'existe plus.
#[test]
fn q3_convert_pcm_bytes_reduit_24_vers_16_bits_avec_un_dither() {
    let x = sinus(997.0, -20.0, N, 0.0);
    let pcm24 = vers_pcm(&x, 24);
    let ideal: Vec<f64> = depuis_pcm(&pcm24, 24)
        .iter()
        .map(|&v| v as f64 / 256.0)
        .collect();
    let sortie = depuis_pcm(&convert_pcm_bytes(&pcm24, 24, 16), 16);
    let classe = classer_quantification(&ideal, &sortie);
    let biais = ideal
        .iter()
        .zip(&sortie)
        .map(|(i, &s)| s as f64 - i)
        .sum::<f64>()
        / ideal.len() as f64;
    eprintln!("q3 convert_pcm_bytes 24→16 : {classe}, biais moyen {biais:+.4} LSB");
    assert_eq!(classe, "bruit ajouté avant arrondi (dither)");
    assert!(
        biais.abs() < 0.05,
        "le décalage biaisait vers −∞ d'un demi-LSB ; le dither doit être centré : {biais:+.4} LSB"
    );
}

/// La conséquence audible du décalage : un signal continu SOUS le LSB de la
/// cible disparaissait entièrement. 255/256 de LSB tronquait à 0 — silence.
/// Dithéré, il survit : sa moyenne vaut ce qu'elle doit valoir.
#[test]
fn q3_un_signal_sous_le_lsb_ne_disparait_plus_a_la_reduction() {
    let mut entree = Vec::new();
    for _ in 0..N {
        ecrire(&mut entree, 255, 24);
    }
    let sortie = depuis_pcm(&convert_pcm_bytes(&entree, 24, 16), 16);
    let moyenne = sortie.iter().map(|&v| v as f64).sum::<f64>() / sortie.len() as f64;
    eprintln!("q3 réduction 24→16 d'un continu à 255/256 LSB : moyenne {moyenne:.4} LSB");
    assert!(
        (moyenne - 255.0 / 256.0).abs() < 0.05,
        "un continu à 255/256 LSB doit ressortir à {:.4} LSB en moyenne, pas {moyenne:.4}",
        255.0 / 256.0
    );
    assert!(
        sortie.iter().any(|&v| v != 0),
        "le signal a disparu : c'est le défaut #4075, pas son correctif"
    );
}

/// Élargir une profondeur ne dithère PAS : c'est un décalage exact, sans
/// perte. La règle « pas de requantification, pas de dither ».
#[test]
fn q3_elargir_une_profondeur_reste_exact_sans_dither() {
    let x = sinus(997.0, -20.0, N, 0.0);
    let pcm16 = vers_pcm(&x, 16);
    for cible in [24u16, 32] {
        let sortie = depuis_pcm(&convert_pcm_bytes(&pcm16, 16, cible), cible);
        let attendu: Vec<i64> = depuis_pcm(&pcm16, 16)
            .iter()
            .map(|&v| v << (cible - 16))
            .collect();
        assert_eq!(sortie, attendu, "16 → {cible} bits doit rester exact");
    }
}

/// `Convolver::process_pcm` : arrondi au plus proche, sans dither — et le
/// **gain parasite** d'une réponse impulsionnelle unité a disparu (#4076,
/// constat voisin).
///
/// Ce qui était mesuré : le décodage divisait par 2^(n−1) et le réencodage
/// multipliait par 2^(n−1) **− 1**. Cette asymétrie est un gain de
/// −0,00027 dB appliqué à chaque passage — 1 LSB perdu par **29 214
/// échantillons sur 44 100 (66 %)** à 16 bits. Un convolveur chargé d'une
/// impulsion unité, donc censé ne rien faire, abîmait le signal.
///
/// Le témoin sépare les deux choses qu'il ne faut pas confondre :
///
/// * **à 16 bits, l'identité est exacte** — c'est la garde forte du
///   correctif : un seul échantillon décalé la fait rougir ;
/// * **au-dessus, l'identité est hors de portée**, et c'est mesuré, pas
///   supposé : le convolveur tient son tampon en `f32`, dont la mantisse de
///   24 bits ne peut pas porter un entier de 24 bits déjà convolué, encore
///   moins de 32. À 24 bits, ce qui est tenu est l'**absence de gain
///   parasite** — l'asymétrie donnait −0,637 LSB sur ce sinus, le plancher du
///   f32 en donne −0,11. À **32 bits, cette garde est impossible** : le
///   plancher du f32 y vaut −62,9 LSB, cent fois ce qu'on chercherait. Le
///   témoin le dit au lieu de faire semblant.
#[test]
fn q3_le_convolveur_arrondit_sans_dither_et_une_ir_unite_ne_perd_plus_de_gain() {
    let x = sinus(997.0, -0.1, N, 0.0);

    // 16 bits : identité EXACTE, octet pour octet.
    let mut pcm = vers_pcm(&x, 16);
    let entree = depuis_pcm(&pcm, 16);
    Convolver::new(&[vec![1.0f32]], 256).process_pcm(&mut pcm, 16);
    let sortie = depuis_pcm(&pcm, 16);
    let perdus = entree.iter().zip(&sortie).filter(|(e, s)| e != s).count();
    eprintln!("q3 Convolver IR unité 16 bits : {perdus}/{N} échantillons ≠ entrée");
    assert_eq!(
        perdus, 0,
        "une IR unité doit être l'identité à 16 bits : {perdus} échantillons décalés (gain parasite 32767/32768 ?)"
    );

    // 24 bits : l'identité n'est plus atteignable (le f32 du convolveur s'y
    // épuise), mais le GAIN PARASITE doit avoir disparu. L'asymétrie donnait
    // −0,637 LSB sur ce sinus ; le plancher numérique du f32, lui, est à
    // −0,11 LSB. Le seuil sépare les deux avec de la marge.
    //
    // 🔍 À **32 bits**, cette garde est IMPOSSIBLE et il vaut mieux l'écrire
    // que la simuler : le plancher du f32 y vaut −62,9 LSB, cent fois
    // l'asymétrie (−0,637 LSB) qu'on voudrait détecter. Aucun seuil ne peut
    // les distinguer. C'est le témoin 16 bits — identité EXACTE — qui garde
    // le correctif ; 32 bits n'y mesure que le plafond du tampon f32.
    for (bits, garde_le_gain, plafond) in [(24u16, true, 64i64), (32u16, false, 4096i64)] {
        let mut pcm = vers_pcm(&x, bits);
        let entree = depuis_pcm(&pcm, bits);
        Convolver::new(&[vec![1.0f32]], 256).process_pcm(&mut pcm, bits);
        let sortie = depuis_pcm(&pcm, bits);
        let ideal: Vec<f64> = entree.iter().map(|&v| v as f64).collect();
        let correlation = correlation_au_signe(&ideal, &sortie);
        let ecart = entree
            .iter()
            .zip(&sortie)
            .map(|(&e, &s)| (s - e).abs())
            .max()
            .unwrap_or(0);
        eprintln!(
            "q3 Convolver IR unité {bits} bits : erreur·signe(signal) = {correlation:+.4} LSB, écart max {ecart} LSB (plancher du tampon f32){}",
            if garde_le_gain {
                ""
            } else {
                " — gain non témoignable à cette profondeur"
            }
        );
        if garde_le_gain {
            assert!(
                correlation.abs() < 0.15,
                "{bits} bits : gain parasite de retour, erreur·signe(signal) = {correlation:+.4} LSB (l'asymétrie 2^(n−1)−1 en donnait −0,637)"
            );
        }
        assert!(
            ecart <= plafond,
            "{bits} bits : écart max {ecart} LSB au-delà du plancher numérique du f32 ({plafond})"
        );
    }

    // L'arrondi, lui, reste : pas de dither dans le convolveur.
    let x = sinus(997.0, -20.0, N, 0.0);
    let mut pcm = vers_pcm(&x, 16);
    let entree = depuis_pcm(&pcm, 16);
    Convolver::new(&[vec![0.5f32]], 256).process_pcm(&mut pcm, 16);
    let ideal: Vec<f64> = entree.iter().map(|&v| v as f64 * 0.5).collect();
    let classe = classer_quantification(&ideal, &depuis_pcm(&pcm, 16));
    eprintln!("q3 Convolver IR 0,5 16 bits : {classe}");
    assert_eq!(classe, "arrondi au plus proche");
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

// ═══════════ #4594 — la réserve et la cascade doivent voir les MÊMES bandes ═══════════

/// La grille ISO à 10 bandes, celle des préréglages livrés
/// (`tune_core::audio::eq_presets::GRILLE_10`, recopiée pour que ce fichier
/// reste lisible seul).
const GRILLE_10: [f64; 10] = [
    31.0, 63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];

fn dix_cloches(band_type: &str, gain: f64) -> EqProfile {
    profil(
        GRILLE_10
            .iter()
            .map(|f| bande(band_type, *f, gain, 1.0))
            .collect(),
    )
}

/// #4594 — un `type` de bande NON CANONIQUE pousse, et ne réservait RIEN.
///
/// `EqBandSpec::coeffs` range tout type inconnu en `peaking_eq` (son bras
/// `_ =>`), et `POST /zones/{id}/eq` ne valide pas ce champ : `band_type` est
/// un `String` nu, désérialisé par `filter_map(.. .ok())`. `"peaking"`,
/// `"Peak"` ou `"bell"` traversent donc l'API, sont montés en cloches qui
/// POUSSENT — et la somme des gains positifs les filtrait par LISTE BLANCHE
/// (`"peak" | "low_shelf" | "high_shelf"`) là où la cascade les filtrait par
/// LISTE NOIRE. Somme à zéro, terme L1 éteint avec elle (il n'entre que si au
/// moins une bande pousse) : réserve 0 dB sur dix cloches à +6 dB.
///
/// **Mesuré avant le correctif**, sur `origin/main` 34227d75, sinus 1 kHz à
/// −6 dBFS, 2 s, stéréo, à travers le chemin flottant qui ne sature pas :
/// crête en sortie **1,731 (+4,77 dBFS)** et **107 192 overs sur 176 400 —
/// 60,8 % des échantillons écrêtés dur**. Les mêmes bandes en `"peak"` :
/// réserve −60,00 dB, crête 0,001731, **zéro** over.
///
/// Après : une seule porte, `EqBandSpec::est_une_bande_a_gain`, pour la somme
/// comme pour la cascade.
#[test]
fn un_type_de_bande_non_canonique_reserve_comme_une_cloche_4594() {
    // Le type non canonique franchit bien la porte d'entrée : c'est ce qui
    // rend le défaut atteignable, pas seulement pensable.
    let depuis_l_api: EqBandSpec =
        serde_json::from_str(r#"{"freq":1000.0,"gain":6.0,"q":1.0,"type":"bell"}"#)
            .expect("l'API accepte un type de bande quelconque");
    assert_eq!(
        depuis_l_api.band_type, "bell",
        "le type est conservé tel quel, sans validation"
    );

    let canonique = dix_cloches("peak", 6.0).automatic_headroom_db(0);
    assert!(
        canonique <= -10.0,
        "dix cloches canoniques à +6 dB réservent la borne vraie de leur \
         cascade, −16,54 dB : {canonique}"
    );

    for etiquette in ["peaking", "PEAK", "bell", "Peak", "cloche"] {
        let reserve = dix_cloches(etiquette, 6.0).automatic_headroom_db(0);
        assert_eq!(
            reserve, canonique,
            "type «{etiquette}» : monté en cloche par coeffs(), il doit réserver \
             comme une cloche — mesuré {reserve} dB au lieu de {canonique} dB"
        );

        // L'effet audible, et pas le raisonnement : plus un seul échantillon
        // hors du rail sur le signal qui en écrêtait 60,8 %.
        let mut eq = EqProcessor::new(&dix_cloches(etiquette, 6.0), FS, 2);
        let mut buf = vec![0.0f32; N * 2];
        for i in 0..N {
            let v = (amplitude(-6.0) * (2.0 * PI * 1000.0 * i as f64 / FS as f64).sin()) as f32;
            buf[i * 2] = v;
            buf[i * 2 + 1] = v;
        }
        let stats = eq.process_interleaved(&mut buf);
        assert_eq!(
            stats.overs, 0,
            "type «{etiquette}» : sinus 1 kHz à −6 dBFS, aucun échantillon ne doit \
             sortir du rail après l'étage d'égalisation"
        );
        let crete = buf[N..].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(
            crete < 1.0,
            "type «{etiquette}» : crête en sortie {crete} ({:.2} dBFS)",
            dbfs(crete as f64)
        );
    }

    // ── Contre-épreuves : la réserve ne s'est pas mise à apparaître partout ──

    // Un profil de cloches non canoniques qui ne fait que CREUSER ne réserve
    // rigoureusement rien — sinon ce correctif atténuerait un profil purement
    // soustractif, ce que personne ne demande.
    for etiquette in ["peak", "peaking", "bell"] {
        let reserve = dix_cloches(etiquette, -6.0).automatic_headroom_db(0);
        assert!(
            reserve.abs() < 1e-9,
            "type «{etiquette}» à −6 dB : aucune marge à réserver, mesuré {reserve}"
        );
    }

    // Les `pass` et le `notch` restent HORS de la somme : leur champ `gain`
    // n'est pas lu par `coeffs`, le réserver serait réserver pour rien. Un
    // `low_pass` ne réserve que sa RÉSONANCE, inchangée.
    for etiquette in ["low_pass", "high_pass", "notch"] {
        let plat = profil(vec![bande(etiquette, 3000.0, 12.0, FRAC_1_SQRT_2)]);
        assert!(
            plat.automatic_headroom_db(0).abs() < 1e-9,
            "type «{etiquette}» à Butterworth : son champ gain ne doit rien réserver"
        );
    }
    let resonant = profil(vec![bande("low_pass", 3000.0, 0.0, 4.0)]);
    assert!(
        (resonant.automatic_headroom_db(0) - (-15.051_499_783_199_058)).abs() < 1e-9,
        "le témoin voisin du passe-bas Q=4 est inchangé : {}",
        resonant.automatic_headroom_db(0)
    );
}

// ══════ #4594 — la réserve est la BORNE VRAIE, et le niveau revient ══════

/// Le pire signal possible pour cette cascade : `x[n] = signe(h[−n])`.
///
/// C'est celui qui ATTEINT `‖h‖₁`, la borne dont `automatic_headroom_db_at`
/// fait sa réserve. Aucune musique ne ressemble à ça — c'est le but : si la
/// réserve tient ici, elle tient partout. `h` est pris sur le processeur
/// EFFECTIVEMENT monté, réserve comprise, donc la sortie attendue vaut 1,0 au
/// facteur de `MARGE_DE_TRONCATURE_DB` près.
fn signal_adverse(p: &EqProfile, sr: u32, n: usize) -> Vec<f32> {
    let mut eq = EqProcessor::new(p, sr, 1);
    let mut h = vec![0.0f32; n];
    h[0] = 1.0;
    eq.process_interleaved(&mut h);
    (0..n)
        .map(|i| if h[n - 1 - i] >= 0.0 { 1.0f32 } else { -1.0f32 })
        .collect()
}

fn carre_pleine_echelle(n: usize, sr: u32, freq: f64) -> Vec<f32> {
    (0..n)
        .map(|i| {
            if (2.0 * PI * freq * i as f64 / sr as f64).sin() >= 0.0 {
                1.0f32
            } else {
                -1.0f32
            }
        })
        .collect()
}

/// Bruit blanc pleine échelle, xorshift — déterministe, sans dépendance.
fn bruit_pleine_echelle(n: usize) -> Vec<f32> {
    let mut x = 0x2545_f491_4f6c_dd1d_u64;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            ((x >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0) as f32
        })
        .collect()
}

/// Crête et nombre d'overs après l'étage d'égalisation, chemin flottant.
fn crete_et_overs(p: &EqProfile, sr: u32, signal: &[f32]) -> (f64, u64) {
    let mut eq = EqProcessor::new(p, sr, 1);
    let mut buf = signal.to_vec();
    let stats = eq.process_interleaved(&mut buf);
    let crete = buf.iter().fold(0.0f64, |m, s| m.max((*s as f64).abs()));
    (crete, stats.overs)
}

/// #4594 — la réserve vaut la borne vraie, et les valeurs sont figées ici.
///
/// Arbitrage de Bertrand du 20/09 pour la v0.9.160 : la réserve n'est plus le
/// `max()` de la somme des gains positifs et de la norme L1, mais **la norme
/// L1 seule** — la seule borne qu'un signal borné ne peut pas dépasser. La
/// somme majorait le maximum fréquentiel comme si toutes les bandes
/// poussaient au même endroit ; sur un égaliseur graphique, où elles sont
/// disjointes par construction, elle atténuait pour rien.
///
/// **Ce que l'auditeur entend** : le son REMONTE — de 6,6 dB sur `bass_boost`,
/// 14,3 dB sur `rock`, 43,5 dB sur un profil expert de dix bandes à +6 dB. Qui
/// avait compensé au volume devra le rebaisser d'autant.
///
/// ```text
/// profil (44 100 Hz)      avant #4594   après #4594   rendu
/// flat                        0,000 dB      0,000 dB   —
/// bass_boost                −20,000 dB    −13,408 dB   +6,6 dB
/// treble_boost              −24,000 dB    −12,334 dB  +11,7 dB
/// loudness                  −27,000 dB    −13,741 dB  +13,3 dB
/// rock                      −28,000 dB    −13,653 dB  +14,3 dB
/// jazz                      −18,000 dB     −9,630 dB   +8,4 dB
/// classical                   0,000 dB      0,000 dB   —
/// dix cloches à +6 dB       −60,000 dB    −16,542 dB  +43,5 dB
/// curseurs +6/+3             −9,000 dB     −8,272 dB   +0,7 dB
/// curseurs +12/+12/+12      −36,000 dB    −19,970 dB  +16,0 dB
/// ```
#[test]
fn la_reserve_de_chaque_prereglage_livre_est_la_borne_vraie_4594() {
    let attendu = [
        ("flat", 0.0),
        ("bass_boost", -13.407_932_007_318_667),
        ("treble_boost", -12.334_241_217_122_317),
        ("loudness", -13.741_067_957_532_147),
        ("rock", -13.653_372_806_391_918),
        ("jazz", -9.629_790_433_680_865),
        ("classical", 0.0),
    ];
    for (nom, reserve_attendue) in attendu {
        let bandes = tune_core::audio::eq_presets::bandes(nom)
            .unwrap_or_else(|| panic!("préréglage livré «{nom}»"));
        let mesuree = profil(bandes).automatic_headroom_db(0);
        assert!(
            (mesuree - reserve_attendue).abs() < 1e-6,
            "préréglage «{nom}» : réserve {mesuree} dB, attendu {reserve_attendue} dB — \
             le niveau perçu de tout le parc vient de bouger"
        );
    }

    // Les trois curseurs du profileur, sans aucune bande expert.
    let curseurs = EqProfile {
        enabled: true,
        bass_gain_db: 6.0,
        treble_gain_db: 3.0,
        ..Default::default()
    };
    assert!(
        (curseurs.automatic_headroom_db(0) - (-8.271_562_619_384_957)).abs() < 1e-6,
        "curseurs graves +6 / aigus +3 : {}",
        curseurs.automatic_headroom_db(0)
    );

    // Dix bandes expert à +6 dB : 60 dB réservés hier, 16,5 aujourd'hui.
    assert!(
        (dix_cloches("peak", 6.0).automatic_headroom_db(0) - (-16.541_839_731_104_73)).abs() < 1e-6,
        "dix cloches à +6 dB : {}",
        dix_cloches("peak", 6.0).automatic_headroom_db(0)
    );

    // La résonance d'un `pass` n'est pas une norme L1 : ce terme-là n'a pas
    // bougé, et le témoin voisin du passe-bas Q=4 le dit.
    let resonant = profil(vec![bande("low_pass", 3000.0, 0.0, 4.0)]);
    assert!(
        (resonant.automatic_headroom_db(0) - (-15.051_499_783_199_058)).abs() < 1e-9,
        "passe-bas Q=4 : {}",
        resonant.automatic_headroom_db(0)
    );
}

/// #4594 — la contre-épreuve qui compte : **rien n'écrête**.
///
/// Rendre 6 à 43 dB de niveau ne vaut que si pas un échantillon ne sort du
/// rail. Ce témoin le MESURE, sur les sept préréglages livrés, les trois
/// curseurs et le profil expert de dix bandes à +6 dB, à 44,1 / 96 / 192 kHz,
/// et avec quatre signaux pleine échelle — dont celui qui atteint la borne.
///
/// Mesuré : crête **0,998849** et **zéro** over partout. Le signal adverse
/// sort exactement au facteur de `MARGE_DE_TRONCATURE_DB` sous le rail, ce qui
/// est la démonstration que la réserve vaut bien la borne — ni plus, ni moins.
///
/// `classical` et `flat` sont HORS de cette garde, et c'est délibéré : ils ne
/// poussent nulle part, donc ne réservent rien, donc peuvent dépasser sur un
/// signal adverse (mesuré : 16 864 overs sur un carré pleine échelle pour
/// `classical`). C'était vrai à l'identique avant #4594 — même réserve de
/// 0,000 dB, mêmes 16 864 overs — et fermer cette porte coûterait jusqu'à
/// 13,3 dB à un profil qui ne fait QUE creuser. Voir
/// `EqProfile::automatic_headroom_db_at`.
#[test]
fn aucun_prereglage_livre_n_ecrete_meme_sur_le_signal_adverse_4594() {
    let mut cas: Vec<(String, EqProfile)> = Vec::new();
    for nom in ["bass_boost", "treble_boost", "loudness", "rock", "jazz"] {
        cas.push((
            nom.to_string(),
            profil(tune_core::audio::eq_presets::bandes(nom).unwrap()),
        ));
    }
    cas.push(("dix cloches à +6 dB".into(), dix_cloches("peak", 6.0)));
    cas.push((
        "dix cloches «bell» à +6 dB".into(),
        dix_cloches("bell", 6.0),
    ));
    cas.push((
        "curseurs +6/+3".into(),
        EqProfile {
            enabled: true,
            bass_gain_db: 6.0,
            treble_gain_db: 3.0,
            ..Default::default()
        },
    ));
    cas.push((
        "curseurs +12/+12/+12".into(),
        EqProfile {
            enabled: true,
            bass_gain_db: 12.0,
            mid_gain_db: 12.0,
            treble_gain_db: 12.0,
            ..Default::default()
        },
    ));

    let n = 1 << 15;
    for sr in [44_100u32, 96_000, 192_000] {
        for (nom, p) in &cas {
            let mut signaux: Vec<(&str, Vec<f32>)> = vec![
                ("adverse", signal_adverse(p, sr, n)),
                ("carré 100 Hz", carre_pleine_echelle(n, sr, 100.0)),
                ("bruit blanc", bruit_pleine_echelle(n)),
            ];
            for freq in GRILLE_10 {
                signaux.push((
                    "sinus",
                    (0..n)
                        .map(|i| (2.0 * PI * freq * i as f64 / sr as f64).sin() as f32)
                        .collect(),
                ));
            }
            for (quoi, signal) in signaux {
                let (crete, overs) = crete_et_overs(p, sr, &signal);
                assert_eq!(
                    overs, 0,
                    "{nom} à {sr} Hz, {quoi} : {overs} échantillons hors du rail \
                     (crête {crete})"
                );
                assert!(
                    crete < 1.0,
                    "{nom} à {sr} Hz, {quoi} : crête {crete} au rail"
                );
            }
        }
    }
}

/// #4594 — un profil qui ne fait QUE creuser ne réserve toujours rien.
///
/// La porte « au moins une bande pousse » est la dernière divergence de
/// `automatic_headroom_db_at`, et elle est délibérée. Ce témoin fige les deux
/// faces : rien n'est réservé, ET ce que sa fermeture coûterait est mesuré, de
/// sorte que personne ne la ferme sans voir le prix.
#[test]
fn un_profil_qui_ne_fait_que_creuser_ne_reserve_rien_4594() {
    for gains in [-3.0, -6.0, -24.0] {
        let creux = dix_cloches("peak", gains);
        assert_eq!(
            creux.automatic_headroom_db(0),
            -0.0,
            "dix cloches à {gains} dB ne poussent nulle part"
        );
    }
    assert_eq!(
        profil(tune_core::audio::eq_presets::bandes("classical").unwrap()).automatic_headroom_db(0),
        -0.0,
        "«classical» ne fait que creuser"
    );

    // Le prix de la fermeture, mesuré : la norme L1 d'une cascade purement
    // soustractive dépasse l'unité — elle sonne — et la réserver retirerait
    // jusqu'à 13,3 dB à qui ne demandait qu'à creuser. C'est le défaut qu'on
    // vient de corriger, par l'autre bout.
    let creux_profonds = dix_cloches("peak", -24.0);
    let mut eq = EqProcessor::new(&creux_profonds, FS, 1);
    assert_eq!(eq.preamp_db(0), Some(-0.0), "aucun préampli sur ce chemin");
    let mut h = vec![0.0f32; 1 << 18];
    h[0] = 1.0;
    eq.process_interleaved(&mut h);
    let l1_db = 20.0 * h.iter().map(|v| (*v as f64).abs()).sum::<f64>().log10();
    assert!(
        l1_db > 5.0,
        "une cascade purement soustractive sonne : L1 = {l1_db} dB"
    );
}
