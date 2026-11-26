//! Revue de #9561 — la marge MANUELLE et la convolution : une seule
//! autorité, mesurée sur le signal (étage des égaliseurs tel que le moteur le
//! construit, puis convolueur tel que le jeu de la zone le charge).
//!
//! | Cas                              | Marge                                   |
//! |----------------------------------|-----------------------------------------|
//! | Convolution seule, manuelle M    | M (gain fixe local, ou le convolueur sur une sortie réseau) ; `insuffisante` si M > −(G + 0,5) |
//! | Égaliseur + convolution, manuelle| `min(M, L1)` à l'égaliseur, rien de plus au filtre ; `insuffisante` si ce qui reste de M après L1 ne couvre pas G |

use super::*;
use crate::audio::eq::{EqBandSpec, EqProcessor, EqProfile};
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::tess::chaine::{ChaineTess, Headroom, MaillonGreffon};
use crate::tess::etages_fixes::{MARGE_DE_SECURITE_CONVOLUTION_DB, marge_de_convolution};
use std::sync::Arc;

const FS: u32 = 48_000;
const BLOC: usize = 1024;

fn base() -> Arc<dyn crate::db::backend::DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

/// Une zone, égaliseur installé ; `locale` : sortie locale, sinon DLNA.
fn zone(b: &Arc<dyn crate::db::backend::DbBackend>, nom: &str, locale: bool) -> i64 {
    let s = SettingsRepo::with_backend(b.clone());
    s.set("plugin_equalizer_installed", "true").unwrap();
    s.set("plugin_equalizer_enabled", "true").unwrap();
    let (t, d) = if locale {
        ("local", format!("local:{nom}-9561"))
    } else {
        ("dlna", format!("uuid:{nom}-9561"))
    };
    ZoneRepo::with_backend(b.clone())
        .create(nom, Some(t), Some(&d))
        .unwrap()
}

/// Un paramétrique d'une bande : +6 dB à 50 Hz.
fn bosse_50_hz() -> EqProfile {
    EqProfile {
        enabled: true,
        bands: vec![EqBandSpec {
            freq: 50.0,
            gain: 6.0,
            q: 1.0,
            band_type: "peak".into(),
            channel: None,
        }],
        ..Default::default()
    }
}

fn maillon_eq(p: &EqProfile) -> MaillonGreffon {
    let mut v = serde_json::to_value(p).unwrap();
    v.as_object_mut().unwrap().remove("enabled");
    MaillonGreffon {
        id: "equalizer-parametric".into(),
        enabled: true,
        settings: v,
    }
}

fn chaine(b: &Arc<dyn crate::db::backend::DbBackend>, z: i64, m: f64, eq: Option<&EqProfile>) {
    let mut c = ChaineTess {
        plugins: eq.map(maillon_eq).into_iter().collect(),
        ..ChaineTess::default()
    };
    c.fixed.headroom = Headroom::Manual { db: m };
    crate::tess::migration::ecrire_chaine(&SettingsRepo::with_backend(b.clone()), z, &c).unwrap();
}

/// Un filtre plat de +6,02 dB (Dirac de 2), écrit en WAV.
fn ri_plus_6(nom: &str) -> (crate::test_scratch::ScratchDir, String) {
    let d = crate::test_scratch::scratch_dir(nom);
    let p = d.path().join("plus6.wav");
    let mut h = vec![0.0f32; 256];
    h[0] = 2.0;
    ecrire_wav_float32(p.to_str().unwrap(), &h, 1, FS).unwrap();
    let p = p.to_str().unwrap().to_owned();
    (d, p)
}

fn sinus(f: f64, secondes: f64, a: f64) -> Vec<f64> {
    let n = (f64::from(FS) * secondes) as usize;
    (0..n)
        .flat_map(|i| {
            let x = a * (2.0 * std::f64::consts::PI * f * i as f64 / f64::from(FS)).sin();
            [x, x]
        })
        .collect()
}

fn crete(v: &[f64]) -> f64 {
    v.iter().fold(0.0f64, |m, x| m.max(x.abs()))
}

