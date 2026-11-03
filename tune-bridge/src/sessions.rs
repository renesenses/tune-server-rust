//! Sessions d'écoute à distance ouvertes par laissez-passer (#6603).
//!
//! L'app présente un laissez-passer délivré par mozaiklabs.fr et la
//! signature d'un défi par la clé de son appareil. Le relais vérifie les deux
//! HORS LIGNE (`tune-laissez-passer`), demande l'ouverture au site, puis rend
//! un jeton de session opaque de 256 bits. Ce jeton prend exactement la place
//! de l'ancien jeton du pont : mêmes en-têtes (`X-Bridge-Token`), mêmes
//! paramètres (`?token=`). Il reste le même toute la session, renouvellements
//! compris : un lecteur audio garde l'URL de son flux pour toutes les
//! requêtes `Range` d'une piste.
//!
//! # Une écoute à distance par compte
//!
//! Une session ne compte que si elle transporte de l'audio (un premier
//! `/stream/relay`) et cesse de compter après [`AUDIO_ACTIF_S`] sans audio.
//! Quand une session du compte se met à jouer, les autres sessions du même
//! compte qui jouaient sont coupées : la nouvelle prend la main (politique
//! B). Le relais l'applique seul, tout de suite ; le site l'applique en plus à
//! travers les chemins (relais et direct) au battement suivant.
//!
//! # Battement
//!
//! Chaque minute, le relais envoie au site l'état de toutes ses sessions. La
//! réponse dit lesquelles couper et ce qui est révoqué : une révocation agit
//! donc en moins d'une minute, sans attendre l'expiration du laissez-passer.
//!
//! # Secours
//!
//! Site injoignable : un laissez-passer encore valide est accepté (la
//! vérification est hors ligne) et l'ouverture est rejouée au retour du site.
//! Un laissez-passer expiré depuis moins de [`SECOURS_S`] est accepté si
//! l'appareil est connu du relais depuis moins de [`APPAREIL_CONNU_S`]. Tout
//! le reste est refusé (`cloud_unavailable`).
//!
//! # Chemin hérité
//!
//! Pendant la coexistence, l'ancien jeton du pont reste accepté, compté à
//! part par serveur, avec l'en-tête `X-Tune-Avertissement: lien-obsolete`.
//! [`ENV_JETON_HERITE`] le ferme : il répond alors `401 lien_obsolete`.
//!
//! Aucun jeton, aucune IP n'est écrit dans les traces.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use dashmap::DashMap;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{info, warn};
use tune_laissez_passer::{Contexte, JeuDeCles, LaissezPasser};

use crate::site::{Battement, BattementSession, DemandeOuverture, Site};

/// Clé publique racine minisign (ligne base64 du `.pub`). À défaut,
/// [`tune_laissez_passer::RACINE_EMBARQUEE`].
pub const ENV_RACINE: &str = "TUNE_REMOTE_RACINE";
/// Interrupteur du chemin hérité (ancien jeton du pont). Ouvert par défaut
/// pendant la coexistence ; `0`, `false`, `off` ou `no` le ferme.
pub const ENV_JETON_HERITE: &str = "TUNE_BRIDGE_JETON_HERITE";
/// En-tête de réponse posé sur tout ce que sert le chemin hérité.
pub const ENTETE_AVERTISSEMENT: &str = "x-tune-avertissement";

/// Durée de vie d'un défi.
pub const DUREE_DEFI_S: i64 = 60;
/// Défis en attente, au plus.
pub const DEFIS_MAX: usize = 10_000;
/// Sessions ouvertes, au plus.
pub const SESSIONS_MAX: usize = 10_000;
/// Fenêtre de secours : un laissez-passer expiré depuis moins longtemps reste
/// accepté quand le site ne répond pas.
pub const SECOURS_S: i64 = 24 * 3600;
/// Un appareil vu (ouverture confirmée par le site) depuis moins longtemps
/// est « connu » pour la règle de secours.
pub const APPAREIL_CONNU_S: i64 = 7 * 24 * 3600;
/// Une session cesse de compter comme écoute après ce délai sans audio.
pub const AUDIO_ACTIF_S: i64 = 120;
/// Durée pendant laquelle une révocation reçue du site est gardée.
pub const REVOCATION_GARDEE_S: i64 = 3600;
/// Durée pendant laquelle une session coupée répond encore son motif.
pub const COUPURE_GARDEE_S: i64 = 600;
/// Au plus une relecture du jeu de clés par minute sur un `kid` inconnu.
pub const KID_INCONNU_S: i64 = 60;
/// Période du battement.
pub const PERIODE_BATTEMENT: Duration = Duration::from_secs(60);
/// Période de relecture du jeu de clés.
pub const PERIODE_JEU_DE_CLES: Duration = Duration::from_secs(6 * 3600);

