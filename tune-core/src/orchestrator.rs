use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use tokio::sync::Mutex;
use tracing::{debug, info, warn};

// Repli NFC/NFD partagé (#1865) — voir `crate::library::local_path`.
use crate::library::local_path::resolve_existing_local_path;

mod transcodage;
use transcodage::*;

/// A second play of the SAME track pushed to a NETWORK renderer within this
/// window is treated as a redundant re-trigger (a slow pre-transcode resolve
/// racing a poller/gapless advance) and coalesced instead of re-sent — a second
/// `SetAVTransportURI` restarts a push renderer from 0 (Revox S100 double-play,
/// forum). A legitimate re-play of the same track (repeat-one, the same track
/// twice in a queue) only recurs after the track has played, i.e. minutes
/// later, well outside this window; explicit seeks and stop→replay are exempt.
const DUPLICATE_NET_PLAY_WINDOW: std::time::Duration = std::time::Duration::from_secs(12);

mod radio;
pub(crate) use radio::*;

/// re-resolve or re-send. The `superseded` play_seq guard in `play_inner` only
/// catches an OVERLAPPING second play; when the second play arrives just AFTER
/// the first fully established playback (sequential, a few seconds apart), that
/// guard sees no overlap and lets it through, and the second `SetAVTransportURI`
/// restarts a network renderer from byte 0 (Revox S100 "plays ~10s then jumps to
/// 0" — #1271). Kept short so a deliberate replay of the same track — which
/// lands far later — plays normally; a seek is exempt regardless.
const RETAP_DEDUP_WINDOW: std::time::Duration = std::time::Duration::from_secs(8);

use crate::audio::formats::AudioFormat;
use crate::db::history_repo::{HistoryRepo, ListenRecord};
use crate::db::play_queue_repo::PlayQueueRepo;
use crate::db::settings_repo::SettingsRepo;
use crate::db::track_repo::TrackRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::event_bus::EventBus;
use crate::http::streamer::{AudioStreamer, StreamInfo};
use crate::outputs::registry::OutputRegistry;
use crate::outputs::{OutputCommand, OutputCommandError, OutputCommandResult};
use crate::playback::{NowPlaying, PlayState, PlaybackManager};
use crate::prefetch::PrefetchEngine;
use crate::streaming::registry::ServiceRegistry;

/// Ce que l'auditeur avait demandé, et où il en était — les trois champs que
/// « Continuer l'écoute » a besoin de retrouver pour ROUVRIR cet objet à la
/// bonne place (#2441, FabienM fil 1557).
///
/// Regroupés plutôt qu'ajoutés un à un aux douze paramètres de `record_listen`
/// : ils ne se lisent qu'ensemble, et le rang seul ne veut rien dire sans
/// l'objet auquel il se rapporte.
///
/// C'est délibérément « objet courant + position », PAS l'instantané de la
/// file : pour un artiste ou un label, la file est bâtie par une requête qui
/// change d'un jour à l'autre — « conserver l'ordre » n'y aurait aucun sens —
/// et écrire la file entière à chaque écoute coûterait un facteur dix sur le
/// volume de `listen_history`, pour alimenter une section d'accueil.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContexteEcoute<'a> {
    /// `track`, `album`, `playlist`, `artist` ou `label`. `None` quand
    /// l'appelant n'a rien déclaré — une absence, jamais une déduction.
    pub nature: Option<&'a str>,
    /// L'identifiant de cet objet, dans le référentiel de sa source.
    pub id: Option<&'a str>,
    /// Le rang de la piste dans cet objet. `None` en lecture ALÉATOIRE : voir
    /// `rang_a_retenir`.
    pub rang: Option<i64>,
}

mod regles;
pub use regles::*;

/// Pas d'attente pendant les phases pause / pré-démarrage du forwarder de
/// niveaux : assez court pour réagir vite, assez long pour ne pas marteler
/// le mutex des zones.
/// Taille des blocs du décodage-pour-niveaux (passthrough). Le PCM produit
/// n'est lu par personne — seul compte le fait de borner la mémoire — mais un
/// bloc trop petit multiplierait les allers-retours de canal pour rien.
const LEVELS_DECODE_CHUNK: usize = 64 * 1024;

const LEVELS_HOLD: std::time::Duration = std::time::Duration::from_millis(200);

