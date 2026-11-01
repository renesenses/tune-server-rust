//! La sortie Tune d'un flux AES67 (`OutputTarget`).
//!
//! Le flux est annoncé en SAP dès que la sortie existe, lecture ou non : un
//! utilisateur doit pouvoir abonner son récepteur avant d'appuyer sur
//! lecture. Le retrait SAP part quand la sortie est lâchée.
//!
//! Étape E2 : le SDP suit l'horloge. Quand la référence change (calage sur
//! un maître PTP, repli sur l'horloge locale, autre maître), la version de
//! `o=` avance et l'annonceur SAP réannonce aussitôt.
//!
//! Étape E3 : saut, enchaînement sans blanc, pause et reprise.
//! - Un seul émetteur RTP (le fil du [`cadenceur`]) pour toute la vie de la
//!   sortie, démarré à la première lecture : la séquence RTP ne recule
//!   jamais, ni d'une piste à l'autre, ni après un saut ou un arrêt.
//! - Une lecture ou un saut pose un nouveau FLUX dans l'émetteur : il jette
//!   ce qu'il tenait, repart d'une origine marquée (bit M), jamais avant la
//!   fin du dernier paquet émis. L'horodatage reste celui de l'horloge
//!   média (`mediaclk:direct`) : il avance avec le temps, rien à réannoncer.
//! - `set_next_media` lance tout de suite le décodeur de la piste suivante ;
//!   le producteur l'enchaîne dans le même paquetiseur quand la piste en
//!   cours est décodée : horodatages et séquence continus, aucun paquet de
//!   silence, rééchantillonnage vers la cadence du flux propre à chaque
//!   piste. La sortie déclare donc `can_gapless` et `can_seek`.
//! - La pause tait le flux (le récepteur se tait), la reprise repart
//!   marquée là où le PCM s'était arrêté.

use std::net::SocketAddrV4;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use async_trait::async_trait;
use tracing::info;
use tune_core::outputs::traits::{
    OutputCapabilities, OutputStatus, OutputTarget, PlayMedia, TransportState,
};

use crate::cadenceur::{self, Commandes, Message, ParametresCadence, Statistiques};
use crate::config::{DUREE_PAQUET_US, FluxAes67, TYPE_DE_SORTIE};
use crate::droit::DroitPremium;
use crate::horloge::{ReferenceHorloge, SourceHorloge};
use crate::producteur::{
    self, Frontiere, Frontieres, Gain, InfosPiste, ParametresProducteur, PisteLancee,
    PisteSuivante, SourceAudio,
};
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

    /// La version de `o=` du SDP d'aujourd'hui.
    pub fn version(&self) -> u64 {
        let _ = self.courant();
        self.courant
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .0
            .version
    }
}

/// L'émetteur RTP de la sortie : un fil pour toute sa vie.
struct Emetteur {
    commandes: Arc<Commandes>,
    stats: Arc<Statistiques>,
    fil: Option<JoinHandle<()>>,
}

impl Emetteur {
    fn arreter(mut self) {
        self.commandes.arret.store(true, Ordering::Release);
        if let Some(f) = self.fil.take() {
            let _ = f.join();
        }
    }
}

/// Un flux en cours : une suite de pistes portée par un producteur.
struct Session {
    id: u64,
    arret_producteur: Arc<AtomicBool>,
    echec: Arc<Mutex<Option<String>>>,
    producteur: Option<JoinHandle<()>>,
    frontieres: Frontieres,
}

impl Session {
    fn arreter(mut self) {
        self.arret_producteur.store(true, Ordering::Release);
        if let Some(f) = self.producteur.take() {
            let _ = f.join();
        }
    }
}

/// Ce que l'écran de l'état (`GET /api/v1/ext/aes67`) lit d'une sortie
/// vivante, sans la tenir.
pub struct Vitrine {
    pub id: String,
    pub device_id: String,
    sdp: Arc<SdpDuFlux>,
    annoncee: AtomicBool,
    emetteur: Mutex<Option<(Arc<Commandes>, Arc<Statistiques>)>>,
    session: AtomicU64,
}