/// Source de l'heure, en secondes Unix. Remplaçable dans les tests.
pub type Horloge = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn horloge_systeme() -> Horloge {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    })
}

/// Un refus rendu à l'app : statut HTTP et corps `{code, motif?, appareil?}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rejet {
    #[serde(skip)]
    pub statut: u16,
    pub code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub motif: Option<String>,
    /// Pour une coupure par la limite : l'appareil qui a pris la main.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub appareil: Option<String>,
}

impl Rejet {
    pub fn nouveau(statut: u16, code: &str) -> Self {
        Self {
            statut,
            code: code.to_string(),
            motif: None,
            appareil: None,
        }
    }

    fn avec_motif(mut self, motif: impl Into<String>) -> Self {
        self.motif = Some(motif.into());
        self
    }

    pub fn non_autorise() -> Self {
        Self::nouveau(401, "non_autorise")
    }

    /// `401 session_coupee` : ce que voit un appareil coupé.
    pub fn coupee(motif: &str, appareil: Option<String>) -> Self {
        Self {
            statut: 401,
            code: "session_coupee".into(),
            motif: Some(motif.to_string()),
            appareil,
        }
    }

    fn du_refus(r: tune_laissez_passer::Refus) -> Self {
        Self::nouveau(401, r.code())
    }

    pub fn en_reponse(&self) -> Response {
        let statut = StatusCode::from_u16(self.statut).unwrap_or(StatusCode::UNAUTHORIZED);
        (statut, axum::Json(self)).into_response()
    }
}

/// Corps de `POST /session/{server_id}` et de `POST /session/renouveler`.
#[derive(Debug, Deserialize)]
pub struct CorpsSession {
    pub laissez_passer: String,
    pub defi: String,
    pub signature: String,
}

/// Réponse d'une ouverture ou d'un renouvellement.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Ouverte {
    /// Le jeton de session, à présenter comme l'ancien jeton du pont.
    pub jeton: String,
    /// Fin de validité (secondes Unix) : l'`exp` du laissez-passer.
    pub expire: i64,
    /// Ouverte pendant une panne du site.
    pub secours: bool,
}

/// Une session ouverte. Pas de `Debug` : elle porte son jeton.
pub struct Session {
    jeton: String,
    pub sid: String,
    pub sub: String,
    pub did: String,
    pk_appareil: String,
    jti: std::sync::Mutex<String>,
    exp: AtomicI64,
    nom_appareil: std::sync::Mutex<Option<String>>,
    session_site: std::sync::Mutex<Option<String>>,
    a_rejouer: AtomicBool,
    secours: AtomicBool,
    octets: AtomicU64,
    /// Dernier audio servi, secondes Unix ; `i64::MIN` : jamais.
    dernier_audio: AtomicI64,
    coupure: watch::Sender<Option<Rejet>>,
}

