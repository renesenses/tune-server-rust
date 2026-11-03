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

use super::backend::{Observation, RefusDOuverture, Vidage};
use super::chaine_native::{
    FinDeChaine, HoteDeChaineNative, ReserveDeLaChaine, ReserveHttp, Suivante,
    jouer_la_chaine_native,
};
use super::etage_natif::{EcritureNative, EtageNatif, PuitsAnneauNatif, spec_du_puits_natif_vers};
use super::*;
use crate::outputs::wasapi_exclusive::WasapiExclusiveOutput;

/// #7264 — le volume que reçoit l'étage : l'unité. Le volume logiciel est
/// appliqué par le fil de rendu, après l'anneau (`pop_pcm_bytes_au_gain`) ;
/// appliqué aussi dans l'étage, il le serait deux fois.
const VOLUME_A_L_UNITE: u32 = 1000;

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
    pub(super) transformations_reelles: Arc<std::sync::Mutex<Option<TransformationsReelles>>>,
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
    pub(super) reader: super::LecteurHttpAnnulable,
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
    /// #6247 — la réaffectation des canaux de la zone (#6044), que ce bras
    /// ne recevait pas : elle n'était jamais appliquée en WASAPI exclusif.
    pub(super) reaffectation: Arc<super::CreneauReaffectation>,
    /// #6247 — la disposition déclarée par le fichier (#6057).
    pub(super) disposition: Arc<super::CreneauDisposition>,
    pub(super) dop_active: Arc<AtomicBool>,
    // ── #5204 : l'enchaînement sans blanc ──────────────────────────────
    /// La réserve de `set_next_media`, que ce bras CONSOMME désormais à l'EOF.
    pub(super) next_media: Arc<std::sync::Mutex<Option<PendingNextMedia>>>,
    /// La sonde vivante de `supports_internal_gapless()` : levée quand la
    /// chaîne ne peut plus rien enchaîner.
    pub(super) chain_exhausted: Arc<AtomicBool>,
    /// Ce que la sortie publie du morceau en cours, basculé à l'enchaînement.
    pub(super) current_uri: Arc<std::sync::Mutex<Option<String>>>,
    pub(super) track_title: Arc<std::sync::Mutex<Option<String>>>,
    pub(super) track_artist: Arc<std::sync::Mutex<Option<String>>>,
    pub(super) duration_ms: Arc<AtomicU64>,
    pub(super) seek_offset_ms: Arc<AtomicU64>,
}

