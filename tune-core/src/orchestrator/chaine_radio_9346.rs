//! #9346 — la chaîne de la zone sur une radio que Tune décode pour une sortie
//! qui ne traite rien elle-même : renderer DLNA, OAAT, onglet navigateur.
//!
//! Avant, le décodeur radio ne recevait que le profil d'ÉGALISEUR
//! (`radio_eq_profile`, #2063) : une zone avec une convolution (correction de
//! pièce) ou un crossfeed entendait la station SANS ces étages, alors que le
//! chemin du signal, qui lit les réglages de la zone, les annonçait actifs
//! (forum-hifi, message 228 : « convolution affichée active, mais elle ne
//! s'entend pas »). Le passage direct d'une station `.mp3`/`.flac` n'était
//! de même interdit que par un égaliseur.
//!
//! Le décodeur ne connaît la cadence et le nombre de canaux de la station
//! qu'après sa sonde (`preparer_la_sortie`, `publish_detected_pcm_format`) ;
//! un convolveur et une ligne à retard bâtis à une cadence supposée seraient
//! faux. La chaîne est donc PRÉPARÉE avant le décodage (ce qui ne dépend pas
//! de la cadence : réponse impulsionnelle chargée, réglages du crossfeed et
//! des greffons tiers, droit à la compensation), puis BÂTIE par le relais au
//! premier bloc de PCM, au format que la session publie à cet instant.
//!
//! L'égaliseur reste dans le décodeur, où vivent sa relève en vol (#4407) et
//! son fondu vers le sec (#8422) : il n'est jamais rebâti ici, pour ne pas
//! colorer le signal deux fois. Ordre résultant, celui de
//! [`StreamingDsp::process_flux`] : égaliseur, convolution, crossfeed,
//! compensation.

use super::{PlaybackOrchestrator, StreamingDsp, spawn_streaming_dsp_relay};
use std::sync::Arc;
use tracing::{info, warn};

/// Le crossfeed de la zone, tel que `load_crossfeed_processor` le compose.
struct CrossfeedPrepare {
    reglage: Option<(f32, f32)>,
    ombre: Option<tune_plugin_crossfeed::OmbreDeTete>,
    tiers: Vec<(String, serde_json::Value)>,
}

impl CrossfeedPrepare {
    fn batir(&self, sample_rate: u32) -> crate::audio::crossfeed::CrossfeedProcessor {
        crate::audio::crossfeed::CrossfeedProcessor::composer_avec_ombre(
            sample_rate,
            self.reglage,
            self.ombre,
            &self.tiers,
        )
    }
}

/// Ce que la zone demande à une radio servie, hors égaliseur. `None` à la
/// préparation quand il n'y a rien : la station garde son chemin d'avant à
/// l'octet près.
pub(super) struct ChaineRadio {
    zone_id: i64,
    convolution: Option<crate::audio::convolver::ConvolverConfig>,
    crossfeed: Option<CrossfeedPrepare>,
    /// Interrupteur de compensation de niveau (#5071), PURE exclu.
    compensation: bool,
    /// rc14 — le réglage des enceintes de la zone (hors PURE, « Bypass all »).
    enceintes: Option<crate::audio::reglage_enceintes::ReglagesEnceintes>,
}

impl ChaineRadio {
    /// Le flux portera-t-il le crossfeed ? Publié par la session
    /// (`StreamInfo::crossfeed`, #5114). Une station mono le rend inerte :
    /// le relais le dit au journal ; l'immense majorité est stéréo.
    pub(super) fn crossfeed_prevu(&self) -> bool {
        self.crossfeed.is_some()
    }

    /// La compensation que le flux cuira, en dB (`StreamInfo::compensation_db`).
    /// Celle du seul crossfeed : l'égaliseur de la radio n'a jamais été
    /// compensé (`EqEnVol::pour_la_radio_servie`), ce correctif n'y touche pas.
    pub(super) fn compensation_prevue_db(&self) -> Option<f64> {
        self.compensation(44_100).map(|c| c.cible_db())
    }

    fn compensation(
        &self,
        sample_rate: u32,
    ) -> Option<crate::audio::compensation_reseau::CompensationReseau> {
        if !self.compensation {
            return None;
        }
        let cf = self.crossfeed.as_ref()?.batir(sample_rate);
        crate::audio::compensation_reseau::CompensationReseau::depuis_db(-cf.gain_moyen_db())
    }

