//! #2218 (suite de T9) — l'écrêtage est COMPTÉ et DIT, sans changer un
//! échantillon.
//!
//! T9 (`marge_et_crete_2218.rs`, `docs/mesures/2218-marge-ecretage-crete-vraie.md`)
//! a mesuré : (A) `apply_gain_pcm` à +6 dB sans pic tagué écrête dur 66 % d'un
//! sinus à −0,1 dBFS, excès 31 866 LSB, sans compteur ni journal ; (B)
//! l'égaliseur compte 40,4 % d'overs sur un plateau grave et ne les dit
//! jamais. Ce fichier prouve que c'est compté et dit, et fige les octets de
//! sortie par des empreintes FNV-1a.
//!
//! # 🔴 Les empreintes ReplayGain et mélangeur ont CHANGÉ le 13/09 (#4076)
//!
//! Elles avaient été relevées sur `batch/bugs-12` à 49ecf1fe pour prouver que
//! le COMPTAGE ne déplaçait pas un échantillon. Il ne l'a jamais fait, et ce
//! n'est pas lui qui les change : #4076 remplace la **troncature vers zéro**
//! de `apply_gain_pcm` et `PcmMixer::apply_gain` par un dither TPDF suivi d'un
//! arrondi. Les octets devaient donc bouger — c'est l'objet du correctif, pas
//! un effet de bord. Ce qui n'a PAS bougé, et que ce fichier continue de
//! tenir ligne à ligne : tous les compteurs d'écrêtage (29 174 écrêtés,
//! 31 866 LSB d'excès, 36 896 overs), parce qu'ils comparent la valeur
//! **idéale** au rail, en amont du bruit et de l'arrondi.
//!
//! Les empreintes de l'**égaliseur** (`B_EQ_*`) sont, elles, inchangées : son
//! dither était déjà là, et #4075/#4076 n'ont fait que le déplacer dans
//! `audio::dither` sans toucher un seul nombre. C'est la meilleure preuve que
//! la mutualisation est neutre.
//!
//! Les 18 témoins de T9 et ses 4 ignorés restent à côté.
//!
//! Le journal est capturé par un abonné `tracing_subscriber::fmt` GLOBAL,
//! posé une fois pour le processus, qui écrit dans un tampon PROPRE AU FIL :
//! deux lignes `dsp_ecretage` par piste et par étage, jamais une par bloc.
//! Voir [`capturer`] pour la raison — un abonné de fil (`with_default`) ne
//! capte rien ici.

use std::cell::RefCell;
use std::f64::consts::{FRAC_1_SQRT_2, PI};
use std::sync::OnceLock;

use tune_core::audio::ecretage::{CompteurDEcretage, releve};
use tune_core::audio::eq::{EqBandSpec, EqProcessor, EqProfile};
use tune_core::audio::mixer::PcmMixer;
use tune_core::audio::replaygain::{
    GainReplay, ReplayGainMode, ReplayGainSettings, TrackGain, apply_gain_pcm,
    apply_gain_pcm_compte, gain_factor,
};

const FS: u32 = 44_100;
const N: usize = 44_100;

// ───────── empreintes FNV-1a ─────────
// `A_*`, `E_*`, `Q2_*` (ReplayGain, mélangeur, chaîne) : relevées le 13/09
// APRÈS #4075/#4076, sur `batch/bugs-13` — le dither a remplacé la troncature.
// `B_EQ_*` : relevées à 49ecf1fe, AVANT le comptage, et INCHANGÉES depuis.

