//! Ce que le pont fait transiter, compté (#6512).
//!
//! Le relais `bridge.mozaiklabs.fr` vit sur le même VPS que le site. Avant ce
//! module, il ne comptait rien : impossible de dire quelle part du trafic du
//! VPS venait de l'écoute à distance, ni de quels serveurs.
//!
//! ## Ce qui est compté
//!
//! Des octets **qui entrent** sur le VPS (trames du serveur Tune, corps des
//! requêtes des navigateurs) et **qui en sortent** (corps rendus aux
//! navigateurs, messages vers le serveur Tune), en quatre catégories :
//!
//! | Catégorie | Ce qu'elle couvre |
//! |---|---|
//! | `audio`  | `/stream/relay` : l'écoute à distance du propriétaire |
//! | `circle` | `/stream/circle` : Tune Circle, l'écoute chez un contact |
//! | `api`    | `/api/relay` (JSON), `/ws/client`, battements de cœur et contrôle |
//! | `images` | réponses `/api/relay` dont le `content-type` est `image/*` (pochettes) |
//!
//! Les octets comptés sont ceux des **charges utiles** : le texte des trames
//! WebSocket (base64 compris — c'est ce qui traverse réellement le réseau
//! entre le serveur et le pont) et les corps HTTP. En-têtes HTTP, TLS et
//! trames TCP n'y sont pas : le compteur de l'interface réseau du VPS, lu côté
//! site, donne le total réel, et l'écart entre les deux EST ce surcoût.
//!
//! Chaque compte est aussi rangé sous le `server_id` du serveur Tune concerné
//! — l'identifiant opaque que le pont connaît déjà. **Aucune adresse IP n'est
//! lue ni conservée.**
//!
//! ## Sessions d'écoute
//!
//! Une session est un corps audio ouvert vers un navigateur (`audio` ou
//! `circle`). Sa fin dit comment elle s'est terminée :
//!
//! * `completed` : le serveur a envoyé `relay.stream_end`, tout est parti ;
//! * `client_closed` : le navigateur a fermé avant la fin — **un saut de piste
//!   ou une avance rapide en font autant** qu'une coupure réseau, le pont ne
//!   peut pas les distinguer ;
//! * `server_lost` : le serveur Tune s'est déconnecté en plein flux.
//!
//! Une **reprise** est une requête du même flux avec un `Range` qui ne part
//! pas de zéro, moins de [`FENETRE_DE_REPRISE`] après une fin
//! `client_closed` ou `server_lost` : la signature d'un lecteur qui relance
//! après une coupure.
//!
//! La durée d'une session est sa durée d'**ouverture**. Le navigateur
//! précharge : une piste peut être transférée plus vite qu'elle ne se joue.
//! Les « heures d'écoute » qui en sortent sont une approximation, et le site
//! le dit.
//!
//! Le format n'est connu que par le `content-type` du flux (FLAC, DSD, PCM,
//! compressé) : le pont ne voit ni la fréquence ni la résolution.
//!
//! ## Coût sur le chemin chaud
//!
//! Des `AtomicU64` en `Relaxed`, et rien d'autre : pas de verrou, pas
//! d'allocation par morceau. Les compteurs d'un serveur sont un `Arc` porté
//! par sa connexion, récupéré une fois par flux.
//!
//! ## Agrégats par minute
//!
//! Chaque minute, [`Trafic::clore_minute`] relève les compteurs en les
//! remettant à zéro (`swap`) et range le relevé dans une file bornée à
//! [`MINUTES_GARDEES`] (24 h). [`pousser_au_site`] l'envoie au site, qui
//! accuse réception ; seules les minutes acceptées quittent la file. Voir
//! `main.rs` pour le choix d'un envoi plutôt que d'une route à interroger.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use serde::Serialize;
use tracing::{info, warn};

/// Minutes gardées en attente d'envoi : 24 h. Au-delà, la plus ancienne part.
pub const MINUTES_GARDEES: usize = 24 * 60;

/// Minutes envoyées par requête, au plus.
pub const MINUTES_PAR_ENVOI: usize = 120;

/// Délai pendant lequel une nouvelle requête du même flux, avec un `Range`,
/// compte comme la reprise d'une session coupée.
pub const FENETRE_DE_REPRISE: Duration = Duration::from_secs(60);

