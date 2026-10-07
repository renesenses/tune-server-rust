//! Le rôle `player@v1` côté SERVEUR : ce que Tune envoie à une enceinte (#3326, S2-c).
//!
//! Contrat : `Sendspin/spec` 1.0.0-rc1 (`671a34d4`), `messaging.md` et
//! `roles/player/v1.md`. Résumé et liens dans `docs/sendspin.md`.
//!
//! Ce module ne fait AUCUNE entrée-sortie. Il contient :
//!
//! - le choix du format parmi ceux que l'enceinte annonce ;
//! - le cadrage d'un morceau audio (type binaire 4, horodatage, `send_ahead`) ;
//! - le calcul de l'avance de départ et la comptabilité de `buffer_capacity` ;
//! - [`SessionLecteur`], la machine à états qui décide quels messages partent,
//!   et qui REFUSE ce que la spécification interdit (un `stream/start` avant un
//!   `client/state` disponible, un morceau hors flux, une commande que
//!   l'enceinte n'a pas proposée, un `stream/clear` sans flux actif…).
//!
//! Le pilote de connexion (`tune-server`) n'a plus qu'à chiffrer et envoyer ce
//! que la session rend, et la sortie (`outputs::sendspin`) qu'à lui passer des
//! [`OrdreLecteur`]. Les deux se rejoignent par une [`LiaisonLecteur`].

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::VecDeque;
use tokio::sync::{mpsc, oneshot, watch};

/// Le seul rôle audio que Tune active.
pub const ROLE_LECTEUR: &str = "player@v1";
/// Clé VERSIONNÉE de l'objet de capacités dans `client/hello`. La forme non
/// versionnée est héritée : la spécification interdit d'activer une version
/// de rôle dont l'objet de support manque.
pub const CLE_SUPPORT_LECTEUR: &str = "player@v1_support";
/// Type binaire d'un morceau audio du rôle `player`.
pub const TYPE_AUDIO: u8 = 4;
/// Octet de type + horodatage (8) + `send_ahead` (4).
pub const TAILLE_ENTETE_AUDIO: usize = 13;
/// Un morceau ne dépasse jamais 150 ms (`MUST NOT`).
pub const DUREE_MAX_MORCEAU_US: i64 = 150_000;
/// Et ne descend pas sous 15 ms, sauf le dernier (`SHOULD NOT`).
pub const DUREE_MIN_MORCEAU_US: i64 = 15_000;
/// Marge de Tune ajoutée à l'avance de départ que l'enceinte demande : le
/// temps entre le calcul de l'horodatage du premier morceau et son envoi
/// (lecture du fichier, décodage du premier bloc). Ce n'est pas une valeur de
/// la spécification.
pub const MARGE_DEPART_US: i64 = 150_000;

/// Un format audio tel que la spécification le décrit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatAudio {
    pub codec: String,
    pub sample_rate: u32,
    pub channels: u16,
    pub bit_depth: u16,
}

impl FormatAudio {
    pub fn pcm(sample_rate: u32, channels: u16, bit_depth: u16) -> Self {
        Self {
            codec: "pcm".into(),
            sample_rate,
            channels,
            bit_depth,
        }
    }

    /// Octets par trame (un échantillon sur chaque canal), en PCM.
    #[must_use]
    pub fn octets_par_trame(&self) -> usize {
        usize::from(self.channels) * usize::from(self.bit_depth / 8)
    }

    fn objet(&self) -> Value {
        json!({
            "codec": self.codec,
            "sample_rate": self.sample_rate,
            "channels": self.channels,
            "bit_depth": self.bit_depth,
        })
    }
}

/// Ce que Tune sait PRODUIRE dans cette première version : du PCM entier
/// petit-boutiste, 16, 24 (sur 3 octets) ou 32 bits, mono ou stéréo.
///
/// La spécification impose `flac` ET `pcm` au serveur ; FLAC n'est pas encore
/// branché (voir `docs/sendspin.md`, « Ce qui reste »). Une enceinte qui
/// n'annonce que FLAC n'obtient donc PAS de rôle actif : mieux vaut pas de
/// zone qu'une zone muette.
#[must_use]
pub fn sait_produire(f: &FormatAudio) -> bool {
    f.codec == "pcm"
        && matches!(f.bit_depth, 16 | 24 | 32)
        && (1..=2).contains(&f.channels)
        && (8_000..=384_000).contains(&f.sample_rate)
}