/// Publie les niveaux audio (`playback.audio_levels`) sur le bus, cadencés
/// sur l'horloge de lecture via la fenêtre temporelle portée par chaque
/// [`crate::audio::levels::AudioLevels`].
///
/// Les décodeurs produisent les niveaux à la vitesse du décodage — bien plus
/// vite que le temps réel — et sans cadencement les clients recevaient la
/// piste entière en rafale au début de la lecture. La tâche se fige quand la
/// zone est en pause, et s'arrête quand la lecture est stoppée ou remplacée
/// (le `play_seq` capturé ne correspond plus) : sans cela, deux pistes
/// successives émettraient en parallèle pendant toute leur durée.
///
/// Chaque événement est estampillé de la position de piste qu'il décrit
/// (`position_ms`, début de la fenêtre analysée) : les clients peuvent
/// s'aligner sur la position rapportée par le renderer, ce qui compense le
/// tampon de sortie (plusieurs secondes sur un renderer DLNA/OpenHome).
/// `start_position_ms` est le point de départ du décodage (0, ou le seek).
fn spawn_paced_levels_forwarder(
    bus: Arc<EventBus>,
    playback: Arc<PlaybackManager>,
    zone_id: i64,
    play_seq: u64,
    start_position_ms: i64,
) -> tokio::sync::mpsc::UnboundedSender<crate::audio::tap::RawWindow> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<crate::audio::tap::RawWindow>();
    tokio::spawn(async move {
        // Le forwarder est le métronome du signal : il reçoit les fenêtres
        // brutes à la vitesse du décodage, les recadence sur l'horloge de
        // lecture, publie chaque fenêtre estampillée sur le tap PCM de la
        // zone (la primitive des plugins d'analyse — voir audio::tap), puis
        // calcule et émet `playback.audio_levels`. Le tap transporte donc du
        // temps réel : un consommateur naïf (une FFT par fenêtre) suit sans
        // retamponner, le ring borné n'absorbe que la gigue.
        let tap = playback.zone_tap(zone_id);
        // Génération capturée au spawn : l'avance gapless la bumpe pour tuer
        // le forwarder de la piste précédente à l'instant de la transition —
        // sans attendre qu'il draine sa file (ses stamps décriraient une piste
        // que le renderer ne joue plus).
        let gen_arc = playback.levels_gen(zone_id);
        let gen_at_spawn = gen_arc.load(std::sync::atomic::Ordering::Relaxed);
        let mut next_emit = tokio::time::Instant::now();
        let mut started = false;
        let mut position = std::time::Duration::from_millis(start_position_ms.max(0) as u64);
        // Un pré-transcode DASH peut retarder le démarrage de 30 s ; au-delà
        // de cette borne on abandonne pour ne pas fuiter la tâche.
        let startup_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
        // Le rattrapage ne s'arme qu'après avoir vu la position rapportée
        // PROGRESSER pendant la vie de ce forwarder : au changement de piste,
        // le poller rapporte encore brièvement la position de l'ancienne
        // piste — s'y fier faisait déverser toutes les fenêtres d'un coup
        // (inondation du bus) puis mourir le forwarder, plus aucun niveau
        // pour le reste de la piste.
        let mut last_reported: Option<i64> = None;
        let mut reported_advancing = false;
        // Crête tenue ~300 ms (#1694) : un transitoire survit à une trame
        // perdue. Un état par forwarder = remise à zéro au changement de
        // piste, gratuite par construction.
        let mut peak_hold = crate::audio::levels::PeakHold::default();
        while let Some(raw) = rx.recv().await {
            // La boucle RAPPORTE la position lue au moment où la zone est
            // effectivement en lecture : c'est cette valeur-là, et aucune autre,
            // qui sert au rattrapage plus bas. Un `0` initial n'était jamais lu
            // (toute sortie de boucle passe par une affectation), et le déclarer
            // laissait croire à un repli qui n'existe pas.
            let reported_position_ms: i64 = loop {
                if playback.current_play_seq(zone_id).await != play_seq
                    || gen_arc.load(std::sync::atomic::Ordering::Relaxed) != gen_at_spawn
                {
                    return;
                }
                let zone_state = playback.get_state(zone_id).await;
                let reported_position_ms = zone_state.position_ms;
                if let Some(prev) = last_reported {
                    if reported_position_ms > prev {
                        reported_advancing = true;
                    }
                }
                last_reported = Some(reported_position_ms);
                match zone_state.state {
                    PlayState::Playing => {
                        started = true;
                        break reported_position_ms;
                    }
                    PlayState::Stopped if started => return,
                    // Pause, ou lecture pas encore démarrée (résolution /
                    // transcode en cours) : on gèle l'horloge d'émission.
                    _ => {
                        if !started && tokio::time::Instant::now() > startup_deadline {
                            return;
                        }
                        tokio::time::sleep(LEVELS_HOLD).await;
                        next_emit += LEVELS_HOLD;
                    }
                }
            };
            // Une première fenêtre tardive (téléchargement, transcode) ne doit
            // pas déclencher un rattrapage en rafale de tout le retard
            // accumulé : un instrument vit au présent. On recale l'horloge.
            let now = tokio::time::Instant::now();
            if next_emit < now {
                next_emit = now;
            }
            // Rattrapage borné sur la position du renderer : si le flux émis
            // est en retard sur le son (démarrage tardif du décodage — mesuré
            // ~5 s de staging sur un 24/192 en passthrough), on émet sans
            // attendre jusqu'à recoller à ~1 s. Borné par construction : le
            // retard vaut quelques secondes de fenêtres, pas la piste entière.
            let lagging =
                reported_advancing && (position.as_millis() as i64) < reported_position_ms - 1_000;
            if lagging {
                // Fenêtre du passé audible : personne n'en veut — ni le tap
                // ni les clients. On la saute sans l'émettre (l'émettre en
                // rafale inondait le bus : « broadcast lagged, skipped
                // 2000 messages ») et on garde l'horloge au présent.
                if (position.as_millis() as i64) + 2_000 < reported_position_ms {
                    position += raw.window;
                    next_emit = now;
                    continue;
                }
                next_emit = now;
            } else {
                tokio::time::sleep_until(next_emit).await;
            }

            let window = raw.window;
            // Un seul Arc porte les échantillons : le tap le clone en O(1)
            // pour chaque abonné, compute_levels le lit sans copie.
            let pcm: std::sync::Arc<[u8]> = std::sync::Arc::from(raw.pcm.into_boxed_slice());
            tap.publish(crate::audio::tap::PcmTapFrame {
                zone_id,
                pcm: pcm.clone(),
                format: crate::audio::tap::PcmFormat {
                    sample_rate: raw.sample_rate,
                    channels: raw.channels,
                    bit_depth: raw.bit_depth,
                    // Tous les chemins de décodage produisent de l'entier
                    // signé little-endian (voir decode.rs) ; un futur chemin
                    // f32 devra le déclarer ici.
                    sample_format: crate::audio::tap::SampleFormat::SignedInt,
                },
                track_position: position,
                window,
                play_seq,
            });

            let lvl = crate::audio::levels::compute_levels(
                &pcm,
                raw.bit_depth,
                raw.channels,
                raw.sample_rate,
            );
            let (peak_hold_left_db, peak_hold_right_db) =
                peak_hold.update(lvl.window, lvl.peak_left, lvl.peak_right);
            bus.emit(
                "playback.audio_levels",
                serde_json::json!({
                    "zone_id": zone_id,
                    // Début de la fenêtre analysée, dans le référentiel de la
                    // piste — les clients s'alignent sur la position rapportée
                    // par le renderer pour compenser son tampon de sortie.
                    "position_ms": position.as_millis() as i64,
                    "rms_left_db": lvl.rms_left_db(),
                    "rms_right_db": lvl.rms_right_db(),
                    "peak_left_db": lvl.peak_left_db(),
                    "peak_right_db": lvl.peak_right_db(),
                    // Crête TENUE (max glissant ~300 ms) — champ ADDITIF
                    // (#1694) : un client ancien l'ignore, un client neuf y
                    // lit le transitoire même s'il a raté la trame qui le
                    // portait. Sample peak, avant DSP, comme `peak_*_db`.
                    "peak_hold_left_db": peak_hold_left_db,
                    "peak_hold_right_db": peak_hold_right_db,
                    "rms_left": lvl.rms_left,
                    "rms_right": lvl.rms_right,
                    "spectrum": lvl.spectrum,
                    // Niveau absolu par bande, en dBFS. `spectrum` reste une
                    // forme normalisée trame par trame (contrat des clients
                    // déjà déployés) ; ce champ dit le vrai niveau.
                    "spectrum_db": lvl.spectrum_db,
                    // Fréquence centrale RÉELLE de chaque bande, en Hz —
                    // champ ADDITIF (#2081). Jusqu'ici `spectrum` était une
                    // suite de nombres anonymes : rien ne disait à quelle
                    // fréquence répondait la barre n° 12, et un client ne
                    // pouvait graduer son analyseur qu'en recopiant le
                    // découpage de `levels.rs`, arrondis compris, avec une
                    // fréquence d'échantillonnage devinée depuis les
                    // métadonnées de la piste. C'est la même grille que celle
                    // que l'égaliseur Expert affiche en ISO.
                    //
                    // Deux bandes voisines de même valeur lisent les mêmes
                    // raies FFT : l'analyse ne les distingue pas, et un client
                    // honnête n'y pose qu'un seul repère.
                    "spectrum_hz": &*lvl.spectrum_hz,
                    // De quoi refaire le calcul soi-même si besoin : la
                    // fréquence d'échantillonnage RÉELLEMENT analysée (celle
                    // du décodage, pas celle du tag) et la taille de FFT qui a
                    // servi — elle tombe sous 2048 sur une fenêtre courte.
                    "sample_rate": raw.sample_rate,
                    "spectrum_fft_size": lvl.spectrum_fft_size,
                    // Trames de signal RÉEL analysées, et résolution qui en
                    // découle — champs ADDITIFS (#2866). `spectrum_fft_size`
                    // seul MENTAIT sur la finesse : à 44,1 kHz une fenêtre de
                    // 1764 trames est zéro-paddée à 2048, ce qui resserre les
                    // raies à 21,5 Hz sans rien apprendre de plus que les
                    // 25,0 Hz que porte le signal. Un client qui gradue son
                    // axe doit lire `spectrum_resolution_hz`.
                    "spectrum_frames": lvl.spectrum_frames,
                    "spectrum_resolution_hz": lvl.spectrum_resolution_hz,
                    // Bande par bande : l'analyse la sépare-t-elle de ses
                    // voisines ? Les bandes du grave sont plus étroites que la
                    // résolution — 20,0 à 24,8 Hz pour la première, soit 4,8 Hz
                    // de large contre 25 Hz de résolution. Elles existent, mais
                    // elles recopient la raie de leur voisine : un client
                    // honnête ne leur pose pas de repère propre.
                    "spectrum_resolved": &*lvl.spectrum_resolved,
                }),
            );
            next_emit += window;
            position += window;
        }
    });
    tx
}