const A_RG_PLUS6_16B: u64 = 0x70ed_00f1_327a_171f;
const A_RG_PLUS6_24B: u64 = 0x28b2_9fa3_c47f_2f8e;
const A_RG_PLUS6_32B: u64 = 0x45d6_6c51_9c9b_bfdd;
const E_RG_MOINS1_16B: u64 = 0x7ccf_e03f_36b3_4143;
const E_RG_MOINS1_24B: u64 = 0x1412_74cb_e444_e276;
const E_RG_MOINS1_32B: u64 = 0x4ae4_154e_e968_b69f;
/// 🔴 Relevées à NEUF par #4073. Le cas B n'est plus le passe-bas Q = 4 —
/// `automatic_headroom_db` réserve désormais sa résonance et il ne dépasse
/// plus — mais le plateau grave 80 Hz +6 dB sur un carré 50 Hz à −0,05 dBFS,
/// dont la réponse en TEMPS dépasse encore la réserve (défaut B partie 2,
/// toujours ouvert). Mesuré sur Shrek : 17 825 écrêtés sur 44 100 (40,4 %),
/// excès maximal 450 490 LSB, crête +0,45 dBFS, premier à l'échantillon 267.
///
/// Les anciennes valeurs, pour mémoire : empreinte `0xc2f3_7c72_784a_7078`,
/// 36 896 overs (83,7 %). Elles n'étaient plus atteignables, la réserve les
/// ayant supprimées — les garder aurait figé un comportement corrigé.
const B_EQ_LOWPASS_Q4_24B: u64 = 0x285b_3693_c85d_4121;
const B_EQ_LOWPASS_Q4_OVERS: u64 = 17_825;
const B_EQ_LOWSHELF_CARRE_24B: u64 = 0x285b_3693_c85d_4121;
const B_EQ_LOWSHELF_CARRE_OVERS: u64 = 17_825;
/// 🔴 Relevée à neuf par #4073 : la réserve s'applique aussi au chemin
/// flottant, donc les octets changent. Ancienne : `0xa879_54df_086e_8265`.
const B_EQ_FLOTTANT_Q4: u64 = 0x2f6e_f7be_4ae3_75bb;
const Q2_RG_PIC_16B: u64 = 0xfae0_2d13_6dbf_0670;
const Q2_CHAINE_RG_EQ_16B: u64 = 0x25db_2059_e212_ef86;
const E_MIXEUR_MOINS1_16B: u64 = 0x4c00_4b27_9e0f_b429;
const E_MIXEUR_MOINS1_24B: u64 = 0x53bc_d36e_bb4c_14c0;
const E_MIXEUR_MOINS1_32B: u64 = 0x204e_851d_0428_4df5;
const A_MIXEUR_X2_16B: u64 = 0x64a4_cd35_d280_d173;
/// Le même signal, le même facteur, mais découpé en 100 blocs de 441 trames.
///
/// Ce n'est PLUS la même empreinte que `A_RG_PLUS6_16B` (un seul tenant), et
/// c'est une conséquence assumée de #4076 : un dither tire sa graine du
/// CONTENU du bloc qu'on lui donne, donc découper le flux autrement donne un
/// autre bruit. Un dither sans mémoire ni position ne peut pas faire
/// autrement — et la propriété dont la production a besoin n'est pas
/// « 100 blocs = un seul tenant » mais « le même découpage rend toujours les
/// mêmes octets », que ce témoin tient aussi, puisque cette constante est
/// stable d'une exécution à l'autre.
const A_RG_PLUS6_16B_EN_100_BLOCS: u64 = 0x95b2_a010_14b8_3ba3;

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ───────────── signaux et PCM, les mêmes que T9 ─────────────

fn amplitude(dbfs: f64) -> f64 {
    10f64.powf(dbfs / 20.0)
}

fn sinus(freq: f64, dbfs_crete: f64, n: usize, phase: f64) -> Vec<f64> {
    let a = amplitude(dbfs_crete);
    (0..n)
        .map(|i| a * (2.0 * PI * freq * i as f64 / FS as f64 + phase).sin())
        .collect()
}

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

fn pleine_echelle(bits: u16) -> f64 {
    (1i64 << (bits - 1)) as f64
}

fn rail(bits: u16) -> i64 {
    (1i64 << (bits - 1)) - 1
}

