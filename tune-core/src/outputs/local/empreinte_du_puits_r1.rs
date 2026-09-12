//! R1 (#2219) — **la preuve d'identité du rendu de la frontière producteur →
//! puits.**
//!
//! La réorganisation a sorti de `play_url` la suite de gestes qui transforme
//! des octets en mots de sortie : décoder, refuser un porteur DoP, adapter les
//! canaux, rééchantillonner, écrire. Elle y existait en QUATRE copies ; il n'en
//! reste qu'une, derrière [`EtageDeConversion::pousser`].
//!
//! Ces témoins ne vérifient pas que le code « a l'air pareil ». Ils branchent
//! un puits qui **hache tout ce qu'il reçoit**, bit à bit, et comparent
//! l'empreinte à une constante **mesurée sur la version d'AVANT** — la chaîne
//! en ligne de `play_url` au commit `5318d073`, jouée sur les mêmes octets.
//! Une empreinte qui bouge, c'est un octet qui a changé de route vers le DAC.
//!
//! Les constantes ne sont donc pas des valeurs « attendues » qu'on ajusterait
//! si le test rougissait : ce sont des RELEVÉS. Si l'une d'elles ne tombe plus,
//! le rendu a changé, et c'est le changement qui doit se justifier.
//!
//! **Une seule chose peut légitimement les faire bouger** : un changement de
//! rendu VOULU et MESURÉ ailleurs. C'est arrivé une fois, le 13/09/2026, avec
//! le correctif D1 de #2218 (le rééchantillonneur rendait 20 kHz 10 dB trop
//! bas aux sources 44,1 kHz) ; les deux empreintes qui rééchantillonnent ont
//! été remesurées, les deux autres n'ont pas bougé d'un bit. Voir le bloc de
//! constantes en fin de fichier.

use std::sync::atomic::{AtomicBool, AtomicU32};

use super::{Etage, EtageDeConversion, LocalPcmKind, LocalPcmProcessor, PousseeVersLePuits};
use crate::outputs::traits::{AudioSpec, CaptureOutput, FormatOuvert};

/// Le puits qui n'écrit nulle part et **hache** ce qu'il reçoit.
///
/// C'était `PuitsEmpreinte`, écrit ici même par R1. T8 (#2218) l'a promu dans
/// `tune-output-api` sous le nom [`CaptureOutput`], à l'octet près — même
/// FNV-1a, même décalage de base, même ordre — parce que c'est LE puits de
/// capture, et qu'il n'y a aucune raison d'en tenir deux.
///
/// Ce que la substitution démontre, et que rien d'autre ne pouvait démontrer :
/// les **quatre relevés ci-dessous, mesurés sur la chaîne en ligne d'avant la
/// réorganisation (`5318d073`), tombent sur le puits de capture sans être
/// retouchés d'un chiffre**. Le producteur n'a toujours aucune idée de ce
/// qu'il alimente, et le puits de #2218 reçoit bien les octets de `play_url`.
///
/// `neuf()` ouvre au format identité (44,1 kHz stéréo) : R1 ne mesure que des
/// empreintes, et le format ouvert n'entre dans aucun de ses relevés. Les
/// témoins qui en dépendent vivent dans `capture_bout_en_bout_2218.rs`.
fn puits_empreinte() -> CaptureOutput {
    CaptureOutput::ouvert(FormatOuvert::new(44_100, 2))
}

/// Le DSP au repos : aucun égaliseur, aucun convolveur, aucun crossfeed,
/// aucun repli mono. C'est l'état d'une zone qui ne fait que lire — celui où
/// « pas un octet de différence » est vérifiable à l'octet près.
pub(super) struct DspAuRepos {
    eq: std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: AtomicBool,
    mono_downmix: AtomicBool,
    dop_active: AtomicBool,
    volume: AtomicU32,
    user_volume: AtomicU32,
    rg_factor: AtomicU32,
}

