use super::*;

/// Error marker returned by `resolve_local_track` when a play was superseded by
/// a newer tap before its transcode started; `play_inner` maps it to a quiet
/// no-op result instead of a user-facing error.
pub(super) const SUPERSEDED_BEFORE_TRANSCODE: &str = "__superseded_before_transcode__";

/// Serializes ALAC/PCM→FLAC transcodes of the *same* source file across
/// concurrent plays, keyed by source path. A burst of play taps for a
/// slow-to-decode NAS track otherwise kicks off one full transcode each
/// (Yves: 6 concurrent transcodes of a single file in 20s → overlapping FLAC
/// streams to the DLNA renderer = noise). The winner transcodes; every play a
/// newer tap has already superseded skips it entirely.
pub(super) static TRANSCODE_GATE: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
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
pub(super) fn transcode_budget_for(path: &str) -> std::time::Duration {
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
pub(super) const PLAFOND_BUDGET_TRANSCODAGE: std::time::Duration =
    std::time::Duration::from_secs(30 * 60);

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
pub(super) const MARGE_BUDGET_TRANSCODAGE: f64 = 1.20;

/// Pas de sondage de la balise d'avancement.
///
/// Il fixe aussi la granularité du dépassement : le budget peut être franchi
/// d'au plus un pas. Sur un budget qui vaut au minimum 120 s, un quart de
/// seconde ne se voit pas.
pub(super) const PAS_SONDAGE_BUDGET: std::time::Duration = std::time::Duration::from_millis(250);

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
pub(super) const SONDAGES_AVANT_MESURE: usize = 8;

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
pub(super) struct BudgetAdaptatif {
    /// Durée de la piste, en secondes. `0` quand la base ne la connaît pas —
    /// on ne peut alors rien extrapoler et le budget historique s'applique.
    pub(super) piste_s: f64,
    /// Le budget historique, indexé sur la taille. Plancher indéplaçable.
    pub(super) budget_taille: std::time::Duration,
    pub(super) plafond: std::time::Duration,
}

/// Ce que la politique conclut d'une observation de la balise.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum VerdictBudget {
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
    pub(super) fn new(piste_s: f64, budget_taille: std::time::Duration) -> Self {
        Self {
            piste_s,
            budget_taille,
            plafond: PLAFOND_BUDGET_TRANSCODAGE,
        }
    }

    /// `ecoule` : temps depuis le début du transcodage. `decode` : audio déjà
    /// sorti du décodeur. `sondages` : combien de sondages consécutifs ont vu
    /// de l'avancement.
    pub(super) fn observer(
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
pub(super) struct DepassementBudget {
    pub(super) budget: std::time::Duration,
    pub(super) ecoule: std::time::Duration,
    pub(super) decode: std::time::Duration,
    /// Facteur temps réel mesuré sur cet hôte, si la balise a parlé.
    pub(super) facteur: Option<f64>,
    pub(super) piste_s: f64,
}

impl DepassementBudget {
    /// Le facteur temps réel qu'il aurait fallu pour tenir dans ce budget.
    pub(super) fn facteur_requis(&self) -> Option<f64> {
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
pub(super) async fn transcoder_sous_budget<F, T>(
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
pub(super) async fn transcode_source_to_file(
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
pub(super) async fn abandonner_la_session_de_transcodage(
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