fn vers_pcm(x: &[f64], bits: u16) -> Vec<u8> {
    let fe = pleine_echelle(bits);
    let mut out = Vec::with_capacity(x.len() * (bits as usize / 8));
    for &v in x {
        let raw = (v * fe).round().clamp(-fe, fe - 1.0) as i64;
        match bits {
            16 => out.extend_from_slice(&(raw as i16).to_le_bytes()),
            24 => out.extend_from_slice(&(raw as i32).to_le_bytes()[..3]),
            32 => out.extend_from_slice(&(raw as i32).to_le_bytes()),
            _ => unreachable!("profondeur {bits}"),
        }
    }
    out
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

/// Le compte de T9, contre l'idéal (arrondi au-delà du rail).
fn ecretes_t9(ideal_raw: &[f64], bits: u16) -> (usize, f64) {
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

fn reglages(prevent_clipping: bool, plafond_dbtp: f64) -> ReplayGainSettings {
    ReplayGainSettings {
        mode: ReplayGainMode::Track,
        preamp_db: 0.0,
        prevent_clipping,
        true_peak_ceiling_db: plafond_dbtp,
    }
}

/// Le facteur du cas A de T9, ×1,9953 : +6 dB, aucun pic tagué.
///
/// 🔴 **Garde-fou DÉSARMÉ depuis #4072.** Ce banc mesure ce que compte
/// `apply_gain_pcm` quand elle écrête ; il lui faut donc un facteur qui
/// écrête. Avec `prevent_clipping` armé, `gain_factor` refuse désormais le
/// gain positif sans pic (le défaut A est corrigé) et rendrait 1,0 : plus un
/// seul écrêté à compter, et les quinze empreintes FNV-1a de ce fichier —
/// relevées pour prouver qu'aucun échantillon n'avait bougé — deviendraient
/// celles du signal d'entrée.
///
/// Le facteur rendu ici est **exactement** celui d'avant, au bit près : le
/// chemin désarmé de `gain_factor` n'a pas été touché par #4072. Les
/// empreintes restent donc valides et continuent de garder ce qu'elles
/// gardaient.
fn facteur_plus_6_sans_pic() -> f64 {
    gain_factor(
        TrackGain {
            gain_db: 6.0,
            peak: None,
        },
        reglages(false, 0.0),
    )
}

fn facteur_moins_1() -> f64 {
    gain_factor(
        TrackGain {
            gain_db: -1.0,
            peak: None,
        },
        reglages(true, 0.0),
    )
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

/// Le passe-bas Q = 4 du cas B de T9 — qui, depuis #4073, N'ÉCRÊTE PLUS.
///
/// `automatic_headroom_db` réserve désormais sa résonance (+12,11 dB). Le
/// profil reste ici parce que c'est exactement ce qu'il faut garder : la
/// preuve que la réserve fait son travail.
fn passe_bas_q4() -> EqProcessor {
    EqProcessor::new(&profil(vec![bande("low_pass", 997.0, 0.0, 4.0)]), FS, 1)
}

/// Le profil qui DÉPASSE ENCORE, pour les témoins qui ont besoin d'écrêtage à
/// compter (#4073).
///
/// La réserve couvre le maximum de la réponse en FRÉQUENCE d'un plateau
/// (+6 dB, la somme des gains positifs) ; sa réponse en TEMPS va plus haut —
/// sa norme L1 est plus grande que son gain crête. C'est le défaut B partie 2
/// de T9, TOUJOURS OUVERT (`…_devrait_couvrir_la_reponse_en_temps_d_un_plateau`
/// reste `#[ignore]`).
///
/// Le jour où il sera corrigé, les témoins qui s'appuient ici redemanderont un
/// autre cas — et c'est très bien : aucun ne doit pouvoir rester vert sans
/// écrêtage à observer.
fn plateau_qui_depasse() -> EqProcessor {
    EqProcessor::new(
        &profil(vec![bande("low_shelf", 80.0, 6.0, FRAC_1_SQRT_2)]),
        FS,
        1,
    )
}

/// Le signal du plateau : un carré, dont les fronts font travailler sa réponse
/// en temps. Un sinus à 997 Hz ne dépasserait pas.
fn signal_cas_b() -> Vec<f64> {
    carre(50.0, -0.05, N)
}

// ───────────── capture du journal ─────────────

thread_local! {
    /// Le journal du fil courant. Un tampon PAR FIL, donc par essai : libtest
    /// donne un fil à chacun, et deux essais qui écrêtent en même temps ne se
    /// mélangent pas.
    static TAMPON: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// L'écrivain de l'abonné global : il range chaque ligne dans le tampon du fil
/// qui l'a émise.
#[derive(Clone, Copy, Default)]
struct VersLeFil;

impl std::io::Write for VersLeFil {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        TAMPON.with(|t| t.borrow_mut().extend_from_slice(buf));
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for VersLeFil {
    type Writer = VersLeFil;
    fn make_writer(&'a self) -> Self::Writer {
        VersLeFil
    }
}

/// Pose l'abonné GLOBAL, une fois pour le processus.
///
/// Il DOIT être global, et non posé sur le fil de l'essai le temps d'un
/// `with_default` : `tracing` met en cache POUR TOUT LE PROCESSUS la décision
/// « ce point d'appel intéresse-t-il quelqu'un ? » et le niveau maximal utile.
/// Avec un abonné de fil, les sept voisins de ce binaire — qui écrêtent eux
/// aussi, sans abonné — figent le point d'appel `dsp_ecretage` à « personne »,
/// et le témoin du journal relevait alors ZÉRO ligne quel que soit le
/// comportement du code (mesuré : vert seul, rouge 20 fois sur 20 à
/// `--test-threads=4`, journal entièrement vide). Un abonné global est
/// enregistré avant le premier écrêtage et ne meurt jamais : le point d'appel
/// reste intéressant pour la durée du processus.
fn abonne_global() {
    static POSE: OnceLock<()> = OnceLock::new();
    POSE.get_or_init(|| {
        let abonne = tracing_subscriber::fmt()
            .with_writer(VersLeFil)
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        tracing::subscriber::set_global_default(abonne)
            .expect("aucun autre abonné global dans ce binaire");
    });
}

/// Tout ce que `f` émet au niveau WARN et au-dessus, sur ce fil.
fn capturer(f: impl FnOnce()) -> String {
    abonne_global();
    TAMPON.with(|t| t.borrow_mut().clear());
    f();
    TAMPON.with(|t| String::from_utf8_lossy(&t.borrow()).into_owned())
}

fn lignes_ecretage(journal: &str) -> Vec<&str> {
    journal
        .lines()
        .filter(|l| l.contains("dsp_ecretage"))
        .collect()
}

/// `champ=valeur` ou `champ="valeur"`, selon la façon dont l'abonné imprime.
fn porte(ligne: &str, champ: &str, valeur: &str) -> bool {
    ligne.contains(&format!("{champ}={valeur}")) || ligne.contains(&format!("{champ}=\"{valeur}\""))
}

// ═════════════════════════ A — ReplayGain ═════════════════════════

/// Le cas A de T9 rejoué : +6 dB sans pic tagué sur un sinus à −0,1 dBFS,
/// 16 bits. Compté : 66 % ± 1, excès max 31 866 LSB, premier écrêtage dans les
/// premiers échantillons. Les octets : ceux d'avant, à l'empreinte près, et
/// ceux de `apply_gain_pcm` sans compteur.
#[test]
fn a_replaygain_plus_6_db_sans_pic_compte_66_pour_cent_et_31_866_lsb_sans_bouger_un_octet() {
    let x = sinus(997.0, -0.1, N, 0.0);
    let facteur = facteur_plus_6_sans_pic();

    let mut pcm = vers_pcm(&x, 16);
    let entree = depuis_pcm(&pcm, 16);
    let ideal: Vec<f64> = entree.iter().map(|&v| v as f64 * facteur).collect();
    let mut pcm_sans_compteur = pcm.clone();
    apply_gain_pcm(&mut pcm_sans_compteur, 16, facteur);

    let mut c = CompteurDEcretage::default();
    apply_gain_pcm_compte(&mut pcm, 16, facteur, &mut c);

    let (n_t9, exces_t9) = ecretes_t9(&ideal, 16);
    let pct = c.pourcentage();
    eprintln!(
        "A : vus {} écrêtés {} ({pct:.1} %), excès max {} LSB, crête {:+.2} dBFS, premier à {:?} ; T9 : {n_t9} / {exces_t9:.0} LSB",
        c.echantillons_vus,
        c.echantillons_ecretes,
        c.exces_max_lsb,
        c.crete_max_dbfs().unwrap_or(0.0),
        c.premier_ecretage_a
    );
    assert_eq!(
        pcm, pcm_sans_compteur,
        "le comptage ne change pas un octet de ce que apply_gain_pcm produit"
    );
    assert_eq!(
        fnv1a(&pcm),
        A_RG_PLUS6_16B,
        "empreinte des octets de sortie (dither #4076) : 0x{A_RG_PLUS6_16B:016x} attendue, 0x{:016x} obtenue",
        fnv1a(&pcm)
    );
    assert_eq!(c.echantillons_vus, N as u64, "chaque échantillon est vu");
    assert!(
        (65.2..=67.2).contains(&pct),
        "cas A de T9 : 66 % ± 1 d'échantillons écrêtés, compté {pct:.1} % ({}/{N})",
        c.echantillons_ecretes
    );
    assert!(
        (c.echantillons_ecretes as i64 - n_t9 as i64).abs() <= 8,
        "le compteur suit la condition du clamp, T9 l'arrondi : {} contre {n_t9}",
        c.echantillons_ecretes
    );
    assert_eq!(
        c.exces_max_lsb, 31_866,
        "excès maximal du cas A : 31 866 LSB (T9 : {exces_t9:.0})"
    );
    assert!(
        matches!(c.premier_ecretage_a, Some(p) if p < 20),
        "le sinus franchit le rail dès sa première alternance : {:?}",
        c.premier_ecretage_a
    );
    let crete = c.crete_max_dbfs().expect("crête connue dès qu'on écrête");
    assert!(
        (crete - 5.90).abs() < 0.02,
        "crête idéale +5,90 dBFS (T9), mesurée {crete:+.2}"
    );

    // Les mêmes octets à 24 et 32 bits, le même pourcentage.
    for (bits, empreinte) in [(24u16, A_RG_PLUS6_24B), (32u16, A_RG_PLUS6_32B)] {
        let mut p = vers_pcm(&x, bits);
        let mut c = CompteurDEcretage::default();
        apply_gain_pcm_compte(&mut p, bits, facteur, &mut c);
        assert_eq!(fnv1a(&p), empreinte, "empreinte {bits} bits, dither #4076");
        assert!(
            (65.2..=67.2).contains(&c.pourcentage()),
            "{bits} bits : {:.1} %",
            c.pourcentage()
        );
    }
}

/// `GainReplay` porte la piste : bloc par bloc, les octets sont ceux du
/// traitement d'un seul tenant, et le compteur cumule.
#[test]
fn a_gain_replay_bloc_par_bloc_produit_les_memes_octets_et_cumule() {
    let x = sinus(997.0, -0.1, N, 0.0);
    let facteur = facteur_plus_6_sans_pic();
    let mut pcm = vers_pcm(&x, 16);
    let mut g = GainReplay::new(facteur);
    assert_eq!(g.facteur(), facteur);
    for bloc in pcm.chunks_mut(441 * 2) {
        g.process(bloc, 16);
    }
    let c = g.ecretage();
    assert_eq!(
        fnv1a(&pcm),
        A_RG_PLUS6_16B_EN_100_BLOCS,
        "le même découpage doit rendre les mêmes octets à chaque exécution"
    );
    assert_ne!(
        fnv1a(&pcm),
        A_RG_PLUS6_16B,
        "#4076 : un dither dépend du découpage — si les deux empreintes          coïncident, c'est que le dither a disparu"
    );
    assert_eq!(c.echantillons_vus, N as u64);
    assert_eq!(c.exces_max_lsb, 31_866);
    assert!((65.2..=67.2).contains(&c.pourcentage()));
}

// ═════════════════════════ B — égaliseur ═════════════════════════

/// Le cas B de T9 rejoué : passe-bas 997 Hz Q = 4 sur un sinus à −0,1 dBFS,
/// 🔴 **Retourné par #4073.** Le passe-bas Q = 4 ne dépasse PLUS : la réserve
/// automatique couvre désormais sa résonance (+12,11 dB). Sa moitié de ce
/// témoin garde donc l'inverse de ce qu'elle gardait — zéro écrêté, compteur à
/// zéro, signal intact.
///
/// Le comptage lui-même, qui est le sujet de #4020, reste gardé par la moitié
/// suivante : le plateau grave sur un carré, qui dépasse encore (défaut B
/// partie 2, toujours ouvert).
#[test]
fn b_l_egaliseur_compte_ses_83_7_pour_cent_d_overs_avec_l_exces_et_garde_ses_octets() {
    let mut eq = passe_bas_q4();
    let mut pcm = vers_pcm(&sinus(997.0, -0.1, N, 0.0), 24);
    let entree = pcm.clone();
    let stats = eq.process_pcm(&mut pcm, 24);
    let c = eq.ecretage();
    eprintln!(
        "B : overs {} / compteur {} ({:.1} %), excès max {} LSB, crête {:+.2} dBFS, premier à {:?}",
        stats.overs,
        c.echantillons_ecretes,
        c.pourcentage(),
        c.exces_max_lsb,
        c.crete_max_dbfs().unwrap_or(0.0),
        c.premier_ecretage_a
    );
    assert_eq!(
        stats.overs, 0,
        "réserve correcte depuis #4073 : le passe-bas Q = 4 ne dépasse plus. \
         Avant : 36 896 écrêtés (83,7 %)."
    );
    assert_eq!(
        c.echantillons_ecretes, 0,
        "et le compteur suit : rien à compter"
    );
    assert_eq!(
        c.echantillons_vus, N as u64,
        "les échantillons sont bien VUS"
    );
    assert!(
        c.premier_ecretage_a.is_none(),
        "aucun premier écrêtage : {:?}",
        c.premier_ecretage_a
    );
    // Contre-épreuve : pas seulement « non écrêté » — le signal doit avoir été
    // FILTRÉ puis ramené sous le rail, pas laissé tel quel.
    assert_ne!(
        fnv1a(&pcm),
        fnv1a(&entree),
        "le passe-bas doit tout de même avoir filtré : des octets identiques \
         signifieraient un égaliseur inerte, pas une réserve correcte"
    );

    // Plateau grave +6 dB sur un carré 50 Hz (T9 : 40,4 % d'overs).
    let mut eq2 = EqProcessor::new(&profil(vec![bande("low_shelf", 80.0, 6.0, 0.707)]), FS, 1);
    let mut p2 = vers_pcm(&carre(50.0, -0.05, N), 24);
    let st2 = eq2.process_pcm(&mut p2, 24);
    assert_eq!(
        fnv1a(&p2),
        B_EQ_LOWSHELF_CARRE_24B,
        "empreinte plateau inchangée"
    );
    assert_eq!(st2.overs, B_EQ_LOWSHELF_CARRE_OVERS);
    assert_eq!(eq2.ecretage().echantillons_ecretes, st2.overs);

    // Chemin flottant, même passe-bas : compté, jamais saturé (T9).
    let mut eq3 = passe_bas_q4();
    let mut fl: Vec<f32> = sinus(997.0, -0.1, N, 0.0)
        .iter()
        .map(|&v| v as f32)
        .collect();
    let st3 = eq3.process_interleaved(&mut fl);
    let fl_bytes: Vec<u8> = fl.iter().flat_map(|v| v.to_le_bytes()).collect();
    assert_eq!(
        fnv1a(&fl_bytes),
        B_EQ_FLOTTANT_Q4,
        "empreinte flottante inchangée"
    );
    assert_eq!(
        st3.overs, 0,
        "le chemin FLOTTANT bénéficie de la même réserve (#4073) : plus un \
         seul dépassement. Avant : 36 896."
    );
    assert_eq!(eq3.ecretage().echantillons_ecretes, st3.overs);
    assert_eq!(eq3.ecretage().echantillons_vus, N as u64);

    // La chaîne q2 de T9 : ReplayGain +6 dB (pic d'échantillon tagué) puis
    // égaliseur peak 3 kHz +6 dB Q 1 : rien n'écrête à l'égaliseur.
    let fch = gain_factor(
        TrackGain {
            gain_db: 6.0,
            peak: Some(0.9943),
        },
        reglages(true, 0.0),
    );
    let mut ch = vers_pcm(&sinus(997.0, -0.1, N, 0.0), 16);
    apply_gain_pcm(&mut ch, 16, fch);
    assert_eq!(
        fnv1a(&ch),
        Q2_RG_PIC_16B,
        "empreinte ReplayGain avec pic, dither #4076"
    );
    let mut eq4 = EqProcessor::new(&profil(vec![bande("peak", 3000.0, 6.0, 1.0)]), FS, 1);
    let st4 = eq4.process_pcm(&mut ch, 16);
    assert_eq!(
        fnv1a(&ch),
        Q2_CHAINE_RG_EQ_16B,
        "empreinte de la chaîne, dither #4076"
    );
    assert_eq!(st4.overs, 0);
    assert_eq!(
        eq4.ecretage(),
        CompteurDEcretage {
            echantillons_vus: N as u64,
            ..Default::default()
        }
    );
}

// ═════════════════════════ sous 0 dBFS : zéro, et silence ═════════════════════════

/// Un signal qui reste sous le rail compte zéro à chaque étage, et le journal
/// ne dit rien — ni au premier bloc, ni à la fin de la piste. Les octets de la
/// troncature vers zéro (défaut E de T9, non corrigé) sont ceux d'avant.
#[test]
fn un_signal_sous_0_dbfs_compte_zero_et_le_journal_reste_vide() {
    let x = sinus(997.0, -0.1, N, 0.0);
    let fm1 = facteur_moins_1();

    for (bits, empreinte) in [
        (16u16, E_RG_MOINS1_16B),
        (24u16, E_RG_MOINS1_24B),
        (32u16, E_RG_MOINS1_32B),
    ] {
        let mut p = vers_pcm(&x, bits);
        let mut c = CompteurDEcretage::default();
        apply_gain_pcm_compte(&mut p, bits, fm1, &mut c);
        assert_eq!(
            fnv1a(&p),
            empreinte,
            "ReplayGain −1 dB, {bits} bits : octets dithérés reproductibles"
        );
        assert_eq!(
            c.echantillons_ecretes, 0,
            "{bits} bits : rien n'écrête à −1 dB"
        );
        assert_eq!(c.premier_ecretage_a, None);
        assert_eq!(c.exces_max_lsb, 0);
        assert_eq!(c.crete_max_dbfs(), None);
        assert_eq!(c.echantillons_vus, N as u64);
    }
    for (bits, empreinte) in [
        (16u16, E_MIXEUR_MOINS1_16B),
        (24u16, E_MIXEUR_MOINS1_24B),
        (32u16, E_MIXEUR_MOINS1_32B),
    ] {
        let mut p = vers_pcm(&x, bits);
        PcmMixer::apply_gain(&mut p, fm1 as f32, bits).unwrap();
        assert_eq!(
            fnv1a(&p),
            empreinte,
            "mixeur −1 dB, {bits} bits : octets dithérés reproductibles"
        );
    }

    let journal = capturer(|| {
        let mut g = GainReplay::new(fm1);
        let mut p = vers_pcm(&x, 16);
        for bloc in p.chunks_mut(441 * 2) {
            g.process(bloc, 16);
        }
        assert_eq!(g.ecretage().echantillons_ecretes, 0);
        drop(g);

        let mut eq = EqProcessor::new(&profil(vec![bande("peak", 3000.0, -3.0, 1.0)]), FS, 1);
        let mut p = vers_pcm(&x, 24);
        for bloc in p.chunks_mut(441 * 3) {
            eq.process_pcm(bloc, 24);
        }
        assert_eq!(eq.ecretage().echantillons_ecretes, 0);
        assert_eq!(eq.process_stats().overs, 0);
        drop(eq);
    });
    assert!(
        lignes_ecretage(&journal).is_empty(),
        "une piste propre ne laisse aucune ligne dsp_ecretage :\n{journal}"
    );
}

// ═════════════════════════ le journal : deux lignes par piste ═════════════════════════

/// Une piste de 100 blocs qui écrête à chaque bloc : le journal porte DEUX
/// lignes `dsp_ecretage` par étage — `moment=premier` après le premier bloc,
/// `moment=fin` à la destruction avec le total — et jamais cent.
#[test]
fn le_journal_dit_deux_lignes_par_piste_jamais_une_par_bloc() {
    let x = signal_cas_b();
    let facteur = facteur_plus_6_sans_pic();

    let journal = capturer(|| {
        let mut eq = plateau_qui_depasse();
        let mut p = vers_pcm(&x, 24);
        for bloc in p.chunks_mut(441 * 3) {
            eq.process_pcm(bloc, 24);
        }
        assert_eq!(
            fnv1a(&p),
            B_EQ_LOWPASS_Q4_24B,
            "100 blocs = un seul tenant, à l'octet (l'état des biquads est continu)"
        );
        assert_eq!(eq.ecretage().echantillons_ecretes, B_EQ_LOWPASS_Q4_OVERS);
        drop(eq);

        let mut g = GainReplay::new(facteur);
        let mut p = vers_pcm(&x, 16);
        for bloc in p.chunks_mut(441 * 2) {
            g.process(bloc, 16);
        }
        drop(g);
    });

    let lignes = lignes_ecretage(&journal);
    eprintln!("journal :\n{}", lignes.join("\n"));
    assert_eq!(
        lignes.len(),
        4,
        "deux lignes par étage (premier, fin) pour 100 blocs écrêtants, pas 200 :\n{journal}"
    );
    let eq: Vec<&&str> = lignes
        .iter()
        .filter(|l| porte(l, "etage", "egaliseur"))
        .collect();
    let rg: Vec<&&str> = lignes
        .iter()
        .filter(|l| porte(l, "etage", "replaygain"))
        .collect();
    assert_eq!(eq.len(), 2, "égaliseur : premier + fin");
    assert_eq!(rg.len(), 2, "replaygain : premier + fin");
    for (nom, paire) in [("egaliseur", &eq), ("replaygain", &rg)] {
        assert!(
            porte(paire[0], "moment", "premier") && porte(paire[0], "portee", "piste"),
            "{nom} : la première ligne dit le premier écrêtage de la piste : {}",
            paire[0]
        );
        assert!(
            porte(paire[1], "moment", "fin") && porte(paire[1], "portee", "piste"),
            "{nom} : la seconde dit la fin : {}",
            paire[1]
        );
        assert!(
            paire[0].contains("WARN"),
            "{nom} : niveau WARN, pas DEBUG : {}",
            paire[0]
        );
    }
    assert!(
        porte(eq[1], "echantillons_ecretes", "17825") && porte(eq[1], "echantillons_vus", "44100"),
        "égaliseur, fin : le total de la piste : {}",
        eq[1]
    );
    assert!(
        porte(rg[1], "exces_max_lsb", "32239") && porte(rg[1], "echantillons_vus", "44100"),
        "replaygain, fin : l'excès max de la piste : {}",
        rg[1]
    );
    assert!(
        porte(eq[0], "echantillons_vus", "441") || porte(eq[0], "echantillons_vus", "882"),
        "égaliseur, premier : dit après le PREMIER bloc, pas à la fin : {}",
        eq[0]
    );
}

/// Remplacement à chaud (#1725, #3479) : le processeur relayé par
/// `inherit_state_from` se tait à sa destruction — la piste continue — et le
/// relais dit UNE fin, avec le total des deux.
#[test]
fn un_processeur_relaye_a_chaud_ne_clot_pas_la_piste() {
    let x = signal_cas_b();
    let journal = capturer(|| {
        let mut p = vers_pcm(&x, 24);
        let (avant, apres) = p.split_at_mut(N / 2 * 3);
        let mut eq1 = plateau_qui_depasse();
        eq1.process_pcm(avant, 24);
        let mut eq2 = plateau_qui_depasse();
        eq2.inherit_state_from(&eq1);
        drop(eq1);
        eq2.process_pcm(apres, 24);
        assert_eq!(
            fnv1a(&p),
            B_EQ_LOWPASS_Q4_24B,
            "l'historique relayé rend les mêmes octets qu'un seul processeur"
        );
        assert_eq!(eq2.ecretage().echantillons_ecretes, B_EQ_LOWPASS_Q4_OVERS);
        drop(eq2);
    });
    let lignes = lignes_ecretage(&journal);
    assert_eq!(
        lignes.len(),
        2,
        "un premier (eq1) et UNE fin (eq2), pas une fin par processeur :\n{journal}"
    );
    assert!(porte(lignes[0], "moment", "premier"));
    assert!(
        porte(lignes[1], "moment", "fin") && porte(lignes[1], "echantillons_ecretes", "17825"),
        "la fin porte le total des deux moitiés : {}",
        lignes[1]
    );
}

// ═════════════════════════ mixeur et registre ═════════════════════════

/// Le mixeur compte ce que `SampleFormat::write` ramène au rail, dans le
/// registre du processus, et garde ses octets.
#[test]
fn le_mixeur_compte_ce_qu_il_ecrete_dans_le_registre_et_garde_ses_octets() {
    let x = sinus(997.0, -0.1, N, 0.0);
    let entree = depuis_pcm(&vers_pcm(&x, 16), 16);
    let ideal: Vec<f64> = entree.iter().map(|&v| v as f64 * 2.0).collect();
    let (n_t9, exces_t9) = ecretes_t9(&ideal, 16);

    let avant = releve().mixeur;
    let mut p = vers_pcm(&x, 16);
    PcmMixer::apply_gain(&mut p, 2.0, 16).unwrap();
    let apres = releve().mixeur;

    assert_eq!(fnv1a(&p), A_MIXEUR_X2_16B, "mixeur ×2 : octets inchangés");
    let delta = apres.echantillons_ecretes - avant.echantillons_ecretes;
    eprintln!("mixeur ×2 : registre +{delta} écrêtés (T9 : {n_t9}, excès {exces_t9:.0} LSB)");
    assert!(
        delta >= n_t9 as u64 - 8,
        "le registre a reçu les écrêtés du mixeur : +{delta}, attendu ≥ {}",
        n_t9 - 8
    );
    assert!(apres.echantillons_vus - avant.echantillons_vus >= N as u64);
    assert!(apres.appels_ecretants > avant.appels_ecretants);
    assert!(apres.exces_max_lsb >= exces_t9.round() as u64);
}

/// Le registre cumule par étage ce que `apply_gain_pcm` (sans piste) a
/// écrêté, et le rapport de diagnostic le sérialise sous `dsp_ecretage`.
#[test]
fn le_registre_cumule_par_etage_et_le_rapport_le_lit() {
    let x = sinus(997.0, -0.1, N, 0.0);
    let facteur = facteur_plus_6_sans_pic();
    let avant = releve();
    let mut p = vers_pcm(&x, 16);
    apply_gain_pcm(&mut p, 16, facteur);
    let apres = releve();

    let delta = apres.replaygain.echantillons_ecretes - avant.replaygain.echantillons_ecretes;
    assert!(
        delta >= 29_000,
        "replaygain : +{delta} écrêtés dans le registre (cas A : 29 174 ; les autres témoins du binaire y ajoutent les leurs)"
    );
    assert!(apres.replaygain.appels_ecretants > avant.replaygain.appels_ecretants);
    assert_eq!(
        apres.replaygain.exces_max_lsb.max(31_866),
        apres.replaygain.exces_max_lsb
    );
    assert!(apres.replaygain.pourcentage > 0.0);

    let v = serde_json::to_value(apres).unwrap();
    for etage in ["replaygain", "egaliseur", "mixeur"] {
        for champ in [
            "echantillons_vus",
            "echantillons_ecretes",
            "exces_max_lsb",
            "appels_ecretants",
            "pistes_ecretees",
            "lignes_journal",
        ] {
            assert!(
                v[etage][champ].is_u64(),
                "dsp_ecretage.{etage}.{champ} manque ou n'est pas un entier : {v}"
            );
        }
        assert!(v[etage]["pourcentage"].is_number());
    }
    let noms: Vec<&str> = apres.etages().iter().map(|(n, _)| *n).collect();
    assert_eq!(noms, ["replaygain", "egaliseur", "mixeur"]);
}
