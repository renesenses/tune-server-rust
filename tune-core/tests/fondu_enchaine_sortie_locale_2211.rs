//! #2211 — **le fondu enchaîné raccordé à la sortie locale.**
//!
//! Le moteur `FonduEnchaine` existait, éprouvé par cinq témoins, mais rien ne
//! l'appelait en production. Il est maintenant porté par un
//! [`PuitsDeFondu`] que la boucle gapless de la sortie locale donne à son
//! étage : la piste qui se termine garde ses N dernières trames en réserve,
//! et à la frontière la réserve est soit superposée à la piste suivante, soit
//! rendue intacte.
//!
//! Ce fichier éprouve, sans périphérique ni base :
//!
//! * **le banc de l'enveloppe** : sur des échantillons connus (sortante à 1,
//!   entrante à 0 puis l'inverse), chaque trame du recouvrement vaut
//!   exactement le gain attendu, aux deux bornes et au milieu ;
//! * **le fondu appliqué** : deux pistes se superposent, le compte des
//!   échantillons est conservé, la durée du recouvrement est celle demandée ;
//! * **le gapless respecté** : quand la frontière renonce, le flux livré est
//!   la concaténation exacte des deux pistes, au bit près ;
//! * **la règle** : PURE, bit-perfect strict, DoP, même album — chacun
//!   renonce, avec sa contre-épreuve ;
//! * **la position** : la durée retenue est publiée, puis rendue à zéro.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tune_core::audio::fondu_enchaine::{
    ConsigneDeJonction, CourbeDeFondu, Jonction, MotifSansFondu, PuitsDeFondu, decider_le_fondu,
};
use tune_core::outputs::traits::{FormatOuvert, PuitsDEchantillons};

const CADENCE: u32 = 1_000;
const CANAUX: u16 = 2;

/// Un puits qui retient tout, partagé pour être relu après coup.
#[derive(Clone, Default)]
struct Enregistreur(Arc<Mutex<Vec<f32>>>);

impl Enregistreur {
    fn mots(&self) -> Vec<f32> {
        self.0.lock().unwrap().clone()
    }
}

impl PuitsDEchantillons for Enregistreur {
    fn ecrire(&mut self, mots: &[f32]) -> bool {
        self.0.lock().unwrap().extend_from_slice(mots);
        true
    }
}

struct Banc {
    puits: PuitsDeFondu<'static>,
    capture: Enregistreur,
    duree_ms: Arc<AtomicU32>,
    retenue_ms: Arc<AtomicU64>,
    actif: Arc<AtomicBool>,
}

fn banc(duree_ms: u32, courbe: CourbeDeFondu) -> Banc {
    let capture = Enregistreur::default();
    let duree = Arc::new(AtomicU32::new(duree_ms));
    let retenue = Arc::new(AtomicU64::new(0));
    let actif = Arc::new(AtomicBool::new(false));
    let puits = PuitsDeFondu::nouveau(
        Box::new(capture.clone()),
        FormatOuvert::new(CADENCE, CANAUX),
        courbe,
        duree.clone(),
        retenue.clone(),
        actif.clone(),
    );
    Banc {
        puits,
        capture,
        duree_ms: duree,
        retenue_ms: retenue,
        actif,
    }
}

/// `trames` trames stéréo à la valeur `v`, poussées par blocs de 37 trames
/// (un pas qui ne divise rien : la frontière tombe au milieu d'un bloc).
fn pousser(puits: &mut PuitsDeFondu<'_>, trames: usize, v: impl Fn(usize) -> f32) {
    let mots: Vec<f32> = (0..trames)
        .flat_map(|i| std::iter::repeat_n(v(i), CANAUX as usize))
        .collect();
    for bloc in mots.chunks(37 * CANAUX as usize) {
        assert!(puits.ecrire(bloc));
    }
}