/// Les formats de `supported_formats`, dans l'ordre de préférence de
/// l'enceinte. Une entrée mal formée est ignorée, pas fatale.
#[must_use]
pub fn formats_annonces(support: &Value) -> Vec<FormatAudio> {
    support
        .get("supported_formats")
        .and_then(Value::as_array)
        .map(|liste| {
            liste
                .iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// `buffer_capacity` de l'objet de support, en octets.
#[must_use]
pub fn capacite_tampon(support: &Value) -> Option<u64> {
    support.get("buffer_capacity").and_then(Value::as_u64)
}

/// Règle de `roles/player/v1.md` : le format que l'état préfère s'il est
/// annoncé et que nous savons le produire, sinon la première entrée de
/// `supported_formats` que nous savons produire.
#[must_use]
pub fn choisir_format(
    annonces: &[FormatAudio],
    prefere: Option<&FormatAudio>,
) -> Option<FormatAudio> {
    if let Some(p) = prefere
        && annonces.contains(p)
        && sait_produire(p)
    {
        return Some(p.clone());
    }
    annonces.iter().find(|f| sait_produire(f)).cloned()
}

/// L'objet `player` de `client/state`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EtatLecteur {
    pub volume: Option<u8>,
    pub muted: Option<bool>,
    pub output_delay_ms: u32,
    pub required_lead_time_ms: u32,
    pub min_buffer_ms: u32,
    pub supported_commands: Vec<String>,
    pub format: Option<FormatAudio>,
}

impl EtatLecteur {
    #[must_use]
    pub fn propose(&self, commande: &str) -> bool {
        self.supported_commands.iter().any(|c| c == commande)
    }
}

/// L'état client tel que le serveur le connaît.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EtatClient {
    /// `None` tant qu'aucun `client/state` n'est arrivé.
    pub available: Option<bool>,
    pub lecteur: Option<EtatLecteur>,
}

impl EtatClient {
    #[must_use]
    pub fn disponible(&self) -> bool {
        self.available == Some(true)
    }
}

/// Lit un `client/state` et le fond dans l'état précédent.
///
/// `available` est obligatoire dans chaque message ; un objet de rôle omis
/// laisse l'état de ce rôle inchangé ; un objet présent porte l'état COMPLET.
pub fn fondre_etat(precedent: &EtatClient, payload: &Value) -> Result<EtatClient, &'static str> {
    let available = payload
        .get("available")
        .and_then(Value::as_bool)
        .ok_or("client/state sans available")?;
    let lecteur = match payload.get("player") {
        None => precedent.lecteur.clone(),
        Some(p) => {
            let entier = |cle: &str| -> Result<u32, &'static str> {
                let v = p.get(cle).ok_or("champ de minutage absent")?;
                let n = v.as_u64().ok_or("champ de minutage non entier")?;
                Ok(u32::try_from(n).unwrap_or(u32::MAX))
            };
            Some(EtatLecteur {
                volume: p
                    .get("volume")
                    .and_then(Value::as_u64)
                    .map(|v| v.min(100) as u8),
                muted: p.get("muted").and_then(Value::as_bool),
                // Borné 0-5000 par la spécification, côté client comme ici.
                output_delay_ms: entier("output_delay_ms")?.min(5_000),
                required_lead_time_ms: entier("required_lead_time_ms")?,
                min_buffer_ms: entier("min_buffer_ms")?,
                supported_commands: p
                    .get("supported_commands")
                    .and_then(Value::as_array)
                    .ok_or("supported_commands absent")?
                    .iter()
                    .filter_map(|c| c.as_str().map(str::to_owned))
                    .collect(),
                format: p
                    .get("format")
                    .and_then(|f| serde_json::from_value(f.clone()).ok()),
            })
        }
    };
    Ok(EtatClient {
        available: Some(available),
        lecteur,
    })
}