impl Session {
    /// Compte des octets servis par cette session.
    pub fn compter(&self, n: usize) {
        self.octets.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// Suit la coupure de la session : passe à `Some(rejet)` quand elle tombe.
    pub fn coupure(&self) -> watch::Receiver<Option<Rejet>> {
        self.coupure.subscribe()
    }

    pub fn session_site(&self) -> Option<String> {
        self.session_site.lock().unwrap().clone()
    }

    pub fn nom_appareil(&self) -> Option<String> {
        self.nom_appareil.lock().unwrap().clone()
    }

    pub fn expire(&self) -> i64 {
        self.exp.load(Ordering::Relaxed)
    }

    fn audio_actif(&self, maintenant: i64) -> bool {
        let d = self.dernier_audio.load(Ordering::Relaxed);
        d != i64::MIN && maintenant - d <= AUDIO_ACTIF_S
    }

    /// Ce que l'appareil coupé affiche : le nom de l'appareil qui a pris la
    /// main, à défaut son identifiant.
    fn designation(&self) -> String {
        self.nom_appareil().unwrap_or_else(|| self.did.clone())
    }
}

/// Ce qu'une requête présente au relais, une fois reconnu.
#[derive(Clone)]
pub enum Acces {
    Session(Arc<Session>),
    /// L'ancien jeton du pont, pendant la coexistence.
    Herite,
}

/// La table des sessions et tout ce qui les entoure.
pub struct Sessions {
    racine: String,
    site: Arc<dyn Site>,
    horloge: Horloge,
    jeton_herite_ouvert: bool,
    jeu: std::sync::RwLock<Option<Arc<JeuDeCles>>>,
    dernier_kid_inconnu: AtomicI64,
    site_injoignable: AtomicBool,
    par_jeton: DashMap<String, Arc<Session>>,
    /// Défi → (server_id, fin de validité).
    defis: DashMap<String, (String, i64)>,
    /// device_id → dernière ouverture confirmée par le site.
    appareils_connus: DashMap<String, i64>,
    revoques_jti: DashMap<String, i64>,
    revoques_did: DashMap<String, i64>,
    /// Jeton d'une session coupée → (motif, heure de la coupure).
    coupees: DashMap<String, (Rejet, i64)>,
    /// Requêtes servies par le chemin hérité, par serveur, depuis le dernier
    /// battement.
    herite: DashMap<String, AtomicU64>,
    /// Clés de serveur données par le site, pour quand il ne répond plus.
    cles_serveurs: DashMap<String, String>,
}

impl Sessions {
    pub fn nouveau(
        site: Arc<dyn Site>,
        racine: &str,
        jeton_herite_ouvert: bool,
        horloge: Horloge,
    ) -> Arc<Self> {
        Arc::new(Self {
            racine: racine.trim().to_string(),
            site,
            horloge,
            jeton_herite_ouvert,
            jeu: std::sync::RwLock::new(None),
            dernier_kid_inconnu: AtomicI64::new(i64::MIN),
            site_injoignable: AtomicBool::new(false),
            par_jeton: DashMap::new(),
            defis: DashMap::new(),
            appareils_connus: DashMap::new(),
            revoques_jti: DashMap::new(),
            revoques_did: DashMap::new(),
            coupees: DashMap::new(),
            herite: DashMap::new(),
            cles_serveurs: DashMap::new(),
        })
    }

    pub fn depuis_environnement() -> Arc<Self> {
        let racine = std::env::var(ENV_RACINE)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| tune_laissez_passer::RACINE_EMBARQUEE.to_string());
        if racine.is_empty() {
            // Dit HAUT et fort : sans racine, aucun jeu de clés n'est
            // accepté, donc aucun laissez-passer. Pas de repli permissif.
            warn!(
                "ecoute_distante_laissez_passer_ferme — {ENV_RACINE} absent et aucune racine \
                 embarquee : aucun laissez-passer ne sera accepte"
            );
        }
        let herite = interrupteur_herite(std::env::var(ENV_JETON_HERITE).ok().as_deref());
        if herite {
            info!("ecoute_distante_jeton_herite_ouvert — l'ancien lien d'acces reste accepte");
        } else {
            warn!("ecoute_distante_jeton_herite_ferme — l'ancien lien d'acces est refuse");
        }
        Self::nouveau(
            Arc::new(crate::site::SiteHttp::depuis_environnement()),
            &racine,
            herite,
            horloge_systeme(),
        )
    }

    pub fn maintenant(&self) -> i64 {
        (self.horloge)()
    }

    pub fn jeton_herite_ouvert(&self) -> bool {
        self.jeton_herite_ouvert
    }

    /// Le relais sait-il vérifier un laissez-passer, c'est-à-dire tient-il un
    /// jeu de clés signé par la racine ? Annoncé au serveur dans
    /// `relay.registered` : tant que c'est faux, le serveur garde l'ancien
    /// parcours tel quel.
    pub fn actif(&self) -> bool {
        self.jeu().is_some()
    }

    pub fn site_injoignable(&self) -> bool {
        self.site_injoignable.load(Ordering::Relaxed)
    }

    fn jeu(&self) -> Option<Arc<JeuDeCles>> {
        self.jeu.read().unwrap().clone()
    }

    /// Relit le jeu de clés sur le site. N'accepte qu'un jeu signé par la
    /// racine, et jamais plus ancien que celui qu'on tient.
    pub async fn rafraichir_jeu(&self) -> Result<u64, String> {
        let publication = match self.site.jeu_de_cles().await {
            Ok(p) => p,
            Err(e) => {
                self.site_injoignable.store(true, Ordering::Relaxed);
                return Err(e.0);
            }
        };
        // Le site a répondu : il n'est pas en panne, quoi qu'on pense du jeu.
        self.site_injoignable.store(false, Ordering::Relaxed);
        let jeu = JeuDeCles::depuis_publication(&publication, &self.racine)
            .map_err(|r| format!("jeu de cles refuse : {}", r.code()))?;
        let mut tenu = self.jeu.write().unwrap();
        if let Some(actuel) = tenu.as_ref()
            && actuel.version() > jeu.version()
        {
            return Err(format!(
                "jeu de cles plus ancien ({} < {})",
                jeu.version(),
                actuel.version()
            ));
        }
        let version = jeu.version();
        *tenu = Some(Arc::new(jeu));
        Ok(version)
    }

