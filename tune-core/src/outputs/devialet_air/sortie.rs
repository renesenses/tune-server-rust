//! La sortie Tune d'un ampli Devialet AIR.
//!
//! - `play_media` : allume l'ampli et choisit la source AIR s'il le faut
//!   (séquence de Roon), décode la piste en PCM stéréo 24 bits à une cadence
//!   que AIR transporte sans perte, puis ouvre un flux.
//! - Pause, reprise, saut : paquet d'arrêt, puis flux neuf depuis la position
//!   voulue, avec son silence de tête et son pré-remplissage (environ 2,5 s
//!   de latence, comme chez Roon).
//! - Pas d'enchaînement sans coupure dans cette version expérimentale.
//! - Volume et sourdine : commandes `Dr`. L'état RÉEL se lit dans l'annonce,
//!   car la télécommande infrarouge le change sans aucun message réseau.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::ecoute::{AmpliVu, Ecoute};
use super::emetteur::{
    Cadence, EtatFlux, FinDeFlux, Garde, LotDeTrames, ParametresFlux, diffuser, trames_arrondies,
};
use super::protocole::{self, Annonce, PORT_AMPLI, PORT_ANNONCES, PORT_AUDIO_LOCAL, opcode};
use super::reglages;
use crate::outputs::traits::{
    AudioSpec, FormatOuvert, OutputCapabilities, OutputStatus, OutputTarget, PlayMedia,
    TransformationsReelles, TransportState,
};

/// `output_type` des zones Devialet AIR.
pub const TYPE_DE_SORTIE: &str = "devialet_air";

/// L'identifiant de sortie d'un ampli : sa MAC, seule identité stable.
#[must_use]
pub fn identifiant_de_sortie(mac: &str) -> String {
    format!("devialet-air:{mac}")
}

/// Les ports du protocole. En service : 45454, 45455, 45456 sur toutes les
/// interfaces. Les tests les remplacent par des ports éphémères sur la
/// boucle locale, face au simulateur d'ampli.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ports {
    pub annonces: u16,
    pub ampli: u16,
    pub audio_local: u16,
    pub adresse_locale: Ipv4Addr,
}

impl Ports {
    pub const AIR: Self = Self {
        annonces: PORT_ANNONCES,
        ampli: PORT_AMPLI,
        audio_local: PORT_AUDIO_LOCAL,
        adresse_locale: Ipv4Addr::UNSPECIFIED,
    };
}

/// Ce dont une sortie et son fournisseur ont besoin, hors appareil.
#[derive(Clone)]
pub struct ContexteAir {
    /// L'option expérimentale est-elle activée ? Relue à chaque usage : le
    /// réglage s'applique sans redémarrer.
    pub activee: Arc<dyn Fn() -> bool + Send + Sync>,
    /// L'indice de la source AIR de l'ampli.
    pub indice_source: Arc<dyn Fn() -> u8 + Send + Sync>,
    /// #6918 — la course du curseur en dB. Relue à chaque consigne : une
    /// course changée vaut pour le geste suivant et n'envoie rien.
    pub bornes_db: Arc<dyn Fn() -> reglages::BornesDb + Send + Sync>,
    pub ports: Ports,
    pub cadence: Cadence,
}

impl ContexteAir {
    /// Le contexte de service : réglages lus en base, ports et durées du
    /// protocole.
    #[must_use]
    pub fn depuis_la_base(db: Arc<dyn crate::db::backend::DbBackend>) -> Self {
        let db2 = db.clone();
        let db3 = db.clone();
        Self {
            activee: Arc::new(move || reglages::activee(&db)),
            indice_source: Arc::new(move || reglages::indice_source(&db2)),
            bornes_db: Arc::new(move || reglages::BornesDb::lues(&db3)),
            ports: Ports::AIR,
            cadence: Cadence::AIR,
        }
    }
}

