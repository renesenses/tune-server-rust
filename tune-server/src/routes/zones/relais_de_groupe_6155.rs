//! #6155 (Pierre, fil 2220) — une zone groupée joue ce que joue la zone qui
//! mène le groupe.
//!
//! Le client web forme un groupe depuis la fiche d'une zone : `POST
//! /zones/group {leader_id, zone_ids}`, la zone de la fiche en tête. Le serveur
//! rangeait le groupe dans le réglage `zone_groups`, répondait `201`, et
//! s'arrêtait là : aucun module de lecture ne lisait ce réglage. Lancer un
//! album sur le Devialet Phantom (meneuse) le faisait jouer seul ; la KEF
//! LS50 II (suiveuse) restait muette. Le groupe « existait » sans rien faire.
//!
//! Un groupe générique est, par décision (#2215), un **groupe de commandes
//! non synchronisé** : les renderers DLNA n'ont pas d'horloge commune et le
//! contrat `synchronization.supported = false` le dit déjà. Ce module ne
//! promet donc aucune synchronisation à la milliseconde ; il fait ce que le
//! groupe de commandes annonce :
//!
//! - la meneuse lance une piste (Lire, suivant, précédent, saut dans la file,
//!   enchaînement) → chaque suiveuse reçoit la file et la piste de la meneuse,
//!   à sa position, par le même chemin que le transfert de zone
//!   ([`crate::routes::playback::do_transfer`], source NON arrêtée) ;
//! - la meneuse se met en pause → les suiveuses aussi ; elle reprend → elles
//!   reprennent ;
//! - la meneuse s'arrête → les suiveuses s'arrêtent.
//!
//! La décision se prend sur l'ÉTAT des deux zones, relu au moment d'agir, et
//! jamais sur le seul nom de l'événement : une suiveuse déjà sur la bonne
//! piste n'est pas relancée (elle enchaîne seule grâce à la file copiée), et
//! un arrêt fugace de la meneuse entre deux pistes ne coupe pas les
//! suiveuses ([`DELAI_AVANT_D_ARRETER`]).
//!
//! Seuls les gestes de la MENEUSE sont relayés : une commande donnée sur une
//! suiveuse ne vaut que pour elle.

use std::time::Duration;

use serde_json::Value;
use tracing::{info, warn};

use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::playback::{PlayState, ZoneState};

use crate::state::AppState;

/// Un arrêt de la meneuse n'est relayé que s'il dure : entre deux pistes,
/// l'orchestrateur peut repasser par « arrêté » le temps d'ouvrir le flux
/// suivant. Couper les suiveuses à ce moment-là puis les relancer ferait un
/// trou audible sur chacune.
pub(crate) const DELAI_AVANT_D_ARRETER: Duration = Duration::from_millis(1500);

/// Ce qu'une suiveuse doit faire pour retrouver la meneuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Geste {
    /// Prendre la file et la piste de la meneuse, à sa position, et jouer.
    Rejoindre,
    Pause,
    Reprise,
    Arret,
}

/// L'élément joué, tel qu'on le compare d'une zone à l'autre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Piste {
    pub(crate) source: String,
    pub(crate) track_id: Option<i64>,
    pub(crate) source_id: Option<String>,
}

/// Ce que le relais lit d'une zone : son état et sa piste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Instantane {
    pub(crate) etat: PlayState,
    pub(crate) piste: Option<Piste>,
}

impl Instantane {
    pub(crate) fn de(zone: &ZoneState) -> Self {
        Self {
            etat: zone.state,
            piste: zone.now_playing.as_ref().map(|np| Piste {
                source: np.source.clone(),
                track_id: np.track_id,
                source_id: np.source_id.clone(),
            }),
        }
    }
}

/// Les zones qui suivent `meneuse`, dans l'ordre des groupes puis de leurs
/// `zone_ids`, sans doublon et sans la meneuse elle-même.
///
/// La meneuse d'un groupe est son `leader_id` (ce qu'envoie le client web),
/// sinon son `leader_zone_id` (forme de `/zone-manager/groups`), sinon la
/// première de ses zones — la règle même de `create_group`.
pub(crate) fn zones_suiveuses(groupes: &[Value], meneuse: i64) -> Vec<i64> {
    let mut suiveuses = Vec::new();
    for groupe in groupes {
        let zones: Vec<i64> = groupe["zone_ids"]
            .as_array()
            .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        let tete = groupe["leader_id"]
            .as_i64()
            .or_else(|| groupe["leader_zone_id"].as_i64())
            .or_else(|| zones.first().copied());
        if tete != Some(meneuse) {
            continue;
        }
        for id in zones {
            if id != meneuse && !suiveuses.contains(&id) {
                suiveuses.push(id);
            }
        }
    }
    suiveuses
}

