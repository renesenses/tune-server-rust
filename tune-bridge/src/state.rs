// Bridge server state; some fields are populated by the not-yet-wired bridge
// phases (1-3) and read later — annotate rather than drop.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

use dashmap::DashMap;
use tokio::sync::{Mutex, mpsc, oneshot};

/// Numéro de la prochaine connexion inscrite (#7833). Unique pour tout le
/// processus : deux inscriptions du même `server_id` n'ont jamais le même.
static PROCHAINE_GENERATION: AtomicU64 = AtomicU64::new(1);

pub struct RelayState {
    pub servers: DashMap<String, ServerConnection>,
    pub tokens: DashMap<String, String>,
    /// Verificateur d'eligibilite premium, interroge AVANT tout
    /// enregistrement. Voir `crate::licence`.
    pub licences: std::sync::Arc<crate::licence::Licences>,
    /// Vérificateur des billets d'écoute de Tune Circle (#5327), interrogé à
    /// CHAQUE requête de `/stream/circle/{ticket}`. Voir `crate::circle`.
    pub billets: std::sync::Arc<crate::circle::Billets>,
    /// Octets, sessions et erreurs du pont (#6512). Voir `crate::trafic`.
    pub trafic: Arc<crate::trafic::Trafic>,
    /// Sessions d'ecoute a distance ouvertes par laissez-passer (#6603).
    /// Voir `crate::sessions`.
    pub sessions: Arc<crate::sessions::Sessions>,
    pub max_servers: usize,
    pub max_clients_per_server: usize,
    pub max_streams_per_server: usize,
    /// Octets qu'un flux peut garder en attente sur le pont avant d'etre
    /// coupe (diagnostic CarPlay 5G du 24/11). Voir [`file_de_flux`].
    pub plafond_par_flux: usize,
    /// Memoire commune a tous les flux en attente. Voir [`BudgetDesFlux`].
    pub budget_flux: Arc<BudgetDesFlux>,
}

/// Plafond par defaut d'un flux en attente : 32 Mio, l'essentiel d'une piste
/// FLAC 16/44,1 quand le lecteur cesse de lire (AVPlayer remplit son tampon
/// puis se tait), alors que le serveur pousse a son debit montant.
pub const PLAFOND_PAR_FLUX: usize = 32 * 1024 * 1024;

/// Budget commun par defaut de tous les flux en attente : 512 Mio
/// (`TUNE_BRIDGE_BUDGET_FLUX_MIO`).
pub const BUDGET_DES_FLUX: usize = 512 * 1024 * 1024;

impl RelayState {
    pub fn new() -> Self {
        Self {
            servers: DashMap::new(),
            tokens: DashMap::new(),
            licences: crate::licence::Licences::depuis_environnement(),
            billets: crate::circle::Billets::depuis_environnement(),
            trafic: crate::trafic::Trafic::nouveau(),
            sessions: crate::sessions::Sessions::depuis_environnement(),
            max_servers: 100,
            max_clients_per_server: 10,
            max_streams_per_server: 5,
            plafond_par_flux: PLAFOND_PAR_FLUX,
            budget_flux: BudgetDesFlux::depuis_environnement(),
        }
    }

