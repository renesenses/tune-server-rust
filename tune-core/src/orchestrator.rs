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

/// Plafond absolu du budget de transcodage : 30 minutes.
///
/// C'est la même valeur que le plafond de `transcode_budget_for`, reprise ici
/// parce qu'elle borne AUSSI le budget adaptatif : un hôte trop lent doit
/// finir par rendre la main, et un budget infini n'est pas une réponse. À ce
/// plafond, une piste de durée `D` n'aboutit que si l'hôte décode au moins à
/// `D / 1800` fois le temps réel — soit `× 1,0` pour une piste de 30 minutes.
/// En dessous, la piste est refusée VITE et pour une raison NOMMÉE, ce qui vaut
/// mieux qu'un silence.
const PLAFOND_BUDGET_TRANSCODAGE: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Marge appliquée au temps de décodage restant estimé.
///
/// Le facteur temps réel est mesuré sur la boucle de décodage seule. Deux
/// choses lui échappent, et elles ont été MESURÉES sur Shrek (DSD256 stéréo de
/// 60 s, profil release) :
///
/// | poste | mesuré |
/// |---|---|
/// | décodage (ce que la balise voit) | 26,12 s |
/// | `pcm_bytes` + 24→16 bits + WAV + écriture | **0,59 s, soit +2,3 %** |
/// | dispersion du facteur d'un bout à l'autre du fichier | **±1,5 %** |
///
/// Quatre pour cent suffiraient donc au poste mesuré. Les vingt retenus
/// laissent seize points de réserve pour ce que ce banc ne pouvait PAS
/// mesurer : la CONCURRENCE sur l'hôte — une autre zone qui joue, une passe de
/// scan, un enrichissement — qui ne change pas le travail à faire mais partage
/// les cœurs qui le font. Ces seize points ne sont pas mesurés ; ils sont
/// assumés, et ils ne coûtent qu'à un transcodage qui allait de toute façon
/// échouer.
const MARGE_BUDGET_TRANSCODAGE: f64 = 1.20;

/// Pas de sondage de la balise d'avancement.
///
/// Il fixe aussi la granularité du dépassement : le budget peut être franchi
/// d'au plus un pas. Sur un budget qui vaut au minimum 120 s, un quart de
/// seconde ne se voit pas.
const PAS_SONDAGE_BUDGET: std::time::Duration = std::time::Duration::from_millis(250);

/// Nombre de sondages consécutifs montrant de l'avancement avant de croire la
/// mesure.
///
/// **Combien de fenêtres faut-il ? La question a été mesurée, pas supposée.**
/// Décodage DSD256 réel sur Shrek, balise échantillonnée toutes les 10 ms,
/// facteur estimé après *k* secondes d'audio, comparé au facteur final :
///
/// | audio décodé | horloge | facteur | écart au final |
/// |---|---|---|---|
/// | 0,25 s | 0,111 s | × 2,26 | **+0,3 %** |
/// | 1,0 s | 0,445 s | × 2,29 | +1,4 % |
/// | 5,0 s | 2,20 s | × 2,27 | +0,7 % |
/// | 30 s | 13,1 s | × 2,28 | +1,2 % |
/// | *final (60 s)* | 26,6 s | **× 2,26** | — |
///
/// L'estimation est donc stable à ±1,5 % **dès la PREMIÈRE fenêtre** : sur un
/// fichier local, le coût fixe du démarrage (ouverture, conception du filtre
/// FIR de décimation) est déjà amorti au bout de 110 ms.
///
/// Huit sondages — deux secondes d'horloge, soit ~4,5 s d'audio DSD256 sur
/// Shrek — sont donc largement au-delà de ce que la stabilité exige. Cette
/// marge n'est pas pour le fichier local mesuré ici : elle est pour le fichier
/// SUR UN NAS, dont les premières fenêtres sont bornées par la recopie locale
/// (`stage_locally_for_decode`) et non par le processeur. Deux secondes ne
/// coûtent rien sur un budget qui vaut au minimum 120 s.
///
/// Et ce cas-là se trompe dans le SENS SÛR : un début lent sous-estime le
/// facteur, donc surestime le besoin, donc élargit le budget.
const SONDAGES_AVANT_MESURE: usize = 8;

/// La politique de budget de #3140 : le temps accordé suit le DÉBIT DE DÉCODAGE
/// mesuré sur l'hôte, plus seulement la taille du fichier.
///
/// ## Le défaut corrigé
///
/// `transcode_budget_for` accorde `120 + 120·G` secondes pour `G` gibioctets.
/// En DSD256 stéréo cela vaut `120 + 0,3154·D` pour `D` secondes d'audio, ce
/// qui n'est tenable que si l'hôte décode à `× 3,17` temps réel. Shrek mesure
/// `× 2,2` : toute piste DSD256 de plus de ~14,4 min y expirait, donc silence.
/// Et Tune livre un binaire ARM64, un `.deb` arm64 et une image Docker arm64 —
/// les hôtes plus lents encore sont une population livrée.
///
/// ## La règle
///
/// Le budget effectif ne fait que **s'ÉTENDRE**, jamais se resserrer :
///
/// ```text
/// budget = min(plafond, max(budget_taille, écoulé + restant/R × marge))
/// ```
///
/// C'est ce qui rend le correctif invisible pour tout le monde sauf ceux qui
/// rencontraient le silence. Quiconque aboutissait sous l'ancienne règle avait
/// `budget_taille ≥ besoin` ; le nouveau budget est `≥ budget_taille` : son
/// transcodage se déroule EXACTEMENT comme avant, à la même seconde près, et
/// aucune ligne de journal supplémentaire n'est émise puisque le budget n'a pas
/// bougé. Resserrer aurait demandé de faire confiance à une mesure faite sur
/// les premières fenêtres d'un fichier — et un cache froid, une recopie NAS ou
/// un cœur momentanément pris sous-estiment ce débit-là.
#[derive(Debug, Clone, Copy)]
struct BudgetAdaptatif {
    /// Durée de la piste, en secondes. `0` quand la base ne la connaît pas —
    /// on ne peut alors rien extrapoler et le budget historique s'applique.
    piste_s: f64,
    /// Le budget historique, indexé sur la taille. Plancher indéplaçable.
    budget_taille: std::time::Duration,
    plafond: std::time::Duration,
}

/// Ce que la politique conclut d'une observation de la balise.
#[derive(Debug, Clone, Copy, PartialEq)]
enum VerdictBudget {
    /// Rien de mesurable : durée de piste inconnue, décodeur qui ne publie pas,
    /// ou pas encore assez de fenêtres. Le budget historique reste seul en jeu.
    PasEncore,
    /// Débit mesuré sur l'hôte, et budget total qui en découle.
    Mesure {
        facteur: f64,
        budget: std::time::Duration,
    },
}

