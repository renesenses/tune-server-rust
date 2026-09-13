//! REF-8 (#2219) : le trait backend minimal de la sortie locale, et son
//! premier implémenteur — CPAL partagé.
//!
//! Un backend fait quatre choses et rien d'autre : il **ouvre** un
//! périphérique au format demandé, il **fournit** le puits que le producteur
//! alimente, il **démarre** le rendu, il se laisse **observer** pendant la
//! piste et **drainer** à sa fin. Tout ce qui décode, convertit, refuse un
//! porteur DoP ou compte des trames reste au producteur (`EtageDeConversion`,
//! `BoucleProducteur`, R1) : le trait ne le voit pas.
//!
//! Trois décisions, prises avant la première ligne (plan de nuit du 12/09) :
//!
//! * **D1 — le trait dit QUEL puits il fournit.** [`Puits::Flottant`] pour
//!   CPAL et CoreAudio (le mot est un `f32`, le DSP passe par là) ;
//!   [`Puits::Natif`] pour WASAPI et ASIO natif (des octets au format
//!   ouvert, intacts jusqu'au pilote, #3985).
//! * **D2 — le backend possède son anneau.** `play_url` ne crée plus
//!   d'anneau et n'en passe plus d'`Arc` : il reçoit un puits et une
//!   [`Observation`]. Le vidage de fin de piste est [`BackendLocal::drainer`].
//!   La contenance reste `cadence × canaux × 2` (deux secondes) — c'est le
//!   « figée à 2 s » de #3108, gardé par texte.
//! * **D3 — le régime de volume ne change pas.** Le rappel temps réel ne voit
//!   jamais le trait : il lit ses atomiques et son anneau **par valeur**,
//!   comme aujourd'hui (`SoftMuteRamp` comprise). Pas de `Box<dyn>`, pas de
//!   `Mutex` sur le chemin du rappel ; le seul `Box` de ce module vit côté
//!   producteur, une fois par piste.
//!
//! Le module est un enfant de `local` : il voit les aides privées du parent
//! (`build_int_stream`, `make_stream_error_cb`, `feed_ring_abortable`,
//! `classify_open_failure`, `journaliser_les_teneurs_du_pcm`) par
//! `use super::*`, comme les trois bras exclusifs (#4000). Ces aides restent
//! dans `local.rs` parce que la branche compressée s'en sert aussi.

use super::*;
use crate::outputs::traits::PuitsNatif;

/// Ce que `play_url` sait au moment d'ouvrir, et rien de plus.
///
/// Les `Arc` sont EMPRUNTÉS : le backend clone ceux que son rappel temps réel
/// lit par valeur (D3) et garde des références sur ceux que le puits relit
/// pendant qu'il attend une place dans l'anneau. `stop_rx` n'est pas
/// clonable : le fil de lecture le tient, le backend le regarde.
pub(super) struct DemandeDOuverture<'a> {
    /// Le format de la SOURCE (R5) : cadence, profondeur, canaux.
    pub(super) spec: AudioSpec,
    pub(super) device_name: &'a str,
    /// Identifiant d'endpoint réglé sur la zone (WASAPI, ALSA `hw:…`).
    pub(super) endpoint_id: Option<&'a str>,
    /// L'hôte dont vient `device_name` (#3230).
    pub(super) origin_host: Option<&'a str>,
    /// Le backend demandé par la zone (`"asio"`, `"wasapi"`, `"alsa"`, …).
    pub(super) audio_backend: &'a str,
    /// Mode exclusif demandé. Les quatre bras sont choisis par `cfg` et par
    /// ce drapeau dans `play_url` ; le backend le reçoit pour ne pas avoir
    /// à le redemander.
    pub(super) exclusive: bool,
    /// Les trois témoins d'arrêt du fil de lecture, relus par le puits.
    pub(super) stop_rx: &'a std::sync::mpsc::Receiver<()>,
    pub(super) paused: &'a Arc<AtomicBool>,
    pub(super) force_silent: &'a Arc<AtomicBool>,
    /// Ce que le rappel lit par valeur.
    pub(super) volume: &'a Arc<AtomicU32>,
    /// Levé par le rappel d'erreur quand le périphérique disparaît (#1626).
    /// Créé par `play_url` parce que `BoucleProducteur` le relit aussi.
    pub(super) device_gone: &'a Arc<AtomicBool>,
    /// Le compteur de famine (#3205), confié à l'anneau du format retenu.
    pub(super) starvation: &'a Arc<RingStarvation>,
    /// Porte de la rampe anti-« ploc » (#1590).
    pub(super) soft_mute: crate::audio::soft_mute::SoftMuteGate,
    /// La position publiée, que le vidage fait reculer vers ce qui est
    /// réellement joué.
    pub(super) position_ms: &'a AtomicU64,
}

