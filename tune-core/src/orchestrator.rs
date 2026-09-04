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

/// A public `play()` for the track ALREADY playing on a zone that arrives within
/// this window of the track's start is treated as a redundant controller
/// double-dispatch (a re-tap) and coalesced at the entry point — BEFORE any
/// Retard de départ délibéré sur une radio live, en secondes (#1628).
///
/// Trois secondes couvrent une frontière de segment entière (~8 s de cadence
/// observée, arrivées jusqu'à 100 ms en retard) tout en restant imperceptibles
/// au zapping. En dessous, la réserve se vide à la première irrégularité ;
/// au-dessus, on ferait attendre l'auditeur pour rien.
const RADIO_PREBUFFER_SECS: u64 = 3;

/// re-resolve or re-send. The `superseded` play_seq guard in `play_inner` only
/// catches an OVERLAPPING second play; when the second play arrives just AFTER
/// the first fully established playback (sequential, a few seconds apart), that
/// guard sees no overlap and lets it through, and the second `SetAVTransportURI`
/// restarts a network renderer from byte 0 (Revox S100 "plays ~10s then jumps to
/// 0" — #1271). Kept short so a deliberate replay of the same track — which
/// lands far later — plays normally; a seek is exempt regardless.
const RETAP_DEDUP_WINDOW: std::time::Duration = std::time::Duration::from_secs(8);

/// Resuming a WEBRADIO after a pause longer than this is treated as a re-play
/// of the station (new upstream connection, new decode session, new stream URL
/// to the output) instead of resuming the paused pipeline. A radio stream is
/// LIVE: while the zone is paused its pipeline keeps ageing — the icecast
/// connection can die through debug-only exit paths, the output keeps
/// buffering an unbounded backlog, and OAAT packet timestamps fall behind the
/// endpoint clock by the whole pause — so a "resume" past a few seconds
/// renders silence with nothing in the logs (#1629, .42: 19 min pause → total
/// silence, volume changes ignored). Chosen ABOVE `DUPLICATE_NET_PLAY_WINDOW`
/// (12 s) so the re-play issued here can never be coalesced as a duplicate
/// net send; short pauses below the threshold keep today's working in-place
/// resume.
const RADIO_RESUME_REPLAY_AFTER: std::time::Duration = std::time::Duration::from_secs(15);

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

/// Le rang qu'il faut écrire dans `listen_history`, sachant l'état de la zone.
///
/// Toute la subtilité est le tirage aléatoire. En lecture séquentielle, le
/// rang est ce qui permet de reprendre la playlist à la piste 7. En lecture
/// aléatoire, il ne désigne plus rien de reproductible : la permutation
/// (`shuffle_order`) est régénérée à chaque activation, donc « position 7 »
/// dans le tirage d'hier tombera sur une autre piste demain. L'arbitrage rendu
/// sur #2441 est de RE-TIRER plutôt que de faire semblant de rejouer le même
/// tirage — ce que cette fonction inscrit à l'écriture, faute de quoi il
/// faudrait redevenir l'état « aléatoire » de la zone au moment où l'accueil
/// s'affiche, ce qui n'existe plus.
///
/// Un `None` en base se relit donc « rouvre au début », que ce soit parce
/// qu'on re-tire ou parce que la ligne est antérieure à la migration 94.
pub fn rang_a_retenir(shuffle: bool, queue_position: i64) -> Option<i64> {
    if shuffle || queue_position < 0 {
        None
    } else {
        Some(queue_position)
    }
}

/// Le forçage WAV d'une zone s'applique-t-il à CETTE source ?
///
/// « Forcer le WAV » (`dlna_lpcm` / `dlna_wav24`) existe pour contourner le
/// décodeur ALAC du renderer — le LHC-56 de Yves claque au démarrage sur de
/// l'ALAC direct. L'appliquer aussi aux sources FLAC est un dommage
/// collatéral, jamais l'objectif.
///
/// Tant que les deux réglages s'excluaient, « Forcer le WAV » l'emportait en
/// silence sur « FLAC natif » : un FLAC partait en WAV sans que rien ne
/// l'explique, et l'utilisateur en déduisait que Tune gardait en mémoire les
/// réglages du morceau précédent (forum #1437). Les deux cases décrivent en
/// réalité deux sources différentes et peuvent coexister : l'ALAC part en WAV,
/// le FLAC reste du FLAC.
///
/// L'exception exige l'opt-in `dlna_native_flac`. Sans lui, une source FLAC
/// continue de suivre le forçage — ce dont ont besoin les renderers qui ne
/// savent pas lire le FLAC.
pub fn wav_override_applies(
    force_wav_requested: bool,
    source_is_flac: bool,
    native_flac_opt_in: bool,
) -> bool {
    force_wav_requested && !(source_is_flac && native_flac_opt_in)
}

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

/// Warm-cache for Tidal/Qobuz HI-RES DASH transcodes is opt-in: it changes the
/// file served on the HI-RES streaming path (cache-hit → a previously-finished
/// transcode instead of a fresh one), so it stays OFF until validated on a real
/// DLNA renderer. Enable with `TUNE_DASH_WARM_CACHE=1`.
/// Facteur linéaire d'un trim de gain par renderer (`zone_{id}_gain_trim_db`).
/// Clampe à ±12 dB — au-delà, on harmonise plus rien, on casse.
fn gain_trim_factor(trim_db: f64) -> f64 {
    10f64.powf(trim_db.clamp(-12.0, 12.0) / 20.0)
}

