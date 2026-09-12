//! Le bras CoreAudio exclusif de `play_url` (R6 bis, puis REF-8, #2219).
//!
//! R6 bis a sorti ce bloc de `play_url` à l'identique, avec sa propre boucle
//! de lecture. REF-8 (cette tranche) la fait disparaître : le bras est
//! maintenant **ouvrir → étage de conversion → boucle producteur commune →
//! drainer**, exactement la suite de gestes du chemin CPAL partagé, sur le
//! backend [`BackendCoreAudio`] qui implémente [`BackendLocal`].
//!
//! Trois décisions, tenues mot pour mot (plan de nuit du 12/09) :
//!
//! * **D1 — le mot reste `f32`.** CoreAudio rend du `f32` à l'AudioUnit, qui
//!   convertit vers le format physique hors du dépôt. Le passer en entier
//!   changerait le son : c'est une décision d'écoute, pas de nuit. Le backend
//!   fournit donc [`Puits::Flottant`], et le DoP traverse comme avant : en
//!   `f32`, reconnu par `process_pcm_chunk` après 32 trames, volume figé à
//!   l'unité par `sync_volume_to_dop`. Aucun refus, aucun verrou.
//! * **D2 — le backend possède son anneau.** Il est créé dans
//!   `ExclusiveOutput::new` (`coreaudio_exclusive.rs`), à la même contenance
//!   (`cadence × canaux × 2`, deux secondes). Ce module n'en calcule plus.
//! * **D3 — le volume reste multiplié dans le rappel `Interleaved<f32>`**,
//!   par valeur, comme aujourd'hui. Le rappel ne voit jamais un trait.
//!
//! Le trait `BackendLocal<'a>` est `pub(super)` dans `local::backend` : il
//! n'est pas nommable depuis `outputs::coreaudio_exclusive`, et sa durée de
//! vie `'a` est celle des témoins d'arrêt du fil de lecture, que
//! `ExclusiveOutput` (un type `pub` sans lifetime) n'a aucune raison de
//! porter. L'implémentation vit donc ICI, sur [`BackendCoreAudio<'a>`], qui
//! enveloppe l'`ExclusiveOutput` et tient les emprunts ; ce que l'impl appelle
//! (`new` sans démarrage, `start`, `ring`, `format_info`) vit dans
//! `coreaudio_exclusive.rs`.
//!
//! Après #4013 (REF-7, agent A) : la boucle producteur est générique sur le
//! trait `Etage`, et c'est ELLE qui rapporte la famine, une fois, avec le nom
//! que ce backend lui donne (`BackendLocal::nom` → « CoreAudio »). Le bras
//! n'appelle plus `record_feed_stall_failure` lui-même ; il relit le témoin du
//! puits pour ne pas rendre la queue du DSP à un rappel mort.
//!
//! Compilé par la seule porte macOS (`macos-pr` de `ci.yml`) : Shrek ne voit
//! pas ce fichier. Les gardes de texte qui le lisent (`dsp_track_boundary`,
//! `refus_exclusif_dit_sa_cause_i3108`, `backend_fallback_tests`) le relisent
//! sur ce qu'il fait maintenant ; l'empreinte du chemin décoder → étage →
//! puits est tenue par `empreinte_coreaudio_f70496.rs`, sur Shrek.

// ------- Exclusive mode path (macOS only) -------

use super::backend::{
    BackendLocal, DemandeDOuverture, Observation, Puits, RefusDOuverture, Vidage,
};
use super::*;
use crate::outputs::coreaudio_exclusive::ExclusiveOutput;