/// Pourquoi rien ne s'est ouvert — ou ne démarre pas.
///
/// Aucun bras `_` : chaque refus est nommé, à l'image de `MixError` et de
/// `RefusNatif`. Les données portées sont exactement celles que les trois
/// rapporteurs historiques écrivent aujourd'hui ; [`RefusDOuverture::rapporter`]
/// les leur passe, et les noms d'événement journalisés sont conservés mot
/// pour mot.
#[derive(Debug)]
pub(super) enum RefusDOuverture {
    /// Le périphérique réglé sur la zone est introuvable et l'hôte n'offre
    /// aucun repli (chemin partagé, `find_device_with_fallback` rend `None`).
    PeripheriqueIntrouvable(SharedDeviceResolution),
    /// La cascade f32 → i32 → i16, aux deux cadences, est épuisée : la faute
    /// est au périphérique, pas à l'encodage.
    ToutesLesTentativesRefusees {
        cause: OpenFailure,
        premiere_erreur: String,
        seconde_erreur: String,
    },
    /// Un transport exclusif a refusé l'ouverture (CoreAudio, ASIO, WASAPI).
    /// Aucun repli vers un autre endpoint ni vers le mode partagé. N'existe
    /// que là où un transport exclusif existe — même `cfg` que
    /// `record_exclusive_open_failure`. Construit par les bras exclusifs
    /// quand ils implémentent le trait ; aucun sur cette tranche, d'où
    /// l'`allow`.
    #[cfg(any(target_os = "windows", target_os = "macos", test))]
    #[allow(dead_code)]
    OuvertureExclusiveRefusee {
        backend: &'static str,
        erreur: String,
    },
    /// Le périphérique est ouvert mais refuse de démarrer le rendu.
    DemarrageRefuse { erreur: String },
}

impl RefusDOuverture {
    /// Journalise le refus et renseigne `open_failure`, le créneau que
    /// `take_output_failure()` draine à chaque tick du sondeur.
    ///
    /// Trois des quatre bras passent par les rapporteurs qui existaient déjà
    /// (`record_shared_device_not_found`, `record_exclusive_open_failure`) ;
    /// le quatrième reprend le `warn!` et l'écriture que `play_url` faisait
    /// en ligne. `DemarrageRefuse` n'écrit rien à l'écran : c'était déjà le
    /// cas, le site journalise et s'arrête.
    pub(super) fn rapporter(
        &self,
        device_name: &str,
        open_failure: &std::sync::Mutex<Option<String>>,
    ) {
        match self {
            RefusDOuverture::PeripheriqueIntrouvable(resolution) => {
                record_shared_device_not_found(*resolution, device_name, open_failure);
            }
            RefusDOuverture::ToutesLesTentativesRefusees {
                cause,
                premiere_erreur,
                seconde_erreur,
            } => {
                warn!(
                    device = %device_name,
                    first_error = %premiere_erreur,
                    second_error = %seconde_erreur,
                    hint = %cause.log_hint(),
                    "audio_stream_build_failed_all_formats"
                );
                // Hand the poller something to say. Without
                // this the zone plays on in silence until the
                // stall heuristics fire ~73 s later, with no
                // message anywhere the user can see.
                if let Ok(mut slot) = open_failure.lock() {
                    *slot = Some(format!(
                        "Sortie « {device_name} » : {}.",
                        cause.user_message()
                    ));
                }
            }
            #[cfg(any(target_os = "windows", target_os = "macos", test))]
            RefusDOuverture::OuvertureExclusiveRefusee { backend, erreur } => {
                record_exclusive_open_failure(backend, device_name, erreur, open_failure);
            }
            RefusDOuverture::DemarrageRefuse { erreur } => {
                warn!(error = %erreur, "audio_stream_play_failed");
            }
        }
    }
}

impl std::fmt::Display for RefusDOuverture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefusDOuverture::PeripheriqueIntrouvable(resolution) => {
                f.write_str(resolution.log_event())
            }
            RefusDOuverture::ToutesLesTentativesRefusees {
                cause,
                premiere_erreur,
                seconde_erreur,
            } => write!(
                f,
                "{} ({premiere_erreur} ; {seconde_erreur})",
                cause.log_hint()
            ),
            #[cfg(any(target_os = "windows", target_os = "macos", test))]
            RefusDOuverture::OuvertureExclusiveRefusee { backend, erreur } => {
                write!(f, "{backend}: {erreur}")
            }
            RefusDOuverture::DemarrageRefuse { erreur } => f.write_str(erreur),
        }
    }
}

