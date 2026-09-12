use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use reqwest::Client;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use super::didl::{DidlBuilder, ProtocolStyle};
use super::oh_events::{EventState, UpnpEventListener};
use super::traits::{OutputCapabilities, OutputStatus, OutputTarget, PlayMedia, TransportState};
use crate::discovery::redecouverte::{self, UrlsDeControle};
use crate::http::error as http_error;

const AV_TRANSPORT_URN: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RENDERING_CONTROL_URN: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const SOAP_MAX_RETRIES: usize = 2;

/// Convertit une durée UPnP `H:MM:SS[.mmm]` en millisecondes. `0` si la forme
/// n'est pas reconnue — ce que rend aussi `NOT_IMPLEMENTED`, la réponse
/// normalisée d'un renderer qui ignore sa propre durée.
///
/// Fonction libre parce que les évènements GENA en ont besoin autant que les
/// réponses SOAP : `CurrentTrackDuration` et `RelativeTimePosition` arrivent
/// dans le `LastChange` sous exactement la même forme que dans un
/// `GetPositionInfo` (#2263).
pub fn parse_upnp_time(time_str: &str) -> u64 {
    let parts: Vec<&str> = time_str.split(':').collect();
    if parts.len() == 3 {
        let h: u64 = parts[0].parse().unwrap_or(0);
        let m: u64 = parts[1].parse().unwrap_or(0);
        let s_parts: Vec<&str> = parts[2].split('.').collect();
        let s: u64 = s_parts[0].parse().unwrap_or(0);
        let frac_ms: u64 = if s_parts.len() > 1 {
            let frac = s_parts[1];
            let val: u64 = frac.parse().unwrap_or(0);
            match frac.len() {
                1 => val * 100,
                2 => val * 10,
                3 => val,
                _ => val / 10u64.pow(frac.len() as u32 - 3),
            }
        } else {
            0
        };
        (h * 3600 + m * 60 + s) * 1000 + frac_ms
    } else {
        0
    }
}

/// Préfixe des erreurs SOAP dues à un **timeout**, par opposition à un refus de
/// connexion.
///
/// La distinction porte une information que l'orchestrateur exploite : un
/// timeout ne prouve pas que la commande a été rejetée. La requête a très bien
/// pu atteindre un renderer lent et être exécutée — nous n'avons simplement pas
/// eu la réponse à temps. Détruire la session de flux dans ce cas garantit que
/// le renderer, lorsqu'il ira chercher l'URL, tombera sur un 404 et affichera
/// « chanson non trouvée » (Cyrus Stream X2 de JP).
///
/// Toute modification de cette chaîne doit suivre dans
/// `orchestrator::command_may_have_landed`.
pub const SOAP_TIMEOUT_PREFIX: &str = "soap timeout:";

/// Préfixe des erreurs « statut HTTP d'échec SANS corps SOAP ».
///
/// Un défaut SOAP légitime voyage DANS un 500 avec un corps `UPnPError` — le
/// spec UPnP l'impose — et nos appelants le lisent (la reprise 714 en dépend).
/// Mais un 500 au corps VIDE n'est pas un défaut SOAP : c'est un serveur qui
/// n'a pas su LIRE la requête. Platinum/1.0.5.13 (Eversolo DMP-A8) répond
/// `500 Bad Request: Error Parsing XML Body`, corps vide, quand le corps
/// dépasse un segment TCP : il parse sa première lecture et jette le reste —
/// les octets suivants restent dans la Send-Q (constaté sur .18, 25/08).
/// L'ancien code ne regardait que le corps : ce 500 vide passait pour un
/// acquittement, et la zone « jouait » une piste que le renderer n'avait
/// jamais reçue.
pub(crate) const SOAP_HTTP_SANS_CORPS_PREFIX: &str = "soap http sans corps:";
/// Timeout for the fire-and-forget Stop sent before SetAVTransportURI.
/// Kept short (2s) because we don't need the response — SetAVTransportURI
/// implicitly stops the current track on compliant renderers.
const STOP_BEFORE_PLAY_TIMEOUT_MS: u64 = 2000;

/// Base du délai avant de remettre à l'épreuve un niveau de DIDL dégradé.
///
/// Une minute : plus court que la plus courte des pistes d'une file ordinaire,
/// donc un hoquet ISOLÉ se rattrape dès la piste suivante et l'utilisateur ne
/// voit qu'une piste sans son format.
const DIDL_RESONDE_BASE_MS: u64 = 60_000;

/// Plafond de ce délai. Il double à chaque remise à l'épreuve qui échoue :
/// l'appareil qui ne sait VRAIMENT pas lire un DIDL complet — la pile
/// Platinum de l'Eversolo, #2394 — converge vers UN aller-retour perdu par
/// heure, pas un par piste.
const DIDL_RESONDE_MAX_MS: u64 = 3_600_000;

/// Horloge monotone du processus, en millisecondes.
///
/// `Instant` ne se range pas dans un atomique et l'heure murale peut reculer
/// (NTP, réveil de veille) — or un délai qui recule rouvrirait la sonde en
/// boucle. Une origine posée une fois, et des millisecondes écoulées depuis.
fn horloge_process_ms() -> u64 {
    static ORIGINE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    ORIGINE
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
}

/// Ce qu'un apprentissage vient de faire au niveau de DIDL. Rendu par
/// [`NiveauDidlAppris::apprendre`], qui en journalise chaque forme : la
/// dégradation était MUETTE jusqu'ici (#3675), et c'est ce silence qui a rendu
/// le dossier si long à instruire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransitionNiveauDidl {
    /// Le niveau n'a pas bougé et aucune remise à l'épreuve n'était due.
    Inchange,
    /// Les métadonnées s'appauvrissent (le numéro de niveau MONTE).
    Degrade { ancien: u8, neuf: u8 },
    /// Les métadonnées reviennent (le numéro de niveau DESCEND) : la remise à
    /// l'épreuve a réussi, l'appareil avait seulement hoqueté.
    Restaure { ancien: u8, neuf: u8 },
    /// La remise à l'épreuve a de nouveau échoué : on espace la suivante.
    ResondeEchouee { attente_ms: u64 },
}

/// Niveau de DIDL appris pour UN appareil — **avec sa porte de sortie**.
///
/// Le niveau (0 = complet, 1 = minimal, 2 = vide) est appris à l'usage : on
/// démarre l'échelle là où l'appareil a fini par répondre, pour ne pas re-payer
/// l'aller-retour raté à chaque piste (DMP-A8, #2394).
///
/// 🔴 Mais cet apprentissage ne savait que DESCENDRE : les deux seuls `store`
/// du fichier ne montaient jamais en qualité. Une SEULE réponse « 500 sans
/// corps » — que `soap_action` rend sans même la réessayer — privait donc
/// TOUTES les pistes suivantes, pour la vie du processus, de `sampleFrequency`
/// et `bitsPerSample` (et aussi de l'artiste, de l'album et de la pochette,
/// que le DIDL minimal n'écrit pas davantage). Un appareil qui hoquette une
/// fois au réveil ou pendant une bascule d'entrée restait dégradé à jamais, et
/// RIEN ne le disait : « le Marantz perd le format (44/16) » (#3675).
///
/// Le remède est un délai qui DOUBLE : la dégradation reste apprise, mais elle
/// expire. Un hoquet isolé se rattrape à la piste suivante ; un appareil qui ne
/// sait vraiment pas faire n'est plus sondé qu'une fois l'heure.
pub(crate) struct NiveauDidlAppris {
    niveau: AtomicU8,
    /// Horodatage ([`horloge_process_ms`]) du dernier apprentissage d'un
    /// niveau dégradé, ou de la dernière remise à l'épreuve ratée.
    appris_a_ms: AtomicU64,
    /// Délai courant avant la prochaine remise à l'épreuve.
    attente_ms: AtomicU64,
}

impl NiveauDidlAppris {
    fn neuf() -> Self {
        Self {
            niveau: AtomicU8::new(0),
            appris_a_ms: AtomicU64::new(0),
            attente_ms: AtomicU64::new(DIDL_RESONDE_BASE_MS),
        }
    }

    /// Vrai quand un niveau dégradé a passé son délai de remise à l'épreuve.
    fn resonde_due(&self, maintenant_ms: u64) -> bool {
        self.niveau.load(Ordering::Relaxed) > 0
            && maintenant_ms.saturating_sub(self.appris_a_ms.load(Ordering::Relaxed))
                >= self.attente_ms.load(Ordering::Relaxed)
    }

    /// Le niveau auquel démarrer l'échelle pour la prochaine émission.
    ///
    /// **Seule** façon de lire le niveau appris depuis la production : il n'y a
    /// pas d'accesseur brut, pour qu'aucun chemin ne puisse repartir du niveau
    /// dégradé en contournant la porte.
    fn niveau_de_depart(&self, maintenant_ms: u64, appareil: &str) -> u8 {
        if self.resonde_due(maintenant_ms) {
            info!(
                device = %appareil,
                niveau_appris = self.niveau.load(Ordering::Relaxed),
                attente_ms = self.attente_ms.load(Ordering::Relaxed),
                "dlna_didl_niveau_resonde"
            );
            return 0;
        }
        self.niveau.load(Ordering::Relaxed)
    }

    /// Enregistre le niveau qui a FINI par passer, et journalise le passage
    /// dans les DEUX sens.
    ///
    /// `maintenant_ms` doit être celui passé à [`Self::niveau_de_depart`] pour
    /// la même émission : c'est ce qui rend les deux décisions cohérentes — une
    /// remise à l'épreuve due au départ est encore due à l'arrivée.
    fn apprendre(&self, niveau: u8, maintenant_ms: u64, appareil: &str) -> TransitionNiveauDidl {
        let resondait = self.resonde_due(maintenant_ms);
        let ancien = self.niveau.swap(niveau, Ordering::Relaxed);
        let transition = if niveau < ancien {
            self.attente_ms
                .store(DIDL_RESONDE_BASE_MS, Ordering::Relaxed);
            self.appris_a_ms.store(maintenant_ms, Ordering::Relaxed);
            TransitionNiveauDidl::Restaure {
                ancien,
                neuf: niveau,
            }
        } else if niveau > ancien {
            self.attente_ms
                .store(DIDL_RESONDE_BASE_MS, Ordering::Relaxed);
            self.appris_a_ms.store(maintenant_ms, Ordering::Relaxed);
            TransitionNiveauDidl::Degrade {
                ancien,
                neuf: niveau,
            }
        } else if niveau > 0 && resondait {
            let attente = self
                .attente_ms
                .load(Ordering::Relaxed)
                .saturating_mul(2)
                .min(DIDL_RESONDE_MAX_MS);
            self.attente_ms.store(attente, Ordering::Relaxed);
            self.appris_a_ms.store(maintenant_ms, Ordering::Relaxed);
            TransitionNiveauDidl::ResondeEchouee {
                attente_ms: attente,
            }
        } else {
            TransitionNiveauDidl::Inchange
        };

        // Les deux sens laissent une trace NOMMÉE. Sans elles, la dégradation
        // était invisible : le seul indice était un `warn` par tentative, qui
        // ne disait pas que l'appareil venait de changer d'état DURABLEMENT.
        match transition {
            TransitionNiveauDidl::Degrade { ancien, neuf } => warn!(
                device = %appareil,
                ancien_niveau = ancien,
                niveau = neuf,
                attente_ms = DIDL_RESONDE_BASE_MS,
                "dlna_didl_niveau_degrade"
            ),
            TransitionNiveauDidl::Restaure { ancien, neuf } => info!(
                device = %appareil,
                ancien_niveau = ancien,
                niveau = neuf,
                "dlna_didl_niveau_restaure"
            ),
            TransitionNiveauDidl::ResondeEchouee { attente_ms } => debug!(
                device = %appareil,
                niveau,
                attente_ms,
                "dlna_didl_niveau_resonde_echouee"
            ),
            TransitionNiveauDidl::Inchange => {}
        }
        transition
    }

    /// Lecture brute réservée aux épreuves : la production n'a que
    /// [`Self::niveau_de_depart`].
    #[cfg(test)]
    pub(crate) fn niveau_courant(&self) -> u8 {
        self.niveau.load(Ordering::Relaxed)
    }

    /// Le délai courant, pour les épreuves du barème.
    #[cfg(test)]
    pub(crate) fn attente_courante_ms(&self) -> u64 {
        self.attente_ms.load(Ordering::Relaxed)
    }
}

/// Sur quelle URL de contrôle part une action SOAP. Résolue AU MOMENT de
/// l'envoi par [`DlnaOutput::url_de`] — c'est ce qui permet au rejeu d'après
/// redécouverte (#3829) de partir vers le nouveau port sans que l'appelant
/// ait rien à savoir.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoieSoap {
    AvTransport,
    RenderingControl,
    /// Sonos, zone groupée : même hôte, chemin `/GroupRenderingControl/`.
    GroupRenderingControl,
    /// `ConnectionManager`, replié sur `AVTransport` quand le descripteur ne
    /// l'annonçait pas.
    ConnectionManager,
}
/// Ce qu'un envoi SOAP a rendu, AVANT interprétation.
enum IssueSoap {
    /// Le renderer a répondu : statut HTTP et corps, bruts.
    Reponse {
        statut: reqwest::StatusCode,
        texte: String,
    },
    /// Aucune réponse exploitable.
    Echec {
        message: String,
        /// La dernière tentative a expiré (voir [`SOAP_TIMEOUT_PREFIX`]).
        timeout: bool,
        /// Le port n'écoute plus (`ECONNREFUSED`, `10061`) — motif de
        /// redécouverte (#3829).
        refus: bool,
        /// Les réessais ont été épuisés : on journalise `soap_all_retries_failed`.
        apres_reessais: bool,
    },
}
impl IssueSoap {
    /// Le motif qui JUSTIFIE une redécouverte, ou rien.
    ///
    /// Deux cas, et deux seulement : le refus de connexion (le port n'écoute
    /// plus) et le `404` sur l'URL de contrôle (le port écoute, mais plus ce
    /// chemin-là — même pile, autre arborescence). PAS le timeout : l'appareil
    /// est éteint, un `M-SEARCH` échouerait aussi et doublerait le délai. PAS
    /// la faute SOAP applicative (`701`, `714`…), qui voyage dans un `500`
    /// avec corps : l'appareil est joignable et dit autre chose.
    fn motif_de_redecouverte(&self) -> Option<&'static str> {
        match self {
            IssueSoap::Echec { refus: true, .. } => Some("refus de connexion"),
            IssueSoap::Reponse { statut, .. } if *statut == reqwest::StatusCode::NOT_FOUND => {
                Some("404 sur l'URL de contrôle")
            }
            _ => None,
        }
    }
}
/// Ce que Tune ACCOLE à l'erreur d'origine quand la redécouverte n'a rien
/// donné. Accolé, jamais préfixé : `SOAP_TIMEOUT_PREFIX` et
/// `SOAP_HTTP_SANS_CORPS_PREFIX` restent en tête, là où l'orchestrateur les
/// lit.
pub const MOTIF_REDECOUVERTE_ECHOUEE: &str =
    "le port de contrôle a changé ou l'appareil est injoignable";
/// Répit entre deux redécouvertes du MÊME appareil. Le sondeur à 1 Hz et la
/// lecture peuvent échouer sur le même port mort dans la même seconde : un
/// seul `M-SEARCH` part, les autres appels rejouent sur ce qu'il a rapporté,
/// ou rendent l'erreur enrichie s'il n'a rien rapporté.
const REDECOUVERTE_REPIT: std::time::Duration = std::time::Duration::from_secs(5);
/// Bornes et mémoire de la redécouverte ciblée d'une sortie (#3829).
struct Redecouverte {
    /// Port SSDP visé par le `M-SEARCH` unicast (1900 hors banc).
    port_ssdp: AtomicU64,
    /// Budget d'attente de la réponse, en ms.
    budget_ms: AtomicU64,
    /// Dernière tentative : quand, et si elle a abouti. Le verrou est tenu
    /// pendant tout le geste — c'est lui qui sérialise les appels concurrents.
    derniere: tokio::sync::Mutex<Option<(std::time::Instant, bool)>>,
}
impl Redecouverte {
    fn par_defaut() -> Self {
        Self {
            port_ssdp: AtomicU64::new(redecouverte::PORT_SSDP as u64),
            budget_ms: AtomicU64::new(redecouverte::BUDGET_REPONSE.as_millis() as u64),
            derniere: tokio::sync::Mutex::new(None),
        }
    }
}
/// La `LOCATION` présumée d'une sortie construite sans descriptif : la racine
/// de son URL de contrôle. Purement informative — la redécouverte relit la
/// vraie `LOCATION` dans la réponse `M-SEARCH`.
fn redecouverte_location_depuis(url_de_controle: &str) -> String {
    let hote = crate::discovery::ssdp::host_from_location(url_de_controle).unwrap_or_default();
    let port = crate::discovery::ssdp::port_from_location(url_de_controle);
    format!("http://{hote}:{port}/")
}
pub struct DlnaOutput {
    name: String,
    device_id: String,
    host: String,
    /// Les URLs de contrôle et d'évènements de CET appareil, telles qu'on les
    /// connaît MAINTENANT.
    ///
    /// Elles étaient quatre champs figés à la construction : ce que la
    /// découverte avait appris la première fois, Tune l'appelait pour la vie
    /// du processus. Or une pile Platinum tire un port au hasard à CHAQUE
    /// démarrage (#3829 : 1145 → 1838, même appareil, même UDN) — après un
    /// redémarrage du renderer, chaque action SOAP partait vers un port qui
    /// n'écoutait plus (`10061` sous Windows, `ECONNREFUSED` ailleurs), et
    /// seul un redémarrage de Tune s'en sortait. Elles sont maintenant lues
    /// AU MOMENT de l'envoi ([`DlnaOutput::url_de`]) et rafraîchies par la
    /// redécouverte ciblée ([`DlnaOutput::redecouvrir_les_urls`]).
    urls: std::sync::RwLock<UrlsDeControle>,
    /// Bornes et mémoire de la redécouverte ciblée (#3829).
    redecouverte: Redecouverte,
    client: Client,
    /// Short-timeout client used for fire-and-forget Stop before play.
    stop_client: Client,
    /// Pause between SetAVTransportURI and Play, in ms. Interior-mutable so a
    /// per-zone override (Settings → renderer panel) can be applied live to the
    /// already-registered output without rebuilding it. 0 = no delay.
    play_delay_ms: AtomicU64,
    /// Budget de la fenetre de reveil d'apres-`Play`, en ms
    /// ([`BUDGET_REVEIL_STANDBY`] par defaut).
    ///
    /// Champ et non constante parce que la fenetre dure une DEMI-MINUTE :
    /// eprouver le cycle complet — acquittement, `CurrentURI` vide, relances,
    /// abandon — depuis `play_media` coutait ce temps-la pour de vrai, et
    /// aucune epreuve ne le payait. Le seul ecrivain hors de la construction
    /// est [`DlnaOutput::with_budget_reveil_ms`] ; la production ne l'appelle
    /// nulle part et garde donc la constante, au millieme pres.
    budget_reveil_ms: AtomicU64,
    /// Compteur MONOTONE des `item id` DIDL émis vers CET appareil.
    ///
    /// Les renderers du genre Marantz ND8006 indexent les métadonnées DIDL
    /// qu'ils gardent en cache sur l'`item id` : réutiliser un id déjà vu leur
    /// fait réafficher les anciennes (durée, format, progression de la piste
    /// précédente).
    ///
    /// 🔴 Ce champ était un `AtomicBool` qui alternait « 1 » / « 2 »
    /// (`d53191bb`, 14/06/2026). Deux valeurs pour DEUX appelants —
    /// `play_media` (`SetAVTransportURI`) et `set_next_media`
    /// (`SetNextAVTransportURI`) — ne peuvent pas rester distinctes dans la
    /// durée : la piste 3 récupérait l'id de la piste 1, la piste 4 celui de
    /// la piste 2. La collision revenait donc avec une PÉRIODE DE DEUX, c'est
    /// à dire exactement le « une piste sur deux » que le correctif visait, et
    /// exactement ce que Jean Valjean décrit le lendemain de sa livraison
    /// (fil forum 631, 15/06/2026, #3675).
    ///
    /// Un compteur qui ne revient jamais en arrière n'a pas de période : deux
    /// émissions ne partagent plus jamais d'id sur la vie du processus.
    item_id_seq: AtomicU64,
    /// Niveau de DIDL appris pour CET appareil (0 = complet, 1 = minimal,
    /// 2 = vide). La pile Platinum de l'Eversolo ne lit qu'un segment TCP de
    /// requête : le DIDL complet déborde et finit en « 500 sans corps », le
    /// minimal passe — mais l'échelle re-payait l'aller-retour raté À CHAQUE
    /// piste (un warn + ~200 ms par SetURI/SetNext, constaté sur DMP-A8,
    /// #2394). Une fois le niveau qui passe constaté, on démarre là.
    ///
    /// 🔴 Ce champ était un `AtomicU8` nu, que rien ne rabaissait jamais : un
    /// seul « 500 sans corps » dégradait l'appareil pour la vie du processus.
    /// Il porte maintenant sa propre porte de sortie — voir
    /// [`NiveauDidlAppris`] (#3675).
    didl_niveau_appris: NiveauDidlAppris,
    /// Dernier état « coupé » que **Tune** a posé sur cet appareil, via
    /// `set_mute`.
    ///
    /// `get_status` le rend tel quel au lieu d'aller le redemander au
    /// renderer : le poller interroge chaque zone DLNA à 1 Hz pendant toute la
    /// lecture, et l'action SOAP `GetMute` y valait une requête sur quatre —
    /// pour une valeur que **personne ne lisait**. L'état coupé qu'affichent
    /// l'interface, la base (`zones.muted`) et les évènements est écrit
    /// uniquement par `Orchestrator::set_mute` ; `OutputStatus.muted` ne le
    /// nourrit nulle part (#2263).
    ///
    /// Même convention que les autres sorties sans évènements — AirPlay,
    /// SlimProto, Squeezebox tiennent déjà leur mute en local. Conséquence
    /// assumée : une coupure faite **sur l'appareil lui-même** (télécommande
    /// physique) n'est plus reflétée dans `GET /api/devices/{id}/status`,
    /// seule route qui expose ce champ.
    muted: AtomicBool,
    /// Micromega M-One uses a proprietary TCP protocol on port 7000 for volume.
    micromega_ip: Option<String>,
    /// Récepteur GENA partagé, `None` quand l'écoute n'a pas pu démarrer.
    /// Absent = comportement d'avant #2263, tout en sondage.
    event_listener: Option<Arc<UpnpEventListener>>,
    /// État poussé par le renderer. Partagé avec le récepteur.
    event_state: Arc<tokio::sync::Mutex<EventState>>,
    event_sub_ids: tokio::sync::Mutex<Vec<String>>,
    /// « Silence UPnP » : opt-in par zone. Coupe le dernier `GetPositionInfo`
    /// et fait donc tomber le trafic à ZÉRO action pendant la lecture, au prix
    /// d'une position EXTRAPOLÉE. Voir [`DlnaOutput::etat_evenements`].
    upnp_silence: AtomicBool,
    /// Ancre d'extrapolation de la position en mode « silence UPnP ».
    ancre_position: tokio::sync::Mutex<AncrePosition>,
    /// Depuis quand l'état poussé et la position mesurée se contredisent.
    ///
    /// Compté en HORLOGE MURALE, jamais en sondages : la cadence du sondeur
    /// n'appartient pas à cette couche, et un compteur de tours changerait de
    /// sens si elle bougeait.
    incoherence_depuis: tokio::sync::Mutex<Option<std::time::Instant>>,
    /// Dernière position mesurée en SOAP. `u64::MAX` = aucune mesure encore.
    derniere_position_ms: AtomicU64,
    /// Dernier volume que **Tune** a posé, en pour-cent. `u64::MAX` = jamais.
    /// Seule valeur disponible en mode silence si le renderer n'a jamais
    /// poussé de `Volume`.
    dernier_volume_pct: AtomicU64,
    /// La position rendue par le dernier `get_status` était-elle extrapolée ?
    /// Lu par `GET /api/devices/{id}/status` pour que l'estimation ne se fasse
    /// jamais passer pour une mesure.
    position_extrapolee: AtomicBool,
    /// Durée que TUNE connaît pour une URI donnée, depuis sa bibliothèque.
    ///
    /// Le mode silence n'a que l'évènement pour connaître la durée, et un
    /// renderer qui ne pousse pas `CurrentTrackDuration` la laisserait à zéro —
    /// une TROISIÈME dégradation, celle-là non annoncée. Or Tune connaît la
    /// durée avant même d'envoyer l'URI : il n'a aucune raison de la demander
    /// à l'appareil.
    ///
    /// **Appariée à son URI, jamais servie seule.** Une durée de la piste
    /// précédente appliquée à la suivante ferait pire que zéro : elle
    /// déclencherait la garde « position au-delà de la fin » du sondeur au
    /// milieu du morceau. Le couple n'est donc utilisé que si l'URI en cours
    /// est bien celle qu'il décrit.
    duree_annoncee: tokio::sync::Mutex<Option<(String, u64)>>,
}

