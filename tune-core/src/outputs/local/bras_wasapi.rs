//! Le bras WASAPI exclusif de `play_url` (R6 bis, puis REF-8, #2219).
//!
//! R6 bis a sorti le bloc `#[cfg(target_os = "windows")] if exclusive_mode &&
//! audio_backend != "asio" { … }` de `play_url` à l'identique. REF-8 le fait
//! passer par le trait [`BackendLocal`] : [`BackendWasapi`] ouvre, fournit un
//! [`Puits::Natif`], démarre, s'observe et se draine ; l'étage natif
//! ([`EtageNatif`], `local/etage_natif.rs`) décode et pousse. Ce qui restait
//! en ligne — `leftover`, `must_classify_24_bit`, `dop_latched`, le reliquat
//! 24 bits, la queue du DSP — vit dans l'étage. **Le rendu ne change pas** :
//! `empreinte_wasapi_f70496.rs` le tient contre trois relevés d'avant.
//!
//! Le bras est terminal : il ne rend rien à la suite de `play_url`, qui fait
//! `return` après l'appel. Aucune condition ne change — le `if` et sa
//! bannière restent dans `play_url`.
//!
//! **Pourquoi l'implémentation du trait est ici et non dans
//! `wasapi_exclusive.rs`** : `BackendLocal`, `DemandeDOuverture`,
//! `RefusDOuverture`, `Puits`, `Observation` et `Vidage` sont `pub(super)`
//! dans `local/backend.rs`, donc visibles dans `outputs::local` et ses
//! enfants seulement ; `outputs::wasapi_exclusive` est un frère de `local`,
//! pas un enfant. `WasapiExclusiveOutput` reste l'objet pilote (COM, fil de
//! rendu, `pop_pcm_bytes`) ; `BackendWasapi` est le backend au sens du trait
//! et POSSÈDE l'anneau (D2), partagé par `Arc` avec le fil de rendu.
//!
//! La boucle de lecture reste ici : `BoucleProducteur::tourner` n'est pas
//! encore générique sur un étage (branche de l'agent A absente au moment
//! d'écrire). Quand elle le sera, ce fichier n'aura plus qu'à lui prêter
//! l'étage et le puits.
//!
//! Compilé par les deux étapes du job `windows-pr` de `ci.yml` (la seule
//! qui active `local-audio` sous Windows) : ni Shrek ni le Mac ne voient ce
//! fichier. `super` désigne ici `outputs::local`, pas `outputs` : le module
//! WASAPI se nomme par `crate::outputs::wasapi_exclusive`.

// ------- WASAPI Exclusive mode path (Windows, non-ASIO) -------

use std::io::Read;

use super::backend::{Observation, RefusDOuverture, Vidage};
use super::etage_natif::{EcritureNative, EtageNatif, PuitsAnneauNatif, spec_du_puits_natif};
use super::*;
use crate::outputs::wasapi_exclusive::WasapiExclusiveOutput;

