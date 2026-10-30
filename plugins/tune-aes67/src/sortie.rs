//! La sortie Tune d'un flux AES67 (`OutputTarget`).
//!
//! Étape E1 : lecture, pause, reprise, arrêt, volume logiciel facultatif.
//! Le saut dans la piste et l'enchaînement sans blanc viennent à l'étape E3 ;
//! la sortie les déclare absents dans ses capacités plutôt que de mal les
//! faire.
//!
//! Le flux est annoncé en SAP dès que la sortie existe, lecture ou non : un
//! utilisateur doit pouvoir abonner son récepteur avant d'appuyer sur
//! lecture. Le retrait SAP part quand la sortie est lâchée.
//!
//! Étape E2 : le SDP suit l'horloge. Quand la référence change (calage sur
//! un maître PTP, repli sur l'horloge locale, autre maître), la version de
//! `o=` avance et l'annonceur SAP réannonce aussitôt.

use std::net::SocketAddrV4;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use async_trait::async_trait;
use tracing::info;
use tune_core::outputs::traits::{
    OutputCapabilities, OutputStatus, OutputTarget, PlayMedia, TransportState,
};

use crate::cadenceur::{self, Commandes, ParametresCadence, Statistiques};
use crate::config::{DUREE_PAQUET_US, FluxAes67, TYPE_DE_SORTIE};
use crate::droit::DroitPremium;
use crate::horloge::{ReferenceHorloge, SourceHorloge};
use crate::producteur::{self, Gain, SourceAudio};
use crate::sap::{AnnonceurSap, DESTINATION_SAP, INTERVALLE_SAP, SdpCourant};
use crate::sdp::DescriptionSession;
use crate::{paquetisation, reseau};

/// Paquets en attente entre le producteur et le cadenceur : 200 ms à 1 ms.
const PAQUETS_EN_AVANCE: usize = 200;
/// Paquets prêts avant le premier envoi (et après une reprise) : 50 ms.
const PRETAMPON: usize = 50;

/// Ce que le banc d'essai règle autrement qu'en production.
#[derive(Debug, Clone)]
pub struct ReglagesEmission {
    pub destination_sap: SocketAddrV4,
    pub intervalle_sap: Duration,
    pub temps_reel: bool,
    pub avance: Duration,
    /// `a=mediaclk:direct=<décalage>` (RFC 7273) ; 0 par défaut, comme la
    /// plupart des émetteurs AES67.
    pub decalage_media: u32,
}

impl Default for ReglagesEmission {
    fn default() -> Self {
        Self {
            destination_sap: DESTINATION_SAP,
            intervalle_sap: INTERVALLE_SAP,
            temps_reel: true,
            avance: Duration::ZERO,
            decalage_media: 0,
        }
    }
}

/// Le SDP d'un flux, réécrit quand la référence d'horloge change.
pub struct SdpDuFlux {
    horloge: Arc<dyn SourceHorloge>,
    courant: Mutex<(DescriptionSession, String)>,
}

impl SdpDuFlux {
    fn new(mut description: DescriptionSession, horloge: Arc<dyn SourceHorloge>) -> Self {
        description.reference = horloge.reference();
        let texte = description.ecrire();
        Self {
            horloge,
            courant: Mutex::new((description, texte)),
        }
    }

    /// Le SDP d'aujourd'hui ; `o=` change de version si la référence a
    /// changé depuis la dernière lecture.
    pub fn courant(&self) -> String {
        let reference: ReferenceHorloge = self.horloge.reference();
        let mut garde = self.courant.lock().unwrap_or_else(|p| p.into_inner());
        if garde.0.reference != reference {
            garde.0.reference = reference;
            garde.0.version += 1;
            garde.1 = garde.0.ecrire();
            info!(
                version = garde.0.version,
                ts_refclk = %reference.attribut_sdp(),
                "aes67_sdp_reference_changee"
            );
        }
        garde.1.clone()
    }
}

