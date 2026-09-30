//! #5204, seconde tranche — **ASIO exclusif** : le blanc entre deux pistes de
//! même format.
//!
//! Le correctif WASAPI (#5229, 0.9.167) laissait ASIO de côté : « ASIO et
//! CoreAudio exclusifs restent non enchaînables (bras inchangés) ». Sur ASIO,
//! `sait_enchainer` rendait donc `false` : le sondeur n'armait jamais le
//! gapless, `bras_asio.rs` sortait à l'EOF sans consommer la suivante, et
//! chaque changement de piste fermait puis rouvrait le pilote au MÊME format.
//!
//! Le bras ASIO ne se compile qu'avec la feature `asio` sous Windows. Ce qui
//! est jugé ici, sur Linux, c'est ce qu'il appelle, sans rien de factice
//! entre les deux : son étage natif (`EtageNatifAsio`, déplacé tel quel), la
//! boucle commune (`BoucleProducteur::tourner`), la frontière partagée avec
//! WASAPI (`accepter_la_suivante`), et un puits de capture natif.

use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
use std::sync::mpsc;

use super::chaine_native::{FinDeChaine, ReserveDeLaChaine, Suivante};
use super::chaine_par_la_boucle::{
    ContratDuSignal, EtageNatifAsio, poursuivre_la_chaine_par_la_boucle,
};
use super::enchainement_exclusif::{BrasDeLecture, bras_de_lecture, lire_l_entete_enchainee};
use super::etage_natif::{EtageNatif, spec_du_puits_natif};
use super::{
    AudioSpec, BoucleProducteur, CompteursDePiste, Etage, FinDeBoucle, FormatOuvert,
    OutputSignalPathStatus, RoleDeLaBoucle,
};
use crate::outputs::traits::{CaptureOutputNatif, ProfondeurPcm};

// ─── La capacité publiée ────────────────────────────────────────────────────

/// LE témoin de la capacité : un pilote ASIO en mode exclusif annonce
/// l'enchaînement interne. Sur `main` (0.9.168), `sait_enchainer` rendait
/// `false` pour `AsioExclusif` : le sondeur n'armait jamais le gapless, et le
/// bras ne voyait jamais de piste en réserve.
#[test]
fn le_bras_asio_annonce_l_enchainement_interne_5204() {
    let bras = bras_de_lecture("windows", true, true, "asio");
    assert_eq!(bras, BrasDeLecture::AsioExclusif);
    assert!(
        bras.sait_enchainer(),
        "#5204 : en ASIO exclusif, la sortie doit annoncer l'enchaînement \
         interne — sinon le sondeur n'arme pas le gapless et chaque piste \
         ferme puis rouvre le pilote au même format"
    );
    // #5451 : CoreAudio « hog » enchaîne aussi, par la même poursuite —
    // voir `gapless_coreaudio_5451.rs`.
}

// ─── La chaîne de la route native ───────────────────────────────────────────

/// Ce que `play_url` prête à l'étage et au contrat du signal.
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

    fn etage_natif(&self, spec: AudioSpec) -> EtageNatif<'_> {
        EtageNatif::monter(
            spec,
            FormatOuvert::new(spec.cadence(), spec.canaux()),
            &self.volume,
            &self.eq,
            &self.convolver,
            &self.crossfeed,
            &self.pure_bypass,
            &self.mono_downmix,
        )
    }

    /// L'étage de la route native du bras ASIO, monté comme le bras le monte.
    fn etage_asio(&self, spec: AudioSpec) -> EtageNatifAsio<'_> {
        EtageNatifAsio {
            etage: self.etage_natif(spec),
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
}

/// Les témoins que la boucle commune consulte. Partagés avec
/// `gapless_coreaudio_5451.rs` (#5451), qui juge la même poursuite.
pub(super) struct Temoins {
    pub(super) arret: AtomicBool,
    disparu: AtomicBool,
    pub(super) position: AtomicU64,
    duree: AtomicU64,
    pub(super) constat: std::sync::Mutex<Option<String>>,
    _tx: mpsc::Sender<()>,
    rx: mpsc::Receiver<()>,
}

impl Temoins {
    pub(super) fn neufs() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            arret: AtomicBool::new(false),
            disparu: AtomicBool::new(false),
            position: AtomicU64::new(0),
            duree: AtomicU64::new(0),
            constat: std::sync::Mutex::new(None),
            _tx: tx,
            rx,
        }
    }

    pub(super) fn boucle(&self, role: RoleDeLaBoucle) -> BoucleProducteur<'_> {
        BoucleProducteur {
            role,
            backend: "ASIO",
            device_name: "Fireface UCX",
            cle_de_flux: None,
            stop_rx: &self.rx,
            force_silent: &self.arret,
            device_gone: &self.disparu,
            position_ms: &self.position,
            open_failure: &self.constat,
            debut_du_flux: std::time::Instant::now(),
            duree_de_la_piste_ms: &self.duree,
            cretes_de_sortie: None,
        }
    }
}