    /// Un défi neuf pour ce serveur.
    pub fn nouveau_defi(&self, server_id: &str) -> Result<(String, i64), Rejet> {
        let maintenant = self.maintenant();
        if self.defis.len() >= DEFIS_MAX {
            self.defis.retain(|_, (_, fin)| *fin >= maintenant);
            if self.defis.len() >= DEFIS_MAX {
                return Err(Rejet::nouveau(503, "trop_de_defis"));
            }
        }
        let defi = tune_laissez_passer::jeton_aleatoire()
            .ok_or_else(|| Rejet::nouveau(503, "aleatoire_indisponible"))?;
        let fin = maintenant + DUREE_DEFI_S;
        self.defis
            .insert(defi.clone(), (server_id.to_string(), fin));
        Ok((defi, fin))
    }

    /// Un défi ne sert qu'une fois, pour son serveur, avant sa fin.
    fn consommer_defi(&self, server_id: &str, defi: &str) -> Result<(), Rejet> {
        let maintenant = self.maintenant();
        match self.defis.remove(defi) {
            Some((_, (sid, fin))) if sid == server_id && fin >= maintenant => Ok(()),
            _ => Err(Rejet::nouveau(401, "defi")),
        }
    }

    /// Vérifie un laissez-passer ; relit le jeu de clés sur un `kid`
    /// inconnu, une fois par minute au plus.
    async fn verifier(
        &self,
        jeton: &str,
        server_id: &str,
        grace: i64,
    ) -> Result<LaissezPasser, Rejet> {
        let maintenant = self.maintenant();
        let kid = JeuDeCles::kid_de(jeton);
        let connu = self
            .jeu()
            .is_some_and(|j| kid.as_deref().is_some_and(|k| j.connait(k)));
        if !connu && kid.is_some() {
            let dernier = self.dernier_kid_inconnu.load(Ordering::Relaxed);
            if dernier == i64::MIN || maintenant - dernier >= KID_INCONNU_S {
                self.dernier_kid_inconnu
                    .store(maintenant, Ordering::Relaxed);
                if let Err(e) = self.rafraichir_jeu().await {
                    warn!(erreur = %e, "ecoute_distante_jeu_de_cles_non_relu");
                }
            }
        }
        let Some(jeu) = self.jeu() else {
            return Err(Rejet::nouveau(503, "jeu_de_cles_absent"));
        };
        jeu.verifier(
            jeton,
            &Contexte {
                server_id,
                maintenant,
                grace_expiration: grace,
            },
        )
        .map_err(Rejet::du_refus)
    }

    /// Le site est-il en panne, à l'instant ? Sondé par la route publique du
    /// jeu de clés, jamais déduit d'un constat ancien : un relais sans
    /// session ne bat pas, et un drapeau posé pendant une panne passée
    /// laisserait entrer en secours un laissez-passer expiré alors que le
    /// site est revenu. Le constat met le drapeau à jour dans les deux sens.
    async fn site_en_panne(&self) -> bool {
        let en_panne = self.site.jeu_de_cles().await.is_err();
        self.site_injoignable.store(en_panne, Ordering::Relaxed);
        en_panne
    }

    fn revoque(&self, lp: &LaissezPasser) -> bool {
        self.revoques_jti.contains_key(&lp.revendications.jti)
            || self.revoques_did.contains_key(&lp.revendications.did)
    }

    fn appareil_connu(&self, did: &str, maintenant: i64) -> bool {
        self.appareils_connus
            .get(did)
            .is_some_and(|vu| maintenant - *vu <= APPAREIL_CONNU_S)
    }

