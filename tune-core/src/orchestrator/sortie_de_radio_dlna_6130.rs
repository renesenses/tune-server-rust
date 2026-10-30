//! #6130 — quitter une radio sur une sortie DLNA.
//!
//! Le terrain (.18, rc3, 12/10 05:49, Eversolo DMP-A8, et la même séquence le
//! 11/10) : FIP jouait depuis plus d'une heure et demie ; l'utilisateur lance
//! un titre Qobuz. `play_inner` fermait la session radio
//! (`stream_session_removed`, 52.067), puis `play_media` posait l'URI
//! (`dlna_set_uri_ok`, 52.527) et lançait `Play` (52.531). Le renderer, lui,
//! n'a lâché la connexion de la radio qu'à 53.830 — 1,3 s APRÈS le `Play` —
//! puis a ouvert trois fois le nouveau flux et n'a plus rien lu pendant 118 s.
//!
//! Pourquoi aucun `dlna_stop` dans le journal : sur le chemin d'une lecture,
//! l'orchestrateur n'appelle jamais `OutputTarget::stop` pour une sortie
//! réseau ; seul `play_media` envoie un `Stop` « de politesse », sans attendre
//! sa réponse plus de 500 ms et journalisé en `debug`, puis interroge
//! `GetTransportInfo`. Rien n'attend que le renderer ait réellement quitté le
//! flux de la radio : un `STOPPED` déclaré (que le DMP-A8 donne volontiers
//! pendant qu'il lit encore, cf. `dlna_evenement_contredit_par_le_transport`)
//! suffit à poser la nouvelle URI.
//!
//! Le correctif, sur les sorties DLNA et quand le flux quitté est une radio :
//! `Stop` d'abord (le vrai, `OutputTarget::stop`), puis attendre, au plus
//! [`DELAI_FERMETURE_RADIO`], que le compteur `active_consumers` de la session
//! radio — tenu par `RadioConsumerGuard` dans tune-stream-http — retombe à
//! zéro ; ensuite seulement fermer la session et poser la nouvelle URI.
//!
//! La session n'est fermée qu'APRÈS l'attente : la fermer avant tarirait le
//! corps HTTP de notre côté, et le compteur tomberait sans que le renderer ait
//! rien lâché.
//!
//! Titre → titre, radio sur une autre sortie : rien ne change.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use super::PlaybackOrchestrator;
use crate::http::streamer::StreamSession;
use crate::playback::NowPlaying;

/// Attente maximale du départ du consommateur HTTP de la radio après `Stop`.
pub(crate) const DELAI_FERMETURE_RADIO: Duration = Duration::from_secs(2);
/// Borne du `Stop` lui-même : un renderer muet ne doit pas retenir la lecture
/// le temps des réessais SOAP.
const DELAI_DU_STOP: Duration = Duration::from_secs(2);
/// Pas de sondage du compteur de consommateurs.
const PAS_DE_SONDAGE: Duration = Duration::from_millis(20);

/// Ce qu'a donné la sortie de radio, tel que journalisé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BilanSortieDeRadio {
    pub stop_envoye: bool,
    pub radio_fermee_en_ms: u64,
    pub delai_depasse: bool,
}

/// Consommateurs HTTP encore accrochés à ces sessions.
fn consommateurs(sessions: &[Arc<StreamSession>]) -> u32 {
    sessions
        .iter()
        .map(|s| s.active_consumers.load(Ordering::Relaxed))
        .sum()
}

/// Attendre que plus personne ne lise ces sessions, au plus `delai`.
/// Rend la durée d'attente et si le délai a été dépassé.
pub(crate) async fn attendre_le_depart_des_consommateurs(
    sessions: &[Arc<StreamSession>],
    delai: Duration,
) -> (Duration, bool) {
    let debut = Instant::now();
    loop {
        if consommateurs(sessions) == 0 {
            return (debut.elapsed(), false);
        }
        if debut.elapsed() >= delai {
            return (debut.elapsed(), true);
        }
        tokio::time::sleep(PAS_DE_SONDAGE).await;
    }
}

impl PlaybackOrchestrator {
    /// Avant de poser une nouvelle URI sur une sortie DLNA qui jouait une
    /// radio : `Stop`, puis attente bornée du départ du renderer. Ne fait
    /// rien (et rend `None`) hors de ce cas.
    ///
    /// `ancienne` est la lecture que la nouvelle remplace ; `sessions_a_fermer`
    /// les sessions que `play_inner` s'apprête à fermer.
    pub(super) async fn sortir_de_la_radio_dlna(
        &self,
        zone_id: i64,
        device_id: Option<&str>,
        ancienne: Option<&NowPlaying>,
        sessions_a_fermer: &[String],
    ) -> Option<BilanSortieDeRadio> {
        let device_id = device_id?;
        if sessions_a_fermer.is_empty() {
            return None;
        }
        let sessions: Vec<Arc<StreamSession>> = {
            let registre = self.streamer.sessions_state();
            let registre = registre.lock().await;
            sessions_a_fermer
                .iter()
                .filter_map(|sid| registre.get(sid).cloned())
                .collect()
        };
        let source_radio = ancienne.is_some_and(|np| np.source == "radio");
        let flux_live = sessions.iter().any(|s| s.is_radio);
        if !source_radio && !flux_live {
            return None;
        }
        let sortie = { self.outputs.lock().await.get(device_id) }?;
        if sortie.lock().await.output_type() != "dlna" {
            return None;
        }

        let stop_envoye =
            match tokio::time::timeout(DELAI_DU_STOP, async { sortie.lock().await.stop().await })
                .await
            {
                Ok(Ok(())) => true,
                Ok(Err(e)) => {
                    warn!(zone_id, device_id, error = %e, "dlna_sortie_de_radio_stop_refuse");
                    false
                }
                Err(_) => {
                    warn!(zone_id, device_id, "dlna_sortie_de_radio_stop_sans_reponse");
                    false
                }
            };
        // Le DMP-A8 ne lâche PAS la radio sur `Stop` : il ne ferme qu'après
        // le `Play` suivant (12/12 au journal du .18, 22–25/10, toujours
        // `delai_depasse=true`). L'attente seule ne garantissait donc jamais
        // ce qu'elle devait garantir. Le serveur ferme lui-même le direct :
        // la connexion radio ne peut plus être ouverte au `SetAVTransportURI`.
        let mut congedies = 0usize;
        for s in sessions
            .iter()
            .filter(|s| s.is_radio && s.active_consumers.load(Ordering::Relaxed) > 0)
        {
            s.congedier_les_consommateurs();
            congedies += 1;
        }
        let (attente, delai_depasse) =
            attendre_le_depart_des_consommateurs(&sessions, DELAI_FERMETURE_RADIO).await;
        let bilan = BilanSortieDeRadio {
            stop_envoye,
            radio_fermee_en_ms: attente.as_millis() as u64,
            delai_depasse,
        };
        info!(
            zone_id,
            device_id,
            stop_envoye = bilan.stop_envoye,
            radio_fermee_en_ms = bilan.radio_fermee_en_ms,
            delai_depasse = bilan.delai_depasse,
            radios_fermees_par_le_serveur = congedies,
            consommateurs_restants = consommateurs(&sessions),
            "dlna_sortie_de_radio"
        );
        Some(bilan)
    }
}
