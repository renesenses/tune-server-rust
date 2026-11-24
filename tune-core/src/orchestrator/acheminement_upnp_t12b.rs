//! TESS T12b — les faits du lecteur d'une zone de la famille UPnP, tels que
//! la LECTURE les établit, pour l'état effectif de `GET /zones/{id}/tess`
//! (`tess::reseau_upnp`).
//!
//! **Rien n'est sondé ici.** La capacité LPCM est lue dans la MÉMOIRE de la
//! sonde de lecture (`dlna_lpcm_accepte`, remplie par
//! [`PlaybackOrchestrator::dlna_accepte_lpcm`] quand une lecture l'a
//! interrogée, par lecteur et par profondeur) ; sans réponse mémorisée, la
//! réponse par TYPE de sortie (`capacite_lpcm_par_sortie`), sinon `unknown`.
//! Aucun appel SOAP, aucun verrou de sortie : un `GET /zones/{id}/tess` vers
//! un lecteur endormi ou injoignable répond tout de suite. L'opt-in et le
//! ReplayGain sont lus par les mêmes clés et les mêmes prédicats que
//! `resolve_local` et `resolve_stream` ; la conversion imposée par le format
//! est CONSTATÉE sur la session servie (`voie_servie`).

use super::PlaybackOrchestrator;
use super::capacite_lpcm_par_sortie::{CapaciteLpcm, capacite_lpcm};
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::http::streamer::voie;
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
    /// La session servie au lecteur pour cette piste (`stream_id`).
    pub stream_id: Option<String>,
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
    /// La capacité LPCM du lecteur, à trois états, SANS sonde : la réponse
    /// mémorisée par la lecture (sonde concluante), sinon la réponse par type
    /// de la zone (BluOS et Chromecast en 16 bits, SlimProto refusé), sinon
    /// `unknown`, qui vaut refus pour changer le format servi.
    pub(super) async fn lpcm_du_lecteur(
        &self,
        device_id: &str,
        type_de_zone: &str,
        hi_res: bool,
    ) -> LpcmDuLecteur {
        if !device_id.is_empty() {
            let cle = format!("{device_id}|{}", if hi_res { "24" } else { "16" });
            if let Some(sonde) = self.dlna_lpcm_accepte.lock().await.get(&cle).copied() {
                return if sonde {
                    LpcmDuLecteur::Accepted
                } else {
                    LpcmDuLecteur::Refused
                };
            }
        }
        match capacite_lpcm(type_de_zone, hi_res) {
            CapaciteLpcm::Accepte => LpcmDuLecteur::Accepted,
            CapaciteLpcm::Refuse => LpcmDuLecteur::Refused,
            CapaciteLpcm::Inconnu => LpcmDuLecteur::Unknown,
        }
    }

    /// La session servie pour la piste en cours est-elle une CONVERSION
    /// (fichier ré-encodé, flux décodé) ? Pas le flux continu de #8082
    /// (`FLUX_CONTINU`) : il enchaîne les pistes en passthrough. Vrai aussi quand le format seul l'a
    /// imposée (format illisible par le lecteur, plafond de cadence, DSD
    /// converti) : la lecture y cuit alors l'étage casque
    /// (`crossfeed_du_fichier`, #2742 cas 1).
    async fn session_convertie(&self, stream_id: Option<&str>) -> bool {
        let Some(sid) = stream_id.filter(|s| !s.is_empty()) else {
            return false;
        };
        matches!(
            self.streamer.voie_servie(sid).await,
            voie::FICHIER_CONVERTI | voie::FLUX_CONVERTI
        )
    }

    /// Les faits du lecteur de la zone, ou `None` hors famille UPnP. Ne sonde
    /// rien et ne prend aucun verrou de sortie (voir l'en-tête).
    pub async fn faits_upnp_de_zone(
        &self,
        zone_id: i64,
        piste: &PisteEnCours,
    ) -> Option<FaitsUpnp> {
        let zone = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten()?;
        let type_de_zone = zone.output_type.clone().unwrap_or_default();
        if !famille_upnp(TransportZone::depuis_type_de_sortie(Some(&type_de_zone))) {
            return None;
        }
        let device = zone.output_device_id.clone().unwrap_or_default();
        // La profondeur que la lecture interrogerait ; 16 bits quand rien ne
        // joue, le plancher du relais progressif.
        let hi_res = piste.bit_depth.is_some_and(|b| b > 16);
        let lpcm = self.lpcm_du_lecteur(&device, &type_de_zone, hi_res).await;
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
        // `decodage_progressif_par_range` ne lit de l'adresse que son schéma
        // (`http://` ou `https://`) ; l'adresse signée du CDN n'est pas connue
        // ici, et tout service relayé par Tune en a une en HTTPS. On lui passe
        // donc une adresse FICTIVE de ce schéma : seuls le service et le codec
        // décident, comme dans `profondeur_du_wav_progressif_de_service`.
        let service_decodable_au_fil = origine != OrigineDuFlux::Service
            || super::resolve_stream::decodage_progressif_par_range(
                piste.source.as_deref().unwrap_or_default(),
                piste.format.as_deref().unwrap_or_default(),
                "https://",
            );
        let format_impose_le_transcodage = self.session_convertie(piste.stream_id.as_deref()).await;
        Some(FaitsUpnp {
            lpcm,
            lpcm_bits: if hi_res { 24 } else { 16 },
            progressif_arme,
            origine,
            replaygain_actif,
            service_decodable_au_fil,
            format_impose_le_transcodage,
        })
    }
}
