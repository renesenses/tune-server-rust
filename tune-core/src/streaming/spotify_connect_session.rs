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

/// #8581 — quand Tune prend la main, l'échéance laissée à sa lecture pour
/// remplacer la lecture en cours de la zone : la zone est sondée jusque-là ;
/// si elle quitte la session, c'est la zone que Tune lance (on n'y touche
/// pas) ; si elle la joue encore à l'échéance (autre zone, lecture de Tune
/// échouée), elle est arrêtée.
pub const DELAI_REPRISE_PAR_TUNE: Duration = Duration::from_secs(10);
/// #8581 — l'intervalle de ce sondage.
const SONDE_REPRISE_PAR_TUNE: Duration = Duration::from_millis(200);

/// #8580 — la zone exposée a refusé la session : plafond de zones de
/// l'offre gratuite (`free_zone_cap:…`).
pub const CODE_PLAFOND_DE_ZONES: &str = "zone_cap_reached";
/// #8580 — la zone exposée a refusé la session : contrôle parental (hors
/// des plages, budget épuisé, contenu explicite).
pub const CODE_REFUS_PARENTAL: &str = "parental_refused";

/// #8580 — un refus de lecture de la zone exposée, rendu par un CODE STABLE
/// que l'écran traduit (`session_error`), au lieu d'une sentinelle interne
/// (`free_zone_cap:2:1`, `parental_lease_refused:…`). Tout autre motif est
/// rendu tel quel.
pub fn code_de_refus_de_session(erreur: &str) -> String {
    if erreur.contains("free_zone_cap:") {
        CODE_PLAFOND_DE_ZONES.into()
    } else if erreur.contains(crate::db::bail_d_ecoute::SENTINELLE)
        || erreur.contains(crate::db::politique_parentale::CODE_REFUS_EXPLICITE)
    {
        CODE_REFUS_PARENTAL.into()
    } else {
        erreur.to_string()
    }
}
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
    /// #8580 — le lancement de librespot sous lequel la session a été
    /// ouverte ([`SpotifyConnectManager::lancement_de_librespot`]).
    lancement: Option<u64>,
    /// #8580 — « le dernier geste gagne » : la session a été RENDUE à Tune
    /// (zone arrêtée, ou autre source lancée dans Tune sur la zone exposée).
    /// L'appareil est mis en pause ; seule une NOUVELLE action dans
    /// l'application Spotify (`paused`/`stopped` puis `playing`, ou une
    /// nouvelle connexion) peut rouvrir une session.
    rendu: bool,
}

