//! #7614 — le greffon de suréchantillonnage dans l'étage de la sortie locale.
//!
//! Témoins, tous sur un puits de capture (aucun périphérique, aucun son) :
//! - désactivé, la sortie est identique à l'entrée, bit à bit ;
//! - activé, la cadence est portée ×L, la longueur conservée (n·L plus la
//!   queue du noyau), aucune crête au-dessus de la pleine échelle, et l'étage
//!   le déclare au chemin du signal ;
//! - un changement de réglage pendant la piste ne change RIEN à la piste en
//!   cours (le niveau ne bouge pas) ; la frontière suivante rouvre la sortie ;
//! - PURE allumé en cours de piste : la frontière rouvre, la piste suivante
//!   s'ouvre sans le greffon ;
//! - jamais sous PURE ni sur un porteur DoP à l'ouverture.
use std::sync::Mutex;

use super::empreinte_du_puits_r1::{DspAuRepos, etage};
use super::{
    Etage, EtageDeConversion, MotifDeReouverture, PousseeVersLePuits, ReglesDeCadence,
    SurechDeLEtage, surechantillonneur_admis_a_l_ouverture,
};
use crate::audio::surechantillonnage::{
    ChoixSurechantillonnage, EtageSurechantillonneur, MesureMachine, UpsamplerSettings,
    VerdictMachine,
};
use crate::outputs::traits::{AudioSpec, CaptureOutput, FormatOuvert};

fn verdict() -> VerdictMachine {
    VerdictMachine {
        simd_path: "test".into(),
        margin: 3.0,
        results: vec![MesureMachine {
            filter: "polyvalent".into(),
            from_hz: 44_100,
            to_hz: 352_800,
            realtime_factor: 10.0,
            admitted: true,
        }],
        measured_at: 0,
    }
}

fn choix(enabled: bool) -> ChoixSurechantillonnage {
    ChoixSurechantillonnage {
        reglages: UpsamplerSettings {
            enabled,
            ..Default::default()
        },
        verdict: verdict(),
    }
}

/// fs/4 à 45°, 16 bits stéréo : des ÉCHANTILLONS à ±32 762 (−0,001 dBFS),
/// dont la reconstruction culmine à +3 dBFS entre les échantillons — le cas
/// d'école de l'écrêtage par un suréchantillonneur sans réserve.
fn sinus16(trames: usize) -> Vec<u8> {
    let mut octets = Vec::with_capacity(trames * 4);
    for n in 0..trames {
        let x = 1.414 * (std::f64::consts::TAU * 11_025.0 * n as f64 / 44_100.0 + 0.7854).sin();
        let mot = (x * 32_767.0).round() as i16;
        octets.extend_from_slice(&mot.to_le_bytes());
        octets.extend_from_slice(&(-mot).to_le_bytes());
    }
    octets
}

fn pousser_tout(e: &mut EtageDeConversion<'_>, puits: &mut CaptureOutput) {
    let mut refus = |_dop: bool, _sr: u32, _ch: u16| false;
    loop {
        match e.pousser(puits, &mut refus, &mut |_| {}) {
            PousseeVersLePuits::Poussee { .. } => {}
            _ => break,
        }
    }
}

fn avec_le_greffon<'a>(
    e: &mut EtageDeConversion<'a>,
    slot: &'a Mutex<Option<ChoixSurechantillonnage>>,
    vers: u32,
) {
    let c = slot.lock().unwrap().clone().unwrap();
    e.sortie = FormatOuvert::new(vers, 2);
    e.needs_resample = false;
    e.surechantillonneur = Some(SurechDeLEtage {
        etage: Some(EtageSurechantillonneur::nouveau(&c.reglages, 44_100, vers, 2).unwrap()),
        choix_de_la_zone: slot,
        reglages_a_l_ouverture: Some(c.reglages),
    });
}

