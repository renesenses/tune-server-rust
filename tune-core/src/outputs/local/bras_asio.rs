//! Le bras ASIO exclusif de `play_url` (R6 bis puis REF-8, #2219).
//!
//! R6 bis a sorti ce bloc de `play_url` à l'identique. REF-8 le fait passer
//! par le trait `BackendLocal` (#4009) et par les deux puits de D1 :
//!
//! * **route native** (`Native*` : le pilote prend des mots entiers) —
//!   [`super::etage_natif::EtageNatif`] décode vers des mots `i32` alignés à
//!   gauche et écrit dans un [`super::etage_natif::PuitsAnneauNatif`] posé sur
//!   l'anneau natif du backend (`Puits::Natif`). La décision DoP reste dans
//!   le producteur, le DoP est porté tel quel, le volume s'applique dans
//!   l'étage — exactement ce que `feed_windows_native_exclusive_leftover`
//!   faisait ;
//! * **route traitée** (`Processed*` : le pilote n'accepte qu'un mot qui ne
//!   tient pas tous les bits de la source) — l'étage de R1
//!   ([`EtageDeConversion`]) décode vers des `f32` et écrit dans un puits sur
//!   l'anneau flottant (`Puits::Flottant`) ; le volume reste dans le rappel
//!   (D3, en `f64`). Le refus DoP de cette route est CONSERVÉ : la fermeture
//!   passée à `pousser` refuse tout porteur, et le refus est rapporté par
//!   `record_windows_exclusive_pcm_refusal("ASIO", …)` avec le même motif
//!   qu'avant (`DopUnsupported`, `DopCheckIncomplete` à l'EOF).
//!
//! Le backend ([`BackendAsio`]) POSSÈDE ses deux anneaux à travers
//! [`AsioExclusiveOutput`] (D2) et retient l'un des deux selon le transport ;
//! `WindowsExclusiveRingRef` n'a plus d'appelant. Le fil pompe — la lecture
//! HTTP sur un fil séparé, pour que le fil qui TIENT le périphérique ASIO ne
//! bloque jamais sur le réseau — devient une source `Read`
//! ([`SourcePompee`]) ; la boucle de ce bras la lit comme `BoucleProducteur`
//! lirait n'importe quel amont. La boucle reste ici tant que `tourner` n'est
//! pas générique sur l'étage (REF-7c, agent A) : elle est écrite pour être
//! remplacée par un appel.
//!
//! Compilé par la seule étape « ASIO » du job `windows-pr` de `ci.yml` : ni
//! Shrek ni le Mac ne voient ce fichier. `super` désigne ici `outputs::local`,
//! pas `outputs` : le module ASIO se nomme par `crate::outputs::asio_exclusive`
//! (bloquant de R6, #3981). Tout ce qui pouvait être jugé sur Shrek l'est
//! ailleurs : l'étage natif et son puits (`local/etage_natif.rs`), les
//! empreintes des deux routes (`local/empreinte_asio_f70496.rs`).

// ------- Exclusive mode path (Windows ASIO) -------

use std::io::Read;

use super::backend::{
    BackendLocal, DemandeDOuverture, Observation, Puits, RefusDOuverture, Vidage,
};
use super::etage_natif::{EcritureNative, EtageNatif, PuitsAnneauNatif, spec_du_puits_natif};
use super::*;
use super::{EtageDeConversion, LocalPcmKind, LocalPcmProcessor, PousseeVersLePuits};
use crate::outputs::asio_exclusive::AsioExclusiveOutput;

/// Ce que le bras lisait du contexte de `play_url` — trente-quatre valeurs,
/// toutes déjà possédées par le fil de lecture. Elles sont DÉPLACÉES, jamais
/// empruntées : la pompe HTTP du bras (`std::thread::spawn(move …)`) prend
/// `reader` par valeur, ce qu'un emprunt ne permettrait pas (E0521, leçon de
/// #3386). REF-8 ajoute `spec`, le format source typé (R5) que `play_url`
/// avait déjà construit et que le bras redérivait de trois nombres nus.
pub(super) struct EntreesAsio {
    pub(super) device_name: String,
    pub(super) url: String,
    pub(super) sample_rate: u32,
    pub(super) bit_depth: u16,
    pub(super) channels: u16,
    /// Le format source, typé : cadence, profondeur, canaux (R5).
    pub(super) spec: AudioSpec,
    pub(super) data_offset: usize,
    /// Les 4 096 premiers octets lus par `play_url` ; ce qui suit
    /// `data_offset` est le début du PCM.
    pub(super) header_buf: Vec<u8>,
    /// La réponse HTTP, positionnée après `header_buf`. Le bras la confie à
    /// son fil pompe.
    pub(super) reader: reqwest::blocking::Response,
    pub(super) frame_bytes: usize,
    pub(super) bytes_per_sample: usize,
    pub(super) seek_offset: u64,
    pub(super) pre_seeked: bool,
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

// ───────────────────────────────────────────────────────────────────────────
// Le backend : `AsioExclusiveOutput` vu par le trait.
//
// `BackendLocal` est `pub(super)` dans `local::backend` : seul un descendant
// de `local` peut l'implémenter, et `outputs::asio_exclusive` n'en est pas un.
// L'impl vit donc ici, sur une enveloppe qui ajoute à l'objet pilote les
// témoins d'arrêt de `play_url` (la durée de vie `'a` du trait) et le format
// source.
// ───────────────────────────────────────────────────────────────────────────

/// Le backend ASIO exclusif : l'objet pilote — qui possède le flux, les deux
/// anneaux et le verrou de périphérique — et ce que `play_url` lui prête.
struct BackendAsio<'a> {
    sortie: AsioExclusiveOutput,
    spec: AudioSpec,
    device_name: String,
    stop_rx: &'a std::sync::mpsc::Receiver<()>,
    paused: &'a AtomicBool,
    force_silent: &'a AtomicBool,
    position_ms: &'a AtomicU64,
}

