//! #6230 / #6451 — première étape du pilotage des voies sur les transports
//! EXCLUSIFS (WASAPI exclusif, ASIO, CoreAudio « hog »).
//!
//! #6601 (rc7) a fait ouvrir le chemin cpal partagé sur les sorties de la
//! grille de réaffectation : une grille 6 → 2 sur un 5.1 ouvre 2 voies. Les
//! bras exclusifs ouvrent encore au format de la source, parce que leur étage
//! supposait « format ouvert = format source ». Ces témoins jugent ce que
//! cette étape change, sur des sorties factices, sans aucun son :
//!
//! - l'étage natif (WASAPI, et ASIO par `EtageNatifAsio`) ouvert sur 2 voies
//!   pour une source 6 canaux livre des blocs de 2 voies, matrice appliquée,
//!   et le déclare ;
//! - l'étage flottant du bras CoreAudio, ouvert sur 2 voies, applique la
//!   matrice (c'était déjà vrai) ;
//! - la frontière gapless « à format égal » des trois bras enchaîne la piste
//!   5.1 suivante dans le flux ouvert sur 2 voies quand la grille le veut, et
//!   rouvre sinon ;
//! - la décision des voies à ouvrir (`voies_a_ouvrir_en_exclusif`) : celles
//!   de la grille, sinon celles de la source — sans grille, rien ne change.
//!
//! L'ouverture des bras eux-mêmes (WASAPI, ASIO, CoreAudio, compilés sous
//! Windows ou macOS seulement) n'est PAS branchée dans cette étape.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::chaine_native::FrontiereExclusive;
use super::chaine_par_la_boucle::{ContratDuSignal, EtageNatifAsio};
use super::etage_natif::{
    EcritureNative, EtageNatif, mots_natifs_du_bloc, spec_du_puits_natif_vers,
};
use super::{
    CreneauDisposition, CreneauReaffectation, Etage, EtageDeConversion, LocalPcmKind,
    LocalPcmProcessor, SANS_DISPOSITION, pcm_bytes_to_native_i32,
};
use crate::audio::reaffectation_canaux::{ChannelRemapSettings, Matrice};
use crate::outputs::traits::{
    AudioSpec, BlocPcm, FormatOuvert, OutputSignalPathStatus, ProfondeurPcm, PuitsNatif,
};

/// La grille de Didier (fil 2202) : 5.1 → 2, FL et FR à 0 dB, le reste muet.
fn grille_6_vers_2() -> Arc<Matrice> {
    let mut gains_db = vec![vec![None; 6]; 2];
    gains_db[0][0] = Some(0.0);
    gains_db[1][1] = Some(0.0);
    Arc::new(
        Matrice::du_reglage_arme(&ChannelRemapSettings {
            enabled: true,
            inputs: 6,
            outputs: 2,
            gains_db,
            normalize: false,
            preset: None,
        })
        .expect("grille 6 → 2 armée"),
    )
}

fn creneau_6_vers_2() -> CreneauReaffectation {
    let creneau = CreneauReaffectation::vide();
    creneau.poser(Some(grille_6_vers_2()));
    creneau
}

fn cinq_un(profondeur: ProfondeurPcm) -> AudioSpec {
    AudioSpec::nouvelle(48_000, profondeur, 6).expect("spec")
}

/// `trames` trames 16 bits, 6 voies : la voie `c` vaut `1000 × (c + 1)`.
fn pcm_5_1_16_bits(trames: usize) -> Vec<u8> {
    (0..trames)
        .flat_map(|_| (0..6).map(|c| (1000 * (c + 1)) as i16))
        .flat_map(|v| v.to_le_bytes())
        .collect()
}

/// Un puits factice qui garde les mots, et refuse un bloc d'un autre format
/// que celui de l'ouverture (même contrat que `PuitsAnneauNatif`).
struct Puits {
    spec: AudioSpec,
    mots: Vec<i32>,
}

impl PuitsNatif for Puits {
    fn ecrire(&mut self, bloc: BlocPcm<'_>) -> bool {
        if bloc.spec() != self.spec {
            return false;
        }
        self.mots.extend(mots_natifs_du_bloc(bloc));
        true
    }
}

struct Zone {
    volume: AtomicU32,
    user_volume: AtomicU32,
    rg_factor: AtomicU32,
    eq: std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: AtomicBool,
    mono_downmix: AtomicBool,
    dop_active: AtomicBool,
    signal_path_status: std::sync::Mutex<Option<OutputSignalPathStatus>>,
}