/// WASAPI en mode exclusif événementiel : le backend de tous les DAC Windows
/// réglés « exclusif » sans ASIO.
///
/// Il possède l'anneau entier (D2) — `cadence × canaux × 2`, deux secondes,
/// comme les autres bras — et l'objet pilote. Le fil de rendu de
/// `WasapiExclusiveOutput` tient l'autre bout de l'anneau par `Arc` et ne
/// faisait que sérialiser des mots déjà résolus (volume et DSP dans le
/// producteur). #7264 : le volume pondéré dans le producteur attendait les
/// deux secondes de l'anneau avant de s'entendre ; il est désormais appliqué
/// par le fil de rendu, après l'anneau — le DSP reste dans le producteur.
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

    /// #7264 — l'anneau que le rendu lit, pour que l'étage y marque ses
    /// passages intacts.
    fn anneau(&self) -> Arc<NativePcmRing> {
        self.anneau.clone()
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
        // WASAPI ouvre à la cadence source, sans repli de cadence.
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

        // #6230 (étape 2) — une grille qui pilote les voies (6 → 2) est
        // demandée d'abord, plafonnée aux voies que l'endpoint déclare
        // (`new_sur_voies_pilotees`). Un refus rouvre une fois au format de
        // la source : c'est l'ouverture d'avant, et le chemin du signal dit
        // pourquoi la règle ne s'applique pas (#6595).
        let anneau_pour = |voies: u16| {
            let ring_cap = (sample_rate as usize) * (voies as usize) * 2;
            demande.starvation.begin_stream(sample_rate, voies);
            let ring = Arc::new(NativePcmRing::new_metered(
                ring_cap,
                demande.starvation.clone(),
            ));
            ring.clear();
            ring
        };
        let pilotee = demande
            .voies_de_la_matrice
            .filter(|&voies| voies != demande.spec.canaux())
            .and_then(|voies| {
                let ring = anneau_pour(voies);
                match WasapiExclusiveOutput::new_sur_voies_pilotees(
                    demande.device_name,
                    demande.endpoint_id,
                    sample_rate,
                    bit_depth as u32,
                    u32::from(voies),
                    ring.clone(),
                    demande.paused.clone(),
                ) {
                    Ok(wasapi) => Some((wasapi, ring)),
                    Err(erreur) => {
                        info!(
                            device = %demande.device_name,
                            voies,
                            canaux_source = demande.spec.canaux(),
                            erreur = %erreur,
                            "wasapi_exclusive_voies_de_la_grille_refusees_reouverture_au_format_source"
                        );
                        None
                    }
                }
            });
        let ouverture = match pilotee {
            Some((wasapi, ring)) => Ok((wasapi, ring)),
            None => {
                let channels = demande.spec.canaux();
                let ring = anneau_pour(channels);
                WasapiExclusiveOutput::new(
                    demande.device_name,
                    demande.endpoint_id,
                    sample_rate,
                    bit_depth as u32,
                    u32::from(channels),
                    ring.clone(),
                    demande.paused.clone(),
                )
                .map(|wasapi| (wasapi, ring))
            }
        };

        match ouverture {
            Ok((mut wasapi, ring)) => {
                // #7264 — le fil de rendu pondère par le volume de la zone,
                // relu à chaque période.
                wasapi.regler_le_volume_au_rendu(demande.volume.clone());
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

    /// Le format que `Initialize(EXCLUSIVE)` a accepté : la cadence de la
    /// source, et ses canaux — ou les voies de la grille quand elle les
    /// pilote (#6230).
    fn format_ouvert(&self) -> FormatOuvert {
        self.sortie
    }

    /// Le puits d'anneau natif : des blocs `Entier32` (mots alignés à gauche),
    /// poussés en attendant que le fil de rendu draine.
    fn puits(&self) -> Puits<'a> {
        // #6230 — le puits porte les voies OUVERTES : l'étage natif y livre
        // des blocs à M voies quand la grille pilote, à N sinon.
        Puits::Natif(Box::new(PuitsAnneauNatif::sur(
            self.anneau.clone(),
            spec_du_puits_natif_vers(self.spec, self.sortie.canaux),
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

    /// Le vidage du bras, BORNÉ depuis #6842 : il s'arrête sur stop, silence
    /// forcé, anneau vide, sur la borne de `drain_deadline_for`, ou après
    /// 1,5 s sans qu'un mot ne parte — la règle du bras ASIO. Avant, la borne
    /// était ignorée (défaut conservé par REF-8) : face à un rappel de rendu
    /// mort, le vidage ne finissait JAMAIS (#3108, carte §1.2). Le temps passé
    /// en pause ne compte pas. La position publiée ne recule pas : le bras ne
    /// le faisait pas, et ce n'est pas l'objet de #6842.
    fn drainer(&mut self, borne: std::time::Duration) -> Vidage {
        // Import local : l'en-tête du module reste celui d'avant, pour ne pas
        // croiser #6230 (étape 2) sur les mêmes lignes.
        use super::chaine_native::{FinDuVidage, attendre_le_vidage};
        let fed_position_ms = self.position_ms.load(Ordering::Relaxed);
        let anneau = &self.anneau;
        let stop_rx = self.stop_rx;
        let force_silent = self.force_silent;
        let paused = self.paused;
        let fin = attendre_le_vidage(
            || anneau.available(),
            || stop_rx.try_recv().is_ok() || force_silent.load(Ordering::Relaxed),
            || paused.load(Ordering::Relaxed),
            borne,
            std::time::Duration::from_millis(1500),
            std::time::Duration::from_millis(50),
        );
        if let FinDuVidage::BorneAtteinte { restant } | FinDuVidage::SansProgres { restant } = fin {
            warn!(
                device = %self.wasapi.opened_device_name(),
                ring_available = restant,
                fed_position_ms,
                fin = ?fin,
                "wasapi_drain_timeout"
            );
        }
        Vidage {
            vide: fin == FinDuVidage::Vide,
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

/// #5204 — ce que la chaîne native emprunte au bras WASAPI : ses témoins
/// d'arrêt, sa publication (verdict, position, morceau), et la réserve de
/// `set_next_media`, que le bras consomme enfin à l'EOF.
struct HoteWasapi<'a, F: FnMut(bool, bool)> {
    stop_rx: &'a std::sync::mpsc::Receiver<()>,
    position_ms: &'a AtomicU64,
    publier: &'a mut F,
    /// La réserve de la suivante et sa publication, partagées avec le bras
    /// ASIO (`chaine_native::ReserveHttp`).
    reserve: ReserveHttp<'a>,
}

impl<F: FnMut(bool, bool)> ReserveDeLaChaine for HoteWasapi<'_, F> {
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

impl<F: FnMut(bool, bool)> HoteDeChaineNative for HoteWasapi<'_, F> {
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
        transformations_reelles,
        device_name,
        endpoint_id,
        audio_backend,
        spec,
        soft_mute,
        data_offset,
        header_buf,
        reader,
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
    // #6230 (étape 2) — les voies que la grille pilote pour cette piste, par
    // la MÊME décision que la frontière gapless de l'étage natif
    // (`voies_a_ouvrir_en_exclusif`) : jamais en PURE ni sur un porteur DoP,
    // jamais sur une amorce 24 bits trop courte pour écarter le DoP.
    let voies_de_la_matrice = Some(reaffectation.voies_a_ouvrir_en_exclusif(
        spec,
        header_buf.get(data_offset..).unwrap_or_default(),
        pure_bypass.load(Ordering::Relaxed),
        false,
    ))
    .filter(|&voies| voies != spec.canaux());
    let demande = DemandeDOuverture {
        spec,
        voies_de_la_matrice,
        device_name: &device_name,
        endpoint_id: endpoint_id.as_deref(),
        origin_host: None,
        audio_backend: &audio_backend,
        exclusive: true,
        // #3973 — non lu par ce bras : un transport exclusif ouvre à la
        // cadence de la source ou refuse, il ne rééchantillonne pas.
        strict_bitperfect: false,
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
            // #4176 — seul le flux COURANT a le droit d'écrire l'échec dans le
            // créneau partagé et d'éteindre `playing` : un fil périmé qui perd
            // la course au périphérique (`0x8889000A`) ne doit pas faire
            // arrêter la zone que son successeur joue déjà.
            if play_generation.load(Ordering::SeqCst) == my_generation {
                refus.rapporter(&device_name, &open_failure);
                playing.store(false, Ordering::SeqCst);
            } else {
                warn!(
                    device = %device_name,
                    generation = my_generation,
                    "local_audio_stale_exclusive_open_failure_ignored"
                );
            }
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
    let mut bit_perfect_state = None;

    // #6230 — ce que l'ouverture a fait des voies demandées : noté pour les
    // décisions de réouverture, et dit au journal.
    reaffectation.constater_l_ouverture(
        voies_de_la_matrice,
        backend.format_ouvert().canaux,
        &device_name,
    );

    // #7264 — l'étage à l'UNITÉ : le volume est appliqué au rendu, après
    // l'anneau de deux secondes. L'étage marque ses passages intacts (DoP)
    // dans l'anneau pour que le rendu ne les pondère jamais.
    let volume_de_l_etage = AtomicU32::new(VOLUME_A_L_UNITE);
    let mut etage = EtageNatif::monter(
        spec,
        backend.format_ouvert(),
        &volume_de_l_etage,
        &eq,
        &convolver,
        &crossfeed,
        &pure_bypass,
        &mono_downmix,
    )
    .avec_l_adaptation_des_canaux(&reaffectation, &disposition)
    .avec_publication(&transformations_reelles)
    .avec_le_gain_au_rendu(backend.anneau());
    let mut puits = match backend.puits() {
        Puits::Natif(puits) => puits,
        Puits::Flottant(_) => unreachable!("BackendWasapi ne fournit qu'un puits natif"),
    };

    // Ce que le bras faisait après chaque poussée, en deux copies (amorce,
    // boucle) : l'état DoP de la zone, le volume qui le suit, le verdict du
    // chemin de signal, et sa journalisation au changement.
    let mut publier_le_verdict = |dop: bool, etage_intact: bool| {
        if dop_active.swap(dop, Ordering::SeqCst) != dop {
            info!(dop, "local_audio_dop_stream_state_changed");
            sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, dop);
        }
        let volume_units = volume.load(Ordering::SeqCst);
        // #7264 — l'étage, à l'unité, ne voit plus le volume : le mot reste
        // intact au DAC si l'étage l'a laissé intact ET que le rendu ne le
        // pondère pas (DoP, ou volume à 100 %). Même règle que le natif S32.
        let bit_perfect = etage_intact && (dop || volume_units == VOLUME_A_L_UNITE);
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

    // #6842 — un puits mort (`PuitsMort`) n'est plus traité comme une
    // poussée : c'était la « famine muette » de la carte §1.2, conservée par
    // REF-8. Dès l'amorce, il arrête la lecture et la famine est rapportée.
    let mut amorce_morte = false;
    match etage.decoder_et_pousser(&pcm_data, &mut *puits) {
        EcritureNative::Poussee {
            trames_source,
            dop,
            bit_perfect,
        } => {
            total_frames_fed += trames_source;
            publier_le_verdict(dop, bit_perfect);
        }
        EcritureNative::PuitsMort {
            trames_source,
            dop,
            bit_perfect,
        } => {
            total_frames_fed += trames_source;
            publier_le_verdict(dop, bit_perfect);
            amorce_morte = true;
        }
        EcritureNative::RienAPousser => {}
    }
    if !etage.quarantaine_24_bits_ouverte() && dop_active.swap(false, Ordering::SeqCst) {
        info!(dop = false, "local_audio_dop_stream_state_changed");
        sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, false);
    }

    // #5204 — la boucle de lecture ET l'enchaînement : à l'EOF, la piste
    // préparée par `set_next_media` entre dans le MÊME anneau si elle a le
    // même format — le flux WASAPI reste ouvert, aucun blanc. À format
    // différent, la chaîne rend la main : la piste se termine et la fin
    // naturelle rouvre le périphérique au nouveau format (0.9.165).
    let mut hote = HoteWasapi {
        stop_rx: &stop_rx,
        position_ms: &position_ms,
        publier: &mut publier_le_verdict,
        reserve: ReserveHttp {
            force_silent: &force_silent,
            position_ms: &position_ms,
            next_media: &next_media,
            en_cours: None,
            current_uri: &current_uri,
            track_title: &track_title,
            track_artist: &track_artist,
            duration_ms: &duration_ms,
            seek_offset_ms: &seek_offset_ms,
            track_ended_naturally: &track_ended_naturally,
            track_ended_generation: &track_ended_generation,
            dop_active: &dop_active,
            volume: &volume,
            user_volume: &user_volume_ref,
            rg_factor: &rg_factor_ref,
        },
    };
    let issue = if amorce_morte {
        drop(reader);
        super::chaine_native::IssueDeLaChaine {
            fin: FinDeChaine::PuitsMort,
            http_eof: false,
            trames: total_frames_fed,
            pistes_enchainees: 0,
        }
    } else {
        jouer_la_chaine_native(
            &mut hote,
            reader,
            &mut etage,
            &mut *puits,
            total_frames_fed,
            seek_offset,
        )
    };
    let total_frames_fed = issue.trames;
    let http_eof_wasapi = issue.http_eof;
    if issue.pistes_enchainees > 0 || issue.fin == FinDeChaine::FormatDifferent {
        info!(
            device = %device_name,
            pistes_enchainees = issue.pistes_enchainees,
            fin = ?issue.fin,
            "local_audio_wasapi_exclusive_chain_ended"
        );
    }

    // La chaîne est finie : ce fil n'enchaînera plus rien. Le DIRE au sondeur,
    // qui relit la capacité pendant qu'il attend (#1919, même règle que le
    // chemin partagé) — sauf si une lecture plus récente nous a supplantés :
    // la sonde appartient alors déjà au fil suivant.
    if doit_declarer_chaine_epuisee(
        force_silent.load(Ordering::Relaxed),
        play_generation.load(Ordering::SeqCst),
        my_generation,
    ) {
        chain_exhausted.store(true, Ordering::SeqCst);
        debug!("local_audio_gapless_chain_exhausted");
    }

    // WASAPI exclusive now follows the same DSP tail
    // contract as the other local PCM paths (#2209).
    // #6842 — la queue n'est pas rendue à un puits mort, et un puits qui
    // meurt sur la queue est une famine comme une autre.
    let puits_mort = issue.fin == FinDeChaine::PuitsMort || !etage.rendre_la_queue(&mut *puits);
    if puits_mort {
        // La famine est DITE, au nom du backend, comme sur les autres bras
        // (#3108) : le sondeur la relève par `take_output_failure()`.
        record_feed_stall_failure(
            "WASAPI",
            &device_name,
            position_ms.load(Ordering::Relaxed),
            &open_failure,
        );
    }

    // Signal natural track end BEFORE draining when
    // the HTTP stream reached EOF, so the orchestrator
    // can detect end-of-track even if force_silent is
    // set during slow drain (e.g. 44.1→192 kHz resample).
    if http_eof_wasapi {
        track_ended_naturally.store(true, Ordering::SeqCst);
        track_ended_generation.store(my_generation, Ordering::SeqCst);
        TRACK_END_NOTIFY.notify_one();
    }

    // La borne est celle que le chemin partagé calcule ; `drainer` la
    // respecte depuis #6842. Un puits mort ne se vide pas : on n'attend pas.
    if !puits_mort {
        let borne = drain_deadline_for(
            backend.observer().disponible,
            u64::from(sample_rate),
            u64::from(channels),
        );
        let _ = backend.drainer(borne);
    }

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
