//! #6734 — le chemin **natif S32** de la sortie locale Linux (ALSA `hw:`).
//!
//! Le chemin partagé (`BackendCpal`) fait passer chaque échantillon par
//! l'anneau `f32` (`RingBuf`) : 24 bits de mantisse. Une source PCM 32 bits
//! entiers y perd ses 8 bits de poids faible, même quand le DAC est ouvert en
//! `hw:` et en `i32` (Micetecumseh, RME ADI-2 DAC, fil 2358 : « Bit Test
//! 24 Bit passed » au lieu de 32).
//!
//! Ce bras transporte les mots **entiers** de bout en bout, sur le modèle des
//! bras exclusifs Windows (`bras_wasapi.rs`) dont il reprend l'étage
//! ([`EtageNatif`]), le puits ([`PuitsAnneauNatif`]), l'anneau
//! ([`NativePcmRing`]) et la chaîne gapless ([`jouer_la_chaine_native`]) :
//!
//! `octets WAV → EtageNatif (mots i32 alignés à gauche) → NativePcmRing →
//! rappel cpal i32 (S32_LE) → DAC`.
//!
//! **Quand il est pris** — et seulement alors ([`chemin_natif_s32`] puis
//! [`BackendAlsaNatif::tenter`]) : source PCM **entière 32 bits**, volume
//! effectif à l'unité, DSP neutre (ou PURE), aucune voie pilotée par une
//! grille, aucun fondu enchaîné, hôte ALSA, PCM
//! `hw:` sans repli, et le périphérique **annonce** S32 à la cadence et aux
//! voies de la source. Tout le reste garde le chemin partagé, tel quel, et
//! le verdict honnête de #6743 (pas de « bit-perfect » au-delà de 24 bits).
//! Les 16 et 24 bits ne sont pas concernés : l'anneau `f32` les rend intacts.
//!
//! **Volume et sécurité.** Le volume reste dans le RAPPEL, comme sur le chemin
//! partagé (D3) : un geste sur le curseur prend effet à la période suivante,
//! pas deux secondes plus tard. À l'unité et rampe au repos, les mots partent
//! intacts ; sinon ils sont pondérés en `f64` et bornés à la pleine échelle
//! (`SoftMuteRamp::apply_i32`). Le volume effectif est déjà plafonné à
//! l'unité (`effective_volume_units`) : jamais au-dessus de la pleine échelle.
//! L'étage, lui, reçoit un volume constant à l'unité — sans quoi le volume
//! serait appliqué deux fois — et ne transforme que pour le DSP armé à chaud.
//!
//! **Contrat de signal.** Comme WASAPI et ASIO, le bras publie son contrat
//! (`publish_windows_signal_path_status`, transport `NativeInteger`) : le
//! chemin du signal ne dit « bit-perfect » pour une source 32 bits que quand
//! CE contrat le dit. Le chemin partagé n'en publie aucun sous Linux.
//!
//! DoP : un porteur DoP vit sur 24 bits ; une source 32 bits n'en porte pas,
//! et l'étage garde de toute façon sa décision DoP.

use super::backend::{Observation, RefusDOuverture, Vidage};
use super::chaine_native::{
    FinDeChaine, HoteDeChaineNative, ReserveDeLaChaine, ReserveHttp, Suivante,
    jouer_la_chaine_native,
};
use super::etage_natif::{EcritureNative, EtageNatif, PuitsAnneauNatif, spec_du_puits_natif};
use super::*;

/// Le volume effectif qui laisse les mots intacts (`1000` = 100 %).
const VOLUME_UNITE: u32 = 1000;

/// Ce que `play_url` sait, avant toute ouverture, pour choisir le chemin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ConditionsDuNatif {
    /// Le format de la source.
    pub(super) spec: AudioSpec,
    /// Le volume effectif (utilisateur × ReplayGain), en millièmes.
    pub(super) volume_units: u32,
    /// `local_dsp_is_identity` : aucun DSP ne réécrit les échantillons (ou
    /// PURE les contourne).
    pub(super) dsp_identite: bool,
    /// Une grille de réaffectation demande d'autres voies que la source.
    pub(super) voies_pilotees: bool,
    /// Durée du fondu enchaîné réglée sur la sortie (ms) : la chaîne native
    /// ne fond pas, elle enchaîne.
    pub(super) fondu_ms: u32,
}

