//! Épreuves du lot T07 (plan TESS, ligne T07) :
//! - volume à 0 dB et chaîne vide → bit-perfect (empreinte SHA-256) ;
//! - contre-épreuve : volume à −1 dB → dither présent ;
//! - headroom automatique armé et désarmé avec le traitement ;
//! - pas de double compte avec la compensation (−12 dB de #4594) ;
//! - latence du volume mesurée (< 50 ms visé), banc simulé : AUCUN son réel ;
//! - mise en forme du bruit : stable à 0 dBFS, bruit baissé dans le grave ;
//! - Pure : le volume logiciel reste appliqué (comportement réel vérifié).

use std::collections::VecDeque;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::*;
use crate::audio::dither::Dither as Tpdf;
use crate::tess::chaine::{ChaineTess, Dither, Headroom, MaillonGreffon, TauxDsd, Upsampling};
use crate::tess::etage::{
    Disposition, ErreurEtage, EtageTess, EtapeFinale, FormatFlux, SortieFinale,
};
use crate::tess::greffon::catalogue_de_reference;
use crate::tess::resolution::{
    Badge, CodeEtape, ContexteExecution, ControleVolume, PlacementVolume, Resolution, SourceFlux,
    VolumeZone,
};
use crate::tess::zones::TransportZone;

const CD: FormatFlux = FormatFlux {
    channels: 2,
    sample_rate_hz: 44_100,
};
const BLOC: usize = 512;

fn installe(_: &str) -> bool {
    true
}

fn ctx(volume_db: f64, pure: bool) -> ContexteExecution<'static> {
    ContexteExecution {
        transport: TransportZone::Local,
        source: Some(SourceFlux::Pcm {
            sample_rate_hz: 44_100,
            bits: 24,
            channels: 2,
        }),
        premium: true,
        greffon_disponible: &installe,
        pure,
        volume: VolumeZone {
            control: ControleVolume::Software,
            db: volume_db,
        },
    }
}

fn resoudre(c: &ChaineTess, x: &ContexteExecution<'_>) -> Resolution {
    c.resoudre(&catalogue_de_reference(), x)
}

fn avec(ids: &[(&str, bool)]) -> ChaineTess {
    ChaineTess {
        plugins: ids
            .iter()
            .map(|(id, enabled)| MaillonGreffon {
                id: (*id).into(),
                enabled: *enabled,
                settings: Value::Null,
            })
            .collect(),
        ..ChaineTess::default()
    }
}

/// Mots 24 bits cadrés à gauche : extrêmes, petits mots et un balayage
/// pseudo-aléatoire, comme une vraie piste (déterministe).
fn source_24_bits(trames: usize) -> Vec<i32> {
    let mut g = Tpdf::depuis_graine(0x7e57_7007);
    let mut mots = Vec::with_capacity(trames * 2);
    for i in 0..trames * 2 {
        let m: i32 = match i {
            0 => 8_388_607,
            1 => -8_388_608,
            2 | 3 => 0,
            4 => 1,
            5 => -1,
            _ => ((g.tirer() * 0.5) * 8_388_607.0).round() as i32,
        };
        mots.push(m << 8);
    }
    mots
}

fn empreinte(mots: &[i32]) -> String {
    let mut h = Sha256::new();
    for m in mots {
        h.update(m.to_le_bytes());
    }
    format!("{:x}", h.finalize())
}

/// La chaîne des étages fixes, comme l'exécuteur la montera : Headroom
/// (producteur) → [greffons] → Volume (rendu) → étape finale.
struct Banc {
    headroom: EtageHeadroom,
    greffons: Vec<Box<dyn EtageTess>>,
    volume: EtageVolume,
    finale: Box<dyn EtapeFinale>,
}

impl Banc {
    fn new(
        bilan: &BilanDeNiveau,
        consigne: std::sync::Arc<ConsigneVolume>,
        greffons: Vec<Box<dyn EtageTess>>,
        finale: Box<dyn EtapeFinale>,
    ) -> Self {
        let mut b = Self {
            headroom: EtageHeadroom::new(bilan.gain_d_entree_db).unwrap(),
            greffons,
            volume: EtageVolume::new(consigne, PlacementVolume::AtRender),
            finale,
        };
        b.headroom.preparer(CD, BLOC).unwrap();
        for g in &mut b.greffons {
            assert!(g.preparer(CD, BLOC).unwrap().sur_place);
        }
        b.volume.preparer(CD, BLOC).unwrap();
        b.finale.preparer(CD, BLOC).unwrap();
        b
    }

