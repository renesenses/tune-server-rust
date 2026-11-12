//! #8565 (fil 2778) — une lecture lancée DEPUIS L'APPLICATION SPOTIFY vers
//! le récepteur de Tune joue dans la zone exposée.
//!
//! Avant, le récepteur mémorisait la zone exposée sans jamais la relire : la
//! pompe jetait le PCM de librespot faute de consommateur, l'application
//! Spotify « jouait » sur l'appareil Tune et rien ne sortait nulle part.
//!
//! Le chemin, désormais :
//!
//! 1. librespot publie ses évènements (`--onevent`) ; le récepteur les remet,
//!    entiers, au [`PiloteConnect`] ;
//! 2. [`decider`] (pur) traduit chaque évènement en actions sur la zone :
//!    `playing` ouvre une session, `paused` / `playing` mettent en pause et
//!    reprennent, `track_changed` change le titre affiché, `stopped` /
//!    `session_disconnected` arrêtent, `volume_changed` règle le volume de
//!    la zone ;
//! 3. la session est une source PCM EN DIRECT ([`FournisseurConnect`],
//!    source [`SOURCE`]) servie comme une entrée audio : un seul flux sans
//!    fin, qui traverse les titres (la pompe est attachée en continu, la fin
//!    d'un titre ne la ferme pas) et que du silence comble quand librespot
//!    se tait (chargement, pause), pour que la sortie ne décroche pas.
//!
//! Règles :
//! * une lecture Spotify lancée PAR TUNE (#6018, option « Lecture Spotify
//!   (expérimental) ») garde la main : ses évènements ne pilotent rien ;
//! * Connect prend la main de la zone exposée, comme n'importe quel appareil
//!   Spotify : ce qui y jouait s'arrête ;
//! * le volume de l'application n'est JAMAIS appliqué à l'ouverture (on garde
//!   celui de la zone) ; ensuite il passe par `set_volume`, donc par les
//!   limites de sécurité et la garde de hausse de la zone ;
//! * pause, reprise, volume et arrêt ne touchent la zone que si elle joue
//!   ENCORE la session Connect : une zone reprise dans Tune n'est pas
//!   pilotée par l'application Spotify ;
//! * zone exposée absente ou refusée : rien ne joue, la raison est
//!   journalisée (`spotify_connect_session_impossible`) et rendue par le
//!   statut du récepteur.
//!
//! Indépendant de l'option « Lecture Spotify (expérimental) » : c'est le
//! récepteur lui-même (Réglages › Spotify Connect › « Activer le récepteur »)
//! qui promet que « la lecture arrive dans cette zone ».

use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::source_pcm::{
    Consommation, FluxDirect, FluxPcm, FormatPcm, FournisseurPcm, QualiteRecue,
};
use crate::streaming::spotify_connect::{
    EvenementLibrespot, OCTETS_PAR_SECONDE, OCTETS_PAR_TRAME, SpotifyConnectManager,
};

/// Le nom de la source d'une session Spotify Connect, dans `NowPlaying`.
pub const SOURCE: &str = "spotify-connect";
/// Le `source_id` de la session (il n'y en a qu'une par récepteur).
pub const SOURCE_ID: &str = "session";
/// #8565 — le code rendu par le statut quand la zone exposée n'existe plus.
pub const CODE_ZONE_EXPOSEE_ABSENTE: &str = "exposed_zone_missing";
/// Le titre affiché tant que librespot n'a pas nommé le morceau.
pub const TITRE_PAR_DEFAUT: &str = "Spotify Connect";
/// Les évènements de volume reçus juste après l'ouverture sont ceux de la
/// connexion (le volume mémorisé par l'application) : ignorés.
pub const DELAI_AVANT_VOLUME: Duration = Duration::from_millis(1500);
/// Sans PCM de librespot pendant ce délai, la session sert du silence.
const ATTENTE_AVANT_SILENCE: Duration = Duration::from_millis(100);

/// Ce que l'application Spotify dit du morceau en cours.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Morceau {
    pub piste: Option<String>,
    pub titre: Option<String>,
    pub artistes: Option<String>,
    pub album: Option<String>,
    pub pochette: Option<String>,
    pub duree_ms: Option<u64>,
}

impl Morceau {
    fn depuis(ev: &EvenementLibrespot) -> Self {
        Self {
            piste: ev.piste.clone(),
            titre: ev.titre.clone(),
            artistes: ev.artistes.clone(),
            album: ev.album.clone(),
            pochette: ev.pochette.clone(),
            duree_ms: ev.duree_ms,
        }
    }
}