    /// Register (or re-register) a server connection.
    ///
    /// Security: `server_id` and `bridge_token` are both chosen by the client,
    /// so registration must not let a stranger overwrite an existing server's
    /// connection — otherwise an attacker who guesses a live `server_id` could
    /// take over its socket and receive the client traffic proxied to it. The
    /// `bridge_token` (already the client-facing bearer secret used by
    /// `api_proxy`) doubles as the re-registration authenticator:
    ///
    /// - Replacing a *live* `server_id` is allowed only when the presented
    ///   token is the one already bound to it (a legit server reconnecting with
    ///   its persisted token). A mismatching token is rejected.
    /// - A *new* `server_id` may not claim a token already bound to a different
    ///   server, and is subject to the `max_servers` cap.
    ///
    /// A server that registers with its own key (#6603) proves it by signing
    /// the relay's challenge (`ws_server`); `cle_serveur` is that proven key.
    /// A live `server_id` registered WITH a key is replaced only by a
    /// registrant proving the same key — the bridge token alone no longer
    /// suffices. When the site knows a key for a `server_id`, `ws_server`
    /// refuses any registrant that does not prove it.
    ///
    /// Rend la génération de la connexion inscrite : c'est elle, et non le
    /// seul `server_id`, que la connexion présente pour se désinscrire
    /// (`unregister_server_si`).
    pub fn register_server(
        &self,
        server_id: String,
        server_name: String,
        bridge_token: String,
        cle_serveur: Option<String>,
        ws_tx: mpsc::Sender<String>,
    ) -> Result<u64, &'static str> {
        // Reads clone out of the map (no guard held across the later inserts,
        // which on the same DashMap would deadlock).
        let cle_en_place = self.servers.get(&server_id).map(|c| c.cle_serveur.clone());
        let token_owner = self.server_for_token(&bridge_token);

        if let Some(cle_en_place) = cle_en_place {
            match cle_en_place {
                Some(k) => {
                    if cle_serveur.as_deref() != Some(k.as_str()) {
                        return Err("server_id already registered");
                    }
                }
                None => {
                    if token_owner.as_deref() != Some(server_id.as_str()) {
                        return Err("server_id already registered");
                    }
                }
            }
        } else {
            if let Some(owner) = &token_owner
                && owner != &server_id
            {
                return Err("bridge_token already bound to another server");
            }
            if self.servers.len() >= self.max_servers {
                return Err("max servers reached");
            }
        }

        self.tokens.insert(bridge_token, server_id.clone());
        let trafic = self.trafic.serveur(&server_id);
        let generation = PROCHAINE_GENERATION.fetch_add(1, Ordering::Relaxed);
        self.servers.insert(
            server_id.clone(),
            ServerConnection {
                generation,
                trafic,
                cle_serveur,
                server_id,
                server_name,
                ws_tx,
                pending: Arc::new(Mutex::new(HashMap::new())),
                flux: Arc::new(Mutex::new(HashMap::new())),
                active_streams: AtomicU32::new(0),
                active_clients: AtomicU32::new(0),
                connected_at: Instant::now(),
                last_heartbeat: Instant::now(),
            },
        );
        Ok(generation)
    }

    pub fn unregister_server(&self, server_id: &str) {
        if let Some((_, conn)) = self.servers.remove(server_id) {
            self.tokens.retain(|_, v| v != &conn.server_id);
        }
    }

    /// Désinscrit `server_id` seulement si l'inscription en place est encore
    /// celle de cette `generation` (#7833).
    ///
    /// Un serveur qui se reconnecte avant que le relais ait vu tomber son
    /// ancienne connexion la remplace. La fin de l'ancienne connexion ne doit
    /// alors pas effacer la nouvelle : le serveur resterait relié, se croirait
    /// « Connecté », et l'app recevrait `serveur_absent`.
    pub fn unregister_server_si(&self, server_id: &str, generation: u64) {
        if let Some((_, conn)) = self
            .servers
            .remove_if(server_id, |_, c| c.generation == generation)
        {
            self.tokens.retain(|_, v| v != &conn.server_id);
        }
    }

    /// L'inscription en place pour `server_id` est-elle celle de cette
    /// `generation` ?
    pub fn est_inscrit(&self, server_id: &str, generation: u64) -> bool {
        self.servers
            .get(server_id)
            .is_some_and(|c| c.generation == generation)
    }

    pub fn server_for_token(&self, token: &str) -> Option<String> {
        self.tokens.get(token).map(|v| v.clone())
    }

    /// Qui presente ce jeton pour ce serveur ?
    ///
    /// Un jeton de session d'abord (#6603) ; a defaut, l'ancien jeton du
    /// pont, tant que le chemin herite est ouvert. Tout le reste est refuse,
    /// avec un code que l'app sait lire.
    pub fn autoriser(
        &self,
        jeton: Option<&str>,
        server_id: &str,
    ) -> Result<crate::sessions::Acces, crate::sessions::Rejet> {
        let Some(jeton) = jeton.filter(|j| !j.is_empty()) else {
            return Err(crate::sessions::Rejet::non_autorise());
        };
        if let Some(s) = self.sessions.chercher(jeton, server_id)? {
            return Ok(crate::sessions::Acces::Session(s));
        }
        if self.server_for_token(jeton).as_deref() == Some(server_id) {
            if !self.sessions.jeton_herite_ouvert() {
                return Err(crate::sessions::Rejet::nouveau(401, "lien_obsolete"));
            }
            self.sessions.noter_herite(server_id);
            return Ok(crate::sessions::Acces::Herite);
        }
        Err(crate::sessions::Rejet::non_autorise())
    }
}