/// Désactivé (aucun choix, ou un choix qui n'a pas pu tourner) : la sortie
/// est la même, bit à bit, que celle de l'étage d'avant le greffon.
#[test]
fn desactive_la_sortie_est_identique_a_l_entree() {
    let dsp = DspAuRepos::neuf();
    let octets = sinus16(20_000);
    let mut reference = etage(&dsp, octets.clone(), 44_100, 2, 16, 44_100, 2);
    let mut puits_ref = CaptureOutput::avec_retenue(FormatOuvert::new(44_100, 2), 1 << 20);
    pousser_tout(&mut reference, &mut puits_ref);

    let slot = Mutex::new(None);
    let mut sans = etage(&dsp, octets.clone(), 44_100, 2, 16, 44_100, 2);
    sans.surechantillonneur = Some(SurechDeLEtage {
        etage: None,
        choix_de_la_zone: &slot,
        reglages_a_l_ouverture: None,
    });
    let mut puits = CaptureOutput::avec_retenue(FormatOuvert::new(44_100, 2), 1 << 20);
    pousser_tout(&mut sans, &mut puits);

    assert_eq!(puits.empreinte(), puits_ref.empreinte());
    let mots = puits.mots_livres().unwrap();
    assert!(puits.retenue_complete());
    assert_eq!(mots.len(), 40_000);
    for (i, paire) in octets.chunks_exact(2).enumerate() {
        let attendu = f32::from(i16::from_le_bytes([paire[0], paire[1]])) / 32_768.0;
        assert_eq!(mots[i].to_bits(), attendu.to_bits(), "mot {i}");
    }
    assert!(sans.transformations().surechantillonnage().is_none());
}

#[test]
fn active_la_cadence_est_portee_sans_crete_et_declaree() {
    let dsp = DspAuRepos::neuf();
    let slot = Mutex::new(Some(choix(true)));
    let mut e = etage(&dsp, sinus16(20_000), 44_100, 2, 16, 44_100, 2);
    avec_le_greffon(&mut e, &slot, 352_800);
    let mut puits = CaptureOutput::avec_retenue(FormatOuvert::new(352_800, 2), 1 << 23);
    pousser_tout(&mut e, &mut puits);
    assert_eq!(
        puits.trames(),
        8 * 20_000,
        "n trames d'entrée, 8·n en sortie"
    );
    assert!(e.vider(&mut puits));
    let queue = puits.trames() - 8 * 20_000;
    assert!(
        queue > 0 && queue < 2_000,
        "la queue du noyau : {queue} trames"
    );
    assert_eq!(puits.blocs_non_alignes(), 0);
    let crete = puits
        .mots_livres()
        .unwrap()
        .iter()
        .fold(0.0f32, |m, x| m.max(x.abs()));
    // fs/4 à 45° : +3 dB entre les échantillons sans réserve.
    assert!(crete <= 1.0, "crête {crete} au-dessus de la pleine échelle");
    assert!(
        crete > 0.9,
        "crête {crete} : la réserve est-elle excessive ?"
    );
    let t = e.transformations();
    assert!(t.reechantillonnage());
    let (filtre, reserve) = t.surechantillonnage().expect("déclaré au chemin du signal");
    assert_eq!(filtre, "polyvalent");
    assert!(reserve < 0.0, "réserve {reserve} dB");
}

