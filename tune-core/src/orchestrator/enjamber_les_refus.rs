//! #7895 — « Suivant » et « Précédent » enjambent les titres que le service
//! refuse pour ce compte, comme l'enchaînement automatique le fait déjà.
//!
//! # Le fait
//!
//! Sur un album de service dont certains titres sont indisponibles pour le
//! compte (point 4 de #7778), la fin de piste saute le titre refusé
//! (`poller/fin_de_piste.rs`, `refus_propre_a_la_piste`) : la lecture
//! continue. Mais un appui sur « Suivant » qui tombe sur ce même titre
//! faisait UN seul `play_from_queue`, et l'échec arrêtait tout avec
//! « Impossible de lire la piste suivante : no url ». Deux chemins, deux
//! règles pour le même titre.
//!
//! # La règle
//!
//! Une seule, partagée par les deux gestes :
//!
//! - un refus PROPRE au titre (même liste étroite que la fin de piste,
//!   [`crate::poller::refus_de_piste::refus_propre_a_la_piste`]) fait passer
//!   au titre voisin dans le sens du geste ;
//! - toute autre erreur — une vraie panne, jeton expiré, réseau mort — se
//!   rend telle quelle, sans enjamber : on ne martèle pas le service ;
//! - la source qui dit « plus rien de cette file » (`arret_de_la_file`)
//!   arrête la recherche ;
//! - la recherche est bornée : une position déjà essayée l'arrête (file
//!   entièrement refusée en répétition), et un plafond la coupe de toute
//!   façon.
//!
//! Les titres sautés sont dits : un `playback.track_skipped` par titre, comme
//! la fin de piste, et une fois la lecture repartie un `zone.playback_error`
//! NON fatal (la zone joue) qui nomme le ou les titres et la raison.

use super::*;
use crate::db::play_queue_repo::PlayQueueRepo;
use crate::playback::RepeatMode;

/// Le sens du geste qui cherche un titre jouable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensDuGeste {
    /// « Suivant » : avance dans l'ordre de lecture (tirage compris).
    Suivant,
    /// « Précédent » : recule dans l'ordre de lecture, jusqu'au début.
    Precedent,
}

impl SensDuGeste {
    fn geste(self) -> &'static str {
        match self {
            SensDuGeste::Suivant => "suivant",
            SensDuGeste::Precedent => "précédent",
        }
    }
}

/// Nombre maximal de titres refusés qu'un seul appui enjambe. Même valeur que
/// le plafond de la fin de piste : il n'existe que pour borner une file
/// entièrement refusée.
pub const PLAFOND_REFUS_ENJAMBES_PAR_GESTE: usize = 500;

/// Un titre sauté, tel qu'on le dit à l'utilisateur.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitreSaute {
    pub position: i64,
    pub titre: Option<String>,
    pub raison: &'static str,
}

/// La raison lisible d'un refus propre au titre.
pub fn raison_du_refus(cause: &str) -> &'static str {
    if cause
        .to_lowercase()
        .contains(crate::source_url::MOTIF_PISTE_REFUSEE)
    {
        "refusé par sa source"
    } else {
        "non disponible sur le service pour ce compte"
    }
}

/// Le message de l'interface pour les titres sautés par un geste.
pub fn message_titres_sautes(sautes: &[TitreSaute]) -> Option<String> {
    let nom = |t: &TitreSaute| match t.titre.as_deref() {
        Some(titre) if !titre.trim().is_empty() => format!("« {titre} »"),
        _ => format!("le titre n° {}", t.position + 1),
    };
    match sautes {
        [] => None,
        [seul] => Some(format!("Titre sauté : {} ({}).", nom(seul), seul.raison)),
        plusieurs => {
            let noms: Vec<String> = plusieurs.iter().take(3).map(nom).collect();
            let reste = plusieurs.len().saturating_sub(3);
            let suite = if reste > 0 {
                format!(" et {reste} autre(s)")
            } else {
                String::new()
            };
            Some(format!(
                "{} titres sautés : {}{suite} ({}).",
                plusieurs.len(),
                noms.join(", "),
                plusieurs[0].raison
            ))
        }
    }
}

/// La position qui précède `position` dans l'ordre de lecture, ou `None` au
/// début. En aléatoire, on remonte le tirage ; sinon l'ordre de la file.
fn position_avant(etat: &crate::playback::ZoneState, position: i64) -> Option<i64> {
    if etat.shuffle
        && !etat.shuffle_order.is_empty()
        && let Some(rang) = etat
            .shuffle_order
            .iter()
            .position(|&p| p as i64 == position)
    {
        return rang.checked_sub(1).map(|r| etat.shuffle_order[r] as i64);
    }
    (position > 0).then(|| position - 1)
}

