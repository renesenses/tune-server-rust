//! #7965 (décision de Bertrand du 05/11/2026) — l'historique partagé
//! (`GET /library/history`) contient les épisodes de podcast et les radios.
//!
//! - **Radio** : UNE ligne par lancement de station, au NOM de la station et
//!   avec son logo, jamais le titre ICY du moment (un instantané figé, le
//!   défaut que la v0.8.258 avait réglé en n'écrivant plus rien). La ligne
//!   porte `context_type = 'radio'` : c'est ce qui la distingue, à la
//!   lecture, des anciennes lignes de radio à titre ICY, qui restent cachées.
//!   Une radio n'a pas de durée : la ligne est écrite au lancement
//!   (`armer_l_ecoute`). Un changement de titre ICY ne met à jour que
//!   `now_playing` dans le sondeur et n'écrit rien ; les relances
//!   automatiques du flux passent par `play_without_history`.
//! - **Podcast** : `play_episode` (route du serveur) joue un épisode hors
//!   zone navigateur sans passer par `play` ; il écrit sa ligne par
//!   [`PlaybackOrchestrator::enregistrer_ecoute_podcast`], soumise au même
//!   seuil que les pistes (#7852).
//!
//! La nature `radio` ou `podcast` du contexte tient ces lignes hors de
//! « Reprendre l'écoute » (`listening_resume::started` n'y écrit que les
//! albums, playlists et pistes) : la reprise d'un épisode reste écrite à part
//! (`podcast_started_in_context`). Les statistiques d'écoute excluent toujours
//! `source = 'radio'` (`history_repo`).
use super::{ContexteEcoute, PlaybackOrchestrator};
use crate::db::radio_repo::RadioRepo;

/// Le nom et le logo sous lesquels une écoute de radio est écrite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Station {
    pub(super) id: Option<i64>,
    pub(super) nom: String,
    pub(super) logo: Option<String>,
}

impl PlaybackOrchestrator {
    /// La station de `url` : celle de « Mes radios » quand l'adresse y est,
    /// sinon le nom donné au lancement (`album_title`, puis `title`, que
    /// `jouer_un_flux` remplit tous deux du nom de la station).
    pub(super) fn station_de_l_ecoute(
        &self,
        url: Option<&str>,
        titre: &str,
        album: Option<&str>,
        pochette: Option<&str>,
    ) -> Station {
        let connue = url.and_then(|u| {
            RadioRepo::with_backend(self.db.clone())
                .get_by_url(u)
                .ok()
                .flatten()
        });
        let nom_donne = album
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .unwrap_or(titre)
            .to_string();
        match connue {
            Some(s) => Station {
                id: s.id,
                nom: if s.name.trim().is_empty() {
                    nom_donne
                } else {
                    s.name
                },
                logo: s.logo_url.or_else(|| pochette.map(Into::into)),
            },
            None => Station {
                id: None,
                nom: nom_donne,
                logo: pochette.map(Into::into),
            },
        }
    }

    /// Écrit la ligne d'historique d'un lancement de radio : le nom de la
    /// station, son logo, `context_type = 'radio'`. Appelée par les deux
    /// chemins d'annonce (sortie et zone navigateur), seulement pour une
    /// lecture nouvelle (`record_history`).
    pub(super) fn enregistrer_ecoute_radio(
        &self,
        zone_id: i64,
        url: Option<&str>,
        titre: &str,
        album: Option<&str>,
        pochette: Option<&str>,
        session_profile_id: Option<i64>,
    ) {
        let station = self.station_de_l_ecoute(url, titre, album, pochette);
        let id = station
            .id
            .map(|i| i.to_string())
            .or_else(|| url.map(str::to_string));
        tracing::info!(zone_id, station = %station.nom, "historique_radio_station");
        self.record_listen(
            &station.nom,
            None,
            None,
            "radio",
            url,
            None,
            0,
            zone_id,
            station.logo.as_deref(),
            session_profile_id,
            ContexteEcoute {
                nature: Some("radio"),
                id: id.as_deref(),
                rang: None,
                service: Some("radio"),
                titre: Some(&station.nom),
                pochette: station.logo.as_deref(),
            },
            None,
            None,
        );
    }

    /// Arme la ligne d'historique d'un épisode de podcast joué hors du chemin
    /// `play` (`POST /podcasts/play/{zone_id}`, zones à périphérique). Écrite
    /// au seuil min(30 s, 50 %) par le sondeur, ou tout de suite si la durée
    /// est inconnue, comme une piste (#7852).
    // Un argument par colonne écrite, comme `record_listen`.
    #[allow(clippy::too_many_arguments)]
    pub async fn enregistrer_ecoute_podcast(
        &self,
        zone_id: i64,
        titre: &str,
        podcast: &str,
        pochette: Option<&str>,
        audio_url: &str,
        duree_ms: i64,
        podcast_id: Option<&str>,
    ) {
        let session_profile_id = self.playback.get_state(zone_id).await.session_profile_id;
        self.record_listen(
            titre,
            Some(podcast),
            Some(podcast),
            "podcast",
            Some(audio_url),
            None,
            duree_ms,
            zone_id,
            pochette,
            session_profile_id,
            ContexteEcoute {
                nature: Some("podcast"),
                id: podcast_id.or(Some(podcast)),
                rang: None,
                service: Some("podcast"),
                titre: Some(podcast),
                pochette,
            },
            None,
            None,
        );
    }
}