fn dash_warm_cache_enabled() -> bool {
    std::env::var("TUNE_DASH_WARM_CACHE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
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

/// DIDL `res@duration` (ms) for a native passthrough stream served raw to a
/// network renderer (FLAC/ALAC/… sent as-is, not transcoded).
///
/// Prefer the file container's authoritative duration (`probed_secs`, read from
/// the FLAC STREAMINFO / lofty properties — the true playable length of the
/// bytes we serve) over the scanned `track.duration_ms`, which can be a few
/// seconds too long (recovered by a slow/fallback NAS scan, or drifted vs. the
/// real sample count). An over-long `res@duration` makes the gapless-queued
/// (SetNextAVTransportURI) track cut near EOF and lose its progress display on
/// the Marantz ND 8006 (#1132), because the renderer models the auto-advanced
/// track purely from the DIDL instead of re-probing the stream.
///
/// Falls back to the scanned duration when the probe failed or is non-positive
/// (e.g. a NAS read timeout), so we never blank the duration entirely.
fn passthrough_didl_duration_ms(probed_secs: Option<f64>, scanned_ms: i64) -> i64 {
    probed_secs
        .filter(|s| s.is_finite() && *s > 0.0)
        .map(|s| (s * 1000.0).round() as i64)
        .filter(|&ms| ms > 0)
        .unwrap_or(scanned_ms)
}

/// Recover a local file's real duration (ms) at play time when the DB row has
/// none (`duration_ms <= 0`). DSD (`.dsf`/`.dff`) is computed from the header —
/// lofty (which `get_duration` uses) reports 0 for most DSD files, which is how
/// the 0 got into the DB in the first place — everything else falls back to
/// lofty. Returns `None` when no positive duration can be determined.
async fn probe_local_duration_ms(
    file_path: &str,
    source_format: Option<AudioFormat>,
) -> Option<i64> {
    if source_format == Some(AudioFormat::Dsd) {
        let p = file_path.to_string();
        return tokio::task::spawn_blocking(move || {
            let dur = if p.to_ascii_lowercase().ends_with(".dff") {
                crate::audio::dff::parse_dff(&p)
                    .ok()
                    .and_then(|i| i.duration_ms())
            } else {
                crate::audio::dsf::parse_dsf(&p)
                    .ok()
                    .and_then(|i| i.duration_ms())
            };
            dur.map(|ms| ms as i64)
        })
        .await
        .ok()
        .flatten()
        .filter(|&ms| ms > 0);
    }
    crate::audio::analyzer::get_duration(file_path)
        .await
        .ok()
        .map(|s| (s * 1000.0) as i64)
        .filter(|&ms| ms > 0)
}

/// La commande de transport a-t-elle pu être exécutée malgré l'erreur remontée ?
///
/// Un timeout SOAP (voir [`crate::outputs::dlna::SOAP_TIMEOUT_PREFIX`]) ne prouve
/// rien : la requête a pu atteindre un renderer lent et être honorée, seule la
/// réponse a manqué. Un refus de connexion, lui, est concluant — rien n'est
/// parti. Ce prédicat décide si l'on conserve la session de flux.
pub(crate) fn command_may_have_landed(err: &str) -> bool {
    // `contains` et non `starts_with` : send_to_output enveloppe l'erreur de la
    // sortie dans « Output device error: {e} », le marqueur n'est donc jamais en
    // tête. Un test couvre précisément ce chemin — s'y fier plutôt qu'à la forme
    // supposée de la chaîne.
    err.contains(crate::outputs::dlna::SOAP_TIMEOUT_PREFIX)
}

/// Le flux qui part sur le fil est-il du DSD BRUT ?
///
/// `application/x-dsd` par défaut, ou le MIME que le lecteur a lui-même
/// annoncé pour le DSF/DFF (`audio/x-dsf`, `audio/dff`…) : le passthrough
/// reprend celui-là quand il existe, parce que certains renderers n'acceptent
/// que le MIME exact qu'ils publient. Aucun MIME PCM ne porte ces trois
/// lettres, donc la reconnaissance ne peut pas mordre à côté — un FLAC servi à
/// la même zone n'est jamais confondu avec un DSD.
pub fn est_dsd_brut(mime_type: &str) -> bool {
    let m = mime_type.to_ascii_lowercase();
    m.contains("dsd") || m.contains("dsf") || m.contains("dff")
}

/// Should a network output be fed from a pre-transcoded temp file (blocking,
/// Content-Length known) rather than a streaming session?
///
/// A temp file is required for renderers that reject chunked transfer
/// (darTZeel LHC-208 etc.): every non-WAV target (FLAC), and every WAV target a
/// renderer demands as raw LPCM (`dlna_needs_wav`). The exception is
/// `dsd_lpcm_streams` — a DSD source going out as WAV/LPCM with the
/// `dsd_lpcm_stream` toggle on: the streaming path already advertises an exact
/// Content-Length (`StreamInfo::wav_content_length`), so blocking to /tmp is
/// pointless and, on DSD256/512, fatal (the ~decode exceeds the 120s temp-file
/// timeout → the renderer plays silence).
///
/// `dsp_active` PRIME sur tout le reste. Le bras progressif appelle
/// `decode_to_pcm_streaming_seeked`, qui ne reçoit ni égaliseur, ni convolveur,
/// ni facteur ReplayGain : seul `transcode_source_to_file` les applique. Une
/// zone dont un traitement est actif doit donc repasser par le fichier, sans
/// quoi le traitement est perdu EN SILENCE — famille #1216, déjà corrigée pour
/// le passthrough réseau, le navigateur et les sorties PULL.
///
/// Kept a pure function so the decision matrix is unit-testable without an
/// orchestrator.
fn use_file_transcode_for(
    is_network: bool,
    target_is_wav: bool,
    dlna_needs_wav: bool,
    dsd_lpcm_streams: bool,
    dsp_active: bool,
) -> bool {
    is_network && (!target_is_wav || (dlna_needs_wav && !dsd_lpcm_streams)) || dsp_active
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

/// Le bras streaming HTTPS doit-il PRÉ-TRANSCODER au lieu de servir les octets
/// du CDN verbatim ?
///
/// Deux raisons, et la seconde manquait : le renderer ne sait pas lire le MIME
/// amont (Denon, Marantz, Revox — pas d'`audio/flac` dans leur Sink), OU un
/// traitement de zone doit entrer dans le signal. Une session proxy ne décode
/// rien : égaliseur, convolveur et ReplayGain y sont perdus EN SILENCE. C'est
/// le bras que ni #1168 (navigateur), ni #1653 (sorties PULL), ni #2950 (bras
/// progressif de `play_inner`) n'atteignaient — aucun ne passe par ici.
///
/// Fonction pure : la matrice de décision se teste sans orchestrateur, comme
/// `use_file_transcode_for`.
fn streaming_needs_pretranscode(renderer_supports_mime: bool, dsp_active: bool) -> bool {
    !renderer_supports_mime || dsp_active
}

/// Format d'encodage du pré-transcodage streaming.
///
/// WAV/LPCM quand le renderer a REFUSÉ le MIME amont : c'est le profil
/// `DLNA.ORG_PN=LPCM`, 16 bits seulement, d'où le plafond historique (#1137,
/// Ruark R3 / LHC-62 muets en 24 bits sous ce profil).
///
/// FLAC quand il l'accepte et que SEUL le traitement impose le pré-transcodage :
/// le plafond 16 bits n'a alors aucune raison d'être, et l'appliquer
/// dégraderait un Hi-Res 24 bits pour un simple égaliseur. C'est le même
/// arbitrage que le bras DASH, qui encode déjà en FLAC pleine profondeur quand
/// le renderer sait le lire.
fn streaming_pretranscode_format(renderer_supports_mime: bool) -> &'static str {
    if renderer_supports_mime {
        "flac"
    } else {
        "wav"
    }
}

/// Les types de sortie qui poussent l'audio vers un appareil PAR LE RÉSEAU.
///
/// **L'unique exemplaire de cette liste.** Elle était recopiée à l'identique en
/// trois endroits — `is_push_uri_output_type`, `resolve_local_track` et
/// [`PlaybackOrchestrator::seek`] — et #2893 en réclamait une quatrième. Toutes
/// y passent désormais : un renderer ajouté ici l'est partout, alors qu'une
/// copie oubliée serait restée MUETTE (un morceau qui repart du début).
///
/// `pub` depuis #2189 : il existait une QUATRIÈME copie, hors de cette caisse
/// — `build_signal_path` (`tune-server/src/routes/zones.rs`) — et elle avait
/// déjà dérivé : cinq types au lieu de six, `slimproto` manquant. Le panneau
/// et le chemin audio répondaient donc à deux questions différentes sur la
/// même zone. Le miroir d'affichage appelle maintenant CETTE fonction.
pub fn is_network_output_type(output_type: Option<&str>) -> bool {
    matches!(
        output_type,
        Some("dlna")
            | Some("openhome")
            | Some("chromecast")
            | Some("bluos")
            | Some("squeezebox")
            | Some("slimproto")
    )
}

/// La sortie va CHERCHER le flux elle-même et reçoit donc nos octets **tels
/// quels** : `hqplayer`, `airplay2`, `diretta`, tout greffon hors dépôt.
///
/// C'est la troisième famille de [`pull_output_needs_dsp_transcode`], extraite
/// telle quelle — ni élargie, ni rétrécie. Elle existe séparément parce que le
/// panneau du chemin du signal en a besoin SANS les drapeaux d'exécution
/// (`is_local`, `is_oaat`, format source) : il n'a que le type de la zone.
///
/// La conséquence pour l'affichage est directe et c'est tout le sujet de
/// #2189 : sur ces sorties, le transport ne touche AUCUN échantillon, donc il
/// est bit-perfect. Le seul traitement qui puisse s'y appliquer est celui que
/// [`pull_output_needs_dsp_transcode`] force — EQ, correction de pièce,
/// ReplayGain — et le panneau le compte déjà à part. Le bras par défaut de
/// `build_signal_path` rendait `false` inconditionnellement : une zone
/// HQPlayer était déclarée « non bit-perfect » sur un FLAC 44,1/16 servi
/// octet pour octet (Alex Campbell, 0.9.98 Linux, fil 1524).
pub fn is_pull_dsp_output_type(output_type: Option<&str>) -> bool {
    output_type.is_some() && !is_network_output_type(output_type) && output_type != Some("browser")
}

/// Après une recréation de flux à une position donnée, faut-il ENCORE envoyer
/// un `Seek` à la sortie ?
///
/// # Le défaut #2893
///
/// [`PlaybackOrchestrator::replay_zone_at_position`] recrée le flux avec
/// `PlayRequest { seek_ms: Some(position_ms), .. }` et s'arrête là. Or
/// `seek_ms` n'est honoré que par les deux bras qui **décodent** —
/// `decode_to_pcm_streaming_seeked` reçoit l'offset et démarre le PCM à la
/// bonne seconde. Tous les autres bras posent le flux dans une **session
/// fichier** (transcodage vers un temporaire, cache, passthrough natif) ou une
/// **session proxy** (CDN) : ces sessions servent depuis l'**octet 0** et ne
/// regardent jamais `seek_ms`.
///
/// Et ces deux familles sont exactement les deux moitiés de
/// `is_seekable_session` :
///
/// | session | `seek_ms` honoré | « range-seekable » |
/// |---|---|---|
/// | décodée (canal mpsc) | **oui**, par le producteur | non |
/// | fichier / proxy | **non**, servie depuis 0 | **oui** |
///
/// D'où la règle : une session seekable sur une sortie réseau a été recréée au
/// début du morceau, et il ne manque plus QUE le `Seek` SOAP pour amener le
/// renderer à la position. Une session non seekable, elle, part déjà de
/// l'offset — lui envoyer un `Seek` **doublerait** le saut, la panne de la
/// famille #1518 (« un seek à 4:30 jetait tout le PCM restant → silence
/// total »).
///
/// C'est le miroir de la condition de [`PlaybackOrchestrator::seek`], et non sa
/// copie : là-bas une session seekable se contente d'un `Seek` **sans**
/// recréation ; ici la recréation a déjà eu lieu et impose le `Seek`.
///
/// Symptôme corrigé : sur un Marantz ND8006 en DLNA, basculer le mode Pure
/// faisait repartir le morceau du début — dans les deux sens, puisque les deux
/// sens changent de bras de streaming sans jamais quitter la session fichier
/// (Jean Valjean, 0.9.126, fil 1618).
///
/// Fonction pure : la matrice de décision se teste sans orchestrateur, comme
/// [`use_file_transcode_for`] et [`streaming_needs_pretranscode`].
fn replay_needs_output_seek(
    is_network_output: bool,
    session_is_range_seekable: bool,
    position_ms: u64,
) -> bool {
    is_network_output && session_is_range_seekable && position_ms > 0
}
/// Ce qu'une reprise doit faire de la SESSION DE FLUX d'une zone.
///
/// Reprendre « sur place » suppose que la session qui alimentait la sortie a
/// survécu à la pause. Il y a deux façons de ne pas y survivre, et surtout DEUX
/// REMÈDES qui n'ont rien à voir l'un avec l'autre. C'est tout l'objet de ce
/// type : le premier existait déjà, le second manquait, et les confondre aurait
/// été un défaut de plus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RepriseDeSession {
    /// La session vit : `checked_resume` sur la sortie suffit, et c'est ce qui
    /// marche aujourd'hui.
    SurPlace,
    /// RADIO seulement (#1629). Un flux radio est un DIRECT : on ré-amorce la
    /// station, position JETÉE. On reprend le direct, pas un différé de dix-neuf
    /// minutes.
    RejouerLeDirect,
    /// PISTE (#2512). Rétablir la MÊME écoute au MÊME point. Rejouer une piste
    /// « depuis le direct » n'aurait aucun sens : l'auditeur veut retrouver son
    /// morceau là où il l'a laissé.
    RetablirALaPosition,
    /// La session est morte et rien ne permet de la rétablir. Il reste à le
    /// DIRE : un silence sans message est un défaut à lui seul.
    Expliquer,
}
/// La matrice de décision de [`PlaybackOrchestrator::resume`].
///
/// Fonction PURE, et c'est ce qui permet de l'éprouver : `paused_at` est un
/// `std::time::Instant` que `tokio::time::pause()` n'atteint pas, donc une pause
/// de vingt minutes ne se joue pas dans un test — mais un booléen, si.
///
/// La branche RADIO est la table de vérité de #1629, inchangée : rejouer dès que
/// la pause dépasse le seuil OU que le producteur de décodage est mort, jamais
/// sans URL de station. Elle a été écrite contre un cas mesuré et elle
/// fonctionne ; elle n'est ni généralisée, ni dupliquée, ni contournée.
///
/// La branche PISTE ne regarde PAS `pause_longue`. C'est délibéré et c'est le
/// cœur du correctif : une piste dont la session vit encore reprend sur place,
/// qu'on l'ait laissée trente secondes ou trois heures. Seule la mort de la
/// session — le ramasse-miettes est passé — justifie de rétablir quoi que ce
/// soit, et alors on rétablit à la position, pas au début.
pub(crate) fn reprise_de_session(
    est_radio: bool,
    rejouable: bool,
    pause_longue: bool,
    session_morte: bool,
) -> RepriseDeSession {
    if est_radio {
        if rejouable && (pause_longue || session_morte) {
            RepriseDeSession::RejouerLeDirect
        } else {
            RepriseDeSession::SurPlace
        }
    } else if !session_morte {
        RepriseDeSession::SurPlace
    } else if rejouable {
        RepriseDeSession::RetablirALaPosition
    } else {
        RepriseDeSession::Expliquer
    }
}
/// La demande de lecture qui RÉTABLIT la session d'une piste au point exact où
/// la pause l'a laissée.
///
/// C'est ce qui sépare ce correctif d'une transposition du comportement radio.
/// Le re-play d'une station jette la position — il le doit. Ici la position est
/// le cœur de la demande : `seek_ms` porte le `position_ms` que l'état de zone a
/// conservé à travers la pause. Tout le reste désigne la même écoute, d'où
/// `play_without_history` chez l'appelant : pas de seconde ligne d'historique,
/// même règle que le re-play radio.
///
/// Les champs de résolution restent `None` : `play_inner` re-résout la piste
/// depuis `track_id`/`source_id` comme au premier lancement, et une valeur
/// recopiée ici ne pourrait que le contredire.
///
/// Fonction PURE : le contrat « la MÊME piste, au MÊME point » se prouve sans
/// orchestrateur, sans sortie et sans fichier.
pub(crate) fn requete_de_retablissement(
    zone_id: i64,
    output_device_id: String,
    np: &NowPlaying,
    position_ms: u64,
) -> PlayRequest {
    PlayRequest {
        zone_id,
        output_device_id: Some(output_device_id),
        track_id: np.track_id,
        source: Some(np.source.clone()),
        source_id: np.source_id.clone(),
        title: Some(np.title.clone()),
        artist_name: np.artist_name.clone(),
        album_title: np.album_title.clone(),
        cover_url: np.cover_path.clone(),
        duration_ms: (np.duration_ms > 0).then_some(np.duration_ms),
        seek_ms: Some(position_ms),
        temp_file_path: None,
        sample_rate: None,
        bit_depth: None,
        media_format: None,
        track_number: None,
        disc_number: None,
    }
}
/// La phrase que la zone rend quand sa session n'a pas survécu à la pause et
/// n'a pas pu être rétablie.
///
/// Elle existe parce que l'absence de message EST le défaut : « aucun son,
/// volume dans le vide » et pas une ligne pour dire pourquoi. Elle nomme donc
/// les trois choses que l'auditeur ne peut pas deviner — quelle piste, à quelle
/// position, et pourquoi la session n'est plus là.
///
/// `position_ms` est FACULTATIF (#3244). Sur une zone navigateur personne ne
/// mesure la position — le sondeur ne passe pas — et `position_ms` y vaut 0
/// depuis `play()`. Écrire « ne peut pas reprendre à 0:00 » ferait passer cette
/// absence de mesure pour une mesure, et désignerait le début du morceau alors
/// que l'auditeur en était peut-être à la moitié. `None` dit « je ne sais pas »
/// et la phrase le dit aussi : c'est la même distinction que
/// [`PlaybackOrchestrator::position_entretenue_par_le_sondeur`] pose pour
/// #2595, au site voisin.
///
/// Fonction PURE, éprouvée sans orchestrateur.
pub(crate) fn message_session_perdue(
    titre: &str,
    position_ms: Option<u64>,
    cause: Option<&str>,
) -> String {
    let minutes = crate::http::streamer::SESSION_IDLE_TIMEOUT.as_secs() / 60;
    let mut phrase = match position_ms {
        Some(ms) => {
            let secondes = ms / 1000;
            format!(
                "La lecture de « {titre} » ne peut pas reprendre à {}:{:02} : sa session de \
                 flux n'a pas survécu à la pause (le serveur la libère après {minutes} minutes \
                 sans lecture). Relancez la piste.",
                secondes / 60,
                secondes % 60,
            )
        }
        // Position non mesurée : on nomme la piste et la cause, jamais un
        // horodatage inventé.
        None => format!(
            "La lecture de « {titre} » ne peut pas reprendre là où elle en était : sa \
             session de flux n'a pas survécu à la pause (le serveur la libère après \
             {minutes} minutes sans lecture), et cette zone est lue par le navigateur — \
             le serveur n'y mesure pas la position de lecture. Relancez la piste."
        ),
    };
    if let Some(cause) = cause {
        phrase.push_str(&format!(" Cause : {cause}"));
    }
    phrase
}

/// Temporisation avant le `Seek` qui suit une recréation de flux réseau.
///
/// Même valeur, et même raison, que la branche réseau de
/// [`PlaybackOrchestrator::seek`] : le renderer vient de recevoir une URL
/// neuve, il faut lui laisser commencer à bufferiser avant de lui demander de
/// sauter. Le ND8006 fait déjà des `soap_retry` sur `GetTransportInfo` dans ces
/// instants-là.
const REPLAY_OUTPUT_SEEK_SETTLE_MS: u64 = 500;

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

/// Is `output_type` (from [`PlaybackOrchestrator::output_type_of`]) one of the
/// push-URI renderer types (DLNA/OpenHome/Chromecast/BluOS/Squeezebox/
/// Slimproto) that receives a URI and can restart playback from byte 0 on a
/// redundant `SetAVTransportURI` (or equivalent) — the failure mode the
/// duplicate-net-play coalescing in `play_inner` (#1129) guards against?
/// Pull-based outputs — `local`, and out-of-tree outputs like `oaat` and
/// `diretta` that fetch/stream audio themselves rather than being pushed a
/// URI — never exhibit it, so they must be excluded here rather than only via
/// a `device_id` naming convention (a pull output has no reason to prefix its
/// `device_id` with `"local:"`). Pure so it's unit-testable without an
/// orchestrator or a registered output.
/// True when an output receives the stream with none of our DSP applied to it,
/// so an active EQ / room correction / ReplayGain must force the transcode path
/// to be heard at all.
///
/// The push-URI renderers of [`is_push_uri_output_type`] and browser zones are
/// handled by their own flags. What this covers is the third family: a PULL
/// output that fetches the audio itself and is not built in — `diretta`, and
/// anything an out-of-tree plugin registers. It was silently absent from the
/// list, so the EQ was computed and thrown away (Eric, forum ; same hole as
/// #1216 on a Beoplay A9).
///
/// Excluded on purpose:
/// - `local` and `oaat`, which already transcode as soon as the source format
///   is known, so the DSP reaches them;
/// - an unknown format, which there is no safe way to transcode;
/// - DSD, because converting a native DSD stream to PCM in order to apply an
///   EQ would be a degradation decided on the listener's behalf.
fn pull_output_needs_dsp_transcode(
    output_type: Option<&str>,
    is_local: bool,
    is_oaat: bool,
    source_format: Option<AudioFormat>,
) -> bool {
    // La part « type de sortie » vit dans [`is_pull_dsp_output_type`], d'où le
    // panneau du chemin du signal la lit aussi (#2189). Même ensemble, à la
    // lettre : `is_push_uri_output_type` n'est qu'un alias de
    // `is_network_output_type`, que la fonction extraite appelle.
    is_pull_dsp_output_type(output_type)
        && !is_local
        && !is_oaat
        && source_format.is_some()
        && source_format != Some(AudioFormat::Dsd)
}

/// Les sorties qui reçoivent une URI et peuvent repartir de l'octet 0 sur un
/// envoi redondant (Revox S100) — la porte de coalescence de #1129.
///
/// Même ensemble que [`is_network_output_type`], et ce n'est pas un hasard :
/// « pousser une URI » est précisément ce que fait une sortie réseau ici. La
/// liste ne vit donc qu'à UN endroit ; ce nom-ci reste pour que la porte #1129
/// se lise pour ce qu'elle est.
fn is_push_uri_output_type(output_type: Option<&str>) -> bool {
    is_network_output_type(output_type)
}

/// Profondeur de bits admissible par un appareil de lecture, en sortie.
///
/// Plancher a 16 : en dessous, plus rien ne lit le PCM de facon fiable.
/// Plafond a 24 : c'est la limite de la quasi-totalite des lecteurs reseau —
/// le Marantz ND8006 de Jean Valjean affiche « format non supporte » et reste
/// muet devant un flux 32 bits, qu'il soit transcode en WAV ou envoye en FLAC
/// direct (#1610). Le 32 bits venait de `track.bit_depth`, donc du scan : le
/// format FLAC l'autorise, et rien ne le ramenait a une valeur jouable.
///
/// La regle etait deja appliquee a trois endroits, ecrite de trois facons
/// (`max(16).min(24)`, `min(24).max(16)`) — et oubliee au quatrieme. Une
/// fonction unique rend l'oubli impossible a reproduire.
pub(crate) fn cap_output_bit_depth(bit_depth: u16) -> u16 {
    bit_depth.clamp(16, 24)
}

/// Resolution ANNONCEE d'une piste : frequence ou profondeur, telle que le
/// client l'affiche.
///
/// `ligne` est la valeur de la ligne `tracks` (la source, ce que le scan a lu
/// dans le fichier). `resolu` est celle du `ResolvedStream` — pour une piste
/// locale c'est la resolution de SORTIE, et `resolve_local_track` la fabrique
/// quand la ligne se tait (`unwrap_or(44100)` / `unwrap_or(16)`, puis
/// `cap_output_bit_depth`). La substituer affiche un chiffre que personne n'a
/// mesure.
///
/// Le streaming, lui, n'a pas de ligne en bibliotheque : `resolu` y EST la
/// resolution de la source, et reste le repli legitime.
pub(crate) fn resolution_annoncee(
    ligne: Option<u32>,
    resolu: Option<u32>,
    source_locale: bool,
) -> Option<u32> {
    if source_locale {
        // La ligne, ou rien. Jamais un chiffre fabrique par la sortie.
        ligne
    } else {
        ligne.or(resolu)
    }
}

/// Faut-il emballer ce DSD en DoP (trames PCM 24 bits au seizième du débit) ?
///
/// - **Sortie locale** : « natif » et « dop » y mènent tous deux, une carte son
///   ne recevant pas de DSD autrement.
/// - **Renderer réseau** : uniquement sur choix EXPLICITE « dop ». En « auto »
///   ou « natif », c'est `should_dsd_passthrough` qui tranche entre l'envoi du
///   fichier tel quel et le transcodage.
///
/// Règle extraite en fonction libre parce que son absence côté réseau était
/// invisible : `"dop"` n'était comparé qu'à un seul endroit du dépôt, sous un
/// garde `is_local_output` (#1772).
/// Cadence et canaux à ANNONCER pour un flux DoP, le fichier faisant foi.
///
/// L'en-tête WAV et la charge utile doivent décrire la même chose. L'encodeur
/// (`decode_dsd_to_dop_streaming`) se construit sur ce que rend
/// `parse_dsf`/`parse_dff` ; annoncer la ligne `tracks` à la place revient à
/// parier que la base et le fichier concordent. Quand ils divergent d'un canal,
/// chaque mot de 24 bits est décalé, le marqueur DoP ne tombe plus sur l'octet
/// de poids fort, et le DAC joue le train DSD comme du PCM : du bruit blanc.
///
/// La base ne sert que de repli, pour un en-tête illisible — mieux vaut
/// diffuser avec des valeurs approximatives que refuser de lire.
fn dop_wire_params(
    probe: Option<(u32, u32)>,
    db_rate: Option<u32>,
    db_channels: u32,
) -> (u32, u16) {
    let rate = probe
        .map(|(sr, _)| sr)
        .unwrap_or_else(|| db_rate.unwrap_or(2_822_400));
    let channels = probe
        .map(|(_, ch)| ch as u16)
        .unwrap_or(db_channels as u16)
        .max(2);
    (rate, channels)
}

/// Le conteneur peut-il porter plus de 16 bits alors que la base l'ignore ?
///
/// Seul l'ALAC est concerné : sa profondeur ne se lit ni dans les tags ni par
/// `lofty`, mais dans le cookie magique du fichier. Tous les autres conteneurs
/// renseignent la leur au scan, donc les sonder serait de l'E/S pour rien.
fn conteneur_a_profondeur_cachee(fmt: Option<AudioFormat>) -> bool {
    matches!(fmt, Some(AudioFormat::Alac))
}

/// Profondeur réelle lue DANS le fichier, quand la base ne la connaît pas.
///
/// Rend `None` — donc « garder ce que dit la base » — dès que le conteneur
/// n'est pas concerné, que le fichier est illisible, ou que la sonde ne trouve
/// rien. Ne jamais échouer la lecture pour une profondeur : au pire on reste
/// sur le comportement d'avant.
fn profondeur_sondee_si_la_base_ignore(file_path: &str, fmt: Option<AudioFormat>) -> Option<u16> {
    if !conteneur_a_profondeur_cachee(fmt) {
        return None;
    }
    let (_, bd) = crate::metadata::probe_m4a_props(std::path::Path::new(file_path))?;
    let bd = bd?;
    info!(
        path = %file_path,
        bit_depth = bd,
        "alac_bit_depth_probed_from_file_for_wav24"
    );
    Some(bd)
}

pub(crate) fn dop_requested(is_local: bool, is_network: bool, dsd_mode: &str) -> bool {
    (is_local && (dsd_mode == "native" || dsd_mode == "dop")) || (is_network && dsd_mode == "dop")
}

/// Cette piste est-elle du 1 bit (DSF/DFF) ? Le format vient de la base, tel
/// que le scan l'a écrit.
pub fn est_source_dsd(format: Option<&str>) -> bool {
    format.is_some_and(|f| matches!(f.to_ascii_lowercase().as_str(), "dsf" | "dff" | "dsd"))
}

/// Le fichier à décoder pour ré-alimenter les VU-mètres après une avance
/// gapless — `None` quand il n'y a rien à mesurer.
///
/// Le DSD en était exclu tout court (`file_path.filter(|_| !is_dsd)`), pour un
/// motif qui n'existe plus : `decode_to_pcm_streaming_with_levels` décode
/// DSF/DFF **en flux** depuis #1423, exactement comme le transcode de la
/// première lecture. L'exclusion laissait donc la zone SANS forwarder après
/// chaque enchaînement — `bump_levels_gen` venait de tuer le précédent — et
/// les aiguilles gelaient sur leur dernière valeur pour tout le reste de
/// l'album, alors que le FLAC de la même zone continuait de les animer
/// (#1541, Smart DX1 en `dsd_mode: pcm`).
///
/// Ce que le DSD garde en propre, c'est le PRIX : rendre du 1 bit en PCM coûte
/// cher, et sur le seul chemin qui ne mesure rien — OAAT en DSD natif, cf.
/// [`PlaybackOrchestrator::output_produces_levels`] — ce serait précisément le
/// décodage retiré pour débloquer Zicmu (`dsd_streaming_send_timeout`, #2280).
/// On ne le paie que si la sortie publie vraiment des niveaux ; les autres
/// formats gardent leur comportement, sans nouvelle condition.
pub(crate) fn fichier_a_mesurer_apres_avance(
    format: Option<&str>,
    file_path: Option<String>,
    la_sortie_mesure: bool,
) -> Option<String> {
    file_path.filter(|_| !est_source_dsd(format) || la_sortie_mesure)
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

/// Les encodages que Bandcamp nomme dans le CHEMIN d'une URL de flux.
///
/// Liste fermée et non heuristique : un segment de chemin Bandcamp est le plus
/// souvent un hachage hexadécimal, et prendre n'importe quel segment pour un
/// encodage inventerait une qualité.
const BC_KNOWN_ENCODINGS: &[&str] = &[
    "mp3-128",
    "mp3-320",
    "mp3-v0",
    "flac",
    "alac",
    "aac-hi",
    "aiff-lossless",
    "vorbis",
    "wav",
];

/// L'encodage nommé par une URL Bandcamp, en minuscules.
///
/// Bandcamp écrit le nom de l'encodage dans l'URL elle-même, sous deux formes :
///
/// * segment de chemin — `https://t4.bcbits.com/stream/<hash>/mp3-128/<id>?…` ;
/// * paramètre de requête — `https://bandcamp.com/stream_redirect?enc=mp3-128&…`.
///
/// Un fichier d'album **acheté** emprunte la seconde forme avec une autre
/// valeur (`enc=flac`, `enc=mp3-320`, `enc=alac`…). C'est toute la raison
/// d'être de cette fonction : la règle écrite dans le plugin — « un flux à
/// 128 kbit/s doit être annoncé comme tel partout où il apparaît »,
/// `plugins/tune-bandcamp/src/lib.rs` — porte sur la QUALITÉ du flux, jamais
/// sur le nom du service. Décider depuis `source == "bandcamp"` collerait
/// « 128 kbit/s » sur un FLAC acheté (#2074).
///
/// Rend `None` quand l'URL ne nomme aucun encodage connu : on n'annonce alors
/// rien plutôt que de deviner un chiffre.
pub fn bandcamp_encoding(url: &str) -> Option<String> {
    let lower = url.to_lowercase();
    let (path, query) = lower.split_once('?').unwrap_or((lower.as_str(), ""));
    if let Some(enc) = query
        .split('&')
        .find_map(|p| p.strip_prefix("enc="))
        .filter(|v| !v.is_empty())
    {
        return Some(enc.to_string());
    }
    path.split('/')
        .find(|seg| BC_KNOWN_ENCODINGS.contains(seg))
        .map(|seg| seg.to_string())
}

/// Ce qu'un encodage Bandcamp vaut réellement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BandcampQuality {
    /// Codec, sous la forme d'extension que lit le chemin du signal.
    pub codec: &'static str,
    /// Type MIME à annoncer à la zone.
    pub mime_type: &'static str,
    /// Débit CONSTANT en kbit/s. `None` pour un débit variable ou un encodage
    /// sans perte : un débit inventé serait pire que pas de débit du tout.
    pub bitrate_kbps: Option<u32>,
}

/// Traduire un encodage Bandcamp en codec, MIME et débit annonçables.
pub fn bandcamp_quality(enc: &str) -> Option<BandcampQuality> {
    let q = |codec, mime_type, bitrate_kbps| {
        Some(BandcampQuality {
            codec,
            mime_type,
            bitrate_kbps,
        })
    };
    match enc {
        "flac" => q("flac", "audio/flac", None),
        "alac" => q("alac", "audio/mp4", None),
        "aiff-lossless" => q("aiff", "audio/aiff", None),
        "wav" => q("wav", "audio/wav", None),
        "vorbis" => q("ogg", "audio/ogg", None),
        "aac-hi" => q("aac", "audio/mp4", None),
        // Débit variable : le nom ne porte aucun chiffre, et en inventer un
        // serait exactement le mensonge que ce correctif supprime.
        "mp3-v0" => q("mp3", "audio/mpeg", None),
        // `mp3-128` (écoute libre) et `mp3-320` (achat) partagent la même
        // forme : on lit le nombre plutôt que d'énumérer, sinon un futur
        // `mp3-256` retomberait silencieusement sans débit.
        other => other
            .strip_prefix("mp3-")
            .and_then(|n| n.parse::<u32>().ok())
            .and_then(|kbps| q("mp3", "audio/mpeg", Some(kbps))),
    }
}

fn guess_mime_from_url(url: &str) -> &'static str {
    let lower = url.to_lowercase();
    let path = lower.split('?').next().unwrap_or(&lower);
    if path.ends_with(".mp3") {
        "audio/mpeg"
    } else if path.ends_with(".m4a") || path.ends_with(".aac") || path.ends_with(".mp4") {
        "audio/mp4"
    } else if path.ends_with(".ogg") || path.ends_with(".opus") {
        "audio/ogg"
    } else if path.ends_with(".flac") || path.ends_with(".flc") {
        // ".flc" is the extension Lyrion/LMS uses for FLAC in its stream URLs
        // (…/music/<id>/download.flc); it fell through to the "audio/mpeg"
        // default, so the DLNA renderer got FLAC bytes labelled as MP3.
        "audio/flac"
    } else if path.ends_with(".wav") {
        "audio/wav"
    } else if path.ends_with(".aif") || path.ends_with(".aiff") {
        "audio/aiff"
    } else {
        "audio/mpeg"
    }
}