/// Pourquoi le chemin natif S32 n'est pas pris — journalisé tel quel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MotifDuCheminPartage {
    /// La source n'est pas un PCM entier 32 bits : 16 et 24 bits traversent
    /// l'anneau `f32` intacts, un flottant 32 bits aussi.
    SourcePasEntiere32Bits,
    /// Volume logiciel ≠ 100 % : les mots seront de toute façon réécrits.
    VolumeLogiciel,
    /// Un DSP réécrit les échantillons.
    DspActif,
    /// Une grille de réaffectation pilote les voies ouvertes.
    VoiesPilotees,
    /// Un fondu enchaîné est réglé.
    FonduEnchaine,
}

impl MotifDuCheminPartage {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::SourcePasEntiere32Bits => "source_pas_entiere_32_bits",
            Self::VolumeLogiciel => "volume_logiciel",
            Self::DspActif => "dsp_actif",
            Self::VoiesPilotees => "voies_pilotees",
            Self::FonduEnchaine => "fondu_enchaine",
        }
    }
}

/// La règle, sans périphérique : le chemin natif S32 est-il VOULU ? `Ok` ne
/// promet rien du périphérique — [`BackendAlsaNatif::tenter`] juge ensuite
/// l'hôte, le PCM et le format annoncé, et tout refus retombe sur le chemin
/// partagé.
pub(super) fn chemin_natif_s32(c: &ConditionsDuNatif) -> Result<(), MotifDuCheminPartage> {
    if c.spec.profondeur() != ProfondeurPcm::Entier32 {
        return Err(MotifDuCheminPartage::SourcePasEntiere32Bits);
    }
    if c.volume_units != VOLUME_UNITE {
        return Err(MotifDuCheminPartage::VolumeLogiciel);
    }
    if !c.dsp_identite {
        return Err(MotifDuCheminPartage::DspActif);
    }
    if c.voies_pilotees {
        return Err(MotifDuCheminPartage::VoiesPilotees);
    }
    if c.fondu_ms > 0 {
        return Err(MotifDuCheminPartage::FonduEnchaine);
    }
    Ok(())
}

/// Le verdict que le bras publie après chaque poussée : l'étage dit si ses
/// octets sont partis intacts (DSP neutre, voies non adaptées) ; le volume,
/// appliqué dans le rappel, doit en plus être à l'unité. DoP : contourné à
/// l'unité, toujours intact.
pub(super) fn verdict_natif_s32(etage_intact: bool, dop: bool, volume_units: u32) -> bool {
    etage_intact && (dop || volume_units == VOLUME_UNITE)
}

/// Ce que le saut d'un seek prend dans l'amorce (octets lus avec l'en-tête),
/// et ce qui reste à jeter du flux.
pub(super) fn sauter_dans_l_amorce(amorce: usize, a_sauter: u64) -> (usize, u64) {
    let pris = a_sauter.min(amorce as u64);
    (pris as usize, a_sauter - pris)
}

/// Jette `octets` du flux. `false` si le flux s'est arrêté avant (fin,
/// erreur, silence forcé) : la lecture partira alors de là où il en est.
fn jeter_du_flux(lecteur: &mut impl std::io::Read, octets: u64, force_silent: &AtomicBool) -> bool {
    let mut tampon = vec![0u8; 65536];
    let mut reste = octets;
    while reste > 0 {
        if force_silent.load(Ordering::Relaxed) {
            return false;
        }
        let n = (reste as usize).min(tampon.len());
        match lecteur.read(&mut tampon[..n]) {
            Ok(0) => return false,
            Ok(lus) => reste -= lus as u64,
            Err(ref e)
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => return false,
        }
    }
    true
}