/// Le geste qui ramène `suiveuse` sur `meneuse`, ou `None` si elle y est déjà.
pub(crate) fn geste_pour_la_suiveuse(meneuse: &Instantane, suiveuse: &Instantane) -> Option<Geste> {
    let meme_piste = meneuse.piste.is_some() && meneuse.piste == suiveuse.piste;
    match meneuse.etat {
        PlayState::Playing => match suiveuse.etat {
            PlayState::Playing if meme_piste => None,
            PlayState::Paused if meme_piste => Some(Geste::Reprise),
            _ if meneuse.piste.is_none() => None,
            _ => Some(Geste::Rejoindre),
        },
        PlayState::Paused => (suiveuse.etat == PlayState::Playing).then_some(Geste::Pause),
        PlayState::Stopped => (suiveuse.etat != PlayState::Stopped).then_some(Geste::Arret),
    }
}

fn groupes_enregistres(state: &AppState) -> Vec<Value> {
    SettingsRepo::with_backend(state.backend.clone())
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Ramène chaque suiveuse de `meneuse` sur ce que joue la meneuse, maintenant.
///
/// Appelé par le relais à chaque geste de la meneuse, et par `create_group`
/// quand le groupe se forme autour d'une zone qui joue déjà.
pub(crate) async fn aligner_les_suiveuses(state: &AppState, meneuse: i64) {
    let suiveuses = zones_suiveuses(&groupes_enregistres(state), meneuse);
    if suiveuses.is_empty() {
        return;
    }
    let tete = Instantane::de(&state.playback.get_state(meneuse).await);
    for suiveuse in suiveuses {
        let etat = Instantane::de(&state.playback.get_state(suiveuse).await);
        let Some(geste) = geste_pour_la_suiveuse(&tete, &etat) else {
            continue;
        };
        info!(meneuse, suiveuse, geste = ?geste, "zone_group_follower_aligned");
        let device_id = ZoneRepo::with_backend(state.backend.clone())
            .get(suiveuse)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id);
        let resultat = match geste {
            Geste::Rejoindre => {
                let reponse =
                    crate::routes::playback::do_transfer(state, meneuse, suiveuse, false).await;
                if reponse.status().is_success() {
                    Ok(())
                } else {
                    Err(format!("transfer status {}", reponse.status()))
                }
            }
            Geste::Pause => state
                .orchestrator
                .pause(suiveuse, device_id.as_deref())
                .await
                .map_err(|e| format!("{e:?}")),
            Geste::Reprise => state
                .orchestrator
                .resume(suiveuse, device_id.as_deref())
                .await
                .map_err(|e| format!("{e:?}")),
            Geste::Arret => {
                state
                    .orchestrator
                    .stop(suiveuse, device_id.as_deref())
                    .await;
                Ok(())
            }
        };
        if let Err(error) = resultat {
            warn!(meneuse, suiveuse, geste = ?geste, %error, "zone_group_follower_failed");
        }
    }
}

/// Lance le relais : une tâche qui écoute le canal de lecture et aligne les
/// suiveuses après chaque geste d'une meneuse. Appelé une fois au démarrage
/// (`background.rs`).
pub fn lancer_le_relais_de_groupe(state: &AppState) {
    let state = state.clone();
    let mut evenements = state.playback.subscribe();
    tokio::spawn(async move {
        loop {
            let evenement = match evenements.recv().await {
                Ok(evenement) => evenement,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(perdus)) => {
                    warn!(perdus, "zone_group_relay_lagged");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            let meneuse = evenement.zone_id;
            match evenement.event.as_str() {
                "started" | "paused" | "resumed" => aligner_les_suiveuses(&state, meneuse).await,
                // Différé hors de la boucle : l'attente ne retarde pas les
                // gestes des autres zones.
                "stopped" if !zones_suiveuses(&groupes_enregistres(&state), meneuse).is_empty() => {
                    let state = state.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(DELAI_AVANT_D_ARRETER).await;
                        aligner_les_suiveuses(&state, meneuse).await;
                    });
                }
                _ => {}
            }
        }
    });
}

#[cfg(test)]
#[path = "relais_de_groupe_6155_tests.rs"]
mod tests;
