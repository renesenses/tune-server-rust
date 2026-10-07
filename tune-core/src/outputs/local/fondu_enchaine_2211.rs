//! #2211 — le fondu enchaîné **branché sur la sortie locale**.
//!
//! Le moteur `audio::fondu_enchaine` avait ses témoins, mais aucun appelant
//! de production. Ce module éprouve le raccordement tel que `play_url` le
//! monte, sans carte son :
//!
//! 1. **le chemin réel** : l'étage de conversion de la sortie locale
//!    ([`EtageDeConversion`]), la frontière gapless qu'elle appelle
//!    (`enchainer_la_piste`), puis la frontière du fondu
//!    ([`jonction_du_fondu`]), sur un puits qui retient tout. Fondu appliqué,
//!    gapless respecté au bit près, PURE respecté, et une piste suivante à
//!    une autre cadence fondue APRÈS conversion ;
//! 2. **la sortie** : la durée bornée, la consigne rangée avec la piste
//!    suivante, la position qui retranche la réserve, l'étape déclarée
//!    pendant le recouvrement seulement ;
//! 3. **le branchement** : `play_url` monte bien le puits de fondu, appelle
//!    la frontière après `enchainer_la_piste`, et solde la réserve en fin de
//!    chaîne.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
use std::sync::{Arc, Mutex};

use super::empreinte_du_puits_r1::{DspAuRepos, etage};
use super::*;
use crate::audio::fondu_enchaine::{
    ConsigneDeJonction, CourbeDeFondu, Jonction, MotifSansFondu, PuitsDeFondu,
};
use crate::outputs::traits::{OutputTarget, PlayMedia};

const SR: u32 = 44_100;
const FONDU_MS: u32 = 500;

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

/// Un sinus stéréo 16 bits : `trames` trames à `freq` Hz, amplitude `amp`.
fn sinus16(trames: usize, cadence: u32, freq: f32, amp: f32) -> Vec<u8> {
    let mut octets = Vec::with_capacity(trames * 4);
    for i in 0..trames {
        let v = (amp
            * (2.0 * std::f32::consts::PI * freq * i as f32 / cadence as f32).sin()
            * f32::from(i16::MAX)) as i16;
        octets.extend_from_slice(&v.to_le_bytes());
        octets.extend_from_slice(&v.to_le_bytes());
    }
    octets
}

fn pousser_tout(e: &mut EtageDeConversion<'_>, puits: &mut PuitsDeFondu<'_>) {
    let mut refus = |_dop: bool, _sr: u32, _ch: u16| false;
    loop {
        match e.pousser(puits, &mut refus, &mut |_| {}) {
            PousseeVersLePuits::Poussee { .. } => {}
            PousseeVersLePuits::RienAPousser => break,
            autre => panic!(
                "poussée inattendue : {}",
                matches!(autre, PousseeVersLePuits::PuitsMort { .. })
            ),
        }
    }
}

struct Lecture {
    mots: Vec<f32>,
    melangees: usize,
    actif_pendant: bool,
}

/// Joue A puis B par l'étage réel, comme la boucle gapless de `play_url`.
fn jouer(fondu_ms: u32, pure: bool, consigne: ConsigneDeJonction, cadence_b: u32) -> Lecture {
    let dsp = DspAuRepos::neuf();
    let a = sinus16(SR as usize * 2, SR, 441.3, 0.5);
    let b = sinus16(cadence_b as usize * 2, cadence_b, 659.7, 0.5);
    let mut e = etage(&dsp, a, SR, 2, 16, SR, 2);
    let capture = Enregistreur::default();
    let actif = Arc::new(AtomicBool::new(false));
    let mut puits = PuitsDeFondu::nouveau(
        Box::new(capture.clone()),
        FormatOuvert::new(SR, 2),
        CourbeDeFondu::default(),
        Arc::new(AtomicU32::new(fondu_ms)),
        Arc::new(AtomicU64::new(0)),
        actif.clone(),
    );
    pousser_tout(&mut e, &mut puits);

    let spec_b = AudioSpec::depuis_entete(cadence_b, 16, 2).expect("format valide");
    e.enchainer_la_piste(&mut puits, spec_b, ReglesDeCadence::default(), "banc")
        .expect("la frontière gapless enchaîne");
    let jonction = Jonction {
        fondu_arme: puits.fondu_arme(),
        pure,
        bitperfect_strict: false,
        dop: false,
        reserve_vide: puits.reserve_vide(),
        consigne,
        queue_silencieuse: puits.queue_silencieuse(),
    };
    assert!(jonction_du_fondu(&mut puits, jonction, "banc"));
    let actif_pendant = actif.load(Ordering::Relaxed);
    e.recevoir(&b);
    pousser_tout(&mut e, &mut puits);
    let melangees = puits.trames_melangees();
    if e.needs_resample {
        e.vider(&mut puits);
    }
    assert!(puits.terminer());
    Lecture {
        mots: capture.mots(),
        melangees,
        actif_pendant,
    }
}

