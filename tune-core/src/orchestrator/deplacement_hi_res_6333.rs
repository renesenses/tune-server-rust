//! #6333 — FabienM, zone « Salon » (Devialet, renderer Rygel
//! `DvltRygelRendererPlugin`, agent `Devialet/2.16.1`), fil 2268 : après une
//! avance dans une piste Hi-Res, plus de son, alors que la position continue.
//!
//! Ce que les journaux des 19 et 20/10 établissent : le `Seek` SOAP est
//! acquitté, le Devialet rouvre le flux à l'octet qu'il a choisi
//! (`Range: bytes=N-`), Tune le sert en entier, sans erreur, et l'appareil
//! annonce qu'il joue, mais reste muet jusqu'au changement de piste. Le son
//! revient avec une NOUVELLE adresse (Suivant, enchaînement gapless), jamais
//! avec Pause/Lecture. Le même geste sur du 16 bits / 44,1 kHz, bibliothèque
//! ou Qobuz, ne coupe pas ; tous les cas coupés sont en 24 bits.
//!
//! Le remède : sur ce renderer, une avance dans un fichier Hi-Res (plus de
//! 16 bits) ne passe plus par le `Seek` à l'octet. Tune relance le flux NATIF
//! à la position (mécanique de #6059 : en-tête réécrit, puis les trames du
//! fichier à partir de la position, bit-perfect conservé) : le Devialet
//! reçoit une nouvelle adresse qui commence au bon endroit, comme au passage
//! à la piste suivante. Le 16 bits garde le `Seek` ordinaire, qui marche.
//!
//! Le renderer est repéré par son adresse de contrôle UPnP
//! (`/Control/DvltRygelRendererPlugin…`, relevée dans le journal de #5537).
//! Le réglage de zone `zone_{id}_deplacement_hi_res_nouveau_flux` (`true` /
//! `false`, absent = ce que dit l'adresse) l'arme ou le désarme à la main.

use std::sync::Arc;

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

use super::PlaybackOrchestrator;

/// La clé de réglage qui arme (ou désarme) la règle sur la zone.
pub fn cle(zone_id: i64) -> String {
    format!("zone_{zone_id}_deplacement_hi_res_nouveau_flux")
}

/// L'adresse de contrôle est-elle celle du Rygel de Devialet ? Pur.
pub(crate) fn adresse_de_devialet(url_de_controle: &str) -> bool {
    url_de_controle
        .to_ascii_lowercase()
        .contains("/control/dvltrygelrendererplugin")
}

/// La piste est-elle « Hi-Res » au sens de ce défaut : plus de 16 bits ?
/// Une profondeur inconnue ne conclut rien : le `Seek` ordinaire reste.
fn plus_de_16_bits(bit_depth: Option<u32>) -> bool {
    bit_depth.is_some_and(|b| b > 16)
}

/// La décision, pure : le réglage de zone d'abord, sinon le renderer
/// reconnu ; puis la profondeur de la piste.
pub(crate) fn relancer_le_deplacement(
    reglage: Option<&str>,
    renderer_devialet: bool,
    bit_depth: Option<u32>,
) -> bool {
    let arme = match reglage.map(str::trim) {
        Some("true" | "1") => true,
        Some("false" | "0") => false,
        _ => renderer_devialet,
    };
    arme && plus_de_16_bits(bit_depth)
}

fn reglage_de_la_zone(db: &Arc<dyn DbBackend>, zone_id: i64) -> Option<String> {
    SettingsRepo::with_backend(db.clone())
        .get(&cle(zone_id))
        .ok()
        .flatten()
}

impl PlaybackOrchestrator {
    /// Le renderer `did` est-il le Rygel de Devialet ? Lu sur l'adresse de
    /// contrôle de la sortie DLNA enregistrée ; toute autre sortie : non.
    async fn renderer_devialet(&self, did: &str) -> bool {
        let Some(sortie) = ({ self.outputs.lock().await.get(did) }) else {
            return false;
        };
        let sortie = sortie.lock().await;
        sortie
            .as_any()
            .downcast_ref::<crate::outputs::dlna::DlnaOutput>()
            .is_some_and(|dlna| adresse_de_devialet(&dlna.url_av_transport()))
    }

    /// Une avance dans une piste de `bit_depth` bits, sur la zone `zone_id`
    /// et le renderer `did`, doit-elle relancer le flux à la position plutôt
    /// que d'envoyer un `Seek` ?
    pub(super) async fn deplacement_hi_res_par_nouveau_flux(
        &self,
        zone_id: i64,
        did: &str,
        bit_depth: Option<u32>,
    ) -> bool {
        if !plus_de_16_bits(bit_depth) {
            return false;
        }
        let reglage = reglage_de_la_zone(&self.db, zone_id);
        let devialet = match reglage.as_deref().map(str::trim) {
            Some("true" | "1" | "false" | "0") => false,
            _ => self.renderer_devialet(did).await,
        };
        relancer_le_deplacement(reglage.as_deref(), devialet, bit_depth)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l_adresse_du_rygel_de_devialet_est_reconnue_6333() {
        assert!(adresse_de_devialet(
            "http://192.168.1.39:43403/Control/DvltRygelRendererPlugin/RygelAVTransport"
        ));
        assert!(adresse_de_devialet(
            "http://192.168.1.39:43403/control/dvltrygelrendererplugin/x"
        ));
        assert!(!adresse_de_devialet(
            "http://192.168.1.20:49152/Control/RygelPlaybinRendererPlugin/RygelAVTransport"
        ));
        assert!(!adresse_de_devialet(
            "http://192.168.1.18:49152/AVTransport/control"
        ));
    }

    #[test]
    fn seul_le_hi_res_d_un_devialet_relance_le_flux_6333() {
        assert!(relancer_le_deplacement(None, true, Some(24)));
        assert!(
            !relancer_le_deplacement(None, true, Some(16)),
            "le 16 bits garde le Seek, qui marche"
        );
        assert!(
            !relancer_le_deplacement(None, true, None),
            "profondeur inconnue"
        );
        assert!(
            !relancer_le_deplacement(None, false, Some(24)),
            "autre renderer : rien ne change"
        );
    }

    #[test]
    fn le_reglage_de_zone_arme_ou_desarme_la_regle_6333() {
        assert!(relancer_le_deplacement(Some("true"), false, Some(24)));
        assert!(!relancer_le_deplacement(Some("false"), true, Some(24)));
        assert!(!relancer_le_deplacement(Some("true"), false, Some(16)));
        assert!(
            relancer_le_deplacement(Some("n'importe quoi"), true, Some(24)),
            "réglage illisible : ce que dit le renderer"
        );
    }
}
