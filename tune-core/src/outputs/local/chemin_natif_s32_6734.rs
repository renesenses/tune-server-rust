//! #6734 — témoins du chemin natif S32 de Linux (`bras_alsa_natif.rs`).
//!
//! Aucun périphérique n'est ouvert : la chaîne de production est jugée de
//! l'étage au rappel — `EtageNatif` → `PuitsAnneauNatif` → `NativePcmRing` →
//! `rendre_une_periode_native_s32` — avec les mêmes fonctions que le bras
//! appelle, et le tampon que cpal remettrait au pilote ALSA en S32_LE.

use super::*;
use crate::audio::soft_mute::SoftMuteRamp;

const CADENCE: u32 = 192_000;

/// Des mots 32 bits dont les 8 bits de poids faible ne sont jamais nuls —
/// exactement ce qu'un `f32` (24 bits de mantisse) ne peut pas porter — plus
/// les deux pleines échelles.
fn mots_de_test() -> Vec<i32> {
    let mut mots = vec![
        i32::MAX,
        i32::MIN,
        i32::MIN + 1,
        -1,
        1,
        0x1234_5678,
        -0x1234_5679,
        0x7FFF_FF01,
        0x0000_00FF,
        -0x0000_0081,
    ];
    // Une rampe pseudo-aléatoire, bits bas forcés non nuls.
    let mut x: u32 = 0x9E37_79B9;
    while mots.len() < 2 * 4096 {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        mots.push((x | 0x01) as i32);
    }
    mots
}

fn octets_le(mots: &[i32]) -> Vec<u8> {
    mots.iter().flat_map(|m| m.to_le_bytes()).collect()
}

fn spec(profondeur: ProfondeurPcm) -> AudioSpec {
    AudioSpec::nouvelle(CADENCE, profondeur, 2).unwrap()
}

/// Les DSP et réglages d'une zone, au repos.
struct Zone {
    eq: std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: AtomicBool,
    mono_downmix: AtomicBool,
    reaffectation: CreneauReaffectation,
    disposition: CreneauDisposition,
}

impl Zone {
    fn au_repos() -> Self {
        Self {
            eq: std::sync::Mutex::new(None),
            convolver: std::sync::Mutex::new(None),
            crossfeed: std::sync::Mutex::new(None),
            pure_bypass: AtomicBool::new(false),
            mono_downmix: AtomicBool::new(false),
            reaffectation: CreneauReaffectation::vide(),
            disposition: std::sync::Mutex::new(None),
        }
    }

    fn conditions(&self, spec: AudioSpec, volume_units: u32) -> ConditionsDuNatif {
        ConditionsDuNatif {
            spec,
            volume_units,
            // La fonction même que `play_url` appelle.
            dsp_identite: local_dsp_is_identity(
                &self.eq,
                &self.convolver,
                &self.crossfeed,
                &self.pure_bypass,
                &self.mono_downmix,
            ),
            voies_pilotees: false,
            fondu_ms: 0,
        }
    }
}