/// Le puits qu'un backend fournit — et de quel mot il est fait (D1).
///
/// Un `Box` côté producteur, une fois par piste : le rappel temps réel n'en
/// voit rien, il tient l'autre bout de l'anneau par valeur (D3). Le puits ne
/// dépend pas de l'emprunt du backend : `play_url` peut démarrer, observer et
/// alimenter dans le même tour de boucle.
pub(super) enum Puits<'a> {
    /// Des mots `f32` entrelacés au format ouvert : le chemin DSP (CPAL
    /// partagé, CoreAudio).
    Flottant(Box<dyn PuitsDEchantillons + 'a>),
    /// Des octets au format ouvert, intacts (WASAPI, ASIO natif — #3985).
    /// Construit par les bras exclusifs quand ils implémentent le trait ;
    /// aucun sur cette tranche, d'où l'`allow`.
    #[allow(dead_code)]
    Natif(Box<dyn PuitsNatif + 'a>),
}

/// Ce que le fil de lecture relève pendant la piste, sans verrou.
///
/// Un backend qui ne mesure pas un compteur rend `None` — jamais zéro : un
/// zéro se lirait « aucune sous-alimentation » là où il veut dire « personne
/// n'a compté » (CoreAudio n'observe que `disponible`, #1626).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Observation {
    /// Mots (ou trames natives) en attente dans l'anneau.
    pub(super) disponible: usize,
    /// Contenance de l'anneau, dans la même unité.
    pub(super) capacite: usize,
    /// Le rappel d'erreur a vu partir le périphérique. Toujours faux pour un
    /// backend sans rappel d'erreur.
    pub(super) peripherique_perdu: bool,
    /// Sous-alimentations du PILOTE (#3205), `None` si non mesuré.
    pub(super) sous_alimentations_pilote: Option<u64>,
    /// Erreurs du rappel de rendu, `None` si non mesuré.
    pub(super) erreurs_de_rappel: Option<u64>,
}

/// Ce que le vidage de fin de piste a rendu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Vidage {
    /// L'anneau s'est vidé de lui-même : la fin naturelle peut être déclarée.
    pub(super) vide: bool,
    /// Ce qui restait en attente quand le vidage s'est arrêté (0 si `vide`).
    pub(super) restant: usize,
    /// La position que l'alimentation avait atteinte au début du vidage —
    /// celle à republier une fois l'anneau vide.
    pub(super) position_alimentee_ms: u64,
}

/// Le contrat qu'un backend de sortie locale remplit — et rien de plus.
///
/// `'a` est la durée de vie des témoins d'arrêt que `play_url` tient (le
/// `Receiver` du stop, les atomiques de pause et de silence) : le backend et
/// son puits les regardent, ils ne les possèdent pas.
///
/// | méthode | CPAL partagé (ici) | CoreAudio | WASAPI | ASIO |
/// |---|---|---|---|---|
/// | `ouvrir` | résolution, décision de cadence, cascade f32/i32/i16 | `prepare_exclusive_device` + rappel | `resolve_wasapi_endpoint` + `Initialize` exclusif | `try_with_asio_device_lock`, transport, `build_native_stream` |
/// | `puits` | `Flottant` (`PuitsAnneauCpal`) | `Flottant` | `Natif` | `Natif` (route native) |
/// | `demarrer` | `stream.play()` | `audio_unit.start()` | `start()` | `stream.play()` |
/// | `observer` | `available`, `device_gone`, sous-alimentations pilote | `available` seul | `available`, `underrun_count`, `callback_error_count` | idem |
/// | `drainer` | borné, position réelle | borné (`drain_deadline_for`) | à borner | double borne (`asio_drain_timeout`) |
pub(super) trait BackendLocal<'a>: Sized {
    /// Réserve le périphérique et pose le rappel temps réel. Ne démarre pas.
    /// Rend le format réellement ouvert par [`BackendLocal::format_ouvert`].
    fn ouvrir(demande: &DemandeDOuverture<'a>) -> Result<Self, RefusDOuverture>;

    /// Ce que le périphérique a RÉELLEMENT ouvert (cadence, canaux) — le
    /// format des mots que le puits attend, pas celui de la source.
    fn format_ouvert(&self) -> FormatOuvert;

    /// Le puits que le producteur alimente. Le backend possède l'anneau ; le
    /// puits en est l'écrivain, le rappel en est le lecteur.
    fn puits(&self) -> Puits<'a>;

    /// Démarre le rendu — après le pré-remplissage, jamais avant.
    fn demarrer(&mut self) -> Result<(), RefusDOuverture>;

    /// Un relevé sans verrou : disponible, capacité, périphérique perdu,
    /// compteurs.
    fn observer(&self) -> Observation;

    /// Vidage de fin de piste, borné par `borne`. Fait reculer la position
    /// publiée vers ce qui est réellement joué et rend où il s'est arrêté.
    fn drainer(&mut self, borne: std::time::Duration) -> Vidage;

    /// Le nom du backend tel que les journaux et les messages à l'écran le
    /// disent — `"CPAL"`, `"CoreAudio"`, `"WASAPI"`, `"ASIO"`. REF-7 (#2219) :
    /// c'est la boucle producteur commune qui rapporte la famine
    /// (`record_feed_stall_failure`), une fois, et elle ne sait pas sur quel
    /// backend elle tourne ; ce nom est ce qu'elle met dans le rapport.
    fn nom(&self) -> &'static str;
}