/// Ce que la zone rend d'un sinus : l'égaliseur que le moteur construit
/// (`profil_du_moteur`), puis le convolueur du jeu de la zone. Crête du
/// régime établi.
fn rendu(
    b: &Arc<dyn crate::db::backend::DbBackend>,
    z: i64,
    ri: &str,
    f: f64,
    a: f64,
) -> (f64, crate::tess::etages_fixes::MargeDeConvolution) {
    let s = SettingsRepo::with_backend(b.clone());
    let jeu = JeuDeZone::avec_principal(&s, z, ri.to_owned());
    let config = jeu.charger().unwrap();
    let marge = config.marge_pour(FS).unwrap();
    let mut y = sinus(f, 2.0, a);
    if let Some(p) = crate::tess::egaliseurs_en_serie::profil_du_moteur(&s, z, true) {
        let mut eq = EqProcessor::new(&p, FS, 2);
        if eq.is_enabled() {
            eq.process_interleaved_f64(&mut y);
        }
    }
    let mut c = config.build_for(BLOC, FS, 2).unwrap();
    c.process_interleaved_f64(&mut y);
    (crete(&y[y.len() / 2..]), marge)
}

fn db(x: f64) -> f64 {
    20.0 * x.log10()
}

/// La règle, cas par cas (le tableau du module).
#[test]
fn regle_manuelle_une_seule_autorite_9561() {
    // Aucun étage d'égaliseur : le convolueur porte M.
    let m = marge_de_convolution(&Headroom::Manual { db: -8.0 }, 6.0, None);
    assert_eq!(m.appliquee_db, -8.0);
    assert!(!m.insuffisante, "{m:?}");
    let m = marge_de_convolution(&Headroom::Manual { db: -3.0 }, 6.0, None);
    assert_eq!(m.appliquee_db, -3.0);
    assert!(m.insuffisante, "M > −(G + 0,5) : {m:?}");
    // L'étage des égaliseurs porte M (gain fixe : il laisse M au filtre).
    let m = marge_de_convolution(&Headroom::Manual { db: -8.0 }, 6.0, Some(-8.0));
    assert_eq!(m.appliquee_db, 0.0, "jamais deux fois");
    assert!(!m.insuffisante);
    // Empilé : L1 = −7 dB en prend 7 sur M = −10 ; il en reste 3 au filtre.
    let m = marge_de_convolution(&Headroom::Manual { db: -10.0 }, 6.0, Some(-3.0));
    assert_eq!(m.appliquee_db, 0.0);
    assert!(m.insuffisante, "L1 compte : {m:?}");
    // Auto : inchangé, l'étage ne compte pas.
    let m = marge_de_convolution(&Headroom::Auto {}, 6.0, Some(-3.0));
    assert_eq!(m.appliquee_db, -(6.0 + MARGE_DE_SECURITE_CONVOLUTION_DB));
    // Un filtre qui n'amplifie pas n'est jamais « insuffisant ».
    assert!(!marge_de_convolution(&Headroom::Manual { db: 0.0 }, -2.0, None).insuffisante);
}

/// Défaut 3 (a) — sortie RÉSEAU sans égaliseur, M = −8 dB, filtre +6 dB :
/// le convolueur porte M (personne d'autre ne l'appliquerait). Mesure : un
/// sinus à −1 dBFS sort à −1 + 6 − 8 = −3 dBFS, et non à +5 dBFS (saturation,
/// le filtre à 0 dB de marge avant le correctif).
#[test]
fn reseau_sans_egaliseur_le_convolueur_porte_m_9561() {
    let b = base();
    let z = zone(&b, "reseau-conv", false);
    chaine(&b, z, -8.0, None);
    let (_d, ri) = ri_plus_6("ri-9561-reseau");
    let a = 10f64.powf(-1.0 / 20.0);
    let (c, marge) = rendu(&b, z, &ri, 997.0, a);
    assert_eq!(marge.appliquee_db, -8.0, "{marge:?}");
    assert!(!marge.insuffisante, "{marge:?}");
    assert!(c <= 1.0, "saturation : {:.2} dBFS", db(c));
    assert!(
        (db(c) - (-1.0 + 20.0 * 2f64.log10() - 8.0)).abs() < 0.05,
        "{:.2} dBFS",
        db(c)
    );
    // M trop faible (−3 dB) : appliquée telle quelle, et dite insuffisante.
    chaine(&b, z, -3.0, None);
    let (c, marge) = rendu(&b, z, &ri, 997.0, a);
    assert_eq!(marge.appliquee_db, -3.0);
    assert!(marge.insuffisante, "{marge:?}");
    assert!(c > 1.0, "contre-épreuve : M insuffisante sature ({c})");
}