/// Une période du rappel natif S32, hors de la fermeture pour être mesurable.
///
/// Jumeau entier de `render_local_shared_f32_callback` : même rampe
/// anti-« ploc », même porte de pré-remplissage, même comptage de famine
/// (#3814) — mais les mots tirés de l'anneau ne deviennent jamais des `f32`.
#[allow(clippy::too_many_arguments)]
pub(super) fn rendre_une_periode_native_s32(
    ring: &NativePcmRing,
    volume: &AtomicU32,
    paused: &AtomicBool,
    silent: &AtomicBool,
    data_started: &AtomicBool,
    ramp: &mut crate::audio::soft_mute::SoftMuteRamp,
    armed_ms: u32,
    min_buffer_samples: usize,
    output: &mut [i32],
) -> usize {
    ramp.arm(armed_ms);
    let silence = paused.load(Ordering::Relaxed) || silent.load(Ordering::Relaxed);
    if ramp.begin(silence) == crate::audio::soft_mute::Rendering::Silent {
        output.fill(0);
        ring.starvation.record_silent_period(output.len());
        return 0;
    }
    if !data_started.load(Ordering::Acquire) {
        if ring.available() < min_buffer_samples {
            output.fill(0);
            ring.starvation.record_silent_period(output.len());
            return 0;
        }
        data_started.store(true, Ordering::Release);
    }
    let read = ring.pop(output);
    let v = volume.load(Ordering::Relaxed) as f32 / 1000.0;
    ramp.apply_i32(&mut output[..read], v);
    output[read..].fill(0);
    read
}

/// Le périphérique annonce-t-il S32 à ces voies et à cette cadence ? Pur,
/// sur la liste que cpal rend (`supported_output_configs`).
pub(super) fn s32_annonce(
    plages: impl IntoIterator<Item = (cpal::SampleFormat, u16, u32, u32)>,
    voies: u16,
    cadence: u32,
) -> bool {
    plages.into_iter().any(|(format, canaux, min, max)| {
        format == cpal::SampleFormat::I32 && canaux == voies && min <= cadence && cadence <= max
    })
}

/// ALSA `hw:` ouvert en S32 : le backend du chemin natif.
///
/// Il possède le flux cpal `i32` et l'anneau entier (D2). Le rappel lit son
/// anneau, le volume et la rampe par valeur (D3).
pub(super) struct BackendAlsaNatif<'a> {
    stream: cpal::Stream,
    anneau: Arc<NativePcmRing>,
    spec: AudioSpec,
    sortie: FormatOuvert,
    pcm: String,
    device_name: String,
    stop_rx: &'a std::sync::mpsc::Receiver<()>,
    paused: &'a AtomicBool,
    force_silent: &'a AtomicBool,
    device_gone: Arc<AtomicBool>,
    starvation: Arc<RingStarvation>,
    position_ms: &'a AtomicU64,
}

impl<'a> BackendAlsaNatif<'a> {
    /// Tente l'ouverture native ; `Err` dit pourquoi elle n'a pas eu lieu, et
    /// l'appelant retombe sur le chemin partagé. Rien n'est joué ici.
    pub(super) fn tenter(demande: &DemandeDOuverture<'a>) -> Result<Self, String> {
        let cadence = demande.spec.cadence();
        let voies = demande.spec.canaux();
        let host = select_host(demande.audio_backend);
        let host_id_name: &'static str = host.id().name();
        if !host_id_name.eq_ignore_ascii_case("alsa") {
            return Err(format!("hôte {host_id_name}, pas ALSA"));
        }
        let Some((device, fell_back)) = find_device_with_fallback(
            &host,
            demande.device_name,
            demande.endpoint_id,
            demande.origin_host,
        ) else {
            return Err("périphérique introuvable".to_string());
        };
        if fell_back {
            return Err("repli sur le périphérique par défaut".to_string());
        }
        let pcm = device.id().map(|id| id.to_string()).unwrap_or_default();
        if !alsa_pcm_is_direct_hardware(&pcm) {
            return Err(format!("PCM « {pcm} » : pas un accès direct hw:"));
        }
        let plages: Vec<(cpal::SampleFormat, u16, u32, u32)> = device
            .supported_output_configs()
            .map(|it| {
                it.map(|r| {
                    (
                        r.sample_format(),
                        r.channels(),
                        r.min_sample_rate(),
                        r.max_sample_rate(),
                    )
                })
                .collect()
            })
            .map_err(|e| format!("énumération refusée : {e}"))?;
        if !s32_annonce(plages.iter().copied(), voies, cadence) {
            return Err(format!(
                "S32 non annoncé à {cadence} Hz sur {voies} voies ({} plages)",
                plages.len()
            ));
        }

        let cfg = config_de_flux(voies, cadence);
        let ring_cap = (cadence as usize) * (voies as usize) * 2;
        demande.starvation.begin_stream(cadence, voies);
        let anneau = Arc::new(NativePcmRing::new_metered(
            ring_cap,
            demande.starvation.clone(),
        ));
        anneau.clear();
        let min_buffer = garde_de_prechargement(&cfg, (cadence as usize) * (voies as usize) / 5);
        let data_started = Arc::new(AtomicBool::new(false));
        let device_gone = demande.device_gone.clone();
        let ring_cb = anneau.clone();
        let vol_cb = demande.volume.clone();
        let paused_cb = demande.paused.clone();
        let silent_cb = demande.force_silent.clone();
        let soft_mute_cb = demande.soft_mute.clone();
        let mut ramp_cb = soft_mute_cb.ramp(cadence, voies);
        let mut promotion = PromotionDuFilDeRendu::nouvelle();
        let stream = device
            .build_output_stream(
                &cfg,
                move |data: &mut [i32], _: &cpal::OutputCallbackInfo| {
                    promotion.a_la_premiere_periode();
                    rendre_une_periode_native_s32(
                        &ring_cb,
                        &vol_cb,
                        &paused_cb,
                        &silent_cb,
                        &data_started,
                        &mut ramp_cb,
                        soft_mute_cb.armed_ms(),
                        min_buffer,
                        data,
                    );
                },
                make_stream_error_cb(device_gone.clone(), demande.starvation.clone()),
                None,
            )
            .map_err(|e| format!("flux S32 refusé : {e}"))?;

        Ok(BackendAlsaNatif {
            stream,
            anneau,
            spec: demande.spec,
            sortie: FormatOuvert::new(cadence, voies),
            pcm,
            device_name: demande.device_name.to_string(),
            stop_rx: demande.stop_rx,
            paused: demande.paused.as_ref(),
            force_silent: demande.force_silent.as_ref(),
            device_gone,
            starvation: demande.starvation.clone(),
            position_ms: demande.position_ms,
        })
    }