impl Vitrine {
    /// `lecture`, `pause` ou `arret`.
    pub fn transport(&self) -> &'static str {
        let session = self.session.load(Ordering::Acquire);
        let garde = self.emetteur.lock().unwrap_or_else(|p| p.into_inner());
        match garde.as_ref() {
            Some((c, s)) if session != 0 => {
                let fini = s.flux_courant.load(Ordering::Acquire) == session
                    && c.fini.load(Ordering::Acquire);
                if fini {
                    "arret"
                } else if c.pause.load(Ordering::Acquire) {
                    "pause"
                } else {
                    "lecture"
                }
            }
            _ => "arret",
        }
    }

    pub fn releve(&self) -> serde_json::Value {
        let emission = self
            .emetteur
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|(_, s)| s.releve());
        serde_json::json!({
            "id": self.id,
            "device_id": self.device_id,
            "annoncee": self.annoncee.load(Ordering::Acquire),
            "transport": self.transport(),
            "sdp": self.sdp.courant(),
            "version_sdp": self.sdp.version(),
            "emission": emission,
        })
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
    emetteur: Mutex<Option<Emetteur>>,
    session: Mutex<Option<Session>>,
    suivante: PisteSuivante,
    prochain_flux: AtomicU64,
    volume: AtomicU64,
    muet: AtomicBool,
    gain: Arc<Gain>,
    echec_a_signaler: Mutex<Option<String>>,
    vitrine: Arc<Vitrine>,
}

/// L'état d'un flux vu de la sortie : la piste qui joue, sa position, fini.
struct Lecture {
    piste: Frontiere,
    position_ms: u64,
    fini: bool,
    pause: bool,
    echec: Option<String>,
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
        let vitrine = Arc::new(Vitrine {
            id: flux.id.clone(),
            device_id: flux.device_id(),
            sdp: sdp.clone(),
            annoncee: AtomicBool::new(true),
            emetteur: Mutex::new(None),
            session: AtomicU64::new(0),
        });
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
            emetteur: Mutex::new(None),
            session: Mutex::new(None),
            suivante: Arc::new(Mutex::new(None)),
            prochain_flux: AtomicU64::new(1),
            volume: AtomicU64::new(1.0f64.to_bits()),
            muet: AtomicBool::new(false),
            gain: Arc::new(Gain::new(1.0)),
            echec_a_signaler: Mutex::new(None),
            vitrine,
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

    /// Ce que l'écran de l'état lit de cette sortie.
    pub fn vitrine(&self) -> Arc<Vitrine> {
        self.vitrine.clone()
    }

    fn prendre_la_session(&self) -> Option<Session> {
        let s = self
            .session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        self.vitrine.session.store(0, Ordering::Release);
        s
    }

    /// Arrête le producteur du flux en cours (l'émetteur, lui, continue).
    async fn arreter_la_session(&self) -> Option<u64> {
        let s = self.prendre_la_session()?;
        let id = s.id;
        let _ = tokio::task::spawn_blocking(move || s.arreter()).await;
        Some(id)
    }

    fn commandes(&self) -> Option<(Arc<Commandes>, Arc<Statistiques>)> {
        self.emetteur
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .map(|e| (e.commandes.clone(), e.stats.clone()))
    }