/// Le puits du chemin CPAL partagé : l'anneau flottant que draine le rappel.
///
/// Il porte les trois témoins d'arrêt du fil de lecture parce que l'attente a
/// lieu ICI : quand l'anneau est plein, c'est `feed_ring_abortable` qui dort,
/// et c'est donc lui qu'un arrêt doit pouvoir réveiller. Un puits qui ne
/// bloque jamais — un puits de capture — n'en aura pas besoin, et c'est
/// précisément pourquoi ils vivent dans l'implémentation et non dans le trait.
///
/// R8 : l'anneau est un `Arc` partagé avec le backend qui le possède, plus un
/// emprunt sur `play_url` — c'est ce qui permet de démarrer le flux pendant
/// que le puits est prêté à la boucle producteur.
struct PuitsAnneauCpal<'a> {
    anneau: Arc<RingBuf>,
    stop_rx: &'a std::sync::mpsc::Receiver<()>,
    paused: &'a AtomicBool,
    force_silent: &'a AtomicBool,
}

impl PuitsDEchantillons for PuitsAnneauCpal<'_> {
    fn ecrire(&mut self, mots: &[f32]) -> bool {
        feed_ring_abortable(
            &self.anneau,
            mots,
            self.stop_rx,
            self.paused,
            Some(self.force_silent),
        )
    }
}

/// CPAL en mode partagé : le backend de tous les DAC qui ne sont pas en
/// exclusif, sur les trois systèmes.
///
/// Il possède le flux cpal et l'anneau flottant (D2). Le rappel de rendu — la
/// fermeture de `build_stream` dans `ouvrir`, et
/// `render_local_shared_integer_callback` pour les DAC qui refusent le
/// flottant — est celui d'avant, inchangé : volume et rampe anti-« ploc »
/// restent dans le rappel, par valeur (D3).
pub(super) struct BackendCpal<'a> {
    stream: cpal::Stream,
    anneau: Arc<RingBuf>,
    sortie: FormatOuvert,
    device_name: String,
    stop_rx: &'a std::sync::mpsc::Receiver<()>,
    paused: &'a AtomicBool,
    force_silent: &'a AtomicBool,
    device_gone: Arc<AtomicBool>,
    starvation: Arc<RingStarvation>,
    position_ms: &'a AtomicU64,
}