/// Ce que le bras lisait du contexte de `play_url` — toutes déjà possédées
/// par le fil de lecture. Elles sont DÉPLACÉES, jamais empruntées (leçon de
/// #3386). Seul bras à lire `endpoint_id` : WASAPI ouvre l'`IMMDevice` exact
/// capturé à la découverte (#2207).
///
/// REF-8 : `sample_rate`, `bit_depth`, `channels` et `frame_bytes` sont
/// devenus `spec` (R5 : un `AudioSpec`, pas quatre nombres nus) ;
/// `audio_backend` et `soft_mute` entrent parce que [`DemandeDOuverture`] les
/// porte — WASAPI n'en fait rien, le trait ne le sait pas.
pub(super) struct EntreesWasapi {
    pub(super) device_name: String,
    pub(super) endpoint_id: Option<String>,
    pub(super) audio_backend: String,
    /// Le format de la SOURCE (R5) : cadence, profondeur, canaux.
    pub(super) spec: AudioSpec,
    pub(super) soft_mute: crate::audio::soft_mute::SoftMuteGate,
    pub(super) data_offset: usize,
    /// Les 4 096 premiers octets lus par `play_url` ; ce qui suit
    /// `data_offset` est le début du PCM.
    pub(super) header_buf: Vec<u8>,
    /// La réponse HTTP, positionnée après `header_buf`.
    pub(super) reader: reqwest::blocking::Response,
    pub(super) seek_offset: u64,
    pub(super) my_generation: u64,
    pub(super) starvation: Arc<RingStarvation>,
    pub(super) volume: Arc<AtomicU32>,
    pub(super) user_volume_ref: Arc<AtomicU32>,
    pub(super) rg_factor_ref: Arc<AtomicU32>,
    pub(super) paused: Arc<AtomicBool>,
    pub(super) playing: Arc<AtomicBool>,
    pub(super) force_silent: Arc<AtomicBool>,
    pub(super) stop_rx: std::sync::mpsc::Receiver<()>,
    pub(super) open_failure: Arc<std::sync::Mutex<Option<String>>>,
    pub(super) signal_path_status: Arc<std::sync::Mutex<Option<OutputSignalPathStatus>>>,
    pub(super) position_ms: Arc<AtomicU64>,
    pub(super) play_generation: Arc<AtomicU64>,
    pub(super) track_ended_naturally: Arc<AtomicBool>,
    pub(super) track_ended_generation: Arc<AtomicU64>,
    pub(super) eq: Arc<std::sync::Mutex<Option<crate::audio::eq::EqProcessor>>>,
    pub(super) convolver: Arc<std::sync::Mutex<Option<crate::audio::convolver::Convolver>>>,
    pub(super) crossfeed:
        Arc<std::sync::Mutex<Option<crate::audio::crossfeed::CrossfeedProcessor>>>,
    pub(super) pure_bypass: Arc<AtomicBool>,
    pub(super) mono_downmix: Arc<AtomicBool>,
    pub(super) dop_active: Arc<AtomicBool>,
}

/// WASAPI en mode exclusif événementiel : le backend de tous les DAC Windows
/// réglés « exclusif » sans ASIO.
///
/// Il possède l'anneau entier (D2) — `cadence × canaux × 2`, deux secondes,
/// comme les autres bras — et l'objet pilote. Le fil de rendu de
/// `WasapiExclusiveOutput` tient l'autre bout de l'anneau par `Arc` et ne
/// fait que sérialiser des mots déjà résolus (D3 : volume et DSP dans le
/// producteur, jamais ici).
pub(super) struct BackendWasapi<'a> {
    wasapi: WasapiExclusiveOutput,
    anneau: Arc<NativePcmRing>,
    spec: AudioSpec,
    sortie: FormatOuvert,
    stop_rx: &'a std::sync::mpsc::Receiver<()>,
    paused: &'a AtomicBool,
    force_silent: &'a AtomicBool,
    position_ms: &'a AtomicU64,
}

impl BackendWasapi<'_> {
    /// Arrête le rendu et libère les ressources COM — `stop()`, que le bras
    /// appelait explicitement avant de journaliser sa fin.
    fn arreter(&mut self) {
        self.wasapi.stop();
    }

    fn opened_device_name(&self) -> &str {
        self.wasapi.opened_device_name()
    }

    fn opened_device_id(&self) -> &str {
        self.wasapi.opened_device_id()
    }

    fn format_info(&self) -> String {
        self.wasapi.format_info()
    }
}