/// Ce que le bras lisait du contexte de `play_url` — trente-deux valeurs,
/// toutes déjà possédées par le fil de lecture (des `Arc` clonés dans le
/// préambule de `play_url`, ou des valeurs). Elles sont DÉPLACÉES dans la
/// structure, jamais empruntées : le bras est terminal, `play_url` n'en a
/// plus besoin après l'appel, et rien ne peut rendre E0521 (leçon de #3386).
pub(super) struct EntreesCoreAudio {
    pub(super) device_name: String,
    pub(super) url: String,
    pub(super) sample_rate: u32,
    pub(super) bit_depth: u16,
    pub(super) channels: u16,
    pub(super) data_offset: usize,
    /// Les 4 096 premiers octets lus par `play_url` ; ce qui suit
    /// `data_offset` est le début du PCM.
    pub(super) header_buf: Vec<u8>,
    /// La réponse HTTP, positionnée après `header_buf`.
    pub(super) reader: reqwest::blocking::Response,
    pub(super) frame_bytes: usize,
    pub(super) spec: AudioSpec,
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

/// Le puits du bras CoreAudio : l'anneau flottant de l'`ExclusiveOutput`, que
/// draine le rappel `Interleaved<f32>`.
///
/// Écrit sur le modèle de `PuitsAnneauCpal` (`local/backend.rs`) : il porte
/// les trois témoins d'arrêt parce que l'attente a lieu ICI — quand l'anneau
/// est plein, c'est `feed_ring_abortable` qui dort. Contrat de `false`, mot
/// pour mot celui de `PuitsDEchantillons` : rendu UNIQUEMENT quand le
/// détecteur de blocage s'est déclenché (le rappel de rendu n'a plus tiré
/// pendant ≥ 5 s — rappel mort, DAC USB arraché, #1626) ; `true` sinon, y
/// compris sur un arrêt ou un silence forcé, que les appelants détectent par
/// leurs propres témoins.
///
/// Le verdict est en plus MÉMORISÉ dans `bloque` : la boucle commune le
/// traduit en `FinDeBoucle::Interrompue`, indistinguable d'un stop — elle a
/// déjà rapporté la famine sous le nom de ce backend (#3108, REF-7) ; le
/// drapeau dit au bras de ne pas rendre la queue du DSP à un rappel mort.
struct PuitsAnneauCoreAudio<'a> {
    anneau: Arc<RingBuf>,
    stop_rx: &'a std::sync::mpsc::Receiver<()>,
    paused: &'a AtomicBool,
    force_silent: &'a AtomicBool,
    bloque: Arc<AtomicBool>,
}

impl PuitsDEchantillons for PuitsAnneauCoreAudio<'_> {
    fn ecrire(&mut self, mots: &[f32]) -> bool {
        let vivant = feed_ring_abortable(
            &self.anneau,
            mots,
            self.stop_rx,
            self.paused,
            Some(self.force_silent),
        );
        if !vivant {
            self.bloque.store(true, Ordering::SeqCst);
        }
        vivant
    }
}

/// CoreAudio en mode exclusif (hog) : le backend de la sortie locale sur
/// macOS quand la zone est réglée « exclusif ».
///
/// Il possède l'`ExclusiveOutput`, donc l'anneau (D2) et l'AudioUnit. Le
/// rappel de rendu est celui d'avant, inchangé : il tire dans l'anneau et
/// multiplie le volume par valeur (D3).
pub(super) struct BackendCoreAudio<'a> {
    sortie: ExclusiveOutput,
    format: FormatOuvert,
    device_name: String,
    stop_rx: &'a std::sync::mpsc::Receiver<()>,
    paused: &'a AtomicBool,
    force_silent: &'a AtomicBool,
    position_ms: &'a AtomicU64,
    /// Levé par le puits quand le rappel de rendu ne tire plus (#3108).
    bloque: Arc<AtomicBool>,
}

impl BackendCoreAudio<'_> {
    /// Le nom du périphérique que CoreAudio a RÉELLEMENT ouvert
    /// (`resolve_output_device` retombe sur la sortie système quand le nom
    /// stocké n'existe plus).
    fn peripherique_ouvert(&self) -> &str {
        &self.sortie.format_info().device_name
    }

    /// Le puits a-t-il constaté un rappel de rendu mort ? Hors trait : la
    /// boucle commune l'a déjà rapporté ; le bras s'en sert pour ne rien
    /// rendre de plus à un rappel qui ne tire plus.
    fn puits_bloque(&self) -> bool {
        self.bloque.load(Ordering::SeqCst)
    }
}