/// Decode an infinite radio HTTP stream to PCM and send chunks through the
/// session channel.  Runs on a blocking thread (called via spawn_blocking).
///
/// Whether a prefetch buffer is too short to stand in for the whole track.
///
/// An UNKNOWN duration (`duration_ms == 0`) is treated as truncated. The 30s
/// prefetch mode buffers only the head of the track, and `duration_ms` is not
/// always populated for streaming queue items (Qobuz) — when it is 0 the old
/// `duration_ms > 0 && …` guard evaluated false and the partial buffer was
/// served to a DLNA renderer anyway. The renderer then stalls at the buffer's
/// end (Patricia Barber / Qobuz on an Eversolo DMP-A8: `bytes_sent=0`,
/// `peak_pos=30000`, zone force-stopped). Callers only consult this for network
/// outputs, so local gapless (which serves the buffer) is unaffected.
fn prefetch_buffer_truncated(buffered_ms: u64, duration_ms: u64) -> bool {
    duration_ms == 0 || buffered_ms + 2000 < duration_ms
}

/// Uses symphonia with `ReadOnlySource` to handle the non-seekable HTTP stream.
/// Decodes packets progressively and converts to interleaved 16-bit PCM bytes.
/// The loop runs until the stream ends, the sender is dropped (stop), or an
/// unrecoverable error occurs.
/// Choose a renderer-safe WAV output sample rate for a decoded radio stream.
///
/// Most DLNA/UPnP renderers only lock onto 44.1/48 kHz (and higher standard
/// multiples). HE-AAC / aacPlus streams (Radio Morow: `morow_hi.aacp`) decode
/// at the AAC-LC core rate — typically 22050 Hz — because symphonia does not
/// apply the SBR extension that would double the rate to 44100. A 22050 Hz WAV
/// is reported as PLAYING yet emitted as SILENCE by many renderers (Yves,
/// LHC-60). We upsample any sub-44.1 kHz stream to 44100 Hz so sound is
/// guaranteed. Streams already at 44.1 kHz or above pass through unchanged, so
/// stations that already work incur no extra CPU and no quality change.
pub(crate) fn renderer_safe_wav_rate(source_rate: u32) -> u32 {
    if source_rate < 44_100 {
        44_100
    } else {
        source_rate
    }
}