pub struct ServerConnection {
    /// Numéro de cette connexion (#7833), rendu par `register_server`.
    pub generation: u64,
    pub server_id: String,
    pub server_name: String,
    pub ws_tx: mpsc::Sender<String>,
    /// Cle publique Ed25519 que le serveur a prouvee en s'enregistrant
    /// (#6603), s'il en a une.
    pub cle_serveur: Option<String>,
    /// Les compteurs de trafic de ce serveur (#6512), pris une fois ici pour
    /// que le chemin chaud n'ait pas a les chercher.
    pub trafic: Arc<crate::trafic::CompteursServeur>,
    pub pending: Arc<Mutex<HashMap<String, Attente>>>,
    /// Puits des morceaux audio en vol, par identifiant de requete.
    ///
    /// Une reponse d'API se resout d'un coup : le `oneshot` de `pending`
    /// suffit. Un flux audio, non — il arrive en morceaux, longtemps apres
    /// l'en-tete. Le `oneshot` livre le debut de la reponse, et ce puits
    /// recoit la suite jusqu'a `relay.stream_end`.
    ///
    /// La carte vit dans la connexion : un serveur qui se deconnecte emporte
    /// ses puits, donc ferme les corps HTTP restes ouverts au lieu de les
    /// laisser pendre.
    pub flux: Arc<Mutex<HashMap<String, Puits>>>,
    pub active_streams: AtomicU32,
    pub active_clients: AtomicU32,
    pub connected_at: Instant,
    pub last_heartbeat: Instant,
}

/// Une requete relayee qui attend la reponse du serveur.
pub struct Attente {
    pub tx: oneshot::Sender<PendingResponse>,
    /// Ce que la reponse est, pour compter ses octets a l'arrivee (#6512).
    pub categorie: crate::trafic::Categorie,
}

impl Attente {
    pub fn nouvelle(
        tx: oneshot::Sender<PendingResponse>,
        categorie: crate::trafic::Categorie,
    ) -> Self {
        Self { tx, categorie }
    }
}

/// Le puits d'un flux audio en vol.
pub struct Puits {
    pub envoi: EnvoiDeFlux,
    pub categorie: crate::trafic::Categorie,
    /// Pose par `relay.stream_end` : la session s'est finie proprement.
    pub fin_propre: Arc<std::sync::atomic::AtomicBool>,
}

/// Corps d'une reponse relayee.
///
/// Le relais transporte deux choses de nature differente. Une reponse d'API
/// tient en memoire et arrive d'un bloc. Un flux audio ne tient pas en
/// memoire — un FLAC fait des dizaines de megaoctets — et arrive au fil de
/// l'eau. Les distinguer ici evite d'avoir a se demander, plus loin, si un
/// `Option<String>` vide veut dire « corps vide » ou « corps a venir ».
pub enum CorpsRelaye {
    /// Corps complet, deja recu — en octets : une pochette n'est pas du texte.
    Entier(Option<Vec<u8>>),
    /// Morceaux a venir, jusqu'a la fermeture du canal. `fin_propre` passe a
    /// vrai quand le serveur a annonce la fin (`relay.stream_end`).
    Morceaux(ReceptionDeFlux, Arc<std::sync::atomic::AtomicBool>),
}