impl BackendAsio<'_> {
    /// Le nom du périphérique RÉELLEMENT ouvert (résolution par sous-chaîne,
    /// `"default"` = premier pilote listé).
    fn opened_device_name(&self) -> &str {
        self.sortie.opened_device_name()
    }

    /// Pourquoi ce transport ne peut pas tenir un contrat bit-perfect —
    /// `None` sur la route native. Hors trait : c'est un renseignement ASIO.
    fn bit_perfect_unavailable_reason(&self) -> Option<&'static str> {
        self.sortie.bit_perfect_unavailable_reason()
    }

    /// La route que le transport impose : native (`Native*`) ou traitée.
    fn transport_natif(&self) -> bool {
        self.sortie.uses_native_transport()
    }
}

impl<'a> BackendLocal<'a> for BackendAsio<'a> {
    /// `try_with_asio_device_lock` n'est pas appelé ici : `new` prend le
    /// verrou de périphérique en BLOQUANT, comme avant — une session qui se
    /// démonte encore fait attendre la suivante au lieu de la faire échouer.
    /// Le choix du transport et `build_native_stream` sont dans `new`.
    fn ouvrir(demande: &DemandeDOuverture<'a>) -> Result<Self, RefusDOuverture> {
        let spec = demande.spec;
        let sortie = AsioExclusiveOutput::new(
            demande.device_name,
            spec.cadence(),
            u32::from(spec.profondeur().bits_declares()),
            u32::from(spec.canaux()),
            demande.starvation.clone(),
            demande.volume.clone(),
            demande.paused.clone(),
        )
        .map_err(|erreur| RefusDOuverture::OuvertureExclusiveRefusee {
            backend: "ASIO",
            erreur,
        })?;
        Ok(Self {
            sortie,
            spec,
            device_name: demande.device_name.to_string(),
            stop_rx: demande.stop_rx,
            paused: demande.paused.as_ref(),
            force_silent: demande.force_silent.as_ref(),
            position_ms: demande.position_ms,
        })
    }

    /// Le format négocié par `find_exclusive_config` : la cadence source et
    /// `channels.min(config.channels())`.
    fn format_ouvert(&self) -> FormatOuvert {
        FormatOuvert::new(
            self.sortie.opened_sample_rate(),
            self.sortie.opened_channels(),
        )
    }

    /// D1 : `Natif` sur la route native, `Flottant` sur la route traitée. Le
    /// puits partage l'anneau retenu par `Arc` et n'emprunte pas le backend.
    fn puits(&self) -> Puits<'a> {
        if self.transport_natif() {
            Puits::Natif(Box::new(PuitsAnneauNatif::sur(
                self.sortie.native_ring().clone(),
                spec_du_puits_natif(self.spec),
                self.stop_rx,
                self.paused,
                self.force_silent,
            )))
        } else {
            Puits::Flottant(Box::new(PuitsAnneauFlottantAsio {
                anneau: self.sortie.float_ring().clone(),
                stop_rx: self.stop_rx,
                paused: self.paused,
                force_silent: self.force_silent,
            }))
        }
    }

    /// `stream.play()`. Un échec est rapporté comme un refus d'ouverture
    /// exclusive, avec le texte d'avant (`Failed to start ASIO stream: …`) :
    /// c'est ce que `new` rendait quand `play` y vivait.
    fn demarrer(&mut self) -> Result<(), RefusDOuverture> {
        self.sortie
            .start()
            .map_err(|erreur| RefusDOuverture::OuvertureExclusiveRefusee {
                backend: "ASIO",
                erreur,
            })
    }

    fn observer(&self) -> Observation {
        Observation {
            disponible: self.sortie.available(),
            capacite: self.sortie.capacity(),
            // ASIO n'a pas de rappel d'erreur qui dise « périphérique parti » ;
            // un pilote mort se voit à l'anneau qui ne se vide plus.
            peripherique_perdu: false,
            sous_alimentations_pilote: Some(self.sortie.underrun_count()),
            erreurs_de_rappel: Some(self.sortie.callback_error_count()),
        }
    }

    /// Le vidage doublement borné d'avant, mot pour mot : l'échéance
    /// (`borne`, calculée par l'appelant comme avant — deux fois la contenance
    /// de l'anneau en temps, au moins une seconde) ET un détecteur de
    /// blocage qui abandonne si `available()` n'a pas reculé depuis 1,5 s
    /// (pilote RME figé après une réouverture au point de boucle Repeat —
    /// DEvir bug-22, la régression #789). Les deux sorties journalisent
    /// `asio_drain_timeout`.
    ///
    /// REF-8 ajoute la position réelle : pendant le vidage, la position
    /// publiée recule vers ce qui est réellement joué (alimenté − encore en
    /// attente), comme les backends CPAL et CoreAudio le font.
    fn drainer(&mut self, borne: std::time::Duration) -> Vidage {
        let device_name = &self.device_name;
        let stop_rx = self.stop_rx;
        let force_silent = self.force_silent;
        let position_ms = self.position_ms;
        let sample_rate = self.spec.cadence();
        let channels = self.spec.canaux();

        let fed_position_ms = position_ms.load(Ordering::Relaxed);
        let mut drained_naturally = false;
        let drain_deadline = borne;
        let drain_started = std::time::Instant::now();
        let mut last_avail = self.sortie.available();
        let mut last_progress_at = std::time::Instant::now();
        loop {
            if stop_rx.try_recv().is_ok() {
                break;
            }
            if force_silent.load(Ordering::Relaxed) {
                break;
            }
            let avail = self.sortie.available();
            if avail == 0 {
                drained_naturally = true;
                break;
            }
            if avail < last_avail {
                last_avail = avail;
                last_progress_at = std::time::Instant::now();
            } else if last_progress_at.elapsed() >= std::time::Duration::from_millis(1500) {
                warn!(
                    device = %device_name,
                    ring_available = avail,
                    fed_position_ms,
                    "asio_drain_timeout"
                );
                break;
            }
            if drain_started.elapsed() >= drain_deadline {
                warn!(
                    device = %device_name,
                    ring_available = avail,
                    elapsed_ms = drain_started.elapsed().as_millis() as u64,
                    "asio_drain_timeout"
                );
                break;
            }
            // Report real playback: subtract the still-queued ring content
            // (interleaved words at the source rate/channels).
            if sample_rate > 0 && channels > 0 {
                let ring_ms = (avail as f64 / channels as f64 / sample_rate as f64 * 1000.0) as u64;
                position_ms.store(fed_position_ms.saturating_sub(ring_ms), Ordering::Relaxed);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Vidage {
            vide: drained_naturally,
            restant: self.sortie.available(),
            position_alimentee_ms: fed_position_ms,
        }
    }
}

/// Le puits de la route traitée : l'anneau flottant que draine un rappel
/// `Processed*`, qui y applique le volume en `f64` (D3).
///
/// Jumeau de `PuitsAnneauCpal` (`backend.rs`, privé au module) : mêmes trois
/// témoins d'arrêt, parce que l'attente a lieu ICI quand l'anneau est plein.
struct PuitsAnneauFlottantAsio<'a> {
    anneau: Arc<RingBuf>,
    stop_rx: &'a std::sync::mpsc::Receiver<()>,
    paused: &'a AtomicBool,
    force_silent: &'a AtomicBool,
}