/// Au bout de combien de temps de contradiction entre l'état poussé et la
/// position mesurée on va trancher en SOAP. Deux secondes : assez pour laisser
/// passer un tour de sondage où la position n'a pas eu le temps de bouger,
/// assez court pour que le sondeur ne bâtisse rien sur un état faux.
const INCOHERENCE_AVANT_ARBITRAGE: std::time::Duration = std::time::Duration::from_secs(2);

/// Ancre d'extrapolation de la position en mode « silence UPnP ».
#[derive(Debug, Clone)]
struct AncrePosition {
    position_ms: u64,
    instant: std::time::Instant,
    /// La lecture avançait-elle à l'instant de l'ancrage ? Replié à chaque
    /// changement d'état, pour qu'une pause de trois minutes ne se retrouve
    /// pas ajoutée à la position au moment de la reprise.
    avance: bool,
    /// URI ancrée. Un changement d'URI (piste suivante, gapless) remet la
    /// position à zéro : sans cela la deuxième piste démarrerait à la position
    /// finale de la première.
    uri: Option<String>,
}

impl Default for AncrePosition {
    fn default() -> Self {
        Self {
            position_ms: 0,
            instant: std::time::Instant::now(),
            avance: false,
            uri: None,
        }
    }
}

/// Ce que le chemin DLNA sait de ses évènements, à l'instant où on le demande.
///
/// Rendu tel quel par `GET /api/devices/{id}/status` : le mode « silence »
/// dégrade deux choses, et les taire derrière un interrupteur muet reviendrait
/// à laisser un client afficher une position estimée comme une position
/// mesurée.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct EtatEvenementsUpnp {
    /// Un abonnement GENA est tenu et le renderer a déjà poussé un état.
    pub abonne: bool,
    /// L'option « silence UPnP » est armée sur cette zone.
    pub silence: bool,
    /// La position rendue par le dernier `get_status` est une ESTIMATION
    /// (ancre + horloge murale), pas une mesure lue sur l'appareil.
    pub position_extrapolee: bool,
}

impl DlnaOutput {
    pub fn new(
        name: String,
        device_id: String,
        host: String,
        av_transport_url: String,
        rendering_control_url: String,
        connection_manager_url: Option<String>,
    ) -> Self {
        let micromega_ip = if name.to_lowercase().contains("micromega") {
            let ip = host
                .trim_start_matches("http://")
                .trim_start_matches("https://")
                .split(':')
                .next()
                .unwrap_or("")
                .to_string();
            if !ip.is_empty() {
                info!(device = %name, ip = %ip, "micromega_device_detected — proprietary volume on port 7000");
                Some(ip)
            } else {
                None
            }
        } else {
            None
        };
        let location = redecouverte_location_depuis(&av_transport_url);
        Self {
            name,
            device_id,
            host,
            urls: std::sync::RwLock::new(UrlsDeControle {
                location,
                av_transport: av_transport_url,
                rendering_control: rendering_control_url,
                connection_manager: connection_manager_url,
                event_sub_urls: HashMap::new(),
            }),
            redecouverte: Redecouverte::par_defaut(),
            client: crate::http::client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            stop_client: crate::http::client::builder()
                .timeout(std::time::Duration::from_millis(
                    STOP_BEFORE_PLAY_TIMEOUT_MS,
                ))
                .build()
                .unwrap_or_default(),
            play_delay_ms: AtomicU64::new(0),
            budget_reveil_ms: AtomicU64::new(BUDGET_REVEIL_STANDBY.as_millis() as u64),
            item_id_seq: AtomicU64::new(1),
            didl_niveau_appris: NiveauDidlAppris::neuf(),
            muted: AtomicBool::new(false),
            micromega_ip,
            event_listener: None,
            event_state: Arc::new(tokio::sync::Mutex::new(EventState::default())),
            event_sub_ids: tokio::sync::Mutex::new(Vec::new()),
            upnp_silence: AtomicBool::new(false),
            ancre_position: tokio::sync::Mutex::new(AncrePosition::default()),
            incoherence_depuis: tokio::sync::Mutex::new(None),
            derniere_position_ms: AtomicU64::new(u64::MAX),
            dernier_volume_pct: AtomicU64::new(u64::MAX),
            position_extrapolee: AtomicBool::new(false),
            duree_annoncee: tokio::sync::Mutex::new(None),
        }
    }

    pub fn with_play_delay(self, delay_ms: u64) -> Self {
        self.play_delay_ms.store(delay_ms, Ordering::Relaxed);
        self
    }
    /// Raccourcit la fenetre de reveil d'apres-`Play` (defaut :
    /// [`BUDGET_REVEIL_STANDBY`], 30 s).
    ///
    /// Existe pour qu'un banc puisse traverser le cycle ENTIER d'un ampli qui
    /// ne se reveille jamais — c'est le seul moyen de mesurer ce que Tune
    /// ANNONCE au bout, sans attendre une demi-minute par epreuve. Aucun
    /// appelant de production : la valeur par defaut est la constante.
    pub fn with_budget_reveil_ms(self, budget_ms: u64) -> Self {
        self.budget_reveil_ms.store(budget_ms, Ordering::Relaxed);
        self
    }

    /// Branche les évènements GENA sur cette sortie.
    ///
    /// `event_sub_urls` porte les `eventSubURL` du descripteur, DÉJÀ résolues
    /// en absolu — même règle que les `controlURL` : une radio Frontier
    /// Silicon (Ruark, Stream 94i) publie des URL absolues que concaténer à
    /// `host:port` rendrait injoignables.
    ///
    /// Sans appel à cette méthode, la sortie se comporte exactement comme
    /// avant #2263 : tout en sondage. C'est le repli, jamais une panne.
    pub fn with_upnp_events(
        mut self,
        listener: Option<Arc<UpnpEventListener>>,
        event_sub_urls: HashMap<String, String>,
    ) -> Self {
        self.event_listener = listener;
        self.urls
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .event_sub_urls = event_sub_urls;
        self
    }

    /// Règle le `M-SEARCH` de la redécouverte ciblée (#3829) : port SSDP visé
    /// et budget d'attente. Existe pour qu'un banc puisse tenir un faux
    /// répondeur SSDP sans privilège sur le port 1900. Aucun appelant de
    /// production : les défauts sont [`redecouverte::PORT_SSDP`] et
    /// [`redecouverte::BUDGET_REPONSE`].
    pub(crate) fn with_redecouverte(self, port_ssdp: u16, budget: std::time::Duration) -> Self {
        self.redecouverte
            .port_ssdp
            .store(port_ssdp as u64, Ordering::Relaxed);
        self.redecouverte
            .budget_ms
            .store(budget.as_millis() as u64, Ordering::Relaxed);
        self
    }

    /// L'URL de contrôle `AVTransport` courante — celle que le prochain envoi
    /// utilisera. Lecture brute pour les journaux et les bancs.
    pub fn url_av_transport(&self) -> String {
        self.url_de(VoieSoap::AvTransport)
    }

    /// L'URL de contrôle sur laquelle part une action, résolue À L'ENVOI.
    fn url_de(&self, voie: VoieSoap) -> String {
        let u = self.urls.read().unwrap_or_else(|e| e.into_inner());
        match voie {
            VoieSoap::AvTransport => u.av_transport.clone(),
            VoieSoap::RenderingControl => u.rendering_control.clone(),
            // Sonos refuse `RenderingControl` sur une zone groupée et ne
            // répond que sur `GroupRenderingControl`, même hôte.
            VoieSoap::GroupRenderingControl => u
                .rendering_control
                .replace("/RenderingControl/", "/GroupRenderingControl/"),
            VoieSoap::ConnectionManager => u
                .connection_manager
                .clone()
                .unwrap_or_else(|| u.av_transport.clone()),
        }
    }

    /// L'`eventSubURL` absolue d'un service, si le descripteur l'annonçait.
    fn url_evenements(&self, service: &str) -> Option<String> {
        self.urls
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .event_sub_urls
            .get(service)
            .filter(|u| !u.is_empty())
            .cloned()
    }