#[derive(Debug, Clone)]
struct Media {
    source: String,
    uri: String,
    titre: Option<String>,
    artiste: Option<String>,
    duree_ms: u64,
    bits: Option<u32>,
}

impl Media {
    fn depuis(media: &PlayMedia<'_>) -> Self {
        Self {
            source: media.file_path.unwrap_or(media.url).to_string(),
            uri: media.url.to_string(),
            titre: media.title.map(str::to_owned),
            artiste: media.artist.map(str::to_owned),
            duree_ms: media.duration_ms.unwrap_or(0),
            bits: media.bit_depth,
        }
    }
}

/// Un flux en cours.
struct Flux {
    media: Media,
    debut_ms: u64,
    frequence: u32,
    etat: Arc<Mutex<EtatFlux>>,
    socket: Arc<UdpSocket>,
    session: [u8; 4],
    tache: JoinHandle<FinDeFlux>,
}

#[derive(Default)]
enum Phase {
    #[default]
    Arret,
    Lecture(Flux),
    Pause {
        media: Media,
        position_ms: u64,
    },
    /// Le flux a fini (naturellement ou non) ; l'état le rend jusqu'à la
    /// commande suivante.
    Termine {
        media: Media,
        naturelle: bool,
    },
}

pub struct DevialetAirOutput {
    nom: String,
    device_id: String,
    mac: String,
    ecoute: Arc<Ecoute>,
    contexte: ContexteAir,
    phase: AsyncMutex<Phase>,
    compteur_flux: Arc<AtomicU32>,
    compteur_paquets: AtomicU16,
    /// Ce que le flux en cours fait au signal, pour le chemin du signal.
    transformations: Mutex<Option<TransformationsReelles>>,
    echec: Mutex<Option<String>>,
    /// Dernières consignes, pour l'état quand l'ampli ne s'annonce plus.
    volume_voulu: Mutex<f64>,
}

/// L'identifiant de flux suivant. Son octet de poids faible ne vaut jamais 0 :
/// l'annonce l'utilise pour dire « aucun flux ».
fn flux_suivant(compteur: &AtomicU32) -> u32 {
    loop {
        let f = compteur.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        if f & 0xFF != 0 {
            return f;
        }
    }
}

/// L'octet de flux d'une annonce désigne-t-il l'un de NOS flux récents ?
/// Juste après un arrêt, l'annonce peut encore montrer notre flux précédent
/// pendant une période : ce n'est pas un autre hôte.
fn est_un_de_nos_flux(compteur: &AtomicU32, octet: u8) -> bool {
    let c = compteur.load(Ordering::SeqCst);
    (0..8u32).any(|k| (c.wrapping_sub(k) & 0xFF) as u8 == octet)
}

fn graine_de_flux() -> u32 {
    // Un départ qui change d'un démarrage du serveur à l'autre, pour ne pas
    // reprendre l'identifiant d'un flux que l'ampli aurait encore en tête.
    let s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    ((s as u32) & 0x00FF_FF00) | 0x10
}

impl DevialetAirOutput {
    #[must_use]
    pub fn new(nom: String, mac: String, ecoute: Arc<Ecoute>, contexte: ContexteAir) -> Self {
        Self {
            device_id: identifiant_de_sortie(&mac),
            nom,
            mac,
            ecoute,
            contexte,
            phase: AsyncMutex::new(Phase::Arret),
            compteur_flux: Arc::new(AtomicU32::new(graine_de_flux())),
            compteur_paquets: AtomicU16::new(0),
            transformations: Mutex::new(None),
            echec: Mutex::new(None),
            volume_voulu: Mutex::new(reglages::db_vers_volume(-40.0)),
        }
    }

    fn ampli(&self) -> Option<AmpliVu> {
        self.ecoute.ampli(&self.mac)
    }