/// Une action sur la zone exposée.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Ouvrir {
        zone_id: i64,
        morceau: Morceau,
    },
    Morceau {
        zone_id: i64,
        morceau: Morceau,
    },
    Pause {
        zone_id: i64,
    },
    Reprise {
        zone_id: i64,
    },
    Arreter {
        zone_id: i64,
    },
    /// `volume` entre 0 et 1.
    Volume {
        zone_id: i64,
        volume: f64,
    },
}

/// L'état de la session, tenu par le pilote.
#[derive(Debug, Default)]
pub struct EtatSession {
    zone: Option<i64>,
    en_pause: bool,
    ouverte_a: Option<Instant>,
    morceau: Morceau,
}

impl EtatSession {
    pub fn zone(&self) -> Option<i64> {
        self.zone
    }
    fn fermer(&mut self) {
        self.zone = None;
        self.en_pause = false;
        self.ouverte_a = None;
    }
}

/// Traduit un évènement de librespot en actions sur la zone. Pure.
///
/// `zone_exposee` : la zone du récepteur s'il est activé ; `lecture_de_tune` :
/// la pompe sert une lecture lancée par Tune (#6018).
pub fn decider(
    etat: &mut EtatSession,
    ev: &EvenementLibrespot,
    zone_exposee: Option<i64>,
    lecture_de_tune: bool,
    maintenant: Instant,
) -> Vec<Action> {
    // Une lecture lancée par Tune garde la main ; une session encore ouverte
    // (sur une autre zone) s'arrête, la pompe ne la sert plus.
    if lecture_de_tune {
        return match etat.zone {
            Some(zone_id) => {
                etat.fermer();
                vec![Action::Arreter { zone_id }]
            }
            None => Vec::new(),
        };
    }
    // Récepteur désactivé, ou zone exposée changée : la session se ferme.
    if let Some(zone_id) = etat.zone
        && zone_exposee != Some(zone_id)
    {
        etat.fermer();
        return vec![Action::Arreter { zone_id }];
    }
    let ouverte = etat.zone;
    match ev.evenement.as_str() {
        "track_changed" => {
            etat.morceau = Morceau::depuis(ev);
            match ouverte {
                Some(zone_id) => vec![Action::Morceau {
                    zone_id,
                    morceau: etat.morceau.clone(),
                }],
                None => Vec::new(),
            }
        }
        "playing" => {
            // Un `playing` qui nomme un autre titre que le dernier connu (les
            // métadonnées viennent avec `track_changed`) : on garde au moins
            // son identifiant et sa durée.
            let autre_titre = ev.piste.is_some() && ev.piste != etat.morceau.piste;
            if autre_titre {
                etat.morceau = Morceau {
                    piste: ev.piste.clone(),
                    duree_ms: ev.duree_ms,
                    ..Default::default()
                };
            }
            match ouverte {
                None => {
                    let Some(zone_id) = zone_exposee else {
                        return Vec::new();
                    };
                    etat.zone = Some(zone_id);
                    etat.en_pause = false;
                    etat.ouverte_a = Some(maintenant);
                    vec![Action::Ouvrir {
                        zone_id,
                        morceau: etat.morceau.clone(),
                    }]
                }
                Some(zone_id) if etat.en_pause => {
                    etat.en_pause = false;
                    let mut actions = vec![Action::Reprise { zone_id }];
                    if autre_titre {
                        actions.push(Action::Morceau {
                            zone_id,
                            morceau: etat.morceau.clone(),
                        });
                    }
                    actions
                }
                Some(zone_id) if autre_titre => vec![Action::Morceau {
                    zone_id,
                    morceau: etat.morceau.clone(),
                }],
                Some(_) => Vec::new(),
            }
        }
        "paused" => match ouverte {
            Some(zone_id) if !etat.en_pause => {
                etat.en_pause = true;
                vec![Action::Pause { zone_id }]
            }
            _ => Vec::new(),
        },
        "stopped" | "session_disconnected" => match ouverte {
            Some(zone_id) => {
                etat.fermer();
                vec![Action::Arreter { zone_id }]
            }
            None => Vec::new(),
        },
        "volume_changed" => match (ouverte, ev.volume, etat.ouverte_a) {
            (Some(zone_id), Some(v), Some(depuis))
                if maintenant.saturating_duration_since(depuis) >= DELAI_AVANT_VOLUME =>
            {
                vec![Action::Volume {
                    zone_id,
                    volume: f64::from(v) / f64::from(u16::MAX),
                }]
            }
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

/// Ce que le pilote fait à une zone. Séparé pour être doublé en test : la
/// production passe par l'orchestrateur.
#[async_trait]
pub trait ZonesConnect: Send + Sync {
    /// Ouvre la session sur la zone ; `Err` : la zone n'existe pas ou a
    /// refusé.
    async fn ouvrir(&self, zone_id: i64, morceau: &Morceau) -> Result<(), String>;
    /// La zone joue-t-elle ENCORE la session Connect ?
    async fn joue_la_session(&self, zone_id: i64) -> bool;
    async fn morceau(&self, zone_id: i64, morceau: &Morceau);
    async fn pause(&self, zone_id: i64);
    async fn reprise(&self, zone_id: i64);
    async fn arreter(&self, zone_id: i64);
    async fn volume(&self, zone_id: i64, volume: f64);
}

/// Le pilote : reçoit les évènements du récepteur et les applique.
pub struct PiloteConnect<Z: ZonesConnect> {
    pub recepteur: Arc<SpotifyConnectManager>,
    pub zones: Z,
}

impl<Z: ZonesConnect> PiloteConnect<Z> {
    pub async fn piloter(self, mut rx: mpsc::UnboundedReceiver<EvenementLibrespot>) {
        let mut etat = EtatSession::default();
        while let Some(ev) = rx.recv().await {
            self.traiter(&mut etat, &ev).await;
        }
    }

    pub async fn traiter(&self, etat: &mut EtatSession, ev: &EvenementLibrespot) {
        let zone_exposee = self.recepteur.zone_exposee().await;
        let pompe = self.recepteur.pompe();
        let actions = decider(
            etat,
            ev,
            zone_exposee,
            pompe.consommateur_de_tune(),
            Instant::now(),
        );
        for action in actions {
            match action {
                Action::Ouvrir { zone_id, morceau } => {
                    match self.zones.ouvrir(zone_id, &morceau).await {
                        Ok(()) => {
                            self.recepteur.noter_erreur_de_session(None);
                            info!(zone_id, piste = ?morceau.piste, "spotify_connect_session_ouverte");
                        }
                        Err(e) => {
                            warn!(zone_id, erreur = %e, "spotify_connect_session_impossible");
                            self.recepteur.noter_erreur_de_session(Some(e));
                            etat.fermer();
                        }
                    }
                }
                Action::Morceau { zone_id, morceau } => {
                    if self.zones.joue_la_session(zone_id).await {
                        self.zones.morceau(zone_id, &morceau).await;
                    }
                }
                Action::Pause { zone_id } => {
                    if self.zones.joue_la_session(zone_id).await {
                        self.zones.pause(zone_id).await;
                    }
                }
                Action::Reprise { zone_id } => {
                    if self.zones.joue_la_session(zone_id).await {
                        self.zones.reprise(zone_id).await;
                    }
                }
                Action::Arreter { zone_id } => {
                    if self.zones.joue_la_session(zone_id).await {
                        self.zones.arreter(zone_id).await;
                    }
                    pompe.detacher_continu();
                    info!(zone_id, evenement = %ev.evenement, "spotify_connect_session_fermee");
                }
                Action::Volume { zone_id, volume } => {
                    if self.zones.joue_la_session(zone_id).await {
                        self.zones.volume(zone_id, volume).await;
                    }
                }
            }
        }
    }
}

/// La production : les zones de l'orchestrateur.
pub struct ZonesDeLOrchestrateur(pub Arc<crate::orchestrator::PlaybackOrchestrator>);

#[async_trait]
impl ZonesConnect for ZonesDeLOrchestrateur {
    async fn ouvrir(&self, zone_id: i64, morceau: &Morceau) -> Result<(), String> {
        let existe = crate::db::zone_repo::ZoneRepo::with_backend(self.0.db.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .is_some();
        if !existe {
            // Un code stable : l'écran le traduit (#7077).
            return Err(CODE_ZONE_EXPOSEE_ABSENTE.into());
        }
        // Comme un point de contrôle UPnP (#7974) : pas de profil Tune.
        self.0.playback.set_session_profile(zone_id, None).await;
        let req = crate::orchestrator::PlayRequest {
            zone_id,
            source: Some(SOURCE.into()),
            source_id: Some(SOURCE_ID.into()),
            title: Some(
                morceau
                    .titre
                    .clone()
                    .unwrap_or_else(|| TITRE_PAR_DEFAUT.into()),
            ),
            artist_name: morceau.artistes.clone(),
            album_title: morceau.album.clone(),
            cover_url: morceau.pochette.clone(),
            ..Default::default()
        };
        let r = self.0.play_transfere_sans_historique(req).await?;
        if let Some(refus) = r.sortie_refusee() {
            return Err(refus.to_string());
        }
        if let Some(e) = r.error {
            return Err(e);
        }
        Ok(())
    }

    async fn joue_la_session(&self, zone_id: i64) -> bool {
        self.0
            .playback
            .get_state(zone_id)
            .await
            .now_playing
            .is_some_and(|np| np.source == SOURCE)
    }

    async fn morceau(&self, zone_id: i64, morceau: &Morceau) {
        let Some(mut np) = self.0.playback.get_state(zone_id).await.now_playing else {
            return;
        };
        np.title = morceau
            .titre
            .clone()
            .unwrap_or_else(|| TITRE_PAR_DEFAUT.into());
        np.artist_name = morceau.artistes.clone();
        np.album_title = morceau.album.clone();
        np.cover_path = morceau.pochette.clone();
        np.duration_ms = morceau
            .duree_ms
            .and_then(|d| i64::try_from(d).ok())
            .unwrap_or(0);
        self.0.playback.update_now_playing(zone_id, np).await;
    }

    async fn pause(&self, zone_id: i64) {
        if let Err(e) = self.0.pause(zone_id, None).await {
            warn!(zone_id, erreur = %e, "spotify_connect_pause_refusee");
        }
    }

    async fn reprise(&self, zone_id: i64) {
        if let Err(e) = self.0.resume(zone_id, None).await {
            warn!(zone_id, erreur = %e, "spotify_connect_reprise_refusee");
        }
    }

    async fn arreter(&self, zone_id: i64) {
        self.0.stop(zone_id, None).await;
    }

    async fn volume(&self, zone_id: i64, volume: f64) {
        if let Err(e) = self.0.set_volume(zone_id, volume, None).await {
            warn!(zone_id, erreur = %e, "spotify_connect_volume_refuse");
        }
    }
}

/// La source PCM en direct d'une session Spotify Connect.
pub struct FournisseurConnect {
    recepteur: Arc<SpotifyConnectManager>,
}

impl FournisseurConnect {
    pub fn new(recepteur: Arc<SpotifyConnectManager>) -> Self {
        Self { recepteur }
    }
}

impl FournisseurPcm for FournisseurConnect {
    fn qualite_recue(&self) -> Option<QualiteRecue> {
        let q = self.recepteur.qualite_recue();
        Some(QualiteRecue {
            codec: q.codec.into(),
            debit_kbps: Some(q.bitrate_kbps),
        })
    }

    fn ouvrir(&self, source_id: &str, _depuis_ms: u64) -> Result<FluxPcm, String> {
        Err(format!(
            "« {source_id} » est une session Spotify Connect en direct : elle se pilote depuis l'application Spotify"
        ))
    }

    fn en_direct(&self) -> bool {
        true
    }

    fn ouvrir_direct(&self, _source_id: &str, _c: Consommation) -> Result<FluxDirect, String> {
        let rx = self.recepteur.pompe().attacher_continu();
        Ok(FluxDirect {
            format: FormatPcm::CD,
            lecteur: Box::new(LecteurDeSession {
                rx,
                rt: tokio::runtime::Handle::current(),
                courant: Vec::new(),
                pos: 0,
            }),
            etat: None,
        })
    }
}

/// Lit la pompe ; quand librespot se tait, rend du silence au rythme du
/// temps réel. Canal fermé (pompe reprise, session détachée) : fin normale.
struct LecteurDeSession {
    rx: mpsc::Receiver<Vec<u8>>,
    rt: tokio::runtime::Handle,
    courant: Vec<u8>,
    pos: usize,
}

impl Read for LecteurDeSession {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.courant.len() {
            match self
                .rt
                .block_on(tokio::time::timeout(ATTENTE_AVANT_SILENCE, self.rx.recv()))
            {
                Ok(Some(bloc)) => {
                    self.courant = bloc;
                    self.pos = 0;
                }
                Ok(None) => return Ok(0),
                Err(_) => {
                    // Le silence du délai écoulé, en trames entières.
                    let voulu =
                        OCTETS_PAR_SECONDE * ATTENTE_AVANT_SILENCE.as_millis() as usize / 1000;
                    let n = voulu.min(buf.len());
                    let n = n - n % OCTETS_PAR_TRAME;
                    buf[..n].fill(0);
                    return Ok(n);
                }
            }
        }
        let n = (self.courant.len() - self.pos).min(buf.len());
        buf[..n].copy_from_slice(&self.courant[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
#[path = "spotify_connect_session_8565_tests.rs"]
mod tests;