/// La file d'un flux audio, entre la boucle de lecture de la WebSocket du
/// serveur et le corps HTTP du lecteur.
///
/// Diagnostic CarPlay en 5G (24/11) : cette file etait un canal borne a 64
/// morceaux, et la boucle de lecture ATTENDAIT d'y avoir de la place. Quand
/// AVPlayer cessait de lire, toute la connexion du serveur s'arretait : l'API,
/// les autres flux, et `relay.pong` — au bout de 90 s le pont coupait la
/// session, les corps se fermaient avant leur `Content-Length`, et iOS
/// affichait une « erreur TLS ».
///
/// Desormais le depot ne s'attend jamais. Il est compte en octets, contre le
/// plafond du flux ET contre le budget commun a tous les flux du pont
/// ([`BudgetDesFlux`]). Un depot qui depasserait l'un ou l'autre coupe CE flux
/// ([`RefusDeFlux::Plein`]) : sa file est videe sur-le-champ, ses octets
/// rendus au budget, et son corps se termine par une ERREUR — jamais par une
/// fin propre qu'un lecteur prendrait pour la fin de la piste. Le protocole
/// n'a pas (encore) de pause par flux a demander au serveur, qui n'est pas
/// prevenu de la coupure.
pub fn file_de_flux(plafond: usize, budget: &Arc<BudgetDesFlux>) -> (EnvoiDeFlux, ReceptionDeFlux) {
    let partage = Arc::new(FileDeFlux {
        etat: std::sync::Mutex::new(EtatDeFile::default()),
        reveil: tokio::sync::Notify::new(),
        budget: budget.clone(),
        plafond,
    });
    (
        EnvoiDeFlux {
            partage: partage.clone(),
        },
        ReceptionDeFlux { partage },
    )
}

/// Le budget memoire commun a tous les flux en attente sur le pont.
pub struct BudgetDesFlux {
    limite: usize,
    pris: std::sync::atomic::AtomicUsize,
}

impl BudgetDesFlux {
    pub fn nouveau(limite: usize) -> Arc<Self> {
        Arc::new(Self {
            limite,
            pris: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// `TUNE_BRIDGE_BUDGET_FLUX_MIO` (Mio), [`BUDGET_DES_FLUX`] sinon.
    pub fn depuis_environnement() -> Arc<Self> {
        let limite = std::env::var("TUNE_BRIDGE_BUDGET_FLUX_MIO")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|&m| m > 0)
            .map_or(BUDGET_DES_FLUX, |m| m * 1024 * 1024);
        Self::nouveau(limite)
    }

    /// Octets pris par les flux en attente.
    pub fn pris(&self) -> usize {
        self.pris.load(Ordering::Acquire)
    }

    fn prendre(&self, n: usize) -> bool {
        let mut pris = self.pris.load(Ordering::Acquire);
        loop {
            let Some(total) = pris.checked_add(n).filter(|&t| t <= self.limite) else {
                return false;
            };
            match self
                .pris
                .compare_exchange_weak(pris, total, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return true,
                Err(actuel) => pris = actuel,
            }
        }
    }

    fn rendre(&self, n: usize) {
        self.pris.fetch_sub(n, Ordering::AcqRel);
    }
}

struct FileDeFlux {
    etat: std::sync::Mutex<EtatDeFile>,
    /// Un seul lecteur attend : `notify_one` garde le reveil s'il arrive
    /// avant l'attente.
    reveil: tokio::sync::Notify,
    budget: Arc<BudgetDesFlux>,
    plafond: usize,
}

#[derive(Default)]
struct EtatDeFile {
    morceaux: std::collections::VecDeque<Vec<u8>>,
    /// Octets de `morceaux`, pris sur le budget.
    octets: usize,
    /// Coupe pour depassement : le corps se termine par une erreur.
    coupe: bool,
    /// Le pont a lache le flux (fin ou retrait du puits).
    envoi_ferme: bool,
    /// Le corps HTTP est tombe (lecteur parti).
    reception_fermee: bool,
}

impl FileDeFlux {
    fn etat(&self) -> std::sync::MutexGuard<'_, EtatDeFile> {
        self.etat.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Vide la file et rend ses octets au budget.
    fn vider(&self, etat: &mut EtatDeFile) {
        etat.morceaux.clear();
        self.budget.rendre(etat.octets);
        etat.octets = 0;
    }
}

/// Le cote du pont qui depose les morceaux : jamais d'attente.
pub struct EnvoiDeFlux {
    partage: Arc<FileDeFlux>,
}

/// Pourquoi un morceau n'a pas ete depose.
#[derive(Debug, PartialEq, Eq)]
pub enum RefusDeFlux {
    /// Le lecteur est parti.
    Ferme,
    /// Le lecteur ne lit plus (plafond du flux ou budget commun) : le flux
    /// est coupe.
    Plein,
}

/// Le corps d'un flux coupe pour depassement.
#[derive(Debug, PartialEq, Eq)]
pub struct FluxCoupe;

impl std::fmt::Display for FluxCoupe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("flux coupe par le pont : le lecteur ne lisait plus")
    }
}