    /// Ouvre une session. `cle_serveur` : la clé que le serveur visé a
    /// prouvée en s'enregistrant, s'il en a une.
    pub async fn ouvrir(
        &self,
        server_id: &str,
        cle_serveur: Option<&str>,
        corps: &CorpsSession,
    ) -> Result<Ouverte, Rejet> {
        self.consommer_defi(server_id, &corps.defi)?;
        let (lp, expire) = match self.verifier(&corps.laissez_passer, server_id, 0).await {
            Ok(lp) => (lp, false),
            // Expiré : il ne peut valoir qu'en secours, si le site est en
            // panne. Sinon l'app doit demander un nouveau laissez-passer.
            Err(r) if r.code == "expire" && self.site_en_panne().await => (
                self.verifier(&corps.laissez_passer, server_id, SECOURS_S)
                    .await?,
                true,
            ),
            Err(r) => return Err(r),
        };
        lp.verifier_defi(server_id, &corps.defi, &corps.signature)
            .map_err(Rejet::du_refus)?;
        if self.revoque(&lp) {
            return Err(Rejet::nouveau(403, "revoque"));
        }
        if let (Some(attendue), Some(annoncee)) = (cle_serveur, lp.revendications.srv_pk.as_deref())
            && attendue != annoncee
        {
            return Err(Rejet::nouveau(401, "serveur"));
        }
        if self.par_jeton.len() >= SESSIONS_MAX {
            return Err(Rejet::nouveau(503, "trop_de_sessions"));
        }

        let maintenant = self.maintenant();
        let r = &lp.revendications;
        if expire {
            if !self.appareil_connu(&r.did, maintenant) {
                return Err(Rejet::nouveau(503, "cloud_unavailable"));
            }
            let s = self.creer(&lp, None, None, true)?;
            info!(did = %r.did, "ecoute_distante_session_ouverte_en_secours");
            return Ok(self.ouverte(&s));
        }

        let demande = DemandeOuverture {
            jti: r.jti.clone(),
            sub: r.sub.clone(),
            did: r.did.clone(),
            sid: r.sid.clone(),
            chemin: "relais",
            secours: false,
        };
        match self.site.ouvrir_session(&demande).await {
            Ok(rep) => {
                self.site_injoignable.store(false, Ordering::Relaxed);
                if !rep.ok {
                    let motif = rep.motif.unwrap_or_else(|| "refuse".into());
                    info!(did = %r.did, motif = %motif, "ecoute_distante_session_refusee_par_le_site");
                    return Err(Rejet::nouveau(403, "refuse_par_le_site").avec_motif(motif));
                }
                self.appareils_connus.insert(r.did.clone(), maintenant);
                let s = self.creer(&lp, rep.session_id, rep.appareil, false)?;
                for id in &rep.couper {
                    self.couper_par_id_site(
                        id,
                        Rejet::coupee("ecoute_reprise_ailleurs", Some(s.designation())),
                    );
                }
                info!(did = %r.did, "ecoute_distante_session_ouverte");
                Ok(self.ouverte(&s))
            }
            Err(e) => {
                // Le laissez-passer est valide et vérifié hors ligne :
                // accepté, l'ouverture sera rejouée au retour du site.
                self.site_injoignable.store(true, Ordering::Relaxed);
                warn!(erreur = %e.0, "ecoute_distante_site_injoignable_a_l_ouverture");
                let s = self.creer(&lp, None, None, false)?;
                Ok(self.ouverte(&s))
            }
        }
    }

    fn ouverte(&self, s: &Session) -> Ouverte {
        Ouverte {
            jeton: s.jeton.clone(),
            expire: s.expire(),
            secours: s.secours.load(Ordering::Relaxed),
        }
    }

    fn creer(
        &self,
        lp: &LaissezPasser,
        session_site: Option<String>,
        nom_appareil: Option<String>,
        secours: bool,
    ) -> Result<Arc<Session>, Rejet> {
        let jeton = tune_laissez_passer::jeton_aleatoire()
            .ok_or_else(|| Rejet::nouveau(503, "aleatoire_indisponible"))?;
        let r = &lp.revendications;
        let a_rejouer = session_site.is_none();
        let s = Arc::new(Session {
            jeton: jeton.clone(),
            sid: r.sid.clone(),
            sub: r.sub.clone(),
            did: r.did.clone(),
            pk_appareil: r.cnf.pk.clone(),
            jti: std::sync::Mutex::new(r.jti.clone()),
            exp: AtomicI64::new(r.exp),
            nom_appareil: std::sync::Mutex::new(nom_appareil),
            session_site: std::sync::Mutex::new(session_site),
            a_rejouer: AtomicBool::new(a_rejouer),
            secours: AtomicBool::new(secours),
            octets: AtomicU64::new(0),
            dernier_audio: AtomicI64::new(i64::MIN),
            coupure: watch::channel(None).0,
        });
        self.par_jeton.insert(jeton, s.clone());
        Ok(s)
    }