/// Un WAV PCM entier canonique, en-tête de 44 octets puis `pcm`.
pub(super) fn wav(cadence: u32, bits: u16, canaux: u16, pcm: &[u8]) -> Vec<u8> {
    let block_align = canaux * bits / 8;
    let mut w = Vec::new();
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(b"WAVE");
    w.extend_from_slice(b"fmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&canaux.to_le_bytes());
    w.extend_from_slice(&cadence.to_le_bytes());
    w.extend_from_slice(&(cadence * u32::from(block_align)).to_le_bytes());
    w.extend_from_slice(&block_align.to_le_bytes());
    w.extend_from_slice(&bits.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(pcm.len() as u32).to_le_bytes());
    w.extend_from_slice(pcm);
    w
}

pub(super) fn pcm(octets: usize, graine: usize) -> Vec<u8> {
    (0..octets)
        .map(|i| (i * 31 + graine * 7 + 3) as u8)
        .collect()
}

pub(super) fn spec(cadence: u32, profondeur: ProfondeurPcm, canaux: u16) -> AudioSpec {
    AudioSpec::nouvelle(cadence, profondeur, canaux).unwrap()
}

/// La réserve de `set_next_media`, en mémoire.
#[derive(Default)]
pub(super) struct ReserveFactice {
    pub(super) reserve: VecDeque<Vec<u8>>,
    pub(super) enchainements: u32,
}

impl ReserveDeLaChaine for ReserveFactice {
    type Lecteur = Cursor<Vec<u8>>;

    fn silence_force(&self) -> bool {
        false
    }

    fn preparer_la_suivante(&mut self) -> Suivante<Self::Lecteur> {
        let Some(octets) = self.reserve.pop_front() else {
            return Suivante::Aucune;
        };
        let mut lecteur = Cursor::new(octets);
        match lire_l_entete_enchainee(&mut lecteur, &AtomicBool::new(false)) {
            Ok(entete) => Suivante::Prete { lecteur, entete },
            Err(_) => Suivante::Refusee,
        }
    }

    fn piste_enchainee(&mut self) {
        self.enchainements += 1;
    }
}

/// Ce que le bras fait pour la piste initiale : pousser l'amorce, puis la
/// lire par la boucle commune jusqu'à sa fin de flux. Rend les trames.
fn jouer_la_piste_initiale(
    temoins: &Temoins,
    etage: &mut EtageNatifAsio<'_>,
    puits: &mut CaptureOutputNatif,
    wav_initial: Vec<u8>,
) -> u64 {
    let mut lecteur = Cursor::new(wav_initial);
    let entete = lire_l_entete_enchainee(&mut lecteur, &AtomicBool::new(false)).unwrap();
    let mut compteurs = CompteursDePiste {
        total_bytes_read: 0,
        total_frames_fed: 0,
        seek_offset: 0,
        skip_bytes: 0,
        skipped_bytes: 0,
        premiere_donnee_journalisee: false,
    };
    etage.recevoir(entete.amorce());
    if let super::PousseeVersLePuits::Poussee { trames_source } =
        etage.pousser(puits, &mut |_, _, _| false, &mut |_| {})
    {
        compteurs.total_frames_fed += trames_source;
    }
    let fin = temoins.boucle(RoleDeLaBoucle::PisteInitiale).tourner(
        &mut lecteur,
        &mut [0; 4096],
        etage,
        puits,
        &mut |_, _, _| false,
        &mut compteurs,
        &mut |_| true,
    );
    assert!(
        matches!(fin, FinDeBoucle::FinDeFlux),
        "la piste A va au bout"
    );
    compteurs.total_frames_fed
}

/// LE témoin de l'enchaînement ASIO : deux pistes de même format passent dans
/// le MÊME puits — celui du pilote ouvert — sans qu'il soit refermé, et le
/// puits reçoit exactement les mots des deux pistes, bout à bout.
///
/// Avant la correction, le bras rendait la main à l'EOF de A : B n'atteignait
/// jamais ce puits (le sondeur la relançait par `play_url`, qui rouvrait le
/// pilote).
#[test]
fn asio_deux_pistes_de_meme_format_passent_dans_le_meme_flux_5204() {
    let zone = Zone::au_repos();
    let temoins = Temoins::neufs();
    let format = spec(48_000, ProfondeurPcm::Entier16, 2);
    let pcm_a = pcm(4 * 3_000, 1);
    let pcm_b = pcm(4 * 2_000, 2);

    let mut puits = CaptureOutputNatif::ouvrir(spec_du_puits_natif(format));
    let mut etage = zone.etage_asio(format);
    let trames_a =
        jouer_la_piste_initiale(&temoins, &mut etage, &mut puits, wav(48_000, 16, 2, &pcm_a));
    assert_eq!(trames_a, 3_000);
    let mut reserve = ReserveFactice::default();
    reserve.reserve.push_back(wav(48_000, 16, 2, &pcm_b));

    let issue = poursuivre_la_chaine_par_la_boucle(
        &mut reserve,
        &mut etage,
        &mut puits,
        &temoins.boucle(RoleDeLaBoucle::PisteEnchainee),
        &mut [0; 4096],
        |lecteur| lecteur,
        trames_a,
    );

    assert_eq!(
        issue.pistes_enchainees, 1,
        "#5204 : en ASIO, la piste suivante de même format doit être \
         enchaînée dans le flux ouvert, pas laissée au sondeur (fermeture + \
         réouverture du pilote)"
    );
    assert_eq!(
        reserve.enchainements, 1,
        "le morceau suivant est publié une fois"
    );
    assert_eq!(issue.fin, FinDeChaine::RienEnReserve);
    assert!(issue.http_eof, "la dernière piste a atteint sa fin de flux");
    assert_eq!(issue.trames, 2_000, "la position est celle de la piste B");
    assert_eq!(
        puits.trames(),
        5_000,
        "le puits du pilote ouvert a reçu A puis B"
    );
    assert_eq!(puits.blocs_refuses(), 0, "le puits n'a jamais été tué");
    assert_eq!(
        temoins.position.load(std::sync::atomic::Ordering::SeqCst),
        41,
        "la boucle commune a publié la position DANS la piste B (2 000 trames \
         à 48 kHz)"
    );
    assert!(
        temoins.constat.lock().unwrap().is_none(),
        "aucun constat de famine ni de piste tronquée"
    );

    // Bout à bout, à l'octet près : la même chose qu'une seule piste A+B.
    let mut temoin = CaptureOutputNatif::ouvrir(spec_du_puits_natif(format));
    let mut etage_temoin = zone.etage_natif(format);
    let a_puis_b: Vec<u8> = pcm_a.iter().chain(pcm_b.iter()).copied().collect();
    etage_temoin.decoder_et_pousser(&a_puis_b, &mut temoin);
    assert_eq!(puits.empreinte(), temoin.empreinte());
}

/// La contrepartie : à format différent, la suivante n'entre PAS dans le
/// flux ouvert. La chaîne rend la main en le disant — la fin naturelle
/// rouvrira le pilote au nouveau format, comme avant #5204.
#[test]
fn asio_une_piste_d_un_autre_format_n_entre_pas_dans_le_flux_5204() {
    let zone = Zone::au_repos();
    let temoins = Temoins::neufs();
    let format = spec(48_000, ProfondeurPcm::Entier16, 2);
    let mut puits = CaptureOutputNatif::ouvrir(spec_du_puits_natif(format));
    let mut etage = zone.etage_asio(format);
    let trames_a = jouer_la_piste_initiale(
        &temoins,
        &mut etage,
        &mut puits,
        wav(48_000, 16, 2, &pcm(4 * 1_000, 1)),
    );
    let mut reserve = ReserveFactice::default();
    reserve
        .reserve
        .push_back(wav(44_100, 16, 2, &pcm(4 * 1_000, 2)));

    let issue = poursuivre_la_chaine_par_la_boucle(
        &mut reserve,
        &mut etage,
        &mut puits,
        &temoins.boucle(RoleDeLaBoucle::PisteEnchainee),
        &mut [0; 4096],
        |lecteur| lecteur,
        trames_a,
    );

    assert_eq!(issue.fin, FinDeChaine::FormatDifferent);
    assert!(issue.http_eof, "la piste A s'est terminée normalement");
    assert_eq!(issue.pistes_enchainees, 0);
    assert_eq!(reserve.enchainements, 0, "le morceau courant reste publié");
    assert_eq!(issue.trames, 1_000);
    assert_eq!(
        puits.trames(),
        1_000,
        "rien de la piste B dans le flux ouvert"
    );
    assert_eq!(etage.etage.spec(), format, "l'étage n'a pas été touché");
}

/// Trois pistes au format du relevé de #5204 (88,2 kHz / 32 bits) : la chaîne
/// ne s'arrête pas à la deuxième, et rien en réserve la termine proprement.
#[test]
fn asio_la_chaine_continue_tant_que_le_format_ne_change_pas_5204() {
    let zone = Zone::au_repos();
    let temoins = Temoins::neufs();
    let format = spec(88_200, ProfondeurPcm::Entier32, 2);
    let mut puits = CaptureOutputNatif::ouvrir(spec_du_puits_natif(format));
    let mut etage = zone.etage_asio(format);
    let trames_a = jouer_la_piste_initiale(
        &temoins,
        &mut etage,
        &mut puits,
        wav(88_200, 32, 2, &pcm(8 * 700, 1)),
    );
    let mut reserve = ReserveFactice::default();
    reserve
        .reserve
        .push_back(wav(88_200, 32, 2, &pcm(8 * 600, 2)));
    reserve
        .reserve
        .push_back(wav(88_200, 32, 2, &pcm(8 * 500, 3)));

    let issue = poursuivre_la_chaine_par_la_boucle(
        &mut reserve,
        &mut etage,
        &mut puits,
        &temoins.boucle(RoleDeLaBoucle::PisteEnchainee),
        &mut [0; 4096],
        |lecteur| lecteur,
        trames_a,
    );

    assert_eq!(issue.pistes_enchainees, 2);
    assert_eq!(issue.fin, FinDeChaine::RienEnReserve);
    assert_eq!(puits.trames(), 1_800);
    assert_eq!(issue.trames, 500);
}

/// Un arrêt pendant la piste enchaînée : la boucle commune le voit, la chaîne
/// s'arrête SANS fin naturelle (on n'enchaîne pas la file sur un arrêt).
#[test]
fn asio_un_arret_pendant_la_piste_enchainee_n_est_pas_une_fin_naturelle_5204() {
    let zone = Zone::au_repos();
    let temoins = Temoins::neufs();
    let format = spec(48_000, ProfondeurPcm::Entier16, 2);
    let mut puits = CaptureOutputNatif::ouvrir(spec_du_puits_natif(format));
    let mut etage = zone.etage_asio(format);
    let trames_a = jouer_la_piste_initiale(
        &temoins,
        &mut etage,
        &mut puits,
        wav(48_000, 16, 2, &pcm(4 * 500, 1)),
    );
    let mut reserve = ReserveFactice::default();
    reserve
        .reserve
        .push_back(wav(48_000, 16, 2, &pcm(4 * 500, 2)));

    // `stop()` tombe au moment où la pompe de B démarre.
    let issue = poursuivre_la_chaine_par_la_boucle(
        &mut reserve,
        &mut etage,
        &mut puits,
        &temoins.boucle(RoleDeLaBoucle::PisteEnchainee),
        &mut [0; 4096],
        |lecteur| {
            temoins
                .arret
                .store(true, std::sync::atomic::Ordering::SeqCst);
            lecteur
        },
        trames_a,
    );

    assert_eq!(issue.fin, FinDeChaine::Interrompue);
    assert!(!issue.http_eof, "un arrêt n'est pas une fin naturelle");
    assert_eq!(issue.pistes_enchainees, 1);
}

// ─── Le branchement dans le bras ────────────────────────────────────────────

fn compact(src: &str) -> String {
    src.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Le bras ASIO n'est compilé que par l'étape « ASIO » de la CI : cette garde
/// dit, sur Linux, qu'il appelle bien la chaîne sur sa route native, qu'il
/// lève `chain_exhausted` en fin de chaîne ET dès l'ouverture sur sa route
/// traitée, et que `play_url` lui confie la réserve.
#[test]
fn branchement_5204_le_bras_asio_enchaine_sur_sa_route_native() {
    let bras = compact(include_str!("bras_asio.rs"));
    assert!(
        bras.contains("ifhttp_eof_asio&&letRoute::Native{etage,puits}=&mutroute{"),
        "la chaîne ne part que de la route native, à la fin de flux"
    );
    assert!(bras.contains("poursuivre_la_chaine_par_la_boucle(&mutreserve,etage,"));
    assert!(
        bras.contains("ifmatches!(route,Route::Flottante{..})&&doit_declarer_chaine_epuisee("),
        "la route traitée se déclare non enchaînable dès l'ouverture"
    );
    assert_eq!(
        bras.matches("chain_exhausted.store(true,Ordering::SeqCst);")
            .count(),
        2,
        "route traitée à l'ouverture, et fin de chaîne"
    );

    let local = compact(include_str!("../local.rs"));
    let appel = local
        .find("bras_asio::jouer_via_asio(bras_asio::EntreesAsio{")
        .expect("l'appel du bras ASIO");
    let fin = local[appel..].find("});").expect("fin de l'appel") + appel;
    assert!(
        local[appel..fin]
            .contains("next_media:next_media_ref,chain_exhausted:chain_exhausted_ref,"),
        "play_url confie la réserve au bras ASIO"
    );
}