impl PuitsDEchantillons for PuitsAnneauFlottantAsio<'_> {
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

// ───────────────────────────────────────────────────────────────────────────
// La route : un étage et son puits, du mot que le transport impose.
// ───────────────────────────────────────────────────────────────────────────

/// Ce qu'une poussée a produit, sur l'une ou l'autre route — le vocabulaire
/// commun des deux étages, tel que la boucle de ce bras le lit.
enum Poussee {
    /// Rien d'aligné, ou quarantaine 24 bits encore ouverte.
    Rien,
    /// Bloc poussé. Sur la route traitée, `dop` et `bit_perfect` sont faux
    /// par construction : c'est ce que `feed_windows_exclusive_leftover`
    /// rendait.
    Poussee {
        trames: u64,
        dop: bool,
        bit_perfect: bool,
    },
    /// Le puits a cessé de consommer : rappel mort (#3108). Les trames sont
    /// comptées quand même, ce compte est la position.
    PuitsMort { trames: u64 },
    /// Route traitée seulement : porteur DoP refusé avant l'anneau flottant
    /// (`DopUnsupported`).
    PorteurDopRefuse,
}

/// L'étage et le puits d'une route.
enum Route<'a> {
    Native {
        etage: EtageNatif<'a>,
        puits: Box<dyn crate::outputs::traits::PuitsNatif + 'a>,
    },
    Flottante {
        etage: EtageDeConversion<'a>,
        puits: Box<dyn PuitsDEchantillons + 'a>,
    },
}