/// Dire à l'auditeur pourquoi une station n'a pas joué.
///
/// Le décodage d'un flux radio tourne dans une tâche détachée : jusqu'ici son
/// échec ne laissait qu'un `warn!` dans les journaux. Côté interface la lecture
/// partait, la zone affichait la station, et il ne sortait rien — impossible de
/// distinguer « la station est morte » de « Tune est cassé » (issue #1960).
///
/// `fatal: true` est indispensable et non décoratif : le client web étouffe un
/// `zone.playback_error` reçu dans la fenêtre de grâce qui suit un ordre de
/// lecture (elle couvre les pré-transcodages HI-RES lents, #1146), SAUF s'il
/// est marqué fatal. Or une station morte échoue en moins d'une seconde,
/// c'est-à-dire en plein dans cette fenêtre : sans ce drapeau le message
/// afficherait « chargement… » puis plus rien du tout.
fn emit_radio_playback_error(
    bus: &Option<Arc<EventBus>>,
    zone_id: i64,
    station: &str,
    error: &str,
) {
    let Some(bus) = bus else { return };
    // Le flux répond, mais ce n'est pas de l'audio : on dit ce qui est arrivé
    // en clair plutôt que de recopier une erreur de décodeur.
    let message = if error.starts_with(RADIO_NOT_AUDIO) {
        format!(
            "« {station} » n'émet plus d'audio : le serveur renvoie une page web à la place du flux. La station a probablement changé d'adresse."
        )
    } else if error.starts_with(RADIO_HLS_UNSUPPORTED) {
        // Nommer HLS, et dire quoi faire. Avant #2307 l'auditeur recevait au
        // mieux le « radio probe failed: … » de symphonia — le nom d'un
        // sous-système qu'il n'a aucune raison de connaître, sur un défaut
        // qu'il ne peut pas corriger. Ici il apprend que sa station est bien
        // vivante, que c'est Tune qui ne sait pas la lire, et quoi demander.
        format!(
            "« {station} » est diffusée en HLS (manifeste .m3u8) : Tune ne sait pas encore lire ce format de flux. Demandez à la station son adresse de flux directe (MP3 ou AAC), ou choisissez une autre station."
        )
    } else {
        format!("Impossible de lire la station « {station} » : {error}")
    };
    bus.emit(
        "zone.playback_error",
        serde_json::json!({
            "zone_id": zone_id,
            "error": message,
            "fatal": true,
        }),
    );
}