/// Le FREIN du décodage-pour-niveaux, isolé de ce QU'ON décode.
///
/// Rend le couple `(puits, relais)` que tout décodage-pour-niveaux de fichier
/// local branche sur `decode_to_pcm_streaming_with_levels` : le PCM part dans
/// le puits — personne ne le lit, seul compte le fait de borner la mémoire —
/// et les fenêtres passent par le relais, qui compte l'avance décodée avant de
/// les remettre au forwarder.
///
/// **Pourquoi un frein.** Un décodage plein pot produit les fenêtres à la
/// vitesse du DISQUE, pendant que le forwarder ne les publie qu'au TEMPS RÉEL.
/// Sa file est non bornée par construction et chaque
/// [`crate::audio::tap::RawWindow`] porte son `pcm: Vec<u8>` : sans frein, elle
/// retient la piste ENTIÈRE, et la rétention SUIT la durée du morceau au lieu
/// de plafonner. Le puits ne consomme donc pas au-delà de
/// [`PROXY_LEVELS_MAX_AHEAD_MS`] d'avance sur la position rapportée par la
/// zone, et le décodeur — bloqué sur un canal borné — s'aligne dessus.
///
/// Il s'arrête dès que le relais est fermé, donc dès que le forwarder est mort
/// (piste remplacée, zone stoppée) : le décodeur voit son consommateur
/// disparaître et rend la main au lieu de convertir la fin d'une piste que
/// plus personne n'écoute.
///
/// **Le frein ne touche PAS aux paramètres de décodage** : chaque appelant
/// garde les siens. C'est ce qui permet de brider le passthrough — qui décode
/// aux valeurs TAGUÉES de la piste (`Some(sample_rate)` / `Some(channels)`) —
/// sans le renvoyer vers [`spawn_local_file_levels_decode`], qui décode au
/// débit natif du fichier. Sur un fichier bien tagué les deux coïncident ; sur
/// un fichier mal tagué non, et le passthrough est justement le chemin de
/// cette population-là (#3145).
///
/// C'est une fonction, pas un motif à recopier : #3104 avait reproduit à la
/// main la forme du décodage-pour-niveaux, puits compris, mais avec un drain
/// inconditionnel — et a réintroduit la fuite (#3144). Le bloc du passthrough,
/// lui, ne l'avait jamais eue depuis #1423.
fn spawn_braked_levels_sink(
    playback: Arc<PlaybackManager>,
    zone_id: i64,
    levels_tx: tokio::sync::mpsc::UnboundedSender<crate::audio::tap::RawWindow>,
) -> (
    tokio::sync::mpsc::Sender<Vec<u8>>,
    tokio::sync::mpsc::UnboundedSender<crate::audio::tap::RawWindow>,
) {
    // Position DÉCODÉE, alimentée par le relais ci-dessous : c'est elle
    // que le bridage compare à la position rapportée par la zone.
    let avance_ms = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    // Relais : le décodeur ne sait pas compter son avance, mais chaque
    // fenêtre porte sa durée.
    let (relais_tx, mut relais_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::audio::tap::RawWindow>();
    {
        let avance_ms = avance_ms.clone();
        tokio::spawn(async move {
            while let Some(raw) = relais_rx.recv().await {
                avance_ms.fetch_add(
                    raw.window.as_millis() as i64,
                    std::sync::atomic::Ordering::Relaxed,
                );
                if levels_tx.send(raw).is_err() {
                    break;
                }
            }
        });
    }
    // Sonde du même canal : `is_closed()` devient vrai quand le relais a
    // rendu la main, donc quand le forwarder est mort.
    let relais = relais_tx.clone();
    let (sink_tx, mut sink_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    tokio::spawn(async move {
        while sink_rx.recv().await.is_some() {
            loop {
                if relais.is_closed() {
                    return;
                }
                let position = playback.get_state(zone_id).await.position_ms;
                if !levels_decode_doit_freiner(
                    avance_ms.load(std::sync::atomic::Ordering::Relaxed),
                    position,
                ) {
                    break;
                }
                tokio::time::sleep(LEVELS_HOLD).await;
            }
        }
    });
    (sink_tx, relais_tx)
}

/// Décode un fichier local EN FLUX, uniquement pour alimenter un forwarder de
/// niveaux neuf : c'est ce qui rend les aiguilles à la piste devenue courante
/// après une avance gapless, et ce qui les rend aussi à une piste servie depuis
/// le cache de transcodage (`transcode_cache_hit`, #3104) — là le « fichier
/// local » est la RENDITION en cache, celle qui part au renderer.
///
/// Le PCM produit part dans un puits — seules comptent les fenêtres de niveaux
/// et le fait de borner la mémoire. `decode_to_pcm` matérialisait ici la piste
/// ENTIÈRE avant d'émettre la moindre fenêtre (~1,9 Go pour un 24/192 de dix
/// minutes ; pire encore pour un DSD rendu en 176,4 kHz) : c'est la faute que
/// #1423 avait corrigée sur le chemin passthrough et qui était restée sur
/// celui-ci. Le décodeur en flux couvre en outre DSF/DFF, ce que la variante
/// « tout en mémoire » ne pouvait pas se permettre (#1541).
///
/// Le puits s'arrête dès que le forwarder est mort (piste remplacée, zone
/// stoppée) : le décodeur voit son consommateur disparaître et rend la main au
/// lieu de convertir la fin d'une piste que plus personne n'écoute.
///
/// Et il est BRIDÉ au rythme de lecture, exactement comme la sonde proxy
/// (`decode_http_stream_for_levels`) : un décodage plein pot produit les
/// fenêtres bien plus vite que le forwarder ne les publie, et la file du
/// forwarder — non bornée par construction — retiendrait alors tout le PCM de
/// la piste (~600 Mo pour un DSD64 de dix minutes rendu en 176,4 kHz). C'est
/// [`spawn_braked_levels_sink`] qui porte ce frein : le puits ne consomme pas
/// plus vite que [`PROXY_LEVELS_MAX_AHEAD_MS`] d'avance, et le décodeur,
/// bloqué sur un canal borné, s'aligne dessus.
///
/// Ce frein n'est PAS un détail d'implémentation qu'on peut recopier de
/// mémoire : #3104 a reproduit la forme de cette fonction en ligne dans la
/// branche du cache hit, puits compris, mais avec un drain inconditionnel.
/// Mesuré sur un WAV 44,1/16 stéréo
/// (`la_rendition_en_cache_ne_retient_plus_toute_la_piste`) : sans frein la
/// file retient 10 551 296 octets pour 60 s de piste et 21 102 592 pour 120 s —
/// elle SUIT la durée ; avec frein elle plafonne à 5 636 096 octets (31,9 s
/// d'audio) dans les deux cas. Tout nouvel appelant passe par ici.
fn spawn_local_file_levels_decode(
    bus: Arc<EventBus>,
    playback: Arc<PlaybackManager>,
    zone_id: i64,
    play_seq: u64,
    path: String,
) {
    tokio::spawn(async move {
        let cadence = playback.clone();
        let levels_tx = spawn_paced_levels_forwarder(bus, playback, zone_id, play_seq, 0);
        let (sink_tx, relais_tx) = spawn_braked_levels_sink(cadence, zone_id, levels_tx);
        let ready = std::sync::Arc::new(tokio::sync::Notify::new());
        let result = tokio::task::spawn_blocking(move || {
            crate::audio::decode::decode_to_pcm_streaming_with_levels(
                &path,
                None,
                None,
                None,
                sink_tx,
                LEVELS_DECODE_CHUNK,
                ready,
                relais_tx,
            )
        })
        .await;
        match result {
            Err(e) => debug!(zone_id, error = %e, "gapless_levels_task_panic"),
            Ok(Err(e)) => debug!(zone_id, error = %e, "gapless_levels_decode_failed"),
            Ok(Ok(_)) => {}
        }
    });
}

/// Avance maximale du décodage-pour-niveaux d'une session proxy sur la
/// position rapportée par la zone. Borne à la fois la mémoire (fenêtres en
/// attente dans le canal du forwarder) et la bande passante : le second fetch
/// CDN s'étale sur la durée de la piste au lieu de télécharger le fichier
/// d'un bloc.
const PROXY_LEVELS_MAX_AHEAD_MS: i64 = 30_000;

/// Le décodage-pour-niveaux doit-il attendre ? Règle unique des deux sondes —
/// la sonde HTTP d'une session proxy et le décodage de fichier local rearmé
/// après une avance gapless.
fn levels_decode_doit_freiner(avance_ms: i64, position_rapportee_ms: i64) -> bool {
    avance_ms > position_rapportee_ms + PROXY_LEVELS_MAX_AHEAD_MS
}

/// Décode un flux HTTP en arrière-plan, UNIQUEMENT pour les VU-mètres.
///
/// Une session proxy (Qobuz/Tidal direct) sert les octets CDN verbatim —
/// bit-perfect, rien n'est décodé côté serveur, donc aucun événement
/// `playback.audio_levels` n'était émis : aiguilles figées sur une piste
/// Qobuz alors qu'une piste locale (décodage-pour-niveaux du passthrough)
/// animait les VU. Même principe que ce décodage parallèle des fichiers
/// locaux, mais la « source » est l'URL CDN : on ouvre une seconde connexion
/// et on décode au fil de l'eau, cadencé sur `reported_position_ms` (position
/// de la zone, échantillonnée par l'appelant) pour rester ≤
/// [`PROXY_LEVELS_MAX_AHEAD_MS`] en avance — un seek en avant se rattrape
/// donc naturellement (le forwarder draine, la sonde décode plein pot).
///
/// Le flux servi au client n'est PAS touché : la sonde est un pur
/// observateur, son échec (CDN, codec inconnu) laisse juste les VU muets.
/// S'arrête dès que le forwarder de niveaux disparaît (stop / piste
/// remplacée : le récepteur du canal est lâché).
fn decode_http_stream_for_levels(
    url: String,
    codec_hint: String,
    levels_tx: tokio::sync::mpsc::UnboundedSender<crate::audio::tap::RawWindow>,
    reported_position_ms: std::sync::Arc<std::sync::atomic::AtomicI64>,
) -> Result<(), String> {
    use symphonia::core::audio::conv::IntoSample;
    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::{MediaSourceStream, ReadOnlySource};
    use symphonia::core::meta::MetadataOptions;
    use tracing::debug;

    // Pas de timeout total : la connexion vit toute la piste (le débit est
    // volontairement bridé au rythme de lecture, jamais idle assez longtemps
    // pour un drop keep-alive CDN).
    let response = crate::http::client::blocking_builder()
        .timeout(None)
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .and_then(|c| c.get(&url).send())
        .map_err(|e| format!("levels probe fetch failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("levels probe HTTP error: {}", response.status()));
    }

    let source = ReadOnlySource::new(response);
    let mss = MediaSourceStream::new(Box::new(source), Default::default());
    let mut hint = Hint::new();
    if !codec_hint.is_empty() {
        hint.with_extension(&codec_hint);
    }

    let mut format: Box<dyn symphonia::core::formats::FormatReader> =
        symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|e| format!("levels probe format probe failed: {e}"))?;

    let (track_id, audio_params) = {
        let track = format
            .default_track(TrackType::Audio)
            .ok_or("levels probe: no audio track found")?;
        let params = match &track.codec_params {
            Some(CodecParameters::Audio(params)) => params.clone(),
            _ => return Err("levels probe: no audio codec parameters".into()),
        };
        (track.id, params)
    };
    let channels = audio_params
        .channels
        .as_ref()
        .map(|c| c.count() as u16)
        .unwrap_or(2);
    let sample_rate = audio_params.sample_rate.unwrap_or(44100);

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
        .map_err(|e| format!("levels probe decoder init failed: {e}"))?;

    // Position décodée (référentiel piste) pour le bridage.
    let mut decoded_ms: i64 = 0;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            // Fin de piste, ou drop CDN mid-track : la sonde s'arrête là. Le
            // flux AUDIO a sa propre reprise (resumable_proxy_body) ; on ne
            // la réplique pas pour de simples VU — au pire ils gèlent en fin
            // de piste, la suivante relance une sonde neuve.
            Ok(None) => break,
            Err(e) => {
                debug!(error = %e, "levels_probe_packet_error_stopping");
                break;
            }
        };
        if packet.track_id != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(e) => {
                debug!(error = %e, "levels_probe_frame_skip");
                continue;
            }
        };

        let frames = decoded.frames();
        let ch = decoded.spec().channels().count();
        let mut interleaved: Vec<f32> = Vec::with_capacity(frames * ch);
        decoded.copy_to_vec_interleaved::<f32>(&mut interleaved);
        let mut pcm: Vec<u8> = Vec::with_capacity(interleaved.len() * 2);
        for sample in &interleaved {
            let s16: i16 = (*sample).into_sample();
            pcm.extend_from_slice(&s16.to_le_bytes());
        }

        if !crate::audio::tap::send_windowed_pcm(&levels_tx, &pcm, 16, channels, sample_rate) {
            // Forwarder parti (stop, ou piste remplacée) — plus personne
            // n'écoute, on coupe la connexion CDN.
            return Ok(());
        }
        if sample_rate > 0 {
            decoded_ms += (frames as i64) * 1000 / sample_rate as i64;
        }

        // Bridage : rester au plus PROXY_LEVELS_MAX_AHEAD_MS devant la
        // position rapportée (0 tant que la lecture n'a pas démarré — la
        // sonde constitue alors juste son avance initiale puis attend).
        while levels_decode_doit_freiner(
            decoded_ms,
            reported_position_ms.load(std::sync::atomic::Ordering::Relaxed),
        ) {
            if levels_tx.is_closed() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
    Ok(())
}

/// Corps de la sonde de niveaux proxy : forwarder cadencé + échantillonneur
/// de position (le pont entre l'horloge de lecture async et la sonde
/// bloquante) + décodage HTTP en tâche bloquante. Détaché de `self` pour être
/// appelable depuis le funnel d'avance gapless, qui démarre les niveaux même
/// pendant un pré-chargement (comme la branche locale du funnel).
///
/// `play_seq` est celui de la piste POUR LAQUELLE la sonde est créée, lu par
/// l'appelant. Le lire ici, dans la tâche, le rattachait à ce que la zone
/// jouait au moment où l'ordonnanceur daignait la démarrer : si la piste avait
/// changé entre-temps, le forwarder adoptait la génération de la NOUVELLE
/// piste et survivait au lieu de mourir, publiant le PCM de l'ancienne sur
/// l'horloge de la nouvelle (#1110).
fn spawn_proxy_levels_probe_task(
    playback: Arc<PlaybackManager>,
    bus: Arc<EventBus>,
    zone_id: i64,
    url: String,
    codec_hint: String,
    play_seq: u64,
) {
    tokio::spawn(async move {
        let levels_tx = spawn_paced_levels_forwarder(bus, playback.clone(), zone_id, play_seq, 0);
        let reported = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));

        let sampler_pos = reported.clone();
        let sampler_probe_tx = levels_tx.clone();
        let sampler_playback = playback.clone();
        tokio::spawn(async move {
            while !sampler_probe_tx.is_closed() {
                let state = sampler_playback.get_state(zone_id).await;
                sampler_pos.store(state.position_ms, std::sync::atomic::Ordering::Relaxed);
                tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
            }
        });

        let result = tokio::task::spawn_blocking(move || {
            decode_http_stream_for_levels(url, codec_hint, levels_tx, reported)
        })
        .await;
        match result {
            Ok(Ok(())) => debug!(zone_id, "proxy_levels_probe_ended"),
            Ok(Err(e)) => debug!(zone_id, error = %e, "proxy_levels_probe_failed"),
            Err(e) => debug!(zone_id, error = %e, "proxy_levels_probe_panic"),
        }
    });
}