    /// L'émetteur, démarré à la première lecture.
    fn emetteur_pret(&self) -> Result<(Arc<Commandes>, Arc<Statistiques>), String> {
        let mut garde = self.emetteur.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(e) = garde.as_ref() {
            return Ok((e.commandes.clone(), e.stats.clone()));
        }
        let socket = reseau::ouvrir_socket(
            self.destination,
            self.flux.interface,
            self.flux.ttl,
            self.flux.dscp,
            true,
        )
        .map_err(|e| format!("socket RTP vers {} : {e}", self.destination))?;
        let commandes = Arc::new(Commandes::default());
        let stats = Arc::new(Statistiques::default());
        let fil = cadenceur::lancer(
            socket,
            None,
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
        )
        .map_err(|e| format!("fil RTP : {e}"))?;
        *garde = Some(Emetteur {
            commandes: commandes.clone(),
            stats: stats.clone(),
            fil: Some(fil),
        });
        *self
            .vitrine
            .emetteur
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some((commandes.clone(), stats.clone()));
        Ok((commandes, stats))
    }

    /// Pose un nouveau flux qui commence par `piste` à `depuis_ms` : un
    /// producteur neuf, un canal neuf, posé dans l'émetteur.
    fn poser_un_flux(
        &self,
        commandes: &Commandes,
        infos: InfosPiste,
        depuis_ms: u64,
    ) -> Result<(), String> {
        let runtime = tokio::runtime::Handle::current();
        let premiere = PisteLancee::lancer(
            &runtime,
            infos.clone(),
            depuis_ms,
            self.flux.cadence,
            self.flux.canaux,
        );
        let (tx, rx): (_, Receiver<Message>) = sync_channel(PAQUETS_EN_AVANCE);
        let id = self.prochain_flux.fetch_add(1, Ordering::Relaxed);
        let frontieres: Frontieres = Arc::new(Mutex::new(vec![Frontiere {
            trame: 0,
            infos,
            depuis_ms,
        }]));
        let arret_producteur = Arc::new(AtomicBool::new(false));
        let echec = Arc::new(Mutex::new(None));
        let producteur = producteur::lancer(
            runtime,
            premiere,
            ParametresProducteur {
                cadence: self.flux.cadence,
                canaux: self.flux.canaux,
                trames_par_paquet: self.trames_par_paquet,
                gain: self.gain.clone(),
                suivante: self.suivante.clone(),
                frontieres: frontieres.clone(),
                arret: arret_producteur.clone(),
                echec: echec.clone(),
            },
            tx,
        )
        .map_err(|e| format!("fil PCM : {e}"))?;
        commandes.installer(id, Some(rx));
        *self.session.lock().unwrap_or_else(|p| p.into_inner()) = Some(Session {
            id,
            arret_producteur,
            echec,
            producteur: Some(producteur),
            frontieres,
        });
        self.vitrine.session.store(id, Ordering::Release);
        Ok(())
    }

    /// Arrête les annonces SAP (retrait envoyé) et la lecture.
    pub fn fermer(&self) {
        if let Some(s) = self.prendre_la_session() {
            s.arreter();
        }
        if let Some(e) = self
            .emetteur
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            e.arreter();
        }
        self.suivante
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(mut a) = self
            .annonceur
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            a.arreter();
        }
        self.vitrine.annoncee.store(false, Ordering::Release);
    }

    /// La piste qui joue dans le flux en cours, et où.
    fn lecture(&self) -> Option<Lecture> {
        let garde = self.session.lock().unwrap_or_else(|p| p.into_inner());
        let s = garde.as_ref()?;
        let (commandes, stats) = self.commandes()?;
        let courant = stats.flux_courant.load(Ordering::Acquire) == s.id;
        let trames = if courant {
            stats
                .trames
                .load(Ordering::Relaxed)
                .saturating_sub(stats.trames_au_debut_du_flux.load(Ordering::Relaxed))
        } else {
            0
        };
        let frontieres = s.frontieres.lock().unwrap_or_else(|p| p.into_inner());
        let piste = producteur::frontiere_a(&frontieres, trames)?.clone();
        let position_ms =
            piste.depuis_ms + (trames - piste.trame) * 1000 / u64::from(self.flux.cadence);
        Some(Lecture {
            piste,
            position_ms,
            fini: courant && commandes.fini.load(Ordering::Acquire),
            pause: commandes.pause.load(Ordering::Acquire),
            echec: s.echec.lock().unwrap_or_else(|p| p.into_inner()).clone(),
        })
    }

    fn statut_de_la_session(&self) -> OutputStatus {
        let base = OutputStatus {
            volume: self.volume(),
            muted: self.muet.load(Ordering::Relaxed),
            ..OutputStatus::default()
        };
        let Some(l) = self.lecture() else {
            return base;
        };
        let state = if l.fini {
            TransportState::Stopped
        } else if l.pause {
            TransportState::Paused
        } else {
            TransportState::Playing
        };
        if l.fini
            && let Some(e) = l.echec.as_ref()
        {
            *self
                .echec_a_signaler
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(e.clone());
        }
        OutputStatus {
            state,
            position_ms: l.position_ms,
            duration_ms: l.piste.infos.duree_ms,
            current_uri: Some(l.piste.infos.uri.clone()),
            track_title: l.piste.infos.titre.clone(),
            track_artist: l.piste.infos.artiste.clone(),
            ended_naturally: l.fini && l.echec.is_none(),
            ..base
        }
    }
}

