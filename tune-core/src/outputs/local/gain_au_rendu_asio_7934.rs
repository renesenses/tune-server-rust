//! #7934 — témoins du volume ASIO natif appliqué au RENDU.
//!
//! Le défaut : sous Windows, sortie locale en ASIO natif, un geste sur le
//! volume ne s'entendait que deux à trois secondes plus tard. L'étage natif
//! du bras ASIO (`bras_asio.rs`) pondérait les mots par le volume de la zone
//! dans le producteur, avant l'anneau natif de deux secondes
//! (`asio_exclusive.rs`, `ring_cap = cadence × voies × 2`), et les rappels
//! natifs I32, I24 et I16 ne faisaient que recopier l'anneau. C'est le défaut
//! que #7264 (PR #7498) a corrigé pour le WASAPI exclusif ; ASIO était resté
//! hors de son périmètre.
//!
//! Le remède est le même : l'étage est monté à l'unité et le rappel pondère
//! les mots qu'il tire de l'anneau ([`NativePcmRing::pop_mapped_au_gain`]),
//! avec la consigne relue à chaque rappel.
//!
//! Aucun son : un anneau en mémoire et les conversions des trois rappels.
//! Les rappels et le bras ne compilent que sous Windows avec `asio` : leur
//! branchement est jugé sur leur texte.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::NativePcmRing;
use super::chaine_par_la_boucle::ContratDuSignal;
use super::gain_au_rendu_7264::GainAuRendu;

const CADENCE: u32 = 44_100;
const VOIES: u32 = 2;
/// Une période de pilote ASIO courante : 512 trames.
const PERIODE: usize = 512;
/// Un mot 24 bits constant, aligné à gauche : `0x40_0000` (−6 dBFS).
const MOT_24: i32 = 0x40_0000 << 8;

/// Les trois rappels natifs d'`asio_exclusive.rs`, réduits à leur
/// conversion : le mot rendu, ramené à sa valeur utile 24 bits.
#[derive(Clone, Copy, Debug)]
enum Rappel {
    I32,
    I24,
    I16,
}

impl Rappel {
    /// Rend une période comme le rappel la rend, et la relit en valeurs
    /// utiles 24 bits (I16 : décalé de 8 pour comparer à la même échelle).
    fn rendre(self, anneau: &NativePcmRing, gain: &mut GainAuRendu) -> Vec<i32> {
        let n = PERIODE * VOIES as usize;
        match self {
            Rappel::I32 => {
                let mut out = vec![0i32; n];
                anneau.pop_mapped_au_gain(&mut out, 24, gain, |m| m);
                out.into_iter().map(|m| m >> 8).collect()
            }
            Rappel::I24 => {
                let mut out = vec![0i32; n];
                anneau.pop_mapped_au_gain(&mut out, 24, gain, |m| m >> 8);
                out
            }
            Rappel::I16 => {
                let mut out = vec![0i16; n];
                anneau.pop_mapped_au_gain(&mut out, 16, gain, |m| (m >> 16) as i16);
                out.into_iter().map(|m| i32::from(m) << 8).collect()
            }
        }
    }
}

fn anneau_de_deux_secondes() -> NativePcmRing {
    // La contenance qu'`asio_exclusive.rs` donne à l'anneau natif.
    NativePcmRing::new((CADENCE * VOIES * 2) as usize)
}

fn remplir(anneau: &NativePcmRing, trames: usize) {
    let mots = vec![MOT_24; trames * VOIES as usize];
    assert_eq!(anneau.push(&mots), mots.len(), "l'anneau doit tout prendre");
}

/// LE contrat de #7934 : sur les trois rappels natifs ASIO, un geste sur le
/// volume s'entend en 50 ms au plus, que l'anneau soit presque vide ou plein
/// de ses deux secondes.
#[test]
fn un_geste_sur_le_volume_asio_s_entend_en_moins_de_50_ms_quelle_que_soit_la_profondeur_de_l_anneau()
 {
    for rappel in [Rappel::I32, Rappel::I24, Rappel::I16] {
        for trames_en_attente in [CADENCE as usize / 10, (CADENCE * 2) as usize] {
            let anneau = anneau_de_deux_secondes();
            remplir(&anneau, trames_en_attente);
            let volume = Arc::new(AtomicU32::new(1000));
            let mut gain = GainAuRendu::nouveau(volume.clone(), CADENCE, VOIES);

            let avant = rappel.rendre(&anneau, &mut gain);
            assert!(
                avant.iter().all(|&m| m == 0x40_0000),
                "{rappel:?} : à 100 %, le mot sort intact"
            );

            // Le geste : 100 % → 50 %.
            volume.store(500, Ordering::Relaxed);
            let borne_trames = (CADENCE as usize * 50) / 1000;
            let mut rendues = 0usize;
            let mut atteint_a = None;
            while rendues < borne_trames + PERIODE {
                let periode = rappel.rendre(&anneau, &mut gain);
                for (trame, voies) in periode
                    .as_chunks::<{ VOIES as usize }>()
                    .0
                    .iter()
                    .enumerate()
                {
                    if atteint_a.is_none() && voies.iter().all(|&m| m == 0x20_0000) {
                        atteint_a = Some(rendues + trame);
                    }
                }
                rendues += PERIODE;
            }
            let atteint_a = atteint_a.unwrap_or_else(|| {
                panic!(
                    "#7934 {rappel:?}, anneau de {trames_en_attente} trames : le volume à 50 % \
                     n'est pas entendu en 50 ms — le rendu ASIO ne relit pas la consigne"
                )
            });
            assert!(
                atteint_a <= borne_trames,
                "#7934 {rappel:?} : 50 % atteint après {atteint_a} trames (> {borne_trames})"
            );
        }
    }
}