impl Route<'_> {
    fn est_native(&self) -> bool {
        matches!(self, Route::Native { .. })
    }

    /// Octets reçus et pas encore poussés (la quarantaine 24 bits comprise).
    fn en_attente(&self) -> usize {
        match self {
            Route::Native { etage, .. } => etage.en_attente().len(),
            Route::Flottante { etage, .. } => etage.en_attente.len(),
        }
    }

    /// Le geste élémentaire : ajouter `octets`, décoder ce qui est aligné,
    /// pousser. Sur la route traitée, la fermeture refuse TOUT porteur DoP —
    /// c'est le `DopUnsupported` d'avant, avant toute conversion (#3233).
    fn pousser(&mut self, octets: &[u8]) -> Poussee {
        match self {
            Route::Native { etage, puits } => {
                match etage.decoder_et_pousser(octets, puits.as_mut()) {
                    EcritureNative::RienAPousser => Poussee::Rien,
                    EcritureNative::Poussee {
                        trames_source,
                        dop,
                        bit_perfect,
                    } => Poussee::Poussee {
                        trames: trames_source,
                        dop,
                        bit_perfect,
                    },
                    EcritureNative::PuitsMort { trames_source, .. } => Poussee::PuitsMort {
                        trames: trames_source,
                    },
                }
            }
            Route::Flottante { etage, puits } => {
                etage.en_attente.extend_from_slice(octets);
                match etage.pousser(puits.as_mut(), &mut |dop, _, _| dop, &mut |_| {}) {
                    PousseeVersLePuits::RienAPousser => Poussee::Rien,
                    PousseeVersLePuits::Poussee { trames_source } => Poussee::Poussee {
                        trames: trames_source,
                        dop: false,
                        bit_perfect: false,
                    },
                    PousseeVersLePuits::PuitsMort { trames_source } => Poussee::PuitsMort {
                        trames: trames_source,
                    },
                    PousseeVersLePuits::PorteurDopRefuse => Poussee::PorteurDopRefuse,
                }
            }
        }
    }

    /// Fin de flux. Route native : le reliquat 24 bits jamais classé part
    /// brut (`Some(octets)`, à journaliser). Route traitée : une sonde 24 bits
    /// restée ouverte avec des octets en attente est un refus
    /// (`DopCheckIncomplete`).
    fn vider(&mut self, bit_depth: u16) -> Result<Option<usize>, WindowsExclusivePcmError> {
        match self {
            Route::Native { etage, puits } => Ok(etage.vider(puits.as_mut()).map(|reliquat| {
                info!(
                    backend = "ASIO",
                    bytes = reliquat.octets,
                    bit_perfect = true,
                    "windows_exclusive_short_24bit_stream_forced_raw"
                );
                reliquat.octets
            })),
            Route::Flottante { etage, .. } => finish_windows_exclusive_probe(
                bit_depth,
                etage.pcm_kind.is_awaiting_probe(),
                etage.en_attente.len(),
            )
            .map(|()| None),
        }
    }

    /// La queue du DSP (#2209), au format de la piste qui se termine. Route
    /// native : l'étage vide le convolveur, applique le volume et quantifie
    /// (D3). Route traitée : `flush_local_dsp` puis l'étage de R1, sans
    /// volume — il est dans le rappel.
    fn rendre_la_queue(&mut self, channels: u16, dop: bool) {
        match self {
            Route::Native { etage, puits } => {
                // `EtageNatif::rendre_la_queue` vide le convolveur avec
                // `dop = false` ; `flush_local_dsp(…, dop = true)` ne rendait
                // RIEN — un DoP porté n'a jamais alimenté le convolveur, et un
                // convolveur configuré rend `latency_frames()` de silence
                // même à vide. Ne pas l'appeler sur DoP est l'équivalent exact.
                if !dop {
                    etage.rendre_la_queue(puits.as_mut());
                }
            }
            Route::Flottante { etage, puits } => {
                let queue = flush_local_dsp(
                    etage.pcm.convolver,
                    etage.pcm.crossfeed,
                    etage.pcm.pure_bypass,
                    etage.pcm.mono_downmix,
                    channels,
                    dop,
                );
                if !queue.is_empty() {
                    etage.rendre_la_queue_du_dsp(puits.as_mut(), queue);
                }
            }
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Le fil pompe, vu comme une source `Read`.
// ───────────────────────────────────────────────────────────────────────────

/// La lecture HTTP sur un fil séparé, lue par le fil du périphérique comme
/// n'importe quel amont.
///
/// Pump thread: it owns the blocking HTTP read so the thread that HOLDS THE
/// ASIO DEVICE never blocks on the network. Before this, stop() set
/// force_silent but the device thread sat in reader.read() until the HTTP
/// session died as a side effect of the NEXT play — it then released the
/// ASIO lock ~2.5s INTO the new play. Two repeats survived by timing; the
/// 3rd hit the wrong interleaving: silent output and the poller oscillating
/// at EOF (DEvir, Fireface ASIO, repeat-all, v0.9.0). With the pump, the
/// device thread polls a channel (500ms) and honours stop within one tick;
/// the pump thread may linger in a blocked read but only owns the socket,
/// and exits when the receiver drops or the session closes.
///
/// `read` rend ce que la boucle lisait dans le canal : `Ok(0)` à l'EOF
/// (chunk vide ou pompe partie), `Err(TimedOut)` quand rien n'est venu en
/// 500 ms — la boucle y applique l'EOF par inactivité —, et l'erreur de
/// lecture telle quelle (transitoire ou non : c'est la boucle qui trie, comme
/// avant). Un chunk plus long que le tampon du lecteur est rendu en plusieurs
/// lectures, sans perte.
struct SourcePompee {
    rx: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    /// Approximate depth of the pump→device channel. Incremented by the pump
    /// before each send, decremented here on each successful recv. A high
    /// steady depth means the device thread is NOT draining (ring full /
    /// callback dead); a depth of ~0 means the device is starved (EOF never
    /// latches). Surfaced in the periodic `asio_exclusive_feed_stats` log
    /// (DEvir bug-22).
    profondeur: Arc<std::sync::atomic::AtomicUsize>,
    /// Le chunk en cours de livraison et ce qui en a déjà été rendu.
    en_cours: Vec<u8>,
    rendu: usize,
    /// L'instant de la dernière donnée reçue — l'horloge de l'EOF par
    /// inactivité (5 s).
    derniere_donnee: std::time::Instant,
}

impl SourcePompee {
    /// Démarre le fil pompe sur `reader` ; il s'arrête quand la source rend
    /// EOF, sur une erreur non transitoire, ou quand cette `SourcePompee` est
    /// lâchée (le canal se ferme, `send` échoue).
    fn demarrer(reader: reqwest::blocking::Response) -> Self {
        let profondeur = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (pump_tx, pump_rx) = std::sync::mpsc::sync_channel::<std::io::Result<Vec<u8>>>(64);
        {
            let pump_depth = profondeur.clone();
            std::thread::spawn(move || {
                let mut reader = reader;
                let mut buf = vec![0u8; 65536];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => {
                            pump_depth.fetch_add(1, Ordering::Relaxed);
                            if pump_tx.send(Ok(Vec::new())).is_err() {
                                pump_depth.fetch_sub(1, Ordering::Relaxed);
                            }
                            break;
                        }
                        Ok(n) => {
                            pump_depth.fetch_add(1, Ordering::Relaxed);
                            if pump_tx.send(Ok(buf[..n].to_vec())).is_err() {
                                pump_depth.fetch_sub(1, Ordering::Relaxed);
                                break; // receiver gone — playback stopped
                            }
                        }
                        Err(e) => {
                            let transient = matches!(
                                e.kind(),
                                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                            );
                            pump_depth.fetch_add(1, Ordering::Relaxed);
                            if pump_tx.send(Err(e)).is_err() {
                                pump_depth.fetch_sub(1, Ordering::Relaxed);
                                break;
                            }
                            if !transient {
                                break;
                            }
                        }
                    }
                }
            });
        }
        Self {
            rx: pump_rx,
            profondeur,
            en_cours: Vec::new(),
            rendu: 0,
            derniere_donnee: std::time::Instant::now(),
        }
    }

    /// Profondeur approximative du canal pompe → périphérique.
    fn profondeur(&self) -> usize {
        self.profondeur.load(Ordering::Relaxed)
    }

    /// Depuis combien de temps aucune donnée n'est venue de la pompe.
    fn inactive_depuis(&self) -> std::time::Duration {
        self.derniere_donnee.elapsed()
    }

    /// Copie ce qui reste du chunk en cours dans `buf`.
    fn servir(&mut self, buf: &mut [u8]) -> usize {
        let reste = &self.en_cours[self.rendu..];
        let n = reste.len().min(buf.len());
        buf[..n].copy_from_slice(&reste[..n]);
        self.rendu += n;
        n
    }
}

