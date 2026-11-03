//! Écoute à distance en DIRECT, pair-à-pair (#6785, étape 11 du cahier du
//! contrôle central).
//!
//! Le chemin direct ne sert qu'à une chose : écouter à distance SA propre
//! bibliothèque, depuis une app reliée au même compte mozaiklabs. Jamais une
//! connexion entre utilisateurs, jamais le serveur d'un autre compte. Le
//! relais reste le chemin par défaut, et le secours.
//!
//! # Désactivé par défaut
//!
//! - à la compilation : fonction `ecoute-directe`, hors défaut (iroh n'entre
//!   dans l'arbre qu'avec elle) ;
//! - à l'exécution : rien n'écoute tant que `TUNE_ECOUTE_DIRECTE` ne vaut
//!   pas vrai, ou, sans cette variable, tant que le réglage
//!   [`REGLAGE_ACTIVE`] ne vaut pas `true`. La variable, quand elle est
//!   posée, l'emporte (`0` éteint même si le réglage allume) ;
//! - aucun relais iroh public tiers : relais auto-hébergé si
//!   [`REGLAGE_RELAIS_IROH`] / `TUNE_ECOUTE_DIRECTE_RELAIS` donne son
//!   adresse, sinon direct seulement. Ni pkarr, ni DNS d'n0 (preset
//!   `Minimal`).
//!
//! # Admission
//!
//! Sur le flux de contrôle ([`protocole`]), le serveur tend un défi, puis
//! n'ouvre une session que si :
//!
//! 1. le serveur a le droit Premium de l'écoute à distance (le même contrôle
//!    que pour lancer le client du relais) ;
//! 2. le laissez-passer est valide, vérifié HORS LIGNE par
//!    `tune-laissez-passer` avec un jeu de clés signé par la racine
//!    EMBARQUÉE ([`tune_laissez_passer::RACINE_EMBARQUEE`], aucune
//!    surcharge) : signature, `iss`, `aud`, `sid` = CE serveur, `nbf`, `exp`
//!    (sans grâce : pas d'expiré accepté en direct) ;
//! 3. il porte `p2p: true` ;
//! 4. `srv_pk`, s'il est présent, est la clé de CE serveur ;
//! 5. le défi est signé par la clé d'appareil du laissez-passer (`cnf`) ;
//! 6. ni le `jti` ni l'appareil ne sont révoqués (battement) ;
//! 7. le site accepte l'ouverture (`chemin: "direct"`). Site injoignable :
//!    session acceptée en secours, ouverture rejouée à son retour.
//!
//! # Une écoute à distance par compte
//!
//! Même règle que le relais (politique B) : quand une session se met à jouer,
//! les autres sessions du même compte qui jouaient sont coupées
//! (`ecoute_reprise_ailleurs`). Le site fait de même entre relais et direct,
//! par `couper` (ouverture et battement).

mod protocole;
mod site;
mod tunnel;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};
use tune_laissez_passer::{
    CleAppareil, CleServeur, Contexte, JeuDeCles, LaissezPasser, Publication, TOLERANCE_HORLOGE_S,
};

use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;

pub use protocole::{
    ALPN, CODE_COUPEE, CODE_FIN, CODE_REFUS, MessageApp, MessageServeur, TRAME_MAX, VERSION,
    ecrire_trame, lire_trame,
};
pub use site::{
    Battement, BattementSession, Coupure as CoupureDuSite, DemandeOuverture, EntetesSignes,
    ReponseBattement, ReponseOuverture, Revoques, SiteDirect, SiteHttp, SiteInjoignable,
    signer_requete,
};
pub use tunnel::{
    ENTETE_APPAREIL_ID, ENTETE_APPAREIL_NOM, ENTETES_TRANSMIS, est_audio, url_locale,
};

/// Réglage : `true` allume l'écoute directe (sans `TUNE_ECOUTE_DIRECTE`).
pub const REGLAGE_ACTIVE: &str = "ecoute_directe_active";
/// Réglage : adresse du relais iroh AUTO-HÉBERGÉ (`https://…`). Absent,
/// vide ou `aucun` : direct seulement.
pub const REGLAGE_RELAIS_IROH: &str = "ecoute_directe_relais_iroh";
/// Réglage : port UDP d'écoute (0 ou absent : port libre tiré au hasard).
pub const REGLAGE_PORT: &str = "ecoute_directe_port";
/// Variable d'environnement qui allume (`1`, `true`…) ou éteint (`0`,
/// `false`…) l'écoute directe ; posée, elle l'emporte sur le réglage.
pub const ENV_ACTIVE: &str = "TUNE_ECOUTE_DIRECTE";
/// Variable d'environnement : relais iroh auto-hébergé ; l'emporte sur le
/// réglage.
pub const ENV_RELAIS_IROH: &str = "TUNE_ECOUTE_DIRECTE_RELAIS";
/// Variable d'environnement : port UDP ; l'emporte sur le réglage.
pub const ENV_PORT: &str = "TUNE_ECOUTE_DIRECTE_PORT";
/// Variable d'environnement : `0` coupe la redirection de port automatique
/// (UPnP, NAT-PMP, PCP) de la box. Allumée par défaut.
pub const ENV_PORTMAPPER: &str = "TUNE_ECOUTE_DIRECTE_PORTMAPPER";