pub struct PlaybackOrchestrator {
    pub db: Arc<dyn crate::db::backend::DbBackend>,
    pub playback: Arc<PlaybackManager>,
    pub streamer: Arc<AudioStreamer>,
    pub services: Arc<Mutex<ServiceRegistry>>,
    pub outputs: Arc<Mutex<OutputRegistry>>,
    pub advertised_ip: Option<String>,
    pub event_bus: Option<Arc<EventBus>>,
    /// Optional license manager for the free-tier zone cap (set in production,
    /// left None in tests → no gating). Enforced at zone activation in `play`.
    pub license: Option<Arc<crate::license::LicenseManager>>,
    gapless_sessions: Mutex<HashMap<i64, String>>,
    pub prefetch: Arc<PrefetchEngine>,
    dsd_capabilities: Mutex<HashMap<String, crate::outputs::dlna::DsdCapability>>,
    /// Cache of MIME types that each DLNA renderer does NOT support.
    /// Key: device_id, Value: set of unsupported MIME types (e.g. "audio/flac").
    /// Only negative results are cached — if a MIME is not in the set, it's
    /// either supported or hasn't been checked yet.
    dlna_unsupported_mimes: Mutex<HashMap<String, Vec<String>>>,
    /// Zones dont une résolution gapless est en cours : les sessions créées
    /// pendant cette fenêtre pré-chargent la piste SUIVANTE — leur attacher
    /// un forwarder de niveaux daterait les fenêtres avec l'horloge de la
    /// piste courante (stamps d'une piste sur la position d'une autre). Les
    /// niveaux de la piste suivante démarrent à l'avance gapless, voir
    /// [`Self::advance_queue_metadata`]. Verrou std : accès courts.
    levels_prewarm: std::sync::Mutex<std::collections::HashSet<i64>>,
    /// Anti-rebond du redémarrage de flux déclenché par un changement
    /// d'égaliseur sur un chemin NON local (#1710, lot 2).
    ///
    /// Deux états, deux rôles distincts :
    /// - `eq_replay_gen` : numéro de la dernière demande par zone. Chaque
    ///   demande incrémente, puis une tâche différée vérifie qu'elle est
    ///   toujours la plus récente avant d'agir. Un curseur de 31 bandes
    ///   qu'on fait glisser produit donc UN redémarrage, pas 31.
    /// - `eq_replay_last` : quand la zone a réellement redémarré. Plancher
    ///   dur, pour qu'une rafale espacée juste au-delà de l'anti-rebond ne
    ///   hache pas la lecture malgré tout.
    ///
    /// Verrous std : accès très courts, jamais tenus à travers un await.
    eq_replay_gen: std::sync::Mutex<std::collections::HashMap<i64, u64>>,
    eq_replay_last: std::sync::Mutex<std::collections::HashMap<i64, std::time::Instant>>,
    /// Per-zone record of the last track pushed to a NETWORK renderer:
    /// `zone_id → (source, source_id, when)`. Used in `play_inner` to coalesce a
    /// redundant re-play of the same track within `DUPLICATE_NET_PLAY_WINDOW`,
    /// which would otherwise restart a push renderer from 0. Cleared on `stop`.
    last_net_play: Mutex<HashMap<i64, (String, Option<String>, Option<i64>, std::time::Instant)>>,
    /// Annonces « en écoute » DIFFÉRÉES des zones navigateur (#1998).
    ///
    /// Une zone navigateur n'a pas de périphérique de sortie : la sortie est
    /// l'onglet, qui tire `stream_url` lui-même. `output_sent` y vaut donc
    /// toujours faux, et la garde posée pour le cas BluOS a du même coup
    /// supprimé TOUT scrobble de zone navigateur, y compris quand elle joue.
    ///
    /// On ne renonce pas à l'annonce et on ne la rend pas non plus gratuite :
    /// on la met en attente ici, et elle ne part qu'une fois constaté que
    /// l'onglet tire réellement des octets du flux — la même preuve que celle
    /// dont `output_reach` se sert déjà (`tune-server/src/routes/zones.rs`).
    /// Une piste que personne ne tire n'est jamais annoncée : l'entrée est
    /// simplement écrasée par la lecture suivante.
    ///
    /// Verrou std : accès très courts, jamais tenus à travers un await.
    annonces_navigateur: std::sync::Mutex<HashMap<i64, AnnonceNavigateurDifferee>>,
}