impl DspAuRepos {
    pub(super) fn neuf() -> Self {
        Self {
            eq: std::sync::Mutex::new(None),
            convolver: std::sync::Mutex::new(None),
            crossfeed: std::sync::Mutex::new(None),
            pure_bypass: AtomicBool::new(false),
            mono_downmix: AtomicBool::new(false),
            dop_active: AtomicBool::new(false),
            volume: AtomicU32::new(100),
            user_volume: AtomicU32::new(100),
            rg_factor: AtomicU32::new(100),
        }
    }

    fn processeur(&self) -> LocalPcmProcessor<'_> {
        LocalPcmProcessor {
            eq: &self.eq,
            convolver: &self.convolver,
            crossfeed: &self.crossfeed,
            pure_bypass: &self.pure_bypass,
            mono_downmix: &self.mono_downmix,
            dop_active: &self.dop_active,
            volume: &self.volume,
            user_volume: &self.user_volume,
            rg_factor: &self.rg_factor,
        }
    }
}

/// Un étage monté comme `play_url` le monte, sur un format source donné.
///
/// `pub(super)` depuis T8 (#2218) : `capture_bout_en_bout_2218.rs` monte la
/// même chaîne, et deux constructeurs d'étage dans le même module seraient
/// deux endroits où l'ordre des conversions pourrait diverger.
///
/// R5 (#2219) n'a touché que le CORPS : la signature — les sept mêmes
/// arguments, dans le même ordre — est inchangée, et aucun des témoins de ce
/// fichier ni de `capture_bout_en_bout_2218.rs` n'a bougé d'un caractère. Les
/// quatre relevés tombent donc sur la chaîne typée sans être retouchés, ce qui
/// est la seule preuve qui vaille que le rendu n'a pas changé.
///
/// Les octets par trame ne sont plus calculés ici : [`AudioSpec`] les déduit de
/// la profondeur et des canaux. Cette fonction en tenait sa propre copie —
/// `bytes_per_sample`, le même `if bit_depth == 0 { 4 }` qu'ailleurs — et
/// c'était un troisième endroit où la même conséquence pouvait diverger.
pub(super) fn etage<'a>(
    dsp: &'a DspAuRepos,
    octets: Vec<u8>,
    sample_rate: u32,
    channels: u16,
    bit_depth: u16,
    output_sr: u32,
    output_ch: u16,
) -> EtageDeConversion<'a> {
    EtageDeConversion {
        pcm: dsp.processeur(),
        en_attente: octets,
        resampler: None,
        resample_leftover: Vec::new(),
        pcm_kind: LocalPcmKind::for_bit_depth(bit_depth),
        spec: AudioSpec::depuis_entete(sample_rate, bit_depth, channels)
            .expect("format source hors du jeu fermé (0, 16, 24, 32 bits) ou sans canal"),
        sortie: FormatOuvert::new(output_sr, output_ch),
        needs_resample: output_sr != sample_rate,
    }
}

/// Une rampe de mots 16 bits déterministe, pleine échelle et changeant de
/// signe : le pire cas pour une conversion qui se tromperait d'octet.
fn pcm16(trames: usize, channels: u16) -> Vec<u8> {
    let mut octets = Vec::with_capacity(trames * channels as usize * 2);
    let mut graine: i32 = 1;
    for _ in 0..trames * channels as usize {
        graine = (graine.wrapping_mul(1_103_515_245).wrapping_add(12_345)) & 0x7fff_ffff;
        let mot = ((graine >> 8) as i16).wrapping_sub(i16::MAX / 2);
        octets.extend_from_slice(&mot.to_le_bytes());
    }
    octets
}

/// Pousse tout ce qui est poussable et rend l'empreinte du puits.
fn pousser_tout(etage: &mut EtageDeConversion<'_>, puits: &mut CaptureOutput) -> u64 {
    let mut refus = |_dop: bool, _sr: u32, _ch: u16| false;
    loop {
        match etage.pousser(puits, &mut refus, &mut |_| {}) {
            PousseeVersLePuits::Poussee { .. } => {}
            PousseeVersLePuits::RienAPousser => break,
            PousseeVersLePuits::PuitsMort { .. } => break,
            PousseeVersLePuits::PorteurDopRefuse => panic!("aucun porteur DoP dans ce flux"),
        }
    }
    puits.empreinte()
}