struct Session {
    commandes: Arc<Commandes>,
    stats: Arc<Statistiques>,
    arret_producteur: Arc<AtomicBool>,
    echec: Arc<Mutex<Option<String>>>,
    fils: Vec<JoinHandle<()>>,
    uri: String,
    titre: Option<String>,
    artiste: Option<String>,
    duree_ms: u64,
}

impl Session {
    fn arreter(mut self) {
        self.arret_producteur.store(true, Ordering::Release);
        self.commandes.arret.store(true, Ordering::Release);
        for f in self.fils.drain(..) {
            let _ = f.join();
        }
    }
}

pub struct SortieAes67 {
    flux: FluxAes67,
    device_id: String,
    horloge: Arc<dyn SourceHorloge>,
    droit: Arc<dyn DroitPremium>,
    reglages: ReglagesEmission,
    destination: SocketAddrV4,
    sdp: Arc<SdpDuFlux>,
    trames_par_paquet: usize,
    ssrc: u32,
    annonceur: Mutex<Option<AnnonceurSap>>,
    session: Mutex<Option<Session>>,
    volume: AtomicU64,
    muet: AtomicBool,
    gain: Arc<Gain>,
    echec_a_signaler: Mutex<Option<String>>,
}

/// Un identifiant de session SDP et un SSRC tirés de l'horloge et du flux :
/// stables pour la vie de la sortie, différents d'une sortie à l'autre.
fn identifiants(flux: &FluxAes67, maintenant_ns: u64) -> (u64, u32) {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for o in flux.id.bytes().chain(maintenant_ns.to_le_bytes()) {
        h ^= u64::from(o);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    (h >> 1, (h >> 32) as u32 ^ h as u32)
}

impl SortieAes67 {
    /// Construit la sortie et démarre l'annonce SAP. Le flux n'est PAS
    /// revalidé ici : la saisie l'a été ([`FluxAes67::valider`]), et le banc
    /// d'essai émet en unicast sur la boucle locale.
    pub fn new(
        flux: FluxAes67,
        horloge: Arc<dyn SourceHorloge>,
        droit: Arc<dyn DroitPremium>,
        reglages: ReglagesEmission,
    ) -> Result<Self, String> {
        let trames_par_paquet = paquetisation::trames_par_paquet(flux.cadence, DUREE_PAQUET_US)
            .ok_or_else(|| {
                format!(
                    "cadence {} sans nombre entier de trames par ms",
                    flux.cadence
                )
            })?;
        let destination = SocketAddrV4::new(flux.adresse, flux.port);
        let origine = reseau::adresse_d_origine(destination, flux.interface);
        let (id_session, ssrc) = identifiants(&flux, horloge.maintenant_ns());
        let description = DescriptionSession {
            id_session,
            version: 1,
            origine,
            nom: flux.nom_de_session(),
            destination: flux.adresse,
            port: flux.port,
            ttl: flux.ttl,
            type_charge: flux.type_charge,
            cadence: flux.cadence,
            canaux: flux.canaux,
            duree_paquet_us: DUREE_PAQUET_US,
            reference: horloge.reference(),
            decalage_media: reglages.decalage_media,
        };
        let sdp = Arc::new(SdpDuFlux::new(description, horloge.clone()));
        let socket_sap = reseau::ouvrir_socket(
            reglages.destination_sap,
            flux.interface,
            flux.ttl,
            flux.dscp,
            false,
        )
        .map_err(|e| format!("socket SAP : {e}"))?;
        let lecteur = sdp.clone();
        let sdp_courant: SdpCourant = Arc::new(move || lecteur.courant());
        let annonceur = AnnonceurSap::demarrer_dynamique(
            socket_sap,
            reglages.destination_sap,
            origine,
            sdp_courant,
            reglages.intervalle_sap,
        );
        info!(
            device_id = %flux.device_id(),
            %destination,
            %origine,
            "aes67_flux_annonce"
        );
        Ok(Self {
            device_id: flux.device_id(),
            flux,
            horloge,
            droit,
            reglages,
            destination,
            sdp,
            trames_par_paquet,
            ssrc,
            annonceur: Mutex::new(Some(annonceur)),
            session: Mutex::new(None),
            volume: AtomicU64::new(1.0f64.to_bits()),
            muet: AtomicBool::new(false),
            gain: Arc::new(Gain::new(1.0)),
            echec_a_signaler: Mutex::new(None),
        })
    }

    /// Le SDP annoncé (celui de l'horloge d'aujourd'hui).
    pub fn sdp(&self) -> String {
        self.sdp.courant()
    }

    pub fn flux(&self) -> &FluxAes67 {
        &self.flux
    }

    fn volume(&self) -> f64 {
        f64::from_bits(self.volume.load(Ordering::Relaxed))
    }

    /// Le gain effectif : unité quand le volume logiciel est éteint.
    fn recalculer_le_gain(&self) {
        let g = if !self.flux.volume_logiciel {
            1.0
        } else if self.muet.load(Ordering::Relaxed) {
            0.0
        } else {
            tune_core::audio::volume_scale::gain_perceptif(self.volume())
        };
        self.gain.poser(g as f32);
    }

    fn prendre_la_session(&self) -> Option<Session> {
        self.session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    async fn arreter_la_session(&self) {
        if let Some(s) = self.prendre_la_session() {
            let _ = tokio::task::spawn_blocking(move || s.arreter()).await;
        }
    }

    /// Arrête les annonces SAP (retrait envoyé) et la lecture.
    pub fn fermer(&self) {
        if let Some(s) = self.prendre_la_session() {
            s.arreter();
        }
        if let Some(mut a) = self
            .annonceur
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            a.arreter();
        }
    }

    fn statut_de_la_session(&self) -> OutputStatus {
        let garde = self.session.lock().unwrap_or_else(|p| p.into_inner());
        let base = OutputStatus {
            volume: self.volume(),
            muted: self.muet.load(Ordering::Relaxed),
            ..OutputStatus::default()
        };
        let Some(s) = garde.as_ref() else {
            return base;
        };
        let trames = s.stats.trames.load(Ordering::Relaxed);
        let position_ms = trames * 1000 / u64::from(self.flux.cadence);
        let fini = s.commandes.fini.load(Ordering::Acquire);
        let echec = s.echec.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let state = if fini {
            TransportState::Stopped
        } else if s.commandes.pause.load(Ordering::Acquire) {
            TransportState::Paused
        } else {
            TransportState::Playing
        };
        if fini && let Some(e) = echec.as_ref() {
            *self
                .echec_a_signaler
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(e.clone());
        }
        OutputStatus {
            state,
            position_ms,
            duration_ms: s.duree_ms,
            current_uri: Some(s.uri.clone()),
            track_title: s.titre.clone(),
            track_artist: s.artiste.clone(),
            ended_naturally: fini && echec.is_none(),
            ..base
        }
    }
}

impl Drop for SortieAes67 {
    fn drop(&mut self) {
        self.fermer();
    }
}

#[async_trait]
impl OutputTarget for SortieAes67 {
    fn name(&self) -> &str {
        &self.flux.nom
    }
    fn device_id(&self) -> &str {
        &self.device_id
    }
    fn output_type(&self) -> &str {
        TYPE_DE_SORTIE
    }

    fn capabilities(&self) -> OutputCapabilities {
        let v = self.flux.volume_logiciel;
        OutputCapabilities::v1(true, true, false, v, v, false)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        if !self.droit.autorise().await {
            return Err("premium_required: la Sortie AES67 demande Premium".into());
        }
        self.arreter_la_session().await;
        let source = SourceAudio::choisir(media.file_path, media.url);
        let socket = reseau::ouvrir_socket(
            self.destination,
            self.flux.interface,
            self.flux.ttl,
            self.flux.dscp,
            true,
        )
        .map_err(|e| format!("socket RTP vers {} : {e}", self.destination))?;
        self.recalculer_le_gain();
        let (tx, rx) = sync_channel(PAQUETS_EN_AVANCE);
        let commandes = Arc::new(Commandes::default());
        let stats = Arc::new(Statistiques::default());
        let arret_producteur = Arc::new(AtomicBool::new(false));
        let echec = Arc::new(Mutex::new(None));
        let producteur = producteur::lancer(
            tokio::runtime::Handle::current(),
            source.clone(),
            self.flux.cadence,
            self.flux.canaux,
            self.trames_par_paquet,
            self.gain.clone(),
            tx,
            arret_producteur.clone(),
            echec.clone(),
        )
        .map_err(|e| format!("fil PCM : {e}"))?;
        let cadenceur = match cadenceur::lancer(
            socket,
            rx,
            self.horloge.clone(),
            ParametresCadence {
                cadence: self.flux.cadence,
                trames_par_paquet: self.trames_par_paquet as u32,
                type_charge: self.flux.type_charge,
                ssrc: self.ssrc,
                avance: self.reglages.avance,
                decalage_media: self.reglages.decalage_media,
                pretampon_paquets: PRETAMPON,
                temps_reel: self.reglages.temps_reel,
            },
            commandes.clone(),
            stats.clone(),
        ) {
            Ok(c) => c,
            Err(e) => {
                arret_producteur.store(true, Ordering::Release);
                let _ = producteur.join();
                return Err(format!("fil RTP : {e}"));
            }
        };
        info!(device_id = %self.device_id, ?source, "aes67_lecture");
        *self.session.lock().unwrap_or_else(|p| p.into_inner()) = Some(Session {
            commandes,
            stats,
            arret_producteur,
            echec,
            fils: vec![producteur, cadenceur],
            uri: media.url.to_string(),
            titre: media.title.map(str::to_string),
            artiste: media.artist.map(str::to_string),
            duree_ms: media.duration_ms.unwrap_or(0),
        });
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        if let Some(s) = self
            .session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            s.commandes.pause.store(true, Ordering::Release);
        }
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        if let Some(s) = self
            .session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            s.commandes.pause.store(false, Ordering::Release);
        }
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        self.arreter_la_session().await;
        Ok(())
    }

    async fn seek(&self, _position_ms: u64) -> Result<(), String> {
        Err("seek non pris en charge par la Sortie AES67 (étape E1)".into())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        self.volume
            .store(volume.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
        self.recalculer_le_gain();
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        self.muet.store(muted, Ordering::Relaxed);
        self.recalculer_le_gain();
        Ok(())
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        Ok(self.statut_de_la_session())
    }

    async fn is_available(&self) -> bool {
        true
    }

    fn take_output_failure(&self) -> Option<String> {
        self.echec_a_signaler
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
    }

    fn host(&self) -> Option<&str> {
        None
    }

    fn diagnostics_json(&self) -> Option<serde_json::Value> {
        let garde = self.session.lock().unwrap_or_else(|p| p.into_inner());
        let stats = garde.as_ref().map(|s| s.stats.releve());
        Some(serde_json::json!({
            "destination": self.destination.to_string(),
            "reference_horloge": self.horloge.reference().attribut_sdp(),
            "horloge_calee": self.horloge.calee(),
            "generation_horloge": self.horloge.generation(),
            "volume_logiciel": self.flux.volume_logiciel,
            "sdp": self.sdp.courant(),
            "emission": stats,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deux_sorties_n_ont_pas_le_meme_ssrc() {
        let mut a: FluxAes67 = serde_json::from_value(serde_json::json!({
            "id": "salon", "nom": "Salon", "adresse": "239.69.1.10"
        }))
        .unwrap();
        let (sa, ra) = identifiants(&a, 1);
        a.id = "bureau".into();
        let (sb, rb) = identifiants(&a, 1);
        assert_ne!(sa, sb);
        assert_ne!(ra, rb);
    }
}