/// Un passage marqué intact (DoP) n'est jamais pondéré par le rendu ASIO,
/// même à un volume de zone bas : un porteur DoP ne supporte aucun facteur.
#[test]
fn le_rendu_asio_ne_pondere_jamais_un_passage_intact() {
    let anneau = anneau_de_deux_secondes();
    anneau.marquer_passage_intact(true);
    remplir(&anneau, PERIODE);
    anneau.marquer_passage_intact(false);
    remplir(&anneau, PERIODE);
    let volume = Arc::new(AtomicU32::new(250));
    let mut gain = GainAuRendu::nouveau(volume, CADENCE, VOIES);

    let intact = Rappel::I32.rendre(&anneau, &mut gain);
    assert!(
        intact.iter().all(|&m| m == 0x40_0000),
        "le passage DoP doit sortir au bit près"
    );
    let pcm = Rappel::I32.rendre(&anneau, &mut gain);
    assert!(
        pcm.iter().all(|&m| m == 0x10_0000),
        "le PCM qui suit sort directement à 25 %, sans rampe depuis l'unité"
    );
}

/// L'étage monté à l'unité ne voit plus le volume : le verdict publié du
/// chemin du signal ASIO tient compte du volume de la zone, sinon une zone à
/// 50 % se dirait bit-perfect alors que le rappel pondère.
#[test]
fn le_contrat_asio_n_est_bit_perfect_qu_a_100_pour_cent_ou_en_dop() {
    let signal_path_status = std::sync::Mutex::new(None);
    let dop_active = AtomicBool::new(false);
    let volume = AtomicU32::new(500);
    let user_volume = AtomicU32::new(500);
    let rg_factor = AtomicU32::new(1000);
    let eq = std::sync::Mutex::new(None);
    let convolver = std::sync::Mutex::new(None);
    let crossfeed = std::sync::Mutex::new(None);
    let pure_bypass = AtomicBool::new(false);
    let mono_downmix = AtomicBool::new(false);
    let mut contrat = ContratDuSignal {
        signal_path_status: &signal_path_status,
        transport_natif: true,
        dop_active: &dop_active,
        volume: &volume,
        user_volume: &user_volume,
        rg_factor: &rg_factor,
        eq: &eq,
        convolver: &convolver,
        crossfeed: &crossfeed,
        pure_bypass: &pure_bypass,
        mono_downmix: &mono_downmix,
        bit_perfect_state: None,
    };
    let publie = || {
        signal_path_status
            .lock()
            .unwrap()
            .as_ref()
            .expect("un contrat publié")
            .bit_perfect
    };

    // L'étage à l'unité rend des mots intacts : c'est le volume qui décide.
    contrat.publier(false, true);
    assert!(
        !publie(),
        "#7934 : une zone ASIO à 50 % ne doit pas se dire bit-perfect"
    );

    volume.store(1000, Ordering::SeqCst);
    contrat.publier(false, true);
    assert!(publie(), "à 100 % : bit-perfect");

    volume.store(500, Ordering::SeqCst);
    contrat.publier(true, true);
    assert!(
        publie(),
        "un porteur DoP n'est jamais pondéré : bit-perfect"
    );
}

/// Le branchement, tenu pour que le défaut ne revienne pas. Le bras ASIO
/// monte son étage natif à l'UNITÉ et le confie au gain au rendu ; les trois
/// rappels natifs passent par `pop_mapped_au_gain`, aucun ne recopie plus
/// l'anneau sans gain. Les commentaires sont retirés : une garde ne doit pas
/// être satisfaite par la phrase qui la décrit.
#[test]
fn le_bras_asio_monte_l_etage_a_l_unite_et_les_rappels_rendent_le_volume() {
    let bras = sans_commentaires(include_str!("bras_asio.rs"));
    assert!(
        bras.contains("let volume_de_l_etage = AtomicU32::new(VOLUME_A_L_UNITE);"),
        "#7934 : l'étage ASIO doit recevoir l'unité, sinon le volume attend l'anneau"
    );
    assert!(
        bras.contains("&volume_de_l_etage,"),
        "#7934 : l'étage natif ASIO est monté sur le volume de la zone"
    );
    assert!(
        bras.contains(".avec_le_gain_au_rendu(anneau_natif)"),
        "#7934 : l'étage ASIO ne marque plus ses passages intacts dans l'anneau"
    );
    assert!(bras.contains("let anneau_natif = backend.sortie.native_ring().clone();"));

    let rappels = sans_commentaires(
        include_str!("../asio_exclusive.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap_or_default(),
    );
    assert_eq!(
        rappels.matches("native_ring.pop_mapped_au_gain(").count(),
        3,
        "#7934 : les rappels natifs I32, I24 et I16 passent par le gain au rendu"
    );
    assert!(
        !rappels.contains("native_ring.pop(") && !rappels.contains("native_ring.pop_mapped("),
        "#7934 : un rappel natif ASIO recopie encore l'anneau sans gain"
    );
    assert!(rappels.contains("GainAuRendu::nouveau(volume.clone(), rendu.cadence, rendu.voies)"));
}

fn sans_commentaires(source: &str) -> String {
    source
        .lines()
        .filter(|ligne| !ligne.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}