impl std::error::Error for FluxCoupe {}

impl EnvoiDeFlux {
    /// Depose un morceau sans attendre. Un morceau plus gros que le plafond
    /// du flux passe encore si la file est vide (jamais au-dela du budget).
    pub fn deposer(&self, morceau: Vec<u8>) -> Result<(), RefusDeFlux> {
        let f = &self.partage;
        let n = morceau.len();
        let mut etat = f.etat();
        if etat.reception_fermee {
            return Err(RefusDeFlux::Ferme);
        }
        if etat.coupe {
            return Err(RefusDeFlux::Plein);
        }
        let au_plafond = etat.octets > 0 && etat.octets + n > f.plafond;
        if au_plafond || !f.budget.prendre(n) {
            etat.coupe = true;
            f.vider(&mut etat);
            drop(etat);
            f.reveil.notify_one();
            return Err(RefusDeFlux::Plein);
        }
        etat.octets += n;
        etat.morceaux.push_back(morceau);
        drop(etat);
        f.reveil.notify_one();
        Ok(())
    }

    /// Octets en attente (journal, tests).
    pub fn en_attente(&self) -> usize {
        self.partage.etat().octets
    }
}

impl Drop for EnvoiDeFlux {
    fn drop(&mut self) {
        self.partage.etat().envoi_ferme = true;
        self.partage.reveil.notify_one();
    }
}

/// Le cote du corps HTTP.
pub struct ReceptionDeFlux {
    partage: Arc<FileDeFlux>,
}

impl ReceptionDeFlux {
    /// Le morceau suivant ; `None` a la fin du flux, `Err` s'il a ete coupe.
    pub async fn recv(&mut self) -> Option<Result<Vec<u8>, FluxCoupe>> {
        let f = &self.partage;
        loop {
            {
                let mut etat = f.etat();
                if etat.coupe {
                    return Some(Err(FluxCoupe));
                }
                if let Some(m) = etat.morceaux.pop_front() {
                    etat.octets -= m.len();
                    f.budget.rendre(m.len());
                    return Some(Ok(m));
                }
                if etat.envoi_ferme {
                    return None;
                }
            }
            f.reveil.notified().await;
        }
    }
}

impl Drop for ReceptionDeFlux {
    fn drop(&mut self) {
        let f = &self.partage;
        let mut etat = f.etat();
        etat.reception_fermee = true;
        f.vider(&mut etat);
    }
}

pub struct PendingResponse {
    pub status: u16,
    pub headers: serde_json::Map<String, serde_json::Value>,
    pub body: CorpsRelaye,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_tx() -> mpsc::Sender<String> {
        mpsc::channel(1).0
    }

    fn state_with_cap(max_servers: usize) -> RelayState {
        RelayState {
            servers: DashMap::new(),
            tokens: DashMap::new(),
            // Sans jeton de service, `verifier` laisse passer : ces tests
            // portent sur les regles d'enregistrement, pas sur la licence.
            licences: crate::licence::Licences::depuis_environnement(),
            billets: std::sync::Arc::new(crate::circle::Billets::nouveau(
                "http://cloud.invalide",
                None,
            )),
            trafic: crate::trafic::Trafic::nouveau(),
            sessions: crate::sessions::Sessions::depuis_environnement(),
            max_servers,
            max_clients_per_server: 10,
            max_streams_per_server: 5,
            plafond_par_flux: PLAFOND_PAR_FLUX,
            budget_flux: BudgetDesFlux::nouveau(BUDGET_DES_FLUX),
        }
    }

