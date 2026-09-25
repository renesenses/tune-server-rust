//! Résolution d'une ligne dont la `source` est fournie par un greffon en PCM
//! (#4863). Voir `crate::source_pcm` pour le contrat.
//!
//! Le flux est servi par une session FINIE de l'`AudioStreamer` — la même
//! famille que le transcodage en WAV d'une piste locale : en-tête RIFF
//! envoyé dans le canal (`wav_header_included`), longueur exacte annoncée
//! (`file_size`), fin d'entrée signalée (`end_session_input`) pour que la
//! sortie voie un vrai EOF et enchaîne la piste suivante.

use super::*;
use crate::source_pcm::{FinDePompe, FournisseurPcm, pomper};

impl PlaybackOrchestrator {
    /// Le registre des sources PCM fournies par les greffons.
    pub fn sources_pcm(&self) -> &crate::source_pcm::SourcesPcm {
        &self.sources_pcm
    }

    pub(super) async fn resolve_source_pcm(
        &self,
        source: &str,
        fournisseur: Arc<dyn FournisseurPcm>,
        req: &PlayRequest,
    ) -> Result<ResolvedStream, String> {
        let source_id = req
            .source_id
            .clone()
            .ok_or_else(|| format!("source « {source} » : source_id requis"))?;
        let depuis_ms = req.seek_ms.unwrap_or(0);
        // L'ouverture parle au matériel (lecture de la TOC d'un disque) : hors
        // du fil asynchrone.
        let ouvrir = {
            let f = fournisseur.clone();
            let sid = source_id.clone();
            tokio::task::spawn_blocking(move || f.ouvrir(&sid, depuis_ms))
        };
        let flux = ouvrir
            .await
            .map_err(|e| format!("source « {source} » : ouverture interrompue ({e})"))??;
        let format = flux.format;
        let octets = flux.octets;
        let data_size = u32::try_from(octets)
            .map_err(|_| format!("source « {source} » : flux trop long pour un en-tête WAV"))?;
        let info = StreamInfo {
            format: "wav".into(),
            mime_type: "audio/wav".into(),
            sample_rate: format.frequence,
            bit_depth: format.bits,
            channels: format.canaux,
            // La longueur EXACTE : elle devient le Content-Length (voir le
            // contrat de longueur dans `crate::source_pcm`).
            file_size: Some(44 + octets),
            duration_ms: Some(octets * 1000 / format.octets_par_seconde().max(1)),
            seek_ms: (depuis_ms > 0).then_some(depuis_ms),
            ..Default::default()
        };
        let (session_id, tx, data_ready) = self.streamer.create_session(info, false, 256).await;
        {
            let sessions = self.streamer.sessions_state();
            let sessions = sessions.lock().await;
            if let Some(s) = sessions.get(&session_id) {
                s.wav_header_included
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
        }
        info!(
            zone_id = req.zone_id,
            source,
            source_id = %source_id,
            depuis_ms,
            octets,
            stream_id = %session_id,
            "source_pcm_session_ouverte"
        );

        // #5078 — les niveaux (crête-mètre, vu-mètre, spectre) du PCM pompé.
        let niveaux = self
            .niveaux_de_la_source_pcm(req.zone_id, &session_id, depuis_ms)
            .await;

        let streamer = self.streamer.clone();
        let bus = self.event_bus.clone();
        let zone_id = req.zone_id;
        let sid = session_id.clone();
        let nom = source.to_string();
        let titre = req.title.clone().unwrap_or_default();
        let mut lecteur = flux.lecteur;
        tokio::spawn(async move {
            let rt = tokio::runtime::Handle::current();
            let fin = tokio::task::spawn_blocking(move || {
                let entete = crate::audio::wav::build_wav_header_with_data_size(
                    format.canaux,
                    format.frequence,
                    format.bits,
                    data_size,
                );
                if rt.block_on(tx.send(entete.to_vec())).is_err() {
                    return FinDePompe::ConsommateurParti;
                }
                data_ready.notify_one();
                pomper(&mut *lecteur, octets, |t| {
                    if let Some(ltx) = &niveaux {
                        crate::audio::tap::send_windowed_pcm(
                            ltx,
                            &t,
                            format.bits,
                            format.canaux,
                            format.frequence,
                        );
                    }
                    rt.block_on(tx.send(t)).is_ok()
                })
            })
            .await;
            match fin {
                Ok(FinDePompe::Complete) => debug!(stream_id = %sid, "source_pcm_complete"),
                Ok(FinDePompe::ConsommateurParti) => {
                    debug!(stream_id = %sid, "source_pcm_consommateur_parti")
                }
                Ok(FinDePompe::Interrompue { remis, raison }) => {
                    warn!(
                        zone_id,
                        source = %nom,
                        stream_id = %sid,
                        remis,
                        attendus = octets,
                        raison = %raison,
                        "source_pcm_interrompue"
                    );
                    if let Some(bus) = bus {
                        bus.emit(
                            "zone.playback_error",
                            serde_json::json!({
                                "zone_id": zone_id,
                                "error": format!("Lecture de « {titre} » interrompue : {raison}"),
                                "fatal": true,
                            }),
                        );
                    }
                }
                Err(e) => warn!(stream_id = %sid, error = %e, "source_pcm_tache_paniquee"),
            }
            // Contenu FINI : fermer l'entrée pour que la sortie voie l'EOF.
            streamer.end_session_input(&sid).await;
        });

        let url = self
            .streamer
            .get_stream_url(&session_id, &self.server_ip(), "wav");
        Ok(ResolvedStream {
            url,
            mime_type: "audio/wav".into(),
            title: req.title.clone().unwrap_or_default(),
            artist: req.artist_name.clone(),
            album: req.album_title.clone(),
            duration_ms: Some(flux.duree_ms as i64),
            source: source.to_string(),
            cover_url: req.cover_url.clone(),
            stream_id: Some(session_id),
            file_size: Some(44 + octets),
            sample_rate: Some(format.frequence),
            bit_depth: Some(format.bits as u32),
            channels: Some(format.canaux as u32),
            origin_url: None,
            bitrate_kbps: None,
        })
    }

    /// #5078 (GgB, fil 1945) — la source PCM n'alimentait AUCUN instrument :
    /// elle ouvrait sa session sans forwarder de niveaux, là où les autres
    /// chemins qui tiennent le PCM en main en attachent un. Même forwarder
    /// cadencé qu'eux : il publie une fenêtre quand la sortie la joue
    /// (fil 1908), pas quand la pompe la lit.
    ///
    /// Pendant un pré-armement gapless, aucun forwarder ne peut naître : il
    /// serait daté de la piste qui joue encore (voir `levels_prewarm`). Les
    /// fenêtres attendent alors l'avance qui adopte ce flux
    /// ([`adopter_les_niveaux_pre_armes`]) : relire le disque pour les
    /// niveaux, comme le fait la sonde d'une piste de service, le ferait
    /// sauter d'une piste à l'autre.
    async fn niveaux_de_la_source_pcm(
        &self,
        zone_id: i64,
        session_id: &str,
        depuis_ms: u64,
    ) -> Option<tokio::sync::mpsc::UnboundedSender<crate::audio::tap::RawWindow>> {
        let bus = self.event_bus.clone()?;
        if self.levels_attach_allowed(zone_id) {
            return self
                .levels_forwarder_if_allowed(zone_id, depuis_ms as i64)
                .await;
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let (adopte_tx, adopte_rx) = tokio::sync::oneshot::channel();
        niveaux_en_attente().insert(session_id.to_string(), adopte_tx);
        tokio::spawn(relayer_les_niveaux_pre_armes(
            bus,
            self.playback.clone(),
            self.streamer.clone(),
            zone_id,
            session_id.to_string(),
            depuis_ms,
            rx,
            adopte_rx,
        ));
        Some(tx)
    }
}

type Adoption = tokio::sync::oneshot::Sender<u64>;

/// Les sessions PCM pré-armées dont les niveaux attendent l'avance gapless,
/// par identifiant de session (un UUID : unique d'un orchestrateur à l'autre).
fn niveaux_en_attente() -> std::sync::MutexGuard<'static, HashMap<String, Adoption>> {
    static EN_ATTENTE: std::sync::LazyLock<std::sync::Mutex<HashMap<String, Adoption>>> =
        std::sync::LazyLock::new(Default::default);
    EN_ATTENTE.lock().unwrap_or_else(|e| e.into_inner())
}

/// L'avance gapless vient d'adopter `stream_id` : si c'est une source PCM
/// pré-armée, ses niveaux démarrent, sous le `play_seq` de la zone.
pub(super) fn adopter_les_niveaux_pre_armes(stream_id: &str, play_seq: u64) {
    if let Some(adoption) = niveaux_en_attente().remove(stream_id) {
        let _ = adoption.send(play_seq);
    }
}

/// Au plus deux minutes d'audio tenues en attente : la pompe devance la
/// sortie de la capacité de la session (256 tronçons, ~47 s de CD).
const ATTENTE_MAX: std::time::Duration = std::time::Duration::from_secs(120);

#[allow(clippy::too_many_arguments)]
async fn relayer_les_niveaux_pre_armes(
    bus: Arc<crate::event_bus::EventBus>,
    playback: Arc<PlaybackManager>,
    streamer: Arc<AudioStreamer>,
    zone_id: i64,
    session_id: String,
    depuis_ms: u64,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<crate::audio::tap::RawWindow>,
    mut adopte: tokio::sync::oneshot::Receiver<u64>,
) {
    let mut tampon = std::collections::VecDeque::new();
    let mut tenu = std::time::Duration::ZERO;
    let mut ecarte = std::time::Duration::ZERO;
    let mut pompe_finie = false;
    let mut controle = tokio::time::interval(std::time::Duration::from_secs(2));
    let play_seq = loop {
        tokio::select! {
            r = &mut adopte => match r {
                Ok(seq) => break seq,
                Err(_) => return,
            },
            f = rx.recv(), if !pompe_finie => match f {
                Some(f) => {
                    tenu += f.window;
                    tampon.push_back(f);
                    while tenu > ATTENTE_MAX {
                        let Some(vieille) = tampon.pop_front() else { break };
                        tenu -= vieille.window;
                        ecarte += vieille.window;
                    }
                }
                None => pompe_finie = true,
            },
            _ = controle.tick() => {
                // Flux jamais adopté (file changée, zone arrêtée) : sa
                // session a disparu, l'attente aussi.
                let vivante = streamer.sessions_state().lock().await.contains_key(&session_id);
                if !vivante {
                    niveaux_en_attente().remove(&session_id);
                    return;
                }
            }
        }
    };
    let fwd = super::spawn_paced_levels_forwarder(
        bus,
        playback,
        zone_id,
        play_seq,
        (depuis_ms as i64).saturating_add(ecarte.as_millis() as i64),
    );
    for f in tampon {
        if fwd.send(f).is_err() {
            return;
        }
    }
    while let Some(f) = rx.recv().await {
        if fwd.send(f).is_err() {
            return;
        }
    }
}
