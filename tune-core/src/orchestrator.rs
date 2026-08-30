use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use tokio::sync::Mutex;
use tracing::{debug, info, warn};

// Repli NFC/NFD partagé (#1865) — voir `crate::library::local_path`.
use crate::library::local_path::resolve_existing_local_path;

/// Error marker returned by `resolve_local_track` when a play was superseded by
/// a newer tap before its transcode started; `play_inner` maps it to a quiet
/// no-op result instead of a user-facing error.
const SUPERSEDED_BEFORE_TRANSCODE: &str = "__superseded_before_transcode__";

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

/// Serializes ALAC/PCM→FLAC transcodes of the *same* source file across
/// concurrent plays, keyed by source path. A burst of play taps for a
/// slow-to-decode NAS track otherwise kicks off one full transcode each
/// (Yves: 6 concurrent transcodes of a single file in 20s → overlapping FLAC
/// streams to the DLNA renderer = noise). The winner transcodes; every play a
/// newer tap has already superseded skips it entirely.
static TRANSCODE_GATE: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Decode `source` to PCM, reduce its bit depth to `target_bd` when the source
/// is deeper, apply the zone `eq` if any, encode to `target_fmt`, and write the
/// result to `dest`. Returns `(encoded_size, pcm_bytes, actual_bit_depth)`.
///
/// Extracted from `play()` so the on-demand transcode and the background cache
/// warm-up (`spawn_warm_next_local`) produce byte-identical output — a warm-up
/// that diverged would populate a cache entry the real play never hits.
/// Combien de temps laisser au transcodage d'un fichier vers PCM.
///
/// Une constante ne peut pas convenir : le travail est proportionnel au volume
/// de données à décoder, et l'écart entre un FLAC et un DSD256 est d'un ordre
/// de grandeur. Un budget fixe de 120 s rendait donc injouables les fichiers
/// les plus lourds — la lecture ne démarrait simplement jamais (#1330).
///
/// Plancher de 120 s (le comportement historique, qui convient à tout ce qui
/// est léger), plus 120 s par gibioctet de source, plafonné à 30 minutes pour
/// qu'un disque en perdition finisse malgré tout par rendre la main.
///
/// Taille illisible (fichier distant qui a disparu, permissions) : on retombe
/// sur le plancher plutôt que d'accorder un budget arbitraire.
fn transcode_budget_for(path: &str) -> std::time::Duration {
    const FLOOR_S: u64 = 120;
    const PER_GIB_S: u64 = 120;
    const CEILING_S: u64 = 30 * 60;
    let gib = std::fs::metadata(path)
        .map(|m| m.len() as f64 / (1024.0 * 1024.0 * 1024.0))
        .unwrap_or(0.0);
    let extra = (gib * PER_GIB_S as f64).round() as u64;
    std::time::Duration::from_secs((FLOOR_S + extra).min(CEILING_S))
}

async fn transcode_source_to_file(
    source: String,
    out_sr: u32,
    channels: u16,
    target_bd: u16,
    target_fmt: String,
    eq: Option<crate::audio::eq::EqProcessor>,
    convolver: Option<crate::audio::convolver::Convolver>,
    replaygain: Option<f64>,
    dest: String,
) -> Result<(u64, Vec<u8>, u16), String> {
    // 1. Decode source to PCM (blocking I/O).
    let decoded = tokio::task::spawn_blocking(move || {
        crate::audio::decode::decode_to_pcm(&source, Some(out_sr), Some(channels as u32), 0.0, 0.0)
    })
    .await
    .map_err(|e| format!("decode task panic: {e}"))??;

    let mut pcm_bytes = decoded.pcm_bytes();
    let mut actual_bd = decoded.bit_depth;

    // 1a. Reduce bit depth to the negotiated target when the source is deeper
    // (e.g. 24-bit ALAC/FLAC → 16-bit LPCM for a 16-bit-only DLNA renderer).
    if target_bd < actual_bd {
        pcm_bytes = crate::audio::decode::convert_pcm_bytes(&pcm_bytes, actual_bd, target_bd);
        actual_bd = target_bd;
    }

    // 1b. Apply ReplayGain BEFORE the tone controls, where a pre-amp belongs:
    // the level normalisation is what the EQ then works on. A network renderer
    // gets an already-encoded stream, so unlike a local DAC the gain has to be
    // baked into the samples here or it never happens at all.
    if let Some(factor) = replaygain {
        crate::audio::replaygain::apply_gain_pcm(&mut pcm_bytes, actual_bd, factor);
    }

    // 1c. Apply EQ if enabled for this zone.
    if let Some(mut eq) = eq {
        eq.process_pcm(&mut pcm_bytes, actual_bd);
    }

    // 1d. Apply the room-correction FIR convolver (after EQ) if the zone has an
    // uploaded impulse response. This is what brings room correction to network
    // renderers (DLNA/UPnP/AirPlay): the local output has its own convolver, but
    // a streamed zone only gets DSP that runs here, before encoding.
    if let Some(mut conv) = convolver {
        conv.process_pcm(&mut pcm_bytes, actual_bd);
    }

    // 2. Encode to the target format.
    let mut encoder = crate::audio::encoder::AudioEncoder::new(
        &target_fmt,
        decoded.sample_rate,
        actual_bd as u32,
        decoded.channels,
    );
    encoder.start().await?;
    encoder.write(&pcm_bytes).await?;
    let encoded_data = encoder.finish().await?;

    // 3. Write to `dest` (blocking I/O).
    let file_size = encoded_data.len() as u64;
    let encoded_clone = encoded_data.clone();
    tokio::task::spawn_blocking(move || {
        std::fs::write(&dest, &encoded_clone).map_err(|e| format!("write temp file: {e}"))
    })
    .await
    .map_err(|e| format!("write task panic: {e}"))??;

    Ok((file_size, pcm_bytes, actual_bd))
}

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
            let mut reported_position_ms: i64 = 0;
            loop {
                if playback.current_play_seq(zone_id).await != play_seq
                    || gen_arc.load(std::sync::atomic::Ordering::Relaxed) != gen_at_spawn
                {
                    return;
                }
                let zone_state = playback.get_state(zone_id).await;
                reported_position_ms = zone_state.position_ms;
                if let Some(prev) = last_reported {
                    if reported_position_ms > prev {
                        reported_advancing = true;
                    }
                }
                last_reported = Some(reported_position_ms);
                match zone_state.state {
                    PlayState::Playing => {
                        started = true;
                        break;
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
            }
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
                }),
            );
            next_emit += window;
            position += window;
        }
    });
    tx
}

/// Décode un fichier local EN FLUX, uniquement pour alimenter un forwarder de
/// niveaux neuf : c'est ce qui rend les aiguilles à la piste devenue courante
/// après une avance gapless.
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
/// la piste (~600 Mo pour un DSD64 de dix minutes rendu en 176,4 kHz). Le
/// puits ne consomme donc pas plus vite que
/// [`PROXY_LEVELS_MAX_AHEAD_MS`] d'avance, et le décodeur, bloqué sur un canal
/// borné, s'aligne dessus.
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
                    let position = cadence.get_state(zone_id).await.position_ms;
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
fn est_dsd_brut(mime_type: &str) -> bool {
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
/// timeout → the renderer plays silence). Kept a pure function so the decision
/// matrix is unit-testable without an orchestrator.
fn use_file_transcode_for(
    is_network: bool,
    target_is_wav: bool,
    dlna_needs_wav: bool,
    dsd_lpcm_streams: bool,
) -> bool {
    is_network && (!target_is_wav || (dlna_needs_wav && !dsd_lpcm_streams))
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
    output_type.is_some()
        && !is_local
        && !is_oaat
        && !is_push_uri_output_type(output_type)
        && output_type != Some("browser")
        && source_format.is_some()
        && source_format != Some(AudioFormat::Dsd)
}

fn is_push_uri_output_type(output_type: Option<&str>) -> bool {
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
pub(crate) fn est_source_dsd(format: Option<&str>) -> bool {
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

    /// Crée le flux WAV éphémère demandé par un renderer qui parcourt les
    /// radios du MediaServer.
    ///
    /// La connexion à la station ne commence qu'ici, au GET audio — jamais au
    /// Browse ni au HEAD. La route HTTP possède la durée de vie de la session
    /// et la retire lorsque son corps est terminé ou abandonné.
    pub async fn create_media_server_radio_session(&self, radio_url: String) -> String {
        let wav_info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: 44100,
            bit_depth: 16,
            channels: 2,
            file_size: None,
            duration_ms: None,
            ..Default::default()
        };
        let (stream_id, tx, data_ready, session) =
            self.streamer.create_radio_session(wav_info, 256).await;
        let stream_id_for_task = stream_id.clone();
        let session_for_done = session.clone();

        info!(
            stream_id = %stream_id,
            url = %radio_url,
            "media_server_radio_decode_started"
        );
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                decode_radio_stream_to_pcm(radio_url, tx, data_ready, session, None, None)
            })
            .await;

            session_for_done
                .producer_done
                .store(true, std::sync::atomic::Ordering::Relaxed);
            // Réveille aussi le corps qui attend encore la détection du vrai
            // format. `notify_one` mémorise un permis si l'échec précède son
            // premier poll, contrairement à `notify_waiters`.
            session_for_done.data_ready.notify_one();
            // `create_radio_session` conserve un émetteur de garde pour les
            // flux permanents. Cette session est liée à une requête : quand le
            // producteur finit, le corps HTTP doit recevoir EOF.
            session_for_done.close_sender().await;

            match result {
                Ok(Ok(())) => debug!(
                    stream_id = %stream_id_for_task,
                    "media_server_radio_decode_ended"
                ),
                Ok(Err(error)) => warn!(
                    stream_id = %stream_id_for_task,
                    error = %error,
                    "media_server_radio_decode_failed"
                ),
                Err(error) => warn!(
                    stream_id = %stream_id_for_task,
                    error = %error,
                    "media_server_radio_decode_task_panicked"
                ),
            }
        });

        stream_id
    }

    /// Marque la zone en résolution gapless jusqu'au drop du garde.
    fn begin_levels_prewarm(&self, zone_id: i64) -> LevelsPrewarmScope<'_> {
        self.levels_prewarm
            .lock()
            .expect("levels_prewarm lock")
            .insert(zone_id);
        LevelsPrewarmScope {
            set: &self.levels_prewarm,
            zone_id,
        }
    }

    /// Faut-il attacher un forwarder de niveaux aux sessions de cette zone ?
    /// Non pendant une résolution gapless (pré-chargement de la piste
    /// suivante).
    fn levels_attach_allowed(&self, zone_id: i64) -> bool {
        !self
            .levels_prewarm
            .lock()
            .expect("levels_prewarm lock")
            .contains(&zone_id)
    }

    /// Forwarder de niveaux pour la zone, si elle y a droit (bus présent et
    /// pas de pré-chargement gapless en cours). Capture le `play_seq`
    /// courant : le forwarder meurt de lui-même quand la piste est remplacée.
    /// Factorise le motif répété par tous les chemins qui ont le PCM décodé
    /// en main (transcodes streaming, prefetch) — voir #1105/#1106.
    async fn levels_forwarder_if_allowed(
        &self,
        zone_id: i64,
        start_position_ms: i64,
    ) -> Option<tokio::sync::mpsc::UnboundedSender<crate::audio::tap::RawWindow>> {
        let bus = self
            .event_bus
            .clone()
            .filter(|_| self.levels_attach_allowed(zone_id))?;
        let play_seq = self.playback.current_play_seq(zone_id).await;
        Some(spawn_paced_levels_forwarder(
            bus,
            self.playback.clone(),
            zone_id,
            play_seq,
            start_position_ms,
        ))
    }

    /// VU-mètres d'une session proxy (passthrough streaming Qobuz/Tidal) :
    /// lance en tâche de fond une seconde connexion CDN décodée uniquement
    /// pour les niveaux (voir [`decode_http_stream_for_levels`]). Le flux
    /// servi au renderer n'est pas touché — bit-perfect préservé. Une tâche
    /// sœur échantillonne la position rapportée de la zone pour brider la
    /// sonde au rythme de lecture ; les deux s'arrêtent quand le forwarder
    /// disparaît (stop / piste remplacée).
    async fn spawn_proxy_levels_probe(&self, zone_id: i64, url: String, codec_hint: String) {
        let Some(bus) = self
            .event_bus
            .clone()
            .filter(|_| self.levels_attach_allowed(zone_id))
        else {
            return;
        };
        // Épinglé ICI, pas dans la tâche : c'est la piste dont on vient de
        // décider les niveaux (#1110).
        let play_seq = self.playback.current_play_seq(zone_id).await;
        spawn_proxy_levels_probe_task(
            self.playback.clone(),
            bus,
            zone_id,
            url,
            codec_hint,
            play_seq,
        );
    }

    /// Duplicate-network-play detector. Returns `true` when `(source,
    /// source_id)` was recorded as this zone's last network play within
    /// `DUPLICATE_NET_PLAY_WINDOW` of `now` (⇒ a redundant re-send to coalesce);
    /// otherwise records it as the new last play and returns `false`. Pure map
    /// logic split out of `play_inner` for unit testing.
    /// La cle doit identifier la PISTE, pas seulement sa source.
    ///
    /// `source_id` ne suffit pas : une piste de la bibliotheque locale se joue
    /// par `track_id`, et `play_from_queue` laisse alors `source` et
    /// `source_id` a `None`. La cle valait donc `("local", None)` pour TOUTES
    /// les pistes locales d'une zone, si bien que deux morceaux DIFFERENTS
    /// envoyes au meme renderer reseau a moins de douze secondes d'intervalle
    /// se ressemblaient parfaitement.
    ///
    /// Consequence pour l'utilisateur : sur Chromecast, DLNA ou AirPlay, appuyer
    /// sur « piste suivante » pendant les douze premieres secondes faisait
    /// avancer le serveur SANS rien envoyer au renderer, qui continuait le
    /// morceau precedent. Le bouton paraissait mort (FabienM, v0.9.102, zone
    /// Enfants en Chromecast : quinze `api_next_requested` d'affilee, tous
    /// suivis d'un `orchestrator_play_coalesced_duplicate_net_send` sur des
    /// titres pourtant differents).
    ///
    /// Le test d'origine n'exercait que `tidal` et `qobuz` — des sources qui
    /// portent TOUJOURS un `source_id`. Il ne pouvait pas voir le cas local.
    fn record_or_detect_duplicate_net_play(
        map: &mut HashMap<i64, (String, Option<String>, Option<i64>, std::time::Instant)>,
        zone_id: i64,
        source: &str,
        source_id: &Option<String>,
        track_id: Option<i64>,
        now: std::time::Instant,
    ) -> bool {
        let dup = map.get(&zone_id).is_some_and(|(src, sid, tid, when)| {
            src == source
                && sid == source_id
                && *tid == track_id
                && now.duration_since(*when) < DUPLICATE_NET_PLAY_WINDOW
        });
        if !dup {
            map.insert(
                zone_id,
                (source.to_string(), source_id.clone(), track_id, now),
            );
        }
        dup
    }

    /// Remove any gapless-prepared stream session for a zone.
    /// Called when a zone starts a new track or stops, so the
    /// previously prepared session doesn't leak.
    async fn cleanup_gapless_session(&self, zone_id: i64) {
        let old_sid = self.gapless_sessions.lock().await.remove(&zone_id);
        if let Some(ref sid) = old_sid {
            self.streamer.remove_session(sid).await;
            debug!(zone_id, stream_id = %sid, "gapless_session_cleaned_up");
        }
    }

    fn server_ip(&self) -> String {
        self.advertised_ip.clone().unwrap_or_else(|| {
            crate::discovery::ssdp::get_local_ip()
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "127.0.0.1".into())
        })
    }

    pub async fn play(&self, req: PlayRequest) -> Result<PlayResult, String> {
        // Free-tier zone cap, enforced at *activation* (a zone's first play).
        // This is the single choke point for every play entry point (direct
        // play, resume, streaming, transfer, alarms, AI) — gating here, rather
        // than at zone creation, both fixes the false "premium required" when
        // dormant auto-discovered zones filled the quota AND closes the "play a
        // few zones at a time to stay free" loophole.
        self.enforce_zone_cap(req.zone_id).await?;

        // Re-tap dedup (#1271): a controller (Flutter/web) can emit `play` TWICE
        // for the SAME track a few seconds apart. The first play already sent
        // SetAVTransportURI + Play to the renderer; the second, arriving AFTER the
        // first fully established playback (so the `superseded` play_seq guard in
        // play_inner sees no overlap and lets it through), would send a SECOND
        // SetAVTransportURI and restart a network renderer from byte 0 — the Revox
        // S100 "plays ~10s then jumps to 0" (forum, Philippe Vella). If this
        // request targets the track already playing on the zone and that track was
        // (re)started within RETAP_DEDUP_WINDOW, coalesce it: return the current
        // state WITHOUT re-resolving the source or re-sending to the renderer.
        //
        // This is complementary to the `superseded` (overlapping) and the
        // last_net_play (post-resolve) guards in play_inner, both untouched.
        // Safety of the exclusions:
        //   - a play of a DIFFERENT track never matches (is_same_track_retap);
        //   - a genuine replay of the same track after the window has an older
        //     start timestamp, so it is NOT coalesced (plays from 0);
        //   - an explicit seek is exempt (seek_ms > 0);
        //   - stall recovery stops the zone first, so state != Playing here, and
        //     auto-resume runs from a Stopped state — neither trips this guard.
        if req.seek_ms.unwrap_or(0) == 0 {
            let state = self.playback.get_state(req.zone_id).await;
            if state.state == PlayState::Playing {
                if let Some(np) = state.now_playing.as_ref() {
                    let recent = state
                        .last_play_started_at
                        .map(|t| t.elapsed() < RETAP_DEDUP_WINDOW)
                        .unwrap_or(false);
                    if recent && Self::is_same_track_retap(np, &req) {
                        info!(
                            zone_id = req.zone_id,
                            title = %np.title,
                            source = %np.source,
                            "orchestrator_play_retap_deduped_same_inflight_track"
                        );
                        return Ok(PlayResult {
                            stream_url: None,
                            output_sent: false,
                            source: np.source.clone(),
                            error: None,
                        });
                    }
                }
            }
        }

        // Public entry point: this is a *new* logical play, so it is recorded
        // in the listen history.
        self.play_inner(req, true).await
    }

    /// Identity match for the re-tap dedup: is `req` targeting the SAME track the
    /// zone's current `now_playing` (`np`) represents? Prefers the library
    /// `track_id` when both sides carry one; otherwise matches a non-empty
    /// streaming `(source, source_id)` — and if `req` names a `source` it must
    /// agree with the now-playing source. Returns `false` when neither side
    /// yields a positive identifier, so two unidentifiable plays never collide
    /// (a false negative merely lets the normal play path run). Pure so it can be
    /// unit-tested without a live orchestrator.
    fn is_same_track_retap(np: &NowPlaying, req: &PlayRequest) -> bool {
        if let (Some(a), Some(b)) = (np.track_id, req.track_id) {
            return a == b;
        }
        match (&np.source_id, &req.source_id) {
            (Some(a), Some(b)) if !a.is_empty() && a == b => {
                req.source.as_deref().is_none_or(|s| s == np.source)
            }
            _ => false,
        }
    }

    /// Free-tier gate: block *activating* a brand-new zone once the free active
    /// limit is reached. A zone that has already played (`last_track_id` set) is
    /// unaffected, so replays / auto-advance / resume never trip the gate; only
    /// the first play of an as-yet-unused zone counts. No license set (tests) or
    /// Premium tier → always allowed.
    async fn enforce_zone_cap(&self, zone_id: i64) -> Result<(), String> {
        let Some(ref lic) = self.license else {
            return Ok(());
        };
        let zrepo = ZoneRepo::with_backend(self.db.clone());
        let already_active = zrepo
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.last_track_id)
            .is_some();
        if already_active || lic.is_premium().await {
            return Ok(());
        }
        let active = zrepo.count_active().unwrap_or(0);
        if active >= lic.free_zone_limit() {
            return Err(format!(
                "premium_required:Free tier is limited to {} active zones. Upgrade to Tune Premium for unlimited zones.",
                lic.free_zone_limit()
            ));
        }
        Ok(())
    }

    /// Like `play`, but does NOT write a listen-history row.  Used for internal
    /// stream re-creations of a track that is *already* being played (seek,
    /// radio auto-retry, reconnect) so a single logical play is not counted
    /// multiple times in the "Historique de lecture".
    pub async fn play_without_history(&self, req: PlayRequest) -> Result<PlayResult, String> {
        self.play_inner(req, false).await
    }

    /// Pick a live output to re-bind a zone to, when its stored
    /// `output_device_id` has vanished from the registry.
    ///
    /// Matches on the zone's display name (case-insensitive) and **prefers a
    /// `local:` output**: the case this exists for is a zone created long ago
    /// against a *network* view of a device that is now only reachable locally
    /// (Alex Campbell's "Mac Studio Speakers", once seen over the network by a
    /// second server on a Raspberry Pi, today a plain CoreAudio output).
    ///
    /// Returns `None` when there is no match **or** when the match is ambiguous
    /// — several same-name outputs with no single local one. Binding "at
    /// random" would send audio to the wrong device, which is worse than the
    /// clear error the caller falls back to.
    async fn find_rebind_target(&self, zone_name: &str) -> Option<(String, String)> {
        let candidates = { self.outputs.lock().await.find_by_name(zone_name) };
        if candidates.is_empty() {
            return None;
        }
        let mut locals: Vec<&(String, String)> = candidates
            .iter()
            .filter(|(id, _)| id.starts_with("local:"))
            .collect();
        if locals.len() == 1 {
            return Some(locals.remove(0).clone());
        }
        if locals.is_empty() && candidates.len() == 1 {
            return Some(candidates[0].clone());
        }
        warn!(
            zone_name,
            candidates = candidates.len(),
            locals = locals.len(),
            "zone_rebind_ambiguous_not_rebinding"
        );
        None
    }

    /// Gate playback on a zone whose stored output device may be gone, trying an
    /// auto-rebind before refusing.
    ///
    /// `Ok(None)` — nothing to do, carry on (this is the nominal path).
    /// `Ok(Some(id))` — the zone was re-bound to `id`, which the caller must use
    /// as the request's `output_device_id`.
    /// `Err(msg)` — playback must be refused; `msg` carries the
    /// `zone_output_unavailable:` sentinel the API maps to a 409.
    async fn gate_or_rebind_offline_zone(
        &self,
        zone_id: i64,
        zone: &crate::db::zone_repo::Zone,
    ) -> Result<Option<String>, String> {
        if zone.online {
            return Ok(None);
        }
        let dev_id = zone.output_device_id.as_deref().unwrap_or("");
        // Skip zones with no device yet (being configured) and `local:` zones,
        // which are reputed always available. Then allow a grace window for SSDP
        // polling gaps: if the device is still in the live registry it is
        // reachable, whatever the DB says.
        if dev_id.is_empty()
            || dev_id.starts_with("local:")
            || self.outputs.lock().await.contains(dev_id)
        {
            return Ok(None);
        }

        // The stored device really is gone. Before rejecting, look for a live
        // output carrying the same name (#1287).
        if let Some((new_id, new_type)) = self.find_rebind_target(&zone.name).await {
            let repo = ZoneRepo::with_backend(self.db.clone());
            // Persist so the rebind is sticky — the point is that the user never
            // has to think about this again. `output_type` must follow the id:
            // leaving a zone typed `dlna` while pointing at a `local:` output
            // would take the wrong branch everywhere downstream.
            repo.update_output_device(zone_id, &new_id)?;
            repo.update_output_type(zone_id, &new_type)?;
            repo.update_online(zone_id, true)?;
            info!(
                zone_id,
                zone_name = %zone.name,
                stale_device_id = dev_id,
                new_device_id = %new_id,
                new_output_type = %new_type,
                "zone_rebound_to_live_output_with_same_name"
            );
            if let Some(ref bus) = self.event_bus {
                bus.emit(
                    "zone.rebound",
                    serde_json::json!({
                        "zone_id": zone_id,
                        "device_id": new_id,
                        "output_type": new_type,
                    }),
                );
            }
            return Ok(Some(new_id));
        }

        let msg = format!(
            "zone_output_unavailable:La sortie de cette zone n'est plus disponible. Choisissez une sortie dans les réglages de la zone « {} ».",
            zone.name
        );
        warn!(zone_id, zone_name = %zone.name, "play_rejected_zone_offline");
        if let Some(ref bus) = self.event_bus {
            bus.emit(
                "zone.playback_error",
                serde_json::json!({
                    "zone_id": zone_id,
                    "error": msg,
                }),
            );
        }
        Err(msg)
    }

    async fn play_inner(
        &self,
        mut req: PlayRequest,
        record_history: bool,
    ) -> Result<PlayResult, String> {
        let play_start = std::time::Instant::now();
        // Zone navigateur : la sortie est l'onglet, pas un appareil. Relevé ici
        // parce que la ligne de zone est déjà lue juste en dessous, et qu'il
        // faudra le savoir bien plus bas, au moment d'annoncer l'écoute
        // (#1998). Une zone navigateur n'a JAMAIS d'`output_device_id` : si le
        // demandeur en fournit un, ce n'en est pas une.
        let mut zone_navigateur = false;
        // Ensure output_device_id is populated: if the caller didn't provide
        // it (e.g. web client sends only zone_id + track_id), look it up from
        // the zone's DB record.  This is the primary gate for send_to_output —
        // without it, the stream is created but never sent to the output device.
        if req.output_device_id.is_none() {
            let zone_db = ZoneRepo::with_backend(self.db.clone())
                .get(req.zone_id)
                .ok()
                .flatten();

            // Refuse to start playback on a zone whose device is confirmed gone
            // — unless a live output of the same name can take over (#1287).
            let rebound = match zone_db {
                Some(ref zone) => self.gate_or_rebind_offline_zone(req.zone_id, zone).await?,
                None => None,
            };

            let looked_up =
                rebound.or_else(|| zone_db.as_ref().and_then(|z| z.output_device_id.clone()));
            if looked_up.is_some() {
                debug!(
                    zone_id = req.zone_id,
                    device_id = ?looked_up,
                    "output_device_id_resolved_from_zone_db"
                );
            } else {
                warn!(
                    zone_id = req.zone_id,
                    "output_device_id_missing_not_in_request_nor_zone_db"
                );
                // Orphan-zone guard (Yacine, 24/07): a zone row with NO
                // output_device_id can never produce sound — send_to_output is
                // skipped and play() "succeeds" with output_sent=false, so the
                // client shows the track while nothing plays. Fail loudly with
                // a sentinel the API maps to a clean 4xx instead of a silent
                // success. Browser zones are exempt: they legitimately have no
                // output device (the web client pulls stream_url itself). Zones
                // absent from the DB keep the old behaviour (in-memory tests /
                // transient states).
                if let Some(ref zone) = zone_db {
                    zone_navigateur = zone.output_type.as_deref() == Some("browser");
                    if !zone_navigateur {
                        let msg = format!(
                            "zone_no_output_device:Zone '{}' has no output device assigned — assign an output device to this zone or delete it and re-create it from a device.",
                            zone.name
                        );
                        warn!(zone_id = req.zone_id, zone_name = %zone.name, "play_rejected_zone_without_output_device");
                        if let Some(ref bus) = self.event_bus {
                            bus.emit(
                                "zone.playback_error",
                                serde_json::json!({
                                    "zone_id": req.zone_id,
                                    "error": msg,
                                }),
                            );
                        }
                        return Err(msg);
                    }
                }
            }
            req.output_device_id = looked_up;
        } else {
            // output_device_id was provided by the caller — run the same gate.
            // The client's id comes from the same stale zone row, so a rebind
            // must override it, otherwise we would keep aiming at the dead
            // device the caller just told us about.
            let zone_db = ZoneRepo::with_backend(self.db.clone())
                .get(req.zone_id)
                .ok()
                .flatten();
            let rebound = match zone_db {
                Some(ref zone) => self.gate_or_rebind_offline_zone(req.zone_id, zone).await?,
                None => None,
            };
            if let Some(new_id) = rebound {
                req.output_device_id = Some(new_id);
            }
        }

        // Clean up any gapless-prepared session for this zone before
        // creating a new stream.
        self.cleanup_gapless_session(req.zone_id).await;

        // Remember old session for cleanup AFTER output has been stopped
        let prev_state = self.playback.get_state(req.zone_id).await;
        let old_stream_id = prev_state
            .now_playing
            .as_ref()
            .and_then(|np| np.stream_id.clone());

        // Bump track_generation NOW so the poller resets its wall-clock
        // timer immediately. Without this, a long DASH transcode (20-30s)
        // can run into the 300s timeout from the previous track.
        let play_gen = self.playback.bump_generation(req.zone_id).await;

        // Signaler la recherche AVANT de la lancer : sur YouTube elle peut durer
        // une trentaine de secondes, et un écran muet pendant ce temps se lit
        // comme une panne (forum #1359). Le drapeau retombe dans `play()`, dès
        // qu'une URL jouable existe — y compris sur les chemins d'erreur, qui
        // passent tous par un `play()` ou un `stop()` ultérieur.
        self.playback.set_resolving(req.zone_id, true).await;

        // ⚠ TOUTE sortie de ce bloc doit abaisser le drapeau, sinon la zone reste
        // affichée « recherche en cours » indéfiniment. Trois chemins quittent
        // ici sans passer par `play()` : l'échec du fichier uploadé, la reprise
        // par une lecture plus récente, et l'échec de résolution.
        let resolved = if let Some(ref temp_path) = req.temp_file_path {
            match self.resolve_uploaded_file(temp_path, &req).await {
                Ok(r) => r,
                Err(e) => {
                    self.playback.set_resolving(req.zone_id, false).await;
                    return Err(e);
                }
            }
        } else {
            match self.resolve_stream(&req).await {
                Ok(r) => r,
                // A newer tap superseded this play before its transcode ran:
                // yield quietly (the winning play drives the output) instead of
                // surfacing an error for the redundant tap.
                Err(e) if e == SUPERSEDED_BEFORE_TRANSCODE => {
                    info!(
                        zone_id = req.zone_id,
                        "orchestrator_play_superseded_before_transcode"
                    );
                    // La lecture gagnante a pose son propre drapeau : ne pas
                    // l'abaisser ici, on effacerait SON etat de recherche.
                    return Ok(PlayResult {
                        stream_url: None,
                        output_sent: false,
                        source: "local".into(),
                        error: Some("superseded by a newer play".into()),
                    });
                }
                Err(e) => {
                    self.playback.set_resolving(req.zone_id, false).await;
                    return Err(e);
                }
            }
        };
        let resolve_ms = play_start.elapsed().as_millis();

        // If a newer play for this zone started while we were resolving, abort
        // before sending output. Resolving can take tens of seconds (a slow
        // network-volume ALAC→FLAC transcode for a DLNA renderer), during which
        // a user tapping play again — or the poller — stacks several plays; if
        // each one pushed its stream to the renderer, the overlapping audio came
        // out as noise (Yves, DMP-A10 over DLNA). Only the latest play should
        // reach the device.
        if self.playback.current_play_seq(req.zone_id).await != play_gen {
            info!(
                zone_id = req.zone_id,
                title = %resolved.title,
                resolve_ms,
                "orchestrator_play_superseded_skipping_output"
            );
            if let Some(ref sid) = resolved.stream_id {
                self.streamer.remove_session(sid).await;
            }
            return Ok(PlayResult {
                stream_url: None,
                output_sent: false,
                source: resolved.source,
                error: Some("superseded by a newer play".into()),
            });
        }

        // Coalesce a redundant re-play of the SAME track to a NETWORK renderer.
        // The generation guard above only aborts an OVERLAPPING play; it cannot
        // stop one that starts just AFTER the first already sent its URI. A slow
        // pre-transcode-to-file resolve (Tidal AAC / hi-res DASH, 4-10s) races a
        // second play/advance trigger for the same track, and BOTH reach the
        // renderer — the second SetAVTransportURI restarts it from 0 (Revox S100:
        // plays a few seconds, jumps to 0, then plays through — forum, Philippe
        // Vella). If we pushed this exact (source, source_id) to this zone within
        // DUPLICATE_NET_PLAY_WINDOW and this is not an explicit seek, skip the
        // redundant send. Push-URI outputs only (the ones that can even exhibit a
        // SetAVTransportURI restart-from-0) — checked against the registered
        // output's real `output_type()` via `is_push_uri_output_type`, rather
        // than guessed from the device_id string. Local/USB outputs prefill
        // their own ring buffer and don't restart-glitch, and neither do
        // pull-based out-of-tree outputs (oaat, diretta) that fetch audio
        // themselves instead of being pushed a URI — a `!starts_with("local:")`
        // guess would have wrongly caught both of those (they don't use a
        // "local:" device_id prefix either), coalescing a legitimate same-track
        // replay within the window. The record is cleared on stop(), so a
        // stop→replay of the same track still plays.
        let is_net_output = match req.output_device_id.as_deref() {
            Some(id) => is_push_uri_output_type(self.output_type_of(id).await.as_deref()),
            None => false,
        };
        let is_seek = req.seek_ms.unwrap_or(0) > 0;
        if is_net_output && !is_seek {
            let is_dup = {
                let mut last = self.last_net_play.lock().await;
                Self::record_or_detect_duplicate_net_play(
                    &mut last,
                    req.zone_id,
                    &resolved.source,
                    &req.source_id,
                    req.track_id,
                    std::time::Instant::now(),
                )
            };
            if is_dup {
                info!(
                    zone_id = req.zone_id,
                    title = %resolved.title,
                    source = %resolved.source,
                    "orchestrator_play_coalesced_duplicate_net_send"
                );
                if let Some(ref sid) = resolved.stream_id {
                    self.streamer.remove_session(sid).await;
                }
                return Ok(PlayResult {
                    stream_url: None,
                    output_sent: false,
                    source: resolved.source,
                    error: None,
                });
            }
        }

        let cover_path = req.cover_url.clone().or(resolved.cover_url.clone());
        let album = req.album_title.clone().or(resolved.album.clone());
        let track_meta = req.track_id.and_then(|tid| {
            crate::db::track_repo::TrackRepo::with_backend(self.db.clone())
                .get(tid)
                .ok()
                .flatten()
        });
        let np = NowPlaying {
            track_id: req.track_id,
            title: resolved.title.clone(),
            artist_name: resolved.artist.clone(),
            album_title: album.clone(),
            cover_path: cover_path.clone(),
            duration_ms: resolved.duration_ms.unwrap_or(0),
            source: resolved.source.clone(),
            source_id: req.source_id.clone(),
            stream_id: resolved.stream_id.clone(),
            format: track_meta
                .as_ref()
                .and_then(|t| t.format.clone())
                // Qobuz only ever streams FLAC; surface the source format even
                // when the stream is transcoded to WAV for a local output, so
                // the format chip shows FLAC and not the output container
                // (Progman/Cyrille: Qobuz shown compressed / wrong format).
                .or_else(|| match resolved.source.as_str() {
                    "qobuz" => Some("flac".to_string()),
                    // Bandcamp : le dire ici — et non depuis le client — pour
                    // deux raisons : le repli sur le type MIME afficherait
                    // « MPEG » au lieu de « MP3 », et surtout l'avance de file
                    // re-résout la piste SANS qu'aucun client repasse un
                    // format. Sans cette ligne, la piste 1 s'affichait
                    // autrement que la piste 2 du même album.
                    //
                    // Le codec vient de l'URL, pas du nom du service : sans
                    // achat Bandcamp ne sert que du `mp3-128`, mais un fichier
                    // acheté descend en `flac`/`alac` par la même porte, et
                    // l'annoncer « MP3 » serait faux (#2074). Repli sur `mp3`
                    // quand l'URL ne nomme rien : c'est l'écoute libre.
                    "bandcamp" => Some(
                        req.source_id
                            .as_deref()
                            .and_then(bandcamp_encoding)
                            .and_then(|enc| bandcamp_quality(&enc))
                            .map(|q| q.codec.to_string())
                            .unwrap_or_else(|| "mp3".to_string()),
                    ),
                    _ => None,
                })
                // A media-server (UPnP/NAS) item has no local track row, so the
                // codec the client read from the DIDL res@protocolInfo is the only
                // authoritative source: audio/mp4 is ambiguous ALAC-vs-AAC, so
                // surface "alac" here instead of falling back to the "mp4" MIME
                // and mislabeling a lossless ALAC as lossy AAC (Yves, NAS).
                .or_else(|| req.media_format.clone())
                .or_else(|| {
                    let mime = &resolved.mime_type;
                    Some(
                        mime.strip_prefix("audio/")
                            .unwrap_or(mime)
                            .replace("x-", "")
                            .to_string(),
                    )
                }),
            // Prefer the SOURCE resolution (library metadata) over the resolved
            // OUTPUT format. Local playback forces a 32-bit WAV to the DAC, but
            // the "now playing" label must show the file's real depth (16/24) —
            // matching the gapless path (advance_queue_metadata), which avoids the
            // "first tracks show 32-bit then correct to 16" glitch. Streaming has
            // no local row so it falls back to the resolved stream format. DSD
            // display is handled separately in zones.rs.
            //
            // Le repli tenait sur `track_meta = None`, et c'etait trop large :
            // une piste LOCALE dont la ligne ignore la frequence ou la
            // profondeur y retombait aussi, et heritait alors du chiffre que
            // `resolve_local_track` venait de fabriquer (`unwrap_or(44100)` /
            // `unwrap_or(16)`). La meme ligne atteinte par avance gapless,
            // elle, n'annoncait rien (`NowPlaying::from_track`). La lecture
            // aleatoire rendait l'ecart visible : elle demarre sans cesse une
            // PREMIERE piste tiree au hasard (#2250, fil 1036). Le predicat
            // porte desormais sur la SOURCE, pas sur la presence d'une ligne.
            sample_rate: resolution_annoncee(
                track_meta
                    .as_ref()
                    .and_then(|t| t.sample_rate.map(|v| v as u32)),
                resolved.sample_rate,
                resolved.source == "local",
            ),
            bit_depth: resolution_annoncee(
                track_meta
                    .as_ref()
                    .and_then(|t| t.bit_depth.map(|v| v as u32)),
                resolved.bit_depth,
                resolved.source == "local",
            ),
            genre: track_meta.as_ref().and_then(|t| t.genre.clone()),
            year: track_meta.as_ref().and_then(|t| t.year),
            // L'album et l'artiste par IDENTIFIANT. Sans eux, le client devait
            // deviner l'album depuis son TITRE : cliquer sur « Entreat (2010) »
            // ouvrait la page de The Cure (FabienM, v0.9.102). `track_meta` les
            // a deja sous la main — c'est la ligne de la bibliotheque.
            //
            // `None` pour une radio ou un flux : ils n'ont pas d'entree en
            // bibliotheque, et inventer un identifiant serait pire que de n'en
            // donner aucun.
            album_id: track_meta.as_ref().and_then(|t| t.album_id),
            artist_id: track_meta.as_ref().and_then(|t| t.artist_id),
            // Le débit annoncé par la SOURCE, quand elle le nomme. C'est ce
            // qui manquait au 128 kbit/s de Bandcamp : annoncé sur l'écran de
            // découverte, il disparaissait dès que la piste partait en zone —
            // le chemin du signal ne montrait plus qu'un « MP3 » qu'aucun
            // auditeur ne peut distinguer d'un 320 (#2074). Une piste locale
            // n'en porte pas : sa résolution réelle est lue au scan.
            bitrate_kbps: resolved.bitrate_kbps,
        };

        self.playback.play(req.zone_id, np).await;

        // Persist play state for auto-resume after server restart
        crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .save_play_state(req.zone_id, "playing")
            .ok();

        // L'annonce « en écoute » N'EST PLUS ici : elle attend de savoir si la
        // sortie a accepté le titre. Voir plus bas, après `output_sent`.
        //
        // Elle partait à cet endroit, c'est-à-dire AVANT toute tentative
        // d'envoi. Chez Bilou (#1998), quatre échecs de sortie BluOS d'affilée
        // ont produit quatre annonces à Last.fm pour un titre jamais entendu :
        // `output_sent=false`, zone arrêtée sur-le-champ, session de flux
        // fermée — et 233 ms plus tard « en écoute ».
        //
        // Le profil d'écoute est publié hors de chez l'utilisateur, sur un
        // service tiers, sans correction commode. Une écoute inventée y reste.

        // For local outputs, keep the old stream alive until after play_url()
        // calls stop() — otherwise the audio thread gets a read error when the
        // HTTP session is removed while it's still reading. For network outputs
        // (DLNA), close the old stream first to avoid stale bytes.
        let is_local = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("local:"));
        if !is_local {
            if let Some(ref old_sid) = old_stream_id {
                self.streamer.remove_session(old_sid).await;
            }
        }

        let (output_sent, output_error) = if let Some(ref device_id) = req.output_device_id {
            let resolved_cover_url = self.resolve_cover_url(cover_path.as_deref());
            // One DB read for the local row: the output needs its path, and its
            // album numbering when the request did not carry any (a play by
            // track id, not from a queue row).
            let local_track = if resolved.source == "local" {
                req.track_id.and_then(|tid| {
                    TrackRepo::with_backend(self.db.clone())
                        .get(tid)
                        .ok()
                        .flatten()
                })
            } else {
                None
            };
            let local_file_path = local_track.as_ref().and_then(|t| t.file_path.clone());
            let media_source_id = req
                .source_id
                .clone()
                .or_else(|| req.track_id.map(|t| t.to_string()));
            // The library row stores 0 for "unknown", so filter it out rather
            // than telling the output this is track 0.
            let media_track_number = req.track_number.or_else(|| {
                local_track
                    .as_ref()
                    .map(|t| t.track_number)
                    .filter(|n| *n > 0)
                    .map(|n| n as u32)
            });
            let media_disc_number = req.disc_number.or_else(|| {
                local_track
                    .as_ref()
                    .map(|t| t.disc_number)
                    .filter(|n| *n > 0)
                    .map(|n| n as u32)
            });
            // Une session-canal (conversion à la volée) ne sait pas rejouer un
            // octet passé : la DIDL doit annoncer DLNA.ORG_OP=00, sans quoi le
            // renderer seeke par tranches et gèle à 0:00 (DMP-A8, DSD, 24/08).
            let byte_seekable = match resolved.stream_id.as_deref() {
                Some(sid) => {
                    let session = {
                        let sessions = self.streamer.sessions_state();
                        let guard = sessions.lock().await;
                        guard.get(sid).cloned()
                    };
                    match session {
                        Some(s) => !s.is_channel().await,
                        None => true,
                    }
                }
                None => true,
            };
            let media = crate::outputs::traits::PlayMedia {
                url: &resolved.url,
                mime_type: &resolved.mime_type,
                title: Some(&resolved.title),
                artist: resolved.artist.as_deref(),
                album: album.as_deref(),
                cover_url: resolved_cover_url.as_deref(),
                duration_ms: resolved.duration_ms.map(|d| d as u64),
                file_size: resolved.file_size,
                file_path: local_file_path.as_deref(),
                sample_rate: resolved.sample_rate,
                bit_depth: resolved.bit_depth,
                channels: resolved.channels,
                // Internet radio is an infinite live stream — mark it so the
                // DLNA DIDL advertises live/senderPaced semantics instead of a
                // seekable file (Yamaha R-N2000A stays silent otherwise).
                live_stream: resolved.source == "radio",
                byte_seekable,
                origin_url: resolved.origin_url.as_deref(),
                source: Some(&resolved.source),
                source_id: media_source_id.as_deref(),
                track_number: media_track_number,
                disc_number: media_disc_number,
            };
            let zone_audiophile = self.zone_audiophile(req.zone_id);

            // #1259: prebuffer ~2s before `Play` on network (DLNA) playback.
            //
            // A DLNA renderer starts its clock on the first byte it pulls. On
            // the initial play the decode pipeline is cold, so the first ~5s
            // trickle out and the renderer under-runs → micro-dropouts
            // (biblio/Qobuz/radio, macOS). Local/USB output has no such glitch:
            // LocalOutput prefills its ring buffer before it starts the DAC
            // (outputs/local.rs). We reproduce that server-side for DLNA here.
            //
            // Gating (kept tight for zero regression on the read path):
            //   - ONLY DLNA outputs (network push renderers with the cold-start
            //     clock). Local/USB and OAAT pull outputs already prefill.
            //   - ONLY channel sessions (WAV transcode / radio). Proxy passthrough
            //     and on-disk serve_file sessions are excluded INSIDE
            //     wait_prefill_ready (they have no channel to fill; blocking on
            //     them would hang).
            //   - Capped by a timeout so a slow or very short source never
            //     freezes the start of playback; `Play` is sent regardless.
            // The poller FSM is untouched: its 45s track-load grace already
            // tolerates the few extra seconds before Play.
            //
            // La RADIO a besoin de la même barrière quelle que soit la sortie
            // (#1628). Un flux live n'a pas de réserve par construction : on ne
            // peut envoyer que ce que la station a déjà émis. En partant sans
            // retard délibéré, la lecture colle au bord du direct et chaque
            // frontière de segment arrivant un peu tard passe sous le lead —
            // mesuré sur FIP → endpoint OAAT le 13/08 : des underruns
            // rythmiques toutes les ~8 s (la cadence des segments), de 0 à
            // 96 ms, indéfiniment. Les 2 s de tampon de l'endpoint n'y peuvent
            // rien : elles ne sont JAMAIS remplies. Accumuler ce retard avant
            // de lancer la lecture donne au flux la réserve qui lui manque ;
            // le coût est quelques secondes au zapping d'une station.
            // Temps réellement passé dans cette barrière. Il était jusqu'ici
            // noyé dans `output_ms` de `playback_timing`, ce qui rendait la
            // ligne trompeuse sur DLNA : jusqu'à 4 s d'attente DÉLIBÉRÉE s'y
            // lisaient comme « la sortie traîne » (#2488). Reste 0 quand la
            // barrière ne s'applique pas.
            let mut prebuffer_ms: u128 = 0;
            if let Some(ref sid) = resolved.stream_id {
                let is_dlna = self.output_type_of(device_id).await.as_deref() == Some("dlna");
                let is_radio = resolved.source == "radio";
                if is_dlna || is_radio {
                    let prebuffer_start = std::time::Instant::now();
                    let sr = resolved.sample_rate.unwrap_or(44100) as u64;
                    let ch = (resolved.channels.unwrap_or(2) as u64).max(1);
                    let bytes_per_sample = ((resolved.bit_depth.unwrap_or(16) as u64) / 8).max(1);
                    // La radio vise plus large : il faut couvrir une frontière
                    // de segment entière (~8 s observées) sans jamais toucher
                    // le fond, là où un fichier n'a qu'un démarrage à froid à
                    // absorber.
                    let secs = if is_radio { RADIO_PREBUFFER_SECS } else { 2 };
                    let target_bytes = sr * ch * bytes_per_sample * secs;
                    // Plafond : une station lente ou muette ne doit jamais
                    // geler le départ — `Play` part de toute façon.
                    let timeout = std::time::Duration::from_secs(if is_radio { 8 } else { 4 });
                    let reached = self
                        .streamer
                        .wait_prefill_ready(sid, target_bytes, timeout)
                        .await;
                    prebuffer_ms = prebuffer_start.elapsed().as_millis();
                    info!(
                        zone_id = req.zone_id,
                        stream_id = %sid,
                        target_bytes,
                        reached,
                        is_radio,
                        prebuffer_ms,
                        "initial_prebuffer_done"
                    );
                }
            }

            // A local output opens this URL in its own reader thread just
            // after `play_media`. If that reader never starts, the session is
            // alive but silent and used to leave only `stale_session_removed`
            // thirty minutes later (#2270). Arming here excludes sessions that
            // are merely prepared in advance for gapless playback.
            if is_local && let Some(ref sid) = resolved.stream_id {
                let _ = arm_local_stream_consumer_watch(
                    self.streamer.clone(),
                    sid.clone(),
                    req.zone_id,
                    device_id.clone(),
                    std::time::Duration::from_secs(15),
                )
                .await;
            }

            let result = self
                .send_to_output(
                    device_id,
                    &media,
                    req.seek_ms,
                    zone_audiophile,
                    req.zone_id,
                    req.track_id,
                )
                .await;
            let total_ms = play_start.elapsed().as_millis();
            // `output_ms` ne compte plus l'attente de pré-tampon : les trois
            // termes s'additionnent maintenant pour donner `total_ms`, et un
            // blanc s'impute à la bonne étape sans relire la source.
            info!(
                zone_id = req.zone_id,
                resolve_ms,
                prebuffer_ms,
                output_ms = total_ms
                    .saturating_sub(resolve_ms)
                    .saturating_sub(prebuffer_ms),
                total_ms,
                title = %resolved.title,
                "playback_timing"
            );

            // After play_media succeeds, send the zone's stored volume to the
            // renderer — but ONLY if the user has explicitly set a volume
            // (not the default 50). This prevents blasting speakers at an
            // unexpected level after a server restart.
            if result.0 {
                let zone_db = ZoneRepo::with_backend(self.db.clone())
                    .get(req.zone_id)
                    .ok()
                    .flatten();
                let is_fixed = zone_db.as_ref().is_some_and(|z| z.fixed_volume);
                // Only (re)assert the volume on play for fixed-volume (bit-perfect)
                // zones, which must sit at 100%. For a normal zone, leave the
                // device's current volume untouched: Tune previously pushed the
                // stored zone volume on EVERY play, overriding a level the user
                // had set directly on the device — the stored value drifts from
                // the device (no external-change sync) so a low device jumped to
                // the stored 50% on play (Fabien, "Salon"). Trade-off: this drops
                // the old "re-apply saved volume after a restart to avoid a blast"
                // behaviour for normal zones; the device keeps whatever level it
                // is physically at.
                if is_fixed {
                    let did = device_id.clone();
                    let outputs = self.outputs.clone();
                    let zone_id = req.zone_id;
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        let arc = { outputs.lock().await.get(&did) };
                        if let Some(output) = arc {
                            if let Err(e) = output.lock().await.checked_set_volume(1.0).await {
                                warn!(zone_id, volume = 1.0, error = %e, "play_initial_volume_failed");
                            } else {
                                info!(zone_id, volume = 1.0, "play_initial_volume_sent");
                            }
                        }
                    });
                }
            }

            result
        } else {
            warn!(
                zone_id = req.zone_id,
                "no_output_device_id_skipping_send_to_output"
            );
            (false, None)
        };

        // For local outputs, clean up the old stream now that play_url() has
        // called stop() and the old audio thread is no longer reading.
        if is_local {
            if let Some(ref old_sid) = old_stream_id {
                self.streamer.remove_session(old_sid).await;
            }
        }

        // Only record a listen-history row for genuinely new plays.  Seek and
        // radio auto-retry re-create the stream for a track that is already
        // playing (via play_without_history) and must not add duplicate rows.
        //
        // Skip live radio entirely: the title/artist supplied at play time is a
        // frozen snapshot (station name, or a stale song copied from a history
        // line when replaying) and never describes what the station is actually
        // streaming now, so recording it produces listen_history rows with no
        // relation to what was heard, plus a fresh bogus row on every replay
        // click (Bilou). Station plays are already tracked in the radio_stations
        // table (record_play), so nothing is lost.
        // Ce que la sortie a refusé n'a été entendu nulle part.
        //
        // `output_sent` est établi juste au-dessus, et l'arrêt immédiat de la
        // zone s'appuie déjà dessus quelques lignes plus bas : le signal
        // existait, dans la même fonction, et ces deux écritures ne le
        // consultaient pas.
        //
        // Le cas « aucune sortie configurée » rend lui aussi `false` (avec
        // `no_output_device_id_skipping_send_to_output`) : ne rien annoncer y
        // est également juste — le titre n'est parti vers aucun appareil.
        //
        // SAUF la zone navigateur, dont la sortie EST l'onglet : elle n'a pas
        // de périphérique, `output_sent` y vaut toujours faux, et une lecture
        // peut parfaitement avoir lieu. La garde ci-dessus lui a supprimé toute
        // annonce, y compris quand elle joue — c'est ce que ce ticket a rouvert
        // (#1998). Elle n'est pas remise dans le cas général : son annonce est
        // DIFFÉRÉE jusqu'à la preuve que l'onglet tire le flux.
        if !output_sent && !zone_navigateur {
            debug!(
                zone_id = req.zone_id,
                title = %resolved.title,
                "play_not_announced_output_not_sent"
            );
        }

        if !output_sent && zone_navigateur {
            let attente = AnnonceNavigateurDifferee {
                stream_id: resolved.stream_id.clone().unwrap_or_default(),
                title: resolved.title.clone(),
                artist: resolved.artist.clone(),
                album: album.clone(),
                source: resolved.source.clone(),
                source_id: req.source_id.clone(),
                track_id: req.track_id,
                duration_ms: resolved.duration_ms.unwrap_or(0),
                cover_path: cover_path.clone(),
                record_history,
            };
            if attente.stream_id.is_empty() {
                // Sans flux, rien à observer : on ne saurait jamais dire que
                // l'onglet a lu. Ne rien mettre en attente vaut mieux qu'une
                // entrée qu'aucune preuve ne pourra jamais libérer.
                debug!(
                    zone_id = req.zone_id,
                    title = %resolved.title,
                    "browser_now_playing_not_deferred_no_stream"
                );
            } else {
                debug!(
                    zone_id = req.zone_id,
                    title = %resolved.title,
                    stream_id = %attente.stream_id,
                    "browser_now_playing_deferred_until_stream_pulled"
                );
                if let Ok(mut en_attente) = self.annonces_navigateur.lock() {
                    en_attente.insert(req.zone_id, attente);
                }
            }
        }

        // Annonce « en écoute » multi-service, avec palier de licence.
        if output_sent {
            self.dispatch_now_playing(
                &resolved.title,
                resolved.artist.as_deref(),
                album.as_deref(),
            );
        }

        // `record_listen` alimente `listen_history`, la statistique locale. Il
        // souffrait du même défaut, et l'issue posait la question sans pouvoir
        // la trancher sur les seuls journaux : oui, l'historique local était
        // falsifié lui aussi.
        //
        // Le scrobble DÉFINITIF, lui, n'a jamais été concerné : il est
        // déclenché par le poller une fois le seuil des 50 % / 4 min franchi
        // (`dispatch_scrobble`, #1113), et une lecture qui n'a jamais démarré
        // ne l'atteint pas. C'est la seconde question ouverte du ticket.
        if output_sent && record_history && resolved.source != "radio" {
            // Owning profile = the zone's current session, set by the play
            // handler from X-Profile-Id and inherited by autoplay / gapless
            // advances (which reuse the zone without touching it). Resolved here
            // in async context so record_listen itself stays sync.
            // Meme lecture, meme raison que le profil : l'etat de zone porte
            // aussi CE QUE l'auditeur a demande (piste, album, playlist,
            // artiste, label), pose par le gestionnaire de `play` a partir du
            // corps de la requete. Une avance automatique herite du contexte
            // sans y toucher.
            let etat = self.playback.get_state(req.zone_id).await;
            let session_profile_id = etat.session_profile_id;
            let context = (etat.session_context_type, etat.session_context_id);
            self.record_listen(
                &resolved.title,
                resolved.artist.as_deref(),
                album.as_deref(),
                &resolved.source,
                req.source_id.as_deref(),
                req.track_id.and_then(|tid| {
                    TrackRepo::with_backend(self.db.clone())
                        .get(tid)
                        .ok()
                        .flatten()
                        .and_then(|t| t.album_id)
                }),
                resolved.duration_ms.unwrap_or(0),
                req.zone_id,
                cover_path.as_deref(),
                session_profile_id,
                context.0.as_deref(),
                context.1.as_deref(),
            );
        }

        info!(
            zone_id = req.zone_id,
            title = %resolved.title,
            source = %resolved.source,
            output_sent,
            "orchestrator_play"
        );

        // Fail fast when the initial output send itself errored.
        //
        // play() already flipped the zone to Playing and bumped
        // track_generation (so the poller armed its 45s track-load grace).
        // That grace exists for a real renderer that accepts the stream but
        // takes a few seconds to start pulling bytes — NOT for a renderer that
        // outright rejected the stream (e.g. AirPlay ANNOUNCE → 403). Without
        // this short-circuit the zone sits "loading" for ~45s of grace + ~30
        // stopped ticks (~100s total) before the poller finally gives up with
        // bytes_sent=0 (Bilou, forum #1135).
        //
        // We only trip here on an explicit send error (output_error.is_some()
        // with a requested output device). The success path (play_media → Ok,
        // output_sent=true) is untouched, so a slow-but-valid renderer keeps
        // its full grace period. The superseded and "no output device" cases
        // returned earlier / set output_error=None and are likewise unaffected.
        if !output_sent && output_error.is_some() && req.output_device_id.is_some() {
            warn!(
                zone_id = req.zone_id,
                device_id = req.output_device_id.as_deref().unwrap_or(""),
                error = output_error.as_deref().unwrap_or(""),
                "output_send_failed_stopping_zone_immediately"
            );
            // La session de flux ne se détruit que si la commande n'a
            // certainement PAS été exécutée. Sur un timeout, elle a pu atteindre
            // un renderer lent : détruire le flux garantit alors qu'il tombe sur
            // un 404 en allant le chercher, et affiche « chanson non trouvée ».
            // On la laisse vivre — la GC des sessions périmées la ramassera si
            // personne ne la consomme.
            let may_have_landed = output_error.as_deref().is_some_and(command_may_have_landed);
            if let Some(ref sid) = resolved.stream_id {
                if may_have_landed {
                    info!(
                        zone_id = req.zone_id,
                        stream_id = %sid,
                        "output_send_timed_out_keeping_stream_session"
                    );
                } else {
                    self.streamer.remove_session(sid).await;
                }
            }
            // Surface the failure now: flip the zone to Stopped so the poller's
            // load-grace path never runs and the UI reflects the error within a
            // poll tick instead of ~100s later.
            self.playback.stop(req.zone_id).await;
            crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
                .save_play_state(req.zone_id, "stopped")
                .ok();
            return Ok(PlayResult {
                stream_url: Some(resolved.url),
                output_sent: false,
                source: resolved.source,
                error: output_error,
            });
        }

        // Trigger prefetch of the next track in the background.
        // This runs concurrently with the current playback so the next
        // streaming track is already decoded in memory when needed.
        {
            let prefetch = self.prefetch.clone();
            let db = self.db.clone();
            let services = self.services.clone();
            let playback = self.playback.clone();
            let zone_id = req.zone_id;
            tokio::spawn(async move {
                prefetch
                    .prefetch_next(db, services, playback, zone_id)
                    .await;
            });
        }

        Ok(PlayResult {
            stream_url: Some(resolved.url),
            output_sent,
            source: resolved.source,
            error: output_error,
        })
    }

    /// Check whether a DLNA renderer supports a given MIME type by querying
    /// its ConnectionManager GetProtocolInfo Sink.  Results are cached per
    /// device_id so the SOAP call only happens once per renderer per session.
    async fn dlna_supports_mime(&self, device_id: &str, mime: &str) -> bool {
        // Check negative cache first
        {
            let cache = self.dlna_unsupported_mimes.lock().await;
            if let Some(unsupported) = cache.get(device_id) {
                if unsupported.iter().any(|m| m == mime) {
                    return false;
                }
                // We already probed this device — if the MIME is not in the
                // unsupported list, it means it was supported.
                if !unsupported.is_empty() {
                    // Device was probed at least once (it returned some
                    // unsupported entries or we stored an empty vec for it).
                    // But we can't distinguish "probed and supported" from
                    // "never checked this mime".  So we only use the cache
                    // for known negatives and re-probe below if needed.
                }
            }
        }

        // Probe the renderer. None = inconclusive probe (SOAP failed / empty
        // Sink) — fall back conservatively but do NOT cache, so one transient
        // failure doesn't force WAV for the whole session (Marco's Denon).
        let probe = {
            let arc = { self.outputs.lock().await.get(device_id) };
            if let Some(output) = arc {
                let locked = output.lock().await;
                if let Some(dlna) = locked
                    .as_any()
                    .downcast_ref::<crate::outputs::dlna::DlnaOutput>()
                {
                    dlna.supports_mime(mime).await
                } else {
                    // Not a DLNA output — format negotiation doesn't apply
                    Some(true)
                }
            } else {
                Some(true)
            }
        };

        match probe {
            Some(true) => true,
            Some(false) => {
                // Renderer's Sink was read and genuinely lacks this MIME — cache.
                let mut cache = self.dlna_unsupported_mimes.lock().await;
                let entry = cache.entry(device_id.to_string()).or_default();
                if !entry.iter().any(|m| m == mime) {
                    entry.push(mime.to_string());
                }
                false
            }
            None => {
                // Inconclusive — universal formats assumed OK, others not, but
                // not cached so the next play re-probes.
                matches!(
                    mime.to_lowercase().as_str(),
                    "audio/wav" | "audio/x-wav" | "audio/l16" | "audio/mpeg"
                )
            }
        }
    }

    /// Faut-il envoyer le DSD tel quel au renderer ?
    ///
    /// Fonction PURE : le mode reglé, et ce que le sondage a repondu —
    /// `Some(true)` / `Some(false)` sur une reponse concluante, `None` sinon. La
    /// sonde reseau vit dans `sonder_dsd` ; ici il n'y a que la decision, donc
    /// elle se teste.
    ///
    /// La subtilite est le troisieme cas. `None` ne veut PAS dire « non » : des
    /// renderers lisent le DSD sans l'annoncer dans leur GetProtocolInfo.
    /// Ecraser un reglage explicite sur une absence de reponse priverait de DSD
    /// natif des gens qui l'avaient — la faute symetrique de celle qu'on
    /// corrige (#2122).
    pub(crate) fn decider_passthrough_dsd(mode: &str, annonce: Option<bool>) -> bool {
        match mode {
            "pcm" => false,
            // `dop` n'est pas du passthrough : le renderer doit recevoir le DSD
            // emballe en trames PCM, pas le .dsf brut.
            "dop" => false,
            // Choix explicite : la parole de l'utilisateur, TOUJOURS.
            //
            // La version precedente cedait devant un « non » du Sink
            // (`annonce != Some(false)`, #2122). Le terrain l'a dementie :
            // l'Eversolo DMP-A8 annonce 392 formats dans son GetProtocolInfo,
            // AUCUN DSD — et joue le .dsf brut qu'on lui envoie. Un Sink qui
            // omet un format n'est pas un refus, meme quand il a l'air
            // exhaustif : une absence n'est pas une preuve.
            //
            // `native` n'est pas un reglage d'usine : quelqu'un l'a choisi,
            // pour CE renderer. Si la zone reste muette, c'est ce reglage
            // qu'il faut changer — et le journal le dit — pas le serveur qui
            // decide en silence de convertir.
            "native" => true,
            // `auto` : sans reponse claire, on prend le chemin sur.
            _ => annonce.unwrap_or(false),
        }
    }

    /// Le message d'échec de lecture, tel que l'utilisateur le lit.
    ///
    /// Un message d'échec doit permettre d'AGIR. Celui du DLNA — « Le renderer
    /// a acquitté Play mais joue toujours une autre source » — décrit
    /// fidèlement ce que l'appareil a fait, et c'est justement le problème :
    /// il désigne le matériel. L'utilisateur cherche du côté du renderer, du
    /// réseau, de son installation ; l'un d'eux a réinstallé son système
    /// entier (#2396).
    ///
    /// Or dans un cas précis le serveur SAIT, avant même d'envoyer, pourquoi
    /// cela ne marchera probablement pas : la zone est en DSD « natif », le
    /// Sink du lecteur a répondu qu'il ne lit pas le DSD, et on lui envoie le
    /// flux brut quand même. Ce choix-là n'est pas remis en cause — « natif »
    /// est un réglage explicite et des renderers lisent le DSD sans l'annoncer
    /// (Eversolo DMP-A8), cf. [`Self::decider_passthrough_dsd`]. Ce qui était
    /// faux, c'est le message : il accusait l'appareil au lieu de nommer le
    /// réglage et l'action qui le corrige.
    ///
    /// Hors de ce cas — le lecteur a dit oui, le sondage est resté muet, la
    /// zone est en `auto`, la source n'est pas du DSD brut — le message ne
    /// change pas d'un caractère. Accuser un réglage à tort serait la faute
    /// symétrique de celle qu'on corrige.
    ///
    /// Le préfixe « Output device error » est conservé dans tous les cas : la
    /// route de lecture s'en sert pour rendre un 503 « appareil indisponible »
    /// plutôt qu'un 500 (`tune-server/src/routes/playback.rs`), et
    /// [`command_may_have_landed`] cherche le marqueur de timeout SOAP à
    /// l'intérieur.
    pub(crate) fn message_echec_sortie(
        erreur: &str,
        dsd_mode: &str,
        annonce: Option<bool>,
        mime_type: &str,
    ) -> String {
        if dsd_mode == "native" && annonce == Some(false) && est_dsd_brut(mime_type) {
            return format!(
                "Output device error: le mode DSD de cette zone est réglé sur « natif » \
                 et ce lecteur annonce ne pas lire le DSD — le flux DSD brut lui a été \
                 envoyé quand même, et il ne l'a pas appliqué. Passer le mode DSD de la \
                 zone en « DoP » ou « PCM » pour lire ce fichier. \
                 (réponse du lecteur : {erreur})"
            );
        }
        format!("Output device error: {erreur}")
    }

    /// Le réglage DSD de la zone et ce que le lecteur en avait dit.
    ///
    /// Relu sur le chemin d'ERREUR seulement, et sans jamais resonder le
    /// réseau : le sondage a déjà eu lieu avant l'envoi ([`Self::sonder_dsd`])
    /// et seul un sondage CONCLUANT entre en cache. Un cache vide rend donc
    /// `None` — « je ne sais pas », jamais « non ». C'est précisément ce qui
    /// empêche d'imputer un échec à un réglage sur une absence de réponse.
    async fn contexte_dsd(&self, zone_id: i64, device_id: &str) -> (String, Option<bool>) {
        let mode = ZoneRepo::with_backend(self.db.clone()).get_dsd_mode(zone_id);
        let annonce = self
            .dsd_capabilities
            .lock()
            .await
            .get(device_id)
            .map(|cap| cap.supports_dsf || cap.supports_dff);
        (mode, annonce)
    }

    async fn should_dsd_passthrough(&self, zone_id: i64, device_id: &str) -> bool {
        let dsd_mode = ZoneRepo::with_backend(self.db.clone()).get_dsd_mode(zone_id);
        // Le sondage n'a de sens que si la decision peut en dependre : `pcm` et
        // `dop` tranchent sans lui, inutile d'aller sur le reseau.
        let annonce = match dsd_mode.as_str() {
            "pcm" | "dop" => None,
            _ => self.sonder_dsd(device_id).await,
        };
        let passthrough = Self::decider_passthrough_dsd(&dsd_mode, annonce);
        // La ligne qui manquait : ce qui part VRAIMENT sur le fil. Sans elle, le
        // seul événement DSD du journal était celui du DoP, qui ne décide de
        // rien sur une sortie réseau (#2122).
        info!(
            zone_id,
            device_id,
            dsd_mode = %dsd_mode,
            annonce_du_renderer = ?annonce,
            passthrough,
            "dsd_passthrough_decide"
        );
        if dsd_mode == "native" && annonce == Some(false) {
            tracing::info!(
                zone_id,
                device_id,
                "dsd_natif_sans_annonce_du_renderer — le Sink n'annonce pas de \
                 DSD, on envoie le flux brut QUAND MEME : « natif » est un \
                 reglage explicite, et des renderers lisent le DSD sans \
                 l'annoncer (Eversolo DMP-A8). Si la zone reste muette, passer \
                 le mode DSD de la zone en « auto » ou « pcm »."
            );
        }
        passthrough
    }

    /// Le renderer annonce-t-il savoir lire du DSD ?
    ///
    /// `Some(true)` / `Some(false)` sur un sondage CONCLUANT, `None` sinon —
    /// et la distinction compte : `None` ne veut pas dire « non ».
    ///
    /// Extrait pour etre partage par `native` et `auto`. Les deux posaient la
    /// meme question ; un seul la posait.
    async fn sonder_dsd(&self, device_id: &str) -> Option<bool> {
        // Seul un sondage CONCLUANT entre en cache. `probe_dsd_support` rend
        // `None` quand GetProtocolInfo a echoue ou que le Sink etait vide, et un
        // appareil qui n'est pas une sortie DLNA (ou qui a quitte la table)
        // n'est pas plus concluant. Mettre ces cas en cache epinglerait
        // l'appareil sur « pas de DSD » pour toute la vie du processus — un
        // echec passager juste apres la decouverte forcerait silencieusement le
        // transcodage PCM sur un renderer qui lit le DSD nativement, sans
        // recours autre qu'un redemarrage. Meme regle que `DlnaOutput::supports_mime`.
        let mut cache = self.dsd_capabilities.lock().await;
        if let Some(cap) = cache.get(device_id) {
            return Some(cap.supports_dsf || cap.supports_dff);
        }
        let cap = {
            let arc = { self.outputs.lock().await.get(device_id) };
            match arc {
                Some(output) => {
                    let locked = output.lock().await;
                    match locked
                        .as_any()
                        .downcast_ref::<crate::outputs::dlna::DlnaOutput>()
                    {
                        Some(dlna) => dlna.probe_dsd_support().await,
                        None => None,
                    }
                }
                None => None,
            }
        };
        cap.map(|cap| {
            let resultat = cap.supports_dsf || cap.supports_dff;
            cache.insert(device_id.to_string(), cap);
            resultat
        })
    }

    async fn resolve_uploaded_file(
        &self,
        file_path: &str,
        req: &PlayRequest,
    ) -> Result<ResolvedStream, String> {
        let path = std::path::Path::new(file_path);
        if !path.exists() {
            return Err(format!("uploaded file not found: {file_path}"));
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("wav")
            .to_lowercase();
        let format = crate::audio::formats::AudioFormat::from_extension(&ext);
        let meta = crate::metadata::try_read_metadata(path);
        let title = req
            .title
            .clone()
            .or_else(|| meta.as_ref().ok().and_then(|m| m.title.clone()))
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or("Unknown")
                    .to_string()
            });
        let artist = req
            .artist_name
            .clone()
            .or_else(|| meta.as_ref().ok().and_then(|m| m.artist.clone()));
        let album = req
            .album_title
            .clone()
            .or_else(|| meta.as_ref().ok().and_then(|m| m.album.clone()));
        let duration_ms = req
            .duration_ms
            .map(|d| d as u64)
            .or_else(|| meta.as_ref().ok().and_then(|m| m.duration_ms))
            .unwrap_or(0);
        let sample_rate = meta.as_ref().ok().and_then(|m| m.sample_rate);
        let bit_depth = meta.as_ref().ok().and_then(|m| m.bit_depth);
        let channels = meta.as_ref().ok().and_then(|m| m.channels).unwrap_or(2);

        let mime = format
            .as_ref()
            .map(|f| f.mime_type())
            .unwrap_or("audio/wav")
            .to_string();
        let file_size = std::fs::metadata(path).ok().map(|m| m.len());

        let info = StreamInfo {
            format: ext.clone(),
            mime_type: mime.clone(),
            sample_rate: sample_rate.unwrap_or(44100) as u32,
            bit_depth: bit_depth.unwrap_or(16),
            channels: channels as u16,
            file_size,
            duration_ms: Some(duration_ms as u64),
            ..Default::default()
        };

        let (session_id, tx, data_ready) = self.streamer.create_session(info, true, 128).await;
        let fp = file_path.to_string();
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            let file = std::fs::read(&fp);
            match file {
                Ok(data) => {
                    let _ = rt.block_on(tx.send(data));
                    data_ready.notify_one();
                }
                Err(e) => {
                    tracing::warn!(error = %e, "uploaded_file_read_failed");
                }
            }
        });

        let server_ip = self.server_ip();
        let stream_url = self.streamer.get_stream_url(&session_id, &server_ip, &ext);

        Ok(ResolvedStream {
            url: stream_url,
            stream_id: Some(session_id),
            title,
            artist,
            album,
            duration_ms: Some(duration_ms as i64),
            source: "upload".into(),
            mime_type: mime,
            sample_rate: sample_rate.map(|s| s as u32),
            bit_depth: bit_depth.map(|b| b as u32),
            channels: Some(channels as u32),
            origin_url: None,
            bitrate_kbps: None,
            cover_url: None,
            file_size,
        })
    }

    async fn resolve_stream(&self, req: &PlayRequest) -> Result<ResolvedStream, String> {
        if let Some(ref source) = req.source
            && source != "local"
        {
            // An out-of-library file dragged into the queue is stored as
            // source="upload" with source_id = the uploaded temp file path (see
            // queue_add). Every advance/jump/repeat funnels through resolve_stream,
            // so resolve it here — not only via the one-shot temp_file_path field —
            // otherwise it plays once but fails on queue advance (Sergio:
            // glisser-lire un fichier hors bibliothèque).
            if source == "upload" {
                let path = req
                    .source_id
                    .as_deref()
                    .ok_or("upload source requires source_id (file path)")?;
                return self.resolve_uploaded_file(path, req).await;
            }
            // `bandcamp` entre par la MÊME porte qu'un podcast ou une radio :
            // une URL distante déjà jouable, sans service enregistré derrière.
            // Sans cette ligne il tombait dans `resolve_streaming_url`, qui
            // cherche un service nommé « bandcamp » dans le registre et
            // échoue — c'est pour ça que la vue jouait dans l'onglet plutôt
            // que dans la zone (#1768).
            if source == "podcast" || source == "radio" || source == "upnp" || source == "bandcamp"
            {
                return self.resolve_direct_url(req).await;
            }
            return self.resolve_streaming_url(source, req).await;
        }

        self.resolve_local_track(req).await
    }

    /// Some radio servers (Icecast) answer HEAD with 400 Bad Request. A DLNA
    /// renderer that HEAD-probes the stream URL before playback then refuses to
    /// play it (Cyrille's Yamaha R-N2000A: the MP3 Icecast stations Radio
    /// Classique / TSF Jazz stay silent while AAC stations work). Returns false
    /// ONLY on a confirmed non-success HEAD, so we proxy just those; a transient
    /// network error stays `true` to avoid needlessly proxying working stations.
    async fn radio_head_ok(&self, url: &str) -> bool {
        let probe = url.replacen("https://", "http://", 1);
        match crate::http::client::shared()
            .head(&probe)
            .timeout(std::time::Duration::from_secs(4))
            .send()
            .await
        {
            Ok(resp) => resp.status().is_success(),
            Err(_) => true,
        }
    }

    /// Dereference an M3U/PLS *playlist* URL to its first real http(s) stream.
    ///
    /// Many stations are published as a small `.m3u`/`.pls` file whose body is
    /// just the actual stream URL(s) — e.g. `radioswissjazz.ch/live/mp3.m3u`
    /// contains a single line pointing at the Icecast stream. Playing the
    /// playlist URL directly feeds the playlist *text* to the audio decoder, so
    /// the level meter twitches on garbage but no sound comes out (Pascal,
    /// v0.9.21).
    ///
    /// Returns `Some(stream_url)` only when `url` is a playlist that
    /// dereferenced to a different http(s) URL; `None` for a direct media URL
    /// (no network hit — cheap extension gate first), for HLS `.m3u8` (that
    /// manifest IS the stream, consumed directly by the player), or on any
    /// fetch/parse failure — so the caller keeps the original URL.
    async fn resolve_playlist_url(&self, url: &str) -> Option<String> {
        let path = url
            .split(['?', '#'])
            .next()
            .unwrap_or(url)
            .to_ascii_lowercase();
        if !(path.ends_with(".m3u") || path.ends_with(".pls")) {
            return None;
        }
        let body = crate::http::client::shared()
            .get(url)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
            .ok()?
            .bytes()
            .await
            .ok()?;
        let inner = crate::library::m3u_parser::parse_m3u_content(&body, true)
            .into_iter()
            .map(|e| e.path)
            .find(|p| {
                let p = p.trim();
                p.starts_with("http://") || p.starts_with("https://")
            })?;
        let inner = inner.trim().to_string();
        if inner == url.trim() {
            return None; // playlist pointed back at itself — nothing gained
        }
        info!(playlist = %url, stream = %inner, "radio_playlist_dereferenced");
        Some(inner)
    }

    async fn resolve_direct_url(&self, req: &PlayRequest) -> Result<ResolvedStream, String> {
        let raw_url = req
            .source_id
            .as_deref()
            .ok_or("source_id (audio URL) required for podcast/radio playback")?;
        // A station is often published as an .m3u/.pls PLAYLIST file rather than a
        // direct stream. Dereference it to the real stream first, otherwise the
        // decoder is fed the playlist text and no sound plays (Pascal). Cheap for
        // a direct URL (extension gate, no network hit); keeps `raw_url` on any
        // failure. Applies to every downstream radio path (local and network).
        let resolved_playlist = self.resolve_playlist_url(raw_url).await;
        let audio_url: &str = resolved_playlist.as_deref().unwrap_or(raw_url);
        let title = req.title.clone().unwrap_or_else(|| "Episode".into());
        let artist = req.artist_name.clone();
        let album = req.album_title.clone();
        let cover_url = req.cover_url.clone();
        let duration_ms = req.duration_ms;
        let source = req.source.clone().unwrap_or_else(|| "podcast".into());
        // La qualité Bandcamp est LUE DANS L'URL, jamais déduite du nom du
        // service. L'écoute libre est du `mp3-128` ; un fichier ACHETÉ entre
        // par la même porte en `flac`, `alac` ou `mp3-320`, et l'étiqueter
        // « MP3 128 » serait un mensonge dans le sens le plus coûteux pour ce
        // logiciel (#2074). `None` quand l'URL ne nomme rien : on retombe
        // alors sur ce que Bandcamp sert sans session, sans rien affirmer de
        // plus.
        let bc_quality = (source == "bandcamp")
            .then(|| bandcamp_encoding(audio_url))
            .flatten()
            .and_then(|enc| bandcamp_quality(&enc));
        // Les URL de flux Bandcamp (`t4.bcbits.com/stream/<hash>/mp3-128/<id>`)
        // n'ont pas d'extension : `guess_mime_from_url` retomberait sur son
        // défaut, qui se trouve être le bon. On l'affirme plutôt que d'en
        // dépendre — si ce défaut changeait, la zone recevrait un MIME faux.
        let mime_type = if source == "bandcamp" {
            bc_quality
                .as_ref()
                .map(|q| q.mime_type)
                .unwrap_or("audio/mpeg")
        } else {
            guess_mime_from_url(audio_url)
        };
        let is_radio = source == "radio";
        let is_bandcamp = source == "bandcamp";

        let is_local_output = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("local:"));
        let is_oaat_output = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("oaat:") || id.starts_with("oaat-group:"));
        // Une zone navigateur n'a volontairement aucun `output_device_id` :
        // l'onglet est la sortie et tire lui-même `stream_url`. On doit donc
        // lire son type en base plutôt que déduire « aucune sortie » de
        // l'absence de périphérique (#2076, #2158). Cette propriété vaut pour
        // Bandcamp comme pour la radio dont l'EQ force désormais le proxy WAV.
        let is_browser_output = req.output_device_id.is_none()
            && ZoneRepo::with_backend(self.db.clone())
                .get(req.zone_id)
                .ok()
                .flatten()
                .and_then(|zone| zone.output_type)
                .as_deref()
                == Some("browser");

        // La sortie locale applique déjà l'EQ dans son callback : le refaire
        // ici colorerait le signal deux fois. OAAT, DLNA et navigateur
        // consomment en revanche le WAV construit par ce décodeur ; le profil
        // doit voyager jusqu'au moment où son format réel sera connu (#2063).
        // Un profil neutre ne force aucun transcodage inutile.
        let radio_eq_profile = if is_radio
            && !is_local_output
            && (is_oaat_output || req.output_device_id.is_some() || is_browser_output)
        {
            self.load_eq_profile(req.zone_id).filter(|profile| {
                crate::audio::eq::EqProcessor::new(profile, 44_100, 2).is_enabled()
            })
        } else {
            None
        };

        let (url, stream_id, out_mime, out_sr, out_bd, out_ch) = if is_radio
            && (is_local_output || is_oaat_output)
        {
            // Local/OAAT outputs cannot play compressed streams directly —
            // they expect raw PCM in a WAV container.  For radio (infinite
            // stream), we decode the HTTP stream progressively to PCM and
            // serve it as WAV through a streaming session.
            let wav_info = StreamInfo {
                format: "wav".into(),
                mime_type: "audio/wav".into(),
                sample_rate: 44100,
                bit_depth: 16,
                channels: 2,
                file_size: None,
                duration_ms: None,
                ..Default::default()
            };

            let (session_id, tx, data_ready, session) =
                self.streamer.create_radio_session(wav_info, 256).await;

            info!(
                source = "radio",
                url = %audio_url,
                "radio_decode_to_wav_for_local_output"
            );

            let radio_url = audio_url.to_string();
            // VU-mètres sur radio : forwarder de niveaux alimenté par le PCM
            // décodé du flux (le décodage-pour-niveaux fichier ne s'applique
            // pas à un live). Observateur pur, n'affecte pas le flux servi.
            let radio_levels_tx = if let Some(ref bus) = self.event_bus {
                let play_seq = self.playback.current_play_seq(req.zone_id).await;
                Some(spawn_paced_levels_forwarder(
                    bus.clone(),
                    self.playback.clone(),
                    req.zone_id,
                    play_seq,
                    0,
                ))
            } else {
                None
            };
            // Clone kept OUTSIDE the decode task: several of its exit paths
            // (consumer dropped, reconnect give-up) only log at debug!, so in
            // production the producer can die invisibly. The flag lets
            // resume() detect that state and re-play the station (#1629).
            let session_for_done = session.clone();
            // De quoi DIRE l'échec plutôt que de le laisser au journal.
            let err_bus = self.event_bus.clone();
            let err_zone = req.zone_id;
            let err_station = title.clone();
            tokio::spawn(async move {
                // Download + decode in a blocking thread since symphonia and
                // reqwest::blocking are both synchronous.
                let result = tokio::task::spawn_blocking(move || {
                    decode_radio_stream_to_pcm(
                        radio_url,
                        tx,
                        data_ready,
                        session,
                        if is_local_output {
                            None
                        } else {
                            radio_eq_profile.clone()
                        },
                        radio_levels_tx,
                    )
                })
                .await;

                // Whatever the exit path — clean end, error or panic — nothing
                // will produce PCM for this session anymore.
                session_for_done
                    .producer_done
                    .store(true, std::sync::atomic::Ordering::Relaxed);

                match result {
                    Ok(Ok(())) => {
                        debug!("radio_local_decode_stream_ended");
                    }
                    Ok(Err(e)) => {
                        warn!(error = %e, "radio_local_decode_failed");
                        emit_radio_playback_error(&err_bus, err_zone, &err_station, &e);
                    }
                    Err(e) => {
                        warn!(error = %e, "radio_local_decode_task_panic");
                        emit_radio_playback_error(
                            &err_bus,
                            err_zone,
                            &err_station,
                            "erreur interne du décodeur",
                        );
                    }
                }
            });

            let server_ip = self.server_ip();
            let stream_url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");
            (
                stream_url,
                Some(session_id),
                "audio/wav".to_string(),
                Some(44100u32),
                Some(16u32),
                Some(2u32),
            )
        } else if is_bandcamp && is_oaat_output {
            // Un endpoint OAAT ne consomme que du PCM en conteneur WAV : son
            // chemin HTTP le dit noir sur blanc (« Compressed formats fall
            // through to HTTP streaming where the orchestrator already decoded
            // them to WAV »). Lui pousser le mp3-128 de Bandcamp tel quel
            // donnerait un flux qu'il ne sait pas ouvrir — c'est-à-dire le
            // silence, exactement ce qu'on corrige.
            //
            // On réutilise la MÊME session de décodage que la radio sur OAAT,
            // qui tourne en production sur .18 : `decode_radio_stream_to_pcm`
            // décode un flux HTTP au fil de l'eau et se termine proprement à
            // la fin des octets — une piste finie n'est qu'un flux qui
            // s'arrête. Aucun chemin existant n'est modifié : la branche est
            // fermée sur `source == "bandcamp"`.
            let wav_info = StreamInfo {
                format: "wav".into(),
                mime_type: "audio/wav".into(),
                sample_rate: 44100,
                bit_depth: 16,
                channels: 2,
                file_size: None,
                duration_ms: None,
                ..Default::default()
            };
            let (session_id, tx, data_ready, session) =
                self.streamer.create_radio_session(wav_info, 256).await;
            info!(url = %audio_url, "bandcamp_decode_to_wav_for_oaat_output");
            let bc_url = audio_url.to_string();
            let bc_levels_tx = if let Some(ref bus) = self.event_bus {
                let play_seq = self.playback.current_play_seq(req.zone_id).await;
                Some(spawn_paced_levels_forwarder(
                    bus.clone(),
                    self.playback.clone(),
                    req.zone_id,
                    play_seq,
                    0,
                ))
            } else {
                None
            };
            let session_for_done = session.clone();
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    decode_radio_stream_to_pcm(bc_url, tx, data_ready, session, None, bc_levels_tx)
                })
                .await;
                session_for_done
                    .producer_done
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                match result {
                    Ok(Ok(())) => debug!("bandcamp_oaat_decode_stream_ended"),
                    Ok(Err(e)) => warn!(error = %e, "bandcamp_oaat_decode_failed"),
                    Err(e) => warn!(error = %e, "bandcamp_oaat_decode_task_panic"),
                }
            });
            let server_ip = self.server_ip();
            let stream_url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");
            (
                stream_url,
                Some(session_id),
                "audio/wav".to_string(),
                Some(44100u32),
                Some(16u32),
                Some(2u32),
            )
        } else if is_bandcamp
            && !is_local_output
            && (req.output_device_id.is_some() || is_browser_output)
        {
            // Sortie RÉSEAU (DLNA/OpenHome) ou navigateur. Bandcamp ne publie
            // ses flux qu'en HTTPS : un renderer DLNA ne sait pas ouvrir TLS,
            // tandis que le client web réécrit une URL tierce en chemin local
            // et reçoit alors du text/html au lieu du MP3 (#2076, #2158).
            //
            // On la sert donc par une session proxy locale, en clair, comme
            // les pistes Tidal/Qobuz (`create_proxy_session`). Les octets
            // passent verbatim : c'est du MP3 que tout renderer sait lire, il
            // n'y a rien à transcoder.
            //
            // Conteneur et MIME suivent l'encodage LU DANS L'URL, avec repli
            // sur le `mp3` de l'écoute libre : le proxy passe les octets tels
            // quels, donc annoncer `audio/mpeg` sur un FLAC acheté ferait
            // exactement le mislabel dont ce chemin se protège (#2074).
            let bc_codec = bc_quality.as_ref().map(|q| q.codec).unwrap_or("mp3");
            let info = StreamInfo {
                format: bc_codec.into(),
                mime_type: mime_type.to_string(),
                sample_rate: 44100,
                bit_depth: 16,
                channels: 2,
                file_size: None,
                duration_ms: duration_ms.map(|d| d as u64),
                ..Default::default()
            };
            let session_id = self
                .streamer
                .create_proxy_session(info, audio_url.to_string(), false)
                .await;
            let server_ip = self.server_ip();
            let stream_url = self
                .streamer
                .get_stream_url(&session_id, &server_ip, bc_codec);
            info!(
                url = %audio_url,
                browser = is_browser_output,
                codec = bc_codec,
                "bandcamp_proxy_for_network_or_browser_output"
            );
            (
                stream_url,
                Some(session_id),
                mime_type.to_string(),
                Some(44100u32),
                Some(16u32),
                Some(2u32),
            )
        } else if is_radio {
            // Network outputs (DLNA): check if the renderer supports the
            // radio stream format (typically AAC). If not, proxy + transcode
            // to WAV so the renderer can play it.
            // Passthrough ONLY when the URL carries an unambiguous,
            // renderer-supported extension (.mp3/.flac/.wav). Extension-less
            // Icecast mounts fall through guess_mime_from_url() to the default
            // "audio/mpeg", and .aac (ADTS) maps to "audio/mp4" — both are
            // mislabels. The renderer then opens a stream whose bytes don't
            // match the advertised protocolInfo, reports PLAYING and emits
            // SILENCE (Cyrille, Yamaha R-N2000A). Transcode every ambiguous
            // codec (.aac/.ogg/.opus/HLS/extension-less) to WAV so sound is
            // guaranteed; explicit .mp3/.flac stations still pass through with
            // no CPU/bandwidth cost.
            let url_path = audio_url.split(['?', '#']).next().unwrap_or(audio_url);
            let reliable_ext = {
                let p = url_path.to_lowercase();
                p.ends_with(".mp3") || p.ends_with(".flac") || p.ends_with(".wav")
            };
            // A radio stream bound to a specific DLNA renderer is ALWAYS
            // proxied+transcoded to WAV. Direct passthrough of an infinite
            // Icecast stream is unreliable: it carries no Content-Length and
            // may use ICY framing, so the renderer HEAD-probes, reports
            // PLAYING, then emits silence — even for an explicit .mp3 whose
            // HEAD returns 200 (Cyrille, Yamaha R-N2000A: Radio Classique
            // proxied → sound, TSF Jazz sent direct → silent + retry loop).
            // WAV is universally supported, so proxying guarantees sound at
            // low CPU/LAN cost. Only device-less network resolves (no HEAD to
            // gamble on) keep the extension-based passthrough.
            // Un EQ actif interdit le passthrough, même pour un MP3 explicite :
            // les octets compressés contourneraient entièrement le DSP. C'est
            // notamment le cas d'une zone navigateur, qui n'a aucun device_id
            // mais doit recevoir le WAV déjà égalisé par Tune (#2063).
            //
            // Une zone NAVIGATEUR n'a jamais droit au passthrough, EQ ou pas
            // (#2670). Le client web reecrit toute URL absolue en chemin
            // relatif — `browserPlay`, `u.pathname + u.search`, pour joindre
            // l'hote Tune plutot que l'IP annoncee par le serveur. Lui rendre
            // l'URL de la station fait donc demander `/tsfjazz-high.mp3` a
            // Tune, qui repond par son repli SPA : 200 `text/html`, sa propre
            // page. L'auditeur recoit une page web a la place du flux, et Tune
            // n'a rien a en dire puisqu'il n'a jamais ouvert le flux lui-meme :
            // le controle `non_audio_content_type` vit dans
            // `decode_radio_stream_to_pcm`, que ce chemin court-circuite.
            // C'est la MEME cause que #2076 / #2158, deja corrigee pour
            // Bandcamp quelques branches plus haut par un proxy local.
            //
            // La bascule ne coute rien de nouveau : une zone navigateur recoit
            // deja du WAV pour toute station au codec ambigu (.aac, .ogg, sans
            // extension), soit 44 des 51 entrees de l'annuaire au 28/08/2026.
            // Seules les rares URL en .mp3/.flac/.wav prenaient ce raccourci —
            // TSF Jazz en fait partie, et c'est la station signalee.
            let needs_proxy = req.output_device_id.is_some()
                || is_browser_output
                || !reliable_ext
                || radio_eq_profile.is_some();

            if needs_proxy {
                let wav_info = StreamInfo {
                    format: "wav".into(),
                    mime_type: "audio/wav".into(),
                    sample_rate: 44100,
                    bit_depth: 16,
                    channels: 2,
                    file_size: None,
                    duration_ms: None,
                    ..Default::default()
                };
                let (session_id, tx, data_ready, session) =
                    self.streamer.create_radio_session(wav_info, 256).await;
                info!(url = %audio_url, "radio_proxy_transcode_for_dlna");
                let radio_url = audio_url.to_string();
                // VU-mètres sur radio (DLNA) : forwarder de niveaux alimenté
                // par le PCM décodé. Observateur pur, n'affecte pas le flux.
                let radio_levels_tx = if let Some(ref bus) = self.event_bus {
                    let play_seq = self.playback.current_play_seq(req.zone_id).await;
                    Some(spawn_paced_levels_forwarder(
                        bus.clone(),
                        self.playback.clone(),
                        req.zone_id,
                        play_seq,
                        0,
                    ))
                } else {
                    None
                };
                // Même marquage que le chemin local/OAAT : resume() lit ce
                // drapeau pour savoir que plus rien n'alimente la session et
                // rejouer la station (#1629).
                let session_for_done = session.clone();
                // Même dette que le chemin local : l'échec restait au journal.
                let err_bus = self.event_bus.clone();
                let err_zone = req.zone_id;
                let err_station = title.clone();
                tokio::spawn(async move {
                    let result = tokio::task::spawn_blocking(move || {
                        decode_radio_stream_to_pcm(
                            radio_url,
                            tx,
                            data_ready,
                            session,
                            radio_eq_profile.clone(),
                            radio_levels_tx,
                        )
                    })
                    .await;
                    session_for_done
                        .producer_done
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    match result {
                        Ok(Ok(())) => debug!("radio_dlna_decode_stream_ended"),
                        Ok(Err(e)) => {
                            warn!(error = %e, "radio_dlna_decode_failed");
                            emit_radio_playback_error(&err_bus, err_zone, &err_station, &e);
                        }
                        Err(e) => {
                            warn!(error = %e, "radio_dlna_decode_task_panic");
                            emit_radio_playback_error(
                                &err_bus,
                                err_zone,
                                &err_station,
                                "erreur interne du décodeur",
                            );
                        }
                    }
                });
                let server_ip = self.server_ip();
                let stream_url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");
                (
                    stream_url,
                    Some(session_id),
                    "audio/wav".to_string(),
                    Some(44100u32),
                    Some(16u32),
                    Some(2u32),
                )
            } else {
                // Renderer supports the format — send direct URL.
                // Downgrade https→http since DLNA renderers can't do TLS.
                let direct_url = if audio_url.starts_with("https://") {
                    audio_url.replacen("https://", "http://", 1)
                } else {
                    audio_url.to_string()
                };
                (direct_url, None, mime_type.to_string(), None, None, None)
            }
        } else if is_bandcamp {
            // Sortie LOCALE (ou aucune sortie encore liée). `LocalOutput`
            // télécharge et décode lui-même un flux HTTP compressé
            // (`local_audio_non_wav_stream_detected_decoding`) : rien à
            // interposer, et un transcodage ne ferait que dégrader deux fois.
            //
            // La résolution est AFFIRMÉE plutôt que laissée au défaut
            // (44,1 kHz / 16 bits est ce que le mp3-128 de Bandcamp décode) :
            // le chemin du signal doit annoncer « MP3 — Avec perte », et non
            // hériter d'une valeur par défaut qu'on n'aurait pas choisie.
            (
                audio_url.to_string(),
                None,
                mime_type.to_string(),
                Some(44100u32),
                Some(16u32),
                Some(2u32),
            )
        } else {
            // Media-server / podcast direct URL. Carry the real resolution the
            // client passed from the DIDL res@ attributes (e.g. 24-bit ALAC)
            // instead of letting the signal path default to 44.1kHz/16bit and
            // mislabel a hi-res ALAC as lossy AAC (Yves, NAS).
            (
                audio_url.to_string(),
                None,
                mime_type.to_string(),
                req.sample_rate,
                req.bit_depth.map(|b| b as u32),
                None,
            )
        };

        // Every branch above may have replaced the station/enclosure URL with one
        // of our proxy endpoints (WAV transcode for renderers that need it, or a
        // local decode session). Keep the original so an output that wants the
        // bytes as published — and the ICY metadata the proxy drops — can ask
        // for them. `None` when we are handing out the upstream URL unchanged.
        let origin_url = (url != audio_url).then(|| audio_url.to_string());

        Ok(ResolvedStream {
            url,
            mime_type: out_mime,
            title,
            artist,
            album,
            duration_ms,
            source,
            cover_url,
            stream_id,
            file_size: None,
            sample_rate: out_sr,
            bit_depth: out_bd,
            channels: out_ch,
            origin_url,
            // Le débit voyage jusqu'à la zone quelle que soit la sortie prise
            // ci-dessus — locale, WAV décodé pour OAAT, ou proxy MP3 pour un
            // renderer réseau : les trois portent le MÊME flux source, et
            // c'est LUI que le chemin du signal doit annoncer (#2074).
            bitrate_kbps: bc_quality.as_ref().and_then(|q| q.bitrate_kbps),
        })
    }

    async fn resolve_local_track(&self, req: &PlayRequest) -> Result<ResolvedStream, String> {
        let track_id = req.track_id.ok_or("no track_id for local playback")?;
        let repo = TrackRepo::with_backend(self.db.clone());
        let mut track = repo
            .get(track_id)
            .map_err(|e| e.to_string())?
            .ok_or("track not found")?;

        let file_path = track.file_path.ok_or("track has no file_path")?;

        // The DB row can outlive the file (moved/deleted external drive, stale
        // scan, duplicate compilation entry pointing at an old path). Without
        // this check the missing file is only discovered later, inside the
        // spawned streaming transcode task (transcode_streaming_decode_failed),
        // AFTER output_play_sent — so the track "plays" silently with no error
        // surfaced and the queue can stall (JP: two "Studio 105" entries, the
        // one pointing at a moved X:\…\.flac played no sound). Fail fast here so
        // play() returns a clean error the client shows, instead of streaming
        // silence.
        //
        // The DB stores paths NFC-normalized (scanner), but a file is opened by
        // its raw on-disk bytes: on a Samba/CIFS or macOS-origin share whose
        // filenames are NFD (decomposed), the stored NFC path misses the real
        // file and a present, listable track reads as "missing" (Dominique
        // Comet, 0.9.48 after a rescan rewrote paths to NFC). Resolve the true
        // on-disk spelling (stored form, then NFD) before giving up.
        let file_path = match resolve_existing_local_path(&file_path) {
            Some(resolved) => resolved,
            None => {
                warn!(track_id, file = %file_path, "local_track_file_missing");
                return Err(format!("file_not_found:{file_path}"));
            }
        };

        let fmt = track.format.unwrap_or_else(|| "flac".into());
        let source_format = AudioFormat::from_extension(&fmt);
        // DSD is 1-bit at MHz rates. When the DB row is missing audio props
        // (lofty returns None for many .dsf/.dff files), fall back to DSD64
        // defaults, NOT the PCM 44100/16 defaults — otherwise a native-DSD track
        // played to a DSD-capable renderer shows "44.1 kHz / 16 bit" in the
        // signal path / now-playing chip (Benjithom, HiFi Rose RS130), and the
        // DSD→PCM transcode-fallback rate math is fed the wrong input rate.
        let is_dsd_source = source_format == Some(AudioFormat::Dsd);

        // Play-time duration backfill. A scan that timed out on slow storage
        // (NAS: Pierre M, Yacine) falls back to filename-only metadata with
        // duration_ms = 0, and DSD/other files lofty can't read a duration for
        // also land at 0 in the DB. A 0 is quietly corrosive: the poller reads
        // now_playing.duration_ms (= the DB value) verbatim, and at 0 it loses
        // gapless arming, the position-past-end fast advance, BOTH wall-clock
        // advance nets, prefetch AND crossfade — so the queue stalls or cuts on
        // that track. Recover the real duration now and persist it so the track
        // self-heals for every later read. DSD is read from the header because
        // lofty (which get_duration uses) is exactly what returned 0 for it.
        if track.duration_ms <= 0 {
            if let Some(ms) = probe_local_duration_ms(&file_path, source_format).await {
                track.duration_ms = ms;
                let repo2 = TrackRepo::with_backend(self.db.clone());
                tokio::task::spawn_blocking(move || {
                    if let Err(e) = repo2.update_duration(track_id, ms) {
                        warn!(track_id, error = %e, "play_time_duration_persist_failed");
                    }
                });
                info!(track_id, duration_ms = ms, "play_time_duration_backfilled");
            }
        }

        let sample_rate = track
            .sample_rate
            .unwrap_or(if is_dsd_source { 2_822_400 } else { 44100 })
            as u32;
        let bit_depth = track
            .bit_depth
            .unwrap_or(if is_dsd_source { 1 } else { 16 }) as u16;
        let channels = track.channels as u16;

        // Determine the output type and max_sample_rate for this zone.
        let zone = ZoneRepo::with_backend(self.db.clone())
            .get(req.zone_id)
            .ok()
            .flatten();
        let zone_output_type = zone.as_ref().and_then(|z| z.output_type.clone());
        // Quirks catalogue (marque+modèle choisis par l'utilisateur pour la zone).
        // Additif : n'a d'effet que si l'utilisateur a explicitement sélectionné
        // un modèle catalogué. Sinon profil neutre (aucun changement).
        let device_quirks = crate::device_catalog::resolve_zone_quirks(&self.db, req.zone_id);
        // Le plafond catalogue se combine en `min` avec l'override de zone : il
        // ne peut que rendre la contrainte plus stricte, jamais l'assouplir.
        let zone_max_sample_rate = crate::device_catalog::combine_max_sample_rate(
            zone.as_ref().and_then(|z| z.max_sample_rate),
            device_quirks.max_sample_rate,
        );

        let is_oaat_output = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("oaat:") || id.starts_with("oaat-group:"));
        // OAAT endpoints: transcode to WAV for reliable bit-perfect playback.
        // Always transcode, even WAV sources, to normalise EXTENSIBLE/FLOAT
        // variants into simple PCM that the endpoint can reliably parse.
        let oaat_needs_wav = is_oaat_output && source_format.is_some();

        // Local output (cpal) has a simple WAV parser that only understands
        // standard PCM (format tag 1).  Real-world WAV files can use
        // WAVE_FORMAT_EXTENSIBLE (0xFFFE), IEEE_FLOAT (3), or have extra
        // metadata chunks that shift the data offset beyond the parser's
        // 4096-byte header buffer.  Feeding such files as passthrough causes
        // white noise because the byte layout doesn't match what the parser
        // expects (wrong bit depth, wrong data offset, or float-as-integer).
        //
        // Fix: ALWAYS transcode through symphonia for local output, even when
        // the source is already WAV.  Symphonia handles all WAV variants and
        // produces normalised integer PCM.  The HTTP stream handler then
        // prepends a simple 44-byte PCM header that the local parser handles
        // correctly.  The overhead is negligible (memcpy, no re-encoding).
        let is_local_output = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("local:"));
        let local_needs_wav = is_local_output && source_format.is_some();

        // Calculé ici, et non plus après la branche DoP : celle-ci en a besoin
        // pour servir du DoP à un renderer réseau (#1772). Ne dépend que de
        // `zone_output_type`, connu bien plus haut.
        let is_network_output = matches!(
            zone_output_type.as_deref(),
            Some("dlna")
                | Some("openhome")
                | Some("chromecast")
                | Some("bluos")
                | Some("squeezebox")
                | Some("slimproto")
        );

        // DSD en DoP (DSD over PCM), c'est-à-dire du DSD transporté dans des
        // trames PCM 24 bits au seizième du débit.
        //
        // Deux cas, et le second manquait (#1772, Marco Polo, Wiim Pro → DAC
        // Denafrips) :
        //
        //  - sortie locale : « natif » comme « dop » passent par ici, la carte
        //    son ne sachant pas recevoir de DSD autrement ;
        //  - renderer réseau : uniquement sur choix EXPLICITE « dop ». Le
        //    lecteur réseau qui ne sait pas lire un .dsf sait souvent lire le
        //    DoP — c'est ce que fait MinimServer, que le testeur a comparé.
        //
        // Avant ce correctif, `"dop"` n'était comparé qu'ici, sous le garde
        // `is_local_output` : sur un renderer, le réglage tombait dans le
        // fourre-tout de `should_dsd_passthrough`, était traité comme « auto »,
        // et le Wiim n'annonçant pas le DSF, le serveur transcodait en PCM. Le
        // DAC recevait donc du WAV 176,4 kHz — le débit DoP du DSD64, ce qui
        // rendait le symptôme parfaitement trompeur.
        //
        // Le DoP réseau est plus sûr que le local : les octets partent par HTTP
        // sans passer par le rappel cpal, donc ni le volume ni le ReplayGain ne
        // peuvent détruire les marqueurs (cf. le grésillement de Cyrille).
        let dsd_mode = if source_format == Some(AudioFormat::Dsd) {
            ZoneRepo::with_backend(self.db.clone()).get_dsd_mode(req.zone_id)
        } else {
            String::new()
        };
        let dop_requested = dop_requested(is_local_output, is_network_output, &dsd_mode);

        // Un mode « auto » qui ne fait rien d'automatique, et qui se taisait.
        //
        // `dop_requested` ne reconnaît que `"native"` et `"dop"` — et en réseau,
        // `"dop"` seul. Le mode par DÉFAUT, `"auto"`, ne produit donc JAMAIS de
        // DoP, nulle part. Une piste DSD y part en PCM, ce qui est un choix
        // défendable, mais rien ne le disait : ni à l'écran, ni au journal.
        //
        // Conséquence vécue : un testeur dont un autre serveur lit le même
        // fichier sur le même DAC conclut que Tune « bloque sur le DSD », et
        // nous cherchons un défaut de lecture là où il n'y a qu'un réglage par
        // défaut trompeur (Tades, Hifiman Serenade, #1657). Une ligne de journal
        // aurait suffi à le voir sans lui poser la question.
        //
        // Tracé ici plutôt qu'en amont : c'est le seul endroit qui connaisse à
        // la fois le format de la source, le type de sortie et le mode réglé.
        //
        // Le nom de l'événement disait « sera converti en PCM ». C'était faux sur
        // une sortie réseau : ici, seul le DoP est écarté. La conversion, elle,
        // dépend de `should_dsd_passthrough`, plus bas — et en mode « native »
        // elle n'avait PAS lieu. Le journal annonçait donc du PCM pendant qu'une
        // URL `.dsd` partait sur le fil, ce qui a coûté trois diagnostics faux
        // (#2122). L'événement dit maintenant ce qu'il sait vraiment ; la
        // décision de conversion est tracée là où elle est prise.
        if source_format == Some(AudioFormat::Dsd) && !dop_requested {
            info!(
                zone_id = req.zone_id,
                dsd_mode = %dsd_mode,
                is_local_output,
                is_network_output,
                "dsd_dop_not_requested"
            );
        }

        if source_format == Some(AudioFormat::Dsd) {
            if dop_requested {
                // La cadence et le nombre de canaux se lisent DANS LE FICHIER,
                // pas dans la base.
                //
                // L'en-tête WAV décrivait la ligne `tracks` pendant que la
                // charge utile sortait de `parse_dsf`/`parse_dff` : deux
                // sources qui n'ont aucune raison de coïncider. Un écart d'un
                // canal désaligne chaque mot de 24 bits, le marqueur DoP ne
                // tombe plus sur l'octet de poids fort, le DAC ne verrouille
                // pas en DSD et joue le train DSD comme du PCM — c'est-à-dire
                // du bruit blanc (Marco Polo, Wiim Pro, #1894). Un écart de
                // cadence annonce un débit que le renderer n'appliquera pas.
                //
                // Le fichier est la seule source qui décrit ce qui part
                // réellement sur le fil, et c'est la même que celle dont
                // l'encodeur se sert (`decode_dsd_to_dop_streaming`). La base
                // ne sert plus que de repli si l'en-tête est illisible.
                let dsd_probe = {
                    let ext = std::path::Path::new(&file_path)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("dsf")
                        .to_lowercase();
                    if ext == "dff" {
                        crate::audio::dff::parse_dff(&file_path)
                            .ok()
                            .map(|i| (i.sample_rate, i.channels))
                    } else {
                        crate::audio::dsf::parse_dsf(&file_path)
                            .ok()
                            .map(|i| (i.sample_rate, i.channels))
                    }
                };
                if dsd_probe.is_none() {
                    warn!(
                        path = %file_path,
                        "dsd_header_unreadable_falling_back_to_db_metadata"
                    );
                }
                let (dsd_rate, dop_channels) = dop_wire_params(
                    dsd_probe,
                    track.sample_rate.map(|v| v as u32),
                    track.channels as u32,
                );
                let dop_rate = crate::audio::dsd_to_dop::DsdToDoP::dop_rate(dsd_rate);
                // Réutilise le plafond déjà combiné avec le quirk catalogue.
                let zone_max_sr = zone_max_sample_rate;
                if let Some(max_sr) = zone_max_sr {
                    if dop_rate > max_sr {
                        info!(
                            dsd_rate,
                            dop_rate, max_sr, "dsd_dop_rate_exceeds_zone_max_falling_back_to_pcm"
                        );
                        // Fall through to normal DSD→PCM transcode path
                    }
                }
                if zone_max_sr.is_none_or(|max_sr| dop_rate <= max_sr) {
                    // `dop_channels` vient de `dop_wire_params`, calculé plus
                    // haut avec la cadence : même source, même raison.

                    let wav_info = StreamInfo {
                        format: "wav".into(),
                        mime_type: "audio/wav".into(),
                        sample_rate: dop_rate,
                        bit_depth: 24,
                        channels: dop_channels,
                        file_size: None,
                        duration_ms: Some(track.duration_ms as u64),
                        ..Default::default()
                    };

                    let (session_id, tx, data_ready) =
                        self.streamer.create_session(wav_info, true, 128).await;

                    info!(
                        file = %file_path,
                        dsd_rate,
                        dop_rate,
                        channels = dop_channels,
                        sortie = if is_local_output { "locale" } else { "réseau" },
                        "dsd_dop_streaming"
                    );

                    let fp = file_path.clone();
                    let ext = std::path::Path::new(&fp)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("dsf")
                        .to_lowercase();
                    tokio::task::spawn_blocking(move || {
                        // Send WAV header first
                        let wav_hdr =
                            crate::audio::wav::build_wav_header(dop_channels, dop_rate, 24);
                        let rt = tokio::runtime::Handle::current();
                        let _ = rt.block_on(tx.send(wav_hdr.to_vec()));
                        data_ready.notify_one();

                        let mut first = false;
                        match crate::audio::decode::decode_dsd_to_dop_streaming(
                            &fp, &ext, tx, 65536, &mut first, &None, &rt,
                        ) {
                            Ok(_) => tracing::debug!("dsd_dop_stream_complete"),
                            Err(e) => tracing::warn!(error = %e, "dsd_dop_stream_failed"),
                        }
                    });

                    let server_ip = self.server_ip();
                    let stream_url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");

                    return Ok(ResolvedStream {
                        url: stream_url,
                        stream_id: Some(session_id),
                        title: track.title.clone(),
                        artist: track.artist_name.clone(),
                        album: track.album_title.clone(),
                        duration_ms: Some(track.duration_ms),
                        source: "local".into(),
                        mime_type: "audio/wav".into(),
                        sample_rate: Some(dop_rate),
                        bit_depth: Some(24),
                        channels: Some(dop_channels as u32),
                        origin_url: None,
                        bitrate_kbps: None,
                        cover_url: self.resolve_cover_url(track.cover_path.as_deref()),
                        file_size: None,
                    });
                } // end dop_rate <= max check
            }
        }

        // Transcode exotic formats (AIFF, DSD, WavPack, APE, ALAC, WMA) for network outputs
        // that receive a URL and play it directly. FLAC, WAV, MP3, AAC pass through as-is.
        // (`is_network_output` est calculé plus haut, la branche DoP en a besoin.)

        // Browser (Web Audio) zones pull the file themselves via <audio> and can
        // only decode the mainstream web codecs (FLAC/MP3/AAC/WAV/Ogg/Opus). An
        // exotic source — above all DSD — is otherwise served RAW (no network/
        // local output claims it, so nothing forces a transcode) and the <audio>
        // element is handed bytes it can't play, staying SILENT (Reivax66, local
        // DSD album on the "Cet ordinateur" zone, 0.9.44). Decode those to PCM/WAV
        // here, mirroring the streaming arm which already serves WAV to browser
        // zones. Codecs a browser plays natively stay direct (no regression).
        let is_browser_output = zone_output_type.as_deref() == Some("browser");
        let browser_needs_wav = is_browser_output
            && matches!(
                source_format,
                Some(AudioFormat::Dsd)
                    | Some(AudioFormat::WavPack)
                    | Some(AudioFormat::Ape)
                    | Some(AudioFormat::Wma)
                    | Some(AudioFormat::Aiff)
                    | Some(AudioFormat::Alac)
            );

        // DSD native passthrough: skip transcode when the renderer supports DSD natively.
        let dsd_passthrough = if source_format == Some(AudioFormat::Dsd) && is_network_output {
            let did = req
                .output_device_id
                .as_deref()
                .or(zone.as_ref().and_then(|z| z.output_device_id.as_deref()))
                .unwrap_or("");
            self.should_dsd_passthrough(req.zone_id, did).await
        } else {
            false
        };

        // ALAC native passthrough (opt-in per zone): serve the ALAC file
        // straight to a renderer that decodes it, instead of transcoding to
        // FLAC — bit-perfect and zero CPU. Off by default because ALAC and AAC
        // share the audio/mp4 MIME, so it can't be auto-detected safely.
        // LPCM override: a zone set to serve WAV/LPCM must transcode (to strip
        // the renderer's ALAC decoder quirks — e.g. LHC-56 pops at start), so it
        // takes precedence over ALAC passthrough.
        let dlna_lpcm =
            is_network_output && ZoneRepo::with_backend(self.db.clone()).get_dlna_lpcm(req.zone_id);
        // Opt-in per zone: serve genuine 24-bit WAV instead of the 16-bit LPCM
        // fallback. Only offered in the UI for renderers that advertise
        // `audio/L24` (capability probe). Forces the WAV path exactly like
        // `dlna_lpcm`, but the DIDL drops the 16-bit-only `DLNA.ORG_PN=LPCM`
        // profile (see didl::dlna_flags_for_mime_bd) so a strict renderer no
        // longer maps the stream back to 16-bit and reads misaligned samples
        // (the #1137 silence class). Only meaningful when the source is deeper
        // than 16-bit; a 16-bit source keeps the plain LPCM path.
        // `bit_depth` vient de la ligne `tracks`, et pour un ALAC elle peut y
        // être absente : la profondeur n'est lisible que par une sonde
        // Symphonia sur le cookie magique (`probe_m4a_props`), arrivée après
        // coup. Une piste scannée avant — ou dont la sonde a échoué — porte
        // alors le défaut `16`, et l'opt-in 24 bits ne s'arme JAMAIS, quel que
        // soit le réglage : coché, vérifié, sans effet (Yves, ALAC 24/96, #1654).
        //
        // Sonder le fichier lève l'ambiguïté, et le coût est nul en pratique :
        // la sonde ne tourne que si la zone a EXPLICITEMENT demandé le 24 bits
        // (opt-in rare) et que la base annonce 16 ou moins sur un conteneur qui
        // sait porter davantage. Un vrai ALAC 16 bits répond 16 et rien ne
        // change.
        let wav24_opt_in = is_network_output
            && ZoneRepo::with_backend(self.db.clone()).get_dlna_wav24(req.zone_id);
        let bit_depth_wire = if wav24_opt_in && bit_depth <= 16 {
            profondeur_sondee_si_la_base_ignore(&file_path, source_format).unwrap_or(bit_depth)
        } else {
            bit_depth
        };
        let dlna_wav24 = wav24_opt_in && bit_depth_wire > 16;
        // Both WAV overrides force a transcode away from FLAC/ALAC passthrough.
        //
        // …SAUF sur une source FLAC dont la zone demande explicitement le FLAC
        // natif. Le forçage WAV existe pour contourner le décodeur ALAC du
        // renderer (LHC-56 qui claque au démarrage, cf. le commentaire de
        // `dlna_lpcm` ci-dessus) : l'appliquer aussi au FLAC est un dommage
        // collatéral, jamais l'objectif.
        //
        // Sans cette exception, les deux réglages se contredisent et c'est le
        // WAV qui gagne en silence : Yves a lu un FLAC transcodé en WAV alors
        // que « FLAC natif » était coché, et en a conclu que Tune gardait en
        // mémoire les réglages du morceau précédent (forum #1437). Les deux
        // cases décrivent en fait deux sources différentes, et peuvent donc
        // coexister — l'ALAC part en WAV, le FLAC reste du FLAC.
        //
        // L'exception exige l'opt-in `dlna_native_flac` : sans lui, une source
        // FLAC continue de suivre le forçage, ce dont ont besoin les renderers
        // qui ne savent pas lire le FLAC.
        let dlna_force_wav = wav_override_applies(
            dlna_lpcm || dlna_wav24,
            source_format == Some(AudioFormat::Flac),
            is_network_output
                && ZoneRepo::with_backend(self.db.clone()).get_dlna_native_flac(req.zone_id),
        );
        // Opt-in per zone: cap output to 16-bit. Some renderers advertise
        // `audio/flac` (so Tune sends hi-res FLAC/ALAC direct) but only decode
        // 16-bit internally — 24-bit direct plays SILENCE (Ruark R3, Yves #1137).
        // Forces a 16-bit downconvert (kept as FLAC) instead of direct
        // passthrough, without regressing renderers that genuinely play 24-bit.
        // Only meaningful when the source is deeper than 16-bit.
        // Flag zone `dlna_cap_16bit` OR quirk catalogue `force_16bit` (additif :
        // le quirk ne peut que l'activer, jamais le désactiver — Ruark R3 #1137).
        let dlna_cap_16bit = is_network_output
            && bit_depth > 16
            && (ZoneRepo::with_backend(self.db.clone()).get_dlna_cap_16bit(req.zone_id)
                || device_quirks.force_16bit);
        let alac_passthrough = source_format == Some(AudioFormat::Alac)
            && is_network_output
            && !dlna_force_wav
            && !dlna_cap_16bit
            && ZoneRepo::with_backend(self.db.clone()).get_alac_passthrough(req.zone_id);
        // Même mécanique pour l'AAC (Marco Polo, #1424) : un Marantz SR7009 ou
        // un Denon RC12 le décodent nativement, et le transcoder en FLAC ne fait
        // que retarder le premier son et consommer du processeur — l'AAC étant
        // déjà compressé avec perte, le transcodage n'apporte aucune qualité.
        //
        // Pas de garde `dlna_cap_16bit` ici, contrairement à l'ALAC : ce plafond
        // vise les sources plus profondes que 16 bits, ce qu'un AAC n'est jamais.
        // `dlna_force_wav` reste respecté — un renderer qui exige du LPCM le
        // dit, et son exigence prime sur une préférence.
        let aac_passthrough = source_format == Some(AudioFormat::Aac)
            && is_network_output
            && !dlna_force_wav
            && ZoneRepo::with_backend(self.db.clone()).get_aac_passthrough(req.zone_id);

        // Chromecast's Default Media Receiver decodes a narrower set than most
        // DLNA renderers — notably it cannot play AIFF (which DLNA plays
        // direct). Serving AIFF direct to a Cast device fails the LOAD, so the
        // track never leaves position 0; auto-advance then skips to the next
        // track every few seconds and the shuffle-all queue "resets" endlessly,
        // never becoming audible (forum #1210, Mika, BeoPlay A9 via CAST).
        let is_chromecast = zone_output_type.as_deref() == Some("chromecast");
        let needs_transcode_for_output = is_network_output
            && !dsd_passthrough
            && !alac_passthrough
            && !aac_passthrough
            && source_format.as_ref().is_some_and(|f| {
                if is_chromecast {
                    f.needs_transcode_for_chromecast()
                } else {
                    f.needs_transcode_for_dlna()
                }
            });

        // DLNA format negotiation: if the output will be FLAC (either source
        // is FLAC, or source needs transcode and target is FLAC), check that
        // the renderer supports audio/flac. Otherwise force WAV (LPCM).
        let is_dlna = zone_output_type.as_deref() == Some("dlna");
        let will_be_flac = source_format == Some(AudioFormat::Flac)
            || (needs_transcode_for_output
                && source_format
                    .map(|f| f.dlna_transcode_target() == AudioFormat::Flac)
                    .unwrap_or(false));
        let dlna_needs_wav = if is_dlna && will_be_flac {
            let did = req
                .output_device_id
                .as_deref()
                .or(zone.as_ref().and_then(|z| z.output_device_id.as_deref()))
                .unwrap_or("");
            if dlna_force_wav {
                // User forces WAV for this zone (16-bit LPCM via `dlna_lpcm`, or
                // genuine 24-bit via `dlna_wav24`): skips the slow native FLAC
                // encoder for hi-res AND avoids a renderer whose ALAC decoder
                // pops at start (Yves, LHC-56). Takes precedence over the FLAC
                // override below.
                true
            } else if did.is_empty() {
                false
            } else if ZoneRepo::with_backend(self.db.clone()).get_dlna_native_flac(req.zone_id) {
                // User forces native FLAC for this zone: some renderers decode
                // FLAC but never advertise it (Marco's Denon Ceol N12 returns an
                // empty GetProtocolInfo Sink), so protocol negotiation wrongly
                // falls back to WAV. Honour the override and send FLAC.
                false
            } else {
                !self.dlna_supports_mime(did, "audio/flac").await
            }
        } else {
            false
        };

        // Downsample if the zone has a max_sample_rate cap and the source
        // exceeds it. For DSD, `sample_rate` is the raw DSD bit rate (MHz), so
        // this uses the PCM *output* rate for the comparison and never
        // downsamples a native DSD passthrough — otherwise a capped zone would
        // silently turn passthrough into a full DSD→PCM transcode (100s decode,
        // transcode_timeout_120s, album cutoff on the HiFi Rose RS130).
        let needs_downsample = crate::audio::formats::needs_downsample_for_cap(
            source_format,
            sample_rate,
            zone_max_sample_rate,
            dsd_passthrough,
        );
        // Un égaliseur ACTIVÉ sur la zone doit s'entendre : en passthrough
        // réseau (FLAC servi brut à la Beoplay A9), l'EqProcessor n'était
        // jamais appliqué — profil « appliqué » côté UI, zéro effet audible
        // (Mika, forum #1216). Activer l'EQ est un choix explicite de
        // traitement (les puristes ont le mode PURE, qui désactive ceci via
        // load_eq_processor→None) : on force alors le chemin transcodé, où
        // l'EQ est déjà branché. Jamais sur un passthrough DSD/ALAC voulu.
        // Les zones NAVIGATEUR tirent aussi le fichier brut via <audio> (FLAC
        // local servi direct) : même trou que #1216, mesuré sur .18 — deux
        // captures du flux EQ ±12 dB strictement identiques (md5). L'EQ y
        // force donc aussi le transcodage.
        // Une sortie PULL hors dépôt — `diretta`, `oaat` — va chercher le flux
        // elle-même et n'est ni « réseau » ni « navigateur » au sens ci-dessus.
        // Elle recevait donc le fichier brut : même trou que #1216, une
        // troisième fois (Eric, forum : égaliseur sans effet vers un renderer
        // Diretta). La sortie LOCALE est exclue — elle passe déjà par le
        // transcodage dès que le format source est connu (`local_needs_wav`).
        let is_pull_dsp_output = pull_output_needs_dsp_transcode(
            zone_output_type.as_deref(),
            is_local_output,
            is_oaat_output,
            source_format,
        );
        let eq_forces_transcode = (is_network_output || is_browser_output || is_pull_dsp_output)
            && !dsd_passthrough
            && !alac_passthrough
            && (self.zone_has_active_eq(req.zone_id)
                || self.zone_has_active_ir(req.zone_id)
                // ReplayGain scales the samples, so it lives in the same place
                // as the EQ — and would be discarded in the same way on a
                // passthrough. Enabling it is an explicit choice of processing;
                // PURE zones are excluded upstream.
                || self.zone_replaygain_changes_audio(req.zone_id, req.track_id));
        // En navigateur, la sortie transcodée doit être du WAV : un FLAC
        // ré-encodé à la volée n'a pas de seektable et cale le <audio> sur les
        // Range (#1168) — même règle que le bras streaming.
        let browser_needs_wav = browser_needs_wav || (is_browser_output && eq_forces_transcode);

        let needs_transcode = needs_transcode_for_output
            || oaat_needs_wav
            || local_needs_wav
            || browser_needs_wav
            || needs_downsample
            || dlna_needs_wav
            || eq_forces_transcode
            // 16-bit cap on a FLAC-direct renderer: force a transcode so the
            // hi-res FLAC is re-encoded at 16-bit instead of served direct
            // (silent on the Ruark R3, #1137). ALAC already transcodes because
            // the cap disables alac_passthrough above.
            || (dlna_cap_16bit && will_be_flac);
        if eq_forces_transcode && !needs_transcode_for_output && !dlna_needs_wav {
            info!(zone_id = req.zone_id, "eq_active_forcing_network_transcode");
        }

        let (
            session_id,
            out_mime,
            out_ext,
            resolved_file_size,
            resolved_sr,
            resolved_bd,
            resolved_ch,
        ) = if needs_transcode {
            let src_fmt = source_format.unwrap_or(AudioFormat::Flac);
            let target_fmt = if oaat_needs_wav || local_needs_wav || browser_needs_wav {
                AudioFormat::Wav
            } else if dlna_needs_wav {
                // Renderer doesn't support FLAC — transcode to WAV (LPCM)
                // which has a proper DLNA.ORG_PN=LPCM profile.
                AudioFormat::Wav
            } else if needs_downsample && !needs_transcode_for_output {
                // Only downsampling — keep the same lossless format
                AudioFormat::Flac
            } else if is_chromecast && src_fmt == AudioFormat::Aiff {
                // AIFF → FLAC for Chromecast (Cast decodes FLAC up to
                // 24-bit/96k, but not AIFF). dlna_transcode_target(Aiff) is a
                // no-op (Aiff→Aiff) meant for DLNA, so it must be overridden
                // here or the Cast device would be fed AIFF again (#1210).
                AudioFormat::Flac
            } else if src_fmt == AudioFormat::Dsd && is_network_output {
                // DSD → network renderer: stream as progressive WAV/LPCM instead
                // of a blocking pre-transcode to a FLAC file.
                //
                // DSD→FLAC is the slowest transcode (74–86s for a track). The
                // FLAC path takes `use_file_transcode` below, which decodes AND
                // encodes the WHOLE file to /tmp BEFORE serving a single byte —
                // so a renderer that can't wait ~80s for its transport URI to
                // become playable times out and plays SILENCE. Linn Klimax /
                // OpenHome (Pierre Mack) never decodes DSD itself, so it always
                // hit this ~80s stall.
                //
                // A WAV target routes through the streaming session instead: the
                // decoder feeds PCM as it runs (first bytes in ~1s), and the HTTP
                // layer still advertises an exact Content-Length
                // (StreamInfo::wav_content_length, from the known duration) +
                // Accept-Ranges + 206-on-`bytes=0-` — exactly what DLNA/OpenHome
                // renderers require. This is the same streaming-WAV path the
                // Eversolo DMP-A6/A8 already use. Renderers that need a 16-bit
                // LPCM cap keep it via `dlna_needs_wav` above; this branch only
                // catches FLAC-capable renderers (Linn) that were paying the full
                // ~80s stall for nothing.
                AudioFormat::Wav
            } else {
                src_fmt.dlna_transcode_target()
            };
            let mut out_sr = src_fmt.dsd_output_sample_rate(sample_rate);
            // Apply zone max_sample_rate cap
            if let Some(max_sr) = zone_max_sample_rate {
                if out_sr > max_sr {
                    info!(
                        zone_id = req.zone_id,
                        source_rate = out_sr,
                        max_rate = max_sr,
                        "zone_max_sample_rate_cap_applied"
                    );
                    out_sr = max_sr;
                }
            }
            let out_bd: u16 = if local_needs_wav {
                // Local output (cpal/WASAPI): always use 32-bit WAV.
                //
                // Symphonia decodes all audio into AudioBuffer<i32> (left-justified
                // 32-bit integers) regardless of source bit depth.  When packing
                // these into 24-bit (3 bytes/sample), any mismatch between the
                // reported source_bd and the actual sample range causes byte
                // misalignment in the PCM stream — the local parser then reads
                // from wrong offsets, producing white noise.
                //
                // Using 32-bit eliminates this class of bugs entirely: each i32
                // sample is written as 4 bytes, matching the WAV header's declared
                // byte width.  The local output converts to f32 for cpal anyway,
                // so there is zero quality loss.
                32
            } else if browser_needs_wav {
                // Browser <audio> plays 16-bit PCM WAV everywhere; 24/32-bit are
                // spotty across engines. Match the streaming arm (browser = 16-bit
                // WAV) so playback is guaranteed audible.
                16
            } else if src_fmt == AudioFormat::Dsd {
                24
            } else if oaat_needs_wav {
                // OAAT endpoints (Tune's own RPi renderers) parse the WAV fmt
                // chunk and handle true 24-bit PCM: cap at 24-bit.
                cap_output_bit_depth(bit_depth)
            } else if dlna_wav24 {
                // Zone opt-in: serve genuine 24-bit WAV to a renderer that
                // advertises `audio/L24`. The DIDL drops the 16-bit-only
                // `DLNA.ORG_PN=LPCM` profile (didl::dlna_flags_for_mime_bd keyed
                // on this bit_depth), so the renderer parses the real 24-bit WAV
                // header instead of mapping a false profile back to 16-bit and
                // reading misaligned samples (#1137). `dlna_wav24` is already
                // gated on `bit_depth_wire > 16` above; cap at 24 (FLAC/WAV
                // ceiling).
                //
                // `bit_depth_wire`, pas `bit_depth` : sur un ALAC dont la base
                // ignore la profondeur, c'est la sonde du fichier qui fait foi.
                // Prendre la valeur de la base ici servirait un en-tête 16 bits
                // pour un flux 24 — exactement le défaut que ce chemin corrige
                // (#1654).
                bit_depth_wire.min(24)
            } else if dlna_needs_wav {
                // Generic DLNA renderers that need a WAV/LPCM fallback: cap at
                // 16-bit.
                //
                // The WAV we serve is advertised in DIDL with
                // `DLNA.ORG_PN=LPCM` and Content-Type `audio/wav`.  The DLNA
                // LPCM profile is standardised for 16-bit only (`audio/L16`);
                // there is no standard PN for 24-bit LPCM.  Many hi-fi
                // renderers (Ruark R3, LHC-62 — Yves, forum #1137) map that
                // advertised profile to 16-bit and, fed genuine 24-bit PCM
                // (3 bytes/sample), read misaligned samples and play SILENCE.
                // 16-bit tracks worked because 16-bit WAV *is* valid LPCM.
                //
                // Renderers that can preserve hi-res advertise `audio/flac`
                // and take the FLAC branch above (dlna_needs_wav = false), so
                // this cap only ever applies to the LPCM fallback where
                // guaranteed-audible 16-bit is the correct trade-off.
                16
            } else if dlna_cap_16bit {
                // Zone opt-in cap: renderer advertises `audio/flac` but only
                // decodes 16-bit (Ruark R3, #1137). Downconvert to 16-bit FLAC
                // instead of sending silent hi-res direct.
                16
            } else if src_fmt == AudioFormat::Alac {
                // ALAC: transcode to FLAC for DLNA (universally supported).
                // FLAC max is 24-bit; cap at min(source_bd, 24) but at least 16.
                cap_output_bit_depth(bit_depth)
            } else {
                cap_output_bit_depth(bit_depth)
            };
            let out_mime = if oaat_needs_wav || local_needs_wav {
                "audio/wav".to_string()
            } else {
                target_fmt.mime_type().to_string()
            };
            let out_ext = if oaat_needs_wav || local_needs_wav {
                "wav".to_string()
            } else {
                target_fmt.container_format().to_string()
            };

            info!(
                file = %file_path,
                source = ?src_fmt,
                target = ?target_fmt,
                sample_rate = out_sr,
                bit_depth = out_bd,
                "transcode_required"
            );

            // For network outputs (DLNA, OpenHome, etc.) with non-WAV targets
            // (e.g. FLAC), pre-transcode to a temp file on disk so the HTTP
            // handler can serve it with Content-Length and Accept-Ranges.
            // Renderers like the darTZeel LHC-208 reject chunked transfer
            // (no Content-Length) and require a known file size.
            //
            // For local/OAAT outputs (WAV target), keep using streaming
            // sessions — those outputs don't need Content-Length.
            let target_format_str = if target_fmt == AudioFormat::Wav {
                "wav".to_string()
            } else {
                target_fmt.container_format().to_string()
            };
            // Network outputs need file transcode for Content-Length + Range.
            // Local outputs use streaming sessions — the _keep_alive_tx in
            // StreamSession prevents the channel from closing when the decoder
            // finishes, so ASIO/WASAPI can consume all buffered data at their
            // own pace. This avoids the 28s download delay of file transcode.
            // A DSD source served as WAV/LPCM can stream (exact Content-Length
            // from wav_content_length) instead of blocking on a temp file that
            // times out at 120s for DSD256/512 → silence (Villerio). Gated by
            // the `dsd_lpcm_stream` setting (toggle in Settings → Lecture),
            // off by default pending field validation; read live so the toggle
            // takes effect without a restart.
            let dsd_lpcm_streams = src_fmt == AudioFormat::Dsd
                && target_fmt == AudioFormat::Wav
                && SettingsRepo::with_backend(self.db.clone())
                    .get("dsd_lpcm_stream")
                    .ok()
                    .flatten()
                    .as_deref()
                    == Some("true");
            let use_file_transcode = use_file_transcode_for(
                is_network_output,
                target_format_str == "wav",
                dlna_needs_wav,
                dsd_lpcm_streams,
            )
                // Browser zone with an active EQ: the streaming pipe does NOT
                // run the EqProcessor (only transcode_source_to_file does), so
                // the "forced" transcode served EQ-less audio — measured on
                // .18: EQ'd WAV capture was byte-identical to the decoded
                // source. Route through the temp-file path, which also gives
                // <audio> the Content-Length + Range it wants (#1168).
                || (is_browser_output && eq_forces_transcode);

            let info = StreamInfo {
                format: out_ext.clone(),
                mime_type: out_mime.clone(),
                sample_rate: out_sr,
                bit_depth: out_bd,
                channels,
                file_size: None,
                duration_ms: Some(track.duration_ms as u64),
                ..Default::default()
            };

            if use_file_transcode {
                // ── Pre-transcode to temp file (FLAC) ──────────────────
                // Decode → encode → write to /tmp, then create a file session.
                // The HTTP handler serves file sessions with Content-Length
                // and Range support, which DLNA renderers require.
                let fp = file_path.clone();
                let ev_bus = self.event_bus.clone();
                let playback = self.playback.clone();
                let zone_id = req.zone_id;
                // EQ alters the encoded bytes and is not part of the cache key,
                // so a zone with an active EQ never uses the cache (always fresh).
                let eq_profile = self.load_eq_processor(req.zone_id, out_sr, channels);
                // The FIR convolver, like the EQ, alters the encoded bytes and
                // is not part of the cache key → a zone with an active IR never
                // uses the cache (always fresh).
                let convolver = self.load_convolver(req.zone_id, out_sr, channels);
                // ReplayGain scales the samples, so like the EQ and the FIR it
                // changes the encoded bytes without being part of the cache key.
                // A cached transcode made at a different gain would be served
                // silently at the wrong level — so a gained transcode is never
                // cached, and never reads the cache.
                // NOT for a local zone: the local output applies the gain on
                // its own render path, and a local zone with a known source
                // format always comes through here (`local_needs_wav`) — so
                // baking it in as well multiplied the gain twice. A -6 dB track
                // played at -12 dB, quietly.
                let replaygain_factor = match (
                    is_local_output || self.zone_audiophile(req.zone_id),
                    req.track_id,
                ) {
                    (false, Some(tid)) => {
                        let f = crate::audio::replaygain::playback_factor(&self.db, tid);
                        if (f - 1.0).abs() > 1e-6 {
                            Some(f)
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                let cache_path_opt =
                    if eq_profile.is_some() || convolver.is_some() || replaygain_factor.is_some() {
                        None
                    } else {
                        crate::transcode_cache::cache_path(
                            &file_path, &out_ext, out_sr, out_bd, channels,
                        )
                    };
                // The transcode always writes to a fresh `tune-transcode-*` file
                // (subject to the normal cleanup); on success it is atomically
                // renamed into the cache. A crash mid-transcode therefore can
                // never leave a partial file under a cache name that a later hit
                // would serve.
                let tmp_path = std::env::temp_dir()
                    .join(format!(
                        "tune-transcode-{}.{}",
                        uuid::Uuid::new_v4(),
                        &out_ext
                    ))
                    .to_string_lossy()
                    .to_string();

                // Serialize transcodes of this same source file and drop any
                // play a newer tap has already superseded, so a burst of taps
                // can't spawn overlapping ALAC→FLAC transcodes of one file
                // (Yves, DMP-A10 over DLNA). Capture our own play seq, then
                // wait our turn on the per-file gate; if a newer play bumped the
                // generation while we waited, skip the transcode entirely.
                let my_seq = self.playback.current_play_seq(req.zone_id).await;
                let file_gate = {
                    let mut gates = TRANSCODE_GATE.lock().await;
                    gates
                        .entry(file_path.clone())
                        .or_insert_with(|| Arc::new(Mutex::new(())))
                        .clone()
                };
                let _file_hold = file_gate.lock().await;
                if self.playback.current_play_seq(req.zone_id).await != my_seq {
                    info!(
                        zone_id = req.zone_id,
                        file = %file_path,
                        "transcode_skipped_superseded_burst"
                    );
                    return Err(SUPERSEDED_BEFORE_TRANSCODE.into());
                }

                // Cache hit: an identical rendition already exists on disk —
                // serve it and skip the entire decode/encode (Yves: ~30s → instant
                // on replay / superseded burst).
                if let Some(cp) = cache_path_opt
                    .as_ref()
                    .filter(|cp| crate::transcode_cache::is_hit(cp))
                {
                    crate::transcode_cache::touch(cp);
                    let file_size = std::fs::metadata(cp).map(|m| m.len()).unwrap_or(0);
                    info!(file = %file_path, cache = %cp, file_size, "transcode_cache_hit");
                    let file_info = StreamInfo {
                        format: out_ext.clone(),
                        mime_type: out_mime.clone(),
                        sample_rate: out_sr,
                        bit_depth: out_bd,
                        channels,
                        file_size: Some(file_size),
                        duration_ms: Some(track.duration_ms as u64),
                        ..Default::default()
                    };
                    let session_id = self
                        .streamer
                        .create_file_session(file_info, cp.clone(), false)
                        .await;
                    // The current track was a cache hit → warm the next one too,
                    // so an album keeps hitting the cache track after track.
                    self.spawn_warm_next_local(
                        req.zone_id,
                        sample_rate,
                        bit_depth,
                        channels,
                        out_ext.clone(),
                        out_sr,
                        out_bd,
                        target_format_str.clone(),
                    );
                    (
                        session_id,
                        out_mime,
                        out_ext,
                        Some(file_size),
                        Some(out_sr),
                        Some(out_bd as u32),
                        Some(channels as u32),
                    )
                } else {
                    info!(
                        file = %fp,
                        tmp = %tmp_path,
                        target = %target_format_str,
                        sample_rate = out_sr,
                        bit_depth = out_bd,
                        "transcode_to_temp_file_start"
                    );

                    // Target bit depth chosen above (out_bd). For the generic DLNA
                    // WAV/LPCM fallback this is 16 (LPCM is a 16-bit-only profile);
                    // the decoded PCM must actually be reduced to 16-bit here, not
                    // merely relabelled — otherwise 24-bit samples are served under
                    // a 16-bit WAV header and the renderer plays silence (#1137).
                    let target_bd = out_bd;
                    // Le budget doit suivre la TAILLE, pas une constante.
                    //
                    // 120 s fixes suffisaient tant qu'on transcodait du FLAC ;
                    // ils ne suffisent plus pour du DSD. Journaux de Cyrille
                    // (#1330, ampli Yamaha en zone PCM, source sur NAS) : un
                    // FLAC DXD est prêt en ~6 s, un DSD128 en ~20 s, et un
                    // mouvement de symphonie en DSD256 courait encore au-delà.
                    // Passé le délai, la lecture ne démarre JAMAIS — d'où « le
                    // DSD128 passe, le DSD256 non », qui n'a rien à voir avec
                    // la fréquence (les deux visent 352,8 kHz) et tout à voir
                    // avec le volume de données à décoder.
                    let transcode_budget = transcode_budget_for(&fp);
                    info!(
                        file = %file_path,
                        budget_s = transcode_budget.as_secs(),
                        "transcode_budget_selected"
                    );
                    // Même raison que le pré-transcode DASH plus bas : la ligne
                    // de fin doit porter sa propre durée, pour rester lisible
                    // seule dans un export de journal tronqué par la rotation.
                    let file_transcode_start = std::time::Instant::now();
                    let transcode_result = tokio::time::timeout(
                        transcode_budget,
                        transcode_source_to_file(
                            fp.clone(),
                            out_sr,
                            channels,
                            target_bd,
                            target_format_str.clone(),
                            eq_profile,
                            convolver,
                            replaygain_factor,
                            tmp_path.clone(),
                        ),
                    )
                    .await;

                    match transcode_result {
                        Ok(Ok((file_size, pcm_bytes, actual_bd))) => {
                            if file_size < 1024 {
                                warn!(
                                    file = %file_path,
                                    file_size,
                                    "transcode_produced_empty_file — source may be corrupted or encrypted"
                                );
                                let _ = std::fs::remove_file(&tmp_path);
                                return Err(
                                    "transcode produced empty file (corrupted source?)".into()
                                );
                            }
                            // Promote the completed file into the cache (atomic rename
                            // within the temp dir) so the next identical request is a
                            // hit. If we're not caching, or the rename fails, serve the
                            // freshly-written file as before.
                            let serve_path = match cache_path_opt.as_ref() {
                                Some(cp) if std::fs::rename(&tmp_path, cp).is_ok() => {
                                    tokio::task::spawn_blocking(crate::transcode_cache::evict);
                                    cp.clone()
                                }
                                _ => tmp_path.clone(),
                            };
                            info!(
                                file = %file_path,
                                tmp = %serve_path,
                                file_size,
                                elapsed_ms = file_transcode_start.elapsed().as_millis() as u64,
                                "transcode_to_temp_file_complete"
                            );

                            // Emit audio levels in the background, paced to
                            // the playback clock by the forwarder. Pas pendant
                            // un pré-chargement gapless : la session décrit la
                            // piste SUIVANTE, ses niveaux partiraient datés de
                            // l'horloge de la piste courante.
                            if let Some(bus) = ev_bus
                                .clone()
                                .filter(|_| self.levels_attach_allowed(zone_id))
                            {
                                let playback = playback.clone();
                                let actual_ch = channels;
                                let sr = out_sr;
                                // Génération épinglée au moment de la décision,
                                // pas au démarrage de la tâche (#1110).
                                let play_seq = playback.current_play_seq(zone_id).await;
                                tokio::spawn(async move {
                                    // Temp-file : le PCM décodé part du début
                                    // du fichier (un seek passe par Range HTTP).
                                    let levels_tx = spawn_paced_levels_forwarder(
                                        bus, playback, zone_id, play_seq, 0,
                                    );
                                    tokio::task::spawn_blocking(move || {
                                        crate::audio::tap::send_windowed_pcm(
                                            &levels_tx, &pcm_bytes, actual_bd, actual_ch, sr,
                                        );
                                    })
                                    .await
                                    .ok();
                                });
                            }

                            // Create a file session — HTTP handler serves with
                            // Content-Length and Range support.
                            let file_info = StreamInfo {
                                format: out_ext.clone(),
                                mime_type: out_mime.clone(),
                                sample_rate: out_sr,
                                bit_depth: out_bd,
                                channels,
                                file_size: Some(file_size),
                                duration_ms: Some(track.duration_ms as u64),
                                ..Default::default()
                            };
                            let session_id = self
                                .streamer
                                .create_file_session(file_info, serve_path, false)
                                .await;

                            // Current track just transcoded into the cache → warm
                            // the next one in the background while this one plays,
                            // so the album transition is a cache hit (no 30s gap).
                            // Only when the current was actually cached (Some means
                            // no EQ) — warming an EQ zone would populate an entry
                            // the real (EQ) play never hits.
                            if cache_path_opt.is_some() {
                                self.spawn_warm_next_local(
                                    req.zone_id,
                                    sample_rate,
                                    bit_depth,
                                    channels,
                                    out_ext.clone(),
                                    out_sr,
                                    out_bd,
                                    target_format_str.clone(),
                                );
                            }
                            (
                                session_id,
                                out_mime,
                                out_ext,
                                Some(file_size),
                                Some(out_sr),
                                Some(out_bd as u32),
                                Some(channels as u32),
                            )
                        }
                        Ok(Err(e)) => {
                            warn!(error = %e, file = %file_path, "transcode_to_temp_file_failed");
                            let _ = std::fs::remove_file(&tmp_path);
                            return Err(format!("transcode failed: {e}"));
                        }
                        Err(_) => {
                            let budget_s = transcode_budget.as_secs();
                            let size_mb = std::fs::metadata(&fp)
                                .map(|m| m.len() / (1024 * 1024))
                                .unwrap_or(0);
                            // Message explicite : l'ancien annoncait « 120s »
                            // meme quand le budget etait tout autre, et ne
                            // disait pas la taille en cause.
                            warn!(
                                file = %file_path,
                                budget_s,
                                size_mb,
                                "transcode_timeout"
                            );
                            let _ = std::fs::remove_file(&tmp_path);
                            return Err(format!(
                                "transcode timeout after {budget_s}s for a {size_mb} MB source \u{2014} \
                                 disk or network too slow, or the file is unusually large"
                            ));
                        }
                    }
                }
            } else {
                // ── Streaming transcode (WAV for local/OAAT) ──────────
                // Use the computed WAV content length for the DIDL size
                // attribute so DLNA renderers know the correct stream size.
                let transcode_file_size = info.wav_content_length();

                let (session_id, tx, data_ready) =
                    self.streamer.create_session(info, false, 256).await;

                // Mark session: the streaming decoder sends the WAV header
                // with the real source sample rate, so the stream handler
                // must NOT prepend its own.
                {
                    let sessions = self.streamer.sessions_state();
                    let sessions = sessions.lock().await;
                    if let Some(session) = sessions.get(&session_id) {
                        session
                            .wav_header_included
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }

                let fp = file_path.clone();
                let ev_bus = self.event_bus.clone();
                let playback = self.playback.clone();
                let zone_id = req.zone_id;
                let seek_s = req.seek_ms.map(|ms| ms as f64 / 1000.0).unwrap_or(0.0);
                let streamer_sessions = self.streamer.sessions_state();
                let close_session_id = session_id.clone();
                // Pré-chargement gapless : session de la piste suivante, pas
                // de forwarder (voir `levels_prewarm`).
                let attach_levels = self.levels_attach_allowed(zone_id);
                tokio::spawn(async move {
                    debug!(file = %fp, sample_rate = out_sr, channels, "transcode_decoding");

                    // Bus conservé pour signaler un échec de décodage au client :
                    // un décodage transcodé qui échoue (codec non supporté, fichier
                    // corrompu…) ne doit PLUS produire un flux silencieux qui boucle
                    // toutes les ~2 s — on remonte une erreur visible.
                    let err_bus = ev_bus.clone();

                    // Forwarder cadencé si le bus existe ; sinon un canal dont
                    // le récepteur est aussitôt abandonné (le décodeur ignore
                    // les erreurs d'envoi).
                    let levels_tx = match ev_bus.filter(|_| attach_levels) {
                        Some(bus) => {
                            let play_seq = playback.current_play_seq(zone_id).await;
                            spawn_paced_levels_forwarder(
                                bus,
                                playback,
                                zone_id,
                                play_seq,
                                (seek_s * 1000.0) as i64,
                            )
                        }
                        None => {
                            tokio::sync::mpsc::unbounded_channel::<crate::audio::tap::RawWindow>().0
                        }
                    };

                    let fp_clone = fp.clone();
                    let tx_clone = tx.clone();
                    drop(tx);

                    let result = tokio::task::spawn_blocking(move || {
                        crate::audio::decode::decode_to_pcm_streaming_seeked(
                            &fp_clone,
                            Some(out_sr),
                            Some(channels as u32),
                            Some(out_bd),
                            tx_clone,
                            32768,
                            data_ready,
                            levels_tx,
                            seek_s,
                        )
                    })
                    .await;

                    match result {
                        Ok(Ok(_bit_depth)) => {
                            debug!(file = %fp, "transcode_complete_streaming");
                        }
                        Ok(Err(e)) => {
                            warn!(error = %e, file = %fp, "transcode_streaming_decode_failed");
                            if let Some(ref bus) = err_bus {
                                bus.emit(
                                    "zone.playback_error",
                                    serde_json::json!({
                                        "zone_id": zone_id,
                                        "error": format!("Impossible de décoder la piste : {e}"),
                                    }),
                                );
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, file = %fp, "transcode_streaming_task_panic");
                            if let Some(ref bus) = err_bus {
                                bus.emit(
                                    "zone.playback_error",
                                    serde_json::json!({
                                        "zone_id": zone_id,
                                        "error": "Le décodage de la piste a échoué (erreur interne).",
                                    }),
                                );
                            }
                        }
                    }

                    // Signal EOF by dropping the keep-alive sender. The
                    // decoder's tx is already dropped at this point, but the
                    // _keep_alive_tx in the session keeps the channel open
                    // until we explicitly close it here.
                    let sessions = streamer_sessions.lock().await;
                    if let Some(session) = sessions.get(&close_session_id) {
                        session.close_sender().await;
                    }
                });

                (
                    session_id,
                    out_mime,
                    out_ext,
                    transcode_file_size,
                    Some(out_sr),
                    Some(out_bd as u32),
                    Some(channels as u32),
                )
            }
        } else {
            // Standard passthrough: serve the raw file.
            // For DSD, use the MIME type declared by the renderer (from GetProtocolInfo)
            // instead of the generic application/x-dsd — some renderers (Yamaha R-N2000A)
            // only accept the specific MIME they advertise (e.g. audio/dsf).
            let mime = if source_format == Some(AudioFormat::Dsd) && is_network_output {
                let did = req
                    .output_device_id
                    .as_deref()
                    .or(zone.as_ref().and_then(|z| z.output_device_id.as_deref()))
                    .unwrap_or("");
                let cap = self.dsd_capabilities.lock().await;
                cap.get(did)
                    .and_then(|c| c.dsf_mime.clone())
                    .unwrap_or_else(|| "application/x-dsd".into())
            } else {
                source_format
                    .map(|f| f.mime_type().to_string())
                    .unwrap_or_else(|| "audio/flac".into())
            };

            // For a native passthrough served to a *network* renderer (DLNA
            // native FLAC, ALAC, DSD…), advertise the ACTUAL on-disk byte
            // length as `res@size` / HEAD Content-Length instead of the
            // scanned `track.file_size`.
            //
            // The GET handler (`serve_file`) always streams `disk_size` bytes,
            // but the DIDL `res@size` and the HEAD Content-Length are taken from
            // the DB `track.file_size`. When those disagree — the file was
            // re-tagged / had cover art (re)embedded after the scan, or was
            // scanned by an older/fallback code path — a renderer that models
            // playback position from `bytes_received / (size/duration)` (Marantz
            // ND 8006, native FLAC) reaches true EOF while its estimate still
            // reads position < duration, so it restarts/loops the track near the
            // end instead of advancing to the next queued item, and loses the
            // format/duration/progress display on that queued track (#1132).
            //
            // For a *compressed* stream (FLAC) we cannot derive duration from
            // size, but making `res@size` equal the exact bytes the renderer
            // will actually receive keeps its position model consistent — the
            // FLAC analogue of the WAV size/duration fix in 1046ae8e. Only the
            // network passthrough path is touched; local/OAAT/WAV-transcode
            // paths keep their existing sizing (they never reach this branch).
            let passthrough_disk_size = if is_network_output {
                tokio::fs::metadata(&file_path).await.ok().map(|m| m.len())
            } else {
                None
            };
            let passthrough_file_size =
                passthrough_disk_size.or_else(|| track.file_size.map(|s| s as u64));

            let info = StreamInfo {
                format: fmt.clone(),
                mime_type: mime.clone(),
                sample_rate,
                bit_depth,
                channels,
                file_size: passthrough_file_size,
                duration_ms: Some(track.duration_ms as u64),
                ..Default::default()
            };

            let session_id = self
                .streamer
                .create_file_session(info, file_path.clone(), false)
                .await;

            // For M4A/ALAC passthrough, attach an on-the-fly faststart map so the
            // file is served as `ftyp + patched-moov + mdat` (moov relocated to
            // the front). The renderer then reads its metadata up front and starts
            // immediately instead of seeking to the END of the file first — a slow
            // start + Range storm, esp. over a NAS mount (Yves, LHC-56, 192/24
            // ALAC on SMB). This reads only ftyp+moov (never mdat), so it adds no
            // copy latency, and falls back to the original file if not applicable.
            if source_format == Some(AudioFormat::Alac) {
                let fp = file_path.clone();
                // Two shapes to fix: (1) moov-after-mdat → relocate moov to the
                // front (and strip the cover on the way); (2) ALREADY faststart
                // (ftyp|moov|mdat) → moov stays put but its `covr` cover art still
                // makes the LHC-56 "ploc" at track start, so strip it in place.
                // prepare_faststart handles (1) and returns None for (2), which was
                // the gap: already-faststart files with artwork kept clicking
                // (Yves: "Do What U Will" / "ABOVE AND BEYOND"). Fall back to the
                // in-place cover strip. Both read only ftyp+moov (no mdat copy).
                let mapped = tokio::task::spawn_blocking(move || {
                    crate::audio::faststart::prepare_faststart(std::path::Path::new(&fp))
                        .map(|m| ("relocate", m))
                        .or_else(|| {
                            crate::audio::faststart::prepare_cover_strip_faststart(
                                std::path::Path::new(&fp),
                            )
                            .map(|m| ("cover_strip", m))
                        })
                })
                .await;
                if let Ok(Some((how, map))) = mapped {
                    info!(file = %file_path, how, "m4a_faststart_applied");
                    self.streamer.set_faststart(&session_id, map).await;
                }
            }

            // Parallel decode-for-levels: decode the audio in the background
            // purely to emit VU-meter events for the web client. This does not
            // affect the actual audio stream served to the output device.
            // Skip DSD (1-bit at MHz rates, can't decode inline for levels)
            // and exotic formats that need heavy conversion.
            let skip_passthrough_levels = source_format
                .as_ref()
                .is_some_and(|f| f.needs_transcode_for_dlna());
            // Ce decodage parallele n'a de sens que si PERSONNE d'autre ne
            // decode le fichier cote serveur : sortie reseau ou navigateur, qui
            // recoivent une URL et lisent eux-memes. Une sortie locale (comme
            // OAAT, AirPlay, HQPlayer ou le pont) decode deja pour alimenter le
            // peripherique, et son chemin de lecture emet ses propres niveaux :
            // on decodait donc la piste ENTIERE une seconde fois pour rien,
            // ~65 evenements/s au lieu de ~32, avec des horodatages qui
            // divergent apres un seek (l'un part du seek, l'autre de 0) et,
            // depuis #1106, des fenetres dupliquees sur le tap PCM (#1110).
            let output_decodes_server_side = !(is_network_output || is_browser_output);
            if !skip_passthrough_levels
                && !output_decodes_server_side
                && self.levels_attach_allowed(req.zone_id)
            {
                if let Some(ref bus) = self.event_bus {
                    let bus = bus.clone();
                    let playback = self.playback.clone();
                    let fp = file_path.clone();
                    let zone_id = req.zone_id;
                    let sr = sample_rate;
                    let ch = channels as u32;
                    // Génération épinglée au moment de la décision (#1110) :
                    // ce décodage complet dure toute la piste, il ne doit pas
                    // pouvoir se raccrocher à la suivante.
                    let play_seq = self.playback.current_play_seq(req.zone_id).await;
                    tokio::spawn(async move {
                        // Passthrough : le décodage pour niveaux part de 0.
                        let levels_tx =
                            spawn_paced_levels_forwarder(bus, playback, zone_id, play_seq, 0);
                        // Décodage EN FLUX, pas en une fois. `decode_to_pcm`
                        // matérialisait la piste entière en mémoire avant
                        // d'émettre la moindre fenêtre : ~1,9 Go pour un
                        // 24/192 de dix minutes, alloué à chaque début de
                        // piste et uniquement pour animer des aiguilles.
                        // C'est la même faute que #1109 (ReplayGain), un cran
                        // plus loin dans la chaîne. Le décodeur en flux émet
                        // les niveaux au fil de l'eau ; le PCM produit part
                        // dans un puits, seul l'ordre de grandeur du tampon
                        // reste en mémoire.
                        let (sink_tx, mut sink_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
                        tokio::spawn(async move { while sink_rx.recv().await.is_some() {} });
                        let ready = std::sync::Arc::new(tokio::sync::Notify::new());
                        let result = tokio::task::spawn_blocking(move || {
                            crate::audio::decode::decode_to_pcm_streaming_with_levels(
                                &fp,
                                Some(sr),
                                Some(ch),
                                None,
                                sink_tx,
                                LEVELS_DECODE_CHUNK,
                                ready,
                                levels_tx,
                            )
                        })
                        .await;
                        match result {
                            Err(e) => debug!(error = %e, "passthrough_levels_task_panic"),
                            Ok(Err(e)) => debug!(error = %e, "passthrough_levels_decode_failed"),
                            Ok(Ok(_)) => {}
                        }
                    });
                }
            }

            (
                session_id,
                mime,
                fmt.clone(),
                passthrough_file_size,
                Some(sample_rate),
                Some(bit_depth as u32),
                Some(channels as u32),
            )
        };

        let server_ip = self.server_ip();
        let stream_url = self
            .streamer
            .get_stream_url(&session_id, &server_ip, &out_ext);

        // For a transcoded WAV/LPCM stream served with an exact byte length
        // (the file-transcode path pre-encodes the whole WAV, so file_size is
        // the real body size), advertise a DIDL `res@duration` derived from
        // that byte length instead of the scanned `track.duration_ms`. The two
        // can disagree by a few seconds (the FLAC STREAMINFO/scan duration vs.
        // the actual decoded sample count), and when the DIDL duration is
        // LONGER than the bytes the renderer receives, some renderers (Marantz
        // ND 8006) reach EOF, see position < advertised duration, and
        // restart/loop the track near the end instead of advancing (#1132).
        // Computing duration from size/byte_rate keeps duration and size
        // mathematically consistent, so the progress bar tracks correctly and
        // the track advances cleanly. Only applies when we know the exact size
        // AND the audio params; otherwise fall back to the scanned duration.
        let didl_duration_ms = if out_mime == "audio/wav" || out_mime == "audio/x-wav" {
            match (resolved_file_size, resolved_sr, resolved_bd, resolved_ch) {
                (Some(size), Some(sr), Some(bd), Some(ch))
                    if size > 44 && sr > 0 && bd > 0 && ch > 0 =>
                {
                    let byte_rate = sr as u64 * ch as u64 * (bd as u64 / 8);
                    if byte_rate > 0 {
                        Some(((size - 44) * 1000 / byte_rate) as i64)
                    } else {
                        Some(track.duration_ms)
                    }
                }
                _ => Some(track.duration_ms),
            }
        } else if !needs_transcode && is_network_output {
            // Native passthrough (FLAC/ALAC/… served raw) to a network renderer.
            //
            // The gapless-queued (SetNextAVTransportURI) track is the one that
            // regresses on the Marantz ND 8006 (Jean Valjean, #1132): odd tracks
            // start via a fresh SetAVTransportURI + Play — the renderer fetches
            // the URL itself, learns the true byte length, and ends the track at
            // the real EOF, so an over-long DIDL duration is harmless. But on the
            // gapless auto-transition to the *next* track the renderer does NOT
            // re-probe the stream — it models playback purely from the DIDL
            // `res@duration` we supplied via SetNext. When the scanned
            // `track.duration_ms` (possibly recovered by a slow/fallback scan on
            // a NAS, or drifted vs. the real sample count) is a few seconds LONGER
            // than the file's true duration, the renderer holds at the real EOF
            // with its estimate still reading position < duration, loses the
            // format/duration/progress display and cuts near the end of the
            // queued track. 1626ec21 only made `res@size` consistent; the
            // duration was still the scanned value on this passthrough path.
            //
            // Prefer the file container's authoritative duration (FLAC STREAMINFO
            // total_samples / sample_rate via lofty — metadata only, no decode)
            // so the SetNext DIDL `res@duration` matches the bytes actually
            // served. This corrects the current-track DIDL identically (it can
            // only get MORE accurate, never worse — the initial Play already
            // ends at real EOF). Fall back to the scanned duration if the probe
            // fails, so a NAS timeout never blanks the duration entirely.
            let probed_secs = crate::audio::analyzer::get_duration(&file_path).await.ok();
            Some(passthrough_didl_duration_ms(probed_secs, track.duration_ms))
        } else {
            Some(track.duration_ms)
        };

        Ok(ResolvedStream {
            url: stream_url,
            mime_type: out_mime,
            title: track.title,
            artist: track.artist_name,
            album: track.album_title,
            duration_ms: didl_duration_ms,
            source: "local".into(),
            cover_url: track.cover_path,
            stream_id: Some(session_id),
            file_size: resolved_file_size,
            sample_rate: resolved_sr,
            bit_depth: resolved_bd,
            channels: resolved_ch,
            origin_url: None,
            bitrate_kbps: None,
        })
    }

    async fn resolve_streaming_url(
        &self,
        service_name: &str,
        req: &PlayRequest,
    ) -> Result<ResolvedStream, String> {
        let source_id = req
            .source_id
            .as_deref()
            .ok_or("source_id required for streaming")?;

        // Check for prefetched PCM data before downloading.
        // If the prefetch engine has already decoded this track, serve
        // the PCM directly via a streaming session — zero download delay.
        // Skip prefetch for network outputs (DLNA) when buffer is truncated
        // (30s mode) — the renderer needs the full file.
        //
        // A seek must resolve a FRESH stream at the requested position. The
        // prefetch buffer always starts at position 0, so serving it on a seek
        // would (a) play from the wrong position and (b) race the recreated
        // local output: the buffered PCM feed completes before ASIO/WASAPI
        // attaches, leaving the stream with 0 frames → playback stops.
        // (DEvir: seek on a TIDAL track → title stays but music stops.)
        // Only consider the prefetch buffer when NOT seeking.
        let prefetched = if req.seek_ms.is_some_and(|ms| ms > 0) {
            None
        } else {
            self.prefetch.take_prefetched(service_name, source_id).await
        };
        if let Some(prefetched) = prefetched {
            let is_network = req
                .output_device_id
                .as_deref()
                .is_some_and(|id| !id.starts_with("local:") && !id.starts_with("oaat:"));
            let bytes_per_sec = (prefetched.sample_rate as usize)
                * (prefetched.bit_depth as usize / 8)
                * (prefetched.channels as usize);
            let buffered_ms = if bytes_per_sec > 0 {
                (prefetched.pcm_data.len() as u64 * 1000) / bytes_per_sec as u64
            } else {
                0
            };
            let is_truncated = prefetch_buffer_truncated(buffered_ms, prefetched.duration_ms);

            // Skip a truncated prefetch buffer for EVERY output, not just DLNA.
            // The prefetch head-start is only ~30s; `serve_prefetched_pcm` feeds
            // exactly that PCM into the session and then drops the sender. On a
            // network output that meant a short file; on a LOCAL EXCLUSIVE output
            // (ASIO) the blocking HTTP read never gets a clean EOF at the loop
            // point, so once the 30s buffer is consumed the audio thread starves
            // and freezes until the 20s watchdog resets the host to WASAPI
            // (DEvir bug-20, repeat-one on a >30s track). Fetching the full
            // stream instead keeps the exclusive read fed for the whole track.
            if is_truncated {
                info!(
                    service = service_name,
                    source_id = %source_id,
                    buffered_ms,
                    duration_ms = prefetched.duration_ms,
                    is_network,
                    "prefetch_skip_truncated_serving_full_stream"
                );
            } else {
                info!(
                    service = service_name,
                    source_id = %source_id,
                    title = %prefetched.title,
                    buffer_bytes = prefetched.pcm_data.len(),
                    "prefetch_hit_serving_buffered_pcm"
                );
                return self.serve_prefetched_pcm(prefetched, req).await;
            }
        }

        let registry = self.services.lock().await;
        let svc = registry
            .get(service_name)
            .ok_or_else(|| format!("unknown service: {service_name}"))?;
        let mut svc = svc.write().await;

        // Try to get the track URL; if it fails with an auth error, attempt
        // a token refresh and retry once. This handles Qobuz tokens expiring
        // mid-session (search still works without auth, but playback doesn't).
        let stream_data = match svc.get_track_url(source_id, None).await {
            Ok(data) => data,
            Err(ref e)
                if {
                    let msg = e.to_string();
                    msg.contains("401") || msg.contains("403")
                } =>
            {
                info!(
                    service = service_name,
                    error = %e,
                    "streaming_auth_error_attempting_refresh"
                );
                if svc.refresh_if_needed().await.unwrap_or(false) {
                    svc.get_track_url(source_id, None)
                        .await
                        .map_err(|e| e.to_string())?
                } else {
                    return Err(e.to_string());
                }
            }
            Err(e) => return Err(e.to_string()),
        };

        let info = StreamInfo {
            format: stream_data.quality.codec.to_lowercase(),
            mime_type: stream_data.mime_type.clone(),
            sample_rate: stream_data.quality.sample_rate,
            bit_depth: stream_data.quality.bit_depth,
            channels: 2,
            file_size: None,
            duration_ms: None,
            ..Default::default()
        };

        let is_https = stream_data.url.starts_with("https://");
        // file:// URLs come from Tidal DASH multi-segment downloads — the fMP4
        // has already been assembled on disk by get_track_url().
        let is_dash_file = stream_data.url.starts_with("file://");
        let is_oaat_stream = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("oaat:") || id.starts_with("oaat-group:"));
        let is_local_stream = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("local:"));

        // Local and OAAT outputs expect raw PCM in a WAV container.
        // Streaming services deliver compressed audio (FLAC, AAC, etc.)
        // which LocalOutput cannot decode — it would interpret compressed
        // bytes as raw PCM samples, producing white noise.
        // Fix: download → decode → WAV transcode, same as local files.
        let (stream_url, sid, out_mime, stream_file_size) = if is_local_stream || is_oaat_stream {
            let upstream_url = stream_data.url.clone();
            let codec = stream_data.quality.codec.to_lowercase();
            // Cap the WAV rate to the zone's max_sample_rate (e.g. an OAAT
            // endpoint whose DAC tops out at 96k). resolve_local_track applies
            // this cap for local files; the streaming path historically did NOT,
            // so a 192k Qobuz/Tidal track was transcoded to a 192k WAV and handed
            // to a 96k OAAT endpoint → the DAC rejected the rate → silence with no
            // server-side error (radio at 44.1/48k on the same zone played fine).
            // decode_to_pcm_streaming_with_levels resamples to `sr`, so capping
            // here downsamples the PCM, not just the WAV header.
            let zone_max_sample_rate = ZoneRepo::with_backend(self.db.clone())
                .get(req.zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.max_sample_rate);
            let mut sr = stream_data.quality.sample_rate;
            if let Some(max_sr) = zone_max_sample_rate {
                if sr > max_sr {
                    info!(
                        zone_id = req.zone_id,
                        source_rate = sr,
                        max_rate = max_sr,
                        "streaming_zone_max_sample_rate_cap_applied"
                    );
                    sr = max_sr;
                }
            }
            // Local output: 32-bit to avoid 24-bit byte misalignment noise
            // (see local_needs_wav comment in resolve_local_track).
            // OAAT: cap at 24-bit (endpoints may not support 32-bit WAV).
            let bd = if is_local_stream {
                32
            } else {
                cap_output_bit_depth(stream_data.quality.bit_depth)
            };

            let wav_info = StreamInfo {
                format: "wav".into(),
                mime_type: "audio/wav".into(),
                sample_rate: sr,
                bit_depth: bd,
                channels: 2,
                file_size: None,
                duration_ms: None,
                ..Default::default()
            };

            // Guard against a stale/cleaned-up DASH temp file (mirrors the
            // `is_dash_file` DLNA path below). The local transcode runs
            // fire-and-forget in a spawned task, so a missing file would decode
            // to nothing while play() still reports output_sent=true. Fail early
            // so the caller sees the real failure instead of silent no-playback.
            // (Reported on ASIO with 24/192 Tidal DASH after the temp file is gone.)
            if upstream_url.starts_with("file://") {
                let fp = upstream_url
                    .strip_prefix("file://")
                    .unwrap_or(&upstream_url);
                let size = std::fs::metadata(fp).map(|m| m.len()).unwrap_or(0);
                if size == 0 {
                    warn!(path = %fp, "streaming_dash_file_missing_or_empty");
                    return Err(format!(
                        "DASH temp file missing or empty (needs re-download): {fp}"
                    ));
                }
            }

            let (session_id, tx, data_ready) =
                self.streamer.create_session(wav_info, false, 256).await;

            {
                let sessions = self.streamer.sessions_state();
                let sessions = sessions.lock().await;
                if let Some(session) = sessions.get(&session_id) {
                    session
                        .wav_header_included
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }

            info!(
                service = service_name,
                codec = %codec,
                sample_rate = sr,
                bit_depth = bd,
                "streaming_transcode_to_wav_for_local_output"
            );

            let ev_bus = self.event_bus.clone();
            let playback = self.playback.clone();
            let zone_id = req.zone_id;
            let streamer_for_eof = self.streamer.clone();
            let session_id_for_eof = session_id.clone();
            // Pré-chargement gapless : pas de forwarder (voir `levels_prewarm`).
            let attach_levels = self.levels_attach_allowed(zone_id);
            // Seek d'une piste streaming (Qobuz/Tidal) sur sortie locale/OAAT :
            // le chemin local passait déjà l'offset au décodeur, celui-ci
            // repartait TOUJOURS de zéro — l'audio recommençait au début alors
            // que l'UI affichait la position demandée (repros Hard To Say
            // Goodbye 405s et Bina 1015s, .18, 28/07).
            let seek_s = req.seek_ms.map(|ms| ms as f64 / 1000.0).unwrap_or(0.0);

            // Detect file:// URLs from DASH multi-segment downloads — the fMP4
            // is already on disk, skip the HTTP download step.
            let is_dash_local = upstream_url.starts_with("file://");
            // Le CDN YouTube accepte les requetes Range. Un M4A peut garder son
            // atome `moov` a la fin : une source HTTP seekable permet a
            // Symphonia de lire cet index puis de revenir aux premiers paquets,
            // sans attendre le telechargement complet (#1885). Les autres
            // services gardent leur chemin eprouve dans cette premiere vague.
            let use_http_range = service_name.eq_ignore_ascii_case("youtube")
                && matches!(codec.as_str(), "m4a" | "mp4" | "aac");

            // Background task: download upstream → temp file → decode → WAV → session
            tokio::spawn(async move {
                // Audio-levels channel so the web client VU-meter works for
                // streaming-service content played through local/OAAT outputs.
                // Paced to the playback clock by the forwarder; without a bus,
                // the receiver is dropped and the decoder's sends are no-ops.
                let levels_tx = match ev_bus.filter(|_| attach_levels) {
                    Some(bus) => {
                        let play_seq = playback.current_play_seq(zone_id).await;
                        spawn_paced_levels_forwarder(
                            bus,
                            playback,
                            zone_id,
                            play_seq,
                            (seek_s * 1000.0) as i64,
                        )
                    }
                    None => {
                        tokio::sync::mpsc::unbounded_channel::<crate::audio::tap::RawWindow>().0
                    }
                };

                // Sonde Range AVANT d'envoyer un en-tete WAV. Si le CDN le
                // refuse, aucun octet n'a encore rejoint la session et le repli
                // historique par fichier temporaire reste parfaitement propre.
                let ranged_source = if use_http_range {
                    let upstream = upstream_url.clone();
                    match tokio::task::spawn_blocking(move || {
                        crate::audio::http_range::HttpRangeSource::open(&upstream)
                    })
                    .await
                    {
                        Ok(Ok(source)) => {
                            info!("streaming_http_range_decode_selected");
                            Some(source)
                        }
                        Ok(Err(e)) => {
                            info!(error = %e, "streaming_http_range_unavailable_falling_back");
                            None
                        }
                        Err(e) => {
                            warn!(error = %e, "streaming_http_range_probe_task_failed");
                            None
                        }
                    }
                } else {
                    None
                };

                // Sans source Range, conserver strictement le chemin existant :
                // fichier DASH deja local ou telechargement complet vers un temp.
                let tmp_file = if ranged_source.is_some() {
                    None
                } else if is_dash_local {
                    let file_path = upstream_url
                        .strip_prefix("file://")
                        .unwrap_or(&upstream_url)
                        .to_string();
                    let file_size = std::fs::metadata(&file_path)
                        .ok()
                        .map(|m| m.len())
                        .unwrap_or(0);
                    info!(
                        path = %file_path,
                        file_size,
                        "streaming_dash_file_already_on_disk"
                    );
                    Some((file_path, false))
                } else {
                    let tmp_path = std::env::temp_dir()
                        .join(format!("tune-stream-{}.{}", uuid::Uuid::new_v4(), codec))
                        .to_string_lossy()
                        .to_string();
                    let tmp_path_clone = tmp_path.clone();
                    let upstream = upstream_url.clone();
                    let download_result = tokio::task::spawn_blocking(move || {
                        let resp = crate::http::client::blocking_builder()
                            .timeout(std::time::Duration::from_secs(120))
                            .build()
                            .and_then(|c| c.get(&upstream).send());
                        match resp {
                            Ok(mut r) if r.status().is_success() => {
                                let mut file = match std::fs::File::create(&tmp_path_clone) {
                                    Ok(f) => f,
                                    Err(e) => return Err(format!("tmp create: {e}")),
                                };
                                match std::io::copy(&mut r, &mut file) {
                                    Ok(bytes) => {
                                        debug!(bytes, path = %tmp_path_clone, "streaming_download_complete");
                                        Ok(tmp_path_clone)
                                    }
                                    Err(e) => Err(format!("download copy: {e}")),
                                }
                            }
                            Ok(r) => Err(format!("upstream HTTP {}", r.status())),
                            Err(e) => Err(format!("upstream fetch: {e}")),
                        }
                    })
                    .await;

                    match download_result {
                        Ok(Ok(path)) => Some((path, true)),
                        Ok(Err(e)) => {
                            warn!(error = %e, "streaming_transcode_download_failed");
                            let _ = std::fs::remove_file(&tmp_path);
                            return;
                        }
                        Err(e) => {
                            warn!(error = %e, "streaming_transcode_task_join_failed");
                            let _ = std::fs::remove_file(&tmp_path);
                            return;
                        }
                    }
                };

                let tx_for_decode = tx.clone();
                // Drop the original sender so the channel closes when decode finishes.
                drop(tx);
                let decode_result = if let Some(source) = ranged_source {
                    tokio::task::spawn_blocking(move || {
                        crate::audio::decode::decode_http_range_to_pcm_streaming_seeked(
                            source,
                            &codec,
                            Some(sr),
                            Some(2),
                            Some(bd),
                            tx_for_decode,
                            32768,
                            data_ready,
                            levels_tx,
                            seek_s,
                        )
                    })
                    .await
                } else {
                    let tmp_file_clone = tmp_file.as_ref().unwrap().0.clone();
                    tokio::task::spawn_blocking(move || {
                        crate::audio::decode::decode_to_pcm_streaming_seeked(
                            &tmp_file_clone,
                            Some(sr),
                            Some(2),
                            Some(bd),
                            tx_for_decode,
                            32768,
                            data_ready,
                            levels_tx,
                            seek_s,
                        )
                    })
                    .await
                };

                // Clean up the temp file — but ONLY if WE downloaded it. For a
                // file:// DASH source, tmp_file IS the Tidal-cache-owned
                // tune-dash-*.mp4 that is still referenced by the cached stream
                // URL. Deleting it here made every subsequent re-resolution
                // (repeat=one, or a seek that recreates the local stream) see the
                // file gone, mark the cache stale, and re-download the whole
                // ~54MB DASH — while concurrent transcodes raced on the emptied
                // file (file_size=0 → decode failed). That was the ASIO "repeat"
                // runaway (also on Qobuz). Leave cache-owned files alone.
                if let Some((tmp_file, owned)) = tmp_file
                    && owned
                {
                    let _ = std::fs::remove_file(&tmp_file);
                }

                match decode_result {
                    Ok(Ok((_bit_depth, actual_rate))) => {
                        if actual_rate != sr {
                            tracing::info!(
                                api_rate = sr,
                                actual_rate,
                                "streaming_sample_rate_mismatch_wav_header_has_correct_rate"
                            );
                        }
                        debug!("streaming_transcode_complete_progressive");
                    }
                    Ok(Err(e)) => {
                        warn!(error = %e, "streaming_transcode_decode_failed");
                    }
                    Err(e) => {
                        warn!(error = %e, "streaming_transcode_decode_task_panic");
                    }
                }

                // Fin d'entrée : sans ça, le keep-alive de la session garde le
                // canal ouvert après la fin du décodage, le corps HTTP ne se
                // termine jamais, et l'OAAT (gapless interne basé sur l'EOF)
                // reste muet en fin de piste puis se fait relancer par le
                // superviseur — silence + « le dernier morceau est rejoué ».
                streamer_for_eof
                    .end_session_input(&session_id_for_eof)
                    .await;
            });

            let server_ip = self.server_ip();
            let url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");
            (url, Some(session_id), "audio/wav".to_string(), None)
        } else if is_dash_file {
            // DASH multi-segment fMP4 already assembled on disk by get_track_url().
            // DLNA renderers can't decode fMP4+FLAC directly, and chunked WAV
            // causes noise on many renderers (darTZeel, Eversolo, etc.).
            // Pre-transcode to a FLAC temp file so we can serve with Content-Length.
            let dash_file_path = stream_data
                .url
                .strip_prefix("file://")
                .unwrap_or(&stream_data.url)
                .to_string();

            if !std::path::Path::new(&dash_file_path).exists() {
                warn!(path = %dash_file_path, "streaming_dash_file_missing_skipping_decode");
                return Err("DASH file missing (already consumed by prior decode)".into());
            }

            // Zone EQ, loaded ONCE and reused by both the warm-cache decision and
            // the transcode below. A second load could observe a just-enabled EQ
            // and store an EQ'd transcode under the EQ-less cache key, poisoning
            // every later hit for this track.
            let eq_profile_pretranscode =
                self.load_eq_processor(req.zone_id, stream_data.quality.sample_rate, 2);

            // Browser (Web Audio) zones pull the stream themselves via <audio> and
            // issue arbitrary byte-Range requests to buffer/seek. Our native FLAC
            // encoder writes no SEEKTABLE, so a mid-file offset never lands on a
            // frame boundary; Safari can't resync and playback stalls a few seconds
            // in while the timeline keeps running (Philippe Vella, Tidal HI-RES on
            // the browser "Cet ordinateur" zone, 0.9.42). WAV's linear byte↔sample
            // layout makes every Range resolvable, so serve WAV to browser zones —
            // the same format the local output already plays fine for these tracks.
            let is_browser_output = ZoneRepo::with_backend(self.db.clone())
                .get(req.zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_type)
                .as_deref()
                == Some("browser");

            struct DashWarm {
                cache_path: String,
                enc_format: &'static str,
                key_bit_depth: u16,
                force_flac: bool,
            }

            // Warm-cache (opt-in, TUNE_DASH_WARM_CACHE): a prior play/warm of this
            // exact track+quality+format may have left a finished transcode on
            // disk. All the format-decision work (incl. a dlna_supports_mime await)
            // runs ONLY when the flag is on, so a disabled build is byte-identical.
            // `warm` is None when the flag is off or a zone EQ is active (EQ is
            // out of the key). When Some, its format decision is authoritative for
            // the whole DASH arm (see dash_enc_format below), so the cache key and
            // the encoded bytes can never disagree.
            let warm: Option<DashWarm> = if dash_warm_cache_enabled() {
                let wsr = stream_data.quality.sample_rate;
                let wbd = stream_data.quality.bit_depth.max(16).min(24);
                let wdid = req.output_device_id.as_deref().unwrap_or("");
                let wflac =
                    ZoneRepo::with_backend(self.db.clone()).get_dlna_native_flac(req.zone_id);
                let wfmt = if is_browser_output {
                    "wav"
                } else if wdid.is_empty()
                    || wflac
                    || self.dlna_supports_mime(wdid, "audio/flac").await
                {
                    "flac"
                } else {
                    "wav"
                };
                let wkbd = if wfmt == "wav" { 16 } else { wbd };
                if eq_profile_pretranscode.is_none() {
                    Some(DashWarm {
                        cache_path: crate::transcode_cache::cache_path_streaming(
                            service_name,
                            source_id,
                            wfmt,
                            wsr,
                            wkbd,
                            2,
                        ),
                        enc_format: wfmt,
                        key_bit_depth: wkbd,
                        force_flac: wflac,
                    })
                } else {
                    None
                }
            } else {
                None
            };

            // Cache hit → serve the finished transcode, skipping the whole
            // download+decode+encode. The fMP4 on disk is left untouched (not
            // renamed to `.decoding` / consumed), so a concurrent path can still
            // use it. Mirrors the common metadata tail before returning.
            if let Some(w) = warm.as_ref() {
                if crate::transcode_cache::is_hit(&w.cache_path) {
                    crate::transcode_cache::touch(&w.cache_path);
                    if let Ok(md) = std::fs::metadata(&w.cache_path) {
                        let file_size = md.len();
                        let hit_mime = if w.enc_format == "flac" {
                            "audio/flac"
                        } else {
                            "audio/wav"
                        };
                        let file_info = StreamInfo {
                            format: w.enc_format.into(),
                            mime_type: hit_mime.into(),
                            sample_rate: stream_data.quality.sample_rate,
                            bit_depth: w.key_bit_depth,
                            channels: 2,
                            file_size: Some(file_size),
                            duration_ms: None,
                            ..Default::default()
                        };
                        let session_id = self
                            .streamer
                            .create_file_session(file_info, w.cache_path.clone(), false)
                            .await;
                        let server_ip = self.server_ip();
                        let stream_url =
                            self.streamer
                                .get_stream_url(&session_id, &server_ip, w.enc_format);
                        info!(cache = %w.cache_path, file_size, "streaming_dash_warm_cache_hit");
                        // Warm N+1 into the cache while this track plays (same
                        // device → same FLAC/WAV decision, so inherit it).
                        self.spawn_warm_next_streaming(
                            req.zone_id,
                            source_id.to_string(),
                            w.enc_format,
                        );

                        let has_title = req.title.as_deref().is_some_and(|s| !s.is_empty());
                        let (title, artist, album, duration_ms, cover_path) = if has_title {
                            (
                                req.title.clone().unwrap_or_default(),
                                req.artist_name.clone(),
                                req.album_title.clone(),
                                req.duration_ms,
                                req.cover_url.clone(),
                            )
                        } else {
                            match svc.get_track(source_id).await {
                                Ok(track) => (
                                    track.title,
                                    Some(track.artist),
                                    track.album,
                                    Some(track.duration_ms as i64),
                                    track.cover_path,
                                ),
                                Err(_) => (
                                    req.title
                                        .clone()
                                        .filter(|s| !s.is_empty())
                                        .unwrap_or_else(|| "Unknown".into()),
                                    req.artist_name.clone(),
                                    req.album_title.clone(),
                                    req.duration_ms,
                                    req.cover_url.clone(),
                                ),
                            }
                        };
                        return Ok(ResolvedStream {
                            url: stream_url,
                            mime_type: hit_mime.into(),
                            title,
                            artist,
                            album,
                            duration_ms,
                            source: service_name.into(),
                            cover_url: cover_path,
                            stream_id: Some(session_id),
                            file_size: Some(file_size),
                            sample_rate: Some(stream_data.quality.sample_rate),
                            bit_depth: Some(stream_data.quality.bit_depth as u32),
                            channels: Some(2),
                            origin_url: None,
                            bitrate_kbps: None,
                        });
                    }
                }
            }

            let unique_path = format!("{}.decoding", &dash_file_path);
            if std::fs::rename(&dash_file_path, &unique_path).is_err() {
                warn!(path = %dash_file_path, "streaming_dash_file_already_being_decoded");
                return Err("DASH file already being decoded".into());
            }

            let sr = stream_data.quality.sample_rate;
            let bd = stream_data.quality.bit_depth.max(16).min(24);

            let tmp_path = std::env::temp_dir()
                .join(format!("tune-dash-transcode-{}.flac", uuid::Uuid::new_v4()))
                .to_string_lossy()
                .to_string();

            info!(
                path = %unique_path,
                tmp = %tmp_path,
                sample_rate = sr,
                bit_depth = bd,
                "streaming_dash_pre_transcode_to_flac"
            );

            // Strict DLNA renderers (Revox, Denon, Marantz) reject FLAC — their
            // Sink doesn't advertise audio/flac, so they fetch the file but play
            // nothing. Serve them LPCM/WAV instead, like the local-file path.
            // Otherwise keep FLAC (smaller, Content-Length). Previously these
            // streaming paths always emitted audio/flac (Philippe / Revox S100).
            let dash_did = req.output_device_id.as_deref().unwrap_or("");
            // Honour the per-zone "native FLAC" override for streaming DASH too
            // (Tidal/Qobuz Hi-Res), not just local files: some renderers decode
            // FLAC but never advertise it (Marco's Denon Ceol N12 returns an
            // empty GetProtocolInfo Sink), so negotiation wrongly falls back to
            // WAV. When the zone forces native FLAC, keep FLAC here as well.
            //
            // When the warm-cache key was computed above, REUSE its decision
            // instead of re-deriving it: the same logic evaluated twice can
            // diverge (device cache refresh, zone toggle flipped mid-request)
            // and would store a transcode under a key describing other bytes.
            let (dash_enc_format, dash_force_flac) = match warm.as_ref() {
                Some(w) => (w.enc_format, w.force_flac),
                None => {
                    let force =
                        ZoneRepo::with_backend(self.db.clone()).get_dlna_native_flac(req.zone_id);
                    let fmt = if is_browser_output {
                        // Browser pulls with byte-Range requests; a seektable-less
                        // FLAC stalls it (see is_browser_output note above). WAV.
                        "wav"
                    } else if dash_did.is_empty()
                        || force
                        || self.dlna_supports_mime(dash_did, "audio/flac").await
                    {
                        "flac"
                    } else {
                        "wav"
                    };
                    (fmt, force)
                }
            };
            // Make the streaming-DLNA format decision explicit in the log so we
            // can tell why a renderer got WAV vs FLAC (Marco: multiple Denon
            // zones — is the "native FLAC" toggle set on the ZONE being played?).
            info!(
                zone_id = req.zone_id,
                device_id = %dash_did,
                native_flac_override = dash_force_flac,
                chosen_format = dash_enc_format,
                "streaming_dash_dlna_format_decision"
            );

            // Streaming remux (#1146, opt-in TUNE_DASH_STREAM_REMUX): chunked-stream
            // the remuxed FLAC to a Lavf-class renderer (DMP-A8) AS the DASH file
            // downloads, matching Qobuz's instant start — no wait for the whole
            // file + no re-encode. Only FLAC + no-EQ (a WAV renderer or a zone EQ
            // needs decoded PCM → keep the file path). Reads the GROWING fMP4 via
            // the dash_growth registry when TUNE_DASH_STREAM_DECODE armed the
            // background download, so playback begins on the first fragments.
            if dash_enc_format == "flac"
                && eq_profile_pretranscode.is_none()
                && std::env::var("TUNE_DASH_STREAM_REMUX")
                    .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                    .unwrap_or(false)
            {
                let info = StreamInfo {
                    format: "flac".into(),
                    mime_type: "audio/flac".into(),
                    sample_rate: sr,
                    bit_depth: bd,
                    channels: 2,
                    file_size: None, // chunked — no Content-Length
                    duration_ms: None,
                    ..Default::default()
                };
                let (session_id, tx, data_ready, _session) =
                    self.streamer.create_radio_session(info, 256).await;
                let up = unique_path.clone();
                tokio::spawn(async move {
                    let up_stream = up.clone();
                    let r = tokio::task::spawn_blocking(move || {
                        crate::audio::decode::remux_flac_dash_stream(&up_stream, tx)
                    })
                    .await;
                    match r {
                        Ok(Ok(())) => debug!("streaming_dash_remux_stream_ended"),
                        Ok(Err(e)) => warn!(error = %e, "streaming_dash_remux_stream_failed"),
                        Err(e) => warn!(error = %e, "streaming_dash_remux_stream_panic"),
                    }
                    let _ = std::fs::remove_file(&up);
                });
                data_ready.notify_one();
                info!(
                    zone_id = req.zone_id,
                    "streaming_dash_remux_chunked_started"
                );

                let server_ip = self.server_ip();
                let stream_url = self
                    .streamer
                    .get_stream_url(&session_id, &server_ip, "flac");

                let has_title = req.title.as_deref().is_some_and(|s| !s.is_empty());
                let (title, artist, album, duration_ms, cover_path) = if has_title {
                    (
                        req.title.clone().unwrap_or_default(),
                        req.artist_name.clone(),
                        req.album_title.clone(),
                        req.duration_ms,
                        req.cover_url.clone(),
                    )
                } else {
                    match svc.get_track(source_id).await {
                        Ok(track) => (
                            track.title,
                            Some(track.artist),
                            track.album,
                            Some(track.duration_ms as i64),
                            track.cover_path,
                        ),
                        Err(_) => (
                            req.title
                                .clone()
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| "Unknown".into()),
                            req.artist_name.clone(),
                            req.album_title.clone(),
                            req.duration_ms,
                            req.cover_url.clone(),
                        ),
                    }
                };
                return Ok(ResolvedStream {
                    url: stream_url,
                    mime_type: "audio/flac".into(),
                    title,
                    artist,
                    album,
                    duration_ms,
                    source: service_name.into(),
                    cover_url: cover_path,
                    stream_id: Some(session_id),
                    file_size: None,
                    sample_rate: Some(sr),
                    bit_depth: Some(bd as u32),
                    channels: Some(2),
                    origin_url: None,
                    bitrate_kbps: None,
                });
            }

            let tmp_path_clone = tmp_path.clone();
            let unique_path_clone = unique_path.clone();
            // When falling back to WAV/LPCM (renderer has no audio/flac sink),
            // the served WAV is advertised with `DLNA.ORG_PN=LPCM`, a 16-bit-only
            // DLNA profile. A 24-bit Hi-Res stream (Tidal/Qobuz) served under it
            // plays SILENCE on renderers like the Ruark R3 / LHC-62 (Yves,
            // #1137). Cap the LPCM fallback at 16-bit; FLAC keeps full hi-res.
            let dash_is_wav = dash_enc_format == "wav";
            // VU-mètres : le PCM décodé de ce pré-transcode part aussi vers le
            // forwarder de niveaux (cadencé par lui — voir #1105). Sans ça,
            // une piste DASH (Tidal HI-RES) sur DLNA/browser laissait les
            // aiguilles figées. Le chemin remux (opt-in TUNE_DASH_REMUX) ne
            // décode rien : VU légitimement muets dans ce cas.
            let dash_levels_tx = self.levels_forwarder_if_allowed(req.zone_id, 0).await;
            // Durée du pré-transcode, sur la ligne de FIN.
            //
            // C'est l'étape qui domine le démarrage d'une piste DASH (Tidal
            // HI-RES) vers un renderer réseau : décodage intégral en PCM puis
            // ré-encodage, avant que le moindre octet ne parte. Le journal
            // disait qu'elle avait eu lieu, jamais combien elle avait coûté :
            // il fallait soustraire deux horodatages. Or le fichier de journal
            // est plafonné et tourne — la ligne de DÉBUT peut avoir disparu de
            // l'export d'un testeur alors que la ligne de FIN y est encore, et
            // la durée devenait alors impossible à établir. Même convention que
            // `tidal_dash_multi_segment_download_complete`, qui porte déjà son
            // `elapsed_ms` (`streaming/tidal.rs`).
            let pre_transcode_start = std::time::Instant::now();
            let transcode_result = tokio::task::spawn_blocking(move || {
                // Fast path: Tidal HI-RES DASH is ALREADY FLAC (frames inside an
                // fMP4). If the renderer takes FLAC and no zone EQ is active, REMUX
                // (copy the FLAC frames + STREAMINFO into a .flac) instead of
                // decode→PCM→re-encode — a ~59s CPU transcode becomes a sub-second
                // I/O copy, bit-identical (#1146). Opt-in via TUNE_DASH_REMUX;
                // WAV renderers and EQ zones fall through to the decode path.
                let remux = !dash_is_wav
                    && eq_profile_pretranscode.is_none()
                    && std::env::var("TUNE_DASH_REMUX")
                        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
                        .unwrap_or(false);
                if remux {
                    return crate::audio::decode::remux_flac_dash(
                        &unique_path_clone,
                        &tmp_path_clone,
                    );
                }

                let decoded = crate::audio::decode::decode_to_pcm(
                    &unique_path_clone,
                    Some(sr),
                    Some(2),
                    0.0,
                    0.0,
                )?;

                let mut pcm_bytes = decoded.pcm_bytes();
                let mut actual_bd = decoded.bit_depth;

                if dash_is_wav && actual_bd > 16 {
                    pcm_bytes = crate::audio::decode::convert_pcm_bytes(&pcm_bytes, actual_bd, 16);
                    actual_bd = 16;
                }

                if let Some(mut eq) = eq_profile_pretranscode {
                    eq.process_pcm(&mut pcm_bytes, actual_bd);
                }

                // Niveaux post-EQ : les VU décrivent ce qui sera entendu.
                if let Some(ref ltx) = dash_levels_tx {
                    crate::audio::tap::send_windowed_pcm(
                        ltx,
                        &pcm_bytes,
                        actual_bd,
                        decoded.channels as u16,
                        decoded.sample_rate,
                    );
                }

                let rt = tokio::runtime::Handle::try_current()
                    .map_err(|e| format!("no tokio runtime: {e}"))?;
                let encoded_data = rt.block_on(async {
                    let mut encoder = crate::audio::encoder::AudioEncoder::new(
                        dash_enc_format,
                        decoded.sample_rate,
                        actual_bd as u32,
                        decoded.channels,
                    );
                    encoder.start().await?;
                    encoder.write(&pcm_bytes).await?;
                    encoder.finish().await
                })?;

                std::fs::write(&tmp_path_clone, &encoded_data)
                    .map_err(|e| format!("write temp file: {e}"))?;

                let file_size = encoded_data.len() as u64;
                Ok::<(u64, u16, u32), String>((file_size, actual_bd, decoded.sample_rate))
            })
            .await;

            let _ = std::fs::remove_file(&unique_path);

            match transcode_result {
                Ok(Ok((file_size, actual_bd, actual_sr))) => {
                    info!(
                        tmp = %tmp_path,
                        file_size,
                        bit_depth = actual_bd,
                        elapsed_ms = pre_transcode_start.elapsed().as_millis() as u64,
                        "streaming_dash_pre_transcode_complete"
                    );

                    let dash_mime = if dash_enc_format == "flac" {
                        "audio/flac"
                    } else {
                        "audio/wav"
                    };
                    let file_info = StreamInfo {
                        format: dash_enc_format.into(),
                        mime_type: dash_mime.into(),
                        sample_rate: sr,
                        // Use the *encoded* depth (`actual_bd`), which the WAV
                        // fallback caps at 16-bit — otherwise DIDL/WAV would
                        // advertise 24-bit LPCM and the renderer plays silence.
                        bit_depth: actual_bd,
                        channels: 2,
                        file_size: Some(file_size),
                        duration_ms: None,
                        ..Default::default()
                    };
                    // Store into the warm cache (atomic rename) when enabled, so
                    // the next play of this exact track is an instant hit. Any
                    // rename failure falls back to serving the temp file — no
                    // regression. `evict` keeps the cache under its size cap.
                    //
                    // Guard: only store when the DECODED reality matches the key.
                    // `quality.bit_depth`/`sample_rate` come from the service API
                    // and can lie about the actual stream; a later hit would then
                    // advertise a depth/rate the file doesn't have in DIDL — the
                    // Ruark-silence class of bug (#1137, 24-bit LPCM). A skipped
                    // store just means the old temp-file behaviour for this track.
                    let key_matches_reality =
                        warm.as_ref().is_some_and(|w| w.key_bit_depth == actual_bd)
                            && sr == actual_sr;
                    let serve_path = match warm.as_ref() {
                        Some(w)
                            if key_matches_reality
                                && std::fs::rename(&tmp_path, &w.cache_path).is_ok() =>
                        {
                            tokio::task::spawn_blocking(crate::transcode_cache::evict);
                            info!(cache = %w.cache_path, file_size, "streaming_dash_warm_cache_store");
                            w.cache_path.clone()
                        }
                        _ => tmp_path,
                    };
                    // Warm the next streaming track into the cache in the
                    // background (same zone/device → inherit dash_enc_format).
                    if warm.is_some() {
                        self.spawn_warm_next_streaming(
                            req.zone_id,
                            source_id.to_string(),
                            dash_enc_format,
                        );
                    }
                    let session_id = self
                        .streamer
                        .create_file_session(file_info, serve_path, false)
                        .await;

                    let server_ip = self.server_ip();
                    let url =
                        self.streamer
                            .get_stream_url(&session_id, &server_ip, dash_enc_format);
                    (
                        url,
                        Some(session_id),
                        dash_mime.to_string(),
                        Some(file_size),
                    )
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "streaming_dash_pre_transcode_failed");
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(format!("DASH transcode failed: {e}"));
                }
                Err(e) => {
                    warn!(error = %e, "streaming_dash_pre_transcode_task_panic");
                    let _ = std::fs::remove_file(&tmp_path);
                    return Err(format!("DASH transcode task panic: {e}"));
                }
            }
        } else if is_https {
            let codec_lower = stream_data.quality.codec.to_lowercase();
            // Codecs that legacy DLNA renderers can't decode must be
            // pre-transcoded to FLAC. AAC/MP4 (most renderers reject AAC over
            // DLNA) plus Opus/Ogg-Vorbis: YouTube delivers Opus-in-WebM, which
            // old renderers like the Cyrus Stream X reject outright (no
            // audio/webm or audio/opus sink), leaving the transport in
            // ERROR_OCCURRED.
            let needs_flac_transcode = codec_lower == "aac"
                || codec_lower == "mp4"
                || stream_data.mime_type.contains("mp4")
                || AudioFormat::from_extension(&codec_lower)
                    .is_some_and(|f| f.needs_transcode_for_dlna());

            if needs_flac_transcode {
                // AAC/MP4 streams need transcoding for DLNA — most renderers
                // (DMP-A8, etc.) don't support AAC via DLNA.  Pre-transcode to
                // FLAC temp file so we serve with Content-Length (chunked WAV
                // causes noise on many renderers).
                let sr = stream_data.quality.sample_rate;
                let bd = stream_data.quality.bit_depth.max(16).min(24) as u16;

                info!(
                    service = service_name,
                    codec = %codec_lower,
                    sample_rate = sr,
                    "streaming_aac_transcode_to_wav_channel"
                );

                // ── Téléchargement court, puis CANAL streaming ──
                //
                // L'ancien chemin transcodait la PISTE ENTIÈRE en fichier avant
                // de jouer : télécharger + tout décoder + tout encoder = 34 s
                // mesurées entre la décision et le play (Tidal AAC → DMP-A8,
                // .18, 25/08). Le canal WAV — le chemin des DSD et des radios,
                // au contrat rendu honnête en 0.9.106 — démarre dès les
                // premiers blocs décodés. Seul le téléchargement du fichier
                // AAC reste devant le play : quelques secondes.
                let upstream_url = stream_data.url.clone();
                let codec = codec_lower.clone();
                let tmp_dl = std::env::temp_dir()
                    .join(format!("tune-stream-{}.{}", uuid::Uuid::new_v4(), codec))
                    .to_string_lossy()
                    .to_string();
                let tmp_dl_clone = tmp_dl.clone();
                let dl = tokio::task::spawn_blocking(move || {
                    let resp = crate::http::client::blocking_builder()
                        .timeout(std::time::Duration::from_secs(120))
                        .build()
                        .and_then(|c| c.get(&upstream_url).send())
                        .map_err(|e| format!("upstream fetch: {e}"))?;
                    if !resp.status().is_success() {
                        return Err(format!("upstream HTTP {}", resp.status()));
                    }
                    let bytes = resp.bytes().map_err(|e| format!("download: {e}"))?;
                    std::fs::write(&tmp_dl_clone, &bytes).map_err(|e| format!("write dl: {e}"))?;
                    Ok::<(), String>(())
                })
                .await;
                match dl {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        warn!(error = %e, "streaming_aac_download_failed");
                        let _ = std::fs::remove_file(&tmp_dl);
                        return Err(format!("AAC download failed: {e}"));
                    }
                    Err(e) => {
                        warn!(error = %e, "streaming_aac_download_task_panic");
                        let _ = std::fs::remove_file(&tmp_dl);
                        return Err(format!("AAC download task panic: {e}"));
                    }
                }

                let info = StreamInfo {
                    format: "wav".into(),
                    mime_type: "audio/wav".into(),
                    sample_rate: sr,
                    bit_depth: bd,
                    channels: 2,
                    ..Default::default()
                };
                let (session_id, tx, data_ready) =
                    self.streamer.create_session(info, false, 256).await;
                {
                    let sessions = self.streamer.sessions_state();
                    let sessions = sessions.lock().await;
                    if let Some(session) = sessions.get(&session_id) {
                        session
                            .wav_header_included
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }

                let ev_bus = self.event_bus.clone();
                let playback = self.playback.clone();
                let zone_id = req.zone_id;
                let attach_levels = self.levels_attach_allowed(zone_id);
                let fp = tmp_dl.clone();
                tokio::spawn(async move {
                    let err_bus = ev_bus.clone();
                    let levels_tx = match ev_bus.filter(|_| attach_levels) {
                        Some(bus) => {
                            let play_seq = playback.current_play_seq(zone_id).await;
                            spawn_paced_levels_forwarder(bus, playback, zone_id, play_seq, 0)
                        }
                        None => {
                            tokio::sync::mpsc::unbounded_channel::<crate::audio::tap::RawWindow>().0
                        }
                    };
                    let fp_clone = fp.clone();
                    let tx_clone = tx.clone();
                    drop(tx);
                    let result = tokio::task::spawn_blocking(move || {
                        crate::audio::decode::decode_to_pcm_streaming_seeked(
                            &fp_clone,
                            Some(sr),
                            Some(2),
                            Some(bd),
                            tx_clone,
                            32768,
                            data_ready,
                            levels_tx,
                            0.0,
                        )
                    })
                    .await;
                    let _ = std::fs::remove_file(&fp);
                    match result {
                        Ok(Ok(_)) => {
                            debug!("streaming_aac_channel_complete");
                        }
                        Ok(Err(e)) => {
                            warn!(error = %e, "streaming_aac_channel_decode_failed");
                            if let Some(ref bus) = err_bus {
                                bus.emit(
                                    "zone.playback_error",
                                    serde_json::json!({
                                        "zone_id": zone_id,
                                        "error": format!("Impossible de décoder la piste : {e}"),
                                    }),
                                );
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "streaming_aac_channel_task_panic");
                        }
                    }
                });

                let server_ip = self.server_ip();
                let url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");
                (url, Some(session_id), "audio/wav".to_string(), None)
            } else {
                // Non-AAC codecs (FLAC, etc.) — check if the DLNA renderer
                // actually supports this MIME type before proxying directly.
                // Strict renderers (Denon, Marantz, Revox) reject FLAC because
                // their GetProtocolInfo Sink doesn't list audio/flac.  In that
                // case, transcode to WAV (LPCM) which has a proper DLNA.ORG_PN
                // profile and is universally supported.
                let zone = ZoneRepo::with_backend(self.db.clone())
                    .get(req.zone_id)
                    .ok()
                    .flatten();
                let zone_output_type = zone.as_ref().and_then(|z| z.output_type.clone());
                let is_dlna = zone_output_type.as_deref() == Some("dlna");
                let device_id = req
                    .output_device_id
                    .as_deref()
                    .or(zone.as_ref().and_then(|z| z.output_device_id.as_deref()))
                    .unwrap_or("");
                let renderer_supports_mime = if is_dlna
                    && (stream_data.mime_type == "audio/flac"
                        || stream_data.mime_type == "audio/x-flac")
                    && !device_id.is_empty()
                {
                    self.dlna_supports_mime(device_id, &stream_data.mime_type)
                        .await
                } else {
                    true
                };

                if !renderer_supports_mime {
                    // Renderer does not support FLAC — transcode to WAV (LPCM).
                    // Same pattern as AAC pre-transcode: download → decode → encode → file session.
                    let sr = stream_data.quality.sample_rate;
                    let bd = stream_data.quality.bit_depth.max(16).min(24);

                    info!(
                        service = service_name,
                        codec = %codec_lower,
                        device = %device_id,
                        sample_rate = sr,
                        bit_depth = bd,
                        "streaming_flac_transcode_to_wav_renderer_unsupported"
                    );

                    let upstream_url = stream_data.url.clone();
                    let tmp_dl = std::env::temp_dir()
                        .join(format!(
                            "tune-stream-{}.{}",
                            uuid::Uuid::new_v4(),
                            codec_lower
                        ))
                        .to_string_lossy()
                        .to_string();
                    let tmp_wav = std::env::temp_dir()
                        .join(format!("tune-flac-to-wav-{}.wav", uuid::Uuid::new_v4()))
                        .to_string_lossy()
                        .to_string();

                    let tmp_dl_clone = tmp_dl.clone();
                    let tmp_wav_clone = tmp_wav.clone();
                    // VU-mètres : même ajout que les autres pré-transcodes —
                    // le PCM décodé alimente le forwarder de niveaux.
                    let wav_levels_tx = self.levels_forwarder_if_allowed(req.zone_id, 0).await;
                    let transcode_result = tokio::task::spawn_blocking(move || {
                        // 1. Download
                        let resp = crate::http::client::blocking_builder()
                            .timeout(std::time::Duration::from_secs(120))
                            .build()
                            .and_then(|c| c.get(&upstream_url).send())
                            .map_err(|e| format!("upstream fetch: {e}"))?;
                        if !resp.status().is_success() {
                            return Err(format!("upstream HTTP {}", resp.status()));
                        }
                        let bytes = resp.bytes().map_err(|e| format!("download: {e}"))?;
                        std::fs::write(&tmp_dl_clone, &bytes)
                            .map_err(|e| format!("write dl: {e}"))?;

                        // 2. Decode to PCM
                        let decoded = crate::audio::decode::decode_to_pcm(
                            &tmp_dl_clone,
                            Some(sr),
                            Some(2),
                            0.0,
                            0.0,
                        )?;
                        let mut pcm_bytes = decoded.pcm_bytes();
                        let mut actual_bd = decoded.bit_depth;
                        let actual_sr = decoded.sample_rate;
                        let actual_ch = decoded.channels;

                        // The renderer rejected FLAC, so we serve WAV/LPCM
                        // (DLNA.ORG_PN=LPCM), a 16-bit-only DLNA profile. A
                        // 24-bit Hi-Res FLAC (Qobuz/Tidal) served under it plays
                        // SILENCE on strict renderers like the Ruark R3 / LHC-62
                        // (Yves, #1137). Cap to 16-bit so the WAV matches the
                        // advertised LPCM profile and is audible.
                        if actual_bd > 16 {
                            pcm_bytes =
                                crate::audio::decode::convert_pcm_bytes(&pcm_bytes, actual_bd, 16);
                            actual_bd = 16;
                        }

                        if let Some(ref ltx) = wav_levels_tx {
                            crate::audio::tap::send_windowed_pcm(
                                ltx,
                                &pcm_bytes,
                                actual_bd,
                                actual_ch as u16,
                                actual_sr,
                            );
                        }

                        // 3. Encode to WAV
                        let rt = tokio::runtime::Handle::try_current()
                            .map_err(|e| format!("no tokio runtime: {e}"))?;
                        let encoded_data = rt.block_on(async {
                            let mut encoder = crate::audio::encoder::AudioEncoder::new(
                                "wav",
                                actual_sr,
                                actual_bd as u32,
                                actual_ch,
                            );
                            encoder.start().await?;
                            encoder.write(&pcm_bytes).await?;
                            encoder.finish().await
                        })?;

                        std::fs::write(&tmp_wav_clone, &encoded_data)
                            .map_err(|e| format!("write wav: {e}"))?;

                        let _ = std::fs::remove_file(&tmp_dl_clone);
                        let file_size = encoded_data.len() as u64;
                        Ok::<(u64, u16, u32, u16), String>((
                            file_size,
                            actual_bd,
                            actual_sr,
                            actual_ch as u16,
                        ))
                    })
                    .await;

                    match transcode_result {
                        Ok(Ok((file_size, actual_bd, actual_sr, actual_ch))) => {
                            info!(
                                tmp = %tmp_wav,
                                file_size,
                                bit_depth = actual_bd,
                                sample_rate = actual_sr,
                                "streaming_flac_to_wav_transcode_complete"
                            );

                            let file_info = StreamInfo {
                                format: "wav".into(),
                                mime_type: "audio/wav".into(),
                                sample_rate: actual_sr,
                                bit_depth: actual_bd,
                                channels: actual_ch,
                                file_size: Some(file_size),
                                duration_ms: None,
                                ..Default::default()
                            };
                            let session_id = self
                                .streamer
                                .create_file_session(file_info, tmp_wav, false)
                                .await;

                            let server_ip = self.server_ip();
                            let url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");
                            (
                                url,
                                Some(session_id),
                                "audio/wav".to_string(),
                                Some(file_size),
                            )
                        }
                        Ok(Err(e)) => {
                            warn!(error = %e, "streaming_flac_to_wav_transcode_failed");
                            let _ = std::fs::remove_file(&tmp_dl);
                            let _ = std::fs::remove_file(&tmp_wav);
                            return Err(format!("FLAC→WAV transcode failed: {e}"));
                        }
                        Err(e) => {
                            warn!(error = %e, "streaming_flac_to_wav_transcode_task_panic");
                            let _ = std::fs::remove_file(&tmp_dl);
                            let _ = std::fs::remove_file(&tmp_wav);
                            return Err(format!("FLAC→WAV transcode task panic: {e}"));
                        }
                    }
                } else {
                    // Renderer supports FLAC — proxy directly as before.
                    //
                    // Qobuz/Tidal signed CDN URLs carry a short TTL (Qobuz
                    // `etsp=<unix-expiry>`, ~60 min). On a long Hi-Res track the
                    // URL expires mid-playback and a client Range-resume against
                    // the stored URL fails at the connection/auth level. Attach a
                    // re-resolver so the proxy layer can fetch a FRESH signed URL
                    // for the same track+quality and resume byte-exact (#1136).
                    // Only for real https CDN URLs (not file:// DASH assemblies).
                    let reresolve: Option<crate::http::streamer::ReresolveFn> = if is_https {
                        let services = self.services.clone();
                        let service_name = service_name.to_string();
                        let source_id = source_id.to_string();
                        Some(std::sync::Arc::new(move || {
                            let services = services.clone();
                            let service_name = service_name.clone();
                            let source_id = source_id.clone();
                            Box::pin(async move {
                                let registry = services.lock().await;
                                let svc = registry
                                    .get(&service_name)
                                    .ok_or_else(|| format!("unknown service: {service_name}"))?;
                                let mut svc = svc.write().await;
                                // Best-effort token refresh, then re-resolve with
                                // the same default quality the initial play used.
                                let _ = svc.refresh_if_needed().await;
                                match svc.get_track_url(&source_id, None).await {
                                    Ok(data) => Ok(data.url),
                                    Err(e) => Err(e.to_string()),
                                }
                            })
                                as std::pin::Pin<
                                    Box<
                                        dyn std::future::Future<Output = Result<String, String>>
                                            + Send,
                                    >,
                                >
                        }))
                    } else {
                        None
                    };
                    let session_id = self
                        .streamer
                        .create_proxy_session_with_reresolve(
                            info,
                            stream_data.url.clone(),
                            false,
                            reresolve,
                        )
                        .await;
                    let server_ip = self.server_ip();
                    let url = self
                        .streamer
                        .get_stream_url(&session_id, &server_ip, &codec_lower);

                    // VU-mètres (#1106) : le proxy sert les octets CDN
                    // verbatim, rien n'est décodé côté serveur → aucun
                    // `playback.audio_levels`, aiguilles figées sur Qobuz/
                    // Tidal direct alors qu'une piste locale les anime. On
                    // décode le même flux en parallèle, uniquement pour les
                    // niveaux — le flux servi reste bit-perfect.
                    //
                    // On tape NOTRE session proxy (`url`, localhost) et non
                    // l'URL CDN signée `stream_data.url` : le navigateur, en
                    // consommant le proxy, fait re-résoudre une URL signée
                    // fraîche, et l'ancienne signature tapée directement était
                    // rejetée par le CDN (aucune fenêtre décodée → aiguilles
                    // figées, la 1re version du fix #1247). Passer par le proxy
                    // réutilise sa re-résolution / reprise et sert exactement
                    // les octets joués. Le bridage ≤30 s en avance impose une
                    // contre-pression TCP : le proxy ne pré-télécharge pas
                    // toute la piste.
                    self.spawn_proxy_levels_probe(req.zone_id, url.clone(), codec_lower.clone())
                        .await;

                    // Report the mime of the codec we actually serve, not the
                    // upstream API's mime_type. Qobuz can return a mime that does
                    // not normalise to a lossless format, so Now Playing showed
                    // FLAC tracks as "compressé"/lossy (Progman). codec_lower is
                    // authoritative for what the proxy streams.
                    (url, Some(session_id), format!("audio/{codec_lower}"), None)
                }
            }
        } else {
            (
                stream_data.url.clone(),
                None,
                stream_data.mime_type.clone(),
                None,
            )
        };

        // Only trust the caller-supplied title when it is actually non-empty.
        // Repeat All (and some queue paths) re-play a streaming_queue row whose
        // stored title is "" — `req.title` is then Some("") and the old
        // `is_some()` check served that empty title verbatim, wiping Now Playing
        // (DEvir: `auto_next title=Shine...` followed by `orchestrator_play
        // title=`). Falling through to get_track() refetches the real metadata
        // from the service. The network call only fires when the title is
        // missing, so the happy path is unchanged.
        let has_title = req.title.as_deref().is_some_and(|s| !s.is_empty());
        let (title, artist, album, mut duration_ms, cover_path) = if has_title {
            (
                req.title.clone().unwrap_or_default(),
                req.artist_name.clone(),
                req.album_title.clone(),
                req.duration_ms,
                req.cover_url.clone(),
            )
        } else {
            match svc.get_track(source_id).await {
                Ok(track) => (
                    track.title,
                    Some(track.artist),
                    track.album,
                    Some(track.duration_ms as i64),
                    track.cover_path,
                ),
                Err(_) => (
                    req.title
                        .clone()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "Unknown".into()),
                    req.artist_name.clone(),
                    req.album_title.clone(),
                    req.duration_ms,
                    req.cover_url.clone(),
                ),
            }
        };

        // Duration backfill, mirroring serve_prefetched_pcm (#497): a non-empty
        // title with duration 0 skips the get_track branch above, and duration 0
        // on an EXCLUSIVE local output disarms the poller's position-past-end
        // advance (#483, which requires duration > 0) — on a Repeat All loop
        // transition the ring then starved at exactly one track length and
        // playback froze forever (DEvir, v0.9.14, ASIO, DASH file reused from
        // disk). The network call only fires in the degraded duration-0 case,
        // so the happy path is unchanged.
        if duration_ms.unwrap_or(0) == 0
            && let Ok(track) = svc.get_track(source_id).await
            && track.duration_ms > 0
        {
            duration_ms = Some(track.duration_ms as i64);
        }

        // Same contract as the radio branch: every path above may have replaced
        // the service's signed CDN URL with one of our own proxy or transcode
        // endpoints. Keep the upstream so an output that wants the bytes as the
        // service published them — a recorder keeping the original FLAC instead
        // of the proxy's re-stream or a WAV transcode — can ask for them. `None`
        // when we are handing out the upstream unchanged.
        let origin_url = (stream_url != stream_data.url).then(|| stream_data.url.clone());

        Ok(ResolvedStream {
            url: stream_url,
            mime_type: out_mime,
            title,
            artist,
            album,
            duration_ms,
            source: service_name.into(),
            cover_url: cover_path,
            stream_id: sid,
            file_size: stream_file_size,
            sample_rate: Some(stream_data.quality.sample_rate),
            bit_depth: Some(stream_data.quality.bit_depth as u32),
            channels: Some(2),
            origin_url,
            bitrate_kbps: None,
        })
    }

    /// Serve prefetched PCM data as a WAV stream session.
    ///
    /// Creates a streaming session and feeds the already-decoded PCM into it,
    /// bypassing the download+decode pipeline entirely.
    async fn serve_prefetched_pcm(
        &self,
        prefetched: crate::prefetch::PrefetchedTrack,
        req: &PlayRequest,
    ) -> Result<ResolvedStream, String> {
        let sr = prefetched.sample_rate;
        let bd = prefetched.bit_depth;
        let ch = prefetched.channels;

        // Prefer the request's metadata (from now_playing) over the prefetch
        // buffer's. The buffer is built for the *next* track and can carry an
        // empty title (prefetched before its metadata was resolved); serving it
        // verbatim after a seek wipes the Now Playing title (DEvir: title
        // disappears when seeking shortly after a TIDAL track starts).
        let mut title = req
            .title
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| prefetched.title.clone());
        let mut artist = req
            .artist_name
            .clone()
            .or_else(|| prefetched.artist.clone());
        let mut album = req.album_title.clone().or_else(|| prefetched.album.clone());
        let mut cover_url = req
            .cover_url
            .clone()
            .or_else(|| prefetched.cover_url.clone());

        // Duration can also be missing from the prefetch buffer (metadata not
        // resolved at prefetch time → `prefetched.duration_ms == 0`). Serving a
        // zero duration is worse than a blank title: on an exclusive output the
        // poller's position-based end detection needs duration > 0, so a
        // 0-duration repeat can only advance via the 45 s load-grace timeout —
        // and since the next repeat inherits 0 again, playback falls into an
        // infinite 45 s silent loading loop (DEvir: seek under Repeat One).
        // Prefer the prefetch value, fall back to the request, then recover from
        // the service metadata below alongside the title.
        let mut duration_ms: u64 = if prefetched.duration_ms > 0 {
            prefetched.duration_ms
        } else {
            req.duration_ms
                .filter(|d| *d > 0)
                .map(|d| d as u64)
                .unwrap_or(0)
        };

        // Both the request and the prefetch buffer can carry an empty title when
        // the streaming_queue row was persisted without metadata (DEvir: Repeat
        // All on a single-track queue prefetches itself, then re-plays via this
        // prefetched path with `title=""` — auto_next logs the right title but
        // orchestrator_play/Now Playing go blank). When that (or a missing
        // duration) happens, refetch the real metadata from the service so Now
        // Playing is never blanked and end detection has a duration.
        if title.is_empty() || duration_ms == 0 {
            let registry = self.services.lock().await;
            if let Some(svc) = registry.get(&prefetched.source) {
                let svc = svc.read().await;
                if let Ok(track) = svc.get_track(&prefetched.source_id).await {
                    if title.is_empty() {
                        title = track.title;
                        artist = artist.or(Some(track.artist));
                        album = album.or(track.album);
                        cover_url = cover_url.or(track.cover_path);
                    }
                    if duration_ms == 0 && track.duration_ms > 0 {
                        duration_ms = track.duration_ms;
                    }
                }
            }
        }

        // Determine output bit depth based on output type
        let is_local_stream = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| id.starts_with("local:"));
        let is_network_output = req
            .output_device_id
            .as_deref()
            .is_some_and(|id| !id.starts_with("local:") && !id.starts_with("oaat:"));
        let out_bd = if is_local_stream {
            32
        } else {
            bd.max(16).min(24)
        };

        // For DLNA/network outputs, encode prefetched PCM to a file.
        // Use FLAC if the renderer supports it, otherwise WAV.
        if is_network_output {
            let use_wav = if let Some(device_id) = req.output_device_id.as_deref() {
                !self.dlna_supports_mime(device_id, "audio/flac").await
            } else {
                false
            };
            let ext = if use_wav { "wav" } else { "flac" };
            let tmp_path =
                std::env::temp_dir().join(format!("tune-prefetch-{}.{ext}", uuid::Uuid::new_v4()));
            let tmp_str = tmp_path.to_string_lossy().to_string();
            // Match the encoded header's bit depth (out_bd) to the actual PCM.
            let pcm_data = if bd != out_bd {
                crate::audio::decode::convert_pcm_bytes(&prefetched.pcm_data, bd, out_bd)
            } else {
                prefetched.pcm_data
            };
            let encode_sr = sr;
            let encode_bd = out_bd;
            let encode_ch = ch;
            let encode_path = tmp_str.clone();
            let encode_wav = use_wav;
            // VU-mètres : le tampon prefetch EST le PCM décodé — sans ce
            // renvoi, une piste streaming servie depuis le prefetch (gapless
            // N+1) laissait les aiguilles figées alors que la piste jouée via
            // le pipeline download+decode les animait.
            let prefetch_levels_tx = self.levels_forwarder_if_allowed(req.zone_id, 0).await;
            tokio::task::spawn_blocking(move || {
                use std::io::Write;
                if let Some(ref ltx) = prefetch_levels_tx {
                    crate::audio::tap::send_windowed_pcm(
                        ltx,
                        &pcm_data,
                        encode_bd,
                        encode_ch as u16,
                        encode_sr,
                    );
                }
                let data_size = pcm_data.len() as u32;
                let byte_rate = encode_sr * encode_ch as u32 * (encode_bd as u32 / 8);
                let block_align = encode_ch as u16 * (encode_bd as u16 / 8);
                if encode_wav {
                    let mut f = std::fs::File::create(&encode_path)
                        .map_err(|e| format!("create tmp wav: {e}"))?;
                    let mut hdr = Vec::with_capacity(44);
                    hdr.extend_from_slice(b"RIFF");
                    hdr.extend_from_slice(&(36 + data_size).to_le_bytes());
                    hdr.extend_from_slice(b"WAVEfmt ");
                    hdr.extend_from_slice(&16u32.to_le_bytes());
                    hdr.extend_from_slice(&1u16.to_le_bytes());
                    hdr.extend_from_slice(&(encode_ch as u16).to_le_bytes());
                    hdr.extend_from_slice(&encode_sr.to_le_bytes());
                    hdr.extend_from_slice(&byte_rate.to_le_bytes());
                    hdr.extend_from_slice(&block_align.to_le_bytes());
                    hdr.extend_from_slice(&(encode_bd as u16).to_le_bytes());
                    hdr.extend_from_slice(b"data");
                    hdr.extend_from_slice(&data_size.to_le_bytes());
                    f.write_all(&hdr)
                        .map_err(|e| format!("write wav header: {e}"))?;
                    f.write_all(&pcm_data)
                        .map_err(|e| format!("write wav pcm: {e}"))?;
                    Ok::<(), String>(())
                } else {
                    // Encodage FLAC NATIF. Le chemin precedent ecrivait un WAV
                    // temporaire puis lancait `ffmpeg -c:a flac` — un binaire
                    // externe retire du projet en v0.8.46, donc un echec
                    // systematique partout ou il n'est pas installe par
                    // ailleurs, et un fichier de cache jamais produit.
                    //
                    // `AudioEncoder` est deja dans l'arbre et fait le meme
                    // travail sans processus externe ni fichier intermediaire.
                    // On est dans un `spawn_blocking`, donc les variantes
                    // `_sync` sont exactement ce qu'il faut (cf. leur
                    // documentation : pas d'await, encodage pur CPU).
                    let mut enc = crate::audio::encoder::AudioEncoder::new(
                        "flac",
                        encode_sr,
                        encode_bd as u32,
                        encode_ch as u32,
                    );
                    enc.start_sync()?;
                    enc.write_sync(&pcm_data)?;
                    let flac = enc.finish_sync()?;
                    std::fs::write(&encode_path, &flac).map_err(|e| format!("write flac: {e}"))?;
                    Ok(())
                }
            })
            .await
            .map_err(|e| format!("spawn: {e}"))??;

            let file_size = std::fs::metadata(&tmp_str).map(|m| m.len()).unwrap_or(0);
            let (out_format, out_mime) = if use_wav {
                ("wav", "audio/wav")
            } else {
                ("flac", "audio/flac")
            };
            info!(
                title = %prefetched.title,
                file_size,
                format = out_format,
                "prefetch_pcm_encoded_for_dlna"
            );

            let flac_info = StreamInfo {
                format: out_format.into(),
                mime_type: out_mime.into(),
                sample_rate: sr,
                bit_depth: out_bd,
                channels: ch,
                file_size: Some(file_size),
                duration_ms: Some(duration_ms),
                ..Default::default()
            };

            let session_id = self
                .streamer
                .create_file_session(flac_info, tmp_str.clone(), false)
                .await;

            let server_ip = self.server_ip();
            let stream_url = self
                .streamer
                .get_stream_url(&session_id, &server_ip, "flac");

            return Ok(ResolvedStream {
                url: stream_url,
                stream_id: Some(session_id),
                title: title.clone(),
                artist: artist.clone(),
                album: None,
                duration_ms: Some(duration_ms as i64),
                source: prefetched.source,
                mime_type: "audio/flac".into(),
                sample_rate: Some(sr),
                bit_depth: Some(out_bd as u32),
                channels: Some(ch as u32),
                // What we serve here is a local session over the decoded buffer;
                // the service's own URL travels on `PrefetchedTrack` so a
                // recorder still gets the published bytes rather than our
                // re-encode. A track shorter than the prefetch window is served
                // from this path, so without it short tracks were the only ones
                // captured through the proxy — and filed under `Stream/`.
                origin_url: prefetched.upstream_url,
                bitrate_kbps: None,
                cover_url: cover_url.clone(),
                file_size: Some(file_size),
            });
        }

        let wav_info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: sr,
            bit_depth: out_bd,
            channels: ch,
            file_size: None,
            duration_ms: Some(duration_ms),
            ..Default::default()
        };

        let (session_id, tx, data_ready) = self.streamer.create_session(wav_info, false, 256).await;

        // Feed the prefetched PCM data into the session in chunks.
        // This happens nearly instantly since the data is already in memory.
        // The buffer is stored at the source bit depth (`bd`); widen it to the
        // WAV header's `out_bd` (32 for local output) or the device reads 32-bit
        // frames out of 16-bit data → white noise (Bilou: bruit blanc next-track).
        let pcm_data = if bd != out_bd {
            info!(
                from_bd = bd,
                to_bd = out_bd,
                "prefetch_pcm_bit_depth_converted"
            );
            crate::audio::decode::convert_pcm_bytes(&prefetched.pcm_data, bd, out_bd)
        } else {
            prefetched.pcm_data
        };
        // VU-mètres : même renvoi que la branche réseau — le tampon prefetch
        // est le PCM décodé, il alimente le forwarder de niveaux. Fenêtrage
        // AVANT le gavage de la session (une passe memcpy, quelques dizaines
        // de ms) : le gavage, lui, dure toute la piste (canal borné, rythmé
        // par le client), les niveaux seraient arrivés trop tard.
        let prefetch_levels_tx = self.levels_forwarder_if_allowed(req.zone_id, 0).await;
        let pcm_data = std::sync::Arc::new(pcm_data);
        tokio::spawn(async move {
            if let Some(ltx) = prefetch_levels_tx {
                let levels_pcm = pcm_data.clone();
                let levels_bd = out_bd;
                let levels_ch = ch;
                let levels_sr = sr;
                tokio::task::spawn_blocking(move || {
                    crate::audio::tap::send_windowed_pcm(
                        &ltx,
                        &levels_pcm,
                        levels_bd,
                        levels_ch as u16,
                        levels_sr,
                    );
                });
            }
            let chunk_size = 32768;
            let mut first = true;
            for chunk in pcm_data.chunks(chunk_size) {
                if tx.send(chunk.to_vec()).await.is_err() {
                    debug!("prefetch_session_consumer_dropped");
                    return;
                }
                if first {
                    first = false;
                    data_ready.notify_one();
                }
            }
            if first {
                // No data was sent (empty buffer)
                data_ready.notify_one();
            }
            debug!("prefetch_pcm_feed_complete");
        });

        let server_ip = self.server_ip();
        let stream_url = self.streamer.get_stream_url(&session_id, &server_ip, "wav");

        Ok(ResolvedStream {
            url: stream_url,
            mime_type: "audio/wav".into(),
            title: title.clone(),
            artist: artist.clone(),
            album: album.clone(),
            duration_ms: Some(duration_ms as i64),
            source: prefetched.source,
            cover_url: cover_url.clone(),
            stream_id: Some(session_id),
            file_size: None,
            sample_rate: Some(sr),
            bit_depth: Some(out_bd as u32),
            channels: Some(ch as u32),
            // Same as the FLAC-session branch above: this is a WAV session over
            // the decoded buffer, so an output that wants the source container
            // needs the service's own URL, not ours.
            origin_url: prefetched.upstream_url,
            bitrate_kbps: None,
        })
    }

    /// Convert a cover_path (which may be a short hash or a full URL) into an
    /// absolute HTTP URL accessible by network renderers (DLNA/OpenHome).
    /// Hash-only values like `"abc123def"` become `http://IP:PORT/api/v1/artwork/abc123def`.
    /// Full URLs (starting with `http://` or `https://`) are passed through unchanged.
    fn resolve_cover_url(&self, cover: Option<&str>) -> Option<String> {
        let c = cover?;
        if c.starts_with("http://") || c.starts_with("https://") {
            return Some(c.to_string());
        }
        // It's a local artwork hash — build an absolute URL
        let server_ip = self.server_ip();
        // Use the streamer port (same as API server port)
        let port = std::env::var("TUNE_PORT")
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(8888);
        Some(format!(
            "http://{server_ip}:{port}/api/v1/library/artwork/{c}"
        ))
    }

    /// Recreate a local (cpal) output on demand and play to it. Only the
    /// `local-audio` build has `outputs::local`; without that feature there is
    /// no local backend, so this is a no-op that reports the device as missing.
    #[cfg(feature = "local-audio")]
    async fn recreate_local_and_play(
        &self,
        device_id: &str,
        media: &crate::outputs::traits::PlayMedia<'_>,
        start_position_ms: Option<u64>,
    ) -> (bool, Option<String>) {
        let device_name = device_id.strip_prefix("local:").unwrap_or(device_id);
        info!(device_id, "output_not_found_recreating_local_output");
        let local_out = crate::outputs::local::LocalOutput::new(device_name.to_string());
        if let Some(position_ms) = start_position_ms {
            local_out.set_pending_start_position_ms(position_ms);
            // Producer always pre-seeked — see the comment in send_to_output.
            local_out.set_producer_seeked(true);
        }
        {
            let mut outputs = self.outputs.lock().await;
            outputs.register(Box::new(local_out));
        }
        let arc = { self.outputs.lock().await.get(device_id) };
        if let Some(arc) = arc {
            let output = arc.lock().await;
            match output.play_media(media).await {
                Ok(()) => {
                    drop(output);
                    info!(device_id, "output_play_sent_after_recreate");
                    (true, None)
                }
                Err(e) => {
                    drop(output);
                    warn!(device_id, error = %e, "output_play_failed_after_recreate");
                    (false, Some(format!("Output device error: {e}")))
                }
            }
        } else {
            (false, Some(format!("Device not found: {device_id}")))
        }
    }

    #[cfg(not(feature = "local-audio"))]
    async fn recreate_local_and_play(
        &self,
        device_id: &str,
        _media: &crate::outputs::traits::PlayMedia<'_>,
        _start_position_ms: Option<u64>,
    ) -> (bool, Option<String>) {
        (false, Some(format!("Device not found: {device_id}")))
    }

    /// `output_type()` of the registered output for `device_id` (e.g. "dlna",
    /// "local", "openhome"), or `None` when the device is not registered. Used
    /// to gate the initial DLNA prebuffer barrier (#1259) to DLNA outputs only,
    /// and the duplicate-net-play coalescing (#1129) to push-URI outputs only.
    async fn output_type_of(&self, device_id: &str) -> Option<String> {
        let arc = { self.outputs.lock().await.get(device_id) };
        match arc {
            Some(arc) => Some(arc.lock().await.output_type().to_string()),
            None => None,
        }
    }

    async fn send_to_output(
        &self,
        device_id: &str,
        media: &crate::outputs::traits::PlayMedia<'_>,
        start_position_ms: Option<u64>,
        zone_audiophile: bool,
        zone_id: i64,
        track_id: Option<i64>,
    ) -> (bool, Option<String>) {
        let lock_start = std::time::Instant::now();
        let (output_arc, used_device_id) = {
            let outputs = self.outputs.lock().await;
            let elapsed = lock_start.elapsed();
            if elapsed.as_millis() > 200 {
                warn!(
                    device_id,
                    elapsed_ms = elapsed.as_millis() as u64,
                    "send_to_output_lock_contention"
                );
            }
            // Bug 2 fix: never fall back to another zone/device.
            // If the exact requested device is not found, return an error so
            // audio never comes out of an unexpected speaker.
            match outputs.get(device_id) {
                Some(arc) => (Some(arc), device_id.to_string()),
                None => (None, device_id.to_string()),
            }
        };
        if let Some(output_arc) = output_arc {
            // For OAAT outputs, arm the start position before play: the
            // native DSD direct path reads the local file itself and must
            // seek to this offset (the seek-positioned HTTP transcode URL
            // is bypassed on that path).
            //
            // Gated on `oaat`: `crate::outputs::oaat` only exists with that
            // feature (outputs/mod.rs), so an ungated reference broke the
            // postgres-without-oaat build (test-postgres "Engine module tests"),
            // mirroring the `local-audio` gate on the analogous block below.
            #[cfg(feature = "oaat")]
            if let Some(position_ms) = start_position_ms {
                if device_id.starts_with("oaat:") {
                    let output = output_arc.lock().await;
                    if let Some(oaat_output) = output
                        .as_any()
                        .downcast_ref::<crate::outputs::oaat::OaatOutput>()
                    {
                        oaat_output.set_pending_start_position_ms(position_ms);
                    }
                    drop(output);
                }
            }
            // For local outputs, set the pending start position before play
            #[cfg(feature = "local-audio")]
            if let Some(position_ms) = start_position_ms {
                if device_id.starts_with("local:") {
                    let output = output_arc.lock().await;
                    if let Some(local_output) = output
                        .as_any()
                        .downcast_ref::<crate::outputs::local::LocalOutput>()
                    {
                        local_output.set_pending_start_position_ms(position_ms);
                        // The WAV a local output receives is ALWAYS pre-seeked:
                        // since b3a4a79f the streaming (Qobuz/Tidal) transcode
                        // arm feeds seek_s to decode_to_pcm_streaming_seeked,
                        // exactly like the local-file arm. Deriving this flag
                        // from media.file_path (streaming → false → consumer
                        // byte-skip) made the output skip the seek offset a
                        // SECOND time on an already-seeked stream: a seek at
                        // 4:30 discarded the entire remainder of the track —
                        // silence, then a ~3s restart loop as the poller kept
                        // recovering the "ended" track (Vincent, #1518).
                        local_output.set_producer_seeked(true);
                    }
                    drop(output);
                }
            }
            // PURE (audiophile) mode: bypass the room-correction convolver on
            // this local output for the zone about to play, so the signal path
            // stays bit-perfect. Applied every play (not just on seek) so a zone
            // toggled in/out of PURE takes effect on the next track; other zones
            // on the same output keep their convolution.
            #[cfg(feature = "local-audio")]
            if device_id.starts_with("local:") {
                let output = output_arc.lock().await;
                if let Some(local_output) = output
                    .as_any()
                    .downcast_ref::<crate::outputs::local::LocalOutput>()
                {
                    local_output.set_pure_bypass(zone_audiophile);
                    // ReplayGain, applied by the output itself for a local DAC.
                    // A PURE zone is left strictly alone: applying a gain would
                    // multiply every sample and the path would no longer be
                    // bit-perfect, which is the one thing PURE promises.
                    let rg = match (zone_audiophile, track_id) {
                        (false, Some(tid)) => {
                            crate::audio::replaygain::playback_factor(&self.db, tid)
                        }
                        _ => 1.0,
                    };
                    local_output.set_replaygain_factor(rg);
                    // Headphone crossfeed (local DAC only). Returns None when the
                    // zone has crossfeed disabled OR is in PURE mode, so a PURE
                    // zone stays bit-perfect. Rebuilt per-play at the resolved
                    // stream sample rate so the delay line matches the DAC clock.
                    let cf_sr = media.sample_rate.unwrap_or(44100);
                    local_output.set_crossfeed(self.load_crossfeed_processor(zone_id, cf_sr));
                    // Zone equalizer, applied by the output itself for a local
                    // DAC — exactly like the crossfeed above, and for the same
                    // reason: a local zone never goes through
                    // `transcode_source_to_file`, the ONLY place the
                    // `EqProcessor` was ever run. `use_file_transcode_for`
                    // requires a network output, so a local zone always took the
                    // streaming pipe, which touches no DSP. Result: the EQ was
                    // applied nowhere at all on a local output — profil
                    // enregistré, courbe affichée, zéro effet au DAC (Jean
                    // Marie, forum #1416 : ses journaux ne montrent QUE des
                    // sorties `local:`, sur les deux zones qu'il a essayées).
                    //
                    // `load_eq_processor` returns None in PURE mode, so the
                    // bit-perfect promise is kept without a second guard.
                    // Built at the resolved stream's rate/channel count so the
                    // biquads match the DAC clock, mirroring the crossfeed.
                    let eq_ch = media.channels.unwrap_or(2).clamp(1, 8) as u16;
                    local_output.set_eq(self.load_eq_processor(zone_id, cf_sr, eq_ch));
                    // Repli mono (#2362) — sortie LOCALE uniquement, comme le
                    // crossfeed juste au-dessus. `zone_mono_downmix` rend
                    // `false` en mode PURE, donc la promesse bit-perfect tient
                    // sans garde supplémentaire, exactement comme pour l'EQ.
                    local_output.set_mono_downmix(self.zone_mono_downmix(zone_id));
                }
                drop(output);
            }
            let output = output_arc.lock().await;
            match output.play_media(media).await {
                Ok(()) => {
                    drop(output);
                    info!(device_id = %used_device_id, "output_play_sent");
                    (true, None)
                }
                Err(e) => {
                    drop(output);
                    warn!(device_id = %used_device_id, error = %e, "output_play_failed");
                    // Le message rendu à l'utilisateur nomme le RÉGLAGE quand
                    // le serveur savait, avant d'envoyer, que la zone forçait
                    // du DSD brut vers un lecteur qui annonce ne pas le lire
                    // (#2396). Relecture sur le chemin d'erreur uniquement :
                    // aucun coût sur une lecture qui démarre.
                    let (dsd_mode, annonce) = self.contexte_dsd(zone_id, &used_device_id).await;
                    (
                        false,
                        Some(Self::message_echec_sortie(
                            &e,
                            &dsd_mode,
                            annonce,
                            media.mime_type,
                        )),
                    )
                }
            }
        } else if device_id.starts_with("local:") {
            self.recreate_local_and_play(device_id, media, start_position_ms)
                .await
        } else {
            warn!(device_id, "output_not_found");
            (
                false,
                Some(format!(
                    "Device not yet discovered: {device_id}. Please retry in a few seconds."
                )),
            )
        }
    }

    /// Pre-transcode the NEXT local queue track into the transcode cache while
    /// the current one plays, so its play is a cache hit — this masks the ~30s
    /// file-transcode latency across an album (the per-track transition gap
    /// Yves hears on DLNA). Best-effort, background, and a no-op unless the next
    /// track decodes to the SAME PCM params (sample rate / bit depth / channels,
    /// non-DSD): only then is the negotiated output — and thus the cache key —
    /// guaranteed identical to what the next track's real play would produce.
    /// Callers fire this only when the current track was itself cached (no EQ).
    #[allow(clippy::too_many_arguments)]
    fn spawn_warm_next_local(
        &self,
        zone_id: i64,
        cur_sr: u32,
        cur_bd: u16,
        cur_ch: u16,
        out_ext: String,
        out_sr: u32,
        out_bd: u16,
        target_fmt: String,
    ) {
        let db = self.db.clone();
        tokio::spawn(async move {
            // Locate the current queue position and the item right after it.
            let qrepo = PlayQueueRepo::with_backend(db.clone());
            let queue = match qrepo.get_queue(zone_id) {
                Ok(q) => q,
                Err(_) => return,
            };
            let Some(cur_pos) = queue.iter().find(|q| q.is_current).map(|q| q.position) else {
                return;
            };
            let Some(next) = queue.iter().find(|q| q.position == cur_pos + 1) else {
                return;
            };
            let Some(next_file) = next.file_path.clone() else {
                return;
            };
            // Same decoded params as the current track? A NULL prop or a DSD
            // source means the negotiated output could differ — skip to avoid
            // warming a cache key the real play won't hit.
            let trepo = TrackRepo::with_backend(db.clone());
            let Some(t) = trepo.get(next.track_id).ok().flatten() else {
                return;
            };
            let is_dsd = t
                .format
                .as_deref()
                .map(|f| matches!(f.to_ascii_lowercase().as_str(), "dsd" | "dsf" | "dff"))
                .unwrap_or(false);
            let (Some(n_sr), Some(n_bd)) = (t.sample_rate, t.bit_depth) else {
                return;
            };
            if is_dsd
                || n_sr as u32 != cur_sr
                || n_bd as u16 != cur_bd
                || t.channels as u16 != cur_ch
            {
                return;
            }
            // Already warmed / cached?
            let Some(cp) =
                crate::transcode_cache::cache_path(&next_file, &out_ext, out_sr, out_bd, cur_ch)
            else {
                return;
            };
            if crate::transcode_cache::is_hit(&cp) {
                return;
            }
            // Transcode into a fresh temp file, then atomically rename it into the
            // cache (crash-safe: a partial write never lands under a cache name).
            let tmp = std::env::temp_dir()
                .join(format!(
                    "tune-transcode-{}.{}",
                    uuid::Uuid::new_v4(),
                    out_ext
                ))
                .to_string_lossy()
                .to_string();
            match transcode_source_to_file(
                next_file,
                out_sr,
                cur_ch,
                out_bd,
                target_fmt,
                None,
                None,
                None,
                tmp.clone(),
            )
            .await
            {
                Ok((size, _, _)) if size >= 1024 && std::fs::rename(&tmp, &cp).is_ok() => {
                    tokio::task::spawn_blocking(crate::transcode_cache::evict);
                    info!(zone_id, cache = %cp, "transcode_cache_warmed_next");
                }
                _ => {
                    let _ = std::fs::remove_file(&tmp);
                }
            }
        });
    }

    /// Pre-transcode the NEXT streaming track (Tidal/Qobuz HI-RES DASH) into the
    /// warm cache while the current one plays, so an album/playlist advance is an
    /// instant cache hit instead of another 6-23s blocking download+transcode
    /// (#1146). Opt-in via TUNE_DASH_WARM_CACHE (same flag as the check/store).
    ///
    /// The next track goes to the SAME zone/device as the current one, so the
    /// FLAC-vs-WAV decision is identical — we inherit `out_fmt` from the current
    /// play instead of re-probing the renderer from this detached task. The
    /// current track is located by `cur_source_id` in the streaming queue (robust
    /// against in-flight playback-state timing); we warm the item right after it.
    fn spawn_warm_next_streaming(
        &self,
        zone_id: i64,
        cur_source_id: String,
        out_fmt: &'static str,
    ) {
        if !dash_warm_cache_enabled() {
            return;
        }
        let db = self.db.clone();
        let services = self.services.clone();
        tokio::spawn(async move {
            // Find the current track in the streaming queue, then the next item.
            let sq = PlayQueueRepo::with_backend(db.clone())
                .get_streaming_queue(zone_id)
                .unwrap_or_default();
            let Some(cur_idx) = sq
                .iter()
                .position(|it| it["source_id"].as_str() == Some(cur_source_id.as_str()))
            else {
                return;
            };
            let Some(item) = sq.get(cur_idx + 1) else {
                return;
            };
            let source = item["source"].as_str().unwrap_or("").to_string();
            let source_id = item["source_id"].as_str().unwrap_or("").to_string();
            if source.is_empty() || source_id.is_empty() {
                return;
            }

            // Resolve the next track's stream. Only a DASH (file://) result is
            // worth caching — a direct proxy stream isn't transcoded.
            let stream_data = {
                let registry = services.lock().await;
                let Some(svc) = registry.get(&source) else {
                    return;
                };
                let svc = svc.read().await;
                match svc.get_track_url(&source_id, None).await {
                    Ok(d) => d,
                    Err(_) => return,
                }
            };
            let Some(dash_file) = stream_data.url.strip_prefix("file://").map(String::from) else {
                return;
            };
            if !std::path::Path::new(&dash_file).exists() {
                return;
            }

            let sr = stream_data.quality.sample_rate;
            let bd = stream_data.quality.bit_depth.max(16).min(24);
            let key_bd = if out_fmt == "wav" { 16 } else { bd };
            let cp = crate::transcode_cache::cache_path_streaming(
                &source, &source_id, out_fmt, sr, key_bd, 2,
            );
            if crate::transcode_cache::is_hit(&cp) {
                return; // already warmed
            }

            // Decode the fMP4 → encode (FLAC/WAV, WAV capped at 16-bit) → temp,
            // then atomically rename into the cache. Mirrors the play path.
            let is_wav = out_fmt == "wav";
            let tmp = std::env::temp_dir()
                .join(format!(
                    "tune-dash-warm-{}.{}",
                    uuid::Uuid::new_v4(),
                    out_fmt
                ))
                .to_string_lossy()
                .to_string();
            let dash_file_c = dash_file.clone();
            let tmp_c = tmp.clone();
            let result = tokio::task::spawn_blocking(move || {
                let decoded =
                    crate::audio::decode::decode_to_pcm(&dash_file_c, Some(sr), Some(2), 0.0, 0.0)?;
                let mut pcm_bytes = decoded.pcm_bytes();
                let mut actual_bd = decoded.bit_depth;
                if is_wav && actual_bd > 16 {
                    pcm_bytes = crate::audio::decode::convert_pcm_bytes(&pcm_bytes, actual_bd, 16);
                    actual_bd = 16;
                }
                let rt = tokio::runtime::Handle::try_current()
                    .map_err(|e| format!("no tokio runtime: {e}"))?;
                let encoded = rt.block_on(async {
                    let mut encoder = crate::audio::encoder::AudioEncoder::new(
                        out_fmt,
                        decoded.sample_rate,
                        actual_bd as u32,
                        decoded.channels,
                    );
                    encoder.start().await?;
                    encoder.write(&pcm_bytes).await?;
                    encoder.finish().await
                })?;
                std::fs::write(&tmp_c, &encoded).map_err(|e| format!("write temp: {e}"))?;
                Ok::<(u64, u16, u32), String>((
                    encoded.len() as u64,
                    actual_bd,
                    decoded.sample_rate,
                ))
            })
            .await;

            // The fMP4 has served its purpose (a warm cache-hit serves the FLAC),
            // so consume it like the play path does.
            let _ = std::fs::remove_file(&dash_file);

            // Same guard as the play-path store: only cache when the decoded
            // reality matches the key (`quality.*` from the service API can lie);
            // a mismatched entry would mis-advertise depth/rate in DIDL on every
            // later hit (Ruark-silence class, #1137).
            match result {
                Ok(Ok((size, actual_bd, actual_sr)))
                    if size >= 1024
                        && actual_bd == key_bd
                        && actual_sr == sr
                        && std::fs::rename(&tmp, &cp).is_ok() =>
                {
                    tokio::task::spawn_blocking(crate::transcode_cache::evict);
                    info!(zone_id, cache = %cp, next_source_id = %source_id, "streaming_dash_warm_next_stored");
                }
                _ => {
                    let _ = std::fs::remove_file(&tmp);
                }
            }
        });
    }

    /// True when the zone is in PURE (audiophile) mode: bypass ALL per-zone
    /// signal processing for a bit-perfect path — the equalizer, its
    /// room-correction gains (in `load_eq_processor`) and the room-correction
    /// convolver (in the local output). Bertrand: "PURE doit désactiver toutes
    /// les modifs".
    fn zone_audiophile(&self, zone_id: i64) -> bool {
        crate::audio::audiophile::zone_enabled(&self.db, zone_id)
    }

    /// True when the zone has an ENABLED equalizer profile with an audible
    /// effect (and is not in PURE mode). Cheap settings read used to decide
    /// routing BEFORE the sample rate is known — the actual EqProcessor is
    /// built later by the transcode path at the real rate.
    /// True when ReplayGain would change the samples for this zone's track.
    ///
    /// Same shape as [`Self::zone_has_active_eq`], and needed for the same
    /// reason: a network renderer served the file raw never runs any of our
    /// DSP, so without forcing the transcode the gain would be computed,
    /// logged, and silently thrown away.
    fn zone_replaygain_changes_audio(&self, zone_id: i64, track_id: Option<i64>) -> bool {
        if self.zone_audiophile(zone_id) {
            return false;
        }
        match track_id {
            Some(tid) => {
                (crate::audio::replaygain::playback_factor(&self.db, tid) - 1.0).abs() > 1e-6
            }
            None => false,
        }
    }

    /// Réappliquer l'égaliseur d'une zone à la sortie locale qui joue, sans
    /// attendre la piste suivante.
    ///
    /// `set_eq` n'était appelé qu'au démarrage d'une piste — « rebuilt at each
    /// play », par construction, puisqu'un `EqProcessor` se bâtit POUR un couple
    /// (taux, canaux). Bouger un curseur en cours de lecture persistait donc le
    /// profil, renvoyait 200, et ne changeait rien avant la piste suivante
    /// (#1725). Or c'est exactement le geste par lequel on règle un égaliseur :
    /// musique en cours, à l'oreille. Trois signalements « l'égaliseur ne
    /// fonctionne pas » (#1372, #1555, #1688) l'ont précédé.
    ///
    /// `LocalOutput::current_format` mémorise désormais le couple réellement vu
    /// par `apply_local_dsp`, ce qui permet de rebâtir aux bons coefficients.
    ///
    /// Renvoie `true` si une sortie locale vivante a reçu le nouveau contrat,
    /// y compris quand ce contrat retire l'égaliseur (`None`) en mode PURE ou
    /// avec un profil désactivé. `false` est réservé à l'absence de chemin
    /// local vivant : zone distante, sortie absente ou format encore inconnu.
    pub async fn refresh_zone_eq(&self, zone_id: i64) -> bool {
        #[cfg(not(feature = "local-audio"))]
        {
            let _ = zone_id;
            false
        }
        #[cfg(feature = "local-audio")]
        {
            let Some(device_id) = ZoneRepo::with_backend(self.db.clone())
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id)
            else {
                return false;
            };
            if !device_id.starts_with("local:") {
                return false;
            }
            let Some(output_arc) = ({ self.outputs.lock().await.get(&device_id) }) else {
                return false;
            };
            let output = output_arc.lock().await;
            let Some(local_output) = output
                .as_any()
                .downcast_ref::<crate::outputs::local::LocalOutput>()
            else {
                return false;
            };
            // Pas de flux en cours : la prochaine lecture rebâtira l'EQ de
            // toute façon, et bâtir pour un format inconnu donnerait des
            // coefficients faux.
            let Some((taux, canaux)) = local_output.current_format() else {
                return false;
            };
            let eq = self.load_eq_processor(zone_id, taux, canaux);
            let actif = eq.is_some();
            // `replace_eq_live` et non `set_eq` : la piste est en cours, donc
            // l'historique des biquads doit survivre au remplacement, sinon le
            // geste même qu'on vient de rendre possible — bouger un curseur en
            // écoutant — claque à chaque cran.
            local_output.replace_eq_live(eq);
            info!(
                zone_id,
                device_id = %device_id,
                sample_rate = taux,
                channels = canaux,
                actif,
                "zone_eq_refreshed_live"
            );
            true
        }
    }

    /// Rejouer la piste en cours d'une zone **à sa position courante**, en
    /// recréant le flux.
    ///
    /// Extrait tel quel de la branche `seek` (#1710, lot 1). Aucun changement
    /// de comportement : le seek passe désormais par ici, et les mêmes
    /// événements sont journalisés par l'appelant.
    ///
    /// Pourquoi cette manœuvre existe : une sortie locale ou OAAT consomme un
    /// flux HTTP **séquentiel** (mpsc / chunké). On ne peut pas y chercher une
    /// position par `Range` — sur un DSD servi en WAV chunké, une requête
    /// `Range` brute atterrit au milieu d'un bloc DSD et joue du BRUIT BLANC
    /// (Xavier). Il faut arrêter et rejouer depuis l'offset.
    ///
    /// Elle est extraite parce qu'elle ne sert pas qu'au seek : c'est la seule
    /// voie connue pour faire prendre effet un changement d'égaliseur sur un
    /// chemin transcodé ou navigateur, où le fichier est déjà écrit et déjà
    /// téléchargé (#1710). `raison` sert à distinguer les appelants dans les
    /// journaux — un redémarrage inattendu doit pouvoir être imputé.
    ///
    /// ⚠️ Coûteux et AUDIBLE : environ une seconde de silence. Tout appelant
    /// déclenché par un geste répétable — un curseur qu'on fait glisser — doit
    /// l'anti-rebondir lui-même, sinon il produit une coupure par cran.
    ///
    /// Rend `Err` si la zone n'a rien en lecture ou si la relecture échoue ;
    /// la position est repositionnée dans les deux cas, pour que le poller ne
    /// prenne pas un état `Stopped` pour une panne de lecture.
    pub async fn replay_zone_at_position(
        &self,
        zone_id: i64,
        position_ms: u64,
        raison: &str,
    ) -> Result<(), String> {
        let state = self.playback.get_state(zone_id).await;
        let Some(np) = state.now_playing.clone() else {
            return Err("rien en lecture sur cette zone".into());
        };
        self.playback.seek(zone_id, position_ms as i64).await;

        let output_device_id = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id);

        // Arrêter la sortie AVANT que play() n'en crée une autre : sans ça,
        // l'ancien fil ASIO/WASAPI peut encore tenir la connexion HTTP quand la
        // nouvelle session démarre — d'où un « request or response body error »
        // intermittent. Les 300 ms lui laissent relâcher le périphérique.
        if let Some(ref did) = output_device_id {
            if did.starts_with("local:") || did.starts_with("oaat:") {
                let arc = { self.outputs.lock().await.get(did.as_str()) };
                if let Some(output) = arc {
                    let _ = output.lock().await.stop().await;
                }
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        }

        let req = PlayRequest {
            zone_id,
            output_device_id,
            track_id: np.track_id,
            source: Some(np.source.clone()),
            source_id: np.source_id.clone(),
            title: Some(np.title.clone()),
            artist_name: np.artist_name.clone(),
            album_title: np.album_title.clone(),
            cover_url: np.cover_path.clone(),
            duration_ms: Some(np.duration_ms),
            seek_ms: Some(position_ms),
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: None,
            disc_number: None,
        };

        let resultat = self.play_without_history(req).await;
        // Repositionner DANS LES DEUX CAS : à la réussite pour que la grâce du
        // poller parte d'après la relecture, à l'échec pour qu'il ne prenne pas
        // l'état `Stopped` pour une panne.
        self.playback.seek(zone_id, position_ms as i64).await;
        match resultat {
            Ok(_) => {
                info!(zone_id, position_ms, raison, "zone_replayed_at_position");
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// Faire prendre effet un changement d'égaliseur, PAR TOUS LES CHEMINS.
    ///
    /// Point d'entrée unique des routes qui écrivent `zone_{id}_eq_profile`.
    /// Il existe parce que la règle « local d'abord, redémarrage sinon » ne
    /// doit vivre qu'à un seul endroit : c'est sa duplication implicite entre
    /// quatre routes qui avait produit #1725.
    ///
    /// - **sortie locale** : [`Self::refresh_zone_eq`] remplace l'`EqProcessor`
    ///   derrière son mutex — immédiat, inaudible, aucune coupure ;
    /// - **tout le reste** (DLNA, navigateur) : le fichier transcodé est déjà
    ///   écrit et téléchargé, rien à remplacer. [`Self::schedule_eq_replay`]
    ///   programme un redémarrage anti-rebondi (#1710).
    ///
    /// Rend `true` quand le réglage a atteint le son **immédiatement** — donc
    /// uniquement sur le chemin local. Un redémarrage programmé rend `false` :
    /// il n'a pas encore eu lieu, et une demande plus récente peut encore
    /// l'annuler. L'interface continue donc d'annoncer « prendra effet à la
    /// piste suivante », ce qui est vrai jusqu'à ce que le redémarrage advienne
    /// — mieux vaut cela qu'une promesse que l'anti-rebond peut retirer.
    ///
    /// Toute mutation annonce ensuite `zone.updated`. Le contrat de cet
    /// événement est volontairement minimal : les clients rechargent la zone
    /// et reconstruisent ainsi `signal_path` depuis le profil EQ qui vient
    /// d'être persisté, au lieu de conserver l'instantané de la lecture (#1985).
    pub async fn apply_eq_change(self: &std::sync::Arc<Self>, zone_id: i64) -> bool {
        let applique_a_chaud = self.refresh_zone_eq(zone_id).await;
        if !applique_a_chaud {
            // Pas de chemin local vivant. Reste le redémarrage — mais uniquement si
            // quelque chose joue : sinon la prochaine lecture rebâtira l'EQ toute
            // seule, et redémarrer un flux inexistant n'a aucun sens.
            let joue = self.playback.get_state(zone_id).await.now_playing.is_some();
            if joue {
                self.schedule_eq_replay(zone_id);
            }
        }

        if let Some(ref bus) = self.event_bus {
            bus.emit("zone.updated", serde_json::json!({ "zone_id": zone_id }));
        }
        applique_a_chaud
    }

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

    /// Faire prendre effet un changement d'égaliseur sur un chemin **non
    /// local**, en redémarrant le flux à la position courante (#1710, lot 2).
    ///
    /// Sur une sortie locale, `refresh_zone_eq` suffit : l'`EqProcessor` vit
    /// derrière un mutex relu à chaque paquet. Ailleurs — DLNA, navigateur — le
    /// fichier transcodé est déjà écrit, déjà en cours de téléchargement,
    /// souvent déjà en cache. Rien à remplacer : il faut re-résoudre.
    ///
    /// **Anti-rebondi et planchéié**, parce que la manœuvre est audible. Sans
    /// ça, un curseur de 31 bandes produirait 31 coupures d'une seconde — un
    /// remède pire que le mal. Voir [`Self::replay_zone_at_position`].
    ///
    /// Rend immédiatement : le redémarrage est différé dans une tâche. La
    /// valeur dit si un redémarrage a été **programmé**, pas s'il a eu lieu —
    /// une demande plus récente peut encore l'annuler.
    pub fn schedule_eq_replay(self: &std::sync::Arc<Self>, zone_id: i64) -> bool {
        let generation = {
            let mut gens = self.eq_replay_gen.lock().unwrap();
            let g = gens.entry(zone_id).or_insert(0);
            *g += 1;
            *g
        };
        let moi = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(
                Self::EQ_REPLAY_DEBOUNCE_MS,
            ))
            .await;
            // Une demande plus récente est arrivée pendant l'attente : c'est
            // elle qui redémarrera, pas nous.
            {
                let gens = moi.eq_replay_gen.lock().unwrap();
                if gens.get(&zone_id).copied() != Some(generation) {
                    return;
                }
            }
            // Plancher : trop tôt après le précédent, on renonce plutôt que de
            // hacher. Le réglage prendra effet à la piste suivante, ce que le
            // client sait déjà dire (`applied_live: false`).
            {
                let derniers = moi.eq_replay_last.lock().unwrap();
                if let Some(t) = derniers.get(&zone_id) {
                    if t.elapsed().as_millis() < Self::EQ_REPLAY_FLOOR_MS as u128 {
                        info!(zone_id, "eq_replay_skipped_floor");
                        return;
                    }
                }
            }
            let position_ms = moi.playback.get_state(zone_id).await.position_ms.max(0) as u64;
            match moi
                .replay_zone_at_position(zone_id, position_ms, "eq_change")
                .await
            {
                Ok(()) => {
                    moi.eq_replay_last
                        .lock()
                        .unwrap()
                        .insert(zone_id, std::time::Instant::now());
                    info!(zone_id, position_ms, "eq_replay_done");
                }
                Err(e) => warn!(zone_id, error = %e, "eq_replay_failed"),
            }
        });
        true
    }

    /// Réappliquer le crossfeed d'une zone à la sortie locale qui joue, sans
    /// attendre la piste suivante.
    ///
    /// Jumeau de [`Self::refresh_zone_eq`], pour le même défaut : `set_crossfeed`
    /// n'était appelé qu'au démarrage d'une piste (`orchestrator.rs`, chemin de
    /// lecture), si bien qu'activer le crossfeed ou déplacer `amount` /
    /// `delay_ms` en écoutant persistait la configuration, renvoyait un succès,
    /// et ne changeait rien avant la piste suivante (#1786).
    ///
    /// Renvoie `true` si un crossfeed a été poussé vers une sortie vivante.
    /// `false` couvre tout le reste — zone sans sortie locale, rien en lecture,
    /// crossfeed désactivé, mode PURE (où `load_crossfeed_processor` rend `None`,
    /// donc la promesse bit-perfect tient sans garde supplémentaire).
    pub async fn refresh_zone_crossfeed(&self, zone_id: i64) -> bool {
        #[cfg(not(feature = "local-audio"))]
        {
            let _ = zone_id;
            false
        }
        #[cfg(feature = "local-audio")]
        {
            let Some(device_id) = ZoneRepo::with_backend(self.db.clone())
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id)
            else {
                return false;
            };
            if !device_id.starts_with("local:") {
                return false;
            }
            let Some(output_arc) = ({ self.outputs.lock().await.get(&device_id) }) else {
                return false;
            };
            let output = output_arc.lock().await;
            let Some(local_output) = output
                .as_any()
                .downcast_ref::<crate::outputs::local::LocalOutput>()
            else {
                return false;
            };
            // Pas de flux en cours : la prochaine lecture rebâtira le crossfeed
            // de toute façon, et le bâtir pour un taux inconnu donnerait une
            // ligne à retard de la mauvaise longueur.
            let Some((taux, _canaux)) = local_output.current_format() else {
                return false;
            };
            let cf = self.load_crossfeed_processor(zone_id, taux);
            let actif = cf.is_some();
            // `replace_crossfeed_live` et non `set_crossfeed` : la piste est en
            // cours, donc les lignes à retard doivent survivre au remplacement.
            local_output.replace_crossfeed_live(cf);
            info!(
                zone_id,
                device_id = %device_id,
                sample_rate = taux,
                actif,
                "zone_crossfeed_refreshed_live"
            );
            actif
        }
    }

    /// Réappliquer TOUT ce que le mode PURE gouverne à la sortie locale qui
    /// joue, sans attendre la piste suivante.
    ///
    /// Le bloc PURE du chemin de lecture — `set_pure_bypass`, ReplayGain,
    /// crossfeed, égaliseur — n'était exécuté qu'au démarrage d'une piste, et
    /// son commentaire l'assumait : « so a zone toggled in/out of PURE takes
    /// effect on the **next track** ». Or basculer PURE est un geste qu'on fait
    /// en écoutant, exactement comme bouger un curseur d'égaliseur (#1725) ou
    /// de crossfeed (#1786). Entre le clic et la piste suivante, l'interrupteur
    /// est vert, le badge PURE est allumé, le panneau annonce un chemin
    /// intouché — et l'`EqProcessor` installé dans la sortie continue de
    /// filtrer chaque échantillon (Jean Valjean, #1986 : « il me semble que
    /// l'égaliseur est toujours actif », +8 dB de Bass Boost en PURE).
    ///
    /// Ici l'écart est plus grave que pour l'EQ ou le crossfeed : ces deux-là
    /// ne promettaient qu'un réglage tardif. PURE promet, lui, que rien ne
    /// touche le signal — et l'affichage le REPETE, puisque le chemin du signal
    /// lit la même clé (`zones.rs`, `zone_eq_alters_signal`). Le panneau disait
    /// donc vrai sur l'intention et faux sur le son.
    ///
    /// La sortie de PURE est tout aussi concernée, et c'est le point 3 du même
    /// signalement (« je devrais revenir au réglage précédent ») : les
    /// processeurs restent alors à `None` et l'égaliseur choisi par
    /// l'utilisateur ne revient qu'à la piste suivante.
    ///
    /// Les quatre réglages sont repoussés ensemble, sous le même verrou, parce
    /// qu'ils décrivent un seul état : en repousser trois laisserait une
    /// combinaison que le chemin de lecture ne produit jamais.
    ///
    /// Rend `true` quand une sortie locale VIVANTE a reçu le nouvel état —
    /// sans rien dire de ce qu'il contient. C'est la différence avec
    /// [`Self::refresh_zone_eq`], qui rend `true` si un égaliseur est actif :
    /// entrer en PURE éteint tout, donc un `false` de ce genre signifierait
    /// « rien reçu » alors que tout vient d'être appliqué.
    pub async fn refresh_zone_pure_dsp(&self, zone_id: i64) -> bool {
        #[cfg(not(feature = "local-audio"))]
        {
            let _ = zone_id;
            false
        }
        #[cfg(feature = "local-audio")]
        {
            let Some(device_id) = ZoneRepo::with_backend(self.db.clone())
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id)
            else {
                return false;
            };
            if !device_id.starts_with("local:") {
                return false;
            }
            let Some(output_arc) = ({ self.outputs.lock().await.get(&device_id) }) else {
                return false;
            };
            // Le track_id est lu AVANT de prendre le verrou de la sortie : le
            // ReplayGain en dépend, et `get_state` prend ses propres verrous.
            let track_id = self
                .playback
                .get_state(zone_id)
                .await
                .now_playing
                .and_then(|np| np.track_id);
            let output = output_arc.lock().await;
            let Some(local_output) = output
                .as_any()
                .downcast_ref::<crate::outputs::local::LocalOutput>()
            else {
                return false;
            };
            // Rien en cours : la prochaine lecture appliquera l'état complet de
            // toute façon, et bâtir des filtres pour un format inconnu donnerait
            // des coefficients faux. Même garde que les deux jumelles.
            let Some((taux, canaux)) = local_output.current_format() else {
                return false;
            };

            let pure = self.zone_audiophile(zone_id);
            local_output.set_pure_bypass(pure);
            // Mêmes expressions que le bloc du chemin de lecture, à dessein :
            // toutes trois rendent `None`/1.0 en PURE, donc l'état repoussé est
            // celui qu'une lecture démarrée maintenant produirait.
            let rg = match (pure, track_id) {
                (false, Some(tid)) => crate::audio::replaygain::playback_factor(&self.db, tid),
                _ => 1.0,
            };
            local_output.set_replaygain_factor(rg);
            // `replace_*_live` et non `set_*` : la piste est en cours, donc
            // l'historique des biquads et les lignes à retard doivent survivre
            // au remplacement — sinon la bascule claque.
            local_output.replace_crossfeed_live(self.load_crossfeed_processor(zone_id, taux));
            local_output.replace_eq_live(self.load_eq_processor(zone_id, taux, canaux));
            // Le repli mono est lui aussi gouverné par PURE (#2362) : basculer
            // PURE doit donc le désarmer ou le réarmer dans le même geste, sans
            // quoi une zone qui sort de PURE resterait stéréo jusqu'à la piste
            // suivante alors que le panneau annonce déjà « Mono ».
            local_output.set_mono_downmix(self.zone_mono_downmix(zone_id));
            info!(
                zone_id,
                device_id = %device_id,
                pure,
                sample_rate = taux,
                channels = canaux,
                replaygain = rg,
                "zone_pure_dsp_refreshed_live"
            );
            true
        }
    }

    /// Faire prendre effet une bascule du mode PURE, PAR TOUS LES CHEMINS.
    ///
    /// Jumeau de [`Self::apply_eq_change`], et pour la même raison : la règle
    /// « local d'abord, redémarrage sinon » ne doit vivre qu'à un endroit.
    ///
    /// - **sortie locale** : [`Self::refresh_zone_pure_dsp`] repousse l'état
    ///   derrière les mutex de la sortie — immédiat, sans coupure ;
    /// - **tout le reste** (DLNA, navigateur) : les traitements ont été gravés
    ///   dans le fichier transcodé, déjà écrit et déjà téléchargé. Rien à
    ///   remplacer ; seul un redémarrage du flux le re-rend, et c'est
    ///   exactement ce que [`Self::schedule_eq_replay`] sait faire — même
    ///   anti-rebond, même plancher, parce que c'est le même coût (environ une
    ///   seconde de silence) et le même geste répétable.
    ///
    /// Rend `true` quand la bascule a atteint le son **immédiatement**. Un
    /// redémarrage programmé rend `false` : il n'a pas encore eu lieu.
    pub async fn apply_audiophile_change(self: &std::sync::Arc<Self>, zone_id: i64) -> bool {
        if self.refresh_zone_pure_dsp(zone_id).await {
            return true;
        }
        // Pas de chemin local vivant. Le redémarrage n'a de sens que si quelque
        // chose joue : sinon la prochaine lecture appliquera l'état toute seule.
        let joue = self.playback.get_state(zone_id).await.now_playing.is_some();
        if joue {
            self.schedule_eq_replay(zone_id);
        }
        false
    }

    fn zone_has_active_eq(&self, zone_id: i64) -> bool {
        // 44100/2 is only a probe: EqProcessor::is_enabled() depends on the
        // gains, not the rate.
        self.load_eq_processor(zone_id, 44100, 2).is_some()
    }

    fn load_eq_processor(
        &self,
        zone_id: i64,
        sample_rate: u32,
        channels: u16,
    ) -> Option<crate::audio::eq::EqProcessor> {
        let profile = self.load_eq_profile(zone_id)?;
        let eq = crate::audio::eq::EqProcessor::new(&profile, sample_rate, channels);
        if eq.is_enabled() { Some(eq) } else { None }
    }

    /// Profil EQ réellement actif pour une zone, sans encore le lier à un
    /// format PCM. Le décodeur radio ne connaît le taux et le nombre de canaux
    /// qu'après sa sonde ; lui transmettre le profil permet de construire les
    /// coefficients exacts à cet instant au lieu de supposer 44,1 kHz (#2063).
    fn load_eq_profile(&self, zone_id: i64) -> Option<crate::audio::eq::EqProfile> {
        // PURE mode: never build an EqProcessor so the PCM reaches the output
        // untouched.
        if self.zone_audiophile(zone_id) {
            return None;
        }
        let settings = crate::db::settings_repo::SettingsRepo::with_backend(self.db.clone());
        let key = format!("zone_{zone_id}_eq_profile");
        let profile: crate::audio::eq::EqProfile = settings
            .get(&key)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())?;
        if !profile.enabled {
            return None;
        }
        Some(profile)
    }

    /// True when the zone has an uploaded room-correction IR and is not in PURE
    /// mode. Cheap settings read (like `zone_has_active_eq`) used to force the
    /// transcode path so the FIR reaches network renderers, not just local.
    fn zone_has_active_ir(&self, zone_id: i64) -> bool {
        if self.zone_audiophile(zone_id) {
            return false;
        }
        crate::db::settings_repo::SettingsRepo::with_backend(self.db.clone())
            .get(&format!("ir_path_{zone_id}"))
            .ok()
            .flatten()
            .map(|p| !p.is_empty() && std::path::Path::new(&p).exists())
            .unwrap_or(false)
    }

    /// Build the room-correction FIR convolver for a zone's TRANSCODED stream,
    /// or `None`. Symmetric to `load_eq_processor`: PURE (audiophile) mode →
    /// `None`; otherwise load the uploaded IR (`ir_path_{zone}`) for the
    /// stream's sample rate + channel count. Applied in `transcode_source_to_file`
    /// after the EQ, so it colours the bytes served to a network renderer.
    fn load_convolver(
        &self,
        zone_id: i64,
        sample_rate: u32,
        channels: u16,
    ) -> Option<crate::audio::convolver::Convolver> {
        if self.zone_audiophile(zone_id) {
            return None;
        }
        let path = crate::db::settings_repo::SettingsRepo::with_backend(self.db.clone())
            .get(&format!("ir_path_{zone_id}"))
            .ok()
            .flatten()
            .filter(|p| !p.is_empty())?;
        match crate::audio::convolver::Convolver::from_wav_for(
            &path,
            1024,
            sample_rate,
            channels as usize,
        ) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(zone_id, path, error = %e, "room_correction_ir_load_failed");
                None
            }
        }
    }

    /// Build the headphone crossfeed processor for a zone's LOCAL output, or
    /// `None` when it should not run. Symmetric to `load_eq_processor`:
    ///
    ///   - PURE (audiophile) mode → `None` (bit-perfect path, no coloration).
    ///   - crossfeed `enabled == false` (the default) → `None`.
    ///   - `amount == 0` → `None` (would be a pure identity anyway).
    ///
    /// Config lives in the settings key `zone_{id}_crossfeed` as JSON
    /// `{ "enabled": bool, "amount": f32, "delay_ms": f32 }`. Values are clamped
    /// defensively (amount 0..0.5, delay_ms 0..5) mirroring the route validation.
    fn load_crossfeed_processor(
        &self,
        zone_id: i64,
        sample_rate: u32,
    ) -> Option<crate::audio::crossfeed::CrossfeedProcessor> {
        // PURE mode: no crossfeed, keep the signal path bit-perfect.
        if self.zone_audiophile(zone_id) {
            return None;
        }
        let settings = crate::db::settings_repo::SettingsRepo::with_backend(self.db.clone());
        let cfg: serde_json::Value = settings
            .get(&format!("zone_{zone_id}_crossfeed"))
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())?;
        if !cfg
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return None;
        }
        let amount = cfg.get("amount").and_then(|v| v.as_f64()).unwrap_or(0.30) as f32;
        let amount = amount.clamp(0.0, 0.5);
        if amount == 0.0 {
            return None;
        }
        let delay_ms = cfg.get("delay_ms").and_then(|v| v.as_f64()).unwrap_or(0.30) as f32;
        let delay_ms = delay_ms.clamp(0.0, 5.0);
        Some(crate::audio::crossfeed::CrossfeedProcessor::new(
            sample_rate,
            amount,
            delay_ms,
        ))
    }

    /// La zone demande-t-elle le repli mono sur sa sortie LOCALE ? (#2362)
    ///
    /// Symétrique de [`Self::load_crossfeed_processor`] :
    ///
    ///   - mode PURE (audiophile) → `false` (chemin bit-perfect, intouché) ;
    ///   - réglage absent, vide, ou différent de `"true"` → `false` (défaut).
    ///
    /// Le réglage vit dans la clé `zone_{id}_mono_downmix`, écrite par
    /// `PATCH /zones/{id}` — même forme que `zone_{id}_upnp_renderer` : la clé
    /// est SUPPRIMÉE quand l'utilisateur désactive, jamais mise à `"false"`.
    ///
    /// Public : `tune-server` le relit pour composer le chemin du signal, afin
    /// que le panneau et le son répondent à la MÊME question — c'est la leçon
    /// de #1548/#1559 (EQ oublié du verdict) et de #1627 (ReplayGain).
    pub fn zone_mono_downmix(&self, zone_id: i64) -> bool {
        Self::zone_mono_downmix_with(&self.db, zone_id)
    }

    /// Même règle, lisible sans orchestrateur — c'est par là que le serveur
    /// compose le chemin du signal.
    pub fn zone_mono_downmix_with(
        db: &std::sync::Arc<dyn crate::db::backend::DbBackend>,
        zone_id: i64,
    ) -> bool {
        // PURE : le PCM atteint la sortie intact, aucun repli n'est appliqué.
        if crate::audio::audiophile::zone_enabled(db, zone_id) {
            return false;
        }
        crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
            .get(&format!("zone_{zone_id}_mono_downmix"))
            .ok()
            .flatten()
            .as_deref()
            == Some("true")
    }

    /// Réappliquer le repli mono d'une zone à la sortie locale qui joue, sans
    /// attendre la piste suivante.
    ///
    /// Jumeau de [`Self::refresh_zone_crossfeed`], pour le même défaut : sans
    /// lui, cocher la case en écoutant persisterait le réglage, renverrait un
    /// succès, et ne changerait rien avant la piste suivante (#1725, #1786).
    /// Or c'est exactement ainsi qu'on vérifie ce réglage-ci : une seule
    /// enceinte, on coche, et on doit entendre revenir ce qui était panné à
    /// droite.
    ///
    /// Contrairement au crossfeed, il n'y a **pas** de garde sur
    /// `current_format()` : le repli n'a aucun filtre à bâtir pour un taux
    /// donné, donc rien à faire dépendre d'un flux en cours. Armer le drapeau
    /// sur une sortie silencieuse est correct et évite de perdre le réglage.
    ///
    /// Renvoie `true` si le drapeau a été poussé vers une sortie locale vivante.
    pub async fn refresh_zone_mono_downmix(&self, zone_id: i64) -> bool {
        #[cfg(not(feature = "local-audio"))]
        {
            let _ = zone_id;
            false
        }
        #[cfg(feature = "local-audio")]
        {
            let Some(device_id) = ZoneRepo::with_backend(self.db.clone())
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id)
            else {
                return false;
            };
            if !device_id.starts_with("local:") {
                return false;
            }
            let Some(output_arc) = ({ self.outputs.lock().await.get(&device_id) }) else {
                return false;
            };
            let output = output_arc.lock().await;
            let Some(local_output) = output
                .as_any()
                .downcast_ref::<crate::outputs::local::LocalOutput>()
            else {
                return false;
            };
            let mono = self.zone_mono_downmix(zone_id);
            local_output.set_mono_downmix(mono);
            info!(
                zone_id,
                device_id = %device_id,
                mono,
                "zone_mono_downmix_refreshed_live"
            );
            true
        }
    }

    fn record_listen(
        &self,
        title: &str,
        artist: Option<&str>,
        album: Option<&str>,
        source: &str,
        source_id: Option<&str>,
        album_id: Option<i64>,
        duration_ms: i64,
        zone_id: i64,
        cover_url: Option<&str>,
        session_profile_id: Option<i64>,
        context_type: Option<&str>,
        context_id: Option<&str>,
    ) {
        // The owning profile is resolved by the caller from the zone's session
        // (set by the play handler from X-Profile-Id, inherited by autoplay /
        // gapless advances). `None` → tag NULL rather than guess an owner: a
        // wrong attribution pollutes a person's taste profile once per-profile
        // recommendations land, an absence doesn't.
        let repo = HistoryRepo::with_backend(self.db.clone());
        repo.record(&ListenRecord {
            id: None,
            track_id: None,
            title: title.into(),
            artist_name: artist.map(Into::into),
            album_title: album.map(Into::into),
            source: source.into(),
            source_id: source_id.map(Into::into),
            album_id,
            duration_ms,
            listened_at: None,
            zone_id: Some(zone_id),
            cover_url: cover_url.map(Into::into),
            profile_id: session_profile_id,
            // Ce que l'auditeur a demande. Ecrit tel qu'il l'a dit : rien
            // n'est deduit ici de ce qui a fini par jouer. Le ticket #2441
            // etablit que cette information n'etait ecrite NULLE PART — la
            // section « Continuer l'ecoute » ne pouvait donc que repartir de
            // la table `albums`.
            context_type: context_type.map(Into::into),
            context_id: context_id.map(Into::into),
        })
        .ok();

        // NOTE: scrobbling is intentionally NOT dispatched here. It used to fire
        // at play-start, which (a) scrobbled a track the instant it began — so
        // skipping after a few seconds still scrobbled it, ignoring Last.fm's
        // 50%/4-min rule — and (b) was gated by `record_history`, which the
        // gapless/prefetch advance paths bypass (`play_without_history`), so
        // every other track on an album was silently dropped (Bilou, #1113). The
        // poller now dispatches the scrobble once the track has actually been
        // listened past the threshold (see `dispatch_scrobble`).
    }

    /// L'onglet a-t-il commencé à tirer le flux de cette zone ? Si oui, et
    /// seulement alors, l'annonce « en écoute » mise en attente au démarrage
    /// part — une fois (#1998).
    ///
    /// Appelée à chaque tick par le poller pour une zone SANS périphérique de
    /// sortie. C'est le poller qui a l'horloge ; la décision, elle, reste ici,
    /// avec les données du démarrage — `record_history` en particulier, qu'un
    /// observateur extérieur ne peut pas reconstituer.
    ///
    /// La preuve est celle dont `output_reach` se sert déjà pour dire
    /// « browser_unattended » (`tune-server/src/routes/zones.rs`) : des octets
    /// réellement partis sur la session de flux. Aucune détection nouvelle.
    ///
    /// Le délai vaut au plus un tick de poller (~1 s) après le premier octet
    /// tiré : la règle de durée minimale de Last.fm porte sur le scrobble
    /// définitif (50 % / 4 min, côté poller), pas sur « en écoute », et une
    /// seconde ne coûte aucune écoute légitime.
    ///
    /// Rend `true` quand l'annonce vient de partir.
    pub async fn confirmer_lecture_navigateur(&self, zone_id: i64, stream_id: &str) -> bool {
        // Rien en attente pour CE flux → rien à faire, et surtout pas
        // d'interrogation du streamer à chaque tick de chaque zone.
        {
            let Ok(en_attente) = self.annonces_navigateur.lock() else {
                return false;
            };
            if en_attente.get(&zone_id).map(|a| a.stream_id.as_str()) != Some(stream_id) {
                return false;
            }
        }

        let tire = self
            .streamer
            .stream_bytes_sent(stream_id)
            .await
            .is_some_and(|n| n > 0);
        if !tire {
            return false;
        }

        // Retirer AVANT d'annoncer : le verrou « une seule fois » est le retrait
        // lui-même. Re-vérifier le flux protège d'une lecture qui aurait
        // remplacé l'entrée pendant l'attente ci-dessus.
        let attente = {
            let Ok(mut en_attente) = self.annonces_navigateur.lock() else {
                return false;
            };
            match en_attente.get(&zone_id) {
                Some(a) if a.stream_id == stream_id => en_attente.remove(&zone_id),
                _ => None,
            }
        };
        let Some(attente) = attente else {
            return false;
        };

        info!(
            zone_id,
            title = %attente.title,
            source = %attente.source,
            stream_id = %stream_id,
            "browser_playback_confirmed_announcing"
        );

        self.dispatch_now_playing(
            &attente.title,
            attente.artist.as_deref(),
            attente.album.as_deref(),
        );

        // Même exclusion que le chemin nominal : la radio n'entre pas dans
        // l'historique local (son titre au démarrage est un instantané figé),
        // et une re-création de flux pour une piste déjà en cours
        // (`play_without_history`) ne doit pas doublonner la ligne.
        if attente.record_history && attente.source != "radio" {
            let etat = self.playback.get_state(zone_id).await;
            let album_id = attente.track_id.and_then(|tid| {
                TrackRepo::with_backend(self.db.clone())
                    .get(tid)
                    .ok()
                    .flatten()
                    .and_then(|t| t.album_id)
            });
            self.record_listen(
                &attente.title,
                attente.artist.as_deref(),
                attente.album.as_deref(),
                &attente.source,
                attente.source_id.as_deref(),
                album_id,
                attente.duration_ms,
                zone_id,
                attente.cover_path.as_deref(),
                etat.session_profile_id,
                etat.session_context_type.as_deref(),
                etat.session_context_id.as_deref(),
            );
        }

        true
    }

    /// Oublie l'annonce en attente d'une zone navigateur : la lecture s'arrête
    /// sans que l'onglet ait rien tiré, il n'y a donc rien à annoncer.
    fn oublier_annonce_navigateur(&self, zone_id: i64) {
        if let Ok(mut en_attente) = self.annonces_navigateur.lock() {
            en_attente.remove(&zone_id);
        }
    }

    /// Dispatch scrobbles to all configured services, respecting tier limits.
    /// Free = 1 service max, Premium = all simultaneously.
    ///
    /// Called by the poller once the current track has been played past the
    /// Last.fm threshold (50% or 4 min), so a scrobble reflects a real listen
    /// rather than a mere play-start (#1113).
    pub fn dispatch_scrobble(&self, title: &str, artist: Option<&str>, album: Option<&str>) {
        let settings = SettingsRepo::with_backend(self.db.clone());

        let lastfm_ready = self.lastfm_keys().is_some();
        let lb_ready = self.listenbrainz_token().is_some();

        // Check tier: if both services are active and user is Free, only
        // dispatch to the first one (Last.fm has priority as legacy default).
        let is_premium = {
            let tier_str = settings.get("license_tier").ok().flatten();
            matches!(tier_str.as_deref(), Some("premium"))
        };

        if lastfm_ready {
            self.lastfm_scrobble(title, artist, album);
        }

        if lb_ready {
            if !lastfm_ready || is_premium {
                // Either Last.fm is not active (so LB is the sole service)
                // or user is Premium (simultaneous allowed).
                self.listenbrainz_scrobble(title, artist, album);
            } else {
                debug!(
                    "listenbrainz_scrobble_skipped_free_tier: lastfm active, upgrade to Premium for multi-service"
                );
            }
        }
    }

    /// Dispatch now-playing updates to all configured services, respecting tier limits.
    fn dispatch_now_playing(&self, title: &str, artist: Option<&str>, album: Option<&str>) {
        let settings = SettingsRepo::with_backend(self.db.clone());

        let lastfm_ready = self.lastfm_keys().is_some();
        let lb_ready = self.listenbrainz_token().is_some();

        let is_premium = {
            let tier_str = settings.get("license_tier").ok().flatten();
            matches!(tier_str.as_deref(), Some("premium"))
        };

        if lastfm_ready {
            self.lastfm_now_playing(title, artist, album);
        }

        if lb_ready {
            if !lastfm_ready || is_premium {
                self.listenbrainz_now_playing(title, artist, album);
            }
        }
    }

    fn lastfm_keys(&self) -> Option<(String, String, String)> {
        let settings = SettingsRepo::with_backend(self.db.clone());
        let api_key = settings.get("lastfm_api_key").ok().flatten()?;
        let api_secret = settings.get("lastfm_api_secret").ok().flatten()?;
        let session_key = settings.get("lastfm_session_key").ok().flatten()?;
        if api_key.is_empty() || api_secret.is_empty() || session_key.is_empty() {
            return None;
        }
        Some((api_key, api_secret, session_key))
    }

    fn lastfm_scrobble(&self, title: &str, artist: Option<&str>, album: Option<&str>) {
        let artist = match artist {
            Some(a) if !a.is_empty() => a.to_string(),
            _ => return,
        };
        let Some((api_key, api_secret, session_key)) = self.lastfm_keys() else {
            return;
        };
        let title = title.to_string();
        // Send the album too: Last.fm/Pano apps rely on it to fetch the cover
        // (the web site does a looser track-level match), so scrobbles without
        // an album showed no artwork in the apps (#1113).
        let album = album.filter(|a| !a.is_empty()).map(|a| a.to_string());
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        tokio::spawn(async move {
            if let Err(e) = crate::scrobble::scrobble_full(
                &api_key,
                &api_secret,
                &session_key,
                &artist,
                &title,
                album.as_deref(),
                None,
                timestamp,
            )
            .await
            {
                warn!("lastfm_scrobble_error: {e}");
            }
        });
    }

    fn lastfm_now_playing(&self, title: &str, artist: Option<&str>, album: Option<&str>) {
        let artist = match artist {
            Some(a) if !a.is_empty() => a.to_string(),
            _ => return,
        };
        let Some((api_key, api_secret, session_key)) = self.lastfm_keys() else {
            return;
        };
        let title = title.to_string();
        let album = album.filter(|a| !a.is_empty()).map(|a| a.to_string());
        tokio::spawn(async move {
            if let Err(e) = crate::scrobble::update_now_playing_full(
                &api_key,
                &api_secret,
                &session_key,
                &artist,
                &title,
                album.as_deref(),
                None,
            )
            .await
            {
                warn!("lastfm_now_playing_error: {e}");
            }
        });
    }

    fn listenbrainz_token(&self) -> Option<String> {
        let settings = SettingsRepo::with_backend(self.db.clone());
        settings
            .get("listenbrainz_token")
            .ok()
            .flatten()
            .filter(|t| !t.is_empty())
    }

    fn listenbrainz_scrobble(&self, title: &str, artist: Option<&str>, album: Option<&str>) {
        let artist = match artist {
            Some(a) if !a.is_empty() => a.to_string(),
            _ => return,
        };
        let Some(token) = self.listenbrainz_token() else {
            return;
        };
        let title = title.to_string();
        let album = album.map(String::from);
        tokio::spawn(async move {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            let payload = serde_json::json!({
                "listen_type": "single",
                "payload": [{
                    "listened_at": timestamp,
                    "track_metadata": {
                        "artist_name": artist,
                        "track_name": title,
                        "release_name": album,
                    }
                }]
            });

            let client = crate::http::client::shared();
            if let Err(e) = client
                .post("https://api.listenbrainz.org/1/submit-listens")
                .header("Authorization", format!("Token {token}"))
                .header("Content-Type", "application/json")
                .json(&payload)
                .send()
                .await
            {
                warn!("listenbrainz_scrobble_error: {e}");
            }
        });
    }

    fn listenbrainz_now_playing(&self, title: &str, artist: Option<&str>, album: Option<&str>) {
        let artist = match artist {
            Some(a) if !a.is_empty() => a.to_string(),
            _ => return,
        };
        let Some(token) = self.listenbrainz_token() else {
            return;
        };
        let title = title.to_string();
        let album = album.map(String::from);
        tokio::spawn(async move {
            let payload = serde_json::json!({
                "listen_type": "playing_now",
                "payload": [{
                    "track_metadata": {
                        "artist_name": artist,
                        "track_name": title,
                        "release_name": album,
                    }
                }]
            });

            let client = crate::http::client::shared();
            if let Err(e) = client
                .post("https://api.listenbrainz.org/1/submit-listens")
                .header("Authorization", format!("Token {token}"))
                .header("Content-Type", "application/json")
                .json(&payload)
                .send()
                .await
            {
                warn!("listenbrainz_now_playing_error: {e}");
            }
        });
    }

    pub async fn pause(&self, zone_id: i64, device_id: Option<&str>) -> OutputCommandResult<()> {
        if let Some(did) = device_id {
            // Le backend confirme la commande AVANT que la copie mémoire et
            // la base annoncent Paused.
            let output = { self.outputs.lock().await.get(did) }.ok_or_else(|| {
                OutputCommandError::failed(
                    OutputCommand::Pause,
                    format!("output {did} is not registered"),
                )
            })?;
            output.lock().await.checked_pause().await?;
        }
        self.persist_position(zone_id).await;
        crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .save_play_state(zone_id, "paused")
            .ok();
        self.playback.pause(zone_id).await;
        Ok(())
    }

    pub async fn resume(&self, zone_id: i64, device_id: Option<&str>) -> OutputCommandResult<()> {
        // Position is preserved across pause (playback state isn't reset), so we
        // know where to resume from.
        let state = self.playback.get_state(zone_id).await;
        let position_ms = state.position_ms.max(0) as u64;

        // WEBRADIO : une reprise après une pause longue — ou après la mort du
        // producteur de décodage — est traitée comme un RE-PLAY de la station
        // (#1629). Un flux radio est un DIRECT : pendant la pause le pipeline
        // continue de se périmer (la connexion icecast peut mourir par un
        // chemin qui ne logge qu'en debug!, la sortie accumule un retard sans
        // borne, les horodatages OAAT prennent toute la durée de la pause de
        // retard) et la reprise « sur place » rend du silence sans la moindre
        // erreur (.42 : pause 15:48 → reprise 16:07, aucun son, volume dans le
        // vide). Rejouer dans CE chemin commun couvre les trois familles de
        // sorties (locale, OAAT, réseau) — et c'est de toute façon le
        // comportement attendu d'un direct : on reprend le direct, pas un
        // différé de 19 minutes.
        if let Some(np) = state.now_playing.as_ref() {
            let has_url = np.source_id.as_deref().is_some_and(|s| !s.is_empty());
            if np.source == "radio" && has_url {
                let paused_long = state
                    .paused_at
                    .is_some_and(|t| t.elapsed() >= RADIO_RESUME_REPLAY_AFTER);
                // Producteur mort même sous le seuil : plus rien n'alimente la
                // session WAV, reprendre sur place serait silencieux aussi.
                let producer_dead = match np.stream_id.as_deref() {
                    Some(sid) => self.streamer.radio_producer_done(sid).await,
                    None => false,
                };
                if paused_long || producer_dead {
                    let did = device_id.map(str::to_string).or_else(|| {
                        ZoneRepo::with_backend(self.db.clone())
                            .get(zone_id)
                            .ok()
                            .flatten()
                            .and_then(|z| z.output_device_id)
                    });
                    if let Some(did) = did {
                        info!(zone_id, paused_long, producer_dead, "radio_resume_replay");
                        let req = PlayRequest {
                            zone_id,
                            output_device_id: Some(did),
                            track_id: None,
                            source: Some("radio".into()),
                            source_id: np.source_id.clone(),
                            title: Some(np.title.clone()),
                            artist_name: np.artist_name.clone(),
                            album_title: np.album_title.clone(),
                            cover_url: np.cover_path.clone(),
                            duration_ms: None,
                            seek_ms: None,
                            temp_file_path: None,
                            sample_rate: None,
                            bit_depth: None,
                            media_format: None,
                            track_number: None,
                            disc_number: None,
                        };
                        // Même station, même écoute logique : pas de nouvelle
                        // ligne d'historique (même règle que radio_auto_retry).
                        match self.play_without_history(req).await {
                            Ok(_) => return Ok(()),
                            Err(e) => warn!(
                                zone_id,
                                error = %e,
                                "radio_resume_replay_failed_falling_back"
                            ),
                        }
                    }
                }
            }
        }

        let output_type = if let Some(did) = device_id {
            let output = { self.outputs.lock().await.get(did) }.ok_or_else(|| {
                OutputCommandError::failed(
                    OutputCommand::Resume,
                    format!("output {did} is not registered"),
                )
            })?;
            let out = output.lock().await;
            let t = out.output_type().to_string();
            out.checked_resume().await?;
            Some(t)
        } else {
            None
        };

        self.playback.resume(zone_id).await;
        crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .save_play_state(zone_id, "playing")
            .ok();

        // Legacy DLNA/OpenHome renderers (e.g. Cyrus Stream X) restart the stream
        // on Play-after-Pause instead of resuming. Seek back to the paused
        // position once the renderer has had a moment to (re)start, so playback
        // continues instead of replaying from the top. Locks are released during
        // the wait so other zones aren't blocked.
        if matches!(output_type.as_deref(), Some("dlna" | "openhome")) && position_ms > 3000 {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            let did = device_id.expect("output type only exists with a device id");
            let output = { self.outputs.lock().await.get(did) };
            if let Some(output) = output {
                match output.lock().await.checked_seek(position_ms).await {
                    Ok(()) => info!(zone_id, position_ms, "dlna_resume_seek"),
                    Err(e) => warn!(zone_id, position_ms, error = %e, "dlna_resume_seek_failed"),
                }
            }
        }
        Ok(())
    }

    /// Les appareils qu'une AUTRE zone que `zone_id` revendique comme sienne.
    ///
    /// Sert à borner le repli de `stop` : ces sorties-là ne lui appartiennent
    /// pas, les arrêter revient à couper la musique de quelqu'un d'autre.
    /// Prend une projection `(id de zone, appareil)` plutôt que des `Zone`
    /// entières : la règle ne dépend que de ces deux champs, et une fonction
    /// qui n'exige pas de construire une zone complète est une fonction qu'on
    /// peut réellement tester.
    fn sorties_revendiquees_par_les_autres_zones<'a>(
        zones: impl IntoIterator<Item = (Option<i64>, Option<&'a str>)>,
        zone_id: i64,
    ) -> std::collections::HashSet<String> {
        zones
            .into_iter()
            .filter(|(id, _)| *id != Some(zone_id))
            .filter_map(|(_, appareil)| appareil.map(str::to_string))
            .collect()
    }

    /// Quelles sorties le repli de `stop` a le droit de toucher.
    ///
    /// Tout ce qui est revendiqué par une autre zone est épargné, sans
    /// exception : c'est l'invariant que ce repli avait perdu.
    fn sorties_a_arreter_en_repli(
        toutes: &[String],
        revendiquees_ailleurs: &std::collections::HashSet<String>,
    ) -> Vec<String> {
        toutes
            .iter()
            .filter(|did| !revendiquees_ailleurs.contains(did.as_str()))
            .cloned()
            .collect()
    }

    pub async fn stop(&self, zone_id: i64, device_id: Option<&str>) {
        self.persist_position(zone_id).await;
        crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .save_play_state(zone_id, "stopped")
            .ok();
        self.cleanup_gapless_session(zone_id).await;
        self.prefetch.clear().await;
        // Forget the last network-play record so a stop→replay of the SAME track
        // is not mistaken for the duplicate-send it guards against (play_inner).
        self.last_net_play.lock().await.remove(&zone_id);
        // Une zone navigateur arrêtée avant que l'onglet ait tiré quoi que ce
        // soit n'a rien fait entendre : son annonce en attente meurt ici
        // (#1998).
        self.oublier_annonce_navigateur(zone_id);
        let state = self.playback.get_state(zone_id).await;
        let old_stream_id = state
            .now_playing
            .as_ref()
            .and_then(|np| np.stream_id.clone());
        self.playback.stop(zone_id).await;

        // Resolve device_id: prefer explicit, fall back to zone DB
        let resolved_did = match device_id {
            Some(d) => Some(d.to_string()),
            None => crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id),
        };
        if let Some(ref did) = resolved_did {
            let arc = { self.outputs.lock().await.get(did) };
            if let Some(output) = arc {
                if let Err(e) = output.lock().await.stop().await {
                    warn!(zone_id, error = %e, "device_stop_failed");
                }
            }
        } else {
            // Aucun appareil résolu. L'ancien repli arrêtait TOUTES les sorties
            // enregistrées — et c'est ainsi qu'un `next` sur une zone
            // « navigateur » (son dans le navigateur, donc AUCUN appareil côté
            // serveur, par construction : `reject_if_zone_has_no_output_device`
            // laisse justement passer ce cas) coupait la musique de tout le
            // monde.
            //
            // Mesuré sur .18 le 28/08/2026 : dix arrêts en une heure, cadence
            // ~100 s, chacun envoyant `dlna_stop` à l'Eversolo qui jouait la
            // zone 10 et aux deux Sonos. La zone 10 n'étant elle-même jamais
            // passée en « stopped », rien ne reprenait avant que le poller ne
            // déclenche `radio_auto_retry` — jusqu'à près de trois minutes de
            // silence à chaque fois.
            //
            // Le repli garde son utilité — rattraper une sortie orpheline que
            // CETTE zone aurait laissée ouverte — mais il ne doit jamais
            // toucher une sortie qu'une AUTRE zone revendique. Même famille
            // que #2571 : un ordre qui sort du périmètre de la zone active.
            let toutes_les_zones = crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
                .list()
                .unwrap_or_default();
            let revendiquees_ailleurs = Self::sorties_revendiquees_par_les_autres_zones(
                toutes_les_zones
                    .iter()
                    .map(|z| (z.id, z.output_device_id.as_deref())),
                zone_id,
            );
            // Snapshot the Arcs first and release the registry lock, so a slow
            // or offline renderer's stop() SOAP timeout can't hold the lock and
            // starve concurrent playback for ~100s (send_to_output_lock_contention).
            let arcs: Vec<_> = {
                let outputs = self.outputs.lock().await;
                Self::sorties_a_arreter_en_repli(&outputs.list(), &revendiquees_ailleurs)
                    .iter()
                    .filter_map(|did| outputs.get(did))
                    .collect()
            };
            let arretees = arcs.len();
            for output in arcs {
                let _ = output.lock().await.stop().await;
            }
            warn!(
                zone_id,
                arretees,
                epargnees = revendiquees_ailleurs.len(),
                "stop_fallback_no_device_id — repli borné aux sorties qu'aucune autre zone ne revendique"
            );
        }
        // Remove session AFTER the output has been stopped
        if let Some(ref sid) = old_stream_id {
            self.streamer.remove_session(sid).await;
        }
    }

    pub async fn seek(
        &self,
        zone_id: i64,
        mut position_ms: u64,
        device_id: Option<&str>,
    ) -> OutputCommandResult<()> {
        let seek_start = std::time::Instant::now();
        if let Some(did) = device_id {
            let output = { self.outputs.lock().await.get(did) }.ok_or_else(|| {
                OutputCommandError::failed(
                    OutputCommand::Seek,
                    format!("output {did} is not registered"),
                )
            })?;
            output
                .lock()
                .await
                .capabilities()
                .require(OutputCommand::Seek)?;
        }
        // Clamp seek to track duration to prevent out-of-bounds seek on files
        // with incorrect metadata duration (e.g. VBR MP3 with wrong header).
        let state = self.playback.get_state(zone_id).await;
        let original_position_ms = state.position_ms;
        if let Some(ref np) = state.now_playing {
            if np.duration_ms > 0 && position_ms > np.duration_ms as u64 {
                info!(
                    zone_id,
                    requested = position_ms,
                    duration = np.duration_ms,
                    "seek_clamped_to_duration"
                );
                position_ms = (np.duration_ms as u64).saturating_sub(1000);
            }
        }
        if let Some(did) = device_id {
            // For streaming tracks on network outputs (DLNA, OpenHome, etc.),
            // the seek strategy depends on whether the stream session supports
            // HTTP Range-based seeking:
            //
            // - Proxy sessions (FLAC from Tidal/Qobuz CDN) and file sessions
            //   support Range requests.  The renderer can seek by closing the
            //   current HTTP connection and re-requesting with a byte offset.
            //   For these, a direct SOAP Seek command is sufficient — the
            //   renderer handles the rest.
            //
            // - Decoded/transcoded sessions (WAV via mpsc channel) do NOT
            //   support Range seeking.  For these, we must recreate the stream
            //   session as a fallback.
            let is_streaming_source = state
                .now_playing
                .as_ref()
                .map(|np| {
                    np.source != "local"
                        && np.source != "radio"
                        && np.source != "podcast"
                        && np.stream_id.is_some()
                })
                .unwrap_or(false);

            // Determine output type from zone DB (avoids locking the output)
            let zone_output_type = ZoneRepo::with_backend(self.db.clone())
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_type);
            let is_network = matches!(
                zone_output_type.as_deref(),
                Some("dlna")
                    | Some("openhome")
                    | Some("chromecast")
                    | Some("bluos")
                    | Some("squeezebox")
                    | Some("slimproto")
            );

            if is_streaming_source && is_network {
                // Check if the current stream session supports Range seeking
                let stream_id = state
                    .now_playing
                    .as_ref()
                    .and_then(|np| np.stream_id.clone());
                let is_seekable = if let Some(ref sid) = stream_id {
                    self.streamer.is_seekable_session(sid).await
                } else {
                    false
                };

                if is_seekable {
                    // Proxy/file session: the stream handler already supports
                    // Range-based seeking.  Send a direct SOAP Seek — the
                    // renderer will close the current connection and re-request
                    // with the appropriate byte offset.  Same stream URL, no
                    // interruption, no "new track" artifact.
                    info!(
                        zone_id,
                        position_ms,
                        source = ?state.now_playing.as_ref().map(|np| &np.source),
                        stream_id = ?stream_id,
                        "seek_streaming_direct_on_seekable_session"
                    );

                    let output = { self.outputs.lock().await.get(did) }.ok_or_else(|| {
                        OutputCommandError::failed(
                            OutputCommand::Seek,
                            format!("output {did} disappeared during seek"),
                        )
                    })?;
                    output.lock().await.checked_seek(position_ms).await?;
                    info!(
                        zone_id,
                        position_ms,
                        seek_ms = seek_start.elapsed().as_millis() as u64,
                        "seek_streaming_direct_complete"
                    );
                } else {
                    // Decoded/transcoded session (WAV via mpsc): no Range
                    // support.  Recreate the stream so the renderer gets a
                    // fresh URL to buffer from.
                    info!(
                        zone_id,
                        position_ms,
                        source = ?state.now_playing.as_ref().map(|np| &np.source),
                        "seek_streaming_on_network_output_recreating_stream"
                    );

                    // Pre-set the seek timestamp BEFORE play() so the poller's
                    // seek grace period covers the entire stream-recreation
                    // window.  play() calls playback.play() which increments
                    // track_generation and clears last_seek_at — we re-set it
                    // again after play() returns (and once more after the Seek
                    // command) to maintain continuous coverage.
                    self.playback.seek(zone_id, position_ms as i64).await;

                    // Re-create the stream: build a PlayRequest from the current NowPlaying
                    let np = state.now_playing.as_ref().unwrap();
                    let output_device_id = ZoneRepo::with_backend(self.db.clone())
                        .get(zone_id)
                        .ok()
                        .flatten()
                        .and_then(|z| z.output_device_id);
                    let req = PlayRequest {
                        zone_id,
                        output_device_id,
                        track_id: np.track_id,
                        source: Some(np.source.clone()),
                        source_id: np.source_id.clone(),
                        title: Some(np.title.clone()),
                        artist_name: np.artist_name.clone(),
                        album_title: np.album_title.clone(),
                        cover_url: np.cover_path.clone(),
                        duration_ms: Some(np.duration_ms),
                        seek_ms: None,
                        temp_file_path: None,
                        sample_rate: None,
                        bit_depth: None,
                        media_format: None,
                        track_number: None,
                        disc_number: None,
                    };

                    match self.play_without_history(req).await {
                        Ok(_) => {
                            // play() cleared last_seek_at — re-set it immediately
                            // so the poller's seek grace covers the buffering window.
                            self.playback.seek(zone_id, position_ms as i64).await;

                            // Stream is now fresh — issue the seek on the output.
                            // Small delay to let the renderer start buffering.
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            let Some(output) = ({ self.outputs.lock().await.get(did) }) else {
                                self.playback.seek(zone_id, original_position_ms).await;
                                return Err(OutputCommandError::failed(
                                    OutputCommand::Seek,
                                    format!("output {did} disappeared during seek"),
                                ));
                            };
                            if let Err(error) = output.lock().await.checked_seek(position_ms).await
                            {
                                self.playback.seek(zone_id, original_position_ms).await;
                                return Err(error);
                            }
                            // Re-set the seek timestamp so the poller grace period
                            // starts from after the Seek SOAP command, not from
                            // the play() call.
                            self.playback.seek(zone_id, position_ms as i64).await;
                            info!(
                                zone_id,
                                position_ms,
                                seek_ms = seek_start.elapsed().as_millis() as u64,
                                "seek_streaming_complete"
                            );
                        }
                        Err(e) => {
                            warn!(zone_id, error = %e, "seek_streaming_play_recreate_failed");
                            // Restore seek timestamp so the poller doesn't
                            // misinterpret the Stopped state as a playback failure.
                            self.playback.seek(zone_id, position_ms as i64).await;
                            // Fall back to direct seek (best effort)
                            let Some(output) = ({ self.outputs.lock().await.get(did) }) else {
                                self.playback.seek(zone_id, original_position_ms).await;
                                return Err(OutputCommandError::failed(
                                    OutputCommand::Seek,
                                    format!("output {did} disappeared during seek"),
                                ));
                            };
                            if let Err(error) = output.lock().await.checked_seek(position_ms).await
                            {
                                self.playback.seek(zone_id, original_position_ms).await;
                                return Err(error);
                            }
                        }
                    }
                }
            } else {
                // Local + OAAT outputs consume a sequential HTTP transcode
                // stream (mpsc / chunked), so we must stop+replay from the seek
                // position rather than range-seek in place. OAAT DSD is served
                // as a chunked WAV transcode that cannot be range-seeked — a raw
                // Range request lands mid-DSD-block and plays WHITE NOISE
                // (Xavier). Recreating with seek_ms restarts the transcode at
                // the correct offset (paired with the DSD-decode seek fix).
                let is_local_output =
                    zone_output_type.as_deref() == Some("local") || zone_output_type.is_none();
                let is_oaat_output = zone_output_type.as_deref() == Some("oaat");
                let has_track = state.now_playing.is_some();

                if (is_local_output || is_oaat_output) && has_track {
                    info!(zone_id, position_ms, "seek_local_output_recreating_stream");
                    match self
                        .replay_zone_at_position(zone_id, position_ms, "seek")
                        .await
                    {
                        Ok(()) => info!(
                            zone_id,
                            position_ms,
                            seek_ms = seek_start.elapsed().as_millis() as u64,
                            "seek_local_output_complete"
                        ),
                        Err(e) => {
                            warn!(zone_id, error = %e, "seek_local_output_play_failed");
                            self.playback.seek(zone_id, original_position_ms).await;
                            return Err(OutputCommandError::failed(OutputCommand::Seek, e));
                        }
                    }
                } else {
                    let output = { self.outputs.lock().await.get(did) }.ok_or_else(|| {
                        OutputCommandError::failed(
                            OutputCommand::Seek,
                            format!("output {did} disappeared during seek"),
                        )
                    })?;
                    output.lock().await.checked_seek(position_ms).await?;
                }
            }
        }

        // La position publique et persistée ne change qu'après confirmation du
        // backend. Les `seek()` temporaires ci-dessus servent uniquement de
        // garde au poller pendant une recréation de flux.
        self.playback.seek(zone_id, position_ms as i64).await;
        let confirmed_state = self.playback.get_state(zone_id).await;
        if let Some(ref np) = confirmed_state.now_playing {
            if let Err(e) = ZoneRepo::with_backend(self.db.clone()).save_playback_position(
                zone_id,
                position_ms as i64,
                np.track_id,
                Some(np.source.as_str()),
                np.source_id.as_deref(),
            ) {
                warn!(zone_id, error = %e, "persist_seek_position_failed");
            }
        }
        Ok(())
    }

    pub async fn set_volume(
        &self,
        zone_id: i64,
        volume: f64,
        device_id: Option<&str>,
    ) -> OutputCommandResult<()> {
        // When fixed_volume is enabled, pin volume to 1.0 (bit-perfect) and
        // skip sending to the device — the DAC/renderer handles volume.
        let zone = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten();
        if zone.as_ref().is_some_and(|z| z.fixed_volume) {
            self.playback.set_volume(zone_id, 1.0).await;
            ZoneRepo::with_backend(self.db.clone())
                .update_volume(zone_id, 100)
                .map_err(|message| OutputCommandError::failed(OutputCommand::SetVolume, message))?;
            return Ok(());
        }

        // Trim de gain par renderer (setting `zone_{id}_gain_trim_db`, ±12 dB) :
        // composé UNIQUEMENT dans la valeur envoyée au device. Le volume
        // affiché/persisté reste celui de l'utilisateur, le cache de
        // transcodage n'est pas affecté (rien n'est cuit dans le PCM), et les
        // zones fixed_volume ne passent jamais ici (early return ci-dessus).
        // Limite assumée : un trim positif est plafonné quand user_volume est
        // déjà haut (clamp 0..1).
        let device_volume = {
            let trim_db = crate::db::settings_repo::SettingsRepo::with_backend(self.db.clone())
                .get(&format!("zone_{zone_id}_gain_trim_db"))
                .ok()
                .flatten()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(0.0);
            (volume * gain_trim_factor(trim_db)).clamp(0.0, 1.0)
        };
        if let Some(did) = device_id {
            let output = { self.outputs.lock().await.get(did) }.ok_or_else(|| {
                OutputCommandError::failed(
                    OutputCommand::SetVolume,
                    format!("output {did} is not registered"),
                )
            })?;
            info!(
                zone_id,
                volume,
                device_volume,
                device_id = did,
                "device_set_volume_sending"
            );
            if let Err(error) = output.lock().await.checked_set_volume(device_volume).await {
                warn!(zone_id, error = %error, "device_set_volume_failed");
                if let Some(ref bus) = self.event_bus {
                    bus.emit(
                        "zone.playback_error",
                        serde_json::json!({
                            "zone_id": zone_id,
                            "error": error.to_string(),
                        }),
                    );
                }
                return Err(error);
            }
        } else {
            info!(zone_id, volume, "set_volume_no_device_id");
        }

        // Le backend a accepté la commande : seulement maintenant les deux
        // copies internes et la base peuvent annoncer la nouvelle valeur.
        self.playback.set_volume(zone_id, volume).await;
        self.playback.mark_volume_changed(zone_id).await;
        ZoneRepo::with_backend(self.db.clone())
            .update_volume(zone_id, (volume.clamp(0.0, 1.0) * 100.0).round() as i32)
            .map_err(|message| OutputCommandError::failed(OutputCommand::SetVolume, message))?;
        Ok(())
    }

    pub async fn set_mute(
        &self,
        zone_id: i64,
        muted: bool,
        device_id: Option<&str>,
    ) -> OutputCommandResult<()> {
        if let Some(did) = device_id {
            let output = { self.outputs.lock().await.get(did) }.ok_or_else(|| {
                OutputCommandError::failed(
                    OutputCommand::SetMute,
                    format!("output {did} is not registered"),
                )
            })?;
            output.lock().await.checked_set_mute(muted).await?;
        }
        self.playback.set_mute(zone_id, muted).await;
        ZoneRepo::with_backend(self.db.clone())
            .update_muted(zone_id, muted)
            .map_err(|message| OutputCommandError::failed(OutputCommand::SetMute, message))?;
        Ok(())
    }

    /// Clear the prefetch buffer. Should be called when the queue changes
    /// (add/remove/reorder) so stale prefetched data is discarded.
    pub async fn clear_prefetch(&self) {
        self.prefetch.clear().await;
    }

    /// Persist the play_queue table for a zone with the given local track IDs.
    /// Called after queue mutations to keep the DB in sync with in-memory state.
    pub fn persist_local_queue(&self, zone_id: i64, track_ids: &[i64], current_position: i64) {
        let repo = PlayQueueRepo::with_backend(self.db.clone());
        if let Err(e) = repo.set_queue(zone_id, track_ids) {
            warn!(zone_id, error = %e, "persist_local_queue_failed");
            return;
        }
        if current_position > 0 {
            repo.set_current(zone_id, current_position).ok();
        }
    }

    /// Persist the streaming_queue table for a zone.
    pub fn persist_streaming_queue(
        &self,
        zone_id: i64,
        tracks: &[crate::db::play_queue_repo::StreamingQueueItem],
    ) {
        let repo = PlayQueueRepo::with_backend(self.db.clone());
        if let Err(e) = repo.set_streaming_queue(zone_id, tracks) {
            warn!(zone_id, error = %e, "persist_streaming_queue_failed");
        }
    }

    /// Cette sortie produit-elle des niveaux exploitables ?
    ///
    /// `false` sur le seul chemin qui n'en produit aucun : OAAT en DSD natif,
    /// où la sortie ouvre le `.dsf` elle-même et expédie du 1 bit sans que
    /// personne ne décode. Les VU-mètres n'y reçoivent rien — l'aiguille reste
    /// où elle est, ce qui se lit comme une panne alors que c'est une absence
    /// de mesure. Le client ne peut pas deviner la différence entre « pas de
    /// niveaux » et « des niveaux qui tardent » : c'est au serveur de le dire.
    ///
    /// Rendre du DSD en PCM en parallèle rien que pour animer deux aiguilles
    /// coûterait, pendant l'écoute, exactement le décodage qu'on a retiré de
    /// ce chemin (blocage Zicmu, `dsd_streaming_send_timeout`).
    pub async fn output_produces_levels(&self, device_id: Option<&str>) -> bool {
        let Some(device_id) = device_id else {
            return true;
        };
        #[cfg(feature = "oaat")]
        if device_id.starts_with("oaat:") {
            let arc = { self.outputs.lock().await.get(device_id) };
            if let Some(arc) = arc {
                let output = arc.lock().await;
                if let Some(oaat) = output
                    .as_any()
                    .downcast_ref::<crate::outputs::oaat::OaatOutput>()
                {
                    return !oaat.is_native_dsd_active();
                }
            }
        }
        true
    }

    pub async fn play_from_queue(&self, zone_id: i64, position: i64) -> Result<PlayResult, String> {
        let queue_repo = PlayQueueRepo::with_backend(self.db.clone());

        let output_device_id = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id);

        // Unified single-position-space resolution: `position` indexes ONE
        // ordered queue (local + streaming). Look the row up directly — no more
        // "try local, then offset into streaming by position - local_count",
        // which broke manual Next across a source boundary (Sandro S2: the local
        // "next" was never found after a Qobuz track, so the zone froze).
        queue_repo.set_current_pos(zone_id, position).ok();
        let total = queue_repo.count_all(zone_id)?;
        let entry = queue_repo
            .get_at(zone_id, position)?
            .ok_or("no queue item at position")?;

        let req = if let Some(track_id) = entry.track_id {
            // Local track.
            PlayRequest {
                zone_id,
                output_device_id,
                track_id: Some(track_id),
                source: None,
                source_id: None,
                title: entry.title.clone(),
                artist_name: entry.artist_name.clone(),
                album_title: entry.album_title.clone(),
                cover_url: entry.cover_path.clone(),
                duration_ms: entry.duration_ms,
                seek_ms: None,
                temp_file_path: None,
                sample_rate: None,
                bit_depth: None,
                media_format: None,
                track_number: entry.track_number.map(|n| n as u32),
                disc_number: entry.disc_number.map(|n| n as u32),
            }
        } else {
            // Streaming track.
            let source_id = entry.source_id.clone().unwrap_or_default();
            let mut title = entry.title.clone();
            let mut artist = entry.artist_name.clone();
            let mut album = entry.album_title.clone();
            let mut cover = entry.cover_path.clone();
            let mut duration_ms = entry.duration_ms;

            let current_state = self.playback.get_state(zone_id).await;

            // Repeat on a single-track queue re-plays the SAME position, but the
            // streaming row can carry an empty title (persisted without
            // metadata). play() would then hand an empty title down the
            // prefetched path and blank Now Playing (DEvir). When the row title
            // is empty AND now_playing is still the very same track (same
            // source_id), reuse its metadata synchronously — no network
            // round-trip, and it can't mislabel a different track since the
            // source_id must match.
            let title_empty = title.as_deref().unwrap_or("").is_empty();
            if title_empty
                && let Some(np) = current_state.now_playing.as_ref()
                && np.source_id.as_deref() == Some(source_id.as_str())
                && !np.title.is_empty()
            {
                title = Some(np.title.clone());
                artist = artist.or_else(|| np.artist_name.clone());
                album = album.or_else(|| np.album_title.clone());
                cover = cover.or_else(|| np.cover_path.clone());
                // Also reuse the duration: filling ONLY the title from a row
                // whose duration_ms is 0 armed the worst combo downstream —
                // has_title=true disables resolve_streaming_url's get_track
                // duration backfill (reserved for empty titles), duration 0
                // reaches the exclusive local output, and the poller's
                // position-past-end advance (#483) requires duration > 0: on
                // a Repeat All loop transition the ring starved at exactly
                // one track length and playback froze forever, zone stuck
                // "Playing" with a frozen position (DEvir, v0.9.14, ASIO).
                if duration_ms.unwrap_or(0) == 0 && np.duration_ms > 0 {
                    duration_ms = Some(np.duration_ms);
                }
            }

            // Use the stored source, falling back to the current now_playing
            // source (handles old DB rows without a source value).
            let source = entry
                .source
                .clone()
                .filter(|s| !s.is_empty() && s != "local")
                .unwrap_or_else(|| {
                    current_state
                        .now_playing
                        .as_ref()
                        .map(|np| np.source.clone())
                        .unwrap_or_else(|| "tidal".into())
                });

            PlayRequest {
                zone_id,
                output_device_id,
                track_id: None,
                source: Some(source),
                source_id: Some(source_id),
                title,
                artist_name: artist,
                album_title: album,
                cover_url: cover,
                duration_ms,
                seek_ms: None,
                temp_file_path: None,
                sample_rate: None,
                bit_depth: None,
                media_format: None,
                track_number: entry.track_number.map(|n| n as u32),
                disc_number: entry.disc_number.map(|n| n as u32),
            }
        };

        // Set the queue index BEFORE play() emits "started" so the event
        // carries the correct queue_position and the client updates its
        // highlight without refetching the whole queue (#1096).
        self.playback
            .update_queue_info(zone_id, position, total)
            .await;
        let result = self.play(req).await?;
        Ok(result)
    }

    pub async fn advance_queue_metadata(&self, zone_id: i64, position: i64) -> Result<(), String> {
        let queue_repo = PlayQueueRepo::with_backend(self.db.clone());
        queue_repo.set_current_pos(zone_id, position).ok();

        let total = queue_repo.count_all(zone_id)?;
        let entry = queue_repo
            .get_at(zone_id, position)?
            .ok_or("no queue item at position")?;

        let np = if let Some(track_id) = entry.track_id {
            let track_repo = crate::db::track_repo::TrackRepo::with_backend(self.db.clone());
            let track = track_repo.get(track_id).ok().flatten();
            let cover_path = track.as_ref().and_then(|t| t.cover_path.clone());
            // Audio-format fields (format/sample_rate/bit_depth/genre/year) come
            // from the library row via `from_track` (single source of the
            // source-over-output bit-depth rule); display fields come from the
            // queue-entry cache and source is pinned local.
            crate::playback::NowPlaying {
                track_id: Some(track_id),
                title: entry.title.clone().unwrap_or_default(),
                artist_name: entry.artist_name.clone(),
                album_title: entry.album_title.clone(),
                cover_path: self.resolve_cover_url(cover_path.as_deref()),
                duration_ms: entry.duration_ms.unwrap_or(0),
                source: "local".into(),
                source_id: None,
                ..track
                    .as_ref()
                    .map(crate::playback::NowPlaying::from_track)
                    .unwrap_or_default()
            }
        } else {
            let source = entry
                .source
                .clone()
                .filter(|s| !s.is_empty() && s != "local")
                .unwrap_or_else(|| "streaming".into());
            let source = if source == "streaming" {
                let cs = self.playback.get_state(zone_id).await;
                cs.now_playing
                    .as_ref()
                    .map(|np| np.source.clone())
                    .unwrap_or_else(|| "streaming".into())
            } else {
                source
            };
            crate::playback::NowPlaying {
                track_id: None,
                title: entry.title.clone().unwrap_or_default(),
                artist_name: entry.artist_name.clone(),
                album_title: entry.album_title.clone(),
                cover_path: self.resolve_cover_url(entry.cover_path.as_deref()),
                duration_ms: entry.duration_ms.unwrap_or(0),
                source,
                source_id: entry.source_id.clone(),
                stream_id: None,
                ..Default::default()
            }
        };

        // Set the queue index BEFORE update_now_playing emits "track_changed"
        // so the event carries the new queue_position — the client updates its
        // highlight/badge without refetching the whole queue (#1096).
        self.playback
            .update_queue_info(zone_id, position, total)
            .await;
        // Last.fm/ListenBrainz "now playing": a gapless advance is a real track
        // change, but it bypasses play_inner — the only other dispatch site —
        // so the now-playing of every gapless-reached track (tracks 2, 4, 6… of
        // an album) was never sent (#1113). This method is the single funnel
        // for all gapless advance paths (position reset, duration change,
        // confirmed pending advance), so dispatch here exactly once per track.
        self.dispatch_now_playing(
            &np.title,
            np.artist_name.as_deref(),
            np.album_title.as_deref(),
        );
        // Use update_now_playing (not play) to avoid bumping track_generation —
        // the poller must keep its gapless_cooldown intact so it doesn't falsely
        // detect track-end on renderers that briefly report Stopped during
        // gapless transitions. Position MUST reset to 0 (new track from start).
        let advance_track_id = np.track_id;
        let advance_source = np.source.clone();
        let advance_source_id = np.source_id.clone();
        self.playback.update_now_playing(zone_id, np).await;
        self.playback.update_position(zone_id, 0).await;
        self.playback.emit_position(zone_id, 0);

        // Niveaux de la piste devenue courante. Le pré-chargement gapless
        // n'attache pas de forwarder (ses fenêtres seraient datées de
        // l'horloge de la piste précédente — stamps ~4 min devant le
        // renderer, VU morts, observé sur Stream X) : on invalide ici les
        // forwarders de l'ancienne piste puis on démarre un décodage dédié,
        // position 0, comme pour une lecture explicite en passthrough.
        self.playback.bump_levels_gen(zone_id);
        if let (Some(bus), Some(track_id)) = (self.event_bus.clone(), advance_track_id) {
            let track = crate::db::track_repo::TrackRepo::with_backend(self.db.clone())
                .get(track_id)
                .ok()
                .flatten();
            let format = track.as_ref().and_then(|t| t.format.clone());
            // La sortie ne se consulte QUE pour du DSD : c'est le seul format
            // dont le décodage-pour-niveaux coûte assez cher pour valoir une
            // lecture de zone, et le seul chemin qui ne mesure pas (OAAT en
            // DSD natif) n'y arrive qu'en jouant du DSD. Interroger la sortie
            // pour tous les formats aurait éteint les VU d'un FLAC enchaîné
            // juste après un DSD, tant que le drapeau natif n'est pas retombé.
            let la_sortie_mesure = if est_source_dsd(format.as_deref()) {
                let device_id = ZoneRepo::with_backend(self.db.clone())
                    .get(zone_id)
                    .ok()
                    .flatten()
                    .and_then(|z| z.output_device_id);
                self.output_produces_levels(device_id.as_deref()).await
            } else {
                true
            };
            if let Some(path) = fichier_a_mesurer_apres_avance(
                format.as_deref(),
                track.and_then(|t| t.file_path),
                la_sortie_mesure,
            ) {
                // Génération épinglée ici : l'avance vient d'avoir lieu, c'est
                // bien la piste devenue courante (#1110).
                let play_seq = self.playback.current_play_seq(zone_id).await;
                spawn_local_file_levels_decode(bus, self.playback.clone(), zone_id, play_seq, path);
            }
        } else if let (Some(bus), Some(source_id)) =
            (self.event_bus.clone(), advance_source_id.clone())
        {
            // Piste STREAMING devenue courante par avance gapless : pas de
            // fichier local à décoder — sa session prewarm n'a jamais attaché
            // de forwarder (levels_prewarm), donc les pistes 2..n d'un album
            // Qobuz/Tidal gardaient les aiguilles figées même une fois le
            // proxy corrigé pour la lecture explicite. On re-résout l'URL du
            // service (cache DASH compris) et on lance la même sonde de
            // niveaux que la lecture explicite en proxy ; un `file://` (fMP4
            // DASH déjà sur disque) se décode localement, comme une piste
            // passthrough.
            if advance_source != "local" && advance_source != "radio" {
                let services = self.services.clone();
                let playback = self.playback.clone();
                let source = advance_source.clone();
                // Épinglé AVANT la tâche : la résolution d'URL du service peut
                // prendre plusieurs secondes, et lire la génération à son issue
                // rattachait la sonde à ce que la zone jouait ALORS. Si
                // l'auditeur avait enchaîné entre-temps, le forwarder héritait
                // de la nouvelle génération et survivait, en publiant le PCM de
                // la piste précédente sur l'horloge de la nouvelle (#1110).
                let play_seq = self.playback.current_play_seq(zone_id).await;
                tokio::spawn(async move {
                    let resolved = {
                        let registry = services.lock().await;
                        let Some(svc) = registry.get(&source) else {
                            return;
                        };
                        let svc = svc.clone();
                        drop(registry);
                        let svc = svc.read().await;
                        svc.get_track_url(&source_id, None).await.ok()
                    };
                    let Some(data) = resolved else {
                        debug!(zone_id, source = %source, "gapless_streaming_levels_url_unresolved");
                        return;
                    };
                    let codec = data.quality.codec.to_lowercase();
                    if let Some(path) = data.url.strip_prefix("file://") {
                        // fMP4 DASH assemblé sur disque : décodage local
                        // direct, même motif — et même helper — que la branche
                        // fichier ci-dessus.
                        let play_seq = playback.current_play_seq(zone_id).await;
                        spawn_local_file_levels_decode(
                            bus,
                            playback,
                            zone_id,
                            play_seq,
                            path.to_string(),
                        );
                    } else {
                        spawn_proxy_levels_probe_task(
                            playback, bus, zone_id, data.url, codec, play_seq,
                        );
                    }
                });
            }
        }
        Ok(())
    }

    pub async fn resolve_queue_item_url(
        &self,
        zone_id: i64,
        position: i64,
    ) -> Result<ResolvedQueueItem, String> {
        // Pré-chargement gapless : pas de forwarder de niveaux sur les
        // sessions créées ici (voir `levels_prewarm`).
        let _prewarm = self.begin_levels_prewarm(zone_id);
        // Clean up any previously prepared gapless session for this zone
        // before creating a new one.
        self.cleanup_gapless_session(zone_id).await;

        let queue_repo = PlayQueueRepo::with_backend(self.db.clone());

        // Unified single-position-space lookup (local or streaming).
        let entry = queue_repo
            .get_at(zone_id, position)?
            .ok_or("no queue item at position (local or streaming)")?;

        // Local track.
        if let Some(track_id) = entry.track_id {
            let album = entry.album_title.clone();
            let cover = entry.cover_path.clone();
            // Resolve the gapless/prefetch stream FOR THE ACTUAL OUTPUT. Without
            // the device id, resolve_stream doesn't apply the output's format
            // rules, so a local output (which needs WAV/PCM) was pre-armed with
            // the raw FLAC stream — the local gapless chain then hit a non-WAV
            // header and fell back (local_audio_gapless_next_not_wav_falling_back),
            // breaking seamless FLAC gapless (Jean Valjean).
            let output_device_id = ZoneRepo::with_backend(self.db.clone())
                .get(zone_id)
                .ok()
                .flatten()
                .and_then(|z| z.output_device_id);
            let req = PlayRequest {
                zone_id,
                output_device_id,
                track_id: Some(track_id),
                source: None,
                source_id: None,
                title: entry.title.clone(),
                artist_name: entry.artist_name.clone(),
                album_title: album.clone(),
                cover_url: cover.clone(),
                duration_ms: entry.duration_ms,
                seek_ms: None,
                temp_file_path: None,
                sample_rate: None,
                bit_depth: None,
                media_format: None,
                track_number: None,
                disc_number: None,
            };
            let resolved = self.resolve_stream(&req).await?;
            if let Some(ref sid) = resolved.stream_id {
                self.gapless_sessions
                    .lock()
                    .await
                    .insert(zone_id, sid.clone());
            }
            let raw_cover = cover.or(resolved.cover_url);
            return Ok(ResolvedQueueItem {
                url: resolved.url,
                mime_type: resolved.mime_type,
                title: resolved.title,
                artist: resolved.artist,
                album,
                cover_url: self.resolve_cover_url(raw_cover.as_deref()),
                duration_ms: resolved.duration_ms.map(|d| d as u64),
                stream_id: resolved.stream_id,
                sample_rate: resolved.sample_rate,
                bit_depth: resolved.bit_depth,
                channels: resolved.channels,
                file_size: resolved.file_size,
                file_path: None,
                source: Some("local".into()),
                source_id: Some(track_id.to_string()),
                track_number: entry.track_number.map(|n| n as u32),
                disc_number: entry.disc_number.map(|n| n as u32),
            });
        }

        // Streaming track (Tidal, Qobuz, Deezer, etc.).
        let source_id = entry.source_id.clone().unwrap_or_default();
        let title = entry.title.clone();
        let artist = entry.artist_name.clone();
        let album = entry.album_title.clone();
        let cover = entry.cover_path.clone();
        let duration = entry.duration_ms;
        let source = match entry
            .source
            .clone()
            .filter(|s| !s.is_empty() && s != "local")
        {
            Some(s) => s,
            None => {
                let cs = self.playback.get_state(zone_id).await;
                cs.now_playing
                    .as_ref()
                    .map(|np| np.source.clone())
                    .unwrap_or_else(|| "tidal".into())
            }
        };
        let output_device_id = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id);
        let req = PlayRequest {
            zone_id,
            output_device_id,
            track_id: None,
            source: Some(source),
            source_id: Some(source_id.clone()),
            title: title.clone(),
            artist_name: artist.clone(),
            album_title: album.clone(),
            cover_url: cover.clone(),
            duration_ms: duration,
            seek_ms: None,
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: None,
            disc_number: None,
        };
        let resolved = self.resolve_stream(&req).await?;
        if let Some(ref sid) = resolved.stream_id {
            self.gapless_sessions
                .lock()
                .await
                .insert(zone_id, sid.clone());
        }
        let raw_cover = cover.or(resolved.cover_url);
        Ok(ResolvedQueueItem {
            url: resolved.url,
            mime_type: resolved.mime_type,
            // Prefer the queue item's metadata (the streaming resolve returns an
            // empty title for Tidal/Qobuz) so the gapless-next SetNext carries
            // the real title instead of blanking it (DEvir).
            title: title.filter(|s| !s.is_empty()).unwrap_or(resolved.title),
            artist: artist.filter(|s| !s.is_empty()).or(resolved.artist),
            album,
            cover_url: self.resolve_cover_url(raw_cover.as_deref()),
            duration_ms: resolved.duration_ms.map(|d| d as u64),
            stream_id: resolved.stream_id,
            sample_rate: resolved.sample_rate,
            bit_depth: resolved.bit_depth,
            channels: resolved.channels,
            file_size: resolved.file_size,
            file_path: None,
            source: entry.source.clone(),
            source_id: Some(source_id.clone()),
            track_number: entry.track_number.map(|n| n as u32),
            disc_number: entry.disc_number.map(|n| n as u32),
        })
    }

    /// Resolve the next queue item as a LOCAL FILE — file path + metadata + native
    /// format, read straight from the DB WITHOUT creating a transcode/stream
    /// session. Used for OAAT native-DSD gapless: the output opens the `.dsf`
    /// directly, so spinning up the usual DSD->PCM transcode (as the URL path
    /// does) would only orphan an unconsumed decode (`dsd_streaming_send_timeout`)
    /// and stall the transition. Returns Ok with `file_path: None` when the next
    /// item is a streaming track or has no local file — the caller then declines
    /// to arm and lets the natural-end fallback advance the queue.
    pub async fn resolve_gapless_next_local_file(
        &self,
        zone_id: i64,
        position: i64,
    ) -> Result<ResolvedQueueItem, String> {
        // Pré-chargement gapless : pas de forwarder de niveaux (voir
        // `levels_prewarm`).
        let _prewarm = self.begin_levels_prewarm(zone_id);
        // Drop any previously prepared gapless (URL) session for this zone so we
        // don't leak a transcode session when switching to the local-file path.
        self.cleanup_gapless_session(zone_id).await;

        let entry = PlayQueueRepo::with_backend(self.db.clone())
            .get_at(zone_id, position)?
            .ok_or("no queue item at position (local or streaming)")?;

        // A local file is present only for local library tracks; streaming
        // items (track_id None / file_path None) return file_path: None so the
        // caller declines to arm gapless and lets the natural end advance.
        let file_path = entry.track_id.and(entry.file_path.clone());
        let mime_type = entry
            .format
            .as_ref()
            .map(|f| format!("audio/{}", f.to_lowercase()))
            .unwrap_or_default();

        Ok(ResolvedQueueItem {
            url: String::new(),
            mime_type,
            title: entry.title.unwrap_or_default(),
            artist: entry.artist_name,
            album: entry.album_title,
            cover_url: self.resolve_cover_url(entry.cover_path.as_deref()),
            duration_ms: entry.duration_ms.map(|d| d as u64),
            stream_id: None,
            sample_rate: entry.sample_rate.map(|r| r as u32),
            bit_depth: entry.bit_depth.map(|b| b as u32),
            channels: None,
            file_size: None,
            file_path,
            source: entry
                .source
                .clone()
                .or_else(|| entry.track_id.map(|_| "local".to_string())),
            source_id: entry
                .source_id
                .clone()
                .or_else(|| entry.track_id.map(|t| t.to_string())),
            track_number: entry.track_number.map(|n| n as u32),
            disc_number: entry.disc_number.map(|n| n as u32),
        })
    }

    pub async fn wait_stream_data_ready(&self, stream_id: &str, timeout_ms: u64) -> bool {
        self.streamer.wait_data_ready(stream_id, timeout_ms).await
    }

    pub async fn streamer_bytes_sent(&self, stream_id: &str) -> Option<u64> {
        self.streamer.stream_bytes_sent(stream_id).await
    }
    /// Consigne le constat « aucun onglet ne reçoit le son de cette zone »
    /// (voir [`crate::playback::PlaybackManager::note_browser_unattended`]).
    pub async fn note_browser_unattended(&self, zone_id: i64, unattended: bool) {
        self.playback
            .note_browser_unattended(zone_id, unattended)
            .await;
    }

    /// Taille totale du flux (voir [`AudioStreamer::stream_total_bytes`]).
    pub async fn streamer_total_bytes(&self, stream_id: &str) -> Option<u64> {
        self.streamer.stream_total_bytes(stream_id).await
    }

    async fn persist_position(&self, zone_id: i64) {
        let state = self.playback.get_state(zone_id).await;
        if let Some(ref np) = state.now_playing {
            ZoneRepo::with_backend(self.db.clone())
                .save_playback_position(
                    zone_id,
                    state.position_ms,
                    np.track_id,
                    Some(np.source.as_str()),
                    np.source_id.as_deref(),
                )
                .ok();
        }
    }
}

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
                if e.starts_with(RADIO_NOT_AUDIO) {
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
mod transcode_budget_tests {
    use super::transcode_budget_for;
    use std::io::Write;

    fn file_of(bytes: usize) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(&vec![0u8; bytes]).unwrap();
        f.flush().unwrap();
        f
    }

    /// Un petit fichier garde le comportement historique : 120 s.
    #[test]
    fn small_file_gets_the_floor() {
        let f = file_of(4096);
        let d = transcode_budget_for(f.path().to_str().unwrap());
        assert_eq!(d.as_secs(), 120);
    }

    /// Le budget grandit avec la taille — c'est tout l'objet du correctif.
    #[test]
    fn budget_grows_with_size() {
        let small = file_of(1024);
        let big = file_of(300 * 1024 * 1024); // 300 Mio
        let ds = transcode_budget_for(small.path().to_str().unwrap());
        let db = transcode_budget_for(big.path().to_str().unwrap());
        assert!(
            db > ds,
            "un fichier plus gros doit obtenir plus de temps ({db:?} vs {ds:?})"
        );
        // 300 Mio ~ 0,29 Gio -> 120 + ~35 s
        assert!(
            (150..=170).contains(&db.as_secs()),
            "budget inattendu: {db:?}"
        );
    }

    /// Taille illisible : plancher, jamais un budget arbitraire.
    #[test]
    fn unreadable_size_falls_back_to_the_floor() {
        let d = transcode_budget_for("/nonexistent/path/does-not-exist.dsf");
        assert_eq!(d.as_secs(), 120);
    }

    /// Un disque en perdition doit finir par rendre la main.
    #[test]
    fn budget_is_capped() {
        // 30 min = plancher + 120 s/Gio -> plafond atteint vers 14,5 Gio.
        // Verifie sur le calcul, sans ecrire un fichier de cette taille.
        let ceiling = 30 * 60;
        let huge_gib = 100.0_f64;
        let computed = (120 + (huge_gib * 120.0).round() as u64).min(ceiling);
        assert_eq!(computed, ceiling);
    }
}

/// La regle de decision du passthrough DSD (#2122).
///
/// Les douze combinaisons : quatre modes croises avec les trois reponses
/// possibles du sondage. La sonde reseau n'est pas testee ici — c'est
/// justement pour la sortir du chemin qu'elle a ete extraite.
#[cfg(test)]
mod dsd_passthrough_tests {
    use super::PlaybackOrchestrator as O;

    /// La parole de l'utilisateur prime sur le Sink — le terrain l'a exige.
    ///
    /// L'Eversolo DMP-A8 annonce 392 formats dans son GetProtocolInfo, aucun
    /// DSD — et JOUE le .dsf brut. La version precedente de cette regle cedait
    /// devant ce « non » apparent et convertissait en PCM un flux que le
    /// renderer savait lire. Un Sink qui omet un format n'est pas un refus.
    #[test]
    fn la_parole_de_lutilisateur_prime_sur_le_sink() {
        assert!(O::decider_passthrough_dsd("native", Some(false)));
    }

    /// La faute symetrique, celle qu'on ne veut PAS commettre en corrigeant :
    /// un sondage muet n'est pas un refus. Le reglage explicite tient.
    #[test]
    fn native_tient_quand_le_sondage_ne_repond_pas() {
        assert!(O::decider_passthrough_dsd("native", None));
    }

    #[test]
    fn native_tient_quand_le_renderer_confirme() {
        assert!(O::decider_passthrough_dsd("native", Some(true)));
    }

    /// `pcm` est un refus de l'utilisateur : aucune reponse du renderer ne le
    /// renverse, pas meme un oui franc.
    #[test]
    fn pcm_refuse_quoi_que_reponde_le_renderer() {
        for annonce in [Some(true), Some(false), None] {
            assert!(
                !O::decider_passthrough_dsd("pcm", annonce),
                "pcm a laisse passer du DSD avec {annonce:?}"
            );
        }
    }

    /// `dop` non plus n'est pas du passthrough : le renderer doit recevoir le
    /// DSD emballe en trames PCM, donc le fichier passe par la conversion.
    #[test]
    fn dop_n_est_pas_du_passthrough() {
        for annonce in [Some(true), Some(false), None] {
            assert!(
                !O::decider_passthrough_dsd("dop", annonce),
                "dop a laisse passer du DSD brut avec {annonce:?}"
            );
        }
    }

    /// `auto` suit le renderer, et sans reponse prend le chemin sur.
    #[test]
    fn auto_suit_le_sondage_et_se_replie_dans_le_doute() {
        assert!(O::decider_passthrough_dsd("auto", Some(true)));
        assert!(!O::decider_passthrough_dsd("auto", Some(false)));
        assert!(!O::decider_passthrough_dsd("auto", None));
    }

    /// Un mode inconnu en base (valeur ecrite par une version future, champ
    /// vide) doit se comporter comme `auto`, pas envoyer du DSD au hasard.
    #[test]
    fn un_mode_inconnu_se_comporte_comme_auto() {
        assert!(!O::decider_passthrough_dsd("", None));
        assert!(
            !O::decider_passthrough_dsd("Native", None),
            "la casse compte"
        );
    }

    // ── Le message d'échec doit nommer le RÉGLAGE (#2396) ─────────────────
    //
    // Le choix d'envoyer quand même n'est pas remis en cause : « natif » est un
    // réglage explicite et des renderers lisent le DSD sans l'annoncer. C'est
    // le message d'ÉCHEC qui était faux. Il disait « Le renderer a acquitté
    // Play mais joue toujours une autre source » — il accusait l'appareil,
    // alors que le serveur savait AVANT d'envoyer que le Sink annonçait
    // `Some(false)` et que la zone était en « natif ». L'utilisateur cherchait
    // du côté du matériel ; l'un d'eux a réinstallé son système entier.

    /// Le message tel que le renderer le renvoie — celui de #2396, mot pour mot.
    const ECHEC_DLNA: &str = "Le renderer a acquitté Play mais joue toujours une \
         autre source (URI non appliquée après relance)";

    #[test]
    fn le_message_nomme_le_reglage_et_l_action_pas_l_appareil() {
        let msg = O::message_echec_sortie(ECHEC_DLNA, "native", Some(false), "application/x-dsd");

        assert!(
            msg.contains("natif"),
            "le message doit NOMMER le réglage en cause : {msg}"
        );
        assert!(
            msg.contains("DoP") && msg.contains("PCM"),
            "le message doit nommer l'action qui corrige (DoP ou PCM) : {msg}"
        );
        assert!(
            msg.contains("Output device error"),
            "le marqueur qui pilote le 503 côté route ne doit pas disparaître : {msg}"
        );
    }

    /// Zéro régression : hors de ce cas précis, le message ne change pas d'un
    /// caractère. Accuser un réglage à tort serait la faute symétrique.
    #[test]
    fn les_autres_echecs_gardent_leur_message_mot_pour_mot() {
        let intact = format!("Output device error: {ECHEC_DLNA}");

        // Le renderer a dit OUI : l'échec ne vient pas du réglage.
        assert_eq!(
            O::message_echec_sortie(ECHEC_DLNA, "native", Some(true), "application/x-dsd"),
            intact
        );
        // Sondage muet : une absence n'est pas une preuve, on n'accuse rien.
        assert_eq!(
            O::message_echec_sortie(ECHEC_DLNA, "native", None, "application/x-dsd"),
            intact
        );
        // `auto` n'a forcé personne : le passthrough a suivi le renderer.
        assert_eq!(
            O::message_echec_sortie(ECHEC_DLNA, "auto", Some(false), "application/x-dsd"),
            intact
        );
        // Même zone, même appareil, un FLAC : le DSD n'y est pour rien. C'est
        // la preuve par contraste du ticket (FLAC OK / DSF muet à 3 min).
        assert_eq!(
            O::message_echec_sortie(ECHEC_DLNA, "native", Some(false), "audio/flac"),
            intact
        );
    }

    /// Les MIME que prend un DSD brut sur le fil : `application/x-dsd` par
    /// défaut, ou celui que le renderer a annoncé (`audio/x-dsf`, `audio/dff`).
    #[test]
    fn tous_les_mime_dsd_bruts_declenchent_l_explication() {
        for mime in ["application/x-dsd", "audio/x-dsf", "audio/dff", "audio/dsf"] {
            let msg = O::message_echec_sortie(ECHEC_DLNA, "native", Some(false), mime);
            assert!(
                msg.contains("natif"),
                "le MIME {mime} est du DSD brut et n'a pas déclenché l'explication : {msg}"
            );
        }
    }
}

#[cfg(test)]
mod resolution_annoncee_tests {
    use super::resolution_annoncee;
    use crate::db::models::Track;
    use crate::playback::NowPlaying;

    /// Une piste de la bibliotheque dont la ligne ne porte NI frequence NI
    /// profondeur ne doit rien annoncer.
    ///
    /// Le repli `.or(resolved)` de `play_inner` y substituait la resolution de
    /// SORTIE — et pour une piste locale cette valeur est FABRIQUEE :
    /// `resolve_local_track` fait `track.sample_rate.unwrap_or(44100)` et
    /// `track.bit_depth.unwrap_or(16)` precisement quand la ligne se tait, puis
    /// `cap_output_bit_depth` la ramene dans 16..24. Le client affichait donc
    /// « 44,1 kHz / 16 bits » pour un fichier que personne n'a mesure.
    ///
    /// C'est la lecture aleatoire qui le rend visible : elle demarre sans
    /// cesse une PREMIERE piste tiree au hasard, donc une ligne muette bien
    /// plus souvent qu'un album qu'on a choisi (fil 1036, william — #2250).
    #[test]
    fn une_ligne_locale_muette_n_annonce_rien_plutot_qu_une_valeur_inventee() {
        assert_eq!(
            resolution_annoncee(None, Some(44100), true),
            None,
            "frequence : une piste locale sans frequence en base doit rester muette, \
             pas heriter du 44100 fabrique par resolve_local_track"
        );
        assert_eq!(
            resolution_annoncee(None, Some(16), true),
            None,
            "profondeur : une piste locale sans profondeur en base doit rester muette, \
             pas heriter du 16 fabrique par resolve_local_track"
        );
    }

    /// Quand la ligne SAIT, c'est elle qui parle — y compris (surtout) lorsque
    /// la sortie transcode. C'est la regle deja en place, que ce correctif ne
    /// doit pas defaire.
    #[test]
    fn une_ligne_locale_qui_sait_repond_par_sa_propre_valeur() {
        assert_eq!(
            resolution_annoncee(Some(96000), Some(44100), true),
            Some(96000)
        );
        assert_eq!(resolution_annoncee(Some(24), Some(16), true), Some(24));
    }

    /// Le streaming n'a AUCUNE ligne en bibliotheque : la resolution resolue y
    /// est celle de la source, pas d'une sortie. Le repli doit survivre —
    /// sinon Qobuz et Tidal perdent leur affichage.
    #[test]
    fn le_streaming_garde_le_repli_sur_le_format_du_flux() {
        assert_eq!(resolution_annoncee(None, Some(96000), false), Some(96000));
        assert_eq!(resolution_annoncee(None, Some(24), false), Some(24));
    }

    /// La MEME ligne doit s'annoncer pareil qu'on l'atteigne en premiere piste
    /// (`play_inner`) ou par avance gapless (`advance_queue_metadata`, qui
    /// passe par `NowPlaying::from_track`).
    ///
    /// C'est l'asymetrie que decrivait william : la piste 1 d'une file
    /// aleatoire affichait un chiffre, la piste 2 de la meme file n'affichait
    /// rien — pour des lignes de base identiquement muettes.
    #[test]
    fn les_deux_surfaces_annoncent_la_meme_chose_pour_une_ligne_muette() {
        let mut ligne = Track::new("16 bits, profondeur absente de la base".into());
        ligne.id = Some(1);
        ligne.sample_rate = None;
        ligne.bit_depth = None;

        let par_avance_gapless = NowPlaying::from_track(&ligne);

        // Ce que `play_inner` annonce pour cette meme ligne, la sortie ayant
        // fabrique 44100/16.
        let par_premiere_piste_sr =
            resolution_annoncee(ligne.sample_rate.map(|v| v as u32), Some(44100), true);
        let par_premiere_piste_bd =
            resolution_annoncee(ligne.bit_depth.map(|v| v as u32), Some(16), true);

        assert_eq!(
            par_premiere_piste_sr, par_avance_gapless.sample_rate,
            "premiere piste et avance gapless doivent annoncer la MEME frequence"
        );
        assert_eq!(
            par_premiere_piste_bd, par_avance_gapless.bit_depth,
            "premiere piste et avance gapless doivent annoncer la MEME profondeur"
        );
    }

    /// Garde-fou de site d'appel : `play_inner` doit passer par la regle, et
    /// non refaire un `.or(resolved.…)` en direct. Sans cela, la regle
    /// ci-dessus serait vraie en isolation et fausse en production.
    #[test]
    fn play_inner_passe_bien_par_la_regle() {
        let src = include_str!("orchestrator.rs");
        let np = src
            .find("            sample_rate: resolution_annoncee(")
            .zip(src.find("            bit_depth: resolution_annoncee("));
        assert!(
            np.is_some(),
            "le NowPlaying de play_inner doit construire sample_rate ET bit_depth \
             via resolution_annoncee(), sinon la regle ne protege rien"
        );
        assert!(
            !src.contains(".and_then(|t| t.bit_depth.map(|v| v as u32))\n                .or(resolved.bit_depth)"),
            "le repli direct .or(resolved.bit_depth) doit avoir disparu de play_inner"
        );
    }
}

#[cfg(test)]
mod wav_override_tests {
    use super::wav_override_applies;

    /// Le cas de Yves (#1437) : les deux cases cochées, une source FLAC.
    /// Avant, le WAV gagnait en silence et le FLAC natif ne servait à rien.
    #[test]
    fn flac_source_with_native_flac_opt_in_keeps_flac() {
        assert!(!wav_override_applies(true, true, true));
    }

    /// La même zone, une source ALAC : le forçage WAV garde tout son sens,
    /// c'est même sa raison d'être (décodeur ALAC du renderer).
    #[test]
    fn alac_source_still_goes_to_wav() {
        assert!(wav_override_applies(true, false, true));
    }

    /// Sans l'opt-in, une source FLAC suit le forçage comme avant — c'est ce
    /// dont ont besoin les renderers qui ne lisent pas le FLAC.
    #[test]
    fn flac_source_without_opt_in_still_follows_the_override() {
        assert!(wav_override_applies(true, true, false));
    }

    /// Aucun forçage demandé : rien à neutraliser, dans les deux sens.
    #[test]
    fn no_override_requested_stays_off() {
        assert!(!wav_override_applies(false, true, true));
        assert!(!wav_override_applies(false, false, false));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        RADIO_NOT_AUDIO, arm_local_stream_consumer_watch, emit_radio_playback_error,
        non_audio_content_type,
    };
    use crate::event_bus::EventBus;
    use crate::outputs::mock::MockOutput;
    use std::sync::Arc;

    #[tokio::test]
    async fn local_stream_watch_reports_only_an_unconsumed_live_session_once() {
        use crate::http::streamer::{AudioStreamer, StreamInfo};

        let streamer = Arc::new(AudioStreamer::new(0));
        let info = StreamInfo {
            format: "wav".to_string(),
            mime_type: "audio/wav".to_string(),
            ..StreamInfo::default()
        };

        // No warning before the grace period, then one warning for a live
        // session whose HTTP body has never emitted a byte.
        let (unconsumed, _tx, _ready) = streamer.create_session(info.clone(), false, 1).await;
        let task = arm_local_stream_consumer_watch(
            streamer.clone(),
            unconsumed.clone(),
            7,
            "local:test".to_string(),
            std::time::Duration::from_millis(20),
        )
        .await
        .expect("first arm creates the watchdog");
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        assert!(!task.is_finished(), "the grace period must be respected");
        assert!(task.await.expect("watchdog task"));
        assert!(
            arm_local_stream_consumer_watch(
                streamer.clone(),
                unconsumed,
                7,
                "local:test".to_string(),
                std::time::Duration::ZERO,
            )
            .await
            .is_none(),
            "the same session must never report twice"
        );

        // A body that emitted at least one byte is consumed, even if no reader
        // happens to be active at the exact observation instant.
        let (consumed, _tx, _ready) = streamer.create_session(info.clone(), false, 1).await;
        {
            let sessions = streamer.sessions_state();
            let guard = sessions.lock().await;
            guard[&consumed]
                .bytes_sent
                .store(1, std::sync::atomic::Ordering::Relaxed);
        }
        let task = arm_local_stream_consumer_watch(
            streamer.clone(),
            consumed,
            7,
            "local:test".to_string(),
            std::time::Duration::ZERO,
        )
        .await
        .expect("consumed session is armed");
        assert!(!task.await.expect("watchdog task"));

        // A session removed by a normal stop/error path during the grace
        // period does not produce this diagnostic either.
        let (removed, _tx, _ready) = streamer.create_session(info, false, 1).await;
        let task = arm_local_stream_consumer_watch(
            streamer.clone(),
            removed.clone(),
            7,
            "local:test".to_string(),
            std::time::Duration::from_millis(20),
        )
        .await
        .expect("live session is armed");
        streamer.remove_session(&removed).await;
        assert!(!task.await.expect("watchdog task"));
    }

    /// Le débit WAV servi au renderer DLNA doit être « renderer-safe » : un
    /// flux HE-AAC/aacPlus décodé à 22050 Hz (Radio Morow) est rééchantillonné
    /// à 44100 Hz pour être audible ; un flux déjà en 44,1/48 kHz (ou plus)
    /// passe inchangé, sans rééchantillonnage inutile.
    #[test]
    fn renderer_safe_rate_upsamples_low_rates_only() {
        use super::renderer_safe_wav_rate;
        // HE-AAC core rate → upsample to 44100
        assert_eq!(renderer_safe_wav_rate(22050), 44100);
        // Other low/non-standard rates → 44100
        assert_eq!(renderer_safe_wav_rate(11025), 44100);
        assert_eq!(renderer_safe_wav_rate(16000), 44100);
        assert_eq!(renderer_safe_wav_rate(24000), 44100);
        assert_eq!(renderer_safe_wav_rate(32000), 44100);
        // Standard rates pass through unchanged (no needless resample)
        assert_eq!(renderer_safe_wav_rate(44100), 44100);
        assert_eq!(renderer_safe_wav_rate(48000), 48000);
        // Hi-res radio kept as-is
        assert_eq!(renderer_safe_wav_rate(88200), 88200);
        assert_eq!(renderer_safe_wav_rate(96000), 96000);
    }

    /// Issue #1960 — le cas qui motive tout : BBC Radio 3 a retiré son flux,
    /// l'ancienne adresse redirige vers la page d'accueil de la BBC et répond
    /// **200 OK** en `text/html`. Rien n'échoue, le lecteur reçoit du HTML, et
    /// l'auditeur n'a que du silence. Mesuré le 2026-08-20 :
    /// `curl -sSL http://stream.live.vc.bbcmedia.co.uk/bbc_radio_three`
    /// → `200 | text/html | https://www.bbc.co.uk/`.
    #[test]
    fn html_served_instead_of_audio_is_detected() {
        assert_eq!(
            non_audio_content_type("text/html"),
            Some("text/html".to_string())
        );
        // Le paramètre `charset` ne doit pas masquer le type — c'est la forme
        // que renvoie Icecast sur ses 404 (`text/html; charset=UTF-8`).
        assert_eq!(
            non_audio_content_type("text/html; charset=UTF-8"),
            Some("text/html".to_string())
        );
        // Casse et espaces : un en-tête HTTP n'est pas normalisé.
        assert_eq!(
            non_audio_content_type("  TEXT/HTML ;charset=utf-8"),
            Some("text/html".to_string())
        );
        assert_eq!(
            non_audio_content_type("application/json"),
            Some("application/json".to_string())
        );
        assert_eq!(
            non_audio_content_type("image/png"),
            Some("image/png".to_string())
        );
    }

    /// L'échec doit être DIT, et il doit survivre au client.
    ///
    /// Le client web étouffe un `zone.playback_error` reçu dans la fenêtre de
    /// grâce qui suit un ordre de lecture, sauf s'il porte `fatal: true`
    /// (App.svelte, `suppressedByPlayGrace`). Une station morte échoue en une
    /// fraction de seconde, donc EN PLEIN dans cette fenêtre : sans le drapeau,
    /// ce correctif afficherait « chargement… » et rien d'autre.
    #[tokio::test]
    async fn a_dead_station_is_reported_and_survives_the_grace_window() {
        let bus = Arc::new(EventBus::new());
        let mut rx = bus.subscribe();

        emit_radio_playback_error(
            &Some(bus.clone()),
            7,
            "BBC Radio 3",
            &format!(
                "{RADIO_NOT_AUDIO}: le serveur a répondu « text/html » au lieu d'un flux audio"
            ),
        );

        let ev = rx.recv().await.unwrap();
        assert_eq!(ev.event_type, "zone.playback_error");
        assert_eq!(ev.data["zone_id"], 7);
        assert_eq!(
            ev.data["fatal"], true,
            "sans fatal:true le client étouffe le message dans sa fenêtre de grâce"
        );
        let msg = ev.data["error"].as_str().unwrap();
        assert!(
            msg.contains("BBC Radio 3"),
            "le message doit nommer la station : {msg}"
        );
        assert!(
            msg.contains("page web"),
            "le message doit dire ce qui a été reçu à la place de l'audio : {msg}"
        );
    }

    /// Sans bus (tests, démarrage partiel) on ne panique pas, on se tait.
    #[test]
    fn no_event_bus_is_not_a_panic() {
        emit_radio_playback_error(&None, 1, "Station", "boom");
    }

    /// Le garde-fou ne doit RIEN casser : les types réellement servis par les
    /// stations que nous livrons doivent tous passer. Relevés le 2026-08-20 sur
    /// les 46 entrées de l'annuaire — `audio/aac` (Radio France),
    /// `audio/mpeg` (Radio Classique, TSF Jazz, KEXP) — plus les fantaisies
    /// classiques d'Icecast/Shoutcast, et le cas de l'en-tête absent.
    #[test]
    fn real_radio_content_types_pass_through() {
        for ct in [
            "audio/aac",
            "audio/mpeg",
            "audio/aacp",
            "audio/ogg",
            "audio/flac",
            "audio/x-flac",
            "application/ogg",
            "application/octet-stream",
            "audio/x-mpegurl",
            "application/vnd.apple.mpegurl",
            "video/mp2t",
            // En-tête absent ou vide : on ne sait pas, donc on laisse passer.
            "",
            "   ",
        ] {
            assert_eq!(
                non_audio_content_type(ct),
                None,
                "content-type « {ct} » refusé à tort — une station qui marche deviendrait muette"
            );
        }
    }

    /// Le rééchantillonnage 22050→44100 double bien le nombre de trames
    /// (ratio 2.0) et préserve l'entrelacement stéréo : la sortie doit avoir
    /// un nombre de trames pair et cohérent avec le ratio.
    #[test]
    fn radio_resample_doubles_frames_at_2x() {
        // 1024 stereo frames of test signal (interleaved f32)
        let in_frames = 1024usize;
        let src: Vec<f32> = (0..in_frames * 2).map(|i| (i as f32) * 0.001).collect();
        let out = crate::audio::simple_resample(&src, 22050, 44100, 2);
        // 22050 → 44100 is exactly 2x
        assert_eq!(out.len(), in_frames * 2 * 2);
        // Identity when rate unchanged (44100 → 44100)
        let same = crate::audio::simple_resample(&src, 44100, 44100, 2);
        assert_eq!(same, src);
    }

    /// La sonde de niveaux proxy (VU-mètres Qobuz/Tidal direct) décode un
    /// flux HTTP en fenêtres brutes : 1 s de WAV silencieux servie par un
    /// mini serveur one-shot doit produire ~25 fenêtres de 40 ms au format
    /// annoncé. Couvre le pipeline probe → décodage → fenêtrage, et la
    /// terminaison propre en fin de flux.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn levels_probe_decodes_http_stream_into_windows() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let sr: u32 = 44100;
                let data = vec![0u8; sr as usize * 4]; // 1 s, 16-bit stéréo
                let mut wav = Vec::with_capacity(44 + data.len());
                wav.extend_from_slice(b"RIFF");
                wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
                wav.extend_from_slice(b"WAVEfmt ");
                wav.extend_from_slice(&16u32.to_le_bytes());
                wav.extend_from_slice(&1u16.to_le_bytes());
                wav.extend_from_slice(&2u16.to_le_bytes());
                wav.extend_from_slice(&sr.to_le_bytes());
                wav.extend_from_slice(&(sr * 4).to_le_bytes());
                wav.extend_from_slice(&4u16.to_le_bytes());
                wav.extend_from_slice(&16u16.to_le_bytes());
                wav.extend_from_slice(b"data");
                wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
                wav.extend_from_slice(&data);
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    wav.len()
                );
                let _ = s.write_all(&wav);
            }
        });

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // Position rapportée très en avant : le bridage ne s'arme jamais.
        let reported = Arc::new(std::sync::atomic::AtomicI64::new(600_000));
        let url = format!("http://{addr}/probe.wav");
        tokio::task::spawn_blocking(move || {
            super::decode_http_stream_for_levels(url, "wav".into(), tx, reported)
                .expect("probe decodes the served WAV")
        })
        .await
        .expect("probe task join");

        let mut windows = 0;
        let mut total = std::time::Duration::ZERO;
        while let Ok(w) = rx.try_recv() {
            assert_eq!(w.sample_rate, 44100);
            assert_eq!(w.channels, 2);
            assert_eq!(w.bit_depth, 16);
            total += w.window;
            windows += 1;
        }
        assert!(windows >= 24, "1 s / 40 ms ≈ 25 fenêtres, reçu {windows}");
        let ms = total.as_millis();
        assert!(
            (950..=1050).contains(&ms),
            "durée totale ≈ 1 s, reçu {ms} ms"
        );
    }

    /// Sert `secs` secondes de WAV 16-bit stéréo 44,1 kHz silencieux sur un
    /// port éphémère (une seule connexion), et renvoie l'URL. Support de test
    /// pour la chaîne VU sans dépendre du réseau.
    fn spawn_oneshot_wav_server(secs: u32) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let sr: u32 = 44100;
                let data = vec![0u8; sr as usize * 4 * secs as usize];
                let mut wav = Vec::with_capacity(44 + data.len());
                wav.extend_from_slice(b"RIFF");
                wav.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
                wav.extend_from_slice(b"WAVEfmt ");
                wav.extend_from_slice(&16u32.to_le_bytes());
                wav.extend_from_slice(&1u16.to_le_bytes());
                wav.extend_from_slice(&2u16.to_le_bytes());
                wav.extend_from_slice(&sr.to_le_bytes());
                wav.extend_from_slice(&(sr * 4).to_le_bytes());
                wav.extend_from_slice(&4u16.to_le_bytes());
                wav.extend_from_slice(&16u16.to_le_bytes());
                wav.extend_from_slice(b"data");
                wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
                wav.extend_from_slice(&data);
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    wav.len()
                );
                let _ = s.write_all(&wav);
            }
        });
        format!("http://{addr}/probe.wav")
    }

    /// Régression #1247 : la chaîne VU complète d'une session proxy — sonde
    /// HTTP → forwarder cadencé → bus — doit émettre `playback.audio_levels`
    /// pour une zone en Playing dont la position n'est pas rapportée (0), le
    /// cas exact d'une zone « browser » servie en FLAC-proxy (Qobuz/Tidal
    /// direct). Autonome (serveur WAV local one-shot), sans réseau.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn levels_chain_emits_audio_levels_on_bus() {
        let url = spawn_oneshot_wav_server(3);
        let zone_id = 987_655;
        let playback = Arc::new(crate::playback::PlaybackManager::new());
        playback
            .play(zone_id, crate::playback::NowPlaying::default())
            .await;
        let bus = Arc::new(super::EventBus::new());
        let mut rx = bus.subscribe();

        let play_seq = playback.current_play_seq(zone_id).await;
        let levels_tx = super::spawn_paced_levels_forwarder(
            bus.clone(),
            playback.clone(),
            zone_id,
            play_seq,
            0,
        );
        let reported = Arc::new(std::sync::atomic::AtomicI64::new(0));
        let probe = tokio::task::spawn_blocking(move || {
            super::decode_http_stream_for_levels(url, "wav".into(), levels_tx, reported)
        });

        let mut n = 0u32;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(ev)) if ev.event_type == "playback.audio_levels" => n += 1,
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
        let _ = probe.await;
        assert!(
            n >= 40,
            "3 s d'audio ⇒ ~75 fenêtres de 40 ms sur le bus ; reçu {n}"
        );
    }

    /// #1110 : un forwarder créé pour une piste doit MOURIR quand la zone
    /// passe à la suivante, au lieu de publier son PCM sur l'horloge de la
    /// nouvelle. C'est ce que garantit l'épinglage de la génération au moment
    /// de la décision : ici on simule une génération devenue obsolète.
    #[tokio::test]
    async fn levels_forwarder_dies_when_its_track_is_replaced() {
        let zone_id = 987_656;
        let playback = Arc::new(crate::playback::PlaybackManager::new());
        playback
            .play(zone_id, crate::playback::NowPlaying::default())
            .await;
        let stale_seq = playback.current_play_seq(zone_id).await;

        // La zone enchaîne : nouvelle génération (ce que fait toute nouvelle
        // demande de lecture avant de résoudre son flux).
        playback.bump_generation(zone_id).await;
        playback
            .play(zone_id, crate::playback::NowPlaying::default())
            .await;
        assert_ne!(
            playback.current_play_seq(zone_id).await,
            stale_seq,
            "la lecture suivante doit bumper la génération"
        );

        let bus = Arc::new(super::EventBus::new());
        let mut rx = bus.subscribe();
        // Forwarder de l'ANCIENNE piste : c'est exactement ce qu'on obtenait en
        // lisant la génération trop tard, sauf qu'alors il lisait la NOUVELLE
        // et survivait.
        let levels_tx = super::spawn_paced_levels_forwarder(
            bus.clone(),
            playback.clone(),
            zone_id,
            stale_seq,
            0,
        );
        let pcm = vec![0u8; 4096];
        crate::audio::tap::send_windowed_pcm(&levels_tx, &pcm, 16, 2, 44_100);

        let got = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv()).await;
        assert!(
            got.is_err(),
            "un forwarder d'une piste remplacée ne doit rien publier, reçu {got:?}"
        );
    }

    /// Vérification end-to-end de la chaîne VU d'une session proxy, contre
    /// une URL FLAC/HTTP réelle : sonde → forwarder cadencé → bus. Reproduit
    /// exactement le chemin de production (moins le WebSocket) pour une zone
    /// « browser » (état Playing, position non rapportée = 0, comme quand le
    /// navigateur ne bat pas encore le cœur). Compte les événements
    /// `playback.audio_levels` réellement émis sur le bus.
    ///
    /// Piloté par TUNE_DIAG_PROBE_URL (URL d'une session proxy live, p.ex.
    /// http://192.168.1.18:8888/stream/<id>.flac) pour ne pas dépendre du
    /// réseau en CI.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn diag_probe_emits_bus_events() {
        let Ok(url) = std::env::var("TUNE_DIAG_PROBE_URL") else {
            return;
        };
        let zone_id = 987_654;
        let playback = Arc::new(crate::playback::PlaybackManager::new());
        // Passe la zone en Playing (comme un vrai play), sans rapporter de
        // position — le pire cas du forwarder (browser sans heartbeat).
        playback
            .play(zone_id, crate::playback::NowPlaying::default())
            .await;
        let bus = Arc::new(super::EventBus::new());
        let mut rx = bus.subscribe();

        let play_seq = playback.current_play_seq(zone_id).await;
        let levels_tx = super::spawn_paced_levels_forwarder(
            bus.clone(),
            playback.clone(),
            zone_id,
            play_seq,
            0,
        );
        let reported = Arc::new(std::sync::atomic::AtomicI64::new(0));
        let probe = tokio::task::spawn_blocking(move || {
            super::decode_http_stream_for_levels(url, "flac".into(), levels_tx, reported)
        });

        // Compte les audio_levels émis sur ~4 s (cadence réelle ≈ 25/s).
        let mut n = 0u32;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(4);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Ok(ev)) if ev.event_type == "playback.audio_levels" => n += 1,
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
        probe.abort();
        eprintln!("DIAG audio_levels emitted in 4s = {n}");
        assert!(
            n >= 50,
            "chaîne proxy→forwarder→bus doit émettre ~25/s ; reçu {n} en 4 s"
        );
    }

    /// Le repli NFC/NFD lui-même est éprouvé dans
    /// `crate::library::local_path` (#1865). Ce qui se teste ICI, c'est que la
    /// lecture continue de PASSER PAR LUI : la sortir du module l'a rendue
    /// partageable, elle ne doit pas s'en trouver débranchée.
    #[test]
    fn la_lecture_locale_passe_par_le_repli_partage() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Sur le disque en NFD (graphie d'un partage SMB / d'un Mac)…
        let nfd = tmp.path().join("Bjo\u{0308}rk - Jo\u{0301}ga.flac");
        std::fs::write(&nfd, b"x").unwrap();
        // …et en base en NFC, comme le scanner l'enregistre.
        let nfc = tmp
            .path()
            .join("Bj\u{00f6}rk - J\u{00f3}ga.flac")
            .to_string_lossy()
            .to_string();
        assert_ne!(nfc, nfd.to_string_lossy(), "les deux graphies different");
        assert!(
            super::resolve_existing_local_path(&nfc).is_some(),
            "resolve_existing_local_path doit venir de library::local_path et \
             retrouver le fichier NFD depuis le chemin NFC de la base"
        );
    }

    use tokio::sync::Mutex;

    use crate::db::migrations::run_migrations;
    use crate::db::sqlite::SqliteDb;
    use crate::db::zone_repo::ZoneRepo;
    use crate::http::streamer::AudioStreamer;
    use crate::outputs::registry::OutputRegistry;
    use crate::outputs::{OutputCapabilities, OutputCommand, OutputCommandError};
    use crate::playback::{NowPlaying, PlayState, PlaybackManager};
    use crate::streaming::registry::ServiceRegistry;

    use super::{
        PlayRequest, PlaybackOrchestrator, is_push_uri_output_type, passthrough_didl_duration_ms,
        pull_output_needs_dsp_transcode, use_file_transcode_for,
    };

    #[test]
    fn duplicate_net_play_gate_excludes_pull_outputs() {
        // #1129's coalescing exists for renderers that receive a URI and can
        // restart from byte 0 on a redundant send (Revox S100). Pull-based
        // outputs never have that failure mode and must stay excluded.
        assert!(is_push_uri_output_type(Some("dlna")));
        assert!(is_push_uri_output_type(Some("openhome")));
        assert!(is_push_uri_output_type(Some("chromecast")));
        assert!(is_push_uri_output_type(Some("bluos")));
        assert!(is_push_uri_output_type(Some("squeezebox")));
        assert!(is_push_uri_output_type(Some("slimproto")));

        // Pull-based: fetch/stream audio themselves, never restart-glitch.
        assert!(!is_push_uri_output_type(Some("local")));
        assert!(!is_push_uri_output_type(Some("oaat")));
        assert!(!is_push_uri_output_type(Some("diretta")));
    }

    #[test]
    fn pull_output_dsp_transcode_classification() {
        use crate::audio::formats::AudioFormat;
        let flac = Some(AudioFormat::Flac);

        // The case that was silently broken: an out-of-tree pull output was in
        // none of the lists, so an active EQ never reached it.
        assert!(pull_output_needs_dsp_transcode(
            Some("diretta"),
            false,
            false,
            flac
        ));

        // Already transcoding on their own: forcing again would be redundant.
        assert!(!pull_output_needs_dsp_transcode(
            Some("local"),
            true,
            false,
            flac
        ));
        assert!(!pull_output_needs_dsp_transcode(
            Some("oaat"),
            false,
            true,
            flac
        ));

        // Covered by their own flags.
        assert!(!pull_output_needs_dsp_transcode(
            Some("dlna"),
            false,
            false,
            flac
        ));
        assert!(!pull_output_needs_dsp_transcode(
            Some("browser"),
            false,
            false,
            flac
        ));

        // No format, or DSD: never force a transcode we cannot do safely, and
        // never turn native DSD into PCM on the listener's behalf.
        assert!(!pull_output_needs_dsp_transcode(
            Some("diretta"),
            false,
            false,
            None
        ));
        assert!(!pull_output_needs_dsp_transcode(
            Some("diretta"),
            false,
            false,
            Some(AudioFormat::Dsd)
        ));

        // Unknown output type: no basis to decide.
        assert!(!pull_output_needs_dsp_transcode(None, false, false, flac));

        // Unregistered device (output_type_of returned None): never coalesce.
        assert!(!is_push_uri_output_type(None));
    }

    #[test]
    fn dsd_lpcm_streams_only_when_toggled_and_dsd_wav() {
        // The fix: a DSD source served as WAV to a renderer that demands LPCM
        // (dlna_needs_wav) streams instead of blocking on a temp file — but
        // ONLY with the toggle on. Everything else keeps its prior behaviour.

        // DSD → WAV, renderer needs LPCM, toggle ON → stream (the fix).
        assert!(!use_file_transcode_for(true, true, true, true));
        // Same, toggle OFF → temp file (rollback, unchanged).
        assert!(use_file_transcode_for(true, true, true, false));
        // FLAC target (non-WAV) always temp-files for Content-Length — the
        // dsd flag can't apply (dsd_lpcm_streams stays false for non-DSD/WAV).
        assert!(use_file_transcode_for(true, false, false, false));
        // WAV target a renderer is fine to stream (dlna_needs_wav false):
        // streams regardless of the flag (local/OAAT/Linn path, unchanged).
        assert!(!use_file_transcode_for(true, true, false, false));
        // Local/OAAT (not network): never file-transcodes.
        assert!(!use_file_transcode_for(false, true, true, false));
    }

    #[test]
    fn duplicate_net_play_coalesces_same_track_within_window() {
        use std::collections::HashMap;
        use std::time::{Duration, Instant};
        type Map = HashMap<i64, (String, Option<String>, Option<i64>, Instant)>;
        let f = PlaybackOrchestrator::record_or_detect_duplicate_net_play;
        let t0 = Instant::now();
        let sid = Some("tidal-123".to_string());

        let mut map: Map = HashMap::new();
        // First play of the track → recorded, NOT a duplicate.
        assert!(!f(&mut map, 5, "tidal", &sid, None, t0));
        // Same (source, source_id) a few seconds later → duplicate (coalesce).
        assert!(f(
            &mut map,
            5,
            "tidal",
            &sid,
            None,
            t0 + Duration::from_secs(4)
        ));
        // A DIFFERENT track (real advance) → NOT a duplicate.
        let other = Some("tidal-999".to_string());
        assert!(!f(
            &mut map,
            5,
            "tidal",
            &other,
            None,
            t0 + Duration::from_secs(4)
        ));
        // Different source, same id → NOT a duplicate.
        assert!(!f(
            &mut map,
            5,
            "qobuz",
            &sid,
            None,
            t0 + Duration::from_secs(4)
        ));

        // Same track but OUTSIDE the window (repeat-one / dup-in-queue, minutes
        // later) → NOT a duplicate.
        let mut map2: Map = HashMap::new();
        assert!(!f(&mut map2, 7, "tidal", &sid, None, t0));
        let far = t0 + super::DUPLICATE_NET_PLAY_WINDOW + Duration::from_secs(1);
        assert!(!f(&mut map2, 7, "tidal", &sid, None, far));

        // Different zones never collide.
        let mut map3: Map = HashMap::new();
        assert!(!f(&mut map3, 1, "tidal", &sid, None, t0));
        assert!(!f(&mut map3, 2, "tidal", &sid, None, t0));
    }

    /// Deux pistes LOCALES differentes ne sont pas un doublon.
    ///
    /// La bibliotheque locale se joue par `track_id` : `play_from_queue` laisse
    /// `source` et `source_id` a `None`. La cle valait donc `("local", None)`
    /// pour toutes les pistes de la zone, et « piste suivante » sur un renderer
    /// reseau ne poussait plus rien pendant douze secondes — le serveur
    /// avancait, le Chromecast rejouait le meme morceau (FabienM, v0.9.102).
    ///
    /// Le test d'origine n'exercait que `tidal` et `qobuz`, qui portent
    /// TOUJOURS un `source_id` : il ne pouvait pas voir ce cas.
    #[test]
    fn deux_pistes_locales_differentes_ne_sont_pas_un_doublon() {
        use std::collections::HashMap;
        use std::time::{Duration, Instant};
        type Map = HashMap<i64, (String, Option<String>, Option<i64>, Instant)>;
        let f = PlaybackOrchestrator::record_or_detect_duplicate_net_play;
        let t0 = Instant::now();
        let mut map: Map = HashMap::new();

        // Piste 101 : premier envoi.
        assert!(!f(&mut map, 1, "local", &None, Some(101), t0));

        // « Piste suivante » deux secondes plus tard, piste 102 : c'est une
        // AUTRE piste, elle doit partir au renderer.
        assert!(
            !f(
                &mut map,
                1,
                "local",
                &None,
                Some(102),
                t0 + Duration::from_secs(2)
            ),
            "deux pistes locales differentes ne sont pas un doublon : le \
             renderer resterait sur le morceau precedent"
        );

        // Enchainement rapide : encore une autre.
        assert!(!f(
            &mut map,
            1,
            "local",
            &None,
            Some(103),
            t0 + Duration::from_secs(4)
        ));

        // La MEME piste relancee dans la fenetre reste un doublon : c'est la
        // course que le garde-fou existe pour absorber (#1146, Philippe Vella).
        assert!(f(
            &mut map,
            1,
            "local",
            &None,
            Some(103),
            t0 + Duration::from_secs(6)
        ));

        // Et hors fenetre, elle repart (repeat-one, doublon dans la file).
        let loin = t0 + Duration::from_secs(6) + super::DUPLICATE_NET_PLAY_WINDOW;
        assert!(!f(&mut map, 1, "local", &None, Some(103), loin));
    }

    #[test]
    fn retap_identity_matches_same_track_only() {
        // #1271 re-tap dedup identity predicate. Local library track: matches on
        // track_id when both sides have one.
        let np_local = NowPlaying {
            track_id: Some(42),
            source: "local".into(),
            ..Default::default()
        };
        let same_local = PlayRequest {
            track_id: Some(42),
            ..Default::default()
        };
        let other_local = PlayRequest {
            track_id: Some(43),
            ..Default::default()
        };
        assert!(PlaybackOrchestrator::is_same_track_retap(
            &np_local,
            &same_local
        ));
        assert!(!PlaybackOrchestrator::is_same_track_retap(
            &np_local,
            &other_local
        ));

        // Streaming track: matches on (source, source_id) when there is no
        // library track_id. A request that names the source must agree with it.
        let np_stream = NowPlaying {
            track_id: None,
            source: "tidal".into(),
            source_id: Some("tidal-123".into()),
            ..Default::default()
        };
        let same_stream = PlayRequest {
            source: Some("tidal".into()),
            source_id: Some("tidal-123".into()),
            ..Default::default()
        };
        // Web client omits `source` — still matches on the id alone.
        let same_stream_no_src = PlayRequest {
            source: None,
            source_id: Some("tidal-123".into()),
            ..Default::default()
        };
        let other_stream = PlayRequest {
            source: Some("tidal".into()),
            source_id: Some("tidal-999".into()),
            ..Default::default()
        };
        // Same id but a DIFFERENT source (Qobuz vs Tidal) → not the same track.
        let cross_source = PlayRequest {
            source: Some("qobuz".into()),
            source_id: Some("tidal-123".into()),
            ..Default::default()
        };
        assert!(PlaybackOrchestrator::is_same_track_retap(
            &np_stream,
            &same_stream
        ));
        assert!(PlaybackOrchestrator::is_same_track_retap(
            &np_stream,
            &same_stream_no_src
        ));
        assert!(!PlaybackOrchestrator::is_same_track_retap(
            &np_stream,
            &other_stream
        ));
        assert!(!PlaybackOrchestrator::is_same_track_retap(
            &np_stream,
            &cross_source
        ));

        // Neither side yields a positive id → never a match (no false coalesce).
        let np_bare = NowPlaying {
            track_id: None,
            source: "local".into(),
            source_id: None,
            ..Default::default()
        };
        let req_bare = PlayRequest {
            track_id: None,
            source_id: None,
            ..Default::default()
        };
        assert!(!PlaybackOrchestrator::is_same_track_retap(
            &np_bare, &req_bare
        ));
    }

    #[test]
    fn passthrough_duration_prefers_probed_over_scanned() {
        // #1132: the scanned duration (5:65 = 305_000 ms) is a few seconds too
        // long vs. the file's real STREAMINFO duration (300_000 ms). The DIDL
        // must advertise the real one so the gapless-queued track on the Marantz
        // ND 8006 ends at the true EOF instead of cutting/looping near the end.
        assert_eq!(
            passthrough_didl_duration_ms(Some(300.0), 305_000),
            300_000,
            "probed STREAMINFO duration wins over the too-long scanned value"
        );
    }

    #[test]
    fn passthrough_duration_falls_back_when_probe_missing() {
        // NAS read timeout / unreadable header → probe is None. We must keep the
        // scanned duration rather than blank it (a 0 duration hides the progress
        // bar on the renderer entirely).
        assert_eq!(passthrough_didl_duration_ms(None, 240_000), 240_000);
    }

    #[test]
    fn passthrough_duration_ignores_bogus_probe() {
        // Zero / negative / non-finite probed values must not overwrite a valid
        // scanned duration.
        assert_eq!(passthrough_didl_duration_ms(Some(0.0), 180_000), 180_000);
        assert_eq!(passthrough_didl_duration_ms(Some(-5.0), 180_000), 180_000);
        assert_eq!(
            passthrough_didl_duration_ms(Some(f64::NAN), 180_000),
            180_000
        );
    }

    /// Le garde-fou que JP Robbe a demande en revue de #2220 : une sortie OAAT
    /// REELLEMENT ENREGISTREE, drapeau DSD natif actif, doit rendre `false`.
    ///
    /// Mon premier test ne couvrait que les retours `true`. Une inversion de
    /// booleen, un mauvais prefixe ou un downcast rate y seraient passes
    /// inapercus : c'est ce test-ci qui verrouille le prefixe, le lookup, le
    /// downcast et l'inversion ENSEMBLE.
    #[cfg(feature = "oaat")]
    #[tokio::test]
    async fn une_sortie_oaat_en_dsd_natif_ne_mesure_pas() {
        let orch = test_orchestrator();
        let sortie = crate::outputs::oaat::OaatOutput::new(
            "Zicmu".into(),
            "192.168.1.99".into(),
            9000,
            "oaat:zicmu-test".into(),
        );
        // Le constructeur pose le prefixe `oaat:`, et c'est lui qui conditionne
        // le lookup dans `output_produces_levels`.
        let device_id = "oaat:zicmu-test".to_string();

        // En PCM la sortie mesure : ce sont les niveaux du decodage de
        // l'orchestrateur qui alimentent les VU.
        orch.outputs.lock().await.register(Box::new(sortie));
        assert!(
            orch.output_produces_levels(Some(&device_id)).await,
            "hors DSD natif, la chaine mesure"
        );

        // DSD natif : la sortie ouvre le .dsf elle-meme, plus personne ne
        // decode, donc plus aucune fenetre de niveaux.
        {
            let registre = orch.outputs.lock().await;
            let arc = registre.get(&device_id).expect("sortie enregistree");
            let sortie = arc.lock().await;
            let oaat = sortie
                .as_any()
                .downcast_ref::<crate::outputs::oaat::OaatOutput>()
                .expect("downcast vers OaatOutput");
            oaat.set_native_dsd_active_for_test(true);
        }
        assert!(
            !orch.output_produces_levels(Some(&device_id)).await,
            "en DSD natif rien ne mesure : l'ecran doit pouvoir le dire"
        );
    }

    /// Une zone sans sortie, ou une sortie qui n'est pas OAAT, mesure :
    /// `false` est réservé au seul chemin qui ne produit rien.
    ///
    /// Le cas DSD natif lui-même se teste là où vit le drapeau
    /// (`outputs::oaat::integration_test`) : il demande une sortie OAAT
    /// enregistrée, pas un orchestrateur nu.
    #[tokio::test]
    async fn sans_sortie_ou_hors_oaat_on_mesure() {
        let orch = test_orchestrator();
        assert!(orch.output_produces_levels(None).await);
        for did in [
            "local:Haut-parleurs",
            "dlna:uuid:1234",
            "airplay:salon",
            "oaat:zicmu", // enregistré nulle part : on ne conclut pas à l'absence
        ] {
            assert!(
                orch.output_produces_levels(Some(did)).await,
                "{did} : rien ne prouve que cette sortie ne mesure pas"
            );
        }
    }

    fn test_orchestrator() -> PlaybackOrchestrator {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        PlaybackOrchestrator::new(
            db,
            Arc::new(PlaybackManager::new()),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            Arc::new(Mutex::new(OutputRegistry::new())),
            None,
        )
    }

    // ------------------------------------------------------------------
    // #1541 — VU-mètres après une avance gapless, DSD local compris.
    // ------------------------------------------------------------------

    /// Écrit un `.dsf` DSD64 stéréo valide : `blocs_par_canal` super-blocs de
    /// 4096 octets par canal, remplis en carré (un bloc à `0xFF`, le suivant à
    /// `0x00`, soit ~43 Hz). Le signal est FRANC : le test peut exiger des
    /// niveaux au-dessus du silence, et pas seulement l'existence
    /// d'événements — un forwarder nourri de zéros émettrait tout autant.
    fn ecrire_dsf_carre(path: &std::path::Path, blocs_par_canal: usize) {
        const BLOC: usize = 4096;
        const CANAUX: usize = 2;
        let mut data = Vec::with_capacity(blocs_par_canal * BLOC * CANAUX);
        // Disposition DSF : bloc du canal 0, bloc du canal 1, bloc suivant du
        // canal 0… Les deux canaux portent le même carré.
        for indice_bloc in 0..blocs_par_canal * CANAUX {
            let octet: u8 = if (indice_bloc / CANAUX) % 2 == 0 {
                0xFF
            } else {
                0x00
            };
            data.extend(std::iter::repeat_n(octet, BLOC));
        }
        let total_samples = (blocs_par_canal * BLOC * 8) as u64;

        let mut buf = Vec::with_capacity(92 + data.len());
        buf.extend_from_slice(b"DSD ");
        buf.extend_from_slice(&28u64.to_le_bytes());
        buf.extend_from_slice(&(28 + 52 + 12 + data.len() as u64).to_le_bytes());
        buf.extend_from_slice(&0u64.to_le_bytes()); // pas de métadonnées
        buf.extend_from_slice(b"fmt ");
        buf.extend_from_slice(&52u64.to_le_bytes());
        buf.extend_from_slice(&1u32.to_le_bytes()); // version
        buf.extend_from_slice(&0u32.to_le_bytes()); // format = DSD brut
        buf.extend_from_slice(&2u32.to_le_bytes()); // type de canaux = stéréo
        buf.extend_from_slice(&(CANAUX as u32).to_le_bytes());
        buf.extend_from_slice(&2_822_400u32.to_le_bytes()); // DSD64
        buf.extend_from_slice(&1u32.to_le_bytes()); // bits par échantillon
        buf.extend_from_slice(&total_samples.to_le_bytes());
        buf.extend_from_slice(&(BLOC as u32).to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // réservé
        buf.extend_from_slice(b"data");
        buf.extend_from_slice(&(12 + data.len() as u64).to_le_bytes());
        buf.extend_from_slice(&data);
        std::fs::write(path, &buf).unwrap();
    }

    /// Zone à sortie LOCALE, en lecture, dont la file contient deux fois le
    /// même fichier : l'état exact d'un album au moment où l'enchaînement
    /// gapless bascule sur la piste 2.
    async fn zone_locale_prete_a_enchainer(
        chemin: &str,
        format: &str,
    ) -> (
        Arc<PlaybackOrchestrator>,
        Arc<EventBus>,
        i64,
        tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
    ) {
        let bus = Arc::new(EventBus::new());
        let mut orch = test_orchestrator();
        orch.event_bus = Some(bus.clone());
        let orch = Arc::new(orch);

        let zone_id = ZoneRepo::with_backend(orch.db.clone())
            .create("Smart DX1", Some("local"), Some("local:Smart DX1"))
            .unwrap();

        let pistes = crate::db::track_repo::TrackRepo::with_backend(orch.db.clone());
        let mut ids = Vec::new();
        for n in 1..=2 {
            let mut piste = crate::db::models::Track::new(format!("Piste {n}"));
            // `tracks.file_path` est UNIQUE : seule la piste 2 — celle sur
            // laquelle l'enchaînement bascule, donc la seule qui sera décodée
            // — porte le vrai fichier.
            piste.file_path = Some(if n == 2 {
                chemin.to_string()
            } else {
                format!("{chemin}.piste1")
            });
            piste.format = Some(format.to_string());
            piste.sample_rate = Some(2_822_400);
            piste.bit_depth = Some(1);
            piste.channels = 2;
            piste.track_number = n;
            piste.duration_ms = 2_000;
            ids.push(pistes.create(&piste).unwrap());
        }
        crate::db::play_queue_repo::PlayQueueRepo::with_backend(orch.db.clone())
            .set_queue(zone_id, &ids)
            .unwrap();

        // La zone joue déjà la piste 1 : sans état `Playing`, le forwarder
        // attend au lieu d'émettre et le test ne mesurerait que son horloge.
        orch.playback.play(zone_id, NowPlaying::default()).await;
        let rx = bus.subscribe();
        (orch, bus, zone_id, rx)
    }

    /// Compte les `playback.audio_levels` de `zone_id` pendant `fenetre`, et
    /// rend aussi la crête maximale vue. S'arrête dès que `attendus` sont
    /// atteints : un test qui réussit ne paie pas le délai complet.
    async fn compter_niveaux(
        rx: &mut tokio::sync::broadcast::Receiver<crate::event_bus::TuneEvent>,
        zone_id: i64,
        fenetre: std::time::Duration,
        attendus: u32,
    ) -> (u32, f64) {
        let mut n = 0u32;
        let mut crete = f64::NEG_INFINITY;
        let echeance = tokio::time::Instant::now() + fenetre;
        loop {
            let reste = echeance.saturating_duration_since(tokio::time::Instant::now());
            if reste.is_zero() || (attendus > 0 && n >= attendus) {
                break;
            }
            match tokio::time::timeout(reste, rx.recv()).await {
                Ok(Ok(ev))
                    if ev.event_type == "playback.audio_levels"
                        && ev.data.get("zone_id").and_then(|v| v.as_i64()) == Some(zone_id) =>
                {
                    n += 1;
                    if let Some(p) = ev.data.get("peak_left_db").and_then(|v| v.as_f64()) {
                        crete = crete.max(p);
                    }
                }
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
        (n, crete)
    }

    /// #1541 : après une avance gapless sur une piste **DSD locale**, la zone
    /// doit ré-émettre des `playback.audio_levels`.
    ///
    /// `bump_levels_gen` vient de tuer le forwarder de la piste précédente ;
    /// si rien ne le remplace, les aiguilles ne retombent pas à zéro — elles
    /// GÈLENT sur leur dernière valeur, ce que Xavier Joly décrit depuis la
    /// v0.9.98 (« l'aiguille bouge une fois au début puis reste bloquée »),
    /// pendant que le FLAC de la même zone continue de les animer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn avance_gapless_en_dsd_local_ranime_les_vu_metres() {
        let dsf = tempfile::Builder::new().suffix(".dsf").tempfile().unwrap();
        // ~1,5 s de DSD64 : de quoi produire une quarantaine de fenêtres de
        // 40 ms, cadencées à la vitesse de lecture par le forwarder.
        ecrire_dsf_carre(dsf.path(), 130);
        let chemin = dsf.path().to_str().unwrap().to_string();
        let (orch, _bus, zone_id, mut rx) = zone_locale_prete_a_enchainer(&chemin, "dsf").await;

        orch.advance_queue_metadata(zone_id, 1)
            .await
            .expect("l'avance gapless doit aboutir");

        let (n, crete) =
            compter_niveaux(&mut rx, zone_id, std::time::Duration::from_secs(20), 25).await;
        assert!(
            n >= 25,
            "après l'avance gapless, un DSD local doit ré-alimenter les VU : reçu {n} événements"
        );
        assert!(
            crete > -20.0,
            "les niveaux doivent décrire le SIGNAL, pas du silence : crête {crete:.1} dBFS"
        );
    }

    /// Contre-épreuve PERMANENTE du test ci-dessus : la décision d'avant le
    /// correctif — `file_path.filter(|_| !is_dsd)` — recopiée telle quelle,
    /// branchée sur le même harnais.
    ///
    /// Elle vérifie deux choses qu'un test vert ne prouve jamais tout seul :
    /// que l'injection de panne ÉCHOUE bien (aucun forwarder n'est créé), et
    /// que le compteur du harnais rend alors `0`. Si un jour des
    /// `audio_levels` arrivaient dans ce harnais par un autre chemin, le test
    /// principal deviendrait insensible au défaut : celui-ci tomberait en
    /// premier.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn contre_epreuve_le_filtre_dsd_historique_eteint_bien_les_vu() {
        let dsf = tempfile::Builder::new().suffix(".dsf").tempfile().unwrap();
        ecrire_dsf_carre(dsf.path(), 130);
        let chemin = dsf.path().to_str().unwrap().to_string();
        let (orch, bus, zone_id, mut rx) = zone_locale_prete_a_enchainer(&chemin, "dsf").await;

        // Décision historique, verbatim.
        let decision_historique = |format: Option<&str>, file_path: Option<String>| {
            let is_dsd = format.is_some_and(|f: &str| {
                matches!(f.to_ascii_lowercase().as_str(), "dsf" | "dff" | "dsd")
            });
            file_path.filter(|_| !is_dsd)
        };

        // Le reste de l'avance, à l'identique : les forwarders de la piste
        // précédente meurent, puis on ne crée QUE ce que la décision autorise.
        orch.playback.bump_levels_gen(zone_id);
        let play_seq = orch.playback.current_play_seq(zone_id).await;
        let choisi = decision_historique(Some("dsf"), Some(chemin.clone()));
        assert!(
            choisi.is_none(),
            "l'injection de panne doit bien priver le DSD de forwarder"
        );
        if let Some(p) = choisi {
            super::spawn_local_file_levels_decode(bus, orch.playback.clone(), zone_id, play_seq, p);
        }

        let (n, _) = compter_niveaux(&mut rx, zone_id, std::time::Duration::from_secs(3), 0).await;
        assert_eq!(
            n, 0,
            "sous le défaut, ce harnais doit voir ZÉRO niveau — sinon le test principal ne prouve rien"
        );
    }

    /// La décision elle-même, cas par cas. Le DSD n'est plus exclu ; la seule
    /// sortie qui ne mesure pas (OAAT en DSD natif) ne paie toujours pas le
    /// décodage, et aucun autre format ne dépend de cette réponse.
    #[test]
    fn le_fichier_a_mesurer_couvre_le_dsd_sauf_quand_rien_ne_mesure() {
        let f = || Some("/musique/piste".to_string());
        for fmt in ["dsf", "dff", "dsd", "DSF", "Dff"] {
            assert_eq!(
                super::fichier_a_mesurer_apres_avance(Some(fmt), f(), true),
                f(),
                "{fmt} : une sortie qui mesure doit recevoir des niveaux"
            );
            assert_eq!(
                super::fichier_a_mesurer_apres_avance(Some(fmt), f(), false),
                None,
                "{fmt} : rendre du 1 bit en PCM pour une sortie qui ne mesure pas"
            );
        }
        for fmt in ["flac", "wav", "mp3", "alac"] {
            assert_eq!(
                super::fichier_a_mesurer_apres_avance(Some(fmt), f(), true),
                f(),
                "{fmt} : comportement inchangé"
            );
            assert_eq!(
                super::fichier_a_mesurer_apres_avance(Some(fmt), f(), false),
                f(),
                "{fmt} : la réponse de la sortie ne concerne que le DSD"
            );
        }
        assert_eq!(
            super::fichier_a_mesurer_apres_avance(Some("flac"), None, true),
            None,
            "sans chemin de fichier, rien à décoder"
        );
        assert_eq!(
            super::fichier_a_mesurer_apres_avance(None, f(), true),
            f(),
            "format inconnu : on décode, comme avant"
        );
    }

    /// Le bridage du décodage-pour-niveaux : au plus 30 s d'avance sur ce que
    /// la zone rapporte. Sans lui, le décodeur d'un DSD local part plein pot
    /// et la file du forwarder — non bornée — retient tout le PCM de la piste.
    #[test]
    fn le_decodage_pour_niveaux_ne_prend_pas_plus_de_30_s_d_avance() {
        assert!(!super::levels_decode_doit_freiner(0, 0));
        assert!(!super::levels_decode_doit_freiner(30_000, 0));
        assert!(super::levels_decode_doit_freiner(30_001, 0));
        // La lecture avance : le décodage repart d'autant.
        assert!(!super::levels_decode_doit_freiner(90_000, 60_000));
        assert!(super::levels_decode_doit_freiner(90_001, 60_000));
    }

    /// #1985 : persister un nouvel égaliseur sans sortie locale vivante rend
    /// `applied_live=false`, mais ce n'est pas une raison pour laisser le
    /// client afficher l'ancien chemin du signal. `zone.updated` lui ordonne
    /// de relire la zone, donc de reconstruire `signal_path` avec le profil qui
    /// vient d'être écrit.
    #[tokio::test]
    async fn eq_change_announces_a_fresh_signal_path_without_live_output() {
        let bus = Arc::new(EventBus::new());
        let mut rx = bus.subscribe();
        let mut orch = test_orchestrator();
        orch.event_bus = Some(bus);
        let orch = Arc::new(orch);

        assert!(
            !orch.apply_eq_change(1_985).await,
            "sans sortie locale vivante, le réglage ne peut pas être appliqué à chaud"
        );

        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("le client ne doit pas conserver un signal_path périmé")
            .expect("le bus doit rester ouvert");
        assert_eq!(event.event_type, "zone.updated");
        assert_eq!(event.data, serde_json::json!({ "zone_id": 1_985 }));
    }

    /// Zone locale gréée pour les tests de bascule PURE : une `LocalOutput`
    /// enregistrée, un format déclaré (sinon les rafraîchisseurs renoncent), et
    /// un profil d'égaliseur audible en base.
    #[cfg(feature = "local-audio")]
    async fn zone_locale_avec_eq(orch: &PlaybackOrchestrator) -> i64 {
        let zone_id = ZoneRepo::with_backend(orch.db.clone())
            .create("Salon", Some("local"), Some("local:DAC"))
            .unwrap();
        orch.outputs
            .lock()
            .await
            .register(Box::new(crate::outputs::local::LocalOutput::new(
                "DAC".to_string(),
            )));

        let profil = crate::audio::eq::EqProfile {
            enabled: true,
            bands: vec![crate::audio::eq::EqBandSpec {
                freq: 80.0,
                gain: 8.0,
                q: 0.71,
                band_type: "low_shelf".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone())
            .set(
                &format!("zone_{zone_id}_eq_profile"),
                &serde_json::to_string(&profil).unwrap(),
            )
            .unwrap();
        zone_id
    }

    #[cfg(feature = "local-audio")]
    async fn avec_sortie_locale<T>(
        orch: &PlaybackOrchestrator,
        f: impl FnOnce(&crate::outputs::local::LocalOutput) -> T,
    ) -> T {
        let arc = orch.outputs.lock().await.get("local:DAC").unwrap();
        let sortie = arc.lock().await;
        let local = sortie
            .as_any()
            .downcast_ref::<crate::outputs::local::LocalOutput>()
            .expect("sortie locale");
        f(local)
    }

    #[cfg(feature = "local-audio")]
    fn regler_pure(orch: &PlaybackOrchestrator, zone_id: i64, actif: bool) {
        crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone())
            .set(
                &format!("zone_{zone_id}_audiophile"),
                &format!(r#"{{"enabled":{actif}}}"#),
            )
            .unwrap();
    }

    /// #2102 : retirer l'EQ d'une sortie locale vivante est une application à
    /// chaud réussie, pas un échec qui autorise le redémarrage audible réservé
    /// aux sorties réseau. L'état Playing est indispensable à la
    /// contre-épreuve : c'est lui qui faisait armer `eq_replay_gen` auparavant.
    #[cfg(feature = "local-audio")]
    #[tokio::test]
    async fn removing_local_eq_does_not_schedule_a_stream_replay() {
        let orch = Arc::new(test_orchestrator());
        let zone_id = zone_locale_avec_eq(&orch).await;

        avec_sortie_locale(&orch, |local| {
            local.declare_current_format_for_test(44_100, 2);
            local.set_eq(orch.load_eq_processor(zone_id, 44_100, 2));
            assert!(local.has_eq(), "le test doit commencer avec un EQ monté");
        })
        .await;
        orch.playback
            .play(zone_id, crate::playback::NowPlaying::default())
            .await;

        regler_pure(&orch, zone_id, true);
        assert!(
            orch.apply_eq_change(zone_id).await,
            "replace_eq_live(None) a bien servi la sortie locale immédiatement"
        );

        avec_sortie_locale(&orch, |local| {
            assert!(
                !local.has_eq(),
                "la bascule PURE doit retirer l'EqProcessor du flux vivant"
            );
        })
        .await;
        assert!(
            !orch.eq_replay_gen.lock().unwrap().contains_key(&zone_id),
            "une sortie locale déjà servie ne doit jamais armer une relecture de flux"
        );
    }

    /// Le signalement de Jean Valjean (#1986), rejoué : Bass Boost audible,
    /// bascule en PURE **pendant** la lecture. Avant, la clé était écrite et la
    /// sortie n'apprenait rien — l'`EqProcessor` restait monté et `pure_bypass`
    /// à faux, donc chaque échantillon continuait d'être filtré pendant que le
    /// badge PURE s'allumait.
    #[cfg(feature = "local-audio")]
    #[tokio::test]
    async fn switching_to_pure_mid_track_stops_the_eq_at_once() {
        let orch = test_orchestrator();
        let zone_id = zone_locale_avec_eq(&orch).await;

        // Ce que fait le chemin de lecture au démarrage d'une piste hors PURE.
        avec_sortie_locale(&orch, |local| {
            local.declare_current_format_for_test(44_100, 2);
            local.set_eq(orch.load_eq_processor(zone_id, 44_100, 2));
            local.set_pure_bypass(false);
            local.set_replaygain_factor(0.5);
        })
        .await;
        avec_sortie_locale(&orch, |local| {
            assert!(local.has_eq(), "l'égaliseur doit être monté au départ");
        })
        .await;

        regler_pure(&orch, zone_id, true);
        assert!(
            orch.refresh_zone_pure_dsp(zone_id).await,
            "une sortie locale vivante doit recevoir le nouvel état"
        );

        avec_sortie_locale(&orch, |local| {
            assert!(
                !local.has_eq(),
                "PURE promet un chemin intouché : l'EqProcessor doit être retiré"
            );
            assert!(
                local.pure_bypass_for_test(),
                "le drapeau que lit apply_local_dsp doit être armé"
            );
            // Le ReplayGain n'est PAS couvert par ce drapeau — il multiplie les
            // échantillons dans les callbacks de rendu. Sans sa remise à
            // l'unité, PURE laisserait un gain en place.
            assert_eq!(local.replaygain_units_for_test(), 1000);
        })
        .await;
    }

    /// Point 3 du même signalement : « je devrais revenir au réglage
    /// précédent ». Sortir de PURE doit remonter l'égaliseur choisi par
    /// l'utilisateur, sans attendre la piste suivante.
    #[cfg(feature = "local-audio")]
    #[tokio::test]
    async fn leaving_pure_mid_track_brings_the_eq_back() {
        let orch = test_orchestrator();
        let zone_id = zone_locale_avec_eq(&orch).await;
        regler_pure(&orch, zone_id, true);

        avec_sortie_locale(&orch, |local| {
            local.declare_current_format_for_test(44_100, 2);
            local.set_eq(None);
            local.set_pure_bypass(true);
        })
        .await;

        regler_pure(&orch, zone_id, false);
        assert!(orch.refresh_zone_pure_dsp(zone_id).await);

        avec_sortie_locale(&orch, |local| {
            assert!(
                local.has_eq(),
                "hors PURE, le profil activé de la zone doit revenir tout de suite"
            );
            assert!(!local.pure_bypass_for_test());
        })
        .await;
    }

    /// Sans flux en cours, on ne rafraîchit rien : bâtir des biquads pour un
    /// format inconnu donnerait des coefficients faux, et la prochaine lecture
    /// appliquera l'état complet de toute façon. Le `false` rendu est ce qui
    /// distingue « rien reçu » de « reçu, et vide ».
    #[cfg(feature = "local-audio")]
    #[tokio::test]
    async fn nothing_playing_means_nothing_to_refresh() {
        let orch = test_orchestrator();
        let zone_id = zone_locale_avec_eq(&orch).await;
        regler_pure(&orch, zone_id, true);
        // `declare_current_format_for_test` volontairement non appelé.
        assert!(!orch.refresh_zone_pure_dsp(zone_id).await);
    }

    /// Une zone réseau n'a pas de sortie locale à rafraîchir : le traitement est
    /// gravé dans le fichier transcodé. `refresh_zone_pure_dsp` doit rendre
    /// `false` pour que `apply_audiophile_change` bascule sur le redémarrage.
    #[tokio::test]
    async fn a_network_zone_has_no_live_local_output() {
        let orch = test_orchestrator();
        let zone_id = ZoneRepo::with_backend(orch.db.clone())
            .create("Ampli", Some("dlna"), Some("dlna:uuid-42"))
            .unwrap();
        assert!(!orch.refresh_zone_pure_dsp(zone_id).await);
    }

    /// Régression #1629 — reprendre une webradio dont le PRODUCTEUR de
    /// décodage est mort (connexion icecast tombée pendant la pause, chemin de
    /// sortie sans log) doit déclencher un RE-PLAY de la station — un nouveau
    /// `play_media` vers la sortie, comme au premier lancement — et non une
    /// reprise « sur place » qui rend du silence.
    #[tokio::test]
    async fn resuming_a_radio_with_a_dead_producer_replays_the_station() {
        let orch = test_orchestrator();
        let zone_id = ZoneRepo::with_backend(orch.db.clone())
            .create("Zone Radio", Some("mock"), Some("mock-radio"))
            .unwrap();
        orch.outputs
            .lock()
            .await
            .register(Box::new(MockOutput::new("mock-radio", "Mock Radio")));

        // Session radio dont le producteur s'est terminé (comme après
        // `radio_reconnect_giving_up` ou un `consumer_dropped` silencieux).
        let (sid, _tx, _ready, session) = orch
            .streamer
            .create_radio_session(
                crate::http::streamer::StreamInfo {
                    format: "wav".into(),
                    mime_type: "audio/wav".into(),
                    sample_rate: 48000,
                    bit_depth: 16,
                    channels: 2,
                    ..Default::default()
                },
                8,
            )
            .await;
        session
            .producer_done
            .store(true, std::sync::atomic::Ordering::Relaxed);

        // La zone joue cette radio, puis est mise en pause (pause COURTE :
        // c'est bien la mort du producteur qui doit déclencher le re-play).
        orch.playback
            .play(
                zone_id,
                NowPlaying {
                    title: "FIP".into(),
                    source: "radio".into(),
                    source_id: Some("http://icecast.example/fip.aac".into()),
                    stream_id: Some(sid),
                    ..Default::default()
                },
            )
            .await;
        orch.playback.pause(zone_id).await;

        orch.resume(zone_id, Some("mock-radio")).await.unwrap();

        let outputs = orch.outputs.lock().await;
        let out = outputs.get("mock-radio").unwrap();
        let guard = out.lock().await;
        let mock = guard
            .as_any()
            .downcast_ref::<MockOutput>()
            .expect("mock output");
        assert_eq!(
            mock.play_call_count().await,
            1,
            "producteur mort ⇒ la reprise doit rejouer la station (nouveau play_media)"
        );
        // Et la zone doit repartir en lecture avec une NOUVELLE session de flux.
        let state = orch.playback.get_state(zone_id).await;
        let np = state.now_playing.expect("now_playing après re-play");
        assert_eq!(np.source, "radio");
    }

    /// Contre-épreuve #1629 — pause courte ET producteur vivant : la reprise
    /// reste une reprise sur place (aucun nouveau `play_media`), le
    /// comportement d'aujourd'hui qui fonctionne.
    #[tokio::test]
    async fn resuming_a_radio_with_a_live_producer_after_a_short_pause_does_not_replay() {
        let orch = test_orchestrator();
        let zone_id = ZoneRepo::with_backend(orch.db.clone())
            .create("Zone Radio", Some("mock"), Some("mock-radio"))
            .unwrap();
        orch.outputs
            .lock()
            .await
            .register(Box::new(MockOutput::new("mock-radio", "Mock Radio")));

        // Producteur VIVANT : le tx du décodeur est encore détenu (par le
        // test) et `producer_done` reste false.
        let (sid, _tx, _ready, _session) = orch
            .streamer
            .create_radio_session(
                crate::http::streamer::StreamInfo {
                    format: "wav".into(),
                    mime_type: "audio/wav".into(),
                    sample_rate: 48000,
                    bit_depth: 16,
                    channels: 2,
                    ..Default::default()
                },
                8,
            )
            .await;

        orch.playback
            .play(
                zone_id,
                NowPlaying {
                    title: "FIP".into(),
                    source: "radio".into(),
                    source_id: Some("http://icecast.example/fip.aac".into()),
                    stream_id: Some(sid),
                    ..Default::default()
                },
            )
            .await;
        orch.playback.pause(zone_id).await;

        orch.resume(zone_id, Some("mock-radio")).await.unwrap();

        let outputs = orch.outputs.lock().await;
        let out = outputs.get("mock-radio").unwrap();
        let guard = out.lock().await;
        let mock = guard
            .as_any()
            .downcast_ref::<MockOutput>()
            .expect("mock output");
        assert_eq!(
            mock.play_call_count().await,
            0,
            "pause courte + producteur vivant ⇒ reprise sur place, pas de re-play"
        );
        assert_eq!(
            orch.playback.get_state(zone_id).await.state,
            PlayState::Playing,
            "la zone doit être repassée en lecture"
        );
    }

    /// Crée une zone offline pointant vers un device réseau disparu, comme la
    /// « Mac Studio Speakers » d'Alex Campbell (#1287) : la zone avait été créée
    /// quand un second serveur voyait le Mac sur le réseau ; ce device n'existe
    /// plus dans le registre du serveur courant.
    fn stale_network_zone(orch: &PlaybackOrchestrator, name: &str) -> i64 {
        let repo = ZoneRepo::with_backend(orch.db.clone());
        let id = repo
            .create(name, Some("dlna"), Some("dlna-vanished-host"))
            .unwrap();
        repo.update_online(id, false).unwrap();
        id
    }

    #[tokio::test]
    async fn stale_network_zone_rebinds_to_the_local_output_of_the_same_name() {
        let orch = test_orchestrator();
        let zone_id = stale_network_zone(&orch, "Mac Studio Speakers");
        orch.outputs.lock().await.register(Box::new(
            MockOutput::new("local:mac-studio-speakers", "Mac Studio Speakers").with_type("local"),
        ));

        let zone = ZoneRepo::with_backend(orch.db.clone())
            .get(zone_id)
            .unwrap()
            .unwrap();
        let rebound = orch
            .gate_or_rebind_offline_zone(zone_id, &zone)
            .await
            .expect("le rebind doit réussir, pas rejeter la lecture");
        assert_eq!(rebound.as_deref(), Some("local:mac-studio-speakers"));

        // Le rebind est persisté et collant : id, type ET online.
        let after = ZoneRepo::with_backend(orch.db.clone())
            .get(zone_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            after.output_device_id.as_deref(),
            Some("local:mac-studio-speakers")
        );
        assert_eq!(
            after.output_type.as_deref(),
            Some("local"),
            "le type doit suivre l'id, sinon la zone reste typée dlna en pointant du local"
        );
        assert!(after.online);
    }

    #[tokio::test]
    async fn two_outputs_of_the_same_name_are_ambiguous_and_never_auto_bound() {
        let orch = test_orchestrator();
        let zone_id = stale_network_zone(&orch, "Salon");
        {
            let mut reg = orch.outputs.lock().await;
            reg.register(Box::new(
                MockOutput::new("dlna-a", "Salon").with_type("dlna"),
            ));
            reg.register(Box::new(
                MockOutput::new("dlna-b", "Salon").with_type("dlna"),
            ));
        }

        let zone = ZoneRepo::with_backend(orch.db.clone())
            .get(zone_id)
            .unwrap()
            .unwrap();
        let err = orch
            .gate_or_rebind_offline_zone(zone_id, &zone)
            .await
            .expect_err("deux homonymes sans local : binder l'un des deux serait un pari");
        assert!(err.starts_with("zone_output_unavailable:"), "err = {err}");

        // Rien n'a été touché en base.
        let after = ZoneRepo::with_backend(orch.db.clone())
            .get(zone_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            after.output_device_id.as_deref(),
            Some("dlna-vanished-host")
        );
        assert!(!after.online);
    }

    #[tokio::test]
    async fn a_single_local_output_wins_over_other_same_name_candidates() {
        let orch = test_orchestrator();
        let zone_id = stale_network_zone(&orch, "Salon");
        {
            let mut reg = orch.outputs.lock().await;
            reg.register(Box::new(
                MockOutput::new("dlna-a", "Salon").with_type("dlna"),
            ));
            reg.register(Box::new(
                MockOutput::new("local:salon", "Salon").with_type("local"),
            ));
        }

        let zone = ZoneRepo::with_backend(orch.db.clone())
            .get(zone_id)
            .unwrap()
            .unwrap();
        let rebound = orch
            .gate_or_rebind_offline_zone(zone_id, &zone)
            .await
            .unwrap();
        assert_eq!(
            rebound.as_deref(),
            Some("local:salon"),
            "un local unique tranche l'ambiguïté — c'est la règle « préférer local »"
        );
    }

    #[tokio::test]
    async fn no_matching_output_gives_an_actionable_error_not_a_curt_offline() {
        let orch = test_orchestrator();
        let zone_id = stale_network_zone(&orch, "Chambre");
        orch.outputs.lock().await.register(Box::new(
            MockOutput::new("local:autre-chose", "Cuisine").with_type("local"),
        ));

        let zone = ZoneRepo::with_backend(orch.db.clone())
            .get(zone_id)
            .unwrap()
            .unwrap();
        let err = orch
            .gate_or_rebind_offline_zone(zone_id, &zone)
            .await
            .expect_err("aucune sortie du même nom");
        assert!(err.starts_with("zone_output_unavailable:"), "err = {err}");
        assert!(
            err.contains("réglages de la zone"),
            "le message doit dire quoi faire, pas juste « offline » : {err}"
        );
    }

    #[tokio::test]
    async fn a_healthy_zone_is_left_completely_alone() {
        let orch = test_orchestrator();
        let repo = ZoneRepo::with_backend(orch.db.clone());
        let zone_id = repo
            .create("Bureau", Some("local"), Some("local:bureau"))
            .unwrap();
        repo.update_online(zone_id, true).unwrap();
        orch.outputs.lock().await.register(Box::new(
            MockOutput::new("local:bureau", "Bureau").with_type("local"),
        ));

        let zone = repo.get(zone_id).unwrap().unwrap();
        assert_eq!(
            orch.gate_or_rebind_offline_zone(zone_id, &zone)
                .await
                .unwrap(),
            None,
            "chemin nominal : aucun rebind, aucune écriture"
        );
    }

    /// Une zone offline dont le device est TOUJOURS dans le registre vivant ne
    /// doit pas être re-bindée : c'est la fenêtre de grâce pour les trous de
    /// polling SSDP, le device est joignable même si la DB dit offline.
    #[tokio::test]
    async fn an_offline_zone_whose_device_is_still_registered_is_not_touched() {
        let orch = test_orchestrator();
        let repo = ZoneRepo::with_backend(orch.db.clone());
        let zone_id = repo
            .create("Salon", Some("dlna"), Some("dlna-toujours-la"))
            .unwrap();
        repo.update_online(zone_id, false).unwrap();
        {
            let mut reg = orch.outputs.lock().await;
            reg.register(Box::new(
                MockOutput::new("dlna-toujours-la", "Salon").with_type("dlna"),
            ));
            // Un homonyme local qui aurait été choisi si on avait re-bindé.
            reg.register(Box::new(
                MockOutput::new("local:salon", "Salon").with_type("local"),
            ));
        }

        let zone = repo.get(zone_id).unwrap().unwrap();
        assert_eq!(
            orch.gate_or_rebind_offline_zone(zone_id, &zone)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            repo.get(zone_id)
                .unwrap()
                .unwrap()
                .output_device_id
                .as_deref(),
            Some("dlna-toujours-la")
        );
    }

    #[test]
    fn timeout_means_the_command_may_have_landed() {
        let err = format!(
            "{} soap send: error sending request for url (http://192.168.1.92:8080/AVTransport/ctrl): operation timed out",
            crate::outputs::dlna::SOAP_TIMEOUT_PREFIX
        );
        assert!(super::command_may_have_landed(&err));
    }

    #[test]
    fn connection_refused_is_conclusive() {
        // Refus de connexion : rien n'a pu partir, la session doit être détruite.
        assert!(!super::command_may_have_landed(
            "soap send: error sending request: connection refused"
        ));
        assert!(!super::command_may_have_landed("soap read: body error"));
        assert!(!super::command_may_have_landed(""));
    }

    #[test]
    fn timeout_marker_survives_the_send_to_output_wrapper() {
        // send_to_output enveloppe : « Output device error: {e} ». Le marqueur
        // n'est donc pas en tête de chaîne.
        let err = format!(
            "Output device error: {} soap send: operation timed out",
            crate::outputs::dlna::SOAP_TIMEOUT_PREFIX
        );
        assert!(
            super::command_may_have_landed(&err),
            "le marqueur doit être reconnu même enveloppé"
        );
    }

    /// Sortie dont `play_media` expire — le renderer lent qui reçoit peut-être la
    /// commande, mais dont la réponse n'arrive pas (Cyrus Stream X2 de JP).
    struct TimingOutOutput {
        id: String,
    }

    struct FailingCommandOutput {
        id: String,
    }

    #[async_trait::async_trait]
    impl crate::outputs::traits::OutputTarget for FailingCommandOutput {
        fn name(&self) -> &str {
            "FailingCommand"
        }
        fn device_id(&self) -> &str {
            &self.id
        }
        fn output_type(&self) -> &str {
            "test"
        }
        fn capabilities(&self) -> OutputCapabilities {
            OutputCapabilities::v1(true, true, true, true, true, false)
        }
        async fn play_media(
            &self,
            _media: &crate::outputs::traits::PlayMedia<'_>,
        ) -> Result<(), String> {
            Ok(())
        }
        async fn pause(&self) -> Result<(), String> {
            Err("pause refused".into())
        }
        async fn resume(&self) -> Result<(), String> {
            Err("resume refused".into())
        }
        async fn stop(&self) -> Result<(), String> {
            Ok(())
        }
        async fn seek(&self, _position_ms: u64) -> Result<(), String> {
            Err("seek refused".into())
        }
        async fn set_volume(&self, _volume: f64) -> Result<(), String> {
            Err("volume refused".into())
        }
        async fn set_mute(&self, _muted: bool) -> Result<(), String> {
            Err("mute refused".into())
        }
        async fn get_status(&self) -> Result<crate::outputs::traits::OutputStatus, String> {
            Ok(Default::default())
        }
        async fn is_available(&self) -> bool {
            true
        }
    }

    #[async_trait::async_trait]
    impl crate::outputs::traits::OutputTarget for TimingOutOutput {
        fn name(&self) -> &str {
            "TimingOut"
        }
        fn device_id(&self) -> &str {
            &self.id
        }
        fn output_type(&self) -> &str {
            "test"
        }
        async fn play_media(
            &self,
            _media: &crate::outputs::traits::PlayMedia<'_>,
        ) -> Result<(), String> {
            Err(format!(
                "{} soap send: error sending request for url \
                 (http://192.168.1.92:8080/AVTransport/ctrl): operation timed out",
                crate::outputs::dlna::SOAP_TIMEOUT_PREFIX
            ))
        }
        async fn pause(&self) -> Result<(), String> {
            Ok(())
        }
        async fn resume(&self) -> Result<(), String> {
            Ok(())
        }
        async fn stop(&self) -> Result<(), String> {
            Ok(())
        }
        async fn seek(&self, _pos_ms: u64) -> Result<(), String> {
            Ok(())
        }
        async fn set_volume(&self, _vol: f64) -> Result<(), String> {
            Ok(())
        }
        async fn get_status(&self) -> Result<crate::outputs::traits::OutputStatus, String> {
            Ok(Default::default())
        }
        async fn set_mute(&self, _muted: bool) -> Result<(), String> {
            Ok(())
        }
        async fn is_available(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn une_capacite_absente_ne_modifie_ni_memoire_ni_base() {
        let orch = test_orchestrator();
        let device_id = "legacy-noop";
        let zone_repo = ZoneRepo::with_backend(orch.db.clone());
        let zone_id = zone_repo
            .create("Legacy", Some("test"), Some(device_id))
            .unwrap();
        orch.outputs
            .lock()
            .await
            .register(Box::new(TimingOutOutput {
                id: device_id.into(),
            }));

        orch.playback
            .play(
                zone_id,
                NowPlaying {
                    track_id: Some(42),
                    title: "Contrat fail-closed".into(),
                    duration_ms: 120_000,
                    source: "local".into(),
                    ..Default::default()
                },
            )
            .await;
        orch.playback.update_position(zone_id, 12_000).await;
        orch.playback.set_volume(zone_id, 0.5).await;
        orch.playback.set_mute(zone_id, false).await;

        for (result, command) in [
            (
                orch.pause(zone_id, Some(device_id)).await,
                OutputCommand::Pause,
            ),
            (
                orch.seek(zone_id, 42_000, Some(device_id)).await,
                OutputCommand::Seek,
            ),
            (
                orch.set_volume(zone_id, 0.8, Some(device_id)).await,
                OutputCommand::SetVolume,
            ),
            (
                orch.set_mute(zone_id, true, Some(device_id)).await,
                OutputCommand::SetMute,
            ),
        ] {
            assert_eq!(
                result,
                Err(OutputCommandError::Unsupported { command }),
                "{command} doit être refusée avant toute mutation"
            );
        }

        let state = orch.playback.get_state(zone_id).await;
        assert_eq!(state.state, PlayState::Playing);
        assert_eq!(state.position_ms, 12_000);
        assert!((state.volume - 0.5).abs() < f64::EPSILON);
        assert!(!state.muted);

        let persisted = zone_repo.get(zone_id).unwrap().unwrap();
        assert_eq!(persisted.last_position_ms, 0);
        assert_eq!(persisted.volume, 50);
        assert!(!persisted.muted);
    }

    #[tokio::test]
    async fn un_backend_qui_refuse_ne_modifie_ni_memoire_ni_base() {
        let orch = test_orchestrator();
        let device_id = "failing-command";
        let zone_repo = ZoneRepo::with_backend(orch.db.clone());
        let zone_id = zone_repo
            .create("Failing", Some("test"), Some(device_id))
            .unwrap();
        orch.outputs
            .lock()
            .await
            .register(Box::new(FailingCommandOutput {
                id: device_id.into(),
            }));
        orch.playback
            .play(
                zone_id,
                NowPlaying {
                    track_id: Some(43),
                    title: "Refus backend".into(),
                    duration_ms: 120_000,
                    source: "local".into(),
                    ..Default::default()
                },
            )
            .await;
        orch.playback.update_position(zone_id, 13_000).await;
        orch.playback.set_volume(zone_id, 0.5).await;

        assert!(matches!(
            orch.pause(zone_id, Some(device_id)).await,
            Err(OutputCommandError::Failed {
                command: OutputCommand::Pause,
                ..
            })
        ));
        assert!(matches!(
            orch.seek(zone_id, 42_000, Some(device_id)).await,
            Err(OutputCommandError::Failed {
                command: OutputCommand::Seek,
                ..
            })
        ));
        assert!(matches!(
            orch.set_volume(zone_id, 0.8, Some(device_id)).await,
            Err(OutputCommandError::Failed {
                command: OutputCommand::SetVolume,
                ..
            })
        ));
        assert!(matches!(
            orch.set_mute(zone_id, true, Some(device_id)).await,
            Err(OutputCommandError::Failed {
                command: OutputCommand::SetMute,
                ..
            })
        ));

        let state = orch.playback.get_state(zone_id).await;
        assert_eq!(state.state, PlayState::Playing);
        assert_eq!(state.position_ms, 13_000);
        assert_eq!(state.volume, 0.5);
        assert!(!state.muted);
        let persisted = zone_repo.get(zone_id).unwrap().unwrap();
        assert_eq!(persisted.last_position_ms, 0);
        assert_eq!(persisted.volume, 50);
        assert!(!persisted.muted);
    }

    /// Un timeout de transport ne doit PAS détruire la session de flux : la
    /// commande a pu atteindre le renderer, qui ira chercher l'URL. La détruire
    /// lui fait afficher « chanson non trouvée ». Un refus, lui, est concluant.
    #[tokio::test]
    async fn transport_timeout_keeps_the_stream_session_but_refusal_drops_it() {
        let orch = test_orchestrator();
        let flac = tempfile::Builder::new().suffix(".flac").tempfile().unwrap();
        let f = flac.path().to_path_buf();
        std::fs::write(&f, b"fake audio").unwrap();

        for (device_id, output, doit_survivre) in [
            (
                "timeout-dev",
                Box::new(TimingOutOutput {
                    id: "timeout-dev".into(),
                }) as Box<dyn crate::outputs::traits::OutputTarget>,
                true,
            ),
            (
                "reject-dev",
                Box::new(RejectingOutput {
                    id: "reject-dev".into(),
                }) as Box<dyn crate::outputs::traits::OutputTarget>,
                false,
            ),
        ] {
            orch.outputs.lock().await.register(output);

            let sid = orch
                .streamer
                .create_file_session(
                    crate::http::streamer::StreamInfo {
                        format: "flac".into(),
                        mime_type: "audio/flac".into(),
                        ..Default::default()
                    },
                    f.to_string_lossy().into_owned(),
                    false,
                )
                .await;

            assert!(
                orch.streamer.stream_bytes_sent(&sid).await.is_some(),
                "{device_id} : la session doit exister juste après sa création"
            );

            let media = crate::outputs::traits::PlayMedia {
                url: "http://server/stream",
                mime_type: "audio/flac",
                ..Default::default()
            };
            let (output_sent, output_error) = orch
                .send_to_output(device_id, &media, None, false, 1, None)
                .await;
            assert!(!output_sent, "{device_id} : l'envoi doit échouer");
            let err = output_error.expect("une erreur doit être remontée");

            // Même décision que la branche d'échec de play().
            if super::command_may_have_landed(&err) {
                // on conserve
            } else {
                orch.streamer.remove_session(&sid).await;
            }

            let encore_la = orch.streamer.stream_bytes_sent(&sid).await.is_some();
            assert_eq!(
                encore_la, doit_survivre,
                "{device_id} : session présente={encore_la}, attendu={doit_survivre}"
            );
        }
    }

    /// #1518 (Vincent) : seek d'une piste STREAMING (Qobuz/Tidal) sur sortie
    /// locale. Depuis b3a4a79f le transcodage WAV streaming est pré-seeké
    /// (decode_to_pcm_streaming_seeked reçoit seek_s), comme le chemin fichier
    /// local. Dériver le drapeau de media.file_path (None en streaming) faisait
    /// re-sauter l'offset une DEUXIÈME fois côté consommateur : un seek à 4:30
    /// jetait tout le PCM restant de la piste → silence total, puis boucle de
    /// redémarrage ~3 s (le poller voit la piste « finie » et la relance).
    #[cfg(feature = "local-audio")]
    #[tokio::test]
    async fn streaming_seek_on_local_output_is_producer_preseeked_no_consumer_skip() {
        let orch = test_orchestrator();
        let device_id = "local:Test Device 1518";
        orch.outputs
            .lock()
            .await
            .register(Box::new(crate::outputs::local::LocalOutput::new(
                "Test Device 1518".to_string(),
            )));

        // Média streaming : PAS de file_path (cas Qobuz/Tidal). L'URL refuse
        // la connexion, le thread audio s'arrête avant de toucher un device.
        let media = crate::outputs::traits::PlayMedia {
            url: "http://127.0.0.1:1/stream",
            mime_type: "audio/wav",
            ..Default::default()
        };
        let (sent, err) = orch
            .send_to_output(device_id, &media, Some(270_000), false, 1, None)
            .await;
        assert!(sent, "play_media doit partir : {err:?}");

        let arc = orch.outputs.lock().await.get(device_id).unwrap();
        let out = arc.lock().await;
        let local = out
            .as_any()
            .downcast_ref::<crate::outputs::local::LocalOutput>()
            .unwrap();
        assert!(
            local.producer_seeked(),
            "flux streaming transcodé = déjà pré-seeké : le consommateur ne doit PAS re-sauter l'offset (#1518)"
        );
    }

    #[test]
    fn prefetch_buffer_truncated_cases() {
        // Unknown duration (0) must count as truncated — the DMP-A8 cut.
        assert!(super::prefetch_buffer_truncated(30_000, 0));
        // Partial buffer of a known-length track: truncated.
        assert!(super::prefetch_buffer_truncated(30_000, 277_000));
        // Buffer covers (near) the whole track: NOT truncated.
        assert!(!super::prefetch_buffer_truncated(276_000, 277_000));
        assert!(!super::prefetch_buffer_truncated(300_000, 277_000));
        // Within the 2s tolerance: NOT truncated.
        assert!(!super::prefetch_buffer_truncated(60_000, 61_500));
    }

    #[tokio::test]
    async fn test_pause_resume_stop() {
        let orch = test_orchestrator();
        let zone_id = 1;

        // Set up a NowPlaying so pause/stop have state to work with
        let np = NowPlaying {
            track_id: Some(42),
            title: "Test Track".into(),
            artist_name: Some("Test Artist".into()),
            album_title: Some("Test Album".into()),
            cover_path: None,
            duration_ms: 180_000,
            source: "local".into(),
            source_id: None,
            stream_id: None,
            ..Default::default()
        };
        orch.playback.play(zone_id, np).await;

        // Pause
        orch.pause(zone_id, None).await.unwrap();
        let state = orch.playback.get_state(zone_id).await;
        assert_eq!(state.state, PlayState::Paused);

        // Resume
        orch.resume(zone_id, None).await.unwrap();
        let state = orch.playback.get_state(zone_id).await;
        assert_eq!(state.state, PlayState::Playing);

        // Stop
        orch.stop(zone_id, None).await;
        let state = orch.playback.get_state(zone_id).await;
        assert_eq!(state.state, PlayState::Stopped);
    }

    #[tokio::test]
    async fn test_seek_persists() {
        let orch = test_orchestrator();

        // Create a zone in the DB so save_playback_position has a row to UPDATE
        let zone_repo = ZoneRepo::with_backend(orch.db.clone());
        let zone_id = zone_repo.create("Test Zone", None, None).unwrap();

        // Set up NowPlaying (seek persists position only when now_playing exists)
        let np = NowPlaying {
            track_id: Some(99),
            title: "Seek Test".into(),
            artist_name: None,
            album_title: None,
            cover_path: None,
            duration_ms: 300_000,
            source: "local".into(),
            source_id: None,
            stream_id: None,
            ..Default::default()
        };
        orch.playback.play(zone_id, np).await;

        // Seek to 42 seconds
        orch.seek(zone_id, 42_000, None).await.unwrap();

        // Verify in-memory state updated
        let state = orch.playback.get_state(zone_id).await;
        assert_eq!(state.position_ms, 42_000);

        // Verify DB position saved
        let zone = zone_repo.get(zone_id).unwrap().unwrap();
        assert_eq!(zone.last_position_ms, 42_000);
        assert_eq!(zone.last_track_id, Some(99));
        assert_eq!(zone.last_track_source.as_deref(), Some("local"));
    }

    #[tokio::test]
    async fn test_set_volume() {
        let orch = test_orchestrator();
        let zone_id = 1;

        // Initialize zone state with a NowPlaying
        let np = NowPlaying {
            track_id: None,
            title: "Volume Test".into(),
            artist_name: None,
            album_title: None,
            cover_path: None,
            duration_ms: 60_000,
            source: "local".into(),
            source_id: None,
            stream_id: None,
            ..Default::default()
        };
        orch.playback.play(zone_id, np).await;

        // Set volume to 80%
        orch.set_volume(zone_id, 0.8, None).await.unwrap();
        let state = orch.playback.get_state(zone_id).await;
        assert!((state.volume - 0.8).abs() < f64::EPSILON);

        // Set volume to 0 (mute level)
        orch.set_volume(zone_id, 0.0, None).await.unwrap();
        let state = orch.playback.get_state(zone_id).await;
        assert!((state.volume - 0.0).abs() < f64::EPSILON);

        // Set volume to 1.0 (max)
        orch.set_volume(zone_id, 1.0, None).await.unwrap();
        let state = orch.playback.get_state(zone_id).await;
        assert!((state.volume - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn gain_trim_factor_convertit_et_clampe() {
        use crate::orchestrator::gain_trim_factor;
        assert!((gain_trim_factor(0.0) - 1.0).abs() < 1e-9);
        // -6 dB ≈ ×0.5012
        assert!((gain_trim_factor(-6.0) - 0.501_187).abs() < 1e-4);
        // +6 dB ≈ ×1.9953
        assert!((gain_trim_factor(6.0) - 1.995_262).abs() < 1e-4);
        // Clamp ±12 dB
        assert!((gain_trim_factor(-40.0) - gain_trim_factor(-12.0)).abs() < 1e-9);
        assert!((gain_trim_factor(40.0) - gain_trim_factor(12.0)).abs() < 1e-9);
    }

    #[tokio::test]
    async fn le_trim_ne_touche_pas_le_volume_utilisateur() {
        // Le trim n'affecte que la valeur envoyée au device : l'état de
        // lecture (ce que l'UI affiche) et la base gardent le volume brut.
        let orch = test_orchestrator();
        let zone_repo = ZoneRepo::with_backend(orch.db.clone());
        let zone_id = zone_repo.create("Trim Zone", None, None).unwrap();
        crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone())
            .set(&format!("zone_{zone_id}_gain_trim_db"), "-6")
            .unwrap();

        orch.set_volume(zone_id, 0.8, None).await.unwrap();
        let state = orch.playback.get_state(zone_id).await;
        assert!((state.volume - 0.8).abs() < f64::EPSILON);
        let zone = zone_repo.get(zone_id).unwrap().unwrap();
        assert_eq!(zone.volume, 80);
    }

    #[tokio::test]
    async fn test_persist_position_on_pause() {
        let orch = test_orchestrator();

        // Create a zone in DB
        let zone_repo = ZoneRepo::with_backend(orch.db.clone());
        let zone_id = zone_repo.create("Pause Zone", None, None).unwrap();

        // Set up playback at a known position
        let np = NowPlaying {
            track_id: Some(7),
            title: "Pause Persist".into(),
            artist_name: None,
            album_title: None,
            cover_path: None,
            duration_ms: 200_000,
            source: "local".into(),
            source_id: Some("src-7".into()),
            stream_id: None,
            ..Default::default()
        };
        orch.playback.play(zone_id, np).await;
        orch.playback.update_position(zone_id, 55_000).await;

        // Pause triggers persist_position
        orch.pause(zone_id, None).await.unwrap();

        let zone = zone_repo.get(zone_id).unwrap().unwrap();
        assert_eq!(zone.last_position_ms, 55_000);
        assert_eq!(zone.last_track_id, Some(7));
        assert_eq!(zone.last_track_source_id.as_deref(), Some("src-7"));
    }

    #[tokio::test]
    async fn test_persist_position_on_stop() {
        let orch = test_orchestrator();

        let zone_repo = ZoneRepo::with_backend(orch.db.clone());
        let zone_id = zone_repo.create("Stop Zone", None, None).unwrap();

        let np = NowPlaying {
            track_id: Some(10),
            title: "Stop Persist".into(),
            artist_name: Some("Artist".into()),
            album_title: None,
            cover_path: None,
            duration_ms: 120_000,
            source: "tidal".into(),
            source_id: Some("tidal-10".into()),
            stream_id: None,
            ..Default::default()
        };
        orch.playback.play(zone_id, np).await;
        orch.playback.update_position(zone_id, 90_000).await;

        // Stop also persists position
        orch.stop(zone_id, None).await;

        let zone = zone_repo.get(zone_id).unwrap().unwrap();
        assert_eq!(zone.last_position_ms, 90_000);
        assert_eq!(zone.last_track_source.as_deref(), Some("tidal"));
    }

    #[tokio::test]
    async fn test_record_listen() {
        use crate::db::history_repo::HistoryRepo;

        let orch = test_orchestrator();

        // Create a zone so the FK constraint on zone_id is satisfied
        let zone_repo = ZoneRepo::with_backend(orch.db.clone());
        let zone_id = zone_repo.create("Listen Zone", None, None).unwrap();

        orch.record_listen(
            "Test Song",
            Some("Artist"),
            Some("Album"),
            "local",
            None,
            None,
            180_000,
            zone_id,
            None,
            Some(7),
            Some("playlist"),
            Some("12"),
        );

        let repo = HistoryRepo::with_backend(orch.db.clone());
        let history = repo.recent(10).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].title, "Test Song");
        assert_eq!(history[0].artist_name.as_deref(), Some("Artist"));
        // The owning profile passed by the caller is persisted verbatim
        // (session → history tag), no longer read from the global setting.
        // recent()'s RECORD_COLS omits profile_id, so assert on the column
        // directly to prove the write stored the caller's value.
        let stored_profile = orch
            .db
            .query_one("SELECT profile_id FROM listen_history LIMIT 1", &[])
            .ok()
            .flatten()
            .and_then(|cols| cols.first().and_then(|v| v.as_i64()));
        assert_eq!(stored_profile, Some(7));
        // #2441 — l'intention passee par l'appelant est ecrite telle quelle.
        // Sans elle, cette ligne est indiscernable de la meme piste jouee
        // seule, et aucune rubrique ne peut « refleter la realite de ce qu'a
        // voulu faire l'auditeur ».
        assert_eq!(history[0].context_type.as_deref(), Some("playlist"));
        assert_eq!(history[0].context_id.as_deref(), Some("12"));
        assert_eq!(history[0].source, "local");
    }

    // ------------------------------------------------------------------
    // #1998 — zone navigateur : l'annonce suit la PREUVE de lecture.
    //
    // Aucun de ces tests ne touche le réseau : sans clé Last.fm ni jeton
    // ListenBrainz dans la base de test, `dispatch_now_playing` sort avant le
    // moindre appel. Ce qui est observé ici est l'effet vérifiable côté
    // serveur — la ligne `listen_history` et le verrou « une seule fois ».
    // ------------------------------------------------------------------

    /// Une session de flux dont l'onglet a tiré `octets` octets.
    async fn session_navigateur(
        orch: &PlaybackOrchestrator,
        fichier: &std::path::Path,
        octets: u64,
    ) -> String {
        let sid = orch
            .streamer
            .create_file_session(
                crate::http::streamer::StreamInfo {
                    format: "flac".into(),
                    mime_type: "audio/flac".into(),
                    ..Default::default()
                },
                fichier.to_string_lossy().into_owned(),
                false,
            )
            .await;
        if octets > 0 {
            let sessions = orch.streamer.sessions_state();
            let sessions = sessions.lock().await;
            sessions
                .get(&sid)
                .expect("la session vient d'être créée")
                .bytes_sent
                .store(octets, std::sync::atomic::Ordering::Relaxed);
        }
        sid
    }

    fn annonce_en_attente(
        stream_id: &str,
        record_history: bool,
        source: &str,
    ) -> super::AnnonceNavigateurDifferee {
        super::AnnonceNavigateurDifferee {
            stream_id: stream_id.to_string(),
            title: "Come on In".into(),
            artist: Some("Bridge City Sinners".into()),
            album: Some("Unholy Hymns".into()),
            source: source.into(),
            source_id: None,
            track_id: None,
            duration_ms: 180_000,
            cover_path: None,
            record_history,
        }
    }

    fn lignes_historique(orch: &PlaybackOrchestrator) -> usize {
        crate::db::history_repo::HistoryRepo::with_backend(orch.db.clone())
            .recent(10)
            .unwrap()
            .len()
    }

    /// Le cœur du ticket rouvert : tant que l'onglet n'a rien tiré, rien n'est
    /// annoncé ; dès qu'il tire, l'annonce part — et une seule fois.
    #[tokio::test]
    async fn zone_navigateur_l_annonce_attend_que_l_onglet_tire_le_flux() {
        let orch = test_orchestrator();
        let zone_id = ZoneRepo::with_backend(orch.db.clone())
            .create("Ce PC", Some("browser"), None)
            .unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("piste.flac");
        std::fs::write(&f, b"fake audio").unwrap();

        // Onglet muet : la session existe, personne ne la consomme.
        let sid = session_navigateur(&orch, &f, 0).await;
        orch.annonces_navigateur
            .lock()
            .unwrap()
            .insert(zone_id, annonce_en_attente(&sid, true, "local"));

        assert!(
            !orch.confirmer_lecture_navigateur(zone_id, &sid).await,
            "aucun octet tiré : l'annonce ne doit pas partir — c'est le défaut d'origine"
        );
        assert_eq!(
            lignes_historique(&orch),
            0,
            "listen_history ne doit rien porter tant que rien n'a été entendu"
        );
        assert!(
            orch.annonces_navigateur
                .lock()
                .unwrap()
                .contains_key(&zone_id),
            "l'annonce reste en attente : l'onglet peut encore démarrer"
        );

        // L'onglet tire le flux : c'est la preuve.
        {
            let sessions = orch.streamer.sessions_state();
            let sessions = sessions.lock().await;
            sessions
                .get(&sid)
                .unwrap()
                .bytes_sent
                .store(64 * 1024, std::sync::atomic::Ordering::Relaxed);
        }

        assert!(
            orch.confirmer_lecture_navigateur(zone_id, &sid).await,
            "l'onglet consomme le flux : l'annonce doit partir"
        );
        assert_eq!(
            lignes_historique(&orch),
            1,
            "l'historique local doit enfin porter cette écoute"
        );

        // Le tick suivant ne doit pas re-annoncer.
        assert!(
            !orch.confirmer_lecture_navigateur(zone_id, &sid).await,
            "le poller repasse chaque seconde : une écoute, une annonce"
        );
        assert_eq!(
            lignes_historique(&orch),
            1,
            "pas de doublon au tick suivant"
        );
    }

    /// `record_history=false` (recherche de position, reconnexion) : l'annonce
    /// « en écoute » part, mais l'historique ne doublonne pas. Sans ce report
    /// du drapeau, déplacer le curseur ajouterait une ligne à chaque fois.
    #[tokio::test]
    async fn zone_navigateur_une_recreation_de_flux_ne_doublonne_pas_l_historique() {
        let orch = test_orchestrator();
        let zone_id = ZoneRepo::with_backend(orch.db.clone())
            .create("Ce PC", Some("browser"), None)
            .unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("piste.flac");
        std::fs::write(&f, b"fake audio").unwrap();
        let sid = session_navigateur(&orch, &f, 64 * 1024).await;
        orch.annonces_navigateur
            .lock()
            .unwrap()
            .insert(zone_id, annonce_en_attente(&sid, false, "local"));

        assert!(
            orch.confirmer_lecture_navigateur(zone_id, &sid).await,
            "l'annonce « en écoute » part quand même : la piste est bien entendue"
        );
        assert_eq!(
            lignes_historique(&orch),
            0,
            "une re-création de flux pour une piste déjà en cours ne s'ajoute pas à l'historique"
        );
    }

    /// La radio n'entre pas dans l'historique local — même exclusion que le
    /// chemin nominal, pour la même raison (titre figé au démarrage).
    #[tokio::test]
    async fn zone_navigateur_la_radio_reste_hors_de_l_historique() {
        let orch = test_orchestrator();
        let zone_id = ZoneRepo::with_backend(orch.db.clone())
            .create("Ce PC", Some("browser"), None)
            .unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("piste.flac");
        std::fs::write(&f, b"fake audio").unwrap();
        let sid = session_navigateur(&orch, &f, 64 * 1024).await;
        orch.annonces_navigateur
            .lock()
            .unwrap()
            .insert(zone_id, annonce_en_attente(&sid, true, "radio"));

        assert!(orch.confirmer_lecture_navigateur(zone_id, &sid).await);
        assert_eq!(
            lignes_historique(&orch),
            0,
            "la radio ne s'écrit pas dans listen_history"
        );
    }

    /// Une lecture abandonnée avant le premier octet ne s'annonce jamais, même
    /// si un vieux flux traîne : l'attente est identifiée par SON flux.
    #[tokio::test]
    async fn zone_navigateur_un_autre_flux_ne_libere_pas_l_annonce() {
        let orch = test_orchestrator();
        let zone_id = ZoneRepo::with_backend(orch.db.clone())
            .create("Ce PC", Some("browser"), None)
            .unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("piste.flac");
        std::fs::write(&f, b"fake audio").unwrap();
        let attendu = session_navigateur(&orch, &f, 0).await;
        let autre = session_navigateur(&orch, &f, 64 * 1024).await;
        orch.annonces_navigateur
            .lock()
            .unwrap()
            .insert(zone_id, annonce_en_attente(&attendu, true, "local"));

        assert!(
            !orch.confirmer_lecture_navigateur(zone_id, &autre).await,
            "les octets d'un AUTRE flux ne prouvent rien sur celui-ci"
        );
        assert_eq!(lignes_historique(&orch), 0);
    }

    /// Arrêt avant le premier octet : il n'y a rien eu à entendre, l'attente
    /// meurt avec la lecture.
    #[tokio::test]
    async fn zone_navigateur_l_arret_annule_l_annonce_en_attente() {
        let orch = test_orchestrator();
        let zone_id = ZoneRepo::with_backend(orch.db.clone())
            .create("Ce PC", Some("browser"), None)
            .unwrap();
        let tmp = tempfile::TempDir::new().unwrap();
        let f = tmp.path().join("piste.flac");
        std::fs::write(&f, b"fake audio").unwrap();
        let sid = session_navigateur(&orch, &f, 0).await;
        orch.annonces_navigateur
            .lock()
            .unwrap()
            .insert(zone_id, annonce_en_attente(&sid, true, "local"));

        orch.stop(zone_id, None).await;

        assert!(
            orch.annonces_navigateur.lock().unwrap().is_empty(),
            "l'arrêt oublie l'annonce en attente"
        );
        // Même si l'onglet tire des octets après coup (fin de tampon), plus
        // rien ne part : la lecture est terminée.
        {
            let sessions = orch.streamer.sessions_state();
            let sessions = sessions.lock().await;
            if let Some(s) = sessions.get(&sid) {
                s.bytes_sent
                    .store(64 * 1024, std::sync::atomic::Ordering::Relaxed);
            }
        }
        assert!(!orch.confirmer_lecture_navigateur(zone_id, &sid).await);
        assert_eq!(lignes_historique(&orch), 0);
    }

    #[tokio::test]
    async fn test_resolve_cover_url_passthrough() {
        let orch = test_orchestrator();
        let result = orch.resolve_cover_url(Some("https://img.tidal.com/cover.jpg"));
        assert_eq!(result.as_deref(), Some("https://img.tidal.com/cover.jpg"));

        let result = orch.resolve_cover_url(Some("http://local/art.png"));
        assert_eq!(result.as_deref(), Some("http://local/art.png"));
    }

    #[tokio::test]
    async fn test_resolve_cover_url_hash() {
        let orch = test_orchestrator();
        let result = orch.resolve_cover_url(Some("abc123def"));
        let url = result.unwrap();
        assert!(
            url.contains("/api/v1/library/artwork/abc123def"),
            "got: {url}"
        );
        assert!(url.starts_with("http://"), "got: {url}");
    }

    #[tokio::test]
    async fn test_resolve_cover_url_none() {
        let orch = test_orchestrator();
        assert!(orch.resolve_cover_url(None).is_none());
    }

    #[tokio::test]
    async fn test_persist_local_queue() {
        use crate::db::play_queue_repo::PlayQueueRepo;

        let orch = test_orchestrator();
        let zone_repo = ZoneRepo::with_backend(orch.db.clone());
        let zone_id = zone_repo.create("Queue Zone", None, None).unwrap();

        // Insert some tracks so FK constraints are satisfied
        orch.db
            .execute("INSERT INTO artists (id, name) VALUES (1, 'Artist')", &[])
            .unwrap();
        orch.db
            .execute(
                "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Album', 1)",
                &[],
            )
            .unwrap();
        for i in 1..=3i64 {
            let title = format!("Track {i}");
            orch.db
                .execute(
                    "INSERT INTO tracks (id, title, album_id, artist_id, duration_ms) VALUES (?, ?, 1, 1, 180000)",
                    &[&i as &dyn crate::db::backend::ToSqlValue, &title as &dyn crate::db::backend::ToSqlValue],
                )
                .unwrap();
        }

        orch.persist_local_queue(zone_id, &[1, 2, 3], 0);

        let queue_repo = PlayQueueRepo::with_backend(orch.db.clone());
        let queue = queue_repo.get_queue(zone_id).unwrap();
        assert_eq!(queue.len(), 3);
    }

    fn radio_test_eq_profile() -> crate::audio::eq::EqProfile {
        crate::audio::eq::EqProfile {
            enabled: true,
            bands: vec![crate::audio::eq::EqBandSpec {
                freq: 80.0,
                gain: 8.0,
                q: 0.71,
                band_type: "low_shelf".into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn f32_bytes(samples: &[f32]) -> Vec<u8> {
        samples
            .iter()
            .flat_map(|sample| sample.to_le_bytes())
            .collect()
    }

    /// #2063 — contre-épreuve négative : ajouter le crochet DSP au décodeur
    /// radio ne doit modifier AUCUN octet quand la zone n'a pas d'EQ actif.
    #[test]
    fn radio_without_eq_keeps_pcm_byte_for_byte() {
        let mut samples = vec![0.0, -0.0, 0.125, -0.25, 0.75, -0.875];
        let expected = f32_bytes(&samples);
        let mut eq = None;
        super::apply_radio_eq(&mut eq, &mut samples);
        assert_eq!(f32_bytes(&samples), expected);
    }

    /// Le témoin positif complète le précédent : un profil réellement actif
    /// doit atteindre le PCM que Tune servira dans le WAV, pas seulement
    /// forcer une route de transcodage qui oublierait ensuite le processeur.
    #[test]
    fn radio_with_eq_changes_the_pcm_served_to_the_renderer() {
        let profile = radio_test_eq_profile();
        let mut eq = Some(crate::audio::eq::EqProcessor::new(&profile, 44_100, 2));
        let mut samples = vec![0.10f32; 2 * 1024];
        let untouched = f32_bytes(&samples);
        super::apply_radio_eq(&mut eq, &mut samples);
        assert_ne!(f32_bytes(&samples), untouched);
    }

    /// Une URL MP3 explicite passait directement au navigateur. Avec un EQ,
    /// ce passthrough contournerait tous les filtres : la zone doit recevoir
    /// une session WAV Tune même sans `output_device_id`.
    #[tokio::test]
    async fn browser_radio_with_eq_is_forced_through_the_wav_session() {
        let orch = test_orchestrator();
        let zone_id = ZoneRepo::with_backend(orch.db.clone())
            .create("Ce PC", Some("browser"), None)
            .unwrap();
        crate::db::settings_repo::SettingsRepo::with_backend(orch.db.clone())
            .set(
                &format!("zone_{zone_id}_eq_profile"),
                &serde_json::to_string(&radio_test_eq_profile()).unwrap(),
            )
            .unwrap();
        let source = "http://127.0.0.1:9/station.mp3";
        let req = super::PlayRequest {
            zone_id,
            output_device_id: None,
            track_id: None,
            source: Some("radio".into()),
            source_id: Some(source.into()),
            title: Some("Radio avec EQ".into()),
            artist_name: None,
            album_title: None,
            cover_url: None,
            duration_ms: None,
            seek_ms: None,
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: None,
            disc_number: None,
        };

        let resolved = orch.resolve_direct_url(&req).await.unwrap();
        assert!(
            resolved.stream_id.is_some(),
            "l'EQ actif doit interdire le passthrough MP3"
        );
        assert_eq!(resolved.mime_type, "audio/wav");
        assert_eq!(resolved.origin_url.as_deref(), Some(source));
    }

    /// #2670 — une zone NAVIGATEUR ne doit jamais recevoir l'URL de la station.
    ///
    /// Le client web reecrit toute URL absolue en chemin relatif
    /// (`browserPlay` : `u.pathname + u.search`), pour joindre l'hote Tune
    /// plutot que l'IP que le serveur annonce. L'URL d'une station en `.mp3`
    /// devient donc une requete `/station.mp3` adressee a Tune, a laquelle le
    /// repli SPA (`routes/mod.rs`, `ServeDir::fallback(ServeFile(index.html))`)
    /// repond 200 `text/html` : une page web au lieu du flux.
    ///
    /// Et Tune ne peut rien en dire : sans session locale il n'ouvre jamais le
    /// flux, donc ni `non_audio_content_type` ni `RADIO_NOT_AUDIO` — qui vivent
    /// dans `decode_radio_stream_to_pcm` — ne peuvent se declencher. C'etait le
    /// seul chemin radio ou une station morte restait muette ET silencieuse.
    #[tokio::test]
    async fn browser_radio_mp3_is_never_handed_the_station_url() {
        let orch = test_orchestrator();
        let zone_id = ZoneRepo::with_backend(orch.db.clone())
            .create("Ce PC", Some("browser"), None)
            .unwrap();
        // Aucun profil EQ n'est ecrit : c'est precisement le cas que le
        // passthrough laissait passer (#2063 ne couvrait que l'EQ actif).
        // Port 9 (discard) : la tache de decodage echoue en local, aucun appel
        // reseau reel ne sort de ce test.
        let source = "http://127.0.0.1:9/tsfjazz-high.mp3";
        let req = super::PlayRequest {
            zone_id,
            output_device_id: None,
            track_id: None,
            source: Some("radio".into()),
            source_id: Some(source.into()),
            title: Some("TSF Jazz".into()),
            artist_name: None,
            album_title: None,
            cover_url: None,
            duration_ms: None,
            seek_ms: None,
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: None,
            disc_number: None,
        };

        let resolved = orch.resolve_direct_url(&req).await.unwrap();
        assert!(
            resolved.stream_id.is_some(),
            "une zone navigateur doit recevoir une session Tune, pas l'URL de la station"
        );
        assert_ne!(
            resolved.url, source,
            "l'URL de la station renvoyee telle quelle est reecrite en chemin local par le client, \
             qui recoit alors la page HTML de Tune"
        );
        assert_eq!(resolved.mime_type, "audio/wav");
        // L'amont voyage quand meme : un enregistreur ou les titres ICY n'ont
        // pas d'autre chemin de retour vers la source.
        assert_eq!(resolved.origin_url.as_deref(), Some(source));
    }

    #[tokio::test]
    async fn radio_resolve_direct_url_without_output_device() {
        let orch = test_orchestrator();
        let req = super::PlayRequest {
            zone_id: 1,
            output_device_id: None,
            track_id: None,
            source: Some("radio".into()),
            source_id: Some("http://icecast.radiofrance.fr/fip-hifi.aac".into()),
            title: Some("FIP".into()),
            artist_name: None,
            album_title: None,
            cover_url: None,
            duration_ms: None,
            seek_ms: None,
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: None,
            disc_number: None,
        };
        let resolved = orch.resolve_direct_url(&req).await.unwrap();
        // Since the Cyrille/Yamaha fix, ambiguous codecs (.aac/.ogg/HLS/
        // extension-less) are ALWAYS proxied and transcoded to WAV, even
        // without an output device: the advertised protocolInfo must match
        // the bytes, or DLNA renderers play silence.
        assert!(
            resolved.stream_id.is_some(),
            "ambiguous .aac radio must be proxied to WAV"
        );
        assert_eq!(resolved.mime_type, "audio/wav");
        // Because `url` is now ours and not the station's, the upstream has to
        // travel with it: a recorder wanting the original AAC (or the ICY titles
        // the proxy drops) has no other way back to the source.
        assert_eq!(
            resolved.origin_url.as_deref(),
            Some("http://icecast.radiofrance.fr/fip-hifi.aac")
        );
    }

    #[tokio::test]
    async fn radio_reliable_mp3_passes_through_without_output_device() {
        let orch = test_orchestrator();
        let req = super::PlayRequest {
            zone_id: 1,
            output_device_id: None,
            track_id: None,
            source: Some("radio".into()),
            source_id: Some("http://stream.example.com/station.mp3".into()),
            title: Some("MP3 Station".into()),
            artist_name: None,
            album_title: None,
            cover_url: None,
            duration_ms: None,
            seek_ms: None,
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: None,
            disc_number: None,
        };
        let resolved = orch.resolve_direct_url(&req).await.unwrap();
        // Reliable extensions (.mp3/.flac/.wav) pass through untouched: no
        // proxy session, no transcode cost.
        assert!(resolved.stream_id.is_none());
        assert_eq!(resolved.url, "http://stream.example.com/station.mp3");
        // Nothing was substituted, so there is no upstream to point at: `url`
        // already is it.
        assert!(resolved.origin_url.is_none());
    }

    #[tokio::test]
    async fn podcast_resolve_returns_raw_url() {
        let orch = test_orchestrator();
        let req = super::PlayRequest {
            zone_id: 1,
            output_device_id: None,
            track_id: None,
            source: Some("podcast".into()),
            source_id: Some("https://cdn.podcast.com/episode.mp3".into()),
            title: Some("Episode 1".into()),
            artist_name: None,
            album_title: None,
            cover_url: None,
            duration_ms: Some(3600000),
            seek_ms: None,
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: None,
            disc_number: None,
        };
        let resolved = orch.resolve_direct_url(&req).await.unwrap();
        assert!(
            resolved.stream_id.is_none(),
            "podcast should not create proxy session"
        );
        assert_eq!(resolved.url, "https://cdn.podcast.com/episode.mp3");
    }

    /// Une URL de flux Bandcamp telle qu'elle est publiée : pas d'extension,
    /// le codec est dans le CHEMIN (`/mp3-128/`), pas au bout du nom.
    const BC_STREAM: &str =
        "https://t4.bcbits.com/stream/0123456789abcdef/mp3-128/1234567?p=0&ts=1&sig=deadbeef";

    #[tokio::test]
    async fn bandcamp_resolves_by_the_direct_url_door() {
        // Le point de la correction : `source = "bandcamp"` doit ARRIVER dans
        // `resolve_direct_url` via `resolve_stream`, et non partir chercher un
        // service « bandcamp » dans le registre (qui n'existe pas) — c'est
        // l'échec qui laissait la lecture dans l'onglet du navigateur.
        let orch = test_orchestrator();
        let req = super::PlayRequest {
            zone_id: 1,
            output_device_id: None,
            track_id: None,
            source: Some("bandcamp".into()),
            source_id: Some(BC_STREAM.into()),
            title: Some("A Track".into()),
            artist_name: Some("An Artist".into()),
            album_title: Some("An Album".into()),
            cover_url: None,
            duration_ms: Some(212_000),
            seek_ms: None,
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: Some("mp3".into()),
            track_number: None,
            disc_number: None,
        };
        let resolved = orch.resolve_stream(&req).await.unwrap();
        assert_eq!(resolved.source, "bandcamp");
        assert_eq!(
            resolved.url, BC_STREAM,
            "sans sortie : URL servie telle quelle"
        );
        assert!(resolved.stream_id.is_none());
    }

    #[tokio::test]
    async fn bandcamp_mime_is_asserted_not_guessed() {
        // L'URL n'a pas d'extension : `guess_mime_from_url` retomberait sur son
        // défaut. On veut que le MIME soit AFFIRMÉ — Bandcamp ne sert que du
        // mp3-128 —, pas hérité d'un défaut qui pourrait changer.
        assert_eq!(super::guess_mime_from_url(BC_STREAM), "audio/mpeg");
        let orch = test_orchestrator();
        let req = super::PlayRequest {
            zone_id: 1,
            output_device_id: None,
            track_id: None,
            source: Some("bandcamp".into()),
            source_id: Some("https://t4.bcbits.com/stream/x/mp3-128/42".into()),
            title: Some("A Track".into()),
            artist_name: None,
            album_title: None,
            cover_url: None,
            duration_ms: None,
            seek_ms: None,
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: None,
            disc_number: None,
        };
        let resolved = orch.resolve_direct_url(&req).await.unwrap();
        assert_eq!(resolved.mime_type, "audio/mpeg");
    }

    #[tokio::test]
    async fn bandcamp_is_proxied_in_clear_http_for_a_network_renderer() {
        // Bandcamp ne publie qu'en HTTPS ; un renderer DLNA ne sait pas ouvrir
        // TLS. Sans proxy local, il annonce PLAYING et n'émet rien — le faux
        // positif déjà vu sur le Yamaha R-N2000A.
        let orch = test_orchestrator();
        let req = super::PlayRequest {
            zone_id: 1,
            output_device_id: Some("dlna:uuid-1234".into()),
            track_id: None,
            source: Some("bandcamp".into()),
            source_id: Some(BC_STREAM.into()),
            title: Some("A Track".into()),
            artist_name: None,
            album_title: None,
            cover_url: None,
            duration_ms: Some(212_000),
            seek_ms: None,
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: None,
            disc_number: None,
        };
        let resolved = orch.resolve_direct_url(&req).await.unwrap();
        assert!(
            resolved.stream_id.is_some(),
            "une sortie réseau doit passer par une session proxy locale"
        );
        assert!(
            resolved.url.starts_with("http://"),
            "l'URL servie au renderer doit être en clair, pas en TLS : {}",
            resolved.url
        );
        // Les octets passent verbatim : c'est toujours du MP3, rien n'est
        // transcodé. Le renderer doit donc lire « audio/mpeg ».
        assert_eq!(resolved.mime_type, "audio/mpeg");
    }

    #[tokio::test]
    async fn bandcamp_is_proxied_for_a_browser_zone_without_an_output_device() {
        // Contre-épreuve #2076/#2158 : une zone navigateur n'a jamais de
        // `output_device_id`. Ce n'est pas une zone orpheline — l'onglet est la
        // sortie — et il doit tirer une URL Tune locale, pas l'URL tierce que le
        // client réécrirait en chemin relatif avant de recevoir du text/html.
        let orch = test_orchestrator();
        let zone_id = ZoneRepo::with_backend(orch.db.clone())
            .create("Ce PC", Some("browser"), None)
            .unwrap();
        let req = super::PlayRequest {
            zone_id,
            output_device_id: None,
            track_id: None,
            source: Some("bandcamp".into()),
            source_id: Some(BC_STREAM.into()),
            title: Some("A Track".into()),
            artist_name: None,
            album_title: None,
            cover_url: None,
            duration_ms: Some(212_000),
            seek_ms: None,
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: None,
            disc_number: None,
        };

        let resolved = orch.resolve_direct_url(&req).await.unwrap();
        let stream_id = resolved
            .stream_id
            .as_deref()
            .expect("une zone navigateur doit recevoir une session proxy");
        assert!(
            resolved.url.ends_with(&format!("/stream/{stream_id}.mp3")),
            "le navigateur doit tirer le MP3 depuis Tune : {}",
            resolved.url
        );
        assert!(
            !resolved.url.contains("bcbits.com"),
            "l'URL Bandcamp ne doit jamais être rendue au navigateur"
        );
        assert_eq!(resolved.origin_url.as_deref(), Some(BC_STREAM));
        assert_eq!(resolved.mime_type, "audio/mpeg");
    }

    #[tokio::test]
    async fn bandcamp_is_decoded_to_wav_for_an_oaat_endpoint() {
        // Un endpoint OAAT ne consomme que du PCM en conteneur WAV.
        let orch = test_orchestrator();
        let req = super::PlayRequest {
            zone_id: 1,
            output_device_id: Some("oaat:endpoint-1".into()),
            track_id: None,
            source: Some("bandcamp".into()),
            source_id: Some(BC_STREAM.into()),
            title: Some("A Track".into()),
            artist_name: None,
            album_title: None,
            cover_url: None,
            duration_ms: Some(212_000),
            seek_ms: None,
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: None,
            disc_number: None,
        };
        let resolved = orch.resolve_direct_url(&req).await.unwrap();
        assert!(resolved.stream_id.is_some());
        assert_eq!(resolved.mime_type, "audio/wav");
        assert_eq!(resolved.sample_rate, Some(44100));
    }

    #[tokio::test]
    async fn bandcamp_goes_straight_to_a_local_dac() {
        // La sortie locale télécharge et décode elle-même un flux HTTP
        // compressé (`local_audio_non_wav_stream_detected_decoding`) : rien à
        // interposer, et surtout rien à transcoder pour rien.
        let orch = test_orchestrator();
        let req = super::PlayRequest {
            zone_id: 1,
            output_device_id: Some("local:default".into()),
            track_id: None,
            source: Some("bandcamp".into()),
            source_id: Some(BC_STREAM.into()),
            title: Some("A Track".into()),
            artist_name: None,
            album_title: None,
            cover_url: None,
            duration_ms: Some(212_000),
            seek_ms: None,
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: None,
            disc_number: None,
        };
        let resolved = orch.resolve_direct_url(&req).await.unwrap();
        assert!(resolved.stream_id.is_none());
        assert_eq!(resolved.url, BC_STREAM);
        assert_eq!(resolved.mime_type, "audio/mpeg");
        // Affirmée, pas héritée d'un défaut : le chemin du signal doit
        // annoncer « MP3 — Avec perte » de sa propre autorité.
        assert_eq!(resolved.sample_rate, Some(44100));
        assert_eq!(resolved.bit_depth, Some(16));
    }

    /// #2074 — l'URL est la seule autorité sur la qualité.
    ///
    /// Bandcamp nomme l'encodage dans l'URL, sous deux formes. Un fichier
    /// ACHETÉ emprunte la même porte avec une autre valeur : la règle doit
    /// donc porter sur le flux, jamais sur le nom du service.
    #[test]
    fn bandcamp_quality_is_read_from_the_url_never_from_the_source_name() {
        use super::{bandcamp_encoding, bandcamp_quality};

        // Forme « segment de chemin » — l'écoute libre publiée par Bandcamp.
        assert_eq!(bandcamp_encoding(BC_STREAM).as_deref(), Some("mp3-128"));
        let libre = bandcamp_quality("mp3-128").expect("mp3-128 est connu");
        assert_eq!(libre.codec, "mp3");
        assert_eq!(libre.mime_type, "audio/mpeg");
        assert_eq!(libre.bitrate_kbps, Some(128));

        // Forme « paramètre de requête » — la redirection de flux.
        assert_eq!(
            bandcamp_encoding("https://bandcamp.com/stream_redirect?enc=mp3-128&track_id=1")
                .as_deref(),
            Some("mp3-128")
        );

        // ACHAT en lossless : ni MP3, ni débit. C'est le cœur de la règle.
        let achete = bandcamp_quality(
            &bandcamp_encoding("https://popplers5.bandcamp.com/download/track?enc=flac&id=42")
                .expect("enc=flac doit être lu"),
        )
        .expect("flac est connu");
        assert_eq!(achete.codec, "flac");
        assert_eq!(achete.mime_type, "audio/flac");
        assert_eq!(
            achete.bitrate_kbps, None,
            "un flux sans perte n'a aucun débit à annoncer"
        );

        // ACHAT en MP3 320 : même codec que l'extrait, débit différent.
        assert_eq!(
            bandcamp_quality("mp3-320").map(|q| q.bitrate_kbps),
            Some(Some(320))
        );
        // Débit VARIABLE : on n'invente pas de chiffre.
        assert_eq!(
            bandcamp_quality("mp3-v0").map(|q| q.bitrate_kbps),
            Some(None)
        );

        // Un hachage de chemin ne doit jamais passer pour un encodage.
        assert_eq!(
            bandcamp_encoding("https://t4.bcbits.com/stream/0123456789abcdef/7654321?p=0"),
            None
        );
        assert_eq!(bandcamp_quality("chose-inconnue"), None);
    }

    #[tokio::test]
    async fn bandcamp_carries_its_128_kbps_all_the_way_to_the_zone() {
        // Le défaut de #2074 : la qualité était annoncée sur l'écran Bandcamp
        // et se perdait au passage en zone. Les TROIS sorties câblées en
        // 0.9.89 portent le même flux source — locale, WAV décodé pour OAAT,
        // proxy MP3 pour un renderer réseau — donc les trois doivent porter
        // le même débit jusqu'au chemin du signal.
        let orch = test_orchestrator();
        let sorties = [
            None,
            Some("local:default".to_string()),
            Some("oaat:endpoint-1".to_string()),
            Some("dlna:uuid-1234".to_string()),
        ];
        assert_eq!(sorties.len(), 4, "quatre sorties examinées");
        for sortie in sorties {
            let req = super::PlayRequest {
                zone_id: 1,
                output_device_id: sortie.clone(),
                track_id: None,
                source: Some("bandcamp".into()),
                source_id: Some(BC_STREAM.into()),
                title: Some("A Track".into()),
                artist_name: None,
                album_title: None,
                cover_url: None,
                duration_ms: Some(212_000),
                seek_ms: None,
                temp_file_path: None,
                sample_rate: None,
                bit_depth: None,
                media_format: None,
                track_number: None,
                disc_number: None,
            };
            let resolved = orch.resolve_direct_url(&req).await.unwrap();
            assert_eq!(
                resolved.bitrate_kbps,
                Some(128),
                "sortie {sortie:?} : le 128 kbit/s doit atteindre la zone"
            );
        }
    }

    #[tokio::test]
    async fn a_purchased_bandcamp_file_is_never_labelled_mp3_128() {
        // Cas de l'ACHAT : la même porte, un autre encodage. Coller
        // « MP3 128 kbit/s » sur un FLAC serait le mensonge inverse.
        let orch = test_orchestrator();
        let req = super::PlayRequest {
            zone_id: 1,
            output_device_id: Some("dlna:uuid-1234".into()),
            track_id: None,
            source: Some("bandcamp".into()),
            source_id: Some("https://popplers5.bandcamp.com/download/track?enc=flac&id=42".into()),
            title: Some("A Track".into()),
            artist_name: None,
            album_title: None,
            cover_url: None,
            duration_ms: Some(212_000),
            seek_ms: None,
            temp_file_path: None,
            sample_rate: None,
            bit_depth: None,
            media_format: None,
            track_number: None,
            disc_number: None,
        };
        let resolved = orch.resolve_direct_url(&req).await.unwrap();
        assert_eq!(
            resolved.bitrate_kbps, None,
            "aucun débit ne doit être annoncé sur un achat lossless"
        );
        assert_eq!(
            resolved.mime_type, "audio/flac",
            "le renderer doit recevoir le vrai type, pas audio/mpeg"
        );
        let stream_id = resolved
            .stream_id
            .as_deref()
            .expect("une sortie réseau passe toujours par le proxy en clair");
        assert!(
            resolved.url.ends_with(&format!("/stream/{stream_id}.flac")),
            "le conteneur servi doit suivre l'encodage réel : {}",
            resolved.url
        );
    }

    /// An output that rejects `play_media` — mirrors an AirPlay renderer whose
    /// ANNOUNCE returns 403 (Bilou, forum #1135).
    struct RejectingOutput {
        id: String,
    }

    #[async_trait::async_trait]
    impl crate::outputs::traits::OutputTarget for RejectingOutput {
        fn name(&self) -> &str {
            "Rejecting"
        }
        fn device_id(&self) -> &str {
            &self.id
        }
        fn output_type(&self) -> &str {
            "test"
        }
        async fn play_media(
            &self,
            _media: &crate::outputs::traits::PlayMedia<'_>,
        ) -> Result<(), String> {
            Err("ANNOUNCE failed: 403".into())
        }
        async fn pause(&self) -> Result<(), String> {
            Ok(())
        }
        async fn resume(&self) -> Result<(), String> {
            Ok(())
        }
        async fn stop(&self) -> Result<(), String> {
            Ok(())
        }
        async fn seek(&self, _position_ms: u64) -> Result<(), String> {
            Ok(())
        }
        async fn set_volume(&self, _volume: f64) -> Result<(), String> {
            Ok(())
        }
        async fn set_mute(&self, _muted: bool) -> Result<(), String> {
            Ok(())
        }
        async fn get_status(&self) -> Result<crate::outputs::traits::OutputStatus, String> {
            Ok(crate::outputs::traits::OutputStatus::default())
        }
        async fn is_available(&self) -> bool {
            true
        }
    }

    /// When the initial output send errors (e.g. AirPlay 403), the zone must
    /// fail fast: `send_to_output` reports the error and the fail-fast branch
    /// flips the zone to Stopped instead of leaving it "Playing" for ~100s
    /// while the poller runs its load-grace clock (Bilou, forum #1135).
    #[tokio::test]
    async fn output_send_error_fails_fast_to_stopped() {
        let orch = test_orchestrator();
        let zone_id = 7;
        let device_id = "airplay-192.168.1.18-7000";

        {
            let mut outputs = orch.outputs.lock().await;
            outputs.register(Box::new(RejectingOutput {
                id: device_id.to_string(),
            }));
        }

        // Prime the zone exactly as play() does before send_to_output.
        let np = NowPlaying {
            title: "So Long".into(),
            duration_ms: 230_050,
            source: "local".into(),
            ..Default::default()
        };
        orch.playback.play(zone_id, np).await;
        assert_eq!(
            orch.playback.get_state(zone_id).await.state,
            PlayState::Playing,
            "zone must be Playing after play() primes it"
        );

        // The rejecting output must report a send failure (not a false success).
        let media = crate::outputs::traits::PlayMedia {
            url: "http://server/stream",
            mime_type: "audio/wav",
            ..Default::default()
        };
        let (output_sent, output_error) = orch
            .send_to_output(device_id, &media, None, false, 1, None)
            .await;
        assert!(
            !output_sent,
            "rejecting output must report output_sent=false"
        );
        assert!(
            output_error.is_some(),
            "rejecting output must surface an error string"
        );

        // Fail-fast reaction (same as play()'s new short-circuit): stop the zone
        // immediately rather than handing it to the poller in a loading state.
        orch.playback.stop(zone_id).await;
        assert_eq!(
            orch.playback.get_state(zone_id).await.state,
            PlayState::Stopped,
            "output send error must leave the zone Stopped, not Playing"
        );
    }
}

/// Plafond de profondeur en sortie (#1610).
///
/// Marantz ND8006 muet, « format non supporte » : le flux partait en 32 bits,
/// valeur lue dans `track.bit_depth` et jamais ramenee a une profondeur que
/// l'appareil sait lire. La regle existait a trois endroits et manquait au
/// quatrieme.
#[cfg(test)]
mod bit_depth_cap_tests {
    use super::cap_output_bit_depth;

    #[test]
    fn le_32_bits_est_ramene_a_24() {
        // Le cas de Jean Valjean : un FLAC annonce en 32 bits.
        assert_eq!(cap_output_bit_depth(32), 24);
    }

    #[test]
    fn les_profondeurs_courantes_passent_intactes() {
        // Ne rien changer pour ceux que ca marchait deja.
        assert_eq!(cap_output_bit_depth(16), 16);
        assert_eq!(cap_output_bit_depth(24), 24);
    }

    #[test]
    fn en_dessous_de_16_on_remonte() {
        // Plancher : sous 16 bits, plus rien ne lit le PCM de facon fiable.
        assert_eq!(cap_output_bit_depth(8), 16);
        assert_eq!(cap_output_bit_depth(1), 16);
        assert_eq!(cap_output_bit_depth(0), 16);
    }

    #[test]
    fn une_valeur_aberrante_reste_jouable() {
        // Une metadonnee fantaisiste ne doit pas produire un flux injouable.
        assert_eq!(cap_output_bit_depth(64), 24);
        assert_eq!(cap_output_bit_depth(u16::MAX), 24);
    }
}

#[cfg(test)]
mod dop_routing_tests {
    use super::{
        AudioFormat, conteneur_a_profondeur_cachee, dop_requested, dop_wire_params,
        profondeur_sondee_si_la_base_ignore,
    };

    /// #1772 — le cas RÉEL de Marco Polo : Wiim Pro (renderer DLNA) relié en
    /// optique à un DAC Denafrips, zone réglée sur « dop ». Avant le correctif,
    /// ce choix n'était comparé nulle part pour une sortie réseau : le DAC
    /// recevait du PCM 176,4 kHz, soit très exactement le débit DoP du DSD64 —
    /// d'où un symptôme qui ressemblait à s'y méprendre à du DoP qui marche.
    #[test]
    fn un_renderer_reseau_regle_sur_dop_recoit_du_dop() {
        assert!(
            dop_requested(false, true, "dop"),
            "le choix explicite « dop » doit être honoré sur un renderer réseau"
        );
    }

    /// La sortie locale ne régresse pas : ses deux modes historiques passent
    /// toujours par le DoP, faute pour une carte son de recevoir du DSD.
    #[test]
    fn la_sortie_locale_gardes_ses_deux_modes() {
        assert!(dop_requested(true, false, "native"));
        assert!(dop_requested(true, false, "dop"));
    }

    /// Le renderer réseau en « natif » ou « auto » ne doit PAS être détourné
    /// vers le DoP : c'est `should_dsd_passthrough` qui arbitre, et lui seul
    /// sait si l'appareil annonce le DSF/DFF.
    #[test]
    fn un_renderer_en_natif_ou_auto_n_est_pas_detourne() {
        assert!(!dop_requested(false, true, "native"));
        assert!(!dop_requested(false, true, "auto"));
        assert!(!dop_requested(false, true, ""));
    }

    /// « pcm » est un refus explicite : il ne produit jamais de DoP, nulle part.
    #[test]
    fn le_mode_pcm_ne_produit_jamais_de_dop() {
        assert!(!dop_requested(true, false, "pcm"));
        assert!(!dop_requested(false, true, "pcm"));
    }

    /// Une zone qui n'est ni locale ni réseau (navigateur, OAAT) ne reçoit pas
    /// de DoP : ces chemins ont leur propre traitement du DSD.
    #[test]
    fn ni_locale_ni_reseau_ne_recoit_rien() {
        assert!(!dop_requested(false, false, "dop"));
        assert!(!dop_requested(false, false, "native"));
    }

    /// #1657 — le mode par DÉFAUT ne produit de DoP nulle part.
    ///
    /// Ce test ne demande pas que « auto » change de comportement : il fixe le
    /// fait, pour que personne ne le redécouvre en cherchant un défaut de
    /// lecture. C'est ce fait, tu et non documenté, qui a fait passer un réglage
    /// par défaut pour un DSD cassé.
    #[test]
    fn le_mode_auto_ne_produit_de_dop_nulle_part() {
        assert!(!dop_requested(true, false, "auto"));
        assert!(!dop_requested(false, true, "auto"));
        assert!(!dop_requested(false, false, "auto"));
        // Et le voisin qui piège tout autant : en RÉSEAU, « natif » non plus.
        assert!(!dop_requested(false, true, "native"));
    }

    /// #1654 — seul l'ALAC cache sa profondeur ; sonder le reste serait de
    /// l'E/S pour rien.
    #[test]
    fn seul_lalac_a_une_profondeur_cachee() {
        assert!(conteneur_a_profondeur_cachee(Some(AudioFormat::Alac)));
        for f in [
            AudioFormat::Flac,
            AudioFormat::Wav,
            AudioFormat::Dsd,
            AudioFormat::Aac,
            AudioFormat::Mp3,
        ] {
            assert!(!conteneur_a_profondeur_cachee(Some(f)), "{f:?}");
        }
        assert!(!conteneur_a_profondeur_cachee(None));
    }

    /// Un fichier illisible ne doit jamais faire échouer la lecture : la sonde
    /// rend `None`, l'appelant garde ce que dit la base.
    #[test]
    fn une_sonde_qui_echoue_laisse_la_base_decider() {
        assert_eq!(
            profondeur_sondee_si_la_base_ignore("/inexistant/x.m4a", Some(AudioFormat::Alac)),
            None
        );
        // Et un conteneur hors périmètre n'est même pas ouvert.
        assert_eq!(
            profondeur_sondee_si_la_base_ignore("/inexistant/x.flac", Some(AudioFormat::Flac)),
            None
        );
    }

    /// #1894 — l'en-tête WAV doit décrire le FICHIER, jamais la ligne `tracks`.
    #[test]
    fn le_fichier_prime_sur_la_base_pour_annoncer_un_flux_dop() {
        // Le cas qui produit du bruit blanc : la base dit stéréo, le fichier
        // est multicanal. Annoncer 2 canaux pour une charge utile qui en porte
        // 5 décale chaque mot de 24 bits et noie le marqueur DoP.
        let (rate, ch) = dop_wire_params(Some((2_822_400, 5)), Some(2_822_400), 2);
        assert_eq!(ch, 5);
        assert_eq!(rate, 2_822_400);

        // Et le cas symétrique : une cadence périmée en base (DSD64 scanné,
        // fichier remplacé par du DSD128) annoncerait un débit DoP faux.
        let (rate, ch) = dop_wire_params(Some((5_644_800, 2)), Some(2_822_400), 2);
        assert_eq!(rate, 5_644_800);
        assert_eq!(ch, 2);
    }

    #[test]
    fn un_entete_dsd_illisible_retombe_sur_la_base_plutot_que_de_refuser() {
        // Mieux vaut diffuser avec des valeurs approximatives que ne rien lire.
        let (rate, ch) = dop_wire_params(None, Some(5_644_800), 2);
        assert_eq!(rate, 5_644_800);
        assert_eq!(ch, 2);

        // Base muette : le défaut DSD64, et jamais moins de deux canaux —
        // un en-tête WAV à 0 canal est injouable partout.
        let (rate, ch) = dop_wire_params(None, None, 0);
        assert_eq!(rate, 2_822_400);
        assert_eq!(ch, 2);
    }

    #[test]
    fn le_plancher_a_deux_canaux_ne_masque_jamais_le_fichier() {
        // `.max(2)` est un plancher, pas un plafond : il ne doit pas rabattre
        // un fichier multicanal — c'était le risque du `track.channels.max(2)`
        // d'origine, qui ignorait le fichier de bout en bout.
        assert_eq!(dop_wire_params(Some((2_822_400, 6)), None, 2).1, 6);
        assert_eq!(dop_wire_params(Some((2_822_400, 1)), None, 2).1, 2);
    }
}

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
mod annonce_apres_sortie_guard {
    /// Le fichier PRIVÉ de ce module de test.
    ///
    /// ⚠️ La découpe n'est pas un détail. `include_str!` rend le fichier
    /// ENTIER, module de test compris — et les motifs cherchés ci-dessous
    /// figurent aussi, mot pour mot, dans les messages d'assertion. Un
    /// `code_de_production().contains(...)` sur le fichier complet se trouve donc lui-même
    /// et rend vrai quoi qu'il arrive.
    ///
    /// C'est vécu : la première version de ce garde-fou a survécu au sabotage
    /// de la condition qu'elle prétendait garder. Un contrôle qui ne peut pas
    /// dire non ne contrôle rien (#2082).
    fn code_de_production() -> &'static str {
        const TOUT: &str = include_str!("orchestrator.rs");
        const BORNE: &str = "mod annonce_apres_sortie_guard";
        let fin = TOUT
            .find(BORNE)
            .unwrap_or_else(|| panic!("ce module a été renommé : la découpe ne protège plus rien"));
        &TOUT[..fin]
    }

    /// Position de la première occurrence, ou panique avec un message qui dit
    /// quoi chercher — un garde-fou muet sur son propre désaccordage ne garde
    /// rien.
    fn position(motif: &str) -> usize {
        code_de_production().find(motif).unwrap_or_else(|| {
            panic!(
                "motif introuvable dans orchestrator.rs : « {motif} ».\n\
                 Le code a été remanié ; ce garde-fou ne garde plus rien tant \
                 qu'il n'a pas suivi. Voir #1998."
            )
        })
    }

    /// `output_sent` doit être CONNU avant qu'on annonce quoi que ce soit.
    #[test]
    fn l_annonce_vient_apres_le_resultat_de_la_sortie() {
        let resultat = position("let (output_sent, output_error) =");
        let annonce = position("self.dispatch_now_playing(");
        assert!(
            resultat < annonce,
            "`dispatch_now_playing` est appelé AVANT que `output_sent` soit \
             connu : une sortie en échec annoncera de nouveau une écoute qui \
             n'a pas eu lieu (#1998)."
        );
    }

    /// Et il doit être CONSULTÉ, pas seulement connu.
    #[test]
    fn l_annonce_est_conditionnee_a_output_sent() {
        assert!(
            code_de_production()
                .contains("if output_sent {\n            self.dispatch_now_playing("),
            "`dispatch_now_playing` n'est plus gardé par `if output_sent` — \
             l'annonce « en écoute » repartirait sur un envoi refusé (#1998)."
        );
    }

    /// L'historique local souffrait du même défaut. C'était la question laissée
    /// ouverte par le ticket ; la réponse est oui, et elle est corrigée ici.
    #[test]
    fn l_historique_local_est_conditionne_lui_aussi() {
        assert!(
            code_de_production().contains("if output_sent && record_history"),
            "`record_listen` n'est plus gardé par `output_sent` : \
             `listen_history` se remplirait de titres jamais joués (#1998)."
        );
    }

    /// Le scrobble DÉFINITIF n'a jamais été concerné — il part du poller, une
    /// fois le seuil des 50 % / 4 min franchi. Ce test épingle cette séparation
    /// pour que personne ne la « répare » en le ramenant au démarrage : c'est
    /// précisément ce que #1113 avait défait.
    #[test]
    fn le_scrobble_definitif_reste_hors_du_demarrage() {
        let play = position("async fn play_inner(");
        let src = code_de_production();
        let apres = &src[play..];
        let fin = apres
            .find("\n    async fn ")
            .or_else(|| apres.find("\n    pub async fn "))
            .unwrap_or(apres.len());
        assert!(
            !apres[..fin].contains("dispatch_scrobble("),
            "le scrobble définitif est reparti dans le chemin de démarrage : \
             il scrobblerait un titre à la seconde où il commence, en ignorant \
             la règle des 50 % / 4 min de Last.fm (#1113)."
        );
    }

    /// Corps d'une méthode, de sa signature jusqu'à son accolade fermante au
    /// même niveau d'indentation. Sert à vérifier une propriété DANS une
    /// fonction sans que le reste du fichier puisse la satisfaire à sa place.
    fn corps_de(signature: &str) -> &'static str {
        let debut = position(signature);
        let apres = &code_de_production()[debut..];
        let fin = apres
            .find("\n    }\n")
            .map(|i| i + 7)
            .unwrap_or(apres.len());
        &apres[..fin]
    }

    /// La zone navigateur n'a PAS de périphérique : `output_sent` y vaut
    /// toujours faux. La garde ci-dessus lui avait donc supprimé toute annonce,
    /// y compris quand elle joue — c'est la régression pour laquelle #1998 a
    /// été rouvert. Son annonce doit être DIFFÉRÉE, pas supprimée.
    #[test]
    fn la_zone_navigateur_ne_perd_pas_son_annonce() {
        assert!(
            code_de_production().contains("if !output_sent && zone_navigateur {"),
            "le démarrage ne met plus rien en attente pour une zone navigateur : \
             elle ne scrobblerait plus RIEN, même en jouant (#1998, réouverture \
             du 22/08). La sortie d'une zone navigateur est l'onglet, pas un \
             appareil."
        );
    }

    /// Et cette annonce différée ne part que sur PREUVE : des octets réellement
    /// tirés de la session de flux. Pas sur l'intention de jouer.
    #[test]
    fn l_annonce_navigateur_suit_la_preuve_de_lecture() {
        let corps = corps_de("pub async fn confirmer_lecture_navigateur(");
        let preuve = corps
            .find(".stream_bytes_sent(stream_id)")
            .unwrap_or_else(|| {
                panic!(
                    "`confirmer_lecture_navigateur` n'interroge plus les octets tirés : \
                 elle annoncerait une écoute de zone navigateur sans preuve, ce que \
                 #1998 reproche au démarrage."
                )
            });
        let annonce = corps.find("self.dispatch_now_playing(").unwrap_or_else(|| {
            panic!("`confirmer_lecture_navigateur` n'annonce plus rien du tout (#1998)")
        });
        assert!(
            preuve < annonce,
            "l'annonce de zone navigateur part AVANT la preuve que l'onglet tire \
             le flux : c'est très exactement le défaut d'origine, déplacé (#1998)."
        );
    }

    /// L'historique local de zone navigateur suit la même preuve, et garde le
    /// drapeau `record_history` du démarrage : une re-création de flux pour une
    /// piste déjà en cours (recherche de position) ne doit pas doublonner.
    #[test]
    fn l_historique_navigateur_garde_record_history() {
        assert!(
            corps_de("pub async fn confirmer_lecture_navigateur(")
                .contains("if attente.record_history && attente.source != \"radio\" {"),
            "l'historique de zone navigateur ne consulte plus `record_history` : \
             déplacer le curseur ajouterait une ligne à chaque fois (#1998)."
        );
    }
}

#[cfg(test)]
mod stop_scope_tests {
    use super::PlaybackOrchestrator;

    /// Le repli de `stop` ne doit JAMAIS toucher l'appareil d'une autre zone.
    ///
    /// Le défaut mesuré sur .18 le 28/08/2026 : la zone 15 « Cet ordinateur »
    /// est une sortie navigateur, donc sans `output_device_id` par
    /// construction. Chaque `next` dessus tombait dans le repli, qui arrêtait
    /// TOUTES les sorties enregistrées — l'Eversolo de la zone 10 compris, en
    /// pleine lecture. Même famille que #2571.
    #[test]
    fn le_repli_de_stop_epargne_les_sorties_des_autres_zones() {
        // Zone 15 : navigateur, aucun appareil. Zones 10 et 8 : renderers.
        let zones = [
            (Some(15i64), None),
            (Some(10i64), Some("uuid:eversolo-dmp-a8")),
            (Some(8i64), Some("uuid:sonos-chambre")),
        ];
        let revendiquees =
            PlaybackOrchestrator::sorties_revendiquees_par_les_autres_zones(zones, 15);

        let enregistrees = vec![
            "uuid:eversolo-dmp-a8".to_string(),
            "uuid:sonos-chambre".to_string(),
            "uuid:orpheline-sans-zone".to_string(),
        ];
        let a_arreter =
            PlaybackOrchestrator::sorties_a_arreter_en_repli(&enregistrees, &revendiquees);

        assert!(
            !a_arreter.contains(&"uuid:eversolo-dmp-a8".to_string()),
            "un stop sur la zone 15 ne doit pas couper l'Eversolo, qui joue la zone 10"
        );
        assert!(
            !a_arreter.contains(&"uuid:sonos-chambre".to_string()),
            "ni le Sonos de la zone 8"
        );
        assert_eq!(
            a_arreter,
            vec!["uuid:orpheline-sans-zone".to_string()],
            "le repli garde son seul objet légitime : une sortie qu'aucune zone ne revendique"
        );
    }

    /// Et la zone qui demande l'arrêt ne s'épargne pas elle-même : si elle a
    /// laissé une sortie ouverte, le repli doit encore pouvoir la fermer.
    #[test]
    fn le_repli_peut_toujours_fermer_la_sortie_de_la_zone_qui_arrete() {
        let zones = [
            (Some(10i64), Some("uuid:eversolo-dmp-a8")),
            (Some(8i64), Some("uuid:sonos-chambre")),
        ];
        let revendiquees =
            PlaybackOrchestrator::sorties_revendiquees_par_les_autres_zones(zones, 10);
        assert!(
            !revendiquees.contains("uuid:eversolo-dmp-a8"),
            "son propre appareil n'est pas « revendiqué ailleurs »"
        );

        let enregistrees = vec![
            "uuid:eversolo-dmp-a8".to_string(),
            "uuid:sonos-chambre".to_string(),
        ];
        let a_arreter =
            PlaybackOrchestrator::sorties_a_arreter_en_repli(&enregistrees, &revendiquees);
        assert_eq!(a_arreter, vec!["uuid:eversolo-dmp-a8".to_string()]);
    }
}