impl BudgetAdaptatif {
    fn new(piste_s: f64, budget_taille: std::time::Duration) -> Self {
        Self {
            piste_s,
            budget_taille,
            plafond: PLAFOND_BUDGET_TRANSCODAGE,
        }
    }

    /// `ecoule` : temps depuis le début du transcodage. `decode` : audio déjà
    /// sorti du décodeur. `sondages` : combien de sondages consécutifs ont vu
    /// de l'avancement.
    fn observer(
        &self,
        ecoule: std::time::Duration,
        decode: std::time::Duration,
        sondages: usize,
    ) -> VerdictBudget {
        let ecoule_s = ecoule.as_secs_f64();
        let decode_s = decode.as_secs_f64();
        if self.piste_s <= 0.0
            || sondages < SONDAGES_AVANT_MESURE
            || decode_s <= 0.0
            || ecoule_s <= 0.0
        {
            return VerdictBudget::PasEncore;
        }
        let facteur = decode_s / ecoule_s;
        if !facteur.is_finite() || facteur <= 0.0 {
            return VerdictBudget::PasEncore;
        }
        let restant_s = (self.piste_s - decode_s).max(0.0);
        let besoin_s = ecoule_s + restant_s / facteur * MARGE_BUDGET_TRANSCODAGE;
        // `besoin_s` peut déborder si le facteur est infime ; le plafond le
        // rattrape, mais `from_secs_f64` panique sur un flottant hors bornes.
        let besoin = if besoin_s.is_finite() && besoin_s < self.plafond.as_secs_f64() {
            std::time::Duration::from_secs_f64(besoin_s)
        } else {
            self.plafond
        };
        VerdictBudget::Mesure {
            facteur,
            budget: besoin.max(self.budget_taille).min(self.plafond),
        }
    }
}

/// Ce qu'on sait au moment où le budget est épuisé — de quoi NOMMER la cause.
///
/// Avant #3140 le journal n'annonçait qu'un délai dépassé et la taille du
/// fichier, ce qui envoyait chercher du côté du disque ou du réseau alors que
/// la cause était la vitesse du processeur.
#[derive(Debug, Clone, Copy)]
struct DepassementBudget {
    budget: std::time::Duration,
    ecoule: std::time::Duration,
    decode: std::time::Duration,
    /// Facteur temps réel mesuré sur cet hôte, si la balise a parlé.
    facteur: Option<f64>,
    piste_s: f64,
}

impl DepassementBudget {
    /// Le facteur temps réel qu'il aurait fallu pour tenir dans ce budget.
    fn facteur_requis(&self) -> Option<f64> {
        let budget_s = self.budget.as_secs_f64();
        (self.piste_s > 0.0 && budget_s > 0.0).then(|| self.piste_s / budget_s)
    }
}

/// Surveille `travail` sous un budget qui s'étend selon le débit mesuré.
///
/// Remplace le `tokio::time::timeout` fixe. L'horloge est celle de tokio
/// (`tokio::time::Instant`), pas `std::time::Instant` : c'est ce qui rend la
/// contre-épreuve possible sans AUCUN `sleep` réel — un test sous
/// `#[tokio::test(start_paused = true)]` avance cette horloge virtuellement.
async fn transcoder_sous_budget<F, T>(
    travail: F,
    progres: std::sync::Arc<crate::audio::decode_progress::DecodeProgress>,
    politique: BudgetAdaptatif,
    pas: std::time::Duration,
    journal: Option<&str>,
) -> Result<T, DepassementBudget>
where
    F: std::future::Future<Output = T>,
{
    let debut = tokio::time::Instant::now();
    let mut budget = politique.budget_taille;
    let mut facteur: Option<f64> = None;
    let mut sondages = 0usize;
    let mut annonce = false;
    tokio::pin!(travail);
    loop {
        tokio::select! {
            resultat = &mut travail => return Ok(resultat),
            _ = tokio::time::sleep(pas) => {}
        }
        let ecoule = debut.elapsed();
        let decode = std::time::Duration::from_millis(progres.decoded_ms());
        if decode > std::time::Duration::ZERO {
            sondages += 1;
        }
        if let VerdictBudget::Mesure {
            facteur: f,
            budget: b,
        } = politique.observer(ecoule, decode, sondages)
        {
            facteur = Some(f);
            if b > budget {
                // Une seule ligne, à la première extension : la suivre à chaque
                // sondage remplirait le journal d'un tour par quart de seconde.
                if !annonce {
                    if let Some(file) = journal {
                        info!(
                            file,
                            budget_s_initial = politique.budget_taille.as_secs(),
                            budget_s = b.as_secs(),
                            host_realtime_factor = f,
                            track_s = politique.piste_s,
                            "transcode_budget_extended_slow_host"
                        );
                    }
                    annonce = true;
                }
                budget = b;
            }
        }
        if ecoule >= budget {
            return Err(DepassementBudget {
                budget,
                ecoule,
                decode,
                facteur,
                piste_s: politique.piste_s,
            });
        }
    }
}