/// Coupures récentes gardées par serveur, pour reconnaître une reprise.
const COUPURES_GARDEES: usize = 32;

/// Route du site qui reçoit les agrégats.
pub const ROUTE_DU_SITE: &str = "/api/v1/bridge/traffic";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Categorie {
    Audio = 0,
    Cercle = 1,
    Api = 2,
    Images = 3,
}

impl Categorie {
    /// `api` ou `images`, d'après le `content-type` d'une réponse d'API.
    pub fn d_une_reponse(content_type: Option<&str>) -> Categorie {
        match content_type {
            Some(t) if t.trim_start().to_ascii_lowercase().starts_with("image/") => {
                Categorie::Images
            }
            _ => Categorie::Api,
        }
    }
}

/// Famille de format d'un flux audio, d'après son `content-type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Flac = 0,
    Dsd = 1,
    Pcm = 2,
    Compresse = 3,
    Autre = 4,
}

impl Format {
    pub const TOUS: [Format; 5] = [
        Format::Flac,
        Format::Dsd,
        Format::Pcm,
        Format::Compresse,
        Format::Autre,
    ];

    pub fn nom(self) -> &'static str {
        match self {
            Format::Flac => "flac",
            Format::Dsd => "dsd",
            Format::Pcm => "pcm",
            Format::Compresse => "lossy",
            Format::Autre => "other",
        }
    }

    pub fn depuis_content_type(content_type: Option<&str>) -> Format {
        let t = content_type
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if t.contains("flac") {
            Format::Flac
        } else if t.contains("dsf") || t.contains("dff") || t.contains("dsd") {
            Format::Dsd
        } else if t.contains("wav") || t.contains("aiff") || t == "audio/l16" || t == "audio/l24" {
            Format::Pcm
        } else if t.contains("mpeg")
            || t.contains("mp3")
            || t.contains("aac")
            || t.contains("mp4")
            || t.contains("m4a")
            || t.contains("ogg")
            || t.contains("opus")
        {
            Format::Compresse
        } else {
            Format::Autre
        }
    }
}

/// Octets entrants et sortants, par catégorie.
#[derive(Default)]
pub struct Octets {
    entree: [AtomicU64; 4],
    sortie: [AtomicU64; 4],
}

impl Octets {
    fn entree(&self, c: Categorie, n: u64) {
        self.entree[c as usize].fetch_add(n, Ordering::Relaxed);
    }
    fn sortie(&self, c: Categorie, n: u64) {
        self.sortie[c as usize].fetch_add(n, Ordering::Relaxed);
    }
    /// Relève ET remet à zéro : `[catégorie] = (entrée, sortie)`.
    fn relever(&self) -> [(u64, u64); 4] {
        std::array::from_fn(|i| {
            (
                self.entree[i].swap(0, Ordering::Relaxed),
                self.sortie[i].swap(0, Ordering::Relaxed),
            )
        })
    }
}

/// Les compteurs d'un serveur Tune.
#[derive(Default)]
pub struct CompteursServeur {
    octets: Octets,
    sessions: AtomicU64,
    ecoute_ms: AtomicU64,
    reprises: AtomicU64,
    /// Fins non complètes récentes : (clé du flux, instant).
    coupures: std::sync::Mutex<VecDeque<(String, Instant)>>,
}

impl CompteursServeur {
    fn noter_coupure(&self, cle: &str) {
        let mut c = self.coupures.lock().unwrap_or_else(|e| e.into_inner());
        if c.len() >= COUPURES_GARDEES {
            c.pop_front();
        }
        c.push_back((cle.to_string(), Instant::now()));
    }

    /// Une coupure récente du même flux est-elle là ? Elle est consommée.
    fn prendre_coupure(&self, cle: &str) -> bool {
        let mut c = self.coupures.lock().unwrap_or_else(|e| e.into_inner());
        c.retain(|(_, t)| t.elapsed() < FENETRE_DE_REPRISE);
        match c.iter().position(|(k, _)| k == cle) {
            Some(i) => {
                c.remove(i);
                true
            }
            None => false,
        }
    }
}

#[derive(Default)]
struct CompteursFormat {
    octets: AtomicU64,
    sessions: AtomicU64,
    ecoute_ms: AtomicU64,
}