/// Bertrand, 07/11 : activation, désactivation et changement de filtre ne
/// prennent effet qu'à la piste suivante ; le niveau ne bouge jamais pendant
/// une piste.
#[test]
fn un_reglage_change_en_cours_de_piste_attend_la_frontiere() {
    let dsp = DspAuRepos::neuf();
    let octets = sinus16(30_000);
    let (premiere, seconde) = octets.split_at(octets.len() / 2);

    // Référence : le réglage ne bouge pas.
    let slot_ref = Mutex::new(Some(choix(true)));
    let mut reference = etage(&dsp, octets.clone(), 44_100, 2, 16, 44_100, 2);
    avec_le_greffon(&mut reference, &slot_ref, 352_800);
    let mut puits_ref = CaptureOutput::ouvert(FormatOuvert::new(352_800, 2));
    pousser_tout(&mut reference, &mut puits_ref);

    // Le même flux, réglage désactivé puis changé de filtre à mi-piste.
    let slot = Mutex::new(Some(choix(true)));
    let mut e = etage(&dsp, premiere.to_vec(), 44_100, 2, 16, 44_100, 2);
    avec_le_greffon(&mut e, &slot, 352_800);
    let mut puits = CaptureOutput::ouvert(FormatOuvert::new(352_800, 2));
    pousser_tout(&mut e, &mut puits);
    *slot.lock().unwrap() = None;
    e.recevoir(&seconde[..seconde.len() / 2]);
    pousser_tout(&mut e, &mut puits);
    *slot.lock().unwrap() = Some(ChoixSurechantillonnage {
        reglages: UpsamplerSettings {
            enabled: true,
            filter: "precision".into(),
            ..Default::default()
        },
        verdict: verdict(),
    });
    e.recevoir(&seconde[seconde.len() / 2..]);
    pousser_tout(&mut e, &mut puits);
    assert_eq!(
        puits.empreinte(),
        puits_ref.empreinte(),
        "la piste en cours ne doit rien entendre du changement de réglage"
    );

    // À la frontière, la sortie est rouverte.
    let spec = AudioSpec::depuis_entete(44_100, 16, 2).unwrap();
    assert_eq!(
        e.enchainer_la_piste(&mut puits, spec, ReglesDeCadence::default(), "témoin"),
        Err(MotifDeReouverture::Surechantillonnage)
    );
    // Réglage revenu à celui de l'ouverture : la piste s'enchaîne sans blanc,
    // l'étage garde son état.
    *slot.lock().unwrap() = Some(choix(true));
    assert_eq!(
        e.enchainer_la_piste(&mut puits, spec, ReglesDeCadence::default(), "témoin"),
        Ok(false)
    );
    assert!(e.transformations().surechantillonnage().is_some());
}

#[test]
fn pure_allume_en_cours_de_piste_rouvre_a_la_frontiere() {
    let dsp = DspAuRepos::neuf();
    let slot = Mutex::new(Some(choix(true)));
    let mut e = etage(&dsp, sinus16(4_000), 44_100, 2, 16, 44_100, 2);
    avec_le_greffon(&mut e, &slot, 352_800);
    let mut puits = CaptureOutput::ouvert(FormatOuvert::new(352_800, 2));
    pousser_tout(&mut e, &mut puits);
    let regles = ReglesDeCadence {
        pure: true,
        ..Default::default()
    };
    let spec = AudioSpec::depuis_entete(44_100, 16, 2).unwrap();
    assert_eq!(
        e.enchainer_la_piste(&mut puits, spec, regles, "témoin"),
        Err(MotifDeReouverture::Pure)
    );
    // Une piste d'une autre cadence : rouverte aussi.
    let spec_96 = AudioSpec::depuis_entete(96_000, 24, 2).unwrap();
    assert_eq!(
        e.enchainer_la_piste(&mut puits, spec_96, ReglesDeCadence::default(), "témoin"),
        Err(MotifDeReouverture::Surechantillonnage)
    );
}

#[test]
fn jamais_sous_pure_ni_sur_un_porteur_dop() {
    let spec16 = AudioSpec::depuis_entete(44_100, 16, 2).unwrap();
    let spec24 = AudioSpec::depuis_entete(176_400, 24, 2).unwrap();
    let pcm24 = vec![0x11u8; super::DOP_DETECT_FRAMES * 2 * 3];
    assert!(surechantillonneur_admis_a_l_ouverture(
        spec16,
        &[],
        false,
        false
    ));
    assert!(!surechantillonneur_admis_a_l_ouverture(
        spec16,
        &[],
        true,
        false
    ));
    assert!(!surechantillonneur_admis_a_l_ouverture(
        spec16,
        &[],
        false,
        true
    ));
    assert!(surechantillonneur_admis_a_l_ouverture(
        spec24, &pcm24, false, false
    ));
    // Amorce trop courte pour écarter un porteur : refusé.
    assert!(!surechantillonneur_admis_a_l_ouverture(
        spec24,
        &pcm24[..30],
        false,
        false
    ));
    // Un porteur DoP (marqueurs 0x05 / 0xFA alternés dans l'octet de poids fort).
    let mut dop = Vec::new();
    for n in 0..super::DOP_DETECT_FRAMES {
        let marqueur = if n % 2 == 0 { 0x05 } else { 0xFA };
        for _ in 0..2 {
            dop.extend_from_slice(&[0x69, 0x69, marqueur]);
        }
    }
    assert!(!surechantillonneur_admis_a_l_ouverture(
        spec24, &dop, false, false
    ));
}