// ───────────────────────────────────────────────────────────────────────────
// Les relevés. Mesurés sur `5318d073` — la chaîne en ligne de `play_url`,
// AVANT la réorganisation — puis rejoués ici à travers le puits.
// ───────────────────────────────────────────────────────────────────────────

/// Chemin identité : même cadence, mêmes canaux. Rien à convertir, donc rien
/// qui puisse dériver — et c'est précisément ce qu'il faut verrouiller en
/// premier, parce que c'est le chemin de l'immense majorité des lectures.
#[test]
fn le_puits_recoit_exactement_les_memes_octets_sans_conversion() {
    let dsp = DspAuRepos::neuf();
    let mut e = etage(&dsp, pcm16(2048, 2), 44_100, 2, 16, 44_100, 2);
    let mut puits = puits_empreinte();
    let empreinte = pousser_tout(&mut e, &mut puits);

    assert_eq!(
        puits.mots(),
        4096,
        "2048 trames stéréo font 4096 mots : le puits doit tout recevoir"
    );
    assert_eq!(
        empreinte, EMPREINTE_IDENTITE_16_BITS_STEREO,
        "le puits ne reçoit plus les mêmes octets qu'avant la réorganisation \
         (chemin identité, 16 bits stéréo 44,1 kHz)"
    );
}

/// Adaptation de canaux seule : stéréo → mono. `adapt_channels` est appelé
/// avec (source, sortie) dans CET ordre — l'inverser passerait la compilation
/// et détruirait l'image.
#[test]
fn le_puits_recoit_les_memes_octets_apres_adaptation_de_canaux() {
    let dsp = DspAuRepos::neuf();
    let mut e = etage(&dsp, pcm16(2048, 2), 44_100, 2, 16, 44_100, 1);
    let mut puits = puits_empreinte();
    let empreinte = pousser_tout(&mut e, &mut puits);

    assert_eq!(puits.mots(), 2048, "stéréo → mono : moitié moins de mots");
    assert_eq!(
        empreinte, EMPREINTE_ADAPTATION_STEREO_VERS_MONO,
        "l'adaptation de canaux ne rend plus les mêmes octets qu'avant la \
         réorganisation"
    );
}

/// Rééchantillonnage : 44,1 → 48 kHz, le cas le plus courant sur une sortie
/// partagée. L'ordre compte — adaptation PUIS sinc, jamais l'inverse : le
/// rééchantillonneur est bâti pour `output_ch` canaux.
#[test]
fn le_puits_recoit_les_memes_octets_apres_reechantillonnage() {
    let dsp = DspAuRepos::neuf();
    let mut e = etage(&dsp, pcm16(8192, 2), 44_100, 2, 16, 48_000, 2);
    e.resampler = Some(
        crate::audio::resample::new_streaming_resampler(44_100, 48_000, 2)
            .expect("le rééchantillonneur 44,1 → 48 kHz se construit"),
    );
    let mut puits = puits_empreinte();
    let empreinte = pousser_tout(&mut e, &mut puits);

    assert!(
        puits.mots() > 0,
        "le rééchantillonneur doit rendre du signal, pas du vide"
    );
    assert_eq!(
        empreinte, EMPREINTE_REECHANTILLONNAGE_44100_VERS_48000,
        "le rééchantillonnage ne rend plus les mêmes octets que le relevé du \
         13/09 : le noyau de `new_streaming_resampler` a changé. Si c'est \
         voulu, c'est un changement de RENDU — il se mesure au banc T10 \
         (`reechantillonnage_reference_2218.rs`) avant d'être acté ici"
    );
}