fn infos_de(media: &PlayMedia<'_>) -> InfosPiste {
    InfosPiste {
        source: SourceAudio::choisir(media.file_path, media.url),
        uri: media.url.to_string(),
        titre: media.title.map(str::to_string),
        artiste: media.artist.map(str::to_string),
        duree_ms: media.duration_ms.unwrap_or(0),
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
        OutputCapabilities::v1(true, true, true, v, v, true)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        if !self.droit.autorise().await {
            return Err("premium_required: la Sortie AES67 demande Premium".into());
        }
        self.arreter_la_session().await;
        self.suivante
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        self.recalculer_le_gain();
        let (commandes, _) = self.emetteur_pret()?;
        let infos = infos_de(media);
        info!(device_id = %self.device_id, source = ?infos.source, "aes67_lecture");
        commandes.pause.store(false, Ordering::Release);
        self.poser_un_flux(&commandes, infos, 0)
    }

    /// Prépare la piste suivante : son décodeur part tout de suite, le
    /// producteur l'enchaînera sans blanc à la fin de la piste en cours.
    async fn set_next_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        if !self.droit.autorise().await {
            return Err("premium_required: la Sortie AES67 demande Premium".into());
        }
        let infos = infos_de(media);
        info!(device_id = %self.device_id, source = ?infos.source, "aes67_suivante_preparee");
        let piste = PisteLancee::lancer(
            &tokio::runtime::Handle::current(),
            infos,
            0,
            self.flux.cadence,
            self.flux.canaux,
        );
        *self.suivante.lock().unwrap_or_else(|p| p.into_inner()) = Some(piste);
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        if self
            .session
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
            && let Some((c, _)) = self.commandes()
        {
            c.pause.store(true, Ordering::Release);
        }
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        if let Some((c, _)) = self.commandes() {
            c.pause.store(false, Ordering::Release);
        }
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        self.arreter_la_session().await;
        self.suivante
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some((c, _)) = self.commandes() {
            c.pause.store(false, Ordering::Release);
            let id = self.prochain_flux.fetch_add(1, Ordering::Relaxed);
            c.installer(id, None);
        }
        Ok(())
    }

    /// Saut dans la piste qui joue : un flux neuf depuis `position_ms`. En
    /// pause, la sortie reste en pause à la nouvelle position. La piste
    /// suivante préparée reste préparée.
    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        let lecture = self
            .lecture()
            .ok_or_else(|| "seek : aucune piste en cours sur la Sortie AES67".to_string())?;
        let (commandes, _) = self.emetteur_pret()?;
        self.arreter_la_session().await;
        info!(
            device_id = %self.device_id,
            de_ms = lecture.position_ms,
            vers_ms = position_ms,
            "aes67_saut"
        );
        self.poser_un_flux(&commandes, lecture.piste.infos, position_ms)
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
        let stats = self.commandes().map(|(_, s)| s.releve());
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