impl Zone {
    fn au_repos() -> Self {
        Self {
            volume: AtomicU32::new(1000),
            user_volume: AtomicU32::new(1000),
            rg_factor: AtomicU32::new(1000),
            eq: std::sync::Mutex::new(None),
            convolver: std::sync::Mutex::new(None),
            crossfeed: std::sync::Mutex::new(None),
            pure_bypass: AtomicBool::new(false),
            mono_downmix: AtomicBool::new(false),
            dop_active: AtomicBool::new(false),
            signal_path_status: std::sync::Mutex::new(None),
        }
    }

    /// L'étage natif (bras WASAPI) ouvert sur `voies` voies.
    fn etage_natif<'a>(
        &'a self,
        spec: AudioSpec,
        voies: u16,
        reaffectation: &'a CreneauReaffectation,
    ) -> EtageNatif<'a> {
        EtageNatif::monter(
            spec,
            FormatOuvert::new(spec.cadence(), voies),
            &self.volume,
            &self.eq,
            &self.convolver,
            &self.crossfeed,
            &self.pure_bypass,
            &self.mono_downmix,
        )
        .avec_l_adaptation_des_canaux(reaffectation, &SANS_DISPOSITION)
    }

    /// L'étage de la route native du bras ASIO, monté comme le bras le monte.
    fn etage_asio<'a>(
        &'a self,
        spec: AudioSpec,
        voies: u16,
        reaffectation: &'a CreneauReaffectation,
    ) -> EtageNatifAsio<'a> {
        EtageNatifAsio {
            etage: self.etage_natif(spec, voies, reaffectation),
            recu: Vec::new(),
            contrat: ContratDuSignal {
                signal_path_status: &self.signal_path_status,
                transport_natif: true,
                dop_active: &self.dop_active,
                volume: &self.volume,
                user_volume: &self.user_volume,
                rg_factor: &self.rg_factor,
                eq: &self.eq,
                convolver: &self.convolver,
                crossfeed: &self.crossfeed,
                pure_bypass: &self.pure_bypass,
                mono_downmix: &self.mono_downmix,
                bit_perfect_state: None,
            },
            reliquat_force_brut: None,
        }
    }

    /// L'étage flottant du bras CoreAudio exclusif, ouvert sur `voies` voies.
    fn etage_coreaudio<'a>(
        &'a self,
        spec: AudioSpec,
        voies: u16,
        reaffectation: &'a CreneauReaffectation,
        disposition: &'a CreneauDisposition,
    ) -> EtageDeConversion<'a> {
        EtageDeConversion {
            pcm: LocalPcmProcessor {
                eq: &self.eq,
                convolver: &self.convolver,
                crossfeed: &self.crossfeed,
                pure_bypass: &self.pure_bypass,
                mono_downmix: &self.mono_downmix,
                disposition,
                reaffectation,
                dop_active: &self.dop_active,
                volume: &self.volume,
                user_volume: &self.user_volume,
                rg_factor: &self.rg_factor,
            },
            en_attente: Vec::new(),
            resampler: None,
            resample_leftover: Vec::new(),
            pcm_kind: LocalPcmKind::for_bit_depth(spec.profondeur().bits_declares()),
            spec,
            sortie: FormatOuvert::new(spec.cadence(), voies),
            needs_resample: false,
        }
    }
}

/// Les trames de 2 voies attendues pour la grille de Didier : FL et FR de la
/// source, rien d'autre.
fn fl_fr_attendus(trames: usize) -> Vec<i32> {
    pcm_bytes_to_native_i32(
        &(0..trames)
            .flat_map(|_| [1000i16, 2000])
            .flat_map(|v| v.to_le_bytes())
            .collect::<Vec<u8>>(),
        16,
    )
}