/// Ce qu'il faut pour annoncer une écoute de zone navigateur PLUS TARD, une
/// fois la lecture prouvée (voir [`PlaybackOrchestrator::annonces_navigateur`]).
///
/// Tout est capturé au démarrage, y compris `record_history` : une re-création
/// de flux pour une piste déjà en cours (recherche de position, reconnexion
/// radio) passe par `play_without_history` et ne doit PAS ajouter une seconde
/// ligne d'historique. Reconstruire ce drapeau depuis le poller serait le
/// perdre — et ferait doublonner l'historique à chaque déplacement du curseur.
#[derive(Debug, Clone)]
struct AnnonceNavigateurDifferee {
    /// Flux dont on attend qu'il soit tiré. Identifie la lecture : une
    /// nouvelle lecture crée un nouveau flux, donc l'annonce en attente d'une
    /// piste abandonnée ne peut pas partir sous l'identité de la suivante.
    stream_id: String,
    title: String,
    artist: Option<String>,
    album: Option<String>,
    source: String,
    source_id: Option<String>,
    /// Ligne de bibliothèque, quand il y en a une : l'album n'est relu qu'au
    /// moment d'écrire l'historique, pas à chaque démarrage.
    track_id: Option<i64>,
    duration_ms: i64,
    cover_path: Option<String>,
    record_history: bool,
}

