//! #8611 — contrôle parental : la lecture d'une piste dont l'artiste est
//! bloqué par la politique du profil qui écoute est refusée.
//!
//! Même point d'étranglement que la garde des contenus explicites (#8088) :
//! `play_par_l_entree` (geste, file, fin de piste du sondeur, alarmes,
//! renderer UPnP…). Le refus porte le code stable `parental_content_blocked`,
//! la zone ne joue pas. Le refus est PROPRE à la piste
//! (`poller::refus_de_piste`) : en fin de piste ou sur « Suivant », la piste
//! bloquée est sautée (`playback.track_skipped`) et journalisée, la file
//! continue.
//!
//! L'artiste :
//! - piste locale : artiste de la piste, artiste de l'album, balise
//!   `ALBUMARTIST`, lus en base par clé primaire
//!   ([`pp::piste_locale_bloquee`]) ;
//! - piste d'un service ou d'un serveur média : le nom porté par la demande,
//!   puis celui que le service rend pour la piste (`get_track`). Un service
//!   injoignable ne dit rien : la piste n'est pas tenue pour bloquée (sa
//!   lecture échouera de toute façon sans le service).
//!
//! Un profil sans politique, ou sans artiste bloqué, ne coûte qu'une lecture
//! de la politique (clé primaire). Une politique illisible refuse la lecture,
//! comme elle refuse les listes.

use super::{PlayRequest, PlaybackOrchestrator};
use crate::db::politique_parentale as pp;

/// La sentinelle de la lecture refusée, reconnue par les routes.
pub fn sentinelle_contenu_bloque() -> String {
    format!(
        "{}: artiste bloqué par le contrôle parental",
        pp::CODE_REFUS_CONTENU_BLOQUE
    )
}

/// La politique du profil qui écoute sur une zone, telle que la lecture la
/// voit : aucune (pas de ligne, ou pas de table), posée, ou illisible.
pub(crate) enum PolitiqueDeLaZone {
    Aucune,
    Posee(pp::PolitiqueParentale),
    Illisible,
}

impl PlaybackOrchestrator {
    pub(crate) async fn politique_de_la_zone(&self, zone_id: i64) -> PolitiqueDeLaZone {
        let etat = self.playback.get_state(zone_id).await;
        let profil =
            crate::db::hidden_repo::profil_des_bannis_de_la_zone(&self.db, etat.session_profile_id);
        match pp::lire(&self.db, profil) {
            Ok(Some(p)) => PolitiqueDeLaZone::Posee(p.politique),
            Ok(None) => PolitiqueDeLaZone::Aucune,
            Err(e) if pp::table_absente(&e) => PolitiqueDeLaZone::Aucune,
            Err(e) => {
                tracing::warn!(zone_id, profil, error = %e, "parental_policy_unreadable");
                PolitiqueDeLaZone::Illisible
            }
        }
    }

    /// Une garde parentale examine-t-elle chaque piste jouée sur la zone
    /// (contenus explicites #8088, artistes bloqués #8611) ? Alors rien ne
    /// s'arme d'avance sur le renderer (enchaînement sans blanc, flux
    /// continu) : chaque piste repasse par `play_par_l_entree`.
    pub async fn garde_parentale_par_piste(&self, zone_id: i64) -> bool {
        match self.politique_de_la_zone(zone_id).await {
            PolitiqueDeLaZone::Aucune => false,
            PolitiqueDeLaZone::Posee(p) => p.masquer_explicite || !p.artistes_bloques.is_empty(),
            PolitiqueDeLaZone::Illisible => true,
        }
    }

    /// La garde de départ : `Err` porte la sentinelle `parental_content_blocked`.
    pub(super) async fn verifier_l_artiste_bloque(&self, req: &PlayRequest) -> Result<(), String> {
        let politique = match self.politique_de_la_zone(req.zone_id).await {
            PolitiqueDeLaZone::Aucune => return Ok(()),
            PolitiqueDeLaZone::Illisible => {
                tracing::info!(
                    zone_id = req.zone_id,
                    "parental_content_blocked_policy_unreadable"
                );
                return Err(sentinelle_contenu_bloque());
            }
            PolitiqueDeLaZone::Posee(p) => p,
        };
        if politique.artistes_bloques.is_empty() {
            return Ok(());
        }
        let filtre = pp::Filtre::depuis(&politique);
        if !self.demande_d_un_artiste_bloque(&filtre, req).await {
            return Ok(());
        }
        tracing::info!(
            zone_id = req.zone_id,
            track_id = ?req.track_id,
            source = ?req.source,
            source_id = ?req.source_id,
            artist = ?req.artist_name,
            "parental_content_blocked"
        );
        Err(sentinelle_contenu_bloque())
    }

    /// #8611 — la piste demandée est-elle d'un artiste que `filtre` bloque ?
    /// Publique : les routes qui servent le flux d'une piste par son
    /// identifiant (`/library/tracks/{id}/audio`, l'adresse du flux d'un
    /// service) posent la même question.
    pub async fn demande_d_un_artiste_bloque(
        &self,
        filtre: &pp::Filtre,
        req: &PlayRequest,
    ) -> bool {
        if !filtre.bloque_des_artistes() {
            return false;
        }
        let source = req.source.as_deref().unwrap_or("local");
        if let Some(id) = req.track_id
            && matches!(source, "local" | "library" | "")
        {
            return pp::piste_locale_bloquee(&self.db, filtre, id).unwrap_or_else(|e| {
                tracing::warn!(track_id = id, error = %e, "parental_content_track_unreadable");
                true
            });
        }
        if filtre.artiste_bloque(None, req.artist_name.as_deref()) {
            return true;
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
            Ok(piste) => filtre.artiste_bloque(None, Some(&piste.artist)),
            Err(e) => {
                tracing::debug!(source, source_id, error = %e, "parental_content_service_silent");
                false
            }
        }
    }
}
