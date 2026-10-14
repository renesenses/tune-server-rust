//! #2991 — le titre radio par DIDL, pour les lecteurs qui ignorent l'ICY.
//!
//! Pendant une radio, Tune fait suivre le morceau au renderer par les blocs
//! ICY du flux (`radio_icy_block_sent`). Le Hifi Rose RS250A de Serge Asselin
//! (fil 1529) demande l'ICY, reçoit des blocs titrés avec la pochette… et son
//! écran ne suit pas. Il ne lit, d'après ce que l'on voit, que le DIDL-Lite
//! reçu à la pose de l'URI.
//!
//! Ce module ajoute un SECOND canal, opt-in par zone : à chaque changement de
//! morceau, réémettre un DIDL portant le titre, l'artiste et la pochette, sur
//! la MÊME URI de flux. L'ICY n'est pas touché : il continue de partir.
//!
//! ## Deux voies, et aucune n'est prouvée sur le RS250A
//!
//! - [`ModeTitreRadioDidl::Suivante`] (`"next"`) : `SetNextAVTransportURI`
//!   avec la même URI et le nouveau DIDL. Le flux en cours n'est pas
//!   interrompu : l'action ne touche que la piste « suivante ». Certains
//!   renderers relisent ce DIDL pour leur écran, d'autres non.
//! - [`ModeTitreRadioDidl::Recharger`] (`"reload"`) : `SetAVTransportURI` sur
//!   la même URI avec le nouveau DIDL, puis `Play` si l'appareil s'est arrêté.
//!   C'est le seul canal que le standard réserve aux métadonnées de la piste
//!   COURANTE, mais la plupart des renderers rechargent alors le flux : une
//!   coupure courte à chaque morceau est attendue. Le serveur de flux accepte
//!   la reconnexion (`radio_stream_reconnect`).
//!
//! `LastChange` n'est pas une voie : c'est un évènement que le RENDERER émet
//! vers ses abonnés, un point de contrôle ne peut pas le lui envoyer.
//!
//! Le réglage `zone_{id}_dlna_radio_title` est ABSENT par défaut : sans lui,
//! aucune action SOAP de plus ne part, et les renderers qui suivent déjà
//! l'ICY ne voient aucune différence.

use std::sync::Arc;

use tracing::{debug, info, warn};

use super::{DlnaOutput, PlayMedia, faute_commande_soap};
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

/// La voie de réémission choisie pour une zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeTitreRadioDidl {
    /// Défaut : rien ne part, seul l'ICY porte le changement de morceau.
    Desactive,
    /// `SetNextAVTransportURI`, même URI, nouveau DIDL : pas de coupure.
    Suivante,
    /// `SetAVTransportURI`, même URI, nouveau DIDL : coupure probable.
    Recharger,
}

impl ModeTitreRadioDidl {
    /// La valeur stable, celle du réglage et de l'API.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desactive => "off",
            Self::Suivante => "next",
            Self::Recharger => "reload",
        }
    }

    /// Relit une valeur de réglage. Tout ce qui n'est pas une voie connue
    /// vaut « désactivé » : une valeur illisible n'envoie rien à personne.
    pub fn depuis_reglage(valeur: &str) -> Self {
        match valeur.trim() {
            "next" => Self::Suivante,
            "reload" => Self::Recharger,
            _ => Self::Desactive,
        }
    }

    /// Valeur reçue par l'API : `None` si elle n'est pas une des trois.
    pub fn depuis_api(valeur: &str) -> Option<Self> {
        match valeur.trim() {
            "off" | "" => Some(Self::Desactive),
            "next" => Some(Self::Suivante),
            "reload" => Some(Self::Recharger),
            _ => None,
        }
    }
}

/// La clé de réglage de la zone. Absente = désactivé ; elle est supprimée
/// au retour à « off », pour que l'absence et le défaut soient un seul état.
pub fn cle_de_zone(zone_id: i64) -> String {
    format!("zone_{zone_id}_dlna_radio_title")
}