    /// Présente un nouveau laissez-passer sur une session ouverte. Le jeton
    /// de session ne change pas.
    pub async fn renouveler(&self, jeton: &str, corps: &CorpsSession) -> Result<Ouverte, Rejet> {
        let s = match self.par_jeton.get(jeton) {
            Some(s) => s.clone(),
            None => {
                return Err(self
                    .coupees
                    .get(jeton)
                    .map(|c| c.0.clone())
                    .unwrap_or_else(Rejet::non_autorise));
            }
        };
        self.consommer_defi(&s.sid, &corps.defi)?;
        let lp = self.verifier(&corps.laissez_passer, &s.sid, 0).await?;
        lp.verifier_defi(&s.sid, &corps.defi, &corps.signature)
            .map_err(Rejet::du_refus)?;
        let r = &lp.revendications;
        // Même compte, même appareil, même clé : un renouvellement ne fait
        // pas changer une session de main.
        if r.sub != s.sub || r.did != s.did || r.cnf.pk != s.pk_appareil {
            return Err(Rejet::nouveau(401, "autre_appareil"));
        }
        if self.revoque(&lp) {
            self.couper(&s, Rejet::coupee("revoquee", None));
            return Err(Rejet::nouveau(403, "revoque"));
        }
        s.exp.fetch_max(r.exp, Ordering::Relaxed);
        *s.jti.lock().unwrap() = r.jti.clone();
        s.secours.store(false, Ordering::Relaxed);
        Ok(self.ouverte(&s))
    }

    /// La session est-elle encore valable à `maintenant` ? Au-delà de son
    /// `exp`, seulement en secours : site injoignable, moins de
    /// [`SECOURS_S`], appareil connu.
    fn encore_valable(&self, s: &Session, maintenant: i64) -> bool {
        let exp = s.expire() + tune_laissez_passer::TOLERANCE_HORLOGE_S;
        maintenant <= exp
            || (self.site_injoignable()
                && maintenant <= exp + SECOURS_S
                && self.appareil_connu(&s.did, maintenant))
    }

    /// La session d'un jeton, si c'en est un. `Ok(None)` : ce n'est pas un
    /// jeton de session (peut-être l'ancien jeton du pont).
    pub fn chercher(&self, jeton: &str, server_id: &str) -> Result<Option<Arc<Session>>, Rejet> {
        if let Some(s) = self.par_jeton.get(jeton).map(|s| s.clone()) {
            if s.sid != server_id {
                return Err(Rejet::non_autorise());
            }
            if !self.encore_valable(&s, self.maintenant()) {
                let rejet = Rejet::nouveau(401, "session_expiree");
                self.couper(&s, rejet.clone());
                return Err(rejet);
            }
            return Ok(Some(s));
        }
        if let Some(c) = self.coupees.get(jeton) {
            return Err(c.0.clone());
        }
        Ok(None)
    }

    /// De l'audio passe par cette session. Si elle ne jouait pas encore, les
    /// autres sessions du même compte qui jouaient sont coupées : la nouvelle
    /// écoute prend la main.
    pub fn audio(&self, s: &Arc<Session>) {
        let maintenant = self.maintenant();
        let jouait = s.audio_actif(maintenant);
        s.dernier_audio.store(maintenant, Ordering::Relaxed);
        if jouait {
            return;
        }
        let autres: Vec<Arc<Session>> = self
            .par_jeton
            .iter()
            .filter(|e| e.sub == s.sub && e.jeton != s.jeton && e.audio_actif(maintenant))
            .map(|e| e.clone())
            .collect();
        for a in autres {
            info!(did = %a.did, par = %s.did, "ecoute_distante_coupee_par_la_limite");
            self.couper(
                &a,
                Rejet::coupee("ecoute_reprise_ailleurs", Some(s.designation())),
            );
        }
    }

    /// Coupe une session : ses flux et son WebSocket se ferment, et son
    /// jeton répond désormais `401 session_coupee` avec le motif.
    pub fn couper(&self, s: &Session, rejet: Rejet) {
        s.coupure.send_replace(Some(rejet.clone()));
        if self.par_jeton.remove(&s.jeton).is_some() {
            info!(
                did = %s.did,
                motif = rejet.motif.as_deref().unwrap_or(&rejet.code),
                "ecoute_distante_session_coupee"
            );
        }
        self.coupees
            .insert(s.jeton.clone(), (rejet, self.maintenant()));
    }

    fn couper_par_id_site(&self, id: &str, rejet: Rejet) {
        let cibles: Vec<Arc<Session>> = self
            .par_jeton
            .iter()
            .filter(|e| e.session_site().as_deref() == Some(id))
            .map(|e| e.clone())
            .collect();
        for s in cibles {
            self.couper(&s, rejet.clone());
        }
    }