/// Bras WASAPI exclusif (Didier, Marantz AV 7706) : grille 6 → 2 armée, piste
/// 5.1, transport ouvert sur 2 voies. L'étage natif livre des blocs de
/// DEUX voies (FL, FR) au puits ouvert sur 2 voies ; il le déclare
/// (adaptation de canaux, traitement), et le créneau note la matrice
/// appliquée. Avant cette étape, l'étage livrait les 6 voies de la source
/// quel que soit le format ouvert : un puits à 2 voies les refusait.
#[test]
fn wasapi_l_etage_natif_ouvert_sur_2_voies_applique_la_grille_6_vers_2_6230() {
    let zone = Zone::au_repos();
    let creneau = creneau_6_vers_2();
    let spec = cinq_un(ProfondeurPcm::Entier16);
    let mut etage = zone.etage_natif(spec, 2, &creneau);
    let mut puits = Puits {
        spec: spec_du_puits_natif_vers(spec, 2),
        mots: Vec::new(),
    };
    assert_eq!(
        etage.decoder_et_pousser(&pcm_5_1_16_bits(64), &mut puits),
        EcritureNative::Poussee {
            trames_source: 64,
            dop: false,
            bit_perfect: false,
        },
        "le puits ouvert sur 2 voies doit accepter les blocs de l'étage"
    );
    assert_eq!(
        puits.mots,
        fl_fr_attendus(64),
        "2 voies, FL et FR de la source : la grille 6 → 2 de Didier"
    );
    assert!(creneau.appliquee(), "le bloc est passé par la matrice");
    let t = etage.transformations();
    assert!(t.adaptation_canaux(), "6 canaux entrés, 2 voies ouvertes");
    assert!(t.dsp_actif(), "la matrice est un traitement du signal");
}

/// Bras ASIO, route native : le même étage, par `EtageNatifAsio`. Le contrat
/// publié n'est plus bit-perfect, et la queue et la frontière suivent les
/// 2 voies.
#[test]
fn asio_la_route_native_ouverte_sur_2_voies_applique_la_grille_6_vers_2_6230() {
    let zone = Zone::au_repos();
    let creneau = creneau_6_vers_2();
    let spec = cinq_un(ProfondeurPcm::Entier16);
    let mut etage = zone.etage_asio(spec, 2, &creneau);
    let mut puits = Puits {
        spec: spec_du_puits_natif_vers(spec, 2),
        mots: Vec::new(),
    };
    etage.recevoir(&pcm_5_1_16_bits(48));
    let _ = etage.pousser(&mut puits, &mut |_, _, _| false, &mut |_| {});
    assert_eq!(puits.mots, fl_fr_attendus(48), "ASIO : FL et FR seuls");
    assert_eq!(
        zone.signal_path_status
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.bit_perfect),
        Some(false),
        "la matrice touche le signal : le contrat publié n'est pas bit-perfect"
    );
    assert!(
        etage.rendre_la_queue_du_dsp(&mut puits),
        "queue vide, puits vivant"
    );
    assert!(
        etage
            .enchainer_a_format_egal(spec, &pcm_5_1_16_bits(64))
            .is_ok(),
        "la piste 5.1 suivante, même grille : elle entre dans le flux à 2 voies"
    );
}

/// Bras CoreAudio « hog » : son étage flottant ouvert sur 2 voies applique la
/// matrice, et sa frontière enchaîne la piste 5.1 suivante au lieu de
/// rouvrir (elle refusait toute adaptation de canaux).
#[test]
fn coreaudio_l_etage_ouvert_sur_2_voies_applique_et_enchaine_la_grille_6_vers_2_6230() {
    let zone = Zone::au_repos();
    let creneau = creneau_6_vers_2();
    let spec = cinq_un(ProfondeurPcm::Entier16);
    let disposition: CreneauDisposition = std::sync::Mutex::new(None);
    let mut etage = zone.etage_coreaudio(spec, 2, &creneau, &disposition);
    let rendu = etage.convertir(vec![0.1, 0.2, 0.3, 0.4, 0.5, 0.6]);
    assert_eq!(rendu, [0.1, 0.2], "CoreAudio : FL et FR seuls");
    assert!(
        etage
            .enchainer_a_format_egal(spec, &pcm_5_1_16_bits(64))
            .is_ok(),
        "la piste 5.1 suivante, même grille, entre dans le flux à 2 voies"
    );

    // Sans grille, le même étage (ouvert sur 2 voies par le périphérique)
    // rouvre, comme avant.
    creneau.poser(None);
    assert!(
        etage
            .enchainer_a_format_egal(spec, &pcm_5_1_16_bits(64))
            .is_err(),
        "sans grille, 6 canaux ne s'enchaînent pas dans 2 voies"
    );
}