    pub(super) fn pcm(&self) -> &str {
        &self.pcm
    }
}

impl<'a> BackendLocal<'a> for BackendAlsaNatif<'a> {
    fn ouvrir(demande: &DemandeDOuverture<'a>) -> Result<Self, RefusDOuverture> {
        Self::tenter(demande).map_err(|erreur| RefusDOuverture::DemarrageRefuse { erreur })
    }

    fn format_ouvert(&self) -> FormatOuvert {
        self.sortie
    }

    fn puits(&self) -> Puits<'a> {
        Puits::Natif(Box::new(PuitsAnneauNatif::sur(
            self.anneau.clone(),
            spec_du_puits_natif(self.spec),
            self.stop_rx,
            self.paused,
            self.force_silent,
        )))
    }

    fn demarrer(&mut self) -> Result<(), RefusDOuverture> {
        self.stream
            .play()
            .map_err(|e| RefusDOuverture::DemarrageRefuse {
                erreur: e.to_string(),
            })
    }

    fn observer(&self) -> Observation {
        Observation {
            disponible: self.anneau.available(),
            capacite: self.anneau.capacity(),
            peripherique_perdu: self.device_gone.load(Ordering::Relaxed),
            sous_alimentations_pilote: Some(self.starvation.snapshot().driver_underruns),
            erreurs_de_rappel: None,
        }
    }

    /// Le vidage borné du chemin partagé (`BackendCpal::drainer`), sur
    /// l'anneau entier : la position recule vers ce qui est réellement joué.
    fn drainer(&mut self, borne: std::time::Duration) -> Vidage {
        let fed_position_ms = self.position_ms.load(Ordering::Relaxed);
        let (cadence, voies) = (self.sortie.cadence, self.sortie.canaux);
        let mut drained_naturally = false;
        let debut = std::time::Instant::now();
        loop {
            if self.stop_rx.try_recv().is_ok() || self.force_silent.load(Ordering::Relaxed) {
                break;
            }
            let remaining = self.anneau.available();
            if remaining == 0 {
                drained_naturally = true;
                break;
            }
            if self.device_gone.load(Ordering::Relaxed) || debut.elapsed() >= borne {
                warn!(
                    device = %self.device_name,
                    remaining_samples = remaining,
                    device_gone = self.device_gone.load(Ordering::Relaxed),
                    "local_audio_drain_timeout"
                );
                break;
            }
            if cadence > 0 && voies > 0 {
                let ring_ms = (remaining as f64 / voies as f64 / cadence as f64 * 1000.0) as u64;
                self.position_ms
                    .store(fed_position_ms.saturating_sub(ring_ms), Ordering::Relaxed);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Vidage {
            vide: drained_naturally,
            restant: self.anneau.available(),
            position_alimentee_ms: fed_position_ms,
        }
    }

    fn nom(&self) -> &'static str {
        "ALSA S32"
    }
}