    /// Une requête servie par l'ancien jeton du pont.
    pub fn noter_herite(&self, server_id: &str) {
        self.herite
            .entry(server_id.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// La clé que le site connaît pour ce serveur. Site injoignable : la
    /// dernière qu'il a donnée.
    pub async fn cle_du_serveur(&self, server_id: &str) -> Option<String> {
        match self.site.cle_du_serveur(server_id).await {
            Ok(Some(cle)) => {
                self.cles_serveurs
                    .insert(server_id.to_string(), cle.clone());
                Some(cle)
            }
            Ok(None) => {
                self.cles_serveurs.remove(server_id);
                None
            }
            Err(e) => {
                warn!(erreur = %e.0, "ecoute_distante_cle_serveur_non_lue");
                self.cles_serveurs.get(server_id).map(|c| c.clone())
            }
        }
    }

    /// Un tour de battement : ménage, rejeu des ouvertures faites en panne,
    /// envoi de l'état des sessions, application de la réponse.
    pub async fn battre(&self) {
        let maintenant = self.maintenant();

        // 1. Ménage.
        let expirees: Vec<Arc<Session>> = self
            .par_jeton
            .iter()
            .filter(|e| !self.encore_valable(e, maintenant))
            .map(|e| e.clone())
            .collect();
        for s in expirees {
            self.couper(&s, Rejet::nouveau(401, "session_expiree"));
        }
        self.defis.retain(|_, (_, fin)| *fin >= maintenant);
        self.coupees
            .retain(|_, (_, t)| maintenant - *t <= COUPURE_GARDEE_S);
        self.revoques_jti
            .retain(|_, t| maintenant - *t <= REVOCATION_GARDEE_S);
        self.revoques_did
            .retain(|_, t| maintenant - *t <= REVOCATION_GARDEE_S);
        self.appareils_connus
            .retain(|_, t| maintenant - *t <= APPAREIL_CONNU_S);

        // 2. Rejeu des ouvertures acceptées pendant une panne.
        let a_rejouer: Vec<Arc<Session>> = self
            .par_jeton
            .iter()
            .filter(|e| e.a_rejouer.load(Ordering::Relaxed))
            .map(|e| e.clone())
            .collect();
        for s in a_rejouer {
            let demande = DemandeOuverture {
                jti: s.jti.lock().unwrap().clone(),
                sub: s.sub.clone(),
                did: s.did.clone(),
                sid: s.sid.clone(),
                chemin: "relais",
                secours: s.secours.load(Ordering::Relaxed),
            };
            match self.site.ouvrir_session(&demande).await {
                Ok(rep) if rep.ok => {
                    self.site_injoignable.store(false, Ordering::Relaxed);
                    *s.session_site.lock().unwrap() = rep.session_id;
                    if rep.appareil.is_some() {
                        *s.nom_appareil.lock().unwrap() = rep.appareil;
                    }
                    s.a_rejouer.store(false, Ordering::Relaxed);
                    self.appareils_connus.insert(s.did.clone(), maintenant);
                    for id in &rep.couper {
                        self.couper_par_id_site(
                            id,
                            Rejet::coupee("ecoute_reprise_ailleurs", Some(s.designation())),
                        );
                    }
                }
                Ok(rep) => {
                    self.site_injoignable.store(false, Ordering::Relaxed);
                    let motif = rep.motif.unwrap_or_else(|| "refuse".into());
                    self.couper(&s, Rejet::coupee(&motif, None));
                }
                Err(_) => {
                    self.site_injoignable.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }

        // 3. Battement.
        let mut releves = Vec::new();
        let mut corps = Battement::default();
        for e in self.par_jeton.iter() {
            if let Some(id) = e.session_site() {
                let octets = e.octets.swap(0, Ordering::Relaxed);
                releves.push((e.clone(), octets));
                corps.sessions.push(BattementSession {
                    session_id: id,
                    octets_depuis_le_dernier: octets,
                    audio_actif: e.audio_actif(maintenant),
                });
            }
        }
        for e in self.herite.iter() {
            let n = e.value().swap(0, Ordering::Relaxed);
            if n > 0 {
                corps.jeton_herite.insert(e.key().clone(), n);
            }
        }
        if corps.sessions.is_empty() && corps.jeton_herite.is_empty() {
            return;
        }
        match self.site.battement(&corps).await {
            Ok(rep) => {
                self.site_injoignable.store(false, Ordering::Relaxed);
                for c in rep.couper {
                    let motif = c.motif.unwrap_or_else(|| "coupee_par_le_site".into());
                    self.couper_par_id_site(&c.session_id, Rejet::coupee(&motif, None));
                }
                for jti in rep.revoques.jti {
                    self.revoques_jti.insert(jti, maintenant);
                }
                for did in rep.revoques.did {
                    self.revoques_did.insert(did, maintenant);
                }
                let revoquees: Vec<Arc<Session>> = self
                    .par_jeton
                    .iter()
                    .filter(|e| {
                        self.revoques_did.contains_key(&e.did)
                            || self.revoques_jti.contains_key(&*e.jti.lock().unwrap())
                    })
                    .map(|e| e.clone())
                    .collect();
                for s in revoquees {
                    self.couper(&s, Rejet::coupee("revoquee", None));
                }
                let tenue = self.jeu().map(|j| j.version());
                if let Some(v) = rep.jeu_de_cles
                    && tenue.is_none_or(|t| v > t)
                    && let Err(e) = self.rafraichir_jeu().await
                {
                    warn!(erreur = %e, "ecoute_distante_jeu_de_cles_non_relu");
                }
            }
            Err(e) => {
                // Rien n'est perdu : les compteurs reviennent pour le
                // battement suivant.
                self.site_injoignable.store(true, Ordering::Relaxed);
                warn!(erreur = %e.0, "ecoute_distante_battement_non_envoye");
                for (s, octets) in releves {
                    s.octets.fetch_add(octets, Ordering::Relaxed);
                }
                for (sid, n) in corps.jeton_herite {
                    self.herite
                        .entry(sid)
                        .or_insert_with(|| AtomicU64::new(0))
                        .fetch_add(n, Ordering::Relaxed);
                }
            }
        }
    }

    /// La boucle de fond : jeu de clés au démarrage et toutes les 6 h,
    /// battement toutes les minutes.
    pub async fn boucle(self: Arc<Self>) {
        match self.rafraichir_jeu().await {
            Ok(v) => info!(version = v, "ecoute_distante_jeu_de_cles_charge"),
            Err(e) => warn!(erreur = %e, "ecoute_distante_jeu_de_cles_absent"),
        }
        let mut battement = tokio::time::interval(PERIODE_BATTEMENT);
        let mut jeu = tokio::time::interval(PERIODE_JEU_DE_CLES);
        battement.tick().await;
        jeu.tick().await;
        loop {
            tokio::select! {
                _ = battement.tick() => self.battre().await,
                _ = jeu.tick() => {
                    if let Err(e) = self.rafraichir_jeu().await {
                        warn!(erreur = %e, "ecoute_distante_jeu_de_cles_non_relu");
                    }
                }
            }
        }
    }
}

/// `TUNE_BRIDGE_JETON_HERITE` : ouvert sauf `0`, `false`, `off`, `no`.
pub fn interrupteur_herite(valeur: Option<&str>) -> bool {
    !valeur.is_some_and(|v| {
        matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    })
}

/// Habille la réponse selon l'accès : avertissement pour le chemin hérité ;
/// pour une session, le corps compte ses octets et s'arrête net à la
/// coupure de la session.
pub fn habiller(reponse: Response, acces: &Acces) -> Response {
    match acces {
        Acces::Herite => {
            let mut reponse = reponse;
            reponse.headers_mut().insert(
                ENTETE_AVERTISSEMENT,
                HeaderValue::from_static("lien-obsolete"),
            );
            reponse
        }
        Acces::Session(s) => {
            let (parties, corps) = reponse.into_parts();
            let mut coupure = s.coupure();
            // La session est tenue par la fermeture : son émetteur de coupure
            // vit autant que le corps, et seule une vraie coupure l'arrête.
            let tenue = s.clone();
            let fin = async move {
                let _ = coupure.wait_for(|c| c.is_some()).await;
            };
            let flux = corps
                .into_data_stream()
                .inspect(move |morceau| {
                    if let Ok(octets) = morceau {
                        tenue.compter(octets.len());
                    }
                })
                .take_until(fin);
            Response::from_parts(parties, Body::from_stream(flux))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_chemin_herite_est_ouvert_par_defaut_et_se_ferme_explicitement() {
        assert!(interrupteur_herite(None));
        assert!(interrupteur_herite(Some("1")));
        assert!(interrupteur_herite(Some("")));
        for ferme in ["0", "false", "OFF", " no "] {
            assert!(!interrupteur_herite(Some(ferme)), "{ferme}");
        }
    }
}