impl<'a> BackendLocal<'a> for BackendCpal<'a> {
    /// La cascade d'ouverture du chemin partagé, telle qu'elle vivait dans
    /// `play_url` : résolution du périphérique, décision de cadence, flux f32
    /// à la cadence choisie, repli à la cadence source, puis i32 et i16 aux
    /// deux cadences. Le premier format accepté gagne.
    fn ouvrir(demande: &DemandeDOuverture<'a>) -> Result<Self, RefusDOuverture> {
        let device_name = demande.device_name.to_string();
        let sample_rate = demande.spec.cadence();
        let channels = demande.spec.canaux();
        let volume = demande.volume.clone();
        let paused = demande.paused.clone();
        let force_silent = demande.force_silent.clone();
        let device_gone = demande.device_gone.clone();
        let starvation = demande.starvation.clone();
        let soft_mute = demande.soft_mute.clone();
        // Le mode partagé sert aussi une zone réglée « exclusif » là où aucun
        // transport exclusif n'existe (Linux) : c'est `play_url` qui
        // dispatche par `cfg`, ce backend n'a rien à en faire.
        let _ = demande.exclusive;

        let host = select_host(demande.audio_backend);
        // Nom de la VARIANTE cpal ("Wasapi", "Alsa", "Asio", "CoreAudio").
        // `&'static str`, donc aucun emprunt sur `host`.
        let host_id_name: &'static str = host.id().name();
        let Some((device, fell_back)) = find_device_with_fallback(
            &host,
            &device_name,
            demande.endpoint_id,
            demande.origin_host,
        ) else {
            return Err(RefusDOuverture::PeripheriqueIntrouvable(
                SharedDeviceResolution::WavStreamNotFound,
            ));
        };
        if fell_back {
            info!(
                original = %device_name,
                "audio_device_fallback_used_for_wav_stream"
            );
        }

        // Determine the output config for shared mode.
        //
        // Strategy: prefer the device's default/native sample rate and
        // resample with rubato when the source rate differs.  This is more
        // reliable than trying to open the device at the source rate:
        //
        // - On macOS, cpal's CoreAudio backend does NOT call
        //   `set_sample_rate` for output streams (only for input).  So
        //   `build_output_stream` at 96 kHz "succeeds" (CoreAudio inserts
        //   an internal converter), but the conversion is unreliable on
        //   many devices/macOS versions and produces white noise.
        //
        // - On Windows WASAPI shared mode, the system mixer runs at a
        //   fixed rate (usually 48 kHz); requesting a different rate may
        //   be rejected or silently mis-converted.
        //
        // By always opening at the device's native rate and doing our own
        // high-quality sinc resampling (rubato), we guarantee correct
        // output on all platforms.
        //
        // If the source rate happens to match the device rate, no
        // resampling occurs (zero overhead).
        //
        // #3575 - le PCM reellement OUVERT, retenu HORS du bloc de decision
        // de cadence : opened_endpoint_id meurt avec ce bloc, et le chemin
        // d'echec qui en a besoin est 300 lignes plus bas.
        #[cfg(target_os = "linux")]
        let pcm_ouvert = device.id().map(|id| id.to_string()).unwrap_or_default();
        let output_config = {
            // First, get the device's default config (reflects actual
            // operating rate on most platforms).
            let default_cfg = match device.default_output_config() {
                Ok(c) => Some(c.config()),
                Err(e) => {
                    // #3575 — `device_default_sr=None` était une ABSENCE, et
                    // une absence ne prouve rien : elle se lisait « ce
                    // périphérique n'annonce pas de cadence par défaut »
                    // alors qu'elle veut dire « on vient d'échouer à
                    // l'ouvrir ». Sur ALSA cette sonde ouvre le MÊME PCM que
                    // la lecture (`cpal-0.17.3/src/host/alsa/mod.rs:457`) :
                    // son échec EST le premier `EBUSY`, quelques
                    // millisecondes avant celui qui arrêtera la zone.
                    //
                    // Relevé de Belkadi Yacine, 13 ouvertures sur 13 :
                    // `None` sur les DIX échecs, une cadence réellement lue
                    // sur les TROIS réussites. La ligne, elle, manquait.
                    warn!(
                        device = %device_name,
                        error = %e,
                        "local_audio_default_config_probe_failed"
                    );
                    None
                }
            };
            let default_sr = default_cfg.as_ref().map(|c| c.sample_rate);

            // Ce que l'énumération de cpal RÉPOND. Le filtre est
            // TAUTOLOGIQUE quand l'énumération est fabriquée :
            // `find_matching_config` recopie la cadence demandée dans le
            // `StreamConfig` qu'il rend, donc `c.sample_rate ==
            // sample_rate` est vrai par construction dès qu'une plage
            // quelconque a été retenue — et sur WASAPI toutes les plages
            // sont retenues sans test (#2862). Cette réponse n'est donc
            // plus qu'une ENTRÉE de la décision (#3233) : c'est
            // `decide_local_rate_opening` qui tranche, en regardant ce que
            // la réponse vaut. Elle n'est calculée que si le périphérique
            // n'est pas déjà à la bonne cadence — sur WASAPI l'énumération
            // déroule 147 formats, sur ASIO elle touche le pilote.
            let enumerated = if default_sr == Some(sample_rate) {
                None
            } else {
                find_matching_config(&device, channels, sample_rate)
                    .filter(|c| c.sample_rate == sample_rate)
            };
            // Sur ALSA, `endpoint_id` est le nom de PCM (`hw:CARD=…`,
            // `dmix:CARD=…`) : c'est LUI qui dit si le « oui » vient du
            // pilote ou d'un rééchantillonneur (#1655). Le journaliser
            // ici est la ligne qui manquait pour trancher un relevé de
            // terrain sans y retourner.
            let opened_endpoint_id = device.id().map(|id| id.to_string()).unwrap_or_default();
            let rate_evidence =
                sample_rate_evidence_for_device(host_id_name, &opened_endpoint_id, true);
            let decision = decide_local_rate_opening(
                sample_rate,
                default_sr,
                enumerated.is_some(),
                rate_evidence,
            );

            // Ce que la décision ouvre RÉELLEMENT — la seule chose qu'on ait
            // le droit de remonter.
            let (chosen, opened_sr, reason) = match (decision, default_cfg, enumerated) {
                // Le périphérique y est déjà : aucune conversion, et rien à
                // régler côté matériel.
                (LocalRateOpening::DeviceAlreadyAtSourceRate, Some(cfg), _) => {
                    // Le bras NOMINAL — et le plus frequent : le DAC est
                    // deja a la cadence de la source. Les trois autres bras
                    // journalisent `endpoint_id` depuis #1655 ; celui-ci ne
                    // disait rien, si bien qu'un releve de terrain n'aurait
                    // vu QUE les cas anormaux et aurait conclu de travers
                    // sur la part de `hw:` dans le parc (#3209).
                    info!(
                        source_sr = sample_rate,
                        backend = %host_id_name,
                        endpoint_id = %opened_endpoint_id,
                        rate_support_measured = rate_evidence.is_measured(),
                        "local_audio_open_device_already_at_source_rate"
                    );
                    (cfg, sample_rate, None)
                }
                // L'énumération est une MESURE et elle retient la cadence :
                // on ouvre à la cadence de la source, exactement comme
                // avant. Témoin du cas nominal.
                //
                // A DSD256 file decodes to 352.8kHz; on a DAC left at
                // 44.1kHz by the OS the old code resampled 352.8k→44.1k in
                // real time, the sinc resampler underran and no sound came
                // out (Cyrille, FiiO K3 which natively supports 352.8kHz,
                // iFi Neo iDSD).
                (LocalRateOpening::AtSourceRateMeasured, _, Some(cfg)) => {
                    info!(
                        source_sr = sample_rate,
                        device_default_sr = ?default_sr,
                        backend = %host_id_name,
                        endpoint_id = %opened_endpoint_id,
                        rate_support_measured = rate_evidence.is_measured(),
                        "local_audio_open_at_source_rate_reported_supported"
                    );
                    // macOS: cpal's CoreAudio backend does NOT switch the
                    // device's hardware nominal rate for output streams (see
                    // the note above), so opening the cpal stream "at the
                    // source rate" leaves the DAC clocked at the OS rate and
                    // CoreAudio silently converts — which yields SILENCE for
                    // high-rate DSD→PCM (DSD128/256/512 all decode to
                    // 352.8kHz; only DSD64's 176.4k survived). We reach this
                    // branch precisely when the device SUPPORTS the source
                    // rate but its default differs, so set the hardware
                    // nominal rate explicitly (what the exclusive/hog path
                    // already does) — the DAC then actually clocks at
                    // 352.8kHz. Best-effort: if the device can't be
                    // resolved/set we fall through to today's behavior (no
                    // regression). Cyrille: iFi Neo iDSD / FiiO K3, DSD128+
                    // silent.
                    #[cfg(target_os = "macos")]
                    {
                        use coreaudio::audio_unit::macos_helpers;
                        if let Some(dev_id) =
                            macos_helpers::get_device_id_from_name(&device_name, false)
                        {
                            let want = cfg.sample_rate as f64;
                            match macos_helpers::set_device_sample_rate(dev_id, want) {
                                Ok(_) => info!(
                                    device = %device_name,
                                    to = cfg.sample_rate,
                                    "local_audio_coreaudio_nominal_rate_set_shared"
                                ),
                                Err(e) => warn!(
                                    error = %e,
                                    wanted = cfg.sample_rate,
                                    "local_audio_coreaudio_set_rate_failed"
                                ),
                            }
                        }
                    }
                    (cfg, sample_rate, None)
                }
                // On refuse la cadence de la source : rubato convertit. Une
                // décision qui change ce qui part au DAC ne passe jamais en
                // silence (#3209, #1655, #3233).
                (
                    LocalRateOpening::ResampleToDeviceRate {
                        device_sample_rate,
                        reason,
                    },
                    Some(cfg),
                    _,
                ) => {
                    warn!(
                        source_sr = sample_rate,
                        device_sr = device_sample_rate,
                        backend = %host_id_name,
                        endpoint_id = %opened_endpoint_id,
                        rate_support_measured = rate_evidence.is_measured(),
                        reason = reason.code(),
                        "local_audio_rate_mismatch_will_resample"
                    );
                    (cfg, device_sample_rate, Some(reason))
                }
                // Aucune cadence de périphérique connue : rien vers quoi
                // rééchantillonner, on ouvre à la cadence de la source en
                // dernier recours (PipeWire, etc.). Les bras `Some(cfg)`
                // ci-dessus étant exhaustifs pour `default_cfg = Some(..)`,
                // ce bras ne se prend qu'avec `default_cfg = None` — sauf
                // `AtSourceRateMeasured` sans `enumerated`, que
                // `decide_local_rate_opening` ne peut pas produire.
                _ => {
                    let cfg = find_matching_config(&device, channels, sample_rate).unwrap_or(
                        cpal::StreamConfig {
                            channels,
                            sample_rate,
                            buffer_size: cpal::BufferSize::Default,
                        },
                    );
                    let opened = cfg.sample_rate;
                    info!(
                        source_sr = sample_rate,
                        opened_sr = opened,
                        backend = %host_id_name,
                        endpoint_id = %opened_endpoint_id,
                        "local_audio_rate_last_resort_no_device_default"
                    );
                    (cfg, opened, None)
                }
            };
            note_rate_decision(ObservedRate {
                source_sample_rate: sample_rate,
                opened_sample_rate: opened_sr,
                reason,
                evidence_measured: rate_evidence.is_measured(),
            });
            chosen
        };

        // Build output stream at the chosen rate.
        let silent_cb_outer = force_silent.clone();
        // Gate: the cpal callback outputs silence until enough real data
        // has been buffered in the ring buffer.  This prevents stale or
        // garbage audio from reaching the DAC during track transitions.
        let data_started_shared = Arc::new(AtomicBool::new(false));
        let build_stream =
            |cfg: &cpal::StreamConfig,
             ring_cb: Arc<RingBuf>,
             vol_cb: Arc<AtomicU32>,
             paused_cb: Arc<AtomicBool>,
             silent_cb: Arc<AtomicBool>,
             ds_cb: Arc<AtomicBool>,
             min_buf: usize,
             soft_mute_cb: crate::audio::soft_mute::SoftMuteGate| {
                let mut ramp_cb = soft_mute_cb.ramp(cfg.sample_rate, cfg.channels);
                // Prélevé AVANT la fermeture de rendu (#3205).
                let famine_cb = ring_cb.starvation();
                device.build_output_stream(
                    cfg,
                    move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        // Rampe anti-« ploc » (#1590) — voir le callback du
                        // chemin compressé pour le détail. `arm(0)` rétablit la
                        // coupure franche sur DoP, PURE et sortie exclusive.
                        ramp_cb.arm(soft_mute_cb.armed_ms());
                        let silence =
                            paused_cb.load(Ordering::Relaxed) || silent_cb.load(Ordering::Relaxed);
                        if ramp_cb.begin(silence) == crate::audio::soft_mute::Rendering::Silent {
                            data.fill(0.0);
                            return;
                        }
                        // Wait for a minimum amount of data before starting
                        // to read from the ring buffer. This prevents the
                        // audio device from playing stale/garbage samples
                        // during track transitions.
                        if !ds_cb.load(Ordering::Acquire) {
                            if ring_cb.available() < min_buf {
                                data.fill(0.0);
                                return;
                            }
                            ds_cb.store(true, Ordering::Release);
                        }
                        let read = ring_cb.pop(data);
                        let v = vol_cb.load(Ordering::Relaxed) as f32 / 1000.0;
                        ramp_cb.apply(&mut data[..read], v);
                        if read < data.len() {
                            data[read..].fill(0.0);
                        }
                    },
                    make_stream_error_cb(device_gone.clone(), famine_cb),
                    None,
                )
            };

        let ring_cap = (output_config.sample_rate as usize) * (output_config.channels as usize) * 2;
        starvation.begin_stream(output_config.sample_rate, output_config.channels);
        let ring_buf = Arc::new(RingBuf::new_metered(ring_cap, starvation.clone()));
        ring_buf.clear(); // Defensive: zero-fill before callback can read
        // Minimum buffer: ~200ms of audio before the callback starts reading.
        // sr * ch / 5 = 200ms of interleaved samples.
        let min_buffer =
            (output_config.sample_rate as usize) * (output_config.channels as usize) / 5;
        let stream_result = build_stream(
            &output_config,
            ring_buf.clone(),
            volume.clone(),
            paused.clone(),
            silent_cb_outer.clone(),
            data_started_shared.clone(),
            min_buffer,
            soft_mute.clone(),
        );

        let (stream, actual_config, ring) = match stream_result {
            Ok(s) => (s, output_config, ring_buf),
            Err(first_err) => {
                // Last resort: try the source sample rate directly —
                // some platforms (PipeWire) accept arbitrary rates.
                let source_cfg = cpal::StreamConfig {
                    channels,
                    sample_rate,
                    buffer_size: cpal::BufferSize::Default,
                };
                let ring_cap_fb =
                    (source_cfg.sample_rate as usize) * (source_cfg.channels as usize) * 2;
                starvation.begin_stream(source_cfg.sample_rate, source_cfg.channels);
                let ring_fb = Arc::new(RingBuf::new_metered(ring_cap_fb, starvation.clone()));
                ring_fb.clear();
                data_started_shared.store(false, Ordering::SeqCst);
                let min_buffer_fb =
                    (source_cfg.sample_rate as usize) * (source_cfg.channels as usize) / 2;
                match build_stream(
                    &source_cfg,
                    ring_fb.clone(),
                    volume.clone(),
                    paused.clone(),
                    silent_cb_outer.clone(),
                    data_started_shared.clone(),
                    min_buffer_fb,
                    soft_mute.clone(),
                ) {
                    Ok(s) => {
                        info!(
                            source_sr = sample_rate,
                            "local_audio_fallback_to_source_rate"
                        );
                        (s, source_cfg, ring_fb)
                    }
                    Err(second_err) => {
                        // Both f32 attempts failed. The hardware likely rejects
                        // float (bit-perfect integer-only DAC). Cascade integer
                        // formats — i32 then i16 — at the chosen rate, then the
                        // source rate. First one the device accepts wins.
                        let mut candidates: Vec<cpal::StreamConfig> = vec![output_config.clone()];
                        if source_cfg.sample_rate != output_config.sample_rate
                            || source_cfg.channels != output_config.channels
                        {
                            candidates.push(source_cfg.clone());
                        }
                        let mut built: Option<(cpal::Stream, cpal::StreamConfig, Arc<RingBuf>)> =
                            None;
                        'int_cascade: for cand in &candidates {
                            let cap = (cand.sample_rate as usize) * (cand.channels as usize) * 2;
                            let min_buf =
                                (cand.sample_rate as usize) * (cand.channels as usize) / 5;
                            for is_i32 in [true, false] {
                                starvation.begin_stream(cand.sample_rate, cand.channels);
                                let r = Arc::new(RingBuf::new_metered(cap, starvation.clone()));
                                r.clear();
                                data_started_shared.store(false, Ordering::SeqCst);
                                let res = if is_i32 {
                                    build_int_stream::<i32>(
                                        &device,
                                        cand,
                                        r.clone(),
                                        volume.clone(),
                                        paused.clone(),
                                        silent_cb_outer.clone(),
                                        data_started_shared.clone(),
                                        min_buf,
                                        device_gone.clone(),
                                        soft_mute.clone(),
                                    )
                                } else {
                                    build_int_stream::<i16>(
                                        &device,
                                        cand,
                                        r.clone(),
                                        volume.clone(),
                                        paused.clone(),
                                        silent_cb_outer.clone(),
                                        data_started_shared.clone(),
                                        min_buf,
                                        device_gone.clone(),
                                        soft_mute.clone(),
                                    )
                                };
                                if let Ok(s) = res {
                                    info!(
                                        format = if is_i32 { "i32" } else { "i16" },
                                        sample_rate = cand.sample_rate,
                                        "local_audio_fallback_to_integer_format"
                                    );
                                    built = Some((s, cand.clone(), r));
                                    break 'int_cascade;
                                }
                            }
                        }
                        match built {
                            Some(t) => t,
                            None => {
                                // Every format was refused, so the fault is
                                // the device itself, not the encoding. Name
                                // the likely cause: the raw ALSA string
                                // ("Host is down (112)" — Yacine) reads as a
                                // network error and sends people hunting in
                                // the wrong place, when it is what the
                                // PipeWire ALSA plugin returns if it cannot
                                // reach the daemon — typically a server
                                // started outside the user session, or a
                                // USB DAC that went away.
                                let cause = classify_open_failure(&first_err.to_string());
                                // #3575 — quand cpal a DÉTRUIT le motif,
                                // aller chercher dans /proc qui tient le
                                // nœud PCM, au lieu d'attendre un
                                // `fuser -v /dev/snd/*` que personne ne
                                // tapera.
                                #[cfg(target_os = "linux")]
                                if cause == OpenFailure::IndisponibleMotifPerdu {
                                    journaliser_les_teneurs_du_pcm(&pcm_ouvert, &device_name);
                                }
                                return Err(RefusDOuverture::ToutesLesTentativesRefusees {
                                    cause,
                                    premiere_erreur: first_err.to_string(),
                                    seconde_erreur: second_err.to_string(),
                                });
                            }
                        }
                    }
                }
            }
        };

        Ok(BackendCpal {
            stream,
            anneau: ring,
            sortie: FormatOuvert::new(actual_config.sample_rate, actual_config.channels),
            device_name,
            stop_rx: demande.stop_rx,
            paused: demande.paused.as_ref(),
            force_silent: demande.force_silent.as_ref(),
            device_gone,
            starvation,
            position_ms: demande.position_ms,
        })
    }

    fn nom(&self) -> &'static str {
        "CPAL"
    }

    fn format_ouvert(&self) -> FormatOuvert {
        self.sortie
    }

    fn puits(&self) -> Puits<'a> {
        Puits::Flottant(Box::new(PuitsAnneauCpal {
            anneau: self.anneau.clone(),
            stop_rx: self.stop_rx,
            paused: self.paused,
            force_silent: self.force_silent,
        }))
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
            // `make_stream_error_cb` journalise les autres erreurs de flux
            // (plafonnées à une par seconde) sans les compter.
            erreurs_de_rappel: None,
        }
    }

    /// Le vidage de fin de piste du chemin partagé, tel qu'il vivait dans
    /// `play_url` : la position publiée recule vers ce qui est réellement joué
    /// (alimenté − encore en attente), et le vidage s'arrête sur stop, silence
    /// forcé, périphérique perdu ou échéance.
    fn drainer(&mut self, borne: std::time::Duration) -> Vidage {
        let device_name = &self.device_name;
        let ring = &self.anneau;
        let stop_rx = self.stop_rx;
        let force_silent = self.force_silent;
        let device_gone = &self.device_gone;
        let position_ms = self.position_ms;
        let output_sr = self.sortie.cadence;
        let output_ch = self.sortie.canaux;

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
            if device_gone.load(Ordering::Relaxed) || drain_started.elapsed() >= drain_deadline {
                // No natural end: the tail was never actually played, and
                // advancing the queue would immediately hit the same dead
                // device.
                warn!(
                    device = %device_name,
                    remaining_samples = remaining,
                    device_gone = device_gone.load(Ordering::Relaxed),
                    "local_audio_drain_timeout"
                );
                break;
            }
            // Report real playback: subtract the still-queued ring content
            // (interleaved f32 samples at the output rate/channels).
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