impl EtatSession {
    pub fn zone(&self) -> Option<i64> {
        self.zone
    }
    /// #8580 — la session a-t-elle été rendue à Tune ?
    pub fn rendu(&self) -> bool {
        self.rendu
    }
    fn fermer(&mut self) {
        self.zone = None;
        self.en_pause = false;
        self.ouverte_a = None;
        self.lancement = None;
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
    // (sur une autre zone) s'arrête, la pompe ne la sert plus. « Le dernier
    // geste gagne » : l'évènement synthétique de Tune la ferme aussitôt, même
    // si la lecture de Tune échoue ensuite (la pompe est alors libre).
    if lecture_de_tune
        || ev.evenement == crate::streaming::spotify_connect::EVENEMENT_TUNE_PREND_LA_MAIN
    {
        return match etat.zone {
            Some(zone_id) => {
                etat.fermer();
                vec![Action::Arreter { zone_id }]
            }
            None => Vec::new(),
        };
    }
    // #8580 — session rendue à Tune : les titres que l'application continue
    // d'enchaîner ne rouvrent rien (ils couperaient l'album que Tune vient de
    // lancer, ou relanceraient la zone arrêtée). Une nouvelle action dans
    // l'application (pause ou arrêt, puis lecture ; nouvelle connexion) lève
    // la garde.
    if etat.rendu {
        if matches!(
            ev.evenement.as_str(),
            "paused"
                | "stopped"
                | "session_connected"
                | "session_disconnected"
                | "session_client_changed"
        ) {
            etat.rendu = false;
        }
        return Vec::new();
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
    /// #8580 — « le dernier geste gagne » : met en pause l'appareil Spotify
    /// Connect `nom`, pour que l'application Spotify voie que Tune a repris
    /// la zone.
    async fn rendre_l_appareil(&self, nom: &str);
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
        // Lu AVANT tout `await` (évènement reçu sous ce lancement) : une
        // relance de librespot pendant l'ouverture ne doit pas être prise pour
        // le lancement de la session.
        let lancement = self.recepteur.lancement_de_librespot();
        let zone_exposee = self.recepteur.zone_exposee().await;
        let pompe = self.recepteur.pompe();
        // #8580 — une session MORTE ne reste pas ouverte.
        // * librespot relancé (renommage, plantage, relance pour la liaison),
        //   sans geste de Tune : l'état est fermé, le `playing` suivant rouvre.
        // * GESTE DE TUNE (« le dernier geste gagne ») : la zone exposée a été
        //   arrêtée, ou une autre source y a été lancée. La session est RENDUE :
        //   l'appareil est mis en pause pour que l'application Spotify le voie,
        //   et les titres qu'elle enchaînerait encore ne rouvrent rien.
        //   Détecté au premier évènement qui suit le geste : rien n'est
        //   rouvert d'ici là, donc rien n'est coupé.
        //   SAUF si cet évènement est une NOUVELLE action dans l'application,
        //   liste fermée : `paused`, `stopped`, `session_connected`,
        //   `session_disconnected`, `session_client_changed`, et TOUT
        //   évènement reçu pendant que l'application est en pause (aucun
        //   enchaînement possible : volume, recherche, puis Lecture). C'est alors elle le dernier geste : la session
        //   est fermée sans pause de l'appareil ni garde, et l'évènement la
        //   rouvre s'il le faut. Tout autre évènement (`preload_next`,
        //   `end_of_track`, `loading`, `play_request_id_changed`,
        //   `track_changed`, `playing` hors pause…) est l'enchaînement
        //   automatique de la lecture en cours : la session est rendue.
        //
        //   Limite assumée (le scénario « l'album de Tune coupé » prime) : une
        //   nouvelle lecture lancée dans l'application SANS pause préalable,
        //   après un geste de Tune, est indiscernable d'un enchaînement : elle
        //   reste gardée jusqu'à une pause, un arrêt ou une nouvelle
        //   connexion.
        //
        //   Sortie de l'état « rendu » quand la pause de l'appareil a échoué
        //   (401, appareil absent) : l'application continue d'enchaîner, ce
        //   qui ne rouvre rien ; la garde tombe à la prochaine pause, au
        //   prochain arrêt ou à une nouvelle connexion. Un simple changement
        //   de titre ou de contexte ne la lève pas : librespot 0.8.0 n'émet
        //   aucun évènement propre au chargement d'un nouveau contexte
        //   (`loading` et `play_request_id_changed` sont émis titre par titre,
        //   enchaînement automatique compris), indiscernable d'une poursuite.
        if let Some(zone_id) = etat.zone
            && !pompe.consommateur_de_tune()
        {
            let relance = etat.lancement != Some(lancement);
            let joue = self.zones.joue_la_session(zone_id).await;
            if relance || !joue {
                // Liste FERMÉE des nouvelles actions de l'application ; tout le
                // reste (`preload_next`, `preloading`, `end_of_track`,
                // `play_request_id_changed`, `loading`, `track_changed`,
                // `playing` hors pause…) est l'enchaînement de la lecture en
                // cours, et rend la session.
                // Application EN PAUSE : aucun enchaînement possible, tout
                // évènement (volume, recherche dans le titre…) est une action
                // de l'utilisateur ; la Lecture suivante doit rouvrir.
                let nouvelle_action = etat.en_pause
                    || matches!(
                        ev.evenement.as_str(),
                        "paused"
                            | "stopped"
                            | "session_connected"
                            | "session_disconnected"
                            | "session_client_changed"
                    );
                let poursuite = !nouvelle_action;
                pompe.detacher_continu();
                if joue {
                    self.zones.arreter(zone_id).await;
                }
                etat.fermer();
                if !relance && poursuite {
                    etat.rendu = true;
                    self.zones
                        .rendre_l_appareil(&self.recepteur.device_name())
                        .await;
                }
                info!(zone_id, relance, evenement = %ev.evenement, "spotify_connect_session_morte_refermee");
            }
        }
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
                            etat.lancement = Some(lancement);
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
                    // Tune prend la main (#8581) : sa lecture peut viser CETTE
                    // zone, et elle n'a peut-être pas encore remplacé la
                    // lecture en cours. On laisse la résolution finir : si la
                    // zone joue encore la session ensuite (autre zone, ou
                    // lecture de Tune échouée), elle est arrêtée ; sinon elle
                    // joue le titre de Tune, on n'y touche pas.
                    let mut joue = self.zones.joue_la_session(zone_id).await;
                    if joue && pompe.consommateur_de_tune() {
                        let echeance = Instant::now() + DELAI_REPRISE_PAR_TUNE;
                        while joue && Instant::now() < echeance {
                            tokio::time::sleep(SONDE_REPRISE_PAR_TUNE).await;
                            joue = self.zones.joue_la_session(zone_id).await;
                        }
                    }
                    if joue {
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
        let r = self
            .0
            .play_transfere_sans_historique(req)
            .await
            .map_err(|e| code_de_refus_de_session(&e))?;
        if let Some(refus) = r.sortie_refusee() {
            return Err(refus.to_string());
        }
        if let Some(e) = r.error {
            return Err(code_de_refus_de_session(&e));
        }
        Ok(())
    }

    async fn rendre_l_appareil(&self, nom: &str) {
        let service = self.0.services.lock().await.get("spotify");
        let Some(service) = service else {
            return;
        };
        let service = service.read().await;
        if let Some(spotify) = service
            .as_any()
            .downcast_ref::<crate::streaming::spotify::SpotifyService>()
            && let Err(e) = spotify.mettre_en_pause_l_appareil(nom).await
        {
            warn!(erreur = %e, "spotify_connect_pause_apres_geste_de_tune_impossible");
        }
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