/// La frontière gapless « à format égal » de l'étage natif : la suivante
/// entre dans le flux ouvert sur 2 voies quand ELLE AUSSI ouvrirait 2 voies,
/// et seulement alors. Un 24 bits dont l'amorce est un DoP, ou trop courte
/// pour l'écarter, rouvre ; une grille retirée, ou PURE, rouvre.
#[test]
fn la_frontiere_exclusive_suit_les_voies_de_la_grille_6230() {
    let zone = Zone::au_repos();
    let creneau = creneau_6_vers_2();
    let pcm16 = cinq_un(ProfondeurPcm::Entier16);
    assert!(
        zone.etage_natif(pcm16, 2, &creneau)
            .enchainer_la_piste(pcm16, &pcm_5_1_16_bits(64))
            .is_ok(),
        "même format, même grille : on enchaîne"
    );

    let pcm24 = cinq_un(ProfondeurPcm::Entier24);
    let pcm_24: Vec<u8> = (0..64 * 6)
        .flat_map(|i| [0x10, 0x20, (i % 7) as u8])
        .collect();
    assert!(
        zone.etage_natif(pcm24, 2, &creneau)
            .enchainer_la_piste(pcm24, &pcm_24)
            .is_ok(),
        "24 bits PCM : on enchaîne"
    );
    let dop: Vec<u8> = (0..64)
        .flat_map(|t| {
            let marqueur = if t % 2 == 0 { 0x05 } else { 0xFA };
            (0..6).flat_map(move |_| [0x69, 0x69, marqueur])
        })
        .collect();
    assert!(
        super::is_dop_pcm(&dop, 24, 6),
        "prémisse : l'amorce est un porteur DoP"
    );
    assert!(
        zone.etage_natif(pcm24, 2, &creneau)
            .enchainer_la_piste(pcm24, &dop)
            .is_err(),
        "DoP : la grille ne s'applique pas, la suivante ouvrirait 6 voies"
    );
    assert!(
        zone.etage_natif(pcm24, 2, &creneau)
            .enchainer_la_piste(pcm24, &pcm_24[..60])
            .is_err(),
        "amorce trop courte pour écarter le DoP : on rouvre"
    );

    zone.pure_bypass.store(true, Ordering::Relaxed);
    assert!(
        zone.etage_natif(pcm16, 2, &creneau)
            .enchainer_la_piste(pcm16, &pcm_5_1_16_bits(64))
            .is_err(),
        "PURE : pas de grille, la suivante ouvrirait 6 voies"
    );
    zone.pure_bypass.store(false, Ordering::Relaxed);

    creneau.poser(None);
    assert!(
        zone.etage_natif(pcm16, 2, &creneau)
            .enchainer_la_piste(pcm16, &pcm_5_1_16_bits(64))
            .is_err(),
        "grille retirée : 6 canaux ne s'enchaînent pas dans 2 voies"
    );
    assert!(
        zone.etage_natif(pcm16, 6, &creneau)
            .enchainer_la_piste(pcm16, &pcm_5_1_16_bits(64))
            .is_ok(),
        "sans grille, ouvert au format source : la règle d'avant, inchangée"
    );
}

/// Relecture rc7 — grille armée, étage ouvert au FORMAT SOURCE : c'est ce que
/// font les trois bras exclusifs tant que l'étape 2 n'est pas faite (ils
/// ouvrent encore aux canaux de la source). La frontière garde alors la règle
/// « à format égal » d'avant : la piste 5.1 suivante s'enchaîne dans le flux
/// ouvert sur 6 voies, au lieu de rouvrir le périphérique à chaque piste (un
/// trou ou un clic entre les pistes d'un album 5.1).
#[test]
fn grille_armee_etage_ouvert_au_format_source_on_enchaine_comme_avant_6230() {
    let zone = Zone::au_repos();
    let creneau = creneau_6_vers_2();
    let spec = cinq_un(ProfondeurPcm::Entier16);
    assert!(
        zone.etage_natif(spec, 6, &creneau)
            .enchainer_la_piste(spec, &pcm_5_1_16_bits(64))
            .is_ok(),
        "WASAPI : grille armée, étage ouvert au format source : on enchaîne"
    );
    assert!(
        zone.etage_asio(spec, 6, &creneau)
            .enchainer_a_format_egal(spec, &pcm_5_1_16_bits(64))
            .is_ok(),
        "ASIO : grille armée, étage ouvert au format source : on enchaîne"
    );
    let disposition: CreneauDisposition = std::sync::Mutex::new(None);
    assert!(
        zone.etage_coreaudio(spec, 6, &creneau, &disposition)
            .enchainer_a_format_egal(spec, &pcm_5_1_16_bits(64))
            .is_ok(),
        "CoreAudio : grille armée, étage ouvert au format source : on enchaîne"
    );
}

