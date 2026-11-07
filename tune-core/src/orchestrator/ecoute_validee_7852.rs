//! #7852 (fil 2646, go de Bertrand du 04/11/2026) — une écoute n'est validée
//! qu'au bout de **min(30 s, 50 % de la durée du titre)**.
//!
//! Avant, `record_listen` écrivait la ligne d'historique et l'entrée
//! « Reprendre l'écoute » dès que la sortie avait accepté le flux : un titre
//! zappé au bout de 5 s comptait une écoute (le compteur d'écoutes et les
//! listes intelligentes se calculent sur `listen_history`) et faisait naître
//! son album ou sa playlist dans « Reprendre l'écoute ».
//!
//! Désormais, `record_listen` ARME une écoute en attente pour la zone, avec
//! tout ce qu'il faut pour l'écrire plus tard. Le seul point de décision est
//! [`PlaybackOrchestrator::valider_l_ecoute`], appelé par le sondeur à chaque
//! tour pour chaque zone en lecture (zones navigateur comprises) : quand la
//! position du titre courant atteint le seuil, l'écoute est écrite une fois
//! (historique, puis reprise) et l'attente est vidée.
//!
//! - Un titre suivant arme sa propre écoute et remplace l'attente : un titre
//!   zappé ne laisse rien, et un album dont on zappe tout ne laisse aucun
//!   conteneur fantôme. Le conteneur naît avec son premier titre validé.
//! - Titre sans durée connue (flux) : on garde la règle d'aujourd'hui,
//!   l'écoute est écrite au démarrage. Les radios aussi : une ligne par
//!   lancement, au nom de la station (#7965, `historique_radio_podcast_7965`).
//! - Podcasts : la position de reprise d'un épisode reste écrite à part
//!   (`podcast_started_in_context`, recul de #7280/#7840) ; seule sa ligne
//!   d'historique attend le seuil.
//! - Transfert de zone : l'attente de la zone d'origine est DÉPLACÉE vers la
//!   cible ([`PlaybackOrchestrator::transferer_l_ecoute_en_attente`]). Seuil
//!   déjà franchi sur l'origine : plus rien n'attend, rien n'est réécrit.
//!   Sinon la cible valide une seule fois. Une seule écoute dans tous les cas.
//! - La position est celle de la zone, comme pour le scrobble : un saut en
//!   avant au-delà du seuil valide l'écoute.
//! - Arrêt : l'attente n'est PAS vidée par `stop`. Une zone arrêtée n'est pas
//!   « en lecture », le point de décision ne la valide donc pas ; et si le
//!   même titre repart, il est bien écouté : il valide au seuil, une fois.
//!   Un autre titre remplace l'attente (`est_le_titre_joue` l'écarte de
//!   toute façon). Le transfert de zone compte sur cet ordre : il déplace
//!   l'attente vers la cible avant d'arrêter la source.
//! - Redémarrage du serveur : l'attente vit en mémoire ; un titre joué moins
//!   que son seuil au moment d'un redémarrage n'est pas compté.
use super::{ContexteEcoute, PlaybackOrchestrator};
use crate::playback::{PlayState, ZoneState};

/// Plafond du seuil : 30 s, quelle que soit la durée du titre.
pub const SEUIL_MAX_MS: i64 = 30_000;

/// Le seuil de validation d'un titre de `duration_ms` : min(30 s, durée / 2).
/// `None` quand la durée est inconnue : l'écoute ne s'attend pas.
pub fn seuil_d_ecoute_ms(duration_ms: i64) -> Option<i64> {
    (duration_ms > 0).then(|| SEUIL_MAX_MS.min(duration_ms / 2))
}

/// L'écoute d'un titre de `duration_ms`, arrivé à `position_ms`, est-elle
/// validée ? Toujours vrai pour une durée inconnue (règle d'avant #7852).
pub fn ecoute_validee(duration_ms: i64, position_ms: i64) -> bool {
    match seuil_d_ecoute_ms(duration_ms) {
        Some(seuil) => position_ms >= seuil,
        None => true,
    }
}

/// Le conteneur de l'écoute, possédé : [`ContexteEcoute`] emprunte, et
/// l'attente survit à l'appelant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ContexteRetenu {
    pub(super) nature: Option<String>,
    pub(super) id: Option<String>,
    pub(super) rang: Option<i64>,
    pub(super) service: Option<String>,
    pub(super) titre: Option<String>,
    pub(super) pochette: Option<String>,
}

impl ContexteRetenu {
    pub(super) fn de(c: &ContexteEcoute<'_>) -> Self {
        Self {
            nature: c.nature.map(Into::into),
            id: c.id.map(Into::into),
            rang: c.rang,
            service: c.service.map(Into::into),
            titre: c.titre.map(Into::into),
            pochette: c.pochette.map(Into::into),
        }
    }

    pub(super) fn emprunt(&self) -> ContexteEcoute<'_> {
        ContexteEcoute {
            nature: self.nature.as_deref(),
            id: self.id.as_deref(),
            rang: self.rang,
            service: self.service.as_deref(),
            titre: self.titre.as_deref(),
            pochette: self.pochette.as_deref(),
        }
    }
}