/// Connexions servies en même temps, au plus (admises ou en poignée de main).
const CONNEXIONS_MAX: usize = 16;
/// Délai de la poignée de main du flux de contrôle.
const DELAI_POIGNEE: Duration = Duration::from_secs(10);
/// Une session cesse de compter comme écoute après 2 min sans audio.
const ECOUTE_INACTIVE: Duration = Duration::from_secs(120);
/// Période du battement vers le site.
const PERIODE_BATTEMENT: Duration = Duration::from_secs(60);
/// Relecture ordinaire du jeu de clés.
const PERIODE_JEU_DE_CLES: Duration = Duration::from_secs(6 * 3600);
/// Relecture sur `kid` inconnu : une fois par minute au plus.
const RELECTURE_KID_MIN: Duration = Duration::from_secs(60);
/// Durée de mémoire d'une révocation reçue au battement.
const MEMOIRE_REVOCATION: Duration = Duration::from_secs(3600);

/// Secondes Unix.
pub fn maintenant() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn vrai(v: &str) -> Option<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "oui" | "on" => Some(true),
        "0" | "false" | "no" | "non" | "off" | "" => Some(false),
        _ => None,
    }
}

fn lire_reglage(reglages: &SettingsRepo, nom: &str) -> Option<String> {
    reglages
        .get(nom)
        .ok()
        .flatten()
        .map(|v| v.trim().to_string())
}

/// L'écoute directe est-elle allumée ? Faux par défaut. Une variable
/// illisible vaut « éteint ».
pub fn active(reglages: &SettingsRepo) -> bool {
    active_selon(std::env::var(ENV_ACTIVE).ok().as_deref(), reglages)
}

fn active_selon(env: Option<&str>, reglages: &SettingsRepo) -> bool {
    if let Some(v) = env {
        return vrai(v).unwrap_or(false);
    }
    lire_reglage(reglages, REGLAGE_ACTIVE).is_some_and(|v| v == "true")
}

/// Le relais iroh auto-hébergé, s'il est configuré. Jamais un relais public
/// par défaut : absent, c'est le direct seulement.
pub fn relais_iroh(reglages: &SettingsRepo) -> Option<String> {
    std::env::var(ENV_RELAIS_IROH)
        .ok()
        .map(|v| v.trim().to_string())
        .or_else(|| lire_reglage(reglages, REGLAGE_RELAIS_IROH))
        .filter(|v| !v.is_empty() && !v.eq_ignore_ascii_case("aucun"))
}

fn port_udp(reglages: &SettingsRepo) -> u16 {
    std::env::var(ENV_PORT)
        .ok()
        .or_else(|| lire_reglage(reglages, REGLAGE_PORT))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// Le droit Premium de l'écoute à distance. tune-server le branche sur la
/// licence (`Feature::CloudRelay`), le même contrôle que pour le relais.
#[async_trait]
pub trait DroitsDirect: Send + Sync {
    async fn ecoute_distante_permise(&self) -> bool;
}

/// Pourquoi une ouverture ou un renouvellement est refusé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refus {
    /// Code stable rendu à l'app : un code de `tune-laissez-passer`
    /// (`expire`, `serveur`, `defi`…) ou l'un de ceux du direct
    /// (`p2p_non_permis`, `revoque`, `premium_requis`, `jeu_de_cles_absent`,
    /// `refuse_par_le_site`, `autre_appareil`).
    pub code: String,
    pub motif: Option<String>,
}

impl Refus {
    fn nouveau(code: &str) -> Self {
        Self {
            code: code.to_string(),
            motif: None,
        }
    }
}

impl From<tune_laissez_passer::Refus> for Refus {
    fn from(r: tune_laissez_passer::Refus) -> Self {
        Self::nouveau(r.code())
    }
}

/// Pourquoi une session est coupée.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coupure {
    pub motif: String,
    pub appareil: Option<String>,
}

/// Une session directe ouverte.
pub struct Session {
    pub id: u64,
    pub sub: String,
    pub did: String,
    cle_appareil: CleAppareil,
    jti: Mutex<String>,
    exp: AtomicI64,
    /// `session_id` du site ; `None` tant que l'ouverture n'a pas été
    /// acceptée par lui (session de secours).
    site_id: Mutex<Option<String>>,
    secours: AtomicBool,
    pub appareil: Option<String>,
    derniere_audio: Mutex<Option<Instant>>,
    pub(crate) octets: AtomicU64,
    coupure: watch::Sender<Option<Coupure>>,
}

impl Session {
    pub fn expire(&self) -> i64 {
        self.exp.load(Ordering::Relaxed)
    }