    /// #6912 — seulement une annonce encore présente, jamais volume_voulu.
    pub(crate) fn observation_volume(&self) -> Option<crate::playback::ObservationVolumeAppareil> {
        if !(self.contexte.activee)() {
            return None;
        }
        let ampli = self.ampli()?;
        let volume_db = ampli.annonce.volume_db();
        Some(crate::playback::ObservationVolumeAppareil {
            device_id: self.device_id.clone(),
            volume: (self.contexte.bornes_db)().db_vers_volume(volume_db),
            volume_db,
            muted: ampli.annonce.sourdine,
        })
    }

    fn adresse_ampli(&self, a: &AmpliVu) -> SocketAddr {
        SocketAddr::new(a.ip, self.contexte.ports.ampli)
    }

    fn exiger_l_option(&self) -> Result<(), String> {
        if (self.contexte.activee)() {
            Ok(())
        } else {
            Err(format!(
                "{}{}",
                reglages::SENTINELLE_NON_ACTIVEE,
                reglages::MOTIF_NON_ACTIVEE
            ))
        }
    }

    /// Envoie une commande, deux fois de suite comme Roon (deux compteurs de
    /// paquets consécutifs, même compteur de commande).
    async fn commander(&self, op: u16, valeur: f32) -> Result<(), String> {
        self.exiger_l_option()?;
        let a = self
            .ampli()
            .ok_or_else(|| format!("{} : ampli absent du réseau", self.nom))?;
        let cible = self.adresse_ampli(&a);
        let socket = UdpSocket::bind(SocketAddr::new(
            IpAddr::V4(self.contexte.ports.adresse_locale),
            0,
        ))
        .await
        .map_err(|e| format!("devialet_air: socket de commande : {e}"))?;
        let premier = self.compteur_paquets.fetch_add(2, Ordering::SeqCst);
        let commande = premier >> 1;
        for k in 0..2 {
            let o = protocole::commande(premier.wrapping_add(k), commande, op, valeur);
            socket
                .send_to(&o, cible)
                .await
                .map_err(|e| format!("devialet_air: envoi de commande : {e}"))?;
        }
        info!(sortie = %self.nom, op = %format_args!("{op:#06x}"), valeur, "devialet_air_commande");
        Ok(())
    }

