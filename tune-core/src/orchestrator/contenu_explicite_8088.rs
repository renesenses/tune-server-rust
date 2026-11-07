//! #8088 — contrôle parental : la lecture d'un contenu marqué explicite est
//! refusée pour un profil dont la politique masque l'explicite
//! (`PolitiqueParentale::masquer_explicite`).
//!
//! La garde est au point d'étranglement de toutes les entrées de lecture,
//! `play_par_l_entree` (geste, file, fin de piste du sondeur, alarmes,
//! renderer UPnP…). Une piste refusée en fin de piste est traitée comme une
//! piste qui ne se lit pas : la file passe à la suivante.
//!
//! Le marquage :
//! - piste locale : `tracks.explicit = 1`, lu au scan dans la balise
//!   `ITUNESADVISORY` (MP4 `rtng`) ;
//! - piste d'un service : `StreamTrack::explicit`, tel que le service le dit
//!   (Qobuz `parental_warning`, TIDAL et Spotify `explicit`), relu par
//!   `get_track` au départ de la lecture. Un service injoignable ne dit rien :
//!   la piste n'est pas tenue pour explicite (sa lecture échouera de toute
//!   façon sans le service).
//!
//! Une piste sans marquage n'est pas explicite. Un profil sans politique, ou
//! dont la politique ne masque pas l'explicite, ne coûte qu'une lecture de
//! la politique (clé primaire).
//!
//! L'enchaînement sans blanc ne repasse pas par cette garde : pour un profil
//! qui masque l'explicite, la piste suivante n'est pas armée d'avance sur le
//! renderer (voir `poller::fin_de_piste`), la fin de piste passe par ici.

use super::{PlayRequest, PlaybackOrchestrator};
use crate::db::politique_parentale as pp;

/// La sentinelle de la lecture refusée, reconnue par les routes.
pub fn sentinelle_explicite() -> String {
    format!("{}: contenu marqué explicite", pp::CODE_REFUS_EXPLICITE)
}

impl PlaybackOrchestrator {
    /// La politique du profil qui écoute sur la zone masque-t-elle les
    /// contenus explicites ? Une politique illisible les masque.
    pub async fn masque_l_explicite(&self, zone_id: i64) -> bool {
        let etat = self.playback.get_state(zone_id).await;
        let profil =
            crate::db::hidden_repo::profil_des_bannis_de_la_zone(&self.db, etat.session_profile_id);
        match pp::lire(&self.db, profil) {
            Ok(Some(p)) => p.politique.masquer_explicite,
            Ok(None) => false,
            Err(e) if pp::table_absente(&e) => false,
            Err(e) => {
                tracing::warn!(zone_id, profil, error = %e, "parental_explicit_policy_unreadable");
                true
            }
        }
    }

    /// La garde de départ : `Err` porte la sentinelle `parental_explicit_refused`.
    pub(super) async fn verifier_le_contenu_explicite(
        &self,
        req: &PlayRequest,
    ) -> Result<(), String> {
        if !self.masque_l_explicite(req.zone_id).await {
            return Ok(());
        }
        if !self.demande_explicite(req).await {
            return Ok(());
        }
        tracing::info!(
            zone_id = req.zone_id,
            track_id = ?req.track_id,
            source = ?req.source,
            source_id = ?req.source_id,
            "parental_explicit_refused"
        );
        Err(sentinelle_explicite())
    }

    /// La piste demandée est-elle marquée explicite ?
    async fn demande_explicite(&self, req: &PlayRequest) -> bool {
        let source = req.source.as_deref().unwrap_or("local");
        if let Some(id) = req.track_id
            && matches!(source, "local" | "library" | "")
        {
            return pp::piste_locale_explicite(&self.db, id).unwrap_or_else(|e| {
                tracing::warn!(track_id = id, error = %e, "parental_explicit_track_unreadable");
                true
            });
        }
        let Some(source_id) = req.source_id.as_deref().filter(|s| !s.is_empty()) else {
            return false;
        };
        let service = self.services.lock().await.get_actif(source).await;
        let Some(service) = service else {
            return false;
        };
        let service = service.read().await;
        match service.get_track(source_id).await {
            Ok(piste) => piste.explicit,
            Err(e) => {
                tracing::debug!(source, source_id, error = %e, "parental_explicit_service_silent");
                false
            }
        }
    }
}