    pub fn secours(&self) -> bool {
        self.secours.load(Ordering::Relaxed)
    }

    pub fn site_id(&self) -> Option<String> {
        self.site_id.lock().ok().and_then(|g| g.clone())
    }

    pub fn coupure(&self) -> Option<Coupure> {
        self.coupure.borrow().clone()
    }

    fn joue(&self, t: Instant) -> bool {
        self.derniere_audio
            .lock()
            .ok()
            .and_then(|g| *g)
            .is_some_and(|d| t.duration_since(d) < ECOUTE_INACTIVE)
    }

    fn designation(&self) -> Option<String> {
        self.appareil.clone()
    }

    /// Coupe la session : la première coupure gagne.
    fn couper(&self, c: Coupure) {
        self.coupure.send_if_modified(|actuelle| {
            if actuelle.is_none() {
                *actuelle = Some(c);
                true
            } else {
                false
            }
        });
    }
}

#[derive(Default)]
struct Revocations {
    jti: HashMap<String, Instant>,
    did: HashMap<String, Instant>,
}

impl Revocations {
    fn purger(&mut self, t: Instant) {
        self.jti
            .retain(|_, d| t.duration_since(*d) < MEMOIRE_REVOCATION);
        self.did
            .retain(|_, d| t.duration_since(*d) < MEMOIRE_REVOCATION);
    }
}

/// Le moteur de l'écoute directe : admission, sessions, battement.
pub struct EcouteDirecte {
    server_id: String,
    cle_publique: String,
    racine: String,
    jeu: Mutex<Option<JeuDeCles>>,
    derniere_relecture_kid: Mutex<Option<Instant>>,
    site: Arc<dyn SiteDirect>,
    droits: Arc<dyn DroitsDirect>,
    sessions: Mutex<HashMap<u64, Arc<Session>>>,
    prochain_id: AtomicU64,
    revocations: Mutex<Revocations>,
    local: reqwest::Client,
    local_port: u16,
}

/// Ce qu'il faut pour monter le moteur.
pub struct Parametres {
    pub server_id: String,
    /// Graine Ed25519 de la clé du serveur (coffre) : la même clé signe les
    /// requêtes au site et sert d'identité d'endpoint iroh.
    pub graine: [u8; 32],
    /// Racine minisign du jeu de clés. En production :
    /// [`tune_laissez_passer::RACINE_EMBARQUEE`], sans surcharge possible.
    pub racine: String,
    /// Jeu de clés de départ (la copie embarquée en production).
    pub jeu_initial: Option<Publication>,
    pub site: Arc<dyn SiteDirect>,
    pub droits: Arc<dyn DroitsDirect>,
    /// Port HTTP local du serveur.
    pub local_port: u16,
}