/// Ce que le bras emprunte au fil de lecture de `play_url`. Tout est
/// EMPRUNTÉ, sauf le lecteur HTTP : si l'ouverture native échoue, `play_url`
/// garde tout pour le chemin partagé — ce contexte n'est construit qu'après
/// une ouverture réussie.
pub(super) struct ContexteAlsaNatif<'a> {
    pub(super) device_name: &'a str,
    pub(super) spec: AudioSpec,
    pub(super) data_offset: usize,
    pub(super) header_buf: &'a [u8],
    pub(super) reader: super::LecteurHttpAnnulable,
    pub(super) seek_offset: u64,
    /// Octets source à jeter avant la première note : un seek que le
    /// producteur n'a pas pré-positionné (même calcul que le chemin partagé,
    /// multiple d'une trame).
    pub(super) octets_a_sauter: u64,
    pub(super) my_generation: u64,
    pub(super) volume: &'a Arc<AtomicU32>,
    pub(super) user_volume: &'a Arc<AtomicU32>,
    pub(super) rg_factor: &'a Arc<AtomicU32>,
    pub(super) playing: &'a AtomicBool,
    pub(super) force_silent: &'a Arc<AtomicBool>,
    pub(super) stop_rx: &'a std::sync::mpsc::Receiver<()>,
    pub(super) open_failure: &'a std::sync::Mutex<Option<String>>,
    pub(super) signal_path_status: &'a std::sync::Mutex<Option<OutputSignalPathStatus>>,
    pub(super) transformations_reelles: &'a std::sync::Mutex<Option<TransformationsReelles>>,
    pub(super) position_ms: &'a AtomicU64,
    pub(super) play_generation: &'a AtomicU64,
    pub(super) track_ended_naturally: &'a AtomicBool,
    pub(super) track_ended_generation: &'a AtomicU64,
    pub(super) eq: &'a std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    pub(super) convolver: &'a std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    pub(super) crossfeed: &'a std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pub(super) pure_bypass: &'a AtomicBool,
    pub(super) mono_downmix: &'a AtomicBool,
    pub(super) reaffectation: &'a CreneauReaffectation,
    pub(super) disposition: &'a CreneauDisposition,
    pub(super) dop_active: &'a AtomicBool,
    pub(super) next_media: &'a std::sync::Mutex<Option<PendingNextMedia>>,
    pub(super) chain_exhausted: &'a AtomicBool,
    pub(super) current_uri: &'a std::sync::Mutex<Option<String>>,
    pub(super) track_title: &'a std::sync::Mutex<Option<String>>,
    pub(super) track_artist: &'a std::sync::Mutex<Option<String>>,
    pub(super) duration_ms: &'a AtomicU64,
    pub(super) seek_offset_ms: &'a AtomicU64,
}

/// Ce que la chaîne native emprunte au bras : témoins d'arrêt, publication,
/// réserve de la suivante — le même hôte que `HoteWasapi`.
struct HoteAlsaNatif<'a, F: FnMut(bool, bool)> {
    stop_rx: &'a std::sync::mpsc::Receiver<()>,
    position_ms: &'a AtomicU64,
    publier: &'a mut F,
    reserve: ReserveHttp<'a>,
}

impl<F: FnMut(bool, bool)> ReserveDeLaChaine for HoteAlsaNatif<'_, F> {
    type Lecteur = super::LecteurHttpAnnulable;

    fn silence_force(&self) -> bool {
        self.reserve.silence_force()
    }

    fn preparer_la_suivante(&mut self) -> Suivante<Self::Lecteur> {
        self.reserve.preparer_la_suivante()
    }

    fn piste_enchainee(&mut self) {
        self.reserve.piste_enchainee();
    }
}