impl<'a> BackendLocal<'a> for BackendCoreAudio<'a> {
    /// `prepare_exclusive_device` (hog, cadence, format physique, rollback)
    /// puis l'AudioUnit et son rappel — c'est `ExclusiveOutput::new`, qui ne
    /// démarre plus. Le refus est `OuvertureExclusiveRefusee { "CoreAudio" }`
    /// et `rapporter` le passe à `record_exclusive_open_failure`, comme le
    /// bras le faisait en ligne.
    fn ouvrir(demande: &DemandeDOuverture<'a>) -> Result<Self, RefusDOuverture> {
        let device_name = demande.device_name.to_string();
        let sample_rate = demande.spec.cadence();
        let bit_depth = demande.spec.profondeur().bits_declares();
        let channels = demande.spec.canaux();
        // Ce que CoreAudio exclusif ne lit pas : l'endpoint et l'hôte d'origine
        // (il résout par nom), le backend demandé (le `cfg` et `exclusive_mode`
        // ont déjà choisi ce bras), la porte de rampe (le rappel n'en a pas),
        // le témoin de périphérique perdu (aucun rappel d'erreur, #1626).
        let _ = (
            demande.endpoint_id,
            demande.origin_host,
            demande.audio_backend,
            demande.exclusive,
            &demande.soft_mute,
            demande.device_gone,
        );

        let sortie = ExclusiveOutput::new(
            &device_name,
            sample_rate,
            bit_depth as u32,
            channels as u32,
            demande.starvation.clone(),
            demande.volume.clone(),
            demande.paused.clone(),
        )
        .map_err(|erreur| RefusDOuverture::OuvertureExclusiveRefusee {
            backend: "CoreAudio",
            erreur,
        })?;
        // Le contrat physique a été VÉRIFIÉ par lecture (`validate_physical_
        // format_contract`) : ce que le périphérique a ouvert est ce qui a été
        // demandé — cadence et canaux de la source.
        let info = sortie.format_info();
        let format = FormatOuvert::new(info.sample_rate, info.channels as u16);

        Ok(BackendCoreAudio {
            sortie,
            format,
            device_name,
            stop_rx: demande.stop_rx,
            paused: demande.paused.as_ref(),
            force_silent: demande.force_silent.as_ref(),
            position_ms: demande.position_ms,
            bloque: Arc::new(AtomicBool::new(false)),
        })
    }

    fn format_ouvert(&self) -> FormatOuvert {
        self.format
    }

    /// Le nom que la boucle producteur commune met dans son rapport de
    /// famine (REF-7) — celui que `record_feed_stall_failure("CoreAudio", …)`
    /// a toujours porté sur ce chemin.
    fn nom(&self) -> &'static str {
        "CoreAudio"
    }

    fn puits(&self) -> Puits<'a> {
        Puits::Flottant(Box::new(PuitsAnneauCoreAudio {
            anneau: self.sortie.ring().clone(),
            stop_rx: self.stop_rx,
            paused: self.paused,
            force_silent: self.force_silent,
            bloque: self.bloque.clone(),
        }))
    }

    /// `audio_unit.start()` — la dernière étape de l'ancien `new`. Le refus
    /// garde le message et le rapporteur d'avant (`record_exclusive_open_
    /// failure`, via `rapporter`) : c'était un échec d'OUVERTURE pour
    /// l'utilisateur, il le reste.
    fn demarrer(&mut self) -> Result<(), RefusDOuverture> {
        self.sortie
            .start()
            .map_err(|erreur| RefusDOuverture::OuvertureExclusiveRefusee {
                backend: "CoreAudio",
                erreur,
            })
    }

    /// `disponible` et `capacite` seulement. CoreAudio n'a pas de rappel
    /// d'erreur (#1626) : `peripherique_perdu` est toujours faux. Les
    /// sous-alimentations du pilote et les erreurs de rappel ne sont pas
    /// mesurées sur ce chemin : `None`, jamais zéro.
    fn observer(&self) -> Observation {
        let anneau = self.sortie.ring();
        Observation {
            disponible: anneau.available(),
            capacite: anneau.capacity(),
            peripherique_perdu: false,
            sous_alimentations_pilote: None,
            erreurs_de_rappel: None,
        }
    }

    /// Le vidage borné du bras (#3108) — JAMAIS sans fin : face à un rappel de
    /// rendu mort, l'anneau ne se vide jamais, et sans échéance le fil restait
    /// vivant, la zone « en lecture », le réexamen des branchements gelé.
    /// La borne est calculée par l'appelant (`drain_deadline_for`). Pendant le
    /// vidage, la position publiée recule vers ce qui est réellement joué
    /// (alimenté − encore en attente), comme sur le chemin partagé.
    fn drainer(&mut self, borne: std::time::Duration) -> Vidage {
        let device_name = &self.device_name;
        let ring = self.sortie.ring();
        let stop_rx = self.stop_rx;
        let force_silent = self.force_silent;
        let position_ms = self.position_ms;
        let output_sr = self.format.cadence;
        let output_ch = self.format.canaux;

        let fed_position_ms = position_ms.load(Ordering::Relaxed);
        let mut drained_naturally = false;
        let drain_deadline = borne;
        let drain_started = std::time::Instant::now();
        loop {
            if stop_rx.try_recv().is_ok() {
                break;
            }
            if force_silent.load(Ordering::Relaxed) {
                break;
            }
            let remaining = ring.available();
            if remaining == 0 {
                drained_naturally = true;
                break;
            }
            if drain_started.elapsed() >= drain_deadline {
                warn!(
                    device = %device_name,
                    remaining_samples = remaining,
                    "local_audio_exclusive_drain_timeout"
                );
                break;
            }
            if output_sr > 0 && output_ch > 0 {
                let ring_ms =
                    (remaining as f64 / output_ch as f64 / output_sr as f64 * 1000.0) as u64;
                position_ms.store(fed_position_ms.saturating_sub(ring_ms), Ordering::Relaxed);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Vidage {
            vide: drained_naturally,
            restant: ring.available(),
            position_alimentee_ms: fed_position_ms,
        }
    }
}

/// Joue la piste sur CoreAudio en mode exclusif (hog), au format source,
/// jusqu'à la fin du flux ou l'ordre d'arrêt. Terminal : quand il rend, le
/// fil de lecture n'a plus rien à faire.
pub(super) fn jouer_via_coreaudio(entrees: EntreesCoreAudio) {
    let EntreesCoreAudio {
        device_name,
        url,
        sample_rate,
        bit_depth,
        channels,
        data_offset,
        header_buf,
        mut reader,
        frame_bytes,
        spec,
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
    // `frame_bytes` était la largeur de trame que la boucle propre du bras
    // recalculait à chaque lecture ; l'étage la déduit de `spec`.
    let _ = frame_bytes;

    info!(
        device = %device_name,
        sample_rate,
        bit_depth,
        channels,
        "local_audio_exclusive_mode_active"
    );

    // CoreAudio n'a AUCUN rappel d'erreur (#1626) : ce témoin n'est jamais
    // levé. La boucle commune le relit ; il reste faux toute la piste.
    let device_gone = Arc::new(AtomicBool::new(false));
    // La porte de rampe n'existe que pour le type de la demande : le rappel
    // CoreAudio n'a pas de rampe, et `ouvrir` ne la lit pas.
    let soft_mute = crate::audio::soft_mute::SoftMuteGate::new(
        Arc::new(AtomicU32::new(0)),
        dop_active.clone(),
        pure_bypass.clone(),
        true,
    );
    let demande = DemandeDOuverture {
        spec,
        device_name: &device_name,
        endpoint_id: None,
        origin_host: None,
        audio_backend: "coreaudio",
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
    let mut backend = match BackendCoreAudio::ouvrir(&demande) {
        Ok(backend) => backend,
        Err(refus) => {
            refus.rapporter(&device_name, &open_failure);
            playing.store(false, Ordering::SeqCst);
            return;
        }
    };
    // Le rendu démarre AVANT le premier octet, comme quand `start` était la
    // dernière étape de `new` : le rappel tire du silence tant que l'anneau
    // est vide. Rien n'est mis entre l'ouverture et le démarrage.
    if let Err(refus) = backend.demarrer() {
        refus.rapporter(&device_name, &open_failure);
        drop(backend);
        playing.store(false, Ordering::SeqCst);
        return;
    }

    info!(device = %device_name, url = %url, "local_audio_exclusive_playing");
    // CoreAudio exclusif : `resolve_output_device` retombe sur le
    // périphérique système quand le nom stocké n'existe plus (DAC
    // débranché, renommé, routage macOS changé). `opened_id` reste
    // `None` : l'`AudioDeviceID` est un entier réattribué au
    // redémarrage, ce n'est pas une identité qu'on peut afficher.
    note_opened_device(
        "CoreAudio",
        &device_name,
        backend.peripherique_ouvert(),
        None,
    );

    // Feed audio data (no resampling needed -- hardware is set to source rate)
    let pcm_data = if data_offset < header_buf.len() {
        header_buf[data_offset..].to_vec()
    } else {
        Vec::new()
    };

    let mut total_frames_fed: u64 = 0;

    // Read and feed the rest of the stream
    let mut read_buf = vec![0u8; 65536];

    // ── La frontière producteur → puits ────────────────────────────────
    //
    // Le même étage que le chemin partagé, au format IDENTITÉ : la sortie est
    // ouverte à la cadence et aux canaux de la source (contrat physique
    // vérifié), donc ni adaptation de canaux ni rééchantillonnage — `convertir`
    // rend ses mots tels quels. Le DoP n'est pas refusé sur ce bras : il
    // traverse en f32 jusqu'à l'AudioUnit, comme avant (D1).
    let mut etage = EtageDeConversion {
        pcm: LocalPcmProcessor {
            eq: &eq,
            convolver: &convolver,
            crossfeed: &crossfeed,
            pure_bypass: &pure_bypass,
            mono_downmix: &mono_downmix,
            dop_active: &dop_active,
            volume: &volume,
            user_volume: &user_volume_ref,
            rg_factor: &rg_factor_ref,
        },
        en_attente: pcm_data,
        resampler: None,
        resample_leftover: Vec::new(),
        pcm_kind: LocalPcmKind::for_bit_depth(bit_depth),
        spec,
        sortie: backend.format_ouvert(),
        needs_resample: false,
    };
    let mut puits = match backend.puits() {
        Puits::Flottant(puits) => puits,
        Puits::Natif(_) => unreachable!("BackendCoreAudio ne fournit qu'un puits flottant (D1)"),
    };
    let mut refuser_le_porteur_dop = |_dop: bool, _src_sr: u32, _src_ch: u16| -> bool { false };

    // Process leftover from header read
    // #3108 — le verdict de blocage était JETÉ aux trois sites de
    // ce chemin, seul de tous les chemins de lecture. Conséquence
    // exacte du constat : l'anneau exclusif tient deux secondes
    // d'audio (sa contenance, dans `ExclusiveOutput::new`), il se
    // remplit une fois, le rappel de rendu ne tire rien, et la
    // position reste sur 2 000 ms pour toujours — sans un mot.
    // Depuis REF-7 c'est la boucle commune qui le lit et le DIT :
    // un puits déjà mort à l'amorçage est constaté par elle, pas
    // ici, comme sur le chemin partagé.
    match etage.pousser(&mut *puits, &mut refuser_le_porteur_dop, &mut |_| {}) {
        // L'amorce comptait ses trames sans regarder le verdict : ce
        // compte est la position rapportée, il ne change pas.
        PousseeVersLePuits::Poussee { trames_source }
        | PousseeVersLePuits::PuitsMort { trames_source } => {
            total_frames_fed += trames_source;
        }
        PousseeVersLePuits::RienAPousser => {}
        PousseeVersLePuits::PorteurDopRefuse => {
            unreachable!("ce bras ne refuse jamais un porteur DoP (D1)")
        }
    }

    // ── La boucle producteur commune ───────────────────────────────────
    //
    // Celle de `local.rs`, la même que les deux pistes du chemin partagé :
    // `stop_rx`, `force_silent`, EOF et le puits mort sont SES témoins, et
    // c'est elle qui rapporte la famine, sous le nom de ce backend (REF-7).
    // La clé de flux (#3318) est celle de l'URL tirée : les erreurs de
    // lecture de ce bras la portent désormais, comme celles du chemin partagé.
    let cle_de_flux = crate::poller::decisions::stream_id_de_l_uri(Some(&url));
    let producteur = BoucleProducteur {
        backend: backend.nom(),
        role: RoleDeLaBoucle::PisteInitiale,
        device_name: &device_name,
        cle_de_flux: cle_de_flux.as_deref(),
        stop_rx: &stop_rx,
        force_silent: force_silent.as_ref(),
        device_gone: device_gone.as_ref(),
        position_ms: position_ms.as_ref(),
        open_failure: open_failure.as_ref(),
        debut_du_flux: std::time::Instant::now(),
    };
    let mut compteurs = CompteursDePiste {
        total_bytes_read: 0,
        total_frames_fed,
        seek_offset,
        skip_bytes: 0,
        skipped_bytes: 0,
        premiere_donnee_journalisee: false,
    };
    let mut http_eof_excl = false;
    let fin = producteur.tourner(
        &mut reader,
        &mut read_buf,
        &mut etage,
        &mut *puits,
        &mut refuser_le_porteur_dop,
        &mut compteurs,
        &mut |_| true,
    );
    match fin {
        FinDeBoucle::FinDeFlux => http_eof_excl = true,
        // Arrêt demandé, silence forcé — ou puits mort : la boucle ne
        // distingue pas, mais elle a déjà rapporté le blocage
        // (`record_feed_stall_failure`, au nom de `backend.nom()`), et le
        // puits l'a mémorisé pour la suite.
        FinDeBoucle::Interrompue => {}
        FinDeBoucle::PorteurDopRefuse | FinDeBoucle::Abandon => {
            unreachable!("ni refus DoP ni abandon possibles : les deux rappels sont constants")
        }
    }
    total_frames_fed = compteurs.total_frames_fed;
    // La piste n'a PAS fini sur un puits mort : `http_eof_excl` reste
    // faux, donc aucune fin naturelle n'est signalée et la file n'avance
    // pas vers un morceau qui heurterait le même périphérique mort — et
    // rien de plus n'est rendu à un rappel qui ne tire plus.
    let feed_stalled = backend.puits_bloque();

    if http_eof_excl {
        report_incomplete_local_pcm_probe(etage.pcm_kind, etage.en_attente.len());
    }

    // Fin de piste : rendre au périphérique ce que le convolveur
    // retient encore. Sans ça, `latency_frames()` trames restaient
    // dans le moteur et la fin de chaque piste était tronquée
    // (#2209, revue JP Robbe — la fonction etait morte).
    // REF-7 : c'est le geste de l'étage, qui tire lui-même la queue par
    // `flush_local_dsp` et la livre par son unique site d'écriture. Les
    // trames de la queue ne sont plus ajoutées à `total_frames_fed` :
    // ce compte ne servait plus qu'au journal de sortie, et l'étage ne
    // rend pas leur nombre.
    if !feed_stalled {
        etage.rendre_la_queue_du_dsp(&mut *puits);
    }

    // Signal natural track end BEFORE draining when the HTTP
    // stream reached EOF, so the orchestrator can detect
    // end-of-track even if force_silent is set during slow drain.
    if http_eof_excl {
        track_ended_naturally.store(true, Ordering::SeqCst);
        track_ended_generation.store(my_generation, Ordering::SeqCst);
        TRACK_END_NOTIFY.notify_one();
    }

    // Wait for ring buffer to drain — JAMAIS sans fin (#3108). La boucle
    // vit dans le backend (`drainer`) ; ici on ne calcule que sa borne, à
    // partir de ce qu'il observe.
    let drain_deadline = drain_deadline_for(
        backend.observer().disponible,
        sample_rate as u64,
        channels as u64,
    );
    let vidage = backend.drainer(drain_deadline);
    if http_eof_excl && vidage.vide {
        position_ms.store(vidage.position_alimentee_ms, Ordering::Relaxed);
    }

    // ExclusiveOutput::drop() restores sample rate and releases hog mode
    drop(backend);
    if play_generation.load(Ordering::SeqCst) == my_generation {
        playing.store(false, Ordering::SeqCst);
    }
    info!(
        device = %device_name,
        frames = total_frames_fed,
        "local_audio_exclusive_stopped"
    );
}