impl EcouteDirecte {
    pub fn nouveau(p: Parametres) -> Result<Arc<Self>, String> {
        let cle =
            CleServeur::depuis_graine(&p.graine).map_err(|r| format!("cle du serveur : {r}"))?;
        let cle_publique = cle.cle_publique();
        let local = crate::http::client::builder()
            // Pas de délai total : un flux audio dure le temps du morceau.
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| format!("client local : {e}"))?;
        let moteur = Arc::new(Self {
            server_id: p.server_id,
            cle_publique,
            racine: p.racine,
            jeu: Mutex::new(None),
            derniere_relecture_kid: Mutex::new(None),
            site: p.site,
            droits: p.droits,
            sessions: Mutex::new(HashMap::new()),
            prochain_id: AtomicU64::new(1),
            revocations: Mutex::new(Revocations::default()),
            local,
            local_port: p.local_port,
        });
        if let Some(publication) = p.jeu_initial
            && let Err(r) = moteur.accepter_jeu(&publication)
        {
            warn!(refus = %r, "ecoute_directe_jeu_initial_refuse");
        }
        Ok(moteur)
    }

    /// Clé publique du serveur (base64url) : `srv_pk` et identité iroh.
    pub fn cle_publique(&self) -> &str {
        &self.cle_publique
    }

    pub fn server_id(&self) -> &str {
        &self.server_id
    }

    /// Sessions ouvertes, en ce moment.
    pub fn sessions(&self) -> Vec<Arc<Session>> {
        self.sessions
            .lock()
            .map(|g| g.values().cloned().collect())
            .unwrap_or_default()
    }

    fn version_du_jeu(&self) -> Option<u64> {
        self.jeu
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(JeuDeCles::version))
    }

    /// Accepte un jeu de clés s'il est signé par la racine et pas plus
    /// ancien que celui qu'on tient.
    fn accepter_jeu(&self, publication: &Publication) -> Result<bool, tune_laissez_passer::Refus> {
        let jeu = JeuDeCles::depuis_publication(publication, &self.racine)?;
        let mut g = self
            .jeu
            .lock()
            .map_err(|_| tune_laissez_passer::Refus::JeuDeClesFormat)?;
        if g.as_ref()
            .is_some_and(|actuel| actuel.version() > jeu.version())
        {
            return Ok(false);
        }
        *g = Some(jeu);
        Ok(true)
    }

    /// Relit le jeu de clés au site.
    pub async fn relire_jeu(&self) {
        match self.site.jeu_de_cles().await {
            Ok(p) => match self.accepter_jeu(&p) {
                Ok(true) => {
                    info!(version = ?self.version_du_jeu(), "ecoute_directe_jeu_de_cles_relu")
                }
                Ok(false) => {}
                Err(r) => warn!(refus = %r, "ecoute_directe_jeu_de_cles_refuse"),
            },
            Err(e) => warn!(erreur = %e.0, "ecoute_directe_jeu_de_cles_injoignable"),
        }
    }

    fn verifier_laissez_passer(&self, jeton: &str) -> Result<LaissezPasser, Refus> {
        let g = self
            .jeu
            .lock()
            .map_err(|_| Refus::nouveau("jeu_de_cles_absent"))?;
        let Some(jeu) = g.as_ref() else {
            return Err(Refus::nouveau("jeu_de_cles_absent"));
        };
        let ctx = Contexte {
            server_id: &self.server_id,
            maintenant: maintenant(),
            // Jamais de grâce en direct : un laissez-passer expiré ne passe
            // pas, même site injoignable.
            grace_expiration: 0,
        };
        Ok(jeu.verifier(jeton, &ctx)?)
    }

    /// Tout ce qui se vérifie hors ligne, ouverture et renouvellement.
    async fn juger(
        &self,
        defi: &str,
        jeton: &str,
        signature: &str,
    ) -> Result<LaissezPasser, Refus> {
        if !self.droits.ecoute_distante_permise().await {
            return Err(Refus::nouveau("premium_requis"));
        }
        let lp = match self.verifier_laissez_passer(jeton) {
            Err(r) if r.code == "kid_inconnu" => {
                // Une fois par minute au plus.
                let relire = self
                    .derniere_relecture_kid
                    .lock()
                    .map(|mut g| {
                        let t = Instant::now();
                        let ok = g.is_none_or(|d| t.duration_since(d) >= RELECTURE_KID_MIN);
                        if ok {
                            *g = Some(t);
                        }
                        ok
                    })
                    .unwrap_or(false);
                if relire {
                    self.relire_jeu().await;
                }
                self.verifier_laissez_passer(jeton)?
            }
            autre => autre?,
        };
        let r = &lp.revendications;
        if !r.p2p {
            return Err(Refus::nouveau("p2p_non_permis"));
        }
        if r.srv_pk
            .as_deref()
            .is_some_and(|pk| pk != self.cle_publique)
        {
            return Err(Refus::nouveau("serveur"));
        }
        lp.verifier_defi_direct(&self.server_id, defi, signature)?;
        if let Ok(mut rev) = self.revocations.lock() {
            rev.purger(Instant::now());
            if rev.jti.contains_key(&r.jti) || rev.did.contains_key(&r.did) {
                return Err(Refus::nouveau("revoque"));
            }
        }
        Ok(lp)
    }

    /// Ouvre une session : vérifications hors ligne, puis le site.
    pub async fn admettre(
        &self,
        defi: &str,
        jeton: &str,
        signature: &str,
    ) -> Result<Arc<Session>, Refus> {
        let lp = self.juger(defi, jeton, signature).await?;
        let r = &lp.revendications;
        let demande = DemandeOuverture {
            jti: r.jti.clone(),
            sub: r.sub.clone(),
            did: r.did.clone(),
            sid: r.sid.clone(),
            chemin: "direct",
            secours: false,
        };
        let (site_id, appareil, couper, secours) = match self.site.ouvrir_session(&demande).await {
            Ok(rep) if rep.ok => (rep.session_id, rep.appareil, rep.couper, false),
            Ok(rep) => {
                let motif = rep.motif.unwrap_or_else(|| "refuse".into());
                info!(motif = %motif, "ecoute_directe_refusee_par_le_site");
                return Err(Refus {
                    code: "refuse_par_le_site".into(),
                    motif: Some(motif),
                });
            }
            Err(e) => {
                // Laissez-passer valide, vérifié hors ligne : accepté,
                // l'ouverture sera rejouée au retour du site.
                warn!(erreur = %e.0, "ecoute_directe_site_injoignable_session_de_secours");
                (None, None, Vec::new(), true)
            }
        };
        let session = Arc::new(Session {
            id: self.prochain_id.fetch_add(1, Ordering::Relaxed),
            sub: r.sub.clone(),
            did: r.did.clone(),
            cle_appareil: lp.cle_appareil.clone(),
            jti: Mutex::new(r.jti.clone()),
            exp: AtomicI64::new(r.exp),
            site_id: Mutex::new(site_id),
            secours: AtomicBool::new(secours),
            appareil,
            derniere_audio: Mutex::new(None),
            octets: AtomicU64::new(0),
            coupure: watch::Sender::new(None),
        });
        if let Ok(mut g) = self.sessions.lock() {
            g.insert(session.id, session.clone());
        }
        for id in &couper {
            self.couper_par_id_site(id, "ecoute_reprise_ailleurs", session.designation());
        }
        info!(
            session = session.id,
            secours, "ecoute_directe_session_ouverte"
        );
        Ok(session)
    }

    /// Renouvelle une session ouverte avec un nouveau laissez-passer : même
    /// compte, même appareil, même clé ; `p2p` toujours vrai.
    pub async fn renouveler(
        &self,
        session: &Session,
        defi: &str,
        jeton: &str,
        signature: &str,
    ) -> Result<i64, Refus> {
        let lp = self.juger(defi, jeton, signature).await?;
        let r = &lp.revendications;
        if r.sub != session.sub || r.did != session.did || lp.cle_appareil != session.cle_appareil {
            return Err(Refus::nouveau("autre_appareil"));
        }
        if let Ok(mut g) = session.jti.lock() {
            *g = r.jti.clone();
        }
        session.exp.fetch_max(r.exp, Ordering::Relaxed);
        Ok(session.expire())
    }

    fn retirer(&self, id: u64) {
        if let Ok(mut g) = self.sessions.lock() {
            g.remove(&id);
        }
    }

    /// Coupe une session et l'oublie.
    pub fn couper(&self, s: &Session, motif: &str, appareil: Option<String>) {
        info!(session = s.id, motif, "ecoute_directe_session_coupee");
        s.couper(Coupure {
            motif: motif.to_string(),
            appareil,
        });
        self.retirer(s.id);
    }

    fn couper_par_id_site(&self, id_site: &str, motif: &str, appareil: Option<String>) {
        for s in self.sessions() {
            if s.site_id().as_deref() == Some(id_site) {
                self.couper(&s, motif, appareil.clone());
            }
        }
    }

    /// Révoque des laissez-passer et des appareils : les sessions qui les
    /// portent sont coupées, et ils ne rouvrent rien pendant une heure.
    pub fn revoquer(&self, jti: &[String], did: &[String]) {
        let t = Instant::now();
        if let Ok(mut rev) = self.revocations.lock() {
            for j in jti {
                rev.jti.insert(j.clone(), t);
            }
            for d in did {
                rev.did.insert(d.clone(), t);
            }
        }
        for s in self.sessions() {
            let jti_actuel = s.jti.lock().map(|g| g.clone()).unwrap_or_default();
            if jti.contains(&jti_actuel) || did.contains(&s.did) {
                self.couper(&s, "revoquee", None);
            }
        }
    }

    /// Une requête audio de la session : la politique B.
    fn audio(&self, s: &Arc<Session>) {
        let t = Instant::now();
        let jouait = s.joue(t);
        if let Ok(mut g) = s.derniere_audio.lock() {
            *g = Some(t);
        }
        if jouait {
            return;
        }
        for autre in self.sessions() {
            if autre.id != s.id && autre.sub == s.sub && autre.joue(t) {
                self.couper(&autre, "ecoute_reprise_ailleurs", s.designation());
            }
        }
    }

    /// Un battement : rejoue les ouvertures de secours, puis rend compte au
    /// site et applique sa réponse. Rien à dire : rien n'est envoyé.
    pub async fn battre(&self) {
        // 1. Les sessions ouvertes pendant une panne du site.
        for s in self.sessions() {
            if s.site_id().is_some() {
                continue;
            }
            let demande = DemandeOuverture {
                jti: s.jti.lock().map(|g| g.clone()).unwrap_or_default(),
                sub: s.sub.clone(),
                did: s.did.clone(),
                sid: self.server_id.clone(),
                chemin: "direct",
                secours: true,
            };
            match self.site.ouvrir_session(&demande).await {
                Ok(rep) if rep.ok => {
                    if let Ok(mut g) = s.site_id.lock() {
                        *g = rep.session_id;
                    }
                    for id in &rep.couper {
                        self.couper_par_id_site(id, "ecoute_reprise_ailleurs", s.designation());
                    }
                }
                Ok(rep) => {
                    let motif = rep.motif.unwrap_or_else(|| "refuse_par_le_site".into());
                    self.couper(&s, &motif, None);
                }
                Err(_) => return,
            }
        }
        // 2. Le battement.
        let t = Instant::now();
        let sessions: Vec<(Arc<Session>, BattementSession)> = self
            .sessions()
            .into_iter()
            .filter_map(|s| {
                let session_id = s.site_id()?;
                let octets = s.octets.swap(0, Ordering::Relaxed);
                let audio_actif = s.joue(t);
                Some((
                    s,
                    BattementSession {
                        session_id,
                        octets_depuis_le_dernier: octets,
                        audio_actif,
                    },
                ))
            })
            .collect();
        if sessions.is_empty() {
            return;
        }
        let corps = Battement {
            sessions: sessions.iter().map(|(_, b)| b.clone()).collect(),
        };
        match self.site.battement(&corps).await {
            Ok(rep) => {
                for c in &rep.couper {
                    let motif = c
                        .motif
                        .clone()
                        .unwrap_or_else(|| "coupee_par_le_site".into());
                    self.couper_par_id_site(&c.session_id, &motif, None);
                }
                if !rep.revoques.jti.is_empty() || !rep.revoques.did.is_empty() {
                    self.revoquer(&rep.revoques.jti, &rep.revoques.did);
                }
                if rep
                    .jeu_de_cles
                    .is_some_and(|v| self.version_du_jeu().is_none_or(|n| v > n))
                {
                    self.relire_jeu().await;
                }
            }
            Err(e) => {
                // Un battement perdu ne perd pas les compteurs.
                for (s, b) in &sessions {
                    s.octets
                        .fetch_add(b.octets_depuis_le_dernier, Ordering::Relaxed);
                }
                warn!(erreur = %e.0, "ecoute_directe_battement_perdu");
            }
        }
    }

    /// Les tâches de fond : battement chaque minute, jeu de clés toutes les
    /// 6 h.
    pub fn lancer_taches_de_fond(self: &Arc<Self>) {
        let moteur = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut battement = tokio::time::interval(PERIODE_BATTEMENT);
            let mut jeu = tokio::time::interval(PERIODE_JEU_DE_CLES);
            loop {
                tokio::select! {
                    _ = battement.tick() => {
                        let Some(m) = moteur.upgrade() else { break };
                        m.battre().await;
                    }
                    _ = jeu.tick() => {
                        let Some(m) = moteur.upgrade() else { break };
                        m.relire_jeu().await;
                    }
                }
            }
        });
    }

    /// Boucle d'accueil des connexions, jusqu'à la fermeture de l'endpoint.
    pub async fn servir(self: Arc<Self>, endpoint: iroh::Endpoint) {
        let en_cours = Arc::new(tokio::sync::Semaphore::new(CONNEXIONS_MAX));
        while let Some(entrant) = endpoint.accept().await {
            let Ok(place) = en_cours.clone().try_acquire_owned() else {
                entrant.refuse();
                continue;
            };
            let moteur = self.clone();
            tokio::spawn(async move {
                let _place = place;
                let conn = match entrant.await {
                    Ok(c) => c,
                    Err(_) => return,
                };
                if conn.alpn() != ALPN {
                    conn.close(iroh::endpoint::VarInt::from_u32(CODE_REFUS), b"alpn");
                    return;
                }
                moteur.servir_connexion(conn).await;
            });
        }
    }

    async fn servir_connexion(self: Arc<Self>, conn: iroh::endpoint::Connection) {
        let code = iroh::endpoint::VarInt::from_u32;
        let Ok(Ok((mut send, mut recv))) =
            tokio::time::timeout(DELAI_POIGNEE, conn.accept_bi()).await
        else {
            conn.close(code(CODE_REFUS), b"poignee");
            return;
        };
        match tokio::time::timeout(DELAI_POIGNEE, lire_trame::<_, MessageApp>(&mut recv)).await {
            Ok(Ok(MessageApp::Bonjour { v })) if v == VERSION => {}
            _ => {
                let c = refuser(&mut send, Refus::nouveau("version")).await;
                fermer_apres_refus(&conn, &c).await;
                return;
            }
        }
        let Some(defi) = tune_laissez_passer::jeton_aleatoire() else {
            conn.close(code(CODE_REFUS), b"aleatoire");
            return;
        };
        let annonce = MessageServeur::Defi {
            v: VERSION,
            server_id: self.server_id.clone(),
            defi: defi.clone(),
        };
        if ecrire_trame(&mut send, &annonce).await.is_err() {
            return;
        }
        let (jeton, signature) =
            match tokio::time::timeout(DELAI_POIGNEE, lire_trame::<_, MessageApp>(&mut recv)).await
            {
                Ok(Ok(MessageApp::Ouvrir {
                    laissez_passer,
                    signature,
                })) => (laissez_passer, signature),
                _ => {
                    let c = refuser(&mut send, Refus::nouveau("format")).await;
                    fermer_apres_refus(&conn, &c).await;
                    return;
                }
            };
        let session = match self.admettre(&defi, &jeton, &signature).await {
            Ok(s) => s,
            Err(r) => {
                info!(code = %r.code, "ecoute_directe_ouverture_refusee");
                let c = refuser(&mut send, r).await;
                fermer_apres_refus(&conn, &c).await;
                return;
            }
        };
        let Some(mut defi_suivant) = tune_laissez_passer::jeton_aleatoire() else {
            self.couper(&session, "aleatoire", None);
            conn.close(code(CODE_REFUS), b"aleatoire");
            return;
        };
        let ouvert = MessageServeur::Ouvert {
            expire: session.expire(),
            secours: session.secours(),
            defi_suivant: defi_suivant.clone(),
        };
        if ecrire_trame(&mut send, &ouvert).await.is_err() {
            self.retirer(session.id);
            return;
        }

        // Les flux HTTP de la session.
        let flux = {
            let conn = conn.clone();
            let moteur = self.clone();
            let session = session.clone();
            tokio::spawn(async move {
                while let Ok((s, r)) = conn.accept_bi().await {
                    tokio::spawn(tunnel::servir_flux(moteur.clone(), session.clone(), s, r));
                }
            })
        };
        // Les trames de contrôle, lues à part : une lecture interrompue au
        // milieu d'une trame perdrait la suite.
        let (tx, mut rx) = mpsc::channel::<Result<MessageApp, String>>(4);
        let lecteur = tokio::spawn(async move {
            loop {
                let m = lire_trame::<_, MessageApp>(&mut recv).await;
                let fin = m.is_err();
                if tx.send(m).await.is_err() || fin {
                    break;
                }
            }
        });

        let mut coupure = session.coupure.subscribe();
        loop {
            let reste = (session.expire() + TOLERANCE_HORLOGE_S - maintenant()).max(0) as u64;
            tokio::select! {
                _ = attendre_coupure(&mut coupure) => {
                    let c = session.coupure().unwrap_or(Coupure { motif: "coupee".into(), appareil: None });
                    let _ = ecrire_trame(&mut send, &MessageServeur::Coupee {
                        motif: c.motif.clone(),
                        appareil: c.appareil.clone(),
                    }).await;
                    let _ = send.finish();
                    let raison = serde_json::json!({"code": "session_coupee", "motif": c.motif}).to_string();
                    // Laisse à l'app le temps de lire la trame.
                    let _ = tokio::time::timeout(Duration::from_millis(500), conn.closed()).await;
                    conn.close(code(CODE_COUPEE), raison.as_bytes());
                    break;
                }
                _ = tokio::time::sleep(Duration::from_secs(reste)) => {
                    if session.expire() + TOLERANCE_HORLOGE_S <= maintenant() {
                        self.couper(&session, "session_expiree", None);
                    }
                }
                m = rx.recv() => match m {
                    Some(Ok(MessageApp::Renouveler { laissez_passer, signature })) => {
                        match self.renouveler(&session, &defi_suivant, &laissez_passer, &signature).await {
                            Ok(expire) => {
                                let Some(d) = tune_laissez_passer::jeton_aleatoire() else {
                                    self.couper(&session, "aleatoire", None);
                                    continue;
                                };
                                defi_suivant = d;
                                let _ = ecrire_trame(&mut send, &MessageServeur::Renouvele {
                                    expire,
                                    defi_suivant: defi_suivant.clone(),
                                }).await;
                            }
                            Err(r) => self.couper(&session, &r.code, None),
                        }
                    }
                    Some(Ok(_)) => self.couper(&session, "format", None),
                    // Flux de contrôle fermé par l'app : fin de la session.
                    Some(Err(_)) | None => {
                        self.retirer(session.id);
                        conn.close(code(CODE_FIN), b"fin");
                        break;
                    }
                },
                _ = conn.closed() => {
                    self.retirer(session.id);
                    break;
                }
            }
        }
        flux.abort();
        lecteur.abort();
    }
}