/// **Le cas qui distingue l'ORDRE** : adaptation de canaux ET
/// rééchantillonnage, sur le même bloc. Stéréo 44,1 kHz → mono 48 kHz.
///
/// ⚠️ Ce témoin existe parce que la contre-épreuve des trois précédents était
/// NÉGATIVE. Inverser les deux conversions dans `convertir` ne faisait rougir
/// aucun d'eux : le cas « identité » n'en fait aucune, le cas « mono » ne fait
/// que l'adaptation, le cas « sinc » que le rééchantillonnage. Trois témoins
/// verts, et l'ordre libre — exactement la garde écrite mais pas prouvée.
///
/// Ici le rééchantillonneur est bâti pour UN canal : l'adaptation doit venir
/// avant lui. L'inverse lui donnerait deux canaux entrelacés à traiter comme
/// un seul, et le résultat n'aurait plus rien à voir. La contre-épreuve est
/// désormais POSITIVE, mesurée le 12/09.
#[test]
fn le_puits_recoit_les_memes_octets_apres_adaptation_puis_reechantillonnage() {
    let dsp = DspAuRepos::neuf();
    let mut e = etage(&dsp, pcm16(8192, 2), 44_100, 2, 16, 48_000, 1);
    e.resampler = Some(
        crate::audio::resample::new_streaming_resampler(44_100, 48_000, 1)
            .expect("le rééchantillonneur mono 44,1 → 48 kHz se construit"),
    );
    let mut puits = puits_empreinte();
    let empreinte = pousser_tout(&mut e, &mut puits);

    assert_eq!(
        puits.mots(),
        8914,
        "stéréo → mono PUIS 44,1 → 48 kHz : le compte de mots dit déjà si \
         l'ordre a changé"
    );
    assert_eq!(
        empreinte, EMPREINTE_ADAPTATION_PUIS_REECHANTILLONNAGE,
        "la chaîne complète — adaptation de canaux PUIS rééchantillonnage — ne \
         rend plus les mêmes octets que le relevé du 13/09. Si l'ordre a été \
         inversé, le rééchantillonneur reçoit deux canaux entrelacés là où il \
         en attend un — mais le compte de mots ci-dessus l'aurait déjà dit. \
         Sinon, c'est le noyau sinc qui a changé : voir le banc T10."
    );
}

/// Un puits mort est CONSTATÉ, pas confondu avec une fin de flux.
///
/// C'est la moitié du contrat de `PuitsDEchantillons` qu'aucune empreinte ne
/// couvre : `ecrire` rend `false` uniquement quand le consommateur est mort, et
/// le producteur doit alors se démonter — pas enchaîner la piste suivante sur
/// un périphérique arraché (#1626).
#[test]
fn un_puits_qui_ne_consomme_plus_est_rapporte_au_producteur() {
    let dsp = DspAuRepos::neuf();
    let mut e = etage(&dsp, pcm16(2048, 2), 44_100, 2, 16, 44_100, 2);
    let mut puits = puits_empreinte();
    puits.declarer_mort();
    let mut refus = |_dop: bool, _sr: u32, _ch: u16| false;

    match e.pousser(&mut puits, &mut refus, &mut |_| {}) {
        PousseeVersLePuits::PuitsMort { trames_source } => {
            assert_eq!(
                trames_source, 2048,
                "les trames consommées sont rendues même quand le puits est \
                 mort : les amorçages les comptent sans regarder le verdict"
            );
        }
        _ => panic!("un puits qui rend false doit être rapporté comme mort"),
    }
}

/// Le porteur DoP est refusé AVANT que le puits voie quoi que ce soit.
///
/// #3233 : un porteur DoP ne survit ni au sinc ni à l'adaptation de canaux. Le
/// refus doit donc tomber entre le décodage et l'écriture — si un seul mot
/// atteignait le puits, le DAC recevrait du bruit.
#[test]
fn un_porteur_dop_refuse_n_ecrit_rien_dans_le_puits() {
    let dsp = DspAuRepos::neuf();
    let mut e = etage(&dsp, pcm16(2048, 2), 44_100, 2, 16, 48_000, 2);
    let mut puits = puits_empreinte();
    let mut refus = |_dop: bool, _sr: u32, _ch: u16| true;

    match e.pousser(&mut puits, &mut refus, &mut |_| {}) {
        PousseeVersLePuits::PorteurDopRefuse => {}
        _ => panic!("le refus du porteur DoP doit court-circuiter l'écriture"),
    }
    assert_eq!(
        puits.blocs(),
        0,
        "le puits ne doit avoir reçu AUCUN bloc : le refus tombe avant lui"
    );
    assert_eq!(puits.mots(), 0, "ni aucun mot");
}

