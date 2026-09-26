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

    /// #5065 — le registre commun des sources physiques, que les greffons
    /// natifs reçoivent par l'orchestrateur de leurs `HostServices`.
    pub fn sources_physiques(&self) -> &Arc<crate::sources_physiques::RegistreSources> {
        &self.sources_physiques
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
                pomper(&mut *lecteur, octets, |t| rt.block_on(tx.send(t)).is_ok())
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
}
