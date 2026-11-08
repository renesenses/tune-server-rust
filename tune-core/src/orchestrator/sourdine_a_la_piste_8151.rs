//! #8151 — une zone en sourdine reste muette quand une nouvelle piste démarre.
//!
//! Bertrand, 08/11 (.18, Eversolo DMP-A8 en DLNA) : zone en son coupé, un clic
//! sur une piste la faisait repartir AVEC le son. La sourdine de la zone
//! (`zones.muted`) n'était envoyée qu'au geste de l'auditeur : une sortie
//! recréée, un serveur redémarré, une enceinte AirPlay sans session ou un
//! firmware qui lève sa sourdine à la nouvelle URI repartaient au volume.
//!
//! Tune réaffirme donc la sourdine de la zone à la sortie AVANT de lui
//! confier la piste (la sortie connaît alors son état, même neuve) et APRÈS
//! son démarrage (une nouvelle session, un nouveau média a pu la lever). Hors
//! sourdine, rien n'est envoyé : le volume n'est jamais touché ici.

use super::*;

/// Le moment où la sourdine est réaffirmée, tel que le journal le dit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MomentDeLaSourdine {
    AvantLaPiste,
    ApresLeDemarrage,
}

impl MomentDeLaSourdine {
    fn code(self) -> &'static str {
        match self {
            Self::AvantLaPiste => "avant_la_piste",
            Self::ApresLeDemarrage => "apres_le_demarrage",
        }
    }
}

impl PlaybackOrchestrator {
    /// La zone est-elle en sourdine ? La base (`zones.muted`) survit au
    /// redémarrage ; l'état de lecture porte le dernier geste.
    pub(super) async fn zone_en_sourdine(&self, zone_id: i64) -> bool {
        let en_base = crate::db::zone_repo::ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .is_some_and(|z| z.muted);
        en_base || self.playback.get_state(zone_id).await.muted
    }

    /// Réaffirme la sourdine de la zone sur `sortie`. Un refus est journalisé
    /// et ne fait pas échouer la lecture : la sortie qui le refuse (AirPlay 2
    /// sans aide, renderer muet) n'a pas de sourdine à perdre.
    pub(super) async fn reimposer_la_sourdine_de_la_zone(
        &self,
        zone_id: i64,
        sortie: &Arc<Mutex<Box<dyn crate::outputs::traits::OutputTarget>>>,
        moment: MomentDeLaSourdine,
    ) {
        if !self.zone_en_sourdine(zone_id).await {
            return;
        }
        let sortie = sortie.lock().await;
        match sortie.set_mute(true).await {
            Ok(()) => info!(
                zone_id,
                device_id = %sortie.device_id(),
                moment = moment.code(),
                "zone_sourdine_reimposee"
            ),
            Err(e) => warn!(
                zone_id,
                device_id = %sortie.device_id(),
                moment = moment.code(),
                error = %e,
                "zone_sourdine_reimposee_echoue"
            ),
        }
    }
}