/// Le tampon d'attente conserve le reliquat non aligné d'un bloc à l'autre.
///
/// Un octet perdu ici décale TOUS les mots suivants — c'est le bruit blanc
/// 24 bits de l'époque où le reliquat de l'en-tête était jeté. Deux moitiés
/// d'un même flux doivent rendre l'empreinte du flux entier.
#[test]
fn un_flux_coupe_en_deux_rend_la_meme_empreinte_qu_entier() {
    let octets = pcm16(2048, 2);
    // Une coupure au milieu d'une trame : 4093 n'est pas un multiple de 4.
    let (debut, fin) = octets.split_at(4093);

    let dsp_entier = DspAuRepos::neuf();
    let mut e = etage(&dsp_entier, octets.clone(), 44_100, 2, 16, 44_100, 2);
    let mut puits_entier = puits_empreinte();
    let entier = pousser_tout(&mut e, &mut puits_entier);

    let dsp_coupe = DspAuRepos::neuf();
    let mut d = etage(&dsp_coupe, debut.to_vec(), 44_100, 2, 16, 44_100, 2);
    let mut puits_coupe = puits_empreinte();
    pousser_tout(&mut d, &mut puits_coupe);
    d.en_attente.extend_from_slice(fin);
    let coupe = pousser_tout(&mut d, &mut puits_coupe);

    assert_eq!(
        puits_coupe.mots(),
        puits_entier.mots(),
        "le reliquat non aligné doit être reporté, pas jeté"
    );
    assert_eq!(
        coupe, entier,
        "couper le flux entre deux lectures ne doit rien changer à ce que le \
         puits reçoit"
    );
}

// Les relevés eux-mêmes. Voir l'en-tête du fichier : ce sont des MESURES,
// pas des valeurs à ajuster.
//
// Les deux PREMIERS datent de `5318d073`, la chaîne en ligne d'avant la
// réorganisation, et n'ont jamais bougé : aucune conversion de cadence ne les
// traverse. C'est ce qui rend la suite lisible.
//
// Les deux DERNIERS ont été remesurés le 13/09/2026, et il faut dire
// exactement pourquoi.
//
// Ces deux témoins-là injectaient leur PROPRE rééchantillonneur, écrit à la
// main à 64 coefficients — une valeur qui n'existait déjà plus nulle part en
// production. Ils imageaient donc un filtre imaginaire : le correctif D1 de
// #2218 aurait pu changer tout le rendu du produit sans qu'aucun des deux ne
// bronche. Ils prennent désormais `new_streaming_resampler`, le constructeur
// de la production, et leurs empreintes suivent le filtre réel — c'est le
// seul montage où « l'empreinte n'a pas bougé » veut dire quelque chose.
//
// Ce qui a changé dans le son, et RIEN d'autre : la fenêtre du noyau passe de
// Blackman-Harris² à Blackman² et sa longueur de 64 à 256 coefficients, ce
// qui rend 20 kHz aux sources 44,1 kHz (−10,31 dB → −0,00 dB). L'ORDRE des
// étages est intact — c'est ce que ces deux témoins gardent, et le compte de
// mots, lui, n'a pas bougé d'une unité (8 914 pour la chaîne complète) : un
// ordre inversé le ferait sauter avant même l'empreinte.
const EMPREINTE_IDENTITE_16_BITS_STEREO: u64 = 0x1433_8456_2279_0c63;
const EMPREINTE_ADAPTATION_STEREO_VERS_MONO: u64 = 0x3557_16d1_b565_a7d6;
// Était 0x4491_3fae_738e_a9ee avec le noyau 64 écrit à la main.
const EMPREINTE_REECHANTILLONNAGE_44100_VERS_48000: u64 = 0x7d6d_2c8f_4cee_4f8d;
// Était 0x8c1c_f175_68c3_9aaa avec le noyau 64 écrit à la main.
const EMPREINTE_ADAPTATION_PUIS_REECHANTILLONNAGE: u64 = 0x6cc3_fd32_3965_25ed;