/// La voie réglée sur la zone. **Désactivée par défaut.**
pub fn mode_de_zone(db: &Arc<dyn DbBackend>, zone_id: i64) -> ModeTitreRadioDidl {
    SettingsRepo::with_backend(db.clone())
        .get(&cle_de_zone(zone_id))
        .ok()
        .flatten()
        .map(|v| ModeTitreRadioDidl::depuis_reglage(&v))
        .unwrap_or(ModeTitreRadioDidl::Desactive)
}

/// Ce que la dernière pose d'URI acceptée a réellement envoyé : sans cela, la
/// réémission ne saurait ni quelle URI rejouer, ni sous quel MIME ni à quel
/// niveau de DIDL l'appareil l'a acceptée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoseCourante {
    pub url: String,
    /// Le MIME ANNONCÉ qui est passé (peut différer du MIME source après un
    /// repli 714).
    pub mime_annonce: String,
    /// 0 = DIDL complet, 1 = minimal, 2 = vide.
    pub niveau_didl: u8,
    pub live_stream: bool,
    pub byte_seekable: bool,
    pub album: Option<String>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub channels: Option<u32>,
}

/// Le morceau en cours, tel que le sondeur radio vient de le lire.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TitreRadio {
    pub titre: String,
    pub artiste: Option<String>,
    /// Nom de la station, écrit en album comme à la pose initiale.
    pub album: Option<String>,
    /// URL ABSOLUE de la pochette (déjà résolue pour le réseau).
    pub pochette: Option<String>,
}