/// Attend que la session soit coupée. Sans `watch::Ref` dans le futur, qui
/// n'est pas `Send`.
async fn attendre_coupure(rx: &mut watch::Receiver<Option<Coupure>>) {
    loop {
        if rx.borrow_and_update().is_some() {
            return;
        }
        if rx.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Rend le refus à l'app sur le flux de contrôle ; rend son code.
async fn refuser(send: &mut iroh::endpoint::SendStream, r: Refus) -> String {
    let message = MessageServeur::Refus {
        code: r.code.clone(),
        motif: r.motif.clone(),
    };
    let _ = ecrire_trame(send, &message).await;
    let _ = send.finish();
    r.code
}

async fn fermer_apres_refus(conn: &iroh::endpoint::Connection, code_refus: &str) {
    // Laisse à l'app le temps de lire la raison avant la fermeture.
    let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
    conn.close(
        iroh::endpoint::VarInt::from_u32(CODE_REFUS),
        code_refus.as_bytes(),
    );
}

/// Réseau de l'endpoint.
#[derive(Debug, Clone, Default)]
pub struct ConfigReseau {
    /// Relais iroh auto-hébergé ; `None` : direct seulement.
    pub relais_iroh: Option<String>,
    /// Port UDP ; 0 : port libre.
    pub port: u16,
    /// Redirection de port automatique par la box.
    pub portmapper: bool,
    /// Tests : 127.0.0.1 seulement, sans portmapper.
    pub boucle_locale: bool,
}

/// Ouvre l'endpoint iroh du serveur : clé = la clé du serveur, ALPN =
/// [`ALPN`] seule, preset `Minimal` (ni pkarr, ni DNS d'n0), relais iroh
/// AUTO-HÉBERGÉ ou aucun.
pub async fn ouvrir_endpoint(
    graine: &[u8; 32],
    cfg: &ConfigReseau,
) -> Result<iroh::Endpoint, String> {
    use iroh::endpoint::{PortmapperConfig, presets};
    let relais = match cfg.relais_iroh.as_deref() {
        None => iroh::RelayMode::Disabled,
        Some(url) => iroh::RelayMode::custom([url
            .parse::<iroh::RelayUrl>()
            .map_err(|_| "adresse du relais iroh illisible".to_string())?]),
    };
    let mut b = iroh::Endpoint::builder(presets::Minimal)
        .secret_key(iroh::SecretKey::from_bytes(graine))
        .alpns(vec![ALPN.to_vec()])
        .relay_mode(relais);
    if cfg.boucle_locale {
        b = b
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .map_err(|e| e.to_string())?;
    } else if cfg.port != 0 {
        b = b
            .clear_ip_transports()
            .bind_addr(format!("0.0.0.0:{}", cfg.port).as_str())
            .map_err(|e| e.to_string())?
            .bind_addr(format!("[::]:{}", cfg.port).as_str())
            .map_err(|e| e.to_string())?;
    }
    if cfg.boucle_locale || !cfg.portmapper {
        b = b.portmapper_config(PortmapperConfig::Disabled);
    }
    b.bind()
        .await
        .map_err(|e| format!("ouverture de l'endpoint : {e}"))
}

/// Ce qu'une app doit savoir pour composer le serveur en direct.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Annonce {
    /// Identité d'endpoint iroh = clé publique du serveur = `srv_pk`.
    pub endpoint_id: String,
    pub cle_publique: String,
    pub alpn: &'static str,
    /// Adresses directes connues de l'endpoint (IP:port).
    pub adresses: Vec<String>,
    /// Relais iroh auto-hébergé, s'il y en a un.
    pub relais_iroh: Option<String>,
}

/// L'écoute directe en service : le moteur et son endpoint.
pub struct EnService {
    pub moteur: Arc<EcouteDirecte>,
    pub endpoint: iroh::Endpoint,
    pub relais_iroh: Option<String>,
}

impl EnService {
    pub fn annonce(&self) -> Annonce {
        let addr = self.endpoint.addr();
        Annonce {
            endpoint_id: self.endpoint.id().to_string(),
            cle_publique: self.moteur.cle_publique().to_string(),
            alpn: "tune-ecoute/1",
            adresses: addr.ip_addrs().map(|a| a.to_string()).collect(),
            relais_iroh: self.relais_iroh.clone(),
        }
    }
}

static EN_SERVICE: OnceLock<EnService> = OnceLock::new();

/// L'écoute directe en service, si elle a été lancée.
pub fn en_service() -> Option<&'static EnService> {
    EN_SERVICE.get()
}