/// Portée RAII de la résolution gapless d'une zone (voir `levels_prewarm`).
struct LevelsPrewarmScope<'a> {
    set: &'a std::sync::Mutex<std::collections::HashSet<i64>>,
    zone_id: i64,
}

impl Drop for LevelsPrewarmScope<'_> {
    fn drop(&mut self) {
        self.set
            .lock()
            .expect("levels_prewarm lock")
            .remove(&self.zone_id);
    }
}

#[derive(Debug, Clone, Default)]
pub struct PlayRequest {
    pub zone_id: i64,
    pub output_device_id: Option<String>,
    pub track_id: Option<i64>,
    pub source: Option<String>,
    pub source_id: Option<String>,
    pub title: Option<String>,
    pub artist_name: Option<String>,
    pub album_title: Option<String>,
    pub cover_url: Option<String>,
    pub duration_ms: Option<i64>,
    pub seek_ms: Option<u64>,
    pub temp_file_path: Option<String>,
    /// Real resolution/codec for a media-server (`source="upnp"`) URL, taken from
    /// the DIDL res@ attributes (the same the DartZeel reads). Lets the signal
    /// path show the true rate/bit-depth and infer ALAC-vs-AAC instead of
    /// defaulting to "AAC 44kHz/16bit — Avec perte" (Yves, NAS ALAC).
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u16>,
    pub media_format: Option<String>,
    /// Album numbering, passed on to the output in `PlayMedia`. Filled from the
    /// queue row (or the library track) so an output does not have to guess it.
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct PlayResult {
    pub stream_url: Option<String>,
    pub output_sent: bool,
    pub source: String,
    pub error: Option<String>,
}

pub struct ResolvedStream {
    pub url: String,
    pub mime_type: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_ms: Option<i64>,
    pub source: String,
    pub cover_url: Option<String>,
    pub stream_id: Option<String>,
    pub file_size: Option<u64>,
    /// Audio sample rate in Hz for the output stream (e.g. 176400 for DSD64->PCM).
    pub sample_rate: Option<u32>,
    /// Output bit depth (e.g. 24 for DSD->PCM).
    pub bit_depth: Option<u32>,
    /// Number of audio channels.
    pub channels: Option<u32>,
    /// The upstream URL, when `url` ended up being one of our own proxy
    /// endpoints. Carried through to `PlayMedia::origin_url` so an output can
    /// reach the source as it was published; see that field for the rationale.
    pub origin_url: Option<String>,
    /// Débit CONSTANT du flux SOURCE, en kbit/s, quand la source le nomme
    /// elle-même. Jamais déduit, jamais estimé : `None` dit « on ne sait pas »
    /// et rien ne sera affiché.
    ///
    /// Sa raison d'être est le 128 kbit/s de Bandcamp (#2074). La qualité
    /// était annoncée sur l'écran Bandcamp et se perdait au passage en zone :
    /// le chemin du signal n'affichait plus que « MP3 », indiscernable d'un
    /// 320. Or Tune s'adresse à des gens qui règlent leur chaîne au bit près,
    /// et la règle écrite dans `tune-bandcamp` veut que ce débit soit
    /// « annoncé comme tel partout où il apparaît ».
    pub bitrate_kbps: Option<u32>,
}

pub struct ResolvedQueueItem {
    pub url: String,
    pub mime_type: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub cover_url: Option<String>,
    pub duration_ms: Option<u64>,
    pub stream_id: Option<String>,
    /// Audio sample rate in Hz (e.g. 44100, 96000).
    pub sample_rate: Option<u32>,
    /// Audio bit depth (e.g. 16, 24).
    pub bit_depth: Option<u32>,
    /// Number of audio channels (e.g. 2 for stereo).
    pub channels: Option<u32>,
    /// File size in bytes for the stream.
    pub file_size: Option<u64>,
    /// Local on-disk path of the track, for outputs that read the file directly
    /// instead of the transcoded URL (OAAT native DSD gapless). Only set for
    /// local tracks resolved via the local-file gapless path; None for streaming
    /// tracks and for the normal transcode/URL resolution.
    pub file_path: Option<String>,
    /// Where the item came from, and its id there — passed to the output so it
    /// can identify the track without guessing from artist/album/title.
    pub source: Option<String>,
    pub source_id: Option<String>,
    /// Album numbering carried by the queue row.
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
}

/// Chaîne DSP d'une zone appliquée au PCM d'un bras STREAMING (Qobuz, Tidal,
/// YouTube).
///
/// `resolve_streaming_url` n'appelle JAMAIS `transcode_source_to_file` : chacun
/// de ses bras décode et ré-encode chez lui. Les trois étages n'y étaient donc
/// appliqués nulle part — sauf l'égaliseur, sur le seul bras DASH, et encore
/// sans le convolveur ni le ReplayGain (#2863). Ce porteur les regroupe dans
/// l'ORDRE canonique de `transcode_source_to_file` (1b ReplayGain, 1c
/// égaliseur, 1d convolveur FIR), pour qu'un bras ne puisse plus en oublier un.
///
/// Famille #1216 / #1168 / #1653 / #2950 : « un chemin corrigé, les autres nus ».
#[derive(Default)]
struct StreamingDsp {
    replaygain: Option<f64>,
    eq: Option<crate::audio::eq::EqProcessor>,
    convolver: Option<crate::audio::convolver::Convolver>,
}

impl StreamingDsp {
    /// Vrai dès qu'un étage est réellement actif. En mode PURE (audiophile) les
    /// trois chargeurs rendent `None`, donc `false` — le flux reste intact.
    fn is_active(&self) -> bool {
        self.replaygain.is_some() || self.eq.is_some() || self.convolver.is_some()
    }