/// L'issue d'une réémission, pour le journal et les épreuves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueRelai {
    /// Le DIDL est parti et l'appareil l'a acquitté.
    Envoye { action: &'static str },
    /// Rien n'est parti, et voici pourquoi.
    Ignore(&'static str),
}

impl DlnaOutput {
    /// Retient la pose d'URI que l'appareil vient d'accepter.
    pub(super) fn noter_pose_courante(
        &self,
        media: &PlayMedia<'_>,
        mime_annonce: &str,
        niveau_didl: u8,
    ) {
        let pose = PoseCourante {
            url: media.url.to_string(),
            mime_annonce: mime_annonce.to_string(),
            niveau_didl,
            live_stream: media.live_stream,
            byte_seekable: media.byte_seekable,
            album: media.album.map(str::to_string),
            sample_rate: media.sample_rate,
            bit_depth: media.bit_depth,
            channels: media.channels,
        };
        if let Ok(mut g) = self.pose_courante.lock() {
            *g = Some(pose);
        }
    }

    /// La dernière pose acceptée, si elle existe.
    pub fn pose_courante(&self) -> Option<PoseCourante> {
        self.pose_courante.lock().ok()?.clone()
    }

    /// Réémet le DIDL du morceau en cours sur l'URI de flux `url_attendue`.
    ///
    /// Ne part QUE si la dernière pose acceptée porte exactement cette URI et
    /// que c'est un flux en direct : une piste posée entre-temps (file,
    /// gapless) ne doit jamais être remplacée par une radio périmée.
    pub async fn reemettre_titre_radio(
        &self,
        mode: ModeTitreRadioDidl,
        url_attendue: &str,
        titre: &TitreRadio,
    ) -> Result<IssueRelai, String> {
        if mode == ModeTitreRadioDidl::Desactive {
            return Ok(IssueRelai::Ignore("reglage_desactive"));
        }
        let Some(pose) = self.pose_courante() else {
            return Ok(IssueRelai::Ignore("aucune_pose_connue"));
        };
        if pose.url != url_attendue {
            return Ok(IssueRelai::Ignore("uri_posee_differente"));
        }
        if !pose.live_stream {
            return Ok(IssueRelai::Ignore("pas_un_flux_en_direct"));
        }
        // Un appareil qui n'a accepté qu'un DIDL vide n'en lira pas un plein
        // en cours de route : on ne lui fait pas payer un refus de plus.
        if pose.niveau_didl >= 2 {
            return Ok(IssueRelai::Ignore("appareil_au_didl_vide"));
        }
        let item_id = self.next_item_id();
        let media = PlayMedia {
            url: &pose.url,
            mime_type: &pose.mime_annonce,
            title: Some(titre.titre.as_str()),
            artist: titre.artiste.as_deref(),
            album: titre.album.as_deref().or(pose.album.as_deref()),
            cover_url: titre.pochette.as_deref(),
            live_stream: pose.live_stream,
            byte_seekable: pose.byte_seekable,
            sample_rate: pose.sample_rate,
            bit_depth: pose.bit_depth,
            channels: pose.channels,
            source: Some("radio"),
            ..Default::default()
        };
        let didl = if pose.niveau_didl == 0 {
            Self::didl_metadata_mime(&media, &item_id, &pose.mime_annonce)
        } else {
            Self::didl_metadata_minimale(&media, &item_id, &pose.mime_annonce)
        };
        let (action, corps) = match mode {
            ModeTitreRadioDidl::Suivante => (
                "SetNextAVTransportURI",
                format!(
                    "<InstanceID>0</InstanceID><NextURI>{}</NextURI><NextURIMetaData>{didl}</NextURIMetaData>",
                    pose.url
                ),
            ),
            ModeTitreRadioDidl::Recharger => (
                "SetAVTransportURI",
                format!(
                    "<InstanceID>0</InstanceID><CurrentURI>{}</CurrentURI><CurrentURIMetaData>{didl}</CurrentURIMetaData>",
                    pose.url
                ),
            ),
            ModeTitreRadioDidl::Desactive => unreachable!("traité plus haut"),
        };
        let reponse = self.av_action(action, &corps).await?;
        if faute_commande_soap(&reponse) {
            warn!(
                device = %self.name,
                action,
                response = %reponse,
                "radio_didl_relai_refuse"
            );
            return Err(format!("{action} rejected: {reponse}"));
        }
        if mode == ModeTitreRadioDidl::Recharger {
            // #6008 — un rechargement EST une pose d'URI : un renderer qui
            // remonte son volume à chaque pose le ferait à chaque titre.
            self.noter_pose_d_uri();
            self.relancer_si_arrete_apres_rechargement().await;
            self.reimposer_le_volume_apres_uri().await;
        }
        Ok(IssueRelai::Envoye { action })
    }

    /// Après un `SetAVTransportURI` de rechargement, l'appareil a pu s'arrêter.
    /// On ne renvoie `Play` que s'il ne joue plus : un `Play` sur un appareil
    /// déjà en lecture n'apporte rien et peut redémarrer le flux.
    async fn relancer_si_arrete_apres_rechargement(&self) {
        self.attendre_apres_set_uri().await;
        let etat = self
            .av_action("GetTransportInfo", "<InstanceID>0</InstanceID>")
            .await;
        let joue = etat
            .as_deref()
            .is_ok_and(|r| r.contains("PLAYING") || r.contains("TRANSITIONING"));
        if joue {
            debug!(device = %self.name, "radio_didl_relai_reload_toujours_en_lecture");
            return;
        }
        match self
            .av_action("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>")
            .await
        {
            Ok(r) if !faute_commande_soap(&r) => {
                info!(device = %self.name, "radio_didl_relai_reload_play_renvoye");
            }
            Ok(r) => {
                warn!(device = %self.name, response = %r, "radio_didl_relai_reload_play_refuse")
            }
            Err(e) => warn!(device = %self.name, error = %e, "radio_didl_relai_reload_play_echec"),
        }
    }
}

#[cfg(test)]
#[path = "dlna_titre_radio_2991_tests.rs"]
pub(crate) mod tests;