    /// Attend que l'annonce vérifie `condition`, au plus `delai`.
    async fn attendre_l_annonce(
        &self,
        delai: Duration,
        condition: impl Fn(&Annonce) -> bool,
    ) -> Option<Annonce> {
        let fin = Instant::now() + delai;
        loop {
            if let Some(a) = self.ampli()
                && condition(&a.annonce)
            {
                return Some(a.annonce);
            }
            if Instant::now() >= fin {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// La séquence de Roon avant un flux : allumer si besoin, choisir la
    /// source AIR si besoin, et ne pas prendre un ampli qu'un autre hôte
    /// est en train de servir.
    async fn preparer_l_ampli(&self) -> Result<(AmpliVu, u8), String> {
        let c = self.contexte.cadence;
        let indice = (self.contexte.indice_source)();
        let a = self
            .ampli()
            .ok_or_else(|| format!("{} : ampli absent du réseau", self.nom))?;
        if !a.annonce.aucun_flux_air
            && a.annonce.flux_en_cours != 0
            && !est_un_de_nos_flux(&self.compteur_flux, a.annonce.flux_en_cours)
        {
            return Err(format!(
                "{} : l'ampli joue déjà un flux AIR d'un autre hôte (Roon ?) ; \
                 Tune ne le lui prend pas",
                self.nom
            ));
        }
        if !a.annonce.allume {
            info!(sortie = %self.nom, "devialet_air_allumage");
            self.commander(opcode::ALLUMER, 0.0).await?;
            self.attendre_l_annonce(c.attente_allumage, |a| a.allume)
                .await
                .ok_or_else(|| format!("{} : l'ampli ne s'est pas allumé", self.nom))?;
        }
        let a = self
            .ampli()
            .ok_or_else(|| format!("{} : ampli absent du réseau", self.nom))?;
        if a.annonce.source_choisie != indice {
            info!(sortie = %self.nom, indice, "devialet_air_choix_de_source");
            self.commander(opcode::SOURCE, f32::from(indice)).await?;
            self.attendre_l_annonce(c.attente_source, |a| a.source_choisie == indice)
                .await
                .ok_or_else(|| {
                    format!(
                        "{} : l'ampli n'a pas pris la source {indice} (réglage « source AIR »)",
                        self.nom
                    )
                })?;
        }
        let a = self
            .ampli()
            .ok_or_else(|| format!("{} : ampli absent du réseau", self.nom))?;
        Ok((a, indice))
    }

    /// Interrompt le flux en cours, s'il y en a un, et rend sa position.
    async fn interrompre(&self, phase: &mut Phase) -> Option<(Media, u64)> {
        let Phase::Lecture(flux) = std::mem::take(phase) else {
            return None;
        };
        self.oublier_les_transformations();
        let position = position_du_flux(&flux);
        flux.tache.abort();
        let fin = flux.tache.await;
        // Un flux qu'un autre hôte a pris ne reçoit pas d'arrêt de notre part.
        let envoyer = !matches!(
            fin,
            Ok(FinDeFlux::Interrompu {
                envoyer_l_arret: false,
                ..
            })
        );
        if envoyer && let Some(a) = self.ampli() {
            let arret = protocole::paquet_arret(flux.session, flux_suivant(&self.compteur_flux));
            let _ = flux.socket.send_to(&arret, self.adresse_ampli(&a)).await;
        }
        Some((flux.media, position))
    }

    /// Démarre un flux de `media` depuis `debut_ms`.
    async fn lancer(&self, phase: &mut Phase, media: Media, debut_ms: u64) -> Result<(), String> {
        self.exiger_l_option()?;
        let (ampli, indice) = self.preparer_l_ampli().await?;
        let (chemin, garde_fichier) =
            crate::outputs::airplay::url_to_local_path(&media.source).await?;
        let SourceOuverte {
            trames,
            cadence_source,
            cadence_envoyee,
        } = ouvrir_la_source(chemin, debut_ms, garde_fichier).await?;
        let socket = Arc::new(
            UdpSocket::bind(SocketAddr::new(
                IpAddr::V4(self.contexte.ports.adresse_locale),
                self.contexte.ports.audio_local,
            ))
            .await
            .map_err(|e| {
                format!(
                    "{} : port audio {} indisponible ({e}) — un autre lecteur AIR \
                     (Roon ?) joue-t-il depuis cette machine ?",
                    self.nom, self.contexte.ports.audio_local
                )
            })?,
        );
        let flux = flux_suivant(&self.compteur_flux);
        let p = ParametresFlux {
            session: ampli.annonce.session,
            flux,
            frequence: cadence_envoyee,
            cadence: self.contexte.cadence,
        };
        let entree = AudioSpec::depuis_entete(
            cadence_source,
            media.bits.and_then(|b| u16::try_from(b).ok()).unwrap_or(24),
            2,
        );
        let transformations = entree.map(|e| {
            TransformationsReelles::nouvelles(e, FormatOuvert::new(cadence_envoyee, 2), false)
        });
        info!(
            sortie = %self.nom,
            flux,
            cadence_source = cadence_source,
            cadence_envoyee = cadence_envoyee,
            debut_ms,
            "devialet_air_flux"
        );
        let etat = Arc::new(Mutex::new(EtatFlux::default()));
        let garde = self.garde_du_flux(indice);
        let cible = self.adresse_ampli(&ampli);
        let tache = {
            let socket = socket.clone();
            let etat = etat.clone();
            let compteur = flux_suivant(&self.compteur_flux);
            let session = p.session;
            let nom = self.nom.clone();
            tokio::spawn(async move {
                let fin = diffuser(socket.clone(), cible, p, trames, etat, garde).await;
                match &fin {
                    FinDeFlux::Naturelle => {
                        let arret = protocole::paquet_arret(session, compteur);
                        let _ = socket.send_to(&arret, cible).await;
                    }
                    FinDeFlux::Interrompu {
                        motif,
                        envoyer_l_arret,
                    } => {
                        warn!(sortie = %nom, %motif, "devialet_air_flux_interrompu");
                        if *envoyer_l_arret {
                            let arret = protocole::paquet_arret(session, compteur);
                            let _ = socket.send_to(&arret, cible).await;
                        }
                    }
                }
                fin
            })
        };
        *self
            .transformations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = transformations;
        *phase = Phase::Lecture(Flux {
            media,
            debut_ms,
            frequence: cadence_envoyee,
            etat,
            socket,
            session: p.session,
            tache,
        });
        Ok(())
    }

    /// Ce qui arrête un flux en cours de route : ampli parti ou en veille,
    /// source changée à la télécommande, ou un autre hôte qui a pris
    /// l'ampli (identifiant de flux étranger pendant plus d'une seconde).
    fn garde_du_flux(&self, indice: u8) -> Garde {
        let ecoute = self.ecoute.clone();
        let mac = self.mac.clone();
        let nom = self.nom.clone();
        let compteur = self.compteur_flux.clone();
        let etranger_depuis: Mutex<Option<Instant>> = Mutex::new(None);
        Arc::new(move |etat: &EtatFlux| {
            let Some(a) = ecoute.ampli(&mac) else {
                return Err(FinDeFlux::interrompu(format!(
                    "{nom} : l'ampli ne s'annonce plus"
                )));
            };
            let an = &a.annonce;
            if !an.allume {
                return Err(FinDeFlux::interrompu(format!(
                    "{nom} : l'ampli a été mis en veille"
                )));
            }
            if etat.position_jouee > 0 && an.source_choisie != indice {
                return Err(FinDeFlux::interrompu(format!(
                    "{nom} : la source de l'ampli a changé"
                )));
            }
            let mut depuis = etranger_depuis
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if an.flux_en_cours != 0 && !est_un_de_nos_flux(&compteur, an.flux_en_cours) {
                let t = *depuis.get_or_insert_with(Instant::now);
                if t.elapsed() > Duration::from_secs(1) {
                    return Err(FinDeFlux::Interrompu {
                        motif: format!("{nom} : un autre hôte a pris l'ampli"),
                        envoyer_l_arret: false,
                    });
                }
            } else {
                *depuis = None;
            }
            Ok(())
        })
    }

    fn oublier_les_transformations(&self) {
        *self
            .transformations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// Relève la fin d'un flux terminé : naturelle, ou un échec à rapporter.
    async fn constater_la_fin(&self, phase: &mut Phase) {
        let fini = matches!(phase, Phase::Lecture(f) if f.tache.is_finished());
        if !fini {
            return;
        }
        let Phase::Lecture(flux) = std::mem::take(phase) else {
            return;
        };
        self.oublier_les_transformations();
        let naturelle = match flux.tache.await {
            Ok(FinDeFlux::Naturelle) => true,
            Ok(FinDeFlux::Interrompu { motif, .. }) => {
                *self
                    .echec
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(motif);
                false
            }
            Err(e) => {
                *self
                    .echec
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) =
                    Some(format!("devialet_air: tâche du flux : {e}"));
                false
            }
        };
        *phase = Phase::Termine {
            media: flux.media,
            naturelle,
        };
    }
}

fn position_du_flux(flux: &Flux) -> u64 {
    let e = flux
        .etat
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if e.position_jouee <= e.silence_de_tete {
        return flux.debut_ms;
    }
    flux.debut_ms + (e.position_jouee - e.silence_de_tete) * 1_000 / u64::from(flux.frequence)
}

/// Une source PCM ouverte : les trames, et les deux cadences.
struct SourceOuverte {
    trames: mpsc::Receiver<LotDeTrames>,
    cadence_source: u32,
    cadence_envoyee: u32,
}

/// Les octets PCM du décodeur, et la tâche qui les produit (profondeur, débit).
type DecodeurOuvert = (
    mpsc::Receiver<Vec<u8>>,
    JoinHandle<Result<(u16, u32), String>>,
);

fn ouvrir_decodeur(chemin: &str, cadence: Option<u32>, depuis_s: f64) -> DecodeurOuvert {
    let (tx, rx) = mpsc::channel::<Vec<u8>>(8);
    let (niveaux, niveaux_rx) = mpsc::unbounded_channel();
    drop(niveaux_rx);
    let chemin = chemin.to_owned();
    let decodeur = tokio::task::spawn_blocking(move || {
        crate::audio::decode::decode_to_pcm_streaming_seeked(
            &chemin,
            cadence,
            Some(2),
            Some(24),
            tx,
            32 * 1024,
            Arc::new(tokio::sync::Notify::new()),
            niveaux,
            depuis_s,
        )
    });
    (rx, decodeur)
}

/// La cadence décrite par l'en-tête WAV que le décodeur émet en premier.
fn cadence_de_l_entete(entete: &[u8]) -> Option<u32> {
    crate::outputs::sendspin::est_entete_wav(entete)
        .then(|| u32::from_le_bytes([entete[24], entete[25], entete[26], entete[27]]))
}

/// Ouvre le décodage progressif en PCM stéréo 24 bits. La cadence native se
/// lit dans l'en-tête ; si AIR ne la transporte pas sans perte, le décodage
/// est rouvert à la cadence de [`reglages::cadence_envoyee`].
async fn ouvrir_la_source(
    chemin: String,
    debut_ms: u64,
    garde_fichier: Option<crate::outputs::airplay::TempFileGuard>,
) -> Result<SourceOuverte, String> {
    let depuis_s = debut_ms as f64 / 1_000.0;
    let (mut rx, decodeur) = ouvrir_decodeur(&chemin, None, depuis_s);
    let entete = rx.recv().await;
    let Some(cadence_source) = entete.as_deref().and_then(cadence_de_l_entete) else {
        drop(rx);
        return Err(match decodeur.await {
            Ok(Err(e)) => format!("devialet_air: décodage : {e}"),
            _ => "devialet_air: le décodeur n'a pas annoncé son format".into(),
        });
    };
    let cadence_envoyee = reglages::cadence_envoyee(cadence_source);
    let (mut rx, decodeur) = if cadence_envoyee == cadence_source {
        (rx, decodeur)
    } else {
        drop(rx);
        let _ = decodeur.await;
        let (mut rx, decodeur) = ouvrir_decodeur(&chemin, Some(cadence_envoyee), depuis_s);
        match rx.recv().await.as_deref().and_then(cadence_de_l_entete) {
            Some(c) if c == cadence_envoyee => {}
            autre => {
                return Err(format!(
                    "devialet_air: rééchantillonnage vers {cadence_envoyee} Hz refusé \
                     par le décodeur ({autre:?})"
                ));
            }
        }
        (rx, decodeur)
    };
    let (tx, trames) = mpsc::channel::<LotDeTrames>(4);
    tokio::spawn(async move {
        let _garde = garde_fichier;
        let mut reste: Vec<u8> = Vec::new();
        let mut envoye = false;
        while let Some(bloc) = rx.recv().await {
            reste.extend_from_slice(&bloc);
            let complets = reste.len() / 6 * 6;
            let lot: Vec<[i32; 2]> = reste[..complets]
                .as_chunks::<6>()
                .0
                .iter()
                .map(|t| [lire_24_le(&t[0..3]), lire_24_le(&t[3..6])])
                .collect();
            reste.drain(..complets);
            if lot.is_empty() {
                continue;
            }
            envoye = true;
            if tx.send(Ok(lot)).await.is_err() {
                return;
            }
        }
        if let Ok(Err(e)) = decodeur.await
            && !envoye
        {
            let _ = tx.send(Err(e)).await;
        }
    });
    Ok(SourceOuverte {
        trames,
        cadence_source,
        cadence_envoyee,
    })
}

fn lire_24_le(o: &[u8]) -> i32 {
    let v = i32::from(o[0]) | (i32::from(o[1]) << 8) | (i32::from(o[2]) << 16);
    (v << 8) >> 8
}

#[async_trait::async_trait]
impl OutputTarget for DevialetAirOutput {
    fn name(&self) -> &str {
        &self.nom
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        TYPE_DE_SORTIE
    }

    fn capabilities(&self) -> OutputCapabilities {
        // Pas d'enchaînement sans coupure dans cette version : chaque piste
        // ouvre un flux neuf. L'ampli règle son volume par pas de 0,5 dB.
        OutputCapabilities::v1(true, true, true, true, true, false).with_decibel_volume(500)
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        let mut phase = self.phase.lock().await;
        self.interrompre(&mut phase).await;
        *phase = Phase::Arret;
        self.lancer(&mut phase, Media::depuis(media), 0).await
    }

    async fn pause(&self) -> Result<(), String> {
        let mut phase = self.phase.lock().await;
        self.constater_la_fin(&mut phase).await;
        if let Some((media, position_ms)) = self.interrompre(&mut phase).await {
            *phase = Phase::Pause { media, position_ms };
        }
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        let mut phase = self.phase.lock().await;
        let Phase::Pause { media, position_ms } = std::mem::take(&mut *phase) else {
            return Ok(());
        };
        let reprise = self.lancer(&mut phase, media.clone(), position_ms).await;
        if reprise.is_err() {
            *phase = Phase::Pause { media, position_ms };
        }
        reprise
    }

    async fn stop(&self) -> Result<(), String> {
        let mut phase = self.phase.lock().await;
        self.interrompre(&mut phase).await;
        *phase = Phase::Arret;
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        let mut phase = self.phase.lock().await;
        self.constater_la_fin(&mut phase).await;
        match std::mem::take(&mut *phase) {
            Phase::Pause { media, .. } => {
                *phase = Phase::Pause { media, position_ms };
                Ok(())
            }
            Phase::Lecture(flux) => {
                *phase = Phase::Lecture(flux);
                let (media, _) = self
                    .interrompre(&mut phase)
                    .await
                    .ok_or("devialet_air: rien à déplacer")?;
                // #6923 — le flux d'avant est déjà coupé : si le flux neuf ne
                // part pas, la sortie reste arrêtée. Sans motif, le sondeur
                // prenait cet arrêt pour une fin de piste (pic de position
                // au-delà de 80 %) et passait au morceau suivant. Le motif
                // part par `take_output_failure` : la zone s'arrête en le
                // disant.
                let relance = self.lancer(&mut phase, media, position_ms).await;
                if let Err(e) = &relance {
                    warn!(sortie = %self.nom, position_ms, erreur = %e, "devialet_air_deplacement_echoue");
                    *self
                        .echec
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(format!("{} : déplacement impossible ({e})", self.nom));
                }
                relance
            }
            autre => {
                *phase = autre;
                Err("devialet_air: rien à déplacer".into())
            }
        }
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        let bornes = (self.contexte.bornes_db)();
        // #6918 — la course réglée, puis le plafond en dernier mot : aucune
        // consigne au-dessus de −10 dB ne part, quelle que soit la course.
        let db = bornes.volume_vers_db(volume).min(reglages::PLAFOND_DB);
        self.commander(opcode::VOLUME, db as f32).await?;
        *self
            .volume_voulu
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = bornes.db_vers_volume(db);
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        let op = if muted {
            opcode::SOURDINE
        } else {
            opcode::FIN_DE_SOURDINE
        };
        self.commander(op, 0.0).await
    }

    async fn get_status(&self) -> Result<OutputStatus, String> {
        let mut phase = self.phase.lock().await;
        self.constater_la_fin(&mut phase).await;
        let (state, position_ms, media, fini) = match &*phase {
            Phase::Arret => (TransportState::Stopped, 0, None, false),
            Phase::Lecture(f) => (
                TransportState::Playing,
                position_du_flux(f),
                Some(f.media.clone()),
                false,
            ),
            Phase::Pause { media, position_ms } => (
                TransportState::Paused,
                *position_ms,
                Some(media.clone()),
                false,
            ),
            Phase::Termine { media, naturelle } => (
                TransportState::Stopped,
                if *naturelle { media.duree_ms } else { 0 },
                Some(media.clone()),
                *naturelle,
            ),
        };
        drop(phase);
        let ampli = self.ampli();
        let volume = ampli.as_ref().map_or_else(
            || {
                *self
                    .volume_voulu
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            },
            |a| (self.contexte.bornes_db)().db_vers_volume(a.annonce.volume_db()),
        );
        Ok(OutputStatus {
            state,
            position_ms,
            duration_ms: media.as_ref().map_or(0, |m| m.duree_ms),
            volume,
            muted: ampli.as_ref().is_some_and(|a| a.annonce.sourdine),
            current_uri: media.as_ref().map(|m| m.uri.clone()),
            track_title: media.as_ref().and_then(|m| m.titre.clone()),
            track_artist: media.as_ref().and_then(|m| m.artiste.clone()),
            ended_naturally: fini,
            realtime: true,
            dop_active: false,
        })
    }

    async fn is_available(&self) -> bool {
        (self.contexte.activee)() && self.ampli().is_some()
    }

    fn take_output_failure(&self) -> Option<String> {
        self.echec
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    fn transformations_reelles(&self) -> Option<TransformationsReelles> {
        *self
            .transformations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn diagnostics_json(&self) -> Option<serde_json::Value> {
        let ampli = self.ampli();
        let phase = self.phase.try_lock().ok();
        let flux = phase.as_ref().and_then(|p| match &**p {
            Phase::Lecture(f) => Some(
                f.etat
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone(),
            ),
            _ => None,
        });
        Some(serde_json::json!({
            "experimental": true,
            "mac": self.mac,
            "present": ampli.is_some(),
            "ip": ampli.as_ref().map(|a| a.ip.to_string()),
            "allume": ampli.as_ref().map(|a| a.annonce.allume),
            "source_choisie": ampli.as_ref().map(|a| a.annonce.source_choisie),
            "source_air": (self.contexte.indice_source)(),
            "volume_db": ampli.as_ref().map(|a| a.annonce.volume_db()),
            "volume_range_db": (self.contexte.bornes_db)().json(),
            "sourdine": ampli.as_ref().map(|a| a.annonce.sourdine),
            "graves_db": ampli.as_ref().map(|a| f64::from(a.annonce.graves_demi_db) / 2.0),
            "aigus_db": ampli.as_ref().map(|a| f64::from(a.annonce.aigus_demi_db) / 2.0),
            "balance_pour_cent": ampli.as_ref().map(|a| a.annonce.balance_pour_cent),
            "flux": flux.map(|e| serde_json::json!({
                "position_envoyee": e.position_envoyee,
                "position_jouee": e.position_jouee,
                "retours_recus": e.retours_recus,
                "paquets_envoyes": e.paquets_envoyes,
                "paquets_renvoyes": e.paquets_renvoyes,
            })),
        }))
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Le silence de tête d'un flux à `frequence`, en trames (exposé pour les
/// tests de position).
#[must_use]
pub fn silence_de_tete(frequence: u32, cadence: &Cadence) -> u64 {
    trames_arrondies(frequence, cadence.silence_de_tete)
}