/// Préfixe des erreurs « le flux annoncé audio n'en est pas ». Il permet à
/// l'appelant de distinguer ce cas d'une panne réseau : une station remplacée
/// par une page web ne guérira pas en réessayant.
pub(crate) const RADIO_NOT_AUDIO: &str = "radio_not_audio";

/// Le `Content-Type` d'un flux radio dit-il, sans ambiguïté, que ce n'est PAS
/// de l'audio ?
///
/// Le cas qui motive ce contrôle (issue #1960) : BBC Radio 3 a retiré son flux,
/// `stream.live.vc.bbcmedia.co.uk/bbc_radio_three` redirige vers
/// `www.bbc.co.uk` et répond **200 OK** en `text/html`. Rien n'échoue — le
/// décodeur reçoit du HTML, ne trouve pas de piste audio, et l'auditeur n'a que
/// du silence sans le moindre message. Un 404 se voit ; un 200 en HTML, non.
///
/// Volontairement une LISTE NOIRE, pas une liste blanche : les serveurs
/// Icecast/Shoutcast annoncent tout et n'importe quoi (`application/octet-stream`,
/// `application/ogg`, `audio/aacp`, parfois rien du tout), et refuser un flux
/// sur un type inconnu ferait taire des stations qui marchent. On ne rejette
/// donc que ce qui ne peut en aucun cas être un flux audio.
///
/// Renvoie `Some(étiquette)` — le type normalisé, à afficher — quand le flux
/// n'est pas de l'audio ; `None` dans tous les autres cas, y compris un
/// en-tête absent ou illisible.
pub(crate) fn non_audio_content_type(content_type: &str) -> Option<String> {
    // `text/html; charset=UTF-8` → `text/html`
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if ct.is_empty() {
        return None;
    }
    const NEVER_AUDIO: [&str; 5] = [
        "text/html",
        "application/xhtml+xml",
        "text/css",
        "application/json",
        "image/",
    ];
    // Une entrée terminée par `/` est un préfixe de famille (`image/`), les
    // autres sont des types exacts.
    NEVER_AUDIO
        .iter()
        .any(|bad| {
            if bad.ends_with('/') {
                ct.starts_with(bad)
            } else {
                ct == *bad
            }
        })
        .then_some(ct)
}