/// Les erreurs du pont que le site affiche.
#[derive(Debug, Clone, Copy)]
pub enum Erreur {
    /// Le serveur Tune visé n'est pas connecté au pont, ou ne peut plus
    /// recevoir de message.
    ServeurAbsent = 0,
    /// Le serveur n'a pas répondu dans les 30 s.
    Delai = 1,
    /// L'attente a été abandonnée sans réponse (déconnexion en cours de
    /// requête).
    CanalRompu = 2,
    /// Le serveur a répondu un 5xx à une requête de flux.
    Amont5xx = 3,
}

/// Comment une session s'est terminée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fin {
    Complete,
    InterrompueParLeClient,
    CoupeeCoteServeur,
}

/// Tous les compteurs du pont.
#[derive(Default)]
pub struct Trafic {
    octets: Octets,
    serveurs: DashMap<String, Arc<CompteursServeur>>,
    ouvertes: AtomicU64,
    completes: AtomicU64,
    interrompues: AtomicU64,
    coupees: AtomicU64,
    reprises: AtomicU64,
    premier_octet_ms: AtomicU64,
    premier_octet_nombre: AtomicU64,
    erreurs: [AtomicU64; 4],
    formats: [CompteursFormat; 5],
    minutes: std::sync::Mutex<VecDeque<Minute>>,
}

impl Trafic {
    pub fn nouveau() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Les compteurs d'un serveur — créés à la première demande.
    pub fn serveur(&self, server_id: &str) -> Arc<CompteursServeur> {
        if let Some(c) = self.serveurs.get(server_id) {
            return c.clone();
        }
        self.serveurs
            .entry(server_id.to_string())
            .or_default()
            .clone()
    }

    pub fn entree(&self, serveur: &CompteursServeur, c: Categorie, n: usize) {
        self.octets.entree(c, n as u64);
        serveur.octets.entree(c, n as u64);
    }

    pub fn sortie(&self, serveur: &CompteursServeur, c: Categorie, n: usize) {
        self.octets.sortie(c, n as u64);
        serveur.octets.sortie(c, n as u64);
    }