    /// Joue des mots 24 bits, rend des mots 24 bits.
    fn jouer(&mut self, source: &[i32]) -> Vec<i32> {
        let mut sortie = vec![0i32; source.len()];
        let mut f = vec![0f64; BLOC * 2];
        for (bloc, out) in source.chunks(BLOC * 2).zip(sortie.chunks_mut(BLOC * 2)) {
            let trames = bloc.len() / 2;
            for (x, m) in f.iter_mut().zip(bloc) {
                *x = f64::from(*m) / 2_147_483_648.0;
            }
            self.headroom.traiter_sur_place(&mut f, trames).unwrap();
            for g in &mut self.greffons {
                g.traiter_sur_place(&mut f, trames).unwrap();
            }
            self.volume.traiter_sur_place(&mut f, trames).unwrap();
            let n = self
                .finale
                .finaliser(
                    &f,
                    trames,
                    SortieFinale::Pcm {
                        echantillons: out,
                        bits: 24,
                    },
                )
                .unwrap();
            assert_eq!(n, bloc.len());
        }
        sortie
    }
}

/// Un greffon de test : gain fixe (l'égaliseur qui réserve sa marge).
struct GainFixe(f64);

impl EtageTess for GainFixe {
    fn id(&self) -> &str {
        "gain-de-test"
    }
    fn preparer(&mut self, entree: FormatFlux, max: usize) -> Result<Disposition, ErreurEtage> {
        Ok(Disposition::sur_place(entree, max, 0))
    }
    fn traiter_sur_place(&mut self, t: &mut [f64], trames: usize) -> Result<(), ErreurEtage> {
        t[..trames * 2].iter_mut().for_each(|x| *x *= self.0);
        Ok(())
    }
    fn bypass(&self) -> bool {
        false
    }
    fn definir_bypass(&mut self, _: bool) {}
}

fn bilan(chaine: &ChaineTess, r: &Resolution, greffons_db: f64) -> BilanDeNiveau {
    BilanDeNiveau::calculer(&EntreesNiveau {
        headroom: chaine.fixed.headroom,
        headroom_resolu_db: r.headroom_db,
        replaygain_db: 0.0,
        gain_de_zone_db: 0.0,
        gain_moyen_greffons_db: greffons_db,
        compensation_active: true,
    })
}

fn banc_pour(volume_db: f64, chaine: &ChaineTess) -> (Banc, Resolution) {
    let r = resoudre(chaine, &ctx(volume_db, false));
    let b = bilan(chaine, &r, 0.0);
    let consigne = ConsigneVolume::new(b.gain_de_rendu(lineaire(volume_db)).lineaire);
    (
        Banc::new(
            &b,
            consigne,
            Vec::new(),
            Box::new(QuantificationMiseEnForme::new()),
        ),
        r,
    )
}

// ═══════════════ Épreuve : bit-perfect, empreinte ═══════════════

#[test]
fn chaine_vide_et_volume_a_0_db_sont_bit_perfect_a_l_empreinte() {
    let chaine = ChaineTess::default();
    let (mut banc, r) = banc_pour(0.0, &chaine);
    assert_eq!(r.badge, Badge::BitPerfect);
    assert_eq!(r.headroom_db, 0.0);
    assert_eq!(
        choisir_etape_finale(&chaine, &r),
        EtapeFinaleChoisie::Pcm {
            dither: true,
            mise_en_forme: false
        }
    );
    let source = source_24_bits(44_100);
    let sortie = banc.jouer(&source);
    assert_eq!(
        empreinte(&sortie),
        empreinte(&source),
        "chaîne vide, volume 0 dB : la sortie doit être le mot source, à l'octet"
    );
    // Même épreuve avec `noise_shaped` réglé : rien à quantifier, rien
    // d'ajouté.
    let mut c = ChaineTess::default();
    c.fixed.quantization.dither = Dither::NoiseShaped;
    let (mut banc, r) = banc_pour(0.0, &c);
    assert_eq!(r.badge, Badge::BitPerfect);
    assert_eq!(empreinte(&banc.jouer(&source)), empreinte(&source));
}