fn jonction_permise() -> Jonction {
    Jonction {
        fondu_arme: true,
        pure: false,
        bitperfect_strict: false,
        dop: false,
        reserve_vide: false,
        consigne: ConsigneDeJonction::Permise,
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Le banc de l'enveloppe
// ───────────────────────────────────────────────────────────────────────────

/// Sortante à 1, entrante à 0 : chaque trame du recouvrement EST le gain
/// sortant. Puis l'inverse : chaque trame EST le gain entrant. Les deux
/// courbes, toutes les trames, tolérance d'un f32.
#[test]
fn banc_l_enveloppe_du_fondu_trame_par_trame() {
    const N: usize = 250; // 250 ms à 1 kHz
    for courbe in [CourbeDeFondu::Lineaire, CourbeDeFondu::PuissanceConstante] {
        for (s, e) in [(1.0f32, 0.0f32), (0.0, 1.0)] {
            let mut b = banc(N as u32, courbe);
            pousser(&mut b.puits, 1_000, |_| s);
            assert!(b.puits.commencer_le_fondu());
            pousser(&mut b.puits, 1_000, |_| e);
            assert!(b.puits.terminer());

            let mots = b.capture.mots();
            assert_eq!(mots.len(), (1_000 + 1_000 - N) * CANAUX as usize);
            let debut = (1_000 - N) * CANAUX as usize;
            for i in 0..N {
                let t = i as f32 / (N - 1) as f32;
                let (gs, ge) = courbe.gains(t);
                let attendu = s * gs + e * ge;
                for c in 0..CANAUX as usize {
                    let lu = mots[debut + i * CANAUX as usize + c];
                    assert!(
                        (lu - attendu).abs() < 1e-6,
                        "{courbe:?} s={s} e={e} trame {i} voie {c} : {lu} au lieu de {attendu}"
                    );
                }
            }
            // Aux bornes, exactement la sortante puis exactement l'entrante.
            assert_eq!(mots[debut], s);
            assert_eq!(mots[debut + (N - 1) * CANAUX as usize], e);
            // Hors du recouvrement, les mots sont intacts.
            assert!(mots[..debut].iter().all(|&m| m == s));
            assert!(mots[debut + N * CANAUX as usize..].iter().all(|&m| m == e));
        }
    }
}

/// La puissance constante tient la puissance sur deux sources décorrélées :
/// au milieu, `gs² + ge² = 1` — là où le linéaire creuse de 3 dB.
#[test]
fn banc_la_puissance_constante_ne_creuse_pas_le_milieu() {
    let (s, e) = CourbeDeFondu::PuissanceConstante.gains(0.5);
    assert!((s * s + e * e - 1.0).abs() < 1e-6);
    let (s, e) = CourbeDeFondu::Lineaire.gains(0.5);
    assert!(
        (s * s + e * e - 0.5).abs() < 1e-6,
        "contre-épreuve : le linéaire creuse"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Fondu appliqué, gapless respecté
// ───────────────────────────────────────────────────────────────────────────

/// Deux pistes différentes (rampes distinctes) : le recouvrement fait la
/// durée demandée, les deux sources sont dans le même mot, aucun
/// échantillon n'est perdu ni dupliqué.
#[test]
fn fondu_applique_les_deux_pistes_se_superposent() {
    let mut b = banc(100, CourbeDeFondu::PuissanceConstante);
    pousser(&mut b.puits, 600, |i| 0.5 + i as f32 * 1e-4);
    assert!(b.puits.fondu_arme());
    assert_eq!(b.retenue_ms.load(Ordering::Relaxed), 100, "100 ms retenues");

    assert_eq!(decider_le_fondu(jonction_permise()), Ok(()));
    assert!(b.puits.commencer_le_fondu());
    assert!(
        b.actif.load(Ordering::Relaxed),
        "le recouvrement est déclaré"
    );
    assert_eq!(b.retenue_ms.load(Ordering::Relaxed), 0);

    pousser(&mut b.puits, 400, |i| -0.25 - i as f32 * 1e-4);
    assert!(!b.actif.load(Ordering::Relaxed), "le recouvrement est fini");
    assert!(b.puits.fondu_arme(), "l'entrante est la sortante suivante");
    assert!(b.puits.terminer());

    let mots = b.capture.mots();
    assert_eq!(mots.len(), (600 + 400 - 100) * CANAUX as usize);
    // Au milieu du recouvrement, le mot n'est ni la sortante ni l'entrante.
    let milieu = (500 + 50) * CANAUX as usize;
    let sortante = 0.5 + 550.0 * 1e-4;
    let entrante = -0.25 - 50.0 * 1e-4;
    let (gs, ge) = CourbeDeFondu::PuissanceConstante.gains(50.0 / 99.0);
    assert!((mots[milieu] - (sortante * gs + entrante * ge)).abs() < 1e-5);
    assert!((mots[milieu] - sortante).abs() > 0.1 && (mots[milieu] - entrante).abs() > 0.1);
}

/// La frontière renonce (même album, PURE…) : le flux livré est la
/// concaténation EXACTE des deux pistes — l'enchaînement gapless au bit près,
/// seulement retardé de la réserve.
#[test]
fn gapless_respecte_la_reserve_part_intacte() {
    let a = |i: usize| ((i * 7919) % 1000) as f32 / 1000.0 - 0.5;
    let bb = |i: usize| ((i * 104_729) % 997) as f32 / 997.0 - 0.5;

    let mut b = banc(120, CourbeDeFondu::PuissanceConstante);
    pousser(&mut b.puits, 500, a);
    assert!(b.puits.renoncer_au_fondu());
    pousser(&mut b.puits, 300, bb);
    assert!(b.puits.terminer());

    let attendu: Vec<f32> = (0..500)
        .map(a)
        .chain((0..300).map(bb))
        .flat_map(|v| std::iter::repeat_n(v, CANAUX as usize))
        .collect();
    let lu = b.capture.mots();
    assert_eq!(lu.len(), attendu.len());
    assert!(
        lu.iter()
            .zip(&attendu)
            .all(|(x, y)| x.to_bits() == y.to_bits()),
        "le renoncement doit rendre le flux gapless au bit près"
    );
    assert!(!b.actif.load(Ordering::Relaxed));
}

/// Durée nulle : le puits est transparent, il ne retient rien.
#[test]
fn sans_duree_le_puits_est_transparent() {
    let mut b = banc(0, CourbeDeFondu::PuissanceConstante);
    assert!(!b.puits.fondu_arme());
    pousser(&mut b.puits, 50, |i| i as f32);
    assert_eq!(
        b.capture.mots().len(),
        50 * CANAUX as usize,
        "rien n'est retenu"
    );
    assert_eq!(b.retenue_ms.load(Ordering::Relaxed), 0);
    // Contre-épreuve : avec une durée, les 20 dernières trames sont retenues.
    let mut b = banc(20, CourbeDeFondu::PuissanceConstante);
    pousser(&mut b.puits, 50, |i| i as f32);
    assert_eq!(b.capture.mots().len(), 30 * CANAUX as usize);
}

/// Un réglage changé en cours de lecture vaut à la frontière suivante : la
/// réserve se réarme avec la NOUVELLE durée.
#[test]
fn la_duree_se_relit_a_chaque_frontiere() {
    let mut b = banc(0, CourbeDeFondu::PuissanceConstante);
    pousser(&mut b.puits, 50, |_| 0.1);
    b.duree_ms.store(30, Ordering::Relaxed);
    assert!(!b.puits.fondu_arme(), "jamais armé au milieu d'une piste");
    assert!(b.puits.renoncer_au_fondu());
    assert!(b.puits.fondu_arme(), "armé pour la piste qui commence");
    pousser(&mut b.puits, 50, |_| 0.2);
    assert_eq!(b.retenue_ms.load(Ordering::Relaxed), 30);
}

/// Une entrante plus courte que le fondu : à la frontière suivante, la
/// sortante finit son extinction au lieu d'être coupée, et rien ne fond.
#[test]
fn une_entrante_plus_courte_que_le_fondu_ne_perd_rien() {
    let mut b = banc(100, CourbeDeFondu::Lineaire);
    pousser(&mut b.puits, 300, |_| 1.0);
    assert!(b.puits.commencer_le_fondu());
    pousser(&mut b.puits, 40, |_| 0.0);
    assert!(b.puits.recouvrement_en_cours());
    assert!(!b.puits.fondu_arme(), "pas de seconde frontière fondue");
    assert!(b.puits.renoncer_au_fondu());
    assert!(b.puits.terminer());
    // 300 + 40 − 40 superposées : toute la réserve est sortie.
    assert_eq!(b.capture.mots().len(), 300 * CANAUX as usize);
    let dernier = *b.capture.mots().last().unwrap();
    assert!(
        dernier.abs() < 1e-6,
        "l'extinction va jusqu'à zéro : {dernier}"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// La règle de frontière, chaque motif avec sa contre-épreuve
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn regle_pure_et_bit_perfect_strict_ne_fondent_jamais() {
    assert_eq!(
        decider_le_fondu(jonction_permise()),
        Ok(()),
        "contre-épreuve"
    );
    let pure = Jonction {
        pure: true,
        ..jonction_permise()
    };
    assert_eq!(decider_le_fondu(pure), Err(MotifSansFondu::Pure));
    let strict = Jonction {
        bitperfect_strict: true,
        ..jonction_permise()
    };
    assert_eq!(
        decider_le_fondu(strict),
        Err(MotifSansFondu::BitPerfectStrict)
    );
}

#[test]
fn regle_un_porteur_dop_ne_fond_jamais() {
    let dop = Jonction {
        dop: true,
        ..jonction_permise()
    };
    assert_eq!(decider_le_fondu(dop), Err(MotifSansFondu::Dop));
}

/// Décision du 07/10 : deux pistes d'un même album, live ou non, ne fondent
/// jamais. Contre-épreuve : la même jonction entre deux albums fond.
#[test]
fn regle_un_meme_album_ne_fond_jamais() {
    let meme_album = Jonction {
        consigne: ConsigneDeJonction::Interdite(MotifSansFondu::MemeAlbum),
        ..jonction_permise()
    };
    assert_eq!(decider_le_fondu(meme_album), Err(MotifSansFondu::MemeAlbum));
    assert_eq!(decider_le_fondu(jonction_permise()), Ok(()));
}

#[test]
fn regle_le_doute_rend_le_gapless() {
    for (j, motif) in [
        (
            Jonction {
                fondu_arme: false,
                ..jonction_permise()
            },
            MotifSansFondu::Desactive,
        ),
        (
            Jonction {
                consigne: ConsigneDeJonction::Inconnue,
                ..jonction_permise()
            },
            MotifSansFondu::ConsigneAbsente,
        ),
        (
            Jonction {
                reserve_vide: true,
                ..jonction_permise()
            },
            MotifSansFondu::ReserveVide,
        ),
    ] {
        assert_eq!(decider_le_fondu(j), Err(motif));
    }
}