/// Avance de départ du premier morceau après un `stream/start` à vide ou un
/// `stream/clear`, en microsecondes.
///
/// Au moins `min_buffer_ms + output_delay_ms` ; pour une source tamponnée
/// (un fichier), étendue vers `required_lead_time_ms`. Le délai de sortie
/// s'ajoute à part : l'enceinte ne l'inclut dans aucune des deux valeurs.
#[must_use]
pub fn avance_de_depart_us(etat: &EtatLecteur) -> i64 {
    let base = i64::from(etat.min_buffer_ms.max(etat.required_lead_time_ms));
    (base + i64::from(etat.output_delay_ms)) * 1_000 + MARGE_DEPART_US
}

/// `send_ahead` saturé : `0` si l'émission est en retard sur l'horodatage,
/// `u32::MAX` au-delà de ce que le champ représente. Jamais d'enroulement.
#[must_use]
pub fn avance_d_envoi(timestamp_us: i64, maintenant_us: i64) -> u32 {
    let avance = timestamp_us.saturating_sub(maintenant_us);
    if avance <= 0 {
        0
    } else {
        u32::try_from(avance).unwrap_or(u32::MAX)
    }
}

/// Le corps d'un morceau audio, SANS l'octet de type (le transport le pose) :
/// horodatage int64 gros-boutiste, `send_ahead` uint32 gros-boutiste, puis
/// les octets encodés.
#[must_use]
pub fn corps_audio(timestamp_us: i64, send_ahead_us: u32, donnees: &[u8]) -> Vec<u8> {
    let mut corps = Vec::with_capacity(TAILLE_ENTETE_AUDIO - 1 + donnees.len());
    corps.extend_from_slice(&timestamp_us.to_be_bytes());
    corps.extend_from_slice(&send_ahead_us.to_be_bytes());
    corps.extend_from_slice(donnees);
    corps
}

/// Durée, en microsecondes, de `trames` trames à `sample_rate`.
#[must_use]
pub fn duree_us(trames: u64, sample_rate: u32) -> i64 {
    if sample_rate == 0 {
        return 0;
    }
    i64::try_from(u128::from(trames) * 1_000_000 / u128::from(sample_rate)).unwrap_or(i64::MAX)
}

/// La comptabilité de `buffer_capacity` (`roles/player/v1.md`, *Player
/// Buffer Accounting*).
///
/// Chaque morceau compte en entier — en-tête de 13 octets compris — tant que
/// son horodatage + sa durée − `output_delay_ms` n'est pas passé sur notre
/// horloge. `stream/clear` et `stream/end` remettent à zéro.
#[derive(Debug, Default)]
pub struct ComptabiliteTampon {
    capacite: u64,
    morceaux: VecDeque<(i64, u64)>,
    total: u64,
}

impl ComptabiliteTampon {
    #[must_use]
    pub fn nouvelle(capacite: u64) -> Self {
        Self {
            capacite,
            ..Self::default()
        }
    }

    fn purger(&mut self, maintenant_us: i64, delai_sortie_us: i64) {
        while let Some(&(fin, taille)) = self.morceaux.front() {
            if fin - delai_sortie_us > maintenant_us {
                break;
            }
            self.morceaux.pop_front();
            self.total -= taille;
        }
    }

    /// Peut-on commencer à envoyer un morceau de `taille` octets (en-tête
    /// compris) sans dépasser la capacité ?
    pub fn admet(&mut self, taille: u64, maintenant_us: i64, delai_sortie_us: i64) -> bool {
        self.purger(maintenant_us, delai_sortie_us);
        self.total + taille <= self.capacite
    }

    pub fn enregistrer(&mut self, timestamp_us: i64, duree_us: i64, taille: u64) {
        self.morceaux.push_back((timestamp_us + duree_us, taille));
        self.total += taille;
    }

    /// Quand le plus ancien morceau compté cessera de compter.
    #[must_use]
    pub fn prochaine_liberation(&self, delai_sortie_us: i64) -> Option<i64> {
        self.morceaux.front().map(|(fin, _)| fin - delai_sortie_us)
    }

    pub fn vider(&mut self) {
        self.morceaux.clear();
        self.total = 0;
    }