// ═══════════════ Contre-épreuve : −1 dB, dither présent ═══════════════

#[test]
fn volume_a_moins_1_db_requantifie_avec_dither() {
    // L'étape finale du banc est la mise en forme (ce lot) ; la même épreuve
    // en TPDF est celle de `tess::quantification` (T03), à rejouer sur ce
    // banc une fois T03 fusionné.
    let chaine = ChaineTess::default();
    let (mut banc, r) = banc_pour(-1.0, &chaine);
    assert_eq!(r.badge, Badge::ProcessedByTess);
    assert!(
        r.etapes
            .iter()
            .any(|e| e.code == CodeEtape::Quantization && e.effective)
    );
    let source = source_24_bits(44_100);
    let sortie = banc.jouer(&source);
    assert_ne!(empreinte(&sortie), empreinte(&source));
    // L'arrondi nu du même produit : le dither doit s'en écarter.
    let g = lineaire(-1.0);
    let nu: Vec<i32> = source
        .iter()
        .map(|m| ((f64::from(m >> 8) * g).round() as i32) << 8)
        .collect();
    let differents = sortie.iter().zip(&nu).filter(|(a, b)| a != b).count();
    assert!(
        differents > source.len() / 10,
        "dither absent : {differents} mots sur {} diffèrent de l'arrondi nu",
        source.len()
    );
    // Erreur bornée (mise en forme comprise) et de moyenne nulle.
    let mut somme = 0.0;
    for (s, m) in sortie.iter().zip(&source) {
        let e = f64::from(s >> 8) - f64::from(m >> 8) * g;
        assert!(e.abs() < 24.0, "erreur {e} LSB");
        somme += e;
    }
    assert!((somme / sortie.len() as f64).abs() < 0.05);
}

/// Le dither linéarise : un signal à 0,3 LSB après le volume ressort en
/// moyenne à 0,3 LSB (un arrondi nu le rendrait à 0).
#[test]
fn sous_le_lsb_le_dither_garde_le_niveau_moyen() {
    let mut q = QuantificationMiseEnForme::new();
    q.preparer(CD, 4096).unwrap();
    let x = vec![0.3 / 8_388_608.0; 8192];
    let mut mots = vec![0i32; 8192];
    q.finaliser(
        &x,
        4096,
        SortieFinale::Pcm {
            echantillons: &mut mots,
            bits: 24,
        },
    )
    .unwrap();
    let moyenne = mots.iter().map(|m| f64::from(m >> 8)).sum::<f64>() / 8192.0;
    assert!((moyenne - 0.3).abs() < 0.03, "moyenne {moyenne}");
}

// ═══════════════ Headroom automatique ═══════════════

#[test]
fn headroom_auto_arme_avec_un_traitement_et_desarme_sans() {
    let source = source_24_bits(4_410);
    // Crossfeed coché (Free, stéréo) : traitement effectif ⇒ −3 dB.
    let arme = avec(&[("crossfeed", true)]);
    let r = resoudre(&arme, &ctx(0.0, false));
    assert_eq!(r.headroom_db, -3.0);
    let b = bilan(&arme, &r, 0.0);
    assert_eq!(b.gain_d_entree_db, -3.0);
    let mut h = EtageHeadroom::new(b.gain_d_entree_db).unwrap();
    h.preparer(CD, BLOC).unwrap();
    let mut f: Vec<f64> = source[..BLOC * 2]
        .iter()
        .map(|m| f64::from(*m) / 2_147_483_648.0)
        .collect();
    let avant = f.clone();
    h.traiter_sur_place(&mut f, BLOC).unwrap();
    for (a, b) in f.iter().zip(&avant) {
        assert_eq!(*a, b * lineaire(-3.0));
    }
    // Décoché : rien à protéger ⇒ 0 dB, l'étage ne touche rien.
    let desarme = avec(&[("crossfeed", false)]);
    let r = resoudre(&desarme, &ctx(0.0, false));
    assert_eq!(r.headroom_db, 0.0);
    let b = bilan(&desarme, &r, 0.0);
    assert_eq!(b.gain_d_entree_db, 0.0);
    let consigne = ConsigneVolume::new(1.0);
    let mut banc = Banc::new(
        &b,
        consigne,
        Vec::new(),
        Box::new(QuantificationMiseEnForme::new()),
    );
    assert_eq!(empreinte(&banc.jouer(&source)), empreinte(&source));
    // Greffon coché mais éteint (droit perdu) : rien d'effectif ⇒ 0 dB.
    let premium = avec(&[("convolution", true)]);
    let mut x = ctx(0.0, false);
    x.premium = false;
    assert_eq!(resoudre(&premium, &x).headroom_db, 0.0);
}

