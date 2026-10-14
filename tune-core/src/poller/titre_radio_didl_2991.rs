//! #2991 — relayer le changement de morceau radio par DIDL vers un renderer
//! DLNA, quand la zone le demande.
//!
//! Appelé par `refresh_radio_metadata` à chaque changement de titre, APRÈS la
//! publication ICY qui, elle, ne change pas. La voie et ses limites sont
//! décrites dans [`crate::outputs::dlna::titre_radio_2991`].

use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::db::backend::DbBackend;
use crate::outputs::dlna::DlnaOutput;
use crate::outputs::dlna::titre_radio_2991::{
    IssueRelai, ModeTitreRadioDidl, TitreRadio, mode_de_zone,
};
use crate::outputs::registry::OutputRegistry;

/// Réémet le titre radio vers la sortie DLNA de la zone, si la zone le
/// demande et si la sortie joue bien CE flux.
///
/// Zone non réglée : rend aussitôt [`IssueRelai::Ignore`], sans verrou ni
/// action SOAP.
pub(crate) async fn relayer_titre_radio_par_didl(
    db: &Arc<dyn DbBackend>,
    outputs: &Arc<Mutex<OutputRegistry>>,
    zone_id: i64,
    device_id: Option<&str>,
    stream_id: Option<&str>,
    titre: &TitreRadio,
) -> IssueRelai {
    let mode = mode_de_zone(db, zone_id);
    if mode == ModeTitreRadioDidl::Desactive {
        return IssueRelai::Ignore("reglage_desactive");
    }
    let Some(device_id) = device_id else {
        return IssueRelai::Ignore("zone_sans_sortie");
    };
    let Some(stream_id) = stream_id else {
        return IssueRelai::Ignore("stream_id_absent");
    };
    let Some(sortie) = outputs.lock().await.get(device_id) else {
        return IssueRelai::Ignore("sortie_absente_du_registre");
    };
    let guard = sortie.lock().await;
    let Some(dlna) = guard.as_any().downcast_ref::<DlnaOutput>() else {
        return IssueRelai::Ignore("sortie_non_dlna");
    };
    // L'URI rejouée est celle que l'appareil a ACCEPTÉE, et seulement si elle
    // sert bien le flux dont on vient de lire le titre.
    let Some(pose) = dlna.pose_courante() else {
        return journaliser(
            zone_id,
            device_id,
            mode,
            titre,
            IssueRelai::Ignore("aucune_pose_connue"),
        );
    };
    if crate::http::streamer::stream_id_de_l_url(&pose.url) != Some(stream_id) {
        return journaliser(
            zone_id,
            device_id,
            mode,
            titre,
            IssueRelai::Ignore("flux_pose_different"),
        );
    }
    match dlna.reemettre_titre_radio(mode, &pose.url, titre).await {
        Ok(issue) => journaliser(zone_id, device_id, mode, titre, issue),
        Err(e) => {
            warn!(
                zone_id,
                device = device_id,
                mode = mode.as_str(),
                error = %e,
                "radio_didl_relai_echec — l'ICY continue de partir, seul le DIDL a échoué"
            );
            IssueRelai::Ignore("echec_soap")
        }
    }
}

fn journaliser(
    zone_id: i64,
    device_id: &str,
    mode: ModeTitreRadioDidl,
    titre: &TitreRadio,
    issue: IssueRelai,
) -> IssueRelai {
    let issue_txt = match &issue {
        IssueRelai::Envoye { action } => *action,
        IssueRelai::Ignore(raison) => raison,
    };
    info!(
        zone_id,
        device = device_id,
        mode = mode.as_str(),
        artiste = titre.artiste.as_deref().unwrap_or("-"),
        titre = %titre.titre,
        pochette = titre.pochette.as_deref().unwrap_or("-"),
        issue = issue_txt,
        "radio_didl_relai"
    );
    issue
}

#[cfg(test)]
#[path = "titre_radio_didl_2991_tests.rs"]
mod tests;
