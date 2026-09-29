//! #5204 — le bras ASIO exclusif enchaîne à format égal, par la boucle
//! commune.
//!
//! Jusqu'ici `bras_asio.rs` lisait sa piste jusqu'à l'EOF puis rendait la
//! main sans consommer le `next_media` préparé ; `sait_enchainer` le déclarait
//! donc incapable d'enchaîner, le sondeur n'armait jamais le gapless, et
//! chaque changement de piste fermait puis rouvrait le pilote ASIO au MÊME
//! format — le blanc de #5204, que le correctif WASAPI (#5229) laissait sur ce
//! bras.
//!
//! Ce module porte ce que le bras ASIO fait sur sa **route native** (le pilote
//! prend des mots entiers), sorti de `bras_asio.rs` pour être jugé sur Linux :
//! ce fichier-là ne se compile qu'avec la feature `asio` sous Windows, et ni
//! Shrek ni le Mac ne le voient.
//!
//! - [`EtageNatifAsio`] et [`ContratDuSignal`] : l'étage natif de #4011 vu par
//!   le trait `Etage`, et le contrat du signal publié après chaque bloc —
//!   déplacés tels quels ;
//! - [`poursuivre_la_chaine_par_la_boucle`] : à la fin de flux d'une piste, la
//!   suivante de même format entre dans le MÊME puits (la frontière est celle
//!   du bras WASAPI, [`accepter_la_suivante`]), et son flux est lu par la
//!   MÊME boucle commune que la piste initiale — famine rapportée, erreurs de
//!   lecture triées, position publiée.
//!
//! La route traitée (`Processed*`, anneau flottant) n'enchaîne pas : le bras
//! le déclare dès l'ouverture (`chain_exhausted`), le sondeur n'arme pas.
//!
//! #5451 — le bras CoreAudio exclusif (macOS, `bras_coreaudio.rs`) enchaîne
//! par la MÊME poursuite : elle est générique sur [`EtageDeLaChaine`], que
//! l'étage natif d'ASIO et l'étage flottant au format identité de CoreAudio
//! ([`EtageDeConversion`]) implémentent l'un et l'autre. `bras_coreaudio.rs`
//! ne se compile que sous macOS ; ce module, lui, est jugé sur Linux.

use std::io::Read;

#[cfg(any(all(target_os = "windows", feature = "asio"), test))]
use super::BlocDecode;
use super::chaine_native::{
    FinDeChaine, FrontiereExclusive, IssueDeLaChaine, ReserveDeLaChaine, accepter_la_suivante,
};
#[cfg(any(all(target_os = "windows", feature = "asio"), test))]
use super::etage_natif::{EcritureNative, EtageNatif};
use super::*;
use super::{BoucleProducteur, CompteursDePiste, Etage, FinDeBoucle, PousseeVersLePuits};
#[cfg(any(all(target_os = "windows", feature = "asio"), test))]
use crate::outputs::traits::{PuitsNatif, TransformationsReelles};

// ───────────────────────────────────────────────────────────────────────────
// Le contrat du signal, publié après chaque bloc — sur les deux routes.
// ───────────────────────────────────────────────────────────────────────────

/// Ce que le bras publiait après chaque bloc poussé : l'état DoP de la piste
/// (`dop_active`, volume synchronisé), et le contrat du chemin du signal
/// (`publish_windows_signal_path_status`, journal
/// `windows_exclusive_signal_contract` — au premier bloc, puis à chaque
/// changement de verdict).
#[cfg(any(all(target_os = "windows", feature = "asio"), test))]
pub(super) struct ContratDuSignal<'a> {
    pub(super) signal_path_status: &'a std::sync::Mutex<Option<OutputSignalPathStatus>>,
    pub(super) transport_natif: bool,
    pub(super) dop_active: &'a AtomicBool,
    pub(super) volume: &'a AtomicU32,
    pub(super) user_volume: &'a AtomicU32,
    pub(super) rg_factor: &'a AtomicU32,
    pub(super) eq: &'a std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    pub(super) convolver: &'a std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    pub(super) crossfeed: &'a std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pub(super) pure_bypass: &'a AtomicBool,
    pub(super) mono_downmix: &'a AtomicBool,
    pub(super) bit_perfect_state: Option<bool>,
}