    #[must_use]
    pub fn total(&self) -> u64 {
        self.total
    }
}

/// Ce que la sortie demande à la connexion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrdreLecteur {
    /// Ouvre le flux (ou change son format en place) : `stream/start`.
    Demarrer(FormatAudio),
    /// Un morceau déjà horodaté dans le domaine du serveur.
    Morceau {
        timestamp_us: i64,
        donnees: Vec<u8>,
    },
    /// Seek ou saut de piste : `stream/clear`, le flux reste ouvert.
    Vider,
    /// Pause : `stream/end` et groupe `stopped`, l'activité `playback` reste
    /// déclarée (la connexion n'est pas cédée à un autre serveur).
    Suspendre,
    /// Arrêt : `stream/end` si un flux est ouvert, groupe `stopped`, et
    /// l'activité `playback` est retirée.
    Arreter,
    /// Volume perçu, 0-100.
    Volume(u8),
    Sourdine(bool),
}

/// Pourquoi la session refuse un ordre. Rien de tout cela ne part sur le fil.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusOrdre {
    /// Aucun `client/state` reçu : la spécification interdit commandes et
    /// flux avant le premier.
    EtatAttendu,
    /// Le dernier `client/state` dit `available: false`.
    Indisponible,
    /// Le format n'est pas dans `supported_formats`, ou Tune ne sait pas le
    /// produire.
    FormatRefuse,
    /// Morceau, `stream/clear` hors d'un flux ouvert.
    FluxInactif,
    /// La commande n'est pas dans le dernier `supported_commands`.
    CommandeNonProposee(&'static str),
    /// La connexion est fermée.
    Deconnecte,
}

impl std::fmt::Display for RefusOrdre {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EtatAttendu => f.write_str("sendspin: no client/state received yet"),
            Self::Indisponible => f.write_str("sendspin: player reports available=false"),
            Self::FormatRefuse => {
                f.write_str("sendspin: no producible format announced by the player")
            }
            Self::FluxInactif => f.write_str("sendspin: no active player stream"),
            Self::CommandeNonProposee(c) => write!(f, "sendspin: player does not offer '{c}'"),
            Self::Deconnecte => f.write_str("sendspin: player disconnected"),
        }
    }
}

/// Ce que la session demande au pilote d'envoyer.
#[derive(Debug, Clone, PartialEq)]
pub enum Sortie {
    /// Un message JSON (type binaire 0).
    Json {
        type_message: &'static str,
        payload: Value,
    },
    /// Un morceau audio (type binaire 4). Le pilote calcule `send_ahead` au
    /// dernier moment, juste avant le chiffrement, comme l'exige la
    /// spécification (*Transmit timestamps*).
    Audio { timestamp_us: i64, donnees: Vec<u8> },
}

/// La session `player@v1` d'UNE connexion.
#[derive(Debug, Clone)]
pub struct SessionLecteur {
    formats: Vec<FormatAudio>,
    groupe_id: String,
    groupe_nom: String,
    etat: EtatClient,
    flux: Option<FormatAudio>,
    lecture_declaree: bool,
    groupe_joue: bool,
}

impl SessionLecteur {
    /// `None` si le rôle ne peut pas être activé : `player@v1` absent des
    /// rôles, objet `player@v1_support` absent, ou aucun format que Tune sache
    /// produire. La décision d'authentification (PSK longue durée) appartient
    /// à l'appelant.
    #[must_use]
    pub fn admettre(
        client_id: &str,
        nom: &str,
        hello: &super::messages::ClientHello,
    ) -> Option<Self> {
        if !hello.supported_roles.iter().any(|r| r == ROLE_LECTEUR) {
            return None;
        }
        let support = hello.reste.get(CLE_SUPPORT_LECTEUR)?;
        let formats = formats_annonces(support);
        choisir_format(&formats, None)?;
        Some(Self {
            formats,
            groupe_id: format!("tune-{client_id}"),
            groupe_nom: nom.to_owned(),
            etat: EtatClient::default(),
            flux: None,
            lecture_declaree: false,
            groupe_joue: false,
        })
    }

    #[must_use]
    pub fn formats(&self) -> &[FormatAudio] {
        &self.formats
    }