    /// #3829 — redécouvre l'appareil par son UDN et rafraîchit ses URLs.
    ///
    /// Appelée par [`DlnaOutput::soap_action`] sur un refus de connexion ou
    /// un `404` sur l'URL de contrôle, et SEULEMENT là : un timeout dit que
    /// l'appareil est éteint (la redécouverte échouerait aussi et doublerait
    /// le délai), une faute SOAP applicative (`701`, `714`…) dit que
    /// l'appareil est joignable et parle d'autre chose.
    ///
    /// Bornée : le verrou `derniere` couvre tout le geste, et une issue —
    /// réussie ou non — vaut pour [`REDECOUVERTE_REPIT`]. Le sondeur à 1 Hz
    /// et la lecture, tous deux en échec sur le même port mort, ne lancent
    /// donc qu'UN `M-SEARCH` ; le second appel rejoue simplement sur les
    /// URLs que le premier vient de rafraîchir.
    ///
    /// `Ok(())` veut dire « les URLs sont à jour, rejoue une fois » — y
    /// compris quand elles n'ont pas changé : l'appareil a répondu au
    /// `M-SEARCH`, il vaut donc un second essai. `Err` porte le motif que
    /// l'appelant ACCOLE à l'erreur d'origine, sans jamais la remplacer.
    async fn redecouvrir_les_urls(
        &self,
        url_en_echec: &str,
        motif: &'static str,
        action: &str,
    ) -> Result<(), String> {
        let mut derniere = self.redecouverte.derniere.lock().await;
        if let Some((quand, reussie)) = *derniere
            && quand.elapsed() < REDECOUVERTE_REPIT
        {
            if reussie {
                debug!(device = %self.name, action, "dlna_redecouverte_recente_rejeu_direct");
                return Ok(());
            }
            return Err(format!(
                "redécouverte déjà tentée il y a {} ms sans réponse",
                quand.elapsed().as_millis()
            ));
        }
        // L'adresse à interroger est celle de l'URL qui vient d'échouer :
        // c'est là que l'appareil était la dernière fois qu'on l'a vu.
        let ip = crate::discovery::ssdp::host_from_location(url_en_echec)
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| self.host.clone());
        let port_ssdp = self.redecouverte.port_ssdp.load(Ordering::Relaxed) as u16;
        let budget =
            std::time::Duration::from_millis(self.redecouverte.budget_ms.load(Ordering::Relaxed));
        info!(
            device = %self.name,
            id = %self.device_id,
            motif,
            action,
            url = url_en_echec,
            ip = %ip,
            "dlna_redecouverte_ciblee"
        );
        match redecouverte::redecouvrir(&ip, port_ssdp, &self.device_id, budget).await {
            Ok(nouvelles) => {
                let (ancienne, nouvelle) = {
                    let mut u = self.urls.write().unwrap_or_else(|e| e.into_inner());
                    let ancienne = u.av_transport.clone();
                    // Les `eventSubURL` suivent : un abonnement GENA posé sur
                    // l'ancien port ne recevrait plus rien.
                    *u = nouvelles;
                    (ancienne, u.av_transport.clone())
                };
                *derniere = Some((std::time::Instant::now(), true));
                if ancienne != nouvelle {
                    warn!(
                        device = %self.name,
                        id = %self.device_id,
                        ancienne = %ancienne,
                        nouvelle = %nouvelle,
                        "dlna_redecouverte_port_de_controle_change"
                    );
                } else {
                    info!(
                        device = %self.name,
                        id = %self.device_id,
                        url = %nouvelle,
                        "dlna_redecouverte_urls_inchangees_rejeu"
                    );
                }
                Ok(())
            }
            Err(raison) => {
                *derniere = Some((std::time::Instant::now(), false));
                warn!(
                    device = %self.name,
                    id = %self.device_id,
                    ip = %ip,
                    raison = %raison,
                    "dlna_redecouverte_echouee"
                );
                Err(raison)
            }
        }
    }

    /// Arme le « silence UPnP » à la construction (opt-in de zone relu au
    /// moment où la sortie est enregistrée). Même forme que
    /// [`DlnaOutput::with_play_delay`].
    pub fn with_upnp_silence(self, silence: bool) -> Self {
        self.upnp_silence.store(silence, Ordering::Relaxed);
        self
    }

    /// Arme ou désarme le « silence UPnP » sur une sortie DÉJÀ enregistrée
    /// (même forme que [`DlnaOutput::set_play_delay`], appelée depuis
    /// `PATCH /zones/{id}` par abaissement de type).
    pub fn set_upnp_silence(&self, silence: bool) {
        self.upnp_silence.store(silence, Ordering::Relaxed);
    }

    /// Opt-in « silence UPnP » armé pour cette sortie.
    pub fn upnp_silence(&self) -> bool {
        self.upnp_silence.load(Ordering::Relaxed)
    }

    /// État des évènements, pour l'exposer au client (voir
    /// [`EtatEvenementsUpnp`]).
    pub async fn etat_evenements(&self) -> EtatEvenementsUpnp {
        EtatEvenementsUpnp {
            abonne: self.event_state.lock().await.is_live(),
            silence: self.upnp_silence.load(Ordering::Relaxed),
            position_extrapolee: self.position_extrapolee.load(Ordering::Relaxed),
        }
    }

    /// La sortie peut-elle s'abonner ? (récepteur présent ET au moins
    /// l'`eventSubURL` d'AVTransport annoncée par le descripteur).
    pub fn peut_s_abonner(&self) -> bool {
        self.event_listener.is_some() && self.url_evenements("avtransport").is_some()
    }

    /// S'abonne à `AVTransport` (état du transport, piste, durée) et à
    /// `RenderingControl` (volume, coupure).
    ///
    /// Même patron que `OpenHomeOutput::subscribe_events` : on s'abonne au
    /// moment du `play_media`, on se désabonne au `stop`. Le renderer n'a rien
    /// à pousser tant qu'il ne joue pas, et l'abonnement au repos coûterait un
    /// renouvellement toutes les 250 s pour rien.
    async fn subscribe_events(&self) {
        let Some(listener) = &self.event_listener else {
            return;
        };
        // Repartir d'un état vierge : les valeurs de la piste précédente ne
        // doivent pas passer pour l'actualité de la nouvelle le temps que le
        // premier NOTIFY arrive.
        *self.event_state.lock().await = EventState::default();
        *self.incoherence_depuis.lock().await = None;
        self.derniere_position_ms.store(u64::MAX, Ordering::Relaxed);

        let mut sub_ids = self.event_sub_ids.lock().await;
        let mut count = 0u32;
        for svc in ["avtransport", "renderingcontrol"] {
            if let Some(url) = self.url_evenements(svc)
                && let Some(path_id) = listener.subscribe(&url, self.event_state.clone()).await
            {
                sub_ids.push(path_id);
                count += 1;
            }
        }

        if count > 0 {
            info!(
                device = %self.name,
                count,
                silence = self.upnp_silence.load(Ordering::Relaxed),
                "dlna_events_subscribed"
            );
        } else {
            debug!(device = %self.name, "dlna_events_indisponibles_sondage");
        }
    }

    async fn unsubscribe_events(&self) {
        let Some(listener) = &self.event_listener else {
            return;
        };
        let mut sub_ids = self.event_sub_ids.lock().await;
        for path_id in sub_ids.drain(..) {
            listener.unsubscribe(&path_id).await;
        }
        self.event_state.lock().await.alive = false;
    }

    /// Repose l'ancre d'extrapolation sur une position CONNUE.
    async fn ancrer_position(&self, position_ms: u64, avance: bool, uri: Option<String>) {
        *self.ancre_position.lock().await = AncrePosition {
            position_ms,
            instant: std::time::Instant::now(),
            avance,
            uri,
        };
    }

    /// Retient la durée que Tune connaît pour cette URI (voir
    /// [`DlnaOutput::duree_annoncee`]).
    async fn annoncer_duree(&self, url: &str, duration_ms: Option<u64>) {
        if let Some(d) = duration_ms.filter(|d| *d > 0) {
            *self.duree_annoncee.lock().await = Some((url.to_string(), d));
        }
    }

    /// Durée connue pour l'URI en cours, si c'est bien celle qu'on a annoncée.
    async fn duree_connue_pour(&self, uri: Option<&String>) -> Option<u64> {
        let annoncee = self.duree_annoncee.lock().await;
        let (url, d) = annoncee.as_ref()?;
        match uri {
            Some(u) if u == url => Some(*d),
            // Sans URI en cours, on ne peut pas apparier : on préfère ne rien
            // dire à dire la durée d'une autre piste.
            _ => None,
        }
    }

    /// Position extrapolée du mode « silence », et entretien de l'ancre.
    ///
    /// Trois recalages, dans cet ordre :
    /// 1. le renderer a poussé une `RelativeTimePosition` → elle prime, c'est
    ///    une mesure ;
    /// 2. l'URI a changé → nouvelle piste, on repart de zéro ;
    /// 3. l'état a basculé (lecture ⇄ pause/arrêt) → on replie le temps déjà
    ///    couru dans l'ancre avant de changer de régime.
    async fn extrapoler_position(
        &self,
        etat: TransportState,
        uri: Option<&String>,
        position_poussee: Option<(u64, std::time::Instant)>,
    ) -> u64 {
        let avance = etat == TransportState::Playing;
        let mut ancre = self.ancre_position.lock().await;

        if let Some((p, at)) = position_poussee
            && at >= ancre.instant
        {
            *ancre = AncrePosition {
                position_ms: p,
                instant: at,
                avance,
                uri: uri.cloned(),
            };
        } else if uri.is_some() && ancre.uri.as_ref() != uri {
            *ancre = AncrePosition {
                position_ms: 0,
                instant: std::time::Instant::now(),
                avance,
                uri: uri.cloned(),
            };
        } else if ancre.avance != avance {
            let couru = if ancre.avance {
                ancre.instant.elapsed().as_millis() as u64
            } else {
                0
            };
            *ancre = AncrePosition {
                position_ms: ancre.position_ms + couru,
                instant: std::time::Instant::now(),
                avance,
                uri: ancre.uri.clone(),
            };
        }

        if ancre.avance {
            ancre.position_ms + ancre.instant.elapsed().as_millis() as u64
        } else {
            ancre.position_ms
        }
    }

    /// Retient le volume que Tune vient de poser, sur les TROIS voies qui
    /// peuvent aboutir (Micromega en TCP propriétaire, Sonos en
    /// `GroupRenderingControl`, `RenderingControl` pour tout le monde).
    ///
    /// Sert deux choses : ne pas laisser l'état poussé en retard d'un
    /// évènement, et donner au mode silence une valeur honnête quand le
    /// renderer n'émet pas de `Volume`.
    async fn memoriser_volume(&self, niveau_pct: u64) {
        let niveau_pct = niveau_pct.min(100);
        self.dernier_volume_pct.store(niveau_pct, Ordering::Relaxed);
        self.event_state.lock().await.volume = Some(niveau_pct as u32);
    }

    /// Lit le volume du renderer, sur la voie qu'il faut.
    ///
    /// Sonos refuse `RenderingControl::GetVolume` sur une zone groupée et ne
    /// répond que sur `GroupRenderingControl` : la distinction existait déjà
    /// dans `get_status`, elle est ici pour que les DEUX régimes de lecture
    /// l'appliquent, pas seulement celui qui sonde.
    async fn lire_volume(&self) -> Result<f64, String> {
        let volume_resp = if self.device_id.contains("RINCON") {
            self.soap_action(
                VoieSoap::GroupRenderingControl,
                "urn:schemas-upnp-org:service:GroupRenderingControl:1",
                "GetGroupVolume",
                "<InstanceID>0</InstanceID>",
            )
            .await
            .unwrap_or_default()
        } else {
            self.rc_action(
                "GetVolume",
                "<InstanceID>0</InstanceID><Channel>Master</Channel>",
            )
            .await?
        };
        Ok(extract_tag(&volume_resp, "CurrentVolume")
            .and_then(|v| v.parse::<f64>().ok())
            .map(|v| v / 100.0)
            .unwrap_or(0.5))
    }

    /// L'état poussé contredit-il la position mesurée ?
    ///
    /// Un renderer peut accepter un abonnement et cesser d'émettre : l'état
    /// gelé passerait alors pour l'actualité. Les deux sens comptent, et c'est
    /// le point : un `Playing` figé retient la file pour toujours, un `Stopped`
    /// de trop la fait sauter une piste. La contradiction n'est retenue que si
    /// elle DURE — un tour où la position n'a pas eu le temps de bouger n'est
    /// pas une preuve.
    fn etat_contredit_la_position(etat: TransportState, position_a_bouge: bool) -> bool {
        match etat {
            TransportState::Playing => !position_a_bouge,
            TransportState::Stopped | TransportState::Paused => position_a_bouge,
            // Transitoire par nature : la position peut aussi bien être figée
            // (chargement) que sauter (nouvelle piste). On ne conclut rien.
            TransportState::Transitioning => false,
        }
    }

    /// Update the SetAVTransportURI→Play delay on an already-registered output
    /// (via &self downcast in the zone PATCH handler). Takes effect on the next
    /// play; no rebuild needed.
    pub fn set_play_delay(&self, delay_ms: u64) {
        self.play_delay_ms.store(delay_ms, Ordering::Relaxed);
    }

    /// Current SetAVTransportURI→Play delay in ms.
    pub fn play_delay_ms(&self) -> u64 {
        self.play_delay_ms.load(Ordering::Relaxed)
    }

    /// Send a SOAP action without retries and with the short-timeout client.
    /// Used for the fire-and-forget Stop before play — we don't need to wait
    /// for the response because SetAVTransportURI implicitly replaces the
    /// current track.  Returns immediately after the single attempt.
    async fn soap_action_fast(
        &self,
        url: &str,
        service: &str,
        action: &str,
        body: &str,
    ) -> Result<(), String> {
        let soap = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:{action} xmlns:u="{service}">
      {body}
    </u:{action}>
  </s:Body>
</s:Envelope>"#
        );
        let soap_action = format!("{service}#{action}");

        match self
            .stop_client
            .post(url)
            .header("Content-Type", "text/xml; charset=utf-8")
            .header("SOAPAction", format!("\"{soap_action}\""))
            .body(soap)
            .send()
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => Err(format!("soap_fast: {}", http_error::chain(&e))),
        }
    }

    /// Une action SOAP vers `voie`, avec réessais, et — sur un refus de
    /// connexion ou un `404` de l'URL de contrôle — UNE redécouverte ciblée
    /// suivie d'UN rejeu (#3829). Jamais de boucle : le rejeu ne redécouvre
    /// pas.
    async fn soap_action(
        &self,
        voie: VoieSoap,
        service: &str,
        action: &str,
        body: &str,
    ) -> Result<String, String> {
        let soap = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:{action} xmlns:u="{service}">
      {body}
    </u:{action}>
  </s:Body>
</s:Envelope>"#
        );
        let soap_action = format!("{service}#{action}");
        let url = self.url_de(voie);
        let issue = self.envoyer_soap(&url, &soap_action, &soap, action).await;
        let Some(motif) = issue.motif_de_redecouverte() else {
            return self.conclure(issue, action);
        };
        match self.redecouvrir_les_urls(&url, motif, action).await {
            Ok(()) => {
                let url = self.url_de(voie);
                debug!(device = %self.name, action, url = %url, "dlna_redecouverte_rejeu");
                let rejeu = self.envoyer_soap(&url, &soap_action, &soap, action).await;
                self.conclure(rejeu, action)
            }
            // L'erreur d'ORIGINE, enrichie — pas une erreur neuve qui
            // masquerait le `10061` : c'est lui la première information.
            Err(raison) => self
                .conclure(issue, action)
                .map_err(|m| format!("{m} — {MOTIF_REDECOUVERTE_ECHOUEE} ({raison})")),
        }
    }
    /// L'envoi proprement dit, avec ses réessais. Ne juge pas la réponse :
    /// c'est [`DlnaOutput::conclure`] qui le fait, pour que le rejeu d'après
    /// redécouverte soit interprété exactement comme le premier envoi.
    async fn envoyer_soap(
        &self,
        url: &str,
        soap_action: &str,
        soap: &str,
        action: &str,
    ) -> IssueSoap {
        let mut last_err = String::new();
        let mut last_was_timeout = false;
        for attempt in 0..=SOAP_MAX_RETRIES {
            if attempt > 0 {
                let delay = 200 * (1 << (attempt - 1));
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                debug!(device = %self.name, action, attempt, "soap_retry");
            }
            match self
                .client
                .post(url)
                .header("Content-Type", "text/xml; charset=utf-8")
                .header("SOAPAction", format!("\"{soap_action}\""))
                .body(soap.to_string())
                .send()
                .await
            {
                Ok(resp) => {
                    let statut = resp.status();
                    match resp.text().await {
                        Ok(texte) => return IssueSoap::Reponse { statut, texte },
                        Err(e) => last_err = format!("soap read: {}", http_error::chain(&e)),
                    }
                }
                // #3829 — un refus de connexion est la réponse DÉFINITIVE du
                // noyau distant : le port n'écoute pas. Le réessayer 200 ms
                // plus tard ne change rien ; on rend la main tout de suite
                // pour que la redécouverte ait lieu, et c'est ELLE qui rejoue.
                // Reconnu au `kind()` de l'`io::Error`, jamais au texte :
                // celui-ci est localisé (« Aucune connexion n'a pu être
                // établie… (os error 10061) » sous Windows).
                Err(e) if http_error::is_connection_refused(&e) => {
                    return IssueSoap::Echec {
                        message: format!("soap send: {}", http_error::chain(&e)),
                        timeout: false,
                        refus: true,
                        apres_reessais: attempt > 0,
                    };
                }
                // `is_connection_closed_early` : le renderer a raccroché avant
                // d'avoir fini sa réponse. Sans ce troisième prédicat, la panne
                // ressortait par le bras « erreur définitive » ci-dessous et
                // n'était JAMAIS réessayée — le Marantz ND8006 de Jean Valjean
                // échouait dès la première tentative (#1984), y compris sur le
                // GetProtocolInfo qui arme le bouton « 24 bits ».
                //
                // La deuxième tentative repart sur une connexion neuve : celle
                // qui vient d'échouer a été évacuée du pool par l'échec même.
                // C'est ce qui rend le simple réessai suffisant, sans avoir à
                // désactiver la mutualisation vers tous les renderers.
                Err(e)
                    if e.is_connect()
                        || e.is_timeout()
                        || http_error::is_connection_closed_early(&e) =>
                {
                    last_was_timeout = e.is_timeout();
                    last_err = format!("soap send: {}", http_error::chain(&e));
                }
                Err(e) => {
                    return IssueSoap::Echec {
                        message: format!("soap send: {}", http_error::chain(&e)),
                        timeout: false,
                        refus: false,
                        apres_reessais: false,
                    };
                }
            }
        }
        IssueSoap::Echec {
            message: last_err,
            timeout: last_was_timeout,
            refus: false,
            apres_reessais: true,
        }
    }
    /// Interprète une issue d'envoi exactement comme avant #3829 : succès,
    /// « statut d'échec sans corps », timeout préfixé, ou l'erreur telle
    /// quelle.
    fn conclure(&self, issue: IssueSoap, action: &str) -> Result<String, String> {
        match issue {
            IssueSoap::Reponse { statut, texte } => {
                // Statut d'échec + corps vide : le renderer n'a pas lu la
                // requête (voir SOAP_HTTP_SANS_CORPS_PREFIX). Un échec AVEC
                // corps reste rendu tel quel — c'est un défaut SOAP que
                // l'appelant sait interpréter.
                if !statut.is_success() && texte.trim().is_empty() {
                    return Err(format!(
                        "{SOAP_HTTP_SANS_CORPS_PREFIX} {statut} sur {action}"
                    ));
                }
                Ok(texte)
            }
            IssueSoap::Echec {
                message,
                timeout,
                refus,
                apres_reessais,
            } => {
                if apres_reessais || refus {
                    http_error::hint_if_local_network_denied(&message);
                    warn!(device = %self.name, action, error = %message, "soap_all_retries_failed");
                }
                // Voir SOAP_TIMEOUT_PREFIX : un timeout laisse la commande
                // peut-être exécutée, un refus de connexion non.
                if timeout {
                    Err(format!("{SOAP_TIMEOUT_PREFIX} {message}"))
                } else {
                    Err(message)
                }
            }
        }
    }
    async fn av_action(&self, action: &str, body: &str) -> Result<String, String> {
        self.soap_action(VoieSoap::AvTransport, AV_TRANSPORT_URN, action, body)
            .await
    }
    async fn rc_action(&self, action: &str, body: &str) -> Result<String, String> {
        self.soap_action(
            VoieSoap::RenderingControl,
            RENDERING_CONTROL_URN,
            action,
            body,
        )
        .await
    }

    /// Réenvoie `SetAVTransportURI` au niveau de DIDL qui a fini par passer
    /// pour cet appareil : rejouer le complet referait échouer la lecture chez
    /// Platinum. Deux appelants — le réarmement d'un 701 sans média (#2581) et
    /// la relance d'un Play acquitté mais jamais appliqué.
    async fn reposer_uri(
        &self,
        media: &PlayMedia<'_>,
        item_id: &str,
        mime: &str,
        niveau_didl: u8,
    ) -> Result<String, String> {
        let metadata = match niveau_didl {
            0 => Self::didl_metadata_mime(media, item_id, mime),
            1 => Self::didl_metadata_minimale(media, item_id, mime),
            _ => String::new(),
        };
        self.av_action(
            "SetAVTransportURI",
            &format!(
                "<InstanceID>0</InstanceID><CurrentURI>{}</CurrentURI><CurrentURIMetaData>{metadata}</CurrentURIMetaData>",
                media.url
            ),
        )
        .await
    }

    /// Le battement que CET appareil a déclaré nécessaire entre un
    /// `SetAVTransportURI` et le `Play` qui le suit — réglage `dlna_play_delay_ms`
    /// (panneau renderer, `[device_delays]`, catalogue d'appareils : 800 ms pour
    /// un Yamaha R-N2000A).
    ///
    /// Il n'était tenu que sur la PREMIÈRE pose d'URI. Les deux reprises qui en
    /// reposent une — le réarmement d'un 701 (#2581) et la relance d'un `Play`
    /// jamais appliqué — enchaînaient le `Play` sans lui, c'est-à-dire dans la
    /// fenêtre exacte que ce réglage existe pour éviter, et sur les deux chemins
    /// qui ne s'exécutent QUE lorsque l'appareil est déjà en train de refuser.
    /// À 0 — le défaut de tout le monde — cette fonction ne fait rien.
    /// Ce que le renderer dit TENIR — sur les **deux** champs que son
    /// AVTransport publie, pas seulement sur le premier (#3580).
    ///
    /// `GetMediaInfo` → `CurrentURI` est le champ nominal, et c'est le seul
    /// que Tune lisait. Mais l'AVTransport en publie un second,
    /// `GetPositionInfo` → `TrackURI`, et rien dans la specification n'oblige
    /// un renderer a renseigner les deux au meme instant : un appareil qui
    /// traite `SetAVTransportURI` comme le CHARGEMENT D'UNE PISTE peut ne
    /// remplir que le second. Vu de Tune, un tel appareil « ne tient AUCUN
    /// media » pour toujours — verdict [`UriVerdict::PasEncore`], zone coupee,
    /// message d'echec — alors que le protocole nomme NOTRE flux deux octets
    /// plus loin. C'est la seule hypothese de #3580 que le dossier nommait
    /// sans pouvoir l'eprouver ; elle ne coute rien a fermer.
    ///
    /// **Le cas nominal ne paie rien.** Le second champ n'est demande que si le
    /// premier est vide, c'est-a-dire uniquement sur le chemin qui allait de
    /// toute facon echouer. Une lecture qui demarre garde exactement une action
    /// SOAP par relecture, comme avant.
    ///
    /// **Temoin POSITIF seulement, donc regression impossible.** `TrackURI`
    /// n'est retenu que s'il designe NOTRE flux (verdict
    /// [`UriVerdict::Appliquee`]). Un `TrackURI` vide, etranger, ou perime ne
    /// change rien : on rend ce que `CurrentURI` disait, au mot pres. Le seul
    /// verdict que cette lecture peut deplacer est « echec » → « succes », et
    /// seulement quand l'appareil a NOMME l'URL qu'on vient de lui poser.
    ///
    /// Un renderer sans `GetPositionInfo` garde lui aussi l'ancienne conduite :
    /// son refus est avale, pas propage — c'est `GetMediaInfo` seul qui decide
    /// du silence SOAP (`soap_muet`), et lui seul.
    async fn uri_tenue_par_le_renderer(
        &self,
        url_attendue: &str,
    ) -> Result<Option<String>, String> {
        let courante = self
            .av_action("GetMediaInfo", "<InstanceID>0</InstanceID>")
            .await
            .map(|xml| extract_tag(&xml, "CurrentURI"))?;
        if courante.as_deref().is_some_and(|u| !u.trim().is_empty()) {
            return Ok(courante);
        }
        let Ok(xml) = self
            .av_action("GetPositionInfo", "<InstanceID>0</InstanceID>")
            .await
        else {
            return Ok(courante);
        };
        let piste = extract_tag(&xml, "TrackURI");
        if verdict_uri_appliquee(piste.as_deref(), url_attendue) == UriVerdict::Appliquee {
            info!(
                device = %self.name,
                track_uri = piste.as_deref().unwrap_or("-"),
                "dlna_uri_tenue_lue_dans_trackuri"
            );
            return Ok(piste);
        }
        Ok(courante)
    }

    async fn attendre_apres_set_uri(&self) {
        let delai = self.play_delay_ms.load(Ordering::Relaxed);
        if delai > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delai)).await;
        }
    }

    fn didl_metadata(media: &PlayMedia<'_>, item_id: &str) -> String {
        Self::didl_metadata_mime(media, item_id, media.mime_type)
    }

    /// Like [`Self::didl_metadata`] but announces an explicit `mime` instead of
    /// `media.mime_type`. Used to align the announced MIME with the renderer's
    /// GetProtocolInfo Sink spelling (Beoplay A9 / Sink audio/x-flac, forum
    /// 714) and for the 714 PCM fallback.
    fn didl_metadata_mime(media: &PlayMedia<'_>, item_id: &str, mime: &str) -> String {
        let is_dsd = mime.contains("dsd") || mime.contains("dsf");
        DidlBuilder::new(media.title.unwrap_or("Unknown"), media.url, mime)
            .protocol_style(ProtocolStyle::Dlna)
            .live_stream(media.live_stream)
            .byte_seekable(media.byte_seekable)
            .dlna_art_profile(true)
            .include_upnp_artist(true)
            .item_id(item_id)
            .artist_opt(media.artist)
            .album_opt(media.album)
            .album_art_opt(media.cover_url)
            .duration_ms_opt(media.duration_ms)
            .file_size_opt(media.file_size)
            .sample_rate_opt(if is_dsd { None } else { media.sample_rate })
            .bit_depth_opt(if is_dsd { None } else { media.bit_depth })
            .channels_opt(if is_dsd { None } else { media.channels })
            .build_escaped()
    }

    /// DIDL réduit au strict jouable : titre, ressource, protocolInfo, durée.
    ///
    /// Ni artiste, ni album, ni pochette : la pile Platinum/1.0.5.13 de
    /// l'Eversolo ne lit qu'un segment TCP de requête — un DIDL complet
    /// (~1,9 Ko d'enveloppe) déborde et finit en `500 Error Parsing XML Body`,
    /// quand les mêmes octets passent en une seule trame. Ce DIDL-ci tient
    /// l'enveloppe sous un segment. Le protocolInfo reste : sans lui, le
    /// DMP-A8 accepte l'URI d'un `.dsf` mais ne vient jamais le chercher.
    ///
    /// Le protocolInfo doit être JUSTE, pas seulement présent. Le profil
    /// `DLNA.ORG_PN=LPCM` n'est défini que pour 16 bits (#1137) et pour 44,1 /
    /// 48 kHz (#1458) : l'annoncer sur un WAV 24 bits ou hi-res fait rabattre
    /// le flux sur le profil déclaré, lire des échantillons désalignés, et
    /// jouer du SILENCE. Or ce DIDL-ci ne transmettait NI la profondeur NI la
    /// fréquence : `dlna_flags_for_mime_bd_sr(mime, None, None)` retombait donc
    /// sur `PN=LPCM` quoi qu'il arrive, et les deux correctifs restaient sans
    /// effet dès que l'appareil avait appris le niveau réduit — c'est-à-dire à
    /// chaque piste, définitivement (`didl_niveau_appris`, #2394).
    ///
    /// `sans_attributs_audio` rend les valeurs au calcul du profil sans écrire
    /// `sampleFrequency` / `bitsPerSample` dans `<res>` : le budget d'un
    /// segment TCP est préservé — la variante sans `PN` est même plus courte.
    fn didl_metadata_minimale(media: &PlayMedia<'_>, item_id: &str, mime: &str) -> String {
        let is_dsd = mime.contains("dsd") || mime.contains("dsf");
        DidlBuilder::new(media.title.unwrap_or("Unknown"), media.url, mime)
            .protocol_style(ProtocolStyle::Dlna)
            .live_stream(media.live_stream)
            .byte_seekable(media.byte_seekable)
            .item_id(item_id)
            .duration_ms_opt(media.duration_ms)
            .sample_rate_opt(if is_dsd { None } else { media.sample_rate })
            .bit_depth_opt(if is_dsd { None } else { media.bit_depth })
            .sans_attributs_audio()
            .build_escaped()
    }

    /// Accès de test aux deux niveaux de DIDL (budget de taille mesuré dans
    /// `dlna_test.rs` — l'échelle ne vaut que si le minimal tient un segment).
    #[cfg(test)]
    pub(crate) fn didl_metadata_pour_test(
        media: &PlayMedia<'_>,
        item_id: &str,
        mime: &str,
    ) -> String {
        Self::didl_metadata_mime(media, item_id, mime)
    }

    #[cfg(test)]
    pub(crate) fn didl_metadata_minimale_pour_test(
        media: &PlayMedia<'_>,
        item_id: &str,
        mime: &str,
    ) -> String {
        Self::didl_metadata_minimale(media, item_id, mime)
    }

    /// Rend un `item id` DIDL JAMAIS DÉJÀ ÉMIS vers cet appareil, et avance le
    /// compteur.
    ///
    /// Deux appelants seulement, et ils se suivent piste après piste :
    /// [`DlnaOutput::play_media`] (`SetAVTransportURI`) et
    /// [`DlnaOutput::set_next_media`] (`SetNextAVTransportURI`). Voir
    /// [`DlnaOutput::item_id_seq`] pour la raison du compteur monotone : une
    /// alternance à deux valeurs collisionnait une fois sur deux (#3675).
    fn next_item_id(&self) -> String {
        self.item_id_seq.fetch_add(1, Ordering::Relaxed).to_string()
    }

    fn parse_time(time_str: &str) -> u64 {
        parse_upnp_time(time_str)
    }

    fn format_time(ms: u64) -> String {
        let total_secs = ms / 1000;
        let h = total_secs / 3600;
        let m = (total_secs % 3600) / 60;
        let s = total_secs % 60;
        format!("{h}:{m:02}:{s:02}")
    }
}