/// Préfixe des erreurs « cette station est diffusée en HLS » (#2307).
///
/// Distinct de [`RADIO_NOT_AUDIO`] parce que le remède n'est pas le même : une
/// station en `text/html` est morte ou a changé d'adresse, une station en HLS
/// est bien vivante et c'est Tune qui ne sait pas la lire. Confondre les deux
/// enverrait l'auditeur chercher une adresse de remplacement qui n'existe pas.
pub(crate) const RADIO_HLS_UNSUPPORTED: &str = "radio_hls_unsupported";

/// Cette station est-elle publiée en HLS ?
///
/// HLS n'est pas un format de conteneur qu'il suffirait d'ajouter au décodeur :
/// c'est un PROTOCOLE. Un `.m3u8` est un manifeste qui liste des segments à
/// télécharger l'un après l'autre, et qu'il faut re-télécharger périodiquement
/// tant que le direct dure. `decode_radio_stream_to_pcm` fait un GET, un seul,
/// et pousse le corps dans symphonia. Tune ne sait donc pas lire HLS ; dire
/// lequel, et le dire à l'auditeur, est tout ce que ce contrôle sert à faire.
///
/// Deux signaux, tous deux SANS AMBIGUÏTÉ — c'est délibérément étroit, parce
/// qu'un faux positif rendrait muette une station qui marche aujourd'hui :
///
///   * l'extension `.m3u8` du chemin, paramètres et ancre retirés ;
///   * le type MIME `application/vnd.apple.mpegurl`, le type ENREGISTRÉ de HLS
///     (RFC 8216) — le seul cas où une URL sans extension se dénonce.
///
/// Volontairement ABSENTS : `audio/x-mpegurl`, `audio/mpegurl` et
/// `application/x-mpegurl`, que les serveurs servent aussi pour une simple
/// playlist `.m3u`. Une `.m3u` est déréférencée en amont par
/// `resolve_playlist_url` ; quand ce déréférencement échoue (réseau), le
/// décodeur la reçoit telle quelle — et l'annoncer « HLS » serait un
/// diagnostic FAUX sur le chemin le plus fréquenté. Mieux vaut se taire sur
/// ces types-là que mentir.
///
/// Ce contrôle vit à part de [`non_audio_content_type`] et ne le modifie pas :
/// cette liste noire répond à une autre question (« le serveur a-t-il rendu une
/// page web ? ») et son témoin exige justement que les types `mpegurl` la
/// traversent. Les deux gardes sont indépendantes.
///
/// `content_type` peut être vide : c'est le mode « avant le réseau », où seule
/// l'extension parle.
pub(crate) fn is_hls_manifest(url: &str, content_type: &str) -> bool {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    if path.ends_with(".m3u8") {
        return true;
    }
    content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("application/vnd.apple.mpegurl")
}