/// Armer l'automatique en cours de lecture glisse en 20 ms : aucune marche.
#[test]
fn le_headroom_change_sans_marche() {
    let mut h = EtageHeadroom::new(0.0).unwrap();
    h.preparer(CD, 4096).unwrap();
    h.definir_gain_db(-3.0).unwrap();
    let mut f = vec![1.0; 4096 * 2];
    h.traiter_sur_place(&mut f, 4096).unwrap();
    let pas_max = (1.0 - lineaire(-3.0)) / (44_100.0 * 0.020) + 1e-12;
    for w in f.chunks(2).collect::<Vec<_>>().windows(2) {
        assert!((w[0][0] - w[1][0]).abs() <= pas_max);
    }
    assert_eq!(*f.last().unwrap(), lineaire(-3.0));
    assert!(h.definir_gain_db(0.5).is_err(), "l'entrée ne pousse jamais");
    assert!(h.definir_gain_db(f64::NAN).is_err());
    assert!(h.appliquer_reglages(&json!({ "db": -6.0 })).is_ok());
    assert!(h.appliquer_reglages(&json!({})).is_err());
}

// ═══════════════ Pas de double compte (#4594) ═══════════════

#[test]
fn pas_de_double_compte_avec_la_compensation_4594() {
    let auto = |reserve: f64| {
        BilanDeNiveau::calculer(&EntreesNiveau {
            headroom: Headroom::Auto {},
            headroom_resolu_db: -3.0,
            replaygain_db: 0.0,
            gain_de_zone_db: 0.0,
            gain_moyen_greffons_db: reserve,
            compensation_active: true,
        })
    };
    // AutoEq qui réserve 12 dB : l'automatique n'ajoute RIEN (pas −15 dB).
    let b = auto(-12.0);
    assert_eq!(b.headroom_db, 0.0);
    assert_eq!(b.absorbe_par_les_greffons_db, 3.0);
    assert_eq!(b.gain_d_entree_db, 0.0);
    assert_eq!(b.compensation_db, 12.0, "rendue une fois, pas 15 ni 0");
    // Volume à −20 dB : le niveau perçu est celui du volume, sans les −12 dB.
    let g = b.gain_de_rendu(lineaire(-20.0));
    assert!((decibels(g.lineaire) - -8.0).abs() < 1e-9);
    assert_eq!(g.rabote_db, 0.0);
    // Volume plein : raboté à l'unité, et dit (#5069).
    let g = b.gain_de_rendu(1.0);
    assert_eq!(g.lineaire, 1.0);
    assert!((g.rabote_db - 12.0).abs() < 1e-9);
    // Petite réserve : l'automatique complète jusqu'à 3 dB, pas plus.
    let b = auto(-1.0);
    assert!((b.headroom_db - -2.0).abs() < 1e-12);
    assert!((b.compensation_db - 3.0).abs() < 1e-12);
    // Sans réserve : −3 dB, rendus par la compensation.
    let b = auto(0.0);
    assert_eq!((b.headroom_db, b.compensation_db), (-3.0, 3.0));
    // Manuel : choix explicite, appliqué tel quel, compté une fois.
    let b = BilanDeNiveau::calculer(&EntreesNiveau {
        headroom: Headroom::Manual { db: -6.0 },
        headroom_resolu_db: -6.0,
        replaygain_db: 0.0,
        gain_de_zone_db: 0.0,
        gain_moyen_greffons_db: -12.0,
        compensation_active: true,
    });
    assert_eq!((b.headroom_db, b.compensation_db), (-6.0, 18.0));
    // Interrupteur coupé : rien de rendu.
    let mut e = EntreesNiveau {
        headroom: Headroom::Auto {},
        headroom_resolu_db: -3.0,
        replaygain_db: 0.0,
        gain_de_zone_db: 0.0,
        gain_moyen_greffons_db: -12.0,
        compensation_active: false,
    };
    assert_eq!(BilanDeNiveau::calculer(&e).compensation_db, 0.0);
    // ReplayGain et gain de zone : du NIVEAU, jamais compensés. Négatifs à
    // l'entrée, positifs au rendu (sous le rabot, comme aujourd'hui).
    e.compensation_active = true;
    e.replaygain_db = -6.5;
    e.gain_de_zone_db = 1.5;
    let b = BilanDeNiveau::calculer(&e);
    assert_eq!(b.gain_d_entree_db, -5.0);
    assert_eq!(b.compensation_db, 12.0);
    assert_eq!(b.gain_au_rendu_db, 12.0);
    e.replaygain_db = 4.0;
    e.gain_de_zone_db = 0.0;
    let b = BilanDeNiveau::calculer(&e);
    assert_eq!(b.gain_d_entree_db, 0.0);
    assert_eq!(b.gain_au_rendu_db, 16.0);
    // Valeurs illisibles : 0 dB, jamais un gain fou.
    e.replaygain_db = f64::NAN;
    e.gain_moyen_greffons_db = f64::INFINITY;
    let b = BilanDeNiveau::calculer(&e);
    assert!(b.gain_au_rendu_db.is_finite() && b.gain_d_entree_db.is_finite());
}