impl<F: FnMut(bool, bool)> HoteDeChaineNative for HoteAlsaNatif<'_, F> {
    fn arret_recu(&mut self) -> bool {
        self.stop_rx.try_recv().is_ok()
    }

    fn publier_le_verdict(&mut self, dop: bool, bit_perfect: bool) {
        (self.publier)(dop, bit_perfect);
    }

    fn publier_la_position(&mut self, position_ms: u64) {
        self.position_ms.store(position_ms, Ordering::Relaxed);
    }
}

/// Publie le contrat du chemin natif S32 et le rend. Sorti du bras pour que
/// le témoin le juge sans périphérique.
#[allow(clippy::too_many_arguments)]
pub(super) fn publier_le_contrat_natif_s32(
    slot: &std::sync::Mutex<Option<OutputSignalPathStatus>>,
    etage_intact: bool,
    dop: bool,
    volume_units: u32,
    eq: &std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>,
    convolver: &std::sync::Mutex<Option<crate::audio::convolver::Convolver>>,
    crossfeed: &std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>,
    pure_bypass: &AtomicBool,
    mono_downmix: &AtomicBool,
) -> OutputSignalPathStatus {
    publish_windows_signal_path_status(
        slot,
        verdict_natif_s32(etage_intact, dop, volume_units),
        true,
        dop,
        volume_units,
        eq,
        convolver,
        crossfeed,
        pure_bypass,
        mono_downmix,
    )
}

