use super::PositionPoller;
use crate::cloud::tune_tested_auto::{AppareilObserve, FormatObserve};
use crate::db::settings_repo::SettingsRepo;
use crate::playback::ZoneState;

impl PositionPoller {
    pub(super) async fn noter_fin_tune_tested(
        &self,
        etat: &ZoneState,
        device_id: &str,
        progression: u64,
    ) {
        if etat.now_playing.as_ref().is_some_and(|np| {
            np.duration_ms > 0 && progression >= (np.duration_ms as u64).saturating_sub(1000).max(1)
        }) {
            self.noter_tune_tested(etat, device_id, true).await;
        }
    }

    pub(super) async fn noter_tune_tested(&self, etat: &ZoneState, device_id: &str, succes: bool) {
        let settings = SettingsRepo::with_backend(self.db.clone());
        if !crate::cloud::telemetry::TelemetryReporter::is_enabled_for(&settings) {
            return;
        }
        let sortie = { self.outputs.lock().await.get(device_id) };
        let Some(sortie) = sortie else {
            return;
        };
        let protocole = {
            // Une observation facultative ne retarde jamais une commande audio.
            let Ok(sortie) = sortie.try_lock() else {
                return;
            };
            sortie.output_type().to_owned()
        };
        let format = if !succes {
            None
        } else if let Some(transformation) = etat.transformations_reelles.as_ref() {
            let cadence = transformation.ouvert().cadence;
            Some(FormatObserve {
                codec: if etat.dop_active { "dsd" } else { "pcm" }.into(),
                sample_rate: if etat.dop_active {
                    cadence.checked_mul(16)
                } else {
                    Some(cadence)
                },
                // La cadence ouverte est mesurée ; la profondeur du convertisseur
                // ne se déduit pas du flottant de traitement.
                bit_depth: etat.dop_active.then_some(1),
                dsd_mode: etat.dop_active.then(|| "dop".into()),
            })
        } else if ["dlna", "openhome", "chromecast", "sonos", "bluos"].contains(&protocole.as_str())
        {
            let sessions = self.orchestrator.streamer.sessions_state();
            let session = if let Some(stream_id) = etat
                .now_playing
                .as_ref()
                .and_then(|np| np.stream_id.as_ref())
            {
                sessions.lock().await.get(stream_id).cloned()
            } else {
                None
            };
            session.and_then(|session| {
                format_de_session(&session.effective_output_info(), etat.dop_active)
            })
        } else {
            None
        };
        // La découverte conserve aussi des adresses et noms conviviaux : seuls
        // ces deux champs techniques en sortent, jamais le document entier.
        let identite = settings
            .get(crate::device_catalog::KNOWN_RENDERERS_KEY)
            .ok()
            .flatten()
            .and_then(|brut| serde_json::from_str::<Vec<serde_json::Value>>(&brut).ok())
            .and_then(|entrees| {
                entrees
                    .into_iter()
                    .find(|e| e["device_id"].as_str() == Some(device_id))
            })
            .and_then(|e| {
                Some((
                    e["manufacturer"].as_str()?.to_owned(),
                    e["model"].as_str()?.to_owned(),
                ))
            });
        let resultat = self
            .observateur_tune_tested
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .terminer_avec_identite(
                &settings,
                (
                    etat.zone_id,
                    etat.track_generation,
                    // L'avance gapless invalide les niveaux, y compris en
                    // répétition d'une piste, sans changer track_generation.
                    self.playback
                        .levels_gen(etat.zone_id)
                        .load(std::sync::atomic::Ordering::Relaxed),
                ),
                AppareilObserve {
                    protocole: &protocole,
                    identite_detectee: identite
                        .as_ref()
                        .map(|(marque, modele)| (marque.as_str(), modele.as_str())),
                },
                succes,
                format,
            );
        if resultat.is_err() {
            tracing::debug!("tune_tested_auto_observation_non_conservee");
        }
    }
}

fn format_de_session(info: &crate::http::streamer::StreamInfo, dop: bool) -> Option<FormatObserve> {
    use crate::audio::formats::AudioFormat;
    let codec = match AudioFormat::from_extension(&info.format)? {
        AudioFormat::Flac => "flac",
        AudioFormat::Wav => "wav",
        AudioFormat::Mp3 => "mp3",
        AudioFormat::Aac => "aac",
        AudioFormat::Alac => "alac",
        AudioFormat::Ogg => "ogg",
        AudioFormat::Opus => "opus",
        AudioFormat::Aiff => "aiff",
        AudioFormat::Dsd => "dsd",
        // Un conteneur non sondé ne prouve aucun codec.
        _ => return None,
    };
    let dsd = codec == "dsd" || dop;
    Some(FormatObserve {
        codec: if dsd { "dsd" } else { codec }.into(),
        sample_rate: if dop && codec != "dsd" {
            info.sample_rate.checked_mul(16)
        } else {
            (info.sample_rate > 0).then_some(info.sample_rate)
        },
        bit_depth: if dsd {
            Some(1)
        } else {
            (info.bit_depth > 0).then_some(info.bit_depth)
        },
        // Un fichier DSF accepté sur HTTP ne révèle pas le transport du DAC.
        // Seul un mode mesuré peut être déclaré ; le conteneur ne certifie
        // jamais DSD natif.
        dsd_mode: dop.then(|| "dop".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::streamer::StreamInfo;

    #[test]
    fn tune_tested_auto_format_de_sortie_converti_et_non_format_source() {
        // Le convertisseur a produit ce WAV 44,1/16, indépendamment du DSD
        // haute résolution de la source. Aucun NowPlaying n'entre ici.
        let info = StreamInfo {
            format: "wav".into(),
            sample_rate: 44100,
            bit_depth: 16,
            ..Default::default()
        };
        let format = format_de_session(&info, false).unwrap();
        assert_eq!(format.codec, "wav");
        assert_eq!(format.sample_rate, Some(44100));
        assert_eq!(format.bit_depth, Some(16));
        assert_eq!(format.dsd_mode, None);
    }

    #[test]
    fn tune_tested_auto_dop_observe_et_conteneur_inconnu_non_devine() {
        let mut info = StreamInfo {
            format: "wav".into(),
            sample_rate: 176400,
            bit_depth: 24,
            ..Default::default()
        };
        let format = format_de_session(&info, true).unwrap();
        assert_eq!(format.codec, "dsd");
        assert_eq!(format.sample_rate, Some(2822400));
        assert_eq!(format.bit_depth, Some(1));
        assert_eq!(format.dsd_mode.as_deref(), Some("dop"));
        info.format = "m4a".into();
        assert!(format_de_session(&info, false).is_none());
    }

    #[test]
    fn tune_tested_auto_fichier_dsd_ne_certifie_pas_le_transport_natif() {
        let info = StreamInfo {
            format: "dsf".into(),
            sample_rate: 2822400,
            bit_depth: 1,
            ..Default::default()
        };
        let observe = format_de_session(&info, false).unwrap();
        assert_eq!(observe.codec, "dsd");
        assert_eq!(observe.sample_rate, Some(2822400));
        assert_eq!(observe.bit_depth, Some(1));
        assert_eq!(
            observe.dsd_mode, None,
            "le format HTTP ne prouve pas le mode du DAC"
        );
    }
}