    fn reg(state: &RelayState, server_id: &str, token: &str) -> Result<u64, &'static str> {
        state.register_server(
            server_id.to_string(),
            "name".to_string(),
            token.to_string(),
            None,
            dummy_tx(),
        )
    }

    #[test]
    fn new_server_registers_and_binds_token() {
        let s = state_with_cap(10);
        assert!(reg(&s, "srv-a", "tok-a").is_ok());
        assert_eq!(s.server_for_token("tok-a").as_deref(), Some("srv-a"));
    }

    #[test]
    fn stranger_cannot_overwrite_live_server() {
        // The core P0: a different token must not take over a live server_id.
        let s = state_with_cap(10);
        reg(&s, "srv-a", "tok-a").unwrap();
        assert_eq!(
            reg(&s, "srv-a", "attacker-token"),
            Err("server_id already registered"),
        );
        // The original binding is untouched.
        assert_eq!(s.server_for_token("tok-a").as_deref(), Some("srv-a"));
        assert_eq!(s.server_for_token("attacker-token"), None);
    }

    #[test]
    fn legit_reconnect_with_same_token_replaces() {
        let s = state_with_cap(10);
        reg(&s, "srv-a", "tok-a").unwrap();
        // Same identity reconnecting (its persisted token) is allowed to replace.
        assert!(reg(&s, "srv-a", "tok-a").is_ok());
        assert_eq!(s.servers.len(), 1);
    }

    #[test]
    fn token_bound_to_another_server_is_rejected() {
        let s = state_with_cap(10);
        reg(&s, "srv-a", "tok-a").unwrap();
        assert_eq!(
            reg(&s, "srv-b", "tok-a"),
            Err("bridge_token already bound to another server"),
        );
    }

    #[test]
    fn unregister_frees_id_and_token_for_reconnect() {
        let s = state_with_cap(10);
        reg(&s, "srv-a", "tok-a").unwrap();
        s.unregister_server("srv-a");
        assert_eq!(s.server_for_token("tok-a"), None);
        // A fresh registration (even a new token) is fine once the id is free.
        assert!(reg(&s, "srv-a", "tok-a2").is_ok());
    }

    /// Renouvellement du lien d'accès (`POST /cloud/bridge/rotate-token`) :
    /// le serveur referme sa session et se réinscrit avec un nouveau jeton.
    /// L'ancien ne doit plus rien ouvrir, le nouveau oui.
    #[test]
    fn renewed_token_replaces_the_old_one_after_reconnect() {
        let s = state_with_cap(10);
        reg(&s, "srv-a", "tok-ancien").unwrap();

        // Tant que la session à l'ancien jeton vit, un autre jeton ne la
        // remplace pas : le serveur doit d'abord la refermer.
        assert_eq!(
            reg(&s, "srv-a", "tok-nouveau"),
            Err("server_id already registered"),
        );
        assert_eq!(s.server_for_token("tok-nouveau"), None);

        // Fermeture de la session (fin de `handle_server_ws`), réinscription.
        s.unregister_server("srv-a");
        reg(&s, "srv-a", "tok-nouveau").unwrap();

        assert_eq!(
            s.server_for_token("tok-ancien"),
            None,
            "l'ancien jeton ouvre encore le serveur"
        );
        assert_eq!(s.server_for_token("tok-nouveau").as_deref(), Some("srv-a"));
        assert_eq!(s.tokens.len(), 1);
    }

    #[test]
    fn max_servers_is_enforced_for_new_ids() {
        let s = state_with_cap(1);
        reg(&s, "srv-a", "tok-a").unwrap();
        assert_eq!(reg(&s, "srv-b", "tok-b"), Err("max servers reached"));
        // But re-registering the existing one is not blocked by the cap.
        assert!(reg(&s, "srv-a", "tok-a").is_ok());
    }
}