    #[must_use]
    pub fn etat(&self) -> &EtatClient {
        &self.etat
    }

    #[must_use]
    pub fn flux_actif(&self) -> bool {
        self.flux.is_some()
    }

    fn groupe(&mut self, joue: bool) -> Sortie {
        self.groupe_joue = joue;
        Sortie::Json {
            type_message: "group/update",
            payload: json!({
                "playback_state": if joue { "playing" } else { "stopped" },
                "group_id": self.groupe_id,
                "group_name": self.groupe_nom,
            }),
        }
    }

    /// Le premier `server/activate` : aucune activité, le rôle `player@v1`
    /// actif (une session longue durée vide est *playback-capable*), suivi du
    /// `group/update` que la spécification exige aussitôt après.
    pub fn activation_initiale(&mut self) -> Vec<Sortie> {
        vec![
            Sortie::Json {
                type_message: "server/activate",
                payload: json!({"activities": [], "active_roles": [ROLE_LECTEUR]}),
            },
            self.groupe(false),
        ]
    }

    /// L'activation qui suit un re-échange quand le rôle est conservé : le
    /// rôle et l'activité en cours sont redéclarés, rien d'autre ne change
    /// (les flux persistent à travers un re-échange).
    #[must_use]
    pub fn activation_apres_reechange(&self) -> Sortie {
        let activites: Vec<&str> = if self.lecture_declaree {
            vec!["playback"]
        } else {
            vec![]
        };
        Sortie::Json {
            type_message: "server/activate",
            payload: json!({"activities": activites, "active_roles": [ROLE_LECTEUR]}),
        }
    }

    /// Un `client/state` reçu. Rend `true` au premier état complet du rôle
    /// `player`.
    pub fn recevoir_etat(&mut self, payload: &Value) -> Result<bool, &'static str> {
        let premier = self.etat.lecteur.is_none();
        self.etat = fondre_etat(&self.etat, payload)?;
        Ok(premier && self.etat.lecteur.is_some())
    }

    fn etat_lecteur(&self) -> Result<&EtatLecteur, RefusOrdre> {
        self.etat.lecteur.as_ref().ok_or(RefusOrdre::EtatAttendu)
    }

    /// Traduit un ordre en messages, ou le refuse.
    pub fn executer(
        &mut self,
        ordre: OrdreLecteur,
        maintenant_us: i64,
    ) -> Result<Vec<Sortie>, RefusOrdre> {
        match ordre {
            OrdreLecteur::Demarrer(format) => {
                self.etat_lecteur()?;
                if !self.etat.disponible() {
                    return Err(RefusOrdre::Indisponible);
                }
                if !self.formats.contains(&format) || !sait_produire(&format) {
                    return Err(RefusOrdre::FormatRefuse);
                }
                let mut sorties = Vec::new();
                if !self.lecture_declaree {
                    self.lecture_declaree = true;
                    sorties.push(Sortie::Json {
                        type_message: "server/activate",
                        payload: json!({"activities": ["playback"]}),
                    });
                }
                if !self.groupe_joue {
                    sorties.push(self.groupe(true));
                }
                if self.flux.as_ref() != Some(&format) {
                    sorties.push(Sortie::Json {
                        type_message: "stream/start",
                        payload: json!({
                            "server_transmitted": maintenant_us,
                            "player": format.objet(),
                        }),
                    });
                    self.flux = Some(format);
                }
                Ok(sorties)
            }
            OrdreLecteur::Morceau {
                timestamp_us,
                donnees,
            } => {
                if self.flux.is_none() {
                    return Err(RefusOrdre::FluxInactif);
                }
                Ok(vec![Sortie::Audio {
                    timestamp_us,
                    donnees,
                }])
            }
            OrdreLecteur::Vider => {
                if self.flux.is_none() {
                    return Err(RefusOrdre::FluxInactif);
                }
                Ok(vec![Sortie::Json {
                    type_message: "stream/clear",
                    payload: json!({"server_transmitted": maintenant_us, "roles": ["player"]}),
                }])
            }
            fin @ (OrdreLecteur::Suspendre | OrdreLecteur::Arreter) => {
                let arreter = fin == OrdreLecteur::Arreter;
                let mut sorties = Vec::new();
                if self.flux.take().is_some() {
                    sorties.push(Sortie::Json {
                        type_message: "stream/end",
                        payload: json!({"roles": ["player"]}),
                    });
                }
                if self.groupe_joue {
                    sorties.push(self.groupe(false));
                }
                if arreter && self.lecture_declaree {
                    self.lecture_declaree = false;
                    sorties.push(Sortie::Json {
                        type_message: "server/activate",
                        payload: json!({"activities": []}),
                    });
                }
                Ok(sorties)
            }
            OrdreLecteur::Volume(v) => {
                if !self.etat_lecteur()?.propose("volume") {
                    return Err(RefusOrdre::CommandeNonProposee("volume"));
                }
                Ok(vec![Sortie::Json {
                    type_message: "server/command",
                    payload: json!({"player": {"command": "volume", "volume": v.min(100)}}),
                }])
            }
            OrdreLecteur::Sourdine(m) => {
                if !self.etat_lecteur()?.propose("mute") {
                    return Err(RefusOrdre::CommandeNonProposee("mute"));
                }
                Ok(vec![Sortie::Json {
                    type_message: "server/command",
                    payload: json!({"player": {"command": "mute", "mute": m}}),
                }])
            }
        }
    }
}

