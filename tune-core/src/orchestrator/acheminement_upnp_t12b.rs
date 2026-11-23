//! TESS T12b — les faits du lecteur d'une zone de la famille UPnP, tels que
//! la LECTURE les établit, pour l'état effectif de `GET /zones/{id}/tess`
//! (`tess::reseau_upnp`).
//!
//! Rien n'est sondé deux fois : la capacité LPCM vient de la même fonction
//! que la décision de lecture ([`PlaybackOrchestrator::dlna_accepte_lpcm`],
//! mémorisée par lecteur et par profondeur), à la profondeur que la lecture
//! interrogerait (`bit_depth > 16`). L'opt-in et le ReplayGain sont lus par
//! les mêmes clés et les mêmes prédicats que `resolve_local` et
//! `resolve_stream`.

use super::PlaybackOrchestrator;
use super::capacite_lpcm_par_sortie::{CapaciteLpcm, capacite_lpcm};
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::tess::TransportZone;
use crate::tess::reseau_upnp::{FaitsUpnp, LpcmDuLecteur, OrigineDuFlux, famille_upnp};

/// Ce qui joue sur la zone, du point de vue des faits du lecteur.
#[derive(Debug, Clone, Default)]
pub struct PisteEnCours {
    pub track_id: Option<i64>,
    /// `local` pour la bibliothèque, le nom du service sinon ; `None` : rien
    /// ne joue.
    pub source: Option<String>,
    pub source_id: Option<String>,
    /// Le codec annoncé (`flac`, `m4a`…).
    pub format: Option<String>,
    pub bit_depth: Option<u32>,
}

impl PisteEnCours {
    fn origine(&self) -> OrigineDuFlux {
        match self.source.as_deref() {
            None | Some("") => OrigineDuFlux::Unknown,
            Some(s) if s.eq_ignore_ascii_case("local") || s.eq_ignore_ascii_case("upnp") => {
                OrigineDuFlux::Library
            }
            Some(_) => OrigineDuFlux::Service,
        }
    }
}

impl PlaybackOrchestrator {
    /// La capacité LPCM du lecteur `device_id`, à trois états, par la MÊME
    /// sonde que la lecture. Une réponse mémorisée (sonde concluante) dit
    /// accepte ou refuse ; sinon la réponse par type
    /// (`capacite_lpcm_par_sortie`) ; une sortie absente du registre reste
    /// « accepte », la convention de `dlna_accepte_lpcm`.
    pub(super) async fn lpcm_du_lecteur(&self, device_id: &str, hi_res: bool) -> LpcmDuLecteur {
        if device_id.is_empty() {
            return LpcmDuLecteur::Unknown;
        }
        let accepte = self.dlna_accepte_lpcm(device_id, hi_res).await;
        let cle = format!("{device_id}|{}", if hi_res { "24" } else { "16" });
        if let Some(sonde) = self.dlna_lpcm_accepte.lock().await.get(&cle).copied() {
            return if sonde {
                LpcmDuLecteur::Accepted
            } else {
                LpcmDuLecteur::Refused
            };
        }
        if accepte {
            return LpcmDuLecteur::Accepted;
        }
        let type_de_sortie = {
            let arc = self.outputs.lock().await.get(device_id);
            match arc {
                Some(o) => o.lock().await.output_type().to_string(),
                None => String::new(),
            }
        };
        match capacite_lpcm(&type_de_sortie, hi_res) {
            CapaciteLpcm::Refuse => LpcmDuLecteur::Refused,
            _ => LpcmDuLecteur::Unknown,
        }
    }

    /// Les faits du lecteur de la zone, ou `None` hors famille UPnP.
    pub async fn faits_upnp_de_zone(
        &self,
        zone_id: i64,
        piste: &PisteEnCours,
    ) -> Option<FaitsUpnp> {
        let zone = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()?;
        if !famille_upnp(TransportZone::depuis_type_de_sortie(
            zone.output_type.as_deref(),
        )) {
            return None;
        }
        let device = zone.output_device_id.clone().unwrap_or_default();
        // La profondeur que la lecture interrogerait ; 16 bits quand rien ne
        // joue, le plancher du relais progressif.
        let hi_res = piste.bit_depth.is_some_and(|b| b > 16);
        let lpcm = self.lpcm_du_lecteur(&device, hi_res).await;
        let progressif_arme = SettingsRepo::with_backend(self.db.clone())
            .get("dsp_progressif_reseau")
            .ok()
            .flatten()
            .as_deref()
            == Some("true");
        let origine = piste.origine();
        let rg = crate::audio::replaygain::PisteRg::new(
            piste.track_id,
            piste.source.as_deref(),
            piste.source_id.as_deref(),
        );
        let replaygain_actif = self.zone_replaygain_changes_audio(zone_id, rg);
        let service_decodable_au_fil = origine != OrigineDuFlux::Service
            || super::resolve_stream::decodage_progressif_par_range(
                piste.source.as_deref().unwrap_or_default(),
                piste.format.as_deref().unwrap_or_default(),
                "https://",
            );
        Some(FaitsUpnp {
            lpcm,
            lpcm_bits: if hi_res { 24 } else { 16 },
            progressif_arme,
            origine,
            replaygain_actif,
            service_decodable_au_fil,
        })
    }
}
