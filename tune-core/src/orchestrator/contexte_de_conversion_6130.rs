//! #6130 — le contexte d'un flux de CONVERSION, posé sur sa session pour le
//! journal de chaque connexion HTTP (`tune-stream-http`).
//!
//! Zone 10 du .18 (Eversolo DMP-A8, 11/10) : un fort bruit blanc, environ une
//! fois tous les deux jours, sur un Qobuz FLAC converti en WAV avec un
//! greffon natif tiers actif. Impossible à reproduire : il faut que la
//! prochaine occurrence laisse au journal la source, la chaîne d'étages et ce
//! que la zone jouait juste avant. Rien ici ne touche au rendu.
use super::*;
use crate::http::streamer::ContexteDeConversion;

impl PlaybackOrchestrator {
    /// Ce que la zone joue AVANT que la nouvelle piste ne la remplace : radio
    /// ou piste, format, fréquence, et si sa session de flux vit encore.
    ///
    /// Appelé pendant la résolution, avant que le `NowPlaying` de la nouvelle
    /// piste soit publié : l'état de la zone est encore celui de la précédente.
    pub(super) async fn decrire_la_piste_precedente(&self, zone_id: i64) -> String {
        let Some(np) = self.playback.get_state(zone_id).await.now_playing else {
            return "aucune".to_string();
        };
        let session = match np.stream_id.as_deref() {
            Some(sid) => self
                .streamer
                .sessions_state()
                .lock()
                .await
                .get(sid)
                .cloned(),
            None => None,
        };
        let genre = if np.source == "radio" || session.as_ref().is_some_and(|s| s.is_radio) {
            "radio"
        } else {
            "piste"
        };
        // La fréquence RÉELLE d'une radio n'est connue que de sa sonde (FIP :
        // 48 000, pas les 44 100 de la session) ; à défaut, celle annoncée.
        let frequence = session
            .as_ref()
            .and_then(|s| s.detected_output_format().map(|(sr, _)| sr))
            .or(session.as_ref().map(|s| s.info.sample_rate))
            .or(np.sample_rate);
        format!(
            "{genre} source={} format={} frequence={} profondeur={} stream_id={} session_ouverte={}",
            np.source,
            np.format.as_deref().unwrap_or("-"),
            frequence.map_or_else(|| "-".to_string(), |f| f.to_string()),
            np.bit_depth
                .map_or_else(|| "-".to_string(), |b| b.to_string()),
            np.stream_id.as_deref().unwrap_or("-"),
            session.is_some(),
        )
    }

    /// Pose le contexte sur la session `session_id`. Une seule fois : un
    /// second appel est sans effet.
    pub(super) async fn poser_le_contexte_de_conversion(
        &self,
        session_id: &str,
        contexte: ContexteDeConversion,
    ) {
        info!(
            stream_id = session_id,
            zone_id = contexte.zone_id,
            source = %contexte.source,
            chaine = %contexte.chaine,
            precedente = %contexte.precedente,
            "conversion_contexte_pose"
        );
        let sessions = self.streamer.sessions_state();
        let sessions = sessions.lock().await;
        if let Some(session) = sessions.get(session_id) {
            let _ = session.contexte_de_conversion.set(contexte);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn une_chaine_vide_le_dit() {
        let dsp = StreamingDsp {
            channels: 2,
            sample_rate: 44_100,
            ..Default::default()
        };
        assert_eq!(dsp.decrire(), "aucun_etage canaux=2");
    }

    #[test]
    fn chaque_etage_porte_sa_frequence_d_entree_et_de_sortie() {
        let dsp = StreamingDsp {
            replaygain: Some(0.5),
            channels: 2,
            sample_rate: 48_000,
            ..Default::default()
        };
        assert_eq!(
            dsp.decrire(),
            "replaygain:x0.5000@48000[48000->48000] canaux=2"
        );
    }

    #[test]
    fn le_crossfeed_dit_la_frequence_a_laquelle_il_a_ete_prepare() {
        // Préparé à 48 000 mais posé sur un porteur à 44 100 : c'est
        // exactement l'écart que le journal doit rendre lisible.
        let dsp = StreamingDsp {
            crossfeed: Some(crate::audio::crossfeed::CrossfeedProcessor::new(
                48_000, 0.3, 0.3,
            )),
            channels: 2,
            sample_rate: 44_100,
            ..Default::default()
        };
        let d = dsp.decrire();
        assert!(d.starts_with("crossfeed:"), "{d}");
        assert!(d.contains("@48000[44100->44100]"), "{d}");
    }
}