    /// La chaîne au format réel de la station.
    pub(super) fn batir(&self, sample_rate: u32, channels: u16) -> StreamingDsp {
        let convolver = self.convolution.as_ref().and_then(|config| {
            match config.build_for(1024, sample_rate, channels as usize) {
                Ok(c) => Some(c),
                Err(error) => {
                    warn!(
                        zone_id = self.zone_id,
                        sample_rate,
                        channels,
                        %error,
                        "radio_convolution_non_appliquee_9346"
                    );
                    None
                }
            }
        });
        let crossfeed = self.crossfeed.as_ref().map(|c| c.batir(sample_rate));
        let compensation = if channels == 2 {
            self.compensation(sample_rate)
        } else {
            None
        };
        let enceintes = self
            .enceintes
            .clone()
            .filter(|r| !r.est_neutre(usize::from(channels)))
            .map(crate::audio::reglage_enceintes::ProcesseurEnceintes::new);
        // Une radio est un flux continu : rien à reprendre ni à garder.
        StreamingDsp {
            convolver,
            crossfeed,
            compensation,
            enceintes,
            channels,
            sample_rate,
            ..Default::default()
        }
    }
}

impl PlaybackOrchestrator {
    /// La chaîne de la zone pour une radio servie, ou `None` quand la zone ne
    /// demande ni convolution ni crossfeed (ni greffon tiers de cet étage),
    /// ou qu'elle est en PURE. Mêmes chargeurs, mêmes gardes (licence
    /// comprise) que `load_streaming_dsp`.
    pub(super) fn preparer_la_chaine_radio(&self, zone_id: i64) -> Option<ChaineRadio> {
        if self.zone_audiophile(zone_id) {
            return None;
        }
        let convolution = self.jeu_ir(zone_id).and_then(|jeu| {
            let chemin = jeu.principal.clone();
            match jeu.charger() {
                Ok(config) => Some(config),
                Err(error) => {
                    warn!(zone_id, path = chemin, %error, "room_correction_ir_load_failed");
                    None
                }
            }
        });
        let reglage = self.crossfeed_configure(zone_id);
        let tiers = self.etages_tiers_configures(zone_id);
        let crossfeed = (reglage.is_some() || !tiers.is_empty()).then(|| CrossfeedPrepare {
            reglage,
            ombre: self.crossfeed_ombre_configuree(zone_id),
            tiers,
        });
        let enceintes = self
            .zone_reglage_enceintes(zone_id)
            .filter(|r| !r.est_neutre_partout());
        if convolution.is_none() && crossfeed.is_none() && enceintes.is_none() {
            return None;
        }
        Some(ChaineRadio {
            zone_id,
            convolution,
            crossfeed,
            compensation: self.zone_compensation_de_niveau(zone_id),
            enceintes,
        })
    }
}

/// Le relais posé entre le décodeur radio et sa session : au premier bloc,
/// la chaîne est bâtie au format publié par la session, puis chaque bloc la
/// traverse ([`spawn_streaming_dsp_relay`], état du convolveur conservé d'un
/// bloc à l'autre). Le canal radio ne porte aucun en-tête WAV (le serveur
/// HTTP l'écrit d'après `publish_detected_pcm_format`) : rien à sauter.
pub(super) fn relayer_par_la_chaine_radio(
    chaine: ChaineRadio,
    session: Arc<crate::http::streamer::StreamSession>,
    aval: tokio::sync::mpsc::Sender<Vec<u8>>,
) -> tokio::sync::mpsc::Sender<Vec<u8>> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    tokio::spawn(async move {
        let Some(premier) = rx.recv().await else {
            return;
        };
        let (sample_rate, channels) = session.detected_output_format().unwrap_or((44_100, 2));
        let bit_depth = session.output_bit_depth();
        let dsp = chaine.batir(sample_rate, channels);
        info!(
            zone_id = chaine.zone_id,
            sample_rate,
            channels,
            bit_depth,
            chaine = %dsp.decrire(),
            "radio_chaine_de_zone_appliquee_9346"
        );
        let aval = if dsp.is_active() {
            spawn_streaming_dsp_relay(dsp, bit_depth, false, aval)
        } else {
            aval
        };
        if aval.send(premier).await.is_err() {
            return;
        }
        while let Some(bloc) = rx.recv().await {
            if aval.send(bloc).await.is_err() {
                break;
            }
        }
    });
    tx
}

#[cfg(test)]
#[path = "chaine_radio_9346_tests.rs"]
mod tests;
