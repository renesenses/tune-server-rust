//! #3245 — le mode exclusif se règle PAR ZONE.
//!
//! Le réglage `zone_<id>_exclusive_mode` (absent = la zone suit le réglage de
//! machine `local_exclusive_mode`) est lu aux deux sites qui enregistrent une
//! sortie locale (`startup.rs`, `background.rs`). Ce module porte le réglage
//! jusqu'à une sortie DÉJÀ enregistrée : `PATCH /zones/{id}` l'aligne tout de
//! suite si rien ne joue, sinon dès que la lecture rend le périphérique.

use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use tracing::info;

use crate::state::AppState;
use tune_core::outputs::local::{ChangementDExclusif, LocalOutput};

/// Les appareils dont un alignement attend la fin d'une lecture : un seul
/// guetteur par appareil, qui relit le réglage à chaque essai.
static ALIGNEMENTS_EN_ATTENTE: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Pas du guetteur, et sa durée de vie : au-delà, le rescan périodique prend
/// le relais (il aligne lui aussi les sorties au repos).
const PAS_DU_GUETTEUR: Duration = Duration::from_secs(2);
const VIE_DU_GUETTEUR: Duration = Duration::from_secs(6 * 3600);

/// Aligne le mode exclusif de la sortie locale `device_id` sur le réglage de
/// sa zone (machine en repli), contrainte de plateforme comprise. `None` :
/// aucune sortie locale enregistrée sous ce nom.
pub async fn aligner_la_sortie(state: &AppState, device_id: &str) -> Option<ChangementDExclusif> {
    if !device_id.starts_with("local:") {
        return None;
    }
    let sortie = state.outputs.lock().await.get(device_id)?;
    let demande =
        state.demande_d_exclusif_de_l_appareil(device_id, state.requested_exclusive_mode());
    let temoin =
        tune_core::config::temoin_asio_pour_la_demande(state.exclusif_arme_par_asio(), demande);
    let configure = state.effective_audio_backend();
    let sortie = sortie.lock().await;
    let locale = sortie.as_any().downcast_ref::<LocalOutput>()?;
    let statut = tune_core::config::local_exclusive_mode_du_peripherique(
        &configure,
        locale.origin_host(),
        demande.demande,
        temoin,
    );
    let changement = locale.changer_de_mode_exclusif_au_repos(statut.effective);
    if changement != ChangementDExclusif::Inchange {
        info!(
            device_id,
            demande = demande.demande,
            demande_par_la_zone = demande.par_la_zone,
            effectif = statut.effective,
            impose = statut.forced,
            resultat = changement.code(),
            "local_output_exclusive_mode_par_zone"
        );
    }
    Some(changement)
}

/// Aligne toutes les sorties locales au repos (rescan périodique).
pub async fn aligner_toutes_les_sorties(state: &AppState) {
    let ids: Vec<String> = state
        .outputs
        .lock()
        .await
        .list()
        .into_iter()
        .filter(|id| id.starts_with("local:"))
        .collect();
    for id in ids {
        let _ = aligner_la_sortie(state, &id).await;
    }
}

/// Aligne `device_id` maintenant ; s'il joue, confie l'alignement à un
/// guetteur qui réessaie jusqu'à ce que la lecture rende le périphérique.
pub async fn aligner_maintenant_ou_apres_la_lecture(
    state: &AppState,
    device_id: &str,
) -> Option<ChangementDExclusif> {
    let changement = aligner_la_sortie(state, device_id).await;
    if changement == Some(ChangementDExclusif::DiffereEnLecture) && prendre_le_guet(device_id) {
        let state = state.clone();
        let device_id = device_id.to_string();
        tokio::spawn(async move {
            // Le guet est rendu même si l'alignement panique : sans quoi plus
            // aucun PATCH ne relancerait de guetteur pour cet appareil.
            let _guet = GuetTenu(device_id.clone());
            let debut = std::time::Instant::now();
            while debut.elapsed() < VIE_DU_GUETTEUR {
                tokio::time::sleep(PAS_DU_GUETTEUR).await;
                if aligner_la_sortie(&state, &device_id).await
                    != Some(ChangementDExclusif::DiffereEnLecture)
                {
                    break;
                }
            }
        });
    }
    changement
}

/// Rend le guet de son appareil quand le guetteur se termine, panique comprise.
struct GuetTenu(String);

impl Drop for GuetTenu {
    fn drop(&mut self) {
        rendre_le_guet(&self.0);
    }
}

fn prendre_le_guet(device_id: &str) -> bool {
    let mut guet = ALIGNEMENTS_EN_ATTENTE
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    guet.get_or_insert_with(HashSet::new)
        .insert(device_id.to_string())
}

fn rendre_le_guet(device_id: &str) {
    if let Some(guet) = ALIGNEMENTS_EN_ATTENTE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
    {
        guet.remove(device_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_seul_guetteur_par_appareil_3245() {
        let id = "local:guet-3245-test";
        assert!(prendre_le_guet(id));
        assert!(
            !prendre_le_guet(id),
            "un second guetteur ne doit pas naître"
        );
        rendre_le_guet(id);
        assert!(prendre_le_guet(id));
        drop(GuetTenu(id.to_string()));
        assert!(
            prendre_le_guet(id),
            "le guet doit être rendu à la fin du guetteur"
        );
        rendre_le_guet(id);
    }
}