/// Un ordre et sa réponse.
#[derive(Debug)]
pub struct Demande {
    pub ordre: OrdreLecteur,
    pub reponse: oneshot::Sender<Result<(), RefusOrdre>>,
}

/// Le côté SORTIE d'une connexion lecteur.
#[derive(Debug, Clone)]
pub struct LiaisonLecteur {
    ordres: mpsc::Sender<Demande>,
    etat: watch::Receiver<EtatClient>,
    formats: Vec<FormatAudio>,
    capacite: u64,
}

/// Le côté CONNEXION : le pilote lit les ordres et publie l'état.
#[derive(Debug)]
pub struct CoteConnexion {
    pub ordres: mpsc::Receiver<Demande>,
    pub etat: watch::Sender<EtatClient>,
}

/// Capacité retenue quand l'enceinte n'en annonce aucune : 1 Mio, le plafond
/// d'un message du transport. Ce n'est pas une valeur de la spécification
/// (`buffer_capacity` y est requis) ; elle ne sert qu'à ne pas déborder une
/// enceinte qui l'aurait omis.
pub const CAPACITE_PAR_DEFAUT: u64 = 1 << 20;

/// Fabrique les deux côtés d'une connexion lecteur.
#[must_use]
pub fn relier(formats: Vec<FormatAudio>, capacite: Option<u64>) -> (LiaisonLecteur, CoteConnexion) {
    let (tx, rx) = mpsc::channel(32);
    let (etat_tx, etat_rx) = watch::channel(EtatClient::default());
    (
        LiaisonLecteur {
            ordres: tx,
            etat: etat_rx,
            formats,
            capacite: capacite.unwrap_or(CAPACITE_PAR_DEFAUT),
        },
        CoteConnexion {
            ordres: rx,
            etat: etat_tx,
        },
    )
}

impl LiaisonLecteur {
    /// Envoie un ordre et attend la décision de la session.
    pub async fn ordonner(&self, ordre: OrdreLecteur) -> Result<(), RefusOrdre> {
        let (reponse, recu) = oneshot::channel();
        self.ordres
            .send(Demande { ordre, reponse })
            .await
            .map_err(|_| RefusOrdre::Deconnecte)?;
        recu.await.map_err(|_| RefusOrdre::Deconnecte)?
    }

    #[must_use]
    pub fn connectee(&self) -> bool {
        !self.ordres.is_closed()
    }

    #[must_use]
    pub fn etat(&self) -> EtatClient {
        self.etat.borrow().clone()
    }

    #[must_use]
    pub fn formats(&self) -> &[FormatAudio] {
        &self.formats
    }

    #[must_use]
    pub fn capacite(&self) -> u64 {
        self.capacite
    }
}

#[cfg(test)]
#[path = "lecteur/tests.rs"]
mod tests;