    /// Applique les trois étages EN PLACE.
    ///
    /// Sans étage actif, `pcm` n'est pas touché d'un octet : c'est le témoin
    /// anti-régression de l'immense majorité des zones, qui doivent continuer à
    /// entendre exactement les mêmes échantillons qu'avant ce correctif.
    fn process(&mut self, pcm: &mut [u8], bit_depth: u16) {
        if let Some(factor) = self.replaygain {
            crate::audio::replaygain::apply_gain_pcm(pcm, bit_depth, factor);
        }
        if let Some(eq) = self.eq.as_mut() {
            eq.process_pcm(pcm, bit_depth);
        }
        if let Some(conv) = self.convolver.as_mut() {
            conv.process_pcm(pcm, bit_depth);
        }
    }
}

mod session;
pub(crate) use session::*;

/// Insère la chaîne DSP entre le décodeur progressif et la session HTTP.
///
/// Le bras AAC→WAV (Tidal AAC, YouTube Opus vers un renderer DLNA) pousse le
/// PCM décodé chunk par chunk dans un canal : aucun point de ce bras ne voit la
/// piste entière, donc le traitement doit s'appliquer au fil de l'eau. Les
/// trois étages sont à ÉTAT (biquads de l'égaliseur, recouvrement du
/// convolveur), ce qui rend le découpage transparent — c'est déjà ainsi que la
/// radio applique son égaliseur (#2063). Le relais préserve donc le démarrage
/// immédiat conquis en 0.9.106 : rien n'est bufferisé en plus.
///
/// ⚠️ `skip_header` : `decode_to_pcm_streaming_inner` émet l'en-tête WAV comme
/// PREMIER chunk, seul, avant le moindre octet de PCM (marqueur de journal
/// `streaming_decode_wav_header_sent`). Le passer dans l'égaliseur reviendrait
/// à filtrer les lettres « RIFF » — en-tête corrompu, donc bruit ou silence.
fn spawn_streaming_dsp_relay(
    mut dsp: StreamingDsp,
    bit_depth: u16,
    skip_header: bool,
    downstream: tokio::sync::mpsc::Sender<Vec<u8>>,
) -> tokio::sync::mpsc::Sender<Vec<u8>> {
    let (up_tx, mut up_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    tokio::spawn(async move {
        let mut header_pending = skip_header;
        while let Some(mut chunk) = up_rx.recv().await {
            if header_pending {
                header_pending = false;
            } else {
                dsp.process(&mut chunk, bit_depth);
            }
            if downstream.send(chunk).await.is_err() {
                break;
            }
        }
    });
    up_tx
}

impl PlaybackOrchestrator {
    pub fn new(
        db: Arc<dyn crate::db::backend::DbBackend>,
        playback: Arc<PlaybackManager>,
        streamer: Arc<AudioStreamer>,
        services: Arc<Mutex<ServiceRegistry>>,
        outputs: Arc<Mutex<OutputRegistry>>,
        advertised_ip: Option<String>,
    ) -> Self {
        Self {
            db,
            playback,
            streamer,
            services,
            outputs,
            advertised_ip,
            event_bus: None,
            license: None,
            gapless_sessions: Mutex::new(HashMap::new()),
            prefetch: Arc::new(PrefetchEngine::new()),
            dsd_capabilities: Mutex::new(HashMap::new()),
            dlna_unsupported_mimes: Mutex::new(HashMap::new()),
            levels_prewarm: std::sync::Mutex::new(std::collections::HashSet::new()),
            eq_replay_gen: std::sync::Mutex::new(std::collections::HashMap::new()),
            eq_replay_last: std::sync::Mutex::new(std::collections::HashMap::new()),
            last_net_play: Mutex::new(HashMap::new()),
            annonces_navigateur: std::sync::Mutex::new(HashMap::new()),
        }
    }

    // `radio_head_ok` vivait ici : un HEAD sur la station décidait s'il fallait
    // la proxifier pour un renderer DLNA. Le sondage a été ABANDONNÉ par
    // « always proxy Icecast radio to DLNA renderers » (#537) : une station liée
    // à un renderer est désormais toujours transcodée en WAV, y compris un .mp3
    // explicite dont le HEAD rend 200 (Cyrille, Yamaha R-N2000A — TSF Jazz
    // envoyée en direct restait muette). `needs_proxy`, plus bas, ne consulte
    // donc plus le réseau du tout, et la fonction est restée sans appelant.
    // Retirée plutôt que rebranchée : la rebrancher rejouerait le défaut.

    /// Délai d'anti-rebond avant de redémarrer un flux sur changement d'EQ.
    ///
    /// Assez long pour qu'un curseur qu'on fait glisser ne produise qu'un seul
    /// redémarrage, assez court pour que le réglage s'entende dans la seconde.
    const EQ_REPLAY_DEBOUNCE_MS: u64 = 500;
    /// Plancher dur entre deux redémarrages d'une même zone.
    ///
    /// L'anti-rebond seul ne suffit pas : une rafale espacée de 600 ms le
    /// franchirait à chaque fois et hacherait la lecture. Le redémarrage coûte
    /// environ une seconde de silence — deux par cinq secondes est déjà
    /// beaucoup.
    const EQ_REPLAY_FLOOR_MS: u64 = 5_000;
}

mod commun;

mod transport;

mod resolve_stream;

mod resolve_local;

mod dsp;

mod resolve_direct;

mod queue;

mod history;

mod bandcamp;
pub use bandcamp::*;

/// Arm a one-shot diagnostic for a stream URL handed to a local output.
///
/// Creating a stream session is not enough to infer a fault: gapless prepares
/// sessions well before they are meant to be consumed. The orchestrator calls
/// this only on the main local-play path, immediately before `play_media`.
/// Returning the task handle keeps the behaviour directly testable without
/// scraping logs.
async fn arm_local_stream_consumer_watch(
    streamer: Arc<AudioStreamer>,
    stream_id: String,
    zone_id: i64,
    device_id: String,
    grace: std::time::Duration,
) -> Option<tokio::task::JoinHandle<bool>> {
    use std::sync::atomic::Ordering;

    let sessions = streamer.sessions_state();
    let session = {
        let guard = sessions.lock().await;
        guard.get(&stream_id).cloned()
    }?;

    if session.consumer_watch_armed.swap(true, Ordering::AcqRel) {
        return None;
    }

    Some(tokio::spawn(async move {
        tokio::time::sleep(grace).await;

        let Some(bytes_sent) = streamer.stream_bytes_sent(&stream_id).await else {
            // The normal failure/stop paths remove the session. They already
            // carry their own diagnostic and must not trigger a stale alert.
            return false;
        };
        if bytes_sent != 0 {
            return false;
        }

        let file_path = session.file_path.lock().await.clone();
        let proxy_url = session.proxy_url.lock().await.clone();
        let active_consumers = session.active_consumers.load(Ordering::Relaxed);
        let session_age_ms = session.created_at.elapsed().as_millis() as u64;
        warn!(
            zone_id,
            device_id = %device_id,
            stream_id = %stream_id,
            grace_ms = grace.as_millis() as u64,
            session_age_ms,
            format = %session.info.format,
            mime_type = %session.info.mime_type,
            file_path = ?file_path,
            proxy_url = ?proxy_url,
            active_consumers,
            "local_stream_never_consumed"
        );
        true
    }))
}

// Le repli NFC/NFD qui vivait ici en `fn` privée est parti dans
// `crate::library::local_path` (#1865) : il était connu, documenté et
// correct, mais enfermé — la lecture en profitait, aucune passe de fond ne
// pouvait s'en servir. Il est importé en haut de fichier, pas réécrit : une
// seconde normalisation, forcément divergente, serait pire que l'absence.

#[cfg(test)]
mod transcode_budget_tests;

/// #3140 — le budget suit le DÉBIT DE DÉCODAGE de l'hôte, plus la seule taille.
///
/// ## Le fait de base mesuré ici
///
/// À débit de décodage donné et durée de piste donnée, **le transcodage
/// aboutit au lieu d'expirer**. Pas un code HTTP, pas un compte d'événements :
/// la valeur rendue par le chien de garde, `Ok` ou `Err`.
///
/// ## Aucun `sleep` réel
///
/// Tout tourne sous `#[tokio::test(start_paused = true)]` : l'horloge de tokio
/// est virtuelle, `tokio::time::Instant` la suit, et le décodeur feint publie
/// sur la même balise que le vrai. Un test de budget qui dort serait
/// intermittent ; celui-ci s'exécute en quelques millisecondes et rend TOUJOURS
/// le même verdict.
#[cfg(test)]
mod budget_adaptatif_tests;

/// La regle de decision du passthrough DSD (#2122).
///
/// Les douze combinaisons : quatre modes croises avec les trois reponses
/// possibles du sondage. La sonde reseau n'est pas testee ici — c'est
/// justement pour la sortir du chemin qu'elle a ete extraite.
#[cfg(test)]
mod dsd_passthrough_tests;

#[cfg(test)]
mod resolution_annoncee_tests;

#[cfg(test)]
mod wav_override_tests;

#[cfg(test)]
mod tests;

/// Plafond de profondeur en sortie (#1610).
///
/// Marantz ND8006 muet, « format non supporte » : le flux partait en 32 bits,
/// valeur lue dans `track.bit_depth` et jamais ramenee a une profondeur que
/// l'appareil sait lire. La regle existait a trois endroits et manquait au
/// quatrieme.
#[cfg(test)]
mod bit_depth_cap_tests;

#[cfg(test)]
mod dop_routing_tests;

/// Garde-fou #1998 : ce que la sortie a refusé n'est annoncé nulle part.
///
/// Chez Bilou, quatre échecs de sortie BluOS d'affilée ont produit quatre
/// annonces « en écoute » à Last.fm pour un titre jamais entendu. L'annonce
/// partait AVANT la tentative d'envoi ; `output_sent` était établi vingt lignes
/// plus bas, dans la même fonction, et l'arrêt immédiat de la zone s'appuyait
/// déjà dessus.
///
/// Ce test relit la source plutôt que d'exercer le chemin de lecture : la
/// propriété à tenir est un ORDRE et une CONDITION dans une fonction async de
/// plusieurs centaines de lignes, et c'est exactement ce qu'un copier-coller
/// ultérieur ré-inverse. Même procédé que `eq_refresh_guard` dans
/// `tune-server/src/routes/mod.rs`, et pour la même raison.
#[cfg(test)]
mod annonce_apres_sortie_guard;

#[cfg(test)]
mod stop_scope_tests;

/// La profondeur ANNONCÉE au renderer et celle réellement ÉCRITE dans le flux
/// doivent être le même nombre (#1437).
///
/// `transcode_source_to_file` ne descendait que la profondeur (`target_bd <
/// actual_bd`) et ne la montait jamais, et personne ne bornait la cible aux
/// trois largeurs que la chaîne sait écrire. Deux défauts, deux symptômes
/// distincts, mesurés ici sur un vrai fichier ALAC produit par l'encodeur du
/// dépôt :
///
/// - cible plus PROFONDE que la source (`dlna_wav24` sur une piste que la base
///   annonce 24 bits alors qu'elle en fait 16) : le fichier restait à la
///   largeur de la source pendant que le DIDL annonçait la cible — deux octets
///   par échantillon lus à un pas de trois, la famille #1137 ;
/// - cible ININSCRIPTIBLE (20 bits, légal en ALAC/FLAC/AIFF/WavPack et rendu
///   tel quel par `cap_output_bit_depth`) : `encode_wav` refusait après le
///   décodage complet, la piste ne démarrait jamais.
#[cfg(test)]
mod profondeur_annoncee_egale_profondeur_ecrite;

/// #1770 (point 3) — le SITE de production, pas seulement la résolution.
///
/// L'essai voisin (`les_reglages_de_sortie_locale_viennent_de_la_base`) prouve
/// que `reglages_sortie_locale` lit bien la base ; il resterait vert si
/// quelqu'un remettait `LocalOutput::new(...)` — ou
/// `with_options(nom, false, "auto")` — dans `recreate_local_and_play`. C'est
/// ce site-là que ce module épingle.
///
/// Hors de toute `feature` : `recreate_local_and_play` vit derrière
/// `local-audio`, qui n'est PAS dans le jeu du job `test` de la CI
/// (`--no-default-features --features oaat,cloud-relay,bandcamp`). Une garde
/// posée derrière cette fonctionnalité ne serait exécutée que par
/// `test-shipped-features` et `audio-embedding`, tous deux conditionnés à
/// `full` — donc jamais sur une PR vers `batch/*`. `include_str!` lit le
/// texte du fichier quelles que soient les `cfg`.
#[cfg(test)]
mod recreation_locale_guard;