#[async_trait::async_trait]
impl OutputTarget for DlnaOutput {
    fn name(&self) -> &str {
        &self.name
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn output_type(&self) -> &str {
        "dlna"
    }

    fn capabilities(&self) -> OutputCapabilities {
        OutputCapabilities::v1(true, true, true, true, true, true).with_percent_volume()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn host(&self) -> Option<&str> {
        Some(&self.host)
    }

    async fn play_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        // Les abonnements de la piste précédente d'abord : sans ce retrait,
        // chaque lecture en empilerait deux de plus dans le récepteur, tous
        // renouvelés toutes les 250 s pour un flux mort.
        self.unsubscribe_events().await;
        // Fire-and-forget Stop with a tight deadline: give the renderer up to
        // 500ms to acknowledge Stop, then proceed regardless.  Most renderers
        // accept SetAVTransportURI while playing (implicit stop), but we still
        // send Stop for renderers like DMP-A8 that need it.  The short deadline
        // ensures we don't block 2-10s waiting for a slow SOAP response.
        let url_stop = self.url_de(VoieSoap::AvTransport);
        let stop_fut = self.soap_action_fast(
            &url_stop,
            AV_TRANSPORT_URN,
            "Stop",
            "<InstanceID>0</InstanceID>",
        );
        tokio::select! {
            result = stop_fut => {
                match result {
                    Ok(()) => debug!(device = %self.name, "dlna_play_pre_stop_ok"),
                    Err(e) => debug!(device = %self.name, error = %e, "dlna_play_pre_stop_ignored"),
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                debug!(device = %self.name, "dlna_play_pre_stop_timeout_proceeding");
            }
        }

        // Un Stop ACQUITTÉ n'est pas un Stop APPLIQUÉ. L'Eversolo répond OK
        // puis met ~1-2 s à s'arrêter ; un SetAVTransportURI envoyé 5 ms plus
        // tard est acquitté… et ignoré — il continue son flux précédent (la
        // course des 5 ms, .42, 24/08 ; la même séquence espacée de 2 s est
        // acceptée). On attend l'arrêt réel, borné à ~2 s, et on continue quoi
        // qu'il arrive : c'est une politesse, jamais une barrière. Le renderer
        // déjà arrêté — le cas nominal — coûte UN GetTransportInfo.
        for attente in 0..8u32 {
            match self
                .av_action("GetTransportInfo", "<InstanceID>0</InstanceID>")
                .await
            {
                Ok(resp) if arret_effectif(&resp) => {
                    if attente > 0 {
                        debug!(device = %self.name, polls = attente + 1, "dlna_pre_stop_arret_confirme");
                    }
                    break;
                }
                // Un renderer sans GetTransportInfo ne doit rien bloquer.
                Err(_) => break,
                Ok(_) if attente == 7 => {
                    warn!(device = %self.name, "dlna_pre_stop_jamais_applique_on_continue");
                }
                Ok(_) => {
                    // À mi-parcours, escalader : l'Eversolo coincé en
                    // TRANSITIONING (flux mort qu'il ressasse) ACQUITTE les
                    // Stop sans les exécuter — seul Pause→Stop le libère
                    // (constaté par SOAP direct sur le DMP-A8, 25/08 : Stop →
                    // toujours PLAYING ; Pause → PAUSED_PLAYBACK ; Stop →
                    // STOPPED).
                    if attente == 3 {
                        debug!(device = %self.name, "dlna_pre_stop_escalade_pause_puis_stop");
                        let _ = self.av_action("Pause", "<InstanceID>0</InstanceID>").await;
                        let _ = self.av_action("Stop", "<InstanceID>0</InstanceID>").await;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            }
        }

        // Un id neuf pour CETTE piste : l'ancienne alternance à deux valeurs
        // rendait la piste 3 identique à la piste 1 (#3675).
        let item_id_courant = self.next_item_id();
        let item_id = item_id_courant.as_str();

        // First attempt: announce `media.mime_type` UNCHANGED — exactly the
        // previous behaviour. The Sink is NOT probed here: a healthy renderer
        // (Sonos & co) accepts this MIME, so the happy path does ZERO extra
        // GetProtocolInfo round-trip (no latency added in nominal playback).
        // The Sink is probed ONLY when a 714 actually occurs (see below).
        let mut attempt_mime = media.mime_type.to_string();
        // Sink probed lazily on the first 714 and reused across the ≤2 retries.
        let mut sink: Vec<String> = Vec::new();
        let mut tried_exact = false;
        let mut tried_fallback = false;
        // Échelle de métadonnées : DIDL complet → minimal → vide. On ne
        // descend que sur un échec de LECTURE de la requête (500 sans corps,
        // Platinum) — jamais sur un défaut SOAP, qui a sa propre reprise 714.
        // On démarre au niveau APPRIS pour cet appareil : re-payer l'échec du
        // complet à chaque piste coûtait un aller-retour et un warn par SetURI
        // (DMP-A8, #2394) pour finir au même DIDL minimal de toute façon.
        // Mais l'apprentissage EXPIRE : un appareil qui a hoqueté une fois est
        // remis à l'épreuve après un délai qui double (#3675). Le même
        // horodatage sert au départ et à l'apprentissage de cette émission.
        let maintenant_ms = horloge_process_ms();
        let mut niveau_didl: u8 = self
            .didl_niveau_appris
            .niveau_de_depart(maintenant_ms, &self.name);
        let debut_set_uri = std::time::Instant::now();
        loop {
            let metadata = match niveau_didl {
                0 => Self::didl_metadata_mime(media, item_id, &attempt_mime),
                1 => Self::didl_metadata_minimale(media, item_id, &attempt_mime),
                _ => String::new(),
            };
            let set_uri_resp = match self.av_action("SetAVTransportURI", &format!(
                "<InstanceID>0</InstanceID><CurrentURI>{}</CurrentURI><CurrentURIMetaData>{metadata}</CurrentURIMetaData>",
                media.url
            )).await {
                Ok(r) => r,
                Err(e) if e.starts_with(SOAP_HTTP_SANS_CORPS_PREFIX) && niveau_didl < 2 => {
                    niveau_didl += 1;
                    warn!(
                        device = %self.name,
                        ctrl = %self.url_av_transport(),
                        niveau = niveau_didl,
                        error = %e,
                        "dlna_set_uri_corps_illisible_didl_reduit"
                    );
                    continue;
                }
                Err(e) => return Err(e),
            };

            if !(set_uri_resp.contains("UPnPError") || set_uri_resp.contains("<errorCode>")) {
                self.didl_niveau_appris
                    .apprendre(niveau_didl, maintenant_ms, &self.name);
                // Le SUCCÈS se journalise, pas seulement l'échec. Sans cette
                // ligne, un SetAVTransportURI lent laisse un trou muet et
                // l'incident n'est plus instruisable : dans le journal de
                // FabienM (#2581), 23,5 s s'écoulent entre « flux prêt » et le
                // premier refus de Play sans une seule trace de la sortie.
                info!(
                    device = %self.name,
                    url = media.url,
                    niveau_didl,
                    advertised_mime = %attempt_mime,
                    duree_ms = debut_set_uri.elapsed().as_millis() as u64,
                    "dlna_set_uri_ok"
                );
                break;
            }

            // Error 714 ("Illegal MIME-type"): the renderer parsed the DIDL but
            // its ConnectionManager Sink does not list the announced MIME.
            // Beoplay A9 / Sink audio/x-flac, forum 714: strict renderers (B&O,
            // Lyngdorf) reject `audio/flac` when their Sink only lists
            // `audio/x-flac`, even though they decode the stream. ONLY here (on
            // a real 714) do we pay a single GetProtocolInfo probe, then retry
            // up to twice: (a) with the exact Sink spelling, (b) with a PCM
            // profile the Sink lists. Strict renderers gate on the announced
            // MIME but decode by content, so a Sink-accepted label lets the
            // actual FLAC bytes through.
            let is_714 = set_uri_resp.contains(">714<")
                || set_uri_resp.to_lowercase().contains("illegal mime");

            if is_714 && (!tried_exact || !tried_fallback) {
                // Probe the Sink once, on the first 714 only.
                if sink.is_empty() {
                    sink = self.get_protocol_info().await.unwrap_or_default();
                }

                // Retry (a): announce the exact spelling the Sink lists
                // (e.g. audio/x-flac) if it differs from what we just sent.
                if !tried_exact {
                    tried_exact = true;
                    let exact = advertised_mime_for_sink(media.mime_type, &sink);
                    if !exact.eq_ignore_ascii_case(&attempt_mime) {
                        warn!(
                            device = %self.name,
                            advertised_mime = %attempt_mime,
                            exact_mime = %exact,
                            sink = ?sink,
                            "dlna_set_uri_714_exact_spelling_retry"
                        );
                        attempt_mime = exact;
                        continue;
                    }
                }

                // Retry (b): fall back to a PCM MIME the Sink lists (audio/wav
                // then audio/L16) if we have not tried it yet.
                if !tried_fallback {
                    tried_fallback = true;
                    if let Some(fb) = fallback_mime_from_sink(&sink) {
                        if !fb.eq_ignore_ascii_case(&attempt_mime) {
                            warn!(
                                device = %self.name,
                                advertised_mime = %attempt_mime,
                                fallback_mime = %fb,
                                sink = ?sink,
                                "dlna_set_uri_714_pcm_fallback_retry"
                            );
                            attempt_mime = fb;
                            continue;
                        }
                    }
                }
            }

            if is_714 {
                // Surface the exact MIME + Sink so the mismatch is diagnosable
                // from a single log line (Mickaël, #1146: TIDAL → Beoplay 714).
                warn!(
                    device = %self.name,
                    advertised_mime = %attempt_mime,
                    live_stream = media.live_stream,
                    sink_entries = sink.len(),
                    sink = ?sink,
                    response = %set_uri_resp,
                    "dlna_set_uri_illegal_mime_714"
                );
                return Err(format!(
                    "SetAVTransportURI rejected 714 Illegal MIME-type: renderer Sink does not accept advertised MIME '{attempt_mime}' (sink has {} entries); rejected: {set_uri_resp}",
                    sink.len()
                ));
            }
            warn!(device = %self.name, response = %set_uri_resp, "dlna_set_uri_error");
            return Err(format!("SetAVTransportURI rejected: {set_uri_resp}"));
        }

        let play_delay = self.play_delay_ms.load(Ordering::Relaxed);
        self.attendre_apres_set_uri().await;

        // Retry Play with backoff — some renderers (Revox S100, stagefright-based)
        // reject Play immediately after SetAVTransportURI while still loading the URI.
        // On first 501, send another Stop then retry — the Revox needs an explicit
        // Stop after SetAVTransportURI when it was already playing.
        //
        // Le 701 « Transition not available » ne dit pas « je suis en panne » :
        // il dit « pas CETTE transition, MAINTENANT » — et le renderer sait
        // dans quel état il est. Le barème aveugle répondait à côté (#2581,
        // journal FabienM du 27/08) : cinq refus 701 en 11,4 s, zone arrêtée
        // après 36 s… et la MÊME piste vers le MÊME appareil part du premier
        // coup 1,8 s plus tard, dès qu'un SetAVTransportURI est rejoué. On lit
        // donc le transport avant de réessayer : il charge encore → le laisser
        // finir, sans le Stop du barème qui le ferait retomber ; il ne tient
        // plus de média → lui réarmer l'URI, sans quoi chaque Play suivant est
        // un 701 de plus. Le renderer qui ne dit rien d'exploitable garde le
        // barème historique, au mot près.
        let mut last_err = String::new();
        let mut reprise = RepriseApresRefus::StopPuisPlay;
        for attempt in 0..5u32 {
            if attempt > 0 {
                let delay = match attempt {
                    1 => 500,
                    2 => 1500,
                    3 => 3000,
                    _ => 4000,
                };
                match reprise {
                    RepriseApresRefus::StopPuisPlay if attempt == 1 => {
                        debug!(device = %self.name, "dlna_play_retry_sending_stop");
                        let _ = self.av_action("Stop", "<InstanceID>0</InstanceID>").await;
                        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
                        let _ = self
                            .av_action("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>")
                            .await;
                    }
                    RepriseApresRefus::ReArmerUri => {
                        info!(device = %self.name, attempt, "dlna_play_701_rearmement_uri");
                        let _ = self
                            .reposer_uri(media, item_id, &attempt_mime, niveau_didl)
                            .await;
                        // Même battement que la pose initiale : reposer l'URI
                        // puis jouer aussitôt, c'est refabriquer le « trop tôt ».
                        self.attendre_apres_set_uri().await;
                    }
                    // Chargement en cours, ou barème historique hors du premier
                    // essai : ne rien envoyer de plus.
                    _ => {}
                }
                info!(device = %self.name, attempt, delay_ms = delay, "dlna_play_retry");
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            let play_resp = self
                .av_action("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>")
                .await?;

            if !play_resp.contains("UPnPError") && !play_resp.contains("<errorCode>") {
                if attempt > 0 {
                    info!(device = %self.name, attempt, "dlna_play_retry_succeeded");
                }
                last_err.clear();
                break;
            }
            warn!(device = %self.name, attempt, response = %play_resp, "dlna_play_error");
            last_err = format!("Play rejected: {play_resp}");
            // Un 701 nomme un état : on va le LIRE plutôt que le deviner. Un
            // renderer sans GetTransportInfo ne change rien au barème.
            reprise = if est_701(&play_resp) {
                let etat = self
                    .av_action("GetTransportInfo", "<InstanceID>0</InstanceID>")
                    .await
                    .ok()
                    .and_then(|xml| extract_tag(&xml, "CurrentTransportState"));
                let choix = reprise_apres_refus_play(&play_resp, etat.as_deref());
                info!(
                    device = %self.name,
                    attempt,
                    etat = etat.as_deref().unwrap_or("-"),
                    reprise = ?choix,
                    "dlna_play_701_transport_lu"
                );
                choix
            } else {
                RepriseApresRefus::StopPuisPlay
            };
        }
        if !last_err.is_empty() {
            return Err(last_err);
        }

        // Le Play est acquitté — est-il APPLIQUÉ ? Dans la course des 5 ms,
        // l'Eversolo répond OK à toute la séquence et garde l'URI précédente :
        // la zone affichait « playing » sur la position de l'ancienne piste,
        // et l'utilisateur relançait à la main. On relit l'URI courante ; en
        // cas d'écart, UNE relance complète, puis un échec VISIBLE plutôt
        // qu'un état menteur. Une URI qu'on ne sait pas interpréter (renderer
        // qui réécrit) ne conclut rien — zéro régression sur ces appareils.
        // La verification est EXTRAITE dans `verifier_uri_appliquee` : un test
        // qui la retranscrirait resterait vert pendant que CE chemin-ci se
        // degrade. Elle ne recoit que les deux actions qu'elle pilote — lire ce
        // que le renderer TIENT (`uri_tenue_par_le_renderer`, qui consulte les
        // DEUX champs de l'AVTransport), et reposer l'URI puis rejouer — pour
        // qu'un banc puisse les simuler sans renderer (#2749).
        let moi = &*self;
        let media_verif = media;
        let url_verif = media.url;
        let mime_relance = attempt_mime.as_str();
        let verif = verifier_uri_appliquee(
            media.url,
            std::time::Duration::from_millis(self.budget_reveil_ms.load(Ordering::Relaxed)),
            || async move { moi.uri_tenue_par_le_renderer(url_verif).await },
            || async move {
                warn!(device = %moi.name, url = media_verif.url, ctrl = %moi.url_av_transport(), "dlna_play_acquitte_mais_pas_applique_relance");
                let _ = moi
                    .reposer_uri(media_verif, item_id, mime_relance, niveau_didl)
                    .await;
                moi.attendre_apres_set_uri().await;
                match moi
                    .av_action("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>")
                    .await
                {
                    Ok(resp) if resp.contains("UPnPError") || resp.contains("<errorCode>") => {
                        warn!(device = %moi.name, response = %resp, "dlna_relance_play_refuse");
                        Some(resp)
                    }
                    _ => None,
                }
            },
        )
        .await;
        let applique = verif.verdict;
        let uri_tenue = verif.uri_tenue.clone();
        let refus_relance = verif.refus_relance.clone();
        if matches!(applique, UriVerdict::PasAppliquee | UriVerdict::PasEncore) {
            // Deux pannes, deux journaux. « Jamais applique » decrit un
            // renderer qui TIENT autre chose ; l'URI restee VIDE decrit un
            // appareil qui ne tient RIEN — un reveil qui n'a pas abouti. Le
            // meme evenement pour les deux rendait le tri impossible cote
            // support (#2749).
            if applique == UriVerdict::PasEncore {
                warn!(
                    device = %self.name,
                    url = media.url,
                    ctrl = %self.url_av_transport(),
                    attente_ms = verif.attente_ms,
                    relances = verif.relances,
                    soap_muet = verif.soap_muet,
                    "dlna_play_uri_restee_vide"
                );
            } else {
                warn!(
                    device = %self.name,
                    url = media.url,
                    ctrl = %self.url_av_transport(),
                    tenue = uri_tenue.as_deref().unwrap_or("-"),
                    attente_ms = verif.attente_ms,
                    relances = verif.relances,
                    "dlna_play_jamais_applique"
                );
            }
            // Si le renderer tient un flux de NOTRE serveur, ce flux va mourir
            // avec la session que l'appelant s'apprête à démonter — et le
            // DMP-A8 ressasse une URI morte en zombie (PLAYING/TRANSITIONING,
            // sourd aux Stop) jusqu'à bloquer toute prise de contrôle
            // ultérieure. On vide son média, au mieux. Un flux ÉTRANGER, lui,
            // est peut-être une lecture légitime d'un autre serveur : on n'y
            // touche pas.
            let notre_origine: String = media
                .url
                .splitn(4, '/')
                .take(3)
                .collect::<Vec<_>>()
                .join("/");
            if uri_tenue
                .as_deref()
                .is_some_and(|u| !notre_origine.is_empty() && u.starts_with(&notre_origine))
            {
                debug!(device = %self.name, "dlna_echec_vidage_du_media_mort");
                let _ = self
                    .av_action(
                        "SetAVTransportURI",
                        "<InstanceID>0</InstanceID><CurrentURI></CurrentURI><CurrentURIMetaData></CurrentURIMetaData>",
                    )
                    .await;
            }
            // Le renderer a-t-il ACQUITTÉ, ou REFUSÉ ? Les deux échouent ici,
            // mais ils n'appellent pas la même conduite : un refus nomme un
            // état (701 « Transition not available » : pas cette transition,
            // maintenant), un acquittement sans effet désigne une URI restée en
            // place. Dire l'un pour l'autre envoie l'utilisateur chercher là où
            // il n'y a rien.
            if let Some(resp) = &refus_relance {
                return Err(format!(
                    "Le renderer a REFUSÉ le Play de la relance{} : {resp}",
                    if est_701(resp) {
                        " (701 « Transition not available » : il n'accepte pas cette transition dans son état actuel)"
                    } else {
                        ""
                    }
                ));
            }
            // #2749 — une URI VIDE n'est pas « une autre source ».
            //
            // Un Denon/HEOS en veille reseau garde sa pile UPnP vivante : il
            // acquitte tout, et ne tient RIEN. Lui dire qu'il « joue une autre
            // source » envoie l'utilisateur chercher un conflit qui n'existe
            // pas — c'est la meme faute que #2396, sur le meme message.
            if applique == UriVerdict::PasEncore {
                let secondes = verif.attente_ms / 1000;
                return Err(if verif.soap_muet {
                    format!(
                        "Le renderer a acquitté Play, n'a jamais appliqué l'URI (CurrentURI resté vide) \
                         puis a CESSÉ de répondre en SOAP au bout de {secondes} s — appareil éteint, \
                         débranché ou sorti du réseau ?"
                    )
                } else {
                    format!(
                        "Le renderer a acquitté Play mais ne tient toujours AUCUN média après {secondes} s \
                         (ni CurrentURI ni TrackURI) : il ne joue pas autre chose, il n'a rien chargé. \
                         Tune ne peut pas dire POURQUOI : l'appareil répond et n'exécute pas. Sur un ampli \
                         en veille réseau (Denon/HEOS, Marantz), relancer aussitôt aboutit souvent — la \
                         première tentative l'a réveillé ; le délai de bascule varie d'un appareil et d'un \
                         état à l'autre, et peut dépasser cette attente"
                    )
                });
            }
            let detail = match uri_tenue.as_deref() {
                Some(u) if !u.trim().is_empty() => format!("il tient encore : {u}"),
                _ => "URI non appliquée après relance".to_string(),
            };
            return Err(format!(
                "Le renderer a acquitté Play mais joue toujours une autre source ({detail})"
            ));
        }

        info!(device = %self.name, url = media.url, ctrl = %self.url_av_transport(), delay_ms = play_delay, "dlna_play");
        // La piste tourne : on s'abonne, et l'ancre repart de zéro sur cette
        // URI. Même moment que `OpenHomeOutput::play_media` — un abonnement
        // pris avant que le renderer ait la piste ne décrirait rien.
        self.ancrer_position(0, true, Some(media.url.to_string()))
            .await;
        self.annoncer_duree(media.url, media.duration_ms).await;
        self.subscribe_events().await;
        Ok(())
    }

    async fn pause(&self) -> Result<(), String> {
        self.av_action("Pause", "<InstanceID>0</InstanceID>")
            .await?;
        Ok(())
    }

    async fn resume(&self) -> Result<(), String> {
        self.av_action("Play", "<InstanceID>0</InstanceID><Speed>1</Speed>")
            .await?;
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        self.unsubscribe_events().await;
        self.av_action("Stop", "<InstanceID>0</InstanceID>").await?;
        info!(device = %self.name, "dlna_stop");
        Ok(())
    }

    async fn seek(&self, position_ms: u64) -> Result<(), String> {
        let target = Self::format_time(position_ms);
        self.av_action(
            "Seek",
            &format!("<InstanceID>0</InstanceID><Unit>REL_TIME</Unit><Target>{target}</Target>"),
        )
        .await?;
        // Seul déplacement que le mode silence voit tout de suite : celui qui
        // passe par Tune. Celui fait sur la façade de l'appareil attendra le
        // prochain évènement — c'est le prix annoncé de l'option.
        let ancre = self.ancre_position.lock().await.clone();
        self.ancrer_position(position_ms, ancre.avance, ancre.uri)
            .await;
        Ok(())
    }

    async fn set_volume(&self, volume: f64) -> Result<(), String> {
        if let Some(ip) = &self.micromega_ip {
            let target_vol = volume * 100.0;
            let msg = format!("GET /volume HTTP/1.0\r\n\r\nvolume={target_vol:.1}\r\n");
            let addr = format!("{ip}:7000");
            match tokio::time::timeout(std::time::Duration::from_secs(3), TcpStream::connect(&addr))
                .await
            {
                Ok(Ok(mut stream)) => {
                    let _ = stream.write_all(msg.as_bytes()).await;
                    let _ = stream.shutdown().await;
                    debug!(device = %self.name, volume = target_vol, "micromega_volume_set");
                    self.memoriser_volume(target_vol.round() as u64).await;
                }
                Ok(Err(e)) => {
                    warn!(device = %self.name, volume = target_vol, error = %e, "micromega_volume_error");
                }
                Err(_) => {
                    warn!(device = %self.name, volume = target_vol, "micromega_volume_timeout");
                }
            }
            return Ok(());
        }
        let level = (volume * 100.0).round() as u32;
        let resp = self.rc_action("SetVolume", &format!(
            "<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredVolume>{level}</DesiredVolume>"
        )).await?;
        if resp.contains("UPnPError") || resp.contains("<errorCode>") {
            // Sonos rejects RenderingControl SetVolume with 401.
            // Try GroupRenderingControl on the same host instead.
            if self.device_id.contains("RINCON") {
                let grc_resp = self
                    .soap_action(
                        VoieSoap::GroupRenderingControl,
                        "urn:schemas-upnp-org:service:GroupRenderingControl:1",
                        "SetGroupVolume",
                        &format!(
                            "<InstanceID>0</InstanceID><DesiredVolume>{level}</DesiredVolume>"
                        ),
                    )
                    .await?;
                if grc_resp.contains("UPnPError") || grc_resp.contains("<errorCode>") {
                    warn!(device = %self.name, level, response = %grc_resp, "sonos_group_volume_rejected");
                    return Err(format!(
                        "« {} » a refusé le réglage de volume. Réglez-le sur l'appareil lui-même.",
                        self.name
                    ));
                }
                debug!(device = %self.name, level, "sonos_group_volume_ok");
                self.memoriser_volume(level as u64).await;
                return Ok(());
            }
            // The renderer answered, and said no. Reporting Ok() here — as this
            // did — made the slider move, the value persist, and nothing come
            // out of the speakers any louder: three layers agreeing on a change
            // that never happened (Eric, forum, renderer Diretta + PC vu comme
            // zone DLNA). Say it instead.
            warn!(device = %self.name, level, response = %resp, "dlna_set_volume_rejected");
            return Err(format!(
                "« {} » a refusé le réglage de volume. Réglez-le sur l'appareil lui-même.",
                self.name
            ));
        }
        debug!(device = %self.name, level, "dlna_set_volume_ok");
        self.memoriser_volume(level as u64).await;
        Ok(())
    }

    async fn set_mute(&self, muted: bool) -> Result<(), String> {
        let val = if muted { "1" } else { "0" };
        self.rc_action("SetMute", &format!(
            "<InstanceID>0</InstanceID><Channel>Master</Channel><DesiredMute>{val}</DesiredMute>"
        )).await?;
        // Même geste que `OpenHomeOutput::set_mute` : l'état poussé porte
        // désormais la coupure, on ne le laisse pas en retard d'un évènement
        // sur ce que Tune vient d'obtenir.
        self.event_state.lock().await.muted = Some(muted);
        // Mémorisé seulement après un SetMute accepté : `get_status` ne
        // redemande plus rien au renderer (#2263), donc ce champ est la seule
        // source du `muted` rendu — il ne doit jamais annoncer une coupure que
        // l'appareil a refusée.
        self.muted.store(muted, Ordering::Relaxed);
        Ok(())
    }

    /// Trois régimes, du plus bavard au plus muet — et le plus bavard est
    /// toujours celui du repli.
    ///
    /// | régime | conditions | actions SOAP par appel |
    /// |---|---|---|
    /// | sondage | pas d'abonnement tenu | **3** (comme avant #2263) |
    /// | évènements | abonnement tenu | **1** (`GetPositionInfo`) |
    /// | silence UPnP | abonnement tenu + opt-in de zone | **0** |
    ///
    /// Le régime « évènements » ne prend aux évènements que ce qu'ils savent
    /// dire mieux que le sondage — l'état du transport, le volume, la coupure —
    /// et continue de MESURER la position. C'est délibéré : la position n'est
    /// pas une variable évènementielle obligatoire d'`AVTransport:1`, presque
    /// aucun renderer ne la pousse, et l'inventer par défaut changerait la
    /// vérité de l'état pour tout le monde sans que personne l'ait demandé.
    ///
    /// Le régime « silence » l'invente, justement, et c'est tout son prix : la
    /// position devient une EXTRAPOLATION (dernière ancre + horloge murale), et
    /// un déplacement fait sur la façade de l'appareil ne se voit qu'au
    /// prochain évènement. Les deux conséquences remontent au client par
    /// [`DlnaOutput::etat_evenements`], jamais tues.
    async fn get_status(&self) -> Result<OutputStatus, String> {
        let (
            evt_vivant,
            evt_etat,
            evt_volume,
            evt_muted,
            evt_uri,
            evt_duree,
            evt_titre,
            evt_artiste,
            evt_position,
        ) = {
            let es = self.event_state.lock().await;
            (
                es.is_live(),
                es.transport_state,
                es.volume,
                es.muted,
                es.track_uri.clone(),
                es.duration_ms,
                es.track_title.clone(),
                es.track_artist.clone(),
                es.position_ms.zip(es.position_at),
            )
        };
        let silence = self.upnp_silence.load(Ordering::Relaxed);

        if evt_vivant && silence {
            // ── Silence UPnP : zéro action ────────────────────────────────
            let etat = evt_etat.unwrap_or(TransportState::Stopped);
            let position_ms = self
                .extrapoler_position(etat, evt_uri.as_ref(), evt_position)
                .await;
            self.position_extrapolee.store(true, Ordering::Relaxed);
            let volume = match evt_volume {
                Some(v) => v as f64 / 100.0,
                // Le renderer n'a jamais poussé de volume (RenderingControl
                // absent ou muet). On rend le dernier que Tune a posé — et à
                // défaut la même valeur de repli que le chemin de sondage quand
                // la réponse est illisible. Interroger l'appareil ici
                // trahirait le silence promis.
                None => match self.dernier_volume_pct.load(Ordering::Relaxed) {
                    u64::MAX => 0.5,
                    v => v as f64 / 100.0,
                },
            };
            return Ok(OutputStatus {
                state: etat,
                position_ms,
                duration_ms: match evt_duree {
                    Some(d) => d,
                    None => self.duree_connue_pour(evt_uri.as_ref()).await.unwrap_or(0),
                },
                volume,
                muted: evt_muted.unwrap_or_else(|| self.muted.load(Ordering::Relaxed)),
                current_uri: evt_uri,
                track_title: evt_titre,
                track_artist: evt_artiste,
                ended_naturally: false,
                realtime: true,
                dop_active: false,
            });
        }

        let position_resp = self
            .av_action("GetPositionInfo", "<InstanceID>0</InstanceID>")
            .await?;

        if evt_vivant {
            // ── Évènements : une seule action, la position ────────────────
            let position_ms = extract_tag(&position_resp, "RelTime")
                .map(|t| Self::parse_time(&t))
                .unwrap_or(0);
            let precedente = self
                .derniere_position_ms
                .swap(position_ms, Ordering::Relaxed);
            let position_a_bouge = precedente != u64::MAX && position_ms != precedente;

            let mut etat = evt_etat.unwrap_or(TransportState::Stopped);
            if Self::etat_contredit_la_position(etat, position_a_bouge) && precedente != u64::MAX {
                let mut depuis = self.incoherence_depuis.lock().await;
                let debut = depuis.get_or_insert_with(std::time::Instant::now);
                if debut.elapsed() >= INCOHERENCE_AVANT_ARBITRAGE {
                    // On tranche à la source, exactement comme le chemin de
                    // sondage — une action de plus, seulement le temps que la
                    // contradiction dure.
                    if let Ok(resp) = self
                        .av_action("GetTransportInfo", "<InstanceID>0</InstanceID>")
                        .await
                    {
                        let arbitre = etat_du_transport(&resp);
                        if arbitre != etat {
                            warn!(
                                device = %self.name,
                                evenement = ?etat,
                                mesure = ?arbitre,
                                "dlna_evenement_contredit_par_le_transport"
                            );
                        }
                        etat = arbitre;
                    }
                }
            } else {
                *self.incoherence_depuis.lock().await = None;
            }

            let duration_ms = extract_tag(&position_resp, "TrackDuration")
                .map(|t| Self::parse_time(&t))
                .unwrap_or(0);
            let current_uri = extract_tag(&position_resp, "TrackURI").or(evt_uri);
            // Tenir l'ancre à jour même hors mode silence : basculer l'option
            // en pleine lecture ne doit pas repartir d'une ancre périmée.
            self.ancrer_position(
                position_ms,
                etat == TransportState::Playing,
                current_uri.clone(),
            )
            .await;
            self.position_extrapolee.store(false, Ordering::Relaxed);

            let volume = match evt_volume {
                Some(v) => v as f64 / 100.0,
                // Le RenderingControl n'a rien poussé : on ne devine pas, on
                // demande — comme avant. Un abonnement AVTransport tenu ne
                // dispense pas d'avoir un volume juste.
                None => self.lire_volume().await?,
            };

            return Ok(OutputStatus {
                state: etat,
                position_ms,
                duration_ms,
                volume,
                muted: evt_muted.unwrap_or_else(|| self.muted.load(Ordering::Relaxed)),
                current_uri,
                track_title: extract_tag(&position_resp, "dc:title").or(evt_titre),
                track_artist: extract_tag(&position_resp, "dc:creator").or(evt_artiste),
                ended_naturally: false,
                realtime: true,
                dop_active: false,
            });
        }

        // ── Sondage : le chemin d'avant #2263, mot pour mot ───────────────
        let transport_resp = self
            .av_action("GetTransportInfo", "<InstanceID>0</InstanceID>")
            .await?;
        // Pas de `GetMute` ici. Le poller passe par cette fonction une fois
        // par seconde et par zone pendant TOUTE la lecture : l'action valait
        // un quart du trafic SOAP envoyé au renderer, pour une valeur que
        // personne ne lisait (#2263). L'état coupé se lit maintenant en local.
        let volume = self.lire_volume().await?;
        let state = etat_du_transport(&transport_resp);

        let position_ms = extract_tag(&position_resp, "RelTime")
            .map(|t| Self::parse_time(&t))
            .unwrap_or(0);
        let duration_ms = extract_tag(&position_resp, "TrackDuration")
            .map(|t| Self::parse_time(&t))
            .unwrap_or(0);
        let muted = self.muted.load(Ordering::Relaxed);
        let current_uri = extract_tag(&position_resp, "TrackURI");

        // Même entretien que dans le régime « évènements » : l'ancre suit la
        // mesure, pour qu'un abonnement qui s'établit en pleine lecture (ou une
        // option armée en cours de route) ne reparte pas de zéro.
        self.derniere_position_ms
            .store(position_ms, Ordering::Relaxed);
        self.ancrer_position(
            position_ms,
            state == TransportState::Playing,
            current_uri.clone(),
        )
        .await;
        self.position_extrapolee.store(false, Ordering::Relaxed);

        Ok(OutputStatus {
            state,
            position_ms,
            duration_ms,
            volume,
            muted,
            current_uri,
            track_title: extract_tag(&position_resp, "dc:title"),
            track_artist: extract_tag(&position_resp, "dc:creator"),
            ended_naturally: false,
            // A renderer plays at 1x: keep the poller's wall-clock guards.
            realtime: true,
            // Aucune sortie hors la locale ne produit du DoP : le DSD y part
            // tel quel ou transcode, jamais empaquete dans du PCM 24 bits.
            dop_active: false,
        })
    }

    async fn is_available(&self) -> bool {
        self.client
            .get(self.url_av_transport())
            .timeout(std::time::Duration::from_secs(3))
            .send()
            .await
            .is_ok()
    }

    async fn set_next_media(&self, media: &PlayMedia<'_>) -> Result<(), String> {
        // Id neuf, distinct de celui du `SetAVTransportURI` qui précède comme de
        // tous ceux déjà émis vers cet appareil (#3675).
        let item_id_suivant = self.next_item_id();
        let item_id = item_id_suivant.as_str();
        // Même échelle que le SetAVTransportURI du play : le DIDL complet du
        // gapless a la même taille, donc le même échec de lecture chez
        // Platinum — et un gapless silencieusement perdu, c'est une file qui
        // s'arrête entre deux pistes.
        let mut resp = None;
        // Même départ au niveau appris que le play : l'échec du DIDL complet
        // est une propriété de l'appareil, pas de la piste (#2394) — mais une
        // propriété qui EXPIRE, pour qu'un hoquet ne dégrade pas la file
        // entière (#3675). Même porte, même horodatage que `play_media`.
        let maintenant_ms = horloge_process_ms();
        let depart = self
            .didl_niveau_appris
            .niveau_de_depart(maintenant_ms, &self.name);
        for niveau in depart..=2 {
            let metadata = match niveau {
                0 => Self::didl_metadata(media, item_id),
                1 => Self::didl_metadata_minimale(media, item_id, media.mime_type),
                _ => String::new(),
            };
            match self.av_action("SetNextAVTransportURI", &format!(
                "<InstanceID>0</InstanceID><NextURI>{}</NextURI><NextURIMetaData>{metadata}</NextURIMetaData>",
                media.url
            )).await {
                Ok(r) => {
                    self.didl_niveau_appris
                        .apprendre(niveau, maintenant_ms, &self.name);
                    resp = Some(r);
                    break;
                }
                Err(e) if e.starts_with(SOAP_HTTP_SANS_CORPS_PREFIX) && niveau < 2 => {
                    warn!(device = %self.name, niveau = niveau + 1, error = %e, "dlna_set_next_corps_illisible_didl_reduit");
                }
                Err(e) => return Err(e),
            }
        }
        let resp =
            resp.ok_or_else(|| "SetNextAVTransportURI: aucune tentative aboutie".to_string())?;
        if resp.contains("UPnPError") || resp.contains("<errorCode>") {
            warn!(device = %self.name, response = %resp, "dlna_set_next_rejected");
            return Err(format!("SetNextAVTransportURI rejected: {resp}"));
        }
        // Chemin SŒUR du `play_media` : en gapless, le renderer passe à la
        // piste suivante tout seul et l'URI change sans repasser par là-bas.
        // Sans cette ligne, le mode silence retomberait à zéro sur la durée dès
        // la deuxième piste d'une file.
        self.annoncer_duree(media.url, media.duration_ms).await;
        info!(device = %self.name, url = media.url, "dlna_set_next");
        Ok(())
    }
}

impl DlnaOutput {
    pub async fn get_protocol_info(&self) -> Result<Vec<String>, String> {
        let body = self
            .soap_action(
                VoieSoap::ConnectionManager,
                "urn:schemas-upnp-org:service:ConnectionManager:1",
                "GetProtocolInfo",
                "",
            )
            .await?;
        let sink = extract_tag(&body, "Sink").unwrap_or_default();
        Ok(sink
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect())
    }
}

#[derive(Debug, Clone, Default)]
pub struct DsdCapability {
    pub supports_dsf: bool,
    pub supports_dff: bool,
    pub dsf_mime: Option<String>,
}

/// Read native DSD support out of a non-empty GetProtocolInfo Sink.
///
/// Split out of `probe_dsd_support` so the parsing can be unit-tested without a
/// live renderer: the caller owns the "did the probe even succeed" question
/// (`Option`), this owns "what does the Sink say".
///
/// `dsf_mime` keeps the renderer's own spelling of the MIME (3rd colon-separated
/// field of `http-get:*:audio/dsf:*`), because some renderers only accept the
/// exact MIME they advertise rather than the generic `application/x-dsd`.
fn parse_dsd_capability(protocols: &[String]) -> DsdCapability {
    let mut cap = DsdCapability::default();
    for proto in protocols {
        let lower = proto.to_lowercase();
        if lower.contains("x-dsd")
            || lower.contains("audio/dsf")
            || lower.contains("audio/x-dsf")
            || lower.contains("application/x-dsd")
            || lower.contains("application/dsf")
            || lower.contains("audio/vnd.dsd")
        {
            cap.supports_dsf = true;
            if cap.dsf_mime.is_none() {
                let parts: Vec<&str> = proto.split(':').collect();
                if parts.len() >= 3 {
                    cap.dsf_mime = Some(parts[2].trim().to_string());
                }
            }
        }
        if lower.contains("audio/dff") || lower.contains("x-dff") || lower.contains("audio/x-dff") {
            cap.supports_dff = true;
        }
    }
    cap
}

impl DlnaOutput {
    /// Probe the renderer's GetProtocolInfo Sink for native DSD support.
    ///
    /// `Some(cap)` when the Sink was actually read — including a conclusive
    /// "this renderer does not do DSD" (all flags false). `None` when the probe
    /// was **inconclusive**: GetProtocolInfo failed, or the Sink came back
    /// empty. The caller must fall back conservatively for `None` but must NOT
    /// cache it — same rule as `supports_mime` below. A transient
    /// GetProtocolInfo failure (renderer asleep, busy, or slow to answer right
    /// after discovery) would otherwise pin a DSD-capable renderer to the
    /// DSD→PCM transcode path for the whole session, with no way to recover
    /// short of restarting the server.
    pub async fn probe_dsd_support(&self) -> Option<DsdCapability> {
        let protocols = match self.get_protocol_info().await {
            Ok(p) => p,
            Err(e) => {
                warn!(device = %self.name, error = %e, "dsd_probe_protocol_info_failed");
                return None;
            }
        };
        if protocols.is_empty() {
            debug!(device = %self.name, "dsd_probe_empty_sink");
            return None;
        }
        debug!(device = %self.name, protocols = ?protocols, "dsd_probe_protocol_info_raw");
        let cap = parse_dsd_capability(&protocols);
        info!(device = %self.name, supports_dsf = cap.supports_dsf, supports_dff = cap.supports_dff, dsf_mime = ?cap.dsf_mime, protocols_count = protocols.len(), "dsd_probe_result");
        Some(cap)
    }

    /// Probe the renderer's GetProtocolInfo Sink to check if a given MIME type
    /// is supported.  Protocol info entries have the format:
    ///   `http-get:*:audio/flac:*`
    /// The third colon-separated field is the MIME type.
    /// `Some(true)`/`Some(false)` when the Sink was successfully probed;
    /// `None` when the probe failed or the Sink was empty (inconclusive). The
    /// caller falls back conservatively for `None` but must NOT cache it — a
    /// transient GetProtocolInfo failure on a budget renderer (Marco's Denon
    /// Ceol N12) must not poison FLAC support for the whole session, forcing a
    /// WAV transcode on every track even though the renderer decodes FLAC.
    pub async fn supports_mime(&self, mime: &str) -> Option<bool> {
        let protocols = match self.get_protocol_info().await {
            Ok(p) => p,
            Err(e) => {
                debug!(device = %self.name, error = %e, mime, "protocol_info_unavailable");
                return None;
            }
        };
        if protocols.is_empty() {
            debug!(device = %self.name, mime, "protocol_info_empty_sink");
            return None;
        }
        if protocol_sink_supports_mime(mime, &protocols) {
            return Some(true);
        }
        info!(device = %self.name, mime, protocols_count = protocols.len(), "dlna_mime_not_supported_by_renderer");
        Some(false)
    }

    /// One-shot capability probe for the renderer-config UI: reads the
    /// GetProtocolInfo `Sink` ONCE and summarises which audio formats it
    /// advertises, so the user can pick a sensible output override (native FLAC,
    /// native ALAC, forced WAV/LPCM…) with evidence rather than by trial. A
    /// failed/empty probe returns `probed: false` (inconclusive — the renderer
    /// may still decode more than it advertises; the negotiation fallbacks stay
    /// in charge).
    pub async fn probe_capabilities(&self) -> RendererCapabilities {
        match self.get_protocol_info().await {
            Ok(sink) if !sink.is_empty() => renderer_caps_from_sink(sink),
            // Le `_ =>` d'origine avalait l'erreur : l'utilisateur voyait
            // « impossible de lire les capacités » et le journal ne portait
            // AUCUNE trace de la sonde (#1984). Dire lequel des deux cas s'est
            // produit — l'appel a échoué, ou le Sink est vide — coûte une ligne
            // et distingue « injoignable » de « joignable mais muet ».
            Ok(_) => {
                warn!(device = %self.name, "renderer_caps_probe_empty_sink");
                RendererCapabilities::inconclusive("empty_sink")
            }
            Err(e) => {
                warn!(device = %self.name, error = %e, "renderer_caps_probe_failed");
                RendererCapabilities::inconclusive("soap_failed")
            }
        }
    }
}

/// What a DLNA renderer advertises in its GetProtocolInfo `Sink`. `probed` is
/// false when the Sink could not be read (empty/timeout) — everything else is
/// then meaningless and the UI should say "couldn't read capabilities".
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RendererCapabilities {
    pub probed: bool,
    /// Stable machine-readable cause when `probed` is false. The API, not the
    /// translated UI, knows whether SOAP failed or returned an empty Sink.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    pub flac: bool,
    /// Plain `audio/wav` / `audio/x-wav`.
    pub wav: bool,
    /// 16-bit LPCM (`audio/L16`) — the standard DLNA WAV profile.
    pub lpcm16: bool,
    /// 24-bit LPCM (`audio/L24`) — gates the "WAV 24-bit" override.
    pub lpcm24: bool,
    pub alac: bool,
    pub aac: bool,
    pub mp3: bool,
    pub dsd: bool,
    /// Raw Sink entries, for an advanced/debug view.
    pub sink: Vec<String>,
}

impl RendererCapabilities {
    fn inconclusive(reason: &'static str) -> Self {
        Self {
            reason: Some(reason),
            ..Self::default()
        }
    }
}

/// Pure Sink → capabilities mapping (unit-tested; `probe_capabilities` wraps it
/// around the SOAP call).
fn renderer_caps_from_sink(sink: Vec<String>) -> RendererCapabilities {
    // Param-aware match: LPCM entries carry `;rate=…;channels=…` after the MIME
    // (`audio/L16;rate=44100;channels=2`), so we compare the base MIME only.
    // Also accepts the `audio/x-…` legacy variant and the `*` wildcard, like
    // `protocol_sink_supports_mime` (which only handles the param-less case).
    let has = |want: &str| -> bool {
        let want = want.to_lowercase();
        let alt = want
            .strip_prefix("audio/x-")
            .map(|r| format!("audio/{r}"))
            .or_else(|| want.strip_prefix("audio/").map(|r| format!("audio/x-{r}")));
        sink.iter().any(|p| {
            let Some(field) = p.split(':').nth(2) else {
                return false;
            };
            let mime = field.trim().to_lowercase();
            let base = mime.split(';').next().unwrap_or(&mime).trim();
            base == want || base == "*" || alt.as_deref() == Some(base)
        })
    };
    let dsd = sink.iter().any(|p| {
        let l = p.to_lowercase();
        l.contains("x-dsd")
            || l.contains("audio/dsf")
            || l.contains("audio/dff")
            || l.contains("audio/x-dsf")
            || l.contains("audio/x-dff")
            || l.contains("audio/vnd.dsd")
            || l.contains("application/x-dsd")
    });
    RendererCapabilities {
        probed: true,
        reason: None,
        flac: has("audio/flac"),
        wav: has("audio/wav"),
        lpcm16: has("audio/l16"),
        lpcm24: has("audio/l24"),
        // ALAC is rarely advertised distinctly; renderers expose it as m4a/mp4.
        alac: has("audio/x-m4a") || has("audio/alac") || has("audio/mp4"),
        aac: has("audio/aac") || has("audio/mp4"),
        mp3: has("audio/mpeg"),
        dsd,
        sink,
    }
}

/// Whether a renderer's GetProtocolInfo `Sink` entries advertise support for
/// `mime`. Each entry looks like `http-get:*:audio/flac:*` (the third
/// colon-separated field is the MIME type).
///
/// Matches the exact MIME, a `*` wildcard, and the legacy `x-` variant: many
/// renderers advertise `audio/x-flac` for `audio/flac` (Denon Ceol N12,
/// Marco), and forcing WAV on those wastes bandwidth and loses bit-perfect
/// FLAC the renderer could decode natively.
fn protocol_sink_supports_mime(mime: &str, protocols: &[String]) -> bool {
    let mime_lower = mime.to_lowercase();
    let mime_alt = if let Some(rest) = mime_lower.strip_prefix("audio/x-") {
        format!("audio/{rest}")
    } else if let Some(rest) = mime_lower.strip_prefix("audio/") {
        format!("audio/x-{rest}")
    } else {
        mime_lower.clone()
    };
    for proto in protocols {
        let fields: Vec<&str> = proto.split(':').collect();
        if fields.len() >= 3 {
            let proto_mime = fields[2].trim().to_lowercase();
            if proto_mime == mime_lower || proto_mime == mime_alt || proto_mime == "*" {
                return true;
            }
        }
    }
    false
}

/// Base MIME (third colon-separated field, params stripped) of a Sink entry
/// such as `http-get:*:audio/L16;rate=44100;channels=2:DLNA.ORG_PN=LPCM`.
fn sink_entry_base_mime(entry: &str) -> Option<String> {
    let field = entry.split(':').nth(2)?;
    let mime = field.trim();
    Some(mime.split(';').next().unwrap_or(mime).trim().to_string())
}

/// Choose the MIME spelling to announce in the DIDL / SetAVTransportURI given
/// the renderer's GetProtocolInfo `Sink`.
///
/// Beoplay A9 / Sink audio/x-flac, forum 714: strict renderers (B&O, Lyngdorf)
/// reject SetAVTransportURI with 714 "Illegal MIME-type" when the announced
/// MIME differs from the exact spelling listed in their Sink, even though they
/// can decode the stream. If `desired` is already listed we keep it; if only a
/// known alias is listed (`audio/flac`↔`audio/x-flac`, `audio/mpeg`↔`audio/mp3`,
/// `audio/wav`↔`audio/x-wav`) we announce the spelling the Sink actually lists;
/// otherwise `desired` is returned unchanged (empty/unknown Sink ⇒ previous
/// behaviour, no regression).
fn advertised_mime_for_sink(desired: &str, sink: &[String]) -> String {
    let listed: Vec<String> = sink
        .iter()
        .filter_map(|e| sink_entry_base_mime(e))
        .collect();
    // Already listed verbatim (case-insensitive): announce as-is.
    if listed.iter().any(|b| b.eq_ignore_ascii_case(desired)) {
        return desired.to_string();
    }
    let aliases: &[&str] = match desired.to_lowercase().as_str() {
        "audio/flac" => &["audio/x-flac"],
        "audio/x-flac" => &["audio/flac"],
        "audio/mpeg" => &["audio/mp3"],
        "audio/mp3" => &["audio/mpeg"],
        "audio/wav" => &["audio/x-wav"],
        "audio/x-wav" => &["audio/wav"],
        _ => &[],
    };
    for alias in aliases {
        if let Some(found) = listed.iter().find(|b| b.eq_ignore_ascii_case(alias)) {
            return found.clone();
        }
    }
    desired.to_string()
}

/// Pick a universally-decodable PCM MIME the renderer's `Sink` lists, for the
/// one-shot 714 fallback (Beoplay A9 / forum 714). Prefers WAV, then LPCM
/// (`audio/L16`), returning the exact spelling the Sink uses so the announced
/// MIME passes the renderer's strict Sink check. `None` when the Sink lists no
/// PCM profile.
fn fallback_mime_from_sink(sink: &[String]) -> Option<String> {
    for want in ["audio/wav", "audio/x-wav", "audio/l16"] {
        if let Some(found) = sink
            .iter()
            .filter_map(|e| sink_entry_base_mime(e))
            .find(|b| b.eq_ignore_ascii_case(want))
        {
            return Some(found);
        }
    }
    None
}

fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_string())
}

/// État du transport lu dans une réponse `GetTransportInfo`.
///
/// Extrait de `get_status` pour que les deux régimes de lecture (sondage, et
/// arbitrage d'une contradiction en régime évènementiel) rendent le MÊME
/// verdict sur la même réponse. Deux copies auraient divergé au premier
/// renderer exotique.
///
/// Le test d'inclusion, et son ordre, sont ceux d'avant #2263 : `PAUSED` avant
/// `TRANSITIONING` parce que `PAUSED_PLAYBACK` ne contient pas l'autre, et tout
/// le reste — `STOPPED`, `NO_MEDIA_PRESENT`, une réponse illisible — vaut
/// arrêté.
fn etat_du_transport(transport_resp: &str) -> TransportState {
    if transport_resp.contains("PLAYING") {
        TransportState::Playing
    } else if transport_resp.contains("PAUSED") {
        TransportState::Paused
    } else if transport_resp.contains("TRANSITIONING") {
        TransportState::Transitioning
    } else {
        TransportState::Stopped
    }
}

/// Le renderer a-t-il réellement cessé de jouer, d'après sa réponse
/// `GetTransportInfo` ? Un Stop acquitté n'est pas un Stop appliqué :
/// l'Eversolo répond OK puis met ~1-2 s à s'arrêter, et un
/// SetAVTransportURI envoyé dans cette fenêtre est acquitté… et ignoré
/// (la course des 5 ms, .42, 24/08).
fn arret_effectif(transport_resp: &str) -> bool {
    !transport_resp.contains("PLAYING") && !transport_resp.contains("TRANSITIONING")
}

/// Combien de temps laisser a un renderer qui a ACQUITTE `Play` et ne tient
/// encore AUCUN media (`CurrentURI` vide) pour finir de se reveiller (#2749).
///
/// **Pourquoi 30 s.** Un ampli Denon/HEOS en veille reseau garde sa pile UPnP
/// vivante : il repond 200 a `SetAVTransportURI` puis a `Play` alors qu'il
/// n'est pas sorti de veille, n'a pas bascule sur son entree reseau et ne tient
/// rien. Le releve de terrain (AVR-X1600H, 0.9.121) donne 15 a 30 s entre
/// l'ordre et l'URI reellement posee : une borne PRISE DANS cette plage ne
/// corrigerait qu'une partie des cas, elle doit donc la couvrir en entier.
///
/// ⚠️ **Cette plage n'est PAS une loi, et le message d'echec ne doit plus la
/// citer.** Le meme AVR-X1600H, mesure en 0.9.145 (ticket support 109,
/// #3580) : 3 min 06 s entre le premier clic et la premiere URI tenue, ampli
/// sous tension pendant toute la fenetre ; et 4,9 s quand il vient de jouer.
/// Le testeur a conteste le « 15 a 30 s » affiche, et il avait raison de le
/// faire — c'est un releve fait sur UN appareil dans UN etat, promu en
/// explication generale. La borne reste a 30 s parce que la grace de
/// chargement du sondeur la contraint (ci-dessous), pas parce que 30 s
/// suffiraient : sur cet ampli-la, elles ne suffisent pas.
///
/// **Pourquoi pas plus.** La borne haute n'est pas un gout. `play()` a DEJA
/// bascule la zone en lecture et arme la grace de chargement du sondeur
/// (`TRACK_LOAD_GRACE_SECS` = 45 s, `poller.rs`) : tant que l'attente reste
/// franchement sous cette grace, le sondeur ne peut pas conclure « demarrage
/// mort » pendant qu'on attend encore — une seule instance decide. Sur CE
/// chemin (le `Play` a ete acquitte du premier coup, sinon on a deja rendu
/// `Err`) le reste de `play_media` tient sous ~2 s : ~32 s au pire, sous les
/// 45 s de la grace comme sous le budget HTTP de l'appelant.
const BUDGET_REVEIL_STANDBY: std::time::Duration = std::time::Duration::from_secs(30);
/// Battement entre deux relectures de `CurrentURI` pendant le reveil. Une
/// seconde : le reveil se compte en dizaines de secondes, et chaque lecture est
/// une action SOAP de plus sur un appareil qui demarre.
const CADENCE_REVEIL: std::time::Duration = std::time::Duration::from_secs(1);
/// Intervalle entre deux `SetAVTransportURI` + `Play` de rearmement pendant le
/// reveil. Un HEOS qui finit de demarrer a pu perdre l'URI posee avant son
/// reveil : la reposer periodiquement est ce qui la fait prendre.
const INTERVALLE_RELANCE_REVEIL: std::time::Duration = std::time::Duration::from_secs(8);
/// Ce qu'il doit rester de budget pour qu'une relance vaille d'etre engagee
/// (#3580).
///
/// Une relance n'est pas une lecture : elle repose l'URI, attend le
/// `play_delay` de la zone, puis joue. C'est le travail le plus long du cycle,
/// et il partait sans qu'on regarde le budget — le journal de Reivax66 montre
/// une quatrieme relance a 10:35:39,442 suivie de la SORTIE de la boucle a
/// 10:35:42,647 : trois actions SOAP envoyees a un appareil, dont personne ne
/// lira jamais l'effet, et trois secondes d'attente de plus pour l'auditeur.
///
/// On n'engage donc une relance que s'il reste de quoi en LIRE le resultat.
const RESERVE_DE_RELECTURE_APRES_RELANCE: std::time::Duration = std::time::Duration::from_secs(4);
/// Battement du bareme historique (#2390), quand le renderer tient une AUTRE
/// source. Inchange.
const CADENCE_URI_ETRANGERE: std::time::Duration = std::time::Duration::from_millis(400);
/// Verdict sur l'URI que le renderer dit tenir après notre Play.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UriVerdict {
    /// C'est bien la nôtre : le Play est appliqué.
    Appliquee,
    /// Un flux Tune qui n'est pas le nôtre : le renderer a acquitté toute la
    /// séquence et joue toujours autre chose.
    PasAppliquee,
    /// `CurrentURI` VIDE : le renderer ne tient AUCUN media. Il ne joue pas
    /// autre chose — il n'a RIEN charge. C'est ce que rend un ampli HEOS en
    /// veille reseau dont la pile SOAP repond deja alors que l'appareil se
    /// reveille encore (#2749) ; le confondre avec `PasAppliquee` faisait
    /// couper la zone en 13 s sur un message faux.
    PasEncore,
    /// Une URI étrangère qu'on ne sait pas interpréter (un renderer qui
    /// réécrit, un GetMediaInfo exotique) : on ne conclut rien.
    Indeterminee,
}

/// La partie discriminante de l'URL d'un flux : son chemin (`/stream/…`).
/// L'hôte peut différer entre ce qu'on envoie et ce que le renderer
/// rapporte (résolution DNS, réécriture d'IP) — le chemin, lui, est unique.
fn chemin_du_flux(url: &str) -> &str {
    url.strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .and_then(|reste| reste.find('/').map(|i| &reste[i..]))
        .unwrap_or(url)
}

fn verdict_uri_appliquee(current_uri: Option<&str>, url_attendue: &str) -> UriVerdict {
    let Some(uri) = current_uri else {
        return UriVerdict::Indeterminee;
    };
    let uri = uri.trim();
    if uri.is_empty() {
        // #2749 — VIDE veut dire « aucun media », pas « un autre media ».
        return UriVerdict::PasEncore;
    }
    if uri.contains(chemin_du_flux(url_attendue)) {
        return UriVerdict::Appliquee;
    }
    if uri.contains("/stream/") {
        // Un flux Tune — le périmé d'avant notre Play, ou celui d'un autre
        // serveur : dans les deux cas, PAS ce qu'on vient d'envoyer.
        return UriVerdict::PasAppliquee;
    }
    UriVerdict::Indeterminee
}

/// Ce que la verification d'apres-`Play` a etabli.
#[derive(Debug, Clone)]
struct VerifUri {
    verdict: UriVerdict,
    /// La derniere `CurrentURI` lue.
    uri_tenue: Option<String>,
    /// Le refus SOAP rendu par le `Play` d'une relance, s'il y en a eu un.
    refus_relance: Option<String>,
    /// L'appareil a repondu au moins une fois, PUIS s'est tu. C'est la
    /// difference entre « il se reveille encore » et « il n'est plus la »
    /// (#2749) : un renderer qui n'implemente pas `GetMediaInfo` echoue des la
    /// PREMIERE lecture et ne met donc jamais ce drapeau — il ne bloque rien,
    /// exactement comme avant.
    soap_muet: bool,
    /// Duree totale de la verification, en ms — le chiffre que le journal doit
    /// porter pour qu'on puisse lire un vrai temps de reveil.
    attente_ms: u64,
    /// Nombre de `SetAVTransportURI` + `Play` de relance envoyes.
    relances: u32,
}

/// La verification d'apres-`Play`, en DEUX temps.
///
/// **Temps 1 — bareme historique (#2390), inchange.** Trois lectures de
/// `CurrentURI` espacees de 400 ms, une relance complete, trois lectures de
/// plus. C'est ce qui rattrape la course des 5 ms de l'Eversolo, et c'est ce
/// qui fait echouer un renderer zombie (#2394) : il tient un flux Tune perime,
/// verdict `PasAppliquee`, on n'attend rien de plus.
///
/// **Temps 2 — fenetre de reveil (#2749), NOUVEAU.** On n'y entre que si le
/// temps 1 se termine sur `PasEncore` : `CurrentURI` VIDE, donc aucun media
/// tenu, donc rien a quoi notre flux se disputerait la place. On relit alors a
/// `CADENCE_REVEIL` et on repose l'URI toutes les `INTERVALLE_RELANCE_REVEIL`,
/// jusqu'a `budget_reveil`.
///
/// La condition « tant que SOAP repond » est EFFECTIVE : des que `lire_uri`
/// rend `Err` — l'appareil a ete eteint ou debranche pendant l'attente — on
/// sort immediatement au lieu de bruler le budget. Un refus SOAP sur le `Play`
/// d'une relance sort aussi : un appareil qui REFUSE la transition nomme un
/// etat (701), il ne dort pas.
///
/// Les deux actions sont passees en parametres pour que cette boucle-ci — celle
/// de production — soit eprouvable sans renderer.
async fn verifier_uri_appliquee<L, LF, R, RF>(
    url_attendue: &str,
    budget_reveil: std::time::Duration,
    mut lire_uri: L,
    mut relancer: R,
) -> VerifUri
where
    L: FnMut() -> LF,
    LF: std::future::Future<Output = Result<Option<String>, String>>,
    R: FnMut() -> RF,
    RF: std::future::Future<Output = Option<String>>,
{
    let debut = tokio::time::Instant::now();
    let mut v = VerifUri {
        verdict: UriVerdict::Indeterminee,
        uri_tenue: None,
        refus_relance: None,
        soap_muet: false,
        attente_ms: 0,
        relances: 0,
    };
    let mut une_lecture_a_repondu = false;

    // ── Temps 1 : le bareme de #2390, au battement pres. ──────────────────
    'bareme: for relance in 0..2u32 {
        for essai in 0..3u32 {
            match lire_uri().await {
                Ok(uri) => {
                    une_lecture_a_repondu = true;
                    v.uri_tenue = uri.clone();
                    v.verdict = verdict_uri_appliquee(uri.as_deref(), url_attendue);
                }
                // Un renderer sans GetMediaInfo ne doit rien bloquer : si RIEN
                // n'a jamais repondu, on ne conclut rien. S'il avait repondu et
                // se tait maintenant, c'est un appareil qui a disparu.
                Err(_) => {
                    v.soap_muet = une_lecture_a_repondu;
                    break 'bareme;
                }
            }
            match v.verdict {
                UriVerdict::Appliquee | UriVerdict::Indeterminee => break 'bareme,
                UriVerdict::PasAppliquee | UriVerdict::PasEncore if essai < 2 => {
                    tokio::time::sleep(CADENCE_URI_ETRANGERE).await;
                }
                _ => {}
            }
        }
        if relance == 0 {
            v.relances += 1;
            v.refus_relance = relancer().await;
        }
    }

    // ── Temps 2 : la fenetre de reveil. ───────────────────────────────────
    if v.verdict == UriVerdict::PasEncore && v.refus_relance.is_none() && !v.soap_muet {
        // #3580 — LA BORNE COMPTE LE TEMPS DEJA PASSE.
        //
        // Elle ne comptait que ses propres sommeils : le temps 1 (six actions
        // SOAP et une relance complete) s'ajoutait ENTIEREMENT a la fenetre,
        // et chaque tour engageait une lecture — puis parfois une relance —
        // apres le dernier controle. Sous `start_paused`, ou une action SOAP
        // ne coute rien, la borne tenait ses ~32 s et l'epreuve restait verte ;
        // sur le reseau de Reivax66 elle a rendu `attente_ms=41290` pour un
        // budget de 30 s (ticket support 78, #3580). La marge sous la grace de
        // chargement du sondeur (`TRACK_LOAD_GRACE_SECS` = 45 s), qui existe
        // pour qu'UNE SEULE instance decide, tombait de 13 s a 3,7 s — et une
        // zone reglee avec un `play_delay` la franchit.
        let mut derniere_relance = tokio::time::Instant::now();
        while debut.elapsed() < budget_reveil {
            tokio::time::sleep(CADENCE_REVEIL).await;
            // Le sommeil a pu consommer ce qui restait : on n'engage pas une
            // action SOAP de plus hors du budget.
            if debut.elapsed() >= budget_reveil {
                break;
            }
            match lire_uri().await {
                Ok(uri) => {
                    v.uri_tenue = uri.clone();
                    v.verdict = verdict_uri_appliquee(uri.as_deref(), url_attendue);
                }
                Err(_) => {
                    // « Tant que SOAP repond » : il ne repond plus. On rend la
                    // main TOUT DE SUITE — faire patienter 30 s devant un
                    // appareil debranche serait un second defaut.
                    v.soap_muet = true;
                    break;
                }
            }
            if v.verdict != UriVerdict::PasEncore {
                break;
            }
            if derniere_relance.elapsed() >= INTERVALLE_RELANCE_REVEIL
                && debut.elapsed() + RESERVE_DE_RELECTURE_APRES_RELANCE <= budget_reveil
            {
                derniere_relance = tokio::time::Instant::now();
                v.relances += 1;
                v.refus_relance = relancer().await;
                if v.refus_relance.is_some() {
                    break;
                }
            }
        }
    }

    v.attente_ms = debut.elapsed().as_millis() as u64;
    v
}

/// Un refus SOAP portant le code UPnP **701 « Transition not available »**.
/// Ce n'est pas une panne : le renderer refuse LA TRANSITION à cet instant.
fn est_701(reponse_play: &str) -> bool {
    reponse_play.contains(">701<")
        || reponse_play
            .to_ascii_lowercase()
            .contains("transition not available")
}

/// Ce qu'il faut envoyer — ou ne pas envoyer — avant de redemander `Play`
/// après un refus.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum RepriseApresRefus {
    /// Barème historique : au PREMIER essai, un Stop puis un Play (écrit pour
    /// le Revox S100 et son 501). Conduite par défaut, inchangée.
    StopPuisPlay,
    /// 701 alors que le transport charge encore l'URI : le laisser finir. Un
    /// Stop ici le ferait retomber et rendrait le 701 suivant certain.
    Attendre,
    /// 701 alors que le transport ne tient plus de média : sans réarmement de
    /// l'URI, chaque `Play` suivant est un 701 de plus. C'est ce que montre le
    /// journal de FabienM (#2581) — cinq refus, puis un succès immédiat dès
    /// qu'un `SetAVTransportURI` est rejoué.
    ReArmerUri,
}