impl Read for SourcePompee {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.rendu < self.en_cours.len() {
            return Ok(self.servir(buf));
        }
        match self.rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(recu) => {
                self.profondeur.fetch_sub(1, Ordering::Relaxed);
                match recu {
                    Ok(data) if data.is_empty() => Ok(0),
                    Ok(data) => {
                        self.derniere_donnee = std::time::Instant::now();
                        self.en_cours = data;
                        self.rendu = 0;
                        Ok(self.servir(buf))
                    }
                    Err(e) => Err(e),
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "aucune donnée de la pompe depuis 500 ms",
            )),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Ok(0),
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Le bras.
// ───────────────────────────────────────────────────────────────────────────

/// Joue la piste sur un pilote ASIO en mode exclusif, au format source, par
/// le puits natif ou flottant selon le transport du pilote, jusqu'à la fin du
/// flux ou l'ordre d'arrêt. Terminal : quand il rend, le fil de lecture n'a
/// plus rien à faire.
pub(super) fn jouer_via_asio(entrees: EntreesAsio) {
    let EntreesAsio {
        device_name,
        url,
        sample_rate,
        bit_depth,
        channels,
        spec,
        data_offset,
        header_buf,
        reader,
        frame_bytes,
        bytes_per_sample,
        seek_offset,
        pre_seeked,
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

    info!(
        device = %device_name,
        sample_rate,
        bit_depth,
        channels,
        "local_audio_asio_exclusive_mode_active"
    );

    // Ce que la demande d'ouverture exige et qu'ASIO n'a pas : un témoin de
    // périphérique perdu (aucun rappel d'erreur ne le lève, il reste faux) et
    // une porte de rampe anti-« ploc » (le rappel ASIO n'en a pas).
    let device_gone = Arc::new(AtomicBool::new(false));
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
        audio_backend: "asio",
        exclusive: true,
        stop_rx: &stop_rx,
        paused: &paused,
        force_silent: &force_silent,
        volume: &volume,
        device_gone: &device_gone,
        starvation: &starvation,
        soft_mute,
        position_ms: &*position_ms,
    };

    // `ouvrir` : verrou de périphérique, résolution, transport, rappel ;
    // les deux anneaux naissent dans le backend (D2). Un refus est rapporté
    // par `RefusDOuverture::rapporter` → `record_exclusive_open_failure(
    // "ASIO", …)`, mot pour mot ce qu'on écrivait ici.
    let mut backend = match BackendAsio::ouvrir(&demande) {
        Ok(backend) => backend,
        Err(refus) => {
            refus.rapporter(&device_name, &open_failure);
            playing.store(false, Ordering::SeqCst);
            return;
        }
    };
    // Démarré tout de suite après l'ouverture, là où `new` appelait `play` :
    // ASIO ne pré-remplit pas, le rappel rend du silence tant que l'anneau
    // est vide. Attendre un pré-remplissage changerait ce qu'on entend.
    if let Err(refus) = backend.demarrer() {
        refus.rapporter(&device_name, &open_failure);
        drop(backend);
        playing.store(false, Ordering::SeqCst);
        return;
    }

    // D1 : le backend dit quel puits il fournit ; la route suit.
    let mut route = match backend.puits() {
        Puits::Natif(puits) => Route::Native {
            etage: EtageNatif::monter(
                spec,
                backend.format_ouvert(),
                &volume,
                &eq,
                &convolver,
                &crossfeed,
                &pure_bypass,
                &mono_downmix,
            ),
            puits,
        },
        Puits::Flottant(puits) => Route::Flottante {
            // L'étage de R1, monté comme `play_url` le monte — sans
            // rééchantillonnage ni adaptation de canaux : la route traitée
            // d'avant poussait les mots source tels quels dans l'anneau
            // flottant, `sortie` est donc le format SOURCE, pas le format
            // négocié (`channels.min(…)` ne changeait pas l'entrelacement).
            etage: EtageDeConversion {
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
                en_attente: Vec::new(),
                resampler: None,
                resample_leftover: Vec::new(),
                pcm_kind: LocalPcmKind::for_bit_depth(bit_depth),
                spec,
                sortie: FormatOuvert::new(sample_rate, channels),
                needs_resample: false,
            },
            puits,
        },
    };
    let transport_natif = route.est_native();
    if let Some(reason) = backend.bit_perfect_unavailable_reason() {
        info!(
            backend = "ASIO",
            device = %device_name,
            bit_perfect = false,
            reason,
            "windows_exclusive_signal_contract"
        );
    }

    info!(device = %device_name, url = %url, "local_audio_asio_exclusive_playing");
    // ASIO exclusif : résolution par sous-chaîne, et `"default"`
    // prend le premier pilote listé. `opened_id` reste `None` :
    // ASIO n'expose aucun identifiant d'endpoint.
    note_opened_device("ASIO", &device_name, backend.opened_device_name(), None);

    // Feed audio data (no resampling needed -- hardware is set to source rate)
    let pcm_data = if data_offset < header_buf.len() {
        header_buf[data_offset..].to_vec()
    } else {
        Vec::new()
    };

    let mut total_frames_fed: u64 = 0;

    // Only skip bytes if the stream was NOT pre-seeked by the
    // decoder. When pre_seeked=true, the decoder already produced
    // audio starting at the seek position — skipping would discard
    // the entire stream (double-seek bug reported by DEvir).
    let skip_bytes_asio: u64 = if seek_offset > 0 && !pre_seeked {
        let skip_frames = (seek_offset as f64 / 1000.0 * sample_rate as f64) as u64;
        skip_frames * channels as u64 * bytes_per_sample as u64
    } else {
        0
    };
    let mut skipped_bytes_asio: u64 = 0;

    // L'attente d'octets bruts vit dans l'étage : c'est aussi la quarantaine
    // DoP — aucun échantillon 24 bits initial n'atteint un anneau tant que
    // 32 trames n'ont pas tranché.
    let mut bit_perfect_state = None;
    // #3108 — le verdict de blocage du puits (rappel mort, anneau jamais
    // drainé) était JETÉ sur ce chemin ; il est relu et rapporté.
    let mut feed_stalled = false;
    let mut pcm_refusal = None;

    // Track-local contract: never inherit the prior stream's DoP
    // state while the first 24-bit probe is still quarantined.
    if dop_active.swap(false, Ordering::SeqCst) {
        sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, false);
    }