/// Le même bilan sur le signal : un « égaliseur » qui réserve 12 dB, volume
/// à −20 dB ; la sortie est la source à −20 dB, ni −32 ni −35.
#[test]
fn le_niveau_percu_est_celui_du_volume_sur_le_signal() {
    let chaine = avec(&[("equalizer-graphic", true)]);
    let r = resoudre(&chaine, &ctx(-20.0, false));
    assert_eq!(r.headroom_db, -3.0);
    let b = bilan(&chaine, &r, -12.0);
    let consigne = ConsigneVolume::new(b.gain_de_rendu(lineaire(-20.0)).lineaire);
    let mut banc = Banc::new(
        &b,
        consigne,
        vec![Box::new(GainFixe(lineaire(-12.0)))],
        Box::new(QuantificationMiseEnForme::new()),
    );
    let source = source_24_bits(44_100);
    let sortie = banc.jouer(&source);
    let energie = |m: &[i32]| m.iter().map(|x| f64::from(x >> 8).powi(2)).sum::<f64>();
    let db = 10.0 * (energie(&sortie) / energie(&source)).log10();
    assert!(
        (db - -20.0).abs() < 0.01,
        "niveau {db:.3} dB au lieu de −20"
    );
}

// ═══════════════ Latence du volume (banc simulé, #7264) ═══════════════