impl PlaybackOrchestrator {
    /// Jouer `depart` ; si le service refuse CE titre, passer au voisin dans
    /// le sens du geste, jusqu'au premier titre jouable (#7895).
    ///
    /// Rend le résultat du titre démarré, ou l'erreur qui a arrêté la
    /// recherche : la vraie panne telle quelle, ou un motif qui dit qu'aucun
    /// titre jouable n'a été trouvé.
    pub async fn jouer_en_enjambant_les_refus(
        &self,
        zone_id: i64,
        depart: i64,
        sens: SensDuGeste,
    ) -> Result<PlayResult, String> {
        let mut position = depart;
        let mut essayees = std::collections::HashSet::new();
        let mut sautes: Vec<TitreSaute> = Vec::new();
        loop {
            let cause = match self.play_from_queue(zone_id, position).await {
                Ok(resultat) => {
                    self.dire_titres_sautes(zone_id, sens, &sautes);
                    return Ok(resultat);
                }
                Err(e) => e,
            };
            if crate::source_url::arret_de_la_file(&cause)
                || !crate::poller::refus_de_piste::refus_propre_a_la_piste(&cause)
            {
                // Vraie panne, ou la source arrête la file : rien à enjamber.
                return Err(cause);
            }
            essayees.insert(position);
            let titre = PlayQueueRepo::with_backend(self.db.clone())
                .get_at(zone_id, position)
                .ok()
                .flatten()
                .and_then(|l| l.title);
            info!(
                zone_id,
                position,
                geste = sens.geste(),
                cause = %cause,
                "geste_enjambe_piste_refusee"
            );
            if let Some(ref bus) = self.event_bus {
                bus.emit(
                    "playback.track_skipped",
                    serde_json::json!({
                        "zone_id": zone_id,
                        "position": position,
                        "title": titre,
                        "reason": cause,
                    }),
                );
            }
            sautes.push(TitreSaute {
                position,
                titre,
                raison: raison_du_refus(&cause),
            });

            let aucun_jouable = || {
                format!(
                    "aucun titre jouable, {} titre(s) refusé(s) par le service \
                     ({cause})",
                    sautes.len()
                )
            };
            if sautes.len() >= PLAFOND_REFUS_ENJAMBES_PAR_GESTE {
                warn!(
                    zone_id,
                    refuses = sautes.len(),
                    "geste_plafond_des_refus_atteint"
                );
                return Err(aucun_jouable());
            }
            let mut etat = self.playback.get_state(zone_id).await;
            let voisine = match sens {
                SensDuGeste::Suivant => {
                    // Un geste manuel ignore la répétition d'un titre, comme
                    // `next_position_manual` (#1110).
                    if etat.repeat == RepeatMode::One {
                        etat.repeat = RepeatMode::All;
                    }
                    match crate::poller::PositionPoller::next_position_after(&etat, position) {
                        Some(p) => self
                            .enjamber_les_pistes_bannies(zone_id, p)
                            .await
                            .position_depuis(p),
                        None => None,
                    }
                }
                SensDuGeste::Precedent => position_avant(&etat, position),
            };
            match voisine {
                Some(p) if !essayees.contains(&p) => position = p,
                // Fin (ou début) de file, ou retour sur un titre déjà
                // refusé : la file n'a plus de titre jouable dans ce sens.
                _ => return Err(aucun_jouable()),
            }
        }
    }

    /// Dire à toutes les télécommandes quels titres un geste a sautés.
    /// `fatal: false` : la zone joue le titre trouvé.
    fn dire_titres_sautes(&self, zone_id: i64, sens: SensDuGeste, sautes: &[TitreSaute]) {
        let Some(message) = message_titres_sautes(sautes) else {
            return;
        };
        info!(zone_id, geste = sens.geste(), %message, "geste_titres_sautes");
        if let Some(ref bus) = self.event_bus {
            bus.emit(
                "zone.playback_error",
                serde_json::json!({
                    "zone_id": zone_id,
                    "error": message,
                    "fatal": false,
                    "pistes_sautees": sautes
                        .iter()
                        .map(|t| serde_json::json!({
                            "position": t.position,
                            "title": t.titre,
                            "reason": t.raison,
                        }))
                        .collect::<Vec<_>>(),
                }),
            );
        }
    }
}