impl<'a> BackendLocal<'a> for BackendWasapi<'a> {
    /// `WasapiExclusiveOutput::new` : résolution de l'endpoint,
    /// `Initialize(EXCLUSIVE | EVENTCALLBACK)` au format source, avec la
    /// reprise alignée de #2208. Un refus est
    /// [`RefusDOuverture::OuvertureExclusiveRefusee`] pour `"WASAPI"` — le
    /// même `record_exclusive_open_failure` qu'avant, par `rapporter`.
    fn ouvrir(demande: &DemandeDOuverture<'a>) -> Result<Self, RefusDOuverture> {
        let sample_rate = demande.spec.cadence();
        let bit_depth = demande.spec.profondeur().bits_declares();
        let channels = demande.spec.canaux();
        // WASAPI ouvre au format source : ni cadence, ni canaux, ni repli.
        // `audio_backend`, `origin_host`, `exclusive`, `soft_mute` et
        // `device_gone` sont ceux de la demande, le trait les porte pour les
        // bras qui en ont besoin ; celui-ci n'en fait rien.
        let _ = (
            demande.audio_backend,
            demande.origin_host,
            demande.exclusive,
            &demande.soft_mute,
            demande.device_gone,
        );

        let ring_cap = (sample_rate as usize) * (channels as usize) * 2;
        demande.starvation.begin_stream(sample_rate, channels);
        let ring = Arc::new(NativePcmRing::new_metered(
            ring_cap,
            demande.starvation.clone(),
        ));
        ring.clear();

        match WasapiExclusiveOutput::new(
            demande.device_name,
            demande.endpoint_id,
            sample_rate,
            bit_depth as u32,
            channels as u32,
            ring.clone(),
            demande.paused.clone(),
        ) {
            Ok(wasapi) => {
                let sortie =
                    FormatOuvert::new(wasapi.opened_sample_rate(), wasapi.opened_channels() as u16);
                Ok(BackendWasapi {
                    wasapi,
                    anneau: ring,
                    spec: demande.spec,
                    sortie,
                    stop_rx: demande.stop_rx,
                    paused: demande.paused.as_ref(),
                    force_silent: demande.force_silent.as_ref(),
                    position_ms: demande.position_ms,
                })
            }
            Err(e) => Err(RefusDOuverture::OuvertureExclusiveRefusee {
                backend: "WASAPI",
                erreur: e,
            }),
        }
    }

    /// Le format que `Initialize(EXCLUSIVE)` a accepté : celui de la source.
    fn format_ouvert(&self) -> FormatOuvert {
        self.sortie
    }

    /// Le puits d'anneau natif : des blocs `Entier32` (mots alignés à gauche),
    /// poussés en attendant que le fil de rendu draine.
    fn puits(&self) -> Puits<'a> {
        Puits::Natif(Box::new(PuitsAnneauNatif::sur(
            self.anneau.clone(),
            spec_du_puits_natif(self.spec),
            self.stop_rx,
            self.paused,
            self.force_silent,
        )))
    }

    /// `start()` : amorce le premier tampon d'endpoint (silence si l'anneau
    /// est encore vide), `IAudioClient::Start`, puis le fil de rendu. Un
    /// refus est rapporté comme un refus d'ouverture exclusive, comme avant :
    /// le bras appelait `record_exclusive_open_failure("WASAPI", …)` sur les
    /// deux sites, `new` et `start`.
    fn demarrer(&mut self) -> Result<(), RefusDOuverture> {
        self.wasapi
            .start()
            .map_err(|erreur| RefusDOuverture::OuvertureExclusiveRefusee {
                backend: "WASAPI",
                erreur,
            })
    }

    /// `underrun_count` et `callback_error_count` du fil de rendu ;
    /// `deadline_miss_count` n'a pas de case dans [`Observation`] et reste
    /// journalisé à l'arrêt (`wasapi_exclusive_stopped`). Aucun rappel
    /// d'erreur : le périphérique n'est jamais vu « perdu » ici.
    fn observer(&self) -> Observation {
        Observation {
            disponible: self.anneau.available(),
            capacite: self.anneau.capacity(),
            peripherique_perdu: false,
            sous_alimentations_pilote: Some(self.wasapi.underrun_count()),
            erreurs_de_rappel: Some(self.wasapi.callback_error_count()),
        }
    }

    /// Le vidage du bras, **tel quel : sans borne**. La signature en porte
    /// une ; elle est ignorée, et c'est un défaut nommé — le seul bras dont
    /// le vidage ne s'arrête que sur stop, silence forcé ou anneau vide. Face
    /// à un rappel de rendu mort, il ne se vide JAMAIS (#3108, carte §1.2).
    /// À borner après l'écoute, pas dans une PR « sans changer le rendu ».
    /// La position publiée ne recule pas non plus : le bras ne le faisait pas.
    fn drainer(&mut self, _borne: std::time::Duration) -> Vidage {
        let fed_position_ms = self.position_ms.load(Ordering::Relaxed);
        let mut drained_naturally = false;
        // Wait for ring buffer to drain
        loop {
            if self.stop_rx.try_recv().is_ok() {
                break;
            }
            if self.force_silent.load(Ordering::Relaxed) {
                break;
            }
            if self.anneau.available() == 0 {
                drained_naturally = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Vidage {
            vide: drained_naturally,
            restant: self.anneau.available(),
            position_alimentee_ms: fed_position_ms,
        }
    }

    /// Le nom que la boucle commune met dans son rapport de famine (REF-7,
    /// #3108). `74b7247e` a été écrit AVANT que #4013 n'ajoute `nom` au trait,
    /// et la fusion `96025275` n'a adapté que le bras ASIO : l'impl WASAPI est
    /// restée sans cette méthode. Ni Shrek ni le Mac ne compilent ce fichier
    /// (`cfg(target_os = "windows")` + feature `local-audio`), et l'étape
    /// « Windows livré sans ASIO » ne l'active pas non plus — seule l'étape
    /// « ASIO » du job `windows-pr` l'a vu, en E0046.
    fn nom(&self) -> &'static str {
        "WASAPI"
    }
}