// ── 1. Le chemin réel ──────────────────────────────────────────────────────

#[test]
fn fondu_applique_sur_l_etage_de_la_sortie_locale() {
    let reference = jouer(0, false, ConsigneDeJonction::Permise, SR);
    let fondu = jouer(FONDU_MS, false, ConsigneDeJonction::Permise, SR);
    let n = (SR as usize * FONDU_MS as usize / 1000) * 2;

    assert!(
        fondu.actif_pendant,
        "le recouvrement est déclaré à la frontière"
    );
    assert_eq!(
        fondu.mots.len(),
        reference.mots.len() - n,
        "les deux pistes se superposent pendant {FONDU_MS} ms, rien n'est perdu"
    );
    let debut = SR as usize * 2 * 2 - n;
    assert_eq!(
        fondu.mots[..debut],
        reference.mots[..debut],
        "avant le recouvrement, la sortante passe intacte"
    );
    let meles = fondu.mots[debut..debut + n]
        .iter()
        .zip(&reference.mots[debut..debut + n])
        .filter(|(x, y)| x.to_bits() != y.to_bits())
        .count();
    assert!(
        meles > n * 9 / 10,
        "pendant le recouvrement, les mots mélangent les deux pistes : {meles}/{n} diffèrent"
    );
    assert_eq!(fondu.melangees, n / 2, "le recouvrement fait {FONDU_MS} ms");
    assert!(
        fondu.mots[debut + n..]
            .iter()
            .zip(&reference.mots[debut + n * 2..])
            .all(|(x, y)| x.to_bits() == y.to_bits()),
        "après le recouvrement, l'entrante passe intacte"
    );
}

#[test]
fn gapless_respecte_un_album_sans_blanc_rend_le_flux_au_bit_pres() {
    let reference = jouer(0, false, ConsigneDeJonction::Permise, SR);
    // Même album, et la sortante (un sinus à −6 dBFS) finit sur de la musique.
    let meme_album = jouer(FONDU_MS, false, ConsigneDeJonction::PermiseSiBlanc, SR);
    assert!(!meme_album.actif_pendant);
    assert_eq!(meme_album.melangees, 0);
    assert!(
        meme_album.mots.len() == reference.mots.len()
            && meme_album
                .mots
                .iter()
                .zip(&reference.mots)
                .all(|(x, y)| x.to_bits() == y.to_bits()),
        "le gapless prime : le flux est celui d'un enchaînement sans fondu, au bit près"
    );
}

#[test]
fn pure_respecte_rien_n_est_mele() {
    let reference = jouer(0, false, ConsigneDeJonction::Permise, SR);
    let pure = jouer(FONDU_MS, true, ConsigneDeJonction::Permise, SR);
    assert!(!pure.actif_pendant);
    assert!(
        pure.mots.len() == reference.mots.len()
            && pure
                .mots
                .iter()
                .zip(&reference.mots)
                .all(|(x, y)| x.to_bits() == y.to_bits()),
        "PURE : aucun échantillon n'est additionné"
    );
}

/// La piste suivante à 48 kHz sur un flux ouvert à 44,1 kHz : l'étage la
/// convertit (conversion déjà gérée par la chaîne), et le fondu porte sur des
/// mots au format OUVERT — sa durée est comptée à 44,1 kHz.
#[test]
fn une_cadence_differente_est_fondue_apres_conversion() {
    let fondu = jouer(FONDU_MS, false, ConsigneDeJonction::Permise, 48_000);
    assert!(fondu.actif_pendant);
    assert_eq!(fondu.melangees, SR as usize * FONDU_MS as usize / 1000);
}

// ── 2. La sortie ───────────────────────────────────────────────────────────

#[test]
fn sortie_la_duree_est_bornee_a_douze_secondes() {
    let sortie = LocalOutput::new("Banc".into());
    assert_eq!(sortie.fondu_enchaine_ms(), 0, "désactivé par défaut");
    sortie.set_fondu_enchaine_ms(60_000);
    assert_eq!(sortie.fondu_enchaine_ms(), 12_000);
    sortie.set_fondu_enchaine_ms(3_000);
    assert_eq!(sortie.fondu_enchaine_ms(), 3_000);
}