/// Joue la piste par le chemin natif S32, jusqu'à la fin du flux (et des
/// pistes enchaînées à format égal) ou l'ordre d'arrêt. Terminal.
pub(super) fn jouer_via_alsa_natif(mut backend: BackendAlsaNatif<'_>, ctx: ContexteAlsaNatif<'_>) {
    let ContexteAlsaNatif {
        device_name,
        spec,
        data_offset,
        header_buf,
        mut reader,
        seek_offset,
        octets_a_sauter,
        my_generation,
        volume,
        user_volume,
        rg_factor,
        playing,
        force_silent,
        stop_rx,
        open_failure,
        signal_path_status,
        transformations_reelles,
        position_ms,
        play_generation,
        track_ended_naturally,
        track_ended_generation,
        eq,
        convolver,
        crossfeed,
        pure_bypass,
        mono_downmix,
        reaffectation,
        disposition,
        dop_active,
        next_media,
        chain_exhausted,
        current_uri,
        track_title,
        track_artist,
        duration_ms,
        seek_offset_ms,
    } = ctx;
    let sortie = backend.format_ouvert();
    info!(
        device = %device_name,
        endpoint_id = %backend.pcm(),
        sample_rate = sortie.cadence,
        channels = sortie.canaux,
        bit_depth = spec.profondeur().bits_declares(),
        "local_audio_alsa_natif_s32_ouvert"
    );
    note_rate_decision(ObservedRate {
        source_sample_rate: spec.cadence(),
        opened_sample_rate: sortie.cadence,
        reason: None,
        evidence_measured: true,
    });
    reaffectation.constater_l_ouverture(None, sortie.canaux, device_name);

    if let Err(refus) = backend.demarrer() {
        refus.rapporter(device_name, open_failure);
        playing.store(false, Ordering::SeqCst);
        return;
    }

    // Le volume est appliqué par le RAPPEL : l'étage reçoit l'unité, sans
    // quoi il serait appliqué deux fois.
    let volume_de_l_etage = AtomicU32::new(VOLUME_UNITE);
    let mut etage = EtageNatif::monter(
        spec,
        sortie,
        &volume_de_l_etage,
        eq,
        convolver,
        crossfeed,
        pure_bypass,
        mono_downmix,
    )
    .avec_l_adaptation_des_canaux(reaffectation, disposition);
    if let Ok(mut slot) = transformations_reelles.lock() {
        *slot = Some(etage.transformations());
    }
    let mut puits = match backend.puits() {
        Puits::Natif(puits) => puits,
        Puits::Flottant(_) => unreachable!("BackendAlsaNatif ne fournit qu'un puits natif"),
    };

    let mut bit_perfect_state = None;
    let mut publier_le_verdict = |dop: bool, etage_intact: bool| {
        if dop_active.swap(dop, Ordering::SeqCst) != dop {
            info!(dop, "local_audio_dop_stream_state_changed");
            sync_volume_to_dop(volume, user_volume, rg_factor, dop);
        }
        let volume_units = volume.load(Ordering::SeqCst);
        let runtime = publier_le_contrat_natif_s32(
            signal_path_status,
            etage_intact,
            dop,
            volume_units,
            eq,
            convolver,
            crossfeed,
            pure_bypass,
            mono_downmix,
        );
        if bit_perfect_state != Some(runtime.bit_perfect) {
            bit_perfect_state = Some(runtime.bit_perfect);
            info!(
                backend = "ALSA S32",
                bit_perfect = runtime.bit_perfect,
                dop,
                volume_units,
                reasons = ?runtime.reasons,
                "linux_native_s32_signal_contract"
            );
        }
    };

    if dop_active.swap(false, Ordering::SeqCst) {
        sync_volume_to_dop(volume, user_volume, rg_factor, false);
    }

    let mut pcm_data = header_buf.get(data_offset..).unwrap_or_default();
    // Le seek non pré-positionné : jeter d'abord dans l'amorce, puis dans le
    // flux, avant que le premier mot n'entre dans l'anneau.
    let (amorce_sautee, reste) = sauter_dans_l_amorce(pcm_data.len(), octets_a_sauter);
    pcm_data = &pcm_data[amorce_sautee..];
    if reste > 0 && !jeter_du_flux(&mut reader, reste, force_silent) {
        debug!("local_audio_alsa_natif_s32_seek_interrompu");
    }
    let mut total_frames_fed: u64 = 0;
    match etage.decoder_et_pousser(pcm_data, &mut *puits) {
        EcritureNative::Poussee {
            trames_source,
            dop,
            bit_perfect,
        }
        | EcritureNative::PuitsMort {
            trames_source,
            dop,
            bit_perfect,
        } => {
            total_frames_fed += trames_source;
            publier_le_verdict(dop, bit_perfect);
        }
        EcritureNative::RienAPousser => {}
    }

    let mut hote = HoteAlsaNatif {
        stop_rx,
        position_ms,
        publier: &mut publier_le_verdict,
        reserve: ReserveHttp {
            force_silent,
            position_ms,
            next_media,
            en_cours: None,
            current_uri,
            track_title,
            track_artist,
            duration_ms,
            seek_offset_ms,
            track_ended_naturally,
            track_ended_generation,
            dop_active,
            volume,
            user_volume,
            rg_factor,
        },
    };
    let issue = jouer_la_chaine_native(
        &mut hote,
        reader,
        &mut etage,
        &mut *puits,
        total_frames_fed,
        seek_offset,
    );
    if issue.pistes_enchainees > 0 || issue.fin == FinDeChaine::FormatDifferent {
        info!(
            device = %device_name,
            pistes_enchainees = issue.pistes_enchainees,
            fin = ?issue.fin,
            "local_audio_alsa_natif_s32_chain_ended"
        );
    }
    if doit_declarer_chaine_epuisee(
        force_silent.load(Ordering::Relaxed),
        play_generation.load(Ordering::SeqCst),
        my_generation,
    ) {
        chain_exhausted.store(true, Ordering::SeqCst);
        debug!("local_audio_gapless_chain_exhausted");
    }
    // #6842 — la chaîne s'arrête sur un puits mort ; la famine est dite au
    // nom du backend, et un anneau que plus personne ne draine ne se vide pas.
    let puits_mort = issue.fin == FinDeChaine::PuitsMort || !etage.rendre_la_queue(&mut *puits);
    if puits_mort {
        record_feed_stall_failure(
            "ALSA S32",
            device_name,
            position_ms.load(Ordering::Relaxed),
            open_failure,
        );
    }

    if issue.http_eof {
        track_ended_naturally.store(true, Ordering::SeqCst);
        track_ended_generation.store(my_generation, Ordering::SeqCst);
        TRACK_END_NOTIFY.notify_one();
    }
    let borne = drain_deadline_for(
        backend.observer().disponible,
        u64::from(sortie.cadence),
        u64::from(sortie.canaux),
    );
    if !puits_mort {
        let _ = backend.drainer(borne);
    }

    drop(puits);
    drop(backend);
    if play_generation.load(Ordering::SeqCst) == my_generation {
        playing.store(false, Ordering::SeqCst);
    }
    info!(
        device = %device_name,
        frames = issue.trames,
        "local_audio_alsa_natif_s32_stopped"
    );
}

#[cfg(test)]
#[path = "chemin_natif_s32_6734.rs"]
mod chemin_natif_s32_6734;