    let amorce: &[u8] = if !pcm_data.is_empty() {
        let discard = if skip_bytes_asio > skipped_bytes_asio {
            ((skip_bytes_asio - skipped_bytes_asio) as usize).min(pcm_data.len())
        } else {
            0
        };
        skipped_bytes_asio += discard as u64;
        &pcm_data[discard..]
    } else {
        &[]
    };

    match route.pousser(amorce) {
        Poussee::Poussee {
            trames,
            dop,
            bit_perfect,
        } => {
            total_frames_fed += trames;
            if dop_active.swap(dop, Ordering::SeqCst) != dop {
                info!(dop, "local_audio_dop_stream_state_changed");
                sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, dop);
            }
            let volume_units = volume.load(Ordering::SeqCst);
            let runtime = publish_windows_signal_path_status(
                &signal_path_status,
                bit_perfect,
                transport_natif,
                dop,
                volume_units,
                &eq,
                &convolver,
                &crossfeed,
                &pure_bypass,
                &mono_downmix,
            );
            bit_perfect_state = Some(runtime.bit_perfect);
            info!(
                backend = "ASIO",
                bit_perfect = runtime.bit_perfect,
                dop,
                volume_units,
                reasons = ?runtime.reasons,
                "windows_exclusive_signal_contract"
            );
        }
        Poussee::PuitsMort { trames } => {
            total_frames_fed += trames;
            feed_stalled = true;
        }
        Poussee::Rien => {}
        Poussee::PorteurDopRefuse => {
            pcm_refusal = Some(WindowsExclusivePcmError::DopUnsupported);
        }
    }
    let quarantaine_ouverte = match &route {
        Route::Native { etage, .. } => etage.quarantaine_24_bits_ouverte(),
        Route::Flottante { etage, .. } => etage.pcm_kind.is_awaiting_probe(),
    };
    if !quarantaine_ouverte && dop_active.swap(false, Ordering::SeqCst) {
        info!(dop = false, "local_audio_dop_stream_state_changed");
        sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, false);
    }

    let mut http_eof_asio = false;
    let mut source = SourcePompee::demarrer(reader);
    let mut tampon_de_lecture = vec![0u8; 65536];
    let mut last_stats_at = std::time::Instant::now();
    while pcm_refusal.is_none() && !feed_stalled {
        if stop_rx.try_recv().is_ok() {
            break;
        }
        if force_silent.load(Ordering::Relaxed) {
            debug!("local_audio_asio_exclusive_aborted_by_stop");
            break;
        }

        // Periodic health snapshot (~500ms) so a wedge is diagnosable
        // from DEvir's next log: ring full + high pump_depth => the
        // callback stopped draining; ring/pump ~empty => starved / EOF
        // never latched (bug-22 / #789).
        if last_stats_at.elapsed() >= std::time::Duration::from_millis(500) {
            let observation = backend.observer();
            debug!(
                ring_available = observation.disponible,
                ring_capacity = observation.capacite,
                total_frames_fed,
                pump_depth = source.profondeur(),
                leftover_bytes = route.en_attente(),
                "asio_exclusive_feed_stats"
            );
            last_stats_at = std::time::Instant::now();
        }

        let n = match source.read(&mut tampon_de_lecture) {
            Ok(0) => {
                http_eof_asio = true;
                break;
            }
            Ok(n) => n,
            Err(ref e)
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                // A streaming HTTP source (transcoded WAV over a
                // keep-alive connection) may never return a clean
                // EOF: after the last byte it just keeps timing out.
                // Once the whole track has been fed AND the ring has
                // fully drained (everything played), a sustained read
                // idle means the track ended — signal EOF so the
                // orchestrator can advance/repeat. Without this, the
                // loop spins forever and end-of-track is never
                // detected on exclusive ASIO outputs (DEvir: repeat
                // never fired on a clean playthrough).
                if total_frames_fed > 0
                    && route.en_attente() == 0
                    && backend.observer().disponible == 0
                    && source.inactive_depuis() > std::time::Duration::from_secs(5)
                {
                    info!("local_audio_asio_exclusive_stream_idle_eof");
                    http_eof_asio = true;
                    break;
                }
                continue;
            }
            Err(e) => {
                warn!(error = %e, "local_audio_asio_exclusive_read_error");
                http_eof_asio = true;
                break;
            }
        };
        let chunk = &tampon_de_lecture[..n];

        let octets: &[u8] = if skip_bytes_asio > 0 && skipped_bytes_asio < skip_bytes_asio {
            let remaining = (skip_bytes_asio - skipped_bytes_asio) as usize;
            if n <= remaining {
                skipped_bytes_asio += n as u64;
                continue;
            }
            skipped_bytes_asio = skip_bytes_asio;
            &chunk[remaining..]
        } else {
            chunk
        };

        match route.pousser(octets) {
            Poussee::Poussee {
                trames,
                dop,
                bit_perfect,
            } => {
                total_frames_fed += trames;
                if dop_active.swap(dop, Ordering::SeqCst) != dop {
                    info!(dop, "local_audio_dop_stream_state_changed");
                    sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, dop);
                }
                let volume_units = volume.load(Ordering::SeqCst);
                let runtime = publish_windows_signal_path_status(
                    &signal_path_status,
                    bit_perfect,
                    transport_natif,
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
                        backend = "ASIO",
                        bit_perfect = runtime.bit_perfect,
                        dop,
                        volume_units,
                        reasons = ?runtime.reasons,
                        "windows_exclusive_signal_contract"
                    );
                }
            }
            Poussee::PuitsMort { trames } => {
                total_frames_fed += trames;
                feed_stalled = true;
                break;
            }
            Poussee::Rien => {}
            Poussee::PorteurDopRefuse => {
                pcm_refusal = Some(WindowsExclusivePcmError::DopUnsupported);
                break;
            }
        }

        let pos = (total_frames_fed as f64 / sample_rate as f64 * 1000.0) as u64 + seek_offset;
        position_ms.store(pos, Ordering::Relaxed);
    }

    if pcm_refusal.is_none() && !feed_stalled && http_eof_asio {
        match route.vider(bit_depth) {
            Ok(Some(aligned)) => {
                total_frames_fed += (aligned / frame_bytes) as u64;
            }
            Ok(None) => {}
            Err(error) => pcm_refusal = Some(error),
        }
    }
    if feed_stalled {
        // La piste n'a PAS fini : `http_eof_asio` reste faux, donc aucune fin
        // naturelle n'est signalée et la file n'avance pas vers un morceau
        // qui heurterait le même pilote mort. Le seul mot dit à l'utilisateur
        // part d'ici (#3108).
        record_feed_stall_failure(
            "ASIO",
            &device_name,
            position_ms.load(Ordering::Relaxed),
            &open_failure,
        );
    }
    if let Some(error) = pcm_refusal {
        record_windows_exclusive_pcm_refusal(error, "ASIO", &device_name, &open_failure);
        force_silent.store(true, Ordering::SeqCst);
        dop_active.store(false, Ordering::SeqCst);
        sync_volume_to_dop(&volume, &user_volume_ref, &rg_factor_ref, false);
        drop(route);
        drop(backend);
        if play_generation.load(Ordering::SeqCst) == my_generation {
            playing.store(false, Ordering::SeqCst);
        }
        return;
    }

    // Fin de piste : rendre ce que le convolveur retient (#2209).
    if !feed_stalled {
        route.rendre_la_queue(channels, dop_active.load(Ordering::Relaxed));
    }

    // Signal natural track end BEFORE draining when the HTTP
    // stream reached EOF, so the orchestrator can detect
    // end-of-track even if force_silent is set during slow drain.
    if http_eof_asio {
        track_ended_naturally.store(true, Ordering::SeqCst);
        track_ended_generation.store(my_generation, Ordering::SeqCst);
        TRACK_END_NOTIFY.notify_one();
    }

    // Wait for the ring to drain — but NEVER block forever. If the
    // ASIO render callback stops consuming (RME driver wedged after a
    // stop→start reopen at a Repeat loop point — DEvir bug-22, the
    // #789 regression), `available()` never reaches 0 and this used to
    // spin indefinitely, stranding this thread AND the process-wide
    // ASIO_DEVICE_LOCK (held until the backend is dropped just below).
    // `drainer` bounds it two ways: a hard wall-clock deadline of ~2× the
    // ring's time-capacity (computed here, as before), and a stall detector
    // that bails if `available()` has not decreased for ~1.5s.
    let ring_capacity_ms = if sample_rate > 0 && channels > 0 {
        (backend.observer().capacite as u64 * 1000) / (sample_rate as u64 * channels as u64)
    } else {
        0
    };
    let drain_deadline = std::time::Duration::from_millis((ring_capacity_ms * 2).max(1000));
    let vidage = backend.drainer(drain_deadline);
    if http_eof_asio && vidage.vide {
        // Vidé de lui-même : la position revient sur ce qui a été alimenté,
        // comme le chemin CPAL partagé le fait après son vidage.
        position_ms.store(vidage.position_alimentee_ms, Ordering::Relaxed);
    }

    // Dropping the backend releases the ASIO device and, with it, the
    // process-wide ASIO_DEVICE_LOCK (`AsioExclusiveOutput::drop`). Both loops
    // above are bounded and any panic unwinds through this owned local, so
    // this drop runs on EVERY exit path — the lock can never be stranded.
    drop(route);
    drop(backend);
    if play_generation.load(Ordering::SeqCst) == my_generation {
        playing.store(false, Ordering::SeqCst);
    }
    info!(
        device = %device_name,
        frames = total_frames_fed,
        "local_audio_asio_exclusive_stopped"
    );
}