/// Applique l'EQ au PCM radio déjà décodé, avant sa quantification en i16.
/// `None` est une identité stricte : les chemins sans EQ conservent exactement
/// les mêmes échantillons et ne paient aucun traitement supplémentaire.
fn apply_radio_eq(eq: &mut Option<crate::audio::eq::EqProcessor>, interleaved: &mut [f32]) {
    if let Some(eq) = eq.as_mut() {
        eq.process_interleaved(interleaved);
    }
}

fn decode_radio_stream_to_pcm(
    url: String,
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    data_ready: std::sync::Arc<tokio::sync::Notify>,
    session: std::sync::Arc<crate::http::streamer::StreamSession>,
    // Le profil voyage sans coefficients : le taux/canaux réels ne sont connus
    // qu'après la sonde Symphonia. `None` garde le PCM historique à l'identique
    // (notamment la sortie locale, qui applique déjà son propre EQ).
    eq_profile: Option<crate::audio::eq::EqProfile>,
    // Pur observateur : les VU-mètres. Un flux radio est décodé live (pas de
    // fichier), donc le décodage-pour-niveaux des pistes locales ne s'applique
    // pas — on tappe ici le PCM déjà décodé. `None` = pas de bus (tests).
    levels_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::audio::tap::RawWindow>>,
) -> Result<(), String> {
    use symphonia::core::audio::conv::IntoSample;
    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::codecs::audio::AudioDecoderOptions;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::formats::{FormatOptions, TrackType};
    use symphonia::core::io::{MediaSourceStream, ReadOnlySource};
    use symphonia::core::meta::MetadataOptions;
    use tracing::{debug, info, warn};

    // HLS s'arrête ici, avant le moindre octet de réseau (#2307). Ce
    // décodeur fait un GET unique ; il n'a aucun chargeur de segments, aucun
    // rafraîchissement de playlist, rien de ce qu'un direct HLS exige. Sans
    // cette porte le manifeste partait quand même dans symphonia avec
    // l'indice « mp3 » (le repli du `hint` plus bas, `.m3u8` ne correspondant
    // à aucune branche), et l'auditeur récoltait au mieux un « radio probe
    // failed: ... » illisible, au pire du silence si le probe accrochait une
    // fausse synchro dans le texte du manifeste. On refuse, et on le DIT.
    if is_hls_manifest(&url, "") {
        return Err(format!(
            "{RADIO_HLS_UNSUPPORTED}: {url} est un manifeste HLS, pas un flux audio décodable"
        ));
    }
    let rt =
        tokio::runtime::Handle::try_current().map_err(|_| "no tokio runtime for radio decode")?;

    let mut first_chunk_sent = false;
    let mut pcm_buf: Vec<u8> = Vec::with_capacity(65536);
    let chunk_size: usize = 32768;

    // Radio streams from Radio France (FIP, etc.) periodically drop the upstream
    // HTTP body (`request or response body error`) — Xavier's ~1h30 cutoffs.
    // The old code ended the decode on such an error, tearing down the WAV
    // session and relying on the poller auto-retry (~1min40 of silence). Instead
    // we reconnect the upstream in place and keep feeding the SAME session, so
    // the renderer never stops (a sub-second gap at worst). We give up only after
    // MAX_RECONNECTS so a permanently-dead station still falls back to the poller.
    const MAX_RECONNECTS: u32 = 30;
    let mut reconnects: u32 = 0;
    // When the upstream last dropped, so we can measure how long the renderer
    // was starved during a reconnect (diagnostics: FIP silent-after-reconnect).
    let mut dropped_at: Option<std::time::Instant> = None;
    // Format of the first successful connection. A reconnect that returns a
    // different rate/channel layout would feed PCM that doesn't match the WAV
    // header already sent to the renderer, so we bail to a fresh session instead.
    let mut expected_format: Option<(u16, u32)> = None;
    // Construit une seule fois au format réellement détecté, puis conservé à
    // travers les reconnexions compatibles : réinitialiser les biquads à chaque
    // coupure amont créerait un transitoire audible (#2063).
    let mut radio_eq: Option<crate::audio::eq::EqProcessor> = None;

    'reconnect: loop {
        // ---- Connect + probe + build decoder ----
        let setup = (|| -> Result<
            (
                Box<dyn symphonia::core::formats::FormatReader>,
                Box<dyn symphonia::core::codecs::audio::AudioDecoder>,
                u32,
                u16,
                u32,
            ),
            String,
        > {
            // No total timeout for infinite radio streams
            let response = crate::http::client::blocking_builder()
                .timeout(None)
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .and_then(|c| c.get(&url).send())
                .map_err(|e| format!("radio HTTP fetch failed: {e}"))?;
            if !response.status().is_success() {
                return Err(format!("radio HTTP error: {}", response.status()));
            }
            // Le type réellement reçu, tracé à CHAQUE connexion : c'est la
            // seule façon de savoir, la prochaine fois qu'une station meurt,
            // ce que son serveur a répondu (issue #1960).
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            // Une station peut disparaître en répondant 200 : la BBC redirige
            // son ancien flux vers sa page d'accueil. Sans ce contrôle, le
            // décodeur avale du HTML, échoue plus loin sur un message obscur
            // (« no audio track found ») et l'auditeur n'a que du silence.
            if let Some(bad) = non_audio_content_type(&content_type) {
                return Err(format!(
                    "{RADIO_NOT_AUDIO}: le serveur a répondu « {bad} » au lieu d'un flux audio"
                ));
            }
            // Un manifeste HLS servi depuis une URL sans extension : seul le
            // type MIME le dénonce. Même refus nommé que la porte d'entrée.
            if is_hls_manifest(&url, &content_type) {
                return Err(format!(
                    "{RADIO_HLS_UNSUPPORTED}: le serveur a répondu « {content_type} », un manifeste HLS et non un flux audio"
                ));
            }
            info!(url = %url, content_type = %content_type, "radio_local_decode_stream_connected");

            let source = ReadOnlySource::new(response);
            let mss = MediaSourceStream::new(Box::new(source), Default::default());

            let mut hint = Hint::new();
            let lower = url.to_lowercase();
            let path_part = lower.split('?').next().unwrap_or(&lower);
            if path_part.ends_with(".mp3") {
                hint.with_extension("mp3");
            } else if path_part.ends_with(".aac") || path_part.ends_with(".m4a") {
                hint.with_extension("aac");
            } else if path_part.ends_with(".ogg") {
                hint.with_extension("ogg");
            } else if path_part.ends_with(".flac") {
                hint.with_extension("flac");
            } else {
                hint.with_extension("mp3");
            }

            let format: Box<dyn symphonia::core::formats::FormatReader> =
                symphonia::default::get_probe()
                    .probe(
                        &hint,
                        mss,
                        FormatOptions::default(),
                        MetadataOptions::default(),
                    )
                    .map_err(|e| format!("radio probe failed: {e}"))?;

            // Extract track metadata in a scope so the borrow of `format` ends
            // before we move it into the return tuple.
            let (track_id, audio_params) = {
                let track = format
                    .default_track(TrackType::Audio)
                    .ok_or("radio stream: no audio track found")?;
                let params = match &track.codec_params {
                    Some(CodecParameters::Audio(params)) => params.clone(),
                    _ => return Err("radio stream: no audio codec parameters".into()),
                };
                (track.id, params)
            };
            let source_channels = audio_params
                .channels
                .as_ref()
                .map(|c| c.count() as u16)
                .unwrap_or(2);
            let source_sample_rate = audio_params.sample_rate.unwrap_or(44100);

            let decoder = symphonia::default::get_codecs()
                .make_audio_decoder(&audio_params, &AudioDecoderOptions::default())
                .map_err(|e| format!("radio decoder init failed: {e}"))?;

            Ok((
                format,
                decoder,
                track_id,
                source_channels,
                source_sample_rate,
            ))
        })();

        let (mut format, mut decoder, track_id, source_channels, source_sample_rate) = match setup {
            Ok(v) => v,
            Err(e) => {
                if reconnects == 0 {
                    // Initial connection failed — fail fast (bad URL, etc.)
                    return Err(e);
                }
                // Une station remplacée par une page web ne redeviendra pas un
                // flux audio en réessayant trente fois : on remonte l'erreur
                // tout de suite pour qu'elle soit DITE, au lieu de quinze
                // secondes de silence suivies d'un abandon muet.
                // Ni un manifeste HLS, que trente reconnexions ne
                // transformeront pas davantage en flux Icecast (#2307).
                if e.starts_with(RADIO_NOT_AUDIO) || e.starts_with(RADIO_HLS_UNSUPPORTED) {
                    return Err(e);
                }
                reconnects += 1;
                if reconnects > MAX_RECONNECTS {
                    warn!(url = %url, error = %e, "radio_reconnect_giving_up");
                    return Ok(());
                }
                warn!(url = %url, error = %e, attempt = reconnects, "radio_reconnect_setup_failed_retrying");
                std::thread::sleep(std::time::Duration::from_millis(500));
                continue 'reconnect;
            }
        };

        // Guard against a reconnect changing the audio format underneath the
        // WAV header already advertised to the renderer.
        match expected_format {
            None => expected_format = Some((source_channels, source_sample_rate)),
            Some((ch, sr)) if (ch, sr) != (source_channels, source_sample_rate) => {
                warn!(
                    url = %url,
                    expected_ch = ch, expected_sr = sr,
                    got_ch = source_channels, got_sr = source_sample_rate,
                    "radio_reconnect_format_changed_bailing"
                );
                return Ok(());
            }
            _ => {}
        }

        // Renderer-safe output rate: HE-AAC/aacPlus decodes at its AAC-LC core
        // rate (e.g. 22050 Hz) which many DLNA renderers reject as silence. We
        // upsample sub-44.1 kHz streams to 44100 Hz; 44.1/48 kHz+ pass through.
        let output_sample_rate = renderer_safe_wav_rate(source_sample_rate);
        let needs_resample = output_sample_rate != source_sample_rate;
        if radio_eq.is_none() {
            radio_eq = eq_profile.as_ref().and_then(|profile| {
                let eq = crate::audio::eq::EqProcessor::new(
                    profile,
                    output_sample_rate,
                    source_channels,
                );
                if eq.is_enabled() { Some(eq) } else { None }
            });
        }

        // Publish the OUTPUT format so the HTTP handler advertises the WAV rate
        // that matches the PCM we actually feed (FIP is 48000 → advertised as
        // is; Morow HE-AAC is 22050 → advertised as the resampled 44100). Set
        // BEFORE first_chunk so the header, emitted after data_ready, is right.
        session.publish_detected_output_format(output_sample_rate, source_channels);

        // Measure the reconnect gap: how long the session went without fresh
        // PCM. A long gap can starve the renderer's HTTP read.
        let gap_ms = dropped_at.take().map(|t| t.elapsed().as_millis());
        info!(
            channels = source_channels,
            sample_rate = source_sample_rate,
            output_sample_rate = output_sample_rate,
            resampled = needs_resample,
            reconnect = reconnects,
            gap_ms = ?gap_ms,
            "radio_local_decode_started"
        );
        if let Some(g) = gap_ms {
            if g > 2000 {
                warn!(
                    gap_ms = g,
                    reconnect = reconnects,
                    "radio_reconnect_gap_long — renderer may have been starved"
                );
            }
        }

        // When this connection started streaming. A healthy station streams for
        // minutes between periodic upstream drops; only a permanently-dead
        // station fails in rapid succession. Used below to reset the reconnect
        // counter after a good stretch (see the drop handler).
        let connected_at = std::time::Instant::now();

        // ---- Decode loop ----
        loop {
            if tx.is_closed() {
                debug!("radio_local_decode_channel_closed_before_packet");
                return Ok(());
            }
            let packet = match format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => {
                    debug!("radio_local_decode_stream_ended_upstream");
                    break; // upstream ended — reconnect
                }
                Err(symphonia::core::errors::Error::IoError(ref e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    debug!("radio_local_decode_eof");
                    break; // upstream dropped — reconnect
                }
                Err(e) => {
                    // FIP-style upstream body error — reconnect in place.
                    warn!(error = %e, "radio_local_decode_packet_error");
                    break;
                }
            };

            if packet.track_id != track_id {
                continue;
            }

            let decoded = match decoder.decode(&packet) {
                Ok(d) => d,
                Err(e) => {
                    debug!(error = %e, "radio_local_decode_frame_skip");
                    continue;
                }
            };

            // Convert decoded audio buffer to interleaved 16-bit PCM bytes
            let channels = decoded.spec().channels().count();
            let frames = decoded.frames();

            let mut interleaved: Vec<f32> = Vec::with_capacity(frames * channels);
            decoded.copy_to_vec_interleaved::<f32>(&mut interleaved);

            // Upsample low-rate (HE-AAC 22050) PCM to the renderer-safe rate
            // before packing to i16, so the bytes match the advertised WAV
            // header. No-op (single move) when the stream is already 44.1/48.
            if needs_resample {
                interleaved = crate::audio::simple_resample(
                    &interleaved,
                    source_sample_rate,
                    output_sample_rate,
                    channels as u16,
                );
            }

            // Le WAV servi à OAAT/DLNA/navigateur doit porter le son promis par
            // le profil de zone. Le traitement se fait en f32 avant i16, comme
            // les autres chemins DSP, et les VU observent ainsi le signal final.
            apply_radio_eq(&mut radio_eq, &mut interleaved);

            let mut packet_buf: Vec<u8> = Vec::with_capacity(interleaved.len() * 2);
            for sample in &interleaved {
                let s16: i16 = (*sample).into_sample();
                packet_buf.extend_from_slice(&s16.to_le_bytes());
            }

            pcm_buf.extend_from_slice(&packet_buf);

            while pcm_buf.len() >= chunk_size {
                let chunk: Vec<u8> = pcm_buf.drain(..chunk_size).collect();
                // VU-mètres : tappe le PCM 16-bit avant de le servir (canal
                // séparé, non bloquant — n'affecte pas le flux du renderer).
                if let Some(ref ltx) = levels_tx {
                    crate::audio::tap::send_windowed_pcm(
                        ltx,
                        &chunk,
                        16,
                        channels as u16,
                        output_sample_rate,
                    );
                }
                if rt.block_on(tx.send(chunk)).is_err() {
                    debug!("radio_local_decode_consumer_dropped");
                    return Ok(());
                }
                if !first_chunk_sent {
                    first_chunk_sent = true;
                    data_ready.notify_one();
                }
            }
        }

        // Inner loop broke because the upstream stream dropped (not tx closed).
        // Reconnect and keep feeding the SAME session (pcm_buf carries over).
        if tx.is_closed() {
            return Ok(());
        }
        // MAX_RECONNECTS guards against a *permanently dead* station (rapid
        // back-to-back failures) — not against a healthy station's periodic
        // upstream drops. FIP-style streams drop the body roughly every ~6 min,
        // so a cumulative counter hit 30 at ~3h and cut a good listen (Xavier
        // #1212, a regression of #382). Reset the counter after any sustained
        // good stretch so a normal long listen is never capped, while a dead
        // station (each connection dies in <60s) still burns through
        // MAX_RECONNECTS in seconds and correctly falls back to the poller.
        if connected_at.elapsed() >= std::time::Duration::from_secs(60) {
            reconnects = 0;
        }
        reconnects += 1;
        if reconnects > MAX_RECONNECTS {
            warn!(url = %url, reconnects, "radio_reconnect_giving_up");
            return Ok(());
        }
        dropped_at = Some(std::time::Instant::now());
        info!(url = %url, attempt = reconnects, "radio_upstream_dropped_reconnecting");
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

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