/// Joue la piste sur WASAPI en mode exclusif événementiel, au format source,
/// par l'anneau natif i32, jusqu'à la fin du flux ou l'ordre d'arrêt.
/// Terminal : quand il rend, le fil de lecture n'a plus rien à faire.
///
/// `ouvrir → démarrer → étage natif → lire/pousser → reliquat → queue du DSP
/// → drainer → arrêter`. L'ordre `démarrer` avant la première poussée est
/// celui du bras : WASAPI amorce son premier tampon de silence et part, le
/// pré-remplissage n'existe pas sur ce chemin.
pub(super) fn jouer_via_wasapi(entrees: EntreesWasapi) {
    let EntreesWasapi {
        device_name,
        endpoint_id,
        audio_backend,
        spec,
        soft_mute,
        data_offset,
        header_buf,
        mut reader,
        seek_offset,
        my_generation,
        starvation,
        volume,
        user_volume_ref,
        rg_factor_ref,
        paused,
        playing,
        force_silent,
        stop_rx,
        open_failure,
        signal_path_status,
        position_ms,
        play_generation,
        track_ended_naturally,
        track_ended_generation,
        eq,
        convolver,
        crossfeed,
        pure_bypass,
        mono_downmix,
        dop_active,
    } = entrees;

    let sample_rate = spec.cadence();
    let bit_depth = spec.profondeur().bits_declares();
    let channels = spec.canaux();

    info!(
        device = %device_name,
        sample_rate,
        bit_depth,
        channels,
        "local_audio_wasapi_exclusive_mode_active"
    );

    // Aucun rappel d'erreur sur ce chemin : le témoin existe pour la
    // demande, personne ne le lève.
    let device_gone = Arc::new(AtomicBool::new(false));
    let demande = DemandeDOuverture {
        spec,
        device_name: &device_name,
        endpoint_id: endpoint_id.as_deref(),
        origin_host: None,
        audio_backend: &audio_backend,
        exclusive: true,
        stop_rx: &stop_rx,
        paused: &paused,
        force_silent: &force_silent,
        volume: &volume,
        device_gone: &device_gone,
        starvation: &starvation,
        soft_mute,
        position_ms: &position_ms,
    };
    let mut backend = match BackendWasapi::ouvrir(&demande) {
        Ok(backend) => backend,
        Err(refus) => {
            refus.rapporter(&device_name, &open_failure);
            playing.store(false, Ordering::SeqCst);
            return;
        }
    };
    if let Err(refus) = backend.demarrer() {
        refus.rapporter(&device_name, &open_failure);
        playing.store(false, Ordering::SeqCst);
        return;
    }
    info!(
        requested_device = %device_name,
        device = %backend.opened_device_name(),
        endpoint_id = %backend.opened_device_id(),
        info = %backend.format_info(),
        "wasapi_exclusive_playing"
    );
    // Ces deux accesseurs existaient depuis #2207 et
    // n'avaient que cette ligne de journal pour
    // lecteur. La zone les porte désormais.
    note_opened_device(
        "WASAPI",
        &device_name,
        backend.opened_device_name(),
        Some(backend.opened_device_id()),
    );

    let pcm_data = if data_offset < header_buf.len() {
        header_buf[data_offset..].to_vec()
    } else {
        Vec::new()
    };

    let mut total_frames_fed: u64 = 0;
    let mut read_buf = vec![0u8; 65536];
    let mut bit_perfect_state = None;

    let mut etage = EtageNatif::monter(
        spec,
        backend.format_ouvert(),
        &volume,
        &eq,
        &convolver,
        &crossfeed,
        &pure_bypass,
        &mono_downmix,
    );
    let mut puits = match backend.puits() {
        Puits::Natif(puits) => puits,
        Puits::Flottant(_) => unreachable!("BackendWasapi ne fournit qu'un puits natif"),
    };

    // Ce que le bras faisait après chaque poussée, en deux copies (amorce,
    // boucle) : l'état DoP de la zone, le volume qui le suit, le verdict du
    // chemin de signal, et sa journalisation au changement.
    let mut publier_le_verdict = |dop: bool, bit_perfect: bool| {
        if dop_active.swap(dop, Ordering::SeqCst) != dop {
            info!(dop, "local_audio_dop_stream_state_changed");
            sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, dop);
        }
        let volume_units = volume.load(Ordering::SeqCst);
        let runtime = publish_windows_signal_path_status(
            &signal_path_status,
            bit_perfect,
            true,
            dop,
            volume_units,
            &eq,
            &convolver,
            &crossfeed,
            &pure_bypass,
            &mono_downmix,
        );
        if bit_perfect_state != Some(runtime.bit_perfect) {
            bit_perfect_state = Some(runtime.bit_perfect);
            info!(
                backend = "WASAPI",
                bit_perfect = runtime.bit_perfect,
                dop,
                volume_units,
                reasons = ?runtime.reasons,
                "windows_exclusive_signal_contract"
            );
        }
    };

    // A new track never inherits the DoP/volume state
    // of the previous one while its first 24-bit probe
    // is still quarantined.
    if dop_active.swap(false, Ordering::SeqCst) {
        sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, false);
    }

    // Un puits mort (`PuitsMort`) est traité comme une poussée : le bras
    // ignorait le verdict de `feed_native_ring_abortable` et continuait à
    // lire. C'est la « famine muette » de la carte §1.2 (aucun
    // `record_feed_stall_failure` sur ce bras), conservée telle quelle et
    // nommée comme défaut — à traiter après l'écoute.
    match etage.decoder_et_pousser(&pcm_data, &mut *puits) {
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
    if !etage.quarantaine_24_bits_ouverte() && dop_active.swap(false, Ordering::SeqCst) {
        info!(dop = false, "local_audio_dop_stream_state_changed");
        sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, false);
    }

    let mut http_eof_wasapi = false;
    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        if force_silent.load(Ordering::Relaxed) {
            debug!("local_audio_wasapi_exclusive_aborted_by_stop");
            break;
        }

        match reader.read(&mut read_buf) {
            Ok(0) => {
                http_eof_wasapi = true;
                break;
            }
            Ok(n) => {
                match etage.decoder_et_pousser(&read_buf[..n], &mut *puits) {
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

                let pos =
                    (total_frames_fed as f64 / sample_rate as f64 * 1000.0) as u64 + seek_offset;
                position_ms.store(pos, Ordering::Relaxed);
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                continue;
            }
            Err(e) => {
                warn!(error = %e, "local_audio_wasapi_exclusive_read_error");
                http_eof_wasapi = true;
                break;
            }
        }
    }

    // Less than 32 initial 24-bit frames cannot be
    // classified, but the integer ring can still carry
    // them safely. Keep them raw and at unity rather
    // than guessing PCM and applying sample arithmetic.
    if http_eof_wasapi && let Some(reliquat) = etage.vider(&mut *puits) {
        total_frames_fed += reliquat.trames;
        info!(
            backend = "WASAPI",
            bytes = reliquat.octets,
            "windows_exclusive_short_24bit_stream_forced_raw"
        );
    }

    // WASAPI exclusive now follows the same DSP tail
    // contract as the other local PCM paths (#2209).
    // Le verdict du puits est ignoré, comme avant (famine muette).
    let _ = etage.rendre_la_queue(&mut *puits);

    // Signal natural track end BEFORE draining when
    // the HTTP stream reached EOF, so the orchestrator
    // can detect end-of-track even if force_silent is
    // set during slow drain (e.g. 44.1→192 kHz resample).
    if http_eof_wasapi {
        track_ended_naturally.store(true, Ordering::SeqCst);
        track_ended_generation.store(my_generation, Ordering::SeqCst);
        TRACK_END_NOTIFY.notify_one();
    }

    // La borne est celle que le chemin partagé calcule ; `drainer` l'ignore
    // (vidage sans borne conservé, voir `BackendWasapi::drainer`).
    let borne = drain_deadline_for(
        backend.observer().disponible,
        u64::from(sample_rate),
        u64::from(channels),
    );
    let _ = backend.drainer(borne);

    drop(puits);
    backend.arreter();
    if play_generation.load(Ordering::SeqCst) == my_generation {
        playing.store(false, Ordering::SeqCst);
    }
    info!(
        device = %device_name,
        frames = total_frames_fed,
        "local_audio_wasapi_exclusive_stopped"
    );
}