#[allow(clippy::too_many_arguments)]
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
    progres: Option<std::sync::Arc<crate::audio::decode_progress::DecodeProgress>>,
) -> Result<(u64, Vec<u8>, u16), String> {
    // 1. Decode source to PCM (blocking I/O).
    let decoded = tokio::task::spawn_blocking(move || {
        // La balise se pose SUR CE THREAD : c'est lui qui décode (#3140).
        // Sans balise (`None`, tous les appelants hors chemin de lecture), le
        // décodage est strictement celui d'avant.
        let _balise = progres.map(crate::audio::decode_progress::installer);
        crate::audio::decode::decode_to_pcm(&source, Some(out_sr), Some(channels as u32), 0.0, 0.0)
    })
    .await
    .map_err(|e| format!("decode task panic: {e}"))??;

    let mut pcm_bytes = decoded.pcm_bytes();
    let mut actual_bd = decoded.bit_depth;

    // 1a. Porter le PCM À la profondeur négociée — dans LES DEUX SENS.
    //
    // Cette étape ne descendait que (`target_bd < actual_bd`). Une cible plus
    // PROFONDE que la source était donc ignorée en silence : le fichier écrit
    // gardait la largeur de la source, tandis que `StreamInfo` — et par lui le
    // `<res bitsPerSample>` du DIDL, puis le choix du profil `DLNA.ORG_PN` —
    // annonçait la cible. Un renderer qui suit ce qu'on lui déclare lit alors
    // des échantillons de deux octets à un pas de trois : la famille #1137,
    // celle qui rend du silence ou du bruit sans jamais rien dire. C'est ce que
    // fait `dlna_wav24` dès que la base annonce la source plus profonde qu'elle
    // n'est — le réglage « Forcer le WAV » de Yves (#1437).
    //
    // `container_bit_depth` borne d'abord la cible à ce que la chaîne sait
    // ÉCRIRE : 16, 24 ou 32. Une source de 20 bits — légale en ALAC, en FLAC,
    // en AIFF et en WavPack — donnait `out_bd = 20` (`cap_output_bit_depth` ne
    // borne qu'à 16..24), et le transcodage échouait APRÈS le décodage complet :
    // `encode_wav` rend « unsupported bit depth: 20 », `pcm_to_i32` la même chose
    // pour le FLAC. La piste ne démarrait jamais.
    let target_bd = crate::audio::decode::container_bit_depth(target_bd);
    if target_bd != actual_bd {
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

/// Le producteur d'une session-CANAL vient de mourir sans avoir ecrit un seul
/// octet : la session doit DISPARAITRE, pas seulement se taire.
///
/// `StreamSession::new` garde un `_keep_alive_tx` clone pour toute la vie de
/// la session : laisser tomber le `tx` du producteur ne ferme PAS le canal.
/// Sans ce retrait, la session reste inscrite sans producteur, son corps HTTP
/// ne se termine jamais, et le pre-chargement gapless s'y enchaine — la sortie
/// locale tire un flux qui ne debitera jamais rien et la lecture se fige
/// jusqu'au Stop (#3287, Gros Bidon, Qobuz en USB, 03/09).
///
/// `AudioStreamer::remove_session` ferme le canal AVANT de retirer l'entree :
/// un consommateur deja attache voit un EOF franc au lieu d'attendre a jamais,
/// et `/stream/{id}` repond ensuite 404 plutot que de pendre. C'est aussi ce
/// qui rend la mort LISIBLE : `session_alive` devient faux, et c'est le seul
/// signal qui distingue « pas encore » de « plus jamais » — `data_ready` ne
/// sait dire que le premier. Le poller s'en sert pour ne plus armer `SetNext`
/// sur un flux mort.
async fn abandonner_la_session_de_transcodage(
    streamer: &crate::http::streamer::AudioStreamer,
    session_id: &str,
    tmp_path: &str,
) {
    let _ = std::fs::remove_file(tmp_path);
    streamer.remove_session(session_id).await;
    warn!(
        stream_id = session_id,
        "streaming_session_abandonnee_producteur_mort"
    );
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

            // #2395 — AUCUNE commande de volume n'est envoyée ici, pour aucune
            // zone.
            //
            // Deux comportements sont morts à cet endroit, et pour deux
            // raisons distinctes :
            //
            // 1. Zone ordinaire : Tune poussait le volume stocké à CHAQUE
            //    lecture, écrasant le niveau réglé directement sur l'appareil
            //    — la valeur stockée dérive (rien ne resynchronise depuis le
            //    device), et un appareil laissé bas remontait aux 50 % stockés
            //    (Fabien, « Salon »). Retiré avant #2395 ; la zone garde le
            //    niveau où elle est physiquement.
            //
            // 2. Zone `fixed_volume` : le plein volume était RÉASSERTÉ 500 ms
            //    après chaque piste (`play_initial_volume_sent`). C'est la
            //    part du mode que l'utilisateur n'a jamais consentie : il a
            //    accepté UN saut à l'armement, pas une commande à 100 %
            //    renvoyée à chaque morceau. Sur un renderer qui porte son
            //    propre ampli — Denon RC12, Marco Polo, fil 1546 — chacune de
            //    ces commandes est une puissance acoustique réelle.
            //
            // Le mode bit-perfect reste entier : le 100 % est commandé UNE
            // fois, à l'armement, derrière la confirmation explicite
            // (`fixed_volume_confirmation_required`, routes/zones.rs), et il
            // est rendu au désarmement (`audio::fixed_volume`).
            //
            // Contrepartie assumée : une commande extérieure qui rebaisse le
            // renderer EN COURS de session casse le bit-perfect sans que rien
            // ne le dise. C'est le prix du choix — un saut annoncé et
            // réversible — et non un défaut à rattraper par une réassertion.

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
            // Le rang vient de la file de la zone, PAS du contexte : c'est la
            // position reellement atteinte, avance automatique comprise. En
            // aleatoire il reste vide — on re-tirera (#2441).
            let rang = rang_a_retenir(etat.shuffle, etat.queue_position);
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
                ContexteEcoute {
                    nature: context.0.as_deref(),
                    id: context.1.as_deref(),
                    rang,
                },
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
            // …et le DIRE, pas seulement l'écrire dans le journal du serveur.
            //
            // Ce bloc CONNAÎT la cause — le renderer vient de la donner — puis il
            // la range dans `PlayResult.error` et rend `Ok`. Or `Ok` est ce que
            // presque tous les appelants lisent comme un succès : le corps de la
            // réponse n'est relu que par les branches HTTP qui ATTENDENT `play()`
            // (`POST /zones/:id/play` et ses voisines, via
            // `build_zone_json_with_result`).
            //
            // Partout ailleurs la cause mourait ici : `next` et `previous`
            // répondent `{"status":"playing"}` depuis un `tokio::spawn` avant même
            // que l'envoi soit tenté (routes/playback.rs) ; `resume` et `seek`
            // rendent `Ok(())` ; l'avance automatique, l'autoplay et la relance
            // de démarrage mort du sondeur écrivent `Ok(_) =>` et poursuivent
            // comme si la piste avait démarré ; idem pour les alarmes, la reprise
            // au démarrage, le transfert de zone et l'émulation de renderer UPnP.
            // Vu de l'auditeur : la zone dit « en lecture », rien ne part, et rien
            // ne dit pourquoi.
            //
            // `zone.playback_error` existe déjà pour six autres échecs de lecture
            // (périphérique disparu, décodage, volume, radio…) et le serveur le
            // pousse verbatim à tous les clients (`routes/ws.rs`). Le REFUS D'UNE
            // SORTIE était le seul échec de lecture à ne pas s'en servir.
            //
            // `fatal` pour la même raison qu'en #2630 : la zone s'arrête quinze
            // lignes plus bas, et sans ce drapeau la fenêtre de grâce
            // d'après-lecture du client avalerait le message — l'utilisateur
            // n'aurait, une fois de plus, que le silence.
            //
            // Coût nul sur une lecture qui démarre : on n'entre dans ce bloc que
            // lorsqu'une sortie a EXPLICITEMENT refusé le flux.
            if let Some(ref bus) = self.event_bus {
                bus.emit(
                    "zone.playback_error",
                    serde_json::json!({
                        "zone_id": req.zone_id,
                        "error": output_error.as_deref().unwrap_or_default(),
                        "fatal": true,
                    }),
                );
            }
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

    // `radio_head_ok` vivait ici : un HEAD sur la station décidait s'il fallait
    // la proxifier pour un renderer DLNA. Le sondage a été ABANDONNÉ par
    // « always proxy Icecast radio to DLNA renderers » (#537) : une station liée
    // à un renderer est désormais toujours transcodée en WAV, y compris un .mp3
    // explicite dont le HEAD rend 200 (Cyrille, Yamaha R-N2000A — TSF Jazz
    // envoyée en direct restait muette). `needs_proxy`, plus bas, ne consulte
    // donc plus le réseau du tout, et la fonction est restée sans appelant.
    // Retirée plutôt que rebranchée : la rebrancher rejouerait le défaut.

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
    /// (no network hit — cheap extension gate first), for an HLS `.m3u8`
    /// manifest, or on any fetch/parse failure — so the caller keeps the
    /// original URL.
    ///
    /// `.m3u8` est écarté du déréférencement, et ce n'est PAS parce que « le
    /// manifeste EST le flux, consommé directement par le lecteur » — ce que
    /// cette phrase affirmait jusqu'à #2307, en annonçant une capacité qui
    /// n'a jamais existé. Tune n'a aucun chargeur de segments HLS : le seul
    /// lecteur de flux radio est `decode_radio_stream_to_pcm`, un GET unique
    /// poussé dans symphonia. Et déréférencer ne servirait à rien non plus :
    /// une playlist HLS ne porte que des chemins de segments RELATIFS, donc
    /// le filtre `http(s)` ci-dessous n'y trouverait rien à rendre (constat
    /// déjà écrit noir sur blanc dans la migration 86, qui a retiré BBC
    /// Radio 3 pour cette raison). La garde reste donc telle quelle ; c'est
    /// la LECTURE qui refuse HLS, en le NOMMANT — voir
    /// [`RADIO_HLS_UNSUPPORTED`].
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
                            // #3287 : ces deux sorties quittaient la tache
                            // AVANT le `end_session_input` du bas, en se
                            // contentant d'effacer le fichier temporaire. La
                            // session restait donc inscrite, sans producteur,
                            // canal ouvert — et le gapless s'y enchainait.
                            abandonner_la_session_de_transcodage(
                                &streamer_for_eof,
                                &session_id_for_eof,
                                &tmp_path,
                            )
                            .await;
                            return;
                        }
                        Err(e) => {
                            warn!(error = %e, "streaming_transcode_task_join_failed");
                            abandonner_la_session_de_transcodage(
                                &streamer_for_eof,
                                &session_id_for_eof,
                                &tmp_path,
                            )
                            .await;
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

            // Chaîne DSP de la zone, chargée UNE fois et réutilisée par la
            // décision de cache chaud ET par le transcodage ci-dessous. Un
            // second chargement pourrait observer un traitement tout juste
            // activé et ranger un transcodage traité sous la clé du flux brut,
            // empoisonnant tous les accès ultérieurs à cette piste.
            //
            // ⚠️ Ce bras ne chargeait que l'ÉGALISEUR (#2863) : le convolveur de
            // correction de pièce et le ReplayGain y étaient perdus, exactement
            // comme sur les bras non-DASH. `StreamingDsp` porte les trois.
            let mut dash_dsp = self.load_streaming_dsp(
                req.zone_id,
                req.track_id,
                stream_data.quality.sample_rate,
                2,
            );
            let dash_dsp_active = dash_dsp.is_active();

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
                // Le traitement de zone n'entre PAS dans la clé de cache : un
                // flux traité ne peut donc jamais partager la clé d'un flux
                // brut. La garde ne couvrait que l'égaliseur ; le convolveur et
                // le ReplayGain la traversaient (#2863).
                if !dash_dsp_active {
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
                && !dash_dsp_active
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
                    && !dash_dsp_active
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

                // ReplayGain, égaliseur, puis convolveur FIR — l'ordre de
                // `transcode_source_to_file`. Ce bras n'appliquait que le
                // deuxième (#2863).
                dash_dsp.process(&mut pcm_bytes, actual_bd);

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
                // Chaîne DSP de la zone (#2863). Ce bras servait le PCM décodé
                // TEL QUEL : égaliseur, convolveur et ReplayGain y étaient
                // calculés côté interface puis jetés. Le relais les applique au
                // fil de l'eau, sans rien bufferiser de plus — le démarrage
                // immédiat conquis en 0.9.106 est préservé. Sans traitement
                // actif, le canal reste celui d'avant, à l'octet près.
                let aac_dsp = self.load_streaming_dsp(req.zone_id, req.track_id, sr, 2);
                let tx = if aac_dsp.is_active() {
                    info!(
                        zone_id = req.zone_id,
                        "streaming_aac_channel_dsp_relay_inserted"
                    );
                    spawn_streaming_dsp_relay(aac_dsp, bd, true, tx)
                } else {
                    tx
                };
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

                // Chaîne DSP de la zone (#2863). Le bras « proxy verbatim »
                // ci-dessous ne décode RIEN : il relaie les octets du CDN. Un
                // égaliseur, une correction de pièce ou un ReplayGain armés y
                // étaient donc calculés puis jetés, sans une ligne de journal —
                // « je règle mon égaliseur, j'écoute du Qobuz sur ma zone
                // réseau, et je n'entends aucune différence ».
                let sr = stream_data.quality.sample_rate;
                let mut https_dsp = self.load_streaming_dsp(req.zone_id, req.track_id, sr, 2);
                let https_dsp_active = https_dsp.is_active();

                if streaming_needs_pretranscode(renderer_supports_mime, https_dsp_active) {
                    // Deux raisons d'arriver ici : le renderer ne sait pas lire
                    // le FLAC (→ WAV/LPCM), ou un traitement doit entrer dans le
                    // signal (→ FLAC pleine profondeur, le renderer sait le
                    // lire). Même schéma que le pré-transcodage AAC :
                    // téléchargement → décodage → traitement → encodage →
                    // session fichier (Content-Length, pas de chunked).
                    let bd = stream_data.quality.bit_depth.max(16).min(24);
                    let enc_format = streaming_pretranscode_format(renderer_supports_mime);
                    let enc_is_wav = enc_format == "wav";

                    info!(
                        service = service_name,
                        codec = %codec_lower,
                        device = %device_id,
                        sample_rate = sr,
                        bit_depth = bd,
                        enc_format,
                        dsp_active = https_dsp_active,
                        renderer_supports_mime,
                        "streaming_pretranscode_for_renderer_or_dsp"
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
                        .join(format!(
                            "tune-stream-pretranscode-{}.{}",
                            uuid::Uuid::new_v4(),
                            enc_format
                        ))
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

                        // Plafond 16 bits : UNIQUEMENT quand on retombe sur
                        // WAV/LPCM. Le renderer a rejeté le FLAC, on sert donc
                        // sous `DLNA.ORG_PN=LPCM`, un profil 16 bits seulement —
                        // un Hi-Res 24 bits servi dessous joue du SILENCE sur un
                        // Ruark R3 / LHC-62 (Yves, #1137).
                        //
                        // Quand c'est le TRAITEMENT qui impose le
                        // pré-transcodage (#2863), le renderer sait lire le
                        // FLAC : on ré-encode en FLAC pleine profondeur, et le
                        // plafond n'a pas lieu d'être. Sans cette distinction,
                        // armer un égaliseur ferait tomber tout le Hi-Res Qobuz
                        // à 16 bits — une dégradation jamais demandée.
                        if enc_is_wav && actual_bd > 16 {
                            pcm_bytes =
                                crate::audio::decode::convert_pcm_bytes(&pcm_bytes, actual_bd, 16);
                            actual_bd = 16;
                        }

                        // ReplayGain, égaliseur, puis convolveur FIR — l'ordre
                        // de `transcode_source_to_file`. Sans traitement actif,
                        // `pcm_bytes` n'est pas touché d'un octet.
                        https_dsp.process(&mut pcm_bytes, actual_bd);

                        // Niveaux post-traitement : les VU décrivent ce qui sera
                        // entendu, comme sur le bras DASH.
                        if let Some(ref ltx) = wav_levels_tx {
                            crate::audio::tap::send_windowed_pcm(
                                ltx,
                                &pcm_bytes,
                                actual_bd,
                                actual_ch as u16,
                                actual_sr,
                            );
                        }

                        // 3. Encode
                        let rt = tokio::runtime::Handle::try_current()
                            .map_err(|e| format!("no tokio runtime: {e}"))?;
                        let encoded_data = rt.block_on(async {
                            let mut encoder = crate::audio::encoder::AudioEncoder::new(
                                enc_format,
                                actual_sr,
                                actual_bd as u32,
                                actual_ch,
                            );
                            encoder.start().await?;
                            encoder.write(&pcm_bytes).await?;
                            encoder.finish().await
                        })?;

                        std::fs::write(&tmp_wav_clone, &encoded_data)
                            .map_err(|e| format!("write pre-transcode: {e}"))?;

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
                                enc_format,
                                "streaming_pretranscode_complete"
                            );

                            let enc_mime = if enc_is_wav {
                                "audio/wav"
                            } else {
                                "audio/flac"
                            };
                            let file_info = StreamInfo {
                                format: enc_format.into(),
                                mime_type: enc_mime.into(),
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
                            let url =
                                self.streamer
                                    .get_stream_url(&session_id, &server_ip, enc_format);
                            (url, Some(session_id), enc_mime.to_string(), Some(file_size))
                        }
                        Ok(Err(e)) => {
                            warn!(error = %e, "streaming_pretranscode_failed");
                            let _ = std::fs::remove_file(&tmp_dl);
                            let _ = std::fs::remove_file(&tmp_wav);
                            return Err(format!("streaming pre-transcode failed: {e}"));
                        }
                        Err(e) => {
                            warn!(error = %e, "streaming_pretranscode_task_panic");
                            let _ = std::fs::remove_file(&tmp_dl);
                            let _ = std::fs::remove_file(&tmp_wav);
                            return Err(format!("streaming pre-transcode task panic: {e}"));
                        }
                    }
                } else {
                    // Renderer supports FLAC et AUCUN traitement actif — proxy
                    // direct, octets du CDN verbatim, bit-perfect. C'est le
                    // chemin de l'immense majorité des écoutes, et il ne bouge
                    // pas d'un octet.
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

    /// Les deux réglages qu'une sortie locale DOIT porter, lus là où la page
    /// de réglages les écrit : la base.
    ///
    /// #1770 (point 3) — la sortie reconstruite à la volée par
    /// [`Self::recreate_local_and_play`] les codait en dur (`false`,
    /// `"auto"`). Conséquence, sur Windows et macOS — les seules plateformes
    /// qui ont un chemin exclusif ([`crate::outputs::local`],
    /// `exclusive_mode_support`) : un DAC éteint au démarrage, ou retiré par
    /// le balayage à chaud, repartait au PREMIER appui sur Lecture en mode
    /// partagé et jamais en ASIO — `select_host("auto")` ne sonde même pas
    /// ASIO — alors que les réglages disaient le contraire, et sans que
    /// l'écran le trahisse (`display_audio_backend` rend la valeur VOULUE).
    ///
    /// La chaîne est celle de `AppState::effective_audio_backend` /
    /// `effective_exclusive_mode`, MOINS le fichier de configuration, qui
    /// n'existe que dans `tune-server` et n'est pas atteignable d'ici : la
    /// base d'abord, l'environnement en repli, puis les valeurs par défaut de
    /// [`crate::config::TuneConfig`] — c'est-à-dire exactement ce que le
    /// codage en dur rendait, et jamais moins.
    pub fn reglages_sortie_locale(&self) -> (bool, String) {
        self.reglages_sortie_locale_avec(|cle| std::env::var(cle).ok())
    }

    /// Même résolution, avec l'environnement en PARAMÈTRE.
    ///
    /// Même intention que [`crate::config::resolve_local_audio_backend`] : la
    /// règle doit être éprouvable sans dépendre des variables de la machine
    /// qui l'éprouve — sinon l'essai serait vert ou rouge selon le `.env` du
    /// runner, et `std::env::set_var` dans un test casse la suite entière.
    pub fn reglages_sortie_locale_avec<F>(&self, environnement: F) -> (bool, String)
    where
        F: Fn(&str) -> Option<String>,
    {
        let reglages = crate::db::settings_repo::SettingsRepo::with_backend(self.db.clone());
        let backend = reglages
            .get("local_audio_backend")
            .ok()
            .flatten()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| crate::config::resolve_local_audio_backend(&environnement))
            .unwrap_or_else(|| "auto".to_string());
        let demande = reglages
            .get("local_exclusive_mode")
            .ok()
            .flatten()
            .map(|v| matches!(v.trim().to_lowercase().as_str(), "true" | "1" | "yes"))
            .unwrap_or_else(|| {
                environnement("TUNE_LOCAL_EXCLUSIVE_MODE")
                    .map(|v| matches!(v.trim().to_lowercase().as_str(), "true" | "1" | "yes"))
                    .unwrap_or(false)
            });
        // ASIO est exclusif par nature, et c'est une notion Windows (#1268,
        // #3192). La règle vit dans `tune-core::config` et n'est pas recopiée
        // ici : une seule écriture pour les deux chemins.
        let effectif = crate::config::local_exclusive_mode_status(&backend, demande).effective;
        (effectif, backend)
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
        // Les réglages, pas des littéraux : voir `reglages_sortie_locale`
        // (#1770). `endpoint_id` et `origin_host` restent absents — les deux
        // ne viennent QUE d'une énumération de périphériques
        // (`AudioDevice::endpoint_id` / `.backend`), rien ne les persiste, et
        // ce chemin n'en a pas : il existe précisément parce que le
        // périphérique n'est PAS énumérable à cet instant. Les inventer serait
        // pire que de les laisser vides.
        let (exclusive_mode, audio_backend) = self.reglages_sortie_locale();
        info!(
            device_id,
            exclusive_mode,
            audio_backend = %audio_backend,
            "output_not_found_recreating_local_output"
        );
        let local_out = crate::outputs::local::LocalOutput::with_options(
            device_name.to_string(),
            exclusive_mode,
            &audio_backend,
        );
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
                    // Rampe anti-« ploc » à la pause / reprise / arrêt (#1590),
                    // sortie LOCALE uniquement — voir `zone_soft_mute_ms` pour
                    // les sorties qui restent nues et pourquoi.
                    local_output.set_soft_mute_ms(self.zone_soft_mute_ms(zone_id));
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

    /// True when the zone is in PURE (audiophile) mode: bypass ALL per-zone
    /// signal processing for a bit-perfect path — the equalizer, its
    /// room-correction gains (in `load_eq_processor`) and the room-correction
    /// convolver (in the local output). Bertrand: "PURE doit désactiver toutes
    /// les modifs".
    fn zone_audiophile(&self, zone_id: i64) -> bool {
        crate::audio::audiophile::zone_enabled(&self.db, zone_id)
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

        let zone_row = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten();
        let output_type = zone_row.as_ref().and_then(|z| z.output_type.clone());
        let output_device_id = zone_row.and_then(|z| z.output_device_id);

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
            output_device_id: output_device_id.clone(),
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
                // Le flux est neuf. Sur une sortie réseau servie par une
                // session fichier/proxy, il repart de l'OCTET 0 : sans ce
                // Seek, le renderer joue le morceau depuis le début (#2893).
                self.seek_output_after_replay(
                    zone_id,
                    output_device_id.as_deref(),
                    output_type.as_deref(),
                    position_ms,
                )
                .await;
                info!(zone_id, position_ms, raison, "zone_replayed_at_position");
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        }
    }

    /// Envoyer à la sortie le `Seek` qui manque après une recréation de flux
    /// (#2893). Voir [`replay_needs_output_seek`] pour la règle de décision et
    /// la raison pour laquelle elle n'est ni « toujours », ni « jamais ».
    ///
    /// **Au mieux, jamais fatal** : la relecture, elle, a réussi — le
    /// changement de traitement est bien passé dans le signal. Un renderer qui
    /// refuse le `Seek` laisse un morceau qui repart du début, ce qui est
    /// exactement l'ancien comportement ; le faire remonter en `Err`
    /// transformerait cette dégradation en `eq_replay_failed`, et empêcherait
    /// au passage d'armer le plancher anti-rafale.
    async fn seek_output_after_replay(
        &self,
        zone_id: i64,
        device_id: Option<&str>,
        output_type: Option<&str>,
        position_ms: u64,
    ) {
        let Some(did) = device_id else {
            return;
        };
        // La session est celle que `play_without_history` vient d'installer :
        // `now_playing.stream_id` porte le NOUVEAU flux, pas l'ancien.
        let stream_id = self
            .playback
            .get_state(zone_id)
            .await
            .now_playing
            .and_then(|np| np.stream_id);
        let session_is_range_seekable = match stream_id {
            Some(ref sid) => self.streamer.is_seekable_session(sid).await,
            None => false,
        };
        if !replay_needs_output_seek(
            is_network_output_type(output_type),
            session_is_range_seekable,
            position_ms,
        ) {
            return;
        }

        tokio::time::sleep(std::time::Duration::from_millis(
            REPLAY_OUTPUT_SEEK_SETTLE_MS,
        ))
        .await;
        let Some(output) = ({ self.outputs.lock().await.get(did) }) else {
            warn!(
                zone_id,
                device_id = did,
                "replay_output_seek_output_disparue"
            );
            return;
        };
        match output.lock().await.checked_seek(position_ms).await {
            Ok(()) => info!(zone_id, position_ms, "replay_output_seek_sent"),
            Err(e) => warn!(zone_id, position_ms, error = %e, "replay_output_seek_failed"),
        }
        // Repositionner APRÈS le Seek : la grâce du poller doit partir de la
        // commande, pas de la recréation qui la précède de 500 ms.
        self.playback.seek(zone_id, position_ms as i64).await;
    }

    /// La position de lecture de cette zone est-elle ENTRETENUE ?
    ///
    /// #2595 — Pierre M, zone 987 : basculer en mode Audiophile pendant
    /// l'écoute fait repartir le morceau **du début**.
    ///
    /// [`Self::schedule_eq_replay`] relit `position_ms` pour rejouer « là où on
    /// en était ». Or cette valeur n'est entretenue que par l'unique
    /// `update_position` de production du sondeur — et la boucle de transport
    /// de `poller.rs` s'ouvre sur `get_zone_device_id`, dont la branche `None`
    /// fait `continue` **avant** cet appel. Une zone sans périphérique de
    /// sortie — une zone navigateur, « Cet ordinateur », par conception : le
    /// client web tire `stream_url` lui-même — n'est donc jamais observée. Son
    /// `position_ms` reste figé sur ce que la dernière COMMANDE y a écrit,
    /// c'est-à-dire 0 depuis `play()`, pendant que le morceau avance dans
    /// l'onglet.
    ///
    /// Zéro n'est alors pas une position : c'est une absence de mesure.
    /// Rejouer dessus n'est pas « reprendre », c'est recommencer.
    ///
    /// Le prédicat est **celui du sondeur, à l'identique** et à dessein, et il
    /// est STRUCTUREL — pas un seuil, pas une heuristique sur la valeur : la
    /// question posée est « quelqu'un observe-t-il cette zone ? ».
    /// Périphérique présent ⇒ le sondeur passe ⇒ la position est mesurée à la
    /// seconde. Pas de périphérique ⇒ personne ne la mesure, et le serveur ne
    /// sait pas où en est la lecture. Rien ici ne confond « zéro » et « je ne
    /// sais pas » : les deux cas ne se lisent pas dans la même valeur.
    ///
    /// Le seul autre gisement possible serait le client lui-même — c'est
    /// l'onglet qui joue, donc lui seul connaît sa position. Aucune route ne la
    /// remonte aujourd'hui (`SeekRequest` est une COMMANDE, pas un rapport), et
    /// l'inventer côté serveur à partir de `streamer_bytes_sent` mesurerait le
    /// TÉLÉCHARGEMENT, pas l'écoute — un lecteur qui tamponne en avance donnerait
    /// une position en avance. Tant que le client ne la déclare pas, la réponse
    /// honnête est « je ne sais pas ».
    fn position_entretenue_par_le_sondeur(&self, zone_id: i64) -> bool {
        ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id)
            .is_some()
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

    /// Durée de la rampe anti-« ploc » à la pause, à la reprise et à l'arrêt
    /// pour cette zone, en millisecondes (#1590).
    ///
    /// Demandé par Levente : « the app just stops immediately when pushing
    /// Pause/Stop ». Ce n'est pas un fondu artistique — c'est le retrait du clic
    /// que produit une marche de pleine amplitude à zéro en un échantillon.
    ///
    /// **Ne concerne que la sortie LOCALE.** Sur un renderer réseau — DLNA,
    /// AirPlay, BluOS, Chromecast, OpenHome, Squeezebox, HQPlayer — c'est
    /// l'appareil qui décode et qui rend : nous n'avons pas la main sur ses
    /// échantillons. Le seul levier serait son propre volume, avec sa latence
    /// et sa granularité à lui ; le résultat serait très inégal d'un appareil à
    /// l'autre, parfois pire que la coupure. Ces sorties restent donc
    /// délibérément nues, et la sortie locale exclusive aussi (voir plus bas).
    ///
    /// `0` en mode PURE : la rampe est une modification du signal, elle tombe
    /// avec l'égaliseur et le repli mono. Ce n'est pas la seule garde — la
    /// sortie redésarme d'elle-même sur un flux DoP et en mode exclusif, que
    /// l'orchestrateur ait dit ce qu'il voulait ou non
    /// ([`crate::audio::soft_mute::armed_ms`]).
    ///
    /// Réglable par zone via la clé `zone_<id>_soft_mute_ms` du magasin de
    /// réglages — pas de migration, comme `zone_<id>_mono_downmix`. La valeur
    /// est bornée par [`SOFT_MUTE_MAX_MS`](crate::audio::soft_mute::SOFT_MUTE_MAX_MS) :
    /// au-delà, l'appui sur Pause cesse d'être ressenti comme une pause.
    pub fn zone_soft_mute_ms(&self, zone_id: i64) -> u32 {
        Self::zone_soft_mute_ms_with(&self.db, zone_id)
    }

    /// Même règle, lisible sans orchestrateur — jumelle de
    /// [`Self::zone_mono_downmix_with`].
    pub fn zone_soft_mute_ms_with(
        db: &std::sync::Arc<dyn crate::db::backend::DbBackend>,
        zone_id: i64,
    ) -> u32 {
        use crate::audio::soft_mute::{SOFT_MUTE_DEFAULT_MS, SOFT_MUTE_MAX_MS};
        // PURE : le PCM atteint la sortie intact, aucune rampe n'est appliquée.
        if crate::audio::audiophile::zone_enabled(db, zone_id) {
            return 0;
        }
        crate::db::settings_repo::SettingsRepo::with_backend(db.clone())
            .get(&format!("zone_{zone_id}_soft_mute_ms"))
            .ok()
            .flatten()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(SOFT_MUTE_DEFAULT_MS)
            .min(SOFT_MUTE_MAX_MS)
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
        contexte: ContexteEcoute<'_>,
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
            context_type: contexte.nature.map(Into::into),
            context_id: contexte.id.map(Into::into),
            context_position: contexte.rang,
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
                ContexteEcoute {
                    nature: etat.session_context_type.as_deref(),
                    id: etat.session_context_id.as_deref(),
                    rang: rang_a_retenir(etat.shuffle, etat.queue_position),
                },
            );
        }

        true
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

    /// Dire à l'auditeur que la session n'a pas survécu à la pause — sur les
    /// DEUX canaux, parce qu'aucun des deux ne suffit seul.
    ///
    /// L'`OutputCommandError` remonte à l'appelant HTTP : `POST
    /// /zones/{id}/resume` la rend en 502 avec son `message`
    /// (`output_command_error_response`). Mais seul le client qui a appuyé sur
    /// lecture la voit, et seulement s'il attend la réponse.
    /// `zone.playback_error` — le canal que six autres échecs de lecture
    /// utilisent déjà, poussé verbatim à TOUS les clients par `routes/ws.rs` —
    /// porte la même phrase aux autres télécommandes. La deuxième moitié de
    /// #1998 nommait déjà `resume` parmi les chemins où la cause « mourait ici ».
    ///
    /// `fatal: true` pour la raison écrite en #1960 et #2630 : sans lui, la
    /// fenêtre de grâce d'après-lecture du client web avale le message, et
    /// l'auditeur n'a — une fois de plus — que le silence.
    fn dire_session_perdue(
        &self,
        zone_id: i64,
        titre: &str,
        position_ms: Option<u64>,
        cause: Option<&str>,
    ) -> OutputCommandError {
        let message = message_session_perdue(titre, position_ms, cause);
        // `?position_ms` et non `position_ms` : le journal doit distinguer
        // `Some(137000)` de `None`, pas écrire un `0` de plus (#3244).
        warn!(zone_id, ?position_ms, %message, "resume_stream_session_lost");
        if let Some(ref bus) = self.event_bus {
            bus.emit(
                "zone.playback_error",
                serde_json::json!({
                    "zone_id": zone_id,
                    "error": message.clone(),
                    "fatal": true,
                }),
            );
        }
        OutputCommandError::failed(OutputCommand::Resume, message)
    }
    pub async fn resume(&self, zone_id: i64, device_id: Option<&str>) -> OutputCommandResult<()> {
        // Position is preserved across pause (playback state isn't reset), so we
        // know where to resume from.
        let state = self.playback.get_state(zone_id).await;
        let position_ms = state.position_ms.max(0) as u64;
        // #3244 — cette valeur est-elle une MESURE ?
        //
        // Jumeau de #2595 : `position_ms` n'est entretenue que par l'unique
        // `update_position` de production du sondeur, et la boucle de transport
        // de `poller.rs` `continue` avant lui quand la zone n'a pas de
        // périphérique. Sur une zone navigateur la valeur reste donc figée à 0
        // depuis `play()` pendant que le morceau avance dans l'onglet : zéro n'y
        // est pas une position, c'est une absence de mesure.
        //
        // Ce que `resume` en fait, site par site :
        //
        // - `requete_de_retablissement` (le seul geste qui RELANCE à la
        //   position) et le rattrapage `checked_seek` des renderers DLNA sont
        //   déjà gardés par la présence d'une SORTIE — `did` / `device_id`. Or
        //   cette présence est exactement le prédicat de
        //   `position_entretenue_par_le_sondeur` : sans périphérique, aucune des
        //   deux branches n'est atteinte. `resume` ne peut donc PAS relancer une
        //   zone navigateur au début, et le défaut annoncé par #3244 n'est pas
        //   atteignable par là ;
        // - reste `dire_session_perdue`, qui ANNONCE la position à l'auditeur —
        //   et celle-là n'est gardée par rien. Une zone navigateur dont la
        //   session a été ramassée s'entendait dire « ne peut pas reprendre à
        //   0:00 », un horodatage inventé qui désigne le début du morceau. C'est
        //   ce que ce correctif ferme.
        //
        // On ne comble surtout pas la source : `streamer_bytes_sent` mesure le
        // TÉLÉCHARGEMENT, pas l'écoute. Seul le client connaît sa position, et
        // aucune route ne la remonte aujourd'hui (`SeekRequest` est une
        // COMMANDE, pas un rapport).
        let position_mesuree = self
            .position_entretenue_par_le_sondeur(zone_id)
            .then_some(position_ms);

        // Une reprise « sur place » suppose que la SESSION DE FLUX qui alimentait
        // la sortie a survécu à la pause. Deux façons d'y échouer, deux remèdes.
        //
        // WEBRADIO (#1629) : le flux est un DIRECT. Pendant la pause le pipeline
        // continue de se périmer — la connexion icecast peut mourir par un chemin
        // qui ne logge qu'en debug!, la sortie accumule un retard sans borne, les
        // horodatages OAAT prennent toute la durée de la pause de retard — et la
        // reprise « sur place » rend du silence sans la moindre erreur (.42 :
        // pause 15:48 → reprise 16:07, aucun son, volume dans le vide). Le remède
        // est un RE-PLAY de la station : on reprend le direct, pas un différé de
        // dix-neuf minutes. Rien de tout cela ne change ici.
        //
        // PISTE (#2512) : le MÊME silence, par une autre porte, et il n'était pas
        // gardé. Le ramasse-miettes retire toute session NON RADIO restée
        // `SESSION_IDLE_TIMEOUT` (trente minutes) sans servir un octet — la radio
        // y est explicitement exemptée, la piste non — et une lecture en pause ne
        // tire plus rien, ce que le commentaire de la borne dit mot pour mot :
        // « elle survit une demi-heure aujourd'hui ». Passé ce délai la session
        // n'existe plus, `/stream/{id}` répond 404, et toutes les familles de
        // sorties reprennent sur du vide — la locale comprise, qui va chercher
        // son PCM en HTTP.
        //
        // Le remède n'est PAS celui de la radio : on rétablit la même écoute à la
        // MÊME position. Et quand plus rien ne le permet, on le dit.
        if let Some(np) = state.now_playing.as_ref() {
            let est_radio = np.source == "radio";
            let has_url = np.source_id.as_deref().is_some_and(|s| !s.is_empty());
            // De quoi re-demander la MÊME écoute : une URL de station pour une
            // radio, une piste identifiable pour tout le reste.
            let rejouable = if est_radio {
                has_url
            } else {
                np.track_id.is_some() || has_url
            };
            let pause_longue = state
                .paused_at
                .is_some_and(|t| t.elapsed() >= RADIO_RESUME_REPLAY_AFTER);
            // Ce qui tue une session n'est pas le même selon la famille. Radio :
            // le producteur de décodage s'est terminé — plus rien n'alimente la
            // session WAV, reprendre sur place serait silencieux aussi. Piste :
            // `producer_done` n'est jamais armé (seul `decode_radio_stream_to_pcm`
            // le fait), la seule question qui vaille est « la session existe-t-elle
            // encore ».
            let session_morte = match np.stream_id.as_deref() {
                Some(sid) if est_radio => self.streamer.radio_producer_done(sid).await,
                Some(sid) => !self.streamer.session_alive(sid).await,
                None => false,
            };
            let decision = reprise_de_session(est_radio, rejouable, pause_longue, session_morte);
            if decision != RepriseDeSession::SurPlace {
                let did = device_id.map(str::to_string).or_else(|| {
                    ZoneRepo::with_backend(self.db.clone())
                        .get(zone_id)
                        .ok()
                        .flatten()
                        .and_then(|z| z.output_device_id)
                });
                match (decision, did) {
                    (RepriseDeSession::RejouerLeDirect, Some(did)) => {
                        info!(
                            zone_id,
                            paused_long = pause_longue,
                            producer_dead = session_morte,
                            "radio_resume_replay"
                        );
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
                    (RepriseDeSession::RetablirALaPosition, Some(did)) => {
                        info!(zone_id, position_ms, "resume_stream_session_restore");
                        let req = requete_de_retablissement(zone_id, did, np, position_ms);
                        // Même piste, même écoute : pas de seconde ligne
                        // d'historique, exactement comme le re-play radio.
                        match self.play_without_history(req).await {
                            Ok(_) => return Ok(()),
                            // Pas de repli silencieux : la sortie n'a plus rien à
                            // jouer, la reprendre ne ferait que rejouer le défaut.
                            Err(e) => {
                                return Err(self.dire_session_perdue(
                                    zone_id,
                                    &np.title,
                                    position_mesuree,
                                    Some(&e),
                                ));
                            }
                        }
                    }
                    (RepriseDeSession::Expliquer, _)
                    | (RepriseDeSession::RetablirALaPosition, None) => {
                        return Err(self.dire_session_perdue(
                            zone_id,
                            &np.title,
                            position_mesuree,
                            None,
                        ));
                    }
                    // Radio dont on ne connaît aucune sortie : repli inchangé sur
                    // la reprise ordinaire, exactement comme avant #2512.
                    (RepriseDeSession::RejouerLeDirect, None) | (RepriseDeSession::SurPlace, _) => {
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
            let is_network = is_network_output_type(zone_output_type.as_deref());

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

    pub async fn wait_stream_data_ready(&self, stream_id: &str, timeout_ms: u64) -> bool {
        self.streamer.wait_data_ready(stream_id, timeout_ms).await
    }

    /// La session de flux `stream_id` est-elle encore inscrite ?
    ///
    /// A lire APRES [`Self::wait_stream_data_ready`], et seulement quand il a
    /// rendu `false` : ce dernier ne dit que « pas encore », jamais « plus
    /// jamais ». Une session VIVANTE et muette est un telechargement lent
    /// (DASH Hi-Res Tidal) ; une session DISPARUE est un producteur mort.
    /// C'est l'idiome que `resume` emploie deja (#2512) pour ne pas reprendre
    /// en silence sur une session ramassee.
    pub async fn stream_session_alive(&self, stream_id: &str) -> bool {
        self.streamer.session_alive(stream_id).await
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