#[cfg(any(all(target_os = "windows", feature = "asio"), test))]
impl ContratDuSignal<'_> {
    pub(super) fn publier(&mut self, dop: bool, bit_perfect: bool) {
        if self.dop_active.swap(dop, Ordering::SeqCst) != dop {
            info!(dop, "local_audio_dop_stream_state_changed");
            sync_volume_to_dop(self.volume, self.user_volume, self.rg_factor, dop);
        }
        let volume_units = self.volume.load(Ordering::SeqCst);
        let runtime = publish_windows_signal_path_status(
            self.signal_path_status,
            bit_perfect,
            self.transport_natif,
            dop,
            volume_units,
            self.eq,
            self.convolver,
            self.crossfeed,
            self.pure_bypass,
            self.mono_downmix,
        );
        if self.bit_perfect_state != Some(runtime.bit_perfect) {
            self.bit_perfect_state = Some(runtime.bit_perfect);
            info!(
                backend = "ASIO",
                bit_perfect = runtime.bit_perfect,
                dop,
                volume_units,
                reasons = ?runtime.reasons,
                "windows_exclusive_signal_contract"
            );
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// La route native vue par le trait `Etage`.
// ───────────────────────────────────────────────────────────────────────────

/// Le bloc que l'observateur de la boucle voit sur la route native : ce que
/// `EtageNatif::decoder_et_pousser` vient de pousser.
#[cfg(any(all(target_os = "windows", feature = "asio"), test))]
pub(super) struct BlocNatif {
    echantillons: usize,
    non_nul: bool,
}

#[cfg(any(all(target_os = "windows", feature = "asio"), test))]
impl BlocDecode for BlocNatif {
    fn nb_echantillons(&self) -> usize {
        self.echantillons
    }

    fn contient_un_echantillon_non_nul(&self) -> bool {
        self.non_nul
    }
}

/// L'étage natif de #4011 présenté au trait `Etage` de #4013, avec le contrat
/// du signal que le bras publiait après chaque bloc.
///
/// Une enveloppe et non un `impl Etage for EtageNatif` : ce module n'écrit
/// pas dans `etage_natif.rs`, et un second `impl` du même trait sur le même
/// type serait une erreur de compilation le jour où son auteur en pose un.
/// Quand `EtageNatif` implémentera `Etage` lui-même, l'enveloppe ne gardera
/// que le contrat.
///
/// `recevoir` puis `pousser` séparés, comme le trait le demande :
/// `decoder_et_pousser` fait les deux d'un coup, l'enveloppe garde les
/// octets reçus jusqu'à la poussée. La fermeture `refuser_le_porteur_dop` est
/// reçue et ignorée : la route native ne refuse rien, elle porte le DoP et le
/// dit.
#[cfg(any(all(target_os = "windows", feature = "asio"), test))]
pub(super) struct EtageNatifAsio<'a> {
    pub(super) etage: EtageNatif<'a>,
    pub(super) recu: Vec<u8>,
    pub(super) contrat: ContratDuSignal<'a>,
    /// Le reliquat 24 bits forcé brut à l'EOF (`vider`), à compter par le
    /// bras : trames poussées.
    pub(super) reliquat_force_brut: Option<u64>,
}

#[cfg(any(all(target_os = "windows", feature = "asio"), test))]
impl Etage for EtageNatifAsio<'_> {
    type Puits<'p> = dyn PuitsNatif + 'p;
    type Bloc = BlocNatif;

    fn recevoir(&mut self, octets: &[u8]) {
        self.recu.extend_from_slice(octets);
    }

    fn cadence_source(&self) -> u32 {
        self.etage.spec().cadence()
    }

    /// Décoder ce qui est aligné (sonde DoP, volume, DSP, mot natif) et
    /// pousser, puis publier le contrat du bloc. L'observateur voit le bloc
    /// APRÈS la poussée — il ne sert qu'au diagnostic de silence initial.
    fn pousser(
        &mut self,
        puits: &mut (dyn PuitsNatif + '_),
        _refuser_le_porteur_dop: &mut dyn FnMut(bool, u32, u16) -> bool,
        observer: &mut dyn FnMut(&BlocNatif),
    ) -> PousseeVersLePuits {
        let recu = std::mem::take(&mut self.recu);
        let non_nul = recu.iter().any(|&octet| octet != 0);
        let canaux = usize::from(self.etage.spec().canaux());
        match self.etage.decoder_et_pousser(&recu, puits) {
            EcritureNative::RienAPousser => PousseeVersLePuits::RienAPousser,
            EcritureNative::Poussee {
                trames_source,
                dop,
                bit_perfect,
            } => {
                observer(&BlocNatif {
                    echantillons: trames_source as usize * canaux,
                    non_nul,
                });
                self.contrat.publier(dop, bit_perfect);
                PousseeVersLePuits::Poussee { trames_source }
            }
            EcritureNative::PuitsMort { trames_source, .. } => {
                PousseeVersLePuits::PuitsMort { trames_source }
            }
        }
    }

    /// La queue du DSP (#2209) : l'étage vide le convolveur, applique le
    /// volume et quantifie (D3). `EtageNatif::rendre_la_queue` vide avec
    /// `dop = false` ; `flush_local_dsp(…, dop = true)` ne rendait RIEN — un
    /// DoP porté n'a jamais alimenté le convolveur, et un convolveur
    /// configuré rend `latency_frames()` de silence même à vide. Ne pas
    /// l'appeler sur DoP est l'équivalent exact de ce que le bras faisait.
    fn rendre_la_queue_du_dsp(&mut self, puits: &mut (dyn PuitsNatif + '_)) -> bool {
        if self.contrat.dop_active.load(Ordering::Relaxed) {
            return true;
        }
        self.etage.rendre_la_queue(puits)
    }

    /// Fin de flux : le reliquat 24 bits jamais classé part brut
    /// (`windows_exclusive_short_24bit_stream_forced_raw`). Il n'y a pas de
    /// rééchantillonneur à vider sur cette route.
    fn vider(&mut self, puits: &mut (dyn PuitsNatif + '_)) -> bool {
        if let Some(reliquat) = self.etage.vider(puits) {
            info!(
                backend = "ASIO",
                bytes = reliquat.octets,
                bit_perfect = true,
                "windows_exclusive_short_24bit_stream_forced_raw"
            );
            self.reliquat_force_brut = Some(reliquat.trames);
        }
        true
    }

    fn transformations(&self) -> TransformationsReelles {
        self.etage.transformations()
    }
}

// ───────────────────────────────────────────────────────────────────────────
// La chaîne de la route native.
// ───────────────────────────────────────────────────────────────────────────

/// Poursuit la chaîne après la fin de flux de la piste initiale : tant qu'une
/// suivante de même format est en réserve, elle entre dans le MÊME puits et
/// son flux est lu par `producteur` (une boucle de rôle
/// `RoleDeLaBoucle::PisteEnchainee`).
///
/// `sourcer` fait d'un flux de la réserve la source que la boucle lit — pour
/// le bras, la pompe HTTP (`SourcePompee`), qui garde le fil du périphérique
/// hors du réseau. `trames` : les trames déjà poussées de la piste courante.
///
/// Le geste de fin de flux de CHAQUE piste ([`EtageDeLaChaine::finir_la_piste`])
/// est fait ici, la dernière comprise : l'appelant ne le refait pas. La queue du DSP, le
/// signal de fin naturelle et le vidage restent au bras, à la fin de la
/// chaîne — comme sur le bras WASAPI.
pub(super) fn poursuivre_la_chaine_par_la_boucle<R, S, E>(
    reserve: &mut R,
    etage: &mut E,
    puits: &mut E::Puits<'_>,
    producteur: &BoucleProducteur<'_>,
    tampon: &mut [u8],
    mut sourcer: impl FnMut(R::Lecteur) -> S,
    trames: u64,
) -> IssueDeLaChaine
where
    R: ReserveDeLaChaine,
    S: Read,
    E: EtageDeLaChaine,
{
    let mut trames = trames;
    let mut pistes_enchainees = 0u32;
    // La route native ne refuse aucun porteur : elle porte le DoP et le dit.
    let mut ne_rien_refuser = |_: bool, _: u32, _: u16| false;
    loop {
        // Fin de flux de la piste courante : son geste de fin (ASIO : le
        // reliquat 24 bits part brut ; CoreAudio : la sonde incomplète est
        // rapportée, comme à l'EOF d'avant #5451).
        trames += etage.finir_la_piste(&mut *puits);
        let (lecteur, entete) = match accepter_la_suivante(reserve, etage) {
            Ok(acceptee) => acceptee,
            Err(fin) => {
                return IssueDeLaChaine {
                    fin,
                    http_eof: true,
                    trames,
                    pistes_enchainees,
                };
            }
        };
        pistes_enchainees += 1;
        let mut compteurs = CompteursDePiste {
            total_bytes_read: 0,
            total_frames_fed: 0,
            seek_offset: 0,
            skip_bytes: 0,
            skipped_bytes: 0,
            premiere_donnee_journalisee: true,
        };
        // L'amorce : le PCM lu avec l'en-tête. Un puits déjà mort est
        // constaté par la boucle, qui le rapporte (#3108).
        etage.recevoir(entete.amorce());
        match etage.pousser(&mut *puits, &mut ne_rien_refuser, &mut |_| {}) {
            PousseeVersLePuits::Poussee { trames_source }
            | PousseeVersLePuits::PuitsMort { trames_source } => {
                compteurs.total_frames_fed += trames_source;
            }
            PousseeVersLePuits::RienAPousser | PousseeVersLePuits::PorteurDopRefuse => {}
        }
        let mut source = sourcer(lecteur);
        let fin = producteur.tourner(
            &mut source,
            tampon,
            etage,
            &mut *puits,
            &mut ne_rien_refuser,
            &mut compteurs,
            &mut |_| true,
        );
        trames = compteurs.total_frames_fed;
        match fin {
            FinDeBoucle::FinDeFlux => {}
            // Arrêt, périphérique perdu, puits mort (déjà rapporté par la
            // boucle) : la piste ne s'est pas terminée d'elle-même.
            FinDeBoucle::Interrompue | FinDeBoucle::Abandon | FinDeBoucle::PorteurDopRefuse => {
                return IssueDeLaChaine {
                    fin: FinDeChaine::Interrompue,
                    http_eof: false,
                    trames,
                    pistes_enchainees,
                };
            }
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Les étages qui savent enchaîner.
// ───────────────────────────────────────────────────────────────────────────

/// Un étage que [`poursuivre_la_chaine_par_la_boucle`] sait faire passer
/// d'une piste à la suivante sans refermer le puits : la frontière
/// ([`FrontiereExclusive`]) plus le geste de fin de flux de chaque piste.
pub(super) trait EtageDeLaChaine: Etage + FrontiereExclusive {
    /// Fin de flux d'une piste, avant la frontière. Rend les trames source
    /// poussées par ce geste (elles comptent dans la position).
    fn finir_la_piste(&mut self, puits: &mut Self::Puits<'_>) -> u64;
}

#[cfg(any(all(target_os = "windows", feature = "asio"), test))]
impl FrontiereExclusive for EtageNatifAsio<'_> {
    fn spec_courante(&self) -> AudioSpec {
        self.etage.spec()
    }

    fn enchainer_a_format_egal(
        &mut self,
        suivante: AudioSpec,
    ) -> Result<(), super::enchainement_exclusif::EnchainementNatif> {
        self.etage.enchainer_la_piste(suivante)
    }
}

/// ASIO, route native : le reliquat 24 bits jamais classé part brut
/// (`vider`), et ses trames comptent.
#[cfg(any(all(target_os = "windows", feature = "asio"), test))]
impl EtageDeLaChaine for EtageNatifAsio<'_> {
    fn finir_la_piste(&mut self, puits: &mut (dyn PuitsNatif + '_)) -> u64 {
        self.vider(puits);
        self.reliquat_force_brut.take().unwrap_or(0)
    }
}

/// #5451 — l'étage flottant du bras CoreAudio exclusif, monté au format
/// IDENTITÉ (`bras_coreaudio.rs` : la sortie est ouverte à la cadence, à la
/// profondeur et aux canaux de la source, contrat physique vérifié). Même
/// règle de frontière que les bras Windows : à format égal, la suivante
/// entre dans l'anneau ouvert ; sinon on rouvre.
///
/// À format égal, rien n'est rendu au puits ici : la queue du DSP et l'état
/// du convolveur font partie du flux continu, comme sur le chemin partagé à
/// format identique. Ce qui change : la sonde DoP/PCM repart de zéro pour la
/// nouvelle piste (comme en début de piste), et les octets non alignés de la
/// piste finie sont jetés — l'EOF d'avant #5451 les jetait aussi.
#[cfg(any(target_os = "macos", test))]
impl FrontiereExclusive for EtageDeConversion<'_> {
    fn spec_courante(&self) -> AudioSpec {
        self.spec
    }

    fn enchainer_a_format_egal(
        &mut self,
        suivante: AudioSpec,
    ) -> Result<(), super::enchainement_exclusif::EnchainementNatif> {
        use super::enchainement_exclusif::{EnchainementNatif, decider_l_enchainement_natif};
        // Un étage qui convertit n'est pas celui d'un transport exclusif : il
        // n'enchaîne pas par ici (le chemin partagé a sa propre frontière).
        if self.needs_resample || self.needs_channel_adapt() {
            return Err(EnchainementNatif::Rouvrir);
        }
        match decider_l_enchainement_natif(self.spec, self.sortie, suivante) {
            EnchainementNatif::Rouvrir => Err(EnchainementNatif::Rouvrir),
            EnchainementNatif::Enchainer => {
                self.en_attente.clear();
                self.pcm_kind = LocalPcmKind::for_bit_depth(suivante.profondeur().bits_declares());
                Ok(())
            }
        }
    }
}

/// #5451 — CoreAudio : à l'EOF, une sonde 24 bits restée incomplète est
/// RAPPORTÉE (`report_incomplete_local_pcm_probe`), exactement ce que le bras
/// faisait à la fin de sa piste unique. Aucune trame n'est poussée.
#[cfg(any(target_os = "macos", test))]
impl EtageDeLaChaine for EtageDeConversion<'_> {
    fn finir_la_piste(&mut self, _puits: &mut (dyn PuitsDEchantillons + '_)) -> u64 {
        report_incomplete_local_pcm_probe(self.pcm_kind, self.en_attente.len());
        0
    }
}