/// Fait passer `mots` (source 32 bits) par l'étage, le puits, l'anneau et le
/// rappel, au volume `volume_units` ; rend ce que le rappel a écrit dans le
/// tampon du pilote, et le verdict de l'étage.
fn traverser_le_chemin_natif(zone: &Zone, mots: &[i32], volume_units: u32) -> (Vec<i32>, bool) {
    let spec = spec(ProfondeurPcm::Entier32);
    let anneau = Arc::new(NativePcmRing::new(mots.len() * 2));
    let (_stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let paused = AtomicBool::new(false);
    let force_silent = AtomicBool::new(false);
    // Comme le bras : l'étage voit l'unité, le volume est au rappel.
    let unite = AtomicU32::new(1000);
    let mut etage = EtageNatif::monter(
        spec,
        FormatOuvert::new(CADENCE, 2),
        &unite,
        &zone.eq,
        &zone.convolver,
        &zone.crossfeed,
        &zone.pure_bypass,
        &zone.mono_downmix,
    )
    .avec_l_adaptation_des_canaux(&zone.reaffectation, &zone.disposition);
    let mut puits = PuitsAnneauNatif::sur(
        anneau.clone(),
        etage.spec_du_puits(),
        &stop_rx,
        &paused,
        &force_silent,
    );
    let etage_intact = match etage.decoder_et_pousser(&octets_le(mots), &mut puits) {
        EcritureNative::Poussee { bit_perfect, .. } => bit_perfect,
        autre => panic!("l'étage aurait dû pousser le bloc : {autre:?}"),
    };

    let volume = AtomicU32::new(volume_units);
    let data_started = AtomicBool::new(false);
    let mut rampe = SoftMuteRamp::new(CADENCE, 2);
    let mut tampon_du_pilote = vec![0x5A5A_5A5Ai32; mots.len()];
    let lus = rendre_une_periode_native_s32(
        &anneau,
        &volume,
        &paused,
        &force_silent,
        &data_started,
        &mut rampe,
        0,
        0,
        &mut tampon_du_pilote,
    );
    assert_eq!(lus, mots.len(), "le rappel doit tout tirer de l'anneau");
    (tampon_du_pilote, etage_intact)
}

/// LE témoin : au volume unité, sans DSP, un tampon 32 bits entiers dont les
/// 8 bits bas sont non nuls arrive au pilote BIT À BIT.
#[test]
fn un_wav_32_bits_traverse_le_chemin_natif_bit_a_bit_6734() {
    let zone = Zone::au_repos();
    let mots = mots_de_test();
    assert!(
        mots.iter().all(|m| m & 0xFF != 0 || *m == i32::MIN),
        "le témoin n'a de sens que si les 8 bits bas portent quelque chose"
    );
    let (au_pilote, etage_intact) = traverser_le_chemin_natif(&zone, &mots, 1000);
    let premier_ecart = mots.iter().zip(&au_pilote).position(|(a, b)| a != b);
    assert_eq!(
        premier_ecart,
        None,
        "chemin natif S32 : le mot {:?} source {:#010x} arrive {:#010x} au pilote — \
         ses bits de poids faible ont été perdus, le chemin est repassé par un f32 (#6734)",
        premier_ecart,
        premier_ecart.map(|i| mots[i]).unwrap_or_default(),
        premier_ecart.map(|i| au_pilote[i]).unwrap_or_default(),
    );
    assert!(etage_intact, "l'étage doit déclarer ses octets intacts");

    // Contre-témoin : ce que l'ancien chemin (anneau f32, puis
    // `IntoSample<i32>`) aurait remis au même pilote. Il doit DIFFÉRER, sans
    // quoi le témoin ci-dessus ne prouverait rien.
    use symphonia::core::audio::conv::IntoSample;
    let par_le_flottant: Vec<i32> = pcm_bytes_to_f32(&octets_le(&mots), 32)
        .into_iter()
        .map(<f32 as IntoSample<i32>>::into_sample)
        .collect();
    assert_ne!(
        par_le_flottant, mots,
        "l'anneau f32 aurait dû tronquer ces mots : le témoin serait creux"
    );
}

/// Le contrat publié : « bit-perfect », transport entier natif, seulement à
/// l'unité et DSP neutre.
#[test]
fn le_contrat_natif_ne_dit_bit_perfect_qu_a_l_unite_sans_dsp_6734() {
    let zone = Zone::au_repos();
    let slot = std::sync::Mutex::new(None);
    let publier = |etage_intact: bool, volume_units: u32| {
        publier_le_contrat_natif_s32(
            &slot,
            etage_intact,
            false,
            volume_units,
            &zone.eq,
            &zone.convolver,
            &zone.crossfeed,
            &zone.pure_bypass,
            &zone.mono_downmix,
        )
    };

    let intact = publier(true, 1000);
    assert!(
        intact.bit_perfect,
        "chemin natif à l'unité sans DSP : le contrat doit dire bit-perfect ({:?})",
        intact.reasons
    );
    assert_eq!(
        intact.sample_transport,
        OutputSampleTransport::NativeInteger
    );
    assert_eq!(slot.lock().unwrap().as_ref(), Some(&intact));

    let volume = publier(true, 700);
    assert!(
        !volume.bit_perfect,
        "volume logiciel à 70 % sur le chemin natif : le contrat ne doit PAS dire \
         bit-perfect — le rappel réécrit chaque mot (#6734)"
    );
    assert!(volume.reasons.contains(&OutputSignalReason::SoftwareVolume));

    let dsp = publier(false, 1000);
    assert!(!dsp.bit_perfect, "DSP appliqué : pas de bit-perfect");
}

/// Le chemin f32 reste pris dès que le volume n'est pas à 100 %, qu'un DSP
/// réécrit les échantillons, ou que la source n'est pas un entier 32 bits.
#[test]
fn le_chemin_partage_reste_pris_hors_du_cas_natif_6734() {
    let zone = Zone::au_repos();
    let s32 = spec(ProfondeurPcm::Entier32);
    assert_eq!(chemin_natif_s32(&zone.conditions(s32, 1000)), Ok(()));

    assert_eq!(
        chemin_natif_s32(&zone.conditions(s32, 999)),
        Err(MotifDuCheminPartage::VolumeLogiciel),
        "volume à 99,9 % : le chemin natif ne doit pas être pris (#6734)"
    );
    assert_eq!(
        chemin_natif_s32(&zone.conditions(s32, 0)),
        Err(MotifDuCheminPartage::VolumeLogiciel)
    );

    // Un vrai DSP armé — le repli mono réécrit chaque échantillon — vu par
    // `local_dsp_is_identity`, comme dans `play_url`.
    zone.mono_downmix.store(true, Ordering::SeqCst);
    assert_eq!(
        chemin_natif_s32(&zone.conditions(s32, 1000)),
        Err(MotifDuCheminPartage::DspActif),
        "DSP actif : le chemin natif ne doit pas être pris (#6734)"
    );
    // PURE contourne le DSP : le natif redevient possible.
    zone.pure_bypass.store(true, Ordering::SeqCst);
    assert_eq!(chemin_natif_s32(&zone.conditions(s32, 1000)), Ok(()));
    zone.pure_bypass.store(false, Ordering::SeqCst);
    zone.mono_downmix.store(false, Ordering::SeqCst);

    for profondeur in [
        ProfondeurPcm::Entier16,
        ProfondeurPcm::Entier24,
        ProfondeurPcm::FlottantIeee32,
    ] {
        assert_eq!(
            chemin_natif_s32(&zone.conditions(spec(profondeur), 1000)),
            Err(MotifDuCheminPartage::SourcePasEntiere32Bits),
            "{profondeur:?} : l'anneau f32 le porte intact, rien ne change"
        );
    }

    let base = zone.conditions(s32, 1000);
    for (conditions, motif) in [
        (
            ConditionsDuNatif {
                voies_pilotees: true,
                ..base
            },
            MotifDuCheminPartage::VoiesPilotees,
        ),
        (
            ConditionsDuNatif {
                fondu_ms: 3000,
                ..base
            },
            MotifDuCheminPartage::FonduEnchaine,
        ),
    ] {
        assert_eq!(chemin_natif_s32(&conditions), Err(motif));
    }
}

/// Le volume baissé EN COURS de piste sur le chemin natif : appliqué dans le
/// rappel, à la période suivante, sans jamais dépasser la pleine échelle.
#[test]
fn le_volume_du_rappel_natif_reste_dans_la_pleine_echelle_6734() {
    let zone = Zone::au_repos();
    let mots = mots_de_test();
    let (au_pilote, _) = traverser_le_chemin_natif(&zone, &mots, 500);
    for (source, rendu) in mots.iter().zip(&au_pilote) {
        let attendu = (f64::from(*source) * 0.5).round() as i32;
        assert_eq!(*rendu, attendu, "source {source:#010x}");
    }
    // Pleine échelle et gain ≤ 1 : borné, jamais replié.
    let mut rampe = SoftMuteRamp::new(CADENCE, 2);
    let mut extremes = [i32::MAX, i32::MIN, i32::MAX, i32::MIN];
    rampe.apply_i32(&mut extremes, 1.0);
    assert_eq!(extremes, [i32::MAX, i32::MIN, i32::MAX, i32::MIN]);
    rampe.apply_i32(&mut extremes, 0.999);
    assert!(
        extremes[0] > 0 && extremes[1] < 0,
        "aucun repliement de signe"
    );
}

/// Un seek non pré-positionné : le saut prend d'abord l'amorce, puis le flux,
/// et reste aligné sur la trame.
#[test]
fn le_seek_natif_jette_l_amorce_puis_le_flux_6734() {
    assert_eq!(sauter_dans_l_amorce(4000, 0), (0, 0));
    assert_eq!(sauter_dans_l_amorce(4000, 1600), (1600, 0));
    assert_eq!(sauter_dans_l_amorce(4000, 800_000), (4000, 796_000));
    let mut flux = std::io::Cursor::new(vec![9u8; 200_000]);
    assert!(jeter_du_flux(&mut flux, 150_000, &AtomicBool::new(false)));
    assert_eq!(flux.position(), 150_000);
    assert!(
        !jeter_du_flux(&mut flux, 60_000, &AtomicBool::new(false)),
        "fin de flux"
    );
}

/// En pause, le rappel rend du silence sans tirer de l'anneau.
#[test]
fn la_pause_rend_du_silence_sans_vider_l_anneau_natif_6734() {
    let anneau = NativePcmRing::new(64);
    anneau.push(&[7; 32]);
    let mut rampe = SoftMuteRamp::new(CADENCE, 2);
    let mut tampon = [1i32; 16];
    let lus = rendre_une_periode_native_s32(
        &anneau,
        &AtomicU32::new(1000),
        &AtomicBool::new(true),
        &AtomicBool::new(false),
        &AtomicBool::new(true),
        &mut rampe,
        0,
        0,
        &mut tampon,
    );
    assert_eq!(lus, 0);
    assert_eq!(tampon, [0; 16]);
    assert_eq!(anneau.available(), 32, "la pause ne consomme rien");
}

/// La liste cpal décide : S32 annoncé à la cadence et aux voies de la source,
/// sinon le chemin partagé.
#[test]
fn le_natif_exige_s32_annonce_a_la_cadence_et_aux_voies_6734() {
    use cpal::SampleFormat::{F32, I16, I32};
    let adi2 = [(I32, 2, 44_100, 768_000), (I16, 2, 44_100, 768_000)];
    assert!(s32_annonce(adi2, 2, 192_000));
    assert!(!s32_annonce(adi2, 6, 192_000), "voies différentes");
    assert!(!s32_annonce(adi2, 2, 1_536_000), "cadence hors plage");
    assert!(
        !s32_annonce(
            [(I16, 2, 44_100, 192_000), (F32, 2, 44_100, 192_000)],
            2,
            96_000
        ),
        "aucun S32 : chemin partagé"
    );
}

/// Garde de branchement : `play_url` essaie le chemin natif AVANT d'ouvrir le
/// chemin partagé, et le bras publie son contrat. Compté hors définitions.
#[test]
fn play_url_branche_le_chemin_natif_avant_le_partage_6734() {
    let local = include_str!("../local.rs");
    let natif = local
        .find("bras_alsa_natif::BackendAlsaNatif::tenter(&demande)")
        .expect("play_url doit tenter le chemin natif S32");
    let partage = local
        .find("let mut backend = match BackendCpal::ouvrir(&demande)")
        .expect("ouverture du chemin partagé");
    assert!(natif < partage, "le natif doit être tenté avant le partagé");
    let bras = include_str!("bras_alsa_natif.rs");
    assert_eq!(
        bras.matches("publier_le_contrat_natif_s32(").count(),
        2,
        "une définition et UN appel dans le bras"
    );
}