/// Banc : un producteur remplit un anneau de DEUX SECONDES (le pire cas,
/// WASAPI exclusif de #7264) ; le rendu tire des périodes de pilote et
/// applique le volume. Le geste arrive à une trame quelconque ; on compte
/// les trames jusqu'au premier échantillon changé et jusqu'à la consigne
/// atteinte (à 0,1 dB). Aucun périphérique, aucun son.
fn mesurer(cadence: u32, periode: usize, geste_a: usize) -> (f64, f64) {
    let format = FormatFlux {
        channels: 1,
        sample_rate_hz: cadence,
    };
    let consigne = ConsigneVolume::new(lineaire(-20.0));
    let mut v = EtageVolume::new(consigne.clone(), PlacementVolume::AtRender);
    v.preparer(format, periode).unwrap();
    let mut anneau: VecDeque<f64> = VecDeque::new();
    let profondeur = cadence as usize * 2;
    anneau.extend(std::iter::repeat_n(1.0, profondeur));
    let cible = lineaire(-6.0);
    let mut rendues = 0usize;
    let (mut premier, mut atteint) = (None, None);
    let mut tampon = vec![0.0; periode];
    let mut pose = false;
    while atteint.is_none() {
        anneau.extend(std::iter::repeat_n(1.0, periode));
        for x in tampon.iter_mut() {
            *x = anneau.pop_front().unwrap();
        }
        // Le geste, posé à la trame `geste_a` par un autre fil, est lu au
        // premier réveil du rendu qui suit.
        if !pose && rendues >= geste_a {
            consigne.poser(cible);
            pose = true;
        }
        v.traiter_sur_place(&mut tampon, periode).unwrap();
        for (i, x) in tampon.iter().enumerate() {
            let t = rendues + i;
            if t < geste_a {
                continue;
            }
            if premier.is_none() && (*x - lineaire(-20.0)).abs() > 1e-12 {
                premier = Some(t);
            }
            if atteint.is_none() && (decibels(*x) - -6.0).abs() < 0.1 {
                atteint = Some(t);
            }
        }
        rendues += periode;
    }
    let ms = |t: usize| (t - geste_a) as f64 * 1000.0 / f64::from(cadence);
    (ms(premier.unwrap()), ms(atteint.unwrap()))
}

#[test]
fn latence_du_volume_mesuree() {
    let mut pire: f64 = 0.0;
    println!("cadence  période      premier effet   consigne atteinte (anneau de 2 s en amont)");
    for cadence in [44_100u32, 48_000, 96_000, 192_000] {
        // Périodes de pilote : 5, 10 et 20 ms.
        for ms in [5usize, 10, 20] {
            let periode = cadence as usize * ms / 1000;
            let (mut pp, mut pa) = (0f64, 0f64);
            for geste in [1usize, periode / 3, periode / 2, periode - 1] {
                let (p, a) = mesurer(cadence, periode, cadence as usize + geste);
                assert!(p <= ms as f64 + 0.1, "premier effet {p} ms > une période");
                assert!(a < 50.0, "consigne atteinte en {a} ms ≥ 50 ms");
                (pp, pa) = (pp.max(p), pa.max(a));
            }
            pire = pire.max(pa);
            println!("{cadence:>7}  {ms:>3} ms  {pp:>12.1} ms  {pa:>14.1} ms (pire geste)");
        }
    }
    println!("pire cas : {pire:.1} ms (visé < 50 ms)");
}

// ═══════════════ Mise en forme du bruit ═══════════════

fn sinus(f: f64, dbfs: f64, n: usize) -> Vec<f64> {
    let a = lineaire(dbfs);
    (0..n)
        .flat_map(|i| {
            let x = a * (2.0 * std::f64::consts::PI * f * i as f64 / 44_100.0).sin();
            [x, x]
        })
        .collect()
}

fn quantifier_16(q: &mut dyn EtapeFinale, x: &[f64]) -> Vec<i32> {
    let mut mots = vec![0i32; x.len()];
    for (e, s) in x.chunks(BLOC * 2).zip(mots.chunks_mut(BLOC * 2)) {
        q.finaliser(
            e,
            e.len() / 2,
            SortieFinale::Pcm {
                echantillons: s,
                bits: 16,
            },
        )
        .unwrap();
    }
    mots
}

#[test]
fn mise_en_forme_stable_a_0_dbfs_et_a_20_khz() {
    for (f, db) in [
        (1_000.0, 0.0),
        (20_000.0, 0.0),
        (20_000.0, -0.1),
        (50.0, -60.0),
    ] {
        let x = sinus(f, db, 44_100);
        let mut q = QuantificationMiseEnForme::new();
        q.preparer(CD, BLOC).unwrap();
        assert_eq!(q.formeur(), "lipshitz5e");
        let mots = quantifier_16(&mut q, &x);
        for (m, v) in mots.iter().zip(&x) {
            let e = f64::from(m >> 16) - v * 32_768.0;
            // Au rail, la saturation ; ailleurs, l'erreur reste bornée par
            // l'erreur réinjectée (±2 LSB × Σ|c_k|) et le dither.
            assert!(e.abs() < 22.0, "{f} Hz à {db} dBFS : erreur {e} LSB");
        }
    }
}