fn media(url: &str) -> PlayMedia<'_> {
    PlayMedia {
        url,
        mime_type: "audio/wav",
        title: None,
        artist: None,
        album: None,
        cover_url: None,
        duration_ms: Some(1_000),
        file_size: None,
        file_path: None,
        sample_rate: None,
        bit_depth: None,
        channels: None,
        live_stream: false,
        byte_seekable: false,
        origin_url: None,
        source: None,
        source_id: None,
        track_number: None,
        disc_number: None,
    }
}

#[tokio::test]
async fn sortie_la_consigne_voyage_avec_la_piste_suivante_une_seule_fois() {
    let sortie = LocalOutput::new("Banc".into());
    sortie.consigner_la_jonction_suivante(ConsigneDeJonction::Interdite(MotifSansFondu::AlbumLive));
    sortie
        .set_next_media(&media("http://x/a.wav"))
        .await
        .unwrap();
    let rangee = sortie.next_media.lock().unwrap().clone().unwrap();
    assert_eq!(
        rangee.consigne_de_fondu,
        ConsigneDeJonction::Interdite(MotifSansFondu::AlbumLive)
    );
    // Contre-épreuve : la piste armée ensuite sans consigne n'hérite de rien.
    sortie
        .set_next_media(&media("http://x/b.wav"))
        .await
        .unwrap();
    let rangee = sortie.next_media.lock().unwrap().clone().unwrap();
    assert_eq!(rangee.consigne_de_fondu, ConsigneDeJonction::Inconnue);
}

#[tokio::test]
async fn sortie_la_position_retranche_la_reserve_et_l_etape_suit_le_recouvrement() {
    let sortie = LocalOutput::new("Banc".into());
    sortie.position_ms.store(10_000, Ordering::Relaxed);
    sortie.fondu_retenue_ms.store(4_000, Ordering::Relaxed);
    assert_eq!(sortie.get_status().await.unwrap().position_ms, 6_000);
    sortie.fondu_retenue_ms.store(0, Ordering::Relaxed);
    assert_eq!(sortie.get_status().await.unwrap().position_ms, 10_000);

    let spec = AudioSpec::depuis_entete(SR, 16, 2).unwrap();
    *sortie.transformations_reelles.lock().unwrap() = Some(TransformationsReelles::nouvelles(
        spec,
        FormatOuvert::new(SR, 2),
        false,
    ));
    assert!(!sortie.transformations_reelles().unwrap().fondu_enchaine());
    sortie.fondu_actif.store(true, Ordering::Relaxed);
    assert!(
        sortie.transformations_reelles().unwrap().fondu_enchaine(),
        "pendant le recouvrement, la sortie déclare le fondu"
    );
}

// ── 3. Le branchement ──────────────────────────────────────────────────────

fn compact(src: &str) -> String {
    src.chars().filter(|c| !c.is_whitespace()).collect()
}

#[test]
fn branchement_play_url_monte_le_puits_de_fondu_et_tient_la_frontiere() {
    let src = compact(include_str!("../local.rs"));
    assert!(
        src.contains("letmutpuits=matchbackend.puits(){Puits::Flottant(puits)=>{Box::new(crate::audio::fondu_enchaine::PuitsDeFondu::nouveau(puits,FormatOuvert::new(output_sr,output_ch),"),
        "play_url doit placer le puits de fondu entre l'étage et l'anneau, au format ouvert"
    );
    let frontiere = src
        .find("letOk(convolver_format_changed)=etage.enchainer_la_piste(&mut*puits,nouvelle_spec,regles,&device_name)else{break;};")
        .expect("la frontière gapless");
    let jonction = src[frontiere..]
        .find("jonction_du_fondu(&mutpuits,jonction,&device_name)")
        .expect("la frontière du fondu suit la frontière gapless");
    let metadonnees = src[frontiere..]
        .find("*uri_ref.lock().unwrap()=Some(next.url.clone());")
        .expect("la bascule des métadonnées");
    assert!(
        jonction < metadonnees,
        "le fondu se décide avant que la piste suivante ne soit annoncée"
    );
    let bloc = &src[frontiere..frontiere + jonction];
    for attendu in [
        "pure:pure_bypass.load(Ordering::Relaxed),",
        "bitperfect_strict:strict_bitperfect,",
        "dop:dop_active.load(Ordering::Relaxed),",
        "consigne:next.consigne_de_fondu,",
    ] {
        assert!(bloc.contains(attendu), "la frontière doit lire `{attendu}`");
    }
    let vidage = src
        .find("etage.vider(&mut*puits);}")
        .expect("le vidage du rééchantillonneur en fin de chaîne");
    assert!(
        src[vidage..].contains("puits.terminer();"),
        "la réserve se solde en fin de chaîne, après le vidage"
    );
}