/// Défaut 1 — sortie LOCALE, M = −8 dB, filtre +6 dB, sans égaliseur : M
/// est le gain fixe de l'étage des égaliseurs, le convolueur n'ajoute RIEN
/// (pas de double atténuation). Mesure : −1 + 6 − 8 = −3 dBFS. Contre-épreuve :
/// le filtre chargé comme avant (`from_wav`, toujours en Auto) ajoute −6,5 dB.
#[test]
fn locale_manuelle_pas_de_double_attenuation_9561() {
    let b = base();
    let z = zone(&b, "locale-conv", true);
    chaine(&b, z, -8.0, None);
    let (_d, ri) = ri_plus_6("ri-9561-locale");
    let a = 10f64.powf(-1.0 / 20.0);
    let (c, marge) = rendu(&b, z, &ri, 997.0, a);
    assert_eq!(marge.appliquee_db, 0.0, "{marge:?}");
    assert!(!marge.insuffisante);
    let attendu = -1.0 + 20.0 * 2f64.log10() - 8.0;
    assert!((db(c) - attendu).abs() < 0.05, "{:.2} dBFS", db(c));
    // Contre-épreuve : l'ancien chargement de la route TESS (Auto forcé).
    let ancien = ConvolverConfig::from_wav(&ri).unwrap();
    let mut y = sinus(997.0, 2.0, a * 10f64.powf(-8.0 / 20.0));
    ancien
        .build_for(BLOC, FS, 2)
        .unwrap()
        .process_interleaved_f64(&mut y);
    assert!(
        db(crete(&y[y.len() / 2..])) < attendu - 6.0,
        "le from_wav double la marge"
    );
}

/// Défaut 3 (b) — égaliseur (+6 dB à 50 Hz) ET filtre (+6 dB) empilés,
/// marge manuelle : `insuffisante` dit vrai exactement quand le signal
/// sature, parce qu'il compte ce que la réserve L1 de l'égaliseur prend à M.
#[test]
fn empiles_l_indicateur_compte_l1_9561() {
    let b = base();
    let z = zone(&b, "empiles", true);
    let (_d, ri) = ri_plus_6("ri-9561-empiles");
    let eq = bosse_50_hz();
    // L1 de l'égaliseur, à cette cadence.
    let l1 = eq.reserve_db_at(0, f64::from(FS));
    assert!(l1 < -3.0, "L1 = {l1}");
    // M = L1 − 3 dB : M couvre L1 mais ne laisse que 3 dB au filtre de +6.
    // Avant le correctif : M seule (≤ −6,5 dB) passait pour suffisante.
    let m = (l1 - 3.0).min(-6.6);
    chaine(&b, z, m, Some(&eq));
    let (c, marge) = rendu(&b, z, &ri, 50.0, 1.0);
    assert!(c > 1.0, "le signal sature : {:.2} dBFS", db(c));
    assert!(marge.insuffisante, "l'indicateur doit le dire : {marge:?}");
    assert_eq!(marge.appliquee_db, 0.0, "jamais deux fois");
    // M = L1 − 8 dB : il reste 8 dB au filtre, rien ne sature.
    chaine(&b, z, l1 - 8.0, Some(&eq));
    let (c, marge) = rendu(&b, z, &ri, 50.0, 1.0);
    assert!(c <= 1.0, "{:.2} dBFS", db(c));
    assert!(!marge.insuffisante, "{marge:?}");
}