/// Une écoute armée au démarrage d'un titre, écrite au seuil.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct EcouteEnAttente {
    pub(super) title: String,
    pub(super) artist: Option<String>,
    pub(super) album: Option<String>,
    pub(super) source: String,
    pub(super) source_id: Option<String>,
    pub(super) album_id: Option<i64>,
    pub(super) duration_ms: i64,
    pub(super) cover_url: Option<String>,
    pub(super) session_profile_id: Option<i64>,
    pub(super) contexte: ContexteRetenu,
    pub(super) album_ref: Option<String>,
}

impl EcouteEnAttente {
    /// Est-ce bien le titre que joue la zone ? Le titre et la source suffisent
    /// (les trois appelants les tirent de ce qui devient `now_playing`) ;
    /// l'identifiant de source départage quand les deux le portent.
    fn est_le_titre_joue(&self, etat: &ZoneState) -> bool {
        let Some(np) = etat.now_playing.as_ref() else {
            return false;
        };
        np.title == self.title
            && np.source == self.source
            && match (self.source_id.as_deref(), np.source_id.as_deref()) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    }
}

impl PlaybackOrchestrator {
    /// Arme l'écoute de la zone, ou l'écrit tout de suite si le titre n'a pas
    /// de durée connue. Remplace l'attente précédente de la zone.
    pub(super) fn armer_l_ecoute(&self, zone_id: i64, ecoute: EcouteEnAttente) {
        if seuil_d_ecoute_ms(ecoute.duration_ms).is_none() {
            self.ecrire_l_ecoute(zone_id, &ecoute);
            return;
        }
        if let Ok(mut attentes) = self.ecoutes_en_attente.lock() {
            attentes.insert(zone_id, ecoute);
        }
    }

    /// Le point de décision unique de #7852 : écrit l'écoute en attente de la
    /// zone quand le titre joué a atteint son seuil. Rend `true` quand elle
    /// vient d'être écrite. Appelé par le sondeur à chaque tour.
    pub fn valider_l_ecoute(&self, etat: &ZoneState) -> bool {
        if etat.state != PlayState::Playing {
            return false;
        }
        // Retirer AVANT d'écrire : le retrait est le verrou « une seule fois ».
        let ecoute = {
            let Ok(mut attentes) = self.ecoutes_en_attente.lock() else {
                return false;
            };
            match attentes.get(&etat.zone_id) {
                Some(e)
                    if e.est_le_titre_joue(etat)
                        && ecoute_validee(e.duration_ms, etat.position_ms) =>
                {
                    attentes.remove(&etat.zone_id)
                }
                _ => None,
            }
        };
        let Some(ecoute) = ecoute else {
            return false;
        };
        tracing::info!(
            zone_id = etat.zone_id,
            title = %ecoute.title,
            position_ms = etat.position_ms,
            duration_ms = ecoute.duration_ms,
            "ecoute_validee"
        );
        self.ecrire_l_ecoute(etat.zone_id, &ecoute);
        true
    }

    /// Transfert de zone : l'écoute pas encore validée suit le titre sur la
    /// cible, qui la validera une seule fois. Rien n'attend sur l'origine
    /// (seuil déjà franchi, donc écoute déjà écrite) : la cible n'a rien à
    /// écrire non plus.
    pub fn transferer_l_ecoute_en_attente(&self, depuis: i64, vers: i64) {
        if depuis == vers {
            return;
        }
        if let Ok(mut attentes) = self.ecoutes_en_attente.lock() {
            match attentes.remove(&depuis) {
                Some(ecoute) => {
                    attentes.insert(vers, ecoute);
                }
                None => {
                    attentes.remove(&vers);
                }
            }
        }
    }

    /// Bancs des lanceurs : la zone avance jusqu'au seuil de son écoute en
    /// attente, puis le point de décision est consulté, comme au tour du
    /// sondeur. Rend `true` quand l'écoute vient d'être écrite.
    #[cfg(test)]
    pub(super) async fn amener_au_seuil(&self, zone_id: i64) -> bool {
        let duree = self
            .ecoutes_en_attente
            .lock()
            .ok()
            .and_then(|a| a.get(&zone_id).map(|e| e.duration_ms));
        let Some(seuil) = duree.and_then(seuil_d_ecoute_ms) else {
            return false;
        };
        self.playback.update_position(zone_id, seuil).await;
        let etat = self.playback.get_state(zone_id).await;
        self.valider_l_ecoute(&etat)
    }

    /// Bancs qui arment une écoute sans faire jouer la zone (colonnes écrites
    /// par `record_listen`) : le seuil est réputé franchi.
    #[cfg(test)]
    pub(super) fn ecrire_l_ecoute_en_attente(&self, zone_id: i64) -> bool {
        let ecoute = self
            .ecoutes_en_attente
            .lock()
            .ok()
            .and_then(|mut a| a.remove(&zone_id));
        match ecoute {
            Some(e) => {
                self.ecrire_l_ecoute(zone_id, &e);
                true
            }
            None => false,
        }
    }

    /// Une écoute est-elle en attente sur la zone ? (diagnostic et tests)
    pub fn ecoute_en_attente(&self, zone_id: i64) -> bool {
        self.ecoutes_en_attente
            .lock()
            .map(|a| a.contains_key(&zone_id))
            .unwrap_or(false)
    }
}