/// La décision des voies qu'un transport exclusif ouvre : la grille quand elle
/// pilote, la source sinon. Sans grille, le format source (rien ne change).
#[test]
fn les_voies_a_ouvrir_en_exclusif_suivent_la_grille_6230() {
    let creneau = CreneauReaffectation::vide();
    let pcm16 = cinq_un(ProfondeurPcm::Entier16);
    let amorce = pcm_5_1_16_bits(64);
    assert_eq!(
        creneau.voies_a_ouvrir_en_exclusif(pcm16, &amorce, false, false),
        6,
        "sans grille : les canaux de la source"
    );
    creneau.poser(Some(grille_6_vers_2()));
    assert_eq!(
        creneau.voies_a_ouvrir_en_exclusif(pcm16, &amorce, false, false),
        2,
        "la grille 6 → 2 pilote : le transport exclusif doit ouvrir 2 voies (#6230)"
    );
    assert_eq!(
        creneau.voies_a_ouvrir_en_exclusif(pcm16, &amorce, true, false),
        6,
        "PURE"
    );
    assert_eq!(
        creneau.voies_a_ouvrir_en_exclusif(pcm16, &amorce, false, true),
        6,
        "DoP"
    );
    let stereo = AudioSpec::nouvelle(48_000, ProfondeurPcm::Entier16, 2).unwrap();
    assert_eq!(
        creneau.voies_a_ouvrir_en_exclusif(stereo, &amorce, false, false),
        2,
        "une piste stéréo ne relève pas d'une grille 6 → 2"
    );
}

/// Un porteur DoP qui surviendrait malgré tout sur des voies pilotées ne part
/// pas en bruit plein échelle : silence, sur les voies ouvertes (#3233).
#[test]
fn un_porteur_dop_sur_des_voies_pilotees_part_en_silence_6230() {
    let zone = Zone::au_repos();
    let creneau = creneau_6_vers_2();
    let spec = cinq_un(ProfondeurPcm::Entier24);
    let dop: Vec<u8> = (0..64)
        .flat_map(|t| {
            let marqueur = if t % 2 == 0 { 0x05 } else { 0xFA };
            (0..6).flat_map(move |_| [0x69, 0x69, marqueur])
        })
        .collect();
    let mut etage = zone.etage_natif(spec, 2, &creneau);
    let mut puits = Puits {
        spec: spec_du_puits_natif_vers(spec, 2),
        mots: Vec::new(),
    };
    assert!(matches!(
        etage.decoder_et_pousser(&dop, &mut puits),
        EcritureNative::Poussee {
            trames_source: 64,
            dop: false,
            bit_perfect: false,
        }
    ));
    assert_eq!(puits.mots, vec![0; 64 * 2], "silence sur 2 voies");
}

/// Le reliquat 24 bits jamais classé (flux de moins de 32 trames), sur des
/// voies pilotées, part sur les voies OUVERTES : il ne peut pas partir brut à
/// 6 voies dans un puits à 2.
#[test]
fn le_reliquat_24_bits_suit_les_voies_pilotees_6230() {
    let zone = Zone::au_repos();
    let creneau = creneau_6_vers_2();
    let spec = cinq_un(ProfondeurPcm::Entier24);
    let mut etage = zone.etage_natif(spec, 2, &creneau);
    let mut puits = Puits {
        spec: spec_du_puits_natif_vers(spec, 2),
        mots: Vec::new(),
    };
    let court: Vec<u8> = (0..10 * 6).flat_map(|i| [0, 0x10, (i % 6) as u8]).collect();
    assert_eq!(
        etage.decoder_et_pousser(&court, &mut puits),
        EcritureNative::RienAPousser
    );
    let reliquat = etage.vider(&mut puits).expect("reliquat");
    assert_eq!(reliquat.trames, 10);
    assert_eq!(puits.mots.len(), 10 * 2, "10 trames de 2 voies");
}