/// Décide de la reprise à partir du refus reçu et de l'état que le transport
/// déclare (`CurrentTransportState`). On ne dévie du barème historique que sur
/// une information POSITIVE : un renderer muet, ou qui n'a pas
/// `GetTransportInfo`, garde exactement l'ancienne conduite.
fn reprise_apres_refus_play(reponse_play: &str, etat_transport: Option<&str>) -> RepriseApresRefus {
    if !est_701(reponse_play) {
        return RepriseApresRefus::StopPuisPlay;
    }
    match etat_transport.map(|e| e.trim().to_ascii_uppercase()) {
        Some(e) if e.contains("TRANSITIONING") => RepriseApresRefus::Attendre,
        Some(e) if e.contains("NO_MEDIA_PRESENT") || e.contains("STOPPED") => {
            RepriseApresRefus::ReArmerUri
        }
        _ => RepriseApresRefus::StopPuisPlay,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// La faute SOAP EXACTE relevée dans le journal de FabienM (#2581).
    const FAUTE_701: &str = concat!(
        "<s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring>",
        "<detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\">",
        "<errorCode>701</errorCode>",
        "<errorDescription>Transition not available</errorDescription>",
        "</UPnPError></detail></s:Fault>"
    );

    /// 501 Action Failed — le refus pour lequel le barème Stop+Play a été écrit
    /// (Revox S100). Il ne doit RIEN changer de son comportement.
    const FAUTE_501: &str = "<UPnPError><errorCode>501</errorCode><errorDescription>Action Failed</errorDescription></UPnPError>";

    #[test]
    fn le_701_se_reconnait_au_code_comme_au_libelle() {
        assert!(est_701(FAUTE_701));
        assert!(est_701(
            "<errorDescription>Transition not available</errorDescription>"
        ));
        // Un 7010 n'est pas un 701, et les autres codes du fichier non plus.
        assert!(!est_701("<errorCode>7010</errorCode>"));
        assert!(!est_701(FAUTE_501));
        assert!(!est_701("<errorCode>714</errorCode>"));
    }

    /// #2581 — le renderer charge encore l'URI : lui envoyer le Stop du barème
    /// le ferait retomber. On attend.
    #[test]
    fn un_701_pendant_le_chargement_fait_attendre_sans_stop() {
        assert_eq!(
            reprise_apres_refus_play(FAUTE_701, Some("TRANSITIONING")),
            RepriseApresRefus::Attendre
        );
    }

    /// #2581 — le transport ne tient plus de média : sans réarmement de l'URI,
    /// les cinq tentatives sont cinq 701 d'avance.
    #[test]
    fn un_701_sans_media_rearme_l_uri() {
        for etat in ["NO_MEDIA_PRESENT", "STOPPED", " no_media_present "] {
            assert_eq!(
                reprise_apres_refus_play(FAUTE_701, Some(etat)),
                RepriseApresRefus::ReArmerUri,
                "état {etat}"
            );
        }
    }

    /// Zéro régression : un renderer qui ne dit rien d'exploitable garde le
    /// barème historique, au mot près.
    #[test]
    fn un_701_muet_ne_devie_pas_du_bareme_historique() {
        for etat in [None, Some(""), Some("PLAYING"), Some("RECORDING")] {
            assert_eq!(
                reprise_apres_refus_play(FAUTE_701, etat),
                RepriseApresRefus::StopPuisPlay,
                "état {etat:?}"
            );
        }
    }

    /// Zéro régression : le 501 du Revox garde son Stop+Play, QUEL QUE SOIT
    /// l'état déclaré par le transport.
    #[test]
    fn un_refus_qui_n_est_pas_un_701_garde_le_stop_du_revox() {
        for etat in [
            None,
            Some("TRANSITIONING"),
            Some("NO_MEDIA_PRESENT"),
            Some("STOPPED"),
        ] {
            assert_eq!(
                reprise_apres_refus_play(FAUTE_501, etat),
                RepriseApresRefus::StopPuisPlay,
                "état {etat:?}"
            );
        }
    }

    /// L'état lu dans la boucle vient d'un `GetTransportInfo` complet : le
    /// chaînage extraction → décision doit tenir sur la réponse RÉELLE.
    #[test]
    fn l_etat_se_lit_dans_la_reponse_get_transport_info() {
        let reponse = concat!(
            "<u:GetTransportInfoResponse>",
            "<CurrentTransportState>NO_MEDIA_PRESENT</CurrentTransportState>",
            "<CurrentTransportStatus>OK</CurrentTransportStatus>",
            "<CurrentSpeed>1</CurrentSpeed>",
            "</u:GetTransportInfoResponse>"
        );
        let etat = extract_tag(reponse, "CurrentTransportState");
        assert_eq!(etat.as_deref(), Some("NO_MEDIA_PRESENT"));
        assert_eq!(
            reprise_apres_refus_play(FAUTE_701, etat.as_deref()),
            RepriseApresRefus::ReArmerUri
        );
    }

    #[test]
    fn caps_from_sink_maps_advertised_formats() {
        // A typical hi-fi renderer Sink: FLAC (x- variant), 16-bit LPCM, MP3,
        // AAC/MP4, and DSF — but NOT 24-bit LPCM.
        let sink = vec![
            "http-get:*:audio/x-flac:DLNA.ORG_PN=FLAC".to_string(),
            "http-get:*:audio/L16;rate=44100;channels=2:DLNA.ORG_PN=LPCM".to_string(),
            "http-get:*:audio/mpeg:DLNA.ORG_PN=MP3".to_string(),
            "http-get:*:audio/mp4:*".to_string(),
            "http-get:*:audio/x-dsf:*".to_string(),
        ];
        let c = renderer_caps_from_sink(sink);
        assert!(c.probed);
        assert!(c.flac, "x-flac must count as FLAC");
        assert!(c.lpcm16, "audio/L16 present");
        assert!(!c.lpcm24, "no audio/L24 advertised");
        assert!(c.mp3 && c.aac && c.dsd);
        assert_eq!(c.reason, None, "une sonde concluante ne porte aucun refus");
    }

    #[test]
    fn une_sonde_inconclusive_expose_une_raison_stable() {
        let c = RendererCapabilities::inconclusive("empty_sink");
        let json = serde_json::to_value(c).unwrap();

        assert_eq!(json["probed"], false);
        assert_eq!(json["reason"], "empty_sink");
    }

    #[test]
    fn parse_dsd_capability_keeps_the_renderer_own_mime() {
        // Yamaha R-N2000A-shaped Sink: the renderer advertises its own spelling
        // of the DSD MIME, and we must serve that one back rather than the
        // generic application/x-dsd (cf. the passthrough path in orchestrator).
        let sink = vec![
            "http-get:*:audio/L16;rate=44100;channels=2:*".to_string(),
            "http-get:*:audio/dsf:*".to_string(),
        ];
        let cap = parse_dsd_capability(&sink);
        assert!(cap.supports_dsf);
        assert!(!cap.supports_dff);
        assert_eq!(cap.dsf_mime.as_deref(), Some("audio/dsf"));
    }

    #[test]
    fn parse_dsd_capability_reports_no_dsd_for_a_pcm_only_sink() {
        // A conclusive negative — distinct from a failed probe, which never
        // reaches this function (probe_dsd_support returns None instead).
        let sink = vec![
            "http-get:*:audio/mpeg:*".to_string(),
            "http-get:*:audio/L16;rate=44100;channels=2:*".to_string(),
        ];
        let cap = parse_dsd_capability(&sink);
        assert!(!cap.supports_dsf);
        assert!(!cap.supports_dff);
        assert_eq!(cap.dsf_mime, None);
    }

    #[test]
    fn parse_dsd_capability_detects_dff_and_x_dsd_variants() {
        let sink = vec![
            "http-get:*:audio/x-dsd:*".to_string(),
            "http-get:*:audio/x-dff:*".to_string(),
        ];
        let cap = parse_dsd_capability(&sink);
        assert!(cap.supports_dsf, "x-dsd counts as DSD");
        assert!(cap.supports_dff);
        assert_eq!(cap.dsf_mime.as_deref(), Some("audio/x-dsd"));
    }

    #[test]
    fn caps_from_sink_flags_l24_when_present() {
        let sink = vec!["http-get:*:audio/L24;rate=96000;channels=2:*".to_string()];
        let c = renderer_caps_from_sink(sink);
        assert!(c.lpcm24, "audio/L24 gates the WAV 24-bit override");
        assert!(!c.flac);
    }

    #[test]
    fn protocol_sink_matches_x_flac_variant() {
        // Denon Ceol N12 (Marco) advertises FLAC as `audio/x-flac`. Asking for
        // `audio/flac` must match it so we passthrough instead of forcing WAV.
        let sink = vec![
            "http-get:*:audio/x-flac:DLNA.ORG_PN=FLAC".to_string(),
            "http-get:*:audio/mpeg:*".to_string(),
        ];
        assert!(protocol_sink_supports_mime("audio/flac", &sink));
        assert!(protocol_sink_supports_mime("audio/x-flac", &sink));
        // Exact and wildcard still work; an unadvertised format is rejected.
        assert!(protocol_sink_supports_mime("audio/mpeg", &sink));
        assert!(!protocol_sink_supports_mime("audio/aac", &sink));
        assert!(protocol_sink_supports_mime(
            "audio/flac",
            &["http-get:*:*:*".to_string()]
        ));
    }

    #[test]
    fn advertised_mime_rewrites_flac_to_sink_x_flac() {
        // Beoplay A9 (forum 714): Sink lists audio/x-flac but NOT audio/flac.
        // We must announce the exact spelling the Sink lists, else 714.
        let sink = vec![
            "http-get:*:audio/x-flac:DLNA.ORG_PN=FLAC".to_string(),
            "http-get:*:audio/wav:*".to_string(),
            "http-get:*:audio/L16;rate=44100;channels=2:DLNA.ORG_PN=LPCM".to_string(),
        ];
        assert_eq!(
            advertised_mime_for_sink("audio/flac", &sink),
            "audio/x-flac"
        );
    }

    #[test]
    fn advertised_mime_keeps_exact_sink_spelling() {
        // Sink lists audio/flac verbatim → announce it unchanged.
        let sink = vec!["http-get:*:audio/flac:DLNA.ORG_PN=FLAC".to_string()];
        assert_eq!(advertised_mime_for_sink("audio/flac", &sink), "audio/flac");
    }

    #[test]
    fn advertised_mime_unchanged_when_alias_absent() {
        // Neither audio/flac nor audio/x-flac listed, and empty Sink → keep
        // the desired MIME unchanged (previous behaviour, no regression).
        let sink = vec!["http-get:*:audio/mpeg:*".to_string()];
        assert_eq!(advertised_mime_for_sink("audio/flac", &sink), "audio/flac");
        assert_eq!(advertised_mime_for_sink("audio/flac", &[]), "audio/flac");
    }

    #[test]
    fn advertised_mime_rewrites_mpeg_mp3_alias() {
        let sink = vec!["http-get:*:audio/mp3:*".to_string()];
        assert_eq!(advertised_mime_for_sink("audio/mpeg", &sink), "audio/mp3");
    }

    #[test]
    fn fallback_mime_prefers_wav_then_l16() {
        let sink = vec![
            "http-get:*:audio/x-flac:*".to_string(),
            "http-get:*:audio/wav:*".to_string(),
            "http-get:*:audio/L16;rate=44100;channels=2:*".to_string(),
        ];
        assert_eq!(fallback_mime_from_sink(&sink).as_deref(), Some("audio/wav"));

        let sink_l16 = vec!["http-get:*:audio/L16;rate=44100;channels=2:*".to_string()];
        assert_eq!(
            fallback_mime_from_sink(&sink_l16).as_deref(),
            Some("audio/L16")
        );

        let sink_none = vec!["http-get:*:audio/x-flac:*".to_string()];
        assert_eq!(fallback_mime_from_sink(&sink_none), None);
    }

    #[test]
    fn parse_time_works() {
        assert_eq!(DlnaOutput::parse_time("0:03:45"), 225_000);
        assert_eq!(DlnaOutput::parse_time("1:00:00"), 3_600_000);
        assert_eq!(DlnaOutput::parse_time("0:00:00.000"), 0);
    }

    #[test]
    fn parse_time_fractional_seconds() {
        assert_eq!(DlnaOutput::parse_time("0:04:16.487"), 256_487);
        assert_eq!(DlnaOutput::parse_time("0:03:46.5"), 226_500);
        assert_eq!(DlnaOutput::parse_time("0:03:46.50"), 226_500);
        assert_eq!(DlnaOutput::parse_time("0:03:46.500"), 226_500);
        assert_eq!(DlnaOutput::parse_time("0:00:01.1"), 1_100);
        assert_eq!(DlnaOutput::parse_time("0:00:01.12"), 1_120);
        assert_eq!(DlnaOutput::parse_time("0:00:01.123"), 1_123);
    }

    #[test]
    fn format_time_works() {
        assert_eq!(DlnaOutput::format_time(225_000), "0:03:45");
        assert_eq!(DlnaOutput::format_time(3_600_000), "1:00:00");
    }

    /// La course des 5 ms (.42, 24/08) : un Stop acquitté n'est pas appliqué.
    #[test]
    fn arret_effectif_lit_l_etat_du_transport() {
        assert!(!arret_effectif(
            "<CurrentTransportState>PLAYING</CurrentTransportState>"
        ));
        assert!(!arret_effectif(
            "<CurrentTransportState>TRANSITIONING</CurrentTransportState>"
        ));
        assert!(arret_effectif(
            "<CurrentTransportState>STOPPED</CurrentTransportState>"
        ));
        assert!(arret_effectif(
            "<CurrentTransportState>NO_MEDIA_PRESENT</CurrentTransportState>"
        ));
        // PAUSED_PLAYBACK : le transport n'avance plus, l'URI peut changer.
        assert!(arret_effectif(
            "<CurrentTransportState>PAUSED_PLAYBACK</CurrentTransportState>"
        ));
    }

    #[test]
    fn verdict_uri_notre_flux_est_applique() {
        let url = "http://192.168.1.42:8888/stream/abc-123.wav";
        assert_eq!(verdict_uri_appliquee(Some(url), url), UriVerdict::Appliquee);
        // L'hôte peut différer (IP réécrite) — le chemin suffit.
        assert_eq!(
            verdict_uri_appliquee(Some("http://tune.local:8888/stream/abc-123.wav"), url),
            UriVerdict::Appliquee
        );
    }

    /// **Ce test a CHANGE de conclusion sur un point, et un seul : l'URI
    /// VIDE.** Il epinglait `Some("") => PasAppliquee`, c'est-a-dire « le
    /// renderer joue une autre source ». C'etait faux, et c'est la cause de
    /// #2749 : un Denon/HEOS en veille reseau acquitte `Play` puis rend une
    /// `CurrentURI` vide parce qu'il n'a RIEN charge — pas parce qu'il joue
    /// autre chose. Tune coupait la zone en 13 s sur ce contresens.
    ///
    /// Le vide vaut desormais `PasEncore` (voir `UriVerdict`). Le reste du test
    /// est INTACT : un flux Tune perime, ou celui d'un autre serveur, reste
    /// `PasAppliquee` — c'est ce qui fait echouer le zombie du DMP-A8 (#2394),
    /// et ce verdict-la ne bouge pas d'un pouce.
    #[test]
    fn verdict_uri_vide_ou_flux_perime_n_est_pas_applique() {
        let url = "http://192.168.1.42:8888/stream/abc-123.wav";
        // Vide = AUCUN media tenu : pas encore applique (#2749), pas « autre
        // source ». Les espaces seuls comptent pour du vide, `trim` oblige.
        assert_eq!(verdict_uri_appliquee(Some(""), url), UriVerdict::PasEncore);
        assert_eq!(
            verdict_uri_appliquee(Some("   "), url),
            UriVerdict::PasEncore
        );
        // L'Eversolo qui garde l'URI d'avant : un AUTRE flux Tune.
        assert_eq!(
            verdict_uri_appliquee(Some("http://192.168.1.42:8888/stream/vieux-flux.flac"), url),
            UriVerdict::PasAppliquee
        );
        // Le flux d'un AUTRE serveur Tune : pas le nôtre non plus.
        assert_eq!(
            verdict_uri_appliquee(Some("http://192.168.1.18:8888/stream/xyz.wav"), url),
            UriVerdict::PasAppliquee
        );
    }

    #[test]
    fn verdict_uri_etrangere_ou_absente_ne_conclut_rien() {
        let url = "http://192.168.1.42:8888/stream/abc-123.wav";
        // Un renderer qui réécrit (Sonos et ses URI propriétaires) ne doit
        // JAMAIS être déclaré en échec sur cette seule base.
        assert_eq!(
            verdict_uri_appliquee(Some("x-rincon-queue:RINCON_123#0"), url),
            UriVerdict::Indeterminee
        );
        assert_eq!(verdict_uri_appliquee(None, url), UriVerdict::Indeterminee);
    }

    // ─────────────────────────────────────────────────────────────────────
    // #2749 — un renderer EN VEILLE n'est pas un renderer qui joue autre chose
    //
    // Tout ce qui suit fait tourner `verifier_uri_appliquee`, LA boucle que
    // `play_media` appelle. Un banc qui retranscrirait le mecanisme resterait
    // vert pendant que le code de production se degrade (mesure le 01/09) :
    // c'est bien la fonction branchee qui est eprouvee ici, avec ses vraies
    // constantes, sous `start_paused` pour qu'une attente de 30 s ne coute
    // rien.
    // ─────────────────────────────────────────────────────────────────────

    /// Le flux que Tune vient d'envoyer — la radio FIP du releve.
    const URL_2749: &str = "http://192.168.1.18:8888/stream/fip-2749.wav";
    /// Un flux Tune PERIME, celui qu'un renderer zombie ne lache pas (#2394).
    const URI_PERIMEE_2749: &str = "http://192.168.1.42:8888/stream/vieux-flux.flac";

    /// Fait tourner la boucle de production contre un renderer SCRIPTE :
    /// chaque element de `reponses` est ce que rend le `GetMediaInfo` suivant,
    /// la derniere reponse etant rejouee indefiniment — un appareil ne change
    /// pas d'avis tout seul. `refus_relance` est ce que rend le `Play` de
    /// chaque relance.
    ///
    /// Rend le verdict, le nombre de lectures et le nombre de relances.
    async fn eprouver_2749(
        reponses: Vec<Result<Option<&'static str>, &'static str>>,
        refus_relance: Option<&'static str>,
    ) -> (VerifUri, u32, u32) {
        // Sans cout : le banc historique, au battement pres.
        let (v, journal) = eprouver_avec_couts(
            reponses,
            refus_relance,
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
        )
        .await;
        let lectures = journal
            .iter()
            .filter(|a| **a == ActionSoap::Lecture)
            .count() as u32;
        let relances = journal
            .iter()
            .filter(|a| **a == ActionSoap::Relance)
            .count() as u32;
        (v, lectures, relances)
    }
    /// Ce que le renderer a reellement recu, dans l'ORDRE.
    ///
    /// Le compte seul ne dit pas si la derniere action envoyee a l'appareil
    /// est une relance dont personne ne lira l'effet — c'est precisement ce
    /// que le journal de Reivax66 montre, et un compteur ne peut pas le voir.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ActionSoap {
        Lecture,
        Relance,
    }
    /// Le meme banc, mais ou chaque action SOAP COUTE du temps.
    ///
    /// C'est l'angle mort de tout ce qui precede : sous `start_paused`, une
    /// lecture et une relance sont gratuites, si bien que la borne semblait
    /// tenir ses ~32 s. Sur un vrai reseau elles ne le sont pas, et c'est de
    /// la que viennent les 41 290 ms du releve.
    async fn eprouver_avec_couts(
        reponses: Vec<Result<Option<&'static str>, &'static str>>,
        refus_relance: Option<&'static str>,
        cout_lecture: std::time::Duration,
        cout_relance: std::time::Duration,
    ) -> (VerifUri, Vec<ActionSoap>) {
        let file: std::cell::RefCell<std::collections::VecDeque<_>> =
            std::cell::RefCell::new(reponses.into_iter().collect());
        let derniere: std::cell::RefCell<Result<Option<&'static str>, &'static str>> =
            std::cell::RefCell::new(Ok(None));
        let journal: std::cell::RefCell<Vec<ActionSoap>> = std::cell::RefCell::new(Vec::new());
        let (file_r, derniere_r, journal_r) = (&file, &derniere, &journal);
        let v = verifier_uri_appliquee(
            URL_2749,
            BUDGET_REVEIL_STANDBY,
            || async move {
                journal_r.borrow_mut().push(ActionSoap::Lecture);
                if !cout_lecture.is_zero() {
                    tokio::time::sleep(cout_lecture).await;
                }
                if let Some(r) = file_r.borrow_mut().pop_front() {
                    *derniere_r.borrow_mut() = r;
                }
                let r = *derniere_r.borrow();
                r.map(|u| u.map(str::to_string)).map_err(str::to_string)
            },
            || async move {
                journal_r.borrow_mut().push(ActionSoap::Relance);
                if !cout_relance.is_zero() {
                    tokio::time::sleep(cout_relance).await;
                }
                refus_relance.map(str::to_string)
            },
        )
        .await;
        let journal = journal.into_inner();
        (v, journal)
    }
    /// Le cout d'un aller-retour SOAP sur le reseau de Reivax66.
    const COUT_LECTURE_3580: std::time::Duration = std::time::Duration::from_millis(400);
    /// Le cout d'une relance : `SetAVTransportURI`, le `play_delay` de la zone,
    /// puis `Play`. Trois secondes est ce que rend une zone reglee avec un
    /// delai de pose, cas courant sur les amplis lents.
    const COUT_RELANCE_3580: std::time::Duration = std::time::Duration::from_millis(3_000);
    /// #3580 — L'ATTENTE MUETTE DOIT RESTER SOUS SA PROPRE BORNE.
    ///
    /// Reivax66, ticket support 78 : `dlna_play_uri_restee_vide attente_ms=41290
    /// relances=4 soap_muet=false`, pour un `BUDGET_REVEIL_STANDBY` de 30 s.
    /// Le commentaire de la borne affirme pourtant « ~32 s au pire, sous les
    /// 45 s de la grace comme sous le budget HTTP de l'appelant », et
    /// `i2749_une_veille_qui_n_aboutit_pas_echoue_a_la_borne` le certifiait —
    /// en ne facturant AUCUN temps aux actions SOAP.
    ///
    /// Ce banc-ci leur en facture. Il fait tourner la MEME boucle de
    /// production, avec les memes constantes, et mesure ce que l'auditeur
    /// attend vraiment.
    #[tokio::test(start_paused = true)]
    async fn i3580_l_attente_reste_sous_sa_borne_quand_le_soap_coute_du_temps() {
        let (v, _journal) = eprouver_avec_couts(
            vec![Ok(Some(""))],
            None,
            COUT_LECTURE_3580,
            COUT_RELANCE_3580,
        )
        .await;
        assert_eq!(
            v.verdict,
            UriVerdict::PasEncore,
            "le cas mesure est bien celui du Denon : il repond, et ne tient rien"
        );
        assert!(
            v.attente_ms >= 25_000,
            "raccourcir l'attente ne corrigerait pas le defaut, il en creerait              un autre : un ampli qui met 25 s a sortir de veille doit encore              etre rattrape — {} ms",
            v.attente_ms
        );
        assert!(
            v.attente_ms <= 33_000,
            "la borne ne compte que ses propres sommeils : le bareme du temps 1              et chaque action SOAP engagee apres le dernier controle s'y              ajoutent. Reivax66 a mesure 41 290 ms pour un budget de 30 s, la              ou le code annonce « ~32 s au pire, sous les 45 s de la grace du              sondeur » (#3580) — mesure : {} ms",
            v.attente_ms
        );
    }
    /// #3580 — LA DERNIERE ACTION ENVOYEE N'EST JAMAIS UNE RELANCE.
    ///
    /// Le releve montre une quatrieme relance a 10:35:39,442 et la sortie de
    /// la boucle a 10:35:42,647 : `SetAVTransportURI`, `play_delay`, `Play` —
    /// trois actions poussees dans un appareil dont on ne relira jamais la
    /// `CurrentURI`. Elles ne peuvent rien corriger par construction, et elles
    /// coutent leur duree a l'auditeur.
    #[tokio::test(start_paused = true)]
    async fn i3580_aucune_relance_n_est_la_derniere_action_envoyee() {
        let (_v, journal) = eprouver_avec_couts(
            vec![Ok(Some(""))],
            None,
            COUT_LECTURE_3580,
            COUT_RELANCE_3580,
        )
        .await;
        assert!(
            journal.len() > 6,
            "la fenetre de reveil doit s'etre ouverte : {journal:?}"
        );
        assert_eq!(
            journal.last(),
            Some(&ActionSoap::Lecture),
            "une relance dont on ne lit jamais l'effet est une action SOAP pour              rien, et de l'attente en plus (#3580) — enchainement : {journal:?}"
        );
        // Et chaque relance doit etre SUIVIE d'au moins une lecture.
        for (i, action) in journal.iter().enumerate() {
            if *action == ActionSoap::Relance {
                assert!(
                    journal[i + 1..].contains(&ActionSoap::Lecture),
                    "la relance en position {i} n'est jamais relue : {journal:?}"
                );
            }
        }
    }
    /// Temoin — sans cout, le banc historique rend exactement ce qu'il rendait.
    /// Si celui-ci rougit, c'est le banc qui a bouge, pas la borne.
    #[tokio::test(start_paused = true)]
    async fn i3580_le_banc_sans_cout_ne_change_pas_de_sens() {
        let (v, lectures, relances) = eprouver_2749(vec![Ok(Some(""))], None).await;
        assert_eq!(v.verdict, UriVerdict::PasEncore);
        assert!(lectures >= 6, "le bareme du temps 1 fait six lectures");
        assert!(relances >= 3, "l'URI est reposee pendant l'attente");
    }

    /// TEMOIN 1 — `CurrentURI` vide, puis la BONNE URI : la zone joue.
    ///
    /// L'AVR-X1600H en veille reseau acquitte tout et ne tient rien pendant une
    /// vingtaine de secondes, puis pose enfin l'URI. Avant #2749, Tune coupait
    /// la zone au bout de 13,5 s en affirmant qu'il « jouait une autre
    /// source ».
    #[tokio::test(start_paused = true)]
    async fn i2749_uri_vide_puis_reveil_la_zone_n_est_pas_coupee() {
        let mut reponses: Vec<Result<Option<&'static str>, &'static str>> = vec![Ok(Some("")); 25];
        reponses.push(Ok(Some(URL_2749)));
        let (v, lectures, relances) = eprouver_2749(reponses, None).await;
        assert_eq!(
            v.verdict,
            UriVerdict::Appliquee,
            "l'ampli a fini par poser NOTRE URI : la lecture doit aboutir, \
             pas etre coupee ({} lectures, {} ms)",
            lectures,
            v.attente_ms
        );
        assert!(!v.soap_muet, "l'appareil a repondu tout du long");
        assert!(
            v.attente_ms > 15_000 && v.attente_ms < 30_000,
            "le reveil observe sur HEOS tient entre 15 et 30 s : {} ms",
            v.attente_ms
        );
        assert!(
            relances >= 2,
            "l'URI doit etre REPOSEE plusieurs fois pendant l'attente, pas une \
             seule comme dans le bareme de #2390 : {relances}"
        );
    }

    /// TEMOIN 2 — une AUTRE URI reellement tenue : rien ne change.
    ///
    /// C'est le zombie du DMP-A8 (#2394), meme journal
    /// `dlna_play_jamais_applique` mais cause appareil : il tient un flux Tune
    /// PERIME et ne le lachera pas. Le correctif de #2749 ne doit RIEN lui
    /// offrir — ni attente, ni relance de plus, ni message adouci.
    #[tokio::test(start_paused = true)]
    async fn i2749_une_autre_source_reellement_tenue_echoue_comme_avant() {
        let (v, lectures, relances) = eprouver_2749(vec![Ok(Some(URI_PERIMEE_2749))], None).await;
        assert_eq!(
            v.verdict,
            UriVerdict::PasAppliquee,
            "un flux Tune perime reste « pas applique » : le zombie doit \
             continuer d'echouer"
        );
        assert_eq!(v.uri_tenue.as_deref(), Some(URI_PERIMEE_2749));
        assert_eq!(
            lectures, 6,
            "le bareme de #2390 fait SIX lectures, pas une de plus"
        );
        assert_eq!(relances, 1, "UNE relance, exactement comme avant #2749");
        assert!(
            v.attente_ms < 3_000,
            "aucune fenetre de reveil ne doit s'ouvrir sur une AUTRE source : {} ms",
            v.attente_ms
        );
    }

    /// TEMOIN 3 — `CurrentURI` reste vide ET SOAP cesse de repondre :
    /// on abandonne, sans attendre la borne complete.
    ///
    /// C'est la condition « tant que SOAP repond » du rapporteur, rendue
    /// EFFECTIVE : un appareil debranche en cours d'attente ne doit pas faire
    /// patienter 30 s pour rien.
    #[tokio::test(start_paused = true)]
    async fn i2749_uri_vide_puis_soap_muet_abandonne_sans_bruler_la_borne() {
        let mut reponses: Vec<Result<Option<&'static str>, &'static str>> = vec![Ok(Some("")); 8];
        reponses.push(Err(
            "soap send: error trying to connect: connection refused",
        ));
        let (v, _lectures, _relances) = eprouver_2749(reponses, None).await;
        assert_eq!(v.verdict, UriVerdict::PasEncore);
        assert!(
            v.soap_muet,
            "l'appareil avait repondu, puis s'est tu : c'est un appareil qui \
             disparait, pas un appareil qui dort"
        );
        assert!(
            v.attente_ms < 10_000,
            "l'abandon doit etre IMMEDIAT, pas au bout des 30 s : {} ms",
            v.attente_ms
        );
    }

    /// La borne, quand la veille n'aboutit jamais : on echoue — mais APRES
    /// l'avoir vraiment attendue, et sous la grace de chargement du sondeur
    /// (45 s), pour qu'une seule instance decide.
    #[tokio::test(start_paused = true)]
    async fn i2749_une_veille_qui_n_aboutit_pas_echoue_a_la_borne() {
        let (v, _lectures, relances) = eprouver_2749(vec![Ok(Some(""))], None).await;
        assert_eq!(v.verdict, UriVerdict::PasEncore);
        assert!(
            !v.soap_muet,
            "l'appareil repond toujours, il ne se reveille pas"
        );
        assert!(
            v.attente_ms >= 30_000,
            "une attente plus courte que la borne ne corrige rien : {} ms",
            v.attente_ms
        );
        assert!(
            v.attente_ms < 40_000,
            "l'attente doit rester franchement sous la grace de chargement du \
             sondeur (TRACK_LOAD_GRACE_SECS = 45 s) : {} ms",
            v.attente_ms
        );
        assert!(
            relances >= 3,
            "l'URI est reposee toutes les 8 s pendant l'attente : {relances}"
        );
    }

    /// Contre-epreuve — un renderer qui n'implemente PAS `GetMediaInfo` ne
    /// bloque toujours rien. Rien n'a jamais repondu : on ne conclut pas a un
    /// appareil disparu, et surtout on ne fait pas echouer la lecture.
    #[tokio::test(start_paused = true)]
    async fn i2749_un_renderer_sans_get_media_info_ne_bloque_toujours_rien() {
        let (v, lectures, relances) = eprouver_2749(vec![Err("soap send: 501")], None).await;
        assert_eq!(v.verdict, UriVerdict::Indeterminee);
        assert!(
            !v.soap_muet,
            "aucune lecture n'avait abouti : ce n'est pas un appareil qui se tait"
        );
        assert_eq!(lectures, 1);
        assert_eq!(relances, 0);
    }

    /// Contre-epreuve — un REFUS SOAP sur le `Play` de la relance n'ouvre pas
    /// la fenetre de reveil. Un appareil qui refuse la transition NOMME un
    /// etat (701, #2581) ; il ne dort pas. Sans cette porte, le 701 de FabienM
    /// aurait ete travesti en veille et l'utilisateur aurait attendu 30 s.
    #[tokio::test(start_paused = true)]
    async fn i2749_un_refus_sur_la_relance_n_ouvre_pas_la_fenetre_de_reveil() {
        let (v, _lectures, relances) = eprouver_2749(
            vec![Ok(Some(""))],
            Some("<errorCode>701</errorCode>Transition not available"),
        )
        .await;
        assert_eq!(v.verdict, UriVerdict::PasEncore);
        assert!(
            v.refus_relance.is_some(),
            "le refus doit etre remonte tel quel"
        );
        assert_eq!(relances, 1);
        assert!(
            v.attente_ms < 3_000,
            "un refus n'est pas une veille : pas d'attente : {} ms",
            v.attente_ms
        );
    }

    /// Contre-epreuve — une URI reecrite (Sonos) ne conclut toujours RIEN, et
    /// n'ouvre aucune attente.
    #[tokio::test(start_paused = true)]
    async fn i2749_une_uri_reecrite_ne_conclut_toujours_rien() {
        let (v, lectures, relances) =
            eprouver_2749(vec![Ok(Some("x-rincon-queue:RINCON_123#0"))], None).await;
        assert_eq!(v.verdict, UriVerdict::Indeterminee);
        assert_eq!(lectures, 1);
        assert_eq!(relances, 0);
    }

    #[test]
    fn extract_tag_works() {
        let xml = "<RelTime>0:03:45</RelTime><TrackDuration>0:05:30</TrackDuration>";
        assert_eq!(extract_tag(xml, "RelTime"), Some("0:03:45".into()));
        assert_eq!(extract_tag(xml, "TrackDuration"), Some("0:05:30".into()));
        assert_eq!(extract_tag(xml, "Missing"), None);
    }

    #[test]
    fn didl_metadata_with_cover_and_album() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://example.com/stream",
                mime_type: "audio/flac",
                title: Some("Test Track"),
                artist: Some("Test Artist"),
                album: Some("Test Album"),
                cover_url: Some("http://example.com/cover.jpg"),
                duration_ms: Some(256_000),
                file_size: Some(50_000_000),
                ..Default::default()
            },
            "1",
        );
        assert!(didl.contains("Test Track"));
        assert!(didl.contains("Test Artist"));
        assert!(didl.contains("Test Album"));
        assert!(didl.contains("albumArtURI"));
        assert!(didl.contains("cover.jpg"));
        assert!(
            didl.contains("dlna:profileID"),
            "albumArtURI must include dlna:profileID"
        );
        assert!(
            didl.contains("JPEG_TN"),
            "albumArtURI must use JPEG_TN profile"
        );
        assert!(
            didl.contains("xmlns:dlna"),
            "DIDL-Lite must declare xmlns:dlna namespace"
        );
        assert!(
            didl.contains("DLNA.ORG_OP=01"),
            "protocolInfo must include DLNA.ORG_OP"
        );
        assert!(
            didl.contains("DLNA.ORG_FLAGS="),
            "protocolInfo must include DLNA.ORG_FLAGS"
        );
        assert!(didl.contains("size="), "res must include size attribute");
        assert!(
            didl.contains("duration="),
            "res must include duration attribute"
        );
    }

    #[test]
    fn didl_metadata_without_cover() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://example.com/stream",
                mime_type: "audio/flac",
                title: Some("Title"),
                ..Default::default()
            },
            "1",
        );
        assert!(didl.contains("Title"));
        assert!(!didl.contains("albumArtURI"));
        assert!(!didl.contains("upnp:album"));
        assert!(!didl.contains("dc:creator"));
        assert!(!didl.contains("size="));
        assert!(!didl.contains("duration="));
    }

    #[test]
    fn didl_metadata_null_artist_string() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://example.com/stream",
                mime_type: "audio/flac",
                title: Some("Title"),
                artist: Some("null"),
                ..Default::default()
            },
            "1",
        );
        assert!(
            !didl.contains("dc:creator"),
            "literal 'null' artist must be omitted"
        );
    }

    #[test]
    fn didl_metadata_empty_artist() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://example.com/stream",
                mime_type: "audio/flac",
                title: Some("Title"),
                artist: Some(""),
                ..Default::default()
            },
            "1",
        );
        assert!(!didl.contains("dc:creator"), "empty artist must be omitted");
    }

    #[test]
    fn didl_escapes_special_chars() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://example.com/stream?a=1&b=2",
                mime_type: "audio/flac",
                title: Some("Rock & Roll"),
                artist: Some("AC/DC"),
                ..Default::default()
            },
            "1",
        );
        // build_escaped() double-escapes ampersands: first XML-escape for
        // DIDL content, then partial_escape for SOAP embedding.
        // "&" -> "&amp;" (XML) -> "&amp;amp;" (SOAP partial escape)
        // Note: quotes are NOT escaped (partial_escape), matching what
        // Denon/Marantz renderers expect in SOAP text content.
        assert!(didl.contains("Rock &amp;amp; Roll"));
        assert!(didl.contains("AC/DC"));
        assert!(didl.contains("a=1&amp;amp;b=2"));
    }

    #[test]
    fn didl_dlna_flags_wav() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://x/s",
                mime_type: "audio/wav",
                title: Some("T"),
                ..Default::default()
            },
            "1",
        );
        assert!(
            didl.contains("DLNA.ORG_PN=LPCM"),
            "WAV must have LPCM profile"
        );
    }

    #[test]
    fn didl_dlna_flags_mp3() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://x/s",
                mime_type: "audio/mpeg",
                title: Some("T"),
                ..Default::default()
            },
            "1",
        );
        assert!(
            didl.contains("DLNA.ORG_PN=MP3"),
            "MP3 must have MP3 profile"
        );
    }

    #[test]
    fn didl_metadata_includes_audio_params() {
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://x/s.wav",
                mime_type: "audio/wav",
                title: Some("DSD Track"),
                sample_rate: Some(176_400),
                bit_depth: Some(24),
                channels: Some(2),
                ..Default::default()
            },
            "1",
        );
        assert!(
            didl.contains("sampleFrequency=\"176400\""),
            "DIDL must include sampleFrequency for DSD->PCM"
        );
        assert!(
            didl.contains("bitsPerSample=\"24\""),
            "DIDL must include bitsPerSample for DSD->PCM"
        );
        assert!(
            didl.contains("nrAudioChannels=\"2\""),
            "DIDL must include nrAudioChannels for DSD->PCM"
        );
    }

    #[test]
    fn parse_time_edge_cases() {
        assert_eq!(DlnaOutput::parse_time(""), 0);
        assert_eq!(DlnaOutput::parse_time("NOT_A_TIME"), 0);
        assert_eq!(DlnaOutput::parse_time("0:00:00"), 0);
        assert_eq!(DlnaOutput::parse_time("0:00:01"), 1_000);
        assert_eq!(DlnaOutput::parse_time("23:59:59.999"), 86_399_999);
    }

    #[test]
    fn parse_time_dmp_a6_scenario() {
        // DMP-A6 reports "0:03:46" for a track that's actually 4:16.487.
        // With fractional parsing, "0:03:46.000" should give exactly 226000ms,
        // and "0:04:16.487" should give exactly 256487ms.
        let renderer_dur = DlnaOutput::parse_time("0:03:46");
        let track_dur = DlnaOutput::parse_time("0:04:16.487");
        assert_eq!(renderer_dur, 226_000);
        assert_eq!(track_dur, 256_487);
        let diff = (track_dur as i64 - renderer_dur as i64).unsigned_abs();
        assert!(diff > 2000, "difference should exceed gapless threshold");
    }

    #[test]
    fn format_time_roundtrip() {
        for ms in [0, 1000, 60_000, 225_000, 3_600_000, 86_399_000] {
            let formatted = DlnaOutput::format_time(ms);
            let parsed = DlnaOutput::parse_time(&formatted);
            assert_eq!(parsed, ms, "roundtrip failed for {ms}ms -> {formatted}");
        }
    }

    #[test]
    fn didl_metadata_ecrit_l_item_id_qu_on_lui_donne() {
        // Ce test s'appelait `didl_item_id_alternates` et venait avec le
        // correctif d'alternance de `d53191bb`. Il ne PROUVAIT PAS
        // l'alternance : il passe « 1 » puis « 2 » à la main et n'appelle
        // jamais `next_item_id`. Il est donc resté vert pendant les trois mois
        // où la suite réellement émise valait `1, 2, 1, 2` (#3675). Ce qu'il
        // vérifie, et c'est utile, c'est que l'id fourni ARRIVE dans le
        // document. L'unicité, elle, se garde en appelant le générateur —
        // `quatre_item_id_consecutifs_sont_tous_distincts` et ses voisins.
        let didl_1 = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://x/track1",
                mime_type: "audio/flac",
                title: Some("Track 1"),
                ..Default::default()
            },
            "1",
        );
        let didl_2 = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://x/track2",
                mime_type: "audio/flac",
                title: Some("Track 2"),
                ..Default::default()
            },
            "2",
        );
        assert!(didl_1.contains("id=\"1\""), "first track should have id=1");
        assert!(didl_2.contains("id=\"2\""), "second track should have id=2");
        assert!(didl_1.contains("Track 1"));
        assert!(didl_2.contains("Track 2"));
    }

    #[test]
    fn native_flac_next_track_didl_is_complete() {
        // #1132 (native FLAC): the gapless SetNextAVTransportURI DIDL must carry
        // the SAME full metadata as the initial SetAVTransportURI item — title,
        // artist, album, protocolInfo (format), duration AND a size that matches
        // the bytes the renderer will actually receive. A queued item missing
        // any of these makes the Marantz ND 8006 lose the format/duration/
        // progress display when it transitions to the next track. Both the
        // current-track and next-track paths build via `didl_metadata`, so a
        // single assertion covers the queued item too.
        let didl = DlnaOutput::didl_metadata(
            &PlayMedia {
                url: "http://x/track2.flac",
                mime_type: "audio/flac",
                title: Some("So What"),
                artist: Some("Miles Davis"),
                album: Some("Kind of Blue"),
                duration_ms: Some(562_000),
                file_size: Some(50_000_000),
                sample_rate: Some(96_000),
                bit_depth: Some(24),
                channels: Some(2),
                ..Default::default()
            },
            "2",
        );
        assert!(didl.contains("So What"), "title present");
        assert!(didl.contains("Miles Davis"), "artist present");
        assert!(didl.contains("Kind of Blue"), "album present");
        assert!(didl.contains("audio/flac"), "format/protocolInfo present");
        assert!(didl.contains("DLNA.ORG_OP=01"), "DLNA flags present");
        assert!(
            didl.contains("duration=\"0:09:22.000\""),
            "duration present on the queued FLAC item"
        );
        assert!(
            didl.contains("size=\"50000000\""),
            "size present on the queued FLAC item"
        );
        assert!(didl.contains("sampleFrequency=\"96000\""));
        assert!(didl.contains("bitsPerSample=\"24\""));
    }

    // ───────────────────────────────────────────────────────────────────────
    // #3675 — Marantz ND8006 : « une piste sur deux perd le format, le temps
    // et sa progression » (Jean Valjean, fil forum 631, 15/06/2026).
    //
    // Ces gardes APPELLENT `next_item_id`, la fonction que les deux seuls
    // appelants du fichier consomment : `DlnaOutput::play_media`
    // (`SetAVTransportURI`, la piste courante) et `DlnaOutput::set_next_media`
    // (`SetNextAVTransportURI`, la piste armée en gapless). Elles ne relisent
    // pas la source.
    // ───────────────────────────────────────────────────────────────────────

    /// Un renderer de test. `DlnaOutput::new` ne touche pas au réseau : rien
    /// ne part tant qu'aucune action SOAP n'est émise, et aucune ne l'est ici.
    fn renderer_de_test() -> DlnaOutput {
        DlnaOutput::new(
            "Marantz ND8006".to_string(),
            "uuid:nd8006-de-test".to_string(),
            "http://192.0.2.10:60006".to_string(),
            "http://192.0.2.10:60006/AVTransport/ctrl".to_string(),
            "http://192.0.2.10:60006/RenderingControl/ctrl".to_string(),
            None,
        )
    }

    /// La FORME FAUTIVE, reproduite : l'alternance à deux valeurs livrée le
    /// 14/06/2026 (`d53191bb`). Sans cette moitié, la garde suivante pourrait
    /// être verte sans rien avoir distingué.
    ///
    /// Quatre tirages — deux pistes armées en gapless — ne rendent que DEUX
    /// valeurs : la piste 3 reprend l'id de la piste 1, la piste 4 celui de la
    /// piste 2. La collision de cache que le correctif prétendait supprimer
    /// revient donc avec une PÉRIODE DE DEUX, qui est la période même du
    /// symptôme signalé.
    #[test]
    fn la_forme_fautive_reproduite_collisionne_avec_une_periode_de_deux() {
        let bascule = AtomicBool::new(false);
        let alterner = || {
            if bascule.fetch_xor(true, Ordering::Relaxed) {
                "2"
            } else {
                "1"
            }
        };

        let ids: Vec<&str> = (0..4).map(|_| alterner()).collect();

        assert_eq!(ids[0], ids[2], "piste 3 réutilise l'id de la piste 1");
        assert_eq!(ids[1], ids[3], "piste 4 réutilise l'id de la piste 2");
        let uniques: std::collections::BTreeSet<&&str> = ids.iter().collect();
        assert_eq!(uniques.len(), 2, "le domaine ne compte que deux valeurs");
    }

    /// La forme livrée : quatre émissions consécutives, quatre ids distincts.
    #[test]
    fn quatre_item_id_consecutifs_sont_tous_distincts() {
        let sortie = renderer_de_test();

        let ids: Vec<String> = (0..4).map(|_| sortie.next_item_id()).collect();

        let uniques: std::collections::BTreeSet<&String> = ids.iter().collect();
        assert_eq!(uniques.len(), ids.len(), "ids émis : {ids:?}");
    }

    /// Et sur une file entière : aucun id ne revient, quelle que soit la
    /// répartition entre les deux appelants. Un compteur qui rebouclerait sur
    /// un petit domaine — le défaut corrigé — serait rouge ici.
    #[test]
    fn deux_cents_emissions_ne_repetent_jamais_un_id() {
        let sortie = renderer_de_test();

        let ids: Vec<String> = (0..200).map(|_| sortie.next_item_id()).collect();

        let uniques: std::collections::BTreeSet<&String> = ids.iter().collect();
        assert_eq!(uniques.len(), 200, "un id a été réémis");
    }

    /// Chaque appareil compte pour lui : deux renderers ne se volent pas leur
    /// suite d'ids, et rien n'est partagé entre eux.
    #[test]
    fn le_compteur_est_propre_a_chaque_appareil() {
        let un = renderer_de_test();
        let autre = renderer_de_test();

        // Deux appareils neufs partent du même id : le compteur n'est pas
        // global.
        assert_eq!(un.next_item_id(), autre.next_item_id());
        // Et faire avancer l'un n'avance pas l'autre.
        let _ = un.next_item_id();
        assert_ne!(un.next_item_id(), autre.next_item_id());
    }

    /// L'id neuf arrive bien DANS le document DIDL envoyé au renderer —
    /// l'attribut `id` de `<item>`, celui sur lequel le ND8006 indexe son
    /// cache. Un compteur juste dont la valeur n'atteindrait pas le document
    /// ne corrigerait rien.
    #[test]
    fn les_documents_didl_successifs_portent_des_id_distincts() {
        let sortie = renderer_de_test();
        let media = PlayMedia {
            url: "http://192.0.2.1:8888/stream/1",
            mime_type: "audio/flac",
            title: Some("So What"),
            artist: Some("Miles Davis"),
            album: Some("Kind of Blue"),
            duration_ms: Some(562_000),
            file_size: Some(50_000_000),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            channels: Some(2),
            ..Default::default()
        };

        let ids: Vec<String> = (0..4)
            .map(|_| {
                let didl = DlnaOutput::didl_metadata_pour_test(
                    &media,
                    &sortie.next_item_id(),
                    media.mime_type,
                );
                // `build_escaped` rend `&lt;item id="1" …&gt;` : le chevron
                // est échappé, les guillemets restent bruts (les analyseurs
                // Denon/Marantz butent sur `&quot;`). Le marqueur cherché est
                // donc valide sur les deux formes.
                const MARQUEUR: &str = "item id=\"";
                let debut = didl
                    .find(MARQUEUR)
                    .unwrap_or_else(|| panic!("aucun `{MARQUEUR}` dans le DIDL : {didl}"));
                let reste = &didl[debut + MARQUEUR.len()..];
                reste[..reste.find('"').expect("attribut id non terminé")].to_string()
            })
            .collect();

        let uniques: std::collections::BTreeSet<&String> = ids.iter().collect();
        assert_eq!(uniques.len(), 4, "ids portés par les DIDL : {ids:?}");
    }

    // ───────────────────────────────────────────────────────────────────────
    // #3675, SECOND mécanisme — le niveau de DIDL appris ne savait que
    // DESCENDRE en qualité. Une seule réponse « 500 sans corps » privait
    // TOUTES les pistes suivantes du format annoncé, pour la vie du processus,
    // et rien ne le disait.
    //
    // La durée d'une piste de référence dans tout ce bloc : quatre minutes.
    // ───────────────────────────────────────────────────────────────────────

    const PISTE_MS: u64 = 240_000;

    /// LA garde. Un hoquet isolé — un appareil qui répond mal une fois, au
    /// réveil ou pendant une bascule d'entrée — ne doit pas dégrader la file
    /// entière.
    ///
    /// Avant le correctif, la piste 2 repartait du niveau 1 et toutes les
    /// suivantes aussi : `niveau_de_depart` n'existait pas, les deux `store`
    /// du fichier ne faisaient que monter.
    #[test]
    fn un_hoquet_isole_ne_prive_pas_les_pistes_suivantes_du_format() {
        let porte = NiveauDidlAppris::neuf();

        // Piste 1 : l'appareil rend un « 500 sans corps » sur le DIDL complet.
        // L'échelle descend au DIDL minimal — celui qui n'écrit NI
        // `sampleFrequency` NI `bitsPerSample`.
        let t1 = 0;
        assert_eq!(porte.niveau_de_depart(t1, "Marantz ND8006"), 0);
        assert_eq!(
            porte.apprendre(1, t1, "Marantz ND8006"),
            TransitionNiveauDidl::Degrade { ancien: 0, neuf: 1 },
            "la dégradation doit se NOMMER, pas se faire en silence"
        );

        // Piste 2, quatre minutes plus tard : la porte redonne le complet.
        let t2 = t1 + PISTE_MS;
        assert_eq!(
            porte.niveau_de_depart(t2, "Marantz ND8006"),
            0,
            "le hoquet de la piste 1 a été appris DÉFINITIVEMENT"
        );

        // L'appareil n'avait fait que hoqueter : le complet passe, et la
        // remontée se nomme elle aussi.
        assert_eq!(
            porte.apprendre(0, t2, "Marantz ND8006"),
            TransitionNiveauDidl::Restaure { ancien: 1, neuf: 0 }
        );

        // Piste 3 : plus rien à rattraper, et plus une seule sonde à payer.
        let t3 = t2 + PISTE_MS;
        assert_eq!(porte.niveau_de_depart(t3, "Marantz ND8006"), 0);
        assert_eq!(
            porte.apprendre(0, t3, "Marantz ND8006"),
            TransitionNiveauDidl::Inchange
        );
    }

    /// La FORME FAUTIVE, reproduite. Sans cette moitié, la garde ci-dessus
    /// pourrait être verte sans avoir rien distingué.
    ///
    /// Le champ était un `AtomicU8` nu : le départ valait le niveau appris, et
    /// rien au monde ne le rabaissait.
    #[test]
    fn la_forme_fautive_reproduite_ne_remonte_jamais() {
        // Un « 500 sans corps » a eu lieu sur la piste 1.
        let appris_sans_retour = AtomicU8::new(1);
        let depart_fautif = || appris_sans_retour.load(Ordering::Relaxed);

        // Cent pistes — près de sept heures de musique — plus tard : toujours
        // le DIDL minimal, donc toujours pas de format sur l'afficheur.
        for piste in 1..=100u64 {
            assert_eq!(
                depart_fautif(),
                1,
                "piste {piste}, à {} ms : la forme fautive ne remonte jamais",
                piste * PISTE_MS
            );
        }

        // La porte neuve, elle, rend la main — mais pas avant son délai.
        let porte = NiveauDidlAppris::neuf();
        porte.apprendre(1, 0, "Marantz ND8006");
        assert_eq!(
            porte.niveau_de_depart(DIDL_RESONDE_BASE_MS - 1, "Marantz ND8006"),
            1,
            "avant le délai, l'apprentissage tient"
        );
        assert_eq!(
            porte.niveau_de_depart(DIDL_RESONDE_BASE_MS, "Marantz ND8006"),
            0,
            "au délai, l'appareil est remis à l'épreuve"
        );
    }

    /// L'autre moitié du contrat : un appareil qui ne sait VRAIMENT pas lire un
    /// DIDL complet — la pile Platinum de l'Eversolo, #2394 — ne doit pas être
    /// resondé à chaque piste. C'est exactement ce que l'apprentissage était
    /// venu supprimer, et qu'une simple remise à zéro par piste rétablirait.
    #[test]
    fn un_appareil_qui_ne_sait_pas_faire_n_est_pas_resonde_a_chaque_piste() {
        let porte = NiveauDidlAppris::neuf();
        // Soixante pistes : quatre heures de musique d'affilée.
        const PISTES: u32 = 60;
        let mut resondes = 0u32;

        for piste in 0..PISTES {
            let t = u64::from(piste) * PISTE_MS;
            let etait_degrade = porte.niveau_courant() > 0;
            let depart = porte.niveau_de_depart(t, "Eversolo DMP-A8");
            if etait_degrade && depart == 0 {
                resondes += 1;
            }
            // La pile Platinum refuse le DIDL complet À CHAQUE FOIS.
            porte.apprendre(1, t, "Eversolo DMP-A8");
        }

        assert!(
            resondes >= 1,
            "une porte qui ne se rouvre jamais est le défaut qu'on corrige"
        );
        assert!(
            resondes * 5 <= PISTES,
            "{resondes} aller-retours perdus sur {PISTES} pistes : la sonde \
             doit s'espacer, pas revenir à chaque piste"
        );
        assert_eq!(
            porte.attente_courante_ms(),
            DIDL_RESONDE_MAX_MS,
            "au bout de quatre heures, le délai doit avoir atteint son plafond"
        );
    }

    /// Le barème lui-même : le délai double à chaque remise à l'épreuve ratée,
    /// et il plafonne.
    #[test]
    fn le_delai_de_resonde_double_et_plafonne() {
        let porte = NiveauDidlAppris::neuf();
        porte.apprendre(1, 0, "Eversolo DMP-A8");
        assert_eq!(porte.attente_courante_ms(), DIDL_RESONDE_BASE_MS);

        let mut t = 0u64;
        let mut attente = DIDL_RESONDE_BASE_MS;
        for tour in 0..20 {
            t += attente;
            assert_eq!(
                porte.niveau_de_depart(t, "Eversolo DMP-A8"),
                0,
                "tour {tour} : la sonde était due à {t} ms"
            );
            attente = attente.saturating_mul(2).min(DIDL_RESONDE_MAX_MS);
            assert_eq!(
                porte.apprendre(1, t, "Eversolo DMP-A8"),
                TransitionNiveauDidl::ResondeEchouee {
                    attente_ms: attente
                },
                "tour {tour}"
            );
        }
        assert_eq!(porte.attente_courante_ms(), DIDL_RESONDE_MAX_MS);
    }

    /// Une dégradation NEUVE repart du délai de base : le barème accumulé par
    /// un incident ancien ne doit pas retarder le rattrapage du suivant.
    #[test]
    fn une_degradation_neuve_repart_du_delai_de_base() {
        let porte = NiveauDidlAppris::neuf();
        porte.apprendre(1, 0, "Marantz ND8006");

        let mut t = 0u64;
        let mut attente = DIDL_RESONDE_BASE_MS;
        for _ in 0..3 {
            t += attente;
            porte.apprendre(1, t, "Marantz ND8006");
            attente = attente.saturating_mul(2).min(DIDL_RESONDE_MAX_MS);
        }
        assert!(porte.attente_courante_ms() > DIDL_RESONDE_BASE_MS);

        // L'appareil se remet.
        t += attente;
        assert_eq!(porte.niveau_de_depart(t, "Marantz ND8006"), 0);
        assert_eq!(
            porte.apprendre(0, t, "Marantz ND8006"),
            TransitionNiveauDidl::Restaure { ancien: 1, neuf: 0 }
        );
        assert_eq!(
            porte.attente_courante_ms(),
            DIDL_RESONDE_BASE_MS,
            "le barème doit être remis à plat par la restauration"
        );

        // Un nouvel incident, plus tard : rattrapé au bout d'une minute, pas
        // au bout du délai qu'avait atteint l'incident précédent.
        let t_incident = t + 10 * PISTE_MS;
        porte.apprendre(1, t_incident, "Marantz ND8006");
        assert_eq!(
            porte.niveau_de_depart(t_incident + DIDL_RESONDE_BASE_MS, "Marantz ND8006"),
            0
        );
    }

    /// CE QUE L'UTILISATEUR PERD, mesuré sur les deux documents que les deux
    /// niveaux produisent réellement — pas décrit dans un commentaire.
    ///
    /// C'est le lien entre le niveau et le symptôme rapporté : « le Marantz
    /// perd le format (44/16) ».
    #[test]
    fn le_didl_minimal_prive_le_renderer_du_format_que_le_complet_annonce() {
        let media = PlayMedia {
            url: "http://192.0.2.1:8888/stream/1",
            mime_type: "audio/flac",
            title: Some("So What"),
            artist: Some("Miles Davis"),
            album: Some("Kind of Blue"),
            cover_url: Some("http://192.0.2.1:8888/cover/1"),
            duration_ms: Some(562_000),
            file_size: Some(50_000_000),
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            channels: Some(2),
            ..Default::default()
        };

        let complet = DlnaOutput::didl_metadata_pour_test(&media, "1", media.mime_type);
        let minimal = DlnaOutput::didl_metadata_minimale_pour_test(&media, "1", media.mime_type);

        for attendu in [
            "sampleFrequency=\"44100\"",
            "bitsPerSample=\"16\"",
            "Miles Davis",
            "Kind of Blue",
        ] {
            assert!(
                complet.contains(attendu),
                "le DIDL complet doit porter `{attendu}` : {complet}"
            );
            assert!(
                !minimal.contains(attendu),
                "le DIDL minimal ne porte PAS `{attendu}` — c'est ce que \
                 l'utilisateur perd quand le niveau est rabaissé : {minimal}"
            );
        }

        // Le titre et la durée, eux, survivent : la perte est bornée, et c'est
        // exactement la liste ci-dessus.
        assert!(minimal.contains("So What"));
        assert!(minimal.contains("duration="));
    }

    /// « Écrit mais pas branché » : la porte éprouvée ci-dessus est bien LE
    /// champ du `DlnaOutput` réel, celui que `play_media` (`SetAVTransportURI`)
    /// et `set_next_media` (`SetNextAVTransportURI`) interrogent.
    ///
    /// [`NiveauDidlAppris`] n'expose AUCUNE lecture brute du niveau hors des
    /// épreuves : `niveau_de_depart` est le seul chemin de production, aucun
    /// appelant ne peut donc repartir du niveau dégradé en contournant le
    /// délai.
    #[test]
    fn la_porte_est_bien_le_champ_du_renderer_reel() {
        let sortie = renderer_de_test();

        assert_eq!(
            sortie.didl_niveau_appris.niveau_de_depart(0, &sortie.name),
            0,
            "un appareil neuf part du DIDL complet"
        );
        sortie.didl_niveau_appris.apprendre(1, 0, &sortie.name);
        assert_eq!(
            sortie
                .didl_niveau_appris
                .niveau_de_depart(PISTE_MS, &sortie.name),
            0,
            "quatre minutes plus tard, le renderer réel est remis à l'épreuve"
        );
    }
}