/// La mise en forme baisse le bruit sous 2 kHz par rapport au TPDF plat :
/// puissance de l'erreur sur les raies 0–2 kHz d'une DFT fenêtrée (Hann),
/// moyennée sur huit fenêtres de 4 096 échantillons.
#[test]
fn mise_en_forme_baisse_le_bruit_dans_le_grave() {
    let x = sinus(997.0, -20.0, 44_100);
    let mut q = QuantificationMiseEnForme::new();
    q.preparer(CD, BLOC).unwrap();
    let forme = quantifier_16(&mut q, &x);
    let mut d = Tpdf::depuis_graine(1);
    let plat: Vec<i32> = x
        .iter()
        .map(|v| ((v * 32_768.0 + d.tirer()).round() as i32) << 16)
        .collect();
    let n_dft: usize = 4096;
    let raies = 2_000 * n_dft / 44_100;
    let grave = |mots: &[i32]| {
        let e: Vec<f64> = mots
            .iter()
            .zip(&x)
            .step_by(2)
            .map(|(m, v)| f64::from(m >> 16) - v * 32_768.0)
            .collect();
        let mut p = 0.0;
        for fenetre in e.chunks_exact(n_dft).take(8) {
            for k in 1..=raies {
                let (mut re, mut im) = (0.0, 0.0);
                for (n, v) in fenetre.iter().enumerate() {
                    let w =
                        0.5 - 0.5 * (2.0 * std::f64::consts::PI * n as f64 / n_dft as f64).cos();
                    let a = 2.0 * std::f64::consts::PI * (k * n) as f64 / n_dft as f64;
                    re += w * v * a.cos();
                    im -= w * v * a.sin();
                }
                p += re * re + im * im;
            }
        }
        p
    };
    let gain_db = 10.0 * (grave(&forme) / grave(&plat)).log10();
    println!("bruit sous 2 kHz, mise en forme / TPDF plat : {gain_db:.1} dB");
    assert!(gain_db < -6.0, "{gain_db:.1} dB");
}

#[test]
fn formeur_selon_la_cadence_et_refus_sans_panique() {
    assert_eq!(formeur_pour(48_000), "lipshitz5e");
    assert_eq!(formeur_pour(96_000), "ordre2");
    let mut q = QuantificationMiseEnForme::new();
    let mut mots = [0i32; 4];
    let mut essai = |q: &mut QuantificationMiseEnForme, e: &[f64], trames, bits| {
        q.finaliser(
            e,
            trames,
            SortieFinale::Pcm {
                echantillons: &mut mots,
                bits,
            },
        )
    };
    assert_eq!(
        essai(&mut q, &[0.0; 4], 2, 24),
        Err(ErreurEtage::NonPrepare)
    );
    q.preparer(CD, 2).unwrap();
    assert_eq!(
        essai(&mut q, &[0.0; 4], 2, 20),
        Err(ErreurEtage::ModeNonPrisEnCharge)
    );
    assert_eq!(
        essai(&mut q, &[0.0; 4], 3, 24),
        Err(ErreurEtage::BlocTropGrand { trames: 3, max: 2 })
    );
    assert!(matches!(
        essai(&mut q, &[0.0; 2], 2, 24),
        Err(ErreurEtage::TamponTropPetit { .. })
    ));
    let mut octets = [0u8; 4];
    assert_eq!(
        q.finaliser(
            &[0.0; 4],
            2,
            SortieFinale::Dsd {
                octets: &mut octets,
                taux: TauxDsd::Dsd64
            }
        ),
        Err(ErreurEtage::ModeNonPrisEnCharge)
    );
    let mut v = EtageVolume::new(ConsigneVolume::new(0.5), PlacementVolume::AtRender);
    assert_eq!(
        v.traiter_sur_place(&mut [0.0; 4], 2),
        Err(ErreurEtage::NonPrepare)
    );
}

// ═══════════════ Volume : silence, bornes, contournement ═══════════════