    pub fn erreur(&self, e: Erreur) {
        self.erreurs[e as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// Relève tous les compteurs (remis à zéro) dans la minute qui commence à
    /// `minute` (secondes Unix), et la range dans la file d'envoi.
    pub fn clore_minute(&self, minute: i64) -> Minute {
        let octets = self.octets.relever();
        let mut servers = Vec::new();
        // Un serveur sans activité et sans connexion (personne d'autre ne
        // tient son `Arc`) quitte la carte : elle reste bornée par le nombre
        // de serveurs connectés.
        self.serveurs.retain(|server_id, c| {
            let o = c.octets.relever();
            let sessions = c.sessions.swap(0, Ordering::Relaxed);
            let ecoute_ms = c.ecoute_ms.swap(0, Ordering::Relaxed);
            let reprises = c.reprises.swap(0, Ordering::Relaxed);
            let entree: u64 = o.iter().map(|(e, _)| e).sum();
            let sortie: u64 = o.iter().map(|(_, s)| s).sum();
            let actif = entree + sortie + sessions + ecoute_ms + reprises > 0;
            if actif {
                servers.push(MinuteServeur {
                    server_id: server_id.clone(),
                    bytes_in: entree,
                    bytes_out: sortie,
                    stream_out: o[Categorie::Audio as usize].1 + o[Categorie::Cercle as usize].1,
                    sessions,
                    listening_ms: ecoute_ms,
                    resumes: reprises,
                });
            }
            actif || Arc::strong_count(c) > 1
        });
        servers.sort_by(|a, b| a.server_id.cmp(&b.server_id));

        let formats = Format::TOUS
            .iter()
            .filter_map(|f| {
                let c = &self.formats[*f as usize];
                let m = MinuteFormat {
                    format: f.nom(),
                    bytes_out: c.octets.swap(0, Ordering::Relaxed),
                    sessions: c.sessions.swap(0, Ordering::Relaxed),
                    listening_ms: c.ecoute_ms.swap(0, Ordering::Relaxed),
                };
                (m.bytes_out + m.sessions + m.listening_ms > 0).then_some(m)
            })
            .collect();

        let par = |c: Categorie| SensOctets {
            r#in: octets[c as usize].0,
            out: octets[c as usize].1,
        };
        let m = Minute {
            minute,
            categories: MinuteCategories {
                audio: par(Categorie::Audio),
                circle: par(Categorie::Cercle),
                api: par(Categorie::Api),
                images: par(Categorie::Images),
            },
            servers,
            sessions: MinuteSessions {
                started: self.ouvertes.swap(0, Ordering::Relaxed),
                completed: self.completes.swap(0, Ordering::Relaxed),
                client_closed: self.interrompues.swap(0, Ordering::Relaxed),
                server_lost: self.coupees.swap(0, Ordering::Relaxed),
                resumed: self.reprises.swap(0, Ordering::Relaxed),
            },
            first_byte: PremierOctet {
                sum_ms: self.premier_octet_ms.swap(0, Ordering::Relaxed),
                count: self.premier_octet_nombre.swap(0, Ordering::Relaxed),
            },
            formats,
            errors: MinuteErreurs {
                server_absent: self.erreurs[Erreur::ServeurAbsent as usize]
                    .swap(0, Ordering::Relaxed),
                timeout: self.erreurs[Erreur::Delai as usize].swap(0, Ordering::Relaxed),
                channel_broken: self.erreurs[Erreur::CanalRompu as usize]
                    .swap(0, Ordering::Relaxed),
                upstream_5xx: self.erreurs[Erreur::Amont5xx as usize].swap(0, Ordering::Relaxed),
            },
        };

        let mut file = self.minutes.lock().unwrap_or_else(|e| e.into_inner());
        while file.len() >= MINUTES_GARDEES {
            file.pop_front();
        }
        file.push_back(m.clone());
        m
    }

    /// Les plus anciennes minutes en attente d'envoi, au plus `n`.
    pub fn minutes_en_attente(&self, n: usize) -> Vec<Minute> {
        let file = self.minutes.lock().unwrap_or_else(|e| e.into_inner());
        file.iter().take(n).cloned().collect()
    }

    /// Retire de la file les minutes acceptées par le site.
    pub fn acquitter(&self, minutes: &[i64]) {
        let mut file = self.minutes.lock().unwrap_or_else(|e| e.into_inner());
        file.retain(|m| !minutes.contains(&m.minute));
    }

    pub fn nombre_en_attente(&self) -> usize {
        self.minutes.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// Ouvre une session d'écoute sur un corps audio.
    ///
    /// `cle` désigne le flux (chemin du flux, ou piste de cercle) ; `range`
    /// est le `Range` de la requête, pour reconnaître une reprise.
    pub fn ouvrir_session(self: &Arc<Self>, o: Ouverture<'_>) -> Session {
        let Ouverture {
            serveur,
            categorie,
            content_type,
            cle,
            range,
            debut_requete,
            fin_propre,
        } = o;
        let format = Format::depuis_content_type(content_type);
        self.ouvertes.fetch_add(1, Ordering::Relaxed);
        serveur.sessions.fetch_add(1, Ordering::Relaxed);
        self.formats[format as usize]
            .sessions
            .fetch_add(1, Ordering::Relaxed);
        if range_hors_debut(range) && serveur.prendre_coupure(&cle) {
            self.reprises.fetch_add(1, Ordering::Relaxed);
            serveur.reprises.fetch_add(1, Ordering::Relaxed);
        }
        Session {
            trafic: self.clone(),
            serveur,
            categorie,
            format,
            cle,
            ouverte_a: Instant::now(),
            debut_requete,
            premier_octet_vu: false,
            fin_atteinte: false,
            fin_propre,
        }
    }
}

/// Ce qu'il faut pour ouvrir une [`Session`].
pub struct Ouverture<'a> {
    pub serveur: Arc<CompteursServeur>,
    pub categorie: Categorie,
    pub content_type: Option<&'a str>,
    /// Le flux : chemin, ou piste de cercle.
    pub cle: String,
    /// Le `Range` de la requête, pour reconnaître une reprise.
    pub range: Option<&'a str>,
    pub debut_requete: Instant,
    /// Posé par `relay.stream_end`.
    pub fin_propre: Arc<AtomicBool>,
}

/// Un `Range` qui ne commence pas à l'octet zéro.
fn range_hors_debut(range: Option<&str>) -> bool {
    let Some(r) = range else { return false };
    let Some(reste) = r.trim().strip_prefix("bytes=") else {
        return false;
    };
    let debut = reste.split(['-', ',']).next().unwrap_or("").trim();
    // `bytes=-500` (les 500 derniers octets) n'est pas une reprise.
    !debut.is_empty() && debut.parse::<u64>().is_ok_and(|n| n > 0)
}

/// Une session d'écoute : un corps audio ouvert vers un navigateur.
///
/// Elle compte les octets rendus au fil des morceaux, le délai jusqu'au
/// premier, et sa fin quand elle tombe (`Drop`).
pub struct Session {
    trafic: Arc<Trafic>,
    serveur: Arc<CompteursServeur>,
    categorie: Categorie,
    format: Format,
    cle: String,
    ouverte_a: Instant,
    debut_requete: Instant,
    premier_octet_vu: bool,
    /// Le canal de morceaux s'est fermé (fin du flux, propre ou non).
    fin_atteinte: bool,
    /// Posé par `relay.stream_end` : la fin est propre.
    fin_propre: Arc<AtomicBool>,
}

impl Session {
    /// Un morceau vient de partir vers le navigateur.
    pub fn morceau_rendu(&mut self, n: usize) {
        self.trafic.sortie(&self.serveur, self.categorie, n);
        self.trafic.formats[self.format as usize]
            .octets
            .fetch_add(n as u64, Ordering::Relaxed);
        if !self.premier_octet_vu && n > 0 {
            self.premier_octet_vu = true;
            let ms = self.debut_requete.elapsed().as_millis() as u64;
            self.trafic
                .premier_octet_ms
                .fetch_add(ms, Ordering::Relaxed);
            self.trafic
                .premier_octet_nombre
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Le canal de morceaux est fermé : plus rien ne viendra.
    pub fn fin_du_flux(&mut self) {
        self.fin_atteinte = true;
    }

    fn fin(&self) -> Fin {
        if !self.fin_atteinte {
            Fin::InterrompueParLeClient
        } else if self.fin_propre.load(Ordering::Relaxed) {
            Fin::Complete
        } else {
            Fin::CoupeeCoteServeur
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let ms = self.ouverte_a.elapsed().as_millis() as u64;
        self.serveur.ecoute_ms.fetch_add(ms, Ordering::Relaxed);
        self.trafic.formats[self.format as usize]
            .ecoute_ms
            .fetch_add(ms, Ordering::Relaxed);
        let t = &self.trafic;
        match self.fin() {
            Fin::Complete => {
                t.completes.fetch_add(1, Ordering::Relaxed);
            }
            Fin::InterrompueParLeClient => {
                t.interrompues.fetch_add(1, Ordering::Relaxed);
                self.serveur.noter_coupure(&self.cle);
            }
            Fin::CoupeeCoteServeur => {
                t.coupees.fetch_add(1, Ordering::Relaxed);
                self.serveur.noter_coupure(&self.cle);
            }
        }
    }
}

// --- Le relevé d'une minute, tel qu'il part au site ---

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SensOctets {
    pub r#in: u64,
    pub out: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MinuteCategories {
    pub audio: SensOctets,
    pub circle: SensOctets,
    pub api: SensOctets,
    pub images: SensOctets,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MinuteServeur {
    pub server_id: String,
    pub bytes_in: u64,
    pub bytes_out: u64,
    /// Sortie des flux audio (`audio` + `circle`).
    pub stream_out: u64,
    pub sessions: u64,
    pub listening_ms: u64,
    pub resumes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MinuteSessions {
    pub started: u64,
    pub completed: u64,
    pub client_closed: u64,
    pub server_lost: u64,
    pub resumed: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PremierOctet {
    pub sum_ms: u64,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MinuteFormat {
    pub format: &'static str,
    pub bytes_out: u64,
    pub sessions: u64,
    pub listening_ms: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MinuteErreurs {
    pub server_absent: u64,
    pub timeout: u64,
    pub channel_broken: u64,
    pub upstream_5xx: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Minute {
    /// Début de la minute, en secondes Unix (UTC).
    pub minute: i64,
    pub categories: MinuteCategories,
    pub servers: Vec<MinuteServeur>,
    pub sessions: MinuteSessions,
    pub first_byte: PremierOctet,
    pub formats: Vec<MinuteFormat>,
    pub errors: MinuteErreurs,
}

// --- L'envoi au site ---

/// Jeton de service du pont : le même que pour `listen/verify`.
pub fn jeton_depuis_environnement() -> Option<String> {
    std::env::var(crate::circle::JETON_DE_VERIFICATION_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub fn base_depuis_environnement() -> String {
    std::env::var(crate::licence::CLOUD_BASE_ENV)
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "https://mozaiklabs.fr".to_string())
}

/// Ce que donne un envoi.
#[derive(Debug, PartialEq, Eq)]
pub enum Envoi {
    /// Rien à envoyer, ou pas de jeton.
    Rien,
    /// `n` minutes acceptées par le site.
    Accepte(usize),
    /// Le site a refusé (statut HTTP) ou n'a pas répondu (`None`) : les
    /// minutes restent dans la file.
    Echec(Option<u16>),
}

/// Envoie au site les minutes en attente, par paquets, jusqu'à vider la file
/// ou rencontrer un échec.
pub async fn pousser_au_site(
    trafic: &Trafic,
    client: &reqwest::Client,
    base: &str,
    jeton: Option<&str>,
) -> Envoi {
    let Some(jeton) = jeton else {
        return Envoi::Rien;
    };
    let url = format!("{base}{ROUTE_DU_SITE}");
    let mut acceptees = 0;
    // Borne : autant de paquets que la file en compte au depart. Un site qui
    // repondrait 200 sans que la file se vide ne doit pas faire tourner le
    // pont en rond.
    let tours = trafic.nombre_en_attente().div_ceil(MINUTES_PAR_ENVOI);
    for _ in 0..tours {
        let paquet = trafic.minutes_en_attente(MINUTES_PAR_ENVOI);
        if paquet.is_empty() {
            break;
        }
        let corps = serde_json::json!({
            "bridge_version": env!("CARGO_PKG_VERSION"),
            "minutes": paquet,
        });
        let reponse = client
            .post(&url)
            .bearer_auth(jeton)
            .header("accept", "application/json")
            .json(&corps)
            .send()
            .await;
        match reponse {
            Ok(r) if r.status().is_success() => {
                let ids: Vec<i64> = paquet.iter().map(|m| m.minute).collect();
                trafic.acquitter(&ids);
                acceptees += ids.len();
            }
            Ok(r) => return Envoi::Echec(Some(r.status().as_u16())),
            Err(_) => return Envoi::Echec(None),
        }
    }
    if acceptees == 0 {
        Envoi::Rien
    } else {
        Envoi::Accepte(acceptees)
    }
}

fn secondes_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// La boucle de fond : à chaque changement de minute, clore la précédente,
/// puis envoyer ce qui attend.
pub async fn boucle(trafic: Arc<Trafic>) {
    let jeton = jeton_depuis_environnement();
    let base = base_depuis_environnement();
    if jeton.is_none() {
        warn!(
            "bridge_trafic_non_envoye — {} absent : les agregats restent sur le pont (24 h au plus)",
            crate::circle::JETON_DE_VERIFICATION_ENV
        );
    } else {
        info!(base = %base, "bridge_trafic_envoi_actif");
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    let mut dernier_echec: Option<Option<u16>> = None;
    loop {
        let maintenant = secondes_unix();
        let prochaine = (maintenant / 60 + 1) * 60;
        tokio::time::sleep(Duration::from_secs((prochaine - maintenant).max(1) as u64)).await;
        trafic.clore_minute(prochaine - 60);
        match pousser_au_site(&trafic, &client, &base, jeton.as_deref()).await {
            Envoi::Echec(statut) => {
                // Dit une fois par changement d'état, pas une fois par minute.
                if dernier_echec != Some(statut) {
                    warn!(
                        statut = ?statut,
                        en_attente = trafic.nombre_en_attente(),
                        "bridge_trafic_envoi_refuse"
                    );
                }
                dernier_echec = Some(statut);
            }
            _ => {
                if dernier_echec.take().is_some() {
                    info!("bridge_trafic_envoi_retabli");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_categories_d_une_reponse_d_api() {
        assert_eq!(
            Categorie::d_une_reponse(Some("image/jpeg")),
            Categorie::Images
        );
        assert_eq!(
            Categorie::d_une_reponse(Some("Image/PNG; q=1")),
            Categorie::Images
        );
        assert_eq!(
            Categorie::d_une_reponse(Some("application/json")),
            Categorie::Api
        );
        assert_eq!(Categorie::d_une_reponse(None), Categorie::Api);
    }

    #[test]
    fn les_familles_de_format() {
        assert_eq!(
            Format::depuis_content_type(Some("audio/flac")),
            Format::Flac
        );
        assert_eq!(
            Format::depuis_content_type(Some("audio/x-flac")),
            Format::Flac
        );
        assert_eq!(Format::depuis_content_type(Some("audio/dsf")), Format::Dsd);
        assert_eq!(
            Format::depuis_content_type(Some("audio/x-dff")),
            Format::Dsd
        );
        assert_eq!(Format::depuis_content_type(Some("audio/wav")), Format::Pcm);
        assert_eq!(
            Format::depuis_content_type(Some("audio/mpeg")),
            Format::Compresse
        );
        assert_eq!(
            Format::depuis_content_type(Some("application/octet-stream")),
            Format::Autre
        );
        assert_eq!(Format::depuis_content_type(None), Format::Autre);
    }

    #[test]
    fn seul_un_range_hors_debut_peut_etre_une_reprise() {
        assert!(range_hors_debut(Some("bytes=1000-")));
        assert!(range_hors_debut(Some("bytes=1-2")));
        assert!(!range_hors_debut(Some("bytes=0-")));
        assert!(!range_hors_debut(Some("bytes=-500")));
        assert!(!range_hors_debut(Some("items=5-")));
        assert!(!range_hors_debut(None));
    }

    #[test]
    fn une_minute_releve_et_remet_a_zero() {
        let t = Trafic::nouveau();
        let s = t.serveur("srv");
        t.entree(&s, Categorie::Api, 10);
        t.sortie(&s, Categorie::Images, 7);
        t.erreur(Erreur::Delai);

        let m = t.clore_minute(600);
        assert_eq!(m.minute, 600);
        assert_eq!(m.categories.api, SensOctets { r#in: 10, out: 0 });
        assert_eq!(m.categories.images, SensOctets { r#in: 0, out: 7 });
        assert_eq!(m.errors.timeout, 1);
        assert_eq!(m.servers.len(), 1);
        assert_eq!(m.servers[0].bytes_in, 10);
        assert_eq!(m.servers[0].bytes_out, 7);

        let vide = t.clore_minute(660);
        assert_eq!(vide.categories.api, SensOctets { r#in: 0, out: 0 });
        assert_eq!(vide.errors.timeout, 0);
        assert!(vide.servers.is_empty(), "rien ne s'est passe : {vide:?}");
    }

    /// La carte des serveurs reste bornée : un serveur inactif dont personne
    /// ne tient plus les compteurs disparaît ; un serveur connecté reste.
    #[test]
    fn un_serveur_parti_et_inactif_quitte_la_carte() {
        let t = Trafic::nouveau();
        let connecte = t.serveur("connecte");
        {
            let parti = t.serveur("parti");
            t.sortie(&parti, Categorie::Api, 1);
        }
        t.clore_minute(0); // « parti » était actif : il reste cette fois-ci
        t.clore_minute(60);
        assert!(t.serveurs.contains_key("connecte"));
        assert!(!t.serveurs.contains_key("parti"));
        drop(connecte);
    }

    #[test]
    fn la_file_est_bornee_a_24_heures() {
        let t = Trafic::nouveau();
        for i in 0..(MINUTES_GARDEES as i64 + 10) {
            t.clore_minute(i * 60);
        }
        assert_eq!(t.nombre_en_attente(), MINUTES_GARDEES);
        // Les plus anciennes sont parties.
        assert_eq!(t.minutes_en_attente(1)[0].minute, 10 * 60);
    }

    #[test]
    fn le_releve_part_avec_les_noms_du_contrat() {
        let t = Trafic::nouveau();
        let m = t.clore_minute(120);
        let v = serde_json::to_value(&m).unwrap();
        for cle in [
            "minute",
            "categories",
            "servers",
            "sessions",
            "first_byte",
            "formats",
            "errors",
        ] {
            assert!(v.get(cle).is_some(), "{cle} manque : {v}");
        }
        assert!(v["categories"]["audio"].get("in").is_some());
        assert!(v["categories"]["circle"].get("out").is_some());
    }
}