/// Lance l'écoute directe si elle est allumée : clé du serveur (coffre),
/// `server_id`, jeu de clés embarqué, endpoint, accueil et tâches de fond.
///
/// `Ok(false)` : éteinte, rien n'écoute.
pub async fn lancer(
    backend: Arc<dyn DbBackend>,
    local_port: u16,
    droits: Arc<dyn DroitsDirect>,
) -> Result<bool, String> {
    let reglages = SettingsRepo::with_backend(backend.clone());
    if !active(&reglages) {
        info!("ecoute_directe_desactivee");
        return Ok(false);
    }
    if EN_SERVICE.get().is_some() {
        return Ok(true);
    }
    let server_id = crate::cloud::telemetry::TelemetryReporter::get_or_create_server_id(&reglages);
    let graine = crate::cloud::cle_serveur::graine_du_serveur(&backend)?;
    let cle = Arc::new(CleServeur::depuis_graine(&graine).map_err(|r| r.to_string())?);
    let site = Arc::new(SiteHttp::depuis_environnement(server_id.clone(), cle));
    let moteur = EcouteDirecte::nouveau(Parametres {
        server_id,
        graine,
        racine: tune_laissez_passer::RACINE_EMBARQUEE.to_string(),
        jeu_initial: Some(tune_laissez_passer::publication_embarquee()),
        site,
        droits,
        local_port,
    })?;
    let relais = relais_iroh(&reglages);
    let cfg = ConfigReseau {
        relais_iroh: relais.clone(),
        port: port_udp(&reglages),
        portmapper: std::env::var(ENV_PORTMAPPER)
            .ok()
            .and_then(|v| vrai(&v))
            .unwrap_or(true),
        boucle_locale: false,
    };
    let endpoint = ouvrir_endpoint(&graine, &cfg).await?;
    moteur.lancer_taches_de_fond();
    tokio::spawn(moteur.clone().servir(endpoint.clone()));
    info!(
        endpoint = %endpoint.id().fmt_short(),
        relais_iroh = relais.is_some(),
        "ecoute_directe_en_service"
    );
    let _ = EN_SERVICE.set(EnService {
        moteur,
        endpoint,
        relais_iroh: relais,
    });
    Ok(true)
}