#[test]
fn volume_nul_rend_des_zeros_exacts_et_jamais_plus_fort_que_l_unite() {
    let consigne = ConsigneVolume::new(0.0);
    let mut v = EtageVolume::new(consigne.clone(), PlacementVolume::AtRender);
    v.preparer(CD, BLOC).unwrap();
    let mut f = vec![0.5; BLOC * 2];
    v.traiter_sur_place(&mut f, BLOC).unwrap();
    assert!(f.iter().all(|x| *x == 0.0));
    let mut q = QuantificationMiseEnForme::new();
    q.preparer(CD, BLOC).unwrap();
    let mut mots = vec![7i32; BLOC * 2];
    q.finaliser(
        &f,
        BLOC,
        SortieFinale::Pcm {
            echantillons: &mut mots,
            bits: 24,
        },
    )
    .unwrap();
    assert!(mots.iter().all(|m| *m == 0), "pas de dither sur le silence");
    consigne.poser(4.0);
    assert_eq!(consigne.lire(), 1.0);
    consigne.poser(f64::NAN);
    assert_eq!(consigne.lire(), 1.0);
    // Contourné (volume de l'appareil) : l'entrée sort telle quelle.
    consigne.poser(0.25);
    v.definir_bypass(true);
    let mut f = vec![0.5; BLOC * 2];
    v.traiter_sur_place(&mut f, BLOC).unwrap();
    assert!(f.iter().all(|x| *x == 0.5));
}

// ═══════════════ Étape finale choisie ; exception DSD ═══════════════

#[test]
fn etape_finale_selon_le_reglage_et_exception_dsd() {
    let mut c = avec(&[("crossfeed", true)]);
    let x = ctx(-10.0, false);
    for (d, attendu) in [
        (Dither::Tpdf, (true, false)),
        (Dither::NoiseShaped, (true, true)),
        (Dither::Off, (false, false)),
        (Dither::Unknown, (true, false)),
    ] {
        c.fixed.quantization.dither = d;
        let r = resoudre(&c, &x);
        assert_eq!(
            choisir_etape_finale(&c, &r),
            EtapeFinaleChoisie::Pcm {
                dither: attendu.0,
                mise_en_forme: attendu.1
            }
        );
        assert_eq!(placement_du_volume(&r), Some(PlacementVolume::AtRender));
    }
    c.fixed.upsampling = Upsampling::Dsd {
        rate: TauxDsd::Dsd128,
        filter: "polyvalent".into(),
    };
    let r = resoudre(&c, &x);
    assert_eq!(
        choisir_etape_finale(&c, &r),
        EtapeFinaleChoisie::ModulateurDsd {
            taux: TauxDsd::Dsd128
        }
    );
    assert_eq!(
        placement_du_volume(&r),
        Some(PlacementVolume::BeforeDsdModulator)
    );
    // Sans Premium, l'upsampling DSD s'éteint : retour au PCM.
    let mut x = ctx(-10.0, false);
    x.premium = false;
    let r = resoudre(&c, &x);
    assert!(matches!(
        choisir_etape_finale(&c, &r),
        EtapeFinaleChoisie::Pcm { .. }
    ));
}

// ═══════════════ Pure : comportement réel du volume ═══════════════

/// Vérifié dans le code réel (`audio/audiophile.rs`, `effective_volume`) :
/// en Pure, le volume logiciel RESTE appliqué, sauf si le verrou
/// `audiophile_lock_volume` (inactif par défaut, décision de Bertrand) le
/// tient à 100 % — la consigne arrive alors à 0 dB. La résolution le dit
/// désormais : volume effectif, quantification effective, pas de badge
/// « Bit-perfect » qui mentirait.
#[test]
fn pure_le_volume_logiciel_reste_applique_sauf_verrou() {
    let r = resoudre(&ChaineTess::default(), &ctx(-18.0, true));
    let vol = r
        .etapes
        .iter()
        .find(|e| e.code == CodeEtape::Volume)
        .unwrap();
    assert!(vol.effective, "{vol:?}");
    assert_eq!(r.volume.placement, Some(PlacementVolume::AtRender));
    assert_eq!(r.badge, Badge::ProcessedByTess);
    assert_eq!(r.headroom_db, 0.0);
    // Verrou armé : la consigne vaut 0 dB, la zone est bit-perfect.
    let r = resoudre(&ChaineTess::default(), &ctx(0.0, true));
    assert_eq!(r.badge, Badge::BitPerfect);
}
